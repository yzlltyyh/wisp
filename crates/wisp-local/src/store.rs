//! 本机存储：`<data_dir>/models/` 的目录布局、安装记录、删除与占用统计。
//!
//! ```text
//! models/
//!   runtimes/<runtime>/<version>/…            解出的运行时文件 + installed.json
//!   <model-id>/…                               模型文件 + installed.json
//!   .downloads/<model-id>/<file>.part          断点续传的部分文件（+ .meta：来源、ETag、已校验长度）
//!   .trash-<name>-<n>/                         删除时先改名到这里，再在阻塞线程里删（失败下次启动重试）
//!   run/<slot>.pid                             推理进程的 pid 文件（崩溃后的清扫）
//! ```
//!
//! 安装状态只看 `installed.json` 与文件大小：不在每次加载时重算 GB 级文件的哈希。文件缺失或大小不符即视为损坏，界面提示
//! 重新下载。不在启动时扫描：第一次打开设置页或第一次用到离线引擎时才读一次（[`Store::scan`]）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use wisp_types::{LocalModelId, LocalRuntimeId};

use crate::catalog::{self, Target};
use crate::error::LocalError;

/// 安装记录的文件名
pub const INSTALLED_FILE: &str = "installed.json";
/// 运行时所在的子目录
const RUNTIMES_DIR: &str = "runtimes";
/// 断点续传的部分文件所在的子目录
const DOWNLOADS_DIR: &str = ".downloads";
/// pid 文件所在的子目录
const RUN_DIR: &str = "run";

/// 安装记录（`installed.json`）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledManifest {
    /// 安装时的目录版本（[`catalog::CATALOG_REVISION`]）
    pub catalog_revision: u32,
    pub files: Vec<InstalledFile>,
    /// 安装完成的时刻（Unix 毫秒）
    pub installed_at: i64,
}

/// 安装记录里的一个文件
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledFile {
    /// 相对模型（或运行时）目录的路径
    pub name: String,
    pub size: u64,
    /// 下载时校验过的 sha256
    pub sha256: String,
}

/// 一个模型的安装状态
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallState {
    NotInstalled,
    /// 已安装；`stale` = 安装记录的目录版本与当前不同（模型有更新）
    Installed {
        stale: bool,
    },
    /// 安装记录在，但文件缺失或大小不符
    Broken,
}

/// 一个运行时的安装状态
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeInstall {
    /// 当前目录版本的运行时已安装且文件完整
    pub installed: bool,
    /// 占用的磁盘空间（字节，含其他版本的残留）
    pub size: u64,
}

/// 一次扫描的结果
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Inventory {
    pub models: BTreeMap<LocalModelId, InstallState>,
    pub runtimes: BTreeMap<LocalRuntimeId, RuntimeInstall>,
    /// 各模型已下载的部分（字节，断点续传用；含运行时的部分文件）
    pub partial: BTreeMap<LocalModelId, u64>,
    /// `models/` 一共占用的磁盘空间（字节）
    pub disk_used: u64,
}

impl Inventory {
    /// 模型是否可以直接使用（已安装且不损坏；不看运行时）
    pub fn is_installed(&self, id: LocalModelId) -> bool {
        matches!(self.models.get(&id), Some(InstallState::Installed { .. }))
    }

    /// 运行时是否已安装
    pub fn runtime_installed(&self, id: LocalRuntimeId) -> bool {
        self.runtimes.get(&id).is_some_and(|r| r.installed)
    }
}

