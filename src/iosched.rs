//! 统一 IO 代理读调度层（L1，设计 §19）：**队列在调度器、串行在调度器、
//! 优先级在调度器**。读线程池按优先级队列取任务直接执行源读，读毕把
//! buffer 经各服务现有回执通道投回 UI poll，服务 spawn 纯 CPU 解码 worker。
//! 服务只剩"提交 → 收 buffer → 解码"，不再有 deferred/记账/io_gate。
//!
//! 纳管范围与介质事实（用户调研定案）：
//! - **目录模式 = 最重**：评级 XMP 头随机读（≤512KB/文件）与大图解码
//!   （18MB 级）在同一介质正面撞——Rating 队列排在 Decode 之后天然让路；
//! - **zip 流式快路径 = 次重**：流式解码/预取直读 NAS 上的压缩包——
//!   Decode/Prefetch 都在本调度器排队；
//! - **缓存/解压路径无需纳管**：非流式解码读本地缓存（ImageSource::File）、
//!   解压会话读 NAS 写本地、raw 缓存源同理——读写分离两块盘；
//! - **mpv stream_cb / 解压会话 / 像素探针 = 外部租户不纳管**：视频/图片
//!   两态互斥、解压会话单在途串行、探针 3~8B 直读零解码成本。
//!
//! 优先级（I3）：读线程每次取任务按 **Decode > Prefetch > Rating** 队列序，
//! 各队列 FIFO 保序（单图串行链"当前张 → +1 → −1"由请求顺序涌现）。评级
//! 82 项压在队列里也不会挡住一次翻页读——优先级是队列序的天然结果。
//!
//! 读线程数 = `io_parallel_max_num`（D1，语义回归本源）：max=1 单读线程，
//! 同一时刻至多一个源读（HDD 连续读）；max=4 四读并行（SSD 收益）。
//! **动态伸缩**：读线程每轮取任务前核对目标数——超编自行退出、缺员由
//! 提交侧补 spawn，reload 即时生效且不打断在途读。
//!
//! 取消语义（D6）：换上下文 `cancel(class)` 清队列；在途读毕按 gen 代次
//! 校验丢弃（服务侧 pending 记代次，迟到 buffer 不吃掉新请求占位）。
//!
//! 看门狗（D5，NAS 断连场景）：读无法中断（`fs::read` 挂死），单读线程
//! 卡死 = 读线全停（已拿 buffer 的解码 worker 不受影响）。UI poll 每帧调
//! [`IoScheduler::watchdog`]：队列非空且超 [`READ_STALL`] 无读毕 → error
//! 日志 + spawn 一次性救援读线程执行队首任务（fail-open 退化两读并行，
//! 与旧 GATE_TIMEOUT 旁路同等级）。

use crate::config::IoConfig;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// 读线程救援看门狗阈值：队列非空且超此时长无读毕 = 疑似读卡死
/// （NAS 断连/磁盘停滞），spawn 救援线程退化两读并行
const READ_STALL: Duration = Duration::from_secs(10);

/// io_parallel_max_num 配置钳制域：防 0（死锁）与病态大值（IO 风暴）
pub const PARALLEL_MIN: usize = 1;
pub const PARALLEL_MAX: usize = 16;

/// IO 任务类（优先级 Decode > Prefetch > Rating，队列序）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoClass {
    /// 显式解码：图片 base/full 高清、raw 帧软解、raw 直方图
    Decode,
    /// 后台预取：±1 邻居、对比候选（user 可见内容未到，空闲预算先行备好）
    Prefetch,
    /// 评级探测：单文件随机头读（≤512KB）。最低优先级——排在两队列之后
    /// 让路涌现，不再需要 App 侧派发闸门
    Rating,
}

impl IoClass {
    /// 优先级序号（队列数组下标，值小者优先）
    fn prio(self) -> usize {
        match self {
            IoClass::Decode => 0,
            IoClass::Prefetch => 1,
            IoClass::Rating => 2,
        }
    }
}

