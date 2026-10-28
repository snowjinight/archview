//! mpv 最小 FFI 绑定：libloading 运行时动态加载 libmpv-2.dll。
//! 原因：shinchiro 构建的 libmpv.dll.a 是 MinGW 导入库，MSVC 无法链接，故运行时加载。

use libloading::Library;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::Path;
use std::sync::OnceLock;

// mpv 错误码（client.h）
pub const MPV_ERROR_SUCCESS: c_int = 0;
pub const MPV_ERROR_LOADING_FAILED: c_int = -13;
pub const MPV_ERROR_UNSUPPORTED: c_int = -18;
// 属性格式
pub const MPV_FORMAT_FLAG: c_int = 3;
pub const MPV_FORMAT_DOUBLE: c_int = 5;
// mpv_event_id（client.h 精确值）
pub const MPV_EVENT_NONE: c_int = 0;
pub const MPV_EVENT_SHUTDOWN: c_int = 1;
pub const MPV_EVENT_LOG_MESSAGE: c_int = 2;
pub const MPV_EVENT_START_FILE: c_int = 5;
pub const MPV_EVENT_END_FILE: c_int = 6;
pub const MPV_EVENT_FILE_LOADED: c_int = 7;
pub const MPV_EVENT_VIDEO_RECONFIG: c_int = 15;
pub const MPV_EVENT_PLAYBACK_RESTART: c_int = 21;

pub type MpvHandle = c_void;

// 回调类型签名（stream_cb.h）
pub type StreamCbOpenRoFn = unsafe extern "C" fn(
    user_data: *mut c_void,
    uri: *mut c_char,
    info: *mut StreamCbInfo,
) -> c_int;
pub type StreamCbReadFn =
    unsafe extern "C" fn(cookie: *mut c_void, buf: *mut c_char, nbytes: u64) -> i64;
pub type StreamCbSeekFn = unsafe extern "C" fn(cookie: *mut c_void, offset: i64) -> i64;
pub type StreamCbSizeFn = unsafe extern "C" fn(cookie: *mut c_void) -> i64;
pub type StreamCbCloseFn = unsafe extern "C" fn(cookie: *mut c_void);

#[repr(C)]
pub struct StreamCbInfo {
    pub cookie: *mut c_void,
    pub read_fn: Option<StreamCbReadFn>,
    pub seek_fn: Option<StreamCbSeekFn>,
    pub size_fn: Option<StreamCbSizeFn>,
    pub close_fn: Option<StreamCbCloseFn>,
    pub cancel_fn: Option<unsafe extern "C" fn(cookie: *mut c_void)>,
}

// ---------- 事件轮询（client.h，诊断与后续事件驱动用） ----------

/// mpv_wait_event 返回的事件头。data 按 event_id 解释（LOG_MESSAGE → MpvEventLogMessage）
#[repr(C)]
pub struct MpvEvent {
    pub event_id: c_int,
    pub error: c_int,
    pub reply_userdata: u64,
    pub data: *mut c_void,
}

/// MPV_EVENT_LOG_MESSAGE 的 data（字符串指针归 mpv 所有，仅当次事件内有效）
#[repr(C)]
pub struct MpvEventLogMessage {
    pub prefix: *const c_char,
    pub level: *const c_char,
    pub text: *const c_char,
    pub log_level: c_int,
}

// ---------- render API（render.h，用于 egui 单平面纹理渲染，ARCHITECTURE.md 13）----------

pub type MpvRenderContext = c_void;

// mpv_render_param_type 精确枚举值
pub const MPV_RENDER_PARAM_INVALID: u32 = 0;
pub const MPV_RENDER_PARAM_API_TYPE: u32 = 1;
pub const MPV_RENDER_PARAM_OPENGL_INIT_PARAMS: u32 = 2;
pub const MPV_RENDER_PARAM_OPENGL_FBO: u32 = 3;
pub const MPV_RENDER_PARAM_FLIP_Y: u32 = 4;
// mpv_render_update_flag：update 返回 FRAME 位 = 有新帧待渲染
// （PaintCallback 闭包用它做真实首帧检出）
pub const MPV_RENDER_UPDATE_FRAME: u64 = 1 << 0;

