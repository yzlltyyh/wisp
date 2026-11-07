//! 实时同步（实验性）的探测规划：现在要不要探测服务器（[`crate::Engine::peek`]），不探测就等多久。
//!
//! WebDAV 没有推送，「实时」只能靠轮询，所以节奏完全跟着人走：
//! - **热**（2 s）：剪贴板面板或悬浮剪贴板显示着、刚看到过别的设备的改动（60 s 内）、本机刚推送过（10 s 内）；
//! - **活跃**（5 s）：30 s 内有输入；
//! - **安静**（20 s，最多 25 s）：输入空闲 30 s 到 5 分钟。25 s 以内的间隔让两次探测之间的连接不被闲置回收；
//! - **离开**：5 分钟没有输入、锁屏或睡眠时不联网，等「回到电脑前 / 解锁 / 唤醒」再立即探测；
//!   拿不到这类事件的平台只在本机低频复查输入空闲（不联网）。
//!
//! 用电池时放慢（热 3 s、活跃 10 s，安静档不超过 25 s），省电 / 低电量模式下暂停，按流量计费的网络上再放慢一倍。
//! 在这之上还有：服务商的最小间隔（坚果云的请求额度按账号计、所有设备共享，间隔随设备数放宽）、30 分钟滑动窗口里的
//! 请求额度（[`QuotaWindow`]）、±15% 的抖动（几台设备不会齐步请求）、探测失败的退避（5 s 起翻倍，最多 5 分钟），
//! 以及服务器限流（429 / 503）之后 30 分钟只用安静档。
//!
//! 这里只有纯函数：不读时钟、不联网、不起任务。时间一律是调用方给的毫秒（同一个单调时钟），随机数也由调用方给出，
//! 桌面端的任务与对真实服务器的测试共用同一份规则。

/// 热档的探测间隔
pub const HOT_MS: u64 = 2_000;
/// 活跃档
pub const ACTIVE_MS: u64 = 5_000;
/// 安静档
pub const QUIET_MS: u64 = 20_000;
/// 用电池时的热档 / 活跃档
pub const HOT_BATTERY_MS: u64 = 3_000;
pub const ACTIVE_BATTERY_MS: u64 = 10_000;
/// 安静档的上限（用电池、按流量计费、限流之后都不超过它）：低于 WebDAV 客户端空闲连接的保留时长，
/// 安静时的探测仍然复用连接
pub const QUIET_MAX_MS: u64 = 25_000;

/// 这么久内有输入算活跃
pub const ACTIVE_INPUT_MS: u64 = 30_000;
/// 这么久没有输入算离开
pub const AWAY_INPUT_MS: u64 = 5 * 60_000;
/// 取不到输入空闲（Wayland 等）时，按 Wisp 自己看到的用户活动判断：这么久之内算活跃
pub const ACTIVITY_ACTIVE_MS: u64 = 2 * 60_000;
/// 同上，这么久没有活动算离开
pub const ACTIVITY_AWAY_MS: u64 = 10 * 60_000;
/// 看到别的设备的改动之后，这么久之内保持热档（对方多半还在连着复制）
pub const HOT_AFTER_CHANGE_MS: u64 = 60_000;
/// 本机推送之后，这么久之内保持热档（用户可能正要到另一台设备上用它，回过来的改动也快）
pub const HOT_AFTER_PUSH_MS: u64 = 10_000;

/// 抖动幅度（±15%）
pub const JITTER: f64 = 0.15;
/// 拿不到「回到电脑前」事件时，安静档与离开档在本机复查输入空闲的间隔（不联网，一次系统调用）
pub const RECHECK_MS: u64 = 10_000;
/// 省电模式下复查电源状态的间隔（多数平台没有省电模式变化的通知）
pub const LOW_POWER_RECHECK_MS: u64 = 60_000;
/// 探测失败的第一次退避与上限
pub const BACKOFF_BASE_MS: u64 = 5_000;
pub const BACKOFF_MAX_MS: u64 = 5 * 60_000;
/// 服务器限流（429 / 503）之后只用安静档的时长
pub const SLOW_AFTER_THROTTLE_MS: u64 = 30 * 60_000;

