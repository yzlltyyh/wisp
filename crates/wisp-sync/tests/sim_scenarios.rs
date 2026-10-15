//! 较长的多设备场景：离线设备回来、其他设备离线期间的压缩、钥匙串被拒、内容开关、进度与请求数、
//! 附件上传持续失败、保留策略的压缩频率。

mod sim;

use std::sync::Arc;

use serde_json::json;
use sim::{Device, assert_converged, blob_paths, head_devices, read_log, segment_paths, settle, tick};
use wisp_sync::remote::memory::{Fault, Op};
use wisp_sync::{CycleReason, Domain, LocalError, MemoryRemote, RemoteError, SyncError};

#[tokio::test]
async fn offline_device_catches_up_after_others_compact() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let mut b = Device::new(&server, "b", 0xb);
    let mut c = Device::new(&server, "c", 0xc);
    a.connect().await.unwrap();
    b.connect().await.unwrap();
    c.connect().await.unwrap();
    let doomed = a.store.copy("will be deleted");
    settle(&mut [&mut a, &mut b, &mut c]).await;
    let (vault, _) = a.creds.clone().unwrap();

    // c 离线期间：a 推送很多次（段数超过上限 ⇒ 压缩），b 删除与改动
    c.remote.set_offline(true);
    let mut compacted = false;
    for i in 0..24 {
        a.store.set(Domain::Cfg, "/general/theme", json!(format!("v{i}")));
        a.store
            .set(Domain::Glo, &format!("term{}", i % 5), json!({"target": i}));
        compacted |= a.sync().await.unwrap().compacted;
    }
    assert!(compacted, "超过段数上限后应当压缩");
    assert!(segment_paths(&server, &vault, &a.device_id).len() <= 16);
    b.store.delete(&doomed);
    b.store.set(Domain::Glo, "from-b", json!({"target": "b"}));
    b.sync().await.unwrap();
    a.sync().await.unwrap();
    // c 自己离线时的改动
    c.store.set(Domain::Glo, "from-c", json!({"target": "c"}));
    assert!(c.sync().await.is_err());

    c.remote.set_offline(false);
    settle(&mut [&mut a, &mut b, &mut c]).await;
    assert_converged(&[&a, &b, &c]);
    let s = c.state();
    assert_eq!(s.cfg["/general/theme"], json!("v23"));
    assert!(s.glo.contains_key("from-b") && s.glo.contains_key("from-c"));
    assert!(!s.clip.contains_key(&doomed));
    // 压缩后 a 的日志里每个 key 只留最新的一版
    let (_, records) = read_log(&server, a.creds.as_ref().unwrap(), &a.device_id).unwrap();
    assert_eq!(records.iter().filter(|r| r.k == "/general/theme").count(), 1);
}

#[tokio::test]
async fn compaction_while_a_peer_is_offline_keeps_winning_records() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let mut b = Device::new(&server, "b", 0xb);
    a.connect().await.unwrap();
    b.connect().await.unwrap();
    settle(&mut [&mut a, &mut b]).await;
    // b 只读到一半就离线
    a.store.set(Domain::Glo, "early", json!({"target": "1"}));
    a.sync().await.unwrap();
    b.sync().await.unwrap();
    b.remote.set_offline(true);
    for i in 0..20 {
        a.store.set(Domain::Glo, "late", json!({"target": i}));
        a.sync().await.unwrap();
    }
    // 一台全新的设备加入时只读到压缩产物，也能拿到全部胜出的记录
    let mut d = Device::new(&server, "d", 0xd);
    d.connect().await.unwrap();
    d.sync_fully().await.unwrap();
    assert_eq!(d.state().glo["early"], json!({"target": "1"}));
    assert_eq!(d.state().glo["late"], json!({"target": 19}));
    b.remote.set_offline(false);
    settle(&mut [&mut a, &mut b, &mut d]).await;
    assert_converged(&[&a, &b, &d]);
}

