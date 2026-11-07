//! 实时同步的只读探测：没有变化时只发一个列目录请求、一次也不写 sync.db；发现变化后的一轮复用这次的列表；
//! 各种服务器变化都能看出来，本机自己的推送不会被当成别人的改动；同步进行中、服务器不给签名、未解锁时的结论。

mod sim;

use std::time::Duration;

use rusqlite::Connection;
use serde_json::json;
use sim::{Device, noop_progress, settle, text_hash};
use wisp_sync::remote::memory::{Metadata, Op};
use wisp_sync::{CycleReason, CycleReport, Domain, Listing, MemoryRemote, Peek, Remote, SyncError, layout};

/// 另一个连接看到的 sync.db 版本号：别的连接每提交一次写入就变一次（SQLite 的 `PRAGMA data_version`）
fn data_version(conn: &Connection) -> i64 {
    conn.query_row("PRAGMA data_version", [], |r| r.get(0))
        .expect("data_version")
}

async fn peek<R: Remote>(d: &Device<R>) -> Peek {
    d.engine().peek(&d.remote).await.expect("peek")
}

fn changed(p: Peek) -> Listing {
    match p {
        Peek::Changed(listing) => listing,
        other => panic!("应当发现变化，实际 {other:?}"),
    }
}

async fn cycle_with<R: Remote>(d: &Device<R>, listing: Listing) -> CycleReport {
    d.engine()
        .sync_cycle_with(
            &d.remote,
            &d.store,
            CycleReason::Poll,
            &d.options,
            noop_progress(),
            Some(listing),
        )
        .await
        .expect("cycle")
}

async fn pair(server: &MemoryRemote) -> (Device, Device) {
    let mut a = Device::new(server, "a", 0xa);
    let mut b = Device::new(server, "b", 0xb);
    a.store.copy("hello");
    a.connect().await.expect("connect");
    b.connect().await.expect("connect");
    settle(&mut [&mut a, &mut b]).await;
    (a, b)
}

#[tokio::test]
async fn idle_peek_is_one_request_and_writes_nothing() {
    let server = MemoryRemote::new();
    let (a, _b) = pair(&server).await;
    let conn = Connection::open(a.db_path()).expect("second connection");
    let before = data_version(&conn);
    a.remote.clear_ops();
    for _ in 0..5 {
        assert!(matches!(peek(&a).await, Peek::Unchanged));
    }
    let ops = a.remote.ops();
    assert_eq!(ops.len(), 5, "{ops:?}");
    assert!(
        ops.iter().all(|(op, path)| *op == Op::List && path.is_empty()),
        "{ops:?}"
    );
    assert_eq!(data_version(&conn), before, "探测不写 sync.db");
    let wal = std::fs::metadata(a.dir.path().join("sync.db-wal"))
        .map(|m| m.len())
        .ok();
    for _ in 0..20 {
        peek(&a).await;
    }
    assert_eq!(
        std::fs::metadata(a.dir.path().join("sync.db-wal"))
            .map(|m| m.len())
            .ok(),
        wal,
        "WAL 也不增长"
    );

    // 对照：没有变化的完整一轮也会写 sync.db（所以探测要另走只读的路）
    let mut a = a;
    a.sync().await.unwrap();
    assert_ne!(data_version(&conn), before);
}

#[tokio::test]
async fn a_peer_change_is_seen_and_the_next_cycle_reuses_the_listing() {
    let server = MemoryRemote::new();
    let (a, mut b) = pair(&server).await;
    b.store.copy("from b");
    b.sync().await.unwrap();

    a.remote.clear_ops();
    let listing = changed(peek(&a).await);
    assert_eq!(a.remote.ops().len(), 1);
    a.remote.clear_ops();
    let report = cycle_with(&a, listing).await;
    let ops: Vec<Op> = a.remote.ops().into_iter().map(|(op, _)| op).collect();
    assert_eq!(ops, [Op::Get, Op::Get], "只取 head 与新段，不再列一次目录");
    assert_eq!(report.requests, 2);
    // 其他设备的新复制交给调用方（hash 与时间，没有内容）
    assert_eq!(report.remote_clips.len(), 1, "{:?}", report.remote_clips);
    assert_eq!(report.remote_clips[0].hash, text_hash("from b"));
    assert!(report.remote_clips[0].recorded_ms > 0 && report.remote_clips[0].last_used_ms > 0);
    assert!(a.state().clip.values().any(|c| c.text == "from b"));

    // 同步完之后回到「没有变化」
    a.remote.clear_ops();
    assert!(matches!(peek(&a).await, Peek::Unchanged));
    assert_eq!(a.remote.ops().len(), 1);
}

#[tokio::test]
async fn own_pushes_are_not_mistaken_for_peer_changes() {
    for metadata in [Metadata::Full, Metadata::NoEtag] {
        let server = MemoryRemote::new();
        server.set_metadata(metadata);
        let (mut a, b) = pair(&server).await;
        a.store.copy("mine");
        a.store.set(Domain::Cfg, "/general/theme", json!("dark"));
        let report = a.sync().await.unwrap();
        assert!(report.pushed > 0);
        // PUT 没回 ETag 时第一次探测记下本机 head 的签名，之后照常
        for _ in 0..3 {
            assert!(matches!(peek(&a).await, Peek::Unchanged), "{metadata:?}");
        }
        // 对端看得到
        let listing = changed(peek(&b).await);
        cycle_with(&b, listing).await;
        assert!(b.state().clip.values().any(|c| c.text == "mine"));
        assert!(matches!(peek(&b).await, Peek::Unchanged));
    }
}

