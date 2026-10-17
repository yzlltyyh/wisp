//! Windows 原生实现。引用 crate 一律写 `::windows::…`（本模块与 `windows` crate 同名）。
//!
//! ## 面板由本文件直接用 Win32 管理（不走 tao 的显示 / 焦点 API）
//!
//! tao 在内部维护一份窗口标志（VISIBLE、FOCUSABLE、IGNORE_CURSOR_EVENT…），任何一次标志变化都会按这份标志**整体重写**
//! `GWL_EXSTYLE`，并在它认为窗口不可见时 `ShowWindow(SW_HIDE)`；而它的 `show()` 用 `SW_SHOW`（会激活窗口），做不到
//! 面板要求的「显示但不激活」。所以面板（不可缩放、前端也不调用任何会改 tao 标志的窗口 API）全程由这里直接调用
//! Win32：`SetWindowPos(HWND_TOPMOST, SWP_NOACTIVATE | SWP_SHOWWINDOW)` 显示、`SW_HIDE` 隐藏、原地增删
//! `WS_EX_NOACTIVATE` / `WS_EX_TOOLWINDOW` / `WS_EX_TRANSPARENT`。tao 的标志保持创建时的样子，永远不会被触发重写。
//! **Overlay** 同理全程用 Win32 显示 / 隐藏（它的 `WS_EX_TOOLWINDOW` 一旦经 tao 的 show / hide 就会被抹掉、出现在
//! Alt-Tab 里）；面板与 Overlay 的前端都不调用会改 tao 标志的窗口 API（setFocus 等）。
//! **Pinned** 是普通窗口（用户会缩放、最大化，置顶开关也走 tao），它的 tao 标志必须与实际一致，所以显示 / 隐藏仍经 tao
//! （在 UI 线程上同步执行），显示期间先设为不可激活，做到不抢原应用的前台（见 [`show_pinned_without_activation`]）。
//!
//! - **线程**：需要 UI 线程的操作经 `on_main_sync` / `on_main_async` 转过去，HWND 在 UI 线程里现取——tauri 的 `hwnd()`
//!   在其他线程上是一次没有超时的主线程往返，UI 线程卡住时会把调用方一起卡死；
//! - **定位**：先在隐藏状态（或不改可见性）下 `SetWindowPos` 定位，再核对实际矩形（[`place`]）：移到 DPI 不同的显示器时
//!   系统同步发 `WM_DPICHANGED`，tao 会按缩放比例（Win11 直接用系统建议的矩形）改掉刚设好的物理尺寸；
//! - **焦点**：翻译 / OCR / toast 带 `WS_EX_NOACTIVATE`，原应用的标题栏保持激活；输入 / 剪贴板与 `panel_activate`
//!   去掉 NOACTIVATE 并 `SetForegroundWindow`（由低级钩子触发、系统拒绝时用 `AttachThreadInput` 兜底）；
//! - **透明区点击穿透**：面板常驻 `WS_EX_LAYERED`（预热时设好并 `SetLayeredWindowAttributes(255, LWA_ALPHA)`——
//!   分层窗口在设置分层属性之前不显示内容），之后只切换 `WS_EX_TRANSPARENT`。`WH_MOUSE_LL` 装在**专用线程**上
//!   （[`click_through`]，自带消息循环，每次显示重装、隐藏即卸载）：低级钩子经安装线程的消息循环同步回调，装在 UI 线程上
//!   时 UI 线程一卡，全系统的鼠标都跟着卡；
//! - **WebView2**：prepare 前同步 `SetIsVisible(true)` + 内存目标 NORMAL，隐藏后 `SetIsVisible(false)` +
//!   LOW（`advanced.webview_low_memory_when_hidden`）。

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use ::windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use ::windows::Win32::Graphics::Dwm::{
    DWM_WINDOW_CORNER_PREFERENCE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DWMWCP_ROUND, DwmFlush,
    DwmSetWindowAttribute,
};
use ::windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use ::windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetThreadDpiAwarenessContext,
};
use ::windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GWL_EXSTYLE, GWLP_HINSTANCE, GetClassNameW, GetCursorPos, GetForegroundWindow, GetMessageW,
    GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, HC_ACTION, HHOOK, HWND_TOPMOST, IsWindow, LWA_ALPHA,
    MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, PeekMessageW, PostThreadMessageW, SW_HIDE, SWP_FRAMECHANGED, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SetForegroundWindow, SetLayeredWindowAttributes,
    SetWindowLongPtrW, SetWindowPos, SetWindowsHookExW, ShowWindow, UnhookWindowsHookEx, WH_MOUSE_LL, WINDOW_EX_STYLE,
    WM_APP, WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_MOUSEMOVE, WM_RBUTTONDOWN, WM_USER, WS_EX_APPWINDOW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
};
use ::windows::core::Interface as _;
use tauri::{PhysicalPosition, PhysicalSize};
use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL, ICoreWebView2_19,
    ICoreWebView2Controller, ICoreWebView2Settings3,
};
use wisp_types::{FrameMode, Placement, Rect};

use super::{CardSurface, ClickRegion, Fade, ShowAlpha, ShowOpts, ShowTicket, VerticalBound, WidthSwitch};
use crate::error::{IpcError, IpcErrorCode};
use crate::windows::placement::DesktopUnit;

/// 桌面坐标单位：虚拟桌面物理像素
pub const DESKTOP_UNIT: DesktopUnit = DesktopUnit::Physical;
/// Windows 上按需创建 WebView2 控制器要 100–400 ms，默认空闲时预建一个隐藏的主显示器 Overlay
pub const PREWARM_OVERLAY: bool = true;
/// 多屏截图时按键焦点跟随指针：这一端非前台窗口照常收到指针事件，页面在指针进入时调用 `overlay_focus`；
/// 编排器另外保留会话期间每 50 ms 一次的光标轮询作兜底（只在未 commit 的多屏会话里存在），真机确认页面的调用之后
/// 可以改为 true 去掉轮询
pub const OVERLAY_FOLLOWS_POINTER: bool = false;
/// 面板回收（空闲卸载的变体，见 `PanelOptions::recycle_after`）：这一端默认关。macOS 默认开（45 s）是因为
/// WKWebView 显示过之后会不停重试把 backing store 标记为易失；这一端「会话后」的 WebView 内存与唤醒还没有
/// 足够的测量数据（Windows 另有 WebView2 的低内存目标 `webview_low_memory_when_hidden`），默认不回收
pub const PANEL_RECYCLE_AFTER: Option<std::time::Duration> = None;
/// 取不到窗口泄漏检查的计数（`window_census` 只有 macOS 实现）
pub const HAS_WINDOW_CENSUS: bool = false;
/// 输入 / 剪贴板视图激活了面板，隐藏后要把前台还给原窗口
pub const RESTORE_FOCUS_ON_HIDE: bool = true;

/// Pinned 用 DWM 的圆角与阴影（无装饰 + `shadow: true`），不透明，不需要 CSS 阴影边距
pub fn pinned_css_shadow() -> bool {
    false
}

fn platform_err(message: String) -> IpcError {
    IpcError::new(IpcErrorCode::Platform, message)
}

/// 主线程同步调用的最长等待
const MAIN_THREAD_TIMEOUT: Duration = Duration::from_millis(500);

/// 当前面板窗口的 HWND（`panel_attach` 时记下，`panel_destroy` 时清掉）。点击穿透在监听线程上切换，不能为了取句柄
/// 去等 UI 线程
static PANEL_HWND: AtomicIsize = AtomicIsize::new(0);

/// Windows 没有激活策略的概念
pub fn set_regular_activation(app: &tauri::AppHandle, regular: bool) {
    let _ = (app, regular);
}

