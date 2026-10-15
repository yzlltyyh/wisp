//! 失败与身份：head 写入的响应丢失、推送期间的新改动、克隆检测、断开后重连（连接期间的新改动、
//! 断开期间改过的配置）、本机段在服务器上丢失、并发创建。

mod sim;

use std::sync::Arc;

use serde_json::json;
use sim::{Device, assert_converged, fault_once, head_devices, read_log, segment_paths, settle, tick};
use wisp_sync::remote::memory::{Fault, Op};
use wisp_sync::{ConnectOutcome, Domain, MemoryRemote, RemoteError, SyncError};

async fn pair(server: &MemoryRemote) -> (Device, Device) {
    let mut a = Device::new(server, "a", 0xa);
    let mut b = Device::new(server, "b", 0xb);
    a.connect().await.expect("测试步骤");
    b.connect().await.expect("测试步骤");
    settle(&mut [&mut a, &mut b]).await;
    (a, b)
}

#[tokio::test]
async fn lost_head_put_response_commits_on_next_cycle() {
    let server = MemoryRemote::new();
    let (mut a, mut b) = pair(&server).await;
    let a_id = a.device_id.clone();
    let (vault, _) = a.creds.clone().unwrap();

    a.store.set(Domain::Cfg, "/general/theme", json!("dark"));
    let head = format!("{vault}.{a_id}.head");
    a.remote.set_hook(Some(fault_once(
        move |op, p| op == Op::Put && p == head,
        Fault::After(RemoteError::Timeout),
    )));
    let err = a.sync().await.unwrap_err();
    assert!(matches!(err, SyncError::Remote(RemoteError::Timeout)), "{err:?}");
    // head 实际已写入：其他设备已经能看到
    b.sync().await.unwrap();
    assert_eq!(b.state().cfg["/general/theme"], json!("dark"));

    // 下一轮重新读本机 head，发现那一段在「进行中的写入」里：补做提交，而不是当成克隆
    let report = a.sync().await.unwrap();
    assert!(report.new_device_id.is_none());
    assert!(!report.pending);
    assert_eq!(a.device_id, a_id);
    assert_eq!(head_devices(&server, &vault).len(), 2);
    assert!(!a.engine().status().unwrap().pending);

    // 重启后的首轮同样不会误判
    a.store.set(Domain::Cfg, "/general/theme", json!("darker"));
    let head = format!("{vault}.{a_id}.head");
    a.remote.set_hook(Some(fault_once(
        move |op, p| op == Op::Put && p == head,
        Fault::After(RemoteError::Connect),
    )));
    a.sync().await.unwrap_err();
    a.restart();
    let report = a.sync().await.unwrap();
    assert!(report.new_device_id.is_none());
    settle(&mut [&mut a, &mut b]).await;
    assert_converged(&[&a, &b]);
    assert_eq!(head_devices(&server, &vault).len(), 2);
}

#[tokio::test]
async fn failed_segment_put_retries_with_a_new_number() {
    let server = MemoryRemote::new();
    let (mut a, mut b) = pair(&server).await;
    let (vault, _) = a.creds.clone().unwrap();
    a.store.set(Domain::Glo, "x", json!({"target": "1"}));
    // 段写入成功但响应丢失，随后 head 写入失败：段成了孤儿
    let seg_prefix = format!("{vault}/log/{}/", a.device_id);
    a.remote.set_hook(Some(fault_once(
        move |op, p| op == Op::Put && p.starts_with(&seg_prefix),
        Fault::After(RemoteError::Timeout),
    )));
    a.sync().await.unwrap_err();
    let orphans = segment_paths(&server, &vault, &a.device_id);
    a.sync().await.unwrap();
    let after = segment_paths(&server, &vault, &a.device_id);
    // 重试用了新的编号，没有覆盖之前的文件
    assert!(after.len() > orphans.len());
    for p in &orphans {
        assert!(after.contains(p));
    }
    b.sync().await.unwrap();
    assert_eq!(b.state().glo["x"], json!({"target": "1"}));
}

