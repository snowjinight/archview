//! encode 引擎：裸数据 dump 序列 → 视频文件（libmpv encode 模式）。
//!
//! 架构定位（doc/架构设计/03_整体架构.md 不变量）：播放器与 VarTidy 是 VFS 层的两个
//! 并列消费者，本引擎是第三个消费者——输入 `Arc<dyn ArchiveVFS>` +
//! `VirtualEntry` 序列（DirVFS 磁盘目录 / ZipVFS / 7z / RAR 通吃），
//! 输出编码文件。UI 与 CLI 均为本引擎的薄壳（后台线程 + 进度展示 /
//! 参数解析），引擎自身不依赖任何 UI 或控制台设施。
//!
//! 数据通路：
//! ```text
//! N 个 dump 条目（已按编码顺序排列）
//!   → ConcatFile 逻辑拼接（零拷贝，rawvideo demuxer 只认连续字节流）
//!   → zcat://<id> stream_cb 协议（zvf:// 的姊妹协议，read/seek/size/
//!     close 四回调直接复用 player::stream_cb，仅 open_cb 是本模块专属）
//!   → headless mpv（vo=null，与 GUI 播放的 render API 链路完全无关）
//!     · demuxer 选项走 loadfile 第四参数（与播放链路同构）：
//!       demuxer=rawvideo,demuxer-rawvideo-w/h/mp-format/fps
//!     · 编码选项走全局选项：o=<输出> ovc=<编码器> ovcopts=<参数>
//!   → END_FILE(EOF) = 编码完成
//! ```

/// CLI 壳（`archview encode` 子命令 + SendTo；doc/架构设计/15_encode引擎.md §15.5）
pub mod cli;

use crate::config::raw_pixfmt_bits;
use crate::player::mpv_ffi as ffi;
use crate::player::stream_cb as zvf_cb;
use crate::vfs::{ArchiveVFS, VirtualEntry, VirtualFile};
use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// 帧序 dump 的格式参数（来源：`[[raw.demux]]` 预设命中值或 CLI/UI 显式输入，
/// 由壳负责解析；引擎只做校验）
#[derive(Debug, Clone)]
pub struct EncodeFormat {
    /// 像素格式名，须在 config::RAWVIDEO_PIXFMTS 名单内（如 "nv12"）
    pub pixfmt: String,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// 12bit 低对齐数据存 16bit 容器的高位修正（0 = 无；语义同播放链路，
    /// 编码侧由 ShiftReader 在流层修正——修不正的数据编进去也是废片）
    pub bit_shift: u32,
}

/// 编码器选项（容器由输出文件扩展名推断，如 .mp4/.mkv）
#[derive(Debug, Clone)]
pub struct EncodeOptions {
    /// mpv ovc（视频编码器名，取决于 libmpv 构建内置的 ffmpeg 编码器集）
    pub vcodec: String,
    /// mpv ovcopts（编码器参数，如 "crf=18,preset=medium"；None = 编码器默认）
    pub ovcopts: Option<String>,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            vcodec: "libx264".into(),
            ovcopts: None,
        }
    }
}

/// 编码结果统计
#[derive(Debug, Clone, Copy)]
pub struct EncodeStats {
    pub files: usize,
    pub total_bytes: u64,
    pub frames: u64,
}

