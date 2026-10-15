//! 编排器与外界的接缝：设置、钥匙串、LLM、平台（取词 / 焦点 / 剪贴板 / 截图）、窗口层、埋点。
//!
//! 状态机（`engine.rs`）只通过 [`Env`] 产生副作用，因此可以在没有 Tauri 窗口的单元测试里用内存实现驱动它
//! （`wisp_llm::fake` + 内存版的平台 mock）。
//! 真实实现 [`TauriEnv`] 把每个方法转给 `AppState` 里的服务。
//!
//! **阻塞**：标「阻塞」的方法会等主线程、钥匙串或系统调用，核心只在 `spawn_blocking` 里调用它们；
//! 其余方法只读状态或只投递，可以在异步上下文里直接调用。
//!
//! **平台条件编译**（只有 wisp-platform / wisp-capture / windows/native / lib.rs 使用 `cfg(target_os)`）：
//! 编排器里没有。原生冻结帧的判断用 wisp-capture 的跨平台 `Frame::has_native_image()`；
//! 按操作系统区分的其余行为（UIA 预热、钥匙串会不会弹框）都是运行期判断（`OsKind::current()`，与
//! `PlatformInfo.os` 同一来源）。核心与流程测试只经 [`Env`] 看到这些判断。

use std::sync::Arc;
use std::time::Duration;

use tauri::{Emitter, Manager};
use tokio::sync::{mpsc, watch};
use wisp_capture::{CaptureError, CaptureSet, Frame};
use wisp_llm::{CancellationToken, ChatRequest, Secret};
use wisp_platform::{Selection, SelectionError, SelectionOpts};
use wisp_prompts::GlossaryMatcher;
use wisp_types::{
    AppRef, ClipboardPayload, HideReason, HotkeyAction, KeychainState, MonitorId, MonitorInfo, OnboardingHotkey,
    OsKind, OverlayEndReason, OverlaySessionEnd, OverlaySessionStart, PanelPayload, PanelView, PasteOn, PermissionKind,
    PermissionState, ReduceMotionPref, SessionKind, Settings, StreamEvent, Style, Usage, WindowTarget, event,
};

use super::backend::Backend;
use crate::app::perf::PerfMarks;
use crate::clipboard_svc::ocr_history::OcrRecord;
use crate::error::IpcError;
use crate::settings::SettingsService;
use crate::state::{AppState, Deps};
use crate::windows::overlay::OVERLAY_PREFIX;
use crate::windows::settings_window::ONBOARDING_LABEL;
use crate::windows::{HiddenSet, OverlaySession, PanelShowRequest, PanelState};

