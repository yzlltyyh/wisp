//! 剪贴板与识别历史：加入不复活已删除 / 已清空的条目、加入不删除本机独有的历史、清空后置顶、
//! 远端置顶让本机清理过的条目重新出现、保留策略裁剪服务器历史、blob 只由上传者回收、
//! 加入永不删除本机的模型服务 / Key / 术语。

mod sim;

use std::sync::atomic::Ordering;

use serde_json::json;
use sim::{Device, assert_converged, blob_paths, fault_once, read_log, settle, tick};
use wisp_sync::remote::memory::{Fault, Op};
use wisp_sync::{ConnectOutcome, Domain, MemoryRemote, RemoteError};

#[tokio::test]
async fn join_never_deletes_local_providers_keys_or_terms() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    a.store.set(
        Domain::Cfg,
        "/providers/openai",
        json!({"base_url": "https://api.openai.com"}),
    );
    a.store.set_key("openai", "sk-a");
    a.store.set(Domain::Glo, "term", json!({"target": "a"}));
    a.connect().await.unwrap();
    a.sync_fully().await.unwrap();
    // a 删除了它们：服务器上只剩墓碑
    a.store.remove(Domain::Cfg, "/providers/openai");
    a.store.remove(Domain::Key, "openai");
    a.store.remove(Domain::Glo, "term");
    a.sync_fully().await.unwrap();

    // c 本机有同名的模型服务、Key 与术语（id 由模板决定，很容易同名）
    let mut c = Device::new(&server, "c", 0xc);
    c.store.set(
        Domain::Cfg,
        "/providers/openai",
        json!({"base_url": "https://proxy.example"}),
    );
    c.store.set_key("openai", "sk-c");
    c.store.set(Domain::Glo, "term", json!({"target": "c"}));
    c.connect().await.unwrap();
    settle(&mut [&mut a, &mut c]).await;

    let s = c.state();
    assert_eq!(s.cfg["/providers/openai"], json!({"base_url": "https://proxy.example"}));
    assert_eq!(s.keys["openai"]["key"], json!("sk-c"));
    assert_eq!(s.glo["term"], json!({"target": "c"}));
    assert_converged(&[&a, &c]);
}

#[tokio::test]
async fn join_does_not_resurrect_deleted_or_cleared_items() {
    let server = MemoryRemote::new();
    // c 很早就有这些条目（条目自身的时间早于之后的删除与清空）
    let mut c = Device::new(&server, "c", 0xc);
    c.store.copy("old-deleted");
    c.store.copy("old-cleared");
    tick();
    let mut a = Device::new(&server, "a", 0xa);
    a.store.copy("old-deleted");
    a.store.copy("old-cleared");
    let kept = a.store.copy("kept-pinned");
    a.connect().await.unwrap();
    a.sync_fully().await.unwrap();
    tick();
    a.store.pin(&kept, true);
    a.store.delete(&sim::text_hash_hex("old-deleted"));
    tick();
    a.store.clear(true);
    a.sync_fully().await.unwrap();
    tick();
    let fresh = c.store.copy("new-after");

    c.connect().await.unwrap();
    settle(&mut [&mut a, &mut c]).await;
    let old_deleted = sim::text_hash_hex("old-deleted");
    let old_cleared = sim::text_hash_hex("old-cleared");
    let s = a.state();
    assert!(!s.clip.contains_key(&old_deleted), "删除的条目复活了");
    assert!(!s.clip.contains_key(&old_cleared), "清空的条目复活了");
    assert!(s.clip[&kept].pinned);
    assert!(s.clip.contains_key(&fresh));
    // c 上：服务器记着被删除的那一条，c 的同一条随之删除；被清空的那一条服务器上已经没有记录，
    // 对 c 来说是加入前本机独有的历史，留在本机、不发布
    let mut sc = c.state();
    assert!(!sc.clip.contains_key(&old_deleted));
    assert!(sc.clip.remove(&old_cleared).is_some(), "加入删除了本机独有的历史");
    assert_eq!(sc.clip.keys().collect::<Vec<_>>(), s.clip.keys().collect::<Vec<_>>());
    assert!(sc.clip[&kept].pinned);
}

