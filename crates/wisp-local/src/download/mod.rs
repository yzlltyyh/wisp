//! 模型与运行时的下载：断点续传、多源择优与换源、边下边校验、解包，进度回调。
//!
//! - **客户端**：每个下载任务新建一个 reqwest 客户端，任务结束即丢弃（平时不存在）。跟随重定向最多 5 次、只允许 https、
//!   不带任何鉴权头与 Referer、读系统代理（与 wisp-llm 一致）、连接超时 15 s；按字节数放宽的读空闲超时（60 s 没有数据
//!   判定断流）。
//! - **续传**：`.part` + `Range: bytes=<n>-` + `If-Range`（ETag / Last-Modified）。服务器回 200（ModelScope 的小文件会 200
//!   带 Content-Range）时按 Content-Range 判断，不能续传就从头；重定向后的 CDN 签名过期（403）时回到源地址重新解析再续传。
//! - **校验**：边下边算 sha256（ring），完成后比对钉死的哈希与大小，原子改名进模型目录；不符时删掉 `.part`、换源重试一次，
//!   仍不符报 `checksum_mismatch`。
//! - **多源**：`auto` 时并发向各源发 `Range: bytes=0-0`（4 s 超时），用最先成功的；失败或连续 20 s 低于 50 KB/s 时换下一个源，
//!   在同一个 `.part` 上续传（内容相同，哈希兜底）。`model_scope` / `hugging_face` 只用指定的源。运行时只从 GitHub 下载。
//! - **空间**：开始前由调用方用 [`plan`] + [`Plan::required_space`] 与平台层的可用空间比较（本 crate 不依赖平台层），
//!   不足报 `disk_full`；下载或解包中途写盘遇到磁盘已满同样报 `disk_full`。
//! - **进度**：回调按 ≥ 250 ms 或 ≥ 1% 节流，速度为 3 s 滑动平均；整个任务的最后一段（模型文件落位，或只装推理组件时的
//!   解包）与续传前重算大文件的哈希时报 [`Stage::Verifying`]，中途的推理组件解包不报（进度条不在两种状态之间闪）。
//! - **日志**：只记 host + 路径首段、字节数、耗时、状态码；不记签名查询参数。
//! - **落位**：模型文件先下到 `.downloads/<模型>/<文件>.part`，校验通过后改名为 `.downloads/<模型>/<文件>`（已校验，续传时
//!   跳过），全部完成后逐个改名进模型目录、最后写 `installed.json`（同一个文件系统内的改名是原子的；中途崩溃时没有安装
//!   记录，库存显示为未安装、已下载的部分保留）。运行时解包到 `runtimes/<运行时>/.extract-<版本>`，写好安装记录后整个目录
//!   改名为正式目录。
//!
//! 一个任务（[`run`]）下完一个模型需要的全部东西：先补齐缺的运行时，再下模型文件，最后写安装记录。同一时刻只运行一个任务
//! （排队由调用方——desktop 的模型服务——负责）；取消令牌触发时尽快返回 [`LocalError::Cancelled`] 并保留 `.part`，
//! 暂停与取消的区别（是否删除 `.part`）由调用方决定。

pub mod extract;
mod fetch;
mod ranged;

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use wisp_types::{DownloadErrorCode, DownloadSource, LocalModelId, LocalRuntimeId};

pub(crate) use fetch::is_disk_full;
pub use fetch::{Fetcher, Remote, Tuning};
pub use ranged::Ranged;

use crate::catalog::{self, FileSpec, RuntimeAsset, Target};
use crate::error::LocalError;
use crate::store::{InstalledFile, InstalledManifest, Inventory, Store};

/// 连接超时
pub const CONNECT_TIMEOUT: Duration = crate::net::CONNECT_TIMEOUT;
/// 读空闲超时（这么久没有收到数据即判定断流）
pub const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// 最多跟随的重定向次数
pub const MAX_REDIRECTS: usize = 5;
/// 多源探测的超时
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(4);
/// 低速换源：连续这么久低于 [`SLOW_BYTES_PER_SEC`]
pub const SLOW_WINDOW: Duration = Duration::from_secs(20);
/// 低速换源的阈值（字节 / 秒）
pub const SLOW_BYTES_PER_SEC: u64 = 50 * 1024;
/// 进度回调的最小间隔（另一个条件是进度变化 ≥ 1%）
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
/// 速度的滑动平均窗口
pub const SPEED_WINDOW: Duration = Duration::from_secs(3);
/// 解包等需要的额外空间（字节）
pub const SPACE_HEADROOM: u64 = 200 * 1024 * 1024;

