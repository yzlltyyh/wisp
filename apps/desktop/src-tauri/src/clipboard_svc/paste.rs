//! 粘贴流程与 `copy_text`。
//!
//! # 状态机（剪贴板流程的显式建模）
//! 一次粘贴是一个显式的状态机：[`PasteStage`] × [`PasteEvent`] → [`paste_step`]（转移表见下，单测逐格覆盖），由
//! [`PasteGate`] 单飞并按面板会话去重。
//!
//! | 阶段 \ 事件 | Begin | Written | WriteFailed | Cancelled | CopyOnly | Hidden | Pasted / PasteFailed |
//! | --- | --- | --- | --- | --- | --- | --- | --- |
//! | Idle | → Writing | 非法 | 非法 | 非法 | 非法 | 非法 | 非法 |
//! | Writing | Busy | → Handoff | → Idle | 非法 | 非法 | 非法 | 非法 |
//! | Handoff | Busy | 非法 | 非法 | → Idle | → Idle | → Pasting | 非法 |
//! | Pasting | Busy | 非法 | 非法 | 非法 | 非法 | 非法 | → Idle |
//!
//! - **重入**：粘贴进行中（任何非 Idle 阶段）再来的请求直接拒绝（`conflict`），不写入、不 touch、不 toast；
//! - **按面板会话去重**：一个剪贴板面板会话只交接一次——进入 Handoff 并离开之后（面板已隐藏或已被结束），同一会话再来
//!   的请求也拒绝；面板没有显示剪贴板视图（已隐藏、正在隐藏）时的请求同样拒绝。单击粘贴模式下双击条目、Windows 写剪贴板
//!   （`EmptyClipboard` 最长约 1.5 s）期间再按一次 ↩，都只粘贴一次，也不会多出 touch 与 toast；
//! - **失败**（取载荷、文件都已不存在、写入失败）：回到 Idle，面板保持打开，同一会话可以换一条或重试；
//! - **取消**：写入期间面板被别人结束（Esc、失焦、截图前隐藏）时不再发键、不弹 toast，内容留在剪贴板里；
//! - **toast 在闸门之外**：显示 toast 要与主线程往返（最长约 1 s），状态机先回到 Idle 再显示——这期间用户新打开的剪贴板
//!   面板可以照常粘贴（这次粘贴的会话已记为交接过，不受影响）；
//! - 非法转移在 debug 构建断言失败，发布构建记 error 并按「回到 Idle」处理。
//!
//! # 顺序
//! 取载荷 → **写入**（面板仍有焦点：Wayland 只允许获焦的客户端设置 selection）→ **抑制**这次写入 → **touch**
//! （只更新时间与次数、移到顶部，不改来源与 HTML）→ 核对会话仍有效 →（Windows：先把前台交还目标）
//! → `panel_hide_now(Paste)`（立即隐藏、无退出动画）并**等窗口离屏**（≤ [`PASTE_HIDE_WAIT`]：
//! macOS 的 orderOut 投递到主线程执行，非激活面板仍是 key 窗口时发出的 ⌘V 会落进面板自己）→ `paste_into(target)` →
//! 埋点「选中 → 发出粘贴键」。
//!
//! - 纯文本粘贴写只含 `text` 的载荷，再 `paste_into(plain = true)`（三端都不在 `paste_into` 里改写剪贴板）；
//! - 写入用默认 `WriteOpts`：macOS 上 transient 会被 Wisp 自己读成 concealed；自身写入的识别靠
//!   `from_wisp` 与抑制区间，监听方看到也会跳过；
//! - 只复制的各分支都以 toast 视图提示，返回 `Copied { reason }`：⌘↩ 仅复制（CopyOnly，先把焦点还给原应用）、
//!   不知道原应用（Manual；Wayland 会话改为 Wayland，见 [`no_target_reason`]）、
//!   目标已退出 / 最小化、焦点切换超时、提权、无辅助功能权限、Wayland；其余失败（缺 XTEST、该应用的粘贴键设为不发送、
//!   发键失败）只提示按键、不写原因（Manual，见 [`copied_reason`]）；
//! - 写入失败（例如 KDE Wayland 的 `wayland_clipboard_bridge`）时**不隐藏面板**、原样返回平台错误（message 是裸 key）：
//!   面板仍有焦点，前端退回 `navigator.clipboard`；
//! - 文件条目只写仍然存在的文件（用户的明确操作里才 stat），纯文本粘贴的路径文本同样只列这些；一个都不在了时
//!   返回 `not_found`，面板保持打开。