#[tokio::test]
async fn joining_keeps_local_only_history_hidden_by_an_earlier_clear() {
    let server = MemoryRemote::new();
    // b 很早就有只属于自己的条目（一条置顶）与识别记录
    let mut b = Device::new(&server, "b", 0xb);
    let x = b.store.copy("b-only");
    let y = b.store.copy("b-only-pinned");
    b.store.pin(&y, true);
    let o = b.store.ocr_add("b-only-ocr");
    tick();
    // a 之后清空过全部历史（连置顶的）与识别历史
    let mut a = Device::new(&server, "a", 0xa);
    a.store.copy("a-old");
    a.connect().await.unwrap();
    a.sync_fully().await.unwrap();
    tick();
    a.store.clear(false);
    a.store.ocr_clear();
    a.sync_fully().await.unwrap();
    tick();
    let fresh = b.store.copy("b-new");

    assert_eq!(b.connect().await.unwrap(), ConnectOutcome::Joined);
    settle(&mut [&mut a, &mut b]).await;
    // 加入不删除本机独有的历史（置顶的也在、仍是置顶），但也不把它们发布出去（否则会在其他设备上复活）
    let s = b.state();
    assert!(s.clip.contains_key(&x), "{:?}", s.clip);
    assert!(s.clip.get(&y).is_some_and(|i| i.pinned), "{:?}", s.clip);
    assert!(s.ocr.contains_key(&o));
    assert!(s.clip.contains_key(&fresh));
    let sa = a.state();
    assert!(!sa.clip.contains_key(&x) && !sa.clip.contains_key(&y) && !sa.ocr.contains_key(&o));
    assert!(sa.clip.contains_key(&fresh));

    // 加入之后的清空照常作用于它们
    tick();
    a.store.clear(false);
    a.store.ocr_clear();
    settle(&mut [&mut a, &mut b]).await;
    assert!(b.state().clip.is_empty() && b.state().ocr.is_empty());
    assert_converged(&[&a, &b]);
}

#[tokio::test]
async fn reconnecting_device_applies_clears_made_while_it_was_away() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let mut b = Device::new(&server, "b", 0xb);
    a.connect().await.unwrap();
    b.connect().await.unwrap();
    let x = b.store.copy("b-synced");
    b.store.pin(&x, true);
    let o = b.store.ocr_add("b-synced-ocr");
    settle(&mut [&mut a, &mut b]).await;
    assert!(a.state().clip.contains_key(&x) && a.state().ocr.contains_key(&o));
    let id = b.device_id.clone();

    // b 断开期间：本机又有了新条目；a 清空了全部历史与识别历史
    b.disconnect();
    let w = b.store.copy("b-while-away");
    tick();
    a.store.clear(false);
    a.store.ocr_clear();
    a.sync_fully().await.unwrap();

    // b 以同一个 id 重连：它向这份数据发布过，离开期间的清空照常作用于本机（包括本机独有的条目）
    assert_eq!(b.connect().await.unwrap(), ConnectOutcome::Joined);
    let report = b.sync_fully().await.unwrap();
    assert!(report.new_device_id.is_none());
    assert_eq!(b.device_id, id);
    settle(&mut [&mut a, &mut b]).await;
    let s = b.state();
    assert!(!s.clip.contains_key(&x) && !s.clip.contains_key(&w), "{:?}", s.clip);
    assert!(!s.ocr.contains_key(&o));
    assert_converged(&[&a, &b]);
}

