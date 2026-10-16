//! 收藏（名称、收藏筛选、名称检索）、验证码条目的过期、条目检视与详情的列位置。

mod common;

use common::{Fixture, NOW, expect_inserted, policy, raw_text};
use wisp_clipboard::{
    IngestPolicy, MAX_NAME_CHARS, OTP_TTL_MS, RecentCache, StoreError, clean_favorite_name, otp_expired,
    smart::RecognizeOpts,
};
use wisp_types::{ClipFilter, ClipKind, EntityKind, RawClipboard, TransformId};

const OTP: &str = "【银行】您的验证码为 482915，5 分钟内有效，请勿泄露。";

fn ids(page: &wisp_types::ClipPage) -> Vec<i64> {
    page.items.iter().map(|h| h.item.id).collect()
}

// ---------- 名称 ----------

#[test]
fn naming_favorites_the_item_and_clearing_keeps_it() {
    let f = Fixture::new();
    let a = f.text("ssh deploy@203.0.113.7 -p 2222", NOW);
    let b = f.text("later item", NOW + 1);
    assert!(a.name.is_none() && !a.pinned);

    let named = f.store.set_name(a.id, Some("  Staging server ")).expect("命名");
    assert_eq!(named.name.as_deref(), Some("Staging server"));
    assert!(named.pinned, "命名即收藏");
    // 收藏排在前面
    assert_eq!(
        ids(&f.store.recent(0, 10, ClipFilter::All).expect("列表")),
        [a.id, b.id]
    );

    let cleared = f.store.set_name(a.id, None).expect("清除");
    assert!(cleared.name.is_none() && cleared.pinned, "清除名称不取消收藏");
    // 空白等同于清除
    f.store.set_name(a.id, Some("x")).expect("命名");
    assert!(f.store.set_name(a.id, Some("   ")).expect("清除").name.is_none());

    // 取消收藏时名称一并去掉
    f.store.set_name(a.id, Some("again")).expect("命名");
    let unpinned = f.store.set_pinned(a.id, false).expect("取消收藏");
    assert!(unpinned.name.is_none() && !unpinned.pinned);
    let repinned = f.store.set_pinned(a.id, true).expect("收藏");
    assert!(repinned.name.is_none(), "重新收藏不会找回旧名称");
}

#[test]
fn invalid_names_are_rejected_without_echoing_them() {
    let f = Fixture::new();
    let a = f.text("note", NOW);
    let long = "名".repeat(MAX_NAME_CHARS + 1);
    for bad in [long.as_str(), "line\nbreak", "\u{202E}spoof"] {
        match f.store.set_name(a.id, Some(bad)) {
            Err(StoreError::InvalidInput(msg)) => assert!(!msg.contains(bad.trim()), "{msg}"),
            other => panic!("应拒绝：{other:?}"),
        }
    }
    assert!(clean_favorite_name(&long).is_err());
    assert!(matches!(
        f.store.set_name(9999, Some("x")),
        Err(StoreError::NotFound(9999))
    ));
    assert!(matches!(f.store.set_name(9999, None), Err(StoreError::NotFound(9999))));
    // 文件、图片这类没有 meta_json 的条目也能命名与清除
    let files = RawClipboard {
        files: Some(vec!["/tmp/wisp-report.pdf".into()]),
        ..RawClipboard::default()
    };
    let fm = expect_inserted(f.ingest(&files, NOW + 1));
    assert_eq!(
        f.store.set_name(fm.id, Some("Report")).expect("命名").name.as_deref(),
        Some("Report")
    );
    assert!(f.store.set_name(fm.id, None).expect("清除").name.is_none());
}

