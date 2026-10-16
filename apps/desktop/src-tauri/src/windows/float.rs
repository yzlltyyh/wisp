//! 悬浮剪贴板：单例窗口，label 与模板都是 `clipfloat`（不用 `pinned-` 前缀：那是翻译 / OCR 固定窗口的命名空间）。
//!
//! **打开**（`clipboard_float_open`，剪贴板面板头部的钉子按钮或 ⌘P）：在记住的位置（state.json 的 `clip_float`，原显示器
//! 还在、矩形大半在工作区内才用）或面板卡片的位置**隐藏**创建窗口，记下要继承的查询、筛选与光标。页面先订阅事件、
//! 再 `clipboard_float_ready` 取初始状态（此后 `clipboard:changed` 同时推给它），同步渲染首帧后
//! `clipboard_float_reveal`：在**同一个主线程回调**里显示悬浮窗、隐藏发起的面板会话（与固定窗口的交接相同，没有空帧）。
//! 页面迟迟不 reveal 时 [`REVEAL_FALLBACK`] 后照样交接。已经开着时只把它带到前面、隐藏面板。
//!
//! **不抢焦点**：平时点击条目不让它成为 key / 前台（平台差异在 `native::float_*`），粘贴键直接落进目标应用；搜索框要
//! 打字时页面调用 `clipboard_float_focus(true)` 临时可聚焦；粘贴前由粘贴管线经 [`WindowManager::float_yield`] 交还。
//!
//! **关闭即销毁**（✕ / ⌘W，或系统的关闭）：窗口、WebView 与订阅一起释放，关着时零开销；销毁时把位置与大小写进
//! state.json。获得焦点或指针移入时顺带触发一次同步拉取（同步层自己节流）。

use std::time::Duration;

use parking_lot::Mutex;
use tauri::{Manager, WebviewWindowBuilder, WindowEvent};
use wisp_types::{AppRef, ClipFilter, HideReason, Rect};

use super::manager::WindowManager;
use super::native;
use super::panel::PANEL_LABEL;
use super::placement::{self, DesktopUnit, Screen};
use super::settings_window::window_background;
use super::sizing::SHADOW_MARGIN;
use crate::clipboard_svc::FLOAT_LABEL;
use crate::error::IpcError;
use crate::settings::state_store::WindowGeometry;
use crate::state::AppState;

/// 页面迟迟不 reveal 时的兜底交接时间
pub const REVEAL_FALLBACK: Duration = Duration::from_millis(1500);
/// 卡片的最小尺寸（与 tauri.conf.json 的 `clipfloat` 模板一致）
pub const FLOAT_MIN_SIZE: (f64, f64) = (320.0, 280.0);
/// 从面板交接、又没有记住的位置时的最小高度：面板只按内容长高，悬浮窗要能放下一屏条目
pub const FLOAT_OPEN_HEIGHT: f64 = 420.0;

/// 从面板继承的视图状态
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloatSeed {
    pub query: String,
    pub filter: ClipFilter,
    pub selected_id: Option<i64>,
}

/// 悬浮窗的登记（窗口存在期间）
#[derive(Debug)]
struct Entry {
    seed: FloatSeed,
    /// 发起交接的面板会话：reveal 时只隐藏它
    session: Option<u64>,
    /// 首次显示的矩形（桌面坐标）
    rect: Rect,
    /// 页面已订阅事件（`clipboard_float_ready` 之后）
    ready: bool,
    /// 已交接上屏
    shown: bool,
    on_top: bool,
}

/// 悬浮剪贴板的状态（`WindowManager` 持有一份）
#[derive(Debug, Default)]
pub struct FloatWindow {
    entry: Mutex<Option<Entry>>,
    /// 最近一次的外框（桌面坐标）：移动 / 缩放时记下，销毁或退出时写盘
    frame: Mutex<Option<Rect>>,
    /// 搜索框临时可聚焦之前的前台应用（Windows / X11）：交还焦点时激活它
    prev: Mutex<Option<AppRef>>,
}

