//! 任务栏「最近」列表登记（jump list Recent 区）：Shell 只对文件关联启动
//! （双击 zip/7z/rar）自动登记，拖入/SendTo/对话框打开一律漏掉。这里在
//! 打开成功时由应用主动登记，压缩包/目录/单文件入口统一进最近项。
//!
//! **目录有官方限制**：SHAddToRecentDocs 文档明言"文件夹只出现在
//! Windows Explorer 任务栏按钮的跳转列表，不出现在任何其它应用的跳转
//! 列表"——SHARD_APPIDINFOIDLIST 对目录无效。因此两条登记路径：
//! - 文件（压缩包/媒体）：SHARD_APPIDINFOIDLIST——进「最近」区 + 用户
//!   Recent 文件夹（开始菜单可见），点击按文件关联启动
//! - 目录：SHARDAPPIDINFOLINK（文档指定的唯一正道）——IShellLink 指向
//!   本 exe、参数 = 带引号目录路径，System.Title = 目录名（硬性要求，
//!   缺了不显示）。点击带参启动 exe → 半单实例转发接住，运行中实例
//!   直接切换打开该目录
//!
//! AUMID 归属：winit 不设置 AppUserModelID（任务栏按钮按 exe 路径启发式
//! 分组），带显式 AUMID 的最近项无法归入——进程级
//! SetCurrentProcessExplicitAppUserModelID 须在建窗前调用认领同一标识。
//! **显式 AUMID 必须有对应的已安装快捷方式**（PKEY_AppUserModel_ID 属性），
//! Shell 才能把任务栏按钮解析为可固定应用：实测无快捷方式时拖文件到任务
//! 栏图标直接红色拒绝（v1 无 AUMID 时是正常的"固定到"提示）——
//! ensure_start_menu_shortcut 在启动时补齐用户级快捷方式。
//! UI 线程 COM 由 winit 初始化（CoInitializeEx APARTMENTTHREADED），
//! add 只能在 UI 线程调用（打开链路的 apply_open_done 满足）。

use std::path::{Path, PathBuf};

use windows_sys::core::{GUID, PCWSTR};
use windows_sys::Win32::Foundation::PROPERTYKEY;
use windows_sys::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows_sys::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows_sys::Win32::System::Variant::VT_LPWSTR;
use windows_sys::Win32::UI::Shell::{
    ILCreateFromPathW, ILFree, SetCurrentProcessExplicitAppUserModelID, SHAddToRecentDocs,
    SHARD_APPIDINFOLINK, SHARDAPPIDINFOLINK, SHARD_APPIDINFOIDLIST, SHARDAPPIDINFOIDLIST,
};

/// 本应用的 AppUserModelID：任务栏分组与 jump list 归属的统一标识
/// （带 NUL，encode_utf16 后即为 API 所需的 PCWSTR）
const APP_ID: &str = "snowjinight.archview\0";

/// windows-sys 不导出 COM 类/接口 GUID，以下为 shell32/propsys 的固定值
const CLSID_SHELL_LINK: GUID = guid(0x00021401, 0x0000, 0x0000, [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46]);
const IID_ISHELL_LINK_W: GUID = guid(0x000214F9, 0x0000, 0x0000, [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46]);
const IID_IPROPERTY_STORE: GUID = guid(0x886D8EEB, 0x8CF2, 0x4446, [0x8D, 0x02, 0xCD, 0xBA, 0x1D, 0xBD, 0xCF, 0x99]);
/// IPersistFile（存 .lnk：开始菜单快捷方式 + 回环测试）
const IID_IPERSIST_FILE: GUID = guid(0x0000010B, 0x0000, 0x0000, [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46]);
/// System.Title 的 fmtid（文档属性集 FMTID_DocumentSummaryInformation），
/// pid 2 = Title
const KEY_TITLE: PROPERTYKEY = PROPERTYKEY {
    fmtid: guid(0xF29F85E0, 0x4FF9, 0x1068, [0xAB, 0x91, 0x08, 0x00, 0x2B, 0x27, 0xB3, 0xD9]),
    pid: 2,
};
/// System.AppUserModel.ID（pid 5）：开始菜单快捷方式带此属性后，Shell
/// 才能把显式 AUMID 解析为已安装应用（GUID 核自 windows-0.62.2 生成码）
const KEY_APP_ID: PROPERTYKEY = PROPERTYKEY {
    fmtid: guid(0x9F4C2855, 0x9F79, 0x4B39, [0xA8, 0xD0, 0xE1, 0xD4, 0x2D, 0xE1, 0xD5, 0xF3]),
    pid: 5,
};

