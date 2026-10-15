//! 多设备同步的存储接口：导出、远端条目的检查 / 落库 / 合并、按 hash 置顶与删除、远端清空；识别历史的同步键、导出、
//! 落库去重与按键删除。两台「设备」各用一个临时库，导出 → 应用 → 再导出应当一致。

mod common;

use std::path::{Path, PathBuf};

use common::{Fixture, NOW, app, expect_inserted, image_raw, png, policy, raw_text};
use wisp_clipboard::{
    BlobKind, ClipStore, IngestPolicy, MAX_REMOTE_BATCH, OcrApplied, OcrHistoryNew, RejectReason, Rejected,
    RemoteApplied, RemoteItem, RemoteOcr, RetentionPolicy, SkipReason, StoreError, SyncItem, SyncSource, hash_hex,
    ocr_key, parse_hash_hex,
};
use wisp_types::{ClipFilter, ClipKind, OcrMode, RawClipboard};

const DAY: i64 = 86_400_000;

fn keep_all() -> RetentionPolicy {
    RetentionPolicy {
        max_items: 1000,
        max_age_days: None,
    }
}

/// 一条文本远端条目（hash 由「另一台设备」按同样的规范化算出）
fn remote_text(text: &str, created_at: i64, last_used_at: i64, pinned: Option<bool>) -> RemoteItem {
    let other = Fixture::new();
    let m = other.text(text, created_at);
    let hash = other.store.hash_of(m.id).expect("hash").expect("存在");
    RemoteItem {
        hash,
        kind: m.kind,
        created_at,
        last_used_at,
        pinned,
        text: Some(text.to_owned()),
        html: None,
        rtf: None,
        image_path: None,
        source: None,
        truncated: false,
    }
}

/// 把一条导出的条目当作远端条目（图片的原图路径就是导出方的 blob 文件）
fn as_remote(item: &SyncItem) -> RemoteItem {
    RemoteItem {
        hash: item.hash,
        kind: item.kind,
        created_at: item.created_at,
        last_used_at: item.last_used_at,
        pinned: Some(item.pinned),
        text: item.text.clone(),
        html: item.html.clone(),
        rtf: item.rtf.clone(),
        image_path: item.image.as_ref().map(|i| i.path.clone()),
        source: item.source.clone(),
        truncated: item.truncated,
    }
}

fn prepare_ok(store: &ClipStore, item: RemoteItem) -> wisp_clipboard::PreparedRemote {
    store
        .prepare_remote(item, &policy(), &keep_all(), NOW)
        .expect("应当接收")
}

fn reject_reason(store: &ClipStore, item: RemoteItem, p: &IngestPolicy, r: &RetentionPolicy) -> RejectReason {
    match store.prepare_remote(item, p, r, NOW) {
        Err(Rejected::Policy(reason)) => reason,
        other => panic!("应被本机策略拒绝：{other:?}"),
    }
}

fn apply_one(store: &ClipStore, item: RemoteItem) -> RemoteApplied {
    let prepared = prepare_ok(store, item);
    let mut out = store.apply_remote_many(vec![prepared]).expect("落库");
    assert_eq!(out.len(), 1);
    out.remove(0)
}

fn search_hits(store: &ClipStore, q: &str) -> usize {
    store.search(q, ClipFilter::All, 50).expect("检索").items.len()
}

fn write_tmp(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, bytes).expect("写入");
    p
}

// ---------- 导出 ----------

