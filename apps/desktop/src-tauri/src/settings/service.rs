//! `SettingsService`：设置的读取、订阅、补丁、风格与术语表、state.json、钥匙串与能力缓存的入口。
//!
//! **数据流**：所有写入（`patch` / `update` / 风格 / 术语表 / 热加载）都在内存里先算出完整的新值，校验通过后原子写盘，
//! 写盘成功才替换内存并递增 [`SettingsService::changes`] 的修订号；`app::reactor` 订阅修订号，统一处理副作用
//! （日志级别、托盘、外观、自启……）并广播 `settings:changed`。写盘失败时内存保持旧值并返回 `io` 错误。
//!
//! **热加载**：`watcher.rs` 在文件变化（去抖 300 ms）后调用 [`SettingsService::reload`]；内容的 blake3 与已知内容
//! （自己最近一次写入的、启动时读到的、或上一次处理过的外部修改）相同则忽略。手改的文件只做宽松解析与校验，**不回写**
//! （用户改正后自动恢复）；顶层不是合法 JSON 时保留旧值并记 `SettingsIssueKind::InvalidJson`。
//!
//! **锁顺序**（避免死锁与丢更新）：各文件的写锁（settings → `write_lock`，styles → `styles_file`，glossary →
//! `glossary_lock`）在外，`file_io` 在内。写入与热加载都按这个顺序加锁。只有安装改动钩子时同时持有三把写锁，
//! 按 settings → styles → glossary 的顺序取得；其他路径一次只持有一把。
//!
//! **钥匙串**是阻塞调用：异步上下文里用 [`key_get_blocking`] / [`run_blocking`]。
//!
//! **改动钩子**（同步用，[`SettingsService::set_change_hook`]）：三份配置文件的内存值每一次被替换（写入、热加载、同步应用），
//! 都在该文件的写锁内、替换之后立即调用钩子，带上替换前后的规范化文档与来源（[`ChangeOrigin`]）。钩子在锁内调用，
//! 调用顺序就是改动顺序，不会漏掉、也不会与基线错位；没有安装钩子时各写入点不做任何额外工作。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::{Mutex, RwLock};
use tokio::sync::watch;
use wisp_prompts::GlossaryMatcher;
use wisp_types::{
    AppKeymap, DictMode, ExcludedApp, GlossaryEntry, GlossaryFile, ModelEntry, ModelRef, OsKind, PromptPreview,
    Provider, RuntimeInfo, SETTINGS_SCHEMA_VERSION, Settings, SettingsFileIssue, SettingsIssueKind, SettingsSnapshot,
    Style, StylesFile,
};

use crate::app::paths::Paths;
use crate::error::IpcError;
use crate::settings::caps_store::SettingsCapsStore;
use crate::settings::keychain::{BackendFactory, Keychain};
use crate::settings::state_store::StateStore;
use crate::settings::sync_credential::SyncCredentialStore;
use crate::settings::{fsio, glossary_store, migrate, patch, styles_store, validate};

/// 当前引导流程的版本：`onboarding_complete` 写入 state.json；以后引导有实质变化时加一，旧版本的用户会再看到引导
pub const ONBOARDING_VERSION: u32 = 1;

/// 风格温度的允许范围（0–1，留空表示不发）
pub const STYLE_TEMPERATURE: (f32, f32) = (0.0, 1.0);

/// 用户可手改、参与热加载的三份配置文件
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ConfigFile {
    Settings,
    Styles,
    Glossary,
}

impl ConfigFile {
    /// 全部（按提示优先级排列）
    pub const ALL: [Self; 3] = [Self::Settings, Self::Styles, Self::Glossary];

    /// 文件名
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::Settings => "settings.json",
            Self::Styles => "styles.json",
            Self::Glossary => "glossary.json",
        }
    }

    /// 按文件名识别
    pub fn from_file_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.file_name() == name)
    }

    /// 完整路径
    pub fn path(self, paths: &Paths) -> PathBuf {
        match self {
            Self::Settings => paths.settings_file(),
            Self::Styles => paths.styles_file(),
            Self::Glossary => paths.glossary_file(),
        }
    }
}

/// 配置改动的来源。同步只把本机的改动发布出去：自己应用的远端值（`Sync`）不再发回，否则两台设备会互相回传
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeOrigin {
    /// 本机的写入：设置窗口、托盘、快捷键录制、删除服务时的连带清理、能力缓存写回等
    Local,
    /// 用户手改配置文件后的热加载
    HotReload,
    /// 同步应用的远端内容（[`SettingsService::apply_synced`]）
    Sync,
}

/// 一次配置文件内容的替换。`before` / `after` 是规范化文档：先序列化成 JSON 文本再解析回来（与写盘的内容一致）。
/// 直接 `to_value` 会把 f32（`max_height_ratio`、风格温度）拓宽成 0.6000000238418579，与读回的 0.6 比较时出现假差异
#[derive(Debug)]
pub struct ConfigChange<'a> {
    pub file: ConfigFile,
    pub before: &'a serde_json::Value,
    pub after: &'a serde_json::Value,
    pub origin: ChangeOrigin,
    /// 新内容是手改文件的宽松解析结果，而且解析时丢掉或退回了一些内容（某个字段或条目写错了）。按约定这些内容不回写，
    /// 用户改正文件后自动恢复，所以 `after` 里缺少的东西不代表用户删除了它：同步对这种改动只记新增与修改，不记删除
    pub lossy: bool,
}

/// 配置改动的钩子。在该文件的写锁内同步调用：只能做常数时间的工作（记下改动、交给其他线程），不得阻塞，
/// 也不得回调 `SettingsService` 的读写接口（同一把锁不可重入，会死锁）
pub type ChangeHook = Arc<dyn Fn(&ConfigChange<'_>) + Send + Sync>;

/// Key 写入 / 删除的钩子：`(provider_id, Some(key))` 为写入，`None` 为删除。只由用户的操作触发（保存 / 删除 Key、
/// 删除服务）；同步写入远端的 Key、清理遗留的钥匙串条目不触发。同样不得阻塞
pub type SecretHook = Arc<dyn Fn(&str, Option<&str>) + Send + Sync>;

/// 配置文件的读取情况（[`SettingsService::config_load_state`]）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadState {
    /// 正常读取
    Normal,
    /// 启动时文件不存在，已写出默认内容：内存里的值不是用户的选择
    Created,
    /// 启动时文件损坏（已备份）或读不了，改用了默认值：同上
    ResetToDefault,
    /// 热加载遇到非法 JSON（多半正在编辑器里改）：内存保留上一次的有效值，文件暂时不能被覆盖
    InvalidJson,
    /// 内存里的值来自一次有损的宽松解析（启动时或最近一次热加载）：文件里有写错的字段或条目，被退回默认值或丢掉了。
    /// 缺少的内容不代表用户删除了它，同步不把它当作删除发布
    Lossy,
}

/// [`SettingsService::apply_synced`] 的结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncApply {
    /// 合并后（经校验）与当前内容相同，或合并函数返回 None：没有写盘
    Unchanged,
    /// 已写盘并替换内存
    Applied,
    /// 文件正在被手改（非法 JSON，或磁盘上有热加载还没处理的修改）：没有写入，稍后再试
    Deferred,
}

/// 热加载的结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadOutcome {
    /// 内容与自己最近一次写入的相同，或解析后与内存中的值相同
    Unchanged,
    /// 已加载新内容
    Applied,
    /// JSON 非法：保留旧值并记录问题
    InvalidJson,
    /// 文件不存在（被删除）：保留内存中的值
    Missing,
    /// 文件暂时读不了（Windows 上杀毒软件 / 编辑器以独占方式短暂打开它）：保留内存中的值，调用方稍后重试
    Busy,
}

/// 设置服务
pub struct SettingsService {
    paths: Paths,
    /// 当前设置。**写入一律用 `send_replace` / `send_modify`**：tokio 的 `watch::Sender::send` 在没有任何接收者时
    /// 返回 Err 且**不写入新值**，`get()` 会一直读到旧设置（更新被静默丢失）
    current: watch::Sender<Arc<Settings>>,
    /// 常驻的接收端：保证 `current` 永远至少有一个接收者，即使误用 `send` 也不会丢更新（不读取）
    _keep_alive: watch::Receiver<Arc<Settings>>,
    styles: RwLock<Arc<Vec<Style>>>,
    /// styles.json 的内存副本（同时充当风格写入的锁）
    styles_file: Mutex<StylesFile>,
    glossary_entries: RwLock<Arc<Vec<GlossaryEntry>>>,
    glossary: RwLock<Arc<GlossaryMatcher>>,
    /// 术语表写入的锁
    glossary_lock: Mutex<()>,
    state: Arc<StateStore>,
    keychain: Keychain,
    caps: Arc<SettingsCapsStore>,
    /// 各配置文件当前的问题（`RuntimeInfo.settings_issue` 取优先级最高的一条）
    issues: Mutex<BTreeMap<ConfigFile, SettingsFileIssue>>,
    /// 快照修订号：设置、风格、术语表、文件问题、钥匙串状态任何一项变化都加一（`settings:changed` 的触发源）
    revision: Arc<watch::Sender<u64>>,
    _revision_keep_alive: watch::Receiver<u64>,
    /// 串行化 settings.json 的读改写（热加载 settings.json 时同样持有）
    write_lock: Mutex<()>,
    /// 各文件「已知内容」的哈希：自己最近一次写入的、启动时读到的、或热加载已经处理过的外部内容。热加载读到的内容与之
    /// 相同就忽略（自写触发的事件、重复事件）；处理过外部修改后更新为它，这样外部再改回旧内容时也能识别为新的修改
    written: Mutex<HashMap<ConfigFile, fsio::ContentHash>>,
    /// 配置文件的读写互斥：写入（记哈希 + 原子写）与热加载的「读文件 + 比对哈希」不交错，
    /// 否则热加载可能读到上一版内容、却拿去和新记下的哈希比较，把旧内容当成外部修改加载回来
    file_io: Mutex<()>,
    /// state.json 在启动时被重置、而设置里已有 Provider：钥匙串索引（`has_key`）可能漏记，首次需要时核对一次
    key_index_unverified: AtomicBool,
    /// 来自更新版本的配置文件：写盘时保留本版本不认识的内容与原来的 `schema_version`（降级后不丢数据，见 `migrate::Newer`）
    newer: Mutex<HashMap<ConfigFile, migrate::Newer>>,
    /// 配置改动的钩子（同步开启时安装）。只在持有对应文件的写锁时读取；安装时同时持有三把写锁
    change_hook: RwLock<Option<ChangeHook>>,
    /// Key 写入 / 删除的钩子
    secret_hook: RwLock<Option<SecretHook>>,
    /// 串行化 Key 的写入、删除与钩子调用：钩子收到的顺序与钥匙串里最终的结果一致
    secret_lock: Mutex<()>,
    /// 启动时各配置文件的读取情况（不存在而被创建 / 损坏而重置；其余为 Normal）
    startup_state: BTreeMap<ConfigFile, LoadState>,
    /// 内存值来自有损的宽松解析的文件（启动时记下，每次热加载按这次的解析结果更新）。写盘不清除它：写盘后文件里也没有了
    /// 被丢掉的内容，但那仍然不是用户的删除；下一次干净的热加载或重启后才算正常
    lossy: Mutex<BTreeSet<ConfigFile>>,
    /// 同步凭据（钥匙串里单独的一项）
    sync_credentials: SyncCredentialStore,
}

impl std::fmt::Debug for SettingsService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsService")
            .field("config_dir", &self.paths.config_dir)
            .finish_non_exhaustive()
    }
}

impl SettingsService {
    /// 加载设置：`wisp_types::parse_lenient`（单个字段 / 条目非法只退回或丢弃它，逐条 `warn`）；只有顶层不是合法 JSON
    /// 对象时才备份为 `settings.json.bak-<ts>` 并改用默认值。首次运行时写出默认的 settings.json
    pub fn load(paths: &Paths) -> Result<Arc<Self>, IpcError> {
        Ok(Self::load_with(paths, None))
    }

    /// 同 [`Self::load`]，可替换钥匙串后端（测试用）
    pub(crate) fn load_with(paths: &Paths, keychain_backend: Option<BackendFactory>) -> Arc<Self> {
        if let Err(e) = std::fs::create_dir_all(&paths.config_dir) {
            log::error!("cannot create config dir: {e}");
        }
        let mut issues = BTreeMap::new();
        let mut written = HashMap::new();
        let mut startup_state = BTreeMap::new();
        let mut lossy = BTreeSet::new();

        // settings.json
        let settings_path = paths.settings_file();
        let mut newer = HashMap::new();
        let LoadedSettings {
            settings,
            issue: settings_issue,
            rewrite: write_settings,
            hash: settings_hash,
            newer: settings_newer,
            state: settings_state,
            lossy: settings_lossy,
        } = load_settings(&settings_path);
        startup_state.insert(ConfigFile::Settings, settings_state);
        if settings_lossy {
            lossy.insert(ConfigFile::Settings);
        }
        if let Some(n) = settings_newer {
            newer.insert(ConfigFile::Settings, n);
            issues.insert(ConfigFile::Settings, newer_issue(ConfigFile::Settings));
        }
        if let Some(issue) = settings_issue {
            issues.insert(ConfigFile::Settings, issue);
        }
        if let Some(hash) = settings_hash {
            written.insert(ConfigFile::Settings, hash);
        }
        if write_settings {
            match fsio::to_pretty_json(&settings).and_then(|b| {
                fsio::write_atomic(&settings_path, &b)?;
                Ok(b)
            }) {
                Ok(bytes) => {
                    written.insert(ConfigFile::Settings, fsio::content_hash(&bytes));
                }
                Err(e) => log::error!("cannot write settings.json: {e}"),
            }
        }

        // styles.json / glossary.json（不存在时写出默认内容：配置目录里三份文件始终都在，用户可以直接打开编辑，
        // macOS 上热加载也可以只监听这三个文件，见 watcher.rs）
        let styles_file = match styles_store::load_with_backup(&paths.styles_file()) {
            Ok(loaded) => {
                note_loaded(ConfigFile::Styles, &loaded, &mut issues, &mut written, &mut newer);
                startup_state.insert(ConfigFile::Styles, loaded_state(&loaded));
                if !loaded.issues.is_empty() {
                    lossy.insert(ConfigFile::Styles);
                }
                let missing = !loaded.existed;
                let mut value = loaded.value;
                styles_store::normalize(&mut value);
                if missing {
                    write_default(
                        ConfigFile::Styles,
                        &paths.styles_file(),
                        styles_store::encode(&value),
                        &mut written,
                    );
                }
                value
            }
            Err(e) => {
                log::error!("cannot read styles.json: {}", e.message);
                startup_state.insert(ConfigFile::Styles, LoadState::ResetToDefault);
                let mut value = StylesFile::default();
                styles_store::normalize(&mut value);
                value
            }
        };
        let glossary_file = match glossary_store::load_with_backup(&paths.glossary_file()) {
            Ok(loaded) => {
                note_loaded(ConfigFile::Glossary, &loaded, &mut issues, &mut written, &mut newer);
                startup_state.insert(ConfigFile::Glossary, loaded_state(&loaded));
                if !loaded.issues.is_empty() {
                    lossy.insert(ConfigFile::Glossary);
                }
                if !loaded.existed {
                    write_default(
                        ConfigFile::Glossary,
                        &paths.glossary_file(),
                        glossary_store::encode(&loaded.value),
                        &mut written,
                    );
                }
                loaded.value
            }
            Err(e) => {
                log::error!("cannot read glossary.json: {}", e.message);
                startup_state.insert(ConfigFile::Glossary, LoadState::ResetToDefault);
                GlossaryFile::default()
            }
        };

        // state.json 与钥匙串索引
        let state = Arc::new(StateStore::load(paths.state_file()));
        let (revision, revision_rx) = watch::channel(0u64);
        let revision = Arc::new(revision);
        // 替换了 Key 的后端（测试、冒烟）时，同步凭据同样只放在进程内存里，不碰系统钥匙串
        let sync_credentials = if keychain_backend.is_some() {
            SyncCredentialStore::with_backend(crate::settings::keychain::memory_backend())
        } else {
            SyncCredentialStore::new()
        };
        let keychain = {
            let base = match keychain_backend {
                Some(factory) => Keychain::with_backend(factory),
                None => Keychain::new(),
            };
            let persist_state = Arc::clone(&state);
            let notify = Arc::clone(&revision);
            base.with_index(
                state.read(|s| s.stored_keys.clone()),
                move |ids| {
                    if let Err(e) = persist_state.update(&mut |s| s.stored_keys.clone_from(ids)) {
                        log::warn!("cannot persist keychain index: {}", e.message);
                    }
                },
                move || notify.send_modify(|r| *r = r.wrapping_add(1)),
            )
        };

        let key_index_unverified = state.was_reset() && !settings.providers.is_empty();
        if key_index_unverified {
            log::info!("state.json was reset; the keychain index will be verified when it is first needed");
        }
        let (current, keep_alive) = watch::channel(Arc::new(settings));
        let styles = styles_store::resolve(&styles_file);
        let matcher = GlossaryMatcher::new(&glossary_file.entries);
        let svc = Arc::new(Self {
            paths: paths.clone(),
            current,
            _keep_alive: keep_alive,
            styles: RwLock::new(Arc::new(styles)),
            styles_file: Mutex::new(styles_file),
            glossary_entries: RwLock::new(Arc::new(glossary_file.entries)),
            glossary: RwLock::new(Arc::new(matcher)),
            glossary_lock: Mutex::new(()),
            state,
            keychain,
            caps: Arc::new(SettingsCapsStore::new()),
            issues: Mutex::new(issues),
            revision,
            _revision_keep_alive: revision_rx,
            write_lock: Mutex::new(()),
            written: Mutex::new(written),
            file_io: Mutex::new(()),
            key_index_unverified: AtomicBool::new(key_index_unverified),
            newer: Mutex::new(newer),
            change_hook: RwLock::new(None),
            secret_hook: RwLock::new(None),
            secret_lock: Mutex::new(()),
            startup_state,
            lossy: Mutex::new(lossy),
            sync_credentials,
        });
        svc.caps.bind(Arc::downgrade(&svc));
        svc
    }

    /// 隔离加载：钥匙串只用进程内存（冒烟模式；配合 `Paths::smoke` 使用，不碰用户的真实数据与系统钥匙串）
    pub fn load_isolated(paths: &Paths) -> Arc<Self> {
        Self::load_with(paths, Some(crate::settings::keychain::memory_backend()))
    }

    /// 单测用：使用内存钥匙串后端加载
    #[cfg(test)]
    pub(crate) fn load_for_tests(paths: &Paths) -> Arc<Self> {
        let backend = Arc::new(crate::settings::keychain::tests::MemoryBackend::default());
        Self::load_with(
            paths,
            Some(Box::new(move || {
                Ok(Box::new(Arc::clone(&backend)) as Box<dyn crate::settings::keychain::SecretBackend>)
            })),
        )
    }

    /// 单测用：持有设置的写锁，模拟一次正在进行的写入
    #[cfg(test)]
    pub(crate) fn write_lock_for_tests(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.write_lock.lock()
    }

    /// 应用目录
    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// 当前设置
    pub fn get(&self) -> Arc<Settings> {
        self.current.borrow().clone()
    }

    /// 订阅设置变化
    pub fn subscribe(&self) -> watch::Receiver<Arc<Settings>> {
        self.current.subscribe()
    }

