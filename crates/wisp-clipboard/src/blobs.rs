//! blobs：原图（PNG）、缩略图（长边 256 px，不透明 JPEG q80 / 带 alpha PNG）、应用图标、OCR 历史图。
//!
//! 文件名即「key」：`blob_path(kind, key)` = `blobs/<dir>/<key>`；`wisp://` 资源路径 = `<asset_prefix>/<key>`
//! （见 [`BlobKind::asset_prefix`]）。因此 `AssetSource` 收到前缀之后的剩余部分，原样交给 `ClipStore::blob_path` 即可。
//! 写入一律「临时文件 + `sync_all` + rename」（[`stage`] → [`StagedBlob::commit`]）：数据先落盘再改名，断电后不会出现
//! 「文件名已在、内容为空或残缺」的 blob（而对应的行已经持久化）。写入和刷盘（macOS 上是 `F_FULLFSYNC`，4–7 ms）
//! 都在存储锁之外进行，锁内只做 rename；正在写的临时文件不会被孤儿清理误删（[`TMP_GRACE`]），
//! 崩溃遗留的临时文件过了保留期由孤儿清理删除。

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// blob 种类
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlobKind {
    /// 原图 `blobs/img/<hash>.png`
    Img,
    /// 缩略图 `blobs/thumb/<hash>.{jpg,png}`
    Thumb,
    /// 应用图标 `blobs/icons/<key>.png`
    Icon,
    /// OCR 历史图 `blobs/ocr/<key>`
    Ocr,
}

impl BlobKind {
    /// 全部种类
    pub const ALL: [Self; 4] = [Self::Img, Self::Thumb, Self::Icon, Self::Ocr];

    /// 在 blobs/ 下的子目录名
    pub const fn dir(self) -> &'static str {
        match self {
            Self::Img => "img",
            Self::Thumb => "thumb",
            Self::Icon => "icons",
            Self::Ocr => "ocr",
        }
    }

    /// `wisp://` 资源路径的前缀（desktop 的 `assets.rs`：`img/`、`thumb/`、`icon/`、`ocrimg/`）
    pub const fn asset_prefix(self) -> &'static str {
        match self {
            Self::Img => "img",
            Self::Thumb => "thumb",
            Self::Icon => "icon",
            Self::Ocr => "ocrimg",
        }
    }

    /// 由资源前缀反查种类
    pub fn from_asset_prefix(prefix: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.asset_prefix() == prefix)
    }
}

/// 资源路径（不带开头的 `/`，前端经 `resolveAssetUrl` 转成 `wisp://` URL）
pub(crate) fn asset_path(kind: BlobKind, key: &str) -> String {
    format!("{}/{key}", kind.asset_prefix())
}

/// 内容 hash → blob 文件名主干（前 16 字节的十六进制，128 位足以避免碰撞）
pub(crate) fn hash_stem(hash: &[u8; 32]) -> String {
    let mut s = String::with_capacity(32);
    for b in &hash[..16] {
        s.push(char::from_digit(u32::from(b >> 4), 16).unwrap_or('0'));
        s.push(char::from_digit(u32::from(b & 0x0f), 16).unwrap_or('0'));
    }
    s
}

/// 临时文件的前缀：孤儿清理据此删除崩溃遗留的半成品
pub(crate) const TMP_PREFIX: &str = ".tmp-";

/// 临时文件的保留期：比这更新的临时文件可能正在锁外写入，孤儿清理不删
pub(crate) const TMP_GRACE: Duration = Duration::from_secs(10 * 60);

/// 已写好并落盘、尚未就位的 blob。[`Self::commit`] 把它改名为正式文件名；没有 commit 就被丢弃时删除临时文件
#[derive(Debug)]
pub(crate) struct StagedBlob {
    tmp: PathBuf,
    dest: PathBuf,
    committed: bool,
}

