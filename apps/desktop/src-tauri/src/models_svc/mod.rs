//! 离线模型服务：模型库存、下载队列、推理槽位与生命周期、IPC、事件、诊断。
//!
//! - **默认零成本**：[`ModelsService::new`] 只建结构——不读盘、不起线程、不建 HTTP 客户端、不装内存压力监听。第一次打开
//!   设置页（`offline_models_get`）或第一次用到离线引擎时才读一次库存；用户点了下载才建下载客户端。启动链里只在模型目录
//!   已经存在时才读库存（几个小文件），让面板的模型菜单知道装了哪些。
//! - **槽位**：`ort`（ORT 工作进程：PP-OCRv6 与版面检测）、`vl`（llama-server：PaddleOCR-VL）、`translate`（llama-server：
//!   Index-Translate）。各自 `stopped → starting → ready → (busy) → stopping`；`ensure` 单飞（并发请求只起一次），启动失败
//!   归类后返回，同一原因 30 s 内不重启（见 `slots` / `lifecycle`）。
//! - **闲置释放**：每个槽位一个一次性定时器（`offline.idle_release_min`），每次请求开始与结束都重排；没有槽位加载时不存在任何
//!   定时器。**内存压力**：至少一个槽位加载时才安装 `wisp_platform::pressure` 监听，全部停止后卸载。
//! - **加载前的内存检查**：macOS 与内存总量比、以系统的内存压力为准（它报告的「可用」不含可压缩的部分）；Windows / Linux
//!   与可用内存比并留余量。不够时先停其他空闲的槽位再复查（见 `lifecycle::judge`）。
//! - **切换**：设置里换了模型就停掉不再选中的槽位（服务自己订阅 `SettingsService::subscribe`）；删除模型先停槽位。
//! - **退出**：`RunEvent::Exit` 与 Windows 安装更新之前调用 [`ModelsService::shutdown`]：下载变为暂停，关所有槽位的 stdin，
//!   最多等 1 s，再强制结束，删除 pid 文件。崩溃兜底见 `wisp_platform::child`（启动时按 pid 文件清扫孤儿）。
//! - **事件**：`offline_models:changed` 带完整快照（下载中按 ≥ 250 ms 或 ≥ 1% 节流；槽位加载 / 释放、安装 / 删除时也发）；
//!   安装 / 删除还会刷新设置快照（`RuntimeInfo.offline`）。
//! - **隐私**：日志只写模型 id、字节数、耗时与错误种类；不写路径里的用户名、下载地址的查询参数、推理进程的输出。

pub mod engines;
mod launcher;
mod lifecycle;
#[cfg(test)]
mod live_tests;
mod queue;
mod slots;
mod snapshot;
#[cfg(test)]
mod tests;

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tauri::Emitter;
use wisp_local::catalog::{self, Target};
use wisp_local::store::{InstallState, Inventory, Store};
use wisp_types::{DeviceInfo, GpuInfo, LocalModelId, LocalModelsSnapshot, LocalRuntimeId, OfflineSummary, event};

pub use launcher::PlatformLauncher;
pub(crate) use slots::SlotKind;

use crate::error::{IpcError, IpcErrorCode};
use crate::orchestrator::not_ready::OfflineStatus;
use crate::settings::SettingsService;
use crate::state::Deps;

/// 下载进度广播的最小间隔（另一个条件是进度变化 ≥ 1%）
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
/// 显卡检测结果的缓存文件（`runtimes/llama.cpp/` 下，随运行时版本失效）
const DEVICES_FILE: &str = "devices.json";
/// 删除正在下载的模型时，等下载任务收尾（关掉 `.part`、删掉部分文件）的上限
const DOWNLOAD_STOP_WAIT: Duration = Duration::from_secs(10);

/// 在调用方所在的 tokio 运行时上起任务：Wisp 里就是 tauri 的全局运行时（启动时 `tauri::async_runtime::set` 过）；
/// 测试里是测试自己的运行时，暂停的时钟因此同样作用于闲置定时器与启动等待。不在任何运行时里调用时（内存压力的
/// 通知线程）交给 tauri 的运行时
fn spawn<F>(f: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(h) => h.spawn(f),
        Err(_) => tauri::async_runtime::handle().inner().spawn(f),
    }
}

