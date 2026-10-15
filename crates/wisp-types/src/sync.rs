//! 多设备同步的状态与 IPC 载荷（设置项 `Settings.sync` 见 [`crate::settings::SyncSettings`]，`sync_connect` 的
//! 同步内容参数是 [`crate::settings::SyncContent`]）。
//!
//! **隐私**：服务器地址、账号、设备名都不能进日志与错误信息。含这些字段的类型手写 `Debug`，把它们换成 `***`，
//! 这样即使有人用 `{:?}` 打印整个结构体也不会泄漏。`SyncStatus.error_detail` 只由固定词汇拼成（HTTP 状态码、错误种类、
//! `io::ErrorKind`、tls / dns / timeout / redirect），不含地址、路径或服务器返回的正文。

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::app::OsKind;

/// 被遮住的值（`Debug` 输出用）
const MASK: &str = "***";

/// 同步的总体阶段
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncPhase {
    /// 未开启同步（或已断开）
    #[default]
    Off,
    /// 已开启，当前没有在同步
    Idle,
    /// 正在同步（连接后的首轮上传 / 下载也在这里，带进度）
    Syncing,
    /// 最近一次同步失败（种类见 `SyncStatus.error`）
    Error,
}

/// 同步失败的种类。界面按种类给出「原因 + 建议」；日志只记种类本身
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncErrorKind {
    /// 网络不通、超时、DNS / TLS 失败（暂时性）
    Network,
    /// 401：账号或应用密码不对
    Auth,
    /// 403：没有权限访问该目录
    Forbidden,
    /// 404 / 409：地址指向的目录不存在
    NotFound,
    /// 507：服务器空间不足
    Quota,
    /// 5xx、429、意外的重定向等服务器侧问题（暂时性）
    Server,
    /// 地址是 http 而服务器不在本机或局域网，或服务器把 http 跳转到 https
    HttpsRequired,
    /// 同步密码不对
    Passphrase,
    /// 服务器上的同步数据已被清除或换成了新的一份，需要重新连接
    VaultReset,
    /// 服务器上的数据由更新版本的 Wisp 写出
    FormatTooNew,
    /// 凭据存储里没有与当前地址、账号匹配的凭据
    CredentialsMissing,
    /// 读写系统凭据存储被拒绝
    Keychain,
    /// 某台设备的数据无法解密或解析（只跳过那台设备）
    Corrupt,
    /// 其他内部错误
    Internal,
}

impl SyncErrorKind {
    /// 序列化后的字符串（与 serde 一致），日志里记录错误种类用
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

    /// 持续性失败：重试也不会自己好，自动同步暂停到用户处理为止（连接 / 恢复、立即同步、改同步设置）。
    /// 网络与服务器错误是暂时性的，只退避、不暂停；`corrupt` 只影响出问题的那台设备，`internal` 按暂时性处理，
    /// 免得一次偶发的内部错误让自动同步一直停着
    pub const fn is_persistent(self) -> bool {
        matches!(
            self,
            Self::Auth
                | Self::Forbidden
                | Self::NotFound
                | Self::Quota
                | Self::HttpsRequired
                | Self::Passphrase
                | Self::VaultReset
                | Self::FormatTooNew
                | Self::CredentialsMissing
                | Self::Keychain
        )
    }
}

/// 后台上传 / 下载的进度（条目数）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct SyncProgress {
    pub done: u32,
    pub total: u32,
}

/// 同步状态（`sync_status`、`sync:status` 事件、`RuntimeInfo.sync`）。不含地址、账号与设备名
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct SyncStatus {
    pub phase: SyncPhase,
    /// 最近一次成功完成同步的时间（Unix 毫秒）
    pub last_ok_ms: Option<i64>,
    /// 本机有改动等待上传
    pub pending: bool,
    /// 最近一次失败的种类；成功后清除
    pub error: Option<SyncErrorKind>,
    /// 失败是暂时性的（稍后自动重试）；为 false 时需要用户处理
    pub transient: bool,
    /// 失败的技术细节（界面里可折叠的「详情」）。只由固定词汇拼成：HTTP 状态码、错误种类、`io::ErrorKind`、
    /// tls / dns / timeout / redirect，不含地址、路径或服务器返回的正文
    pub error_detail: Option<String>,
    /// 后台上传 / 下载进行中时的进度
    pub progress: Option<SyncProgress>,
    /// 服务器上参与同步的设备数（含本机；未连接时为 0）
    pub device_count: u32,
}

impl SyncStatus {
    /// 未开启同步时的状态（也是 `Default`）
    pub fn off() -> Self {
        Self::default()
    }
}

