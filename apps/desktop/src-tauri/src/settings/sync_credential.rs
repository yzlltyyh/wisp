//! 同步凭据：WebDAV 地址、账号、应用密码，以及同步数据的 vault id 与密钥，作为**一项**保存在系统凭据存储里
//! （service `wisp-sync`、账户 `account`，值是 JSON）。
//!
//! - 只有一项：连接时授权一次、断开时一次删除；macOS 上 ad-hoc 签名的更新之后每读一项就弹一次授权框，合成一项能少弹。
//! - 与模型服务的 Key 分开放在另一个 service 名下：provider_id 是用户可以随意起的，同一个 service 下可能撞名；
//!   也不写 state.json 的 Key 索引（`has_key` 只管模型服务）。
//! - Linux 没有 Secret Service 时与模型服务的 Key 一样只放在本次运行的内存里（重启后要重新输入密码），绝不明文落盘。
//! - 地址与账号一起存进去：设置里的地址或账号改了之后，旧密码不会被拿去访问另一个服务器（[`SyncCredential::matches`]）。
//! - 全部方法都是**阻塞**调用（可能弹出系统授权框），异步上下文里放进 `spawn_blocking`。错误信息不含任何凭据内容。

use serde::{Deserialize, Serialize};
use wisp_llm::Secret;
use wisp_types::KeychainState;

use crate::error::IpcError;
use crate::settings::keychain::{BackendFactory, Keychain};

/// 同步凭据所在的 service 名
pub const SYNC_SERVICE: &str = "wisp-sync";
/// 同步凭据的账户名（固定，只有这一项）
pub const SYNC_ACCOUNT: &str = "account";

/// 一份同步凭据。`Debug` 不输出任何字段；drop 时清零内容
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncCredential {
    /// WebDAV 地址（与 `settings.sync.server_url` 一致时才使用）
    pub server_url: String,
    /// WebDAV 账号（与 `settings.sync.username` 一致时才使用）
    pub username: String,
    /// WebDAV 应用密码
    pub password: String,
    /// 服务器上同步数据的 id
    pub vault_id: String,
    /// 由同步密码派生的密钥（编码由同步模块决定）
    pub key: String,
}

impl SyncCredential {
    /// 是否属于设置里当前的这个账号：地址或账号改过就不能沿用（同步按「缺少凭据」处理，请用户重新输入密码）
    pub fn matches(&self, server_url: &str, username: &str) -> bool {
        self.server_url == server_url && self.username == username
    }
}

impl std::fmt::Debug for SyncCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SyncCredential { .. }")
    }
}

impl Drop for SyncCredential {
    fn drop(&mut self) {
        for field in [
            &mut self.server_url,
            &mut self.username,
            &mut self.password,
            &mut self.vault_id,
            &mut self.key,
        ] {
            let mut bytes = std::mem::take(field).into_bytes();
            bytes.fill(0);
            // 防止编译器把清零当作无用写入优化掉
            std::hint::black_box(&bytes);
        }
    }
}

/// 同步凭据的读写
#[derive(Debug)]
pub struct SyncCredentialStore {
    keychain: Keychain,
}

impl Default for SyncCredentialStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncCredentialStore {
    /// 系统凭据存储（首次使用时才初始化，构造时不碰钥匙串）
    pub fn new() -> Self {
        Self {
            keychain: Keychain::for_service(SYNC_SERVICE),
        }
    }

    /// 使用自定义后端（测试、冒烟模式的进程内存后端）
    pub fn with_backend(factory: BackendFactory) -> Self {
        Self {
            keychain: Keychain::with_backend(factory).named(SYNC_SERVICE),
        }
    }

    /// 读取（**阻塞**，可能弹授权框）。没有保存过为 `Ok(None)`；内容无法解析（被别的程序改坏）同样当作没有，
    /// 让用户重新输入，而不是一直报错；拒绝授权等失败为 `keychain` 错误
    pub fn get(&self) -> Result<Option<SyncCredential>, IpcError> {
        let Some(raw) = self.keychain.get(SYNC_ACCOUNT)? else {
            return Ok(None);
        };
        if let Ok(credential) = serde_json::from_str::<SyncCredential>(raw.expose()) {
            Ok(Some(credential))
        } else {
            log::warn!("keychain: the stored sync credentials are malformed; treating them as missing");
            Ok(None)
        }
    }

    /// 写入（覆盖；**阻塞**）
    pub fn set(&self, credential: &SyncCredential) -> Result<(), IpcError> {
        // 序列化出的 JSON 含密码：包进 Secret，用完清零
        let json = serde_json::to_string(credential)
            .map(Secret::new)
            .map_err(|_| IpcError::internal("cannot encode the sync credentials"))?;
        self.keychain.set(SYNC_ACCOUNT, json.expose())
    }

