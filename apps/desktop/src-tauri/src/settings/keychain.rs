//! 钥匙串：keyring 4，`Entry::new("wisp", provider_id)`，访问串行化。Linux 没有 Secret Service 时进入
//! 「仅本次会话」内存模式并显示警告，**绝不明文落盘**；用户拒绝授权导致读取失败时报 `keychain` 错误，不进入仅本次会话模式。
//!
//! 设计要点：
//! - **首次使用时才初始化**：构造时不碰系统钥匙串；第一次 `set` / `get` / `delete` 时才创建凭据存储
//!   （Linux 上会连接 Secret Service）。
//! - **初始化失败与重试——按真实后端如实说明**：keyring 4 的 v1 接口把凭据存储的初始化结果放在进程级的 `LazyLock` 里，
//!   **整个进程只算一次**，失败后每次都返回同一个缓存的错误，在 v1 之上重试没有意义。各平台的实际情况：
//!   - macOS / Windows：`Store::new()` 只构造句柄、不会失败；失败（拒绝授权、钥匙串锁定）都发生在单次操作上，按单次错误
//!     返回，下一次操作自然会再试，不需要「重新初始化」；
//!   - Linux：初始化要连接 Secret Service。为了不在登录自启、Secret Service 还没注册到会话总线时把失败永久缓存下来，
//!     **第一次初始化之前先经会话总线确认 `org.freedesktop.secrets` 已在总线上或可按需激活**（`app::session_bus`）；确认
//!     不在时**不碰 keyring**，以暂时性失败进入仅本次会话模式，之后的显式 `set` / `get` / `delete` 按退避间隔
//!     （[`RETRY_INIT_AFTER`] 起翻倍，最长 [`RETRY_INIT_MAX`]）重新探测；探测到之后才真正初始化，成功后把内存里的 Key
//!     **迁入系统钥匙串**（全部迁移成功才切换，否则保持内存模式、稍后再试）。探测说「在」而 keyring 初始化仍失败、或查询
//!     工具都不可用时，结果被 keyring 永久缓存，本次进程保持仅本次会话模式——要彻底支持重试需绕开 v1 的 `LazyLock`、直接
//!     依赖 keyring-core 与各平台 store；
//!   - 可替换后端（[`Keychain::with_backend`]，测试、冒烟）可以报告暂时性失败（[`InitError::transient`]），同样按退避重试。
//! - **`has` 不读取明文**：钥匙串里没有「只查是否存在」的接口（macOS 的查询会连同数据一起读出，ad-hoc 签名更新后每读
//!   一次就弹一次授权框），因此用一份**只含 provider_id 的索引**回答 `has`。索引由 [`Keychain::with_index`] 注入并经
//!   回调持久化到 state.json（`stored_keys`）；每次 `get` 的结果都会自愈索引（读到了就补上，读不到就删掉）。state.json
//!   被重置时由 [`Keychain::rebuild_index`] 按需重建（`SettingsService::verify_key_index`）。
//! - **全部方法都是阻塞调用**（macOS 可能弹出钥匙串授权框，直到用户响应才返回）。在异步上下文里请用
//!   [`super::service::key_get_blocking`] / [`super::service::run_blocking`] 或 `spawn_blocking`，不要占住 tokio worker。
//! - 错误信息只含固定的英文简述与平台错误码，**绝不包含 Key**（keyring 的 `BadEncoding` 携带原始字节，不格式化它）。

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use wisp_llm::Secret;
use wisp_types::{KeychainState, OsKind};

use crate::error::IpcError;

/// 钥匙串条目的 service 名
pub const SERVICE: &str = "wisp";
/// Secret Service 在会话总线上的名字
pub const SECRET_SERVICE_NAME: &str = "org.freedesktop.secrets";
/// 暂时性的初始化失败之后，至少隔多久才在下一次显式访问时重试（之后每次翻倍）
pub const RETRY_INIT_AFTER: Duration = Duration::from_secs(5);
/// 重试间隔的上限
pub const RETRY_INIT_MAX: Duration = Duration::from_secs(5 * 60);

/// 系统凭据存储的后端（可替换，测试用内存实现）
pub trait SecretBackend: Send + Sync {
    /// 写入（覆盖）
    fn set(&self, id: &str, key: &str) -> Result<(), BackendError>;
    /// 读取；不存在时为 `Ok(None)`
    fn get(&self, id: &str) -> Result<Option<String>, BackendError>;
    /// 删除；不存在时视为成功
    fn delete(&self, id: &str) -> Result<(), BackendError>;
}

/// 后端错误（只携带安全的英文简述，不含 Key）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendError(pub String);