#[tokio::test]
async fn pin_after_clear_keep_pinned_brings_the_item_back() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let mut b = Device::new(&server, "b", 0xb);
    let x = a.store.copy("x");
    let y = a.store.copy("y");
    a.connect().await.unwrap();
    b.connect().await.unwrap();
    settle(&mut [&mut a, &mut b]).await;
    assert!(b.state().clip.contains_key(&x));

    // b 清空（保留置顶）；a 还不知道，随后把 x 置顶
    tick();
    b.store.clear(true);
    tick();
    a.store.pin(&x, true);
    b.sync().await.unwrap();
    a.sync().await.unwrap();
    // b 收到置顶：x 按判定式仍然存在，从作者的日志里取回正文重新出现
    b.sync().await.unwrap();
    settle(&mut [&mut a, &mut b]).await;
    assert_converged(&[&a, &b]);
    let s = b.state();
    assert!(
        s.clip.get(&x).is_some_and(|i| i.pinned && i.text == "x"),
        "{:?}",
        s.clip
    );
    assert!(!s.clip.contains_key(&y));
}

#[tokio::test]
async fn remote_pin_brings_back_items_this_device_trimmed_or_rejected() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let mut b = Device::new(&server, "b", 0xb);
    let x = a.store.copy("trimmed-on-b");
    a.connect().await.unwrap();
    b.connect().await.unwrap();
    settle(&mut [&mut a, &mut b]).await;
    assert!(b.state().clip.contains_key(&x));
    // b 的保留策略清理掉了 x（本机清理不记录、不同步）
    b.store.state.lock().clip.remove(&x);
    // b 按本机的保留策略拒收新来的未置顶条目
    b.store.reject_unpinned.store(true, Ordering::SeqCst);
    let y = a.store.copy("rejected-on-b");
    a.sync().await.unwrap();
    b.sync().await.unwrap();
    assert!(!b.state().clip.contains_key(&y));

    // a 置顶这两条：置顶的条目不受保留策略限制，b 上应以置顶状态重新出现
    tick();
    a.store.pin(&x, true);
    a.store.pin(&y, true);
    settle(&mut [&mut a, &mut b]).await;
    let s = b.state();
    assert!(
        s.clip.get(&x).is_some_and(|i| i.pinned && i.text == "trimmed-on-b"),
        "{:?}",
        s.clip
    );
    assert!(
        s.clip.get(&y).is_some_and(|i| i.pinned && i.text == "rejected-on-b"),
        "{:?}",
        s.clip
    );
    assert_converged(&[&a, &b]);
}

#[tokio::test]
async fn unpin_after_clear_keep_pinned_keeps_the_item() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let mut b = Device::new(&server, "b", 0xb);
    let x = a.store.copy("x");
    a.store.pin(&x, true);
    a.connect().await.unwrap();
    b.connect().await.unwrap();
    settle(&mut [&mut a, &mut b]).await;
    tick();
    b.store.clear(true);
    b.sync().await.unwrap();
    a.sync().await.unwrap();
    assert!(a.state().clip.contains_key(&x));
    // 清空之后取消置顶算作活动：不应在其他设备上事后删除
    tick();
    a.store.pin(&x, false);
    settle(&mut [&mut a, &mut b]).await;
    assert_converged(&[&a, &b]);
    assert!(b.state().clip.get(&x).is_some_and(|i| !i.pinned));
}

#[tokio::test]
async fn favorite_names_travel_with_the_pin_register() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let mut b = Device::new(&server, "b", 0xb);
    let x = a.store.copy("named");
    a.connect().await.unwrap();
    b.connect().await.unwrap();
    settle(&mut [&mut a, &mut b]).await;
    // 命名即收藏：名称随 `p/` 的值到达另一台设备
    tick();
    a.store.name(&x, Some("Home"));
    settle(&mut [&mut a, &mut b]).await;
    let item = b.state().clip[&x].clone();
    assert!(item.pinned);
    assert_eq!(item.name.as_deref(), Some("Home"));
    // 另一台设备改名：整值 LWW，后写的胜出
    tick();
    b.store.name(&x, Some("Office"));
    settle(&mut [&mut a, &mut b]).await;
    assert_eq!(a.state().clip[&x].name.as_deref(), Some("Office"));
    // 清除名称：仍是收藏
    tick();
    a.store.name(&x, None);
    settle(&mut [&mut a, &mut b]).await;
    assert!(b.state().clip[&x].pinned && b.state().clip[&x].name.is_none());
    // 取消收藏一并去掉名称
    tick();
    b.store.name(&x, Some("Again"));
    settle(&mut [&mut a, &mut b]).await;
    tick();
    a.store.pin(&x, false);
    settle(&mut [&mut a, &mut b]).await;
    assert_converged(&[&a, &b]);
    let item = b.state().clip[&x].clone();
    assert!(!item.pinned && item.name.is_none());
}

