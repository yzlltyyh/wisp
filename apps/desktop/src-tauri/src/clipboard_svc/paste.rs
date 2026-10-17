//! 粘贴流程与 `copy_text`（以及 OCR 重新识别时取代上一次自动复制的 `copy_text_replacing`、切换排版后让剪贴板跟随的
//! `recopy_text`）。
//!
//! # 统一管线
//! 所有粘贴都是一个 [`PasteRequest`]，经 [`Engine::paste`] 走同一个状态机，按两个维度分支：
//! - **来源**（[`PasteSource`]，决定 Writing 写什么）：整条条目、「粘贴为…」变换后的文本、片段、多条合并。后三种的写入
//!   同样带自写抑制，不会成为新的历史条目，只 touch 来源条目；
//! - **宿主**（[`PasteHost`]，决定 Handoff 怎么交接、目标应用是谁、只复制时谁来提示）：面板（隐藏面板、等离屏、Rust 弹
//!   toast）、悬浮剪贴板（不隐藏，先交还键盘焦点）、连续粘贴的队列（按键由队列重放，这里只写入）。宿主由调用窗口的 label
//!   与面板当前的视图推出（[`PasteHost::of_window`]），不信任前端自报：条目只能在剪贴板视图里粘贴，片段还可以在 OCR
//!   视图里粘贴（[`PasteKind`]）。
//!
//! 加一种来源 = 给 [`PasteSource`] 加一个变体并在 Writing 里给出载荷；加一种宿主 = 给 [`PasteHost`] 加一个变体并实现它的交接。
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
//! - **按面板会话去重**：一个面板会话只交接一次——进入 Handoff 并离开之后（面板已隐藏或已被结束），同一会话再来
//!   的请求也拒绝；面板已不在显示这个会话（已隐藏、正在隐藏、换了会话）时的请求同样拒绝。单击粘贴模式下双击条目、
//!   Windows 写剪贴板（`EmptyClipboard` 最长约 1.5 s）期间再按一次 ↩，都只粘贴一次，也不会多出 touch 与 toast。
//!   悬浮窗与队列没有会话，只单飞；
//! - **失败**（取载荷、文件都已不存在、写入失败）：回到 Idle，面板保持打开，同一会话可以换一条或重试；
//! - **取消**：写入期间面板被别人结束（Esc、失焦、截图前隐藏）时不再发键、不弹 toast，内容留在剪贴板里；
//! - **toast 在闸门之外**：显示 toast 要与主线程往返（最长约 1 s），状态机先回到 Idle 再显示——这期间用户新打开的剪贴板
//!   面板可以照常粘贴（这次粘贴的会话已记为交接过，不受影响）；
//! - 非法转移在 debug 构建断言失败，发布构建记 error 并按「回到 Idle」处理。
//!
//! # 顺序（面板宿主；悬浮窗把「隐藏面板并等离屏」换成「交还键盘焦点」，队列在 touch 之后就结束）
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

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use wisp_clipboard::{ClipStore, IngestOutcome, smart};
use wisp_platform::common::keymap::{self, KeyChoice};
use wisp_platform::{PasteError, PlatformError, SuppressRange, WriteOpts, reason};
use wisp_types::{
    AppRef, Capability, ClipPayload, CopiedReason, OsKind, PanelView, PasteMode, PasteOutcome, RawClipboard,
    ToastPayload, TransformId,
};

use crate::error::{IpcError, IpcErrorCode};
use crate::windows::panel::PANEL_LABEL;

use super::engine::{Engine, store_error};
use super::files;
use super::port::PASTE_HIDE_WAIT;

/// 自身写入的抑制时长（与取词回退的抑制 ttl 相同；Windows 的去抖最长 250 ms，覆盖得住）
pub const SELF_WRITE_TTL: Duration = Duration::from_millis(300);

/// `copy_text` 的回执：OCR 自动复制把它记在截图上，切换排版后据此判断剪贴板还是不是这次写入的内容、该替换历史里的哪一条
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyReceipt {
    /// 写入后系统剪贴板的 change_id
    pub change_id: u64,
    /// 这次写入在剪贴板历史里新建的条目。内容早已在历史里（只是移到顶部）、没有在记录（暂停、关闭）、被跳过或记录
    /// 失败时为 None——那样的条目不是这次复制产生的，切换排版时不能删
    pub created: Option<CopiedItem>,
}

