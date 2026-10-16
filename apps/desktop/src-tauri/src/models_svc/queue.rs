//! 下载队列：同一时刻只下载一个模型（连同它缺的推理组件）；其余排队。
//!
//! - 暂停保留 `.part`（下次从断点继续）；取消删除 `.part`；应用退出时正在下载的任务变为「已暂停」（不自动续）。
//!   暂停时记下当时的进度，快照照此显示「已暂停」——区间提取（Windows 的 ONNX Runtime）没有 `.part`，只看库存的话会显示成没下载。
//! - 失败的任务记下失败种类（快照里显示「失败，可重试」），已下载的部分保留。
//! - **待检测的设备**：Windows / Linux 上还没装 llama.cpp 时检测不到显卡，需要显卡的模型（PaddleOCR-VL、9B）的适配是「待检测」。
//!   用 llama.cpp 的模型在显卡还没检测过时先只装推理组件（已经装着就跳过）、检测显卡；入队时适配还是「待检测」的（用户
//!   下载前只确认过「待检测」），检测后的结论是「较慢 / 内存不足」就把任务停在「已暂停」（界面在「继续」时请用户确认），
//!   否则接着下模型——不必下完几个 GB 才发现本机跑不动。引导页「高质量」组合同时排了 9B 与 VL，两个各自停下来等确认。
//! - 下载完成：重新读库存、刷新设置快照（`RuntimeInfo.offline`）、设置窗口不在前台时发一条系统通知；装上了 llama.cpp
//!   时（非 Apple 芯片）顺便检测一次显卡；「下载并使用」的模型此时才选用（下载期间原来的模型照常可用）；llama.cpp 跑的模型
//!   在后台预热一次（见 `lifecycle::spawn_warm_up`）。

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use tokio_util::sync::CancellationToken;
use wisp_local::catalog::{self, Target};
use wisp_local::download::{self, Job, Progress, Stage};
use wisp_local::{LocalError, fitness};
use wisp_types::{
    DownloadErrorCode, FitnessLevel, LocalModelId, LocalModelKind, LocalModelState, LocalRuntimeId, ModelRef, Settings,
};

use super::ModelsService;
use crate::error::{IpcError, IpcErrorCode};

/// 暂停 / 取消 / 退出时，正在下载的任务被取消的原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Intent {
    Run,
    Pause,
    Cancel,
    Shutdown,
}

/// 正在下载的任务
pub(super) struct Active {
    pub(super) model: LocalModelId,
    pub(super) cancel: CancellationToken,
    pub(super) intent: Intent,
    /// 任务收尾（关掉 `.part`、按意图删除部分文件）之后触发：删除正在下载的模型时等它
    pub(super) finished: CancellationToken,
    /// 最后一次「下载中」的进度（已完成, 总量）：暂停落在校验或显卡检测时，快照照此显示「已暂停」
    pub(super) progress: Option<(u64, u64)>,
}

/// 「下载并使用」登记时同类别的选择（`offline.<类别>_model` 与 `defaults.<任务>`）：装好时设置仍是这样才选用
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Baseline {
    model: Option<LocalModelId>,
    default: Option<ModelRef>,
}

impl Baseline {
    pub(super) fn of(settings: &Settings, kind: LocalModelKind) -> Self {
        match kind {
            LocalModelKind::Ocr => Self {
                model: settings.offline.ocr_model,
                default: settings.defaults.ocr.clone(),
            },
            LocalModelKind::Translate => Self {
                model: settings.offline.translate_model,
                default: settings.defaults.translate.clone(),
            },
        }
    }
}

/// 下载队列
#[derive(Default)]
pub(super) struct Queue {
    /// 排队中的模型（按点击的先后）
    pub(super) waiting: VecDeque<LocalModelId>,
    pub(super) current: Option<Active>,
    /// 快照里的状态（排队、下载中、校验中、已暂停、失败），优先于库存
    pub(super) states: BTreeMap<LocalModelId, LocalModelState>,
    /// 「下载并使用」：装好后选用，带登记时的选择。同一类别只认最后一次登记；暂停、失败后重试都保留，取消与删除时清掉
    pub(super) select: BTreeMap<LocalModelId, Baseline>,
    /// 入队时适配还是「待检测」的模型：检测显卡之后结论是较慢 / 内存不足就停下来等用户确认
    pub(super) unconfirmed: BTreeSet<LocalModelId>,
}