#[test]
fn rows_and_export_carry_identity_times_and_content() {
    let f = Fixture::new();
    let raw = RawClipboard {
        text: Some("bold words".into()),
        html: Some("<b>bold words</b>".into()),
        rtf: Some(b"{\\rtf1 bold words}".to_vec()),
        source: Some(app("com.apple.TextEdit", "TextEdit")),
        ..RawClipboard::default()
    };
    let rich = expect_inserted(f.ingest(&raw, NOW));
    let plain = f.text("plain text", NOW + 10);
    f.store.set_pinned(rich.id, true).expect("置顶");
    let files = expect_inserted(f.ingest(
        &RawClipboard {
            files: Some(vec!["/tmp/wisp-sync-test-a.txt".into()]),
            ..RawClipboard::default()
        },
        NOW + 20,
    ));

    let rows = f.store.sync_rows().expect("rows");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].kind, ClipKind::Files, "最近使用的在前");
    let rich_hash = f.store.hash_of(rich.id).expect("hash").expect("存在");
    let row = rows.iter().find(|r| r.hash == rich_hash).expect("在列表里");
    assert_eq!(
        (row.kind, row.created_at, row.last_used_at, row.pinned),
        (ClipKind::RichText, NOW, NOW, true)
    );
    assert_eq!(
        f.store.ident_of(files.id).expect("ident").map(|(_, k)| k),
        Some(ClipKind::Files)
    );
    assert_eq!(f.store.hash_of(9_999).expect("hash"), None);

    let item = f.store.sync_export(&rich_hash).expect("导出").expect("存在");
    assert_eq!(item.text.as_deref(), Some("bold words"));
    assert!(item.html.as_deref().is_some_and(|h| h.contains("<b>")));
    assert_eq!(item.rtf.as_deref(), Some(&b"{\\rtf1 bold words}"[..]));
    let src = item.source.as_ref().expect("来源");
    assert_eq!(
        (src.app_id.as_str(), src.app_name.as_str()),
        ("com.apple.TextEdit", "TextEdit")
    );
    assert!(item.pinned && item.image.is_none());

    let plain_hash = f.store.hash_of(plain.id).expect("hash").expect("存在");
    let p = f.store.sync_export(&plain_hash).expect("导出").expect("存在");
    assert_eq!((p.text.as_deref(), p.source.is_none()), (Some("plain text"), true));

    // 文件条目不同步；不存在的 hash 为 None
    let files_hash = f.store.hash_of(files.id).expect("hash").expect("存在");
    assert!(f.store.sync_export(&files_hash).expect("导出").is_none());
    assert!(f.store.sync_export(&[0; 32]).expect("导出").is_none());

    // Debug 输出不含正文与来源
    let dbg = format!("{item:?} {rows:?}");
    assert!(!dbg.contains("bold") && !dbg.contains("TextEdit"), "{dbg}");
}

#[test]
fn image_export_gives_the_blob_path_and_skips_damaged_files() {
    let f = Fixture::new();
    let bytes = png(40, 30, true, |x, y| [x as u8, y as u8, 99, 200]);
    let m = expect_inserted(f.ingest(&image_raw(bytes.clone()), NOW));
    let hash = f.store.hash_of(m.id).expect("hash").expect("存在");
    let item = f.store.sync_export(&hash).expect("导出").expect("存在");
    let img = item.image.as_ref().expect("原图");
    assert_eq!((img.width, img.height, img.bytes), (40, 30, bytes.len() as u64));
    assert_eq!(std::fs::read(&img.path).expect("原图文件"), bytes);
    assert!(item.text.is_none() && item.html.is_none());
    // 原图丢了：不发布
    std::fs::remove_file(&img.path).expect("删除");
    assert!(f.store.sync_export(&hash).expect("导出").is_none());
}

// ---------- 检查 ----------

#[test]
fn remote_text_goes_through_local_privacy_rules_and_switches() {
    let f = Fixture::new();
    let p = policy();
    let keep = keep_all();
    // 疑似密钥
    let secret = remote_text_unchecked("sk-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789AbCdEfGhIjKl");
    assert_eq!(
        reject_reason(&f.store, secret, &p, &keep),
        RejectReason::Skipped(SkipReason::Secret)
    );
    // 排除名单（按应用标识，与系统无关）
    let mut from_excluded = remote_text("from a password manager", NOW, NOW, None);
    from_excluded.source = Some(SyncSource {
        app_id: "com.apple.Passwords".into(),
        app_name: "Passwords".into(),
    });
    assert_eq!(
        reject_reason(&f.store, from_excluded, &p, &keep),
        RejectReason::Skipped(SkipReason::ExcludedApp)
    );
    // 类型开关
    let mut off = policy();
    off.record_text = false;
    assert_eq!(
        reject_reason(&f.store, remote_text("hello", NOW, NOW, None), &off, &keep),
        RejectReason::Skipped(SkipReason::KindDisabled)
    );
    // 文件条目不接收
    let mut files = remote_text("/tmp/a", NOW, NOW, None);
    files.kind = ClipKind::Files;
    assert_eq!(reject_reason(&f.store, files, &p, &keep), RejectReason::Unsupported);
    // 声称的 hash 与内容不符
    let mut lying = remote_text("hello", NOW, NOW, None);
    lying.text = Some("something else".into());
    assert_eq!(reject_reason(&f.store, lying, &p, &keep), RejectReason::Mismatch);
    // 本机的单条上限更小：截断后 hash 不同，不接收
    let long = "x".repeat(4096);
    let mut small = policy();
    small.max_text_bytes = 1024;
    assert_eq!(
        reject_reason(&f.store, remote_text(&long, NOW, NOW, None), &small, &keep),
        RejectReason::Mismatch
    );
    // 空正文
    let mut empty = remote_text("hello", NOW, NOW, None);
    empty.text = Some("   ".into());
    assert_eq!(reject_reason(&f.store, empty, &p, &keep), RejectReason::Invalid);
}

