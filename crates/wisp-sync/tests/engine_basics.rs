//! 引擎的基本流程：创建、加入、恢复、空闲检查的请求数、测试连接、清除服务器数据。

mod sim;

use serde_json::json;
use sim::{Device, assert_converged, head_devices, settle};
use wisp_sync::remote::memory::{Metadata, Op};
use wisp_sync::{ConnectOutcome, Domain, MemoryRemote, SyncError, probe, reset_remote};

#[tokio::test]
async fn create_then_join_merges_both_sides() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    a.store.set(Domain::Cfg, "/general/theme", json!("dark"));
    a.store.set(
        Domain::Cfg,
        "/providers/openai",
        json!({"base_url": "https://api.openai.com"}),
    );
    a.store.set_key("openai", "sk-a");
    a.store.set(Domain::Glo, "hello", json!({"target": "你好"}));
    assert_eq!(a.connect().await.unwrap(), ConnectOutcome::Created);
    a.sync_fully().await.unwrap();

    let mut b = Device::new(&server, "b", 0xb);
    // b 的非默认值：服务器上是 a 的默认值（底值），b 的值胜出
    b.store.set(Domain::Cfg, "/translate/primary_lang", json!("ja"));
    // 双方都改过：服务器（真实 t）胜出
    b.store.set(Domain::Cfg, "/general/theme", json!("light"));
    b.store.set(
        Domain::Cfg,
        "/providers/local",
        json!({"base_url": "http://localhost:11434"}),
    );
    b.store.set_key("local", "sk-b");
    b.store.set(Domain::Glo, "world", json!({"target": "世界"}));
    assert_eq!(b.connect().await.unwrap(), ConnectOutcome::Joined);
    settle(&mut [&mut a, &mut b]).await;

    assert_converged(&[&a, &b]);
    let s = a.state();
    assert_eq!(s.cfg["/general/theme"], json!("dark"));
    assert_eq!(s.cfg["/translate/primary_lang"], json!("ja"));
    assert!(s.cfg.contains_key("/providers/openai") && s.cfg.contains_key("/providers/local"));
    assert_eq!(s.keys.len(), 2);
    assert_eq!(s.glo.len(), 2);
    let (vault, _) = a.creds.clone().unwrap();
    assert_eq!(head_devices(&server, &vault).len(), 2);
}

#[tokio::test]
async fn idle_cycle_is_one_request() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let mut b = Device::new(&server, "b", 0xb);
    a.store.copy("hello");
    a.connect().await.unwrap();
    b.connect().await.unwrap();
    settle(&mut [&mut a, &mut b]).await;

    for d in [&mut a, &mut b] {
        d.remote.clear_ops();
        let report = d.sync().await.unwrap();
        assert_eq!(d.remote.ops().len(), 1, "{}: {:?}", d.label, d.remote.ops());
        assert_eq!(d.remote.ops()[0].0, Op::List);
        assert_eq!(report.requests, 1);
        assert!(!report.pending && !report.again);
    }
    // 另一台设备有改动：只多出取 head 与取段的请求
    a.store.set(Domain::Cfg, "/general/theme", json!("dark"));
    a.sync().await.unwrap();
    b.remote.clear_ops();
    b.sync().await.unwrap();
    assert_eq!(b.remote.ops().len(), 3, "{:?}", b.remote.ops());
    assert_eq!(b.state().cfg["/general/theme"], json!("dark"));
}

#[tokio::test]
async fn restart_restores_without_network_and_continues() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let mut b = Device::new(&server, "b", 0xb);
    a.connect().await.unwrap();
    b.connect().await.unwrap();
    settle(&mut [&mut a, &mut b]).await;

    a.restart();
    a.store.set(Domain::Glo, "after-restart", json!({"target": "x"}));
    // 重启后的第一轮多读一次本机 head（核对），之后回到 1 个请求
    a.remote.clear_ops();
    a.sync().await.unwrap();
    assert!(
        a.remote
            .ops()
            .iter()
            .any(|(op, p)| *op == Op::Get && p.ends_with(".head"))
    );
    a.remote.clear_ops();
    a.sync().await.unwrap();
    assert_eq!(a.remote.ops().len(), 1);
    b.sync().await.unwrap();
    assert!(b.state().glo.contains_key("after-restart"));

    // 「恢复」：副本属于服务器上的 vault，只更新凭据
    let outcome = a.connect().await.unwrap();
    assert_eq!(outcome, ConnectOutcome::Restored);
    a.sync().await.unwrap();
    assert_converged(&[&a, &b]);
}