/// 阻塞的工作放进调用方所在运行时的阻塞线程池（理由同 [`spawn`]：测试里暂停的时钟在阻塞任务进行时不会自动前进）
fn spawn_blocking<F, R>(f: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(h) => h.spawn_blocking(f),
        Err(_) => tauri::async_runtime::handle().inner().spawn_blocking(f),
    }
}

/// 在阻塞线程里执行并把 join 失败也归为 IPC 错误
async fn run_blocking<T, F>(f: F) -> Result<T, IpcError>
where
    F: FnOnce() -> Result<T, IpcError> + Send + 'static,
    T: Send + 'static,
{
    spawn_blocking(f)
        .await
        .map_err(|e| IpcError::internal(format!("blocking task failed: {e}")))?
}

/// 下载期间缓存的设备信息：进度每 250 ms 广播一次快照，内存总量与磁盘可用空间各要一次系统调用。内存总量不会变；
/// 可用空间随下载减少，但界面只在下载前用它判断空间够不够，期间显示开始时的值即可。任务结束时清掉，之后的快照重新读
#[derive(Debug, Clone, Copy)]
struct DownloadEnv {
    total_memory: Option<u64>,
    disk_free: Option<u64>,
}

/// 离线模型服务
pub struct ModelsService {
    settings: Arc<SettingsService>,
    /// 测试里为 None（不发事件）
    app: Option<tauri::AppHandle>,
    store: Store,
    /// 启动受托管子进程（推理进程）；测试里可以换成假的
    launcher: Arc<dyn wisp_local::launch::Launcher>,
    state: Mutex<State>,
    slots: slots::Slots,
    /// 内存压力监听（至少一个槽位加载时才有）
    pressure: Mutex<Option<wisp_platform::pressure::Watch>>,
    /// 设置变化的订阅已经开始（第一次加载槽位时开始）
    watching: AtomicBool,
    /// 崩溃兜底的清扫（孤儿进程、没删完的目录）：启动链里或第一次加载槽位之前，取先到者，只做一次
    swept: tokio::sync::OnceCell<()>,
    /// 上一次进度广播（节流）
    last_progress: Mutex<Option<(Instant, LocalModelId, u64)>>,
    /// 最近一次推理进程失败的种类（i18n key，诊断包用）
    last_failure: Mutex<Option<String>>,
    /// 下载进行中时的设备信息缓存（见 [`DownloadEnv`]）
    download_env: Mutex<Option<DownloadEnv>>,
    /// 测试：缩短闲置释放的时长
    #[cfg(test)]
    idle_override: Mutex<Option<Duration>>,
    /// 测试：固定加载前内存检查看到的内存与压力（不随测试机器变化）
    #[cfg(test)]
    memory_override: Mutex<Option<lifecycle::MemoryProbe>>,
    /// 测试：安装系统的内存压力监听。默认不装：测试机器内存吃紧（例如全量编译测试）时，系统的压力警告会释放测试里的槽位，
    /// 结果就随机器状态变化；用真实模型的测试打开它
    #[cfg(test)]
    os_pressure: AtomicBool,
    /// 测试：换掉真正的下载（访问本机的模拟服务器）
    #[cfg(test)]
    download_override: Mutex<Option<queue::TestDownload>>,
    /// 测试：固定「是不是 Apple 芯片」（在 Apple 芯片的机器上也能走 Windows / Linux 的显卡检测）
    #[cfg(test)]
    apple_override: Mutex<Option<bool>>,
}

/// 可变状态（锁内只做 O(1) 的工作，不跨 await 持有）
#[derive(Default)]
struct State {
    /// 第一次需要时才读；之后随下载、删除更新
    inventory: Option<Inventory>,
    /// 已经关闭（退出流程中）：不再接受新的下载与加载
    shut_down: bool,
    downloads: queue::Queue,
    /// 检测到的显卡（`llama-server --list-devices`）；None = 还没检测
    gpus: Option<Vec<GpuInfo>>,
}