#[tokio::test]
async fn keychain_denied_is_persistent_and_retried_by_user_action() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let mut b = Device::new(&server, "b", 0xb);
    a.store.set_key("openai", "sk-a");
    a.connect().await.unwrap();
    b.connect().await.unwrap();
    settle(&mut [&mut a, &mut b]).await;
    assert_eq!(b.state().keys["openai"]["key"], json!("sk-a"));

    a.store.set_key("openai", "sk-rotated");
    a.sync().await.unwrap();
    b.store.keychain_denied.store(true, std::sync::atomic::Ordering::SeqCst);
    let err = b.sync().await.unwrap_err();
    assert!(matches!(err, SyncError::Local(LocalError::Keychain)));
    assert!(!err.is_transient());
    assert_eq!(b.state().keys["openai"]["key"], json!("sk-a"));
    // 后台的循环不碰钥匙串：Key 既不推送也不应用，但其他域照常
    b.options.keychain = false;
    a.store.set(Domain::Glo, "x", json!({"target": "1"}));
    a.sync().await.unwrap();
    b.sync_reason(CycleReason::LocalChange).await.unwrap();
    assert!(b.state().glo.contains_key("x"));
    // 用户操作后允许读写：积压的 Key 记录随即应用
    b.store
        .keychain_denied
        .store(false, std::sync::atomic::Ordering::SeqCst);
    b.options.keychain = true;
    b.sync().await.unwrap();
    assert_eq!(b.state().keys["openai"]["key"], json!("sk-rotated"));
    assert_converged(&[&a, &b]);
}

#[tokio::test]
async fn content_switches_hold_records_until_reenabled() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let mut b = Device::new(&server, "b", 0xb);
    a.connect().await.unwrap();
    b.options.content.settings = false;
    b.options.content.clipboard = false;
    b.connect().await.unwrap();
    settle(&mut [&mut a, &mut b]).await;

    a.store.set(Domain::Cfg, "/general/theme", json!("dark"));
    let x = a.store.copy("x");
    a.sync().await.unwrap();
    b.store.set(Domain::Glo, "local-only-for-now", json!({"target": "b"}));
    b.sync().await.unwrap();
    // 关闭期间：不应用、不推送（改动照常记录）
    assert_eq!(b.state().cfg["/general/theme"], json!("system"));
    assert!(!b.state().clip.contains_key(&x));
    a.sync().await.unwrap();
    assert!(!a.state().glo.contains_key("local-only-for-now"));

    // 重新打开：积压的远端记录应用，本机记录推送；剪贴板做一次加入
    b.options.content.settings = true;
    b.options.content.clipboard = true;
    let y = b.store.copy("y");
    b.engine().reapply(&Domain::Clip).unwrap();
    settle(&mut [&mut a, &mut b]).await;
    assert_converged(&[&a, &b]);
    assert!(a.state().glo.contains_key("local-only-for-now"));
    assert!(a.state().clip.contains_key(&y) && b.state().clip.contains_key(&x));
}

#[tokio::test]
async fn status_devices_and_progress() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let mut b = Device::new(&server, "b", 0xb);
    for i in 0..30 {
        a.store.copy(&format!("item {i}"));
    }
    a.connect().await.unwrap();
    let status = a.engine().status().unwrap();
    assert!(status.unlocked && status.joining);
    let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&seen);
    let progress = move |p: wisp_sync::Progress| sink.lock().push(p);
    let report = a
        .engine()
        .sync_cycle(&a.remote, &a.store, CycleReason::Connect, &a.options, &progress)
        .await
        .unwrap();
    assert_eq!(report.pushed, 30);
    let seen = seen.lock().clone();
    assert!(
        seen.last().is_some_and(|p| p.done == p.total && p.total >= 30),
        "{seen:?}"
    );
    assert!(!a.engine().status().unwrap().joining);

    b.connect().await.unwrap();
    b.sync_fully().await.unwrap();
    a.sync().await.unwrap();
    let devices = a.engine().devices().unwrap();
    assert_eq!(devices.len(), 2);
    assert!(devices.iter().any(|d| d.this_device && d.id == a.device_id));
    assert!(
        devices
            .iter()
            .any(|d| !d.this_device && d.id == b.device_id && d.name == "b")
    );
    assert_eq!(a.engine().status().unwrap().device_count, 2);
    let (vault, _) = a.creds.clone().unwrap();
    assert_eq!(head_devices(&server, &vault).len(), 2);
}

