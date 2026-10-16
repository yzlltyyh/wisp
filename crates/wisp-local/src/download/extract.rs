//! 运行时发布包的解包：只取 [`ExtractRule`] 挑中的文件，拒绝绝对路径与 `..`。全部是阻塞调用。
//!
//! - **tar.gz**（macOS / Linux 的发布包）：普通文件与软链接。软链接只允许指向同一目录里的文件名（发布包里的
//!   `libllama.0.dylib -> libllama.0.7.0.dylib` 这类版本号链接；动态库的依赖写的就是链接名），指向别处或 `..` 的一律拒绝；
//!   硬链接与其他类型跳过。权限位取自包内记录（去掉 setuid 等特殊位）：官方包里 `llama-server` 是 0755，整包哈希钉死，
//!   所以权限也是钉死的；规则要求可执行而包里没有可执行位时视为包损坏。
//! - **zip**（Windows 的发布包）：只有普通文件；链接一律跳过（Windows 包里没有）。解压时 zip 自带的 CRC32 校验失败即报错。
//! - 每条规则至少要选中一个文件（发布包结构变了就报错，不留下缺文件的运行时）。

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Component, Path, PathBuf};

use crate::catalog::{ArchiveKind, ExtractRule};
use crate::error::LocalError;
use wisp_types::DownloadErrorCode;

/// tar 的权限掩码：去掉组与其他人的写权限
const TAR_MASK: u32 = 0o022;

/// 包内路径是否被某条规则挑中；返回解出后的相对路径（规则的目标目录 + 包内的文件名）与是否需要可执行位
pub fn select(rules: &[ExtractRule], path_in_archive: &str) -> Option<(PathBuf, bool)> {
    let path = path_in_archive.trim_start_matches("./");
    rules.iter().find_map(|r| {
        let rest = path.strip_prefix(r.prefix)?;
        if r.suffix.is_empty() {
            if !rest.is_empty() {
                return None;
            }
        } else if rest.contains('/') || !rest.ends_with(r.suffix) {
            return None;
        }
        // 前缀可以是目录（`build/bin/`）也可以是文件名的开头（`lib/libggml.0`）：解出的名字总是包内的文件名
        let name = path.rsplit('/').next()?;
        let rel = safe_relative(&Path::new(r.dest_dir).join(name))?;
        Some((rel, r.executable))
    })
}

/// 只允许普通的相对路径（没有根、盘符、`..`、`.`）；否则为 None
pub fn safe_relative(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(seg) => out.push(seg),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// 把 `archive` 里挑中的文件解到 `dest`（`dest` 已存在；同名文件覆盖）。返回解出的相对路径
pub fn extract(
    archive: &Path,
    kind: ArchiveKind,
    rules: &[ExtractRule],
    dest: &Path,
) -> Result<Vec<PathBuf>, LocalError> {
    let file = File::open(archive)?;
    let mut hits = vec![0usize; rules.len()];
    let out = match kind {
        ArchiveKind::TarGz => extract_tar(BufReader::new(file), rules, dest, &mut hits)?,
        ArchiveKind::Zip => extract_zip(BufReader::new(file), rules, dest, &mut hits)?,
    };
    if hits.contains(&0) {
        return Err(bad_archive());
    }
    Ok(out)
}

fn bad_archive() -> LocalError {
    LocalError::Download(DownloadErrorCode::Extract)
}

/// 记下这个包内路径命中了哪条规则
fn note_hit(rules: &[ExtractRule], path: &str, hits: &mut [usize]) {
    if let Some(i) = rules
        .iter()
        .position(|r| select(std::slice::from_ref(r), path).is_some())
    {
        hits[i] += 1;
    }
}

/// 解出的位置；已有同名文件或链接时先删掉（换成新内容，不跟随旧链接写到别处）
fn prepare_target(dest: &Path, rel: &Path) -> Result<PathBuf, LocalError> {
    let target = dest.join(rel);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::symlink_metadata(&target) {
        Ok(m) if m.is_dir() => return Err(bad_archive()),
        Ok(_) => std::fs::remove_file(&target)?,
        Err(_) => {}
    }
    Ok(target)
}

fn extract_tar(
    reader: impl Read,
    rules: &[ExtractRule],
    dest: &Path,
    hits: &mut [usize],
) -> Result<Vec<PathBuf>, LocalError> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(reader));
    archive.set_mask(TAR_MASK);
    archive.set_preserve_permissions(false);
    let mut out = Vec::new();
    let mut links: Vec<(PathBuf, PathBuf)> = Vec::new();
    for entry in archive.entries().map_err(|_| bad_archive())? {
        let mut entry = entry.map_err(|_| bad_archive())?;
        let path = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        let Some((rel, executable)) = select(rules, &path) else {
            continue;
        };
        let kind = entry.header().entry_type();
        if kind.is_symlink() {
            // 只接受同目录里的文件名
            let link = entry
                .link_name()
                .map_err(|_| bad_archive())?
                .ok_or_else(bad_archive)?
                .into_owned();
            let name = link
                .to_str()
                .filter(|n| !n.is_empty() && !n.contains(['/', '\\']) && *n != "." && *n != "..");
            let Some(name) = name else {
                return Err(bad_archive());
            };
            let target = prepare_target(dest, &rel)?;
            entry.unpack(&target).map_err(|_| bad_archive())?;
            links.push((rel.clone(), rel.with_file_name(name)));
        } else if kind.is_file() || kind == tar::EntryType::Continuous {
            if executable && entry.header().mode().map_err(|_| bad_archive())? & 0o111 == 0 {
                return Err(bad_archive());
            }
            let target = prepare_target(dest, &rel)?;
            entry.unpack(&target).map_err(|e| io_or_extract(&e))?;
        } else {
            // 目录、硬链接、设备文件等不解出
            continue;
        }
        note_hit(rules, &path, hits);
        out.push(rel);
    }
    // 软链接必须指向这次解出的普通文件（不能悬空，也不能链到链接上绕出去）
    for (link, target) in &links {
        let ok = out.contains(target) && !links.iter().any(|(l, _)| l == target);
        if !ok || link == target {
            return Err(bad_archive());
        }
    }
    Ok(out)
}

