//! X11 修饰键的 keysym 常量与分类（纯逻辑，只依赖 std / wisp-types）。
//!
//! 主键名 → keysym 的映射由 `common::combo::x11_keysym`（三端共用的键表）提供；这里只放 X11 专有的
//! 修饰键 / 锁定键分类（模拟按键前抬起残留修饰键、双击修饰键判定），以及核心键盘映射表的查找
//! （按布局反查键码、AltGr 判定）。

use wisp_types::ModifierKey;

pub const XK_SHIFT_L: u32 = 0xffe1;
pub const XK_SHIFT_R: u32 = 0xffe2;
pub const XK_CONTROL_L: u32 = 0xffe3;
pub const XK_CONTROL_R: u32 = 0xffe4;
#[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
pub const XK_CAPS_LOCK: u32 = 0xffe5;
#[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
pub const XK_SHIFT_LOCK: u32 = 0xffe6;
pub const XK_META_L: u32 = 0xffe7;
pub const XK_META_R: u32 = 0xffe8;
pub const XK_ALT_L: u32 = 0xffe9;
pub const XK_ALT_R: u32 = 0xffea;
pub const XK_SUPER_L: u32 = 0xffeb;
pub const XK_SUPER_R: u32 = 0xffec;
pub const XK_HYPER_L: u32 = 0xffed;
pub const XK_HYPER_R: u32 = 0xffee;
pub const XK_ISO_LEVEL3_SHIFT: u32 = 0xfe03;
pub const XK_ISO_LEVEL5_SHIFT: u32 = 0xfe11;
pub const XK_MODE_SWITCH: u32 = 0xff7e;
#[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
pub const XK_NUM_LOCK: u32 = 0xff7f;

/// 「按住会影响快捷键语义」的修饰键（发送合成按键前要先抬起它们）
pub const HELD_MODIFIERS: &[u32] = &[
    XK_SHIFT_L,
    XK_SHIFT_R,
    XK_CONTROL_L,
    XK_CONTROL_R,
    XK_META_L,
    XK_META_R,
    XK_ALT_L,
    XK_ALT_R,
    XK_SUPER_L,
    XK_SUPER_R,
    XK_HYPER_L,
    XK_HYPER_R,
    XK_ISO_LEVEL3_SHIFT,
    XK_ISO_LEVEL5_SHIFT,
    XK_MODE_SWITCH,
];

/// keysym 是否为修饰键（锁定键不算：Caps Lock / Num Lock 打开时不应影响双击修饰键判定）
pub fn is_modifier(keysym: u32) -> bool {
    HELD_MODIFIERS.contains(&keysym)
}

/// 同一种修饰键的左右两侧（与 Alt 共用一个键的 Meta 也算 Alt）归到同一个代表
fn modifier_family(keysym: u32) -> u32 {
    match keysym {
        XK_SHIFT_R => XK_SHIFT_L,
        XK_CONTROL_R => XK_CONTROL_L,
        XK_ALT_R | XK_META_L | XK_META_R => XK_ALT_L,
        XK_SUPER_R => XK_SUPER_L,
        XK_HYPER_R => XK_HYPER_L,
        other => other,
    }
}

/// 重放用户按下的组合时，要模拟按下（之后抬起）的修饰键：`wanted` 里同一种（左右任一侧）已经被按住的跳过。
/// 用户按住 Ctrl 连按 V 时，合成的 Control_L 抬起会让服务器认为 Ctrl 已经松开，下一下 V 就成了普通的「v」
pub fn modifiers_to_press(wanted: &[u32], held: &[u32]) -> Vec<u32> {
    wanted
        .iter()
        .copied()
        .filter(|w| !held.iter().any(|h| modifier_family(*h) == modifier_family(*w)))
        .collect()
}

/// keysym 是否为锁定键
#[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
pub fn is_lock(keysym: u32) -> bool {
    matches!(keysym, XK_CAPS_LOCK | XK_SHIFT_LOCK | XK_NUM_LOCK)
}

/// 双击修饰键的目标对应哪些 keysym（右 Alt 在多数欧洲布局上是 AltGr，即 ISO_Level3_Shift）
#[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
pub fn modifier_key_syms(key: ModifierKey) -> &'static [u32] {
    match key {
        ModifierKey::LeftCtrl => &[XK_CONTROL_L],
        ModifierKey::RightCtrl => &[XK_CONTROL_R],
        ModifierKey::LeftAlt => &[XK_ALT_L, XK_META_L],
        ModifierKey::RightAlt => &[XK_ALT_R, XK_META_R, XK_ISO_LEVEL3_SHIFT],
        ModifierKey::LeftCmd => &[XK_SUPER_L],
        ModifierKey::RightCmd => &[XK_SUPER_R],
    }
}