#[tokio::test]
async fn wrong_passphrase_and_weak_passphrase() {
    let server = MemoryRemote::new();
    let a = Device::new(&server, "a", 0xa);
    let err = a
        .engine()
        .connect(&a.remote, &a.store, "short", &[], &a.options, sim::noop_progress())
        .await
        .unwrap_err();
    assert!(matches!(err, SyncError::PassphraseRule(_)));
    let err = a
        .engine()
        .connect(
            &a.remote,
            &a.store,
            "webdav-password-1",
            &["WEBDAV-PASSWORD-1"],
            &a.options,
            sim::noop_progress(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, SyncError::PassphraseRule(_)));
    a.engine()
        .connect(&a.remote, &a.store, sim::PASS, &[], &a.options, sim::noop_progress())
        .await
        .unwrap();

    let b = Device::new(&server, "b", 0xb);
    let err = b
        .engine()
        .connect(
            &b.remote,
            &b.store,
            "another passphrase",
            &[],
            &b.options,
            sim::noop_progress(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, SyncError::Passphrase));
    // 口令错误什么都不写
    assert!(b.remote.writes().is_empty(), "{:?}", b.remote.writes());
}

#[tokio::test]
async fn probe_reports_server_state() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let report = probe(&a.remote, None).await.unwrap();
    assert!(report.vault.is_none());
    // 探测文件写完即删
    assert!(server.paths().is_empty());

    a.connect().await.unwrap();
    a.sync().await.unwrap();
    let (vault, _) = a.creds.clone().unwrap();
    let report = probe(&a.remote, Some(&vault)).await.unwrap();
    let summary = report.vault.unwrap();
    assert!(summary.supported && summary.restorable);
    assert_eq!(summary.device_count, 1);
    assert!(summary.last_modified_ms.is_some());
    let other = probe(&a.remote, Some("0000000000000000")).await.unwrap();
    assert!(!other.vault.unwrap().restorable);
    assert_eq!(
        wisp_sync::Replica::peek_vault_id(&a.db_path()).unwrap().as_deref(),
        Some(vault.as_str())
    );
}

#[tokio::test]
async fn reset_then_old_device_reports_vault_reset_and_writes_nothing() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let mut b = Device::new(&server, "b", 0xb);
    a.connect().await.unwrap();
    b.connect().await.unwrap();
    b.store.set(Domain::Glo, "b-before", json!({"target": "x"}));
    settle(&mut [&mut a, &mut b]).await;
    let (old_vault, _) = a.creds.clone().unwrap();

    reset_remote(&a.remote).await.unwrap();
    assert!(server.paths().is_empty(), "{:?}", server.paths());
    // a 用同一个口令重新开始
    a.disconnect();
    assert_eq!(a.connect().await.unwrap(), ConnectOutcome::Created);
    a.sync_fully().await.unwrap();
    let (new_vault, _) = a.creds.clone().unwrap();
    assert_ne!(old_vault, new_vault);

    // b 错过了清除：发现 vault 被替换，不写任何东西
    b.store.set(Domain::Glo, "b-offline", json!({"target": "y"}));
    for _ in 0..2 {
        b.remote.clear_ops();
        let err = b.sync().await.unwrap_err();
        assert!(matches!(err, SyncError::VaultReset), "{err:?}");
        assert!(b.remote.writes().is_empty(), "{:?}", b.remote.writes());
    }
    assert!(head_devices(&server, &old_vault).is_empty());

    // b 重新连接：按加入处理，本机的术语都保留并发布
    b.disconnect();
    assert_eq!(b.connect().await.unwrap(), ConnectOutcome::Joined);
    settle(&mut [&mut a, &mut b]).await;
    assert_converged(&[&a, &b]);
    assert!(a.state().glo.contains_key("b-offline") && a.state().glo.contains_key("b-before"));
}

#[tokio::test]
async fn vault_json_removed_is_vault_reset() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    a.connect().await.unwrap();
    a.sync().await.unwrap();
    server.raw_remove(wisp_sync::layout::VAULT_FILE);
    a.store.set(Domain::Glo, "x", json!({"target": "y"}));
    a.remote.clear_ops();
    assert!(matches!(a.sync().await.unwrap_err(), SyncError::VaultReset));
    assert!(a.remote.writes().is_empty());
}

#[tokio::test]
async fn servers_without_etags_or_metadata_still_converge() {
    for metadata in [Metadata::NoEtag, Metadata::None] {
        let server = MemoryRemote::new();
        server.set_metadata(metadata);
        let mut a = Device::new(&server, "a", 0xa);
        let mut b = Device::new(&server, "b", 0xb);
        a.store.set(Domain::Glo, "x", json!({"target": "1"}));
        a.connect().await.unwrap();
        b.connect().await.unwrap();
        settle(&mut [&mut a, &mut b]).await;
        b.store.set(Domain::Glo, "y", json!({"target": "2"}));
        settle(&mut [&mut a, &mut b]).await;
        assert_converged(&[&a, &b]);
        assert!(a.state().glo.contains_key("y"));
        // 没有变化时：只有大小与修改时间 ⇒ 仍是 1 个请求；什么都没有 ⇒ 每轮重读 vault.json 与其他设备的 head
        a.remote.clear_ops();
        a.sync().await.unwrap();
        let expected = if metadata == Metadata::NoEtag { 1 } else { 3 };
        assert_eq!(a.remote.ops().len(), expected, "{metadata:?}: {:?}", a.remote.ops());
    }
}

