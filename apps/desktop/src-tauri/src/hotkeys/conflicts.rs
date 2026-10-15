//! 静态冲突表（`data/conflicts.json`：各平台的系统快捷键、常见应用的默认键与邻居工具，来源写在每一条里）与分层的冲突判定。
//!
//! [`analyze`] 是纯函数，把录制规则（`recorder::rules`）、静态表、macOS 系统热键（CopySymbolicHotKeys）、与自身其他
//! 动作 / 风格的重复、邻居应用合成一份 `ConflictReport`；注册探测（第 3 层）需要真的调用插件，由 `HotkeyService::check`
//! 在它之后追加。组合键一律先规范化再比较（`Combo` 的解析结果），不比较原始字符串。

use std::sync::OnceLock;

use serde::Deserialize;
use wisp_platform::common::combo::Combo;
use wisp_types::{Conflict, ConflictLevel, ConflictReport, ConflictSource, HotkeyTarget, OsKind};

use super::display::display;
use super::plan::RegError;
use super::recorder::{self, conflict, conflict_with};

/// 内置的静态冲突数据
pub const CONFLICTS_JSON: &str = include_str!("data/conflicts.json");

/// i18n key：系统保留
pub const KEY_RESERVED: &str = "hotkeys.conflict.reserved";
/// i18n key：与常用应用冲突
pub const KEY_STATIC_APP: &str = "hotkeys.conflict.static_app";
/// i18n key：已被系统快捷键占用（macOS CopySymbolicHotKeys；系统不提供名称，文案指向系统设置）
pub const KEY_SYSTEM_TAKEN: &str = "hotkeys.conflict.system_taken";
/// i18n key：与自身其他动作 / 风格重复
pub const KEY_SELF_DUPLICATE: &str = "hotkeys.conflict.self_duplicate";
/// i18n key：注册探测失败——被其他程序占用（Windows 1409 / X11 BadAccess）
pub const KEY_REGISTRATION: &str = "hotkeys.conflict.registration";
/// i18n key：注册探测失败——系统拒绝注册（macOS 只能拿到这一种：Carbon 非 noErr、键码未知）
pub const KEY_REJECTED: &str = "hotkeys.conflict.rejected";
/// i18n key：通用编辑键（复制 / 粘贴 / 剪切 / 全选 / 撤销；Wisp 自己会模拟其中几个）
pub const KEY_EDIT_KEY: &str = "hotkeys.conflict.edit_key";
/// i18n key：`hotkey_set` 拒绝时的通用文案（禁止级、原冲突带参数时用；不带参数即可完整显示）
pub const KEY_UNUSABLE: &str = "hotkeys.conflict.unusable";
/// i18n key：`hotkey_set` 拒绝时的通用文案（警告级、需要确认后才能使用，原冲突带参数时用）
pub const KEY_NEEDS_CONFIRM: &str = "hotkeys.conflict.needs_confirm";
/// i18n key：邻居应用使用同一组合
pub const KEY_NEIGHBOR: &str = "hotkeys.conflict.neighbor";

/// 静态表的级别
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryLevel {
    Forbidden,
    Warning,
    Info,
}

/// 禁止级条目的类别（决定文案）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// 系统保留（`hotkeys.conflict.reserved`）
    #[default]
    System,
    /// 通用编辑键（`hotkeys.conflict.edit_key`）：复制 / 粘贴 / 剪切 / 全选 / 撤销，以及 Wisp 在终端里模拟的复制粘贴键。
    /// 注册成全局键后，Carbon / RegisterHotKey / XGrabKey 会连 Wisp 自己注入的按键一起截获——取词的复制回退与
    /// 粘贴回原应用就会触发 Wisp 自己的动作，全系统的复制粘贴也随之失效
    Edit,
}

/// 静态表的一条
#[derive(Debug, Clone, Deserialize)]
pub struct Entry {
    pub os: Vec<OsKind>,
    pub combo: String,
    pub level: EntryLevel,
    #[serde(default)]
    pub kind: EntryKind,
    /// 冲突的应用（warning / info 必填，填进 `{app}`）
    #[serde(default)]
    pub app: Option<String>,
}

/// 已知的邻居应用
#[derive(Debug, Clone, Deserialize)]
pub struct Neighbor {
    pub name: String,
    pub os: OsKind,
    /// macOS bundle id / Windows exe 名 / X11 WM_CLASS（比较时不区分大小写）
    pub ids: Vec<String>,
    /// 它的默认全局快捷键（规范串）
    pub combos: Vec<String>,
}