/// 不经过「另一台设备」的条目（被拒绝的内容在那边也存不进去）：hash 随便给
fn remote_text_unchecked(text: &str) -> RemoteItem {
    RemoteItem {
        hash: [1; 32],
        kind: ClipKind::Text,
        created_at: NOW,
        last_used_at: NOW,
        pinned: None,
        text: Some(text.to_owned()),
        html: None,
        rtf: None,
        image_path: None,
        source: None,
        truncated: false,
    }
}

#[test]
fn retention_pre_check_rejects_expired_unpinned_items() {
    let f = Fixture::new();
    let week = RetentionPolicy {
        max_items: 1000,
        max_age_days: Some(7),
    };
    let old = remote_text("old item", NOW - 30 * DAY, NOW - 8 * DAY, None);
    assert_eq!(
        reject_reason(&f.store, old.clone(), &policy(), &week),
        RejectReason::Retention
    );
    // 置顶的不受天数限制
    let mut pinned = old;
    pinned.pinned = Some(true);
    assert!(f.store.prepare_remote(pinned, &policy(), &week, NOW).is_ok());
    let fresh = remote_text("fresh item", NOW - DAY, NOW - DAY, None);
    assert!(f.store.prepare_remote(fresh, &policy(), &week, NOW).is_ok());
}

#[test]
fn remote_images_are_decoded_and_rehashed() {
    let a = Fixture::new();
    let b = Fixture::new();
    let bytes = png(64, 48, false, |x, y| [x as u8 * 3, y as u8 * 5, 7, 255]);
    let m = expect_inserted(a.ingest(&image_raw(bytes.clone()), NOW));
    let hash = a.store.hash_of(m.id).expect("hash").expect("存在");
    let exported = a.store.sync_export(&hash).expect("导出").expect("存在");
    let spool = tempfile::tempdir().expect("临时目录");
    let src = write_tmp(spool.path(), "download.png", &bytes);
    let mut item = as_remote(&exported);
    item.image_path = Some(src.clone());

    // 声称的 hash 与像素不符
    let mut wrong = item.clone();
    wrong.image_path = Some(write_tmp(
        spool.path(),
        "other.png",
        &png(64, 48, false, |_, _| [1, 2, 3, 255]),
    ));
    assert_eq!(
        reject_reason(&b.store, wrong, &policy(), &keep_all()),
        RejectReason::Mismatch
    );
    // 不是 PNG
    let mut garbage = item.clone();
    garbage.image_path = Some(write_tmp(spool.path(), "garbage.png", b"not a png at all"));
    assert_eq!(
        reject_reason(&b.store, garbage, &policy(), &keep_all()),
        RejectReason::Invalid
    );
    // 没有原图文件
    let mut missing = item.clone();
    missing.image_path = None;
    assert_eq!(
        reject_reason(&b.store, missing, &policy(), &keep_all()),
        RejectReason::Invalid
    );
    // 本机不记录图片 / 超过本机上限
    let mut no_images = policy();
    no_images.record_images = false;
    assert_eq!(
        reject_reason(&b.store, item.clone(), &no_images, &keep_all()),
        RejectReason::Skipped(SkipReason::KindDisabled)
    );
    let mut tiny = policy();
    tiny.max_image_bytes = bytes.len() - 1;
    assert_eq!(
        reject_reason(&b.store, item.clone(), &tiny, &keep_all()),
        RejectReason::Skipped(SkipReason::TooLarge)
    );
    // 读不到文件是暂时性失败，不是策略拒绝
    let mut gone = item.clone();
    gone.image_path = Some(spool.path().join("vanished.png"));
    assert!(matches!(
        b.store.prepare_remote(gone, &policy(), &keep_all(), NOW),
        Err(Rejected::Failed(StoreError::Io(_)))
    ));

    // 被拒绝的尝试不留下任何 blob
    assert!(b.blob_files(BlobKind::Img).is_empty());
    let RemoteApplied::Inserted(meta) = apply_one(&b.store, item) else {
        panic!("应插入")
    };
    assert_eq!(meta.kind, ClipKind::Image);
    assert_eq!(meta.image.as_ref().map(|i| (i.width, i.height)), Some((64, 48)));
    assert!(
        meta.image.as_ref().is_some_and(|i| i.thumb_url.is_none()),
        "缩略图由调用方在后台生成"
    );
    assert_eq!(
        b.blob_files(BlobKind::Img),
        a.blob_files(BlobKind::Img),
        "同一像素 → 同一文件名"
    );
    assert!(src.exists(), "来源文件原样保留，由调用方处理");
    assert!(b.store.make_thumb(meta.id).expect("缩略图").is_some());
    let back = b.store.sync_export(&hash).expect("导出").expect("存在");
    assert_eq!(std::fs::read(&back.image.expect("原图").path).expect("读"), bytes);
}

