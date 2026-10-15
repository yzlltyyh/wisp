//! 组合键的显示格式：macOS 用 ⌃⌥⇧⌘ 符号、无分隔；Windows / Linux 用文字、`+` 分隔，顺序
//! Win（Linux 为 Super）、Ctrl、Alt、Shift。
//!
//! 与前端 `formatHotkey(combo, os)`（`src/shared/keys/formatHotkey.ts`）逐字符一致，两边共用测试用例
//! `src/shared/keys/__fixtures__/format-cases.json`。解析同样宽松（与 `shared/keys/combo.ts` 的 `parseCombo` 相同）：
//! 修饰键别名、`Mod` / `CmdOrCtrl` 按平台展开、单字符主键、未知主键原样显示——显示函数永不失败，
//! 托盘与设置页拿到任何串都能显示出东西。

use wisp_types::OsKind;

/// 宽松解析后的组合（与 TS `ParsedCombo` 同构）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Parsed {
    ctrl: bool,
    alt: bool,
    shift: bool,
    /// macOS ⌘ / Windows Win / Linux Super
    meta: bool,
    /// 主键（KeyboardEvent.code 形式）；只有修饰键时为空串
    code: String,
}

/// 修饰键（`CmdOrCtrl` 一族按平台展开）
#[derive(Clone, Copy)]
enum Mod {
    Ctrl,
    Alt,
    Shift,
    Meta,
    CmdOrCtrl,
}

fn modifier(token: &str) -> Option<Mod> {
    Some(match token.to_ascii_lowercase().as_str() {
        "control" | "ctrl" => Mod::Ctrl,
        "alt" | "option" | "opt" => Mod::Alt,
        "shift" => Mod::Shift,
        "super" | "meta" | "cmd" | "command" | "win" | "windows" => Mod::Meta,
        "mod" | "cmdorctrl" | "cmdorcontrol" | "commandorctrl" | "commandorcontrol" => Mod::CmdOrCtrl,
        _ => return None,
    })
}

/// 主键别名（小写）→ KeyboardEvent.code（与 TS `KEY_ALIASES` 相同）
fn key_alias(lower: &str) -> Option<&'static str> {
    Some(match lower {
        "esc" | "escape" => "Escape",
        "return" | "enter" => "Enter",
        "space" | "spacebar" | " " => "Space",
        "tab" => "Tab",
        "backspace" => "Backspace",
        "delete" | "del" => "Delete",
        "up" | "arrowup" => "ArrowUp",
        "down" | "arrowdown" => "ArrowDown",
        "left" | "arrowleft" => "ArrowLeft",
        "right" | "arrowright" => "ArrowRight",
        "home" => "Home",
        "end" => "End",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        "insert" => "Insert",
        "printscreen" => "PrintScreen",
        "capslock" => "CapsLock",
        "," | "comma" => "Comma",
        "." | "period" => "Period",
        "/" | "slash" => "Slash",
        "\\" | "backslash" => "Backslash",
        ";" | "semicolon" => "Semicolon",
        "'" | "quote" => "Quote",
        "[" | "bracketleft" => "BracketLeft",
        "]" | "bracketright" => "BracketRight",
        "-" | "minus" => "Minus",
        "=" | "equal" => "Equal",
        "`" | "backquote" => "Backquote",
        _ => return None,
    })
}

/// 主键规范化为 KeyboardEvent.code；无法识别时原样返回（与 TS `normalizeKey` 相同）
fn normalize_key(token: &str) -> String {
    if token.is_empty() {
        return String::new();
    }
    let lower = token.to_ascii_lowercase();
    if let Some(alias) = key_alias(&lower) {
        return alias.to_owned();
    }
    let bytes = token.as_bytes();
    if bytes.len() == 1 && bytes[0].is_ascii_alphabetic() {
        return format!("Key{}", token.to_ascii_uppercase());
    }
    if bytes.len() == 1 && bytes[0].is_ascii_digit() {
        return format!("Digit{token}");
    }
    if bytes.len() == 4 && lower.starts_with("key") && bytes[3].is_ascii_alphabetic() {
        return format!("Key{}", lower[3..].to_ascii_uppercase());
    }
    if bytes.len() == 6 && lower.starts_with("digit") && bytes[5].is_ascii_digit() {
        return format!("Digit{}", &lower[5..]);
    }
    if is_function_key_name(&lower) {
        return lower.to_ascii_uppercase();
    }
    if lower.starts_with("numpad") && token.is_char_boundary(6) {
        return format!("Numpad{}", &token[6..]);
    }
    token.to_owned()
}

