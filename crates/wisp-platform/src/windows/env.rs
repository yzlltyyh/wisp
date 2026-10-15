//! 键盘布局探测：`GetKeyboardLayoutList` 统计输入语言数；对每个布局用 `VkKeyScanExW` 探测常见的
//! AltGr 字符（高字节 = Ctrl+Alt），有则说明该布局使用 AltGr，`Ctrl+Alt+字母` 族可能与字符输入冲突。

use std::collections::HashSet;

use ::windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardLayoutList, HKL, VkKeyScanExW};

use crate::types::LayoutInfo;

/// 各欧洲布局常见的、需要 AltGr 才能输入的字符
const ALTGR_PROBE: &str = "@€{}[]\\|~#$^`²³µ§łąęćńśźżđšžčćőű";

/// 当前用户的键盘布局信息
pub(crate) fn keyboard_layouts() -> LayoutInfo {
    // SAFETY: 传 None 只查询数量
    let n = unsafe { GetKeyboardLayoutList(None) };
    let Ok(n) = usize::try_from(n) else {
        return LayoutInfo::default();
    };
    if n == 0 {
        return LayoutInfo::default();
    }
    let mut list = vec![HKL::default(); n];
    // SAFETY: 缓冲区长度由切片给出
    let got = unsafe { GetKeyboardLayoutList(Some(&mut list)) };
    list.truncate(usize::try_from(got).unwrap_or(0));
    let languages: HashSet<u16> = list.iter().map(|h| language_id(*h)).collect();
    LayoutInfo {
        has_altgr: list.iter().any(|h| has_altgr(*h)),
        input_language_count: u32::try_from(languages.len()).unwrap_or(u32::MAX),
        // 多种输入语言时系统默认用左 Alt+Shift 切换
        alt_shift_switches: languages.len() > 1,
    }
}

/// HKL 的低 16 位是输入语言 id
fn language_id(hkl: HKL) -> u16 {
    (hkl.0 as usize & 0xFFFF) as u16
}

fn has_altgr(hkl: HKL) -> bool {
    ALTGR_PROBE.encode_utf16().any(|ch| {
        // SAFETY: 纯查询
        let r = unsafe { VkKeyScanExW(ch, hkl) };
        r != -1 && is_altgr_shift_state(r)
    })
}

/// `VkKeyScanEx` 结果的高字节：1 = Shift，2 = Ctrl，4 = Alt；Ctrl+Alt 同时出现即 AltGr
fn is_altgr_shift_state(r: i16) -> bool {
    let shift_state = ((r as u16) >> 8) as u8;
    shift_state & 0x06 == 0x06
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shift_state_decoding() {
        // 德语 '@' = AltGr+Q：VK 0x51，shift state 6
        assert!(is_altgr_shift_state(0x0651));
        // '@' 在美式布局 = Shift+2：shift state 1
        assert!(!is_altgr_shift_state(0x0132));
        assert!(is_altgr_shift_state(0x0751));
    }

    #[test]
    fn layouts_are_counted_by_language() {
        let info = keyboard_layouts();
        // SAFETY: 传 None 只查询数量
        let layouts = unsafe { GetKeyboardLayoutList(None) };
        // 任何会话至少有一个键盘布局；输入语言按语言 id 去重，不会多于布局数
        assert!(layouts >= 1);
        assert!(info.input_language_count >= 1);
        assert!(i64::from(info.input_language_count) <= i64::from(layouts));
    }
}
