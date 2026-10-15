//! 多设备同步的接入点：本机改动按约定通知 sink（粘贴 touch、保留策略、识别历史裁剪不通知）；远端改动的应用不回调 sink、
//! 不为自己打开数据库，缓存与 `clipboard:changed` 按视图状态推送，结果逐条对应输入。

use std::sync::Arc;

use parking_lot::Mutex;
use wisp_clipboard::{ClipStore, RemoteItem, RemoteOcr, SyncSource, ocr_key};
use wisp_types::{ClipKind, ClipboardChanged, OcrHistoryChanged, OcrMode, RawClipboard};

use super::port::ClipView;
use super::sync::{ClipRemoteChange, ClipRemoteClear, ClipSyncSink, OcrRemoteChange, RemoteOutcome, Superseded};
use super::tests::{ICON_64, Rig, rig};

#[derive(Debug, Clone, PartialEq, Eq)]
enum SinkCall {
    Content([u8; 32]),
    Pin([u8; 32], bool),
    Delete([u8; 32]),
    Clear(bool),
    OcrAdd(String),
    OcrDelete(String),
    OcrClear,
}

#[derive(Default)]
struct RecordingSink(Mutex<Vec<SinkCall>>);

impl RecordingSink {
    fn take(&self) -> Vec<SinkCall> {
        std::mem::take(&mut *self.0.lock())
    }
}

impl ClipSyncSink for RecordingSink {
    fn content(&self, hash: [u8; 32]) {
        self.0.lock().push(SinkCall::Content(hash));
    }
    fn pin(&self, hash: [u8; 32], pinned: bool) {
        self.0.lock().push(SinkCall::Pin(hash, pinned));
    }
    fn delete(&self, hash: [u8; 32]) {
        self.0.lock().push(SinkCall::Delete(hash));
    }
    fn clear(&self, keep_pinned: bool) {
        self.0.lock().push(SinkCall::Clear(keep_pinned));
    }
    fn ocr_add(&self, key: &str) {
        self.0.lock().push(SinkCall::OcrAdd(key.to_owned()));
    }
    fn ocr_delete(&self, key: &str) {
        self.0.lock().push(SinkCall::OcrDelete(key.to_owned()));
    }
    fn ocr_clear(&self) {
        self.0.lock().push(SinkCall::OcrClear);
    }
}

fn with_sink(r: &Rig) -> Arc<RecordingSink> {
    let sink = Arc::new(RecordingSink::default());
    r.engine.set_sync_sink(Some(Arc::clone(&sink) as Arc<dyn ClipSyncSink>));
    sink
}

fn store(r: &Rig) -> Arc<ClipStore> {
    r.engine.ready_store().expect("已打开")
}

fn hash(r: &Rig, id: i64) -> [u8; 32] {
    store(r).hash_of(id).expect("hash").expect("存在")
}

/// 「另一台设备」上同一段文本的身份（与本机按同样的规范化计算）
fn foreign_hash(text: &str) -> [u8; 32] {
    let other = rig();
    let id = other.copy_text(1, text).id;
    hash(&other, id)
}

fn remote_text(text: &str, last_used_at: i64, pinned: Option<bool>) -> RemoteItem {
    RemoteItem {
        hash: foreign_hash(text),
        kind: ClipKind::Text,
        created_at: last_used_at,
        last_used_at,
        pinned,
        text: Some(text.to_owned()),
        html: None,
        rtf: None,
        image_path: None,
        source: Some(SyncSource {
            app_id: "com.example.remote".into(),
            app_name: "Remote".into(),
        }),
        truncated: false,
    }
}

const T0: i64 = 1_760_000_000_000;

// ---------- 本机改动 → sink ----------

