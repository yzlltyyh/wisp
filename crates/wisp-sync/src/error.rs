//! 错误类型。
//!
//! 所有错误的 `Display` 只由固定词汇组成（错误种类、HTTP 状态码、`io::ErrorKind`、SQLite 错误码），
//! 不含服务器地址、账号、口令、密钥、远端路径、设备名或任何记录内容，因此可以直接写进日志。
//! 第三方错误（serde_json、reqwest 等）的消息可能夹带数据片段或 URL，一律在边界处折叠成本模块的变体，不原样保留。

use std::io;

/// 同步错误的种类，与界面上的错误分类一一对应（桌面端逐项映射到 IPC 类型）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// 网络不可达、DNS、TLS、超时
    Network,
    /// 401：账号或应用密码不对
    Auth,
    /// 403
    Forbidden,
    /// 404 / 409 / 410：地址或目录不存在
    NotFound,
    /// 507 / 413：空间不足
    Quota,
    /// 其他 HTTP 失败、响应无法解析、不允许的重定向
    Server,
    /// 非局域网地址使用了 http
    HttpsRequired,
    /// 同步密码不对，或不满足规则
    Passphrase,
    /// 服务器上的同步数据已被清除或替换
    VaultReset,
    /// 服务器上的数据格式比本程序新
    FormatTooNew,
    /// 本机没有可用的凭据（需要重新连接）
    CredentialsMissing,
    /// 钥匙串读写被拒绝
    Keychain,
    /// 某个文件解密或解析失败
    Corrupt,
    /// 程序内部错误（本地数据库、文件读写等）
    Internal,
}

impl ErrorKind {
    /// snake_case 名称，与 IPC 的枚举值一致
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::Auth => "auth",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::Quota => "quota",
            Self::Server => "server",
            Self::HttpsRequired => "https_required",
            Self::Passphrase => "passphrase",
            Self::VaultReset => "vault_reset",
            Self::FormatTooNew => "format_too_new",
            Self::CredentialsMissing => "credentials_missing",
            Self::Keychain => "keychain",
            Self::Corrupt => "corrupt",
            Self::Internal => "internal",
        }
    }

    /// 持续性错误：自动重试不会好转，暂停自动同步直到用户操作；其余种类按暂时性失败退避重试
    pub const fn is_persistent(self) -> bool {
        matches!(
            self,
            Self::Auth
                | Self::Forbidden
                | Self::NotFound
                | Self::Quota
                | Self::Passphrase
                | Self::VaultReset
                | Self::FormatTooNew
                | Self::CredentialsMissing
                | Self::Keychain
                | Self::HttpsRequired
        )
    }
}

/// 远端（WebDAV）操作的错误
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RemoteError {
    /// 非成功的 HTTP 状态码（成功语义已由各方法处理，例如 GET 的 404 返回 `None`）
    #[error("http status {0}")]
    Status(u16),
    #[error("request timed out")]
    Timeout,
    #[error("dns lookup failed")]
    Dns,
    #[error("tls handshake failed")]
    Tls,
    #[error("connection failed")]
    Connect,
    /// 不允许的重定向（非 GET 遇到 3xx、跨主机降级到 http、超过次数）
    #[error("redirect not allowed")]
    Redirect,
    /// 服务器把 http 跳转到了 https：应改用 https 地址
    #[error("server requires https")]
    HttpsRequired,
    /// 响应无法解析（PROPFIND 多状态响应格式不对等）
    #[error("malformed server response")]
    Protocol,
    /// 读写本地文件失败（上传源文件、下载目标文件）
    #[error("local file io: {0:?}")]
    Io(io::ErrorKind),
    /// 不应出现的内部状态；参数是固定描述
    #[error("internal: {0}")]
    Internal(&'static str),
}

impl RemoteError {
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Status(401) => ErrorKind::Auth,
            Self::Status(403) => ErrorKind::Forbidden,
            Self::Status(404 | 409 | 410) => ErrorKind::NotFound,
            Self::Status(413 | 507) => ErrorKind::Quota,
            Self::Status(_) => ErrorKind::Server,
            Self::Timeout | Self::Dns | Self::Tls | Self::Connect => ErrorKind::Network,
            Self::Redirect | Self::Protocol => ErrorKind::Server,
            Self::HttpsRequired => ErrorKind::HttpsRequired,
            Self::Io(_) | Self::Internal(_) => ErrorKind::Internal,
        }
    }

    /// 暂时性失败（网络、超时、5xx、429 等）：退避后自动重试
    pub fn is_transient(&self) -> bool {
        !self.kind().is_persistent()
    }

    /// 给界面「详情」用的短描述，只由固定词汇组成
    pub fn detail(&self) -> String {
        match self {
            Self::Status(code) => format!("http {code}"),
            Self::Timeout => "timeout".into(),
            Self::Dns => "dns".into(),
            Self::Tls => "tls".into(),
            Self::Connect => "connect".into(),
            Self::Redirect => "redirect".into(),
            Self::HttpsRequired => "https".into(),
            Self::Protocol => "protocol".into(),
            Self::Io(kind) => format!("io {kind:?}"),
            Self::Internal(what) => format!("internal {what}"),
        }
    }
}

