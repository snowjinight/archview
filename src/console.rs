//! Windows 控制台管理：ANSI 转义开启、启动来源判定、控制台窗口显隐切换。
//!
//! exe 是 console 子系统，Windows 加载时总会分配一个控制台窗口（黑框），
//! 代码无法阻止其创建，但可以在启动早期隐藏它。策略（doc/架构落地开发）：
//! - 从终端启动（cmd/powershell/wt/…）→ 保留窗口，用户本来就想看日志；
//! - 双击/资源管理器/sendto/拖拽启动 → 隐藏，GUI 用户不需要看黑框；
//! - F12（UI 全局快捷键，ui/nav.rs handle_shortcuts）随时调出/收回。
//!
//! 隐藏只是窗口不可见，控制台及其缓冲区仍在，日志照常写入。

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Console::{
    GetConsoleMode, GetConsoleWindow, GetStdHandle, SetConsoleMode, CONSOLE_MODE,
    ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    IsIconic, IsWindowVisible, ShowWindow, SW_HIDE, SW_RESTORE, SW_SHOW,
};

/// 开启 stdout/stderr 的 Virtual Terminal Processing（Win10+）。
/// tracing_subscriber 默认无条件输出 ANSI 颜色，legacy conhost 不开 VT
/// 时会把转义序列打成字面乱码（←[32m…）；开启后颜色正常渲染。
/// 对 Windows Terminal 无副作用；失败静默（无控制台/旧系统），颜色
/// 退化为乱码但不影响功能。
pub fn enable_ansi() {
    for std_handle in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        unsafe {
            let h = GetStdHandle(std_handle);
            if h.is_null() || h == INVALID_HANDLE_VALUE {
                continue;
            }
            let mut mode: CONSOLE_MODE = 0;
            if GetConsoleMode(h, &mut mode) != 0 {
                SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
            }
        }
    }
}

/// 终端类父进程镜像名（小写）。终端模拟器（wt/conhost）之下必然是 shell
/// 进程，所以匹配 shell 名即可覆盖所有"用户主动开终端跑 exe"的场景。
const TERMINAL_PARENTS: &[&str] = &[
    "cmd.exe",
    "powershell.exe",
    "pwsh.exe",
    "wt.exe",
    "windowsterminal.exe",
    "conhost.exe",
    "bash.exe",
    "zsh.exe",
    "fish.exe",
    "wsl.exe",
    "mintty.exe",
    "far.exe",
];

/// 启动来源是否终端：父进程是 shell/终端（cmd/powershell/…），
/// 或处于 cargo run 开发环境（CARGO 环境变量存在，父进程是 cargo）。
/// 判定失败（快照/权限问题）保守返回 true——宁可多显示不可错隐藏。
pub fn launched_from_terminal() -> bool {
    if std::env::var_os("CARGO").is_some() {
        return true;
    }
    match parent_image_name() {
        Some(path) => {
            let name = path
                .rsplit(['\\', '/'])
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            TERMINAL_PARENTS.contains(&name.as_str())
        }
        None => true,
    }
}

/// 父进程完整镜像路径（ToolHelp 快照查自身 PID → 父 PID → 查镜像名）。
fn parent_image_name() -> Option<String> {
    unsafe {
        let me = windows_sys::Win32::System::Threading::GetCurrentProcessId();
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap.is_null() || snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut parent_pid = None;
        if Process32FirstW(snap, &mut entry) != 0 {
            loop {
                if entry.th32ProcessID == me {
                    parent_pid = Some(entry.th32ParentProcessID);
                    break;
                }
                if Process32NextW(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
        let parent = parent_pid?;
        let proc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, parent);
        if proc.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok =
            QueryFullProcessImageNameW(proc, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) != 0;
        CloseHandle(proc);
        if ok {
            Some(String::from_utf16_lossy(&buf[..len as usize]))
        } else {
            None
        }
    }
}

/// 控制台窗口当前是否可见（无控制台 = false）
pub fn is_visible() -> bool {
    let hwnd = unsafe { GetConsoleWindow() };
    !hwnd.is_null() && unsafe { IsWindowVisible(hwnd) != 0 }
}

/// 显示（最小化则还原）或隐藏控制台窗口
pub fn set_visible(show: bool) {
    let hwnd = unsafe { GetConsoleWindow() };
    if hwnd.is_null() {
        return;
    }
    unsafe {
        let cmd = if show {
            if IsIconic(hwnd) != 0 {
                SW_RESTORE
            } else {
                SW_SHOW
            }
        } else {
            SW_HIDE
        };
        ShowWindow(hwnd, cmd);
    }
}

/// 切换控制台显隐，返回切换后是否可见
pub fn toggle() -> bool {
    let show = !is_visible();
    set_visible(show);
    show
}