fn extract_zip(
    reader: impl Read + std::io::Seek,
    rules: &[ExtractRule],
    dest: &Path,
    hits: &mut [usize],
) -> Result<Vec<PathBuf>, LocalError> {
    let mut archive = zip::ZipArchive::new(reader).map_err(|_| bad_archive())?;
    let mut out = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|_| bad_archive())?;
        if entry.is_dir() || entry.is_symlink() {
            continue;
        }
        let Ok(path) = entry.name().map(|n| n.into_owned()) else {
            continue;
        };
        let Some((rel, _)) = select(rules, &path) else {
            continue;
        };
        let target = prepare_target(dest, &rel)?;
        let mut file = File::create(&target)?;
        std::io::copy(&mut entry, &mut file).map_err(|e| io_or_extract(&e))?;
        file.sync_all()?;
        note_hit(rules, &path, hits);
        out.push(rel);
    }
    Ok(out)
}

/// 写盘失败（磁盘满）与包内容损坏分开报
fn io_or_extract(e: &std::io::Error) -> LocalError {
    if crate::download::is_disk_full(e) {
        LocalError::Download(DownloadErrorCode::DiskFull)
    } else {
        bad_archive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: [ExtractRule; 2] = [
        ExtractRule {
            prefix: "build/bin/",
            suffix: ".dylib",
            dest_dir: "",
            executable: false,
        },
        ExtractRule {
            prefix: "build/bin/llama-server",
            suffix: "",
            dest_dir: "",
            executable: true,
        },
    ];

    #[test]
    fn rules_pick_only_the_listed_files() {
        assert_eq!(
            select(&RULES, "build/bin/libggml.dylib"),
            Some((PathBuf::from("libggml.dylib"), false))
        );
        assert_eq!(
            select(&RULES, "./build/bin/llama-server"),
            Some((PathBuf::from("llama-server"), true))
        );
        assert_eq!(select(&RULES, "build/bin/llama-cli"), None);
        assert_eq!(select(&RULES, "build/bin/sub/libx.dylib"), None, "不进子目录");
        assert_eq!(select(&RULES, "build/bin/llama-server.sig"), None);
        let versioned = [ExtractRule {
            prefix: "pkg/libllama.0",
            suffix: ".dylib",
            dest_dir: "lib",
            executable: false,
        }];
        assert_eq!(
            select(&versioned, "pkg/libllama.0.7.0.dylib"),
            Some((PathBuf::from("lib/libllama.0.7.0.dylib"), false)),
            "前缀是文件名的开头时保留完整的文件名"
        );
        assert_eq!(select(&versioned, "pkg/libllama.dylib"), None);
    }

    #[test]
    fn unsafe_paths_are_rejected() {
        assert_eq!(safe_relative(Path::new("a/b")), Some(PathBuf::from("a/b")));
        assert_eq!(safe_relative(Path::new("./a")), Some(PathBuf::from("a")));
        assert_eq!(safe_relative(Path::new("../a")), None);
        assert_eq!(safe_relative(Path::new("a/../../b")), None);
        assert_eq!(safe_relative(Path::new("/etc/passwd")), None);
        assert_eq!(safe_relative(Path::new("")), None);
    }
}
