//! X11 取词与模拟粘贴。
//!
//! 取词：PRIMARY（选中即写入，零副作用）→ 为空时按映射表模拟复制再读 CLIPBOARD（7 步）。
//! 终端绝不发送 Ctrl+C；XFixes / XTest 不可用时不走回退。锚点是 XQueryPointer 的位置。
//! 每次读取都只有一个绝对截止时间：等待 owner、UTF8_STRING 被拒后改试其他文本类型，全部共用它。
//!
//! 浮标判定（`has_selection`）额外要求 PRIMARY 在本次手势开始之后被重新占有过（`primary_gate`），否则旧选区
//! 会让每一次拖动窗口 / 滚动条都弹出浮标。
//!
//! 粘贴：xterm / urxvt 一类终端的 Shift+Insert 粘贴的是 PRIMARY，发键前让 Wisp 同时占有 PRIMARY（内容与剪贴板
//! 相同），否则粘贴进去的会是刚才选中的原文。

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use wisp_types::{Anchor, AppKeymap, AppRef, OsKind, Rect};
use x11rb::NONE;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};

use super::fallback_logic::{self as logic, Backup, CopyOutcome, Proceed};
use super::primary_gate::Verdict;
use super::session::reason;
use super::terminal;
use super::x11::clipboard::Snap;
use super::x11::transfer::{Fetched, LimitMode, decode_text};
use super::x11::{X11, clipboard, focus, keyboard, screen};
use crate::common::combo::{Combo, Modifiers};
use crate::common::keymap::{self, KeyChoice};
use crate::common::text::{self, MAX_SELECTION_BYTES};
use crate::traits::{PasteError, SelectionOpts};
use crate::types::{CaptureMethod, Selection, SelectionError, SuppressRange};

/// PRIMARY 的 owner 首次应答上限
const PRIMARY_WAIT: Duration = Duration::from_millis(50);
/// 大选区走 INCR 时，整体读取的上限
const PRIMARY_TOTAL: Duration = Duration::from_millis(400);
/// 快照的预算
const SNAPSHOT_MAX_BYTES: usize = 10 * 1024 * 1024;
const SNAPSHOT_BUDGET: Duration = Duration::from_millis(50);
/// 模拟复制后读取 CLIPBOARD：owner 首次应答的上限与整体上限
const CLIPBOARD_READ: Duration = Duration::from_millis(100);
const CLIPBOARD_TOTAL: Duration = Duration::from_millis(400);
/// 粘贴键读 PRIMARY 的终端（xterm 与 rxvt 一族：Shift+Insert = insert-selection(PRIMARY)），按 WM_CLASS 匹配
const PRIMARY_PASTE_TERMINALS: &[&str] = &[
    "xterm",
    "uxterm",
    "urxvt",
    "rxvt",
    "urxvt-unicode",
    "rxvt-unicode",
    "eterm",
    "aterm",
];
/// 激活目标窗口后等待它到前台的上限
const ACTIVATE_WAIT: Duration = Duration::from_millis(100);

#[cfg(test)]
fn ctrl(code: &str) -> Combo {
    Combo {
        modifiers: Modifiers {
            control: true,
            ..Modifiers::default()
        },
        code: code.to_owned(),
    }
}

fn ctrl_shift(code: &str) -> Combo {
    Combo {
        modifiers: Modifiers {
            control: true,
            shift: true,
            ..Modifiers::default()
        },
        code: code.to_owned(),
    }
}

/// 映射表里是否有该应用、且给出了该字段的条目（按 WM_CLASS 的 class 或 instance 匹配）
fn has_entry(table: &[AppKeymap], app: &AppRef, field: impl Fn(&AppKeymap) -> Option<&str>) -> bool {
    table
        .iter()
        .any(|k| k.os == OsKind::Linux && keymap::app_matches(OsKind::Linux, &k.app_id, app) && field(k).is_some())
}

