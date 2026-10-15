//! 自动同步的触发判定：节流、退避与暂停。
//!
//! 面板显示、托盘打开这类热路径直接调用 [`Gate::allows`]，所以全部是原子读写（O(1)），不加锁、不分配。没有任何计时器：
//! 退避只记下「什么时候之后才可以再试」，由下一次用户活动来检查。时间一律取自 [`clock_ms`]。

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU32, Ordering};
use std::time::Instant;

use parking_lot::Mutex;
use wisp_sync::CycleReason;

use super::identity::now_ms;

/// 本机改动后的去抖：最后一次改动之后等这么久
pub const DEBOUNCE_MS: i64 = 2_000;
/// 去抖的上限：从第一次改动算起最多等这么久（连续不断的改动也不会一直推迟上传）
pub const DEBOUNCE_MAX_MS: i64 = 10_000;
/// 剪贴板视图显示：距上次同步至少这么久才再拉取
pub const CLIPBOARD_SHOWN_MS: i64 = 5_000;
/// 其他面板视图显示
pub const PANEL_SHOWN_MS: i64 = 60_000;
/// 托盘打开、设置窗口获焦
pub const FOCUS_MS: i64 = 15_000;
/// 暂时性失败后的第一次退避
pub const BACKOFF_BASE_MS: i64 = 60_000;
/// 退避的上限
pub const BACKOFF_MAX_MS: i64 = 30 * 60_000;

/// 节流、退避与去抖用的时钟（毫秒，进程内单调不减；第一次调用时取当时的系统时间作起点，所以总是正数）。
///
/// 不直接用系统时间：系统时间被往回调（开机后的 NTP 校正、双系统把硬件时钟当本地时间、手动改时间）时，「距上次同步」
/// 会变成负数，自动同步要停到时间追上来为止。也不只用 `Instant`：它在 macOS 与 Linux 上不计睡眠的时间，合盖一夜醒来
/// 节流与退避都还没过去。所以每次按两者各自走过的量中较大的那个前进：往回调的系统时间被忽略，睡眠期间系统时间走过的
/// 量照样计入。只在触发、一轮开始与结束、本机改动时调用，锁只保护三个数，持有时间可以忽略
pub fn clock_ms() -> i64 {
    struct Anchor {
        at: Instant,
        wall: i64,
        value: i64,
    }
    static ANCHOR: Mutex<Option<Anchor>> = Mutex::new(None);
    let mut guard = ANCHOR.lock();
    // 在锁内取时间：并发的调用按取时间的先后更新，锚点不会倒退
    let at = Instant::now();
    let wall = now_ms();
    let anchor = guard.get_or_insert(Anchor {
        at,
        wall,
        value: wall.max(1),
    });
    let mono = i64::try_from(at.saturating_duration_since(anchor.at).as_millis()).unwrap_or(i64::MAX);
    let step = mono.max(wall.saturating_sub(anchor.wall));
    anchor.value = anchor.value.saturating_add(step);
    anchor.at = at;
    anchor.wall = wall;
    anchor.value
}

/// 某种触发距上次同步的最小间隔
pub fn throttle_ms(reason: CycleReason) -> i64 {
    match reason {
        CycleReason::ClipboardShown => CLIPBOARD_SHOWN_MS,
        CycleReason::PanelShown => PANEL_SHOWN_MS,
        CycleReason::TrayOpened | CycleReason::SettingsFocused => FOCUS_MS,
        _ => 0,
    }
}

/// 用户明确要求的同步（「立即同步」、连接后的首轮）：不受自动同步开关、节流、退避与暂停的限制
pub fn is_explicit(reason: CycleReason) -> bool {
    matches!(reason, CycleReason::Manual | CycleReason::Connect)
}

/// 合并排队的触发时保留「更强」的那个：用户明确的操作优先（它们不受限制，钥匙串也只在用户活动里读）
fn rank(reason: CycleReason) -> u8 {
    match reason {
        CycleReason::LocalChange => 1,
        CycleReason::PanelShown => 2,
        CycleReason::TrayOpened => 3,
        CycleReason::SettingsFocused => 4,
        CycleReason::ClipboardShown => 5,
        CycleReason::Relaunch => 6,
        CycleReason::SettingsChanged => 7,
        CycleReason::Connect => 8,
        CycleReason::Manual => 9,
    }
}