impl Queue {
    /// 在下载、排队或校验（不含失败、已暂停）
    pub(super) fn busy_with(&self, model: LocalModelId) -> bool {
        self.current.as_ref().is_some_and(|a| a.model == model) || self.waiting.contains(&model)
    }
}

/// 一个任务收尾时的结论（下载的结果加上用户的意图）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    Installed,
    Held {
        done: u64,
        total: u64,
    },
    /// 暂停：带最后一次「下载中」的进度
    Paused(Option<(u64, u64)>),
    Cancelled,
    Shutdown,
    Failed(DownloadErrorCode),
}

/// 一个任务怎么结束的（失败另见 `Err`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Finished {
    /// 模型与它需要的推理组件都装好了
    Installed,
    /// 装好推理组件、检测过显卡后发现本机跑这个模型较慢或内存不足：停在「已暂停」，等用户确认后继续
    Held { done: u64, total: u64 },
}

/// 下载失败的种类（快照里的 `failed.code`）
fn error_code(e: &LocalError) -> DownloadErrorCode {
    match e {
        LocalError::Download(code) => *code,
        LocalError::Io(_) => DownloadErrorCode::Io,
        LocalError::Runtime(_) => DownloadErrorCode::Extract,
        LocalError::Cancelled | LocalError::NotInstalled | LocalError::Protocol(_) | LocalError::Unimplemented(_) => {
            DownloadErrorCode::Server
        }
    }
}

/// 暂停时快照里的状态：按下载中最后一次的进度（与进度条同一口径，不倒退；暂停落在校验或显卡检测时也一样）；
/// 还没有进度时交给库存（`.part` 的大小）
fn paused_state(last: Option<(u64, u64)>) -> Option<LocalModelState> {
    last.map(|(done, total)| LocalModelState::Paused {
        done: done.min(total),
        total,
    })
}

/// 检测过显卡之后是否要停下来请用户确认
fn hold_after_probe(level: FitnessLevel) -> bool {
    matches!(level, FitnessLevel::Slow | FitnessLevel::Insufficient)
}

/// 下载进度回调
type ProgressFn = Arc<dyn Fn(Progress) + Send + Sync>;

/// 测试换掉真正的下载：（模型目录, 任务, 进度, 取消, 只装推理组件）
#[cfg(test)]
pub(super) type TestDownload = Arc<
    dyn Fn(
            wisp_local::store::Store,
            Job,
            ProgressFn,
            CancellationToken,
            bool,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), LocalError>> + Send>>
        + Send
        + Sync,
>;

impl ModelsService {
    /// 开始（或继续）下载；已在下载或排队、或已经齐全时什么也不做。`select`（「下载并使用」）：装好后选用它
    /// ——已经齐全时立即选用，已在下载或排队时把这个标记补记到那个任务上
    pub async fn download(self: &Arc<Self>, model: LocalModelId, select: bool) -> Result<(), IpcError> {
        let Some(target) = Target::current() else {
            return Err(IpcError::new(
                IpcErrorCode::Platform,
                "offline models are not available on this platform",
            ));
        };
        let inventory = self.inventory().await;
        let settings = self.settings.get();
        let job = Job {
            model,
            target,
            source: settings.offline.download_source,
        };
        let plan = download::plan(&inventory, &job);
        if plan.runtimes.is_empty() && plan.files.is_empty() {
            if select {
                // 最后一次点击为准：同类别之前登记的「装好后选用」作废
                self.state
                    .lock()
                    .downloads
                    .select
                    .retain(|m, _| m.kind() != model.kind());
                self.select_model(model, None).await?;
            }
            return Ok(());
        }
        // 用户下载前只确认过「待检测」：检测显卡之后若较慢 / 内存不足，要再停下来让用户看到结论（继续时适配已有结论，不再记）
        let unknown =
            fitness::assess(model, &self.device_info(), settings.offline.use_gpu).level == FitnessLevel::Unknown;
        {
            let mut g = self.state.lock();
            if g.shut_down {
                return Err(IpcError::internal("Wisp is quitting"));
            }
            if select {
                g.downloads.select.retain(|m, _| m.kind() != model.kind());
                g.downloads.select.insert(model, Baseline::of(&settings, model.kind()));
            }
            if g.downloads.busy_with(model) {
                return Ok(());
            }
            if unknown {
                g.downloads.unconfirmed.insert(model);
            }
            g.downloads.waiting.push_back(model);
            g.downloads.states.insert(model, LocalModelState::Queued);
        }
        log::info!("offline: download queued model={}", model.as_str());
        self.kick_downloads();
        self.broadcast_now();
        Ok(())
    }