/// evdev 键码：右 Alt（xkeyboard-config 的 `<RALT>`）
const EVDEV_RALT: u8 = 108;
/// xkeyboard-config 为所有布局都定义的伪键：`<LVL3>`（= ISO_Level3_Shift，用来把 Mod5 绑到第 3 级）与
/// `<MDSW>`（= Mode_switch）。它们不对应任何物理按键，判断 AltGr 时必须排除
const PSEUDO_LEVEL3_KEYS: [u8; 2] = [92, 203];

/// 核心键盘映射（GetKeyboardMapping）的快照：每个 keycode 有 `per` 列 keysym
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyMap {
    min: u8,
    per: usize,
    syms: Vec<u32>,
}

impl KeyMap {
    /// `syms` 按 keycode 从 `min` 开始、每个 keycode `per` 列依次排列
    pub fn new(min: u8, per: usize, syms: Vec<u32>) -> Self {
        Self { min, per, syms }
    }

    /// 列数（keysyms_per_keycode）
    pub fn per(&self) -> usize {
        self.per
    }

    fn count(&self) -> usize {
        self.syms.len() / self.per.max(1)
    }

    fn code(&self, i: usize) -> Option<u8> {
        self.min.checked_add(u8::try_from(i).ok()?)
    }

    /// keycode 第 `index` 列的 keysym（0 = 第 1 组无 Shift）
    pub fn keysym(&self, keycode: u8, index: usize) -> u32 {
        if keycode < self.min || index >= self.per {
            return 0;
        }
        let base = usize::from(keycode - self.min) * self.per;
        self.syms.get(base + index).copied().unwrap_or(0)
    }

    /// 查找产生 `sym` 的 keycode，返回 (keycode, 是否需要 Shift)：先第 1 组第 1 级，再第 1 组第 2 级，最后其他组
    pub fn find(&self, sym: u32) -> Option<(u8, bool)> {
        let count = self.count();
        for col in [0usize, 1].into_iter().filter(|&c| c < self.per) {
            for i in 0..count {
                if self.syms.get(i * self.per + col) == Some(&sym) {
                    return self.code(i).map(|c| (c, col == 1));
                }
            }
        }
        for i in 0..count {
            for col in 2..self.per {
                if self.syms.get(i * self.per + col) == Some(&sym) {
                    return self.code(i).map(|c| (c, col % 2 == 1));
                }
            }
        }
        None
    }

    /// 一个完全没有 keysym 的 keycode（用于临时映射），从高往低找
    pub fn spare(&self) -> Option<u8> {
        (0..self.count())
            .rev()
            .find(|&i| self.syms[i * self.per..(i + 1) * self.per].iter().all(|&s| s == 0))
            .and_then(|i| self.code(i))
    }