/// conflicts.json
#[derive(Debug, Clone, Deserialize)]
pub struct Table {
    pub schema_version: u32,
    pub entries: Vec<Entry>,
    #[serde(default)]
    pub neighbors: Vec<Neighbor>,
}

/// 解析后的数据表（首次使用时解析；数据非法时退回空表并记日志——单测保证内置数据合法）
pub fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(CONFLICTS_JSON).unwrap_or_else(|e| {
            log::error!("hotkeys: built-in conflicts.json is invalid: {e}");
            Table {
                schema_version: 0,
                entries: Vec::new(),
                neighbors: Vec::new(),
            }
        })
    })
}

/// 两个组合串是否等价（都能解析且解析结果相同）
fn same(a: &Combo, b: &str) -> bool {
    Combo::parse(b).is_some_and(|x| &x == a)
}

/// 某组合在某平台上命中的静态表条目
fn static_entries<'a>(os: OsKind, combo: &'a Combo) -> impl Iterator<Item = &'static Entry> + 'a {
    table()
        .entries
        .iter()
        .filter(move |e| e.os.contains(&os) && same(combo, &e.combo))
}

/// 静态表里是否是禁止级（注册时视为失败、参与回退）
pub fn is_reserved(os: OsKind, combo: &Combo) -> bool {
    static_entries(os, combo).any(|e| e.level == EntryLevel::Forbidden)
}

/// 某组合键在某平台上的静态冲突
pub fn static_conflicts(os: OsKind, combo: &str) -> Vec<Conflict> {
    let Some(parsed) = Combo::parse(combo) else {
        return Vec::new();
    };
    static_entries(os, &parsed)
        .map(|e| match e.level {
            EntryLevel::Forbidden => conflict_with(
                ConflictLevel::Forbidden,
                ConflictSource::Reserved,
                match e.kind {
                    EntryKind::System => KEY_RESERVED,
                    EntryKind::Edit => KEY_EDIT_KEY,
                },
                &[("combo", display(&parsed.to_canonical(), os))],
            ),
            EntryLevel::Warning | EntryLevel::Info => conflict_with(
                if e.level == EntryLevel::Warning {
                    ConflictLevel::Warning
                } else {
                    ConflictLevel::Info
                },
                ConflictSource::StaticApp,
                KEY_STATIC_APP,
                &[("app", e.app.clone().unwrap_or_default())],
            ),
        })
        .collect()
}

/// 注册探测失败对应的冲突：被其他程序占用与被系统拒绝分开说明——macOS 跨进程重复注册本来就不报错，
/// 拿到的「被拒绝」意味着系统不接受这个组合或键码未知，不能说成「已被其他程序占用」
pub fn registration_conflict(err: RegError) -> Conflict {
    conflict(
        ConflictLevel::Forbidden,
        ConflictSource::Registration,
        match err {
            RegError::Taken => KEY_REGISTRATION,
            RegError::Rejected => KEY_REJECTED,
        },
    )
}

/// `hotkey_set` 拒绝时下发的 i18n key：`IpcError.message` 只能带 key、带不了参数，所以必须是**不带参数即可完整显示**
/// 的 key——冲突本身不带参数时原样使用（例如 `registration`、`single_letter`），带参数的（`reserved {combo}`、
/// `self_duplicate {action}`、`neighbor {app, combo}`…）按级别换成通用文案；完整的报告由前端重新调用 `hotkey_check` 取得
pub fn rejection_key(c: &Conflict) -> &str {
    if c.params.is_empty() {
        &c.message_key
    } else if c.level == ConflictLevel::Forbidden {
        KEY_UNUSABLE
    } else {
        KEY_NEEDS_CONFIRM
    }
}

/// 已被占用的组合（自身其他动作 / 风格）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Taken {
    pub target: HotkeyTarget,
    /// 规范串
    pub combo: String,
    /// 显示名（按界面语言，见 `names`）
    pub name: String,
}

/// 冲突判定所需的运行期环境
#[derive(Debug, Clone, Default)]
pub struct CheckEnv {
    pub os: Option<OsKind>,
    /// Windows：系统里装有 AltGr 布局
    pub has_altgr: bool,
    /// macOS：CopySymbolicHotKeys 里已启用的系统热键（规范串）
    pub system: Vec<String>,
    /// 其他目标正在使用（生效中或显式绑定）的组合
    pub taken: Vec<Taken>,
    /// 正在运行的邻居应用：(名称, 默认键规范串)
    pub neighbors: Vec<(String, String)>,
}

impl CheckEnv {
    fn os(&self) -> OsKind {
        self.os.unwrap_or_else(OsKind::current)
    }
}