// ---------- 落库与合并 ----------

#[test]
fn remote_items_keep_their_times_pin_and_are_searchable() {
    let f = Fixture::new();
    let RemoteApplied::Inserted(m) = apply_one(
        &f.store,
        remote_text("remote zqsyncword", NOW - 5_000, NOW - 1_000, Some(true)),
    ) else {
        panic!("应插入")
    };
    assert_eq!(
        (m.created_at, m.last_used_at, m.pinned, m.use_count),
        (NOW - 5_000, NOW - 1_000, true, 1)
    );
    assert_eq!(search_hits(&f.store, "zqsyncword"), 1, "FTS 由触发器维护");
    assert_eq!(search_hits(&f.store, "sy"), 1, "短查询的 LIKE 同样能找到");
    // 排序按它在别的设备上的时间：比本机更新的条目排在后面
    f.text("local newer", NOW);
    let page = f.store.recent(0, 10, ClipFilter::All).expect("列表");
    assert_eq!(page.items[0].item.id, m.id, "置顶在前");
}

#[test]
fn merging_takes_earliest_created_latest_used_and_the_remote_pin() {
    let f = Fixture::new();
    let local = f.text("shared text", NOW);
    // 远端更早创建、更晚使用、置顶
    let RemoteApplied::Merged { meta, moved } =
        apply_one(&f.store, remote_text("shared text", NOW - 100, NOW + 500, Some(true)))
    else {
        panic!("应合并")
    };
    assert_eq!(meta.id, local.id);
    assert_eq!(
        (meta.created_at, meta.last_used_at, meta.pinned),
        (NOW - 100, NOW + 500, true)
    );
    assert!(moved);
    assert_eq!(meta.use_count, local.use_count, "使用次数只属于本机");
    // 远端更旧的时间不会把本机的时间往回拨；没有置顶记录时保留本机的
    assert_eq!(
        apply_one(&f.store, remote_text("shared text", NOW, NOW + 100, None)),
        RemoteApplied::Unchanged(local.id)
    );
    // 只改了 created_at：不算移动
    let RemoteApplied::Merged { meta, moved } =
        apply_one(&f.store, remote_text("shared text", NOW - 200, NOW, Some(true)))
    else {
        panic!("应合并")
    };
    assert_eq!((meta.created_at, moved), (NOW - 200, false));
    // 取消置顶
    let RemoteApplied::Merged { meta, moved } = apply_one(&f.store, remote_text("shared text", NOW, NOW, Some(false)))
    else {
        panic!("应合并")
    };
    assert!(!meta.pinned && moved);
    assert_eq!(f.count("SELECT count(*) FROM items"), 1);
}

