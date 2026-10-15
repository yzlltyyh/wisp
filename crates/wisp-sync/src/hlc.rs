//! 混合逻辑时钟（HLC）。
//!
//! `Hlc(u64)` = `物理毫秒 << 16 | 计数`。时间戳在**捕获改动的那一刻**分配（配置钩子、剪贴板 sink 内），
//! 所以分配必须是 O(1) 且无锁：[`Clock`] 用一个原子量做 `fetch_update`。
//! 比较两条记录时用 `(t, device_id)` 的字典序，物理时间只是让「后改的赢」在大多数情况下符合直觉；
//! 本机时钟回拨也不会产生更小的 t（取 `max(now << 16, last + 1)`）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// 混合逻辑时间戳
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Hlc(pub u64);

/// 远端时间戳的物理部分超前本机这么多时只记一条警告（仍然接受，否则两台时钟不准的设备会互相拒绝）
pub const MAX_DRIFT_MS: u64 = 24 * 60 * 60 * 1000;

impl Hlc {
    /// 计数部分占的位数
    pub const COUNTER_BITS: u32 = 16;

    /// 加入时「等于内置默认值」的配置所用的底值：任何真实改动都比它新
    pub const FLOOR_DEFAULT: Hlc = Hlc(0);

    /// 加入时「本机非默认值、服务器没有记录」的配置所用的底值：只赢过默认值的底值
    pub const FLOOR_VALUE: Hlc = Hlc(1);

    /// 只有物理时间、计数为 0 的时间戳（剪贴板 / 识别条目在加入时按条目自身的时间发布）
    pub const fn from_wall_ms(ms: u64) -> Hlc {
        Hlc(ms << Self::COUNTER_BITS)
    }

    /// 同 [`Self::from_wall_ms`]，负数（时间早于 1970 年的异常数据）按 0 处理
    pub fn from_wall_ms_i64(ms: i64) -> Hlc {
        Self::from_wall_ms(u64::try_from(ms).unwrap_or(0))
    }

    /// 物理毫秒部分
    pub const fn wall_ms(self) -> u64 {
        self.0 >> Self::COUNTER_BITS
    }

    /// 计数部分
    pub const fn counter(self) -> u16 {
        // 截断是有意的：低 16 位就是计数
        (self.0 & 0xFFFF) as u16
    }

    /// 是否是加入时使用的底值（物理时间为 0）
    pub const fn is_floor(self) -> bool {
        self.wall_ms() == 0
    }

    /// SQLite 只有 i64：真实时间戳远小于 2^63（约公元 4400 万年），超出时饱和
    pub fn to_i64(self) -> i64 {
        i64::try_from(self.0).unwrap_or(i64::MAX)
    }

    /// [`Self::to_i64`] 的逆；负数（不应出现）按 0 处理
    pub fn from_i64(v: i64) -> Hlc {
        Hlc(u64::try_from(v).unwrap_or(0))
    }
}

/// 当前系统时间（毫秒）；早于 1970 年的系统时钟按 0 处理
pub fn wall_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// 原子的 HLC 时钟。`now` 与 `observe` 都是一次 CAS 循环，可以在任意线程、任意锁内调用
#[derive(Debug)]
pub struct Clock {
    last: AtomicU64,
    source: fn() -> u64,
}

impl Clock {
    /// `last` 是上次持久化的值（从 sync.db 读出；新库用 `Hlc::default()`）
    pub fn new(last: Hlc) -> Clock {
        Self::with_source(last, wall_now_ms)
    }

    /// 指定物理时间来源（测试用）
    pub fn with_source(last: Hlc, source: fn() -> u64) -> Clock {
        Clock {
            last: AtomicU64::new(last.0),
            source,
        }
    }