    /// 右 Alt 是否是 AltGr（AltGr 布局选右 Alt 作双击修饰键时警告）。
    ///
    /// 不能只看「有没有 ISO_Level3_Shift」：xkeyboard-config 给所有布局（包括 us）都定义了伪键 `<LVL3>`。
    /// 判定顺序：① evdev 的右 Alt（108）第 1 列是 ISO_Level3_Shift / Mode_switch → 是；② 有物理键产生
    /// Alt_R / Meta_R（右 Alt 仍是 Alt）→ 否；③ 其余情况（例如旧的 kbd 驱动键码）看伪键以外有没有键在第 1 列
    /// 产生 ISO_Level3_Shift / Mode_switch
    pub fn has_altgr(&self) -> bool {
        let is_level3 = |s: u32| s == XK_ISO_LEVEL3_SHIFT || s == XK_MODE_SWITCH;
        if is_level3(self.keysym(EVDEV_RALT, 0)) {
            return true;
        }
        let firsts = || (0..self.count()).filter_map(|i| Some((self.code(i)?, self.syms[i * self.per])));
        if firsts().any(|(_, s)| s == XK_ALT_R || s == XK_META_R) {
            return false;
        }
        firsts().any(|(code, s)| is_level3(s) && !PSEUDO_LEVEL3_KEYS.contains(&code))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_modifiers_are_not_pressed_again() {
        let wanted = [XK_CONTROL_L, XK_SHIFT_L];
        // 用户按住右 Ctrl：Ctrl 已经满足，只补 Shift
        assert_eq!(modifiers_to_press(&wanted, &[XK_CONTROL_R]), vec![XK_SHIFT_L]);
        assert_eq!(
            modifiers_to_press(&wanted, &[XK_CONTROL_L, XK_SHIFT_R]),
            Vec::<u32>::new()
        );
        // 按住的是组合不要的修饰键：不影响要按的
        assert_eq!(modifiers_to_press(&[XK_CONTROL_L], &[XK_ALT_L]), vec![XK_CONTROL_L]);
        assert_eq!(modifiers_to_press(&[XK_ALT_L], &[XK_META_L]), Vec::<u32>::new());
        assert_eq!(modifiers_to_press(&wanted, &[]), wanted.to_vec());
    }

    /// 按 keycode → 第 1 组前两列构造映射（其余列补 0）
    fn keymap(entries: &[(u8, u32, u32)]) -> KeyMap {
        let min = 8u8;
        let max = entries.iter().map(|e| e.0).max().unwrap_or(min);
        let per = 4;
        let mut syms = vec![0u32; usize::from(max - min + 1) * per];
        for &(code, a, b) in entries {
            let base = usize::from(code - min) * per;
            syms[base] = a;
            syms[base + 1] = b;
        }
        KeyMap::new(min, per, syms)
    }

    #[test]
    fn finds_keycodes_by_level_and_group() {
        // keycode 8..=11，每个 4 列：[g1 无 shift, g1 shift, g2 无 shift, g2 shift]
        let m = KeyMap::new(
            8,
            4,
            vec![
                u32::from(b'c'),
                u32::from(b'C'),
                0x6d1,
                0x6f1, // c / Cyrillic
                u32::from(b'1'),
                u32::from(b'!'),
                0,
                0,
                0,
                0,
                u32::from(b'v'),
                u32::from(b'V'), // v 只在第 2 组
                0,
                0,
                0,
                0,
            ],
        );
        assert_eq!(m.find(u32::from(b'c')), Some((8, false)));
        assert_eq!(m.find(u32::from(b'!')), Some((9, true)));
        assert_eq!(m.find(u32::from(b'v')), Some((10, false)));
        assert_eq!(m.find(u32::from(b'V')), Some((10, true)));
        assert_eq!(m.find(0x6d1), Some((8, false)));
        assert_eq!(m.find(0xdead), None);
        assert_eq!(m.spare(), Some(11));
        assert_eq!(m.keysym(9, 1), u32::from(b'!'));
        assert_eq!(m.keysym(7, 0), 0);
        assert!(!m.has_altgr());
    }

    #[test]
    fn altgr_detection_ignores_pseudo_level3_keys() {
        // us：右 Alt = Alt_R；<LVL3>（92）= ISO_Level3_Shift、<MDSW>（203）= Mode_switch 是所有布局都有的伪键
        let us = keymap(&[
            (64, XK_ALT_L, XK_META_L),
            (92, XK_ISO_LEVEL3_SHIFT, 0),
            (108, XK_ALT_R, XK_META_R),
            (203, XK_MODE_SWITCH, 0),
        ]);
        assert!(!us.has_altgr());
        // de：右 Alt = ISO_Level3_Shift
        let de = keymap(&[
            (64, XK_ALT_L, XK_META_L),
            (92, XK_ISO_LEVEL3_SHIFT, 0),
            (108, XK_ISO_LEVEL3_SHIFT, 0),
            (203, XK_MODE_SWITCH, 0),
        ]);
        assert!(de.has_altgr());
        // 旧 kbd 驱动键码（右 Alt = 113）上的 AltGr 布局；没有任何键产生 Alt_R
        let legacy = keymap(&[(64, XK_ALT_L, XK_META_L), (113, XK_ISO_LEVEL3_SHIFT, 0)]);
        assert!(legacy.has_altgr());
        // 旧 kbd 驱动键码上的 us 布局
        let legacy_us = keymap(&[(64, XK_ALT_L, XK_META_L), (113, XK_ALT_R, XK_META_R)]);
        assert!(!legacy_us.has_altgr());
        // AltGr 放在 Menu 键上（lv3:menu_switch）：右 Alt 仍然是 Alt
        let menu = keymap(&[
            (92, XK_ISO_LEVEL3_SHIFT, 0),
            (108, XK_ALT_R, XK_META_R),
            (135, XK_ISO_LEVEL3_SHIFT, 0),
        ]);
        assert!(!menu.has_altgr());
    }

    #[test]
    fn modifier_tables() {
        assert!(is_modifier(XK_CONTROL_R));
        assert!(is_modifier(XK_ISO_LEVEL3_SHIFT));
        assert!(!is_modifier(XK_CAPS_LOCK));
        assert!(is_lock(XK_CAPS_LOCK));
        assert!(modifier_key_syms(ModifierKey::RightAlt).contains(&XK_ISO_LEVEL3_SHIFT));
        for k in [
            ModifierKey::LeftCtrl,
            ModifierKey::RightCtrl,
            ModifierKey::LeftAlt,
            ModifierKey::RightAlt,
            ModifierKey::LeftCmd,
            ModifierKey::RightCmd,
        ] {
            assert!(modifier_key_syms(k).iter().all(|&s| is_modifier(s)));
        }
    }
}
