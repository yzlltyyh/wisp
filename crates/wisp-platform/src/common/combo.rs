//! global-hotkey 字符串与修饰键位、键码之间的转换。规范串的修饰键顺序固定为 Control+Alt+Shift+Super。
//!
//! **全仓库唯一的规范化入口**：任何组合键比较（冲突检测、`on_shortcut` 匹配、默认表查重、设置项比对）都必须先经过
//! [`canonical`]（或 [`Combo::parse`]）解析成（修饰键集合, 主键 code）再比较，禁止直接比较原始字符串——
//! global-hotkey 0.8 的 `HotKey::into_string()` 输出小写、`shift+control+alt+super` 顺序的串，与规范串不同。
//!
//! 解析规则与 global-hotkey 0.8 的 `parse_hotkey` 一致并略宽松：
//! - 以 `+` 分隔，各段两端空白忽略，大小写不敏感；修饰键必须写在主键之前，主键恰好一个；
//! - 修饰键别名：`Option` / `Alt`、`Control` / `Ctrl`、`Command` / `Cmd` / `Super` / `Meta`、`Shift`，
//!   以及 `CmdOrCtrl` 一族（macOS 为 Super，其他平台为 Control）；
//! - 主键接受 W3C `KeyboardEvent.code` 名（`KeyD`、`Digit1`、`ArrowUp`…）与 global-hotkey 的简写（`D`、`1`、`Up`、`Esc`…）；
//! - 只有一个主键、没有修饰键也合法（裸 F 键）；纯修饰键、空段、多个主键都返回 None。
//!
//! 键码表同时给出三端的虚拟键码：macOS `kVK_*`（ANSI 物理位置，与布局无关）、Windows `VK_*`、X11 keysym，
//! 供 CopySymbolicHotKeys 转换与按映射表发键使用。

/// 修饰键集合
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Modifiers {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    /// macOS ⌘ / Windows ⊞ / Linux Super
    pub super_key: bool,
}

/// Carbon 修饰位（`Events.h`）：cmdKey
pub const CARBON_CMD: u32 = 1 << 8;
/// Carbon 修饰位：shiftKey
pub const CARBON_SHIFT: u32 = 1 << 9;
/// Carbon 修饰位：optionKey
pub const CARBON_OPTION: u32 = 1 << 11;
/// Carbon 修饰位：controlKey
pub const CARBON_CONTROL: u32 = 1 << 12;
/// Carbon 修饰位：rightShiftKey / rightOptionKey / rightControlKey（与左侧同义）
pub const CARBON_RIGHT_SHIFT: u32 = 1 << 13;
pub const CARBON_RIGHT_OPTION: u32 = 1 << 14;
pub const CARBON_RIGHT_CONTROL: u32 = 1 << 15;
/// Carbon 修饰位：kEventKeyModifierFnMask。CopySymbolicHotKeys 的条目用它表示 Fn / 🌐（macOS 26.4 实测）
pub const CARBON_FN: u32 = 1 << 17;
/// NSEvent / CGEvent 风格的 Fn 位（`NSEventModifierFlagFunction`），部分来源的条目会用它表示 Fn
pub const SYMBOLIC_FN_BIT: u32 = 0x0080_0000;

impl Modifiers {
    /// 没有任何修饰键
    pub const NONE: Self = Self {
        control: false,
        alt: false,
        shift: false,
        super_key: false,
    };

    /// 是否一个修饰键都没有
    pub const fn is_empty(&self) -> bool {
        !self.control && !self.alt && !self.shift && !self.super_key
    }

    /// 转成 Carbon 修饰位（`RegisterEventHotKey` / `CopySymbolicHotKeys` 使用）
    pub const fn to_carbon(&self) -> u32 {
        let mut bits = 0;
        if self.control {
            bits |= CARBON_CONTROL;
        }
        if self.alt {
            bits |= CARBON_OPTION;
        }
        if self.shift {
            bits |= CARBON_SHIFT;
        }
        if self.super_key {
            bits |= CARBON_CMD;
        }
        bits
    }

