//! 取词：UIA 首选 → 模拟 Ctrl+C 回退。
//!
//! 回退的 7 步：① 记录剪贴板序号 s0 → ② 从 s0 起登记抑制区间 → ③ 全类型备份（≤10 MB、≤50 ms），
//! 备份结束后重新读取序号作为基线 → ④ 处理残留修饰键后发复制键（按映射表）→ ⑤ 每 10 ms 查序号是否离开基线，
//! 最多 `copy_wait` → ⑥ 读文本 → ⑦ 立即恢复。序号一直没变说明剪贴板没被动过，直接返回 Empty。目标已提权时不注入
//! （UIPI）。
//!
//! 基线为什么在备份之后读：快照逐格式调用 `GetClipboardData`，原所有者对延迟渲染的格式（Excel、Office、浏览器都
//! 常用）会在 `WM_RENDERFORMAT` 里调用 `SetClipboardData`，序号可能因此增加。若拿备份之前的 s0 比较，第 ⑤ 步会在
//! 目标应用响应复制键之前就认为「已复制」，把原剪贴板里的文本当成选区返回。抑制区间仍从 s0 开放，渲染造成的
//! 序号变化同样被抑制；快照与第 ⑥ 步的读取还会把渲染造成的序号登记为同内容别名（见 `clipboard::ReadGuard`）。

use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use wisp_types::{Anchor, AppRef};

use crate::common::text::{MAX_SELECTION_BYTES, is_blank, normalize_selection};
use crate::error::PlatformError;
use crate::traits::{SelectedText, SelectionOpts};
use crate::types::{CaptureMethod, Selection, SelectionError};

use super::Core;
use super::clip_watch::ClipHub;
use super::clipboard::{self, SnapshotOutcome};
use super::geom::{self, PhysRectF};
use super::input;
use super::monitor;
use super::process;
use super::uia::{UiaError, UiaSelection};
use super::util::{millis, run_with_timeout, spawn_call};

/// UIA 首选路径的等待上限（UIA 自身的连接 / 事务超时是 100 ms）
const UIA_WAIT: Duration = Duration::from_millis(110);
/// 备份上限
const BACKUP_MAX_BYTES: usize = 10 * 1024 * 1024;
/// 备份读取时限
const BACKUP_DEADLINE: Duration = Duration::from_millis(50);
/// 备份超时后，再有界地等快照线程关闭剪贴板的时长。它还开着剪贴板（例如卡在原所有者的延迟渲染里）时发复制键，
/// 目标应用的 `OpenClipboard` 会失败（Office 还会弹「无法打开剪贴板」），这次复制注定拿不到结果
const BACKUP_GRACE: Duration = Duration::from_millis(50);
/// 复制后读取文本的时限
const READ_TIMEOUT: Duration = Duration::from_millis(200);
/// 读取文本时 `OpenClipboard` 的重试预算：序号一变就去读，此时目标应用往往还开着剪贴板（Excel / Word 复制大范围
/// 时会占用上百毫秒），在时限内每 10 ms 重试一次，留出读取本身的时间
const READ_OPEN_BUDGET: Duration = Duration::from_millis(180);
/// 恢复的时限
const RESTORE_TIMEOUT: Duration = Duration::from_millis(300);
/// 恢复时 `OpenClipboard` 的重试预算
const RESTORE_OPEN_BUDGET: Duration = Duration::from_millis(220);
/// 恢复后抑制区间的存活时间
const SUPPRESS_TTL: Duration = Duration::from_millis(300);
/// 恢复超时后开放区间的存活时间：覆盖恢复线程迟到的写入（`OpenClipboard` 重试 + 写入通常远小于 1 s）
const RESTORE_LATE_TTL: Duration = Duration::from_millis(1000);

/// 剪贴板过大且历史关闭时跳过模拟复制的原因串（i18n key，与 Linux 一致）：上层据此提示
/// 「剪贴板内容过大，未执行自动复制」
pub(crate) const REASON_CLIPBOARD_TOO_LARGE: &str = crate::reason::CLIPBOARD_TOO_LARGE;

/// `SelectedText` 的实现
pub(crate) struct WinSelection(pub(crate) Arc<Core>);

