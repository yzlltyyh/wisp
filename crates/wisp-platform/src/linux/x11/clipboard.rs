//! X11 剪贴板：变更序号与监听（XFixes）、读取、全类型快照与恢复、写入、抑制 Wisp 自身造成的变更。
//!
//! 变更序号就是 XFixes `SelectionNotify`（CLIPBOARD owner 变化）的计数，由 hub 线程维护；
//! 监听完全事件驱动，空闲零唤醒。PRIMARY 的变化（包括 Wisp 往 xterm 粘贴时临时占有 PRIMARY）不计入序号、
//! 不通知监听者。监听回调投递到回调线程执行（见 `x11/mod.rs`），回调里可以直接读剪贴板。
//!
//! 序号与抑制区间（[`ClipState`]）保存在跨连接的 `Registry` 里：断线重连后序号接着递增（并为重连本身加一，
//! 断开期间剪贴板可能已经变了），保证 `change_id` 单调，上层基于「已见序号」的比较不会失效。
//!
//! 退出时（`Shared` 被丢弃）若 Wisp 仍持有 CLIPBOARD，按 freedesktop 剪贴板管理器规范请求 `CLIPBOARD_MANAGER`
//! 保存（`SAVE_TARGETS`）——模拟复制恢复后 Wisp 持有的是用户原来的剪贴板，不能随 Wisp 退出而丢失。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex as StdMutex, PoisonError};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use wisp_types::{ClipPayload, PhysSize, RawClipboard};
use x11rb::NONE;
use x11rb::protocol::xfixes;
use x11rb::protocol::xproto::{Atom, AtomEnum};

use super::owner::{self, OwnedItem};
use super::transfer::{self, Fetched, LimitMode};
use super::{X11, focus};
use crate::common::secrets_markers::{self, LINUX_PASSWORD_HINT_MIME};
use crate::common::suppress::Suppressor;
use crate::linux::fallback_logic::{self as logic, ItemOutcome, SNAPSHOT_IN_FLIGHT, SnapStep, TargetClass};
use crate::linux::uri;
use crate::types::{ReadLimits, SuppressRange};

/// 剪贴板监听回调
pub(crate) type ClipWatcher = Arc<dyn Fn(u64) + Send + Sync>;

/// 剪贴板的变更序号与抑制区间（hub 线程写，其他线程读）。跨连接保存在 `Registry` 里，监听者也在那里
#[derive(Default)]
pub(crate) struct ClipState {
    change_id: AtomicU64,
    changed: (StdMutex<()>, Condvar),
    suppressor: Mutex<Suppressor>,
}

impl ClipState {
    /// 当前变更序号
    pub(crate) fn change_id(&self) -> u64 {
        self.change_id.load(Ordering::Acquire)
    }