    /// 从 Carbon 修饰位解析（左右同义）；未知位（Fn 位、CapsLock 位等）忽略
    pub const fn from_carbon(bits: u32) -> Self {
        Self {
            control: bits & (CARBON_CONTROL | CARBON_RIGHT_CONTROL) != 0,
            alt: bits & (CARBON_OPTION | CARBON_RIGHT_OPTION) != 0,
            shift: bits & (CARBON_SHIFT | CARBON_RIGHT_SHIFT) != 0,
            super_key: bits & CARBON_CMD != 0,
        }
    }
}

/// 解析后的组合键
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Combo {
    pub modifiers: Modifiers,
    /// W3C `KeyboardEvent.code` 风格的主键名，例如 "KeyD"、"Digit1"、"Space"
    pub code: String,
}

impl Combo {
    /// 用修饰键与主键名（W3C code 或别名）构造；不认识的主键返回 None
    pub fn new(modifiers: Modifiers, code: &str) -> Option<Self> {
        normalize_code(code).map(|c| Self {
            modifiers,
            code: c.to_owned(),
        })
    }

    /// 解析 global-hotkey 字符串（大小写与修饰键顺序宽松）；无法解析时返回 None
    pub fn parse(s: &str) -> Option<Self> {
        let mut modifiers = Modifiers::NONE;
        let mut code: Option<&'static str> = None;
        for token in s.split('+').map(str::trim) {
            if token.is_empty() || code.is_some() {
                // 空段（"Ctrl++"、"+D"）或主键之后还有内容（"Ctrl+C+A"、"C+Shift"）
                return None;
            }
            if !apply_modifier(&mut modifiers, token) {
                code = Some(normalize_code(token)?);
            }
        }
        code.map(|c| Self {
            modifiers,
            code: c.to_owned(),
        })
    }

    /// 输出规范串（修饰键顺序 Control+Alt+Shift+Super）
    pub fn to_canonical(&self) -> String {
        let mut parts: Vec<&str> = Vec::with_capacity(5);
        if self.modifiers.control {
            parts.push("Control");
        }
        if self.modifiers.alt {
            parts.push("Alt");
        }
        if self.modifiers.shift {
            parts.push("Shift");
        }
        if self.modifiers.super_key {
            parts.push("Super");
        }
        parts.push(&self.code);
        parts.join("+")
    }

    /// 由 macOS 虚拟键码与 Carbon 修饰位构造（CopySymbolicHotKeys 的条目）。
    ///
    /// Fn 位的处理：F 键、方向键与导航键的事件天然带 Fn 位（系统合成的），剥离后照常比较；其他键带 Fn 位表示
    /// 必须按住 Fn / 🌐（例如 🌐C 控制中心、🌐F 全屏），global-hotkey 无法注册含 Fn 的组合，不可能与之冲突，返回 None。
    /// 未知键码（亮度、调度中心等专用键）同样返回 None
    pub fn from_mac(keycode: u16, carbon_modifiers: u32) -> Option<Self> {
        let code = code_from_mac_keycode(keycode)?;
        let has_fn = carbon_modifiers & (CARBON_FN | SYMBOLIC_FN_BIT) != 0;
        if has_fn && !fn_is_synthesized(code) {
            return None;
        }
        Some(Self {
            modifiers: Modifiers::from_carbon(carbon_modifiers & !(CARBON_FN | SYMBOLIC_FN_BIT)),
            code: code.to_owned(),
        })
    }

    /// 主键的 macOS 虚拟键码（ANSI 物理位置；字母键在非 QWERTY 布局下应改用按布局反查的结果）
    pub fn mac_keycode(&self) -> Option<u16> {
        mac_keycode(&self.code)
    }

    /// 主键的 Windows 虚拟键码
    pub fn win_vk(&self) -> Option<u16> {
        win_vk(&self.code)
    }

    /// 主键的 X11 keysym
    pub fn x11_keysym(&self) -> Option<u32> {
        x11_keysym(&self.code)
    }