#[test]
fn copies_pins_deletes_and_clears_notify_the_sink() {
    let r = rig();
    // 同步关闭（没有 sink）时什么都不做
    r.copy_text(100, "before sync");
    let sink = with_sink(&r);

    let a = r.copy_text(101, "alpha");
    let ha = hash(&r, a.id);
    assert_eq!(sink.take(), vec![SinkCall::Content(ha)], "首次复制");
    r.copy_text(102, "alpha");
    assert_eq!(
        sink.take(),
        vec![SinkCall::Content(ha)],
        "再次复制（touched）同样写内容"
    );

    r.engine.pin(a.id, true).expect("置顶");
    assert_eq!(sink.take(), vec![SinkCall::Pin(ha, true)]);
    r.engine.pin(a.id, false).expect("取消置顶");
    assert_eq!(sink.take(), vec![SinkCall::Pin(ha, false)]);

    // 删除：hash 在删除之前取
    r.engine.delete(a.id, false).expect("删除");
    assert_eq!(sink.take(), vec![SinkCall::Delete(ha)]);
    // 已不存在的：不通知
    r.engine.delete(a.id, false).expect("删除");
    assert!(sink.take().is_empty());

    r.engine.clear(true).expect("清空");
    assert_eq!(sink.take(), vec![SinkCall::Clear(true)]);
    r.engine.clear(false).expect("清空");
    assert_eq!(sink.take(), vec![SinkCall::Clear(false)]);

    // 卸载之后不再通知
    r.engine.set_sync_sink(None);
    r.copy_text(103, "after sync");
    assert!(sink.take().is_empty());
}

#[test]
fn paste_touch_retention_skips_and_files_do_not_notify() {
    let r = rig();
    let a = r.copy_text(101, "keep");
    for i in 0..5 {
        r.copy_text(102 + i, &format!("filler {i}"));
    }
    let sink = with_sink(&r);
    // 粘贴时的 touch 只影响本机
    r.engine.touch(&store(&r), a.id).expect("touch");
    assert!(sink.take().is_empty());
    // 疑似密钥被跳过：不留痕迹
    r.copy(
        120,
        RawClipboard {
            change_id: 120,
            text: Some("sk-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789AbCdEfGhIjKl".into()),
            ..RawClipboard::default()
        },
    );
    assert!(sink.take().is_empty());
    // 文件条目不同步
    let (files, _) = r.copy_files(121, ["a.txt", "b.txt"]);
    r.engine.pin(files.id, true).expect("置顶");
    r.engine.delete(files.id, true).expect("删除");
    assert!(sink.take().is_empty());
    // 保留策略的删除不写墓碑
    r.set_settings(|s| s.clipboard.max_items = 2);
    assert!(r.engine.run_cleanup().removed_items > 0);
    assert!(sink.take().is_empty());
}

#[test]
fn ocr_history_changes_notify_the_sink_with_derived_keys() {
    let r = rig();
    r.set_settings(|s| s.ocr.history = true);
    let sink = with_sink(&r);
    let rec = |text: &str, replaces: Option<i64>| super::ocr_history::OcrRecord {
        mode: OcrMode::Markdown,
        model: "Fake Vision".to_owned(),
        text: text.to_owned(),
        image: Some(Arc::new(ICON_64.to_vec())),
        thumb: None,
        replaces,
    };
    let first = r
        .engine
        .ocr_history_record(&rec("first", None))
        .expect("记录")
        .expect("写入");
    let key1 = store(&r).ocr_key_of(first).expect("键").expect("存在");
    assert_eq!(sink.take(), vec![SinkCall::OcrAdd(key1.clone())]);
    let item = store(&r).ocr_sync_export_by_id(first).expect("导出").expect("存在");
    assert_eq!(key1, ocr_key(item.created_at, OcrMode::Markdown, "first"));

    // 重新识别取代旧记录：新增 + 删除旧的（键在删除的同一个事务里取）
    let second = r
        .engine
        .ocr_history_record(&rec("second", Some(first)))
        .expect("记录")
        .expect("写入");
    let key2 = store(&r).ocr_key_of(second).expect("键").expect("存在");
    assert_eq!(
        sink.take(),
        vec![SinkCall::OcrAdd(key2.clone()), SinkCall::OcrDelete(key1)]
    );

    // 用户删除
    r.engine.ocr_history_delete(second).expect("删除");
    assert_eq!(sink.take(), vec![SinkCall::OcrDelete(key2)]);
    r.engine.ocr_history_delete(second).expect("已删除");
    assert!(sink.take().is_empty());

    // 条数裁剪只属于本机
    r.set_settings(|s| s.ocr.history_limit = 10);
    for i in 0..12 {
        r.engine.ocr_history_record(&rec(&format!("n{i}"), None)).expect("记录");
    }
    let calls = sink.take();
    assert_eq!(calls.len(), 12);
    assert!(
        calls.iter().all(|c| matches!(c, SinkCall::OcrAdd(_))),
        "裁剪不通知：{calls:?}"
    );
    assert_eq!(store(&r).ocr_sync_rows().expect("rows").len(), 10);

    r.engine.ocr_history_clear().expect("清空");
    assert_eq!(sink.take(), vec![SinkCall::OcrClear]);
}