/// 一次取词的路径日志（不含文本）
struct Trace<'a> {
    app: &'a str,
    uia_ms: Option<u32>,
    copy_ms: Option<u32>,
    backup_abandoned: bool,
}

impl Trace<'_> {
    fn emit(&self, method: &str, result: &str) {
        log::info!(
            target: "wisp_platform::selection",
            "app={} method={} uia_ms={} copy_ms={} result={} backup_abandoned={}",
            self.app,
            method,
            self.uia_ms.map_or_else(|| "-".to_owned(), |v| v.to_string()),
            self.copy_ms.map_or_else(|| "-".to_owned(), |v| v.to_string()),
            result,
            self.backup_abandoned
        );
    }
}

impl SelectedText for WinSelection {
    fn get(&self, opts: &SelectionOpts) -> Result<Selection, SelectionError> {
        let core = &self.0;
        let t0 = Instant::now();
        let source = opts.target.clone().or_else(|| core.focus.frontmost());
        let app_id = source.as_ref().map(|a| a.id.clone()).unwrap_or_default();
        let mut trace = Trace {
            app: &app_id,
            uia_ms: None,
            copy_ms: None,
            backup_abandoned: false,
        };

        // 1. UIA
        let uia = core
            .uia
            .selection(t0 + UIA_WAIT, true, MAX_SELECTION_BYTES.saturating_add(1));
        trace.uia_ms = Some(millis(t0.elapsed()));
        let uia_timed_out = matches!(uia, Err(UiaError::Timeout));
        match uia {
            Ok(UiaSelection::Text { text, rect }) => {
                let (text, truncated) = normalize_selection(&text);
                if !is_blank(&text) {
                    trace.emit("uia", "ok");
                    return Ok(Selection {
                        text,
                        truncated,
                        anchor: rect.and_then(|r| anchor_of(&r)),
                        source,
                        method: CaptureMethod::Uia,
                        elapsed_ms: millis(t0.elapsed()),
                    });
                }
            }
            Ok(UiaSelection::Password) => {
                // 密码框：不读取，也不模拟复制（复制在密码框里本来就被禁用）
                trace.emit("uia", "empty");
                return Err(SelectionError::Empty);
            }
            Ok(UiaSelection::NoPattern) | Err(_) => {}
        }

        if !opts.allow_copy_fallback {
            let result = if uia_timed_out { "timeout" } else { "empty" };
            trace.emit("none", result);
            return Err(if uia_timed_out {
                SelectionError::Timeout
            } else {
                SelectionError::Empty
            });
        }

        // 2. 模拟复制回退
        let t1 = Instant::now();
        let r = copy_fallback(core, opts, source.as_ref(), &mut trace);
        trace.copy_ms = Some(millis(t1.elapsed()));
        match r {
            Ok(text) => {
                let (text, truncated) = normalize_selection(&text);
                if is_blank(&text) {
                    trace.emit("copy", "empty");
                    return Err(SelectionError::Empty);
                }
                trace.emit("copy", "ok");
                Ok(Selection {
                    text,
                    truncated,
                    anchor: None,
                    source,
                    method: CaptureMethod::Copy,
                    elapsed_ms: millis(t0.elapsed()),
                })
            }
            Err(e) => {
                let result = match &e {
                    SelectionError::Empty => "empty",
                    SelectionError::Elevated => "elevated",
                    SelectionError::Timeout => "timeout",
                    SelectionError::Unsupported(_) => "unsupported",
                    SelectionError::PermissionMissing(_) => "permission",
                    SelectionError::Other(_) => "error",
                };
                trace.emit("copy", result);
                Err(e)
            }
        }
    }

    fn has_selection(&self, deadline: Duration) -> Option<bool> {
        self.0.uia.has_selection(Instant::now() + deadline)
    }

    fn link_at_cursor(&self, deadline: Duration) -> Option<bool> {
        self.0.uia.link_at_cursor(Instant::now() + deadline)
    }
}