/// 一次写入新建的历史条目：行号连同内容 hash。行号没有 AUTOINCREMENT，这一条被删掉后行号可能分给别的条目（例如同步
/// 拉来的新条目），所以删除之前要核对 hash 仍是这一条的
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopiedItem {
    pub id: i64,
    pub hash: [u8; 32],
}

/// 一次自身写入在历史里的结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Recorded {
    /// 新建了这一条
    Created(CopiedItem),
    /// 记进了历史、但不归这次写入：内容早已在历史里（这一条被移到顶部），或新建了却没读到它的 hash（无法核对身份的
    /// 条目以后不删）。`hash` 是这一行此刻的内容 hash（读不到时为 None）
    Existing { id: i64, hash: Option<[u8; 32]> },
}

impl Recorded {
    fn created(self) -> Option<CopiedItem> {
        match self {
            Self::Created(item) => Some(item),
            Self::Existing { .. } => None,
        }
    }
}

/// 一次自身写入之后的交接：新回执记下的新建条目，以及被这次写入取代、要删掉的上一条
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Succession {
    pub created: Option<CopiedItem>,
    pub discard: Option<CopiedItem>,
}

/// 同一张截图又一次写剪贴板（重新识别的自动复制、切换排版的跟随）之后，上一次写入新建的那一条（`prev`）怎么处理。
/// `replace`：剪贴板仍是上一次写入的内容（期间没有别的复制），可以取代它；`recorded`：这次写入在历史里的结果。
/// - 内容相同只会把上一次新建的那一条移到顶部（行号与内容 hash 都对得上）：它仍是这张截图的复制新建的，继续记在新回执上，
///   之后的写入还能替换它；
/// - 可以取代、新文本也进了历史：删掉上一条（删除时还会核对 hash，收藏过的保留），历史里只留这一次的；
/// - 可以取代、但新文本没能进入历史（暂停、被隐私规则跳过、记录失败）：不删——只删不增会让这次结果从历史里消失；旧条目
///   留在新回执上，下一次写入时再替换；
/// - 期间有过别的复制：上一条已经成了普通的历史，不删，也不再记着它
pub(crate) fn succession(prev: Option<CopiedItem>, replace: bool, recorded: Option<Recorded>) -> Succession {
    match (prev, recorded) {
        (Some(old), Some(Recorded::Existing { id, hash })) if id == old.id && hash == Some(old.hash) => Succession {
            created: Some(old),
            discard: None,
        },
        (Some(old), Some(r)) if replace => Succession {
            created: r.created(),
            discard: Some(old),
        },
        (Some(old), None) if replace => Succession {
            created: Some(old),
            discard: None,
        },
        (_, r) => Succession {
            created: r.and_then(Recorded::created),
            discard: None,
        },
    }
}

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
    /// 只复制收尾（⌘↩、不知道原应用、预激活失败、悬浮窗交还不了焦点）：面板宿主的面板已隐藏（toast 在回到 Idle 之后显示）
    CopyOnly,
    /// 交接完成：面板已离屏（或等待超时）、悬浮窗已交还键盘焦点，或队列（无需交接）
    Hidden,
    /// 粘贴键已发出（`paste_into` 成功；队列由它自己重放）
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
    /// 最近一个已经交接（面板已隐藏 / 已结束）的面板会话：同一会话不再粘贴
    handed_off: Option<u64>,
}