#[tokio::test]
async fn upload_budget_splits_large_image_backlogs_across_cycles() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    // 12 张 5 MB 的图：超过单轮 50 MB 的额度
    for i in 0..12u8 {
        a.store.copy_image(&vec![i; 5 * 1024 * 1024]);
        tick();
    }
    a.connect().await.unwrap();
    let first = a.sync().await.unwrap();
    assert!(first.again && first.pending, "{first:?}");
    assert!(first.bytes_up <= wisp_sync::engine::CYCLE_BLOB_BUDGET_BYTES + 6 * 1024 * 1024);
    let rest = a.sync_fully().await.unwrap();
    assert!(!rest.pending);
    let (vault, _) = a.creds.clone().unwrap();
    assert_eq!(sim::blob_paths(&server, &vault, &a.device_id).len(), 12);
}

#[tokio::test]
async fn reapply_restores_config_after_a_local_reset() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let mut b = Device::new(&server, "b", 0xb);
    a.store.set(Domain::Cfg, "/general/theme", json!("dark"));
    a.store.set(
        Domain::Cfg,
        "/providers/openai",
        json!({"base_url": "https://api.openai.com"}),
    );
    a.store.set_key("openai", "sk-a");
    a.connect().await.unwrap();
    b.connect().await.unwrap();
    settle(&mut [&mut a, &mut b]).await;

    // b 的配置文件被删掉、以默认值重建（不记录为本机改动），会话钥匙串也丢了 Key
    {
        let mut s = b.store.state.lock();
        s.cfg = sim::cfg_defaults();
        s.keys.clear();
    }
    b.engine().reapply(&Domain::Cfg).unwrap();
    b.engine().reapply(&Domain::Key).unwrap();
    b.sync().await.unwrap();
    let s = b.state();
    assert_eq!(s.cfg["/general/theme"], json!("dark"));
    assert!(s.cfg.contains_key("/providers/openai"));
    assert_eq!(s.keys["openai"]["key"], json!("sk-a"));
    // 默认值没有被当成本机改动发布出去
    a.sync().await.unwrap();
    assert_eq!(a.state().cfg["/general/theme"], json!("dark"));
    assert_converged(&[&a, &b]);
}

#[tokio::test]
async fn failing_attachment_upload_backs_off_instead_of_rerunning() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let mut b = Device::new(&server, "b", 0xb);
    let img = a.store.copy_image(&[3u8; 64 * 1024]);
    tick();
    let text = a.store.copy("text");
    a.connect().await.unwrap();
    b.connect().await.unwrap();
    // 这张图的上传一直失败（例如网关对大文件返回 503）：每一轮都报告暂时性失败，让调用方退避，
    // 而不是成功并要求立即再来一轮
    a.remote.set_hook(Some(Arc::new(|op, p: &str| {
        (op == Op::Put && p.ends_with(".blob")).then_some(Fault::Before(RemoteError::Status(503)))
    })));
    for _ in 0..3 {
        let err = a.sync().await.unwrap_err();
        assert!(err.is_transient(), "{err:?}");
        assert!(matches!(err, SyncError::Remote(RemoteError::Status(503))), "{err:?}");
    }
    // 图片留在待上传；其余工作照常提交
    assert!(a.engine().status().unwrap().pending);
    b.sync().await.unwrap();
    assert!(b.state().clip.contains_key(&text) && !b.state().clip.contains_key(&img));

    a.remote.set_hook(None);
    let report = a.sync().await.unwrap();
    assert!(!report.again && !report.pending, "{report:?}");
    b.sync().await.unwrap();
    assert!(b.state().clip.get(&img).is_some_and(|i| i.image.is_some()));
}