/// 进度阶段
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    /// 下载中（`done` / `total` 有意义）
    Downloading,
    /// 校验与解包（不确定进度）
    Verifying,
}

/// 进度（`done` / `total` 含一并下载的运行时）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub stage: Stage,
    pub done: u64,
    pub total: u64,
    /// 3 s 滑动平均（刚开始时为 0）
    pub bytes_per_sec: u64,
}

/// 一个下载任务
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Job {
    pub model: LocalModelId,
    pub target: Target,
    pub source: DownloadSource,
}

/// 一个任务还要下载的东西
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// 缺的运行时（先下载）
    pub runtimes: Vec<LocalRuntimeId>,
    /// 还没安装的模型文件
    pub files: Vec<&'static FileSpec>,
    /// 全部要下载的字节数：运行时按实际下载量（[`RuntimeAsset::download_bytes`]，能区间提取时不按整包）+ 模型文件
    pub total: u64,
    /// 已经下载的部分（`.part`）
    pub done: u64,
}

impl Plan {
    /// 开始前需要的可用空间：剩余字节 × 1.1 + [`SPACE_HEADROOM`]
    pub fn required_space(&self) -> u64 {
        let remaining = self.total.saturating_sub(self.done);
        remaining.saturating_add(remaining / 10).saturating_add(SPACE_HEADROOM)
    }
}

/// 按库存算出一个任务还要下载什么（不做 I/O）
pub fn plan(inventory: &Inventory, job: &Job) -> Plan {
    let spec = catalog::model(job.model);
    let runtimes: Vec<LocalRuntimeId> = spec
        .runtimes
        .iter()
        .copied()
        .filter(|r| !inventory.runtime_installed(*r))
        .collect();
    let runtime_bytes: u64 = runtimes
        .iter()
        .filter_map(|r| catalog::runtime_asset(*r, job.target))
        .map(RuntimeAsset::download_bytes)
        .sum();
    let files: Vec<&'static FileSpec> = if inventory.is_installed(job.model) {
        Vec::new()
    } else {
        spec.files.iter().collect()
    };
    let file_bytes: u64 = files.iter().map(|f| f.size).sum();
    Plan {
        runtimes,
        files,
        total: runtime_bytes + file_bytes,
        done: inventory.partial.get(&job.model).copied().unwrap_or(0),
    }
}

/// 执行一个下载任务（见模块说明）。`progress` 在下载线程上被调用（已节流），只做 O(1) 的工作
pub async fn run(
    store: &Store,
    job: &Job,
    progress: &(dyn Fn(Progress) + Send + Sync),
    cancel: CancellationToken,
) -> Result<(), LocalError> {
    execute(store, job, progress, cancel, true).await
}

/// 只补齐这个任务缺的运行时、不下模型文件（先装推理组件、检测显卡，再决定是否继续下模型）。进度的总量与已完成量
/// 仍按整个任务算（与 [`run`] 同一口径）；没有缺的运行时时直接返回
pub async fn run_runtimes(
    store: &Store,
    job: &Job,
    progress: &(dyn Fn(Progress) + Send + Sync),
    cancel: CancellationToken,
) -> Result<(), LocalError> {
    execute(store, job, progress, cancel, false).await
}

async fn execute(
    store: &Store,
    job: &Job,
    progress: &(dyn Fn(Progress) + Send + Sync),
    cancel: CancellationToken,
    with_model: bool,
) -> Result<(), LocalError> {
    let s = store.clone();
    let inventory = blocking(move || s.scan()).await?;
    let plan = plan(&inventory, job);
    if plan.runtimes.is_empty() && (plan.files.is_empty() || !with_model) {
        return Ok(());
    }
    let mut f = Fetcher::new(progress, plan.total, plan.done, cancel, Tuning::default())?;
    for (i, rt) in plan.runtimes.iter().enumerate() {
        let asset =
            catalog::runtime_asset(*rt, job.target).ok_or(LocalError::Protocol("no runtime for this platform"))?;
        // 后面还有要下载的（另一个推理组件、模型文件，包括「先装推理组件、检测显卡再下模型」的第二段）时，解包不报
        // 「校验中」：解包只要几秒，报了的话进度条会在「下载中 → 校验中 → 下载中」之间闪一下
        let last = i + 1 == plan.runtimes.len() && plan.files.is_empty();
        install_runtime(&mut f, store, job.model, *rt, asset, last).await?;
    }
    if with_model && !plan.files.is_empty() {
        install_model(&mut f, store, job, &plan.files).await?;
    }
    Ok(())
}