/// 编排器需要的全部外部能力
pub trait Env: Send + Sync + 'static {
    // ---- 设置与凭据 ----
    fn settings(&self) -> Arc<Settings>;
    fn styles(&self) -> Arc<Vec<Style>>;
    fn glossary(&self) -> Arc<GlossaryMatcher>;
    fn keychain_state(&self) -> KeychainState;
    /// 是否已保存 Key（按索引回答，不读明文）
    fn has_key(&self, provider_id: &str) -> bool;
    /// 读 Key（**阻塞**：系统钥匙串，macOS 可能弹授权框）
    fn key_get(&self, provider_id: &str) -> Result<Option<Secret>, IpcError>;
    /// 读 Key 是否可能弹系统对话框（macOS 钥匙串授权、Linux keyring 解锁）。Windows 凭据管理器、Key 只在内存里
    /// （钥匙串不可用 / 仅本次会话）时为 false。编排器据此决定能不能在触发的瞬间预读
    fn keychain_may_prompt(&self) -> bool;
    /// 用量统计（内存合并、稍后落盘，不阻塞）
    fn record_usage(&self, provider_id: &str, model_id: &str, usage: &Usage);

    // ---- LLM ----
    /// 发起流式请求（必须在 tokio 运行时里调用）
    fn llm_stream(&self, req: ChatRequest, cancel: CancellationToken) -> mpsc::Receiver<StreamEvent>;

    // ---- 平台 ----
    /// 当前前台应用（**阻塞**）
    fn frontmost(&self) -> Option<AppRef>;
    /// 取词（**阻塞**：AX / UIA / PRIMARY，必要时模拟复制）
    fn read_selection(&self, opts: &SelectionOpts) -> Result<Selection, SelectionError>;
    /// 交还焦点（**阻塞**）
    fn activate(&self, app: &AppRef);
    /// 写剪贴板（会进入历史；**阻塞**）
    fn copy_text(&self, text: &str) -> Result<(), IpcError>;
    /// 当前是否是 Wayland 会话
    fn wayland(&self) -> bool;
    /// 解析后的「减少动效」（**阻塞**：macOS 读系统偏好）
    fn reduce_motion(&self) -> bool;
    /// Windows 的 UIA 冷启动预热（**阻塞**；其他平台空操作）
    fn prewarm_selection(&self);
    /// 帧是否带原生冻结帧（macOS SCK 帧；`from_rgba` 假帧、Windows / X11 为 false）：带的显示器建好 Overlay 立即
    /// `overlay_show(Some(frame))`
    fn frame_is_native(&self, frame: &Frame) -> bool;

    // ---- 面板 ----
    /// **阻塞**
    fn panel_prepare(&self, req: PanelShowRequest) -> Option<u64>;
    fn panel_session_live(&self, session_id: u64) -> bool;
    fn panel_state(&self) -> Option<PanelState>;
    fn panel_prev_app(&self, session_id: u64) -> Option<AppRef>;
    fn panel_request_hide(&self, reason: HideReason);
    fn view_width(&self, view: PanelView, payload: &PanelPayload) -> f64;
    fn panel_card_width(&self, session_id: u64) -> Option<f64>;
    /// 面板会话结束的通知（[`crate::windows::WindowManager::panel_hidden`]）；窗口层还没就绪时为 None
    fn panel_hidden(&self) -> Option<watch::Receiver<u64>>;

    // ---- 埋点 ----
    fn perf(&self) -> &PerfMarks;
    fn perf_finish(&self, trace: u64);

    // ---- 截图 ----
    /// 截图预热（非阻塞，热键回调里第一件事）
    fn capture_prewarm(&self);
    /// 屏幕录制权限预检（不弹窗）
    fn screen_permission(&self) -> PermissionState;
    /// 请求屏幕录制权限（`CGRequestScreenCaptureAccess`，系统只弹一次；**阻塞**）
    fn request_screen_permission(&self);
    /// **阻塞**（≤ 约 300 ms + 一帧）
    fn hide_for_capture(&self) -> HiddenSet;
    /// **阻塞**
    fn restore_after_capture(&self, set: HiddenSet);
    fn monitors(&self) -> Vec<MonitorInfo>;
    /// **阻塞**
    fn capture_all(&self, monitors: &[MonitorInfo]) -> Result<CaptureSet, CaptureError>;
    /// **阻塞**
    fn create_overlays(&self, session: &OverlaySession) -> Result<(), IpcError>;
    /// **阻塞**；返回是否用上了原生冻结帧
    fn overlay_show(&self, session_id: u64, monitor_id: &str, frozen: Option<&Frame>) -> Result<bool, IpcError>;
    fn overlay_monitor(&self, label: &str) -> Option<MonitorId>;
    /// 通知某块显示器的 Overlay 页面重新 init（emit_to `overlay:session`）
    fn overlay_session_started(&self, session_id: u64, monitor_id: &str);
    /// 截图会话（未 commit 的 Overlay）期间的全局 Esc（**阻塞**：快捷键插件与主线程往返）
    fn overlay_escape_guard(&self, active: bool);
    /// 光标所在的显示器（多屏时按键焦点跟随光标）
    fn cursor_monitor(&self) -> Option<MonitorId>;
    /// 让这块显示器的 Overlay 成为 key、接收键盘（不激活应用；**阻塞**）
    fn overlay_focus(&self, session_id: u64, monitor_id: &str);
    /// 多屏截图时窗口层自己让按键焦点跟随指针（macOS：Overlay 上的 NSTrackingArea），
    /// 编排器不必每 50 ms 轮询光标所在屏。默认 false（测试替身照旧走轮询路径）
    fn overlay_follows_pointer(&self) -> bool {
        false
    }
    /// 截图会话 commit 了——窗口层不再让按键焦点跟随指针切到别的屏（macOS 的
    /// NSTrackingArea；飞入 / 淡出期间按键留在 commit 的那块屏）。不阻塞（只投递）；默认实现为空（测试替身不关心，
    /// 轮询路径本来就只在未 commit 时切换）
    fn overlay_committed(&self, session_id: u64) {
        let _ = session_id;
    }
    /// 截图会话即将结束——emit_to 绑定该会话的每个 `overlay-<n>` 一条 `overlay:end`
    /// （`OverlaySessionEnd`），页面据此收尾、释放冻结帧（Windows 复用的预建 Overlay 隐藏后不留大块内存）。
    /// 必须在拆除（隐藏 / 销毁、解除窗口与会话的绑定）**之前**调用；默认实现为空（测试替身不关心）。
    /// 调用点（全局 Esc、再按截图快捷键、看门狗、commit 之后的 done）在窗口层与编排器里
    fn overlay_session_ended(&self, session_id: u64, reason: OverlayEndReason) {
        let _ = (session_id, reason);
    }
    /// **阻塞**。`fade_out`：commit 之后的正常收尾，macOS 先淡出窗口再销毁
    fn destroy_overlays(&self, fade_out: bool);

    // ---- 其他服务 ----
    /// 引导窗口是否存在（不阻塞）
    fn onboarding_open(&self) -> bool;
    /// 引导窗口是否在前台（**阻塞**：查询窗口焦点要等主线程）
    fn onboarding_focused(&self) -> bool;
    fn emit_onboarding_hotkey(&self, action: HotkeyAction);
    fn clipboard_payload(&self) -> ClipboardPayload;
    /// 当前剪贴板内容已经记进历史（正在记录且最近记录的序号就是现在的序号；**阻塞**，微秒级）。取词的模拟复制备份失败时
    /// 只有这时才能放弃恢复（不能只看 `clipboard.enabled`：暂停与隐私规则跳过的内容不在历史里）
    fn clipboard_has_current(&self) -> bool;
    fn toggle_clipboard_pause(&self);
    fn clear_clipboard(&self, keep_pinned: bool);
    fn check_update(&self);
    fn open_window(&self, target: WindowTarget);
    fn quit(&self);
    /// 记一条 OCR 历史——`ocr.history` 是否开启、按 `ocr.history_limit` 裁剪、
    /// 推送 `ocr_history:changed` 都由剪贴板服务负责（开关关闭时什么都不做，返回 None）。**阻塞**（数据库、写图片）。
    /// 默认实现为空（测试替身不关心）
    fn record_ocr_history(&self, rec: OcrRecord) -> Option<i64> {
        let _ = rec;
        None
    }
}