/// 参与同步的一台设备（`sync_devices`）
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SyncDevice {
    /// 设备 id（16 位小写十六进制随机数，不含任何可识别信息）
    pub id: String,
    /// 设备名（用户可见；不进日志）
    pub name: String,
    pub os: OsKind,
    pub app_version: String,
    /// 这台设备最近一次上传的时间（Unix 毫秒）
    pub last_seen_ms: i64,
    /// 是否为本机
    pub this_device: bool,
}

impl fmt::Debug for SyncDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncDevice")
            .field("id", &self.id)
            .field("name", &MASK)
            .field("os", &self.os)
            .field("app_version", &self.app_version)
            .field("last_seen_ms", &self.last_seen_ms)
            .field("this_device", &self.this_device)
            .finish()
    }
}

/// WebDAV 账号（`sync_probe` / `sync_connect` / `sync_reset_remote` 的参数）。密码单独传，不在这里
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SyncAccount {
    /// 用户填写的服务器地址（http / https），同步数据放在它下面的 `wisp/` 子目录
    pub server_url: String,
    pub username: String,
}

impl fmt::Debug for SyncAccount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncAccount")
            .field("server_url", &MASK)
            .field("username", &MASK)
            .finish()
    }
}

/// 「测试连接」的报告（`sync_probe`）。传输层面的失败放在报告里，不作为命令错误抛出
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SyncProbe {
    /// 认证、建目录、写入并删除探测文件都成功
    pub ok: bool,
    pub error: Option<SyncErrorKind>,
    /// 同 `SyncStatus.error_detail`，只由固定词汇拼成
    pub error_detail: Option<String>,
    /// 服务器上已有的同步数据；没有（或 `ok = false`）时为 null
    pub vault: Option<SyncVaultInfo>,
}

/// 服务器上已有的同步数据概况
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SyncVaultInfo {
    /// 本版本能读取这份数据（为 false 时由更新版本的 Wisp 写出）
    pub supported: bool,
    /// 已有数据的设备数
    pub device_count: u32,
    /// 服务器上的数据最近一次更新的时间（Unix 毫秒；服务器没有报告时为 null）
    pub last_modified_ms: Option<i64>,
    /// 本机保存的同步状态正属于这份数据：重新连接即可继续（本机尚未上传的改动会接着同步）
    pub restorable: bool,
}

/// 连接的方式（由服务器与本机的状态决定，不由调用方选择）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncConnectOutcome {
    /// 服务器上还没有同步数据：用本机的数据新建
    Created,
    /// 服务器上已有数据：与本机数据合并
    Joined,
    /// 本机保存的同步状态属于服务器上的这份数据：只更新凭据并继续
    Restored,
}

/// `sync_connect` 的返回值
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SyncConnectResult {
    pub outcome: SyncConnectOutcome,
    pub status: SyncStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_account_and_device_name() {
        let account = SyncAccount {
            server_url: "https://dav.example.com/remote.php/dav/files/someone".to_owned(),
            username: "someone@example.com".to_owned(),
        };
        let text = format!("{account:?}");
        assert!(!text.contains("example") && !text.contains("someone"), "{text}");
        let device = SyncDevice {
            id: "0123456789abcdef".to_owned(),
            name: "Someone’s MacBook".to_owned(),
            os: OsKind::Macos,
            app_version: "0.2.0".to_owned(),
            last_seen_ms: 1,
            this_device: true,
        };
        let text = format!("{device:?}");
        assert!(!text.contains("MacBook") && text.contains("0123456789abcdef"), "{text}");
    }

    #[test]
    fn error_kind_strings_match_serde() {
        for kind in [
            SyncErrorKind::Network,
            SyncErrorKind::Auth,
            SyncErrorKind::Forbidden,
            SyncErrorKind::NotFound,
            SyncErrorKind::Quota,
            SyncErrorKind::Server,
            SyncErrorKind::HttpsRequired,
            SyncErrorKind::Passphrase,
            SyncErrorKind::VaultReset,
            SyncErrorKind::FormatTooNew,
            SyncErrorKind::CredentialsMissing,
            SyncErrorKind::Keychain,
            SyncErrorKind::Corrupt,
            SyncErrorKind::Internal,
        ] {
            assert_eq!(
                serde_json::to_value(kind).expect("serialize"),
                serde_json::Value::String(kind.as_str().to_owned())
            );
        }
        assert!(SyncErrorKind::Auth.is_persistent());
        assert!(!SyncErrorKind::Network.is_persistent());
        assert!(!SyncErrorKind::Server.is_persistent());
    }

    #[test]
    fn off_is_default() {
        let s = SyncStatus::off();
        assert_eq!(s.phase, SyncPhase::Off);
        assert_eq!(s, SyncStatus::default());
        assert!(s.error.is_none() && s.progress.is_none() && !s.pending);
    }
}