/// 把 `bytes` 写进 `dir` 下的临时文件并 `sync_all`。临时文件名带进程号与序号，并发写同一个 key 互不干扰。
/// 应在存储锁之外调用
pub(crate) fn stage(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<StagedBlob> {
    let staged = StagedBlob::new(dir, name);
    let mut f = fs::File::create(&staged.tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(staged)
}

/// 同 [`stage`]，内容从文件 `src` 流式复制（同步下载到本地的原图：几 MB 的文件不整块读进内存）。`src` 原样保留，
/// 由调用方处理。应在存储锁之外调用
pub(crate) fn stage_file(dir: &Path, name: &str, src: &Path) -> io::Result<StagedBlob> {
    let staged = StagedBlob::new(dir, name);
    let mut from = fs::File::open(src)?;
    // 先建临时文件再复制：中途失败时由 Drop 删除半成品
    let mut to = fs::File::create(&staged.tmp)?;
    io::copy(&mut from, &mut to)?;
    to.sync_all()?;
    Ok(staged)
}

/// 文件内容的 blake3 与长度（流式读取，不整块读进内存）
pub(crate) fn hash_file(path: &Path) -> io::Result<([u8; 32], u64)> {
    let mut f = fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(&mut f)?;
    let len = f.metadata()?.len();
    Ok((*hasher.finalize().as_bytes(), len))
}

/// 文件开头的若干字节（按魔数识别格式用）；文件比 `n` 短时返回实际读到的部分
pub(crate) fn read_head(path: &Path, n: usize) -> io::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut buf = Vec::with_capacity(n);
    fs::File::open(path)?.take(n as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

impl StagedBlob {
    /// 临时文件名带进程号与序号，并发写同一个 key 互不干扰
    fn new(dir: &Path, name: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        Self {
            tmp: dir.join(format!("{TMP_PREFIX}{}-{seq}-{name}", std::process::id())),
            dest: dir.join(name),
            committed: false,
        }
    }

    /// 改名为正式文件名（已存在时覆盖）。调用方持有存储锁，保证孤儿清理看到这个文件时，引用它的行已在同一临界区内写入
    pub(crate) fn commit(mut self) -> io::Result<()> {
        fs::rename(&self.tmp, &self.dest)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for StagedBlob {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.tmp);
        }
    }
}

/// 临时文件是否仍在保留期内（修改时间晚于「现在 − TMP_GRACE」；时钟回拨导致修改时间在未来时也算）
pub(crate) fn is_fresh_tmp(meta: &fs::Metadata) -> bool {
    meta.modified()
        .map(|t| t.elapsed().map_or(true, |age| age < TMP_GRACE))
        .unwrap_or(false)
}

/// 删除文件：不存在视为成功；返回是否真的删除了
pub(crate) fn remove_quietly(path: &Path) -> bool {
    match fs::remove_file(path) {
        Ok(()) => true,
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => {
            log::warn!("clipboard blob remove failed: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_round_trip() {
        for k in BlobKind::ALL {
            assert_eq!(BlobKind::from_asset_prefix(k.asset_prefix()), Some(k));
        }
        assert_eq!(BlobKind::from_asset_prefix("capture"), None);
        assert_eq!(asset_path(BlobKind::Icon, "ab.png"), "icon/ab.png");
    }

    #[test]
    fn stem_is_lower_hex() {
        let mut h = [0u8; 32];
        h[0] = 0xab;
        h[15] = 0x01;
        let s = hash_stem(&h);
        assert_eq!(s.len(), 32);
        assert!(s.starts_with("ab") && s.ends_with("01"));
    }

    #[test]
    fn staged_write_commits_or_cleans_up() {
        let dir = tempfile::tempdir().expect("临时目录");
        stage(dir.path(), "a.bin", b"one")
            .expect("写入")
            .commit()
            .expect("就位");
        stage(dir.path(), "a.bin", b"two")
            .expect("写入")
            .commit()
            .expect("覆盖");
        assert_eq!(fs::read(dir.path().join("a.bin")).expect("读取"), b"two");
        // 未 commit 的临时文件随 drop 删除
        let staged = stage(dir.path(), "b.bin", b"x").expect("写入");
        assert!(
            fs::read_dir(dir.path())
                .expect("目录")
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with(TMP_PREFIX))
        );
        drop(staged);
        let names: Vec<String> = fs::read_dir(dir.path())
            .expect("目录")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["a.bin"]);
        assert!(remove_quietly(&dir.path().join("a.bin")));
        assert!(!remove_quietly(&dir.path().join("a.bin")));
    }

    #[test]
    fn tmp_freshness() {
        let dir = tempfile::tempdir().expect("临时目录");
        let p = dir.path().join(".tmp-x");
        let f = fs::File::create(&p).expect("创建");
        assert!(is_fresh_tmp(&f.metadata().expect("元数据")));
        f.set_modified(std::time::SystemTime::now() - TMP_GRACE - Duration::from_secs(1))
            .expect("改时间");
        assert!(!is_fresh_tmp(&f.metadata().expect("元数据")));
    }
}
