//! 设置 / 引导窗口：同时最多 1 个；关闭即销毁；存在期间 macOS ActivationPolicy = Regular，
//! 关掉最后一个时切回 Accessory。
//!
//! **首次显示不白闪**：窗口按模板保持隐藏创建，并按已解析的主题设置窗口 / WebView 背景色（与
//! tokens.css 的 `--surface-solid` 一致）；等页面加载完成（`PageLoadEvent::Finished`：模块脚本已执行、boot 属性已生效）
//! 后再 show + set_focus。加载异常时 [`REVEAL_FALLBACK`] 后兜底显示（背景色已设，不会白底）。
//!
//! **位置记忆**（settings 窗口）：移动 / 缩放时在内存里记下外框（不写盘），窗口销毁时把它换算成
//! 「显示器 id + 本屏逻辑坐标」写进 state.json 的 `settings_window`；下次创建时原显示器还在、且矩形至少一半落在
//! 它的工作区内就恢复（尺寸不小于模板的最小尺寸），否则沿用模板的居中。恢复发生在显示之前，用户看不到跳动。

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tauri::webview::PageLoadEvent;
use tauri::window::Color;
use tauri::{Emitter, Manager, WebviewWindowBuilder, WindowEvent};
use wisp_types::{Boot, Rect, ResolvedTheme, event};

use super::placement::{self, Screen};
use crate::error::IpcError;
use crate::settings::state_store::WindowGeometry;
use crate::state::AppState;

/// 设置窗口 label
pub const SETTINGS_LABEL: &str = "settings";
/// 引导窗口 label（入口同为 settings.html，前端按 boot.label 区分）
pub const ONBOARDING_LABEL: &str = "onboarding";

/// 页面迟迟没有加载完成时的兜底显示时间
pub const REVEAL_FALLBACK: Duration = Duration::from_millis(1200);

/// 设置窗口的最小尺寸（与 tauri.conf.json 的 settings 模板一致）
pub const SETTINGS_MIN_SIZE: (f64, f64) = (680.0, 480.0);

/// 窗口背景色（首帧之前 WebView 透出的颜色）。**与 `shared/styles/tokens.css` 的 `--surface-solid` 保持一致**
/// （浅色 #fbfbfc / 深色 #1c1c1f；调整令牌时同步这里）
pub fn window_background(theme: ResolvedTheme) -> Color {
    match theme {
        ResolvedTheme::Light => Color(0xfb, 0xfb, 0xfc, 0xff),
        ResolvedTheme::Dark => Color(0x1c, 0x1c, 0x1f, 0xff),
    }
}

/// 已创建、尚未首次显示（页面还在加载）
static SETTINGS_PENDING: AtomicBool = AtomicBool::new(false);
static ONBOARDING_PENDING: AtomicBool = AtomicBool::new(false);

/// 设置窗口最近一次的外框（桌面坐标），销毁时写盘
static SETTINGS_FRAME: Mutex<Option<Rect>> = Mutex::new(None);

/// 原生关闭按钮之后、前端收尾期间（[`close_requested`]）
static SETTINGS_CLOSING: AtomicBool = AtomicBool::new(false);
static ONBOARDING_CLOSING: AtomicBool = AtomicBool::new(false);

/// 原生关闭之后等前端收尾的上限（与前端 `CLOSE_SETTLE_TIMEOUT_MS` 相同）：前端不响应（页面卡死、还没加载）时照样关闭，
/// 不让关窗卡住
pub const CLOSE_HANDSHAKE_MAX: Duration = Duration::from_millis(1500);

fn closing_flag(label: &str) -> Option<&'static AtomicBool> {
    match label {
        SETTINGS_LABEL => Some(&SETTINGS_CLOSING),
        ONBOARDING_LABEL => Some(&ONBOARDING_CLOSING),
        _ => None,
    }
}

/// 关闭请求（`WindowEvent::CloseRequested`）要不要放行：原生关闭按钮直接销毁 WebView，
/// 前端的卸载清理不会执行、异步 IPC 也不保证送达，停顿中的输入（450 ms 防抖）会丢。第一次请求拦下并告诉前端
/// （`window:close_requested`），前端收尾之后自己 `window.close()`——这一次（以及收尾期间用户再点一次）放行。
/// 返回 true 表示放行
fn close_requested(flag: &AtomicBool) -> bool {
    flag.swap(true, Ordering::AcqRel)
}

