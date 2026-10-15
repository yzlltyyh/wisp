//! 作为选区 owner 提供数据：写剪贴板、恢复快照（CLIPBOARD），以及往 xterm 一类「粘贴键读
//! PRIMARY」的终端粘贴时临时占有 PRIMARY。两个选区的内容分别保存。
//!
//! 支持 ICCCM 要求的 TARGETS / TIMESTAMP / MULTIPLE，大数据用 INCR 分块（每块 256 KB）发送。INCR 期间要收到请求方
//! 窗口的 PropertyNotify（DELETE 表示要下一块）与 DestroyNotify（请求方退出就立即丢弃传输），所以临时修改本连接在
//! 该窗口上的事件掩码——只对本模块新订阅的外部窗口这样做，传输结束后按窗口恢复；根窗口、XSETTINGS 管理器窗口等
//! 本连接常驻订阅的窗口一律不动。不再响应的请求方的传输 10 s 后丢弃（任何剪贴板活动都会顺带清理）。
//! 所有处理都在 hub 线程上进行，只发请求不等对端回复（MULTIPLE 读取请求方属性除外，那是对 X 服务器的往返）。

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, EventMask, PropMode, SELECTION_NOTIFY_EVENT,
    SelectionClearEvent, SelectionNotifyEvent, SelectionRequestEvent, Window,
};
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};

use super::X11;
use crate::linux::xfer_logic::owner_time_ok;

/// 单块大小：超过它的数据走 INCR
const CHUNK: usize = 256 * 1024;
/// 不再响应的 INCR 请求方的超时
const INCR_STALE: Duration = Duration::from_secs(10);

/// Wisp 提供的一个目标
#[derive(Debug, Clone)]
pub(crate) struct OwnedItem {
    pub target: Atom,
    pub ty: Atom,
    pub format: u8,
    pub data: Arc<Vec<u8>>,
}

/// 当前持有的一个选区的内容
#[derive(Debug)]
pub(crate) struct Owned {
    pub items: Vec<OwnedItem>,
    /// 成为 owner 的服务器时间。`server_time` 超时（hub 线程正忙）时先以 CurrentTime 占有，之后由 XFixes 的
    /// owner 变化事件补上；补上之前不做请求时间校验（`xfer_logic::owner_time_ok`）
    stamp: OnceLock<u32>,
    /// 是否让 Wisp 自己的剪贴板监听看到这次写入（恢复快照、排除历史的写入为 false）
    pub report: bool,
    /// 写入时带了「排除历史」标记（读取时视为 concealed）
    pub exclude_from_history: bool,
}

impl Owned {
    /// 成为 owner 的服务器时间（未知时为 None）
    pub(crate) fn stamp(&self) -> Option<u32> {
        self.stamp.get().copied()
    }
}

struct IncrOut {
    requestor: Window,
    property: Atom,
    ty: Atom,
    format: u8,
    data: Arc<Vec<u8>>,
    offset: usize,
    last: Instant,
}

#[derive(Default)]
struct IncrTable {
    transfers: Vec<IncrOut>,
    /// 由本模块修改过事件掩码的外部窗口（传输全部结束后恢复为不订阅）
    watched: Vec<Window>,
}

/// owner 状态
#[derive(Default)]
pub(crate) struct Owner {
    clipboard: Mutex<Option<Arc<Owned>>>,
    primary: Mutex<Option<Arc<Owned>>>,
    incr: Mutex<IncrTable>,
}

impl Owner {
    fn slot(&self, x: &X11, selection: Atom) -> Option<&Mutex<Option<Arc<Owned>>>> {
        if selection == x.atoms.CLIPBOARD {
            Some(&self.clipboard)
        } else if selection == u32::from(AtomEnum::PRIMARY) {
            Some(&self.primary)
        } else {
            None
        }
    }

    /// 当前持有的 CLIPBOARD 内容（不检查服务器上的实际 owner）
    pub(crate) fn clipboard(&self) -> Option<Arc<Owned>> {
        self.clipboard.lock().clone()
    }
}