#[tokio::test]
async fn edits_during_push_stay_pending() {
    let server = MemoryRemote::new();
    let (mut a, mut b) = pair(&server).await;
    let (vault, _) = a.creds.clone().unwrap();
    a.store.set(Domain::Cfg, "/general/theme", json!("v1"));
    // 段 PUT 期间用户又改了同一个 key
    let store = Arc::clone(&a.store);
    let seg_prefix = format!("{vault}/log/{}/", a.device_id);
    let fired = std::sync::atomic::AtomicBool::new(false);
    a.remote.set_hook(Some(Arc::new(move |op, p: &str| {
        if op == Op::Put && p.starts_with(&seg_prefix) && !fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
            store.set(Domain::Cfg, "/general/theme", json!("v2"));
        }
        None
    })));
    let report = a.sync().await.unwrap();
    assert!(report.pending, "推送期间的新改动必须保持待上传");
    a.remote.set_hook(None);
    a.sync().await.unwrap();
    b.sync().await.unwrap();
    assert_eq!(b.state().cfg["/general/theme"], json!("v2"));
    assert_converged(&[&a, &b]);
}

/// 复制 a 的数据目录得到一台「克隆」设备（同一个 device_id、同一份副本与凭据）
fn clone_of(server: &MemoryRemote, a: &mut Device, label: &str) -> Device {
    a.store.detach();
    let engine = a.engine.take().expect("测试步骤");
    engine.close().expect("测试步骤");
    let mut c = Device::with_state(server, label, 0xc, a.state());
    c.store.detach();
    c.engine.take().expect("测试步骤").close().expect("测试步骤");
    for suffix in ["", "-wal", "-shm"] {
        let from = a.dir.path().join(format!("sync.db{suffix}"));
        let to = c.dir.path().join(format!("sync.db{suffix}"));
        let _ = std::fs::remove_file(&to);
        if from.exists() {
            std::fs::copy(&from, &to).expect("测试步骤");
        }
    }
    c.device_id = a.device_id.clone();
    c.creds = a.creds.clone();
    c.restart();
    a.restart();
    c
}

#[tokio::test]
async fn clone_detection_moves_the_copy_to_a_new_id() {
    let server = MemoryRemote::new();
    let (mut a, mut b) = pair(&server).await;
    a.store.set(Domain::Glo, "shared", json!({"target": "0"}));
    a.sync().await.unwrap();
    let mut c = clone_of(&server, &mut a, "c");
    let original = a.device_id.clone();
    let (vault, _) = a.creds.clone().unwrap();

    // 原机先写；克隆的首轮读到自己 id 下陌生的段 ⇒ 换新 id
    a.store.set(Domain::Glo, "from-a", json!({"target": "a"}));
    a.sync().await.unwrap();
    c.store.set(Domain::Glo, "from-c", json!({"target": "c"}));
    let report = c.sync().await.unwrap();
    assert!(report.new_device_id.is_some());
    assert_ne!(c.device_id, original);
    assert_eq!(a.device_id, original);

    settle(&mut [&mut a, &mut b, &mut c]).await;
    assert_converged(&[&a, &b, &c]);
    let s = b.state();
    assert!(s.glo.contains_key("from-a") && s.glo.contains_key("from-c") && s.glo.contains_key("shared"));
    assert_eq!(head_devices(&server, &vault).len(), 3);
}

