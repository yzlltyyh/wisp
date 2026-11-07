//! 实时同步（实验性）：用户开启后，在使用电脑期间每隔几秒做一次只读探测（`Engine::peek`，一个列目录请求、不写
//! sync.db），发现别的设备有新改动才跑完整的一轮，并把这次的列表交给它复用。
//!
//! 这是「不引入常驻定时器或轮询」的明确例外，只在用户打开「实时同步」时存在：
//! - **关闭时零成本**：没有任务、没有计时器、不保留 WebDAV 客户端，其余行为与没有这个功能时完全一样；
//! - **单个可取消的任务**：节奏全部由 `wisp_sync::realtime::plan` 决定（分档、服务商下限、请求额度、抖动、退避）。
//!   面板 / 悬浮窗显示、托盘与设置窗口、本机复制、「回到电脑前 / 解锁 / 唤醒」都只是 O(1) 地唤醒它重新规划；
//!   离开电脑时不联网，有系统事件可等的平台连本机计时器也不设；
//! - **安静**：没有变化的探测不发 `sync:status`（只悄悄更新 `checked_ms`，`sync_status` 能读到）；探测失败不碰自动同步
//!   的退避，连续 3 次才在状态里显示暂时性错误、恢复后清掉；只有服务器限流（429 / 503，遵守 `Retry-After`）和发现变化
//!   后那一轮的失败进入退避；
//! - **凭据**：只用内存里缓存的同步凭据（实时模式开启时，启动后在后台读一次钥匙串）；关闭、断开、凭据失效时清掉保留的
//!   WebDAV 客户端（里面有密码）；
//! - **日志**：每 30 分钟最多一条只有计数的汇总。
//!
//! # 给「写入系统剪贴板」的接口
//!
//! 每轮成功同步之后，实时同步开着且本轮从其他设备拿到了新剪贴板条目时，调用 [`SyncService::set_remote_clip_hook`]
//! 装上的回调，参数是 [`RemoteClipArrival`]（本轮的触发原因与条目的 hash、时间，按时间从新到旧）。回调在同步任务上
//! 同步执行，只能做 O(1) 的事（例如把它交给自己的工作线程）；它负责判断要不要写进系统剪贴板。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use tauri::Manager;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use wisp_platform::{PresenceEvent, PresenceWatch};
use wisp_sync::realtime::{self as plan, Inputs, Plan, Provider, QuotaWindow};
use wisp_sync::{Listing, Peek, RemoteClip, SyncError};
use wisp_types::{PanelView, SyncPhase, SyncRealtime, SyncSettings};

use super::gate::clock_ms;
use super::identity::now_ms;
use super::writer::describe;
use super::{CycleReason, SyncService, errors};
use crate::state::AppState;

/// 汇总日志的间隔
const SUMMARY_EVERY_MS: i64 = 30 * 60_000;
/// 连续这么多次探测失败才在状态里显示（睡眠唤醒后网络还没就绪很常见）
const SHOW_FAILURES_AFTER: u32 = 3;
/// 关闭时等任务退出的上限（进行中的探测会被立即取消，这只是兜底）
const STOP_WAIT: Duration = Duration::from_secs(5);

/// 本轮从其他设备拿到的新剪贴板条目（见模块说明）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteClipArrival {
    pub reason: CycleReason,
    /// 按条目时间从新到旧
    pub clips: Vec<RemoteClip>,
}

/// 「其他设备的新剪贴板条目」回调
pub type RemoteClipHook = Arc<dyn Fn(RemoteClipArrival) + Send + Sync>;

/// 运行中的任务
struct Running {
    generation: u64,
    token: CancellationToken,
    handle: tauri::async_runtime::JoinHandle<()>,
}

/// 只有计数的汇总
#[derive(Debug, Default)]
struct Stats {
    since_ms: i64,
    polls: u32,
    changed: u32,
    busy: u32,
    failures: u32,
}