    /// 主键是字母键（`KeyA`–`KeyZ`）时返回对应的小写字母
    pub fn letter(&self) -> Option<char> {
        letter_of(&self.code)
    }
}

/// 可按值复制的组合键：主键是键码表里的静态名。钩子规格按值传递、钩子回调里逐键比较，不能带堆上的字符串，
/// 所以用它而不是 [`Combo`]；两者可以互相转换
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyChord {
    pub modifiers: Modifiers,
    /// W3C `KeyboardEvent.code` 风格的主键名（键码表里的静态串）
    pub code: &'static str,
}

impl KeyChord {
    /// 由 [`Combo`] 得出；主键不在键码表里时为 None
    pub fn of(combo: &Combo) -> Option<Self> {
        normalize_code(&combo.code).map(|code| Self {
            modifiers: combo.modifiers,
            code,
        })
    }

    /// 转回 [`Combo`]（发键、格式化用）
    pub fn to_combo(self) -> Combo {
        Combo {
            modifiers: self.modifiers,
            code: self.code.to_owned(),
        }
    }
}

/// 把任意 global-hotkey 风格的字符串规范化为规范串（修饰键顺序 Control+Alt+Shift+Super，主键为 W3C code）；
/// 无法解析时返回 None。所有组合键比较都以它的输出为准
pub fn canonical(s: &str) -> Option<String> {
    Combo::parse(s).map(|c| c.to_canonical())
}

