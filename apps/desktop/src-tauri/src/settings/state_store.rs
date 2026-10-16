//! state.json（运行态）：窗口位置、引导是否完成、上次检查更新时间等，不污染用户配置。
//!
//! 读写规则：启动时读取（不存在 → 默认值；损坏 → 备份为 `state.json.bak-<unix>` 并用默认值，只记日志——运行态丢失
//! 不需要打扰用户）；[`StateStore::update`] 改内存并**同步原子写盘**（内容没变时不写；**阻塞**，异步上下文里经
//! `run_blocking` 调用）；高频的小更新（用量统计）走 [`StateStore::update_deferred`]：只改内存，1 s 后在阻塞线程池里
//! 合并写一次（一次性定时，空闲时不存在任何定时器），退出时 [`StateStore::flush`] 写掉未落盘的内容。
//! state.json 不参与热加载。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};

use crate::error::IpcError;
use crate::settings::{fsio, migrate};

/// 合并写入的延迟
pub const DEFERRED_WRITE_DELAY: Duration = Duration::from_secs(1);
/// 用量统计保留的天数（更早的按天删除）
pub const USAGE_KEEP_DAYS: usize = 400;

/// 窗口位置与尺寸（逻辑 px，所在显示器）
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowGeometry {
    pub monitor_id: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// 悬浮剪贴板记住的位置、大小（显示器 id + 本屏逻辑坐标的卡片矩形）与置顶。只属于本机，不同步
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipFloatState {
    pub geometry: Option<WindowGeometry>,
    /// 默认置顶
    pub on_top: bool,
}

impl Default for ClipFloatState {
    fn default() -> Self {
        Self {
            geometry: None,
            on_top: true,
        }
    }
}

/// 记住的位置
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Position {
    pub x: f64,
    pub y: f64,
}

/// 已见过的授权状态（检测「更新后授权失效」）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionsSeen {
    pub accessibility: bool,
    pub screen_recording: bool,
}

