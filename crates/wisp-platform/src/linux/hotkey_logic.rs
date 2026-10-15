//! X11 全局快捷键的纯逻辑：规范串 → (keysym, 修饰位)、抓取时要覆盖的锁定位组合、
//! 按键事件的匹配与「按住只算一次」的状态表。只依赖 std 与 `common::combo`，`tests/smoke_linux.rs` 用 `#[path]`
//! 引入，在 macOS 开发机上也能跑单测。
//!
//! 为什么自己实现：global-hotkey 0.8 的 X11 后端在自己的线程里 `poll_for_event` + `sleep(50 ms)` 轮询，进程常驻
//! 20 次 / 秒的唤醒（违背空闲零唤醒的目标）。这里把 `XGrabKey` 的按键事件并进平台层唯一的 X11 事件线程
//! （阻塞在 `wait_for_event` 上），空闲时零唤醒。
//!
//! 修饰位与 global-hotkey 一致：Control → ControlMask、Alt → Mod1、Shift → ShiftMask、Super → Mod4；NumLock（Mod2）
//! 与 CapsLock（Lock）打开时 X 把它们算进 state，所以每个组合按 4 种锁定位组合各抓一次，匹配时只比较这四个修饰位。

use crate::common::combo::{self, Combo};

/// X 核心协议的修饰位（`KeyButMask` / `ModMask` 的低 8 位）
pub const MOD_SHIFT: u16 = 1;
pub const MOD_LOCK: u16 = 1 << 1;
pub const MOD_CONTROL: u16 = 1 << 2;
pub const MOD_1: u16 = 1 << 3;
pub const MOD_2: u16 = 1 << 4;
pub const MOD_4: u16 = 1 << 6;

/// 参与匹配的修饰位（其余位：锁定键、AltGr 所在的 Mod5、鼠标键、XKB 组号，一律忽略）
pub const RELEVANT: u16 = MOD_SHIFT | MOD_CONTROL | MOD_1 | MOD_4;

/// 每个组合要一并抓取的锁定位组合：无 / CapsLock / NumLock / 两者
pub const LOCK_VARIANTS: [u16; 4] = [0, MOD_LOCK, MOD_2, MOD_LOCK | MOD_2];

/// 一个组合要抓取的内容
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spec {
    /// 主键的 keysym（按当前键盘映射反查 keycode）
    pub keysym: u32,
    /// 修饰位（[`RELEVANT`] 的子集）
    pub mods: u16,
}

/// 修饰键集合 → 修饰位
pub fn mod_mask(m: &combo::Modifiers) -> u16 {
    let mut bits = 0;
    if m.shift {
        bits |= MOD_SHIFT;
    }
    if m.control {
        bits |= MOD_CONTROL;
    }
    if m.alt {
        bits |= MOD_1;
    }
    if m.super_key {
        bits |= MOD_4;
    }
    bits
}

/// 解析规范串（宽松：大小写、修饰键顺序、别名都接受），返回（规范串, 抓取内容）。主键没有 X11 keysym 时为 None
pub fn spec(s: &str) -> Option<(String, Spec)> {
    let c = Combo::parse(s)?;
    let keysym = c.x11_keysym()?;
    Some((
        c.to_canonical(),
        Spec {
            keysym,
            mods: mod_mask(&c.modifiers),
        },
    ))
}

/// 事件的 state 是否就是这个组合的修饰位（锁定位等无关位忽略）
pub fn matches(state: u16, mods: u16) -> bool {
    state & RELEVANT == mods
}

/// 一个已注册的组合
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// 规范串（回调里原样交出）
    pub combo: String,
    pub spec: Spec,
    /// 当前连接 / 键盘映射下抓取用的 keycode（还没抓上时为 None：断线、布局里暂时没有这个键）
    pub keycode: Option<u8>,
    /// 按下且还没抬起（按住时的重复按下不再回调）
    pressed: bool,
}