fn level_rank(l: ConflictLevel) -> u8 {
    match l {
        ConflictLevel::Forbidden => 0,
        ConflictLevel::Warning => 1,
        ConflictLevel::Info => 2,
    }
}

/// 排序（禁止 → 警告 → 提示，同级保持判定顺序）并去掉完全相同的条目；`acceptable` = 没有禁止级
pub fn finish(combo: String, display_str: String, mut conflicts: Vec<Conflict>) -> ConflictReport {
    let mut seen: Vec<Conflict> = Vec::with_capacity(conflicts.len());
    conflicts.retain(|c| {
        if seen.contains(c) {
            false
        } else {
            seen.push(c.clone());
            true
        }
    });
    conflicts.sort_by_key(|c| level_rank(c.level));
    let acceptable = conflicts.iter().all(|c| c.level != ConflictLevel::Forbidden);
    ConflictReport {
        combo,
        display: display_str,
        conflicts,
        acceptable,
    }
}

/// 分层冲突判定（除注册探测外）。`raw` 为前端录到的组合（任意大小写 / 顺序）
pub fn analyze(env: &CheckEnv, target: &HotkeyTarget, raw: &str) -> ConflictReport {
    let os = env.os();
    let combo = match recorder::parse(raw) {
        Ok(c) => c,
        Err(why) => {
            let shown = raw.trim().to_owned();
            return finish(
                shown.clone(),
                display(&shown, os),
                vec![recorder::invalid_conflict(why)],
            );
        }
    };
    let canonical = combo.to_canonical();
    let shown = display(&canonical, os);
    let mut out = recorder::rules(os, &combo, env.has_altgr);
    out.extend(static_conflicts(os, &canonical));
    if env.system.iter().any(|s| same(&combo, s)) {
        out.push(conflict(
            ConflictLevel::Forbidden,
            ConflictSource::SystemDynamic,
            KEY_SYSTEM_TAKEN,
        ));
    }
    for t in env
        .taken
        .iter()
        .filter(|t| &t.target != target && same(&combo, &t.combo))
    {
        out.push(conflict_with(
            ConflictLevel::Forbidden,
            ConflictSource::SelfDuplicate,
            KEY_SELF_DUPLICATE,
            &[("action", t.name.clone())],
        ));
    }
    for (app, _) in env.neighbors.iter().filter(|(_, c)| same(&combo, c)) {
        out.push(conflict_with(
            ConflictLevel::Warning,
            ConflictSource::Neighbor,
            KEY_NEIGHBOR,
            &[("app", app.clone()), ("combo", shown.clone())],
        ));
    }
    finish(canonical, shown, out)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashSet};

    use wisp_types::HotkeyAction;

    use super::*;

    #[test]
    fn table_is_valid_and_canonical() {
        let t: Table = serde_json::from_str(CONFLICTS_JSON).expect("conflicts.json 合法");
        assert_eq!(t.schema_version, 1);
        assert!(t.entries.len() >= 40, "静态表条目过少：{}", t.entries.len());
        let mut seen = HashSet::new();
        for e in &t.entries {
            let c = Combo::parse(&e.combo).unwrap_or_else(|| panic!("无法解析 {}", e.combo));
            assert_eq!(c.to_canonical(), e.combo, "条目必须写规范串");
            assert!(!e.os.is_empty(), "{} 没有平台", e.combo);
            if e.level != EntryLevel::Forbidden {
                assert!(e.app.as_deref().is_some_and(|a| !a.is_empty()), "{} 缺少 app", e.combo);
            }
            if e.kind == EntryKind::Edit {
                assert_eq!(e.level, EntryLevel::Forbidden, "编辑键 {} 必须是禁止级", e.combo);
            }
            for os in &e.os {
                assert!(seen.insert((*os, e.combo.clone())), "{os:?} {} 重复", e.combo);
            }
        }
        for n in &t.neighbors {
            assert!(!n.ids.is_empty(), "{} 没有 id", n.name);
            for c in &n.combos {
                assert_eq!(Combo::parse(c).map(|x| x.to_canonical()).as_deref(), Some(c.as_str()));
            }
        }
        // 邻居探测的对象都在表里
        for name in [
            "Bob", "Easydict", "Pot", "Raycast", "PopClip", "Maccy", "Paste", "Snipaste", "PixPin", "uTools", "Ditto",
            "QQ",
        ] {
            assert!(t.neighbors.iter().any(|n| n.name == name), "邻居表缺少 {name}");
        }
    }

    /// 三端默认表的首选 / 备选都不在静态禁止表里（否则默认值自己就注册失败）
    #[test]
    fn defaults_are_never_reserved() {
        for os in [OsKind::Macos, OsKind::Windows, OsKind::Linux] {
            for d in wisp_types::defaults::hotkey_table(os) {
                for raw in [d.primary, d.fallback] {
                    let c = Combo::parse(raw).expect("默认值可解析");
                    assert!(!is_reserved(os, &c), "{os:?} 的默认键 {raw} 在禁止表里");
                }
            }
        }
    }

    fn action(a: HotkeyAction) -> HotkeyTarget {
        HotkeyTarget::Action { action: a }
    }

    /// 数据驱动的用例（`data/check_cases.json`）
    #[derive(Deserialize)]
    struct Case {
        os: OsKind,
        combo: String,
        #[serde(default)]
        altgr: bool,
        #[serde(default)]
        system: Vec<String>,
        acceptable: bool,
        /// 期望的冲突（level + message_key，顺序即排序后的顺序）
        expect: Vec<(ConflictLevel, String)>,
        #[serde(default)]
        params: BTreeMap<String, String>,
    }

    /// 数据驱动的冲突判定
    #[test]
    fn data_driven_cases() {
        let cases: Vec<Case> = serde_json::from_str(include_str!("data/check_cases.json")).expect("用例合法");
        assert!(cases.len() >= 30);
        for c in &cases {
            let env = CheckEnv {
                os: Some(c.os),
                has_altgr: c.altgr,
                system: c.system.clone(),
                ..CheckEnv::default()
            };
            let r = analyze(&env, &action(HotkeyAction::Ocr), &c.combo);
            let got: Vec<(ConflictLevel, String)> =
                r.conflicts.iter().map(|x| (x.level, x.message_key.clone())).collect();
            assert_eq!(got, c.expect, "{:?} {}", c.os, c.combo);
            assert_eq!(r.acceptable, c.acceptable, "{:?} {}", c.os, c.combo);
            for (k, v) in &c.params {
                assert!(
                    r.conflicts.iter().any(|x| x.params.get(k) == Some(v)),
                    "{:?} {}：缺少参数 {k}={v}，实际 {:?}",
                    c.os,
                    c.combo,
                    r.conflicts
                );
            }
        }
    }

    /// 与自身其他动作或风格重复时禁止，并给出对方的名称；同一目标重录自己的键不算重复
    #[test]
    fn self_duplicates_are_forbidden() {
        let env = CheckEnv {
            os: Some(OsKind::Macos),
            taken: vec![
                Taken {
                    target: action(HotkeyAction::TranslateSelection),
                    combo: "Alt+KeyD".into(),
                    name: "划词翻译".into(),
                },
                Taken {
                    target: HotkeyTarget::Style {
                        style_id: "academic".into(),
                    },
                    combo: "Alt+Shift+KeyT".into(),
                    name: "学术".into(),
                },
            ],
            ..CheckEnv::default()
        };
        let r = analyze(&env, &action(HotkeyAction::Ocr), "option+d");
        assert!(!r.acceptable);
        assert_eq!(r.combo, "Alt+KeyD");
        assert_eq!(r.display, "⌥D");
        assert_eq!(r.conflicts[0].source, ConflictSource::SelfDuplicate);
        assert_eq!(
            r.conflicts[0].params.get("action").map(String::as_str),
            Some("划词翻译")
        );
        // 风格快捷键与动作互斥（与 ⌥D 重复时标红）
        let style = HotkeyTarget::Style {
            style_id: "technical".into(),
        };
        assert!(!analyze(&env, &style, "Alt+KeyD").acceptable);
        assert!(!analyze(&env, &action(HotkeyAction::Clipboard), "Shift+Alt+KeyT").acceptable);
        // 重录自己的键
        assert!(analyze(&env, &action(HotkeyAction::TranslateSelection), "Alt+KeyD").acceptable);
    }

    /// 只在邻居应用的默认键与录到的键重合时提示（警告级，可以「仍然使用」）
    #[test]
    fn neighbors_warn_on_overlap() {
        let env = CheckEnv {
            os: Some(OsKind::Macos),
            neighbors: vec![("Easydict".into(), "Alt+KeyD".into())],
            ..CheckEnv::default()
        };
        let r = analyze(&env, &action(HotkeyAction::TranslateSelection), "Alt+KeyD");
        assert!(r.acceptable);
        assert_eq!(r.conflicts.len(), 1);
        assert_eq!(r.conflicts[0].level, ConflictLevel::Warning);
        assert_eq!(r.conflicts[0].params.get("app").map(String::as_str), Some("Easydict"));
        assert_eq!(r.conflicts[0].params.get("combo").map(String::as_str), Some("⌥D"));
        assert!(
            analyze(&env, &action(HotkeyAction::Ocr), "Alt+KeyS")
                .conflicts
                .is_empty()
        );
    }

    /// Wisp 自己会模拟的复制 / 粘贴键（`common::keymap` 的平台默认与内置终端表）一律不能注册成全局键：否则 Carbon /
    /// RegisterHotKey / XGrabKey 会截获 Wisp 自己注入的按键（取词的复制回退、粘贴回原应用都会失效）
    #[test]
    fn simulated_edit_keys_are_forbidden() {
        use wisp_platform::common::keymap;
        for os in [OsKind::Macos, OsKind::Windows, OsKind::Linux] {
            let env = CheckEnv {
                os: Some(os),
                ..CheckEnv::default()
            };
            let mut simulated = vec![
                keymap::default_copy(os).to_owned(),
                keymap::default_paste(os).to_owned(),
            ];
            for k in keymap::builtin(os) {
                simulated.extend(k.copy.into_iter().chain(k.paste));
            }
            simulated.retain(|c| !c.eq_ignore_ascii_case("none"));
            assert!(simulated.len() >= 2);
            for combo in &simulated {
                let r = analyze(&env, &action(HotkeyAction::Ocr), combo);
                assert!(!r.acceptable, "{os:?} {combo} 是 Wisp 会模拟的按键，必须禁止");
            }
        }
        // 编辑键用专门的文案，并带上显示串
        let env = CheckEnv {
            os: Some(OsKind::Macos),
            ..CheckEnv::default()
        };
        let r = analyze(&env, &action(HotkeyAction::Ocr), "Cmd+C");
        assert_eq!(r.conflicts[0].message_key, KEY_EDIT_KEY);
        assert_eq!(r.conflicts[0].params.get("combo").map(String::as_str), Some("⌘C"));
    }

    /// 注册探测：被占用与被拒绝分开说明（macOS 只会拿到被拒绝）
    #[test]
    fn registration_failures_are_distinguished() {
        let taken = registration_conflict(RegError::Taken);
        let rejected = registration_conflict(RegError::Rejected);
        assert_eq!(taken.message_key, KEY_REGISTRATION);
        assert_eq!(rejected.message_key, KEY_REJECTED);
        for c in [&taken, &rejected] {
            assert_eq!(c.level, ConflictLevel::Forbidden);
            assert_eq!(c.source, ConflictSource::Registration);
            assert!(c.params.is_empty());
        }
    }

    /// `hotkey_set` 拒绝时下发的 key 不带参数也能完整显示（IpcError 只能带 key）
    #[test]
    fn rejection_keys_never_need_params() {
        let env = CheckEnv {
            os: Some(OsKind::Macos),
            neighbors: vec![("Easydict".into(), "Alt+KeyD".into())],
            taken: vec![Taken {
                target: action(HotkeyAction::Ocr),
                combo: "Alt+KeyS".into(),
                name: "截图 OCR".into(),
            }],
            ..CheckEnv::default()
        };
        let cases = [
            ("Super+Space", KEY_RESERVED, KEY_UNUSABLE),
            ("Alt+KeyS", KEY_SELF_DUPLICATE, KEY_UNUSABLE),
            ("Alt+KeyD", KEY_NEIGHBOR, KEY_NEEDS_CONFIRM),
            ("Alt+KeyE", recorder::KEY_DEAD_KEY, KEY_NEEDS_CONFIRM),
            ("Super+KeyK", recorder::KEY_SINGLE_LETTER, recorder::KEY_SINGLE_LETTER),
            ("Shift+KeyA", recorder::KEY_SHIFT_LETTER, recorder::KEY_SHIFT_LETTER),
        ];
        for (combo, raw_key, sent) in cases {
            let r = analyze(&env, &action(HotkeyAction::TranslateSelection), combo);
            let c = &r.conflicts[0];
            assert_eq!(c.message_key, raw_key, "{combo}");
            assert_eq!(rejection_key(c), sent, "{combo}");
        }
        assert_eq!(rejection_key(&registration_conflict(RegError::Rejected)), KEY_REJECTED);
    }

    #[test]
    fn finish_sorts_and_dedupes() {
        let w = conflict(ConflictLevel::Warning, ConflictSource::StaticApp, "a");
        let f = conflict(ConflictLevel::Forbidden, ConflictSource::Reserved, "b");
        let i = conflict(ConflictLevel::Info, ConflictSource::StaticApp, "c");
        let r = finish("x".into(), "x".into(), vec![i.clone(), w.clone(), f.clone(), w.clone()]);
        assert_eq!(r.conflicts, [f, w, i]);
        assert!(!r.acceptable);
    }
}
