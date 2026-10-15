//! 注册计划：由设置算出每个目标的候选组合，按「首选 → 备选 → 留空」尝试注册，
//! 产出 `HotkeyStatus` 列表。全部是纯逻辑，真正的注册经 [`Registrar`] 完成（服务里是全局快捷键插件，单测里是假实现）。
//!
//! 规则：
//! - 候选：`none` → 无；`default` → [首选, 备选]（只有默认值才回退）；自定义组合与风格快捷键 → [组合]；
//! - 两轮认领：先给显式绑定（自定义组合、风格快捷键）认领，再给 `default` 认领——默认值的回退不会抢走用户显式选择的键；
//! - 判定失败的原因（依次）：已被本轮其他目标认领 → 与自身重复；静态表禁止级 → 系统保留；命中 CopySymbolicHotKeys
//!   → 系统快捷键；插件注册失败 → 被占用（AlreadyRegistered）/ 被拒绝（其他错误）；
//! - **先注册新键、再注销旧键**：已经由本进程持有的组合直接沿用（不重复注册），全部认领完成后才注销不再需要的旧组合；
//!   注册失败不影响仍在工作的旧键。

use std::collections::BTreeMap;

use wisp_platform::common::combo::Combo;
use wisp_types::defaults::hotkey_default;
use wisp_types::{Binding, HotkeyAction, HotkeySettings, HotkeyState, HotkeyStatus, HotkeyTarget, OsKind, Style};

use super::conflicts;
use super::display::display;

/// 注册失败的原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// 与本轮另一个目标的组合相同
    Duplicate,
    /// 静态表禁止级（系统保留）
    Reserved,
    /// macOS 系统快捷键（CopySymbolicHotKeys）
    System,
    /// 被其他程序占用（Windows 1409 / X11 BadAccess）
    Taken,
    /// 系统拒绝注册（macOS 非 noErr、未知键码等）
    Rejected,
    /// 当前平台没有这个主键的键码（手改设置写了 macOS 的 F21 之类），不调用插件
    Unsupported,
}

/// `HotkeyStatus.reason_key` 的取值（i18n key，hotkeys.json 的 `reason` 组）
pub mod reason {
    pub const DUPLICATE: &str = "hotkeys.reason.duplicate";
    pub const RESERVED: &str = "hotkeys.reason.reserved";
    pub const SYSTEM: &str = "hotkeys.reason.system";
    pub const TAKEN: &str = "hotkeys.reason.taken";
    pub const REJECTED: &str = "hotkeys.reason.rejected";
    pub const INVALID: &str = "hotkeys.reason.invalid";
    /// 默认值的首选与备选都不可用
    pub const BOTH_UNAVAILABLE: &str = "hotkeys.reason.both_unavailable";
    /// 录制快捷键期间暂停
    pub const RECORDING: &str = "hotkeys.reason.recording";
}

impl Cause {
    /// 对应的 `reason_key`
    pub const fn reason_key(self) -> &'static str {
        match self {
            Self::Duplicate => reason::DUPLICATE,
            Self::Reserved => reason::RESERVED,
            Self::System => reason::SYSTEM,
            Self::Taken => reason::TAKEN,
            Self::Rejected => reason::REJECTED,
            Self::Unsupported => reason::INVALID,
        }
    }

    /// 是否可能自行消失、值得之后再试：被其他程序占用（对方退出）、被拒绝（多半是暂时的系统状态）、macOS 系统快捷键
    /// （用户可能去系统设置里关掉了它，而系统不会通知 Wisp）。重复 / 保留 / 无法识别只会因设置变化而改变，设置变化
    /// 本来就会重新注册
    pub const fn transient(self) -> bool {
        matches!(self, Self::Taken | Self::Rejected | Self::System)
    }
}

/// 插件注册的结果分类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegError {
    Taken,
    Rejected,
}

/// 真正执行注册 / 注销的后端（参数都是规范串）
pub trait Registrar {
    fn register(&self, combo: &str) -> Result<(), RegError>;
    fn unregister(&self, combo: &str);
}

/// 一个目标的期望
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Want {
    pub target: HotkeyTarget,
    pub binding: Binding,
    /// 候选组合（规范串）
    pub candidates: Vec<String>,
    /// 绑定是 `default`（只有它会回退到备选）
    pub is_default: bool,
    /// 绑定串无法解析（手改的 styles.json）；此时 candidates 为空
    pub invalid: bool,
}

