//! 剪贴板服务的单测：mock 的平台（剪贴板 / 粘贴 / 焦点 / 应用图标）与宿主（面板、事件、托盘、toast、state.json），
//! 真实的 `wisp_clipboard::ClipStore`（临时目录）。后台工作默认同步执行（断言是确定的）；需要覆盖时序的用例把它切到
//! 「排队」模式，由测试决定后台任务与延时任务何时执行（[`TestExec`]）。
//!
//! 覆盖：记录管线（插入、去重、自身写入、隐私标记、疑似密钥、排除名单、暂停、读取失败重试）、粘贴的每个分支与顺序、
//! copy_text、置顶 / 删除 / 清空与缓存的一致、空闲清理、首页不查库、数据库来自更新版本、缩略图与图标的 `updated`、
//! 文件条目、`wisp://` 资源。状态机、时序与后台任务的接线见 `tests_flow.rs`。

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use wisp_clipboard::{BlobKind, ClipStore};
use wisp_platform::{
    AppInfo, ClipSnapshot, ClipboardAccess, FocusTracker, HookHandle, Paste, PasteError, PlatformError, ReadLimits,
    SuppressRange, WriteOpts,
};
use wisp_types::{
    AppRef, Capability, ClipFilter, ClipItemMeta, ClipKind, ClipPayload, ClipboardChanged, CopiedReason,
    OcrHistoryChanged, OcrMode, OsKind, PanelView, PasteMode, PasteOutcome, RawClipboard, Settings, ToastPayload,
};

use crate::assets::AssetRegistry;
use crate::error::{IpcError, IpcErrorCode};
use crate::windows::panel::PANEL_LABEL;

use super::engine::Engine;
use super::paste::{CopyReceipt, PasteHost, PasteRequest, PasteSource};
use super::port::{ClipView, Exec, Host, OffscreenWait, Port, TargetCheck};
use super::retention::{CleanupReport, Plan, QUIET, plan};
use super::watch::{Capture, process_with};

pub(super) const ICON_32: &[u8] = include_bytes!("../../icons/32x32.png");
pub(super) const ICON_64: &[u8] = include_bytes!("../../icons/64x64.png");
pub(super) const SELF_ID: &str = "io.github.yzlltyyh.wisp";

// ---------- mock ----------

/// 跨 mock 的调用顺序
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Call {
    Write,
    Suppress(u64),
    Activate(String),
    Hide,
    /// 粘贴流程在等离屏时收到了这个会话的离屏通知（[`Offscreen::Arrives`]）
    Offscreen,
    /// 粘贴流程等离屏等到了上限（[`Offscreen::Never`]；值是它传入的上限）
    OffscreenTimeout(Duration),
    PasteInto {
        plain: bool,
    },
    /// 悬浮剪贴板交还键盘焦点（值是目标应用 id）
    FloatYield(String),
    /// 连续粘贴：装上 / 卸下拦下粘贴键的钩子、重放一次粘贴键（值是主键名）
    HookInstall,
    HookRemove,
    Replay(&'static str),
    /// 连续粘贴期间的全局 Esc
    EscGuard(bool),
    Toast(CopiedReason),
    Perf(String),
}

pub(super) type Log = Arc<Mutex<Vec<Call>>>;

/// 在 mock 内部某个时刻执行一次的钩子（模拟「粘贴进行中」发生的事）
pub(super) type Hook = Mutex<Option<Box<dyn FnOnce() + Send>>>;

fn run_hook(hook: &Hook) {
    let f = hook.lock().take();
    if let Some(f) = f {
        f();
    }
}

pub(super) struct MockClipboard {
    log: Log,
    pub(super) change: Arc<AtomicU64>,
    pub(super) reads: Mutex<VecDeque<Result<RawClipboard, PlatformError>>>,
    pub(super) read_calls: AtomicUsize,
    pub(super) watching: Arc<AtomicBool>,
    pub(super) installs: AtomicUsize,
    /// `poll_now` 的调用次数（剪贴板面板即将显示时让轮询立即读一次）
    pub(super) polls: AtomicUsize,
}

impl ClipboardAccess for MockClipboard {
    fn change_id(&self) -> u64 {
        self.change.load(Ordering::SeqCst)
    }

    fn read(&self, _limits: &ReadLimits) -> Result<RawClipboard, PlatformError> {
        self.read_calls.fetch_add(1, Ordering::SeqCst);
        self.reads
            .lock()
            .pop_front()
            .unwrap_or_else(|| Err(PlatformError::Other("nothing queued".into())))
    }

    fn snapshot(&self, _max: usize, _deadline: Duration) -> Result<Option<ClipSnapshot>, PlatformError> {
        Ok(None)
    }

    fn restore(&self, _snap: ClipSnapshot) -> Result<u64, PlatformError> {
        Ok(0)
    }

    fn watch(&self, _cb: Box<dyn Fn(u64) + Send + Sync>) -> Result<HookHandle, PlatformError> {
        self.installs.fetch_add(1, Ordering::SeqCst);
        self.watching.store(true, Ordering::SeqCst);
        let flag = Arc::clone(&self.watching);
        Ok(HookHandle::new(move || flag.store(false, Ordering::SeqCst)))
    }

    fn suppress(&self, range: SuppressRange) {
        self.log.lock().push(Call::Suppress(range.from_change_id));
    }

    fn poll_now(&self) {
        self.polls.fetch_add(1, Ordering::SeqCst);
    }
}

pub(super) struct MockPaste {
    log: Log,
    change: Arc<AtomicU64>,
    pub(super) writes: Mutex<Vec<ClipPayload>>,
    pub(super) write_error: Mutex<Option<PlatformError>>,
    pub(super) paste_result: Mutex<Result<(), PasteError>>,
    /// 写入进行中执行一次（模拟 Windows 写剪贴板期间用户的操作）
    pub(super) during_write: Hook,
}

impl Paste for MockPaste {
    fn write(&self, payload: &ClipPayload, opts: WriteOpts) -> Result<u64, PlatformError> {
        assert_eq!(opts, WriteOpts::default(), "入库 / touch 语义的写入用默认 WriteOpts");
        run_hook(&self.during_write);
        if let Some(e) = self.write_error.lock().take() {
            return Err(e);
        }
        self.log.lock().push(Call::Write);
        self.writes.lock().push(payload.clone());
        Ok(self.change.fetch_add(1, Ordering::SeqCst) + 1)
    }

    fn paste_into(&self, _target: &AppRef, plain: bool) -> Result<(), PasteError> {
        self.log.lock().push(Call::PasteInto { plain });
        self.paste_result.lock().clone()
    }
}

pub(super) struct MockFocus {
    log: Log,
    pub(super) activate_result: Mutex<Result<(), PlatformError>>,
    /// 当前的前台应用（悬浮窗与队列的粘贴目标）
    pub(super) frontmost: Mutex<Option<AppRef>>,
}

impl FocusTracker for MockFocus {
    fn frontmost(&self) -> Option<AppRef> {
        self.frontmost.lock().clone()
    }

    fn activate(&self, app: &AppRef) -> Result<(), PlatformError> {
        self.log.lock().push(Call::Activate(app.id.clone()));
        self.activate_result.lock().clone()
    }

    fn is_elevated(&self, _app: &AppRef) -> bool {
        false
    }
}

pub(super) struct MockApps {
    pub(super) icon: AtomicBool,
    pub(super) calls: AtomicUsize,
    /// `user_apps` 的结果（排除名单选择器）
    pub(super) running: Mutex<Vec<AppRef>>,
}

impl AppInfo for MockApps {
    fn icon_png(&self, _app: &AppRef, _size_px: u32) -> Option<Vec<u8>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.icon.load(Ordering::SeqCst).then(|| ICON_32.to_vec())
    }

    fn display_name(&self, app: &AppRef) -> String {
        app.name.clone()
    }

    fn running_apps(&self) -> Vec<AppRef> {
        Vec::new()
    }

    fn user_apps(&self) -> Vec<AppRef> {
        self.running.lock().clone()
    }
}

/// 钩子装上之后平台层的信号出口
type SignalSink = Arc<dyn Fn(wisp_platform::InputSignal) + Send + Sync>;

/// 连续粘贴的钩子：记下装卸与重放；`sink` 是装上之后平台层的信号出口（测试经 [`MockHooks::press`] 模拟按键）
pub(super) struct MockHooks {
    log: Log,
    pub(super) sink: Arc<Mutex<Option<SignalSink>>>,
    pub(super) keys: Mutex<Option<wisp_platform::PasteKeys>>,
    pub(super) install_error: Mutex<Option<wisp_platform::HookError>>,
    pub(super) replay_error: Mutex<Option<wisp_platform::HookError>>,
    pub(super) unavailable: Mutex<Option<String>>,
}

impl MockHooks {
    /// 模拟平台层拦下一次粘贴键（钩子没装时什么都不发生：按键照常交给应用）
    pub(super) fn press(&self, chord: wisp_platform::KeyChord) -> bool {
        let sink = self.sink.lock().clone();
        match sink {
            Some(sink) => {
                sink(wisp_platform::InputSignal::PasteKey { chord });
                true
            }
            None => false,
        }
    }

    pub(super) fn signal(&self, sig: wisp_platform::InputSignal) {
        let sink = self.sink.lock().clone();
        if let Some(sink) = sink {
            sink(sig);
        }
    }

    pub(super) fn installed(&self) -> bool {
        self.sink.lock().is_some()
    }
}

impl super::port::PasteKeyHooks for MockHooks {
    fn install(
        &self,
        keys: wisp_platform::PasteKeys,
        sink: Box<dyn Fn(wisp_platform::InputSignal) + Send + Sync>,
    ) -> Result<HookHandle, wisp_platform::HookError> {
        if let Some(e) = self.install_error.lock().clone() {
            return Err(e);
        }
        self.log.lock().push(Call::HookInstall);
        *self.keys.lock() = Some(keys);
        *self.sink.lock() = Some(Arc::from(sink));
        let log = Arc::clone(&self.log);
        let slot = Arc::clone(&self.sink);
        Ok(HookHandle::new(move || {
            slot.lock().take();
            log.lock().push(Call::HookRemove);
        }))
    }

    fn replay(&self, chord: wisp_platform::KeyChord) -> Result<(), wisp_platform::HookError> {
        self.log.lock().push(Call::Replay(chord.code));
        self.replay_error.lock().clone().map_or(Ok(()), Err)
    }

    fn unavailable(&self) -> Option<String> {
        self.unavailable.lock().clone()
    }
}

pub(super) struct MockPort {
    pub(super) clipboard: MockClipboard,
    pub(super) paste: MockPaste,
    pub(super) focus: MockFocus,
    pub(super) apps: MockApps,
    pub(super) hooks: MockHooks,
    /// Windows 预激活前的检查结果
    pub(super) target_check: Mutex<TargetCheck>,
    /// 平台报告的「模拟粘贴键」能力（默认可用）
    pub(super) paste_inject: Mutex<Capability>,
}

