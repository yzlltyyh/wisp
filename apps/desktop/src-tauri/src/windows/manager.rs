//! `WindowManager`：全部窗口的创建、显示时序与几何。服务之间不互相持有：面板显示管线在 panel.rs
//! （[`PanelController`]），定位在 placement.rs，固定窗口在 pinned.rs，Overlay 在 overlay.rs，设置 / 引导窗口在
//! settings_window.rs，平台差异全部在 native/。本文件负责把请求翻译成几何与策略（视图宽度、锚点、关闭监听、焦点），
//! 并维护「自身窗口矩形表」（浮标判定、截图前隐藏）。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use tauri::{Emitter, Manager};
pub use wisp_types::FrameMode;
use wisp_types::{
    Anchor, AppRef, AppearanceState, Boot, HideReason, MonitorId, MonitorInfo, OsKind, OverlayMode, PanelPayload,
    PanelShown, PanelTransition, PanelView, PinState, Point, Rect, ReduceMotionPref, ToastPayload, TranslateSettings,
    WindowTarget, event,
};

use super::native::{self, CardSurface, TemplateKind};
use super::outside_click::GuardSpec;
use super::overlay::OverlayRegistry;
use super::panel::{
    BootProvider, PANEL_LABEL, PanelController, PanelHost, PanelOpen, PanelOptions, PerfHook, PerfNote, TauriPanelHost,
};
use super::pinned::{self, PinRequest, PinnedRegistry};
use super::placement::{self, PlaceAnchor, Screen, initial_height};
use super::sizing::SHADOW_MARGIN;
use crate::app::perf::PerfMarks;
use crate::error::IpcError;
use crate::settings::SettingsService;
use crate::state::{AppState, Deps};

/// 面板显示请求
#[derive(Debug, Clone, PartialEq)]
pub struct PanelShowRequest {
    pub view: PanelView,
    pub payload: PanelPayload,
    pub anchor: AnchorSource,
    pub take_focus: bool,
    pub prev_app: Option<AppRef>,
    pub transition: PanelTransition,
}

/// 定位锚点来源（优先级：选区矩形 > 鼠标位置 > 屏幕中心）
#[derive(Debug, Clone, PartialEq)]
pub enum AnchorSource {
    Selection(Anchor),
    /// OCR 框选区域
    Rect(Anchor),
    Cursor,
    Center,
    Remembered,
}

/// 面板当前状态
#[derive(Debug, Clone, PartialEq)]
pub struct PanelState {
    pub session_id: u64,
    pub view: PanelView,
    pub monitor_id: MonitorId,
    pub rect: Rect,
    pub key: bool,
}

/// 截图前被隐藏的窗口
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HiddenSet {
    pub panel: bool,
    pub pinned: Vec<String>,
    /// 悬浮剪贴板被隐藏了
    pub float: bool,
}

/// 一次截图会话
#[derive(Debug, Clone, PartialEq)]
pub struct OverlaySession {
    pub session_id: u64,
    pub monitors: Vec<MonitorInfo>,
    pub mode: OverlayMode,
}

/// 截图前隐藏面板时最多等它离屏这么久
const CAPTURE_HIDE_WAIT: Duration = Duration::from_millis(300);
/// 截图会话（`hide_for_capture` → `restore_after_capture`）最多算「在忙」这么久：万一某条结束路径漏调了
/// `restore_after_capture`，面板回收也只会被推迟这么久，不会永远停掉
const CAPTURE_BUSY_MAX: Duration = Duration::from_secs(120);
/// 设置 / 引导窗口打开之后多久内算「在忙」：建窗口、无白闪首显与进入动画
const OPEN_SETTLE: Duration = Duration::from_secs(3);
/// 新建 pinned 之后多久内算「在忙」：交接、展开过渡，以及交接过去的流接着输出（一次翻译的流式输出通常在这之内结束）
const PINNED_SETTLE: Duration = Duration::from_secs(30);
/// pinned / 设置 / 引导窗口被移动（拖动）之后多久内算「在忙」：拖动期间每次移动都会续上
const MOVE_SETTLE: Duration = Duration::from_secs(2);

/// 面板回收推迟判定的输入：此刻其他 Wisp 窗口的短时活动（[`busy_reason`]）
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct BusyProbe {
    /// 进行中的截图会话已经开始了多久
    pub capture_for: Option<Duration>,
    /// 显示着的 Overlay（label）
    pub overlay: Option<String>,
    /// 有窗口在跑原生逐帧动画（macOS 的 displayLink：pinned 的展开过渡等）
    pub animating: bool,
    /// 浮标显示着（选中文字后短暂出现）
    pub bobber: bool,
    /// 最近一次短时活动（刚打开的设置 / 引导窗口、刚建的 pinned、拖动）：原因与还要多久结束（已结束的为 0，不算忙）
    pub activity: Option<(String, Duration)>,
}

/// 面板回收此刻是否要推迟，返回原因（按下面的顺序取第一条）。只看**短时**活动：截图（最多算 [`CAPTURE_BUSY_MAX`]）、
/// Overlay、原生逐帧动画、浮标、刚打开的设置 / 引导窗口、刚建的或正在拖动的 pinned。长时间留在屏幕上的 pinned、开着的
/// 设置窗口本身**不算**——否则常用 pinned 的用户的面板永远回收不了，推迟的计时器也会一直每 15 s 醒一次。推迟的总时长
/// 另由 panel.rs 的 `RECYCLE_DEFER_MAX` 封顶
pub(crate) fn busy_reason(p: &BusyProbe) -> Option<String> {
    if p.capture_for.is_some_and(|d| d < CAPTURE_BUSY_MAX) {
        return Some("capture in progress".to_owned());
    }
    if let Some(label) = &p.overlay {
        return Some(format!("{label} shown"));
    }
    if p.animating {
        return Some("native animation running".to_owned());
    }
    if p.bobber {
        return Some("bobber shown".to_owned());
    }
    p.activity
        .as_ref()
        .filter(|(_, left)| !left.is_zero())
        .map(|(what, left)| format!("{what}, settles in {} ms", left.as_millis()))
}

/// 设置里各宽度的合法范围（`Appearance` 的注释：翻译最小 320–480、最大 440–720、剪贴板 360–560）
const TRANSLATE_MIN_RANGE: (u32, u32) = (320, 480);
const TRANSLATE_MAX_RANGE: (u32, u32) = (440, 720);
const CLIPBOARD_WIDTH_RANGE: (u32, u32) = (360, 560);
/// 「鼠标离开 N px」的合法范围（`translate.mouse_leave_px` 80–400）
const MOUSE_LEAVE_RANGE: (u32, u32) = (80, 400);
/// 没有起始矩形时 pinned 的默认尺寸
const PINNED_DEFAULT_SIZE: (f64, f64) = (420.0, 240.0);

/// 窗口管理器
pub struct WindowManager {
    app: tauri::AppHandle,
    settings: Arc<SettingsService>,
    platform: Arc<wisp_platform::Platform>,
    frame_mode: FrameMode,
    next_session: Arc<AtomicU64>,
    /// 面板的真实宿主（外观变化时更新原生卡片表面）
    host: Arc<TauriPanelHost>,
    /// 面板显示管线（见 panel.rs）
    panel: Arc<PanelController>,
    pinned: PinnedRegistry,
    /// 悬浮剪贴板（float.rs）
    float: super::float::FloatWindow,
    overlays: OverlayRegistry,
    /// 最近一次取到的显示器（`self_hit` 在平台钩子线程上调用，不能现场查询）
    screens: RwLock<Vec<Screen>>,
    /// 自身窗口矩形表（pinned / settings / onboarding，桌面坐标；浮标判定「落在 Wisp 窗口内」用）
    own_rects: Mutex<HashMap<String, Rect>>,
    /// 进行中的截图会话从什么时候开始（`hide_for_capture` 记下、`restore_after_capture` 清掉；面板回收据此推迟）
    capture_since: Mutex<Option<std::time::Instant>>,
    /// 其他窗口最近一次短时活动：原因与结束时刻（[`Self::note_activity`]；面板回收据此推迟）
    activity: Mutex<Option<(String, std::time::Instant)>>,
    /// 面板帧模式实验台（debug 构建 + `WISP_FRAME_LAB`，见 frame_lab.rs）
    #[cfg(any(debug_assertions, feature = "fake-llm"))]
    lab: Option<super::frame_lab::LabConfig>,
}

/// Css 帧模式且窗口能透明时卡片四周的阴影边距；Native 帧模式（窗口即卡片）或窗口不能透明（Linux Wayland 等，
/// 卡片铺满窗口）时为 0。`Boot.shadow_margin` 与面板几何同源
fn shadow_margin(frame_mode: FrameMode) -> f64 {
    match frame_mode {
        FrameMode::Css if crate::app::runtime::transparent_windows() => SHADOW_MARGIN,
        FrameMode::Css | FrameMode::Native => 0.0,
    }
}

/// 某个窗口的 boot 阴影边距：面板按帧模式（见 [`shadow_margin`]）；pinned 只在它由 CSS 画圆角阴影时（Linux 透明）
/// 才有；其他窗口没有卡片外的透明边距
fn boot_shadow_margin(frame_mode: FrameMode, label: &str) -> f64 {
    match TemplateKind::of(label) {
        TemplateKind::Panel => shadow_margin(frame_mode),
        TemplateKind::Pinned | TemplateKind::Floating if native::pinned_css_shadow() => SHADOW_MARGIN,
        TemplateKind::Pinned | TemplateKind::Floating | TemplateKind::Regular | TemplateKind::Overlay => 0.0,
    }
}

