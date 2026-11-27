//! 半单实例：带可打开目标（压缩包/目录/媒体文件）启动且已有实例运行时，
//! 把目标经 WM_COPYDATA 转发给旧实例（其窗口子类过程接收，进入与任务栏
//! 拖入相同的队列 → open_path 直接替换当前打开的内容），前置旧实例窗口
//! 后本进程退出；**裸启动（无可打开目标）不参与单实例**，保持开新窗口
//! 的行为（任务栏右键图标/双击 exe 期待的就是新窗口）。
//!
//! 覆盖所有"带路径启动"入口：任务栏驻留图标拖入触发的 Shell 启动（新
//! 系统的行为——弹"打开方式"选中后带参启动）、拖到 exe、SendTo、文件
//! 关联。判据 = 命名互斥体（进程退出自动释放，无需显式清理）。查找旧
//! 实例主窗口按"同 exe 名的可见顶层有标题窗口"过滤，不依赖标题/类名。

use std::path::Path;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM,
};
use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
use windows_sys::Win32::System::Threading::{
    CreateMutexW, OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, BringWindowToTop, GetWindowThreadProcessId, GetWindowTextLengthW,
    IsWindowVisible, SendMessageW, SetForegroundWindow, ShowWindow, SW_RESTORE, WM_COPYDATA,
};

/// 转发入口：`initial` = 本次启动的可打开目标（main.rs initial_archive 的
/// 判定结果）。**所有 GUI 实例都认领互斥体**（含裸启动——否则裸启动的
/// 实例不占名，后续带路径启动检测不到它，转发就失效了）；裸启动或首
/// 实例正常返回继续开窗，带目标且已有实例运行时转发并退出本进程
pub fn forward_if_exists(initial: Option<&Path>) {
    unsafe {
        let name: Vec<u16> = "archview_instance\0".encode_utf16().collect();
        let mutex = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if mutex.is_null() {
            return; // 创建失败退回多实例行为，不阻塞启动
        }
        if GetLastError() != ERROR_ALREADY_EXISTS {
            return; // 首实例：继续正常启动（句柄随进程存续，无需关闭）
        }
        // 已有实例运行。裸启动（无可打开目标）不参与转发：保持开新窗口
        let Some(target) = initial else {
            return;
        };
        let Some(exe_name) = std::env::current_exe().ok().and_then(|p| {
            p.file_name().map(|n| n.to_string_lossy().encode_utf16().collect::<Vec<u16>>())
        }) else {
            return;
        };
        let mut ctx = FindCtx { exe_name, hwnd: 0 };
        EnumWindows(Some(enum_proc), &mut ctx as *mut _ as LPARAM);
        if ctx.hwnd == 0 {
            return; // 没找到旧窗口（如旧实例恰在退出）：继续启动，不丢本次打开目标
        }
        let hwnd = ctx.hwnd as HWND;
        // 转发目标（UTF-16 + NUL）；接收方与排空方同队列，open_path 自行校验
        let mut wide: Vec<u16> = target.to_string_lossy().encode_utf16().collect();
        wide.push(0);
        let cds = COPYDATASTRUCT {
            dwData: 0,
            cbData: (wide.len() * 2) as u32,
            lpData: wide.as_ptr() as *mut core::ffi::c_void,
        };
        SendMessageW(hwnd, WM_COPYDATA, 0, &cds as *const _ as LPARAM);
        // 尽力前台化：最小化的先还原
        ShowWindow(hwnd, SW_RESTORE);
        SetForegroundWindow(hwnd);
        BringWindowToTop(hwnd);
        std::process::exit(0);
    }
}

/// EnumWindows 的搜索上下文：exe 文件名（宽字符）+ 命中的主窗口句柄
/// （0 = 未找到；HWND 是指针，用 usize 存哨兵）
struct FindCtx {
    exe_name: Vec<u16>,
    hwnd: usize,
}

/// 枚举回调：命中同 exe 名的可见有标题顶层窗口即记录并终止枚举
unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> i32 {
    let ctx = &mut *(lparam as *mut FindCtx);
    if ctx.hwnd != 0 {
        return 0;
    }
    // 隐藏窗口/无标题辅助窗口跳过（eframe 主窗口两者皆满足）
    if IsWindowVisible(hwnd) == 0 || GetWindowTextLengthW(hwnd) == 0 {
        return 1;
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid == 0 {
        return 1;
    }
    let proc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if proc.is_null() {
        return 1;
    }
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(proc, 0, buf.as_mut_ptr(), &mut len);
    CloseHandle(proc);
    if ok != 0 {
        let full = &buf[..len as usize];
        // 取路径末段文件名，ASCII 大小写不敏感比较
        let name = match full.iter().rposition(|&c| c == '\\' as u16) {
            Some(i) => &full[i + 1..],
            None => full,
        };
        if name.len() == ctx.exe_name.len()
            && name.iter().zip(&ctx.exe_name).all(|(a, b)| ascii_upper(*a) == ascii_upper(*b))
        {
            ctx.hwnd = hwnd as usize;
            return 0;
        }
    }
    1
}

fn ascii_upper(c: u16) -> u16 {
    if (b'a' as u16..=b'z' as u16).contains(&c) { c - 32 } else { c }
}