fn pending_flag(label: &str) -> Option<&'static AtomicBool> {
    match label {
        SETTINGS_LABEL => Some(&SETTINGS_PENDING),
        ONBOARDING_LABEL => Some(&ONBOARDING_PENDING),
        _ => None,
    }
}

/// 打开（不存在时按 tauri.conf.json 的模板隐藏创建，页面加载完成后再显示）或聚焦窗口。
///
/// `boot.route` 为深链目标（设置分组 / 引导步骤）：新建窗口时已经写进 boot 脚本；窗口已存在时
/// emit_to 该窗口 `window:navigate`（载荷 `WindowTarget`），前端切到对应分组 / 步骤
pub fn open(app: &tauri::AppHandle, boot: &Boot) -> Result<(), IpcError> {
    let label = boot.label.as_str();
    if let Some(win) = app.get_webview_window(label) {
        let pending = pending_flag(label).is_some_and(|f| f.load(Ordering::Acquire));
        if !pending {
            super::native::set_regular_activation(app, true);
            win.unminimize()?;
            win.show()?;
            win.set_focus()?;
        }
        // 仍在加载时不提前显示（会白闪）；路由事件照发，页面若尚未订阅则以 Boot.route 为准
        if let Some(target) = &boot.route
            && let Err(e) = app.emit_to(label, event::WINDOW_NAVIGATE, target)
        {
            log::warn!("emit {} to {label} failed: {e}", event::WINDOW_NAVIGATE);
        }
        return Ok(());
    }
    let config = app
        .config()
        .app
        .windows
        .iter()
        .find(|w| w.label == label)
        .cloned()
        .ok_or_else(|| IpcError::not_found(format!("window template {label}")))?;
    let created = Instant::now();
    let builder = WebviewWindowBuilder::from_config(app, &config)?
        .initialization_script(super::boot_script(boot))
        .visible(false)
        .background_color(window_background(boot.theme))
        .on_page_load(move |win, payload| {
            if payload.event() == PageLoadEvent::Finished {
                reveal(&win, "page_load", created);
            }
        });
    let kind = super::native::TemplateKind::of(label);
    let win = super::native::template_overrides(builder, kind, boot.frame_mode).build()?;
    // 关闭后窗口与它的 WebView 必须真的释放（macOS 的 tao 引用泄漏，见 native::adopt_window）
    super::native::adopt_window(&win);
    if let Some(flag) = pending_flag(label) {
        flag.store(true, Ordering::Release);
    }
    if label == SETTINGS_LABEL {
        super::native::settings_attach(&win);
        restore_geometry(app, &win);
    }
    let handle = app.clone();
    let owned_label = label.to_owned();
    let tracked = win.clone();
    win.on_window_event(move |ev| match ev {
        WindowEvent::CloseRequested { api, .. } => {
            let Some(flag) = closing_flag(&owned_label) else { return };
            if close_requested(flag) {
                return;
            }
            api.prevent_close();
            if let Err(e) = handle.emit_to(owned_label.as_str(), event::WINDOW_CLOSE_REQUESTED, ()) {
                log::warn!("emit {} failed: {e}", event::WINDOW_CLOSE_REQUESTED);
            }
            let app = handle.clone();
            let label = owned_label.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(CLOSE_HANDSHAKE_MAX).await;
                if let Some(win) = app.get_webview_window(&label) {
                    log::warn!("{label}: the page did not close itself within {CLOSE_HANDSHAKE_MAX:?}; closing");
                    if let Err(e) = win.close() {
                        log::warn!("close {label} failed: {e}");
                    }
                }
            });
        }
        WindowEvent::Moved(_) | WindowEvent::Resized(_) => {
            let frame = super::native::window_frame(&tracked);
            if owned_label == SETTINGS_LABEL && frame.is_some() {
                *SETTINGS_FRAME.lock() = frame;
            }
            // 自身窗口矩形表（浮标判定）
            if let Some(state) = handle.try_state::<AppState>() {
                state.windows.note_own_rect(&owned_label, frame);
            }
        }
        WindowEvent::Destroyed => {
            // set_window_frame 会给窗口建逐帧驱动：销毁后释放（防止窗口泄漏）
            super::native::release_offscreen_drivers(&handle);
            if let Some(state) = handle.try_state::<AppState>() {
                state.windows.note_own_rect(&owned_label, None);
            }
            if let Some(flag) = pending_flag(&owned_label) {
                flag.store(false, Ordering::Release);
            }
            if let Some(flag) = closing_flag(&owned_label) {
                flag.store(false, Ordering::Release);
            }
            if owned_label == SETTINGS_LABEL
                && let Some(frame) = SETTINGS_FRAME.lock().take()
            {
                save_geometry(&handle, frame);
            }
            if !any_regular_window(&handle) {
                super::native::set_regular_activation(&handle, false);
            }
        }
        _ => {}
    });
    let fallback = win.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(REVEAL_FALLBACK).await;
        reveal(&fallback, "fallback", created);
    });
    Ok(())
}

