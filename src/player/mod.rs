//! 播放服务：持有 mpv 实例，封装命令/属性访问。
//! UI 线程专用（mpv handle 本身线程安全，但回调线程不得触碰，见 stream_cb.rs）。

pub mod mpv_ffi;
pub mod stream_cb;

use crate::config::Config;
use glow::HasContext as _;
use std::cell::Cell;
use std::ffi::{c_int, c_void, CStr};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// mpv render API 的 get_proc_address 回调。glow 的加载器私有于 Context、不对外暴露，
/// Windows 上直接走系统 API：扩展函数 wglGetProcAddress，GL 1.x 固定函数从
/// opengl32.dll 导出表兜底（libmpv-rs 同款做法）；user_data 不需要（传 null_mut()）。
#[cfg(windows)]
unsafe extern "C" fn gpa_trampoline(
    _user: *mut c_void,
    name: *const std::ffi::c_char,
) -> *mut c_void {
    use windows_sys::Win32::Graphics::OpenGL::wglGetProcAddress;
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    let sym = std::ffi::CStr::from_ptr(name).as_ptr().cast::<u8>();
    if let Some(f) = wglGetProcAddress(sym) {
        return f as *const () as *mut c_void;
    }
    let module: Vec<u16> = "opengl32.dll\0".encode_utf16().collect();
    let lib = GetModuleHandleW(module.as_ptr());
    match GetProcAddress(lib, sym) {
        Some(f) => f as *const () as *mut c_void,
        None => std::ptr::null_mut(),
    }
}

/// mpv 帧更新回调：request_repaint 唤醒 egui（Context 线程安全，专为跨线程唤醒设计）。
/// 回调无返回值（render.h: typedef void (*mpv_render_update_fn)(void *cb_ctx)）
unsafe extern "C" fn update_trampoline(user: *mut c_void) {
    let c = &*(user as *const egui::Context);
    c.request_repaint();
}

/// 创建离屏 FBO + RGBA 纹理（mpv 渲染目标），并清成黑底防首帧前闪未定义内容
fn make_fbo(gl: &glow::Context, w: i32, h: i32) -> (glow::NativeFramebuffer, glow::NativeTexture) {
    unsafe {
        let texture = gl.create_texture().unwrap();
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA as i32,
            w,
            h,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(None),
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_S,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_T,
            glow::CLAMP_TO_EDGE as i32,
        );
        let fbo = gl.create_framebuffer().unwrap();
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(texture),
            0,
        );
        gl.clear_color(0.0, 0.0, 0.0, 1.0);
        gl.clear(glow::COLOR_BUFFER_BIT);
        debug_assert_eq!(
            gl.check_framebuffer_status(glow::FRAMEBUFFER),
            glow::FRAMEBUFFER_COMPLETE,
            "视频离屏 FBO 不完整，mpv 渲染将静默失败"
        );
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        gl.bind_texture(glow::TEXTURE_2D, None);
        (fbo, texture)
    }
}

/// render API 状态（ARCHITECTURE.md 13）。生存到 shutdown_render（eframe on_exit，
/// GL current 线程、早于 mpv terminate）——顺序不可颠倒。
struct RenderState {
    rctx: *mut mpv_ffi::MpvRenderContext,
    fbo: glow::NativeFramebuffer,
    texture: glow::NativeTexture,
    w: i32,
    h: i32,
    /// FFI user_data 薄指针的原始 Box（闭包/Context 不能直接过 FFI），shutdown 时按原型回收
    update_user_data: *mut egui::Context,
}

pub struct PlayerService {
    ctx: *mut mpv_ffi::MpvHandle,
    render: Option<RenderState>,
    /// mpv 日志泵线程停止标志（Drop 先停泵再 destroy，避免泵线程 use-after-free）
    log_stop: Option<Arc<AtomicBool>>,
    log_thread: Option<std::thread::JoinHandle<()>>,
    /// loadfile 提交时刻 + 是否已记录首渲染（诊断切换延迟的时间线锚点）
    load_mark: Cell<Option<Instant>>,
    painted_since_load: Cell<bool>,
    /// mpv 是否已产出首个视频帧（PaintCallback 闭包内经 render_context_update
    /// 检出 MPV_RENDER_UPDATE_FRAME 时置位）。与 painted_since_load 分离的原因：
    /// 后者在"提交绘制"时置位会早于真实帧就绪 → 16ms 高频轮询过早停止，
    /// 首帧实际呈现回落到 100ms 慢节拍 ≈ 200ms 切换延迟（LOG 实测）
    first_frame: Arc<AtomicBool>,
    /// 打开完成标志（事件泵线程收到 PLAYBACK_RESTART 时置位，load 时重置）。
    /// 首帧 ≠ 打开完成：首帧实测 7~20ms 即到，但音频初始化/cache 填充还要
    /// 占 core 锁 ~0.5s，此窗口内 UI 属性读（paused/time_pos/eof/buffering）
    /// 每次都要等锁——帧探针实测切换后连续长帧合计 ~0.5s，即卡顿主体。
    /// opening() 门控因此持续到本标志置位而非首帧
    playback_ready: Arc<AtomicBool>,
    /// pause 本地缓存（observe_property 订阅，事件泵收到 PROPERTY_CHANGE 更新）。
    /// 变更源：toggle_pause 的 cycle 命令、EOF 时 keep-open 置 pause=yes——
    /// 推送同步比门控读更及时准确。mute 不走此机制：静音状态唯一真相在
    /// cfg.player.muted（UI 直读写，播放器创建时应用），无 mpv 回读需求
    paused_cache: Arc<AtomicBool>,
    /// time-pos 本地缓存（f64 位模式存 AtomicU64，DOUBLE 推送）。变更源：
    /// playloop 每迭代推进、seek 后跳变。推送速率 ≈ 帧率（每迭代每属性
    /// 至多 1 条），泵线程纯字节比较 + 原子写，消费能力高出生产上限数个
    /// 数量级，无洪流风险
    time_pos_cache: Arc<AtomicU64>,
    /// duration 本地缓存（DOUBLE 推送；每文件装载时 1~2 条）。0.0 = 不可用
    duration_cache: Arc<AtomicU64>,
    /// eof-reached 本地缓存（FLAG 推送）。seek 卡顿实证：seek 后首帧的
    /// get_property 在 core 锁上等 ~194ms（core 完成 seek 才放锁），故改订阅
    eof_cache: Arc<AtomicBool>,
    /// pause-for-cache 本地缓存（FLAG 推送；缓冲暂停翻转时推）
    buffering_cache: Arc<AtomicBool>,
    /// 配置原文 hwdec："auto" 时启用探测记忆（hwdec_fixed），其余取值原样生效
    hwdec_cfg: String,
    /// hwdec 探测记忆：首次成功解码后 hwdec-current 的取值（如 "d3d11va-copy"）。
    /// 配置为 auto 时，后续装载以 loadfile 的 per-file 选项固定该方法，跳过
    /// auto 试探链（每次试探都要创建解码设备、占 core 锁）。方法取决于
    /// GPU/驱动而非文件，进程内记忆即可；遇到该方法不支持的编码 mpv 会
    /// 自动回落软解，无失败风险。不能运行时 set 属性下发——旧视频还在播，
    /// 改属性会立即重建旧解码器（实测白耗 60ms）
    hwdec_fixed: std::cell::RefCell<Option<String>>,
}

// mpv client API 线程安全；回调线程只走全局注册表，不触碰此 handle。
unsafe impl Send for PlayerService {}

/// mpv render context 句柄包装：egui PaintCallback 闭包按字段捕获（2021 edition
/// disjoint capture），裸字段必须自带 Send/Sync；实际使用恒在 UI/GL 线程
/// （mpv render 调用要求与创建同线程），安全
#[derive(Clone, Copy)]
pub struct MpvRenderCtx(pub *mut mpv_ffi::MpvRenderContext);
impl MpvRenderCtx {
    /// 方法调用（而非 .0 字段访问）保证闭包捕获止步于本 newtype
    pub fn as_ptr(self) -> *mut mpv_ffi::MpvRenderContext {
        self.0
    }
}
unsafe impl Send for MpvRenderCtx {}
unsafe impl Sync for MpvRenderCtx {}

