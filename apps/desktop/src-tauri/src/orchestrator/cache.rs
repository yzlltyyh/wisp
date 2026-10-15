//! 翻译结果 LRU 缓存：200 条、10 分钟、只在内存中。
//!
//! - 键由 `wisp_prompts::cache_key` 计算（provider、model、风格 id + 模板与参数的哈希、dict 标记、源语言、
//!   目标语言、术语表版本、规范化后的原文）；polish / proofread 以动作 id 作为风格参与缓存。
//! - ⌘R（`bypass_cache`）绕过查询，结果照常写入并覆盖旧值；refine 不缓存（结果依赖译稿，`CacheKeyInput` 没有译稿维度）；
//!   OCR 不缓存。
//! - 只缓存正常结束、未被截断、有文本的结果；过期条目在下一次查询时惰性清除——**没有任何定时器**（空闲零唤醒）。
//! - 只存经后处理的完整输出：命中时 `Done.usage` 为 null（这次没有消耗 token），模型已在键里，不需要另存。

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lru::LruCache;
use parking_lot::Mutex;

/// 条数上限
pub const CAPACITY: usize = 200;
/// 有效期
pub const TTL: Duration = Duration::from_secs(10 * 60);

/// 翻译结果缓存：键 → 经后处理的完整输出
pub struct TranslationCache {
    inner: Mutex<LruCache<u128, (Instant, Arc<str>)>>,
    ttl: Duration,
}

impl TranslationCache {
    /// 默认容量与有效期
    pub fn new() -> Self {
        Self::with_limits(CAPACITY, TTL)
    }

    /// 自定义容量与有效期（测试用）
    pub fn with_limits(capacity: usize, ttl: Duration) -> Self {
        let cap = NonZeroUsize::new(capacity.max(1)).unwrap_or(NonZeroUsize::MIN);
        Self {
            inner: Mutex::new(LruCache::new(cap)),
            ttl,
        }
    }

    /// 查询；命中时刷新最近使用（不延长有效期），过期则移除
    pub fn get(&self, key: u128, now: Instant) -> Option<Arc<str>> {
        let mut g = self.inner.lock();
        let fresh = match g.get(&key) {
            Some((at, v)) if now.saturating_duration_since(*at) < self.ttl => Some(Arc::clone(v)),
            Some(_) => None,
            None => return None,
        };
        if fresh.is_none() {
            g.pop(&key);
        }
        fresh
    }

    /// 写入（覆盖同键的旧值；满了淘汰最久未用的）
    pub fn put(&self, key: u128, text: String, now: Instant) {
        self.inner.lock().put(key, (now, Arc::from(text)));
    }

    /// 当前条数（含尚未清除的过期条目；测试用）
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.lock().len()
    }
}

impl Default for TranslationCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(text: &str) -> String {
        text.to_owned()
    }

    #[test]
    fn hit_within_ttl_miss_after() {
        let c = TranslationCache::new();
        let t0 = Instant::now();
        c.put(1, value("你好"), t0);
        assert_eq!(
            c.get(1, t0 + Duration::from_secs(599)).map(|v| v.to_string()),
            Some("你好".into())
        );
        assert!(c.get(1, t0 + TTL).is_none(), "10 分钟后重新请求");
        assert_eq!(c.len(), 0, "过期条目在查询时清除");
    }

    #[test]
    fn lru_evicts_the_least_recently_used_201st() {
        let c = TranslationCache::new();
        let t0 = Instant::now();
        for k in 0..CAPACITY as u128 {
            c.put(k, value("x"), t0);
        }
        // 用一下第 0 条：它变成最近使用的
        assert!(c.get(0, t0).is_some());
        c.put(10_000, value("new"), t0);
        assert_eq!(c.len(), CAPACITY);
        assert!(c.get(0, t0).is_some(), "最近用过的保留");
        assert!(c.get(1, t0).is_none(), "第 201 条淘汰最久未用的");
        assert!(c.get(10_000, t0).is_some());
    }

    #[test]
    fn bypass_overwrites() {
        let c = TranslationCache::new();
        let t0 = Instant::now();
        c.put(7, value("旧"), t0);
        c.put(7, value("新"), t0 + Duration::from_secs(1));
        assert_eq!(
            c.get(7, t0 + Duration::from_secs(2)).map(|v| v.to_string()),
            Some("新".into())
        );
    }
}