impl Port for MockPort {
    fn clipboard(&self) -> &dyn ClipboardAccess {
        &self.clipboard
    }

    fn paste(&self) -> &dyn Paste {
        &self.paste
    }

    fn focus(&self) -> &dyn FocusTracker {
        &self.focus
    }

    fn apps(&self) -> &dyn AppInfo {
        &self.apps
    }

    fn target_check(&self, _target: &AppRef) -> TargetCheck {
        *self.target_check.lock()
    }

    fn paste_inject(&self) -> Capability {
        self.paste_inject.lock().clone()
    }

    fn paste_hooks(&self) -> &dyn super::port::PasteKeyHooks {
        &self.hooks
    }
}

/// `hide_for_paste` 之后的离屏通知。mock 的等待端在**被调用的那一刻**按它给出结果（不睡、不起线程）：「等了没有、上限多少」
/// 都记进调用序列，断言是确定的，与机器快慢无关（按墙钟判定的话，负载高的机器上会偶发失败）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Offscreen {
    /// 拿不到通知通道（不等）
    NoChannel,
    /// 等待时离屏通知已经到了：记 [`Call::Offscreen`]、发出这个会话的离屏通知（`panel_hidden` 的订阅方收到），等待返回 true
    Arrives,
    /// 一直没有离屏通知：等待按上限超时——记 [`Call::OffscreenTimeout`]（带上传入的上限），立即返回 false
    Never,
}

/// mock 的离屏等待端（见 [`Offscreen`]）
struct MockOffscreen {
    log: Log,
    hidden: tokio::sync::watch::Sender<u64>,
    session: u64,
    arrives: bool,
}

impl OffscreenWait for MockOffscreen {
    fn wait(self: Box<Self>, cap: Duration) -> bool {
        if self.arrives {
            self.log.lock().push(Call::Offscreen);
            self.hidden.send_replace(self.session);
        } else {
            self.log.lock().push(Call::OffscreenTimeout(cap));
        }
        self.arrives
    }
}

pub(super) struct MockHost {
    log: Log,
    pub(super) settings: RwLock<Arc<Settings>>,
    /// 剪贴板视图的状态（默认已显示，会话 1）
    view: Mutex<ClipView>,
    /// 面板正显示别的视图（剪贴板视图为 Hidden 时也算面板可见）
    pub(super) other_view: AtomicBool,
    /// 面板正显示 OCR 视图的会话（剪贴板视图为 Hidden 时才看它）
    pub(super) ocr_session: Mutex<Option<u64>>,
    /// `hide_for_paste` 是否结束会话（false：模拟隐藏还没反映到面板状态里）
    pub(super) hide_ends_session: AtomicBool,
    pub(super) offscreen: Mutex<Offscreen>,
    pub(super) hidden_tx: tokio::sync::watch::Sender<u64>,
    pub(super) events: Mutex<Vec<ClipboardChanged>>,
    /// 推给设置窗口的 `ocr_history:changed`
    pub(super) ocr_events: Mutex<Vec<OcrHistoryChanged>>,
    pub(super) toasts: Mutex<Vec<ToastPayload>>,
    pub(super) flashes: AtomicUsize,
    pub(super) paused: Mutex<Vec<bool>>,
    pub(super) last_vacuum: AtomicI64,
    pub(super) too_new_notices: AtomicUsize,
    pub(super) now: AtomicI64,
    /// 下一次读设置时执行一次（模拟读设置期间发生的事）
    pub(super) on_settings: Hook,
    /// 下一次显示 toast 时执行一次（模拟 toast 与主线程往返期间发生的事）
    pub(super) on_toast: Hook,
    /// 下一次向前端发事件时、记下这个事件之前执行一次（模拟投递期间另一个线程的投递）
    pub(super) on_emit: Hook,
    /// 最近分配的面板会话 id
    sessions: AtomicU64,
    /// 面板会话触发时的前台应用（面板宿主的粘贴目标）
    pub(super) prev_app: Mutex<Option<AppRef>>,
    /// 悬浮窗能否交还键盘焦点
    pub(super) float_yields: AtomicBool,
    /// 悬浮窗已就绪（事件同时推给它）
    pub(super) float_ready: AtomicBool,
    /// 推给悬浮窗的 `clipboard:changed`
    pub(super) float_events: Mutex<Vec<ClipboardChanged>>,
    /// 连续粘贴的进度推送（状态, 是否带 toast）
    pub(super) queue_events: Mutex<Vec<(wisp_types::QueueStatus, bool)>>,
}

impl MockHost {
    pub(super) fn set_view(&self, view: ClipView) {
        *self.view.lock() = view;
    }

    pub(super) fn view(&self) -> ClipView {
        *self.view.lock()
    }

    /// 面板当前的会话与视图：剪贴板视图，否则 OCR 视图
    fn panel(&self) -> Option<(u64, PanelView)> {
        self.view()
            .session()
            .map(|s| (s, PanelView::Clipboard))
            .or_else(|| self.ocr_session.lock().map(|s| (s, PanelView::Ocr)))
    }

    /// 某个会话仍是面板当前的会话（与窗口层一致：隐藏、结束之后都不是）
    fn current(&self, session: u64) -> bool {
        self.panel().is_some_and(|(s, _)| s == session)
    }
}

impl Host for MockHost {
    fn settings(&self) -> Arc<Settings> {
        run_hook(&self.on_settings);
        Arc::clone(&self.settings.read())
    }

    fn clipboard_view(&self) -> ClipView {
        self.view()
    }

    fn panel_view(&self) -> Option<(u64, PanelView)> {
        self.panel()
    }

    fn panel_visible(&self) -> bool {
        self.panel().is_some() || self.other_view.load(Ordering::SeqCst)
    }

    fn session_live(&self, session: u64) -> bool {
        self.current(session)
    }

    fn panel_prev_app(&self, session: u64) -> Option<AppRef> {
        // 只有当前会话有记录（与窗口层一致：会话结束后取不到）
        if self.current(session) {
            self.prev_app.lock().clone()
        } else {
            None
        }
    }

    fn float_yield(&self, target: &AppRef) -> bool {
        self.log.lock().push(Call::FloatYield(target.id.clone()));
        self.float_yields.load(Ordering::SeqCst)
    }

    fn float_ready(&self) -> bool {
        self.float_ready.load(Ordering::SeqCst)
    }

    fn emit_float(&self, change: &ClipboardChanged) {
        self.float_events.lock().push(change.clone());
    }

    fn queue_changed(&self, status: &wisp_types::QueueStatus, toast: Option<ToastPayload>) {
        if let Some(t) = &toast {
            assert_eq!(t.queue, Some(*status), "进度 toast 带的就是这次的进度");
        }
        self.queue_events.lock().push((*status, toast.is_some()));
    }

    fn queue_escape_guard(&self, active: bool) {
        self.log.lock().push(Call::EscGuard(active));
    }

    fn panel_hidden(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        Some(self.hidden_tx.subscribe())
    }

    fn emit(&self, change: &ClipboardChanged) {
        run_hook(&self.on_emit);
        self.events.lock().push(change.clone());
    }

    fn emit_ocr_history(&self, change: &OcrHistoryChanged) {
        self.ocr_events.lock().push(change.clone());
    }

    fn hide_for_paste(&self, session: u64) -> Option<Box<dyn OffscreenWait>> {
        self.log.lock().push(Call::Hide);
        if self.hide_ends_session.load(Ordering::SeqCst) && self.current(session) {
            self.set_view(ClipView::Hidden);
            *self.ocr_session.lock() = None;
        }
        let arrives = match *self.offscreen.lock() {
            Offscreen::NoChannel => return None,
            Offscreen::Arrives => true,
            Offscreen::Never => false,
        };
        Some(Box::new(MockOffscreen {
            log: Arc::clone(&self.log),
            hidden: self.hidden_tx.clone(),
            session,
            arrives,
        }))
    }

    fn show_toast(&self, toast: ToastPayload) {
        self.log.lock().push(Call::Toast(toast.reason));
        self.toasts.lock().push(toast);
        run_hook(&self.on_toast);
    }

    fn flash_tray(&self) {
        self.flashes.fetch_add(1, Ordering::SeqCst);
    }

    fn paused_changed(&self, paused: bool) {
        self.paused.lock().push(paused);
    }

    fn last_vacuum_ms(&self) -> i64 {
        self.last_vacuum.load(Ordering::SeqCst)
    }

    fn set_last_vacuum_ms(&self, ms: i64) {
        self.last_vacuum.store(ms, Ordering::SeqCst);
    }

    fn perf_paste(&self, method: &str, selected_at: Instant, posted_at: Instant) {
        assert!(posted_at >= selected_at);
        self.log.lock().push(Call::Perf(method.to_owned()));
    }

    fn self_app(&self) -> AppRef {
        AppRef {
            id: SELF_ID.to_owned(),
            alt_id: None,
            name: "Wisp".to_owned(),
            pid: Some(1),
            path: None,
            window: None,
        }
    }

    fn notify_history_too_new(&self) {
        self.too_new_notices.fetch_add(1, Ordering::SeqCst);
    }

    fn now_ms(&self) -> i64 {
        // 每次取时间前进 1 ms：先后发生的记录与 touch 有确定的顺序
        self.now.fetch_add(1, Ordering::SeqCst)
    }
}

type Job = Box<dyn FnOnce() + Send + 'static>;

/// 后台执行器：默认同步执行；`queue(true)` 之后排队，由 [`TestExec::run_jobs`] 执行。延时任务一律排队，由
/// [`TestExec::fire_timers`] 触发（不真的等）
#[derive(Default)]
pub(super) struct TestExec {
    queued: AtomicBool,
    jobs: Mutex<VecDeque<Job>>,
    timers: Mutex<Vec<Job>>,
}

impl TestExec {
    pub(super) fn queue(&self, on: bool) {
        self.queued.store(on, Ordering::SeqCst);
    }

    /// 执行排队中的后台任务（含执行期间新排的），返回执行数
    pub(super) fn run_jobs(&self) -> usize {
        let mut n = 0;
        loop {
            let job = self.jobs.lock().pop_front();
            let Some(job) = job else { return n };
            job();
            n += 1;
        }
    }

    pub(super) fn pending_jobs(&self) -> usize {
        self.jobs.lock().len()
    }

    /// 触发当前已排的延时任务（触发期间新排的留到下一次），返回触发数
    pub(super) fn fire_timers(&self) -> usize {
        let timers = std::mem::take(&mut *self.timers.lock());
        let n = timers.len();
        for t in timers {
            t();
        }
        n
    }
}

impl Exec for TestExec {
    fn blocking(&self, job: Job) {
        if self.queued.load(Ordering::SeqCst) {
            self.jobs.lock().push_back(job);
        } else {
            job();
        }
    }