/// 已注册组合的状态表（跨连接保存；重连、键盘映射变化后重新算 keycode 并重新抓取）
#[derive(Debug, Default)]
pub struct GrabTable {
    entries: Vec<Entry>,
}

impl GrabTable {
    /// 是否已注册（按规范串）
    pub fn contains(&self, combo: &str) -> bool {
        self.entries.iter().any(|e| e.combo == combo)
    }

    /// 某个 (keycode, 修饰位) 是否已经被另一个组合占用（不同写法的同一组合在 [`spec`] 之后规范串相同，不会走到这里）
    pub fn owner_of(&self, keycode: u8, mods: u16) -> Option<&str> {
        self.entries
            .iter()
            .find(|e| e.keycode == Some(keycode) && e.spec.mods == mods)
            .map(|e| e.combo.as_str())
    }

    /// 登记（已登记时返回 false、不改动）
    pub fn insert(&mut self, combo: String, spec: Spec, keycode: Option<u8>) -> bool {
        if self.contains(&combo) {
            return false;
        }
        self.entries.push(Entry {
            combo,
            spec,
            keycode,
            pressed: false,
        });
        true
    }

    /// 注销，返回原来的条目
    pub fn remove(&mut self, combo: &str) -> Option<Entry> {
        let i = self.entries.iter().position(|e| e.combo == combo)?;
        Some(self.entries.remove(i))
    }

    /// 全部条目（重新抓取用）
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// 更新某个组合的 keycode（重新抓取之后）。返回 None = 这个组合已经不在表里（调用方要撤销刚抓上的）；
    /// Some(true) = keycode 变了且原来正按着——旧 keycode 的抬起不会再送到 Wisp，按住状态已清掉，调用方补发一次抬起；
    /// keycode 没变时按住状态保留
    pub fn set_keycode(&mut self, combo: &str, keycode: Option<u8>) -> Option<bool> {
        let e = self.entries.iter_mut().find(|e| e.combo == combo)?;
        if e.keycode == keycode {
            return Some(false);
        }
        e.keycode = keycode;
        Some(std::mem::take(&mut e.pressed))
    }

    /// 连接换了（断线重连）：旧连接上的抓取随连接一起消失，全部 keycode 作废；返回原来正按着的组合（抬起不会再来，
    /// 调用方补发抬起）
    pub fn forget_keycodes(&mut self) -> Vec<String> {
        self.entries
            .iter_mut()
            .filter_map(|e| {
                e.keycode = None;
                std::mem::take(&mut e.pressed).then(|| e.combo.clone())
            })
            .collect()
    }

    /// 收到按下：返回应当回调的组合（按住期间的重复按下、未注册的键返回 None）
    pub fn press(&mut self, keycode: u8, state: u16) -> Option<String> {
        let e = self
            .entries
            .iter_mut()
            .find(|e| e.keycode == Some(keycode) && matches(state, e.spec.mods))?;
        if e.pressed {
            return None;
        }
        e.pressed = true;
        Some(e.combo.clone())
    }

    /// 收到抬起：返回这次抬起结束的组合（同一个键上按下过的；修饰键可能先于主键抬起，因此不比较 state）
    pub fn release(&mut self, keycode: u8) -> Vec<String> {
        self.entries
            .iter_mut()
            .filter(|e| e.keycode == Some(keycode) && e.pressed)
            .map(|e| {
                e.pressed = false;
                e.combo.clone()
            })
            .collect()
    }
}

/// 重新抓取（键盘映射变化之后）时一个组合要做的事
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Regrab {
    /// keycode 没变、仍然抓着：什么都不做——不撤销再抓（没有空窗，按住时的抬起不会丢），按住状态保留
    Keep,
    /// 撤销旧的抓取（`old` 为 Some 时），在新 keycode 上抓取（`new` 为 Some 时）
    Move { old: Option<u8>, new: Option<u8> },
}

