//! 剪贴板 4 Hz changeCount 轮询的「屏幕空闲」闸门。
//!
//! 主进程稳态空闲时唯一的周期唤醒就是这个轮询（空闲预算里唯一允许的周期唤醒）。显示器睡眠、
//! 锁屏、快速用户切换走开时，没有人会复制，4 Hz 轮询纯属浪费：
//! - 收到 `NSWorkspaceScreensDidSleep` / `com.apple.screenIsLocked` / `NSWorkspaceSessionDidResignActive` 时，把全部监听
//!   定时器改成 30 s 一次（leeway 10 s，系统可以把它并进别的唤醒）——不是彻底停掉：万一漏了恢复的通知，最迟 30 s 后自愈；
//! - 收到 `com.apple.screenIsUnlocked` / `NSWorkspaceSessionDidBecomeActive` 时立即恢复；`NSWorkspaceScreensDidWake` 之后可能
//!   仍在锁屏，按屏幕的实际状态决定（[`screen_idle_now`]）。恢复时定时器立即触发一次：锁屏期间的变化（例如通用剪贴板）
//!   照常记下最新的一条；
//! - 慢速 tick 时核对一次屏幕的实际状态（`CGDisplayIsAsleep` + `CGSessionCopyCurrentDictionary`），已经不空闲就恢复。
//!
//! 通知在主线程上送达（工作区通知由 NSWorkspace 在主线程发布；分布式通知用 DeliverImmediately）。没有 NSApplication 事件
//! 循环时（单测、命令行工具）收不到通知，闸门一直开着，行为与没有闸门时相同。
//!
//! **用户空闲分档**：屏幕亮着、但人不在键盘前时 4 Hz 同样是浪费。
//! 每次 tick 读一次 `CGEventSourceSecondsSinceLastEventType`（合并会话状态、任意输入事件；约 0.3 µs，不需要权限），与
//! 最近一次 changeCount 变化一起决定档位：
//! - 30 s 内有输入或变化：活跃（调用方给的 4 Hz / leeway 50 ms）；
//! - 安静 30 s 起：1 Hz / leeway 250 ms；安静 5 分钟起：0.2 Hz / leeway 1 s；屏幕空闲（上面的闸门）优先，30 s 一次；
//! - 慢档的 tick 发现期间有过输入、或 changeCount 变了，立即回到 4 Hz；Wisp 自己要看最新内容时（剪贴板面板即将显示）经
//!   [`PollGate::nudge`] 立即读一次并回到 4 Hz，面板里永远是最新的。
//!
//! 用户离开之后的第一次复制本身就是输入事件，最多晚一个慢档周期（1 s / 5 s）记进历史，之后立即回到 4 Hz；没有输入的
//! 程序化写入（脚本、通用剪贴板）同样最多晚一个周期。

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchRetained, DispatchSource, DispatchTime};
use objc2_app_kit::{
    NSWorkspaceScreensDidSleepNotification, NSWorkspaceScreensDidWakeNotification,
    NSWorkspaceSessionDidBecomeActiveNotification, NSWorkspaceSessionDidResignActiveNotification,
};
use objc2_core_foundation::{CFBoolean, CFString, CFType};
use objc2_core_graphics::{
    CGDisplayIsAsleep, CGEventSource, CGEventSourceStateID, CGEventType, CGMainDisplayID,
    CGSessionCopyCurrentDictionary,
};
use objc2_foundation::{NSNotification, NSNotificationName};
use parking_lot::Mutex;

use super::Ctx;
use super::focus::WorkspaceObserver;
use super::notify::DistributedObserver;
use super::util::guarded;

/// 屏幕空闲时的周期：只为在漏掉恢复通知时自愈，以及补上空闲期间极少见的程序化变化
const IDLE_INTERVAL: Duration = Duration::from_secs(30);
/// 屏幕空闲时的 leeway（允许系统把它并进别的唤醒）
const IDLE_LEEWAY: Duration = Duration::from_secs(10);

/// 用户安静（没有输入、changeCount 没变）这么久之后放慢到 1 Hz
const QUIET_AFTER: Duration = Duration::from_secs(30);
/// 安静这么久之后放慢到 0.2 Hz
const AWAY_AFTER: Duration = Duration::from_secs(300);
/// 1 Hz 档
const QUIET_CADENCE: Cadence = Cadence {
    interval: Duration::from_secs(1),
    leeway: Duration::from_millis(250),
};
/// 0.2 Hz 档
const AWAY_CADENCE: Cadence = Cadence {
    interval: Duration::from_secs(5),
    leeway: Duration::from_secs(1),
};