#[tokio::test]
async fn turning_clipboard_sync_off_removes_this_devices_records_from_the_server() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    a.connect().await.unwrap();
    let x = a.store.copy("secret");
    a.store.ocr_add("recognized");
    a.sync_fully().await.unwrap();
    let creds = a.creds.clone().unwrap();
    let (_, records) = read_log(&server, &creds, &a.device_id).unwrap();
    assert!(records.iter().any(|r| r.k == format!("c/{x}")));

    // 关闭剪贴板与识别历史的同步：下一轮就把本机上传过的记录从服务器上移除，不等别的原因触发压缩
    a.options.content.clipboard = false;
    a.options.content.ocr_history = false;
    let report = a.sync().await.unwrap();
    assert!(report.compacted);
    let (_, records) = read_log(&server, &creds, &a.device_id).unwrap();
    assert!(!records.iter().any(|r| r.d.is_content()), "{records:?}");
    // 移除之后不再反复压缩
    assert!(!a.sync().await.unwrap().compacted);

    let mut c = Device::new(&server, "c", 0xc);
    c.connect().await.unwrap();
    c.sync_fully().await.unwrap();
    assert!(c.state().clip.is_empty() && c.state().ocr.is_empty());
}

#[tokio::test]
async fn reenabling_clipboard_sync_republishes_history_removed_while_off() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let x1 = a.store.copy("x1");
    let x2 = a.store.copy("x2");
    let image = vec![5u8; 30_000];
    let img = a.store.copy_image(&image);
    a.connect().await.unwrap();
    a.sync_fully().await.unwrap();
    let creds = a.creds.clone().unwrap();

    // 关闭期间压缩把本机的剪贴板记录从日志里移除，图片也被回收
    a.options.content.clipboard = false;
    assert!(a.sync().await.unwrap().compacted);
    assert!(blob_paths(&server, &creds.0, &a.device_id).is_empty());

    // 重新打开：对该域重新加入，本机还有的条目重新发布（图片重新上传）
    a.options.content.clipboard = true;
    a.engine().reapply(&Domain::Clip).unwrap();
    a.sync_fully().await.unwrap();
    let (_, records) = read_log(&server, &creds, &a.device_id).unwrap();
    for h in [&x1, &x2, &img] {
        assert!(records.iter().any(|r| r.k == format!("c/{h}") && r.v.is_some()), "{h}");
    }
    assert_eq!(blob_paths(&server, &creds.0, &a.device_id).len(), 1);

    let mut d = Device::new(&server, "d", 0xd);
    d.connect().await.unwrap();
    d.sync_fully().await.unwrap();
    let s = d.state();
    assert!(s.clip.contains_key(&x1) && s.clip.contains_key(&x2));
    assert_eq!(s.clip.get(&img).and_then(|i| i.image.clone()), Some(image));
}

#[tokio::test]
async fn history_at_the_retention_cap_does_not_rewrite_the_log_on_every_copy() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    a.options.retention.clip_max_items = 5;
    for i in 0..5 {
        a.store.copy(&format!("item {i}"));
    }
    a.connect().await.unwrap();
    a.sync_fully().await.unwrap();
    // 历史已在上限：每次复制都会挤出一条已发布的旧条目，但不必每次都整体重写日志
    let rounds = 10;
    for i in 0..rounds {
        tick();
        a.store.copy(&format!("more {i}"));
        let report = a.sync().await.unwrap();
        assert!(!report.compacted, "第 {i} 次复制触发了压缩");
    }
    let creds = a.creds.clone().unwrap();
    let (_, records) = read_log(&server, &creds, &a.device_id).unwrap();
    let live = records
        .iter()
        .filter(|r| r.k.starts_with("c/") && r.v.is_some())
        .count();
    assert!(live <= 5 + rounds, "{live}");
}