#[tokio::test]
async fn clones_writing_at_the_same_time_split_into_two_ids() {
    let server = MemoryRemote::new();
    let (mut a, mut b) = pair(&server).await;
    a.sync().await.unwrap();
    let mut c = clone_of(&server, &mut a, "c");
    let original = a.device_id.clone();
    let (vault, _) = a.creds.clone().unwrap();
    // 两边各自完成首轮核对（都还没写），然后在同一时刻推送：两边都以为 head 是自己的
    a.sync().await.unwrap();
    c.sync().await.unwrap();
    a.store.set(Domain::Glo, "from-a", json!({"target": "a"}));
    c.store.set(Domain::Glo, "from-c", json!({"target": "c"}));
    server.set_latency(Some(std::time::Duration::from_millis(2)));
    let (ra, rc) = tokio::join!(a.sync(), c.sync());
    ra.unwrap();
    rc.unwrap();
    server.set_latency(None);
    // b 读到的是后写的那份 head
    b.sync().await.unwrap();
    // head 被对方覆盖的那一台在下一轮发现并换新 id；另一台保留原来的 id
    a.sync().await.unwrap();
    c.sync().await.unwrap();
    let changed = [&a, &c].iter().filter(|d| d.device_id != original).count();
    assert_eq!(changed, 1, "a={} c={}", a.device_id, c.device_id);

    settle(&mut [&mut a, &mut b, &mut c]).await;
    assert_converged(&[&a, &b, &c]);
    let s = b.state();
    assert!(s.glo.contains_key("from-a") && s.glo.contains_key("from-c"));
    assert_eq!(head_devices(&server, &vault).len(), 3);
}

#[tokio::test]
async fn reconnect_after_losing_local_state_adopts_own_log() {
    let server = MemoryRemote::new();
    let (mut a, mut b) = pair(&server).await;
    let (vault, _) = a.creds.clone().unwrap();
    a.store.set(Domain::Cfg, "/general/theme", json!("dark"));
    a.store.set(Domain::Glo, "a-term", json!({"target": "1"}));
    let h = a.store.copy("a-clip");
    a.sync_fully().await.unwrap();
    b.sync_fully().await.unwrap();
    let id = a.device_id.clone();
    let before = segment_paths(&server, &vault, &id);
    assert!(!before.is_empty());

    // 断开（删除 sync.db），本机数据与 device_id 保留，再连接
    a.disconnect();
    assert_eq!(a.connect().await.unwrap(), ConnectOutcome::Joined);
    let report = a.sync_fully().await.unwrap();
    assert!(report.new_device_id.is_none(), "不应被当成克隆");
    assert_eq!(a.device_id, id);
    assert_eq!(head_devices(&server, &vault).len(), 2);
    // 旧日志没有被新写入覆盖
    let (_, records) = read_log(&server, a.creds.as_ref().unwrap(), &id).unwrap();
    assert!(records.iter().any(|r| r.k == "a-term"));
    assert!(records.iter().any(|r| r.k == format!("c/{h}")));

    a.store.set(Domain::Glo, "after", json!({"target": "2"}));
    settle(&mut [&mut a, &mut b]).await;
    assert_converged(&[&a, &b]);
    assert!(b.state().glo.contains_key("after"));
    assert_eq!(b.state().cfg["/general/theme"], json!("dark"));
}

#[tokio::test]
async fn concurrent_create_race_ends_in_one_vault() {
    for round in 0..3u64 {
        let server = MemoryRemote::new();
        server.set_latency(Some(std::time::Duration::from_millis(1 + round)));
        let mut a = Device::new(&server, "a", 0xa);
        let mut b = Device::new(&server, "b", 0xb);
        a.store.set(Domain::Glo, "from-a", json!({"target": "a"}));
        b.store.set(Domain::Glo, "from-b", json!({"target": "b"}));
        let (ra, rb) = tokio::join!(a.connect(), b.connect());
        ra.unwrap();
        rb.unwrap();
        // 至多一台在之后发现自己创建的 vault 被覆盖（vault_reset），按提示重新连接即可
        for d in [&mut a, &mut b] {
            if let Err(SyncError::VaultReset) = d.sync_fully().await {
                d.disconnect();
                assert_eq!(d.connect().await.unwrap(), ConnectOutcome::Joined);
            }
        }
        server.set_latency(None);
        settle(&mut [&mut a, &mut b]).await;
        assert_eq!(a.creds.as_ref().unwrap().0, b.creds.as_ref().unwrap().0);
        assert_converged(&[&a, &b]);
        let s = a.state();
        assert!(s.glo.contains_key("from-a") && s.glo.contains_key("from-b"));
        tick();
    }
}

