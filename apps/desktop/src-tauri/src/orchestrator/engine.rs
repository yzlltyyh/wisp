//! 编排器的核心：四条流程（翻译、输入、OCR、直接翻译）与剪贴板面板的打开、前端发起的请求、截图会话、会话收尾。
//!
//! 只经 [`Env`] 产生副作用；阻塞调用一律放进 `spawn_blocking`（运行时只有 2 个 worker）。
//!
//! **关键时序**
//! - 请求与面板显示并行：读 Key 不会弹系统对话框时，触发时就在阻塞线程上预读（[`KeyFetch`]，与取词 /
//!   框选并行）；拿到原文后立即登记请求并在后台发出（`llm_stream`），同时在阻塞线程上 `panel_prepare`；
//!   `TranslatePayload.request_id` 让视图 `stream_attach` 回放并接上实时事件。可能弹框（macOS 未签名构建更新后第一次
//!   访问、Linux keyring 被锁）时推迟到真正要用的时候：缓存未命中之后；截图在 Overlay 拆完之后（见 [`KeySource`]）。
//!   读 Key 最多等 [`KEY_WAIT`]，对话框没人回答时以可重试的 `Error{keychain}` 结束，不让面板一直停在骨架态。
//! - 重入：新的触发让**还没拿到面板会话**的 run 一律失效（不论阶段，见 [`super::fsm`]）。
//!   `panel_prepare` 在准备闸门（`prepare_gate`）里串行执行，并在同一个临界区里把会话记到 run 上：被作废的 run 要么根本
//!   不 prepare，要么在新 run 之前 prepare（随即被新会话替换），「以最新一次为准」由此严格成立。面板上的 run 在新会话
//!   替换面板、旧会话结束时经 `panel_hidden` 收尾，进行中的请求随之取消。
//! - 屏幕栅栏（[`ScreenGate`]）：被取代的 Overlay 的拆除（交还焦点 → 销毁 → 恢复 pinned）与截图段（隐藏 → 截图 →
//!   建 Overlay）按顺序执行；新流程在读前台应用 / 取词 / 显示面板之前先等它们做完。commit 之后的 Overlay 从 commit
//!   起就登记在栅栏上（[`ScreenGate::hold`]），飞入期间来了新的触发时由 `start_run` 立即拆掉，不等 `overlay_done`。
//! - 到达顺序：每个触发在到达时取得序号并开始埋点（[`Trigger`]）；引导窗口存在时要先在阻塞线程上确认它
//!   是否在前台，几次确认可能乱序完成——比已经处理过的触发更早到达的一律丢弃。
//! - 会话收尾：每次收到 `panel_hidden` 通知、以及每次登记新的会话之后，对登记过的**全部**面板
//!   会话调用 `panel_session_live`，结束了的：取消并移除它的请求、释放它的截图、结束 run 与埋点。已经转给 pinned 的
//!   请求不受影响。
//! - 截图：热键处理的第一件事 `prewarm()` → 隐藏自己的窗口 → `capture_all` → 建 Overlay →（macOS）
//!   带原生冻结帧立即显示，之后通知页面 `overlay:session` 重新 init /（Windows、X11）页面首帧画完 `overlay_ready` 后
//!   显示 → commit：裁剪、在 Overlay 下面准备并显示面板、后台编码并发出请求、返回飞入目标 → `overlay_done`：确认面板
//!   已显示，下一帧销毁 Overlay、交还焦点→ 每条结束路径都 `restore_after_capture`。

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::watch;
use wisp_llm::{CancellationToken, Secret};
use wisp_types::{
    AppRef, ErrorCode, HotkeyAction, LlmError, NotReady, OverlayEndReason, OverlayMode, RequestId, StreamEvent,
};

use super::cache::TranslationCache;
use super::captures::{CaptureStore, Crop};
use super::env::Env;
use super::fsm::{self, Event, Flow, Stage, Transition};
use super::not_ready::KeyIndex;
use super::ocr::{FocusReturn, OverlayState};
use super::plan::{Resolved, Throughput};
use super::registry::{Owner, PumpEnd, Registry, Sink, Tags};
use super::{RequestCaller, RequestHost};
use crate::app::perf::PerfStage;
use crate::bus::{Command, TriggerOrigin};
use crate::error::IpcError;
use crate::windows::PanelShowRequest;

/// 等面板显示（Shown / ready 超时）的上限：之后照常结束埋点 / 销毁 Overlay
const SHOWN_WAIT: Duration = Duration::from_secs(2);
/// 新流程等屏幕栅栏的上限（截图段通常约 300 ms 隐藏 + 截图；X11 硬超时 5 s 时不让新流程一直等）
const SETTLE_WAIT: Duration = Duration::from_secs(2);
/// 读 Key 的上限：系统对话框（macOS 钥匙串授权、Linux keyring 解锁）一直没人回答时，请求以可重试的
/// `Error{keychain}` 结束，不让面板无提示地停在骨架态（阻塞的读取在后台照常结束，成功的话下次就不会再弹框）
pub(super) const KEY_WAIT: Duration = if cfg!(test) {
    Duration::from_millis(400)
} else {
    Duration::from_secs(10)
};
/// 超过这个字节数的原文，CPU 工作（语言检测、术语表匹配、缓存键、提示词装配）放进阻塞线程池，不占 2 个 tokio worker
pub(super) const HEAVY_TEXT_BYTES: usize = 64 * 1024;

/// 原文来源
#[derive(Debug, Clone)]
pub enum Source {
    /// 读取当前选区
    Selection,
    /// 触发本身带着文本（剪贴板条目送翻译、引导页「试一试」、冒烟）
    Text(String),
}

