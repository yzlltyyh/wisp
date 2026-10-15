//! 扩展触发（浮标 / 双击修饰键 / 鼠标键）的安装与派发。
//!
//! - 运行时默认关闭；全部关闭时**不装钩子、不起线程、不留任务**（`InputHooks::install` 只在规格非空时调用，派发任务随
//!   最后一个发送端一起结束）。需要平台钩子的代码统一用 desktop 自己的 `#[cfg(feature = "mouse-hook")]` 包裹；
//! - 任意时刻只保留**一个完整的**规格：设置或权限变化时先 install 新规格、成功后再 drop 旧 Handle。
//!   macOS 上同一规格缺任何一项所需权限整体返回 `PermissionMissing`，这里按项拆分重试：浮标与鼠标键要
//!   辅助功能，双击修饰键要输入监控——缺的那几项进入「等待授权」，其余照常生效；
//! - 失败不影响仍在工作的旧钩子：整体安装遇到 `HookError::Other`（tap 线程启动超时、CGEventTapCreate 失败…）时按项
//!   逐个累加安装。旧钩子做的每件事仍是用户想要的（参数也相同）时以旧规格为起点、只逐个尝试新增的项——每次装上的
//!   都是旧规格的超集，一项都没加上就保留旧 Handle；旧钩子里有用户已经关掉、改了参数或失去权限的项时它必须注销，
//!   此时从空规格起步（先试旧规格里仍想要的项）。装不上的项标为不可用，并安排一次延迟重试。逐项尝试受时间预算约束
//!   （`SPLIT_BUDGET`），线程启动超时这类与具体哪一项无关的失败不会把锁占上好几秒；
//! - 权限变化只在可能改变结果时才重装（`perms_matter`）：启动时权限缓存从占位值变成真实基线、而上次各项都已生效
//!   时不会把同一规格再装一遍；
//! - 钩子回调（sink）只把信号连同到达时刻投进无界通道（O(1)），派发在 tokio 任务里进行；
//! - 浮标：候选（mouse-up）后等 80 ms，在阻塞线程上用 `has_selection` 只读一次选区（最晚在 mouse-up 后
//!   120 ms 内完成，**绝不模拟复制**），非空才由 `BobberWindow` 显示；点击后发 `Command::Action { TranslateSelection,
//!   Bobber }`（完整取词流程，浮标是非激活窗口，源应用的选区仍在）。候选后 200 ms 内的 `BobberDismiss(MouseDown)` 取消
//!   尚未完成的探测（三击会取消双击的探测）；其余 `BobberDismiss` 只是通知——浮标已由平台层自己隐藏，重复、过期的
//!   通知一律无害（不依赖它们维护任何会影响点击的状态）；
//! - 双击修饰键的时序状态机在平台层（`common::double_tap`），这里只把 `DoubleTap` 派发为划词翻译；
//! - 鼠标键：平台层负责中键判定（选区 + 链接，30 ms）与吞掉事件，这里只派发绑定的动作。X11 不拦截
//!   中键：Linux 上中键绑定不交给平台层，状态里如实说明（`errors.platform.x11_middle_button`）。

#[cfg(any(test, feature = "mouse-hook"))]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[cfg(any(test, feature = "mouse-hook"))]
use wisp_platform::types::{DismissReason, InputSignal};
use wisp_platform::types::{DoubleTapSpec, HookError, HookSpec, MouseSpec};
#[cfg(any(test, feature = "mouse-hook"))]
use wisp_types::HotkeyAction;
use wisp_types::defaults::{DOUBLE_TAP_INTERVALS_MS, default_double_tap_key};
use wisp_types::{
    ExtTriggerState, ExtTriggerStatus, HotkeySettings, OsKind, PermissionKind, PermissionSet, PermissionState,
};

/// 扩展触发的 i18n key（hotkeys.json 的 `ext` 组）
pub mod reason {
    pub const NEED_INPUT_MONITORING: &str = "hotkeys.ext.need_input_monitoring";
    pub const NEED_ACCESSIBILITY: &str = "hotkeys.ext.need_accessibility";
    pub const UNAVAILABLE: &str = "hotkeys.ext.unavailable";
}

/// 按设置期望安装的扩展触发
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Desired {
    pub bobber: bool,
    pub double_tap: Option<DoubleTapSpec>,
    /// 交给平台层的鼠标规格（总开关关闭或一个键都没绑定时为 None）
    pub mouse: Option<MouseSpec>,
    /// Linux：用户绑定了中键，但 X11 不拦截中键，已从规格里去掉
    pub middle_dropped: bool,
    /// Linux：当前是 Wayland 会话（此时中键不生效的原因是整个输入钩子不可用，而不是 X11 不拦截中键）
    pub wayland: bool,
}

impl Desired {
    /// 由设置算出（纯函数）
    pub fn from_settings(os: OsKind, s: &HotkeySettings) -> Self {
        let double_tap = s.double_tap.enabled.then(|| {
            let ms = if DOUBLE_TAP_INTERVALS_MS.contains(&s.double_tap.interval_ms) {
                s.double_tap.interval_ms
            } else {
                300
            };
            DoubleTapSpec {
                key: s.double_tap.key.unwrap_or_else(|| default_double_tap_key(os)),
                interval: std::time::Duration::from_millis(u64::from(ms)),
            }
        });
        let pick = |b: &wisp_types::MouseBinding| if b.enabled { b.action } else { None };
        let mut middle_dropped = false;
        let mouse = s
            .mouse
            .enabled
            .then(|| {
                let mut middle = pick(&s.mouse.middle);
                if os == OsKind::Linux && middle.is_some() {
                    middle = None;
                    middle_dropped = true;
                }
                MouseSpec {
                    long_press: s.mouse.long_press,
                    middle,
                    back: pick(&s.mouse.back),
                    forward: pick(&s.mouse.forward),
                }
            })
            .filter(|m| m.middle.is_some() || m.back.is_some() || m.forward.is_some());
        Self {
            bobber: s.bobber.enabled,
            double_tap,
            mouse,
            middle_dropped,
            wayland: false,
        }
    }

    /// 标明会话类型（`crate::app::runtime::is_wayland_session()`）
    #[must_use]
    pub fn in_session(mut self, wayland: bool) -> Self {
        self.wayland = wayland;
        self
    }

    /// 中键被去掉时如实报告的原因：Wayland 下输入钩子整体不可用，X11 下是不能拦截中键
    fn middle_note(&self) -> Option<&'static str> {
        self.middle_dropped.then_some(if self.wayland {
            wisp_platform::reason::WAYLAND_INPUT_HOOKS
        } else {
            wisp_platform::reason::X11_MIDDLE_BUTTON
        })
    }

    /// 交给平台层的完整规格
    pub fn spec(&self) -> HookSpec {
        HookSpec {
            bobber: self.bobber,
            double_tap: self.double_tap,
            mouse: self.mouse,
        }
    }

    /// 是否什么都不需要（此时不装任何钩子）
    pub fn is_empty(&self) -> bool {
        self.spec().is_empty() && !self.middle_dropped
    }
}

/// 与安装结果相关的权限（只有 macOS 有意义；其他平台恒为 NotApplicable）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermKey {
    pub accessibility: PermissionState,
    pub input_monitoring: PermissionState,
}

impl From<PermissionSet> for PermKey {
    fn from(p: PermissionSet) -> Self {
        Self {
            accessibility: p.accessibility,
            input_monitoring: p.input_monitoring,
        }
    }
}

impl PermKey {
    #[cfg(any(test, feature = "mouse-hook"))]
    fn get(self, kind: PermissionKind) -> PermissionState {
        match kind {
            PermissionKind::Accessibility => self.accessibility,
            PermissionKind::InputMonitoring => self.input_monitoring,
            PermissionKind::ScreenRecording => PermissionState::NotApplicable,
        }
    }
}

/// 三项扩展触发
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    Bobber,
    DoubleTap,
    Mouse,
}

const ITEMS: [Item; 3] = [Item::Bobber, Item::DoubleTap, Item::Mouse];

fn has(spec: &HookSpec, item: Item) -> bool {
    match item {
        Item::Bobber => spec.bobber,
        Item::DoubleTap => spec.double_tap.is_some(),
        Item::Mouse => spec.mouse.is_some(),
    }
}