/// 凭据存储初始化失败
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitError {
    /// 英文简述
    pub reason: String,
    /// 暂时性的（之后的显式访问按退避重试）；否则本次进程不再重试
    pub transient: bool,
}

impl InitError {
    /// 本次进程内不会好转的失败（例如 keyring 已缓存的初始化错误）
    pub fn permanent(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            transient: false,
        }
    }

    /// 暂时性的失败（例如 Secret Service 还没注册到会话总线）
    pub fn transient(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            transient: true,
        }
    }
}

/// 创建后端
pub type BackendFactory = Box<dyn Fn() -> Result<Box<dyn SecretBackend>, InitError> + Send + Sync>;

/// 索引变化时的持久化回调
type PersistIndex = Box<dyn Fn(&BTreeSet<String>) + Send + Sync>;

/// 下一次重试初始化的安排
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Retry {
    /// 不早于这个时刻
    at: Instant,
    /// 已经失败的次数（决定下一次的间隔）
    failures: u32,
}

impl Retry {
    fn due(retry: Option<Self>) -> bool {
        retry.is_some_and(|r| Instant::now() >= r.at)
    }
}

/// 第 `failures` 次暂时性失败之后的重试间隔：`base` 起每次翻倍，不超过 [`RETRY_INIT_MAX`]（纯函数）
pub fn retry_delay(base: Duration, failures: u32) -> Duration {
    let factor = 1u32 << failures.saturating_sub(1).min(16);
    base.saturating_mul(factor).min(RETRY_INIT_MAX.max(base))
}

/// 后端初始化后的模式
enum Mode {
    /// 系统钥匙串
    Os(Arc<dyn SecretBackend>),
    /// 仅本次会话（Linux 无 Secret Service）：Key 只在内存里，退出即失。`retry` 为 Some 时失败是暂时性的，到点后的下一次
    /// 显式访问重新初始化，成功后把内存里的 Key 迁入系统钥匙串
    Session { retry: Option<Retry> },
    /// 不可用（非 Linux 平台上凭据存储初始化失败）；`retry` 同上
    Unavailable { reason: String, retry: Option<Retry> },
}

impl Mode {
    const fn state(&self) -> KeychainState {
        match self {
            Self::Os(_) => KeychainState::Available,
            Self::Session { .. } => KeychainState::SessionOnly,
            Self::Unavailable { .. } => KeychainState::Unavailable,
        }
    }
}

/// 一次访问所用的后端（从 [`Mode`] 取出，不持有 `mode` 的锁）
enum Active {
    Os(Arc<dyn SecretBackend>),
    Session,
    Unavailable(String),
}

/// 钥匙串访问。
///
/// `set` / `get` / `delete` 都返回 `Result<_, IpcError>`，
/// `get` 为 `Result<Option<Secret>, IpcError>`——不存在是 `Ok(None)`，用户拒绝授权 / 钥匙串锁定等失败是
/// `Err(IpcError { code: keychain })`（不得把失败当成「没有 Key」静默吞掉）
pub struct Keychain {
    factory: BackendFactory,
    /// None = 还没初始化（只在持有 `io` 时写入）
    mode: RwLock<Option<Mode>>,
    /// 暂时性失败之后的首次重试间隔（测试可以缩短）
    retry_after: Duration,
    /// 串行化全部钥匙串访问（同时也保护索引的读改写与初始化）
    io: Mutex<()>,
    /// 仅本次会话模式下的 Key
    session: Mutex<HashMap<String, Secret>>,
    /// 已保存 Key 的 provider_id（系统钥匙串模式）
    index: RwLock<BTreeSet<String>>,
    /// 索引变化时的持久化回调
    persist: Option<PersistIndex>,
    /// has / state 可能变化时的通知（触发 `settings:changed`）
    on_change: Option<Box<dyn Fn() + Send + Sync>>,
}

impl std::fmt::Debug for Keychain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keychain")
            .field("state", &self.state())
            .field("indexed", &self.index.read().len())
            .finish_non_exhaustive()
    }
}

impl Default for Keychain {
    fn default() -> Self {
        Self::new()
    }
}

impl Keychain {
    /// 新建（不访问钥匙串，首次使用时才创建句柄）；索引只在内存里
    pub fn new() -> Self {
        Self::with_backend(Box::new(os_backend))
    }

    /// 使用自定义后端工厂（测试用）
    pub fn with_backend(factory: BackendFactory) -> Self {
        Self {
            factory,
            mode: RwLock::new(None),
            retry_after: RETRY_INIT_AFTER,
            io: Mutex::new(()),
            session: Mutex::new(HashMap::new()),
            index: RwLock::new(BTreeSet::new()),
            persist: None,
            on_change: None,
        }
    }

