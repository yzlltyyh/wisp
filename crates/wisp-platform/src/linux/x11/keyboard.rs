//! 键盘映射与 XTest 模拟按键。
//!
//! - keysym → keycode 用核心键盘映射反查（等价于 XKeysymToKeycode），优先第 1 组第 1 级；当前布局完全没有该
//!   keysym 时（例如只装了俄语布局），临时把一个空闲 keycode 映射成它，用完再还原；
//! - 发送组合键之前，先对仍被按住的修饰键发 XTest KeyRelease（热键在按下时就触发，用户手还在修饰键上）；
//! - 映射表缓存到收到 MappingNotify 为止；查找逻辑（`keysym::KeyMap`）是纯函数，单测在 `keysym.rs`。

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use x11rb::CURRENT_TIME;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, KEY_PRESS_EVENT, KEY_RELEASE_EVENT, Keycode};
use x11rb::protocol::xtest;

use super::X11;
use crate::common::combo::Combo;
use crate::linux::keysym::{self, KeyMap, XK_ALT_L, XK_CONTROL_L, XK_SHIFT_L, XK_SUPER_L};

/// 键盘状态（映射缓存）
#[derive(Default)]
struct Cache {
    map: Option<Arc<KeyMap>>,
    /// 每次失效加 1：读取期间映射又变了（读到的可能是旧映射）时不写回缓存
    generation: u64,
}

/// 键盘状态（映射缓存）
#[derive(Default)]
pub(crate) struct Keyboard {
    cache: Mutex<Cache>,
}

impl Keyboard {
    /// 映射变化（MappingNotify）时丢弃缓存
    pub(crate) fn invalidate(&self) {
        let mut c = self.cache.lock();
        c.map = None;
        c.generation = c.generation.wrapping_add(1);
    }
}

/// 取得（必要时读取）键盘映射。读取与失效可能交错（连续两次改映射：第一次的 MappingNotify 触发读取，读取的回复
/// 还在路上时第二次的 MappingNotify 已经让缓存失效）——这时读到的映射照样返回给这次调用，但不写回缓存，下一次
/// 调用重新读取，缓存永远不会停在旧映射上
pub(crate) fn keymap(x: &X11) -> Option<Arc<KeyMap>> {
    let generation = {
        let c = x.keyboard.cache.lock();
        if let Some(m) = c.map.as_ref() {
            return Some(Arc::clone(m));
        }
        c.generation
    };
    let setup = x.conn.setup();
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let r = x.conn.get_keyboard_mapping(min, max - min + 1).ok()?.reply().ok()?;
    let map = Arc::new(KeyMap::new(min, usize::from(r.keysyms_per_keycode), r.keysyms));
    let mut c = x.keyboard.cache.lock();
    if c.generation == generation {
        c.map = Some(Arc::clone(&map));
    }
    Some(map)
}

fn fake_key(x: &X11, press: bool, code: Keycode) -> Result<(), String> {
    let ty = if press { KEY_PRESS_EVENT } else { KEY_RELEASE_EVENT };
    xtest::fake_input(&x.conn, ty, code, CURRENT_TIME, x.root, 0, 0, 0)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 抬起所有仍被按住的修饰键
pub(crate) fn release_held_modifiers(x: &X11) -> Result<(), String> {
    let Some(map) = keymap(x) else {
        return Ok(());
    };
    let keys = x
        .conn
        .query_keymap()
        .map_err(|e| e.to_string())?
        .reply()
        .map_err(|e| e.to_string())?
        .keys;
    for (byte_idx, byte) in keys.iter().enumerate() {
        if *byte == 0 {
            continue;
        }
        for bit in 0..8 {
            if byte & (1 << bit) == 0 {
                continue;
            }
            let Ok(code) = u8::try_from(byte_idx * 8 + bit) else {
                continue;
            };
            if keysym::is_modifier(map.keysym(code, 0)) {
                fake_key(x, false, code)?;
            }
        }
    }
    Ok(())
}

/// 发送组合键：按下修饰键 → 按下主键 → 抬起主键 → 抬起修饰键（逆序）
pub(crate) fn send_combo(x: &Arc<X11>, combo: &Combo) -> Result<(), String> {
    let main =
        crate::common::combo::x11_keysym(&combo.code).ok_or_else(|| format!("unsupported key {}", combo.code))?;
    let map = keymap(x).ok_or("keyboard mapping unavailable")?;
    let mut mods: Vec<u32> = Vec::new();
    if combo.modifiers.control {
        mods.push(XK_CONTROL_L);
    }
    if combo.modifiers.alt {
        mods.push(XK_ALT_L);
    }
    if combo.modifiers.shift {
        mods.push(XK_SHIFT_L);
    }
    if combo.modifiers.super_key {
        mods.push(XK_SUPER_L);
    }
    let (main_code, needs_shift, temp) = match map.find(main) {
        Some((code, shift)) => (code, shift, false),
        None => {
            let code = map.spare().ok_or("no spare keycode for temporary mapping")?;
            remap(x, code, main).map_err(|e| format!("temporary mapping: {e}"))?;
            (code, false, true)
        }
    };
    if needs_shift && !mods.contains(&XK_SHIFT_L) {
        mods.push(XK_SHIFT_L);
    }
    let mut mod_codes = Vec::with_capacity(mods.len());
    for m in &mods {
        let (code, _) = map
            .find(*m)
            .ok_or_else(|| format!("modifier keysym 0x{m:x} is not mapped"))?;
        mod_codes.push(code);
    }
    let result = (|| {
        for &c in &mod_codes {
            fake_key(x, true, c)?;
        }
        fake_key(x, true, main_code)?;
        fake_key(x, false, main_code)?;
        for &c in mod_codes.iter().rev() {
            fake_key(x, false, c)?;
        }
        x.conn.flush().map_err(|e| e.to_string())
    })();
    if temp {
        restore_spare_later(x, main_code);
    }
    result
}

fn remap(x: &X11, code: u8, sym: u32) -> Result<(), String> {
    let map = keymap(x).ok_or("keyboard mapping unavailable")?;
    let per = u8::try_from(map.per().max(2)).map_err(|e| e.to_string())?;
    let mut syms = vec![0u32; usize::from(per)];
    syms[0] = sym;
    syms[1] = sym;
    x.conn
        .change_keyboard_mapping(1, code, per, &syms)
        .map_err(|e| e.to_string())?
        .check()
        .map_err(|e| e.to_string())?;
    x.keyboard.invalidate();
    Ok(())
}

/// 临时映射在目标应用处理完按键后再还原（MappingNotify 与按键事件按顺序送达；200 ms 足够对方处理）
fn restore_spare_later(x: &Arc<X11>, code: u8) {
    let per = keymap(x).map_or(2, |m| m.per().max(2));
    let Ok(per8) = u8::try_from(per) else { return };
    let x = Arc::clone(x);
    let _ = std::thread::Builder::new()
        .name("wisp-keymap-restore".into())
        .spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            let zeros = vec![0u32; usize::from(per8)];
            let _ = x.conn.change_keyboard_mapping(1, code, per8, &zeros);
            let _ = x.conn.flush();
            x.keyboard.invalidate();
        });
}

