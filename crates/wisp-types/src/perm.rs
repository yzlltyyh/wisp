//! 系统权限类型。

use serde::{Deserialize, Serialize};

/// 权限种类（macOS TCC）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionKind {
    Accessibility,
    ScreenRecording,
    InputMonitoring,
}

/// 权限状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionState {
    Granted,
    Denied,
    NotDetermined,
    NotApplicable,
}

/// 三项权限的状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PermissionSet {
    pub accessibility: PermissionState,
    pub screen_recording: PermissionState,
    pub input_monitoring: PermissionState,
    /// 更新后授权失效：签名身份变化时出现（0.1.1 及更早是 ad-hoc 签名，每个版本的 cdhash 都不同；以后换证书也一样）
    pub lost_after_update: bool,
}

impl PermissionSet {
    /// 全部为 not_applicable（非 macOS 平台）
    pub const fn not_applicable() -> Self {
        Self {
            accessibility: PermissionState::NotApplicable,
            screen_recording: PermissionState::NotApplicable,
            input_monitoring: PermissionState::NotApplicable,
            lost_after_update: false,
        }
    }

    /// 取某一项的状态
    pub const fn get(&self, kind: PermissionKind) -> PermissionState {
        match kind {
            PermissionKind::Accessibility => self.accessibility,
            PermissionKind::ScreenRecording => self.screen_recording,
            PermissionKind::InputMonitoring => self.input_monitoring,
        }
    }
}
