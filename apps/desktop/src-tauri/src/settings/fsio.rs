//! 配置文件的底层读写：原子写入（先写 `<name>.tmp`、fsync，再 rename 覆盖）、损坏文件备份为
//! `<name>.bak-<unix>`（只保留最近 [`MAX_BACKUPS`] 份）、内容哈希（热加载用来识别自己写入的内容）。
//!
//! 所有函数都是同步阻塞的小文件 IO（KB 级），调用方负责串行化同一个文件的写入。

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde::de::DeserializeOwned;
use wisp_types::ConfigIssue;

/// 同一个文件最多保留的损坏备份份数（更早的自动删除，避免反复损坏时无限增长）
pub const MAX_BACKUPS: usize = 5;

/// 内容哈希（blake3）
pub type ContentHash = [u8; 32];

/// 计算内容哈希
pub fn content_hash(bytes: &[u8]) -> ContentHash {
    *blake3::hash(bytes).as_bytes()
}

/// 当前 Unix 时间（毫秒）
pub fn unix_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// 序列化为带缩进、以换行结尾的 JSON（用户可手改的文件统一用这种格式）
pub fn to_pretty_json<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// 原子写入：先写同目录的 `<name>.tmp` 并 fsync，再 rename 覆盖目标。任意时刻强杀进程，目标文件要么是旧内容，
/// 要么是新内容，不会是半截 JSON。目录不存在时先创建
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(dir)?;
    let tmp = tmp_path(path);
    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        rename_with_retry(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
        return result;
    }
    // 目录项落盘（尽力而为：Windows 上不能以只读方式打开目录，失败不影响正确性）
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Windows 上 rename 覆盖目标时的重试间隔（毫秒）：杀毒软件（Defender 会扫描刚写入的文件）或以不共享删除方式打开目标的
/// 编辑器会让 `MoveFileExW` 短暂失败；总共最多等 [`RENAME_BACKOFF_MS`] 之和（约 0.4 s）
pub const RENAME_BACKOFF_MS: [u64; 5] = [10, 25, 50, 100, 200];

/// rename 覆盖目标；Windows 上遇到暂时性的占用错误时按 [`RENAME_BACKOFF_MS`] 退避重试
fn rename_with_retry(from: &Path, to: &Path) -> io::Result<()> {
    let mut delays = RENAME_BACKOFF_MS.iter();
    loop {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if is_transient_sharing_error(wisp_types::OsKind::current(), &e) => {
                let Some(ms) = delays.next() else { return Err(e) };
                log::debug!("rename {} busy ({e}); retrying in {ms} ms", to.display());
                std::thread::sleep(std::time::Duration::from_millis(*ms));
            }
            Err(e) => return Err(e),
        }
    }
}

/// 只有 Windows 的「拒绝访问 / 共享冲突 / 锁冲突」（5 / 32 / 33）是值得重试的暂时性错误（杀毒软件扫描、编辑器以独占
/// 方式短暂打开文件）；其他平台的同号错误码含义不同。rename 与热加载的读文件都据此重试
pub fn is_transient_sharing_error(os: wisp_types::OsKind, e: &io::Error) -> bool {
    os == wisp_types::OsKind::Windows && matches!(e.raw_os_error(), Some(5 | 32 | 33))
}

/// `<name>.tmp`
pub fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

/// 读取文件；不存在时返回 `Ok(None)`
pub fn read_optional(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// 把损坏的文件改名为 `<name>.bak-<unix 秒>`（同一秒内重复时追加序号），并清理多余的旧备份。返回备份路径
pub fn backup_corrupt(path: &Path) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let secs = unix_now_ms() / 1000;
    let mut backup = path.with_file_name(format!("{name}.bak-{secs}"));
    let mut n = 1;
    while backup.exists() {
        backup = path.with_file_name(format!("{name}.bak-{secs}-{n}"));
        n += 1;
    }
    fs::rename(path, &backup)?;
    prune_backups(path, &name);
    Ok(backup)
}

