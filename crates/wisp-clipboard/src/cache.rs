//! 最近 200 条元数据的内存缓存：剪贴板面板首屏不查库。
//!
//! 不变式：缓存始终是数据库按「置顶在前、最近使用在前、id 新的在前」排序后的**前缀**（`complete` 表示前缀就是全部）。
//! 因此任意过滤器下，缓存里匹配的条目恰好是数据库里该过滤器的前 k 条，`page` 返回的 `next_offset` 可以直接交给
//! `ClipStore::recent` 继续翻页，不会重复也不会遗漏。缓存只存元数据（预览、缩略图路径），不存正文和图片字节。
//!
//! 维持不变式的前提：每次改动数据库的 `ClipStore` 调用与对应的缓存更新在同一把锁内串行执行（见 lib.rs 的调用方约定）；
//! 唯一允许滞后到达的是后台缩略图 / 图标的结果，[`RecentCache::update`] 只合并内容字段、从不移动条目。

use std::collections::VecDeque;

use wisp_types::{ClipFilter, ClipHit, ClipItemMeta, ClipPage};

use crate::error::StoreError;
use crate::ingest::IngestOutcome;
use crate::row::filter_matches;
use crate::store::ClipStore;

/// 缓存条数上限
pub(crate) const CACHE_CAP: usize = 200;

/// 最近条目缓存（只存元数据）
#[derive(Debug, Clone, Default)]
pub struct RecentCache {
    items: VecDeque<ClipItemMeta>,
    /// 缓存是否就是数据库的全部条目
    complete: bool,
}

impl RecentCache {
    /// 从数据库加载最近 200 条
    pub fn load(store: &ClipStore) -> Result<Self, StoreError> {
        let page = store.recent(0, CACHE_CAP as u32, ClipFilter::All)?;
        Ok(Self {
            complete: page.next_offset.is_none(),
            items: page.items.into_iter().map(|h| h.item).collect(),
        })
    }

    /// 应用一次记录结果：插入 / touch 的条目放到排序位置上（touch 的条目因此移到顶部），超出上限时丢掉末尾
    pub fn apply(&mut self, outcome: &IngestOutcome) {
        match outcome {
            IngestOutcome::Inserted(m) | IngestOutcome::Touched(m) => self.place(m),
            IngestOutcome::Skipped(_) => {}
        }
    }

    /// 移除一条
    pub fn remove(&mut self, id: i64) {
        self.items.retain(|m| m.id != id);
    }

    /// 按 id 原地更新一条的元数据（后台缩略图生成完成后调用）：不改变顺序；不在缓存里时忽略（例如生成期间已被删除）。
    ///
    /// 只取内容字段（缩略图、来源与图标、预览等）；排序相关的 `pinned` / `last_used_at` / `use_count` 保留缓存里的值：
    /// 后台任务拿到的元数据可能已经过时（生成缩略图期间用户又粘贴或置顶了这一条），不能让旧值把条目挪回原位或回退次数。
    /// 推送 `updated` 时用 [`Self::get`] 取合并后的元数据
    pub fn update(&mut self, meta: &ClipItemMeta) {
        if let Some(slot) = self.items.iter_mut().find(|m| m.id == meta.id) {
            let (pinned, last_used_at, use_count) = (slot.pinned, slot.last_used_at, slot.use_count);
            slot.clone_from(meta);
            slot.pinned = pinned;
            slot.last_used_at = last_used_at;
            slot.use_count = use_count;
        }
    }

    /// 条目的排序键变化后（`set_pinned`、粘贴时的 `touch`）更新缓存：移到新的排序位置；不在缓存里、但落在缓存前缀
    /// 之内时加入（例如把一条旧条目置顶），否则忽略。传入的必须是刚由 `ClipStore` 返回的元数据（与该调用在同一把锁内）
    pub fn upsert(&mut self, meta: &ClipItemMeta) {
        self.place(meta);
    }

    /// 缓存中的一条
    pub fn get(&self, id: i64) -> Option<&ClipItemMeta> {
        self.items.iter().find(|m| m.id == id)
    }

    /// 清空后同步缓存（与 `ClipStore::clear` 对应），不必重新查库
    pub fn clear(&mut self, keep_pinned: bool) {
        if keep_pinned {
            // 缓存若已包含全部置顶条目（完整，或末尾已经是非置顶条目），清空后它就是数据库的全部
            let had_all_pinned = self.complete || self.items.back().is_some_and(|m| !m.pinned);
            self.items.retain(|m| m.pinned);
            self.complete = had_all_pinned;
        } else {
            self.items.clear();
            self.complete = true;
        }
    }