/// 把 dump 序列编码为视频。阻塞直至完成/失败/取消。
///
/// - `entries`：已按编码顺序排列（多文件序列 = 文件名升序），壳负责排序与过滤
/// - `cancel`：置位后引擎下发 stop 中止编码，返回 Err("已取消")；半成品输出
///   文件不删除（是否清理由壳决定——失败现场有调试价值，重试会覆盖）
/// - `progress`：进度回调（0~100；在引擎线程调用，壳自行处理跨线程）。
///   编码速度不受实时约束（全速转码），percent-pos 由 mpv 按播放位置推算
pub fn encode_raw_sequence(
    dll_dir: &Path,
    vfs: &Arc<dyn ArchiveVFS>,
    entries: &[VirtualEntry],
    fmt: &EncodeFormat,
    out_path: &Path,
    opts: &EncodeOptions,
    cancel: &AtomicBool,
    progress: &dyn Fn(f64),
) -> Result<EncodeStats, String> {
    // ---- 参数与数据完整性校验（错误文案与播放链路 raw_demux_opts 同口径） ----
    if entries.is_empty() {
        return Err("编码输入为空：至少需要一个 dump 文件".into());
    }
    if entries.iter().any(|e| e.is_dir) {
        return Err("编码输入含目录条目".into());
    }
    if entries.iter().any(|e| e.encrypted) {
        return Err("编码输入含加密条目（暂不支持，请先解压）".into());
    }
    if fmt.pixfmt.eq_ignore_ascii_case("bayer_auto16le") {
        return Err(
            "编码引擎不支持 bayer_auto16le（自动排列探测仅播放链路），\
             请在预设中写明具体排列（如 bayer_rggb16le）"
                .into(),
        );
    }
    let Some(bits) = raw_pixfmt_bits(&fmt.pixfmt) else {
        return Err(format!(
            "像素格式 [{}] 不在 rawvideo 支持名单（config.toml 的 raw.pixfmts）",
            fmt.pixfmt
        ));
    };
    if fmt.width == 0 || fmt.height == 0 {
        return Err(format!("宽高不可为 0：{}x{}", fmt.width, fmt.height));
    }
    if !(fmt.fps.is_finite() && fmt.fps > 0.0) {
        return Err(format!("帧率非法：{}", fmt.fps));
    }
    if fmt.bit_shift > 8 {
        return Err(format!("bit_shift 越界：{}（允许 0~8）", fmt.bit_shift));
    }
    let frame_bytes = u64::from(fmt.width) * u64::from(fmt.height) * u64::from(bits) / 8;
    let total_bytes: u64 = entries.iter().map(|e| e.size).sum();
    if total_bytes % frame_bytes != 0 {
        return Err(format!(
            "总大小 {} 字节不是一帧 {} 字节（{}x{} {}）的整数倍——\
             序列与格式参数不匹配，或混入了非 dump 文件",
            total_bytes, frame_bytes, fmt.width, fmt.height, fmt.pixfmt
        ));
    }
    let frames = total_bytes / frame_bytes;

    // 预置取消：不加载 dll 直接退出（壳的"开始前就取消"零成本路径）
    if cancel.load(Ordering::Relaxed) {
        return Err("已取消".into());
    }

    // mpv 的 o 选项按进程工作目录解析相对路径，编码驱动在引擎线程内
    // 落盘——统一转绝对路径消除歧义
    let out_abs = if out_path.is_absolute() {
        out_path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("输出路径转绝对失败: {e}"))?
            .join(out_path)
    };
    if let Some(parent) = out_abs.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("输出目录创建失败 {}: {e}", parent.display()))?;
        }
    }

    // ---- headless mpv：与 GUI 播放链路（render API + PlayerService）无关 ----
    // 只创建不初始化：o/ovc 是非运行期选项，必须在 mpv_initialize 前设置
    ffi::init(dll_dir)?;
    let ctx = ffi::create_uninit()?;
    let mut last_err = String::new();
    let result = drive_encode(
        ctx, vfs, entries, fmt, &out_abs, opts, cancel, progress, &mut last_err,
    );
    ffi::destroy(ctx);
    result.map(|_| EncodeStats {
        files: entries.len(),
        total_bytes,
        frames,
    })
}