/// 生成某个窗口的 Boot：外观取当前解析值；`dev` = 开发者模式 || debug 构建
fn make_boot(
    app: &tauri::AppHandle,
    settings: &SettingsService,
    frame_mode: FrameMode,
    label: &str,
    route: Option<WindowTarget>,
) -> Boot {
    let a = crate::app::appearance::current(app);
    let debug_build = cfg!(debug_assertions);
    let developer_mode = settings.get().general.developer_mode;
    Boot {
        label: label.to_owned(),
        os: a.os,
        os_version: crate::app::runtime::os_version().to_owned(),
        theme: a.theme,
        accent: a.accent,
        reduce_motion: a.reduce_motion,
        reduce_transparency: a.reduce_transparency,
        material: a.material,
        frame_mode,
        shadow_margin: boot_shadow_margin(frame_mode, label),
        font_size: a.font_size,
        ui_language: a.ui_language,
        dev: developer_mode || debug_build,
        debug_build,
        route,
        // 面板冷唤出时由 TauriPanelHost::create 填入暂存的 prepare
        prepare: None,
    }
}

/// 按面板 session 记录性能阶段（Ready / WindowShown / KeyGained / Shown / ReadyTimeout）与本次唤出用的页面。
/// 编排器在 `panel_prepare` 拿到 session 后把它与触发时的 trace 关联（`PerfMarks::bind_session`），这里只知道 session
fn perf_mark(perf: &PerfMarks, session_id: u64, note: PerfNote) {
    match note {
        PerfNote::Stage(stage) => perf.mark_session(session_id, stage),
        PerfNote::Page(page) => perf.set_page_for_session(session_id, page),
    }
}

/// 设置 → 面板运行期选项
fn panel_options(settings: &wisp_types::Settings, system_reduce_motion: bool) -> PanelOptions {
    let reduce_motion = match settings.appearance.reduce_motion {
        ReduceMotionPref::On => true,
        ReduceMotionPref::Off => false,
        ReduceMotionPref::System => system_reduce_motion,
    };
    PanelOptions {
        reduce_motion,
        low_memory_when_hidden: settings.advanced.webview_low_memory_when_hidden,
        unload_after: settings
            .advanced
            .unload_panel_after_idle_min
            .map(|m| Duration::from_secs(u64::from(m.clamp(1, 24 * 60)) * 60)),
        recycle_after: panel_recycle_after(),
    }
}

/// 面板回收的延迟（空闲卸载的变体，见 `PanelOptions::recycle_after`）：平台默认值 macOS 45 s、Windows / Linux
/// None。debug 构建可以用 `WISP_PANEL_RECYCLE_SECS=<秒>` 覆盖（测量会话后内存与
/// 回收后首次唤出的延迟用；`0` / `off` 关闭），发布版始终按平台默认值
fn panel_recycle_after() -> Option<Duration> {
    #[cfg(any(debug_assertions, feature = "fake-llm"))]
    {
        static OVERRIDE: std::sync::OnceLock<Option<Option<Duration>>> = std::sync::OnceLock::new();
        let parsed = OVERRIDE.get_or_init(|| {
            let v = std::env::var("WISP_PANEL_RECYCLE_SECS").ok()?;
            match v.trim() {
                "0" | "off" => Some(None),
                secs => match secs.parse::<u64>() {
                    Ok(n) => {
                        log::info!("panel recycle enabled by WISP_PANEL_RECYCLE_SECS={n}");
                        Some(Some(Duration::from_secs(n)))
                    }
                    Err(_) => {
                        log::warn!("WISP_PANEL_RECYCLE_SECS={secs} 无法识别（秒数 | 0 | off），忽略");
                        None
                    }
                },
            }
        });
        if let Some(v) = *parsed {
            return v;
        }
    }
    native::PANEL_RECYCLE_AFTER
}

/// 视图的关闭监听
fn guard_spec(view: PanelView, t: &TranslateSettings) -> GuardSpec {
    match view {
        PanelView::Translate | PanelView::Ocr => GuardSpec {
            outside_click: t.close_on_click_outside,
            esc: true,
            mouse_leave_px: (view == PanelView::Translate && t.close_on_mouse_leave)
                .then(|| f64::from(t.mouse_leave_px.clamp(MOUSE_LEAVE_RANGE.0, MOUSE_LEAVE_RANGE.1))),
            blur_hides: false,
        },
        PanelView::Input | PanelView::Clipboard => GuardSpec {
            blur_hides: true,
            ..GuardSpec::default()
        },
        // toast：不激活、不注册 Esc 与外部点击，点击即关闭、1.6 s 后前端淡出
        PanelView::Toast => GuardSpec::default(),
    }
}

/// 视图宽度
fn view_width(view: PanelView, payload: &PanelPayload, s: &wisp_types::Settings) -> f64 {
    let a = &s.appearance;
    match (view, payload) {
        (PanelView::Translate, PanelPayload::Translate(t)) => {
            let min = a
                .translate_min_width
                .clamp(TRANSLATE_MIN_RANGE.0, TRANSLATE_MIN_RANGE.1);
            let max = a
                .translate_max_width
                .clamp(TRANSLATE_MAX_RANGE.0, TRANSLATE_MAX_RANGE.1);
            placement::translate_width(&t.source_text, f64::from(min), f64::from(max), t.dict_mode)
        }
        (PanelView::Translate, _) => f64::from(
            a.translate_min_width
                .clamp(TRANSLATE_MIN_RANGE.0, TRANSLATE_MIN_RANGE.1),
        ),
        (PanelView::Ocr, _) => placement::OCR_WIDTH,
        (PanelView::Input, _) => placement::INPUT_WIDTH,
        (PanelView::Clipboard, _) => f64::from(
            a.clipboard_width
                .clamp(CLIPBOARD_WIDTH_RANGE.0, CLIPBOARD_WIDTH_RANGE.1),
        ),
        (PanelView::Toast, PanelPayload::Toast(t)) => placement::toast_width(&t.paste_key),
        (PanelView::Toast, _) => placement::TOAST_MIN_WIDTH,
    }
}

/// 视图的初始估计高度
fn view_initial_height(view: PanelView) -> f64 {
    match view {
        PanelView::Translate => initial_height::TRANSLATE,
        PanelView::Ocr => initial_height::OCR,
        PanelView::Input => initial_height::INPUT,
        PanelView::Clipboard => initial_height::CLIPBOARD,
        PanelView::Toast => initial_height::TOAST,
    }
}

impl WindowManager {
    /// 构造（不创建任何窗口）
    pub fn new(d: &Deps) -> Arc<Self> {
        // macOS = Native（窗口即卡片，原生材质只画在卡片矩形内）；其他平台 = Css：Windows 的 Acrylic 作用于整个 HWND、
        // Win11 的瞬态材质只在窗口激活时渲染（翻译 / OCR 视图从不激活），都做不到「材质只在卡片里、透明区不拦截点击」
        let frame_mode = if OsKind::current() == OsKind::Macos {
            FrameMode::Native
        } else {
            FrameMode::Css
        };
        #[cfg(any(debug_assertions, feature = "fake-llm"))]
        let lab = super::frame_lab::LabConfig::from_env();
        #[cfg(any(debug_assertions, feature = "fake-llm"))]
        let frame_mode = lab.map_or(frame_mode, |l| l.frame_mode);
        // window lab：真实应用换成另一种帧模式，对照两种帧模式里同一个交互的高度轨迹（菜单预留的 snappy）
        #[cfg(any(debug_assertions, feature = "fake-llm"))]
        let frame_mode = match std::env::var("WISP_WINDOW_LAB_FRAME_MODE").as_deref() {
            Ok("css") => FrameMode::Css,
            Ok("native") => FrameMode::Native,
            _ => frame_mode,
        };

        let boot: BootProvider = {
            let app = d.app.clone();
            let settings = Arc::clone(&d.settings);
            Arc::new(move || {
                let boot = make_boot(&app, &settings, frame_mode, PANEL_LABEL, None);
                let surface = CardSurface::resolve(boot.theme, boot.material);
                (boot, surface)
            })
        };
        let perf: PerfHook = {
            let perf = Arc::clone(&d.perf);
            Arc::new(move |session_id, note| perf_mark(&perf, session_id, note))
        };
        let host = Arc::new(TauriPanelHost::new(d.app.clone(), frame_mode, boot, perf));
        let opaque = !crate::app::runtime::transparent_windows();
        let panel = PanelController::new(
            Arc::clone(&host) as Arc<dyn PanelHost>,
            frame_mode,
            shadow_margin(frame_mode),
            opaque,
            native::DESKTOP_UNIT,
        );
        let settings = d.settings.get();
        let system_reduce =
            settings.appearance.reduce_motion == ReduceMotionPref::System && d.platform.screen.reduce_motion();
        panel.set_options(panel_options(&settings, system_reduce));
        let wm = Arc::new(Self {
            app: d.app.clone(),
            settings: Arc::clone(&d.settings),
            platform: Arc::clone(&d.platform),
            frame_mode,
            next_session: Arc::new(AtomicU64::new(0)),
            host,
            panel,
            pinned: PinnedRegistry::new(),
            float: super::float::FloatWindow::new(),
            overlays: OverlayRegistry::new(),
            screens: RwLock::new(Vec::new()),
            own_rects: Mutex::new(HashMap::new()),
            capture_since: Mutex::new(None),
            activity: Mutex::new(None),
            #[cfg(any(debug_assertions, feature = "fake-llm"))]
            lab,
        });
        let me = Arc::downgrade(&wm);
        wm.host
            .set_busy_probe(Box::new(move || me.upgrade().and_then(|wm| wm.others_busy())));
        wm.watch_settings();
        wm
    }