use std::time::{Duration, Instant};

use parking_lot::Mutex;
use wisp_platform::common::keymap::{self, KeyChoice};
use wisp_platform::{PasteError, PlatformError, SuppressRange, WriteOpts, reason};
use wisp_types::{
    AppRef, Capability, ClipPayload, CopiedReason, OsKind, PasteMode, PasteOutcome, RawClipboard, ToastPayload,
};

use crate::error::{IpcError, IpcErrorCode};

use super::engine::{Engine, store_error};
use super::files;
use super::port::PASTE_HIDE_WAIT;

/// 自身写入的抑制时长（与取词回退的抑制 ttl 相同；Windows 的去抖最长 250 ms，覆盖得住）
pub const SELF_WRITE_TTL: Duration = Duration::from_millis(300);

/// 埋点里的发键方式名（`perf paste method=…`）
pub(crate) fn paste_method(os: OsKind) -> &'static str {
    match os {
        OsKind::Macos => "cg_event",
        OsKind::Windows => "send_input",
        OsKind::Linux => "xtest",
    }
}

/// Wayland 会话不能替用户粘贴的两个原因（不能注入按键 / 取不到、切不了前台应用）。按原因 key 分支只用
/// `wisp_platform::reason` 的常量
fn is_wayland_paste_reason(key: &str) -> bool {
    key == reason::WAYLAND_PASTE || key == reason::WAYLAND_FOCUS
}

/// `paste_into` 的失败 → toast 的原因
pub(crate) fn copied_reason(e: &PasteError) -> CopiedReason {
    match e {
        PasteError::TargetGone => CopiedReason::TargetGone,
        PasteError::FocusTimeout => CopiedReason::FocusTimeout,
        PasteError::Elevated => CopiedReason::Elevated,
        PasteError::PermissionMissing => CopiedReason::NoPermission,
        // Wayland 会话（「已复制，按 Ctrl+V 粘贴」）
        PasteError::Unsupported(key) if is_wayland_paste_reason(key) => CopiedReason::Wayland,
        // 其余（X11 缺 XTEST、该应用的粘贴键被设为不发送、发键失败等）：内容已在剪贴板里，提示按键、不写原因
        // （`Manual`：不借用 FocusTimeout 说错原因，也不像 CopyOnly 那样漏掉按键提示）
        PasteError::Unsupported(_) | PasteError::Other(_) => CopiedReason::Manual,
    }
}

/// 不知道原应用（target 为 None）时 toast 的原因。target 为 None 只表示「不知道原应用」（从托盘 / 非 Command 方式打开、
/// 平台取不到前台应用），从来不表示「原应用已经关闭」（原应用退出时 target 不为 None，由 `paste_into` 返回
/// TargetGone），所以这里是 `Manual`：只提示按键、不写原因（不用 TargetGone）。
/// Wayland 会话里平台取不到前台应用（`FocusTracker::frontmost` 恒为 None），几乎每次粘贴都会走到这里，此时
/// 提示 Wayland 的原因。判断依据是平台报告的「模拟粘贴键」能力
pub(crate) fn no_target_reason(paste_inject: &Capability) -> CopiedReason {
    let wayland = !paste_inject.available && paste_inject.reason_key.as_deref().is_some_and(is_wayland_paste_reason);
    if wayland {
        CopiedReason::Wayland
    } else {
        CopiedReason::Manual
    }
}

/// Windows 预激活失败 → toast 的原因（失败时不再调用 `paste_into`：它会再等一次前台切换，失败路径因此要等两次）
fn activate_failure_reason(e: &PlatformError) -> CopiedReason {
    match e {
        PlatformError::Timeout => CopiedReason::FocusTimeout,
        // 平台层「target window is gone」：检查之后、激活之前窗口刚好关闭
        _ => CopiedReason::TargetGone,
    }
}

