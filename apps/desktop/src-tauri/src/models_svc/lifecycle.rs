//! 槽位的生命周期：取得租约（必要时单飞启动）、闲置释放、内存压力、强制停止、设置变化。

use std::sync::Arc;
use std::time::{Duration, Instant};

use std::path::Path;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use wisp_local::catalog::{self, Target};
use wisp_local::llama::{self, Server, ServerConfig};
use wisp_local::ocr::PpOcrTier;
use wisp_local::worker::{OrtWorker, WorkerConfig};
use wisp_local::{LocalError, RuntimeFailure};
use wisp_platform::pressure::Level;
use wisp_platform::system::MemoryInfo;
use wisp_types::{ErrorCode, LlmError, LocalModelId, LocalRuntimeId, OfflineSettings};

use super::ModelsService;
use super::slots::{Failure, Loaded, Phase, Proc, STOP_GRACE, SlotInner, SlotKind, StartResult, StopTicket};

/// 版面检测会话（PP-DocLayoutV3）加上 ORT 本身大约占用的内存（MB）。PaddleOCR-VL 的估计值只算 llama-server 那部分
const LAYOUT_MEMORY_MB: u64 = 400;
/// 一 MB
const MB: u64 = 1024 * 1024;
/// 会压缩内存的系统（macOS）上，一个模型加上其他已加载的槽位最多占物理内存的这个比例。macOS 报告的「可用」只有空闲与
/// 可直接回收的页，不含文件缓存与可压缩的匿名内存（开着浏览器的 16 GB 机器上常常只有 2–5 GB），拿它比会把 16 GB 机器上
/// 能正常运行的 9B 拒掉；llama.cpp 用 mmap 加载模型，系统可以按需压缩或换出别的页。所以改与总量比，并以系统的内存压力为准
const UNIFIED_SHARE_PERCENT: u64 = 70;
/// Windows / Linux 的「可用」（`ullAvailPhys` 含备用列表、`MemAvailable`）已经算上了可回收的缓存，可以直接比较；另留这么多
/// 余量（MB），给推理时的 KV 缓存与计算缓冲、以及其他程序的增长
const HEADROOM_MB: u64 = 512;

/// 加载前内存检查看到的情况
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MemoryProbe {
    pub(super) memory: Option<MemoryInfo>,
    pub(super) pressure: Option<Level>,
    /// 系统会压缩、换出其他程序的页来腾地方，「可用」偏低、不能直接拿来比较（macOS）
    pub(super) compressing: bool,
}

impl MemoryProbe {
    /// 现在的情况（几次系统调用，不常驻、不轮询）。Wisp 在 macOS 上只支持 Apple 芯片，「是 Apple 芯片」即「是 macOS」
    fn current() -> Self {
        Self {
            memory: wisp_platform::system::memory(),
            pressure: wisp_platform::pressure::current(),
            compressing: wisp_platform::system::is_apple_silicon(),
        }
    }
}

/// 加载前内存检查的结论
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Budget {
    /// 直接加载
    Fits,
    /// 先停掉其他空闲的槽位，再判断一次
    FreeOthers,
    /// 不加载，报内存不足
    Refuse,
}

/// 加载前的内存判断（纯函数）。`need` 是这个模型的估计值，`others` 是其他已加载槽位的估计值之和（都是字节）；
/// `freed` = 已经停过其他空闲的槽位，这一次是复查。读不到内存时不拦（加载失败会按进程的输出归类）
pub(super) fn judge(need: u64, others: u64, probe: &MemoryProbe, freed: bool) -> Budget {
    let Some(mem) = probe.memory else {
        return Budget::Fits;
    };
    if probe.compressing {
        let cap = mem.total / 100 * UNIFIED_SHARE_PERCENT;
        if need > cap {
            // 停掉别的也装不下
            return Budget::Refuse;
        }
        if probe.pressure == Some(Level::Critical) {
            return if freed { Budget::Refuse } else { Budget::FreeOthers };
        }
        // 「警告」在开着浏览器的 Mac 上很常见，单凭它不停别的槽位：否则离线 OCR + 离线翻译的直接翻译每次都要互相停掉
        // 对方、两边都冷启动，而为加载一个模型停掉几百 MB 的 ORT 也得不偿失。只有加上其他已加载的模型超过上限时才腾；
        // 压力转入警告时，压力监听本来就会停掉空闲的槽位
        if !freed && need.saturating_add(others) > cap {
            return Budget::FreeOthers;
        }
        return Budget::Fits;
    }
    let warning = matches!(probe.pressure, Some(Level::Warning | Level::Critical));
    if need.saturating_add(HEADROOM_MB * MB) > mem.available {
        return if freed { Budget::Refuse } else { Budget::FreeOthers };
    }
    if !freed && warning {
        return Budget::FreeOthers;
    }
    Budget::Fits
}

/// 一个槽位为一个模型加载时的内存估计（字节）。ORT 槽位为 PaddleOCR-VL 启动时只有版面检测
fn need_bytes(kind: SlotKind, model: LocalModelId) -> u64 {
    let mb = match (kind, PpOcrTier::of(model)) {
        (SlotKind::Ort, None) => LAYOUT_MEMORY_MB,
        _ => u64::from(catalog::model(model).memory_estimate_mb),
    };
    mb * MB
}

/// 同一次请求要同时用到的槽位（PaddleOCR-VL：版面检测 + 识别）。为其中一个腾内存时不能停掉另一个——它可能刚就绪、
/// 还没被这次请求取得租约
fn siblings(model: LocalModelId) -> &'static [SlotKind] {
    match model {
        LocalModelId::PaddleOcrVl16 => &[SlotKind::Ort, SlotKind::Vl],
        _ => &[],
    }
}

/// 一次请求对槽位的使用权：在、槽位就不会被闲置释放；放手时重排闲置定时器
pub(crate) struct Lease {
    svc: Arc<ModelsService>,
    kind: SlotKind,
    loaded: Option<Arc<Loaded>>,
}

impl Lease {
    pub(super) fn loaded(&self) -> &Loaded {
        self.loaded
            .as_deref()
            .expect("a lease holds its slot until it is dropped")
    }

    /// ORT 工作进程（ORT 槽位）
    pub(super) fn ort(&self) -> Option<&OrtWorker> {
        match &self.loaded().proc {
            Proc::Ort(w) => Some(w),
            Proc::Llama { .. } => None,
        }
    }