/// 显卡检测结果的缓存（带运行时版本：升级运行时后重新检测）
#[derive(Debug, Serialize, Deserialize)]
struct DevicesCache {
    version: String,
    gpus: Vec<GpuInfo>,
}

impl ModelsService {
    /// 构造（不做任何 I/O）
    pub fn new(d: &Deps) -> Arc<Self> {
        let models_dir = d.settings.paths().models_dir();
        Self::with_parts(
            Arc::clone(&d.settings),
            Some(d.app.clone()),
            models_dir,
            Arc::new(PlatformLauncher),
        )
    }

    /// 由各部件构造（测试用：不带 AppHandle、临时目录、假的启动器）
    pub(crate) fn with_parts(
        settings: Arc<SettingsService>,
        app: Option<tauri::AppHandle>,
        models_dir: PathBuf,
        launcher: Arc<dyn wisp_local::launch::Launcher>,
    ) -> Arc<Self> {
        Arc::new(Self {
            settings,
            app,
            store: Store::new(models_dir),
            launcher,
            state: Mutex::new(State::default()),
            slots: slots::Slots::new(),
            pressure: Mutex::new(None),
            watching: AtomicBool::new(false),
            swept: tokio::sync::OnceCell::new(),
            last_progress: Mutex::new(None),
            last_failure: Mutex::new(None),
            download_env: Mutex::new(None),
            #[cfg(test)]
            idle_override: Mutex::new(None),
            #[cfg(test)]
            memory_override: Mutex::new(None),
            #[cfg(test)]
            os_pressure: AtomicBool::new(false),
            #[cfg(test)]
            download_override: Mutex::new(None),
            #[cfg(test)]
            apple_override: Mutex::new(None),
        })
    }

    /// 模型目录的布局（编排器与诊断用）
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// 推理进程的启动器
    pub fn launcher(&self) -> Arc<dyn wisp_local::launch::Launcher> {
        Arc::clone(&self.launcher)
    }

    fn is_shut_down(&self) -> bool {
        self.state.lock().shut_down
    }

    /// 设置页的完整快照（`offline_models_get`）。第一次调用时在阻塞线程里读库存
    pub async fn get(&self) -> Result<LocalModelsSnapshot, IpcError> {
        let inventory = self.inventory().await;
        Ok(self.snapshot(&inventory))
    }

    /// 删除一个模型：停掉用到它的槽位、删除目录；它被选用时把对应的 `offline.*_model` 置 null
    pub async fn delete(self: &Arc<Self>, model: LocalModelId) -> Result<(), IpcError> {
        // 还在下载：先取消（删除部分文件），并等下载任务收尾——它还开着 `.part`，Windows 上这时把目录改名进回收区会失败
        let (downloading, finished) = {
            let g = self.state.lock();
            let finished = g
                .downloads
                .current
                .as_ref()
                .filter(|a| a.model == model)
                .map(|a| a.finished.clone());
            (g.downloads.busy_with(model), finished)
        };
        if downloading {
            self.cancel(model).await?;
        }
        if let Some(finished) = finished
            && tokio::time::timeout(DOWNLOAD_STOP_WAIT, finished.cancelled())
                .await
                .is_err()
        {
            log::warn!("offline: the download of {} did not stop in time", model.as_str());
        }
        for kind in self.slots_using(model) {
            // ORT 工作进程同时服务 PP-OCR：删 PaddleOCR-VL（只用到它的版面检测）时不打断正在进行的文字识别，做完再停
            let force = !(kind == SlotKind::Ort && model == LocalModelId::PaddleOcrVl16);
            self.retire(kind, force).await;
        }
        let store = self.store.clone();
        run_blocking(move || {
            store
                .delete_model(model)
                .map_err(|_| IpcError::new(IpcErrorCode::Io, "deleting the model failed"))
        })
        .await?;
        let settings = Arc::clone(&self.settings);
        run_blocking(move || {
            settings
                .update(&mut |s| {
                    if s.offline.ocr_model == Some(model) {
                        s.offline.ocr_model = None;
                    }
                    if s.offline.translate_model == Some(model) {
                        s.offline.translate_model = None;
                    }
                })
                .map(|_| ())
        })
        .await?;
        log::info!("offline: model deleted model={}", model.as_str());
        {
            let mut g = self.state.lock();
            g.downloads.states.remove(&model);
            g.downloads.select.remove(&model);
            g.downloads.unconfirmed.remove(&model);
        }
        self.clear_failures(model);
        self.rescan().await;
        self.settings.notify_changed();
        self.broadcast_now();
        Ok(())
    }

