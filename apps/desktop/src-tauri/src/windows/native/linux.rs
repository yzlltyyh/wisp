//! Linux 原生实现：GTK / X11 窗口经 Tauri 的跨平台 API 操作（tao 在 Linux 上直接驱动 GTK，没有 Windows 那种
//! 「标志整体重写样式」的问题）；坐标用物理像素（混合缩放的多屏下按显示器换算，见 placement.rs）。
//!
//! - **同步显示 / 隐藏**：tao 把 move / resize / show / hide 投递到 glib 通道，下一次主循环迭代才
//!   执行，调用返回时窗口还没变。需要「返回时已经显示 / 已经离屏」的地方（面板 show / hide、固定交接、Overlay）随后
//!   轮询 `is_visible` 直到状态到位（[`wait_visible`]，每次轮询都是一次排在这些请求之后的主线程往返）；
//!
//! - **焦点**：翻译 / OCR / toast `focusable(false)`，点击后 `panel_activate` 改为 `focusable(true)` 并 `set_focus`；
//!   输入 / 剪贴板显示时直接 `set_focus`；
//! - **透明与不透明**：`preflight_env` 判定不能透明（Wayland、关闭了合成）时模板覆盖为
//!   `.transparent(false)`，`Boot.shadow_margin` 为 0，卡片铺满窗口；
//! - **透明区点击穿透**：GTK 的输入区域只能整窗切换（tao 的 `set_ignore_cursor_events` = 空输入形状 / 恢复），
//!   取得窗口 XID 需要 gtk / raw-window-handle 依赖（本 crate 没有）。所以面板可见且有透明
//!   富余区时，用一个 30 Hz 的轮询线程经 X11 `QueryPointer` 读光标，光标落在窗口内、卡片外时整窗忽略鼠标，回到卡片上
//!   时恢复；面板隐藏即停止轮询（空闲零唤醒）。轮询线程与主路径按代号互斥（见 [`ClickPoller`]），旧线程不会在主路径
//!   恢复命中测试之后再把它关掉，线程意外退出后下一次显示会重新启动。

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{PhysicalPosition, PhysicalSize};
use wisp_types::{FrameMode, Placement, Rect};

use super::{CardSurface, ClickRegion, Fade, ShowAlpha, ShowOpts, ShowTicket, VerticalBound, WidthSwitch};
use crate::error::IpcError;
use crate::windows::placement::DesktopUnit;

/// 桌面坐标单位：虚拟桌面物理像素
pub const DESKTOP_UNIT: DesktopUnit = DesktopUnit::Physical;
/// Linux 不预建 Overlay
pub const PREWARM_OVERLAY: bool = false;
/// 多屏截图时按键焦点跟随指针：这一端非前台窗口照常收到指针事件，页面在指针进入时调用 `overlay_focus`；
/// 编排器另外保留会话期间每 50 ms 一次的光标轮询作兜底（只在未 commit 的多屏会话里存在），真机确认页面的调用之后
/// 可以改为 true 去掉轮询
pub const OVERLAY_FOLLOWS_POINTER: bool = false;
/// 面板回收（空闲卸载的变体，见 `PanelOptions::recycle_after`）：这一端默认关。macOS 默认开（45 s）是因为
/// WKWebView 显示过之后会不停重试把 backing store 标记为易失；这一端「会话后」的 WebView 内存与唤醒还没有
/// 足够的测量数据，默认不回收
pub const PANEL_RECYCLE_AFTER: Option<std::time::Duration> = None;
/// 取不到窗口泄漏检查的计数（`window_census` 只有 macOS 实现）
pub const HAS_WINDOW_CENSUS: bool = false;
/// 输入 / 剪贴板视图取得了键盘焦点，隐藏后由平台层 `_NET_ACTIVE_WINDOW` 把焦点还给原窗口
pub const RESTORE_FOCUS_ON_HIDE: bool = true;

/// Pinned 无装饰：能透明时由 CSS 画圆角与阴影（boot 带阴影边距），不能透明时铺满窗口
pub fn pinned_css_shadow() -> bool {
    crate::app::runtime::transparent_windows()
}