    /// 面板以外的 Wisp 窗口此刻有没有短时活动，返回原因（面板回收据此推迟，见 `PanelHost::busy` 与 [`busy_reason`]）。
    /// 只读几个锁与原子量，不等主线程
    pub(crate) fn others_busy(&self) -> Option<String> {
        let now = std::time::Instant::now();
        let probe = BusyProbe {
            capture_for: self.capture_since.lock().map(|t| now.saturating_duration_since(t)),
            overlay: self.overlays.shown_labels().into_iter().next(),
            animating: native::animating(),
            bobber: self.bobber_shown(),
            activity: self
                .activity
                .lock()
                .as_ref()
                .map(|(what, until)| (what.clone(), until.saturating_duration_since(now))),
        };
        busy_reason(&probe)
    }

    /// 记下其他窗口的一次短时活动，`lasts` 之后结束（与已记下的取结束得晚的那个）
    pub(super) fn note_activity(&self, what: impl Into<String>, lasts: Duration) {
        let until = std::time::Instant::now() + lasts;
        let mut g = self.activity.lock();
        if g.as_ref().is_none_or(|(_, u)| *u < until) {
            *g = Some((what.into(), until));
        }
    }

    /// 设置变化时更新面板选项（事件驱动：watch 通道，没有轮询）
    fn watch_settings(self: &Arc<Self>) {
        let mut rx = self.settings.subscribe();
        let me = Arc::downgrade(self);
        tauri::async_runtime::spawn(async move {
            while rx.changed().await.is_ok() {
                let settings = rx.borrow_and_update().clone();
                let Some(me) = me.upgrade() else { return };
                let system_reduce =
                    settings.appearance.reduce_motion == ReduceMotionPref::System && me.platform.screen.reduce_motion();
                me.panel.set_options(panel_options(&settings, system_reduce));
            }
        });
    }

    /// macOS = Native，其他平台 = Css
    pub fn frame_mode(&self) -> FrameMode {
        self.frame_mode
    }

    pub(super) fn app(&self) -> &tauri::AppHandle {
        &self.app
    }

    pub(super) fn settings(&self) -> &Arc<SettingsService> {
        &self.settings
    }

    pub(super) fn float(&self) -> &super::float::FloatWindow {
        &self.float
    }

    /// 重新取一次显示器（悬浮窗恢复记住的位置用）
    pub(super) fn screens_now(&self) -> Vec<Screen> {
        self.refresh_screens()
    }

    /// 托盘就绪后异步预热隐藏的 Panel：按模板隐藏创建，macOS 转为非激活 NSPanel，Native 帧模式装上原生卡片表面
    pub fn prewarm_panel(&self) -> Result<(), IpcError> {
        #[cfg(any(debug_assertions, feature = "fake-llm"))]
        if let Some(lab) = self.lab {
            self.host.set_lab(lab.init_script(), Arc::new(move |s| lab.surface(s)));
            self.panel.create()?;
            let counter = Arc::clone(&self.next_session);
            super::frame_lab::spawn(lab, self.app.clone(), Arc::clone(&self.panel), move || {
                counter.fetch_add(1, Ordering::Relaxed) + 1
            });
            return Ok(());
        }
        #[cfg(any(debug_assertions, feature = "fake-llm"))]
        if super::window_lab::enabled()
            && let Some(script) = super::window_lab::panel_init_script()
        {
            // 实验台的 A/B（预热开 / 关的内存对比）：面板页面的附加初始化脚本，原生表面不改
            self.host.set_lab(script, Arc::new(|s| s));
        }
        self.panel.create()?;
        #[cfg(any(debug_assertions, feature = "fake-llm"))]
        if super::window_lab::enabled() {
            super::window_lab::spawn(self.app.clone());
        }
        Ok(())
    }

    /// window lab：接下来 `hold` 之内不发 `panel:prepare`（模拟冷页面上监听迟迟没注册，见 `TauriPanelHost::lab_hold_prepare`）
    #[cfg(any(debug_assertions, feature = "fake-llm"))]
    pub fn lab_hold_prepare(&self, hold: std::time::Duration) {
        self.host.lab_hold_prepare(hold);
    }

    /// window lab：之后新建的面板页面 Boot 里带不带暂存的 prepare（冷唤出 A/B）
    #[cfg(any(debug_assertions, feature = "fake-llm"))]
    pub fn lab_set_boot_prepare(&self, enabled: bool) {
        self.host.lab_set_boot_prepare(enabled);
    }

    /// 空闲时预建隐藏的主显示器 Overlay（Windows 默认开；macOS / Linux 不预建，返回 Ok）
    pub fn prewarm_overlay(&self) -> Result<(), IpcError> {
        if !native::PREWARM_OVERLAY {
            return Ok(());
        }
        let monitors = self.platform.screen.monitors();
        let primary = monitors
            .iter()
            .find(|m| m.is_primary)
            .or_else(|| monitors.first())
            .ok_or_else(|| IpcError::not_found("no monitor"))?;
        self.overlays
            .prewarm(&self.app, &|label| self.boot_for(label, None), primary)
    }

    /// 刷新显示器缓存并返回。平台层给不出显示器时（Wayland：拿不到全局布局）退回 Tauri 的显示器接口，
    /// 面板仍能以输入模式居中显示（Wayland 的降级）
    fn refresh_screens(&self) -> Vec<Screen> {
        let mut screens: Vec<Screen> = self.platform.screen.monitors().iter().map(Screen::from_info).collect();
        if screens.is_empty() {
            screens = tauri_screens(&self.app);
        }
        if !screens.is_empty() {
            self.screens.write().clone_from(&screens);
        }
        screens
    }

    /// 光标所在的屏幕与光标位置（本屏逻辑坐标）
    fn cursor_screen(&self, screens: &[Screen]) -> Option<(Screen, Point)> {
        let (monitor, p) = self.platform.screen.cursor();
        placement::screen_by_id(screens, &monitor).map(|s| (s.clone(), p))
    }

    /// 光标锚点：光标所在屏幕 + 光标下方的矩形。光标取不到（Wayland 下 `cursor()` 的显示器 id 为空）时退回主显示器
    /// 居中
    fn cursor_anchor(&self, screens: &[Screen]) -> Option<(Screen, PlaceAnchor)> {
        self.cursor_screen(screens)
            .map(|(s, p)| (s, PlaceAnchor::Rect(placement::cursor_rect(p))))
            .or_else(|| primary_screen(screens).map(|s| (s, PlaceAnchor::Center)))
    }

    /// 锚点 → 屏幕与放置锚点
    fn resolve_anchor(&self, anchor: &AnchorSource, screens: &[Screen]) -> Option<(Screen, PlaceAnchor)> {
        match anchor {
            AnchorSource::Selection(a) | AnchorSource::Rect(a) => match screens.iter().find(|s| s.id == a.monitor_id) {
                Some(s) => Some((s.clone(), PlaceAnchor::Rect(a.rect))),
                // 锚点所在显示器已经断开（取词与显示之间拔掉了外接屏）：按锚点的优先级退回光标锚点
                None => self.cursor_anchor(screens),
            },
            AnchorSource::Cursor => self.cursor_anchor(screens),
            AnchorSource::Center => self
                .cursor_screen(screens)
                .map(|(s, _)| s)
                .or_else(|| primary_screen(screens))
                .map(|s| (s, PlaceAnchor::Center)),
            AnchorSource::Remembered => self
                .cursor_screen(screens)
                .map(|(s, _)| s)
                .or_else(|| primary_screen(screens))
                .map(|s| {
                    let saved = self.settings.state().get().clipboard_positions.get(&s.id).copied();
                    match saved {
                        Some(p) => (s, PlaceAnchor::At(Point { x: p.x, y: p.y })),
                        None => (s, PlaceAnchor::Center),
                    }
                }),
        }
    }