fn is_terminal(app: &AppRef) -> bool {
    terminal::is_terminal(Some(&app.id), app.alt_id.as_deref())
}

/// xterm / rxvt 一族（粘贴键 Shift+Insert，读 PRIMARY；Ctrl+Shift+V 在 urxvt 里是 ISO 14755 输入模式）
fn is_primary_paste_terminal(app: &AppRef) -> bool {
    [Some(app.id.as_str()), app.alt_id.as_deref()]
        .into_iter()
        .flatten()
        .any(|n| PRIMARY_PASTE_TERMINALS.iter().any(|t| t.eq_ignore_ascii_case(n.trim())))
}

fn shift_insert() -> Combo {
    Combo {
        modifiers: Modifiers {
            shift: true,
            ..Modifiers::default()
        },
        code: "Insert".to_owned(),
    }
}

/// 这个粘贴键在 X11 终端里读的是 PRIMARY 而不是 CLIPBOARD（xterm / urxvt 的 Shift+Insert）。GTK / Qt 应用的
/// Shift+Insert 读 CLIPBOARD，此时同时占有 PRIMARY 也没有副作用，所以只看按键本身
pub(crate) fn reads_primary(combo: &Combo) -> bool {
    *combo == shift_insert()
}

/// 模拟复制用的按键；None 表示这个应用不发复制键。规则由 `common::keymap::copy_key` 决定（用户条目 → 内置表 →
/// 平台默认，逐字段合并）；另加一层安全网：映射表里没有条目的终端**绝不发 Ctrl+C**（会中断前台进程）
pub(crate) fn copy_combo(keymaps: &RwLock<Vec<AppKeymap>>, app: Option<&AppRef>) -> Option<Combo> {
    let table = keymaps.read();
    if let Some(app) = app
        && is_terminal(app)
        && !has_entry(&table, app, |k| k.copy.as_deref())
    {
        return None;
    }
    match keymap::copy_key(&table, OsKind::Linux, app) {
        KeyChoice::Send(c) => Some(c),
        KeyChoice::Skip => None,
    }
}

/// 粘贴键：`common::keymap::paste_key`；映射表外的终端用 Ctrl+Shift+V（Ctrl+V 在终端里是「逐字输入」），
/// 其中 rxvt 一族用 Shift+Insert。None 表示该应用配置为不发粘贴键（只激活目标，内容留在剪贴板）
pub(crate) fn paste_combo(keymaps: &RwLock<Vec<AppKeymap>>, app: &AppRef) -> Option<Combo> {
    let table = keymaps.read();
    if is_terminal(app) && !has_entry(&table, app, |k| k.paste.as_deref()) {
        return Some(if is_primary_paste_terminal(app) {
            shift_insert()
        } else {
            ctrl_shift("KeyV")
        });
    }
    match keymap::paste_key(&table, OsKind::Linux, Some(app)) {
        KeyChoice::Send(c) => Some(c),
        KeyChoice::Skip => None,
    }
}

fn pointer_anchor(x: &X11) -> Option<Anchor> {
    let (px, py, _) = screen::pointer(x)?;
    let monitors = screen::monitors(x);
    let m = super::geometry::monitor_at(&monitors, px, py)?;
    let p = m.to_local(px, py);
    Some(Anchor {
        monitor_id: m.id.clone(),
        rect: Rect::new(p.x, p.y, 0.0, 0.0),
    })
}

enum TextRead {
    Text(String, bool),
    Empty,
    Timeout,
}

