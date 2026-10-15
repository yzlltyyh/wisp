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
//! `glossary_lock`）在外，`file_io` 在内。写入与热加载都按这个顺序加锁。
//!
//! **钥匙串**是阻塞调用：异步上下文里用 [`key_get_blocking`] / [`run_blocking`]。

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::{Mutex, RwLock};
use tokio::sync::watch;
use wisp_prompts::GlossaryMatcher;
use wisp_types::{
    DictMode, GlossaryEntry, GlossaryFile, ModelEntry, ModelRef, PromptPreview, Provider, RuntimeInfo,
    SETTINGS_SCHEMA_VERSION, Settings, SettingsFileIssue, SettingsIssueKind, SettingsSnapshot, Style, StylesFile,
};

use crate::app::paths::Paths;
use crate::error::IpcError;
use crate::settings::caps_store::SettingsCapsStore;
use crate::settings::keychain::{BackendFactory, Keychain};
use crate::settings::state_store::StateStore;
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

        // settings.json
        let settings_path = paths.settings_file();
        let mut newer = HashMap::new();
        let LoadedSettings {
            settings,
            issue: settings_issue,
            rewrite: write_settings,
            hash: settings_hash,
            newer: settings_newer,
        } = load_settings(&settings_path);
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
                let mut value = StylesFile::default();
                styles_store::normalize(&mut value);
                value
            }
        };
        let glossary_file = match glossary_store::load_with_backup(&paths.glossary_file()) {
            Ok(loaded) => {
                note_loaded(ConfigFile::Glossary, &loaded, &mut issues, &mut written, &mut newer);
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
                GlossaryFile::default()
            }
        };

        // state.json 与钥匙串索引
        let state = Arc::new(StateStore::load(paths.state_file()));
        let (revision, revision_rx) = watch::channel(0u64);
        let revision = Arc::new(revision);
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
        let current = self.get();
        if *current == next {
            return Ok(current);
        }
        let bytes = fsio::to_pretty_json(&next)?;
        self.write_file(ConfigFile::Settings, &bytes)?;
        let next = Arc::new(next);
        self.current.send_replace(Arc::clone(&next));
        self.notify_changed();
        Ok(next)
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
        let value = match serde_json::from_slice::<serde_json::Value>(&bytes) {
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
        for fix in validate::check(&mut next) {
            log::warn!("settings.json: {fix}");
        }
        self.note_newer(ConfigFile::Settings, outcome, raw.as_ref(), &next);
        if *self.get() == next {
            return ReloadOutcome::Unchanged;
        }
        log::info!("settings.json reloaded after an external change");
        self.current.send_replace(Arc::new(next));
        self.notify_changed();
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
        self.note_newer(ConfigFile::Styles, outcome, raw.as_ref(), &next);
        styles_store::normalize(&mut next);
        if *file == next {
            return ReloadOutcome::Unchanged;
        }
        log::info!("styles.json reloaded after an external change");
        *self.styles.write() = Arc::new(styles_store::resolve(&next));
        *file = next;
        self.notify_changed();
        ReloadOutcome::Applied
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
        self.note_newer(ConfigFile::Glossary, outcome, raw.as_ref(), &next);
        let entries = glossary_store::normalize(next.entries);
        if *self.glossary_entries() == entries {
            return ReloadOutcome::Unchanged;
        }
        log::info!("glossary.json reloaded after an external change");
        self.install_glossary(entries);
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
            *self.styles.write() = Arc::new(styles_store::resolve(&next));
            *file = next;
            drop(file);
            self.notify_changed();
        }
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

    fn install_glossary(&self, entries: Vec<GlossaryEntry>) {
        *self.glossary.write() = Arc::new(GlossaryMatcher::new(&entries));
        *self.glossary_entries.write() = Arc::new(entries);
        self.notify_changed();
    }

    /// 替换术语表（条目先规范化：去空白、丢弃空 source、按 source 去重）
    pub fn glossary_set(&self, entries: Vec<GlossaryEntry>) -> Result<(), IpcError> {
        let _g = self.glossary_lock.lock();
        let entries = glossary_store::normalize(entries);
        if *self.glossary_entries() == entries {
            return Ok(());
        }
        let file = GlossaryFile {
            schema_version: migrate::DOC_SCHEMA_VERSION,
            entries,
        };
        self.write_file(ConfigFile::Glossary, &glossary_store::encode(&file)?)?;
        self.install_glossary(file.entries);
        Ok(())
    }

    /// 合并导入的条目（同 source 替换、新的追加），返回导入的条数
    pub fn glossary_import(&self, imported: Vec<GlossaryEntry>) -> Result<u32, IpcError> {
        let count = u32::try_from(imported.len()).unwrap_or(u32::MAX);
        let merged = glossary_store::merge(&self.glossary_entries(), imported);
        crate::ipc::validate::validate_glossary(&merged)?;
        self.glossary_set(merged)?;
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
        let key_removed = match self.keychain.delete(provider_id) {
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
}

/// 本平台的新配置（首次运行、损坏重置、读不了文件时）：`Settings::default()` 加平台默认值（
/// `wisp_types::defaults::unload_panel_default`）。已有文件里的值一律按文件，不受影响
fn fresh_settings() -> Settings {
    Settings::for_os(wisp_types::OsKind::current())
}

/// 启动时读取 settings.json
fn load_settings(path: &std::path::Path) -> LoadedSettings {
    let fallback = |rewrite: bool| LoadedSettings {
        settings: fresh_settings(),
        issue: None,
        rewrite,
        hash: None,
        newer: None,
    };
    let bytes = match fsio::read_optional(path) {
        Ok(Some(b)) => b,
        // 首次运行：写出默认内容
        Ok(None) => return fallback(true),
        Err(e) => {
            // 读不了（权限等）：不覆盖用户的文件，本次用默认值
            log::error!("cannot read settings.json ({e}); using defaults without overwriting it");
            return fallback(false);
        }
    };
    let hash = Some(fsio::content_hash(&bytes));
    let parsed = serde_json::from_slice::<serde_json::Value>(&bytes)
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
            for fix in validate::check(&mut settings) {
                log::warn!("settings.json: {fix}");
            }
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
            }
        }
    }
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
        assert_eq!(on_disk["schema_version"], 1);
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
        assert_eq!(read_json(&paths.settings_file())["schema_version"], 1);
    }

    #[test]
    fn lenient_load_keeps_valid_fields_and_does_not_rewrite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(dir.path());
        std::fs::create_dir_all(&paths.config_dir).expect("mkdir");
        let text = r#"{"appearance": {"theme": "Dark", "font_size": 16}, "general": {"start_hidden": false}}"#;
        std::fs::write(paths.settings_file(), text).expect("write");
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
}