/// 请求额度的滑动窗口
pub const WINDOW_MS: u64 = 30 * 60_000;
const BUCKET_MS: i64 = 60_000;
const BUCKETS: usize = 30;
/// 一般服务器每台设备每个窗口的请求额度（约 0.5 次 / s）
pub const DEFAULT_BUDGET: u32 = 900;
/// 坚果云免费版每个账号每 30 分钟的请求上限（付费版更多；按免费版保守估计）
pub const JIANGUOYUN_ACCOUNT_LIMIT: u32 = 600;
/// 实时同步最多用掉坚果云账号额度的这一部分：账号下的其他应用（以及手动同步、上传）还要用
pub const JIANGUOYUN_SHARE_PERCENT: u32 = 30;

/// 按服务器地址的主机名区分的服务商（只用来定最小间隔与额度，不写日志）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// 坚果云：按账号限额，所有设备共享
    Jianguoyun,
    /// 123 云盘：没有公开的频率限制，但每个请求较慢、并发连接少
    Pan123,
    Other,
}

impl Provider {
    pub fn from_host(host: &str) -> Provider {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let under = |domain: &str| host == domain || host.ends_with(&format!(".{domain}"));
        if under("jianguoyun.com") {
            Provider::Jianguoyun
        } else if under("123pan.cn") || under("123pan.com") {
            Provider::Pan123
        } else {
            Provider::Other
        }
    }

    /// 两次探测之间的最小间隔。坚果云：6 s × 设备数（额度按账号计，每多一台设备都要分走一份）
    pub fn floor_ms(self, devices: u32) -> u64 {
        match self {
            Provider::Jianguoyun => 6_000 * u64::from(devices.max(1)),
            Provider::Pan123 => 3_000,
            Provider::Other => HOT_MS,
        }
    }

    /// 这台设备在一个窗口里最多发多少请求（探测与同步合计）。坚果云：账号额度的 30% 按设备数平分
    pub fn budget(self, devices: u32) -> u32 {
        match self {
            Provider::Jianguoyun => (JIANGUOYUN_ACCOUNT_LIMIT * JIANGUOYUN_SHARE_PERCENT / 100 / devices.max(1)).max(1),
            Provider::Pan123 | Provider::Other => DEFAULT_BUDGET,
        }
    }
}

/// 30 分钟滑动窗口里的请求计数：30 个一分钟的桶，内存固定，不分配
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaWindow {
    /// `buckets[0]` 是 `head` 这一分钟，`buckets[i]` 是 `head - i`
    buckets: [u32; BUCKETS],
    head: i64,
}

impl Default for QuotaWindow {
    fn default() -> Self {
        QuotaWindow {
            buckets: [0; BUCKETS],
            head: i64::MIN,
        }
    }
}

impl QuotaWindow {
    fn minute(now_ms: i64) -> i64 {
        now_ms.div_euclid(BUCKET_MS)
    }

    /// 记下 `now_ms` 这一刻发出的 `n` 个请求
    pub fn note(&mut self, now_ms: i64, n: u32) {
        let minute = Self::minute(now_ms);
        if self.head == i64::MIN || minute > self.head {
            let shift = if self.head == i64::MIN {
                BUCKETS
            } else {
                usize::try_from(minute.saturating_sub(self.head))
                    .unwrap_or(BUCKETS)
                    .min(BUCKETS)
            };
            self.buckets.rotate_right(shift);
            self.buckets[..shift].fill(0);
            self.head = minute;
        }
        // 时钟往回走（调用方的时钟不会，但不信任输入）：记进最新的桶
        let i = usize::try_from(self.head.saturating_sub(minute))
            .unwrap_or(0)
            .min(BUCKETS - 1);
        self.buckets[i] = self.buckets[i].saturating_add(n);
    }

    /// 窗口里还算数的桶：`(那一分钟, 计数)`
    fn live(&self, now_ms: i64) -> impl Iterator<Item = (i64, u32)> + '_ {
        let minute = Self::minute(now_ms);
        let head = self.head;
        self.buckets.iter().enumerate().filter_map(move |(i, n)| {
            let at = head.checked_sub(i64::try_from(i).ok()?)?;
            (head != i64::MIN && *n > 0 && at > minute - BUCKETS as i64).then_some((at, *n))
        })
    }

    /// 最近 30 分钟的请求数
    pub fn used(&self, now_ms: i64) -> u32 {
        self.live(now_ms).fold(0u32, |acc, (_, n)| acc.saturating_add(n))
    }

    /// 最早的一批请求还要多久滑出窗口（窗口是空的时为 0）
    pub fn frees_in_ms(&self, now_ms: i64) -> u64 {
        self.live(now_ms).map(|(at, _)| at).min().map_or(0, |oldest| {
            u64::try_from((oldest + BUCKETS as i64) * BUCKET_MS - now_ms).unwrap_or(0)
        })
    }
}