// ---------- 远端改动 → 本机 ----------

#[test]
fn nothing_is_applied_before_the_database_is_open() {
    let r = rig();
    let out = r
        .engine
        .apply_remote_batch(vec![ClipRemoteChange::Upsert(remote_text("x", T0, None))], None);
    assert_eq!(out, vec![RemoteOutcome::Deferred]);
    let clear = ClipRemoteClear {
        wall_ms: T0,
        keep_pinned: true,
        exclude: Vec::new(),
    };
    assert_eq!(r.engine.apply_remote_clear(&clear), RemoteOutcome::Deferred);
    let ocr = r
        .engine
        .apply_remote_ocr(vec![OcrRemoteChange::Delete { key: "0".repeat(32) }], None);
    assert_eq!(ocr, vec![RemoteOutcome::Deferred]);
    assert!(r.engine.ready_store().is_none(), "同步从不为自己打开数据库");
}

#[test]
fn remote_items_land_in_the_cache_and_are_pushed_without_echo() {
    let r = rig();
    r.engine.preload();
    let sink = with_sink(&r);
    let local = r.copy_text(101, "local");
    sink.take();
    r.events();

    let newest = remote_text("remote newest", T0 + 1_000_000, None);
    let pinned = remote_text("remote pinned", T0 - 5_000, Some(true));
    let secret = RemoteItem {
        text: Some("sk-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789AbCdEfGhIjKl".into()),
        ..remote_text("placeholder", T0, None)
    };
    let out = r.engine.apply_remote_batch(
        vec![
            ClipRemoteChange::Upsert(newest),
            ClipRemoteChange::Upsert(pinned),
            ClipRemoteChange::Upsert(secret),
        ],
        None,
    );
    assert_eq!(
        out,
        vec![RemoteOutcome::Applied, RemoteOutcome::Applied, RemoteOutcome::Evicted]
    );
    assert!(sink.take().is_empty(), "应用远端改动不回调 sink");

    let page = r.engine.first_page();
    let previews: Vec<&str> = page.items.iter().map(|h| h.item.preview.as_str()).collect();
    assert_eq!(previews, ["remote pinned", "remote newest", "local"]);
    assert_eq!(page.items[1].item.last_used_at, T0 + 1_000_000, "沿用远端的时间");
    let events = r.events();
    assert_eq!(events.len(), 2, "{events:?}");
    assert!(events.iter().all(|e| matches!(e, ClipboardChanged::Inserted { .. })));

    // 远端置顶本机已有的条目：touched（前端按新的排序键移动它）
    let h = hash(&r, local.id);
    let out = r
        .engine
        .apply_remote_batch(vec![ClipRemoteChange::Pin { hash: h, pinned: true }], None);
    assert_eq!(out, vec![RemoteOutcome::Applied]);
    match r.events().as_slice() {
        [ClipboardChanged::Touched { item }] => assert!(item.id == local.id && item.pinned),
        other => panic!("{other:?}"),
    }
    // 状态没变：不推送
    r.engine
        .apply_remote_batch(vec![ClipRemoteChange::Pin { hash: h, pinned: true }], None);
    assert!(r.events().is_empty());
    // 远端删除：removed
    let out = r
        .engine
        .apply_remote_batch(vec![ClipRemoteChange::Delete { hash: h }], None);
    assert_eq!(out, vec![RemoteOutcome::Applied]);
    assert_eq!(r.events(), vec![ClipboardChanged::Removed { id: local.id }]);
    assert!(!r.ids().contains(&local.id));
    // 不存在的条目：置顶报告未接收（同步层据此重新取回内容），取消置顶与删除算已应用
    let out = r.engine.apply_remote_batch(
        vec![
            ClipRemoteChange::Pin {
                hash: [9; 32],
                pinned: true,
            },
            ClipRemoteChange::Pin {
                hash: [9; 32],
                pinned: false,
            },
            ClipRemoteChange::Delete { hash: [9; 32] },
        ],
        None,
    );
    assert_eq!(
        out,
        vec![RemoteOutcome::Evicted, RemoteOutcome::Applied, RemoteOutcome::Applied]
    );
    assert!(sink.take().is_empty());
}