/// PaintCallback 绘制闭包捕获用的全部值（不引用 PlayerService）。
/// 裸指针字段经 newtype 自带 Send/Sync；first_frame 为 Arc 共享原子——
/// 闭包内写入、UI 线程读取。实际使用恒在 UI/GL 线程（mpv render 调用
/// 要求与创建同线程），安全
#[derive(Clone)]
pub struct RenderInfo {
    pub rctx: MpvRenderCtx,
    /// glow FBO 裸名（回调内经 NativeFramebuffer(name) 重建绑定）
    pub fbo: u32,
    pub w: i32,
    pub h: i32,
    /// 首帧检出标志：闭包内 render_context_update 返回 FRAME 位时置位
    pub first_frame: Arc<AtomicBool>,
}

impl PlayerService {
    pub fn new(cfg: &Config, gl: &glow::Context, egui_ctx: &egui::Context) -> Result<Self, String> {
        let ctx = mpv_ffi::create()?;
        let mut s = Self {
            ctx,
            render: None,
            log_stop: None,
            log_thread: None,
            load_mark: Cell::new(None),
            painted_since_load: Cell::new(true),
            first_frame: Arc::new(AtomicBool::new(true)),
            playback_ready: Arc::new(AtomicBool::new(true)),
            paused_cache: Arc::new(AtomicBool::new(false)),
            time_pos_cache: Arc::new(AtomicU64::new(0.0f64.to_bits())),
            duration_cache: Arc::new(AtomicU64::new(0.0f64.to_bits())),
            eof_cache: Arc::new(AtomicBool::new(false)),
            buffering_cache: Arc::new(AtomicBool::new(false)),
            hwdec_cfg: cfg.player.hwdec.clone(),
            hwdec_fixed: std::cell::RefCell::new(None),
        };
        // 播放器参数全部来自 config.toml
        // 常驻事件泵线程：不随日志级别启停（此前 level=no 时整线程不跑，
        // PLAYBACK_RESTART 收不到）。wait_event 同一 client 只能单线程消费
        // （此处是进程内唯一消费者）。
        // 职责 1（恒定）：PLAYBACK_RESTART → playback_ready 置位。这是
        //   "打开完成、core 空闲"的准确信号（音频已起、cache 就绪），opening
        //   门控据此解除——首帧远早于它，详见 playback_ready 字段注释
        // 职责 2（level != no）：LOG_MESSAGE 转 owned 后走 tracing（带相对
        //   时间戳），与生命周期事件打点对齐成完整时间线（诊断切换延迟）
        let log_on = cfg.player.mpv_log_level != "no";
        if log_on {
            if let Err(e) = mpv_ffi::request_log_messages(ctx, &cfg.player.mpv_log_level) {
                tracing::warn!("订阅 mpv 日志失败（事件泵仍启动）: {e}");
            }
        }
        {
            let stop = Arc::new(AtomicBool::new(false));
            let stop2 = stop.clone();
            let ready_flag = s.playback_ready.clone();
            let paused_flag = s.paused_cache.clone();
            let eof_flag = s.eof_cache.clone();
            let buffering_flag = s.buffering_cache.clone();
            let time_pos_flag = s.time_pos_cache.clone();
            let duration_flag = s.duration_cache.clone();
            // 裸指针以 usize 过线程边界（mpv client API 线程安全）
            let ctx_addr = ctx as usize;
            let t0 = Instant::now();
            s.log_thread = Some(std::thread::spawn(move || {
                let c = ctx_addr as *mut mpv_ffi::MpvHandle;
                loop {
                    if stop2.load(Ordering::Relaxed) {
                        break;
                    }
                    let ev = unsafe { mpv_ffi::wait_event(c, 0.2) };
                    if ev.is_null() {
                        continue;
                    }
                    let ev = unsafe { &*ev };
                    match ev.event_id {
                        mpv_ffi::MPV_EVENT_NONE => {}
                        mpv_ffi::MPV_EVENT_SHUTDOWN => break,
                        // 打开完成：opening 门控解除信号（常驻泵的核心职责）
                        mpv_ffi::MPV_EVENT_PLAYBACK_RESTART => {
                            ready_flag.store(true, Ordering::Relaxed);
                        }
                        // 属性订阅推送：更新本地缓存（FLAG: pause/eof-reached/
                        // pause-for-cache，DOUBLE: time-pos/duration）。变更源含
                        // pause=yes、seek 后 time-pos 跳变、缓冲翻转等一切内外部
                        // 变化，统一经此同步。name/data 指针仅当次事件内有效，
                        // 此处即时取值，不外传裸指针。处理保持零分配（字节比较 +
                        // 原子写）：time-pos 推送速率 ≈ 帧率，泵消费能力高出生产
                        // 上限数个数量级。属性不可用时 mpv 推 format=NONE 且
                        // data=null（如装载前的 time-pos），自然被 data 判空跳过
                        mpv_ffi::MPV_EVENT_PROPERTY_CHANGE => {
                            if !ev.data.is_null() {
                                let p = unsafe { &*(ev.data as *mut mpv_ffi::MpvEventProperty) };
                                if !p.name.is_null() && !p.data.is_null() {
                                    let name = unsafe { CStr::from_ptr(p.name) }.to_bytes();
                                    if p.format == mpv_ffi::MPV_FORMAT_FLAG {
                                        let v = unsafe { *(p.data as *const c_int) } != 0;
                                        match name {
                                            b"pause" => paused_flag.store(v, Ordering::Relaxed),
                                            b"eof-reached" => eof_flag.store(v, Ordering::Relaxed),
                                            b"pause-for-cache" => {
                                                buffering_flag.store(v, Ordering::Relaxed)
                                            }
                                            _ => {}
                                        }
                                    } else if p.format == mpv_ffi::MPV_FORMAT_DOUBLE {
                                        let v = unsafe { *(p.data as *const f64) };
                                        match name {
                                            b"time-pos" => {
                                                time_pos_flag.store(v.to_bits(), Ordering::Relaxed)
                                            }
                                            b"duration" => {
                                                duration_flag.store(v.to_bits(), Ordering::Relaxed)
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                        }
                        mpv_ffi::MPV_EVENT_LOG_MESSAGE if log_on => {
                            // 字符串指针归 mpv 所有、仅当次事件有效：先转 owned 再格式化
                            let m = unsafe { &*(ev.data as *mut mpv_ffi::MpvEventLogMessage) };
                            let (prefix, text) = unsafe {
                                (
                                    CStr::from_ptr(m.prefix).to_string_lossy().into_owned(),
                                    CStr::from_ptr(m.text).to_string_lossy().into_owned(),
                                )
                            };
                            tracing::debug!(
                                "[mpv {:>8.3}] {}: {}",
                                t0.elapsed().as_secs_f64(),
                                prefix,
                                text.trim_end()
                            );
                        }
                        // 关键生命周期事件也打点：与日志行对齐成时间线
                        mpv_ffi::MPV_EVENT_START_FILE
                        | mpv_ffi::MPV_EVENT_END_FILE
                        | mpv_ffi::MPV_EVENT_FILE_LOADED
                        | mpv_ffi::MPV_EVENT_VIDEO_RECONFIG => {
                            tracing::debug!(
                                "[mpv {:>8.3}] == 事件 {} ==",
                                t0.elapsed().as_secs_f64(),
                                ev.event_id
                            );
                        }
                        _ => {}
                    }
                }
            }));
            s.log_stop = Some(stop);
            if log_on {
                tracing::debug!("mpv 日志泵已启动 (level={})", cfg.player.mpv_log_level);
            }
        }
        // 每帧读取的属性全部走 observe_property 订阅（须在泵线程消费前注册；
        // 事件会在 mpv 侧排队，泵起来即可收到含初始值在内的全部推送）。
        // 帧路径纯原子读，不再进 mpv——seek 后 core 忙窗口（实测 ~194ms）
        // 里任何 get_property 都会在 core 锁上阻塞 UI。time-pos/duration 为
        // DOUBLE，其余 FLAG
        for prop in ["pause", "eof-reached", "pause-for-cache"] {
            if let Err(e) = mpv_ffi::observe_property_flag(ctx, prop) {
                tracing::warn!("订阅 {prop} 属性失败（缓存将停在初始值 false）: {e}");
            }
        }
        for prop in ["time-pos", "duration"] {
            if let Err(e) = mpv_ffi::observe_property_double(ctx, prop) {
                tracing::warn!("订阅 {prop} 属性失败（缓存将停在初始值 0）: {e}");
            }
        }
        s.opt("hwdec", &cfg.player.hwdec);
        s.opt("cache-secs", &cfg.player.cache_secs.to_string());
        s.opt(
            "demuxer-max-bytes",
            &format!("{}MiB", cfg.player.demuxer_max_mb),
        );
        // demux 探测预算（打开阶段的字节/时长上限）：压缩包内流每读一字节都
        // 要解压，预算直接换算成打开耗时，调小即提速。"auto" = 不设置，
        // 走 mpv/FFmpeg 原生默认（probesize≈5MB、analyzeduration≈5s）
        if cfg.player.demuxer_probesize != "auto" {
            s.opt("demuxer-lavf-probesize", &cfg.player.demuxer_probesize);
        }
        if cfg.player.demuxer_analyzeduration != "auto" {
            s.opt(
                "demuxer-lavf-analyzeduration",
                &cfg.player.demuxer_analyzeduration,
            );
        }
        s.opt("keep-open", "always"); // 播完停在末帧，不触发退出
        s.opt("osc", "no"); // 禁用 mpv 内置 OSC（预览窗口里的白条进度条），进度条由 UI 提供
        s.opt("osd-level", "0"); // 不在预览窗口里弹 OSD 提示
        s.set_prop_str("volume", &cfg.player.volume.to_string());
        // 静音状态唯一真相在 cfg（未播放时也可预设），播放器创建时统一应用
        s.set_prop_str("mute", if cfg.player.muted { "yes" } else { "no" });
        // 核心：注册 zvf:// 自定义协议
        mpv_ffi::stream_cb_add_ro(ctx, "zvf", null_mut(), stream_cb::open_cb)
            .map_err(|e| format!("注册 zvf:// 协议失败: {e}"))?;

        // render API 模式（ARCHITECTURE.md 13）：vo=libmpv 必须在 create_render_context
        // 之前设置；timing-offset=0 配合 render_gl_render 的 BLOCK_FOR_TARGET_TIME=0
        // （render.h 建议）：render 永不阻塞，帧节奏由 vsync 连续重绘保证
        s.opt("vo", "libmpv");
        s.opt("video-timing-offset", "0");

        // get_proc_address 走系统 API（见 gpa_trampoline 注释），user_data 不需要
        let rctx = match mpv_ffi::render_context_create(ctx, gpa_trampoline, null_mut()) {
            Ok(r) => r,
            Err(e) => {
                mpv_ffi::destroy(ctx);
                return Err(e);
            }
        };
        let update_box = Box::into_raw(Box::new(egui_ctx.clone()));
        mpv_ffi::render_context_set_update_callback(
            rctx,
            Some(update_trampoline),
            update_box as *mut c_void,
        );
        let (fbo, texture) = make_fbo(gl, 16, 16);
        s.render = Some(RenderState {
            rctx,
            fbo,
            texture,
            w: 16,
            h: 16,
            update_user_data: update_box,
        });
        Ok(s)
    }

    fn opt(&self, name: &str, value: &str) {
        if let Err(e) = mpv_ffi::set_option_string(self.ctx, name, value) {
            tracing::warn!("mpv 选项 {name}={value} 设置失败: {e}");
        }
    }

    fn set_prop_str(&self, name: &str, value: &str) {
        if let Err(e) = mpv_ffi::set_property_string(self.ctx, name, value) {
            tracing::warn!("mpv 属性 {name}={value} 设置失败: {e}");
        }
    }

    /// 运行时（播放器在场）的属性写入走异步版本：同步 set_property 与
    /// get_property 同拿 core 锁，打开窗口（demux 探测等占锁，~0.5s）内
    /// UI 线程调用会阻塞等锁、整帧冻结；异步入队即返回，core 空出手再
    /// 应用（同 client 内写入顺序保持，volume 与 mute 交错不乱序）。
    /// 写失败只经回复事件回报（泵不消费：真相在 cfg，创建时对齐）。
    /// 创建期初始值仍走同步版——那时 core 空闲无竞争，还能即时报错
    fn set_prop_str_async(&self, name: &str, value: &str) {
        if let Err(e) = mpv_ffi::set_property_string_async(self.ctx, name, value) {
            tracing::warn!("mpv 属性 {name}={value} 异步入队失败: {e}");
        }
    }

    /// PaintCallback 前调用（UI 线程）：确保离屏 FBO 与目标物理像素同尺寸（变了就重建），
    /// 返回闭包要捕获的全部值。w/h 为 0 时返回 None（无有效绘制目标）
    pub fn render_info(&mut self, gl: &glow::Context, w: i32, h: i32) -> Option<RenderInfo> {
        let r = self.render.as_mut()?;
        if w <= 0 || h <= 0 {
            return None;
        }
        if r.w != w || r.h != h {
            unsafe {
                gl.delete_framebuffer(r.fbo);
                gl.delete_texture(r.texture);
            }
            let (fbo, texture) = make_fbo(gl, w, h);
            r.fbo = fbo;
            r.texture = texture;
            r.w = w;
            r.h = h;
        }
        Some(RenderInfo {
            rctx: MpvRenderCtx(r.rctx),
            fbo: r.fbo.0.get(),
            w: r.w,
            h: r.h,
            first_frame: self.first_frame.clone(),
        })
    }

    /// eframe on_exit 调用（GL current、早于 App drop 与 mpv terminate）。
    /// 顺序：摘更新回调 → free render ctx → 回收 user_data 薄指针 Box → 删 GL 资源。
    /// 之后 PlayerService Drop 只剩 mpv terminate_destroy。
    pub fn shutdown_render(&mut self, gl: &glow::Context) {
        if let Some(r) = self.render.take() {
            mpv_ffi::render_context_set_update_callback(r.rctx, None, null_mut());
            mpv_ffi::render_context_free(r.rctx);
            unsafe {
                drop(Box::from_raw(r.update_user_data));
                gl.delete_framebuffer(r.fbo);
                gl.delete_texture(r.texture);
            }
        }
    }

    /// hwdec 探测记忆的 per-file 选项片段：配置为 auto 且已探得方法时返回
    /// `hwdec=<method>`（拼进 loadfile 第四参数 options），否则空串。
    /// per-file 是 mpv 官方的按文件选项下发方式：不影响全局属性、不触发
    /// 旧视频解码器重建（运行时 set 属性会，实测白耗 60ms）
    fn hwdec_file_opt(&self) -> String {
        if self.hwdec_cfg == "auto" {
            if let Some(h) = self.hwdec_fixed.borrow().as_ref() {
                return format!("hwdec={h}");
            }
        }
        String::new()
    }

    pub fn load(&self, archive_id: u64, entry_idx: usize) {
        // 重置打开完成标志（必须先于 loadfile 提交：快文件 PLAYBACK_RESTART
        // 可在数十 ms 内到达泵线程，若先提交后重置，会被泵线程抢先置位再
        // 被抹掉，门控将卡到 2s 兜底）
        self.playback_ready.store(false, Ordering::Relaxed);
        let hwdec_opt = self.hwdec_file_opt();
        let uri = stream_cb::entry_uri(archive_id, entry_idx);
        let cmd = if hwdec_opt.is_empty() {
            format!("loadfile {uri} replace")
        } else {
            // 第四参数 options：index 占位 -1
            format!("loadfile {uri} replace -1 {hwdec_opt}")
        };
        if let Err(e) = mpv_ffi::command_string(self.ctx, &cmd) {
            tracing::error!("loadfile 失败: {e}");
        }
        // keep-open=always 在 EOF 时会把 pause 置为 yes，且 loadfile 不清除它——
        // 不显式取消暂停的话，播完一个视频后切换新视频会停在第一帧
        self.set_prop_str("pause", "no");
        // 装载窗口起点：供 has_painted_since_load 打首帧耗时 + UI 轮询判定
        self.load_mark.set(Some(Instant::now()));
        self.painted_since_load.set(false);
        self.first_frame.store(false, Ordering::Relaxed);
    }

    /// 播放本地文件路径（7z/RAR 后台解压完成后的慢路径）
    pub fn load_path(&self, path: &std::path::Path) {
        // 同 load()：重置时机必须先于 loadfile 提交
        self.playback_ready.store(false, Ordering::Relaxed);
        let hwdec_opt = self.hwdec_file_opt();
        let url = to_file_url(path);
        let cmd = if hwdec_opt.is_empty() {
            format!("loadfile {url} replace")
        } else {
            format!("loadfile {url} replace -1 {hwdec_opt}")
        };
        if let Err(e) = mpv_ffi::command_string(self.ctx, &cmd) {
            tracing::error!("loadfile {url} 失败: {e}");
        }
        self.set_prop_str("pause", "no"); // 同 load()：清除 EOF 粘滞的暂停态
        self.load_mark.set(Some(Instant::now()));
        self.painted_since_load.set(false);
        self.first_frame.store(false, Ordering::Relaxed);
    }

    /// loadfile 后是否已画出首帧。false = 处于装载窗口，UI 应高频轮询
    /// repaint（帧就绪后 mpv 不触发 update 回调，靠 200ms 内部超时推进，
    /// 主动轮询把这段空洞压掉）。
    /// 首帧以 PaintCallback 闭包内 render_context_update 检出的
    /// MPV_RENDER_UPDATE_FRAME 为准（真实换帧而非提交绘制）；另有 2s
    /// 兜底超时——纯音频等永远无视频帧的场景停止高频轮询
    pub fn has_painted_since_load(&self) -> bool {
        if !self.painted_since_load.get() && self.first_frame.load(Ordering::Relaxed) {
            self.painted_since_load.set(true);
            if let Some(t0) = self.load_mark.get() {
                tracing::debug!(
                    "mpv 首帧真实渲染 距loadfile {:.3}s",
                    t0.elapsed().as_secs_f64()
                );
            }
            // 首视频探得 hwdec 方法后记忆（进程内仅一次）：后续装载跳过 auto
            // 试探链。hwdec-current 为空/"no"（纯音频、软解回退）不记忆
            if self.hwdec_fixed.borrow().is_none() && self.hwdec_cfg == "auto" {
                let cur = mpv_ffi::get_property_string(self.ctx, "hwdec-current")
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                let cur = cur.trim().to_owned();
                if !cur.is_empty() && cur != "no" {
                    tracing::debug!("hwdec 探测记忆: {cur}（后续视频跳过试探）");
                    *self.hwdec_fixed.borrow_mut() = Some(cur);
                }
            }
        }
        if self.painted_since_load.get() {
            return true;
        }
        match self.load_mark.get() {
            Some(t0) if t0.elapsed() > std::time::Duration::from_secs(2) => {
                self.painted_since_load.set(true);
                // ready 同步兜底置位：坏文件永远等不来 PLAYBACK_RESTART，
                // 不置位的话 opening 门控会一直拦住属性轮询
                self.playback_ready.store(true, Ordering::Relaxed);
                tracing::warn!("2s 未见视频帧（纯音频或解码失败？），停止高频轮询");
                true
            }
            _ => false,
        }
    }

    /// 停止播放并清空画面（视频 → 静态图纹理直显切换时调用）：
    /// mpv stop 命令清空播放列表进入 idle——音频立即停止，末帧不再占窗口
    pub fn stop(&self) {
        if let Err(e) = mpv_ffi::command_string(self.ctx, "stop") {
            tracing::error!("stop 失败: {e}");
        }
        // 复位装载窗口：stop 后无画面可画，UI 不再轮询（图片直显模式）
        self.load_mark.set(None);
        self.painted_since_load.set(true);
        self.first_frame.store(true, Ordering::Relaxed);
    }

    pub fn toggle_pause(&self) {
        // 打开窗口期短路：core 锁长占，提交命令会阻塞 UI；且新文件即将
        // 起播，此刻切换暂停无意义
        if self.opening() {
            return;
        }
        // cycle 由 mpv 内部翻转布尔，无需先读当前值：读（get_property）
        // 在 core 忙时会阻塞 UI 线程，读-改-写还有竞态。
        // 注：若改回读属性，pause 必须用 MPV_FORMAT_FLAG——INT64 读会
        // 失败（mpv 不做跨格式转换）
        if let Err(e) = mpv_ffi::command_string(self.ctx, "no-osd cycle pause") {
            tracing::error!("切换暂停失败: {e}");
        }
    }

    /// 显式设置暂停（toggle_pause 的绝对值版：EOF 后复活播放用）
    pub fn set_paused(&self, on: bool) {
        self.set_prop_str("pause", if on { "yes" } else { "no" });
    }

    /// 单视频循环开关（loop-file=inf）：mpv 原生循环，EOF 不触发（播完无缝
    /// 回到开头继续），eof-reached 也永远为 false。运行时设置即生效，
    /// 但 EOF 已经发生时不会回溯重启——复活由调用方 seek(0)+解除暂停处理
    pub fn set_loop(&self, on: bool) {
        self.set_prop_str("loop-file", if on { "inf" } else { "no" });
    }

    pub fn seek(&self, sec: f64) {
        // no-osd 前缀：不在预览窗口里闪现 mpv 的 OSD 进度条。
        // 条件探针（提交 ≥5ms 才打点，debug 级）：同步命令需等 core 完成
        // demuxer seek（cache 未命中时 zip VFS = 重开 entry 从头解压丢弃）
        let t0 = std::time::Instant::now();
        if let Err(e) = mpv_ffi::command_string(self.ctx, &format!("no-osd seek {sec} absolute")) {
            tracing::error!("seek 失败: {e}");
        }
        let d = t0.elapsed().as_secs_f64() * 1000.0;
        if d >= 5.0 {
            tracing::debug!("[seek探针] 命令提交 → {sec:.1}s 耗时 {d:.1}ms");
        }
    }

    /// 相对 seek（秒，可为负），mpv 默认关键帧级（快）
    pub fn seek_relative(&self, sec: f64) {
        if let Err(e) = mpv_ffi::command_string(self.ctx, &format!("no-osd seek {sec:+} relative"))
        {
            tracing::error!("seek 相对跳转失败: {e}");
        }
    }

    /// 逐帧步进：dir>0 = frame-step（下一帧），dir<0 = frame-back-step（上一帧）。
    /// mpv 语义：播放中调 frame-step 会自动暂停；UI 侧只在暂停时映射方向键，
    /// 播放中误触发也安全（等效暂停 + 步进）。opening 短路同 toggle_pause
    pub fn frame_step(&self, dir: i32) {
        if self.opening() {
            return;
        }
        let cmd = if dir >= 0 {
            "no-osd frame-step"
        } else {
            "no-osd frame-back-step"
        };
        if let Err(e) = mpv_ffi::command_string(self.ctx, cmd) {
            tracing::error!("逐帧步进失败: {e}");
        }
    }

    /// 静音状态设置（真相在 cfg.player.muted，UI 翻转后调用此处下发 mpv；
    /// 未播放时只改 cfg，播放器创建时会统一应用）
    pub fn set_muted(&self, on: bool) {
        self.set_prop_str_async("mute", if on { "yes" } else { "no" });
    }

    pub fn set_volume(&self, v: i64) {
        self.set_prop_str_async("volume", &v.to_string());
        // 主动调整音量即解除静音（含拖动音量条和方向键调音量）
        self.set_prop_str_async("mute", "no");
    }

    /// 打开窗口期（loadfile → 首帧 && PLAYBACK_RESTART，2s 兜底自限）。
    /// 此时 mpv core 被 demux 探测、hwdec 设备试探、音频初始化、cache
    /// 填充长时间占锁，任何 get_property 都会在 core 锁上阻塞 UI 线程。
    /// 窗口不能止于首帧——首帧（实测 7~20ms）远早于打开完成（音频/cache
    /// 余段还要 ~0.5s），首帧后各帧属性轮询逐次等锁，帧探针实测
    /// repaint决策/eof_tick/controls 段连续长帧合计 ~0.5s（切换卡顿主体），
    /// 故窗口延至 playback_ready（事件泵收 PLAYBACK_RESTART 置位 = 打开
    /// 完成、core 空闲）。窗口判定只依赖原子标志，不进 mpv，界面维持
    /// vsync 节拍
    fn opening(&self) -> bool {
        self.load_mark.get().is_some()
            && !(self.painted_since_load.get() && self.playback_ready.load(Ordering::Relaxed))
    }

    /// 当前播放位置：observe_property 本地缓存，纯原子读。seek 卡顿实证：
    /// seek 后 core 完成 seek 前，任何 get_property 都在 core 锁上阻塞
    /// （实测首读 ~194ms = seek 全程），帧路径不得再进 mpv
    pub fn time_pos(&self) -> Option<f64> {
        if self.opening() {
            return None;
        }
        Some(f64::from_bits(self.time_pos_cache.load(Ordering::Relaxed)))
    }

    /// 总时长：observe_property 本地缓存（每文件装载推 1~2 条）。0.0 = 不可用
    pub fn duration(&self) -> Option<f64> {
        if self.opening() {
            return None;
        }
        let d = f64::from_bits(self.duration_cache.load(Ordering::Relaxed));
        (d > 0.0).then_some(d)
    }

    /// 当前是否暂停：observe_property 本地缓存，纯原子读。不进 mpv，故无需
    /// opening 门控——toggle 的 cycle、EOF 时 keep-open 置 pause=yes 等一切
    /// 变更都经泵线程推送同步
    pub fn paused(&self) -> bool {
        self.paused_cache.load(Ordering::Relaxed)
    }

    /// 是否缓冲暂停（pause-for-cache）：observe_property 本地缓存，纯原子读
    pub fn buffering(&self) -> bool {
        if self.opening() {
            return true;
        }
        self.buffering_cache.load(Ordering::Relaxed)
    }

    /// 是否已播到末尾（keep-open=always 下停末帧且 pause 被置 yes）。
    /// observe_property 本地缓存，纯原子读——seek 卡顿实测：seek 后首帧的
    /// eof-reached get_property 在 core 锁上等了 seek 全程（两次操作各
    /// ~194ms），seek 后无 opening 门控保护正是中招原因。打开窗口期短路
    /// false：新文件装载时 eof 已复位，窗口期查询无意义
    pub fn eof_reached(&self) -> bool {
        if self.opening() {
            return false;
        }
        self.eof_cache.load(Ordering::Relaxed)
    }
}

impl Drop for PlayerService {
    fn drop(&mut self) {
        mpv_ffi::destroy(self.ctx);
    }
}

/// Windows 路径 → file:// URL（百分号编码，避免 mpv 命令解析吃掉空格/反斜杠）
fn to_file_url(p: &std::path::Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file:///");
    for c in s.chars() {
        match c {
            ' ' => out.push_str("%20"),
            '%' => out.push_str("%25"),
            '#' => out.push_str("%23"),
            '?' => out.push_str("%3F"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    //! 自动化端到端播放测试：无需 UI/人工，模拟真实用户播放。
    //! headless mpv（vo=null ao=null）+ cache=no（强制每次 seek 打到 VFS 层），
    //! 覆盖：mpv FFI 加载、zvf:// stream_cb open/read/seek/close、deflate seek 适配器。
    //! 前置：libmpv-2.dll（build.rs 已复制到 target）+ ffmpeg 生成测试视频，缺任一则跳过。

    use super::*;
    use crate::vfs::zip_vfs::ZipVFS;
    use crate::vfs::ArchiveVFS;
    use std::path::PathBuf;

    /// 查找 libmpv-2.dll 所在目录：test exe 在 target\{debug,release}\deps\ 下，
    /// dll 在上一级；也兼容直接 target\{debug,release}\ 与 CARGO_MANIFEST_DIR。
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

    /// ffmpeg 是否可用
    fn have_ffmpeg() -> bool {
        std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// 生成 3 秒测试视频并打进 deflate zip（两个 entry），返回 zip 路径
    fn make_test_zip() -> Option<PathBuf> {
        let dir = std::env::temp_dir().join(format!("zvf_e2e_{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        let mp4 = dir.join("clip.mp4");
        // 3 秒 320x180 testsrc2，超快速编码
        let st = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=duration=3:size=320x180:rate=15",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&mp4)
            .output()
            .ok()?;
        if !st.status.success() {
            eprintln!("ffmpeg 生成失败: {}", String::from_utf8_lossy(&st.stderr));
            return None;
        }
        let data = std::fs::read(&mp4).ok()?;
        let zip_path = dir.join("test.zip");
        let f = std::fs::File::create(&zip_path).ok()?;
        let mut w = zip::ZipWriter::new(f);
        let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        w.start_file("clip.mp4", opts).ok()?;
        std::io::Write::write_all(&mut w, &data).ok()?;
        // 第二个 entry：用于"播完后切换视频"场景
        w.start_file("clip2.mp4", opts).ok()?;
        std::io::Write::write_all(&mut w, &data).ok()?;
        w.finish().ok()?;
        Some(zip_path)
    }

    /// 轮询直到属性满足条件，超时返回 None
    fn wait_prop(
        ctx: *mut mpv_ffi::MpvHandle,
        name: &str,
        pred: impl Fn(f64) -> bool,
        secs: u64,
    ) -> Option<f64> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        loop {
            if let Ok(Some(v)) = mpv_ffi::get_property_f64(ctx, name) {
                if pred(v) {
                    return Some(v);
                }
            }
            if std::time::Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    /// 端到端：zip 内 deflate 视频 → zvf:// 流 → headless mpv 播放 + 下游/上游 seek
    #[test]
    fn e2e_playback_with_seek_via_stream_cb() {
        let Some(dll_dir) = find_mpv_dll_dir() else {
            eprintln!("跳过：未找到 libmpv-2.dll");
            return;
        };
        if !have_ffmpeg() {
            eprintln!("跳过：未找到 ffmpeg");
            return;
        }
        let Some(zip_path) = make_test_zip() else {
            eprintln!("跳过：测试 zip 生成失败");
            return;
        };

        mpv_ffi::init(&dll_dir).expect("加载 libmpv 失败");

        // 注册 VFS
        let vfs = ZipVFS::open(&zip_path, None).unwrap();
        let arc: std::sync::Arc<dyn crate::vfs::ArchiveVFS> = std::sync::Arc::new(vfs);
        let (archive_id, entries) = stream_cb::register_with_progress(arc, &zip_path, &|_, _| {});
        let entry_idx = entries
            .iter()
            .position(|e| e.name == "clip.mp4")
            .expect("zip 内应有 clip.mp4");

        // headless mpv：无窗口渲染；cache=no 强制 seek 直接打到 VFS；
        // keep-open=always 与 App 的 PlayerService 一致（复现播完暂停场景）
        let ctx = mpv_ffi::create().unwrap();
        mpv_ffi::set_option_string(ctx, "terminal", "yes").unwrap();
        mpv_ffi::set_option_string(ctx, "msg-level", "all=error").unwrap();
        mpv_ffi::set_option_string(ctx, "vo", "null").unwrap();
        mpv_ffi::set_option_string(ctx, "ao", "null").unwrap();
        mpv_ffi::set_option_string(ctx, "cache", "no").unwrap();
        mpv_ffi::set_option_string(ctx, "keep-open", "always").unwrap();
        mpv_ffi::stream_cb_add_ro(ctx, "zvf", std::ptr::null_mut(), stream_cb::open_cb)
            .expect("注册 zvf:// 失败");

        // 播放
        let uri = stream_cb::entry_uri(archive_id, entry_idx);
        mpv_ffi::command_string(ctx, &format!("loadfile {uri}")).expect("loadfile 失败");

        // 1) 起播：time-pos > 0.5s（vo=null 全速解码，瞬间到达）
        let started = wait_prop(ctx, "time-pos", |t| t > 0.5, 30);
        assert!(
            started.is_some(),
            "30 秒内未起播（stream_cb 链路可能崩溃/挂起）"
        );

        // 1.1) pause 属性必须用 flag 格式读：toggle_pause/paused 依赖它。
        // 此前用 INT64 读恒失败 → toggle_pause 永远认为"未暂停" → 空格/暂停按钮无法恢复播放
        assert_eq!(
            mpv_ffi::get_property_flag(ctx, "pause").ok().flatten(),
            Some(false),
            "flag 方式应能读到 pause=no"
        );
        mpv_ffi::set_property_string(ctx, "pause", "yes").unwrap();
        assert_eq!(
            mpv_ffi::get_property_flag(ctx, "pause").ok().flatten(),
            Some(true),
            "置 yes 后 flag 方式应能读到 pause=yes"
        );
        // toggle 循环（暂停按钮/空格的完整行为路径）：yes → 翻转回 no
        mpv_ffi::set_property_string(ctx, "pause", "no").unwrap();
        assert_eq!(
            mpv_ffi::get_property_flag(ctx, "pause").ok().flatten(),
            Some(false),
            "toggle 回 no 后应能读到 pause=no"
        );
        // mute 同为 flag 属性（静音按钮路径）
        mpv_ffi::set_property_string(ctx, "mute", "yes").unwrap();
        assert_eq!(
            mpv_ffi::get_property_flag(ctx, "mute").ok().flatten(),
            Some(true),
            "mute 置 yes 后 flag 方式应能读到"
        );
        mpv_ffi::set_property_string(ctx, "mute", "no").unwrap();

        // 2) 时长合理（3s ± 1）
        let dur = mpv_ffi::get_property_f64(ctx, "duration")
            .ok()
            .flatten()
            .expect("读不到 duration");
        assert!((dur - 3.0).abs() < 1.0, "duration 异常: {dur}");

        // 暂停：time-pos 冻结，seek 验证不再受全速播放影响（否则轮询可能错过断言窗口）
        mpv_ffi::set_property_string(ctx, "pause", "yes").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));

        // 3) 下游 seek 到中点（播放时间向文件尾）
        let target = dur * 0.5;
        mpv_ffi::command_string(ctx, &format!("seek {target} absolute")).unwrap();
        let reached = wait_prop(ctx, "time-pos", |t| (t - target).abs() < 1.0, 30);
        assert!(reached.is_some(), "下游 seek 超时");

        // 4) 上游 seek 回 15% 处（deflate 重解压路径）
        let back = dur * 0.15;
        mpv_ffi::command_string(ctx, &format!("seek {back} absolute")).unwrap();
        let reached_back = wait_prop(ctx, "time-pos", |t| (t - back).abs() < 1.0, 30);
        assert!(reached_back.is_some(), "上游 seek 超时");

        // 5) 真实场景复现：播完第一个视频（EOF）后切换第二个视频
        // 取消暂停让视频自然播到结尾（vo=null 全速），等待真正的 eof-reached=yes
        // （keep-open=always 下 mpv 会在结尾暂停——正是用户遇到的场景）
        mpv_ffi::set_property_string(ctx, "pause", "no").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let eof = mpv_ffi::get_property_flag(ctx, "eof-reached")
                .ok()
                .flatten();
            if eof == Some(true) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "未到达 EOF（eof-reached={eof:?}, time-pos={:?}, pause={:?}）",
                mpv_ffi::get_property_f64(ctx, "time-pos").ok().flatten(),
                mpv_ffi::get_property_flag(ctx, "pause").ok().flatten(),
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let pause_at_eof = mpv_ffi::get_property_flag(ctx, "pause").ok().flatten();
        eprintln!("EOF 到达，此时 pause = {pause_at_eof:?}");

        // 切换第二个视频（同一压缩包不同 entry，与 UI 点击行为一致）
        let entry_idx2 = entries
            .iter()
            .position(|e| e.name == "clip2.mp4")
            .expect("zip 内应有 clip2.mp4");
        let uri2 = stream_cb::entry_uri(archive_id, entry_idx2);
        mpv_ffi::command_string(ctx, &format!("loadfile {uri2} replace")).expect("loadfile 失败");
        // 修复验证：loadfile 后显式取消暂停（PlayerService::load 的行为）
        mpv_ffi::set_property_string(ctx, "pause", "no").unwrap();
        let started2 = wait_prop(ctx, "time-pos", |t| t > 0.5, 15);
        let pause_after_load = mpv_ffi::get_property_flag(ctx, "pause").ok().flatten();
        eprintln!("切换后 time-pos = {started2:?}, pause = {pause_after_load:?}");
        assert!(
            started2.is_some(),
            "播完 EOF 后切换第二个视频未起播（pause={pause_after_load:?}：keep-open EOF 暂停跨 loadfile 粘滞，需显式 pause=no）"
        );

        mpv_ffi::destroy(ctx);
        let _ = std::fs::remove_dir_all(zip_path.parent().unwrap());
        eprintln!("端到端播放+seek 测试通过 ✓");
    }

    /// 端到端（慢路径）：7z 包内视频 → SevenzVFS 后台解压 → headless mpv 播本地文件 + seek。
    /// 7z 测试包用 sevenz-rust writer 纯 Rust 自造，无需外部 7z.exe。
    #[test]
    fn e2e_playback_7z_extract_then_play() {
        let Some(dll_dir) = find_mpv_dll_dir() else {
            eprintln!("跳过：未找到 libmpv-2.dll");
            return;
        };
        if !have_ffmpeg() {
            eprintln!("跳过：未找到 ffmpeg");
            return;
        }

        // 生成 3 秒测试视频
        let dir = std::env::temp_dir().join(format!("zvf_7z_e2e_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mp4 = dir.join("clip.mp4");
        let st = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=duration=3:size=320x180:rate=15",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&mp4)
            .output()
            .unwrap();
        assert!(st.status.success(), "ffmpeg 生成失败");

        // 纯 Rust 打 7z 包（LZMA2）
        let arch_path = dir.join("test.7z");
        let mut w = sevenz_rust::SevenZWriter::create(&arch_path).unwrap();
        let entry = sevenz_rust::SevenZArchiveEntry::from_path(&mp4, "movies/clip.mp4".into());
        w.push_archive_entry(entry, Some(std::fs::File::open(&mp4).unwrap()))
            .unwrap();
        w.finish().unwrap();

        // VFS 解压（慢路径）
        let vfs = crate::vfs::sevenz_vfs::SevenzVFS::open(&arch_path, None).unwrap();
        let entries = vfs.list();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "movies/clip.mp4");
        let extracted = dir.join("extracted.mp4");
        vfs.extract_entry(
            &entries[0],
            &extracted,
            &std::sync::atomic::AtomicBool::new(false),
            &|_, _| {},
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(&extracted).unwrap().len(),
            entries[0].size
        );

        // headless mpv 播放解压后的本地文件
        mpv_ffi::init(&dll_dir).expect("加载 libmpv 失败");
        let ctx = mpv_ffi::create().unwrap();
        mpv_ffi::set_option_string(ctx, "vo", "null").unwrap();
        mpv_ffi::set_option_string(ctx, "ao", "null").unwrap();
        let url = to_file_url(&extracted);
        mpv_ffi::command_string(ctx, &format!("loadfile {url}")).unwrap();

        let started = wait_prop(ctx, "time-pos", |t| t > 0.5, 30);
        assert!(started.is_some(), "7z 解压路径未起播");
        let dur = mpv_ffi::get_property_f64(ctx, "duration")
            .ok()
            .flatten()
            .unwrap();
        assert!((dur - 3.0).abs() < 1.0, "duration 异常: {dur}");

        mpv_ffi::set_property_string(ctx, "pause", "yes").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let target = dur * 0.5;
        mpv_ffi::command_string(ctx, &format!("seek {target} absolute")).unwrap();
        let reached = wait_prop(ctx, "time-pos", |t| (t - target).abs() < 1.0, 30);
        assert!(reached.is_some(), "7z 路径 seek 超时");

        mpv_ffi::destroy(ctx);
        let _ = std::fs::remove_dir_all(&dir);
        eprintln!("7z 解压播放端到端测试通过 ✓");
    }

    /// FFI 层 observe_property 实证：订阅即推送初始值（idle 静默玩家收到的
    /// 事件即 PROPERTY_CHANGE），cycle mute 后推送变更值。同时钉死
    /// MPV_EVENT_PROPERTY_CHANGE 精确值与 MpvEventProperty 解析——
    /// muted_cache 帧路径正确性依赖这两者
    #[test]
    fn ffi_observe_property() {
        let Some(dll_dir) = find_mpv_dll_dir() else {
            eprintln!("跳过：未找到 libmpv-2.dll");
            return;
        };
        mpv_ffi::init(&dll_dir).expect("加载 libmpv 失败");
        let ctx = mpv_ffi::create().unwrap();
        mpv_ffi::set_option_string(ctx, "vo", "null").unwrap();
        mpv_ffi::set_option_string(ctx, "ao", "null").unwrap();

        // 收一个 PROPERTY_CHANGE(prop) 且 FLAG 值等于 want（5s 超时）
        let recv_flag = |prop: &[u8], want: bool| -> bool {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let ev = unsafe { mpv_ffi::wait_event(ctx, 0.05) };
                if !ev.is_null() {
                    let ev = unsafe { &*ev };
                    if ev.event_id == mpv_ffi::MPV_EVENT_PROPERTY_CHANGE && !ev.data.is_null() {
                        let p = unsafe { &*(ev.data as *mut mpv_ffi::MpvEventProperty) };
                        if !p.name.is_null()
                            && p.format == mpv_ffi::MPV_FORMAT_FLAG
                            && !p.data.is_null()
                        {
                            let name = unsafe { CStr::from_ptr(p.name) }.to_bytes();
                            let v = unsafe { *(p.data as *const std::ffi::c_int) } != 0;
                            if name == prop && v == want {
                                return true;
                            }
                        }
                    }
                }
                if std::time::Instant::now() > deadline {
                    return false;
                }
            }
        };

        // 1) 订阅后 mpv 立即推送当前值（idle 未静音 → false）。
        //    若常量值错（非 22），这里收不到事件直接失败
        mpv_ffi::observe_property_flag(ctx, "mute").expect("订阅 mute");
        assert!(
            recv_flag(b"mute", false),
            "订阅后应立即收到初始值 mute=false 推送"
        );

        // 2) cycle 翻转 → 推送 true；再翻回 → false（FLAG 推送变更同步
        //    机制实证，生产泵线程按值分发到缓存依赖此点）
        mpv_ffi::command_string(ctx, "no-osd cycle mute").unwrap();
        assert!(recv_flag(b"mute", true), "cycle 后应收到 mute=true 推送");
        mpv_ffi::command_string(ctx, "no-osd cycle mute").unwrap();
        assert!(
            recv_flag(b"mute", false),
            "再次 cycle 后应收到 mute=false 推送"
        );

        // 3) 同一 ctx 二路订阅互不干扰：pause 的初始推送与 cycle 翻转都
        //    准确送达（生产泵线程按 name 分发到各自缓存依赖此点）
        mpv_ffi::observe_property_flag(ctx, "pause").expect("订阅 pause");
        assert!(
            recv_flag(b"pause", false),
            "订阅 pause 后应立即收到初始值 false 推送"
        );
        mpv_ffi::command_string(ctx, "no-osd cycle pause").unwrap();
        assert!(recv_flag(b"pause", true), "cycle 后应收到 pause=true 推送");

        // 4) DOUBLE 订阅实证：播放中 time-pos 以 DOUBLE 推送且推进、duration
        //    装载即推——钉死 MPV_FORMAT_DOUBLE=5 与 f64 解码（time_pos_cache/
        //    duration_cache 帧路径正确性依赖此点）。无 ffmpeg 时跳过
        if have_ffmpeg() {
            let mp4 = std::env::temp_dir().join(format!("zvf_dbl_{}.mp4", std::process::id()));
            let st = std::process::Command::new("ffmpeg")
                .args([
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=duration=3:size=320x180:rate=15",
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-pix_fmt",
                    "yuv420p",
                ])
                .arg(&mp4)
                .output()
                .unwrap();
            assert!(st.status.success(), "ffmpeg 生成失败");
            // 3) 末尾把 pause 翻成了 true，先解除再起播（否则 time-pos 不推进）
            mpv_ffi::command_string(ctx, "no-osd cycle pause").unwrap();
            assert!(
                recv_flag(b"pause", false),
                "解除暂停应收到 pause=false 推送"
            );

            // 收一个 PROPERTY_CHANGE(prop) 且 DOUBLE 值满足 pred（10s 超时）
            let recv_double = |prop: &[u8], pred: &dyn Fn(f64) -> bool| -> Option<f64> {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                loop {
                    let ev = unsafe { mpv_ffi::wait_event(ctx, 0.05) };
                    if !ev.is_null() {
                        let ev = unsafe { &*ev };
                        if ev.event_id == mpv_ffi::MPV_EVENT_PROPERTY_CHANGE && !ev.data.is_null() {
                            let p = unsafe { &*(ev.data as *mut mpv_ffi::MpvEventProperty) };
                            if !p.name.is_null()
                                && p.format == mpv_ffi::MPV_FORMAT_DOUBLE
                                && !p.data.is_null()
                            {
                                let name = unsafe { CStr::from_ptr(p.name) }.to_bytes();
                                if name == prop {
                                    let v = unsafe { *(p.data as *const f64) };
                                    if pred(v) {
                                        return Some(v);
                                    }
                                }
                            }
                        }
                    }
                    if std::time::Instant::now() > deadline {
                        return None;
                    }
                }
            };
            // 起播前 time-pos 不可用：mpv 推 format=NONE 且 data=null 的变更，
            // 生产泵按 data 判空跳过（缓存不被污染），此处只等 DOUBLE 值
            mpv_ffi::observe_property_double(ctx, "time-pos").expect("订阅 time-pos");
            mpv_ffi::command_string(ctx, &format!("loadfile {}", to_file_url(&mp4))).unwrap();
            assert!(
                recv_double(b"time-pos", &|v| v > 0.5).is_some(),
                "播放中应收到 time-pos DOUBLE 推送且值推进"
            );
            mpv_ffi::observe_property_double(ctx, "duration").expect("订阅 duration");
            assert!(
                recv_double(b"duration", &|v| v > 2.0 && v < 4.0).is_some(),
                "装载后应收到 duration DOUBLE 推送（3s 视频）"
            );
            let _ = std::fs::remove_file(&mp4);
        } else {
            eprintln!("跳过 DOUBLE 订阅段：无 ffmpeg");
        }

        mpv_ffi::destroy(ctx);
    }

    /// FFI 层 mpv_set_property_async 实证：入队即返回（不等 core，打开忙
    /// 窗口内 UI 线程也不阻塞），写入由 core 异步完成——经 observe 推送
    /// 验证 mute 真被置上；同时钉死 STRING data 拷贝语义（临时 CString
    /// 释放后仍正确写入，client.h 承诺 data 会被拷贝）。运行时 mute/volume
    /// 写入走异步（set_prop_str_async）的底层依据
    #[test]
    fn ffi_set_property_async() {
        let Some(dll_dir) = find_mpv_dll_dir() else {
            eprintln!("跳过：未找到 libmpv-2.dll");
            return;
        };
        mpv_ffi::init(&dll_dir).expect("加载 libmpv 失败");
        let ctx = mpv_ffi::create().unwrap();
        mpv_ffi::set_option_string(ctx, "vo", "null").unwrap();
        mpv_ffi::set_option_string(ctx, "ao", "null").unwrap();

        // 收一个 PROPERTY_CHANGE(mute) 且 FLAG 值等于 want（5s 超时）。
        // 队列里混有 SET_PROPERTY_REPLY 回复事件，非目标事件自然跳过
        let recv_mute = |want: bool| -> bool {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let ev = unsafe { mpv_ffi::wait_event(ctx, 0.05) };
                if !ev.is_null() {
                    let ev = unsafe { &*ev };
                    if ev.event_id == mpv_ffi::MPV_EVENT_PROPERTY_CHANGE && !ev.data.is_null() {
                        let p = unsafe { &*(ev.data as *mut mpv_ffi::MpvEventProperty) };
                        if !p.name.is_null()
                            && p.format == mpv_ffi::MPV_FORMAT_FLAG
                            && !p.data.is_null()
                        {
                            let name = unsafe { CStr::from_ptr(p.name) }.to_bytes();
                            let v = unsafe { *(p.data as *const std::ffi::c_int) } != 0;
                            if name == b"mute" && v == want {
                                return true;
                            }
                        }
                    }
                }
                if std::time::Instant::now() > deadline {
                    return false;
                }
            }
        };

        mpv_ffi::observe_property_flag(ctx, "mute").expect("订阅 mute");
        assert!(recv_mute(false), "订阅后应立即收到初始值 mute=false");

        // 异步置 yes：入队调用立刻返回，随后 observe 推送 true 证明 core
        // 已完成写入（写入成功与否也可看 SET_PROPERTY_REPLY 的 error 字段）
        mpv_ffi::set_property_string_async(ctx, "mute", "yes").unwrap();
        assert!(recv_mute(true), "异步 set mute=yes 后应收到 true 推送");
        mpv_ffi::set_property_string_async(ctx, "mute", "no").unwrap();
        assert!(recv_mute(false), "异步 set mute=no 后应收到 false 推送");

        mpv_ffi::destroy(ctx);
    }

    /// 诊断：webp 起播 ~5 秒延迟的卡点定位。同一 mpv 实例三场景对比：
    /// A zvf:// 冷启（生产链路）→ B file:/// 直读（绕过 zvf，隔离 mpv/lavf
    /// 对 webp 本身的处理）→ C zvf:// 切换（复现"上一个 webp 切下一个"）。
    /// 逐条打印 mpv 内部日志（带时间戳，level=v），到 PLAYBACK_RESTART
    /// （首帧呈现）为止。前置：libmpv-2.dll + test_media\641.webp、1000.webp。
    #[test]
    fn diag_webp_load_latency() {
        let Some(dll_dir) = find_mpv_dll_dir() else {
            eprintln!("跳过：未找到 libmpv-2.dll");
            return;
        };
        let Some(media_dir) = std::env::var("CARGO_MANIFEST_DIR")
            .ok()
            .map(PathBuf::from)
            .map(|m| m.join("test_media"))
        else {
            return;
        };
        if !media_dir.join("641.webp").exists() || !media_dir.join("1000.webp").exists() {
            eprintln!("跳过：test_media 缺 641.webp / 1000.webp");
            return;
        }

        mpv_ffi::init(&dll_dir).expect("加载 libmpv 失败");

        // 目录模式注册（同生产：本地文件 O(1) seek 直通）
        let dir_vfs = crate::vfs::dir_vfs::DirVFS::open(&media_dir, &["webp".to_string()]).unwrap();
        let vfs: std::sync::Arc<dyn crate::vfs::ArchiveVFS> = std::sync::Arc::new(dir_vfs);
        let (archive_id, entries) = stream_cb::register_with_progress(vfs, &media_dir, &|_, _| {});
        let idx641 = entries
            .iter()
            .position(|e| e.name == "641.webp")
            .expect("应有 641.webp");
        let idx1000 = entries
            .iter()
            .position(|e| e.name == "1000.webp")
            .expect("应有 1000.webp");

        // headless，但 cache/demuxer/keep-open 与生产 PlayerService 一致
        let ctx = mpv_ffi::create().unwrap();
        mpv_ffi::set_option_string(ctx, "vo", "null").unwrap();
        mpv_ffi::set_option_string(ctx, "ao", "null").unwrap();
        mpv_ffi::set_option_string(ctx, "hwdec", "no").unwrap();
        mpv_ffi::set_option_string(ctx, "cache-secs", "30").unwrap();
        mpv_ffi::set_option_string(ctx, "demuxer-max-bytes", "512MiB").unwrap();
        mpv_ffi::set_option_string(ctx, "keep-open", "always").unwrap();
        mpv_ffi::stream_cb_add_ro(ctx, "zvf", std::ptr::null_mut(), stream_cb::open_cb)
            .expect("注册 zvf:// 失败");
        mpv_ffi::request_log_messages(ctx, "v").unwrap();

        fn ev_name(id: std::ffi::c_int) -> &'static str {
            match id {
                mpv_ffi::MPV_EVENT_NONE => "NONE",
                mpv_ffi::MPV_EVENT_SHUTDOWN => "SHUTDOWN",
                mpv_ffi::MPV_EVENT_LOG_MESSAGE => "LOG",
                mpv_ffi::MPV_EVENT_START_FILE => "START_FILE",
                mpv_ffi::MPV_EVENT_END_FILE => "END_FILE",
                mpv_ffi::MPV_EVENT_FILE_LOADED => "FILE_LOADED",
                mpv_ffi::MPV_EVENT_VIDEO_RECONFIG => "VIDEO_RECONFIG",
                mpv_ffi::MPV_EVENT_PLAYBACK_RESTART => "PLAYBACK_RESTART",
                _ => "OTHER",
            }
        }

        // loadfile 并抽水事件直到 PLAYBACK_RESTART（返回耗时）或超时；日志逐条带时间戳
        let run = |label: &str, uri: String| -> f64 {
            eprintln!("\n===== 场景 {label}: {uri}");
            let t0 = std::time::Instant::now();
            mpv_ffi::command_string(ctx, &format!("loadfile {uri} replace")).unwrap();
            loop {
                let ev = unsafe { mpv_ffi::wait_event(ctx, 0.1) };
                let dt = t0.elapsed().as_secs_f64();
                let id = unsafe { (*ev).event_id };
                if id == mpv_ffi::MPV_EVENT_NONE {
                    if dt > 20.0 {
                        eprintln!("[{dt:8.3}] !!! 20 秒未到 PLAYBACK_RESTART");
                        return dt;
                    }
                    continue;
                }
                if id == mpv_ffi::MPV_EVENT_LOG_MESSAGE {
                    let m = unsafe { (*ev).data as *mut mpv_ffi::MpvEventLogMessage };
                    let s = |p: *const std::ffi::c_char| unsafe {
                        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
                    };
                    eprintln!(
                        "[{dt:8.3}] {:<12} {}",
                        unsafe { s((*m).prefix) },
                        unsafe { s((*m).text) }.trim_end()
                    );
                    continue;
                }
                eprintln!("[{dt:8.3}] >>> 事件 {}", ev_name(id));
                if id == mpv_ffi::MPV_EVENT_PLAYBACK_RESTART {
                    eprintln!("===== 场景 {label} 首帧呈现耗时 {dt:.3}s");
                    return dt;
                }
            }
        };
        // 场景间隙抽水残余事件（上个文件的 EOF 等），不污染下个场景计时
        let drain = |ms: u64| {
            let dl = std::time::Instant::now() + std::time::Duration::from_millis(ms);
            while std::time::Instant::now() < dl {
                unsafe { mpv_ffi::wait_event(ctx, 0.05) };
            }
        };

        let t_a = run("A zvf冷启641", stream_cb::entry_uri(archive_id, idx641));
        drain(300);
        let t_b = run("B file直读1000", to_file_url(&media_dir.join("1000.webp")));
        drain(300);
        let t_c = run("C zvf切换1000", stream_cb::entry_uri(archive_id, idx1000));
        drain(300);

        mpv_ffi::destroy(ctx);
        eprintln!(
            "\n===== 汇总: A(zvf冷启)={t_a:.3}s  B(file直读)={t_b:.3}s  C(zvf切换)={t_c:.3}s"
        );
        eprintln!("A/C 慢而 B 快 → zvf 链路问题；三者都慢 → mpv/lavf 对 webp 本身的问题");
    }
}
