//! 应用内命令总线：托盘、快捷键、扩展触发、CLI、引导页把动作投递到这里，
//! 消费循环由 `Orchestrator::spawn` 实现。

use std::fmt;

use tokio::sync::mpsc;
use wisp_types::{HotkeyAction, WindowTarget};

/// 应用内命令
#[derive(Clone, PartialEq, Eq)]
pub enum Command {
    Action {
        action: HotkeyAction,
        origin: TriggerOrigin,
    },
    TranslateWithStyle {
        style_id: String,
        origin: TriggerOrigin,
    },
    TranslateText {
        text: String,
        origin: TriggerOrigin,
    },
    /// 识别一张现成图片里的文字（剪贴板图片条目「识别文字」）：与截图 OCR 共用同一段启动流程，没有截图与框选
    OcrImage {
        image: wisp_capture::RgbaImage,
        origin: TriggerOrigin,
    },
    OpenWindow(WindowTarget),
    ToggleClipboardPause,
    ClearClipboard {
        keep_pinned: bool,
    },
    CheckUpdate,
    Quit,
}

// 手写 Debug：禁止把正文写进日志，TranslateText 只输出字符数，OcrImage 只输出尺寸
impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Action { action, origin } => f
                .debug_struct("Action")
                .field("action", action)
                .field("origin", origin)
                .finish(),
            Self::TranslateWithStyle { style_id, origin } => f
                .debug_struct("TranslateWithStyle")
                .field("style_id", style_id)
                .field("origin", origin)
                .finish(),
            Self::TranslateText { text, origin } => f
                .debug_struct("TranslateText")
                .field("chars", &text.chars().count())
                .field("origin", origin)
                .finish(),
            Self::OcrImage { image, origin } => f
                .debug_struct("OcrImage")
                .field("width", &image.width())
                .field("height", &image.height())
                .field("origin", origin)
                .finish(),
            Self::OpenWindow(t) => f.debug_tuple("OpenWindow").field(t).finish(),
            Self::ToggleClipboardPause => f.write_str("ToggleClipboardPause"),
            Self::ClearClipboard { keep_pinned } => f
                .debug_struct("ClearClipboard")
                .field("keep_pinned", keep_pinned)
                .finish(),
            Self::CheckUpdate => f.write_str("CheckUpdate"),
            Self::Quit => f.write_str("Quit"),
        }
    }
}

/// 触发来源
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TriggerOrigin {
    Hotkey,
    Tray,
    Bobber,
    DoubleTap,
    MouseButton,
    Cli,
    /// 引导页「试一试」（`trigger_action`，origin = onboarding）
    Onboarding,
    /// 剪贴板面板把条目送去翻译（`trigger_action`，origin = clipboard）
    Clipboard,
    /// 面板里的按钮（截图失败卡片的「重新截图」，`trigger_action`，origin = panel）
    Panel,
    /// 设置 › OCR › 识别历史的「重新翻译」（`trigger_action`，origin = ocr_history）
    OcrHistory,
}

impl From<wisp_types::ActionOrigin> for TriggerOrigin {
    fn from(o: wisp_types::ActionOrigin) -> Self {
        match o {
            wisp_types::ActionOrigin::Onboarding => Self::Onboarding,
            wisp_types::ActionOrigin::Clipboard => Self::Clipboard,
            wisp_types::ActionOrigin::Panel => Self::Panel,
            wisp_types::ActionOrigin::OcrHistory => Self::OcrHistory,
        }
    }
}

impl TriggerOrigin {
    /// 日志与 `LatencyMarks.origin` 用的短名
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hotkey => "hotkey",
            Self::Tray => "tray",
            Self::Bobber => "bobber",
            Self::DoubleTap => "double_tap",
            Self::MouseButton => "mouse_button",
            Self::Cli => "cli",
            Self::Onboarding => "onboarding",
            Self::Clipboard => "clipboard",
            Self::Panel => "panel",
            Self::OcrHistory => "ocr_history",
        }
    }
}

/// 命令总线的发送端（可廉价克隆）
#[derive(Clone)]
pub struct Bus {
    pub tx: mpsc::UnboundedSender<Command>,
}

impl Bus {
    /// 新建总线，返回发送端与接收端（接收端交给 `Orchestrator::spawn`）
    pub fn new() -> (Self, mpsc::UnboundedReceiver<Command>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }

    /// 投递命令；消费端已退出（应用正在退出）时只记日志
    pub fn send(&self, cmd: Command) {
        if let Err(e) = self.tx.send(cmd) {
            log::warn!("bus closed, dropped command: {:?}", e.0);
        }
    }
}