/// 用量统计
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageCounter {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// state.json 的结构
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StateFile {
    pub schema_version: u32,
    pub onboarding_done: bool,
    pub onboarding_version: u32,
    pub clipboard_paused: bool,
    /// 剪贴板列表顶部的「收藏」分组已折叠（只属于本机，不同步）
    pub clipboard_favorites_collapsed: bool,
    pub settings_window: Option<WindowGeometry>,
    /// 按显示器记住的剪贴板面板位置
    pub clipboard_positions: BTreeMap<String, Position>,
    pub last_update_check_ms: i64,
    pub last_vacuum_ms: i64,
    pub permissions_seen_granted: PermissionsSeen,
    /// 上一次看到授权为 granted 时的应用版本（版本变了且授权从有到无，才判定为「更新后授权失效」）
    pub permissions_seen_version: String,
    pub pinned_on_top_default: bool,
    /// 悬浮剪贴板的位置、大小与置顶
    pub clip_float: ClipFloatState,
    /// `<yyyy-mm-dd>`（UTC）→ `<provider_id>/<model_id>` → 计数
    pub usage: BTreeMap<String, BTreeMap<String, UsageCounter>>,
    /// 已在系统钥匙串里保存了 Key 的 provider_id（只记 id；`has_key` 据此回答，不必读钥匙串，见 `keychain.rs`）
    pub stored_keys: BTreeSet<String>,
    /// Provider 已删除、但钥匙串条目当时没删掉（拒绝授权、钥匙串锁定）的 provider_id：之后的显式钥匙串操作时重试清理，
    /// 新建 Provider 时也不会复用这些 id（见 `SettingsService::provider_delete`）
    pub orphan_keys: BTreeSet<String>,
    /// 重启续接：`app_relaunch(resume)` 写入，下次启动时取走一次（`app::relaunch`）
    pub resume: Option<crate::app::relaunch::ResumeRequest>,
    /// 同步用的本机设备 id（16 位小写十六进制）：只属于这台机器，断开同步后再连接时沿用；发现数据目录被复制到另一台
    /// 机器时由同步模块换新（见 [`StateStore::sync_device_id`]）
    pub sync_device_id: Option<String>,
}

impl Default for StateFile {
    fn default() -> Self {
        Self {
            schema_version: migrate::DOC_SCHEMA_VERSION,
            onboarding_done: false,
            onboarding_version: 1,
            clipboard_paused: false,
            clipboard_favorites_collapsed: false,
            settings_window: None,
            clipboard_positions: BTreeMap::new(),
            last_update_check_ms: 0,
            last_vacuum_ms: 0,
            permissions_seen_granted: PermissionsSeen::default(),
            permissions_seen_version: String::new(),
            pinned_on_top_default: true,
            clip_float: ClipFloatState::default(),
            usage: BTreeMap::new(),
            stored_keys: BTreeSet::new(),
            orphan_keys: BTreeSet::new(),
            resume: None,
            sync_device_id: None,
        }
    }
}

/// state.json 的读写
#[derive(Debug)]
pub struct StateStore {
    path: PathBuf,
    inner: RwLock<StateFile>,
    /// 启动时 state.json 不存在或已损坏（内容是默认值）：其中的派生索引（`stored_keys`）不可信，见 [`StateStore::was_reset`]
    reset: bool,
    /// state.json 来自更新的版本：写盘时保留本版本不认识的内容与原来的版本号（见 `migrate::Newer`）
    newer: Option<migrate::Newer>,
    /// 串行化写盘
    write: Mutex<()>,
    /// 内存里有尚未落盘的修改
    dirty: AtomicBool,
    /// 已安排了一次合并写入
    scheduled: Arc<AtomicBool>,
}

impl StateStore {
    fn with(path: PathBuf, file: StateFile, reset: bool, newer: Option<migrate::Newer>) -> Self {
        Self {
            path,
            inner: RwLock::new(file),
            reset,
            newer,
            write: Mutex::new(()),
            dirty: AtomicBool::new(false),
            scheduled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 读取 state.json（不存在 → 默认值；损坏 → 备份并用默认值）
    pub fn load(path: PathBuf) -> Self {
        let (file, reset, newer) = match fsio::load_document::<StateFile>(&path, |v| {
            migrate::run(v, migrate::DOC_SCHEMA_VERSION, migrate::STATE_STEPS)
        }) {
            Ok(loaded) => {
                if let Some(backup) = &loaded.backup {
                    log::warn!("state.json is corrupt; backed up to {} and reset", backup.display());
                }
                for issue in &loaded.issues {
                    log::warn!("state.json: {issue}");
                }
                let reset = !loaded.existed || loaded.backup.is_some();
                (loaded.value, reset, loaded.newer)
            }
            Err(e) => {
                log::warn!("state.json could not be read ({e}); using defaults");
                (StateFile::default(), true, None)
            }
        };
        Self::with(path, file, reset, newer)
    }

    /// 启动时 state.json 不存在、读不了或已损坏而改用了默认值（首次运行也是 true）。此时 `stored_keys` 等派生索引是空的，
    /// 与钥匙串的实际内容可能不一致，需要按需重建（`SettingsService::verify_key_index`）
    pub fn was_reset(&self) -> bool {
        self.reset
    }

    /// 文件路径
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 当前内容的副本
    pub fn get(&self) -> StateFile {
        self.inner.read().clone()
    }

    /// 只读访问（避免整份克隆）
    pub fn read<R>(&self, f: impl FnOnce(&StateFile) -> R) -> R {
        f(&self.inner.read())
    }

    /// 修改并写盘（内容没变时不写）。**阻塞**（序列化整份 + fsync）：异步上下文里经 `run_blocking` 调用
    pub fn update(&self, f: &mut dyn FnMut(&mut StateFile)) -> Result<(), IpcError> {
        let changed = {
            let mut guard = self.inner.write();
            let before = guard.clone();
            f(&mut guard);
            *guard != before
        };
        if changed || self.dirty.load(Ordering::Acquire) {
            self.dirty.store(true, Ordering::Release);
            self.flush()?;
        }
        Ok(())
    }

    /// 修改内存，[`DEFERRED_WRITE_DELAY`] 后合并写盘（高频的小更新用）
    pub fn update_deferred(self: &Arc<Self>, f: impl FnOnce(&mut StateFile)) {
        {
            let mut guard = self.inner.write();
            f(&mut guard);
        }
        self.dirty.store(true, Ordering::Release);
        if self.scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        let this = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(DEFERRED_WRITE_DELAY).await;
            // 写盘（整份序列化 + fsync，杀毒软件扫描时可能要几十毫秒）放到阻塞线程池，不占 tokio worker
            let written = tauri::async_runtime::spawn_blocking(move || {
                this.scheduled.store(false, Ordering::Release);
                this.flush()
            })
            .await;
            match written {
                Ok(Ok(())) => {}
                Ok(Err(e)) => log::warn!("state.json deferred write failed: {}", e.message),
                Err(e) => log::warn!("state.json deferred write task failed: {e}"),
            }
        });
    }

    /// 写掉尚未落盘的修改（退出时调用；没有修改时什么也不做）
    pub fn flush(&self) -> Result<(), IpcError> {
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        let _w = self.write.lock();
        let bytes = fsio::to_pretty_json_preserving(&*self.inner.read(), self.newer.as_ref())?;
        if let Err(e) = fsio::write_atomic(&self.path, &bytes) {
            self.dirty.store(true, Ordering::Release);
            return Err(e.into());
        }
        Ok(())
    }

    /// 累计一次请求的用量：按 UTC 日期与 `<provider>/<model>` 记，合并写盘，只保留最近 [`USAGE_KEEP_DAYS`] 天
    pub fn record_usage(self: &Arc<Self>, provider_id: &str, model_id: &str, input_tokens: u64, output_tokens: u64) {
        let day = utc_date(fsio::unix_now_ms());
        let key = format!("{provider_id}/{model_id}");
        self.update_deferred(|s| {
            let counter = s.usage.entry(day).or_default().entry(key).or_default();
            counter.requests = counter.requests.saturating_add(1);
            counter.input_tokens = counter.input_tokens.saturating_add(input_tokens);
            counter.output_tokens = counter.output_tokens.saturating_add(output_tokens);
            while s.usage.len() > USAGE_KEEP_DAYS {
                let Some(oldest) = s.usage.keys().next().cloned() else {
                    break;
                };
                s.usage.remove(&oldest);
            }
        });
    }

    /// 清空用量统计
    pub fn clear_usage(&self) -> Result<(), IpcError> {
        self.update(&mut |s| s.usage.clear())
    }

    /// 同步用的本机设备 id。state.json 里的值不是 16 位小写十六进制（被手改坏）时视为没有，由同步模块重新生成
    pub fn sync_device_id(&self) -> Option<String> {
        self.read(|s| s.sync_device_id.clone())
            .filter(|id| is_sync_device_id(id))
    }

    /// 记下（`Some`）或清除（`None`）本机设备 id 并立即写盘（**阻塞**）。形状不对时 `invalid_argument`
    pub fn set_sync_device_id(&self, id: Option<&str>) -> Result<(), IpcError> {
        if id.is_some_and(|id| !is_sync_device_id(id)) {
            return Err(IpcError::invalid_argument(
                "sync_device_id must be 16 lowercase hexadecimal digits",
            ));
        }
        self.update(&mut |s| s.sync_device_id = id.map(str::to_owned))
    }
}

/// 设备 id 的形状：16 位小写十六进制
pub fn is_sync_device_id(id: &str) -> bool {
    id.len() == 16 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Unix 毫秒 → UTC 日期 `yyyy-mm-dd`（Howard Hinnant 的 civil_from_days）
pub fn utc_date(unix_ms: i64) -> String {
    let days = unix_ms.div_euclid(86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_dates() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(951_782_400_000), "2000-02-29");
        assert_eq!(utc_date(1_760_054_400_000), "2025-10-10");
        assert_eq!(utc_date(-86_400_000), "1969-12-31");
    }

    #[test]
    fn update_persists_and_reloads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let store = StateStore::load(path.clone());
        assert!(!store.get().onboarding_done);
        assert!(store.was_reset(), "文件不存在：派生索引不可信");
        store
            .update(&mut |s| {
                s.onboarding_done = true;
                s.stored_keys.insert("openai-main".to_owned());
            })
            .expect("update");
        let reloaded = StateStore::load(path);
        assert!(!reloaded.was_reset());
        assert!(reloaded.get().onboarding_done);
        assert!(reloaded.get().stored_keys.contains("openai-main"));
    }

    #[test]
    fn unchanged_update_does_not_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let store = StateStore::load(path.clone());
        store.update(&mut |_| {}).expect("noop");
        assert!(!path.exists(), "没有修改时不写盘");
    }

    #[test]
    fn corrupt_state_is_backed_up() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        std::fs::write(&path, b"{oops").expect("write");
        let store = StateStore::load(path);
        assert_eq!(store.get(), StateFile::default());
        assert!(store.was_reset());
        let backups = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("state.json.bak-"))
            .count();
        assert_eq!(backups, 1);
    }

    #[test]
    fn usage_accumulates_and_flushes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let store = Arc::new(StateStore::load(path.clone()));
        store.record_usage("p", "m", 10, 5);
        store.record_usage("p", "m", 1, 2);
        store.flush().expect("flush");
        let reloaded = StateStore::load(path);
        let usage = reloaded.get().usage;
        let day = usage.values().next().expect("one day");
        assert_eq!(
            day.get("p/m"),
            Some(&UsageCounter {
                requests: 2,
                input_tokens: 11,
                output_tokens: 7
            })
        );
        store.clear_usage().expect("clear");
        assert!(store.get().usage.is_empty());
    }

    #[test]
    fn sync_device_id_persists_and_rejects_bad_shapes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let store = StateStore::load(path.clone());
        assert_eq!(store.sync_device_id(), None);
        store.set_sync_device_id(Some("0123456789abcdef")).expect("set");
        assert_eq!(
            StateStore::load(path.clone()).sync_device_id().as_deref(),
            Some("0123456789abcdef"),
            "重启后沿用"
        );
        for bad in [
            "0123456789ABCDEF",
            "0123456789abcde",
            "0123456789abcdefg",
            "g123456789abcdef",
            "",
        ] {
            assert_eq!(
                store.set_sync_device_id(Some(bad)).expect_err("bad").code,
                crate::error::IpcErrorCode::InvalidArgument,
                "{bad}"
            );
        }
        // 手改坏的值读成「没有」
        std::fs::write(&path, br#"{"sync_device_id": "not-an-id"}"#).expect("write");
        assert_eq!(StateStore::load(path.clone()).sync_device_id(), None);
        store.set_sync_device_id(None).expect("clear");
        assert_eq!(store.sync_device_id(), None);
    }

    #[test]
    fn unknown_fields_are_ignored_and_missing_fields_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        std::fs::write(&path, br#"{"onboarding_done": true, "future_field": 1}"#).expect("write");
        let store = StateStore::load(path);
        let s = store.get();
        assert!(s.onboarding_done);
        assert!(s.pinned_on_top_default);
    }
}