/// 把阻塞的工作放进阻塞线程
pub(crate) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, LocalError> + Send + 'static,
) -> Result<T, LocalError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| LocalError::Protocol("blocking task failed"))?
}

/// 现在的 Unix 毫秒
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// 模型文件按下载源排好的地址
fn sources(source: DownloadSource, f: &FileSpec) -> Vec<&'static str> {
    match source {
        DownloadSource::ModelScope => vec![f.model_scope],
        DownloadSource::HuggingFace => vec![f.hugging_face],
        DownloadSource::Auto => vec![f.model_scope, f.hugging_face],
    }
}

/// 下载一个模型的全部文件，校验后落位并写安装记录
async fn install_model(
    f: &mut Fetcher<'_>,
    store: &Store,
    job: &Job,
    files: &[&'static FileSpec],
) -> Result<(), LocalError> {
    let partial = store.partial_dir(job.model);
    // auto：探测一次两个源，整个任务按同一个顺序（同一个源的各文件在同一个 CDN 上）
    let order = match (job.source, files.first()) {
        (DownloadSource::Auto, Some(first)) => f.rank(&sources(DownloadSource::Auto, first)).await,
        _ => vec![0],
    };
    for spec in files {
        let staged = partial.join(spec.name);
        // 上次已经下完并校验过（还没落位就被暂停或中断）
        if std::fs::metadata(&staged).is_ok_and(|m| m.is_file() && m.len() == spec.size) {
            continue;
        }
        let all = sources(job.source, spec);
        let urls: Vec<&str> = order.iter().filter_map(|i| all.get(*i).copied()).collect();
        let part = part_path(&staged);
        f.file(
            &Remote {
                urls: &urls,
                size: spec.size,
                sha256: spec.sha256,
            },
            &part,
        )
        .await?;
        std::fs::rename(&part, &staged).map_err(LocalError::Io)?;
        log::info!("download: {} verified ({} B)", job.model.as_str(), spec.size);
    }
    f.meter.verifying();
    let (store, model) = (store.clone(), job.model);
    let files: Vec<&'static FileSpec> = files.to_vec();
    blocking(move || {
        let dir = store.model_dir(model);
        std::fs::create_dir_all(&dir)?;
        // 先去掉旧的安装记录：中途失败时目录不会被当成「已安装」
        let _ = std::fs::remove_file(dir.join(crate::store::INSTALLED_FILE));
        let partial = store.partial_dir(model);
        for spec in &files {
            let target = dir.join(spec.name);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(partial.join(spec.name), &target)?;
        }
        store.write_manifest(
            &dir,
            &InstalledManifest {
                catalog_revision: catalog::CATALOG_REVISION,
                files: files
                    .iter()
                    .map(|s| InstalledFile {
                        name: s.name.to_owned(),
                        size: s.size,
                        sha256: s.sha256.to_owned(),
                    })
                    .collect(),
                installed_at: now_ms(),
            },
        )?;
        store.discard_partial(model)
    })
    .await
}

/// `<文件>.part`
fn part_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".part");
    PathBuf::from(p)
}