    /// 返回 session_id。顺序：定位（隐藏状态）→ Windows 先 SetIsVisible(true) + 内存目标 NORMAL → emit panel:prepare →
    /// 启动 40 ms ready 超时。视图宽度在显示前确定并在会话内锁定，上限为工作区 × `max_height_ratio`
    /// 与所选一侧可用空间中的较小者。
    ///
    /// 找不到任何显示器（平台层与 Tauri 都给不出）时什么都不做、返回 None：没有会话 id，也就没有要收尾的东西。
    ///
    /// **会阻塞**：刷新显示器缓存（macOS 缓存对不上时同步等主线程 ≤ 150 ms）、隐藏状态下定位与固定 WebView（macOS 两次
    /// 主线程往返，每次最多 500 ms），在阻塞线程上调用（`spawn_blocking`），不要在 tokio worker 上直接调用
    pub fn panel_prepare(&self, req: PanelShowRequest) -> Option<u64> {
        let settings = self.settings.get();
        let screens = self.refresh_screens();
        let Some((screen, anchor)) = self.resolve_anchor(&req.anchor, &screens) else {
            log::error!("panel_prepare view={:?}: no monitor available", req.view);
            return None;
        };
        let session_id = self.next_session.fetch_add(1, Ordering::Relaxed) + 1;
        let width = view_width(req.view, &req.payload, &settings);
        let mut cap = placement::height_cap(&screen, f64::from(settings.appearance.max_height_ratio));
        if req.view == PanelView::Clipboard {
            cap = cap.min(placement::CLIPBOARD_MAX_HEIGHT);
        }
        let initial = match &req.payload {
            // 剪贴板按首屏条目预估打开高度：居中放置时卡片中心落在工作区 1/3 处
            PanelPayload::Clipboard(c) => placement::clipboard_estimated_height(c, cap),
            _ => view_initial_height(req.view),
        };
        let geometry = placement::place_panel(&screen, anchor, width, initial, cap);
        let view = req.view;
        log::debug!(
            "panel_prepare session={session_id} view={:?} placement={:?} width={} max_height={}",
            req.view,
            geometry.placement,
            geometry.width,
            geometry.max_height
        );
        self.panel.prepare(PanelOpen {
            session_id,
            view: req.view,
            payload: req.payload,
            geometry,
            focused: req.take_focus,
            transition: req.transition,
            estimated_height: initial,
            prev_app: req.prev_app,
            guards: guard_spec(req.view, &settings.translate),
        });
        // 面板显示时顺带在后台刷新授权状态与惰性检查更新：都只是投递一个后台任务，
        // 放在 prepare 之后，不占显示的关键路径
        crate::app::permissions::refresh(&self.app);
        crate::updater::maybe_check(&self.app);
        // 同步：用户正要看数据，取回其他设备的改动（O(1)：节流与退避都只是原子读；剪贴板视图的间隔更短，并先应用
        // 剪贴板的改动）。轻提示不算「在看」
        let reason = match view {
            PanelView::Clipboard => Some(crate::sync_svc::CycleReason::ClipboardShown),
            PanelView::Translate | PanelView::Ocr | PanelView::Input => Some(crate::sync_svc::CycleReason::PanelShown),
            PanelView::Toast => None,
        };
        if let Some(reason) = reason
            && let Some(state) = self.app.try_state::<AppState>()
        {
            state.sync.trigger(reason);
        }
        Some(session_id)
    }

    /// 某个视图按当前设置的卡片宽度，与 [`Self::panel_prepare`] 用的是同一套规则。编排器算 `StartAck.width`
    /// 时直接调用它（与 [`Self::panel_card_width`] 比较，相同则为 null），不得自行实现
    pub fn view_width(&self, view: PanelView, payload: &PanelPayload) -> f64 {
        view_width(view, payload, &self.settings.get())
    }

    /// 某个会话当前的卡片宽度（逻辑 px，含 `panel_set_width` 之后的宽度）；会话已结束、正在隐藏或从未显示过时为 None
    pub fn panel_card_width(&self, session_id: u64) -> Option<f64> {
        self.panel.card_width(session_id)
    }

    /// 某个面板会话是否仍然有效（准备中、已显示，或排队 / 暂存等待显示）；隐藏中、已结束、被取代的都返回 false。
    /// 与 [`Self::panel_hidden`] 配合使用
    pub fn panel_session_live(&self, session_id: u64) -> bool {
        self.panel.is_live(session_id)
    }

    /// 仍然有效的面板会话触发时的前台应用（`PanelShowRequest.prev_app`）。剪贴板粘贴的目标从这里取，而且必须在
    /// `panel_hide_now(Paste)` **之前**取（隐藏之后会话即清空）
    pub fn panel_prev_app(&self, session_id: u64) -> Option<AppRef> {
        self.panel.prev_app(session_id)
    }

    /// 在命令内同步 show（macOS 原生帧模式同时启动窗口 alpha 进入动画），返回 PanelShown；session 已过期时返回 None。
    /// **会阻塞**（主线程往返），在阻塞线程上调用
    pub fn panel_ready(&self, session_id: u64, content_height: f64) -> Option<PanelShown> {
        self.panel.ready(session_id, content_height)
    }

    /// 前端第一个 opacity > 0 的帧 → PerfStage::Shown
    pub fn panel_painted(&self, session_id: u64) {
        self.panel.painted(session_id);
    }

    /// Css 模式：阶梯；Native 模式：重定目标的逐帧弹簧（保留速度；`spring` 缺省 gentle，浮层预留引起的变化用 snappy）。Css 模式会同步改窗口（主线程往返），在阻塞线程上调用
    pub fn panel_resize(&self, session_id: u64, content_height: f64, spring: Option<wisp_types::ResizeSpring>) {
        self.panel
            .resize(session_id, content_height, spring.unwrap_or_default());
    }

    /// 视图内切换的新卡片宽度（前端在 ViewSwitch 交换、内容完全透明的那次提交调用）。
    /// Css 模式会同步改窗口（主线程往返），在阻塞线程上调用
    pub fn panel_set_width(&self, session_id: u64, width: f64) {
        self.panel.set_width(session_id, width);
    }

    /// emit panel:hide_request（前端先播放退出动画）
    pub fn panel_request_hide(&self, reason: HideReason) {
        self.panel.request_hide(reason);
    }

    /// 真正隐藏 + 按需还原焦点 + 注销 Esc 和外部点击监听；reason = Paste 时立即 orderOut，不播放退出动画。
    /// Native 帧模式的退出动画是窗口 alpha 淡出（由这里启动，前端不再播放卡片退出动画）
    ///
    /// 隐藏开始后补发 `panel:hide_request`：由 Rust 发起的隐藏（粘贴后立即隐藏、冒烟）前端并不知道，否则视图会一直挂在
    /// 离屏的页面里；前端自己经 `panel_hide` 发起时这次回声被忽略（见 `PanelController::hide_now_and_notify`）
    pub fn panel_hide_now(&self, session_id: u64, reason: HideReason) {
        self.panel.hide_now_and_notify(session_id, reason);
    }

    /// 以 toast 视图不激活地显示在上次面板位置（没有上次位置时在光标下方），1.6 s 后由前端淡出并 panel_hide。
    ///
    /// **会阻塞**（与 [`Self::panel_prepare`] 相同的主线程往返），在阻塞线程上调用
    pub fn panel_show_toast(&self, toast: ToastPayload) {
        let session_id = self.next_session.fetch_add(1, Ordering::Relaxed) + 1;
        let settings = self.settings.get();
        let screens = self.refresh_screens();
        let width = placement::toast_width(&toast.paste_key);
        let located = match self.panel.last_card() {
            Some(last) => {
                let screen = screens
                    .iter()
                    .find(|s| s.id == last.screen.id)
                    .cloned()
                    .unwrap_or(last.screen);
                Some((screen, PlaceAnchor::At(placement::toast_anchor(last.rect, width))))
            }
            None => self.cursor_anchor(&screens),
        };
        let Some((screen, anchor)) = located else {
            log::error!("panel_show_toast: no monitor available");
            return;
        };
        let cap = placement::height_cap(&screen, f64::from(settings.appearance.max_height_ratio));
        let geometry = placement::place_panel(&screen, anchor, width, initial_height::TOAST, cap);
        self.panel.prepare(PanelOpen {
            session_id,
            view: PanelView::Toast,
            payload: PanelPayload::Toast(toast),
            geometry,
            focused: false,
            transition: PanelTransition::Enter,
            estimated_height: initial_height::TOAST,
            prev_app: None,
            guards: GuardSpec::default(),
        });
    }

    /// 翻译视图被点击后成为 key（Windows 上去掉 NOACTIVATE）
    pub fn panel_activate(&self, session_id: u64) {
        self.panel.activate(session_id);
    }

    /// 用户从面板的头部 / 空白处开始拖动（所有面板视图；剪贴板在「记住位置」模式下隐藏时记住位置）
    pub fn panel_drag_start(&self, session_id: u64) {
        self.panel.drag_start(session_id);
    }

    /// { session_id, view, monitor_id, rect, key }：rect 为卡片矩形（本屏逻辑坐标）；没有会话或正在隐藏时为 None
    pub fn panel_state(&self) -> Option<PanelState> {
        self.panel.snapshot().map(|s| PanelState {
            session_id: s.session_id,
            view: s.view,
            monitor_id: s.screen.id,
            rect: s.rect,
            key: s.key,
        })
    }

    /// 面板显示管线（只给 debug 构建的实验台用）
    #[cfg(any(debug_assertions, feature = "fake-llm"))]
    pub(crate) fn panel_controller(&self) -> &Arc<PanelController> {
        &self.panel
    }

    /// 某个会话是否已经 ready 并显示（调试实验台与冒烟计时用）
    pub fn panel_shown(&self, session_id: u64) -> bool {
        self.panel
            .snapshot()
            .is_some_and(|s| s.session_id == session_id && s.shown)
    }

    /// 面板会话结束的通知（watch 通道，事件驱动）。**每个**会话结束时都会发一次：窗口离屏完毕；还没显示就被取消
    /// （截图前隐藏）；被新的 prepare 取代（原地替换 / 交叉淡入、重新触发、排队或暂存的 prepare 被覆盖）；页面重载、
    /// 固定交接、WebView 失效后放弃。
    ///
    /// watch 只保留最新值，接连结束的几个会话只看得到最后一个，所以值只当作「有会话结束了」的信号：编排器每次
    /// 收到通知，都用 [`Self::panel_session_live`] 核对自己登记的**全部**会话，对返回 false 的收尾（取消流、释放所有权；
    /// 已经 `transfer_to_pinned` 的请求除外）；登记新会话之后也立即核对一次（登记之前的通知可能已经错过）。
    pub fn panel_hidden(&self) -> tokio::sync::watch::Receiver<u64> {
        self.panel.hidden()
    }