    /// 注入持久化的索引：`initial` 为上次保存的 provider_id 集合，`persist` 在索引变化时被调用（写 state.json），
    /// `on_change` 在 `has` / 状态可能变化时被调用（广播 `settings:changed`）
    pub fn with_index(
        mut self,
        initial: BTreeSet<String>,
        persist: impl Fn(&BTreeSet<String>) + Send + Sync + 'static,
        on_change: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        *self.index.get_mut() = initial;
        self.persist = Some(Box::new(persist));
        self.on_change = Some(Box::new(on_change));
        self
    }

    /// 本次访问用的后端（**调用方持有 `io`**）：还没初始化、或上次是暂时性失败且已到重试时刻时（重新）初始化；
    /// 状态变化时通知
    fn active(&self) -> Active {
        let retry = {
            let mode = self.mode.read();
            match &*mode {
                Some(Mode::Os(backend)) => return Active::Os(Arc::clone(backend)),
                Some(Mode::Session { retry }) if !Retry::due(*retry) => return Active::Session,
                Some(Mode::Unavailable { reason, retry }) if !Retry::due(*retry) => {
                    return Active::Unavailable(reason.clone());
                }
                Some(Mode::Session { retry } | Mode::Unavailable { retry, .. }) => *retry,
                None => None,
            }
        };
        let failures = retry.map_or(0, |r| r.failures);
        let schedule = |failures: u32| Retry {
            at: Instant::now() + retry_delay(self.retry_after, failures),
            failures,
        };
        let mode = match (self.factory)() {
            Ok(backend) => {
                let backend: Arc<dyn SecretBackend> = Arc::from(backend);
                match self.migrate_session_keys(&backend) {
                    Ok(0) if retry.is_some() => {
                        log::info!("keychain: credential store is available now");
                        Mode::Os(backend)
                    }
                    Ok(0) => Mode::Os(backend),
                    Ok(moved) => {
                        log::info!("keychain: credential store is available now; moved {moved} session key(s) into it");
                        Mode::Os(backend)
                    }
                    Err(reason) => {
                        log::warn!("keychain: cannot move the session keys into the credential store yet ({reason})");
                        Mode::Session {
                            retry: Some(schedule(failures + 1)),
                        }
                    }
                }
            }
            Err(InitError { reason, transient }) => {
                let retry = transient.then(|| schedule(failures + 1));
                if OsKind::current() == OsKind::Linux {
                    match retry {
                        Some(r) => log::warn!(
                            "keychain: secret service unavailable ({reason}); keys are kept in memory for now, \
                             retrying on the next use after {} s",
                            retry_delay(self.retry_after, r.failures).as_secs()
                        ),
                        None => log::warn!(
                            "keychain: secret service unavailable ({reason}); keys are kept in memory for this session only"
                        ),
                    }
                    Mode::Session { retry }
                } else {
                    log::error!("keychain: credential store unavailable ({reason})");
                    Mode::Unavailable { reason, retry }
                }
            }
        };
        let active = match &mode {
            Mode::Os(backend) => Active::Os(Arc::clone(backend)),
            Mode::Session { .. } => Active::Session,
            Mode::Unavailable { reason, .. } => Active::Unavailable(reason.clone()),
        };
        let before = self.state();
        let after = mode.state();
        *self.mode.write() = Some(mode);
        if before != after {
            self.notify();
        }
        active
    }

    /// 仅本次会话模式下保存在内存里的 Key 迁入刚连上的系统钥匙串（**调用方持有 `io`**）。全部写入成功才清空内存并返回
    /// 条数；任何一条失败都保留内存里的全部 Key（已写入的下次覆盖），返回失败原因
    fn migrate_session_keys(&self, backend: &Arc<dyn SecretBackend>) -> Result<usize, String> {
        // 复制成 Secret（drop 时清零），不在持有 `session` 锁时调用后端（可能等待解锁对话框，而 `has` 要读这把锁）
        let pending: Vec<(String, Secret)> = self
            .session
            .lock()
            .iter()
            .map(|(id, key)| (id.clone(), Secret::new(key.expose().to_owned())))
            .collect();
        for (id, key) in &pending {
            backend.set(id, key.expose()).map_err(|e| e.0)?;
        }
        self.session.lock().clear();
        for (id, _) in &pending {
            self.touch_index(id, true);
        }
        Ok(pending.len())
    }

    fn notify(&self) {
        if let Some(f) = &self.on_change {
            f();
        }
    }

    /// 修改索引；有变化时持久化并通知
    fn touch_index(&self, id: &str, present: bool) {
        let changed = {
            let mut index = self.index.write();
            if present {
                index.insert(id.to_owned())
            } else {
                index.remove(id)
            }
        };
        if changed {
            if let Some(persist) = &self.persist {
                persist(&self.index.read());
            }
            self.notify();
        }
    }