const fn guid(d1: u32, d2: u16, d3: u16, b: [u8; 8]) -> GUID {
    GUID { data1: d1, data2: d2, data3: d3, data4: b }
}

/// 进程级认领 AUMID：须在窗口创建前调用。重复设置返回失败——忽略
/// （每进程只调一次，此处不构成问题）。顺带补齐开始菜单快捷方式
/// （显式 AUMID 的解析前提，见模块文档）
pub fn init_app_id() {
    let id: Vec<u16> = APP_ID.encode_utf16().collect();
    unsafe {
        let _ = SetCurrentProcessExplicitAppUserModelID(id.as_ptr());
        ensure_start_menu_shortcut();
    }
}

/// 确保用户级开始菜单存在带本应用 AUMID 的快捷方式（exe 基名.lnk）。
/// 已存在则不重写（不刷新时间戳/不覆盖用户自定义）；失败静默
unsafe fn ensure_start_menu_shortcut() {
    // 建窗前 winit 尚未初始化 COM，这里自初始化（进程生命周期内不卸载；
    // 已初始化返回 S_FALSE/RPC_E_CHANGED_MODE 均无害）
    if CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) < 0 {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let Some(name) = exe.file_stem().and_then(|n| n.to_str()) else { return };
    let Some(appdata) = std::env::var_os("APPDATA") else { return };
    let lnk = PathBuf::from(appdata)
        .join(r"Microsoft\Windows\Start Menu\Programs")
        .join(format!("{name}.lnk"));
    if lnk.exists() {
        return;
    }
    let exe_w: Vec<u16> = exe.to_string_lossy().encode_utf16().chain([0]).collect();
    let lnk_w: Vec<u16> = lnk.to_string_lossy().encode_utf16().chain([0]).collect();
    let app_id_w: Vec<u16> = APP_ID.encode_utf16().collect();

    let mut link: *mut core::ffi::c_void = std::ptr::null_mut();
    if CoCreateInstance(&CLSID_SHELL_LINK, std::ptr::null_mut(), CLSCTX_INPROC_SERVER, &IID_ISHELL_LINK_W, &mut link) != 0
        || link.is_null()
    {
        return;
    }
    type HrPcwFn = unsafe extern "system" fn(*mut core::ffi::c_void, PCWSTR) -> i32;
    let set_path: HrPcwFn = vtable_slot(link, 20);
    let set_icon: unsafe extern "system" fn(*mut core::ffi::c_void, PCWSTR, i32) -> i32 =
        vtable_slot(link, 17);
    let _ = set_path(link, exe_w.as_ptr());
    let _ = set_icon(link, exe_w.as_ptr(), 0);

    // 快捷方式带 PKEY_AppUserModel_ID = APP_ID：Shell 据此把 AUMID 解析回
    // 本 exe（任务栏按钮固定/拖入/jump list 归属的前提）
    let qi: unsafe extern "system" fn(*mut core::ffi::c_void, *const GUID, *mut *mut core::ffi::c_void) -> i32 =
        vtable_slot(link, 0);
    let mut store: *mut core::ffi::c_void = std::ptr::null_mut();
    if qi(link, &IID_IPROPERTY_STORE, &mut store) == 0 && !store.is_null() {
        let mut pv = PROPVARIANT::default();
        pv.Anonymous.Anonymous.vt = VT_LPWSTR;
        pv.Anonymous.Anonymous.Anonymous.pwszVal = app_id_w.as_ptr() as *mut u16;
        let set_value: unsafe extern "system" fn(*mut core::ffi::c_void, *const PROPERTYKEY, *const PROPVARIANT) -> i32 =
            vtable_slot(store, 6);
        let commit: unsafe extern "system" fn(*mut core::ffi::c_void) -> i32 = vtable_slot(store, 7);
        let _ = set_value(store, &KEY_APP_ID, &pv);
        let _ = commit(store);
        release(store);
    }

    let mut persist: *mut core::ffi::c_void = std::ptr::null_mut();
    if qi(link, &IID_IPERSIST_FILE, &mut persist) == 0 && !persist.is_null() {
        let save: unsafe extern "system" fn(*mut core::ffi::c_void, PCWSTR, i32) -> i32 =
            vtable_slot(persist, 6);
        let _ = save(persist, lnk_w.as_ptr(), 0);
        release(persist);
    }
    release(link);
}

