//! 实时同步（实验性）：用户开启后，在使用电脑期间每隔几秒做一次只读探测（`Engine::peek`，一个列目录请求、不写
//! sync.db），发现别的设备有新改动才跑完整的一轮，并把这次的列表交给它复用。
//!
//! 这是「不引入常驻定时器或轮询」的明确例外，只在用户打开「实时同步」时存在：
//! - **关闭时零成本**：没有任务、没有计时器、不保留 WebDAV 客户端，其余行为与没有这个功能时完全一样；
//! - **单个可取消的任务**：节奏全部由 `wisp_sync::realtime::plan` 决定（分档、服务商下限、请求额度、抖动、退避）。
//!   面板 / 悬浮窗显示、与悬浮窗的交互、托盘与设置窗口、本机复制、「回到电脑前 / 解锁 / 唤醒」都只是 O(1) 地唤醒它
//!   重新规划；离开电脑时不联网，有系统事件可等的平台连本机计时器也不设；
//! - **热档只给正在看的剪贴板**：剪贴板面板显示着，或者悬浮剪贴板显示着、并且 [`FLOAT_HOT_MS`] 内与它交互过（打开、
//!   获得焦点、指针移入、搜索框聚焦、从中粘贴）。悬浮窗可以开一整天，只是开着不算：过了这段时间按输入空闲落回活跃 /
//!   安静档，由规划器下一次计算时自然降档，不另设计时器；
//! - **安静**：没有变化的探测不发 `sync:status`（只悄悄更新 `checked_ms`，`sync_status` 能读到）；探测失败不碰自动同步
//!   的退避，连续 3 次才在状态里显示暂时性错误、恢复后清掉；只有服务器限流（429 / 503，遵守 `Retry-After`）和发现变化
//!   后那一轮的失败进入退避。退避期间本机的改动不安排去抖上传，退避结束后的第一次探测补上（不然要等下一次用户活动）；
//! - **服务器不支持时退回自动同步**：列目录不给签名，或者签名不稳定（连续几次「有变化」却什么都没拿到）时停下，
//!   直到用户重新打开实时同步或重新连接；
//! - **凭据**：只用内存里缓存的同步凭据（实时模式开启时，启动后在后台读一次钥匙串）；关闭、断开、凭据失效时清掉保留的
//!   WebDAV 客户端（里面有密码）；
//! - **日志**：每 30 分钟最多一条只有计数的汇总。
//!
//! # 给「写入系统剪贴板」的接口
//!
//! 每轮成功同步之后，实时同步**正在运行**（设置允许、任务在跑：关掉自动同步、服务器不支持、因需要处理的错误暂停时都
//! 不算）且本轮从其他设备拿到了新剪贴板条目时，调用 [`SyncService::set_remote_clip_hook`] 装上的回调，参数是
//! [`RemoteClipArrival`]（本轮的触发原因与条目的 hash、时间，按时间从新到旧）。任何原因的轮次都算：实时同步运行期间，
//! 打开面板那一轮先拿到的复制，探测就不会再看到变化。回调在同步任务上同步执行，只能做 O(1) 的事（例如把它交给自己的
//! 工作线程）；它负责判断要不要写进系统剪贴板。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use tauri::Manager;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use wisp_platform::{PowerState, PresenceEvent, PresenceWatch};
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
/// 电源状态在任务里缓存这么久（插拔电源、省电模式的通知与唤醒会让它立即作废）：每次规划都读一遍，在 Linux 上是
/// 读一整个 sysfs 目录
const POWER_TTL_MS: i64 = 60_000;
/// 与悬浮剪贴板交互之后，这么久之内把它当作正在看的剪贴板（热档）。只是开着的悬浮窗不算：它可以开一整天，一直按
/// 热档检查的开销（CPU 与流量）是活跃档的两倍多
const FLOAT_HOT_MS: i64 = 60_000;

/// 本轮从其他设备拿到的新剪贴板条目（见模块说明）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteClipArrival {
    pub reason: CycleReason,
    /// 按条目时间从新到旧
    pub clips: Vec<RemoteClip>,
}

/// 「其他设备的新剪贴板条目」回调
pub type RemoteClipHook = Arc<dyn Fn(RemoteClipArrival) + Send + Sync>;

type Handle = tauri::async_runtime::JoinHandle<()>;