/// 真实实现：转给 `AppState` 里的服务
pub struct TauriEnv {
    app: tauri::AppHandle,
    settings: Arc<SettingsService>,
    platform: Arc<wisp_platform::Platform>,
    perf: Arc<PerfMarks>,
    backend: Backend,
}

impl TauriEnv {
    pub fn new(d: &Deps) -> Self {
        Self {
            app: d.app.clone(),
            settings: Arc::clone(&d.settings),
            platform: Arc::clone(&d.platform),
            perf: Arc::clone(&d.perf),
            backend: Backend::from_env(),
        }
    }

    fn state(&self) -> Option<tauri::State<'_, AppState>> {
        self.app.try_state::<AppState>()
    }
}

impl Env for TauriEnv {
    fn settings(&self) -> Arc<Settings> {
        self.settings.get()
    }

    fn styles(&self) -> Arc<Vec<Style>> {
        self.settings.styles()
    }

    fn glossary(&self) -> Arc<GlossaryMatcher> {
        self.settings.glossary()
    }

    fn keychain_state(&self) -> KeychainState {
        self.settings.keychain().state()
    }

    fn has_key(&self, provider_id: &str) -> bool {
        self.settings.keychain().has(provider_id)
    }

    fn key_get(&self, provider_id: &str) -> Result<Option<Secret>, IpcError> {
        self.settings.keychain().get(provider_id)
    }

    fn keychain_may_prompt(&self) -> bool {
        OsKind::current() != OsKind::Windows && self.keychain_state() == KeychainState::Available
    }

    fn record_usage(&self, provider_id: &str, model_id: &str, usage: &Usage) {
        self.settings.state_arc().record_usage(
            provider_id,
            model_id,
            u64::from(usage.input_tokens),
            u64::from(usage.output_tokens),
        );
    }

    fn llm_stream(&self, req: ChatRequest, cancel: CancellationToken) -> mpsc::Receiver<StreamEvent> {
        match self.state() {
            Some(state) => self.backend.stream(&state.llm, req, cancel),
            None => self.backend.unavailable(req),
        }
    }

    fn frontmost(&self) -> Option<AppRef> {
        self.platform.focus.frontmost()
    }

    fn read_selection(&self, opts: &SelectionOpts) -> Result<Selection, SelectionError> {
        self.platform.selection.get(opts)
    }

    fn activate(&self, app: &AppRef) {
        if let Err(e) = self.platform.focus.activate(app) {
            log::debug!("activate previous app failed: {e}");
        }
    }

    fn copy_text(&self, text: &str) -> Result<(), IpcError> {
        match self.state() {
            Some(state) => state.clipboard.copy_text(text),
            None => Err(IpcError::internal("app state is not ready")),
        }
    }

