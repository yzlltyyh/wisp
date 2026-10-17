//! 编排器状态机的流程测试：用内存版 [`Env`]（取词、面板、截图、钥匙串都是脚本化的假实现）与
//! `wisp_llm::fake` 驱动真实的编排器核心，覆盖取消、重入、缓存命中、长文本、未就绪、所有权转移、截图会话（含 macOS
//! 原生冻结帧的 Pending → overlay:session → init、`overlay_show` 失败、多显示器部分失败、截图中 / Overlay 中被取代）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::{mpsc, watch};
use wisp_capture::{CaptureError, CaptureSet, Frame};
use wisp_llm::fake::{FakeConfig, FakeLlm};
use wisp_llm::{CancellationToken, ChatRequest, Purpose, Secret};
use wisp_local::fake::{FakeEngine, FakeEngineConfig};
use wisp_platform::{CaptureMethod, Selection, SelectionError, SelectionOpts, reason};
use wisp_prompts::GlossaryMatcher;
use wisp_types::{
    AppRef, ClipFilter, ClipPage, ClipboardPayload, DictMode, ErrorCode, HideReason, HotkeyAction, KeychainState,
    LlmError, LocalModelId, MonitorId, MonitorInfo, NotReady, NoticeKind, OcrLayout, OcrMode, OcrReq, OverlayCommit,
    OverlayEndReason, OverlaySelectionKind, PanelPayload, PanelTransition, PanelView, PermissionKind, PermissionState,
    PhysPoint, PhysSize, Rect, Settings, StreamEvent, StreamPhase, Style, TaskKind, TranslateReq, Usage, WindowTarget,
};

use super::backend::fake::output_for;
use super::captures::{CropSource, FrameSource};
use super::engine::{Core, KEY_WAIT};
use super::env::{EngineImage, EngineRequest, Env};
use super::fsm::{Flow, Stage};
use super::not_ready::OfflineStatus;
use super::ocr::delivered_text;
use super::plan::EngineRef;
use super::plan::tests::{configured, model_ref};
use super::registry::Outcome;
use super::registry::tests::Collector;
use super::{RequestCaller, RequestHost};
use crate::app::perf::{PerfMarks, PerfStage};
use crate::assets::AssetSource;
use crate::bus::{Command, TriggerOrigin};
use crate::clipboard_svc::paste::{Recorded, succession};
use crate::clipboard_svc::smart::layout_text;
use crate::clipboard_svc::{CopiedItem, CopyReceipt};
use crate::error::IpcError;
use crate::windows::{HiddenSet, OverlaySession, PanelShowRequest, PanelState};

const MONITOR: &str = "m1";
const WAIT: Duration = Duration::from_secs(5);

/// 一次脚本化的取词
struct SelScript {
    delay: Duration,
    result: Result<Selection, SelectionError>,
}

fn selection(text: &str) -> Result<Selection, SelectionError> {
    Ok(Selection {
        text: text.to_owned(),
        truncated: false,
        anchor: None,
        source: None,
        method: CaptureMethod::Ax,
        elapsed_ms: 3,
    })
}

fn monitor() -> MonitorInfo {
    monitor_at(MONITOR, 0.0)
}

fn monitor_at(id: &str, x: f64) -> MonitorInfo {
    MonitorInfo {
        id: id.to_owned(),
        name: id.to_owned(),
        physical_origin: PhysPoint { x: 0, y: 0 },
        physical_size: PhysSize {
            width: 400,
            height: 200,
        },
        scale_factor: 2.0,
        frame: Rect::new(x, 0.0, 200.0, 100.0),
        work_area: Rect::new(x, 0.0, 200.0, 100.0),
        is_primary: x == 0.0,
    }
}

/// 内存版环境
struct FakeEnv {
    settings: Mutex<Arc<Settings>>,
    styles: Arc<Vec<Style>>,
    glossary: Arc<GlossaryMatcher>,
    keychain: Mutex<(KeychainState, bool)>,
    key_error: Mutex<Option<IpcError>>,
    selections: Mutex<VecDeque<SelScript>>,
    selection_reads: AtomicUsize,
    /// 首 token 延迟与 chunk 间隔
    llm_pace: Mutex<(Duration, Duration)>,
    /// 按用途覆盖输出
    llm_outputs: Mutex<HashMap<&'static str, String>>,
    llm_calls: Mutex<Vec<(String, Purpose, Instant)>>,
    prepare_delay: Mutex<Duration>,
    prepares: Mutex<Vec<(Instant, PanelShowRequest, u64)>>,
    live: Mutex<Option<(u64, PanelView)>>,
    next_session: AtomicU64,
    hidden_tx: watch::Sender<u64>,
    view_width: Mutex<f64>,
    card_width: Mutex<Option<f64>>,
    perf: Arc<PerfMarks>,
    finished: Mutex<Vec<u64>>,
    copies: Mutex<Vec<String>>,
    /// 假的系统剪贴板：当前的 change_id（每次 `copy_text` 加一；测试里直接加一表示别的程序复制了东西）
    clip_change: AtomicU64,
    /// `copy_text` 新建的历史条目 id（1 起）；被取代而删掉的条目
    clip_items: AtomicI64,
    clip_removed: Mutex<Vec<i64>>,
    /// 假的剪贴板历史里还在的条目（id, 文本）：内容相同的再次复制只把那一条移到顶部（与存储层的去重一致）
    clip_live: Mutex<Vec<(i64, String)>>,
    /// 收藏过的条目：被取代时保留（命名即收藏）
    clip_pinned: Mutex<HashSet<i64>>,
    /// `copy_text` 失败（模拟 Windows 剪贴板被别的程序占着）
    copy_fails: Mutex<bool>,
    usage: Mutex<Vec<Usage>>,
    calls: Mutex<Vec<String>>,
    permission: Mutex<PermissionState>,
    capture_error: Mutex<Option<CaptureError>>,
    onboarding: Mutex<(bool, bool)>,
    onboarding_hotkeys: Mutex<Vec<HotkeyAction>>,
    /// 找不到任何显示器：`panel_prepare` 返回 None
    no_monitor: Mutex<bool>,
    /// 显示器（默认一块）
    monitors: Mutex<Vec<MonitorInfo>>,
    /// 截图帧是否带原生冻结帧（模拟 macOS 的 SCK 帧）
    native_frames: Mutex<bool>,
    /// `overlay_show` 失败的显示器
    overlay_fail: Mutex<HashSet<String>>,
    /// 带原生冻结帧的 `overlay_show` 的耗时（观察 Pending）
    native_show_delay: Mutex<Duration>,
    /// `overlay_session_started` 的时刻
    sessions_started: Mutex<Vec<(String, Instant)>>,
    /// 每次 `capture_all` 的耗时（按调用顺序，用完为 0）
    capture_delays: Mutex<VecDeque<Duration>>,
    destroy_delay: Mutex<Duration>,
    /// 每次 `panel_prepare` 的耗时（按调用顺序，用完后用 `prepare_delay`）
    prepare_delays: Mutex<VecDeque<Duration>>,
    /// 读 Key 的耗时（真实钥匙串 5–30 ms，首次访问可能更久；等用户回答授权框时可能是几秒）
    key_delay: Mutex<Duration>,
    /// 读 Key 可能弹系统对话框（macOS / Linux 的系统钥匙串）；默认 false（Windows 凭据管理器的行为）
    key_prompts: Mutex<bool>,
    /// Wayland 会话
    wayland: Mutex<bool>,
    /// 每次查询引导窗口焦点的耗时（按调用顺序，用完为 0）与已开始的查询次数
    onboarding_delays: Mutex<VecDeque<Duration>>,
    onboarding_checks: AtomicUsize,
    /// 当前面板是否是 key 窗口（prepare 时按 take_focus 设置）
    panel_key: Mutex<bool>,
    /// 截图会话的全局 Esc 是否注册着
    esc_guard: Mutex<bool>,
    /// 光标所在的显示器（None = 查不到）
    cursor: Mutex<Option<MonitorId>>,
    /// `overlay_focus` 依次给过的显示器
    focused_overlays: Mutex<Vec<String>>,
    /// `overlay:end` 依次发出的（会话, 原因）
    overlay_ends: Mutex<Vec<(u64, OverlayEndReason)>>,
    /// 窗口层自己让按键焦点跟随指针（macOS 的 NSTrackingArea）
    follows_pointer: Mutex<bool>,
    /// 交给 `record_ocr_history` 的记录；返回的 id 是 1 起的序号
    ocr_history: Mutex<Vec<crate::clipboard_svc::ocr_history::OcrRecord>>,
    /// `record_ocr_history` 每次写库前的人为延迟（依次取用，用完为 0；模拟机器繁忙时的慢写入，覆盖「取代」的竞态）
    ocr_history_delay: Mutex<std::collections::VecDeque<Duration>>,
    /// `overlay_committed` 依次给过的会话（commit 之后窗口层停止跟随指针）
    committed: Mutex<Vec<u64>>,
    /// 每次 `llm_stream` 的服务商 id（离线翻译模型是 `@offline`）
    llm_providers: Mutex<Vec<String>>,
    /// 离线模型的就绪情况（没写的按「没下载」）
    offline: Mutex<HashMap<LocalModelId, OfflineStatus>>,
    /// 识别引擎的脚本化输出（阶段、延迟、失败）
    engine: Mutex<FakeEngineConfig>,
    /// 每次 `ocr_engine_stream`：（引擎, 模式, 交来的是不是像素）
    engine_calls: Mutex<Vec<(EngineRef, OcrMode, bool)>>,
}

impl FakeEnv {
    fn new(settings: Settings) -> Arc<Self> {
        let (hidden_tx, _) = watch::channel(0);
        Arc::new(Self {
            settings: Mutex::new(Arc::new(settings)),
            styles: Arc::new(wisp_prompts::builtin_styles()),
            glossary: Arc::new(GlossaryMatcher::default()),
            keychain: Mutex::new((KeychainState::Available, true)),
            key_error: Mutex::new(None),
            selections: Mutex::new(VecDeque::new()),
            selection_reads: AtomicUsize::new(0),
            llm_pace: Mutex::new((Duration::from_millis(5), Duration::from_millis(2))),
            llm_outputs: Mutex::new(HashMap::new()),
            llm_calls: Mutex::new(Vec::new()),
            prepare_delay: Mutex::new(Duration::ZERO),
            prepares: Mutex::new(Vec::new()),
            live: Mutex::new(None),
            next_session: AtomicU64::new(0),
            hidden_tx,
            view_width: Mutex::new(360.0),
            card_width: Mutex::new(None),
            perf: Arc::new(PerfMarks::new()),
            finished: Mutex::new(Vec::new()),
            copies: Mutex::new(Vec::new()),
            clip_change: AtomicU64::new(100),
            clip_items: AtomicI64::new(0),
            clip_removed: Mutex::new(Vec::new()),
            clip_live: Mutex::new(Vec::new()),
            clip_pinned: Mutex::new(HashSet::new()),
            copy_fails: Mutex::new(false),
            usage: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            permission: Mutex::new(PermissionState::Granted),
            capture_error: Mutex::new(None),
            onboarding: Mutex::new((false, false)),
            onboarding_hotkeys: Mutex::new(Vec::new()),
            no_monitor: Mutex::new(false),
            monitors: Mutex::new(vec![monitor()]),
            native_frames: Mutex::new(false),
            overlay_fail: Mutex::new(HashSet::new()),
            native_show_delay: Mutex::new(Duration::ZERO),
            sessions_started: Mutex::new(Vec::new()),
            capture_delays: Mutex::new(VecDeque::new()),
            destroy_delay: Mutex::new(Duration::ZERO),
            prepare_delays: Mutex::new(VecDeque::new()),
            key_delay: Mutex::new(Duration::ZERO),
            key_prompts: Mutex::new(false),
            wayland: Mutex::new(false),
            onboarding_delays: Mutex::new(VecDeque::new()),
            onboarding_checks: AtomicUsize::new(0),
            panel_key: Mutex::new(false),
            esc_guard: Mutex::new(false),
            cursor: Mutex::new(Some(MONITOR.to_owned())),
            focused_overlays: Mutex::new(Vec::new()),
            overlay_ends: Mutex::new(Vec::new()),
            follows_pointer: Mutex::new(false),
            ocr_history: Mutex::new(Vec::new()),
            ocr_history_delay: Mutex::new(std::collections::VecDeque::new()),
            committed: Mutex::new(Vec::new()),
            llm_providers: Mutex::new(Vec::new()),
            offline: Mutex::new(HashMap::new()),
            engine: Mutex::new(FakeEngineConfig {
                text: "Settings\nOpen the file to continue.".to_owned(),
                chunk_chars: 4,
                phase: Some(StreamPhase::LoadingModel),
                first_delay: Duration::from_millis(5),
                chunk_delay: Duration::from_millis(1),
                fail: None,
            }),
            engine_calls: Mutex::new(Vec::new()),
        })
    }

    /// 某个离线模型已经可以用
    fn offline_ready(&self, model: LocalModelId) {
        self.offline.lock().insert(model, OfflineStatus::Ready);
    }

    fn engine_count(&self) -> usize {
        self.engine_calls.lock().len()
    }

    fn count(&self, name: &str) -> usize {
        self.calls.lock().iter().filter(|c| *c == name).count()
    }

    fn live_session(&self) -> Option<u64> {
        self.live.lock().map(|(s, _)| s)
    }

    fn call(&self, name: &str) {
        self.calls.lock().push(name.to_owned());
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().clone()
    }

    fn called(&self, name: &str) -> bool {
        self.calls.lock().iter().any(|c| c == name)
    }

    /// 结束当前面板会话（用户关闭 / 固定交接 / 截图前隐藏）并发出通知
    fn hide(&self, session: u64) {
        {
            let mut live = self.live.lock();
            if live.is_some_and(|(s, _)| s == session) {
                *live = None;
            }
        }
        self.hidden_tx.send_replace(session);
    }

    fn last_prepare(&self) -> Option<(PanelShowRequest, u64)> {
        self.prepares.lock().last().map(|(_, r, s)| (r.clone(), *s))
    }

    fn prepare_count(&self) -> usize {
        self.prepares.lock().len()
    }

    fn llm_count(&self) -> usize {
        self.llm_calls.lock().len()
    }
}

impl Env for FakeEnv {
    fn settings(&self) -> Arc<Settings> {
        Arc::clone(&self.settings.lock())
    }
    fn styles(&self) -> Arc<Vec<Style>> {
        Arc::clone(&self.styles)
    }
    fn glossary(&self) -> Arc<GlossaryMatcher> {
        Arc::clone(&self.glossary)
    }
    fn keychain_state(&self) -> KeychainState {
        self.keychain.lock().0
    }
    fn has_key(&self, _: &str) -> bool {
        self.keychain.lock().1
    }
    fn key_get(&self, _: &str) -> Result<Option<Secret>, IpcError> {
        self.call("key_get");
        let delay = *self.key_delay.lock();
        std::thread::sleep(delay);
        match self.key_error.lock().clone() {
            Some(e) => Err(e),
            None => Ok(Some(Secret::new("sk-test".to_owned()))),
        }
    }
    fn keychain_may_prompt(&self) -> bool {
        *self.key_prompts.lock()
    }
    fn record_usage(&self, _: &str, _: &str, usage: &Usage) {
        self.usage.lock().push(*usage);
    }
    fn llm_stream(&self, req: ChatRequest, cancel: CancellationToken) -> mpsc::Receiver<StreamEvent> {
        self.call("llm_stream");
        self.llm_providers.lock().push(req.provider.id.clone());
        self.llm_calls
            .lock()
            .push((req.request_id.clone(), req.purpose, Instant::now()));
        let key = match req.purpose {
            Purpose::Translate => "translate",
            Purpose::Ocr => "ocr",
            Purpose::Reflect => "reflect",
            Purpose::Probe => "probe",
        };
        let text = self
            .llm_outputs
            .lock()
            .get(key)
            .cloned()
            .unwrap_or_else(|| output_for(&req));
        let (first, chunk) = *self.llm_pace.lock();
        FakeLlm::new(FakeConfig {
            text,
            chunk_chars: 3,
            first_token_delay: first,
            chunk_delay: chunk,
        })
        .stream(req, cancel)
    }
    fn ocr_engine_stream(&self, req: EngineRequest, cancel: CancellationToken) -> mpsc::Receiver<StreamEvent> {
        self.call("ocr_engine_stream");
        let pixels = matches!(req.image, EngineImage::Rgba(_));
        self.engine_calls.lock().push((req.engine.clone(), req.mode, pixels));
        FakeEngine::new(self.engine.lock().clone()).stream(cancel)
    }
    fn offline_status(&self, model: LocalModelId) -> OfflineStatus {
        self.offline
            .lock()
            .get(&model)
            .copied()
            .unwrap_or(OfflineStatus::Missing)
    }
    fn frontmost(&self) -> Option<AppRef> {
        Some(AppRef {
            id: "com.apple.TextEdit".to_owned(),
            alt_id: None,
            name: "TextEdit".to_owned(),
            pid: Some(42),
            path: None,
            window: None,
        })
    }
    fn read_selection(&self, opts: &SelectionOpts) -> Result<Selection, SelectionError> {
        assert!(opts.target.is_some(), "取词时带上触发时的前台应用");
        self.call("read_selection");
        let script = self.selections.lock().pop_front();
        self.selection_reads.fetch_add(1, Ordering::SeqCst);
        let result = match script {
            Some(s) => {
                std::thread::sleep(s.delay);
                s.result
            }
            None => Err(SelectionError::Empty),
        };
        self.call("read_selection_done");
        result
    }
    fn activate(&self, _: &AppRef) {
        self.call("activate");
    }
    /// 取代上一条的规则用剪贴板服务的那一份（`succession`）；写入、去重与删除（hash 核对、收藏保留）在假历史上模拟
    fn copy_text(&self, replaces: Option<CopyReceipt>, text: &str) -> Result<CopyReceipt, IpcError> {
        let replace = replaces.is_some_and(|p| self.clip_change.load(Ordering::SeqCst) == p.change_id);
        let (change_id, recorded) = self.fake_write(text)?;
        Ok(self.fake_succeed(replaces.and_then(|p| p.created), replace, change_id, recorded))
    }
    fn recopy_text(&self, prev: CopyReceipt, text: &str) -> Result<Option<CopyReceipt>, IpcError> {
        if self.clip_change.load(Ordering::SeqCst) != prev.change_id {
            return Ok(None);
        }
        let (change_id, recorded) = self.fake_write(text)?;
        Ok(Some(self.fake_succeed(prev.created, true, change_id, recorded)))
    }
    fn wayland(&self) -> bool {
        *self.wayland.lock()
    }
    fn reduce_motion(&self) -> bool {
        false
    }
    fn prewarm_selection(&self) {
        self.call("prewarm_selection");
    }
    fn frame_is_native(&self, _: &Frame) -> bool {
        *self.native_frames.lock()
    }
    fn panel_prepare(&self, req: PanelShowRequest) -> Option<u64> {
        self.call("prepare_enter");
        let delay = self
            .prepare_delays
            .lock()
            .pop_front()
            .unwrap_or(*self.prepare_delay.lock());
        std::thread::sleep(delay);
        self.call("panel_prepare");
        if *self.no_monitor.lock() {
            return None;
        }
        let session = self.next_session.fetch_add(1, Ordering::SeqCst) + 1;
        *self.panel_key.lock() = req.take_focus;
        let previous = self.live.lock().replace((session, req.view));
        if let Some((old, _)) = previous {
            // 新会话替换旧会话：旧会话结束并通知
            self.hidden_tx.send_replace(old);
        }
        self.prepares.lock().push((Instant::now(), req, session));
        // 模拟前端下一帧画出第一帧（panel_painted → Shown）
        let perf = Arc::clone(&self.perf);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            perf.mark_session(session, PerfStage::Shown);
        });
        Some(session)
    }
    fn panel_session_live(&self, session_id: u64) -> bool {
        self.live.lock().is_some_and(|(s, _)| s == session_id)
    }
    fn panel_state(&self) -> Option<PanelState> {
        self.live.lock().map(|(session_id, view)| PanelState {
            session_id,
            view,
            monitor_id: MONITOR.to_owned(),
            rect: Rect::new(10.0, 40.0, 360.0, 120.0),
            key: *self.panel_key.lock(),
        })
    }
    fn panel_prev_app(&self, _: u64) -> Option<AppRef> {
        self.frontmost()
    }
    fn panel_request_hide(&self, reason: HideReason) {
        self.call(&format!("request_hide:{reason:?}"));
        let live = self.live.lock().map(|(s, _)| s);
        if let Some(s) = live {
            self.hide(s);
        }
    }
    fn view_width(&self, _: PanelView, _: &PanelPayload) -> f64 {
        *self.view_width.lock()
    }
    fn panel_card_width(&self, session_id: u64) -> Option<f64> {
        if self.panel_session_live(session_id) {
            *self.card_width.lock()
        } else {
            None
        }
    }
    fn panel_hidden(&self) -> Option<watch::Receiver<u64>> {
        Some(self.hidden_tx.subscribe())
    }
    fn perf(&self) -> &PerfMarks {
        &self.perf
    }
    fn perf_finish(&self, trace: u64) {
        self.finished.lock().push(trace);
    }
    fn capture_prewarm(&self) {
        self.call("prewarm");
    }
    fn screen_permission(&self) -> PermissionState {
        *self.permission.lock()
    }
    fn request_screen_permission(&self) {
        self.call("request_screen_permission");
    }
    fn hide_for_capture(&self) -> HiddenSet {
        self.call("hide_for_capture");
        let live = self.live.lock().map(|(s, _)| s);
        if let Some(s) = live {
            self.hide(s);
        }
        HiddenSet {
            panel: live.is_some(),
            pinned: vec!["p-hidden".to_owned()],
            ..HiddenSet::default()
        }
    }
    fn restore_after_capture(&self, set: HiddenSet) {
        assert_eq!(set.pinned, vec!["p-hidden".to_owned()]);
        self.call("restore");
    }
    fn monitors(&self) -> Vec<MonitorInfo> {
        self.monitors.lock().clone()
    }
    fn capture_all(&self, monitors: &[MonitorInfo]) -> Result<CaptureSet, CaptureError> {
        self.call("capture_all");
        let delay = self.capture_delays.lock().pop_front().unwrap_or_default();
        std::thread::sleep(delay);
        if let Some(e) = self.capture_error.lock().clone() {
            return Err(e);
        }
        let frames = monitors
            .iter()
            .map(|m| Frame::from_rgba(m.clone(), 400, 200, vec![200; 400 * 200 * 4]))
            .collect();
        Ok(CaptureSet {
            frames,
            windows: vec![wisp_capture::WindowRect {
                monitor_id: MONITOR.to_owned(),
                rect: Rect::new(0.0, 0.0, 100.0, 50.0),
                app_name: "Editor".to_owned(),
            }],
        })
    }
    fn create_overlays(&self, session: &OverlaySession) -> Result<(), IpcError> {
        assert_eq!(session.monitors.len(), self.monitors.lock().len());
        self.call("create_overlays");
        Ok(())
    }
    fn overlay_show(&self, _: u64, monitor_id: &str, frozen: Option<&Frame>) -> Result<bool, IpcError> {
        if frozen.is_some() {
            let delay = *self.native_show_delay.lock();
            std::thread::sleep(delay);
        }
        self.call(&format!("overlay_show:{monitor_id}:{}", frozen.is_some()));
        if self.overlay_fail.lock().contains(monitor_id) {
            return Err(IpcError::internal("overlay window is gone"));
        }
        Ok(frozen.is_some())
    }
    fn overlay_monitor(&self, label: &str) -> Option<MonitorId> {
        let index: usize = label.strip_prefix("overlay-")?.parse().ok()?;
        self.monitors.lock().get(index).map(|m| m.id.clone())
    }
    fn overlay_session_started(&self, _: u64, monitor_id: &str) {
        self.sessions_started
            .lock()
            .push((monitor_id.to_owned(), Instant::now()));
        self.call("overlay_session_started");
    }
    fn destroy_overlays(&self, fade_out: bool) {
        let delay = *self.destroy_delay.lock();
        std::thread::sleep(delay);
        self.call("destroy_overlays");
        if fade_out {
            self.call("destroy_overlays:fade");
        }
    }
    fn onboarding_open(&self) -> bool {
        self.onboarding.lock().0
    }
    fn onboarding_focused(&self) -> bool {
        let delay = self.onboarding_delays.lock().pop_front().unwrap_or_default();
        self.onboarding_checks.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(delay);
        self.onboarding.lock().1
    }
    fn emit_onboarding_hotkey(&self, action: HotkeyAction) {
        self.onboarding_hotkeys.lock().push(action);
    }
    fn clipboard_payload(&self) -> ClipboardPayload {
        ClipboardPayload {
            page: ClipPage::empty(ClipFilter::All),
            paused: false,
            hint_bar: true,
            paste_on_double_click: false,
            unavailable: None,
            recording: wisp_types::ClipboardRecording::Recording,
            favorites_collapsed: false,
            queue_unavailable: None,
        }
    }
    fn clipboard_has_current(&self) -> bool {
        true
    }
    fn overlay_escape_guard(&self, active: bool) {
        self.call(if active { "esc_guard_on" } else { "esc_guard_off" });
        *self.esc_guard.lock() = active;
    }
    fn cursor_monitor(&self) -> Option<MonitorId> {
        self.cursor.lock().clone()
    }
    fn overlay_focus(&self, _session_id: u64, monitor_id: &str) {
        self.focused_overlays.lock().push(monitor_id.to_owned());
    }
    fn overlay_follows_pointer(&self) -> bool {
        *self.follows_pointer.lock()
    }
    fn overlay_committed(&self, session_id: u64) {
        self.committed.lock().push(session_id);
    }
    fn overlay_session_ended(&self, session_id: u64, reason: OverlayEndReason) {
        self.overlay_ends.lock().push((session_id, reason));
        self.call("overlay_end");
    }
    fn toggle_clipboard_pause(&self) {
        self.call("toggle_clipboard_pause");
    }
    fn clear_clipboard(&self, keep_pinned: bool) {
        self.call(&format!("clear_clipboard:{keep_pinned}"));
    }
    fn check_update(&self) {
        self.call("check_update");
    }
    fn open_window(&self, target: WindowTarget) {
        self.call(&format!("open_window:{target:?}"));
    }
    fn quit(&self) {
        self.call("quit");
    }
    fn record_ocr_history(&self, rec: crate::clipboard_svc::ocr_history::OcrRecord) -> Option<i64> {
        let delay = self.ocr_history_delay.lock().pop_front();
        if let Some(delay) = delay {
            std::thread::sleep(delay);
        }
        let mut h = self.ocr_history.lock();
        h.push(rec);
        i64::try_from(h.len()).ok()
    }
}

