//! 读取选区（PRIMARY / CLIPBOARD）：ConvertSelection → SelectionNotify → GetProperty，支持 INCR 增量传输。
//!
//! 每个读取会话从窗口池里借一个隐藏的**请求方窗口**，hub 线程按 SelectionNotify 的 requestor 与 PropertyNotify 的
//! window 把事件转发到对应会话的 channel，因此剪贴板历史读取（图片走 INCR 时可能要几百毫秒）、取词的 PRIMARY 读取、
//! 模拟复制前的快照可以**并发**进行，互不阻塞。读取方在自己的线程上带超时等待（所有跨进程等待都有硬上限）。
//!
//! 会话内可以同时发出多个转换请求（快照按优先级每批最多 4 个，减少往返），每个请求使用轮换的属性原子。会话结束时，
//! 若对端还可能往这个窗口写数据（超时、INCR 中途放弃——包括 INCR 握手刚完成就因声明大小超限而放弃），窗口直接
//! 销毁而不放回池中——迟到的数据块永远不会混进下一次读取。
//! 状态转移规则在纯逻辑模块 `xfer_logic` 里（可在任何平台单测）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;

use parking_lot::Mutex;
use x11rb::CURRENT_TIME;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, SelectionNotifyEvent, Window, WindowClass,
};

use crate::linux::xfer_logic::{self as logic, NotifyPlan, State};
pub(crate) use crate::linux::xfer_logic::{Fetched, LimitMode, decode_text, parse_atoms};

use super::X11;

/// 转发给读取会话的事件
#[derive(Debug, Clone)]
enum XEvt {
    Notify(SelectionNotifyEvent),
    /// 请求方窗口上某个属性有了新值（INCR 的下一块）
    NewValue(Atom),
}

/// hub → 读取会话的路由，以及空闲的请求方窗口池
#[derive(Default)]
pub(crate) struct Router {
    /// 正在使用的请求方窗口 → 会话的 channel
    routes: Mutex<HashMap<Window, Sender<XEvt>>>,
    /// 空闲、可复用的请求方窗口（数量不超过曾经的最大并发数，通常 1–3 个）
    free: Mutex<Vec<Window>>,
    closed: AtomicBool,
    /// 下一个要用的属性原子序号
    next_prop: AtomicUsize,
}

impl Router {
    /// hub 线程调用：SelectionNotify 转给请求方窗口对应的会话；没有会话（已超时放弃）时丢弃
    pub(crate) fn deliver_notify(&self, e: SelectionNotifyEvent) {
        if let Some(tx) = self.routes.lock().get(&e.requestor) {
            let _ = tx.send(XEvt::Notify(e));
        }
    }

    /// hub 线程调用：请求方窗口上的属性变化。返回该窗口是否属于读取会话（是则调用方不再处理）
    pub(crate) fn deliver_property(&self, window: Window, atom: Atom, new_value: bool) -> bool {
        match self.routes.lock().get(&window) {
            Some(tx) => {
                if new_value {
                    let _ = tx.send(XEvt::NewValue(atom));
                }
                true
            }
            None => false,
        }
    }

    /// 连接断开：唤醒所有等待者，不再开新会话
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.routes.lock().clear();
    }
}

/// 属性原子池大小
const PROP_POOL: usize = 16;

struct Xfer {
    target: Atom,
    prop: Atom,
    limit: usize,
    mode: LimitMode,
    state: State,
    /// 是否完成过 INCR 握手（读取并删除了 INCR 属性）：之后对端会往窗口里写，中途结束时窗口不能复用
    entered_incr: bool,
}

/// 读取会话：独占一个请求方窗口，结束时归还（或销毁）
pub(crate) struct Session<'a> {
    x: &'a X11,
    win: Window,
    rx: Receiver<XEvt>,
    /// 对端可能还会往窗口里写：结束时销毁窗口而不是放回池中
    tainted: bool,
}