/// 只保留最近 [`MAX_BACKUPS`] 份备份（按文件名里的时间戳排序；尽力而为）
fn prune_backups(path: &Path, name: &str) {
    let Some(dir) = path.parent() else { return };
    let Ok(entries) = fs::read_dir(dir) else { return };
    let prefix = format!("{name}.bak-");
    let mut backups: Vec<(u64, u64, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let file_name = e.file_name().to_string_lossy().into_owned();
            let rest = file_name.strip_prefix(&prefix)?;
            let (secs, seq) = match rest.split_once('-') {
                Some((s, q)) => (s.parse().ok()?, q.parse().ok()?),
                None => (rest.parse().ok()?, 0),
            };
            Some((secs, seq, e.path()))
        })
        .collect();
    if backups.len() <= MAX_BACKUPS {
        return;
    }
    backups.sort_unstable_by_key(|b| (b.0, b.1));
    let excess = backups.len() - MAX_BACKUPS;
    for (_, _, p) in backups.into_iter().take(excess) {
        if let Err(e) = fs::remove_file(&p) {
            log::debug!("prune backup failed: {e}");
        }
    }
}

/// 一份配置文档的读取结果
#[derive(Debug)]
pub struct Loaded<T> {
    pub value: T,
    /// 文件原本是否存在
    pub existed: bool,
    /// 损坏时的备份路径（此时 `value` 为默认值）
    pub backup: Option<PathBuf>,
    /// 宽松反序列化时被退回 / 丢弃的字段（只含路径与类型信息）
    pub issues: Vec<ConfigIssue>,
    /// 读到的原始内容的哈希（不存在或损坏时为 None）
    pub hash: Option<ContentHash>,
    /// 文件来自更新的版本：写回时要保留的内容（见 `migrate::Newer`）
    pub newer: Option<crate::settings::migrate::Newer>,
}

/// 读取一份配置文档：不存在 → 默认值；顶层不是合法 JSON 对象 → 备份为 `.bak-<unix>` 并返回默认值；
/// 否则经 `migrate`（返回迁移后的文档与结果）后宽松反序列化（单个字段非法只影响它自己）。文件来自更新的版本时，
/// 记下本版本不认识的内容（`Loaded::newer`），写回时合并回去
pub fn load_document<T>(
    path: &Path,
    migrate: impl FnOnce(serde_json::Value) -> (serde_json::Value, crate::settings::migrate::Outcome),
) -> io::Result<Loaded<T>>
where
    T: DeserializeOwned + Default + Serialize,
{
    let Some(bytes) = read_optional(path)? else {
        return Ok(Loaded {
            value: T::default(),
            existed: false,
            backup: None,
            issues: Vec::new(),
            hash: None,
            newer: None,
        });
    };
    let hash = content_hash(&bytes);
    let parsed = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .filter(serde_json::Value::is_object)
        .map(migrate)
        .and_then(|(v, outcome)| {
            let raw = matches!(outcome, crate::settings::migrate::Outcome::Newer { .. }).then(|| v.clone());
            let (value, issues) = wisp_types::from_value_lenient::<T>(v).ok()?;
            let newer = raw.as_ref().and_then(|raw| {
                let ours = serde_json::to_value(&value).ok()?;
                crate::settings::migrate::Newer::from_outcome(outcome, Some(raw), &ours)
            });
            Some((value, issues, newer))
        });
    match parsed {
        Some((value, issues, newer)) => Ok(Loaded {
            value,
            existed: true,
            backup: None,
            issues,
            hash: Some(hash),
            newer,
        }),
        None => {
            let backup = backup_corrupt(path)?;
            Ok(Loaded {
                value: T::default(),
                existed: true,
                backup: Some(backup),
                issues: Vec::new(),
                hash: None,
                newer: None,
            })
        }
    }
}

