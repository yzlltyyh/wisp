//! 热加载：notify 监听配置文件（FSEvents / ReadDirectoryChangesW / inotify，事件驱动，空闲零唤醒），
//! 按文件去抖后调用 [`SettingsService::reload`]。
//!
//! - **Windows 的空闲零唤醒靠 vendor/notify 的补丁**：上游 notify 8.2.0 的 ReadDirectoryChangesW 服务线程每 100 ms 超时醒
//!   一次（监听常驻，空闲主进程因此约 10 次/秒唤醒，空闲预算是 0.2 次/秒）。补丁改为无限期的可警报
//!   等待，单测 `notify_is_patched_so_the_windows_watcher_sleeps_until_an_event` 守着它；
//!
//! - **监听什么**（[`watch_targets`]）：Windows / Linux 监听**配置目录**——inotify 按 inode 监听单个文件，编辑器「写临时
//!   文件再 rename」保存一次之后就失效；那里的配置目录也不和数据目录共用。**macOS** 的配置目录与数据目录是同一个
//!   （`~/Library/Application Support/wisp`），监听目录会让 clipboard.db / -wal / -shm 的每次写入、`blobs/`、
//!   `updates/` 下的变化都唤醒 FSEvents 线程（实测：50 次 WAL 追加写 → 5 次回调，写一张缩略图 → 2 次），因此只监听
//!   三个配置文件本身：FSEvents 按路径匹配，rename 覆盖、删除后重建都照样收到事件，而兄弟文件的写入是 0 次回调。
//!   配置目录里三份文件始终存在（`SettingsService::load` 缺失时写出默认内容）；任何一个文件监听失败时退回监听目录；
//! - 只关心 settings.json / styles.json / glossary.json（state.json、`*.tmp`、`*.bak-*` 的事件直接丢弃）；
//! - 去抖：最后一个事件之后安静 [`DEBOUNCE`] 再加载；持续写入时最迟 [`MAX_DELAY`] 也会加载一次，保证「手改后 1 s 内
//!   所有窗口收到 `settings:changed`」（加载后由 `app::reactor` 广播）；
//! - 自己写入的内容由 `reload` 按内容哈希识别并忽略；JSON 非法时保留旧值并记 `SettingsIssueKind::InvalidJson`，
//!   设置窗口据 `RuntimeInfo.settings_issue` 提示（i18n `settings.invalid_file`）；
//! - 文件暂时读不了（Windows 上杀毒软件 / 编辑器以独占方式短暂打开它，`ReloadOutcome::Busy`）时把它重新放回去抖队列，
//!   再等一个 [`DEBOUNCE`] 周期读一次，最多 [`MAX_BUSY_RETRIES`] 次——外部修改不会因此丢掉。

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Duration;

use notify::{EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio::time::Instant;

use parking_lot::Mutex;
use wisp_types::OsKind;

use crate::app::paths::Paths;
use crate::error::IpcError;
use crate::settings::SettingsService;
use crate::settings::service::{ConfigFile, ReloadOutcome};

/// 最后一个事件之后的安静期
pub const DEBOUNCE: Duration = Duration::from_millis(300);
/// 持续有事件时最多等待多久就加载一次
pub const MAX_DELAY: Duration = Duration::from_millis(700);
/// 文件暂时读不了时最多重新排队几次（每次再等一个去抖周期）
pub const MAX_BUSY_RETRIES: u8 = 5;

/// 要监听的路径（见模块说明）：macOS 为三个配置文件，其他平台为配置目录
pub fn watch_targets(os: OsKind, paths: &Paths) -> Vec<PathBuf> {
    match os {
        OsKind::Macos => ConfigFile::ALL.iter().map(|f| f.path(paths)).collect(),
        OsKind::Windows | OsKind::Linux => vec![paths.config_dir.clone()],
    }
}

/// 一次加载的结果是否要重新排队（`Busy` 且次数未用完）；`attempts` 记每个文件连续 `Busy` 的次数
pub fn requeue_after(attempts: &mut HashMap<ConfigFile, u8>, file: ConfigFile, outcome: ReloadOutcome) -> bool {
    if outcome != ReloadOutcome::Busy {
        attempts.remove(&file);
        return false;
    }
    let n = attempts.entry(file).or_insert(0);
    *n += 1;
    if *n > MAX_BUSY_RETRIES {
        attempts.remove(&file);
        log::warn!(
            "{} stayed busy after {MAX_BUSY_RETRIES} retries; the external change will be loaded on its next save",
            file.file_name()
        );
        return false;
    }
    true
}

/// 监听句柄：drop 时停止监听并结束去抖任务
pub struct SettingsWatcher {
    _watcher: notify::RecommendedWatcher,
    task: tauri::async_runtime::JoinHandle<()>,
}

impl std::fmt::Debug for SettingsWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsWatcher").finish_non_exhaustive()
    }
}

