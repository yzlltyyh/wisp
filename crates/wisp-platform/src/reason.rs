//! 平台层下发的原因 key（前端 i18n key，`errors.platform.<reason>`，文案在 `locales/*/errors.json`）。
//!
//! 三个平台实现与上层（编排器、快捷键服务）共用这一份常量：`Capability.reason_key`、`SelectionError::Unsupported`、
//! `PasteError::Unsupported`、`PlatformError::Unsupported` 携带的都是这里的值。上层按 key 分支时（例如编排器把
//! [`WAYLAND_SELECTION`] 映射成「先复制」提示）只能引用这里的常量，不要写字符串字面量。
//! 新增 key 时同步 `locales/{zh-CN,en-US}/errors.json`，`pnpm i18n:check` 把关。

/// macOS：缺少辅助功能权限（取词、模拟复制 / 粘贴）
pub const ACCESSIBILITY_MISSING: &str = "errors.platform.accessibility_missing";
/// macOS：缺少屏幕录制权限（截图 OCR）
pub const SCREEN_RECORDING_MISSING: &str = "errors.platform.screen_recording_missing";
/// macOS：缺少输入监控权限（扩展触发）
pub const INPUT_MONITORING_MISSING: &str = "errors.platform.input_monitoring_missing";
/// 编译时未开启 `mouse-hook` feature：没有扩展触发
pub const HOOKS_NOT_COMPILED: &str = "errors.platform.hooks_not_compiled";
/// Windows / Linux 没有对应的系统权限模型
pub const NO_PERMISSION_MODEL: &str = "errors.platform.no_permission_model";
/// 剪贴板内容过大无法备份、且剪贴板历史已关闭：跳过模拟复制（以输入模式打开并提示
/// 「剪贴板内容过大，未执行自动复制」）。三端相同
pub const CLIPBOARD_TOO_LARGE: &str = "errors.platform.clipboard_too_large";

/// Wayland：无法读取其他应用的选区（以输入模式打开，提示先复制再粘贴）
pub const WAYLAND_SELECTION: &str = "errors.platform.wayland_selection";
/// Wayland：不能注入按键，模拟复制不可用
pub const WAYLAND_SIMULATED_COPY: &str = "errors.platform.wayland_simulated_copy";
/// Wayland：只复制不粘贴（提示「已复制，按 Ctrl+V 粘贴」）
pub const WAYLAND_PASTE: &str = "errors.platform.wayland_paste";
/// Wayland：全局快捷键需要 GlobalShortcuts portal（后续）
pub const WAYLAND_HOTKEYS: &str = "errors.platform.wayland_hotkeys";
/// Wayland：截图需要 Screenshot portal（后续）
pub const WAYLAND_CAPTURE: &str = "errors.platform.wayland_capture";
/// Wayland：剪贴板监听需要 data-control 协议（后续）
pub const WAYLAND_CLIPBOARD_WATCH: &str = "errors.platform.wayland_clipboard_watch";
/// Wayland：扩展触发（浮标 / 双击修饰键 / 鼠标键）不可用
pub const WAYLAND_INPUT_HOOKS: &str = "errors.platform.wayland_input_hooks";
/// Wayland：没有 XWayland 时也无法写剪贴板
pub const WAYLAND_CLIPBOARD: &str = "errors.platform.wayland_clipboard";
/// Wayland：合成器只在 XWayland 窗口获焦时才把 X11 剪贴板同步给 Wayland 应用（KWin），后台写入不会生效；
/// 上层改由面板（WebView）在获焦时写剪贴板
pub const WAYLAND_CLIPBOARD_BRIDGE: &str = "errors.platform.wayland_clipboard_bridge";
/// Wayland：取不到前台应用 / 无法切换焦点
pub const WAYLAND_FOCUS: &str = "errors.platform.wayland_focus";
/// 既没有 X11 也没有 Wayland（无头环境）
pub const NO_DISPLAY: &str = "errors.platform.no_display";
/// 连接 X 服务器失败
pub const X11_CONNECT_FAILED: &str = "errors.platform.x11_connect_failed";
/// X 服务器缺少 XTEST 扩展：不能模拟按键（模拟复制 / 粘贴）
pub const X11_MISSING_XTEST: &str = "errors.platform.x11_missing_xtest";
/// X 服务器缺少 XFIXES 扩展：不能监听剪贴板（模拟复制的变更检测也依赖它）
pub const X11_MISSING_XFIXES: &str = "errors.platform.x11_missing_xfixes";
/// X 服务器缺少 XInput 2.2：不能安装扩展触发 / 外部点击监听
pub const X11_MISSING_XINPUT: &str = "errors.platform.x11_missing_xinput";
/// 鼠标侧键已被其他程序抓取（XGrabButton 返回 BadAccess），侧键绑定无法生效
pub const X11_MOUSE_BUTTON_BUSY: &str = "errors.platform.x11_mouse_button_busy";
/// X11 下中键用于粘贴 PRIMARY 选区，不拦截中键
pub const X11_MIDDLE_BUTTON: &str = "errors.platform.x11_middle_button";

/// 全部 key（单测校验命名空间与唯一性；i18n 检查另由 `scripts/check-i18n.mjs` 扫描源码完成）
pub const ALL: &[&str] = &[
    ACCESSIBILITY_MISSING,
    SCREEN_RECORDING_MISSING,
    INPUT_MONITORING_MISSING,
    HOOKS_NOT_COMPILED,
    NO_PERMISSION_MODEL,
    CLIPBOARD_TOO_LARGE,
    WAYLAND_SELECTION,
    WAYLAND_SIMULATED_COPY,
    WAYLAND_PASTE,
    WAYLAND_HOTKEYS,
    WAYLAND_CAPTURE,
    WAYLAND_CLIPBOARD_WATCH,
    WAYLAND_INPUT_HOOKS,
    WAYLAND_CLIPBOARD,
    WAYLAND_CLIPBOARD_BRIDGE,
    WAYLAND_FOCUS,
    NO_DISPLAY,
    X11_CONNECT_FAILED,
    X11_MISSING_XTEST,
    X11_MISSING_XFIXES,
    X11_MISSING_XINPUT,
    X11_MOUSE_BUTTON_BUSY,
    X11_MIDDLE_BUTTON,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_unique_and_namespaced() {
        let mut seen = std::collections::HashSet::new();
        for key in ALL {
            assert!(key.starts_with("errors.platform."), "{key}");
            assert!(
                key["errors.platform.".len()..]
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "{key}"
            );
            assert!(seen.insert(*key), "重复的 key {key}");
        }
    }
}