#[tokio::test]
async fn a_device_joining_later_receives_names_with_the_items() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let x = a.store.copy("named before b joined");
    a.store.name(&x, Some("Wi-Fi"));
    a.connect().await.unwrap();
    a.sync_fully().await.unwrap();
    // b 之后才加入：条目与名称一起导入（ClipUpsert 带着收藏状态与名称）
    let mut b = Device::new(&server, "b", 0xb);
    b.connect().await.unwrap();
    settle(&mut [&mut a, &mut b]).await;
    assert_converged(&[&a, &b]);
    assert_eq!(b.state().clip[&x].name.as_deref(), Some("Wi-Fi"));
}

#[tokio::test]
async fn deletes_and_clears_propagate_and_purge_the_log() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let mut b = Device::new(&server, "b", 0xb);
    let secret = a.store.copy("password123");
    let other = a.store.copy("other");
    a.connect().await.unwrap();
    b.connect().await.unwrap();
    settle(&mut [&mut a, &mut b]).await;
    b.store.delete(&secret);
    settle(&mut [&mut a, &mut b]).await;
    assert!(!a.state().clip.contains_key(&secret));
    // a 的日志里不再有这条正文（墓碑取代了 a 写的记录 ⇒ 清除式压缩）
    let creds = a.creds.clone().unwrap();
    let (_, records) = read_log(&server, &creds, &a.device_id).unwrap();
    assert!(
        !records.iter().any(
            |r| r.k == format!("c/{secret}") && r.v.as_ref().is_some_and(|v| v.to_string().contains("password123"))
        ),
        "删除的正文仍在服务器上"
    );
    assert!(records.iter().any(|r| r.k == format!("c/{other}")));

    tick();
    a.store.clear(false);
    settle(&mut [&mut a, &mut b]).await;
    assert!(b.state().clip.is_empty());
    let (_, records) = read_log(&server, &creds, &a.device_id).unwrap();
    assert!(!records.iter().any(|r| r.k.starts_with("c/") && r.v.is_some()));
}

#[tokio::test]
async fn retention_trims_server_history() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    a.options.retention.clip_max_items = 5;
    let pinned = a.store.copy("pinned-oldest");
    a.store.pin(&pinned, true);
    tick();
    // 超出上限的已发布条目攒够一批才为它压缩
    for i in 0..5 + wisp_sync::engine::COMPACT_RETENTION_BATCH {
        a.store.copy(&format!("item {i}"));
    }
    a.connect().await.unwrap();
    a.sync_fully().await.unwrap();
    a.sync_fully().await.unwrap();
    let creds = a.creds.clone().unwrap();
    let (_, records) = read_log(&server, &creds, &a.device_id).unwrap();
    let live: Vec<_> = records
        .iter()
        .filter(|r| r.k.starts_with("c/") && r.v.is_some())
        .collect();
    assert_eq!(live.len(), 6, "5 条未置顶 + 1 条置顶");
    assert!(live.iter().any(|r| r.k == format!("c/{pinned}")));

    // 新设备只拿到保留范围内的历史
    let mut d = Device::new(&server, "d", 0xd);
    d.connect().await.unwrap();
    d.sync_fully().await.unwrap();
    assert_eq!(d.state().clip.len(), 6);
    assert!(d.state().clip.contains_key(&pinned));
}