impl X11 {
    /// 开启读取会话（不等待其他会话：每个会话有自己的请求方窗口）。连接已断开或无法创建窗口时返回 None
    pub(crate) fn read_session(&self) -> Option<Session<'_>> {
        if self.reads.closed.load(Ordering::Acquire) {
            return None;
        }
        let win = match self.reads.free.lock().pop() {
            Some(w) => w,
            None => self.create_reader_window()?,
        };
        let (tx, rx) = mpsc::channel();
        self.reads.routes.lock().insert(win, tx);
        Some(Session {
            x: self,
            win,
            rx,
            tainted: false,
        })
    }

    fn create_reader_window(&self) -> Option<Window> {
        let win = self.conn.generate_id().ok()?;
        self.conn
            .create_window(
                0,
                win,
                self.root,
                -10,
                -10,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &CreateWindowAux::new()
                    .override_redirect(1)
                    .event_mask(EventMask::PROPERTY_CHANGE),
            )
            .ok()?;
        Some(win)
    }

    fn sel_prop(&self) -> Atom {
        let i = self.reads.next_prop.fetch_add(1, Ordering::Relaxed) % PROP_POOL;
        self.intern(&format!("_WISP_SEL_{i}")).unwrap_or(self.atoms.WISP_SEL)
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        self.x.reads.routes.lock().remove(&self.win);
        if self.tainted || self.x.reads.closed.load(Ordering::Acquire) {
            // 迟到的应答 / INCR 块会写到一个不存在的窗口上（对端收到 BadWindow 后放弃），不会混进下一次读取
            let _ = self.x.conn.destroy_window(self.win);
            let _ = self.x.conn.flush();
        } else {
            self.x.reads.free.lock().push(self.win);
        }
    }
}