impl Default for PasteGate {
    fn default() -> Self {
        Self {
            inner: Mutex::new(GateState {
                stage: PasteStage::Idle,
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

    /// 开始一次粘贴。`session` 是面板宿主的会话（按会话去重）；悬浮窗与队列为 None，只单飞
    fn begin(&self, session: Option<u64>) -> Result<PasteRun<'_>, IpcError> {
        let mut g = self.inner.lock();
        if session.is_some() && g.handed_off == session {
            return Err(rejected("this panel session has already pasted"));
        }
        match paste_step(g.stage, PasteEvent::Begin) {
            PasteStep::Next(next) => {
                g.stage = next;
                Ok(PasteRun { gate: self, session })
            }
            PasteStep::Busy | PasteStep::Illegal => Err(rejected("a paste is already in progress")),
        }
    }
}

/// 被拒绝的粘贴请求（重入、同一会话再次粘贴、面板已不显示能承接这次粘贴的视图）：前端静默忽略
fn rejected(why: &str) -> IpcError {
    log::debug!("clipboard paste rejected: {why}");
    IpcError::new(IpcErrorCode::Conflict, why)
}

/// 进行中的一次粘贴（持有闸门的「进行中」状态，离开时保证回到 Idle）
struct PasteRun<'a> {
    gate: &'a PasteGate,
    /// 面板宿主的会话（悬浮窗与队列为 None）
    session: Option<u64>,
}

impl PasteRun<'_> {
    /// 按转移表前进一步
    fn advance(&mut self, event: PasteEvent) {
        let mut g = self.gate.inner.lock();
        match paste_step(g.stage, event) {
            PasteStep::Next(next) => {
                // 离开 Handoff（面板已隐藏或已被结束）：这个会话交接过了
                if g.stage == PasteStage::Handoff && self.session.is_some() {
                    g.handed_off = self.session;
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

// ---------- 请求 ----------

/// 悬浮剪贴板窗口的 label。不用 `pinned-` 前缀：那是翻译 / OCR 固定窗口的命名空间（`pinned::id_of` 会把它当成固定窗口）
pub const FLOAT_LABEL: &str = "clipfloat";

/// 粘贴什么。条目之外的来源（变换结果、片段、合并文本）写入时同样带自写抑制，**不会**成为新的历史条目，只 touch 来源条目
#[derive(Clone, PartialEq, Eq)]
pub enum PasteSource {
    /// 一个条目；`transform` 给出时粘贴的是「粘贴为…」变换后的文本
    Item { id: i64, transform: Option<TransformId> },
    /// 一段文字（芯片条的片段、识别结果里的片段）；`from` 是它所在的条目（粘贴后 touch 它）
    Fragment { from: Option<i64>, text: String },
    /// 多个条目按给出的顺序以换行合并成一段文本（多选的合并粘贴）
    Many { ids: Vec<i64> },
}

// 手写 Debug：片段是用户内容，只输出字符数
impl std::fmt::Debug for PasteSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Item { id, transform } => f
                .debug_struct("Item")
                .field("id", id)
                .field("transform", transform)
                .finish(),
            Self::Fragment { from, text } => f
                .debug_struct("Fragment")
                .field("from", from)
                .field("chars", &text.chars().count())
                .finish(),
            Self::Many { ids } => f.debug_struct("Many").field("count", &ids.len()).finish(),
        }
    }
}

impl PasteSource {
    /// 粘贴之后要 touch 的条目，按 touch 的先后：合并粘贴倒着 touch，第一条（用户最先选的）最后被 touch、排在最上面
    fn touched(&self) -> Vec<i64> {
        match self {
            Self::Item { id, .. } => vec![*id],
            Self::Fragment { from, .. } => from.iter().copied().collect(),
            Self::Many { ids } => ids.iter().rev().copied().collect(),
        }
    }

    /// 这次粘贴属于哪一种（决定面板的哪些视图能当宿主）
    pub(crate) fn kind(&self) -> PasteKind {
        match self {
            Self::Item { .. } | Self::Many { .. } => PasteKind::Items,
            Self::Fragment { .. } => PasteKind::Fragment,
        }
    }
}

/// 粘贴的种类：决定面板正显示哪些视图时可以当宿主（[`PasteKind::panel_hosts`]）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteKind {
    /// 剪贴板历史里的条目（整条、「粘贴为…」、多条合并、连续粘贴），以及从剪贴板面板交接出悬浮剪贴板：条目 id 来自
    /// 剪贴板视图列出的行，只有它能承接
    Items,
    /// 一段文字（芯片条的片段、OCR 结果里识别出的片段）：不引用历史条目，剪贴板视图与 OCR 视图都能承接
    Fragment,
}

impl PasteKind {
    /// 面板正显示 `view` 时能不能当这种粘贴的宿主。OCR 视图与剪贴板视图一样记下了触发时的前台应用（截图之前取），
    /// 粘贴后隐藏面板、把内容送回那个应用的流程对两者相同；翻译、输入视图没有可以粘贴的片段，toast 不接受交互
    pub fn panel_hosts(self, view: PanelView) -> bool {
        match view {
            PanelView::Clipboard => true,
            PanelView::Ocr => self == Self::Fragment,
            PanelView::Translate | PanelView::Input | PanelView::Toast => false,
        }
    }
}

/// 粘贴发生在哪里。由调用窗口的 label 与面板当前的视图推出（[`PasteHost::of_window`]），不由前端自报——这同时就是
/// 权限校验；队列只由 Rust 内部发起
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteHost {
    /// 面板的这个会话（剪贴板视图，或承接片段的 OCR 视图）：写入后立即隐藏面板、等它离屏再发键；一个会话只交接一次。
    /// 目标应用是会话的 `prev_app`
    Panel { session: u64 },
    /// 悬浮剪贴板：窗口不隐藏，发键前把键盘焦点交还目标应用（[`Host::float_yield`](super::port::Host::float_yield)）。
    /// 目标应用是当前的前台应用（平台的前台查询跳过 Wisp 自己的窗口）
    Floating,
    /// 连续粘贴的下一条：用户已经在目标应用里按下了粘贴键，那组按键由队列在写入之后自己重放，这里只写入与 touch
    Queue,
}

impl PasteHost {
    /// 由调用窗口推出宿主（纯函数）：面板必须正显示能承接 `kind` 的视图（`panel` 是面板当前的会话与视图，隐藏中、
    /// 已结束时为 None），否则返回 `conflict`（前端静默）；悬浮剪贴板窗口为 [`PasteHost::Floating`]；其余窗口不能粘贴
    pub fn of_window(label: &str, panel: Option<(u64, PanelView)>, kind: PasteKind) -> Result<Self, IpcError> {
        if label == PANEL_LABEL {
            return hosting_session(panel, kind)
                .map(|session| Self::Panel { session })
                .ok_or_else(|| rejected("the panel is not showing a view that can paste this"));
        }
        if label == FLOAT_LABEL {
            return Ok(Self::Floating);
        }
        Err(IpcError::invalid_argument(format!("window {label} cannot paste")))
    }

    /// 只复制的结果要不要由 Rust 弹 toast：只有面板（它已经隐藏，toast 视图显示在原位置）；悬浮窗在自己窗口里提示，
    /// 队列经 `clipboard:queue` 报告进度
    fn toasts(self) -> bool {
        matches!(self, Self::Panel { .. })
    }
}

/// 面板里能承接这种粘贴的会话：面板正显示（准备中或已显示）、且视图能承接 `kind`。命令层推出宿主与管线开始时的核对
/// 用的是同一条规则
fn hosting_session(panel: Option<(u64, PanelView)>, kind: PasteKind) -> Option<u64> {
    panel
        .filter(|&(_, view)| kind.panel_hosts(view))
        .map(|(session, _)| session)
}

/// 一次粘贴请求（所有粘贴的唯一入口 [`Engine::paste`]）
#[derive(Debug, Clone)]
pub struct PasteRequest {
    pub source: PasteSource,
    pub mode: PasteMode,
    pub host: PasteHost,
    /// 选中条目的时刻（命令进入的时刻），埋点「选中 → 发出粘贴键」的起点
    pub selected_at: Instant,
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

    /// 只复制，不弹 toast（写入期间面板已被别人结束；悬浮窗与队列自己提示）
    fn silent(reason: CopiedReason) -> Self {
        Self {
            outcome: PasteOutcome::Copied { reason },
            toast: None,
        }
    }

    /// 只复制：宿主是面板时弹 toast（[`PasteHost::toasts`]）
    fn copied(host: PasteHost, reason: CopiedReason, target: Option<AppRef>) -> Self {
        Self {
            outcome: PasteOutcome::Copied { reason },
            toast: host.toasts().then_some((reason, target)),
        }
    }
}

impl Engine {
    /// 统一的粘贴管线：每种来源、每种宿主都走这里（流程与状态机见模块文档）。**会阻塞**
    pub(crate) fn paste(&self, req: PasteRequest) -> Result<PasteOutcome, IpcError> {
        let finished = self.run_paste(&req)?;
        // toast 会阻塞（主线程往返）：放在单飞闸门之外，状态机此时已回到 Idle
        if let Some((reason, target)) = &finished.toast {
            self.toast(*reason, target.as_ref());
        }
        Ok(finished.outcome)
    }

    /// 状态机驱动的粘贴流程（持有单飞闸门；返回时闸门已回到 Idle）
    fn run_paste(&self, req: &PasteRequest) -> Result<Finished, IpcError> {
        let mut run = self.paste_gate.begin(self.gate_key(req)?)?;
        // 目标应用在写入之前取：面板会话结束之后就取不到它的 prev_app 了
        let target = self.paste_target(req.host);
        // Writing
        let plain = req.mode == PasteMode::Plain;
        let store = match self.write_for_paste(&req.source, plain) {
            Ok(store) => store,
            Err(e) => {
                run.advance(PasteEvent::WriteFailed);
                return Err(e);
            }
        };
        for id in req.source.touched() {
            // touch：失败（例如条目刚被删除）不影响这次粘贴
            if let Err(e) = self.touch(&store, id) {
                log::info!("touch after paste failed: {}", e.message);
            }
        }
        run.advance(PasteEvent::Written);

        // Handoff → Pasting：按宿主交接
        Ok(match req.host {
            PasteHost::Panel { session } => self.handoff_panel(&mut run, session, req.mode, target, req.selected_at),
            PasteHost::Floating => self.handoff_floating(&mut run, req.mode, target, req.selected_at),
            PasteHost::Queue => handoff_queue(&mut run),
        })
    }

    /// 闸门的键：面板按会话去重（一个会话只交接一次），且面板必须仍在显示这个会话、视图仍能承接这次粘贴（命令层推出
    /// 宿主之后，面板可能已经隐藏或换成了别的会话）；悬浮窗与队列只单飞
    fn gate_key(&self, req: &PasteRequest) -> Result<Option<u64>, IpcError> {
        match req.host {
            PasteHost::Panel { session } => {
                if hosting_session(self.host.panel_view(), req.source.kind()) == Some(session) {
                    Ok(Some(session))
                } else {
                    Err(rejected("the panel is no longer showing this session"))
                }
            }
            PasteHost::Floating | PasteHost::Queue => Ok(None),
        }
    }

    /// 粘贴的目标应用（见 [`PasteHost`]）。队列不发键，不需要目标。**会阻塞**（前台查询）
    fn paste_target(&self, host: PasteHost) -> Option<AppRef> {
        match host {
            PasteHost::Panel { session } => self.host.panel_prev_app(session),
            PasteHost::Floating => self.port.focus().frontmost(),
            PasteHost::Queue => None,
        }
    }

    /// 面板：核对会话 → 只复制分支 →（Windows：先交还前台）→ 隐藏面板并等它离屏 → 发键
    fn handoff_panel(
        &self,
        run: &mut PasteRun<'_>,
        session: u64,
        mode: PasteMode,
        target: Option<AppRef>,
        selected_at: Instant,
    ) -> Finished {
        let host = PasteHost::Panel { session };
        // 写入期间（Windows 最长约 1.5 s）面板可能已被用户关掉——不再发键，内容留在剪贴板里
        if !self.host.session_live(session) {
            log::info!("panel closed while writing; paste cancelled (content stays copied)");
            run.advance(PasteEvent::Cancelled);
            return Finished::silent(CopiedReason::CopyOnly);
        }
        let target = match (mode, target) {
            (PasteMode::CopyOnly, target) => {
                if let Some(t) = target.as_ref() {
                    // ⌘↩ 仅复制也把焦点还给原应用（macOS 的面板不激活应用，隐藏后自然还原）
                    self.restore_focus(t);
                }
                return self.copy_only_panel(run, session, CopiedReason::CopyOnly, target);
            }
            (_, None) => {
                let reason = no_target_reason(&self.port.paste_inject());
                return self.copy_only_panel(run, session, reason, None);
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
                        return self.copy_only_panel(run, session, reason, Some(target));
                    }
                }
                Err(why) => log::debug!("paste target not pre-activated: {why}"),
            }
        }
        self.hide_and_wait(session);
        run.advance(PasteEvent::Hidden);
        self.post_paste(run, host, &target, mode == PasteMode::Plain, selected_at)
    }

