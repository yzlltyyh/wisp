//! 保留策略：条数 / 时长上限；孤儿 blob 清理；VACUUM 条件；
//! 清空时连同 blob 删除。

mod common;

use std::time::{Duration, SystemTime};

use common::{Fixture, NOW, expect_inserted, image_raw, png};
use wisp_clipboard::{BlobKind, RetentionPolicy, RetentionReport};
use wisp_types::ClipFilter;

const DAY: i64 = 86_400_000;

fn image(f: &Fixture, seed: u8, now: i64) -> i64 {
    let bytes = png(16, 16, false, move |x, y| [seed, x as u8, y as u8, 255]);
    let m = expect_inserted(f.ingest(&image_raw(bytes), now));
    f.store.make_thumb(m.id).expect("缩略图").expect("生成");
    m.id
}

#[test]
fn max_items_removes_oldest_unpinned_with_blobs() {
    // 把上限改为 10 后，多余条目与其 blob 都被删掉
    let f = Fixture::new();
    let mut image_ids = Vec::new();
    for i in 0..10 {
        image_ids.push(image(&f, i as u8, NOW + i));
    }
    for i in 0..10 {
        f.text(&format!("text item {i}"), NOW + 100 + i);
    }
    let pinned = f.text("pinned forever", NOW - 10 * DAY);
    f.store.set_pinned(pinned.id, true).expect("置顶");
    assert_eq!(f.blob_files(BlobKind::Img).len(), 10);
    assert_eq!(f.blob_files(BlobKind::Thumb).len(), 10);

    let report = f
        .store
        .retain(
            &RetentionPolicy {
                max_items: 10,
                max_age_days: None,
            },
            NOW + 1000,
        )
        .expect("清理");
    assert_eq!(
        report,
        RetentionReport {
            removed_items: 10,
            removed_blobs: 20
        }
    );
    // 剩下：置顶 1 条（不计入上限）+ 最近的 10 条文本
    let page = f.store.recent(0, 100, ClipFilter::All).expect("列表");
    assert_eq!(page.items.len(), 11);
    assert_eq!(page.pinned_count, 1);
    assert!(page.items[0].item.pinned);
    assert!(f.blob_files(BlobKind::Img).is_empty());
    assert!(f.blob_files(BlobKind::Thumb).is_empty());
    for id in image_ids {
        assert!(f.store.get_detail(id).expect("详情").is_none());
    }
    // FTS 与 items 一致
    f.side_conn()
        .execute(
            "INSERT INTO items_fts(items_fts, rank) VALUES ('integrity-check', 1)",
            [],
        )
        .expect("integrity-check");

    // 再执行一次没有可删的
    let again = f
        .store
        .retain(
            &RetentionPolicy {
                max_items: 10,
                max_age_days: None,
            },
            NOW + 2000,
        )
        .expect("清理");
    assert_eq!(again, RetentionReport::default());
}

#[test]
fn max_age_uses_last_used_and_spares_pinned() {
    let f = Fixture::new();
    let old = f.text("old", NOW - 40 * DAY);
    let revived = f.text("revived", NOW - 40 * DAY);
    f.store.touch(revived.id, NOW - DAY).expect("touch");
    let pinned_old = f.text("pinned old", NOW - 400 * DAY);
    f.store.set_pinned(pinned_old.id, true).expect("置顶");
    let fresh = f.text("fresh", NOW);

    let report = f
        .store
        .retain(
            &RetentionPolicy {
                max_items: 1000,
                max_age_days: Some(30),
            },
            NOW,
        )
        .expect("清理");
    assert_eq!(report.removed_items, 1);
    assert!(f.store.get_detail(old.id).expect("详情").is_none());
    for id in [revived.id, pinned_old.id, fresh.id] {
        assert!(f.store.get_detail(id).expect("详情").is_some());
    }
}

