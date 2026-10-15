//! 录制规则：把前端录到的按键规范化为 global-hotkey 规范串，并按「允许 / 禁止 / 警告」规则
//! 给出冲突（与静态表、系统热键、注册探测等其他层在 `conflicts::analyze` 里合并）。
//!
//! - 允许：≥1 个修饰键 + 非修饰主键；裸 F13–F24（macOS 到 F20 为止）；
//! - 禁止：纯修饰键、无法解析（含 Fn / 🌐、区分左右的修饰键名、global-hotkey 不认识的主键）、当前平台没有键码的主键
//!   （macOS 的 F21–F24、PrintScreen、Pause、媒体键等，global-hotkey 的 macOS 后端注册必然失败；Windows 的
//!   NumpadEqual 会被 global-hotkey 错映射成 E）、没有修饰键的普通键、单独 ⇧ + 字母（以及单独 ⇧ + 其他会影响输入的
//!   键）、Windows 的裸 F12；
//! - 警告：裸 F1–F11（以及 macOS / Linux 的裸 F12）、⌘ / ⌃（Win / Linux 的 Ctrl）+ 单字母、Win / Linux 的 Alt + 字母、
//!   Windows 的 Win + …、AltGr 布局下 Windows 的 Ctrl + Alt、macOS 的死键 ⌥E / U / I / N / `。
//!
//! 「与自身其他动作 / 风格重复」与系统保留键由 `conflicts` 判定（需要运行期状态与数据表）。

use std::collections::BTreeMap;
use std::str::FromStr;

use wisp_platform::common::combo::{Combo, Modifiers};
use wisp_types::{Conflict, ConflictLevel, ConflictSource, OsKind};

/// i18n key：至少需要一个修饰键（与录制器前端的提示共用）
pub const KEY_MODIFIER_REQUIRED: &str = "hotkeys.recorder.modifier_required";
/// i18n key：无法识别的组合
pub const KEY_INVALID: &str = "hotkeys.conflict.invalid";
/// i18n key：单独 ⇧ + 字母
pub const KEY_SHIFT_LETTER: &str = "hotkeys.conflict.shift_letter";
/// i18n key：单独 ⇧ + 其他会影响输入 / 选择的键
pub const KEY_SHIFT_KEY: &str = "hotkeys.conflict.shift_key";
/// i18n key：Windows 的 F12 保留给调试器
pub const KEY_F12: &str = "hotkeys.conflict.f12";
/// i18n key：裸功能键
pub const KEY_BARE_KEY: &str = "hotkeys.conflict.bare_key";
/// i18n key：修饰键 + 单字母
pub const KEY_SINGLE_LETTER: &str = "hotkeys.conflict.single_letter";
/// i18n key：Alt + 字母（菜单访问键）
pub const KEY_ALT_LETTER: &str = "hotkeys.conflict.alt_letter";
/// i18n key：Win 组合
pub const KEY_WIN_KEY: &str = "hotkeys.conflict.win_key";
/// i18n key：AltGr 布局
pub const KEY_LAYOUT: &str = "hotkeys.conflict.layout";
/// i18n key：死键
pub const KEY_DEAD_KEY: &str = "hotkeys.conflict.dead_key";

/// 录到的串解析失败的原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalid {
    /// 只有修饰键（例如 `Control+Alt`）
    ModifierOnly,
    /// 无法解析，或 global-hotkey 不认识（不能注册）
    Unparsable,
}

/// 解析并规范化；返回规范串可以被 global-hotkey 解析（能交给插件注册）
pub fn parse(raw: &str) -> Result<Combo, Invalid> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(Invalid::Unparsable);
    }
    match Combo::parse(trimmed) {
        Some(combo) => {
            if tauri_plugin_global_shortcut::Shortcut::from_str(&combo.to_canonical()).is_ok() {
                Ok(combo)
            } else {
                Err(Invalid::Unparsable)
            }
        }
        None if is_modifier_only(trimmed) => Err(Invalid::ModifierOnly),
        None => Err(Invalid::Unparsable),
    }
}