/// 读取选区的文本：UTF8_STRING，被拒绝时依次再试 STRING、text/plain;charset=utf-8、text/plain（只提供老式类型的
/// Java / Motif / Wine 程序）。`notify_deadline` 是 owner 首次应答的绝对截止时间，`deadline` 是整体（含 INCR）
/// 的绝对截止时间；几次尝试共用它们，总耗时不会超过 `deadline`
fn read_text(x: &X11, selection: u32, limit: usize, notify_deadline: Instant, deadline: Instant) -> TextRead {
    let Some(mut session) = x.read_session() else {
        return TextRead::Timeout;
    };
    let a = &x.atoms;
    for target in [
        a.UTF8_STRING,
        u32::from(AtomEnum::STRING),
        a.TEXT_PLAIN_UTF8,
        a.TEXT_PLAIN,
    ] {
        let res = session
            .fetch_many_with(
                selection,
                &[(target, limit, LimitMode::Truncate)],
                notify_deadline,
                deadline,
            )
            .pop()
            .unwrap_or(Fetched::Timeout);
        match res {
            Fetched::Data {
                ty, bytes, truncated, ..
            } => {
                let text = decode_text(ty, bytes);
                return if text::is_blank(&text) {
                    TextRead::Empty
                } else {
                    TextRead::Text(text, truncated)
                };
            }
            Fetched::Refused => continue,
            Fetched::Timeout => return TextRead::Timeout,
            Fetched::TooLarge(_) => return TextRead::Empty,
        }
    }
    TextRead::Empty
}

/// 读取 PRIMARY。没有 owner、或 owner 是 Wisp 自己（往 xterm 粘贴时占有的）都视为没有选区
fn read_primary(x: &X11, limit: usize, notify_deadline: Instant, deadline: Instant) -> TextRead {
    let primary = u32::from(AtomEnum::PRIMARY);
    let owner = x
        .conn
        .get_selection_owner(primary)
        .ok()
        .and_then(|c| c.reply().ok())
        .map_or(NONE, |r| r.owner);
    if owner == NONE || owner == x.win {
        return TextRead::Empty;
    }
    read_text(x, primary, limit, notify_deadline, deadline)
}

fn finish(
    text: String,
    truncated: bool,
    method: CaptureMethod,
    source: Option<AppRef>,
    anchor: Option<Anchor>,
    started: Instant,
) -> Result<Selection, SelectionError> {
    let (text, cut) = text::normalize_selection(&text);
    if text::is_blank(&text) {
        return Err(SelectionError::Empty);
    }
    Ok(Selection {
        text,
        truncated: truncated || cut,
        anchor,
        source,
        method,
        elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
    })
}

/// 取词（工作线程调用）
pub(crate) fn get(
    x: &Arc<X11>,
    opts: &SelectionOpts,
    keymaps: &RwLock<Vec<AppKeymap>>,
) -> Result<Selection, SelectionError> {
    let started = Instant::now();
    let source = opts.target.clone().or_else(|| focus::frontmost(x));
    let app_id = source.as_ref().map_or("-", |s| s.id.as_str()).to_owned();
    let anchor = pointer_anchor(x);
    let t_primary = Instant::now();
    let primary = read_primary(
        x,
        MAX_SELECTION_BYTES + 4,
        t_primary + PRIMARY_WAIT,
        t_primary + PRIMARY_TOTAL,
    );
    let primary_ms = t_primary.elapsed().as_millis();
    match primary {
        TextRead::Text(t, truncated) => {
            let r = finish(t, truncated, CaptureMethod::Primary, source, anchor, started);
            log::info!(
                "取词 app={app_id} method=primary primary_ms={primary_ms} result={}",
                if r.is_ok() { "ok" } else { "empty" }
            );
            return r;
        }
        TextRead::Timeout => log::debug!("PRIMARY 的 owner 在 {} ms 内没有应答", PRIMARY_WAIT.as_millis()),
        TextRead::Empty => {}
    }
    if !opts.allow_copy_fallback {
        log::info!("取词 app={app_id} method=none primary_ms={primary_ms} result=empty");
        return Err(SelectionError::Empty);
    }
    copy_fallback(x, opts, keymaps, source, anchor, started, &app_id)
}