#[tokio::test]
async fn server_changes_are_detected() {
    let server = MemoryRemote::new();
    let (mut a, b) = pair(&server).await;
    let (vault, _) = a.creds.clone().unwrap();

    // 新设备加入
    let mut c = Device::new(&server, "c", 0xc);
    c.connect().await.unwrap();
    c.sync().await.unwrap();
    let listing = changed(peek(&a).await);
    cycle_with(&a, listing).await;
    assert!(matches!(peek(&a).await, Peek::Unchanged));

    // 一台设备消失
    assert!(server.raw_remove(&layout::head_path(&vault, &c.device_id)));
    let listing = changed(peek(&a).await);
    cycle_with(&a, listing).await;
    assert!(matches!(peek(&a).await, Peek::Unchanged));

    // 本机 head 被删掉（服务器回档等）：一轮会把它写回去
    assert!(server.raw_remove(&layout::head_path(&vault, &a.device_id)));
    let listing = changed(peek(&a).await);
    cycle_with(&a, listing).await;
    assert!(server.raw_get(&layout::head_path(&vault, &a.device_id)).is_some());
    assert!(matches!(peek(&a).await, Peek::Unchanged));

    // vault.json 被替换：探测说「有变化」，完整的一轮报告数据已被清除
    let original = server.raw_get(layout::VAULT_FILE).unwrap();
    let mut other = Device::new(&MemoryRemote::new(), "x", 0xd);
    other.connect().await.unwrap();
    let other_vault = other.remote.inner.raw_get(layout::VAULT_FILE).unwrap();
    server.raw_put(layout::VAULT_FILE, other_vault);
    let listing = changed(peek(&a).await);
    let err = a
        .engine()
        .sync_cycle_with(
            &a.remote,
            &a.store,
            CycleReason::Poll,
            &a.options,
            noop_progress(),
            Some(listing),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, SyncError::VaultReset), "{err:?}");
    // 失败之后不留快照：换回来以后先跑一轮再说
    server.raw_put(layout::VAULT_FILE, original);
    let listing = changed(peek(&a).await);
    cycle_with(&a, listing).await;
    assert!(matches!(peek(&a).await, Peek::Unchanged));
    drop(b);
    a.sync().await.unwrap();
}

#[tokio::test]
async fn a_fresh_process_runs_one_full_cycle_first() {
    let server = MemoryRemote::new();
    let (mut a, _b) = pair(&server).await;
    a.restart();
    let listing = changed(peek(&a).await);
    a.remote.clear_ops();
    cycle_with(&a, listing).await;
    let ops = a.remote.ops();
    assert!(ops.iter().all(|(op, _)| *op != Op::List), "复用探测的列表：{ops:?}");
    assert!(matches!(peek(&a).await, Peek::Unchanged));
}

#[tokio::test]
async fn busy_unsupported_and_locked() {
    let server = MemoryRemote::new();
    let (a, _b) = pair(&server).await;

    // 正在跑一轮：不等它、不发请求
    server.set_latency(Some(Duration::from_millis(200)));
    let running = a
        .engine()
        .sync_cycle(&a.remote, &a.store, CycleReason::Manual, &a.options, noop_progress());
    let probe = async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let before = a.remote.ops().len();
        let p = peek(&a).await;
        (p, a.remote.ops().len() - before)
    };
    let (cycle, (p, sent)) = tokio::join!(running, probe);
    cycle.unwrap();
    assert!(matches!(p, Peek::Busy), "{p:?}");
    assert_eq!(sent, 0);
    server.set_latency(None);

    // 服务器不给签名：无法判断变化
    server.set_metadata(Metadata::None);
    assert!(matches!(peek(&a).await, Peek::Unsupported));
    server.set_metadata(Metadata::Full);

    // 没有解锁
    a.engine().lock();
    assert!(matches!(a.engine().peek(&a.remote).await, Err(SyncError::Locked)));
}

#[tokio::test]
async fn a_listing_from_another_vault_is_ignored() {
    let server = MemoryRemote::new();
    let (a, mut b) = pair(&server).await;
    let other = MemoryRemote::new();
    let (x, _y) = pair(&other).await;
    // x 的服务器上有改动：拿它的列表给 a 用必须作废（a 自己列一次）
    let foreign = changed({
        let mut z = Device::new(&other, "z", 0xe);
        z.connect().await.unwrap();
        peek(&x).await
    });
    b.store.copy("for a");
    b.sync().await.unwrap();
    a.remote.clear_ops();
    cycle_with(&a, foreign).await;
    assert_eq!(a.remote.ops()[0], (Op::List, String::new()));
    assert!(a.state().clip.values().any(|c| c.text == "for a"));
}