    /// llama-server 的地址与这次启动的 api-key（vl / 翻译槽位）
    pub(super) fn endpoint(&self) -> Option<(&str, &str)> {
        match &self.loaded().proc {
            Proc::Llama { base_url, api_key, .. } => Some((base_url.as_str(), api_key.as_str())),
            Proc::Ort(_) => None,
        }
    }

    /// 强制停止时触发（请求据此尽快放手）
    pub(super) fn cancelled(&self) -> CancellationToken {
        self.loaded().cancel.clone()
    }

    /// 请求失败了：进程已经退出时把错误换成归类后的原因，并清掉这个槽位（下一次请求重新启动）
    pub(super) fn classify(&self, err: LlmError) -> LlmError {
        if err.code == ErrorCode::Cancelled {
            return err;
        }
        match self.loaded().exited() {
            Some(failure) => {
                log::warn!(
                    "offline: {} slot exited during a request ({:?})",
                    self.kind.name(),
                    failure
                );
                self.svc.note_failure_kind(failure.hint_key());
                let generation = self.loaded().generation;
                let svc = Arc::clone(&self.svc);
                let kind = self.kind;
                super::spawn(async move { svc.drop_dead(kind, generation).await });
                failure.to_llm_error()
            }
            None => err,
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(loaded) = self.loaded.take() {
            let generation = loaded.generation;
            drop(loaded);
            self.svc.lease_released(self.kind, generation);
        }
    }
}

/// [`ModelsService::step`] 的结果
enum Step {
    Leased(Lease),
    Failed(LlmError),
    /// 槽位里是别的模型：停掉它（锁外）再来
    Replace(Arc<Loaded>, StopTicket),
    /// 等正在进行的启动
    Wait(watch::Receiver<StartResult>),
}

fn cancelled() -> LlmError {
    LlmError::new(ErrorCode::Cancelled, "cancelled")
}

/// 推理线程数：性能核的一半（与前台程序留出余量），llama-server 2–8、ORT 1–4
fn threads(min: usize, max: usize) -> u32 {
    let n = (wisp_platform::system::performance_cores() / 2).clamp(min, max);
    u32::try_from(n).unwrap_or(2)
}

impl ModelsService {
    /// 取得槽位的租约：已就绪直接返回；否则（必要时启动）等它就绪。等待之前调用一次 `on_loading`（冷启动的阶段提示）。
    /// `cancel` 只结束这次等待，不打断启动
    pub(crate) async fn acquire<F, Fut>(
        self: &Arc<Self>,
        kind: SlotKind,
        model: LocalModelId,
        cancel: &CancellationToken,
        on_loading: F,
    ) -> Result<Lease, LlmError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        self.lease(kind, model, cancel, on_loading, false).await
    }