/// 代理读任务（D4 闭包表达）：读闭包捕获 ImageSource/raw 参数等，调度器
/// 不感知具体读形态（read_source_bytes/read_frame_bytes/read_rating/webp
/// 嗅探留在各自模块，接口零耦合）；on_done = 投回执消息 + request_repaint
/// （读线程内执行，极轻）。解码函数留在服务侧 worker——读线程只读，
/// 解码段边界清晰
pub struct IoJob {
    /// 日志观测（文件名）
    pub name: String,
    pub class: IoClass,
    /// 服务侧提交令牌（服务自增序号，0 = 无令牌）：服务看门狗剔除后用
    /// [`IoScheduler::cancel_token`] 按令牌摘除**仍在排队**的旧任务，防
    /// 重发堆积（在读任务不摘——读毕由服务按 gen 代次校验丢弃）
    pub token: u64,
    /// 大字节读标记（raw 整帧 24~48MB 级）：看门狗宽限 3×READ_STALL——
    /// NAS 慢盘上大读 >10s 是常态，不宽限会被误判卡死反复派救援
    /// （fail-open 两读并行违背 max=1 串行不变量）
    pub heavy: bool,
    /// 读段：一次 IO 拿全源字节（纯读，无解码）
    pub work: Box<dyn FnOnce() -> Result<Vec<u8>, String> + Send>,
    /// 读毕回执（成功/失败都调）：投 Buffer/ReadFailed 消息回服务通道
    pub on_done: Box<dyn FnOnce(Result<Vec<u8>, String>) + Send>,
}

struct Queues {
    /// 三条 FIFO，按优先级序取任务
    q: [VecDeque<IoJob>; 3],
}

/// 统一 IO 代理读调度器（Arc 共享；提交/查询在 UI 线程，读线程池内部执行。
/// 提交侧方法以 `&Arc<Self>` 为接收者——补 spawn 读线程需要 Arc 句柄）
pub struct IoScheduler {
    /// 任务队列 + 读线程等待条件（同一把锁：取任务/入队/查询互斥）
    queues: Mutex<Queues>,
    /// 读线程阻塞等任务
    cond: Condvar,
    /// 读线程目标数 = io_parallel_max_num（reload 即时生效）
    readers_target: AtomicUsize,
    /// 在编读线程数（含救援线程；超编线程取任务前自行退出）
    readers_active: AtomicUsize,
    /// 最近一次读毕时刻（看门狗基准一）
    last_read_finished: Mutex<Instant>,
    /// 最近一次救援 spawn 时刻（看门狗基准二：防卡死期每帧重复救援）
    last_rescue: Mutex<Instant>,
    /// 在读的大字节任务数（heavy 标记）：看门狗按此分级宽限
    heavy_in_flight: AtomicUsize,
}

impl IoScheduler {
    pub fn new(cfg: &IoConfig) -> Arc<Self> {
        Arc::new(Self::build(cfg))
    }

    fn build(cfg: &IoConfig) -> Self {
        Self {
            queues: Mutex::new(Queues {
                q: Default::default(),
            }),
            cond: Condvar::new(),
            readers_target: AtomicUsize::new(
                cfg.io_parallel_max_num.clamp(PARALLEL_MIN, PARALLEL_MAX),
            ),
            readers_active: AtomicUsize::new(0),
            last_read_finished: Mutex::new(Instant::now()),
            last_rescue: Mutex::new(Instant::now()),
            heavy_in_flight: AtomicUsize::new(0),
        }
    }

    /// 提交任务（UI 线程）：入对应类 FIFO 队尾，补齐读线程后唤醒。
    /// 重复提交在服务侧已被 pending/缓存检查挡住，队列内不做去重（D2）
    pub fn submit(self: &Arc<Self>, job: IoJob) {
        let class = job.class;
        let name = job.name.clone();
        let depth = {
            let mut g = self.queues.lock().unwrap();
            let q = &mut g.q[class.prio()];
            q.push_back(job);
            q.len()
        };
        // 入队（info，队列深度）：IO 等待关键转折观测点
        tracing::info!("IO 入队 {class:?} {name}（队列 {depth}）");
        self.ensure_readers();
        self.cond.notify_one();
    }

    /// 清空指定类队列（换包/会话重置），返回清除条数。在途读不受影响
    /// （读毕由服务按 gen 代次校验丢弃）
    pub fn cancel(&self, class: IoClass) -> usize {
        let mut g = self.queues.lock().unwrap();
        let n = g.q[class.prio()].len();
        g.q[class.prio()].clear();
        n
    }

    /// 按提交令牌摘除指定类队列中的任务（服务看门狗剔除重发前调用）：
    /// 只摘**仍在排队**的旧任务防重发堆积；在读任务不摘（读毕由服务按
    /// gen 代次校验丢弃）。返回摘除条数
    pub fn cancel_token(&self, class: IoClass, token: u64) -> usize {
        if token == 0 {
            return 0;
        }
        let mut g = self.queues.lock().unwrap();
        let q = &mut g.q[class.prio()];
        let n = q.len();
        q.retain(|j| j.token != token);
        n - q.len()
    }