/// 窗口四周的阴影边距（本屏逻辑像素）：Linux 透明窗口由 CSS 画圆角阴影（与固定窗口相同），其余为 0
fn shadow_margin() -> f64 {
    if native::pinned_css_shadow() {
        SHADOW_MARGIN
    } else {
        0.0
    }
}

/// 卡片矩形 → 窗口矩形（本屏逻辑坐标，四周加上阴影边距）
fn inflate(card: Rect, margin: f64) -> Rect {
    Rect::new(
        card.x - margin,
        card.y - margin,
        card.width + 2.0 * margin,
        card.height + 2.0 * margin,
    )
}

/// 卡片矩形 → 窗口矩形
fn window_rect(card: Rect) -> Rect {
    inflate(card, shadow_margin())
}

/// 记住的几何 → 桌面坐标矩形（原显示器已断开、或矩形大半在工作区外时为 None）
pub fn restorable_rect(screens: &[Screen], saved: &WindowGeometry) -> Option<Rect> {
    restorable_rect_in(screens, saved, native::DESKTOP_UNIT, shadow_margin())
}

/// [`restorable_rect`] 的实现：记住的是卡片矩形，加回阴影边距之后才是窗口矩形（不加回时每次重开都缩小一圈并漂移）
fn restorable_rect_in(screens: &[Screen], saved: &WindowGeometry, unit: DesktopUnit, margin: f64) -> Option<Rect> {
    let screen = screens.iter().find(|s| s.id == saved.monitor_id)?;
    let local = Rect::new(
        saved.x,
        saved.y,
        saved.width.max(FLOAT_MIN_SIZE.0),
        saved.height.max(FLOAT_MIN_SIZE.1),
    );
    if !placement::restorable(screen, local) {
        return None;
    }
    Some(screen.to_desktop(inflate(placement::clamp_into_work(screen, local), margin), unit))
}

/// 窗口外框（桌面坐标）→ 要记住的卡片几何（本屏逻辑坐标）。阴影边距是逻辑像素：先换算到本屏逻辑坐标再去掉，
/// Windows / Linux 的桌面坐标是物理像素，直接相减在缩放屏上会多减或少减
fn card_geometry_in(screens: &[Screen], frame: Rect, unit: DesktopUnit, margin: f64) -> Option<WindowGeometry> {
    let center = wisp_types::Point {
        x: frame.x + frame.width / 2.0,
        y: frame.y + frame.height / 2.0,
    };
    let screen = placement::screen_at_desktop(screens, center, unit)?;
    let local = screen.from_desktop(frame, unit);
    Some(WindowGeometry {
        monitor_id: screen.id.clone(),
        x: (local.x + margin).round(),
        y: (local.y + margin).round(),
        width: (local.width - 2.0 * margin).max(1.0).round(),
        height: (local.height - 2.0 * margin).max(1.0).round(),
    })
}

/// 从面板卡片（本屏逻辑坐标）得出首次显示的卡片矩形：宽度沿用面板，高度至少 [`FLOAT_OPEN_HEIGHT`]，并夹回工作区
pub fn card_from_panel(screen: &Screen, panel: Rect) -> Rect {
    let card = Rect::new(
        panel.x,
        panel.y,
        panel.width.max(FLOAT_MIN_SIZE.0),
        panel.height.max(FLOAT_OPEN_HEIGHT).min(screen.work.height),
    );
    placement::clamp_into_work(screen, card)
}

impl FloatWindow {
    pub fn new() -> Self {
        Self::default()
    }

    /// 页面已订阅事件、窗口还在：`clipboard:changed` 同时推给它
    pub fn ready(&self) -> bool {
        self.entry.lock().as_ref().is_some_and(|e| e.ready)
    }

