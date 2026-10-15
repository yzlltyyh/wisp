//! 内存缓存：首屏来自缓存且与数据库一致；`next_offset` 可直接交给 `recent` 翻页；只存元数据。

mod common;

use common::{Fixture, NOW, expect_inserted, image_raw, png};
use wisp_clipboard::RecentCache;
use wisp_types::{ClipFilter, ClipHit};

fn ids(items: &[ClipHit]) -> Vec<i64> {
    items.iter().map(|h| h.item.id).collect()
}

/// 用缓存首屏 + recent 翻页拼出完整列表，应与直接查库完全一致
fn assert_paging_matches_db(f: &Fixture, cache: &RecentCache, filter: ClipFilter, first: u32, page: u32) {
    let mut all = Vec::new();
    let p = cache.page(filter, first);
    all.extend(ids(&p.items));
    let mut next = p.next_offset;
    while let Some(off) = next {
        let p = f.store.recent(off, page, filter).expect("翻页");
        all.extend(ids(&p.items));
        next = p.next_offset;
    }
    let direct = f.store.recent(0, 500, filter).expect("全部");
    assert_eq!(all, ids(&direct.items), "过滤器 {filter:?}");
}

#[test]
fn cache_page_is_prefix_of_database_order() {
    let f = Fixture::new();
    for i in 0..260 {
        if i % 10 == 0 {
            expect_inserted(f.ingest(&image_raw(png(4, 4, false, move |_, _| [i as u8, 0, 0, 255])), NOW + i));
        } else if i % 7 == 0 {
            f.text(&format!("https://example.com/{i}"), NOW + i);
        } else {
            f.text(&format!("entry number {i}"), NOW + i);
        }
    }
    let old = f.text("entry number 3", NOW + 1000); // touch：旧条目回到顶部
    f.store.set_pinned(old.id, true).expect("置顶");

    let cache = RecentCache::load(&f.store).expect("加载");
    assert_eq!(cache.len(), 200);
    let first = cache.page(ClipFilter::All, 50);
    assert_eq!(first.items[0].item.id, old.id);
    assert_eq!(first.pinned_count, 1);
    assert_eq!(first.next_offset, Some(50));
    // 缓存首屏与直接查库完全一致（元数据逐字段相同）
    let db = f.store.recent(0, 50, ClipFilter::All).expect("查库");
    assert_eq!(first.items, db.items);

    for filter in [
        ClipFilter::All,
        ClipFilter::Image,
        ClipFilter::Link,
        ClipFilter::Text,
        ClipFilter::Color,
    ] {
        assert_paging_matches_db(&f, &cache, filter, 30, 25);
    }
}

#[test]
fn apply_keeps_cache_in_sync_with_store() {
    let f = Fixture::new();
    for i in 0..5 {
        f.text(&format!("item {i}"), NOW + i);
    }
    let mut cache = RecentCache::load(&f.store).expect("加载");
    let o = f.ingest(&common::raw_text("item 1"), NOW + 10);
    cache.apply(&o);
    let o = f.ingest(&common::raw_text("brand new"), NOW + 11);
    cache.apply(&o);
    let m = f
        .store
        .set_pinned(cache.page(ClipFilter::All, 10).items[4].item.id, true)
        .expect("置顶");
    cache.upsert(&m);
    let removed = cache.page(ClipFilter::All, 10).items[3].item.id;
    f.store.delete(removed).expect("删除");
    cache.remove(removed);

    let db = f.store.recent(0, 10, ClipFilter::All).expect("查库");
    let cached = cache.page(ClipFilter::All, 10);
    assert_eq!(cached.items, db.items);
    assert_eq!(cached.pinned_count, db.pinned_count);
    assert_eq!(cached.next_offset, None);

    f.store.clear(true).expect("清空");
    cache.clear(true);
    assert_eq!(
        cache.page(ClipFilter::All, 10).items,
        f.store.recent(0, 10, ClipFilter::All).expect("查库").items
    );
}

#[test]
fn cache_holds_metadata_only() {
    let f = Fixture::new();
    let long = "长文本 ".repeat(100_000);
    f.text(&long, NOW);
    expect_inserted(f.ingest(&image_raw(common::screenshot_like(800, 600, 3)), NOW + 1));
    let cache = RecentCache::load(&f.store).expect("加载");
    let page = cache.page(ClipFilter::All, 10);
    let size: usize = page
        .items
        .iter()
        .map(|h| serde_json::to_string(&h.item).expect("序列化").len())
        .sum();
    assert!(size < 4096, "缓存条目应只有元数据，实际 {size} 字节");
    assert!(page.items.iter().all(|h| h.item.preview.chars().count() <= 200));
}

#[test]
fn late_thumbnail_does_not_break_the_prefix_invariant() {
    // 缩略图在后台生成；拿到结果之前用户粘贴（touch）了这张图，又删掉了另一张还在生成缩略图的图
    let f = Fixture::new();
    let a = expect_inserted(f.ingest(&image_raw(png(40, 30, false, |x, _| [x as u8, 1, 2, 255])), NOW));
    let b = expect_inserted(f.ingest(&image_raw(png(40, 30, false, |x, _| [x as u8, 9, 9, 255])), NOW + 1));
    for i in 0..5 {
        f.text(&format!("text {i}"), NOW + 10 + i);
    }
    let mut cache = RecentCache::load(&f.store).expect("加载");

    let thumb_a = f.store.make_thumb(a.id).expect("缩略图").expect("生成");
    let thumb_b = f.store.make_thumb(b.id).expect("缩略图").expect("生成");
    let touched = f.store.touch(a.id, NOW + 100).expect("粘贴");
    cache.upsert(&touched);
    f.store.delete(b.id).expect("删除");
    cache.remove(b.id);

    // 过时的结果晚到
    cache.update(&thumb_a);
    cache.update(&thumb_b);

    let db = f.store.recent(0, 50, ClipFilter::All).expect("查库");
    assert_eq!(cache.page(ClipFilter::All, 50).items, db.items);
    let merged = cache.get(a.id).expect("在缓存里");
    assert_eq!(merged.use_count, 2);
    assert!(merged.image.as_ref().and_then(|i| i.thumb_url.as_ref()).is_some());
    assert!(cache.get(b.id).is_none(), "已删除的条目不能被插回来");
}