#[tokio::test]
async fn a_copy_recorded_while_reconnecting_does_not_split_the_device() {
    let server = MemoryRemote::new();
    let (mut a, mut b) = pair(&server).await;
    let (vault, _) = a.creds.clone().unwrap();
    let before = a.store.copy("before");
    a.store.set(Domain::Glo, "a-term", json!({"target": "1"}));
    settle(&mut [&mut a, &mut b]).await;
    let id = a.device_id.clone();

    a.disconnect();
    // 连接过程中（清空副本之后、核对本机 head 之前）日志写入线程又记下了一次复制
    let store = Arc::clone(&a.store);
    let lists = std::sync::atomic::AtomicUsize::new(0);
    a.remote.set_hook(Some(Arc::new(move |op, p: &str| {
        if op == Op::List && p.is_empty() && lists.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
            store.copy("during");
        }
        None
    })));
    assert_eq!(a.connect().await.unwrap(), ConnectOutcome::Joined);
    a.remote.set_hook(None);
    let report = a.sync_fully().await.unwrap();
    assert!(report.new_device_id.is_none(), "不应被当成克隆");
    assert_eq!(a.device_id, id);
    assert_eq!(head_devices(&server, &vault).len(), 2);

    settle(&mut [&mut a, &mut b]).await;
    assert_converged(&[&a, &b]);
    let s = b.state();
    assert!(s.clip.contains_key(&before) && s.clip.contains_key(&sim::text_hash_hex("during")));
    assert!(s.glo.contains_key("a-term"));
    assert_eq!(b.engine().devices().unwrap().len(), 2);
}

#[tokio::test]
async fn reconnecting_publishes_a_value_changed_while_disconnected_over_its_own_floor_record() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    a.connect().await.unwrap();
    // b 加入时带着非默认的主题：服务器上只有默认值的底值，b 的值以底值发布
    let mut b = Device::new(&server, "b", 0xb);
    b.store.state.lock().cfg.insert("/general/theme".into(), json!("dark"));
    b.connect().await.unwrap();
    settle(&mut [&mut a, &mut b]).await;
    assert_eq!(a.state().cfg["/general/theme"], json!("dark"));

    // 断开期间改了主题（同步关闭时不记录），之后以同一个 id 重连
    b.disconnect();
    b.store.state.lock().cfg.insert("/general/theme".into(), json!("light"));
    assert_eq!(b.connect().await.unwrap(), ConnectOutcome::Joined);
    settle(&mut [&mut a, &mut b]).await;
    assert_eq!(b.state().cfg["/general/theme"], json!("light"));
    assert_converged(&[&a, &b]);
}

#[tokio::test]
async fn compaction_republishes_records_of_a_lost_own_segment() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    a.connect().await.unwrap();
    a.sync_fully().await.unwrap();
    let (vault, _) = a.creds.clone().unwrap();
    let before = segment_paths(&server, &vault, &a.device_id);
    a.store.set(Domain::Glo, "keep", json!({"target": "1"}));
    a.sync().await.unwrap();
    let added: Vec<String> = segment_paths(&server, &vault, &a.device_id)
        .into_iter()
        .filter(|p| !before.contains(p))
        .collect();
    assert_eq!(added.len(), 1);
    // 这一段在服务器上丢了（被删除、服务器回滚等）
    assert!(server.raw_remove(&added[0]));

    // 一次删除触发清除式压缩：丢失段里仍然胜出的记录重新发布，而不是随压缩一起消失
    a.store.set(Domain::Glo, "gone", json!({"target": "2"}));
    a.store.remove(Domain::Glo, "gone");
    let report = a.sync().await.unwrap();
    assert!(report.compacted);
    assert!(!report.pending, "{report:?}");

    let mut d = Device::new(&server, "d", 0xd);
    d.connect().await.unwrap();
    d.sync_fully().await.unwrap();
    assert_eq!(d.state().glo.get("keep"), Some(&json!({"target": "1"})));
    settle(&mut [&mut a, &mut d]).await;
    assert_converged(&[&a, &d]);
}