/// 成为 `selection`（CLIPBOARD 或 PRIMARY）的 owner
pub(crate) fn take(
    x: &X11,
    selection: Atom,
    items: Vec<OwnedItem>,
    report: bool,
    exclude_from_history: bool,
) -> Result<(), String> {
    let slot = x.owner.slot(x, selection).ok_or("unsupported selection")?;
    prune_stale(x);
    // ICCCM 要求用真实的服务器时间占有选区；取不到（hub 线程 50 ms 内没处理到 PropertyNotify）时用 CurrentTime，
    // 时间戳留空，由 XFixes 事件补上（见 `learn_stamp`）
    let ts = x.server_time(Duration::from_millis(50));
    let owned = Arc::new(Owned {
        items,
        stamp: ts.map(OnceLock::from).unwrap_or_default(),
        report,
        exclude_from_history,
    });
    // 先放好内容再占有：剪贴板管理器收到 XFixes 通知后会立即来请求
    *slot.lock() = Some(Arc::clone(&owned));
    x.conn
        .set_selection_owner(x.win, selection, ts.unwrap_or(CURRENT_TIME))
        .map_err(|e| e.to_string())?
        .check()
        .map_err(|e| e.to_string())?;
    if current_owner(x, selection)? != x.win {
        let mut cur = slot.lock();
        if cur.as_ref().is_some_and(|o| Arc::ptr_eq(o, &owned)) {
            cur.take();
        }
        return Err("another client took the selection".to_owned());
    }
    if ts.is_none() {
        // 时间戳未知时，排队中的旧 SelectionClear 可能在 SetSelectionOwner 生效之前按「已不是 owner」清掉了槽位；
        // 刚刚已经确认 Wisp 是 owner，补回（另一个线程已经换上了更新的内容时不动）
        let mut cur = slot.lock();
        if cur.is_none() {
            *cur = Some(owned);
        }
    }
    Ok(())
}

/// 服务器上 `selection` 当前的 owner 窗口（一次往返，不涉及对端应用）
pub(crate) fn current_owner(x: &X11, selection: Atom) -> Result<Window, String> {
    Ok(x.conn
        .get_selection_owner(selection)
        .map_err(|e| e.to_string())?
        .reply()
        .map_err(|e| e.to_string())?
        .owner)
}

/// XFixes 通知「Wisp 成为了 `selection` 的 owner」（hub 线程）：补上 `take` 时没能取到的时间戳
pub(crate) fn learn_stamp(x: &X11, selection: Atom, time: u32) {
    if let Some(slot) = x.owner.slot(x, selection)
        && let Some(o) = slot.lock().as_ref()
    {
        let _ = o.stamp.set(time);
    }
}

/// 清空剪贴板（原剪贴板为空时的「恢复」）
pub(crate) fn clear(x: &X11) -> Result<(), String> {
    let ts = x.server_time(Duration::from_millis(50)).unwrap_or(CURRENT_TIME);
    x.owner.clipboard.lock().take();
    x.conn
        .set_selection_owner(NONE, x.atoms.CLIPBOARD, ts)
        .map_err(|e| e.to_string())?
        .check()
        .map_err(|e| e.to_string())
}

/// 处理 SelectionRequest（hub 线程）
pub(crate) fn handle_request(x: &X11, ev: &SelectionRequestEvent) {
    if ev.owner != x.win {
        return;
    }
    // 过时的客户端把 property 设为 None：按 ICCCM 用 target 作为属性名
    let property = if ev.property == NONE { ev.target } else { ev.property };
    let owned = x.owner.slot(x, ev.selection).and_then(|s| s.lock().clone());
    let ok = match owned {
        // 请求方是本连接自己的窗口：只会出现在「读取检查完 owner 之后、发出转换之前，另一线程恰好写了剪贴板」的
        // 竞争里。读取方对自己是 owner 的情况走内存，这里直接拒绝（不在自己的窗口上做 INCR）
        _ if x.is_own_window(ev.requestor) => false,
        Some(o) if owner_time_ok(o.stamp(), ev.time) => {
            if ev.target == x.atoms.MULTIPLE {
                serve_multiple(x, &o, ev.requestor, property)
            } else {
                serve(x, &o, ev.requestor, ev.target, property)
            }
        }
        _ => false,
    };
    let notify = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: ev.time,
        requestor: ev.requestor,
        selection: ev.selection,
        target: ev.target,
        property: if ok { property } else { NONE },
    };
    let _ = x.conn.send_event(false, ev.requestor, EventMask::NO_EVENT, notify);
    let _ = x.conn.flush();
}