/// `f1`–`f24`（小写，不接受前导 0）
fn is_function_key_name(lower: &str) -> bool {
    lower
        .strip_prefix('f')
        .filter(|n| !n.is_empty() && !n.starts_with('0') && n.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|n| n.parse::<u32>().ok())
        .is_some_and(|v| (1..=24).contains(&v))
}

/// 宽松解析（大小写不敏感，`+` 分隔；`Mod` 按平台展开；`Ctrl++` 的主键是 `+`）
fn parse(combo: &str, os: OsKind) -> Parsed {
    let mut out = Parsed::default();
    let (body, plus_key) = match combo.strip_suffix("++") {
        Some(rest) => (rest, true),
        None => (combo, false),
    };
    let tokens = body.split('+').chain(plus_key.then_some("+"));
    for raw in tokens {
        let token = raw.trim();
        if token.is_empty() {
            continue;
        }
        match modifier(token) {
            Some(Mod::Ctrl) => out.ctrl = true,
            Some(Mod::Alt) => out.alt = true,
            Some(Mod::Shift) => out.shift = true,
            Some(Mod::Meta) => out.meta = true,
            Some(Mod::CmdOrCtrl) if os == OsKind::Macos => out.meta = true,
            Some(Mod::CmdOrCtrl) => out.ctrl = true,
            None if token == "+" => out.code = "Equal".to_owned(),
            None => out.code = normalize_key(token),
        }
    }
    out
}

/// macOS 的特殊键符号
fn mac_key(code: &str) -> Option<&'static str> {
    Some(match code {
        "Escape" => "⎋",
        "Tab" => "⇥",
        "Backspace" => "⌫",
        "Delete" => "⌦",
        "Enter" => "↩",
        "ArrowUp" => "↑",
        "ArrowDown" => "↓",
        "ArrowLeft" => "←",
        "ArrowRight" => "→",
        "PageUp" => "⇞",
        "PageDown" => "⇟",
        "Home" => "↖",
        "End" => "↘",
        "CapsLock" => "⇪",
        "Space" => "Space",
        _ => return None,
    })
}

/// Windows / Linux 的特殊键文字
fn text_key(code: &str) -> Option<&'static str> {
    Some(match code {
        "Escape" => "Esc",
        "Tab" => "Tab",
        "Backspace" => "Backspace",
        "Delete" => "Delete",
        "Enter" => "Enter",
        "ArrowUp" => "↑",
        "ArrowDown" => "↓",
        "ArrowLeft" => "←",
        "ArrowRight" => "→",
        "PageUp" => "PgUp",
        "PageDown" => "PgDn",
        "Home" => "Home",
        "End" => "End",
        "Insert" => "Ins",
        "PrintScreen" => "PrtSc",
        "CapsLock" => "CapsLock",
        "Space" => "Space",
        _ => return None,
    })
}

/// 标点与小键盘运算符（三端相同）
fn punct(code: &str) -> Option<&'static str> {
    Some(match code {
        "Comma" => ",",
        "Period" | "NumpadDecimal" => ".",
        "Slash" | "NumpadDivide" => "/",
        "Backslash" => "\\",
        "Semicolon" => ";",
        "Quote" => "'",
        "BracketLeft" => "[",
        "BracketRight" => "]",
        "Minus" | "NumpadSubtract" => "-",
        "Equal" | "NumpadEqual" => "=",
        "Backquote" => "`",
        "NumpadAdd" => "+",
        "NumpadMultiply" => "*",
        _ => return None,
    })
}

/// 主键的显示文本（与 TS `keyLabel` 相同）
pub fn key_label(code: &str, os: OsKind) -> String {
    if code.len() == 4
        && let Some(letter) = code.strip_prefix("Key")
    {
        return letter.to_owned();
    }
    if code.len() == 6
        && let Some(digit) = code.strip_prefix("Digit")
    {
        return digit.to_owned();
    }
    if let Some(n) = code.strip_prefix("Numpad")
        && n.len() == 1
        && n.as_bytes()[0].is_ascii_digit()
    {
        return n.to_owned();
    }
    let special = if os == OsKind::Macos {
        mac_key(code)
    } else {
        text_key(code)
    };
    special
        .or_else(|| punct(code))
        .map_or_else(|| code.to_owned(), str::to_owned)
}

