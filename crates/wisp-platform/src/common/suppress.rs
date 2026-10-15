//! 抑制窗口：忽略 Wisp 自身造成的剪贴板变更。
//!
//! 模拟复制回退会产生两次变更（目标应用复制一次、Wisp 恢复一次）：调用方在发复制键之前以「当前序号 + 1」为起点
//! 布防，ttl 取「复制等待上限 + 150 ms」（或 300 ms）；恢复产生的那一次由平台层直接把「已见序号」推进到恢复后的值
//! （macOS），或把区间上界延伸到恢复后的序号（Windows）。抑制与托盘的「暂停」机制相互独立。
//!
//! 同一时刻可以有多个区间同时生效（例如取词回退进行中，剪贴板服务又为自己的写入布防）：新区间不覆盖旧区间，
//! 每个区间按自己的 ttl 到期，任一区间命中即忽略。

use std::time::{Duration, Instant};

use crate::types::SuppressRange;

/// 默认 ttl
pub const DEFAULT_TTL: Duration = Duration::from_millis(300);

/// 同时保留的区间上限（正常情况下最多两三个；超出时丢弃最早布防的那个，调用方异常时也不会无界增长）
const MAX_RANGES: usize = 16;

/// 抑制器（各平台的剪贴板监听共用）
#[derive(Debug, Default)]
pub struct Suppressor {
    /// (区间, 布防时刻)，按布防先后排列
    active: Vec<(SuppressRange, Instant)>,
}

fn alive(range: &SuppressRange, armed_at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(armed_at) <= range.ttl
}

impl Suppressor {
    /// 新建（未激活）
    pub fn new() -> Self {
        Self::default()
    }

    /// 激活一个抑制区间：与已有的区间并存，并顺便清掉已过期的区间
    pub fn arm(&mut self, range: SuppressRange, now: Instant) {
        self.active.retain(|(r, at)| alive(r, *at, now));
        if self.active.len() >= MAX_RANGES {
            self.active.remove(0);
        }
        self.active.push((range, now));
    }

    /// 某个变更是否应被忽略：序号不小于某个未过期区间的起点（只读判断，过期区间在下一次 `arm` 时清理）
    pub fn should_ignore(&self, change_id: u64, now: Instant) -> bool {
        self.active
            .iter()
            .any(|(r, at)| change_id >= r.from_change_id && alive(r, *at, now))
    }

    /// 撤销某一个区间（最近一次以相同参数布防的那个），返回是否找到。
    /// 布防方确定不会再有变更时使用（例如回退流程在发键之前就失败），不影响其他调用方的区间
    pub fn retract(&mut self, range: SuppressRange) -> bool {
        match self.active.iter().rposition(|(r, _)| *r == range) {
            Some(i) => {
                self.active.remove(i);
                true
            }
            None => false,
        }
    }

    /// 撤销全部区间
    pub fn disarm(&mut self) {
        self.active.clear();
    }

    /// 当前是否有未过期的区间
    pub fn is_armed(&self, now: Instant) -> bool {
        self.active.iter().any(|(r, at)| alive(r, *at, now))
    }
}