/// 实时同步的状态（`SyncService.realtime`）。热路径上只有原子读写与一次 `Notify::notify_one`
pub(super) struct Realtime {
    task: Mutex<Option<Running>>,
    generation: AtomicU64,
    /// 有任务在跑（热路径据此决定要不要唤醒它、去抖用哪档）
    alive: AtomicBool,
    nudge: Arc<Notify>,
    /// [`clock_ms`]；0 = 没有
    last_activity_ms: AtomicI64,
    last_change_ms: AtomicI64,
    last_push_ms: AtomicI64,
    slow_until_ms: AtomicI64,
    /// 连续的探测失败
    failures: AtomicU32,
    /// 状态里正显示着探测失败（由实时同步放上去的，恢复时由它清掉）
    error_shown: AtomicBool,
    /// 服务器不提供可比较的签名：本次运行不再自动开始，直到用户重新打开实时同步或换了账号
    unsupported: AtomicBool,
    quota: Mutex<QuotaWindow>,
    /// 探测发现变化时的列表：只给紧接着的那一轮 `Poll` 用，被取走或那一轮换成了别的原因时清掉
    prefetched: Mutex<Option<Listing>>,
    stats: Mutex<Stats>,
    rng: AtomicU64,
    hook: RwLock<Option<RemoteClipHook>>,
}

impl Default for Realtime {
    fn default() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64)
            ^ (u64::from(std::process::id()) << 32);
        Realtime {
            task: Mutex::new(None),
            generation: AtomicU64::new(0),
            alive: AtomicBool::new(false),
            nudge: Arc::new(Notify::new()),
            last_activity_ms: AtomicI64::new(0),
            last_change_ms: AtomicI64::new(0),
            last_push_ms: AtomicI64::new(0),
            slow_until_ms: AtomicI64::new(0),
            failures: AtomicU32::new(0),
            error_shown: AtomicBool::new(false),
            unsupported: AtomicBool::new(false),
            quota: Mutex::new(QuotaWindow::default()),
            prefetched: Mutex::new(None),
            stats: Mutex::new(Stats::default()),
            rng: AtomicU64::new(seed | 1),
            hook: RwLock::new(None),
        }
    }
}

fn opt(ms: i64) -> Option<i64> {
    (ms > 0).then_some(ms)
}

impl Realtime {
    /// 有任务在跑
    pub(super) fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// 唤醒任务重新规划（O(1)；没有任务时什么也不做）
    pub(super) fn nudge(&self) {
        if self.alive() {
            self.nudge.notify_one();
        }
    }

    /// Wisp 看到了用户活动（面板、托盘、设置窗口、本机复制）
    pub(super) fn note_activity(&self, now: i64) {
        self.last_activity_ms.store(now, Ordering::Release);
    }

    /// 本轮同步成功：记下推送与看到的改动、请求数；网络显然是通的，探测的失败计数归零
    pub(super) fn note_cycle(&self, now: i64, pushed: bool, pulled: bool, requests: u32) {
        if pushed {
            self.last_push_ms.store(now, Ordering::Release);
        }
        if pulled {
            self.last_change_ms.store(now, Ordering::Release);
        }
        self.failures.store(0, Ordering::Release);
        self.error_shown.store(false, Ordering::Release);
        self.quota.lock().note(now, requests);
    }

    /// 取走探测拿到的列表：只有 `Poll` 那一轮用得上，其余原因的一轮自己列目录（列表照样清掉，不留给更晚的一轮）
    pub(super) fn take_listing(&self, reason: CycleReason) -> Option<Listing> {
        let listing = self.prefetched.lock().take();
        listing.filter(|_| reason == CycleReason::Poll)
    }

    /// [-1, 1] 之间的随机数（xorshift，只用来错开几台设备的请求，不需要密码学强度）
    fn jitter(&self) -> f64 {
        let mut x = self.rng.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng.store(x, Ordering::Relaxed);
        (x >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    }
}

/// 设置层面实时同步该不该运行：开着同步、自动同步与实时同步，并且同步剪贴板或识别历史（只同步设置时没有「实时」的意义）
pub(super) fn wanted_by_settings(s: &SyncSettings) -> bool {
    s.enabled && s.auto && s.realtime && (s.content.clipboard || s.content.ocr_history)
}

/// 规划结果 → 界面上的状态
pub(super) fn state_of(plan: &Plan) -> SyncRealtime {
    match plan {
        Plan::Poll { slowed: true, .. } | Plan::Wait { slowed: true, .. } => SyncRealtime::Slowed,
        Plan::Poll { .. } | Plan::Wait { .. } => SyncRealtime::Running,
        Plan::Park { .. } => SyncRealtime::Paused,
        Plan::Stop => SyncRealtime::Off,
    }
}

async fn sleep_or_forever(wait: Option<Duration>) {
    match wait {
        Some(d) => tokio::time::sleep(d).await,
        None => std::future::pending().await,
    }
}

impl SyncService {
    /// 装上（或去掉）「其他设备的新剪贴板条目」回调（见模块说明）
    pub fn set_remote_clip_hook(&self, hook: Option<RemoteClipHook>) {
        *self.realtime.hook.write() = hook;
    }