    /// 删除一个运行时；仍有已安装的模型依赖它时返回 `invalid_argument`
    pub async fn delete_runtime(self: &Arc<Self>, runtime: LocalRuntimeId) -> Result<(), IpcError> {
        let inventory = self.inventory().await;
        let depended = LocalModelId::ALL
            .into_iter()
            .any(|m| inventory.is_installed(m) && catalog::model(m).runtimes.contains(&runtime));
        if depended {
            return Err(IpcError::invalid_argument(
                "the runtime is still used by an installed model",
            ));
        }
        let downloading = {
            let g = self.state.lock();
            LocalModelId::ALL
                .into_iter()
                .any(|m| g.downloads.busy_with(m) && catalog::model(m).runtimes.contains(&runtime))
        };
        if downloading {
            return Err(IpcError::invalid_argument("the runtime is being downloaded"));
        }
        let kinds: &[SlotKind] = match runtime {
            LocalRuntimeId::LlamaCpp => &[SlotKind::Vl, SlotKind::Translate],
            LocalRuntimeId::OnnxRuntime => &[SlotKind::Ort],
        };
        for kind in kinds {
            self.retire(*kind, true).await;
        }
        let store = self.store.clone();
        run_blocking(move || {
            store
                .delete_runtime(runtime)
                .map_err(|_| IpcError::new(IpcErrorCode::Io, "deleting the runtime failed"))
        })
        .await?;
        if runtime == LocalRuntimeId::LlamaCpp {
            self.state.lock().gpus = None;
        }
        log::info!("offline: runtime deleted runtime={}", runtime.as_str());
        self.rescan().await;
        self.settings.notify_changed();
        self.broadcast_now();
        Ok(())
    }

    /// 立即停止所有空闲的槽位（设置页的「释放内存」）；正在运行的请求做完就停，正在启动的启动完、等着的请求做完再停
    pub async fn release(self: &Arc<Self>) -> Result<(), IpcError> {
        for kind in SlotKind::ALL {
            self.retire(kind, false).await;
        }
        Ok(())
    }

    /// 在访达 / 资源管理器 / 文件管理器里打开模型目录（不存在时先创建）
    pub async fn reveal(&self) -> Result<(), IpcError> {
        let root = self.store.root().to_path_buf();
        let dir = root.clone();
        run_blocking(move || {
            std::fs::create_dir_all(&dir)
                .map_err(|_| IpcError::new(IpcErrorCode::Io, "cannot create the models folder"))
        })
        .await?;
        let Some(app) = &self.app else { return Ok(()) };
        use tauri_plugin_opener::OpenerExt;
        app.opener()
            .open_path(root.to_string_lossy(), None::<&str>)
            .map_err(|_| IpcError::new(IpcErrorCode::Platform, "cannot open the models folder"))
    }

