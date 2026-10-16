//! 推理槽位：`ort`（ORT 工作进程：PP-OCRv6 与版面检测）、`vl`（llama-server：PaddleOCR-VL）、`translate`（llama-server：
//! Index-Translate）。这里是槽位的数据结构；状态转换在 `lifecycle`。
//!
//! - **单飞**：槽位在 `Stopped → Starting → Ready` 之间转换；`Starting` 期间来的请求都等同一次启动（[`watch`] 通道广播结果），
//!   启动在独立的任务里进行——请求方取消只是不再等，加载照常完成，之后由闲置定时器释放（用户多半马上会重试）。
//! - **租约**：请求拿到租约才能用槽位；租约在、槽位就不会被闲置释放。最后一个租约放手时重排闲置定时器
//!   （一次性，`offline.idle_release_min`）；有新请求时取消它。没有槽位加载时不存在任何定时器。
//! - **强制停止**（换模型、删除、退出）：先取消进行中的请求（[`Loaded::cancel`]），等它们放手（最多 1 s），再关
//!   stdin、等进程退出、必要时强制结束；仍有请求没放手时由最后一个租约的析构结束进程（平台层的子进程句柄析构即结束整棵树）。
//! - **新旧进程**：把进程从槽位里取下（或放弃一次启动）时，在同一把锁里登记一个停止流程（[`StopTicket`]）；同一槽位的新进程
//!   启动前先等这些流程走完、再做内存检查——不让新旧两个进程同时占着内存，旧进程收尾时也不会删掉新进程的 pid 文件。
//! - **失败**：启动失败按原因归类后返回；同一模型同一原因在 30 s 内第二次失败之后，30 s 内不再重启、直接报同样的错误
//!   （避免反复拉起一个注定失败的进程）。设置变化（换模型、关 GPU）、重新下载或删除这个模型会清掉这条记录。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;
use wisp_local::RuntimeFailure;
use wisp_local::llama::Server;
use wisp_local::ocr::PpOcrTier;
use wisp_local::worker::OrtWorker;
use wisp_types::{LlmError, LoadState, LocalModelId};

use super::ModelsService;

/// 强制停止时等进行中的请求放手、等进程自己退出的上限
pub(super) const STOP_GRACE: Duration = Duration::from_secs(1);
/// 同一原因连续失败的观察窗口
pub(super) const FAILURE_WINDOW: Duration = Duration::from_secs(30);

/// 槽位
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum SlotKind {
    /// ORT 工作进程（PP-OCRv6 Tiny / Small 与 PaddleOCR-VL 的版面检测共用一个进程）
    Ort,
    /// llama-server：PaddleOCR-VL-1.6
    Vl,
    /// llama-server：Index-Translate 2B / 9B
    Translate,
}

impl SlotKind {
    pub(crate) const ALL: [Self; 3] = [Self::Ort, Self::Vl, Self::Translate];

    /// pid 文件名与日志里的名字
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Ort => "ort",
            Self::Vl => "vl",
            Self::Translate => "translate",
        }
    }

    /// 这个槽位里为 `loaded` 启动的进程能不能为 `wanted` 服务（ORT 工作进程按档位懒建会话，一个进程服务全部 OCR 模型）
    pub(crate) fn serves(self, loaded: LocalModelId, wanted: LocalModelId) -> bool {
        match self {
            Self::Ort => true,
            Self::Vl | Self::Translate => loaded == wanted,
        }
    }

    /// 可能为这个模型启动过的槽位（不论现在加载着什么；翻译槽位为 2B、9B 都启动过）
    pub(crate) const fn of_model(model: LocalModelId) -> &'static [Self] {
        match model {
            LocalModelId::PpOcrV6Tiny | LocalModelId::PpOcrV6Small => &[Self::Ort],
            LocalModelId::PaddleOcrVl16 => &[Self::Vl, Self::Ort],
            LocalModelId::IndexTranslate2b | LocalModelId::IndexTranslate9b => &[Self::Translate],
        }
    }
}

/// 槽位里运行的进程
pub(super) enum Proc {
    Ort(OrtWorker),
    Llama {
        /// 停止时取走（`Server::stop` 消耗它）；`exited` 要可变借用
        server: Mutex<Option<Server>>,
        base_url: String,
        api_key: String,
    },
}

