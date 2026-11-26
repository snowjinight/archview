//! 日志管线：内存环形缓冲 + 后台线程定时落盘。
//!
//! release 是 `windows` 子系统（无控制台），tracing 输出改走本模块：
//! - **内存环形缓冲**（无 ANSI 色，最近 [`RING_LINES`] 行）——F12 日志面板的
//!   即时数据源，UI 线程每帧拉取快照，与后台落盘互不阻塞；
//! - **后台落盘线程**：每 5s（或增量攒满 [`FLUSH_BYTES`] 提前唤醒）把增量
//!   追加写入 archview.log，避免每条日志一次磁盘 IO；进程退出时最多丢
//!   最近 5s 未落盘增量（诊断日志可接受）；
//! - 启动时日志文件超过 [`ROTATE_BYTES`] 轮转为 `archview.log.old`
//!   （覆盖上一代），防止无限增长。
//!
//! 接线（main.rs）：`tracing_subscriber::registry()` 挂两个 fmt 层——
//! 本模块层 `with_ansi(false)`（缓冲+落盘都要纯文本）+ debug 构建追加
//! stderr 层（console 子系统，cargo run 终端可见）。

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use tracing_subscriber::fmt::MakeWriter;

/// 环形缓冲行数上限（F12 面板可见的最大行数）
const RING_LINES: usize = 600;
/// 单行截断长度（超长行不吃内存；UTF-8 按字节计，截断点用 lossy 容错）
const LINE_TRUNC: usize = 1500;
/// 未落盘增量的强制落盘阈值（攒够即唤醒落盘线程，不等 5s）
const FLUSH_BYTES: usize = 64 * 1024;
/// 日志文件轮转阈值：启动时超过则改名 .old 重开
const ROTATE_BYTES: u64 = 4 * 1024 * 1024;

struct Inner {
    /// 环形缓冲（完整行，无 ANSI）——F12 面板数据源
    ring: VecDeque<String>,
    /// 尚未落盘的字节增量（落盘线程消费）
    pending: Vec<u8>,
    /// 跨多次 write 调用的未收尾行（tracing 一条事件可能分片写入）
    cur: Vec<u8>,
    /// 累计挤出行数（单调递增）：满环 pop_front 的总次数。UI 面板据此按
    /// 行距补偿滚动 offset——快照索引整体前移时正文才不会"上顶"（假滚动）
    evicted_total: u64,
}

pub struct LogSink {
    inner: Mutex<Inner>,
    /// 落盘线程唤醒：定时（5s）+ 增量攒满提前
    kick: Condvar,
}

static SINK: OnceLock<Arc<LogSink>> = OnceLock::new();
static PATH: OnceLock<PathBuf> = OnceLock::new();

impl LogSink {
    /// 初始化全局 sink 并启动落盘线程。全局注册供 UI（F12 面板）免穿透访问
    pub fn start(file: PathBuf) -> Arc<Self> {
        // 启动轮转：>4MB → archview.log.old（覆盖上一代，只留一代）
        if std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0) > ROTATE_BYTES {
            let old = file.with_extension("log.old");
            let _ = std::fs::remove_file(&old);
            let _ = std::fs::rename(&file, old);
        }
        let sink = Arc::new(Self {
            inner: Mutex::new(Inner {
                ring: VecDeque::with_capacity(RING_LINES),
                pending: Vec::new(),
                cur: Vec::new(),
                evicted_total: 0,
            }),
            kick: Condvar::new(),
        });
        let _ = PATH.set(file.clone());
        let _ = SINK.set(sink.clone());
        // 落盘线程：append 打开（轮转后即新文件）
        let t = sink.clone();
        std::thread::Builder::new()
            .name("log-flush".into())
            .spawn(move || t.flush_loop(file))
            .ok();
        sink
    }

    fn flush_loop(&self, file: PathBuf) {
        let mut out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file)
            .ok()
            .map(std::io::BufWriter::new);
        loop {
            // 毒化恢复：任何持锁线程 panic 后，日志管线继续工作（数据可能
            // 短暂不一致，好过锁死/ panic 风暴）
            let batch = {
                let mut g = match self.inner.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                // 增量未满阈值则等 5s（虚假唤醒无害：空批次直接跳过写）
                if g.pending.len() < FLUSH_BYTES {
                    let (guard, _) = self
                        .kick
                        .wait_timeout(g, std::time::Duration::from_secs(5))
                        .unwrap_or_else(|p| p.into_inner());
                    g = guard;
                }
                std::mem::take(&mut g.pending)
            };
        if let Some(o) = out.as_mut() {
            if !batch.is_empty() {
                let _ = o.write_all(&batch);
                let _ = o.flush();
            }
        }
        }
    }

    /// fmt 层的唯一入口：增量入 pending + 拆行进环形缓冲
    fn push_bytes(&self, buf: &[u8]) {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(), // 毒化恢复：日志管线不 panic 风暴
        };
        g.pending.extend_from_slice(buf);
        for &b in buf {
            if b == b'\n' {
                let mut line = String::from_utf8_lossy(&g.cur).into_owned();
                if line.len() > LINE_TRUNC {
                    line.truncate(LINE_TRUNC);
                    line.push('…');
                }
                if g.ring.len() == RING_LINES {
                    g.ring.pop_front();
                }
                g.ring.push_back(line);
                g.cur.clear();
            } else if g.cur.len() < LINE_TRUNC {
                g.cur.push(b);
            }
        }
        if g.pending.len() >= FLUSH_BYTES {
            self.kick.notify_all();
        }
    }

    /// 环形缓冲快照（UI 每帧克隆一次；600 行级容量，开销可忽略）。
    /// 毒化恢复同上
    fn snapshot(&self) -> Vec<String> {
        match self.inner.lock() {
            Ok(g) => g.ring.iter().cloned().collect(),
            Err(p) => p.into_inner().ring.iter().cloned().collect(),
        }
    }
}

