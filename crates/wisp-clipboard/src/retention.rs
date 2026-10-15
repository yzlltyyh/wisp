//! 保留策略与清理。
//!
//! 执行时机由调用方决定（desktop：面板隐藏且 30 s 无变更后一次性调度）：依次调用 `ClipStore::retain`、
//! `ClipStore::sweep_orphan_blobs`、`ClipStore::vacuum_if_due`，之后用 `RecentCache::load` 重新加载缓存。

use wisp_types::ClipboardSettings;

/// 保留策略
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// 不含置顶
    pub max_items: u32,
    /// None = 不限
    pub max_age_days: Option<u32>,
}

impl From<&ClipboardSettings> for RetentionPolicy {
    fn from(s: &ClipboardSettings) -> Self {
        Self {
            max_items: s.max_items,
            max_age_days: s.max_age_days,
        }
    }
}

/// 清理结果
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RetentionReport {
    /// 删除的条目数
    pub removed_items: u32,
    /// 随之删除的 blob 数
    pub removed_blobs: u32,
}

/// 一天的毫秒数
pub(crate) const DAY_MS: i64 = 86_400_000;
/// VACUUM 的最小间隔
pub(crate) const VACUUM_INTERVAL_MS: i64 = 30 * DAY_MS;
/// freelist 达到这个字节数也触发 VACUUM
pub(crate) const VACUUM_FREE_BYTES: i64 = 4 * 1024 * 1024;

/// 早于这个时间（last_used_at）的非置顶条目过期；不限时返回 None
pub(crate) fn age_cutoff(policy: &RetentionPolicy, now_ms: i64) -> Option<i64> {
    policy
        .max_age_days
        .map(|d| now_ms.saturating_sub(i64::from(d).saturating_mul(DAY_MS)))
}

/// VACUUM 条件：距上次 ≥ 30 天，且 freelist ≥ 总页数 10% 或 ≥ 4 MB
pub(crate) fn vacuum_due(
    now_ms: i64,
    last_vacuum_ms: i64,
    freelist_pages: i64,
    page_count: i64,
    page_size: i64,
) -> bool {
    if now_ms.saturating_sub(last_vacuum_ms) < VACUUM_INTERVAL_MS || freelist_pages <= 0 {
        return false;
    }
    freelist_pages.saturating_mul(10) >= page_count || freelist_pages.saturating_mul(page_size) >= VACUUM_FREE_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_760_000_000_000;

    #[test]
    fn vacuum_conditions() {
        let old = NOW - VACUUM_INTERVAL_MS;
        // 时间够、freelist 占 10%
        assert!(vacuum_due(NOW, old, 10, 100, 4096));
        // 时间够、freelist 不到 10% 但 ≥ 4 MB
        assert!(vacuum_due(NOW, old, 1024, 100_000, 4096));
        // 时间够、freelist 太小
        assert!(!vacuum_due(NOW, old, 9, 100, 4096));
        assert!(!vacuum_due(NOW, old, 0, 100, 4096));
        // freelist 够、时间不到 30 天
        assert!(!vacuum_due(NOW, old + 1, 50, 100, 4096));
        // 从未执行过（last = 0）
        assert!(vacuum_due(NOW, 0, 50, 100, 4096));
    }

    #[test]
    fn cutoffs() {
        let p = RetentionPolicy {
            max_items: 10,
            max_age_days: Some(7),
        };
        assert_eq!(age_cutoff(&p, NOW), Some(NOW - 7 * DAY_MS));
        let p = RetentionPolicy {
            max_items: 10,
            max_age_days: None,
        };
        assert_eq!(age_cutoff(&p, NOW), None);
    }
}
