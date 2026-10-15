//! 应用与平台信息类型。

use serde::{Deserialize, Serialize};

/// 操作系统
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OsKind {
    Macos,
    Windows,
    Linux,
}

impl OsKind {
    /// 当前编译目标的操作系统（按 `std::env::consts::OS` 判断；未知系统按 Linux 处理）
    pub fn current() -> Self {
        match std::env::consts::OS {
            "macos" => Self::Macos,
            "windows" => Self::Windows,
            _ => Self::Linux,
        }
    }
}

/// 会话类型：macOS / Windows 为 native，Linux 区分 X11 与 Wayland
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    Native,
    X11,
    Wayland,
}

/// 对外部应用的引用
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AppRef {
    /// macOS bundle id；Windows 是 exe 文件名（小写）；X11 是 WM_CLASS 的 class（小写）
    pub id: String,
    /// X11 是 WM_CLASS 的 instance；其他平台为 null
    pub alt_id: Option<String>,
    pub name: String,
    pub pid: Option<u32>,
    /// bundle 路径或 exe 路径
    pub path: Option<String>,
    /// HWND 或 XID
    pub window: Option<u64>,
}

/// 单项能力是否可用
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Capability {
    pub available: bool,
    /// 不可用的原因（前端 i18n key，形如 `errors.platform.<reason>`，见 crate 文档的命名空间表）
    pub reason_key: Option<String>,
}

impl Capability {
    /// 可用
    pub fn yes() -> Self {
        Self {
            available: true,
            reason_key: None,
        }
    }

    /// 不可用，并附原因（i18n key）
    pub fn no(reason_key: impl Into<String>) -> Self {
        Self {
            available: false,
            reason_key: Some(reason_key.into()),
        }
    }
}

/// 平台能力集合
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Capabilities {
    pub global_hotkeys: Capability,
    pub selection: Capability,
    pub simulated_copy: Capability,
    pub paste_inject: Capability,
    pub capture_overlay: Capability,
    pub clipboard_watch: Capability,
    pub input_hooks: Capability,
}

/// 平台信息
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformInfo {
    pub os: OsKind,
    pub os_version: String,
    pub arch: String,
    pub session: SessionKind,
    pub capabilities: Capabilities,
}