/// 当前环形缓冲快照（UI「F12 日志面板」用；未初始化 = 空）
pub fn snapshot() -> Vec<String> {
    SINK.get().map(|s| s.snapshot()).unwrap_or_default()
}

/// 环形缓冲累计挤出行数（单调递增；F12 面板按帧差值补偿滚动 offset 用）
pub fn evicted_total() -> u64 {
    SINK.get()
        .map(|s| {
            match s.inner.lock() {
                Ok(g) => g.evicted_total,
                Err(p) => p.into_inner().evicted_total, // 毒化恢复同 push_bytes
            }
        })
        .unwrap_or(0)
}

/// 崩溃现场直接落盘（panic 钩子专用）：绕过 pending 批量管线，同步 append
/// 写文件（进程即将终止，不等 5s 落盘线程）+ 环形缓冲（F12 面板可见）。
/// 锁用毒化恢复——panic 钩子自身绝不能再 panic
pub fn push_panic(text: &str) {
    let line = format!("{text}\n");
    if let Some(s) = SINK.get() {
        let mut g = match s.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(), // 毒化也照收：数据可能不一致但可诊断
        };
        if g.ring.len() == RING_LINES {
            g.ring.pop_front();
            g.evicted_total += 1;
        }
        g.ring.push_back(text.to_string());
        g.pending.extend_from_slice(line.as_bytes());
        s.kick.notify_all();
    }
    // 直写文件（append）：panic 后进程随时终止，落盘线程不可依赖
    if let Some(p) = PATH.get() {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// 日志文件路径（UI「用记事本打开」用；未初始化 = None）
pub fn file_path() -> Option<PathBuf> {
    PATH.get().cloned()
}

/// 清空环形缓冲（F12 面板「清屏」）：仅清界面视图，落盘增量与文件不动
pub fn clear_view() {
    if let Some(s) = SINK.get() {
        match s.inner.lock() {
            Ok(mut g) => g.ring.clear(),
            Err(p) => p.into_inner().ring.clear(),
        }
    }
}

// ---- MakeWriter：给 tracing_subscriber fmt 层用 ---------------------------

pub struct LogWriter<'a> {
    sink: &'a LogSink,
}

impl Write for LogWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.sink.push_bytes(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// MakeWriter 句柄：本地 newtype 包住 Arc<LogSink>（孤儿规则不允许直接
/// 为外部类型 Arc<T> 实现外部 trait）。Clone 便宜，fmt 层持一份即可
#[derive(Clone)]
pub struct SinkWriter(pub Arc<LogSink>);

impl<'a> MakeWriter<'a> for SinkWriter {
    type Writer = LogWriter<'a>;
    fn make_writer(&'a self) -> Self::Writer {
        LogWriter { sink: &self.0 }
    }
}