    /// 实时同步此刻该不该运行：设置允许、同步已开启、没有因需要处理的错误暂停、内存里有与设置一致的凭据、服务器支持
    pub(super) fn realtime_wanted(&self) -> bool {
        let settings = self.settings.get();
        wanted_by_settings(&settings.sync)
            && self.gate.enabled()
            && self.gate.auto()
            && !self.gate.paused()
            && !self.realtime.unsupported.load(Ordering::Acquire)
            && self
                .creds
                .lock()
                .as_ref()
                .is_some_and(|c| c.matches(&settings.sync.server_url, &settings.sync.username))
    }

    /// 该运行而没有在运行时启动任务
    pub(super) fn ensure_realtime(self: &Arc<Self>) {
        if !self.realtime_wanted() {
            return;
        }
        let mut task = self.realtime.task.lock();
        if task.is_some() {
            return;
        }
        let generation = self.realtime.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let token = CancellationToken::new();
        self.realtime.alive.store(true, Ordering::Release);
        let svc = Arc::clone(self);
        let child = token.clone();
        let handle = tauri::async_runtime::spawn(async move { svc.realtime_loop(child, generation).await });
        *task = Some(Running {
            generation,
            token,
            handle,
        });
    }

    /// 停下任务（不等它退出）。设置或条件变化时用
    pub(super) fn stop_realtime(&self) {
        if let Some(running) = self.realtime.task.lock().take() {
            running.token.cancel();
        }
        self.realtime.alive.store(false, Ordering::Release);
        self.realtime.prefetched.lock().take();
    }

    /// 停下任务并等它退出（关闭同步、断开之前：之后要关库）
    pub(super) async fn stop_realtime_and_wait(&self) {
        let running = self.realtime.task.lock().take();
        self.realtime.alive.store(false, Ordering::Release);
        self.realtime.prefetched.lock().take();
        if let Some(running) = running {
            running.token.cancel();
            if tokio::time::timeout(STOP_WAIT, running.handle).await.is_err() {
                log::warn!("sync: realtime task did not stop in time");
            }
        }
    }

    /// 实时同步的设置或前提条件变了：该运行就启动，不该运行就停下
    pub(super) fn follow_realtime(self: &Arc<Self>) {
        if self.realtime_wanted() {
            self.ensure_realtime();
            self.realtime.nudge();
            return;
        }
        if self.realtime.alive() {
            self.stop_realtime();
        }
        // 设置里关掉了（或条件不满足）：「服务器不支持」之类的旧状态也一并清掉
        if !wanted_by_settings(&self.settings.get().sync) {
            self.set_realtime_state(SyncRealtime::Off);
        }
    }

    /// 用户重新打开了实时同步、或者重新连接了：之前判定的「服务器不支持」作废，再试一次
    pub(super) fn realtime_reenabled(&self) {
        self.realtime.unsupported.store(false, Ordering::Release);
        self.realtime.failures.store(0, Ordering::Release);
    }