/// Windows 的模板覆盖：
/// - pinned：无装饰 + `shadow(true)`（DWM 阴影；Win11 的圆角在 [`pinned_attach`] 里设 `DWMWCP_ROUND`）；
/// - panel / overlay：Css 帧模式（Windows 恒为 Css），沿用模板（透明、无阴影、`noRedirectionBitmap`，卡片阴影由 CSS 画）；
/// - settings / onboarding：不透明（首帧背景色见 settings_window.rs）。Win11 的 Mica 需要页面背景透明配合。
pub fn template_overrides<'a, R: tauri::Runtime, M: tauri::Manager<R>>(
    builder: tauri::WebviewWindowBuilder<'a, R, M>,
    kind: super::TemplateKind,
    frame_mode: wisp_types::FrameMode,
) -> tauri::WebviewWindowBuilder<'a, R, M> {
    let _ = frame_mode;
    match kind {
        super::TemplateKind::Pinned => builder.decorations(false).shadow(true),
        // 悬浮剪贴板：外观同 pinned；常驻不可激活（tao 的 FOCUSABLE 关闭 = `WS_EX_NOACTIVATE`），点击条目不夺前台
        super::TemplateKind::Floating => builder.decorations(false).shadow(true).focusable(false),
        super::TemplateKind::Panel | super::TemplateKind::Regular | super::TemplateKind::Overlay => builder,
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 基础设施

/// HWND 不是 Send：跨线程时以整数传递，在目标线程里还原
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RawHwnd(isize);

impl RawHwnd {
    fn hwnd(self) -> HWND {
        HWND(self.0 as *mut core::ffi::c_void)
    }
}

/// 窗口的 HWND。只在 UI 线程上调用（在那里是直接读取；在其他线程上是一次没有超时的主线程往返）
fn hwnd_of(window: &tauri::WebviewWindow) -> Result<HWND, IpcError> {
    Ok(window.hwnd()?)
}

/// 在窗口的 UI 线程执行 `f` 并等待结果；已在 UI 线程时直接执行。HWND 在 UI 线程里取（见模块注释）。超时（UI 线程
/// 卡住）时 `f` 作废，不会稍后再执行（见 [`super::post_and_wait`]）
fn on_main_sync<T: Send + 'static>(
    window: &tauri::WebviewWindow,
    f: impl FnOnce(HWND) -> T + Send + 'static,
) -> Result<T, IpcError> {
    if crate::app::runtime::on_main_thread() {
        return Ok(f(hwnd_of(window)?));
    }
    let w = window.clone();
    super::post_and_wait(
        |job| window.run_on_main_thread(job),
        move || hwnd_of(&w).map(f),
        MAIN_THREAD_TIMEOUT,
    )?
}

/// 在窗口的 UI 线程执行 `f`，不等待
fn on_main_async(window: &tauri::WebviewWindow, f: impl FnOnce(HWND) + Send + 'static) {
    let w = window.clone();
    let posted = window.run_on_main_thread(move || match hwnd_of(&w) {
        Ok(hwnd) => f(hwnd),
        Err(e) => log::warn!("{}: {}", w.label(), e.message),
    });
    if let Err(e) = posted {
        log::warn!("run_on_main_thread failed: {e}");
    }
}

/// 窗口是否属于本进程
fn is_own_window(hwnd: HWND) -> bool {
    let mut pid = 0u32;
    // SAFETY: 只读查询；hwnd 无效时 pid 保持 0
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid != 0 && pid == std::process::id()
}

fn ex_style(hwnd: HWND) -> WINDOW_EX_STYLE {
    // SAFETY: 读窗口扩展样式（直接读窗口数据，跨线程也不发消息）
    WINDOW_EX_STYLE(unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32)
}

/// 原地增删扩展样式（不经 tao 的标志）；`frame_changed` 为 true 时通知系统重算非客户区（改 TOOLWINDOW 需要）。
/// 从其他线程调用时，系统把 `WM_STYLECHANGING` / `WM_STYLECHANGED` 同步发给窗口所在的 UI 线程
fn update_ex_style(hwnd: HWND, add: WINDOW_EX_STYLE, remove: WINDOW_EX_STYLE, frame_changed: bool) {
    let old = ex_style(hwnd);
    let new = WINDOW_EX_STYLE((old.0 | add.0) & !remove.0);
    if new == old {
        return;
    }
    // SAFETY: 写窗口扩展样式；SetWindowPos 只刷新框架，不移动、不改尺寸、不改 Z 序、不激活
    unsafe {
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new.0 as isize);
        if frame_changed {
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
    }
}

fn set_corner_preference(hwnd: HWND, pref: DWM_WINDOW_CORNER_PREFERENCE) {
    // SAFETY: 属性值是 4 字节的 DWM_WINDOW_CORNER_PREFERENCE；Win10 不认识这个属性，返回错误时忽略
    let _ = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            std::ptr::from_ref(&pref).cast(),
            std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        )
    };
}

/// 物理像素矩形 → SetWindowPos 的参数
fn px(rect: Rect) -> (i32, i32, i32, i32) {
    (
        rect.x.round() as i32,
        rect.y.round() as i32,
        rect.width.round().max(1.0) as i32,
        rect.height.round().max(1.0) as i32,
    )
}

fn window_rect(hwnd: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    // SAFETY: 输出参数指向局部变量
    unsafe { GetWindowRect(hwnd, &mut r) }.ok().map(|()| r)
}

/// 把窗口放到 `(x, y, w, h)`（物理像素），不改可见性、不改 Z 序、不激活，然后核对实际矩形。
///
/// 混合 DPI 多屏（例如 100% + 150%）：把窗口移到 DPI 不同的显示器时，系统在这次 `SetWindowPos` 里同步发
/// `WM_DPICHANGED`，tao 的处理按新旧缩放比例重算尺寸（Win11 直接用系统建议的矩形）再 `SetWindowPos` 一次，覆盖掉这里给的
/// 物理尺寸。此时窗口已经在目标屏上、DPI 已经切换，再放一次就是精确的（不会再触发 DPI 变化）。面板与 Overlay 都在隐藏
/// 状态下定位，中间那一步用户看不到
fn place(hwnd: HWND, (x, y, w, h): (i32, i32, i32, i32)) -> Result<(), IpcError> {
    let flags = SWP_NOACTIVATE | SWP_NOZORDER;
    // SAFETY: 标准 Win32 调用
    unsafe { SetWindowPos(hwnd, None, x, y, w, h, flags) }.map_err(|e| platform_err(format!("SetWindowPos: {e}")))?;
    let off = window_rect(hwnd).is_some_and(|r| (r.left, r.top, r.right - r.left, r.bottom - r.top) != (x, y, w, h));
    if off {
        log::debug!("window rect adjusted by the system (DPI change), re-applying");
        // SAFETY: 同上
        unsafe { SetWindowPos(hwnd, None, x, y, w, h, flags) }
            .map_err(|e| platform_err(format!("SetWindowPos: {e}")))?;
    }
    Ok(())
}

/// 不移动、不改尺寸地置顶并显示（不激活）
fn show_topmost(hwnd: HWND) -> Result<(), IpcError> {
    // SAFETY: 标准 Win32 调用
    unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
    }
    .map_err(|e| platform_err(format!("SetWindowPos: {e}")))
}

/// `ShowWindow(SW_HIDE)`（返回值是「之前是否可见」，不是错误）
fn hide_window(hwnd: HWND) {
    // SAFETY: 标准 Win32 调用
    let _was_visible = unsafe { ShowWindow(hwnd, SW_HIDE) };
}

/// 把窗口带到前台：先直接 `SetForegroundWindow`（快捷键 / 点击触发时进程有权这样做）；被系统拒绝（例如由低级钩子
/// 触发的唤出）时临时 `AttachThreadInput` 到当前前台线程再试一次
fn bring_to_foreground(hwnd: HWND) {
    // SAFETY: 标准 Win32 调用；AttachThreadInput 成功后立即解除
    unsafe {
        if SetForegroundWindow(hwnd).as_bool() {
            return;
        }
        let fg = GetForegroundWindow();
        let fg_thread = GetWindowThreadProcessId(fg, None);
        let me = GetCurrentThreadId();
        if fg_thread != 0 && fg_thread != me && AttachThreadInput(me, fg_thread, true).as_bool() {
            let _ = SetForegroundWindow(hwnd);
            let _ = AttachThreadInput(me, fg_thread, false);
        }
    }
}