    /// 等待序号超过 `after`，返回新序号；超时返回 None（事件驱动，不轮询）
    pub(crate) fn wait_change(&self, after: u64, deadline: Instant) -> Option<u64> {
        let (lock, cv) = &self.changed;
        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            let id = self.change_id();
            if id > after {
                return Some(id);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            guard = cv
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// 序号加一并唤醒等待者，返回新序号
    fn bump(&self) -> u64 {
        let id = self.change_id.fetch_add(1, Ordering::AcqRel) + 1;
        let _g = self.changed.0.lock().unwrap_or_else(PoisonError::into_inner);
        self.changed.1.notify_all();
        id
    }

    /// 建立了新连接（断线重连）：断开期间剪贴板可能已经变了（例如 XWayland 重启后剪贴板为空），序号加一。
    /// 不通知监听者——没有可以归因的复制
    pub(crate) fn on_reconnect(&self) {
        let id = self.bump();
        log::debug!("X11 重连：剪贴板序号推进到 #{id}");
    }

    /// 连接断开时唤醒所有等待者
    pub(crate) fn wake_all(&self) {
        self.changed.1.notify_all();
    }

    /// 下达抑制指令。区间与其他调用方（例如剪贴板服务为自己的写入）布防的区间并存
    pub(crate) fn suppress(&self, range: SuppressRange) {
        self.suppressor.lock().arm(range, Instant::now());
    }

    /// 撤销自己布防的那一个区间（模拟复制的按键没有发出去，确定不会有变更时），不影响其他调用方的区间
    pub(crate) fn retract(&self, range: SuppressRange) {
        self.suppressor.lock().retract(range);
    }
}

/// XFixes：CLIPBOARD owner 变化（hub 线程）
pub(crate) fn on_owner_change(x: &X11, ev: &xfixes::SelectionNotifyEvent) {
    let id = x.clip.bump();
    // 只有「有新的 owner」才是一次复制；owner 窗口销毁 / 客户端退出只是剪贴板变空
    if ev.subtype != xfixes::SelectionEvent::SET_SELECTION_OWNER || ev.owner == NONE {
        return;
    }
    if ev.owner == x.win {
        // 补上 take 时可能没能取到的时间戳（服务器处理 SetSelectionOwner 时记录的时间）
        owner::learn_stamp(x, x.atoms.CLIPBOARD, ev.selection_timestamp);
    }
    if x.clip.suppressor.lock().should_ignore(id, Instant::now()) {
        log::debug!("剪贴板变更 #{id} 已抑制（Wisp 自身的模拟复制 / 恢复）");
        return;
    }
    if ev.owner == x.win && !x.owner.clipboard().is_some_and(|o| o.report) {
        // 自己的恢复 / 排除历史的写入：不进入历史（忽略 owner 是自己窗口的事件）
        return;
    }
    if ev.owner != x.win {
        // 别人接管了剪贴板：顺带清理本端超时的 INCR 发送（没有专门的计时器）
        owner::prune_stale(x);
    }
    let watchers: Vec<ClipWatcher> = x.reg.clip_watchers.lock().iter().map(|(_, w)| Arc::clone(w)).collect();
    for w in watchers {
        x.reg.post("剪贴板监听", move || w(id));
    }
}

/// 文件列表的读取上限
const MAX_FILE_LIST_BYTES: usize = 1024 * 1024;

/// 一次读取中各个请求对应的内容种类
#[derive(Debug, Clone, Copy)]
enum Slot {
    Text,
    Html,
    Rtf,
    Png,
    Files,
}

/// 读取用的目标原子组
struct Wanted {
    text: Option<Atom>,
    html: Option<Atom>,
    rtf: Option<Atom>,
    png: Option<Atom>,
    uri_list: Option<Atom>,
    gnome_files: Option<Atom>,
}

fn pick(targets: &[Atom], prefs: &[Atom]) -> Option<Atom> {
    prefs.iter().copied().find(|p| targets.contains(p))
}

/// 读取当前剪贴板（工作线程调用；全程有硬超时）
pub(crate) fn read(x: &X11, limits: &ReadLimits) -> Result<RawClipboard, String> {
    let change_id = x.clip.change_id();
    let a = &x.atoms;
    let owner_win = owner::current_owner(x, a.CLIPBOARD)?;
    let mut raw = RawClipboard {
        change_id,
        ..RawClipboard::default()
    };
    if owner_win == NONE {
        return Ok(raw);
    }
    if owner_win == x.win {
        if let Some(o) = x.owner.clipboard() {
            raw.from_wisp = true;
            if o.exclude_from_history {
                raw.concealed = true;
                return Ok(raw);
            }
            fill_from_items(x, &mut raw, &o.items, limits);
        }
        return Ok(raw);
    }
    raw.source = focus::app_for_owner(x, owner_win);
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut session = x.read_session().ok_or("X11 connection lost")?;
    let targets = match session.fetch(
        a.CLIPBOARD,
        a.TARGETS,
        64 * 1024,
        LimitMode::Reject,
        Instant::now() + Duration::from_millis(150),
    ) {
        Fetched::Data { bytes, format, .. } => transfer::parse_atoms(&bytes, format),
        Fetched::Timeout => return Err("clipboard owner did not answer TARGETS".to_owned()),
        // 不支持 TARGETS 的老程序：直接尝试 UTF8_STRING
        _ => vec![a.UTF8_STRING],
    };
    // 1. 隐私标记先于正文
    if targets.contains(&a.KDE_PASSWORD_HINT) {
        let hint = session.fetch(a.CLIPBOARD, a.KDE_PASSWORD_HINT, 256, LimitMode::Reject, deadline);
        let secret = match hint {
            Fetched::Data { bytes, .. } => secrets_markers::linux_is_password_hint(LINUX_PASSWORD_HINT_MIME, &bytes),
            // 有标记但读不到值：按安全优先处理
            _ => true,
        };
        if secret {
            raw.concealed = true;
            return Ok(raw);
        }
    }
    raw.from_wisp = targets.contains(&a.WISP_MARKER);
    let w = Wanted {
        text: pick(
            &targets,
            &[
                a.UTF8_STRING,
                a.TEXT_PLAIN_UTF8,
                u32::from(AtomEnum::STRING),
                a.TEXT_PLAIN,
            ],
        ),
        html: pick(&targets, &[a.TEXT_HTML]),
        rtf: pick(&targets, &[a.TEXT_RTF, a.APPLICATION_RTF, a.TEXT_RICHTEXT]),
        png: pick(&targets, &[a.IMAGE_PNG]),
        uri_list: pick(&targets, &[a.URI_LIST]),
        gnome_files: pick(&targets, &[a.GNOME_COPIED_FILES]),
    };
    let mut req: Vec<(Atom, usize, LimitMode)> = Vec::new();
    let mut slots: Vec<Slot> = Vec::new();
    let mut add = |atom: Option<Atom>, limit: usize, mode: LimitMode, slot: Slot| {
        if let Some(t) = atom {
            req.push((t, limit, mode));
            slots.push(slot);
        }
    };
    add(w.text, limits.max_text_bytes, LimitMode::Truncate, Slot::Text);
    add(w.html, limits.max_rich_bytes, LimitMode::Reject, Slot::Html);
    add(w.rtf, limits.max_rich_bytes, LimitMode::Reject, Slot::Rtf);
    add(w.png, limits.max_image_bytes, LimitMode::Reject, Slot::Png);
    add(
        w.uri_list.or(w.gnome_files),
        MAX_FILE_LIST_BYTES,
        LimitMode::Reject,
        Slot::Files,
    );
    let results = session.fetch_many(a.CLIPBOARD, &req, deadline);
    for ((slot, (target, _, _)), res) in slots.iter().zip(req.iter()).zip(results) {
        let Fetched::Data {
            ty, bytes, truncated, ..
        } = res
        else {
            if let Fetched::TooLarge(n) = res {
                log::debug!("剪贴板的 {slot:?} 超过上限（{n} 字节），已丢弃");
            }
            continue;
        };
        match slot {
            Slot::Text => {
                let text = transfer::decode_text(ty, bytes);
                if !text.is_empty() {
                    raw.text = Some(text);
                    raw.text_truncated = truncated;
                }
            }
            Slot::Html => raw.html = decode_html(bytes),
            Slot::Rtf => raw.rtf = (!bytes.is_empty()).then_some(bytes),
            Slot::Png => {
                if let Some(size) = png_size(&bytes) {
                    raw.image_size = Some(size);
                    raw.image_png = Some(bytes);
                }
            }
            Slot::Files => {
                let text = String::from_utf8_lossy(&bytes);
                let paths = if *target == a.GNOME_COPIED_FILES {
                    uri::paths_from_gnome_copied_files(&text)
                } else {
                    uri::paths_from_uri_list(&text)
                };
                if !paths.is_empty() {
                    raw.files = Some(paths);
                }
            }
        }
    }
    Ok(raw)
}

/// 从 Wisp 自己持有的内容填充（owner 是 Wisp 时不经过 X 服务器往返）
fn fill_from_items(x: &X11, raw: &mut RawClipboard, items: &[OwnedItem], limits: &ReadLimits) {
    let a = &x.atoms;
    let find = |t: Atom| items.iter().find(|i| i.target == t);
    if let Some(i) = find(a.UTF8_STRING).or_else(|| find(u32::from(AtomEnum::STRING))) {
        let n = i.data.len().min(limits.max_text_bytes);
        raw.text = Some(transfer::decode_text(i.ty, i.data[..n].to_vec()));
        raw.text_truncated = i.data.len() > limits.max_text_bytes;
    }
    if let Some(i) = find(a.TEXT_HTML).filter(|i| i.data.len() <= limits.max_rich_bytes) {
        raw.html = decode_html(i.data.as_ref().clone());
    }
    if let Some(i) = find(a.TEXT_RTF).filter(|i| i.data.len() <= limits.max_rich_bytes) {
        raw.rtf = Some(i.data.as_ref().clone());
    }
    if let Some(i) = find(a.IMAGE_PNG).filter(|i| i.data.len() <= limits.max_image_bytes) {
        raw.image_size = png_size(&i.data);
        raw.image_png = raw.image_size.map(|_| i.data.as_ref().clone());
    }
    if let Some(i) = find(a.URI_LIST) {
        let paths = uri::paths_from_uri_list(&String::from_utf8_lossy(&i.data));
        raw.files = (!paths.is_empty()).then_some(paths);
    }
}

/// text/html：Firefox 等程序可能给出带 BOM 的 UTF-16
fn decode_html(bytes: Vec<u8>) -> Option<String> {
    let text = match bytes.as_slice() {
        [0xFF, 0xFE, rest @ ..] => utf16(rest, false),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, true),
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        _ => String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()),
    };
    let text = text.trim_end_matches('\0').to_owned();
    (!text.trim().is_empty()).then_some(text)
}