/// 一次触发。到达时（[`Core::arrive`]）就取得序号、开始埋点并记下时刻：之后的任何等待（例如确认引导窗口是否在
/// 前台）都算进触发到显示的延迟预算，「已发出」日志的 +ms 也从这里算起
#[derive(Debug, Clone, Copy)]
pub(super) struct Trigger {
    /// 到达序号（[`Core::claim`]：以最新一次为准）
    pub(super) seq: u64,
    pub(super) origin: TriggerOrigin,
    pub(super) trace: u64,
    pub(super) started: Instant,
}

/// 一次 Rust 发起的流程
pub(super) struct Run {
    pub(super) flow: Flow,
    pub(super) stage: Stage,
    pub(super) trace: u64,
    /// `panel_prepare` 交给它的面板会话；None = 面板还没出现（新的触发会作废它）
    pub(super) session: Option<u64>,
    pub(super) request: Option<RequestId>,
}

#[derive(Default)]
pub(super) struct Runs {
    pub(super) next: u64,
    pub(super) map: HashMap<u64, Run>,
}

/// 屏幕栅栏：改动屏幕与焦点的阻塞段按顺序执行——截图会话的「隐藏自己的窗口 → 截图 → 建 Overlay → 首次显示」
/// （[`Self::exclusive`]）与 Overlay 的拆除（[`Self::spawn`]）。新流程在读前台应用 / 取词 / 显示面板之前调用
/// [`Self::settled`]：被取代的 Overlay 还是 key 窗口、焦点还没还给原应用时取词会读错应用；旧截图的隐藏晚于新面板的
/// prepare 时，会把新面板当作「截图前取消」结束掉。
///
/// 拆除在决定拆的那一刻（例如 `start_run` 里，同步）登记计数，之后启动的新流程一定看得到它；commit 之后的 Overlay
/// 在 commit 时就登记（[`Self::hold`]），直到它真正拆完。空闲时都是零开销。
pub(super) struct ScreenGate {
    lock: Arc<tokio::sync::Mutex<()>>,
    /// 已登记、还没做完的拆除
    pending: watch::Sender<u32>,
}

impl ScreenGate {
    fn new() -> Self {
        Self {
            lock: Arc::new(tokio::sync::Mutex::new(())),
            pending: watch::channel(0).0,
        }
    }

    /// 等登记的拆除做完（最多 [`SETTLE_WAIT`]）
    pub(super) async fn wait_pending(&self) {
        let mut rx = self.pending.subscribe();
        let idle = tokio::time::timeout(SETTLE_WAIT, async move { rx.wait_for(|n| *n == 0).await.is_ok() }).await;
        if idle.is_err() {
            log::warn!("screen gate: a teardown is still running after {SETTLE_WAIT:?}; continuing");
        }
    }

    /// 新流程：等登记的拆除与进行中的截图段做完（最多 [`SETTLE_WAIT`]，超时照常继续并记日志）
    pub(super) async fn settled(&self) {
        self.wait_pending().await;
        if tokio::time::timeout(SETTLE_WAIT, self.lock.lock()).await.is_err() {
            log::warn!("screen gate: a capture is still running after {SETTLE_WAIT:?}; continuing");
        }
    }

    /// 截图段：先等登记的拆除做完（拆除销毁的是全部 Overlay，必须在新会话建 Overlay 之前做完），再独占
    pub(super) async fn exclusive(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.wait_pending().await;
        Arc::clone(&self.lock).lock_owned().await
    }

    /// 登记一项还要过一会儿才开始的拆除（commit 之后的 Overlay：拆除要等飞入结束的 `overlay_done`，但从 commit 起
    /// 新流程就得等它）。返回的登记在 drop 时撤销
    pub(super) fn hold(&self) -> ScreenHold {
        self.pending.send_modify(|n| *n += 1);
        ScreenHold(self.pending.clone())
    }

    /// 登记并在阻塞线程上执行一次拆除（排在进行中的截图段之后）；返回可等待的句柄
    pub(super) fn spawn(&self, work: impl FnOnce() + Send + 'static) -> tauri::async_runtime::JoinHandle<()> {
        self.pending.send_modify(|n| *n += 1);
        let lock = Arc::clone(&self.lock);
        let pending = self.pending.clone();
        tauri::async_runtime::spawn(async move {
            let guard = lock.lock_owned().await;
            blocking(work).await;
            drop(guard);
            pending.send_modify(|n| *n = n.saturating_sub(1));
        })
    }
}

/// 屏幕栅栏上的一项登记（[`ScreenGate::hold`]）：存在期间 [`ScreenGate::settled`] 一直等（最多 `SETTLE_WAIT`），
/// drop 时撤销——随拆除的窗口操作一起移进阻塞线程，做完才 drop
pub(super) struct ScreenHold(watch::Sender<u32>);

impl Drop for ScreenHold {
    fn drop(&mut self) {
        self.0.send_modify(|n| *n = n.saturating_sub(1));
    }
}

/// 预读的 Key（把钥匙串访问移出「面板可见 → 首 token」的关键路径）。触发时就在阻塞线程上开始读——划词时与取词
/// 并行、截图时与框选并行——请求真正发出时多半已经读好。读到的 Secret 只用于这一次请求，不缓存（Key 随时可能被改）
pub(super) struct KeyFetch {
    provider_id: String,
    handle: tauri::async_runtime::JoinHandle<Result<Option<Secret>, IpcError>>,
}

impl KeyFetch {
    /// 等读取结束；None = 阻塞任务失败
    async fn join(self) -> Option<Result<Option<Secret>, IpcError>> {
        match self.handle.await {
            Ok(r) => Some(r),
            Err(e) => {
                log::error!("orchestrator key read failed: {e}");
                None
            }
        }
    }
}