    /// 已交接上屏（截图前要隐藏它、粘贴前要交还焦点）
    pub fn shown(&self) -> bool {
        self.entry.lock().as_ref().is_some_and(|e| e.shown)
    }

    fn remove(&self) -> Option<Rect> {
        *self.entry.lock() = None;
        self.frame.lock().take()
    }
}

impl WindowManager {
    /// 剪贴板面板 → 悬浮剪贴板（见模块文档）。**会阻塞**到窗口建好（WebView 创建 100–400 ms），在阻塞线程上调用
    pub fn float_open(&self, session: u64, seed: FloatSeed) -> Result<(), IpcError> {
        // 正在建、还没交接（连按两次 ⌘P）：交给第一次，它交接时隐藏面板；现在就显示会先露出一张还没渲染的空卡片
        if self.float_take_over_pending(session) {
            return Ok(());
        }
        if let Some(win) = self.app().get_webview_window(FLOAT_LABEL) {
            // 已经开着：带到前面，面板照常退出（继承的状态不覆盖用户在悬浮窗里的操作）
            let prev = self.panel_prev_app(session);
            native::float_show(&win)?;
            self.panel_controller().request_hide_for(session, HideReason::Pin);
            self.hand_focus_back(prev);
            return Ok(());
        }
        let screens = self.screens_now();
        let state = self.settings().state().get().clip_float;
        // 面板卡片所在的位置（会话已过期时用最近一次显示过的卡片）
        let from_panel = self.panel_controller().last_card().map(|last| {
            let card = self
                .panel_state()
                .filter(|p| p.session_id == session)
                .map_or(last.rect, |p| p.rect);
            last.screen
                .to_desktop(window_rect(card_from_panel(&last.screen, card)), native::DESKTOP_UNIT)
        });
        let rect = state
            .geometry
            .as_ref()
            .and_then(|g| restorable_rect(&screens, g))
            .or(from_panel)
            .ok_or_else(|| IpcError::not_found("no monitor for the floating clipboard"))?;
        let on_top = state.on_top;
        {
            let mut entry = self.float().entry.lock();
            // 正在建（连按两次 ⌘P）：交给第一次，它交接时会隐藏面板
            if let Some(e) = entry.as_mut() {
                e.session = Some(session);
                return Ok(());
            }
            *entry = Some(Entry {
                seed,
                session: Some(session),
                rect,
                ready: false,
                shown: false,
                on_top,
            });
        }
        match self.build_float(rect, on_top) {
            Ok(win) => self.watch_float(&win),
            Err(e) => {
                self.float().remove();
                return Err(e);
            }
        }
        self.note_activity(format!("{FLOAT_LABEL} opened"), REVEAL_FALLBACK);
        let app = self.app().clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(REVEAL_FALLBACK).await;
            // 交接要同步等主线程：放进阻塞线程池
            let _ = tauri::async_runtime::spawn_blocking(move || {
                if let Some(state) = app.try_state::<AppState>()
                    && state.windows.float_reveal_pending()
                {
                    log::info!("floating clipboard revealed by the fallback");
                    state.windows.float_reveal();
                }
            })
            .await;
        });
        Ok(())
    }

    /// 隐藏创建窗口并装上平台属性（窗口一建好页面就开始加载：登记已在调用方完成）
    fn build_float(&self, rect: Rect, on_top: bool) -> Result<tauri::WebviewWindow, IpcError> {
        let app = self.app();
        let boot = self.boot_for(FLOAT_LABEL, None);
        let config = app
            .config()
            .app
            .windows
            .iter()
            .find(|w| w.label == FLOAT_LABEL)
            .cloned()
            .ok_or_else(|| IpcError::not_found("window template clipfloat"))?;
        // 刚销毁的窗口的 `Destroyed` 处理完再建（见 native::note_destroying）
        native::await_destroyed(app);
        let frame_mode = boot.frame_mode;
        let built = WebviewWindowBuilder::from_config(app, &config).and_then(|builder| {
            let mut builder = builder
                .initialization_script(super::boot_script(&boot))
                .visible(false)
                .always_on_top(on_top);
            if !native::pinned_css_shadow() {
                builder = builder.background_color(window_background(boot.theme));
            }
            native::template_overrides(builder, native::TemplateKind::Floating, frame_mode).build()
        });
        let win = built.map_err(|e| {
            log::error!("create floating clipboard failed: {e}");
            IpcError::from(e)
        })?;
        native::adopt_window(&win);
        if let Err(e) = native::float_attach(&win, on_top) {
            log::warn!("floating clipboard attach failed: {}", e.message);
        }
        if let Err(e) = native::set_window_frame(&win, rect) {
            log::warn!("place floating clipboard failed: {}", e.message);
        }
        log::info!("floating clipboard created on_top={on_top}");
        Ok(win)
    }

    /// 生命周期：关闭即销毁（销毁时写下位置与大小）、移动 / 缩放记下外框、获得焦点时同步拉取
    fn watch_float(&self, win: &tauri::WebviewWindow) {
        let window = win.clone();
        let app = self.app().clone();
        win.on_window_event(move |ev| {
            let Some(state) = app.try_state::<AppState>() else {
                return;
            };
            match ev {
                WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    native::destroy_window(&window);
                }
                WindowEvent::Destroyed => {
                    native::release_offscreen_drivers(&app);
                    state.windows.note_own_rect(FLOAT_LABEL, None);
                    if let Some(frame) = state.windows.float().remove() {
                        let app = app.clone();
                        tauri::async_runtime::spawn_blocking(move || {
                            if let Some(state) = app.try_state::<AppState>() {
                                store_geometry(&state, frame);
                            }
                        });
                    }
                    log::info!("floating clipboard closed");
                }
                WindowEvent::Moved(_) | WindowEvent::Resized(_) => {
                    let frame = native::window_frame(&window);
                    if frame.is_some() && state.windows.float().shown() {
                        *state.windows.float().frame.lock() = frame;
                        state.windows.note_own_rect(FLOAT_LABEL, frame);
                    }
                }
                WindowEvent::Focused(true) => state.sync.trigger(crate::sync_svc::CycleReason::ClipboardShown),
                _ => {}
            }
        });
    }

    /// `clipboard_float_ready`：页面已订阅事件，取走继承的视图状态与置顶。窗口不是经 `float_open` 建的（例如页面重载）
    /// 时给出空的初始状态
    pub fn float_page_ready(&self) -> (FloatSeed, bool) {
        let mut g = self.float().entry.lock();
        match g.as_mut() {
            Some(e) => {
                e.ready = true;
                (e.seed.clone(), e.on_top)
            }
            None => (
                FloatSeed {
                    query: String::new(),
                    filter: ClipFilter::All,
                    selected_id: None,
                },
                self.settings().state().get().clip_float.on_top,
            ),
        }
    }

    /// 还没交接（reveal 兜底用）
    fn float_reveal_pending(&self) -> bool {
        self.float().entry.lock().as_ref().is_some_and(|e| !e.shown)
    }

    /// 悬浮窗正在建、还没交接时，改由它在交接时隐藏 `session`（最新发起交接的面板会话），返回 true
    fn float_take_over_pending(&self, session: u64) -> bool {
        match self.float().entry.lock().as_mut().filter(|e| !e.shown) {
            Some(e) => {
                e.session = Some(session);
                true
            }
            None => false,
        }
    }

    /// Windows / X11：把前台交还给 `prev`（后台线程，激活要等窗口管理器）。悬浮窗不可激活，接不住面板留下的前台，
    /// 不交还的话键入会落到系统随便挑的窗口（或仍可聚焦的悬浮窗搜索框）。macOS 的非激活面板不需要
    fn hand_focus_back(&self, prev: Option<AppRef>) {
        if !native::RESTORE_FOCUS_ON_HIDE {
            return;
        }
        let Some(prev) = prev else { return };
        let app = self.app().clone();
        tauri::async_runtime::spawn_blocking(move || {
            if let Some(state) = app.try_state::<AppState>()
                && let Err(e) = state.platform.focus.activate(&prev)
            {
                log::info!("floating clipboard: returning focus failed: {e}");
            }
        });
    }

    /// 退出前（`RunEvent::Exit`，写盘之前）：悬浮窗还开着时把当前位置与大小记进 state.json——托盘退出、更新重启
    /// 不经过窗口的 `Destroyed`
    pub fn float_save_on_exit(&self) {
        let frame = *self.float().frame.lock();
        if let (Some(frame), Some(state)) = (frame, self.app().try_state::<AppState>()) {
            store_geometry(&state, frame);
        }
    }

    /// `clipboard_float_reveal`：首帧已渲染，同一个主线程回调里显示悬浮窗、隐藏发起的面板会话。已交接过时什么都不做。
    /// **会阻塞**到主线程执行完交接
    pub fn float_reveal(&self) {
        let (rect, session) = {
            let mut g = self.float().entry.lock();
            let Some(e) = g.as_mut().filter(|e| !e.shown) else {
                return;
            };
            e.shown = true;
            (e.rect, e.session)
        };
        let Some(win) = self.app().get_webview_window(FLOAT_LABEL) else {
            return;
        };
        // 面板以「固定」原因隐藏，不会自己还原焦点：交接之后由这里交还（见 hand_focus_back）
        let prev = session.and_then(|id| self.panel_prev_app(id));
        let hiding = session.filter(|&id| self.panel_controller().begin_external_hide_for(id));
        let panel = hiding.and_then(|_| self.app().get_webview_window(PANEL_LABEL));
        let shown = native::float_handoff(&win, panel.as_ref(), rect);
        if let Some(id) = hiding {
            if shown.is_err() {
                // 交接中途失败：面板未必已经离屏，照常走退出流程
                self.panel_controller().request_hide_for(id, HideReason::Pin);
            }
            self.panel_controller().finish_external_hide(id, HideReason::Pin);
        }
        if hiding.is_some() {
            self.hand_focus_back(prev);
        }
        match shown {
            Ok(()) => {
                *self.float().frame.lock() = Some(rect);
                self.note_own_rect(FLOAT_LABEL, Some(rect));
                if let Some(state) = self.app().try_state::<AppState>() {
                    state.sync.trigger(crate::sync_svc::CycleReason::ClipboardShown);
                }
            }
            Err(e) => log::warn!("floating clipboard handoff failed: {}", e.message),
        }
    }

    /// 关闭（销毁）悬浮剪贴板；没开着时什么都不做
    pub fn float_close(&self) {
        if let Some(win) = self.app().get_webview_window(FLOAT_LABEL) {
            native::destroy_window(&win);
        }
    }

    /// 置顶 / 取消置顶，并记为下次打开的默认值
    pub fn float_set_on_top(&self, on_top: bool) -> Result<(), IpcError> {
        let win = self
            .app()
            .get_webview_window(FLOAT_LABEL)
            .ok_or_else(|| IpcError::not_found("floating clipboard"))?;
        native::float_set_on_top(&win, on_top)?;
        if let Some(e) = self.float().entry.lock().as_mut() {
            e.on_top = on_top;
        }
        self.settings()
            .state_arc()
            .update_deferred(move |s| s.clip_float.on_top = on_top);
        Ok(())
    }

    /// 搜索框获得 / 失去焦点（以及 Esc 交还焦点）：临时可聚焦 / 恢复（见 `native::float_set_focusable`）；获得时顺带
    /// 同步拉取。
    ///
    /// Windows / X11 上恢复不可聚焦只改窗口样式，不会交出前台：悬浮窗仍在前台时之后的键入照样落进它的搜索框。所以
    /// 可聚焦之前记下当时的前台应用，恢复时悬浮窗还在前台就把前台交还给它（macOS 的 `float_set_focusable(false)`
    /// 自己退出 key）
    pub fn float_focus(&self, focused: bool) -> Result<(), IpcError> {
        let Some(win) = self.app().get_webview_window(FLOAT_LABEL) else {
            return Ok(());
        };
        let state = self.app().try_state::<AppState>();
        if focused {
            if native::RESTORE_FOCUS_ON_HIDE
                && let Some(state) = state.as_ref()
                && let Some(front) = state.platform.focus.frontmost()
            {
                *self.float().prev.lock() = Some(front);
            }
            native::float_set_focusable(&win, true)?;
            if let Some(state) = state {
                state.sync.trigger(crate::sync_svc::CycleReason::ClipboardShown);
            }
            return Ok(());
        }
        let foreground = native::RESTORE_FOCUS_ON_HIDE && win.is_focused().unwrap_or(false);
        native::float_set_focusable(&win, false)?;
        let prev = self.float().prev.lock().take();
        if foreground {
            self.hand_focus_back(prev.or_else(|| state.and_then(|s| s.platform.focus.frontmost())));
        }
        Ok(())
    }

    /// 指针移入悬浮窗：同步拉取（同步层自己节流，O(1)）
    pub fn float_hover(&self) {
        if let Some(state) = self.app().try_state::<AppState>() {
            state.sync.trigger(crate::sync_svc::CycleReason::ClipboardShown);
        }
    }

    /// 页面已订阅事件（粘贴管线、事件投递用）
    pub fn float_ready(&self) -> bool {
        self.float().ready()
    }

    /// 粘贴管线发键之前：把键盘焦点交还目标应用（悬浮窗没在屏幕上时什么都不用做）。返回 false 时只复制。**会阻塞**
    pub fn float_yield(&self, target: &wisp_types::AppRef) -> bool {
        if !self.float().shown() {
            return true;
        }
        match self.app().get_webview_window(FLOAT_LABEL) {
            Some(win) => native::float_yield(&win, target),
            None => true,
        }
    }

    /// 截图前：在屏幕上的悬浮窗立即离屏，返回是否隐藏了
    pub(crate) fn float_hide_for_capture(&self) -> bool {
        if !self.float().shown() {
            return false;
        }
        let Some(win) = self.app().get_webview_window(FLOAT_LABEL) else {
            return false;
        };
        if !win.is_visible().unwrap_or(false) || win.is_minimized().unwrap_or(false) {
            return false;
        }
        match native::float_hide(&win) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("hide floating clipboard for capture failed: {}", e.message);
                false
            }
        }
    }

    /// 截图后恢复（不抢焦点）
    pub(crate) fn float_restore_after_capture(&self) {
        if let Some(win) = self.app().get_webview_window(FLOAT_LABEL)
            && let Err(e) = native::float_show(&win)
        {
            log::warn!("restore floating clipboard failed: {}", e.message);
        }
    }
}