impl Want {
    fn explicit(&self) -> bool {
        !self.is_default
    }
}

/// 规范化；无法解析时返回 None
fn canonical(raw: &str) -> Option<String> {
    super::recorder::normalize(raw)
}

/// 由设置与风格算出全部目标的期望：五个动作（`HotkeyAction::ALL` 顺序）+ 设了快捷键的风格（风格顺序）
pub fn wants(os: OsKind, hotkeys: &HotkeySettings, styles: &[Style]) -> Vec<Want> {
    let mut out: Vec<Want> = HotkeyAction::ALL
        .iter()
        .map(|&action| {
            let binding = hotkeys.binding(action);
            let target = HotkeyTarget::Action { action };
            if binding.is_none() {
                return Want {
                    target,
                    binding,
                    candidates: Vec::new(),
                    is_default: false,
                    invalid: false,
                };
            }
            if binding.is_default() {
                let d = hotkey_default(os, action);
                return Want {
                    target,
                    binding,
                    candidates: vec![d.primary.to_owned(), d.fallback.to_owned()],
                    is_default: true,
                    invalid: false,
                };
            }
            let c = canonical(binding.as_str());
            Want {
                target,
                invalid: c.is_none(),
                candidates: c.into_iter().collect(),
                binding,
                is_default: false,
            }
        })
        .collect();
    for s in styles {
        let Some(hotkey) = s.hotkey.as_deref().map(str::trim).filter(|h| !h.is_empty()) else {
            continue;
        };
        let c = canonical(hotkey);
        out.push(Want {
            target: HotkeyTarget::Style { style_id: s.id.clone() },
            binding: Binding(c.clone().unwrap_or_else(|| hotkey.to_owned())),
            invalid: c.is_none(),
            candidates: c.into_iter().collect(),
            is_default: false,
        });
    }
    out
}

/// 计算计划所需的环境
pub struct Env<'a> {
    pub os: OsKind,
    /// macOS 系统快捷键（规范串）
    pub system: &'a [String],
}

/// 某目标的状态（未注册时）
fn idle_status(w: &Want, state: HotkeyState, reason_key: Option<&str>) -> HotkeyStatus {
    HotkeyStatus {
        target: w.target.clone(),
        binding: w.binding.clone(),
        effective: None,
        display: None,
        state,
        reason_key: reason_key.map(str::to_owned),
    }
}

/// 快捷键不可用（插件未注册 / Wayland）时的状态：未绑定的仍是 Unbound，其余 Unavailable
pub fn unavailable(wants: &[Want], reason_key: &str) -> Vec<HotkeyStatus> {
    wants
        .iter()
        .map(|w| {
            if w.binding.is_none() {
                idle_status(w, HotkeyState::Unbound, None)
            } else {
                idle_status(w, HotkeyState::Unavailable, Some(reason_key))
            }
        })
        .collect()
}

/// 录制期间：已生效的标为 Suspended（保留生效的组合，界面仍显示），其余不变
pub fn suspended(statuses: &[HotkeyStatus]) -> Vec<HotkeyStatus> {
    statuses
        .iter()
        .map(|s| match s.state {
            HotkeyState::Registered | HotkeyState::Fallback => HotkeyStatus {
                state: HotkeyState::Suspended,
                reason_key: Some(reason::RECORDING.to_owned()),
                ..s.clone()
            },
            _ => s.clone(),
        })
        .collect()
}

/// 一次执行计划的结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub statuses: Vec<HotkeyStatus>,
    /// 有目标没用上首选（回退或失败），且原因可能自行消失（[`Cause::transient`]）：之后取状态时值得再试一次
    pub retryable: bool,
}