    /// 面板窗口真正离屏的通知（值是刚离屏的会话 id）。[`Self::panel_hidden`] 在隐藏**开始**时就会通知（编排器立即取消请求，
    /// 不等退出动画）；要等窗口离开屏幕再动作的调用方（粘贴发键）订阅这个
    pub fn panel_offscreen(&self) -> tokio::sync::watch::Receiver<u64> {
        self.panel.offscreen()
    }

    /// 面板页面加载次数的通知（值递增；启动时等第一次加载完成，记 `panel_page_ready` 启动阶段）
    pub fn panel_loaded(&self) -> tokio::sync::watch::Receiver<u64> {
        self.panel.loaded()
    }

    /// 点是否落在 Wisp 自己的窗口内（浮标判定用）：可见的面板卡片、有会话的 Overlay 所在显示器、pinned / 设置窗口。
    /// 只读缓存，不查询窗口系统（在平台钩子线程上调用）
    pub fn self_hit(&self, monitor: &MonitorId, p: Point) -> bool {
        if let Some(s) = self.panel.snapshot()
            && s.visible
            && s.screen.id == *monitor
            && s.rect.contains(p)
        {
            return true;
        }
        if self.overlays.active_monitors().iter().any(|m| m == monitor) {
            return true;
        }
        let screens = self.screens.read();
        let Some(screen) = screens.iter().find(|s| s.id == *monitor) else {
            return false;
        };
        let dp = screen.point_to_desktop(p, native::DESKTOP_UNIT);
        self.own_rects.lock().values().any(|r| r.contains(dp))
    }

    /// 记录 / 移除自身窗口的矩形（桌面坐标；pinned、设置、引导窗口的移动 / 缩放 / 销毁事件里调用）
    pub(crate) fn note_own_rect(&self, label: &str, rect: Option<Rect>) {
        {
            let mut rects = self.own_rects.lock();
            match rect {
                Some(r) => {
                    rects.insert(label.to_owned(), r);
                }
                None => {
                    rects.remove(label);
                    return;
                }
            }
        }
        // 移动 / 缩放（拖动期间每次都会续上）：面板回收推迟到它停下来之后
        self.note_activity(format!("{label} moved"), MOVE_SETTLE);
    }

    /// 截图前立即隐藏自己的窗口（不播放动画）：面板（等它真正离屏；还在准备的会话取消，截图期间不会冒出来）、
    /// 可见的 pinned（同步离屏；最小化的不动——它不在屏幕上，恢复时也不该被还原）与浮标；之后等合成器出完下一帧。
    /// **会阻塞**（最多约 300 ms + 一帧），在阻塞线程上调用
    pub fn hide_for_capture(&self) -> HiddenSet {
        *self.capture_since.lock() = Some(std::time::Instant::now());
        // 上一次截图的 Overlay 可能还在淡出（destroy_overlays_fading）：等它离开，不把它截进新的冻结帧（最多 300 ms）
        native::await_destroyed(&self.app);
        let mut set = HiddenSet {
            panel: self.panel.hide_for_capture(CAPTURE_HIDE_WAIT),
            pinned: Vec::new(),
            float: false,
        };
        for id in self.pinned.shown_ids() {
            let Some(win) = pinned::window(&self.app, &id) else {
                continue;
            };
            // Windows 的 IsWindowVisible 对最小化窗口也返回 true
            if !win.is_visible().unwrap_or(false) || win.is_minimized().unwrap_or(false) {
                continue;
            }
            match native::pinned_hide(&win) {
                Ok(()) => set.pinned.push(id),
                Err(e) => log::warn!("hide pinned {id} for capture failed: {}", e.message),
            }
        }
        set.float = self.float_hide_for_capture();
        // 浮标是临时窗口：直接隐藏（不播放动画），截图后不恢复，HiddenSet 不记它
        let bobber = self.hide_bobber_for_capture();
        if set.panel || !set.pinned.is_empty() || set.float || bobber {
            native::flush_after_hide();
        }
        set
    }

    /// 浮标是否显示着
    #[cfg(feature = "mouse-hook")]
    fn bobber_shown(&self) -> bool {
        self.platform.bobber.rect().is_some()
    }

    /// 没有编入扩展触发时没有浮标
    #[cfg(not(feature = "mouse-hook"))]
    fn bobber_shown(&self) -> bool {
        false
    }

    /// 浮标可见时立即隐藏它，返回是否隐藏了
    #[cfg(feature = "mouse-hook")]
    fn hide_bobber_for_capture(&self) -> bool {
        if self.platform.bobber.rect().is_none() {
            return false;
        }
        self.platform.bobber.hide(false);
        true
    }

    /// 没有编入扩展触发时没有浮标
    #[cfg(not(feature = "mouse-hook"))]
    fn hide_bobber_for_capture(&self) -> bool {
        false
    }

    /// 截图后恢复：截图前被隐藏的 pinned 重新显示（不抢焦点、不还原最小化）；面板不恢复（OCR 流程会在结果出来时重新
    /// prepare）。截图会话的**每一条**结束路径（commit 成功、出错、取消）都要调用一次。
    /// **会阻塞**（每个 pinned 一次主线程往返），在阻塞线程上调用
    pub fn restore_after_capture(&self, set: HiddenSet) {
        *self.capture_since.lock() = None;
        if set.float {
            self.float_restore_after_capture();
        }
        for id in set.pinned {
            if let Some(win) = pinned::window(&self.app, &id)
                && let Err(e) = native::pinned_show(&win, false)
            {
                log::warn!("restore pinned {id} failed: {}", e.message);
            }
        }
    }

    /// `panel_pin` 命令的入口：面板当前会话是 `session_id` 时取其卡片矩形作为 pinned 的起始位置；会话已过期时改用最近
    /// 一次显示过的卡片矩形（一次都没显示过才按默认位置）。交接时只隐藏 `session_id` 这个会话：交接完成之前用户又触发
    /// 了新的翻译时，新面板不受影响。**会阻塞**到 pinned 窗口建好（WebView 创建 100–400 ms），在阻塞线程上调用。
    /// 返回 pinned_id
    pub fn pin_from_session(&self, session_id: u64, state: PinState, expand: bool) -> Result<String, IpcError> {
        let from_rect = self
            .panel_state()
            .filter(|p| p.session_id == session_id)
            .map(|p| p.rect);
        self.open_pinned_for(state, expand, from_rect, Some(session_id))
    }

    /// 固定交接：在 from_rect（面板所在屏幕的本屏逻辑坐标，即 `PanelState.rect`）处创建**隐藏**的
    /// pinned 窗口，把进行中的流交给它。页面 `pinned_init` → `stream_attach` 回放完 → 同步测量 →
    /// `pinned_ready`，由 [`Self::pinned_ready`] 完成交接；页面迟迟不 ready 时 1.5 s 后照样交接。交接时隐藏的是**调用时**
    /// 可见的面板会话（之后才出现的会话不受影响）。**会阻塞**到 pinned 窗口建好，在阻塞线程上调用
    pub fn open_pinned(&self, state: PinState, expand: bool, from_rect: Option<Rect>) -> Result<String, IpcError> {
        let session = self.panel.snapshot().filter(|s| s.visible).map(|s| s.session_id);
        self.open_pinned_for(state, expand, from_rect, session)
    }