fn remove(spec: &mut HookSpec, item: Item) {
    match item {
        Item::Bobber => spec.bobber = false,
        Item::DoubleTap => spec.double_tap = None,
        Item::Mouse => spec.mouse = None,
    }
}

/// 把 `from` 里的一项（连同参数）加进 `to`
fn add(to: &mut HookSpec, from: &HookSpec, item: Item) {
    match item {
        Item::Bobber => to.bobber = from.bobber,
        Item::DoubleTap => to.double_tap = from.double_tap,
        Item::Mouse => to.mouse = from.mouse,
    }
}

/// `inner` 的每一项（连同参数）都在 `outer` 里：旧钩子做的事仍是用户想要的
fn covers(outer: &HookSpec, inner: &HookSpec) -> bool {
    (!inner.bobber || outer.bobber)
        && (inner.double_tap.is_none() || outer.double_tap == inner.double_tap)
        && (inner.mouse.is_none() || outer.mouse == inner.mouse)
}

fn idx(item: Item) -> usize {
    match item {
        Item::Bobber => 0,
        Item::DoubleTap => 1,
        Item::Mouse => 2,
    }
}

/// 某项需要的 macOS 权限（浮标、鼠标键要辅助功能，双击修饰键要输入监控）
fn needs(item: Item) -> PermissionKind {
    match item {
        Item::Bobber | Item::Mouse => PermissionKind::Accessibility,
        Item::DoubleTap => PermissionKind::InputMonitoring,
    }
}

fn need_reason(kind: PermissionKind) -> &'static str {
    match kind {
        PermissionKind::InputMonitoring => reason::NEED_INPUT_MONITORING,
        PermissionKind::Accessibility | PermissionKind::ScreenRecording => reason::NEED_ACCESSIBILITY,
    }
}

/// 只与鼠标键有关的平台原因（X11）：去掉鼠标项后其余照常安装
fn mouse_only_reason(key: &str) -> bool {
    key == wisp_platform::reason::X11_MIDDLE_BUTTON || key == wisp_platform::reason::X11_MOUSE_BUTTON_BUSY
}

/// 一次安装的结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// 安装之后生效中的规格：新装上的，或保留下来的旧钩子的规格（空规格表示没有任何钩子）
    pub installed: HookSpec,
    pub status: ExtTriggerStatus,
    /// 每项（浮标、双击、鼠标）不正常时的原因
    pub reasons: [Option<String>; 3],
    /// 遇到过 `HookError::Other`（多半是暂时的：tap 线程启动超时等）且有想要的项没装上：值得稍后自动重试一次
    pub retry: bool,
}

/// 逐项拆分安装的时间预算：从第一次尝试算起超过这个时长，就不再开始新的尝试（剩下的项标为不可用，交给延迟重试）。
/// 略长于 macOS tap 线程的启动等待（2 s）：线程启动超时这类与具体哪一项无关的失败最多再试一次，ext 锁最多占约 4 s；
/// CGEventTapCreate 这类立即返回的失败不受影响，照常逐项试完
#[cfg(any(test, feature = "mouse-hook"))]
const SPLIT_BUDGET: std::time::Duration = std::time::Duration::from_millis(2500);

/// 这一次尝试是否还在预算之内：第一次尝试（整体安装）总是进行；之后从第一次尝试算起不超过 [`SPLIT_BUDGET`]
#[cfg(any(test, feature = "mouse-hook"))]
fn within_budget(first_try: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    first_try.is_none_or(|t| now.saturating_duration_since(t) < SPLIT_BUDGET)
}

/// 按项拆分的安装过程（纯逻辑；`install` 真正安装并在成功时保存 Handle）。`old` 是当前仍在工作的钩子的规格（没有
/// 钩子时为空规格）：
/// - 先整体安装；`PermissionMissing` 去掉缺该权限的项、X11 只与鼠标有关的 `Unsupported` 去掉鼠标项后重试（每次失败
///   至少去掉一项，最多 4 次）；其余 `Unsupported` 整体不可用；
/// - `Other`（与具体哪一项无关的失败，例如 tap 线程启动超时）时按项逐个累加安装（[`install_each`]），只把装不上的
///   项标为不可用；之后由调用方安排一次延迟重试（[`Outcome::retry`]）
pub fn plan_install(
    desired: &Desired,
    old: &HookSpec,
    mut install: impl FnMut(HookSpec) -> Result<(), HookError>,
) -> Outcome {
    let mut spec = desired.spec();
    let mut states = [ExtTriggerState::Off; 3];
    let mut reasons: [Option<String>; 3] = [None, None, None];
    for item in ITEMS {
        if has(&spec, item) {
            states[idx(item)] = ExtTriggerState::Active;
        }
    }
    if spec.mouse.is_none()
        && let Some(note) = desired.middle_note()
    {
        states[2] = ExtTriggerState::Unavailable;
        reasons[2] = Some(note.to_owned());
    }
    let mut installed = HookSpec::default();
    let mut transient = false;
    // 每次失败至少去掉一项（或转入逐项安装后结束），循环至多 4 轮
    while !spec.is_empty() {
        match install(spec) {
            Ok(()) => {
                installed = spec;
                break;
            }
            Err(HookError::PermissionMissing(kind)) => drop_missing(&mut spec, kind, &mut states, &mut reasons),
            Err(HookError::Unsupported(key)) => drop_unsupported(&mut spec, key, &mut states, &mut reasons),
            Err(HookError::Other(msg)) => {
                log::warn!("ext triggers: install failed: {msg}");
                transient = true;
                installed = install_each(spec, old, &mut install, &mut states, &mut reasons);
                break;
            }
        }
    }
    Outcome {
        installed,
        status: status_of(states, &reasons, desired.middle_note()),
        retry: transient && installed != desired.spec(),
        reasons,
    }
}

/// 整体安装缺权限：缺该权限的项进入等待授权、从规格里去掉；规格里没有需要该权限的项时（平台层的归因与这里的
/// 映射不一致）整体不可用
fn drop_missing(
    spec: &mut HookSpec,
    kind: PermissionKind,
    states: &mut [ExtTriggerState; 3],
    reasons: &mut [Option<String>; 3],
) {
    let hit: Vec<Item> = ITEMS
        .into_iter()
        .filter(|i| has(spec, *i) && needs(*i) == kind)
        .collect();
    if hit.is_empty() {
        fail_all(spec, states, reasons, reason::UNAVAILABLE);
    }
    for item in hit {
        remove(spec, item);
        states[idx(item)] = ExtTriggerState::WaitingPermission;
        reasons[idx(item)] = Some(need_reason(kind).to_owned());
    }
}

/// 整体安装不受支持：X11 只与鼠标有关的原因只去掉鼠标项，其余整体不可用
fn drop_unsupported(
    spec: &mut HookSpec,
    key: String,
    states: &mut [ExtTriggerState; 3],
    reasons: &mut [Option<String>; 3],
) {
    if mouse_only_reason(&key) && spec.mouse.is_some() {
        remove(spec, Item::Mouse);
        states[2] = ExtTriggerState::Unavailable;
        reasons[2] = Some(key);
    } else {
        fail_all(spec, states, reasons, &key);
    }
}

