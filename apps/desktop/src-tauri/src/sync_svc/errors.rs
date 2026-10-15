//! 同步错误 → 状态里的错误种类、命令返回的 `IpcError`。
//!
//! 命令的错误消息都是界面文案的 key：口令与地址类用 `errors.sync.*`（就地显示在对应输入框下），其余用设置页状态行同一套
//! 「原因」文案。消息里不带地址、账号、口令或服务器返回的任何内容。

use wisp_sync::{ErrorKind, PassphraseIssue, ServerUrlError, SyncError};
use wisp_types::SyncErrorKind;

use crate::error::{IpcError, IpcErrorCode};

/// 引擎的错误种类 → 共享类型（两边同名同序）
pub fn kind_of(kind: ErrorKind) -> SyncErrorKind {
    match kind {
        ErrorKind::Network => SyncErrorKind::Network,
        ErrorKind::Auth => SyncErrorKind::Auth,
        ErrorKind::Forbidden => SyncErrorKind::Forbidden,
        ErrorKind::NotFound => SyncErrorKind::NotFound,
        ErrorKind::Quota => SyncErrorKind::Quota,
        ErrorKind::Server => SyncErrorKind::Server,
        ErrorKind::HttpsRequired => SyncErrorKind::HttpsRequired,
        ErrorKind::Passphrase => SyncErrorKind::Passphrase,
        ErrorKind::VaultReset => SyncErrorKind::VaultReset,
        ErrorKind::FormatTooNew => SyncErrorKind::FormatTooNew,
        ErrorKind::CredentialsMissing => SyncErrorKind::CredentialsMissing,
        ErrorKind::Keychain => SyncErrorKind::Keychain,
        ErrorKind::Corrupt => SyncErrorKind::Corrupt,
        ErrorKind::Internal => SyncErrorKind::Internal,
    }
}

/// 服务器地址不合法
pub fn url_error(e: ServerUrlError) -> IpcError {
    match e {
        ServerUrlError::HttpsRequired => IpcError::invalid_argument("errors.sync.https_required"),
        _ => IpcError::invalid_argument("errors.sync.invalid_url"),
    }
}

/// 同步未开启
pub fn not_connected() -> IpcError {
    IpcError::new(IpcErrorCode::NotReady, "errors.sync.not_connected")
}

/// 连接、断开或清除正在进行
pub fn busy() -> IpcError {
    IpcError::new(IpcErrorCode::Conflict, "errors.sync.busy")
}

/// 没有与地址、账号匹配的已保存密码
pub fn credentials_missing() -> IpcError {
    IpcError::new(IpcErrorCode::NotFound, "errors.sync.credentials_missing")
}

/// 某种失败的界面文案 key（设置页状态行的「原因」）
pub fn reason_key(kind: SyncErrorKind) -> &'static str {
    match kind {
        SyncErrorKind::Network => "settings.sync.error.network.reason",
        SyncErrorKind::Auth => "settings.sync.error.auth.reason",
        SyncErrorKind::Forbidden => "settings.sync.error.forbidden.reason",
        SyncErrorKind::NotFound => "settings.sync.error.not_found.reason",
        SyncErrorKind::Quota => "settings.sync.error.quota.reason",
        SyncErrorKind::Server => "settings.sync.error.server.reason",
        SyncErrorKind::HttpsRequired => "errors.sync.https_required",
        SyncErrorKind::Passphrase => "errors.sync.passphrase",
        SyncErrorKind::VaultReset => "settings.sync.error.vault_reset.reason",
        SyncErrorKind::FormatTooNew => "settings.sync.error.format_too_new.reason",
        SyncErrorKind::CredentialsMissing => "errors.sync.credentials_missing",
        SyncErrorKind::Keychain => "settings.sync.error.keychain.reason",
        SyncErrorKind::Corrupt => "settings.sync.error.corrupt.reason",
        SyncErrorKind::Internal => "settings.sync.error.internal.reason",
    }
}

fn code_of(kind: SyncErrorKind) -> IpcErrorCode {
    match kind {
        SyncErrorKind::Network | SyncErrorKind::Server => IpcErrorCode::Io,
        SyncErrorKind::Auth | SyncErrorKind::Forbidden => IpcErrorCode::Permission,
        SyncErrorKind::NotFound | SyncErrorKind::CredentialsMissing => IpcErrorCode::NotFound,
        SyncErrorKind::HttpsRequired => IpcErrorCode::InvalidArgument,
        SyncErrorKind::Passphrase | SyncErrorKind::VaultReset | SyncErrorKind::FormatTooNew => IpcErrorCode::Conflict,
        SyncErrorKind::Keychain => IpcErrorCode::Keychain,
        SyncErrorKind::Quota | SyncErrorKind::Corrupt | SyncErrorKind::Internal => IpcErrorCode::Internal,
    }
}

/// 命令里的同步错误 → IpcError
pub fn ipc_error(e: &SyncError) -> IpcError {
    match e {
        SyncError::PassphraseRule(PassphraseIssue::TooShort) => {
            IpcError::invalid_argument("errors.sync.passphrase_weak")
        }
        SyncError::PassphraseRule(PassphraseIssue::Reused) => {
            IpcError::invalid_argument("errors.sync.passphrase_reused")
        }
        SyncError::ServerUrl(u) => url_error(*u),
        _ => {
            let kind = kind_of(e.kind());
            IpcError::new(code_of(kind), reason_key(kind))
        }
    }
}

#[cfg(test)]
mod tests {
    use wisp_sync::RemoteError;

    use super::*;

    #[test]
    fn every_kind_matches_by_name() {
        for kind in [
            ErrorKind::Network,
            ErrorKind::Auth,
            ErrorKind::Forbidden,
            ErrorKind::NotFound,
            ErrorKind::Quota,
            ErrorKind::Server,
            ErrorKind::HttpsRequired,
            ErrorKind::Passphrase,
            ErrorKind::VaultReset,
            ErrorKind::FormatTooNew,
            ErrorKind::CredentialsMissing,
            ErrorKind::Keychain,
            ErrorKind::Corrupt,
            ErrorKind::Internal,
        ] {
            assert_eq!(kind.as_str(), kind_of(kind).as_str());
            assert_eq!(kind.is_persistent(), kind_of(kind).is_persistent());
        }
    }

    #[test]
    fn command_errors_are_message_keys() {
        assert_eq!(
            ipc_error(&SyncError::PassphraseRule(PassphraseIssue::TooShort)).message,
            "errors.sync.passphrase_weak"
        );
        assert_eq!(ipc_error(&SyncError::Passphrase).message, "errors.sync.passphrase");
        assert_eq!(ipc_error(&SyncError::Passphrase).code, IpcErrorCode::Conflict);
        assert_eq!(
            ipc_error(&SyncError::Remote(RemoteError::Status(401))).message,
            "settings.sync.error.auth.reason"
        );
        assert_eq!(
            ipc_error(&SyncError::ServerUrl(ServerUrlError::Query)).message,
            "errors.sync.invalid_url"
        );
    }
}
