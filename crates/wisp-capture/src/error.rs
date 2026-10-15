//! 捕获错误。

/// 捕获错误
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CaptureError {
    #[error("screen recording permission denied")]
    PermissionDenied,
    #[error("no displays")]
    NoDisplays,
    /// 用户主动取消（目前只有 Wayland 门户的交互截图会产生）：调用方静默结束，不显示错误卡片
    #[error("cancelled by the user")]
    Cancelled,
    #[error("backend: {0}")]
    Backend(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
}

impl CaptureError {
    /// 是否是用户主动取消（等价于匹配 [`CaptureError::Cancelled`]）
    pub fn is_cancelled(&self) -> bool {
        matches!(self, CaptureError::Cancelled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_cancelled_variant_counts_as_cancelled() {
        assert!(CaptureError::Cancelled.is_cancelled());
        assert!(!CaptureError::Backend("cancelled".to_owned()).is_cancelled());
        assert!(!CaptureError::Backend("X11 capture timed out".to_owned()).is_cancelled());
        assert!(!CaptureError::PermissionDenied.is_cancelled());
    }
}