/// 打开成功后登记进任务栏「最近」（仅 UI 线程调用）。失败静默：增强
/// 特性，单条登记失败只损失一条最近项
pub fn add(path: &Path) {
    if path.is_dir() {
        unsafe { add_dir_link(path) };
    } else {
        unsafe { add_file_pidl(path) };
    }
}

/// 文件登记：路径 → PIDL → SHARDAPPIDINFOIDLIST（进最近区 + Recent 文件夹）
unsafe fn add_file_pidl(path: &Path) {
    let Some(p) = path.to_str() else { return };
    let wide: Vec<u16> = p.encode_utf16().chain([0]).collect();
    let pidl = ILCreateFromPathW(wide.as_ptr());
    if pidl.is_null() {
        return;
    }
    // AUMID 必须活到 SHAddToRecentDocs 返回——不能内联在 let 里（临时
    // Vec 语句末即释放，指针悬垂）
    let app_id: Vec<u16> = APP_ID.encode_utf16().collect();
    let info = SHARDAPPIDINFOIDLIST { pidl, pszAppID: app_id.as_ptr() };
    SHAddToRecentDocs(SHARD_APPIDINFOIDLIST as u32, &info as *const _ as *const core::ffi::c_void);
    ILFree(pidl);
}

/// 目录登记：SHARDAPPIDINFOLINK + IShellLink（见模块文档的官方限制说明）
unsafe fn add_dir_link(path: &Path) {
    let Ok(exe) = std::env::current_exe() else { return };
    let Some(title) = path.file_name().and_then(|n| n.to_str()) else { return };
    if title.is_empty() {
        return;
    }
    // 全部宽字符串活到本函数结束，指针使用期都在 AddToRecentDocs 内
    let exe_w: Vec<u16> = exe.to_string_lossy().encode_utf16().chain([0]).collect();
    let args_w: Vec<u16> =
        format!("\"{}\"", path.display()).encode_utf16().chain([0]).collect();
    let mut title_w: Vec<u16> = title.encode_utf16().chain([0]).collect();
    let app_id_w: Vec<u16> = APP_ID.encode_utf16().collect();

    let mut link: *mut core::ffi::c_void = std::ptr::null_mut();
    if CoCreateInstance(
        &CLSID_SHELL_LINK,
        std::ptr::null_mut(),
        CLSCTX_INPROC_SERVER,
        &IID_ISHELL_LINK_W,
        &mut link,
    ) != 0 || link.is_null() {
        return;
    }

    // IShellLinkW 虚表槽位：0 QI / 11 SetArguments / 17 SetIconLocation / 20 SetPath
    type HrFn = unsafe extern "system" fn(*mut core::ffi::c_void, PCWSTR) -> i32;
    let set_path: HrFn = vtable_slot(link, 20);
    let set_args: HrFn = vtable_slot(link, 11);
    let set_icon: unsafe extern "system" fn(*mut core::ffi::c_void, PCWSTR, i32) -> i32 =
        vtable_slot(link, 17);
    let _ = set_path(link, exe_w.as_ptr());
    let _ = set_args(link, args_w.as_ptr());
    let _ = set_icon(link, exe_w.as_ptr(), 0);

    // System.Title：SHARDAPPIDINFOLINK 的硬性要求（IShellLink 实现
    // IPropertyStore，直接 QI 后 SetValue + Commit）
    let qi: unsafe extern "system" fn(*mut core::ffi::c_void, *const GUID, *mut *mut core::ffi::c_void) -> i32 =
        vtable_slot(link, 0);
    let mut store: *mut core::ffi::c_void = std::ptr::null_mut();
    if qi(link, &IID_IPROPERTY_STORE, &mut store) == 0 && !store.is_null() {
        let mut pv = PROPVARIANT::default(); // 全零 = VT_EMPTY，改写为 VT_LPWSTR
        pv.Anonymous.Anonymous.vt = VT_LPWSTR;
        pv.Anonymous.Anonymous.Anonymous.pwszVal = title_w.as_mut_ptr();
        let set_value: unsafe extern "system" fn(*mut core::ffi::c_void, *const PROPERTYKEY, *const PROPVARIANT) -> i32 =
            vtable_slot(store, 6);
        let commit: unsafe extern "system" fn(*mut core::ffi::c_void) -> i32 = vtable_slot(store, 7);
        let _ = set_value(store, &KEY_TITLE, &pv);
        let _ = commit(store);
        release(store);
    }

    let info = SHARDAPPIDINFOLINK { psl: link, pszAppID: app_id_w.as_ptr() };
    SHAddToRecentDocs(SHARD_APPIDINFOLINK as u32, &info as *const _ as *const core::ffi::c_void);
    release(link);
}

