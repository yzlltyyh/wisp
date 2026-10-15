//! 按剪贴板序号区间抑制 Wisp 自己造成的变更（纯逻辑），以及变更通知的去抖计算。
//!
//! - **抑制区间**：模拟复制回退会产生多次变更（目标应用的复制与 Wisp 的恢复；`EmptyClipboard` 与每次
//!   `SetClipboardData` 都会让序号增加）。开始回退时先登记开放区间 (s0, ∞)，恢复完成后收窄为 (s0, 恢复后序号]；
//!   剪贴板没被动过时直接撤销。`ClipboardAccess::suppress` 登记的是 [from, ∞) 的开放区间；
//! - **同内容别名**：Wisp 读取剪贴板（读取、快照、回退第 ⑥ 步）时，`GetClipboardData` 会让原所有者为延迟渲染的
//!   格式调用 `SetClipboardData`，序号可能因此增加，但内容没有变。读取方一打开剪贴板就登记开放别名 (打开时序号, ∞)
//!   ——Wisp 开着剪贴板期间其他程序无法清空或写入，这段时间里的任何序号都只可能来自渲染——关闭之前收窄为
//!   (打开时序号, 关闭前序号]。监听方先按别名还原再与「已见序号」比较，Wisp 自己的读取不会被当成一次新的复制
//!   （无论系统在渲染当时还是关闭剪贴板时发出变更通知）；若打开时的那次变更还在去抖、没有投递，照常投递一次，
//!   不会丢。
//!
//! 区间与别名都有存活时间和数量上限。存活时间用 `saturating_duration_since` 判断，调用方传入 `Duration::MAX`
//! 表示「一直抑制到撤销」也不会溢出。

use std::time::{Duration, Instant};

/// 抑制区间、别名各自的数量上限：超出时丢弃最早登记的那个（正常使用中同时存活的只有一两个，上限只防止上层
/// 反复登记长 ttl 的区间时无限累积）
pub(crate) const MAX_ENTRIES: usize = 16;

/// 登记时刻 + 存活时间
#[derive(Debug, Clone, Copy)]
struct Life {
    armed: Instant,
    ttl: Duration,
}

impl Life {
    fn new(now: Instant, ttl: Duration) -> Self {
        Self { armed: now, ttl }
    }

    fn alive(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.armed) < self.ttl
    }
}

#[derive(Debug, Clone, Copy)]
struct Range {
    token: u64,
    /// 不含
    lo: u64,
    /// 含；None 表示开放
    hi: Option<u64>,
    life: Life,
}

/// (base, hi] 内的序号与 base 是同一份内容
#[derive(Debug, Clone, Copy)]
struct Alias {
    token: u64,
    base: u64,
    /// 含；None 表示开放（读取方还开着剪贴板）
    hi: Option<u64>,
    life: Life,
}

impl Alias {
    fn covers(&self, seq: u64) -> bool {
        seq > self.base && self.hi.is_none_or(|hi| seq <= hi)
    }
}

/// 抑制区间与同内容别名
#[derive(Debug, Default)]
pub(crate) struct SeqSuppressor {
    ranges: Vec<Range>,
    aliases: Vec<Alias>,
    next_token: u64,
}

/// 追加一项；已满时先丢弃最早登记的
fn push_capped<T>(v: &mut Vec<T>, item: T) {
    if v.len() >= MAX_ENTRIES {
        v.remove(0);
    }
    v.push(item);
}

impl SeqSuppressor {
    fn token(&mut self) -> u64 {
        self.next_token += 1;
        self.next_token
    }

    /// 登记开放区间 (lo, ∞)，`ttl` 后过期；返回用于收窄 / 撤销的 token
    pub(crate) fn open(&mut self, lo_exclusive: u64, ttl: Duration, now: Instant) -> u64 {
        self.prune(now);
        let token = self.token();
        push_capped(
            &mut self.ranges,
            Range {
                token,
                lo: lo_exclusive,
                hi: None,
                life: Life::new(now, ttl),
            },
        );
        token
    }

    /// 把区间收窄为 (lo, hi]，并从现在起重新计 `ttl`
    pub(crate) fn close(&mut self, token: u64, hi_inclusive: u64, ttl: Duration, now: Instant) {
        if let Some(r) = self.ranges.iter_mut().find(|r| r.token == token) {
            r.hi = Some(hi_inclusive);
            r.life = Life::new(now, ttl);
        }
    }

    /// 保持区间开放，从现在起重新计 `ttl`（恢复超时：恢复线程可能稍后才真正写入，不能按当前序号收窄）
    pub(crate) fn extend(&mut self, token: u64, ttl: Duration, now: Instant) {
        if let Some(r) = self.ranges.iter_mut().find(|r| r.token == token) {
            r.life = Life::new(now, ttl);
        }
    }

    /// 撤销区间（剪贴板没有被改动）
    pub(crate) fn cancel(&mut self, token: u64) {
        self.ranges.retain(|r| r.token != token);
    }

