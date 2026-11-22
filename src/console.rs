//! Windows 控制台工具：ANSI VT 开启 + GUI 子系统下的致命错误弹窗。
//!
//! release 构建（`windows_subsystem = "windows"`，见 main.rs）没有控制台，
//! 日志走 logsink（环形缓冲 + archview.log 落盘），F12 界面面板查看；
//! debug 构建保持 console 子系统，cargo run 时终端直接可见日志。
//!
//! CLI 子命令（tidy/encode）的控制台接入各自自管——`Out::attach()`
//! （encode/cli.rs、vartidy/mod.rs）：已有控制台用 println，无则
//! AttachConsole(ATTACH_PARENT_PROCESS) + CONOUT$ 直写，附加失败
//! （SendTo/双击启动）输出走报告文件 + 结束弹窗。

use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::System::Console::{
    GetConsoleMode, GetStdHandle, SetConsoleMode, CONSOLE_MODE,
    ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
};

/// 开启 stdout/stderr 的 Virtual Terminal Processing（Win10+）。
/// tracing_subscriber 默认无条件输出 ANSI 颜色，legacy conhost 不开 VT
/// 时会把转义序列打成字面乱码（←[32m…）；开启后颜色正常渲染。
/// 对 Windows Terminal 无副作用；失败静默（无控制台/旧系统——release
/// windows 子系统常态如此），颜色退化为乱码但不影响功能。
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

/// 致命错误弹窗（windows 子系统下 eprintln/panic 均不可见，配置加载
/// 失败等启动期致命错误用本函数保证用户可见）。
pub fn fatal_msgbox(title: &str, text: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let t = wide(title);
    let c = wide(text);
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            c.as_ptr(),
            t.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