pub const MPV_RENDER_API_TYPE_OPENGL: &[u8] = b"opengl\0";

/// get_proc_address 回调：按名称返回 GL 函数指针（喂给 mpv 的 GL 加载器）
pub type GetProcAddressFn =
    unsafe extern "C" fn(ctx: *mut c_void, name: *const c_char) -> *mut c_void;
/// render 更新回调：无返回值（render.h: typedef void (*mpv_render_update_fn)(void *cb_ctx)）
pub type RenderUpdateFn = unsafe extern "C" fn(cb_ctx: *mut c_void);

/// 参数数组元素，以 `{ MPV_RENDER_PARAM_INVALID, null }` 结尾
#[repr(C)]
pub struct MpvRenderParam {
    pub param_type: u32,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct MpvOpenglInitParams {
    pub get_proc_address: Option<GetProcAddressFn>,
    pub get_proc_address_ctx: *mut c_void,
}

/// 注意：宽高在结构体内（不是独立的 FBO_WIDTH/HEIGHT 参数）
#[repr(C)]
pub struct MpvOpenglFbo {
    pub fbo: c_int,
    pub w: c_int,
    pub h: c_int,
    pub internal_format: c_int,
}

struct MpvApi {
    _lib: Library,
    create: unsafe extern "C" fn() -> *mut MpvHandle,
    initialize: unsafe extern "C" fn(ctx: *mut MpvHandle) -> c_int,
    terminate_destroy: unsafe extern "C" fn(ctx: *mut MpvHandle),
    set_option_string: unsafe extern "C" fn(
        ctx: *mut MpvHandle,
        name: *const c_char,
        data: *const c_char,
    ) -> c_int,
    command_string: unsafe extern "C" fn(ctx: *mut MpvHandle, args: *const c_char) -> c_int,
    set_property_string: unsafe extern "C" fn(
        ctx: *mut MpvHandle,
        name: *const c_char,
        data: *const c_char,
    ) -> c_int,
    get_property: unsafe extern "C" fn(
        ctx: *mut MpvHandle,
        name: *const c_char,
        format: c_int,
        out: *mut c_void,
    ) -> c_int,
    error_string: unsafe extern "C" fn(err: c_int) -> *const c_char,
    request_log_messages:
        unsafe extern "C" fn(ctx: *mut MpvHandle, min_level: *const c_char) -> c_int,
    wait_event: unsafe extern "C" fn(ctx: *mut MpvHandle, timeout: f64) -> *mut MpvEvent,
    stream_cb_add_ro: unsafe extern "C" fn(
        ctx: *mut MpvHandle,
        protocol: *const c_char,
        user_data: *mut c_void,
        open_fn: StreamCbOpenRoFn,
    ) -> c_int,
    // render API（签名严格对照 mpv render.h，参数顺序不可颠倒）
    render_context_create: unsafe extern "C" fn(
        res: *mut *mut MpvRenderContext,
        mpv: *mut MpvHandle,
        params: *mut MpvRenderParam,
    ) -> c_int,
    render_context_free: unsafe extern "C" fn(ctx: *mut MpvRenderContext),
    render_context_set_update_callback: unsafe extern "C" fn(
        ctx: *mut MpvRenderContext,
        callback: Option<RenderUpdateFn>,
        callback_ctx: *mut c_void,
    ),
    render_context_update: unsafe extern "C" fn(ctx: *mut MpvRenderContext) -> u64,
    render_context_render: unsafe extern "C" fn(
        ctx: *mut MpvRenderContext,
        params: *mut MpvRenderParam,
    ) -> c_int,
}

static API: OnceLock<MpvApi> = OnceLock::new();

fn load_err(e: impl std::fmt::Display) -> String {
    format!("加载 libmpv-2.dll 失败: {e}")
}

/// 加载 dll。dll_dir 为搜索目录（exe 旁或 config 指定）。
pub fn init(dll_dir: &Path) -> Result<(), String> {
    if API.get().is_some() {
        return Ok(());
    }
    let dll_path = dll_dir.join("libmpv-2.dll");
    // 若目录里没有，退回系统搜索（PATH）
    let lib = unsafe {
        if dll_path.exists() {
            Library::new(&dll_path).map_err(load_err)?
        } else {
            Library::new("libmpv-2.dll").map_err(load_err)?
        }
    };
    unsafe {
        let api = MpvApi {
            create: *lib.get(b"mpv_create").map_err(load_err)?,
            initialize: *lib.get(b"mpv_initialize").map_err(load_err)?,
            terminate_destroy: *lib.get(b"mpv_terminate_destroy").map_err(load_err)?,
            set_option_string: *lib.get(b"mpv_set_option_string").map_err(load_err)?,
            command_string: *lib.get(b"mpv_command_string").map_err(load_err)?,
            set_property_string: *lib.get(b"mpv_set_property_string").map_err(load_err)?,
            get_property: *lib.get(b"mpv_get_property").map_err(load_err)?,
            error_string: *lib.get(b"mpv_error_string").map_err(load_err)?,
            request_log_messages: *lib.get(b"mpv_request_log_messages").map_err(load_err)?,
            wait_event: *lib.get(b"mpv_wait_event").map_err(load_err)?,
            stream_cb_add_ro: *lib.get(b"mpv_stream_cb_add_ro").map_err(load_err)?,
            render_context_create: *lib.get(b"mpv_render_context_create").map_err(load_err)?,
            render_context_free: *lib.get(b"mpv_render_context_free").map_err(load_err)?,
            render_context_set_update_callback: *lib
                .get(b"mpv_render_context_set_update_callback")
                .map_err(load_err)?,
            render_context_update: *lib.get(b"mpv_render_context_update").map_err(load_err)?,
            render_context_render: *lib.get(b"mpv_render_context_render").map_err(load_err)?,
            _lib: lib,
        };
        // 并发竞态输家（如两个 e2e 测试并行 init）：复用赢家已装的 API
        let _ = API.set(api);
    }
    Ok(())
}

fn api() -> Result<&'static MpvApi, String> {
    API.get().ok_or_else(|| "libmpv 未初始化".to_string())
}