/// 一次规划的全部输入（都是调用方此刻读到的值）
#[derive(Debug, Clone, PartialEq)]
pub struct Inputs {
    /// 实时同步应当运行：设置开着、自动同步开着、没有因需要处理的错误暂停、凭据可用、服务器支持
    pub wanted: bool,
    pub now_ms: i64,
    /// 上一次探测开始的时间（本次运行还没探测过为 None）
    pub last_poll_ms: Option<i64>,
    /// 距最近一次键盘 / 鼠标输入多久；取不到时为 None
    pub input_idle_ms: Option<u64>,
    /// 屏幕已锁定或显示器睡眠（来自系统事件；不知道时为 false）
    pub screen_idle: bool,
    /// Wisp 自己最近一次看到用户活动（打开面板、托盘、设置窗口、本机复制）的时间
    pub last_activity_ms: Option<i64>,
    /// 剪贴板面板或悬浮剪贴板正显示着
    pub hot_view: bool,
    /// 最近一次看到别的设备的改动
    pub last_change_ms: Option<i64>,
    /// 本机最近一次推送
    pub last_push_ms: Option<i64>,
    pub on_battery: bool,
    /// 省电 / 低电量 / 节电模式
    pub low_power: bool,
    /// 按流量计费的网络
    pub metered: bool,
    /// 服务商的最小间隔（[`Provider::floor_ms`]）
    pub floor_ms: u64,
    /// 窗口里已用与可用的请求数（[`QuotaWindow::used`]、[`Provider::budget`]）与最早一批滑出的时间
    pub quota_used: u32,
    pub quota_limit: u32,
    pub quota_frees_in_ms: u64,
    /// 连续的探测失败次数
    pub failures: u32,
    /// 自动同步的退避截止时间（同步失败、服务器限流之后），之前不探测
    pub retry_after_ms: Option<i64>,
    /// 服务器限流之后只用安静档，直到这个时间
    pub slow_until_ms: Option<i64>,
    /// 平台会在「回到电脑前 / 解锁 / 唤醒」时通知调用方：离开档不必在本机复查
    pub return_events: bool,
    /// [-1, 1] 之间的随机数（每次探测之后换一个，同一段等待里保持不变）
    pub jitter: f64,
}

/// 用户在不在、在做什么
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tier {
    Hot,
    Active,
    Quiet,
    Away,
}

/// 为什么停在原地不联网
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParkReason {
    /// 离开电脑（长时间没有输入、锁屏、睡眠）
    Away,
    /// 省电模式
    LowPower,
}

/// 规划结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// 现在探测
    Poll { tier: Tier, slowed: bool },
    /// 等 `ms` 毫秒再规划（期间有事件就提前）
    Wait { ms: u64, tier: Tier, slowed: bool },
    /// 不联网，等事件；`recheck_ms` 有值时到点在本机复查一次
    Park {
        recheck_ms: Option<u64>,
        reason: ParkReason,
    },
    /// 实时同步不该运行：结束
    Stop,
}

fn within(now: i64, at: Option<i64>, ms: u64) -> bool {
    at.is_some_and(|t| now.saturating_sub(t) < i64::try_from(ms).unwrap_or(i64::MAX))
}

fn since(now: i64, at: Option<i64>) -> u64 {
    at.map_or(u64::MAX, |t| u64::try_from(now.saturating_sub(t)).unwrap_or(0))
}

