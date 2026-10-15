//! objc2 系列 crate 没有覆盖的系统函数：Carbon（HIToolbox 的 TIS / UCKeyTranslate / CopySymbolicHotKeys）与
//! IOKit（输入监控权限）。全部声明为 `unsafe extern "C"`，调用方负责满足各自的线程要求（TIS 与
//! CopySymbolicHotKeys 只能在主线程调用，见 HIToolbox 头文件的 "Not thread safe"）。

use std::ffi::c_void;

/// `TISInputSourceRef` / `CFStringRef` / `CFArrayRef` 等 CF 对象的裸指针
pub(crate) type CFTypeRef = *const c_void;

/// `kUCKeyActionDown`
pub(crate) const UC_KEY_ACTION_DOWN: u16 = 0;
/// `kUCKeyTranslateNoDeadKeysMask`
pub(crate) const UC_NO_DEAD_KEYS: u32 = 1;

/// `kIOHIDRequestTypeListenEvent`
pub(crate) const IOHID_REQUEST_LISTEN_EVENT: u32 = 1;
/// `kIOHIDAccessTypeGranted`
pub(crate) const IOHID_ACCESS_GRANTED: u32 = 0;
/// `kIOHIDAccessTypeDenied`
pub(crate) const IOHID_ACCESS_DENIED: u32 = 1;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    pub(crate) fn TISCopyCurrentKeyboardLayoutInputSource() -> CFTypeRef;
    pub(crate) fn TISCopyCurrentASCIICapableKeyboardLayoutInputSource() -> CFTypeRef;
    pub(crate) fn TISGetInputSourceProperty(source: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    pub(crate) fn TISCreateInputSourceList(properties: CFTypeRef, include_all_installed: u8) -> CFTypeRef;
    pub(crate) static kTISPropertyUnicodeKeyLayoutData: CFTypeRef;
    pub(crate) static kTISPropertyInputSourceCategory: CFTypeRef;
    pub(crate) static kTISCategoryKeyboardInputSource: CFTypeRef;
    pub(crate) static kTISPropertyInputSourceIsSelectCapable: CFTypeRef;
    pub(crate) static kTISPropertyInputSourceType: CFTypeRef;
    pub(crate) static kTISTypeKeyboardLayout: CFTypeRef;
    pub(crate) static kTISTypeKeyboardInputMode: CFTypeRef;
    pub(crate) static kTISNotifySelectedKeyboardInputSourceChanged: CFTypeRef;
    pub(crate) fn LMGetKbdType() -> u8;
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn UCKeyTranslate(
        key_layout: *const c_void,
        virtual_key_code: u16,
        key_action: u16,
        modifier_key_state: u32,
        keyboard_type: u32,
        key_translate_options: u32,
        dead_key_state: *mut u32,
        max_string_length: usize,
        actual_string_length: *mut usize,
        unicode_string: *mut u16,
    ) -> i32;
    pub(crate) fn CopySymbolicHotKeys(out_hot_key_array: *mut CFTypeRef) -> i32;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    pub(crate) fn IOHIDCheckAccess(request_type: u32) -> u32;
    pub(crate) fn IOHIDRequestAccess(request_type: u32) -> bool;
}