/// 当前抓取用的 keycode 与新映射下应当用的 keycode → 要做的事。Wisp 自己发送按键时的临时映射（`keyboard::remap`）
/// 每次都会触发两次 `MappingNotify`，只改一个空闲 keycode，已注册组合的 keycode 不变，因此全部是 `Keep`
pub fn regrab(current: Option<u8>, wanted: Option<u8>) -> Regrab {
    if current == wanted {
        Regrab::Keep
    } else {
        Regrab::Move {
            old: current,
            new: wanted,
        }
    }
}

/// 自动重复的 Release 与 Press 时间戳之差的上限（毫秒）。服务器（xserver `AccessXRepeatKeyExpire`）连着生成这一对，
/// 但两次各取一次 `GetTimeInMillis()`，跨过毫秒边界、或服务器正忙时会差 1 ms（SDL 的同类判定同样放宽到 < 2 ms）
pub const AUTOREPEAT_SLACK_MS: u32 = 1;

/// X11 没有启用 XKB 的「可检测自动重复」时，按住一个键会产生成对的 KeyRelease + KeyPress（同一 keycode、时间戳相同或差
/// 1 ms，服务器一次写出）。抬起之后紧跟着的事件如果是这样的按下，这一对就是自动重复，两者都丢弃。
/// 这只是快速路径：这一对被拆在两次读取里时（服务器或本进程正忙），调用方再查一次按键状态（`QueryKeymap`）兜底
pub fn is_autorepeat(release: (u8, u32), next_press: Option<(u8, u32)>) -> bool {
    next_press.is_some_and(|(code, time)| code == release.0 && time.wrapping_sub(release.1) <= AUTOREPEAT_SLACK_MS)
}