    /// 订阅快照修订号（设置 / 风格 / 术语表 / 文件问题 / 钥匙串状态任何一项变化都会递增）
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.revision.subscribe()
    }

    /// 通知快照可能已变化（其他模块改了快照里的运行时信息、需要广播 `settings:changed` 时调用）
    pub fn notify_changed(&self) {
        self.revision.send_modify(|r| *r = r.wrapping_add(1));
    }

    /// RFC 7396 → `wisp_types::from_value_lenient` 回 `Settings`（问题列表非空即 `invalid_argument`，不落盘）→ 校验
    /// （夹紧可以接受，有损修正即 `invalid_argument`）→ 落盘 → `current.send_replace` → 广播
    pub fn patch(&self, patch: serde_json::Value) -> Result<Arc<Settings>, IpcError> {
        let _w = self.write_lock.lock();
        let current = self.get();
        let mut value = serde_json::to_value(&*current).map_err(|e| IpcError::internal(e.to_string()))?;
        patch::merge_patch(&mut value, &patch);
        let (mut next, issues) = wisp_types::from_value_lenient::<Settings>(value)
            .map_err(|_| IpcError::invalid_argument("settings must be a JSON object"))?;
        if !issues.is_empty() {
            let paths: Vec<&str> = issues.iter().map(|i| i.path.as_str()).collect();
            return Err(IpcError::invalid_argument(format!(
                "invalid settings value at {}",
                paths.join(", ")
            )));
        }
        self.validated(&mut next)?;
        self.commit(next)
    }

    /// 以闭包修改设置 → 校验 → 落盘 → `current.send_replace` → 广播（内容没变时不写盘、不广播）
    pub fn update(&self, f: &mut dyn FnMut(&mut Settings)) -> Result<Arc<Settings>, IpcError> {
        let _w = self.write_lock.lock();
        let mut next = (*self.get()).clone();
        f(&mut next);
        self.validated(&mut next)?;
        self.commit(next)
    }

    /// 校验：夹紧记 warn；有损修正（非法值、非法条目）返回 `invalid_argument`
    fn validated(&self, next: &mut Settings) -> Result<(), IpcError> {
        let fixes = validate::check(next);
        let lossy: Vec<&str> = fixes.iter().filter(|f| f.is_lossy()).map(|f| f.path.as_str()).collect();
        if !lossy.is_empty() {
            return Err(IpcError::invalid_argument(format!(
                "invalid settings value at {}",
                lossy.join(", ")
            )));
        }
        for fix in &fixes {
            log::warn!("settings: {fix}");
        }
        Ok(())
    }

    /// 写盘并替换内存（调用方持有 `write_lock`）
    fn commit(&self, next: Settings) -> Result<Arc<Settings>, IpcError> {
        self.commit_as(next, ChangeOrigin::Local)
    }

    /// 同 [`Self::commit`]，并以 `origin` 调用改动钩子
    fn commit_as(&self, next: Settings, origin: ChangeOrigin) -> Result<Arc<Settings>, IpcError> {
        let current = self.get();
        if *current == next {
            return Ok(current);
        }
        let bytes = fsio::to_pretty_json(&next)?;
        self.write_file(ConfigFile::Settings, &bytes)?;
        let next = Arc::new(next);
        self.current.send_replace(Arc::clone(&next));
        self.notify_changed();
        self.report_change(
            ConfigFile::Settings,
            origin,
            false,
            || canonical(&*current),
            || canonical(&*next),
        );
        Ok(next)
    }

    /// 调用改动钩子（**调用方持有该文件的写锁**）。没有安装钩子时直接返回，不做序列化
    fn report_change(
        &self,
        file: ConfigFile,
        origin: ChangeOrigin,
        lossy: bool,
        before: impl FnOnce() -> serde_json::Value,
        after: impl FnOnce() -> serde_json::Value,
    ) {
        let Some(hook) = self.change_hook.read().clone() else {
            return;
        };
        let (before, after) = (before(), after());
        // 内存值不同、规范化文档却相同（例如只差 f32 的拓宽误差）：对同步来说没有改动
        if before == after {
            return;
        }
        hook(&ConfigChange {
            file,
            before: &before,
            after: &after,
            origin,
            lossy,
        });
    }

    /// 安装（`Some`）或卸载（`None`）配置改动钩子，返回安装那一刻三份文件的规范化文档（作为同步的基线）。
    ///
    /// 按 settings → styles → glossary 的固定顺序同时持有三把写锁：在此之前的改动都已在返回的文档里，之后的改动都会
    /// 经过钩子，两者之间没有缝隙（其他路径从不同时持有两把写锁，不会死锁）
    pub fn set_change_hook(&self, hook: Option<ChangeHook>) -> BTreeMap<ConfigFile, serde_json::Value> {
        let _w = self.write_lock.lock();
        let styles = self.styles_file.lock();
        let _g = self.glossary_lock.lock();
        *self.change_hook.write() = hook;
        BTreeMap::from([
            (ConfigFile::Settings, canonical(&*self.get())),
            (ConfigFile::Styles, canonical(&*styles)),
            (ConfigFile::Glossary, glossary_doc(&self.glossary_entries())),
        ])
    }

    /// 某个配置文件当前内容的规范化文档（与钩子收到的 `before` / `after` 同一形式）。**不要在钩子里调用**
    pub fn config_value(&self, file: ConfigFile) -> serde_json::Value {
        match file {
            ConfigFile::Settings => canonical(&*self.get()),
            ConfigFile::Styles => canonical(&*self.styles_file.lock()),
            ConfigFile::Glossary => glossary_doc(&self.glossary_entries()),
        }
    }

    /// 配置文件的读取情况：热加载遇到非法 JSON 时为 `InvalidJson`（改回合法内容后恢复）；否则是启动时的情况——
    /// 文件不存在而被创建（`Created`）、损坏或读不了而用了默认值（`ResetToDefault`），这两种情况下内存里的默认值
    /// 不代表用户的选择，同步不应把它们当作本机的改动发布；再其次，启动时或最近一次热加载的宽松解析丢掉了内容时为
    /// `Lossy`（同步不把缺少的内容当作删除）
    pub fn config_load_state(&self, file: ConfigFile) -> LoadState {
        if self.file_invalid(file) {
            return LoadState::InvalidJson;
        }
        match self.startup_state.get(&file).copied().unwrap_or(LoadState::Normal) {
            LoadState::Normal if self.lossy.lock().contains(&file) => LoadState::Lossy,
            state => state,
        }
    }

    /// 记下这次热加载的解析是否有损（**调用方持有该文件的写锁**），返回同一个值
    fn note_lossy(&self, file: ConfigFile, lossy: bool) -> bool {
        let mut set = self.lossy.lock();
        if lossy {
            set.insert(file);
        } else {
            set.remove(&file);
        }
        lossy
    }

    /// 热加载在这个文件里遇到了非法 JSON（还没改回来）
    fn file_invalid(&self, file: ConfigFile) -> bool {
        self.issues
            .lock()
            .get(&file)
            .is_some_and(|i| i.kind == SettingsIssueKind::InvalidJson)
    }

    /// 同步现在不能写这个文件（**调用方持有该文件的写锁**）：文件是非法 JSON、读不了，或者磁盘上有还没处理的手改。
    ///
    /// 有手改时不等热加载：监听可能没能启动（例如 Linux 的 inotify 名额用完）或者漏了事件，那样热加载永远不会来，远端的值
    /// 也就永远写不进来。`reload` 在同一把锁内把它作为热加载处理掉（改动钩子照常以 `HotReload` 记下这次手改），然后这一批
    /// 远端值留到下一轮：那时手改已经进了副本，引擎按先后取舍，不会用更旧的远端值盖掉用户刚改的内容。内容与内存相同
    /// （只改了空白之类）就直接照常写入
    fn sync_write_blocked(&self, file: ConfigFile, reload: impl FnOnce() -> ReloadOutcome) -> bool {
        if self.file_invalid(file) {
            return true;
        }
        let edited = {
            let _io = self.file_io.lock();
            match fsio::read_optional(&file.path(&self.paths)) {
                Ok(Some(bytes)) => self.written.lock().get(&file) != Some(&fsio::content_hash(&bytes)),
                // 文件被删掉了：没有用户的内容可丢，照常写回
                Ok(None) => false,
                Err(e) => {
                    log::debug!(
                        "{} cannot be read before a sync write ({e}); deferring",
                        file.file_name()
                    );
                    return true;
                }
            }
        };
        if !edited {
            return false;
        }
        match reload() {
            ReloadOutcome::Unchanged | ReloadOutcome::Missing => false,
            ReloadOutcome::Applied | ReloadOutcome::InvalidJson | ReloadOutcome::Busy => true,
        }
    }

    /// 应用同步合并后的内容（**阻塞**）。在该文件的写锁内：取当前的规范化文档 → `merge` 返回新文档（None = 不变）→
    /// 迁移 → 宽松解析 → 保留只属于本机的设置 → 校验（与热加载一样接受夹紧与修正，不拒绝）→ 写盘（记下哈希，热加载
    /// 忽略这次写入）→ 替换内存 → 广播（副作用与 `settings:changed` 照常发生）→ 以 `Sync` 调用改动钩子。
    ///
    /// 文件正在被手改（非法 JSON，或有热加载还没处理的修改：先在锁内把它作为热加载处理）时不写，返回
    /// [`SyncApply::Deferred`]。`merge` 在锁内调用，不得回调 `SettingsService`
    pub fn apply_synced(
        &self,
        file: ConfigFile,
        merge: &mut dyn FnMut(&serde_json::Value) -> Option<serde_json::Value>,
    ) -> Result<SyncApply, IpcError> {
        match file {
            ConfigFile::Settings => {
                let _w = self.write_lock.lock();
                if self.sync_write_blocked(file, || self.reload_locked(file, |value| self.apply_settings(value))) {
                    return Ok(SyncApply::Deferred);
                }
                let current = self.get();
                let Some(merged) = merge(&canonical(&*current)) else {
                    return Ok(SyncApply::Unchanged);
                };
                let (merged, _) = migrate::run(merged, SETTINGS_SCHEMA_VERSION, migrate::SETTINGS_STEPS);
                let (mut next, issues) = wisp_types::from_value_lenient::<Settings>(merged)
                    .map_err(|_| IpcError::invalid_argument("synced settings must be a JSON object"))?;
                for issue in &issues {
                    log::warn!("synced settings: {issue}");
                }
                keep_local_settings(&mut next, &current);
                for fix in validate::check(&mut next) {
                    log::warn!("synced settings: {fix}");
                }
                if *current == next {
                    return Ok(SyncApply::Unchanged);
                }
                self.commit_as(next, ChangeOrigin::Sync)?;
                Ok(SyncApply::Applied)
            }
            ConfigFile::Styles => {
                let mut styles = self.styles_file.lock();
                if self.sync_write_blocked(file, || {
                    self.reload_locked(file, |value| self.apply_styles(&mut styles, value))
                }) {
                    return Ok(SyncApply::Deferred);
                }
                let Some(merged) = merge(&canonical(&*styles)) else {
                    return Ok(SyncApply::Unchanged);
                };
                let (merged, _) = migrate::run(merged, migrate::DOC_SCHEMA_VERSION, migrate::STYLES_STEPS);
                let (mut next, issues) = wisp_types::from_value_lenient::<StylesFile>(merged)
                    .map_err(|_| IpcError::invalid_argument("synced styles must be a JSON object"))?;
                for issue in &issues {
                    log::warn!("synced styles: {issue}");
                }
                next.schema_version = styles.schema_version;
                styles_store::normalize(&mut next);
                if next == *styles {
                    return Ok(SyncApply::Unchanged);
                }
                self.write_file(ConfigFile::Styles, &styles_store::encode(&next)?)?;
                self.install_styles(&mut styles, next, ChangeOrigin::Sync, false);
                Ok(SyncApply::Applied)
            }
            ConfigFile::Glossary => {
                let _g = self.glossary_lock.lock();
                if self.sync_write_blocked(file, || self.reload_locked(file, |value| self.apply_glossary(value))) {
                    return Ok(SyncApply::Deferred);
                }
                let current = self.glossary_entries();
                let Some(merged) = merge(&glossary_doc(&current)) else {
                    return Ok(SyncApply::Unchanged);
                };
                let (merged, _) = migrate::run(merged, migrate::DOC_SCHEMA_VERSION, migrate::GLOSSARY_STEPS);
                let (next, issues) = wisp_types::from_value_lenient::<GlossaryFile>(merged)
                    .map_err(|_| IpcError::invalid_argument("synced glossary must be a JSON object"))?;
                for issue in &issues {
                    log::warn!("synced glossary: {issue}");
                }
                if self.store_glossary_locked(next.entries, ChangeOrigin::Sync)? {
                    Ok(SyncApply::Applied)
                } else {
                    Ok(SyncApply::Unchanged)
                }
            }
        }
    }

    /// 原子写入并记下内容哈希（热加载据此忽略自写）。文件来自更新的版本时，合并回本版本不认识的内容并保留原来的
    /// `schema_version`（能力缓存的自动写回等任何写盘都不会在降级后丢掉新版本的字段）
    fn write_file(&self, file: ConfigFile, bytes: &[u8]) -> Result<(), IpcError> {
        let preserved = match self.newer.lock().get(&file) {
            Some(newer) => Some(preserve_newer(bytes, newer)?),
            None => None,
        };
        let bytes = preserved.as_deref().unwrap_or(bytes);
        let _io = self.file_io.lock();
        let hash = fsio::content_hash(bytes);
        let previous = self.written.lock().insert(file, hash);
        if let Err(e) = fsio::write_atomic(&file.path(&self.paths), bytes) {
            let mut written = self.written.lock();
            match previous {
                Some(h) => written.insert(file, h),
                None => written.remove(&file),
            };
            log::error!("cannot write {}: {e}", file.file_name());
            return Err(e.into());
        }
        Ok(())
    }

    /// 热加载：重新读取某个配置文件（`watcher.rs` 在去抖后调用）。
    ///
    /// 先拿该文件的写锁（settings → `write_lock`，styles → `styles_file`，glossary → `glossary_lock`），再在 `file_io` 内
    /// 读文件、比对哈希，解析与应用也都在写锁内完成：「读到外部内容 X」与「把 X 装进内存」之间不会插进一次写入。否则 patch
    /// 可能在这个间隙把基于旧内存的 Y 写盘，随后内存被换成 X、磁盘却是 Y，而 Y 的文件事件又被当成自写忽略——这次 patch
    /// 就静默丢了，下一次写盘还会把它覆盖掉
    pub fn reload(&self, file: ConfigFile) -> ReloadOutcome {
        match file {
            ConfigFile::Settings => {
                let _w = self.write_lock.lock();
                self.reload_locked(file, |value| self.apply_settings(value))
            }
            ConfigFile::Styles => {
                let mut styles = self.styles_file.lock();
                self.reload_locked(file, |value| self.apply_styles(&mut styles, value))
            }
            ConfigFile::Glossary => {
                let _g = self.glossary_lock.lock();
                self.reload_locked(file, |value| self.apply_glossary(value))
            }
        }
    }

    /// 读文件 → 已知内容直接忽略 → 解析 JSON → `apply`（**调用方持有该文件的写锁**）
    fn reload_locked(&self, file: ConfigFile, apply: impl FnOnce(serde_json::Value) -> ReloadOutcome) -> ReloadOutcome {
        let path = file.path(&self.paths);
        let bytes = {
            let _io = self.file_io.lock();
            let bytes = match fsio::read_optional(&path) {
                Ok(Some(b)) => b,
                Ok(None) => {
                    log::info!("{} was removed; keeping the settings in memory", file.file_name());
                    return ReloadOutcome::Missing;
                }
                Err(e) if fsio::is_transient_sharing_error(wisp_types::OsKind::current(), &e) => {
                    log::debug!("{} is busy ({e}); the reload will be retried", file.file_name());
                    return ReloadOutcome::Busy;
                }
                Err(e) => {
                    log::warn!("cannot read {}: {e}", file.file_name());
                    return ReloadOutcome::Unchanged;
                }
            };
            // 与已知内容相同（自写、重复事件）：忽略；否则记为已知，之后同样的内容不再重复处理
            let hash = fsio::content_hash(&bytes);
            if self.written.lock().insert(file, hash) == Some(hash) {
                return ReloadOutcome::Unchanged;
            }
            bytes
        };
        let value = match serde_json::from_slice::<serde_json::Value>(fsio::strip_bom(&bytes)) {
            Ok(v) if v.is_object() => v,
            _ => {
                log::warn!("{} is not valid JSON; keeping the previous values", file.file_name());
                self.set_issue(file, Some(SettingsIssueKind::InvalidJson), None);
                return ReloadOutcome::InvalidJson;
            }
        };
        let outcome = apply(value);
        // 合法的 JSON：清掉非法 JSON 的提示；文件仍来自更新的版本时改为 NewerVersion（apply 里的 note_newer 已更新记录）
        let newer = self.newer.lock().contains_key(&file);
        self.set_issue(file, newer.then_some(SettingsIssueKind::NewerVersion), None);
        outcome
    }

    /// 应用外部修改后的 settings.json（调用方持有 `write_lock`）
    fn apply_settings(&self, value: serde_json::Value) -> ReloadOutcome {
        let (value, outcome) = migrate::run(value, SETTINGS_SCHEMA_VERSION, migrate::SETTINGS_STEPS);
        let raw = newer_raw(outcome, &value);
        let Ok((mut next, issues)) = wisp_types::from_value_lenient::<Settings>(value) else {
            return ReloadOutcome::Unchanged;
        };
        for issue in &issues {
            log::warn!("settings.json: {issue}");
        }
        let fixes = validate::check(&mut next);
        for fix in &fixes {
            log::warn!("settings.json: {fix}");
        }
        let lossy = self.note_lossy(
            ConfigFile::Settings,
            !issues.is_empty() || fixes.iter().any(validate::Fix::is_lossy),
        );
        self.note_newer(ConfigFile::Settings, outcome, raw.as_ref(), &next);
        let current = self.get();
        if *current == next {
            return ReloadOutcome::Unchanged;
        }
        log::info!("settings.json reloaded after an external change");
        let next = Arc::new(next);
        self.current.send_replace(Arc::clone(&next));
        self.notify_changed();
        self.report_change(
            ConfigFile::Settings,
            ChangeOrigin::HotReload,
            lossy,
            || canonical(&*current),
            || canonical(&*next),
        );
        ReloadOutcome::Applied
    }

    /// 应用外部修改后的 styles.json（`file` 即持有中的 `styles_file`）
    fn apply_styles(&self, file: &mut StylesFile, value: serde_json::Value) -> ReloadOutcome {
        let (value, outcome) = migrate::run(value, migrate::DOC_SCHEMA_VERSION, migrate::STYLES_STEPS);
        let raw = newer_raw(outcome, &value);
        let Ok((mut next, issues)) = wisp_types::from_value_lenient::<StylesFile>(value) else {
            return ReloadOutcome::Unchanged;
        };
        for issue in &issues {
            log::warn!("styles.json: {issue}");
        }
        let lossy = self.note_lossy(ConfigFile::Styles, !issues.is_empty());
        self.note_newer(ConfigFile::Styles, outcome, raw.as_ref(), &next);
        styles_store::normalize(&mut next);
        if *file == next {
            return ReloadOutcome::Unchanged;
        }
        log::info!("styles.json reloaded after an external change");
        self.install_styles(file, next, ChangeOrigin::HotReload, lossy);
        ReloadOutcome::Applied
    }

    /// 替换风格的内存值（styles.json 已写好或是热加载读到的；`file` 即持有中的 `styles_file`）→ 通知 → 改动钩子
    fn install_styles(&self, file: &mut StylesFile, next: StylesFile, origin: ChangeOrigin, lossy: bool) {
        *self.styles.write() = Arc::new(styles_store::resolve(&next));
        let previous = std::mem::replace(file, next);
        self.notify_changed();
        self.report_change(
            ConfigFile::Styles,
            origin,
            lossy,
            || canonical(&previous),
            || canonical(&*file),
        );
    }

    /// 应用外部修改后的 glossary.json（调用方持有 `glossary_lock`）
    fn apply_glossary(&self, value: serde_json::Value) -> ReloadOutcome {
        let (value, outcome) = migrate::run(value, migrate::DOC_SCHEMA_VERSION, migrate::GLOSSARY_STEPS);
        let raw = newer_raw(outcome, &value);
        let Ok((next, issues)) = wisp_types::from_value_lenient::<GlossaryFile>(value) else {
            return ReloadOutcome::Unchanged;
        };
        for issue in &issues {
            log::warn!("glossary.json: {issue}");
        }
        let lossy = self.note_lossy(ConfigFile::Glossary, !issues.is_empty());
        self.note_newer(ConfigFile::Glossary, outcome, raw.as_ref(), &next);
        let entries = glossary_store::normalize(next.entries);
        if *self.glossary_entries() == entries {
            return ReloadOutcome::Unchanged;
        }
        log::info!("glossary.json reloaded after an external change");
        self.install_glossary(entries, ChangeOrigin::HotReload, lossy);
        ReloadOutcome::Applied
    }

    /// 热加载后更新「来自更新版本」的记录（文件被改回当前版本时清除）
    fn note_newer<T: serde::Serialize>(
        &self,
        file: ConfigFile,
        outcome: migrate::Outcome,
        raw: Option<&serde_json::Value>,
        ours: &T,
    ) {
        let newer = serde_json::to_value(ours)
            .ok()
            .and_then(|ours| migrate::Newer::from_outcome(outcome, raw, &ours));
        let mut map = self.newer.lock();
        match newer {
            Some(n) => {
                map.insert(file, n);
            }
            None => {
                map.remove(&file);
            }
        }
    }

    /// 设置或清除某个文件的问题；有变化时通知
    fn set_issue(&self, file: ConfigFile, kind: Option<SettingsIssueKind>, backup: Option<String>) {
        let changed = {
            let mut issues = self.issues.lock();
            match kind {
                Some(kind) => {
                    if issues.get(&file).is_some_and(|i| i.kind == kind) {
                        false
                    } else {
                        issues.insert(
                            file,
                            SettingsFileIssue {
                                kind,
                                file: file.file_name().to_owned(),
                                backup_path: backup,
                                at_ms: fsio::unix_now_ms(),
                            },
                        );
                        true
                    }
                }
                None => issues.remove(&file).is_some(),
            }
        };
        if changed {
            self.notify_changed();
        }
    }

    /// 解析后的全部风格（内置 + override + 自定义，按 order 排序）
    pub fn styles(&self) -> Arc<Vec<Style>> {
        self.styles.read().clone()
    }

    /// 修改 styles.json：闭包在副本上修改，规范化后与旧值不同才写盘、重新解析并通知
    fn mutate_styles(
        &self,
        f: impl FnOnce(&mut StylesFile, &[Style]) -> Result<(), IpcError>,
    ) -> Result<Arc<Vec<Style>>, IpcError> {
        let mut file = self.styles_file.lock();
        let mut next = file.clone();
        f(&mut next, &self.styles())?;
        styles_store::normalize(&mut next);
        if next != *file {
            self.write_file(ConfigFile::Styles, &styles_store::encode(&next)?)?;
            self.install_styles(&mut file, next, ChangeOrigin::Local, false);
        }
        drop(file);
        Ok(self.styles())
    }

    /// 保存风格（新建自定义风格或修改内置风格的 override）。
    /// - 内置风格：与代码里的默认版本比较，只把不同的字段存为 override（相同则删除 override）；名称不可改；
    /// - 自定义风格：`id` 为空时生成 `custom-<uuid8>`；名称不能为空；新风格追加到顺序末尾；
    /// - `temperature` 必须在 0–1（null 表示不发送）；`hotkey` 必须能被解析（空串视为未绑定）
    pub fn style_save(&self, s: Style) -> Result<Arc<Vec<Style>>, IpcError> {
        let mut s = s;
        if let Some(t) = s.temperature
            && !(t.is_finite() && (STYLE_TEMPERATURE.0..=STYLE_TEMPERATURE.1).contains(&t))
        {
            return Err(IpcError::invalid_argument("style.temperature must be between 0 and 1"));
        }
        s.hotkey = s.hotkey.map(|h| h.trim().to_owned()).filter(|h| !h.is_empty());
        if let Some(h) = &s.hotkey
            && !validate::shortcut_parses(h)
        {
            return Err(IpcError::invalid_argument("style.hotkey is not a valid shortcut"));
        }
        if s.model_ref
            .as_ref()
            .is_some_and(|m| m.provider_id.trim().is_empty() || m.model_id.trim().is_empty())
        {
            return Err(IpcError::invalid_argument("style.model_ref is incomplete"));
        }
        // 风格绑定随同步到其他设备：不能指向只属于本机的离线模型（保留的服务商 id）
        if s.model_ref
            .as_ref()
            .is_some_and(|m| validate::is_reserved_provider_id(&m.provider_id))
        {
            return Err(IpcError::invalid_argument(
                "style.model_ref must not use a reserved provider",
            ));
        }
        if styles_store::is_builtin(&s.id) {
            let base = wisp_prompts::builtin_styles()
                .into_iter()
                .find(|b| b.id == s.id)
                .ok_or_else(|| IpcError::not_found("builtin style"))?;
            let ov = styles_store::override_diff(&base, &s);
            let id = s.id.clone();
            return self.mutate_styles(move |file, _| {
                if ov.is_empty() {
                    file.overrides.remove(&id);
                } else {
                    file.overrides.insert(id, ov);
                }
                Ok(())
            });
        }
        let name = s.name.trim().to_owned();
        if name.is_empty() {
            return Err(IpcError::invalid_argument("style.name must not be empty"));
        }
        s.name = name;
        s.builtin = false;
        s.modified = false;
        if !s.id.is_empty() && !s.id.starts_with(wisp_types::defaults::CUSTOM_STYLE_PREFIX) {
            return Err(IpcError::invalid_argument("custom style ids must start with custom-"));
        }
        self.mutate_styles(move |file, resolved| {
            if s.id.is_empty() {
                s.id = unique_custom_id(resolved);
            }
            match file.custom.iter_mut().find(|c| c.id == s.id) {
                Some(existing) => *existing = s,
                None => {
                    if file.order.is_empty() {
                        // 先把当前顺序具体化，否则只含新 id 的 order 会把它排到最前面
                        file.order = resolved.iter().map(|r| r.id.clone()).collect();
                    }
                    file.order.push(s.id.clone());
                    file.custom.push(s);
                }
            }
            Ok(())
        })
    }

    /// 删除自定义风格（内置风格不能删除）；被删的风格是默认风格时，默认风格改回 natural
    pub fn style_delete(&self, id: &str) -> Result<Arc<Vec<Style>>, IpcError> {
        if styles_store::is_builtin(id) {
            return Err(IpcError::invalid_argument("builtin styles cannot be deleted"));
        }
        let styles = self.mutate_styles(|file, _| {
            let before = file.custom.len();
            file.custom.retain(|c| c.id != id);
            if file.custom.len() == before {
                return Err(IpcError::not_found("style"));
            }
            file.order.retain(|o| o != id);
            Ok(())
        })?;
        if self.get().translate.default_style == id {
            self.update(&mut |s| wisp_types::defaults::DEFAULT_STYLE_ID.clone_into(&mut s.translate.default_style))?;
        }
        Ok(styles)
    }

    /// 还原内置风格（删掉 override）
    pub fn style_reset(&self, id: &str) -> Result<Arc<Vec<Style>>, IpcError> {
        if !styles_store::is_builtin(id) {
            return if self.styles().iter().any(|s| s.id == id) {
                Err(IpcError::invalid_argument("only builtin styles can be reset"))
            } else {
                Err(IpcError::not_found("style"))
            };
        }
        self.mutate_styles(|file, _| {
            file.overrides.remove(id);
            Ok(())
        })
    }

    /// 调整风格顺序：未知 id 忽略，列表里缺少的风格按原顺序追加到末尾
    pub fn styles_reorder(&self, ids: Vec<String>) -> Result<Arc<Vec<Style>>, IpcError> {
        self.mutate_styles(|file, resolved| {
            let known: Vec<&str> = resolved.iter().map(|s| s.id.as_str()).collect();
            let mut order: Vec<String> = Vec::with_capacity(known.len());
            for id in ids {
                if known.contains(&id.as_str()) && !order.contains(&id) {
                    order.push(id);
                }
            }
            for id in known {
                if !order.iter().any(|o| o == id) {
                    order.push(id.to_owned());
                }
            }
            file.order = order;
            Ok(())
        })
    }

    /// 预览最终提示词（`style_preview`）：按当前的主 / 次语言与术语表装配一次翻译提示词
    pub fn style_preview(&self, style: &Style, sample: &str) -> PromptPreview {
        let settings = self.get();
        let source = wisp_prompts::detect_lang(sample);
        let target = wisp_prompts::target_for(
            &source,
            settings.translate.primary_lang,
            settings.translate.secondary_lang,
        );
        let dict = wisp_prompts::use_dictionary(sample, style, DictMode::Auto);
        let hits = self.glossary().hits(sample);
        let built = wisp_prompts::build(
            &wisp_prompts::PromptTask::Translate { style, dict },
            &wisp_prompts::PromptCtx {
                text: sample,
                source,
                target,
                glossary: &hits,
                reasoning_model: false,
            },
        );
        PromptPreview {
            system: built.system,
            user: built.user_text,
        }
    }

    /// 术语匹配器
    pub fn glossary(&self) -> Arc<GlossaryMatcher> {
        self.glossary.read().clone()
    }

    /// 术语表条目
    pub fn glossary_entries(&self) -> Arc<Vec<GlossaryEntry>> {
        self.glossary_entries.read().clone()
    }

    /// 替换术语表的内存值（调用方持有 `glossary_lock`）→ 通知 → 改动钩子
    fn install_glossary(&self, entries: Vec<GlossaryEntry>, origin: ChangeOrigin, lossy: bool) {
        let previous = self.glossary_entries();
        *self.glossary.write() = Arc::new(GlossaryMatcher::new(&entries));
        let next = Arc::new(entries);
        *self.glossary_entries.write() = Arc::clone(&next);
        self.notify_changed();
        self.report_change(
            ConfigFile::Glossary,
            origin,
            lossy,
            || glossary_doc(&previous),
            || glossary_doc(&next),
        );
    }

    /// 规范化后与当前不同才写盘并替换（调用方持有 `glossary_lock`）；返回是否有变化
    fn store_glossary_locked(&self, entries: Vec<GlossaryEntry>, origin: ChangeOrigin) -> Result<bool, IpcError> {
        let entries = glossary_store::normalize(entries);
        if *self.glossary_entries() == entries {
            return Ok(false);
        }
        let file = GlossaryFile {
            schema_version: migrate::DOC_SCHEMA_VERSION,
            entries,
        };
        self.write_file(ConfigFile::Glossary, &glossary_store::encode(&file)?)?;
        self.install_glossary(file.entries, origin, false);
        Ok(true)
    }

    /// 替换术语表（条目先规范化：去空白、丢弃空 source、按 source 去重）。只给测试准备数据：设置页逐条写入
    /// （`glossary_upsert` / `glossary_remove`），整表替换会覆盖编辑器打开之后别处（其他设备同步、导入）加进来的术语
    #[cfg(test)]
    pub(crate) fn glossary_set(&self, entries: Vec<GlossaryEntry>) -> Result<(), IpcError> {
        let _g = self.glossary_lock.lock();
        self.store_glossary_locked(entries, ChangeOrigin::Local)?;
        Ok(())
    }

    /// 写入一条术语（设置页逐条编辑）：在锁内对最新的术语表只改这一条，编辑器打开之后别处加进来的术语原样保留。
    /// `previous_source` 是这一行改动前的原文：改了原文时新条目占原条目的位置；已有同原文的条目时合并为一条（位置取
    /// 两者中靠前的）；都没有就追加到末尾。原文去掉首尾空白后为空时 `invalid_argument`
    pub fn glossary_upsert(&self, entry: GlossaryEntry, previous_source: Option<&str>) -> Result<(), IpcError> {
        let entry = glossary_store::normalize(vec![entry])
            .pop()
            .ok_or_else(|| IpcError::invalid_argument("entry.source must not be empty"))?;
        let previous = previous_source.map(str::trim).filter(|p| !p.is_empty());
        let _g = self.glossary_lock.lock();
        let current = self.glossary_entries();
        let mut next = Vec::with_capacity(current.len() + 1);
        let mut placed = false;
        for e in current.iter() {
            if e.source == entry.source || previous == Some(e.source.as_str()) {
                if !placed {
                    next.push(entry.clone());
                    placed = true;
                }
                continue;
            }
            next.push(e.clone());
        }
        if !placed {
            next.push(entry);
        }
        self.store_glossary_locked(next, ChangeOrigin::Local)?;
        Ok(())
    }

    /// 删除一条术语（按原文；去掉首尾空白后比较）。已经不在了（例如在其他设备上删掉了）视为成功
    pub fn glossary_remove(&self, source: &str) -> Result<(), IpcError> {
        let source = source.trim();
        let _g = self.glossary_lock.lock();
        let current = self.glossary_entries();
        if !current.iter().any(|e| e.source == source) {
            return Ok(());
        }
        let next = current.iter().filter(|e| e.source != source).cloned().collect();
        self.store_glossary_locked(next, ChangeOrigin::Local)?;
        Ok(())
    }

    /// 合并导入的条目（同 source 替换、新的追加），返回导入的条数。读取、合并与写入都在术语表的锁内：锁外读出的旧表会把
    /// 这期间别处（同步、热加载）加进来的术语整表覆盖掉，而这种覆盖又会作为本机的删除发到所有设备
    pub fn glossary_import(&self, imported: Vec<GlossaryEntry>) -> Result<u32, IpcError> {
        let count = u32::try_from(imported.len()).unwrap_or(u32::MAX);
        let _g = self.glossary_lock.lock();
        let merged = glossary_store::merge(&self.glossary_entries(), imported);
        crate::ipc::validate::validate_glossary(&merged)?;
        self.store_glossary_locked(merged, ChangeOrigin::Local)?;
        Ok(count)
    }

    /// 由厂商模板生成一个 id 不与现有 Provider 冲突的草稿（此时尚未保存，`provider_new` 命令）；模板不存在时 `not_found`。
    /// 钥匙串里还留着旧 Key 的已删除 Provider 的 id（`orphan_keys`）同样不复用，新 Provider 不会读到别人的旧 Key
    pub fn provider_new(&self, template_id: &str) -> Result<Provider, IpcError> {
        let settings = self.get();
        let orphans = self.state.read(|s| s.orphan_keys.clone());
        let existing: Vec<&str> = settings
            .providers
            .iter()
            .map(|p| p.id.as_str())
            .chain(orphans.iter().map(String::as_str))
            .collect();
        wisp_llm::provider_from_template(template_id, &existing).ok_or_else(|| IpcError::not_found("provider template"))
    }

    /// 删除 Provider（**阻塞**）：先删钥匙串条目，再从设置中移除，并清掉指向它的默认模型与风格绑定、能力缓存的
    /// 待写结论。钥匙串删除失败（拒绝授权、钥匙串锁定、Linux 取消解锁）**不阻止删除 Provider**——否则设置里会留下一个
    /// 永远删不掉的条目：此时把 id 记进 state.json 的 `orphan_keys`，之后的显式钥匙串操作（再删 Provider、保存 / 删除 Key）
    /// 时重试清理（[`Self::purge_orphan_keys`]），新建 Provider 也不复用这个 id。返回的第二项为 Key 是否已从钥匙串删除
    pub fn provider_delete(&self, provider_id: &str) -> Result<(Arc<Settings>, bool), IpcError> {
        self.purge_orphan_keys();
        let key_removed = {
            let _s = self.secret_lock.lock();
            let removed = match self.keychain.delete(provider_id) {
                Ok(()) => true,
                Err(e) => {
                    log::warn!(
                        "provider {provider_id}: the key could not be removed from the keychain ({}); \
                         the provider is deleted anyway and the key will be removed later",
                        e.message
                    );
                    self.note_orphan_key(provider_id, true);
                    false
                }
            };
            // 删除服务就是删除它的 Key：本机这次没删掉（留待之后清理）也照样报告，其他设备上的这个 Key 同样不再需要
            self.report_secret(provider_id, None);
            removed
        };
        self.caps.forget_provider(provider_id);
        let settings = self.update(&mut |s| {
            s.providers.retain(|p| p.id != provider_id);
            for slot in [&mut s.defaults.translate, &mut s.defaults.ocr] {
                if slot.as_ref().is_some_and(|m| m.provider_id == provider_id) {
                    *slot = None;
                }
            }
        })?;
        self.mutate_styles(|file, _| {
            for c in &mut file.custom {
                if c.model_ref.as_ref().is_some_and(|m| m.provider_id == provider_id) {
                    c.model_ref = None;
                }
            }
            for ov in file.overrides.values_mut() {
                if ov
                    .model_ref
                    .as_ref()
                    .is_some_and(|m| m.as_ref().is_some_and(|m| m.provider_id == provider_id))
                {
                    ov.model_ref = None;
                }
            }
            Ok(())
        })?;
        log::info!("provider deleted provider={provider_id} key_removed={key_removed}");
        Ok((settings, key_removed))
    }

    /// 按 id 保存一个已有的服务（设置页编辑服务，**阻塞**）：在写锁内对最新的设置只替换这一项，设置窗口里的旧快照不会把
    /// 其他服务（例如刚同步来的改动）写回旧值。该服务此刻已经不在了（在其他设备上或配置文件里删掉了）时返回
    /// `not_found`，不把它加回来
    pub fn provider_save(&self, provider: Provider) -> Result<Arc<Settings>, IpcError> {
        let mut found = false;
        let settings = self.update(&mut |s| {
            if let Some(slot) = s.providers.iter_mut().find(|p| p.id == provider.id) {
                slot.clone_from(&provider);
                found = true;
            }
        })?;
        if !found {
            return Err(IpcError::not_found("provider"));
        }
        Ok(settings)
    }

    /// 添加服务（设置页「添加服务」，**阻塞**）：追加到末尾，其他服务不动。同 id 的服务此时已经存在时替换它——模板生成的
    /// id 是确定的，另一台设备上添加的同一模板的服务可能刚同步过来，两者本来就会合并成同一个服务
    pub fn provider_add(&self, provider: Provider) -> Result<Arc<Settings>, IpcError> {
        self.update(&mut |s| match s.providers.iter_mut().find(|p| p.id == provider.id) {
            Some(slot) => slot.clone_from(&provider),
            None => s.providers.push(provider.clone()),
        })
    }

    /// 调整服务的顺序（**阻塞**）：只重排、不改内容；未知 id 忽略，列表里缺少的服务按原顺序排在后面
    pub fn providers_reorder(&self, ids: &[String]) -> Result<Arc<Settings>, IpcError> {
        self.update(&mut |s| {
            let mut rest = std::mem::take(&mut s.providers);
            let mut ordered = Vec::with_capacity(rest.len());
            for id in ids {
                if let Some(i) = rest.iter().position(|p| &p.id == id) {
                    ordered.push(rest.remove(i));
                }
            }
            ordered.append(&mut rest);
            s.providers = ordered;
        })
    }

    /// 加入或更新一个剪贴板排除应用（设置页逐个编辑，**阻塞**）：在写锁内对最新的设置按（系统，应用 id）只改这一项——
    /// 已在名单里就原位替换，否则追加。整组写入会让设置窗口里的旧名单把别处刚加进来的应用（例如另一台设备排除的密码
    /// 管理器）当作删除写回去。应用 id 去掉首尾空白后为空时 `invalid_argument`
    pub fn excluded_app_upsert(&self, mut entry: ExcludedApp) -> Result<Arc<Settings>, IpcError> {
        entry.app_id = entry.app_id.trim().to_owned();
        if entry.app_id.is_empty() {
            return Err(IpcError::invalid_argument("entry.app_id must not be empty"));
        }
        self.update(&mut |s| {
            let list = &mut s.clipboard.excluded_apps;
            match list.iter_mut().find(|a| a.os == entry.os && a.app_id == entry.app_id) {
                Some(slot) => slot.clone_from(&entry),
                None => list.push(entry.clone()),
            }
        })
    }

    /// 从剪贴板排除名单移除一个应用（按系统与应用 id，**阻塞**）。已经不在了（例如在其他设备上移除了）视为成功
    pub fn excluded_app_remove(&self, os: OsKind, app_id: &str) -> Result<Arc<Settings>, IpcError> {
        let app_id = app_id.trim();
        self.update(&mut |s| {
            s.clipboard
                .excluded_apps
                .retain(|a| !(a.os == os && a.app_id == app_id));
        })
    }

    /// 加入或更新一条应用的复制 / 粘贴键（按系统与应用 id 只改这一项，**阻塞**），理由同 [`Self::excluded_app_upsert`]
    pub fn app_keymap_upsert(&self, mut entry: AppKeymap) -> Result<Arc<Settings>, IpcError> {
        entry.app_id = entry.app_id.trim().to_owned();
        if entry.app_id.is_empty() {
            return Err(IpcError::invalid_argument("entry.app_id must not be empty"));
        }
        self.update(&mut |s| {
            let list = &mut s.hotkeys.app_keymaps;
            match list.iter_mut().find(|k| k.os == entry.os && k.app_id == entry.app_id) {
                Some(slot) => slot.clone_from(&entry),
                None => list.push(entry.clone()),
            }
        })
    }

    /// 移除一条应用的复制 / 粘贴键（按系统与应用 id，**阻塞**）。已经不在了视为成功
    pub fn app_keymap_remove(&self, os: OsKind, app_id: &str) -> Result<Arc<Settings>, IpcError> {
        let app_id = app_id.trim();
        self.update(&mut |s| {
            s.hotkeys.app_keymaps.retain(|k| !(k.os == os && k.app_id == app_id));
        })
    }

    /// 安装（`Some`）或卸载（`None`）Key 的钩子
    pub fn set_secret_hook(&self, hook: Option<SecretHook>) {
        let _s = self.secret_lock.lock();
        *self.secret_hook.write() = hook;
    }

    /// 调用 Key 的钩子（调用方持有 `secret_lock`）
    fn report_secret(&self, provider_id: &str, key: Option<&str>) {
        if let Some(hook) = self.secret_hook.read().clone() {
            hook(provider_id, key);
        }
    }

    /// 保存 Key（用户操作，**阻塞**）：写入钥匙串（首尾空白去掉）→ 调用 Key 的钩子 → 顺带清理之前删服务时没删掉的条目
    /// （用户刚授权访问钥匙串）
    pub fn secret_set(&self, provider_id: &str, key: &str) -> Result<(), IpcError> {
        let _s = self.secret_lock.lock();
        self.keychain.set(provider_id, key)?;
        self.report_secret(provider_id, Some(key.trim()));
        self.purge_orphan_keys();
        Ok(())
    }

    /// 删除 Key（用户操作，**阻塞**）：从钥匙串删除（不存在视为成功）→ 调用 Key 的钩子 → 清理遗留条目
    pub fn secret_delete(&self, provider_id: &str) -> Result<(), IpcError> {
        let _s = self.secret_lock.lock();
        self.keychain.delete(provider_id)?;
        self.report_secret(provider_id, None);
        self.purge_orphan_keys();
        Ok(())
    }

    /// 写入（`Some`）或删除（`None`）同步来的 Key（**阻塞**）：只改钥匙串，不调用钩子（远端的值不再发回去），
    /// 也不清理遗留条目（那是用户操作时才做的事）。
    ///
    /// `allow` 在拿到 Key 的写锁之后调用，返回 false 时什么都不写、返回 `Ok(false)`：用户保存 / 删除 Key 时在同一把锁内
    /// 报告改动，锁内的判断要么已经看到用户的改动（同步来的值让路），要么用户的改动排在这次写入之后（用户的值胜出）
    pub fn secret_apply_synced(
        &self,
        provider_id: &str,
        key: Option<&str>,
        allow: &dyn Fn() -> bool,
    ) -> Result<bool, IpcError> {
        let _s = self.secret_lock.lock();
        if !allow() {
            return Ok(false);
        }
        match key {
            Some(key) => self.keychain.set(provider_id, key),
            None => self.keychain.delete(provider_id),
        }
        .map(|()| true)
    }

    /// 同步凭据（钥匙串里单独的一项；方法都是阻塞的）
    pub fn sync_credentials(&self) -> &SyncCredentialStore {
        &self.sync_credentials
    }

    /// 记下 / 清除「Provider 已删除但钥匙串条目还在」的 id（写 state.json；失败只记日志）
    fn note_orphan_key(&self, provider_id: &str, orphan: bool) {
        let result = self.state.update(&mut |s| {
            if orphan {
                s.orphan_keys.insert(provider_id.to_owned());
            } else {
                s.orphan_keys.remove(provider_id);
            }
        });
        if let Err(e) = result {
            log::warn!("cannot record the orphaned key of {provider_id}: {}", e.message);
        }
    }

    /// 清理之前没删掉的钥匙串条目（**阻塞**，可能弹授权框——只在用户显式操作钥匙串时调用：删除 Provider、保存 / 删除
    /// Key）。已经有同 id 的 Provider（例如手改设置加回来）时不删、只移出清单；某一次删除失败就停止，不连续弹框。返回删掉的条数
    pub fn purge_orphan_keys(&self) -> usize {
        let orphans = self.state.read(|s| s.orphan_keys.clone());
        if orphans.is_empty() {
            return 0;
        }
        let settings = self.get();
        let mut removed = 0;
        for id in orphans {
            if settings.provider(&id).is_some() {
                self.note_orphan_key(&id, false);
                continue;
            }
            match self.keychain.delete(&id) {
                Ok(()) => {
                    log::info!("keychain: removed the orphaned key of the deleted provider {id}");
                    self.note_orphan_key(&id, false);
                    removed += 1;
                }
                Err(e) => {
                    log::warn!("keychain: orphaned key of {id} still cannot be removed: {}", e.message);
                    break;
                }
            }
        }
        if removed > 0 {
            // RuntimeInfo.orphan_keys 随之变化：设置窗口的「Key 暂未删除」提示要消失
            self.notify_changed();
        }
        removed
    }

    /// 被默认模型或风格引用的、属于该 Provider 的模型 id（拉取模型列表合并时必须保留）
    pub fn referenced_models(&self, provider_id: &str) -> Vec<String> {
        let settings = self.get();
        let mut out: Vec<String> = [&settings.defaults.translate, &settings.defaults.ocr]
            .into_iter()
            .flatten()
            .chain(self.styles().iter().filter_map(|s| s.model_ref.as_ref()))
            .filter(|m| m.provider_id == provider_id)
            .map(|m| m.model_id.clone())
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// 「测试连接」用的 Provider（默认翻译模型优先）：wisp-llm 用 `models` 里第一个已启用的模型做探针，
    /// 这里把默认翻译模型（属于这个 Provider 时）移到最前面并临时启用，见 [`put_default_model_first`]
    pub fn provider_for_probe(&self, mut provider: Provider) -> Provider {
        put_default_model_first(&mut provider, self.get().defaults.translate.as_ref());
        provider
    }

    /// 钥匙串索引可能漏记（state.json 在启动时不存在或已损坏、而设置里已有 Provider）时，在阻塞线程池里按 Provider 逐个
    /// 核对一次，修正 `has_key`。只做一次；macOS 上读取可能弹授权框，所以不在启动时做，而是等到需要显示 `has_key` 时
    /// （设置 / 引导窗口获焦）。没有可疑情况时什么也不做（非常便宜）
    pub fn verify_key_index(self: &Arc<Self>) {
        if !self.key_index_unverified.load(Ordering::Acquire) {
            return;
        }
        let svc = Arc::clone(self);
        tauri::async_runtime::spawn_blocking(move || {
            svc.rebuild_key_index_now();
        });
    }

    /// [`Self::verify_key_index`] 的同步实现（**阻塞**）；返回补回索引的条数
    pub fn rebuild_key_index_now(&self) -> usize {
        if !self.key_index_unverified.swap(false, Ordering::AcqRel) {
            return 0;
        }
        let ids: Vec<String> = self.get().providers.iter().map(|p| p.id.clone()).collect();
        let restored = self.keychain.rebuild_index(&ids);
        log::info!("keychain index verified providers={} restored={restored}", ids.len());
        restored
    }

    /// 引导完成（`onboarding_complete` 命令）：state.json 的 `onboarding_done = true`、`onboarding_version` = 当前版本
    pub fn complete_onboarding(&self) -> Result<(), IpcError> {
        self.state.update(&mut |s| {
            s.onboarding_done = true;
            s.onboarding_version = ONBOARDING_VERSION;
        })
    }

    /// 是否需要显示引导（从未完成，或引导版本已更新）
    pub fn needs_onboarding(&self) -> bool {
        self.state
            .read(|s| !s.onboarding_done || s.onboarding_version < ONBOARDING_VERSION)
    }

    /// state.json
    pub fn state(&self) -> &StateStore {
        &self.state
    }

    /// state.json（共享句柄，用于合并写入的用量统计等）
    pub fn state_arc(&self) -> Arc<StateStore> {
        Arc::clone(&self.state)
    }

    /// 钥匙串
    pub fn keychain(&self) -> &Keychain {
        &self.keychain
    }

    /// 能力缓存（写回 settings.json 的 models[].caps，合并去抖 1 s）
    pub fn caps_store(&self) -> Arc<dyn wisp_llm::CapsStore> {
        self.caps.clone()
    }

    /// 立即写掉能力缓存里待写的结论（定时器到期、退出时）
    pub fn caps_flush(&self) {
        self.caps.flush();
    }

    /// 清除某个模型学到的能力（`model_caps_reset`）
    pub fn caps_reset(&self, provider_id: &str, model_id: &str) -> Result<(), IpcError> {
        self.caps.reset(provider_id, model_id)
    }

    /// 退出前写掉全部待写内容（能力缓存、state.json）
    pub fn flush(&self) {
        self.caps.flush();
        if let Err(e) = self.state.flush() {
            log::warn!("state.json flush failed: {}", e.message);
        }
    }

    /// 配置文件最近一次读取失败的情况（`RuntimeInfo.settings_issue`）：启动时损坏 → `reset_to_default`（带备份路径）；
    /// 热加载遇到非法 JSON → `invalid_json`（保留旧值）；文件恢复合法并成功加载后清除；文件来自更新的版本 →
    /// `newer_version`（改回当前版本后清除）。变化时随 `settings:changed` 广播。
    /// 多个文件同时有问题时按 `reset_to_default` > `invalid_json` > `newer_version`，同级按 settings > styles > glossary
    pub fn file_issue(&self) -> Option<SettingsFileIssue> {
        let issues = self.issues.lock();
        [
            SettingsIssueKind::ResetToDefault,
            SettingsIssueKind::InvalidJson,
            SettingsIssueKind::NewerVersion,
        ]
        .into_iter()
        .find_map(|kind| issues.values().find(|i| i.kind == kind))
        .cloned()
    }

    /// 已删除的 Provider 中、Key 还没能从钥匙串删掉的个数（`RuntimeInfo.orphan_keys`）
    pub fn orphan_key_count(&self) -> u32 {
        u32::try_from(self.state.read(|s| s.orphan_keys.len())).unwrap_or(u32::MAX)
    }

    /// 设置快照
    pub fn snapshot(&self, runtime: RuntimeInfo) -> SettingsSnapshot {
        SettingsSnapshot {
            settings: (*self.get()).clone(),
            styles: (*self.styles()).clone(),
            runtime,
        }
    }
}