fn from_rank(rank: u8) -> Option<CycleReason> {
    Some(match rank {
        1 => CycleReason::LocalChange,
        2 => CycleReason::PanelShown,
        3 => CycleReason::TrayOpened,
        4 => CycleReason::SettingsFocused,
        5 => CycleReason::ClipboardShown,
        6 => CycleReason::Relaunch,
        7 => CycleReason::SettingsChanged,
        8 => CycleReason::Connect,
        9 => CycleReason::Manual,
        _ => return None,
    })
}

/// 两个触发里更强的那个
pub fn stronger(a: Option<CycleReason>, b: Option<CycleReason>) -> Option<CycleReason> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if rank(a) >= rank(b) { a } else { b }),
        (a, b) => a.or(b),
    }
}

/// 一轮同步正在进行时到来的触发：合并成一个，本轮结束后再跑一次
#[derive(Debug, Default)]
pub struct Queue(AtomicU8);

impl Queue {
    pub fn push(&self, reason: CycleReason) {
        self.0.fetch_max(rank(reason), Ordering::AcqRel);
    }

    pub fn take(&self) -> Option<CycleReason> {
        from_rank(self.0.swap(0, Ordering::AcqRel))
    }

    pub fn is_empty(&self) -> bool {
        self.0.load(Ordering::Acquire) == 0
    }
}

/// 自动同步的开关、节流、退避与暂停
#[derive(Debug, Default)]
pub struct Gate {
    enabled: AtomicBool,
    auto: AtomicBool,
    /// 持续性失败：自动同步暂停，直到用户操作（连接 / 恢复、立即同步、改同步设置）
    paused: AtomicBool,
    /// 上一轮同步开始的时间（[`clock_ms`]；0 = 本进程还没同步过）
    last_cycle_ms: AtomicI64,
    /// 暂时性失败之后，在这个时间之前不自动重试
    retry_after_ms: AtomicI64,
    /// 连续的暂时性失败次数（决定下一次退避多久）
    failures: AtomicU32,
}

impl Gate {
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Release);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub fn set_auto(&self, on: bool) {
        self.auto.store(on, Ordering::Release);
    }

    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    /// 这次触发现在能不能发起一轮同步
    pub fn allows(&self, reason: CycleReason, now_ms: i64) -> bool {
        if !self.enabled() {
            return false;
        }
        if is_explicit(reason) {
            return true;
        }
        if !self.auto.load(Ordering::Acquire) {
            return false;
        }
        // 改同步设置本身就是用户操作：调用方先 resume，再按自动同步处理
        if reason == CycleReason::SettingsChanged {
            return true;
        }
        if self.paused() || now_ms < self.retry_after_ms.load(Ordering::Acquire) {
            return false;
        }
        let last = self.last_cycle_ms.load(Ordering::Acquire);
        last == 0 || now_ms.saturating_sub(last) >= throttle_ms(reason)
    }

    /// 上一轮还有剩余（流量预算用完）时能不能接着再跑一轮：这是同一次触发的延续，不受节流限制；但同样要求同步开启，
    /// 并且（「立即同步」与连接后的首轮除外）开着自动同步、没有暂停、不在退避期内
    pub fn allows_continuation(&self, reason: CycleReason, now_ms: i64) -> bool {
        if !self.enabled() {
            return false;
        }
        if is_explicit(reason) {
            return true;
        }
        self.auto.load(Ordering::Acquire) && !self.paused() && now_ms >= self.retry_after_ms.load(Ordering::Acquire)
    }

    /// 本机改动之后能不能安排去抖（退避期内、暂停时、关闭自动同步时都不安排）
    pub fn debounce_allowed(&self, now_ms: i64) -> bool {
        self.enabled()
            && self.auto.load(Ordering::Acquire)
            && !self.paused()
            && now_ms >= self.retry_after_ms.load(Ordering::Acquire)
    }

    /// 一轮同步开始联网
    pub fn started(&self, now_ms: i64) {
        self.last_cycle_ms.store(now_ms, Ordering::Release);
    }

    /// 成功：清除退避与暂停
    pub fn succeeded(&self) {
        self.failures.store(0, Ordering::Release);
        self.retry_after_ms.store(0, Ordering::Release);
        self.paused.store(false, Ordering::Release);
    }

    /// 暂时性失败：1 分钟起指数退避，上限 30 分钟。返回下一次允许自动重试的时间
    pub fn failed_transient(&self, now_ms: i64) -> i64 {
        let n = self.failures.fetch_add(1, Ordering::AcqRel).min(16);
        let delay = BACKOFF_BASE_MS.saturating_mul(1i64 << n).min(BACKOFF_MAX_MS);
        let at = now_ms.saturating_add(delay);
        self.retry_after_ms.store(at, Ordering::Release);
        at
    }

    /// 持续性失败：暂停自动同步
    pub fn failed_persistent(&self) {
        self.paused.store(true, Ordering::Release);
    }

    /// 用户操作（连接、立即同步、改同步设置）：解除暂停与退避
    pub fn resume(&self) {
        self.paused.store(false, Ordering::Release);
        self.retry_after_ms.store(0, Ordering::Release);
        self.failures.store(0, Ordering::Release);
    }
}