    /// 启动完成之后（托盘就绪、面板预热之后）：实时同步开着、内存里还没有凭据时在后台读一次钥匙串，开机后不必先
    /// 操作 Wisp 就能开始。只读同步凭据这一项；没开实时同步时什么也不做
    pub fn after_startup(self: &Arc<Self>) {
        let sync = self.settings.get().sync.clone();
        if !wanted_by_settings(&sync) || self.creds.lock().is_some() {
            return;
        }
        let svc = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            if !svc.gate.enabled() {
                return;
            }
            let settings = Arc::clone(&svc.settings);
            match super::blocking(super::KEYCHAIN_TIMEOUT, move || settings.sync_credentials().get()).await {
                Some(Ok(Some(c))) if c.matches(&sync.server_url, &sync.username) => {
                    let mut cached = svc.creds.lock();
                    if cached.is_none() {
                        *cached = Some(c);
                    }
                }
                Some(Ok(_)) => {
                    log::info!("sync: realtime waits for the account to be reconnected");
                    return;
                }
                Some(Err(e)) => {
                    log::info!(
                        "sync: realtime waits for the first use; the keychain refused ({:?})",
                        e.code
                    );
                    return;
                }
                None => return,
            }
            svc.ensure_realtime();
        });
    }

    /// 剪贴板面板或悬浮剪贴板正显示着
    fn hot_view(&self) -> bool {
        let Some(state) = self.app.as_ref().and_then(|a| a.try_state::<AppState>()) else {
            return false;
        };
        state
            .windows
            .panel_state()
            .is_some_and(|p| p.view == PanelView::Clipboard)
            || state.windows.float_ready()
    }

    fn platform(&self) -> Option<Arc<wisp_platform::Platform>> {
        Some(Arc::clone(&self.app.as_ref()?.try_state::<AppState>()?.platform))
    }

    /// 订阅「回到电脑前 / 解锁 / 唤醒」：回调只唤醒任务
    fn presence_watch(&self) -> Option<Box<dyn PresenceWatch>> {
        let nudge = Arc::clone(&self.realtime.nudge);
        self.platform()?.presence.watch(Arc::new(move |e: PresenceEvent| {
            let _ = e;
            nudge.notify_one();
        }))
    }

    fn provider(&self) -> Provider {
        let url = self.settings.get().sync.server_url.clone();
        wisp_sync::validate_server_url(&url)
            .ok()
            .and_then(|u| u.host_str().map(Provider::from_host))
            .unwrap_or(Provider::Other)
    }

    fn realtime_inputs(&self, watch: Option<&dyn PresenceWatch>, last_poll: Option<i64>, jitter: f64) -> Inputs {
        let rt = &self.realtime;
        let now = clock_ms();
        let platform = self.platform();
        let (input_idle, power) = match &platform {
            Some(p) => (p.presence.input_idle(), p.presence.power()),
            None => (None, wisp_platform::PowerState::default()),
        };
        let devices = self.status.borrow().device_count;
        let provider = self.provider();
        let quota = rt.quota.lock();
        Inputs {
            wanted: self.realtime_wanted(),
            now_ms: now,
            last_poll_ms: last_poll,
            input_idle_ms: input_idle.map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
            screen_idle: watch.is_some_and(PresenceWatch::screen_idle),
            last_activity_ms: opt(rt.last_activity_ms.load(Ordering::Acquire)),
            hot_view: self.hot_view(),
            last_change_ms: opt(rt.last_change_ms.load(Ordering::Acquire)),
            last_push_ms: opt(rt.last_push_ms.load(Ordering::Acquire)),
            on_battery: power.on_battery == Some(true),
            low_power: power.low_power == Some(true),
            // 按流量计费的网络各平台都没有便宜的查询方式，暂不区分
            metered: false,
            floor_ms: provider.floor_ms(devices),
            quota_used: quota.used(now),
            quota_limit: provider.budget(devices),
            quota_frees_in_ms: quota.frees_in_ms(now),
            failures: rt.failures.load(Ordering::Acquire),
            retry_after_ms: self.gate.retry_after(),
            slow_until_ms: opt(rt.slow_until_ms.load(Ordering::Acquire)),
            return_events: watch.is_some_and(PresenceWatch::reports_return),
            jitter,
        }
    }

    fn set_realtime_state(&self, state: SyncRealtime) {
        self.set_status(|s| {
            if s.phase != SyncPhase::Off {
                s.realtime = state;
            }
        });
    }

    /// 由规划驱动的任务主体
    async fn realtime_loop(self: Arc<Self>, token: CancellationToken, generation: u64) {
        log::debug!("sync: realtime started");
        let watch = self.presence_watch();
        let mut last_poll: Option<i64> = None;
        let mut jitter = self.realtime.jitter();
        let mut last_state = None;
        self.realtime.stats.lock().since_ms = clock_ms();
        loop {
            if token.is_cancelled() {
                break;
            }
            let inputs = self.realtime_inputs(watch.as_deref(), last_poll, jitter);
            let decision = plan::plan(&inputs);
            let state = state_of(&decision);
            if last_state != Some(state) {
                last_state = Some(state);
                log::debug!("sync: realtime {state:?}");
                self.set_realtime_state(state);
            }
            let wait = match decision {
                Plan::Stop => break,
                Plan::Park { recheck_ms, .. } => recheck_ms.map(Duration::from_millis),
                Plan::Wait { ms, .. } => Some(Duration::from_millis(ms)),
                Plan::Poll { .. } => {
                    last_poll = Some(clock_ms());
                    jitter = self.realtime.jitter();
                    if !self.poll_once(&token).await {
                        break;
                    }
                    self.maybe_log_summary(false);
                    continue;
                }
            };
            tokio::select! {
                () = token.cancelled() => break,
                () = self.realtime.nudge.notified() => {}
                () = sleep_or_forever(wait) => {}
            }
        }
        drop(watch);
        self.maybe_log_summary(true);
        self.realtime_exited(generation);
    }

    /// 任务退出：还是当前这一个时清掉登记、状态回到 off（服务器不支持时保留那个状态），队列也空着就释放客户端
    fn realtime_exited(&self, generation: u64) {
        {
            let mut task = self.realtime.task.lock();
            if task.as_ref().is_some_and(|t| t.generation == generation) {
                *task = None;
                self.realtime.alive.store(false, Ordering::Release);
            }
        }
        if self.realtime.unsupported.load(Ordering::Acquire) {
            self.set_realtime_state(SyncRealtime::Unsupported);
        } else if !self.realtime.alive() {
            self.set_realtime_state(SyncRealtime::Off);
        }
        if !self.realtime.alive() {
            self.realtime.prefetched.lock().take();
            if !self.running.load(Ordering::Acquire) {
                self.release_remote();
            } else if let Some(slot) = self.remote.lock().as_ref() {
                slot.remote.set_keep_alive(false);
            }
        }
        log::debug!("sync: realtime stopped");
    }

    /// 探测一次。返回 false 表示任务应当结束（服务器不支持、需要用户处理的错误、凭据不可用、同步正在关闭）
    async fn poll_once(self: &Arc<Self>, token: &CancellationToken) -> bool {
        let Some(active) = self.active() else { return false };
        if self.running.load(Ordering::Acquire) {
            // 一轮正在进行：它本来就会看到变化，不与它争用连接
            self.realtime.stats.lock().busy += 1;
            return true;
        }
        let credential = match self.credentials(CycleReason::Poll, &active).await {
            Ok(Some(c)) => c,
            Ok(None) => return false,
            Err(e) => {
                log::info!("sync: realtime stopped ({})", describe(&e));
                self.fail(&e);
                return false;
            }
        };
        let remote = match self.shared_remote(&credential) {
            Ok(r) => r,
            Err(e) => {
                self.fail(&e);
                return false;
            }
        };
        drop(credential);
        let now = clock_ms();
        self.realtime.quota.lock().note(now, 1);
        self.realtime.stats.lock().polls += 1;
        let result = tokio::select! {
            r = active.engine.peek(&*remote) => r,
            () = token.cancelled() => return false,
        };
        match result {
            Ok(Peek::Unchanged) => self.probe_ok(),
            Ok(Peek::Changed(listing)) => {
                self.probe_ok();
                self.realtime.stats.lock().changed += 1;
                *self.realtime.prefetched.lock() = Some(listing);
                self.trigger(CycleReason::Poll);
            }
            Ok(Peek::Busy) => self.realtime.stats.lock().busy += 1,
            Ok(Peek::Unsupported) => {
                log::info!("sync: realtime stopped: the server does not report file versions");
                self.realtime.unsupported.store(true, Ordering::Release);
                return false;
            }
            Err(e) => return self.probe_failed(&e),
        }
        true
    }

    /// 探测成功：失败计数归零，清掉实时同步自己显示的错误；悄悄记下检查时间（不推送事件）
    fn probe_ok(&self) {
        self.realtime.failures.store(0, Ordering::Release);
        if self.realtime.error_shown.swap(false, Ordering::AcqRel) {
            self.set_status(|s| {
                if s.phase == SyncPhase::Error && s.transient {
                    s.phase = SyncPhase::Idle;
                    s.error = None;
                    s.transient = false;
                    s.error_detail = None;
                }
            });
        }
        let now = now_ms();
        self.status.send_if_modified(|s| {
            s.checked_ms = Some(now);
            false
        });
    }

    /// 探测失败：限流进退避并放慢；需要用户处理的错误照常暂停自动同步（任务随之结束）；其余只计数，不碰退避
    fn probe_failed(&self, e: &SyncError) -> bool {
        self.realtime.stats.lock().failures += 1;
        if e.is_throttle() {
            let until = clock_ms().saturating_add(i64::try_from(plan::SLOW_AFTER_THROTTLE_MS).unwrap_or(i64::MAX));
            self.realtime.slow_until_ms.store(until, Ordering::Release);
            log::info!("sync: realtime slowed down ({})", describe(e));
            self.fail(e);
            return true;
        }
        if !e.is_transient() {
            log::info!("sync: realtime stopped ({})", describe(e));
            self.fail(e);
            return false;
        }
        let n = self.realtime.failures.fetch_add(1, Ordering::AcqRel).saturating_add(1);
        if n >= SHOW_FAILURES_AFTER && !self.realtime.error_shown.swap(true, Ordering::AcqRel) {
            log::info!("sync: realtime check failing ({})", describe(e));
            let kind = errors::kind_of(e.kind());
            let detail = e.detail();
            self.set_status(|s| {
                s.phase = SyncPhase::Error;
                s.error = Some(kind);
                s.transient = true;
                s.error_detail = detail;
                s.progress = None;
            });
        }
        true
    }

    /// 每 30 分钟最多一条只有计数的汇总；任务结束时补一条（只在有过探测时）
    fn maybe_log_summary(&self, last: bool) {
        let now = clock_ms();
        let mut stats = self.realtime.stats.lock();
        if stats.polls == 0 || (!last && now.saturating_sub(stats.since_ms) < SUMMARY_EVERY_MS) {
            return;
        }
        let used = self.realtime.quota.lock().used(now);
        log::info!(
            "sync: realtime polls={} changed={} busy={} failures={} requests_30min={} minutes={}",
            stats.polls,
            stats.changed,
            stats.busy,
            stats.failures,
            used,
            now.saturating_sub(stats.since_ms) / 60_000
        );
        *stats = Stats {
            since_ms: now,
            ..Stats::default()
        };
    }

    /// 本轮成功之后交出其他设备的新剪贴板条目（实时同步开着、并且装了回调时）
    pub(super) fn hand_off_remote_clips(&self, reason: CycleReason, clips: &[RemoteClip]) {
        if clips.is_empty() || !self.settings.get().sync.realtime {
            return;
        }
        let hook = self.realtime.hook.read().clone();
        if let Some(hook) = hook {
            hook(RemoteClipArrival {
                reason,
                clips: clips.to_vec(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_gate_realtime() {
        let mut s = SyncSettings {
            enabled: true,
            realtime: true,
            ..SyncSettings::default()
        };
        assert!(wanted_by_settings(&s));
        s.auto = false;
        assert!(!wanted_by_settings(&s), "需要自动同步");
        s.auto = true;
        s.content.clipboard = false;
        assert!(wanted_by_settings(&s), "只同步识别历史也可以");
        s.content.ocr_history = false;
        assert!(!wanted_by_settings(&s), "只同步设置时没有实时的意义");
        s.content.clipboard = true;
        s.realtime = false;
        assert!(!wanted_by_settings(&s));
        s.realtime = true;
        s.enabled = false;
        assert!(!wanted_by_settings(&s));
    }

    #[test]
    fn plans_map_to_ui_states() {
        use plan::{ParkReason, Tier};
        assert_eq!(
            state_of(&Plan::Poll {
                tier: Tier::Hot,
                slowed: false
            }),
            SyncRealtime::Running
        );
        assert_eq!(
            state_of(&Plan::Wait {
                ms: 1,
                tier: Tier::Quiet,
                slowed: true
            }),
            SyncRealtime::Slowed
        );
        assert_eq!(
            state_of(&Plan::Park {
                recheck_ms: None,
                reason: ParkReason::LowPower
            }),
            SyncRealtime::Paused
        );
        assert_eq!(state_of(&Plan::Stop), SyncRealtime::Off);
    }

    #[test]
    fn jitter_stays_in_range_and_varies() {
        let rt = Realtime::default();
        let samples: Vec<f64> = (0..1000).map(|_| rt.jitter()).collect();
        assert!(samples.iter().all(|j| (-1.0..=1.0).contains(j)));
        assert!(samples.iter().any(|j| *j < -0.5) && samples.iter().any(|j| *j > 0.5));
    }

    #[test]
    fn a_prefetched_listing_only_serves_the_poll_cycle() {
        let rt = Realtime::default();
        assert!(rt.take_listing(CycleReason::Poll).is_none());
    }
}