pub fn err_str(err: c_int) -> String {
    match api() {
        Ok(a) => unsafe {
            CStr::from_ptr((a.error_string)(err))
                .to_string_lossy()
                .into_owned()
        },
        Err(_) => format!("mpv 错误 {err}"),
    }
}

/// 创建并初始化 mpv 实例
pub fn create() -> Result<*mut MpvHandle, String> {
    let a = api()?;
    unsafe {
        let ctx = (a.create)();
        if ctx.is_null() {
            return Err("mpv_create 返回空".into());
        }
        let r = (a.initialize)(ctx);
        if r < 0 {
            (a.terminate_destroy)(ctx);
            return Err(format!("mpv_initialize 失败: {}", err_str(r)));
        }
        Ok(ctx)
    }
}

pub fn destroy(ctx: *mut MpvHandle) {
    if let Ok(a) = api() {
        if !ctx.is_null() {
            unsafe { (a.terminate_destroy)(ctx) };
        }
    }
}

pub fn set_option_string(ctx: *mut MpvHandle, name: &str, data: &str) -> Result<(), String> {
    let a = api()?;
    let name = CString::new(name).map_err(|e| e.to_string())?;
    let data = CString::new(data).map_err(|e| e.to_string())?;
    let r = unsafe { (a.set_option_string)(ctx, name.as_ptr(), data.as_ptr()) };
    if r < 0 {
        Err(err_str(r))
    } else {
        Ok(())
    }
}

