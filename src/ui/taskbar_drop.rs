//! 任务栏驻留图标拖入通道（Windows 专用）：winit 的 OLE 拖放只注册在窗口
//! 客户区，拖到任务栏驻留图标上不进 egui 的 dropped_files。这里用 Shell 层
//! DragAcceptFiles 注册 + 子类化窗口过程接 WM_DROPFILES 兜底——任务栏对
//! 运行中窗口的投递走该消息（窗口已注册 OLE 目标时客户区拖放仍走 winit
//! 原路，两条通道互不重复）。路径进共享队列，UI 每帧排空第一条，与窗口
//! 拖入同一条分流（open_path）。
//!
//! 应用未运行时拖到驻留图标 = Shell 直接带文件参数启动 exe，不经此模块
//! （走 main.rs 的 initial_open 命令行链路）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, DefWindowProcW, SetWindowLongPtrW, GWLP_WNDPROC, WM_DROPFILES, WNDPROC,
};

/// 任务栏拖入路径队列：WM_DROPFILES 在 UI 线程触发，本就与排空方同线程，
/// 队列只消解"消息到达 → 下一帧排空"的时机差（const Mutex::new 已稳定）
static TASKBAR_DROPS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
/// 子类化前的原始窗口过程地址（CallWindowProcW 回放用；0 = 未安装兜底）
static PREV_WNDPROC: AtomicIsize = AtomicIsize::new(0);
/// egui 上下文：收到拖入时唤醒重绘。空闲态无任何排帧源，不唤醒则路径
/// 躺在队列里要等下一次鼠标事件才被消费，表现为"拖了没反应"
static EGUI_CTX: OnceLock<egui::Context> = OnceLock::new();

/// 子类化主窗口：注册接受文件拖放 + 换窗口过程。eframe 只有一个顶层窗口，
/// App::new 仅在启动时调用一次，无重复安装问题
pub fn install(cc: &eframe::CreationContext) {
    let Some(window) = cc.winit_window() else {
        return;
    };
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(h) = handle.as_raw() else {
        return;
    };
    let hwnd: HWND = h.hwnd.get() as HWND;
    // 失败静默：通道是增强特性，拿不到上下文只损失空闲唤醒
    let _ = EGUI_CTX.set(cc.egui_ctx.clone());
    unsafe {
        // 置窗口的 WS_EX_ACCEPTFILES 语义位：Shell（含任务栏投递）据此派发
        // WM_DROPFILES
        DragAcceptFiles(hwnd, 1);
        // 函数指针按地址存进窗口过程槽；返回值为原过程地址
        let prev = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, wnd_proc as *const () as usize as isize);
        PREV_WNDPROC.store(prev, Ordering::Relaxed);
    }
}

/// 每帧排空：返回本帧内拖入的全部路径（调用方取第一条，与窗口拖入同规则）
pub fn drain() -> Vec<PathBuf> {
    TASKBAR_DROPS
        .lock()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default()
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_DROPFILES {
        let hdrop = wparam as windows_sys::Win32::UI::Shell::HDROP;
        // index -1（u32::MAX）= 查询拖入总数；逐项先查路径长度再取宽字符
        let count = DragQueryFileW(hdrop, u32::MAX, std::ptr::null_mut(), 0);
        for i in 0..count {
            let len = DragQueryFileW(hdrop, i, std::ptr::null_mut(), 0);
            if len == 0 {
                continue;
            }
            let mut buf = vec![0u16; len as usize + 1];
            let copied = DragQueryFileW(hdrop, i, buf.as_mut_ptr(), len + 1);
            if copied > 0 {
                let path = String::from_utf16_lossy(&buf[..copied as usize]);
                if let Ok(mut q) = TASKBAR_DROPS.lock() {
                    q.push(PathBuf::from(path));
                }
            }
        }
        DragFinish(hdrop);
        if let Some(ctx) = EGUI_CTX.get() {
            ctx.request_repaint();
        }
        return 0;
    }
    let prev = PREV_WNDPROC.load(Ordering::Relaxed);
    if prev != 0 {
        CallWindowProcW(std::mem::transmute::<isize, WNDPROC>(prev), hwnd, msg, wparam, lparam)
    } else {
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}