/// 整体安装遇到 `Other` 之后按项逐个累加安装，返回安装之后生效中的规格（`install` 每次成功都会换成新的 Handle，
/// 最后一次成功的 Handle 就对应返回值；一次都没成功时返回旧规格或空规格，调用方据此保留或注销旧 Handle）：
/// - 起点：旧钩子做的每件事（连同参数）都还在 `spec` 里时就是旧规格——它已由旧 Handle 实现，不需要重装；每一步只在
///   它之外再加一项新增的项，装上的规格总是旧规格的超集，部分成功不会顶替掉仍在工作的旧项。
///   否则旧钩子里有用户已经关掉、改了参数或失去权限的项，旧 Handle 无论如何都要注销：从空规格起步，先试旧规格里
///   仍然想要的项（它们刚才还在工作），再试其余的；
/// - 与刚刚整体失败的规格完全相同的组合不再重复尝试（macOS 上一次失败可能要等满 tap 线程的 2 s 启动等待），装不上的
///   那一项按错误标记，其余继续
fn install_each(
    spec: HookSpec,
    old: &HookSpec,
    install: &mut impl FnMut(HookSpec) -> Result<(), HookError>,
    states: &mut [ExtTriggerState; 3],
    reasons: &mut [Option<String>; 3],
) -> HookSpec {
    let base = if covers(&spec, old) { *old } else { HookSpec::default() };
    let mut acc = base;
    let order = ITEMS
        .into_iter()
        .filter(|i| has(old, *i))
        .chain(ITEMS.into_iter().filter(|i| !has(old, *i)));
    for item in order.filter(|i| has(&spec, *i) && !has(&base, *i)) {
        let mut next = acc;
        add(&mut next, &spec, item);
        let result = if next == spec {
            Err(HookError::Other("not retried: same spec as the failed install".into()))
        } else {
            install(next)
        };
        let (state, why) = match result {
            Ok(()) => {
                acc = next;
                continue;
            }
            Err(HookError::PermissionMissing(kind)) => {
                (ExtTriggerState::WaitingPermission, need_reason(kind).to_owned())
            }
            Err(HookError::Unsupported(key)) => (ExtTriggerState::Unavailable, key),
            Err(HookError::Other(msg)) => {
                log::warn!("ext triggers: {item:?} install failed: {msg}");
                (ExtTriggerState::Unavailable, reason::UNAVAILABLE.to_owned())
            }
        };
        states[idx(item)] = state;
        reasons[idx(item)] = Some(why);
    }
    acc
}

/// 当前安装的钩子：Handle 与它实现的规格
#[cfg(any(test, feature = "mouse-hook"))]
pub(crate) struct Installed<H> {
    /// 只为持有：drop 时平台层注销钩子、结束钩子线程
    _handle: H,
    pub spec: HookSpec,
}

/// 用这次安装的结果更新钩子槽位（先装后卸）：
/// - 有新 Handle：替换（新的已经装好，旧的随后注销）；
/// - 没有新 Handle、而生效中的规格就是旧钩子的规格（[`install_each`] 一项都没加上）：保留旧 Handle；
/// - 其余（一项都没装上、旧钩子里有不再想要的项或失去了权限）：注销旧 Handle。
#[cfg(any(test, feature = "mouse-hook"))]
pub(crate) fn commit<H>(slot: &mut Option<Installed<H>>, fresh: Option<H>, outcome: &Outcome) {
    if let Some(handle) = fresh {
        *slot = Some(Installed {
            _handle: handle,
            spec: outcome.installed,
        });
        return;
    }
    let keep = !outcome.installed.is_empty() && slot.as_ref().is_some_and(|old| old.spec == outcome.installed);
    if keep {
        log::warn!("ext triggers: new items failed to install, keeping the working hooks");
    } else {
        *slot = None;
    }
}

/// 设置没变、只有权限变了时，是否需要重装（否则只记下新的权限、沿用上次的结果）。权限的缓存只是变化的探测器——
/// 每次安装时平台层都会自己查真实的权限——所以只在可能改变结果时重装：
/// - 上次生效的项：它需要的权限现在明确是未授权（被撤销）时重装，以便如实报告「等待授权」；变成已授权、或仍是
///   `NotApplicable`（启动时缓存还没建立的占位值，即「未知」）都不影响它；
/// - 上次在等待授权的项：它需要的权限现在已授权时重装；
/// - 上次不可用的项（多半是暂时性失败）：任何权限变化都是重试的时机。
#[cfg(any(test, feature = "mouse-hook"))]
pub(crate) fn perms_matter(last: &Outcome, spec: &HookSpec, perms: PermKey) -> bool {
    let states = [last.status.bobber, last.status.double_tap, last.status.mouse];
    ITEMS.into_iter().filter(|i| has(spec, *i)).any(|item| {
        let now = perms.get(needs(item));
        match states[idx(item)] {
            ExtTriggerState::Active => matches!(now, PermissionState::Denied | PermissionState::NotDetermined),
            ExtTriggerState::WaitingPermission => now == PermissionState::Granted,
            ExtTriggerState::Unavailable | ExtTriggerState::Off => true,
        }
    })
}

fn fail_all(spec: &mut HookSpec, states: &mut [ExtTriggerState; 3], reasons: &mut [Option<String>; 3], key: &str) {
    for item in ITEMS {
        if has(spec, item) {
            states[idx(item)] = ExtTriggerState::Unavailable;
            reasons[idx(item)] = Some(key.to_owned());
        }
    }
    *spec = HookSpec::default();
}

/// 合成状态：`reason_key` 取第一个（浮标 → 双击 → 鼠标）不正常项的原因；都正常而中键被去掉时说明中键不生效
fn status_of(
    states: [ExtTriggerState; 3],
    reasons: &[Option<String>; 3],
    middle_note: Option<&str>,
) -> ExtTriggerStatus {
    let first = ITEMS
        .into_iter()
        .find(|i| {
            matches!(
                states[idx(*i)],
                ExtTriggerState::WaitingPermission | ExtTriggerState::Unavailable
            )
        })
        .and_then(|i| reasons[idx(i)].clone());
    let reason_key = first.or_else(|| middle_note.map(str::to_owned));
    ExtTriggerStatus {
        bobber: states[0],
        double_tap: states[1],
        mouse: states[2],
        reason_key,
    }
}

/// 编译时未开启 `mouse-hook`：开启的项一律不可用
pub fn not_compiled(desired: &Desired) -> ExtTriggerStatus {
    let spec = desired.spec();
    let mut states = [ExtTriggerState::Off; 3];
    let mut reasons: [Option<String>; 3] = [None, None, None];
    for item in ITEMS {
        if has(&spec, item) || (item == Item::Mouse && desired.middle_dropped) {
            states[idx(item)] = ExtTriggerState::Unavailable;
            reasons[idx(item)] = Some(wisp_platform::reason::HOOKS_NOT_COMPILED.to_owned());
        }
    }
    status_of(states, &reasons, None)
}

/// 浮标探测的闸门：候选代数、浮标是否仍在规格里、派发器是否已关闭。派发任务、阻塞线程上
/// 的探测与浮标的点击回调共享它（全是原子量，不加锁）
#[cfg(any(test, feature = "mouse-hook"))]
#[derive(Debug, Default)]
pub(crate) struct BobberGate {
    /// 候选代数：每个候选 +1，取消探测（候选后的鼠标按下）也 +1；探测结束时代数没变才显示浮标
    seq: AtomicU64,
    /// 当前安装的规格里有浮标（关掉浮标后，进行中的探测不再显示、已显示的浮标点击不再触发）
    bobber_on: AtomicBool,
    /// 派发器已关闭（全部扩展触发关闭）
    closed: AtomicBool,
}