    /// 某序号的变更是否应被忽略（顺带清理过期项）
    pub(crate) fn should_ignore(&mut self, seq: u64, now: Instant) -> bool {
        self.prune(now);
        self.ranges
            .iter()
            .any(|r| seq > r.lo && r.hi.is_none_or(|hi| seq <= hi))
    }

    /// Wisp 打开剪贴板读取：登记开放别名 (base, ∞)，`ttl` 后过期；返回用于收窄的 token
    pub(crate) fn alias_open(&mut self, base: u64, ttl: Duration, now: Instant) -> u64 {
        self.prune(now);
        let token = self.token();
        push_capped(
            &mut self.aliases,
            Alias {
                token,
                base,
                hi: None,
                life: Life::new(now, ttl),
            },
        );
        token
    }

    /// 读取结束（关闭剪贴板之前）：把别名收窄为 (base, hi]，并从现在起重新计 `ttl`；序号没变时撤销。开放别名
    /// 已经过期或被挤掉时按 (base, hi] 重新登记
    pub(crate) fn alias_close(&mut self, token: u64, base: u64, hi: u64, ttl: Duration, now: Instant) {
        self.prune(now);
        let pos = self.aliases.iter().position(|a| a.token == token);
        match (pos, hi > base) {
            (Some(i), true) => {
                let a = &mut self.aliases[i];
                a.hi = Some(hi);
                a.life = Life::new(now, ttl);
            }
            (Some(i), false) => {
                self.aliases.remove(i);
            }
            (None, true) => push_capped(
                &mut self.aliases,
                Alias {
                    token,
                    base,
                    hi: Some(hi),
                    life: Life::new(now, ttl),
                },
            ),
            (None, false) => {}
        }
    }

    /// 按别名还原出内容真正变化时的序号（没有命中别名时就是 `seq` 本身）。别名可以首尾相接（先读取、再快照，
    /// 各渲染了一部分格式），逐级还原到最早的那个
    pub(crate) fn canonical(&mut self, seq: u64, now: Instant) -> u64 {
        self.prune(now);
        let mut s = seq;
        // 每一步都严格变小，必然终止；次数上限只是防御
        for _ in 0..=self.aliases.len() {
            match self.aliases.iter().find(|a| a.covers(s)) {
                Some(a) => s = a.base,
                None => break,
            }
        }
        s
    }

    fn prune(&mut self, now: Instant) {
        self.ranges.retain(|r| r.life.alive(now));
        self.aliases.retain(|a| a.life.alive(now));
    }
}

