//! 平台 trait。

use std::time::Duration;

#[cfg(feature = "mouse-hook")]
use wisp_types::Rect;
use wisp_types::{AppRef, ClipPayload, MonitorId, MonitorInfo, PermissionKind, PermissionSet, Point, RawClipboard};

use crate::error::PlatformError;
use crate::types::{
    ClickEvent, ClipSnapshot, HookHandle, LayoutInfo, ReadLimits, Selection, SelectionError, SuppressRange,
};
#[cfg(feature = "mouse-hook")]
use crate::types::{HookError, HookSpec, InputSignal, KeyChord};

/// 读取当前选中文本
pub trait SelectedText: Send + Sync {
    /// 阻塞调用，请在工作线程中调用。首选路径 → （按需）模拟复制回退
    fn get(&self, opts: &SelectionOpts) -> Result<Selection, SelectionError>;
    /// 浮标、中键拦截用：只读选区长度，绝不模拟复制；deadline 内拿不到结果就返回 None
    fn has_selection(&self, deadline: Duration) -> Option<bool>;
    /// 光标处是否是链接（中键拦截用；X11 恒返回 Some(false)）
    fn link_at_cursor(&self, deadline: Duration) -> Option<bool>;
}

/// 取词选项
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionOpts {
    pub allow_copy_fallback: bool,
    /// 默认 150 ms，可配 50–500
    pub copy_wait: Duration,
    /// 触发时记录的前台应用
    pub target: Option<AppRef>,
    /// 当前剪贴板内容已经在 Wisp 的历史里（由应用层按「正在记录且最近记录的就是当前内容」判定，不只是监听开关）：
    /// 剪贴板过大、无法备份时，只有这时才仍执行模拟复制——原内容可以从历史里找回
    pub history_enabled: bool,
}

impl Default for SelectionOpts {
    fn default() -> Self {
        Self {
            allow_copy_fallback: true,
            copy_wait: Duration::from_millis(150),
            target: None,
            history_enabled: true,
        }
    }
}

/// 写剪贴板并粘贴到目标应用
pub trait Paste: Send + Sync {
    /// 写入剪贴板（带 Wisp 私有标记 + Transient / 排除历史格式），返回写入后的 change_id
    fn write(&self, payload: &ClipPayload, opts: WriteOpts) -> Result<u64, PlatformError>;
    /// 激活目标应用 → 等待它到前台（≤100 ms）→ 处理残留修饰键 → 发送粘贴键（按映射表）
    fn paste_into(&self, target: &AppRef, plain: bool) -> Result<(), PasteError>;
    /// 预激活（Windows「先 SetForegroundWindow(prev)，再隐藏面板」）与交还焦点之前的只读检查
    /// （与 `paste_into` 的判定一致的保守子集）：Windows 只认 `AppRef.window`
    /// 记录的那个窗口（与 `paste_into` 同一个判定：属于同一进程，UWP 宿主 ApplicationFrameHost 里的应用也算）——
    /// 没有记录、已关闭、属于别的进程、最小化、隐藏、无响应时返回 Err（原因是英文简述，只进日志），不恢复最小化窗口、
    /// 不改用同一进程的其他窗口、不向目标发消息。其他平台不预激活，恒为 Ok
    fn prepare_target(&self, target: &AppRef) -> Result<(), &'static str> {
        let _ = target;
        Ok(())
    }
}

/// 写剪贴板选项
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WriteOpts {
    pub transient: bool,
    pub exclude_from_history: bool,
}

/// 粘贴失败原因
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PasteError {
    #[error("target app is gone")]
    TargetGone,
    #[error("target app did not come to front in time")]
    FocusTimeout,
    #[error("target app is elevated")]
    Elevated,
    #[error("permission missing")]
    PermissionMissing,
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("{0}")]
    Other(String),
}

/// 前台应用追踪
pub trait FocusTracker: Send + Sync {
    /// 已排除 Wisp 自己和 shell（Windows 用 WinEvent 记录最近的有效前台窗口）
    fn frontmost(&self) -> Option<AppRef>;
    fn activate(&self, app: &AppRef) -> Result<(), PlatformError>;
    /// 只有 Windows 有意义
    fn is_elevated(&self, app: &AppRef) -> bool;
}

/// 系统权限
pub trait Permissions: Send + Sync {
    /// 绝不弹系统对话框
    fn status(&self) -> PermissionSet;
    fn request(&self, kind: PermissionKind) -> Result<(), PlatformError>;
    fn open_settings(&self, kind: PermissionKind) -> Result<(), PlatformError>;
}