    fn after(&self, _delay: Duration, job: Job) {
        self.timers.lock().push(job);
    }
}

pub(super) struct Rig {
    pub(super) dir: tempfile::TempDir,
    pub(super) log: Log,
    pub(super) port: Arc<MockPort>,
    pub(super) host: Arc<MockHost>,
    pub(super) exec: Arc<TestExec>,
    pub(super) engine: Arc<Engine>,
}

pub(super) fn mock_port(log: &Log) -> Arc<MockPort> {
    let change = Arc::new(AtomicU64::new(100));
    Arc::new(MockPort {
        clipboard: MockClipboard {
            log: Arc::clone(log),
            change: Arc::clone(&change),
            reads: Mutex::default(),
            read_calls: AtomicUsize::new(0),
            watching: Arc::default(),
            installs: AtomicUsize::new(0),
            polls: AtomicUsize::new(0),
        },
        paste: MockPaste {
            log: Arc::clone(log),
            change,
            writes: Mutex::default(),
            write_error: Mutex::default(),
            paste_result: Mutex::new(Ok(())),
            during_write: Mutex::default(),
        },
        focus: MockFocus {
            log: Arc::clone(log),
            activate_result: Mutex::new(Ok(())),
            frontmost: Mutex::default(),
        },
        apps: MockApps {
            icon: AtomicBool::new(true),
            calls: AtomicUsize::new(0),
            running: Mutex::default(),
        },
        hooks: MockHooks {
            log: Arc::clone(log),
            sink: Arc::default(),
            keys: Mutex::default(),
            install_error: Mutex::default(),
            replay_error: Mutex::default(),
            unavailable: Mutex::default(),
        },
        target_check: Mutex::new(Ok(())),
        paste_inject: Mutex::new(Capability::yes()),
    })
}

pub(super) fn mock_host(log: &Log) -> Arc<MockHost> {
    Arc::new(MockHost {
        log: Arc::clone(log),
        settings: RwLock::new(Arc::new(Settings::default())),
        view: Mutex::new(ClipView::Shown(1)),
        other_view: AtomicBool::new(false),
        ocr_session: Mutex::default(),
        hide_ends_session: AtomicBool::new(true),
        offscreen: Mutex::new(Offscreen::NoChannel),
        hidden_tx: tokio::sync::watch::Sender::new(0),
        events: Mutex::default(),
        ocr_events: Mutex::default(),
        toasts: Mutex::default(),
        flashes: AtomicUsize::new(0),
        paused: Mutex::default(),
        last_vacuum: AtomicI64::new(0),
        too_new_notices: AtomicUsize::new(0),
        now: AtomicI64::new(1_760_000_000_000),
        on_settings: Mutex::default(),
        on_toast: Mutex::default(),
        on_emit: Mutex::default(),
        sessions: AtomicU64::new(1),
        prev_app: Mutex::default(),
        float_yields: AtomicBool::new(true),
        float_ready: AtomicBool::new(false),
        float_events: Mutex::default(),
        queue_events: Mutex::default(),
    })
}

pub(super) fn rig_with(os: OsKind, dir: tempfile::TempDir) -> Rig {
    let log: Log = Arc::default();
    let port = mock_port(&log);
    let host = mock_host(&log);
    let exec = Arc::new(TestExec::default());
    let engine = Engine::new(
        Arc::clone(&port) as Arc<dyn Port>,
        Arc::clone(&host) as Arc<dyn Host>,
        Arc::clone(&exec) as Arc<dyn Exec>,
        os,
        dir.path().to_path_buf(),
        false,
    );
    Rig {
        dir,
        log,
        port,
        host,
        exec,
        engine,
    }
}

pub(super) fn rig() -> Rig {
    rig_with(OsKind::Macos, tempfile::tempdir().expect("临时目录"))
}

pub(super) fn app(id: &str) -> AppRef {
    AppRef {
        id: id.to_owned(),
        alt_id: None,
        name: id.to_owned(),
        pid: Some(42),
        path: None,
        window: None,
    }
}

/// 面板会话 `session` 里粘贴条目 `id` 的请求（在 mock 的钩子里直接调用 `Engine::paste` 时用）
pub(super) fn panel_item(session: u64, id: i64) -> PasteRequest {
    PasteRequest {
        source: PasteSource::Item { id, transform: None },
        mode: PasteMode::Normal,
        host: PasteHost::Panel { session },
        selected_at: Instant::now(),
    }
}

/// 一条文本条目的元数据（纯函数的单测用）
pub(super) fn text_meta() -> ClipItemMeta {
    ClipItemMeta {
        id: 0,
        kind: ClipKind::Text,
        created_at: 0,
        last_used_at: 0,
        use_count: 1,
        pinned: false,
        source: None,
        preview: String::new(),
        char_count: None,
        line_count: None,
        truncated: false,
        image: None,
        files: None,
        link: None,
        color: None,
        name: None,
        expires_at: None,
    }
}

pub(super) fn text_raw(change: u64, text: &str) -> RawClipboard {
    RawClipboard {
        change_id: change,
        source: Some(app("com.example.editor")),
        text: Some(text.to_owned()),
        ..RawClipboard::default()
    }
}

impl Rig {
    pub(super) fn queue_read(&self, r: Result<RawClipboard, PlatformError>) {
        self.port.clipboard.reads.lock().push_back(r);
    }

    /// 走一遍「回调 → 读取 → 记录」
    pub(super) fn copy(&self, change: u64, raw: RawClipboard) -> Capture {
        self.queue_read(Ok(raw));
        assert!(self.engine.begin_change(change));
        self.engine.capture(change)
    }

    pub(super) fn copy_text(&self, change: u64, text: &str) -> ClipItemMeta {
        assert_eq!(self.copy(change, text_raw(change, text)), Capture::Done);
        self.top()
    }

    pub(super) fn top(&self) -> ClipItemMeta {
        self.engine.first_page().items.first().expect("至少一条").item.clone()
    }

    pub(super) fn ids(&self) -> Vec<i64> {
        self.engine.first_page().items.iter().map(|h| h.item.id).collect()
    }

    pub(super) fn events(&self) -> Vec<ClipboardChanged> {
        std::mem::take(&mut *self.host.events.lock())
    }

    pub(super) fn calls(&self) -> Vec<Call> {
        std::mem::take(&mut *self.log.lock())
    }

    pub(super) fn set_settings(&self, f: impl FnOnce(&mut Settings)) {
        let mut s = (**self.host.settings.read()).clone();
        f(&mut s);
        *self.host.settings.write() = Arc::new(s);
    }

    /// 在面板里粘贴一个条目：目标应用记为当前面板会话的 `prev_app`，宿主按面板窗口推出（与命令层相同）
    pub(super) fn paste(&self, id: i64, mode: PasteMode, target: Option<AppRef>) -> Result<PasteOutcome, IpcError> {
        self.paste_from(PANEL_LABEL, PasteSource::Item { id, transform: None }, mode, target)
    }

    /// 从某个窗口发起一次粘贴（宿主按 label 推出）。面板的目标应用是会话的 `prev_app`，悬浮窗是当前的前台应用
    pub(super) fn paste_from(
        &self,
        label: &str,
        source: PasteSource,
        mode: PasteMode,
        target: Option<AppRef>,
    ) -> Result<PasteOutcome, IpcError> {
        let host = PasteHost::of_window(label, self.host.panel_view(), source.kind())?;
        if label == PANEL_LABEL {
            *self.host.prev_app.lock() = target;
        } else {
            *self.port.focus.frontmost.lock() = target;
        }
        self.engine.paste(PasteRequest {
            source,
            mode,
            host,
            selected_at: Instant::now(),
        })
    }

    /// 重新打开剪贴板面板（新的会话，已显示）
    pub(super) fn reopen(&self) -> u64 {
        let next = self.host.sessions.fetch_add(1, Ordering::SeqCst) + 1;
        *self.host.ocr_session.lock() = None;
        self.host.set_view(ClipView::Shown(next));
        next
    }

    /// 面板换成显示 OCR 结果（新的会话；剪贴板视图随之 Hidden）
    pub(super) fn show_ocr(&self) -> u64 {
        let next = self.host.sessions.fetch_add(1, Ordering::SeqCst) + 1;
        self.host.set_view(ClipView::Hidden);
        *self.host.ocr_session.lock() = Some(next);
        next
    }

    /// 写一个文件条目（两个文件都在临时目录里），返回条目与两个路径
    pub(super) fn copy_files(&self, change: u64, names: [&str; 2]) -> (ClipItemMeta, [std::path::PathBuf; 2]) {
        let paths = names.map(|n| self.dir.path().join(n));
        for p in &paths {
            std::fs::write(p, b"x").expect("写文件");
        }
        let raw = RawClipboard {
            change_id: change,
            files: Some(paths.iter().map(|p| p.to_string_lossy().into_owned()).collect()),
            ..RawClipboard::default()
        };
        assert_eq!(self.copy(change, raw), Capture::Done);
        (self.top(), paths)
    }
}

// ---------- 记录管线 ----------

#[test]
fn a_copy_is_recorded_cached_and_pushed_while_visible() {
    let r = rig();
    let meta = r.copy_text(101, "hello wisp");
    assert_eq!(meta.preview, "hello wisp");
    assert_eq!(meta.source.as_ref().map(|s| s.id.as_str()), Some("com.example.editor"));
    let events = r.events();
    assert!(
        matches!(&events[0], ClipboardChanged::Inserted { item } if item.id == meta.id),
        "{events:?}"
    );
    // 来源图标在后台取到后经 updated 推送，缓存里的条目也带上了
    assert!(
        events.iter().any(|e| matches!(e, ClipboardChanged::Updated { item }
            if item.id == meta.id && item.source.as_ref().is_some_and(|s| s.icon_url.is_some()))),
        "{events:?}"
    );
    assert!(meta.source.as_ref().is_some_and(|s| s.icon_url.is_some()));
}

#[test]
fn hidden_panel_gets_no_events_but_the_cache_is_current() {
    let r = rig();
    r.host.set_view(ClipView::Hidden);
    let meta = r.copy_text(101, "quiet");
    assert!(r.events().is_empty());
    assert_eq!(r.engine.first_page().items[0].item.id, meta.id);
}

#[test]
fn copying_the_same_text_touches_instead_of_inserting() {
    let r = rig();
    let a = r.copy_text(101, "same");
    r.copy_text(102, "other");
    r.events();
    let again = r.copy_text(103, "same");
    assert_eq!(again.id, a.id);
    assert_eq!(again.use_count, 2);
    assert_eq!(r.ids().len(), 2);
    assert!(matches!(r.events()[0], ClipboardChanged::Touched { .. }));
}