/// 两个组合键串是否等价（都能解析且解析结果相同）
pub fn same_combo(a: &str, b: &str) -> bool {
    match (Combo::parse(a), Combo::parse(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// 若 token 是修饰键则记入 `m` 并返回 true
fn apply_modifier(m: &mut Modifiers, token: &str) -> bool {
    const ALT: &[&str] = &["option", "alt"];
    const CONTROL: &[&str] = &["control", "ctrl"];
    const SUPER: &[&str] = &["command", "cmd", "super", "meta"];
    const CMD_OR_CTRL: &[&str] = &["commandorcontrol", "commandorctrl", "cmdorctrl", "cmdorcontrol"];
    let is = |names: &[&str]| names.iter().any(|n| n.eq_ignore_ascii_case(token));
    if is(ALT) {
        m.alt = true;
    } else if is(CONTROL) {
        m.control = true;
    } else if is(SUPER) {
        m.super_key = true;
    } else if token.eq_ignore_ascii_case("shift") {
        m.shift = true;
    } else if is(CMD_OR_CTRL) {
        if cfg!(target_os = "macos") {
            m.super_key = true;
        } else {
            m.control = true;
        }
    } else {
        return false;
    }
    true
}

/// 键码表的一行
#[derive(Debug)]
struct KeyDef {
    /// W3C code
    code: &'static str,
    /// global-hotkey 接受的其他写法（大小写不敏感）
    aliases: &'static [&'static str],
    /// macOS `kVK_*`
    mac: Option<u16>,
    /// Windows `VK_*`
    win: Option<u16>,
    /// X11 keysym
    x11: Option<u32>,
}

/// 构造键码表的一行：`(code, [别名…], mac, win, x11)`，三个键码用 0 表示「该平台没有」
macro_rules! keys {
    ($(($code:literal, [$($alias:literal),*], $mac:literal, $win:literal, $x11:literal)),* $(,)?) => {
        &[$(KeyDef {
            code: $code,
            aliases: &[$($alias),*],
            mac: if $mac == 0xFFFF { None } else { Some($mac) },
            win: if $win == 0 { None } else { Some($win) },
            x11: if $x11 == 0 { None } else { Some($x11) },
        }),*]
    };
}

/// 主键表：W3C code、别名与三端键码。macOS 列用 0xFFFF 表示没有（0 是 `kVK_ANSI_A`），其余两列用 0
static KEYS: &[KeyDef] = keys![
    ("KeyA", ["A"], 0x00, 0x41, 0x61),
    ("KeyB", ["B"], 0x0B, 0x42, 0x62),
    ("KeyC", ["C"], 0x08, 0x43, 0x63),
    ("KeyD", ["D"], 0x02, 0x44, 0x64),
    ("KeyE", ["E"], 0x0E, 0x45, 0x65),
    ("KeyF", ["F"], 0x03, 0x46, 0x66),
    ("KeyG", ["G"], 0x05, 0x47, 0x67),
    ("KeyH", ["H"], 0x04, 0x48, 0x68),
    ("KeyI", ["I"], 0x22, 0x49, 0x69),
    ("KeyJ", ["J"], 0x26, 0x4A, 0x6A),
    ("KeyK", ["K"], 0x28, 0x4B, 0x6B),
    ("KeyL", ["L"], 0x25, 0x4C, 0x6C),
    ("KeyM", ["M"], 0x2E, 0x4D, 0x6D),
    ("KeyN", ["N"], 0x2D, 0x4E, 0x6E),
    ("KeyO", ["O"], 0x1F, 0x4F, 0x6F),
    ("KeyP", ["P"], 0x23, 0x50, 0x70),
    ("KeyQ", ["Q"], 0x0C, 0x51, 0x71),
    ("KeyR", ["R"], 0x0F, 0x52, 0x72),
    ("KeyS", ["S"], 0x01, 0x53, 0x73),
    ("KeyT", ["T"], 0x11, 0x54, 0x74),
    ("KeyU", ["U"], 0x20, 0x55, 0x75),
    ("KeyV", ["V"], 0x09, 0x56, 0x76),
    ("KeyW", ["W"], 0x0D, 0x57, 0x77),
    ("KeyX", ["X"], 0x07, 0x58, 0x78),
    ("KeyY", ["Y"], 0x10, 0x59, 0x79),
    ("KeyZ", ["Z"], 0x06, 0x5A, 0x7A),
    ("Digit0", ["0"], 0x1D, 0x30, 0x30),
    ("Digit1", ["1"], 0x12, 0x31, 0x31),
    ("Digit2", ["2"], 0x13, 0x32, 0x32),
    ("Digit3", ["3"], 0x14, 0x33, 0x33),
    ("Digit4", ["4"], 0x15, 0x34, 0x34),
    ("Digit5", ["5"], 0x17, 0x35, 0x35),
    ("Digit6", ["6"], 0x16, 0x36, 0x36),
    ("Digit7", ["7"], 0x1A, 0x37, 0x37),
    ("Digit8", ["8"], 0x1C, 0x38, 0x38),
    ("Digit9", ["9"], 0x19, 0x39, 0x39),
    ("Backquote", ["`"], 0x32, 0xC0, 0x60),
    ("Backslash", ["\\"], 0x2A, 0xDC, 0x5C),
    ("BracketLeft", ["["], 0x21, 0xDB, 0x5B),
    ("BracketRight", ["]"], 0x1E, 0xDD, 0x5D),
    ("Comma", [","], 0x2B, 0xBC, 0x2C),
    ("Equal", ["="], 0x18, 0xBB, 0x3D),
    ("Minus", ["-"], 0x1B, 0xBD, 0x2D),
    ("Period", ["."], 0x2F, 0xBE, 0x2E),
    ("Quote", ["'"], 0x27, 0xDE, 0x27),
    ("Semicolon", [";"], 0x29, 0xBA, 0x3B),
    ("Slash", ["/"], 0x2C, 0xBF, 0x2F),
    ("IntlBackslash", [], 0x0A, 0xE2, 0x3C),
    ("Space", [], 0x31, 0x20, 0x20),
    ("Enter", ["Return"], 0x24, 0x0D, 0xFF0D),
    ("Tab", [], 0x30, 0x09, 0xFF09),
    ("Backspace", [], 0x33, 0x08, 0xFF08),
    ("Delete", [], 0x75, 0x2E, 0xFFFF),
    ("Escape", ["Esc"], 0x35, 0x1B, 0xFF1B),
    ("Insert", [], 0x72, 0x2D, 0xFF63),
    ("Home", [], 0x73, 0x24, 0xFF50),
    ("End", [], 0x77, 0x23, 0xFF57),
    ("PageUp", [], 0x74, 0x21, 0xFF55),
    ("PageDown", [], 0x79, 0x22, 0xFF56),
    ("ArrowLeft", ["Left"], 0x7B, 0x25, 0xFF51),
    ("ArrowUp", ["Up"], 0x7E, 0x26, 0xFF52),
    ("ArrowRight", ["Right"], 0x7C, 0x27, 0xFF53),
    ("ArrowDown", ["Down"], 0x7D, 0x28, 0xFF54),
    ("CapsLock", [], 0x39, 0x14, 0xFFE5),
    ("NumLock", [], 0x47, 0x90, 0xFF7F),
    ("ScrollLock", [], 0xFFFF, 0x91, 0xFF14),
    ("PrintScreen", [], 0xFFFF, 0x2C, 0xFF61),
    ("Pause", ["PauseBreak"], 0xFFFF, 0x13, 0xFF13),
    ("Numpad0", ["Num0"], 0x52, 0x60, 0xFFB0),
    ("Numpad1", ["Num1"], 0x53, 0x61, 0xFFB1),
    ("Numpad2", ["Num2"], 0x54, 0x62, 0xFFB2),
    ("Numpad3", ["Num3"], 0x55, 0x63, 0xFFB3),
    ("Numpad4", ["Num4"], 0x56, 0x64, 0xFFB4),
    ("Numpad5", ["Num5"], 0x57, 0x65, 0xFFB5),
    ("Numpad6", ["Num6"], 0x58, 0x66, 0xFFB6),
    ("Numpad7", ["Num7"], 0x59, 0x67, 0xFFB7),
    ("Numpad8", ["Num8"], 0x5B, 0x68, 0xFFB8),
    ("Numpad9", ["Num9"], 0x5C, 0x69, 0xFFB9),
    ("NumpadAdd", ["NumAdd", "NumpadPlus", "NumPlus"], 0x45, 0x6B, 0xFFAB),
    ("NumpadDecimal", ["NumDecimal"], 0x41, 0x6E, 0xFFAE),
    ("NumpadDivide", ["NumDivide"], 0x4B, 0x6F, 0xFFAF),
    ("NumpadEnter", ["NumEnter"], 0x4C, 0x0D, 0xFF8D),
    ("NumpadEqual", ["NumEqual"], 0x51, 0, 0xFFBD),
    ("NumpadMultiply", ["NumMultiply"], 0x43, 0x6A, 0xFFAA),
    ("NumpadSubtract", ["NumSubtract"], 0x4E, 0x6D, 0xFFAD),
    ("F1", [], 0x7A, 0x70, 0xFFBE),
    ("F2", [], 0x78, 0x71, 0xFFBF),
    ("F3", [], 0x63, 0x72, 0xFFC0),
    ("F4", [], 0x76, 0x73, 0xFFC1),
    ("F5", [], 0x60, 0x74, 0xFFC2),
    ("F6", [], 0x61, 0x75, 0xFFC3),
    ("F7", [], 0x62, 0x76, 0xFFC4),
    ("F8", [], 0x64, 0x77, 0xFFC5),
    ("F9", [], 0x65, 0x78, 0xFFC6),
    ("F10", [], 0x6D, 0x79, 0xFFC7),
    ("F11", [], 0x67, 0x7A, 0xFFC8),
    ("F12", [], 0x6F, 0x7B, 0xFFC9),
    ("F13", [], 0x69, 0x7C, 0xFFCA),
    ("F14", [], 0x6B, 0x7D, 0xFFCB),
    ("F15", [], 0x71, 0x7E, 0xFFCC),
    ("F16", [], 0x6A, 0x7F, 0xFFCD),
    ("F17", [], 0x40, 0x80, 0xFFCE),
    ("F18", [], 0x4F, 0x81, 0xFFCF),
    ("F19", [], 0x50, 0x82, 0xFFD0),
    ("F20", [], 0x5A, 0x83, 0xFFD1),
    ("F21", [], 0xFFFF, 0x84, 0xFFD2),
    ("F22", [], 0xFFFF, 0x85, 0xFFD3),
    ("F23", [], 0xFFFF, 0x86, 0xFFD4),
    ("F24", [], 0xFFFF, 0x87, 0xFFD5),
    ("AudioVolumeDown", ["VolumeDown"], 0x49, 0xAE, 0x1008_FF11),
    ("AudioVolumeUp", ["VolumeUp"], 0x48, 0xAF, 0x1008_FF13),
    ("AudioVolumeMute", ["VolumeMute"], 0x4A, 0xAD, 0x1008_FF12),
    ("MediaPlay", [], 0xFFFF, 0xFA, 0x1008_FF14),
    ("MediaPause", [], 0xFFFF, 0, 0x1008_FF31),
    ("MediaPlayPause", [], 0xFFFF, 0xB3, 0x1008_FF14),
    ("MediaStop", [], 0xFFFF, 0xB2, 0x1008_FF15),
    ("MediaTrackNext", [], 0xFFFF, 0xB0, 0x1008_FF17),
    ("MediaTrackPrevious", ["MediaTrackPrev"], 0xFFFF, 0xB1, 0x1008_FF16),
];

fn key_def(token: &str) -> Option<&'static KeyDef> {
    KEYS.iter()
        .find(|k| k.code.eq_ignore_ascii_case(token) || k.aliases.iter().any(|a| a.eq_ignore_ascii_case(token)))
}

/// 规范化单个主键名（接受别名，大小写不敏感），返回 W3C code
pub fn normalize_code(token: &str) -> Option<&'static str> {
    key_def(token).map(|k| k.code)
}