/// 下载并安装一个运行时。`last`：这是整个任务的最后一段，解包时报「校验中」
async fn install_runtime(
    f: &mut Fetcher<'_>,
    store: &Store,
    model: LocalModelId,
    rt: LocalRuntimeId,
    asset: &'static RuntimeAsset,
    last: bool,
) -> Result<(), LocalError> {
    let staging = store.runtime_staging_dir(rt);
    {
        let staging = staging.clone();
        blocking(move || {
            if staging.exists() {
                std::fs::remove_dir_all(&staging)?;
            }
            std::fs::create_dir_all(&staging)?;
            Ok(())
        })
        .await?;
    }
    let mut ranged = false;
    if let (true, Some(sha), Some(rule)) = (
        asset.ranged_entry,
        asset.entry_sha256,
        asset.extract.iter().find(|r| r.suffix.is_empty()),
    ) {
        // 进度的总量里这个运行时按区间提取的字节数计（见 `RuntimeAsset::download_bytes`）
        let expected = asset.download_bytes();
        let before = f.meter.done();
        let outcome = f
            .zip_entry(asset.url, asset.size, rule.prefix, sha, &staging.join(asset.main))
            .await?;
        let fetched = f.meter.done().saturating_sub(before);
        match outcome {
            Ranged::Done => {
                // 实际取到的与钉死的数不同（服务器多给或少给了包尾）时把差额补齐，让这一段正好走完
                f.meter.add(expected.saturating_sub(fetched));
                ranged = true;
            }
            Ranged::Unsupported => {
                // 退回整包：总量补上「整包 − 这一段尚未计入的部分」，已经取到的字节照旧算数，进度不倒退
                f.meter
                    .grow_total(asset.size.saturating_sub(expected.saturating_sub(fetched)));
                log::info!("download: ranged extraction unavailable, fetching the whole archive");
            }
        }
    }
    let archive_name = asset.url.rsplit('/').next().unwrap_or("runtime");
    let part = store.partial_dir(model).join(format!("{archive_name}.part"));
    if !ranged {
        f.file(
            &Remote {
                urls: &[asset.url],
                size: asset.size,
                sha256: asset.sha256,
            },
            &part,
        )
        .await?;
    }
    if last {
        f.meter.verifying();
    }
    let store = store.clone();
    blocking(move || {
        let files = if ranged {
            vec![PathBuf::from(asset.main)]
        } else {
            extract::extract(&part, asset.archive, asset.extract, &staging)?
        };
        if let (false, Some(sha)) = (ranged, asset.entry_sha256) {
            // 整包退路：解出的主文件也要与钉死的条目哈希一致
            if hash_file(&staging.join(asset.main))? != sha {
                return Err(LocalError::Download(DownloadErrorCode::ChecksumMismatch));
            }
        }
        let mut listed = Vec::with_capacity(files.len());
        for rel in &files {
            let path = staging.join(rel);
            let size = std::fs::metadata(&path)?.len();
            listed.push(InstalledFile {
                name: rel.to_string_lossy().replace('\\', "/"),
                size,
                sha256: hash_file(&path)?,
            });
        }
        store.write_manifest(
            &staging,
            &InstalledManifest {
                catalog_revision: catalog::CATALOG_REVISION,
                files: listed,
                installed_at: now_ms(),
            },
        )?;
        let dir = store.runtime_dir(rt);
        store.trash(&dir, &format!("runtime-{}", rt.as_str()))?;
        std::fs::rename(&staging, &dir)?;
        let _ = std::fs::remove_file(&part);
        Ok(())
    })
    .await?;
    log::info!("download: runtime {} installed", rt.as_str());
    Ok(())
}

/// 文件的 sha256（阻塞）
fn hash_file(path: &Path) -> Result<String, LocalError> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
    }
    Ok(ctx.finish().as_ref().iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_space_has_headroom() {
        let p = Plan {
            runtimes: Vec::new(),
            files: Vec::new(),
            total: 1_000_000_000,
            done: 400_000_000,
        };
        assert_eq!(p.required_space(), 660_000_000 + SPACE_HEADROOM);
    }

    /// Windows 的 ONNX Runtime 按区间提取的实际字节数计（不按 78.6 MB 的整包）
    #[test]
    fn plan_counts_the_bytes_actually_downloaded() {
        let job = |target| Job {
            model: LocalModelId::PpOcrV6Tiny,
            target,
            source: DownloadSource::Auto,
        };
        let empty = Inventory::default();
        let model = catalog::model_download_bytes(LocalModelId::PpOcrV6Tiny);
        let win = plan(&empty, &job(Target::WindowsX64));
        assert_eq!(win.runtimes, [LocalRuntimeId::OnnxRuntime]);
        assert_eq!(win.total, model + 5_730_829);
        let mac = plan(&empty, &job(Target::MacosArm64));
        let ort = catalog::runtime_asset(LocalRuntimeId::OnnxRuntime, Target::MacosArm64).expect("asset");
        assert_eq!(mac.total, model + ort.size);
    }
}