/// 去抖延迟：每次变更都把一次性定时器重置为 `debounce`（合并突发的多次写入），但从第一次尚未投递的变更算起最多等
/// `max_wait`——同步工具、rdpclip 持续改写剪贴板时回调不会一直发不出去。返回本次应设置的延迟，为零表示立即投递
pub(crate) fn debounce_delay(first_pending: Instant, now: Instant, debounce: Duration, max_wait: Duration) -> Duration {
    let waited = now.saturating_duration_since(first_pending);
    debounce.min(max_wait.saturating_sub(waited))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_millis(300);

    #[test]
    fn copy_fallback_range_lifecycle() {
        let t0 = Instant::now();
        let mut s = SeqSuppressor::default();
        let tok = s.open(10, Duration::from_millis(650), t0);
        // 目标应用复制、Wisp 恢复期间的任意序号都被忽略
        assert!(s.should_ignore(11, t0));
        assert!(s.should_ignore(15, t0));
        assert!(!s.should_ignore(10, t0));
        // 恢复完成：收窄到 (10, 14]
        let t1 = t0 + Duration::from_millis(100);
        s.close(tok, 14, TTL, t1);
        assert!(s.should_ignore(14, t1));
        assert!(!s.should_ignore(15, t1), "用户紧接着的复制不应被吞掉");
        // 过期
        assert!(!s.should_ignore(12, t1 + TTL + Duration::from_millis(1)));
    }

    #[test]
    fn cancel_and_open_ended_suppress() {
        let t0 = Instant::now();
        let mut s = SeqSuppressor::default();
        let tok = s.open(5, TTL, t0);
        s.cancel(tok);
        assert!(!s.should_ignore(6, t0));
        // trait 的 suppress：[from, ∞) 在 ttl 内
        s.open(19, TTL, t0);
        assert!(s.should_ignore(20, t0));
        assert!(s.should_ignore(1000, t0));
        assert!(!s.should_ignore(19, t0));
        assert!(!s.should_ignore(20, t0 + TTL));
    }

    #[test]
    fn debounce_is_trailing_but_bounded() {
        let debounce = Duration::from_millis(50);
        let max_wait = Duration::from_millis(250);
        let t0 = Instant::now();
        // 第一次变更：完整的去抖窗口
        assert_eq!(debounce_delay(t0, t0, debounce, max_wait), debounce);
        // 持续改写：每次都重置为 50 ms……
        let t1 = t0 + Duration::from_millis(120);
        assert_eq!(debounce_delay(t0, t1, debounce, max_wait), debounce);
        // ……但不会超过第一次变更后的 250 ms
        let t2 = t0 + Duration::from_millis(230);
        assert_eq!(debounce_delay(t0, t2, debounce, max_wait), Duration::from_millis(20));
        let t3 = t0 + Duration::from_millis(250);
        assert_eq!(debounce_delay(t0, t3, debounce, max_wait), Duration::ZERO);
        assert_eq!(
            debounce_delay(t0, t0 + Duration::from_secs(5), debounce, max_wait),
            Duration::ZERO
        );
    }

    #[test]
    fn extend_keeps_range_open() {
        let t0 = Instant::now();
        let mut s = SeqSuppressor::default();
        let tok = s.open(7, TTL, t0);
        // 恢复超时：区间保持开放并重新计时，迟到的恢复写入仍被忽略
        let t1 = t0 + Duration::from_millis(250);
        s.extend(tok, TTL, t1);
        let t2 = t0 + Duration::from_millis(400);
        assert!(s.should_ignore(9, t2));
        assert!(s.should_ignore(100, t2));
        assert!(!s.should_ignore(9, t1 + TTL));
    }

    #[test]
    fn unbounded_ttl_does_not_overflow() {
        // 上层用 Duration::MAX 表示「一直抑制到撤销」：Instant + Duration 会溢出 panic（发布版 abort），这里不会
        let t0 = Instant::now();
        let mut s = SeqSuppressor::default();
        let tok = s.open(3, Duration::MAX, t0);
        let later = t0 + Duration::from_secs(10 * 24 * 3600);
        assert!(s.should_ignore(4, later));
        s.extend(tok, Duration::MAX, later);
        s.close(tok, 9, Duration::MAX, later);
        assert!(s.should_ignore(9, later + Duration::from_secs(3600)));
        assert!(!s.should_ignore(10, later));
        let a = s.alias_open(20, Duration::MAX, later);
        s.alias_close(a, 20, 22, Duration::MAX, later);
        assert_eq!(s.canonical(22, later + Duration::from_secs(3600)), 20);
        s.cancel(tok);
        assert!(!s.should_ignore(9, later));
    }

    #[test]
    fn entries_are_capped_dropping_the_oldest() {
        let t0 = Instant::now();
        let mut s = SeqSuppressor::default();
        for i in 0..(MAX_ENTRIES as u64 + 4) {
            s.open(i * 10, Duration::MAX, t0);
            let a = s.alias_open(i * 10, Duration::MAX, t0);
            s.alias_close(a, i * 10, i * 10 + 2, Duration::MAX, t0);
        }
        assert_eq!(s.ranges.len(), MAX_ENTRIES);
        assert_eq!(s.aliases.len(), MAX_ENTRIES);
        // 最早的 4 个区间 / 别名（lo = 0、10、20、30）被丢弃
        assert!(!s.should_ignore(1, t0));
        assert!(s.should_ignore(41, t0));
        assert_eq!(s.canonical(32, t0), 32);
        assert_eq!(s.canonical(42, t0), 40);
    }

    #[test]
    fn render_alias_maps_back_to_the_real_change() {
        let t0 = Instant::now();
        let mut s = SeqSuppressor::default();
        // Wisp 打开剪贴板读取（打开时序号 10）：读取期间的任何序号都只可能来自渲染，即使系统在渲染当时就发出通知
        let a = s.alias_open(10, TTL, t0);
        assert_eq!(s.canonical(11, t0), 10);
        assert_eq!(s.canonical(500, t0), 10);
        // 原所有者渲染了两个延迟格式：关闭前收窄为 (10, 12]
        s.alias_close(a, 10, 12, TTL, t0);
        assert_eq!(s.canonical(11, t0), 10);
        assert_eq!(s.canonical(12, t0), 10);
        assert_eq!(s.canonical(10, t0), 10);
        assert_eq!(s.canonical(13, t0), 13, "之后的真实变更不受影响");
        // 随后的快照又渲染了剩余格式：12 → 14，逐级还原到 10
        let b = s.alias_open(12, TTL, t0);
        s.alias_close(b, 12, 14, TTL, t0);
        assert_eq!(s.canonical(14, t0), 10);
        // 序号没变：撤销
        let c = s.alias_open(20, TTL, t0);
        s.alias_close(c, 20, 20, TTL, t0);
        assert_eq!(s.aliases.len(), 2);
        assert_eq!(s.canonical(21, t0), 21);
        // 开放别名在读取结束前已过期：按 (base, hi] 重新登记
        let d = s.alias_open(30, TTL, t0);
        let late = t0 + TTL + Duration::from_millis(1);
        assert_eq!(s.canonical(31, late), 31);
        s.alias_close(d, 30, 31, TTL, late);
        assert_eq!(s.canonical(31, late), 30);
        // 过期
        assert_eq!(s.canonical(12, t0 + TTL), 12);
    }
}