    /// 写入 Key（首尾空白会被去掉；空 Key 返回 `invalid_argument`）
    pub fn set(&self, provider_id: &str, key: &str) -> Result<(), IpcError> {
        check_provider_id(provider_id)?;
        let key = key.trim();
        if key.is_empty() {
            return Err(IpcError::invalid_argument("key must not be empty"));
        }
        let _io = self.io.lock();
        match self.active() {
            Active::Os(backend) => {
                backend.set(provider_id, key).map_err(to_ipc)?;
                drop(_io);
                self.touch_index(provider_id, true);
                log::info!("keychain: key saved provider={provider_id}");
                Ok(())
            }
            Active::Session => {
                let inserted = self
                    .session
                    .lock()
                    .insert(provider_id.to_owned(), Secret::new(key.to_owned()))
                    .is_none();
                drop(_io);
                if inserted {
                    self.notify();
                }
                log::info!("keychain: key kept in memory (session only) provider={provider_id}");
                Ok(())
            }
            Active::Unavailable(reason) => Err(unavailable(&reason)),
        }
    }

    /// 读取 Key；不存在时为 Ok(None)，授权被拒等失败时为 `keychain` 错误
    pub fn get(&self, provider_id: &str) -> Result<Option<Secret>, IpcError> {
        check_provider_id(provider_id)?;
        let _io = self.io.lock();
        match self.active() {
            Active::Os(backend) => {
                let found = backend.get(provider_id).map_err(to_ipc)?;
                drop(_io);
                self.touch_index(provider_id, found.is_some());
                Ok(found.map(Secret::new))
            }
            Active::Session => Ok(self
                .session
                .lock()
                .get(provider_id)
                .map(|s| Secret::new(s.expose().to_owned()))),
            Active::Unavailable(reason) => Err(unavailable(&reason)),
        }
    }

    /// 删除 Key（不存在时视为成功）
    pub fn delete(&self, provider_id: &str) -> Result<(), IpcError> {
        check_provider_id(provider_id)?;
        let _io = self.io.lock();
        match self.active() {
            Active::Os(backend) => {
                backend.delete(provider_id).map_err(to_ipc)?;
                drop(_io);
                self.touch_index(provider_id, false);
                log::info!("keychain: key deleted provider={provider_id}");
                Ok(())
            }
            Active::Session => {
                let removed = self.session.lock().remove(provider_id).is_some();
                drop(_io);
                if removed {
                    self.notify();
                }
                Ok(())
            }
            // 不可用时没有任何 Key 可删
            Active::Unavailable(_) => Ok(()),
        }
    }

    /// 是否存在 Key（不读取明文，不访问系统钥匙串：按索引回答）
    pub fn has(&self, provider_id: &str) -> bool {
        match self.state() {
            KeychainState::SessionOnly => self.session.lock().contains_key(provider_id),
            KeychainState::Unavailable => false,
            KeychainState::Available => self.index.read().contains(provider_id),
        }
    }

    /// 重建索引（state.json 被重置后，`has` 会把已保存的 Key 报成「没有」）：对 `ids` 里索引中还没有的逐个读取一次
    /// （读到即经自愈补进索引）。**阻塞**，macOS 上可能弹授权框；某一次读取失败（例如用户拒绝授权）就停止，不再连续弹框。
    /// 返回补上的条数；仅本次会话 / 不可用时什么也不做
    pub fn rebuild_index(&self, ids: &[String]) -> usize {
        let mut found = 0;
        for id in ids {
            if self.state() != KeychainState::Available {
                break;
            }
            if self.index.read().contains(id) {
                continue;
            }
            match self.get(id) {
                Ok(Some(_)) => found += 1,
                Ok(None) => {}
                Err(e) => {
                    log::warn!("keychain: index rebuild stopped: {}", e.message);
                    break;
                }
            }
        }
        found
    }

    /// 当前状态（不触发初始化：还没用过时按「可用」报告）
    pub fn state(&self) -> KeychainState {
        self.mode.read().as_ref().map_or(KeychainState::Available, Mode::state)
    }

    /// 钥匙串是否可用（含仅本次会话模式）
    pub fn available(&self) -> bool {
        self.state() != KeychainState::Unavailable
    }

    /// 是否处于「仅本次会话」内存模式
    pub fn session_only(&self) -> bool {
        self.state() == KeychainState::SessionOnly
    }
}