/// mpv 实例驱动：选项下发 → zcat 注册 → loadfile → 事件循环等 END_FILE。
/// ctx 的销毁由调用方负责（错误路径同样要销毁实例）
fn drive_encode(
    ctx: *mut ffi::MpvHandle,
    vfs: &Arc<dyn ArchiveVFS>,
    entries: &[VirtualEntry],
    fmt: &EncodeFormat,
    out_abs: &Path,
    opts: &EncodeOptions,
    cancel: &AtomicBool,
    progress: &dyn Fn(f64),
    last_err: &mut String,
) -> Result<(), String> {
    // headless：vo 必须显式设为 lavc——命令行 --o 会隐式把 vo 切到 lavc，
    // 但运行时 API 不做该隐式切换；若留 vo=null，文件被当普通播放消费、
    // 不产生输出。ao=null 丢弃音频（dump 序列无音轨）；
    // cache=no 让读请求直打 ConcatFile（e2e 播放测试同款）
    for (name, val) in [
        ("vo", "lavc"),
        ("ao", "null"),
        ("cache", "no"),
        ("terminal", "no"),
    ] {
        ffi::set_option_string(ctx, name, val)
            .map_err(|e| format!("设置 mpv 选项 {name} 失败: {e}"))?;
    }
    // 编码模式选项
    ffi::set_option_string(ctx, "o", &out_abs.to_string_lossy())
        .map_err(|e| format!("设置编码输出失败: {e}"))?;
    ffi::set_option_string(ctx, "ovc", &opts.vcodec)
        .map_err(|e| format!("设置编码器 [{}] 失败: {e}（该编码器可能不在本 libmpv 构建内）", opts.vcodec))?;
    // x264 默认 bframes=3 → 首帧 DTS 为负 → mp4 muxer 写 edit list 补偿 →
    // ffmpeg 系解码器按 edit list 裁掉末帧（e2e 实证：10 帧只解出 9，
    // 丢的是亮度 225 的最后一帧）。dump 序列不需要 B 帧，默认强制 bf=0；
    // 注意必须用 ffmpeg 通用名 bf（AVCodecContext.max_b_frames）——x264
    // 私有名 bframes 会被包装层用 avctx 值覆写，实证设了不生效。
    // 用户 ovcopts 显式指定 bf=/bframes= 时以其为准。
    let user_opts = opts.ovcopts.as_deref().unwrap_or("");
    let has_bf = user_opts.contains("bf=") || user_opts.contains("bframes");
    let eff_opts = if has_bf {
        user_opts.to_string()
    } else if user_opts.is_empty() {
        "bf=0".into()
    } else {
        format!("{user_opts},bf=0")
    };
    ffi::set_option_string(ctx, "ovcopts", &eff_opts)
        .map_err(|e| format!("设置编码器参数失败: {e}"))?;
    // mp4 系容器禁用 edit list：mpv 编码路径发出的包 duration=0，movenc
    // 用 PTS 差推算，末包时长 0 → track/elst 时长比真实内容少一帧 →
    // libavformat 系解码器（mpv/ffmpeg 均是）按 elst 裁掉末帧
    // （实证：10 帧只解出 9，ffmpeg -ignore_editlist 1 可解出全部 10 帧）。
    // bf=0 已消除负 DTS，本就不需要 elst 补偿。
    if let Some(ext) = out_abs
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
    {
        if matches!(ext.as_str(), "mp4" | "m4v" | "mov") {
            ffi::set_option_string(ctx, "ofopts", "use_editlist=0")
                .map_err(|e| format!("设置封装选项失败: {e}"))?;
        }
    }
    // 选项全部就位后才初始化（o/ovc 在 initialize 后设置不生效，
    // 见 create_uninit 的注释）；失败时上层 encode_raw_sequence 统一 destroy
    ffi::initialize(ctx).map_err(|e| format!("mpv 初始化失败: {e}"))?;
    ffi::request_log_messages(ctx, "error")
        .map_err(|e| format!("订阅 mpv 日志失败: {e}"))?;

    // zcat 协议：read/seek/size/close 复用 zvf 的协议无关回调
    ffi::stream_cb_add_ro(ctx, "zcat", std::ptr::null_mut(), zcat_open_cb)
        .map_err(|e| format!("注册 zcat:// 协议失败: {e}"))?;
    let zcat_id = zcat_register(vfs.clone(), entries.to_vec(), fmt.bit_shift);
    let cleanup_zcat = || zcat_unregister(zcat_id);

    // demuxer 选项与播放链路（ui::extract raw_demux_opts）同构
    let demux_opts = format!(
        "demuxer=rawvideo,demuxer-rawvideo-w={},demuxer-rawvideo-h={},\
         demuxer-rawvideo-mp-format={},demuxer-rawvideo-fps={}",
        fmt.width, fmt.height, fmt.pixfmt, fmt.fps
    );
    let load = ffi::command_string(
        ctx,
        &format!("loadfile zcat://{zcat_id} replace -1 {demux_opts}"),
    );
    if let Err(e) = load {
        cleanup_zcat();
        return Err(format!("loadfile 失败: {e}"));
    }

    // ---- 事件循环：END_FILE 定成败，周期取进度 ----
    let mut saw_start = false;
    let mut end: Option<(i32, i32)> = None; // (reason, error)
    let mut last_pct = -1.0f64;
    let mut progress_tick = Instant::now();
    let mut stop_sent = false;
    let result = loop {
        // 取消：下发 stop，等 END_FILE(STOP) 落定后退出循环
        if cancel.load(Ordering::Relaxed) && !stop_sent && end.is_none() {
            let _ = ffi::command_string(ctx, "stop");
            stop_sent = true;
        }
        let ev = unsafe { ffi::wait_event(ctx, 0.25) };
        let id = unsafe { (*ev).event_id };
        match id {
            ffi::MPV_EVENT_LOG_MESSAGE => {
                let m = unsafe { &*((*ev).data as *const ffi::MpvEventLogMessage) };
                let text = unsafe { CStr::from_ptr(m.text).to_string_lossy() };
                if m.log_level <= ffi::MPV_LOG_LEVEL_ERROR {
                    *last_err = text.trim().to_string();
                }
            }
            ffi::MPV_EVENT_START_FILE => saw_start = true,
            ffi::MPV_EVENT_END_FILE => {
                let ef = unsafe { &*((*ev).data as *const ffi::MpvEventEndFile) };
                // 无 replace 场景不会有旧文件伪事件；saw_start 守卫仅为防御
                if saw_start {
                    end = Some((ef.reason, ef.error));
                }
            }
            ffi::MPV_EVENT_SHUTDOWN => break Ok::<(), String>(()),
            _ => {}
        }
        if let Some((reason, err)) = end {
            break match reason {
                // mpv_end_file_reason EOF=0：编码完成，编码器已收尾写盘
                0 => Ok(()),
                ffi::MPV_END_FILE_REASON_STOP if stop_sent => Err("已取消".into()),
                _ => Err(format!(
                    "编码中止（end_file_reason={} error={}）{}",
                    reason,
                    err,
                    if last_err.is_empty() {
                        String::new()
                    } else {
                        format!("：{last_err}")
                    }
                )),
            };
        }
        // 进度上报（250ms 节流）：编码全速跑，percent-pos 按播放位置推算
        if progress_tick.elapsed().as_millis() >= 250 {
            progress_tick = Instant::now();
            if let Some(p) = current_percent(ctx) {
                if (p - last_pct).abs() >= 0.5 {
                    last_pct = p;
                    progress(p);
                }
            }
        }
    };
    cleanup_zcat();
    let res = result.and_then(|_| {
        // 成功路径的产出校验：mpv END_FILE(EOF) 正常但输出缺失/空文件
        // 属于异常（如容器与编码器不兼容被静默吞掉）
        match std::fs::metadata(out_abs) {
            Ok(m) if m.len() > 0 => Ok(()),
            _ => Err(format!(
                "编码结束但输出文件无效：{}{}",
                out_abs.display(),
                if last_err.is_empty() {
                    String::new()
                } else {
                    format!("（{last_err}）")
                }
            )),
        }
    });
    if !res.is_err() {
        progress(100.0);
    }
    res
}