// ---------- 状态机 ----------

/// 一次粘贴所处的阶段
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PasteStage {
    /// 没有进行中的粘贴
    Idle,
    /// 取载荷、写入剪贴板、抑制、touch（面板仍显示、仍有焦点）
    Writing,
    /// 交接：核对会话、只复制分支、Windows 预激活、隐藏面板并等它离屏
    Handoff,
    /// `paste_into` 进行中
    Pasting,
}

/// 驱动粘贴状态机的事件
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PasteEvent {
    /// 新的粘贴请求
    Begin,
    /// 写入、抑制、touch 完成
    Written,
    /// 取载荷 / 写入失败（面板保持打开）
    WriteFailed,
    /// 写入期间面板会话已被别人结束：不发键
    Cancelled,
    /// 只复制收尾（⌘↩、不知道原应用、预激活失败）：面板已隐藏（toast 在回到 Idle 之后显示）
    CopyOnly,
    /// 面板已离屏（或等待超时）
    Hidden,
    /// `paste_into` 成功
    Pasted,
    /// `paste_into` 失败：只复制（toast 在回到 Idle 之后显示）
    PasteFailed,
}

/// 一次转移的结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PasteStep {
    Next(PasteStage),
    /// 粘贴进行中：拒绝新的请求
    Busy,
    /// 代码路径与状态机不一致（实现错误）
    Illegal,
}

/// 转移表（见模块文档）
pub(crate) fn paste_step(stage: PasteStage, event: PasteEvent) -> PasteStep {
    use PasteEvent as E;
    use PasteStage as S;
    use PasteStep::{Busy, Illegal, Next};
    match (stage, event) {
        (S::Idle, E::Begin) => Next(S::Writing),
        (_, E::Begin) => Busy,
        (S::Writing, E::Written) => Next(S::Handoff),
        (S::Writing, E::WriteFailed) => Next(S::Idle),
        (S::Handoff, E::Cancelled | E::CopyOnly) => Next(S::Idle),
        (S::Handoff, E::Hidden) => Next(S::Pasting),
        (S::Pasting, E::Pasted | E::PasteFailed) => Next(S::Idle),
        _ => Illegal,
    }
}

/// 粘贴的单飞闸门：当前阶段 + 进行中的会话 + 已经交接过的会话
#[derive(Debug)]
pub(crate) struct PasteGate {
    inner: Mutex<GateState>,
}

#[derive(Debug)]
struct GateState {
    stage: PasteStage,
    /// 进行中的粘贴所属的面板会话
    session: u64,
    /// 最近一个已经交接（面板已隐藏 / 已结束）的会话：同一会话不再粘贴
    handed_off: Option<u64>,
}

impl Default for PasteGate {
    fn default() -> Self {
        Self {
            inner: Mutex::new(GateState {
                stage: PasteStage::Idle,
                session: 0,
                handed_off: None,
            }),
        }
    }
}

impl PasteGate {
    /// 当前阶段（单测用）
    #[cfg(test)]
    pub(crate) fn stage(&self) -> PasteStage {
        self.inner.lock().stage
    }

    /// 开始一次粘贴：`session` 是当前剪贴板面板会话（没有时为 None）
    fn begin(&self, session: Option<u64>) -> Result<PasteRun<'_>, IpcError> {
        let Some(session) = session else {
            return Err(rejected("clipboard panel is not showing"));
        };
        let mut g = self.inner.lock();
        if g.handed_off == Some(session) {
            return Err(rejected("this clipboard panel has already pasted"));
        }
        match paste_step(g.stage, PasteEvent::Begin) {
            PasteStep::Next(next) => {
                g.stage = next;
                g.session = session;
                Ok(PasteRun { gate: self, session })
            }
            PasteStep::Busy | PasteStep::Illegal => Err(rejected("a paste is already in progress")),
        }
    }
}