/// 规范化录到的组合；不合法时返回 None
pub fn normalize(raw: &str) -> Option<String> {
    parse(raw).ok().map(|c| c.to_canonical())
}

/// 串里每一段都是修饰键名（包括左右区分的写法），没有主键
fn is_modifier_only(raw: &str) -> bool {
    const MODS: &[&str] = &[
        "control",
        "ctrl",
        "alt",
        "option",
        "opt",
        "shift",
        "super",
        "meta",
        "cmd",
        "command",
        "win",
        "windows",
        "cmdorctrl",
        "cmdorcontrol",
        "commandorctrl",
        "commandorcontrol",
        "controlleft",
        "controlright",
        "altleft",
        "altright",
        "shiftleft",
        "shiftright",
        "metaleft",
        "metaright",
        "osleft",
        "osright",
        "fn",
        "fnlock",
    ];
    let mut any = false;
    for token in raw.split('+').map(str::trim) {
        if token.is_empty() {
            continue;
        }
        if !MODS.iter().any(|m| m.eq_ignore_ascii_case(token)) {
            return false;
        }
        any = true;
    }
    any
}

/// 解析失败转成冲突（禁止级）
pub fn invalid_conflict(why: Invalid) -> Conflict {
    match why {
        Invalid::ModifierOnly => conflict(
            ConflictLevel::Forbidden,
            ConflictSource::Reserved,
            KEY_MODIFIER_REQUIRED,
        ),
        Invalid::Unparsable => conflict(ConflictLevel::Forbidden, ConflictSource::Reserved, KEY_INVALID),
    }
}

/// 构造一条不带参数的冲突
pub fn conflict(level: ConflictLevel, source: ConflictSource, key: &str) -> Conflict {
    Conflict {
        level,
        source,
        message_key: key.to_owned(),
        params: BTreeMap::new(),
    }
}

/// 构造一条带参数的冲突
pub fn conflict_with(level: ConflictLevel, source: ConflictSource, key: &str, params: &[(&str, String)]) -> Conflict {
    Conflict {
        level,
        source,
        message_key: key.to_owned(),
        params: params.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect(),
    }
}

/// `F1`–`F24` 的序号
fn function_key(code: &str) -> Option<u32> {
    code.strip_prefix('F')
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|n| n.parse().ok())
        .filter(|n| (1..=24).contains(n))
}

/// 单独按住时会产生字符或参与文本编辑的键（⇧ + 这些键会影响输入与选择文本）
fn is_typing_key(code: &str) -> bool {
    code.starts_with("Key")
        || code.starts_with("Digit")
        || code.starts_with("Numpad")
        || matches!(
            code,
            "Backquote"
                | "Backslash"
                | "BracketLeft"
                | "BracketRight"
                | "Comma"
                | "Equal"
                | "Minus"
                | "Period"
                | "Quote"
                | "Semicolon"
                | "Slash"
                | "IntlBackslash"
                | "Space"
                | "Enter"
                | "Tab"
                | "Backspace"
                | "Delete"
                | "Insert"
                | "Home"
                | "End"
                | "PageUp"
                | "PageDown"
                | "ArrowLeft"
                | "ArrowRight"
                | "ArrowUp"
                | "ArrowDown"
        )
}

/// 该平台有没有这个主键的键码（与 global-hotkey 各后端能注册的键一致：macOS 只到 F20、没有 PrintScreen / Pause /
/// 媒体键；Windows 没有 NumpadEqual / MediaPause 的虚拟键）
pub fn platform_has_key(os: OsKind, code: &str) -> bool {
    use wisp_platform::common::combo::{mac_keycode, win_vk, x11_keysym};
    match os {
        OsKind::Macos => mac_keycode(code).is_some(),
        OsKind::Windows => win_vk(code).is_some(),
        OsKind::Linux => x11_keysym(code).is_some(),
    }
}