/// W3C code → macOS 虚拟键码（ANSI 物理位置）
pub fn mac_keycode(code: &str) -> Option<u16> {
    key_def(code).and_then(|k| k.mac)
}

/// macOS 虚拟键码 → W3C code
pub fn code_from_mac_keycode(keycode: u16) -> Option<&'static str> {
    KEYS.iter().find(|k| k.mac == Some(keycode)).map(|k| k.code)
}

/// W3C code → Windows 虚拟键码
pub fn win_vk(code: &str) -> Option<u16> {
    key_def(code).and_then(|k| k.win)
}

/// W3C code → X11 keysym
pub fn x11_keysym(code: &str) -> Option<u32> {
    key_def(code).and_then(|k| k.x11)
}

/// 该键的事件是否天然带 Fn 位（F 键、方向键、导航键）
pub fn fn_is_synthesized(code: &str) -> bool {
    let is_f_key = code
        .strip_prefix('F')
        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
    is_f_key
        || matches!(
            code,
            "ArrowLeft"
                | "ArrowRight"
                | "ArrowUp"
                | "ArrowDown"
                | "Home"
                | "End"
                | "PageUp"
                | "PageDown"
                | "Delete"
                | "Insert"
        )
}

/// 字母键对应的小写字母（`KeyC` → 'c'），其他键返回 None
pub fn letter_of(code: &str) -> Option<char> {
    let rest = code.strip_prefix("Key")?;
    let mut chars = rest.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_uppercase() => Some(c.to_ascii_lowercase()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_and_alias_resolves_to_its_code() {
        for k in KEYS {
            assert_eq!(normalize_code(k.code), Some(k.code));
            for a in k.aliases {
                assert_eq!(normalize_code(a), Some(k.code), "alias {a}");
            }
        }
    }

    #[test]
    fn mac_keycodes_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for k in KEYS.iter().filter_map(|k| k.mac) {
            assert!(seen.insert(k), "duplicate mac keycode {k:#x}");
        }
    }

    #[test]
    fn codes_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for k in KEYS {
            assert!(seen.insert(k.code.to_ascii_lowercase()), "duplicate code {}", k.code);
        }
    }
}