#[test]
fn count_limit_rejects_items_that_would_be_trimmed_at_once() {
    let f = Fixture::new();
    for i in 0..10 {
        f.text(&format!("local {i}"), NOW + i);
    }
    let ten = RetentionPolicy {
        max_items: 10,
        max_age_days: None,
    };
    let prep = |item: RemoteItem| f.store.prepare_remote(item, &policy(), &ten, NOW).expect("检查通过");
    let out = f
        .store
        .apply_remote_many(vec![
            prep(remote_text("older than all", NOW - 1, NOW - 1, None)),
            prep(remote_text("newer than all", NOW + 100, NOW + 100, None)),
            prep(remote_text("old but pinned", NOW - 1, NOW - 1, Some(true))),
        ])
        .expect("落库");
    assert_eq!(out[0], RemoteApplied::Rejected(RejectReason::Retention));
    assert!(matches!(out[1], RemoteApplied::Inserted(_)));
    assert!(matches!(out[2], RemoteApplied::Inserted(_)));
}

#[test]
fn batches_are_bounded() {
    let f = Fixture::new();
    let many: Vec<_> = (0..=MAX_REMOTE_BATCH)
        .map(|i| prepare_ok(&f.store, remote_text(&format!("item {i}"), NOW, NOW, None)))
        .collect();
    assert!(matches!(
        f.store.apply_remote_many(many),
        Err(StoreError::InvalidInput(_))
    ));
    assert!(f.store.apply_remote_many(Vec::new()).expect("空批").is_empty());
}

#[test]
fn a_batch_is_one_transaction_and_duplicates_in_it_merge() {
    let f = Fixture::new();
    let out = f
        .store
        .apply_remote_many(vec![
            prepare_ok(&f.store, remote_text("dup", NOW, NOW, None)),
            prepare_ok(&f.store, remote_text("dup", NOW - 10, NOW + 10, None)),
        ])
        .expect("落库");
    assert!(matches!(out[0], RemoteApplied::Inserted(_)));
    let RemoteApplied::Merged { meta, .. } = &out[1] else {
        panic!("同一批里的第二条合并进第一条")
    };
    assert_eq!((meta.created_at, meta.last_used_at), (NOW - 10, NOW + 10));
}

#[test]
fn round_trip_between_two_devices_preserves_everything() {
    let a = Fixture::new();
    let b = Fixture::new();
    let raw = RawClipboard {
        text: Some("https://example.com/path".into()),
        html: Some("<a href=\"https://example.com/path\">link</a>".into()),
        source: Some(app("com.apple.Safari", "Safari")),
        ..RawClipboard::default()
    };
    let link = expect_inserted(a.ingest(&raw, NOW));
    let code = a.text("fn main() {\n    run();\n}", NOW + 1);
    let img = expect_inserted(a.ingest(&image_raw(png(8, 8, true, |x, _| [x as u8, 0, 0, 128])), NOW + 2));
    for m in [&link, &code, &img] {
        let hash = a.store.hash_of(m.id).expect("hash").expect("存在");
        let item = a.store.sync_export(&hash).expect("导出").expect("存在");
        let RemoteApplied::Inserted(got) = apply_one(&b.store, as_remote(&item)) else {
            panic!("应插入")
        };
        assert_eq!(
            (got.kind, got.created_at, got.last_used_at),
            (m.kind, m.created_at, m.last_used_at)
        );
        assert_eq!(
            got.source.as_ref().map(|s| s.name.as_str()),
            m.source.as_ref().map(|s| s.name.as_str())
        );
        let back = b.store.sync_export(&hash).expect("导出").expect("存在");
        assert_eq!(
            (back.text.clone(), back.html.clone(), back.kind),
            (item.text, item.html, item.kind)
        );
    }
    assert_eq!(b.store.sync_rows().expect("rows").len(), 3);
    // b 的条目之后被本机复制：去重到同一条
    let again = b.text("fn main() {\n    run();\n}", NOW + 50);
    assert_eq!(again.use_count, 2);
}