/// 此刻的档位
pub fn tier(i: &Inputs) -> Tier {
    if i.screen_idle {
        return Tier::Away;
    }
    let recent =
        within(i.now_ms, i.last_change_ms, HOT_AFTER_CHANGE_MS) || within(i.now_ms, i.last_push_ms, HOT_AFTER_PUSH_MS);
    let (idle, active_below, away_from) = match i.input_idle_ms {
        Some(idle) => (idle, ACTIVE_INPUT_MS, AWAY_INPUT_MS),
        None => (
            since(i.now_ms, i.last_activity_ms),
            ACTIVITY_ACTIVE_MS,
            ACTIVITY_AWAY_MS,
        ),
    };
    // 人走开了：面板开着、对方还在复制都不算数（面板没人看，回来时的那一下会立刻探测）。只有 Wisp 活动可依据时，
    // 面板正显示本身就说明人在
    if idle >= away_from && !(i.input_idle_ms.is_none() && i.hot_view) {
        return Tier::Away;
    }
    if i.hot_view || recent {
        Tier::Hot
    } else if idle < active_below {
        Tier::Active
    } else {
        Tier::Quiet
    }
}

/// 档位的基本间隔
pub fn tier_ms(tier: Tier, on_battery: bool) -> u64 {
    match (tier, on_battery) {
        (Tier::Hot, false) => HOT_MS,
        (Tier::Hot, true) => HOT_BATTERY_MS,
        (Tier::Active, false) => ACTIVE_MS,
        (Tier::Active, true) => ACTIVE_BATTERY_MS,
        (Tier::Quiet | Tier::Away, false) => QUIET_MS,
        (Tier::Quiet | Tier::Away, true) => QUIET_MAX_MS,
    }
}

/// 第 `failures` 次连续探测失败之后的最小间隔：5 s、10 s、20 s……最多 5 分钟
pub fn backoff_ms(failures: u32) -> u64 {
    if failures == 0 {
        return 0;
    }
    BACKOFF_BASE_MS
        .checked_shl(failures - 1)
        .unwrap_or(BACKOFF_MAX_MS)
        .min(BACKOFF_MAX_MS)
}