#[test]
fn wisp_own_writes_and_concealed_changes_leave_no_trace() {
    let r = rig();
    let mut own = text_raw(101, "written by wisp");
    own.from_wisp = true;
    assert_eq!(r.copy(101, own), Capture::Done);
    let concealed = RawClipboard {
        change_id: 102,
        concealed: true,
        ..RawClipboard::default()
    };
    assert_eq!(r.copy(102, concealed), Capture::Done);
    // 数据库都没有打开，也没有任何事件或图标请求
    assert!(r.engine.ready_store().is_none());
    assert!(r.events().is_empty());
    assert_eq!(r.port.apps.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn duplicate_change_ids_are_processed_once() {
    let r = rig();
    assert!(r.engine.begin_change(5));
    assert!(!r.engine.begin_change(5));
    assert!(r.engine.begin_change(6));
    // 读到的序号比回调新：之后报告这个序号的回调视为已处理
    r.queue_read(Ok(text_raw(7, "x")));
    assert_eq!(r.engine.capture(6), Capture::Done);
    assert!(!r.engine.begin_change(7));
    // 只比较相等：序号变小（pboard 重启）照常处理
    assert!(r.engine.begin_change(1));
}

#[test]
fn suspected_secrets_are_dropped_with_a_tray_flash() {
    let r = rig();
    let key = format!("sk-{}", "aB3dE5fG7hJ9kL1mN3pQ5rS7tU9vW1xY3zA5bC7dE9fG1hJ3");
    assert_eq!(r.copy(101, text_raw(101, &key)), Capture::Done);
    assert_eq!(r.host.flashes.load(Ordering::SeqCst), 1);
    assert_eq!(r.events(), vec![ClipboardChanged::SecretSkipped]);
    assert!(r.engine.first_page().items.is_empty());
    // 被跳过的复制不取图标
    assert_eq!(r.port.apps.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn excluded_apps_are_not_recorded() {
    let r = rig();
    r.set_settings(|s| {
        s.clipboard.excluded_apps.push(wisp_types::ExcludedApp {
            os: OsKind::Macos,
            app_id: "com.example.editor".into(),
            name: String::new(),
        });
    });
    assert_eq!(r.copy(101, text_raw(101, "hidden")), Capture::Done);
    assert!(r.engine.first_page().items.is_empty());
    assert!(r.events().is_empty());
}

#[test]
fn pausing_unregisters_the_watch_and_resuming_reinstalls_it() {
    let r = rig();
    r.engine.apply_settings_now(&Settings::default().clipboard);
    r.engine.reconcile_watch().expect("安装监听");
    assert!(r.engine.watching());
    r.engine.set_paused(true);
    assert!(r.engine.paused());
    assert!(!r.engine.watching(), "暂停时完全注销监听");
    assert_eq!(*r.host.paused.lock(), vec![true]);
    assert_eq!(r.events(), vec![ClipboardChanged::Paused { paused: true }]);
    // 暂停期间的回调（注销前已在路上的）不读取、不记录
    assert!(r.engine.begin_change(101));
    assert_eq!(r.engine.capture(101), Capture::Done);
    assert_eq!(r.port.clipboard.read_calls.load(Ordering::SeqCst), 0);
    // 同值重复设置不重复通知
    r.engine.set_paused(true);
    assert_eq!(r.host.paused.lock().len(), 1);
    r.engine.set_paused(false);
    assert!(r.engine.watching());
    assert_eq!(r.port.clipboard.installs.load(Ordering::SeqCst), 2);
}

#[test]
fn the_enabled_switch_controls_the_watch() {
    let r = rig();
    r.set_settings(|s| s.clipboard.enabled = false);
    r.engine.apply_settings_now(&r.host.settings().clipboard);
    r.engine.reconcile_watch().expect("ok");
    assert!(!r.engine.watching());
    r.set_settings(|s| s.clipboard.enabled = true);
    r.engine.apply_settings(&r.host.settings().clipboard);
    assert!(r.engine.watching());
    r.set_settings(|s| s.clipboard.enabled = false);
    r.engine.apply_settings(&r.host.settings().clipboard);
    assert!(!r.engine.watching());
}

#[test]
fn retention_changes_schedule_an_idle_cleanup() {
    let r = rig();
    r.engine.apply_settings_now(&Settings::default().clipboard);
    assert!(!r.engine.idle.dirty());
    r.set_settings(|s| s.clipboard.show_hint_bar = false);
    r.engine.apply_settings(&r.host.settings().clipboard);
    assert!(!r.engine.idle.dirty(), "与保留无关的设置不触发清理");
    r.set_settings(|s| s.clipboard.max_items = 10);
    r.engine.apply_settings(&r.host.settings().clipboard);
    assert!(r.engine.idle.dirty());
}

#[tokio::test]
async fn read_failures_are_retried_then_recorded() {
    let r = rig();
    r.queue_read(Err(PlatformError::Timeout));
    r.queue_read(Err(PlatformError::Busy("locked".into())));
    r.queue_read(Ok(text_raw(101, "after retries")));
    r.engine.signal().post(101);
    process_with(&r.engine, 101, &[Duration::ZERO; 3]).await;
    assert_eq!(r.port.clipboard.read_calls.load(Ordering::SeqCst), 3);
    assert_eq!(r.top().preview, "after retries");
}

/// macOS：clearContents 递增序号、writeObjects 不递增。轮询落在两者之间读到空剪贴板时重试，而不是把这个序号当作
/// 「空内容、已处理」记下——否则紧接着写入的内容就永远丢了
#[tokio::test]
async fn an_empty_read_is_retried_until_the_writer_finishes() {
    let r = rig();
    r.queue_read(Ok(RawClipboard {
        change_id: 101,
        source: Some(app("com.example.editor")),
        ..RawClipboard::default()
    }));
    r.queue_read(Ok(text_raw(101, "written after clear")));
    r.engine.signal().post(101);
    process_with(&r.engine, 101, &[Duration::ZERO; 3]).await;
    assert_eq!(r.port.clipboard.read_calls.load(Ordering::SeqCst), 2);
    assert_eq!(r.top().preview, "written after clear");
    // 序号已经变了（读到的是更新的内容）时不重试：空内容按跳过处理
    r.queue_read(Ok(RawClipboard {
        change_id: 103,
        ..RawClipboard::default()
    }));
    assert!(r.engine.begin_change(102));
    assert_eq!(r.engine.capture(102), Capture::Done);
}

/// 载荷里的监听状态（暂停 / 设置里关闭 / 正在记录）与历史问题（正常时没有）
#[test]
fn recording_state_reports_paused_disabled_and_recording() {
    let r = rig();
    assert_eq!(r.engine.recording_state(), wisp_types::ClipboardRecording::Recording);
    assert_eq!(r.engine.history_issue(), None);
    r.engine.set_paused(true);
    assert_eq!(r.engine.recording_state(), wisp_types::ClipboardRecording::Paused);
    r.engine.set_paused(false);
    r.set_settings(|s| s.clipboard.enabled = false);
    assert_eq!(r.engine.recording_state(), wisp_types::ClipboardRecording::Disabled);
}

/// 「当前剪贴板内容已在历史里」：记录过、序号一致、没有暂停时才成立（取词的模拟复制据此决定备份失败时能否放弃恢复）
#[test]
fn current_content_recorded_requires_recording_and_the_same_change() {
    let r = rig();
    r.port.clipboard.change.store(101, Ordering::SeqCst);
    assert!(!r.engine.current_content_recorded(), "什么都没记过");
    r.copy_text(101, "recorded");
    assert!(r.engine.current_content_recorded());
    // 剪贴板又变了、还没记录
    r.port.clipboard.change.store(102, Ordering::SeqCst);
    assert!(!r.engine.current_content_recorded());
    r.port.clipboard.change.store(101, Ordering::SeqCst);
    r.engine.set_paused(true);
    assert!(!r.engine.current_content_recorded(), "暂停期间不算");
    r.engine.set_paused(false);
    assert!(r.engine.current_content_recorded());
    // 被隐私规则跳过的内容不算记录
    r.port.clipboard.change.store(104, Ordering::SeqCst);
    assert_eq!(
        r.copy(
            104,
            RawClipboard {
                change_id: 104,
                concealed: true,
                ..RawClipboard::default()
            }
        ),
        Capture::Done
    );
    assert!(!r.engine.current_content_recorded());
}

#[tokio::test]
async fn reads_give_up_after_three_retries() {
    let r = rig();
    for _ in 0..5 {
        r.queue_read(Err(PlatformError::Timeout));
    }
    r.engine.signal().post(101);
    process_with(&r.engine, 101, &[Duration::ZERO; 3]).await;
    assert_eq!(
        r.port.clipboard.read_calls.load(Ordering::SeqCst),
        4,
        "1 次 + 重试 3 次"
    );
    assert!(r.engine.ready_store().is_none());
}

#[tokio::test]
async fn a_newer_change_cancels_the_retry() {
    let r = rig();
    r.queue_read(Err(PlatformError::Timeout));
    r.engine.signal().post(102);
    process_with(&r.engine, 101, &[Duration::ZERO; 3]).await;
    assert_eq!(r.port.clipboard.read_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn unsupported_reads_are_not_retried() {
    let r = rig();
    r.queue_read(Err(PlatformError::Unsupported(
        wisp_platform::reason::WAYLAND_CLIPBOARD_WATCH.into(),
    )));
    assert!(r.engine.begin_change(101));
    assert_eq!(r.engine.capture(101), Capture::Done);
}

// ---------- 粘贴 ----------

#[test]
fn paste_writes_suppresses_touches_hides_then_pastes() {
    let r = rig();
    let a = r.copy_text(101, "first");
    r.copy_text(102, "second");
    r.calls();
    let out = r
        .paste(a.id, PasteMode::Normal, Some(app("com.example.target")))
        .expect("粘贴");
    assert_eq!(out, PasteOutcome::Pasted);
    let change = r.port.clipboard.change_id();
    assert_eq!(
        r.calls(),
        vec![
            Call::Write,
            Call::Suppress(change),
            Call::Hide,
            Call::PasteInto { plain: false },
            Call::Perf("cg_event".into()),
        ]
    );
    // touch：移到顶部、次数 +1，没有新条目
    let top = r.top();
    assert_eq!((top.id, top.use_count), (a.id, 2));
    assert_eq!(r.ids().len(), 2);
    assert!(r.host.toasts.lock().is_empty());
    // 监听方随后看到这次写入（带 Wisp 标记）：跳过，不会再 touch 一次
    let mut echo = text_raw(change, "first");
    echo.from_wisp = true;
    r.copy(change, echo);
    assert_eq!(r.top().use_count, 2);
}

#[test]
fn plain_paste_of_rich_text_writes_text_only_and_keeps_the_html() {
    let r = rig();
    let raw = RawClipboard {
        change_id: 101,
        source: Some(app("com.example.browser")),
        text: Some("bold".into()),
        html: Some("<b>bold</b>".into()),
        ..RawClipboard::default()
    };
    r.copy(101, raw);
    let item = r.top();
    assert_eq!(item.kind, ClipKind::RichText);
    r.calls();
    r.paste(item.id, PasteMode::Plain, Some(app("com.example.target")))
        .expect("粘贴");
    let written = r.port.paste.writes.lock().last().cloned().expect("写入");
    assert_eq!(written.text.as_deref(), Some("bold"));
    assert!(written.html.is_none() && written.rtf.is_none());
    assert!(r.calls().contains(&Call::PasteInto { plain: true }));
    // 原条目的 HTML 与来源不变
    let detail = r.engine.detail(item.id).expect("详情").expect("存在");
    assert!(detail.has_html);
    assert_eq!(detail.meta.source.map(|s| s.id), Some("com.example.browser".into()));
    assert_eq!(r.ids().len(), 1);
}

#[test]
fn windows_hands_focus_back_before_hiding() {
    let r = rig_with(OsKind::Windows, tempfile::tempdir().expect("临时目录"));
    let a = r.copy_text(101, "win");
    r.calls();
    r.paste(a.id, PasteMode::Normal, Some(app("notepad.exe")))
        .expect("粘贴");
    let calls = r.calls();
    assert_eq!(
        &calls[2..],
        &[
            Call::Activate("notepad.exe".into()),
            Call::Hide,
            Call::PasteInto { plain: false },
            Call::Perf("send_input".into()),
        ]
    );
}

#[test]
fn copy_only_returns_focus_and_shows_a_toast() {
    for (os, activates) in [(OsKind::Macos, false), (OsKind::Windows, true), (OsKind::Linux, true)] {
        let r = rig_with(os, tempfile::tempdir().expect("临时目录"));
        let a = r.copy_text(101, "copy only");
        r.calls();
        let out = r.paste(a.id, PasteMode::CopyOnly, Some(app("target"))).expect("复制");
        assert_eq!(
            out,
            PasteOutcome::Copied {
                reason: CopiedReason::CopyOnly
            }
        );
        let calls = r.calls();
        assert!(!calls.iter().any(|c| matches!(c, Call::PasteInto { .. })), "{os:?}");
        assert_eq!(calls.contains(&Call::Activate("target".into())), activates, "{os:?}");
        assert_eq!(
            &calls[calls.len() - 2..],
            &[Call::Hide, Call::Toast(CopiedReason::CopyOnly)],
            "{os:?}"
        );
        assert!(!r.host.toasts.lock()[0].paste_key.is_empty());
        assert_eq!(r.top().use_count, 2, "仅复制同样 touch");
    }
}

#[test]
fn unknown_target_only_copies() {
    let r = rig();
    let a = r.copy_text(101, "no target");
    r.calls();
    let out = r.paste(a.id, PasteMode::Normal, None).expect("复制");
    assert_eq!(
        out,
        PasteOutcome::Copied {
            reason: CopiedReason::Manual
        }
    );
    let calls = r.calls();
    assert_eq!(&calls[2..], &[Call::Hide, Call::Toast(CopiedReason::Manual)]);
}

#[test]
fn every_paste_failure_becomes_a_toast() {
    let cases = [
        (PasteError::TargetGone, CopiedReason::TargetGone),
        (PasteError::FocusTimeout, CopiedReason::FocusTimeout),
        (PasteError::Elevated, CopiedReason::Elevated),
        (PasteError::PermissionMissing, CopiedReason::NoPermission),
        (
            PasteError::Unsupported(wisp_platform::reason::WAYLAND_PASTE.into()),
            CopiedReason::Wayland,
        ),
        // 没有对应原因的失败：只提示按键、不写原因（Manual）
        (
            PasteError::Unsupported(wisp_platform::reason::X11_MISSING_XTEST.into()),
            CopiedReason::Manual,
        ),
        (PasteError::Other("send failed".into()), CopiedReason::Manual),
    ];
    for (err, reason) in cases {
        let r = rig();
        let a = r.copy_text(101, "x");
        *r.port.paste.paste_result.lock() = Err(err.clone());
        r.calls();
        let out = r.paste(a.id, PasteMode::Normal, Some(app("target"))).expect("只复制");
        assert_eq!(out, PasteOutcome::Copied { reason }, "{err:?}");
        let calls = r.calls();
        assert_eq!(calls.last(), Some(&Call::Toast(reason)), "{err:?}");
        assert!(!calls.iter().any(|c| matches!(c, Call::Perf(_))), "失败时不记粘贴耗时");
    }
}

#[test]
fn a_failed_write_keeps_the_panel_open_and_reports_the_platform_key() {
    let r = rig();
    let a = r.copy_text(101, "kde");
    *r.port.paste.write_error.lock() = Some(PlatformError::Unsupported(
        wisp_platform::reason::WAYLAND_CLIPBOARD_BRIDGE.into(),
    ));
    r.calls();
    let err = r
        .paste(a.id, PasteMode::Normal, Some(app("target")))
        .expect_err("写入失败");
    assert_eq!(err.code, IpcErrorCode::Platform);
    assert_eq!(err.message, wisp_platform::reason::WAYLAND_CLIPBOARD_BRIDGE);
    assert!(r.calls().is_empty(), "不隐藏、不 toast、不抑制");
    assert_eq!(r.top().use_count, 1);
}

#[test]
fn pasting_a_missing_item_is_not_found() {
    let r = rig();
    r.copy_text(101, "x");
    r.calls();
    let err = r.paste(9999, PasteMode::Normal, Some(app("t"))).expect_err("不存在");
    assert_eq!(err.code, IpcErrorCode::NotFound);
    assert!(r.calls().is_empty());
}

#[test]
fn file_items_paste_only_files_that_still_exist() {
    let r = rig();
    let keep = r.dir.path().join("keep.txt");
    std::fs::write(&keep, b"k").expect("写文件");
    let gone = r.dir.path().join("gone.txt");
    std::fs::write(&gone, b"g").expect("写文件");
    let paths = vec![keep.to_string_lossy().into_owned(), gone.to_string_lossy().into_owned()];
    let raw = RawClipboard {
        change_id: 101,
        files: Some(paths.clone()),
        ..RawClipboard::default()
    };
    r.copy(101, raw);
    let item = r.top();
    assert_eq!(item.kind, ClipKind::Files);
    std::fs::remove_file(&gone).expect("删文件");
    r.paste(item.id, PasteMode::Normal, Some(app("finder"))).expect("粘贴");
    let written = r.port.paste.writes.lock().last().cloned().expect("写入");
    assert_eq!(written.files, Some(vec![paths[0].clone()]));
    assert_eq!(written.text.as_deref(), Some(paths[0].as_str()));
    std::fs::remove_file(&keep).expect("删文件");
    r.reopen();
    r.calls();
    let err = r
        .paste(item.id, PasteMode::Normal, Some(app("finder")))
        .expect_err("都不在了");
    assert_eq!(err.code, IpcErrorCode::NotFound);
    assert!(r.calls().is_empty());
}

// ---------- copy_text ----------

#[test]
fn copied_text_enters_history_as_coming_from_wisp() {
    let r = rig();
    r.engine.copy_text("译文").expect("复制");
    let change = r.port.clipboard.change_id();
    assert_eq!(r.calls(), vec![Call::Write, Call::Suppress(change)]);
    let top = r.top();
    assert_eq!(top.preview, "译文");
    let source = top.source.expect("来源");
    assert_eq!(source.id, SELF_ID);
    // Wisp 自己的图标来自内置资源，不向平台要
    assert!(source.icon_url.is_some());
    assert_eq!(r.port.apps.calls.load(Ordering::SeqCst), 0);
    // 监听方随后看到这次写入：跳过，不会重复
    let mut echo = text_raw(change, "译文");
    echo.from_wisp = true;
    r.copy(change, echo);
    assert_eq!(r.engine.first_page().items.len(), 1);
}

#[test]
fn copied_text_is_not_recorded_while_paused() {
    let r = rig();
    r.engine.set_paused(true);
    let receipt = r.engine.copy_text("secret plan").expect("复制");
    assert_eq!(r.port.paste.writes.lock().len(), 1, "照常写入剪贴板");
    assert!(r.engine.first_page().items.is_empty());
    assert_eq!(receipt.created, None, "没有记进历史");
    assert_eq!(receipt.change_id, r.port.clipboard.change_id());
}

#[test]
fn copy_receipts_name_only_items_the_copy_created() {
    let r = rig();
    let first = r.engine.copy_text("识别结果").expect("复制");
    assert_eq!(first.change_id, r.port.clipboard.change_id());
    assert_eq!(first.created, Some(r.top().id));
    // 内容早已在历史里：只是移到顶部，不是这次复制产生的
    let older = r.copy_text(500, "以前复制过的");
    let again = r.engine.copy_text("以前复制过的").expect("复制");
    assert_eq!(again.created, None);
    assert_eq!(r.top().id, older.id);
}

/// 写入 → 回执 → 用 `recopy_text` 改写成 `text`
fn recopy(r: &Rig, prev: CopyReceipt, text: &str) -> Option<CopyReceipt> {
    r.engine.recopy_text(prev, text).expect("跟随")
}

#[test]
fn recopy_replaces_the_item_the_auto_copy_created() {
    let r = rig();
    let first = r.engine.copy_text("第一段\n\n第二段").expect("复制");
    let old = first.created.expect("新建");
    r.events();
    let next = recopy(&r, first, "第一段\n第二段").expect("剪贴板没变：写入");
    assert_eq!(next.change_id, r.port.clipboard.change_id());
    let new = next.created.expect("新文本新建了一条");
    assert_ne!(new, old);
    assert_eq!(r.ids(), vec![new], "历史里只留现在这一版");
    let events = r.events();
    assert!(events.contains(&ClipboardChanged::Removed { id: old }), "{events:?}");
    assert_eq!(
        r.port
            .paste
            .writes
            .lock()
            .last()
            .and_then(|p| p.text.clone())
            .as_deref(),
        Some("第一段\n第二段")
    );
    // 再切回去：替换的是刚才那一条
    let back = recopy(&r, next, "第一段\n\n第二段").expect("写入");
    assert_eq!(r.ids(), vec![back.created.expect("新建")]);
}

#[test]
fn recopy_does_nothing_once_something_else_was_copied() {
    let r = rig();
    let first = r.engine.copy_text("识别结果").expect("复制");
    // 用户在别处复制了东西（监听方随后记下它）
    r.port.clipboard.change.fetch_add(1, Ordering::SeqCst);
    let writes = r.port.paste.writes.lock().len();
    let ids = r.ids();
    assert_eq!(recopy(&r, first, "排版后的识别结果"), None);
    assert_eq!(r.port.paste.writes.lock().len(), writes, "不写剪贴板");
    assert_eq!(r.ids(), ids, "不删也不增");
}

#[test]
fn recopy_keeps_favorites_and_named_items() {
    for name in [None, Some("报销单号")] {
        let r = rig();
        let first = r.engine.copy_text("识别结果").expect("复制");
        let old = first.created.expect("新建");
        match name {
            None => drop(r.engine.pin(old, true).expect("收藏")),
            Some(n) => drop(r.engine.set_name(old, Some(n)).expect("命名")),
        }
        let next = recopy(&r, first, "排版后的识别结果").expect("写入");
        let new = next.created.expect("新建");
        assert!(r.ids().contains(&old), "收藏 / 命名过的保留：{name:?}");
        assert!(r.ids().contains(&new));
        // 保留下来的那一条不再算作自动复制产生的：下一次跟随只替换新的这一条
        let last = recopy(&r, next, "识别结果，再排一次").expect("写入");
        assert!(r.ids().contains(&old));
        assert!(!r.ids().contains(&new));
        assert!(last.created.is_some());
    }
}

#[test]
fn recopy_never_deletes_what_it_just_wrote_or_what_the_user_copied_before() {
    let r = rig();
    // 排版结果与上一版相同（例如没有空行可去）：只 touch 自己那一条，不删，回执仍记着它
    let first = r.engine.copy_text("单独一行").expect("复制");
    let own = first.created.expect("新建");
    let same = recopy(&r, first, "单独一行").expect("写入");
    assert_eq!(same.created, Some(own));
    assert_eq!(r.ids(), vec![own]);
    // 新文本是用户早先复制过的内容：那一条被移到顶部，不是这次产生的；自动复制新建的那一条照常替换
    let user = r.copy_text(900, "用户自己复制的");
    let next = recopy(&r, same, "用户自己复制的").expect("写入");
    assert_eq!(next.created, None);
    assert_eq!(r.ids(), vec![user.id]);
    // 之后再切换：没有可替换的条目，用户那一条不动
    let after = recopy(&r, next, "又一版").expect("写入");
    assert!(r.ids().contains(&user.id));
    assert_eq!(r.ids().len(), 2);
    assert!(after.created.is_some());
}

#[test]
fn recopy_keeps_the_old_item_when_the_new_text_is_not_recorded() {
    let r = rig();
    let first = r.engine.copy_text("识别结果").expect("复制");
    let old = first.created.expect("新建");
    // 期间暂停了记录：照常写剪贴板，但不能只删不增
    r.engine.set_paused(true);
    let next = recopy(&r, first, "排版后的识别结果").expect("写入");
    assert_eq!(
        r.port
            .paste
            .writes
            .lock()
            .last()
            .and_then(|p| p.text.clone())
            .as_deref(),
        Some("排版后的识别结果")
    );
    assert_eq!(r.ids(), vec![old]);
    assert_eq!(
        next.created,
        Some(old),
        "旧条目留在回执上，恢复记录后的下一次跟随再替换"
    );
    r.engine.set_paused(false);
    let last = recopy(&r, next, "再排一次").expect("写入");
    assert_eq!(r.ids(), vec![last.created.expect("新建")]);
}

// ---------- 条目操作 ----------

#[test]
fn pin_delete_and_clear_keep_the_cache_in_step() {
    let r = rig();
    let a = r.copy_text(101, "a");
    let b = r.copy_text(102, "b");
    let c = r.copy_text(103, "c");
    assert_eq!(r.ids(), vec![c.id, b.id, a.id]);
    let pinned = r.engine.pin(a.id, true).expect("置顶");
    assert!(pinned.pinned);
    assert_eq!(r.ids(), vec![a.id, c.id, b.id], "置顶项固定在顶部");
    r.events();

    let err = r.engine.delete(a.id, false).expect_err("置顶项要求 force");
    assert_eq!(err.code, IpcErrorCode::Conflict);
    r.engine.delete(b.id, false).expect("删除");
    assert_eq!(r.ids(), vec![a.id, c.id]);
    assert_eq!(r.events(), vec![ClipboardChanged::Removed { id: b.id }]);
    r.engine.delete(b.id, false).expect("已删除的视为成功");

    let n = r.engine.clear(true).expect("清空");
    assert_eq!(n, 1);
    assert_eq!(r.ids(), vec![a.id]);
    assert_eq!(r.events(), vec![ClipboardChanged::Cleared { keep_pinned: true }]);
    r.engine.delete(a.id, true).expect("强制删除置顶项");
    assert!(r.ids().is_empty());
    // 数据库与缓存一致
    let page = r.engine.recent(1, 10, ClipFilter::All).expect("翻页");
    assert!(page.items.is_empty());
}

#[test]
fn first_pages_come_from_the_cache_and_later_pages_from_the_database() {
    let r = rig();
    let ids: Vec<i64> = (0..5).map(|i| r.copy_text(101 + i, &format!("item {i}")).id).collect();
    let first = r.engine.recent(0, 2, ClipFilter::All).expect("首页");
    assert_eq!(first.items.len(), 2);
    assert_eq!(first.next_offset, Some(2));
    let rest = r.engine.recent(2, 10, ClipFilter::All).expect("下一页");
    let got: Vec<i64> = first.items.iter().chain(&rest.items).map(|h| h.item.id).collect();
    let mut expected = ids;
    expected.reverse();
    assert_eq!(got, expected);
    let found = r.engine.search("item 3", ClipFilter::All, 10).expect("检索");
    assert_eq!(found.items.len(), 1);
}

#[test]
fn preload_opens_once_and_first_page_never_queries_afterwards() {
    let dir = tempfile::tempdir().expect("临时目录");
    {
        let store = ClipStore::open(dir.path()).expect("建库");
        let policy = wisp_clipboard::IngestPolicy::from(&Settings::default().clipboard);
        for i in 0..3u64 {
            store
                .ingest(&text_raw(i, &format!("old {i}")), &policy, 1_000 + i as i64)
                .expect("写入");
        }
    }
    let r = rig_with(OsKind::Macos, dir);
    assert!(r.engine.ready_store().is_none(), "构造时不打开数据库");
    r.engine.preload();
    let store = r.engine.ready_store().expect("已打开");
    let page = r.engine.first_page();
    assert_eq!(page.items.len(), 3);
    assert_eq!(page.items[0].item.preview, "old 2");
    assert!(Arc::ptr_eq(&store, &r.engine.ready_store().expect("同一个")));
}

#[test]
fn first_page_before_preload_opens_the_store_lazily() {
    let r = rig();
    assert!(r.engine.first_page().items.is_empty());
    assert!(r.engine.ready_store().is_some());
}

/// 剪贴板面板即将显示（取首页）时让轮询立即读一次（macOS 用户安静时轮询放慢到 1 s / 5 s）；
/// 没有监听（暂停记录）时不碰平台
#[test]
fn first_page_asks_the_watcher_to_poll_now() {
    let r = rig();
    let _ = r.engine.first_page();
    assert_eq!(r.port.clipboard.polls.load(Ordering::SeqCst), 0, "还没有安装监听");
    r.engine.reconcile_watch().expect("安装监听");
    let _ = r.engine.first_page();
    assert_eq!(r.port.clipboard.polls.load(Ordering::SeqCst), 1);
}

#[test]
fn concurrent_records_and_operations_keep_the_cache_a_prefix_of_the_database() {
    let r = rig();
    r.host.set_view(ClipView::Hidden);
    let engine = Arc::clone(&r.engine);
    std::thread::scope(|s| {
        for t in 0..4u64 {
            let engine = Arc::clone(&engine);
            s.spawn(move || {
                for i in 0..25u64 {
                    let raw = text_raw(0, &format!("t{t} n{i} {}", "x".repeat((i % 7) as usize)));
                    engine.ingest(&raw).expect("记录");
                    if i % 5 == 0 {
                        let top = engine.first_page().items[0].item.id;
                        engine.pin(top, i % 10 == 0).expect("置顶");
                    }
                }
            });
        }
        let engine = Arc::clone(&engine);
        s.spawn(move || {
            for _ in 0..200 {
                let _ = engine.first_page();
            }
        });
    });
    let cached: Vec<i64> = r.engine.first_page().items.iter().map(|h| h.item.id).collect();
    let store = r.engine.ready_store().expect("已打开");
    let db: Vec<i64> = store
        .recent(0, 500, ClipFilter::All)
        .expect("查库")
        .items
        .iter()
        .map(|h| h.item.id)
        .collect();
    assert_eq!(db.len(), 100);
    assert_eq!(cached, db, "缓存与数据库的排序一致");
}

// ---------- 数据库来自更新版本 ----------

/// 把库的 user_version（文件头偏移 60，大端）改成很大的值，模拟「用户从新版降级」
fn make_schema_too_new(dir: &Path) {
    drop(ClipStore::open(dir).expect("建库"));
    let db = dir.join("clipboard.db");
    let mut bytes = std::fs::read(&db).expect("读库");
    bytes[60..64].copy_from_slice(&999u32.to_be_bytes());
    std::fs::write(&db, bytes).expect("写库");
}

#[test]
fn a_newer_database_disables_history_without_touching_the_file() {
    let dir = tempfile::tempdir().expect("临时目录");
    make_schema_too_new(dir.path());
    let before = std::fs::read(dir.path().join("clipboard.db")).expect("读库");
    let r = rig_with(OsKind::Macos, dir);
    r.engine.apply_settings_now(&Settings::default().clipboard);
    r.engine.reconcile_watch().expect("ok");
    assert!(r.engine.watching());
    assert!(r.engine.first_page().items.is_empty());
    assert!(r.engine.too_new());
    assert!(!r.engine.watching(), "停用历史时注销监听");
    assert_eq!(r.host.too_new_notices.load(Ordering::SeqCst), 1);
    // 之后的操作不再尝试打开、不重复提示
    assert_eq!(r.engine.pin(1, true).expect_err("停用").code, IpcErrorCode::NotReady);
    assert!(r.engine.recent(0, 10, ClipFilter::All).expect("空页").items.is_empty());
    r.engine.copy_text("still copies").expect("照常写剪贴板");
    assert_eq!(r.host.too_new_notices.load(Ordering::SeqCst), 1);
    let after = std::fs::read(r.dir.path().join("clipboard.db")).expect("读库");
    assert_eq!(before, after, "库文件原样保留");
}

// ---------- 缩略图、图标、文件条目 ----------

#[test]
fn images_get_a_background_thumbnail_pushed_as_updated() {
    let r = rig();
    let raw = RawClipboard {
        change_id: 101,
        image_png: Some(ICON_64.to_vec()),
        ..RawClipboard::default()
    };
    r.copy(101, raw);
    let events = r.events();
    let ClipboardChanged::Inserted { item } = &events[0] else {
        panic!("{events:?}")
    };
    assert!(
        item.image.as_ref().is_some_and(|i| i.thumb_url.is_none()),
        "ingest 不等缩略图"
    );
    let updated = events
        .iter()
        .find_map(|e| match e {
            ClipboardChanged::Updated { item: u } if u.id == item.id => Some(u.clone()),
            _ => None,
        })
        .expect("缩略图生成后推送 updated");
    let thumb = updated.image.and_then(|i| i.thumb_url).expect("thumb_url");
    assert!(thumb.starts_with("thumb/"));
    // 缓存里也是新的；资源可以经 wisp:// 取到
    assert_eq!(r.top().image.and_then(|i| i.thumb_url), Some(thumb.clone()));
    let registry = AssetRegistry::new();
    super::assets::register(&registry, &r.engine);
    let asset = registry.fetch(&thumb).expect("缩略图资源");
    assert!(asset.mime.starts_with("image/"));
    assert!(!asset.bytes.is_empty());
    // 原图（Space 预览）
    let detail = r.engine.detail(item.id).expect("详情").expect("存在");
    let image = registry.fetch(&detail.image_url.expect("原图")).expect("原图资源");
    assert_eq!(image.bytes.as_slice(), ICON_64);
}

#[test]
fn icons_are_fetched_once_per_app() {
    let r = rig();
    r.copy_text(101, "one");
    r.copy_text(102, "two");
    assert_eq!(
        r.port.apps.calls.load(Ordering::SeqCst),
        1,
        "第二条在 ingest 时直接带上图标"
    );
    let top = r.top();
    let url = top.source.and_then(|s| s.icon_url).expect("图标");
    assert!(url.starts_with("icon/") && url.ends_with(".png"));
    let registry = AssetRegistry::new();
    super::assets::register(&registry, &r.engine);
    assert!(registry.fetch(&url).is_some());

    // 平台取不到图标的应用只问一次
    r.port.apps.icon.store(false, Ordering::SeqCst);
    let mut raw = text_raw(103, "three");
    raw.source = Some(app("com.example.noicon"));
    r.copy(103, raw);
    let mut raw = text_raw(104, "four");
    raw.source = Some(app("com.example.noicon"));
    r.copy(104, raw);
    assert_eq!(r.port.apps.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn missing_files_are_greyed_out_in_the_page_itself() {
    let r = rig();
    let (item, [_, b]) = r.copy_files(101, ["a.txt", "b.txt"]);
    r.events();
    std::fs::remove_file(&b).expect("删文件");
    // 首页交出之前就检查完毕（本地 stat 远小于 5 ms 的时限）：页面本身已经置灰，不需要事件
    let page = r.engine.first_page();
    let files = page.items[0].item.files.clone().expect("files");
    assert_eq!(files.iter().map(|f| f.exists).collect::<Vec<_>>(), vec![true, false]);
    assert!(r.events().is_empty(), "结果随页面交出，不再推送");
    // 缓存同步更新；再次显示没有变化
    assert_eq!(r.top().files, Some(files.clone()));
    assert!(r.events().is_empty());
    // 翻页与检索（数据库返回的 exists 全是「未检查」）同样在交出之前检查
    let recent = r.engine.recent(0, 500, ClipFilter::All).expect("翻页");
    assert_eq!(recent.items[0].item.files, Some(files.clone()));
    let from_db = r.engine.recent(0, 1, ClipFilter::Files).expect("按类型翻页");
    assert_eq!(from_db.items[0].item.files, Some(files.clone()));
    // 置顶（数据库返回的 exists 全是「未检查」）不让已置灰的文件闪回正常
    let pinned = r.engine.pin(item.id, true).expect("置顶");
    assert_eq!(pinned.files, Some(files.clone()));
    assert_eq!(r.top().files, Some(files));
}

// ---------- 空闲清理 ----------

#[test]
fn idle_plan_waits_for_quiet_and_a_hidden_panel() {
    assert_eq!(plan(false, Duration::ZERO, false, QUIET), Plan::Sleep);
    assert_eq!(
        plan(true, Duration::from_secs(10), false, QUIET),
        Plan::Wait(QUIET - Duration::from_secs(10))
    );
    assert_eq!(plan(true, QUIET, true, QUIET), Plan::WaitPanel);
    assert_eq!(plan(true, QUIET, false, QUIET), Plan::Run);
    // 没活时面板可见与否都是 Sleep（面板隐藏另有通知唤醒，只用来检查过期）
    assert_eq!(plan(false, QUIET, true, QUIET), Plan::Sleep);
}

#[test]
fn cleanup_applies_the_retention_limit_and_sweeps_orphans() {
    let r = rig();
    let pinned = r.copy_text(100, "keep me").id;
    r.engine.pin(pinned, true).expect("置顶");
    for i in 0..6 {
        r.copy_text(101 + i, &format!("item {i}"));
    }
    r.set_settings(|s| s.clipboard.max_items = 3);
    let orphan = r
        .dir
        .path()
        .join("blobs")
        .join(BlobKind::Img.dir())
        .join(format!("{}.png", "0".repeat(32)));
    std::fs::write(&orphan, ICON_32).expect("孤儿文件");
    let before = r.ids();
    r.events();
    let report = r.engine.run_cleanup();
    assert_eq!(report.removed_items, 3);
    // 被保留策略删掉的行逐个推送 removed（视图不会显示马上就不存在的行）
    let removed: Vec<i64> = r
        .events()
        .into_iter()
        .filter_map(|e| match e {
            ClipboardChanged::Removed { id } => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(removed.len(), 3, "{removed:?}");
    let after = r.ids();
    assert!(removed.iter().all(|id| before.contains(id) && !after.contains(id)));
    assert!(report.orphans >= 1);
    assert!(!orphan.exists());
    let page = r.engine.first_page();
    assert_eq!(page.items.len(), 4, "3 条 + 置顶（置顶不计入上限）");
    assert_eq!(page.items[0].item.id, pinned);
    assert_eq!(page.items[1].item.preview, "item 5");
    // VACUUM 只在条件满足时记时间
    assert_eq!(report.vacuumed, r.host.last_vacuum.load(Ordering::SeqCst) != 0);
}

#[test]
fn cleanup_does_not_open_the_database_by_itself() {
    let r = rig();
    assert_eq!(r.engine.run_cleanup(), CleanupReport::default());
    assert!(r.engine.ready_store().is_none());
}

// ---------- 资源与杂项 ----------

#[test]
fn only_generated_key_shapes_are_served() {
    use super::assets::key_mime;
    let stem = "0123456789abcdef0123456789abcdef";
    assert_eq!(key_mime(BlobKind::Img, &format!("{stem}.png")), Some("image/png"));
    assert_eq!(key_mime(BlobKind::Thumb, &format!("{stem}.jpg")), Some("image/jpeg"));
    assert_eq!(key_mime(BlobKind::Ocr, &format!("{stem}.png")), Some("image/png"));
    for bad in [
        format!("{stem}.jpg"),
        format!("{stem}.PNG"),
        format!("{}.png", stem.to_uppercase()),
        format!("{stem}0.png"),
        format!("{stem}.png.png"),
        "x.png".to_owned(),
        stem.to_owned(),
    ] {
        assert_eq!(key_mime(BlobKind::Img, &bad), None, "{bad}");
    }
    assert_eq!(key_mime(BlobKind::Icon, &format!("{stem}.jpg")), None);
}

#[test]
fn paste_methods_and_toast_keys() {
    use super::paste::paste_method;
    assert_eq!(paste_method(OsKind::Macos), "cg_event");
    assert_eq!(paste_method(OsKind::Windows), "send_input");
    assert_eq!(paste_method(OsKind::Linux), "xtest");
    let r = rig();
    assert!(!r.engine.paste_key_label(None).is_empty());
}

/// 粗测（debug 构建、mock 平台，结果偏保守）：`cargo test -p wisp-desktop --lib clipboard_svc::tests::perf_probe -- --ignored --nocapture`
#[test]
#[ignore = "粗测耗时，手动运行"]
#[allow(clippy::print_stdout)] // 手动粗测：结果打印到标准输出，供记录
fn perf_probe() {
    let r = rig();
    r.host.set_view(ClipView::Hidden);
    let started = Instant::now();
    for i in 0..1000u64 {
        r.engine
            .ingest(&text_raw(
                i,
                &format!("probe item {i} {}", "lorem ipsum ".repeat((i % 20) as usize)),
            ))
            .expect("记录");
    }
    let ingest = started.elapsed() / 1000;
    let started = Instant::now();
    for _ in 0..100 {
        assert_eq!(r.engine.first_page().items.len(), 200);
    }
    let first_page = started.elapsed() / 100;
    let target = Some(app("com.example.target"));
    let ids: Vec<i64> = r.ids().into_iter().take(50).collect();
    let started = Instant::now();
    for id in &ids {
        r.reopen();
        r.paste(*id, PasteMode::Normal, target.clone()).expect("粘贴");
    }
    let paste = started.elapsed() / u32::try_from(ids.len()).unwrap_or(1);
    let started = Instant::now();
    let found = r.engine.search("ipsum", ClipFilter::All, 200).expect("检索");
    let search = started.elapsed();

    // 首页里有 64 个文件条目（各 2 个真实文件）：交出之前的 exists 检查在另一个线程上 stat，调用方等结果
    struct ThreadExec;
    impl Exec for ThreadExec {
        fn blocking(&self, job: Job) {
            std::thread::spawn(job);
        }
        fn after(&self, _delay: Duration, _job: Job) {}
    }
    let log: Log = Arc::default();
    let dir = tempfile::tempdir().expect("临时目录");
    let host = mock_host(&log);
    host.set_view(ClipView::Hidden);
    let files_engine = Engine::new(
        mock_port(&log) as Arc<dyn Port>,
        host as Arc<dyn Host>,
        Arc::new(ThreadExec),
        OsKind::current(),
        dir.path().to_path_buf(),
        false,
    );
    for i in 0..64u64 {
        let paths: Vec<String> = (0..2)
            .map(|k| {
                let p = dir.path().join(format!("f{i}-{k}.txt"));
                std::fs::write(&p, b"x").expect("写文件");
                p.to_string_lossy().into_owned()
            })
            .collect();
        let raw = RawClipboard {
            change_id: i,
            files: Some(paths),
            ..RawClipboard::default()
        };
        files_engine.ingest(&raw).expect("记录");
    }
    let started = Instant::now();
    for _ in 0..100 {
        assert_eq!(files_engine.first_page().items.len(), 64);
    }
    let files_page = started.elapsed() / 100;
    println!(
        "perf_probe (debug): ingest {ingest:?}/item, first_page(200) {first_page:?}, paste core {paste:?}, \
         search {search:?} ({} hits), first_page(64 file items, exists checked) {files_page:?}",
        found.items.len()
    );
}

// ---------- 真实平台的端到端（手动运行） ----------

/// 用外部进程写系统剪贴板（只有 macOS 的 pbcopy；没有这个命令时返回 false，测试跳过）
fn external_copy(text: &str) -> bool {
    use std::io::Write;
    let Ok(mut child) = std::process::Command::new("pbcopy")
        .stdin(std::process::Stdio::piped())
        .spawn()
    else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    child.wait().is_ok_and(|s| s.success())
}

pub(super) async fn wait_until(mut f: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    f()
}

/// 真实剪贴板上的「监听 → 读取 → 记录」、自身写入去重、暂停与恢复。会临时改写系统剪贴板（先快照、结束时恢复）：
/// `cargo test -p wisp-desktop --lib clipboard_svc::tests::live_ -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "需要真实剪贴板（会临时改写并恢复系统剪贴板）"]
#[allow(clippy::print_stderr)] // 真机手工用例：打印观察到的入库耗时，供记录
async fn live_watch_records_external_copies_and_skips_own_writes() {
    let platform = wisp_platform::Platform::create(wisp_platform::PlatformConfig {
        own_pid: std::process::id(),
        main_thread: Arc::new(|f: Box<dyn FnOnce() + Send>| f()),
        self_hit: Arc::new(|_, _| false),
        keymaps: Arc::new(RwLock::new(Vec::new())),
        os_version: String::new(),
    });
    let snapshot = platform
        .clipboard
        .snapshot(64 * 1024 * 1024, Duration::from_secs(2))
        .expect("快照");
    let dir = tempfile::tempdir().expect("临时目录");
    let host = rig().host;
    let engine = Engine::new(
        Arc::clone(&platform) as Arc<dyn Port>,
        Arc::clone(&host) as Arc<dyn Host>,
        Arc::new(TestExec::default()),
        OsKind::current(),
        dir.path().to_path_buf(),
        false,
    );
    engine.apply_settings_now(&Settings::default().clipboard);
    engine.reconcile_watch().expect("安装监听");
    let worker = tokio::spawn(super::watch::run(Arc::clone(&engine)));
    let nonce = std::process::id();
    let count = |e: &Engine, needle: &str| {
        e.first_page()
            .items
            .iter()
            .filter(|h| h.item.preview.contains(needle))
            .count()
    };

    let external = format!("wisp live external {nonce}");
    if !external_copy(&external) {
        eprintln!("没有 pbcopy：跳过");
        return;
    }
    let started = Instant::now();
    assert!(
        wait_until(|| count(&engine, &external) == 1, Duration::from_secs(3)).await,
        "外部复制应被记录"
    );
    let latency = started.elapsed();

    let own = format!("wisp live own {nonce}");
    let e = Arc::clone(&engine);
    let own_text = own.clone();
    tokio::task::spawn_blocking(move || e.copy_text(&own_text))
        .await
        .expect("任务")
        .expect("copy_text");
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(count(&engine, &own), 1, "自身写入只记一次（监听方因 from_wisp 跳过）");
    let own_meta = engine.first_page().items[0].item.clone();
    assert_eq!(own_meta.use_count, 1);

    engine.set_paused(true);
    let while_paused = format!("wisp live paused {nonce}");
    assert!(external_copy(&while_paused));
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(count(&engine, &while_paused), 0, "暂停期间不记录");
    engine.set_paused(false);
    let resumed = format!("wisp live resumed {nonce}");
    assert!(external_copy(&resumed));
    assert!(
        wait_until(|| count(&engine, &resumed) == 1, Duration::from_secs(3)).await,
        "恢复后照常记录"
    );
    assert_eq!(count(&engine, &while_paused), 0, "暂停期间的复制不补录");

    worker.abort();
    engine.set_paused(true);
    if let Some(snap) = snapshot {
        platform.clipboard.restore(snap).expect("恢复剪贴板");
    }
    eprintln!("live: external copy recorded in {latency:?}");
}

// ---------- OCR 历史 ----------

fn ocr_record(text: &str, image: Option<&[u8]>, replaces: Option<i64>) -> super::ocr_history::OcrRecord {
    super::ocr_history::OcrRecord {
        mode: OcrMode::Lines,
        model: "Fake Vision".to_owned(),
        text: text.to_owned(),
        image: image.map(|b| Arc::new(b.to_vec())),
        thumb: None,
        replaces,
    }
}

#[test]
fn ocr_history_is_off_by_default_and_records_when_enabled() {
    let r = rig();
    // 默认关闭：什么都不写、不推送，也不为它打开数据库
    assert!(!Settings::default().ocr.history);
    assert_eq!(
        r.engine
            .ocr_history_record(&ocr_record("hello", None, None))
            .expect("记录"),
        None
    );
    assert!(r.host.ocr_events.lock().is_empty());
    assert!(r.engine.ready_store().is_none());

    r.set_settings(|s| s.ocr.history = true);
    // 空白结果不记
    assert_eq!(
        r.engine
            .ocr_history_record(&ocr_record("  \n", None, None))
            .expect("记录"),
        None
    );
    let id = r
        .engine
        .ocr_history_record(&ocr_record("识别结果", Some(ICON_64), None))
        .expect("记录")
        .expect("已写入");
    let events = std::mem::take(&mut *r.host.ocr_events.lock());
    match events.as_slice() {
        [OcrHistoryChanged::Inserted { item }] => {
            assert_eq!(item.id, id);
            assert_eq!(item.text, "识别结果");
            assert_eq!(item.model, "Fake Vision");
            assert_eq!(item.mode, OcrMode::Lines);
            // 原图原样保存、缩略图由存储层从原图生成，都给出资源路径
            assert!(item.image_url.as_deref().is_some_and(|u| u.starts_with("ocrimg/")));
            assert!(item.thumb_url.as_deref().is_some_and(|u| u.starts_with("ocrimg/")));
        }
        other => panic!("{other:?}"),
    }
    let store = r.engine.ready_store().expect("已打开");
    assert_eq!(store.ocr_history_list(0, 10).expect("列表").len(), 1);
}

#[test]
fn ocr_history_trims_to_the_limit_and_replaces_re_recognitions() {
    let r = rig();
    r.set_settings(|s| {
        s.ocr.history = true;
        s.ocr.history_limit = 10;
    });
    let ids: Vec<i64> = (0..10)
        .map(|i| {
            r.engine
                .ocr_history_record(&ocr_record(&format!("t{i}"), None, None))
                .expect("记录")
                .expect("已写入")
        })
        .collect();
    r.host.ocr_events.lock().clear();
    // 第 11 条：最旧的一条被裁掉，先推 inserted 再推 removed
    let newest = r
        .engine
        .ocr_history_record(&ocr_record("t10", None, None))
        .expect("记录")
        .expect("已写入");
    let events = std::mem::take(&mut *r.host.ocr_events.lock());
    assert!(
        matches!(&events[0], OcrHistoryChanged::Inserted { item } if item.id == newest),
        "{events:?}"
    );
    assert_eq!(events[1], OcrHistoryChanged::Removed { ids: vec![ids[0]] });
    assert_eq!(events.len(), 2);

    // 同一张截图、同一模式重新识别：新结果取代上一条
    let again = r
        .engine
        .ocr_history_record(&ocr_record("t10 better", None, Some(newest)))
        .expect("记录")
        .expect("已写入");
    let events = std::mem::take(&mut *r.host.ocr_events.lock());
    assert!(
        matches!(&events[0], OcrHistoryChanged::Inserted { item } if item.id == again),
        "{events:?}"
    );
    assert_eq!(events[1], OcrHistoryChanged::Removed { ids: vec![newest] });
    let store = r.engine.ready_store().expect("已打开");
    let list = store.ocr_history_list(0, 100).expect("列表");
    assert_eq!(list.len(), 10);
    assert_eq!(list[0].text, "t10 better");

    // 调小上限后由空闲清理补裁，同样推送 removed
    assert_eq!(r.engine.ocr_history_trim(&store, 8), 2);
    assert!(matches!(
        std::mem::take(&mut *r.host.ocr_events.lock()).as_slice(),
        [OcrHistoryChanged::Removed { ids }] if ids.len() == 2
    ));
}

#[test]
fn ocr_history_delete_and_clear_push_changes() {
    let r = rig();
    r.set_settings(|s| s.ocr.history = true);
    let a = r
        .engine
        .ocr_history_record(&ocr_record("a", None, None))
        .expect("记录")
        .expect("已写入");
    r.engine
        .ocr_history_record(&ocr_record("b", None, None))
        .expect("记录")
        .expect("已写入");
    r.host.ocr_events.lock().clear();
    r.engine.ocr_history_delete(a).expect("删除");
    // 已经不存在的 id：不推送
    r.engine.ocr_history_delete(a).expect("删除");
    r.engine.ocr_history_clear().expect("清空");
    assert_eq!(
        std::mem::take(&mut *r.host.ocr_events.lock()),
        vec![OcrHistoryChanged::Removed { ids: vec![a] }, OcrHistoryChanged::Cleared]
    );
}

// ---------- 正在运行的应用 ----------

#[test]
fn running_apps_are_deduped_sorted_and_get_icons() {
    let r = rig();
    let named = |id: &str, name: &str| AppRef {
        name: name.to_owned(),
        ..app(id)
    };
    *r.port.apps.running.lock() = vec![
        named("com.example.zeta", "Zeta"),
        named(SELF_ID, "Wisp"),
        named("com.example.alpha", " Alpha "),
        named("COM.EXAMPLE.ALPHA", "Alpha (dup)"),
        named("  ", "blank"),
        named("com.example.noname", ""),
    ];
    // 库还没打开：不为图标打开它，图标一律为 null
    let apps = r.engine.apps_running();
    assert_eq!(
        apps.iter()
            .map(|a| (a.id.as_str(), a.name.as_str()))
            .collect::<Vec<_>>(),
        [
            ("com.example.alpha", "Alpha"),
            ("com.example.noname", "com.example.noname"),
            ("com.example.zeta", "Zeta"),
        ]
    );
    assert!(apps.iter().all(|a| a.icon_url.is_none()));
    assert!(r.engine.ready_store().is_none());

    // 库已打开：没缓存的图标向平台取一次并落盘，第二次直接命中缓存
    r.engine.open_store("test").expect("打开");
    let before = r.port.apps.calls.load(Ordering::SeqCst);
    let apps = r.engine.apps_running();
    assert!(
        apps.iter()
            .all(|a| a.icon_url.as_deref().is_some_and(|u| u.starts_with("icon/")))
    );
    assert_eq!(r.port.apps.calls.load(Ordering::SeqCst) - before, 3);
    let again = r.engine.apps_running();
    assert_eq!(again, apps);
    assert_eq!(
        r.port.apps.calls.load(Ordering::SeqCst) - before,
        3,
        "命中缓存，不再向平台取"
    );

    // 平台取不到图标的应用记一笔，本进程内不再重取
    r.port.apps.icon.store(false, Ordering::SeqCst);
    r.port.apps.running.lock().push(named("com.example.noicon", "No Icon"));
    let calls = r.port.apps.calls.load(Ordering::SeqCst);
    let apps = r.engine.apps_running();
    assert!(
        apps.iter()
            .any(|a| a.id == "com.example.noicon" && a.icon_url.is_none())
    );
    r.engine.apps_running();
    assert_eq!(r.port.apps.calls.load(Ordering::SeqCst) - calls, 1);
}