fn serve(x: &X11, o: &Owned, requestor: Window, target: Atom, property: Atom) -> bool {
    let a = &x.atoms;
    if target == a.TARGETS {
        let mut list = vec![a.TARGETS, a.MULTIPLE, a.TIMESTAMP];
        list.extend(o.items.iter().map(|i| i.target));
        return x
            .conn
            .change_property32(PropMode::REPLACE, requestor, property, AtomEnum::ATOM, &list)
            .is_ok();
    }
    if target == a.TIMESTAMP {
        // 时间戳还不知道时拒绝这个目标（读取方会退回其他判断方式），绝不报一个错误的时间
        let Some(stamp) = o.stamp() else {
            return false;
        };
        return x
            .conn
            .change_property32(PropMode::REPLACE, requestor, property, AtomEnum::INTEGER, &[stamp])
            .is_ok();
    }
    let Some(item) = o.items.iter().find(|i| i.target == target) else {
        return false;
    };
    if item.data.len() > CHUNK {
        start_incr(x, item, requestor, property)
    } else {
        write_prop(x, requestor, property, item.ty, item.format, &item.data)
    }
}

fn serve_multiple(x: &X11, o: &Owned, requestor: Window, property: Atom) -> bool {
    let Some(reply) = x
        .conn
        .get_property(false, requestor, property, AtomEnum::ANY, 0, 1024)
        .ok()
        .and_then(|c| c.reply().ok())
    else {
        return false;
    };
    let Some(values) = reply.value32() else {
        return false;
    };
    let mut pairs: Vec<u32> = values.collect();
    for pair in pairs.chunks_exact_mut(2) {
        let (t, p) = (pair[0], pair[1]);
        if t == x.atoms.MULTIPLE || !serve(x, o, requestor, t, p) {
            pair[1] = NONE;
        }
    }
    x.conn
        .change_property32(PropMode::REPLACE, requestor, property, x.atoms.ATOM_PAIR, &pairs)
        .is_ok()
}

fn write_prop(x: &X11, win: Window, property: Atom, ty: Atom, format: u8, data: &[u8]) -> bool {
    let unit = usize::from(format / 8).max(1);
    let len = (data.len() / unit) as u32;
    x.conn
        .change_property(PropMode::REPLACE, win, property, ty, format, len, data)
        .is_ok()
}

/// 本连接常驻订阅了属性变化的窗口：不能改它们的事件掩码（会覆盖原有订阅）
fn has_base_subscription(x: &X11, w: Window) -> bool {
    w == x.root || x.screen_state.is_xsettings_window(w)
}

fn start_incr(x: &X11, item: &OwnedItem, requestor: Window, property: Atom) -> bool {
    let mut table = x.owner.incr.lock();
    prune_locked(x, &mut table, Instant::now());
    if !has_base_subscription(x, requestor) && !table.watched.contains(&requestor) {
        // 订阅请求方的属性变化（DELETE = 要下一块）与销毁（请求方退出时立即清理）；只影响本连接在该窗口上的掩码
        if x.conn
            .change_window_attributes(
                requestor,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
            )
            .is_err()
        {
            return false;
        }
        table.watched.push(requestor);
    }
    let ok = x
        .conn
        .change_property32(
            PropMode::REPLACE,
            requestor,
            property,
            x.atoms.INCR,
            &[item.data.len().min(u32::MAX as usize) as u32],
        )
        .is_ok();
    if ok {
        table
            .transfers
            .retain(|t| !(t.requestor == requestor && t.property == property));
        table.transfers.push(IncrOut {
            requestor,
            property,
            ty: item.ty,
            format: item.format,
            data: Arc::clone(&item.data),
            offset: 0,
            last: Instant::now(),
        });
    } else {
        unwatch_if_idle(x, &mut table, requestor, true);
    }
    ok
}