#[tokio::test]
async fn blobs_are_collected_only_by_their_uploader() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let mut b = Device::new(&server, "b", 0xb);
    let image = vec![7u8; 200_000];
    let h = a.store.copy_image(&image);
    a.connect().await.unwrap();
    b.connect().await.unwrap();
    settle(&mut [&mut a, &mut b]).await;
    let (vault, _) = a.creds.clone().unwrap();
    assert_eq!(blob_paths(&server, &vault, &a.device_id).len(), 1);
    assert_eq!(b.state().clip[&h].image.as_deref(), Some(image.as_slice()));

    // b 删除这张图：b 不会去删 a 的 blob，a 收到墓碑后自己回收
    b.remote.clear_ops();
    b.store.delete(&h);
    settle(&mut [&mut a, &mut b]).await;
    let a_blobs = format!("{vault}/blobs/{}/", a.device_id);
    assert!(
        !b.remote
            .ops()
            .iter()
            .any(|(op, p)| *op == Op::Delete && p.starts_with(&a_blobs)),
        "b 删除了 a 的 blob"
    );
    assert!(
        blob_paths(&server, &vault, &a.device_id).is_empty(),
        "a 没有回收不再引用的 blob"
    );

    // b 再复制同一张图：上传到 b 自己的目录，a 能拿到
    b.store.copy_image(&image);
    settle(&mut [&mut a, &mut b]).await;
    assert_eq!(blob_paths(&server, &vault, &b.device_id).len(), 1);
    assert_eq!(a.state().clip[&h].image.as_deref(), Some(image.as_slice()));
    assert_converged(&[&a, &b]);
}

#[tokio::test]
async fn recopying_an_image_after_a_lost_delete_response_uploads_it_again() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let mut b = Device::new(&server, "b", 0xb);
    let image = vec![9u8; 50_000];
    let h = a.store.copy_image(&image);
    a.connect().await.unwrap();
    b.connect().await.unwrap();
    settle(&mut [&mut a, &mut b]).await;
    let (vault, _) = a.creds.clone().unwrap();
    assert_eq!(blob_paths(&server, &vault, &a.device_id).len(), 1);

    // a 删除这张图；回收 blob 时 DELETE 已在服务器上生效，但响应丢失
    let blob_dir = format!("{vault}/blobs/{}/", a.device_id);
    a.remote.set_hook(Some(fault_once(
        move |op, p| op == Op::Delete && p.starts_with(&blob_dir),
        Fault::After(RemoteError::Timeout),
    )));
    a.store.delete(&h);
    settle(&mut [&mut a, &mut b]).await;
    a.remote.set_hook(None);
    assert!(blob_paths(&server, &vault, &a.device_id).is_empty());
    assert!(!b.state().clip.contains_key(&h));

    // 再次复制同一张图：必须重新上传，b 才拿得到原图
    tick();
    a.store.copy_image(&image);
    settle(&mut [&mut a, &mut b]).await;
    assert_eq!(blob_paths(&server, &vault, &a.device_id).len(), 1);
    assert_eq!(
        b.state().clip.get(&h).and_then(|i| i.image.clone()).as_deref(),
        Some(image.as_slice())
    );
    assert_converged(&[&a, &b]);
}

#[tokio::test]
async fn images_over_the_limit_or_switched_off_are_not_published() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let mut b = Device::new(&server, "b", 0xb);
    a.options.images_max_bytes = 1000;
    let big = a.store.copy_image(&[1u8; 5000]);
    let small = a.store.copy_image(&[2u8; 500]);
    a.connect().await.unwrap();
    b.options.content.images = false;
    b.connect().await.unwrap();
    settle(&mut [&mut a, &mut b]).await;
    let (vault, _) = a.creds.clone().unwrap();
    assert_eq!(blob_paths(&server, &vault, &a.device_id).len(), 1);
    // b 关闭了图片：图片条目被本机规则拒绝
    assert!(!b.state().clip.contains_key(&big) && !b.state().clip.contains_key(&small));
    // 打开之后，同一版本不会自动重试（拒绝按版本记下），新版本才会
    b.options.content.images = true;
    a.store.copy_image(&[2u8; 500]);
    settle(&mut [&mut a, &mut b]).await;
    assert!(b.state().clip.contains_key(&small));
    assert!(!b.state().clip.contains_key(&big));
}