    fn open_pinned_for(
        &self,
        state: PinState,
        expand: bool,
        from_rect: Option<Rect>,
        session: Option<u64>,
    ) -> Result<String, IpcError> {
        let screens = self.refresh_screens();
        let last = self.panel.last_card();
        let (screen, from) = match (from_rect, last) {
            (Some(r), Some(last)) => (last.screen, r),
            (None, Some(last)) => (last.screen, last.rect),
            (from, None) => {
                let (screen, _) = self
                    .cursor_screen(&screens)
                    .ok_or_else(|| IpcError::not_found("no monitor"))?;
                let rect = from.unwrap_or_else(|| {
                    let w = screen.work;
                    Rect::new(
                        (w.x + (w.width - PINNED_DEFAULT_SIZE.0) / 2.0).round(),
                        (w.y + (w.height - PINNED_DEFAULT_SIZE.1) / 3.0).round(),
                        PINNED_DEFAULT_SIZE.0,
                        PINNED_DEFAULT_SIZE.1,
                    )
                });
                (screen, rect)
            }
        };
        let from = Rect::new(
            from.x,
            from.y,
            from.width.max(placement::PINNED_MIN_WIDTH),
            from.height.max(placement::PINNED_MIN_HEIGHT),
        );
        let on_top = self.settings.state().get().pinned_on_top_default;
        let request_id = state.request_id.clone();
        let app = self.app.clone();
        let id = self.pinned.open(
            &self.app,
            |label| self.boot_for(label, None),
            PinRequest {
                state,
                expand,
                screen,
                from,
                session,
                on_top,
            },
            // 在窗口开始加载页面之前把流的所有权转给它——页面再快，`stream_attach` 也不会早于转移
            move |pinned_id| {
                if let Some(request_id) = request_id
                    && let Some(state) = app.try_state::<AppState>()
                {
                    state.orchestrator.transfer_to_pinned(&request_id, pinned_id);
                }
            },
        )?;
        self.note_activity(format!("pinned {id} opened"), PINNED_SETTLE);
        if let Some(win) = pinned::window(&self.app, &id) {
            let label = win.label().to_owned();
            // 窗口销毁（✕ / ⌘W / Alt+F4、退出）：注销登记，并让编排器取消转给这个窗口的流、释放它持有的截图
            let closed = {
                let app = self.app.clone();
                let id = id.clone();
                let label = label.clone();
                move || {
                    if let Some(state) = app.try_state::<AppState>() {
                        state.windows.pinned.remove(&id);
                        state.windows.note_own_rect(&label, None);
                        state.orchestrator.pinned_closed(&id);
                    }
                }
            };
            let moved = {
                let app = self.app.clone();
                let tracked = win.clone();
                move || {
                    if let Some(state) = app.try_state::<AppState>() {
                        let rect = native::window_frame(&tracked);
                        state.windows.note_own_rect(tracked.label(), rect);
                    }
                }
            };
            pinned::watch_window(&win, closed, moved);
        }
        let app = self.app.clone();
        let fallback_id = id.clone();
        let fallback_height = from.height;
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(pinned::READY_FALLBACK).await;
            // pinned_ready 会同步等主线程完成交接：放进阻塞线程池，不占 tokio worker
            let handoff = tauri::async_runtime::spawn_blocking(move || {
                if let Some(state) = app.try_state::<AppState>()
                    && state.windows.pinned.state(&fallback_id).is_some()
                {
                    state.windows.pinned_ready(&fallback_id, fallback_height);
                }
            });
            if let Err(e) = handoff.await {
                log::warn!("pinned ready fallback failed: {e}");
            }
        });
        Ok(id)
    }

    /// Pinned 窗口启动时拉取初始状态
    pub fn pinned_state(&self, pinned_id: &str) -> Option<PinState> {
        self.pinned.state(pinned_id)
    }

    /// Pinned 首帧已就绪（`pinned_ready` 命令）：同一帧 show pinned 并隐藏 panel（不出现空帧）；展开模式按
    /// `content_height` 把高度定为 min(内容, 屏幕 70%)、宽 640、在面板所在屏幕居中（macOS 用 spring.gentle 逐帧过渡；
    /// Windows / Linux 直接在目标矩形显示，面板由 CSS 淡出）。pinned_id 未知或已就绪过时忽略。
    ///
    /// 三端一致：交接时 pinned **不激活**（不抢原应用的前台：pinned 是普通窗口，用户点击它时才激活应用）。
    ///
    /// 只隐藏发起固定的那个面板会话：交接完成之前（页面加载 + 回放，慢时要等 1.5 s 兜底）用户又按 ⌥D 触发了新的翻译时，
    /// 新会话照常显示，这里只显示 pinned（固定后按 ⌥D，Panel 显示新内容，固定窗不变）。
    /// **会阻塞**到主线程执行完交接（在阻塞线程上调用）
    pub fn pinned_ready(&self, pinned_id: &str, content_height: f64) {
        let Some(h) = self.pinned.begin_ready(pinned_id, content_height) else {
            return;
        };
        let Some(win) = pinned::window(&self.app, pinned_id) else {
            return;
        };
        let animate = !self.panel.options().reduce_motion;
        let result = if h.expand_to.is_some() && self.frame_mode == FrameMode::Css {
            let shown = native::pinned_handoff(&win, None, h.rect, h.expand_to, animate);
            if let Some(id) = h.session {
                self.panel.request_hide_for(id, HideReason::Pin);
            }
            shown
        } else {
            let hiding = h.session.filter(|&id| self.panel.begin_external_hide_for(id));
            let panel_window = hiding.and_then(|_| self.app.get_webview_window(PANEL_LABEL));
            let shown = native::pinned_handoff(&win, panel_window.as_ref(), h.rect, h.expand_to, animate);
            if let Some(id) = hiding {
                if shown.is_err() {
                    // 交接中途失败：面板未必已经离屏，补一次隐藏（对已隐藏的窗口无效果），不留下没有会话的可见面板
                    PanelHost::hide(&*self.host, None, Box::new(|| {}));
                }
                // 面板已在同一帧离屏：清掉会话并让前端照常走退出流程
                self.panel.finish_external_hide(id, HideReason::Pin);
            }
            shown
        };
        match result {
            Ok(()) => self.note_own_rect(&h.label, Some(h.expand_to.unwrap_or(h.rect))),
            Err(e) => log::warn!("pinned handoff failed: {}", e.message),
        }
    }

    /// 切换 Pinned 窗口置顶，并记为之后新建 pinned 的默认值
    pub fn pinned_set_on_top(&self, pinned_id: &str, on_top: bool) -> Result<(), IpcError> {
        let win = pinned::window(&self.app, pinned_id).ok_or_else(|| IpcError::not_found("pinned window"))?;
        win.set_always_on_top(on_top)?;
        self.pinned.set_on_top(pinned_id, on_top);
        let app = self.app.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let Some(state) = app.try_state::<AppState>() else {
                return;
            };
            if let Err(e) = state.settings.state().update(&mut |s| s.pinned_on_top_default = on_top) {
                log::warn!("save pinned_on_top_default failed: {}", e.message);
            }
        });
        Ok(())
    }

    /// 托盘「显示固定窗口」（pinned 不切 ActivationPolicy，macOS 上不进 ⌘Tab，从托盘找回；最小化的也还原）
    pub fn show_all_pinned(&self) {
        for id in self.pinned.shown_ids() {
            if let Some(win) = pinned::window(&self.app, &id)
                && let Err(e) = native::pinned_show(&win, true)
            {
                log::warn!("show pinned {id} failed: {}", e.message);
            }
        }
    }

    /// `window_open` 命令的入口：按目标打开或聚焦设置 / 引导窗口
    pub fn open_target(&self, target: WindowTarget) {
        match target {
            WindowTarget::Settings { section, anchor } => self.open_settings_at(section, anchor),
            WindowTarget::Onboarding { step } => self.open_onboarding(step),
        }
    }

    /// 打开或聚焦设置窗口（存在期间 ActivationPolicy = Regular）。`section` 为分组 id（例如 "providers"、"hotkeys"）：
    /// 首次创建时写入 `Boot.route`；窗口已打开时 emit_to 该窗口 `window:navigate`
    pub fn open_settings(&self, section: Option<String>) {
        self.open_settings_at(section, None);
    }

    /// 同 [`Self::open_settings`]，另带分组内的锚点（`ipc::validate::validate_window_target`
    /// 已核对组合合法）。没有分组时锚点无意义，忽略
    pub fn open_settings_at(&self, section: Option<String>, anchor: Option<String>) {
        let route = section.map(|s| WindowTarget::Settings {
            section: Some(s),
            anchor,
        });
        self.open_regular_window(super::settings_window::SETTINGS_LABEL, route);
    }

    /// 打开或聚焦引导窗口（存在期间 ActivationPolicy = Regular）。`step` 的传递方式同 `open_settings`
    pub fn open_onboarding(&self, step: Option<u8>) {
        let route = step.map(|s| WindowTarget::Onboarding { step: Some(s) });
        self.open_regular_window(super::settings_window::ONBOARDING_LABEL, route);
    }

    fn open_regular_window(&self, label: &'static str, route: Option<WindowTarget>) {
        self.note_activity(format!("{label} opened"), OPEN_SETTLE);
        let app = self.app.clone();
        let boot = self.boot_for(label, route);
        // Windows 上在事件回调 / 同步命令里同步创建 WebView2 窗口会死锁，统一放到异步任务里
        tauri::async_runtime::spawn(async move {
            // 刚销毁的窗口（例如上一次关闭的同一个设置窗口）的 `Destroyed` 处理完再建（见 native::note_destroying）
            native::settle_destroyed(&app).await;
            if let Err(e) = super::settings_window::open(&app, &boot) {
                log::error!("open {label} window failed: {e}");
            }
        });
    }

    /// 生成某个窗口的 Boot：外观取当前解析值；`dev` = 开发者模式 || debug 构建；
    /// 阴影边距只给有卡片透明边距的窗口：Css 帧模式且能透明时的面板，以及 Linux 上由 CSS 画阴影的 pinned
    /// （`runtime::transparent_windows` 为 false 时一律 0，卡片铺满窗口）
    pub fn boot_for(&self, label: &str, route: Option<WindowTarget>) -> Boot {
        make_boot(&self.app, &self.settings, self.frame_mode, label, route)
    }

    /// 为每个显示器准备 Overlay（隐藏；macOS：非激活 NSPanel），并登记 label → 显示器的对应关系
    /// （`overlay_init` 无参，按调用方 label 反查，见 [`Self::overlay_monitor`]）。
    /// 新建的窗口页面加载后自己调用 `overlay_init`；**复用预建的隐藏 Overlay** 时先登记、再 emit_to 该窗口
    /// `overlay:session`（`OverlaySessionStart`），页面据此重置并重新 init。显示由 [`Self::overlay_show`] 完成。
    /// **会阻塞**（创建 WebView 窗口，每个 100–400 ms），在阻塞线程上调用
    pub fn create_overlays(&self, session: &OverlaySession) -> Result<(), IpcError> {
        let screens: Vec<Screen> = session.monitors.iter().map(Screen::from_info).collect();
        if !screens.is_empty() {
            self.screens.write().clone_from(&screens);
        }
        self.overlays.create(
            &self.app,
            &|label| self.boot_for(label, None),
            session.session_id,
            &session.monitors,
        )
    }

    /// 显示某块显示器的 Overlay（覆盖完整区域）。光标所在显示器的那个成为 key（不激活应用）。
    /// `frozen` 带 CGImage（macOS SCK 帧）时先把它设为窗口底层图层的 contents 再显示——首帧即冻结帧，可以在创建后
    /// 立即调用；没有原生冻结帧时（Windows / X11、假帧）应等页面首帧渲染完成（`overlay_ready`）再调用。
    /// 返回是否用上了原生冻结帧：`OverlayInit.frozen_native` **只**取这个返回值。
    /// **会阻塞**（macOS 两次主线程往返），在阻塞线程上调用
    pub fn overlay_show(
        &self,
        session_id: u64,
        monitor_id: &str,
        frozen: Option<&wisp_capture::Frame>,
    ) -> Result<bool, IpcError> {
        let (label, monitor) = self
            .overlays
            .find(session_id, monitor_id)
            .ok_or_else(|| IpcError::not_found("overlay for monitor"))?;
        let win = self
            .app
            .get_webview_window(&label)
            .ok_or_else(|| IpcError::not_found("overlay window"))?;
        let native_frame = frozen.is_some_and(|f| native::overlay_set_frozen(&win, f));
        let key = self.platform.screen.cursor().0 == monitor.id;
        native::overlay_show(&win, OverlayRegistry::monitor_rect(&monitor), key)?;
        self.overlays.mark_shown(&label, true);
        Ok(native_frame)
    }

    /// Overlay 窗口当前会话绑定的显示器；该窗口没有会话（预建、空闲中）或 label 未知时返回 None
    pub fn overlay_monitor(&self, label: &str) -> Option<MonitorId> {
        self.overlays.monitor_of(label)
    }

    /// Overlay 窗口当前绑定的截图会话（命令层核对调用方用）
    pub fn overlay_session_of(&self, label: &str) -> Option<u64> {
        self.overlays.session_of(label)
    }

    /// 让这块显示器的 Overlay 接收键盘（多屏时按键焦点跟随光标）。不激活应用；只投递，不等主线程
    pub fn overlay_focus(&self, session_id: u64, monitor_id: &str) {
        if let Some((label, _)) = self.overlays.find(session_id, monitor_id)
            && let Some(win) = self.app.get_webview_window(&label)
        {
            native::overlay_focus(&win);
        }
    }

    /// 截图会话 commit 了：这次会话的全部 Overlay 不再由原生层跟随指针切换 key（macOS 的 NSTrackingArea；
    /// 其他平台空操作——那里的轮询与页面请求本来就只在未 commit 时生效）。只投递，不等主线程
    pub fn overlay_stop_following(&self, session_id: u64) {
        for (label, win) in self.app.webview_windows() {
            if self.overlays.session_of(&label) == Some(session_id) {
                native::overlay_stop_following(&win);
            }
        }
    }

    /// 销毁全部 Overlay（预建的那个改为隐藏复用）。会等主线程（Windows 隐藏复用的那个），在阻塞线程上调用
    pub fn destroy_overlays(&self) {
        self.overlays.destroy_all(&self.app, None);
    }

    /// commit 之后的正常收尾（`overlay_done`）：与 [`Self::destroy_overlays`] 相同，但 macOS 先把窗口淡出（fast +
    /// ease.exit；减少动效时 100 ms 线性）再销毁——原生冻结帧不会一帧硬切成真实屏幕
    pub fn destroy_overlays_fading(&self) {
        let fade = if self.panel.options().reduce_motion {
            native::Fade {
                duration: crate::windows::sizing::timing::REDUCED_FADE,
                curve: crate::windows::sizing::CubicBezier::LINEAR,
            }
        } else {
            native::Fade {
                duration: crate::windows::sizing::duration::FAST,
                curve: crate::windows::sizing::CubicBezier::EXIT,
            }
        };
        self.overlays.destroy_all(&self.app, Some(fade));
    }

    /// 面板在 Overlay 还在时显示（OCR commit 时 Panel 先显示在 Overlay 下方）：把 Overlay 重新压到
    /// 最上面。macOS 靠层级（101 > 25）天然如此；Windows 两者都是 `HWND_TOPMOST`，后显示的会盖住先显示的
    pub(crate) fn raise_overlays(&self) {
        for label in self.overlays.shown_labels() {
            if let Some(win) = self.app.get_webview_window(&label) {
                native::overlay_raise(&win);
            }
        }
    }

    /// Overlay 窗口被销毁时移除登记
    pub(crate) fn forget_overlay(&self, label: &str) {
        self.overlays.forget(label);
    }

    /// 外观变化时广播到全部窗口（Native 帧模式同时更新面板的原生卡片表面；减少动效同步到面板的窗口动画）
    pub fn broadcast_appearance(&self, a: &AppearanceState) {
        self.host.set_surface(CardSurface::resolve(a.theme, a.material));
        self.host.set_theme(a.theme);
        let mut options = self.panel.options();
        options.reduce_motion = a.reduce_motion;
        self.panel.set_options(options);
        if let Err(e) = self.app.emit(event::APPEARANCE_CHANGED, a) {
            log::warn!("emit appearance:changed failed: {e}");
        }
    }
}

