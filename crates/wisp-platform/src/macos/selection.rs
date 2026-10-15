//! 取词。
//!
//! 首选 AX（无副作用）。AX 阶段（含 Electron 的 AXManualAccessibility 与重试、选区矩形）共用一个 0.1 s 的预算；
//! 为空、不支持、超时、出错时走模拟 ⌘C 回退的 7 步：
//! ① 记录 changeCount → ③ 全类型快照（> 10 MB 或 > 50 ms 放弃；快照期间序号变了说明别人动了剪贴板，快照作废）
//! → ② 布防抑制（键码此前已从缓存解析好，布防之后立即发键）→ ④ 发复制键（Private 事件源、flags 只带组合键要求的
//! 修饰位、键码按布局）→ ⑤ 每 10 ms 查一次序号，最多 copy_wait → ⑥ 读文本（辅助线程，≤ 100 ms）→ ⑦ 立即恢复。
//!
//! 抑制用 [`pasteboard::FallbackGuard`]：从发键前到恢复完成，序号大于 s0 的变更一律不上报，**不按
//! 时间计**——无论快照、等待、读文本、恢复各花多久，监听方都不会在恢复之前把目标应用的复制或恢复的中间状态报出去；
//! 恢复之后 (s0, 恢复后] 整段永久忽略。等待上限内序号始终没变时剪贴板没被动过，直接返回 Empty，守卫改成从 s0+1 起、
//! 再保留 180 ms 的区间，慢应用在等待结束之后才完成的那次复制仍然不进历史。回退整体互斥执行。
//!
//! 耗时上界：AX 0.1 s + 快照 ≤ 50 ms + copy_wait（默认 150 ms）+ 读文本 ≤ 100 ms（只在序号变化后）。
//! AX 很快失败（不支持 / 为空）的常见情况下整体远低于 250 ms；目标应用被 SIGSTOP 时约 0.3 s 返回 Empty。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use wisp_types::{AppRef, OsKind, PermissionKind};

use super::Ctx;
use super::ax::{self, AxFail};
use super::displays;
use super::focus::running_app;
use super::keyboard;
use super::pasteboard;
use super::util::pool;
use crate::common::keymap::{self, KeyChoice};
use crate::common::text::{is_blank, normalize_selection};
use crate::traits::{SelectedText, SelectionOpts};
use crate::types::{CaptureMethod, Selection, SelectionError};

/// 剪贴板过大且历史关闭时跳过模拟复制的原因串（i18n key，三端一致）
pub(crate) const REASON_CLIPBOARD_TOO_LARGE: &str = crate::reason::CLIPBOARD_TOO_LARGE;

/// 回退前备份剪贴板的上限
const BACKUP_MAX_BYTES: usize = 10 * 1024 * 1024;
/// 回退前备份剪贴板的时间上限
const BACKUP_DEADLINE: Duration = Duration::from_millis(50);
/// 复制等待的轮询间隔
const POLL_STEP: Duration = Duration::from_millis(10);
/// 复制等待的允许范围（可配 50–500 ms）
const COPY_WAIT_MIN: Duration = Duration::from_millis(50);
const COPY_WAIT_MAX: Duration = Duration::from_millis(500);
/// 等待上限内没有变化时，抑制区间在等待结束后再保留的余量（覆盖慢应用晚到的复制）
const SUPPRESS_SLACK: Duration = Duration::from_millis(150);
/// 序号变化后等待文本写完的余量（个别应用先 clearContents 再写入）
const SETTLE: Duration = Duration::from_millis(30);
/// 回退第 ⑥ 步读文本（含 SETTLE）的硬截止时间：延迟提供数据的应用卡住时不拖住调用方
const TEXT_READ_DEADLINE: Duration = Duration::from_millis(100);
/// 设置 AXManualAccessibility 失败（应用正忙 / 超时）后，同一个 pid 最多再试几次
const ELECTRON_MAX_ATTEMPTS: u8 = 3;
/// 同一应用连续多少次「AX 为空但复制拿到了文本」后，本次会话内跳过 AX
const AX_SKIP_AFTER: u8 = 3;
/// 跳过期间每隔多少次仍试一次 AX（应用的页面 / 控件变了之后能自动恢复）
const AX_REPROBE_EVERY: u8 = 8;
/// 记录 Electron / AX 跳过名单的应用上限（防止无界增长）
const STATE_CAP: usize = 256;

