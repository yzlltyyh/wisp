//! 剪贴板服务核心对外部世界的接口。
//!
//! - [`Port`]：平台能力（读 / 写 / 监听剪贴板、模拟粘贴、前台应用、应用图标、能否注入粘贴键）。生产实现就是
//!   `wisp_platform::Platform`；
//! - [`Host`]：宿主能力（设置、面板状态、事件、toast、托盘、state.json、埋点、通知）。生产实现 [`AppHost`] 基于
//!   `AppHandle`，服务之间一律在运行时经 `app.try_state::<AppState>()` 取得对方（依赖注入的约定）；
//! - [`Exec`]：把后台工作（缩略图、图标、文件 exists 检查、监听的安装 / 注销）交给阻塞线程池，以及一次性的延时任务。
//!   单测里可以同步执行（断言是确定的），也可以排队、由测试决定何时执行（覆盖后台任务与面板 / 前端的时序）。
//!
//! 单测用 mock 实现这三个接口（`clipboard_svc::tests`），不需要真实的剪贴板、窗口与 Tauri 运行时。
//!
//! Windows 预激活之前的只读检查由平台层的 `Paste::prepare_target` 提供（应用层没有 `cfg(target_os)` 与 Win32 调用）。

use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tauri::{Emitter, Manager};
use tauri_plugin_notification::NotificationExt;
use wisp_platform::{
    AppInfo, ClipboardAccess, FocusTracker, HookError, HookHandle, InputSignal, KeyChord, Paste, PasteKeys, Platform,
};
use wisp_types::{
    AppRef, Capability, ClipboardChanged, HideReason, OcrHistoryChanged, PanelView, QueueStatus, Settings,
    ToastPayload, event,
};

use crate::app::perf::PerfMarks;
use crate::settings::SettingsService;
use crate::state::AppState;
use crate::windows::WindowManager;
use crate::windows::panel::PANEL_LABEL;
use crate::windows::settings_window::SETTINGS_LABEL;

use super::texts::{self, Text};

/// Windows 粘贴前「先把前台交还目标」之前的检查结果（见 [`Port::target_check`]）：
/// - `Ok(())`：记录的窗口仍在、属于同一进程、没有最小化、没有挂起，可以先交还前台再隐藏面板；
/// - `Err(原因)`：不预激活，直接交给 `paste_into` 判定（原因只进日志）。窗口已关闭、已最小化、挂起或无法确定时都是
///   这样：预激活会把最小化的窗口恢复、或把同一应用的其他窗口拉到前台，而 `paste_into` 对这些情形本来就只复制并提示
pub(crate) type TargetCheck = Result<(), &'static str>;

/// 连续粘贴拦下粘贴键的钩子（`queue.rs`）。生产实现是平台层的输入钩子（`mouse-hook` 没编入时不可用）
pub(crate) trait PasteKeyHooks: Send + Sync {
    /// 装上只含这些粘贴键的钩子；返回的句柄 drop 即卸载
    fn install(&self, keys: PasteKeys, sink: Box<dyn Fn(InputSignal) + Send + Sync>) -> Result<HookHandle, HookError>;
    /// 重放一次拦下的粘贴键。**阻塞**
    fn replay(&self, chord: KeyChord) -> Result<(), HookError>;
    /// 不可用的原因（i18n key）；可用时 None。不阻塞
    fn unavailable(&self) -> Option<String>;
}

/// 平台能力
pub(crate) trait Port: Send + Sync {
    fn clipboard(&self) -> &dyn ClipboardAccess;
    fn paste(&self) -> &dyn Paste;
    fn focus(&self) -> &dyn FocusTracker;
    fn apps(&self) -> &dyn AppInfo;
    /// 连续粘贴的按键拦截
    fn paste_hooks(&self) -> &dyn PasteKeyHooks;
    /// Windows 预激活之前的检查：与 `paste_into` 的判定一致的保守子集——只认 `AppRef.window` 记录的那个窗口，
    /// 不恢复最小化窗口、不改用同一进程的其他窗口、挂起时不等待。只在 Windows 上调用（其他平台不预激活）。
    /// 生产实现就是 `Paste::prepare_target`
    fn target_check(&self, target: &AppRef) -> TargetCheck;
    /// 能否模拟粘贴按键（`PlatformInfo.capabilities.paste_inject`，不阻塞）。不知道原应用时据此选择 toast 的原因：
    /// Wayland 会话取不到前台应用（prev_app 恒为 None），提示应是「Wayland 下请手动粘贴」而不是「原应用已关闭」
    fn paste_inject(&self) -> Capability;
}