fn check_provider_id(id: &str) -> Result<(), IpcError> {
    // 空串在 Keychain Services 里是通配符，必须拒绝
    if id.trim().is_empty() {
        return Err(IpcError::invalid_argument("provider_id must not be empty"));
    }
    Ok(())
}

fn to_ipc(e: BackendError) -> IpcError {
    IpcError::keychain(e.0)
}

fn unavailable(reason: &str) -> IpcError {
    IpcError::keychain(format!("credential store unavailable: {reason}"))
}

/// keyring 的进程级初始化已经发生（结果被 `LazyLock` 缓存，之后不会改变）
static STORE_COMMITTED: AtomicBool = AtomicBool::new(false);

/// 系统钥匙串后端（keyring v1 模式：macOS Keychain / Windows Credential Manager / Linux Secret Service）。
/// Linux 上第一次初始化之前先确认 Secret Service 在会话总线上（或可按需激活），确认不在时返回暂时性失败、不碰 keyring
/// （见模块说明）。**阻塞**
fn os_backend() -> Result<Box<dyn SecretBackend>, InitError> {
    if OsKind::current() == OsKind::Linux
        && !STORE_COMMITTED.load(Ordering::Acquire)
        && crate::app::session_bus::name_reachable(SECRET_SERVICE_NAME) == Some(false)
    {
        return Err(InitError::transient("the Secret Service is not on the session bus yet"));
    }
    STORE_COMMITTED.store(true, Ordering::Release);
    match keyring::Entry::store_status() {
        Ok(()) => Ok(Box::new(OsBackend)),
        // keyring 已把这个结果缓存到进程结束，重试没有意义
        Err(e) => Err(InitError::permanent(describe(e))),
    }
}

struct OsBackend;

impl OsBackend {
    fn entry(id: &str) -> Result<keyring::Entry, BackendError> {
        keyring::Entry::new(SERVICE, id).map_err(|e| BackendError(describe(&e)))
    }
}

impl SecretBackend for OsBackend {
    fn set(&self, id: &str, key: &str) -> Result<(), BackendError> {
        Self::entry(id)?
            .set_password(key)
            .map_err(|e| BackendError(describe(&e)))
    }

    fn get(&self, id: &str) -> Result<Option<String>, BackendError> {
        match Self::entry(id)?.get_password() {
            Ok(key) => Ok(Some(key)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(BackendError(describe(&e))),
        }
    }

    fn delete(&self, id: &str) -> Result<(), BackendError> {
        match Self::entry(id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(BackendError(describe(&e))),
        }
    }
}

/// 进程内存后端：不碰系统钥匙串、不弹授权框，退出即失（冒烟模式用，见 `SettingsService::load_isolated`）
#[derive(Default)]
pub struct MemoryStore {
    items: Mutex<HashMap<String, String>>,
}

impl SecretBackend for MemoryStore {
    fn set(&self, id: &str, key: &str) -> Result<(), BackendError> {
        self.items.lock().insert(id.to_owned(), key.to_owned());
        Ok(())
    }

    fn get(&self, id: &str) -> Result<Option<String>, BackendError> {
        Ok(self.items.lock().get(id).cloned())
    }