impl From<io::Error> for RemoteError {
    fn from(e: io::Error) -> Self {
        // 只保留种类：自定义 io::Error 的消息可能带路径
        Self::Io(e.kind())
    }
}

/// 本机存储（桌面端 [`LocalStore`](crate::engine::LocalStore) 实现）报告的错误
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LocalError {
    /// 本机存储暂时不可用（剪贴板库尚未打开、配置文件当前是无效 JSON 等）：稍后重试，不算失败
    #[error("local store unavailable")]
    Unavailable,
    /// 钥匙串读写被拒绝
    #[error("keychain access denied")]
    Keychain,
    #[error("local io: {0:?}")]
    Io(io::ErrorKind),
    /// 其他失败；参数必须是固定描述，不得包含用户数据
    #[error("local store: {0}")]
    Failed(&'static str),
}

impl LocalError {
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Keychain => ErrorKind::Keychain,
            Self::Unavailable | Self::Io(_) | Self::Failed(_) => ErrorKind::Internal,
        }
    }
}

impl From<io::Error> for LocalError {
    fn from(e: io::Error) -> Self {
        Self::Io(e.kind())
    }
}

/// 同步口令不满足规则的原因
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PassphraseIssue {
    /// 少于 12 个字符
    #[error("too short")]
    TooShort,
    /// 与 WebDAV 密码、账号或服务器主机名相同（忽略大小写）
    #[error("reuses an account credential")]
    Reused,
}

impl PassphraseIssue {
    /// 前端 i18n key 的末段（`errors.sync.<reason>`）
    pub const fn reason(self) -> &'static str {
        match self {
            Self::TooShort => "passphrase_weak",
            Self::Reused => "passphrase_reused",
        }
    }
}

/// 服务器地址不合法的原因
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ServerUrlError {
    /// 超过 1 KB
    #[error("too long")]
    TooLong,
    /// 不是合法的 URL 或没有主机
    #[error("invalid")]
    Invalid,
    /// 不是 http / https
    #[error("unsupported scheme")]
    Scheme,
    /// 地址里带了账号或密码
    #[error("contains user info")]
    UserInfo,
    #[error("contains a query")]
    Query,
    #[error("contains a fragment")]
    Fragment,
    /// 非局域网地址使用 http
    #[error("https required")]
    HttpsRequired,
}

impl ServerUrlError {
    /// 前端 i18n key 的末段（`errors.sync.<reason>`）
    pub const fn reason(self) -> &'static str {
        match self {
            Self::HttpsRequired => "https_required",
            Self::TooLong | Self::Invalid | Self::Scheme | Self::UserInfo | Self::Query | Self::Fragment => {
                "invalid_url"
            }
        }
    }
}