impl Port for Platform {
    fn clipboard(&self) -> &dyn ClipboardAccess {
        &*self.clipboard
    }

    fn paste(&self) -> &dyn Paste {
        &*self.paste
    }

    fn focus(&self) -> &dyn FocusTracker {
        &*self.focus
    }

    fn apps(&self) -> &dyn AppInfo {
        &*self.apps
    }

    fn target_check(&self, target: &AppRef) -> TargetCheck {
        self.paste.prepare_target(target)
    }

    fn paste_inject(&self) -> Capability {
        // `Platform::info` 只读已有的探测结果、不阻塞
        self.info().capabilities.paste_inject
    }

    fn paste_hooks(&self) -> &dyn PasteKeyHooks {
        self
    }
}

impl PasteKeyHooks for Platform {
    #[cfg(feature = "mouse-hook")]
    fn install(&self, keys: PasteKeys, sink: Box<dyn Fn(InputSignal) + Send + Sync>) -> Result<HookHandle, HookError> {
        self.hooks.install(
            wisp_platform::HookSpec {
                paste_keys: keys,
                ..Default::default()
            },
            sink,
        )
    }

    #[cfg(not(feature = "mouse-hook"))]
    fn install(&self, keys: PasteKeys, sink: Box<dyn Fn(InputSignal) + Send + Sync>) -> Result<HookHandle, HookError> {
        let _ = (keys, sink);
        Err(HookError::Unsupported(
            wisp_platform::reason::HOOKS_NOT_COMPILED.to_owned(),
        ))
    }

    #[cfg(feature = "mouse-hook")]
    fn replay(&self, chord: KeyChord) -> Result<(), HookError> {
        self.hooks.replay_paste(chord)
    }

    #[cfg(not(feature = "mouse-hook"))]
    fn replay(&self, chord: KeyChord) -> Result<(), HookError> {
        let _ = chord;
        Err(HookError::Unsupported(
            wisp_platform::reason::HOOKS_NOT_COMPILED.to_owned(),
        ))
    }

    fn unavailable(&self) -> Option<String> {
        let caps = self.info().capabilities;
        super::queue::unavailable_of(&caps.paste_inject, &caps.input_hooks)
    }
}

/// 剪贴板视图在面板里的状态（`clipboard:changed` 何时可以推送）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClipView {
    /// 面板没有显示剪贴板视图（隐藏、正在隐藏，或显示的是别的视图）
    Hidden,
    /// 剪贴板视图的会话已 prepare、还没 ready：视图可能还没挂载、没订阅事件，此时推送可能丢失
    Preparing(u64),
    /// 已 ready 并显示：视图已按载荷渲染、已订阅事件
    Shown(u64),
}

impl ClipView {
    /// 当前剪贴板面板会话（准备中或已显示）
    pub(crate) fn session(self) -> Option<u64> {
        match self {
            Self::Hidden => None,
            Self::Preparing(s) | Self::Shown(s) => Some(s),
        }
    }
}

/// 连续粘贴的进度要不要用面板的 toast 显示：面板正显示（用户又打开了剪贴板）时不拿 toast 换掉它；悬浮剪贴板开着时
/// 进度在它里面。正显示的是上一次的进度 toast 时照常换成新的，否则一直停在「1 / 5」
pub(crate) fn queue_toast_shown(panel: Option<PanelView>, float_ready: bool) -> bool {
    panel.is_none_or(|v| v == PanelView::Toast) && !float_ready
}

/// 由面板状态（会话 id 与视图）得出剪贴板视图的状态（纯函数，单测覆盖）
pub(crate) fn clip_view_of(state: Option<(u64, PanelView)>, shown: impl FnOnce(u64) -> bool) -> ClipView {
    match state {
        Some((session, PanelView::Clipboard)) if shown(session) => ClipView::Shown(session),
        Some((session, PanelView::Clipboard)) => ClipView::Preparing(session),
        _ => ClipView::Hidden,
    }
}