/// 在 UI 线程拿到 WebView2 控制器执行 `f`，并等待执行完（`with_webview` 在 UI 线程上调用时是同步的）
fn with_controller_sync(window: &tauri::WebviewWindow, f: impl FnOnce(&ICoreWebView2Controller) + Send + 'static) {
    let (tx, rx) = mpsc::sync_channel(1);
    let posted = window.with_webview(move |pw| {
        f(&pw.controller());
        let _ = tx.send(());
    });
    match posted {
        Ok(()) => {
            if rx.recv_timeout(MAIN_THREAD_TIMEOUT).is_err() {
                log::warn!("webview2 controller call timed out");
            }
        }
        Err(e) => log::warn!("with_webview failed: {e}"),
    }
}

/// WebView2 内存目标（`ICoreWebView2_19`，运行时 ≥ 1.0.1774；更老的运行时没有这个接口，静默跳过）
fn set_memory_target(ctrl: &ICoreWebView2Controller, low: bool) {
    // SAFETY: COM 调用在 UI 线程执行（with_webview 的回调）
    let result = unsafe {
        ctrl.CoreWebView2()
            .and_then(|core| core.cast::<ICoreWebView2_19>())
            .and_then(|wv| {
                wv.SetMemoryUsageTargetLevel(if low {
                    COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW
                } else {
                    COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL
                })
            })
    };
    if let Err(e) = result {
        log::debug!("webview2 memory target unavailable: {e}");
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 面板

/// 预热时的面板准备：不进 Alt-Tab（`WS_EX_TOOLWINDOW`，去掉 `WS_EX_APPWINDOW`）、默认不激活（`WS_EX_NOACTIVATE`）、
/// 常驻分层（`WS_EX_LAYERED` + `SetLayeredWindowAttributes(255, LWA_ALPHA)`：分层窗口在设置分层属性之前不显示内容；
/// 点击穿透之后只切换 `WS_EX_TRANSPARENT`，不再反复进出分层模式），不让 DWM 给透明窗口切圆角（圆角与阴影由 CSS 画在
/// 窗口内）。Css 帧模式没有原生表面
pub fn panel_attach(
    window: &tauri::WebviewWindow,
    frame_mode: FrameMode,
    surface: CardSurface,
) -> Result<(), IpcError> {
    let _ = (frame_mode, surface);
    on_main_sync(window, |hwnd| -> Result<(), IpcError> {
        update_ex_style(
            hwnd,
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED,
            WS_EX_APPWINDOW,
            true,
        );
        // SAFETY: 窗口已带 WS_EX_LAYERED；整窗 alpha 255（逐像素透明仍由 WebView2 的合成决定）
        unsafe { SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA) }
            .map_err(|e| platform_err(format!("SetLayeredWindowAttributes: {e}")))?;
        set_corner_preference(hwnd, DWMWCP_DONOTROUND);
        PANEL_HWND.store(hwnd.0 as isize, Ordering::Release);
        Ok(())
    })?
}

/// 外观变化：Css 帧模式的卡片颜色全部来自 CSS 令牌，原生侧无事可做
pub fn panel_set_surface(window: &tauri::WebviewWindow, surface: CardSurface) {
    let _ = (window, surface);
}

/// 立即把面板放到 `rect`（物理像素），并核对系统没有因为 DPI 变化改掉它（[`place`]）
pub fn panel_set_frame(window: &tauri::WebviewWindow, rect: Rect) -> Result<(), IpcError> {
    let r = px(rect);
    on_main_sync(window, move |hwnd| place(hwnd, r))?
}

/// Css 帧模式没有淡出中的窗口
pub fn panel_finish_exit(window: &tauri::WebviewWindow) {
    let _ = window;
}

/// 在 `rect` 处显示面板：先在隐藏状态下定位并核对（[`place`]，混合 DPI 时不会以错误尺寸露出一帧），再
/// `SetWindowPos(HWND_TOPMOST, SWP_NOACTIVATE | SWP_SHOWWINDOW)` 置顶显示，不激活（原应用的标题栏保持激活）。
/// `opts.key` 为 true 时（输入 / 剪贴板视图）随后去掉 NOACTIVATE 并带到前台。alpha 在 Css 帧模式下不适用（进入动画由
/// CSS 播放，超时显示时卡片仍是 opacity 0）。同步：返回时窗口已显示；票已作废时什么都不做
pub fn panel_show(
    window: &tauri::WebviewWindow,
    rect: Rect,
    opts: ShowOpts,
    ticket: &ShowTicket,
) -> Result<(), IpcError> {
    let r = px(rect);
    let ticket = ticket.clone();
    on_main_sync(window, move |hwnd| -> Result<(), IpcError> {
        if !ticket.valid() {
            return Err(ShowTicket::superseded());
        }
        place(hwnd, r)?;
        show_topmost(hwnd)?;
        if opts.key {
            update_ex_style(hwnd, WINDOW_EX_STYLE(0), WS_EX_NOACTIVATE, false);
            bring_to_foreground(hwnd);
        }
        Ok(())
    })?
}

/// 冷唤出的原生外壳只在 Native 帧模式（macOS）下使用；这一端是 Css 帧模式，退回普通显示
pub fn panel_show_shell(
    window: &tauri::WebviewWindow,
    rect: Rect,
    max_height: f64,
    fade: Option<Fade>,
    key: bool,
    on_escape: crate::windows::panel::ShellEscape,
    ticket: &ShowTicket,
) -> Result<(), IpcError> {
    let _ = (max_height, on_escape);
    let opts = ShowOpts {
        alpha: fade.map_or(ShowAlpha::Opaque, ShowAlpha::FadeIn),
        key,
    };
    panel_show(window, rect, opts, ticket)
}

/// 见 [`panel_show_shell`]：Css 帧模式没有外壳，从不被调用
pub fn panel_move_shell(window: &tauri::WebviewWindow, rect: Rect, max_height: Option<f64>, animate: bool) {
    let _ = (window, rect, max_height, animate);
}

/// 见 [`panel_show_shell`]：退回 [`panel_reveal`]
pub fn panel_reveal_shell(
    window: &tauri::WebviewWindow,
    rect: Rect,
    placement: wisp_types::Placement,
    fade: Fade,
    key: bool,
    animate: bool,
    ticket: &ShowTicket,
) -> Result<(), IpcError> {
    let _ = (placement, animate);
    panel_reveal(window, rect, Some(fade), key, ticket)
}

/// 已经在屏幕上的面板（ready 超时后）改到 `rect`；Css 帧模式没有窗口淡入。票已作废时什么都不做
pub fn panel_reveal(
    window: &tauri::WebviewWindow,
    rect: Rect,
    fade: Option<Fade>,
    key: bool,
    ticket: &ShowTicket,
) -> Result<(), IpcError> {
    let _ = fade;
    let r = px(rect);
    let ticket = ticket.clone();
    on_main_sync(window, move |hwnd| -> Result<(), IpcError> {
        if !ticket.valid() {
            return Err(ShowTicket::superseded());
        }
        place(hwnd, r)?;
        if key {
            update_ex_style(hwnd, WINDOW_EX_STYLE(0), WS_EX_NOACTIVATE, false);
            bring_to_foreground(hwnd);
        }
        Ok(())
    })?
}

/// Native 帧模式只在 macOS 使用（frame lab 也只针对 macOS）；这里按固定边立即改高度（`bound` 同 macOS：窗口在边界内时
/// 伸出的部分整体推回），作为正确的退化行为
pub fn panel_animate_height(
    window: &tauri::WebviewWindow,
    height: f64,
    placement: Placement,
    bound: Option<VerticalBound>,
    animate: bool,
    spring: wisp_types::ResizeSpring,
) {
    let _ = (animate, spring);
    on_main_async(window, move |hwnd| {
        let Some(r) = window_rect(hwnd) else { return };
        // SAFETY: 只读查询；失败返回 0
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        let scale = if dpi == 0 { 1.0 } else { f64::from(dpi) / 96.0 };
        let h = (height * scale).round() as i32;
        let mut y = match placement {
            Placement::Above => r.bottom - h,
            Placement::Below | Placement::Center => r.top,
        };
        if let Some(b) = bound {
            let (top, bottom) = (b.top.round() as i32, b.bottom.round() as i32);
            if r.top >= top - 1 && r.bottom <= bottom + 1 {
                y = y.min(bottom - h).max(top);
            }
        }
        if let Err(e) = place(hwnd, (r.left, y, r.right - r.left, h)) {
            log::warn!("panel resize failed: {}", e.message);
        }
    });
}

/// Native 帧模式只在 macOS 使用；这里的高度变化立即到位（没有进行中的弹簧），无事可做
pub fn panel_freeze_height(window: &tauri::WebviewWindow) {
    let _ = window;
}

/// Native 帧模式的宽度切换只在 macOS 使用；这里到时刻后改左边与宽度（高度与顶边不动），作为正确的退化行为
pub fn panel_switch_width(window: &tauri::WebviewWindow, switch: WidthSwitch) {
    let window = window.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(switch.delay).await;
        let (x, _, w, _) = px(Rect::new(switch.x, 0.0, switch.width, 1.0));
        on_main_async(&window, move |hwnd| {
            let Some(r) = window_rect(hwnd) else { return };
            if let Err(e) = place(hwnd, (x, r.top, w, r.bottom - r.top)) {
                log::warn!("panel width switch failed: {}", e.message);
            }
        });
    });
}