/// 模拟复制回退（记录序号 → 抑制 → 备份 → 发复制键 → 等待 → 读取 → 恢复）。分支的决定都在 `fallback_logic` 里（可在任何平台单测）
fn copy_fallback(
    x: &Arc<X11>,
    opts: &SelectionOpts,
    keymaps: &RwLock<Vec<AppKeymap>>,
    source: Option<AppRef>,
    anchor: Option<Anchor>,
    started: Instant,
    app_id: &str,
) -> Result<Selection, SelectionError> {
    if !x.ext.xtest || !x.ext.xfixes {
        // 扩展不可用时隐藏回退路径
        log::info!("取词 app={app_id} method=none result=unsupported（缺少 XTEST / XFIXES）");
        return Err(SelectionError::Empty);
    }
    let Some(combo) = copy_combo(keymaps, source.as_ref()) else {
        log::info!("取词 app={app_id} method=none result=empty（该应用不发送复制键）");
        return Err(SelectionError::Empty);
    };
    let copy_wait = logic::clamp_copy_wait(opts.copy_wait);
    // ① 记录序号
    let s0 = x.clip.change_id();
    // ③ 备份全部类型（必须的类型超过 10 MB 或 50 ms 放弃）。快照本身不改变剪贴板，所以抑制在它之后、发键之前才布防
    let t_snap = Instant::now();
    let (backup, snap) = match clipboard::snapshot(x, SNAPSHOT_MAX_BYTES, SNAPSHOT_BUDGET) {
        Ok(Snap::Taken(snap, _)) => (Backup::Taken, Some(snap)),
        Ok(Snap::TooLarge) => (Backup::TooLarge, None),
        Ok(Snap::Timeout) => (Backup::Timeout, None),
        Err(e) => {
            log::debug!("剪贴板快照失败：{e}");
            (Backup::Timeout, None)
        }
    };
    let snap_ms = t_snap.elapsed().as_millis();
    let backup_skipped = snap.is_none();
    match logic::proceed(backup, opts.history_enabled) {
        Proceed::Go => {}
        // 历史关闭时，无法备份就不能覆盖用户的剪贴板。只有确实过大才报「剪贴板内容过大」，
        // owner 没有及时应答则按超时处理（重试多半就能成功）
        abort => {
            let too_large = abort == Proceed::AbortTooLarge;
            log::info!(
                "取词 app={app_id} method=copy result=skipped backup_skipped=true backup_too_large={too_large} snapshot_ms={snap_ms}"
            );
            return Err(if too_large {
                SelectionError::Unsupported(reason::CLIPBOARD_TOO_LARGE.to_owned())
            } else {
                SelectionError::Timeout
            });
        }
    }
    // ② 抑制：目标应用复制一次（区间）+ Wisp 恢复一次（owner 是 Wisp 且不回调）。保存区间本身，
    // 只撤销自己布防的这一个，不影响其他调用方（剪贴板服务为自己的写入）布防的区间
    let range = SuppressRange {
        from_change_id: s0 + 1,
        ttl: logic::suppress_ttl(copy_wait),
    };
    x.clip.suppress(range);
    // ④ 处理残留修饰键后发复制键
    let sent = keyboard::release_held_modifiers(x).and_then(|()| keyboard::send_combo(x, &combo));
    if let Err(e) = sent {
        log::warn!("模拟复制按键失败：{e}");
        if logic::retract_suppression(CopyOutcome::SendFailed) {
            x.clip.retract(range);
        }
        return Err(SelectionError::Other(e));
    }
    // ⑤ 等待序号变化（XFixes 事件驱动，不轮询）
    let t_wait = Instant::now();
    let changed = x.clip.wait_change(s0, t_wait + copy_wait);
    let wait_ms = t_wait.elapsed().as_millis();
    if changed.is_none() {
        // 序号没变：剪贴板没有被动过，不需要恢复。抑制区间不撤销、按 ttl 自然过期——慢应用在等待结束之后才完成的
        // 复制仍是 Wisp 造成的，不能进入历史（`fallback_logic::retract_suppression`）
        if logic::retract_suppression(CopyOutcome::NoChange) {
            x.clip.retract(range);
        }
        log::info!(
            "取词 app={app_id} method=copy result=empty snapshot_ms={snap_ms} wait_ms={wait_ms} backup_skipped={backup_skipped}"
        );
        return Err(SelectionError::Empty);
    }
    // 变更已经发生：区间留到 ttl 结束（恢复那一次由「owner 是 Wisp」识别，不回调）
    if logic::retract_suppression(CopyOutcome::Changed) {
        x.clip.retract(range);
    }
    // ⑥ 读取文本（UTF8_STRING 被拒时改试老式文本类型）
    let t_read = Instant::now();
    let text = read_text(
        x,
        x.atoms.CLIPBOARD,
        MAX_SELECTION_BYTES + 4,
        t_read + CLIPBOARD_READ,
        t_read + CLIPBOARD_TOTAL,
    );
    let read_ms = t_read.elapsed().as_millis();
    // ⑦ 立即恢复
    let t_restore = Instant::now();
    if let Some(snap) = snap
        && let Err(e) = clipboard::restore(x, snap)
    {
        log::warn!("恢复剪贴板失败：{e}");
    }
    let restore_ms = t_restore.elapsed().as_millis();
    let result = match text {
        TextRead::Text(t, truncated) => finish(t, truncated, CaptureMethod::Copy, source, anchor, started),
        TextRead::Timeout => Err(SelectionError::Timeout),
        TextRead::Empty => Err(SelectionError::Empty),
    };
    log::info!(
        "取词 app={app_id} method=copy result={} snapshot_ms={snap_ms} wait_ms={wait_ms} read_ms={read_ms} restore_ms={restore_ms} total_ms={} backup_skipped={backup_skipped}",
        match &result {
            Ok(_) => "ok",
            Err(SelectionError::Timeout) => "timeout",
            Err(_) => "empty",
        },
        started.elapsed().as_millis()
    );
    result
}