/// 被拒绝的粘贴请求（重入、同一会话再次粘贴、面板已不显示剪贴板）：前端静默忽略
fn rejected(why: &str) -> IpcError {
    log::debug!("clipboard paste rejected: {why}");
    IpcError::new(IpcErrorCode::Conflict, why)
}

/// 进行中的一次粘贴（持有闸门的「进行中」状态，离开时保证回到 Idle）
struct PasteRun<'a> {
    gate: &'a PasteGate,
    session: u64,
}

impl PasteRun<'_> {
    /// 按转移表前进一步
    fn advance(&mut self, event: PasteEvent) {
        let mut g = self.gate.inner.lock();
        match paste_step(g.stage, event) {
            PasteStep::Next(next) => {
                // 离开 Handoff（面板已隐藏或已被结束）：这个会话交接过了
                if g.stage == PasteStage::Handoff {
                    g.handed_off = Some(self.session);
                }
                g.stage = next;
            }
            step => {
                debug_assert!(false, "illegal paste transition {:?} × {event:?} → {step:?}", g.stage);
                log::error!("illegal paste transition {:?} × {event:?}", g.stage);
                g.stage = PasteStage::Idle;
            }
        }
    }
}

impl Drop for PasteRun<'_> {
    fn drop(&mut self) {
        let mut g = self.gate.inner.lock();
        if g.stage != PasteStage::Idle {
            // 只有实现遗漏（或 panic 展开）才会走到这里：回到 Idle，不让闸门永远卡在「进行中」
            if !std::thread::panicking() {
                debug_assert!(false, "paste run dropped in stage {:?}", g.stage);
            }
            log::error!("paste run ended in stage {:?}", g.stage);
            g.stage = PasteStage::Idle;
        }
    }
}

// ---------- 流程 ----------

/// 一次粘贴流程走完（状态机已回到 Idle、闸门已放开）之后的收尾
struct Finished {
    outcome: PasteOutcome,
    /// 要显示的 toast（原因、目标应用：粘贴键按目标应用的映射表显示）
    toast: Option<(CopiedReason, Option<AppRef>)>,
}

impl Finished {
    fn pasted() -> Self {
        Self {
            outcome: PasteOutcome::Pasted,
            toast: None,
        }
    }

    /// 只复制，不弹 toast（写入期间面板已被别人结束）
    fn silent(reason: CopiedReason) -> Self {
        Self {
            outcome: PasteOutcome::Copied { reason },
            toast: None,
        }
    }

    /// 只复制并弹 toast
    fn copied(reason: CopiedReason, target: Option<AppRef>) -> Self {
        Self {
            outcome: PasteOutcome::Copied { reason },
            toast: Some((reason, target)),
        }
    }
}

impl Engine {
    /// 粘贴一个条目（流程与状态机见模块文档）。`target` 是命令层在隐藏之前取到的剪贴板会话的 `prev_app`。**会阻塞**
    pub(crate) fn paste(
        &self,
        id: i64,
        mode: PasteMode,
        target: Option<AppRef>,
        selected_at: Instant,
    ) -> Result<PasteOutcome, IpcError> {
        let finished = self.run_paste(id, mode, target, selected_at)?;
        // toast 会阻塞（主线程往返）：放在单飞闸门之外，状态机此时已回到 Idle
        if let Some((reason, target)) = &finished.toast {
            self.toast(*reason, target.as_ref());
        }
        Ok(finished.outcome)
    }

