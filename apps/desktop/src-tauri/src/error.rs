//! IPC 错误。所有命令返回 `Result<T, IpcError>`，前端封装为 `WispIpcError` 抛出。
//! `message` 只写英文简述，**禁止**包含正文、Key、请求头的值。

use serde::Serialize;
use wisp_types::LlmError;

/// IPC 错误码
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcErrorCode {
    InvalidArgument,
    NotFound,
    NotReady,
    Permission,
    Platform,
    Keychain,
    Io,
    Llm,
    Conflict,
    NotImplemented,
    Internal,
}

impl IpcErrorCode {
    /// 全部错误码（黄金样例逐个覆盖；新增变体时 `tests::every_code_is_listed` 会编译失败提醒同步）
    pub const ALL: [IpcErrorCode; 11] = [
        Self::InvalidArgument,
        Self::NotFound,
        Self::NotReady,
        Self::Permission,
        Self::Platform,
        Self::Keychain,
        Self::Io,
        Self::Llm,
        Self::Conflict,
        Self::NotImplemented,
        Self::Internal,
    ];
}

/// IPC 错误
#[derive(Debug, Clone, PartialEq, Serialize, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct IpcError {
    pub code: IpcErrorCode,
    pub message: String,
    pub llm: Option<LlmError>,
}

impl IpcError {
    /// 通用构造
    pub fn new(code: IpcErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            llm: None,
        }
    }

    /// 尚未实现（存根阶段）
    pub fn not_implemented(what: &str) -> Self {
        Self::new(IpcErrorCode::NotImplemented, format!("{what} is not implemented yet"))
    }

    /// 参数非法
    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new(IpcErrorCode::InvalidArgument, message)
    }

    /// 资源不存在
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(IpcErrorCode::NotFound, message)
    }

    /// 内部错误
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(IpcErrorCode::Internal, message)
    }

    /// 钥匙串错误（包括用户拒绝授权）
    pub fn keychain(message: impl Into<String>) -> Self {
        Self::new(IpcErrorCode::Keychain, message)
    }
}

impl From<LlmError> for IpcError {
    fn from(e: LlmError) -> Self {
        Self {
            code: IpcErrorCode::Llm,
            message: format!("{:?}", e.code),
            llm: Some(e),
        }
    }
}

impl From<std::io::Error> for IpcError {
    fn from(e: std::io::Error) -> Self {
        Self::new(IpcErrorCode::Io, e.to_string())
    }
}

impl From<tauri::Error> for IpcError {
    fn from(e: tauri::Error) -> Self {
        Self::internal(e.to_string())
    }
}