/// 执行计划：注册需要的新组合 → 产出状态 → 注销不再需要的旧组合。`held` 是当前持有的注册（规范串 → 目标），
/// 返回时已更新为新的持有集合
pub fn apply(wants: &[Want], held: &mut BTreeMap<String, HotkeyTarget>, env: &Env<'_>, reg: &dyn Registrar) -> Applied {
    let mut claimed: BTreeMap<String, HotkeyTarget> = BTreeMap::new();
    let mut out: Vec<Option<HotkeyStatus>> = vec![None; wants.len()];
    let mut retryable = false;
    for explicit_pass in [true, false] {
        for (i, w) in wants.iter().enumerate() {
            if w.explicit() != explicit_pass {
                continue;
            }
            let (status, transient) = claim(w, held, &mut claimed, env, reg);
            retryable |= transient;
            out[i] = Some(status);
        }
    }
    let stale: Vec<String> = held.keys().filter(|c| !claimed.contains_key(*c)).cloned().collect();
    for combo in stale {
        reg.unregister(&combo);
    }
    *held = claimed;
    Applied {
        statuses: out.into_iter().flatten().collect(),
        retryable,
    }
}

/// 录制开始时注销当前持有的全部注册（录制中按 ⌥D 不会弹出面板）；`held` 清空
pub fn release_all(held: &mut BTreeMap<String, HotkeyTarget>, reg: &dyn Registrar) {
    for combo in held.keys() {
        reg.unregister(combo);
    }
    held.clear();
}

/// `hotkey_set` 的提交（先注册新键、失败保留旧值）：
/// 1. 新组合还没被 Wisp 持有、且当前能注册时，先真正注册它——失败直接返回，什么都不写；
/// 2. 写设置（`persist`）——失败时撤销第 1 步刚做的注册，`held` 恢复原样；
/// 3. 成功后由调用方按新设置整体重算（旧键在那一步最后注销）。
///
/// `register_now` 为 false（录制中、快捷键不可用）时只写设置。返回 `RegError` 表示第 1 步失败
pub fn commit_binding<E>(
    held: &mut BTreeMap<String, HotkeyTarget>,
    target: &HotkeyTarget,
    binding: &Binding,
    register_now: bool,
    reg: &dyn Registrar,
    persist: impl FnOnce() -> Result<(), E>,
) -> Result<(), Commit<E>> {
    let mut fresh: Option<String> = None;
    if binding.is_combo() && register_now && !held.contains_key(binding.as_str()) {
        reg.register(binding.as_str()).map_err(Commit::Register)?;
        held.insert(binding.0.clone(), target.clone());
        fresh = Some(binding.0.clone());
    }
    if let Err(e) = persist() {
        if let Some(combo) = fresh {
            reg.unregister(&combo);
            held.remove(&combo);
        }
        return Err(Commit::Persist(e));
    }
    Ok(())
}

/// [`commit_binding`] 的失败
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Commit<E> {
    /// 新组合注册失败（什么都没写）
    Register(RegError),
    /// 写设置失败（新注册已撤销）
    Persist(E),
}

/// 给一个目标认领一个组合；返回状态与「没用上首选的原因里有可能自行消失的」
fn claim(
    w: &Want,
    held: &BTreeMap<String, HotkeyTarget>,
    claimed: &mut BTreeMap<String, HotkeyTarget>,
    env: &Env<'_>,
    reg: &dyn Registrar,
) -> (HotkeyStatus, bool) {
    if w.binding.is_none() {
        return (idle_status(w, HotkeyState::Unbound, None), false);
    }
    if w.invalid || w.candidates.is_empty() {
        return (idle_status(w, HotkeyState::Failed, Some(reason::INVALID)), false);
    }
    let mut first_cause: Option<Cause> = None;
    let mut last_cause = Cause::Rejected;
    let mut transient = false;
    for (i, cand) in w.candidates.iter().enumerate() {
        let cause = obstacle(cand, held, claimed, env, reg);
        match cause {
            None => {
                claimed.insert(cand.clone(), w.target.clone());
                let (state, reason_key) = if i == 0 {
                    (HotkeyState::Registered, None)
                } else {
                    (HotkeyState::Fallback, first_cause.map(Cause::reason_key))
                };
                let status = HotkeyStatus {
                    target: w.target.clone(),
                    binding: w.binding.clone(),
                    effective: Some(cand.clone()),
                    display: Some(display(cand, env.os)),
                    state,
                    reason_key: reason_key.map(str::to_owned),
                };
                return (status, transient);
            }
            Some(c) => {
                first_cause.get_or_insert(c);
                last_cause = c;
                transient |= c.transient();
            }
        }
    }
    let reason_key = if w.is_default {
        reason::BOTH_UNAVAILABLE
    } else {
        last_cause.reason_key()
    };
    (idle_status(w, HotkeyState::Failed, Some(reason_key)), transient)
}