    /// [`Self::acquire`]；`warm` = 安装后的预热（不算真实请求：没有真实请求等过、用过的话，放手即停）
    async fn lease<F, Fut>(
        self: &Arc<Self>,
        kind: SlotKind,
        model: LocalModelId,
        cancel: &CancellationToken,
        on_loading: F,
        warm: bool,
    ) -> Result<Lease, LlmError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let mut on_loading = Some(on_loading);
        loop {
            if cancel.is_cancelled() {
                return Err(cancelled());
            }
            let mut rx = match self.step(kind, model, warm) {
                Step::Leased(lease) => return Ok(lease),
                Step::Failed(e) => return Err(e),
                Step::Replace(old, ticket) => {
                    // 换了模型（翻译 2B ↔ 9B）：停掉旧的再启动
                    self.stop_loaded(kind, old, ticket).await;
                    continue;
                }
                Step::Wait(rx) => rx,
            };
            if let Some(f) = on_loading.take() {
                f().await;
            }
            let outcome = tokio::select! {
                () = cancel.cancelled() => return Err(cancelled()),
                r = rx.wait_for(Option::is_some) => r.map(|v| v.clone()),
            };
            match outcome {
                // 这次启动就是为它（或 ORT 槽位，服务全部 OCR 模型）：失败原样报给请求方
                Ok(Some((m, Err(e)))) if kind.serves(m, model) => return Err(e),
                // 就绪、启动的是别的模型（翻译槽位正在启动 2B 时来了 9B 的请求）、或启动被停止（发送端丢弃）：
                // 回到循环重新看状态
                _ => {}
            }
        }
    }

    /// [`Self::acquire`] 的一步：只在锁里看状态、做 O(1) 的转换，等待与停止在锁外
    fn step(self: &Arc<Self>, kind: SlotKind, model: LocalModelId, warm: bool) -> Step {
        let slot = self.slots.get(kind);
        let mut g = slot.inner.lock();
        if self.is_shut_down() {
            return Step::Failed(LlmError::new(ErrorCode::Internal, "Wisp is quitting"));
        }
        let mut dead = None;
        if let Some(l) = g.ready_for(kind, model).cloned() {
            if l.exited().is_none() {
                g.busy += 1;
                g.wanted |= !warm;
                g.cancel_timer();
                drop(g);
                if kind == SlotKind::Ort && PpOcrTier::of(model).is_some() && l.used.lock().insert(model) {
                    self.broadcast_now();
                }
                return Step::Leased(Lease {
                    svc: Arc::clone(self),
                    kind,
                    loaded: Some(l),
                });
            }
            // 进程已经退出（被系统结束、崩溃）：清掉它（锁外停止），这一轮重新启动
            log::info!("offline: {} slot pid={} is gone; restarting", kind.name(), l.pid);
            g.phase = Phase::Stopped;
            g.busy = 0;
            g.cancel_timer();
            dead = Some((l, StopTicket::issue(self, &mut g, kind)));
        }
        let (step, start) = match &g.phase {
            Phase::Ready(l) => {
                let l = Arc::clone(l);
                g.phase = Phase::Stopped;
                g.busy = 0;
                g.cancel_timer();
                let ticket = StopTicket::issue(self, &mut g, kind);
                (Step::Replace(l, ticket), None)
            }
            Phase::Starting { done, .. } => {
                let rx = done.clone();
                g.wanted |= !warm;
                (Step::Wait(rx), None)
            }
            Phase::Stopped => match g.failure.as_ref().filter(|f| f.blocks(model, Instant::now())) {
                Some(f) => (Step::Failed(f.error.clone()), None),
                None => {
                    let (tx, rx) = watch::channel(None);
                    let token = CancellationToken::new();
                    let attempt = g.next_generation;
                    g.next_generation += 1;
                    g.phase = Phase::Starting {
                        model,
                        attempt,
                        done: rx.clone(),
                        cancel: token.clone(),
                    };
                    g.stop_when_idle = false;
                    g.wanted = !warm;
                    let guard = StartGuard {
                        svc: Arc::clone(self),
                        kind,
                        model,
                        attempt,
                        tx: Some(tx),
                    };
                    (Step::Wait(rx), Some((guard, token)))
                }
            },
        };
        // 停止凭据与启动的收尾保证析构时都要锁这个槽位：先放锁再交给任务
        drop(g);
        if let Some((l, ticket)) = dead {
            self.spawn_stop(kind, l, ticket);
        }
        if let Some((guard, token)) = start {
            let svc = Arc::clone(self);
            super::spawn(async move {
                svc.run_start(guard, token).await;
            });
            self.broadcast_now();
        }
        step
    }

    /// 启动任务：不依赖请求方（请求方取消只是不再等）
    async fn run_start(self: Arc<Self>, mut guard: StartGuard, token: CancellationToken) {
        let (kind, model, attempt) = (guard.kind, guard.model, guard.attempt);
        let t0 = Instant::now();
        let result = self.start(kind, model, attempt, &token).await;
        let slot = self.slots.get(kind);
        let mut g = slot.inner.lock();
        let ours = matches!(&g.phase, Phase::Starting { attempt: a, .. } if *a == attempt);
        // 这次启动被强制停止过（删除、退出）：那时登记的停止流程由这里收尾
        let owed = g.abandoned.remove(&attempt).then(|| StopTicket::adopt(&self, kind));
        let outcome = match result {
            Ok(loaded) if ours && !token.is_cancelled() && !self.is_shut_down() => {
                log::info!(
                    "offline: {} slot ready model={} pid={} in {} ms",
                    kind.name(),
                    model.as_str(),
                    loaded.pid,
                    t0.elapsed().as_millis()
                );
                let loaded = Arc::new(loaded);
                // 换成 Ready 时放掉了 Starting 里那份接收端：剩下的接收端都是还在等这次启动的请求
                g.phase = Phase::Ready(Arc::clone(&loaded));
                g.busy = 0;
                g.failure = None;
                if g.stop_when_idle && guard.waiting() == 0 {
                    // 启动期间被要求停止（释放内存、改了设置），又没有请求在等：不留进程
                    g.phase = Phase::Stopped;
                    let ticket = StopTicket::issue(&self, &mut g, kind);
                    drop(g);
                    self.spawn_stop(kind, loaded, ticket);
                    guard.send(Ok(()));
                    self.after_slot_change();
                    return;
                }
                // 等着的请求马上会取得租约（并取消这个定时器）；请求方都已取消时由它在闲置后释放。启动期间被要求停止的，
                // 等着的请求做完就停（`release_lease`）；它们万一恰在这一刻都取消了、谁也不来放手，用一个短的定时器收尾，
                // 不让进程留到默认的几分钟之后
                let idle = if g.stop_when_idle {
                    STOP_GRACE
                } else {
                    self.idle_duration()
                };
                self.schedule_idle_in(&mut g, kind, loaded.generation, idle);
                Ok(())
            }
            Ok(loaded) => {
                // 启动期间被停止（换模型、删除、退出）：不留进程
                if ours {
                    g.phase = Phase::Stopped;
                }
                let ticket = match owed {
                    Some(t) => t,
                    None => StopTicket::issue(&self, &mut g, kind),
                };
                drop(g);
                self.spawn_stop(kind, Arc::new(loaded), ticket);
                guard.send(Err(cancelled()));
                self.after_slot_change();
                return;
            }
            Err(e) => {
                if ours {
                    g.phase = Phase::Stopped;
                    if e.code != ErrorCode::Cancelled {
                        log::warn!(
                            "offline: {} slot failed to start model={} ({:?}, {})",
                            kind.name(),
                            model.as_str(),
                            e.code,
                            e.hint.as_deref().unwrap_or("-")
                        );
                        let next = Failure::next(g.failure.as_ref(), model, e.clone(), Instant::now());
                        g.failure = Some(next);
                    }
                }
                Err(e)
            }
        };
        drop(g);
        drop(owed);
        if outcome.as_ref().is_err_and(Self::is_model_corrupt) {
            self.on_model_corrupt();
        }
        if let Err(e) = &outcome
            && let Some(hint) = &e.hint
        {
            self.note_failure_kind(hint);
        }
        guard.send(outcome);
        self.after_slot_change();
    }

    /// 启动一个槽位（加载前的内存预算、启动进程、等就绪）
    async fn start(
        self: &Arc<Self>,
        kind: SlotKind,
        model: LocalModelId,
        generation: u64,
        token: &CancellationToken,
    ) -> Result<Loaded, LlmError> {
        // 崩溃兜底的清扫必须在写自己的 pid 文件之前做完（否则会把刚启动的进程当成孤儿）
        self.housekeeping().await;
        let target = Target::current().ok_or_else(|| LocalError::NotInstalled.to_llm_error())?;
        let inventory = self.inventory().await;
        let spec = catalog::model(model);
        if !inventory.is_installed(model) || !spec.runtimes.iter().all(|r| inventory.runtime_installed(*r)) {
            return Err(LocalError::NotInstalled.to_llm_error());
        }
        // 同一槽位的旧进程还在收尾（崩溃后重启、换模型、预热刚结束、闲置到期）：等它走完再启动，见 `Slot::wait_stops`。
        // 放在内存检查之前：Windows / Linux 的「可用」里还扣着旧进程时，会把装得下的模型判成要腾内存甚至内存不足
        self.slots.get(kind).wait_stops(STOP_GRACE * 3).await;
        if token.is_cancelled() {
            return Err(cancelled());
        }
        self.memory_budget(kind, model).await?;
        if token.is_cancelled() {
            return Err(cancelled());
        }
        let offline = self.settings.get().offline;
        let cancel = CancellationToken::new();
        let proc = match kind {
            SlotKind::Ort => {
                let dylib = self
                    .store
                    .runtime_main(LocalRuntimeId::OnnxRuntime, target)
                    .ok_or_else(|| LocalError::NotInstalled.to_llm_error())?;
                let exe = self
                    .launcher
                    .current_exe()
                    .map_err(|_| RuntimeFailure::StartFailed.to_llm_error())?;
                let cfg = WorkerConfig {
                    exe,
                    dylib,
                    models: self.store.root().to_path_buf(),
                    threads: threads(1, 4),
                };
                let launcher = Arc::clone(&self.launcher);
                let spawned = super::spawn_blocking(move || OrtWorker::spawn(&*launcher, &cfg))
                    .await
                    .map_err(|_| RuntimeFailure::StartFailed.to_llm_error())?;
                Proc::Ort(spawned.map_err(|e| e.to_llm_error())?)
            }
            SlotKind::Vl | SlotKind::Translate => {
                let cfg = self.server_config(kind, model, target, &offline)?;
                let server = Server::start(Arc::clone(&self.launcher), cfg, token.clone())
                    .await
                    .map_err(|e| e.to_llm_error())?;
                Proc::Llama {
                    base_url: server.base_url(),
                    api_key: server.api_key().to_owned(),
                    server: parking_lot::Mutex::new(Some(server)),
                }
            }
        };
        let pid = match &proc {
            Proc::Ort(w) => w.pid(),
            Proc::Llama { server, .. } => server.lock().as_ref().map_or(0, Server::pid),
        };
        let used = if kind == SlotKind::Ort && PpOcrTier::of(model).is_some() {
            std::iter::once(model).collect()
        } else {
            Default::default()
        };
        Ok(Loaded {
            model,
            generation,
            proc,
            cancel,
            used: parking_lot::Mutex::new(used),
            pid,
        })
    }

    /// llama-server 的启动参数
    fn server_config(
        &self,
        kind: SlotKind,
        model: LocalModelId,
        target: Target,
        offline: &OfflineSettings,
    ) -> Result<ServerConfig, LlmError> {
        let corrupt = || RuntimeFailure::ModelCorrupt.to_llm_error();
        let server = self
            .store
            .runtime_main(LocalRuntimeId::LlamaCpp, target)
            .ok_or_else(corrupt)?;
        let files = catalog::gguf(model).ok_or_else(corrupt)?;
        let dir = self.store.model_dir(model);
        let model_file = dir.join(files.model);
        let mmproj = match kind {
            SlotKind::Vl => Some(dir.join(files.mmproj.ok_or_else(corrupt)?)),
            _ => None,
        };
        let device = self.device_info();
        Ok(ServerConfig {
            slot: match kind {
                SlotKind::Vl => llama::Slot::Vl,
                _ => llama::Slot::Translate,
            },
            server,
            model: model_file,
            mmproj,
            ctx: llama::DEFAULT_CTX,
            threads: threads(2, 8),
            use_gpu: offline.use_gpu,
            // 核显也算：推理照常交给 llama.cpp 自动卸载到显卡（与 `gpu_in_use` 同一口径），计算在显卡上时 CPU 线程不必忙等
            has_gpu: has_any_gpu(&device),
            idle_release_s: offline.idle_release_min.saturating_mul(60),
        })
    }

    /// 加载前内存检查看到的情况（测试里可以固定）
    fn memory_probe(&self) -> MemoryProbe {
        #[cfg(test)]
        if let Some(p) = *self.memory_override.lock() {
            return p;
        }
        MemoryProbe::current()
    }

    /// 可以为腾内存而停掉的槽位（除了自己与同一次请求要用到的）
    fn other_slots(kind: SlotKind, model: LocalModelId) -> Vec<SlotKind> {
        SlotKind::ALL
            .into_iter()
            .filter(|k| *k != kind && !siblings(model).contains(k))
            .collect()
    }

    /// 这些槽位里已加载（或正在加载）的模型的估计值之和（字节）
    fn loaded_bytes(&self, kinds: &[SlotKind]) -> u64 {
        kinds
            .iter()
            .filter_map(|k| self.loaded_model(*k).map(|m| need_bytes(*k, m)))
            .sum()
    }

    /// 加载前的内存检查（规则见 [`judge`]）：需要时先停掉其他空闲的槽位再复查；仍不够就不启动
    async fn memory_budget(self: &Arc<Self>, kind: SlotKind, model: LocalModelId) -> Result<(), LlmError> {
        let need = need_bytes(kind, model);
        let others = Self::other_slots(kind, model);
        let mut probe = self.memory_probe();
        let mut verdict = judge(need, self.loaded_bytes(&others), &probe, false);
        if verdict == Budget::FreeOthers {
            if self.stop_idle(&others).await > 0 {
                // 进程退出后系统要一点时间回收页面
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            probe = self.memory_probe();
            verdict = judge(need, self.loaded_bytes(&others), &probe, true);
        }
        if verdict == Budget::Refuse {
            log::warn!(
                "offline: not enough memory for {} ({} MB needed, total {} MB, available {} MB, pressure {:?})",
                model.as_str(),
                need / MB,
                probe.memory.map_or(0, |m| m.total / MB),
                probe.memory.map_or(0, |m| m.available / MB),
                probe.pressure
            );
            return Err(RuntimeFailure::OutOfMemory.to_llm_error());
        }
        Ok(())
    }

    /// 不停任何槽位就能加载（安装后预热只在这种时候做）
    fn memory_fits(&self, kind: SlotKind, model: LocalModelId) -> bool {
        let others = Self::other_slots(kind, model);
        judge(
            need_bytes(kind, model),
            self.loaded_bytes(&others),
            &self.memory_probe(),
            false,
        ) == Budget::Fits
    }

    /// 租约放手：最后一个放手时重排闲置定时器（内存严重不足或设置变了时直接停）
    pub(super) fn lease_released(self: &Arc<Self>, kind: SlotKind, generation: u64) {
        self.release_lease(kind, generation, false);
    }

    /// 预热的租约放手：没有真实请求等过、用过这次启动就立即停（与检查同在一次加锁里，不会停掉刚取得租约的请求）
    fn release_warm(self: &Arc<Self>, mut lease: Lease) {
        if let Some(loaded) = lease.loaded.take() {
            let generation = loaded.generation;
            drop(loaded);
            self.release_lease(lease.kind, generation, true);
        }
    }

    fn release_lease(self: &Arc<Self>, kind: SlotKind, generation: u64, warm: bool) {
        let slot = self.slots.get(kind);
        let mut g = slot.inner.lock();
        let current = matches!(&g.phase, Phase::Ready(l) if l.generation == generation);
        if current {
            g.busy = g.busy.saturating_sub(1);
            if g.busy == 0 {
                // 预热与真实请求共享了这次启动（真实请求在等它、或在它就绪后用过）时，预热最后放手也按普通的放手处理：
                // 用户刚用过的进程照常闲置后再释放，不必下一次又冷启动
                if g.stop_when_idle || (warm && !g.wanted) {
                    if let Phase::Ready(l) = std::mem::replace(&mut g.phase, Phase::Stopped) {
                        g.cancel_timer();
                        let ticket = StopTicket::issue(self, &mut g, kind);
                        drop(g);
                        slot.released.notify_waiters();
                        self.spawn_stop(kind, l, ticket);
                        return;
                    }
                } else {
                    self.schedule_idle(&mut g, kind, generation);
                }
            }
        }
        drop(g);
        slot.released.notify_waiters();
    }

    /// 闲置时长（`offline.idle_release_min` 分钟；测试可以缩短）
    fn idle_duration(&self) -> Duration {
        #[cfg(test)]
        if let Some(d) = *self.idle_override.lock() {
            return d;
        }
        Duration::from_secs(u64::from(self.settings.get().offline.idle_release_min.max(1)) * 60)
    }

    /// 排一个一次性闲置定时器（替换旧的）
    pub(super) fn schedule_idle(self: &Arc<Self>, g: &mut SlotInner, kind: SlotKind, generation: u64) {
        let idle = self.idle_duration();
        self.schedule_idle_in(g, kind, generation, idle);
    }

    /// 排一个 `idle` 之后到期的一次性闲置定时器（替换旧的）
    fn schedule_idle_in(self: &Arc<Self>, g: &mut SlotInner, kind: SlotKind, generation: u64, idle: Duration) {
        g.cancel_timer();
        let svc = Arc::clone(self);
        g.timer = Some(super::spawn(async move {
            tokio::time::sleep(idle).await;
            svc.idle_expired(kind, generation).await;
        }));
    }

    async fn idle_expired(self: &Arc<Self>, kind: SlotKind, generation: u64) {
        let taken = {
            let mut g = self.slots.get(kind).inner.lock();
            match &g.phase {
                Phase::Ready(l) if l.generation == generation && g.busy == 0 => {
                    // 定时器就是自己：不能 abort 自己，只把句柄扔掉
                    g.timer = None;
                    match std::mem::replace(&mut g.phase, Phase::Stopped) {
                        Phase::Ready(l) => Some((l, StopTicket::issue(self, &mut g, kind))),
                        _ => None,
                    }
                }
                _ => None,
            }
        };
        if let Some((l, ticket)) = taken {
            log::info!("offline: {} slot idle; releasing", kind.name());
            self.stop_loaded(kind, l, ticket).await;
        }
    }

    /// 停掉这些槽位里空闲的（没有租约的）；有租约的不动。返回停掉的个数
    pub(super) async fn stop_idle(self: &Arc<Self>, kinds: &[SlotKind]) -> usize {
        let mut taken = Vec::new();
        for kind in kinds {
            let mut g = self.slots.get(*kind).inner.lock();
            if g.busy == 0 && matches!(g.phase, Phase::Ready(_)) {
                g.cancel_timer();
                if let Phase::Ready(l) = std::mem::replace(&mut g.phase, Phase::Stopped) {
                    let ticket = StopTicket::issue(self, &mut g, *kind);
                    taken.push((*kind, l, ticket));
                }
            }
        }
        let n = taken.len();
        for (kind, l, ticket) in taken {
            self.stop_loaded(kind, l, ticket).await;
        }
        n
    }

    /// 让槽位尽快停止：空闲的立即停，有请求的做完就停；正在启动的让它启动完、等着的请求做完再停——用户没有取消，
    /// 那些请求不能以「已取消」结束。`force`（删除模型或运行时、退出）：取消进行中的请求与启动，立即停
    pub(super) async fn retire(self: &Arc<Self>, kind: SlotKind, force: bool) {
        let taken = {
            let mut g = self.slots.get(kind).inner.lock();
            g.failure = None;
            match &g.phase {
                Phase::Stopped => None,
                Phase::Starting { cancel, attempt, .. } => {
                    if force {
                        cancel.cancel();
                        // 启动任务还在收尾（ORT 工作进程的启动不能中途打断）：记一笔停止流程，之后的启动等它走完
                        let attempt = *attempt;
                        g.abandoned.insert(attempt);
                        g.stopping += 1;
                        g.phase = Phase::Stopped;
                    } else {
                        g.stop_when_idle = true;
                    }
                    None
                }
                Phase::Ready(_) if g.busy > 0 && !force => {
                    g.stop_when_idle = true;
                    None
                }
                Phase::Ready(_) => {
                    g.cancel_timer();
                    g.busy = 0;
                    match std::mem::replace(&mut g.phase, Phase::Stopped) {
                        Phase::Ready(l) => Some((l, StopTicket::issue(self, &mut g, kind))),
                        _ => None,
                    }
                }
            }
        };
        if let Some((l, ticket)) = taken {
            self.stop_loaded(kind, l, ticket).await;
        } else {
            self.after_slot_change();
        }
    }

    /// 进程在请求中途退出：清掉这一代（之后的请求重新启动）
    pub(super) async fn drop_dead(self: &Arc<Self>, kind: SlotKind, generation: u64) {
        let taken = {
            let mut g = self.slots.get(kind).inner.lock();
            match &g.phase {
                Phase::Ready(l) if l.generation == generation => {
                    g.cancel_timer();
                    g.busy = 0;
                    match std::mem::replace(&mut g.phase, Phase::Stopped) {
                        Phase::Ready(l) => Some((l, StopTicket::issue(self, &mut g, kind))),
                        _ => None,
                    }
                }
                _ => None,
            }
        };
        if let Some((l, ticket)) = taken {
            self.stop_loaded(kind, l, ticket).await;
        }
    }

    /// 在后台停止（调用方不等）。`ticket` 是取下进程时在同一把锁里登记的停止流程
    pub(super) fn spawn_stop(self: &Arc<Self>, kind: SlotKind, loaded: Arc<Loaded>, ticket: StopTicket) {
        let svc = Arc::clone(self);
        super::spawn(async move { svc.stop_loaded(kind, loaded, ticket).await });
    }

    /// 停止一个已经从槽位里取下的进程：取消进行中的请求、等它们放手、关 stdin、等进程退出（最多 1 s）、必要时强制结束。
    /// 走完（或这个任务被丢弃）时注销 `ticket`，同一槽位等着启动的新进程随之开始
    pub(super) async fn stop_loaded(self: &Arc<Self>, kind: SlotKind, loaded: Arc<Loaded>, ticket: StopTicket) {
        let slot = self.slots.get(kind);
        loaded.cancel.cancel();
        slot.wait_unshared(&loaded, STOP_GRACE).await;
        let pid = loaded.pid;
        let pid_file = self.store.pid_file(kind.name());
        match Arc::try_unwrap(loaded) {
            Ok(l) => {
                let t0 = Instant::now();
                let _ = super::spawn_blocking(move || {
                    l.shutdown(STOP_GRACE);
                    remove_pid_file_of(&pid_file, pid);
                })
                .await;
                log::info!(
                    "offline: {} slot pid={pid} stopped in {} ms",
                    kind.name(),
                    t0.elapsed().as_millis()
                );
            }
            Err(shared) => {
                // 还有请求没放手（不会超过 1 s 的取消响应）：最后一个租约析构时，平台层的子进程句柄随之结束整棵进程树。
                // pid 文件留着：进程还活着，这期间 Wisp 崩溃的话下次启动的清扫还要靠它（之后同名文件由新进程覆盖，或由
                // 下次启动的清扫核对后删除）
                log::warn!(
                    "offline: {} slot pid={pid} is still in use; it ends with its last request",
                    kind.name()
                );
                drop(shared);
            }
        }
        drop(ticket);
        self.after_slot_change();
    }

    /// 槽位状态变了：按需安装 / 卸载内存压力监听，广播快照
    pub(super) fn after_slot_change(self: &Arc<Self>) {
        let active = self.slots.any_active();
        {
            let mut p = self.pressure.lock();
            if active && p.is_none() {
                let weak = Arc::downgrade(self);
                *p = wisp_platform::pressure::watch(move |level| {
                    if let Some(svc) = weak.upgrade() {
                        super::spawn(async move { svc.on_pressure(level).await });
                    }
                });
                if p.is_some() {
                    log::debug!("offline: memory pressure watch installed");
                }
            } else if !active && p.take().is_some() {
                log::debug!("offline: memory pressure watch removed");
            }
        }
        if active {
            self.ensure_settings_watch();
        }
        self.broadcast_now();
    }

    /// 内存压力：警告时停掉所有空闲的槽位；严重时另外让正在运行的请求做完就停
    async fn on_pressure(self: &Arc<Self>, level: wisp_platform::pressure::Level) {
        use wisp_platform::pressure::Level;
        if level == Level::Normal {
            return;
        }
        log::info!("offline: memory pressure {level:?}; releasing idle models");
        if level == Level::Critical {
            for slot in self.slots.all() {
                let mut g = slot.inner.lock();
                if g.busy > 0 {
                    g.stop_when_idle = true;
                }
            }
        }
        self.stop_idle(&SlotKind::ALL).await;
    }

    /// 只在第一次加载槽位时开始订阅设置（之后一直在，等待通知不占任何资源）
    fn ensure_settings_watch(self: &Arc<Self>) {
        if self.watching.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        let mut rx = self.settings.subscribe();
        let weak = Arc::downgrade(self);
        super::spawn(async move {
            let mut last = rx.borrow().offline;
            while rx.changed().await.is_ok() {
                let now = rx.borrow_and_update().offline;
                if now == last {
                    continue;
                }
                let Some(svc) = weak.upgrade() else { break };
                svc.on_offline_settings(&last, &now).await;
                last = now;
            }
        });
    }

    /// 设置变化：换了模型、关了 GPU 就停掉不再适用的槽位；闲置时长变了就重排定时器
    pub(super) async fn on_offline_settings(self: &Arc<Self>, old: &OfflineSettings, new: &OfflineSettings) {
        if old.use_gpu != new.use_gpu {
            self.retire(SlotKind::Vl, false).await;
            self.retire(SlotKind::Translate, false).await;
        }
        if old.translate_model != new.translate_model {
            let loaded = self.loaded_model(SlotKind::Translate);
            if loaded.is_some() && loaded != new.translate_model {
                self.retire(SlotKind::Translate, false).await;
            }
        }
        if old.ocr_model != new.ocr_model {
            let vl = |m: Option<LocalModelId>| m == Some(LocalModelId::PaddleOcrVl16);
            if !vl(new.ocr_model) {
                self.retire(SlotKind::Vl, false).await;
            }
            // 一个 ORT 工作进程服务全部离线 OCR（两个 PP-OCR 档位与 PaddleOCR-VL 的版面检测，会话按需懒建）：在两个档位之间
            // 切换不必重启。不再用离线 OCR、或不再用 PaddleOCR-VL 时才停，版面检测那份几百 MB 的会话随进程一起释放
            if new.ocr_model.is_none() || (vl(old.ocr_model) && !vl(new.ocr_model)) {
                self.retire(SlotKind::Ort, false).await;
            }
        }
        if old.idle_release_min != new.idle_release_min {
            for kind in SlotKind::ALL {
                let mut g = self.slots.get(kind).inner.lock();
                if g.busy == 0
                    && let Phase::Ready(l) = &g.phase
                {
                    let generation = l.generation;
                    self.schedule_idle(&mut g, kind, generation);
                }
            }
        }
    }

    /// 安装后预热：llama-server 第一次在显卡上运行时要在后端初始化里编译着色器（Metal / Vulkan，可能十几秒），之后由系统或
    /// 驱动缓存。刚下载完就在后台启动一次、就绪后立即停止，用户第一次用时就不必等这一段。只在用得上显卡、槽位空着、
    /// 不用停别的槽位就装得下时做；期间设置页显示「加载中」。失败只记日志（用户真正用到时会照常报告原因）
    pub(super) fn spawn_warm_up(self: &Arc<Self>, model: LocalModelId) {
        let kind = match model {
            LocalModelId::PaddleOcrVl16 => SlotKind::Vl,
            LocalModelId::IndexTranslate2b | LocalModelId::IndexTranslate9b => SlotKind::Translate,
            LocalModelId::PpOcrV6Tiny | LocalModelId::PpOcrV6Small => return,
        };
        let svc = Arc::clone(self);
        super::spawn(async move { svc.warm_up(kind, model).await });
    }

    /// 推理是否在显卡上进行：用户没关 GPU 加速，且本机是 Apple 芯片或检测到了显卡——**集成显卡也算**：llama.cpp 照常把层
    /// 卸载到 Vulkan 设备，核显上同样会遇到驱动问题。PaddleOCR-VL 的结果全空时据此提示「关闭 GPU 加速」（关掉后在 CPU 上
    /// 运行）；不在显卡上推理时全空就是「没有文字」
    pub(super) fn gpu_in_use(&self) -> bool {
        self.settings.get().offline.use_gpu && has_any_gpu(&self.device_info())
    }

    /// 安装后值得预热：推理在 Apple 芯片或独立显卡上。只有核显时不预热——适配评估把核显按 CPU 档算（同样较慢），在后台
    /// 加载一次（9B 要占几 GB 内存、几十秒核显）只为第一次少等着色器编译，得不偿失
    pub(super) fn warm_up_worthwhile(&self) -> bool {
        let device = self.device_info();
        self.settings.get().offline.use_gpu && (device.apple_silicon || device.gpus.iter().any(|g| !g.integrated))
    }

    async fn warm_up(self: &Arc<Self>, kind: SlotKind, model: LocalModelId) {
        if !self.warm_up_worthwhile()
            || self.is_shut_down()
            || !matches!(self.slots.get(kind).inner.lock().phase, Phase::Stopped)
        {
            return;
        }
        if !self.memory_fits(kind, model) {
            log::info!("offline: skipping the warm-up of {} (memory is tight)", model.as_str());
            return;
        }
        if !super::usable(&self.inventory().await, model) {
            return;
        }
        let t0 = Instant::now();
        match self
            .lease(kind, model, &CancellationToken::new(), || async {}, true)
            .await
        {
            Ok(lease) => {
                log::info!(
                    "offline: {} warmed up in {} ms",
                    model.as_str(),
                    t0.elapsed().as_millis()
                );
                self.release_warm(lease);
            }
            Err(e) => log::info!("offline: warming up {} failed ({:?})", model.as_str(), e.code),
        }
    }

    /// 槽位里加载着的模型
    fn loaded_model(&self, kind: SlotKind) -> Option<LocalModelId> {
        match &self.slots.get(kind).inner.lock().phase {
            Phase::Ready(l) => Some(l.model),
            Phase::Starting { model, .. } => Some(*model),
            Phase::Stopped => None,
        }
    }

    /// 用到这个模型的槽位
    pub(super) fn slots_using(&self, model: LocalModelId) -> Vec<SlotKind> {
        match model {
            LocalModelId::PpOcrV6Tiny | LocalModelId::PpOcrV6Small => vec![SlotKind::Ort],
            LocalModelId::PaddleOcrVl16 => vec![SlotKind::Vl, SlotKind::Ort],
            LocalModelId::IndexTranslate2b | LocalModelId::IndexTranslate9b => {
                if self.loaded_model(SlotKind::Translate) == Some(model) {
                    vec![SlotKind::Translate]
                } else {
                    Vec::new()
                }
            }
        }
    }

    /// 退出：取下全部槽位（取消正在进行的启动），在几个线程里并行停止（总计约 1 s），删 pid 文件。阻塞
    pub(super) fn shutdown_slots(&self) {
        let mut taken = Vec::new();
        for slot in self.slots.all() {
            let mut g = slot.inner.lock();
            g.cancel_timer();
            match std::mem::replace(&mut g.phase, Phase::Stopped) {
                Phase::Ready(l) => {
                    l.cancel.cancel();
                    taken.push((slot.kind, l));
                }
                Phase::Starting { cancel, .. } => cancel.cancel(),
                Phase::Stopped => {}
            }
            g.busy = 0;
        }
        *self.pressure.lock() = None;
        let handles: Vec<_> = taken
            .into_iter()
            .map(|(kind, l)| {
                std::thread::spawn(move || {
                    // 进行中的请求已被取消，给它们一点时间放手（只在退出的这一刻轮询）
                    let deadline = Instant::now() + Duration::from_millis(300);
                    while Arc::strong_count(&l) > 1 && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    match Arc::try_unwrap(l) {
                        Ok(l) => l.shutdown(STOP_GRACE),
                        // 仍在用：进程随 Wisp 一起结束（守护进程读到 EOF / Job Object 关闭）
                        Err(_) => log::warn!("offline: {} slot still in use at exit", kind.name()),
                    }
                })
            })
            .collect();
        for h in handles {
            let _ = h.join();
        }
        for kind in SlotKind::ALL {
            let _ = std::fs::remove_file(self.store.pid_file(kind.name()));
        }
    }
}