    /// 重新检测显卡（`llama-server --list-devices`），返回新的快照。Apple 芯片不检测（统一内存、Metal）；
    /// llama.cpp 还没装时检测不了，照常返回快照（`gpu_probed = false`）
    pub async fn devices_refresh(self: &Arc<Self>) -> Result<LocalModelsSnapshot, IpcError> {
        let mut inventory = self.inventory().await;
        if !inventory.runtime_installed(LocalRuntimeId::LlamaCpp) {
            // 缓存的库存可能早于推理组件的安装（下载器自己读写磁盘，不更新这里的缓存）：说「没装」时先重读一次再判断
            inventory = self.rescan().await;
        }
        let server = Target::current().and_then(|t| self.store.runtime_main(LocalRuntimeId::LlamaCpp, t));
        let Some(server) =
            server.filter(|_| !self.apple_silicon() && inventory.runtime_installed(LocalRuntimeId::LlamaCpp))
        else {
            return Ok(self.snapshot(&inventory));
        };
        self.housekeeping().await;
        let gpus = wisp_local::llama::list_devices(Arc::clone(&self.launcher), server)
            .await
            .map_err(|e| IpcError::new(IpcErrorCode::Platform, format!("GPU detection failed: {e}")))?;
        log::info!("offline: GPU detection found {} device(s)", gpus.len());
        let cache = DevicesCache {
            version: catalog::LLAMA_CPP_VERSION.to_owned(),
            gpus: gpus.clone(),
        };
        let path = self.devices_file();
        let _ = spawn_blocking(move || {
            if let Ok(bytes) = serde_json::to_vec_pretty(&cache)
                && let Err(e) = crate::settings::fsio::write_atomic(&path, &bytes)
            {
                log::debug!("offline: caching the GPU list failed: {e}");
            }
        })
        .await;
        self.state.lock().gpus = Some(gpus);
        let snapshot = self.snapshot(&inventory);
        self.emit(&snapshot);
        Ok(snapshot)
    }

    fn devices_file(&self) -> PathBuf {
        let dir = self.store.runtime_dir(LocalRuntimeId::LlamaCpp);
        dir.parent()
            .map_or(dir.clone(), std::path::Path::to_path_buf)
            .join(DEVICES_FILE)
    }

    /// 设置快照里的摘要（`RuntimeInfo.offline`）：只读缓存，没读过库存时为空。不阻塞
    pub fn summary(&self) -> OfflineSummary {
        let state = self.state.lock();
        let installed = state.inventory.as_ref().map_or_else(Vec::new, |inv| {
            LocalModelId::ALL.into_iter().filter(|id| usable(inv, *id)).collect()
        });
        OfflineSummary { installed }
    }

    /// 离线模型能不能马上用（编排器的未就绪检查）。第一次调用时读库存（**阻塞**，几个小文件），之后读缓存
    pub fn offline_status(&self, model: LocalModelId) -> OfflineStatus {
        let inventory = match self.cached_inventory() {
            Some(inv) => inv,
            None => {
                let scanned = self.scan_blocking();
                self.state.lock().inventory.get_or_insert(scanned).clone()
            }
        };
        if self.state.lock().downloads.busy_with(model) {
            OfflineStatus::Downloading
        } else if Target::current().is_some() && usable(&inventory, model) {
            OfflineStatus::Ready
        } else {
            OfflineStatus::Missing
        }
    }