/// 运行中的任务
struct Running {
    generation: u64,
    token: CancellationToken,
    handle: Handle,
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
    /// 已经取消、还没确认退出的任务（取消时可能正在本地解锁，手里还有副本的引用）：关闭同步时一并等它们，之后才关库
    stopping: Mutex<Vec<Handle>>,
    generation: AtomicU64,
    /// 有任务在跑（热路径据此决定要不要唤醒它、去抖用哪档）
    alive: AtomicBool,
    nudge: Arc<Notify>,
    /// [`clock_ms`]；0 = 没有
    last_activity_ms: AtomicI64,
    /// 最近一次与悬浮剪贴板交互（[`SyncService::note_float_use`]）
    last_float_ms: AtomicI64,
    last_change_ms: AtomicI64,
    last_push_ms: AtomicI64,
    slow_until_ms: AtomicI64,
    /// 连续的探测失败
    failures: AtomicU32,
    /// 状态里正显示着探测失败（由实时同步放上去的，恢复时由它清掉）
    error_shown: AtomicBool,
    /// 服务器不提供可比较的签名：本次运行不再自动开始，直到用户重新打开实时同步或换了账号
    unsupported: AtomicBool,
    /// 连续几次探测说有变化、那一轮却什么都没拿到也没推送（签名不稳定的服务器）
    noop_changes: AtomicU32,
    /// 自动同步在退避时本机有了改动（去抖没有安排）：退避结束后的第一次探测替它发起上传
    upload_deferred: AtomicBool,
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
            stopping: Mutex::new(Vec::new()),
            generation: AtomicU64::new(0),
            alive: AtomicBool::new(false),
            nudge: Arc::new(Notify::new()),
            last_activity_ms: AtomicI64::new(0),
            last_float_ms: AtomicI64::new(0),
            last_change_ms: AtomicI64::new(0),
            last_push_ms: AtomicI64::new(0),
            slow_until_ms: AtomicI64::new(0),
            failures: AtomicU32::new(0),
            error_shown: AtomicBool::new(false),
            unsupported: AtomicBool::new(false),
            noop_changes: AtomicU32::new(0),
            upload_deferred: AtomicBool::new(false),
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

    /// 本轮同步成功：记下推送与看到的改动、请求数；网络显然是通的，探测的失败计数归零。结束时没有待上传的改动，
    /// 退避期间攒下的上传也就不用补了；还有（例如「立即同步」那一轮进行中又复制了一条，那时仍在退避）就留着标记
    pub(super) fn note_cycle(&self, now: i64, pushed: bool, pulled: bool, requests: u32, pending: bool) {
        if pushed {
            self.last_push_ms.store(now, Ordering::Release);
        }
        if pulled {
            self.last_change_ms.store(now, Ordering::Release);
        }
        self.failures.store(0, Ordering::Release);
        self.error_shown.store(false, Ordering::Release);
        if !pending {
            self.upload_deferred.store(false, Ordering::Release);
        }
        self.quota.lock().note(now, requests);
    }

    /// 探测发现变化后的那一轮（`Poll`）结束：什么都没拿到也没推送就记一次，连续 [`plan::NOOP_CHANGES_LIMIT`] 次返回
    /// true（这个服务器的签名不稳定）。其余原因的轮次不算，也不打断计数
    fn note_poll_cycle(&self, reason: CycleReason, idle: bool) -> bool {
        if reason != CycleReason::Poll {
            return false;
        }
        if !idle {
            self.noop_changes.store(0, Ordering::Release);
            return false;
        }
        self.noop_changes.fetch_add(1, Ordering::AcqRel).saturating_add(1) >= plan::NOOP_CHANGES_LIMIT
    }

    /// 取走探测拿到的列表：只有 `Poll` 那一轮用得上，其余原因的一轮自己列目录（列表照样清掉，不留给更晚的一轮）
    pub(super) fn take_listing(&self, reason: CycleReason) -> Option<Listing> {
        take_for_poll(&self.prefetched, reason)
    }

