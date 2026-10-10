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

pub type MpvHandle = c_void;

// 回调类型签名（stream_cb.h）
pub type StreamCbOpenRoFn =
    unsafe extern "C" fn(user_data: *mut c_void, uri: *mut c_char, info: *mut StreamCbInfo) -> c_int;
pub type StreamCbReadFn = unsafe extern "C" fn(cookie: *mut c_void, buf: *mut c_char, nbytes: u64) -> i64;
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

struct MpvApi {
    _lib: Library,
    create: unsafe extern "C" fn() -> *mut MpvHandle,
    initialize: unsafe extern "C" fn(ctx: *mut MpvHandle) -> c_int,
    terminate_destroy: unsafe extern "C" fn(ctx: *mut MpvHandle),
    set_option_string: unsafe extern "C" fn(ctx: *mut MpvHandle, name: *const c_char, data: *const c_char) -> c_int,
    command_string: unsafe extern "C" fn(ctx: *mut MpvHandle, args: *const c_char) -> c_int,
    set_property_string: unsafe extern "C" fn(ctx: *mut MpvHandle, name: *const c_char, data: *const c_char) -> c_int,
    get_property: unsafe extern "C" fn(ctx: *mut MpvHandle, name: *const c_char, format: c_int, out: *mut c_void) -> c_int,
    get_property_string: unsafe extern "C" fn(ctx: *mut MpvHandle, name: *const c_char) -> *mut c_char,
    free: unsafe extern "C" fn(data: *mut c_void),
    error_string: unsafe extern "C" fn(err: c_int) -> *const c_char,
    stream_cb_add_ro: unsafe extern "C" fn(ctx: *mut MpvHandle, protocol: *const c_char, user_data: *mut c_void, open_fn: StreamCbOpenRoFn) -> c_int,
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
            get_property_string: *lib.get(b"mpv_get_property_string").map_err(load_err)?,
            free: *lib.get(b"mpv_free").map_err(load_err)?,
            error_string: *lib.get(b"mpv_error_string").map_err(load_err)?,
            stream_cb_add_ro: *lib.get(b"mpv_stream_cb_add_ro").map_err(load_err)?,
            _lib: lib,
        };
        API.set(api).map_err(|_| "mpv API 已初始化".to_string())?;
    }
    Ok(())
}

fn api() -> Result<&'static MpvApi, String> {
    API.get().ok_or_else(|| "libmpv 未初始化".to_string())
}

pub fn err_str(err: c_int) -> String {
    match api() {
        Ok(a) => unsafe {
            CStr::from_ptr((a.error_string)(err)).to_string_lossy().into_owned()
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
    if r < 0 { Err(err_str(r)) } else { Ok(()) }
}

pub fn command_string(ctx: *mut MpvHandle, args: &str) -> Result<(), String> {
    let a = api()?;
    let args = CString::new(args).map_err(|e| e.to_string())?;
    let r = unsafe { (a.command_string)(ctx, args.as_ptr()) };
    if r < 0 { Err(err_str(r)) } else { Ok(()) }
}

pub fn set_property_string(ctx: *mut MpvHandle, name: &str, data: &str) -> Result<(), String> {
    let a = api()?;
    let name = CString::new(name).map_err(|e| e.to_string())?;
    let data = CString::new(data).map_err(|e| e.to_string())?;
    let r = unsafe { (a.set_property_string)(ctx, name.as_ptr(), data.as_ptr()) };
    if r < 0 { Err(err_str(r)) } else { Ok(()) }
}

/// 读 f64 属性（time-pos / duration 等），属性不存在返回 None
pub fn get_property_f64(ctx: *mut MpvHandle, name: &str) -> Result<Option<f64>, String> {
    let a = api()?;
    let name = CString::new(name).map_err(|e| e.to_string())?;
    let mut out: f64 = 0.0;
    let r = unsafe { (a.get_property)(ctx, name.as_ptr(), MPV_FORMAT_DOUBLE, &mut out as *mut f64 as *mut c_void) };
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
    let r = unsafe { (a.get_property)(ctx, name.as_ptr(), MPV_FORMAT_FLAG, &mut out as *mut c_int as *mut c_void) };
    if r == MPV_ERROR_SUCCESS { Ok(Some(out != 0)) } else { Ok(None) }
}

pub fn get_property_string(ctx: *mut MpvHandle, name: &str) -> Option<String> {
    let a = api().ok()?;
    let name = CString::new(name).ok()?;
    unsafe {
        let ptr = (a.get_property_string)(ctx, name.as_ptr());
        if ptr.is_null() {
            return None;
        }
        let s = CStr::from_ptr(ptr).to_string_lossy().into_owned();
        (a.free)(ptr as *mut c_void);
        Some(s)
    }
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
    if r < 0 { Err(err_str(r)) } else { Ok(()) }
}