/// 把窗口外框记进 state.json（记的是卡片矩形，见 [`card_geometry_in`]；延迟写盘，退出时的 flush 会写掉）。
/// **会阻塞**（查询显示器）
fn store_geometry(state: &AppState, frame: Rect) {
    let screens: Vec<Screen> = state.platform.screen.monitors().iter().map(Screen::from_info).collect();
    let Some(geometry) = card_geometry_in(&screens, frame, native::DESKTOP_UNIT, shadow_margin()) else {
        return;
    };
    state
        .settings
        .state_arc()
        .update_deferred(move |s| s.clip_float.geometry = Some(geometry.clone()));
}

#[cfg(test)]
mod tests {
    use wisp_types::PhysPoint;

    use super::*;

    fn screen() -> Screen {
        Screen {
            id: "1".to_owned(),
            frame: Rect::new(0.0, 0.0, 1440.0, 900.0),
            work: Rect::new(0.0, 25.0, 1440.0, 805.0),
            scale: 2.0,
            physical_origin: PhysPoint { x: 0, y: 0 },
            primary: true,
        }
    }

    #[test]
    fn opens_at_the_panel_with_room_for_a_screenful() {
        let s = screen();
        // 矮面板（只有几条）：悬浮窗至少放得下一屏条目
        let c = card_from_panel(&s, Rect::new(500.0, 100.0, 420.0, 200.0));
        assert_eq!((c.x, c.y, c.width), (500.0, 100.0, 420.0));
        assert!((c.height - FLOAT_OPEN_HEIGHT).abs() < 1e-6);
        // 靠近底边时整体夹回工作区，不改尺寸
        let c = card_from_panel(&s, Rect::new(500.0, 700.0, 420.0, 300.0));
        assert!(c.y + c.height <= s.work.y + s.work.height + 1e-6);
        assert!((c.height - FLOAT_OPEN_HEIGHT).abs() < 1e-6);
    }