/// 首次显示（只执行一次：页面加载完成与兜底计时器谁先到谁显示；dev 下整页刷新再次 Finished 时不会抢焦点）
fn reveal(win: &tauri::WebviewWindow, via: &str, created: Instant) {
    let Some(flag) = pending_flag(win.label()) else { return };
    if !flag.swap(false, Ordering::AcqRel) {
        return;
    }
    log::info!(
        "{} window shown via {via} after {} ms",
        win.label(),
        created.elapsed().as_millis()
    );
    super::native::set_regular_activation(win.app_handle(), true);
    if let Err(e) = win.show().and_then(|()| win.set_focus()) {
        log::warn!("show {} failed: {e}", win.label());
    }
}

/// 是否还有需要 Regular 激活策略的窗口
fn any_regular_window(app: &tauri::AppHandle) -> bool {
    [SETTINGS_LABEL, ONBOARDING_LABEL]
        .iter()
        .any(|l| app.get_webview_window(l).is_some())
}

fn screens(app: &tauri::AppHandle) -> Vec<Screen> {
    app.try_state::<AppState>()
        .map(|s| s.platform.screen.monitors().iter().map(Screen::from_info).collect())
        .unwrap_or_default()
}

/// 记住的矩形还能用时，显示前把窗口放回去（见模块注释）
fn restore_geometry(app: &tauri::AppHandle, win: &tauri::WebviewWindow) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let Some(saved) = state.settings.state().get().settings_window else {
        return;
    };
    let screens = screens(app);
    let Some(rect) = restorable_rect(&screens, &saved) else {
        log::info!("settings window: saved monitor gone or off-screen, centering");
        return;
    };
    match super::native::set_window_frame(win, rect) {
        Ok(()) => log::info!("settings window geometry restored on monitor {}", saved.monitor_id),
        Err(e) => log::warn!("restore settings window geometry failed: {}", e.message),
    }
}

/// 记住的几何 → 桌面坐标矩形（原显示器已断开、或矩形大半在工作区外时为 None）
pub fn restorable_rect(screens: &[Screen], saved: &WindowGeometry) -> Option<Rect> {
    let screen = screens.iter().find(|s| s.id == saved.monitor_id)?;
    let local = Rect::new(
        saved.x,
        saved.y,
        saved.width.max(SETTINGS_MIN_SIZE.0),
        saved.height.max(SETTINGS_MIN_SIZE.1),
    );
    if !placement::restorable(screen, local) {
        return None;
    }
    let local = placement::clamp_into_work(screen, local);
    Some(screen.to_desktop(local, super::native::DESKTOP_UNIT))
}

/// 桌面坐标的外框 → 「显示器 id + 本屏逻辑坐标」
pub fn geometry_of(screens: &[Screen], frame: Rect) -> Option<WindowGeometry> {
    let center = wisp_types::Point {
        x: frame.x + frame.width / 2.0,
        y: frame.y + frame.height / 2.0,
    };
    let screen = placement::screen_at_desktop(screens, center, super::native::DESKTOP_UNIT)?;
    let local = screen.from_desktop(frame, super::native::DESKTOP_UNIT);
    Some(WindowGeometry {
        monitor_id: screen.id.clone(),
        x: local.x.round(),
        y: local.y.round(),
        width: local.width.round(),
        height: local.height.round(),
    })
}

/// 窗口销毁后在后台把几何写进 state.json
fn save_geometry(app: &tauri::AppHandle, frame: Rect) {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let Some(state) = app.try_state::<AppState>() else {
            return;
        };
        let Some(geometry) = geometry_of(&screens(&app), frame) else {
            return;
        };
        let result = state.settings.state().update(&mut |s| {
            s.settings_window = Some(geometry.clone());
        });
        if let Err(e) = result {
            log::warn!("save settings window geometry failed: {}", e.message);
        }
    });
}