    /// 暂停（保留已下载的部分）
    pub async fn pause(self: &Arc<Self>, model: LocalModelId) -> Result<(), IpcError> {
        let rescan = {
            let mut g = self.state.lock();
            match g.downloads.current.as_mut() {
                Some(a) if a.model == model => {
                    a.intent = Intent::Pause;
                    a.cancel.cancel();
                    false
                }
                _ => {
                    g.downloads.waiting.retain(|m| *m != model);
                    g.downloads.states.remove(&model).is_some()
                }
            }
        };
        if rescan {
            self.rescan().await;
            self.broadcast_now();
        }
        Ok(())
    }

    /// 取消（删除已下载的部分）
    pub async fn cancel(self: &Arc<Self>, model: LocalModelId) -> Result<(), IpcError> {
        let running = {
            let mut g = self.state.lock();
            g.downloads.select.remove(&model);
            g.downloads.unconfirmed.remove(&model);
            match g.downloads.current.as_mut() {
                Some(a) if a.model == model => {
                    a.intent = Intent::Cancel;
                    a.cancel.cancel();
                    true
                }
                _ => {
                    g.downloads.waiting.retain(|m| *m != model);
                    g.downloads.states.remove(&model);
                    false
                }
            }
        };
        if !running {
            // 排队、暂停或失败的任务：现在就删掉部分文件（正在下载的由任务收尾时删）
            self.discard_partial(model).await;
            self.rescan().await;
            self.broadcast_now();
        }
        Ok(())
    }

    /// 队列空闲时开始下一个任务
    pub(super) fn kick_downloads(self: &Arc<Self>) {
        let next = {
            let mut g = self.state.lock();
            if g.shut_down || g.downloads.current.is_some() {
                return;
            }
            let Some(model) = g.downloads.waiting.pop_front() else {
                return;
            };
            let cancel = CancellationToken::new();
            let finished = CancellationToken::new();
            g.downloads.current = Some(Active {
                model,
                cancel: cancel.clone(),
                intent: Intent::Run,
                finished: finished.clone(),
                progress: None,
            });
            (model, cancel, finished)
        };
        let svc = Arc::clone(self);
        super::spawn(async move { svc.run_download(next.0, next.1, next.2).await });
    }

    async fn run_download(
        self: Arc<Self>,
        model: LocalModelId,
        cancel: CancellationToken,
        finished: CancellationToken,
    ) {
        let t0 = Instant::now();
        let result = self.download_one(model, &cancel).await;
        let (end, baseline) = {
            let mut g = self.state.lock();
            let active = g.downloads.current.take().filter(|a| a.model == model);
            let intent = active.as_ref().map_or(Intent::Run, |a| a.intent);
            let last = active.and_then(|a| a.progress);
            // 检测显卡之后停下来的任务（`Held`）同样看意图：检测期间点了取消就按取消收尾，退出时按退出收尾
            let end = match (&result, intent) {
                (Ok(Finished::Installed), _) => End::Installed,
                (_, Intent::Cancel) => End::Cancelled,
                (_, Intent::Shutdown) => End::Shutdown,
                (Ok(Finished::Held { done, total }), _) => End::Held {
                    done: *done,
                    total: *total,
                },
                (Err(_), Intent::Pause) => End::Paused(last),
                (Err(e), Intent::Run) => End::Failed(error_code(e)),
            };
            let mut baseline = None;
            match end {
                End::Installed => {
                    g.downloads.states.remove(&model);
                    g.downloads.unconfirmed.remove(&model);
                    baseline = g.downloads.select.remove(&model);
                }
                End::Held { done, total } => {
                    g.downloads
                        .states
                        .insert(model, LocalModelState::Paused { done, total });
                }
                End::Paused(last) => {
                    g.downloads.states.remove(&model);
                    if let Some(paused) = paused_state(last) {
                        g.downloads.states.insert(model, paused);
                    }
                }
                End::Cancelled => {
                    g.downloads.states.remove(&model);
                    g.downloads.select.remove(&model);
                    g.downloads.unconfirmed.remove(&model);
                }
                End::Shutdown => {}
                End::Failed(code) => {
                    g.downloads.states.insert(model, LocalModelState::Failed { code });
                }
            }
            (end, baseline)
        };
        self.end_download_env();
        match end {
            End::Installed => {
                log::info!(
                    "offline: download finished model={} in {} s",
                    model.as_str(),
                    t0.elapsed().as_secs()
                );
                self.clear_failures(model);
                self.rescan().await;
                if let Some(baseline) = baseline
                    && let Err(e) = self.select_model(model, Some(baseline)).await
                {
                    log::warn!(
                        "offline: selecting {} after download failed: {}",
                        model.as_str(),
                        e.message
                    );
                }
                self.settings.notify_changed();
                self.notify_installed(model);
                self.probe_devices_after_install();
                self.spawn_warm_up(model);
            }
            End::Held { .. } => {
                log::info!(
                    "offline: download of {} paused after the GPU check (this device would be slow)",
                    model.as_str()
                );
                self.rescan().await;
                self.settings.notify_changed();
            }
            // 退出：保留部分文件，下次启动时显示为「已暂停」
            End::Shutdown => {
                finished.cancel();
                return;
            }
            End::Cancelled => {
                log::info!("offline: download cancelled model={}", model.as_str());
                self.discard_partial(model).await;
                self.rescan().await;
            }
            End::Paused(_) => {
                log::info!("offline: download paused model={}", model.as_str());
                self.rescan().await;
            }
            End::Failed(code) => {
                log::warn!("offline: download failed model={} ({code:?})", model.as_str());
                self.rescan().await;
            }
        }
        finished.cancel();
        self.broadcast_now();
        self.kick_downloads();
    }