pub fn command_string(ctx: *mut MpvHandle, args: &str) -> Result<(), String> {
    let a = api()?;
    let args = CString::new(args).map_err(|e| e.to_string())?;
    let r = unsafe { (a.command_string)(ctx, args.as_ptr()) };
    if r < 0 {
        Err(err_str(r))
    } else {
        Ok(())
    }
}

pub fn set_property_string(ctx: *mut MpvHandle, name: &str, data: &str) -> Result<(), String> {
    let a = api()?;
    let name = CString::new(name).map_err(|e| e.to_string())?;
    let data = CString::new(data).map_err(|e| e.to_string())?;
    let r = unsafe { (a.set_property_string)(ctx, name.as_ptr(), data.as_ptr()) };
    if r < 0 {
        Err(err_str(r))
    } else {
        Ok(())
    }
}

/// 读 f64 属性（time-pos / duration 等），属性不存在返回 None
pub fn get_property_f64(ctx: *mut MpvHandle, name: &str) -> Result<Option<f64>, String> {
    let a = api()?;
    let name = CString::new(name).map_err(|e| e.to_string())?;
    let mut out: f64 = 0.0;
    let r = unsafe {
        (a.get_property)(
            ctx,
            name.as_ptr(),
            MPV_FORMAT_DOUBLE,
            &mut out as *mut f64 as *mut c_void,
        )
    };
    if r == MPV_ERROR_SUCCESS {
        Ok(Some(out))
    } else {
        Ok(None) // 属性不存在（如未加载文件时 time-pos）
    }
}

/// 读 flag 属性（pause / eof-reached / idle-active 等），返回 0/1
pub fn get_property_flag(ctx: *mut MpvHandle, name: &str) -> Result<Option<bool>, String> {
    let a = api()?;
    let name = CString::new(name).map_err(|e| e.to_string())?;
    let mut out: c_int = 0;
    let r = unsafe {
        (a.get_property)(
            ctx,
            name.as_ptr(),
            MPV_FORMAT_FLAG,
            &mut out as *mut c_int as *mut c_void,
        )
    };
    if r == MPV_ERROR_SUCCESS {
        Ok(Some(out != 0))
    } else {
        Ok(None)
    }
}

/// 订阅 mpv 内部日志消息（client.h mpv_request_log_messages）。
/// min_level："no"/"fatal"/"error"/"warn"/"info"/"status"/"v"/"debug"/"trace"
pub fn request_log_messages(ctx: *mut MpvHandle, min_level: &str) -> Result<(), String> {
    let a = api()?;
    let lvl = CString::new(min_level).map_err(|e| e.to_string())?;
    let r = unsafe { (a.request_log_messages)(ctx, lvl.as_ptr()) };
    if r < 0 {
        Err(err_str(r))
    } else {
        Ok(())
    }
}

/// 轮询一个事件（timeout 秒，0=立即返回）。无事件时返回 event_id == MPV_EVENT_NONE
/// 的指针（mpv 静态占位）。事件内存归 mpv 所有，必须在下一次 wait_event 前读完。
pub unsafe fn wait_event(ctx: *mut MpvHandle, timeout_s: f64) -> *mut MpvEvent {
    let a = api().expect("libmpv 未初始化");
    (a.wait_event)(ctx, timeout_s)
}

/// 注册 stream_cb 只读协议（zvf://）
pub fn stream_cb_add_ro(
    ctx: *mut MpvHandle,
    protocol: &str,
    user_data: *mut c_void,
    open_fn: StreamCbOpenRoFn,
) -> Result<(), String> {
    let a = api()?;
    let protocol = CString::new(protocol).map_err(|e| e.to_string())?;
    let r = unsafe { (a.stream_cb_add_ro)(ctx, protocol.as_ptr(), user_data, open_fn) };
    if r < 0 {
        Err(err_str(r))
    } else {
        Ok(())
    }
}

