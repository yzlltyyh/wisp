//! StreamRegistry：Rust 在触发后立即发起 LLM 请求，事件写入这里的日志；前端 `stream_attach` 时先回放
//! 再推送实时事件；固定时转移所有权。
//!
//! **管线**：`LlmClient::stream` → [`PostProcessor`]（`push`，Done 时补 `finish().tail`、
//! 填 `no_text`）→ 16 ms [`Coalescer`] → 日志 + 通道（[`pump`]）。
//!
//! **事件序列**：每个请求一登记就写入 `Start`（模型信息由编排器给出）与 `Meta`（语言与方向，本地就能算出，
//! 不等网络）；wisp-llm 稍后自己发的 `Start` 只记日志不转发（同一请求只有一个 Start）。之后是
//! `(Meta | Phase | Notice | Delta)*`，最后恰好一个 `Done` 或 `Error`。
//!
//! **取消**：[`Registry::cancel`] 立即取消令牌、在日志末尾写入 `Error{cancelled}` 并把请求
//! 标为结束；此后 [`Registry::emit`] 对这个请求一律返回 false——wisp-llm 在 cancel **之前**已经入队的 Delta、合并器里
//! 还没冲刷的文本都不会再到达前端。泵（[`pump`]）看到 false 就丢弃接收端，连接随之关闭。
//!
//! **所有权与通道**：请求属于一个宿主（[`Owner`]：还没定会话的 Rust 发起请求、面板会话、pinned 窗口），按 [`Lane`]
//! 分通道。同一宿主同一通道里只保留**最新登记**（`seq` 最大）的请求，其余立即取消并移除（一个面板只有一个
//! 活跃请求：连切风格只有最后一个在流式）。Rust 发起的请求在 `panel_prepare` 返回之后才绑定到会话
//! （[`Registry::bind`]）：前端在此之前已经对这个会话发起了更新的请求时，被取消的是这个更早的请求，而不是更新的那个。
//! 面板会话结束、pinned 关闭时由编排器调用 [`Registry::end_where`] 收尾。
//!
//! **宿主核对**：`stream_attach` / `cancel` 只接受请求宿主窗口的调用（[`Registry::attach`] / [`Registry::cancel_if`]
//! 的 `allow`），别的窗口拿着可猜的请求 id 也抢不走实时通道、取消不了别人的流。
//!
//! **日志上限**（1 MB，超出后只保留合并后的文本）：相邻的 Delta 写进日志时直接合并成一条，回放时前端收到的是
//! 截至此刻的完整文本（一次追加），日志条数只与 Meta / Phase / Notice 的个数有关；文本本身受模型输出上限约束
//! （翻译、OCR 都 ≤ 4096 token）。另设 [`MAX_ENTRIES`] 兜底：超过时淘汰最早结束的请求。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tauri::ipc::Channel;
use tokio::sync::mpsc;
use wisp_llm::{CancellationToken, Coalescer};
use wisp_prompts::{PostKind, PostProcessor};
use wisp_types::{ErrorCode, LlmError, RequestId, StreamEvent, TruncateReason, Usage};

use crate::error::IpcError;

/// 16 ms 合并窗口
pub const COALESCE_WINDOW: Duration = Duration::from_millis(16);
/// 注册表最多保留的请求数（超过时淘汰最早结束的；进行中的不淘汰）
pub const MAX_ENTRIES: usize = 64;

/// 事件的接收方（前端的 `Channel`；测试里是收集器）
pub trait EventSink: Send + Sync {
    /// 投递一个事件；返回 false 表示接收方已经不在了（之后不再投递）
    fn deliver(&self, ev: &StreamEvent) -> bool;
}

impl EventSink for Channel<StreamEvent> {
    fn deliver(&self, ev: &StreamEvent) -> bool {
        match self.send(ev.clone()) {
            Ok(()) => true,
            Err(e) => {
                log::debug!("stream channel {} closed: {e}", self.id());
                false
            }
        }
    }
}

/// 接收方的共享句柄
pub type Sink = Arc<dyn EventSink>;

/// 请求的宿主
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Owner {
    /// Rust 发起、`panel_prepare` 还没返回会话 id（请求与面板显示并行）
    Unbound,
    /// 面板会话
    Panel(u64),
    /// pinned 窗口（固定交接转来的，或在 pinned 里新发起的）
    Pinned(String),
}

/// 宿主内的请求通道：同一通道同一时刻只有一个活跃请求
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    /// 文本任务：翻译 / 润色 / 校对 / 提升（含 OCR 面板里的翻译区）
    Text,
    /// OCR 转录（lines / paragraphs / markdown，含「查看原文」）
    Ocr,
    /// 截图直接翻译
    OcrTranslate,
}

/// 请求的结局
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Failed,
    Cancelled,
}

/// `Start` 事件里的模型信息
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModelInfo {
    pub provider_id: String,
    pub model_id: String,
    pub model_label: String,
}