    /// 状态机驱动的粘贴流程（持有单飞闸门；返回时闸门已回到 Idle）
    fn run_paste(
        &self,
        id: i64,
        mode: PasteMode,
        target: Option<AppRef>,
        selected_at: Instant,
    ) -> Result<Finished, IpcError> {
        let mut run = self.paste_gate.begin(self.host.clipboard_view().session())?;
        // Writing
        let plain = mode == PasteMode::Plain;
        let store = match self.write_for_paste(id, plain) {
            Ok(store) => store,
            Err(e) => {
                run.advance(PasteEvent::WriteFailed);
                return Err(e);
            }
        };
        // touch：失败（例如条目刚被删除）不影响这次粘贴
        if let Err(e) = self.touch(&store, id) {
            log::info!("touch after paste failed: {}", e.message);
        }
        run.advance(PasteEvent::Written);

        // Handoff：写入期间（Windows 最长约 1.5 s）面板可能已被用户关掉——不再发键，内容留在剪贴板里
        if !self.host.session_live(run.session) {
            log::info!("clipboard panel closed while writing; paste cancelled (content stays copied)");
            run.advance(PasteEvent::Cancelled);
            return Ok(Finished::silent(CopiedReason::CopyOnly));
        }
        let target = match (mode, target) {
            (PasteMode::CopyOnly, target) => {
                if let Some(t) = target.as_ref() {
                    // ⌘↩ 仅复制也把焦点还给原应用（macOS 的面板不激活应用，隐藏后自然还原）
                    self.restore_focus(t);
                }
                return Ok(self.copy_only(&mut run, CopiedReason::CopyOnly, target));
            }
            (_, None) => {
                let reason = no_target_reason(&self.port.paste_inject());
                return Ok(self.copy_only(&mut run, reason, None));
            }
            (_, Some(target)) => target,
        };
        // Windows：先 SetForegroundWindow(prev)，再隐藏面板；只对记录的那个、仍可用的窗口这样做
        if self.os == OsKind::Windows {
            match self.port.target_check(&target) {
                Ok(()) => {
                    if let Err(e) = self.port.focus().activate(&target) {
                        let reason = activate_failure_reason(&e);
                        log::info!("activate paste target failed ({e}); copy only reason={reason:?}");
                        return Ok(self.copy_only(&mut run, reason, Some(target)));
                    }
                }
                Err(why) => log::debug!("paste target not pre-activated: {why}"),
            }
        }
        self.hide_and_wait(run.session);
        run.advance(PasteEvent::Hidden);

        // Pasting
        match self.port.paste().paste_into(&target, plain) {
            Ok(()) => {
                self.host.perf_paste(paste_method(self.os), selected_at, Instant::now());
                run.advance(PasteEvent::Pasted);
                Ok(Finished::pasted())
            }
            Err(e) => {
                let reason = copied_reason(&e);
                log::info!("paste fell back to copy reason={reason:?}: {e}");
                run.advance(PasteEvent::PasteFailed);
                Ok(Finished::copied(reason, Some(target)))
            }
        }
    }

    /// 取载荷、只留仍存在的文件、写入并抑制。返回数据库（随后 touch 用）。**会阻塞**
    fn write_for_paste(&self, id: i64, plain: bool) -> Result<std::sync::Arc<wisp_clipboard::ClipStore>, IpcError> {
        let store = self.open_store("paste")?;
        // 总是取完整载荷：文件条目的路径列表在纯文本模式下也要按「仍存在」过滤（纯文本的路径文本由它生成）
        let payload = store
            .get_payload(id, false)
            .map_err(store_error)?
            .ok_or_else(|| IpcError::not_found("clipboard item is gone or its image is missing"))?;
        let payload = paste_payload(payload, plain)?;
        // 写入 → 抑制（监听方即便抢在抑制之前看到，也会因 from_wisp 跳过）
        let change = self.port.paste().write(&payload, WriteOpts::default())?;
        self.port.clipboard().suppress(SuppressRange {
            from_change_id: change,
            ttl: SELF_WRITE_TTL,
        });
        Ok(store)
    }

    /// 只复制收尾：隐藏面板；toast 由 [`Engine::paste`] 在闸门放开之后显示
    fn copy_only(&self, run: &mut PasteRun<'_>, reason: CopiedReason, target: Option<AppRef>) -> Finished {
        // 只复制不发键：不必等面板离屏
        drop(self.host.hide_for_paste(run.session));
        run.advance(PasteEvent::CopyOnly);
        Finished::copied(reason, target)
    }