#[test]
fn names_are_searchable_on_both_search_paths() {
    let f = Fixture::new();
    let a = f.text("ssh deploy@203.0.113.7 -p 2222", NOW);
    let b = f.text("unrelated text", NOW + 1);
    f.store.set_name(a.id, Some("Staging 服务器")).expect("命名");
    // ≥ 3 个字符：FTS 路径；名称命中的条目与正文命中的一起返回
    let page = f.store.search("staging", ClipFilter::All, 50).expect("搜索");
    assert_eq!(ids(&page), [a.id]);
    assert_eq!(page.items[0].item.name.as_deref(), Some("Staging 服务器"));
    let page = f.store.search("text", ClipFilter::All, 50).expect("搜索");
    assert_eq!(ids(&page), [b.id], "正文检索不受影响");
    // 1–2 个字符：LIKE 路径（大小写折叠一致）
    assert_eq!(ids(&f.store.search("服务", ClipFilter::All, 50).expect("搜索")), [a.id]);
    // 筛选同样作用于名称命中
    assert!(
        f.store
            .search("staging", ClipFilter::Image, 50)
            .expect("搜索")
            .items
            .is_empty()
    );
    // 取消收藏后名称消失，也就搜不到了
    f.store.set_pinned(a.id, false).expect("取消收藏");
    assert!(
        f.store
            .search("staging", ClipFilter::All, 50)
            .expect("搜索")
            .items
            .is_empty()
    );
}

// ---------- 收藏筛选 ----------

#[test]
fn favorite_filter_queries_the_database_and_matches_the_cache() {
    let f = Fixture::new();
    let mut favs = Vec::new();
    for i in 0..5 {
        let m = f.text(&format!("item {i}"), NOW + i);
        if i % 2 == 0 {
            favs.push(f.store.set_pinned(m.id, true).expect("收藏").id);
        }
    }
    favs.sort_unstable_by(|a, b| b.cmp(a));
    let page = f.store.recent(0, 10, ClipFilter::Favorite).expect("收藏");
    assert_eq!(ids(&page), favs);
    assert_eq!(page.pinned_count, 3);
    assert_eq!(page.filter, ClipFilter::Favorite);
    // 缓存给出同样的结果（缓存是数据库排序的前缀，收藏都排在前面）
    let cache = RecentCache::load(&f.store).expect("缓存");
    assert_eq!(ids(&cache.page(ClipFilter::Favorite, 10)), favs);
    // 分页
    let first = f.store.recent(0, 2, ClipFilter::Favorite).expect("第一页");
    assert_eq!((first.items.len(), first.next_offset), (2, Some(2)));
}

// ---------- 详情的列位置 ----------

#[test]
fn detail_columns_follow_the_extended_meta_columns() {
    let f = Fixture::new();
    let raw = RawClipboard {
        text: Some("bold text".into()),
        html: Some("<b>bold text</b>".into()),
        ..RawClipboard::default()
    };
    let m = expect_inserted(f.ingest(&raw, NOW));
    f.store.set_name(m.id, Some("Named")).expect("命名");
    let d = f.store.get_detail(m.id).expect("详情").expect("存在");
    assert_eq!(d.text.as_deref(), Some("bold text"));
    assert!(d.has_html && !d.has_rtf);
    assert_eq!(d.meta.name.as_deref(), Some("Named"));
    assert_eq!(d.meta.kind, ClipKind::RichText);
    // 文件条目的完整列表在最后一列
    let files = RawClipboard {
        files: Some(vec!["/tmp/wisp-a.txt".into(), "/tmp/wisp-b.txt".into()]),
        ..RawClipboard::default()
    };
    let fm = expect_inserted(f.ingest(&files, NOW + 1));
    let d = f.store.get_detail(fm.id).expect("详情").expect("存在");
    assert_eq!(d.files.as_ref().map(Vec::len), Some(2));
    assert!(d.text.is_none() && d.image_url.is_none());
}

// ---------- 验证码过期 ----------

#[test]
fn otp_items_expire_and_favorites_do_not() {
    let f = Fixture::new();
    let otp = f.text(OTP, NOW);
    let note = f.text("not a code 482915", NOW + 1);
    assert_eq!(otp.expires_at, Some(NOW + OTP_TTL_MS));
    assert!(note.expires_at.is_none());
    assert_eq!(f.store.next_expiry().expect("下次"), Some(NOW + OTP_TTL_MS));

    // 再次复制同一条（touch）不延长
    let again = f.text(OTP, NOW + 60_000);
    assert_eq!((again.id, again.expires_at), (otp.id, Some(NOW + OTP_TTL_MS)));

    let otp_hash = f.store.hash_of(otp.id).expect("hash").expect("存在");
    // 到期之前什么都不删；读取时按 expires_at 过滤
    assert!(f.store.expire_due(NOW + OTP_TTL_MS - 1).expect("清理").is_empty());
    assert!(!otp_expired(&again, NOW + OTP_TTL_MS - 1) && otp_expired(&again, NOW + OTP_TTL_MS));

    let due = f.store.expire_due(NOW + OTP_TTL_MS).expect("清理");
    assert_eq!(due, [(otp.id, otp_hash)]);
    assert!(f.store.get_detail(otp.id).expect("详情").is_none(), "已删除");
    assert_eq!(f.store.next_expiry().expect("下次"), None);
    // 被删内容不留在索引里
    assert!(
        f.store
            .search("482915", ClipFilter::All, 10)
            .expect("搜索")
            .items
            .iter()
            .all(|h| h.item.id != otp.id)
    );
}