impl FakeEnv {
    /// 写假剪贴板并记进假历史：内容已在历史里时只是移到顶部
    fn fake_write(&self, text: &str) -> Result<(u64, Recorded), IpcError> {
        if *self.copy_fails.lock() {
            return Err(IpcError::internal("clipboard busy"));
        }
        self.copies.lock().push(text.to_owned());
        let change_id = self.clip_change.fetch_add(1, Ordering::SeqCst) + 1;
        let mut live = self.clip_live.lock();
        let recorded = match live.iter().find(|(_, t)| t == text) {
            Some(&(id, _)) => Recorded::Existing {
                id,
                hash: Some(fake_item(id).hash),
            },
            None => {
                let id = self.clip_items.fetch_add(1, Ordering::SeqCst) + 1;
                live.push((id, text.to_owned()));
                Recorded::Created(fake_item(id))
            }
        };
        Ok((change_id, recorded))
    }

    /// 按剪贴板服务的规则交接，删掉被取代、没有收藏的那一条
    fn fake_succeed(&self, prev: Option<CopiedItem>, replace: bool, change_id: u64, recorded: Recorded) -> CopyReceipt {
        let next = succession(prev, replace, Some(recorded));
        if let Some(old) = next.discard
            && !self.clip_pinned.lock().contains(&old.id)
        {
            let mut live = self.clip_live.lock();
            if let Some(i) = live.iter().position(|(id, _)| *id == old.id) {
                live.remove(i);
                self.clip_removed.lock().push(old.id);
            }
        }
        CopyReceipt {
            change_id,
            created: next.created,
        }
    }

    /// 假历史里还在的条目 id（按写入顺序）
    fn clip_ids(&self) -> Vec<i64> {
        self.clip_live.lock().iter().map(|(id, _)| *id).collect()
    }
}

/// 假剪贴板历史里第 `id` 条（内容 hash 只需各条不同）
fn fake_item(id: i64) -> CopiedItem {
    let mut hash = [0; 32];
    hash[..8].copy_from_slice(&id.to_le_bytes());
    CopiedItem { id, hash }
}

fn setup(settings: Settings) -> (Arc<FakeEnv>, Arc<Core>) {
    let env = FakeEnv::new(settings);
    let core = Core::new(Arc::clone(&env) as Arc<dyn Env>);
    let (_tx, rx) = mpsc::unbounded_channel();
    core.spawn(rx);
    (env, core)
}

/// 等条件成立（每 5 ms 检查一次），超时 panic
async fn until(what: &str, limit: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn translate_payload(req: &PanelShowRequest) -> wisp_types::TranslatePayload {
    match &req.payload {
        PanelPayload::Translate(t) => t.clone(),
        other => panic!("expected a translate payload, got {other:?}"),
    }
}

fn ocr_payload(req: &PanelShowRequest) -> wisp_types::OcrPayload {
    match &req.payload {
        PanelPayload::Ocr(p) => p.clone(),
        other => panic!("expected an OCR payload, got {other:?}"),
    }
}

fn text_req(session_id: u64, text: &str, style: &str) -> TranslateReq {
    TranslateReq {
        session_id,
        text: text.to_owned(),
        task: TaskKind::Translate,
        style_id: style.to_owned(),
        target_lang: None,
        model_ref: None,
        dict_mode: DictMode::Auto,
        bypass_cache: false,
        draft: None,
        confirm_long: false,
    }
}

fn pinned(id: &str) -> RequestHost {
    RequestHost::Pinned {
        pinned_id: id.to_owned(),
    }
}

fn is_cancelled(ev: &Option<StreamEvent>) -> bool {
    matches!(ev, Some(StreamEvent::Error(e)) if e.code == ErrorCode::Cancelled)
}

fn phases(sink: &Collector) -> Vec<StreamPhase> {
    sink.take()
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Phase { phase } => Some(*phase),
            _ => None,
        })
        .collect()
}

fn translate_text(core: &Arc<Core>, text: &str) {
    core.dispatch(Command::TranslateText {
        text: text.to_owned(),
        origin: TriggerOrigin::Cli,
    });
}