/// 本机是 Apple 芯片或检测到了显卡（含集成显卡）
fn has_any_gpu(device: &wisp_types::DeviceInfo) -> bool {
    device.apple_silicon || !device.gpus.is_empty()
}

/// 启动任务的收尾保证（析构时）：任务没有走到正常收尾就结束了（被丢弃、调试构建里 panic）时，把仍属于这次启动的相位置回
/// `Stopped`、记一次启动失败并告诉等着的请求——否则它们每一轮都看到同一个已关闭的通道，在 tokio 线程上空转到请求取消为止，
/// 槽位一直显示「加载中」。这次启动被强制停止时登记的停止流程也在这里注销
pub(super) struct StartGuard {
    pub(super) svc: Arc<ModelsService>,
    pub(super) kind: SlotKind,
    pub(super) model: LocalModelId,
    pub(super) attempt: u64,
    pub(super) tx: Option<watch::Sender<StartResult>>,
}

impl StartGuard {
    /// 还在等这次启动的请求数
    fn waiting(&self) -> usize {
        self.tx.as_ref().map_or(0, watch::Sender::receiver_count)
    }

    /// 正常收尾：把结果告诉等着的请求
    fn send(&mut self, result: Result<(), LlmError>) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Some((self.model, result)));
        }
    }
}