/// 隐藏面板（退出动画已由 CSS 播完；粘贴时不播放）；`done` 在隐藏后于 UI 线程调用
pub fn panel_hide(window: &tauri::WebviewWindow, fade: Option<Fade>, done: Box<dyn FnOnce() + Send>) {
    let _ = fade;
    on_main_async(window, move |hwnd| {
        hide_window(hwnd);
        done();
    });
}

/// 显示前设置是否可激活：翻译 / OCR / toast 加 `WS_EX_NOACTIVATE`（点击也不抢前台，直到 `panel_activate`）
pub fn panel_set_focusable(window: &tauri::WebviewWindow, focusable: bool) {
    on_main_async(window, move |hwnd| {
        if focusable {
            update_ex_style(hwnd, WINDOW_EX_STYLE(0), WS_EX_NOACTIVATE, false);
        } else {
            update_ex_style(hwnd, WS_EX_NOACTIVATE, WINDOW_EX_STYLE(0), false);
        }
    });
}

/// 点击本来就不会让带 `WS_EX_NOACTIVATE` 的面板成为前台（OCR 视图在 [`panel_set_focusable`] 里已经带上），不需要按
/// 视图另外设置
pub fn panel_set_key_on_click(window: &tauri::WebviewWindow, key_on_click: bool) {
    let _ = (window, key_on_click);
}

/// 翻译视图被点击后成为前台（去掉 NOACTIVATE 并 `SetForegroundWindow`）
pub fn panel_activate(window: &tauri::WebviewWindow) {
    on_main_async(window, |hwnd| {
        update_ex_style(hwnd, WINDOW_EX_STYLE(0), WS_EX_NOACTIVATE, false);
        bring_to_foreground(hwnd);
    });
}

/// prepare 之前同步恢复 WebView2 的可见性与内存目标（隐藏的 WebView2 会暂停渲染，前端收到 prepare 后
/// 必须能立即同步渲染并测量）。等 UI 线程执行完才返回
pub fn panel_restore_visibility(window: &tauri::WebviewWindow) {
    with_controller_sync(window, |ctrl| {
        // SAFETY: COM 调用在 UI 线程执行
        if let Err(e) = unsafe { ctrl.SetIsVisible(true) } {
            log::warn!("webview2 SetIsVisible(true): {e}");
        }
        set_memory_target(ctrl, false);
    });
}

/// 隐藏后：`SetIsVisible(false)`（停止渲染与计时器节流），按设置把内存目标降为 LOW
pub fn panel_release_visibility(window: &tauri::WebviewWindow, low_memory: bool) {
    let posted = window.with_webview(move |pw| {
        let ctrl = pw.controller();
        // SAFETY: COM 调用在 UI 线程执行
        if let Err(e) = unsafe { ctrl.SetIsVisible(false) } {
            log::warn!("webview2 SetIsVisible(false): {e}");
        }
        if low_memory {
            set_memory_target(&ctrl, true);
        }
    });
    if let Err(e) = posted {
        log::warn!("with_webview failed: {e}");
    }
}

/// Native 帧模式只在 macOS 使用；Css 帧模式的 WebView 随窗口 autoresize
pub fn panel_pin_webview(window: &tauri::WebviewWindow, height: f64) -> Result<(), IpcError> {
    let _ = (window, height);
    Ok(())
}

/// 透明区点击穿透：`Some` 时启用（或更新区域），`None` 时停用并恢复命中测试。不经 UI 线程（见 [`click_through`]）
pub fn panel_set_click_through(window: &tauri::WebviewWindow, region: Option<ClickRegion>) {
    let _ = window;
    let hwnd = PANEL_HWND.load(Ordering::Acquire);
    click_through::set(region.filter(|_| hwnd != 0).map(|r| (RawHwnd(hwnd), r)));
}

/// 透明区点击穿透：`WH_MOUSE_LL` 装在一条专用线程上（自带 GetMessage 循环；第一次启用时启动，之后常驻——没有钩子时
/// 阻塞在 GetMessage 上，零唤醒）。
///
/// - 钩子回调只记下光标位置、比较矩形，需要切换时给本线程投递一条 `WM_APP_APPLY`（同一时刻最多一条）后立即返回；
/// - 本线程处理 `WM_APP_APPLY` 时增删窗口的 `WS_EX_TRANSPARENT`（窗口常驻 `WS_EX_LAYERED`，见 [`panel_attach`]）。跨线程
///   改样式会把 `WM_STYLECHANGING / CHANGED` 同步发给 UI 线程——UI 线程卡住时本线程在这里等，但等待期间系统照样把后续的
///   低级钩子回调送进来处理（它们按非排队消息投递），全系统的鼠标不受影响，只是这块透明区要等 UI 线程恢复后才放行点击；
/// - 每次显示（`None → Some`）都重装钩子（先装新的、再卸旧的）：回调超过 `LowLevelHooksTimeout` 被系统静默卸载过的钩子
///   下一次显示就恢复；隐藏（`Some → None`）卸载钩子并去掉 `WS_EX_TRANSPARENT`。全部操作在本线程上按 FIFO 执行，快速的
///   隐藏 → 显示不会乱序
mod click_through {
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::sync::{Mutex, OnceLock, PoisonError};

    use super::*;

    /// 显示：装（或重装）钩子并按当前光标同步一次
    const WM_APP_ARM: u32 = WM_APP + 1;
    /// 钩子回调发现要切换：按回调记下的光标位置同步
    const WM_APP_APPLY: u32 = WM_APP + 2;
    /// 区域变了（光标可能没动）：按当前光标同步
    const WM_APP_RECHECK: u32 = WM_APP + 3;
    /// 隐藏：卸载钩子，去掉 WS_EX_TRANSPARENT
    const WM_APP_DISARM: u32 = WM_APP + 4;

    /// 当前窗口与区域（None：未启用）。钩子回调只 try_lock，控制方持锁只做几次赋值
    static TARGET: Mutex<Option<(RawHwnd, ClickRegion)>> = Mutex::new(None);
    /// 窗口当前是否带 WS_EX_TRANSPARENT（只由本线程写；钩子回调据此判断要不要投递）
    static APPLIED: AtomicBool = AtomicBool::new(false);
    /// 已投递、还没处理的 WM_APP_APPLY
    static APPLY_POSTED: AtomicBool = AtomicBool::new(false);
    /// 钩子回调最近一次见到的光标位置（物理像素，x 在高 32 位、y 在低 32 位）
    static LAST_POINT: AtomicU64 = AtomicU64::new(0);
    /// 工作线程 id（0 表示没能启动）
    static WORKER: OnceLock<u32> = OnceLock::new();

    fn pack(p: POINT) -> u64 {
        (u64::from(p.x as u32) << 32) | u64::from(p.y as u32)
    }

    fn unpack(v: u64) -> (f64, f64) {
        (f64::from((v >> 32) as u32 as i32), f64::from(v as u32 as i32))
    }