/// 启动时读取 settings.json 的结果
struct LoadedSettings {
    settings: Settings,
    issue: Option<SettingsFileIssue>,
    /// 需要写回（首次运行、迁移过、损坏已备份）
    rewrite: bool,
    /// 读到的原始内容的哈希（热加载据此识别「没变」）
    hash: Option<fsio::ContentHash>,
    /// 来自更新的版本：写回时要保留的内容
    newer: Option<migrate::Newer>,
    /// 读取情况（文件不存在 → Created；损坏或读不了 → ResetToDefault）
    state: LoadState,
    /// 宽松解析或校验丢掉 / 退回了文件里的内容（见 [`LoadState::Lossy`]）
    lossy: bool,
}

/// 本平台的新配置（首次运行、损坏重置、读不了文件时）：`Settings::default()` 加平台默认值（
/// `wisp_types::defaults::unload_panel_default`）。已有文件里的值一律按文件，不受影响
fn fresh_settings() -> Settings {
    Settings::for_os(wisp_types::OsKind::current())
}

/// 启动时读取 settings.json
fn load_settings(path: &std::path::Path) -> LoadedSettings {
    let fallback = |rewrite: bool, state: LoadState| LoadedSettings {
        settings: fresh_settings(),
        issue: None,
        rewrite,
        hash: None,
        newer: None,
        state,
        lossy: false,
    };
    let bytes = match fsio::read_optional(path) {
        Ok(Some(b)) => b,
        // 首次运行（或文件被删掉后启动）：写出默认内容
        Ok(None) => return fallback(true, LoadState::Created),
        Err(e) => {
            // 读不了（权限等）：不覆盖用户的文件，本次用默认值
            log::error!("cannot read settings.json ({e}); using defaults without overwriting it");
            return fallback(false, LoadState::ResetToDefault);
        }
    };
    let hash = Some(fsio::content_hash(&bytes));
    let parsed = serde_json::from_slice::<serde_json::Value>(fsio::strip_bom(&bytes))
        .ok()
        .filter(serde_json::Value::is_object)
        .map(|v| migrate::run(v, SETTINGS_SCHEMA_VERSION, migrate::SETTINGS_STEPS))
        .and_then(|(v, outcome)| {
            let raw = newer_raw(outcome, &v);
            wisp_types::from_value_lenient::<Settings>(v)
                .ok()
                .map(|(s, issues)| (s, issues, outcome, raw))
        });
    match parsed {
        Some((mut settings, issues, outcome, raw)) => {
            for issue in &issues {
                log::warn!("settings.json: {issue}");
            }
            let fixes = validate::check(&mut settings);
            for fix in &fixes {
                log::warn!("settings.json: {fix}");
            }
            let lossy = !issues.is_empty() || fixes.iter().any(validate::Fix::is_lossy);
            let migrated = matches!(outcome, migrate::Outcome::Migrated { .. });
            let newer = serde_json::to_value(&settings)
                .ok()
                .and_then(|ours| migrate::Newer::from_outcome(outcome, raw.as_ref(), &ours));
            if let Some(n) = &newer {
                log::warn!(
                    "settings.json was written by a newer Wisp (schema_version={}); settings this version does not \
                     know are kept as they are when it is saved",
                    n.found
                );
            }
            LoadedSettings {
                settings,
                issue: None,
                rewrite: migrated,
                hash,
                newer,
                state: LoadState::Normal,
                lossy,
            }
        }
        None => {
            let backup = match fsio::backup_corrupt(path) {
                Ok(p) => Some(p.to_string_lossy().into_owned()),
                Err(e) => {
                    log::error!("cannot back up the corrupt settings.json: {e}");
                    None
                }
            };
            log::warn!("settings.json is not a valid JSON object; reset to defaults (backup kept)");
            let issue = SettingsFileIssue {
                kind: SettingsIssueKind::ResetToDefault,
                file: ConfigFile::Settings.file_name().to_owned(),
                backup_path: backup.clone(),
                at_ms: fsio::unix_now_ms(),
            };
            // 备份失败时不覆盖原文件（保住用户的数据）
            LoadedSettings {
                settings: fresh_settings(),
                issue: Some(issue),
                rewrite: backup.is_some(),
                hash,
                newer: None,
                state: LoadState::ResetToDefault,
                lossy: false,
            }
        }
    }
}