/// Electron 应用的 AXManualAccessibility 记录（按 pid）
#[derive(Default)]
struct ElectronState {
    /// 已成功开启
    enabled: HashSet<i32>,
    /// 已确认不是 Electron（永久跳过）
    not_electron: HashSet<i32>,
    /// 设置失败的次数（未达上限时下次取词再试）
    failures: HashMap<i32, u8>,
}

impl ElectronState {
    fn len(&self) -> usize {
        self.enabled.len() + self.not_electron.len() + self.failures.len()
    }
}

/// 取词的进程内状态
#[derive(Default)]
pub(crate) struct SelectionState {
    electron: Mutex<ElectronState>,
    /// bundle id → (连续「AX 为空但复制有内容」的次数, 跳过以来的取词次数)
    ax_misses: Mutex<HashMap<String, (u8, u8)>>,
    /// 模拟复制回退的互斥锁
    fallback: Mutex<()>,
}

/// SelectedText 实现
pub(crate) struct MacSelection {
    ctx: Arc<Ctx>,
}

impl MacSelection {
    pub(crate) fn new(ctx: Arc<Ctx>) -> Self {
        Self { ctx }
    }
}

/// AX 一次尝试的结论
enum AxOutcome {
    Text(ax::AxSelection),
    /// 空串 / 不支持
    Empty,
    Timeout,
    Failed,
}

fn try_ax(pid: Option<i32>, budget: &ax::Budget, route: ax::Route) -> Result<AxOutcome, SelectionError> {
    match ax::read_selection(pid, true, budget, route) {
        Ok(Some(sel)) => Ok(AxOutcome::Text(sel)),
        Ok(None) | Err(AxFail::Unsupported) => Ok(AxOutcome::Empty),
        Err(AxFail::PermissionMissing) => Err(SelectionError::PermissionMissing(PermissionKind::Accessibility)),
        Err(AxFail::Timeout) => Ok(AxOutcome::Timeout),
        Err(AxFail::Other(code)) => {
            log::debug!("selection: AX error {code}");
            Ok(AxOutcome::Failed)
        }
    }
}

/// 应用 bundle 里是否带 Electron
fn is_electron(app: &AppRef) -> bool {
    let path = app.path.clone().or_else(|| {
        pool(|| {
            running_app(app)
                .and_then(|r| r.bundleURL())
                .and_then(|u| u.path())
                .map(|p| p.to_string())
        })
    });
    path.is_some_and(|p| {
        std::path::Path::new(&p)
            .join("Contents/Frameworks/Electron Framework.framework")
            .exists()
    })
}

impl MacSelection {
    /// 对 Electron 应用打开完整无障碍树，返回本次是否新开启（需要重试 AX）。
    ///
    /// 「不是 Electron」与「已成功开启」的 pid 永久跳过；设置失败（应用正忙、超时）的允许在之后的取词中重试，
    /// 最多 [`ELECTRON_MAX_ATTEMPTS`] 次
    fn enable_electron(&self, app: &AppRef, budget: &ax::Budget) -> bool {
        let Some(pid) = app.pid.and_then(|p| i32::try_from(p).ok()) else {
            return false;
        };
        // 预算已经用完：这次没有机会尝试，也不计入失败次数
        if budget.expired() {
            return false;
        }
        {
            let state = self.ctx.selection.electron.lock();
            if state.enabled.contains(&pid)
                || state.not_electron.contains(&pid)
                || state.failures.get(&pid).is_some_and(|n| *n >= ELECTRON_MAX_ATTEMPTS)
            {
                return false;
            }
        }
        // 文件系统检查不持锁
        let electron = is_electron(app);
        let result = if electron {
            Some(ax::enable_manual_accessibility(pid, budget))
        } else {
            None
        };
        let mut state = self.ctx.selection.electron.lock();
        if state.len() >= STATE_CAP {
            *state = ElectronState::default();
        }
        match result {
            None => {
                state.not_electron.insert(pid);
                false
            }
            Some(Ok(())) => {
                log::info!("selection: AXManualAccessibility enabled for {} (pid {pid})", app.id);
                state.failures.remove(&pid);
                state.enabled.insert(pid);
                true
            }
            Some(Err(e)) => {
                let n = state.failures.entry(pid).or_insert(0);
                *n = n.saturating_add(1);
                log::info!(
                    "selection: AXManualAccessibility for {} (pid {pid}) failed ({e:?}), attempt {n}",
                    app.id
                );
                false
            }
        }
    }