    /// 隐藏面板并等它离屏（≤ [`PASTE_HIDE_WAIT`]；超时只记日志，照常发键）
    fn hide_and_wait(&self, session: u64) {
        let started = Instant::now();
        let Some(offscreen) = self.host.hide_for_paste(session) else {
            return;
        };
        if !offscreen.wait(PASTE_HIDE_WAIT) {
            log::info!(
                "clipboard panel not confirmed off-screen within {}ms; pasting anyway",
                PASTE_HIDE_WAIT.as_millis()
            );
        } else {
            log::debug!("clipboard panel off-screen after {}ms", started.elapsed().as_millis());
        }
    }

    /// 把文本写入系统剪贴板并照常进入历史（OCR 自动复制、复制按钮）。**会阻塞**（Windows 写剪贴板最长约 1.5 s）
    pub(crate) fn copy_text(&self, text: &str) -> Result<(), IpcError> {
        let change = self
            .port
            .paste()
            .write(&ClipPayload::text(text), WriteOpts::default())?;
        self.port.clipboard().suppress(SuppressRange {
            from_change_id: change,
            ttl: SELF_WRITE_TTL,
        });
        if !self.recording() {
            return Ok(());
        }
        // 监听方会因 from_wisp 跳过这次变更，由这里直接记录（来源记为 Wisp 自己）
        let raw = RawClipboard {
            change_id: change,
            source: Some(self.host.self_app()),
            text: Some(text.to_owned()),
            ..RawClipboard::default()
        };
        if let Err(e) = self.ingest(&raw) {
            log::info!("record copied text failed: {}", e.message);
        }
        Ok(())
    }

    /// 只复制时把焦点还给原应用（Windows / X11；macOS 的面板是非激活的，什么都不用做）。Windows 上同样只认记录的那个、
    /// 仍可用的窗口：不恢复最小化的窗口，也不把同一应用的其他窗口拉到前台
    fn restore_focus(&self, target: &AppRef) {
        if self.os == OsKind::Macos {
            return;
        }
        if self.os == OsKind::Windows
            && let Err(why) = self.port.target_check(target)
        {
            log::debug!("focus not returned after copy: {why}");
            return;
        }
        if let Err(e) = self.port.focus().activate(target) {
            log::debug!("restore focus after copy failed: {e}");
        }
    }

    /// 在原面板位置显示 toast（会阻塞，主线程往返；只在闸门之外调用）
    fn toast(&self, reason: CopiedReason, target: Option<&AppRef>) {
        self.host.show_toast(ToastPayload {
            reason,
            paste_key: self.paste_key_label(target),
        });
    }

    /// 当前平台 / 目标应用实际的粘贴键显示串（「⌘V」「Ctrl+Shift+V」）：按映射表（用户条目优先）取键，再交给快捷键模块
    /// 统一格式化（与设置页、托盘同一套显示规则）
    pub(crate) fn paste_key_label(&self, target: Option<&AppRef>) -> String {
        let settings = self.host.settings();
        let keymaps = crate::app::merged_keymaps(&settings.hotkeys.app_keymaps);
        let canonical = match keymap::paste_key(&keymaps, self.os, target) {
            KeyChoice::Send(combo) => combo.to_canonical(),
            KeyChoice::Skip => keymap::default_paste(self.os).to_owned(),
        };
        crate::hotkeys::display::display(&canonical, self.os)
    }
}

/// 由完整载荷得出要写入的载荷：文件条目只保留仍然存在的文件（纯文本时写这些路径逐行拼接的文本），一个都不在了时返回
/// `not_found`；纯文本粘贴去掉 HTML / RTF（与 `ClipStore::get_payload(plain = true)` 同一口径，图片照常给图片）
pub(crate) fn paste_payload(mut payload: ClipPayload, plain: bool) -> Result<ClipPayload, IpcError> {
    if let Some(paths) = payload.files.take() {
        let total = paths.len();
        let alive = files::existing(paths);
        if alive.is_empty() {
            return Err(IpcError::not_found("files no longer exist"));
        }
        if alive.len() < total {
            log::info!("paste files: {} of {total} no longer exist", total - alive.len());
        }
        payload.text = Some(alive.join("\n"));
        payload.files = (!plain).then_some(alive);
    }
    if plain {
        payload.html = None;
        payload.rtf = None;
    }
    Ok(payload)
}