/// 锁屏 / 解锁的分布式通知（loginwindow 发布，沿用多年的公开约定）
const SCREEN_LOCKED: &str = "com.apple.screenIsLocked";
const SCREEN_UNLOCKED: &str = "com.apple.screenIsUnlocked";

/// 屏幕状态信号
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Signal {
    /// 显示器睡眠、锁屏、会话被切走：暂停
    Idle,
    /// 解锁、会话切回：恢复
    Active,
    /// 显示器醒来：可能仍在锁屏，按实际状态决定
    Recheck,
}

/// 监听定时器的周期设定（活跃时的周期由调用方给出：剪贴板的 250 ms / leeway 50 ms）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cadence {
    pub interval: Duration,
    pub leeway: Duration,
}

/// 用户空闲分档（屏幕空闲另由 `paused` 表示、优先于它）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Tier {
    /// 最近 30 s 内有输入或 changeCount 变化：调用方给的活跃周期（4 Hz）
    #[default]
    Active,
    /// 安静 30 s 起：1 Hz
    Quiet,
    /// 安静 5 分钟起：0.2 Hz
    Away,
}

impl Tier {
    /// 安静了 `quiet` 之后应处的档位
    pub(crate) fn for_quiet(quiet: Duration) -> Self {
        if quiet >= AWAY_AFTER {
            Self::Away
        } else if quiet >= QUIET_AFTER {
            Self::Quiet
        } else {
            Self::Active
        }
    }
}

/// 闸门：登记全部监听定时器，屏幕空闲时一起放慢
pub(crate) struct PollGate {
    active: Cadence,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    paused: bool,
    /// 用户空闲分档（`paused` 时不起作用，恢复时回到 Active）
    tier: Tier,
    /// 最近一次看到 changeCount 变化、或 Wisp 自己要用剪贴板（[`PollGate::nudge`]）的时刻（None：还没有过，只按输入算）
    last_activity: Option<Instant>,
    next_id: u64,
    sources: Vec<(u64, DispatchRetained<DispatchSource>)>,
    /// 已经请求在主线程上挂系统通知（只挂一次）
    observing: bool,
    /// 系统通知观察者（第一次登记定时器时在主线程上挂上，之后一直保留：事件驱动，没有成本）
    observers: Option<Observers>,
}

struct Observers {
    _workspace: Vec<WorkspaceObserver>,
    _distributed: Vec<DistributedObserver>,
}