/// `ms` 加上 ±15% 的抖动（`jitter` 超出 [-1, 1] 时按边界算）
pub fn jittered(ms: u64, jitter: f64) -> u64 {
    let j = if jitter.is_finite() {
        jitter.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    (ms as f64 * (1.0 + JITTER * j)).round().max(0.0) as u64
}

/// 现在要不要探测
pub fn plan(i: &Inputs) -> Plan {
    if !i.wanted {
        return Plan::Stop;
    }
    if i.low_power {
        return Plan::Park {
            recheck_ms: Some(LOW_POWER_RECHECK_MS),
            reason: ParkReason::LowPower,
        };
    }
    let tier = tier(i);
    // 本机复查只对能取到输入空闲的平台有意义；取不到时只能等 Wisp 自己的活动
    let recheck = (!i.return_events && i.input_idle_ms.is_some()).then_some(RECHECK_MS);
    if tier == Tier::Away {
        return Plan::Park {
            recheck_ms: recheck,
            reason: ParkReason::Away,
        };
    }

    let mut interval = tier_ms(tier, i.on_battery);
    if i.metered {
        interval = (interval * 2).min(QUIET_MAX_MS).max(interval);
    }
    interval = interval.max(i.floor_ms);
    let mut slowed = false;
    if i.slow_until_ms.is_some_and(|t| i.now_ms < t) {
        interval = interval.max(QUIET_MAX_MS);
        slowed = true;
    }
    let limit = i.quota_limit.max(1);
    let exhausted = i.quota_used >= limit;
    // 窗口用掉一半以后按「额度刚好用到窗口结束」的平均间隔走：热档的短间隔可以先用，但不会把额度一口气用完
    let sustainable = WINDOW_MS / u64::from(limit);
    if exhausted {
        slowed = true;
    } else if i.quota_used.saturating_mul(2) >= limit && sustainable > interval {
        interval = sustainable;
        slowed = true;
    }
    interval = interval.max(backoff_ms(i.failures));
    // 抖动之后也不低于服务商的最小间隔
    let step = i64::try_from(jittered(interval, i.jitter).max(i.floor_ms)).unwrap_or(i64::MAX);

    let mut due = i.last_poll_ms.map_or(i.now_ms, |t| t.saturating_add(step));
    if let Some(t) = i.retry_after_ms {
        due = due.max(t);
    }
    if exhausted {
        due = due.max(
            i.now_ms
                .saturating_add(i64::try_from(i.quota_frees_in_ms).unwrap_or(i64::MAX)),
        );
    }
    if i.now_ms >= due {
        return Plan::Poll { tier, slowed };
    }
    let mut ms = u64::try_from(due - i.now_ms).unwrap_or(u64::MAX);
    if tier == Tier::Quiet
        && let Some(r) = recheck
    {
        // 安静时人回来了要尽快换到活跃档：没有事件可等，就在本机复查
        ms = ms.min(r);
    }
    Plan::Wait { ms, tier, slowed }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000;

    fn base() -> Inputs {
        Inputs {
            wanted: true,
            now_ms: 100 * MIN,
            last_poll_ms: None,
            input_idle_ms: Some(1_000),
            screen_idle: false,
            last_activity_ms: None,
            hot_view: false,
            last_change_ms: None,
            last_push_ms: None,
            on_battery: false,
            low_power: false,
            metered: false,
            floor_ms: Provider::Other.floor_ms(2),
            quota_used: 0,
            quota_limit: DEFAULT_BUDGET,
            quota_frees_in_ms: 0,
            failures: 0,
            retry_after_ms: None,
            slow_until_ms: None,
            return_events: true,
            jitter: 0.0,
        }
    }

    /// 刚探测过时要等多久（Poll 视为 0）
    fn wait_after_poll(i: &Inputs) -> u64 {
        let i = Inputs {
            last_poll_ms: Some(i.now_ms),
            ..i.clone()
        };
        match plan(&i) {
            Plan::Wait { ms, .. } => ms,
            Plan::Poll { .. } => 0,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn first_plan_polls_right_away() {
        assert_eq!(
            plan(&base()),
            Plan::Poll {
                tier: Tier::Active,
                slowed: false
            }
        );
    }

    #[test]
    fn tiers_follow_presence() {
        let mut i = base();
        assert_eq!(tier(&i), Tier::Active);
        assert_eq!(wait_after_poll(&i), ACTIVE_MS);
        i.input_idle_ms = Some(ACTIVE_INPUT_MS);
        assert_eq!(tier(&i), Tier::Quiet);
        assert_eq!(wait_after_poll(&i), QUIET_MS);
        i.input_idle_ms = Some(AWAY_INPUT_MS);
        assert_eq!(tier(&i), Tier::Away);
        assert!(matches!(
            plan(&i),
            Plan::Park {
                reason: ParkReason::Away,
                recheck_ms: None
            }
        ));
        // 面板开着：热档
        i.input_idle_ms = Some(1_000);
        i.hot_view = true;
        assert_eq!(tier(&i), Tier::Hot);
        assert_eq!(wait_after_poll(&i), HOT_MS);
        // 人走开了，面板开着也不算
        i.input_idle_ms = Some(AWAY_INPUT_MS + 1);
        assert_eq!(tier(&i), Tier::Away);
        // 锁屏优先于一切
        let mut locked = base();
        locked.hot_view = true;
        locked.screen_idle = true;
        assert_eq!(tier(&locked), Tier::Away);
    }

    #[test]
    fn recent_changes_and_pushes_keep_it_hot_for_a_while() {
        let mut i = base();
        i.input_idle_ms = Some(40_000);
        i.last_change_ms = Some(i.now_ms - 59_000);
        assert_eq!(tier(&i), Tier::Hot);
        i.last_change_ms = Some(i.now_ms - 60_000);
        assert_eq!(tier(&i), Tier::Quiet);
        i.last_push_ms = Some(i.now_ms - 9_000);
        assert_eq!(tier(&i), Tier::Hot);
        i.last_push_ms = Some(i.now_ms - 10_000);
        assert_eq!(tier(&i), Tier::Quiet);
    }

    #[test]
    fn without_input_idle_wisp_activity_decides() {
        let mut i = base();
        i.input_idle_ms = None;
        assert_eq!(tier(&i), Tier::Away, "没有任何依据：不联网，等 Wisp 的活动");
        i.last_activity_ms = Some(i.now_ms - 60_000);
        assert_eq!(tier(&i), Tier::Active);
        i.last_activity_ms = Some(i.now_ms - 5 * MIN);
        assert_eq!(tier(&i), Tier::Quiet);
        i.last_activity_ms = Some(i.now_ms - 10 * MIN);
        assert_eq!(tier(&i), Tier::Away);
        // 没有本机复查可做：离开时只等事件
        assert!(matches!(plan(&i), Plan::Park { recheck_ms: None, .. }));
        i.hot_view = true;
        assert_eq!(tier(&i), Tier::Hot, "面板显示着说明人在");
    }

    #[test]
    fn battery_slows_down_but_quiet_stays_capped() {
        let mut i = base();
        i.on_battery = true;
        assert_eq!(wait_after_poll(&i), ACTIVE_BATTERY_MS);
        i.hot_view = true;
        assert_eq!(wait_after_poll(&i), HOT_BATTERY_MS);
        i.hot_view = false;
        i.input_idle_ms = Some(60_000);
        assert_eq!(wait_after_poll(&i), QUIET_MAX_MS);
        i.metered = true;
        assert_eq!(wait_after_poll(&i), QUIET_MAX_MS);
        i.on_battery = false;
        i.input_idle_ms = Some(1_000);
        assert_eq!(wait_after_poll(&i), 2 * ACTIVE_MS, "按流量计费的网络慢一倍");
    }

    #[test]
    fn low_power_parks_with_a_slow_recheck() {
        let mut i = base();
        i.low_power = true;
        i.hot_view = true;
        assert_eq!(
            plan(&i),
            Plan::Park {
                recheck_ms: Some(LOW_POWER_RECHECK_MS),
                reason: ParkReason::LowPower
            }
        );
    }

    #[test]
    fn not_wanted_stops() {
        let mut i = base();
        i.wanted = false;
        assert_eq!(plan(&i), Plan::Stop);
    }

    #[test]
    fn provider_floors_and_budgets() {
        assert_eq!(Provider::from_host("dav.jianguoyun.com"), Provider::Jianguoyun);
        assert_eq!(Provider::from_host("DAV.JianGuoYun.com."), Provider::Jianguoyun);
        assert_eq!(Provider::from_host("webdav.123pan.cn"), Provider::Pan123);
        assert_eq!(Provider::from_host("x.123pan.com"), Provider::Pan123);
        assert_eq!(Provider::from_host("not123pan.cn"), Provider::Other);
        assert_eq!(Provider::from_host("nas.local"), Provider::Other);
        assert_eq!(Provider::Jianguoyun.floor_ms(0), 6_000);
        assert_eq!(Provider::Jianguoyun.floor_ms(3), 18_000);
        assert_eq!(Provider::Pan123.floor_ms(3), 3_000);
        assert_eq!(Provider::Other.floor_ms(3), HOT_MS);
        assert_eq!(Provider::Jianguoyun.budget(1), 180);
        assert_eq!(Provider::Jianguoyun.budget(2), 90);
        assert_eq!(Provider::Other.budget(5), DEFAULT_BUDGET);
        // 坚果云三台设备：热档也不快于 18 s；所有设备合计不超过账号额度的 30%
        let mut i = base();
        i.hot_view = true;
        i.floor_ms = Provider::Jianguoyun.floor_ms(3);
        i.quota_limit = Provider::Jianguoyun.budget(3);
        assert_eq!(wait_after_poll(&i), 18_000);
        let per_window = WINDOW_MS / 18_000 * 3;
        assert!(per_window <= u64::from(JIANGUOYUN_ACCOUNT_LIMIT) / 2, "{per_window}");
    }

    #[test]
    fn half_used_budget_paces_and_exhausted_budget_waits_for_the_window() {
        let mut i = base();
        i.hot_view = true;
        i.quota_limit = 90;
        i.quota_used = 44;
        assert_eq!(wait_after_poll(&i), HOT_MS);
        i.quota_used = 45;
        assert_eq!(wait_after_poll(&i), WINDOW_MS / 90, "用掉一半后按平均间隔");
        assert!(matches!(
            plan(&Inputs {
                last_poll_ms: Some(i.now_ms),
                ..i.clone()
            }),
            Plan::Wait { slowed: true, .. }
        ));
        i.quota_used = 90;
        i.quota_frees_in_ms = 7 * 60_000;
        let mut stale = i.clone();
        stale.last_poll_ms = Some(i.now_ms - 3_600_000);
        assert_eq!(
            plan(&stale),
            Plan::Wait {
                ms: 7 * 60_000,
                tier: Tier::Hot,
                slowed: true
            }
        );
    }

    #[test]
    fn jitter_stays_within_fifteen_percent_and_respects_the_floor() {
        for k in -10..=10 {
            let j = f64::from(k) / 10.0;
            let out = jittered(10_000, j);
            assert!((8_500..=11_500).contains(&out), "{j} → {out}");
        }
        assert_eq!(jittered(10_000, 5.0), 11_500);
        assert_eq!(jittered(10_000, f64::NAN), 10_000);
        let mut i = base();
        i.jitter = -1.0;
        assert_eq!(wait_after_poll(&i), 4_250);
        i.floor_ms = ACTIVE_MS;
        assert_eq!(wait_after_poll(&i), ACTIVE_MS, "抖动不低于服务商下限");
    }

    #[test]
    fn probe_backoff_doubles_and_caps() {
        let seq: Vec<u64> = (0..10).map(backoff_ms).collect();
        assert_eq!(
            seq,
            [
                0, 5_000, 10_000, 20_000, 40_000, 80_000, 160_000, 300_000, 300_000, 300_000
            ]
        );
        assert_eq!(backoff_ms(u32::MAX), BACKOFF_MAX_MS);
        let mut i = base();
        i.hot_view = true;
        i.failures = 3;
        assert_eq!(wait_after_poll(&i), 20_000);
        i.failures = 40;
        assert_eq!(wait_after_poll(&i), BACKOFF_MAX_MS, "一直失败也不停，只是 5 分钟一次");
    }

    #[test]
    fn gate_backoff_and_throttling_are_respected() {
        let mut i = base();
        i.retry_after_ms = Some(i.now_ms + 90_000);
        assert_eq!(
            plan(&i),
            Plan::Wait {
                ms: 90_000,
                tier: Tier::Active,
                slowed: false
            }
        );
        i.retry_after_ms = Some(i.now_ms - 1);
        assert!(matches!(plan(&i), Plan::Poll { .. }));
        // 限流之后 30 分钟只用安静档
        i.hot_view = true;
        i.slow_until_ms = Some(i.now_ms + 1);
        assert_eq!(wait_after_poll(&i), QUIET_MAX_MS);
        assert!(matches!(plan(&i), Plan::Poll { slowed: true, .. }));
        i.slow_until_ms = Some(i.now_ms);
        assert_eq!(wait_after_poll(&i), HOT_MS);
    }

    #[test]
    fn local_rechecks_only_without_return_events() {
        let mut i = base();
        i.return_events = false;
        i.input_idle_ms = Some(60_000);
        assert_eq!(wait_after_poll(&i), RECHECK_MS, "安静档：本机复查比探测更勤");
        i.input_idle_ms = Some(AWAY_INPUT_MS);
        assert_eq!(
            plan(&i),
            Plan::Park {
                recheck_ms: Some(RECHECK_MS),
                reason: ParkReason::Away
            }
        );
        i.input_idle_ms = Some(1_000);
        assert_eq!(wait_after_poll(&i), ACTIVE_MS, "活跃档不需要复查");
    }

    #[test]
    fn quota_window_slides() {
        let mut w = QuotaWindow::default();
        assert_eq!(w.used(0), 0);
        assert_eq!(w.frees_in_ms(0), 0);
        let t0 = 1_000 * MIN + 30_000;
        w.note(t0, 3);
        w.note(t0 + 10_000, 2);
        assert_eq!(w.used(t0 + 10_000), 5);
        w.note(t0 + MIN, 4);
        assert_eq!(w.used(t0 + MIN), 9);
        // 最早那一分钟在 30 分钟后滑出
        assert_eq!(w.frees_in_ms(t0 + MIN), (28 * MIN + 30_000) as u64);
        assert_eq!(w.used(t0 + 29 * MIN), 9);
        assert_eq!(w.used(t0 + 30 * MIN), 4);
        assert_eq!(w.used(t0 + 31 * MIN), 0);
        // 很久之后再记：旧桶全部清掉
        w.note(t0 + 600 * MIN, 1);
        assert_eq!(w.used(t0 + 600 * MIN), 1);
        // 时钟往回走的输入不会越界
        w.note(t0, 1);
        assert_eq!(w.used(t0 + 600 * MIN), 2);
    }
}