    /// 首页（置顶在前）。`next_offset` 可直接用于 `ClipStore::recent` 翻页；`pinned_count` 为缓存中满足过滤器的置顶条数。
    /// 不访问数据库与文件系统（文件条目的 exists 见 [`crate::refresh_file_exists`]）
    pub fn page(&self, filter: ClipFilter, limit: u32) -> ClipPage {
        let limit = limit as usize;
        let mut matching = 0usize;
        let mut pinned = 0u32;
        let mut items = Vec::with_capacity(limit.min(CACHE_CAP));
        for m in self.items.iter().filter(|m| filter_matches(filter, m.kind)) {
            matching += 1;
            if m.pinned {
                pinned += 1;
            }
            if items.len() < limit {
                items.push(ClipHit {
                    item: m.clone(),
                    highlights: Vec::new(),
                });
            }
        }
        let returned = items.len();
        let next_offset = if matching > returned || !self.complete {
            Some(u32::try_from(returned).unwrap_or(u32::MAX))
        } else {
            None
        };
        ClipPage {
            items,
            pinned_count: pinned,
            next_offset,
            query: None,
            filter,
        }
    }

    /// 缓存条数
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// 缓存是否为空
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    fn place(&mut self, meta: &ClipItemMeta) {
        self.remove(meta.id);
        let idx = self.items.partition_point(|x| comes_before(x, meta));
        if idx == self.items.len() && !self.complete {
            // 落在已知前缀之后：与末尾之间可能还有未缓存的条目
            return;
        }
        self.items.insert(idx, meta.clone());
        if self.items.len() > CACHE_CAP {
            self.items.pop_back();
            self.complete = false;
        }
    }
}