    /// 下载一个模型需要的全部东西（缺的推理组件 + 模型文件），写安装记录。显卡待检测时先只装推理组件（见模块说明）
    async fn download_one(
        self: &Arc<Self>,
        model: LocalModelId,
        cancel: &CancellationToken,
    ) -> Result<Finished, LocalError> {
        let target = Target::current().ok_or(LocalError::NotInstalled)?;
        let inventory = self.rescan().await;
        let job = Job {
            model,
            target,
            source: self.settings.get().offline.download_source,
        };
        let plan = download::plan(&inventory, &job);
        if plan.runtimes.is_empty() && plan.files.is_empty() {
            return Ok(Finished::Installed);
        }
        // 空间：剩余字节 × 1.1 + 200 MB（解包要额外空间）。取不到可用空间时不拦
        let root = self.store.root().to_path_buf();
        let free = super::spawn_blocking(move || {
            std::fs::create_dir_all(&root)?;
            Ok::<_, std::io::Error>(wisp_platform::system::disk_free(&root))
        })
        .await
        .map_err(|_| LocalError::Protocol("blocking task failed"))??;
        // 测试里的假下载不按目录里的大小写盘（9B 的目录大小是 5 GB 级）：不做空间检查
        #[cfg(test)]
        let free = free.filter(|_| self.download_override.lock().is_none());
        if let Some(free) = free
            && free < plan.required_space()
        {
            log::warn!(
                "offline: not enough disk space for {} ({} MB free, {} MB needed)",
                model.as_str(),
                free / (1024 * 1024),
                plan.required_space() / (1024 * 1024)
            );
            return Err(LocalError::Download(DownloadErrorCode::DiskFull));
        }
        self.begin_download_env();
        self.set_progress(model, plan.done, plan.total, 0);
        self.broadcast_now();
        let svc = Arc::clone(self);
        let progress: ProgressFn = Arc::new(move |p: Progress| svc.on_progress(model, p));
        let use_gpu = self.settings.get().offline.use_gpu;
        let needs_llama = catalog::model(model).runtimes.contains(&LocalRuntimeId::LlamaCpp);
        if needs_llama && !plan.files.is_empty() && !self.apple_silicon() && self.state.lock().gpus.is_none() {
            if plan.runtimes.contains(&LocalRuntimeId::LlamaCpp) {
                self.fetch(&job, &progress, cancel, true).await?;
                // 下载器自己读写磁盘：先刷新库存，检测才看得到刚装好的推理组件
                self.rescan().await;
            }
            self.probe_before_model(model, cancel).await?;
        }
        let unconfirmed = self.state.lock().downloads.unconfirmed.remove(&model);
        if unconfirmed && !plan.files.is_empty() {
            let level = fitness::assess(model, &self.device_info(), use_gpu).level;
            if hold_after_probe(level) {
                let rest = download::plan(&self.rescan().await, &job);
                return Ok(Finished::Held {
                    done: rest.done.min(rest.total),
                    total: rest.total,
                });
            }
        }
        self.fetch(&job, &progress, cancel, false).await?;
        Ok(Finished::Installed)
    }