/// 请求用的 Key 从哪里来
pub(super) enum KeySource {
    /// 现在读（阻塞线程）
    Read,
    /// 触发时已开始预读（Provider 对不上时作废、重新读）
    Prefetched(KeyFetch),
    /// 已经读好（OCR 两次调用模式的第二步、同一个 Provider）
    Known(Option<Secret>),
}

impl From<Option<KeyFetch>> for KeySource {
    fn from(k: Option<KeyFetch>) -> Self {
        k.map_or(Self::Read, Self::Prefetched)
    }
}

/// 编排器核心
pub struct Core {
    pub(super) env: Arc<dyn Env>,
    pub(super) registry: Arc<Registry>,
    pub(super) cache: TranslationCache,
    pub(super) captures: Arc<CaptureStore>,
    pub(super) runs: Mutex<Runs>,
    pub(super) overlay: Mutex<Option<OverlayState>>,
    pub(super) next_overlay: AtomicU64,
    pub(super) speed: Mutex<Throughput>,
    /// 准备闸门：编排器发起的 `panel_prepare` 串行执行，「核对 run 仍有效 → prepare → 把会话记到 run 上」在同一个临界区
    /// （只在阻塞线程上持有）
    pub(super) prepare_gate: Mutex<()>,
    pub(super) screen: ScreenGate,
    /// 本进程里已经成功读过 Key 的 Provider：再读不会弹系统对话框（[`Core::key_read_is_quiet`]）
    pub(super) quiet_keys: Arc<Mutex<HashSet<String>>>,
    /// 触发的到达序号（[`Core::arrive`]）与已经处理过的最大序号（[`Core::claim`]）
    pub(super) arrivals: AtomicU64,
    pub(super) claimed: AtomicU64,
    /// 测试用：commit 之后编码的额外耗时（模拟大选区在 debug 构建下的编码）
    #[cfg(test)]
    pub(super) encode_delay: Mutex<Duration>,
    /// 指向自己（只有 `&self` 的地方需要派发后台任务时用）
    pub(super) me: Weak<Core>,
}

/// 阻塞调用放进阻塞线程池；线程池任务失败（panic 被捕获）时为 None
pub(super) async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    match tauri::async_runtime::spawn_blocking(f).await {
        Ok(v) => Some(v),
        Err(e) => {
            log::error!("orchestrator blocking task failed: {e}");
            None
        }
    }
}

/// CPU 工作：`heavy`（超长原文）时放进阻塞线程池，否则就地执行。None = 阻塞任务失败
pub(super) async fn cpu<T: Send + 'static>(heavy: bool, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    if heavy { blocking(f).await } else { Some(f()) }
}

pub(super) struct EnvKeys<'a>(&'a dyn Env);

impl KeyIndex for EnvKeys<'_> {
    fn keychain_state(&self) -> wisp_types::KeychainState {
        self.0.keychain_state()
    }
    fn has_key(&self, provider_id: &str) -> bool {
        self.0.has_key(provider_id)
    }
}

/// 未就绪 → 前端发起的请求里的错误（这类请求没有载荷可放 `not_ready`，以 ErrorCard 呈现）
pub(super) fn not_ready_error(nr: &NotReady) -> LlmError {
    match nr {
        NotReady::KeychainUnavailable => LlmError::new(ErrorCode::Keychain, "the system keychain is unavailable"),
        NotReady::NoVisionModel => LlmError::new(ErrorCode::NotConfigured, "no vision model is configured"),
        _ => LlmError::new(ErrorCode::NotConfigured, "no model is configured"),
    }
}

/// 读 Key 失败（拒绝授权、钥匙串锁定）→ `Error{keychain, retryable}`
pub(super) fn keychain_error(e: &IpcError) -> LlmError {
    let mut err = LlmError::new(ErrorCode::Keychain, e.message.clone());
    err.retryable = true;
    err
}

pub(super) fn owner_of(host: &RequestHost) -> Owner {
    match host {
        RequestHost::Panel { session_id } => Owner::Panel(*session_id),
        RequestHost::Pinned { pinned_id } => Owner::Pinned(pinned_id.clone()),
    }
}

/// 请求「已发出」的日志（`llm_stream` 返回之后打印；`trace` 与 `perf …` 行对得上，`+ms` 与其中的 `shown_ms` 同一起点）
pub(super) fn log_dispatched(id: &str, trace: Option<u64>, started: Option<Instant>, resolved: &Resolved) {
    match (trace, started) {
        (Some(trace), Some(t0)) => log::info!(
            "request {id} dispatched trace={trace} at +{} ms model={}/{}",
            t0.elapsed().as_millis(),
            resolved.provider.id,
            resolved.model.id
        ),
        _ => log::info!(
            "request {id} dispatched model={}/{}",
            resolved.provider.id,
            resolved.model.id
        ),
    }
}

/// [`Core::prepare_for`] 的结果
enum Prepared {
    /// 会话已交给 run
    Shown(u64),
    /// run 在 prepare 之前或期间被新的触发作废；Some = prepare 已经做了（这个会话随即被新 run 的 prepare 替换）
    Superseded(Option<u64>),
    /// 找不到任何显示器，或阻塞任务失败
    Failed,
}

impl Core {
    pub fn new(env: Arc<dyn Env>) -> Arc<Self> {
        Arc::new_cyclic(|weak: &Weak<Self>| {
            let me = weak.clone();
            let weak = weak.clone();
            let registry = Registry::new(Box::new(move |tags: Tags| {
                if let Some(core) = weak.upgrade() {
                    core.on_first_delta(tags);
                }
            }));
            Self {
                env,
                registry: Arc::new(registry),
                cache: TranslationCache::new(),
                captures: CaptureStore::new(),
                runs: Mutex::new(Runs::default()),
                overlay: Mutex::new(None),
                next_overlay: AtomicU64::new(0),
                speed: Mutex::new(Throughput::default()),
                prepare_gate: Mutex::new(()),
                screen: ScreenGate::new(),
                quiet_keys: Arc::new(Mutex::new(HashSet::new())),
                arrivals: AtomicU64::new(0),
                claimed: AtomicU64::new(0),
                #[cfg(test)]
                encode_delay: Mutex::new(Duration::ZERO),
                me,
            }
        })
    }