    /// 悬浮窗：窗口不隐藏；把键盘焦点交还目标应用之后发键。只复制的结果不弹 toast（悬浮窗在自己窗口里提示）
    fn handoff_floating(
        &self,
        run: &mut PasteRun<'_>,
        mode: PasteMode,
        target: Option<AppRef>,
        selected_at: Instant,
    ) -> Finished {
        let target = match (mode, target) {
            (PasteMode::CopyOnly, _) => {
                run.advance(PasteEvent::CopyOnly);
                return Finished::silent(CopiedReason::CopyOnly);
            }
            (_, None) => {
                run.advance(PasteEvent::CopyOnly);
                return Finished::silent(no_target_reason(&self.port.paste_inject()));
            }
            (_, Some(target)) => target,
        };
        // 悬浮窗可能是 key 窗口（用户在它的搜索框里打过字）：交还不了就只复制——这时发出的粘贴键会落进悬浮窗自己
        if !self.host.float_yield(&target) {
            log::info!("floating clipboard kept the keyboard focus; copy only");
            run.advance(PasteEvent::CopyOnly);
            return Finished::silent(CopiedReason::FocusTimeout);
        }
        run.advance(PasteEvent::Hidden);
        self.post_paste(run, PasteHost::Floating, &target, mode == PasteMode::Plain, selected_at)
    }