/// 选区矩形 → 锚点（拿不到合理矩形时返回 None，上层退回鼠标位置）
fn anchor_of(r: &PhysRectF) -> Option<Anchor> {
    let (cx, cy) = r.center();
    let m = monitor::monitor_at(geom::round_i32(cx), geom::round_i32(cy))?;
    geom::plausible_anchor(r, &m).then(|| Anchor {
        rect: geom::phys_rect_to_local(&m, r),
        monitor_id: m.id,
    })
}

fn copy_fallback(
    core: &Arc<Core>,
    opts: &SelectionOpts,
    source: Option<&AppRef>,
    trace: &mut Trace<'_>,
) -> Result<String, SelectionError> {
    // 复制键会送到当前真正的前台窗口：它是 Wisp 自己或 shell 时不注入
    let Some(fg) = core.focus.foreground_valid_root() else {
        return Err(SelectionError::Empty);
    };
    let (_, pid) = process::window_thread_process(fg);
    if process::is_elevated(pid) {
        return Err(SelectionError::Elevated);
    }
    // 复制键按真正接收按键的前台窗口查映射表（通常就是触发时记录的 source）。控制台窗口的所属进程是客户端
    // （cmd.exe / python.exe …），另按 conhost.exe 匹配，避免退回会中断前台程序的 Ctrl+C
    let target = match source {
        Some(a) if a.pid == Some(pid) => Some(a.clone()),
        _ => core.names.app_for_pid(pid, None),
    };
    // 映射表要求不发复制键（例如 X11 的 xterm；Windows 上用户也可以这样配置）：直接视为没有选区
    let Some(combo) = input::app_keys_for_window(&core.keymaps, target.as_ref(), fg, true) else {
        return Err(SelectionError::Empty);
    };
    let wait = opts
        .copy_wait
        .clamp(Duration::from_millis(50), Duration::from_millis(500));

    // ① ②
    let s0 = clipboard::seq();
    let token = core.clip.suppress_open(s0, wait + Duration::from_millis(800));

    // ③ 备份，并在快照线程关闭剪贴板之后重新取基线
    let Ok((snapshot, s_base)) = take_backup(&core.clip) else {
        log::info!(
            target: "wisp_platform::selection",
            "clipboard backup is still holding the clipboard; copy fallback skipped"
        );
        core.clip.suppress_cancel(token);
        return Err(SelectionError::Timeout);
    };
    if s_base != s0 {
        log::debug!(
            target: "wisp_platform::selection",
            "clipboard sequence moved {s0} -> {s_base} during the backup (delayed rendering)"
        );
    }
    // 放弃备份的原因：过大 / 超时（「剪贴板内容过大」），或剪贴板被占用 / 读取出错
    let (snapshot, failure) = match snapshot {
        Some(Ok(SnapshotOutcome::Taken(s, _))) => (Some(s), None),
        Some(Ok(SnapshotOutcome::Abandoned(why))) => {
            log::info!(target: "wisp_platform::selection", "clipboard backup abandoned: {why}");
            (None, None)
        }
        Some(Err(e)) => {
            log::info!(target: "wisp_platform::selection", "clipboard backup failed: {e}");
            (None, Some(e))
        }
        None => {
            log::info!(target: "wisp_platform::selection", "clipboard backup abandoned: backup thread unavailable");
            (None, None)
        }
    };
    trace.backup_abandoned = snapshot.is_none();
    if snapshot.is_none() && !opts.history_enabled {
        // 历史关闭时原内容无处找回，不冒险覆盖剪贴板
        core.clip.suppress_cancel(token);
        return Err(match failure {
            // 剪贴板在备份时限内一直被其他程序占用
            Some(PlatformError::Busy(_) | PlatformError::Timeout) => SelectionError::Timeout,
            Some(e) => SelectionError::Other(e.to_string()),
            None => SelectionError::Unsupported(REASON_CLIPBOARD_TOO_LARGE.to_owned()),
        });
    }

    // ④ 发复制键
    if let Err(e) = input::send_combo(combo) {
        core.clip.suppress_cancel(token);
        return Err(SelectionError::Other(e));
    }

    // ⑤ 等序号离开基线
    if !wait_for_copy(s_base, wait) {
        // 剪贴板没被动过（渲染只是把原内容的延迟格式变成了实际数据，内容没有变）：无需恢复
        core.clip.suppress_cancel(token);
        return Err(SelectionError::Empty);
    }

    // ⑥ 读文本
    let hub = Arc::clone(&core.clip);
    let text = run_with_timeout("wisp-clip-read", READ_TIMEOUT, move || {
        clipboard::read_text(MAX_SELECTION_BYTES.saturating_add(1), READ_OPEN_BUDGET, Some(&hub))
    });

    // ⑦ 恢复（备份被放弃时保留复制结果：原内容在历史里）
    let s_end = match snapshot {
        Some(snap) => {
            let owner = core.clip.owner();
            match run_with_timeout("wisp-clip-restore", RESTORE_TIMEOUT, move || {
                clipboard::restore(snap, owner, RESTORE_OPEN_BUDGET)
            }) {
                Some(Ok(s)) => Some(s),
                Some(Err(e)) => {
                    log::warn!("clipboard restore failed: {e}");
                    Some(clipboard::seq())
                }
                None => {
                    // 恢复线程仍在等剪贴板，稍后才会真正写入：区间保持开放，免得把迟到的恢复记成新条目
                    log::warn!("clipboard restore timed out");
                    None
                }
            }
        }
        None => Some(clipboard::seq()),
    };
    match s_end {
        Some(s) => core.clip.suppress_close(token, s, SUPPRESS_TTL),
        None => core.clip.suppress_extend(token, RESTORE_LATE_TTL),
    }

    match text {
        Some(Ok(Some(t))) => Ok(t),
        Some(Ok(None)) => Err(SelectionError::Empty),
        // 目标应用在读取预算内一直占着剪贴板：按超时处理
        Some(Err(PlatformError::Busy(_) | PlatformError::Timeout)) | None => Err(SelectionError::Timeout),
        Some(Err(e)) => Err(SelectionError::Other(e.to_string())),
    }
}