    fn wayland(&self) -> bool {
        self.platform.info().session == SessionKind::Wayland
    }

    fn reduce_motion(&self) -> bool {
        match self.settings.get().appearance.reduce_motion {
            ReduceMotionPref::On => true,
            ReduceMotionPref::Off => false,
            ReduceMotionPref::System => self.platform.screen.reduce_motion(),
        }
    }

    fn prewarm_selection(&self) {
        // 只建 UIA 线程与对象，不读取选区内容（deadline 为 0）。只在 Windows 上做：macOS / X11 的
        // `has_selection` 不是空操作（会做一次 AX / X11 查询）
        if OsKind::current() == OsKind::Windows {
            let _ = self.platform.selection.has_selection(Duration::ZERO);
        }
    }

    fn frame_is_native(&self, frame: &Frame) -> bool {
        frame.has_native_image()
    }

    fn panel_prepare(&self, req: PanelShowRequest) -> Option<u64> {
        self.state()?.windows.panel_prepare(req)
    }

    fn panel_session_live(&self, session_id: u64) -> bool {
        self.state().is_some_and(|s| s.windows.panel_session_live(session_id))
    }

    fn panel_state(&self) -> Option<PanelState> {
        self.state()?.windows.panel_state()
    }

    fn panel_prev_app(&self, session_id: u64) -> Option<AppRef> {
        self.state()?.windows.panel_prev_app(session_id)
    }

    fn panel_request_hide(&self, reason: HideReason) {
        if let Some(s) = self.state() {
            s.windows.panel_request_hide(reason);
        }
    }

    fn view_width(&self, view: PanelView, payload: &PanelPayload) -> f64 {
        self.state().map_or(0.0, |s| s.windows.view_width(view, payload))
    }

    fn panel_card_width(&self, session_id: u64) -> Option<f64> {
        self.state()?.windows.panel_card_width(session_id)
    }

    fn panel_hidden(&self) -> Option<watch::Receiver<u64>> {
        Some(self.state()?.windows.panel_hidden())
    }

    fn perf(&self) -> &PerfMarks {
        &self.perf
    }

    fn perf_finish(&self, trace: u64) {
        self.perf.finish(trace, &self.app);
    }

    fn capture_prewarm(&self) {
        wisp_capture::prewarm();
    }

    fn screen_permission(&self) -> PermissionState {
        wisp_capture::preflight()
    }

    fn request_screen_permission(&self) {
        if let Err(e) = self.platform.permissions.request(PermissionKind::ScreenRecording) {
            log::debug!("request screen recording permission failed: {e}");
        }
    }

    fn hide_for_capture(&self) -> HiddenSet {
        self.state().map(|s| s.windows.hide_for_capture()).unwrap_or_default()
    }

    fn restore_after_capture(&self, set: HiddenSet) {
        if let Some(s) = self.state() {
            s.windows.restore_after_capture(set);
        }
    }

    fn monitors(&self) -> Vec<MonitorInfo> {
        self.platform.screen.monitors()
    }

    fn capture_all(&self, monitors: &[MonitorInfo]) -> Result<CaptureSet, CaptureError> {
        wisp_capture::capture_all(monitors, std::process::id())
    }

    fn create_overlays(&self, session: &OverlaySession) -> Result<(), IpcError> {
        match self.state() {
            Some(s) => s.windows.create_overlays(session),
            None => Err(IpcError::internal("app state is not ready")),
        }
    }

    fn overlay_show(&self, session_id: u64, monitor_id: &str, frozen: Option<&Frame>) -> Result<bool, IpcError> {
        match self.state() {
            Some(s) => s.windows.overlay_show(session_id, monitor_id, frozen),
            None => Err(IpcError::internal("app state is not ready")),
        }
    }

    fn overlay_monitor(&self, label: &str) -> Option<MonitorId> {
        self.state()?.windows.overlay_monitor(label)
    }

    fn overlay_session_started(&self, session_id: u64, monitor_id: &str) {
        let Some(state) = self.state() else { return };
        let labels: Vec<String> = self
            .app
            .webview_windows()
            .into_keys()
            .filter(|l| l.starts_with(OVERLAY_PREFIX))
            .collect();
        for label in labels {
            if state.windows.overlay_monitor(&label).as_deref() == Some(monitor_id) {
                let payload = OverlaySessionStart {
                    session_id,
                    monitor_id: monitor_id.to_owned(),
                };
                if let Err(e) = self.app.emit_to(label.as_str(), event::OVERLAY_SESSION, payload) {
                    log::warn!("emit overlay:session to {label} failed: {e}");
                }
            }
        }
    }