#[test]
fn favoriting_removes_the_expiry() {
    let f = Fixture::new();
    let otp = f.text(OTP, NOW);
    let fav = f.store.set_pinned(otp.id, true).expect("收藏");
    assert!(fav.expires_at.is_none());
    assert!(f.store.expire_due(NOW + OTP_TTL_MS * 2).expect("清理").is_empty());
    // 取消收藏后也不再过期（用户已经明确要保留过）
    let unfav = f.store.set_pinned(otp.id, false).expect("取消收藏");
    assert!(unfav.expires_at.is_none());
    // 命名同样会去掉过期时间
    let other = f.text("验证码 551234，10 分钟内有效", NOW + 1);
    assert!(other.expires_at.is_some());
    assert!(
        f.store
            .set_name(other.id, Some("keep"))
            .expect("命名")
            .expires_at
            .is_none()
    );
}

#[test]
fn auto_delete_off_records_no_expiry() {
    let f = Fixture::new();
    let p = IngestPolicy {
        otp_ttl_ms: None,
        ..policy()
    };
    let m = match f.store.ingest(&raw_text(OTP), &p, NOW).expect("记录") {
        wisp_clipboard::IngestOutcome::Inserted(m) => m,
        other => panic!("{other:?}"),
    };
    assert!(m.expires_at.is_none());
    assert_eq!(f.store.next_expiry().expect("下次"), None);
    // 但仍然记下了验证码标记（「同步验证码」关闭时据此不发布）
    let row = f.store.sync_rows().expect("rows").into_iter().next().expect("一条");
    assert!(row.otp && row.expires_at.is_none());
}

// ---------- 检视 ----------

#[test]
fn inspect_reads_the_item_text() {
    let f = Fixture::new();
    let m = f.text("联系 a@b.io\n电话 13800138000", NOW);
    let r = f
        .store
        .inspect(m.id, &RecognizeOpts::default())
        .expect("检视")
        .expect("存在");
    assert_eq!(
        r.entities.iter().map(|e| e.kind).collect::<Vec<_>>(),
        [EntityKind::Email, EntityKind::Phone]
    );
    assert!(r.transforms.contains(&TransformId::JoinLines));
    assert!(
        f.store
            .inspect(9999, &RecognizeOpts::default())
            .expect("检视")
            .is_none()
    );
    // JSON 按全文判断（超过识别范围的长 JSON 也能格式化）
    let big = format!("{{\"k\":[{}1]}}", "1,".repeat(20_000));
    let j = f.text(&big, NOW + 1);
    let r = f
        .store
        .inspect(j.id, &RecognizeOpts::default())
        .expect("检视")
        .expect("存在");
    assert!(r.transforms.contains(&TransformId::JsonPretty));
    // 变换按完整正文执行
    let (kind, text) = f.store.text_of(j.id).expect("正文").expect("存在");
    let pretty = wisp_clipboard::smart::apply(TransformId::JsonPretty, kind, text.as_deref().unwrap_or_default())
        .expect("格式化");
    assert!(pretty.starts_with("{\n  \"k\": [\n    1,") && pretty.ends_with("\n  ]\n}"));
    // 图片与文件没有正文
    let files = RawClipboard {
        files: Some(vec!["/tmp/wisp-x".into()]),
        ..RawClipboard::default()
    };
    let fm = expect_inserted(f.ingest(&files, NOW + 2));
    assert_eq!(f.store.text_of(fm.id).expect("正文"), Some((ClipKind::Files, None)));
    assert_eq!(
        f.store.inspect(fm.id, &RecognizeOpts::default()).expect("检视"),
        Some(wisp_types::ClipInspect::default())
    );
}