/// 只按下了恰好这一个修饰键
fn only(m: &Modifiers, which: fn(&Modifiers) -> bool) -> bool {
    let count = [m.control, m.alt, m.shift, m.super_key].iter().filter(|b| **b).count();
    count == 1 && which(m)
}

/// 录制规则：只看组合本身与平台 / 布局，不看运行期状态
pub fn rules(os: OsKind, combo: &Combo, has_altgr: bool) -> Vec<Conflict> {
    use ConflictLevel::{Forbidden, Warning};
    use ConflictSource::{DeadKey, Layout, Reserved, StaticApp};
    let m = &combo.modifiers;
    let code = combo.code.as_str();
    let mut out = Vec::new();
    let letter = combo.letter().is_some();

    if !platform_has_key(os, code) {
        // 这个平台根本注册不了（例如 macOS 的裸 F21），不要等到注册时才失败
        out.push(conflict(Forbidden, Reserved, KEY_INVALID));
        return out;
    }
    if m.is_empty() {
        match function_key(code) {
            Some(13..=24) => {}
            Some(12) if os == OsKind::Windows => out.push(conflict(Forbidden, Reserved, KEY_F12)),
            Some(_) => out.push(conflict(Warning, StaticApp, KEY_BARE_KEY)),
            None => out.push(conflict(Forbidden, Reserved, KEY_MODIFIER_REQUIRED)),
        }
        return out;
    }
    if only(m, |m| m.shift) && is_typing_key(code) {
        let key = if letter { KEY_SHIFT_LETTER } else { KEY_SHIFT_KEY };
        out.push(conflict(Forbidden, Reserved, key));
        return out;
    }
    match os {
        OsKind::Macos => {
            if letter && (only(m, |m| m.super_key) || only(m, |m| m.control)) {
                out.push(conflict(Warning, StaticApp, KEY_SINGLE_LETTER));
            }
            if only(m, |m| m.alt) && matches!(code, "KeyE" | "KeyU" | "KeyI" | "KeyN" | "Backquote") {
                out.push(conflict_with(
                    Warning,
                    DeadKey,
                    KEY_DEAD_KEY,
                    &[("combo", super::display::display(&combo.to_canonical(), os))],
                ));
            }
        }
        OsKind::Windows | OsKind::Linux => {
            if letter && only(m, |m| m.control) {
                out.push(conflict(Warning, StaticApp, KEY_SINGLE_LETTER));
            }
            if letter && only(m, |m| m.alt) {
                out.push(conflict(Warning, StaticApp, KEY_ALT_LETTER));
            }
            if os == OsKind::Windows && m.super_key {
                out.push(conflict(Warning, Reserved, KEY_WIN_KEY));
            }
            if os == OsKind::Windows && has_altgr && m.control && m.alt && !m.super_key {
                out.push(conflict(Warning, Layout, KEY_LAYOUT));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(os: OsKind, raw: &str, altgr: bool) -> Vec<(ConflictLevel, String)> {
        let combo = parse(raw).expect("可解析");
        rules(os, &combo, altgr)
            .into_iter()
            .map(|c| (c.level, c.message_key))
            .collect()
    }

    #[test]
    fn normalize_canonicalizes_and_rejects() {
        assert_eq!(
            normalize("shift+control+alt+KeyS").as_deref(),
            Some("Control+Alt+Shift+KeyS")
        );
        assert_eq!(normalize(" Option + d ").as_deref(), Some("Alt+KeyD"));
        assert_eq!(normalize("Cmd+Shift+V").as_deref(), Some("Shift+Super+KeyV"));
        assert_eq!(normalize("F13").as_deref(), Some("F13"));
        assert_eq!(normalize("Control+Alt"), None);
        assert_eq!(parse("Control+Alt"), Err(Invalid::ModifierOnly));
        assert_eq!(parse("AltLeft+KeyD"), Err(Invalid::Unparsable));
        assert_eq!(parse("ShiftLeft"), Err(Invalid::ModifierOnly));
        assert_eq!(parse("Fn+KeyE"), Err(Invalid::Unparsable));
        assert_eq!(parse("Alt+KeyD+KeyS"), Err(Invalid::Unparsable));
        assert_eq!(parse(""), Err(Invalid::Unparsable));
        // Combo 认识、global-hotkey 不认识的主键：不能注册，按无法识别处理
        assert_eq!(parse("Alt+IntlBackslash"), Err(Invalid::Unparsable));
    }

    /// 每种禁止组合都被拒绝、每种警告组合都标黄、合法组合没有提示
    #[test]
    fn recorder_rules() {
        use ConflictLevel::{Forbidden, Warning};
        let f = |k: &str| (Forbidden, k.to_owned());
        let w = |k: &str| (Warning, k.to_owned());
        // 没有修饰键
        assert_eq!(keys(OsKind::Macos, "KeyA", false), [f(KEY_MODIFIER_REQUIRED)]);
        assert_eq!(keys(OsKind::Windows, "Space", false), [f(KEY_MODIFIER_REQUIRED)]);
        assert_eq!(keys(OsKind::Linux, "Escape", false), [f(KEY_MODIFIER_REQUIRED)]);
        // 裸 F 键：F13–F24 可以，F1–F11 警告，F12 Windows 禁止、其他平台警告
        for os in [OsKind::Macos, OsKind::Windows, OsKind::Linux] {
            assert!(keys(os, "F13", false).is_empty());
            assert!(keys(os, "F20", false).is_empty());
            assert_eq!(keys(os, "F1", false), [w(KEY_BARE_KEY)]);
            assert_eq!(keys(os, "F11", false), [w(KEY_BARE_KEY)]);
        }
        // F21–F24：Windows / X11 可以注册；global-hotkey 的 macOS 后端没有它们的键码，注册必然失败
        for key in ["F21", "F22", "F23", "F24"] {
            assert!(keys(OsKind::Windows, key, false).is_empty(), "{key}");
            assert!(keys(OsKind::Linux, key, false).is_empty(), "{key}");
            assert_eq!(keys(OsKind::Macos, key, false), [f(KEY_INVALID)], "{key}");
            assert_eq!(
                keys(OsKind::Macos, &format!("Control+Alt+{key}"), false),
                [f(KEY_INVALID)]
            );
        }
        assert_eq!(keys(OsKind::Windows, "F12", false), [f(KEY_F12)]);
        assert_eq!(keys(OsKind::Macos, "F12", false), [w(KEY_BARE_KEY)]);
        assert_eq!(keys(OsKind::Linux, "F12", false), [w(KEY_BARE_KEY)]);
        // ⇧ + 字母（例如 ⇧A 不被接受）与其他影响输入的键
        assert_eq!(keys(OsKind::Macos, "Shift+KeyA", false), [f(KEY_SHIFT_LETTER)]);
        assert_eq!(keys(OsKind::Windows, "Shift+Digit1", false), [f(KEY_SHIFT_KEY)]);
        assert_eq!(keys(OsKind::Linux, "Shift+ArrowLeft", false), [f(KEY_SHIFT_KEY)]);
        assert!(keys(OsKind::Windows, "Shift+F5", false).is_empty());
        // ⌘ / ⌃ + 单字母（例如 ⌘K 显示黄色警告）
        assert_eq!(keys(OsKind::Macos, "Super+KeyK", false), [w(KEY_SINGLE_LETTER)]);
        assert_eq!(keys(OsKind::Macos, "Control+KeyD", false), [w(KEY_SINGLE_LETTER)]);
        assert_eq!(keys(OsKind::Windows, "Control+KeyK", false), [w(KEY_SINGLE_LETTER)]);
        assert!(keys(OsKind::Macos, "Super+Digit1", false).is_empty());
        // Alt + 字母：Windows / Linux 警告，macOS 是默认族
        assert_eq!(keys(OsKind::Windows, "Alt+KeyD", false), [w(KEY_ALT_LETTER)]);
        assert_eq!(keys(OsKind::Linux, "Alt+KeyD", false), [w(KEY_ALT_LETTER)]);
        assert!(keys(OsKind::Macos, "Alt+KeyD", false).is_empty());
        // Win + …
        assert_eq!(keys(OsKind::Windows, "Shift+Super+KeyV", false), [w(KEY_WIN_KEY)]);
        assert!(keys(OsKind::Linux, "Alt+Super+KeyD", false).is_empty());
        // AltGr 布局下的 Ctrl + Alt
        assert_eq!(keys(OsKind::Windows, "Control+Alt+KeyD", true), [w(KEY_LAYOUT)]);
        assert!(keys(OsKind::Windows, "Control+Alt+KeyD", false).is_empty());
        assert!(keys(OsKind::Macos, "Control+Alt+KeyD", true).is_empty());
        // 死键（⌥E 录成 Alt+KeyE 并有死键警告）
        assert_eq!(keys(OsKind::Macos, "Alt+KeyE", false), [w(KEY_DEAD_KEY)]);
        assert_eq!(keys(OsKind::Macos, "Alt+Backquote", false), [w(KEY_DEAD_KEY)]);
        assert!(keys(OsKind::Macos, "Alt+Shift+KeyE", false).is_empty());
        assert!(keys(OsKind::Windows, "Control+Alt+KeyE", false).is_empty());
    }

    /// 平台没有键码的主键一律禁止（与 global-hotkey 各后端的键表一致）
    #[test]
    fn keys_missing_on_a_platform_are_forbidden() {
        let invalid = [(ConflictLevel::Forbidden, KEY_INVALID.to_owned())];
        for raw in [
            "Alt+PrintScreen",
            "Alt+Pause",
            "Control+Alt+MediaPlayPause",
            "Alt+ScrollLock",
        ] {
            assert_eq!(keys(OsKind::Macos, raw, false), invalid, "{raw}");
        }
        assert_eq!(keys(OsKind::Windows, "Control+Alt+NumpadEqual", false), invalid);
        assert!(keys(OsKind::Macos, "Control+Alt+NumpadEqual", false).is_empty());
        assert!(keys(OsKind::Windows, "Control+Alt+Pause", false).is_empty());
        assert!(keys(OsKind::Linux, "Control+Alt+MediaPlayPause", false).is_empty());
        // 三端默认表用到的主键在各自平台上都有键码
        for os in [OsKind::Macos, OsKind::Windows, OsKind::Linux] {
            for d in wisp_types::defaults::hotkey_table(os) {
                for raw in [d.primary, d.fallback] {
                    let c = parse(raw).expect("默认值可解析");
                    assert!(platform_has_key(os, &c.code), "{os:?} {raw}");
                }
            }
        }
    }

    #[test]
    fn dead_key_names_the_combo() {
        let c = rules(OsKind::Macos, &parse("Alt+KeyU").expect("可解析"), false);
        assert_eq!(c[0].params.get("combo").map(String::as_str), Some("⌥U"));
    }

    /// 三端默认表里的组合都不触发禁止级规则
    #[test]
    fn defaults_pass_recorder_rules() {
        for os in [OsKind::Macos, OsKind::Windows, OsKind::Linux] {
            for d in wisp_types::defaults::hotkey_table(os) {
                for raw in [d.primary, d.fallback] {
                    let combo = parse(raw).expect("默认值可解析");
                    assert_eq!(combo.to_canonical(), raw, "默认表是规范串");
                    assert!(
                        rules(os, &combo, false)
                            .iter()
                            .all(|c| c.level != ConflictLevel::Forbidden),
                        "{os:?} {raw}"
                    );
                }
            }
        }
    }
}