    fn overlay_escape_guard(&self, active: bool) {
        if let Some(s) = self.state() {
            s.hotkeys.overlay_esc_guard(active);
        }
    }

    fn cursor_monitor(&self) -> Option<MonitorId> {
        Some(self.platform.screen.cursor().0)
    }

    fn overlay_focus(&self, session_id: u64, monitor_id: &str) {
        if let Some(s) = self.state() {
            s.windows.overlay_focus(session_id, monitor_id);
        }
    }

    fn overlay_follows_pointer(&self) -> bool {
        crate::windows::native::OVERLAY_FOLLOWS_POINTER
    }

    fn overlay_committed(&self, session_id: u64) {
        if let Some(s) = self.state() {
            s.windows.overlay_stop_following(session_id);
        }
    }

    fn overlay_session_ended(&self, session_id: u64, reason: OverlayEndReason) {
        let Some(state) = self.state() else { return };
        let labels: Vec<String> = self
            .app
            .webview_windows()
            .into_keys()
            .filter(|l| l.starts_with(OVERLAY_PREFIX))
            .collect();
        let payload = OverlaySessionEnd { session_id, reason };
        for label in labels {
            if state.windows.overlay_session_of(&label) == Some(session_id)
                && let Err(e) = self.app.emit_to(label.as_str(), event::OVERLAY_END, payload)
            {
                log::warn!("emit overlay:end to {label} failed: {e}");
            }
        }
    }

    fn destroy_overlays(&self, fade_out: bool) {
        if let Some(s) = self.state() {
            if fade_out {
                s.windows.destroy_overlays_fading();
            } else {
                s.windows.destroy_overlays();
            }
        }
    }

    fn onboarding_open(&self) -> bool {
        self.app.get_webview_window(ONBOARDING_LABEL).is_some()
    }

    fn onboarding_focused(&self) -> bool {
        self.app
            .get_webview_window(ONBOARDING_LABEL)
            .is_some_and(|w| w.is_focused().unwrap_or(false))
    }

    fn emit_onboarding_hotkey(&self, action: HotkeyAction) {
        if let Err(e) = self
            .app
            .emit_to(ONBOARDING_LABEL, event::ONBOARDING_HOTKEY, OnboardingHotkey { action })
        {
            log::warn!("emit onboarding:hotkey failed: {e}");
        }
    }

    fn clipboard_payload(&self) -> ClipboardPayload {
        let settings = self.settings.get();
        let (page, paused, (unavailable, recording)) = match self.state() {
            // 先取首页（可能顺带打开数据库），再读历史状态：打开失败 / 库来自更新版本时横幅随这一次载荷就出现
            Some(s) => {
                let page = s.clipboard.first_page();
                (page, s.clipboard.paused(), s.clipboard.history_state())
            }
            None => (
                wisp_types::ClipPage::empty(wisp_types::ClipFilter::All),
                false,
                (None, wisp_types::ClipboardRecording::Recording),
            ),
        };
        ClipboardPayload {
            page,
            paused,
            hint_bar: settings.clipboard.show_hint_bar,
            paste_on_double_click: settings.clipboard.paste_on == PasteOn::DoubleClick,
            unavailable: unavailable.map(str::to_owned),
            recording,
        }
    }

    fn clipboard_has_current(&self) -> bool {
        self.state().is_some_and(|s| s.clipboard.current_content_recorded())
    }

    fn toggle_clipboard_pause(&self) {
        if let Some(s) = self.state() {
            let paused = s.clipboard.paused();
            s.clipboard.set_paused(!paused);
        }
    }

    fn clear_clipboard(&self, keep_pinned: bool) {
        let Some(s) = self.state() else { return };
        let clipboard = Arc::clone(&s.clipboard);
        tauri::async_runtime::spawn(async move {
            match clipboard.clear(keep_pinned).await {
                Ok(n) => log::info!("clipboard cleared count={n} keep_pinned={keep_pinned}"),
                Err(e) => log::warn!("clear clipboard failed: {}", e.message),
            }
        });
    }

    fn check_update(&self) {
        crate::updater::check_interactive(&self.app);
    }

    fn open_window(&self, target: WindowTarget) {
        if let Some(s) = self.state() {
            s.windows.open_target(target);
        }
    }

    fn quit(&self) {
        self.app.exit(0);
    }

    fn record_ocr_history(&self, rec: OcrRecord) -> Option<i64> {
        self.state()?.clipboard.ocr_history_record(&rec)
    }
}