fn utf16(bytes: &[u8], big_endian: bool) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| {
            if big_endian {
                u16::from_be_bytes([c[0], c[1]])
            } else {
                u16::from_le_bytes([c[0], c[1]])
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

/// PNG 的宽高（只读 IHDR，不解码）
pub(crate) fn png_size(bytes: &[u8]) -> Option<PhysSize> {
    const SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if bytes.len() < 24 || bytes[..8] != SIG || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    (w > 0 && h > 0).then_some(PhysSize { width: w, height: h })
}

/// 快照的平台私有结构
pub(crate) struct LinuxSnapshot {
    pub items: Vec<OwnedItem>,
    /// 快照时 owner 是 Wisp 自己、且那次写入带了「排除历史」：恢复后 `read()` 仍按 concealed 处理
    pub exclude_from_history: bool,
}

impl LinuxSnapshot {
    fn foreign(items: Vec<OwnedItem>) -> Self {
        Self {
            items,
            exclude_from_history: false,
        }
    }
}

/// 快照结果
pub(crate) enum Snap {
    Taken(LinuxSnapshot, usize),
    /// 合计或某个类型超过上限（「剪贴板内容过大」）
    TooLarge,
    /// owner 没有在预算内应答（不是「过大」）
    Timeout,
}

/// 全类型快照：读 TARGETS 后按优先级分批读取（`fallback_logic::plan_snapshot`：标记 → 文本 → 富文本 →
/// 文件 → 一种图片 → 私有格式），每批最多 [`SNAPSHOT_IN_FLIGHT`] 个请求在途，预算用完就不再发出新请求。必须的
/// 类别过大 / 超时则放弃整个快照；私有格式读不到只是少恢复一项（保留已读到的）
pub(crate) fn snapshot(x: &X11, max_bytes: usize, budget: Duration) -> Result<Snap, String> {
    let deadline = Instant::now() + budget;
    let a = &x.atoms;
    let owner_win = owner::current_owner(x, a.CLIPBOARD)?;
    if owner_win == NONE {
        return Ok(Snap::Taken(LinuxSnapshot::foreign(Vec::new()), 0));
    }
    if owner_win == x.win
        && let Some(o) = x.owner.clipboard()
    {
        let bytes = o.items.iter().map(|i| i.data.len()).sum();
        if bytes > max_bytes {
            return Ok(Snap::TooLarge);
        }
        let snap = LinuxSnapshot {
            items: o.items.clone(),
            exclude_from_history: o.exclude_from_history,
        };
        return Ok(Snap::Taken(snap, bytes));
    }
    let mut session = x.read_session().ok_or("X11 connection lost")?;
    let targets = match session.fetch(a.CLIPBOARD, a.TARGETS, 64 * 1024, LimitMode::Reject, deadline) {
        Fetched::Data { bytes, format, .. } => transfer::parse_atoms(&bytes, format),
        Fetched::TooLarge(_) => return Ok(Snap::TooLarge),
        Fetched::Timeout => return Ok(Snap::Timeout),
        // 不支持 TARGETS 的老程序：无法知道有哪些类型，按超时一类处理（不是「过大」）
        Fetched::Refused => {
            log::info!("剪贴板 owner 不支持 TARGETS，放弃备份");
            return Ok(Snap::Timeout);
        }
    };
    let names = x.atom_names(&targets);
    let named: Vec<(Atom, &str)> = targets
        .iter()
        .zip(&names)
        .filter_map(|(&t, n)| Some((t, n.as_deref()?)))
        .collect();
    let plan = logic::plan_snapshot(&named);
    let mut items = Vec::with_capacity(plan.len());
    let mut total = 0usize;
    let mut skipped = 0usize;
    for (i, batch) in plan.chunks(SNAPSHOT_IN_FLIGHT).enumerate() {
        if Instant::now() >= deadline {
            let rest = &plan[i * SNAPSHOT_IN_FLIGHT..];
            if logic::budget_exhausted(rest) == SnapStep::AbortTimeout {
                log::info!("剪贴板快照超时（{} ms），放弃备份", budget.as_millis());
                return Ok(Snap::Timeout);
            }
            skipped += rest.len();
            break;
        }
        // 每一项的上限是剩余额度：超出的不必传完
        let room = max_bytes.saturating_sub(total);
        let req: Vec<(Atom, usize, LimitMode)> = batch.iter().map(|&(t, _)| (t, room, LimitMode::Reject)).collect();
        let results = session.fetch_many(a.CLIPBOARD, &req, deadline);
        let mut stop = false;
        for (&(target, class), res) in batch.iter().zip(results) {
            let outcome = match &res {
                Fetched::Data { bytes, .. } if total + bytes.len() <= max_bytes => ItemOutcome::Data,
                Fetched::Data { .. } | Fetched::TooLarge(_) => ItemOutcome::TooLarge,
                Fetched::Timeout => ItemOutcome::Timeout,
                Fetched::Refused => ItemOutcome::Refused,
            };
            match logic::snapshot_step(class, outcome) {
                SnapStep::Keep => {
                    if let Fetched::Data { ty, format, bytes, .. } = res {
                        total += bytes.len();
                        items.push(OwnedItem {
                            target,
                            ty,
                            format,
                            data: Arc::new(bytes),
                        });
                    }
                }
                // owner 声明了却拒绝转换的目标、过大的私有格式：恢复时不提供
                SnapStep::Skip => skipped += usize::from(class == TargetClass::Extra),
                SnapStep::Stop => {
                    skipped += 1;
                    stop = true;
                }
                SnapStep::AbortTooLarge => {
                    log::info!("剪贴板快照超过 {max_bytes} 字节，放弃备份");
                    return Ok(Snap::TooLarge);
                }
                SnapStep::AbortTimeout => {
                    log::info!("剪贴板快照超时（{} ms），放弃备份", budget.as_millis());
                    return Ok(Snap::Timeout);
                }
            }
        }
        if stop {
            skipped += plan.len().saturating_sub((i + 1) * SNAPSHOT_IN_FLIGHT);
            break;
        }
    }
    if skipped > 0 {
        log::info!(
            "剪贴板快照：{} 项已备份，{skipped} 个应用私有格式未备份（超出预算 / 过大 / 被拒绝，恢复时不提供）",
            items.len()
        );
    }
    Ok(Snap::Taken(LinuxSnapshot::foreign(items), total))
}

/// 恢复快照：成为 owner 并提供原来的全部类型；原剪贴板为空时清空。返回恢复后的变更序号
pub(crate) fn restore(x: &X11, snap: LinuxSnapshot) -> Result<u64, String> {
    let before = x.clip.change_id();
    if snap.items.is_empty() {
        // 原本就没有 owner：如果现在的 owner 是模拟复制产生的，清空它
        owner::clear(x)?;
    } else {
        // 恢复一律不通知历史监听（owner 是 Wisp 的恢复不回调）；「排除历史」标记原样保留，
        // 恢复后读取仍是 concealed
        owner::take(x, x.atoms.CLIPBOARD, snap.items, false, snap.exclude_from_history)?;
    }
    Ok(x.clip
        .wait_change(before, Instant::now() + Duration::from_millis(100))
        .unwrap_or_else(|| x.clip.change_id()))
}

/// 写入剪贴板，返回写入后的变更序号
pub(crate) fn write(x: &X11, payload: &ClipPayload, exclude_from_history: bool) -> Result<u64, String> {
    let items = payload_items(x, payload, exclude_from_history);
    if items.is_empty() {
        return Err("empty payload".to_owned());
    }
    let before = x.clip.change_id();
    owner::take(x, x.atoms.CLIPBOARD, items, !exclude_from_history, exclude_from_history)?;
    Ok(x.clip
        .wait_change(before, Instant::now() + Duration::from_millis(100))
        .unwrap_or_else(|| x.clip.change_id()))
}

/// 把 Wisp 当前持有的 CLIPBOARD 内容同时作为 PRIMARY 提供：xterm / urxvt 的 Shift+Insert 粘贴的是
/// PRIMARY。PRIMARY 的变化不计入剪贴板序号、不通知历史监听。Wisp 不是 CLIPBOARD 的 owner（以服务器为准，本地
/// 记录可能还没被 SelectionClear 清掉）时返回错误，绝不拿过时的内容去占有 PRIMARY
pub(crate) fn mirror_to_primary(x: &X11) -> Result<(), String> {
    if owner::current_owner(x, x.atoms.CLIPBOARD)? != x.win {
        return Err("Wisp does not own the clipboard".to_owned());
    }
    let o = x.owner.clipboard().ok_or("Wisp does not own the clipboard")?;
    owner::take(
        x,
        u32::from(AtomEnum::PRIMARY),
        o.items.clone(),
        false,
        o.exclude_from_history,
    )
}

/// 退出前把 Wisp 持有的 CLIPBOARD 交给剪贴板管理器保存（freedesktop ClipboardManager 规范）：向
/// `CLIPBOARD_MANAGER` 请求转换 `SAVE_TARGETS`，管理器随即向 Wisp 请求全部目标（hub 线程照常应答，含 INCR），保存完
/// 成后回 SelectionNotify；Wisp 退出后由管理器接管剪贴板。没有管理器、或 Wisp 不是 owner 时什么也不做。
/// 最多等待 `timeout`
pub(crate) fn hand_off_to_manager(x: &X11, timeout: Duration) {
    let a = &x.atoms;
    if x.owner.clipboard().is_none() || !owner::current_owner(x, a.CLIPBOARD).is_ok_and(|w| w == x.win) {
        return;
    }
    if !owner::current_owner(x, a.CLIPBOARD_MANAGER).is_ok_and(|w| w != NONE) {
        log::info!("退出：没有剪贴板管理器，Wisp 持有的剪贴板内容将随进程退出而清空");
        return;
    }
    let Some(mut session) = x.read_session() else {
        return;
    };
    let started = Instant::now();
    // 不在属性里列出目标 = 请管理器保存全部目标
    let res = session.fetch(
        a.CLIPBOARD_MANAGER,
        a.SAVE_TARGETS,
        64,
        LimitMode::Truncate,
        started + timeout,
    );
    // 管理器的应答只表示「处理完了」（多数实现不写属性），不区分成败
    if res == Fetched::Timeout {
        log::warn!("退出：剪贴板管理器没有在 {} ms 内完成保存", timeout.as_millis());
    } else {
        log::info!("退出：剪贴板已交给剪贴板管理器（{} ms）", started.elapsed().as_millis());
    }
}

fn payload_items(x: &X11, p: &ClipPayload, exclude_from_history: bool) -> Vec<OwnedItem> {
    let a = &x.atoms;
    let mut items: Vec<OwnedItem> = Vec::new();
    let push = |items: &mut Vec<OwnedItem>, target: Atom, ty: Atom, data: Vec<u8>| {
        items.push(OwnedItem {
            target,
            ty,
            format: 8,
            data: Arc::new(data),
        });
    };
    // 文件条目同时给出纯文本路径（粘贴到终端 / 编辑器时得到路径）
    let text = p
        .text
        .clone()
        .or_else(|| p.files.as_ref().filter(|f| !f.is_empty()).map(|f| f.join("\n")));
    if let Some(text) = text {
        let utf8 = text.into_bytes();
        push(&mut items, a.UTF8_STRING, a.UTF8_STRING, utf8.clone());
        push(&mut items, a.TEXT_PLAIN_UTF8, a.TEXT_PLAIN_UTF8, utf8.clone());
        push(&mut items, a.TEXT, a.UTF8_STRING, utf8.clone());
        // STRING 只能表示 Latin-1：能完整表示时才提供
        if let Ok(s) = std::str::from_utf8(&utf8)
            && s.chars().all(|c| u32::from(c) <= 0xFF)
        {
            push(
                &mut items,
                u32::from(AtomEnum::STRING),
                u32::from(AtomEnum::STRING),
                s.chars().map(|c| c as u8).collect(),
            );
        }
        push(&mut items, a.TEXT_PLAIN, a.TEXT_PLAIN, utf8);
    }
    if let Some(html) = &p.html {
        push(&mut items, a.TEXT_HTML, a.TEXT_HTML, html.clone().into_bytes());
    }
    if let Some(rtf) = &p.rtf {
        push(&mut items, a.TEXT_RTF, a.TEXT_RTF, rtf.clone());
        push(&mut items, a.APPLICATION_RTF, a.APPLICATION_RTF, rtf.clone());
    }
    if let Some(png) = &p.image_png {
        push(&mut items, a.IMAGE_PNG, a.IMAGE_PNG, png.clone());
    }
    if let Some(files) = p.files.as_ref().filter(|f| !f.is_empty()) {
        push(
            &mut items,
            a.URI_LIST,
            a.URI_LIST,
            uri::build_uri_list(files).into_bytes(),
        );
        push(
            &mut items,
            a.GNOME_COPIED_FILES,
            a.GNOME_COPIED_FILES,
            uri::build_gnome_copied_files(files).into_bytes(),
        );
    }
    if items.is_empty() {
        return items;
    }
    // Wisp 私有标记；排除历史时附加 KDE 的密码提示（Klipper、CopyQ、GPaste 等剪贴板管理器都会跳过）
    push(&mut items, a.WISP_MARKER, a.WISP_MARKER, b"1".to_vec());
    if exclude_from_history {
        push(
            &mut items,
            a.KDE_PASSWORD_HINT,
            a.KDE_PASSWORD_HINT,
            secrets_markers::LINUX_PASSWORD_HINT_SECRET.as_bytes().to_vec(),
        );
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_header_parsing() {
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
        png.extend(b"IHDR");
        png.extend(640u32.to_be_bytes());
        png.extend(480u32.to_be_bytes());
        assert_eq!(
            png_size(&png),
            Some(PhysSize {
                width: 640,
                height: 480
            })
        );
        assert_eq!(png_size(&png[..20]), None);
        assert_eq!(png_size(b"GIF89a.................."), None);
    }

    #[test]
    fn html_decoding() {
        assert_eq!(decode_html(b"<b>x</b>".to_vec()).as_deref(), Some("<b>x</b>"));
        let utf16: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("<i>中</i>".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert_eq!(decode_html(utf16).as_deref(), Some("<i>中</i>"));
        assert_eq!(decode_html(b"  \0".to_vec()), None);
    }
}