/// `QueryKeymap` 的 32 字节位图里这个 keycode 是否按着
pub fn key_down(keys: &[u8], keycode: u8) -> bool {
    keys.get(usize::from(keycode / 8))
        .is_some_and(|byte| byte & (1 << (keycode % 8)) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_follows_global_hotkey_masks() {
        let (canon, s) = spec("alt+shift+keyd").expect("可解析");
        assert_eq!(canon, "Alt+Shift+KeyD");
        assert_eq!(s.keysym, u32::from(b'd'));
        assert_eq!(s.mods, MOD_1 | MOD_SHIFT);
        let (_, s) = spec("Control+Super+Space").expect("可解析");
        assert_eq!(s.mods, MOD_CONTROL | MOD_4);
        assert_eq!(s.keysym, 0x20);
        // 裸 Esc（全局 Esc guard）
        let (canon, s) = spec("Escape").expect("可解析");
        assert_eq!(canon, "Escape");
        assert_eq!((s.keysym, s.mods), (0xff1b, 0));
        assert!(spec("Control+").is_none());
        assert!(spec("Shift").is_none());
    }

    #[test]
    fn lock_keys_do_not_change_the_match() {
        let mods = MOD_CONTROL | MOD_1;
        for lock in LOCK_VARIANTS {
            assert!(matches(mods | lock, mods));
        }
        // AltGr（Mod5）、鼠标键位、XKB 组号同样忽略
        assert!(matches(mods | 0x80 | 0x100 | 0x2000, mods));
        assert!(!matches(MOD_CONTROL, mods));
        assert!(!matches(mods | MOD_SHIFT, mods));
    }

    #[test]
    fn holding_fires_once_and_release_rearms() {
        let mut t = GrabTable::default();
        let (combo, s) = spec("Alt+KeyD").expect("可解析");
        assert!(t.insert(combo.clone(), s, Some(40)));
        assert!(!t.insert(combo.clone(), s, Some(40)), "重复登记");
        assert_eq!(t.owner_of(40, MOD_1), Some("Alt+KeyD"));
        assert_eq!(t.owner_of(40, 0), None);
        // 修饰位不符（多按了 Shift）不触发
        assert_eq!(t.press(40, MOD_1 | MOD_SHIFT), None);
        assert_eq!(t.press(40, MOD_1 | MOD_2).as_deref(), Some("Alt+KeyD"));
        // 按住：重复按下不再回调
        assert_eq!(t.press(40, MOD_1), None);
        // 先抬起 Alt 再抬起 D：抬起不看 state
        assert_eq!(t.release(40), vec![combo.clone()]);
        assert!(t.release(40).is_empty());
        assert_eq!(t.press(40, MOD_1).as_deref(), Some("Alt+KeyD"));
        // keycode 没变的重新抓取：按住状态保留（之后的抬起照常结束这次按下）
        assert_eq!(t.set_keycode(&combo, Some(40)), Some(false));
        assert_eq!(t.press(40, MOD_1), None, "仍在按住");
        assert_eq!(t.release(40), vec![combo.clone()]);
        // 按住期间映射变化换了 keycode：旧的按住状态清掉，并告诉调用方补发抬起
        assert_eq!(t.press(40, MOD_1).as_deref(), Some("Alt+KeyD"));
        assert_eq!(t.set_keycode(&combo, Some(41)), Some(true));
        assert_eq!(t.press(40, MOD_1), None);
        assert_eq!(t.press(41, MOD_1).as_deref(), Some("Alt+KeyD"));
        // 断线重连：keycode 全部作废，正按着的组合交给调用方补发抬起
        assert_eq!(t.forget_keycodes(), vec![combo.clone()]);
        assert_eq!(t.entries()[0].keycode, None);
        assert!(t.forget_keycodes().is_empty());
        assert_eq!(t.set_keycode(&combo, Some(41)), Some(false), "没在按住");
        assert_eq!(t.remove(&combo).map(|e| e.keycode), Some(Some(41)));
        assert!(t.entries().is_empty());
        assert_eq!(t.press(41, MOD_1), None);
        // 已注销的组合：告诉调用方撤销刚抓上的
        assert_eq!(t.set_keycode(&combo, Some(41)), None);
    }

    #[test]
    fn regrab_only_moves_changed_keycodes() {
        assert_eq!(regrab(Some(40), Some(40)), Regrab::Keep);
        assert_eq!(regrab(None, None), Regrab::Keep);
        assert_eq!(
            regrab(Some(40), Some(41)),
            Regrab::Move {
                old: Some(40),
                new: Some(41)
            }
        );
        // 之前没抓上（被占用、布局里没有这个键）：重试
        assert_eq!(
            regrab(None, Some(41)),
            Regrab::Move {
                old: None,
                new: Some(41)
            }
        );
        // 新布局里没有这个键：撤销旧的
        assert_eq!(
            regrab(Some(40), None),
            Regrab::Move {
                old: Some(40),
                new: None
            }
        );
    }

    #[test]
    fn autorepeat_pairs_share_keycode_and_time() {
        assert!(is_autorepeat((40, 1000), Some((40, 1000))));
        // 两个时间戳跨过毫秒边界
        assert!(is_autorepeat((40, 1000), Some((40, 1001))));
        assert!(is_autorepeat((40, u32::MAX), Some((40, 0))), "服务器时间回绕");
        assert!(!is_autorepeat((40, 1000), Some((40, 1002))));
        assert!(!is_autorepeat((40, 1000), Some((40, 1031))));
        assert!(!is_autorepeat((40, 1000), Some((40, 999))), "按下早于抬起：不是这一对");
        assert!(!is_autorepeat((40, 1000), Some((41, 1000))));
        assert!(!is_autorepeat((40, 1000), None));
    }

    #[test]
    fn key_down_reads_the_keymap_bitmap() {
        let mut keys = [0u8; 32];
        keys[44 / 8] |= 1 << (44 % 8);
        keys[255 / 8] |= 1 << (255 % 8);
        assert!(key_down(&keys, 44));
        assert!(key_down(&keys, 255));
        assert!(!key_down(&keys, 45));
        assert!(!key_down(&keys[..4], 44), "位图太短视为没按着");
    }
}