impl Drop for StartGuard {
    fn drop(&mut self) {
        let Some(tx) = self.tx.take() else { return };
        log::warn!(
            "offline: {} slot start for {} ended abnormally",
            self.kind.name(),
            self.model.as_str()
        );
        let err = RuntimeFailure::StartFailed.to_llm_error();
        let owed = {
            let mut g = self.svc.slots.get(self.kind).inner.lock();
            if matches!(&g.phase, Phase::Starting { attempt, .. } if *attempt == self.attempt) {
                g.phase = Phase::Stopped;
                let next = Failure::next(g.failure.as_ref(), self.model, err.clone(), Instant::now());
                g.failure = Some(next);
            }
            g.abandoned.remove(&self.attempt)
        };
        if owed {
            drop(StopTicket::adopt(&self.svc, self.kind));
        }
        let _ = tx.send(Some((self.model, Err(err))));
        self.svc.after_slot_change();
    }
}

/// 删除槽位的 pid 文件——只在它记的还是这个进程时。同一槽位可能已经起了新进程并写了同名文件（启动期间被停止的旧进程
/// 收尾得晚），删掉它会让新进程失去崩溃后清扫这一层兜底。读不懂时不删：下次启动的清扫会核对 pid、启动时间与路径再处理
fn remove_pid_file_of(path: &Path, pid: u32) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    if text.lines().next() == Some(format!("pid={pid}").as_str()) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * MB;

    fn probe(total_gb: u64, available_gb: u64, pressure: Option<Level>, compressing: bool) -> MemoryProbe {
        MemoryProbe {
            memory: Some(MemoryInfo {
                total: total_gb * GB,
                available: available_gb * GB,
            }),
            pressure,
            compressing,
        }
    }

    #[test]
    fn macos_compares_with_the_total_and_follows_the_pressure() {
        let nine_b = 6_200 * MB;
        // 16 GB 的 Mac 开着浏览器：「可用」只有 2 GB，照样可以加载 9B（系统会压缩别的页）
        let busy_mac = probe(16, 2, Some(Level::Normal), true);
        assert_eq!(judge(nine_b, 0, &busy_mac, false), Budget::Fits);
        // 8 GB：超过总量的 70%，停掉别的也装不下
        assert_eq!(judge(nine_b, 0, &probe(8, 6, None, true), false), Budget::Refuse);
        // 警告：单凭它不停别的槽位；加上已加载的其他模型超过 70% 时才先停空闲的，复查时照常加载
        let warn = probe(16, 2, Some(Level::Warning), true);
        assert_eq!(judge(nine_b, 0, &warn, false), Budget::Fits);
        assert_eq!(judge(nine_b, 300 * MB, &warn, false), Budget::Fits, "不为 9B 停掉 ORT");
        assert_eq!(judge(nine_b, 6 * GB, &warn, false), Budget::FreeOthers);
        assert_eq!(judge(nine_b, 6 * GB, &warn, true), Budget::Fits);
        // 严重：停过别的之后仍然严重才拒绝
        let critical = probe(16, 2, Some(Level::Critical), true);
        assert_eq!(judge(nine_b, 0, &critical, false), Budget::FreeOthers);
        assert_eq!(judge(nine_b, 0, &critical, true), Budget::Refuse);
        // 加上已加载的其他模型超过 70%：先停空闲的
        assert_eq!(judge(nine_b, 6 * GB, &busy_mac, false), Budget::FreeOthers);
        assert_eq!(judge(nine_b, 6 * GB, &busy_mac, true), Budget::Fits);
    }

    #[test]
    fn windows_and_linux_keep_the_available_measure_with_headroom() {
        let two_b = 1_700 * MB;
        assert_eq!(judge(two_b, 0, &probe(16, 8, None, false), false), Budget::Fits);
        // 可用 2 GB：1.7 GB 加上余量不够，先停别的；停过还不够就拒绝
        let tight = probe(16, 2, None, false);
        assert_eq!(judge(two_b, 0, &tight, false), Budget::FreeOthers);
        assert_eq!(judge(two_b, 0, &tight, true), Budget::Refuse);
        // 低内存通知：先停空闲的，再按可用内存判断
        let low = probe(16, 8, Some(Level::Warning), false);
        assert_eq!(judge(two_b, 0, &low, false), Budget::FreeOthers);
        assert_eq!(judge(two_b, 0, &low, true), Budget::Fits);
        // 读不到内存：不拦
        let unknown = MemoryProbe {
            memory: None,
            pressure: None,
            compressing: false,
        };
        assert_eq!(judge(two_b, 0, &unknown, true), Budget::Fits);
    }

    #[test]
    fn a_vl_request_never_frees_its_own_other_slot() {
        let vl = LocalModelId::PaddleOcrVl16;
        assert_eq!(ModelsService::other_slots(SlotKind::Ort, vl), vec![SlotKind::Translate]);
        assert_eq!(ModelsService::other_slots(SlotKind::Vl, vl), vec![SlotKind::Translate]);
        assert_eq!(
            ModelsService::other_slots(SlotKind::Translate, LocalModelId::IndexTranslate9b),
            vec![SlotKind::Ort, SlotKind::Vl]
        );
        assert_eq!(need_bytes(SlotKind::Ort, vl), LAYOUT_MEMORY_MB * MB, "ORT 只做版面检测");
    }

    #[test]
    fn the_pid_file_of_a_newer_process_is_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("translate.pid");
        std::fs::write(&path, "pid=200\nstart=1\nexe=/x\n").expect("write");
        remove_pid_file_of(&path, 100);
        assert!(path.exists(), "同名文件属于新进程：不删");
        remove_pid_file_of(&path, 200);
        assert!(!path.exists());
        // 读不懂的不删（留给下次启动的清扫）；不存在的不报错
        std::fs::write(&path, "garbage").expect("write");
        remove_pid_file_of(&path, 200);
        assert!(path.exists());
        remove_pid_file_of(&dir.path().join("missing.pid"), 1);
    }
}