/// 点击穿透轮询的间隔（30 Hz，与「鼠标离开 N px」的轮询一致）
const CLICK_THROUGH_POLL: Duration = Duration::from_millis(33);

/// 截图前隐藏窗口后等合成器出完一两帧（X11 合成器重绘根窗口的延迟）
const COMPOSITOR_SETTLE: Duration = Duration::from_millis(34);

/// 等主循环执行完已投递的显示 / 隐藏的上限（主循环卡住时宁可放弃等待，也不无限阻塞调用方）
const MAIN_LOOP_WAIT: Duration = Duration::from_millis(500);

/// pinned 不激活地显示后，过这么久再恢复可获得焦点：窗口管理器在处理映射（MapRequest）时读 WM_HINTS 决定要不要
/// 给它焦点，那一步在 X 服务器上异步发生，恢复得太早会被它读到
const PINNED_FOCUSABLE_RESTORE: Duration = Duration::from_millis(300);

/// Linux 没有激活策略的概念
pub fn set_regular_activation(app: &tauri::AppHandle, regular: bool) {
    let _ = (app, regular);
}

/// Linux 的模板覆盖。透明与否取 `preflight_env` 的结论（`app::runtime::transparent_windows()`）：
/// - 可以透明（X11 且合成可用）：pinned 无装饰 + 透明（圆角与阴影由 CSS 画）；panel / overlay 沿用模板（透明）；
/// - 不能透明（Wayland、或设置了 `WEBKIT_DISABLE_COMPOSITING_MODE`）：panel / pinned / overlay 一律 `.transparent(false)`，
///   此时 `Boot.shadow_margin` 为 0，卡片铺满窗口、纯色、直角（前端按 shadow_margin == 0 渲染）；
/// - settings / onboarding 沿用模板。
pub fn template_overrides<'a, R: tauri::Runtime, M: tauri::Manager<R>>(
    builder: tauri::WebviewWindowBuilder<'a, R, M>,
    kind: super::TemplateKind,
    frame_mode: wisp_types::FrameMode,
) -> tauri::WebviewWindowBuilder<'a, R, M> {
    let _ = frame_mode;
    let transparent = crate::app::runtime::transparent_windows();
    match kind {
        super::TemplateKind::Pinned => builder.decorations(false).transparent(transparent),
        super::TemplateKind::Panel | super::TemplateKind::Overlay if !transparent => builder.transparent(false),
        super::TemplateKind::Panel | super::TemplateKind::Regular | super::TemplateKind::Overlay => builder,
    }
}