    /// 下模型文件之前检测显卡。检测要启动一次 llama-server（最长 `LIST_DEVICES_TIMEOUT`），期间照常响应暂停 / 取消
    /// （检测进程由阻塞线程里的超时收尾）；检测失败时适配仍是「待检测」，照常下载（用户下载前已经确认过）
    async fn probe_before_model(
        self: &Arc<Self>,
        model: LocalModelId,
        cancel: &CancellationToken,
    ) -> Result<(), LocalError> {
        // 推理组件已经下完：检测期间不显示停在最后一刻的下载速度
        let last = self
            .state
            .lock()
            .downloads
            .current
            .as_ref()
            .filter(|a| a.model == model)
            .and_then(|a| a.progress);
        if let Some((done, total)) = last {
            self.set_progress(model, done, total, 0);
            self.broadcast_now();
        }
        let probed = tokio::select! {
            () = cancel.cancelled() => return Err(LocalError::Cancelled),
            r = self.devices_refresh() => r,
        };
        if let Err(e) = probed {
            log::info!("offline: GPU detection before the model download failed: {}", e.message);
        }
        Ok(())
    }

    /// 记下「下载中」的进度（快照的状态与暂停时用的最后进度）
    fn set_progress(&self, model: LocalModelId, done: u64, total: u64, bytes_per_sec: u64) {
        let mut g = self.state.lock();
        let Some(active) = g.downloads.current.as_mut().filter(|a| a.model == model) else {
            return;
        };
        active.progress = Some((done, total));
        g.downloads.states.insert(
            model,
            LocalModelState::Downloading {
                done,
                total,
                bytes_per_sec,
            },
        );
    }

    /// 执行下载（`runtimes_only`：只补齐推理组件）
    async fn fetch(
        &self,
        job: &Job,
        progress: &ProgressFn,
        cancel: &CancellationToken,
        runtimes_only: bool,
    ) -> Result<(), LocalError> {
        #[cfg(test)]
        {
            let fake = self.download_override.lock().clone();
            if let Some(fake) = fake {
                return fake(
                    self.store.clone(),
                    *job,
                    Arc::clone(progress),
                    cancel.clone(),
                    runtimes_only,
                )
                .await;
            }
        }
        if runtimes_only {
            download::run_runtimes(&self.store, job, &**progress, cancel.clone()).await
        } else {
            download::run(&self.store, job, &**progress, cancel.clone()).await
        }
    }

    /// 进度回调（下载线程上；只做 O(1) 的工作）
    fn on_progress(self: &Arc<Self>, model: LocalModelId, p: Progress) {
        let state = match p.stage {
            Stage::Downloading => LocalModelState::Downloading {
                done: p.done,
                total: p.total,
                bytes_per_sec: p.bytes_per_sec,
            },
            Stage::Verifying => LocalModelState::Verifying,
        };
        let changed_stage = {
            let mut g = self.state.lock();
            let Some(active) = g.downloads.current.as_mut().filter(|a| a.model == model) else {
                return;
            };
            if p.stage == Stage::Downloading {
                active.progress = Some((p.done, p.total));
            }
            let prev = g.downloads.states.insert(model, state);
            !matches!(
                (prev, state),
                (
                    Some(LocalModelState::Downloading { .. }),
                    LocalModelState::Downloading { .. }
                ) | (Some(LocalModelState::Verifying), LocalModelState::Verifying)
            )
        };
        if changed_stage {
            self.broadcast_now();
        } else {
            self.broadcast_progress(model, p.done, p.total);
        }
    }

    /// 删除一个模型的部分文件（阻塞部分在阻塞线程里）
    pub(super) async fn discard_partial(&self, model: LocalModelId) {
        let store = self.store.clone();
        match super::spawn_blocking(move || store.discard_partial(model)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => log::warn!("offline: discarding the partial download failed: {e}"),
            Err(e) => log::warn!("offline: discard task failed: {e}"),
        }
    }