/// 创建 render context（须在 GL current 的线程上调用；mpv 侧先设好 vo=libmpv）
pub fn render_context_create(
    mpv: *mut MpvHandle,
    get_proc_address: GetProcAddressFn,
    gp_ctx: *mut c_void,
) -> Result<*mut MpvRenderContext, String> {
    let a = api()?;
    unsafe {
        let init_params = MpvOpenglInitParams {
            get_proc_address: Some(get_proc_address),
            get_proc_address_ctx: gp_ctx,
        };
        let mut params = [
            MpvRenderParam {
                param_type: MPV_RENDER_PARAM_API_TYPE,
                data: MPV_RENDER_API_TYPE_OPENGL.as_ptr() as *mut c_void,
            },
            MpvRenderParam {
                param_type: MPV_RENDER_PARAM_OPENGL_INIT_PARAMS,
                data: &init_params as *const MpvOpenglInitParams as *mut c_void,
            },
            MpvRenderParam {
                param_type: MPV_RENDER_PARAM_INVALID,
                data: std::ptr::null_mut(),
            },
        ];
        let mut rctx: *mut MpvRenderContext = std::ptr::null_mut();
        let r = (a.render_context_create)(&mut rctx, mpv, params.as_mut_ptr());
        if r < 0 {
            return Err(format!("mpv_render_context_create 失败: {}", err_str(r)));
        }
        Ok(rctx)
    }
}

/// 释放 render context。必须先于 mpv handle 释放、在创建它的 GL current 线程上调用。
pub fn render_context_free(ctx: *mut MpvRenderContext) {
    if let Ok(a) = api() {
        if !ctx.is_null() {
            unsafe { (a.render_context_free)(ctx) };
        }
    }
}

/// 设置帧更新回调（callback 传 None 即移除）
pub fn render_context_set_update_callback(
    ctx: *mut MpvRenderContext,
    callback: Option<RenderUpdateFn>,
    callback_ctx: *mut c_void,
) {
    if let Ok(a) = api() {
        if !ctx.is_null() {
            unsafe { (a.render_context_set_update_callback)(ctx, callback, callback_ctx) };
        }
    }
}

/// 查询待处理更新标志位（MPV_RENDER_UPDATE_FRAME 等）
pub fn render_context_update(ctx: *mut MpvRenderContext) -> u64 {
    match api() {
        Ok(a) if !ctx.is_null() => unsafe { (a.render_context_update)(ctx) },
        _ => 0,
    }
}

/// 渲染一帧到指定 FBO。flip_y=1：GL FBO 原点在左下，让 mpv 翻转以匹配 GL 约定。
pub fn render_gl_render(ctx: *mut MpvRenderContext, fbo: c_int, w: c_int, h: c_int) -> Result<(), String> {
    let a = api()?;
    unsafe {
        let mut fbo_param = MpvOpenglFbo { fbo, w, h, internal_format: 0 };
        let mut flip_y: c_int = 1;
        let mut params = [
            MpvRenderParam {
                param_type: MPV_RENDER_PARAM_OPENGL_FBO,
                data: &mut fbo_param as *mut MpvOpenglFbo as *mut c_void,
            },
            MpvRenderParam {
                param_type: MPV_RENDER_PARAM_FLIP_Y,
                data: &mut flip_y as *mut c_int as *mut c_void,
            },
            MpvRenderParam {
                param_type: MPV_RENDER_PARAM_INVALID,
                data: std::ptr::null_mut(),
            },
        ];
        let r = (a.render_context_render)(ctx, params.as_mut_ptr());
        if r < 0 {
            Err(format!("mpv_render_context_render 失败: {}", err_str(r)))
        } else {
            Ok(())
        }
    }
}