/// 列表顺序：置顶在前，最近使用在前，同一毫秒 id 新的在前（与 `ClipStore::recent` 的 ORDER BY 一致）
fn comes_before(a: &ClipItemMeta, b: &ClipItemMeta) -> bool {
    (a.pinned, a.last_used_at, a.id) > (b.pinned, b.last_used_at, b.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wisp_types::ClipKind;

    fn meta(id: i64, t: i64, pinned: bool, kind: ClipKind) -> ClipItemMeta {
        ClipItemMeta {
            id,
            kind,
            created_at: t,
            last_used_at: t,
            use_count: 1,
            pinned,
            source: None,
            preview: format!("item {id}"),
            char_count: None,
            line_count: None,
            truncated: false,
            image: None,
            files: None,
            link: None,
            color: None,
        }
    }

    fn ids(page: &ClipPage) -> Vec<i64> {
        page.items.iter().map(|h| h.item.id).collect()
    }

    fn complete_cache() -> RecentCache {
        RecentCache {
            items: VecDeque::new(),
            complete: true,
        }
    }

    #[test]
    fn apply_orders_pinned_first_then_recent() {
        let mut c = complete_cache();
        c.apply(&IngestOutcome::Inserted(meta(1, 10, false, ClipKind::Text)));
        c.apply(&IngestOutcome::Inserted(meta(2, 20, true, ClipKind::Text)));
        c.apply(&IngestOutcome::Inserted(meta(3, 30, false, ClipKind::Image)));
        assert_eq!(ids(&c.page(ClipFilter::All, 10)), [2, 3, 1]);
        // touch 把旧条目移到非置顶的顶部
        let mut touched = meta(1, 40, false, ClipKind::Text);
        touched.use_count = 2;
        c.apply(&IngestOutcome::Touched(touched));
        let p = c.page(ClipFilter::All, 10);
        assert_eq!(ids(&p), [2, 1, 3]);
        assert_eq!(p.items[1].item.use_count, 2);
        assert_eq!((p.pinned_count, p.next_offset), (1, None));
        c.apply(&IngestOutcome::Skipped(crate::SkipReason::Secret));
        assert_eq!(c.len(), 3);
    }

    #[test]
    fn page_filters_and_offsets() {
        let mut c = complete_cache();
        for i in 0..10 {
            let kind = if i % 2 == 0 { ClipKind::Image } else { ClipKind::Text };
            c.apply(&IngestOutcome::Inserted(meta(i, i, false, kind)));
        }
        let p = c.page(ClipFilter::Image, 3);
        assert_eq!(ids(&p), [8, 6, 4]);
        assert_eq!(p.next_offset, Some(3));
        let p = c.page(ClipFilter::Image, 10);
        assert_eq!((p.items.len(), p.next_offset), (5, None));
        // 缓存不完整时总能继续翻页
        c.complete = false;
        assert_eq!(c.page(ClipFilter::Image, 10).next_offset, Some(5));
        assert_eq!(c.page(ClipFilter::Color, 10).next_offset, Some(0));
    }

    #[test]
    fn cap_and_prefix_invariant() {
        let mut c = complete_cache();
        for i in 0..(CACHE_CAP as i64 + 5) {
            c.apply(&IngestOutcome::Inserted(meta(i, i, false, ClipKind::Text)));
        }
        assert_eq!(c.len(), CACHE_CAP);
        assert!(!c.complete);
        // 比缓存末尾还旧的条目（例如取消置顶后落到后面）不加入
        c.upsert(&meta(1, 1, false, ClipKind::Text));
        assert!(!c.items.iter().any(|m| m.id == 1));
        // 不在缓存里的条目：update 一律忽略
        c.update(&meta(1, 1, true, ClipKind::Text));
        assert!(!c.items.iter().any(|m| m.id == 1));
        // 旧条目被置顶 → 进入前缀顶部
        c.upsert(&meta(1, 1, true, ClipKind::Text));
        assert_eq!(c.items[0].id, 1);
        assert_eq!(c.len(), CACHE_CAP);
    }

    #[test]
    fn update_in_place_keeps_order() {
        let mut c = complete_cache();
        c.apply(&IngestOutcome::Inserted(meta(1, 10, false, ClipKind::Image)));
        c.apply(&IngestOutcome::Inserted(meta(2, 20, false, ClipKind::Image)));
        let mut m = meta(1, 10, false, ClipKind::Image);
        m.preview = "with thumb".into();
        c.update(&m);
        assert_eq!(ids(&c.page(ClipFilter::All, 10)), [2, 1]);
        assert_eq!(c.items[1].preview, "with thumb");
        // update 从不移动条目；置顶走 upsert
        c.update(&meta(1, 10, true, ClipKind::Image));
        assert_eq!(ids(&c.page(ClipFilter::All, 10)), [2, 1]);
        assert!(!c.items[1].pinned);
        c.upsert(&meta(1, 10, true, ClipKind::Image));
        assert_eq!(ids(&c.page(ClipFilter::All, 10)), [1, 2]);
        c.remove(1);
        assert_eq!(ids(&c.page(ClipFilter::All, 10)), [2]);
    }

    #[test]
    fn stale_background_update_cannot_move_or_resurrect() {
        let mut c = complete_cache();
        c.apply(&IngestOutcome::Inserted(meta(1, 10, false, ClipKind::Image)));
        c.apply(&IngestOutcome::Inserted(meta(2, 20, false, ClipKind::Text)));
        // 缩略图线程拿到 T0 的元数据期间，用户粘贴了条目 1（T1 = 30，次数 2）
        let mut touched = meta(1, 30, false, ClipKind::Image);
        touched.use_count = 2;
        c.upsert(&touched);
        assert_eq!(ids(&c.page(ClipFilter::All, 10)), [1, 2]);
        let mut stale = meta(1, 10, false, ClipKind::Image);
        stale.preview = "with thumb".into();
        c.update(&stale);
        assert_eq!(
            ids(&c.page(ClipFilter::All, 10)),
            [1, 2],
            "过时的元数据不能把条目挪回原位"
        );
        let merged = c.get(1).expect("在缓存里");
        assert_eq!(
            (merged.last_used_at, merged.use_count, merged.preview.as_str()),
            (30, 2, "with thumb")
        );
        // 生成期间被删除：update 不会把它插回来
        c.remove(2);
        c.update(&meta(2, 20, false, ClipKind::Text));
        assert!(c.get(2).is_none());
    }

    #[test]
    fn clear_variants() {
        let mut c = complete_cache();
        c.apply(&IngestOutcome::Inserted(meta(1, 10, true, ClipKind::Text)));
        c.apply(&IngestOutcome::Inserted(meta(2, 20, false, ClipKind::Text)));
        c.complete = false;
        c.clear(true);
        assert_eq!(ids(&c.page(ClipFilter::All, 10)), [1]);
        assert!(c.complete, "末尾是非置顶条目时，缓存已包含全部置顶条目");
        c.clear(false);
        assert!(c.is_empty() && c.complete);
    }
}