#[cfg(test)]
mod tests {
    use wisp_types::PhysPoint;

    use super::*;

    /// 原生关闭按钮的第一次请求拦下（交给前端收尾），前端随后的 `window.close()`（或用户再点一次）放行；
    /// 窗口销毁后复位
    #[test]
    fn close_handshake_blocks_once_then_lets_the_page_close() {
        let flag = AtomicBool::new(false);
        assert!(!close_requested(&flag), "第一次：拦下，emit window:close_requested");
        assert!(close_requested(&flag), "前端收尾后自己 close：放行");
        assert!(close_requested(&flag), "收尾期间用户再点一次：放行");
        flag.store(false, Ordering::Release);
        assert!(!close_requested(&flag), "销毁后复位，下一个窗口重新握手");
        assert!(closing_flag("panel").is_none(), "只有设置 / 引导窗口握手");
        assert!(closing_flag(SETTINGS_LABEL).is_some() && closing_flag(ONBOARDING_LABEL).is_some());
        assert_eq!(
            CLOSE_HANDSHAKE_MAX,
            Duration::from_millis(1500),
            "与前端 CLOSE_SETTLE_TIMEOUT_MS 一致"
        );
    }

    #[test]
    fn background_matches_surface_solid_tokens() {
        // tokens.css：浅色 --surface-solid: #fbfbfc；深色 --surface-solid: #1c1c1f
        let tokens = include_str!("../../../src/shared/styles/tokens.css");
        let hex = |c: Color| format!("--surface-solid: #{:02x}{:02x}{:02x};", c.0, c.1, c.2);
        for theme in [ResolvedTheme::Light, ResolvedTheme::Dark] {
            let decl = hex(window_background(theme));
            assert!(
                tokens.contains(&decl),
                "tokens.css 里找不到 `{decl}`：设置窗口首帧背景色与令牌不一致"
            );
        }
    }

    fn screens() -> Vec<Screen> {
        vec![
            Screen {
                id: "1".to_owned(),
                frame: Rect::new(0.0, 0.0, 1440.0, 900.0),
                work: Rect::new(0.0, 25.0, 1440.0, 805.0),
                scale: 2.0,
                physical_origin: PhysPoint { x: 0, y: 0 },
                primary: true,
            },
            Screen {
                id: "2".to_owned(),
                frame: Rect::new(1440.0, 0.0, 1920.0, 1080.0),
                work: Rect::new(0.0, 0.0, 1920.0, 1080.0),
                scale: 1.0,
                physical_origin: PhysPoint { x: 2880, y: 0 },
                primary: false,
            },
        ]
    }

    #[test]
    fn geometry_round_trips_through_monitor_local_coordinates() {
        let unit = super::super::native::DESKTOP_UNIT;
        let s = screens();
        let local = Rect::new(200.0, 100.0, 760.0, 560.0);
        let frame = s[1].to_desktop(local, unit);
        let g = geometry_of(&s, frame).expect("geometry");
        assert_eq!(g.monitor_id, "2");
        assert_eq!((g.x, g.y, g.width, g.height), (200.0, 100.0, 760.0, 560.0));
        assert_eq!(restorable_rect(&s, &g), Some(frame));
    }

    #[test]
    fn missing_monitor_or_offscreen_falls_back_to_center() {
        let s = screens();
        let gone = WindowGeometry {
            monitor_id: "9".to_owned(),
            x: 10.0,
            y: 10.0,
            width: 760.0,
            height: 560.0,
        };
        assert_eq!(restorable_rect(&s, &gone), None);
        let off = WindowGeometry {
            monitor_id: "1".to_owned(),
            x: 1300.0,
            y: 10.0,
            width: 760.0,
            height: 560.0,
        };
        assert_eq!(restorable_rect(&s, &off), None);
        // 太小的尺寸提升到最小尺寸
        let small = WindowGeometry {
            monitor_id: "1".to_owned(),
            x: 100.0,
            y: 100.0,
            width: 100.0,
            height: 100.0,
        };
        let r = restorable_rect(&s, &small).expect("restorable");
        let local = s[0].from_desktop(r, super::super::native::DESKTOP_UNIT);
        assert_eq!((local.width, local.height), SETTINGS_MIN_SIZE);
    }
}