    /// 本次是否跳过 AX：连续落空达到阈值后跳过，但每隔几次仍试一次
    fn ax_skipped(&self, app: Option<&AppRef>) -> bool {
        let Some(app) = app else { return false };
        let mut misses = self.ctx.selection.ax_misses.lock();
        let Some((n, since)) = misses.get_mut(&app.id) else {
            return false;
        };
        if *n < AX_SKIP_AFTER {
            return false;
        }
        *since = since.wrapping_add(1);
        *since % AX_REPROBE_EVERY != 0
    }

    /// 记录一次结果：`ax_empty_but_copy_found` = AX 明确返回空 / 不支持、而复制拿到了文本
    fn note_ax_result(&self, app: Option<&AppRef>, ax_empty_but_copy_found: bool) {
        let Some(app) = app else { return };
        let mut misses = self.ctx.selection.ax_misses.lock();
        if ax_empty_but_copy_found {
            if misses.len() >= STATE_CAP && !misses.contains_key(&app.id) {
                misses.clear();
            }
            let (n, _) = misses.entry(app.id.clone()).or_insert((0, 0));
            *n = n.saturating_add(1);
            if *n == AX_SKIP_AFTER {
                log::info!(
                    "selection: AX keeps returning empty for {}, using copy directly",
                    app.id
                );
            }
        } else {
            misses.remove(&app.id);
        }
    }