#[test]
fn image_blobs_self_heal_from_a_remote_copy() {
    let f = Fixture::new();
    let bytes = png(16, 16, false, |x, y| [x as u8, y as u8, 1, 255]);
    let m = expect_inserted(f.ingest(&image_raw(bytes.clone()), NOW));
    let hash = f.store.hash_of(m.id).expect("hash").expect("存在");
    let blob = f
        .store
        .sync_export(&hash)
        .expect("导出")
        .expect("存在")
        .image
        .expect("原图")
        .path;
    std::fs::write(&blob, b"broken").expect("弄坏");
    let spool = tempfile::tempdir().expect("临时目录");
    let item = RemoteItem {
        hash,
        kind: ClipKind::Image,
        created_at: NOW,
        last_used_at: NOW,
        pinned: None,
        text: None,
        html: None,
        rtf: None,
        image_path: Some(write_tmp(spool.path(), "img.png", &bytes)),
        source: None,
        truncated: false,
    };
    assert_eq!(apply_one(&f.store, item), RemoteApplied::Unchanged(m.id));
    assert_eq!(std::fs::read(&blob).expect("读"), bytes, "原图被补回");
}

// ---------- 置顶、删除与清空 ----------

#[test]
fn pin_and_delete_by_hash() {
    let f = Fixture::new();
    let a = f.text("alpha zqdelmark", NOW);
    let b = f.text("beta", NOW + 1);
    let img = expect_inserted(f.ingest(&image_raw(png(4, 4, false, |_, _| [9, 9, 9, 255])), NOW + 2));
    let ha = f.store.hash_of(a.id).expect("hash").expect("存在");
    let hb = f.store.hash_of(b.id).expect("hash").expect("存在");
    let hi = f.store.hash_of(img.id).expect("hash").expect("存在");

    let meta = f.store.set_pinned_by_hash(&ha, true).expect("置顶").expect("存在");
    assert!(meta.pinned && meta.id == a.id);
    assert!(f.store.set_pinned_by_hash(&[0; 32], true).expect("置顶").is_none());

    let removed = f.store.delete_by_hashes(&[ha, hi, [0; 32], ha]).expect("删除");
    assert_eq!(removed, vec![a.id, img.id]);
    assert_eq!(search_hits(&f.store, "zqdelmark"), 0, "索引里也没有了");
    assert!(f.blob_files(BlobKind::Img).is_empty(), "原图一并删除");
    assert_eq!(
        f.store
            .sync_rows()
            .expect("rows")
            .iter()
            .map(|r| r.hash)
            .collect::<Vec<_>>(),
        vec![hb]
    );
    assert!(f.store.delete_by_hashes(&[]).expect("空").is_empty());
}

#[test]
fn remote_clear_removes_unsynced_rows_up_to_the_cutoff() {
    let f = Fixture::new();
    let synced = f.text("has a sync record", NOW);
    let pinned = f.text("pinned and unsynced", NOW + 1);
    f.store.set_pinned(pinned.id, true).expect("置顶");
    let files = expect_inserted(f.ingest(
        &RawClipboard {
            files: Some(vec!["/tmp/wisp-sync-clear.txt".into()]),
            ..RawClipboard::default()
        },
        NOW + 2,
    ));
    let later = f.text("copied after the clear", NOW + 100);
    let synced_hash = f.store.hash_of(synced.id).expect("hash").expect("存在");

    let removed = f
        .store
        .delete_unsynced_before(NOW + 50, true, &[synced_hash])
        .expect("清空");
    assert_eq!(removed, vec![files.id]);
    let left: Vec<i64> = f
        .store
        .recent(0, 10, ClipFilter::All)
        .expect("列表")
        .items
        .iter()
        .map(|h| h.item.id)
        .collect();
    assert_eq!(left, vec![pinned.id, later.id, synced.id]);
    // 清空全部（含置顶）
    let removed = f
        .store
        .delete_unsynced_before(NOW + 50, false, &[synced_hash])
        .expect("清空");
    assert_eq!(removed, vec![pinned.id]);
}

#[test]
fn hex_keys_round_trip() {
    let f = Fixture::new();
    let m = f.text("hex me", NOW);
    let hash = f.store.hash_of(m.id).expect("hash").expect("存在");
    assert_eq!(parse_hash_hex(&hash_hex(&hash)), Some(hash));
}

// ---------- 识别历史 ----------