/// 编码进度（0~100）：优先 percent-pos；rawvideo 无时长元数据的边缘
/// 情况退化为 time-pos / duration 手算
fn current_percent(ctx: *mut ffi::MpvHandle) -> Option<f64> {
    if let Some(p) = ffi::get_property_f64(ctx, "percent-pos").ok().flatten() {
        return Some(p.clamp(0.0, 100.0));
    }
    let t = ffi::get_property_f64(ctx, "time-pos").ok().flatten()?;
    let d = ffi::get_property_f64(ctx, "duration").ok().flatten()?;
    (d > 0.0).then(|| (t / d * 100.0).clamp(0.0, 100.0))
}

// ---- zcat:// 协议：拼接流注册表与 open_cb ----

struct ZcatHandle {
    vfs: Arc<dyn ArchiveVFS>,
    entries: Vec<VirtualEntry>,
    bit_shift: u32,
}

/// 全局拼接流注册表：引擎线程注册，mpv 回调线程查找（与 zvf 的 REGISTRY
/// 同模式；条目在编码结束（成功/失败/取消）后注销）
static ZCAT_REGISTRY: Mutex<Option<HashMap<u64, ZcatHandle>>> = Mutex::new(None);
static ZCAT_SEQ: AtomicU64 = AtomicU64::new(1);

fn zcat_register(vfs: Arc<dyn ArchiveVFS>, entries: Vec<VirtualEntry>, bit_shift: u32) -> u64 {
    let id = ZCAT_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut g = ZCAT_REGISTRY.lock().unwrap();
    g.get_or_insert_with(HashMap::new).insert(
        id,
        ZcatHandle {
            vfs,
            entries,
            bit_shift,
        },
    );
    id
}

fn zcat_unregister(id: u64) {
    if let Some(m) = ZCAT_REGISTRY.lock().unwrap().as_mut() {
        m.remove(&id);
    }
}