    /// 诊断包：已安装的模型（id 与大小）、运行时版本、槽位状态、最近一次失败的种类、内存与显卡（不含路径）
    pub fn diagnostics(&self) -> serde_json::Value {
        let inventory = self.cached_inventory();
        let device = self.device_info();
        let models: Vec<serde_json::Value> = inventory
            .as_ref()
            .map(|inv| {
                LocalModelId::ALL
                    .into_iter()
                    .filter_map(|m| {
                        let state = install_word(inv.models.get(&m)?)?;
                        Some(serde_json::json!({
                            "id": m.as_str(),
                            "state": state,
                            "download_bytes": catalog::model_download_bytes(m),
                        }))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let runtimes: Vec<serde_json::Value> = LocalRuntimeId::ALL
            .into_iter()
            .map(|r| {
                let installed = inventory.as_ref().is_some_and(|inv| inv.runtime_installed(r));
                serde_json::json!({
                    "id": r.as_str(),
                    "version": catalog::runtime(r).version,
                    "installed": installed,
                    "size": inventory.as_ref().and_then(|inv| inv.runtimes.get(&r)).map_or(0, |i| i.size),
                })
            })
            .collect();
        serde_json::json!({
            "inventory_read": inventory.is_some(),
            "models": models,
            "runtimes": runtimes,
            "disk_used": inventory.as_ref().map_or(0, |inv| inv.disk_used),
            "slots": self.slots.describe(),
            "last_failure": *self.last_failure.lock(),
            "memory_total": device.total_memory,
            "memory_available": wisp_platform::system::memory().map(|m| m.available),
            "apple_silicon": device.apple_silicon,
            "gpus": device.gpus.iter().map(|g| serde_json::json!({ "name": g.name, "vram_total_mb": g.vram_total_mb })).collect::<Vec<_>>(),
            "gpu_probed": device.gpu_probed,
        })
    }

    /// 记下最近一次推理进程失败的种类（诊断包）
    fn note_failure_kind(&self, hint: &str) {
        *self.last_failure.lock() = Some(hint.to_owned());
    }

    /// 推理进程报告模型文件缺失或损坏：重读库存并广播。库存平时只在下载与删除之后才重读，不这样做的话设置页仍显示
    /// 「已下载」，而提示让用户去设置里重新下载；重读后按大小核对即显示为损坏，那一行自带「重新下载」
    fn on_model_corrupt(self: &Arc<Self>) {
        let svc = Arc::clone(self);
        spawn(async move {
            svc.rescan().await;
            svc.settings.notify_changed();
            svc.broadcast_now();
        });
    }

    /// 错误是不是「模型文件缺失或损坏」
    fn is_model_corrupt(e: &wisp_types::LlmError) -> bool {
        e.hint.as_deref() == Some(wisp_local::RuntimeFailure::ModelCorrupt.hint_key())
    }

    /// 「下载并使用」：按模型类别选用它（写 `offline.ocr_model` / `offline.translate_model`）。`baseline` 是登记时
    /// 同类别的选择：给出时只在设置仍是那样时才选用（检查与写入在设置的同一次更新里），用户在下载期间改选了别的
    /// （另一个离线模型、云端模型）就以后来的选择为准。返回是否选用了
    async fn select_model(&self, model: LocalModelId, baseline: Option<queue::Baseline>) -> Result<bool, IpcError> {
        let settings = Arc::clone(&self.settings);
        let selected = run_blocking(move || {
            let mut selected = false;
            settings.update(&mut |s| {
                selected = baseline
                    .as_ref()
                    .is_none_or(|b| *b == queue::Baseline::of(s, model.kind()));
                if selected {
                    match model.kind() {
                        wisp_types::LocalModelKind::Ocr => s.offline.ocr_model = Some(model),
                        wisp_types::LocalModelKind::Translate => s.offline.translate_model = Some(model),
                    }
                }
            })?;
            Ok(selected)
        })
        .await?;
        if selected {
            log::info!("offline: {} selected", model.as_str());
        } else {
            log::info!(
                "offline: {} not selected after download (the choice changed meanwhile)",
                model.as_str()
            );
        }
        Ok(selected)
    }

    /// 模型重新装好或删掉之后：清掉可能用过它的槽位里这个模型的启动失败记录。文件损坏连续失败两次之后 30 s 内不再
    /// 重启、直接报同样的错误——重新下载（几秒就装好）之后不应再被旧记录拦住
    fn clear_failures(&self, model: LocalModelId) {
        for kind in SlotKind::of_model(model) {
            let mut g = self.slots.get(*kind).inner.lock();
            if g.failure.as_ref().is_some_and(|f| f.model == model) {
                g.failure = None;
            }
        }
    }

    /// 退出前的收尾（幂等）：停止下载（变为暂停）、关闭全部槽位（最多等 1 s 再强制结束）、删除 pid 文件。阻塞
    pub fn shutdown(&self) {
        {
            let mut state = self.state.lock();
            if state.shut_down {
                return;
            }
            state.shut_down = true;
        }
        let t0 = Instant::now();
        self.shutdown_downloads();
        self.shutdown_slots();
        log::info!("offline: shut down in {} ms", t0.elapsed().as_millis());
    }

    /// 启动链里的一步（托盘就绪之后、空闲时）：模型目录存在时才做——清扫上次崩溃留下的推理进程（只在有 pid 文件时）、
    /// 删掉没删完的目录、读一次库存（面板的模型菜单据此知道装了哪些）
    pub fn spawn_startup(self: &Arc<Self>) {
        let svc = Arc::clone(self);
        spawn(async move {
            let root = svc.store.root().to_path_buf();
            let exists = spawn_blocking(move || root.is_dir()).await.unwrap_or(false);
            if !exists {
                return;
            }
            svc.housekeeping().await;
            let inventory = svc.inventory().await;
            if LocalModelId::ALL.into_iter().any(|m| usable(&inventory, m)) {
                svc.settings.notify_changed();
            }
        });
    }

    /// 崩溃兜底的清扫（只做一次；在第一次写自己的 pid 文件之前）
    async fn housekeeping(&self) {
        let store = self.store.clone();
        self.swept
            .get_or_init(|| async move {
                let _ = spawn_blocking(move || {
                    let run = store.run_dir();
                    let has_pid = std::fs::read_dir(&run).is_ok_and(|entries| {
                        entries
                            .flatten()
                            .any(|e| e.path().extension().is_some_and(|x| x == "pid"))
                    });
                    if has_pid {
                        log::info!("offline: pid files found; sweeping orphaned inference processes");
                        wisp_platform::child::sweep_orphans(&run);
                    }
                    if store.root().is_dir() {
                        store.purge_trash();
                    }
                })
                .await;
            })
            .await;
    }

    /// 读取（必要时第一次读）库存
    async fn inventory(&self) -> Inventory {
        if let Some(inv) = self.cached_inventory() {
            return inv;
        }
        self.rescan_inner(false).await
    }

    /// 重新读库存（下载、删除之后）
    async fn rescan(&self) -> Inventory {
        self.rescan_inner(true).await
    }

    async fn rescan_inner(&self, replace: bool) -> Inventory {
        let store = self.store.clone();
        let devices = self.devices_file();
        let scanned = spawn_blocking(move || (scan(&store), read_devices(&devices))).await;
        let (inv, gpus) = match scanned {
            Ok(v) => v,
            Err(e) => {
                log::warn!("offline: inventory task failed: {e}");
                (Inventory::default(), None)
            }
        };
        let mut g = self.state.lock();
        if g.gpus.is_none() {
            g.gpus = gpus;
        }
        if replace {
            g.inventory = Some(inv.clone());
            inv
        } else {
            g.inventory.get_or_insert(inv).clone()
        }
    }

    /// 同步读库存（编排器的未就绪检查在阻塞线程里调用）
    fn scan_blocking(&self) -> Inventory {
        let gpus = read_devices(&self.devices_file());
        let inv = scan(&self.store);
        let mut g = self.state.lock();
        if g.gpus.is_none() {
            g.gpus = gpus;
        }
        inv
    }

    fn cached_inventory(&self) -> Option<Inventory> {
        self.state.lock().inventory.clone()
    }

    /// 是不是 Apple 芯片（测试可以固定）
    fn apple_silicon(&self) -> bool {
        #[cfg(test)]
        if let Some(apple) = *self.apple_override.lock() {
            return apple;
        }
        wisp_platform::system::is_apple_silicon()
    }

    /// 内存总量（测试里跟随固定的内存检查，设备适配因此不随测试机器变化）
    fn total_memory(&self) -> Option<u64> {
        #[cfg(test)]
        if let Some(p) = *self.memory_override.lock() {
            return p.memory.map(|m| m.total);
        }
        wisp_platform::system::memory().map(|m| m.total)
    }

    /// 本机硬件概况：显卡用缓存的检测结果；Apple 芯片视为已检测（统一内存、Metal）
    fn device_info(&self) -> DeviceInfo {
        let cached = *self.download_env.lock();
        self.device_info_with(cached.map_or_else(|| self.total_memory(), |env| env.total_memory))
    }

    fn device_info_with(&self, total_memory: Option<u64>) -> DeviceInfo {
        let apple_silicon = self.apple_silicon();
        let gpus = self.state.lock().gpus.clone();
        DeviceInfo {
            total_memory,
            apple_silicon,
            gpu_probed: apple_silicon || gpus.is_some(),
            // 核显要结合系统内存判断（显存占到系统内存一半左右的未知型号也算），这里统一标出来，界面与适配评估都用它
            gpus: gpus
                .unwrap_or_default()
                .into_iter()
                .map(|g| wisp_types::GpuInfo {
                    integrated: wisp_local::llama::is_integrated_gpu(&g, total_memory),
                    ..g
                })
                .collect(),
        }
    }

    /// 下载开始：取一次设备信息与可用空间，进度广播期间复用（阻塞很短：两次系统调用）
    fn begin_download_env(&self) {
        let env = DownloadEnv {
            total_memory: self.total_memory(),
            disk_free: wisp_platform::system::disk_free(self.store.root()),
        };
        *self.download_env.lock() = Some(env);
    }

    /// 下载结束：之后的快照重新读设备信息与可用空间
    fn end_download_env(&self) {
        *self.download_env.lock() = None;
    }

    /// 按当前库存、设备、下载队列、槽位与设置组装快照
    fn snapshot(&self, inventory: &Inventory) -> LocalModelsSnapshot {
        let settings = self.settings.get();
        let cached = *self.download_env.lock();
        let (device, disk_free) = match cached {
            Some(env) => (self.device_info_with(env.total_memory), env.disk_free),
            None => (self.device_info(), wisp_platform::system::disk_free(self.store.root())),
        };
        let active = self.state.lock().downloads.states.clone();
        let loads = self.slots.loads();
        snapshot::build(&snapshot::Inputs {
            inventory,
            device,
            offline: &settings.offline,
            target: Target::current(),
            active: &active,
            loads: &loads,
            disk_free,
        })
    }

    /// 立即广播快照（状态变化：排队、开始、完成、失败、槽位加载 / 释放）
    fn broadcast_now(&self) {
        if self.app.is_none() {
            return;
        }
        let inventory = self.cached_inventory().unwrap_or_default();
        let snapshot = self.snapshot(&inventory);
        self.emit(&snapshot);
    }

    /// 下载进度的广播：距上一次 ≥ 250 ms 或进度变化 ≥ 1% 才发
    fn broadcast_progress(&self, model: LocalModelId, done: u64, total: u64) {
        let permille = done.saturating_mul(1000).checked_div(total).unwrap_or(0);
        let now = Instant::now();
        {
            let mut last = self.last_progress.lock();
            if let Some((at, m, p)) = *last
                && m == model
                && now.duration_since(at) < PROGRESS_INTERVAL
                && permille.abs_diff(p) < 10
            {
                return;
            }
            *last = Some((now, model, permille));
        }
        self.broadcast_now();
    }

    /// 广播快照（`offline_models:changed`）
    fn emit(&self, snapshot: &LocalModelsSnapshot) {
        let Some(app) = &self.app else { return };
        if let Err(e) = app.emit(event::OFFLINE_MODELS_CHANGED, snapshot) {
            log::debug!("emit offline_models:changed failed: {e}");
        }
    }
}

/// 诊断包里的安装状态（固定的词，不用 Debug 文本）；没安装的不列出
fn install_word(state: &InstallState) -> Option<&'static str> {
    match state {
        InstallState::NotInstalled => None,
        InstallState::Installed { stale: false } => Some("installed"),
        InstallState::Installed { stale: true } => Some("stale"),
        InstallState::Broken => Some("broken"),
    }
}

/// 模型与它需要的运行时都已安装
fn usable(inv: &Inventory, model: LocalModelId) -> bool {
    inv.is_installed(model) && catalog::model(model).runtimes.iter().all(|r| inv.runtime_installed(*r))
}

/// 读库存（阻塞）；读不了时当作空库存（目录不存在、权限问题），只记 debug
fn scan(store: &Store) -> Inventory {
    match store.scan() {
        Ok(inv) => inv,
        Err(e) => {
            log::debug!("offline: inventory unavailable: {e}");
            Inventory::default()
        }
    }
}

/// 读缓存的显卡检测结果（阻塞）；版本不符（运行时升级了）时当作没检测过
fn read_devices(path: &std::path::Path) -> Option<Vec<GpuInfo>> {
    let bytes = std::fs::read(path).ok()?;
    let cache: DevicesCache = serde_json::from_slice(&bytes).ok()?;
    (cache.version == catalog::LLAMA_CPP_VERSION).then_some(cache.gpus)
}