    fn delete(&self, id: &str) -> Result<(), BackendError> {
        self.items.lock().remove(id);
        Ok(())
    }
}

/// 进程内存后端的工厂
pub fn memory_backend() -> BackendFactory {
    Box::new(|| Ok(Box::new(MemoryStore::default()) as Box<dyn SecretBackend>))
}

/// 把 keyring 错误转成安全的英文简述：**不使用 `Debug`**（`BadEncoding` / `BadDataFormat` 携带原始字节）
fn describe(e: &keyring::Error) -> String {
    match e {
        keyring::Error::NoStorageAccess(p) => {
            format!(
                "access to the keychain was denied or it is locked ({})",
                short(&p.to_string())
            )
        }
        keyring::Error::PlatformFailure(p) => format!("keychain platform failure ({})", short(&p.to_string())),
        keyring::Error::NoEntry => "no matching key".to_owned(),
        keyring::Error::BadEncoding(_) => "stored key is not valid UTF-8".to_owned(),
        keyring::Error::BadDataFormat(..) => "stored key is malformed".to_owned(),
        keyring::Error::BadStoreFormat(_) => "keychain store is malformed".to_owned(),
        keyring::Error::TooLong(name, limit) => format!("{name} exceeds the platform limit of {limit}"),
        keyring::Error::Invalid(attr, _) => format!("invalid keychain attribute {attr}"),
        keyring::Error::Ambiguous(items) => format!("{} keychain entries match", items.len()),
        keyring::Error::NoDefaultStore => "no credential store is available".to_owned(),
        keyring::Error::NotSupportedByStore(what) => format!("not supported by the credential store ({})", short(what)),
        _ => "keychain error".to_owned(),
    }
}

/// 平台错误描述截断到 160 字符（平台错误只是错误码与系统说明，不含 Key）
fn short(s: &str) -> String {
    s.chars().take(160).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::error::IpcErrorCode;

    /// 内存后端（测试用）：可以模拟授权被拒
    #[derive(Default)]
    pub struct MemoryBackend {
        pub items: Mutex<HashMap<String, String>>,
        pub deny: std::sync::atomic::AtomicBool,
        pub calls: AtomicUsize,
    }

    impl SecretBackend for Arc<MemoryBackend> {
        fn set(&self, id: &str, key: &str) -> Result<(), BackendError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.deny.load(Ordering::SeqCst) {
                return Err(BackendError(
                    "access to the keychain was denied or it is locked (-128)".to_owned(),
                ));
            }
            self.items.lock().insert(id.to_owned(), key.to_owned());
            Ok(())
        }
        fn get(&self, id: &str) -> Result<Option<String>, BackendError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.deny.load(Ordering::SeqCst) {
                return Err(BackendError(
                    "access to the keychain was denied or it is locked (-128)".to_owned(),
                ));
            }
            Ok(self.items.lock().get(id).cloned())
        }
        fn delete(&self, id: &str) -> Result<(), BackendError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.deny.load(Ordering::SeqCst) {
                return Err(BackendError(
                    "access to the keychain was denied or it is locked (-128)".to_owned(),
                ));
            }
            self.items.lock().remove(id);
            Ok(())
        }
    }

    /// 以共享的内存后端构造钥匙串
    pub fn memory_keychain(backend: &Arc<MemoryBackend>) -> Keychain {
        let b = Arc::clone(backend);
        Keychain::with_backend(Box::new(move || Ok(Box::new(Arc::clone(&b)) as Box<dyn SecretBackend>)))
    }

    #[test]
    fn construction_does_not_touch_the_store() {
        let created = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&created);
        let kc = Keychain::with_backend(Box::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Arc::new(MemoryBackend::default())) as Box<dyn SecretBackend>)
        }));
        assert!(!kc.has("p"));
        assert_eq!(kc.state(), KeychainState::Available);
        assert_eq!(created.load(Ordering::SeqCst), 0, "has / state 不得初始化钥匙串");
        kc.get("p").expect("get");
        kc.get("p").expect("get");
        assert_eq!(created.load(Ordering::SeqCst), 1, "只初始化一次");
    }

    #[test]
    fn set_get_delete_round_trip_and_index() {
        let backend = Arc::new(MemoryBackend::default());
        let persisted = Arc::new(Mutex::new(Vec::<BTreeSet<String>>::new()));
        let changes = Arc::new(AtomicUsize::new(0));
        let (p2, c2) = (Arc::clone(&persisted), Arc::clone(&changes));
        let kc = memory_keychain(&backend).with_index(
            BTreeSet::new(),
            move |ids| p2.lock().push(ids.clone()),
            move || {
                c2.fetch_add(1, Ordering::SeqCst);
            },
        );
        kc.set("openai-main", "  sk-test-123\n").expect("set");
        assert!(kc.has("openai-main"));
        assert_eq!(
            backend.items.lock().get("openai-main").map(String::as_str),
            Some("sk-test-123")
        );
        let got = kc.get("openai-main").expect("get").expect("some");
        assert_eq!(got.expose(), "sk-test-123");
        kc.delete("openai-main").expect("delete");
        assert!(!kc.has("openai-main"));
        assert!(kc.get("openai-main").expect("get").is_none());
        // 删除不存在的条目视为成功
        kc.delete("openai-main").expect("delete again");
        let persisted = persisted.lock();
        assert_eq!(persisted.len(), 2, "写入与删除各持久化一次");
        assert!(persisted[0].contains("openai-main"));
        assert!(persisted[1].is_empty());
        assert_eq!(changes.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn get_self_heals_the_index() {
        let backend = Arc::new(MemoryBackend::default());
        backend.items.lock().insert("legacy".to_owned(), "k".to_owned());
        let kc = memory_keychain(&backend).with_index(BTreeSet::from(["gone".to_owned()]), |_| {}, || {});
        assert!(kc.has("gone"));
        assert!(!kc.has("legacy"));
        assert!(kc.get("gone").expect("get").is_none());
        assert!(!kc.has("gone"), "钥匙串里已经没有的条目从索引中移除");
        assert!(kc.get("legacy").expect("get").is_some());
        assert!(kc.has("legacy"), "读到的条目补进索引");
    }

    #[test]
    fn denied_access_is_a_keychain_error_not_session_mode() {
        let backend = Arc::new(MemoryBackend::default());
        backend.deny.store(true, Ordering::SeqCst);
        let kc = memory_keychain(&backend);
        let err = kc.get("p").expect_err("denied");
        assert_eq!(err.code, IpcErrorCode::Keychain);
        assert_eq!(kc.state(), KeychainState::Available, "拒绝授权不进入仅本次会话模式");
        assert!(!kc.session_only());
        assert_eq!(kc.set("p", "k").expect_err("denied").code, IpcErrorCode::Keychain);
        assert_eq!(kc.delete("p").expect_err("denied").code, IpcErrorCode::Keychain);
    }

    #[test]
    fn store_init_failure_depends_on_platform() {
        let kc = Keychain::with_backend(Box::new(|| Err(InitError::permanent("no secret service"))));
        let result = kc.set("p", "sk-secret");
        if OsKind::current() == OsKind::Linux {
            result.expect("session mode");
            assert!(kc.session_only());
            assert!(kc.has("p"));
            assert_eq!(kc.get("p").expect("get").expect("some").expose(), "sk-secret");
        } else {
            let err = result.expect_err("unavailable");
            assert_eq!(err.code, IpcErrorCode::Keychain);
            assert_eq!(kc.state(), KeychainState::Unavailable);
            assert!(!kc.available());
            assert!(!kc.has("p"));
        }
    }

    /// keyring v1 把初始化结果缓存到进程结束：永久性失败不再重试（即使重试间隔为 0）
    #[test]
    fn permanent_init_failures_are_not_retried() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let a = Arc::clone(&attempts);
        let mut kc = Keychain::with_backend(Box::new(move || {
            a.fetch_add(1, Ordering::SeqCst);
            Err(InitError::permanent("cached keyring error"))
        }));
        kc.retry_after = Duration::ZERO;
        for _ in 0..3 {
            let _ = kc.get("p");
            let _ = kc.set("p", "k");
        }
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    /// 暂时性失败（例如登录自启时 Secret Service 还没注册到会话总线）：下一次显式访问重新初始化；Linux 上内存里的 Key
    /// 迁入系统钥匙串，退出也不会丢
    #[test]
    fn transient_init_failure_is_retried_and_session_keys_are_moved_into_the_store() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let backend = Arc::new(MemoryBackend::default());
        let persisted = Arc::new(Mutex::new(BTreeSet::new()));
        let (a, b, p) = (Arc::clone(&attempts), Arc::clone(&backend), Arc::clone(&persisted));
        let mut kc = Keychain::with_backend(Box::new(move || {
            if a.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(InitError::transient("secret service is not on the session bus yet"))
            } else {
                Ok(Box::new(Arc::clone(&b)) as Box<dyn SecretBackend>)
            }
        }))
        .with_index(BTreeSet::new(), move |ids| p.lock().clone_from(ids), || {});
        kc.retry_after = Duration::ZERO;
        let first = kc.set("p", "sk-1");
        if OsKind::current() == OsKind::Linux {
            first.expect("session mode for now");
            assert!(kc.session_only());
            assert!(kc.has("p"));
            assert!(backend.items.lock().is_empty(), "还没写进系统钥匙串");
            // 下一次显式访问：连上了，内存里的 Key 迁进系统钥匙串
            assert_eq!(kc.get("p").expect("get").expect("some").expose(), "sk-1");
            assert_eq!(kc.state(), KeychainState::Available);
            assert_eq!(backend.items.lock().get("p").map(String::as_str), Some("sk-1"));
            assert!(kc.has("p"), "迁移后由索引回答");
            assert!(persisted.lock().contains("p"), "索引已持久化");
        } else {
            assert_eq!(first.expect_err("unavailable").code, IpcErrorCode::Keychain);
            assert_eq!(kc.state(), KeychainState::Unavailable);
            kc.set("p", "sk-2").expect("retried");
            assert_eq!(kc.state(), KeychainState::Available);
            assert!(kc.has("p"));
        }
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        kc.get("p").expect("get");
        assert_eq!(attempts.load(Ordering::SeqCst), 2, "可用之后不再重复初始化");
    }

    #[test]
    fn session_keys_stay_in_memory_when_moving_them_fails() {
        if OsKind::current() != OsKind::Linux {
            return;
        }
        let attempts = Arc::new(AtomicUsize::new(0));
        let backend = Arc::new(MemoryBackend::default());
        let (a, b) = (Arc::clone(&attempts), Arc::clone(&backend));
        let mut kc = Keychain::with_backend(Box::new(move || {
            if a.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(InitError::transient("not yet"))
            } else {
                Ok(Box::new(Arc::clone(&b)) as Box<dyn SecretBackend>)
            }
        }));
        kc.retry_after = Duration::ZERO;
        kc.set("p", "sk-1").expect("session");
        // Secret Service 起来了但集合是锁着的：迁移失败，继续用内存里的 Key，稍后再试
        backend.deny.store(true, Ordering::SeqCst);
        assert_eq!(kc.get("p").expect("get").expect("some").expose(), "sk-1");
        assert!(kc.session_only());
        backend.deny.store(false, Ordering::SeqCst);
        assert_eq!(kc.get("p").expect("get").expect("some").expose(), "sk-1");
        assert_eq!(kc.state(), KeychainState::Available);
        assert_eq!(backend.items.lock().get("p").map(String::as_str), Some("sk-1"));
    }

    #[test]
    fn transient_failures_are_not_retried_too_often() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let a = Arc::clone(&attempts);
        let kc = Keychain::with_backend(Box::new(move || {
            a.fetch_add(1, Ordering::SeqCst);
            Err(InitError::transient("no credential store yet"))
        }));
        let _ = kc.get("p");
        let _ = kc.get("p");
        let _ = kc.get("p");
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "重试间隔内不重复初始化");
    }

    #[test]
    fn retry_delay_backs_off_exponentially_up_to_a_cap() {
        let base = RETRY_INIT_AFTER;
        assert_eq!(retry_delay(base, 1), base);
        assert_eq!(retry_delay(base, 2), base * 2);
        assert_eq!(retry_delay(base, 3), base * 4);
        assert_eq!(retry_delay(base, 30), RETRY_INIT_MAX);
        assert_eq!(retry_delay(Duration::ZERO, 5), Duration::ZERO);
    }

    #[test]
    fn index_is_rebuilt_from_the_store() {
        let backend = Arc::new(MemoryBackend::default());
        backend.items.lock().insert("a".to_owned(), "k".to_owned());
        backend.items.lock().insert("c".to_owned(), "k".to_owned());
        // state.json 被重置：索引是空的
        let kc = memory_keychain(&backend).with_index(BTreeSet::new(), |_| {}, || {});
        assert!(!kc.has("a"));
        let ids = ["a", "b", "c"].map(str::to_owned);
        assert_eq!(kc.rebuild_index(&ids), 2);
        assert!(kc.has("a") && !kc.has("b") && kc.has("c"));
        // 已在索引里的不再读取
        let calls = backend.calls.load(Ordering::SeqCst);
        assert_eq!(kc.rebuild_index(&ids[..1]), 0);
        assert_eq!(backend.calls.load(Ordering::SeqCst), calls);
        // 读取被拒绝：停止，不连续弹框
        backend.deny.store(true, Ordering::SeqCst);
        let calls = backend.calls.load(Ordering::SeqCst);
        assert_eq!(kc.rebuild_index(&["x".to_owned(), "y".to_owned()]), 0);
        assert_eq!(backend.calls.load(Ordering::SeqCst), calls + 1);
    }

    #[test]
    fn errors_never_contain_the_key() {
        let backend = Arc::new(MemoryBackend::default());
        backend.deny.store(true, Ordering::SeqCst);
        let kc = memory_keychain(&backend);
        let err = kc.set("p", "sk-super-secret").expect_err("denied");
        assert!(!err.message.contains("sk-super-secret"));
        assert!(!format!("{err:?}").contains("sk-super-secret"));
        let enc = describe(&keyring::Error::BadEncoding(b"sk-raw-bytes".to_vec()));
        assert!(!enc.contains("sk-raw"));
    }

    #[test]
    fn empty_ids_and_keys_are_rejected() {
        let backend = Arc::new(MemoryBackend::default());
        let kc = memory_keychain(&backend);
        assert_eq!(kc.set("", "k").expect_err("id").code, IpcErrorCode::InvalidArgument);
        assert_eq!(
            kc.set("p", "  \n").expect_err("key").code,
            IpcErrorCode::InvalidArgument
        );
        assert_eq!(kc.get(" ").expect_err("id").code, IpcErrorCode::InvalidArgument);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    }

    /// 真实系统钥匙串的往返（会弹出授权框 / 需要 Secret Service），默认忽略
    #[test]
    #[ignore = "需要系统钥匙串（可能弹出授权框）"]
    fn os_keychain_round_trip() {
        let kc = Keychain::new();
        let id = format!("wisp-test-{}", uuid::Uuid::new_v4());
        kc.set(&id, "sk-wisp-test").expect("set");
        assert_eq!(kc.get(&id).expect("get").expect("some").expose(), "sk-wisp-test");
        kc.delete(&id).expect("delete");
        assert!(kc.get(&id).expect("get").is_none());
    }
}