/// 登记一个请求
pub struct OpenSpec {
    pub owner: Owner,
    pub lane: Lane,
    pub model: ModelInfo,
    /// 登记时紧跟 Start 写入的 Meta（语言与方向）
    pub meta: Option<StreamEvent>,
    /// 前端发起的请求随命令带来的通道；Rust 发起的请求为 None（等 `stream_attach`）
    pub sink: Option<Sink>,
    /// 埋点追踪（Rust 发起的请求）
    pub trace: Option<u64>,
    /// 所属的 run（Rust 发起的请求）
    pub run: Option<u64>,
    /// 请求用到的截图（OCR）
    pub capture_id: Option<String>,
}

/// 第一个 Delta 到达时交给钩子的标签
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tags {
    pub trace: Option<u64>,
    pub run: Option<u64>,
}

/// 被结束的请求（[`Registry::end_where`] 的返回值）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    pub id: RequestId,
    pub was_running: bool,
}

struct Entry {
    owner: Owner,
    lane: Lane,
    log: Vec<StreamEvent>,
    sink: Option<Sink>,
    cancel: CancellationToken,
    outcome: Option<Outcome>,
    tags: Tags,
    capture_id: Option<String>,
    seq: u64,
    delivered_delta: bool,
}

impl Entry {
    /// 写日志并投递（相邻 Delta 合并）
    fn push(&mut self, ev: StreamEvent) {
        if let Some(sink) = &self.sink
            && !sink.deliver(&ev)
        {
            self.sink = None;
        }
        match (self.log.last_mut(), ev) {
            (Some(StreamEvent::Delta { text }), StreamEvent::Delta { text: more }) => text.push_str(&more),
            (_, ev) => self.log.push(ev),
        }
    }

    fn text(&self) -> String {
        self.log
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }
}

/// 取消时写入的终止事件
pub fn cancelled_error() -> LlmError {
    LlmError::new(ErrorCode::Cancelled, "cancelled")
}

type FirstDeltaHook = Box<dyn Fn(Tags) + Send + Sync>;

/// 流注册表
pub struct Registry {
    entries: Mutex<HashMap<RequestId, Entry>>,
    next_id: AtomicU64,
    seq: AtomicU64,
    on_first_delta: FirstDeltaHook,
}