    /// 删除（不存在视为成功；**阻塞**）
    pub fn delete(&self) -> Result<(), IpcError> {
        self.keychain.delete(SYNC_ACCOUNT)
    }

    /// 凭据存储的状态（不触发初始化）：`SessionOnly` 表示只在本次运行的内存里，重启后需要重新输入密码
    pub fn state(&self) -> KeychainState {
        self.keychain.state()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::error::IpcErrorCode;
    use crate::settings::keychain::tests::MemoryBackend;
    use crate::settings::keychain::{InitError, SecretBackend};

    fn credential() -> SyncCredential {
        SyncCredential {
            server_url: "https://dav.example.com/dav".to_owned(),
            username: "someone@example.com".to_owned(),
            password: "app-password-1".to_owned(),
            vault_id: "0123456789abcdef".to_owned(),
            key: "a2V5LWJ5dGVz".to_owned(),
        }
    }

    fn store(backend: &Arc<MemoryBackend>) -> SyncCredentialStore {
        let b = Arc::clone(backend);
        SyncCredentialStore::with_backend(Box::new(move || Ok(Box::new(Arc::clone(&b)) as Box<dyn SecretBackend>)))
    }

    #[test]
    fn round_trip_as_a_single_json_item() {
        let backend = Arc::new(MemoryBackend::default());
        let s = store(&backend);
        assert!(s.get().expect("get").is_none());
        s.set(&credential()).expect("set");
        {
            let items = backend.items.lock();
            assert_eq!(items.len(), 1, "只有一项");
            let raw: serde_json::Value = serde_json::from_str(&items[SYNC_ACCOUNT]).expect("json");
            let keys: Vec<&str> = raw.as_object().expect("object").keys().map(String::as_str).collect();
            assert_eq!(keys.len(), 5);
            for k in ["server_url", "username", "password", "vault_id", "key"] {
                assert!(keys.contains(&k), "{k}");
            }
        }
        let got = s.get().expect("get").expect("some");
        assert_eq!(got, credential());
        assert!(got.matches("https://dav.example.com/dav", "someone@example.com"));
        assert!(!got.matches("https://other.example.com/dav", "someone@example.com"));
        assert!(!got.matches("https://dav.example.com/dav", "else"));
        s.delete().expect("delete");
        assert!(s.get().expect("get").is_none());
        s.delete().expect("deleting again is fine");
    }

    #[test]
    fn malformed_items_read_as_missing() {
        let backend = Arc::new(MemoryBackend::default());
        backend
            .items
            .lock()
            .insert(SYNC_ACCOUNT.to_owned(), "{not json".to_owned());
        assert!(store(&backend).get().expect("get").is_none());
    }

    #[test]
    fn denied_access_is_a_keychain_error_without_content() {
        let backend = Arc::new(MemoryBackend::default());
        backend.deny.store(true, Ordering::SeqCst);
        let s = store(&backend);
        let err = s.set(&credential()).expect_err("denied");
        assert_eq!(err.code, IpcErrorCode::Keychain);
        for secret in [
            "app-password-1",
            "a2V5LWJ5dGVz",
            "someone@example.com",
            "dav.example.com",
        ] {
            assert!(!err.message.contains(secret));
        }
        assert_eq!(s.get().expect_err("denied").code, IpcErrorCode::Keychain);
    }

    #[test]
    fn debug_output_never_contains_the_content() {
        let text = format!("{:?}", credential());
        for secret in [
            "app-password-1",
            "a2V5LWJ5dGVz",
            "someone@example.com",
            "dav.example.com",
        ] {
            assert!(!text.contains(secret), "{text}");
        }
    }

    /// 没有 Secret Service 的 Linux：凭据只在本次运行的内存里，照样能读回；其他平台报 `keychain` 错误
    #[test]
    fn without_a_credential_store_it_follows_the_keychain_rules() {
        let s = SyncCredentialStore::with_backend(Box::new(|| Err(InitError::permanent("no secret service"))));
        let result = s.set(&credential());
        if wisp_types::OsKind::current() == wisp_types::OsKind::Linux {
            result.expect("session only");
            assert_eq!(s.state(), KeychainState::SessionOnly);
            assert_eq!(s.get().expect("get"), Some(credential()));
        } else {
            assert_eq!(result.expect_err("unavailable").code, IpcErrorCode::Keychain);
            assert_eq!(s.state(), KeychainState::Unavailable);
        }
    }
}