/// 主显示器（没有标记时取第一块）
fn primary_screen(screens: &[Screen]) -> Option<Screen> {
    screens.iter().find(|s| s.primary).or_else(|| screens.first()).cloned()
}

/// Tauri（GDK / winit）报告的显示器：只在平台层给不出显示器时使用（Wayland）。id 取 `tauri:<名称或序号>`，
/// 不与平台层的 MonitorId 混用（这种情况下平台层本来就没有 id）
fn tauri_screens(app: &tauri::AppHandle) -> Vec<Screen> {
    let primary = app.primary_monitor().ok().flatten();
    let monitors = match app.available_monitors() {
        Ok(m) => m,
        Err(e) => {
            log::warn!("no monitors from the platform layer, and tauri failed too: {e}");
            return Vec::new();
        }
    };
    monitors
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let scale = if m.scale_factor().is_finite() && m.scale_factor() > 0.0 {
                m.scale_factor()
            } else {
                1.0
            };
            let (pos, size, work) = (m.position(), m.size(), m.work_area());
            let frame = Rect::new(
                f64::from(pos.x) / scale,
                f64::from(pos.y) / scale,
                f64::from(size.width) / scale,
                f64::from(size.height) / scale,
            );
            let mut info = wisp_types::MonitorInfo {
                id: format!("tauri:{}", m.name().cloned().unwrap_or_else(|| i.to_string())),
                name: m.name().cloned().unwrap_or_default(),
                physical_origin: wisp_types::PhysPoint { x: pos.x, y: pos.y },
                physical_size: wisp_types::PhysSize {
                    width: size.width,
                    height: size.height,
                },
                scale_factor: scale,
                frame,
                work_area: Rect::new(
                    f64::from(work.position.x - pos.x) / scale,
                    f64::from(work.position.y - pos.y) / scale,
                    f64::from(work.size.width) / scale,
                    f64::from(work.size.height) / scale,
                ),
                is_primary: false,
            };
            info.is_primary = primary
                .as_ref()
                .is_some_and(|p| p.name() == m.name() && p.position() == m.position());
            Screen::from_info(&info)
        })
        .collect()
}

/// 生成注入到每个 WebView 的 boot 脚本：设置 `window.__WISP_BOOT__`（序列化自 [`Boot`]，键集合由 wisp-types 的
/// 黄金样例与 TS 的 `Boot` 对齐），并立即给 `<html>` 写 data-os / data-theme / data-reduced-motion / data-material /
/// data-frame / data-window、lang、`--accent` 与 `--shadow-margin`，与前端 `shared/ipc/boot.ts` 的 `applyBootAttributes`
/// 完全一致，保证 CSS 首帧就生效（防止主题闪烁）。initialization script 由 WebView API 注入，不受 CSP 的 inline-script 限制。
///
/// 注入时机因平台而异：WKWebView 的 document-start 已有 `<html>`；WebView2 的 `AddScriptToExecuteOnDocumentCreated`
/// 与 WebKitGTK 可能还没有 `documentElement`。没有时用 MutationObserver 等 `<html>` 插入后立即补写（回调在首帧之前的
/// 微任务检查点执行），并以 DOMContentLoaded 兜底；模块脚本里的 `applyBootAttributes` 再幂等地写一遍
pub fn boot_script(boot: &Boot) -> String {
    let json = serde_json::to_string(boot).unwrap_or_else(|e| {
        log::error!("serialize boot failed: {e}");
        "{}".to_owned()
    });
    format!(
        r#"(function(){{var b={json};Object.defineProperty(window,"__WISP_BOOT__",{{value:Object.freeze(b),enumerable:true}});function a(h){{var l=String(b.label||""),k=l==="settings"||l==="onboarding"||l==="panel"?l:l.indexOf("pinned-")===0||l==="clipfloat"?"pinned":l.indexOf("overlay-")===0?"overlay":"panel";h.dataset.os=b.os;h.dataset.theme=b.theme;h.dataset.reducedMotion=String(b.reduce_motion);h.dataset.material=b.material;h.dataset.frame=b.frame_mode;h.dataset.window=k;h.lang=b.ui_language;if(b.accent)h.style.setProperty("--accent",b.accent);h.style.setProperty("--shadow-margin",(+b.shadow_margin||0)+"px");}}var d=document,h=d.documentElement;if(h){{a(h);return;}}var m=new MutationObserver(function(){{var e=d.documentElement;if(e){{m.disconnect();a(e);}}}});m.observe(d,{{childList:true}});d.addEventListener("DOMContentLoaded",function(){{m.disconnect();var e=d.documentElement;if(e&&!e.dataset.window)a(e);}},{{once:true}});}})();"#
    )
}

#[cfg(test)]
mod tests {
    use wisp_types::{
        ClipPage, ClipboardPayload, CopiedReason, Lang, ResolvedMaterial, ResolvedTheme, TaskKind, TranslatePayload,
    };