/// 输入语言（XKB 布局）数量：解析根窗口的 `_XKB_RULES_NAMES`（rules, model, layout, variant, options）
pub(crate) fn layout_switching(x: &X11) -> (u32, bool) {
    let Some(r) = super::props::get(
        x,
        x.root,
        x.atoms._XKB_RULES_NAMES,
        x11rb::protocol::xproto::AtomEnum::STRING,
        4096,
    ) else {
        return (1, false);
    };
    parse_rules_names(&r.value)
}

/// `_XKB_RULES_NAMES` 是 rules、model、layout、variant、options 五段以 NUL 分隔的字符串：返回 (布局数, Alt+Shift
/// 是否切换布局)。后者看 options 里的 `grp:` 开关——`grp:alt_shift_toggle`、`grp:lalt_lshift_toggle`、
/// `grp:alt_shift_toggle_bidir` 等都同时含 alt 与 shift（GNOME 自己管理切换键、默认 Super+Space，不经 XKB 选项）
fn parse_rules_names(value: &[u8]) -> (u32, bool) {
    let fields: Vec<String> = value
        .split(|&b| b == 0)
        .map(|f| String::from_utf8_lossy(f).into_owned())
        .collect();
    let layouts = fields.get(2).map(String::as_str).unwrap_or_default();
    let n = layouts.split(',').filter(|s| !s.trim().is_empty()).count();
    let options = fields.get(4).map(String::as_str).unwrap_or_default();
    let alt_shift = n > 1
        && options.split(',').any(|o| {
            let o = o.trim().to_ascii_lowercase();
            o.strip_prefix("grp:")
                .is_some_and(|g| g.contains("alt") && g.contains("shift"))
        });
    (u32::try_from(n.max(1)).unwrap_or(1), alt_shift)
}

#[cfg(test)]
mod tests {
    use super::parse_rules_names;

    #[test]
    fn rules_names_report_layouts_and_alt_shift_switching() {
        assert_eq!(
            parse_rules_names(b"evdev\0pc105\0us,ru\0,\0grp:alt_shift_toggle\0"),
            (2, true)
        );
        assert_eq!(
            parse_rules_names(b"evdev\0pc105\0us,de\0\0grp:lalt_lshift_toggle,caps:escape\0"),
            (2, true)
        );
        // GNOME：切换键不在 XKB 选项里
        assert_eq!(parse_rules_names(b"evdev\0pc105\0us,cn\0\0\0"), (2, false));
        assert_eq!(
            parse_rules_names(b"evdev\0pc105\0us,ru\0\0grp:ctrl_shift_toggle\0"),
            (2, false)
        );
        // 只有一种布局时选项无效
        assert_eq!(
            parse_rules_names(b"evdev\0pc105\0us\0\0grp:alt_shift_toggle\0"),
            (1, false)
        );
        assert_eq!(parse_rules_names(b""), (1, false));
    }
}