    /// 模拟复制回退。Ok(None) 表示没有拿到文本
    fn copy_fallback(&self, opts: &SelectionOpts, target: Option<&AppRef>) -> Result<Option<String>, SelectionError> {
        let combo = match keymap::copy_key(&self.ctx.keymaps.read(), OsKind::Macos, target) {
            KeyChoice::Send(c) => c,
            KeyChoice::Skip => return Ok(None),
        };
        // 键码按当前布局解析（只读缓存，正常不等主线程）；解析不了就不发键，绝不按 ANSI 位置盲发
        // （Dvorak 下 ANSI 的 C 位置是 J，V 位置是 .，⌘. 在终端里是中断）
        let keys = match keyboard::prepare(&self.ctx, &combo) {
            Ok(k) => k,
            Err(e) => {
                log::warn!("selection: copy key unavailable for the current layout ({e}); skipping simulated copy");
                return Ok(None);
            }
        };
        let wait = opts.copy_wait.clamp(COPY_WAIT_MIN, COPY_WAIT_MAX);
        let _serial = self.ctx.selection.fallback.lock();

        // ① 记录序号 ③ 备份全部类型
        let s0 = pasteboard::change_count();
        let mut backup = pasteboard::snapshot(BACKUP_MAX_BYTES, BACKUP_DEADLINE);
        // 快照本身失败（> 10 MB 或 > 50 ms）与「快照期间被别人改了」要分开报告（见下面「剪贴板内容过大」的分支）
        let snapshot_failed = backup.is_none();
        let s1 = pasteboard::change_count();
        let s0 = if s1 == s0 {
            s0
        } else {
            // 快照期间别人改了剪贴板：快照可能新旧混杂，作废（新内容已由监听方正常记录）
            log::info!("selection: clipboard changed during backup; backup discarded");
            backup = None;
            s1
        };
        if backup.is_none() && !opts.history_enabled {
            // 无法备份且历史关闭：复制会永久覆盖用户的剪贴板，放弃回退。还没布防、没发键。
            // 三端统一：剪贴板过大 / 读不完时返回 `Unsupported(clipboard_too_large)`，上层提示
            // 「剪贴板内容过大，未执行自动复制」；只是恰好被别人改了时按「没取到」处理
            if snapshot_failed {
                log::info!("selection: clipboard too large to back up and history disabled; skipping simulated copy");
                return Err(SelectionError::Unsupported(REASON_CLIPBOARD_TOO_LARGE.to_owned()));
            }
            log::info!("selection: clipboard changed during backup and history disabled; skipping simulated copy");
            return Ok(None);
        }
        if backup.is_none() {
            log::info!(
                "selection: clipboard backup unavailable (too large, slow or raced); original content stays in history"
            );
        }
        // ② 布防：从这里到恢复完成，序号大于 s0 的变更都不上报
        let guard = self.ctx.clip.begin_fallback(s0);
        // ④ 发复制键
        if let Err(e) = keyboard::post(&keys) {
            // 键没有发出去，不会有变更
            guard.cancel();
            return Err(SelectionError::Other(e));
        }
        // ⑤ 等待序号变化
        let started = Instant::now();
        let mut changed = false;
        while started.elapsed() < wait {
            std::thread::sleep(POLL_STEP);
            if pasteboard::change_count() != s0 {
                changed = true;
                break;
            }
        }
        if !changed {
            // 等待上限内剪贴板没有被动过：不需要恢复。抑制再保留一段——慢应用在等待结束之后才完成的复制仍会被
            // 忽略，不进历史
            guard.expire_after(SETTLE + SUPPRESS_SLACK);
            log::debug!("selection: no clipboard change within {wait:?}");
            return Ok(None);
        }
        // ⑥ 读文本（序号先于数据变化时稍等片刻；整个读取有硬截止时间）
        let text = pasteboard::read_plain_text(SETTLE, TEXT_READ_DEADLINE);
        // ⑦ 立即恢复；(s0, 恢复后] 整段不上报（没有备份时，目标应用的那次复制同样不进历史）
        let hi = match backup {
            Some(snap) => pasteboard::restore(&self.ctx, snap).unwrap_or_else(|e| {
                log::warn!("selection: clipboard restore failed: {e}");
                pasteboard::change_count()
            }),
            None => pasteboard::change_count(),
        };
        guard.close(hi);
        Ok(text)
    }