    fn target() -> Option<(RawHwnd, ClickRegion)> {
        *TARGET.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn set(next: Option<(RawHwnd, ClickRegion)>) {
        let msg = {
            let mut t = TARGET.lock().unwrap_or_else(PoisonError::into_inner);
            let prev = std::mem::replace(&mut *t, next);
            match (prev, next) {
                (None, None) => return,
                (None, Some(_)) => WM_APP_ARM,
                (Some(_), None) => WM_APP_DISARM,
                // 换了窗口（面板重建）：按新窗口重装
                (Some((a, _)), Some((b, _))) if a != b => WM_APP_ARM,
                (Some(_), Some(_)) => WM_APP_RECHECK,
            }
        };
        let tid = *WORKER.get_or_init(spawn);
        if tid == 0 {
            return;
        }
        // SAFETY: 向工作线程投递消息（它的消息队列在启动时已建立）
        if let Err(e) = unsafe { PostThreadMessageW(tid, msg, WPARAM(0), LPARAM(0)) } {
            log::warn!("click-through post failed: {e}");
        }
    }

    fn spawn() -> u32 {
        let (tx, rx) = mpsc::sync_channel::<u32>(1);
        let spawned = std::thread::Builder::new()
            .name("wisp-click-through".to_owned())
            .spawn(move || worker_main(&tx));
        if let Err(e) = spawned {
            log::error!("spawn click-through thread failed: {e}");
            return 0;
        }
        rx.recv_timeout(Duration::from_secs(2)).unwrap_or_else(|_| {
            log::error!("click-through thread did not start");
            0
        })
    }

    fn worker_main(ready: &mpsc::SyncSender<u32>) {
        // SAFETY: 本线程的 GetCursorPos 按物理像素给坐标（与钩子坐标、ClickRegion 一致）
        unsafe {
            let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        let mut msg = MSG::default();
        // SAFETY: 建立本线程的消息队列（之后 PostThreadMessage 才投递得进来）
        unsafe {
            let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
        }
        // SAFETY: 纯查询
        let _ = ready.send(unsafe { GetCurrentThreadId() });
        let mut hook: Option<HHOOK> = None;
        // 启用期间的窗口（隐藏时按它恢复命中测试）
        let mut armed: Option<RawHwnd> = None;
        // SAFETY: GetMessageW 写入局部变量；返回 0（WM_QUIT）或 -1（出错）时退出
        while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
            match msg.message {
                WM_APP_ARM => {
                    let next = target().map(|(h, _)| h);
                    if let Some(old) = armed.filter(|old| Some(*old) != next) {
                        apply(old, false);
                    }
                    armed = next;
                    hook = rearm(hook, next);
                    sync(cursor());
                }
                WM_APP_APPLY => {
                    // 先清标志再读位置：处理期间（改样式可能在等 UI 线程）到来的回调会重新投递
                    APPLY_POSTED.store(false, Ordering::Release);
                    sync(Some(unpack(LAST_POINT.load(Ordering::Acquire))));
                }
                WM_APP_RECHECK => sync(cursor()),
                WM_APP_DISARM => {
                    if let Some(h) = hook.take() {
                        unhook(h);
                    }
                    if let Some(h) = armed.take() {
                        apply(h, false);
                    }
                    APPLIED.store(false, Ordering::Release);
                }
                _ => {}
            }
        }
        if let Some(h) = hook.take() {
            unhook(h);
        }
    }

    fn cursor() -> Option<(f64, f64)> {
        let mut p = POINT::default();
        // SAFETY: 输出参数指向局部变量
        unsafe { GetCursorPos(&mut p) }
            .ok()
            .map(|()| (f64::from(p.x), f64::from(p.y)))
    }

    fn unhook(h: HHOOK) {
        // SAFETY: 卸载本线程安装的钩子（已被系统静默卸载过的句柄调用失败，忽略）
        let _ = unsafe { UnhookWindowsHookEx(h) };
    }

    /// 装新钩子、再卸旧的（中间不处理消息，不漏事件）；装不上时保留旧的
    fn rearm(old: Option<HHOOK>, hwnd: Option<RawHwnd>) -> Option<HHOOK> {
        let Some(hwnd) = hwnd else { return old };
        // 用窗口所属模块的实例句柄（tao 注册窗口类时用的就是它）
        // SAFETY: 读窗口的 HINSTANCE
        let hinst = unsafe { GetWindowLongPtrW(hwnd.hwnd(), GWLP_HINSTANCE) };
        let module = ::windows::Win32::Foundation::HINSTANCE(hinst as *mut core::ffi::c_void);
        // SAFETY: 全局低级钩子，回调在本线程的消息循环中执行
        match unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), Some(module), 0) } {
            Ok(new) => {
                if let Some(h) = old {
                    unhook(h);
                }
                Some(new)
            }
            Err(e) => {
                log::warn!("install WH_MOUSE_LL failed: {e}");
                old
            }
        }
    }

    /// 按光标位置 `at` 同步窗口的 WS_EX_TRANSPARENT（读的是窗口的实际样式）
    fn sync(at: Option<(f64, f64)>) {
        let Some((hwnd, region)) = target() else { return };
        let through = at.is_some_and(|(x, y)| region.passes_through(x, y));
        apply(hwnd, through);
    }

    fn apply(hwnd: RawHwnd, through: bool) {
        let h = hwnd.hwnd();
        // SAFETY: 只读查询（窗口已销毁时不再改它）
        if !unsafe { IsWindow(Some(h)) }.as_bool() {
            APPLIED.store(false, Ordering::Release);
            return;
        }
        if through {
            update_ex_style(h, WS_EX_TRANSPARENT, WINDOW_EX_STYLE(0), false);
        } else {
            update_ex_style(h, WINDOW_EX_STYLE(0), WS_EX_TRANSPARENT, false);
        }
        APPLIED.store(through, Ordering::Release);
    }

    unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        let msg = wparam.0 as u32;
        if code == HC_ACTION as i32 && matches!(msg, WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN) {
            // SAFETY: HC_ACTION 时 lparam 指向系统提供的 MSLLHOOKSTRUCT，回调期间有效
            let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
            LAST_POINT.store(pack(info.pt), Ordering::Release);
            // 控制方正持锁更新区域时跳过这一次（它随后会投递 RECHECK）
            let region = TARGET.try_lock().ok().and_then(|t| t.map(|(_, r)| r));
            if let Some(region) = region {
                let through = region.passes_through(f64::from(info.pt.x), f64::from(info.pt.y));
                if through != APPLIED.load(Ordering::Acquire) && !APPLY_POSTED.swap(true, Ordering::AcqRel) {
                    // SAFETY: 给本线程投递消息，不等待
                    let posted =
                        unsafe { PostThreadMessageW(GetCurrentThreadId(), WM_APP_APPLY, WPARAM(0), LPARAM(0)) };
                    if posted.is_err() {
                        APPLY_POSTED.store(false, Ordering::Release);
                    }
                }
            }
        }
        // SAFETY: 把事件交给下一个钩子
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn point_packing_round_trips_negative_coordinates() {
            for (x, y) in [(0, 0), (-1920, 1080), (3839, -200), (i32::MIN, i32::MAX)] {
                assert_eq!(unpack(pack(POINT { x, y })), (f64::from(x), f64::from(y)));
            }
        }
    }
}

/// 逐帧统计只有 macOS 的逐帧驱动才有
pub fn set_frame_stats(enabled: bool) {
    let _ = enabled;
}

/// WebContent 进程 pid 只在 macOS 上可取（frame lab 用）
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn web_content_pid(window: &tauri::WebviewWindow) -> Option<i32> {
    let _ = window;
    None
}

// ---------------------------------------------------------------------------------------------------------------
// 任意窗口的矩形

/// 鼠标左键或右键（交换主次键后拖动用的是右键）此刻是否按着（物理按键，任意线程）：拖动面板时据此区分「用户按住不动」
/// 与「已经松开」
pub fn primary_button_down() -> bool {
    use ::windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON};
    // SAFETY: 纯查询；最高位为 1 表示该键此刻按着
    [VK_LBUTTON, VK_RBUTTON]
        .iter()
        .any(|vk| unsafe { GetAsyncKeyState(i32::from(vk.0)) } < 0)
}