fn hotkey(core: &Arc<Core>, action: HotkeyAction) {
    core.dispatch(Command::Action {
        action,
        origin: TriggerOrigin::Hotkey,
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn translate_text_requests_before_the_panel_and_streams_post_processed_text() {
    let (env, core) = setup(configured());
    *env.prepare_delay.lock() = Duration::from_millis(150);
    translate_text(&core, "The quick brown fox jumps over the lazy dog.");
    until("panel prepared", WAIT, || env.prepare_count() == 1).await;
    let (prepared_at, req, session) = env.prepares.lock()[0].clone();
    // 请求在面板显示之前就已发出（LLM 请求与窗口显示并行）
    let (_, purpose, sent_at) = env.llm_calls.lock().first().cloned().expect("request sent");
    assert_eq!(purpose, Purpose::Translate);
    assert!(sent_at < prepared_at, "LLM 请求应早于 panel_prepare 返回");
    let payload = translate_payload(&req);
    assert_eq!(req.view, PanelView::Translate);
    assert!(!req.take_focus, "翻译视图不抢焦点");
    assert_eq!(payload.detected_lang, Some(wisp_types::Lang::En));
    assert_eq!(payload.target_lang, wisp_types::Lang::ZhCn);
    let id = payload.request_id.expect("request id in payload");
    // 前端 attach：先回放再接上实时事件
    let sink = Collector::new();
    core.stream_attach(&RequestCaller::Panel, &id, sink.clone())
        .expect("attach");
    until("stream done", WAIT, || sink.terminal().is_some()).await;
    let events = sink.take();
    assert!(
        matches!(&events[0], StreamEvent::Start { request_id, model_id, .. } if *request_id == id && model_id == "m")
    );
    assert!(matches!(
        events[1],
        StreamEvent::Meta {
            target_lang: Some(wisp_types::Lang::ZhCn),
            ..
        }
    ));
    assert!(matches!(sink.terminal(), Some(StreamEvent::Done { .. })));
    assert!(sink.text().starts_with("敏捷的棕色狐狸"));
    assert_eq!(env.usage.lock().len(), 1, "Done.usage 记入用量统计");
    // 埋点：Prepare、FirstToken、Shown 都已记录，显示之后结束
    until("trace finished", WAIT, || !env.finished.lock().is_empty()).await;
    let trace = env.perf.first_after(0).expect("trace");
    assert_eq!(trace.session_id, Some(session));
    assert!(trace.prepare_ms.is_some() && trace.first_token_ms.is_some() && trace.shown_ms.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retrigger_during_selection_keeps_only_the_latest() {
    let (env, core) = setup(configured());
    env.selections.lock().extend([
        SelScript {
            delay: Duration::from_millis(250),
            result: selection("the first selection"),
        },
        SelScript {
            delay: Duration::ZERO,
            result: selection("the second selection"),
        },
    ]);
    hotkey(&core, HotkeyAction::TranslateSelection);
    until("first read started", WAIT, || {
        env.selection_reads.load(Ordering::SeqCst) == 1
    })
    .await;
    hotkey(&core, HotkeyAction::TranslateSelection);
    until("panel prepared", WAIT, || env.prepare_count() == 1).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    // 连按两次只产生一个有效请求
    assert_eq!(env.prepare_count(), 1);
    assert_eq!(env.llm_count(), 1);
    let (req, _) = env.last_prepare().expect("prepare");
    let payload = translate_payload(&req);
    assert_eq!(payload.source_text, "the second selection");
    assert_eq!(payload.selection_method.as_deref(), Some("ax"));
    assert_eq!(core.run_count(), 1, "被取代的 run 已收尾");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn style_switches_cancel_the_previous_request_and_cache_hits_skip_the_network() {
    let (env, core) = setup(configured());
    let text = "Please check the attached report before Friday.";
    translate_text(&core, text);
    until("panel", WAIT, || env.prepare_count() == 1).await;
    let (_, session) = env.last_prepare().expect("prepare");
    let host = || RequestHost::Panel { session_id: session };
    // 第一次（natural）在 Rust 发起时已经完成，进了缓存
    until("first done", WAIT, || core.cache_len() == 1).await;
    // 连切 3 次风格：只有最后一个在流式
    *env.llm_pace.lock() = (Duration::from_millis(80), Duration::from_millis(5));
    let sinks: Vec<Arc<Collector>> = (0..3).map(|_| Collector::new()).collect();
    let mut ids = Vec::new();
    for (sink, style) in sinks.iter().zip(["technical", "academic", "casual"]) {
        let ack = core
            .translate_start(text_req(session, text, style), host(), sink.clone())
            .await
            .expect("start");
        ids.push(ack.request_id);
    }
    until("last done", WAIT, || {
        matches!(sinks[2].terminal(), Some(StreamEvent::Done { .. }))
    })
    .await;
    assert!(is_cancelled(&sinks[0].terminal()));
    assert!(is_cancelled(&sinks[1].terminal()));
    assert!(sinks[0].text().is_empty() && sinks[1].text().is_empty());
    assert!(!sinks[2].text().is_empty());
    assert_eq!(core.outcome(&ids[0]), None, "被取代的请求已移除");
    // 切回 natural：缓存命中，不发请求（切回时直接显示）
    let calls = env.llm_count();
    let back = Collector::new();
    let t0 = Instant::now();
    core.translate_start(text_req(session, text, "natural"), host(), back.clone())
        .await
        .expect("start");
    until("cached", WAIT, || back.terminal().is_some()).await;
    assert!(t0.elapsed() < Duration::from_millis(100));
    assert_eq!(env.llm_count(), calls, "命中缓存时没有新请求");
    let events = back.take();
    assert!(events.iter().any(|e| matches!(
        e,
        StreamEvent::Notice {
            kind: NoticeKind::CacheHit
        }
    )));
    assert!(matches!(
        back.terminal(),
        Some(StreamEvent::Done {
            cached: true,
            usage: None,
            ..
        })
    ));
    assert!(back.text().starts_with("敏捷的棕色狐狸"));
    // ⌘R：绕过缓存
    let again = Collector::new();
    let mut req = text_req(session, text, "natural");
    req.bypass_cache = true;
    core.translate_start(req, host(), again.clone()).await.expect("start");
    until("regenerated", WAIT, || again.terminal().is_some()).await;
    assert_eq!(env.llm_count(), calls + 1, "⌘R 发出新请求");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_is_final_and_nothing_follows() {
    let (env, core) = setup(configured());
    *env.llm_pace.lock() = (Duration::from_millis(5), Duration::from_millis(15));
    translate_text(&core, "hello there, how are you doing today?");
    until("panel", WAIT, || env.prepare_count() == 1).await;
    let (_, session) = env.last_prepare().expect("prepare");
    let sink = Collector::new();
    let ack = core
        .translate_start(
            text_req(session, "Another sentence to translate, quite long.", "natural"),
            RequestHost::Panel { session_id: session },
            sink.clone(),
        )
        .await
        .expect("start");
    until("first delta", WAIT, || !sink.text().is_empty()).await;
    let t0 = Instant::now();
    core.cancel(&RequestCaller::Panel, &ack.request_id);
    assert!(is_cancelled(&sink.terminal()));
    assert!(t0.elapsed() < Duration::from_millis(50), "50 ms 内收到 cancelled");
    let n = sink.take().len();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(sink.take().len(), n, "取消后不再有任何事件");
    assert_eq!(core.outcome(&ack.request_id), Some(Some(Outcome::Cancelled)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_end_cancels_but_pinned_requests_survive_until_the_window_closes() {
    let (env, core) = setup(configured());
    *env.llm_pace.lock() = (Duration::from_millis(400), Duration::from_millis(5));
    translate_text(&core, "Ownership moves to the pinned window.");
    until("panel", WAIT, || env.prepare_count() == 1).await;
    let (req, session) = env.last_prepare().expect("prepare");
    let id = translate_payload(&req).request_id.expect("id");
    // 面板里改做润色
    let stay = Collector::new();
    let mut polish = text_req(session, "Polish this sentence please.", "natural");
    polish.task = TaskKind::Polish;
    let ack = core
        .translate_start(polish, RequestHost::Panel { session_id: session }, stay.clone())
        .await
        .expect("start");
    // 润色与翻译同在文本通道：Rust 发起的翻译被取代（同一面板同一时刻只有一个活跃请求）
    assert_eq!(core.outcome(&id), None);
    // 固定：所有权转给 pinned，之后面板会话结束（固定交接隐藏面板）
    core.transfer_to_pinned(&ack.request_id, "p1");
    env.hide(session);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(core.outcome(&ack.request_id), Some(None), "转给 pinned 的流照常进行");
    let attached = Collector::new();
    // 面板窗口（例如 WebView 重载后按旧载荷重新 attach）抢不走 pinned 的实时通道（not_found）；面板卸载时的
    // cancel 静默忽略（cancel 返回 ()，前端不会收到 rejection），pinned 的流照常进行
    assert!(
        core.stream_attach(&RequestCaller::Panel, &ack.request_id, Collector::new())
            .is_err()
    );
    core.cancel(&RequestCaller::Panel, &ack.request_id);
    assert_eq!(core.outcome(&ack.request_id), Some(None), "面板取消不了 pinned 的流");
    let p1 = RequestCaller::Pinned {
        pinned_id: "p1".to_owned(),
    };
    core.stream_attach(&p1, &ack.request_id, attached.clone())
        .expect("pinned attach");
    // pinned 关闭：取消归属它的流
    core.pinned_closed("p1");
    assert!(is_cancelled(&attached.terminal()));
    assert_eq!(core.outcome(&ack.request_id), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panel_requests_end_with_their_session() {
    let (env, core) = setup(configured());
    *env.llm_pace.lock() = (Duration::from_millis(400), Duration::from_millis(5));
    translate_text(&core, "This stream belongs to the panel session.");
    until("panel", WAIT, || env.prepare_count() == 1).await;
    let (req, session) = env.last_prepare().expect("prepare");
    let id = translate_payload(&req).request_id.expect("id");
    let sink = Collector::new();
    core.stream_attach(&RequestCaller::Panel, &id, sink.clone())
        .expect("attach");
    env.hide(session);
    until("cancelled with the session", WAIT, || is_cancelled(&sink.terminal())).await;
    assert_eq!(core.outcome(&id), None);
    assert_eq!(core.run_count(), 0, "run 随会话收尾");
    // 会话已结束之后才到达的 translate_start（34.1：登记之后立即核对）
    let late = Collector::new();
    let ack = core
        .translate_start(
            text_req(session, "late request", "natural"),
            RequestHost::Panel { session_id: session },
            late.clone(),
        )
        .await
        .expect("start");
    assert!(is_cancelled(&late.terminal()));
    assert_eq!(core.outcome(&ack.request_id), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retrigger_while_streaming_replaces_the_panel_and_cancels_the_old_stream() {
    let (env, core) = setup(configured());
    *env.llm_pace.lock() = (Duration::from_millis(20), Duration::from_millis(20));
    translate_text(&core, "The first text that keeps streaming for a while.");
    until("panel", WAIT, || env.prepare_count() == 1).await;
    let (req, _) = env.last_prepare().expect("prepare");
    let first = translate_payload(&req).request_id.expect("id");
    let sink = Collector::new();
    core.stream_attach(&RequestCaller::Panel, &first, sink.clone())
        .expect("attach");
    until("streaming", WAIT, || !sink.text().is_empty()).await;
    // 流式中再次触发，旧请求取消，只显示新内容
    translate_text(&core, "The second text.");
    until("second panel", WAIT, || env.prepare_count() == 2).await;
    until("old cancelled", WAIT, || is_cancelled(&sink.terminal())).await;
    let n = sink.take().len();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(sink.take().len(), n, "迟到的旧 Delta 不会出现");
    let (req, _) = env.last_prepare().expect("prepare");
    let second = translate_payload(&req).request_id.expect("id");
    assert_eq!(core.outcome(&second), Some(None));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn long_text_and_not_ready_show_cards_without_requests() {
    let (env, core) = setup(configured());
    translate_text(&core, &"字".repeat(3001));
    until("panel", WAIT, || env.prepare_count() == 1).await;
    let (req, _) = env.last_prepare().expect("prepare");
    let p = translate_payload(&req);
    assert!(p.request_id.is_none());
    assert_eq!(p.long_text.as_ref().map(|l| l.chars), Some(3001));
    assert_eq!(env.llm_count(), 0, "长文本先不发请求");
    // 确认后由前端发起（confirm_long = true，不再判长文本）：长文本确认 Confirming → Requesting，run 随这个请求推进
    let (_, session) = env.last_prepare().expect("prepare");
    assert!(matches!(core.run_of_session(session), Some((_, _, Stage::Confirming))));
    let sink = Collector::new();
    core.translate_start(
        TranslateReq {
            confirm_long: true,
            ..text_req(session, &"字".repeat(3001), "natural")
        },
        RequestHost::Panel { session_id: session },
        sink.clone(),
    )
    .await
    .expect("start");
    until("requested", WAIT, || env.llm_count() == 1).await;
    until("confirmed request done", WAIT, || sink.terminal().is_some()).await;
    until("run done", WAIT, || {
        matches!(core.run_of_session(session), Some((_, _, Stage::Done)))
    })
    .await;

    // 没有 Provider：不取词，直接显示「还没有接入模型」
    let (env, core) = setup(Settings::default());
    env.selections.lock().push_back(SelScript {
        delay: Duration::ZERO,
        result: selection("anything"),
    });
    hotkey(&core, HotkeyAction::TranslateSelection);
    until("panel", WAIT, || env.prepare_count() == 1).await;
    let (req, _) = env.last_prepare().expect("prepare");
    assert_eq!(translate_payload(&req).not_ready, Some(NotReady::NoProvider));
    assert_eq!(env.selection_reads.load(Ordering::SeqCst), 0);
    assert_eq!(env.llm_count(), 0);
    // 前端发起的请求遇到未就绪：Start → Error{not_configured}
    let sink = Collector::new();
    core.translate_start(text_req(0, "hello", "natural"), pinned("p"), sink.clone())
        .await
        .expect("start");
    assert!(matches!(sink.terminal(), Some(StreamEvent::Error(e)) if e.code == ErrorCode::NotConfigured));
    assert!(matches!(sink.take().first(), Some(StreamEvent::Start { .. })));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selection_failures_open_the_input_view_with_a_hint() {
    let (env, core) = setup(configured());
    env.selections.lock().push_back(SelScript {
        delay: Duration::ZERO,
        result: Err(SelectionError::PermissionMissing(PermissionKind::Accessibility)),
    });
    hotkey(&core, HotkeyAction::TranslateSelection);
    until("panel", WAIT, || env.prepare_count() == 1).await;
    let (req, _) = env.last_prepare().expect("prepare");
    assert_eq!(req.view, PanelView::Input);
    assert!(req.take_focus);
    match req.payload {
        PanelPayload::Input(p) => assert_eq!(
            p.hint,
            Some(NotReady::PermissionMissing {
                permission: PermissionKind::Accessibility
            })
        ),
        other => panic!("{other:?}"),
    }
    // 空选区：输入视图，不带提示
    env.selections.lock().push_back(SelScript {
        delay: Duration::ZERO,
        result: selection("   \n "),
    });
    hotkey(&core, HotkeyAction::TranslateSelection);
    until("second panel", WAIT, || env.prepare_count() == 2).await;
    let (req, _) = env.last_prepare().expect("prepare");
    assert!(matches!(req.payload, PanelPayload::Input(ref p) if p.hint.is_none()));
    // 输入视图显示时再按专用快捷键：关闭
    hotkey(&core, HotkeyAction::TranslateInput);
    assert!(env.called("request_hide:Toggle"));
    assert_eq!(env.llm_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_submit_reports_the_new_width_only_when_it_changes() {
    let (env, core) = setup(configured());
    hotkey(&core, HotkeyAction::TranslateInput);
    until("input view", WAIT, || env.prepare_count() == 1).await;
    let (req, session) = env.last_prepare().expect("prepare");
    assert_eq!(req.view, PanelView::Input);
    let host = RequestHost::Panel { session_id: session };
    *env.card_width.lock() = Some(440.0);
    *env.view_width.lock() = 380.0;
    let ack = core
        .translate_start(
            text_req(session, "typed text", "natural"),
            host.clone(),
            Collector::new(),
        )
        .await
        .expect("start");
    assert_eq!(ack.width, Some(380.0));
    *env.card_width.lock() = Some(380.2);
    let ack = core
        .translate_start(
            text_req(session, "typed text again", "technical"),
            host,
            Collector::new(),
        )
        .await
        .expect("start");
    assert_eq!(ack.width, None, "相差不到 0.5 px 时为 null");
    let ack = core
        .translate_start(text_req(0, "pinned", "natural"), pinned("p"), Collector::new())
        .await
        .expect("start");
    assert_eq!(ack.width, None, "pinned 宿主恒为 null");
    assert!(
        core.translate_start(text_req(0, "  ", "natural"), pinned("p"), Collector::new())
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refine_reviews_first_and_skips_improving_when_ok() {
    let (env, core) = setup(configured());
    let mut req = text_req(
        0,
        "A fairly long source paragraph that deserves a careful review.",
        "natural",
    );
    req.task = TaskKind::Refine;
    req.draft = Some("一段相当长的原文。".to_owned());
    env.llm_outputs.lock().insert("reflect", " OK \n".to_owned());
    let sink = Collector::new();
    core.translate_start(req.clone(), pinned("p"), sink.clone())
        .await
        .expect("start");
    until("done", WAIT, || sink.terminal().is_some()).await;
    assert_eq!(phases(&sink), vec![StreamPhase::Reviewing]);
    assert!(sink.text().is_empty(), "OK 时没有 Delta");
    assert!(matches!(sink.terminal(), Some(StreamEvent::Done { .. })));
    assert_eq!(env.llm_count(), 1, "OK 时只有 1 次请求");

    env.llm_outputs
        .lock()
        .insert("reflect", "\"相当长\" → fix: 更自然的说法".to_owned());
    let sink = Collector::new();
    core.translate_start(req, pinned("p"), sink.clone())
        .await
        .expect("start");
    until("done", WAIT, || sink.terminal().is_some()).await;
    assert_eq!(phases(&sink), vec![StreamPhase::Reviewing, StreamPhase::Improving]);
    assert!(!sink.text().is_empty());
    assert_eq!(env.llm_count(), 3);
    assert_eq!(core.cache_len(), 0, "refine 不进缓存");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keychain_failures_become_retryable_errors() {
    let (env, core) = setup(configured());
    *env.key_error.lock() = Some(IpcError::keychain("user denied keychain access"));
    let sink = Collector::new();
    core.translate_start(text_req(0, "hello world", "natural"), pinned("p"), sink.clone())
        .await
        .expect("start");
    until("error", WAIT, || sink.terminal().is_some()).await;
    assert!(matches!(sink.terminal(), Some(StreamEvent::Error(e)) if e.code == ErrorCode::Keychain && e.retryable));
    assert_eq!(env.llm_count(), 0);
    // 钥匙串不可用且没有 Key：发请求之前就判定为未就绪
    *env.key_error.lock() = None;
    *env.keychain.lock() = (KeychainState::Unavailable, false);
    translate_text(&core, "hello");
    until("panel", WAIT, || env.prepare_count() == 1).await;
    let (req, _) = env.last_prepare().expect("prepare");
    assert_eq!(translate_payload(&req).not_ready, Some(NotReady::KeychainUnavailable));
}

/// 不需要 Key 的 Provider（回环地址的本地模型）：钥匙串不可用、或读取报错时请求照常发出，不访问 / 不受钥匙串影响
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_providers_never_fail_on_the_keychain() {
    let mut settings = configured();
    settings.providers[0].base_url = "http://127.0.0.1:11434/v1".to_owned();
    let (env, core) = setup(settings);
    // ① 钥匙串不可用、没有存 Key：就绪，请求不读钥匙串
    *env.keychain.lock() = (KeychainState::Unavailable, false);
    *env.key_error.lock() = Some(IpcError::keychain("A default keychain could not be found"));
    let sink = Collector::new();
    core.translate_start(text_req(0, "hello world", "natural"), pinned("p"), sink.clone())
        .await
        .expect("start");
    until("done", WAIT, || sink.terminal().is_some()).await;
    assert!(
        matches!(sink.terminal(), Some(StreamEvent::Done { .. })),
        "{:?}",
        sink.terminal()
    );
    assert_eq!(env.llm_count(), 1);
    assert_eq!(env.count("key_get"), 0, "不需要 Key 也没存 Key：不访问钥匙串");
    // ② 存过 Key、但读取失败（锁定 / 没有默认钥匙串）：不带 Key 照常请求，而不是 Error{keychain}
    *env.keychain.lock() = (KeychainState::Available, true);
    let sink = Collector::new();
    core.translate_start(text_req(1, "good morning", "natural"), pinned("p"), sink.clone())
        .await
        .expect("start");
    until("done", WAIT, || sink.terminal().is_some()).await;
    assert!(
        matches!(sink.terminal(), Some(StreamEvent::Done { .. })),
        "{:?}",
        sink.terminal()
    );
    assert_eq!(env.llm_count(), 2);
    assert_eq!(env.count("key_get"), 1);
}

/// OCR 同样不为不需要 Key 的 Provider 读钥匙串（不需要 Key 时不预读）——回环地址、
/// 没有模板、也没存 Key 时，截图识别全程 `key_get` 一次都不调用
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_providers_never_touch_the_keychain_for_ocr() {
    let mut settings = configured();
    settings.providers[0].base_url = "http://127.0.0.1:11434/v1".to_owned();
    assert!(settings.providers[0].template_id.is_none());
    let (env, core) = setup(settings);
    *env.keychain.lock() = (KeychainState::Unavailable, false);
    *env.key_error.lock() = Some(IpcError::keychain("A default keychain could not be found"));
    let overlay = overlay_up(&env, &core, HotkeyAction::Ocr).await;
    core.overlay_commit(commit_on(overlay, MONITOR)).await.expect("commit");
    core.overlay_done(overlay);
    let (req, _) = env.last_prepare().expect("prepare");
    let id = ocr_payload(&req).request_id.clone().expect("request");
    let sink = Collector::new();
    core.stream_attach(&RequestCaller::Panel, &id, sink.clone())
        .expect("attach");
    until("ocr done", WAIT, || sink.terminal().is_some()).await;
    assert!(
        matches!(sink.terminal(), Some(StreamEvent::Done { .. })),
        "{:?}",
        sink.terminal()
    );
    assert_eq!(env.llm_count(), 1);
    assert_eq!(env.count("key_get"), 0, "{:?}", env.calls());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ocr_capture_commit_and_done() {
    let (env, core) = setup(configured());
    // 面板正显示着一次翻译：截图前隐藏它
    translate_text(&core, "visible translation");
    until("panel", WAIT, || env.prepare_count() == 1).await;
    env.calls.lock().clear();
    hotkey(&core, HotkeyAction::Ocr);
    assert_eq!(
        env.calls().first().map(String::as_str),
        Some("prewarm"),
        "第一件事是预热"
    );
    until("overlays", WAIT, || env.called("create_overlays")).await;
    // 再按截图快捷键：忽略
    hotkey(&core, HotkeyAction::Ocr);
    let calls = env.calls();
    let pos = |name: &str| calls.iter().position(|c| c == name).expect(name);
    assert!(pos("hide_for_capture") < pos("capture_all") && pos("capture_all") < pos("create_overlays"));
    assert_eq!(calls.iter().filter(|c| *c == "capture_all").count(), 1);
    // Overlay 页面 init：不带原生冻结帧 → 直接应答、给 1/2 帧
    let init = core.overlay_init("overlay-0").expect("init").expect("session");
    assert!(!init.frozen_native);
    assert!(init.half_url.is_some());
    assert_eq!(init.windows.len(), 1);
    assert_eq!(core.overlay_init("overlay-9").expect("init"), None);
    // 冻结帧经 wisp:// 提供
    let full = init
        .image_url
        .strip_prefix("capture/")
        .expect("capture path")
        .to_owned();
    assert!(FrameSource(Arc::clone(&core.captures)).fetch(&full).is_some());
    core.overlay_ready(init.session_id, MONITOR);
    until("overlay shown", WAIT, || {
        env.called(&format!("overlay_show:{MONITOR}:false"))
    })
    .await;
    // commit：面板在 Overlay 下面准备，请求在后台发出，返回飞入目标
    let ack = core
        .overlay_commit(OverlayCommit {
            session_id: init.session_id,
            monitor_id: MONITOR.to_owned(),
            rect: Rect::new(10.0, 10.0, 50.0, 20.0),
            kind: OverlaySelectionKind::Drag,
        })
        .await
        .expect("commit");
    assert_eq!(ack.fly_to, Some(Rect::new(10.0, 40.0, 360.0, 120.0)));
    assert_eq!(env.prepare_count(), 2);
    let (req, session) = env.last_prepare().expect("prepare");
    assert_eq!(req.view, PanelView::Ocr);
    let payload = ocr_payload(&req);
    assert_eq!(payload.mode, OcrMode::Lines);
    assert_eq!(payload.image_size, PhysSize { width: 100, height: 40 });
    assert_eq!(payload.thumb_url, format!("crop/{}.png", payload.capture_id));
    let id = payload.request_id.clone().expect("request");
    let sink = Collector::new();
    core.stream_attach(&RequestCaller::Panel, &id, sink.clone())
        .expect("attach");
    until("ocr done", WAIT, || sink.terminal().is_some()).await;
    assert!(matches!(
        sink.terminal(),
        Some(StreamEvent::Done { no_text: false, .. })
    ));
    assert!(sink.text().contains("Wisp"));
    // 自动复制
    until("auto copy", WAIT, || !env.copies.lock().is_empty()).await;
    assert_eq!(env.copies.lock()[0], sink.text());
    // 缩略图
    let thumb = format!("{}.png", payload.capture_id);
    assert!(CropSource(Arc::clone(&core.captures)).fetch(&thumb).is_some());
    // overlay_done：确认面板已显示，下一帧销毁 Overlay，恢复 pinned
    core.overlay_done(init.session_id);
    until("overlay torn down", WAIT, || {
        env.called("destroy_overlays") && env.called("restore")
    })
    .await;
    assert!(!core.overlay_active());
    assert!(!core.captures.has_frames(), "整屏数据已释放");
    // commit 之后销毁 Overlay，再把焦点交还触发前的应用（OCR 面板非激活显示，不受影响），最后恢复 pinned
    until("focus returned", WAIT, || env.called("activate")).await;
    let calls = env.calls();
    let pos = |name: &str| calls.iter().rposition(|c| c == name).expect(name);
    assert!(pos("destroy_overlays") < pos("activate") && pos("activate") < pos("restore"));
    // 正常收尾：macOS 先淡出 Overlay 窗口再销毁（原生冻结帧不硬切）
    assert!(env.called("destroy_overlays:fade"));
    // 重新识别：用内存中的同一张截图，宿主必须是这个面板会话
    let again = Collector::new();
    core.ocr_start(
        OcrReq {
            session_id: session,
            capture_id: payload.capture_id.clone(),
            mode: OcrMode::Markdown,
            model_ref: Some(model_ref("p", "vision")),
            target_lang: None,
            layout: None,
        },
        RequestHost::Panel { session_id: session },
        again.clone(),
    )
    .await
    .expect("ocr_start");
    until("re-recognised", WAIT, || again.terminal().is_some()).await;
    assert!(matches!(again.terminal(), Some(StreamEvent::Done { .. })));
    let other_host = core
        .ocr_start(
            OcrReq {
                session_id: 0,
                capture_id: payload.capture_id.clone(),
                mode: OcrMode::Lines,
                model_ref: None,
                target_lang: None,
                layout: None,
            },
            pinned("other"),
            Collector::new(),
        )
        .await;
    assert!(other_host.is_err(), "截图只属于它的宿主");
    // 面板关闭：截图释放
    env.hide(session);
    until("capture released", WAIT, || core.captures.crop_count() == 0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overlay_cancel_returns_focus_and_restores_windows() {
    let (env, core) = setup(configured());
    core.dispatch(Command::Action {
        action: HotkeyAction::Ocr,
        origin: TriggerOrigin::Tray,
    });
    until("overlays", WAIT, || env.called("create_overlays")).await;
    let init = core.overlay_init("overlay-0").expect("init").expect("session");
    core.overlay_cancel(init.session_id);
    until("torn down", WAIT, || env.called("restore")).await;
    let calls = env.calls();
    let pos = |name: &str| calls.iter().position(|c| c == name).expect(name);
    assert!(pos("activate") < pos("destroy_overlays") && pos("destroy_overlays") < pos("restore"));
    assert!(!core.overlay_active());
    assert!(!core.captures.has_frames());
    assert_eq!(core.run_count(), 0);
    assert_eq!(env.prepare_count(), 0, "取消不显示面板");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capture_permission_and_failures_show_cards() {
    let (env, core) = setup(configured());
    *env.permission.lock() = PermissionState::Denied;
    hotkey(&core, HotkeyAction::Ocr);
    until("panel", WAIT, || env.prepare_count() == 1).await;
    let (req, _) = env.last_prepare().expect("prepare");
    assert_eq!(
        ocr_payload(&req).not_ready,
        Some(NotReady::PermissionMissing {
            permission: PermissionKind::ScreenRecording
        })
    );
    until("requested", WAIT, || env.called("request_screen_permission")).await;
    assert!(!env.called("capture_all"), "未授权时不进入 Overlay");

    // 截图后端失败：恢复窗口，卡片只带 i18n key
    *env.permission.lock() = PermissionState::Granted;
    *env.capture_error.lock() = Some(CaptureError::Backend("X11 capture timed out".into()));
    hotkey(&core, HotkeyAction::Ocr);
    until("second panel", WAIT, || env.prepare_count() == 2).await;
    let (req, _) = env.last_prepare().expect("prepare");
    assert_eq!(
        ocr_payload(&req).not_ready,
        Some(NotReady::CaptureFailed {
            message: "errors.capture.backend".into()
        })
    );
    assert!(env.called("restore"));
    // 用户取消（门户）：静默结束
    *env.capture_error.lock() = Some(CaptureError::Cancelled);
    hotkey(&core, HotkeyAction::Ocr);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(env.prepare_count(), 2);
    // 没有视觉模型：不截图，直接显示卡片
    let mut s = configured();
    s.defaults.ocr = None;
    let (env, core) = setup(s);
    hotkey(&core, HotkeyAction::OcrTranslate);
    until("panel", WAIT, || env.prepare_count() == 1).await;
    let (req, _) = env.last_prepare().expect("prepare");
    assert_eq!(ocr_payload(&req).not_ready, Some(NotReady::NoVisionModel));
    assert_eq!(ocr_payload(&req).mode, OcrMode::Translate);
    assert!(!env.called("capture_all"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clipboard_images_are_recognised_like_a_capture() {
    let (env, core) = setup(configured());
    // 剪贴板图片「识别文字」：没有截图与框选，同一段启动流程，面板交叉淡入
    core.dispatch(Command::OcrImage {
        image: gray_image(120, 30),
        origin: TriggerOrigin::Clipboard,
    });
    until("panel", WAIT, || env.prepare_count() == 1).await;
    let (req, _) = env.last_prepare().expect("prepare");
    assert_eq!(req.view, PanelView::Ocr);
    assert_eq!(req.transition, PanelTransition::Crossfade);
    let payload = ocr_payload(&req);
    assert_eq!(payload.not_ready, None);
    assert_eq!(payload.mode, OcrMode::Lines);
    assert_eq!(payload.image_size, PhysSize { width: 120, height: 30 });
    assert!(!env.called("capture_all"), "现成的图片不截图");
    // 请求与截图 OCR 相同：可以接上流、结果自动复制，缩略图可取
    let id = payload.request_id.clone().expect("request");
    let sink = Collector::new();
    core.stream_attach(&RequestCaller::Panel, &id, sink.clone())
        .expect("attach");
    until("ocr done", WAIT, || sink.terminal().is_some()).await;
    assert!(matches!(
        sink.terminal(),
        Some(StreamEvent::Done { no_text: false, .. })
    ));
    let thumb = format!("{}.png", payload.capture_id);
    assert!(CropSource(Arc::clone(&core.captures)).fetch(&thumb).is_some());

    // 没有视觉模型：显示未就绪卡片（同样交叉淡入），不发请求
    let mut s = configured();
    s.defaults.ocr = None;
    let (env, core) = setup(s);
    core.dispatch(Command::OcrImage {
        image: gray_image(8, 8),
        origin: TriggerOrigin::Clipboard,
    });
    until("card", WAIT, || env.prepare_count() == 1).await;
    let (req, _) = env.last_prepare().expect("prepare");
    assert_eq!(req.transition, PanelTransition::Crossfade);
    assert_eq!(ocr_payload(&req).not_ready, Some(NotReady::NoVisionModel));
    assert_eq!(env.llm_count(), 0);
}

fn gray_image(width: u32, height: u32) -> wisp_capture::RgbaImage {
    let len = usize::try_from(width * height * 4).expect("size");
    wisp_capture::RgbaImage::from_raw(width, height, vec![250; len]).expect("pixels")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_translate_two_step_caches_the_original() {
    let mut settings = configured();
    settings.ocr.direct_translate_transcribe = true;
    let (env, core) = setup(settings);
    env.llm_outputs
        .lock()
        .insert("ocr", "Settings\nOpen the file to continue.".to_owned());
    hotkey(&core, HotkeyAction::OcrTranslate);
    until("overlays", WAIT, || env.called("create_overlays")).await;
    let init = core.overlay_init("overlay-0").expect("init").expect("session");
    let _ = core
        .overlay_commit(OverlayCommit {
            session_id: init.session_id,
            monitor_id: MONITOR.to_owned(),
            rect: Rect::new(0.0, 0.0, 100.0, 50.0),
            kind: OverlaySelectionKind::Window,
        })
        .await
        .expect("commit");
    let (req, session) = env.last_prepare().expect("prepare");
    let payload = ocr_payload(&req);
    assert_eq!(payload.mode, OcrMode::Translate);
    let sink = Collector::new();
    core.stream_attach(
        &RequestCaller::Panel,
        &payload.request_id.clone().expect("id"),
        sink.clone(),
    )
    .expect("attach");
    until("translated", WAIT, || sink.terminal().is_some()).await;
    assert!(matches!(sink.terminal(), Some(StreamEvent::Done { .. })));
    // 两次调用：先转录（不转发），再走翻译管线
    let purposes: Vec<Purpose> = env.llm_calls.lock().iter().map(|(_, p, _)| *p).collect();
    assert_eq!(purposes, vec![Purpose::Ocr, Purpose::Translate]);
    assert!(!sink.text().contains("Open the file"), "转录不转发给前端");
    assert!(env.copies.lock().is_empty(), "直接翻译不自动复制");
    // 「查看原文」：转录已缓存，不重复请求
    let original = Collector::new();
    core.ocr_start(
        OcrReq {
            session_id: session,
            capture_id: payload.capture_id.clone(),
            mode: OcrMode::Paragraphs,
            model_ref: None,
            target_lang: None,
            layout: None,
        },
        RequestHost::Panel { session_id: session },
        original.clone(),
    )
    .await
    .expect("view original");
    assert_eq!(original.text(), "Settings\nOpen the file to continue.");
    assert_eq!(env.llm_count(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn onboarding_in_front_receives_hotkeys_instead() {
    let (env, core) = setup(configured());
    *env.onboarding.lock() = (true, true);
    hotkey(&core, HotkeyAction::TranslateSelection);
    until("forwarded", WAIT, || !env.onboarding_hotkeys.lock().is_empty()).await;
    assert_eq!(env.onboarding_hotkeys.lock()[0], HotkeyAction::TranslateSelection);
    assert_eq!(env.selection_reads.load(Ordering::SeqCst), 0);
    // 引导页自己带着文本发起（origin = onboarding）：照常翻译
    core.dispatch(Command::TranslateText {
        text: "Try it".to_owned(),
        origin: TriggerOrigin::Onboarding,
    });
    until("panel", WAIT, || env.prepare_count() == 1).await;
    // 引导窗口不在前台：照常处理
    env.onboarding.lock().1 = false;
    env.selections.lock().push_back(SelScript {
        delay: Duration::ZERO,
        result: selection("selected"),
    });
    hotkey(&core, HotkeyAction::TranslateSelection);
    until("second panel", WAIT, || env.prepare_count() == 2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bus_commands_reach_their_services() {
    let (env, core) = setup(configured());
    core.dispatch(Command::ToggleClipboardPause);
    core.dispatch(Command::ClearClipboard { keep_pinned: true });
    core.dispatch(Command::CheckUpdate);
    core.dispatch(Command::OpenWindow(WindowTarget::Settings {
        section: None,
        anchor: None,
    }));
    hotkey(&core, HotkeyAction::Clipboard);
    until("clipboard panel", WAIT, || env.prepare_count() == 1).await;
    let (req, _) = env.last_prepare().expect("prepare");
    assert_eq!(req.view, PanelView::Clipboard);
    assert!(req.take_focus);
    assert!(req.prev_app.is_some(), "记录触发时的前台应用");
    // 剪贴板条目送翻译：交叉淡入
    core.dispatch(Command::TranslateText {
        text: "from clipboard".to_owned(),
        origin: TriggerOrigin::Clipboard,
    });
    until("crossfade", WAIT, || env.prepare_count() == 2).await;
    let (req, _) = env.last_prepare().expect("prepare");
    assert_eq!(req.transition, PanelTransition::Crossfade);
    // 剪贴板面板显示时再按一次：关闭
    hotkey(&core, HotkeyAction::Clipboard);
    until("clipboard again", WAIT, || env.prepare_count() == 3).await;
    hotkey(&core, HotkeyAction::Clipboard);
    for expected in [
        "toggle_clipboard_pause",
        "clear_clipboard:true",
        "check_update",
        "request_hide:Toggle",
    ] {
        assert!(env.called(expected), "{expected} in {:?}", env.calls());
    }
    assert!(env.calls().iter().any(|c| c.starts_with("open_window:")));
    env.calls.lock().clear();
    core.idle_prewarm();
    assert!(env.called("prewarm_selection"));
    assert!(env.called("prewarm"), "已授权：空闲时预热截图（SCK）");
    // 没有屏幕录制权限：不预热（预检绝不弹窗，也不碰 SCK）
    env.calls.lock().clear();
    *env.permission.lock() = PermissionState::Denied;
    core.idle_prewarm();
    assert!(!env.called("prewarm"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_monitor_abandons_the_display_and_the_request() {
    let mut s = configured();
    s.providers.clear();
    let (env, core) = setup(s);
    *env.no_monitor.lock() = true;
    // 没有请求的视图（未就绪卡片）：直接收尾
    translate_text(&core, "nothing to show");
    until("tried", WAIT, || env.called("panel_prepare")).await;
    until("run settled", WAIT, || core.run_count() == 0).await;
    assert_eq!(env.finished.lock().len(), 1);
    // 有请求的：请求一并放弃（34.1：prepare 返回 None 时不发起流）
    let (env, core) = setup(configured());
    *env.no_monitor.lock() = true;
    *env.llm_pace.lock() = (Duration::from_millis(300), Duration::from_millis(5));
    translate_text(&core, "abandoned request");
    until("tried", WAIT, || env.called("panel_prepare")).await;
    until("run settled", WAIT, || core.run_count() == 0).await;
    until("request removed", WAIT, || core.registry.is_empty()).await;
}

// ------------------------------------------------------------------------------------------------
// 截图会话的失败路径与 macOS 主路径、重入的先后、钥匙串不在关键路径上、宿主核对

fn commit_on(session_id: u64, monitor_id: &str) -> OverlayCommit {
    OverlayCommit {
        session_id,
        monitor_id: monitor_id.to_owned(),
        rect: Rect::new(10.0, 10.0, 50.0, 20.0),
        kind: OverlaySelectionKind::Drag,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overlay_show_failure_ends_the_session_with_a_card_and_capture_works_again() {
    let (env, core) = setup(configured());
    env.overlay_fail.lock().insert(MONITOR.to_owned());
    hotkey(&core, HotkeyAction::Ocr);
    until("overlays", WAIT, || env.called("create_overlays")).await;
    let init = core.overlay_init("overlay-0").expect("init").expect("session");
    core.overlay_ready(init.session_id, MONITOR);
    // 唯一的显示器显示失败：收尾（交还焦点 → 销毁 → 恢复 pinned）并按 34.6 显示截图失败卡片
    until("card", WAIT, || env.prepare_count() == 1).await;
    let (req, _) = env.last_prepare().expect("prepare");
    assert_eq!(
        ocr_payload(&req).not_ready,
        Some(NotReady::CaptureFailed {
            message: "errors.capture.backend".into()
        })
    );
    assert!(!core.overlay_active(), "会话已注销");
    assert!(!core.captures.has_frames());
    let calls = env.calls();
    let pos = |name: &str| calls.iter().position(|c| c == name).expect(name);
    assert!(pos("activate") < pos("destroy_overlays") && pos("destroy_overlays") < pos("restore"));
    // 再按 ⌥S 能重新进入截图
    env.overlay_fail.lock().clear();
    hotkey(&core, HotkeyAction::Ocr);
    until("second session", WAIT, || env.count("create_overlays") == 2).await;
    let init = core.overlay_init("overlay-0").expect("init").expect("session");
    core.overlay_ready(init.session_id, MONITOR);
    until("shown", WAIT, || {
        env.count(&format!("overlay_show:{MONITOR}:false")) == 2
    })
    .await;
    assert!(core.overlay_active());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_failing_monitor_keeps_the_session_on_the_other() {
    let (env, core) = setup(configured());
    *env.monitors.lock() = vec![monitor_at("m1", 0.0), monitor_at("m2", 200.0)];
    env.overlay_fail.lock().insert("m2".to_owned());
    hotkey(&core, HotkeyAction::Ocr);
    until("overlays", WAIT, || env.called("create_overlays")).await;
    let a = core.overlay_init("overlay-0").expect("init").expect("m1");
    let b = core.overlay_init("overlay-1").expect("init").expect("m2");
    assert_eq!(a.session_id, b.session_id);
    core.overlay_ready(a.session_id, "m1");
    core.overlay_ready(b.session_id, "m2");
    until("both tried", WAIT, || {
        env.called("overlay_show:m1:false") && env.called("overlay_show:m2:false")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(core.overlay_active(), "还有一块屏显示着，会话继续");
    assert_eq!(env.prepare_count(), 0);
    // 在显示成功的那块屏上照常 commit
    let ack = core
        .overlay_commit(commit_on(a.session_id, "m1"))
        .await
        .expect("commit");
    assert!(ack.fly_to.is_some());
    let (req, _) = env.last_prepare().expect("prepare");
    assert!(ocr_payload(&req).request_id.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_frozen_frames_answer_init_only_after_the_overlay_is_shown() {
    let (env, core) = setup(configured());
    *env.native_frames.lock() = true;
    // 每块屏的原生显示耗时 400 ms：逐块串行时两次 overlay:session 至少相隔 400 ms，并行时只差调度抖动。门限取 250 ms——
    // CI 的 macOS 虚拟机（3 核、与整套单测并发）上并行的两次实测可以相差 120 ms，150 / 100 ms 的阈值太紧
    let show_delay = Duration::from_millis(400);
    *env.native_show_delay.lock() = show_delay;
    *env.monitors.lock() = vec![monitor_at("m1", 0.0), monitor_at("m2", 200.0)];
    hotkey(&core, HotkeyAction::Ocr);
    until("overlays", WAIT, || env.called("create_overlays")).await;
    // overlay_show(Some(frame)) 返回之前 overlay_init 回 None
    assert_eq!(core.overlay_init("overlay-0").expect("init"), None);
    until("both shown", WAIT, || env.sessions_started.lock().len() == 2).await;
    // 两块屏并行显示（不是逐块经主线程往返）：两次 overlay:session 的间隔明显小于一块屏的显示耗时
    let started = env.sessions_started.lock().clone();
    let gap = if started[1].1 > started[0].1 {
        started[1].1 - started[0].1
    } else {
        started[0].1 - started[1].1
    };
    assert!(gap < show_delay * 5 / 8, "gap {gap:?}（串行时至少 {show_delay:?}）");
    // overlay:session 之后重新 init：frozen_native 取 overlay_show 的返回值，不再给 1/2 帧
    let init = core.overlay_init("overlay-0").expect("init").expect("session");
    assert!(init.frozen_native);
    assert_eq!(init.half_url, None);
    // 已经显示的屏，页面的 overlay_ready 不会再显示一次
    core.overlay_ready(init.session_id, "m1");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!env.called("overlay_show:m1:false"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_show_failure_falls_back_to_the_page_path() {
    let (env, core) = setup(configured());
    *env.native_frames.lock() = true;
    env.overlay_fail.lock().insert(MONITOR.to_owned());
    hotkey(&core, HotkeyAction::Ocr);
    until("session started", WAIT, || env.called("overlay_session_started")).await;
    // 原生冻结帧没装上：页面按 Windows / X11 的路径自己画冻结帧
    let init = core.overlay_init("overlay-0").expect("init").expect("session");
    assert!(!init.frozen_native);
    assert!(init.half_url.is_some());
    assert!(core.overlay_active(), "还没判失败：页面路径还能再试一次");
    // 页面画完首帧 overlay_ready → overlay_show(None) 这次成功
    env.overlay_fail.lock().clear();
    core.overlay_ready(init.session_id, MONITOR);
    until("page path shown", WAIT, || {
        env.called(&format!("overlay_show:{MONITOR}:false"))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(core.overlay_active());
    assert_eq!(env.prepare_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pinned_can_re_recognise_a_transferred_capture() {
    let (env, core) = setup(configured());
    hotkey(&core, HotkeyAction::Ocr);
    until("overlays", WAIT, || env.called("create_overlays")).await;
    let init = core.overlay_init("overlay-0").expect("init").expect("session");
    core.overlay_commit(commit_on(init.session_id, MONITOR))
        .await
        .expect("commit");
    let (req, session) = env.last_prepare().expect("prepare");
    let payload = ocr_payload(&req);
    let id = payload.request_id.clone().expect("request");
    // 固定：请求与截图一起转给 pinned，随后面板会话结束（固定交接隐藏面板）
    core.transfer_to_pinned(&id, "p1");
    env.hide(session);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(
        core.captures.crop_count(),
        1,
        "截图随固定交接转给 pinned，会话结束不释放"
    );
    let ocr = |host: RequestHost, sink: Arc<Collector>| {
        let core = Arc::clone(&core);
        let capture_id = payload.capture_id.clone();
        async move {
            core.ocr_start(
                OcrReq {
                    session_id: 0,
                    capture_id,
                    mode: OcrMode::Markdown,
                    model_ref: None,
                    target_lang: None,
                    layout: None,
                },
                host,
                sink,
            )
            .await
        }
    };
    let sink = Collector::new();
    ocr(pinned("p1"), sink.clone()).await.expect("pinned re-recognise");
    until("pinned ocr done", WAIT, || sink.terminal().is_some()).await;
    assert!(matches!(sink.terminal(), Some(StreamEvent::Done { .. })));
    // 面板（已经结束的会话）与别的 pinned 都碰不到它
    assert!(
        ocr(RequestHost::Panel { session_id: session }, Collector::new())
            .await
            .is_err()
    );
    assert!(ocr(pinned("p2"), Collector::new()).await.is_err());
    // 面板卸载时的 capture_release 静默忽略，pinned 的截图还在
    core.capture_release(&RequestCaller::Panel, &payload.capture_id);
    assert_eq!(core.captures.crop_count(), 1);
    core.pinned_closed("p1");
    assert_eq!(core.captures.crop_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_capture_never_clobbers_the_newer_session() {
    let (env, core) = setup(configured());
    // 第一次截图很慢；截图中被划词取代，紧接着又按 ⌥S（旧 run 已从表里移除，忙碌检查放行）
    env.capture_delays.lock().push_back(Duration::from_millis(200));
    hotkey(&core, HotkeyAction::Ocr);
    until("first capture running", WAIT, || env.called("capture_all")).await;
    hotkey(&core, HotkeyAction::TranslateSelection);
    hotkey(&core, HotkeyAction::Ocr);
    until("second session", WAIT, || env.called("create_overlays")).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(env.count("capture_all"), 2);
    assert_eq!(env.count("create_overlays"), 1, "迟到的旧截图没有建 Overlay");
    // 旧截图只恢复它自己隐藏的窗口，而且在新截图隐藏窗口之前（截图段按顺序执行）
    let calls = env.calls();
    let first_restore = calls.iter().position(|c| c == "restore").expect("restore");
    let second_hide = calls
        .iter()
        .enumerate()
        .filter(|(_, c)| *c == "hide_for_capture")
        .nth(1)
        .map(|(i, _)| i)
        .expect("second hide");
    assert!(first_restore < second_hide, "{calls:?}");
    // 新会话的冻结帧与 Overlay 完好
    let init = core.overlay_init("overlay-0").expect("init").expect("session");
    assert_eq!(core.overlay_session(), Some(init.session_id));
    let full = init.image_url.strip_prefix("capture/").expect("path").to_owned();
    assert!(FrameSource(Arc::clone(&core.captures)).fetch(&full).is_some());
    assert!(!env.called("destroy_overlays"));
    assert_eq!(env.selection_reads.load(Ordering::SeqCst), 0, "被取代的划词没有取词");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retrigger_reads_the_selection_only_after_the_overlay_is_gone() {
    let (env, core) = setup(configured());
    *env.destroy_delay.lock() = Duration::from_millis(80);
    hotkey(&core, HotkeyAction::Ocr);
    until("overlays", WAIT, || env.called("create_overlays")).await;
    // Overlay 显示中按划词：先拆掉 Overlay（交还焦点 → 销毁 → 恢复 pinned），再读前台应用与选区
    env.selections.lock().push_back(SelScript {
        delay: Duration::ZERO,
        result: selection("text in the previous app"),
    });
    hotkey(&core, HotkeyAction::TranslateSelection);
    until("panel", WAIT, || env.prepare_count() == 1).await;
    let calls = env.calls();
    let pos = |name: &str| calls.iter().position(|c| c == name).expect(name);
    assert!(pos("activate") < pos("read_selection"), "{calls:?}");
    assert!(pos("destroy_overlays") < pos("read_selection"), "{calls:?}");
    assert!(pos("restore") < pos("read_selection"), "{calls:?}");
    let (req, _) = env.last_prepare().expect("prepare");
    assert_eq!(translate_payload(&req).source_text, "text in the previous app");
    assert!(!core.overlay_active());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_latest_trigger_wins_even_when_the_older_prepare_is_slower() {
    let (env, core) = setup(configured());
    // 第一次 prepare 很慢、第二次很快：没有准备闸门时第一次会后进入窗口层、把第二次的面板替换掉
    env.prepare_delays
        .lock()
        .extend([Duration::from_millis(150), Duration::ZERO]);
    translate_text(&core, "The first text.");
    until("first prepare entered", WAIT, || env.called("prepare_enter")).await;
    // 第一次的 run 已经在 Requesting（请求已发出、面板还没出现）：新的触发照样作废它
    translate_text(&core, "The second text.");
    until("both prepared", WAIT, || env.prepare_count() == 2).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (req, session) = env.last_prepare().expect("prepare");
    assert_eq!(translate_payload(&req).source_text, "The second text.");
    assert_eq!(env.live_session(), Some(session), "最后显示的是最新一次触发");
    let first = translate_payload(&env.prepares.lock()[0].1)
        .request_id
        .expect("first id");
    assert_eq!(core.outcome(&first), None, "被作废的请求已取消并移除");
    assert_eq!(core.run_count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_key_is_read_while_selecting_not_after_the_panel_is_up() {
    let (env, core) = setup(configured());
    // 钥匙串 60 ms、取词 60 ms、prepare 20 ms：Key 在取词时就开始读，请求仍早于面板显示
    *env.key_delay.lock() = Duration::from_millis(60);
    *env.prepare_delay.lock() = Duration::from_millis(20);
    env.selections.lock().push_back(SelScript {
        delay: Duration::from_millis(60),
        result: selection("Read the key while selecting."),
    });
    hotkey(&core, HotkeyAction::TranslateSelection);
    until("panel", WAIT, || env.prepare_count() == 1).await;
    until("request", WAIT, || env.llm_count() == 1).await;
    let (prepared_at, _, _) = env.prepares.lock()[0].clone();
    let (_, _, sent_at) = env.llm_calls.lock()[0].clone();
    assert!(sent_at < prepared_at, "LLM 请求应早于 panel_prepare 返回");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn after_submitting_the_input_hotkey_opens_a_fresh_input() {
    let (env, core) = setup(configured());
    hotkey(&core, HotkeyAction::TranslateInput);
    until("input view", WAIT, || env.prepare_count() == 1).await;
    let (_, session) = env.last_prepare().expect("prepare");
    until("input bound", WAIT, || core.input_view_showing()).await;
    // 提交（⌘↩）：输入流程 Editing → Requesting，前端切到翻译视图（panel_state().view 仍是 input）
    let sink = Collector::new();
    core.translate_start(
        text_req(session, "typed text", "natural"),
        RequestHost::Panel { session_id: session },
        sink.clone(),
    )
    .await
    .expect("submit");
    assert!(!core.input_view_showing());
    until("translated", WAIT, || sink.terminal().is_some()).await;
    // Done 事件先到前端，run 随后（泵返回之后）推进到 Done
    until("run done", WAIT, || {
        matches!(core.run_of_session(session), Some((_, Flow::Input, Stage::Done)))
    })
    .await;
    // 再按输入快捷键：打开新的输入框，而不是关掉译文
    hotkey(&core, HotkeyAction::TranslateInput);
    until("fresh input", WAIT, || env.prepare_count() == 2).await;
    assert!(!env.called("request_hide:Toggle"));
    let (req, _) = env.last_prepare().expect("prepare");
    assert_eq!(req.view, PanelView::Input);
    // 新的输入框还没提交（会话在 prepare 返回后的同一个临界区里记到 run 上）：再按一次关闭
    until("fresh input bound", WAIT, || core.input_view_showing()).await;
    hotkey(&core, HotkeyAction::TranslateInput);
    assert!(env.called("request_hide:Toggle"));
}

/// 输入视图提交与 pinned / OCR 面板里的翻译同样先确认长文本——
/// `confirm_long = false` 且超过阈值时不发请求、`StartAck.long_text` 带回估算；以 true 重发才请求，run 走
/// Editing → Confirming → Requesting
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frontend_submissions_confirm_long_text_first() {
    let (env, core) = setup(configured());
    hotkey(&core, HotkeyAction::TranslateInput);
    until("input view", WAIT, || env.prepare_count() == 1).await;
    let (_, session) = env.last_prepare().expect("prepare");
    until("input bound", WAIT, || core.input_view_showing()).await;
    let long = "字".repeat(3001);
    let host = RequestHost::Panel { session_id: session };
    let ack = core
        .translate_start(text_req(session, &long, "natural"), host.clone(), Collector::new())
        .await
        .expect("submit");
    assert!(ack.request_id.is_empty(), "没有登记请求");
    assert_eq!(ack.long_text.as_ref().map(|l| l.chars), Some(3001));
    assert_eq!(env.llm_count(), 0, "长文本先不发请求");
    assert!(matches!(
        core.run_of_session(session),
        Some((_, Flow::Input, Stage::Confirming))
    ));
    assert!(!core.input_view_showing(), "确认中再按输入快捷键是打开新的输入框");
    // 「继续」：confirm_long = true 重发
    let sink = Collector::new();
    let ack = core
        .translate_start(
            TranslateReq {
                confirm_long: true,
                ..text_req(session, &long, "natural")
            },
            host,
            sink.clone(),
        )
        .await
        .expect("confirm");
    assert!(!ack.request_id.is_empty() && ack.long_text.is_none());
    until("requested", WAIT, || env.llm_count() == 1).await;
    until("done", WAIT, || sink.terminal().is_some()).await;

    // pinned（OCR 固定窗口里的翻译）与润色同样判；短文本、refine 不判
    let ack = core
        .translate_start(text_req(0, &long, "natural"), pinned("p"), Collector::new())
        .await
        .expect("pinned");
    assert!(ack.long_text.is_some() && ack.request_id.is_empty());
    let polish = TranslateReq {
        task: TaskKind::Polish,
        ..text_req(0, &long, "natural")
    };
    assert!(
        core.translate_start(polish, pinned("p"), Collector::new())
            .await
            .expect("polish")
            .long_text
            .is_some()
    );
    let short = core
        .translate_start(text_req(0, "short", "natural"), pinned("p"), Collector::new())
        .await
        .expect("short");
    assert!(short.long_text.is_none() && !short.request_id.is_empty());
    let refine = TranslateReq {
        task: TaskKind::Refine,
        draft: Some("译文".to_owned()),
        ..text_req(0, &long, "natural")
    };
    assert!(
        core.translate_start(refine, pinned("p"), Collector::new())
            .await
            .expect("refine")
            .long_text
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn view_original_requests_again_when_the_model_changes() {
    let mut settings = configured();
    settings.ocr.direct_translate_transcribe = true;
    let (env, core) = setup(settings);
    env.llm_outputs
        .lock()
        .insert("ocr", "Settings\nOpen the file to continue.".to_owned());
    hotkey(&core, HotkeyAction::OcrTranslate);
    until("overlays", WAIT, || env.called("create_overlays")).await;
    let init = core.overlay_init("overlay-0").expect("init").expect("session");
    core.overlay_commit(commit_on(init.session_id, MONITOR))
        .await
        .expect("commit");
    let (req, session) = env.last_prepare().expect("prepare");
    let payload = ocr_payload(&req);
    let sink = Collector::new();
    core.stream_attach(
        &RequestCaller::Panel,
        &payload.request_id.clone().expect("id"),
        sink.clone(),
    )
    .expect("attach");
    until("translated", WAIT, || sink.terminal().is_some()).await;
    let host = RequestHost::Panel { session_id: session };
    let view_original = |model: Option<wisp_types::ModelRef>| {
        let core = Arc::clone(&core);
        let host = host.clone();
        let capture_id = payload.capture_id.clone();
        async move {
            let out = Collector::new();
            core.ocr_start(
                OcrReq {
                    session_id: session,
                    capture_id,
                    mode: OcrMode::Paragraphs,
                    model_ref: model,
                    target_lang: None,
                    layout: None,
                },
                host,
                out.clone(),
            )
            .await
            .expect("ocr_start");
            out
        }
    };
    // 同一个模型（或不指定）：回放缓存的转录，不发请求
    let calls = env.llm_count();
    let same = view_original(Some(model_ref("p", "vision"))).await;
    assert!(matches!(same.terminal(), Some(StreamEvent::Done { cached: true, .. })));
    assert_eq!(env.llm_count(), calls);
    // 换了视觉模型：重新请求，结果覆盖缓存
    env.llm_outputs
        .lock()
        .insert("ocr", "Settings — recognised again.".to_owned());
    let other = view_original(Some(model_ref("p", "vision-2"))).await;
    until("re-recognised", WAIT, || other.terminal().is_some()).await;
    assert!(matches!(
        other.terminal(),
        Some(StreamEvent::Done { cached: false, .. })
    ));
    assert_eq!(env.llm_count(), calls + 1);
    let again = view_original(None).await;
    assert_eq!(again.text(), "Settings — recognised again.");
    assert_eq!(env.llm_count(), calls + 1);
}

/// 截图到 Overlay 显示出来（Windows / X11 路径：页面画完首帧 overlay_ready），返回 Overlay 会话 id
async fn overlay_up(env: &Arc<FakeEnv>, core: &Arc<Core>, action: HotkeyAction) -> u64 {
    hotkey(core, action);
    until("overlays", WAIT, || env.called("create_overlays")).await;
    let init = core.overlay_init("overlay-0").expect("init").expect("session");
    core.overlay_ready(init.session_id, MONITOR);
    until("overlay shown", WAIT, || {
        env.called(&format!("overlay_show:{MONITOR}:false"))
    })
    .await;
    init.session_id
}

/// Overlay 的出路不依赖页面——未 commit 期间注册全局 Esc；再按截图快捷键取消；commit 之后 Esc 交还页面
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overlay_sessions_have_exits_that_do_not_depend_on_the_page() {
    let (env, core) = setup(configured());
    let overlay = overlay_up(&env, &core, HotkeyAction::Ocr).await;
    until("global Esc registered", WAIT, || *env.esc_guard.lock()).await;
    // 埋点：第一块屏上屏的耗时进了这次追踪
    let trace = env.perf.last_id();
    until("overlay_ms recorded", WAIT, || {
        env.perf.snapshot(trace).and_then(|t| t.overlay_ms).is_some()
    })
    .await;
    // 再按截图快捷键：取消（原先忽略）
    hotkey(&core, HotkeyAction::Ocr);
    until("cancelled", WAIT, || !core.overlay_active()).await;
    until("global Esc removed", WAIT, || !*env.esc_guard.lock()).await;
    assert_ne!(core.overlay_session(), Some(overlay));
    // 全局 Esc：有会话时取消它
    env.calls.lock().clear();
    overlay_up(&env, &core, HotkeyAction::OcrTranslate).await;
    until("global Esc registered", WAIT, || *env.esc_guard.lock()).await;
    assert!(core.escape_overlay());
    until("cancelled by Esc", WAIT, || !core.overlay_active()).await;
    assert!(!core.escape_overlay(), "没有截图会话：交给面板");
    until("global Esc removed", WAIT, || !*env.esc_guard.lock()).await;
    // commit 之后撤下全局 Esc（Esc 交还页面：只收起 Overlay）
    env.calls.lock().clear();
    let overlay = overlay_up(&env, &core, HotkeyAction::Ocr).await;
    until("global Esc registered", WAIT, || *env.esc_guard.lock()).await;
    core.overlay_commit(commit_on(overlay, MONITOR)).await.expect("commit");
    until("global Esc removed after commit", WAIT, || !*env.esc_guard.lock()).await;
    core.overlay_done(overlay);
    until("done", WAIT, || !core.overlay_active()).await;
}

/// 原生冻结帧已经显示、页面却一直没有 init（WebContent 卡住 / 崩溃）：拆掉并提示截图失败
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_native_overlay_whose_page_never_initialises_is_torn_down() {
    let (env, core) = setup(configured());
    *env.native_frames.lock() = true;
    hotkey(&core, HotkeyAction::Ocr);
    until("native show", WAIT, || {
        env.called(&format!("overlay_show:{MONITOR}:true"))
    })
    .await;
    until("capture failed card", WAIT, || env.prepare_count() == 1).await;
    let (req, _) = env.last_prepare().expect("prepare");
    assert_eq!(
        ocr_payload(&req).not_ready,
        Some(NotReady::CaptureFailed {
            message: "errors.capture.backend".into()
        })
    );
    assert!(!core.overlay_active());
    // 页面及时 init 的会话不受影响
    let (env, core) = setup(configured());
    *env.native_frames.lock() = true;
    hotkey(&core, HotkeyAction::Ocr);
    until("native show", WAIT, || {
        env.called(&format!("overlay_show:{MONITOR}:true"))
    })
    .await;
    assert!(core.overlay_init("overlay-0").expect("init").is_some());
    tokio::time::sleep(Duration::from_millis(2300)).await;
    assert!(core.overlay_active(), "页面 init 过：不兜底拆除");
}

/// 多屏时按键焦点跟随光标（不点击就按 Esc / ↩ / ⌘A / 空格时，按键送到光标所在屏的页面）
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overlay_key_follows_the_cursor_across_displays() {
    let (env, core) = setup(configured());
    *env.monitors.lock() = vec![monitor(), monitor_at("m2", 200.0)];
    hotkey(&core, HotkeyAction::Ocr);
    until("overlays", WAIT, || env.called("create_overlays")).await;
    let session = core
        .overlay_init("overlay-0")
        .expect("init")
        .expect("session")
        .session_id;
    assert!(core.overlay_init("overlay-1").expect("init").is_some());
    core.overlay_ready(session, MONITOR);
    core.overlay_ready(session, "m2");
    until("both shown", WAIT, || env.called("overlay_show:m2:false")).await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(env.focused_overlays.lock().is_empty(), "光标没动：不抢焦点");
    *env.cursor.lock() = Some("m2".to_owned());
    until("key follows to m2", WAIT, || {
        env.focused_overlays.lock().last().map(String::as_str) == Some("m2")
    })
    .await;
    *env.cursor.lock() = Some(MONITOR.to_owned());
    until("and back", WAIT, || {
        env.focused_overlays.lock().last().map(String::as_str) == Some(MONITOR)
    })
    .await;
    core.overlay_commit(commit_on(session, MONITOR)).await.expect("commit");
    let n = env.focused_overlays.lock().len();
    *env.cursor.lock() = Some("m2".to_owned());
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(env.focused_overlays.lock().len(), n, "commit 之后不再跟随");
}

/// Rust 结束截图会话时先给页面发 `overlay:end`（拆除之前），原因与结束方式一一对应：页面取消 /
/// 全局 Esc → cancelled，再按截图快捷键 → retrigger，commit 之后的 done → done，页面迟迟不 init / 显示失败 → failed
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overlay_end_is_sent_before_teardown_with_the_reason() {
    let ends = |env: &Arc<FakeEnv>| env.overlay_ends.lock().clone();
    let (env, core) = setup(configured());
    // 页面取消
    let s1 = overlay_up(&env, &core, HotkeyAction::Ocr).await;
    core.overlay_cancel(s1);
    until("cancelled", WAIT, || env.called("destroy_overlays")).await;
    assert_eq!(ends(&env), vec![(s1, OverlayEndReason::Cancelled)]);
    let calls = env.calls();
    let pos = |name: &str| calls.iter().position(|c| c == name).expect(name);
    assert!(
        pos("overlay_end") < pos("activate"),
        "先通知页面，再交还焦点、销毁：{calls:?}"
    );
    assert!(pos("overlay_end") < pos("destroy_overlays"));
    // 再按截图快捷键
    until("first gone", WAIT, || !core.overlay_active()).await;
    env.calls.lock().clear();
    let s2 = overlay_up(&env, &core, HotkeyAction::Ocr).await;
    hotkey(&core, HotkeyAction::Ocr);
    until("retriggered", WAIT, || ends(&env).len() == 2).await;
    assert_eq!(ends(&env)[1], (s2, OverlayEndReason::Retrigger));
    // 全局 Esc
    until("second gone", WAIT, || !core.overlay_active()).await;
    env.calls.lock().clear();
    let s3 = overlay_up(&env, &core, HotkeyAction::OcrTranslate).await;
    until("global Esc registered", WAIT, || *env.esc_guard.lock()).await;
    assert!(core.escape_overlay());
    until("escaped", WAIT, || ends(&env).len() == 3).await;
    assert_eq!(ends(&env)[2], (s3, OverlayEndReason::Cancelled));
    // commit 之后的正常收尾
    until("third gone", WAIT, || !core.overlay_active()).await;
    env.calls.lock().clear();
    let s4 = overlay_up(&env, &core, HotkeyAction::Ocr).await;
    core.overlay_commit(commit_on(s4, MONITOR)).await.expect("commit");
    core.overlay_done(s4);
    until("done", WAIT, || ends(&env).len() == 4).await;
    assert_eq!(ends(&env)[3], (s4, OverlayEndReason::Done));
    until("overlay gone", WAIT, || !core.overlay_active()).await;
    // 飞入期间来了别的触发：提前拆掉，也是 done（commit 已经发生）
    env.calls.lock().clear();
    let s5 = overlay_up(&env, &core, HotkeyAction::Ocr).await;
    core.overlay_commit(commit_on(s5, MONITOR)).await.expect("commit");
    hotkey(&core, HotkeyAction::TranslateInput);
    until("torn down early", WAIT, || ends(&env).len() == 5).await;
    assert_eq!(ends(&env)[4], (s5, OverlayEndReason::Done));
    core.overlay_done(s5);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(ends(&env).len(), 5, "页面迟到的 done 不再发第二次");
    // 页面迟迟不 init（原生冻结帧已上屏）：failed
    let (env, core) = setup(configured());
    *env.native_frames.lock() = true;
    hotkey(&core, HotkeyAction::Ocr);
    until("failed", WAIT, || !ends(&env).is_empty()).await;
    assert_eq!(ends(&env)[0].1, OverlayEndReason::Failed);
}

/// 原生冻结帧上屏之后约 1.5 s 内没有 `overlay_init` 就拆掉；时间从会话上屏算起
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_overlay_init_watchdog_fires_after_about_one_and_a_half_seconds() {
    let (env, core) = setup(configured());
    *env.native_frames.lock() = true;
    hotkey(&core, HotkeyAction::Ocr);
    until("native show", WAIT, || {
        env.called(&format!("overlay_show:{MONITOR}:true"))
    })
    .await;
    let shown = Instant::now();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(core.overlay_active(), "1.2 s：还在等页面");
    until("torn down", Duration::from_secs(3), || !core.overlay_active()).await;
    let waited = shown.elapsed();
    assert!(waited < Duration::from_millis(1900), "{waited:?}");
}

/// 窗口层自己让按键焦点跟随指针（macOS 的 NSTrackingArea）时，编排器不再每 50 ms 查光标所在屏
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_cursor_polling_when_the_window_layer_follows_the_pointer() {
    let (env, core) = setup(configured());
    *env.follows_pointer.lock() = true;
    *env.monitors.lock() = vec![monitor(), monitor_at("m2", 200.0)];
    hotkey(&core, HotkeyAction::Ocr);
    until("overlays", WAIT, || env.called("create_overlays")).await;
    let session = core
        .overlay_init("overlay-0")
        .expect("init")
        .expect("session")
        .session_id;
    core.overlay_ready(session, MONITOR);
    core.overlay_ready(session, "m2");
    until("both shown", WAIT, || env.called("overlay_show:m2:false")).await;
    *env.cursor.lock() = Some("m2".to_owned());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(env.focused_overlays.lock().is_empty(), "没有轮询");
    // 页面报告指针进入（Windows / X11 的主路径）照常生效
    core.overlay_focus_request(session, "overlay-1");
    until("page-driven focus", WAIT, || !env.focused_overlays.lock().is_empty()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_trigger_during_the_fly_in_tears_the_overlay_down_first() {
    let (env, core) = setup(configured());
    *env.destroy_delay.lock() = Duration::from_millis(80);
    let overlay = overlay_up(&env, &core, HotkeyAction::Ocr).await;
    core.overlay_commit(commit_on(overlay, MONITOR)).await.expect("commit");
    assert_eq!(env.prepare_count(), 1, "OCR 面板已在 Overlay 下面显示");
    // 飞入 / 淡出期间（页面还没 overlay_done）按输入快捷键：先拆完 Overlay（销毁 → 交还焦点 → 恢复 pinned），
    // 再读前台应用、显示取焦点的输入视图
    hotkey(&core, HotkeyAction::TranslateInput);
    until("input view", WAIT, || env.prepare_count() == 2).await;
    let calls = env.calls();
    let input = calls.iter().rposition(|c| c == "prepare_enter").expect("input prepare");
    let pos = |name: &str| calls.iter().position(|c| c == name).expect(name);
    assert!(pos("destroy_overlays") < input, "{calls:?}");
    assert!(pos("activate") < input, "{calls:?}");
    assert!(pos("restore") < input, "{calls:?}");
    assert!(!core.overlay_active());
    let (req, _) = env.last_prepare().expect("prepare");
    assert_eq!(req.view, PanelView::Input);
    // 页面随后照常 overlay_done：不再拆一次，也不再交还焦点（不抢输入视图的焦点）
    core.overlay_done(overlay);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(env.count("destroy_overlays"), 1);
    assert!(!env.called("destroy_overlays:fade"), "提前拆掉时立即销毁，不淡出");
    assert_eq!(env.count("activate"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn returning_focus_after_the_fly_in_never_steals_it_from_the_panel() {
    let (env, core) = setup(configured());
    let overlay = overlay_up(&env, &core, HotkeyAction::Ocr).await;
    core.overlay_commit(commit_on(overlay, MONITOR)).await.expect("commit");
    // 飞入期间用户点进了 OCR 面板：它成了 key 窗口
    *env.panel_key.lock() = true;
    core.overlay_done(overlay);
    until("overlay torn down", WAIT, || env.called("restore")).await;
    assert!(env.called("destroy_overlays"));
    assert!(!env.called("activate"), "面板已有焦点时不交还给原应用");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_that_may_prompt_is_read_at_commit_not_while_framing() {
    let (env, core) = setup(configured());
    // macOS / Linux：第一次读这个 Provider 的 Key 可能弹系统对话框
    *env.key_prompts.lock() = true;
    let overlay = overlay_up(&env, &core, HotkeyAction::Ocr).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !env.called("key_get"),
        "框选期间不读：对话框不能出现在全屏 Overlay 之下、抢走它的键盘焦点"
    );
    core.overlay_commit(commit_on(overlay, MONITOR)).await.expect("commit");
    // commit 之后（飞入中）就读 Key 并发出请求：请求先于面板可见，不等 Overlay 拆完
    until("request during the fly-in", WAIT, || env.llm_count() == 1).await;
    assert!(!env.called("destroy_overlays"), "{:?}", env.calls());
    core.overlay_done(overlay);
    until("overlay destroyed", WAIT, || env.called("destroy_overlays")).await;
    // 读成功过一次 = 已授权：之后的截图在触发时就预读（用户框选的几秒里读好），commit 时不再读
    let (_, session) = env.last_prepare().expect("prepare");
    env.hide(session);
    env.calls.lock().clear();
    let overlay = overlay_up(&env, &core, HotkeyAction::Ocr).await;
    until("prefetched while framing", WAIT, || env.called("key_get")).await;
    core.overlay_commit(commit_on(overlay, MONITOR)).await.expect("commit");
    until("second request", WAIT, || env.llm_count() == 2).await;
    assert_eq!(env.count("key_get"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_that_may_prompt_waits_for_the_cache_miss_not_the_hotkey() {
    let (env, core) = setup(configured());
    *env.key_prompts.lock() = true;
    // 不发请求的路径（空选区 → 输入视图、长文本 → 确认卡片）完全不碰钥匙串
    hotkey(&core, HotkeyAction::TranslateSelection);
    until("input view", WAIT, || env.prepare_count() == 1).await;
    env.selections.lock().push_back(SelScript {
        delay: Duration::ZERO,
        result: selection(&"字".repeat(3001)),
    });
    hotkey(&core, HotkeyAction::TranslateSelection);
    until("long text card", WAIT, || env.prepare_count() == 2).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!env.called("key_get"), "{:?}", env.calls());
    // 要发请求了：取词完成、缓存未命中之后才读
    env.selections.lock().push_back(SelScript {
        delay: Duration::from_millis(40),
        result: selection("Read the key after the selection."),
    });
    env.calls.lock().clear();
    hotkey(&core, HotkeyAction::TranslateSelection);
    until("request", WAIT, || env.llm_count() == 1).await;
    let calls = env.calls();
    let pos = |name: &str| calls.iter().position(|c| c == name).expect(name);
    assert!(pos("read_selection_done") < pos("key_get"), "{calls:?}");
    // 已授权：之后的划词与取词并行预读
    env.selections.lock().push_back(SelScript {
        delay: Duration::from_millis(100),
        result: selection("Now the key is read while selecting."),
    });
    env.calls.lock().clear();
    hotkey(&core, HotkeyAction::TranslateSelection);
    until("second request", WAIT, || env.llm_count() == 2).await;
    let calls = env.calls();
    let pos = |name: &str| calls.iter().position(|c| c == name).expect(name);
    assert!(pos("key_get") < pos("read_selection_done"), "{calls:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unanswered_keychain_dialog_ends_with_a_retryable_error() {
    let (env, core) = setup(configured());
    // 授权框一直没人回答
    *env.key_delay.lock() = KEY_WAIT + Duration::from_millis(400);
    let sink = Collector::new();
    let t0 = Instant::now();
    core.translate_start(text_req(0, "hello world", "natural"), pinned("p"), sink.clone())
        .await
        .expect("start");
    until("error", WAIT, || sink.terminal().is_some()).await;
    assert!(
        matches!(sink.terminal(), Some(StreamEvent::Error(e)) if e.code == ErrorCode::Keychain && e.retryable),
        "{:?}",
        sink.terminal()
    );
    assert!(
        t0.elapsed() < KEY_WAIT + Duration::from_millis(400),
        "{:?}",
        t0.elapsed()
    );
    // 等待中用户关掉了窗口：立即结束；钥匙串之后才回答也不会再发请求
    let waiting = Collector::new();
    let ack = core
        .translate_start(text_req(0, "another text", "natural"), pinned("p"), waiting.clone())
        .await
        .expect("start");
    tokio::time::sleep(Duration::from_millis(50)).await;
    core.cancel(
        &RequestCaller::Pinned {
            pinned_id: "p".to_owned(),
        },
        &ack.request_id,
    );
    assert!(is_cancelled(&waiting.terminal()));
    tokio::time::sleep(KEY_WAIT + Duration::from_millis(500)).await;
    assert_eq!(env.llm_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wayland_shows_the_capture_card_without_hiding_anything() {
    let (env, core) = setup(configured());
    *env.wayland.lock() = true;
    hotkey(&core, HotkeyAction::Ocr);
    until("card", WAIT, || env.prepare_count() == 1).await;
    let (req, _) = env.last_prepare().expect("prepare");
    assert_eq!(req.view, PanelView::Ocr);
    assert_eq!(
        ocr_payload(&req).not_ready,
        Some(NotReady::CaptureFailed {
            message: reason::WAYLAND_CAPTURE.to_owned()
        })
    );
    // pinned 不闪、面板会话不被「截图前隐藏」结束、不截图、不读钥匙串
    for name in ["hide_for_capture", "capture_all", "restore", "key_get"] {
        assert!(!env.called(name), "{name}");
    }
    assert!(!core.overlay_active());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn onboarding_checks_finishing_out_of_order_keep_the_latest_trigger() {
    let (env, core) = setup(configured());
    // 引导窗口开着、但不在前台：每个动作都要先查一次窗口焦点；第一次查询慢、第二次快
    *env.onboarding.lock() = (true, false);
    env.onboarding_delays
        .lock()
        .extend([Duration::from_millis(150), Duration::ZERO]);
    env.selections.lock().push_back(SelScript {
        delay: Duration::ZERO,
        result: selection("the older trigger"),
    });
    hotkey(&core, HotkeyAction::TranslateSelection);
    // 第一次查询已经开始（取走了 150 ms 的那个耗时），再按第二次
    until("first check started", WAIT, || {
        env.onboarding_checks.load(Ordering::SeqCst) == 1
    })
    .await;
    hotkey(&core, HotkeyAction::TranslateInput);
    until("input view", WAIT, || env.prepare_count() == 1).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    // 较早的那次虽然后完成查询，也不会再执行、作废更新的那次
    assert_eq!(env.prepare_count(), 1);
    assert_eq!(env.last_prepare().expect("prepare").0.view, PanelView::Input);
    assert_eq!(env.selection_reads.load(Ordering::SeqCst), 0);
    // 埋点从动作到达算起，查询焦点的等待也算在内
    env.onboarding_delays.lock().push_back(Duration::from_millis(80));
    let before = env.perf.last_id();
    hotkey(&core, HotkeyAction::Clipboard);
    until("clipboard", WAIT, || env.prepare_count() == 2).await;
    until("prepare marked", WAIT, || {
        env.perf.first_after(before).is_some_and(|t| t.prepare_ms.is_some())
    })
    .await;
    let trace = env.perf.first_after(before).expect("trace");
    assert!(trace.prepare_ms.is_some_and(|ms| ms >= 80), "{trace:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ocr_request_goes_out_before_the_overlay_is_gone_even_with_slow_encoding() {
    let (env, core) = setup(configured());
    // 大选区在 debug 构建下编码要上百毫秒：commit 不等编码，飞入目标照常立即返回
    *core.encode_delay.lock() = Duration::from_millis(220);
    *env.prepare_delay.lock() = Duration::from_millis(10);
    let overlay = overlay_up(&env, &core, HotkeyAction::Ocr).await;
    let ack = core.overlay_commit(commit_on(overlay, MONITOR)).await.expect("commit");
    assert!(ack.fly_to.is_some());
    assert_eq!(env.llm_count(), 0, "commit 返回时编码还没完成（飞入不被编码推迟）");
    // 计时口径：面板在 Overlay 下面准备，Overlay 飞入（220 ms）+ 淡出（140 ms）后拆掉，用户此时才看得见面板
    tokio::time::sleep(Duration::from_millis(360)).await;
    core.overlay_done(overlay);
    until("overlay gone", WAIT, || env.called("destroy_overlays")).await;
    let calls = env.calls();
    let pos = |name: &str| calls.iter().position(|c| c == name).expect(name);
    assert!(pos("llm_stream") < pos("destroy_overlays"), "{calls:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pinning_a_finished_ocr_keeps_its_capture_for_the_pinned_window() {
    let (env, core) = setup(configured());
    let overlay = overlay_up(&env, &core, HotkeyAction::Ocr).await;
    core.overlay_commit(commit_on(overlay, MONITOR)).await.expect("commit");
    core.overlay_done(overlay);
    let (req, session) = env.last_prepare().expect("prepare");
    let payload = ocr_payload(&req);
    let id = payload.request_id.clone().expect("request");
    let sink = Collector::new();
    core.stream_attach(&RequestCaller::Panel, &id, sink.clone())
        .expect("attach");
    until("ocr done", WAIT, || sink.terminal().is_some()).await;
    // 识别完成之后才固定（最常见的情况）：PinState.request_id 照样填这个已经结束的请求（固定窗口据此回放结果）
    core.transfer_to_pinned(&id, "p1");
    env.hide(session);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(core.captures.crop_count(), 1, "截图随固定交接转给 pinned");
    let p1 = RequestCaller::Pinned {
        pinned_id: "p1".to_owned(),
    };
    // pinned 页面 attach 得到完整回放
    let replay = Collector::new();
    core.stream_attach(&p1, &id, replay.clone()).expect("pinned attach");
    assert!(matches!(replay.terminal(), Some(StreamEvent::Done { .. })));
    assert_eq!(replay.text(), sink.text());
    // 重新识别可用
    let again = Collector::new();
    core.ocr_start(
        OcrReq {
            session_id: 0,
            capture_id: payload.capture_id.clone(),
            mode: OcrMode::Markdown,
            model_ref: None,
            target_lang: None,
            layout: None,
        },
        pinned("p1"),
        again.clone(),
    )
    .await
    .expect("pinned re-recognise");
    until("re-recognised", WAIT, || again.terminal().is_some()).await;
    assert!(matches!(again.terminal(), Some(StreamEvent::Done { .. })));
    core.pinned_closed("p1");
    assert_eq!(core.captures.crop_count(), 0);
}

/// OCR 历史默认关闭；开启后 Done 且有文字的结果连同原图、模式、模型显示名交给剪贴板服务，
/// 同一张截图同一模式的重新识别取代上一条，换模式的识别各留一条
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ocr_history_records_results_and_replaces_re_recognition() {
    async fn capture(env: &Arc<FakeEnv>, core: &Arc<Core>) -> (wisp_types::OcrPayload, u64) {
        hotkey(core, HotkeyAction::Ocr);
        until("overlays", WAIT, || env.called("create_overlays")).await;
        let init = core.overlay_init("overlay-0").expect("init").expect("session");
        core.overlay_ready(init.session_id, MONITOR);
        until("overlay shown", WAIT, || {
            env.called(&format!("overlay_show:{MONITOR}:false"))
        })
        .await;
        core.overlay_commit(OverlayCommit {
            session_id: init.session_id,
            monitor_id: MONITOR.to_owned(),
            rect: Rect::new(10.0, 10.0, 50.0, 20.0),
            kind: OverlaySelectionKind::Drag,
        })
        .await
        .expect("commit");
        let (req, session) = env.last_prepare().expect("prepare");
        let payload = ocr_payload(&req);
        let sink = Collector::new();
        core.stream_attach(
            &RequestCaller::Panel,
            payload.request_id.as_deref().expect("request"),
            sink.clone(),
        )
        .expect("attach");
        until("ocr done", WAIT, || sink.terminal().is_some()).await;
        core.overlay_done(init.session_id);
        until("overlay torn down", WAIT, || !core.overlay_active()).await;
        (payload, session)
    }

    // 默认关闭：不交给剪贴板服务
    let (env, core) = setup(configured());
    capture(&env, &core).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(env.ocr_history.lock().is_empty());

    let mut settings = configured();
    settings.ocr.history = true;
    let (env, core) = setup(settings);
    let (payload, session) = capture(&env, &core).await;
    until("recorded", WAIT, || env.ocr_history.lock().len() == 1).await;
    {
        let h = env.ocr_history.lock();
        assert_eq!(h[0].mode, OcrMode::Lines);
        assert!(h[0].text.contains("Wisp"), "{}", h[0].text);
        assert_eq!(h[0].model, "vision");
        assert!(h[0].image.as_ref().is_some_and(|b| !b.is_empty()), "带上了原图");
        // 缩略图在编码截图时从裁剪帧生成（不必在写入时再解码原图）
        let thumb = h[0].thumb.as_ref().expect("带上了编码时生成的缩略图");
        assert!(
            thumb.starts_with(&[0xFF, 0xD8]) || thumb.starts_with(b"\x89PNG"),
            "JPEG / PNG"
        );
        assert_eq!(h[0].replaces, None);
    }
    let rerun = |mode: OcrMode| {
        let core = Arc::clone(&core);
        let capture_id = payload.capture_id.clone();
        async move {
            let sink = Collector::new();
            core.ocr_start(
                OcrReq {
                    session_id: session,
                    capture_id,
                    mode,
                    model_ref: None,
                    target_lang: None,
                    layout: None,
                },
                RequestHost::Panel { session_id: session },
                sink.clone(),
            )
            .await
            .expect("ocr_start");
            until("re-recognised", WAIT, || sink.terminal().is_some()).await;
        }
    };
    // 同一模式重新识别：取代第 1 条
    rerun(OcrMode::Lines).await;
    until("recorded again", WAIT, || env.ocr_history.lock().len() == 2).await;
    assert_eq!(env.ocr_history.lock()[1].replaces, Some(1));
    // 换模式：各留一条；再用这个模式识别时取代的是它自己的那条
    rerun(OcrMode::Markdown).await;
    until("markdown recorded", WAIT, || env.ocr_history.lock().len() == 3).await;
    assert_eq!(env.ocr_history.lock()[2].replaces, None);
    assert_eq!(env.ocr_history.lock()[2].mode, OcrMode::Markdown);
    rerun(OcrMode::Lines).await;
    until("lines recorded", WAIT, || env.ocr_history.lock().len() == 4).await;
    assert_eq!(env.ocr_history.lock()[3].replaces, Some(2));

    // 写库很慢（机器繁忙）时连续两次重新识别：第一次的写库还没完成，第二次请求就结束了（它的写库本身很快）。「读要
    // 取代的条目 → 写库 → 记下新条目」串行执行，不会两次都取代同一条、留下两条近乎重复的记录（不串行时
    // 会留下两条：两次都读到第 4 条）
    env.ocr_history_delay.lock().push_back(Duration::from_millis(600));
    rerun(OcrMode::Lines).await;
    rerun(OcrMode::Lines).await;
    assert!(
        env.ocr_history.lock().len() < 6,
        "第二次请求结束时第一次写库还没完成（用例前提）"
    );
    until("slow writes settled", WAIT, || env.ocr_history.lock().len() >= 5).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    let h = env.ocr_history.lock();
    let lines: Vec<(i64, Option<i64>)> = h
        .iter()
        .enumerate()
        .filter(|(_, r)| r.mode == OcrMode::Lines)
        .map(|(i, r)| (i64::try_from(i + 1).expect("id"), r.replaces))
        .collect();
    let replaced: Vec<i64> = lines.iter().filter_map(|(_, r)| *r).collect();
    let mut distinct = replaced.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(distinct.len(), replaced.len(), "同一条被取代了两次：{lines:?}");
    let survivors: Vec<i64> = lines
        .iter()
        .map(|(id, _)| *id)
        .filter(|id| !replaced.contains(id))
        .collect();
    assert_eq!(survivors.len(), 1, "lines 模式只留一条：{lines:?}");
}

/// 面板正显示截图失败卡片时，点卡片上的 [重新截图]（`trigger_action(ocr | ocr_translate, text = null,
/// origin = panel)`）——卡片先隐藏、不进截图；Overlay 会话照常走完，结果在新的 OCR 面板里；焦点最后交还的是触发卡片之前的
/// 那个应用（面板不是「前台应用」）；埋点记为 origin = panel
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retaking_a_failed_capture_from_its_card_hides_the_card_first() {
    let (env, core) = setup(configured());
    *env.capture_error.lock() = Some(CaptureError::Backend("X11 capture timed out".into()));
    hotkey(&core, HotkeyAction::OcrTranslate);
    until("card", WAIT, || env.prepare_count() == 1).await;
    let (req, card) = env.last_prepare().expect("card");
    assert!(matches!(
        ocr_payload(&req).not_ready,
        Some(NotReady::CaptureFailed { .. })
    ));
    assert!(env.panel_session_live(card));
    let card_trace = env.perf.first_after(0).expect("card trace").id;

    // 用户点了卡片上的按钮：面板此刻是 key 窗口
    *env.capture_error.lock() = None;
    *env.panel_key.lock() = true;
    env.calls.lock().clear();
    core.dispatch(Command::Action {
        action: HotkeyAction::OcrTranslate,
        origin: TriggerOrigin::Panel,
    });
    until("overlays", WAIT, || env.called("create_overlays")).await;
    let calls = env.calls();
    let pos = |name: &str| calls.iter().position(|c| c == name).expect(name);
    assert!(
        pos("hide_for_capture") < pos("capture_all"),
        "卡片在截图之前隐藏，不出现在冻结帧里：{calls:?}"
    );
    assert!(!env.panel_session_live(card), "卡片的会话已结束");
    let trace = env.perf.first_after(card_trace).expect("panel trace");
    assert_eq!(trace.origin, TriggerOrigin::Panel);

    let init = core.overlay_init("overlay-0").expect("init").expect("session");
    core.overlay_ready(init.session_id, MONITOR);
    until("overlay shown", WAIT, || {
        env.called(&format!("overlay_show:{MONITOR}:false"))
    })
    .await;
    core.overlay_commit(commit_on(init.session_id, MONITOR))
        .await
        .expect("commit");
    assert_eq!(env.prepare_count(), 2);
    let (req, session) = env.last_prepare().expect("result panel");
    assert_ne!(session, card);
    let payload = ocr_payload(&req);
    assert_eq!(payload.not_ready, None, "这次是结果，不是卡片");
    assert_eq!(payload.mode, OcrMode::Translate, "沿用卡片的模式");
    assert!(payload.request_id.is_some());
    assert_eq!(
        req.prev_app.as_ref().map(|a| a.id.as_str()),
        Some("com.apple.TextEdit"),
        "交还焦点的目标是触发卡片之前的应用"
    );
    core.overlay_done(init.session_id);
    until("overlay torn down", WAIT, || {
        env.called("destroy_overlays") && env.called("restore")
    })
    .await;
    until("focus returned", WAIT, || env.called("activate")).await;
    assert!(!core.overlay_active());
}

/// 设置窗口在前台时送翻译——识别历史的「重新翻译」（`trigger_action(translate_selection, text,
/// origin = ocr_history)`）：不取词、请求照常先于面板发出、面板在光标处；面板不记「此前的应用」（平台的 frontmost 跳过
/// Wisp 自己的窗口，给的是更早的别的应用——记下它，关闭面板时就会把焦点交给它，把用户从设置窗口里拽走）。
/// 同一时刻别的来源（快捷键送来的文本）照旧记 frontmost
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retranslating_from_the_settings_window_keeps_the_focus_in_wisp() {
    let (env, core) = setup(configured());
    core.dispatch(Command::TranslateText {
        text: "The quick brown fox jumps over the lazy dog.".to_owned(),
        origin: TriggerOrigin::OcrHistory,
    });
    until("panel", WAIT, || env.prepare_count() == 1).await;
    let (req, session) = env.last_prepare().expect("panel");
    assert_eq!(req.view, PanelView::Translate);
    assert_eq!(req.prev_app, None, "设置窗口在前台：没有要交还焦点 / 粘贴的应用");
    assert!(!req.take_focus);
    assert!(matches!(req.anchor, crate::windows::AnchorSource::Cursor));
    assert_eq!(env.selection_reads.load(Ordering::SeqCst), 0, "文本是送来的，不取词");
    let payload = translate_payload(&req);
    let id = payload.request_id.expect("request");
    assert_eq!(env.llm_count(), 1);
    let trace = env.perf.first_after(0).expect("trace");
    assert_eq!(trace.origin, TriggerOrigin::OcrHistory);
    assert_eq!(trace.session_id, Some(session));
    let sink = Collector::new();
    core.stream_attach(&RequestCaller::Panel, &id, sink.clone())
        .expect("attach");
    until("done", WAIT, || sink.terminal().is_some()).await;
    assert!(matches!(sink.terminal(), Some(StreamEvent::Done { .. })));
    assert!(!env.called("activate"), "没有交还焦点");

    // 对照：同样带文本、但来自别处（CLI / 快捷键送来的）——照旧记触发时的前台应用
    translate_text(&core, "Another sentence to translate.");
    until("second panel", WAIT, || env.prepare_count() == 2).await;
    let (req, _) = env.last_prepare().expect("panel");
    assert_eq!(req.prev_app.as_ref().map(|a| a.id.as_str()), Some("com.apple.TextEdit"));
}

/// commit 之后窗口层不再让按键焦点跟随指针切屏（macOS 的 NSTrackingArea 与页面请求、编排器的光标轮询同一
/// 规则）——commit 时通知窗口层，之后页面的 `overlay_focus` 请求也不再生效
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn committing_stops_the_key_from_following_the_pointer() {
    let (env, core) = setup(configured());
    *env.follows_pointer.lock() = true;
    *env.monitors.lock() = vec![monitor(), monitor_at("m2", 200.0)];
    let overlay = overlay_up(&env, &core, HotkeyAction::Ocr).await;
    assert!(env.committed.lock().is_empty(), "commit 之前照常跟随");
    core.overlay_focus_request(overlay, "overlay-1");
    until("follows before commit", WAIT, || env.focused_overlays.lock().len() == 1).await;
    core.overlay_commit(commit_on(overlay, MONITOR)).await.expect("commit");
    assert_eq!(*env.committed.lock(), vec![overlay], "commit 时通知窗口层停止跟随");
    core.overlay_focus_request(overlay, "overlay-1");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        env.focused_overlays.lock().len(),
        1,
        "commit 之后页面的请求不再切换 key"
    );
    core.overlay_done(overlay);
    until("done", WAIT, || !core.overlay_active()).await;
}

// ---- 离线模型与识别引擎 ----

/// 截图 → commit → 接上请求的流，等它结束
async fn capture_and_wait(
    env: &Arc<FakeEnv>,
    core: &Arc<Core>,
    action: HotkeyAction,
) -> (wisp_types::OcrPayload, u64, Arc<Collector>) {
    let overlay = overlay_up(env, core, action).await;
    core.overlay_commit(commit_on(overlay, MONITOR)).await.expect("commit");
    core.overlay_done(overlay);
    let (req, session) = env.last_prepare().expect("prepare");
    let payload = ocr_payload(&req);
    let sink = Collector::new();
    if let Some(id) = payload.request_id.as_deref() {
        core.stream_attach(&RequestCaller::Panel, id, sink.clone())
            .expect("attach");
        until("request ended", WAIT, || sink.terminal().is_some()).await;
    }
    // 下一次截图之前 Overlay 已经拆完（拆除期间的截图快捷键会被忽略），截图记录也清掉
    until("overlay torn down", WAIT, || !core.overlay_active()).await;
    env.calls.lock().retain(|c| c != "create_overlays");
    (payload, session, sink)
}

fn offline_ocr(model: LocalModelId) -> Settings {
    let mut s = configured();
    s.offline.ocr_model = Some(model);
    s
}

/// 离线 OCR：识别经引擎（交给它的是缩放后的像素，不经 PNG），不发 LLM 请求、不碰钥匙串；冷启动的「加载中」照常转发；
/// 不支持的默认模式（PP-OCR 的 markdown）退回 paragraphs；识别历史记下引擎的名字
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_ocr_runs_on_the_engine_and_history_records_its_name() {
    let mut settings = offline_ocr(LocalModelId::PpOcrV6Small);
    settings.ocr.history = true;
    settings.ocr.default_mode = OcrMode::Markdown;
    let (env, core) = setup(settings);
    env.offline_ready(LocalModelId::PpOcrV6Small);
    *env.keychain.lock() = (KeychainState::Unavailable, false);
    let (payload, _, sink) = capture_and_wait(&env, &core, HotkeyAction::Ocr).await;
    assert_eq!(
        payload.mode,
        OcrMode::Paragraphs,
        "PP-OCR 不识别版面：按 paragraphs 显示"
    );
    assert!(matches!(
        sink.terminal(),
        Some(StreamEvent::Done { no_text: false, .. })
    ));
    assert_eq!(sink.text(), "Settings\nOpen the file to continue.");
    assert_eq!(phases(&sink), vec![StreamPhase::LoadingModel]);
    {
        let calls = env.engine_calls.lock();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, EngineRef::Local(LocalModelId::PpOcrV6Small));
        assert_eq!(calls[0].1, OcrMode::Paragraphs);
        assert!(calls[0].2, "离线引擎直接拿像素");
    }
    assert_eq!(env.llm_count(), 0);
    assert_eq!(env.count("key_get"), 0, "{:?}", env.calls());
    match sink.take().first() {
        Some(StreamEvent::Start {
            provider_id,
            model_label,
            ..
        }) => {
            assert_eq!(provider_id, wisp_types::defaults::OFFLINE_PROVIDER_ID);
            assert_eq!(model_label, "PP-OCRv6 Small");
        }
        other => panic!("expected Start first, got {other:?}"),
    }
    until("recorded", WAIT, || env.ocr_history.lock().len() == 1).await;
    let h = env.ocr_history.lock();
    assert_eq!(h[0].model, "PP-OCRv6 Small");
    assert_eq!(h[0].mode, OcrMode::Paragraphs);
    assert!(h[0].image.as_ref().is_some_and(|b| !b.is_empty()), "原图照常编码保存");
}

/// 加载中被取消：请求以取消结束，之后不再有任何事件
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_while_the_offline_model_loads_ends_the_request() {
    let (env, core) = setup(offline_ocr(LocalModelId::PpOcrV6Tiny));
    env.offline_ready(LocalModelId::PpOcrV6Tiny);
    env.engine.lock().first_delay = Duration::from_secs(30);
    let overlay = overlay_up(&env, &core, HotkeyAction::Ocr).await;
    core.overlay_commit(commit_on(overlay, MONITOR)).await.expect("commit");
    let (req, _) = env.last_prepare().expect("prepare");
    let id = ocr_payload(&req).request_id.clone().expect("request");
    let sink = Collector::new();
    core.stream_attach(&RequestCaller::Panel, &id, sink.clone())
        .expect("attach");
    until("loading", WAIT, || phases(&sink) == vec![StreamPhase::LoadingModel]).await;
    core.cancel(&RequestCaller::Panel, &id);
    until("cancelled", WAIT, || sink.terminal().is_some()).await;
    assert!(is_cancelled(&sink.terminal()));
    let n = sink.take().len();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(sink.take().len(), n, "取消之后没有任何事件");
    assert_eq!(sink.deltas(), 0);
    assert_eq!(env.llm_count(), 0);
}

/// 截图直接翻译、离线 OCR + 离线翻译：识别引擎不能一次出译文，强制两步；第二步也在本机（合成服务商），全程没有
/// 任何云端请求、不碰钥匙串
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_translation_with_offline_models_stays_on_this_device() {
    let mut settings = offline_ocr(LocalModelId::PpOcrV6Small);
    settings.offline.translate_model = Some(LocalModelId::IndexTranslate2b);
    assert!(
        !settings.ocr.direct_translate_transcribe,
        "没开「同时转录原文」也走两步"
    );
    let (env, core) = setup(settings);
    env.offline_ready(LocalModelId::PpOcrV6Small);
    env.offline_ready(LocalModelId::IndexTranslate2b);
    let (payload, session, sink) = capture_and_wait(&env, &core, HotkeyAction::OcrTranslate).await;
    assert_eq!(payload.mode, OcrMode::Translate);
    assert!(
        matches!(sink.terminal(), Some(StreamEvent::Done { .. })),
        "{:?}",
        sink.terminal()
    );
    {
        let calls = env.engine_calls.lock();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, OcrMode::Paragraphs, "第一步是 paragraphs 转录");
    }
    assert_eq!(
        *env.llm_providers.lock(),
        vec![wisp_types::defaults::OFFLINE_PROVIDER_ID.to_owned()],
        "第二步也在本机"
    );
    assert_eq!(env.count("key_get"), 0, "{:?}", env.calls());
    assert!(!sink.text().contains("Open the file"), "转录不转发给前端");
    assert_eq!(
        phases(&sink),
        vec![StreamPhase::LoadingModel],
        "第一步的加载提示照常转发"
    );
    // 「查看原文」：转录已缓存
    let original = Collector::new();
    core.ocr_start(
        OcrReq {
            session_id: session,
            capture_id: payload.capture_id.clone(),
            mode: OcrMode::Paragraphs,
            model_ref: None,
            target_lang: None,
            layout: None,
        },
        RequestHost::Panel { session_id: session },
        original.clone(),
    )
    .await
    .expect("view original");
    assert_eq!(original.text(), "Settings\nOpen the file to continue.");
    assert_eq!(env.engine_count(), 1);
    // 面板里从菜单要 translate 模式的重新识别：同样两步
    let again = Collector::new();
    core.ocr_start(
        OcrReq {
            session_id: session,
            capture_id: payload.capture_id.clone(),
            mode: OcrMode::Translate,
            model_ref: None,
            target_lang: None,
            layout: None,
        },
        RequestHost::Panel { session_id: session },
        again.clone(),
    )
    .await
    .expect("translate again");
    until("translated again", WAIT, || again.terminal().is_some()).await;
    assert!(matches!(again.terminal(), Some(StreamEvent::Done { .. })));
    assert_eq!(env.engine_count(), 2, "重新识别了一次");
    // 同一段转录、同一个离线模型：译文命中翻译缓存（缓存键里有 @offline 与模型 id）
    assert!(
        again.take().iter().any(|e| matches!(
            e,
            StreamEvent::Notice {
                kind: NoticeKind::CacheHit
            }
        )),
        "{:?}",
        again.take()
    );
    assert!(env.llm_providers.lock().iter().all(|p| p == "@offline"));
}

/// 选用的离线模型还没下载 / 正在下载：不截图，直接显示对应的卡片；前端发起的请求以 ErrorCard 结束
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_or_downloading_offline_models_show_cards_without_requests() {
    let (env, core) = setup(offline_ocr(LocalModelId::PaddleOcrVl16));
    hotkey(&core, HotkeyAction::Ocr);
    until("card", WAIT, || env.prepare_count() == 1).await;
    let (req, _) = env.last_prepare().expect("prepare");
    assert_eq!(
        ocr_payload(&req).not_ready,
        Some(NotReady::OfflineModelMissing {
            model: LocalModelId::PaddleOcrVl16
        })
    );
    assert!(!env.called("capture_all"), "没就绪时不截图");
    env.offline
        .lock()
        .insert(LocalModelId::PaddleOcrVl16, OfflineStatus::Downloading);
    hotkey(&core, HotkeyAction::Ocr);
    until("second card", WAIT, || env.prepare_count() == 2).await;
    let (req, session) = env.last_prepare().expect("prepare");
    assert_eq!(
        ocr_payload(&req).not_ready,
        Some(NotReady::OfflineModelDownloading {
            model: LocalModelId::PaddleOcrVl16
        })
    );
    // 离线翻译模型没下载：划词翻译显示卡片；面板里的请求以错误结束
    let mut settings = configured();
    settings.offline.translate_model = Some(LocalModelId::IndexTranslate9b);
    *env.settings.lock() = Arc::new(settings);
    env.selections.lock().push_back(SelScript {
        delay: Duration::ZERO,
        result: selection("hello there"),
    });
    hotkey(&core, HotkeyAction::TranslateSelection);
    until("translate card", WAIT, || env.prepare_count() == 3).await;
    let (req, session2) = env.last_prepare().expect("prepare");
    assert_eq!(
        translate_payload(&req).not_ready,
        Some(NotReady::OfflineModelMissing {
            model: LocalModelId::IndexTranslate9b
        })
    );
    let _ = session;
    let sink = Collector::new();
    core.translate_start(
        text_req(session2, "hello there", "natural"),
        RequestHost::Panel { session_id: session2 },
        sink.clone(),
    )
    .await
    .expect("start");
    until("error", WAIT, || sink.terminal().is_some()).await;
    assert!(matches!(
        sink.terminal(),
        Some(StreamEvent::Error(e)) if e.code == ErrorCode::NotConfigured
    ));
    assert_eq!(env.llm_count(), 0);
    assert_eq!(env.engine_count(), 0);
}

/// 离线引擎失败：以错误结束，绝不改用云端的视觉模型重试
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_engine_failures_never_fall_back_to_the_cloud() {
    let (env, core) = setup(offline_ocr(LocalModelId::PpOcrV6Tiny));
    env.offline_ready(LocalModelId::PpOcrV6Tiny);
    let mut crashed = LlmError::new(ErrorCode::LocalRuntime, "the inference process exited unexpectedly");
    crashed.hint = Some("errors.hint.local.crashed".to_owned());
    env.engine.lock().fail = Some(crashed.clone());
    let (_, _, sink) = capture_and_wait(&env, &core, HotkeyAction::Ocr).await;
    assert_eq!(sink.terminal(), Some(StreamEvent::Error(crashed)));
    assert_eq!(env.engine_count(), 1);
    assert_eq!(env.llm_count(), 0, "云端的视觉模型（configured() 里有）一次都没被请求");
    assert_eq!(env.count("key_get"), 0);
    // 离线翻译同理：直接翻译的第二步不会因为本机失败改用云端（这里第一步就失败，第二步根本不发）
    let mut settings = offline_ocr(LocalModelId::PpOcrV6Tiny);
    settings.offline.translate_model = Some(LocalModelId::IndexTranslate2b);
    *env.settings.lock() = Arc::new(settings);
    env.offline_ready(LocalModelId::IndexTranslate2b);
    let (_, _, sink) = capture_and_wait(&env, &core, HotkeyAction::OcrTranslate).await;
    assert!(matches!(sink.terminal(), Some(StreamEvent::Error(e)) if e.code == ErrorCode::LocalRuntime));
    assert_eq!(env.llm_count(), 0);
}

/// 离线翻译模型只负责翻译：润色 / 校对明确提示不支持（不悄悄换云端）；提升译文退化为一次普通翻译；
/// 词典式退化为普通翻译
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_translation_rejects_polish_and_simplifies_refine_and_dictionary() {
    let mut settings = configured();
    settings.offline.translate_model = Some(LocalModelId::IndexTranslate2b);
    let (env, core) = setup(settings);
    env.offline_ready(LocalModelId::IndexTranslate2b);
    let host = pinned("p-1");
    for task in [TaskKind::Polish, TaskKind::Proofread] {
        let sink = Collector::new();
        let mut req = text_req(0, "The meeting moved to Thursday.", "natural");
        req.task = task;
        core.translate_start(req, host.clone(), sink.clone())
            .await
            .expect("start");
        until("error", WAIT, || sink.terminal().is_some()).await;
        assert!(
            matches!(sink.terminal(), Some(StreamEvent::Error(e)) if e.code == ErrorCode::NotConfigured),
            "{task:?}: {:?}",
            sink.terminal()
        );
    }
    assert_eq!(env.llm_count(), 0, "润色 / 校对没有发给任何模型");
    // 提升译文：一次普通翻译（没有反思、没有阶段提示）
    let sink = Collector::new();
    let mut req = text_req(0, "The meeting has been moved to Thursday afternoon.", "natural");
    req.task = TaskKind::Refine;
    req.draft = Some("会议改到周四下午。".to_owned());
    core.translate_start(req, host.clone(), sink.clone())
        .await
        .expect("start");
    until("refined", WAIT, || sink.terminal().is_some()).await;
    assert!(matches!(sink.terminal(), Some(StreamEvent::Done { .. })));
    assert!(phases(&sink).is_empty(), "{:?}", phases(&sink));
    let purposes: Vec<Purpose> = env.llm_calls.lock().iter().map(|(_, p, _)| *p).collect();
    assert_eq!(purposes, vec![Purpose::Translate]);
    // 词典式：Meta 里 dict_mode = false
    let sink = Collector::new();
    core.translate_start(text_req(0, "serendipity", "natural"), host, sink.clone())
        .await
        .expect("start");
    until("translated", WAIT, || sink.terminal().is_some()).await;
    let dict = sink.take().iter().find_map(|e| match e {
        StreamEvent::Meta { dict_mode, .. } => *dict_mode,
        _ => None,
    });
    assert_eq!(dict, Some(false));
    assert!(env.llm_providers.lock().iter().all(|p| p == "@offline"));
}

/// PaddleOCR 云端：识别经引擎（编码字节 + Token，按服务的上限编码），排队提示照常转发；直接翻译走两步，第二步用云端
/// 的翻译模型
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paddle_ocr_cloud_recognises_through_the_engine_with_its_token() {
    let mut settings = configured();
    let mut paddle = super::plan::tests::provider("paddle", &["PP-OCRv6"]);
    paddle.protocol = wisp_types::Protocol::PaddleOcr;
    paddle.template_id = Some("paddleocr".to_owned());
    settings.providers.push(paddle);
    settings.defaults.ocr = Some(model_ref("paddle", "PP-OCRv6"));
    let (env, core) = setup(settings);
    env.engine.lock().phase = Some(StreamPhase::Queued);
    let (_, _, sink) = capture_and_wait(&env, &core, HotkeyAction::Ocr).await;
    assert!(matches!(sink.terminal(), Some(StreamEvent::Done { .. })));
    assert_eq!(phases(&sink), vec![StreamPhase::Queued]);
    {
        let calls = env.engine_calls.lock();
        assert!(matches!(&calls[0].0, EngineRef::Cloud { model, .. } if model == "PP-OCRv6"));
        assert!(!calls[0].2, "云端用编码字节");
    }
    assert!(env.count("key_get") >= 1, "Token 经钥匙串读取");
    assert_eq!(env.llm_count(), 0);
    let (_, _, sink) = capture_and_wait(&env, &core, HotkeyAction::OcrTranslate).await;
    assert!(matches!(sink.terminal(), Some(StreamEvent::Done { .. })));
    assert_eq!(*env.llm_providers.lock(), vec!["p".to_owned()]);
}

/// 直接翻译分两步（离线 OCR 只能转录）时，第二步的翻译模型同样先检查：离线翻译模型没下载 / 在下载、或根本没有服务商时，
/// 与 OCR 未就绪一样直接显示卡片（截图前或 commit 时），不先跑一遍识别再给通用错误
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_translate_checks_the_second_step_before_recognising() {
    let mut settings = offline_ocr(LocalModelId::PpOcrV6Tiny);
    settings.offline.translate_model = Some(LocalModelId::IndexTranslate2b);
    let (env, core) = setup(settings);
    env.offline_ready(LocalModelId::PpOcrV6Tiny);
    // 截图之前：翻译模型没下载
    hotkey(&core, HotkeyAction::OcrTranslate);
    until("card", WAIT, || env.prepare_count() == 1).await;
    let (req, _) = env.last_prepare().expect("prepare");
    assert_eq!(
        ocr_payload(&req).not_ready,
        Some(NotReady::OfflineModelMissing {
            model: LocalModelId::IndexTranslate2b
        })
    );
    assert!(!env.called("capture_all"), "没就绪时不截图");
    // commit 时：截图期间翻译模型开始下载
    env.offline_ready(LocalModelId::IndexTranslate2b);
    hotkey(&core, HotkeyAction::OcrTranslate);
    until("overlays", WAIT, || env.called("create_overlays")).await;
    // 面板（上一张卡片）还开着：截图会话的冻结帧稍晚才就绪
    let mut init = None;
    until("overlay init", WAIT, || {
        init = core.overlay_init("overlay-0").ok().flatten();
        init.is_some()
    })
    .await;
    let overlay = init.expect("session").session_id;
    core.overlay_ready(overlay, MONITOR);
    env.offline
        .lock()
        .insert(LocalModelId::IndexTranslate2b, OfflineStatus::Downloading);
    core.overlay_commit(commit_on(overlay, MONITOR)).await.expect("commit");
    core.overlay_done(overlay);
    until("second card", WAIT, || env.prepare_count() == 2).await;
    let (req, _) = env.last_prepare().expect("prepare");
    assert_eq!(
        ocr_payload(&req).not_ready,
        Some(NotReady::OfflineModelDownloading {
            model: LocalModelId::IndexTranslate2b
        })
    );
    assert_eq!(env.engine_count(), 0, "第一步没有开始识别");
    until("overlay gone", WAIT, || env.called("destroy_overlays")).await;
    // 没有任何服务商、只装了离线 OCR：没有能翻译的模型
    let mut settings = Settings::default();
    settings.offline.ocr_model = Some(LocalModelId::PpOcrV6Tiny);
    *env.settings.lock() = Arc::new(settings);
    hotkey(&core, HotkeyAction::OcrTranslate);
    until("third card", WAIT, || env.prepare_count() == 3).await;
    let (req, _) = env.last_prepare().expect("prepare");
    assert_eq!(ocr_payload(&req).not_ready, Some(NotReady::NoProvider));
    assert_eq!(env.engine_count(), 0);
    assert_eq!(env.llm_count(), 0);
    // 只做 OCR（不翻译）不受影响：照常开始截图
    hotkey(&core, HotkeyAction::Ocr);
    until("overlays again", WAIT, || env.count("create_overlays") == 2).await;
}

/// 面板里切到「翻译」重新识别（离线 OCR 只能转录，同样分两步）：第二步的翻译模型没就绪时直接报，不先识别一遍
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn translating_again_in_the_panel_checks_the_second_step_first() {
    let mut settings = offline_ocr(LocalModelId::PpOcrV6Small);
    settings.offline.translate_model = Some(LocalModelId::IndexTranslate2b);
    let (env, core) = setup(settings);
    env.offline_ready(LocalModelId::PpOcrV6Small);
    env.offline_ready(LocalModelId::IndexTranslate2b);
    let (payload, session, sink) = capture_and_wait(&env, &core, HotkeyAction::Ocr).await;
    assert!(matches!(sink.terminal(), Some(StreamEvent::Done { .. })));
    assert_eq!(env.engine_count(), 1);
    // 翻译模型被删掉之后，在面板里要 translate 模式
    env.offline
        .lock()
        .insert(LocalModelId::IndexTranslate2b, OfflineStatus::Missing);
    let again = Collector::new();
    core.ocr_start(
        OcrReq {
            session_id: session,
            capture_id: payload.capture_id.clone(),
            mode: OcrMode::Translate,
            model_ref: None,
            target_lang: None,
            layout: None,
        },
        RequestHost::Panel { session_id: session },
        again.clone(),
    )
    .await
    .expect("translate again");
    until("ended", WAIT, || again.terminal().is_some()).await;
    assert!(
        matches!(
            again.terminal(),
            Some(StreamEvent::Error(ref e)) if e.code == ErrorCode::NotConfigured
        ),
        "{:?}",
        again.terminal()
    );
    assert_eq!(env.engine_count(), 1, "没有先识别一遍");
    assert_eq!(env.llm_count(), 0);
    // 只要原文（paragraphs）不受影响
    let original = Collector::new();
    core.ocr_start(
        OcrReq {
            session_id: session,
            capture_id: payload.capture_id.clone(),
            mode: OcrMode::Paragraphs,
            model_ref: None,
            target_lang: None,
            layout: None,
        },
        RequestHost::Panel { session_id: session },
        original.clone(),
    )
    .await
    .expect("recognise again");
    until("recognised", WAIT, || original.terminal().is_some()).await;
    assert!(matches!(original.terminal(), Some(StreamEvent::Done { .. })));
    assert_eq!(env.engine_count(), 2);
}

/// 交付文本只在逐行 / 段落模式、不是直接翻译的截图时排版；结构化、直接翻译（包括它的「查看原文」）原样交付。
/// 合并段内换行只在逐行模式：段落模式重新识别时会话排版仍是它，交付的是原样
#[test]
fn delivered_text_lays_out_only_lines_and_paragraphs_outside_direct_translation() {
    let raw = "第一段\n\n第二段";
    let deliver = |mode, direct, layout| delivered_text(raw.to_owned(), mode, direct, layout);
    assert_eq!(
        deliver(OcrMode::Lines, false, OcrLayout::RemoveBlankLines),
        "第一段\n第二段"
    );
    assert_eq!(
        deliver(OcrMode::Paragraphs, false, OcrLayout::JoinLines),
        "第一段第二段"
    );
    assert_eq!(deliver(OcrMode::Lines, false, OcrLayout::Keep), raw);
    let wrapped = "Wisp stays in the background and wakes up only\nwhen you press a hotkey.\n\nSecond paragraph.";
    let reflowed = "Wisp stays in the background and wakes up only when you press a hotkey.\n\nSecond paragraph.";
    assert_eq!(
        delivered_text(wrapped.to_owned(), OcrMode::Lines, false, OcrLayout::Reflow),
        reflowed
    );
    for mode in [OcrMode::Paragraphs, OcrMode::Markdown] {
        assert_eq!(
            delivered_text(wrapped.to_owned(), mode, false, OcrLayout::Reflow),
            wrapped,
            "{mode:?} 下合并段内换行不可用"
        );
    }
    for layout in OcrLayout::ALL {
        assert_eq!(deliver(OcrMode::Markdown, false, layout), raw, "{layout:?}");
        assert_eq!(deliver(OcrMode::Translate, true, layout), raw, "{layout:?}");
        assert_eq!(
            deliver(OcrMode::Paragraphs, true, layout),
            raw,
            "直接翻译的「查看原文」：{layout:?}"
        );
    }
}

/// 识别完成时的交付：自动复制与识别历史是同一份 `layout(R)`（R 是流式结果的全文，视图累积的就是它，切换排版时交给
/// `text_transform` 的也是它）；重新识别时视图带回的排版优先于设置，旧前端不带时按设置；结构化不排版
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ocr_delivers_the_session_layout_to_the_clipboard_and_history() {
    let mut settings = configured();
    settings.ocr.layout = OcrLayout::RemoveBlankLines;
    settings.ocr.history = true;
    let (env, core) = setup(settings);
    let raw = "第一段\n\n第二段\n第三行";
    env.llm_outputs.lock().insert("ocr", raw.to_owned());
    let (payload, session, sink) = capture_and_wait(&env, &core, HotkeyAction::Ocr).await;
    assert_eq!(payload.layout, OcrLayout::RemoveBlankLines, "会话排版的初值取设置");
    assert_eq!(sink.text(), raw, "流式显示的是识别文本本身");
    let laid = layout_text(raw.to_owned(), OcrLayout::RemoveBlankLines);
    assert_eq!(laid, "第一段\n第二段\n第三行");
    until("auto copy", WAIT, || env.copies.lock().len() == 1).await;
    until("recorded", WAIT, || env.ocr_history.lock().len() == 1).await;
    assert_eq!(env.copies.lock()[0], laid);
    assert_eq!(env.ocr_history.lock()[0].text, laid);
    let crop = core.captures.crop(&payload.capture_id).expect("crop");
    until("receipt", WAIT, || crop.copied().is_some()).await;

    let rerun = |mode: OcrMode, layout: Option<OcrLayout>| {
        let core = Arc::clone(&core);
        let capture_id = payload.capture_id.clone();
        async move {
            let sink = Collector::new();
            core.ocr_start(
                OcrReq {
                    session_id: session,
                    capture_id,
                    mode,
                    model_ref: None,
                    target_lang: None,
                    layout,
                },
                RequestHost::Panel { session_id: session },
                sink.clone(),
            )
            .await
            .expect("ocr_start");
            until("re-recognised", WAIT, || sink.terminal().is_some()).await;
            assert_eq!(sink.text(), raw);
        }
    };
    // 视图带回会话里的排版：合并为一行不能设为默认，但会话里可以选
    rerun(OcrMode::Lines, Some(OcrLayout::JoinLines)).await;
    until("copied again", WAIT, || env.copies.lock().len() == 2).await;
    until("recorded again", WAIT, || env.ocr_history.lock().len() == 2).await;
    let joined = layout_text(raw.to_owned(), OcrLayout::JoinLines);
    assert_eq!(joined, "第一段第二段第三行");
    assert_eq!(env.copies.lock()[1], joined);
    assert_eq!(env.ocr_history.lock()[1].text, joined);
    // 旧前端不带排版：按设置
    rerun(OcrMode::Paragraphs, None).await;
    until("copied a third time", WAIT, || env.copies.lock().len() == 3).await;
    assert_eq!(env.copies.lock()[2], laid);
    // 结构化：空行是 Markdown 语法，不排版
    rerun(OcrMode::Markdown, Some(OcrLayout::RemoveBlankLines)).await;
    until("markdown copied", WAIT, || env.copies.lock().len() == 4).await;
    until("markdown recorded", WAIT, || env.ocr_history.lock().len() == 4).await;
    assert_eq!(env.copies.lock()[3], raw);
    assert_eq!(env.ocr_history.lock()[3].text, raw);
}

/// 切换排版后剪贴板跟随（`ocr_recopy`）：剪贴板仍是这张截图上次自动复制的内容时写入，替换上一次新建的历史条目、
/// 换上新的回执；期间别的程序复制过东西就不再跟随
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ocr_recopy_follows_only_while_the_clipboard_still_holds_the_auto_copy() {
    let (env, core) = setup(configured());
    env.llm_outputs.lock().insert("ocr", "第一段\n\n第二段".to_owned());
    let (payload, _, _) = capture_and_wait(&env, &core, HotkeyAction::Ocr).await;
    let cap = payload.capture_id.clone();
    let crop = core.captures.crop(&cap).expect("crop");
    until("auto copy", WAIT, || crop.copied().is_some()).await;
    let first = crop.copied().expect("receipt");
    assert_eq!(first.created, Some(fake_item(1)));
    let rid = payload.request_id.clone().expect("request");
    let recopy = |text: &str| core.ocr_recopy(&RequestCaller::Panel, &cap, &rid, text.to_owned());

    // 切换为「去掉空行」：写入，替换第 1 条
    assert!(recopy("第一段\n第二段").await.expect("recopy"));
    assert_eq!(env.copies.lock().last().map(String::as_str), Some("第一段\n第二段"));
    assert_eq!(*env.clip_removed.lock(), [1]);
    let second = crop.copied().expect("receipt");
    assert_eq!(second.created, Some(fake_item(2)));
    assert_ne!(second.change_id, first.change_id);
    // 切回原样：替换的是第 2 条，历史里始终只留一条
    assert!(recopy("第一段\n\n第二段").await.expect("recopy"));
    assert_eq!(*env.clip_removed.lock(), [1, 2]);

    // 别的程序复制了东西：剪贴板与历史都不动，回执也不变
    env.clip_change.fetch_add(1, Ordering::SeqCst);
    let copies = env.copies.lock().len();
    let receipt = crop.copied();
    assert!(!recopy("第一段\n第二段").await.expect("recopy"));
    assert_eq!(env.copies.lock().len(), copies);
    assert_eq!(env.clip_removed.lock().len(), 2);
    assert_eq!(crop.copied(), receipt);
    // 不存在的截图：与条件不满足一样
    assert!(
        !core
            .ocr_recopy(&RequestCaller::Panel, "c-404", &rid, "x".to_owned())
            .await
            .expect("unknown capture")
    );
}

/// 自动复制关着、截图已经固定到窗口里、直接翻译的截图：切换排版都不写剪贴板
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ocr_recopy_never_writes_without_auto_copy_from_pinned_windows_or_for_direct_translation() {
    // 自动复制关着：识别完不复制，切换也不写
    let mut settings = configured();
    settings.ocr.auto_copy = false;
    let (env, core) = setup(settings);
    let (payload, _, _) = capture_and_wait(&env, &core, HotkeyAction::Ocr).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    let rid = payload.request_id.clone().expect("request");
    assert!(
        !core
            .ocr_recopy(&RequestCaller::Panel, &payload.capture_id, &rid, "x".to_owned())
            .await
            .expect("recopy")
    );
    assert!(env.copies.lock().is_empty());

    // 识别完成、自动复制之后才关掉：同样不写
    let (env, core) = setup(configured());
    let (payload, _, _) = capture_and_wait(&env, &core, HotkeyAction::Ocr).await;
    let cap = payload.capture_id.clone();
    let crop = core.captures.crop(&cap).expect("crop");
    until("auto copy", WAIT, || crop.copied().is_some()).await;
    let rid = payload.request_id.clone().expect("request");
    let mut off = (*env.settings()).clone();
    off.ocr.auto_copy = false;
    *env.settings.lock() = Arc::new(off);
    assert!(
        !core
            .ocr_recopy(&RequestCaller::Panel, &cap, &rid, "x".to_owned())
            .await
            .expect("recopy")
    );
    *env.settings.lock() = Arc::new(configured());

    // 固定到窗口之后：面板与固定窗口都不能带动剪贴板
    core.transfer_to_pinned(payload.request_id.as_deref().expect("request"), "p1");
    let p1 = RequestCaller::Pinned {
        pinned_id: "p1".to_owned(),
    };
    for caller in [RequestCaller::Panel, p1] {
        assert!(
            !core
                .ocr_recopy(&caller, &cap, &rid, "x".to_owned())
                .await
                .expect("recopy"),
            "{caller:?}"
        );
    }
    assert_eq!(env.copies.lock().len(), 1, "只有识别完成时的那一次");
    assert!(env.clip_removed.lock().is_empty());

    // 直接翻译：会话排版恒为原样，不自动复制，也不跟随
    let mut settings = configured();
    settings.ocr.layout = OcrLayout::RemoveBlankLines;
    let (env, core) = setup(settings);
    let (payload, _, _) = capture_and_wait(&env, &core, HotkeyAction::OcrTranslate).await;
    assert_eq!(payload.mode, OcrMode::Translate);
    assert_eq!(payload.layout, OcrLayout::Keep);
    let rid = payload.request_id.clone().expect("request");
    assert!(
        !core
            .ocr_recopy(&RequestCaller::Panel, &payload.capture_id, &rid, "x".to_owned())
            .await
            .expect("recopy")
    );
    assert!(env.copies.lock().is_empty());
}

/// 在面板里重新识别同一张截图（`ocr_start`），返回请求 id
async fn re_recognise(
    core: &Arc<Core>,
    session: u64,
    capture_id: &str,
    sink: Arc<dyn super::registry::EventSink>,
) -> String {
    core.ocr_start(
        OcrReq {
            session_id: session,
            capture_id: capture_id.to_owned(),
            mode: OcrMode::Lines,
            model_ref: None,
            target_lang: None,
            layout: None,
        },
        RequestHost::Panel { session_id: session },
        sink,
    )
    .await
    .expect("ocr_start")
    .request_id
}

/// 跟随只作用于当前这次识别自动复制的那一条：重新识别进行中时，上一次结果的切换不再跟随；重新识别被取消后，半截文字
/// 不写进剪贴板，上一次完整结果新建的历史条目也不删
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ocr_recopy_follows_only_the_current_recognition() {
    let (env, core) = setup(configured());
    env.llm_outputs.lock().insert("ocr", "第一段\n\n第二段".to_owned());
    let (payload, session, _) = capture_and_wait(&env, &core, HotkeyAction::Ocr).await;
    let cap = payload.capture_id.clone();
    let crop = core.captures.crop(&cap).expect("crop");
    until("auto copy", WAIT, || crop.copied().is_some()).await;
    let first = payload.request_id.clone().expect("request");

    // 重新识别：文字长、流得慢（约 2 s），中途取消
    env.llm_outputs.lock().insert("ocr", "第一段\n\n第二段".repeat(20));
    *env.llm_pace.lock() = (Duration::from_millis(5), Duration::from_millis(40));
    let sink = Collector::new();
    let again = re_recognise(&core, session, &cap, sink.clone()).await;
    until("partial text", WAIT, || !sink.text().is_empty()).await;
    assert!(
        !core
            .ocr_recopy(&RequestCaller::Panel, &cap, &first, "第一段\n第二段".to_owned())
            .await
            .expect("recopy"),
        "重新识别进行中：上一次结果的切换不再跟随"
    );
    core.cancel(&RequestCaller::Panel, &again);
    until("cancelled", WAIT, || sink.terminal().is_some()).await;
    assert!(is_cancelled(&sink.terminal()));
    assert!(
        !core
            .ocr_recopy(&RequestCaller::Panel, &cap, &again, sink.text())
            .await
            .expect("recopy"),
        "被取消的半截文字不跟随"
    );
    assert!(
        !core
            .ocr_recopy(&RequestCaller::Panel, &cap, &first, "第一段\n第二段".to_owned())
            .await
            .expect("recopy"),
        "上一次结果的回执已随重新识别作废"
    );
    assert_eq!(env.copies.lock().len(), 1, "只有第一次识别的自动复制");
    assert!(env.clip_removed.lock().is_empty(), "上一次完整结果的历史条目保留");
}

/// 重新识别时自动复制失败（Windows 剪贴板被占着）：没有这次的回执，切换排版不跟随，也不删上一次的条目
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ocr_recopy_does_nothing_when_this_recognitions_auto_copy_failed() {
    let (env, core) = setup(configured());
    env.llm_outputs.lock().insert("ocr", "第一段\n\n第二段".to_owned());
    let (payload, session, _) = capture_and_wait(&env, &core, HotkeyAction::Ocr).await;
    let cap = payload.capture_id.clone();
    let crop = core.captures.crop(&cap).expect("crop");
    until("auto copy", WAIT, || crop.copied().is_some()).await;

    *env.copy_fails.lock() = true;
    let sink = Collector::new();
    let again = re_recognise(&core, session, &cap, sink.clone()).await;
    until("re-recognised", WAIT, || sink.terminal().is_some()).await;
    assert!(matches!(sink.terminal(), Some(StreamEvent::Done { .. })));
    *env.copy_fails.lock() = false;
    let first = payload.request_id.clone().expect("request");
    for request in [&again, &first] {
        assert!(
            !core
                .ocr_recopy(&RequestCaller::Panel, &cap, request, "第一段\n第二段".to_owned())
                .await
                .expect("recopy"),
            "{request}"
        );
    }
    assert_eq!(env.copies.lock().len(), 1);
    assert!(env.clip_removed.lock().is_empty());
}

/// 收到 Done 时就让「前端」切换排版：另起任务调用跟随，并把 Done 的投递拖住一会儿，使这次跟随先于自动复制到达
struct FollowOnDone {
    events: Arc<Collector>,
    on_done: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl super::registry::EventSink for FollowOnDone {
    fn deliver(&self, ev: &StreamEvent) -> bool {
        let ok = self.events.deliver(ev);
        if matches!(ev, StreamEvent::Done { .. })
            && let Some(follow) = self.on_done.lock().take()
        {
            follow();
            std::thread::sleep(Duration::from_millis(150));
        }
        ok
    }
}

/// Done 已经到了前端、自动复制还没写入时切换排版：跟随等这次自动复制完成后再进行，替换的是这次新建的条目，剪贴板最后
/// 是切换后的文字（重新识别的自动复制先取代了上一次识别的条目）
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ocr_recopy_waits_for_the_auto_copy_of_the_result_it_follows() {
    let (env, core) = setup(configured());
    env.llm_outputs.lock().insert("ocr", "第一段\n\n第二段".to_owned());
    let (payload, session, _) = capture_and_wait(&env, &core, HotkeyAction::Ocr).await;
    let cap = payload.capture_id.clone();
    let crop = core.captures.crop(&cap).expect("crop");
    until("auto copy", WAIT, || crop.copied().is_some()).await;

    *env.llm_pace.lock() = (Duration::from_millis(80), Duration::from_millis(2));
    env.llm_outputs.lock().insert("ocr", "第三段\n\n第四段".to_owned());
    let sink = Arc::new(FollowOnDone {
        events: Collector::new(),
        on_done: Mutex::new(None),
    });
    let again = re_recognise(&core, session, &cap, sink.clone()).await;
    let (tx, rx) = tokio::sync::oneshot::channel();
    {
        let core = Arc::clone(&core);
        let cap = cap.clone();
        // 另起线程：在投递 Done 的工作线程上 spawn 的任务进的是它自己的 LIFO 槽，要等这次投递返回才会被执行
        *sink.on_done.lock() = Some(Box::new(move || {
            std::thread::spawn(move || {
                let copied = tauri::async_runtime::block_on(core.ocr_recopy(
                    &RequestCaller::Panel,
                    &cap,
                    &again,
                    "第三段\n第四段".to_owned(),
                ));
                let _ = tx.send(copied);
            });
        }));
    }
    let copied = tokio::time::timeout(WAIT, rx).await.expect("follow").expect("sent");
    assert!(copied.expect("recopy"), "这次自动复制之后剪贴板没变：跟随");
    assert_eq!(
        *env.copies.lock(),
        ["第一段\n\n第二段", "第三段\n\n第四段", "第三段\n第四段"],
        "先是这次识别的自动复制，再是切换后的文字"
    );
    assert_eq!(
        *env.clip_removed.lock(),
        [1, 2],
        "自动复制取代上一次识别的条目，跟随再替换这次自动复制新建的那一条"
    );
    assert_eq!(env.clip_ids(), [3]);
}

/// 在面板里重新识别、等它结束，并等这次的自动复制交付完（回执写下）
async fn re_recognise_and_copy(env: &Arc<FakeEnv>, core: &Arc<Core>, session: u64, cap: &str, text: &str) -> String {
    env.llm_outputs.lock().insert("ocr", text.to_owned());
    let copies = env.copies.lock().len();
    let sink = Collector::new();
    let id = re_recognise(core, session, cap, sink.clone()).await;
    until("re-recognised", WAIT, || sink.terminal().is_some()).await;
    assert!(matches!(sink.terminal(), Some(StreamEvent::Done { .. })));
    let crop = core.captures.crop(cap).expect("crop");
    until("auto copy", WAIT, || {
        env.copies.lock().len() == copies + 1 && crop.copied().is_some()
    })
    .await;
    id
}

/// 重新识别同一张截图（⌘R、换模式 / 模型）：这次的自动复制取代上一次识别新建的历史条目，历史里只留最新的一条；上一次
/// 写剪贴板的是切换排版的跟随时，取代的是跟随写入的那一条
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn re_recognising_replaces_the_previous_auto_copy_in_the_history() {
    let (env, core) = setup(configured());
    env.llm_outputs.lock().insert("ocr", "第一段\n\n第二段".to_owned());
    let (payload, session, _) = capture_and_wait(&env, &core, HotkeyAction::Ocr).await;
    let cap = payload.capture_id.clone();
    let crop = core.captures.crop(&cap).expect("crop");
    until("auto copy", WAIT, || crop.copied().is_some()).await;
    assert_eq!(env.clip_ids(), [1]);

    let again = re_recognise_and_copy(&env, &core, session, &cap, "第三段").await;
    assert_eq!(*env.clip_removed.lock(), [1], "剪贴板仍是上一次的自动复制：取代它");
    assert_eq!(env.clip_ids(), [2]);
    assert_eq!(crop.copied().and_then(|r| r.created), Some(fake_item(2)));
    // 切换排版的跟随替换这次的条目；之后再重新识别，取代的是跟随写入的那一条
    assert!(
        core.ocr_recopy(&RequestCaller::Panel, &cap, &again, "第三段。".to_owned())
            .await
            .expect("recopy")
    );
    assert_eq!(env.clip_ids(), [3]);
    re_recognise_and_copy(&env, &core, session, &cap, "第四段").await;
    assert_eq!(*env.clip_removed.lock(), [1, 2, 3]);
    assert_eq!(env.clip_ids(), [4]);
    assert_eq!(*env.copies.lock(), ["第一段\n\n第二段", "第三段", "第三段。", "第四段"]);
}

/// 上一次识别的条目收藏过（命名即收藏）：重新识别不删它，只是不再记着它，之后的重新识别取代的是新的这一条
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn re_recognising_keeps_a_favorited_entry() {
    let (env, core) = setup(configured());
    env.llm_outputs.lock().insert("ocr", "第一段".to_owned());
    let (payload, session, _) = capture_and_wait(&env, &core, HotkeyAction::Ocr).await;
    let cap = payload.capture_id.clone();
    let crop = core.captures.crop(&cap).expect("crop");
    until("auto copy", WAIT, || crop.copied().is_some()).await;
    env.clip_pinned.lock().insert(1);

    re_recognise_and_copy(&env, &core, session, &cap, "第二段").await;
    assert!(env.clip_removed.lock().is_empty(), "收藏过的保留");
    assert_eq!(env.clip_ids(), [1, 2]);
    assert_eq!(crop.copied().and_then(|r| r.created), Some(fake_item(2)));
    re_recognise_and_copy(&env, &core, session, &cap, "第三段").await;
    assert_eq!(*env.clip_removed.lock(), [2]);
    assert_eq!(env.clip_ids(), [1, 3]);
}

/// 上一次自动复制之后用户在别处复制过东西：剪贴板已不是那次写入的内容，上一次的条目成了普通的历史，重新识别不删它，
/// 之后也不再替换它
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn re_recognising_after_another_copy_keeps_the_previous_entry() {
    let (env, core) = setup(configured());
    env.llm_outputs.lock().insert("ocr", "第一段".to_owned());
    let (payload, session, _) = capture_and_wait(&env, &core, HotkeyAction::Ocr).await;
    let cap = payload.capture_id.clone();
    let crop = core.captures.crop(&cap).expect("crop");
    until("auto copy", WAIT, || crop.copied().is_some()).await;

    env.clip_change.fetch_add(1, Ordering::SeqCst);
    re_recognise_and_copy(&env, &core, session, &cap, "第二段").await;
    assert!(env.clip_removed.lock().is_empty(), "期间别处复制过：不删");
    assert_eq!(env.clip_ids(), [1, 2]);
    // 剪贴板此后没再变：再重新识别只取代这次的条目，第一条不动
    re_recognise_and_copy(&env, &core, session, &cap, "第三段").await;
    assert_eq!(*env.clip_removed.lock(), [2]);
    assert_eq!(env.clip_ids(), [1, 3]);
}

/// 重新识别得到完全相同的文字：自动复制只把上一次新建的那一条移到顶部，它继续记在这次的回执上，之后切换排版仍能替换它
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn re_recognising_the_same_text_keeps_the_entry_for_the_next_layout_switch() {
    let (env, core) = setup(configured());
    env.llm_outputs.lock().insert("ocr", "第一段\n\n第二段".to_owned());
    let (payload, session, _) = capture_and_wait(&env, &core, HotkeyAction::Ocr).await;
    let cap = payload.capture_id.clone();
    let crop = core.captures.crop(&cap).expect("crop");
    until("auto copy", WAIT, || crop.copied().is_some()).await;
    let first = crop.copied().expect("receipt");

    let again = re_recognise_and_copy(&env, &core, session, &cap, "第一段\n\n第二段").await;
    let receipt = crop.copied().expect("receipt");
    assert_eq!(receipt.created, Some(fake_item(1)), "接续上一次新建的那一条");
    assert_ne!(receipt.change_id, first.change_id);
    assert!(env.clip_removed.lock().is_empty());
    assert_eq!(env.clip_ids(), [1]);
    assert!(
        core.ocr_recopy(&RequestCaller::Panel, &cap, &again, "第一段\n第二段".to_owned())
            .await
            .expect("recopy")
    );
    assert_eq!(*env.clip_removed.lock(), [1], "切换排版替换的是接续下来的那一条");
    assert_eq!(env.clip_ids(), [2]);
}

/// 被取消的重新识别不写剪贴板、不删上一次的条目，也不丢掉上一次写入的回执：之后完成的重新识别照样取代上一次的条目
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_re_recognition_leaves_the_previous_entry_alone() {
    let (env, core) = setup(configured());
    env.llm_outputs.lock().insert("ocr", "第一段\n\n第二段".to_owned());
    let (payload, session, _) = capture_and_wait(&env, &core, HotkeyAction::Ocr).await;
    let cap = payload.capture_id.clone();
    let crop = core.captures.crop(&cap).expect("crop");
    until("auto copy", WAIT, || crop.copied().is_some()).await;
    let first = crop.copied().expect("receipt");

    // 文字长、流得慢（约 2 s），中途取消
    env.llm_outputs.lock().insert("ocr", "第三段\n\n第四段".repeat(20));
    *env.llm_pace.lock() = (Duration::from_millis(5), Duration::from_millis(40));
    let sink = Collector::new();
    let cancelled = re_recognise(&core, session, &cap, sink.clone()).await;
    until("partial text", WAIT, || !sink.text().is_empty()).await;
    core.cancel(&RequestCaller::Panel, &cancelled);
    until("cancelled", WAIT, || sink.terminal().is_some()).await;
    assert!(is_cancelled(&sink.terminal()));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(env.copies.lock().len(), 1, "被取消的识别不自动复制");
    assert!(env.clip_removed.lock().is_empty());
    assert_eq!(env.clip_ids(), [1]);
    assert_eq!(crop.last_copy(), Some(first), "上一次写入的回执留着");

    *env.llm_pace.lock() = (Duration::from_millis(5), Duration::from_millis(2));
    re_recognise_and_copy(&env, &core, session, &cap, "第三段").await;
    assert_eq!(*env.clip_removed.lock(), [1]);
    assert_eq!(env.clip_ids(), [2]);
}
