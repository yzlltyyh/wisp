//! 平台层的通用错误。

use wisp_types::PermissionKind;

/// 平台调用失败
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlatformError {
    /// 缺少系统权限
    #[error("permission missing: {0:?}")]
    PermissionMissing(PermissionKind),
    /// 当前平台 / 会话不支持（附原因，便于日志与设置页展示）
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// 硬超时
    #[error("timed out")]
    Timeout,
    /// 资源被占用（例如 Windows OpenClipboard 重试后仍失败）
    #[error("busy: {0}")]
    Busy(String),
    #[error("{0}")]
    Other(String),
}

impl PlatformError {
    /// 尚未实现的能力（存根阶段使用）
    pub fn not_implemented(what: &str) -> Self {
        Self::Unsupported(format!("{what} is not implemented yet"))
    }
}
