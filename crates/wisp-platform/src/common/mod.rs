//! 三端共享的纯逻辑。

pub mod combo;
pub mod double_tap;
pub mod gesture;
pub mod keymap;
pub mod secrets_markers;
pub mod suppress;
pub mod text;

/// Wisp 合成输入事件的标记（"WISP"）：macOS 写入 `kCGEventSourceUserData`，Windows 写入 `dwExtraInfo`，
/// 钩子据此放行自己注入的事件
pub const WISP_INPUT_TAG: u32 = 0x5749_5350;