impl Registry {
    /// `on_first_delta`：某个请求第一次投递 Delta 时调用（埋点 FirstToken、run 进入 Streaming），在锁外执行
    pub fn new(on_first_delta: FirstDeltaHook) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            seq: AtomicU64::new(0),
            on_first_delta,
        }
    }

    /// 新的请求 id：`r-<自增>`，全进程唯一
    pub fn new_id(&self) -> RequestId {
        format!("r-{}", self.next_id.fetch_add(1, Ordering::Relaxed) + 1)
    }

    /// 登记请求：写入 Start（与 Meta），同一宿主同一通道的旧请求取消并移除。返回取消令牌
    pub fn open(&self, id: &RequestId, spec: OpenSpec) -> CancellationToken {
        let cancel = CancellationToken::new();
        let mut entry = Entry {
            owner: spec.owner,
            lane: spec.lane,
            log: Vec::with_capacity(4),
            sink: spec.sink,
            cancel: cancel.clone(),
            outcome: None,
            tags: Tags {
                trace: spec.trace,
                run: spec.run,
            },
            capture_id: spec.capture_id,
            seq: self.seq.fetch_add(1, Ordering::Relaxed),
            delivered_delta: false,
        };
        let mut g = self.entries.lock();
        if entry.owner != Owner::Unbound {
            // 新登记的 seq 最大：同一宿主同一通道的其余请求都比它旧
            supersede(&mut g, &entry.owner, entry.lane, entry.seq);
        }
        // Start / Meta 在锁内投递：与 attach、cancel 的投递保持同一顺序
        entry.push(StreamEvent::Start {
            request_id: id.clone(),
            provider_id: spec.model.provider_id,
            model_id: spec.model.model_id,
            model_label: spec.model.model_label,
            upstream_id: None,
        });
        if let Some(meta) = spec.meta {
            entry.push(meta);
        }
        g.insert(id.clone(), entry);
        evict(&mut g);
        cancel
    }

    /// 追加一个事件并投递。请求不存在或已结束时返回 false（调用方应停止）
    pub fn emit(&self, id: &str, ev: StreamEvent) -> bool {
        let first = {
            let mut g = self.entries.lock();
            let Some(e) = g.get_mut(id) else {
                return false;
            };
            if e.outcome.is_some() {
                return false;
            }
            let mut first = None;
            match &ev {
                StreamEvent::Delta { text } if text.is_empty() => return true,
                StreamEvent::Delta { .. } if !e.delivered_delta => {
                    e.delivered_delta = true;
                    first = Some(e.tags);
                }
                StreamEvent::Done { .. } => e.outcome = Some(Outcome::Done),
                StreamEvent::Error(err) => {
                    e.outcome = Some(if err.code == ErrorCode::Cancelled {
                        Outcome::Cancelled
                    } else {
                        Outcome::Failed
                    });
                }
                _ => {}
            }
            e.push(ev);
            if e.outcome.is_some() {
                e.sink = None;
            }
            first
        };
        if let Some(tags) = first {
            (self.on_first_delta)(tags);
        }
        true
    }

    /// `stream_attach`：先回放完整日志，请求仍在进行时再接上实时事件（替换之前的接收方）。
    /// `allow(宿主)` 为 false（调用方不是请求的宿主窗口）时与请求不存在一样返回 not_found
    pub fn attach(&self, id: &str, sink: Sink, allow: impl Fn(&Owner) -> bool) -> Result<(), IpcError> {
        let mut g = self.entries.lock();
        let e = g
            .get_mut(id)
            .filter(|e| allow(&e.owner))
            .ok_or_else(|| IpcError::not_found(format!("unknown request {id}")))?;
        for ev in &e.log {
            if !sink.deliver(ev) {
                return Ok(());
            }
        }
        if e.outcome.is_none() {
            e.sink = Some(sink);
        }
        Ok(())
    }

    /// 取消：令牌取消 + 写入 `Error{cancelled}`。请求仍在进行时返回 true（测试用）
    #[cfg(test)]
    pub(crate) fn cancel(&self, id: &str) -> bool {
        self.cancel_if(id, |_| true).unwrap_or(false)
    }

    /// 前端的 `cancel`：`allow(宿主)` 为 false 时 Err（不动别人的请求，编排器记日志后忽略）；请求不存在或已结束时
    /// 是空操作（Ok(false)）
    pub fn cancel_if(&self, id: &str, allow: impl Fn(&Owner) -> bool) -> Result<bool, IpcError> {
        let mut g = self.entries.lock();
        match g.get_mut(id) {
            Some(e) if !allow(&e.owner) => Err(IpcError::not_found(format!("unknown request {id}"))),
            Some(e) => Ok(cancel_entry(e)),
            None => Ok(false),
        }
    }

    /// 把 Rust 发起的请求交给面板会话（`panel_prepare` 返回之后）。同一会话同一通道里比它旧的请求取消；已经有比它
    /// **新**的请求（前端在绑定之前就对这个会话发起了，例如立刻换了风格）时，取消的是它自己。返回是否绑定成功
    pub fn bind(&self, id: &str, owner: Owner) -> bool {
        let mut g = self.entries.lock();
        let Some((lane, seq)) = g.get(id).map(|e| (e.lane, e.seq)) else {
            return false;
        };
        let newer = g.values().any(|e| e.owner == owner && e.lane == lane && e.seq > seq);
        if newer {
            if let Some(mut e) = g.remove(id)
                && cancel_entry(&mut e)
            {
                log::debug!("request {id} superseded before binding to {owner:?}");
            }
            return false;
        }
        supersede(&mut g, &owner, lane, seq);
        if let Some(e) = g.get_mut(id) {
            e.owner = owner;
        }
        true
    }

    /// 固定交接：请求改归 pinned 窗口。返回 `(原宿主, 请求用到的截图)`；请求不存在时为 None
    pub fn transfer(&self, id: &str, pinned_id: &str) -> Option<(Owner, Option<String>)> {
        let mut g = self.entries.lock();
        let e = g.get_mut(id)?;
        let previous = std::mem::replace(&mut e.owner, Owner::Pinned(pinned_id.to_owned()));
        Some((previous, e.capture_id.clone()))
    }

    /// 结束宿主满足条件的全部请求：进行中的取消，然后移除
    pub fn end_where(&self, pred: impl Fn(&Owner) -> bool) -> Vec<Ended> {
        let mut g = self.entries.lock();
        let ids: Vec<RequestId> = g
            .iter()
            .filter(|(_, e)| pred(&e.owner))
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| {
                let mut e = g.remove(&id)?;
                let was_running = cancel_entry(&mut e);
                Some(Ended { id, was_running })
            })
            .collect()
    }

    /// 全部面板会话宿主（去重、升序）
    pub fn panel_sessions(&self) -> Vec<u64> {
        let g = self.entries.lock();
        let mut out: Vec<u64> = g
            .values()
            .filter_map(|e| match e.owner {
                Owner::Panel(s) => Some(s),
                _ => None,
            })
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// 移除（结束之后的收尾；进行中的先取消）
    pub fn remove(&self, id: &str) {
        let mut g = self.entries.lock();
        if let Some(mut e) = g.remove(id) {
            cancel_entry(&mut e);
        }
    }

    /// 截至此刻的输出文本（经后处理）
    pub fn text(&self, id: &str) -> Option<String> {
        self.entries.lock().get(id).map(Entry::text)
    }

    /// 结局：None = 请求不存在；Some(None) = 进行中（测试用）
    #[cfg(test)]
    pub(crate) fn outcome(&self, id: &str) -> Option<Option<Outcome>> {
        self.entries.lock().get(id).map(|e| e.outcome)
    }

    /// 宿主
    pub fn owner(&self, id: &str) -> Option<Owner> {
        self.entries.lock().get(id).map(|e| e.owner.clone())
    }

    /// 当前登记的请求数（测试用）
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.lock().len()
    }

    /// 是否没有任何请求（测试用）
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.lock().is_empty()
    }
}