/// 某候选能否用；能用时（已持有或注册成功）返回 None
fn obstacle(
    cand: &str,
    held: &BTreeMap<String, HotkeyTarget>,
    claimed: &BTreeMap<String, HotkeyTarget>,
    env: &Env<'_>,
    reg: &dyn Registrar,
) -> Option<Cause> {
    if claimed.contains_key(cand) {
        return Some(Cause::Duplicate);
    }
    let Some(combo) = Combo::parse(cand) else {
        return Some(Cause::Unsupported);
    };
    if !super::recorder::platform_has_key(env.os, &combo.code) {
        return Some(Cause::Unsupported);
    }
    if conflicts::is_reserved(env.os, &combo) {
        return Some(Cause::Reserved);
    }
    if env.system.iter().any(|s| Combo::parse(s).as_ref() == Some(&combo)) {
        return Some(Cause::System);
    }
    if held.contains_key(cand) {
        return None;
    }
    match reg.register(cand) {
        Ok(()) => None,
        Err(RegError::Taken) => Some(Cause::Taken),
        Err(RegError::Rejected) => Some(Cause::Rejected),
    }
}

/// 把插件的错误串归类（插件把 global-hotkey 的错误转成了字符串：`HotKey already registered: …` → 被占用）
pub fn classify(message: &str) -> RegError {
    if message.starts_with("HotKey already registered") {
        RegError::Taken
    } else {
        RegError::Rejected
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashSet;

    use super::*;

    /// 假注册后端：`taken` 里的组合注册失败（被占用），`rejected` 里的被拒绝；记录调用顺序
    #[derive(Default)]
    struct Fake {
        taken: HashSet<String>,
        rejected: HashSet<String>,
        log: RefCell<Vec<String>>,
    }

    impl Registrar for Fake {
        fn register(&self, combo: &str) -> Result<(), RegError> {
            self.log.borrow_mut().push(format!("+{combo}"));
            if self.taken.contains(combo) {
                Err(RegError::Taken)
            } else if self.rejected.contains(combo) {
                Err(RegError::Rejected)
            } else {
                Ok(())
            }
        }
        fn unregister(&self, combo: &str) {
            self.log.borrow_mut().push(format!("-{combo}"));
        }
    }

    fn fake(taken: &[&str]) -> Fake {
        Fake {
            taken: taken.iter().map(|s| (*s).to_owned()).collect(),
            ..Fake::default()
        }
    }

    fn env(os: OsKind) -> Env<'static> {
        Env { os, system: &[] }
    }

    /// 只取状态（多数用例不关心 retryable）
    fn apply(
        ws: &[Want],
        held: &mut BTreeMap<String, HotkeyTarget>,
        env: &Env<'_>,
        reg: &dyn Registrar,
    ) -> Vec<HotkeyStatus> {
        super::apply(ws, held, env, reg).statuses
    }

    fn settings(bindings: &[(HotkeyAction, &str)]) -> HotkeySettings {
        let mut s = HotkeySettings::default();
        for (a, b) in bindings {
            s.bindings.insert(*a, Binding((*b).to_owned()));
        }
        s
    }

    fn state_of(statuses: &[HotkeyStatus], action: HotkeyAction) -> &HotkeyStatus {
        statuses
            .iter()
            .find(|s| s.target == HotkeyTarget::Action { action })
            .expect("动作都在状态里")
    }

    /// 新装（全部 default）时三端都注册首选键，状态 Registered，显示串正确
    #[test]
    fn fresh_install_registers_primaries() {
        for os in [OsKind::Macos, OsKind::Windows, OsKind::Linux] {
            let ws = wants(os, &HotkeySettings::default(), &[]);
            let reg = fake(&[]);
            let mut held = BTreeMap::new();
            let st = apply(&ws, &mut held, &env(os), &reg);
            assert_eq!(st.len(), 5);
            for (s, d) in st.iter().zip(wisp_types::defaults::hotkey_table(os)) {
                assert_eq!(s.state, HotkeyState::Registered, "{os:?}");
                assert_eq!(s.effective.as_deref(), Some(d.primary));
                assert_eq!(s.binding, Binding::default_binding());
                assert_eq!(s.reason_key, None);
            }
            assert_eq!(held.len(), 5);
        }
        let st = apply(
            &wants(OsKind::Macos, &HotkeySettings::default(), &[]),
            &mut BTreeMap::new(),
            &env(OsKind::Macos),
            &fake(&[]),
        );
        assert_eq!(st[0].display.as_deref(), Some("⌥D"));
    }

    /// Windows 上 Ctrl+Alt+D 被占用 → Alt+Shift+D 并标黄；两个都被占用 → 留空标红
    #[test]
    fn default_falls_back_then_fails() {
        let os = OsKind::Windows;
        let ws = wants(os, &HotkeySettings::default(), &[]);
        let st = apply(&ws, &mut BTreeMap::new(), &env(os), &fake(&["Control+Alt+KeyD"]));
        let s = state_of(&st, HotkeyAction::TranslateSelection);
        assert_eq!(s.state, HotkeyState::Fallback);
        assert_eq!(s.effective.as_deref(), Some("Alt+Shift+KeyD"));
        assert_eq!(s.display.as_deref(), Some("Alt+Shift+D"));
        assert_eq!(s.reason_key.as_deref(), Some(reason::TAKEN));

        let st = apply(
            &ws,
            &mut BTreeMap::new(),
            &env(os),
            &fake(&["Control+Alt+KeyD", "Alt+Shift+KeyD"]),
        );
        let s = state_of(&st, HotkeyAction::TranslateSelection);
        assert_eq!(s.state, HotkeyState::Failed);
        assert_eq!(s.effective, None);
        assert_eq!(s.reason_key.as_deref(), Some(reason::BOTH_UNAVAILABLE));
    }

    /// 回退只用于默认值：用户自定义的组合失败时直接留空标红
    #[test]
    fn custom_binding_never_falls_back() {
        let os = OsKind::Windows;
        let s = settings(&[(HotkeyAction::Ocr, "Control+Shift+Alt+KeyO")]);
        let st = apply(
            &wants(os, &s, &[]),
            &mut BTreeMap::new(),
            &env(os),
            &fake(&["Control+Alt+Shift+KeyO"]),
        );
        let s = state_of(&st, HotkeyAction::Ocr);
        assert_eq!(s.state, HotkeyState::Failed);
        assert_eq!(s.reason_key.as_deref(), Some(reason::TAKEN));
        assert_eq!(s.binding.as_str(), "Control+Shift+Alt+KeyO", "binding 保留用户原值");
    }

    /// 命中 CopySymbolicHotKeys 或静态禁止表都视为注册失败、参与回退（不调用插件）
    #[test]
    fn system_and_reserved_count_as_failures() {
        let os = OsKind::Macos;
        let system = vec!["Alt+KeyS".to_owned()];
        let reg = fake(&[]);
        let st = apply(
            &wants(os, &HotkeySettings::default(), &[]),
            &mut BTreeMap::new(),
            &Env { os, system: &system },
            &reg,
        );
        let s = state_of(&st, HotkeyAction::Ocr);
        assert_eq!(s.state, HotkeyState::Fallback);
        assert_eq!(s.effective.as_deref(), Some("Control+Alt+KeyS"));
        assert_eq!(s.reason_key.as_deref(), Some(reason::SYSTEM));
        assert!(
            !reg.log.borrow().iter().any(|l| l == "+Alt+KeyS"),
            "系统已占用的键不尝试注册"
        );

        // 手改 settings.json 写了系统保留键
        let s = settings(&[(HotkeyAction::Clipboard, "Super+Space")]);
        let st = apply(&wants(os, &s, &[]), &mut BTreeMap::new(), &env(os), &fake(&[]));
        let c = state_of(&st, HotkeyAction::Clipboard);
        assert_eq!(c.state, HotkeyState::Failed);
        assert_eq!(c.reason_key.as_deref(), Some(reason::RESERVED));
    }

    /// 显式绑定先认领：默认值的回退不会抢走用户显式选择的键；显式重复时后者失败
    #[test]
    fn explicit_bindings_claim_first() {
        let os = OsKind::Macos;
        // 剪贴板显式用了划词翻译的备选键 ⌃⌥D，划词翻译首选 ⌥D 被占用 → 不能回退到 ⌃⌥D
        let s = settings(&[(HotkeyAction::Clipboard, "Control+Alt+KeyD")]);
        let st = apply(
            &wants(os, &s, &[]),
            &mut BTreeMap::new(),
            &env(os),
            &fake(&["Alt+KeyD"]),
        );
        assert_eq!(state_of(&st, HotkeyAction::Clipboard).state, HotkeyState::Registered);
        let t = state_of(&st, HotkeyAction::TranslateSelection);
        assert_eq!(t.state, HotkeyState::Failed);
        assert_eq!(t.reason_key.as_deref(), Some(reason::BOTH_UNAVAILABLE));
        // 显式绑定占了别人的首选：别人回退
        let s = settings(&[(HotkeyAction::Ocr, "Alt+KeyD")]);
        let st = apply(&wants(os, &s, &[]), &mut BTreeMap::new(), &env(os), &fake(&[]));
        let t = state_of(&st, HotkeyAction::TranslateSelection);
        assert_eq!(t.state, HotkeyState::Fallback);
        assert_eq!(t.reason_key.as_deref(), Some(reason::DUPLICATE));
        // 状态列表仍按动作顺序
        let order: Vec<_> = st.iter().map(|s| s.target.clone()).collect();
        let expect: Vec<_> = HotkeyAction::ALL
            .iter()
            .map(|&action| HotkeyTarget::Action { action })
            .collect();
        assert_eq!(order, expect);
    }

    /// 风格快捷键与动作互斥；无法解析的风格快捷键标红
    #[test]
    fn style_hotkeys() {
        let os = OsKind::Macos;
        let mut academic = Style::custom("academic", "");
        academic.hotkey = Some("alt+shift+t".to_owned());
        let mut dup = Style::custom("custom-dup00000", "dup");
        dup.hotkey = Some("Alt+KeyD".to_owned());
        let mut bad = Style::custom("custom-bad00000", "bad");
        bad.hotkey = Some("Alt+Nope".to_owned());
        let plain = Style::custom("custom-none0000", "none");
        let ws = wants(os, &HotkeySettings::default(), &[academic, dup, bad, plain]);
        assert_eq!(ws.len(), 8, "没设快捷键的风格不出现在状态里");
        let st = apply(&ws, &mut BTreeMap::new(), &env(os), &fake(&[]));
        assert_eq!(st[5].effective.as_deref(), Some("Alt+Shift+KeyT"));
        assert_eq!(st[5].binding.as_str(), "Alt+Shift+KeyT");
        assert_eq!(st[5].state, HotkeyState::Registered);
        // 风格显式认领 ⌥D，划词翻译回退
        assert_eq!(st[6].state, HotkeyState::Registered);
        assert_eq!(st[0].state, HotkeyState::Fallback);
        assert_eq!(st[7].state, HotkeyState::Failed);
        assert_eq!(st[7].reason_key.as_deref(), Some(reason::INVALID));
    }

    /// 改键时先注册新键、成功后再注销旧键；没变的键不重复注册；失败时保留旧值
    #[test]
    fn registers_new_before_unregistering_old() {
        let os = OsKind::Macos;
        let mut held = BTreeMap::new();
        let reg = fake(&[]);
        apply(&wants(os, &HotkeySettings::default(), &[]), &mut held, &env(os), &reg);
        reg.log.borrow_mut().clear();
        let s = settings(&[(HotkeyAction::Ocr, "Control+Alt+KeyO")]);
        let st = apply(&wants(os, &s, &[]), &mut held, &env(os), &reg);
        assert_eq!(*reg.log.borrow(), ["+Control+Alt+KeyO", "-Alt+KeyS"]);
        assert_eq!(
            state_of(&st, HotkeyAction::Ocr).effective.as_deref(),
            Some("Control+Alt+KeyO")
        );
        assert!(held.contains_key("Control+Alt+KeyO") && !held.contains_key("Alt+KeyS"));
        // 再次应用相同设置：零调用
        reg.log.borrow_mut().clear();
        apply(&wants(os, &s, &[]), &mut held, &env(os), &reg);
        assert!(reg.log.borrow().is_empty());
        // 两个目标交换已持有的键：不注册、不注销，只改路由
        let swapped = settings(&[
            (HotkeyAction::Ocr, "Alt+KeyD"),
            (HotkeyAction::TranslateSelection, "Control+Alt+KeyO"),
        ]);
        apply(&wants(os, &swapped, &[]), &mut held, &env(os), &reg);
        assert!(reg.log.borrow().is_empty(), "{:?}", reg.log.borrow());
        assert_eq!(
            held.get("Alt+KeyD"),
            Some(&HotkeyTarget::Action {
                action: HotkeyAction::Ocr
            })
        );
    }

    /// 全部设为 none：注销全部，状态 Unbound
    #[test]
    fn none_unregisters_everything() {
        let os = OsKind::Linux;
        let mut held = BTreeMap::new();
        let reg = fake(&[]);
        apply(&wants(os, &HotkeySettings::default(), &[]), &mut held, &env(os), &reg);
        let all_none = settings(&HotkeyAction::ALL.map(|a| (a, "none")));
        let st = apply(&wants(os, &all_none, &[]), &mut held, &env(os), &reg);
        assert!(held.is_empty());
        assert!(
            st.iter()
                .all(|s| s.state == HotkeyState::Unbound && s.effective.is_none())
        );
        assert_eq!(reg.log.borrow().iter().filter(|l| l.starts_with('-')).count(), 5);
    }

    #[test]
    fn unavailable_and_suspended_states() {
        let os = OsKind::Linux;
        let s = settings(&[(HotkeyAction::Clipboard, "none")]);
        let ws = wants(os, &s, &[]);
        let st = unavailable(&ws, "errors.platform.wayland_hotkeys");
        assert_eq!(st[0].state, HotkeyState::Unavailable);
        assert_eq!(st[0].reason_key.as_deref(), Some("errors.platform.wayland_hotkeys"));
        assert_eq!(st[4].state, HotkeyState::Unbound);

        let live = apply(&ws, &mut BTreeMap::new(), &env(os), &fake(&[]));
        let paused = suspended(&live);
        assert_eq!(paused[0].state, HotkeyState::Suspended);
        assert_eq!(paused[0].effective, live[0].effective, "录制期间仍显示原来的键");
        assert_eq!(paused[0].reason_key.as_deref(), Some(reason::RECORDING));
        assert_eq!(paused[4].state, HotkeyState::Unbound);
    }

    /// 只有可能自行消失的原因（被占用 / 被拒绝 / 系统快捷键）才值得在取状态时重试；重复、保留、无法识别不重试
    #[test]
    fn retryable_only_for_transient_causes() {
        let os = OsKind::Windows;
        let defaults = wants(os, &HotkeySettings::default(), &[]);
        let ok = super::apply(&defaults, &mut BTreeMap::new(), &env(os), &fake(&[]));
        assert!(!ok.retryable, "全部用上首选");
        let taken = super::apply(&defaults, &mut BTreeMap::new(), &env(os), &fake(&["Control+Alt+KeyD"]));
        assert!(taken.retryable, "首选被占用而回退：对方退出后可以改回首选");
        let both = super::apply(
            &defaults,
            &mut BTreeMap::new(),
            &env(os),
            &fake(&["Control+Alt+KeyD", "Alt+Shift+KeyD"]),
        );
        assert!(both.retryable, "两个都被占用（BOTH_UNAVAILABLE）也记得原因");
        // 显式绑定占了别人的首选：别人回退，原因是重复——设置不变就不会好，不重试
        let dup = settings(&[(HotkeyAction::Ocr, "Control+Alt+KeyD")]);
        let r = super::apply(&wants(os, &dup, &[]), &mut BTreeMap::new(), &env(os), &fake(&[]));
        assert_eq!(
            state_of(&r.statuses, HotkeyAction::TranslateSelection).state,
            HotkeyState::Fallback
        );
        assert!(!r.retryable);
        // 手改设置写了系统保留键 / 平台没有的键：失败但不重试
        let mut reg = fake(&[]);
        reg.rejected.insert("Alt+F21".to_owned());
        let bad = settings(&[(HotkeyAction::Clipboard, "Super+Space"), (HotkeyAction::Ocr, "Alt+F21")]);
        let r = super::apply(
            &wants(OsKind::Macos, &bad, &[]),
            &mut BTreeMap::new(),
            &env(OsKind::Macos),
            &reg,
        );
        assert!(!r.retryable);
        assert_eq!(
            state_of(&r.statuses, HotkeyAction::Ocr).reason_key.as_deref(),
            Some(reason::INVALID)
        );
        assert!(
            !reg.log.borrow().iter().any(|l| l.contains("F21")),
            "平台没有的键不调用插件"
        );
        // macOS 系统快捷键：用户可能去系统设置里关掉，值得重试
        let system = vec!["Alt+KeyS".to_owned()];
        let r = super::apply(
            &wants(OsKind::Macos, &HotkeySettings::default(), &[]),
            &mut BTreeMap::new(),
            &Env {
                os: OsKind::Macos,
                system: &system,
            },
            &fake(&[]),
        );
        assert!(r.retryable);
    }

    /// `hotkey_set`：先注册新键；写设置失败时撤销刚注册的键，持有集合恢复原样；注册失败时不写设置
    #[test]
    fn commit_registers_first_and_rolls_back() {
        let target = HotkeyTarget::Action {
            action: HotkeyAction::Ocr,
        };
        let mut held = BTreeMap::new();
        held.insert(
            "Alt+KeyS".to_owned(),
            HotkeyTarget::Action {
                action: HotkeyAction::Ocr,
            },
        );
        let before = held.clone();
        let reg = fake(&[]);
        let binding = Binding("Control+Alt+KeyO".to_owned());
        // 写设置失败：撤销
        let r = commit_binding(&mut held, &target, &binding, true, &reg, || Err("disk full"));
        assert_eq!(r, Err(Commit::Persist("disk full")));
        assert_eq!(*reg.log.borrow(), ["+Control+Alt+KeyO", "-Control+Alt+KeyO"]);
        assert_eq!(held, before, "旧绑定保持不变");
        // 注册失败：不写设置
        reg.log.borrow_mut().clear();
        let busy = fake(&["Control+Alt+KeyO"]);
        let mut wrote = false;
        let r = commit_binding(&mut held, &target, &binding, true, &busy, || {
            wrote = true;
            Ok::<(), ()>(())
        });
        assert_eq!(r, Err(Commit::Register(RegError::Taken)));
        assert!(!wrote);
        assert_eq!(held, before);
        // 成功：新键已持有（旧键由之后的整体重算注销）
        let r = commit_binding(&mut held, &target, &binding, true, &reg, || Ok::<(), ()>(()));
        assert_eq!(r, Ok(()));
        assert!(held.contains_key("Control+Alt+KeyO") && held.contains_key("Alt+KeyS"));
        // 已持有的组合、录制中（register_now = false）、none / default：只写设置，不调用插件
        reg.log.borrow_mut().clear();
        for (b, now) in [
            (Binding("Control+Alt+KeyO".to_owned()), true),
            (Binding("Control+Alt+KeyP".to_owned()), false),
            (Binding::none(), true),
            (Binding::default_binding(), true),
        ] {
            let mut wrote = false;
            let r = commit_binding(&mut held, &target, &b, now, &reg, || {
                wrote = true;
                Ok::<(), ()>(())
            });
            assert_eq!(r, Ok(()));
            assert!(wrote);
        }
        assert!(reg.log.borrow().is_empty(), "{:?}", reg.log.borrow());
    }

    /// 录制开始：注销全部持有的注册；结束后按设置重新注册全部
    #[test]
    fn recording_releases_and_restores_everything() {
        let os = OsKind::Macos;
        let ws = wants(os, &HotkeySettings::default(), &[]);
        let mut held = BTreeMap::new();
        let reg = fake(&[]);
        apply(&ws, &mut held, &env(os), &reg);
        reg.log.borrow_mut().clear();
        release_all(&mut held, &reg);
        assert!(held.is_empty());
        assert_eq!(reg.log.borrow().len(), 5);
        assert!(reg.log.borrow().iter().all(|l| l.starts_with('-')));
        reg.log.borrow_mut().clear();
        let st = apply(&ws, &mut held, &env(os), &reg);
        assert_eq!(reg.log.borrow().len(), 5);
        assert!(reg.log.borrow().iter().all(|l| l.starts_with('+')));
        assert!(st.iter().all(|s| s.state == HotkeyState::Registered));
    }

    #[test]
    fn classifies_plugin_errors() {
        assert_eq!(
            classify("HotKey already registered: HotKey { mods: Modifiers(ALT), key: KeyD, id: 1 }"),
            RegError::Taken
        );
        assert_eq!(
            classify("Unable to register hotkey: RegisterEventHotKey failed for KeyD"),
            RegError::Rejected
        );
        assert_eq!(classify("Access is denied. (os error 5)"), RegError::Rejected);
    }
}