    #[test]
    fn remembered_geometry_needs_its_monitor() {
        let screens = [screen()];
        let saved = WindowGeometry {
            monitor_id: "1".to_owned(),
            x: 100.0,
            y: 120.0,
            width: 200.0,
            height: 100.0,
        };
        let r = restorable_rect(&screens, &saved).expect("restorable");
        let local = screens[0].from_desktop(r, native::DESKTOP_UNIT);
        assert!(
            local.width >= FLOAT_MIN_SIZE.0 && local.height >= FLOAT_MIN_SIZE.1,
            "不小于最小尺寸"
        );
        let gone = WindowGeometry {
            monitor_id: "9".to_owned(),
            ..saved
        };
        assert!(restorable_rect(&screens, &gone).is_none(), "原显示器已断开");
    }

    #[test]
    fn remembered_geometry_round_trips_with_the_shadow_margin() {
        // Linux 透明窗口：桌面坐标是物理像素（2 倍缩放），阴影边距是逻辑像素。记住 → 恢复 → 再记住，位置与大小不变
        let screens = [screen()];
        let saved = WindowGeometry {
            monitor_id: "1".to_owned(),
            x: 300.0,
            y: 200.0,
            width: 420.0,
            height: 480.0,
        };
        for unit in [DesktopUnit::Physical, DesktopUnit::Logical] {
            let window = restorable_rect_in(&screens, &saved, unit, SHADOW_MARGIN).expect("restorable");
            let local = screens[0].from_desktop(window, unit);
            assert!(
                (local.width - (saved.width + 2.0 * SHADOW_MARGIN)).abs() < 1e-6,
                "{unit:?}：恢复时加回阴影边距"
            );
            let again = card_geometry_in(&screens, window, unit, SHADOW_MARGIN).expect("geometry");
            assert_eq!(again, saved, "{unit:?}");
        }
        // 没有阴影边距的平台：窗口就是卡片
        let window = restorable_rect_in(&screens, &saved, DesktopUnit::Physical, 0.0).expect("restorable");
        assert_eq!(
            card_geometry_in(&screens, window, DesktopUnit::Physical, 0.0),
            Some(saved)
        );
    }

    #[test]
    fn readiness_follows_the_page() {
        let f = FloatWindow::new();
        assert!(!f.ready() && !f.shown());
        *f.entry.lock() = Some(Entry {
            seed: FloatSeed {
                query: "q".to_owned(),
                filter: ClipFilter::Favorite,
                selected_id: Some(3),
            },
            session: Some(7),
            rect: Rect::new(0.0, 0.0, 420.0, 420.0),
            ready: false,
            shown: false,
            on_top: true,
        });
        assert!(!f.ready(), "建好之后、页面订阅之前不推送");
        f.entry.lock().as_mut().expect("entry").ready = true;
        assert!(f.ready());
        assert!(f.remove().is_none());
        assert!(!f.ready(), "销毁之后不再推送");
    }
}