/// 取消一个条目（进行中时返回 true）
fn cancel_entry(e: &mut Entry) -> bool {
    if e.outcome.is_some() {
        return false;
    }
    e.cancel.cancel();
    e.push(StreamEvent::Error(cancelled_error()));
    e.outcome = Some(Outcome::Cancelled);
    e.sink = None;
    true
}

/// 同一宿主同一通道里比 `seq` 更早登记的请求：取消并移除（只取消更旧的，不碰更新的）
fn supersede(g: &mut HashMap<RequestId, Entry>, owner: &Owner, lane: Lane, seq: u64) {
    let old: Vec<RequestId> = g
        .iter()
        .filter(|(_, e)| e.seq < seq && e.owner == *owner && e.lane == lane)
        .map(|(id, _)| id.clone())
        .collect();
    for id in old {
        if let Some(mut e) = g.remove(&id)
            && cancel_entry(&mut e)
        {
            log::debug!("request {id} superseded (owner {owner:?}, lane {lane:?})");
        }
    }
}

/// 超过上限时淘汰最早结束的请求
fn evict(g: &mut HashMap<RequestId, Entry>) {
    while g.len() > MAX_ENTRIES {
        let oldest = g
            .iter()
            .filter(|(_, e)| e.outcome.is_some())
            .min_by_key(|(_, e)| e.seq)
            .map(|(id, _)| id.clone());
        match oldest {
            Some(id) => {
                g.remove(&id);
            }
            None => break,
        }
    }
}

/// 泵的结局
#[derive(Debug, Clone, PartialEq)]
pub enum PumpEnd {
    Done {
        usage: Option<Usage>,
        truncated: Option<TruncateReason>,
        no_text: bool,
        elapsed_ms: u32,
    },
    Failed(LlmError),
    /// 被取消，或请求已被移除（会话结束 / 被新请求取代）
    Cancelled,
}

/// 泵的计时（长文本耗时估算用）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PumpTiming {
    /// 从泵开始到第一个 Delta
    pub first_delta: Option<Duration>,
    /// 从第一个 Delta 到结束
    pub streaming: Option<Duration>,
}

/// 把一个 LLM 流接到注册表：后处理 → 16 ms 合并 → `emit`。返回结局与计时。
///
/// wisp-llm 的 `Start` 不转发（登记时已写入）；取消或请求被移除时丢弃接收端（连接随之关闭）并返回 Cancelled。
///
/// 16 ms 节奏不依赖「通道偶尔为空」：`select!` 偏向接收，通道持续有积压时（一个网络包里有大量 SSE 事件、
/// 本机快速模型）计时分支抢不到执行机会，所以每次 Delta 进缓冲之后都就地检查一次是否到期（`poll`），到期即冲刷
pub async fn pump(
    reg: &Registry,
    id: &str,
    mut rx: mpsc::Receiver<StreamEvent>,
    post: PostKind,
    source: &str,
) -> (PumpEnd, PumpTiming) {
    let started = Instant::now();
    let mut first_at: Option<Instant> = None;
    let mut post = PostProcessor::new(post, source);
    let mut coalescer = Coalescer::new(COALESCE_WINDOW);
    let emit_all = |events: Vec<StreamEvent>, first_at: &mut Option<Instant>| -> bool {
        for ev in events {
            if first_at.is_none() && matches!(&ev, StreamEvent::Delta { text } if !text.is_empty()) {
                *first_at = Some(Instant::now());
            }
            if !reg.emit(id, ev) {
                return false;
            }
        }
        true
    };
    loop {
        let deadline = coalescer.next_deadline();
        let received = tokio::select! {
            biased;
            ev = rx.recv() => ev,
            () = sleep_until(deadline), if deadline.is_some() => {
                if let Some(flushed) = coalescer.poll(Instant::now())
                    && !emit_all(vec![flushed], &mut first_at)
                {
                    return (PumpEnd::Cancelled, timing(started, first_at));
                }
                continue;
            }
        };
        let now = Instant::now();
        let (events, end) = match received {
            Some(StreamEvent::Start {
                upstream_id, model_id, ..
            }) => {
                log::debug!("request {id} upstream started model={model_id} upstream_id={upstream_id:?}");
                continue;
            }
            Some(StreamEvent::Delta { text }) => match post.push(&text) {
                Some(out) => {
                    let mut events = coalescer.push(StreamEvent::Delta { text: out }, now);
                    events.extend(coalescer.poll(now));
                    (events, None)
                }
                None => continue,
            },
            Some(StreamEvent::Done {
                elapsed_ms,
                usage,
                truncated,
                cached,
                no_text: _,
            }) => {
                let fin = post.finish();
                let mut events = Vec::with_capacity(3);
                if !fin.tail.is_empty() {
                    events.extend(coalescer.push(StreamEvent::Delta { text: fin.tail }, now));
                }
                events.extend(coalescer.push(
                    StreamEvent::Done {
                        elapsed_ms,
                        usage,
                        truncated,
                        cached,
                        no_text: fin.no_text,
                    },
                    now,
                ));
                let end = PumpEnd::Done {
                    usage,
                    truncated,
                    no_text: fin.no_text,
                    elapsed_ms,
                };
                (events, Some(end))
            }
            Some(StreamEvent::Error(err)) => {
                if err.code == ErrorCode::Cancelled {
                    // 只有编排器会取消令牌：注册表里已经写好了 Error{cancelled}
                    return (PumpEnd::Cancelled, timing(started, first_at));
                }
                // 失败结束：已经产生的文本保留（滞留在后处理器里的尾巴一并冲刷出去）
                let fin = post.finish();
                let mut events = Vec::with_capacity(3);
                if !fin.tail.is_empty() {
                    events.extend(coalescer.push(StreamEvent::Delta { text: fin.tail }, now));
                }
                let end = PumpEnd::Failed(err.clone());
                events.extend(coalescer.push(StreamEvent::Error(err), now));
                (events, Some(end))
            }
            Some(other) => (coalescer.push(other, now), None),
            None => {
                // 通道关闭却没有终止事件（wisp-llm 保证不会发生；兜底按内部错误结束）
                let err = LlmError::new(ErrorCode::Internal, "the stream ended without a terminal event");
                (
                    coalescer.push(StreamEvent::Error(err.clone()), now),
                    Some(PumpEnd::Failed(err)),
                )
            }
        };
        if !emit_all(events, &mut first_at) {
            return (PumpEnd::Cancelled, timing(started, first_at));
        }
        if let Some(end) = end {
            return (end, timing(started, first_at));
        }
    }
}