/// 宿主能力
pub(crate) trait Host: Send + Sync {
    /// 当前设置
    fn settings(&self) -> Arc<Settings>;
    /// 剪贴板视图的状态（`clipboard:changed` 只在已显示时推送；准备中的先排队）
    fn clipboard_view(&self) -> ClipView;
    /// 面板（任一视图）可见：空闲清理要等它隐藏
    fn panel_visible(&self) -> bool;
    /// 某个面板会话是否仍然有效（`WindowManager::panel_session_live`：隐藏中、已结束、被取代的都是 false）
    fn session_live(&self, session: u64) -> bool;
    /// 面板会话触发时的前台应用（面板宿主的粘贴目标；会话结束后取不到，必须在隐藏之前取）
    fn panel_prev_app(&self, session: u64) -> Option<AppRef>;
    /// 悬浮剪贴板发键之前交还键盘焦点：让目标应用成为接收按键的一方，并等悬浮窗不再是 key 窗口（有上限）。交还不了时
    /// 返回 false，粘贴改为只复制——否则粘贴键会落进悬浮窗自己。**会阻塞**（主线程往返）
    fn float_yield(&self, target: &AppRef) -> bool;
    /// 悬浮剪贴板已就绪（页面已订阅事件）：`clipboard:changed` 同时推给它，投递视为「已显示」
    fn float_ready(&self) -> bool;
    /// 向悬浮剪贴板推送 `clipboard:changed`
    fn emit_float(&self, change: &ClipboardChanged);
    /// 连续粘贴的进度变了：推送 `clipboard:queue`（面板与悬浮剪贴板）、刷新托盘；`toast` 给出时（每粘贴一条）在原面板
    /// 位置以 toast 视图提示进度——面板正显示着、或悬浮剪贴板开着（它自己显示进度）时不弹。**会阻塞**（toast 要主线程往返）
    fn queue_changed(&self, status: &QueueStatus, toast: Option<ToastPayload>);
    /// 连续粘贴期间的全局 Esc（按下即结束队列）。**会阻塞**（注册全局快捷键要与主线程往返）
    fn queue_escape_guard(&self, active: bool);
    /// 面板会话结束的通知（值只是信号）；拿不到时返回 None，空闲清理退回按静默期重新检查
    fn panel_hidden(&self) -> Option<tokio::sync::watch::Receiver<u64>>;
    /// 向面板推送 `clipboard:changed`
    fn emit(&self, change: &ClipboardChanged);
    /// 向设置窗口推送 `ocr_history:changed`（设置窗口没开时丢弃，打开时会重新拉列表）
    fn emit_ocr_history(&self, change: &OcrHistoryChanged);
    /// 立即隐藏剪贴板面板的这个会话（`HideReason::Paste`：无退出动画、不还原焦点，粘贴流程自己负责焦点）。
    /// **先订阅**会话结束的通知再隐藏，返回「窗口已离屏」的等待端（[`OffscreenWait`]；生产实现是 `finish_hide` 发出这个会话的
    /// 结束通知时收到一个值的接收端）；拿不到通知通道时返回 None。不阻塞
    fn hide_for_paste(&self, session: u64) -> Option<Box<dyn OffscreenWait>>;
    /// 以 toast 视图不激活地显示粘贴结果。**会阻塞**（主线程往返），只在阻塞线程上调用
    fn show_toast(&self, toast: ToastPayload);
    /// 跳过疑似密钥时托盘闪一次（是否闪由 `secret_tray_flash` 决定，在托盘模块里判断）
    fn flash_tray(&self);
    /// 暂停状态变了：写 state.json、刷新托盘与 `settings:changed` 里的运行时信息
    fn paused_changed(&self, paused: bool);
    /// state.json 的 `last_vacuum_ms`
    fn last_vacuum_ms(&self) -> i64;
    /// 记下这次 VACUUM 的时间。**会阻塞**（写盘）
    fn set_last_vacuum_ms(&self, ms: i64);
    /// 粘贴埋点：选中条目 → 模拟粘贴按键发出
    fn perf_paste(&self, method: &str, selected_at: Instant, posted_at: Instant);
    /// Wisp 自己（`copy_text` 写入的内容以它为来源）
    fn self_app(&self) -> AppRef;
    /// 剪贴板历史因数据库来自更新版本而停用：提示用户升级（每个进程最多一次，由调用方保证）
    fn notify_history_too_new(&self);
    /// 当前时间（Unix 毫秒）
    fn now_ms(&self) -> i64;
}