/// 应用信息
pub trait AppInfo: Send + Sync {
    fn icon_png(&self, app: &AppRef, size_px: u32) -> Option<Vec<u8>>;
    fn display_name(&self, app: &AppRef) -> String;
    /// 邻居探测用，一次性调用
    fn running_apps(&self) -> Vec<AppRef>;
    /// 「从正在运行的应用中选择」（排除名单选择器）用的应用列表——用户
    /// 认得出、会从里面复制内容的应用：macOS 普通应用 + 用户自己装的顶层菜单栏应用（去掉系统代理进程、嵌在别的应用里的
    /// 辅助进程，以及名字看得出是辅助进程的独立 helper / agent / updater 等）；Windows 有任务栏窗口（Alt-Tab 口径）的
    /// 进程——**系统目录下的进程只要有任务栏窗口照样列出**（记事本、远程桌面、任务管理器），UWP 宿主换成真正的应用——
    /// 再加上只有隐藏 / 工具窗口的「后台应用」（最小化到托盘的密码管理器、托盘常驻的剪贴板 / 截图工具；可执行文件不在
    /// 系统目录下、名字不是辅助进程或 WebView2 之类的运行时），它们的 `window` 为 None；X11 客户端
    /// 列表（去掉 dock / 桌面类窗口）+ 有 `.desktop` 条目的本用户进程。id 与剪贴板来源同一口径（bundle id / exe 名 / WM_CLASS），
    /// 顺序不保证、可能有重复（调用方去重、排序）。**阻塞**（一次性枚举，几到几十毫秒），在工作线程上调用。
    /// 默认实现退回 [`Self::running_apps`]
    fn user_apps(&self) -> Vec<AppRef> {
        self.running_apps()
    }
}

/// 显示器、光标与系统外观
pub trait CursorScreen: Send + Sync {
    fn monitors(&self) -> Vec<MonitorInfo>;
    /// 显示器内逻辑坐标
    fn cursor(&self) -> (MonitorId, Point);
    /// "#RRGGBB"
    fn system_accent(&self) -> Option<String>;
    fn reduce_motion(&self) -> bool;
    fn reduce_transparency(&self) -> bool;
    /// 系统当前是否为深色外观（解析 `ThemePref::System`、生成 boot 脚本用；启动时还没有任何窗口，不能依赖
    /// `Window::theme()`）。macOS 读 `NSApp.effectiveAppearance`，Windows 读注册表 `AppsUseLightTheme`，
    /// Linux 读 portal `org.freedesktop.appearance color-scheme`（取不到时退回 GTK 主题名）
    fn dark_mode(&self) -> bool;
    /// 任务栏 / 托盘区是否为浅色（选托盘图标用）：只有 Windows 返回 Some（注册表 `SystemUsesLightTheme`），
    /// macOS 用模板图、Linux 跟随面板主题，都返回 None
    fn taskbar_light(&self) -> Option<bool>;
    /// 通知区域图标的实际边长（物理像素；选一张恰好等大的托盘图标，系统就不必再重采样）：只有 Windows 返回 Some——
    /// 主显示器当前有效 DPI 下的 `SM_CXSMICON`（通知区域只在主任务栏上；100 % 16、150 % 24、200 % 32……），
    /// 每次调用都按当前缩放计算。macOS 用固定 18 pt 的模板图、Linux 的 StatusNotifierItem 宿主自行缩放，都返回 None。
    /// 只是几个进程内的系统查询（微秒级、不阻塞），可以在主线程上调用
    fn tray_icon_size(&self) -> Option<u32>;
    /// 监听系统外观变化（明暗、强调色、降低动效 / 透明度、Windows 任务栏明暗与显示缩放）。回调不带参数，调用方收到后
    /// 重新读取上面几项并广播 `appearance:changed`；可能合并或重复触发，调用方自行去重。返回的 Handle 被 drop 时注销。
    /// 必须事件驱动（macOS 分布式通知 / KVO，Windows `WM_SETTINGCHANGE` / `WM_DPICHANGED` / `WM_DISPLAYCHANGE`，
    /// Linux portal `SettingChanged`），空闲时零唤醒，**不得轮询**
    fn watch_appearance(&self, cb: Box<dyn Fn() + Send + Sync>) -> Result<HookHandle, PlatformError>;
}

/// 剪贴板读写与监听
pub trait ClipboardAccess: Send + Sync {
    fn change_id(&self) -> u64;
    /// 先检查隐私标记，命中则 concealed=true 且不读正文；过大的内容按类型截断或丢弃（文本保留前 1 MB 并标注截断；HTML / RTF 超过 256 KB 丢弃该格式；图片超过 20 MB 不记录）
    fn read(&self, limits: &ReadLimits) -> Result<RawClipboard, PlatformError>;
    /// None 表示过大或超时，放弃快照
    fn snapshot(&self, max_bytes: usize, deadline: Duration) -> Result<Option<ClipSnapshot>, PlatformError>;
    fn restore(&self, snap: ClipSnapshot) -> Result<u64, PlatformError>;
    /// 开始监听：回调只携带 change_id，读取剪贴板由调用方在工作线程完成；返回的 Handle 被 drop 时注销监听
    fn watch(&self, cb: Box<dyn Fn(u64) + Send + Sync>) -> Result<HookHandle, PlatformError>;
    /// 抑制 Wisp 自己造成的剪贴板变更
    fn suppress(&self, range: SuppressRange);
    /// 马上核对一次有没有新的变更（例如剪贴板面板即将显示）。只对轮询式的监听有意义——macOS
    /// 在用户空闲时把 changeCount 轮询放慢到 1 s / 5 s 一次，这里让它立即读一次并回到 4 Hz；有变更时照常经 `watch` 的回调
    /// 上报（不在调用线程上读取）。事件驱动的平台（Windows / X11）什么都不做
    fn poll_now(&self) {}
}