/// 一个加载好的槽位
pub(crate) struct Loaded {
    /// 启动它的模型（ORT 槽位是第一次用到它的 OCR 模型）
    pub(super) model: LocalModelId,
    /// 这一代进程的编号（租约放手、闲置定时器据此只作用于自己这一代）
    pub(super) generation: u64,
    pub(super) proc: Proc,
    /// 强制停止时取消进行中的请求
    pub(super) cancel: CancellationToken,
    /// ORT 工作进程里用过的 PP-OCR 模型（设置页据此显示「运行中」；版面检测算 PaddleOCR-VL 的一部分，不记在这里）
    pub(super) used: Mutex<BTreeSet<LocalModelId>>,
    /// 进程 pid（日志）
    pub(super) pid: u32,
}

impl Loaded {
    /// 进程已经退出时给出归类后的原因（不阻塞）
    pub(super) fn exited(&self) -> Option<RuntimeFailure> {
        match &self.proc {
            Proc::Ort(w) => (!w.is_alive()).then_some(RuntimeFailure::Crashed),
            Proc::Llama { server, .. } => match server.lock().as_mut() {
                Some(s) => s.exited(),
                None => Some(RuntimeFailure::Crashed),
            },
        }
    }

    /// 关 stdin、最多等 `grace`、再强制结束（阻塞）
    pub(super) fn shutdown(self, grace: Duration) {
        match self.proc {
            Proc::Ort(w) => w.shutdown(grace),
            Proc::Llama { server, .. } => {
                if let Some(s) = server.into_inner() {
                    s.stop(grace);
                }
            }
        }
    }
}

/// 启动结果的广播：（为哪个模型启动, 结果）。None = 还在启动；发送端丢弃 = 启动被停止
pub(super) type StartResult = Option<(LocalModelId, Result<(), LlmError>)>;

pub(super) enum Phase {
    Stopped,
    Starting {
        model: LocalModelId,
        /// 这次启动的编号（就绪后成为进程的 `generation`）：启动任务据此确认槽位仍在等它
        attempt: u64,
        done: watch::Receiver<StartResult>,
        /// 停止 / 退出时取消启动（llama-server 的启动等待随之结束并结束进程）
        cancel: CancellationToken,
    },
    Ready(Arc<Loaded>),
}

/// 最近一次启动失败
#[derive(Debug, Clone)]
pub(super) struct Failure {
    pub(super) model: LocalModelId,
    pub(super) error: LlmError,
    pub(super) at: Instant,
    /// 窗口内同一原因的次数
    pub(super) repeats: u32,
}

impl Failure {
    /// 30 s 内同一原因已经连续失败两次：不再重启，直接报这个错误
    pub(super) fn blocks(&self, model: LocalModelId, now: Instant) -> bool {
        self.model == model && self.repeats >= 2 && now.duration_since(self.at) < FAILURE_WINDOW
    }

    /// 记一次新的失败（同一模型、同一原因、窗口内则累加）
    pub(super) fn next(prev: Option<&Self>, model: LocalModelId, error: LlmError, now: Instant) -> Self {
        let repeats = match prev {
            Some(p)
                if p.model == model
                    && p.error.code == error.code
                    && p.error.hint == error.hint
                    && now.duration_since(p.at) < FAILURE_WINDOW =>
            {
                p.repeats + 1
            }
            _ => 1,
        };
        Self {
            model,
            error,
            at: now,
            repeats,
        }
    }
}

/// 槽位的可变状态（锁内只做 O(1) 的工作，不跨 await 持有）
pub(super) struct SlotInner {
    pub(super) phase: Phase,
    /// 当前这一代进程上的租约数
    pub(super) busy: usize,
    /// 闲置释放的一次性定时器（只在就绪且没有租约时存在）
    pub(super) timer: Option<tokio::task::JoinHandle<()>>,
    /// 内存严重不足或设置变化：请求做完就停
    pub(super) stop_when_idle: bool,
    pub(super) failure: Option<Failure>,
    pub(super) next_generation: u64,
    /// 还没走完的停止流程数（已从槽位取下、进程还没收尾完的；以及被强制停止、启动任务还没收尾的启动）。
    /// 与相位在同一把锁里增减：取下进程与登记停止之间没有空隙，新进程启动前据此等旧进程走完
    pub(super) stopping: usize,
    /// 被强制停止的启动（编号）：各自在 `stopping` 里记了一笔，由那次启动的任务收尾时注销
    pub(super) abandoned: BTreeSet<u64>,
    /// 当前这次启动 / 这一代进程有没有真实请求在等或用过（预热之外）：预热放手时据此决定立即停还是照常闲置
    pub(super) wanted: bool,
}