#[test]
fn orphan_blobs_are_swept() {
    let f = Fixture::new();
    let id = image(&f, 7, NOW);
    let key = f
        .store
        .put_icon("com.example.app", &png(8, 8, true, |_, _| [9, 9, 9, 255]))
        .expect("图标");
    let mut raw = common::raw_text("from app");
    raw.source = Some(common::app("com.example.app", "App"));
    f.ingest(&raw, NOW + 1);

    // 人为放入孤儿文件与崩溃遗留的临时文件
    let root = f.dir.path().join("blobs");
    std::fs::write(root.join("img").join("deadbeef.png"), b"orphan").expect("写");
    std::fs::write(root.join("thumb").join("deadbeef.jpg"), b"orphan").expect("写");
    std::fs::write(root.join("icons").join("0000.png"), b"orphan").expect("写");
    std::fs::write(root.join("ocr").join("x.png"), b"orphan").expect("写");
    // 崩溃遗留、超过保留期的临时文件被删；刚写的（可能正在锁外写入）保留
    let stale = std::fs::File::create(root.join("img").join(".tmp-1-0-abc.png")).expect("写");
    stale
        .set_modified(SystemTime::now() - Duration::from_secs(11 * 60))
        .expect("改时间");
    drop(stale);
    std::fs::write(root.join("img").join(".tmp-1-1-in-flight.png"), b"partial").expect("写");

    assert_eq!(f.store.sweep_orphan_blobs().expect("清理"), 5);
    assert_eq!(
        f.blob_files(BlobKind::Img)
            .iter()
            .filter(|n| !n.starts_with(".tmp-"))
            .count(),
        1
    );
    assert_eq!(
        f.blob_files(BlobKind::Img)
            .iter()
            .filter(|n| n.starts_with(".tmp-"))
            .collect::<Vec<_>>(),
        [".tmp-1-1-in-flight.png"]
    );
    assert_eq!(f.blob_files(BlobKind::Thumb).len(), 1);
    assert_eq!(f.blob_files(BlobKind::Icon), [format!("{key}.png")]);
    assert!(f.blob_files(BlobKind::Ocr).is_empty());
    // 被引用的仍可用
    let d = f.store.get_detail(id).expect("详情").expect("存在");
    assert!(d.meta.image.expect("图片").thumb_url.is_some());
    assert_eq!(f.store.sweep_orphan_blobs().expect("再次清理"), 0);

    // 应用的条目都删除后，图标成为孤儿
    f.store.clear(false).expect("清空");
    assert!(f.blob_files(BlobKind::Icon).is_empty());
    assert!(f.store.icon_key("com.example.app").is_none(), "图标被清理后应重新获取");
}

#[test]
fn clear_keeps_pinned_when_asked_and_deletes_blobs() {
    // 选择保留置顶时置顶项还在，其余被删，blobs 同步清理
    let f = Fixture::new();
    let pinned_img = image(&f, 1, NOW);
    f.store.set_pinned(pinned_img, true).expect("置顶");
    let other_img = image(&f, 2, NOW + 1);
    let pinned_text = f.text("keep me", NOW + 2);
    f.store.set_pinned(pinned_text.id, true).expect("置顶");
    f.text("drop me", NOW + 3);

    assert_eq!(f.store.clear(true).expect("清空"), 2);
    let page = f.store.recent(0, 10, ClipFilter::All).expect("列表");
    let ids: Vec<i64> = page.items.iter().map(|h| h.item.id).collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&pinned_img) && ids.contains(&pinned_text.id));
    assert!(f.store.get_detail(other_img).expect("详情").is_none());
    assert_eq!(f.blob_files(BlobKind::Img).len(), 1);
    assert_eq!(f.blob_files(BlobKind::Thumb).len(), 1);
    assert!(
        f.store
            .search("drop", ClipFilter::All, 10)
            .expect("搜索")
            .items
            .is_empty()
    );

    assert_eq!(f.store.clear(false).expect("全部清空"), 2);
    assert!(f.store.recent(0, 10, ClipFilter::All).expect("列表").items.is_empty());
    assert!(f.blob_files(BlobKind::Img).is_empty());
    assert!(f.blob_files(BlobKind::Thumb).is_empty());
    assert_eq!(f.count("SELECT count(*) FROM items_fts_docsize"), 0);
    // WAL 已截断，不留旧页
    let wal = f.dir.path().join("clipboard.db-wal");
    assert!(!wal.exists() || std::fs::metadata(&wal).expect("wal").len() == 0);
}

/// FTS 索引数据的总字节数
fn fts_bytes(f: &Fixture) -> i64 {
    f.count("SELECT coalesce(sum(length(block)), 0) FROM items_fts_data")
}

/// 批量删除后：索引与内容一致、FTS secure-delete 仍打开、删除触发器仍在
fn assert_fts_settled(f: &Fixture) {
    f.side_conn()
        .execute(
            "INSERT INTO items_fts(items_fts, rank) VALUES ('integrity-check', 1)",
            [],
        )
        .expect("integrity-check");
    assert_eq!(f.count("SELECT v FROM items_fts_config WHERE k = 'secure-delete'"), 1);
    assert_eq!(
        f.count("SELECT count(*) FROM sqlite_schema WHERE type = 'trigger' AND name = 'items_ad'"),
        1
    );
}