fn ocr_entry<'a>(t: i64, mode: OcrMode, text: &'a str, image: Option<&'a [u8]>) -> OcrHistoryNew<'a> {
    OcrHistoryNew {
        created_at: t,
        mode,
        model: "gpt-4o-mini",
        text,
        thumb: None,
        image,
    }
}

fn remote_ocr(t: i64, text: &str) -> RemoteOcr {
    RemoteOcr {
        key: ocr_key(t, OcrMode::Lines, text),
        created_at: t,
        mode: OcrMode::Lines,
        model: "model".into(),
        text: text.to_owned(),
        thumb_path: None,
        image_path: None,
    }
}

#[test]
fn ocr_keys_rows_and_export() {
    let f = Fixture::new();
    let image = png(32, 20, false, |x, y| [x as u8, y as u8, 3, 255]);
    let a = f
        .store
        .ocr_history_add(&ocr_entry(NOW, OcrMode::Markdown, "第一条", Some(&image)))
        .expect("新增");
    let b = f
        .store
        .ocr_history_add(&ocr_entry(NOW + 1, OcrMode::Lines, "second", None))
        .expect("新增");
    let key_a = ocr_key(NOW, OcrMode::Markdown, "第一条");
    assert_eq!(f.store.ocr_key_of(a).expect("键"), Some(key_a.clone()));
    assert_eq!(f.store.ocr_key_of(9_999).expect("键"), None);

    let rows = f.store.ocr_sync_rows().expect("rows");
    assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![b, a], "新的在前");
    assert_eq!(rows[1].key, key_a);
    assert_eq!(rows[1].mode, OcrMode::Markdown);

    let item = f.store.ocr_sync_export(&key_a).expect("导出").expect("存在");
    assert_eq!(
        (item.text.as_str(), item.model.as_str(), item.mode),
        ("第一条", "gpt-4o-mini", OcrMode::Markdown)
    );
    assert_eq!(
        std::fs::read(item.image_path.as_ref().expect("原图")).expect("读"),
        image
    );
    assert!(
        item.thumb_path.as_ref().is_some_and(|p| p.is_file()),
        "缩略图由原图生成"
    );
    assert!(
        f.store
            .ocr_sync_export("0".repeat(32).as_str())
            .expect("导出")
            .is_none()
    );
    let plain = f.store.ocr_sync_export_by_id(b).expect("导出").expect("存在");
    assert!(plain.image_path.is_none() && plain.thumb_path.is_none());
}

#[test]
fn ocr_round_trip_dedups_by_key_and_respects_the_limit() {
    let a = Fixture::new();
    let b = Fixture::new();
    let image = png(48, 32, false, |x, y| [x as u8, 200, y as u8, 255]);
    let id = a
        .store
        .ocr_history_add(&ocr_entry(NOW, OcrMode::Translate, "译文", Some(&image)))
        .expect("新增");
    let exported = a.store.ocr_sync_export_by_id(id).expect("导出").expect("存在");
    let remote = RemoteOcr {
        key: exported.key.clone(),
        created_at: exported.created_at,
        mode: exported.mode,
        model: exported.model.clone(),
        text: exported.text.clone(),
        thumb_path: exported.thumb_path.clone(),
        image_path: exported.image_path.clone(),
    };
    let prep = |r: RemoteOcr| b.store.ocr_prepare_remote(r).expect("检查通过");
    let out = b
        .store
        .ocr_apply_remote(vec![prep(remote.clone()), prep(remote.clone())], 50)
        .expect("落库");
    let OcrApplied::Inserted(item) = &out[0] else {
        panic!("应插入")
    };
    assert_eq!(out[1], OcrApplied::Existing(item.id), "同一批里重复的按键去重");
    assert_eq!(
        (item.created_at, item.mode, item.text.as_str()),
        (NOW, OcrMode::Translate, "译文")
    );
    assert!(item.thumb_url.is_some() && item.image_url.is_some());
    assert_eq!(
        b.blob_files(BlobKind::Ocr),
        a.blob_files(BlobKind::Ocr),
        "图片文件名按内容 hash，与原设备一致"
    );
    // 再来一次：已有
    assert_eq!(
        b.store.ocr_apply_remote(vec![prep(remote.clone())], 50).expect("落库"),
        vec![OcrApplied::Existing(item.id)]
    );
    assert_eq!(b.store.ocr_key_of(item.id).expect("键"), Some(exported.key.clone()));

    // 键不符、正文为空
    let mut lying = remote.clone();
    lying.text = "tampered".into();
    assert!(matches!(
        b.store.ocr_prepare_remote(lying),
        Err(Rejected::Policy(RejectReason::Mismatch))
    ));
    let mut empty = remote_ocr(NOW, " ");
    empty.key = ocr_key(NOW, OcrMode::Lines, " ");
    assert!(matches!(
        b.store.ocr_prepare_remote(empty),
        Err(Rejected::Policy(RejectReason::Invalid))
    ));

    // 上限：排在 limit 之外的不插入
    for i in 1..=3 {
        b.store
            .ocr_history_add(&ocr_entry(NOW + i * 10, OcrMode::Lines, &format!("local {i}"), None))
            .expect("新增");
    }
    let out = b
        .store
        .ocr_apply_remote(
            vec![prep(remote_ocr(NOW - 1, "too old")), prep(remote_ocr(NOW + 100, "new"))],
            4,
        )
        .expect("落库");
    assert_eq!(out[0], OcrApplied::Rejected(RejectReason::Retention));
    assert!(matches!(out[1], OcrApplied::Inserted(_)));
}