impl SlotInner {
    fn new() -> Self {
        Self {
            phase: Phase::Stopped,
            busy: 0,
            timer: None,
            stop_when_idle: false,
            failure: None,
            next_generation: 1,
            stopping: 0,
            abandoned: BTreeSet::new(),
            wanted: false,
        }
    }

    pub(super) fn cancel_timer(&mut self) {
        if let Some(t) = self.timer.take() {
            t.abort();
        }
    }

    /// 就绪的进程能否为这次请求服务
    pub(super) fn ready_for(&self, kind: SlotKind, model: LocalModelId) -> Option<&Arc<Loaded>> {
        match &self.phase {
            Phase::Ready(l) if kind.serves(l.model, model) => Some(l),
            _ => None,
        }
    }
}

/// 一个槽位
pub(super) struct Slot {
    pub(super) kind: SlotKind,
    pub(super) inner: Mutex<SlotInner>,
    /// 有租约放手时通知（强制停止等进行中的请求）
    pub(super) released: Notify,
    /// 一个停止流程走完时通知
    stopped: Notify,
}

/// 一个停止流程的凭据：在把进程从槽位里取下（或放弃一次启动）的**同一把锁里**登记，流程走完时析构即注销——即使所在的
/// 任务被丢弃。登记若晚于取下（例如在被 spawn 的停止任务里才登记），多线程运行时可能先执行后 spawn 的启动任务，新进程
/// 看到计数为 0 就启动，与旧进程短暂共存（9B 时多占 6 GB）。析构时会锁这个槽位：持有槽位锁时不要让它析构
pub(super) struct StopTicket {
    svc: Arc<ModelsService>,
    kind: SlotKind,
}

impl StopTicket {
    /// 登记一个停止流程（`g` 是这个槽位已经锁住的状态）
    pub(super) fn issue(svc: &Arc<ModelsService>, g: &mut SlotInner, kind: SlotKind) -> Self {
        g.stopping += 1;
        Self {
            svc: Arc::clone(svc),
            kind,
        }
    }

    /// 接手一个已经登记过的停止流程（强制停止正在启动的槽位时登记在 `abandoned` 里，由那次启动的任务收尾）
    pub(super) fn adopt(svc: &Arc<ModelsService>, kind: SlotKind) -> Self {
        Self {
            svc: Arc::clone(svc),
            kind,
        }
    }
}

impl Drop for StopTicket {
    fn drop(&mut self) {
        let slot = self.svc.slots.get(self.kind);
        {
            let mut g = slot.inner.lock();
            g.stopping = g.stopping.saturating_sub(1);
        }
        slot.stopped.notify_waiters();
    }
}

impl Slot {
    fn new(kind: SlotKind) -> Self {
        Self {
            kind,
            inner: Mutex::new(SlotInner::new()),
            released: Notify::new(),
            stopped: Notify::new(),
        }
    }

    /// 等这个槽位正在进行的停止流程都走完，最多 `limit`。新进程启动前调用：旧进程收尾时要删同名的 pid 文件，
    /// 新进程先写了的话会被删掉；也不让新旧两个进程同时占着内存
    pub(super) async fn wait_stops(&self, limit: Duration) {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let notified = self.stopped.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.inner.lock().stopping == 0 {
                return;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return;
            }
        }
    }

    /// 等 `loaded` 的其他持有者（租约）都放手，最多 `limit`
    pub(super) async fn wait_unshared(&self, loaded: &Arc<Loaded>, limit: Duration) {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let notified = self.released.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if Arc::strong_count(loaded) == 1 {
                return;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return;
            }
        }
    }
}

/// 三个槽位
pub(super) struct Slots {
    ort: Slot,
    vl: Slot,
    translate: Slot,
}

impl Slots {
    pub(super) fn new() -> Self {
        Self {
            ort: Slot::new(SlotKind::Ort),
            vl: Slot::new(SlotKind::Vl),
            translate: Slot::new(SlotKind::Translate),
        }
    }

    pub(super) fn get(&self, kind: SlotKind) -> &Slot {
        match kind {
            SlotKind::Ort => &self.ort,
            SlotKind::Vl => &self.vl,
            SlotKind::Translate => &self.translate,
        }
    }