#[cfg(any(test, feature = "mouse-hook"))]
impl BobberGate {
    /// 新的候选：返回它的代数（此前尚未完成的探测随之作废）
    pub fn candidate(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// `BobberDismiss`：只有鼠标按下会取消尚未完成的 80 ms 延迟探测（三击取消双击的探测）；其余原因只是通知——浮标
    /// 已由平台层自己隐藏，重复、过期的通知一律无害（不维护任何会影响之后的候选或点击的状态）
    pub fn dismiss(&self, reason: DismissReason) {
        if reason == DismissReason::MouseDown {
            self.seq.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// 代数为 `seq` 的探测结束时是否仍应显示浮标
    pub fn live(&self, seq: u64) -> bool {
        self.seq.load(Ordering::Acquire) == seq && self.click_allowed()
    }

    /// 浮标被点击时是否触发划词翻译（浮标已关闭或派发器已关闭则不触发）
    pub fn click_allowed(&self) -> bool {
        self.bobber_on.load(Ordering::Acquire) && !self.closed.load(Ordering::Acquire)
    }

    /// 当前安装的规格里是否有浮标
    pub fn set_bobber(&self, on: bool) {
        self.bobber_on.store(on, Ordering::Release);
    }

    /// 派发器关闭
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
}

/// 一个钩子信号的去向
#[cfg(any(test, feature = "mouse-hook"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Route {
    /// 浮标候选：延迟探测，代数为此值
    Probe(u64),
    /// 派发一个动作
    Fire(HotkeyAction, crate::bus::TriggerOrigin),
    /// 只是通知（浮标消失）
    Nothing,
}

/// 钩子信号的派发判定（派发任务里按到达顺序调用）
#[cfg(any(test, feature = "mouse-hook"))]
pub(crate) fn route(gate: &BobberGate, sig: &InputSignal) -> Route {
    use crate::bus::TriggerOrigin;
    match sig {
        InputSignal::BobberCandidate { .. } => Route::Probe(gate.candidate()),
        InputSignal::BobberDismiss(reason) => {
            gate.dismiss(*reason);
            Route::Nothing
        }
        InputSignal::DoubleTap => Route::Fire(HotkeyAction::TranslateSelection, TriggerOrigin::DoubleTap),
        InputSignal::MouseAction { action, .. } => Route::Fire(*action, TriggerOrigin::MouseButton),
    }
}

#[cfg(feature = "mouse-hook")]
pub use runtime::Runtime;

/// 运行期：钩子 Handle、派发任务与浮标探测
#[cfg(feature = "mouse-hook")]
mod runtime {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use parking_lot::Mutex;
    use tauri::{Listener, Manager};
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
    use wisp_platform::common::gesture::{BOBBER_PROBE_DEADLINE, BOBBER_PROBE_DELAY};
    use wisp_platform::types::{HookError, HookHandle, HookSpec, InputSignal};
    use wisp_types::{AppearanceState, HotkeyAction, HotkeyTarget, MonitorId, Point, ResolvedTheme, event};

    use super::{
        BobberGate, Desired, Installed, Outcome, PermKey, Route, commit, perms_matter, plan_install, route,
        within_budget,
    };
    use crate::bus::TriggerOrigin;
    use crate::state::AppState;

    /// 选区探测至少留给平台层的时间（候选信号本身被推迟时，仍然给 AX / UIA 一个最短的预算）
    const MIN_PROBE_BUDGET: Duration = Duration::from_millis(15);
    /// 拆分安装的时间预算用完时 `install` 闭包返回的错误信息（不再调用平台层）
    const BUDGET_SPENT: &str = "split install budget spent";

    /// 浮标外观（明暗、减少动效）
    #[derive(Debug, Clone, Copy, Default)]
    struct Look {
        dark: bool,
        reduced_motion: bool,
    }

    /// 派发任务与探测共享的状态
    struct Shared {
        gate: BobberGate,
        look: Arc<Mutex<Look>>,
    }

    /// 钩子信号与到达时刻
    struct Signal {
        sig: InputSignal,
        at: Instant,
    }

    /// 派发器：通道发送端 + 共享状态；drop 时通道关闭、任务结束
    struct Dispatcher {
        tx: UnboundedSender<Signal>,
        shared: Arc<Shared>,
    }

    impl Drop for Dispatcher {
        fn drop(&mut self) {
            self.shared.gate.close();
        }
    }

    /// 扩展触发的运行期状态（由 HotkeyService 在阻塞线程上持锁操作）
    #[derive(Default)]
    pub struct Runtime {
        /// 当前安装的钩子与它实现的规格
        hooks: Option<Installed<HookHandle>>,
        dispatcher: Option<Dispatcher>,
        /// 上次应用时的输入（设置没变、权限的变化不影响结果时不重装，见 [`Runtime::reuse`]）
        applied: Option<(Desired, PermKey)>,
        last: Option<Outcome>,
        /// 已经为哪一组输入安排过延迟重试（每组输入只重试一次，之后等设置或权限变化）
        retried_for: Option<(Desired, PermKey)>,
        look: Arc<Mutex<Look>>,
        look_listener: bool,
    }

    impl Runtime {
        /// 不需要重装时返回上次的结果：设置没变，且权限没变或权限的变化不可能改变结果（[`perms_matter`]；例如启动时
        /// 权限缓存从占位值变成真实基线、而上次各项都已生效）。后一种情况记下新的权限，之后只与它比较
        pub fn reuse(&mut self, desired: &Desired, perms: PermKey) -> Option<Outcome> {
            let (Some((d, p)), Some(last)) = (&self.applied, &self.last) else {
                return None;
            };
            if d != desired || (*p != perms && perms_matter(last, &desired.spec(), perms)) {
                return None;
            }
            if *p != perms {
                log::debug!("ext triggers: permission change does not affect the installed hooks, not reinstalling");
                self.applied = Some((*desired, perms));
            }
            Some(last.clone())
        }

        /// 上次的结果需要延迟重试、且还没为这组输入安排过：记下并返回 true（调用方安排一次定时重试）。上次的结果不需要
        /// 重试（成功了）时清掉记录，之后同一组输入再失败仍会重试
        pub fn take_retry(&mut self) -> bool {
            if !self.last.as_ref().is_some_and(|o| o.retry) {
                self.retried_for = None;
                return false;
            }
            if self.applied.is_some() && self.retried_for != self.applied {
                self.retried_for = self.applied;
                true
            } else {
                false
            }
        }

        /// 延迟重试到点：上次的结果仍需要重试时，忘掉「已应用的输入」让下一次 apply 真正重装；否则返回 false
        pub fn prepare_retry(&mut self) -> bool {
            if self.last.as_ref().is_some_and(|o| o.retry) {
                self.applied = None;
                true
            } else {
                false
            }
        }

        /// 按期望安装（**阻塞**：`InputHooks::install` 要等钩子线程建好；在阻塞线程上调用）
        pub fn apply(&mut self, app: &tauri::AppHandle, desired: Desired, perms: PermKey) -> Outcome {
            let Some(state) = app.try_state::<AppState>() else {
                return plan_install(&Desired::default(), &HookSpec::default(), |_| Ok(()));
            };
            let platform = Arc::clone(&state.platform);
            if desired.spec().is_empty() {
                let had = self.hooks.is_some();
                self.drop_all();
                if had {
                    log::info!("ext triggers: all off, input hooks removed (no hook thread)");
                }
                let outcome = plan_install(&desired, &HookSpec::default(), |_| Ok(()));
                self.applied = Some((desired, perms));
                self.last = Some(outcome.clone());
                return outcome;
            }
            if desired.bobber {
                // 浮标的明暗与动效在安装之前就位（第一次候选就要用到）
                self.watch_look(app);
                *self.look.lock() = look_of(&crate::app::appearance::current(app));
            }
            let dispatcher = self
                .dispatcher
                .get_or_insert_with(|| Dispatcher::spawn(app, Arc::clone(&self.look)));
            let shared = Arc::clone(&dispatcher.shared);
            let tx = dispatcher.tx.clone();
            let mut fresh: Option<HookHandle> = None;
            let old = self.hooks.as_ref().map(|h| h.spec).unwrap_or_default();
            let mut first_try: Option<Instant> = None;
            let outcome = plan_install(&desired, &old, |spec| {
                if !within_budget(first_try, Instant::now()) {
                    return Err(HookError::Other(BUDGET_SPENT.into()));
                }
                first_try.get_or_insert_with(Instant::now);
                let tx = tx.clone();
                let sink = Box::new(move |sig: InputSignal| {
                    // 钩子线程上只做 O(1) 的投递；通道关闭（扩展触发已关闭）时丢弃
                    let _ = tx.send(Signal {
                        sig,
                        at: Instant::now(),
                    });
                });
                let started = Instant::now();
                let r = platform.hooks.install(spec, sink);
                log::debug!(
                    "ext triggers: install bobber={} double_tap={} mouse={} → {} in {} ms",
                    spec.bobber,
                    spec.double_tap.is_some(),
                    spec.mouse.is_some(),
                    if r.is_ok() { "ok" } else { "error" },
                    started.elapsed().as_millis()
                );
                // 逐项累加安装时，每次成功都换成新的 Handle（上一个随之注销）
                r.map(|h| fresh = Some(h))
            });
            // 先装新规格、成功后再丢旧 Handle；一项都没加上时保留仍在工作的旧钩子（见 commit）
            commit(&mut self.hooks, fresh, &outcome);
            shared.gate.set_bobber(outcome.installed.bobber);
            if self.hooks.is_none() {
                // 一项都没装上：不留派发任务
                self.dispatcher = None;
            }
            log::info!(
                "ext triggers: bobber={:?} double_tap={:?} mouse={:?} reason={:?}{}",
                outcome.status.bobber,
                outcome.status.double_tap,
                outcome.status.mouse,
                outcome.status.reason_key,
                if outcome.retry { " (will retry)" } else { "" }
            );
            self.applied = Some((desired, perms));
            self.last = Some(outcome.clone());
            outcome
        }

        /// 注销全部钩子、结束派发任务
        fn drop_all(&mut self) {
            self.hooks = None;
            self.dispatcher = None;
        }

        /// 跟随系统外观（只注册一次；监听本身没有开销）
        fn watch_look(&mut self, app: &tauri::AppHandle) {
            if self.look_listener {
                return;
            }
            self.look_listener = true;
            let look = Arc::clone(&self.look);
            app.listen_any(event::APPEARANCE_CHANGED, move |e| {
                if let Ok(a) = serde_json::from_str::<AppearanceState>(e.payload()) {
                    *look.lock() = look_of(&a);
                }
            });
        }
    }

    fn look_of(a: &AppearanceState) -> Look {
        Look {
            dark: a.theme == ResolvedTheme::Dark,
            reduced_motion: a.reduce_motion,
        }
    }

    impl Dispatcher {
        fn spawn(app: &tauri::AppHandle, look: Arc<Mutex<Look>>) -> Self {
            let (tx, rx) = unbounded_channel();
            let shared = Arc::new(Shared {
                gate: BobberGate::default(),
                look,
            });
            tauri::async_runtime::spawn(run(app.clone(), rx, Arc::clone(&shared)));
            Self { tx, shared }
        }
    }

    /// 派发循环：所有发送端（钩子的 sink 与 Dispatcher）都 drop 后结束
    async fn run(app: tauri::AppHandle, mut rx: UnboundedReceiver<Signal>, shared: Arc<Shared>) {
        log::debug!("ext triggers: dispatcher started");
        while let Some(Signal { sig, at }) = rx.recv().await {
            match route(&shared.gate, &sig) {
                Route::Probe(seq) => {
                    if let InputSignal::BobberCandidate { at: point } = sig {
                        tauri::async_runtime::spawn(probe(app.clone(), Arc::clone(&shared), seq, point, at));
                    }
                }
                Route::Fire(action, origin) => fire(&app, action, origin),
                Route::Nothing => log::trace!("bobber dismissed: {sig:?}"),
            }
        }
        log::debug!("ext triggers: dispatcher stopped");
    }

    /// 浮标探测：mouse-up 后 80 ms 只读一次选区（120 ms 内完成），非空则显示
    async fn probe(app: tauri::AppHandle, shared: Arc<Shared>, seq: u64, point: (MonitorId, Point), up: Instant) {
        tokio::time::sleep_until((up + BOBBER_PROBE_DELAY).into()).await;
        if !shared.gate.live(seq) {
            return;
        }
        let Some(state) = app.try_state::<AppState>() else {
            return;
        };
        let platform = Arc::clone(&state.platform);
        let click_app = app.clone();
        let click_shared = Arc::clone(&shared);
        let r = tauri::async_runtime::spawn_blocking(move || {
            let budget = BOBBER_PROBE_DEADLINE.saturating_sub(up.elapsed()).max(MIN_PROBE_BUDGET);
            let started = Instant::now();
            let selected = platform.selection.has_selection(budget);
            log::trace!(
                "bobber probe: {selected:?} in {} ms ({} ms after mouse-up)",
                started.elapsed().as_millis(),
                up.elapsed().as_millis()
            );
            if selected != Some(true) || !shared.gate.live(seq) {
                return;
            }
            let look = *shared.look.lock();
            platform.bobber.show(
                point,
                look.dark,
                look.reduced_motion,
                Box::new(move || {
                    // 点击时浮标已由平台层立即隐藏；功能已关闭则不再触发
                    if click_shared.gate.click_allowed() {
                        fire(&click_app, HotkeyAction::TranslateSelection, TriggerOrigin::Bobber);
                    }
                }),
            );
        })
        .await;
        if let Err(e) = r {
            log::warn!("bobber probe failed: {e}");
        }
    }

    /// 派发一个动作（经 HotkeyService 的去抖与录制 / 测试期间的抑制）
    fn fire(app: &tauri::AppHandle, action: HotkeyAction, origin: TriggerOrigin) {
        if let Some(state) = app.try_state::<AppState>() {
            state.hotkeys.fire(&HotkeyTarget::Action { action }, origin);
        }
    }

    #[cfg(test)]
    mod tests {
        use wisp_types::PermissionState::{Denied, Granted};
        use wisp_types::{HotkeySettings, OsKind, PermissionSet};

        use super::*;

        /// 启动时：第一次安装时权限缓存还是占位值（NA），基线一到就变成已授权。上次各项都已生效时只记下新的权限、
        /// 不重装；权限被撤销或设置变了才重装
        #[test]
        fn baseline_permissions_do_not_reinstall() {
            let mut s = HotkeySettings::default();
            s.bobber.enabled = true;
            s.double_tap.enabled = true;
            let d = Desired::from_settings(OsKind::Macos, &s);
            let na = PermKey::from(PermissionSet::not_applicable());
            let granted = PermKey {
                accessibility: Granted,
                input_monitoring: Granted,
            };
            let mut rt = Runtime::default();
            assert!(rt.reuse(&d, na).is_none(), "还没装过");
            rt.applied = Some((d, na));
            rt.last = Some(plan_install(&d, &HookSpec::default(), |_| Ok(())));
            assert_eq!(rt.reuse(&d, na), rt.last.clone(), "输入没变");
            assert_eq!(
                rt.reuse(&d, granted),
                rt.last.clone(),
                "NA → 已授权、上次全部生效：不重装"
            );
            assert_eq!(rt.applied, Some((d, granted)), "记下新的权限");
            let revoked = PermKey {
                accessibility: Denied,
                ..granted
            };
            assert!(rt.reuse(&d, revoked).is_none(), "辅助功能被撤销：重装");
            s.double_tap.interval_ms = 400;
            assert!(
                rt.reuse(&Desired::from_settings(OsKind::Macos, &s), granted).is_none(),
                "设置变了：重装"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use wisp_types::{HotkeyAction, ModifierKey, MouseBinding};

    use super::*;

    fn all_on() -> HotkeySettings {
        let mut s = HotkeySettings::default();
        s.bobber.enabled = true;
        s.double_tap.enabled = true;
        s.mouse.enabled = true;
        s.mouse.back.enabled = true;
        s
    }

    /// 默认全部关闭 → 空规格（不装任何钩子）
    #[test]
    fn defaults_install_nothing() {
        for os in [OsKind::Macos, OsKind::Windows, OsKind::Linux] {
            let d = Desired::from_settings(os, &HotkeySettings::default());
            assert!(d.is_empty() && d.spec().is_empty(), "{os:?}");
            let mut calls = 0;
            let o = plan_install(&d, &HookSpec::default(), |_| {
                calls += 1;
                Ok(())
            });
            assert_eq!(calls, 0, "全关时不调用 install");
            assert_eq!(o.status, ExtTriggerStatus::default());
        }
    }

    /// 规格：双击默认键按平台、间隔只取 200 / 300 / 400；鼠标键只交出勾选了的键；侧键默认不启用
    #[test]
    fn spec_from_settings() {
        let d = Desired::from_settings(OsKind::Macos, &all_on());
        let dt = d.double_tap.expect("双击");
        assert_eq!(dt.key, ModifierKey::RightAlt);
        assert_eq!(dt.interval, Duration::from_millis(300));
        let m = d.mouse.expect("鼠标");
        assert_eq!(m.middle, Some(HotkeyAction::TranslateSelection));
        assert_eq!(m.back, Some(HotkeyAction::Clipboard));
        assert_eq!(m.forward, None, "前进键默认没勾选");
        assert!(d.bobber);
        assert_eq!(
            Desired::from_settings(OsKind::Windows, &all_on())
                .double_tap
                .map(|d| d.key),
            Some(ModifierKey::RightCtrl)
        );
        let mut s = all_on();
        s.double_tap.interval_ms = 250;
        s.double_tap.key = Some(ModifierKey::LeftCmd);
        let dt = Desired::from_settings(OsKind::Macos, &s).double_tap.expect("双击");
        assert_eq!(dt.interval, Duration::from_millis(300));
        assert_eq!(dt.key, ModifierKey::LeftCmd);
        // 鼠标总开关打开、一个键都没勾选：等于没有鼠标项
        let mut s = HotkeySettings::default();
        s.mouse.enabled = true;
        s.mouse.middle = MouseBinding {
            enabled: false,
            action: Some(HotkeyAction::TranslateSelection),
        };
        assert!(Desired::from_settings(OsKind::Windows, &s).is_empty());
    }

    /// macOS 缺输入监控时只有双击进入「等待授权」，浮标与鼠标键照常安装
    #[test]
    fn splits_by_permission() {
        let d = Desired::from_settings(OsKind::Macos, &all_on());
        let mut tried = Vec::new();
        let o = plan_install(&d, &HookSpec::default(), |spec| {
            tried.push(spec);
            if spec.double_tap.is_some() {
                Err(HookError::PermissionMissing(PermissionKind::InputMonitoring))
            } else {
                Ok(())
            }
        });
        assert_eq!(tried.len(), 2);
        assert!(o.installed.bobber && o.installed.mouse.is_some() && o.installed.double_tap.is_none());
        assert_eq!(o.status.bobber, ExtTriggerState::Active);
        assert_eq!(o.status.mouse, ExtTriggerState::Active);
        assert_eq!(o.status.double_tap, ExtTriggerState::WaitingPermission);
        assert_eq!(o.status.reason_key.as_deref(), Some(reason::NEED_INPUT_MONITORING));

        // 两种权限都缺：全部等待授权，最后不装任何钩子
        let o = plan_install(&d, &HookSpec::default(), |spec| {
            if spec.bobber || spec.mouse.is_some() {
                Err(HookError::PermissionMissing(PermissionKind::Accessibility))
            } else {
                Err(HookError::PermissionMissing(PermissionKind::InputMonitoring))
            }
        });
        assert!(o.installed.is_empty());
        assert_eq!(o.status.bobber, ExtTriggerState::WaitingPermission);
        assert_eq!(o.status.double_tap, ExtTriggerState::WaitingPermission);
        assert_eq!(o.status.mouse, ExtTriggerState::WaitingPermission);
        assert_eq!(o.status.reason_key.as_deref(), Some(reason::NEED_ACCESSIBILITY));
    }

    /// Wayland / 缺 XInput：整体不可用并带平台原因；X11 侧键被占用：只有鼠标项不可用
    #[test]
    fn unsupported_environments() {
        let d = Desired::from_settings(OsKind::Linux, &all_on());
        let o = plan_install(&d, &HookSpec::default(), |_| {
            Err(HookError::Unsupported(
                wisp_platform::reason::WAYLAND_INPUT_HOOKS.to_owned(),
            ))
        });
        assert!(o.installed.is_empty());
        assert_eq!(o.status.bobber, ExtTriggerState::Unavailable);
        assert_eq!(o.status.mouse, ExtTriggerState::Unavailable);
        assert_eq!(
            o.status.reason_key.as_deref(),
            Some(wisp_platform::reason::WAYLAND_INPUT_HOOKS)
        );

        let o = plan_install(&d, &HookSpec::default(), |spec| {
            if spec.mouse.is_some() {
                Err(HookError::Unsupported(
                    wisp_platform::reason::X11_MOUSE_BUTTON_BUSY.to_owned(),
                ))
            } else {
                Ok(())
            }
        });
        assert!(o.installed.bobber && o.installed.mouse.is_none());
        assert_eq!(o.status.mouse, ExtTriggerState::Unavailable);
        assert_eq!(o.status.bobber, ExtTriggerState::Active);
        assert_eq!(
            o.status.reason_key.as_deref(),
            Some(wisp_platform::reason::X11_MOUSE_BUTTON_BUSY)
        );

        let o = plan_install(&d, &HookSpec::default(), |_| Err(HookError::Other("tap failed".into())));
        assert!(o.installed.is_empty());
        assert_eq!(o.status.double_tap, ExtTriggerState::Unavailable);
        assert_eq!(o.status.reason_key.as_deref(), Some(reason::UNAVAILABLE));
    }

    /// X11 不拦截中键：只绑中键时鼠标项不可用并说明原因，与侧键一起时侧键照常、中键说明不生效
    #[test]
    fn linux_middle_button() {
        let mut s = HotkeySettings::default();
        s.mouse.enabled = true;
        let d = Desired::from_settings(OsKind::Linux, &s);
        assert!(d.middle_dropped && d.mouse.is_none() && !d.is_empty());
        let mut calls = 0;
        let o = plan_install(&d, &HookSpec::default(), |_| {
            calls += 1;
            Ok(())
        });
        assert_eq!(calls, 0);
        assert_eq!(o.status.mouse, ExtTriggerState::Unavailable);
        assert_eq!(
            o.status.reason_key.as_deref(),
            Some(wisp_platform::reason::X11_MIDDLE_BUTTON)
        );

        s.mouse.forward.enabled = true;
        let d = Desired::from_settings(OsKind::Linux, &s);
        let m = d.mouse.expect("侧键");
        assert_eq!((m.middle, m.forward), (None, Some(HotkeyAction::Ocr)));
        let o = plan_install(&d, &HookSpec::default(), |_| Ok(()));
        assert_eq!(o.status.mouse, ExtTriggerState::Active);
        assert_eq!(
            o.status.reason_key.as_deref(),
            Some(wisp_platform::reason::X11_MIDDLE_BUTTON)
        );
        // macOS / Windows 中键照常交给平台层
        assert!(
            Desired::from_settings(OsKind::Windows, &s)
                .mouse
                .is_some_and(|m| m.middle.is_some())
        );
    }

    /// Wayland 会话里只绑了中键：原因是输入钩子整体不可用，不是「X11 不能拦截中键」
    #[test]
    fn linux_middle_button_on_wayland() {
        let mut s = HotkeySettings::default();
        s.mouse.enabled = true;
        let d = Desired::from_settings(OsKind::Linux, &s).in_session(true);
        let o = plan_install(&d, &HookSpec::default(), |_| panic!("规格为空时不调用 install"));
        assert_eq!(o.status.mouse, ExtTriggerState::Unavailable);
        assert_eq!(
            o.status.reason_key.as_deref(),
            Some(wisp_platform::reason::WAYLAND_INPUT_HOOKS)
        );
        let x11 = plan_install(
            &Desired::from_settings(OsKind::Linux, &s).in_session(false),
            &HookSpec::default(),
            |_| Ok(()),
        );
        assert_eq!(
            x11.status.reason_key.as_deref(),
            Some(wisp_platform::reason::X11_MIDDLE_BUTTON)
        );
    }

    /// `HookError::Other`：按项逐个累加安装，只把装不上的那一项标为不可用，并要求稍后重试
    #[test]
    fn other_errors_split_per_item() {
        let d = Desired::from_settings(OsKind::Macos, &all_on());
        let mut tried = Vec::new();
        let o = plan_install(&d, &HookSpec::default(), |spec| {
            tried.push(spec);
            if spec.double_tap.is_some() {
                Err(HookError::Other("tap thread did not start".into()))
            } else {
                Ok(())
            }
        });
        // 整体 → 浮标 → 浮标 + 双击（失败）→ 浮标 + 鼠标
        assert_eq!(tried.len(), 4);
        assert!(tried[1].bobber && tried[1].double_tap.is_none() && tried[1].mouse.is_none());
        assert!(o.installed.bobber && o.installed.mouse.is_some() && o.installed.double_tap.is_none());
        assert_eq!(o.status.bobber, ExtTriggerState::Active);
        assert_eq!(o.status.double_tap, ExtTriggerState::Unavailable);
        assert_eq!(o.status.mouse, ExtTriggerState::Active);
        assert_eq!(o.status.reason_key.as_deref(), Some(reason::UNAVAILABLE));
        assert!(o.retry);

        // 拆分时某项缺权限：照常进入等待授权
        let o = plan_install(&d, &HookSpec::default(), |spec| {
            match (spec.bobber, spec.double_tap, spec.mouse) {
                (true, Some(_), Some(_)) => Err(HookError::Other("boom".into())),
                (_, Some(_), _) => Err(HookError::PermissionMissing(PermissionKind::InputMonitoring)),
                _ => Ok(()),
            }
        });
        assert_eq!(o.status.double_tap, ExtTriggerState::WaitingPermission);
        assert_eq!(o.status.bobber, ExtTriggerState::Active);

        // 只有一项：不重复尝试同一个规格，直接报告不可用并要求稍后重试
        let mut s = HotkeySettings::default();
        s.bobber.enabled = true;
        let mut calls = 0;
        let o = plan_install(&Desired::from_settings(OsKind::Macos, &s), &HookSpec::default(), |_| {
            calls += 1;
            Err(HookError::Other("boom".into()))
        });
        assert_eq!(calls, 1);
        assert!(o.installed.is_empty() && o.retry);

        // 全部成功、或失败原因不是暂时性的：不重试
        assert!(!plan_install(&d, &HookSpec::default(), |_| Ok(())).retry);
        assert!(
            !plan_install(&d, &HookSpec::default(), |_| Err(HookError::PermissionMissing(
                PermissionKind::Accessibility
            )))
            .retry
        );
    }

    /// 记录 drop 顺序的假 Handle（名字是它实现的规格，例如 `b+m`）
    struct FakeHandle(String, Drops);

    type Drops = std::rc::Rc<std::cell::RefCell<Vec<String>>>;

    impl Drop for FakeHandle {
        fn drop(&mut self) {
            self.1.borrow_mut().push(self.0.clone());
        }
    }

    fn label(spec: &HookSpec) -> String {
        let mut parts = Vec::new();
        if spec.bobber {
            parts.push("b");
        }
        if spec.double_tap.is_some() {
            parts.push("d");
        }
        if spec.mouse.is_some() {
            parts.push("m");
        }
        parts.join("+")
    }

    /// 与 `Runtime::apply` 相同的接线：旧规格取自槽位；每次安装成功都换成新的 Handle（上一个随之 drop）；最后 commit
    fn apply_fake(
        slot: &mut Option<Installed<FakeHandle>>,
        desired: &Desired,
        drops: &Drops,
        tried: &mut Vec<HookSpec>,
        mut install: impl FnMut(&HookSpec) -> Result<(), HookError>,
    ) -> Outcome {
        let old = slot.as_ref().map(|i| i.spec).unwrap_or_default();
        let mut fresh = None;
        let outcome = plan_install(desired, &old, |spec| {
            tried.push(spec);
            install(&spec).map(|()| fresh = Some(FakeHandle(label(&spec), drops.clone())))
        });
        commit(slot, fresh, &outcome);
        outcome
    }

    fn settings_with(bobber: bool, double_tap: bool, mouse: bool) -> HotkeySettings {
        let mut s = HotkeySettings::default();
        s.bobber.enabled = bobber;
        s.double_tap.enabled = double_tap;
        s.mouse.enabled = mouse;
        s
    }

    fn other() -> HookError {
        HookError::Other("tap thread did not start".into())
    }

    /// 新增的项装不上（`Other`）时旧 Handle 不被 drop，旧规格里的项照常报告为生效；与刚刚整体失败的规格
    /// 相同的组合不重复尝试
    #[test]
    fn failed_install_keeps_working_hooks() {
        let drops = Drops::default();
        let mut slot = None;
        let mut tried = Vec::new();
        let old_desired = Desired::from_settings(OsKind::Macos, &settings_with(true, false, false));
        let first = apply_fake(&mut slot, &old_desired, &drops, &mut tried, |_| Ok(()));
        assert_eq!(first.status.bobber, ExtTriggerState::Active);

        // 用户再打开鼠标键，这次安装失败：旧规格 + 鼠标就是整体规格，不再重复尝试
        tried.clear();
        let new_desired = Desired::from_settings(OsKind::Macos, &settings_with(true, false, true));
        let kept = apply_fake(&mut slot, &new_desired, &drops, &mut tried, |_| Err(other()));
        assert_eq!(tried, [new_desired.spec()], "只有整体安装那一次");
        assert!(drops.borrow().is_empty(), "旧 Handle 不能被 drop");
        assert!(slot.as_ref().is_some_and(|i| i.spec == old_desired.spec()));
        assert_eq!(kept.installed, old_desired.spec());
        assert_eq!(kept.status.bobber, ExtTriggerState::Active, "浮标照常工作");
        assert_eq!(kept.status.mouse, ExtTriggerState::Unavailable);
        assert_eq!(kept.status.reason_key.as_deref(), Some(reason::UNAVAILABLE));
        assert!(kept.retry);

        // 之后重试成功：新 Handle 装好之后旧的才注销
        let ok = apply_fake(&mut slot, &new_desired, &drops, &mut tried, |_| Ok(()));
        assert!(!ok.retry);
        assert_eq!(*drops.borrow(), ["b"]);
        assert!(slot.as_ref().is_some_and(|i| i.spec == new_desired.spec()));

        // 同一规格重装（例如权限变化）遇到 `Other`：旧钩子就是想要的全部，保留它、全部生效、不需要重试
        drops.borrow_mut().clear();
        let same = apply_fake(&mut slot, &new_desired, &drops, &mut tried, |_| Err(other()));
        assert!(drops.borrow().is_empty());
        assert_eq!(same.installed, new_desired.spec());
        assert_eq!(same.status.bobber, ExtTriggerState::Active);
        assert_eq!(same.status.mouse, ExtTriggerState::Active);
        assert!(!same.retry);
    }

    /// 旧 {浮标, 鼠标}，再打开双击；整体失败（`Other`）后拆分时含鼠标的新组合也失败。拆分只在旧规格之上
    /// 累加，绝不会先装一个比旧规格小的规格（例如只有浮标）把仍在工作的鼠标键顶掉
    #[test]
    fn other_split_never_displaces_working_hooks() {
        let drops = Drops::default();
        let mut slot = None;
        let mut tried = Vec::new();
        let old_desired = Desired::from_settings(OsKind::Macos, &settings_with(true, false, true));
        let old = old_desired.spec();
        apply_fake(&mut slot, &old_desired, &drops, &mut tried, |_| Ok(()));

        tried.clear();
        let new_desired = Desired::from_settings(OsKind::Macos, &settings_with(true, true, true));
        let o = apply_fake(&mut slot, &new_desired, &drops, &mut tried, |spec| {
            // 含双击的一律失败；含鼠标的、除旧规格本身以外的组合也失败
            if spec.double_tap.is_some() || (spec.mouse.is_some() && *spec != old) {
                Err(other())
            } else {
                Ok(())
            }
        });
        assert!(
            tried.iter().all(|t| covers(t, &old)),
            "每次尝试都是旧规格的超集：{tried:?}"
        );
        assert!(drops.borrow().is_empty(), "旧 Handle 没有被 drop");
        assert!(slot.as_ref().is_some_and(|i| i.spec == old));
        assert_eq!(o.installed, old);
        assert_eq!(o.status.bobber, ExtTriggerState::Active);
        assert_eq!(o.status.mouse, ExtTriggerState::Active, "鼠标键照常工作");
        assert_eq!(o.status.double_tap, ExtTriggerState::Unavailable);
        assert!(o.retry);

        // 旧 {浮标}，一次打开双击与鼠标：在旧规格之上逐个加，双击失败、鼠标成功 → 换成 {浮标, 鼠标}，之后才注销旧的
        let drops = Drops::default();
        let mut slot = None;
        let only_b = Desired::from_settings(OsKind::Macos, &settings_with(true, false, false));
        apply_fake(&mut slot, &only_b, &drops, &mut tried, |_| Ok(()));
        tried.clear();
        let o = apply_fake(&mut slot, &new_desired, &drops, &mut tried, |spec| {
            if spec.double_tap.is_some() {
                Err(other())
            } else {
                Ok(())
            }
        });
        assert_eq!(tried.iter().map(label).collect::<Vec<_>>(), ["b+d+m", "b+d", "b+m"]);
        assert_eq!(*drops.borrow(), ["b"]);
        assert!(slot.as_ref().is_some_and(|i| label(&i.spec) == "b+m"));
        assert_eq!(o.status.mouse, ExtTriggerState::Active);
        assert_eq!(o.status.double_tap, ExtTriggerState::Unavailable);
        assert!(o.retry);
    }

    /// 旧钩子里有用户已经关掉的项（关掉鼠标键）、改了参数的项、或失败原因是缺权限时，不保留旧 Handle
    #[test]
    fn failed_install_drops_unwanted_hooks() {
        let drops = Drops::default();
        let mut slot = None;
        let mut tried = Vec::new();
        let old_desired = Desired::from_settings(OsKind::Macos, &settings_with(true, false, true));
        apply_fake(&mut slot, &old_desired, &drops, &mut tried, |_| Ok(()));
        // 关掉鼠标键、打开双击，这次安装全部失败：旧钩子会继续吞中键，必须注销。拆分从空规格起步、先试旧规格里仍想要的
        // 浮标
        tried.clear();
        let new_desired = Desired::from_settings(OsKind::Macos, &settings_with(true, true, false));
        let o = apply_fake(&mut slot, &new_desired, &drops, &mut tried, |_| Err(other()));
        assert_eq!(tried.iter().map(label).collect::<Vec<_>>(), ["b+d", "b", "d"]);
        assert_eq!(*drops.borrow(), ["b+m"]);
        assert!(slot.is_none());
        assert!(o.installed.is_empty());
        assert_eq!(o.status.bobber, ExtTriggerState::Unavailable);

        // 双击间隔从 300 改成 400、整体安装失败：旧钩子的双击参数已经不是用户想要的，换成只有浮标的新钩子
        drops.borrow_mut().clear();
        let mut slot = None;
        let mut s = settings_with(true, true, false);
        apply_fake(
            &mut slot,
            &Desired::from_settings(OsKind::Macos, &s),
            &drops,
            &mut tried,
            |_| Ok(()),
        );
        s.double_tap.interval_ms = 400;
        let changed = Desired::from_settings(OsKind::Macos, &s);
        tried.clear();
        let o = apply_fake(&mut slot, &changed, &drops, &mut tried, |spec| {
            if spec.double_tap.is_some() {
                Err(other())
            } else {
                Ok(())
            }
        });
        assert_eq!(tried.iter().map(label).collect::<Vec<_>>(), ["b+d", "b"]);
        assert_eq!(*drops.borrow(), ["b+d"]);
        assert!(slot.as_ref().is_some_and(|i| label(&i.spec) == "b"));
        assert_eq!(o.status.double_tap, ExtTriggerState::Unavailable);

        // 授权被撤销：旧钩子已不工作，不能报告为生效
        drops.borrow_mut().clear();
        let d = Desired::from_settings(OsKind::Macos, &settings_with(true, false, false));
        let mut slot = None;
        apply_fake(&mut slot, &d, &drops, &mut tried, |_| Ok(()));
        let o = apply_fake(&mut slot, &d, &drops, &mut tried, |_| {
            Err(HookError::PermissionMissing(PermissionKind::Accessibility))
        });
        assert_eq!(*drops.borrow(), ["b"]);
        assert!(slot.is_none());
        assert_eq!(o.status.bobber, ExtTriggerState::WaitingPermission);
    }

    /// 逐项拆分的时间预算：整体安装总是进行；之后从第一次尝试算起超过预算就不再开始新的尝试
    #[test]
    fn split_budget() {
        let t0 = std::time::Instant::now();
        assert!(within_budget(None, t0));
        assert!(within_budget(Some(t0), t0 + SPLIT_BUDGET - Duration::from_millis(1)));
        assert!(!within_budget(Some(t0), t0 + SPLIT_BUDGET));
    }

    fn perms(accessibility: PermissionState, input_monitoring: PermissionState) -> PermKey {
        PermKey {
            accessibility,
            input_monitoring,
        }
    }

    /// 权限变化只在可能改变结果时才重装（启动时缓存从占位值变成真实基线，不把同一规格再装一遍）
    #[test]
    fn perms_change_reinstalls_only_when_it_matters() {
        use PermissionState::{Denied, Granted, NotApplicable as Na, NotDetermined};
        let d = Desired::from_settings(OsKind::Macos, &all_on());
        let spec = d.spec();
        let all_active = plan_install(&d, &HookSpec::default(), |_| Ok(()));
        // NA → 已授权、上次全部生效：不重装
        assert!(!perms_matter(&all_active, &spec, perms(Granted, Granted)));
        assert!(!perms_matter(&all_active, &spec, perms(Granted, Na)));
        // 生效中的项需要的权限被撤销：重装（以便如实报告等待授权）
        assert!(perms_matter(&all_active, &spec, perms(Denied, Granted)));
        assert!(perms_matter(&all_active, &spec, perms(Granted, NotDetermined)));
        // 没开双击时，输入监控怎么变都无关
        let no_dt = Desired::from_settings(OsKind::Macos, &settings_with(true, false, true));
        let o = plan_install(&no_dt, &HookSpec::default(), |_| Ok(()));
        assert!(!perms_matter(&o, &no_dt.spec(), perms(Granted, Denied)));

        // 双击在等待输入监控：仍未授权时不重装（否则启动时每次都把浮标 / 鼠标再装一遍），授权后重装
        let waiting = plan_install(&d, &HookSpec::default(), |spec| {
            if spec.double_tap.is_some() {
                Err(HookError::PermissionMissing(PermissionKind::InputMonitoring))
            } else {
                Ok(())
            }
        });
        assert_eq!(waiting.status.double_tap, ExtTriggerState::WaitingPermission);
        assert!(!perms_matter(&waiting, &spec, perms(Granted, Denied)));
        assert!(!perms_matter(&waiting, &spec, perms(Granted, Na)));
        assert!(perms_matter(&waiting, &spec, perms(Granted, Granted)));

        // 上次有项不可用（暂时性失败）：任何权限变化都是重试的时机
        let failed = plan_install(&d, &HookSpec::default(), |_| Err(other()));
        assert!(perms_matter(&failed, &spec, perms(Granted, Granted)));
    }

    /// 浮标闸门：候选后的鼠标按下取消探测；重复 / 过期的其他 Dismiss 无害；关掉浮标或派发器后不显示、不触发
    #[test]
    fn bobber_gate_sequences() {
        use wisp_platform::types::{DismissReason, InputSignal};
        use wisp_types::{MonitorId, Point};

        use crate::bus::TriggerOrigin;

        let gate = BobberGate::default();
        gate.set_bobber(true);
        let cand = || InputSignal::BobberCandidate {
            at: (MonitorId::new(), Point::default()),
        };
        // 正常：候选 → 探测仍有效
        let Route::Probe(a) = route(&gate, &cand()) else {
            panic!("候选要探测")
        };
        assert!(gate.live(a));
        // 双击后紧接着第三击：第二个候选让第一个的探测作废
        let Route::Probe(b) = route(&gate, &cand()) else {
            panic!("候选要探测")
        };
        assert!(!gate.live(a) && gate.live(b));
        // 候选之后的鼠标按下（三击的第三下按下）：取消尚未完成的探测
        assert_eq!(
            route(&gate, &InputSignal::BobberDismiss(DismissReason::MouseDown)),
            Route::Nothing
        );
        assert!(!gate.live(b));
        // 浮标显示期间：重复、过期的 Dismiss 都不影响下一次候选与点击
        let Route::Probe(c) = route(&gate, &cand()) else {
            panic!("候选要探测")
        };
        for r in [
            DismissReason::Timeout,
            DismissReason::Timeout,
            DismissReason::KeyPress,
            DismissReason::Scroll,
            DismissReason::MovedAway,
            DismissReason::AppSwitched,
        ] {
            assert_eq!(route(&gate, &InputSignal::BobberDismiss(r)), Route::Nothing);
        }
        assert!(gate.live(c));
        assert!(gate.click_allowed());
        // 关掉浮标：进行中的探测不再显示、已显示的浮标点击不再触发
        gate.set_bobber(false);
        assert!(!gate.live(c) && !gate.click_allowed());
        gate.set_bobber(true);
        gate.close();
        assert!(!gate.click_allowed(), "派发器已关闭");
        // 其他信号直接派发
        assert_eq!(
            route(&gate, &InputSignal::DoubleTap),
            Route::Fire(HotkeyAction::TranslateSelection, TriggerOrigin::DoubleTap)
        );
        assert_eq!(
            route(
                &gate,
                &InputSignal::MouseAction {
                    button: wisp_types::MouseButtonKind::Back,
                    action: HotkeyAction::Clipboard,
                }
            ),
            Route::Fire(HotkeyAction::Clipboard, TriggerOrigin::MouseButton)
        );
    }

    #[test]
    fn not_compiled_reports_enabled_items() {
        let st = not_compiled(&Desired::from_settings(OsKind::Windows, &all_on()));
        assert_eq!(st.bobber, ExtTriggerState::Unavailable);
        assert_eq!(st.double_tap, ExtTriggerState::Unavailable);
        assert_eq!(
            st.reason_key.as_deref(),
            Some(wisp_platform::reason::HOOKS_NOT_COMPILED)
        );
        assert_eq!(not_compiled(&Desired::default()), ExtTriggerStatus::default());
    }
}