/// 逐个键帽的文本（与 TS `hotkeyParts` 相同）
pub fn parts(combo: &str, os: OsKind) -> Vec<String> {
    let p = parse(combo, os);
    let mods: [(bool, &str); 4] = match os {
        OsKind::Macos => [(p.ctrl, "⌃"), (p.alt, "⌥"), (p.shift, "⇧"), (p.meta, "⌘")],
        OsKind::Windows => [(p.meta, "Win"), (p.ctrl, "Ctrl"), (p.alt, "Alt"), (p.shift, "Shift")],
        OsKind::Linux => [(p.meta, "Super"), (p.ctrl, "Ctrl"), (p.alt, "Alt"), (p.shift, "Shift")],
    };
    let mut out: Vec<String> = mods
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, label)| (*label).to_owned())
        .collect();
    if !p.code.is_empty() {
        out.push(key_label(&p.code, os));
    }
    out
}

/// 把 global-hotkey 规范串格式化为显示串：macOS `⌥⇧D`，Windows `Ctrl+Alt+Shift+D`，Linux `Super+Alt+D`
pub fn display(combo: &str, os: OsKind) -> String {
    parts(combo, os).join(if os == OsKind::Macos { "" } else { "+" })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与前端共用的 fixture
    const CASES: &str = include_str!("../../../src/shared/keys/__fixtures__/format-cases.json");

    #[derive(serde::Deserialize)]
    struct Case {
        combo: String,
        macos: String,
        windows: String,
        linux: String,
    }

    /// Rust 与 TS 的格式化逐字符一致（三端各跑一遍全部用例）
    #[test]
    fn matches_shared_fixture() {
        let cases: Vec<Case> = serde_json::from_str(CASES).expect("fixture 是合法 JSON");
        assert!(cases.len() >= 30, "fixture 至少 30 例，实际 {}", cases.len());
        for c in &cases {
            assert_eq!(display(&c.combo, OsKind::Macos), c.macos, "macOS：{}", c.combo);
            assert_eq!(display(&c.combo, OsKind::Windows), c.windows, "Windows：{}", c.combo);
            assert_eq!(display(&c.combo, OsKind::Linux), c.linux, "Linux：{}", c.combo);
        }
    }

    /// 默认表里的每个组合在三端都能显示为非空串，且不含规范串的残留（Key / Digit 前缀）
    #[test]
    fn defaults_display_cleanly() {
        for os in [OsKind::Macos, OsKind::Windows, OsKind::Linux] {
            for d in wisp_types::defaults::hotkey_table(os) {
                for combo in [d.primary, d.fallback] {
                    let s = display(combo, os);
                    assert!(
                        !s.is_empty() && !s.contains("Key") && !s.contains("Digit"),
                        "{os:?} {combo} → {s}"
                    );
                }
            }
        }
        assert_eq!(display("Alt+KeyD", OsKind::Macos), "⌥D");
        assert_eq!(display("Control+Alt+KeyD", OsKind::Windows), "Ctrl+Alt+D");
        assert_eq!(display("Alt+Super+KeyD", OsKind::Linux), "Super+Alt+D");
    }

    /// 宽松解析：别名、大小写、`Mod` 展开、`+` 主键、未知主键原样显示
    #[test]
    fn lenient_parsing() {
        assert_eq!(display("cmd+option+k", OsKind::Macos), "⌥⌘K");
        assert_eq!(display("Mod+Shift+P", OsKind::Windows), "Ctrl+Shift+P");
        assert_eq!(display("Mod+Shift+P", OsKind::Macos), "⇧⌘P");
        assert_eq!(display("Ctrl++", OsKind::Linux), "Ctrl+=");
        assert_eq!(display("Alt+F13", OsKind::Windows), "Alt+F13");
        assert_eq!(display("Alt+f5", OsKind::Macos), "⌥F5");
        assert_eq!(display("Alt+f05", OsKind::Macos), "⌥f05");
        assert_eq!(display("Control+Numpad7", OsKind::Windows), "Ctrl+7");
        assert_eq!(display("Control+NumpadAdd", OsKind::Macos), "⌃+");
        assert_eq!(display("Alt+IntlBackslash", OsKind::Windows), "Alt+IntlBackslash");
        assert_eq!(display("Shift+Insert", OsKind::Macos), "⇧Insert");
        assert_eq!(display("Shift+Insert", OsKind::Linux), "Shift+Ins");
        assert_eq!(display("alt+keyd", OsKind::Macos), "⌥D");
        assert_eq!(display("", OsKind::Macos), "");
        assert_eq!(parts("Control+Alt+KeyD", OsKind::Windows), ["Ctrl", "Alt", "D"]);
    }
}