/// 平台层错误 → IpcError。`message` 的格式：
/// - `Unsupported(key)` → `{ code: platform, message: key }`：key 原样（`errors.platform.*`，前端可以直接本地化，
///   `useCopy` 据此退回 `navigator.clipboard`），**不加** `Display` 的 `unsupported: ` 前缀；
/// - `PermissionMissing(kind)` → `{ code: permission, message: errors.platform.<kind>_missing }`；
/// - `Timeout` → `{ code: platform, message: "timeout" }`，`Busy(_)` → `"busy"`（细节只进日志）；
/// - `Other(s)` → `{ code: platform, message: s }`（英文简述，不是 i18n key，前端显示通用文案）。
impl From<wisp_platform::PlatformError> for IpcError {
    fn from(e: wisp_platform::PlatformError) -> Self {
        use wisp_platform::{PlatformError, reason};
        use wisp_types::PermissionKind;
        match e {
            PlatformError::PermissionMissing(kind) => Self::new(
                IpcErrorCode::Permission,
                match kind {
                    PermissionKind::Accessibility => reason::ACCESSIBILITY_MISSING,
                    PermissionKind::ScreenRecording => reason::SCREEN_RECORDING_MISSING,
                    PermissionKind::InputMonitoring => reason::INPUT_MONITORING_MISSING,
                },
            ),
            PlatformError::Unsupported(key) => Self::new(IpcErrorCode::Platform, key),
            PlatformError::Timeout => Self::new(IpcErrorCode::Platform, "timeout"),
            PlatformError::Busy(what) => {
                log::debug!("platform busy: {what}");
                Self::new(IpcErrorCode::Platform, "busy")
            }
            PlatformError::Other(message) => Self::new(IpcErrorCode::Platform, message),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use wisp_types::ErrorCode;

    use super::*;

    /// 新增错误码时这里不再穷尽，编译失败提醒同步 `ALL`、TS 的 `IpcErrorCode` 与黄金样例
    #[test]
    fn every_code_is_listed() {
        for code in IpcErrorCode::ALL {
            match code {
                IpcErrorCode::InvalidArgument
                | IpcErrorCode::NotFound
                | IpcErrorCode::NotReady
                | IpcErrorCode::Permission
                | IpcErrorCode::Platform
                | IpcErrorCode::Keychain
                | IpcErrorCode::Io
                | IpcErrorCode::Llm
                | IpcErrorCode::Conflict
                | IpcErrorCode::NotImplemented
                | IpcErrorCode::Internal => {}
            }
        }
    }

    /// `Unsupported(key)` 的 message 是裸 key（前端 `useCopy` 按 key 精确匹配）
    #[test]
    fn platform_errors_carry_bare_keys() {
        use wisp_platform::{PlatformError, reason};
        let e = IpcError::from(PlatformError::Unsupported(reason::WAYLAND_CLIPBOARD_BRIDGE.to_owned()));
        assert_eq!(e.code, IpcErrorCode::Platform);
        assert_eq!(e.message, "errors.platform.wayland_clipboard_bridge");
        let e = IpcError::from(PlatformError::Unsupported(reason::WAYLAND_CLIPBOARD.to_owned()));
        assert_eq!(e.message, "errors.platform.wayland_clipboard");
        let e = IpcError::from(PlatformError::PermissionMissing(
            wisp_types::PermissionKind::Accessibility,
        ));
        assert_eq!(e.code, IpcErrorCode::Permission);
        assert_eq!(e.message, reason::ACCESSIBILITY_MISSING);
        assert_eq!(IpcError::from(PlatformError::Timeout).message, "timeout");
        assert_eq!(
            IpcError::from(PlatformError::Busy("OpenClipboard".to_owned())).message,
            "busy"
        );
        assert_eq!(IpcError::from(PlatformError::Other("x".to_owned())).message, "x");
    }

    fn fixture_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../src/shared/ipc/fixtures/desktop/IpcError.json")
    }

    /// desktop 侧的黄金样例（防止 Rust 与 TS 的类型漂移）：每个错误码一条，最后一条带 llm。
    /// 前端 `golden.test.ts` 用 shapes.ts 校验 `fixtures/desktop/*.json` 的键集合；`UPDATE_GOLDEN=1` 时重写
    #[test]
    fn golden_ipc_error() {
        let mut samples: Vec<IpcError> = IpcErrorCode::ALL
            .iter()
            .filter(|c| **c != IpcErrorCode::Llm)
            .map(|c| IpcError::new(*c, "example"))
            .collect();
        let mut llm = LlmError::new(ErrorCode::RateLimited, "Rate limit reached");
        llm.retryable = true;
        llm.status = Some(429);
        llm.retry_after_s = Some(20);
        samples.push(IpcError::from(llm));
        let expected = format!("{}\n", serde_json::to_string_pretty(&samples).expect("序列化失败"));
        let path = fixture_path();
        let update = std::env::var("UPDATE_GOLDEN").is_ok_and(|v| !v.is_empty() && v != "0");
        if update {
            std::fs::create_dir_all(path.parent().expect("父目录")).expect("创建目录失败");
            std::fs::write(&path, &expected).expect("写入失败");
            return;
        }
        let actual = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(
            actual, expected,
            "fixtures/desktop/IpcError.json 与 Rust 不一致（确认符合契约后 UPDATE_GOLDEN=1 cargo test -p wisp-desktop --lib error::）"
        );
    }
}