#[tokio::test]
async fn ocr_history_syncs_with_thumbnails_and_clears() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let mut b = Device::new(&server, "b", 0xb);
    let one = a.store.ocr_add("first");
    tick();
    let two = a.store.ocr_add("second");
    a.connect().await.unwrap();
    b.connect().await.unwrap();
    settle(&mut [&mut a, &mut b]).await;
    assert_converged(&[&a, &b]);
    assert_eq!(b.state().ocr[&one].thumb, b"thumb:first".to_vec());
    b.store.ocr_delete(&one);
    settle(&mut [&mut a, &mut b]).await;
    assert!(!a.state().ocr.contains_key(&one) && a.state().ocr.contains_key(&two));
    tick();
    a.store.ocr_clear();
    tick();
    let three = b.store.ocr_add("third");
    settle(&mut [&mut a, &mut b]).await;
    assert_converged(&[&a, &b]);
    assert_eq!(a.state().ocr.keys().cloned().collect::<Vec<_>>(), vec![three]);
}

#[tokio::test]
async fn clipboard_shown_applies_clipboard_first_and_unavailable_store_defers() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let mut b = Device::new(&server, "b", 0xb);
    a.connect().await.unwrap();
    b.connect().await.unwrap();
    settle(&mut [&mut a, &mut b]).await;
    let x = a.store.copy("x");
    a.sync().await.unwrap();
    // 剪贴板库还没打开：记录留在「未应用」，之后照常应用
    b.store
        .clip_unavailable
        .store(true, std::sync::atomic::Ordering::SeqCst);
    b.sync().await.unwrap();
    assert!(!b.state().clip.contains_key(&x));
    b.store
        .clip_unavailable
        .store(false, std::sync::atomic::Ordering::SeqCst);
    b.remote.clear_ops();
    b.sync_reason(wisp_sync::CycleReason::ClipboardShown).await.unwrap();
    assert!(b.state().clip.contains_key(&x));
    // 不需要重新下载：记录早已在副本里
    assert_eq!(b.remote.ops().len(), 1);
}

#[tokio::test]
async fn unavailable_clipboard_store_does_not_download_images() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let mut b = Device::new(&server, "b", 0xb);
    let one = a.store.copy_image(&[1u8; 40_000]);
    let two = a.store.copy_image(&[2u8; 40_000]);
    a.connect().await.unwrap();
    a.sync_fully().await.unwrap();
    b.connect().await.unwrap();

    // 剪贴板库打不开：图片下载了也只能丢掉，不下载，也不要求立即再来一轮
    b.store.clip_unavailable.store(true, Ordering::SeqCst);
    b.remote.clear_ops();
    for _ in 0..2 {
        let report = b.sync().await.unwrap();
        assert!(!report.again, "{report:?}");
    }
    assert!(
        !b.remote
            .ops()
            .iter()
            .any(|(op, p)| *op == Op::Get && p.contains("/blobs/")),
        "{:?}",
        b.remote.ops()
    );

    b.store.clip_unavailable.store(false, Ordering::SeqCst);
    b.sync_fully().await.unwrap();
    assert!(b.state().clip.contains_key(&one) && b.state().clip.contains_key(&two));
}