impl Drop for SettingsWatcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl SettingsWatcher {
    /// 开始监听配置目录（目录由 `SettingsService::load` 创建）
    pub fn start(service: &Arc<SettingsService>) -> Result<Self, IpcError> {
        let (tx, rx) = mpsc::unbounded_channel::<ConfigFile>();
        let retry_tx = tx.clone();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(ev) => {
                for file in classify(&ev) {
                    // 接收端已关闭（应用退出）时忽略
                    let _ = tx.send(file);
                }
            }
            Err(e) => log::warn!("settings watcher error: {e}"),
        })
        .map_err(|e| IpcError::internal(format!("cannot create settings watcher: {e}")))?;
        let dir = service.paths().config_dir.clone();
        let targets = watch_targets(OsKind::current(), service.paths());
        let watched_files = targets.iter().all(|t| *t != dir);
        let failed = targets
            .iter()
            .find_map(|t| watcher.watch(t, RecursiveMode::NonRecursive).err().map(|e| (t, e)));
        if let Some((target, e)) = failed {
            if !watched_files {
                return Err(IpcError::internal(format!("cannot watch config dir: {e}")));
            }
            // 某个配置文件监听失败（例如刚被删掉）：退回监听整个目录
            log::warn!(
                "cannot watch {} ({e}); watching the config directory instead",
                target.display()
            );
            for t in &targets {
                let _ = watcher.unwatch(t);
            }
            watcher
                .watch(&dir, RecursiveMode::NonRecursive)
                .map_err(|e| IpcError::internal(format!("cannot watch config dir: {e}")))?;
        }
        let weak: Weak<SettingsService> = Arc::downgrade(service);
        let busy = Arc::new(Mutex::new(HashMap::new()));
        let task = tauri::async_runtime::spawn(debounce(rx, DEBOUNCE, MAX_DELAY, move |batch| {
            let (weak, busy, retry_tx) = (weak.clone(), Arc::clone(&busy), retry_tx.clone());
            async move {
                let Some(service) = weak.upgrade() else { return };
                // 读文件 + 解析是阻塞的小 IO，放到阻塞线程池，不占 tokio worker
                let result = tauri::async_runtime::spawn_blocking(move || {
                    batch
                        .into_iter()
                        .map(|file| {
                            let outcome = service.reload(file);
                            log::debug!("settings watcher: {} -> {outcome:?}", file.file_name());
                            (file, outcome)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
                match result {
                    Ok(outcomes) => {
                        let mut attempts = busy.lock();
                        for (file, outcome) in outcomes {
                            // 暂时读不了：放回去抖队列，再等一个周期
                            if requeue_after(&mut attempts, file, outcome) {
                                let _ = retry_tx.send(file);
                            }
                        }
                    }
                    Err(e) => log::warn!("settings reload task failed: {e}"),
                }
            }
        }));
        log::info!(
            "settings watcher started ({})",
            if watched_files {
                "config files"
            } else {
                "config directory"
            }
        );
        Ok(Self {
            _watcher: watcher,
            task,
        })
    }
}

/// 从 notify 事件中挑出参与热加载的配置文件（只看文件名；读访问事件忽略）
pub fn classify(ev: &notify::Event) -> BTreeSet<ConfigFile> {
    if matches!(ev.kind, EventKind::Access(_)) {
        return BTreeSet::new();
    }
    ev.paths
        .iter()
        .filter_map(|p| p.file_name()?.to_str())
        .filter_map(ConfigFile::from_file_name)
        .collect()
}

/// 下一次加载的时刻：最后一个事件（`now`）之后安静 `quiet`，但不晚于这批第一个事件（`started`）之后 `max_delay`
pub fn next_deadline(started: Instant, now: Instant, quiet: Duration, max_delay: Duration) -> Instant {
    (now + quiet).min(started + max_delay)
}

/// 去抖循环：收到第一个事件后，等到安静 `quiet` 或累计等待 `max_delay`，把这段时间内涉及的文件一次交给 `on_batch`。
/// 没有事件时只是挂在 `recv` 上，不产生任何唤醒
pub async fn debounce<F, Fut>(
    mut rx: mpsc::UnboundedReceiver<ConfigFile>,
    quiet: Duration,
    max_delay: Duration,
    mut on_batch: F,
) where
    F: FnMut(BTreeSet<ConfigFile>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    while let Some(first) = rx.recv().await {
        let mut batch = BTreeSet::from([first]);
        let started = Instant::now();
        loop {
            let deadline = next_deadline(started, Instant::now(), quiet, max_delay);
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(file)) => {
                    batch.insert(file);
                }
                // 发送端已关闭：处理完手上这批就结束
                Ok(None) => {
                    on_batch(batch).await;
                    return;
                }
                Err(_) => break,
            }
        }
        on_batch(batch).await;
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Instant as StdInstant;

    use notify::event::{AccessKind, CreateKind, ModifyKind};
    use serde_json::json;

    use super::*;
    use crate::settings::service::tests::test_service;

    fn event(kind: EventKind, names: &[&str]) -> notify::Event {
        let mut ev = notify::Event::new(kind);
        for n in names {
            ev = ev.add_path(PathBuf::from("/cfg").join(n));
        }
        ev
    }

    #[test]
    fn classify_keeps_only_config_documents() {
        let ev = event(
            EventKind::Modify(ModifyKind::Any),
            &[
                "settings.json",
                "settings.json.tmp",
                "state.json",
                "styles.json",
                "settings.json.bak-1700000000",
            ],
        );
        assert_eq!(
            classify(&ev),
            BTreeSet::from([ConfigFile::Settings, ConfigFile::Styles])
        );
        assert!(classify(&event(EventKind::Access(AccessKind::Any), &["settings.json"])).is_empty());
        assert_eq!(
            classify(&event(EventKind::Create(CreateKind::File), &["glossary.json"])),
            BTreeSet::from([ConfigFile::Glossary])
        );
    }

    /// 驱动去抖循环：`sends` 为（距上一次发送的间隔 ms，文件）；间隔为 0 的连续发送之间不让出执行权，
    /// 因此必然落在同一批里（与机器快慢无关）。返回每批的（距开始的 ms，文件集合）
    fn run_debounce(sends: Vec<(u64, ConfigFile)>, quiet_ms: u64, max_ms: u64) -> Vec<(u128, BTreeSet<ConfigFile>)> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async move {
            let (tx, rx) = mpsc::unbounded_channel();
            let batches = Arc::new(Mutex::new(Vec::new()));
            let out = Arc::clone(&batches);
            let t0 = StdInstant::now();
            let task = tokio::spawn(debounce(
                rx,
                Duration::from_millis(quiet_ms),
                Duration::from_millis(max_ms),
                move |b| {
                    out.lock().expect("lock").push((t0.elapsed().as_millis(), b));
                    async {}
                },
            ));
            for (delay, file) in sends {
                if delay > 0 {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
                tx.send(file).expect("send");
            }
            tokio::time::sleep(Duration::from_millis(quiet_ms + 80)).await;
            drop(tx);
            task.await.expect("join");
            batches.lock().expect("lock").clone()
        })
    }

    /// notify 走 vendor/notify 的补丁（见 vendor/README.md）：Cargo.lock 里的 notify 来自 vendor（没有
    /// `source`）、版本与补丁一致，Windows 服务线程无限期等待、`configure` 之后补一次唤醒。升级 notify 时这里失败：先看上游
    /// （notify-rs/notify）是否已去掉 100 ms 的超时——去掉了就删除根 Cargo.toml `[patch.crates-io]` 的 notify 与 vendor/notify，
    /// 否则把补丁移植到新版本；两种情况都在 Windows 上用 `scripts/perf/windows-thread-wakeups.ps1` 复测空闲唤醒
    #[test]
    fn notify_is_patched_so_the_windows_watcher_sleeps_until_an_event() {
        let lock = include_str!("../../../../../Cargo.lock");
        let notify = lock
            .split("[[package]]")
            .find(|p| p.contains("\nname = \"notify\"\n"))
            .expect("Cargo.lock 里没有 notify");
        assert!(
            notify.contains("\nversion = \"8.2.0\"\n"),
            "notify 版本变了：核对上游是否已去掉 Windows 后端的 100 ms 超时，移植或删除 vendor/notify 的补丁（见本测试注释）"
        );
        assert!(
            !notify.contains("\nsource = "),
            "notify 没有走 vendor/notify 的补丁：根 Cargo.toml 的 [patch.crates-io] 丢了？"
        );
        let windows = include_str!("../../../../../vendor/notify/src/windows.rs");
        assert!(
            windows.contains("WaitForSingleObjectEx(self.wakeup_sem, INFINITE, 1)"),
            "vendor/notify 的服务线程不再无限期等待"
        );
        assert!(
            !windows.contains("WaitForSingleObjectEx(self.wakeup_sem, 100, 1)"),
            "vendor/notify 又出现了 100 ms 的轮询"
        );
        let configure = windows
            .split("fn configure(&mut self, config: Config)")
            .nth(1)
            .and_then(|rest| rest.split("fn kind()").next())
            .expect("vendor/notify 没有 configure");
        assert!(
            configure.contains("self.wakeup_server();"),
            "vendor/notify 的 configure 没有唤醒服务线程（无限期等待时会永远卡住）"
        );
    }

    #[test]
    fn macos_watches_only_the_config_files() {
        let paths = Paths::under(std::path::Path::new("/root"));
        let mac = watch_targets(OsKind::Macos, &paths);
        assert_eq!(mac.len(), 3);
        assert!(mac.iter().all(|p| p.parent() == Some(paths.config_dir.as_path())));
        assert!(mac.contains(&paths.settings_file()));
        assert!(!mac.contains(&paths.state_file()), "state.json 不参与热加载");
        for os in [OsKind::Windows, OsKind::Linux] {
            assert_eq!(watch_targets(os, &paths), vec![paths.config_dir.clone()]);
        }
    }

    #[test]
    fn busy_files_are_retried_a_bounded_number_of_times() {
        let mut attempts = HashMap::new();
        for _ in 0..MAX_BUSY_RETRIES {
            assert!(requeue_after(&mut attempts, ConfigFile::Settings, ReloadOutcome::Busy));
        }
        assert!(
            !requeue_after(&mut attempts, ConfigFile::Settings, ReloadOutcome::Busy),
            "次数用完后不再重试"
        );
        // 计数已清零：下一次外部修改重新计数
        assert!(requeue_after(&mut attempts, ConfigFile::Settings, ReloadOutcome::Busy));
        // 读成功即清零；其他文件互不影响
        assert!(!requeue_after(
            &mut attempts,
            ConfigFile::Settings,
            ReloadOutcome::Applied
        ));
        assert!(attempts.is_empty());
        assert!(!requeue_after(
            &mut attempts,
            ConfigFile::Styles,
            ReloadOutcome::Unchanged
        ));
    }

    #[test]
    fn deadline_is_the_quiet_period_capped_by_max_delay() {
        let t0 = Instant::now();
        let quiet = Duration::from_millis(300);
        let max = Duration::from_millis(700);
        // 刚开始：安静期决定
        assert_eq!(next_deadline(t0, t0, quiet, max), t0 + quiet);
        assert_eq!(
            next_deadline(t0, t0 + Duration::from_millis(100), quiet, max),
            t0 + Duration::from_millis(400)
        );
        // 持续有事件：不晚于 started + max_delay
        assert_eq!(next_deadline(t0, t0 + Duration::from_millis(600), quiet, max), t0 + max);
        assert_eq!(next_deadline(t0, t0 + Duration::from_secs(5), quiet, max), t0 + max);
    }

    #[test]
    fn bursts_are_coalesced_into_one_reload() {
        let batches = run_debounce(
            vec![
                (0, ConfigFile::Settings),
                (0, ConfigFile::Settings),
                (0, ConfigFile::Styles),
                (0, ConfigFile::Settings),
            ],
            60,
            1000,
        );
        assert_eq!(batches.len(), 1, "{batches:?}");
        assert_eq!(batches[0].1, BTreeSet::from([ConfigFile::Settings, ConfigFile::Styles]));
        // 计时器只会晚、不会早：安静期过后才加载
        assert!(batches[0].0 >= 60, "安静期之后才加载：{batches:?}");
    }

    #[test]
    fn continuous_writes_still_reload_within_max_delay() {
        // 每 20 ms 一个事件、持续 300 ms；安静期 400 ms 在这期间不可能等到，只有 max_delay（60 ms）能触发加载
        let sends: Vec<(u64, ConfigFile)> = (0..15).map(|_| (20, ConfigFile::Settings)).collect();
        let batches = run_debounce(sends, 400, 60);
        assert!(batches.len() >= 2, "持续写入期间也要分批加载：{batches:?}");
    }

    #[test]
    fn separate_edits_reload_separately() {
        // 两次编辑相隔 400 ms，远大于 30 ms 的安静期
        let batches = run_debounce(vec![(0, ConfigFile::Settings), (400, ConfigFile::Glossary)], 30, 1000);
        assert_eq!(batches.len(), 2, "{batches:?}");
        assert_eq!(batches[0].1, BTreeSet::from([ConfigFile::Settings]));
        assert_eq!(batches[1].1, BTreeSet::from([ConfigFile::Glossary]));
    }

    /// 外部改写 settings.json 的 font_size，等到热加载生效；返回耗时（`limit` 内没生效返回 None）
    fn edit_and_wait(svc: &SettingsService, font_size: u32, limit: Duration) -> Option<Duration> {
        let path = svc.paths().settings_file();
        let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("json");
        v["appearance"]["font_size"] = json!(font_size);
        let t0 = StdInstant::now();
        std::fs::write(&path, serde_json::to_vec_pretty(&v).expect("json")).expect("write");
        while svc.get().appearance.font_size != font_size {
            if t0.elapsed() > limit {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Some(t0.elapsed())
    }

    /// 端到端：外部改写 settings.json 后生效（真实的文件系统事件）。
    /// 监听刚启动时 FSEvents 的流可能还没开始投递：先做一次「预热」编辑并等它生效（必要时重写），再计时。
    /// 「1 s 内」是产品要求，只在本机断言；CI（`CI` 环境变量）上机器繁忙，只要求在宽松期限内生效
    #[test]
    fn external_edit_is_applied_within_a_second() {
        let (_dir, svc) = test_service();
        let watcher = SettingsWatcher::start(&svc).expect("watcher");
        let warmed = (0..5).any(|_| edit_and_wait(&svc, 16, Duration::from_secs(2)).is_some());
        assert!(warmed, "监听没有生效：外部修改始终没有被热加载");
        let elapsed = edit_and_wait(&svc, 17, Duration::from_secs(10)).expect("外部修改没有被热加载");
        if std::env::var_os("CI").is_none() {
            assert!(elapsed < Duration::from_secs(1), "热加载耗时 {elapsed:?}");
        }
        // 自己写入不会被当作外部修改反复加载（只验证不会把值改回去）
        svc.patch(json!({"appearance": {"font_size": 15}})).expect("patch");
        std::thread::sleep(DEBOUNCE + Duration::from_millis(200));
        assert_eq!(svc.get().appearance.font_size, 15);
        // 编辑器式保存（写临时文件再 rename 覆盖）之后监听仍然有效——macOS 上监听的是文件路径，不是 inode
        let path = svc.paths().settings_file();
        // （字号要在允许范围内，否则会被夹紧成与当前相同的值）
        for (round, size) in [13u32, 14].into_iter().enumerate() {
            let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("json");
            v["appearance"]["font_size"] = json!(size);
            let swap = path.with_file_name(format!(".settings.json.swp{round}"));
            std::fs::write(&swap, serde_json::to_vec_pretty(&v).expect("json")).expect("write swap");
            std::fs::rename(&swap, &path).expect("rename");
            let t0 = StdInstant::now();
            while svc.get().appearance.font_size != size {
                assert!(
                    t0.elapsed() < Duration::from_secs(10),
                    "rename 保存第 {round} 次没有被热加载"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        drop(watcher);
    }
}