    fn get_inner(&self, opts: &SelectionOpts) -> Result<Selection, SelectionError> {
        let started = Instant::now();
        if !ax::trusted() {
            return Err(SelectionError::PermissionMissing(PermissionKind::Accessibility));
        }
        let target = opts
            .target
            .clone()
            .or_else(|| super::focus::frontmost_excluding(&self.ctx));
        let pid = target.as_ref().and_then(|a| a.pid).and_then(|p| i32::try_from(p).ok());
        let elapsed_ms = |t: Instant| u32::try_from(t.elapsed().as_millis()).unwrap_or(u32::MAX);

        // 「AX 总为空就跳过」只在允许回退时生效：不允许回退时跳过 AX 就等于什么都不做
        let skip_ax = opts.allow_copy_fallback && self.ax_skipped(target.as_ref());
        // AX 阶段（首次读取、Electron 开启与重试、选区矩形）共用一个预算
        let budget = ax::Budget::new(ax::AX_TIMEOUT);
        let mut outcome = if skip_ax {
            AxOutcome::Empty
        } else {
            try_ax(pid, &budget, ax::Route::SystemWideFirst)?
        };
        // 只在 AX 很快返回「空 / 不支持 / 出错」时尝试 Electron；超时说明应用没有响应，重试只会再等一次
        if !skip_ax
            && matches!(outcome, AxOutcome::Empty | AxOutcome::Failed)
            && let Some(app) = target.as_ref()
            && self.enable_electron(app, &budget)
        {
            // 开启的是该 pid 的应用元素，重试直接问它（剩余预算）
            outcome = try_ax(pid, &budget, ax::Route::AppOnly)?;
        }
        if let AxOutcome::Text(sel) = outcome {
            let (text, truncated) = normalize_selection(&sel.text);
            if !is_blank(&text) {
                self.note_ax_result(target.as_ref(), false);
                let selection = Selection {
                    text,
                    truncated,
                    anchor: sel.bounds.and_then(displays::anchor_of),
                    source: target,
                    method: CaptureMethod::Ax,
                    elapsed_ms: elapsed_ms(started),
                };
                log::debug!(
                    "selection: method=ax app={} elapsed={}ms anchor={}",
                    selection.source.as_ref().map_or("-", |a| a.id.as_str()),
                    selection.elapsed_ms,
                    selection.anchor.is_some()
                );
                return Ok(selection);
            }
            outcome = AxOutcome::Empty;
        }
        let ax_timed_out = matches!(outcome, AxOutcome::Timeout);
        // 只有 AX 明确回答「空 / 不支持」才算一次落空；超时、出错（应用一时卡顿）不计
        let ax_said_empty = !skip_ax && matches!(outcome, AxOutcome::Empty);
        if !opts.allow_copy_fallback {
            return Err(if ax_timed_out {
                SelectionError::Timeout
            } else {
                SelectionError::Empty
            });
        }
        let copied = self.copy_fallback(opts, target.as_ref())?;
        let text = copied.map(|t| normalize_selection(&t)).filter(|(t, _)| !is_blank(t));
        let app_id = target.as_ref().map_or("-", |a| a.id.as_str()).to_owned();
        match text {
            Some((text, truncated)) => {
                if ax_said_empty {
                    self.note_ax_result(target.as_ref(), true);
                }
                let elapsed = elapsed_ms(started);
                log::debug!("selection: method=copy app={app_id} elapsed={elapsed}ms");
                Ok(Selection {
                    text,
                    truncated,
                    anchor: None,
                    source: target,
                    method: CaptureMethod::Copy,
                    elapsed_ms: elapsed,
                })
            }
            None => {
                log::debug!("selection: empty app={app_id} elapsed={}ms", elapsed_ms(started));
                Err(SelectionError::Empty)
            }
        }
    }
}

impl SelectedText for MacSelection {
    fn get(&self, opts: &SelectionOpts) -> Result<Selection, SelectionError> {
        pool(|| self.get_inner(opts))
    }

    fn has_selection(&self, deadline: Duration) -> Option<bool> {
        pool(|| ax::has_selection(deadline))
    }

    fn link_at_cursor(&self, deadline: Duration) -> Option<bool> {
        let p = super::screen::cursor_global();
        pool(|| ax::link_at(p, deadline, None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 恢复写入（本进程内的 clearContents + writeObjects，备份上限 10 MB）在最坏情况下的留量
    const RESTORE_ALLOWANCE: Duration = Duration::from_secs(1);

    #[test]
    fn pending_guard_outlives_the_slowest_fallback() {
        // 守卫在发键之前布防（快照与键码解析都已完成，不计入），一直到恢复完成才关闭；它只有一个安全网式的
        // 存活上限。这个上限必须覆盖「发键 → 等待上限 → 读文本截止 → 恢复」的最坏耗时，否则慢应用的复制或恢复的
        // 中间状态可能在恢复之前被监听方报出去
        let worst = COPY_WAIT_MAX + TEXT_READ_DEADLINE + RESTORE_ALLOWANCE;
        assert!(
            worst < pasteboard::PENDING_MAX_AGE,
            "{worst:?} must stay below {:?}",
            pasteboard::PENDING_MAX_AGE
        );
        // 读文本的截止时间包含等待数据写完的余量
        assert!(SETTLE < TEXT_READ_DEADLINE);
    }
}