#[tokio::test]
async fn switched_off_domains_keep_no_bodies_of_other_devices() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let mut b = Device::new(&server, "b", 0xb);
    a.connect().await.unwrap();
    b.options.content.clipboard = false;
    b.options.content.ocr_history = false;
    b.connect().await.unwrap();
    let x = a.store.copy("private text");
    let o = a.store.ocr_add("private ocr");
    a.sync_fully().await.unwrap();
    b.sync().await.unwrap();
    // b 不应用这两个域：sync.db 里不留其他设备条目的正文
    let db = b.db_path();
    let bodies = || {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM records WHERE d IN ('clip', 'ocr') AND substr(k, 1, 2) IN ('c/', 'o/')
               AND v IS NOT NULL",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
    };
    assert_eq!(bodies(), 0);

    // 重新打开：正文从作者的日志里取回
    b.options.content.clipboard = true;
    b.options.content.ocr_history = true;
    b.engine().reapply(&Domain::Clip).unwrap();
    b.engine().reapply(&Domain::Ocr).unwrap();
    b.sync_fully().await.unwrap();
    let s = b.state();
    assert!(s.clip.get(&x).is_some_and(|i| i.text == "private text"));
    assert!(s.ocr.get(&o).is_some_and(|i| i.text == "private ocr"));
    assert_eq!(bodies(), 0);
}

#[tokio::test]
async fn reenabling_clipboard_sync_keeps_favorites_made_while_off() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let mut b = Device::new(&server, "b", 0xb);
    let x = a.store.copy("favorite");
    let y = a.store.copy("plain");
    a.connect().await.unwrap();
    b.connect().await.unwrap();
    settle(&mut [&mut a, &mut b]).await;

    // a 关掉剪贴板同步期间收藏了 x：与桌面端一样，关闭期间不记录剪贴板改动，副本里没有这次收藏，
    // 重新加入时只能从本机枚举的 `p/`（`{"pin": true}` 对象）得知；b 随后清空（保留收藏）
    a.options.content.clipboard = false;
    a.sync().await.unwrap();
    a.store.detach();
    tick();
    a.store.pin(&x, true);
    a.store.attach(a.engine().journal().expect("日志"), a.engine().hlc());
    tick();
    b.store.clear(true);
    b.sync_fully().await.unwrap();

    // 重新打开：重新加入时 x 是本机的收藏，不被那次清空删掉，并发布到 b；未收藏的 y 照常被清掉
    a.options.content.clipboard = true;
    a.engine().reapply(&Domain::Clip).unwrap();
    settle(&mut [&mut a, &mut b]).await;
    let s = a.state();
    assert!(s.clip.get(&x).is_some_and(|i| i.pinned), "{:?}", s.clip);
    assert!(!s.clip.contains_key(&y), "{:?}", s.clip);
    assert!(b.state().clip.get(&x).is_some_and(|i| i.pinned && i.text == "favorite"));
    assert_converged(&[&a, &b]);
}

#[tokio::test]
async fn local_clear_keeping_favorites_does_not_refetch_them() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let mut b = Device::new(&server, "b", 0xb);
    let img = a.store.copy_image(&[7u8; 40_000]);
    let plain = a.store.copy("plain");
    a.store.pin(&img, true);
    a.connect().await.unwrap();
    b.connect().await.unwrap();
    settle(&mut [&mut a, &mut b]).await;
    assert!(b.state().clip.get(&img).is_some_and(|i| i.pinned));

    // b 本机清空（保留收藏）：收藏的图片本机还在，不应被当作已删除而重新下载
    tick();
    b.store.clear(true);
    b.remote.clear_ops();
    b.sync_fully().await.unwrap();
    assert!(
        !b.remote
            .ops()
            .iter()
            .any(|(op, p)| *op == Op::Get && p.contains("/blobs/")),
        "{:?}",
        b.remote.ops()
    );
    settle(&mut [&mut a, &mut b]).await;
    let s = b.state();
    assert!(s.clip.get(&img).is_some_and(|i| i.pinned), "{:?}", s.clip);
    assert!(!s.clip.contains_key(&plain));
    assert_converged(&[&a, &b]);
}