/// 序列化为写盘用的 JSON；`newer` 为 Some 时（文件来自更新的版本）合并回本版本不认识的内容并保留原来的版本号
pub fn to_pretty_json_preserving<T: Serialize>(
    value: &T,
    newer: Option<&crate::settings::migrate::Newer>,
) -> io::Result<Vec<u8>> {
    let Some(newer) = newer else {
        return to_pretty_json(value);
    };
    // 经文本中转而不是 `to_value`：f32 直接转 Value 会变成 0.6000000238418579 这样的 f64
    let text = serde_json::to_vec(value).map_err(io::Error::other)?;
    let mut v: serde_json::Value = serde_json::from_slice(&text).map_err(io::Error::other)?;
    newer.restore(&mut v);
    to_pretty_json(&v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_content_and_leaves_no_tmp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("settings.json");
        write_atomic(&path, b"{\"a\":1}\n").expect("write 1");
        write_atomic(&path, b"{\"a\":2}\n").expect("write 2");
        assert_eq!(fs::read(&path).expect("read"), b"{\"a\":2}\n");
        assert!(!tmp_path(&path).exists(), "tmp 文件应已被 rename 掉");
    }

    #[test]
    fn only_windows_sharing_errors_are_retried() {
        use wisp_types::OsKind;
        let denied = io::Error::from_raw_os_error(5);
        let sharing = io::Error::from_raw_os_error(32);
        let missing = io::Error::from_raw_os_error(2);
        assert!(is_transient_sharing_error(OsKind::Windows, &denied));
        assert!(is_transient_sharing_error(OsKind::Windows, &sharing));
        assert!(!is_transient_sharing_error(OsKind::Windows, &missing));
        // macOS / Linux 上 5 是 EIO、32 是 EPIPE：不重试
        assert!(!is_transient_sharing_error(OsKind::Macos, &sharing));
        assert!(!is_transient_sharing_error(OsKind::Linux, &denied));
        assert!(!is_transient_sharing_error(OsKind::Windows, &io::Error::other("x")));
    }

    #[test]
    fn stale_tmp_from_a_crash_does_not_affect_the_target() {
        // 模拟「写 tmp 途中被强杀」：tmp 是半截内容，目标仍是完整的旧内容
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        write_atomic(&path, b"{\"ok\":true}\n").expect("write");
        fs::write(tmp_path(&path), b"{\"ok\":tr").expect("partial tmp");
        let loaded: Loaded<serde_json::Map<String, serde_json::Value>> =
            load_document(&path, |v| (v, crate::settings::migrate::Outcome::Current)).expect("load");
        assert!(loaded.backup.is_none());
        assert_eq!(loaded.value.get("ok"), Some(&serde_json::Value::Bool(true)));
        // 下一次写入覆盖残留的 tmp
        write_atomic(&path, b"{\"ok\":false}\n").expect("rewrite");
        assert!(!tmp_path(&path).exists());
    }

    #[test]
    fn corrupt_document_is_backed_up_and_defaults_are_used() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("glossary.json");
        fs::write(&path, b"{ not json").expect("write");
        let loaded: Loaded<wisp_types::GlossaryFile> =
            load_document(&path, |v| (v, crate::settings::migrate::Outcome::Current)).expect("load");
        assert_eq!(loaded.value, wisp_types::GlossaryFile::default());
        let backup = loaded.backup.expect("backup");
        assert!(
            backup
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("glossary.json.bak-"))
        );
        assert_eq!(fs::read(&backup).expect("read backup"), b"{ not json");
        assert!(!path.exists(), "损坏的原文件已改名为备份");
    }

    #[test]
    fn top_level_array_counts_as_corrupt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("styles.json");
        fs::write(&path, b"[1,2,3]").expect("write");
        let loaded: Loaded<wisp_types::StylesFile> =
            load_document(&path, |v| (v, crate::settings::migrate::Outcome::Current)).expect("load");
        assert!(loaded.backup.is_some());
    }

    #[test]
    fn missing_document_yields_default_without_backup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let loaded: Loaded<wisp_types::StylesFile> = load_document(&dir.path().join("styles.json"), |v| {
            (v, crate::settings::migrate::Outcome::Current)
        })
        .expect("load");
        assert!(!loaded.existed);
        assert!(loaded.backup.is_none());
        assert!(loaded.hash.is_none());
    }

    #[test]
    fn backups_are_pruned_to_the_newest_few() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        for i in 0..(MAX_BACKUPS + 3) {
            fs::write(dir.path().join(format!("settings.json.bak-{}", 1000 + i)), b"x").expect("seed");
        }
        fs::write(&path, b"broken").expect("write");
        let backup = backup_corrupt(&path).expect("backup");
        let remaining: Vec<String> = fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("settings.json.bak-"))
            .collect();
        assert_eq!(remaining.len(), MAX_BACKUPS, "{remaining:?}");
        assert!(backup.exists(), "最新的备份必须保留");
        assert!(
            !dir.path().join("settings.json.bak-1000").exists(),
            "最旧的备份应被删除"
        );
    }
}