impl PollGate {
    pub(crate) fn new(active: Cadence) -> Self {
        Self {
            active,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// 登记一个监听定时器并按当前状态设定周期（第一次登记时挂上系统通知）；返回登记号（[`Self::remove`] 用）。
    /// 定时器由调用方激活与取消
    pub(crate) fn add(&self, ctx: &Arc<Ctx>, source: DispatchRetained<DispatchSource>) -> u64 {
        let (id, first) = {
            let mut g = self.inner.lock();
            self.arm(&source, g.paused, g.tier, true);
            g.next_id += 1;
            let id = g.next_id;
            g.sources.push((id, source));
            (id, !std::mem::replace(&mut g.observing, true))
        };
        if first {
            // 观察者挂在主线程上（分布式通知经主线程的 run loop 送达；监听可能在任何线程上开始）
            let weak = Arc::downgrade(ctx);
            ctx.main.run_async(move |_mtm| {
                if let Some(ctx) = weak.upgrade() {
                    let observers = observe(&ctx);
                    ctx.clip.gate().inner.lock().observers = Some(observers);
                }
            });
        }
        id
    }

    /// 注销（监听结束时）
    pub(crate) fn remove(&self, id: u64) {
        self.inner.lock().sources.retain(|(i, _)| *i != id);
    }

    /// 是否处于屏幕空闲（慢速）状态
    pub(crate) fn is_paused(&self) -> bool {
        self.inner.lock().paused
    }

    /// 切换状态；变化时立即改写全部定时器的周期（恢复时立即触发一次、回到活跃档）。返回是否变化
    pub(crate) fn set_paused(&self, paused: bool) -> bool {
        let mut g = self.inner.lock();
        if g.paused == paused {
            return false;
        }
        g.paused = paused;
        if !paused {
            // 解锁 / 会话切回：人回来了
            g.tier = Tier::Active;
        }
        for (_, s) in &g.sources {
            self.arm(s, paused, g.tier, true);
        }
        log::debug!(
            "clipboard polling {}",
            if paused { "slowed: screens idle" } else { "resumed" }
        );
        true
    }

    /// 当前的用户空闲分档
    #[cfg(test)]
    pub(crate) fn tier(&self) -> Tier {
        self.inner.lock().tier
    }

    /// 每次活跃周期 / 慢档的 tick 之后调用（屏幕空闲时不调用）：按「多久没有输入、changeCount 多久没变」换档。
    /// `changed`：这次 tick 读到了新的 changeCount
    pub(crate) fn after_tick(&self, changed: bool) {
        self.retier(changed, Instant::now(), user_idle());
    }

    /// [`Self::after_tick`] 的可测部分：`idle` 是距上一次输入事件的时长
    fn retier(&self, changed: bool, now: Instant, idle: Duration) -> Option<Tier> {
        let mut g = self.inner.lock();
        if g.paused {
            return None;
        }
        if changed {
            g.last_activity = Some(now);
        }
        let since_activity = g
            .last_activity
            .map_or(Duration::MAX, |t| now.saturating_duration_since(t));
        let next = Tier::for_quiet(idle.min(since_activity));
        if next == g.tier {
            return None;
        }
        g.tier = next;
        // 刚读过一次：变慢、变快都从下一个周期起算（变快 = 期间有过输入，4 Hz 下一次在 250 ms 内）
        for (_, s) in &g.sources {
            self.arm(s, false, next, false);
        }
        log::debug!("clipboard polling tier {next:?}");
        Some(next)
    }

    /// Wisp 自己要用剪贴板：回到活跃档并保持 30 s，慢档时立即读一次。调用点：剪贴板面板即将显示（`poll_now`，面板里
    /// 是最新内容）、Wisp 自己写剪贴板之前的抑制布防与模拟复制回退（按 ttl 过期的抑制区间要在 4 Hz 下观察，慢档的
    /// 5 s 周期会让它先过期、把 Wisp 自己的写入记进历史）。已在活跃档时只记时刻（下一次 tick 在 250 ms 以内）；屏幕空闲时
    /// 不改周期
    pub(crate) fn nudge(&self) {
        self.nudge_at(Instant::now());
    }

    fn nudge_at(&self, now: Instant) {
        let mut g = self.inner.lock();
        g.last_activity = Some(now);
        if g.paused || g.tier == Tier::Active {
            return;
        }
        g.tier = Tier::Active;
        for (_, s) in &g.sources {
            self.arm(s, false, Tier::Active, true);
        }
        log::debug!("clipboard polling nudged back to Active");
    }

    /// 处理一个屏幕状态信号
    pub(crate) fn on_signal(&self, signal: Signal) {
        match signal {
            Signal::Idle => self.set_paused(true),
            Signal::Active => self.set_paused(false),
            Signal::Recheck => self.set_paused(screen_idle_now()),
        };
    }

    /// 慢速 tick 时调用：屏幕已经不空闲（例如漏掉了解锁通知）就恢复
    pub(crate) fn recheck_if_paused(&self) {
        if self.is_paused() && !screen_idle_now() {
            self.set_paused(false);
        }
    }

    /// 某个状态下的周期
    fn cadence(&self, paused: bool, tier: Tier) -> Cadence {
        if paused {
            return Cadence {
                interval: IDLE_INTERVAL,
                leeway: IDLE_LEEWAY,
            };
        }
        match tier {
            Tier::Active => self.active,
            Tier::Quiet => QUIET_CADENCE,
            Tier::Away => AWAY_CADENCE,
        }
    }

    /// 按状态设定定时器。`now`：立即触发一次（恢复、nudge、登记）；否则从下一个周期起算。屏幕空闲时总是一个周期之后
    fn arm(&self, source: &DispatchSource, paused: bool, tier: Tier, now: bool) {
        let cadence = self.cadence(paused, tier);
        let start = if now && !paused {
            DispatchTime::NOW
        } else {
            DispatchTime::try_from(cadence.interval).unwrap_or(DispatchTime::FOREVER)
        };
        let nanos = |d: Duration| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        source.set_timer(start, nanos(cadence.interval), nanos(cadence.leeway));
    }
}

/// 距本会话上一次输入事件（键盘、鼠标、触控板，任何来源）多久。读的是窗口服务器维护的时间戳：不需要辅助功能 /
/// 输入监控权限，约 0.3 µs；取不到（没有图形会话）时返回 0（按「刚有输入」处理，行为与没有分档时相同）
fn user_idle() -> Duration {
    // kCGAnyInputEventType = ~0
    let secs =
        CGEventSource::seconds_since_last_event_type(CGEventSourceStateID::CombinedSessionState, CGEventType(u32::MAX));
    if secs.is_finite() && secs > 0.0 {
        Duration::try_from_secs_f64(secs).unwrap_or(Duration::ZERO)
    } else {
        Duration::ZERO
    }
}

/// 此刻屏幕是否空闲：主显示器睡着了，或者会话锁屏 / 不在控制台（快速用户切换）
pub(crate) fn screen_idle_now() -> bool {
    if CGDisplayIsAsleep(CGMainDisplayID()) {
        return true;
    }
    let Some(session) = CGSessionCopyCurrentDictionary() else {
        return false;
    };
    let flag = |key: &str| -> Option<bool> {
        let key = CFString::from_str(key);
        let mut out: *const c_void = std::ptr::null();
        // SAFETY: 键是有效的 CFString；取到的值按 Get 规则归字典所有，只在字典存活的这个闭包里借用
        let found = unsafe { session.value_if_present((&raw const *key).cast(), &mut out) };
        if !found || out.is_null() {
            return None;
        }
        // SAFETY: 会话字典的值都是 CF 对象；只按 CFBoolean 读取，其他类型视为没有
        let value = unsafe { &*out.cast::<CFType>() };
        value.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool)
    };
    flag("CGSSessionScreenIsLocked") == Some(true) || flag("kCGSSessionOnConsoleKey") == Some(false)
}

/// 挂上屏幕状态的系统通知
fn observe(ctx: &Arc<Ctx>) -> Observers {
    // SAFETY: 读取系统导出的通知名常量（不可变的静态 NSString）
    let names: [&NSNotificationName; 4] = unsafe {
        [
            NSWorkspaceScreensDidSleepNotification,
            NSWorkspaceSessionDidResignActiveNotification,
            NSWorkspaceScreensDidWakeNotification,
            NSWorkspaceSessionDidBecomeActiveNotification,
        ]
    };
    let workspace = names
        .into_iter()
        .zip([Signal::Idle, Signal::Idle, Signal::Recheck, Signal::Active])
        .map(|(name, signal)| {
            let weak = Arc::downgrade(ctx);
            let block = RcBlock::new(move |_n: NonNull<NSNotification>| deliver(&weak, signal));
            WorkspaceObserver::new(name, &block)
        })
        .collect();
    let distributed = [(SCREEN_LOCKED, Signal::Idle), (SCREEN_UNLOCKED, Signal::Active)]
        .into_iter()
        .filter_map(|(name, signal)| {
            let weak = Arc::downgrade(ctx);
            DistributedObserver::new(ctx.main.clone(), &[name], Arc::new(move || deliver(&weak, signal)))
        })
        .collect();
    Observers {
        _workspace: workspace,
        _distributed: distributed,
    }
}

fn deliver(weak: &Weak<Ctx>, signal: Signal) {
    // 回调在主线程的通知派发帧里：panic 不能展开穿过 AppKit / CF
    guarded("clipboard poll gate", || {
        if let Some(ctx) = weak.upgrade() {
            ctx.clip.gate().on_signal(signal);
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    use dispatch2::{DispatchObject, DispatchQoS, DispatchQueue, GlobalQueueIdentifier};

    use super::*;

    extern "C" fn count_tick(p: *mut std::ffi::c_void) {
        // SAFETY: p 是测试里的 &'static AtomicUsize
        let n = unsafe { &*p.cast::<AtomicUsize>() };
        n.fetch_add(1, Ordering::SeqCst);
    }

    fn wait_for(n: &AtomicUsize, at_least: usize, limit: Duration) -> bool {
        let until = Instant::now() + limit;
        while Instant::now() < until {
            if n.load(Ordering::SeqCst) >= at_least {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        n.load(Ordering::SeqCst) >= at_least
    }

    /// 真实的 libdispatch 定时器：空闲时放慢到 30 s 一次（窗口里不再触发），恢复时立即触发并回到活跃周期
    #[test]
    fn idle_screens_slow_the_timer_and_resume_fires_immediately() {
        static TICKS: AtomicUsize = AtomicUsize::new(0);
        let gate = PollGate::new(Cadence {
            interval: Duration::from_millis(50),
            leeway: Duration::from_millis(5),
        });
        let queue = DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(DispatchQoS::Utility));
        // SAFETY: 定时器类型常量由 libdispatch 导出；handle / mask 对定时器无意义
        let source = unsafe {
            DispatchSource::new(
                (&raw const dispatch2::_dispatch_source_type_timer).cast_mut(),
                0,
                0,
                Some(&queue),
            )
        };
        // SAFETY: context 指向静态计数器
        unsafe { source.set_context((&raw const TICKS).cast_mut().cast()) };
        source.set_event_handler_f(count_tick);
        gate.arm(&source, false, Tier::Active, true);
        source.activate();
        let kept = source.clone();
        {
            let mut g = gate.inner.lock();
            g.sources.push((1, source));
        }
        assert!(wait_for(&TICKS, 3, Duration::from_secs(2)), "活跃时按 50 ms 触发");

        assert!(gate.set_paused(true));
        assert!(!gate.set_paused(true), "重复的信号不再改写");
        std::thread::sleep(Duration::from_millis(60));
        let paused_at = TICKS.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(TICKS.load(Ordering::SeqCst), paused_at, "空闲时 400 ms 内不再触发");

        let t = Instant::now();
        gate.on_signal(Signal::Active);
        assert!(
            wait_for(&TICKS, paused_at + 1, Duration::from_millis(500)),
            "恢复时立即触发"
        );
        assert!(t.elapsed() < Duration::from_millis(500));
        assert!(wait_for(&TICKS, paused_at + 4, Duration::from_secs(2)), "回到活跃周期");
        assert!(!gate.is_paused());

        gate.remove(1);
        assert!(gate.inner.lock().sources.is_empty());
        kept.cancel();
    }

    /// 用户空闲分档的状态转移（时间与输入空闲由测试给出，不碰真实定时器）
    #[test]
    fn user_idle_tiers_follow_input_and_clipboard_changes() {
        let gate = PollGate::new(Cadence {
            interval: Duration::from_millis(250),
            leeway: Duration::from_millis(50),
        });
        let t0 = Instant::now();
        let s = Duration::from_secs;
        // 刚有输入：保持活跃
        assert_eq!(gate.retier(false, t0, s(2)), None);
        assert_eq!(gate.tier(), Tier::Active);
        // 30 s 没有输入、从没变过：1 Hz
        assert_eq!(gate.retier(false, t0 + s(1), s(31)), Some(Tier::Quiet));
        // 5 分钟没有输入：0.2 Hz
        assert_eq!(gate.retier(false, t0 + s(2), s(301)), Some(Tier::Away));
        // 没有输入、但剪贴板变了（脚本 / 通用剪贴板）：立即回到活跃，30 s 内保持
        assert_eq!(gate.retier(true, t0 + s(3), s(302)), Some(Tier::Active));
        assert_eq!(gate.retier(false, t0 + s(20), s(320)), None);
        assert_eq!(gate.retier(false, t0 + s(34), s(334)), Some(Tier::Quiet));
        // 慢档里发现期间有过输入：回到活跃
        assert_eq!(
            gate.retier(false, t0 + s(35), Duration::from_millis(400)),
            Some(Tier::Active)
        );
        // nudge：从慢档回到活跃，并保持 30 s（Wisp 自己写剪贴板、面板要最新内容）
        assert_eq!(gate.retier(false, t0 + s(80), s(45)), Some(Tier::Quiet));
        gate.nudge_at(t0 + s(81));
        assert_eq!(gate.tier(), Tier::Active);
        assert_eq!(
            gate.retier(false, t0 + s(82), s(47)),
            None,
            "nudge 之后 30 s 内保持活跃"
        );
        assert_eq!(gate.retier(false, t0 + s(112), s(77)), Some(Tier::Quiet));
        gate.nudge_at(t0 + s(113));
        // 屏幕空闲优先：不换档；恢复时回到活跃
        assert_eq!(gate.retier(false, t0 + s(150), s(55)), Some(Tier::Quiet));
        assert!(gate.set_paused(true));
        assert_eq!(gate.retier(false, t0 + s(400), s(400)), None);
        gate.nudge_at(t0 + s(401));
        assert_eq!(gate.tier(), Tier::Quiet, "屏幕空闲时 nudge 不改周期");
        assert!(gate.set_paused(false));
        assert_eq!(gate.tier(), Tier::Active);
        // 各档的周期
        assert_eq!(gate.cadence(false, Tier::Quiet).interval, s(1));
        assert_eq!(gate.cadence(false, Tier::Away).interval, s(5));
        assert_eq!(gate.cadence(true, Tier::Active).interval, IDLE_INTERVAL);
    }

    #[test]
    fn user_idle_query_is_cheap_and_sane() {
        let _ = user_idle();
        let t = Instant::now();
        for _ in 0..100 {
            let _ = user_idle();
        }
        assert!(t.elapsed() < Duration::from_millis(100), "100 次查询 {:?}", t.elapsed());
    }

    #[test]
    fn screen_idle_query_does_not_panic() {
        // 跑测试的会话通常既没锁屏、显示器也醒着；只要求查询本身可用
        let _ = screen_idle_now();
    }
}