/// zcat://<id> 的 open_cb：取注册条目 → 打开全部文件拼 ConcatFile →
/// bit_shift 修正 → 挂 zvf 的协议无关回调。
/// 回调在 mpv 内部线程执行——禁止调用任何 mpv API（死锁）
unsafe extern "C" fn zcat_open_cb(
    _user_data: *mut c_void,
    uri: *mut c_char,
    info: *mut ffi::StreamCbInfo,
) -> i32 {
    let uri = CStr::from_ptr(uri).to_string_lossy();
    let Some(id) = uri.strip_prefix("zcat://").and_then(|s| s.parse::<u64>().ok()) else {
        return ffi::MPV_ERROR_LOADING_FAILED;
    };
    let (vfs, entries, bit_shift) = {
        let g = ZCAT_REGISTRY.lock().unwrap();
        match g.as_ref().and_then(|m| m.get(&id)) {
            Some(h) => (h.vfs.clone(), h.entries.clone(), h.bit_shift),
            None => return ffi::MPV_ERROR_LOADING_FAILED,
        }
    };
    let mut files = Vec::with_capacity(entries.len());
    for e in &entries {
        match vfs.open_entry(e) {
            Ok(f) => files.push(f),
            Err(err) => {
                tracing::error!("zcat 打开条目失败 {}: {err}", e.name);
                return ffi::MPV_ERROR_LOADING_FAILED;
            }
        }
    }
    let cat: Box<dyn VirtualFile> = Box::new(ConcatFile::new(files));
    // bit_shift 修正与播放链路同源（ShiftReader）：流层 per-u16 左移
    let vfile = if bit_shift > 0 {
        Box::new(zvf_cb::ShiftReader::new(cat, bit_shift)) as Box<dyn VirtualFile>
    } else {
        cat
    };
    // 双重装箱语义见 zvf open_cb：外层 Box 瘦指针作 cookie
    (*info).cookie = Box::into_raw(Box::new(vfile)) as *mut c_void;
    (*info).read_fn = Some(zvf_cb::read_cb);
    (*info).seek_fn = Some(zvf_cb::seek_cb);
    (*info).size_fn = Some(zvf_cb::size_cb);
    (*info).close_fn = Some(zvf_cb::close_cb);
    (*info).cancel_fn = None;
    ffi::MPV_ERROR_SUCCESS
}

// ---- ConcatFile：多文件逻辑拼接流 ----

/// 把 N 个 VirtualFile 按序拼成一条连续字节流（Read/Seek/size 全逻辑视图，
/// 不落盘）。rawvideo demuxer 只认单条连续流——多 dump 序列的编码输入即
/// 由此实现。跨文件 seek 由前缀和数组定位。
struct ConcatFile {
    files: Vec<Box<dyn VirtualFile>>,
    /// starts[i] = files[i] 的逻辑起始偏移（前缀和）
    starts: Vec<u64>,
    total: u64,
    /// 当前活跃文件下标（read/seek 的局部性缓存）
    cur: usize,
    /// 逻辑偏移（SeekFrom::Current 计算用）
    pos: u64,
}

impl ConcatFile {
    fn new(files: Vec<Box<dyn VirtualFile>>) -> Self {
        let mut starts = Vec::with_capacity(files.len());
        let mut acc = 0u64;
        for f in &files {
            starts.push(acc);
            acc += f.size();
        }
        let total = acc;
        Self {
            files,
            starts,
            total,
            cur: 0,
            pos: 0,
        }
    }

    /// 逻辑偏移 → (文件下标, 文件内偏移)。partition_point 找最后一个
    /// starts[i] <= target 的 i（target == total 时停在末文件末尾，
    /// 内偏移 = 文件大小——后续 read 返回 0 即整体 EOF）
    fn locate(&self, target: u64) -> (usize, u64) {
        let i = self.starts.partition_point(|&s| s <= target) - 1;
        (i, target - self.starts[i])
    }
}

impl Read for ConcatFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut filled = 0usize;
        while filled < buf.len() {
            if self.cur >= self.files.len() {
                break; // 全部文件读完
            }
            let n = self.files[self.cur].read(&mut buf[filled..])?;
            if n == 0 {
                self.cur += 1; // 本文件 EOF，切下一个继续填
                continue;
            }
            filled += n;
            self.pos += n as u64;
        }
        Ok(filled)
    }
}