/// 请求方上已经没有进行中的传输时，恢复本模块改过的事件掩码。`alive = false`（窗口已销毁）时只更新记录
fn unwatch_if_idle(x: &X11, table: &mut IncrTable, requestor: Window, alive: bool) {
    if table.transfers.iter().any(|t| t.requestor == requestor) {
        return;
    }
    let Some(i) = table.watched.iter().position(|&w| w == requestor) else {
        return;
    };
    table.watched.swap_remove(i);
    if alive {
        let _ = x.conn.change_window_attributes(
            requestor,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT),
        );
    }
}

/// 丢弃超时的传输
fn prune_locked(x: &X11, table: &mut IncrTable, now: Instant) {
    let mut stale: Vec<Window> = Vec::new();
    table.transfers.retain(|t| {
        let keep = now.duration_since(t.last) <= INCR_STALE;
        if !keep {
            log::debug!("INCR 传输超时，丢弃（请求方 0x{:x}）", t.requestor);
            stale.push(t.requestor);
        }
        keep
    });
    for w in stale {
        unwatch_if_idle(x, table, w, true);
    }
}

/// 顺带清理超时的传输（任何剪贴板活动时调用，没有专门的计时器）
pub(crate) fn prune_stale(x: &X11) {
    let mut table = x.owner.incr.lock();
    if table.transfers.is_empty() {
        return;
    }
    prune_locked(x, &mut table, Instant::now());
    let _ = x.conn.flush();
}

/// 请求方删除了属性：发送下一块（hub 线程）
pub(crate) fn on_property_deleted(x: &X11, window: Window, atom: Atom) {
    let mut table = x.owner.incr.lock();
    if table.transfers.is_empty() {
        return;
    }
    let now = Instant::now();
    let mut finished = None;
    if let Some(i) = table
        .transfers
        .iter()
        .position(|t| t.requestor == window && t.property == atom)
    {
        let t = &mut table.transfers[i];
        let end = (t.offset + CHUNK).min(t.data.len());
        write_prop(x, t.requestor, t.property, t.ty, t.format, &t.data[t.offset..end]);
        t.last = now;
        if end == t.offset {
            // 长度为 0 的块已经发出：传输结束
            table.transfers.swap_remove(i);
            finished = Some(window);
        } else {
            t.offset = end;
        }
    }
    prune_locked(x, &mut table, now);
    if let Some(w) = finished {
        unwatch_if_idle(x, &mut table, w, true);
    }
    let _ = x.conn.flush();
}

/// 窗口销毁（hub 线程）：请求方退出，丢弃它的传输
pub(crate) fn on_destroy(x: &X11, window: Window) {
    let mut table = x.owner.incr.lock();
    if table.transfers.is_empty() && table.watched.is_empty() {
        return;
    }
    let before = table.transfers.len();
    table.transfers.retain(|t| t.requestor != window);
    if table.transfers.len() != before {
        log::debug!("INCR 请求方 0x{window:x} 已销毁，丢弃传输");
    }
    unwatch_if_idle(x, &mut table, window, false);
}

/// 失去 owner（hub 线程）。进行中的 INCR 传输继续完成（ICCCM）
pub(crate) fn on_clear(x: &X11, ev: &SelectionClearEvent) {
    if ev.owner != x.win {
        return;
    }
    if let Some(slot) = x.owner.slot(x, ev.selection) {
        // 只清掉比这次清除更早的内容（避免和刚刚的再次 take 竞争）。时间戳未知时问服务器：仍是 Wisp 就说明这是上一次
        // 占有留下的过时通知
        let stamp = slot.lock().as_ref().map(|o| o.stamp());
        let applies = match stamp {
            None => false,
            Some(Some(t)) => owner_time_ok(Some(t), ev.time),
            Some(None) => !current_owner(x, ev.selection).is_ok_and(|w| w == x.win),
        };
        if applies {
            slot.lock().take();
        }
    }
    prune_stale(x);
}