#[test]
fn bulk_deletes_leave_no_stale_fts_data() {
    // 清空与大批量保留策略不逐行抹 FTS 条目（那样 1000 条要数秒），而是整体重建 / 合并；被删内容的索引条目同样不留
    let f = Fixture::new();
    for i in 0..300u64 {
        let (text, html) = common::corpus::clip_text(i);
        let raw = wisp_types::RawClipboard {
            text: Some(format!("zqmark{i:03}x {text}")),
            html,
            ..wisp_types::RawClipboard::default()
        };
        common::expect_meta(f.ingest(&raw, NOW + i as i64));
    }
    let pinned = f.text("pinned zqpinnedx survivor", NOW - DAY);
    f.store.set_pinned(pinned.id, true).expect("置顶");

    // 上限从 1000 改为 10
    let report = f
        .store
        .retain(
            &RetentionPolicy {
                max_items: 10,
                max_age_days: None,
            },
            NOW + 1000,
        )
        .expect("清理");
    assert_eq!(report.removed_items, 290);
    assert_fts_settled(&f);
    let after = fts_bytes(&f);
    f.side_conn()
        .execute("INSERT INTO items_fts(items_fts) VALUES ('rebuild')", [])
        .expect("重建");
    let fresh = fts_bytes(&f);
    assert!(after * 100 <= fresh * 115 + 8192, "残留旧索引：{after} vs 新建 {fresh}");
    assert!(
        f.store
            .search("zqmark100x", ClipFilter::All, 10)
            .expect("搜索")
            .items
            .is_empty()
    );
    assert_eq!(
        f.store
            .search("zqmark295x", ClipFilter::All, 10)
            .expect("搜索")
            .items
            .len(),
        1
    );

    // 全部清空：索引里只剩结构记录
    assert_eq!(f.store.clear(false).expect("清空"), 11);
    assert_fts_settled(&f);
    assert_eq!(f.count("SELECT count(*) FROM items_fts_docsize"), 0);
    assert!(fts_bytes(&f) < 256, "清空后索引应为空：{} 字节", fts_bytes(&f));
    assert!(
        f.store
            .search("zqpinnedx", ClipFilter::All, 10)
            .expect("搜索")
            .items
            .is_empty()
    );
    // 之后照常记录与检索
    f.text("after clear zqnewx", NOW + 2000);
    assert_eq!(
        f.store.search("zqnewx", ClipFilter::All, 10).expect("搜索").items.len(),
        1
    );
}

#[test]
fn clearing_most_content_shrinks_the_database_file() {
    let f = Fixture::new();
    let filler = "长文本 long text ".repeat(2000);
    for i in 0..60 {
        f.text(&format!("{i} {filler}"), NOW + i);
    }
    let keep = f.text("pinned survivor", NOW + 100);
    f.store.set_pinned(keep.id, true).expect("置顶");
    f.side_conn()
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .expect("检查点");
    let before = std::fs::metadata(f.db_path()).expect("库").len();
    assert_eq!(f.store.clear(true).expect("清空"), 60);
    let after = std::fs::metadata(f.db_path()).expect("库").len();
    assert!(after * 4 < before, "清空后应顺带 VACUUM：{before} → {after}");
    assert_eq!(f.count("PRAGMA freelist_count"), 0);
    assert_eq!(
        f.store
            .search("survivor", ClipFilter::All, 10)
            .expect("搜索")
            .items
            .len(),
        1
    );
}

#[test]
fn delete_removes_blobs() {
    let f = Fixture::new();
    let id = image(&f, 3, NOW);
    assert!(f.store.delete(id).expect("删除"));
    assert!(f.blob_files(BlobKind::Img).is_empty());
    assert!(f.blob_files(BlobKind::Thumb).is_empty());
    assert!(!f.store.delete(id).expect("再删"));
}

#[test]
fn vacuum_runs_only_when_due() {
    let f = Fixture::new();
    // 写入后按保留策略删掉大部分，制造大量空闲页（clear 会顺带 VACUUM，这里不用它）
    let filler = "填充内容 filler content ".repeat(400);
    for i in 0..200 {
        f.text(&format!("{i} {filler}"), NOW + i);
    }
    f.store
        .retain(
            &RetentionPolicy {
                max_items: 1,
                max_age_days: None,
            },
            NOW + 1000,
        )
        .expect("清理");
    let pages_before = f.count("PRAGMA page_count");
    let free_before = f.count("PRAGMA freelist_count");
    assert!(
        free_before * 10 >= pages_before,
        "应有足够的空闲页：{free_before}/{pages_before}"
    );

    // 距上次不足 30 天：跳过
    assert!(!f.store.vacuum_if_due(NOW, NOW - 29 * DAY).expect("判断"));
    // 满足条件：执行
    assert!(f.store.vacuum_if_due(NOW, NOW - 30 * DAY).expect("执行"));
    let pages_after = f.count("PRAGMA page_count");
    assert!(
        pages_after < pages_before,
        "VACUUM 后页数应减少：{pages_before} → {pages_after}"
    );
    assert_eq!(f.count("PRAGMA freelist_count"), 0);
    // 没有空闲页：即使时间满足也跳过
    assert!(!f.store.vacuum_if_due(NOW + 60 * DAY, NOW).expect("判断"));
    // 库仍然可用
    f.text("after vacuum", NOW + 1);
    assert_eq!(
        f.store.search("vacuum", ClipFilter::All, 10).expect("搜索").items.len(),
        1
    );
}