#[test]
fn remote_reuse_of_an_existing_item_moves_it_like_a_touch() {
    let r = rig();
    let a = r.copy_text(101, "shared");
    r.copy_text(102, "other");
    r.events();
    let out = r.engine.apply_remote_batch(
        vec![ClipRemoteChange::Upsert(remote_text("shared", T0 + 10_000, None))],
        None,
    );
    assert_eq!(out, vec![RemoteOutcome::Applied]);
    match r.events().as_slice() {
        [ClipboardChanged::Touched { item }] => assert_eq!((item.id, item.last_used_at), (a.id, T0 + 10_000)),
        other => panic!("{other:?}"),
    }
    assert_eq!(r.ids()[0], a.id);
    // 远端更早创建、使用时间更旧：只有创建时间变了，原地 updated，不移动
    r.engine.apply_remote_batch(
        vec![ClipRemoteChange::Upsert(remote_text("shared", T0 - 1, None))],
        None,
    );
    match r.events().as_slice() {
        [ClipboardChanged::Updated { item }] => {
            assert_eq!(
                (item.id, item.created_at, item.last_used_at),
                (a.id, T0 - 1, T0 + 10_000)
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(r.ids()[0], a.id);
    // 完全没有变化：不推送
    r.engine.apply_remote_batch(
        vec![ClipRemoteChange::Upsert(remote_text("shared", T0 - 1, None))],
        None,
    );
    assert!(r.events().is_empty());
}

#[test]
fn items_older_than_the_cached_prefix_are_not_pushed() {
    let r = rig();
    for i in 0..205 {
        r.copy_text(1000 + i, &format!("local {i}"));
    }
    r.events();
    let out = r.engine.apply_remote_batch(
        vec![ClipRemoteChange::Upsert(remote_text("ancient", T0 - 1_000_000, None))],
        None,
    );
    assert_eq!(out, vec![RemoteOutcome::Applied]);
    assert!(r.events().is_empty(), "排在缓存前缀之后：前端翻页时自然取到");
    let page = r.engine.recent(200, 50, wisp_types::ClipFilter::All).expect("翻页");
    assert_eq!(page.items.last().map(|h| h.item.preview.as_str()), Some("ancient"));
}

#[test]
fn delivery_follows_the_view_state() {
    let r = rig();
    r.engine.preload();
    // 面板隐藏：不推送，缓存照常更新
    r.host.set_view(ClipView::Hidden);
    r.engine.apply_remote_batch(
        vec![ClipRemoteChange::Upsert(remote_text("while hidden", T0 + 1, None))],
        None,
    );
    assert!(r.events().is_empty());
    assert_eq!(r.engine.first_page().items[0].item.preview, "while hidden");
    // 视图准备中（first_page 之后、ready 之前）：只推一条 reloaded，排队到视图就绪
    r.host.set_view(ClipView::Preparing(9));
    r.engine.apply_remote_batch(
        vec![
            ClipRemoteChange::Upsert(remote_text("while preparing 1", T0 + 2, None)),
            ClipRemoteChange::Upsert(remote_text("while preparing 2", T0 + 3, None)),
        ],
        None,
    );
    assert_eq!(r.engine.queued_events(), 1);
    r.host.set_view(ClipView::Shown(9));
    r.engine.flush_outbox();
    assert_eq!(r.events(), vec![ClipboardChanged::Reloaded]);
    // 准备中的清空同样只推一条 reloaded
    r.host.set_view(ClipView::Preparing(10));
    let clear = ClipRemoteClear {
        wall_ms: T0 + 10,
        keep_pinned: true,
        exclude: Vec::new(),
    };
    assert_eq!(r.engine.apply_remote_clear(&clear), RemoteOutcome::Applied);
    assert_eq!(r.engine.queued_events(), 1);
    r.host.set_view(ClipView::Shown(10));
    r.engine.flush_outbox();
    assert_eq!(r.events(), vec![ClipboardChanged::Reloaded]);
}

#[test]
fn large_batches_use_several_serial_sections_and_bounded_events() {
    let r = rig();
    r.engine.preload();
    let changes: Vec<_> = (0..120)
        .map(|i| ClipRemoteChange::Upsert(remote_text(&format!("bulk {i:03}"), T0 + 100 + i, None)))
        .collect();
    let out = r.engine.apply_remote_batch(changes, None);
    assert!(out.iter().all(|o| *o == RemoteOutcome::Applied));
    let events = r.events();
    assert_eq!(events.len(), 120, "每个串行区最多 50 条，逐条推送");
    assert_eq!(r.engine.first_page().items.len(), 120);

    // 一次清掉很多：视图显示中照样逐条推送 removed（前端在显示期间会把 reloaded 暂存，列表就停在旧内容上）
    let clear = ClipRemoteClear {
        wall_ms: T0 + 10_000,
        keep_pinned: true,
        exclude: Vec::new(),
    };
    assert_eq!(r.engine.apply_remote_clear(&clear), RemoteOutcome::Applied);
    let events = r.events();
    assert_eq!(events.len(), 120, "{events:?}");
    assert!(events.iter().all(|e| matches!(e, ClipboardChanged::Removed { .. })));
    assert!(r.engine.first_page().items.is_empty());
}

#[test]
fn remote_clear_pushes_removals_for_the_cached_prefix_only() {
    let r = rig();
    for i in 0..205 {
        r.copy_text(1000 + i, &format!("local {i}"));
    }
    r.events();
    let clear = ClipRemoteClear {
        wall_ms: r.host.now.load(std::sync::atomic::Ordering::SeqCst) + 1,
        keep_pinned: true,
        exclude: Vec::new(),
    };
    assert_eq!(r.engine.apply_remote_clear(&clear), RemoteOutcome::Applied);
    let events = r.events();
    // 缓存前缀是一页 200 条：首屏能显示的逐条推送，更旧的不推
    assert_eq!(events.len(), 200);
    assert!(events.iter().all(|e| matches!(e, ClipboardChanged::Removed { .. })));
    assert!(r.ids().is_empty());
}

#[test]
fn remote_clear_spares_excluded_pinned_and_newer_rows() {
    let r = rig();
    let synced = r.copy_text(101, "has a sync record");
    let (files, _) = r.copy_files(102, ["x.txt", "y.txt"]);
    let pinned = r.copy_text(103, "pinned");
    r.engine.pin(pinned.id, true).expect("置顶");
    // 之后的复制拿到的时间都不早于它
    let cutoff = r.host.now.load(std::sync::atomic::Ordering::SeqCst);
    // copy_text 返回首屏第一条（这里是置顶项）：新条目排在置顶之后
    r.copy_text(104, "after the clear");
    let later = r.ids()[1];
    r.events();
    let clear = ClipRemoteClear {
        wall_ms: cutoff - 1,
        keep_pinned: true,
        exclude: vec![hash(&r, synced.id)],
    };
    assert_eq!(r.engine.apply_remote_clear(&clear), RemoteOutcome::Applied);
    assert_eq!(r.events(), vec![ClipboardChanged::Removed { id: files.id }]);
    assert_eq!(r.ids(), vec![pinned.id, later, synced.id]);
}

#[test]
fn superseded_changes_are_skipped_inside_the_serial_section() {
    let r = rig();
    let a = r.copy_text(101, "edited locally");
    let ha = hash(&r, a.id);
    r.events();
    let guard: Superseded = Arc::new(move |h: &[u8; 32]| *h == ha);
    let out = r.engine.apply_remote_batch(
        vec![
            ClipRemoteChange::Delete { hash: ha },
            ClipRemoteChange::Upsert(remote_text("unrelated", T0 + 5, None)),
        ],
        Some(&guard),
    );
    assert_eq!(out, vec![RemoteOutcome::Superseded, RemoteOutcome::Applied]);
    assert!(r.ids().contains(&a.id), "本机更新的改动胜出：不删除");
}

#[test]
fn disabled_history_does_not_accept_remote_items() {
    let r = rig();
    r.engine.preload();
    r.set_settings(|s| s.clipboard.enabled = false);
    let out = r
        .engine
        .apply_remote_batch(vec![ClipRemoteChange::Upsert(remote_text("x", T0, None))], None);
    assert_eq!(out, vec![RemoteOutcome::Evicted]);
    assert!(r.engine.first_page().items.is_empty());
}

#[test]
fn remote_images_get_a_background_thumbnail() {
    let a = rig();
    a.engine.preload();
    let raw = RawClipboard {
        change_id: 5,
        image_png: Some(ICON_64.to_vec()),
        ..RawClipboard::default()
    };
    a.copy(5, raw);
    let img = a.top();
    let item = store(&a).sync_export(&hash(&a, img.id)).expect("导出").expect("存在");

    let b = rig();
    b.engine.preload();
    let remote = RemoteItem {
        hash: item.hash,
        kind: ClipKind::Image,
        created_at: item.created_at,
        last_used_at: item.last_used_at,
        pinned: None,
        text: None,
        html: None,
        rtf: None,
        image_path: item.image.as_ref().map(|i| i.path.clone()),
        source: None,
        truncated: false,
    };
    let out = b
        .engine
        .apply_remote_batch(vec![ClipRemoteChange::Upsert(remote)], None);
    assert_eq!(out, vec![RemoteOutcome::Applied]);
    let events = b.events();
    assert!(
        matches!(events.first(), Some(ClipboardChanged::Inserted { .. })),
        "{events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(e, ClipboardChanged::Updated { item }
            if item.image.as_ref().is_some_and(|i| i.thumb_url.is_some()))),
        "缩略图在后台生成后推送 updated：{events:?}"
    );
    assert!(b.engine.idle.dirty(), "排一次空闲清理");
}

#[test]
fn remote_ocr_records_are_deduped_trimmed_and_never_echoed() {
    let r = rig();
    r.set_settings(|s| s.ocr.history = true);
    r.engine
        .ocr_history_record(&super::ocr_history::OcrRecord {
            mode: OcrMode::Lines,
            model: "m".into(),
            text: "local".into(),
            image: None,
            thumb: None,
            replaces: None,
        })
        .expect("记录");
    let sink = with_sink(&r);
    r.host.ocr_events.lock().clear();
    let remote = |t: i64, text: &str| RemoteOcr {
        key: ocr_key(t, OcrMode::Paragraphs, text),
        created_at: t,
        mode: OcrMode::Paragraphs,
        model: "remote model".into(),
        text: text.into(),
        thumb_path: None,
        image_path: None,
    };
    let fresh = remote(T0 + 50, "remote text");
    let out = r.engine.apply_remote_ocr(
        vec![
            OcrRemoteChange::Upsert(fresh.clone()),
            OcrRemoteChange::Upsert(fresh.clone()),
            OcrRemoteChange::Upsert(RemoteOcr {
                text: "tampered".into(),
                ..remote(T0 + 60, "x")
            }),
        ],
        None,
    );
    assert_eq!(
        out,
        vec![RemoteOutcome::Applied, RemoteOutcome::Applied, RemoteOutcome::Evicted]
    );
    let events = std::mem::take(&mut *r.host.ocr_events.lock());
    assert!(
        matches!(events.as_slice(), [OcrHistoryChanged::Inserted { item }] if item.text == "remote text"),
        "{events:?}"
    );
    assert!(sink.take().is_empty(), "应用远端改动不回调 sink");

    // 删除按键
    let out = r
        .engine
        .apply_remote_ocr(vec![OcrRemoteChange::Delete { key: fresh.key.clone() }], None);
    assert_eq!(out, vec![RemoteOutcome::Applied]);
    assert!(matches!(
        std::mem::take(&mut *r.host.ocr_events.lock()).as_slice(),
        [OcrHistoryChanged::Removed { ids }] if ids.len() == 1
    ));
    assert!(sink.take().is_empty());

    // 本机关闭了识别历史：不接收新记录
    r.set_settings(|s| s.ocr.history = false);
    let out = r
        .engine
        .apply_remote_ocr(vec![OcrRemoteChange::Upsert(remote(T0 + 70, "later"))], None);
    assert_eq!(out, vec![RemoteOutcome::Evicted]);
}

#[test]
fn remote_ocr_respects_the_local_limit() {
    let r = rig();
    r.set_settings(|s| {
        s.ocr.history = true;
        s.ocr.history_limit = 10;
    });
    for i in 0..10 {
        r.engine
            .ocr_history_record(&super::ocr_history::OcrRecord {
                mode: OcrMode::Lines,
                model: "m".into(),
                text: format!("local {i}"),
                image: None,
                thumb: None,
                replaces: None,
            })
            .expect("记录");
    }
    let old = RemoteOcr {
        key: ocr_key(T0 - 10, OcrMode::Lines, "old"),
        created_at: T0 - 10,
        mode: OcrMode::Lines,
        model: "m".into(),
        text: "old".into(),
        thumb_path: None,
        image_path: None,
    };
    let out = r.engine.apply_remote_ocr(vec![OcrRemoteChange::Upsert(old)], None);
    assert_eq!(out, vec![RemoteOutcome::Evicted], "排在上限之外：不插入");
    assert_eq!(store(&r).ocr_sync_rows().expect("rows").len(), 10);
}
