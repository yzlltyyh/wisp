//! Esc guard：翻译 / OCR 面板不是 key window 时收不到键盘事件，可见期间临时注册全局 Esc；面板成为
//! key 或隐藏后立即注销（`WindowManager` 的外部点击守卫在 `wisp-panel-guards` 线程上调用 `HotkeyService::esc_guard`）。
//!
//! 按下时调用 `windows.panel_request_hide(HideReason::Escape)`（前端先播放退出动画）。全局 Esc 只在「面板可见且不是
//! key」这段时间存在，期间其他应用收不到 Esc——非 key 的面板收不到键盘事件，这是为了让 Esc 能关掉面板的取舍；录制快捷键期间不注册（录制器自己处理 Esc）。
//!
//! **截图会话（Overlay 显示中、还没 commit）期间同样注册**——Overlay 的退出不依赖页面：页面还没
//! 加载完、WebContent 卡住或崩溃、按键焦点留在另一块屏的 Overlay 上时，Esc 照样取消截图。按下时先交给编排器
//! （有截图会话就取消它），没有才按面板处理。

/// 全局 Esc 的规范串
pub const ESCAPE: &str = "Escape";

/// Esc guard 的状态
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EscGuard {
    /// 面板希望有全局 Esc
    pub wanted: bool,
    /// 截图会话（未 commit 的 Overlay）希望有全局 Esc
    pub overlay: bool,
    /// 当前已注册
    pub registered: bool,
}

/// 下一步要做的事
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscAction {
    Register,
    Unregister,
    Nothing,
}

impl EscGuard {
    /// 根据期望、录制状态与快捷键是否可用，决定注册还是注销
    pub fn next(&self, suspended: bool, available: bool) -> EscAction {
        let want = (self.wanted || self.overlay) && !suspended && available;
        match (want, self.registered) {
            (true, false) => EscAction::Register,
            (false, true) => EscAction::Unregister,
            _ => EscAction::Nothing,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_only_while_wanted_and_not_recording() {
        let mut g = EscGuard::default();
        assert_eq!(g.next(false, true), EscAction::Nothing);
        g.wanted = true;
        assert_eq!(g.next(false, true), EscAction::Register);
        assert_eq!(g.next(true, true), EscAction::Nothing, "录制期间不注册");
        assert_eq!(
            g.next(false, false),
            EscAction::Nothing,
            "快捷键不可用（Wayland）时不注册"
        );
        g.registered = true;
        assert_eq!(g.next(false, true), EscAction::Nothing);
        assert_eq!(g.next(true, true), EscAction::Unregister, "开始录制时注销");
        g.wanted = false;
        assert_eq!(g.next(false, true), EscAction::Unregister);
    }

    #[test]
    fn overlay_sessions_also_want_escape() {
        let mut g = EscGuard {
            overlay: true,
            ..EscGuard::default()
        };
        assert_eq!(g.next(false, true), EscAction::Register);
        g.registered = true;
        g.wanted = true;
        g.overlay = false;
        assert_eq!(g.next(false, true), EscAction::Nothing, "面板还要");
        g.wanted = false;
        assert_eq!(g.next(false, true), EscAction::Unregister);
    }

    #[test]
    fn escape_is_a_valid_shortcut() {
        use std::str::FromStr;
        assert!(tauri_plugin_global_shortcut::Shortcut::from_str(ESCAPE).is_ok());
    }
}