/// styles.json / glossary.json 启动时的读取情况
fn loaded_state<T>(loaded: &fsio::Loaded<T>) -> LoadState {
    if !loaded.existed {
        LoadState::Created
    } else if loaded.backup.is_some() {
        LoadState::ResetToDefault
    } else {
        LoadState::Normal
    }
}

/// 规范化文档：序列化成 JSON 文本再解析回来（见 [`ConfigChange`]）。自己的类型序列化不会失败；万一失败记日志并返回
/// `Null`，钩子把它当作「内容不可用」
fn canonical<T: serde::Serialize>(value: &T) -> serde_json::Value {
    match serde_json::to_vec(value).and_then(|bytes| serde_json::from_slice(&bytes)) {
        Ok(v) => v,
        Err(e) => {
            log::error!("cannot build the canonical document: {e}");
            serde_json::Value::Null
        }
    }
}

/// glossary.json 的规范化文档（与写盘的内容同一形式）
fn glossary_doc(entries: &[GlossaryEntry]) -> serde_json::Value {
    #[derive(serde::Serialize)]
    struct Doc<'a> {
        schema_version: u32,
        entries: &'a [GlossaryEntry],
    }
    canonical(&Doc {
        schema_version: migrate::DOC_SCHEMA_VERSION,
        entries,
    })
}

/// 只属于本机的设置：应用同步来的文档时一律保留本机的值——开机自启、开发者模式、日志与性能选项、同步自身的配置、
/// 剪贴板记录开关、疑似密钥检测、识别历史开关，以及文档的版本号
fn keep_local_settings(next: &mut Settings, current: &Settings) {
    next.schema_version = current.schema_version;
    next.general.launch_at_login = current.general.launch_at_login;
    next.general.developer_mode = current.general.developer_mode;
    next.advanced.clone_from(&current.advanced);
    next.sync.clone_from(&current.sync);
    next.clipboard.enabled = current.clipboard.enabled;
    next.clipboard.secret_detection = current.clipboard.secret_detection;
    next.ocr.history = current.ocr.history;
    // 离线模型的选择只属于本机（隐私：同步不能把这台设备从离线改回云端）
    next.offline = current.offline;
}

/// 「文件来自更新的版本」的提示（`SettingsIssueKind::NewerVersion`）
fn newer_issue(file: ConfigFile) -> SettingsFileIssue {
    SettingsFileIssue {
        kind: SettingsIssueKind::NewerVersion,
        file: file.file_name().to_owned(),
        backup_path: None,
        at_ms: fsio::unix_now_ms(),
    }
}

/// 来自更新版本的文档在宽松解析之前的原文（只在这种少见情况下复制）
fn newer_raw(outcome: migrate::Outcome, value: &serde_json::Value) -> Option<serde_json::Value> {
    matches!(outcome, migrate::Outcome::Newer { .. }).then(|| value.clone())
}

/// 文件来自更新的版本：把本版本序列化出的内容与原文里不认识的部分合并（保留原来的 `schema_version`）
fn preserve_newer(bytes: &[u8], newer: &migrate::Newer) -> Result<Vec<u8>, IpcError> {
    let mut value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| IpcError::internal(format!("cannot re-read own JSON: {e}")))?;
    newer.restore(&mut value);
    Ok(fsio::to_pretty_json(&value)?)
}

/// 首次运行（或文件被删掉后启动）时写出默认的 styles.json / glossary.json，并记为已知内容（失败只记日志）
fn write_default(
    file: ConfigFile,
    path: &std::path::Path,
    bytes: Result<Vec<u8>, IpcError>,
    known: &mut HashMap<ConfigFile, fsio::ContentHash>,
) {
    let result = bytes.and_then(|b| {
        fsio::write_atomic(path, &b)?;
        Ok(b)
    });
    match result {
        Ok(b) => {
            known.insert(file, fsio::content_hash(&b));
        }
        Err(e) => log::warn!("cannot write the default {}: {}", file.file_name(), e.message),
    }
}