fn timing(started: Instant, first_at: Option<Instant>) -> PumpTiming {
    PumpTiming {
        first_delta: first_at.map(|f| f.duration_since(started)),
        streaming: first_at.map(|f| f.elapsed()),
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(tokio::time::Instant::from_std(d)).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    use wisp_types::{Lang, NoticeKind};

    use super::*;

    /// 收集事件的接收方
    #[derive(Default)]
    pub struct Collector {
        pub events: Mutex<Vec<StreamEvent>>,
        pub closed: AtomicBool,
    }

    impl Collector {
        pub fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        pub fn take(&self) -> Vec<StreamEvent> {
            self.events.lock().clone()
        }

        pub fn text(&self) -> String {
            self.events
                .lock()
                .iter()
                .filter_map(|e| match e {
                    StreamEvent::Delta { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect()
        }

        pub fn deltas(&self) -> usize {
            self.events
                .lock()
                .iter()
                .filter(|e| matches!(e, StreamEvent::Delta { .. }))
                .count()
        }

        pub fn terminal(&self) -> Option<StreamEvent> {
            self.events.lock().iter().rev().find(|e| e.is_terminal()).cloned()
        }
    }

    impl EventSink for Collector {
        fn deliver(&self, ev: &StreamEvent) -> bool {
            if self.closed.load(Ordering::Relaxed) {
                return false;
            }
            self.events.lock().push(ev.clone());
            true
        }
    }

    pub fn delta(t: &str) -> StreamEvent {
        StreamEvent::Delta { text: t.to_owned() }
    }

    pub fn done() -> StreamEvent {
        StreamEvent::Done {
            elapsed_ms: 10,
            usage: None,
            truncated: None,
            cached: false,
            no_text: false,
        }
    }

    fn spec(owner: Owner, lane: Lane, sink: Option<Sink>) -> OpenSpec {
        OpenSpec {
            owner,
            lane,
            model: ModelInfo {
                provider_id: "p".into(),
                model_id: "m".into(),
                model_label: "M".into(),
            },
            meta: Some(StreamEvent::Meta {
                detected_source_lang: Some(Lang::En),
                target_lang: Some(Lang::ZhCn),
                dict_mode: Some(false),
                low_confidence: Some(false),
            }),
            sink,
            trace: Some(7),
            run: None,
            capture_id: None,
        }
    }

    fn registry() -> (Registry, Arc<AtomicUsize>) {
        let firsts = Arc::new(AtomicUsize::new(0));
        let f = Arc::clone(&firsts);
        let reg = Registry::new(Box::new(move |tags| {
            assert_eq!(tags.trace, Some(7));
            f.fetch_add(1, Ordering::Relaxed);
        }));
        (reg, firsts)
    }

    #[test]
    fn start_and_meta_come_first_and_attach_replays_then_streams() {
        let (reg, firsts) = registry();
        let id = reg.new_id();
        reg.open(&id, spec(Owner::Unbound, Lane::Text, None));
        assert!(reg.emit(&id, delta("你好")));
        assert!(reg.emit(&id, delta("世界")));
        assert_eq!(firsts.load(Ordering::Relaxed), 1);
        let c = Collector::new();
        reg.attach(&id, c.clone(), |_| true).expect("attach");
        // 回放：Start、Meta、合并后的一个 Delta
        let replay = c.take();
        assert!(
            matches!(&replay[0], StreamEvent::Start { request_id, model_label, .. } if *request_id == id && model_label == "M")
        );
        assert!(matches!(replay[1], StreamEvent::Meta { .. }));
        assert_eq!(replay.len(), 3);
        assert_eq!(c.text(), "你好世界");
        // 实时
        assert!(reg.emit(&id, delta("！")));
        assert!(reg.emit(&id, done()));
        assert_eq!(c.text(), "你好世界！");
        assert!(matches!(c.terminal(), Some(StreamEvent::Done { .. })));
        assert_eq!(reg.outcome(&id), Some(Some(Outcome::Done)));
        // 结束之后不再接受事件
        assert!(!reg.emit(&id, delta("x")));
        // 结束之后 attach 只回放
        let late = Collector::new();
        reg.attach(&id, late.clone(), |_| true).expect("late attach");
        assert_eq!(late.text(), "你好世界！");
        assert!(matches!(late.terminal(), Some(StreamEvent::Done { .. })));
        assert!(reg.attach("r-404", Collector::new(), |_| true).is_err());
        // 不是宿主窗口：与不存在一样
        assert!(reg.attach(&id, Collector::new(), |_| false).is_err());
    }

    #[test]
    fn cancel_is_immediate_and_nothing_follows() {
        let (reg, _) = registry();
        let id = reg.new_id();
        let c = Collector::new();
        let token = reg.open(&id, spec(Owner::Panel(1), Lane::Text, Some(c.clone())));
        assert!(reg.emit(&id, delta("部分")));
        assert!(reg.cancel(&id));
        assert!(token.is_cancelled());
        // 取消之前已入队的 Delta 与随后的终止事件都被丢弃
        assert!(!reg.emit(&id, delta("迟到")));
        assert!(!reg.emit(&id, StreamEvent::Error(cancelled_error())));
        assert_eq!(c.text(), "部分");
        let events = c.take();
        assert!(matches!(events.last(), Some(StreamEvent::Error(e)) if e.code == ErrorCode::Cancelled));
        assert_eq!(events.iter().filter(|e| e.is_terminal()).count(), 1);
        assert!(!reg.cancel(&id), "重复取消是空操作");
    }

    #[test]
    fn same_lane_supersedes_other_lanes_coexist() {
        let (reg, _) = registry();
        let (a, b, c) = (reg.new_id(), reg.new_id(), reg.new_id());
        let sa = Collector::new();
        reg.open(&a, spec(Owner::Panel(3), Lane::Text, Some(sa.clone())));
        reg.open(&b, spec(Owner::Panel(3), Lane::Ocr, None));
        // 同一会话、同一通道的新请求：a 被取消并移除
        reg.open(&c, spec(Owner::Panel(3), Lane::Text, None));
        assert!(matches!(sa.terminal(), Some(StreamEvent::Error(e)) if e.code == ErrorCode::Cancelled));
        assert_eq!(reg.outcome(&a), None);
        assert_eq!(reg.outcome(&b), Some(None));
        assert_eq!(reg.outcome(&c), Some(None));
        // 别的宿主不受影响
        let d = reg.new_id();
        reg.open(&d, spec(Owner::Pinned("x".into()), Lane::Text, None));
        assert_eq!(reg.outcome(&c), Some(None));
        // 绑定：Unbound → Panel(4)
        let e = reg.new_id();
        reg.open(&e, spec(Owner::Unbound, Lane::Text, None));
        assert!(reg.bind(&e, Owner::Panel(4)));
        assert_eq!(reg.panel_sessions(), vec![3, 4]);
        // 会话结束
        let ended = reg.end_where(|o| *o == Owner::Panel(3));
        assert_eq!(ended.len(), 2);
        assert!(ended.iter().all(|e| e.was_running));
        assert_eq!(reg.panel_sessions(), vec![4]);
    }

    #[test]
    fn binding_never_cancels_a_newer_request() {
        let (reg, _) = registry();
        // Rust 发起的请求还没绑定（panel_prepare 还没返回）时，前端已经对会话 5 发起了更新的请求（立刻换了风格）
        let rust = reg.new_id();
        let rs = Collector::new();
        reg.open(&rust, spec(Owner::Unbound, Lane::Text, Some(rs.clone())));
        let newer = reg.new_id();
        reg.open(&newer, spec(Owner::Panel(5), Lane::Text, None));
        assert!(!reg.bind(&rust, Owner::Panel(5)), "更早的请求不能再绑定");
        assert!(matches!(rs.terminal(), Some(StreamEvent::Error(e)) if e.code == ErrorCode::Cancelled));
        assert_eq!(reg.outcome(&rust), None);
        assert_eq!(reg.outcome(&newer), Some(None), "更新的请求不受影响");
        // 正常顺序：绑定时取消的是更早的
        let older = reg.new_id();
        reg.open(&older, spec(Owner::Panel(6), Lane::Text, None));
        let late = reg.new_id();
        reg.open(&late, spec(Owner::Unbound, Lane::Text, None));
        assert!(reg.bind(&late, Owner::Panel(6)));
        assert_eq!(reg.outcome(&older), None);
        assert_eq!(reg.owner(&late), Some(Owner::Panel(6)));
    }

    #[test]
    fn cancel_checks_the_host() {
        let (reg, _) = registry();
        let id = reg.new_id();
        reg.open(&id, spec(Owner::Pinned("p1".into()), Lane::Text, None));
        let panel = |o: &Owner| matches!(o, Owner::Unbound | Owner::Panel(_));
        assert!(reg.cancel_if(&id, panel).is_err(), "面板不能取消 pinned 的流");
        assert_eq!(reg.outcome(&id), Some(None));
        assert_eq!(reg.cancel_if("r-404", panel).ok(), Some(false), "不存在的请求是空操作");
        assert_eq!(
            reg.cancel_if(&id, |o| *o == Owner::Pinned("p1".into())).ok(),
            Some(true)
        );
    }

    #[test]
    fn transfer_moves_ownership_and_capture() {
        let (reg, _) = registry();
        let id = reg.new_id();
        let mut s = spec(Owner::Panel(9), Lane::Ocr, None);
        s.capture_id = Some("c-1".into());
        reg.open(&id, s);
        let (prev, capture) = reg.transfer(&id, "p1").expect("transfer");
        assert_eq!(prev, Owner::Panel(9));
        assert_eq!(capture.as_deref(), Some("c-1"));
        assert!(reg.end_where(|o| *o == Owner::Panel(9)).is_empty());
        assert_eq!(reg.end_where(|o| *o == Owner::Pinned("p1".into())).len(), 1);
    }

    #[test]
    fn eviction_keeps_running_requests() {
        let (reg, _) = registry();
        let running = reg.new_id();
        reg.open(&running, spec(Owner::Pinned("keep".into()), Lane::Text, None));
        for i in 0..(MAX_ENTRIES + 10) {
            let id = reg.new_id();
            reg.open(&id, spec(Owner::Pinned(format!("w{i}")), Lane::Text, None));
            assert!(reg.emit(&id, done()));
        }
        assert!(reg.len() <= MAX_ENTRIES);
        assert_eq!(reg.outcome(&running), Some(None));
    }

    #[tokio::test]
    async fn pump_post_processes_and_coalesces() {
        let (reg, firsts) = registry();
        let id = reg.new_id();
        let c = Collector::new();
        reg.open(&id, spec(Owner::Panel(1), Lane::Text, Some(c.clone())));
        let (tx, rx) = mpsc::channel(256);
        // 每 1 ms 一个 token，共 200 个（16 ms 合并后每秒 ≤ 约 63 个 Delta）；开头带翻译前言
        tokio::spawn(async move {
            let _ = tx
                .send(StreamEvent::Start {
                    request_id: "x".into(),
                    provider_id: "p".into(),
                    model_id: "m".into(),
                    model_label: "M".into(),
                    upstream_id: Some("up".into()),
                })
                .await;
            let _ = tx.send(delta("Translation: ")).await;
            for i in 0..200 {
                let _ = tx.send(delta(if i % 2 == 0 { "字" } else { "词" })).await;
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            let _ = tx
                .send(StreamEvent::Notice {
                    kind: NoticeKind::Retried,
                })
                .await;
            let _ = tx.send(done()).await;
        });
        let started = Instant::now();
        let (end, timing) = pump(&reg, &id, rx, PostKind::Translate, "word").await;
        let elapsed = started.elapsed();
        assert!(matches!(end, PumpEnd::Done { no_text: false, .. }), "{end:?}");
        assert!(timing.first_delta.is_some());
        assert_eq!(firsts.load(Ordering::Relaxed), 1);
        let text = c.text();
        assert_eq!(text.chars().count(), 200, "前言被剥掉、正文一字不差：{text}");
        // 16 ms 合并：Delta 个数 ≤ 耗时 / 16 ms + 余量
        let budget = usize::try_from(elapsed.as_millis() / 16).unwrap_or(usize::MAX) + 4;
        assert!(c.deltas() <= budget, "deltas={} budget={budget}", c.deltas());
        // 只有一个 Start（wisp-llm 的不转发），Notice 在 Done 之前，最后一段不丢
        let events = c.take();
        assert_eq!(
            events.iter().filter(|e| matches!(e, StreamEvent::Start { .. })).count(),
            1
        );
        assert!(matches!(events.last(), Some(StreamEvent::Done { .. })));
        assert!(matches!(events[events.len() - 2], StreamEvent::Notice { .. }));
    }

    /// 记录每个事件到达时刻的接收方
    #[derive(Default)]
    struct Timed {
        events: Mutex<Vec<(Instant, StreamEvent)>>,
    }

    impl EventSink for Timed {
        fn deliver(&self, ev: &StreamEvent) -> bool {
            self.events.lock().push((Instant::now(), ev.clone()));
            true
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pump_keeps_the_16ms_cadence_when_the_channel_never_drains() {
        // 通道里始终有积压（大量事件早已入队）：偏向接收的 select! 抢不到计时分支，冲刷只能靠 Delta 进缓冲后就地检查
        const N: usize = 200_000;
        let (reg, _) = registry();
        let id = reg.new_id();
        let sink = Arc::new(Timed::default());
        reg.open(&id, spec(Owner::Panel(1), Lane::Text, Some(sink.clone())));
        let (tx, rx) = mpsc::channel(N + 8);
        for i in 0..N {
            tx.try_send(delta(if i % 2 == 0 { "字" } else { "词" })).expect("queue");
        }
        tx.try_send(done()).expect("queue");
        let started = Instant::now();
        let (end, _) = pump(&reg, &id, rx, PostKind::Raw, "").await;
        let elapsed = started.elapsed();
        assert!(matches!(end, PumpEnd::Done { .. }), "{end:?}");
        let events = sink.events.lock().clone();
        let deltas: Vec<(Instant, usize)> = events
            .iter()
            .filter_map(|(t, e)| match e {
                StreamEvent::Delta { text } => Some((*t, text.chars().count())),
                _ => None,
            })
            .collect();
        assert_eq!(deltas.iter().map(|(_, n)| n).sum::<usize>(), N, "一字不差");
        // 相邻两批之间至少隔一个合并窗口（不超过约 63 批 / s）；最后一批是 Done 之前的冲刷，不计
        for w in deltas.windows(2).take(deltas.len().saturating_sub(2)) {
            let gap = w[1].0.duration_since(w[0].0);
            assert!(gap >= Duration::from_millis(15), "gap {gap:?}");
        }
        // 消费持续了好几个窗口时必须分批到达，而不是最后一次性到达
        if elapsed >= Duration::from_millis(80) {
            let windows = usize::try_from(elapsed.as_millis() / 16).unwrap_or(usize::MAX);
            assert!(
                deltas.len() >= windows / 2,
                "{} batches in {elapsed:?} (expected about {windows})",
                deltas.len()
            );
        } else {
            log::warn!("pump drained {N} events in {elapsed:?}; cadence check skipped");
        }
    }

    #[tokio::test]
    async fn pump_drops_deltas_queued_before_cancel() {
        let (reg, _) = registry();
        let id = reg.new_id();
        let c = Collector::new();
        reg.open(&id, spec(Owner::Panel(1), Lane::Text, Some(c.clone())));
        let (tx, rx) = mpsc::channel(256);
        tx.send(delta("先到的")).await.expect("send");
        let reg = Arc::new(reg);
        let pumping = {
            let reg = Arc::clone(&reg);
            let id = id.clone();
            tokio::spawn(async move { pump(&reg, &id, rx, PostKind::Raw, "").await })
        };
        tokio::time::sleep(Duration::from_millis(40)).await;
        // 取消之前已入队的 Delta 仍会被泵读到，但不能再到达前端
        for _ in 0..64 {
            tx.send(delta("迟到")).await.expect("send");
        }
        assert!(reg.cancel(&id));
        for _ in 0..8 {
            let _ = tx.send(delta("更迟")).await;
        }
        let _ = tx.send(StreamEvent::Error(cancelled_error())).await;
        let (end, _) = tokio::time::timeout(Duration::from_secs(2), pumping)
            .await
            .expect("pump ends")
            .expect("join");
        assert_eq!(end, PumpEnd::Cancelled);
        assert_eq!(c.text(), "先到的");
        assert!(matches!(c.terminal(), Some(StreamEvent::Error(e)) if e.code == ErrorCode::Cancelled));
        assert!(tx.is_closed(), "取消后丢弃接收端，wisp-llm 随之关闭连接");
    }

    #[tokio::test]
    async fn pump_ocr_no_text_and_failed_keeps_partial_text() {
        let (reg, _) = registry();
        let id = reg.new_id();
        let c = Collector::new();
        reg.open(&id, spec(Owner::Panel(1), Lane::Ocr, Some(c.clone())));
        let (tx, rx) = mpsc::channel(8);
        tx.send(delta("<NO_TEXT>")).await.expect("send");
        tx.send(done()).await.expect("send");
        let (end, _) = pump(
            &reg,
            &id,
            rx,
            PostKind::Ocr {
                mode: wisp_types::OcrMode::Lines,
            },
            "",
        )
        .await;
        assert!(matches!(end, PumpEnd::Done { no_text: true, .. }), "{end:?}");
        assert_eq!(c.text(), "");
        assert!(matches!(c.terminal(), Some(StreamEvent::Done { no_text: true, .. })));

        let id = reg.new_id();
        let c = Collector::new();
        reg.open(&id, spec(Owner::Panel(1), Lane::Text, Some(c.clone())));
        let (tx, rx) = mpsc::channel(8);
        tx.send(delta("半句话")).await.expect("send");
        tx.send(StreamEvent::Error(LlmError::new(ErrorCode::Network, "connection lost")))
            .await
            .expect("send");
        let (end, _) = pump(&reg, &id, rx, PostKind::Translate, "x").await;
        assert!(matches!(end, PumpEnd::Failed(ref e) if e.code == ErrorCode::Network));
        assert_eq!(c.text(), "半句话");
        assert_eq!(reg.outcome(&id), Some(Some(Outcome::Failed)));
    }
}