/// 浮标判定用：只读 PRIMARY，绝不模拟复制。安装了浮标钩子时（XFixes 跟踪 PRIMARY 的占有），PRIMARY 必须在本次
/// 手势第一次左键按下之后被重新占有过才算有选区（应用可能在 mouse-up 之后才占有，最多等到截止时间）；之后再确认
/// 内容非空。全部步骤共用一个绝对截止时间，拿不到结果返回 None
pub(crate) fn has_selection(x: &X11, deadline: Duration) -> Option<bool> {
    let end = Instant::now() + deadline;
    match x.primary.wait_verdict(end) {
        Verdict::Stale => return Some(false),
        Verdict::Fresh | Verdict::Unknown => {}
    }
    match read_primary(x, 64, end, end) {
        TextRead::Text(..) => Some(true),
        TextRead::Empty => Some(false),
        TextRead::Timeout => None,
    }
}

/// 模拟粘贴：调用方已经写好剪贴板并隐藏了面板
pub(crate) fn paste_into(x: &Arc<X11>, target: &AppRef, keymaps: &RwLock<Vec<AppKeymap>>) -> Result<(), PasteError> {
    if !x.ext.xtest {
        return Err(PasteError::Unsupported(reason::X11_MISSING_XTEST.to_owned()));
    }
    let Some(window) = focus::resolve_window(x, target) else {
        return Err(PasteError::TargetGone);
    };
    if focus::active_toplevel(x) != Some(window) {
        focus::request_activate(x, window).map_err(PasteError::Other)?;
        if !focus::wait_active(x, window, ACTIVATE_WAIT) {
            log::info!("粘贴：{} 没有在 {} ms 内回到前台", target.id, ACTIVATE_WAIT.as_millis());
            return Err(PasteError::FocusTimeout);
        }
    }
    let Some(combo) = paste_combo(keymaps, target) else {
        log::info!("粘贴：{} 配置为不发送粘贴键，只激活目标", target.id);
        return Ok(());
    };
    if reads_primary(&combo)
        && let Err(e) = clipboard::mirror_to_primary(x)
    {
        // 调用方没有经 Wisp 写剪贴板（或已被别的应用接管）：终端会粘贴它自己的 PRIMARY，只记日志
        log::info!("粘贴：无法同时占有 PRIMARY（{e}），{} 可能粘贴旧选区", target.id);
    }
    // 连续粘贴进行中时粘贴键被抓取着：Wisp 自己发的这一下要绕开抓取，否则会被当成用户按的、变成一次队列前进
    super::x11::hotkeys::send_outside_paste_grab(x, &combo, || {
        keyboard::release_held_modifiers(x).and_then(|()| keyboard::send_combo(x, &combo))
    })
    .map_err(PasteError::Other)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str, alt: Option<&str>) -> AppRef {
        AppRef {
            id: id.into(),
            alt_id: alt.map(Into::into),
            name: id.into(),
            pid: None,
            path: None,
            window: None,
        }
    }

    fn entry(app_id: &str, copy: Option<&str>, paste: Option<&str>) -> AppKeymap {
        AppKeymap {
            os: OsKind::Linux,
            app_id: app_id.into(),
            copy: copy.map(Into::into),
            paste: paste.map(Into::into),
        }
    }

    #[test]
    fn terminals_never_get_ctrl_c() {
        let table = RwLock::new(Vec::new());
        assert_eq!(copy_combo(&table, Some(&app("kitty", Some("kitty")))), None);
        assert_eq!(copy_combo(&table, Some(&app("xterm", Some("xterm")))), None);
        assert_eq!(
            copy_combo(&table, Some(&app("firefox", Some("Navigator")))),
            Some(ctrl("KeyC"))
        );
        assert_eq!(copy_combo(&table, None), Some(ctrl("KeyC")));
    }

    #[test]
    fn keymap_entries_win_and_none_disables() {
        let table = RwLock::new(vec![
            entry("xterm", Some("none"), Some("Shift+Insert")),
            entry("Gnome-terminal-server", None, None),
        ]);
        assert_eq!(copy_combo(&table, Some(&app("xterm", Some("xterm")))), None);
        // 只有 class 命中不了时按 instance 匹配；条目没写复制键 → 平台默认（终端不发）
        assert_eq!(
            copy_combo(&table, Some(&app("gnome-terminal", Some("gnome-terminal-server")))),
            None
        );
        // 表外终端粘贴用 Ctrl+Shift+V
        assert_eq!(paste_combo(&table, &app("alacritty", None)), Some(ctrl_shift("KeyV")));
        assert_eq!(paste_combo(&table, &app("gedit", None)), Some(ctrl("KeyV")));
        // 有条目的终端按条目处理（xterm：Shift+Insert）
        assert_eq!(paste_combo(&table, &app("xterm", Some("xterm"))), Some(shift_insert()));
    }

    #[test]
    fn primary_paste_terminals() {
        let table = RwLock::new(Vec::new());
        // 表外的 rxvt 一族：Shift+Insert（Ctrl+Shift+V 在 urxvt 里是 ISO 14755 输入模式）
        assert_eq!(paste_combo(&table, &app("urxvt", Some("URxvt"))), Some(shift_insert()));
        assert_eq!(paste_combo(&table, &app("kitty", None)), Some(ctrl_shift("KeyV")));
        // Shift+Insert 读 PRIMARY：粘贴前要同时占有 PRIMARY；Ctrl+V / Ctrl+Shift+V 读 CLIPBOARD
        assert!(reads_primary(&shift_insert()));
        assert!(!reads_primary(&ctrl("KeyV")));
        assert!(!reads_primary(&ctrl_shift("KeyV")));
        let ctrl_insert = Combo {
            modifiers: Modifiers {
                control: true,
                shift: true,
                ..Modifiers::default()
            },
            code: "Insert".to_owned(),
        };
        assert!(!reads_primary(&ctrl_insert));
    }
}