/// wisp-sync 对外的统一错误
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("remote: {0}")]
    Remote(#[from] RemoteError),
    #[error("local store: {0}")]
    Local(#[from] LocalError),
    /// 本机副本数据库（sync.db）出错；Display 只带 SQLite 错误码
    #[error("replica database: {}", db_code(.0))]
    Db(#[source] rusqlite::Error),
    #[error("io: {0:?}")]
    Io(io::ErrorKind),
    /// 同步密码与服务器上的数据不匹配
    #[error("wrong sync passphrase")]
    Passphrase,
    /// 新设置的同步密码不满足规则
    #[error("sync passphrase rejected: {0}")]
    PassphraseRule(PassphraseIssue),
    #[error("invalid server url: {0}")]
    ServerUrl(ServerUrlError),
    /// 服务器上的同步数据已被清除或换成了另一份
    #[error("sync data on the server was reset")]
    VaultReset,
    /// 服务器上的数据格式（或 KDF 参数）超出本版本支持的范围
    #[error("sync data on the server uses a newer format")]
    FormatTooNew,
    /// 解密失败、认证标签不符、明文无法解析
    #[error("sync data is corrupt")]
    Corrupt,
    /// 尚未解锁（没有可用的密钥或凭据）
    #[error("sync is not unlocked")]
    Locked,
    /// 不应出现的内部状态，或尚未实现的功能；参数是固定描述
    #[error("internal: {0}")]
    Internal(&'static str),
}

impl SyncError {
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Remote(e) => e.kind(),
            Self::Local(e) => e.kind(),
            Self::Db(_) | Self::Io(_) | Self::Internal(_) => ErrorKind::Internal,
            Self::Passphrase | Self::PassphraseRule(_) => ErrorKind::Passphrase,
            Self::ServerUrl(ServerUrlError::HttpsRequired) => ErrorKind::HttpsRequired,
            Self::ServerUrl(_) => ErrorKind::Internal,
            Self::VaultReset => ErrorKind::VaultReset,
            Self::FormatTooNew => ErrorKind::FormatTooNew,
            Self::Corrupt => ErrorKind::Corrupt,
            Self::Locked => ErrorKind::CredentialsMissing,
        }
    }

    /// 暂时性失败：退避后自动重试；否则暂停自动同步，等用户处理
    pub fn is_transient(&self) -> bool {
        !self.kind().is_persistent()
    }

    /// 给界面「详情」用的短描述，只由固定词汇组成；没有可补充的信息时为 `None`
    pub fn detail(&self) -> Option<String> {
        match self {
            Self::Remote(e) => Some(e.detail()),
            Self::Local(LocalError::Io(kind)) | Self::Io(kind) => Some(format!("io {kind:?}")),
            Self::Local(LocalError::Failed(what)) | Self::Internal(what) => Some((*what).to_string()),
            Self::Db(e) => Some(format!("sqlite {}", db_code(e))),
            _ => None,
        }
    }
}

impl From<io::Error> for SyncError {
    fn from(e: io::Error) -> Self {
        Self::Io(e.kind())
    }
}

impl From<rusqlite::Error> for SyncError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Db(e)
    }
}

/// SQLite 错误只取错误码：rusqlite 的部分错误消息里可能带列值或 SQL 片段
fn db_code(e: &rusqlite::Error) -> String {
    match e.sqlite_error_code() {
        Some(code) => format!("{code:?}"),
        None => "client".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes_map_to_kinds() {
        assert_eq!(RemoteError::Status(401).kind(), ErrorKind::Auth);
        assert_eq!(RemoteError::Status(403).kind(), ErrorKind::Forbidden);
        assert_eq!(RemoteError::Status(404).kind(), ErrorKind::NotFound);
        assert_eq!(RemoteError::Status(409).kind(), ErrorKind::NotFound);
        assert_eq!(RemoteError::Status(507).kind(), ErrorKind::Quota);
        assert_eq!(RemoteError::Status(503).kind(), ErrorKind::Server);
        assert_eq!(RemoteError::Status(429).kind(), ErrorKind::Server);
        assert_eq!(RemoteError::Timeout.kind(), ErrorKind::Network);
        assert_eq!(RemoteError::HttpsRequired.kind(), ErrorKind::HttpsRequired);
    }

    #[test]
    fn transient_versus_persistent() {
        assert!(RemoteError::Timeout.is_transient());
        assert!(RemoteError::Status(500).is_transient());
        assert!(RemoteError::Status(429).is_transient());
        assert!(!RemoteError::Status(401).is_transient());
        assert!(!RemoteError::Status(507).is_transient());
        assert!(!SyncError::Passphrase.is_transient());
        assert!(!SyncError::VaultReset.is_transient());
        assert!(!SyncError::Local(LocalError::Keychain).is_transient());
        assert!(SyncError::Remote(RemoteError::Connect).is_transient());
        assert!(!SyncError::Locked.is_transient());
    }

    #[test]
    fn details_use_fixed_vocabulary() {
        assert_eq!(
            SyncError::from(RemoteError::Status(502)).detail().as_deref(),
            Some("http 502")
        );
        assert_eq!(
            SyncError::from(io::Error::new(io::ErrorKind::PermissionDenied, "/secret/path"))
                .detail()
                .as_deref(),
            Some("io PermissionDenied")
        );
        // 自定义 io 错误的消息（可能是路径）不会进入 Display
        let e = SyncError::from(io::Error::other("/home/someone/file"));
        assert!(!e.to_string().contains("someone"));
        assert_eq!(SyncError::Passphrase.detail(), None);
    }

    #[test]
    fn kind_names_are_snake_case() {
        assert_eq!(ErrorKind::HttpsRequired.as_str(), "https_required");
        assert_eq!(ErrorKind::FormatTooNew.as_str(), "format_too_new");
        assert_eq!(PassphraseIssue::TooShort.reason(), "passphrase_weak");
        assert_eq!(ServerUrlError::HttpsRequired.reason(), "https_required");
    }
}