/// 窗口矩形（物理像素）：外框左上角 + 客户区尺寸，与 [`set_window_frame`] 的约定相同（读出来再写回去不会漂移；
/// 无装饰窗口两者相同）
pub fn window_frame(window: &tauri::WebviewWindow) -> Option<Rect> {
    let pos = window.outer_position().ok()?;
    let size = window.inner_size().ok()?;
    Some(Rect::new(
        f64::from(pos.x),
        f64::from(pos.y),
        f64::from(size.width),
        f64::from(size.height),
    ))
}

/// 把窗口放到 `rect`（物理像素：外框左上角 + 客户区尺寸）。用 Tauri 的接口保持 tao 的状态一致；先移动、再改尺寸——
/// 移到 DPI 不同的显示器时被系统按新 DPI 调整过的尺寸由随后的 `set_size` 改回目标值
pub fn set_window_frame(window: &tauri::WebviewWindow, rect: Rect) -> Result<(), IpcError> {
    let (x, y, w, h) = px(rect);
    window.set_position(PhysicalPosition::new(x, y))?;
    window.set_size(PhysicalSize::new(w.max(1) as u32, h.max(1) as u32))?;
    Ok(())
}

/// 截图前隐藏窗口后等 DWM 合成完下一帧（`DwmFlush`），保证冻结帧里没有 Wisp 的窗口
pub fn flush_after_hide() {
    // SAFETY: 阻塞到下一次 DWM present；DWM 关闭时立即返回错误
    let _ = unsafe { DwmFlush() };
}

/// 是否为 macOS 的「重新打开」事件：本平台没有这个事件，恒为 false
pub fn is_reopen(_ev: &tauri::RunEvent) -> bool {
    false
}

/// 当前前台是否落在桌面或任务栏上（或没有前台窗口）。剪贴板 / 输入面板「点击外部」失焦关闭时，点的是桌面或任务栏
/// 空白处就把焦点还给原应用；点中的是别的窗口（含开始菜单、搜索这类外壳弹出界面——用户正要用它们——以及
/// Wisp 自己的 pinned / 设置窗口）则保持不动
pub fn foreground_is_shell() -> bool {
    const SHELL_CLASSES: [&str; 4] = ["Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd"];
    // SAFETY: 只读查询
    let fg = unsafe { GetForegroundWindow() };
    if fg.is_invalid() {
        return true;
    }
    let mut buf = [0u16; 64];
    // SAFETY: 缓冲区长度由切片给出；失败返回 0
    let n = unsafe { GetClassNameW(fg, &mut buf) };
    let Ok(n) = usize::try_from(n) else { return false };
    let class = String::from_utf16_lossy(&buf[..n.min(buf.len())]);
    SHELL_CLASSES.contains(&class.as_str())
}

// ---------------------------------------------------------------------------------------------------------------
// Pinned

/// Pinned 的平台属性：Win11 DWM 圆角（`DWMWCP_ROUND`，Win10 忽略），阴影来自模板覆盖的 `shadow(true)`
pub fn pinned_attach(window: &tauri::WebviewWindow) -> Result<(), IpcError> {
    on_main_sync(window, |hwnd| set_corner_preference(hwnd, DWMWCP_ROUND))
}

/// 不激活地显示 pinned（在 UI 线程上调用）。三端一致：交接、截图后恢复、托盘「显示固定窗口」都不抢原应用的前台，
/// 用户点击 pinned 时它才激活。`restore_minimized` 为 false 时最小化的窗口保持最小化（截图后恢复）。
///
/// pinned 的 tao 标志必须与实际一致（模块注释），所以仍经 tao 的 `show()` / `unminimize()`；它们用 `SW_SHOW` /
/// `SW_RESTORE`，本身会激活窗口，因此显示期间先把 tao 的 FOCUSABLE 关掉（`WS_EX_NOACTIVATE`），显示完再打开。
/// 万一系统仍把前台给了它（不同 Windows 版本对 NOACTIVATE 窗口的激活规则不完全一致），就立即把前台交还给原来的前台
/// 窗口——只在原前台属于别的进程时交还：原前台是 Wisp 自己的面板 / Overlay（用户正在与 Wisp 交互）时，焦点留在 pinned
fn show_pinned_without_activation(
    win: &tauri::WebviewWindow,
    hwnd: HWND,
    restore_minimized: bool,
) -> Result<(), IpcError> {
    let minimized = win.is_minimized()?;
    if minimized && !restore_minimized {
        return Ok(());
    }
    // SAFETY: 只读查询
    let prev = unsafe { GetForegroundWindow() };
    win.set_focusable(false)?;
    let shown = (|| -> tauri::Result<()> {
        if minimized {
            win.unminimize()?;
        }
        win.show()
    })();
    win.set_focusable(true)?;
    shown?;
    // SAFETY: 只读查询
    let now = unsafe { GetForegroundWindow() };
    if now == hwnd && !prev.is_invalid() && prev != hwnd && !is_own_window(prev) {
        // SAFETY: 本进程刚取得前台，有权交还
        let _ = unsafe { SetForegroundWindow(prev) };
    }
    Ok(())
}

/// 固定交接：在同一个 UI 线程回调里把 pinned 放到位（隐藏状态下定位并核对，[`place`]）、不激活地显示，再隐藏面板
/// （不出现空帧）。`expand_to` 为 Some（展开）时直接显示在目标矩形；此时调用方不传 `panel`，面板由 CSS 淡出
/// （fast）后自己隐藏，与 pinned 的淡入 + scale 交叉
pub fn pinned_handoff(
    pinned: &tauri::WebviewWindow,
    panel: Option<&tauri::WebviewWindow>,
    rect: Rect,
    expand_to: Option<Rect>,
    animate: bool,
) -> Result<(), IpcError> {
    let _ = animate;
    let target = px(expand_to.unwrap_or(rect));
    let panel = panel.cloned();
    let pinned_win = pinned.clone();
    on_main_sync(pinned, move |hwnd| -> Result<(), IpcError> {
        // 隐藏状态下定位（不改 tao 的标志）
        place(hwnd, target)?;
        show_pinned_without_activation(&pinned_win, hwnd, true)?;
        if let Some(panel) = panel {
            match hwnd_of(&panel) {
                Ok(p) => hide_window(p),
                Err(e) => log::warn!("hide panel during pin handoff: {}", e.message),
            }
        }
        Ok(())
    })?
}

/// 不激活地把 pinned 放到最前（截图后恢复、托盘「显示固定窗口」），见 [`show_pinned_without_activation`]
pub fn pinned_show(window: &tauri::WebviewWindow, restore_minimized: bool) -> Result<(), IpcError> {
    let w = window.clone();
    on_main_sync(window, move |hwnd| {
        show_pinned_without_activation(&w, hwnd, restore_minimized)
    })?
}

/// 立即隐藏 pinned（截图前，不播放动画）。在 UI 线程上经 tao 同步执行（tao 的标志随之更新，之后的 `show()` 才会真的
/// 显示），返回时窗口已经离屏；随后的 `flush_after_hide` 等 DWM 合成完这一帧
pub fn pinned_hide(window: &tauri::WebviewWindow) -> Result<(), IpcError> {
    let w = window.clone();
    Ok(on_main_sync(window, move |_| w.hide())??)
}

// ---------------------------------------------------------------------------------------------------------------
// 悬浮剪贴板
//
// 窗口由 tao 管理（用户会拖动、缩放，置顶开关也走 tao），不可激活经 tao 的 FOCUSABLE 标志表达（`WS_EX_NOACTIVATE`）：
// 直接改 GWL_EXSTYLE 会在 tao 下一次按自己的标志重写样式时被抹掉。

/// 悬浮剪贴板的平台属性：Win11 DWM 圆角；不可激活来自模板覆盖的 `focusable(false)`，置顶来自创建参数
pub fn float_attach(window: &tauri::WebviewWindow, on_top: bool) -> Result<(), IpcError> {
    let _ = on_top;
    on_main_sync(window, |hwnd| set_corner_preference(hwnd, DWMWCP_ROUND))
}