    /// 自动同步在退避，本机改动没有安排上传
    pub(super) fn defer_upload(&self) {
        self.upload_deferred.store(true, Ordering::Release);
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

/// 设置层面实时同步该不该运行：开着同步、自动同步与实时同步，并且同步剪贴板或识别历史（只同步设置时没有「实时」的意义）。
/// 设置页的禁用条件与它一致；「放进本机剪贴板」的子选项也以它为前提（`clipboard_svc::remote_copy`）
pub(crate) fn wanted_by_settings(s: &SyncSettings) -> bool {
    s.enabled && s.auto && s.realtime && (s.content.clipboard || s.content.ocr_history)
}

/// 「正在看剪贴板」（规划器的热档条件之一）：剪贴板面板显示着；或者悬浮剪贴板显示着、并且 [`FLOAT_HOT_MS`] 内
/// 与它交互过。`last_float_ms` 与 `now` 都是 [`clock_ms`]
fn hot_view_of(clipboard_panel: bool, float_shown: bool, last_float_ms: Option<i64>, now: i64) -> bool {
    clipboard_panel || (float_shown && last_float_ms.is_some_and(|at| now.saturating_sub(at) < FLOAT_HOT_MS))
}

/// 取走槽里的值，只在 `Poll` 那一轮交出（[`Realtime::take_listing`]，泛型是为了单测不必构造真的列表）
fn take_for_poll<T>(slot: &Mutex<Option<T>>, reason: CycleReason) -> Option<T> {
    let value = slot.lock().take();
    value.filter(|_| reason == CycleReason::Poll)
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

/// 任务里的在场信息：系统事件的订阅（丢弃即注销），以及缓存的电源状态
struct PresenceState {
    watch: Option<Box<dyn PresenceWatch>>,
    /// 电源状态与读到它的时间（[`clock_ms`]）
    power: Option<(i64, PowerState)>,
    /// 插拔电源、省电模式开关、唤醒的通知来过：缓存作废
    power_stale: Arc<AtomicBool>,
}

impl PresenceState {
    fn watch(&self) -> Option<&dyn PresenceWatch> {
        self.watch.as_deref()
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

    /// 停下任务（不等它退出）。设置或条件变化时用。句柄留着：关闭同步时要等它真的退出
    pub(super) fn stop_realtime(&self) {
        if let Some(running) = self.realtime.task.lock().take() {
            running.token.cancel();
            let mut stopping = self.realtime.stopping.lock();
            stopping.retain(|h| !h.inner().is_finished());
            stopping.push(running.handle);
        }
        self.realtime.alive.store(false, Ordering::Release);
        self.realtime.prefetched.lock().take();
    }

    /// 停下任务并等它（以及之前停下、还没退出的任务）退出：关闭同步、断开之前，之后要关库
    pub(super) async fn stop_realtime_and_wait(&self) {
        let running = self.realtime.task.lock().take();
        self.realtime.alive.store(false, Ordering::Release);
        self.realtime.prefetched.lock().take();
        let mut handles = std::mem::take(&mut *self.realtime.stopping.lock());
        if let Some(running) = running {
            running.token.cancel();
            handles.push(running.handle);
        }
        if handles.is_empty() {
            return;
        }
        let all = async {
            for handle in handles {
                let _ = handle.await;
            }
        };
        if tokio::time::timeout(STOP_WAIT, all).await.is_err() {
            log::warn!("sync: realtime task did not stop in time");
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
        self.realtime.noop_changes.store(0, Ordering::Release);
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
                    // 读钥匙串可能要等很久（授权框）：期间用户可能断开了同步（凭据已从钥匙串删掉）或换了账号。在
                    // `lifecycle` 锁里按此刻的状态再核对一次再缓存；锁被占着（正在连接、断开）就放弃，等第一次用户活动
                    let Ok(_life) = svc.lifecycle.try_lock() else { return };
                    let now = svc.settings.get().sync.clone();
                    if !svc.gate.enabled() || !wanted_by_settings(&now) || !c.matches(&now.server_url, &now.username) {
                        return;
                    }
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

    /// 用户与悬浮剪贴板有了交互（打开、获得焦点、指针移入、搜索框聚焦、从中粘贴）：接下来的 [`FLOAT_HOT_MS`] 里它算作
    /// 正在看的剪贴板，实时同步按热档检查。O(1)：记一个时间戳、唤醒任务重新规划（没有任务时只是一次原子写）
    pub fn note_float_use(&self) {
        self.realtime.last_float_ms.store(clock_ms(), Ordering::Release);
        self.realtime.nudge();
    }

    /// 正在看剪贴板（见 [`hot_view_of`]）
    fn hot_view(&self, now: i64) -> bool {
        let Some(state) = self.app.as_ref().and_then(|a| a.try_state::<AppState>()) else {
            return false;
        };
        let panel = state
            .windows
            .panel_state()
            .is_some_and(|p| p.view == PanelView::Clipboard);
        hot_view_of(
            panel,
            state.windows.float_ready(),
            opt(self.realtime.last_float_ms.load(Ordering::Acquire)),
            now,
        )
    }

    fn platform(&self) -> Option<Arc<wisp_platform::Platform>> {
        Some(Arc::clone(&self.app.as_ref()?.try_state::<AppState>()?.platform))
    }

    /// 订阅「回到电脑前 / 解锁 / 唤醒」：回调只唤醒任务（电源变化与唤醒另外让缓存的电源状态作废）。订阅在有的平台上
    /// 要等一个新线程建好窗口，放到阻塞线程池里做；等待期间取消就放弃（订阅随后台任务的结果一起丢弃、注销）
    async fn presence(&self, token: &CancellationToken) -> Option<PresenceState> {
        let power_stale = Arc::new(AtomicBool::new(true));
        let Some(platform) = self.platform() else {
            return Some(PresenceState {
                watch: None,
                power: None,
                power_stale,
            });
        };
        let nudge = Arc::clone(&self.realtime.nudge);
        let stale = Arc::clone(&power_stale);
        let cb: wisp_platform::PresenceCallback = Arc::new(move |e: PresenceEvent| {
            if matches!(e, PresenceEvent::PowerChanged | PresenceEvent::Woke) {
                stale.store(true, Ordering::Release);
            }
            nudge.notify_one();
        });
        let subscribe = tauri::async_runtime::spawn_blocking(move || platform.presence.watch(cb));
        let watch = tokio::select! {
            () = token.cancelled() => return None,
            w = subscribe => w.ok().flatten(),
        };
        Some(PresenceState {
            watch,
            power: None,
            power_stale,
        })
    }

    /// 电源状态：缓存 [`POWER_TTL_MS`]，期间有电源变化或唤醒的通知就重读
    fn power(&self, presence: &mut PresenceState, now: i64) -> PowerState {
        let stale = presence.power_stale.swap(false, Ordering::AcqRel);
        if let Some((at, power)) = presence.power
            && !stale
            && now.saturating_sub(at) < POWER_TTL_MS
        {
            return power;
        }
        let power = self.platform().map(|p| p.presence.power()).unwrap_or_default();
        presence.power = Some((now, power));
        power
    }

    fn provider(&self) -> Provider {
        let url = self.settings.get().sync.server_url.clone();
        wisp_sync::validate_server_url(&url)
            .ok()
            .and_then(|u| u.host_str().map(Provider::from_host))
            .unwrap_or(Provider::Other)
    }

    fn realtime_inputs(&self, presence: &mut PresenceState, last_poll: Option<i64>, jitter: f64) -> Inputs {
        let rt = &self.realtime;
        let now = clock_ms();
        // 先查输入空闲再读订阅的锁屏状态：X11 的屏幕保护状态随输入空闲的那次查询更新
        let input_idle = self.platform().and_then(|p| p.presence.input_idle());
        let power = self.power(presence, now);
        let watch = presence.watch();
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
            hot_view: self.hot_view(now),
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
            unlock_events: watch.is_some_and(PresenceWatch::reports_unlock),
            power_events: watch.is_some_and(PresenceWatch::reports_power),
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
        let Some(mut presence) = self.presence(&token).await else {
            self.realtime_exited(generation);
            return;
        };
        let mut last_poll: Option<i64> = None;
        let mut jitter = self.realtime.jitter();
        let mut last_state = None;
        self.realtime.stats.lock().since_ms = clock_ms();
        loop {
            if token.is_cancelled() {
                break;
            }
            let inputs = self.realtime_inputs(&mut presence, last_poll, jitter);
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
        drop(presence);
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
            self.release_remote_if_idle();
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
        // 不与取消竞争：这里唯一的等待是副本还锁着时在阻塞线程里解锁，半路放下的话那个线程仍拿着副本，关闭同步就关不了
        // 库。取消之后任务在这一步结束时退出（不再按结果处理凭据），关闭同步会等它（`stop_realtime_and_wait`）
        let credential = self.credentials(CycleReason::Poll, &active).await;
        if token.is_cancelled() {
            return false;
        }
        let credential = match credential {
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
        drop(active);
        if token.is_cancelled() {
            return false;
        }
        self.after_probe(result)
    }

    /// 处理一次探测的结果。返回 false 表示任务应当结束
    pub(super) fn after_probe(self: &Arc<Self>, result: Result<Peek, SyncError>) -> bool {
        match result {
            Ok(Peek::Unchanged) => {
                self.probe_ok();
                // 退避期间本机的改动没有安排上传；服务器又能访问了（探测本身就受退避约束），现在补上
                if self.take_deferred_upload(clock_ms()) {
                    self.trigger(CycleReason::LocalChange);
                }
            }
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

    /// 退避期间有过本机改动、现在自动同步又允许上传了：取走这个标记（只有一个调用方能拿到 true）
    pub(super) fn take_deferred_upload(&self, now: i64) -> bool {
        self.realtime.upload_deferred.load(Ordering::Acquire)
            && self.gate.allows(CycleReason::LocalChange, now)
            && self.realtime.upload_deferred.swap(false, Ordering::AcqRel)
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
            // 状态里的这条错误是探测放上去的：之后的探测都是「没有变化」、不跑一轮，要由下一次成功的探测清掉
            self.realtime.error_shown.store(true, Ordering::Release);
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

    /// 本轮成功之后交出其他设备的新剪贴板条目：实时同步此刻真的在运行（设置允许、任务在跑）、并且装了回调时。
    /// 只看设置里的开关不够：关掉自动同步、服务器不支持、因需要处理的错误暂停时开关还开着，设置页却显示为关
    pub(super) fn hand_off_remote_clips(&self, reason: CycleReason, clips: &[RemoteClip]) {
        if clips.is_empty() || !self.realtime.alive() || !wanted_by_settings(&self.settings.get().sync) {
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

    /// 探测发现变化后的那一轮结束：连续几次什么都没拿到也没推送，就按「服务器不支持」停下（签名不稳定，再探测下去
    /// 每次都是完整的一轮、每次都写本机数据库）
    pub(super) fn after_poll_cycle(&self, reason: CycleReason, pulled: u32, pushed: u32) {
        if !self.realtime.note_poll_cycle(reason, pulled == 0 && pushed == 0) {
            return;
        }
        log::info!(
            "sync: realtime stopped: the server reported {} changes in a row that were not there",
            plan::NOOP_CHANGES_LIMIT
        );
        self.realtime.unsupported.store(true, Ordering::Release);
        self.stop_realtime();
        self.set_realtime_state(SyncRealtime::Unsupported);
    }
}

#[cfg(test)]
mod tests {
    use wisp_sync::RemoteError;
    use wisp_types::SyncErrorKind;

    use super::*;
    use crate::settings::SettingsService;
    use crate::settings::service::tests::test_service;
    use crate::settings::sync_credential::SyncCredential;

    const URL: &str = "https://dav.example.com/dav/";

    fn credential() -> SyncCredential {
        SyncCredential {
            server_url: URL.into(),
            username: "me".into(),
            password: "pw".into(),
            vault_id: "0123456789abcdef".into(),
            // 解不出主密钥：真正要联网时按「缺少凭据」失败（这些测试都不联网）
            key: "AAAA".into(),
        }
    }

    /// 开着同步与实时同步（本机部分已开启、内存里还没有凭据）
    async fn service() -> (tempfile::TempDir, Arc<SettingsService>, Arc<SyncService>) {
        let (dir, settings) = test_service();
        settings
            .update(&mut |s| {
                s.sync.enabled = true;
                s.sync.server_url = URL.into();
                s.sync.username = "me".into();
                s.sync.realtime = true;
                s.sync.realtime_clipboard = true;
            })
            .expect("update");
        let svc = SyncService::with_parts(Arc::clone(&settings), None, "0.0.0".into());
        {
            let _life = svc.lifecycle.lock().await;
            svc.enable_locked().await;
        }
        assert!(svc.is_enabled());
        (dir, settings, svc)
    }

    async fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
        for _ in 0..300 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("超时：{what}");
    }

    fn net_error() -> Result<Peek, SyncError> {
        Err(SyncError::Remote(RemoteError::Connect))
    }

    /// 探测失败的三条路：暂时性的连续 3 次才显示、不碰自动同步的退避，恢复后只清自己放上去的错误；限流进退避并放慢，
    /// 错误同样由下一次成功的探测清掉；需要用户处理的错误暂停自动同步、任务结束
    #[tokio::test]
    async fn probe_failures_take_the_three_paths() {
        let (_dir, _settings, svc) = service().await;
        *svc.creds.lock() = Some(credential());
        assert!(svc.after_probe(net_error()));
        assert!(svc.after_probe(net_error()));
        assert_ne!(svc.status().phase, SyncPhase::Error, "前两次不显示");
        assert!(svc.after_probe(net_error()));
        let status = svc.status();
        assert_eq!(status.phase, SyncPhase::Error);
        assert_eq!(status.error, Some(SyncErrorKind::Network));
        assert!(status.transient);
        assert!(svc.gate.retry_after().is_none(), "探测失败不碰自动同步的退避");
        assert!(svc.after_probe(Ok(Peek::Unchanged)));
        let status = svc.status();
        assert_eq!(status.phase, SyncPhase::Idle, "恢复后清掉");
        assert!(status.checked_ms.is_some());

        // 一轮同步放上去的错误不归探测管
        svc.fail(&SyncError::Remote(RemoteError::Timeout));
        svc.gate.resume();
        assert!(svc.after_probe(Ok(Peek::Unchanged)));
        assert_eq!(svc.status().phase, SyncPhase::Error);
        svc.set_status(|s| {
            s.phase = SyncPhase::Idle;
            s.error = None;
        });

        // 限流：按 Retry-After 退避、之后 30 分钟放慢；错误由下一次成功的探测清掉
        let before = clock_ms();
        assert!(svc.after_probe(Err(SyncError::Remote(RemoteError::Throttled {
            status: 429,
            retry_after_s: 120
        }))));
        assert!(svc.gate.retry_after().is_some_and(|t| t >= before + 120_000));
        assert!(svc.realtime.slow_until_ms.load(Ordering::Acquire) >= before + 29 * 60_000);
        assert_eq!(svc.status().error, Some(SyncErrorKind::Server));
        assert!(svc.after_probe(Ok(Peek::Unchanged)));
        assert_eq!(svc.status().phase, SyncPhase::Idle, "限流的错误也由探测清掉");

        // 需要用户处理：暂停自动同步，任务应当结束，凭据放掉
        assert!(!svc.after_probe(Err(SyncError::Remote(RemoteError::Status(401)))));
        assert!(svc.gate.paused());
        assert!(svc.creds.lock().is_none());
        assert_eq!(svc.status().error, Some(SyncErrorKind::Auth));
        svc.deactivate().await;
    }

    /// 探测被限流、自动同步退避期间本机有了改动：去抖不安排，退避结束后的第一次探测补发上传（不必等用户再操作）
    #[tokio::test]
    async fn local_changes_during_a_probe_backoff_are_uploaded_by_the_next_probe() {
        let (_dir, _settings, svc) = service().await;
        assert!(svc.after_probe(Err(SyncError::Remote(RemoteError::Throttled {
            status: 503,
            retry_after_s: 60
        }))));
        svc.local_changed();
        assert!(svc.status().pending);
        assert!(svc.realtime.upload_deferred.load(Ordering::Acquire));
        // 退避期间（规划器不会在这时探测，这里直接交一个结果）：仍然不上传
        assert!(svc.after_probe(Ok(Peek::Unchanged)));
        assert!(svc.realtime.upload_deferred.load(Ordering::Acquire));
        assert!(!svc.running.load(Ordering::Acquire));

        // 退避到期（这里直接清掉）：下一次探测发起上传。拿住一轮的锁，让发起的那一轮停在开头，好看到它
        let held = svc.cycle_lock.lock().await;
        svc.gate.resume();
        assert!(svc.after_probe(Ok(Peek::Unchanged)));
        assert!(!svc.realtime.upload_deferred.load(Ordering::Acquire), "标记取走了");
        assert!(svc.running.load(Ordering::Acquire), "发起了一轮");
        drop(held);
        wait_until("那一轮结束", || !svc.running.load(Ordering::Acquire)).await;
        // 只发起一次
        assert!(!svc.take_deferred_upload(clock_ms()));

        // 一轮成功（不管什么原因）、结束时没有待上传的改动：标记清掉；还有（那一轮进行中又改了，当时仍在退避）就留着
        svc.realtime.defer_upload();
        svc.realtime.note_cycle(clock_ms(), true, false, 3, true);
        assert!(svc.realtime.upload_deferred.load(Ordering::Acquire));
        svc.realtime.note_cycle(clock_ms(), true, false, 3, false);
        assert!(!svc.realtime.upload_deferred.load(Ordering::Acquire));
        svc.deactivate().await;
    }

    /// 其他设备的复制只在实时同步真的运行时交给剪贴板服务：任务没起来、关掉了自动同步（开关还开着、设置页显示为关）、
    /// 服务器被判为不支持时，「立即同步」这类轮次拉到的条目都不交
    #[tokio::test]
    async fn remote_copies_are_handed_off_only_while_realtime_runs() {
        let (_dir, settings, svc) = service().await;
        let seen: Arc<Mutex<Vec<CycleReason>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        svc.set_remote_clip_hook(Some(Arc::new(move |a: RemoteClipArrival| sink.lock().push(a.reason))));
        let clip = RemoteClip {
            hash: [7; 32],
            recorded_ms: 1,
            last_used_ms: 1,
        };
        svc.hand_off_remote_clips(CycleReason::Manual, &[clip]);
        assert!(seen.lock().is_empty(), "任务还没起来");

        *svc.creds.lock() = Some(credential());
        svc.ensure_realtime();
        assert!(svc.realtime.alive());
        svc.hand_off_remote_clips(CycleReason::Manual, &[]);
        assert!(seen.lock().is_empty(), "没有条目时不调用");
        svc.hand_off_remote_clips(CycleReason::ClipboardShown, &[clip]);
        assert_eq!(
            *seen.lock(),
            [CycleReason::ClipboardShown],
            "运行期间任何原因的轮次都算"
        );

        // 关掉自动同步：实时同步的开关还开着，但已经不在运行（跟随设置的任务停下它之前也不交）
        settings.update(&mut |s| s.sync.auto = false).expect("update");
        svc.hand_off_remote_clips(CycleReason::Manual, &[clip]);
        assert_eq!(seen.lock().len(), 1);
        svc.follow_realtime();
        wait_until("任务退出", || !svc.realtime.alive()).await;
        settings.update(&mut |s| s.sync.auto = true).expect("update");
        svc.follow_realtime();
        assert!(svc.realtime.alive());

        // 服务器的签名不稳定：连续几次「有变化」却什么都没拿到，按不支持停下，之后也不交
        for _ in 0..plan::NOOP_CHANGES_LIMIT {
            svc.after_poll_cycle(CycleReason::Poll, 0, 0);
        }
        assert!(!svc.realtime.alive());
        assert_eq!(svc.status().realtime, SyncRealtime::Unsupported);
        svc.ensure_realtime();
        assert!(!svc.realtime.alive(), "不再自动开始");
        svc.hand_off_remote_clips(CycleReason::Manual, &[clip]);
        assert_eq!(seen.lock().len(), 1);
        // 用户重新打开实时同步：再试一次
        svc.realtime_reenabled();
        svc.ensure_realtime();
        assert!(svc.realtime.alive());
        svc.deactivate().await;
    }

    /// 停下（不等）的任务句柄留着：关闭同步时一并等它们退出，之后才关库
    #[tokio::test]
    async fn closing_waits_for_tasks_that_were_stopped_earlier() {
        let (_dir, _settings, svc) = service().await;
        *svc.creds.lock() = Some(credential());
        svc.ensure_realtime();
        svc.stop_realtime();
        assert!(!svc.realtime.alive());
        assert_eq!(svc.realtime.stopping.lock().len(), 1, "句柄留着");

        // 一个取消之后还要一会儿才退出的任务（例如正在阻塞线程里解锁副本）
        let exited = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&exited);
        svc.realtime
            .stopping
            .lock()
            .push(tauri::async_runtime::spawn(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                flag.store(true, Ordering::Release);
            }));
        svc.stop_realtime_and_wait().await;
        assert!(exited.load(Ordering::Acquire), "等到它退出");
        assert!(svc.realtime.stopping.lock().is_empty());
        svc.deactivate().await;
    }

    /// 启动后在后台读凭据：读的期间有人在断开（拿着 `lifecycle`）或已经断开，读回来的凭据不缓存
    #[tokio::test]
    async fn startup_credentials_are_not_cached_across_a_disconnect() {
        let (_dir, settings, svc) = service().await;
        settings.sync_credentials().set(&credential()).expect("save");
        let life = svc.lifecycle.lock().await;
        svc.after_startup();
        // 让后台任务先读到钥匙串，再模拟断开把凭据删掉
        tokio::time::sleep(Duration::from_millis(50)).await;
        svc.gate.set_enabled(false);
        settings.sync_credentials().delete().expect("delete");
        settings.update(&mut |s| s.sync.enabled = false).expect("update");
        drop(life);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(svc.creds.lock().is_none(), "断开之后不留密码");
        assert!(!svc.realtime.alive());

        // 没有人在断开：照常缓存并开始
        settings.sync_credentials().set(&credential()).expect("save");
        settings.update(&mut |s| s.sync.enabled = true).expect("update");
        svc.gate.set_enabled(true);
        svc.after_startup();
        wait_until("读到凭据并开始", || {
            svc.creds.lock().is_some() && svc.realtime.alive()
        })
        .await;
        svc.deactivate().await;
    }

    /// 同步循环与实时同步都停了才放掉保留的客户端；一轮还在跑时只收短保留时长
    #[tokio::test]
    async fn the_kept_client_is_released_by_whichever_side_stops_last() {
        let (_dir, _settings, svc) = service().await;
        let remote = svc.shared_remote(&credential()).expect("client");
        let short = remote.keep_alive();
        remote.set_keep_alive(true);
        assert!(remote.keep_alive() > short);
        svc.running.store(true, Ordering::Release);
        svc.release_remote_if_idle();
        assert!(svc.remote.lock().is_some(), "一轮还在跑");
        assert_eq!(remote.keep_alive(), short);
        svc.running.store(false, Ordering::Release);
        svc.release_remote_if_idle();
        assert!(svc.remote.lock().is_none());
        svc.deactivate().await;
    }

    /// 悬浮剪贴板只是开着不算正在看：交互之后 [`FLOAT_HOT_MS`] 内是热档，之后在下一次规划时按输入空闲落回活跃档
    /// （不另设计时器）；剪贴板面板显示着始终是热档
    #[test]
    fn the_floating_clipboard_is_hot_only_shortly_after_use() {
        let at = 10 * FLOAT_HOT_MS;
        assert!(hot_view_of(true, false, None, at), "剪贴板面板");
        assert!(hot_view_of(true, true, Some(1), at), "面板不看交互");
        assert!(!hot_view_of(false, true, None, at), "开着、没交互过");
        assert!(hot_view_of(false, true, Some(at), at));
        assert!(hot_view_of(false, true, Some(at), at + FLOAT_HOT_MS - 1));
        assert!(!hot_view_of(false, true, Some(at), at + FLOAT_HOT_MS), "60 s 到期");
        assert!(!hot_view_of(false, false, Some(at), at + 1), "关掉了");

        // 接到规划器上：人在电脑前（1 s 前有输入）、悬浮窗开着
        let inputs = |now: i64| Inputs {
            wanted: true,
            now_ms: now,
            last_poll_ms: Some(now),
            input_idle_ms: Some(1_000),
            screen_idle: false,
            last_activity_ms: None,
            hot_view: hot_view_of(false, true, Some(at), now),
            last_change_ms: None,
            last_push_ms: None,
            on_battery: false,
            low_power: false,
            metered: false,
            floor_ms: Provider::Other.floor_ms(2),
            quota_used: 0,
            quota_limit: Provider::Other.budget(2),
            quota_frees_in_ms: 0,
            failures: 0,
            retry_after_ms: None,
            slow_until_ms: None,
            return_events: true,
            unlock_events: false,
            power_events: false,
            jitter: 0.0,
        };
        let soon = inputs(at + 30_000);
        assert_eq!(plan::tier(&soon), plan::Tier::Hot);
        assert!(matches!(plan::plan(&soon), Plan::Wait { ms: plan::HOT_MS, .. }));
        let later = inputs(at + FLOAT_HOT_MS);
        assert_eq!(plan::tier(&later), plan::Tier::Active);
        assert!(matches!(
            plan::plan(&later),
            Plan::Wait {
                ms: plan::ACTIVE_MS,
                ..
            }
        ));
    }

    /// 与悬浮窗的交互记下时间并唤醒在跑的任务重新规划（进热档不必等这一段等待结束）
    #[tokio::test]
    async fn using_the_floating_clipboard_wakes_the_planner() {
        let (_dir, _settings, svc) = service().await;
        let before = clock_ms();
        svc.note_float_use();
        assert!(svc.realtime.last_float_ms.load(Ordering::Acquire) >= before);
        // 只看唤醒：把「有任务在跑」的标记直接置上，不起真的任务（它会抢走这次唤醒）
        svc.realtime.alive.store(true, Ordering::Release);
        svc.note_float_use();
        tokio::time::timeout(Duration::from_millis(100), svc.realtime.nudge.notified())
            .await
            .expect("唤醒了任务");
        svc.realtime.alive.store(false, Ordering::Release);
        svc.deactivate().await;
    }

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

    /// 探测拿到的列表只交给紧接着的 `Poll` 那一轮；换成别的原因的一轮时照样取走丢掉，不留给更晚的一轮
    #[test]
    fn a_prefetched_listing_only_serves_the_poll_cycle() {
        let slot = Mutex::new(Some(1));
        assert_eq!(take_for_poll(&slot, CycleReason::LocalChange), None, "别的原因不用");
        assert!(slot.lock().is_none(), "也不留给更晚的一轮");
        assert_eq!(take_for_poll(&slot, CycleReason::Poll), None, "已经丢掉了");
        *slot.lock() = Some(2);
        assert_eq!(take_for_poll(&slot, CycleReason::Poll), Some(2));
        assert!(slot.lock().is_none());
        let rt = Realtime::default();
        assert!(rt.take_listing(CycleReason::Poll).is_none());
    }

    /// 签名不稳定的服务器：连续几次「有变化」的那一轮什么都没拿到才算；拿到过东西就重新数，别的原因的轮次不算
    #[test]
    fn only_consecutive_empty_poll_cycles_count_as_an_unstable_server() {
        let rt = Realtime::default();
        for _ in 1..plan::NOOP_CHANGES_LIMIT {
            assert!(!rt.note_poll_cycle(CycleReason::Poll, true));
        }
        assert!(!rt.note_poll_cycle(CycleReason::ClipboardShown, true), "别的原因不算");
        assert!(!rt.note_poll_cycle(CycleReason::Poll, false), "拿到了东西：重新数");
        for _ in 1..plan::NOOP_CHANGES_LIMIT {
            assert!(!rt.note_poll_cycle(CycleReason::Poll, true));
        }
        assert!(rt.note_poll_cycle(CycleReason::Poll, true));
    }
}