    /// Pasting：`paste_into` 发出粘贴键并记埋点；失败时只复制
    fn post_paste(
        &self,
        run: &mut PasteRun<'_>,
        host: PasteHost,
        target: &AppRef,
        plain: bool,
        selected_at: Instant,
    ) -> Finished {
        match self.port.paste().paste_into(target, plain) {
            Ok(()) => {
                self.host.perf_paste(paste_method(self.os), selected_at, Instant::now());
                run.advance(PasteEvent::Pasted);
                Finished::pasted()
            }
            Err(e) => {
                let reason = copied_reason(&e);
                log::info!("paste fell back to copy reason={reason:?}: {e}");
                run.advance(PasteEvent::PasteFailed);
                Finished::copied(host, reason, Some(target.clone()))
            }
        }
    }

    /// Writing：按来源得出要写的载荷，写入并抑制。返回数据库（随后 touch 用）。**会阻塞**
    fn write_for_paste(&self, source: &PasteSource, plain: bool) -> Result<Arc<ClipStore>, IpcError> {
        let store = self.open_store("paste")?;
        let payload = match source {
            PasteSource::Item { id, transform: None } => {
                // 总是取完整载荷：文件条目的路径列表在纯文本模式下也要按「仍存在」过滤（纯文本的路径文本由它生成）
                let payload = store
                    .get_payload(*id, false)
                    .map_err(store_error)?
                    .ok_or_else(|| IpcError::not_found("clipboard item is gone or its image is missing"))?;
                paste_payload(payload, plain)?
            }
            PasteSource::Item { id, transform: Some(t) } => {
                let (kind, text) = store
                    .text_of(*id)
                    .map_err(store_error)?
                    .ok_or_else(|| IpcError::not_found("clipboard item not found"))?;
                let text = text.ok_or_else(|| IpcError::invalid_argument("clipboard item has no text"))?;
                // 错误信息只有变换的原因，不含正文
                let out = smart::apply(*t, kind, &text).map_err(|e| IpcError::invalid_argument(e.to_string()))?;
                ClipPayload::text(out)
            }
            PasteSource::Fragment { text, .. } => ClipPayload::text(text.as_str()),
            PasteSource::Many { ids } => ClipPayload::text(merged_text(&store, ids)?),
        };
        // 写入 → 抑制（监听方即便抢在抑制之前看到，也会因 from_wisp 跳过）
        let change = self.port.paste().write(&payload, WriteOpts::default())?;
        self.port.clipboard().suppress(SuppressRange {
            from_change_id: change,
            ttl: SELF_WRITE_TTL,
        });
        Ok(store)
    }

