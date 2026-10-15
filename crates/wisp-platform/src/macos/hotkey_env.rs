//! 快捷键环境探测（冲突判定的系统快捷键一层）。
//!
//! `CopySymbolicHotKeys()` 读取「键盘 → 快捷键」中已启用的系统热键（Spotlight、Mission Control、截图、输入源切换等），
//! 按虚拟键码 + Carbon 修饰位转成 global-hotkey 规范串；比较前剥离 F 键 / 方向键上合成的 Fn 位。HIToolbox 标注该函数
//! 「Not thread safe」，与 TIS 一样在主线程上调用。

use std::ptr::NonNull;
use std::sync::Arc;
use std::time::Duration;

use objc2::MainThreadMarker;
use objc2_core_foundation::{CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};

use super::Ctx;
use super::ffi;
use crate::common::combo::Combo;
use crate::traits::HotkeyEnv;
use crate::types::LayoutInfo;

/// 主线程查询的最长等待
const MAIN_WAIT: Duration = Duration::from_millis(500);

/// HotkeyEnv 实现
pub(crate) struct MacHotkeyEnv {
    ctx: Arc<Ctx>,
}

impl MacHotkeyEnv {
    pub(crate) fn new(ctx: Arc<Ctx>) -> Self {
        Self { ctx }
    }
}

/// 字典里某个键的值
fn value(dict: &CFDictionary, key: &str) -> Option<CFRetained<CFType>> {
    let key = CFString::from_str(key);
    let key_ref: &CFType = &key;
    // SAFETY: 字典的键与值都是 CF 对象
    let typed = unsafe { dict.cast_unchecked::<CFType, CFType>() };
    typed.get(key_ref)
}

/// 主线程：读取并转换系统热键
fn system_hotkeys_main(_mtm: MainThreadMarker) -> Vec<String> {
    let mut out: ffi::CFTypeRef = std::ptr::null();
    // SAFETY: 输出参数指向有效的指针槽；成功时返回 +1 的 CFArray
    let status = unsafe { ffi::CopySymbolicHotKeys(&mut out) };
    if status != 0 {
        log::warn!("CopySymbolicHotKeys failed: {status}");
        return Vec::new();
    }
    let Some(array) = NonNull::new(out.cast_mut().cast::<CFArray>())
        // SAFETY: Copy 规则返回的 +1 引用
        .map(|p| unsafe { CFRetained::from_raw(p) })
    else {
        return Vec::new();
    };
    // SAFETY: 数组元素都是 CFDictionary
    let array = unsafe { array.cast_unchecked::<CFDictionary>() };
    let mut combos: Vec<String> = array
        .iter()
        .filter_map(|dict| {
            let enabled = value(&dict, "kHISymbolicHotKeyEnabled")
                .and_then(|v| v.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool))
                .unwrap_or(false);
            if !enabled {
                return None;
            }
            let code = value(&dict, "kHISymbolicHotKeyCode")?
                .downcast_ref::<CFNumber>()?
                .as_i64()?;
            let mods = value(&dict, "kHISymbolicHotKeyModifiers")?
                .downcast_ref::<CFNumber>()?
                .as_i64()?;
            // 0xFFFF 表示该项没有绑定按键
            let code = u16::try_from(code).ok().filter(|c| *c != 0xFFFF)?;
            Combo::from_mac(code, u32::try_from(mods).ok()?).map(|c| c.to_canonical())
        })
        .collect();
    combos.sort();
    combos.dedup();
    combos
}

/// 主线程：可选的键盘输入源数量
fn input_source_count_main(_mtm: MainThreadMarker) -> u32 {
    // SAFETY: HIToolbox 导出的常量
    let (cat_key, cat_kbd, select_key) = unsafe {
        (
            ffi::kTISPropertyInputSourceCategory,
            ffi::kTISCategoryKeyboardInputSource,
            ffi::kTISPropertyInputSourceIsSelectCapable,
        )
    };
    // SAFETY: 常量都是有效的 CFString
    let (Some(cat_key), Some(cat_kbd), Some(select_key)) = (unsafe {
        (
            cat_key.cast::<CFString>().as_ref(),
            cat_kbd.cast::<CFType>().as_ref(),
            select_key.cast::<CFString>().as_ref(),
        )
    }) else {
        return 1;
    };
    let yes: &CFType = CFBoolean::new(true);
    let filter = CFDictionary::<CFString, CFType>::from_slices(&[cat_key, select_key], &[cat_kbd, yes]);
    // SAFETY: filter 是有效的 CFDictionary；返回 +1 的 CFArray（或空）
    let list = unsafe { ffi::TISCreateInputSourceList((&*filter as *const CFDictionary<CFString, CFType>).cast(), 0) };
    let Some(list) = NonNull::new(list.cast_mut().cast::<CFArray>())
        // SAFETY: Create 规则返回的 +1 引用
        .map(|p| unsafe { CFRetained::from_raw(p) })
    else {
        return 1;
    };
    // SAFETY: HIToolbox 导出的常量
    let (type_key, layout, mode) = unsafe {
        (
            ffi::kTISPropertyInputSourceType,
            ffi::kTISTypeKeyboardLayout,
            ffi::kTISTypeKeyboardInputMode,
        )
    };
    let count = (0..list.len())
        .filter(|i| {
            // SAFETY: 下标在范围内；元素是 TISInputSourceRef
            let src = unsafe { list.cast_unchecked::<CFType>() }.get(*i);
            let Some(src) = src else { return false };
            // SAFETY: 属性按 Get 规则返回
            let t = unsafe { ffi::TISGetInputSourceProperty((&*src as *const CFType).cast(), type_key) };
            t == layout || t == mode || {
                // 指针不同但内容相同的 CFString 也算
                // SAFETY: 都是 CFString 或空指针
                let (t, layout, mode) = unsafe {
                    (
                        t.cast::<CFString>().as_ref(),
                        layout.cast::<CFString>().as_ref(),
                        mode.cast::<CFString>().as_ref(),
                    )
                };
                matches!((t, layout, mode), (Some(t), Some(l), Some(m)) if t == l || t == m)
            }
        })
        .count();
    u32::try_from(count).unwrap_or(u32::MAX).max(1)
}

impl HotkeyEnv for MacHotkeyEnv {
    fn system_hotkeys(&self) -> Vec<String> {
        self.ctx
            .main
            .run_sync(MAIN_WAIT, system_hotkeys_main)
            .unwrap_or_else(|| {
                log::warn!("system_hotkeys: main thread unavailable");
                Vec::new()
            })
    }

    fn keyboard_layouts(&self) -> LayoutInfo {
        LayoutInfo {
            // macOS 没有 AltGr（⌥ 承担同样的角色，不影响 Control+Alt 组合）
            has_altgr: false,
            input_language_count: self.ctx.main.run_sync(MAIN_WAIT, input_source_count_main).unwrap_or(1),
            // ⌥⇧ 在 macOS 上不切换输入源（⌃Space / Fn / 大写锁定才切换）
            alt_shift_switches: false,
        }
    }
}