/// 记录 styles / glossary 读取结果里的问题，并把读到的内容记为「已知」（热加载据此识别没变的文件）
fn note_loaded<T>(
    file: ConfigFile,
    loaded: &fsio::Loaded<T>,
    issues: &mut BTreeMap<ConfigFile, SettingsFileIssue>,
    known: &mut HashMap<ConfigFile, fsio::ContentHash>,
    newer: &mut HashMap<ConfigFile, migrate::Newer>,
) {
    if let Some(hash) = loaded.hash {
        known.insert(file, hash);
    }
    if let Some(n) = &loaded.newer {
        log::warn!(
            "{} was written by a newer Wisp (schema_version={}); content this version does not know is kept \
             when it is saved",
            file.file_name(),
            n.found
        );
        newer.insert(file, n.clone());
        issues.insert(file, newer_issue(file));
    }
    for issue in &loaded.issues {
        log::warn!("{}: {issue}", file.file_name());
    }
    if let Some(backup) = &loaded.backup {
        log::warn!(
            "{} is not a valid JSON object; reset to defaults (backup kept)",
            file.file_name()
        );
        issues.insert(
            file,
            SettingsFileIssue {
                kind: SettingsIssueKind::ResetToDefault,
                file: file.file_name().to_owned(),
                backup_path: Some(backup.to_string_lossy().into_owned()),
                at_ms: fsio::unix_now_ms(),
            },
        );
    }
}

/// 把默认翻译模型移到 `models` 首位并启用（纯函数）：只在 `default` 属于这个 Provider、且模型在列表里时生效——
/// 草稿里已经删掉的模型不加回来（用户测的是正在编辑的版本）
pub fn put_default_model_first(provider: &mut Provider, default: Option<&ModelRef>) {
    let Some(default) = default.filter(|m| m.provider_id == provider.id) else {
        return;
    };
    let Some(idx) = provider.models.iter().position(|m| m.id == default.model_id) else {
        return;
    };
    let mut entry = provider.models.remove(idx);
    entry.enabled = true;
    provider.models.insert(0, entry);
}

/// 生成不与现有风格冲突的 `custom-<uuid 前 8 位>`
fn unique_custom_id(existing: &[Style]) -> String {
    loop {
        let uuid = uuid::Uuid::new_v4().simple().to_string();
        let id = format!("{}{}", wisp_types::defaults::CUSTOM_STYLE_PREFIX, &uuid[..8]);
        if !existing.iter().any(|s| s.id == id) {
            return id;
        }
    }
}

/// 在阻塞线程池里执行（钥匙串访问可能弹出系统授权框，不能占住 tokio worker）
pub async fn run_blocking<T, F>(f: F) -> Result<T, IpcError>
where
    F: FnOnce() -> Result<T, IpcError> + Send + 'static,
    T: Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| IpcError::internal(format!("blocking task failed: {e}")))?
}

/// 在阻塞线程池里读取 Key（`provider_test` / 编排器用）
pub async fn key_get_blocking(
    settings: Arc<SettingsService>,
    provider_id: String,
) -> Result<Option<wisp_llm::Secret>, IpcError> {
    run_blocking(move || settings.keychain().get(&provider_id)).await
}

/// 测试连接 / 拉取模型用的 Key。返回 (Key, 是否读不出凭据存储)：
/// - Provider 不需要 Key（本地模型等，`plan::provider_needs_key`）且索引里没有存 Key：不访问钥匙串，以 None 继续；
/// - 读取失败时，Provider 不需要 Key、或索引里本来就没有它的 Key（用户没填）：以 None 继续探测，并带回提示——
///   服务端真要鉴权会回 401，比一句「凭据存储不可用」更能说明问题；
/// - 只有确实需要、且存过 Key 却读不出来时才整体失败（`keychain` 错误）
pub async fn key_for_probe(
    settings: Arc<SettingsService>,
    provider: &Provider,
) -> Result<(Option<wisp_llm::Secret>, bool), IpcError> {
    let optional = !crate::orchestrator::plan::provider_needs_key(provider);
    let stored = settings.keychain().has(&provider.id);
    if optional && !stored {
        return Ok((None, false));
    }
    match key_get_blocking(Arc::clone(&settings), provider.id.clone()).await {
        Ok(key) => Ok((key, false)),
        Err(e) if optional || !stored => {
            log::warn!(
                "provider {}: reading the key failed ({}); probing without a key",
                provider.id,
                e.message
            );
            Ok((None, true))
        }
        Err(e) => Err(e),
    }
}

/// 测试连接报告里「凭据存储读不出来」的提示（i18n key）
pub const TEST_NOTICE_KEYCHAIN_UNREADABLE: &str = "settings.providers.test_notice.keychain_unreadable";

/// 测试连接 / 拉取模型用的 Key，含草稿 Key：
/// - `draft = Some(k)`：设置页草稿里刚输入、还没保存的 Key，只用于这一次请求，**不读也不写**钥匙串；与保存时一样去掉
///   首尾空白，为空表示用户把 Key 留空，以 None 探测（服务端要鉴权会回 401）；
/// - `draft = None`：已保存的服务，按 [`key_for_probe`] 从钥匙串读取
pub async fn probe_key(
    settings: Arc<SettingsService>,
    provider: &Provider,
    draft: Option<String>,
) -> Result<(Option<wisp_llm::Secret>, bool), IpcError> {
    match draft.as_deref().map(str::trim) {
        Some("") => Ok((None, false)),
        // 与保存时（`Keychain::set` 去掉首尾空白）同一口径：从网页复制的 Key 常带换行，测试通过的 Key 保存后照样可用
        Some(k) => Ok((Some(wisp_llm::Secret::new(k.to_owned())), false)),
        None => key_for_probe(settings, provider).await,
    }
}