/// 外部点击监听
pub trait OutsideClick: Send + Sync {
    /// 翻译面板可见期间安装的全局鼠标按下/抬起监听（不需要权限）；drop 后注销
    fn watch(&self, cb: Box<dyn Fn(ClickEvent) + Send + Sync>) -> Result<HookHandle, PlatformError>;
}

/// 快捷键环境探测
pub trait HotkeyEnv: Send + Sync {
    /// macOS CopySymbolicHotKeys → 转成 global-hotkey 字符串；其他平台返回空
    fn system_hotkeys(&self) -> Vec<String>;
    /// { has_altgr, input_language_count }
    fn keyboard_layouts(&self) -> LayoutInfo;
    /// 平台层自带的全局快捷键后端。只有 Linux X11 会话返回 Some（x11rb 的 `XGrabKey`，
    /// 按键事件在平台层唯一的 X11 事件线程上分发，空闲零唤醒——取代 global-hotkey 0.8 每 50 ms 轮询一次的 X11 后端）；
    /// macOS / Windows / Wayland / 没有 X 显示时为 None（desktop 照旧用 tauri-plugin-global-shortcut，或报告不可用）
    fn key_grab(&self) -> Option<&dyn KeyGrab> {
        None
    }
}

/// 平台层的全局快捷键后端（见 [`HotkeyEnv::key_grab`]）。组合一律是规范串（`Combo::to_canonical`，与
/// tauri-plugin-global-shortcut 的 `Shortcut::from_str` 能解析的串相同）。注册 / 注销会与 X 服务器同步往返一次
/// （**阻塞**，亚毫秒到几毫秒），不要在按键回调里调用
pub trait KeyGrab: Send + Sync {
    /// 设置按键回调（替换之前的）：`(规范串, 是否按下)`。在平台层的回调线程上调用（不是 X11 事件线程），回调里不得
    /// 注册 / 注销；按住时的自动重复已经折叠掉（只有一次按下、一次抬起）
    fn set_handler(&self, handler: KeyGrabHandler);
    /// 注册（同一组合重复注册视为成功）。被别的客户端占用时 `Taken`；组合无法解析、当前键盘布局没有这个键时
    /// `Rejected`
    fn register(&self, combo: &str) -> Result<(), KeyGrabError>;
    /// 注销（没注册过时什么都不做）
    fn unregister(&self, combo: &str);
}

/// [`KeyGrab::set_handler`] 的回调：(规范串, 是否按下)
pub type KeyGrabHandler = Box<dyn Fn(&str, bool) + Send + Sync>;

/// [`KeyGrab::register`] 的失败原因
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyGrabError {
    /// 已被别的程序抓取（X11 `BadAccess`）
    #[error("hotkey already registered by another client")]
    Taken,
    /// 组合无法解析、布局里没有这个键、或连接不可用
    #[error("hotkey rejected: {0}")]
    Rejected(String),
}

/// 扩展触发的输入钩子
#[cfg(feature = "mouse-hook")]
pub trait InputHooks: Send + Sync {
    /// 按 spec 安装（或合并）钩子；返回的 Handle 被 drop 时注销钩子并结束线程
    fn install(&self, spec: HookSpec, sink: Box<dyn Fn(InputSignal) + Send + Sync>) -> Result<HookHandle, HookError>;
    /// 重放一次被 `paste_keys` 拦下的粘贴键（上层已写好要粘贴的内容）：带 Wisp 标记注入，自己的钩子放行；X11 先撤销
    /// 这一组的抓取、XTEST 发送、再抓回来。仍按着的修饰键保持不动（用户按住 ⌘ 连按 V 时，下一次按键照样带着 ⌘）。
    /// **阻塞**（X11 与服务器同步往返），在工作线程上调用
    fn replay_paste(&self, chord: KeyChord) -> Result<(), HookError>;
}

/// 浮标（原生小窗，无 WebView）
#[cfg(feature = "mouse-hook")]
pub trait BobberWindow: Send + Sync {
    fn show(&self, at: (MonitorId, Point), dark: bool, reduced_motion: bool, on_click: Box<dyn Fn() + Send + Sync>);
    fn hide(&self, animated: bool);
    fn rect(&self) -> Option<(MonitorId, Rect)>;
}