    /// 停止下载（退出）：正在下载的任务变为暂停，排队的清空
    pub(super) fn shutdown_downloads(&self) {
        let mut g = self.state.lock();
        g.downloads.waiting.clear();
        if let Some(a) = g.downloads.current.as_mut() {
            a.intent = Intent::Shutdown;
            a.cancel.cancel();
        }
    }

    /// 下载完成且设置窗口不在前台：发一条系统通知
    fn notify_installed(&self, model: LocalModelId) {
        let Some(app) = self.app.clone() else { return };
        super::spawn_blocking(move || {
            use tauri::Manager;
            use tauri_plugin_notification::NotificationExt;
            let focused = app
                .get_webview_window(crate::windows::settings_window::SETTINGS_LABEL)
                .is_some_and(|w| w.is_focused().unwrap_or(false));
            if focused {
                return;
            }
            let lang = crate::tray::menu::current_lang(&app);
            let name = crate::orchestrator::plan::offline_label(model);
            if let Err(e) = app
                .notification()
                .builder()
                .title(crate::app::i18n::tr(lang, "notify.offline_ready.title"))
                .body(crate::app::i18n::tr_args(
                    lang,
                    "notify.offline_ready.body",
                    &[("model", name)],
                ))
                .show()
            {
                log::debug!("offline: download notification failed: {e}");
            }
        });
    }

    /// 刚装上 llama.cpp（Windows / Linux）：检测一次显卡，设备适配随之更新
    fn probe_devices_after_install(self: &Arc<Self>) {
        let needs = !self.apple_silicon()
            && self.state.lock().gpus.is_none()
            && self
                .cached_inventory()
                .is_some_and(|inv| inv.runtime_installed(LocalRuntimeId::LlamaCpp));
        if needs {
            let svc = Arc::clone(self);
            super::spawn(async move {
                if let Err(e) = svc.devices_refresh().await {
                    log::debug!("offline: GPU detection after install failed: {}", e.message);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_errors_map_to_fixed_codes() {
        assert_eq!(
            error_code(&LocalError::Download(DownloadErrorCode::ChecksumMismatch)),
            DownloadErrorCode::ChecksumMismatch
        );
        assert_eq!(
            error_code(&LocalError::Io(std::io::Error::other("x"))),
            DownloadErrorCode::Io
        );
        assert_eq!(
            error_code(&LocalError::Runtime(wisp_local::RuntimeFailure::Crashed)),
            DownloadErrorCode::Extract
        );
    }

    #[test]
    fn busy_means_running_or_waiting() {
        let mut q = Queue::default();
        assert!(!q.busy_with(LocalModelId::PpOcrV6Tiny));
        q.waiting.push_back(LocalModelId::PpOcrV6Tiny);
        assert!(q.busy_with(LocalModelId::PpOcrV6Tiny));
        q.current = Some(Active {
            model: LocalModelId::IndexTranslate2b,
            cancel: CancellationToken::new(),
            intent: Intent::Run,
            finished: CancellationToken::new(),
            progress: None,
        });
        assert!(q.busy_with(LocalModelId::IndexTranslate2b));
        // 失败的任务不算在下载
        q.states.insert(
            LocalModelId::IndexTranslate9b,
            LocalModelState::Failed {
                code: DownloadErrorCode::Network,
            },
        );
        assert!(!q.busy_with(LocalModelId::IndexTranslate9b));
    }

    #[test]
    fn pausing_keeps_the_last_progress() {
        // 暂停落在下载、校验还是显卡检测都一样：按最后一次「下载中」的进度
        assert_eq!(
            paused_state(Some((20, 39))),
            Some(LocalModelState::Paused { done: 20, total: 39 })
        );
        assert_eq!(
            paused_state(Some((50, 39))),
            Some(LocalModelState::Paused { done: 39, total: 39 })
        );
        // 还没有进度：交给库存
        assert_eq!(paused_state(None), None);
    }

    #[test]
    fn only_slow_or_insufficient_devices_hold_the_download() {
        assert!(hold_after_probe(FitnessLevel::Slow));
        assert!(hold_after_probe(FitnessLevel::Insufficient));
        assert!(!hold_after_probe(FitnessLevel::Good));
        assert!(!hold_after_probe(FitnessLevel::Unknown), "检测失败时照常下载");
    }
}