    /// 分配一个新的时间戳：`max(now << 16, last + 1)`，严格大于之前分配或观察到的任何值
    pub fn now(&self) -> Hlc {
        let physical = Hlc::from_wall_ms((self.source)()).0;
        let mut next = 0;
        // fetch_update 的闭包总是返回 Some，不会失败
        let _ = self.last.fetch_update(Ordering::AcqRel, Ordering::Acquire, |last| {
            next = physical.max(last.saturating_add(1));
            Some(next)
        });
        Hlc(next)
    }

    /// 收到远端记录时推进时钟：`last = max(last, remote)`。
    /// 返回 `true` 表示远端物理时间超前本机超过 [`MAX_DRIFT_MS`]，调用方记一条不含内容的警告
    pub fn observe(&self, remote: Hlc) -> bool {
        self.last.fetch_max(remote.0, Ordering::AcqRel);
        remote.wall_ms() > (self.source)().saturating_add(MAX_DRIFT_MS)
    }

    /// 当前已分配 / 观察到的最大值，用于持久化
    pub fn last(&self) -> Hlc {
        Hlc(self.last.load(Ordering::Acquire))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn fixed_1000() -> u64 {
        1000
    }

    fn fixed_zero() -> u64 {
        0
    }

    #[test]
    fn layout_and_floors() {
        let t = Hlc::from_wall_ms(1_700_000_000_123);
        assert_eq!(t.wall_ms(), 1_700_000_000_123);
        assert_eq!(t.counter(), 0);
        assert!(Hlc::FLOOR_DEFAULT < Hlc::FLOOR_VALUE);
        assert!(Hlc::FLOOR_VALUE < Hlc::from_wall_ms(1));
        assert!(Hlc::FLOOR_DEFAULT.is_floor() && Hlc::FLOOR_VALUE.is_floor());
        assert!(!Hlc::from_wall_ms(1).is_floor());
        assert_eq!(Hlc::from_wall_ms_i64(-5), Hlc(0));
        assert_eq!(Hlc::from_i64(t.to_i64()), t);
    }

    #[test]
    fn now_uses_physical_time_then_counter() {
        let clock = Clock::with_source(Hlc::default(), fixed_1000);
        let a = clock.now();
        let b = clock.now();
        assert_eq!(a, Hlc::from_wall_ms(1000));
        assert_eq!(b, Hlc(a.0 + 1));
        assert_eq!(clock.last(), b);
    }

    #[test]
    fn clock_never_goes_backwards() {
        // 持久化的值比当前物理时间新（系统时钟回拨）
        let clock = Clock::with_source(Hlc::from_wall_ms(5000), fixed_1000);
        let a = clock.now();
        assert!(a > Hlc::from_wall_ms(5000));
    }

    #[test]
    fn observe_advances_and_flags_drift() {
        let clock = Clock::with_source(Hlc::default(), fixed_zero);
        assert!(!clock.observe(Hlc::from_wall_ms(10)));
        assert!(clock.now() > Hlc::from_wall_ms(10));
        assert!(clock.observe(Hlc::from_wall_ms(MAX_DRIFT_MS + 1)));
        // 观察更小的值不会让时钟后退
        let before = clock.last();
        clock.observe(Hlc(1));
        assert_eq!(clock.last(), before);
    }

    #[test]
    fn serializes_as_plain_number() {
        let t = Hlc::from_wall_ms(1_760_000_000_000);
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(json, t.0.to_string());
        assert_eq!(serde_json::from_str::<Hlc>(&json).unwrap(), t);
    }

    proptest! {
        #[test]
        fn now_is_strictly_increasing(ops in proptest::collection::vec(prop_oneof![Just(None), any::<u64>().prop_map(Some)], 1..64)) {
            let clock = Clock::new(Hlc::default());
            let mut prev = clock.now();
            for op in ops {
                if let Some(remote) = op {
                    clock.observe(Hlc(remote >> 1));
                }
                let next = clock.now();
                prop_assert!(next > prev);
                prev = next;
            }
        }
    }
}