/// 取 COM 对象 vtable 第 idx 槽并按调用方标注的签名解释（windows-sys 只
/// 暴露裸指针，无接口 vtable 定义；槽位序号按 shobjidl_core.h 方法顺序，
/// 已对照 windows-0.62 的 IShellLinkW_Vtbl 核实）。槽步进必须用 usize
///（8 字节）——c_void 是 ZST，对 *const c_void 做 add(idx) 恒为原地踏步，
/// 实际会取到槽 0（QueryInterface）按错误签名调用 → access violation
unsafe fn vtable_slot<F: Copy>(obj: *mut core::ffi::c_void, idx: usize) -> F {
    let vt: *const usize = *(obj as *const *const usize);
    std::ptr::read(vt.add(idx) as *const F)
}

/// IUnknown::Release（虚表槽 2）
unsafe fn release(obj: *mut core::ffi::c_void) {
    let f: unsafe extern "system" fn(*mut core::ffi::c_void) -> u32 = vtable_slot(obj, 2);
    f(obj);
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};

    /// 冒烟：目录（IShellLink 通道）+ 文件（PIDL 通道）登记不得崩溃。
    /// COM 由测试线程自行初始化（winit 不在场）
    #[test]
    fn recent_docs_smoke_no_crash() {
        unsafe {
            CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        add(&root.join("doc"));
        add(&root.join("Cargo.toml"));
    }

    /// 回环：目录链接经 IPersistFile 存盘重载后，目标/参数必须原样保留
    /// （Shell 激活最近项时按链接内容建命令行——参数丢失 = 目录点击裸
    /// 启动 exe = 单实例转发失效，多开实例）。参数特意带空格验证引号
    #[test]
    fn dir_link_roundtrip_keeps_arguments() {
        unsafe {
            CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
            let exe = std::env::current_exe().unwrap();
        let args = "\"E:\\some dir\\sub\"";

        // 构造与 add_dir_link 相同的链接并存成 .lnk
        let mut link: *mut core::ffi::c_void = std::ptr::null_mut();
        assert_eq!(
            CoCreateInstance(&CLSID_SHELL_LINK, std::ptr::null_mut(), CLSCTX_INPROC_SERVER, &IID_ISHELL_LINK_W, &mut link),
            0
        );
        let exe_w: Vec<u16> = exe.to_string_lossy().encode_utf16().chain([0]).collect();
        let args_w: Vec<u16> = args.encode_utf16().chain([0]).collect();
        type HrFn = unsafe extern "system" fn(*mut core::ffi::c_void, PCWSTR) -> i32;
        let set_path: HrFn = vtable_slot(link, 20);
        let set_args: HrFn = vtable_slot(link, 11);
        let hr_path = set_path(link, exe_w.as_ptr());
        let hr_args = set_args(link, args_w.as_ptr());
        assert_eq!(hr_path, 0, "SetPath 失败");
        assert_eq!(hr_args, 0, "SetArguments 失败");

        let tmp = std::env::temp_dir().join("archview_link_rt.lnk");
        let tmp_w: Vec<u16> = tmp.to_string_lossy().encode_utf16().chain([0]).collect();
        let qi: unsafe extern "system" fn(*mut core::ffi::c_void, *const GUID, *mut *mut core::ffi::c_void) -> i32 =
            vtable_slot(link, 0);
        let mut persist: *mut core::ffi::c_void = std::ptr::null_mut();
        assert_eq!(qi(link, &IID_IPERSIST_FILE, &mut persist), 0, "QI IPersistFile 失败");
        let save: unsafe extern "system" fn(*mut core::ffi::c_void, PCWSTR, i32) -> i32 =
            vtable_slot(persist, 6);
        assert_eq!(save(persist, tmp_w.as_ptr(), 0), 0, "Save 失败");
        release(persist);
        release(link);

        // 重载第二个链接，逐字段核对
        let mut link2: *mut core::ffi::c_void = std::ptr::null_mut();
        assert_eq!(
            CoCreateInstance(&CLSID_SHELL_LINK, std::ptr::null_mut(), CLSCTX_INPROC_SERVER, &IID_ISHELL_LINK_W, &mut link2),
            0
        );
        let qi2: unsafe extern "system" fn(*mut core::ffi::c_void, *const GUID, *mut *mut core::ffi::c_void) -> i32 =
            vtable_slot(link2, 0);
        let mut persist2: *mut core::ffi::c_void = std::ptr::null_mut();
        assert_eq!(qi2(link2, &IID_IPERSIST_FILE, &mut persist2), 0);
        let load: unsafe extern "system" fn(*mut core::ffi::c_void, PCWSTR, u32) -> i32 =
            vtable_slot(persist2, 5);
        assert_eq!(load(persist2, tmp_w.as_ptr(), 0), 0, "Load 失败");

        let get_path: unsafe extern "system" fn(*mut core::ffi::c_void, *mut u16, i32, *mut core::ffi::c_void, u32) -> i32 =
            vtable_slot(link2, 3);
        let mut path_buf = [0u16; 1024];
        assert_eq!(get_path(link2, path_buf.as_mut_ptr(), 1024, std::ptr::null_mut(), 4), 0);
        let got_path = String::from_utf16_lossy(&path_buf[..path_buf.iter().position(|&c| c == 0).unwrap()]);
        assert_eq!(got_path.to_ascii_lowercase(), exe.to_string_lossy().to_ascii_lowercase(), "目标 exe 不一致");

        let get_args: unsafe extern "system" fn(*mut core::ffi::c_void, *mut u16, i32) -> i32 =
            vtable_slot(link2, 10);
        let mut arg_buf = [0u16; 1024];
        assert_eq!(get_args(link2, arg_buf.as_mut_ptr(), 1024), 0);
        let got_args = String::from_utf16_lossy(&arg_buf[..arg_buf.iter().position(|&c| c == 0).unwrap()]);
        assert_eq!(got_args, args, "参数不一致（引号/空格被改写）");

        release(persist2);
        release(link2);
        let _ = std::fs::remove_file(&tmp);
        }
    }
}
