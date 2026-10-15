//! 平台层的数据类型。扩展触发相关的类型不受 `mouse-hook` feature 控制，只有 trait 与 Platform 字段受控。

use std::any::Any;
use std::fmt;
use std::time::Duration;

use wisp_types::{Anchor, AppRef, HotkeyAction, ModifierKey, MonitorId, MouseButtonKind, PermissionKind, Point};

/// 取词路径
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaptureMethod {
    /// macOS 辅助功能 API
    Ax,
    /// Windows UI Automation
    Uia,
    /// X11 PRIMARY selection
    Primary,
    /// 模拟复制回退
    Copy,
}

impl CaptureMethod {
    /// 日志与开发者模式显示用的短名
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ax => "ax",
            Self::Uia => "uia",
            Self::Primary => "primary",
            Self::Copy => "copy",
        }
    }
}

/// 取词结果
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    /// 已规范化：CRLF / CR → LF，去 NUL，超过 1 MB 截断
    pub text: String,
    pub truncated: bool,
    /// 选区矩形（拿不到时为 None，上层退回鼠标位置）
    pub anchor: Option<Anchor>,
    /// 触发时记录的前台应用
    pub source: Option<AppRef>,
    pub method: CaptureMethod,
    pub elapsed_ms: u32,
}

/// 取词失败原因
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SelectionError {
    #[error("selection is empty")]
    Empty,
    #[error("permission missing: {0:?}")]
    PermissionMissing(PermissionKind),
    #[error("target app is elevated")]
    Elevated,
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("timed out")]
    Timeout,
    #[error("{0}")]
    Other(String),
}

/// 剪贴板全类型快照（不透明；各平台实现自行决定内部结构）
pub struct ClipSnapshot {
    data: Box<dyn Any + Send + Sync>,
    bytes: usize,
}

impl ClipSnapshot {
    /// 由平台实现构造：`data` 是该平台私有的快照结构，`bytes` 是快照的总字节数
    #[doc(hidden)]
    pub fn new<T: Any + Send + Sync>(data: T, bytes: usize) -> Self {
        Self {
            data: Box::new(data),
            bytes,
        }
    }

    /// 由平台实现取回私有结构；类型不符时返回 None
    #[doc(hidden)]
    pub fn into_inner<T: Any + Send + Sync>(self) -> Option<T> {
        self.data.downcast::<T>().ok().map(|b| *b)
    }

    /// 快照总字节数
    pub fn byte_len(&self) -> usize {
        self.bytes
    }
}

impl fmt::Debug for ClipSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClipSnapshot")
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

/// 读取剪贴板时的上限
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadLimits {
    /// 文本超过该值保留前 N 字节并标注 text_truncated（默认 1 MB）
    pub max_text_bytes: usize,
    /// HTML / RTF 各超过该值就丢弃该格式（默认 256 KB）
    pub max_rich_bytes: usize,
    /// 图片超过该值不读取（默认 20 MB）
    pub max_image_bytes: usize,
}

impl Default for ReadLimits {
    fn default() -> Self {
        Self {
            max_text_bytes: 1024 * 1024,
            max_rich_bytes: 256 * 1024,
            max_image_bytes: 20 * 1024 * 1024,
        }
    }
}

/// 抑制区间：change_id ≥ from_change_id 且在 ttl 内的变更视为 Wisp 自身造成
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SuppressRange {
    pub from_change_id: u64,
    pub ttl: Duration,
}

/// 钩子 / 监听的句柄：Drop 时注销
pub struct HookHandle {
    on_drop: Option<Box<dyn FnOnce() + Send>>,
}

impl HookHandle {
    /// Drop 时执行 `unregister`
    pub fn new(unregister: impl FnOnce() + Send + 'static) -> Self {
        Self {
            on_drop: Some(Box::new(unregister)),
        }
    }

    /// 无需注销的空句柄
    pub fn noop() -> Self {
        Self { on_drop: None }
    }
}

impl Drop for HookHandle {
    fn drop(&mut self) {
        if let Some(f) = self.on_drop.take() {
            f();
        }
    }
}

impl fmt::Debug for HookHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HookHandle")
            .field("armed", &self.on_drop.is_some())
            .finish()
    }
}

/// 鼠标按下 / 抬起
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClickKind {
    Down,
    Up,
}

/// 鼠标按键
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
    Other,
}

/// 全局鼠标事件（外部点击判定用）
#[derive(Debug, Clone, PartialEq)]
pub struct ClickEvent {
    pub kind: ClickKind,
    /// 显示器内逻辑坐标
    pub at: (MonitorId, Point),
    pub button: PointerButton,
}

/// 双击修饰键的参数
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DoubleTapSpec {
    pub key: ModifierKey,
    /// 200 / 300 / 400 ms
    pub interval: Duration,
}

/// 鼠标额外按键的参数：某键为 None 表示不拦截
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MouseSpec {
    /// 长按 ≥400 ms 模式
    pub long_press: bool,
    pub middle: Option<HotkeyAction>,
    pub back: Option<HotkeyAction>,
    pub forward: Option<HotkeyAction>,
}

/// 钩子安装规格（多项共用一个钩子线程）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HookSpec {
    pub bobber: bool,
    pub double_tap: Option<DoubleTapSpec>,
    pub mouse: Option<MouseSpec>,
}

impl HookSpec {
    /// 是否什么都不需要监听（此时不应安装任何钩子）
    pub fn is_empty(&self) -> bool {
        !self.bobber && self.double_tap.is_none() && self.mouse.is_none()
    }
}

/// 浮标消失原因
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DismissReason {
    /// 4 s 无交互
    Timeout,
    /// 光标离开浮标中心 >80 px
    MovedAway,
    KeyPress,
    Scroll,
    /// 在浮标外按下鼠标
    MouseDown,
    /// 前台应用切换
    AppSwitched,
}

/// 钩子产出的信号（回调只做 O(1) 工作后转交工作线程）
#[derive(Debug, Clone, PartialEq)]
pub enum InputSignal {
    /// mouse-up 后的浮标候选位置（显示器内逻辑坐标），由上层继续做选区判定
    BobberCandidate {
        at: (MonitorId, Point),
    },
    BobberDismiss(DismissReason),
    DoubleTap,
    MouseAction {
        button: MouseButtonKind,
        action: HotkeyAction,
    },
}

/// 钩子安装失败
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HookError {
    #[error("permission missing: {0:?}")]
    PermissionMissing(PermissionKind),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("{0}")]
    Other(String),
}

/// 键盘布局信息
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LayoutInfo {
    pub has_altgr: bool,
    pub input_language_count: u32,
    /// Alt+Shift 会切换输入语言 / 布局——Windows 装了多种输入语言时（系统默认的左 Alt+Shift）；
    /// X11 的 XKB 选项含 `grp:*alt*shift*` 时；macOS 恒为 false（切换输入源是 ⌃Space / Fn / 大写锁定，⌥⇧ 不切换）
    pub alt_shift_switches: bool,
}