/// `models/` 目录
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// 以 `root` 为模型目录（不做 I/O，目录不存在也没关系）
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// 模型目录本身（`offline_reveal` 打开它）
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 一个模型的目录
    pub fn model_dir(&self, id: LocalModelId) -> PathBuf {
        self.root.join(id.as_str())
    }

    /// 一个运行时（当前目录版本）的目录
    pub fn runtime_dir(&self, id: LocalRuntimeId) -> PathBuf {
        self.root
            .join(RUNTIMES_DIR)
            .join(runtime_dir_name(id))
            .join(catalog::runtime(id).version)
    }

    /// 运行时的主文件（`llama-server` 可执行文件或 ONNX Runtime 动态库）的绝对路径；目录里没有这个目标的发布包时为 None。
    /// 不检查文件是否存在
    pub fn runtime_main(&self, id: LocalRuntimeId, target: Target) -> Option<PathBuf> {
        catalog::runtime_asset(id, target).map(|a| self.runtime_dir(id).join(a.main))
    }

    /// 一个模型（及它一并下载的运行时）的部分文件所在目录
    pub fn partial_dir(&self, id: LocalModelId) -> PathBuf {
        self.root.join(DOWNLOADS_DIR).join(id.as_str())
    }

    /// pid 文件所在目录
    pub fn run_dir(&self) -> PathBuf {
        self.root.join(RUN_DIR)
    }

    /// 某个槽位的 pid 文件
    pub fn pid_file(&self, slot: &str) -> PathBuf {
        self.run_dir().join(format!("{slot}.pid"))
    }

    /// 运行时（全部版本）的目录：`runtimes/<runtime>/`
    pub fn runtime_root(&self, id: LocalRuntimeId) -> PathBuf {
        self.root.join(RUNTIMES_DIR).join(runtime_dir_name(id))
    }

    /// 读取全部安装记录、核对文件大小、统计占用（阻塞）。目录不存在时返回空库存
    pub fn scan(&self) -> Result<Inventory, LocalError> {
        let mut inv = Inventory::default();
        if !self.root.is_dir() {
            return Ok(inv);
        }
        for id in LocalModelId::ALL {
            let state = match read_manifest(&self.model_dir(id)) {
                None => InstallState::NotInstalled,
                Some(Err(())) => InstallState::Broken,
                Some(Ok(m)) if files_intact(&self.model_dir(id), &m) => InstallState::Installed {
                    stale: m.catalog_revision != catalog::CATALOG_REVISION,
                },
                Some(Ok(_)) => InstallState::Broken,
            };
            inv.models.insert(id, state);
            let partial = dir_size(&self.partial_dir(id));
            if partial > 0 {
                inv.partial.insert(id, partial);
            }
        }
        for id in LocalRuntimeId::ALL {
            let dir = self.runtime_dir(id);
            let installed = matches!(read_manifest(&dir), Some(Ok(m)) if files_intact(&dir, &m));
            inv.runtimes.insert(
                id,
                RuntimeInstall {
                    installed,
                    size: dir_size(&self.runtime_root(id)),
                },
            );
        }
        inv.disk_used = dir_size(&self.root);
        Ok(inv)
    }

    /// 写安装记录（原子替换；阻塞）
    pub fn write_manifest(&self, dir: &Path, manifest: &InstalledManifest) -> Result<(), LocalError> {
        let json = serde_json::to_vec_pretty(manifest).map_err(|_| LocalError::Protocol("install manifest"))?;
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!("{INSTALLED_FILE}.tmp"));
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, dir.join(INSTALLED_FILE)).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        Ok(())
    }

    /// 删除一个模型：先改名为 `.trash-<id>-<n>` 再删除（阻塞）。调用方先停止用到它的推理进程。已下载的部分文件一并删除
    pub fn delete_model(&self, id: LocalModelId) -> Result<(), LocalError> {
        self.trash(&self.model_dir(id), id.as_str())?;
        self.discard_partial(id)
    }

    /// 删除一个运行时（全部版本；阻塞）。调用方先确认没有已安装的模型依赖它
    pub fn delete_runtime(&self, id: LocalRuntimeId) -> Result<(), LocalError> {
        self.trash(&self.runtime_root(id), runtime_dir_name(id))
    }

    /// 丢弃一个模型的部分文件（取消下载；阻塞）
    pub fn discard_partial(&self, id: LocalModelId) -> Result<(), LocalError> {
        match std::fs::remove_dir_all(self.partial_dir(id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// 删掉上次没删完的 `.trash-*` 与解包到一半的临时目录（阻塞；失败只记日志）
    pub fn purge_trash(&self) {
        let mut dirs = Vec::new();
        let mut collect = |parent: &Path, prefix: &str| {
            if let Ok(entries) = std::fs::read_dir(parent) {
                for e in entries.flatten() {
                    if e.file_name().to_str().is_some_and(|n| n.starts_with(prefix)) {
                        dirs.push(e.path());
                    }
                }
            }
        };
        collect(&self.root, TRASH_PREFIX);
        for id in LocalRuntimeId::ALL {
            collect(&self.runtime_root(id), STAGING_PREFIX);
        }
        let mut failed = 0usize;
        for d in &dirs {
            if remove_any(d).is_err() {
                failed += 1;
            }
        }
        if failed > 0 {
            log::warn!("local: {failed} leftover folder(s) could not be removed; will retry next launch");
        }
    }

    /// 运行时的解包临时目录（与正式目录同一个父目录，解完后原子改名过去）
    pub(crate) fn runtime_staging_dir(&self, id: LocalRuntimeId) -> PathBuf {
        self.runtime_root(id)
            .join(format!("{STAGING_PREFIX}{}", catalog::runtime(id).version))
    }

    /// 把 `dir` 改名进回收区再删除；目录不存在时什么也不做。删除失败（文件被占用等）留给下次启动的 [`purge_trash`](Self::purge_trash)
    pub(crate) fn trash(&self, dir: &Path, name: &str) -> Result<(), LocalError> {
        if std::fs::symlink_metadata(dir).is_err() {
            return Ok(());
        }
        let trashed = (0u32..1000)
            .map(|n| self.root.join(format!("{TRASH_PREFIX}{name}-{n}")))
            .find(|p| std::fs::symlink_metadata(p).is_err())
            .ok_or(LocalError::Protocol("too many leftover folders"))?;
        std::fs::rename(dir, &trashed)?;
        if remove_any(&trashed).is_err() {
            log::warn!("local: a deleted folder could not be removed yet; will retry next launch");
        }
        Ok(())
    }
}

/// 回收区目录名的前缀
const TRASH_PREFIX: &str = ".trash-";
/// 运行时解包临时目录名的前缀
const STAGING_PREFIX: &str = ".extract-";

/// 读 `dir/installed.json`：没有文件为 None，读不了或解析失败为 Some(Err)
fn read_manifest(dir: &Path) -> Option<Result<InstalledManifest, ()>> {
    let bytes = match std::fs::read(dir.join(INSTALLED_FILE)) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => return Some(Err(())),
    };
    Some(serde_json::from_slice(&bytes).map_err(|_| ()))
}

/// 安装记录里的每个文件都在、大小相符（跟随软链接：运行时里的版本号软链接按目标文件的大小记录）
fn files_intact(dir: &Path, m: &InstalledManifest) -> bool {
    !m.files.is_empty()
        && m.files.iter().all(|f| {
            crate::download::extract::safe_relative(Path::new(&f.name))
                .and_then(|rel| std::fs::metadata(dir.join(rel)).ok())
                .is_some_and(|md| md.is_file() && md.len() == f.size)
        })
}

/// 目录里全部文件的字节数（不跟随软链接，软链接本身不计；目录不存在为 0）
pub(crate) fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map_or(0, |m| m.len()),
            _ => 0,
        })
        .sum()
}

/// 删除目录或文件（不存在不算错）
fn remove_any(path: &Path) -> std::io::Result<()> {
    let r = match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) => Err(e),
    };
    match r {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// 运行时的目录名（模块说明里的目录布局：`llama.cpp`、`onnxruntime`）
fn runtime_dir_name(id: LocalRuntimeId) -> &'static str {
    match id {
        LocalRuntimeId::LlamaCpp => "llama.cpp",
        LocalRuntimeId::OnnxRuntime => "onnxruntime",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout() {
        let s = Store::new(PathBuf::from("/data/wisp/models"));
        assert_eq!(
            s.model_dir(LocalModelId::PpOcrV6Tiny),
            Path::new("/data/wisp/models/pp_ocrv6_tiny")
        );
        assert_eq!(
            s.runtime_dir(LocalRuntimeId::LlamaCpp),
            Path::new("/data/wisp/models/runtimes/llama.cpp").join(catalog::LLAMA_CPP_VERSION)
        );
        assert_eq!(
            s.partial_dir(LocalModelId::IndexTranslate9b),
            Path::new("/data/wisp/models/.downloads/index_translate_9b")
        );
        assert_eq!(s.pid_file("vl"), Path::new("/data/wisp/models/run/vl.pid"));
    }
}