/// 拉取模型列表并与 `provider.models` 合并（`provider_models`）：Key 从钥匙串按 `provider.id` 读取（[`key_for_probe`]）；
/// 被默认模型或风格引用的模型一定保留；结果不落盘（由前端经 `settings_patch` 保存）
pub async fn provider_models(
    settings: Arc<SettingsService>,
    llm: &wisp_llm::LlmClient,
    provider: Provider,
    draft_key: Option<String>,
) -> Result<Vec<ModelEntry>, IpcError> {
    let (key, _) = probe_key(Arc::clone(&settings), &provider, draft_key).await?;
    let remote = llm.list_models(&provider, key.as_ref()).await?;
    let referenced = settings.referenced_models(&provider.id);
    let referenced: Vec<&str> = referenced.iter().map(String::as_str).collect();
    Ok(wisp_llm::merge_models(&provider.models, remote, &referenced))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;
    use wisp_types::{ModelRef, Protocol, Quirks, ThemePref, Timeouts};

    use super::*;
    use crate::error::IpcErrorCode;

    pub fn test_paths(root: &std::path::Path) -> Paths {
        Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
        }
    }

    pub fn test_service() -> (tempfile::TempDir, Arc<SettingsService>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let svc = SettingsService::load_for_tests(&test_paths(dir.path()));
        (dir, svc)
    }

    pub fn provider(id: &str, models: &[&str]) -> Provider {
        Provider {
            id: id.to_owned(),
            name: String::new(),
            protocol: Protocol::OpenAiChat,
            base_url: "https://api.example.com/v1".to_owned(),
            headers: BTreeMap::new(),
            quirks: Quirks::default(),
            models: models.iter().map(|m| ModelEntry::new(*m)).collect(),
            timeouts: Timeouts::default(),
            template_id: None,
        }
    }

    fn read_json(path: &std::path::Path) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(path).expect("read")).expect("json")
    }

    /// 草稿 Key 只用于这一次请求，**不读也不写**钥匙串——即使钥匙串拒绝访问、
    /// 即使已经存过别的 Key；留空（只有空白）= 以无 Key 探测；没有草稿时照旧从钥匙串读已保存的 Key
    #[tokio::test]
    async fn probe_key_uses_the_draft_without_touching_the_keychain() {
        use std::sync::atomic::Ordering;

        use crate::settings::keychain::tests::MemoryBackend;
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = Arc::new(MemoryBackend::default());
        let b = Arc::clone(&backend);
        let svc = SettingsService::load_with(
            &test_paths(dir.path()),
            Some(Box::new(move || {
                Ok(Box::new(Arc::clone(&b)) as Box<dyn crate::settings::keychain::SecretBackend>)
            })),
        );
        let remote = provider("remote", &[]);
        svc.keychain().set("remote", "sk-saved").expect("set");
        // 钥匙串此刻拒绝一切访问：草稿路径若碰了它，就会失败或改变计数
        backend.deny.store(true, Ordering::SeqCst);
        let before = backend.calls.load(Ordering::SeqCst);
        let (key, unreadable) = probe_key(Arc::clone(&svc), &remote, Some("  sk-draft \n".to_owned()))
            .await
            .expect("draft");
        assert_eq!(
            key.as_ref().map(|k| k.expose()),
            Some("sk-draft"),
            "与保存时一样去掉首尾空白"
        );
        assert!(!unreadable);
        let (key, unreadable) = probe_key(Arc::clone(&svc), &remote, Some(" \t ".to_owned()))
            .await
            .expect("blank draft");
        assert!(key.is_none() && !unreadable, "留空：以无 Key 探测，不退回已保存的 Key");
        assert_eq!(backend.calls.load(Ordering::SeqCst), before, "草稿路径不读也不写钥匙串");
        assert_eq!(
            backend.items.lock().get("remote").map(String::as_str),
            Some("sk-saved"),
            "已保存的 Key 不变"
        );
        // 没有草稿：照旧经钥匙串读取（这里被拒绝 → 需要 Key 的 Provider 整体失败）
        let err = probe_key(Arc::clone(&svc), &remote, None)
            .await
            .expect_err("keychain denied");
        assert_eq!(err.code, IpcErrorCode::Keychain);
        assert!(backend.calls.load(Ordering::SeqCst) > before);
        backend.deny.store(false, Ordering::SeqCst);
        let (key, _) = probe_key(Arc::clone(&svc), &remote, None).await.expect("saved");
        assert_eq!(key.as_ref().map(|k| k.expose()), Some("sk-saved"));
    }

    /// 测试连接读不出 Key 时——不需要 Key 的 Provider、没存过 Key 的 Provider 都不带 Key 继续测；只有存过 Key、
    /// 又确实需要 Key 时才整体失败
    #[tokio::test]
    async fn probe_key_falls_back_to_none_when_the_keychain_fails() {
        use std::sync::atomic::Ordering;

        use crate::settings::keychain::tests::MemoryBackend;
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = Arc::new(MemoryBackend::default());
        let b = Arc::clone(&backend);
        let svc = SettingsService::load_with(
            &test_paths(dir.path()),
            Some(Box::new(move || {
                Ok(Box::new(Arc::clone(&b)) as Box<dyn crate::settings::keychain::SecretBackend>)
            })),
        );
        let mut local = provider("local", &[]);
        local.base_url = "http://127.0.0.1:11434/v1".to_owned();
        let remote = provider("remote", &[]);
        backend.deny.store(true, Ordering::SeqCst);
        let before = backend.calls.load(Ordering::SeqCst);
        let (key, unreadable) = key_for_probe(Arc::clone(&svc), &local).await.expect("local");
        assert!(key.is_none() && !unreadable);
        assert_eq!(
            backend.calls.load(Ordering::SeqCst),
            before,
            "不需要 Key 也没存：不访问钥匙串"
        );
        let (key, unreadable) = key_for_probe(Arc::clone(&svc), &remote)
            .await
            .expect("remote, nothing stored");
        assert!(key.is_none() && unreadable);
        // 存过 Key 之后读不出来：需要 Key 的整体失败，不需要的照常继续
        backend.deny.store(false, Ordering::SeqCst);
        svc.keychain().set("remote", "sk-remote").expect("set");
        svc.keychain().set("local", "sk-local").expect("set");
        backend.deny.store(true, Ordering::SeqCst);
        let err = key_for_probe(Arc::clone(&svc), &remote).await.expect_err("needs key");
        assert_eq!(err.code, IpcErrorCode::Keychain);
        let (key, unreadable) = key_for_probe(Arc::clone(&svc), &local).await.expect("local stored");
        assert!(key.is_none() && unreadable);
        backend.deny.store(false, Ordering::SeqCst);
        let (key, unreadable) = key_for_probe(Arc::clone(&svc), &remote).await.expect("remote ok");
        assert_eq!(key.map(|k| k.expose().to_owned()).as_deref(), Some("sk-remote"));
        assert!(!unreadable);
    }

    #[test]
    fn first_run_writes_default_settings() {
        let (_dir, svc) = test_service();
        let on_disk = read_json(&svc.paths().settings_file());
        assert_eq!(on_disk["schema_version"], wisp_types::SETTINGS_SCHEMA_VERSION);
        assert_eq!(on_disk["general"]["update_policy"], "notify");
        // f32 字段写盘为 0.6（直接从强类型序列化，不经 Value 中转）
        let text = std::fs::read_to_string(svc.paths().settings_file()).expect("read");
        assert!(text.contains("\"max_height_ratio\": 0.6"), "{text}");
        assert!(svc.file_issue().is_none());
        assert_eq!(*svc.get(), fresh_settings());
    }

    #[test]
    fn first_run_writes_all_three_config_documents() {
        let (_dir, svc) = test_service();
        for file in ConfigFile::ALL {
            assert!(file.path(svc.paths()).exists(), "{}", file.file_name());
        }
        let styles = read_json(&svc.paths().styles_file());
        assert_eq!(styles["schema_version"], 1);
        assert_eq!(
            read_json(&svc.paths().glossary_file())["entries"],
            serde_json::json!([])
        );
        // 写出的默认内容记为已知：热加载不会把它当成外部修改
        assert_eq!(svc.reload(ConfigFile::Styles), ReloadOutcome::Unchanged);
        assert_eq!(svc.reload(ConfigFile::Glossary), ReloadOutcome::Unchanged);
    }

    /// 降级：更新的版本写的 settings.json 被本版本读取后，任何写盘（这里是一次 patch 与一次能力缓存式的 update）都保留
    /// 新版本的字段与版本号；本版本的修改照常生效
    #[test]
    fn settings_from_a_newer_version_survive_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(dir.path());
        std::fs::create_dir_all(&paths.config_dir).expect("dir");
        let mut newer = serde_json::to_value(Settings::default()).expect("json");
        newer["schema_version"] = serde_json::json!(SETTINGS_SCHEMA_VERSION + 1);
        newer["general"]["future_switch"] = serde_json::json!(true);
        newer["future_section"] = serde_json::json!({"enabled": true});
        let mut p = serde_json::to_value(provider("p", &["m"])).expect("provider");
        p["future_auth"] = serde_json::json!("oauth");
        newer["providers"] = serde_json::json!([p]);
        std::fs::write(paths.settings_file(), serde_json::to_vec_pretty(&newer).expect("json")).expect("write");

        let svc = SettingsService::load_for_tests(&paths);
        assert!(svc.get().provider("p").is_some(), "本版本认识的部分照常读取");
        // 设置窗口据此提示「配置来自更新的版本」
        let issue = svc.file_issue().expect("newer version issue");
        assert_eq!(issue.kind, SettingsIssueKind::NewerVersion);
        assert_eq!(issue.file, "settings.json");
        svc.patch(serde_json::json!({"appearance": {"font_size": 16}}))
            .expect("patch");
        svc.update(&mut |s| s.providers[0].name = "Renamed".to_owned())
            .expect("update");

        let on_disk = read_json(&paths.settings_file());
        assert_eq!(on_disk["schema_version"], SETTINGS_SCHEMA_VERSION + 1, "版本号不回落");
        assert_eq!(on_disk["general"]["future_switch"], true);
        assert_eq!(on_disk["future_section"]["enabled"], true);
        assert_eq!(on_disk["providers"][0]["future_auth"], "oauth");
        assert_eq!(on_disk["providers"][0]["name"], "Renamed");
        assert_eq!(on_disk["appearance"]["font_size"], 16);
        // 自己写入的内容（合并后的）记为已知，热加载不会把它当成外部修改
        assert_eq!(svc.reload(ConfigFile::Settings), ReloadOutcome::Unchanged);
        // 文件被改回当前版本后不再合并
        let mut current = read_json(&paths.settings_file());
        current["schema_version"] = serde_json::json!(SETTINGS_SCHEMA_VERSION);
        current["appearance"]["font_size"] = serde_json::json!(15);
        std::fs::write(
            paths.settings_file(),
            serde_json::to_vec_pretty(&current).expect("json"),
        )
        .expect("write");
        assert_eq!(svc.reload(ConfigFile::Settings), ReloadOutcome::Applied);
        assert!(svc.file_issue().is_none(), "改回当前版本后提示消失");
        svc.patch(serde_json::json!({"appearance": {"font_size": 14}}))
            .expect("patch");
        let on_disk = read_json(&paths.settings_file());
        assert_eq!(on_disk["schema_version"], SETTINGS_SCHEMA_VERSION);
        assert!(on_disk.get("future_section").is_none());
    }

    #[test]
    fn patch_merges_writes_and_bumps_revision() {
        let (_dir, svc) = test_service();
        let mut rx = svc.changes();
        rx.borrow_and_update();
        let next = svc
            .patch(json!({"appearance": {"theme": "dark", "font_size": 16}}))
            .expect("patch");
        assert_eq!(next.appearance.theme, ThemePref::Dark);
        assert_eq!(next.appearance.font_size, 16);
        assert!(rx.has_changed().expect("open"));
        let on_disk = read_json(&svc.paths().settings_file());
        assert_eq!(on_disk["appearance"]["theme"], "dark");
        // 其他字段保持不变
        assert_eq!(on_disk["appearance"]["max_height_ratio"], json!(0.6));
    }

    #[test]
    fn patch_with_no_change_does_not_notify() {
        let (_dir, svc) = test_service();
        let mut rx = svc.changes();
        rx.borrow_and_update();
        svc.patch(json!({"appearance": {"theme": "system"}})).expect("patch");
        assert!(!rx.has_changed().expect("open"));
    }

    #[test]
    fn patch_rejects_invalid_values_without_writing() {
        let (_dir, svc) = test_service();
        let before = std::fs::read(svc.paths().settings_file()).expect("read");
        for bad in [
            json!({"appearance": {"theme": "Dark"}}),
            json!({"general": {"launch_at_login": "yes"}}),
            json!({"appearance": {"accent": "#zzz"}}),
            json!({"hotkeys": {"bindings": {"ocr": "Alt+Nope"}}}),
            json!({"providers": [{"id": "x"}]}),
            // 合并为一行只能在结果面板里按需切换，不能设为默认排版
            json!({"ocr": {"layout": "join_lines"}}),
        ] {
            let err = svc.patch(bad.clone()).expect_err("invalid");
            assert_eq!(err.code, IpcErrorCode::InvalidArgument, "{bad}");
        }
        assert_eq!(std::fs::read(svc.paths().settings_file()).expect("read"), before);
        assert_eq!(*svc.get(), fresh_settings());
    }

    #[test]
    fn patch_clamps_out_of_range_numbers() {
        let (_dir, svc) = test_service();
        let next = svc.patch(json!({"appearance": {"font_size": 99}})).expect("patch");
        assert_eq!(next.appearance.font_size, 18);
    }

    #[test]
    fn patch_null_resets_to_default_and_arrays_replace() {
        let (_dir, svc) = test_service();
        svc.patch(json!({"translate": {"long_text_threshold": 5000}}))
            .expect("set");
        let next = svc
            .patch(json!({"translate": {"long_text_threshold": null}}))
            .expect("reset");
        assert_eq!(next.translate.long_text_threshold, 3000);
        let next = svc.patch(json!({"clipboard": {"excluded_apps": []}})).expect("arrays");
        assert!(next.clipboard.excluded_apps.is_empty());
    }

    #[test]
    fn corrupt_settings_are_backed_up_and_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(dir.path());
        std::fs::create_dir_all(&paths.config_dir).expect("mkdir");
        std::fs::write(paths.settings_file(), b"{ \"general\": ").expect("write");
        let svc = SettingsService::load_for_tests(&paths);
        assert_eq!(*svc.get(), fresh_settings());
        let issue = svc.file_issue().expect("issue");
        assert_eq!(issue.kind, SettingsIssueKind::ResetToDefault);
        assert_eq!(issue.file, "settings.json");
        let backup = issue.backup_path.expect("backup");
        assert_eq!(std::fs::read(&backup).expect("backup"), b"{ \"general\": ");
        // 默认值已写回，文件现在是合法 JSON
        assert_eq!(
            read_json(&paths.settings_file())["schema_version"],
            wisp_types::SETTINGS_SCHEMA_VERSION
        );
    }

    #[test]
    fn settings_with_utf8_bom_are_loaded_not_reset() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(dir.path());
        std::fs::create_dir_all(&paths.config_dir).expect("mkdir");
        let mut text = b"\xEF\xBB\xBF".to_vec();
        text.extend_from_slice(br#"{"schema_version": 1, "appearance": {"font_size": 16}}"#);
        std::fs::write(paths.settings_file(), &text).expect("write");
        let svc = SettingsService::load_for_tests(&paths);
        assert_eq!(svc.get().appearance.font_size, 16, "保留用户的设置");
        assert!(svc.file_issue().is_none(), "不报告损坏");
        assert!(
            !std::fs::read_dir(&paths.config_dir).expect("read_dir").any(|e| e
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .contains(".bak-")),
            "不产生损坏备份"
        );
    }

    #[test]
    fn hot_reload_accepts_external_edits_saved_with_utf8_bom() {
        let (_dir, svc) = test_service();
        let path = svc.paths().settings_file();
        let mut v = read_json(&path);
        v["appearance"]["font_size"] = json!(18);
        let mut text = b"\xEF\xBB\xBF".to_vec();
        text.extend_from_slice(&serde_json::to_vec_pretty(&v).expect("json"));
        std::fs::write(&path, &text).expect("write");
        assert_eq!(svc.reload(ConfigFile::Settings), ReloadOutcome::Applied);
        assert_eq!(svc.get().appearance.font_size, 18);
        assert!(svc.file_issue().is_none());
    }

    #[test]
    fn lenient_load_keeps_valid_fields_and_does_not_rewrite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(dir.path());
        std::fs::create_dir_all(&paths.config_dir).expect("mkdir");
        // 当前版本的文件（旧版本的文件会在迁移后写回一次，那是另一回事）
        let text = format!(
            r#"{{"schema_version": {}, "appearance": {{"theme": "Dark", "font_size": 16}}, "general": {{"start_hidden": false}}}}"#,
            wisp_types::SETTINGS_SCHEMA_VERSION
        );
        std::fs::write(paths.settings_file(), &text).expect("write");
        let svc = SettingsService::load_for_tests(&paths);
        let s = svc.get();
        assert_eq!(s.appearance.theme, ThemePref::System, "非法枚举只退回这一项");
        assert_eq!(s.appearance.font_size, 16);
        assert!(!s.general.start_hidden);
        assert!(svc.file_issue().is_none(), "字段级问题只记日志");
        assert_eq!(
            std::fs::read_to_string(paths.settings_file()).expect("read"),
            text,
            "不回写"
        );
    }

    #[test]
    fn hot_reload_ignores_own_writes_and_applies_external_edits() {
        let (_dir, svc) = test_service();
        svc.patch(json!({"appearance": {"font_size": 15}})).expect("patch");
        assert_eq!(
            svc.reload(ConfigFile::Settings),
            ReloadOutcome::Unchanged,
            "自己写入的内容被忽略"
        );
        let path = svc.paths().settings_file();
        let mut v = read_json(&path);
        v["appearance"]["font_size"] = json!(17);
        std::fs::write(&path, serde_json::to_vec_pretty(&v).expect("json")).expect("write");
        let mut rx = svc.changes();
        rx.borrow_and_update();
        assert_eq!(svc.reload(ConfigFile::Settings), ReloadOutcome::Applied);
        assert_eq!(svc.get().appearance.font_size, 17);
        assert!(rx.has_changed().expect("open"));
    }

    #[test]
    fn hot_reload_waits_for_an_in_flight_write() {
        let (_dir, svc) = test_service();
        let path = svc.paths().settings_file();
        // 外部改写（X：font_size 17）
        let mut v = read_json(&path);
        v["appearance"]["font_size"] = json!(17);
        std::fs::write(&path, serde_json::to_vec_pretty(&v).expect("json")).expect("write");
        // 一次写入正在进行（持有 write_lock）时触发热加载
        let guard = svc.write_lock.lock();
        let reloader = {
            let svc = Arc::clone(&svc);
            std::thread::spawn(move || svc.reload(ConfigFile::Settings))
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        // 这次写入基于旧内存（Y：font_size 15）
        let mut next = (*svc.get()).clone();
        next.appearance.font_size = 15;
        svc.commit(next).expect("commit");
        drop(guard);
        // 热加载在写入完成之后才读文件：读到的是自己写的 Y，内存与磁盘一致，这次写入没有丢
        assert_eq!(reloader.join().expect("join"), ReloadOutcome::Unchanged);
        assert_eq!(svc.get().appearance.font_size, 15);
        assert_eq!(read_json(&path)["appearance"]["font_size"], 15);
    }

    #[test]
    fn external_revert_to_known_content_is_applied() {
        let (_dir, svc) = test_service();
        svc.patch(json!({"appearance": {"font_size": 15}})).expect("patch");
        let path = svc.paths().settings_file();
        let ours = std::fs::read(&path).expect("read");
        let mut v = read_json(&path);
        v["appearance"]["font_size"] = json!(17);
        std::fs::write(&path, serde_json::to_vec_pretty(&v).expect("json")).expect("write");
        assert_eq!(svc.reload(ConfigFile::Settings), ReloadOutcome::Applied);
        assert_eq!(
            svc.reload(ConfigFile::Settings),
            ReloadOutcome::Unchanged,
            "同一份外部内容只处理一次"
        );
        // 编辑器「撤销」回到 Wisp 写过的内容：仍是一次外部修改
        std::fs::write(&path, &ours).expect("revert");
        assert_eq!(svc.reload(ConfigFile::Settings), ReloadOutcome::Applied);
        assert_eq!(svc.get().appearance.font_size, 15);
    }

    #[test]
    fn unchanged_files_from_startup_are_not_reparsed() {
        let (dir, svc) = test_service();
        svc.style_save(Style::custom("custom-aaaa0001", "Mine")).expect("style");
        drop(svc);
        let svc = SettingsService::load_for_tests(&test_paths(dir.path()));
        for file in [ConfigFile::Settings, ConfigFile::Styles] {
            assert_eq!(svc.reload(file), ReloadOutcome::Unchanged, "{file:?}");
        }
    }

    #[test]
    fn default_translate_model_is_probed_first() {
        let mut p = provider("p", &["a", "b", "c"]);
        p.models[2].enabled = false;
        let default = ModelRef {
            provider_id: "p".to_owned(),
            model_id: "c".to_owned(),
        };
        put_default_model_first(&mut p, Some(&default));
        let ids: Vec<&str> = p.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["c", "a", "b"]);
        assert!(p.models[0].enabled, "默认模型被停用时临时启用（只影响这次探针）");
        // 默认模型属于别的 Provider、或已从草稿中删除：不变
        let before = p.clone();
        let other = ModelRef {
            provider_id: "q".to_owned(),
            model_id: "a".to_owned(),
        };
        let removed = ModelRef {
            provider_id: "p".to_owned(),
            model_id: "zzz".to_owned(),
        };
        put_default_model_first(&mut p, Some(&other));
        put_default_model_first(&mut p, Some(&removed));
        put_default_model_first(&mut p, None);
        assert_eq!(p, before);
        // 服务按当前设置里的默认翻译模型排序
        let (_dir, svc) = test_service();
        svc.update(&mut |s| {
            s.providers = vec![provider("p", &["a", "b"])];
            s.defaults.translate = Some(ModelRef {
                provider_id: "p".to_owned(),
                model_id: "b".to_owned(),
            });
        })
        .expect("update");
        let probe = svc.provider_for_probe(provider("p", &["a", "b"]));
        assert_eq!(probe.models[0].id, "b");
    }

    #[test]
    fn key_index_is_verified_after_state_reset() {
        use crate::settings::keychain::SecretBackend;
        use crate::settings::keychain::tests::MemoryBackend;
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(dir.path());
        let backend = Arc::new(MemoryBackend::default());
        let load = || {
            let b = Arc::clone(&backend);
            SettingsService::load_with(
                &paths,
                Some(Box::new(move || Ok(Box::new(Arc::clone(&b)) as Box<dyn SecretBackend>))),
            )
        };
        let svc = load();
        svc.update(&mut |s| s.providers = vec![provider("p", &["m"]), provider("q", &["m"])])
            .expect("providers");
        svc.keychain().set("p", "sk-p").expect("set");
        assert_eq!(svc.rebuild_key_index_now(), 0, "首次运行没有 Provider，不需要核对");
        drop(svc);
        // state.json 丢失（被删除 / 从备份恢复了旧的配置目录）：索引是空的
        std::fs::remove_file(paths.state_file()).expect("remove state.json");
        let svc = load();
        assert!(!svc.keychain().has("p"));
        let calls = backend.calls.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(svc.rebuild_key_index_now(), 1);
        assert!(svc.keychain().has("p") && !svc.keychain().has("q"));
        assert_eq!(svc.rebuild_key_index_now(), 0, "只核对一次");
        assert_eq!(
            backend.calls.load(std::sync::atomic::Ordering::SeqCst),
            calls + 2,
            "每个 Provider 读一次"
        );
    }

    #[test]
    fn hot_reload_of_invalid_json_keeps_old_values_until_fixed() {
        let (_dir, svc) = test_service();
        svc.patch(json!({"appearance": {"font_size": 15}})).expect("patch");
        let path = svc.paths().settings_file();
        let good = std::fs::read(&path).expect("read");
        std::fs::write(&path, b"{ broken").expect("write");
        assert_eq!(svc.reload(ConfigFile::Settings), ReloadOutcome::InvalidJson);
        assert_eq!(svc.get().appearance.font_size, 15);
        let issue = svc.file_issue().expect("issue");
        assert_eq!(issue.kind, SettingsIssueKind::InvalidJson);
        assert!(issue.backup_path.is_none());
        assert_eq!(
            std::fs::read(&path).expect("read"),
            b"{ broken",
            "热加载不改动用户的文件"
        );
        // 改回合法内容（与自写内容不同的格式）后自动恢复并清除提示
        let mut v: serde_json::Value = serde_json::from_slice(&good).expect("json");
        v["appearance"]["font_size"] = json!(13);
        std::fs::write(&path, serde_json::to_vec(&v).expect("json")).expect("write");
        assert_eq!(svc.reload(ConfigFile::Settings), ReloadOutcome::Applied);
        assert_eq!(svc.get().appearance.font_size, 13);
        assert!(svc.file_issue().is_none());
    }

    #[test]
    fn hot_reload_handles_removed_file() {
        let (_dir, svc) = test_service();
        std::fs::remove_file(svc.paths().settings_file()).expect("rm");
        assert_eq!(svc.reload(ConfigFile::Settings), ReloadOutcome::Missing);
        assert_eq!(*svc.get(), fresh_settings());
    }

    #[test]
    fn update_rejects_lossy_changes() {
        let (_dir, svc) = test_service();
        let err = svc
            .update(&mut |s| s.appearance.accent = "not-a-color".to_owned())
            .expect_err("lossy");
        assert_eq!(err.code, IpcErrorCode::InvalidArgument);
        assert_eq!(svc.get().appearance.accent, "system");
    }

    #[test]
    fn custom_style_lifecycle() {
        let (_dir, svc) = test_service();
        let mut s = Style::custom("", "  Mine  ");
        s.temperature = Some(0.4);
        let styles = svc.style_save(s).expect("save");
        let created = styles.iter().find(|s| !s.builtin).cloned().expect("custom");
        assert!(created.id.starts_with("custom-") && created.id.len() == "custom-".len() + 8);
        assert_eq!(created.name, "Mine");
        assert_eq!(
            styles.last().map(|s| s.id.as_str()),
            Some(created.id.as_str()),
            "新风格排在末尾"
        );
        // 编辑
        let mut edited = created.clone();
        edited.dictionary_mode = false;
        let styles = svc.style_save(edited).expect("edit");
        assert!(
            !styles
                .iter()
                .find(|s| s.id == created.id)
                .expect("found")
                .dictionary_mode
        );
        // 「重启」后仍在
        let reloaded = SettingsService::load_for_tests(svc.paths());
        assert!(reloaded.styles().iter().any(|s| s.id == created.id));
        // 设为默认风格后删除：默认风格回到 natural
        svc.patch(json!({"translate": {"default_style": created.id}}))
            .expect("default");
        svc.style_delete(&created.id).expect("delete");
        assert!(!svc.styles().iter().any(|s| s.id == created.id));
        assert_eq!(svc.get().translate.default_style, "natural");
        assert_eq!(
            svc.style_delete(&created.id).expect_err("gone").code,
            IpcErrorCode::NotFound
        );
    }

    #[test]
    fn style_validation() {
        let (_dir, svc) = test_service();
        let mut s = Style::custom("", "x");
        s.temperature = Some(1.5);
        assert_eq!(svc.style_save(s).expect_err("t").code, IpcErrorCode::InvalidArgument);
        let mut s = Style::custom("", "x");
        s.temperature = Some(f32::NAN);
        assert_eq!(svc.style_save(s).expect_err("nan").code, IpcErrorCode::InvalidArgument);
        assert_eq!(
            svc.style_save(Style::custom("", "  ")).expect_err("name").code,
            IpcErrorCode::InvalidArgument
        );
        assert_eq!(
            svc.style_save(Style::custom("mine", "x")).expect_err("id").code,
            IpcErrorCode::InvalidArgument
        );
        let mut s = Style::custom("", "x");
        s.hotkey = Some("Alt+Nope".to_owned());
        assert_eq!(
            svc.style_save(s).expect_err("hotkey").code,
            IpcErrorCode::InvalidArgument
        );
        let mut s = Style::custom("", "x");
        s.temperature = None;
        s.hotkey = Some("  ".to_owned());
        let styles = svc.style_save(s).expect("null temperature and blank hotkey are fine");
        let c = styles.iter().find(|s| !s.builtin).expect("custom");
        assert_eq!(c.temperature, None);
        assert_eq!(c.hotkey, None);
        assert_eq!(
            svc.style_delete("natural").expect_err("builtin").code,
            IpcErrorCode::InvalidArgument
        );
        assert_eq!(
            svc.style_reset(&c.id).expect_err("custom").code,
            IpcErrorCode::InvalidArgument
        );
        assert_eq!(
            svc.style_reset("custom-nope").expect_err("missing").code,
            IpcErrorCode::NotFound
        );
    }

    #[test]
    fn builtin_override_round_trip() {
        let (_dir, svc) = test_service();
        let Some(natural) = wisp_prompts::builtin_styles().into_iter().find(|s| s.id == "natural") else {
            // 没有内置风格时，只验证找不到时的错误
            let mut s = Style::custom("natural", "");
            s.temperature = Some(0.5);
            assert_eq!(svc.style_save(s).expect_err("stub").code, IpcErrorCode::NotFound);
            return;
        };
        let mut edited = natural.clone();
        edited.temperature = Some(0.5);
        let styles = svc.style_save(edited).expect("override");
        let n = styles.iter().find(|s| s.id == "natural").expect("natural");
        assert_eq!(n.temperature, Some(0.5));
        assert!(n.modified);
        let file = read_json(&svc.paths().styles_file());
        assert_eq!(file["overrides"]["natural"], json!({"temperature": 0.5}));
        let styles = svc.style_reset("natural").expect("reset");
        let n = styles.iter().find(|s| s.id == "natural").expect("natural");
        assert_eq!(n.temperature, natural.temperature);
        assert!(!n.modified);
        let file = read_json(&svc.paths().styles_file());
        assert!(
            file["overrides"].get("natural").is_none(),
            "还原后 styles.json 里没有 natural 的 override"
        );
        // 保存与默认值相同的内容等同于还原
        svc.style_save(natural.clone()).expect("same");
        assert!(
            read_json(&svc.paths().styles_file())["overrides"]
                .get("natural")
                .is_none()
        );
    }

    #[test]
    fn reorder_keeps_unknown_out_and_missing_at_end() {
        let (_dir, svc) = test_service();
        let a = svc.style_save(Style::custom("custom-0000000a", "A")).expect("a");
        let _ = a;
        let b = svc.style_save(Style::custom("custom-0000000b", "B")).expect("b");
        let all: Vec<String> = b.iter().map(|s| s.id.clone()).collect();
        let styles = svc
            .styles_reorder(vec![
                "custom-0000000b".to_owned(),
                "bogus".to_owned(),
                "custom-0000000b".to_owned(),
            ])
            .expect("reorder");
        let ids: Vec<&str> = styles.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids.first(), Some(&"custom-0000000b"));
        assert_eq!(ids.len(), all.len());
        assert!(!ids.contains(&"bogus"));
    }

    #[test]
    fn glossary_set_import_and_reload() {
        let (_dir, svc) = test_service();
        let entry = |s: &str, t: &str| GlossaryEntry {
            source: s.to_owned(),
            target: t.to_owned(),
            note: None,
        };
        svc.glossary_set(vec![entry(" token ", "令牌"), entry("", "x")])
            .expect("set");
        assert_eq!(*svc.glossary_entries(), vec![entry("token", "令牌")]);
        assert_eq!(svc.glossary().hits("a token here").len(), 1);
        let count = svc
            .glossary_import(vec![entry("token", "记号"), entry("LLM", "大模型")])
            .expect("import");
        assert_eq!(count, 2);
        assert_eq!(
            *svc.glossary_entries(),
            vec![entry("token", "记号"), entry("LLM", "大模型")]
        );
        // 外部修改 glossary.json → 热加载
        let path = svc.paths().glossary_file();
        std::fs::write(
            &path,
            r#"{"schema_version":1,"entries":[{"source":"GPU","target":"GPU"}]}"#,
        )
        .expect("write");
        assert_eq!(svc.reload(ConfigFile::Glossary), ReloadOutcome::Applied);
        assert_eq!(*svc.glossary_entries(), vec![entry("GPU", "GPU")]);
        let reloaded = SettingsService::load_for_tests(svc.paths());
        assert_eq!(*reloaded.glossary_entries(), vec![entry("GPU", "GPU")]);
    }

    #[test]
    fn provider_delete_removes_key_defaults_and_style_bindings() {
        let (_dir, svc) = test_service();
        let mref = ModelRef {
            provider_id: "p".to_owned(),
            model_id: "m".to_owned(),
        };
        svc.update(&mut |s| {
            s.providers.push(provider("p", &["m"]));
            s.providers.push(provider("q", &["n"]));
            s.defaults.translate = Some(mref.clone());
            s.defaults.ocr = Some(ModelRef {
                provider_id: "q".to_owned(),
                model_id: "n".to_owned(),
            });
        })
        .expect("seed");
        let mut style = Style::custom("custom-000000aa", "Bound");
        style.model_ref = Some(mref.clone());
        svc.style_save(style).expect("style");
        svc.keychain().set("p", "sk-test").expect("key");
        assert_eq!(svc.referenced_models("p"), vec!["m".to_owned()]);
        assert!(svc.keychain().has("p"));
        let (s, key_removed) = svc.provider_delete("p").expect("delete");
        assert!(key_removed);
        assert!(s.provider("p").is_none());
        assert!(s.defaults.translate.is_none());
        assert!(s.defaults.ocr.is_some(), "其他 Provider 的默认模型不受影响");
        assert!(!svc.keychain().has("p"));
        assert!(svc.keychain().get("p").expect("get").is_none());
        let style = svc
            .styles()
            .iter()
            .find(|s| s.id == "custom-000000aa")
            .cloned()
            .expect("style");
        assert!(style.model_ref.is_none());
    }

    #[test]
    fn provider_is_deleted_even_when_the_keychain_refuses() {
        use crate::settings::keychain::SecretBackend;
        use crate::settings::keychain::tests::MemoryBackend;
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = Arc::new(MemoryBackend::default());
        let b = Arc::clone(&backend);
        let svc = SettingsService::load_with(
            &test_paths(dir.path()),
            Some(Box::new(move || Ok(Box::new(Arc::clone(&b)) as Box<dyn SecretBackend>))),
        );
        let template = wisp_llm::templates().first().cloned().expect("template");
        let draft = svc.provider_new(&template.id).expect("draft");
        let id = draft.id.clone();
        svc.update(&mut |s| s.providers.push(draft.clone())).expect("save");
        svc.keychain().set(&id, "sk-old").expect("key");
        // 用户在授权框点了「拒绝」：Provider 照样删掉，Key 记为待清理
        backend.deny.store(true, std::sync::atomic::Ordering::SeqCst);
        let (settings, key_removed) = svc.provider_delete(&id).expect("deleted anyway");
        assert!(!key_removed);
        assert!(settings.provider(&id).is_none(), "设置里不留删不掉的条目");
        assert!(svc.state().read(|s| s.orphan_keys.contains(&id)));
        // 新建同一模板的 Provider 不复用这个 id（不会读到旧 Key）
        let next = svc.provider_new(&template.id).expect("draft");
        assert_ne!(next.id, id);
        // 之后一次显式的钥匙串操作时清理掉
        backend.deny.store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(svc.purge_orphan_keys(), 1);
        assert!(backend.items.lock().get(&id).is_none());
        assert!(svc.state().read(|s| s.orphan_keys.is_empty()));
        assert_eq!(svc.purge_orphan_keys(), 0, "没有待清理时什么也不做");
    }

    #[test]
    fn keychain_index_survives_restart_and_changes_notify() {
        let (_dir, svc) = test_service();
        let mut rx = svc.changes();
        rx.borrow_and_update();
        svc.keychain().set("p", "sk").expect("set");
        assert!(rx.has_changed().expect("open"), "has_key 变化会触发 settings:changed");
        let reloaded = SettingsService::load_for_tests(svc.paths());
        assert!(reloaded.keychain().has("p"), "索引持久化在 state.json，不读钥匙串");
    }

    #[test]
    fn onboarding_state() {
        let (_dir, svc) = test_service();
        assert!(svc.needs_onboarding());
        svc.complete_onboarding().expect("complete");
        assert!(!svc.needs_onboarding());
        let reloaded = SettingsService::load_for_tests(svc.paths());
        assert!(!reloaded.needs_onboarding());
    }

    #[test]
    fn style_preview_uses_current_settings() {
        let (_dir, svc) = test_service();
        let preview = svc.style_preview(&Style::custom("custom-00000001", "x"), "hello world");
        // 具体内容由 wisp-prompts 决定；这里只保证调用路径可用且原文出现在 user 部分
        assert!(preview.user.contains("hello world"));
    }

    // ---- 同步的接入点：改动钩子、apply_synced、读取情况、按 id 写入、Key 钩子 ----

    /// 钩子收到的一次改动（测试里记下来逐项比对）
    #[derive(Debug, Clone)]
    struct Seen {
        file: ConfigFile,
        origin: ChangeOrigin,
        before: serde_json::Value,
        after: serde_json::Value,
        lossy: bool,
    }

    fn record_changes(svc: &SettingsService) -> (Arc<Mutex<Vec<Seen>>>, BTreeMap<ConfigFile, serde_json::Value>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let baseline = svc.set_change_hook(Some(Arc::new(move |c: &ConfigChange<'_>| {
            sink.lock().push(Seen {
                file: c.file,
                origin: c.origin,
                before: c.before.clone(),
                after: c.after.clone(),
                lossy: c.lossy,
            });
        })));
        (seen, baseline)
    }

    fn take(seen: &Mutex<Vec<Seen>>) -> Vec<Seen> {
        std::mem::take(&mut *seen.lock())
    }

    fn gentry(s: &str, t: &str) -> GlossaryEntry {
        GlossaryEntry {
            source: s.to_owned(),
            target: t.to_owned(),
            note: None,
        }
    }

    /// 改写配置文件（模拟用户在编辑器里保存）
    fn edit_file(path: &std::path::Path, f: impl FnOnce(&mut serde_json::Value)) {
        let mut v = read_json(path);
        f(&mut v);
        std::fs::write(path, serde_json::to_vec_pretty(&v).expect("json")).expect("write");
    }

    #[test]
    fn change_hook_reports_every_replacement_with_its_origin() {
        let (_dir, svc) = test_service();
        let (seen, baseline) = record_changes(&svc);
        for file in ConfigFile::ALL {
            assert_eq!(
                baseline[&file],
                svc.config_value(file),
                "基线就是安装那一刻的规范化文档"
            );
        }

        // 本机写入：settings / styles / glossary 各一次
        svc.patch(json!({"appearance": {"font_size": 16}})).expect("patch");
        svc.style_save(Style::custom("custom-0000000a", "Mine")).expect("style");
        svc.glossary_set(vec![gentry("token", "令牌")]).expect("glossary");
        let local = take(&seen);
        assert_eq!(
            local.iter().map(|s| (s.file, s.origin)).collect::<Vec<_>>(),
            [
                (ConfigFile::Settings, ChangeOrigin::Local),
                (ConfigFile::Styles, ChangeOrigin::Local),
                (ConfigFile::Glossary, ChangeOrigin::Local),
            ]
        );
        assert_eq!(local[0].before, baseline[&ConfigFile::Settings], "前后相接，没有缝隙");
        assert_eq!(local[0].before["appearance"]["font_size"], 14);
        assert_eq!(local[0].after["appearance"]["font_size"], 16);
        assert_eq!(local[1].after["custom"][0]["id"], "custom-0000000a");
        assert_eq!(local[2].before["entries"], json!([]));
        assert_eq!(
            local[2].after["entries"],
            json!([{"source": "token", "target": "令牌", "note": null}])
        );

        // 内容没变的写入不调用钩子
        svc.patch(json!({"appearance": {"font_size": 16}})).expect("same");
        svc.glossary_set(vec![gentry("token", "令牌")]).expect("same");
        assert!(take(&seen).is_empty());

        // 手改文件后的热加载
        edit_file(&svc.paths().settings_file(), |v| {
            v["appearance"]["font_size"] = json!(17)
        });
        edit_file(&svc.paths().styles_file(), |v| {
            v["custom"][0]["name"] = json!("Renamed")
        });
        edit_file(&svc.paths().glossary_file(), |v| {
            v["entries"] = json!([{"source": "GPU", "target": "GPU"}]);
        });
        for file in ConfigFile::ALL {
            assert_eq!(svc.reload(file), ReloadOutcome::Applied, "{file:?}");
        }
        let reloaded = take(&seen);
        assert!(
            reloaded.iter().all(|s| s.origin == ChangeOrigin::HotReload && !s.lossy),
            "{reloaded:?}"
        );
        assert_eq!(reloaded.len(), 3);
        assert_eq!(reloaded[0].after["appearance"]["font_size"], 17);
        assert_eq!(reloaded[1].after["custom"][0]["name"], "Renamed");

        // 同步应用
        for file in ConfigFile::ALL {
            let outcome = svc
                .apply_synced(file, &mut |doc| {
                    let mut doc = doc.clone();
                    match file {
                        ConfigFile::Settings => doc["appearance"]["font_size"] = json!(13),
                        ConfigFile::Styles => doc["order"] = json!(["custom-0000000a"]),
                        ConfigFile::Glossary => doc["entries"] = json!([{"source": "LLM", "target": "大模型"}]),
                    }
                    Some(doc)
                })
                .expect("apply");
            assert_eq!(outcome, SyncApply::Applied, "{file:?}");
        }
        let synced = take(&seen);
        assert_eq!(synced.len(), 3);
        assert!(synced.iter().all(|s| s.origin == ChangeOrigin::Sync), "{synced:?}");

        // 每次改动的 after 都与 config_value 一致（下一次改动的 before 接得上）
        for file in ConfigFile::ALL {
            let last = synced.iter().rfind(|s| s.file == file).expect("change");
            assert_eq!(last.after, svc.config_value(file), "{file:?}");
        }
    }

    /// 手改文件里写错的内容被宽松解析丢掉或退回：热加载的改动标为有损，读取情况为 Lossy；改正之后恢复正常。
    /// 启动时读到这样的文件同样是 Lossy；文件不存在而被创建的仍按 Created 处理
    #[test]
    fn lossy_hand_edits_are_flagged() {
        let (_dir, first) = test_service();
        first
            .update(&mut |s| s.providers = vec![provider("keep", &["m"]), provider("typo", &["m"])])
            .expect("seed");
        first
            .glossary_set(vec![gentry("hello", "你好"), gentry("world", "世界")])
            .expect("seed");
        assert_eq!(first.config_load_state(ConfigFile::Settings), LoadState::Created);
        // 重新启动：这次是正常读到的文件
        let svc = SettingsService::load_for_tests(first.paths());
        for file in ConfigFile::ALL {
            assert_eq!(svc.config_load_state(file), LoadState::Normal, "{file:?}");
        }
        let (seen, _) = record_changes(&svc);
        let settings_path = svc.paths().settings_file();
        let glossary_path = svc.paths().glossary_file();

        // 服务的协议写错：这一项被丢掉
        edit_file(&settings_path, |v| {
            v["providers"][1]["protocol"] = json!("openai_chatt")
        });
        assert_eq!(svc.reload(ConfigFile::Settings), ReloadOutcome::Applied);
        let change = take(&seen).pop().expect("change");
        assert_eq!((change.origin, change.lossy), (ChangeOrigin::HotReload, true));
        assert_eq!(svc.get().providers.len(), 1);
        assert_eq!(svc.config_load_state(ConfigFile::Settings), LoadState::Lossy);

        // `"providers": null`：整组退回默认值
        edit_file(&settings_path, |v| v["providers"] = serde_json::Value::Null);
        assert_eq!(svc.reload(ConfigFile::Settings), ReloadOutcome::Applied);
        assert!(take(&seen).pop().expect("change").lossy);
        assert!(svc.get().providers.is_empty());

        // 改正之后：干净的热加载，恢复正常
        edit_file(&settings_path, |v| {
            v["providers"] = json!([provider("keep", &["m"]), provider("typo", &["m"])]);
        });
        assert_eq!(svc.reload(ConfigFile::Settings), ReloadOutcome::Applied);
        assert!(!take(&seen).pop().expect("change").lossy);
        assert_eq!(svc.get().providers.len(), 2);
        assert_eq!(svc.config_load_state(ConfigFile::Settings), LoadState::Normal);

        // 术语表里一条写错（target 不是字符串）：这一条被丢掉
        edit_file(&glossary_path, |v| v["entries"][1]["target"] = json!(42));
        assert_eq!(svc.reload(ConfigFile::Glossary), ReloadOutcome::Applied);
        assert!(take(&seen).pop().expect("change").lossy);
        assert_eq!(*svc.glossary_entries(), vec![gentry("hello", "你好")]);
        assert_eq!(svc.config_load_state(ConfigFile::Glossary), LoadState::Lossy);

        // 关闭期间写错的文件：启动时同样是 Lossy
        edit_file(&settings_path, |v| v["providers"][0]["protocol"] = json!(7));
        let restarted = SettingsService::load_for_tests(svc.paths());
        assert_eq!(restarted.config_load_state(ConfigFile::Settings), LoadState::Lossy);
        assert_eq!(restarted.config_load_state(ConfigFile::Glossary), LoadState::Lossy);
        assert_eq!(restarted.config_load_state(ConfigFile::Styles), LoadState::Normal);
    }

    #[test]
    fn provider_delete_changes_are_reported_as_local() {
        let (_dir, svc) = test_service();
        svc.update(&mut |s| s.providers = vec![provider("p", &["m"])])
            .expect("seed");
        let (seen, _) = record_changes(&svc);
        svc.provider_delete("p").expect("delete");
        let changes = take(&seen);
        assert!(changes.iter().all(|s| s.origin == ChangeOrigin::Local));
        assert!(
            changes
                .iter()
                .any(|s| s.file == ConfigFile::Settings && s.after["providers"] == json!([]))
        );
    }

    #[test]
    fn no_hook_calls_once_uninstalled() {
        let (_dir, svc) = test_service();
        let (seen, _) = record_changes(&svc);
        svc.set_change_hook(None);
        svc.patch(json!({"appearance": {"font_size": 16}})).expect("patch");
        svc.glossary_set(vec![gentry("a", "b")]).expect("glossary");
        svc.apply_synced(ConfigFile::Settings, &mut |doc| {
            let mut doc = doc.clone();
            doc["appearance"]["font_size"] = json!(15);
            Some(doc)
        })
        .expect("apply");
        assert!(seen.lock().is_empty());
    }

    /// 规范化文档里的 f32 与写盘的一样（0.6、0.4），而不是拓宽后的 0.6000000238418579
    #[test]
    fn canonical_documents_keep_f32_values_as_written() {
        let (_dir, svc) = test_service();
        let (seen, baseline) = record_changes(&svc);
        assert_eq!(
            baseline[&ConfigFile::Settings]["appearance"]["max_height_ratio"],
            json!(0.6)
        );
        let mut s = Style::custom("custom-0000000b", "Warm");
        s.temperature = Some(0.4);
        svc.style_save(s).expect("style");
        svc.patch(json!({"appearance": {"max_height_ratio": 0.7}}))
            .expect("patch");
        let changes = take(&seen);
        assert_eq!(changes[0].after["custom"][0]["temperature"], json!(0.4));
        assert_eq!(changes[1].before["appearance"]["max_height_ratio"], json!(0.6));
        assert_eq!(changes[1].after["appearance"]["max_height_ratio"], json!(0.7));
        // 同步原样写回同一份文档：没有假差异，不写盘、不调用钩子
        let outcome = svc
            .apply_synced(ConfigFile::Settings, &mut |doc| Some(doc.clone()))
            .expect("apply");
        assert_eq!(outcome, SyncApply::Unchanged);
        assert!(take(&seen).is_empty());
    }

    #[test]
    fn apply_synced_is_deferred_while_the_file_is_being_edited() {
        let (_dir, svc) = test_service();
        let path = svc.paths().settings_file();
        let bump = |svc: &SettingsService| {
            svc.apply_synced(ConfigFile::Settings, &mut |doc| {
                let mut doc = doc.clone();
                doc["appearance"]["font_size"] = json!(12);
                Some(doc)
            })
            .expect("apply")
        };
        let (seen, _) = record_changes(&svc);
        // 编辑器刚保存、热加载还没来（去抖中，或者监听根本没在工作）：不覆盖用户的修改，而是当场把它作为热加载处理掉，
        // 这一批远端值留到下一轮
        edit_file(&path, |v| v["appearance"]["font_size"] = json!(17));
        assert_eq!(bump(&svc), SyncApply::Deferred);
        assert_eq!(read_json(&path)["appearance"]["font_size"], 17);
        assert_eq!(svc.get().appearance.font_size, 17, "手改已经载入");
        let changes = take(&seen);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].origin, ChangeOrigin::HotReload, "记为手改");
        // 之后迟到的热加载认得这份内容；下一轮照常应用
        assert_eq!(svc.reload(ConfigFile::Settings), ReloadOutcome::Unchanged);
        assert_eq!(bump(&svc), SyncApply::Applied);
        assert_eq!(svc.get().appearance.font_size, 12);
        assert_eq!(read_json(&path)["appearance"]["font_size"], 12);
        // 自己写的内容热加载忽略
        assert_eq!(svc.reload(ConfigFile::Settings), ReloadOutcome::Unchanged);
        // 只改了格式（内容与内存相同）：不必推迟
        let doc = read_json(&path);
        std::fs::write(&path, serde_json::to_vec(&doc).expect("json")).expect("write");
        assert_eq!(bump(&svc), SyncApply::Unchanged, "内容相同，直接照常处理");
        assert_eq!(read_json(&path)["appearance"]["font_size"], 12);

        // 非法 JSON：保留用户的文件，不写，内存保持上一次的有效值
        std::fs::write(&path, b"{ half-written").expect("write");
        assert_eq!(svc.reload(ConfigFile::Settings), ReloadOutcome::InvalidJson);
        assert_eq!(svc.config_load_state(ConfigFile::Settings), LoadState::InvalidJson);
        let outcome = svc
            .apply_synced(ConfigFile::Settings, &mut |doc| {
                let mut doc = doc.clone();
                doc["appearance"]["font_size"] = json!(16);
                Some(doc)
            })
            .expect("apply");
        assert_eq!(outcome, SyncApply::Deferred);
        assert_eq!(std::fs::read(&path).expect("read"), b"{ half-written");
        assert_eq!(svc.get().appearance.font_size, 12);
        // 改回合法内容后恢复为启动时的情况（这里是首次运行创建的文件）
        edit_file_from(&path, &svc.config_value(ConfigFile::Settings));
        assert_eq!(svc.reload(ConfigFile::Settings), ReloadOutcome::Unchanged);
        assert_eq!(svc.config_load_state(ConfigFile::Settings), LoadState::Created);
    }

    fn edit_file_from(path: &std::path::Path, doc: &serde_json::Value) {
        std::fs::write(path, serde_json::to_vec(doc).expect("json")).expect("write");
    }

    #[test]
    fn apply_synced_keeps_local_only_settings() {
        let (_dir, svc) = test_service();
        svc.update(&mut |s| {
            s.sync.enabled = true;
            s.sync.server_url = "https://dav.example.com/dav".to_owned();
            s.sync.username = "me".to_owned();
        })
        .expect("seed");
        let before = svc.get();
        let mut rx = svc.changes();
        rx.borrow_and_update();
        let outcome = svc
            .apply_synced(ConfigFile::Settings, &mut |doc| {
                let mut doc = doc.clone();
                doc["schema_version"] = json!(99);
                doc["general"]["launch_at_login"] = json!(!before.general.launch_at_login);
                doc["general"]["developer_mode"] = json!(true);
                doc["advanced"]["log_level"] = json!("trace");
                doc["sync"] = json!({"enabled": false, "server_url": "https://evil.example.com", "username": "x"});
                doc["clipboard"]["enabled"] = json!(false);
                doc["clipboard"]["secret_detection"] = json!(false);
                doc["ocr"]["history"] = json!(true);
                doc["offline"] =
                    json!({"ocr_model": "pp_ocrv6_small", "translate_model": "index_translate_9b", "use_gpu": false});
                // 全局设置照常应用
                doc["translate"]["long_text_threshold"] = json!(5000);
                Some(doc)
            })
            .expect("apply");
        assert_eq!(outcome, SyncApply::Applied);
        let after = svc.get();
        assert_eq!(after.translate.long_text_threshold, 5000);
        assert_eq!(after.schema_version, before.schema_version);
        assert_eq!(after.general.launch_at_login, before.general.launch_at_login);
        assert_eq!(after.general.developer_mode, before.general.developer_mode);
        assert_eq!(after.advanced, before.advanced);
        assert_eq!(after.sync, before.sync);
        assert_eq!(after.clipboard.enabled, before.clipboard.enabled);
        assert_eq!(after.clipboard.secret_detection, before.clipboard.secret_detection);
        assert_eq!(after.ocr.history, before.ocr.history);
        assert_eq!(after.offline, before.offline, "离线模型的选择只属于本机");
        assert!(rx.has_changed().expect("open"), "副作用与 settings:changed 照常发生");
        assert_eq!(
            read_json(&svc.paths().settings_file())["translate"]["long_text_threshold"],
            5000
        );
    }

    /// 远端的值超出本版本的范围：夹紧后应用，钩子以 Sync 报告夹紧后的值——同步据此更新基线，不把夹紧当作本机改动发回
    #[test]
    fn apply_synced_accepts_fixes_and_reports_them_as_sync() {
        let (_dir, svc) = test_service();
        let (seen, _) = record_changes(&svc);
        let outcome = svc
            .apply_synced(ConfigFile::Settings, &mut |doc| {
                let mut doc = doc.clone();
                doc["appearance"]["font_size"] = json!(99);
                doc["appearance"]["theme"] = json!("Neon");
                Some(doc)
            })
            .expect("apply");
        assert_eq!(outcome, SyncApply::Applied);
        assert_eq!(svc.get().appearance.font_size, 18);
        let changes = take(&seen);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].origin, ChangeOrigin::Sync);
        assert_eq!(changes[0].after["appearance"]["font_size"], 18);
        assert_eq!(changes[0].after["appearance"]["theme"], "system");
        // 合并函数返回 None：什么也不做
        assert_eq!(
            svc.apply_synced(ConfigFile::Settings, &mut |_| None).expect("none"),
            SyncApply::Unchanged
        );
        assert!(take(&seen).is_empty());
    }

    /// 降级运行时应用同步：更新版本写下的字段与版本号照样保留
    #[test]
    fn apply_synced_keeps_content_from_a_newer_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(dir.path());
        std::fs::create_dir_all(&paths.config_dir).expect("dir");
        let mut newer = serde_json::to_value(Settings::default()).expect("json");
        newer["schema_version"] = json!(SETTINGS_SCHEMA_VERSION + 1);
        newer["future_section"] = json!({"enabled": true});
        std::fs::write(paths.settings_file(), serde_json::to_vec_pretty(&newer).expect("json")).expect("write");
        let svc = SettingsService::load_for_tests(&paths);
        let outcome = svc
            .apply_synced(ConfigFile::Settings, &mut |doc| {
                let mut doc = doc.clone();
                doc["appearance"]["font_size"] = json!(16);
                Some(doc)
            })
            .expect("apply");
        assert_eq!(outcome, SyncApply::Applied);
        let on_disk = read_json(&paths.settings_file());
        assert_eq!(on_disk["appearance"]["font_size"], 16);
        assert_eq!(on_disk["schema_version"], SETTINGS_SCHEMA_VERSION + 1);
        assert_eq!(on_disk["future_section"]["enabled"], true);
    }

    #[test]
    fn apply_synced_for_styles_and_glossary() {
        let (_dir, svc) = test_service();
        svc.glossary_set(vec![gentry("token", "令牌")]).expect("glossary");
        let outcome = svc
            .apply_synced(ConfigFile::Styles, &mut |doc| {
                let mut doc = doc.clone();
                doc["custom"] =
                    json!([serde_json::to_value(Style::custom("custom-0000000c", "Remote")).expect("json")]);
                Some(doc)
            })
            .expect("styles");
        assert_eq!(outcome, SyncApply::Applied);
        assert!(
            svc.styles()
                .iter()
                .any(|s| s.id == "custom-0000000c" && s.name == "Remote")
        );
        assert_eq!(
            read_json(&svc.paths().styles_file())["custom"][0]["id"],
            "custom-0000000c"
        );
        let outcome = svc
            .apply_synced(ConfigFile::Glossary, &mut |doc| {
                let mut doc = doc.clone();
                doc["entries"]
                    .as_array_mut()
                    .expect("entries")
                    .push(json!({"source": " GPU ", "target": "GPU", "note": null}));
                Some(doc)
            })
            .expect("glossary");
        assert_eq!(outcome, SyncApply::Applied);
        assert_eq!(
            *svc.glossary_entries(),
            vec![gentry("token", "令牌"), gentry("GPU", "GPU")]
        );
        assert_eq!(svc.glossary().hits("GPU here").len(), 1, "匹配器同步更新");
        for file in [ConfigFile::Styles, ConfigFile::Glossary] {
            assert_eq!(
                svc.reload(file),
                ReloadOutcome::Unchanged,
                "{file:?}：热加载忽略这次写入"
            );
        }
    }

    #[test]
    fn load_state_reports_created_and_reset_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(dir.path());
        let svc = SettingsService::load_for_tests(&paths);
        for file in ConfigFile::ALL {
            assert_eq!(svc.config_load_state(file), LoadState::Created, "{file:?}：首次运行");
        }
        drop(svc);
        let svc = SettingsService::load_for_tests(&paths);
        for file in ConfigFile::ALL {
            assert_eq!(svc.config_load_state(file), LoadState::Normal, "{file:?}");
        }
        drop(svc);
        // 删掉 settings.json（「重置 Wisp」的常见做法）、把 styles.json 写坏
        std::fs::remove_file(paths.settings_file()).expect("rm");
        std::fs::write(paths.styles_file(), b"[oops").expect("write");
        let svc = SettingsService::load_for_tests(&paths);
        assert_eq!(svc.config_load_state(ConfigFile::Settings), LoadState::Created);
        assert_eq!(svc.config_load_state(ConfigFile::Styles), LoadState::ResetToDefault);
        assert_eq!(svc.config_load_state(ConfigFile::Glossary), LoadState::Normal);
        drop(svc);
        std::fs::write(paths.settings_file(), b"not json").expect("write");
        let svc = SettingsService::load_for_tests(&paths);
        assert_eq!(svc.config_load_state(ConfigFile::Settings), LoadState::ResetToDefault);
    }

    #[test]
    fn provider_save_replaces_only_that_provider_against_fresh_state() {
        let (_dir, svc) = test_service();
        svc.update(&mut |s| s.providers = vec![provider("a", &["m"]), provider("b", &["n"])])
            .expect("seed");
        // 设置窗口打开期间，b 在别处改了名（例如同步）
        svc.update(&mut |s| s.providers[1].name = "B from elsewhere".to_owned())
            .expect("remote");
        // 窗口保存的是它那份 a：b 不受影响
        let mut a = provider("a", &["m", "m2"]);
        a.name = "A edited".to_owned();
        let settings = svc.provider_save(a).expect("save");
        assert_eq!(settings.providers[0].name, "A edited");
        assert_eq!(settings.providers[0].models.len(), 2);
        assert_eq!(settings.providers[1].name, "B from elsewhere");
        // 在别处删掉之后再保存：not_found，不加回来
        svc.update(&mut |s| s.providers.retain(|p| p.id != "a"))
            .expect("delete");
        let err = svc.provider_save(provider("a", &["m"])).expect_err("deleted");
        assert_eq!(err.code, IpcErrorCode::NotFound);
        assert!(svc.get().provider("a").is_none());
        // 非法内容照常拒绝
        let mut bad = provider("b", &["n"]);
        bad.headers.insert("bad header".to_owned(), "x".to_owned());
        assert_eq!(
            svc.provider_save(bad).expect_err("lossy").code,
            IpcErrorCode::InvalidArgument
        );
    }

    #[test]
    fn provider_add_appends_or_merges_and_reorder_keeps_content() {
        let (_dir, svc) = test_service();
        svc.update(&mut |s| s.providers = vec![provider("a", &["m"])])
            .expect("seed");
        svc.provider_add(provider("b", &["n"])).expect("add");
        let mut same_id = provider("a", &["x"]);
        same_id.name = "Added again".to_owned();
        let settings = svc.provider_add(same_id).expect("merge");
        let ids: Vec<&str> = settings.providers.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"], "同 id 合并为一个，位置不变");
        assert_eq!(settings.providers[0].name, "Added again");
        svc.update(&mut |s| s.providers[1].name = "B2".to_owned())
            .expect("edit");
        let settings = svc
            .providers_reorder(&["b".to_owned(), "zzz".to_owned()])
            .expect("reorder");
        let ids: Vec<&str> = settings.providers.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["b", "a"]);
        assert_eq!(settings.providers[0].name, "B2", "只重排，内容取最新的");
    }

    #[test]
    fn glossary_upsert_and_remove_touch_one_entry() {
        let (_dir, svc) = test_service();
        svc.glossary_set(vec![gentry("token", "令牌"), gentry("GPU", "GPU")])
            .expect("seed");
        // 新增：追加到末尾
        svc.glossary_upsert(gentry(" LLM ", "大模型"), None).expect("insert");
        // 修改：原位替换
        svc.glossary_upsert(gentry("token", "记号"), Some("token"))
            .expect("update");
        assert_eq!(
            *svc.glossary_entries(),
            vec![gentry("token", "记号"), gentry("GPU", "GPU"), gentry("LLM", "大模型")]
        );
        // 改原文：占原来的位置
        svc.glossary_upsert(gentry("Token", "记号"), Some("token"))
            .expect("rename");
        assert_eq!(svc.glossary_entries()[0], gentry("Token", "记号"));
        // 改成已有的原文：合并为一条，位置取靠前的
        svc.glossary_upsert(gentry("Token", "令牌"), Some("LLM"))
            .expect("merge");
        assert_eq!(
            *svc.glossary_entries(),
            vec![gentry("Token", "令牌"), gentry("GPU", "GPU")]
        );
        // 原来的那条已经在别处删了：照常写入
        svc.glossary_upsert(gentry("CPU", "CPU"), Some("gone"))
            .expect("previous missing");
        assert_eq!(svc.glossary_entries().len(), 3);
        // 删除；不存在的视为成功
        svc.glossary_remove(" GPU ").expect("remove");
        svc.glossary_remove("never").expect("missing is fine");
        assert_eq!(
            *svc.glossary_entries(),
            vec![gentry("Token", "令牌"), gentry("CPU", "CPU")]
        );
        assert_eq!(
            svc.glossary_upsert(gentry("  ", "x"), None).expect_err("empty").code,
            IpcErrorCode::InvalidArgument
        );
        // 写进了文件，重启后仍在
        let reloaded = SettingsService::load_for_tests(svc.paths());
        assert_eq!(*reloaded.glossary_entries(), *svc.glossary_entries());
    }

    #[test]
    fn glossary_edits_keep_entries_added_elsewhere() {
        let (_dir, svc) = test_service();
        svc.glossary_set(vec![gentry("token", "令牌")]).expect("seed");
        // 编辑器打开之后，别处加了一条
        svc.apply_synced(ConfigFile::Glossary, &mut |doc| {
            let mut doc = doc.clone();
            doc["entries"]
                .as_array_mut()
                .expect("entries")
                .push(json!({"source": "Remote", "target": "远端"}));
            Some(doc)
        })
        .expect("apply");
        // 编辑器逐条写入自己那一行：别处加的那条不受影响
        svc.glossary_upsert(gentry("token", "记号"), Some("token"))
            .expect("edit");
        assert_eq!(
            *svc.glossary_entries(),
            vec![gentry("token", "记号"), gentry("Remote", "远端")]
        );
    }

    /// 排除名单与按键映射逐项写入：基于最新的设置只改这一项，别处（同步）刚加进来的应用不会被设置窗口里的旧名单删掉
    #[test]
    fn app_list_edits_touch_one_element() {
        let (_dir, svc) = test_service();
        let os = OsKind::Macos;
        let app = |id: &str, name: &str| ExcludedApp {
            os,
            app_id: id.to_owned(),
            name: name.to_owned(),
        };
        let ids = |svc: &SettingsService| -> Vec<String> {
            svc.get()
                .clipboard
                .excluded_apps
                .iter()
                .map(|a| format!("{}={}", a.app_id, a.name))
                .collect()
        };
        svc.update(&mut |s| {
            s.clipboard.excluded_apps.clear();
            s.hotkeys.app_keymaps.clear();
        })
        .expect("start empty");
        svc.excluded_app_upsert(app("com.a", "A")).expect("add");
        // 设置窗口打开之后，另一台设备排除了一个密码管理器
        svc.apply_synced(ConfigFile::Settings, &mut |doc| {
            let mut doc = doc.clone();
            doc["clipboard"]["excluded_apps"]
                .as_array_mut()
                .expect("list")
                .push(json!({"os": "macos", "app_id": "com.vault", "name": "Vault"}));
            Some(doc)
        })
        .expect("apply");
        svc.excluded_app_upsert(app(" com.c ", "C")).expect("add");
        svc.excluded_app_upsert(app("com.a", "Renamed")).expect("rename");
        assert_eq!(ids(&svc), ["com.a=Renamed", "com.vault=Vault", "com.c=C"]);
        svc.excluded_app_remove(os, "com.a").expect("remove");
        svc.excluded_app_remove(os, "com.gone").expect("已经不在了也算成功");
        svc.excluded_app_remove(OsKind::Windows, "com.c")
            .expect("其他系统的同名条目不受影响");
        assert_eq!(ids(&svc), ["com.vault=Vault", "com.c=C"]);
        let err = svc.excluded_app_upsert(app("  ", "Blank")).expect_err("empty");
        assert_eq!(err.code, crate::error::IpcErrorCode::InvalidArgument);

        let keymap = |id: &str, paste: &str| AppKeymap {
            os,
            app_id: id.to_owned(),
            copy: None,
            paste: Some(paste.to_owned()),
        };
        svc.app_keymap_upsert(keymap("term", "Control+Shift+KeyV"))
            .expect("add");
        svc.app_keymap_upsert(keymap("other", "Control+KeyV")).expect("add");
        svc.app_keymap_upsert(keymap("term", "Shift+Insert")).expect("replace");
        let maps = svc.get().hotkeys.app_keymaps.clone();
        assert_eq!(maps.len(), 2);
        assert_eq!(
            (maps[0].app_id.as_str(), maps[0].paste.as_deref()),
            ("term", Some("Shift+Insert"))
        );
        svc.app_keymap_remove(os, "term").expect("remove");
        assert_eq!(svc.get().hotkeys.app_keymaps.len(), 1);
        assert_eq!(svc.get().hotkeys.app_keymaps[0].app_id, "other");
    }

    /// 导入与同步应用同时发生：导入在术语表的锁内读取最新的表再合并，同步刚加进来的术语不会被整表覆盖掉
    #[test]
    fn glossary_import_merges_against_the_latest_entries() {
        let (_dir, svc) = test_service();
        svc.glossary_set(vec![gentry("token", "令牌")]).expect("seed");
        let guard = svc.glossary_lock.lock();
        let importer = {
            let svc = Arc::clone(&svc);
            std::thread::spawn(move || svc.glossary_import(vec![gentry("LLM", "大模型")]))
        };
        // 导入已经开始；这期间另一处（同步应用）在同一把锁内加了一条
        std::thread::sleep(std::time::Duration::from_millis(50));
        svc.store_glossary_locked(
            vec![gentry("token", "令牌"), gentry("Remote", "远端")],
            ChangeOrigin::Sync,
        )
        .expect("synced");
        drop(guard);
        assert_eq!(importer.join().expect("join").expect("import"), 1);
        assert_eq!(
            *svc.glossary_entries(),
            vec![
                gentry("token", "令牌"),
                gentry("Remote", "远端"),
                gentry("LLM", "大模型")
            ]
        );
    }

    type SecretCalls = Arc<Mutex<Vec<(String, Option<String>)>>>;

    fn record_secrets(svc: &SettingsService) -> SecretCalls {
        let calls: SecretCalls = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&calls);
        svc.set_secret_hook(Some(Arc::new(move |id: &str, key: Option<&str>| {
            sink.lock().push((id.to_owned(), key.map(str::to_owned)));
        })));
        calls
    }

    fn service_with_backend() -> (
        tempfile::TempDir,
        Arc<SettingsService>,
        Arc<crate::settings::keychain::tests::MemoryBackend>,
    ) {
        use crate::settings::keychain::SecretBackend;
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = Arc::new(crate::settings::keychain::tests::MemoryBackend::default());
        let b = Arc::clone(&backend);
        let svc = SettingsService::load_with(
            &test_paths(dir.path()),
            Some(Box::new(move || Ok(Box::new(Arc::clone(&b)) as Box<dyn SecretBackend>))),
        );
        (dir, svc, backend)
    }

    #[test]
    fn secret_hook_fires_only_for_user_key_changes() {
        use std::sync::atomic::Ordering;
        let (_dir, svc, backend) = service_with_backend();
        let calls = record_secrets(&svc);
        svc.secret_set("p", "  sk-new \n").expect("set");
        assert_eq!(backend.items.lock().get("p").map(String::as_str), Some("sk-new"));
        svc.secret_delete("p").expect("delete");
        assert_eq!(
            *calls.lock(),
            [("p".to_owned(), Some("sk-new".to_owned())), ("p".to_owned(), None)],
            "与写入钥匙串的值一致（去掉首尾空白）"
        );
        calls.lock().clear();
        // 同步写入 / 删除远端的 Key：不调用钩子
        assert!(
            svc.secret_apply_synced("q", Some("sk-remote"), &|| true)
                .expect("apply")
        );
        assert_eq!(backend.items.lock().get("q").map(String::as_str), Some("sk-remote"));
        assert!(svc.keychain().has("q"));
        // 锁内判定不允许（本机刚改过）：什么都不写
        assert!(!svc.secret_apply_synced("q", None, &|| false).expect("refused"));
        assert!(svc.keychain().has("q"));
        assert!(svc.secret_apply_synced("q", None, &|| true).expect("apply delete"));
        assert!(backend.items.lock().get("q").is_none());
        assert!(calls.lock().is_empty());
        // 钥匙串拒绝：没有写入，也不报告
        backend.deny.store(true, Ordering::SeqCst);
        assert_eq!(
            svc.secret_set("p", "sk-x").expect_err("denied").code,
            IpcErrorCode::Keychain
        );
        assert!(calls.lock().is_empty());
        backend.deny.store(false, Ordering::SeqCst);
        // 卸载之后不再调用
        svc.set_secret_hook(None);
        svc.secret_set("p", "sk-y").expect("set");
        assert!(calls.lock().is_empty());
    }

    #[test]
    fn provider_delete_reports_the_key_and_orphan_purge_does_not() {
        use std::sync::atomic::Ordering;
        let (_dir, svc, backend) = service_with_backend();
        svc.update(&mut |s| s.providers = vec![provider("p", &["m"]), provider("q", &["m"])])
            .expect("seed");
        svc.secret_set("p", "sk-p").expect("p");
        svc.secret_set("q", "sk-q").expect("q");
        let calls = record_secrets(&svc);
        // 钥匙串拒绝删除：服务照样删掉，Key 记为待清理，同样报告删除
        backend.deny.store(true, Ordering::SeqCst);
        let (_, key_removed) = svc.provider_delete("p").expect("delete");
        assert!(!key_removed);
        assert_eq!(*calls.lock(), [("p".to_owned(), None)]);
        calls.lock().clear();
        // 之后的清理只是本机的事，不报告
        backend.deny.store(false, Ordering::SeqCst);
        assert_eq!(svc.purge_orphan_keys(), 1);
        assert!(calls.lock().is_empty());
        // 正常删除同样报告
        let (_, key_removed) = svc.provider_delete("q").expect("delete");
        assert!(key_removed);
        assert_eq!(*calls.lock(), [("q".to_owned(), None)]);
    }

    #[test]
    fn sync_credentials_use_memory_when_the_keychain_is_replaced() {
        use crate::settings::sync_credential::SyncCredential;
        let (_dir, svc, backend) = service_with_backend();
        let credential = SyncCredential {
            server_url: "https://dav.example.com/dav".to_owned(),
            username: "me".to_owned(),
            password: "pw".to_owned(),
            vault_id: "0123456789abcdef".to_owned(),
            key: "k".to_owned(),
        };
        svc.sync_credentials().set(&credential).expect("set");
        assert_eq!(svc.sync_credentials().get().expect("get"), Some(credential));
        assert!(backend.items.lock().is_empty(), "不与模型服务的 Key 混在一起");
        assert!(!svc.keychain().has("account"));
    }
}