    /// 面板的只复制收尾：隐藏面板；toast 由 [`Engine::paste`] 在闸门放开之后显示
    fn copy_only_panel(
        &self,
        run: &mut PasteRun<'_>,
        session: u64,
        reason: CopiedReason,
        target: Option<AppRef>,
    ) -> Finished {
        // 只复制不发键：不必等面板离屏
        drop(self.host.hide_for_paste(session));
        run.advance(PasteEvent::CopyOnly);
        Finished::copied(PasteHost::Panel { session }, reason, target)
    }

    /// 隐藏面板并等它离屏（≤ [`PASTE_HIDE_WAIT`]；超时只记日志，照常发键）
    fn hide_and_wait(&self, session: u64) {
        let started = Instant::now();
        let Some(offscreen) = self.host.hide_for_paste(session) else {
            return;
        };
        if !offscreen.wait(PASTE_HIDE_WAIT) {
            log::info!(
                "panel not confirmed off-screen within {}ms; pasting anyway",
                PASTE_HIDE_WAIT.as_millis()
            );
        } else {
            log::debug!("panel off-screen after {}ms", started.elapsed().as_millis());
        }
    }

    /// 把文本写入系统剪贴板并照常进入历史（复制按钮），返回这次写入的回执。**会阻塞**（Windows 写剪贴板最长约 1.5 s）
    pub(crate) fn copy_text(&self, text: &str) -> Result<CopyReceipt, IpcError> {
        self.copy_text_replacing(None, text)
    }