impl Seek for ConcatFile {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let target: i64 = match pos {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::Current(d) => self.pos as i64 + d,
            SeekFrom::End(d) => self.total as i64 + d,
        };
        if target < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek 到负偏移",
            ));
        }
        // 越界 clamp 到 total（与 File 语义一致：可 seek 超 EOF，读得 0）
        let target = target.min(self.total as i64) as u64;
        let (i, off) = self.locate(target);
        let real = self.files[i].seek(SeekFrom::Start(off))?;
        debug_assert_eq!(real, off);
        self.cur = i;
        self.pos = target;
        Ok(target)
    }
}

impl VirtualFile for ConcatFile {
    fn size(&self) -> u64 {
        self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ---- ConcatFile 纯逻辑测试（无 mpv/dll 依赖）----

    /// 造临时目录 + DirVFS + 指定内容的文件序列，返回 (vfs, 目录路径)。
    /// 目录名带原子序号：并行测试互不干扰（dir_vfs 测试同惯例）
    fn fixture(files: &[(&str, Vec<u8>)]) -> (PathBuf, Arc<dyn ArchiveVFS>) {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("zcat_cat_{}_{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, data) in files {
            std::fs::write(dir.join(name), data).unwrap();
        }
        let vfs: Arc<dyn ArchiveVFS> = Arc::new(
            crate::vfs::dir_vfs::DirVFS::open(&dir, |_| true, 1).expect("DirVFS 打开失败"),
        );
        (dir, vfs)
    }

    fn open_all(vfs: &Arc<dyn ArchiveVFS>) -> Vec<Box<dyn VirtualFile>> {
        let entries = vfs.list();
        assert!(!entries.is_empty());
        entries
            .iter()
            .map(|e| vfs.open_entry(e).unwrap())
            .collect()
    }

    #[test]
    fn concat_read_across_boundaries() {
        // 三文件（含中间空文件）：跨界读必须无缝拼接
        let (dir, vfs) = fixture(&[
            ("a.bin", vec![1, 2, 3]),
            ("b.bin", vec![]),
            ("c.bin", vec![4, 5, 6, 7]),
        ]);
        let files = open_all(&vfs);
        assert_eq!(files.len(), 3);
        let mut cat = ConcatFile::new(files);
        assert_eq!(cat.size(), 7);

        // 一次性读全部（跨 a→b(空)→c 边界）
        let mut buf = vec![0u8; 7];
        assert_eq!(cat.read(&mut buf).unwrap(), 7);
        assert_eq!(buf, vec![1, 2, 3, 4, 5, 6, 7]);

        // 整体 EOF：再读得 0
        assert_eq!(cat.read(&mut buf).unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concat_seek_semantics() {
        let (dir, vfs) = fixture(&[
            ("a.bin", (1..=3u8).collect()),
            ("b.bin", (4..=6u8).collect()),
            ("c.bin", (7..=9u8).collect()),
        ]);
        let mut cat = ConcatFile::new(open_all(&vfs));

        let mut one = [0u8; 1];
        // seek 到每个文件内部
        for (off, expect) in [(0u64, 1u8), (2, 3), (3, 4), (5, 6), (6, 7), (8, 9)] {
            assert_eq!(cat.seek(SeekFrom::Start(off)).unwrap(), off);
            assert_eq!(cat.read(&mut one).unwrap(), 1);
            assert_eq!(one[0], expect);
        }
        // Current / End 语义
        cat.seek(SeekFrom::Start(0)).unwrap();
        assert_eq!(cat.seek(SeekFrom::Current(4)).unwrap(), 4);
        assert_eq!(cat.seek(SeekFrom::End(-2)).unwrap(), 7);
        // 尾部 EOF 与超尾 clamp
        assert_eq!(cat.seek(SeekFrom::Start(9)).unwrap(), 9);
        assert_eq!(cat.read(&mut one).unwrap(), 0);
        assert_eq!(cat.seek(SeekFrom::Start(100)).unwrap(), 9);
        // 负偏移报错
        assert!(cat.seek(SeekFrom::End(-10)).is_err());
        // seek 回头读（下标重定位正确性）
        assert_eq!(cat.seek(SeekFrom::Start(1)).unwrap(), 1);
        let mut two = [0u8; 2];
        assert_eq!(cat.read(&mut two).unwrap(), 2);
        assert_eq!(two, [2, 3]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 引擎 e2e（需 libmpv-2.dll + ffmpeg，缺任一则跳过；模式同
    //      player::tests 的 e2e_rawvideo_dump_with_demux_opts）----

    fn find_mpv_dll_dir() -> Option<PathBuf> {
        let mut cands: Vec<PathBuf> = vec![];
        if let Ok(exe) = std::env::current_exe() {
            if let Some(d) = exe.parent() {
                cands.push(d.to_path_buf());
                if let Some(dd) = d.parent() {
                    cands.push(dd.to_path_buf());
                }
            }
        }
        if let Ok(m) = std::env::var("CARGO_MANIFEST_DIR") {
            for prof in ["debug", "release"] {
                cands.push(PathBuf::from(&m).join("target").join(prof));
            }
        }
        cands.into_iter().find(|d| d.join("libmpv-2.dll").exists())
    }

    /// ffprobe 探测（rar_vfs::rar_exe 同款三级模式）：
    /// PATH → ARCHVIEW_TEST_FFMPEG 环境变量 → 本机固定目录。
    /// 返回 exe 完整路径（调用处不再裸用 "ffprobe"，防 PATH 无工具时静默跳过）
    fn ffprobe_exe() -> Option<PathBuf> {
        if std::process::Command::new("ffprobe")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Some(PathBuf::from("ffprobe"));
        }
        if let Ok(p) = std::env::var("ARCHVIEW_TEST_FFMPEG") {
            let pb = PathBuf::from(p);
            // 环境变量给目录或 ffprobe.exe 完整路径均可
            let cand = if pb.extension().is_some() { pb } else { pb.join("ffprobe.exe") };
            if cand.exists() {
                return Some(cand);
            }
        }
        // 本机固定位置（换机时设 ARCHVIEW_TEST_FFMPEG 或改这里）
        let local = PathBuf::from(r"D:\11_software\ffprobe.exe");
        local.exists().then_some(local)
    }

    /// nv12 dump 序列（分 2 个文件）→ 编码 → 第二个 mpv 实例回读输出验证
    /// 宽高/时长；有 ffprobe 时再精确断言帧数。
    /// 覆盖：zcat 拼接流喂 rawvideo demuxer、编码选项下发、END_FILE(EOF)、
    /// 输出文件有效性
    #[test]
    fn e2e_encode_nv12_sequence() {
        let Some(dll_dir) = find_mpv_dll_dir() else {
            eprintln!("跳过：未找到 libmpv-2.dll");
            return;
        };
        // 造数据（纯 Rust，不依赖 ffmpeg）：64x48 nv12 每帧 4608B
        // （Y 平面 3072 + UV 平面 1536），每帧恒定亮度（帧号×25）、
        // 中性色度 128——内容只需确定性，rawvideo 编码不在乎语义
        let (w, h, n, fps) = (64usize, 48usize, 10usize, 10.0f64);
        let ysz = w * h;
        let dir = std::env::temp_dir().join(format!("zcat_enc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let frames: Vec<Vec<u8>> = (0..n)
            .map(|i| {
                let mut f = vec![0u8; ysz * 3 / 2];
                f[..ysz].fill((i * 25) as u8);
                f[ysz..].fill(128);
                f
            })
            .collect();
        std::fs::write(dir.join("seq_a.nv12"), frames[..5].concat()).unwrap();
        std::fs::write(dir.join("seq_b.nv12"), frames[5..].concat()).unwrap();

        // 壳侧职责：谓词过滤 + 按文件名升序 = 编码顺序（a 先于 b）
        let vfs: Arc<dyn ArchiveVFS> = Arc::new(
            crate::vfs::dir_vfs::DirVFS::open(&dir, |n| n.starts_with("seq_"), 1).unwrap(),
        );
        let mut entries: Vec<VirtualEntry> = vfs.list();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(entries.len(), 2);

        let out = dir.join("out.mp4");
        let fmt = EncodeFormat {
            pixfmt: "nv12".into(),
            width: w as u32,
            height: h as u32,
            fps,
            bit_shift: 0,
        };
        let cancel = AtomicBool::new(false);
        let stats = encode_raw_sequence(
            &dll_dir,
            &vfs,
            &entries,
            &fmt,
            &out,
            &EncodeOptions::default(),
            &cancel,
            &|_| {},
        )
        .expect("编码失败");
        assert_eq!(stats.frames, 10);
        assert_eq!(stats.files, 2);
        assert_eq!(stats.total_bytes, 10 * 64 * 48 * 12 / 8);
        assert!(out.metadata().map(|m| m.len() > 0).unwrap_or(false));

        // ---- 回读验证：headless mpv 播放输出，读 width/height/duration ----
        // （mpv 即探测器：demux 完成后属性即可读，比 ffprobe 少一个外部依赖；
        //   帧数精确断言见下方可选的 ffprobe 段）
        let vctx = ffi::create().expect("验证实例创建失败");
        let probe = (|| -> Result<(f64, f64, f64), String> {
            for (name, val) in [("vo", "null"), ("ao", "null"), ("cache", "no")] {
                ffi::set_option_string(vctx, name, val)
                    .map_err(|e| format!("设置 {name} 失败: {e}"))?;
            }
            // mpv 命令解析中引号内 \ 是转义符，Windows 路径需转正斜杠
            let url = out.display().to_string().replace('\\', "/");
            ffi::command_string(vctx, &format!("loadfile \"{url}\""))
                .map_err(|e| format!("loadfile 失败: {e}"))?;
            let deadline = Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let ev = unsafe { ffi::wait_event(vctx, 0.05) };
                let ended = matches!(
                    unsafe { (*ev).event_id },
                    ffi::MPV_EVENT_END_FILE | ffi::MPV_EVENT_SHUTDOWN
                );
                let triple = (
                    ffi::get_property_f64(vctx, "width").ok().flatten(),
                    ffi::get_property_f64(vctx, "height").ok().flatten(),
                    ffi::get_property_f64(vctx, "duration").ok().flatten(),
                );
                if let (Some(w), Some(h), Some(d)) = triple {
                    return Ok((w, h, d));
                }
                if ended {
                    return Err("输出未产生可播放的视频流".into());
                }
                if Instant::now() > deadline {
                    return Err("验证实例 10s 内未就绪".into());
                }
            }
        })();
        ffi::destroy(vctx);
        let (vw, vh, vd) = probe.expect("输出回读失败");
        assert_eq!((vw as usize, vh as usize), (w, h), "输出宽高应为 {w}x{h}");
        let want_d = n as f64 / fps;
        assert!(
            (vd - want_d).abs() < 0.25,
            "时长应约 {want_d}s（{n} 帧 @ {fps}fps）: {vd}"
        );

        // ---- 可选增强：ffprobe 精确帧数（缺则跳过，核心链路已由上面覆盖） ----
        if let Some(ffprobe) = ffprobe_exe() {
            let st = std::process::Command::new(&ffprobe)
                .args([
                    "-v",
                    "error",
                    "-count_frames",
                    "-select_streams",
                    "v:0",
                    "-show_entries",
                    "stream=nb_read_frames,width,height",
                    "-of",
                    "csv=p=0",
                ])
                .arg(&out)
                .output()
                .unwrap();
            assert!(st.status.success(), "ffprobe 失败");
            let csv = String::from_utf8_lossy(&st.stdout).into_owned();
            let vals: Vec<&str> = csv.trim().split(',').collect();
            assert_eq!(vals[0], "64", "宽度应为 64: {csv}");
            assert_eq!(vals[1], "48", "高度应为 48: {csv}");
            assert_eq!(vals[2], "10", "帧数应为 10: {csv}");
        } else {
            eprintln!("ffprobe 不可用，跳过精确帧数断言");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 校验与取消路径（无 mpv 依赖的纯校验分支）
    #[test]
    fn validation_and_cancel() {
        let (dir, vfs) = fixture(&[("a.nv12", vec![0u8; 10])]);
        let entries = vfs.list();

        // 大小不整除帧
        let fmt = EncodeFormat {
            pixfmt: "nv12".into(),
            width: 4,
            height: 4,
            fps: 10.0,
            bit_shift: 0,
        };
        let err = encode_raw_sequence(
            Path::new("x"),
            &vfs,
            &entries,
            &fmt,
            Path::new("never.mp4"),
            &EncodeOptions::default(),
            &AtomicBool::new(false),
            &|_| {},
        )
        .unwrap_err();
        assert!(err.contains("整数倍"), "应报整除校验错误: {err}");

        // 预置取消：引擎在 mpv 实例创建前就应退出（dll 缺失也能走通该分支）
        let cancel = AtomicBool::new(true);
        // 造整除数据：4x4 nv12 帧大小 24B
        let (dir2, vfs2) = fixture(&[("a.nv12", vec![7u8; 24])]);
        let entries2 = vfs2.list();
        let r = encode_raw_sequence(
            Path::new("no_such_dll_dir"),
            &vfs2,
            &entries2,
            &fmt,
            Path::new("never.mp4"),
            &EncodeOptions::default(),
            &cancel,
            &|_| {},
        );
        // 取消优先于 dll 加载失败：预置取消路径不触碰 dll，返回确定文案
        assert_eq!(r.unwrap_err(), "已取消");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }
}