/// 等 GTK 主循环执行完已投递的 show / hide，直到窗口的可见状态等于 `visible`（见模块注释）。在主线程上无法等待
/// （等待会卡住主循环本身），直接返回 false；超时也返回 false 并记日志
fn wait_visible(window: &tauri::WebviewWindow, visible: bool) -> bool {
    if crate::app::runtime::on_main_thread() {
        return false;
    }
    let deadline = Instant::now() + MAIN_LOOP_WAIT;
    loop {
        match window.is_visible() {
            Ok(v) if v == visible => return true,
            Ok(_) => {}
            Err(e) => {
                log::warn!("{} is_visible failed: {e}", window.label());
                return false;
            }
        }
        if Instant::now() >= deadline {
            log::warn!(
                "{} did not become visible={visible} within {} ms",
                window.label(),
                MAIN_LOOP_WAIT.as_millis()
            );
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn set_frame(window: &tauri::WebviewWindow, rect: Rect) -> Result<(), IpcError> {
    window.set_position(PhysicalPosition::new(rect.x.round() as i32, rect.y.round() as i32))?;
    window.set_size(PhysicalSize::new(
        rect.width.round().max(1.0) as u32,
        rect.height.round().max(1.0) as u32,
    ))?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// 面板

/// 预热时的面板准备：Css 帧模式没有原生表面；不进任务栏来自模板的 `skipTaskbar`
pub fn panel_attach(
    window: &tauri::WebviewWindow,
    frame_mode: FrameMode,
    surface: CardSurface,
) -> Result<(), IpcError> {
    let _ = (frame_mode, surface);
    window.set_focusable(false)?;
    Ok(())
}

/// 外观变化：Css 帧模式的卡片颜色全部来自 CSS 令牌，原生侧无事可做
pub fn panel_set_surface(window: &tauri::WebviewWindow, surface: CardSurface) {
    let _ = (window, surface);
}

/// 立即把面板放到 `rect`（物理像素）
pub fn panel_set_frame(window: &tauri::WebviewWindow, rect: Rect) -> Result<(), IpcError> {
    set_frame(window, rect)
}

/// Css 帧模式没有淡出中的窗口
pub fn panel_finish_exit(window: &tauri::WebviewWindow) {
    let _ = window;
}

/// 在主循环上执行 `f` 并等它返回（已在主线程时直接执行）；主循环卡住超过 [`MAIN_LOOP_WAIT`] 时 `f` 作废
fn on_main_sync<T: Send + 'static>(
    window: &tauri::WebviewWindow,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, IpcError> {
    if crate::app::runtime::on_main_thread() {
        return Ok(f());
    }
    super::post_and_wait(|job| window.run_on_main_thread(job), f, MAIN_LOOP_WAIT)
}

/// 在 `rect` 处显示面板（Css 帧模式的进入动画由 CSS 播放，alpha 不适用）；`opts.key` 为 true 时取得键盘焦点。
/// 定位与显示在主循环上执行，执行前核对票（期间开始的隐藏让这次显示作废，见 [`ShowTicket`]）。
/// 同步：等主循环真正执行完 show 再返回（`panel_ready` 返回时窗口必须已经显示）；tao 的 `set_focus` 只对已经可见的窗口生效，所以也要等到这之后
pub fn panel_show(
    window: &tauri::WebviewWindow,
    rect: Rect,
    opts: ShowOpts,
    ticket: &ShowTicket,
) -> Result<(), IpcError> {
    let ticket = ticket.clone();
    let w = window.clone();
    on_main_sync(window, move || -> Result<(), IpcError> {
        if !ticket.valid() {
            return Err(ShowTicket::superseded());
        }
        set_frame(&w, rect)?;
        w.show()?;
        Ok(())
    })??;
    wait_visible(window, true);
    if opts.key {
        window.set_focusable(true)?;
        window.set_focus()?;
    }
    Ok(())
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

/// 已经在屏幕上的面板（ready 超时后）改到 `rect`；票已作废时什么都不做
pub fn panel_reveal(
    window: &tauri::WebviewWindow,
    rect: Rect,
    fade: Option<Fade>,
    key: bool,
    ticket: &ShowTicket,
) -> Result<(), IpcError> {
    let _ = fade;
    let ticket = ticket.clone();
    let w = window.clone();
    on_main_sync(window, move || -> Result<(), IpcError> {
        if !ticket.valid() {
            return Err(ShowTicket::superseded());
        }
        set_frame(&w, rect)
    })??;
    if key {
        panel_activate(window);
    }
    Ok(())
}

/// Native 帧模式只在 macOS 使用；这里按固定边立即改高度（`bound` 同 macOS：窗口在边界内时伸出的部分整体推回），作为
/// 正确的退化行为
pub fn panel_animate_height(
    window: &tauri::WebviewWindow,
    height: f64,
    placement: Placement,
    bound: Option<VerticalBound>,
    animate: bool,
    spring: wisp_types::ResizeSpring,
) {
    let _ = (animate, spring);
    let resize = || -> tauri::Result<()> {
        let scale = window.scale_factor()?;
        let pos = window.outer_position()?;
        let size = window.outer_size()?;
        let h = (height * scale).round().max(1.0) as u32;
        let mut y = match placement {
            Placement::Above => pos.y + size.height as i32 - h as i32,
            Placement::Below | Placement::Center => pos.y,
        };
        if let Some(b) = bound {
            let (top, bottom) = (b.top.round() as i32, b.bottom.round() as i32);
            if pos.y >= top - 1 && pos.y + size.height as i32 <= bottom + 1 {
                y = y.min(bottom - h as i32).max(top);
            }
        }
        window.set_position(PhysicalPosition::new(pos.x, y))?;
        window.set_size(PhysicalSize::new(size.width, h))
    };
    if let Err(e) = resize() {
        log::warn!("panel resize failed: {e}");
    }
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
        let resize = || -> tauri::Result<()> {
            let pos = window.outer_position()?;
            let size = window.outer_size()?;
            window.set_position(PhysicalPosition::new(switch.x.round() as i32, pos.y))?;
            window.set_size(PhysicalSize::new(switch.width.round().max(1.0) as u32, size.height))
        };
        if let Err(e) = resize() {
            log::warn!("panel width switch failed: {e}");
        }
    });
}

/// 隐藏面板（退出动画已由 CSS 播完）；`done` 在主循环真正执行完 hide 之后调用（截图前隐藏要等它离屏）。
/// 等待放在阻塞线程池里，调用方（可能就是主线程）不被卡住
pub fn panel_hide(window: &tauri::WebviewWindow, fade: Option<Fade>, done: Box<dyn FnOnce() + Send>) {
    let _ = fade;
    if let Err(e) = window.hide() {
        log::warn!("hide panel failed: {e}");
        done();
        return;
    }
    let window = window.clone();
    tauri::async_runtime::spawn_blocking(move || {
        wait_visible(&window, false);
        done();
    });
}

/// 显示前设置是否可获得焦点（翻译 / OCR / toast 为 false：WM 不把焦点给它）
pub fn panel_set_focusable(window: &tauri::WebviewWindow, focusable: bool) {
    if let Err(e) = window.set_focusable(focusable) {
        log::warn!("set_focusable failed: {e}");
    }
}

/// 翻译视图被点击后取得焦点（`focusable(true)` + `set_focus`）
pub fn panel_activate(window: &tauri::WebviewWindow) {
    if let Err(e) = window.set_focusable(true).and_then(|()| window.set_focus()) {
        log::warn!("activate panel failed: {e}");
    }
}

/// WebView2 的可见性与内存目标只在 Windows 上有意义
pub fn panel_restore_visibility(window: &tauri::WebviewWindow) {
    let _ = window;
}

/// WebView2 的可见性与内存目标只在 Windows 上有意义
pub fn panel_release_visibility(window: &tauri::WebviewWindow, low_memory: bool) {
    let _ = (window, low_memory);
}

/// Native 帧模式只在 macOS 使用；Css 帧模式的 WebView 随窗口 autoresize
pub fn panel_pin_webview(window: &tauri::WebviewWindow, height: f64) -> Result<(), IpcError> {
    let _ = (window, height);
    Ok(())
}

/// 点击穿透轮询的共享状态。`generation` 每次启动 / 停止都递增；轮询线程只在持锁且代号仍是自己的时候改窗口的命中
/// 测试，主路径停用时在同一把锁里递增代号——两者对 `set_ignore_cursor_events` 的调用（都投递到 GTK 主循环）因此严格
/// 有序：旧线程不会在主路径恢复命中测试之后再把它关掉
struct ClickPoller {
    region: Option<ClickRegion>,
    generation: u64,
    /// 有一个代号为 `generation` 的轮询线程在跑（线程退出时在锁内清掉，下一次显示据此重新启动）
    running: bool,
}

static CLICK_POLLER: Mutex<ClickPoller> = Mutex::new(ClickPoller {
    region: None,
    generation: 0,
    running: false,
});

fn poller() -> std::sync::MutexGuard<'static, ClickPoller> {
    CLICK_POLLER.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 透明区点击穿透（见模块注释）：`Some` 时启动（或更新）30 Hz 轮询，`None` 时停止并恢复命中测试
pub fn panel_set_click_through(window: &tauri::WebviewWindow, region: Option<ClickRegion>) {
    let mut p = poller();
    p.region = region;
    match region {
        Some(_) if p.running => {}
        Some(_) => {
            p.generation = p.generation.wrapping_add(1);
            p.running = true;
            let generation = p.generation;
            let window = window.clone();
            let spawned = std::thread::Builder::new()
                .name("wisp-click-through".to_owned())
                .spawn(move || click_through_loop(&window, generation));
            if let Err(e) = spawned {
                p.running = false;
                log::warn!("spawn click-through poller failed: {e}");
            }
        }
        None => {
            // 递增代号与恢复命中测试在同一把锁里：持锁检查代号的轮询线程要么已经投递完，要么之后什么都不做
            p.generation = p.generation.wrapping_add(1);
            p.running = false;
            if let Err(e) = window.set_ignore_cursor_events(false) {
                log::warn!("restore cursor events failed: {e}");
            }
        }
    }
}

fn click_through_loop(window: &tauri::WebviewWindow, generation: u64) {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::ConnectionExt as _;

    let finish = |reason: &str| {
        let mut p = poller();
        if p.generation == generation {
            // 意外退出：清掉标记，下一次 Some 会重新启动；命中测试恢复成默认（不穿透）
            p.running = false;
            if let Err(e) = window.set_ignore_cursor_events(false) {
                log::warn!("restore cursor events failed: {e}");
            }
            if !reason.is_empty() {
                log::info!("click-through poller stopped: {reason}");
            }
        }
    };
    let Ok((conn, screen)) = x11rb::connect(None) else {
        finish("no X11 display");
        return;
    };
    let root = conn.setup().roots[screen].root;
    // 窗口实际的命中测试状态未知（上一个线程可能停在任意状态）：第一次一定显式设置
    let mut ignoring: Option<bool> = None;
    loop {
        let region = {
            let p = poller();
            if p.generation == generation { p.region } else { None }
        };
        // 代号变了（主路径已停用并恢复命中测试，或已换成新线程）：安静退出
        let Some(region) = region else { return };
        let pointer = conn.query_pointer(root).ok().and_then(|c| c.reply().ok());
        let Some(pointer) = pointer else {
            finish("X11 connection lost");
            return;
        };
        let through = region.passes_through(f64::from(pointer.root_x), f64::from(pointer.root_y));
        if ignoring != Some(through) {
            let p = poller();
            if p.generation != generation {
                return;
            }
            if let Err(e) = window.set_ignore_cursor_events(through) {
                drop(p);
                log::warn!("set_ignore_cursor_events failed: {e}");
                finish("");
                return;
            }
            ignoring = Some(through);
        }
        std::thread::sleep(CLICK_THROUGH_POLL);
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

/// 鼠标左键或右键此刻是否按着（拖动面板时区分「用户按住不动」与「已经松开」）。X11 查询指针的按键掩码；Wayland 取不到
/// （也读不到窗口位置），恒为 false
pub fn primary_button_down() -> bool {
    use x11rb::protocol::xproto::{ConnectionExt as _, KeyButMask};
    if std::env::var_os("WAYLAND_DISPLAY").is_some() && std::env::var_os("DISPLAY").is_none() {
        return false;
    }
    let Ok((conn, screen)) = x11rb::connect(None) else {
        return false;
    };
    let Some(root) = x11rb::connection::Connection::setup(&conn)
        .roots
        .get(screen)
        .map(|r| r.root)
    else {
        return false;
    };
    conn.query_pointer(root)
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some_and(|p| p.mask.intersects(KeyButMask::BUTTON1 | KeyButMask::BUTTON3))
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

/// 把窗口放到 `rect`（物理像素：外框左上角 + 客户区尺寸）
pub fn set_window_frame(window: &tauri::WebviewWindow, rect: Rect) -> Result<(), IpcError> {
    set_frame(window, rect)
}

/// 截图前隐藏窗口后等合成器重绘
pub fn flush_after_hide() {
    std::thread::sleep(COMPOSITOR_SETTLE);
}

/// 是否为 macOS 的「重新打开」事件：本平台没有这个事件，恒为 false
pub fn is_reopen(_ev: &tauri::RunEvent) -> bool {
    false
}

/// 当前前台是否落在桌面 / 面板（任务栏）上，或者没有活动窗口（X11：`_NET_ACTIVE_WINDOW` 为空，或它的
/// `_NET_WM_WINDOW_TYPE` 是 DESKTOP / DOCK）。剪贴板 / 输入面板「点击外部」失焦关闭时据此决定要不要把焦点还给原应用。
/// Wayland 下拿不到全局前台，返回 false（不还原）
pub fn foreground_is_shell() -> bool {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};

    if crate::app::runtime::is_wayland_session() {
        return false;
    }
    let Ok((conn, screen)) = x11rb::connect(None) else {
        return false;
    };
    let root = conn.setup().roots[screen].root;
    let atom = |name: &[u8]| {
        conn.intern_atom(true, name)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.atom)
            .filter(|a| *a != 0)
    };
    let Some(active) = atom(b"_NET_ACTIVE_WINDOW") else {
        return false;
    };
    let window = conn
        .get_property(false, root, active, AtomEnum::WINDOW, 0, 1)
        .ok()
        .and_then(|c| c.reply().ok())
        .and_then(|r| r.value32().and_then(|mut v| v.next()));
    let Some(window) = window.filter(|w| *w != 0) else {
        // 窗口管理器报告没有活动窗口：焦点在桌面上
        return true;
    };
    let (Some(kind), Some(desktop), Some(dock)) = (
        atom(b"_NET_WM_WINDOW_TYPE"),
        atom(b"_NET_WM_WINDOW_TYPE_DESKTOP"),
        atom(b"_NET_WM_WINDOW_TYPE_DOCK"),
    ) else {
        return false;
    };
    conn.get_property(false, window, kind, AtomEnum::ATOM, 0, 16)
        .ok()
        .and_then(|c| c.reply().ok())
        .and_then(|r| r.value32().map(|v| v.collect::<Vec<u32>>()))
        .is_some_and(|types| types.contains(&desktop) || types.contains(&dock))
}

// ---------------------------------------------------------------------------------------------------------------
// Pinned

/// Pinned 的平台属性：无装饰来自模板覆盖，没有额外的原生设置
pub fn pinned_attach(window: &tauri::WebviewWindow) -> Result<(), IpcError> {
    let _ = window;
    Ok(())
}

/// 不激活地显示 pinned 之前：暂时不接受焦点（WM_HINTS input = False，窗口管理器映射它时不给焦点），稍后恢复。
/// 三端一致：交接、截图后恢复、托盘「显示固定窗口」都不抢原应用的前台，用户点击时才激活
fn suppress_focus_on_map(window: &tauri::WebviewWindow) {
    if let Err(e) = window.set_focusable(false) {
        log::warn!("pinned set_focusable(false) failed: {e}");
        return;
    }
    let window = window.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(PINNED_FOCUSABLE_RESTORE).await;
        if let Err(e) = window.set_focusable(true) {
            log::warn!("pinned set_focusable(true) failed: {e}");
        }
    });
}

/// 固定交接：在同一个主线程回调里显示 pinned、隐藏面板（两者的请求排进同一次主循环迭代，不出现空帧），
/// 并等主循环执行完再返回。pinned 不激活（见 [`suppress_focus_on_map`]）。`expand_to` 为 Some（展开）时直接显示在目标
/// 矩形，面板由 CSS 淡出后自己隐藏（调用方不传 `panel`）
pub fn pinned_handoff(
    pinned: &tauri::WebviewWindow,
    panel: Option<&tauri::WebviewWindow>,
    rect: Rect,
    expand_to: Option<Rect>,
    animate: bool,
) -> Result<(), IpcError> {
    let _ = animate;
    let target = expand_to.unwrap_or(rect);
    let pinned_win = pinned.clone();
    let panel = panel.cloned();
    suppress_focus_on_map(pinned);
    // 主循环卡住超时时这次交接作废（不会稍后才把 pinned 显示出来），见 post_and_wait
    super::post_and_wait(
        |job| pinned.run_on_main_thread(job),
        move || {
            let shown = set_frame(&pinned_win, target).and_then(|()| Ok(pinned_win.show()?));
            if let Some(panel) = panel
                && let Err(e) = panel.hide()
            {
                log::warn!("hide panel failed: {e}");
            }
            shown
        },
        MAIN_LOOP_WAIT,
    )??;
    wait_visible(pinned, true);
    Ok(())
}

/// 不激活地把 pinned 放到最前。`restore_minimized`：托盘「显示固定窗口」为 true（最小化的也还原）；截图后恢复为
/// false（最小化的保持原样）
pub fn pinned_show(window: &tauri::WebviewWindow, restore_minimized: bool) -> Result<(), IpcError> {
    let minimized = window.is_minimized()?;
    if minimized && !restore_minimized {
        return Ok(());
    }
    suppress_focus_on_map(window);
    if minimized {
        window.unminimize()?;
    }
    window.show()?;
    wait_visible(window, true);
    Ok(())
}

/// 立即隐藏 pinned（截图前，不播放动画），等主循环执行完 hide 再返回（随后 `flush_after_hide` 再等合成器）
pub fn pinned_hide(window: &tauri::WebviewWindow) -> Result<(), IpcError> {
    window.hide()?;
    wait_visible(window, false);
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// Overlay

/// Overlay 的平台属性：置顶（`_NET_WM_STATE_ABOVE`）来自模板的 `alwaysOnTop`，不进任务栏来自 `skipTaskbar`
pub fn overlay_attach(window: &tauri::WebviewWindow) -> Result<(), IpcError> {
    window.set_always_on_top(true)?;
    Ok(())
}

/// X11 的冻结帧由 WebView canvas 画（先 1/2 降采样帧，再全分辨率）
pub fn overlay_set_frozen(window: &tauri::WebviewWindow, frame: &wisp_capture::Frame) -> bool {
    let _ = (window, frame);
    false
}

/// 显示 Overlay：先放到显示器的完整区域，再请求全屏（`_NET_WM_STATE_FULLSCREEN`，盖住面板与任务栏）；
/// `key` 为 true 时取得键盘焦点（tao 的 `set_focus` 只对已经可见的窗口生效，所以等 show 执行完再请求）
pub fn overlay_show(window: &tauri::WebviewWindow, rect: Rect, key: bool) -> Result<(), IpcError> {
    set_frame(window, rect)?;
    window.show()?;
    window.set_fullscreen(true)?;
    wait_visible(window, true);
    if key {
        window.set_focus()?;
    }
    Ok(())
}

/// 多屏截图时让光标所在屏的 Overlay 接收键盘
pub fn overlay_focus(window: &tauri::WebviewWindow) {
    if let Err(e) = window.set_focus() {
        log::debug!("focus {} failed: {e}", window.label());
    }
}

/// X11 上 Overlay 是全屏 + 取得焦点的窗口，窗口管理器把它放在置顶层之上，面板显示后不需要重新压栈
pub fn overlay_raise(window: &tauri::WebviewWindow) {
    let _ = window;
}

/// 隐藏 Overlay（复用的预建窗口）
pub fn overlay_hide(window: &tauri::WebviewWindow) {
    if let Err(e) = window.set_fullscreen(false).and_then(|()| window.hide()) {
        log::warn!("hide {} failed: {e}", window.label());
    }
}

/// 销毁面板窗口（空闲卸载、失效重建）
pub fn panel_destroy(window: &tauri::WebviewWindow) {
    super::note_destroying(window.label());
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

/// 窗口创建之后接管 tao 多出的引用：只有 macOS 的 tao 有这个问题（见 macos.rs 的 `adopt_window`），这里什么都不做
pub fn adopt_window(window: &tauri::WebviewWindow) {
    let _ = window;
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