    /// OCR 识别完成时的自动复制：照常写入并进入历史。`prev` 是这张截图上一次写剪贴板的回执（之前那次识别的自动复制，
    /// 或之后切换排版的跟随）：系统剪贴板仍是那次写入的内容（`change_id` 没变，期间用户没有复制别的东西）时，它新建的那
    /// 一条被这次取代，经正常的删除路径删掉（同步随之传播删除），重新识别不会每次在历史里多一条；收藏、命名过的保留。
    /// 期间别处复制过时不删。重新识别得到相同的文字时只会把那一条移到顶部，它继续记在新回执上（规则见 [`succession`]）。
    /// **会阻塞**
    pub(crate) fn copy_text_replacing(&self, prev: Option<CopyReceipt>, text: &str) -> Result<CopyReceipt, IpcError> {
        // 写入之前核对：写入本身就会改掉 change_id
        let replace = prev.is_some_and(|p| self.port.clipboard().change_id() == p.change_id);
        let (change_id, recorded) = self.write_and_record(text)?;
        Ok(self.hand_over(prev.and_then(|p| p.created), replace, change_id, recorded))
    }

    /// OCR 切换排版后的剪贴板跟随：系统剪贴板仍是 `prev` 那次写入的内容（`change_id` 没变，期间没有别的复制）时改写为
    /// `text`，照常进入历史；`prev` 新建的那一条随后经正常的删除路径删掉（同步随之传播删除），历史里只留现在这一版。
    /// 收藏过的不删（命名即收藏，存储层保证有名称的条目一定是收藏）。新文本没能进入历史（暂停、被隐私规则跳过、
    /// 记录失败）时也不删：只删不增会让这次识别从历史里消失，旧条目留在回执上，下一次跟随时再替换（规则见
    /// [`succession`]）。剪贴板已被别的复制改写时什么都不做，返回 None。**会阻塞**
    pub(crate) fn recopy_text(&self, prev: CopyReceipt, text: &str) -> Result<Option<CopyReceipt>, IpcError> {
        if self.port.clipboard().change_id() != prev.change_id {
            return Ok(None);
        }
        let (change_id, recorded) = self.write_and_record(text)?;
        Ok(Some(self.hand_over(prev.created, true, change_id, recorded)))
    }