impl Session<'_> {
    /// 读取多个目标（并行发出请求），返回与 `targets` 一一对应的结果；整体不超过 `deadline`
    pub(crate) fn fetch_many(
        &mut self,
        selection: Atom,
        targets: &[(Atom, usize, LimitMode)],
        deadline: Instant,
    ) -> Vec<Fetched> {
        self.fetch_many_with(selection, targets, deadline, deadline)
    }

    /// 同 [`Session::fetch_many`]，但对「owner 首次应答」单独设上限：`notify_deadline` 内没有收到 SelectionNotify
    /// 的请求按超时处理；已经开始 INCR 的传输可以持续到 `deadline`
    pub(crate) fn fetch_many_with(
        &mut self,
        selection: Atom,
        targets: &[(Atom, usize, LimitMode)],
        notify_deadline: Instant,
        deadline: Instant,
    ) -> Vec<Fetched> {
        let mut out = Vec::with_capacity(targets.len());
        for chunk in targets.chunks(PROP_POOL) {
            if self.tainted {
                // 前面的转换可能还有对端在往这个窗口写（属性原子会轮换回来）：后续请求换一个新窗口
                self.renew_window();
            }
            let mut xfers: Vec<Xfer> = chunk
                .iter()
                .map(|&(target, limit, mode)| Xfer {
                    target,
                    prop: self.x.sel_prop(),
                    limit,
                    mode,
                    state: State::WaitNotify,
                    entered_incr: false,
                })
                .collect();
            for xf in &xfers {
                let _ = self
                    .x
                    .conn
                    .convert_selection(self.win, selection, xf.target, xf.prop, CURRENT_TIME);
            }
            let _ = self.x.conn.flush();
            self.drive(&mut xfers, notify_deadline.min(deadline), deadline);
            for xf in xfers {
                if logic::may_still_write(&xf.state, xf.entered_incr) {
                    self.tainted = true;
                }
                out.push(match xf.state {
                    State::Done(f) => f,
                    _ => Fetched::Timeout,
                });
            }
        }
        out
    }

    /// 读取单个目标
    pub(crate) fn fetch(
        &mut self,
        selection: Atom,
        target: Atom,
        limit: usize,
        mode: LimitMode,
        deadline: Instant,
    ) -> Fetched {
        self.fetch_many(selection, &[(target, limit, mode)], deadline)
            .pop()
            .unwrap_or(Fetched::Timeout)
    }

    /// 换一个新的请求方窗口并销毁旧的（会话中途被污染时）；旧窗口已经排队的事件一并丢弃。创建失败时沿用旧窗口
    fn renew_window(&mut self) {
        let Some(fresh) = self.x.create_reader_window() else {
            return;
        };
        {
            let mut routes = self.x.reads.routes.lock();
            if let Some(tx) = routes.remove(&self.win) {
                routes.insert(fresh, tx);
            }
        }
        let _ = self.x.conn.destroy_window(self.win);
        let _ = self.x.conn.flush();
        while self.rx.try_recv().is_ok() {}
        self.win = fresh;
        self.tainted = false;
    }

    fn next_event(&mut self, deadline: Instant) -> Option<XEvt> {
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        self.rx.recv_timeout(deadline - now).ok()
    }

    fn drive(&mut self, xfers: &mut [Xfer], notify_deadline: Instant, deadline: Instant) {
        while xfers.iter().any(|x| !matches!(x.state, State::Done(_))) {
            let waiting_notify = xfers.iter().any(|x| matches!(x.state, State::WaitNotify));
            let until = if waiting_notify { notify_deadline } else { deadline };
            let Some(ev) = self.next_event(until) else {
                if waiting_notify && Instant::now() < deadline {
                    // owner 首次应答超时：这些请求放弃（窗口会被销毁），已在 INCR 中的继续
                    for xf in xfers.iter_mut().filter(|x| matches!(x.state, State::WaitNotify)) {
                        xf.state = State::Done(Fetched::Timeout);
                    }
                    continue;
                }
                break;
            };
            match ev {
                XEvt::Notify(n) => {
                    let none = u32::from(AtomEnum::NONE);
                    let idx = xfers.iter().position(|xf| {
                        matches!(xf.state, State::WaitNotify)
                            && xf.target == n.target
                            && (n.property == xf.prop || n.property == none)
                    });
                    if let Some(i) = idx {
                        let (next, handshake) = self.on_notify(&xfers[i], &n);
                        // 即使声明大小超限、立即放弃（Done(TooLarge)），握手已经完成，owner 也会写第一块
                        xfers[i].entered_incr |= handshake;
                        xfers[i].state = next;
                    }
                }
                XEvt::NewValue(atom) => {
                    if let Some(xf) = xfers
                        .iter_mut()
                        .find(|xf| xf.prop == atom && matches!(xf.state, State::Incr { .. }))
                    {
                        let state = std::mem::replace(&mut xf.state, State::WaitNotify);
                        xf.state = self.on_incr_chunk(xf.prop, xf.limit, xf.mode, state);
                    }
                }
            }
        }
    }

    /// 处理 SelectionNotify，返回新状态与「是否完成了 INCR 握手」（`xfer_logic::handshake_started`）
    fn on_notify(&self, xf: &Xfer, n: &SelectionNotifyEvent) -> (State, bool) {
        let conn = &self.x.conn;
        let win = self.win;
        let property_none = n.property == u32::from(AtomEnum::NONE);
        // 先只取类型与长度
        let head = if property_none {
            None
        } else {
            conn.get_property(false, win, xf.prop, AtomEnum::ANY, 0, 0)
                .ok()
                .and_then(|c| c.reply().ok())
        };
        let (head_ty, total) = head
            .as_ref()
            .map_or((u32::from(AtomEnum::NONE), 0), |h| (h.type_, h.bytes_after as usize));
        let plan = logic::plan_notify(
            property_none || head.is_none(),
            head_ty,
            total,
            self.x.atoms.INCR,
            xf.limit,
            xf.mode,
        );
        let state = match plan {
            NotifyPlan::Refused => State::Done(Fetched::Refused),
            NotifyPlan::StartIncr => {
                // 读取（并删除）INCR 属性即表示开始接收；它的值是总大小的下限
                let hint = conn
                    .get_property(true, win, xf.prop, AtomEnum::ANY, 0, 1)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .and_then(|r| r.value32().and_then(|mut v| v.next()))
                    .unwrap_or(0) as usize;
                let _ = conn.flush();
                logic::start_incr(hint, xf.limit, xf.mode)
            }
            NotifyPlan::TooLarge(size) => {
                let _ = conn.delete_property(win, xf.prop);
                let _ = conn.flush();
                State::Done(Fetched::TooLarge(size))
            }
            NotifyPlan::Read { want, truncated } => {
                match conn
                    .get_property(true, win, xf.prop, AtomEnum::ANY, 0, (want as u32).div_ceil(4))
                    .ok()
                    .and_then(|c| c.reply().ok())
                {
                    Some(r) => {
                        if truncated {
                            // 被截断的读取不会自动删除属性（还有剩余数据），手动删掉
                            let _ = conn.delete_property(win, xf.prop);
                            let _ = conn.flush();
                        }
                        logic::finish_read(r.type_, r.format, r.value, want, truncated)
                    }
                    None => State::Done(Fetched::Refused),
                }
            }
        };
        (state, logic::handshake_started(plan))
    }

    fn on_incr_chunk(&self, prop: Atom, limit: usize, mode: LimitMode, state: State) -> State {
        let conn = &self.x.conn;
        let win = self.win;
        let Some(head) = conn
            .get_property(false, win, prop, AtomEnum::ANY, 0, 0)
            .ok()
            .and_then(|c| c.reply().ok())
        else {
            return State::Done(Fetched::Refused);
        };
        if head.type_ == u32::from(AtomEnum::NONE) {
            // 属性不存在：迟到 / 重复的 NewValue 通知，继续等下一块
            return logic::incr_chunk(state, None, limit, mode);
        }
        // 读取并删除，删除即请求下一块
        let Some(r) = conn
            .get_property(true, win, prop, AtomEnum::ANY, 0, head.bytes_after.div_ceil(4))
            .ok()
            .and_then(|c| c.reply().ok())
        else {
            return State::Done(Fetched::Refused);
        };
        let _ = conn.flush();
        let chunk = (r.type_ != u32::from(AtomEnum::NONE)).then_some((r.type_, r.format, r.value));
        logic::incr_chunk(state, chunk, limit, mode)
    }
}