/// 后台执行器
pub(crate) trait Exec: Send + Sync {
    /// 在阻塞线程池里执行（不等结果）
    fn blocking(&self, job: Box<dyn FnOnce() + Send + 'static>);
    /// `delay` 之后执行一次（一次性、不周期；任务本身不得阻塞）
    fn after(&self, delay: Duration, job: Box<dyn FnOnce() + Send + 'static>);
}

/// 生产执行器：Tauri 的阻塞线程池（线程按需创建、空闲后自动回收，空闲时不占 CPU）与异步运行时上的一次性定时器
pub(crate) struct PoolExec;

impl Exec for PoolExec {
    fn blocking(&self, job: Box<dyn FnOnce() + Send + 'static>) {
        // 结果不需要：任务内部自己记日志。JoinHandle 被 drop 不会取消任务
        drop(tauri::async_runtime::spawn_blocking(job));
    }

    fn after(&self, delay: Duration, job: Box<dyn FnOnce() + Send + 'static>) {
        drop(tauri::async_runtime::spawn(async move {
            tokio::time::sleep(delay).await;
            job();
        }));
    }
}

/// 当前 Unix 毫秒
pub(crate) fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// 粘贴时等面板离屏的上限（`HideReason::Paste` 立即 orderOut，通常一两帧内完成；超时只记日志，照常发键）
pub(crate) const PASTE_HIDE_WAIT: Duration = Duration::from_millis(100);

/// 「面板已离屏」的等待端（[`Host::hide_for_paste`] 的返回值）。粘贴流程只经它等待、自己不计时：单测的 mock 在被调用的那一刻
/// 按设定给出「离屏通知已到」或「等到上限也没来」，把「发键之前等了没有、上限是多少」变成调用序列里的断言，不依赖墙钟
/// （按墙钟判定的话，负载高的机器上并行跑测试时会偶发先发键）
pub(crate) trait OffscreenWait: Send {
    /// 等离屏通知，最多 `cap`：收到返回 true；超时或通知通道已关闭返回 false
    fn wait(self: Box<Self>, cap: Duration) -> bool;
}

impl OffscreenWait for mpsc::Receiver<()> {
    fn wait(self: Box<Self>, cap: Duration) -> bool {
        self.recv_timeout(cap).is_ok()
    }
}

/// 等某个面板会话的离屏通知（`panel_offscreen` 里出现这个会话），最多 `timeout`。`rx` 必须在隐藏**之前**订阅并标为
/// 已读，否则离屏得快时通知会在订阅之前发出、被错过。别的会话的通知忽略；通道关闭或超时返回 false
pub(crate) async fn wait_session_ended(
    rx: &mut tokio::sync::watch::Receiver<u64>,
    session: u64,
    timeout: Duration,
) -> bool {
    let ended = async {
        loop {
            if rx.changed().await.is_err() {
                return false;
            }
            if *rx.borrow_and_update() == session {
                return true;
            }
        }
    };
    tokio::time::timeout(timeout, ended).await.unwrap_or(false)
}

/// 生产宿主
pub(crate) struct AppHost {
    app: tauri::AppHandle,
    settings: Arc<SettingsService>,
    perf: Arc<PerfMarks>,
}

impl AppHost {
    pub(crate) fn new(app: tauri::AppHandle, settings: Arc<SettingsService>, perf: Arc<PerfMarks>) -> Self {
        Self { app, settings, perf }
    }

    /// 窗口层（AppState 尚未 manage 时为 None：服务在 bootstrap 中先于 AppState 构造）
    fn windows(&self) -> Option<Arc<WindowManager>> {
        self.app.try_state::<AppState>().map(|s| Arc::clone(&s.windows))
    }
}

impl Host for AppHost {
    fn settings(&self) -> Arc<Settings> {
        self.settings.get()
    }

    fn clipboard_view(&self) -> ClipView {
        let Some(windows) = self.windows() else {
            return ClipView::Hidden;
        };
        clip_view_of(windows.panel_state().map(|p| (p.session_id, p.view)), |s| {
            windows.panel_shown(s)
        })
    }

    fn panel_visible(&self) -> bool {
        self.windows().is_some_and(|w| w.panel_state().is_some())
    }

    fn session_live(&self, session: u64) -> bool {
        self.windows().is_some_and(|w| w.panel_session_live(session))
    }

    fn panel_prev_app(&self, session: u64) -> Option<AppRef> {
        self.windows()?.panel_prev_app(session)
    }

    fn float_yield(&self, target: &AppRef) -> bool {
        // 悬浮窗没开（例如刚关掉）时没有要交还的
        self.windows().is_none_or(|w| w.float_yield(target))
    }

    fn float_ready(&self) -> bool {
        self.windows().is_some_and(|w| w.float_ready())
    }

    fn emit_float(&self, change: &ClipboardChanged) {
        if let Err(e) = self.app.emit_to(super::FLOAT_LABEL, event::CLIPBOARD_CHANGED, change) {
            log::debug!("emit clipboard:changed to the floating clipboard failed: {e}");
        }
    }

    fn queue_changed(&self, status: &QueueStatus, toast: Option<ToastPayload>) {
        for label in [PANEL_LABEL, super::FLOAT_LABEL] {
            if let Err(e) = self.app.emit_to(label, event::CLIPBOARD_QUEUE, status) {
                log::debug!("emit clipboard:queue failed: {e}");
            }
        }
        crate::tray::refresh(&self.app);
        let Some(toast) = toast else { return };
        let Some(windows) = self.windows() else { return };
        if queue_toast_shown(windows.panel_state().map(|s| s.view), windows.float_ready()) {
            windows.panel_show_toast(toast);
        }
    }

    fn queue_escape_guard(&self, active: bool) {
        if let Some(state) = self.app.try_state::<AppState>() {
            state.hotkeys.queue_esc_guard(active);
        }
    }

    fn panel_hidden(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        self.windows().map(|w| w.panel_hidden())
    }

    fn emit(&self, change: &ClipboardChanged) {
        if let Err(e) = self.app.emit_to(PANEL_LABEL, event::CLIPBOARD_CHANGED, change) {
            log::debug!("emit clipboard:changed failed: {e}");
        }
    }

    fn emit_ocr_history(&self, change: &OcrHistoryChanged) {
        if let Err(e) = self.app.emit_to(SETTINGS_LABEL, event::OCR_HISTORY_CHANGED, change) {
            log::debug!("emit ocr_history:changed failed: {e}");
        }
    }

    fn hide_for_paste(&self, session: u64) -> Option<Box<dyn OffscreenWait>> {
        let windows = self.windows()?;
        // 先订阅再隐藏：macOS 的 orderOut 投递到主线程执行，窗口离屏后 finish_hide 才发出这个会话的离屏通知。
        // 订阅的是 panel_offscreen 而不是 panel_hidden——后者在隐藏开始时就通知，那时窗口还在屏幕上
        let mut hidden = windows.panel_offscreen();
        hidden.borrow_and_update();
        windows.panel_hide_now(session, HideReason::Paste);
        let (tx, rx) = mpsc::sync_channel(1);
        drop(tauri::async_runtime::spawn(async move {
            if wait_session_ended(&mut hidden, session, PASTE_HIDE_WAIT).await {
                let _ = tx.send(());
            }
        }));
        Some(Box::new(rx))
    }

    fn show_toast(&self, toast: ToastPayload) {
        if let Some(windows) = self.windows() {
            windows.panel_show_toast(toast);
        }
    }

    fn flash_tray(&self) {
        crate::tray::flash(&self.app);
    }

    fn paused_changed(&self, paused: bool) {
        // 高频切换也只合并写一次盘；托盘与设置窗口立即刷新（运行时信息里的 clipboard_paused）
        self.settings
            .state_arc()
            .update_deferred(move |s| s.clipboard_paused = paused);
        self.settings.notify_changed();
        crate::tray::refresh(&self.app);
    }

    fn last_vacuum_ms(&self) -> i64 {
        self.settings.state().read(|s| s.last_vacuum_ms)
    }

    fn set_last_vacuum_ms(&self, ms: i64) {
        if let Err(e) = self.settings.state().update(&mut |s| s.last_vacuum_ms = ms) {
            log::warn!("record last_vacuum_ms failed: {}", e.message);
        }
    }

    fn perf_paste(&self, method: &str, selected_at: Instant, posted_at: Instant) {
        self.perf.paste(method, selected_at, posted_at);
    }

    fn self_app(&self) -> AppRef {
        AppRef {
            id: self.app.config().identifier.clone(),
            alt_id: None,
            name: self
                .app
                .config()
                .product_name
                .clone()
                .unwrap_or_else(|| "Wisp".to_owned()),
            pid: Some(std::process::id()),
            path: None,
            window: None,
        }
    }

    fn notify_history_too_new(&self) {
        let lang = crate::app::i18n::resolve(self.settings.get().general.ui_language);
        if let Err(e) = self
            .app
            .notification()
            .builder()
            .title(texts::tr(lang, Text::HistoryTooNewTitle))
            .body(texts::tr(lang, Text::HistoryTooNewBody))
            .show()
        {
            log::warn!("clipboard history notice failed: {e}");
        }
    }

    fn now_ms(&self) -> i64 {
        unix_ms()
    }
}