/// 置顶 / 取消置顶
pub fn float_set_on_top(window: &tauri::WebviewWindow, on_top: bool) -> Result<(), IpcError> {
    Ok(window.set_always_on_top(on_top)?)
}

/// 搜索框要打字：临时可激活并取得前台；`false` 时恢复不可激活（只改样式，不交出前台：悬浮窗仍在前台时由
/// `WindowManager::float_focus` 把前台还给打字之前的应用，粘贴时由 `paste_into` 交还目标应用）
pub fn float_set_focusable(window: &tauri::WebviewWindow, focusable: bool) -> Result<(), IpcError> {
    let w = window.clone();
    on_main_sync(window, move |hwnd| -> Result<(), IpcError> {
        w.set_focusable(focusable)?;
        if focusable {
            bring_to_foreground(hwnd);
        }
        Ok(())
    })?
}

/// 发粘贴键之前：悬浮窗在前台（搜索框打过字）时恢复不可激活，前台的交还由随后的 `paste_into` 完成（它按记录的目标窗口
/// 激活并等它到前台）；不在前台时什么都不用做。总是返回 true
pub fn float_yield(window: &tauri::WebviewWindow, target: &wisp_types::AppRef) -> bool {
    let _ = target;
    let w = window.clone();
    let result = on_main_sync(window, move |hwnd| {
        // SAFETY: 只读查询
        if unsafe { GetForegroundWindow() } == hwnd
            && let Err(e) = w.set_focusable(false)
        {
            log::warn!("floating clipboard set_focusable(false) failed: {e}");
        }
    });
    if let Err(e) = result {
        log::warn!("floating clipboard yield: {}", e.message);
    }
    true
}

/// 不激活地显示（在 UI 线程上调用）：窗口本来就不可激活（`WS_EX_NOACTIVATE`）；万一系统仍把前台给了它（不同 Windows
/// 版本对 NOACTIVATE 窗口的激活规则不完全一致），立即交还给原来的前台窗口（与 pinned 的做法相同）。最小化的还原
fn show_float_without_activation(win: &tauri::WebviewWindow, hwnd: HWND) -> Result<(), IpcError> {
    // SAFETY: 只读查询
    let prev = unsafe { GetForegroundWindow() };
    if win.is_minimized()? {
        win.unminimize()?;
    }
    win.show()?;
    // SAFETY: 只读查询
    let now = unsafe { GetForegroundWindow() };
    if now == hwnd && !prev.is_invalid() && prev != hwnd && !is_own_window(prev) {
        // SAFETY: 本进程刚取得前台，有权交还
        let _ = unsafe { SetForegroundWindow(prev) };
    }
    Ok(())
}

/// 交接：同一个 UI 线程回调里把悬浮窗放到位（隐藏状态下定位并核对，[`place`]）、不激活地显示，再隐藏面板（不出现空帧）
pub fn float_handoff(
    float: &tauri::WebviewWindow,
    panel: Option<&tauri::WebviewWindow>,
    rect: Rect,
) -> Result<(), IpcError> {
    let target = px(rect);
    let panel = panel.cloned();
    let float_win = float.clone();
    on_main_sync(float, move |hwnd| -> Result<(), IpcError> {
        place(hwnd, target)?;
        show_float_without_activation(&float_win, hwnd)?;
        if let Some(panel) = panel {
            match hwnd_of(&panel) {
                Ok(p) => hide_window(p),
                Err(e) => log::warn!("hide panel during float handoff: {}", e.message),
            }
        }
        Ok(())
    })?
}

/// 不激活地显示（截图后恢复、再次打开已存在的悬浮窗）
pub fn float_show(window: &tauri::WebviewWindow) -> Result<(), IpcError> {
    let w = window.clone();
    on_main_sync(window, move |hwnd| show_float_without_activation(&w, hwnd))?
}

/// 立即隐藏（截图前）：经 tao 同步执行，之后的 `show()` 才会真的显示
pub fn float_hide(window: &tauri::WebviewWindow) -> Result<(), IpcError> {
    pinned_hide(window)
}

// ---------------------------------------------------------------------------------------------------------------
// Overlay

/// Overlay 的平台属性：不进 Alt-Tab（`WS_EX_TOOLWINDOW`），不让 DWM 切圆角；置顶来自模板的 `alwaysOnTop`（`HWND_TOPMOST`）。
/// 之后的显示 / 隐藏全程用 Win32（[`overlay_show`] / [`overlay_hide`]），tao 的标志保持创建时的样子，不会把 TOOLWINDOW 抹掉
pub fn overlay_attach(window: &tauri::WebviewWindow) -> Result<(), IpcError> {
    on_main_sync(window, |hwnd| {
        update_ex_style(hwnd, WS_EX_TOOLWINDOW, WS_EX_APPWINDOW, true);
        set_corner_preference(hwnd, DWMWCP_DONOTROUND);
    })
}

/// Windows 的冻结帧由 WebView canvas 画（先 1/2 降采样帧，再全分辨率）
pub fn overlay_set_frozen(window: &tauri::WebviewWindow, frame: &wisp_capture::Frame) -> bool {
    let _ = (window, frame);
    false
}

/// 显示 Overlay（覆盖 `rect` = 显示器完整区域，含任务栏）：先在隐藏状态下定位并核对（预建的 Overlay 在主屏上，换到
/// DPI 不同的显示器时尺寸会被系统改掉，见 [`place`]），再 `SetWindowPos(HWND_TOPMOST, SWP_SHOWWINDOW | SWP_NOACTIVATE)`
/// 置顶显示（不经 tao，见模块注释）。`key` 为 true 时（光标所在显示器）再带到前台接收键盘（结束时由编排器
/// 交还焦点）；其他显示器上的 Overlay 只显示、不激活
pub fn overlay_show(window: &tauri::WebviewWindow, rect: Rect, key: bool) -> Result<(), IpcError> {
    let r = px(rect);
    on_main_sync(window, move |hwnd| -> Result<(), IpcError> {
        place(hwnd, r)?;
        show_topmost(hwnd)?;
        if key {
            bring_to_foreground(hwnd);
        }
        Ok(())
    })?
}

/// 多屏截图时让光标所在屏的 Overlay 接收键盘：与显示时的 key 一样 `SetForegroundWindow`
pub fn overlay_focus(window: &tauri::WebviewWindow) {
    on_main_async(window, bring_to_foreground);
}

/// 把 Overlay 重新压到置顶层的最上面（面板在它之后以 `HWND_TOPMOST` 显示时），不激活
pub fn overlay_raise(window: &tauri::WebviewWindow) {
    on_main_async(window, |hwnd| {
        // SAFETY: 标准 Win32 调用
        let _ = unsafe {
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
        };
    });
}

/// 隐藏 Overlay（复用的预建窗口）：`ShowWindow(SW_HIDE)`，不经 tao（见模块注释）
pub fn overlay_hide(window: &tauri::WebviewWindow) {
    on_main_async(window, hide_window);
}

/// 销毁面板窗口（空闲卸载、失效重建）。点击穿透已先停用（监听线程按 FIFO 执行），这里清掉记下的句柄
pub fn panel_destroy(window: &tauri::WebviewWindow) {
    super::note_destroying(window.label());
    PANEL_HWND.store(0, Ordering::Release);
    if let Err(e) = window.destroy() {
        log::warn!("destroy {} failed: {e}", window.label());
    }
}

/// 销毁 Overlay
pub fn overlay_destroy(window: &tauri::WebviewWindow) {
    super::note_destroying(window.label());
    if let Err(e) = window.destroy() {
        log::warn!("destroy {} failed: {e}", window.label());
    }
}

/// commit 之后的正常收尾：这一端的冻结帧画在页面里，页面自己已经淡出，直接销毁（macOS 的原生冻结帧要先淡出窗口）
pub fn overlay_destroy_fading(window: &tauri::WebviewWindow, fade: super::Fade) {
    let _ = fade;
    overlay_destroy(window);
}