    use super::*;

    /// 面板回收的推迟只看短时活动，按固定顺序给出第一条原因；截图最多算 CAPTURE_BUSY_MAX
    #[test]
    fn busy_reason_priorities() {
        let secs = Duration::from_secs;
        let all = BusyProbe {
            capture_for: Some(secs(5)),
            overlay: Some("overlay-0".to_owned()),
            animating: true,
            bobber: true,
            activity: Some(("pinned p1 opened".to_owned(), secs(20))),
        };
        // 什么都没有：不推迟（长时间留在屏幕上的 pinned、开着的设置窗口本身不进快照，也就不算忙）
        assert_eq!(busy_reason(&BusyProbe::default()), None);
        // 逐条去掉优先级最高的那一项，原因依次后移
        let mut p = all.clone();
        assert_eq!(busy_reason(&p).as_deref(), Some("capture in progress"));
        p.capture_for = None;
        assert_eq!(busy_reason(&p).as_deref(), Some("overlay-0 shown"));
        p.overlay = None;
        assert_eq!(busy_reason(&p).as_deref(), Some("native animation running"));
        p.animating = false;
        assert_eq!(busy_reason(&p).as_deref(), Some("bobber shown"));
        p.bobber = false;
        assert_eq!(
            busy_reason(&p).as_deref(),
            Some("pinned p1 opened, settles in 20000 ms")
        );
        // 活动已经结束（剩余时长为 0）：不再推迟
        p.activity = Some(("settings moved".to_owned(), Duration::ZERO));
        assert_eq!(busy_reason(&p), None);
        // 截图超过上限（漏调了 restore_after_capture）：不再算忙，落到下一条
        let stuck = BusyProbe {
            capture_for: Some(CAPTURE_BUSY_MAX),
            ..all.clone()
        };
        assert_eq!(busy_reason(&stuck).as_deref(), Some("overlay-0 shown"));
        let just_under = BusyProbe {
            capture_for: Some(CAPTURE_BUSY_MAX - Duration::from_millis(1)),
            ..BusyProbe::default()
        };
        assert_eq!(busy_reason(&just_under).as_deref(), Some("capture in progress"));
        let only_stuck = BusyProbe {
            capture_for: Some(CAPTURE_BUSY_MAX + secs(1)),
            ..BusyProbe::default()
        };
        assert_eq!(busy_reason(&only_stuck), None);
    }

    /// 短时活动的时长：都远短于回收推迟的上限（panel.rs 的 RECYCLE_DEFER_MAX = 120 s），拖动按 2 s 续
    #[test]
    fn busy_activity_windows_are_short() {
        assert!(OPEN_SETTLE <= Duration::from_secs(5));
        assert!(MOVE_SETTLE <= Duration::from_secs(5));
        assert!(PINNED_SETTLE <= Duration::from_secs(60));
        assert!(CAPTURE_BUSY_MAX <= Duration::from_secs(120));
    }

    #[test]
    fn boot_script_embeds_valid_json() {
        let boot = Boot {
            label: "settings".to_owned(),
            os: OsKind::Macos,
            os_version: "26.4".to_owned(),
            theme: ResolvedTheme::Dark,
            accent: "#0A84FF".to_owned(),
            reduce_motion: true,
            reduce_transparency: false,
            material: ResolvedMaterial::Native,
            frame_mode: FrameMode::Native,
            shadow_margin: 0.0,
            font_size: 14,
            ui_language: "zh-CN".to_owned(),
            dev: false,
            debug_build: false,
            route: Some(WindowTarget::Settings {
                section: Some("providers".to_owned()),
                anchor: None,
            }),
            prepare: None,
        };
        let s = boot_script(&boot);
        let start = s.find("var b=").expect("boot object") + "var b=".len();
        let end = s[start..].find(";Object.defineProperty").expect("end") + start;
        let v: serde_json::Value = serde_json::from_str(&s[start..end]).expect("valid json");
        assert_eq!(v["label"], "settings");
        assert_eq!(v["os"], "macos");
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["frame_mode"], "native");
        assert_eq!(v["reduce_motion"], true);
        assert_eq!(v["os_version"], "26.4");
        assert_eq!(v["route"]["kind"], "settings");
        assert_eq!(v["route"]["section"], "providers");
        // 不是冷唤出的面板：没有 prepare 键（TS 侧可缺省）
        assert!(v.get("prepare").is_none());
        let back: Boot = serde_json::from_value(v).expect("Boot 往返");
        assert_eq!(back, boot);
        assert!(s.contains("h.dataset.reducedMotion=String(b.reduce_motion)"));
        assert!(s.contains("--shadow-margin"));
        // 没有 documentElement 时的补写路径
        assert!(s.contains("new MutationObserver"));
        assert!(s.contains("DOMContentLoaded"));
    }

    #[test]
    fn shadow_margin_only_for_windows_with_transparent_card_margin() {
        // Native 帧模式：面板窗口即卡片
        assert_eq!(boot_shadow_margin(FrameMode::Native, "panel"), 0.0);
        // 设置 / 引导 / Overlay 没有卡片外的透明边距
        for label in ["settings", "onboarding", "overlay-0"] {
            assert_eq!(boot_shadow_margin(FrameMode::Css, label), 0.0, "{label}");
        }
        // Css 帧模式的面板：能透明时为 SHADOW_MARGIN
        let expected = if crate::app::runtime::transparent_windows() {
            SHADOW_MARGIN
        } else {
            0.0
        };
        assert_eq!(boot_shadow_margin(FrameMode::Css, "panel"), expected);
        let pinned = if native::pinned_css_shadow() {
            SHADOW_MARGIN
        } else {
            0.0
        };
        assert_eq!(boot_shadow_margin(FrameMode::Css, "pinned-1"), pinned);
    }

    fn translate(text: &str, dict: bool) -> PanelPayload {
        PanelPayload::Translate(TranslatePayload {
            source_text: text.to_owned(),
            request_id: None,
            task: TaskKind::Translate,
            style_id: "natural".to_owned(),
            detected_lang: None,
            target_lang: Lang::ZhCn,
            dict_mode: dict,
            long_text: None,
            not_ready: None,
            selection_method: None,
        })
    }

    #[test]
    fn view_widths_follow_settings_and_spec() {
        let mut s = wisp_types::Settings::default();
        assert_eq!(view_width(PanelView::Translate, &translate("hi", false), &s), 360.0);
        assert_eq!(
            view_width(PanelView::Translate, &translate(&"a".repeat(400), false), &s),
            520.0
        );
        assert_eq!(
            view_width(PanelView::Translate, &translate(&"a".repeat(400), true), &s),
            360.0
        );
        assert_eq!(view_width(PanelView::Ocr, &translate("", false), &s), 440.0);
        assert_eq!(view_width(PanelView::Input, &translate("", false), &s), 420.0);
        let clip = PanelPayload::Clipboard(ClipboardPayload {
            page: ClipPage::empty(wisp_types::ClipFilter::All),
            paused: false,
            hint_bar: true,
            paste_on_double_click: false,
            unavailable: None,
            recording: wisp_types::ClipboardRecording::Recording,
            favorites_collapsed: false,
            queue_unavailable: None,
        });
        assert_eq!(view_width(PanelView::Clipboard, &clip, &s), 420.0);
        // 设置被手改到范围外时夹回
        s.appearance.clipboard_width = 9999;
        assert_eq!(view_width(PanelView::Clipboard, &clip, &s), 560.0);
        s.appearance.translate_min_width = 100;
        assert_eq!(view_width(PanelView::Translate, &translate("hi", false), &s), 320.0);
        let toast = PanelPayload::Toast(ToastPayload {
            reason: CopiedReason::CopyOnly,
            paste_key: "⌘V".to_owned(),
            queue: None,
        });
        let w = view_width(PanelView::Toast, &toast, &s);
        assert!((placement::TOAST_MIN_WIDTH..=placement::TOAST_MAX_WIDTH).contains(&w));
    }

    #[test]
    fn guard_specs_per_view() {
        let mut t = TranslateSettings::default();
        let g = guard_spec(PanelView::Translate, &t);
        assert!(g.outside_click && g.esc && !g.blur_hides && g.mouse_leave_px.is_none());
        t.close_on_mouse_leave = true;
        t.mouse_leave_px = 5;
        assert_eq!(guard_spec(PanelView::Translate, &t).mouse_leave_px, Some(80.0));
        assert_eq!(guard_spec(PanelView::Ocr, &t).mouse_leave_px, None);
        t.close_on_click_outside = false;
        assert!(!guard_spec(PanelView::Ocr, &t).outside_click);
        for v in [PanelView::Input, PanelView::Clipboard] {
            let g = guard_spec(v, &t);
            assert!(g.blur_hides && !g.esc && !g.outside_click);
        }
        assert_eq!(guard_spec(PanelView::Toast, &t), GuardSpec::default());
    }

    #[test]
    fn panel_options_from_settings() {
        let mut s = wisp_types::Settings::default();
        let o = panel_options(&s, false);
        assert!(!o.reduce_motion);
        assert!(o.low_memory_when_hidden);
        assert_eq!(o.unload_after, None);
        assert!(panel_options(&s, true).reduce_motion, "跟随系统");
        s.appearance.reduce_motion = ReduceMotionPref::Off;
        assert!(!panel_options(&s, true).reduce_motion);
        s.advanced.unload_panel_after_idle_min = Some(30);
        assert_eq!(panel_options(&s, false).unload_after, Some(Duration::from_secs(1800)));
    }
}