/// 本机改动的去抖：只在有待上传的改动时存在一个一次性的等待，没有改动时什么都不运行
#[derive(Debug, Default)]
pub struct Debounce {
    armed: AtomicBool,
    first_ms: AtomicI64,
    last_ms: AtomicI64,
}

impl Debounce {
    /// 记下一次改动；返回 true 表示调用方需要启动等待（之前没有在等）
    pub fn note(&self, now_ms: i64) -> bool {
        self.last_ms.store(now_ms, Ordering::Release);
        let _ = self
            .first_ms
            .compare_exchange(0, now_ms, Ordering::AcqRel, Ordering::Acquire);
        !self.armed.swap(true, Ordering::AcqRel)
    }

    /// 等到什么时候：最后一次改动之后 2 s，但从第一次改动算起不超过 10 s
    pub fn deadline(&self) -> i64 {
        let last = self.last_ms.load(Ordering::Acquire);
        let first = self.first_ms.load(Ordering::Acquire);
        last.saturating_add(DEBOUNCE_MS)
            .min(first.saturating_add(DEBOUNCE_MAX_MS))
    }

    /// 等待结束（触发同步之前调用）：之后的改动重新开始计时
    pub fn fire(&self) {
        self.first_ms.store(0, Ordering::Release);
        self.armed.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(auto: bool) -> Gate {
        let g = Gate::default();
        g.set_enabled(true);
        g.set_auto(auto);
        g
    }

    #[test]
    fn disabled_sync_allows_nothing() {
        let g = Gate::default();
        assert!(!g.allows(CycleReason::Manual, 1));
        assert!(!g.allows(CycleReason::Relaunch, 1));
        assert!(!g.debounce_allowed(1));
    }

    #[test]
    fn views_are_throttled_by_their_own_interval() {
        let g = gate(true);
        assert!(g.allows(CycleReason::PanelShown, 1_000), "本进程第一次");
        g.started(1_000);
        assert!(!g.allows(CycleReason::ClipboardShown, 1_000 + CLIPBOARD_SHOWN_MS - 1));
        assert!(g.allows(CycleReason::ClipboardShown, 1_000 + CLIPBOARD_SHOWN_MS));
        assert!(!g.allows(CycleReason::TrayOpened, 1_000 + FOCUS_MS - 1));
        assert!(g.allows(CycleReason::SettingsFocused, 1_000 + FOCUS_MS));
        assert!(!g.allows(CycleReason::PanelShown, 1_000 + PANEL_SHOWN_MS - 1));
        assert!(g.allows(CycleReason::PanelShown, 1_000 + PANEL_SHOWN_MS));
        assert!(g.allows(CycleReason::Relaunch, 1_001), "再次启动不节流");
    }

    #[test]
    fn auto_off_only_allows_explicit_syncs() {
        let g = gate(false);
        assert!(!g.allows(CycleReason::TrayOpened, 1));
        assert!(!g.allows(CycleReason::LocalChange, 1));
        assert!(!g.allows(CycleReason::SettingsChanged, 1));
        assert!(!g.debounce_allowed(1));
        assert!(g.allows(CycleReason::Manual, 1));
        assert!(g.allows(CycleReason::Connect, 1));
    }

    #[test]
    fn backoff_grows_and_caps_without_blocking_manual_sync() {
        let g = gate(true);
        let at = g.failed_transient(0);
        assert_eq!(at, BACKOFF_BASE_MS);
        assert!(!g.allows(CycleReason::Relaunch, at - 1));
        assert!(!g.debounce_allowed(at - 1), "退避期内不安排去抖");
        assert!(g.allows(CycleReason::Relaunch, at));
        assert!(g.allows(CycleReason::Manual, 1), "立即同步不受退避限制");
        assert_eq!(g.failed_transient(0), 2 * BACKOFF_BASE_MS);
        for _ in 0..20 {
            g.failed_transient(0);
        }
        assert_eq!(g.failed_transient(0), BACKOFF_MAX_MS);
        g.succeeded();
        assert!(g.allows(CycleReason::Relaunch, 1));
    }

    #[test]
    fn persistent_failures_pause_until_the_user_acts() {
        let g = gate(true);
        g.failed_persistent();
        assert!(!g.allows(CycleReason::TrayOpened, 10_000_000));
        assert!(!g.debounce_allowed(10_000_000));
        assert!(g.allows(CycleReason::SettingsChanged, 1));
        assert!(g.allows(CycleReason::Manual, 1));
        g.resume();
        assert!(g.allows(CycleReason::TrayOpened, 1));
    }

    #[test]
    fn continuations_skip_the_throttle_but_respect_backoff_pause_and_auto() {
        let g = gate(true);
        g.started(1_000);
        assert!(
            g.allows_continuation(CycleReason::PanelShown, 1_001),
            "同一次触发的延续不节流"
        );
        let at = g.failed_transient(1_000);
        assert!(
            !g.allows_continuation(CycleReason::ClipboardShown, at - 1),
            "退避期内不续跑"
        );
        assert!(g.allows_continuation(CycleReason::Manual, at - 1), "立即同步照常续跑");
        g.resume();
        g.failed_persistent();
        assert!(!g.allows_continuation(CycleReason::LocalChange, 1_001));
        g.resume();
        g.set_auto(false);
        assert!(!g.allows_continuation(CycleReason::LocalChange, 1_001));
        assert!(g.allows_continuation(CycleReason::Connect, 1_001));
        g.set_enabled(false);
        assert!(!g.allows_continuation(CycleReason::Manual, 1_001));
    }

    #[test]
    fn the_clock_never_goes_backwards() {
        let mut last = clock_ms();
        assert!(last > 0);
        for _ in 0..1_000 {
            let now = clock_ms();
            assert!(now >= last);
            last = now;
        }
    }

    #[test]
    fn queued_triggers_keep_the_strongest() {
        let q = Queue::default();
        assert!(q.is_empty());
        q.push(CycleReason::LocalChange);
        q.push(CycleReason::Manual);
        q.push(CycleReason::PanelShown);
        assert_eq!(q.take(), Some(CycleReason::Manual));
        assert_eq!(q.take(), None);
        assert_eq!(
            stronger(Some(CycleReason::LocalChange), Some(CycleReason::Connect)),
            Some(CycleReason::Connect)
        );
        assert_eq!(stronger(None, Some(CycleReason::Relaunch)), Some(CycleReason::Relaunch));
    }

    #[test]
    fn debounce_waits_two_seconds_but_at_most_ten() {
        let d = Debounce::default();
        assert!(d.note(1_000), "第一次改动启动等待");
        assert_eq!(d.deadline(), 3_000);
        assert!(!d.note(2_500), "等待中的改动只推迟截止时间");
        assert_eq!(d.deadline(), 4_500);
        assert!(!d.note(10_500));
        assert_eq!(d.deadline(), 11_000, "从第一次改动算起最多 10 s");
        d.fire();
        assert!(d.note(20_000));
        assert_eq!(d.deadline(), 22_000);
    }
}