/// ③ 全类型备份。返回备份结果与第 ⑤ 步比较用的基线序号：基线在快照线程关闭剪贴板**之后**读取（见模块文档）。
/// 快照线程在截止时间之后不再读剩余格式；收到它的结果即说明它已关闭剪贴板。`Err(())` 表示等满宽限期后它仍占着
/// 剪贴板（卡在原所有者的延迟渲染里），此时不能发复制键
pub(super) fn take_backup(hub: &Arc<ClipHub>) -> Result<(Option<Result<SnapshotOutcome, PlatformError>>, u64), ()> {
    let backup_deadline = Instant::now() + BACKUP_DEADLINE;
    let hub = Arc::clone(hub);
    let snapshot = match spawn_call("wisp-clip-snapshot", move || {
        clipboard::snapshot(BACKUP_MAX_BYTES, backup_deadline, Some(&hub))
    }) {
        // 无法启动快照线程：没有人占着剪贴板，按「备份被放弃」继续
        None => None,
        Some(rx) => await_backup(&rx)?,
    };
    Ok((snapshot, clipboard::seq()))
}

/// ⑤ 每 10 ms 查一次序号，直到它离开 `base`（返回 true）或等满 `wait`（返回 false）
pub(super) fn wait_for_copy(base: u64, wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    loop {
        if clipboard::seq() != base {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10).min(deadline - now));
    }
}

/// 等待备份结果：先等正常时限，超时后再有界地等 [`BACKUP_GRACE`]，让快照线程放弃剩余格式、关闭剪贴板。
/// `Ok(None)` 表示快照线程已经不在（剪贴板守卫随之释放）；`Err(())` 表示它仍占着剪贴板
fn await_backup<T>(rx: &std::sync::mpsc::Receiver<T>) -> Result<Option<T>, ()> {
    let first = match rx.recv_timeout(BACKUP_DEADLINE + Duration::from_millis(30)) {
        Ok(r) => return Ok(Some(r)),
        Err(e) => e,
    };
    if first == RecvTimeoutError::Disconnected {
        return Ok(None);
    }
    match rx.recv_timeout(BACKUP_GRACE) {
        Ok(r) => Ok(Some(r)),
        Err(RecvTimeoutError::Disconnected) => Ok(None),
        Err(RecvTimeoutError::Timeout) => Err(()),
    }
}
