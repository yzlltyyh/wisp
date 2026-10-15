//! 原生窗口的小工具：窗口类注册、建窗、消息循环。本 crate 的每个原生窗口（剪贴板消息窗口、外观监听窗口、浮标）
//! 都跑在各自的专用线程上，窗口过程通过线程局部变量访问状态，空闲时阻塞在 `GetMessageW` 中，零唤醒。

use std::collections::HashSet;
use std::sync::OnceLock;

use ::windows::Win32::Foundation::{ERROR_CLASS_ALREADY_EXISTS, HINSTANCE, HWND};
use ::windows::Win32::System::LibraryLoader::GetModuleHandleW;
use ::windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DispatchMessageW, GetMessageW, HCURSOR, MSG, PM_NOREMOVE, PeekMessageW, RegisterClassExW,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_USER, WNDCLASSEXW, WNDPROC,
};
use ::windows::core::{HRESULT, PCWSTR};
use parking_lot::Mutex;

use super::util::wide;

/// 本模块（exe）的实例句柄
pub(crate) fn hinstance() -> HINSTANCE {
    // SAFETY: 传 None 取当前进程主模块，不增加引用计数
    unsafe { GetModuleHandleW(PCWSTR::null()) }
        .map(|m| HINSTANCE(m.0))
        .unwrap_or_default()
}

/// 注册窗口类（每个类名在进程内只注册一次）；返回是否可用
pub(crate) fn ensure_class(name: &'static str, wndproc: WNDPROC, cursor: Option<HCURSOR>) -> bool {
    static REGISTERED: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let set = REGISTERED.get_or_init(|| Mutex::new(HashSet::new()));
    let mut guard = set.lock();
    if guard.contains(name) {
        return true;
    }
    let class_name = wide(name);
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: wndproc,
        hInstance: hinstance(),
        hCursor: cursor.unwrap_or_default(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    // SAFETY: 结构体字段完整、类名字符串在调用期间存活（系统会复制类名）
    let atom = unsafe { RegisterClassExW(&wc) };
    if atom == 0 {
        // 可能已被同进程的其他代码注册（例如热重载的测试进程），按已存在处理
        let err = ::windows::core::Error::from_thread();
        if err.code() != HRESULT::from_win32(ERROR_CLASS_ALREADY_EXISTS.0) {
            log::warn!("RegisterClassExW({name}) failed: {err}");
            return false;
        }
    }
    guard.insert(name);
    true
}

/// 在当前线程创建窗口
pub(crate) fn create_window(
    class: &'static str,
    ex_style: WINDOW_EX_STYLE,
    style: WINDOW_STYLE,
    parent: Option<HWND>,
) -> ::windows::core::Result<HWND> {
    let class_name = wide(class);
    let title = wide("Wisp");
    // SAFETY: 类已注册；字符串在调用期间存活；窗口归当前线程所有
    unsafe {
        CreateWindowExW(
            ex_style,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            style,
            0,
            0,
            0,
            0,
            parent,
            None,
            Some(hinstance()),
            None,
        )
    }
}

/// 保证当前线程已有消息队列（`PostThreadMessageW` 之前需要）
pub(crate) fn ensure_message_queue() {
    let mut msg = MSG::default();
    // SAFETY: 只查看、不移除消息；作用是让系统为本线程创建消息队列
    unsafe {
        let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
    }
}

/// 运行消息循环直到 `WM_QUIT`。`on_thread_msg` 先处理线程消息（`hwnd` 为空的消息），返回 true 表示已处理
pub(crate) fn run_message_loop(mut on_thread_msg: impl FnMut(&MSG) -> bool) {
    let mut msg = MSG::default();
    loop {
        // SAFETY: 标准消息循环；msg 指向局部变量
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 == 0 || r.0 == -1 {
            break;
        }
        if msg.hwnd.0.is_null() && on_thread_msg(&msg) {
            continue;
        }
        // SAFETY: 消息来自本线程的队列
        unsafe {
            DispatchMessageW(&msg);
        }
    }
}
