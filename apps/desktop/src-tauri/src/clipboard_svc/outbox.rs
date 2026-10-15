//! `clipboard:changed` 的投递（面板可见时才推送）。
//!
//! - 剪贴板视图**已显示**（ready 之后）：立即推送（先补发排队中的，保持顺序）；
//! - **准备中**（prepare 之后、ready 之前），或**即将打开**（`first_page` 刚把首页交给编排器、面板还没 prepare，
//!   见 [`EXPECT_WINDOW`]）：先排队，视图 ready 后按顺序补发；
//! - 其余（面板隐藏、正在隐藏、显示的是别的视图）：不推送——缓存照常更新，下次打开时首页就是新的。
//!
//! 为什么要排队：视图在暂存层里挂载、订阅事件，然后才调用 `panel_ready`；ready 之前推送的事件可能先于订阅到达而被
//! 丢掉（例如首页交出之后、视图挂载之前刚好完成的文件 exists 检查、缩略图、新的复制）。窗口层没有「已显示」的通知，
//! 所以排队期间每帧（[`FRAME`]）查一次面板状态，最多 [`MAX_WAIT`]（面板 ready 的看门狗是 1 s）。只有排队里有事件时
//! 才会查，空闲时零唤醒。
//!
//! **顺序**：面板状态的读取、排队、补发与本条的发出都在投递锁内完成（`emit_to` 只是投递、不阻塞），因此事件到达前端的
//! 顺序与调用 [`Engine::emit`] 的顺序严格一致——补发排队中的事件时，别的线程新到的事件只能排在它们之后。调用方又在
//! 串行区内调用它（见 `engine.rs`「事件的顺序」），同一条目的事件顺序因此与库里的改动顺序一致。
//!
//! 前端按 id 幂等处理每种事件（`inserted` 遇到已有的 id 视为更新）：排队补发的事件可能与首页内容重叠。
//!
//! **已知缺口**：「ready 之后才推送」假定视图在调用 `panel_ready` 之前已经订阅完成；集成态
//! 的剪贴板视图在挂载的 effect 里异步 `listen`，没有等它完成就 ready，ready 之后紧接着补发的事件仍可能丢失。

use std::time::{Duration, Instant};

use parking_lot::{Mutex, MutexGuard};
use wisp_types::ClipboardChanged;

use super::engine::Engine;
use super::port::ClipView;

/// 排队期间查面板状态的间隔（一帧）
pub(crate) const FRAME: Duration = Duration::from_millis(16);
/// 排队的上限时长：超过仍没显示（ready 看门狗已放弃、会话被取代）就丢弃
pub(crate) const MAX_WAIT: Duration = Duration::from_millis(1500);
/// `first_page` 之后多久之内，面板还没显示剪贴板视图也视为「即将打开」（编排器取完首页随即 prepare，通常不到 1 ms）
pub(crate) const EXPECT_WINDOW: Duration = Duration::from_millis(500);
/// 排队条数上限（准备期只有一两百毫秒，正常不会接近；超出时丢弃最旧的）
const MAX_QUEUED: usize = 256;

/// 待投递的事件
#[derive(Default)]
pub(crate) struct Outbox {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    queue: Vec<ClipboardChanged>,
    /// 视图「即将打开」到这个时刻为止
    expect_until: Option<Instant>,
    /// 这一批排队的放弃时刻（第一条入队时设定，队列清空时清除）
    deadline: Option<Instant>,
    /// 是否已经排了下一次检查
    scheduled: bool,
}

impl Inner {
    fn expecting(&self, now: Instant) -> bool {
        self.expect_until.is_some_and(|t| now < t)
    }
}

impl Engine {
    /// 推送 `clipboard:changed`（规则见模块文档）。不阻塞
    pub(crate) fn emit(&self, change: ClipboardChanged) {
        let mut ob = self.outbox.inner.lock();
        // 持锁读面板状态：与其他线程的投递线性排列（锁外读到的状态可能已经过时）
        let view = self.host.clipboard_view();
        let now = Instant::now();
        match view {
            ClipView::Shown(_) => {
                // 持锁发出：排队中的在前、本条在后
                self.send_queued(&mut ob);
                self.host.emit(&change);
            }
            ClipView::Preparing(_) => self.enqueue(ob, change, now),
            ClipView::Hidden if ob.expecting(now) => self.enqueue(ob, change, now),
            ClipView::Hidden => {}
        }
    }

    /// 发出排队中的全部事件（调用方持有投递锁）
    fn send_queued(&self, ob: &mut Inner) {
        ob.deadline = None;
        for c in ob.queue.drain(..) {
            self.host.emit(&c);
        }
    }

    /// 剪贴板视图即将打开（`first_page` 交出首页时调用）：在 [`EXPECT_WINDOW`] 内产生的事件排队等视图显示
    pub(crate) fn expect_clipboard_view(&self) {
        self.outbox.inner.lock().expect_until = Some(Instant::now() + EXPECT_WINDOW);
    }

    fn enqueue(&self, mut ob: MutexGuard<'_, Inner>, change: ClipboardChanged, now: Instant) {
        if ob.queue.len() >= MAX_QUEUED {
            ob.queue.remove(0);
            log::debug!("clipboard event queue full; oldest dropped");
        }
        ob.queue.push(change);
        ob.deadline.get_or_insert(now + MAX_WAIT);
        if !ob.scheduled {
            ob.scheduled = true;
            drop(ob);
            self.schedule_flush();
        }
    }

    fn schedule_flush(&self) {
        let me = self.me.clone();
        self.exec.after(
            FRAME,
            Box::new(move || {
                if let Some(engine) = me.upgrade() {
                    engine.flush_outbox();
                }
            }),
        );
    }

    /// 排队中的事件：视图已显示就补发；还在准备（或即将打开）就下一帧再看；超时或面板已不显示剪贴板视图就丢弃
    pub(crate) fn flush_outbox(&self) {
        let mut ob = self.outbox.inner.lock();
        ob.scheduled = false;
        if ob.queue.is_empty() {
            ob.deadline = None;
            return;
        }
        let view = self.host.clipboard_view();
        let now = Instant::now();
        let expired = ob.deadline.is_some_and(|d| now >= d);
        match view {
            // 持锁补发（与 emit 相同的理由）
            ClipView::Shown(_) => self.send_queued(&mut ob),
            ClipView::Preparing(_) if !expired => self.reschedule(ob),
            ClipView::Hidden if !expired && ob.expecting(now) => self.reschedule(ob),
            _ => {
                log::debug!(
                    "clipboard events dropped count={} reason={}",
                    ob.queue.len(),
                    if expired { "not shown in time" } else { "view gone" }
                );
                ob.queue.clear();
                ob.deadline = None;
            }
        }
    }

    fn reschedule(&self, mut ob: MutexGuard<'_, Inner>) {
        ob.scheduled = true;
        drop(ob);
        self.schedule_flush();
    }

    /// 排队中的事件数（单测用）
    #[cfg(test)]
    pub(crate) fn queued_events(&self) -> usize {
        self.outbox.inner.lock().queue.len()
    }
}