#[tokio::test]
async fn a_corrupt_peer_is_skipped_without_blocking_others() {
    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    let mut b = Device::new(&server, "b", 0xb);
    a.connect().await.unwrap();
    b.connect().await.unwrap();
    settle(&mut [&mut a, &mut b]).await;
    let (vault, _) = a.creds.clone().unwrap();
    // 一个用别的密钥写的 head（例如旧 vault 的残留被改了名）
    server.raw_put(
        &wisp_sync::layout::head_path(&vault, "00000000000000ff"),
        bytes::Bytes::from_static(b"WSY1\x01\x00garbage-garbage-garbage-garbage"),
    );
    b.store.set(Domain::Glo, "still-flows", json!({"target": "1"}));
    b.sync().await.unwrap();
    let report = a.sync().await.unwrap();
    assert_eq!(report.corrupt_devices, 1);
    assert!(a.state().glo.contains_key("still-flows"));
    // 坏掉的 head 没有变化：下一轮不再重试
    a.remote.clear_ops();
    let report = a.sync().await.unwrap();
    assert_eq!(report.corrupt_devices, 0);
    assert_eq!(a.remote.ops().len(), 1);
}

#[tokio::test]
async fn records_from_newer_versions_are_kept_but_not_applied() {
    use wisp_sync::crypto::{self, FileKind, Keys};
    use wisp_sync::model::{
        FORMAT_VERSION, HeadPayload, Record, SchemaVersions, SegmentEntry, SegmentPayload, segment_hash,
    };

    let server = MemoryRemote::new();
    let mut a = Device::new(&server, "a", 0xa);
    a.connect().await.unwrap();
    a.sync().await.unwrap();
    let (vault, master) = a.creds.clone().unwrap();
    let keys = Keys::derive(&master, &vault).unwrap();
    // 一台「新版本」设备写了一个本版本不认识的域，以及一个认识的域里的记录
    let dev = "00000000000000fe";
    let records = vec![
        Record {
            d: Domain::parse("future"),
            k: "something".into(),
            t: wisp_sync::Hlc::from_wall_ms(1),
            v: Some(json!({"x": 1})),
            b: Vec::new(),
        },
        Record {
            d: Domain::Glo,
            k: "from-future".into(),
            t: wisp_sync::Hlc::from_wall_ms(2),
            v: Some(json!({"target": "f", "extra": true})),
            b: Vec::new(),
        },
    ];
    let seg = SegmentPayload {
        v: FORMAT_VERSION,
        device: dev.into(),
        w: 1,
        lo: 1,
        hi: 1,
        schema: SchemaVersions::default(),
        records,
    }
    .encode()
    .unwrap();
    let sealed = crypto::seal_small(
        &keys,
        FileKind::Segment,
        seg.flags,
        &vault,
        &wisp_sync::layout::segment_aad_name(dev, 1),
        &seg.bytes,
    )
    .unwrap();
    let entry = SegmentEntry {
        w: 1,
        lo: 1,
        hi: 1,
        bytes: sealed.len() as u64,
        hash: segment_hash(&sealed),
    };
    server.raw_put(
        &wisp_sync::layout::segment_path(&vault, dev, 1),
        bytes::Bytes::from(sealed),
    );
    let head = HeadPayload {
        v: FORMAT_VERSION,
        vault_id: vault.clone(),
        device: wisp_sync::DeviceInfo {
            id: dev.into(),
            name: "future".into(),
            os: wisp_types::OsKind::Macos,
            app_version: "9.0.0".into(),
        },
        updated_ms: 1,
        segments: vec![entry],
    }
    .encode()
    .unwrap();
    let sealed = crypto::seal_small(
        &keys,
        FileKind::Head,
        head.flags,
        &vault,
        &wisp_sync::layout::head_aad_name(dev),
        &head.bytes,
    )
    .unwrap();
    server.raw_put(&wisp_sync::layout::head_path(&vault, dev), bytes::Bytes::from(sealed));

    let report = a.sync().await.unwrap();
    assert_eq!(report.corrupt_devices, 0);
    assert_eq!(a.state().glo["from-future"], json!({"target": "f", "extra": true}));
    // 不认识的域留在副本里（升级后再应用），不交给本机存储
    let journal = a.engine().journal().unwrap();
    assert_eq!(journal.winners(&Domain::parse("future")).unwrap().len(), 1);
    assert!(
        a.store
            .received
            .lock()
            .iter()
            .all(|c| !matches!(c, wisp_sync::RemoteChange::Set { key, .. } if key == "something"))
    );
    let devices = a.engine().devices().unwrap();
    assert!(devices.iter().any(|d| d.id == dev && d.app_version == "9.0.0"));
}