    pub(super) fn keys(&self) -> EnvKeys<'_> {
        EnvKeys(&*self.env)
    }

    // ------------------------------------------------------------------------------------------------
    // 启动：总线消费循环与会话结束的订阅

    /// 启动命令总线的消费循环与面板会话结束的订阅（事件驱动，空闲时没有任何唤醒）
    pub fn spawn(self: &Arc<Self>, mut rx: UnboundedReceiver<Command>) {
        let core = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                core.dispatch(cmd);
            }
        });
        match self.env.panel_hidden() {
            Some(mut hidden) => {
                let core = Arc::clone(self);
                tauri::async_runtime::spawn(async move {
                    while hidden.changed().await.is_ok() {
                        core.reconcile();
                    }
                });
            }
            None => log::warn!("orchestrator: panel_hidden is unavailable; sessions are reconciled on demand"),
        }
    }

    /// 分发一条命令（不阻塞：每个流程在自己的任务里执行）
    pub fn dispatch(self: &Arc<Self>, cmd: Command) {
        log::debug!("orchestrator command {cmd:?}");
        match cmd {
            Command::Action { action, origin } => self.on_action(action, self.arrive(origin), None),
            Command::TranslateWithStyle { style_id, origin } => {
                self.on_action(HotkeyAction::TranslateSelection, self.arrive(origin), Some(style_id));
            }
            Command::TranslateText { text, origin } => {
                let t = self.arrive(origin);
                if self.claim(t.seq) {
                    self.start_translate(t, Source::Text(text), None);
                }
            }
            Command::OpenWindow(target) => self.env.open_window(target),
            Command::ToggleClipboardPause => self.env.toggle_clipboard_pause(),
            Command::ClearClipboard { keep_pinned } => self.env.clear_clipboard(keep_pinned),
            Command::CheckUpdate => self.env.check_update(),
            Command::Quit => self.env.quit(),
        }
    }

    /// 触发到达：取得序号、开始埋点、记下时刻
    pub(super) fn arrive(&self, origin: TriggerOrigin) -> Trigger {
        Trigger {
            seq: self.arrivals.fetch_add(1, Ordering::SeqCst) + 1,
            origin,
            trace: self.env.perf().begin(origin),
            started: Instant::now(),
        }
    }

    /// 处理这个触发之前调用：已经处理过更晚到达的触发时返回 false（这次作废，以最新一次为准）
    pub(super) fn claim(&self, seq: u64) -> bool {
        self.claimed.fetch_max(seq, Ordering::SeqCst) < seq
    }

    /// 动作入口：截图类先预热；引导窗口在前台时热键转成 `onboarding:hotkey`
    pub(super) fn on_action(self: &Arc<Self>, action: HotkeyAction, t: Trigger, style: Option<String>) {
        if matches!(action, HotkeyAction::Ocr | HotkeyAction::OcrTranslate) {
            // 第一件事：在隐藏窗口、等窗口消失之前，让截图后端提前做与内容无关的准备
            self.env.capture_prewarm();
        }
        let from_input_device = matches!(
            t.origin,
            TriggerOrigin::Hotkey | TriggerOrigin::Bobber | TriggerOrigin::DoubleTap | TriggerOrigin::MouseButton
        );
        if from_input_device && self.env.onboarding_open() {
            // 窗口焦点要等主线程：放到阻塞线程上查。连按时几次查询可能乱序完成，按到达序号只处理最新的那次
            let core = Arc::clone(self);
            tauri::async_runtime::spawn(async move {
                let env = Arc::clone(&core.env);
                let focused = blocking(move || env.onboarding_focused()).await.unwrap_or(false);
                if !core.claim(t.seq) {
                    log::debug!("hotkey {} dropped: a later trigger was handled first", action.as_str());
                    return;
                }
                if focused {
                    log::info!("hotkey {} forwarded to the onboarding window", action.as_str());
                    core.env.emit_onboarding_hotkey(action);
                    return;
                }
                core.route_action(action, t, style);
            });
            return;
        }
        if self.claim(t.seq) {
            self.route_action(action, t, style);
        }
    }

    pub(super) fn route_action(self: &Arc<Self>, action: HotkeyAction, t: Trigger, style: Option<String>) {
        match action {
            HotkeyAction::TranslateSelection => self.start_translate(t, Source::Selection, style),
            HotkeyAction::TranslateInput => self.start_input(t),
            HotkeyAction::Ocr => self.start_capture(OverlayMode::Ocr, t),
            HotkeyAction::OcrTranslate => self.start_capture(OverlayMode::OcrTranslate, t),
            HotkeyAction::Clipboard => self.start_clipboard(t),
        }
    }

    // ------------------------------------------------------------------------------------------------
    // run 的登记与状态机

    /// 登记一个新的 run；还没拿到面板会话的旧 run（不论阶段）被作废（以最新一次为准）。
    /// 被作废的截图会话、以及 commit 之后还在飞入 / 淡出的 Overlay，都在这里同步登记拆除（[`ScreenGate`]），新流程随后
    /// 会等它们做完（「Overlay 显示中按其他动作 = 先拆完 Overlay 再执行」）
    pub(super) fn start_run(self: &Arc<Self>, flow: Flow, trace: u64) -> u64 {
        let (id, superseded) = {
            let mut g = self.runs.lock();
            g.next += 1;
            let id = g.next;
            let mut superseded = Vec::new();
            for (rid, r) in &mut g.map {
                if r.session.is_some() {
                    continue;
                }
                if let Transition::Next(s) = fsm::step(r.flow, r.stage, Event::Retrigger) {
                    log::debug!("run {rid} {:?}: {:?} --Retrigger--> {s:?}", r.flow, r.stage);
                    r.stage = s;
                    superseded.push(*rid);
                }
            }
            let gone: Vec<Run> = superseded.iter().filter_map(|rid| g.map.remove(rid)).collect();
            g.map.insert(
                id,
                Run {
                    flow,
                    stage: fsm::initial(flow),
                    trace,
                    session: None,
                    request: None,
                },
            );
            (id, gone)
        };
        for run in superseded {
            self.settle(run);
        }
        // commit 之后的 Overlay（飞入 / 淡出中，等 `overlay_done`）：不等页面，立即拆掉（销毁 → 交还焦点 → 恢复 pinned）。
        // 否则新流程会在 Overlay 仍是 key 窗口时读前台应用与选区，随后到达的 overlay_done 交还焦点时还会抢走新面板的焦点
        let committed = {
            let mut g = self.overlay.lock();
            if g.as_ref().is_some_and(|o| o.committed) {
                g.take()
            } else {
                None
            }
        };
        if let Some(st) = committed {
            log::debug!(
                "overlay session {} torn down early: run {id} started during the fly-in",
                st.session_id
            );
            self.teardown_overlay(st, FocusReturn::AfterDestroy, OverlayEndReason::Done);
        }
        log::debug!("run {id} {flow:?} started trace={trace}");
        id
    }

    /// 推进状态机：合法转移返回新阶段；run 已结束（Ignore）或非法转移时返回 None（调用方据此停止）
    pub(super) fn advance(&self, run: u64, ev: Event) -> Option<Stage> {
        let (next, settled) = {
            let mut g = self.runs.lock();
            let r = g.map.get_mut(&run)?;
            match fsm::step(r.flow, r.stage, ev) {
                Transition::Next(s) => {
                    log::debug!("run {run} {:?}: {:?} --{ev:?}--> {s:?}", r.flow, r.stage);
                    r.stage = s;
                    let settled = s.is_terminal().then(|| g.map.remove(&run)).flatten();
                    (Some(s), settled)
                }
                Transition::Ignore => (None, None),
                Transition::Illegal => {
                    let (flow, stage) = (r.flow, r.stage);
                    drop(g);
                    log::error!("run {run} {flow:?}: illegal transition {stage:?} --{ev:?}-->");
                    debug_assert!(false, "illegal transition {flow:?} {stage:?} --{ev:?}-->");
                    return None;
                }
            }
        };
        if let Some(r) = settled {
            self.settle(r);
        }
        next
    }

    /// run 结束：结束埋点；还没交给面板的请求一并取消；被取代的截图会话拆掉 Overlay（交还焦点 → 销毁 → 恢复）
    pub(super) fn settle(&self, run: Run) {
        self.env.perf_finish(run.trace);
        if let Some(id) = &run.request
            && self.registry.owner(id) == Some(Owner::Unbound)
        {
            self.registry.remove(id);
        }
        if matches!(run.flow, Flow::Ocr | Flow::DirectTranslate) {
            let taken = {
                let mut g = self.overlay.lock();
                if g.as_ref().is_some_and(|o| o.trace == run.trace && !o.committed) {
                    g.take()
                } else {
                    None
                }
            };
            if let Some(st) = taken {
                // 未 commit 的截图会话随它的 run 结束：只会是被更新的触发打断（自己结束的会话已先取走登记）
                self.teardown_overlay(st, FocusReturn::BeforeDestroy, OverlayEndReason::Cancelled);
            }
        }
    }

    /// 从表里移除一个 run（面板没能显示等不再有会话的情况）
    pub(super) fn drop_run(&self, run: u64) {
        let r = self.runs.lock().map.remove(&run);
        if let Some(r) = r {
            self.settle(r);
        }
    }

    pub(super) fn set_run_request(&self, run: u64, id: &RequestId) {
        if let Some(r) = self.runs.lock().map.get_mut(&run) {
            r.request = Some(id.clone());
        }
    }

    pub(super) fn run_stage(&self, run: u64) -> Option<Stage> {
        self.runs.lock().map.get(&run).map(|r| r.stage)
    }

    /// 拿到了某个面板会话的 run：`(run, flow, stage)`
    pub(super) fn run_of_session(&self, session: u64) -> Option<(u64, Flow, Stage)> {
        self.runs
            .lock()
            .map
            .iter()
            .find(|(_, r)| r.session == Some(session))
            .map(|(id, r)| (*id, r.flow, r.stage))
    }

    /// 第一个 Delta：埋点 FirstToken，run 进入 Streaming
    pub(super) fn on_first_delta(&self, tags: Tags) {
        if let Some(trace) = tags.trace {
            self.env.perf().mark(trace, PerfStage::FirstToken);
        }
        if let Some(run) = tags.run {
            self.advance(run, Event::FirstDelta);
        }
    }

    // ------------------------------------------------------------------------------------------------
    // 面板

    /// 在阻塞线程上：核对 run 仍有效 → `panel_prepare` → 把会话记到 run 上，三步在准备闸门的同一个临界区里。
    /// 新的触发（`start_run`）要么在核对之前作废 run（不 prepare），要么在记录之后看到「面板已出现」（不作废）；
    /// 两者之间作废的，它的会话不绑定，并且一定会被新 run 随后的 prepare 替换（新 run 排在闸门后面）
    fn prepare_for(&self, run: u64, req: PanelShowRequest) -> Prepared {
        let _gate = self.prepare_gate.lock();
        let alive = |g: &Runs| g.map.get(&run).is_some_and(|r| !r.stage.is_terminal());
        if !alive(&self.runs.lock()) {
            return Prepared::Superseded(None);
        }
        let Some(session) = self.env.panel_prepare(req) else {
            return Prepared::Failed;
        };
        let mut g = self.runs.lock();
        if !alive(&g) {
            return Prepared::Superseded(Some(session));
        }
        if let Some(r) = g.map.get_mut(&run) {
            r.session = Some(session);
        }
        Prepared::Shown(session)
    }

    /// 准备并显示面板（阻塞部分在阻塞线程上），把请求 / 截图交给新会话，然后立即核对一次会话
    pub(super) async fn show(
        self: &Arc<Self>,
        run: u64,
        trace: u64,
        req: PanelShowRequest,
        request: Option<RequestId>,
        capture: Option<Arc<Crop>>,
    ) -> Option<u64> {
        let view = req.view;
        let release = |core: &Self| {
            if let Some(id) = &request {
                core.registry.remove(id);
            }
            if let Some(c) = &capture {
                core.captures.remove(&c.id);
            }
        };
        let core = Arc::clone(self);
        let prepared = blocking(move || core.prepare_for(run, req))
            .await
            .unwrap_or(Prepared::Failed);
        let session = match prepared {
            Prepared::Shown(session) => session,
            Prepared::Superseded(stale) => {
                // 取词 / 截图 / prepare 期间来了新的触发：结果丢弃；已经 prepare 的会话随即被新 run 的 prepare 替换
                log::debug!("run {run}: superseded before its panel appeared (stale session {stale:?}); not bound");
                release(self);
                return None;
            }
            Prepared::Failed => {
                log::error!("run {run}: the panel could not be shown (view={view:?}); request abandoned");
                release(self);
                // 有请求的 run 走 Failed；未就绪 / 输入 / 剪贴板这类没有请求的视图直接收尾
                if self.run_stage(run).is_some_and(Stage::is_requesting) {
                    self.advance(run, Event::Failed);
                }
                self.drop_run(run);
                return None;
            }
        };
        let perf = self.env.perf();
        perf.bind_session(trace, session);
        perf.mark(trace, PerfStage::Prepare);
        if let Some(id) = &request
            && !self.registry.bind(id, Owner::Panel(session))
        {
            log::debug!("run {run}: request {id} was superseded before it reached session {session}");
        }
        if let Some(c) = &capture {
            c.set_owner(Owner::Panel(session));
        }
        log::debug!("run {run}: panel session {session} view={view:?} request={request:?}");
        self.reconcile();
        if request.is_none() {
            // 没有请求的视图（未就绪卡片、长文本确认、输入、剪贴板）：显示之后就结束埋点
            self.spawn_finish_when_shown(trace);
        }
        Some(session)
    }

    /// 面板会话收尾：对登记过的全部面板会话核对 `panel_session_live`，结束了的取消请求、释放截图、结束 run
    pub fn reconcile(&self) {
        let mut sessions: BTreeSet<u64> = self.registry.panel_sessions().into_iter().collect();
        sessions.extend(self.captures.panel_sessions());
        sessions.extend(self.runs.lock().map.values().filter_map(|r| r.session));
        let dead: Vec<u64> = sessions
            .into_iter()
            .filter(|s| !self.env.panel_session_live(*s))
            .collect();
        if dead.is_empty() {
            return;
        }
        let ended = self
            .registry
            .end_where(|o| matches!(o, Owner::Panel(s) if dead.contains(s)));
        for e in ended.iter().filter(|e| e.was_running) {
            log::debug!("request {} cancelled: its panel session ended", e.id);
        }
        self.captures
            .end_where(|o| matches!(o, Owner::Panel(s) if dead.contains(s)));
        let runs: Vec<u64> = self
            .runs
            .lock()
            .map
            .iter()
            .filter(|(_, r)| r.session.is_some_and(|s| dead.contains(&s)))
            .map(|(id, _)| *id)
            .collect();
        for run in runs {
            self.advance(run, Event::Closed);
            // Done / Failed → Closed 也是终止阶段，advance 已经收尾；保险起见确认移除
            self.drop_run(run);
        }
    }

    /// 等这次触发的面板显示（Shown 埋点或 ready 超时，事件驱动），最多 `limit`；返回是否等到
    pub(super) async fn wait_shown(&self, trace: u64, limit: Duration) -> bool {
        let perf = self.env.perf();
        let mut rx = perf.subscribe();
        let until = tokio::time::Instant::now() + limit;
        loop {
            rx.borrow_and_update();
            match perf.snapshot(trace) {
                Some(s) if s.shown_ms.is_some() || s.ready_timeout => return true,
                Some(s) if s.finished => return false,
                None => return false,
                Some(_) => {}
            }
            match tokio::time::timeout_at(until, rx.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => return false,
            }
        }
    }

    /// 面板显示之后再结束埋点（perf 日志行带上 shown_ms），最多等 [`SHOWN_WAIT`]。ready 超时之后照样等到真正显示
    /// （超时就结束追踪、写成 `shown_ms=-` 的话，最慢的那批会话会被延迟分位数剔除）
    pub(super) fn spawn_finish_when_shown(self: &Arc<Self>, trace: u64) {
        let core = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            let started = tokio::time::Instant::now();
            if core.wait_shown(trace, SHOWN_WAIT).await {
                let left = SHOWN_WAIT.saturating_sub(started.elapsed());
                core.wait_actually_shown(trace, left).await;
            }
            core.env.perf_finish(trace);
        });
    }

    /// 等 Shown 埋点本身（不把 ready 超时当作显示），最多 `limit`
    async fn wait_actually_shown(&self, trace: u64, limit: Duration) {
        let perf = self.env.perf();
        let mut rx = perf.subscribe();
        let until = tokio::time::Instant::now() + limit;
        loop {
            rx.borrow_and_update();
            match perf.snapshot(trace) {
                Some(s) if s.shown_ms.is_some() || s.finished => return,
                None => return,
                Some(_) => {}
            }
            match tokio::time::timeout_at(until, rx.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => return,
            }
        }
    }

    /// Rust 发起的请求（或推进了 run 的第一个前端请求）结束：推进 run，显示之后结束埋点
    pub(super) fn request_ended(self: &Arc<Self>, run: Option<u64>, trace: Option<u64>, end: &PumpEnd) {
        if let Some(run) = run {
            let ev = match end {
                PumpEnd::Done { .. } => Event::Finished,
                PumpEnd::Failed(_) => Event::Failed,
                PumpEnd::Cancelled => Event::Cancel,
            };
            self.advance(run, ev);
        }
        if let Some(trace) = trace {
            self.spawn_finish_when_shown(trace);
        }
    }

    /// 触发时的前台应用：剪贴板条目送翻译时沿用剪贴板会话记下的那个。
    /// 识别历史的「重新翻译」（origin = ocr_history）是在 Wisp 自己的设置窗口里点的：前台是设置窗口，而平台的
    /// `frontmost` 跳过 Wisp 自己的窗口、给出的是此前最后一个别的应用——拿它当面板的 prev_app，用户在面板里按 Esc /
    /// 点开别处时焦点会被交还给那个应用，把人从设置窗口里拽走。这里给 None：面板关闭后焦点自然回到设置窗口
    /// （设置窗口在前台时送翻译）
    pub(super) async fn prev_app_for(&self, origin: TriggerOrigin) -> Option<AppRef> {
        if origin == TriggerOrigin::OcrHistory {
            return None;
        }
        if origin == TriggerOrigin::Clipboard
            && let Some(p) = self.env.panel_state()
            && let Some(app) = self.env.panel_prev_app(p.session_id)
        {
            return Some(app);
        }
        let env = Arc::clone(&self.env);
        blocking(move || env.frontmost()).await.flatten()
    }

    // ------------------------------------------------------------------------------------------------
    // Key

    /// 在阻塞线程上开始读某个 Provider 的 Key（[`KeyFetch`]）。读成功就记为「再读不会弹框」，失败（拒绝 / 锁定）则撤销
    pub(super) fn prefetch_key(&self, provider_id: &str) -> KeyFetch {
        let env = Arc::clone(&self.env);
        let quiet = Arc::clone(&self.quiet_keys);
        let provider = provider_id.to_owned();
        KeyFetch {
            provider_id: provider_id.to_owned(),
            handle: tauri::async_runtime::spawn_blocking(move || {
                let r = env.key_get(&provider);
                let mut q = quiet.lock();
                if r.is_ok() {
                    q.insert(provider);
                } else {
                    q.remove(&provider);
                }
                r
            }),
        }
    }

    /// 现在读这个 Provider 的 Key 不会弹系统对话框：钥匙串本身从不弹框（Windows 凭据管理器、Key 只在内存里），或本进程
    /// 已经成功读过它（授权过了）
    pub(super) fn key_read_is_quiet(&self, provider_id: &str) -> bool {
        !self.env.keychain_may_prompt() || self.quiet_keys.lock().contains(provider_id)
    }

    /// 触发时预读：只在读取不会弹框时（[`Self::key_read_is_quiet`]）。可能弹框时推迟到真正要用 Key 的时候——对话框不在
    /// 触发的瞬间、更不在全屏 Overlay 之下出现；长文本 / 空选区 / 缓存命中这些不发请求的路径也不会因此弹框。
    /// 不需要 Key、也没存 Key 的 Provider 不预读（[`Self::key_not_needed`]）
    pub(super) fn prefetch_key_if_quiet(&self, provider: &wisp_types::Provider) -> Option<KeyFetch> {
        (!self.key_not_needed(provider) && self.key_read_is_quiet(&provider.id))
            .then(|| self.prefetch_key(&provider.id))
    }

    /// 这个 Provider 不需要 Key（模板声明不需要或指向回环地址，`plan::provider_needs_key`），且索引里没有存它的 Key：
    /// 请求不访问钥匙串、直接以「没有 Key」发出。钥匙串不可用 / 锁定 / 没有默认钥匙串时本地模型照常可用
    /// （与 `not_ready::check_key` 判为就绪的口径一致）
    pub(super) fn key_not_needed(&self, provider: &wisp_types::Provider) -> bool {
        !super::plan::provider_needs_key(provider) && !self.env.has_key(&provider.id)
    }

    /// 取得请求用的 Key。失败时写入 `Error{keychain, retryable}`；最多等 [`KEY_WAIT`]（系统对话框一直没人回答），
    /// 超时同样以可重试的 `Error{keychain}` 结束；请求在等待中被取消时立即返回
    pub(super) async fn read_key(
        &self,
        id: &str,
        resolved: &Resolved,
        src: KeySource,
        token: &CancellationToken,
    ) -> Result<Option<Secret>, PumpEnd> {
        let fetch = match src {
            KeySource::Known(secret) => return Ok(secret),
            _ if self.key_not_needed(&resolved.provider) => return Ok(None),
            KeySource::Prefetched(k) if k.provider_id == resolved.provider.id => k,
            KeySource::Prefetched(_) | KeySource::Read => self.prefetch_key(&resolved.provider.id),
        };
        let optional = !super::plan::provider_needs_key(&resolved.provider);
        let joined = tokio::select! {
            r = tokio::time::timeout(KEY_WAIT, fetch.join()) => r,
            () = token.cancelled() => return Err(PumpEnd::Cancelled),
        };
        let err = match joined {
            Ok(Some(Ok(secret))) => return Ok(secret),
            Ok(Some(Err(e))) if optional => {
                // 不需要 Key 的 Provider（本地模型）存过 Key 却读不出来：不带 Key 照常请求，服务端真要鉴权会回 401
                log::warn!(
                    "request {id}: reading the key failed ({}); the provider does not require one, sending without it",
                    e.message
                );
                return Ok(None);
            }
            Ok(Some(Err(e))) => {
                log::warn!("request {id}: reading the key failed: {}", e.message);
                keychain_error(&e)
            }
            Ok(None) => LlmError::new(ErrorCode::Internal, "reading the key failed"),
            Err(_) => {
                log::warn!(
                    "request {id}: the keychain did not answer within {KEY_WAIT:?} (a system dialog may be waiting)"
                );
                let mut err = LlmError::new(ErrorCode::Keychain, "the keychain did not answer in time");
                err.retryable = true;
                err
            }
        };
        self.registry.emit(id, StreamEvent::Error(err.clone()));
        Err(PumpEnd::Failed(err))
    }

    // ------------------------------------------------------------------------------------------------
    // 其他命令

    /// `stream_attach`：只有请求的宿主窗口可以接上（面板：还没绑定的与面板会话的；pinned：归属它的），否则 not_found
    pub fn stream_attach(&self, caller: &RequestCaller, request_id: &str, sink: Sink) -> Result<(), IpcError> {
        self.registry.attach(request_id, sink, |owner| caller.may_access(owner))
    }

    /// `cancel`：Error{cancelled} 立即写入，之后不再有任何事件到达前端。已经结束、不存在、或属于别的窗口
    /// （例如面板卸载时去取消已经转给 pinned 的请求）都是空操作——`cancel` 返回 `()`
    pub fn cancel(&self, caller: &RequestCaller, request_id: &str) {
        match self.registry.cancel_if(request_id, |owner| caller.may_access(owner)) {
            Ok(true) => log::info!("request {request_id} cancelled by the user"),
            Ok(false) => {}
            Err(_) => log::debug!("cancel {request_id} from {caller:?} ignored: the request belongs to another window"),
        }
    }

    /// `capture_release`：只释放调用方窗口自己的截图；不存在或属于别的窗口的是空操作（返回 `()`）
    pub fn capture_release(&self, caller: &RequestCaller, capture_id: &str) {
        match self.captures.remove_if(capture_id, |owner| caller.may_access(owner)) {
            Ok(true) => log::debug!("capture {capture_id} released"),
            Ok(false) => {}
            Err(_) => log::debug!("capture_release {capture_id} from {caller:?} ignored: it belongs to another window"),
        }
    }

    /// 固定交接：请求改归 pinned；它用到的截图、以及同一面板会话的其他截图一并转过去（OCR 面板里固定，
    /// pinned 还要能重新识别、查看原文）
    pub fn transfer_to_pinned(&self, request_id: &str, pinned_id: &str) {
        let to = Owner::Pinned(pinned_id.to_owned());
        match self.registry.transfer(request_id, pinned_id) {
            Some((from, capture)) => {
                if let Some(c) = capture.and_then(|c| self.captures.crop(&c)) {
                    c.set_owner(to.clone());
                }
                if matches!(from, Owner::Panel(_)) {
                    self.captures.reassign(&from, &to);
                }
                log::info!("request {request_id} transferred to pinned {pinned_id}");
            }
            None => log::debug!("transfer_to_pinned: unknown request {request_id}"),
        }
    }

    /// pinned 窗口已销毁：取消归属它的全部流，释放它持有的截图
    pub fn pinned_closed(&self, pinned_id: &str) {
        let owner = Owner::Pinned(pinned_id.to_owned());
        let ended = self.registry.end_where(|o| *o == owner);
        let crops = self.captures.end_where(|o| *o == owner);
        log::debug!(
            "pinned {pinned_id} closed: {} request(s), {crops} capture(s) released",
            ended.len()
        );
    }

    /// 启动后的空闲预热（阻塞线程）
    pub fn idle_prewarm(&self) {
        self.env.prewarm_selection();
        // 已有屏幕录制权限时预取一次 SCShareableContent 与自身应用：ScreenCaptureKit 的框架与 WindowServer 连接在
        // 第一次截图之前就热了（冷的第一次捕获约 120–300 ms，热的约 40 ms，第一次 ⌥S 的 Overlay 因此常超过 150 ms）。
        // 预检绝不弹窗；预取的快照按 TTL 由 GCD 的一次性任务释放，不常驻内存
        if self.env.screen_permission() == wisp_types::PermissionState::Granted {
            self.env.capture_prewarm();
        }
        // 让语言检测与提示词装配的代码与表格进入内存（第一次划词时少一次缺页）
        let _ = wisp_prompts::detect_lang("The quick brown fox jumps over the lazy dog, again and again.");
    }

    /// 某个请求的结局（测试用）
    #[cfg(test)]
    pub(crate) fn outcome(&self, id: &str) -> Option<Option<super::registry::Outcome>> {
        self.registry.outcome(id)
    }

    /// 当前进行中的 run 数（测试用）
    #[cfg(test)]
    pub(crate) fn run_count(&self) -> usize {
        self.runs.lock().map.len()
    }

    /// 是否有 Overlay 会话（测试用）
    #[cfg(test)]
    pub(crate) fn overlay_active(&self) -> bool {
        self.overlay.lock().is_some()
    }

    /// 当前 Overlay 会话的 id（测试用）
    #[cfg(test)]
    pub(crate) fn overlay_session(&self) -> Option<u64> {
        self.overlay.lock().as_ref().map(|o| o.session_id)
    }

    /// 缓存条数（测试用）
    #[cfg(test)]
    pub(crate) fn cache_len(&self) -> usize {
        self.cache.len()
    }
}