// ---------------------------------------------------------------------------------------------------------------
// 窗口生命周期与诊断（macOS 有逐帧驱动与 CGWindowList，这里只有占位实现）

/// 销毁一个窗口（Pinned / 设置 / 引导；面板与 Overlay 另有 `panel_destroy` / `overlay_destroy`）。macOS 要在主线程的
/// autorelease pool 里销毁（见 macos.rs），这里直接销毁
pub fn destroy_window(window: &tauri::WebviewWindow) {
    super::note_destroying(window.label());
    if let Err(e) = window.destroy() {
        log::warn!("destroy {} failed: {e}", window.label());
    }
}

/// 窗口创建之后：tao 多出的引用只有 macOS 有（见 macos.rs 的 `adopt_window`）。这里关掉 WebView2 的**浏览器**加速键：
/// wry 的 `browser_accelerator_keys` 默认开、tauri-runtime-wry 没有覆盖，面板 / Pinned /
/// 设置里没被视图吞掉的 F5、Ctrl+R（重载——面板会话随之结束、Pinned 重新 pinned_init）、Ctrl+P（打印预览）、Ctrl+F / F3
/// （查找）、Ctrl+± / Ctrl+0（缩放）都会落到 WebView2。`AreBrowserAcceleratorKeysEnabled = false` 不影响编辑键
/// （Ctrl+C / V / X / A / Z、Home / End 等）。异步投递到 UI 线程，不等。debug 构建设 `WISP_WEBVIEW_DEVKEYS=1` 可保留
/// （F12 开发者工具）
pub fn adopt_window(window: &tauri::WebviewWindow) {
    if cfg!(debug_assertions) && std::env::var_os("WISP_WEBVIEW_DEVKEYS").is_some() {
        return;
    }
    let posted = window.with_webview(|pw| {
        let ctrl = pw.controller();
        // SAFETY: COM 调用在 UI 线程执行（with_webview 的回调）
        let result = unsafe {
            ctrl.CoreWebView2()
                .and_then(|core| core.Settings())
                .and_then(|settings| settings.cast::<ICoreWebView2Settings3>())
                .and_then(|settings| settings.SetAreBrowserAcceleratorKeysEnabled(false))
        };
        if let Err(e) = result {
            log::debug!("webview2 browser accelerator keys stay enabled: {e}");
        }
    });
    if let Err(e) = posted {
        log::warn!("with_webview failed: {e}");
    }
}

/// 窗口销毁之后：没有逐帧驱动需要释放（macOS 释放离屏窗口的 displayLink）
pub fn release_offscreen_drivers(app: &tauri::AppHandle) {
    let _ = app;
}

/// 诊断：窗口是否可见
pub fn describe_window(window: &tauri::WebviewWindow) -> String {
    format!("visible={:?}", window.is_visible().ok())
}

/// 诊断（窗口泄漏检查）：只有 macOS 实现（`NSApp.windows` 与 CGWindowList），这里返回 None
pub fn window_census(app: &tauri::AppHandle) -> Option<super::WindowCensus> {
    let _ = app;
    None
}

/// 有没有 Wisp 窗口正在跑原生逐帧动画：这里的几何变化都是一次到位（Css 帧模式），没有原生动画
pub fn animating() -> bool {
    false
}

/// 托盘图标的无障碍名称：Windows / Linux 的托盘以提示文字（tooltip）作为无障碍名称，不需要另外设置
pub fn tray_set_accessibility(tray: &tauri::tray::TrayIcon, label: &str) {
    let _ = (tray, label);
}

/// 诊断（window lab）：系统层面 Wisp 是否在前台（只有 macOS 能取到）
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn app_is_active(app: &tauri::AppHandle) -> Option<bool> {
    let _ = app;
    None
}

/// 诊断（window lab 的 `WISP_WINDOW_LAB_DOCK_ICON`）：Dock 图标只有 macOS 有（见 macos.rs）
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_app_icon(app: &tauri::AppHandle, dump: Option<std::path::PathBuf>) -> Option<(bool, String)> {
    let _ = (app, dump);
    None
}

/// 诊断（window lab）：记下 / 交还前台——只有 macOS 需要（见 macos.rs）
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_remember_frontmost() {}

/// 诊断（window lab）：见 [`lab_remember_frontmost`]
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_resign_active(app: &tauri::AppHandle) {
    let _ = app;
}

/// 截图会话 commit 之后停止跟随指针：这一端由编排器的光标轮询 / 页面的 `overlay_focus` 负责（两者都只在未 commit 时
/// 生效），原生层没有跟随，空操作
pub fn overlay_stop_following(window: &tauri::WebviewWindow) {
    let _ = window;
}

/// 诊断（window lab）：原生层不跟随指针（见 [`overlay_stop_following`]）
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn overlay_follows_pointer_now(window: &tauri::WebviewWindow) -> Option<bool> {
    let _ = window;
    None
}

/// 诊断（window lab 的 `cold_show_keys`）：冷唤出外壳只有 macOS 有
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_shell_key_stats() -> (usize, usize) {
    (0, 0)
}

/// 诊断（window lab 的 `cold_show_keys`）：只有 macOS 能取到
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_panel_focus_state(window: &tauri::WebviewWindow) -> Option<(bool, bool, bool)> {
    let _ = window;
    None
}

/// 诊断（window lab 的 `cold_show_keys`）：只有 macOS 有
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_post_text(window: &tauri::WebviewWindow, text: &str) -> Result<usize, IpcError> {
    let _ = (window, text);
    Err(IpcError::internal("lab_post_text is macOS only"))
}

/// 诊断（window lab 的 `cold_show_keys`）：只有 macOS 能取到
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_webview_title(window: &tauri::WebviewWindow) -> Option<String> {
    let _ = window;
    None
}

/// 诊断（window lab）：WebKit 辅助进程的采样只有 macOS 有
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_webkit_processes(app: &tauri::AppHandle) -> Vec<super::LabProcess> {
    let _ = app;
    Vec::new()
}

/// 诊断（window lab）：进程的内存构成（`footprint` 工具）只有 macOS 有
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_footprint_categories(pid: i32, top: usize) -> Vec<(String, u64)> {
    let _ = (pid, top);
    Vec::new()
}

/// 诊断（window lab）：malloc 统计只有 macOS 取
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_malloc_in_use() -> Option<u64> {
    None
}

/// 诊断（window lab）：phys_footprint 的生命期最大值只有 macOS 取
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_lifetime_max_footprint_kib() -> Option<u64> {
    None
}

/// window lab：原生逐帧动画只有 macOS 有（Css 帧模式的卡片动画在页面里）
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_take_frame_trace() -> super::LabFrameTrace {
    super::LabFrameTrace::default()
}

/// window lab：原生高度弹簧只有 macOS 有
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_take_height_traces() -> Vec<super::LabHeightTrace> {
    Vec::new()
}

/// 诊断（window lab）：负载只在 macOS 上记录
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_load_average() -> Option<f64> {
    None
}

/// 诊断（window lab 的 `overlay_keys`）：按键探针只有 macOS 有（见 macos.rs）
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_key_probe(app: &tauri::AppHandle, on: bool) -> Vec<(u16, bool)> {
    let _ = (app, on);
    Vec::new()
}

/// 诊断（window lab）：Overlay 的焦点状态（只有 macOS 能取到）
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn overlay_focus_state(window: &tauri::WebviewWindow) -> Option<(bool, bool, bool)> {
    let _ = window;
    None
}

/// 设置窗口不进入全屏（只有 macOS 有全屏按钮，见 macos.rs）
pub fn settings_attach(window: &tauri::WebviewWindow) {
    let _ = window;
}

/// 托盘按钮的无障碍「按下」（只有 macOS 需要补，见 macos.rs）
pub fn tray_enable_press(tray: &tauri::tray::TrayIcon, on_press: impl Fn() + Send + Sync + 'static) {
    let _ = (tray, on_press);
}

/// 应用菜单的「窗口」子菜单（只有 macOS 有应用菜单）
pub fn mark_windows_menu(menu: &tauri::menu::Submenu<tauri::Wry>) {
    let _ = menu;
}