    pub(super) fn all(&self) -> [&Slot; 3] {
        [&self.ort, &self.vl, &self.translate]
    }

    /// 是否有任何槽位加载着或正在加载
    pub(super) fn any_active(&self) -> bool {
        self.all()
            .iter()
            .any(|s| !matches!(s.inner.lock().phase, Phase::Stopped))
    }

    /// 设置页的「加载中 / 运行中」
    pub(super) fn loads(&self) -> BTreeMap<LocalModelId, LoadState> {
        let mut out = BTreeMap::new();
        for slot in self.all() {
            let g = slot.inner.lock();
            match &g.phase {
                Phase::Stopped => {}
                Phase::Starting { model, .. } => {
                    // ORT 槽位为 PaddleOCR-VL 启动（版面检测）时，VL 的状态看 vl 槽位
                    if slot.kind != SlotKind::Ort || PpOcrTier::of(*model).is_some() {
                        out.insert(*model, LoadState::Loading);
                    }
                }
                Phase::Ready(l) => {
                    let models: Vec<LocalModelId> = match slot.kind {
                        SlotKind::Ort => l.used.lock().iter().copied().collect(),
                        SlotKind::Vl | SlotKind::Translate => vec![l.model],
                    };
                    for m in models {
                        out.entry(m).or_insert(LoadState::Loaded);
                    }
                }
            }
        }
        out
    }

    /// 诊断包：每个槽位的状态（不含路径与端口）
    pub(super) fn describe(&self) -> serde_json::Value {
        let mut out = serde_json::Map::new();
        for slot in self.all() {
            let g = slot.inner.lock();
            let state = match &g.phase {
                Phase::Stopped => serde_json::json!({ "state": "stopped" }),
                Phase::Starting { model, .. } => serde_json::json!({ "state": "starting", "model": model.as_str() }),
                Phase::Ready(l) => serde_json::json!({
                    "state": if g.busy > 0 { "busy" } else { "ready" },
                    "model": l.model.as_str(),
                }),
            };
            out.insert(slot.kind.name().to_owned(), state);
        }
        serde_json::Value::Object(out)
    }
}

#[cfg(test)]
mod tests {
    use wisp_types::ErrorCode;

    use super::*;

    fn err(hint: &str) -> LlmError {
        let mut e = LlmError::new(ErrorCode::LocalRuntime, "x");
        e.hint = Some(hint.to_owned());
        e
    }

    #[test]
    fn repeated_failures_with_the_same_cause_block_restarts_for_a_while() {
        let t0 = Instant::now();
        let m = LocalModelId::IndexTranslate9b;
        let oom = "errors.hint.local.out_of_memory";
        let first = Failure::next(None, m, err(oom), t0);
        assert!(!first.blocks(m, t0), "第一次失败之后照常重试");
        let second = Failure::next(Some(&first), m, err(oom), t0);
        assert!(second.blocks(m, t0 + Duration::from_secs(5)));
        assert!(!second.blocks(m, t0 + FAILURE_WINDOW), "30 s 之后再试");
        assert!(!second.blocks(LocalModelId::IndexTranslate2b, t0), "换了模型照常启动");
        // 原因不同：重新计数
        let other = Failure::next(Some(&first), m, err("errors.hint.local.crashed"), t0);
        assert_eq!(other.repeats, 1);
        // 超出窗口：重新计数
        let late = Failure::next(Some(&first), m, err(oom), t0 + FAILURE_WINDOW);
        assert_eq!(late.repeats, 1);
    }

    #[test]
    fn the_ort_slot_serves_every_ocr_model() {
        assert!(SlotKind::Ort.serves(LocalModelId::PpOcrV6Tiny, LocalModelId::PpOcrV6Small));
        assert!(SlotKind::Ort.serves(LocalModelId::PpOcrV6Tiny, LocalModelId::PaddleOcrVl16));
        assert!(!SlotKind::Translate.serves(LocalModelId::IndexTranslate2b, LocalModelId::IndexTranslate9b));
        assert!(SlotKind::Vl.serves(LocalModelId::PaddleOcrVl16, LocalModelId::PaddleOcrVl16));
    }

    #[test]
    fn an_empty_slot_set_reports_nothing() {
        let slots = Slots::new();
        assert!(!slots.any_active());
        assert!(slots.loads().is_empty());
        let d = slots.describe();
        assert_eq!(d["ort"]["state"], "stopped");
        assert_eq!(d["translate"]["state"], "stopped");
    }
}