    /// 按 [`succession`] 交接：删掉被这次写入取代的那一条，返回这次写入的回执
    fn hand_over(
        &self,
        prev: Option<CopiedItem>,
        replace: bool,
        change_id: u64,
        recorded: Option<Recorded>,
    ) -> CopyReceipt {
        let next = succession(prev, replace, recorded);
        if let Some(old) = next.discard {
            self.discard_copied(old);
        }
        CopyReceipt {
            change_id,
            created: next.created,
        }
    }

    /// 写入剪贴板、抑制这次变更并记进历史；返回写入后的 change_id 与历史里记下的条目（没有在记录、被跳过或记录失败时为
    /// None）
    fn write_and_record(&self, text: &str) -> Result<(u64, Option<Recorded>), IpcError> {
        let change = self
            .port
            .paste()
            .write(&ClipPayload::text(text), WriteOpts::default())?;
        self.port.clipboard().suppress(SuppressRange {
            from_change_id: change,
            ttl: SELF_WRITE_TTL,
        });
        if !self.recording() {
            return Ok((change, None));
        }
        // 监听方会因 from_wisp 跳过这次变更，由这里直接记录（来源记为 Wisp 自己）
        let raw = RawClipboard {
            change_id: change,
            source: Some(self.host.self_app()),
            text: Some(text.to_owned()),
            ..RawClipboard::default()
        };
        let recorded = match self.ingest_identified(&raw) {
            Ok((IngestOutcome::Inserted(meta), Some(hash))) => {
                Some(Recorded::Created(CopiedItem { id: meta.id, hash }))
            }
            Ok((IngestOutcome::Touched(meta), hash)) => Some(Recorded::Existing { id: meta.id, hash }),
            Ok((IngestOutcome::Inserted(meta), None)) => Some(Recorded::Existing {
                id: meta.id,
                hash: None,
            }),
            Ok((IngestOutcome::Skipped(_), _)) => None,
            Err(e) => {
                log::info!("record copied text failed: {}", e.message);
                None
            }
        };
        Ok((change, recorded))
    }

    /// 删掉一条被新版本取代的自动复制条目：只在这一行仍是当初新建的那一条（内容 hash 一致）时删；收藏过的保留，已经
    /// 不在了、行号已经属于别的条目都视为删掉
    fn discard_copied(&self, item: CopiedItem) {
        let id = item.id;
        match self.delete_checked(id, false, Some(&item.hash)) {
            Ok(true) => log::debug!("replaced copy removed id={id}"),
            Ok(false) => log::debug!("replaced copy is already gone id={id}"),
            Err(e) if e.code == IpcErrorCode::Conflict => log::debug!("replaced copy kept as a favorite id={id}"),
            Err(e) => log::info!("removing the replaced copy failed id={id}: {}", e.message),
        }
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
            queue: None,
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

/// 队列：用户已经在目标应用里按下了粘贴键，队列吞掉了它、在写入之后自己重放同一组键；这里不发键，交接与发键一步完成
fn handoff_queue(run: &mut PasteRun<'_>) -> Finished {
    run.advance(PasteEvent::Hidden);
    run.advance(PasteEvent::Pasted);
    Finished::pasted()
}

/// 合并粘贴的文本：各条目的纯文本（文件条目是仍存在的路径）按给出的顺序以换行连接。已删除、没有文本（图片）、文件都已
/// 不存在的条目跳过；一条都没有时返回 `not_found`
pub(super) fn merged_text(store: &ClipStore, ids: &[i64]) -> Result<String, IpcError> {
    let mut parts = Vec::with_capacity(ids.len());
    for id in ids {
        let Some(payload) = store.get_payload(*id, true).map_err(store_error)? else {
            continue;
        };
        if let Ok(payload) = paste_payload(payload, true)
            && let Some(text) = payload.text.filter(|t| !t.is_empty())
        {
            parts.push(text);
        }
    }
    if parts.is_empty() {
        return Err(IpcError::not_found("none of the clipboard items has text to paste"));
    }
    if parts.len() < ids.len() {
        log::info!(
            "merged paste: {} of {} item(s) had no text",
            ids.len() - parts.len(),
            ids.len()
        );
    }
    Ok(parts.join("\n"))
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