#[test]
fn ocr_remote_without_thumbnail_derives_one_and_ignores_non_images() {
    let f = Fixture::new();
    let spool = tempfile::tempdir().expect("临时目录");
    let mut r = remote_ocr(NOW, "with image only");
    r.image_path = Some(write_tmp(
        spool.path(),
        "img.png",
        &png(300, 100, false, |x, _| [x as u8, 0, 0, 255]),
    ));
    r.thumb_path = Some(write_tmp(spool.path(), "thumb.bin", b"definitely not an image"));
    let p = f.store.ocr_prepare_remote(r).expect("检查通过");
    let out = f.store.ocr_apply_remote(vec![p], 50).expect("落库");
    let OcrApplied::Inserted(item) = &out[0] else {
        panic!("应插入")
    };
    assert!(item.thumb_url.is_some(), "从原图生成缩略图");
    assert_eq!(f.blob_files(BlobKind::Ocr).len(), 2);
    assert!(spool.path().join("img.png").exists(), "来源文件原样保留");
}

#[test]
fn ocr_delete_by_keys_and_keyed_delete() {
    let f = Fixture::new();
    let image = png(8, 8, false, |_, _| [5, 5, 5, 255]);
    let a = f
        .store
        .ocr_history_add(&ocr_entry(NOW, OcrMode::Lines, "a", Some(&image)))
        .expect("新增");
    let b = f
        .store
        .ocr_history_add(&ocr_entry(NOW + 1, OcrMode::Lines, "b", Some(&image)))
        .expect("新增");
    let c = f
        .store
        .ocr_history_add(&ocr_entry(NOW + 2, OcrMode::Paragraphs, "c", None))
        .expect("新增");
    let removed = f
        .store
        .ocr_delete_by_keys(&[ocr_key(NOW, OcrMode::Lines, "a"), "f".repeat(32)])
        .expect("删除");
    assert_eq!(removed, vec![a]);
    assert_eq!(f.blob_files(BlobKind::Ocr).len(), 2, "仍被 b 引用的图片保留");
    assert_eq!(
        f.store.ocr_history_delete_keyed(b).expect("删除"),
        Some(ocr_key(NOW + 1, OcrMode::Lines, "b"))
    );
    assert!(f.blob_files(BlobKind::Ocr).is_empty());
    assert_eq!(f.store.ocr_history_delete_keyed(b).expect("删除"), None);
    assert_eq!(
        f.store
            .ocr_sync_rows()
            .expect("rows")
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>(),
        vec![c]
    );
    assert!(f.store.ocr_delete_by_keys(&[]).expect("空").is_empty());
}

#[test]
fn ingest_still_records_with_now_times() {
    // 插入路径改为接受显式时间之后，本机记录仍是「现在、不置顶」
    let f = Fixture::new();
    let m = expect_inserted(f.ingest(&raw_text("plain local copy"), NOW));
    assert_eq!(
        (m.created_at, m.last_used_at, m.pinned, m.use_count),
        (NOW, NOW, false, 1)
    );
}