    /// 指定类队列长度（观测：星级筛选"扫描未完成"判定、预取排队汇总）
    pub fn queued(&self, class: IoClass) -> usize {
        self.queues.lock().unwrap().q[class.prio()].len()
    }

    /// 热重载（顶栏「重载配置」）：替换读线程目标数。缺员即时补 spawn、
    /// 超编由读线程取任务前自行退出（不打断在途读）；notify_all 唤醒
    /// park 在空队列上的读者复核退出条件——否则已 park 的超编读者
    /// 永不醒来（缩编不生效）
    pub fn reload(self: &Arc<Self>, cfg: &IoConfig) {
        self.readers_target.store(
            cfg.io_parallel_max_num.clamp(PARALLEL_MIN, PARALLEL_MAX),
            Ordering::Relaxed,
        );
        self.ensure_readers();
        self.cond.notify_all();
    }

    /// 全部队列长度（看门狗判定用）
    fn queued_all(&self) -> usize {
        let g = self.queues.lock().unwrap();
        g.q.iter().map(|q| q.len()).sum()
    }

    /// 读线程目标数
    fn target(&self) -> usize {
        self.readers_target.load(Ordering::Relaxed)
    }

    /// 缺员补 spawn（提交/reload 调用；spawn 失败回退计数）
    fn ensure_readers(self: &Arc<Self>) {
        let active = self.readers_active.load(Ordering::Relaxed);
        let target = self.target();
        if active >= target {
            return;
        }
        self.readers_active.fetch_add(1, Ordering::Relaxed);
        let this = Arc::clone(self);
        let r = std::thread::Builder::new()
            .name("ioreader".into())
            .spawn(move || this.reader_loop());
        if r.is_err() {
            self.readers_active.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// 读线程主循环：等任务 → 按优先级取一个 → 读 → 回执。每轮取任务前
    /// 核对目标数，超编自行退出（动态伸缩，不打断在途读）
    fn reader_loop(self: Arc<Self>) {
        self.reader_loop_from(None, false)
    }

    /// 读线程主循环本体（`first` = 救援线程直带的队首任务）。`rescue=true`
    /// 时本线程即救援线程：自带一名超编名额，退出判定线放宽 target+1，
    /// 跑完救援任务后**留编为常规读者**——若按旧实现"毕活即 fetch_sub
    /// 退出"，会与"原读者按超编自行退出"叠加成零读者饿死队列（只能等
    /// 10s 后看门狗再救一次）
    fn reader_loop_from(self: Arc<Self>, first: Option<IoJob>, rescue: bool) {
        let mut first = first;
        loop {
            let job = match first.take() {
                Some(job) => job,
                None => {
                    let mut g = self.queues.lock().unwrap();
                    loop {
                        // 先取任务（超编读者有活照干，不留活饿死）；队列空
                        // 才核对超编退出。判定线每轮实时计算（target 可被
                        // reload 缩编）——入口/wait 前捕获旧值会让旧读者拿
                        // 旧 target 比对，缩编后永不退出（动态伸缩测试钉住）
                        match Self::take_job(&mut g) {
                            Some(job) => break job,
                            None => {
                                let limit = self.target() + if rescue { 1 } else { 0 };
                                if self.readers_active.load(Ordering::Relaxed) > limit {
                                    self.readers_active.fetch_sub(1, Ordering::Relaxed);
                                    return;
                                }
                                g = self.cond.wait(g).unwrap();
                            }
                        }
                    }
                }
            };
            self.run_job(job);
        }
    }

    /// 按优先级取队首任务（锁内调用）
    fn take_job(g: &mut Queues) -> Option<IoJob> {
        g.q.iter_mut().find_map(|q| q.pop_front())
    }

    /// 执行单个任务（读线程/救援线程共用）：读段 panic 兜底转 Err——畸形
    /// 源可能 panic（坏 VFS 实现），否则线程静默死亡队列全停。读毕更新
    /// 看门狗时间戳
    fn run_job(&self, job: IoJob) {
        let t0 = Instant::now();
        // 开工（debug）
        tracing::debug!("IO 开工 {:?} {}", job.class, job.name);
        if job.heavy {
            self.heavy_in_flight.fetch_add(1, Ordering::Relaxed);
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job.work))
            .unwrap_or_else(|_| {
                // 读段 panic：转 Err 回执（error 级——畸形源分支确认点）
                tracing::error!("IO 读段 panic {:?} {}", job.class, job.name);
                Err("IO 读线程 panic".into())
            });
        if job.heavy {
            self.heavy_in_flight.fetch_sub(1, Ordering::Relaxed);
        }
        // 读毕（debug，耗时）
        tracing::debug!(
            "IO 读毕 {:?} {}（{}ms）",
            job.class,
            job.name,
            t0.elapsed().as_millis()
        );
        // 回执 panic 兜底：on_done 投消息 + repaint，正常不可能 panic；
        // 一旦 panic（如互斥中毒）会杀读线程且 readers_active 不回收
        // （计数虚高 → ensure_readers 永不补员），捕获记 error 防静默死亡
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (job.on_done)(result);
        }))
        .map_err(|_| {
            tracing::error!("IO 回执 on_done panic {:?} {}", job.class, job.name);
        });
        *self.last_read_finished.lock().unwrap() = Instant::now();
    }

    /// 看门狗（UI poll 每帧调）：队列非空且超阈值无读毕（且自上次救援后
    /// 仍无读毕）→ spawn 一次性救援读线程执行队首任务。阈值分级：在读
    /// 大字节任务（heavy，raw 整帧 24~48MB）宽限 3×READ_STALL——NAS 慢盘
    /// 大读 >10s 是常态，不分级会误判卡死反复 fail-open 破坏串行不变量。
    /// 救援线程自带一名超编名额（退出判定线 target+1），跑完队首任务后
    /// 留编为常规读者（原超编读者在队列排空时对账退出）
    pub fn watchdog(self: &Arc<Self>) {
        let queued = self.queued_all();
        if queued == 0 {
            return;
        }
        let stall = if self.heavy_in_flight.load(Ordering::Relaxed) > 0 {
            READ_STALL * 3
        } else {
            READ_STALL
        };
        let base = {
            let last = *self.last_read_finished.lock().unwrap();
            let rescued = *self.last_rescue.lock().unwrap();
            last.max(rescued)
        };
        if base.elapsed() <= stall {
            return;
        }
        let job = {
            let mut g = self.queues.lock().unwrap();
            Self::take_job(&mut g)
        };
        let Some(job) = job else { return };
        *self.last_rescue.lock().unwrap() = Instant::now();
        self.readers_active.fetch_add(1, Ordering::Relaxed);
        tracing::error!(
            "IO 读线程疑似卡死（>{stall:?}无读毕，队列 {queued} 项）——spawn 救援读线程执行队首 {} {:?}，退化为两读并行",
            job.name,
            job.class
        );
        // 任务入槽位传递：救援线程取走执行；spawn 失败（极端）则从槽位
        // 取回归还队首防蒸发（否则服务侧 pending 只能等 10s 超时兜底）
        let job_slot = Arc::new(Mutex::new(Some(job)));
        let this = Arc::clone(self);
        let slot = Arc::clone(&job_slot);
        let r = std::thread::Builder::new()
            .name("iorescue".into())
            .spawn(move || {
                if let Some(j) = slot.lock().unwrap().take() {
                    this.reader_loop_from(Some(j), true);
                }
            });
        if r.is_err() {
            let mut s = job_slot.lock().unwrap();
            if let Some(job) = s.take() {
                // error 级确认分支
                tracing::error!("救援读线程创建失败，任务归还队列: {}", job.name);
                self.readers_active.fetch_sub(1, Ordering::Relaxed);
                let mut g = self.queues.lock().unwrap();
                g.q[job.class.prio()].push_front(job);
            }
        }
    }

    /// 读线程在编数（测试观测）
    #[cfg(test)]
    fn readers_active(&self) -> usize {
        self.readers_active.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn cfg(max: usize) -> IoConfig {
        IoConfig {
            io_parallel_max_num: max,
        }
    }

    /// 空任务（work 立即返回空字节，on_done 空操作）
    fn job(class: IoClass, name: &str) -> IoJob {
        IoJob {
            name: name.to_owned(),
            class,
            token: 0,
            heavy: false,
            work: Box::new(|| Ok(Vec::new())),
            on_done: Box::new(|_| {}),
        }
    }

    /// 优先级队列序：Prefetch 任务先入队并在读，Decode 后入队——读线程
    /// 取任务时 Decode 越过队首 Prefetch 先读（完成顺序观测）
    #[test]
    fn 优先级队列序() {
        let s = IoScheduler::new(&cfg(1));
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (tx, rx) = mpsc::channel::<&'static str>();
        // 占住唯一读线程：Prefetch 长任务（started 信号确认已在读）
        let tx1 = tx.clone();
        s.submit(IoJob {
            name: "prefetch-1".into(),
            class: IoClass::Prefetch,
            token: 0,
            heavy: false,
            work: Box::new(move || {
                let _ = started_tx.send(());
                std::thread::sleep(Duration::from_millis(150));
                Ok(Vec::new())
            }),
            on_done: Box::new(move |_| {
                let _ = tx1.send("prefetch-1");
            }),
        });
        started_rx.recv_timeout(Duration::from_secs(2)).expect("读线程应开工");
        // 读线程正忙：此刻入队 Decode 与 Prefetch 各一
        let tx2 = tx.clone();
        s.submit(IoJob {
            name: "decode-1".into(),
            class: IoClass::Decode,
            token: 0,
            heavy: false,
            work: Box::new(|| Ok(Vec::new())),
            on_done: Box::new(move |_| {
                let _ = tx2.send("decode-1");
            }),
        });
        s.submit(IoJob {
            name: "prefetch-2".into(),
            class: IoClass::Prefetch,
            token: 0,
            heavy: false,
            work: Box::new(|| Ok(Vec::new())),
            on_done: Box::new(move |_| {
                let _ = tx.send("prefetch-2");
            }),
        });
        let order: Vec<_> = rx.iter().take(3).collect();
        assert_eq!(
            order,
            vec!["prefetch-1", "decode-1", "prefetch-2"],
            "Decode 必须越过队首 Prefetch 先读（优先级队列序）"
        );
    }

    /// 串行 max=1：单读线程同一时刻至多一个任务在读（无重叠）
    #[test]
    fn 串行_max1_无重叠() {
        let s = IoScheduler::new(&cfg(1));
        let overlap = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel::<()>();
        for i in 0..4 {
            let (ov, mx) = (overlap.clone(), max_seen.clone());
            let tx = tx.clone();
            s.submit(IoJob {
                name: format!("j{i}"),
                class: IoClass::Decode,
                token: 0,
                heavy: false,
                work: Box::new(move || {
                    let n = ov.fetch_add(1, Ordering::SeqCst) + 1;
                    mx.fetch_max(n, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(20));
                    ov.fetch_sub(1, Ordering::SeqCst);
                    Ok(Vec::new())
                }),
                on_done: Box::new(move |_| {
                    let _ = tx.send(());
                }),
            });
        }
        drop(tx);
        for _ in rx {}
        assert_eq!(max_seen.load(Ordering::SeqCst), 1, "max=1 读段必须串行");
    }

    /// 动态伸缩：max=4 四任务并行；reload 缩编到 1 后超编读者退出
    #[test]
    fn 动态伸缩() {
        let s = IoScheduler::new(&cfg(4));
        let (start_tx, start_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        for _ in 0..4 {
            let st = start_tx.clone();
            let dn = done_tx.clone();
            s.submit(IoJob {
                name: "p".into(),
                class: IoClass::Decode,
                token: 0,
                heavy: false,
                work: Box::new(move || {
                    let _ = st.send(());
                    std::thread::sleep(Duration::from_millis(150));
                    Ok(Vec::new())
                }),
                on_done: Box::new(move |_| {
                    let _ = dn.send(());
                }),
            });
        }
        drop(start_tx);
        let mut started = 0;
        for _ in start_rx.iter() {
            started += 1;
        }
        assert_eq!(started, 4, "4 读线程应全部并行开工");
        drop(done_tx);
        for _ in done_rx {}
        // 缩编：reload 到 1，超编 3 名读者取任务前自行退出（在途读不打断）
        s.reload(&cfg(1));
        let deadline = Instant::now() + Duration::from_secs(2);
        while s.readers_active() > 1 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(s.readers_active() <= 1, "缩编后超编读线程应自行退出");
    }

    /// cancel 清指定类队列（在途读不受影响，队列观测归零）
    #[test]
    fn cancel_清队列() {
        let s = IoScheduler::new(&cfg(1));
        // 占住读线程（长任务）
        let (tx, rx) = mpsc::channel::<()>();
        let txc = tx.clone();
        s.submit(IoJob {
            name: "busy".into(),
            class: IoClass::Decode,
            token: 0,
            heavy: false,
            work: Box::new(|| {
                std::thread::sleep(Duration::from_millis(200));
                Ok(Vec::new())
            }),
            on_done: Box::new(move |_| {
                let _ = txc.send(());
            }),
        });
        std::thread::sleep(Duration::from_millis(50));
        for i in 0..3 {
            s.submit(job(IoClass::Rating, &format!("r{i}")));
        }
        assert_eq!(s.queued(IoClass::Rating), 3);
        assert_eq!(s.cancel(IoClass::Rating), 3);
        assert_eq!(s.queued(IoClass::Rating), 0);
        let _ = rx.recv_timeout(Duration::from_secs(2));
    }

    /// queued 观测：入队即计数、全部读毕归零
    #[test]
    fn queued_观测() {
        let s = IoScheduler::new(&cfg(2));
        // 占住两个读线程
        let (tx, rx) = mpsc::channel::<()>();
        for _ in 0..2 {
            let t = tx.clone();
            s.submit(IoJob {
                name: "hold".into(),
                class: IoClass::Prefetch,
                token: 0,
                heavy: false,
                work: Box::new(|| {
                    std::thread::sleep(Duration::from_millis(500));
                    Ok(Vec::new())
                }),
                on_done: Box::new(move |_| {
                    let _ = t.send(());
                }),
            });
        }
        s.submit(job(IoClass::Prefetch, "queued-1"));
        // 等待稳定态（读者把两个占位任务取走在读）：入队与读者取任务并发，
        // 瞬时观测可能看到 3（读者未及取走占位），轮询至收敛防竞态
        let deadline = Instant::now() + Duration::from_secs(2);
        while s.queued(IoClass::Prefetch) != 1 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(s.queued(IoClass::Prefetch), 1, "第 3 个任务应排队");
        drop(tx);
        for _ in rx {}
        let deadline = Instant::now() + Duration::from_secs(2);
        while s.queued(IoClass::Prefetch) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(s.queued(IoClass::Prefetch), 0, "全部读毕队列应清空");
    }

    /// 看门狗：伪造"很久无读毕 + 队列非空"→ 救援线程执行队首任务并留编
    /// 消化剩余队列；时间戳新鲜时不触发
    #[test]
    fn watchdog_救援队首() {
        let s = IoScheduler::new(&cfg(1));
        // 占住唯一读线程（长任务），队列积压 2 项
        let (tx, rx) = mpsc::channel::<()>();
        let t0 = tx.clone();
        s.submit(IoJob {
            name: "stuck".into(),
            class: IoClass::Decode,
            token: 0,
            heavy: false,
            work: Box::new(|| {
                std::thread::sleep(Duration::from_millis(700));
                Ok(Vec::new())
            }),
            on_done: Box::new(move |_| {
                let _ = t0.send(());
            }),
        });
        std::thread::sleep(Duration::from_millis(50));
        let (rtx, rrx) = mpsc::channel::<&'static str>();
        for name in ["rescued-a", "rescued-b"] {
            let t = rtx.clone();
            s.submit(IoJob {
                name: name.into(),
                class: IoClass::Prefetch,
                token: 0,
                heavy: false,
                work: Box::new(|| Ok(Vec::new())),
                on_done: Box::new(move |_| {
                    let _ = t.send(name);
                }),
            });
        }
        // 队列非空但时间戳新鲜：不触发
        s.watchdog();
        assert_eq!(s.queued(IoClass::Prefetch), 2, "看门狗不应误触发");
        // 伪造时间戳：很久无读毕且无救援
        *s.last_read_finished.lock().unwrap() = Instant::now() - READ_STALL;
        *s.last_rescue.lock().unwrap() = Instant::now() - READ_STALL;
        s.watchdog();
        // 救援取走队首 rescued-a（优先级序）执行
        let got = rrx
            .recv_timeout(Duration::from_secs(2))
            .expect("救援线程应执行队首任务");
        assert_eq!(got, "rescued-a");
        // 救援线程留编为常规读者，继续消化 rescued-b（不再毕活即退出）
        let got2 = rrx
            .recv_timeout(Duration::from_secs(2))
            .expect("救援读者应继续消化队列");
        assert_eq!(got2, "rescued-b");
        // 队列排空后超编名额对账退出：在编读者回落到目标数（防零读者饿死）
        let deadline = Instant::now() + Duration::from_secs(2);
        while s.readers_active() > 1 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(s.readers_active() <= 1, "超编名额应在队列排空后对账退出");
        let _ = rx.recv_timeout(Duration::from_secs(2));
    }

    /// cancel_token：按提交令牌摘除仍在排队的旧任务（防看门狗重发堆积）；
    /// 在读任务不受影响，无令牌（0）恒零动作
    #[test]
    fn cancel_token_摘队() {
        let s = IoScheduler::new(&cfg(1));
        // 占住唯一读线程（长任务）
        let (tx, rx) = mpsc::channel::<()>();
        let txc = tx.clone();
        s.submit(IoJob {
            name: "busy".into(),
            class: IoClass::Decode,
            token: 1,
            heavy: false,
            work: Box::new(|| {
                std::thread::sleep(Duration::from_millis(200));
                Ok(Vec::new())
            }),
            on_done: Box::new(move |_| {
                let _ = txc.send(());
            }),
        });
        std::thread::sleep(Duration::from_millis(50));
        s.submit(job(IoClass::Decode, "q-7"));
        s.submit(job(IoClass::Decode, "q-8"));
        assert_eq!(s.cancel_token(IoClass::Decode, 7), 0, "无令牌任务不摘");
        assert_eq!(s.cancel_token(IoClass::Decode, 0), 0, "令牌 0 恒零动作");
        // 给排队任务标记令牌 7 再摘（重造：cancel 7 应摘掉恰一条）
        {
            let mut g = s.queues.lock().unwrap();
            g.q[IoClass::Decode.prio()][0].token = 7;
        }
        assert_eq!(s.queued(IoClass::Decode), 2);
        assert_eq!(s.cancel_token(IoClass::Decode, 7), 1, "按令牌摘队首");
        assert_eq!(s.queued(IoClass::Decode), 1, "在读与异令牌任务不受影响");
        assert_eq!(s.cancel_token(IoClass::Decode, 7), 0, "重复摘除幂等");
        let _ = rx.recv_timeout(Duration::from_secs(2));
    }

    /// reload 钳制域：0 → 1（串行不并行）、超界收敛不 panic
    #[test]
    fn reload_钳制() {
        let s = IoScheduler::new(&cfg(1));
        s.reload(&cfg(0));
        let overlap = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel::<()>();
        for i in 0..3 {
            let (ov, mx) = (overlap.clone(), max_seen.clone());
            let tx = tx.clone();
            s.submit(IoJob {
                name: format!("j{i}"),
                class: IoClass::Decode,
                token: 0,
                heavy: false,
                work: Box::new(move || {
                    let n = ov.fetch_add(1, Ordering::SeqCst) + 1;
                    mx.fetch_max(n, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(15));
                    ov.fetch_sub(1, Ordering::SeqCst);
                    Ok(Vec::new())
                }),
                on_done: Box::new(move |_| {
                    let _ = tx.send(());
                }),
            });
        }
        drop(tx);
        for _ in rx {}
        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            1,
            "0 应钳到 1：读段必须串行"
        );
        // 99 → 16：补齐 16 读线程（无任务可跑，仅核对不 panic）
        s.reload(&cfg(99));
        std::thread::sleep(Duration::from_millis(100));
    }

    /// 读段 panic 兜底：work panic 转 Err 回执（不杀读线程，后续任务照常）
    #[test]
    fn 读段panic转Err回执() {
        let s = IoScheduler::new(&cfg(1));
        let (tx, rx) = mpsc::channel::<Result<Vec<u8>, String>>();
        let tx1 = tx.clone();
        s.submit(IoJob {
            name: "boom".into(),
            class: IoClass::Decode,
            token: 0,
            heavy: false,
            work: Box::new(|| panic!("坏源")),
            on_done: Box::new(move |r| {
                let _ = tx1.send(r);
            }),
        });
        s.submit(IoJob {
            name: "after".into(),
            class: IoClass::Decode,
            token: 0,
            heavy: false,
            work: Box::new(|| Ok(b"ok".to_vec())),
            on_done: Box::new(move |r| {
                let _ = tx.send(r);
            }),
        });
        assert!(rx.recv_timeout(Duration::from_secs(2)).unwrap().is_err());
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap(),
            b"ok".to_vec(),
            "panic 不得杀读线程"
        );
    }
}
