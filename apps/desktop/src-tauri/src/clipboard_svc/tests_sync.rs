//! 多设备同步的接入点：本机改动按约定通知 sink（粘贴 touch、保留策略、识别历史裁剪不通知）；远端改动的应用不回调 sink、
//! 不为自己打开数据库，缓存与 `clipboard:changed` 按视图状态推送，结果逐条对应输入；实时同步把其他设备最新的复制放进
//! 本机剪贴板（每个条件、防乒乓、与粘贴互斥）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use wisp_clipboard::{ClipStore, RemoteItem, RemoteOcr, SyncSource, ocr_key};
use wisp_types::{ClipKind, ClipPayload, ClipboardChanged, OcrHistoryChanged, OcrMode, PasteMode, RawClipboard};

use super::paste::QuietWrite;
use super::port::ClipView;
use super::remote_copy::{LOCAL_QUIET, RemoteCopy, Skip};
use super::sync::{ClipRemoteChange, ClipRemoteClear, ClipSyncSink, OcrRemoteChange, RemoteOutcome, Superseded};
use super::tests::{Call, ICON_64, Rig, app, rig};

#[derive(Debug, Clone, PartialEq, Eq)]
enum SinkCall {
    Content([u8; 32]),
    Favorite([u8; 32], bool, Option<String>),
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
    fn favorite(&self, hash: [u8; 32], pinned: bool, name: Option<&str>) {
        self.0
            .lock()
            .push(SinkCall::Favorite(hash, pinned, name.map(str::to_owned)));
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
        name: None,
        expires_at: None,
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
    assert_eq!(sink.take(), vec![SinkCall::Favorite(ha, true, None)]);
    r.engine.pin(a.id, false).expect("取消置顶");
    assert_eq!(sink.take(), vec![SinkCall::Favorite(ha, false, None)]);

    // 删除：hash 在删除之前取
    r.engine.delete(a.id, false).expect("删除");
    assert_eq!(sink.take(), vec![SinkCall::Delete(ha)]);
    // 已不存在的：不通知
    r.engine.delete(a.id, false).expect("删除");
    assert!(sink.take().is_empty());

    // OCR 切换排版后的跟随：替换掉的那一条经正常的删除路径通知同步
    let first = r.engine.copy_text("第一段\n\n第二段").expect("复制");
    let replaced = hash(&r, first.created.expect("新建").id);
    sink.take();
    let next = r
        .engine
        .recopy_text(first, "第一段\n第二段")
        .expect("跟随")
        .expect("写入");
    let added = hash(&r, next.created.expect("新建").id);
    assert_eq!(
        sink.take(),
        vec![SinkCall::Content(added), SinkCall::Delete(replaced)],
        "先写新内容，再删被取代的那一条"
    );
    // 重新识别的自动复制取代上一次写入新建的那一条：同样经正常的删除路径通知同步
    let again = r
        .engine
        .copy_text_replacing(Some(next), "重新识别的结果")
        .expect("自动复制");
    let fresh = hash(&r, again.created.expect("新建").id);
    assert_eq!(
        sink.take(),
        vec![SinkCall::Content(fresh), SinkCall::Delete(added)],
        "先写新内容，再删被取代的那一条"
    );

    r.engine.clear(true).expect("清空");
    assert_eq!(sink.take(), vec![SinkCall::Clear(true)]);
    r.engine.clear(false).expect("清空");
    assert_eq!(sink.take(), vec![SinkCall::Clear(false)]);

    // 卸载之后不再通知
    r.engine.set_sync_sink(None);
    r.copy_text(103, "after sync");
    assert!(sink.take().is_empty());
}

/// OCR 切换排版后的跟随删除被替换的条目之前先核对它的内容：行号没有 AUTOINCREMENT，原条目被删掉后，同步拉来的新条目
/// 可能拿到同一个行号（这一步不改系统剪贴板的 change_id）。这时不能把那条不相干的条目删掉、再把删除传播到所有设备
#[test]
fn recopy_never_deletes_an_item_that_took_over_the_replaced_row_id() {
    let r = rig();
    let first = r.engine.copy_text("识别结果").expect("复制");
    let old = first.created.expect("新建").id;
    // 面板还开着时这一条在别处被删掉（悬浮剪贴板里手动删、别的设备删除后同步过来、验证码到期）
    r.engine.delete(old, false).expect("删除");
    // 同步拉来一条新条目：SQLite 把空出来的最大行号分给它
    let synced = remote_text("另一台设备上复制的", T0, None);
    let synced_hash = synced.hash;
    let out = r
        .engine
        .apply_remote_batch(vec![ClipRemoteChange::Upsert(synced)], None);
    assert_eq!(out, vec![RemoteOutcome::Applied]);
    assert_eq!(store(&r).hash_of(old).expect("hash"), Some(synced_hash), "行号被复用");
    let sink = with_sink(&r);

    let next = r
        .engine
        .recopy_text(first, "排版后的识别结果")
        .expect("跟随")
        .expect("剪贴板没变：写入");
    assert!(next.created.is_some());
    assert_eq!(
        store(&r).hash_of(old).expect("hash"),
        Some(synced_hash),
        "同步来的条目保留"
    );
    assert!(
        !sink.take().contains(&SinkCall::Delete(synced_hash)),
        "删除不传播到别的设备"
    );
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
    let out = r.engine.apply_remote_batch(
        vec![ClipRemoteChange::Pin {
            hash: h,
            pinned: true,
            name: None,
        }],
        None,
    );
    assert_eq!(out, vec![RemoteOutcome::Applied]);
    match r.events().as_slice() {
        [ClipboardChanged::Touched { item }] => assert!(item.id == local.id && item.pinned),
        other => panic!("{other:?}"),
    }
    // 状态没变：不推送
    r.engine.apply_remote_batch(
        vec![ClipRemoteChange::Pin {
            hash: h,
            pinned: true,
            name: None,
        }],
        None,
    );
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
                name: None,
            },
            ClipRemoteChange::Pin {
                hash: [9; 32],
                pinned: false,
                name: None,
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
        name: None,
        expires_at: None,
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

// ---------- 收藏命名与验证码 ----------

/// 一条会被识别为验证码的消息
const CODE: &str = "Your verification code is 482915\nDo not share it.";

#[test]
fn names_and_expired_codes_notify_the_sink() {
    let r = rig();
    let sink = with_sink(&r);
    let a = r.copy_text(101, "Guest network: wisp-guest");
    let ha = hash(&r, a.id);
    let code = r.copy_text(102, CODE);
    let hc = hash(&r, code.id);
    assert_eq!(sink.take(), vec![SinkCall::Content(ha), SinkCall::Content(hc)]);
    // 命名即收藏：`p/` 的值带着名称
    r.engine.set_name(a.id, Some("Wi-Fi")).expect("命名");
    assert_eq!(sink.take(), vec![SinkCall::Favorite(ha, true, Some("Wi-Fi".into()))]);
    r.engine.set_name(a.id, None).expect("清除名称");
    assert_eq!(sink.take(), vec![SinkCall::Favorite(ha, true, None)]);

    // 验证码到期删除不通知同步：每台设备按同步过去的过期时间各自删除。发出删除记录会压过另一台设备在到期前的收藏
    r.host.now.store(
        code.expires_at.expect("过期时间") + 1,
        std::sync::atomic::Ordering::SeqCst,
    );
    r.exec.fire_timers();
    assert!(!r.ids().contains(&code.id), "本机照常删除");
    assert_eq!(sink.take(), vec![]);
}

#[test]
fn favoriting_a_code_republishes_its_content() {
    let r = rig();
    let sink = with_sink(&r);
    let code = r.copy_text(102, CODE);
    let hc = hash(&r, code.id);
    let note = r.copy_text(103, "plain note");
    let hn = hash(&r, note.id);
    sink.take();
    // 「同步验证码」关闭时验证码的内容没有发布：收藏时补发内容，其他设备才取得到
    r.engine.pin(code.id, true).expect("收藏");
    assert_eq!(
        sink.take(),
        vec![SinkCall::Favorite(hc, true, None), SinkCall::Content(hc)]
    );
    r.engine.pin(code.id, false).expect("取消收藏");
    assert_eq!(sink.take(), vec![SinkCall::Favorite(hc, false, None)]);
    // 普通条目的内容早已发布，只写收藏
    r.engine.pin(note.id, true).expect("收藏");
    assert_eq!(sink.take(), vec![SinkCall::Favorite(hn, true, None)]);
}

#[test]
fn a_remote_favorite_brings_back_a_code_that_expired_here() {
    let r = rig();
    let code = r.copy_text(102, CODE);
    let hc = hash(&r, code.id);
    // 本机睡眠醒来：先到期删除（不发布删除）……
    r.host.now.store(
        code.expires_at.expect("过期时间") + 1,
        std::sync::atomic::Ordering::SeqCst,
    );
    r.exec.fire_timers();
    assert!(!r.ids().contains(&code.id));
    // ……再收到另一台设备在到期前的收藏：报告「未接收」，同步层据此取回内容，以收藏导入、不再过期
    let out = r.engine.apply_remote_batch(
        vec![ClipRemoteChange::Pin {
            hash: hc,
            pinned: true,
            name: None,
        }],
        None,
    );
    assert_eq!(out, vec![RemoteOutcome::Evicted]);
    let mut item = remote_text(CODE, T0, Some(true));
    item.hash = hc;
    item.expires_at = code.expires_at;
    let out = r.engine.apply_remote_batch(vec![ClipRemoteChange::Upsert(item)], None);
    assert_eq!(out, vec![RemoteOutcome::Applied]);
    let back = r.top();
    assert!(back.pinned && back.expires_at.is_none(), "{back:?}");
}

#[test]
fn remote_favorites_carry_their_names_into_the_cache() {
    let r = rig();
    let local = r.copy_text(101, "Guest network: wisp-guest");
    r.copy_text(102, "newer");
    r.events();
    let h = hash(&r, local.id);
    let named = |name: Option<&str>| ClipRemoteChange::Pin {
        hash: h,
        pinned: true,
        name: name.map(str::to_owned),
    };
    assert_eq!(
        r.engine.apply_remote_batch(vec![named(Some("Wi-Fi"))], None),
        vec![RemoteOutcome::Applied]
    );
    match r.events().as_slice() {
        [ClipboardChanged::Touched { item }] => {
            assert!(item.pinned);
            assert_eq!(item.name.as_deref(), Some("Wi-Fi"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(r.top().name.as_deref(), Some("Wi-Fi"));
    // 只改名：同样推送（前端更新行内的名称）
    r.engine.apply_remote_batch(vec![named(Some("Office"))], None);
    assert_eq!(r.events().len(), 1);
    assert_eq!(r.top().name.as_deref(), Some("Office"));
    // 状态没变：不推送
    r.engine.apply_remote_batch(vec![named(Some("Office"))], None);
    assert!(r.events().is_empty());
    // 远端插入一条带名称的收藏
    let mut item = remote_text("remote named", T0 + 5, Some(true));
    item.name = Some("Remote".into());
    r.engine.apply_remote_batch(vec![ClipRemoteChange::Upsert(item)], None);
    let page = r.engine.first_page();
    let remote = page
        .items
        .iter()
        .find(|h| h.item.preview == "remote named")
        .expect("插入");
    assert_eq!(remote.item.name.as_deref(), Some("Remote"));
}

#[test]
fn remote_codes_follow_the_local_switch_and_expire_here_too() {
    let r = rig();
    r.copy_text(100, "local");
    let now = r.host.now.load(std::sync::atomic::Ordering::SeqCst);
    let code = |text: &str, exp: Option<i64>| {
        let mut item = remote_text(text, now, None);
        item.expires_at = exp;
        ClipRemoteChange::Upsert(item)
    };
    // 「同步验证码」关闭：其他设备的验证码不导入（带过期时间的、按正文识别出来的都算）
    r.set_settings(|s| s.sync.content.verification_codes = false);
    let out = r.engine.apply_remote_batch(
        vec![
            code("Your verification code is 771203", Some(now + 60_000)),
            code("Your verification code is 552901", None),
            code("ordinary text", None),
        ],
        None,
    );
    assert_eq!(
        out,
        vec![RemoteOutcome::Evicted, RemoteOutcome::Evicted, RemoteOutcome::Applied]
    );
    // 打开：导入，并按记录里的绝对时间到期删除
    r.set_settings(|s| s.sync.content.verification_codes = true);
    let out = r
        .engine
        .apply_remote_batch(vec![code("Your verification code is 771203", Some(now + 60_000))], None);
    assert_eq!(out, vec![RemoteOutcome::Applied]);
    let page = r.engine.first_page();
    let imported = page
        .items
        .iter()
        .find(|h| h.item.preview.contains("771203"))
        .expect("导入")
        .item
        .clone();
    assert_eq!(imported.expires_at, Some(now + 60_000));
    r.host.now.store(now + 60_001, std::sync::atomic::Ordering::SeqCst);
    r.exec.fire_timers();
    assert!(!r.ids().contains(&imported.id));
    // 已经过期的不导入
    let out = r
        .engine
        .apply_remote_batch(vec![code("Your verification code is 330021", Some(now + 1))], None);
    assert_eq!(out, vec![RemoteOutcome::Evicted]);
}

// ---------- 实时同步：把其他设备最新的复制放进本机剪贴板 ----------

/// 开着同步、实时同步与写剪贴板子选项、本机已有一条较早的复制（库已打开）；本机的复制算作 11 s 之前，剪贴板序号与
/// 最近处理的一致（没有未处理的本机变化）
fn realtime_rig() -> Rig {
    let r = rig();
    r.set_settings(|s| {
        s.sync.enabled = true;
        s.sync.realtime = true;
        s.sync.realtime_clipboard = true;
    });
    r.engine.reconcile_watch().expect("安装监听");
    r.copy_text(101, "本机较早的复制");
    settle_local(&r);
    r
}

/// 让本机最近一次复制显得已经过去 11 s，并让剪贴板序号与最近处理过的一致
fn settle_local(r: &Rig) {
    *r.engine.last_local_copy.lock() = Instant::now().checked_sub(LOCAL_QUIET + Duration::from_secs(1));
    if let Some(seen) = r.engine.last_seen_change() {
        r.port.clipboard.change.store(seen, std::sync::atomic::Ordering::SeqCst);
    }
}

fn now(r: &Rig) -> i64 {
    r.host.now.load(std::sync::atomic::Ordering::SeqCst)
}

/// 其他设备的一条复制经同步写进本机历史，返回同步交给剪贴板服务的那一条
fn arrive(r: &Rig, item: RemoteItem, recorded_ms: i64) -> RemoteCopy {
    let copy = RemoteCopy {
        hash: item.hash,
        recorded_ms,
        last_used_ms: item.last_used_at,
    };
    let out = r.engine.apply_remote_batch(vec![ClipRemoteChange::Upsert(item)], None);
    assert_eq!(out, vec![RemoteOutcome::Applied]);
    copy
}

fn arrive_text(r: &Rig, text: &str) -> RemoteCopy {
    let at = now(r);
    arrive(r, remote_text(text, at, None), at)
}

fn written_texts(r: &Rig) -> Vec<Option<String>> {
    r.port.paste.writes.lock().iter().map(|p| p.text.clone()).collect()
}

#[test]
fn the_newest_copy_from_another_device_is_placed_without_echoing_back() {
    let r = realtime_rig();
    let sink = with_sink(&r);
    let older = arrive_text(&r, "另一台设备较早的复制");
    let newest = arrive_text(&r, "另一台设备刚复制的");
    let before = r.ids();
    r.log.lock().clear();
    r.host.events.lock().clear();

    // 交来的顺序无所谓：取最近使用的那一条
    let change = r.engine.place_remote_copy(&[older, newest]).expect("应当写入");
    assert_eq!(written_texts(&r), vec![Some("另一台设备刚复制的".to_owned())]);
    assert_eq!(
        *r.log.lock(),
        vec![Call::Write, Call::Suppress(change)],
        "写入后立即登记自写抑制"
    );
    assert_eq!(r.ids(), before, "历史里不多一条、顺序不变（不 touch）");

    // 监听方随后读到这次变化：带 from_wisp 标记，不记录、不通知同步（不会作为本机复制再上传）
    let echo = RawClipboard {
        change_id: change,
        from_wisp: true,
        text: Some("另一台设备刚复制的".into()),
        ..RawClipboard::default()
    };
    r.copy(change, echo);
    assert_eq!(r.ids(), before);
    assert!(sink.take().is_empty(), "不回传给同步");
    assert!(
        !r.host
            .events
            .lock()
            .iter()
            .any(|e| matches!(e, ClipboardChanged::Inserted { .. } | ClipboardChanged::Touched { .. })),
        "列表不变"
    );
    // 自己写进去的不算「本机刚复制过」，同一条再交来一次时什么都不做
    assert_eq!(r.engine.place_remote_copy(&[newest]), Err(Skip::Current));
    assert_eq!(written_texts(&r).len(), 1);
    // 再来一条更新的照常写入
    let next = arrive_text(&r, "又一条");
    assert!(r.engine.place_remote_copy(&[next]).is_ok());
    assert_eq!(written_texts(&r).len(), 2);
}

#[test]
fn nothing_is_placed_when_the_option_or_recording_is_off() {
    let r = realtime_rig();
    let copy = arrive_text(&r, "另一台设备的复制");
    r.set_settings(|s| s.sync.realtime_clipboard = false);
    assert_eq!(r.engine.place_remote_copy(&[copy]), Err(Skip::Off));
    r.set_settings(|s| {
        s.sync.realtime_clipboard = true;
        s.sync.realtime = false;
    });
    assert_eq!(r.engine.place_remote_copy(&[copy]), Err(Skip::Off));
    r.set_settings(|s| s.sync.realtime = true);
    // 开关都开着、但设置页把它显示为关的情形：关掉了自动同步、不同步剪贴板历史
    r.set_settings(|s| s.sync.auto = false);
    assert_eq!(r.engine.place_remote_copy(&[copy]), Err(Skip::Off));
    r.set_settings(|s| {
        s.sync.auto = true;
        s.sync.content.clipboard = false;
    });
    assert_eq!(r.engine.place_remote_copy(&[copy]), Err(Skip::Off));
    r.set_settings(|s| s.sync.content.clipboard = true);
    r.engine.set_paused(true);
    assert_eq!(r.engine.place_remote_copy(&[copy]), Err(Skip::NotRecording));
    r.engine.set_paused(false);
    r.set_settings(|s| s.clipboard.enabled = false);
    assert_eq!(r.engine.place_remote_copy(&[copy]), Err(Skip::NotRecording));
    // 监听没装上：看不到本机的复制，不写
    r.engine.reconcile_watch().expect("卸下监听");
    assert!(!r.engine.watching());
    r.set_settings(|s| s.clipboard.enabled = true);
    assert_eq!(r.engine.place_remote_copy(&[copy]), Err(Skip::NotRecording));
    r.engine.reconcile_watch().expect("安装监听");
    assert!(written_texts(&r).is_empty());
    // 子选项关着时连后台任务都不排
    r.set_settings(|s| {
        s.clipboard.enabled = true;
        s.sync.realtime_clipboard = false;
    });
    r.exec.queue(true);
    r.engine.offer_remote_copies(vec![copy]);
    assert_eq!(r.exec.pending_jobs(), 0);
    r.set_settings(|s| {
        s.sync.realtime_clipboard = true;
        s.sync.auto = false;
    });
    r.engine.offer_remote_copies(vec![copy]);
    assert_eq!(r.exec.pending_jobs(), 0, "自动同步关着：同样不排");
    r.set_settings(|s| s.sync.auto = true);
    r.engine.offer_remote_copies(vec![copy]);
    assert_eq!(r.exec.pending_jobs(), 1, "在阻塞线程上判断与写入");
    r.exec.run_jobs();
    assert_eq!(written_texts(&r), vec![Some("另一台设备的复制".to_owned())]);
}

#[test]
fn an_old_copy_or_a_recent_local_copy_is_left_alone() {
    let r = realtime_rig();
    // 在那台设备上产生超过 1 分钟（长时间离开后补拉的）
    let at = now(&r);
    let stale = arrive(&r, remote_text("一分多钟前的复制", at, None), at - 61_000);
    assert_eq!(r.engine.place_remote_copy(&[stale]), Err(Skip::Stale));

    // 本机 10 s 内刚复制过
    let copy = arrive_text(&r, "另一台设备的复制");
    r.copy_text(102, "本机刚复制的");
    settle_local(&r);
    *r.engine.last_local_copy.lock() = Some(Instant::now());
    let fresh = arrive_text(&r, "另一台设备又复制了");
    assert_eq!(r.engine.place_remote_copy(&[fresh]), Err(Skip::LocalCopy));
    // 剪贴板有还没处理的变化（轮询还没读到的本机复制）
    settle_local(&r);
    r.port
        .clipboard
        .change
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(r.engine.place_remote_copy(&[fresh]), Err(Skip::LocalCopy));
    settle_local(&r);
    // 本机后来又复制过（时间更新）：那一条不是最新的
    assert_eq!(r.engine.place_remote_copy(&[copy]), Err(Skip::Older));
    assert!(written_texts(&r).is_empty());
    assert!(r.engine.place_remote_copy(&[fresh]).is_ok());
}

#[test]
fn codes_partial_texts_and_missing_items_are_never_placed() {
    let r = realtime_rig();
    let at = now(&r);
    let code = arrive(
        &r,
        remote_text("验证码：7319（10分钟内有效），请勿泄露给他人", at, None),
        at,
    );
    assert_eq!(r.engine.place_remote_copy(&[code]), Err(Skip::Sensitive));

    let at = now(&r);
    let mut long = remote_text("超长被截断的文字", at, None);
    long.truncated = true;
    let long = arrive(&r, long, at);
    assert_eq!(r.engine.place_remote_copy(&[long]), Err(Skip::Partial));

    let gone = RemoteCopy {
        hash: [9; 32],
        recorded_ms: now(&r),
        last_used_ms: now(&r) + 10,
    };
    assert_eq!(r.engine.place_remote_copy(&[gone]), Err(Skip::Missing));
    assert!(written_texts(&r).is_empty());
}

#[test]
fn a_paste_in_progress_is_never_disturbed() {
    let r = realtime_rig();
    let id = r.top().id;
    let copy = arrive_text(&r, "另一台设备的复制");
    let seen = Arc::new(Mutex::new(None));
    let engine = Arc::clone(&r.engine);
    let out = Arc::clone(&seen);
    // 粘贴正在写剪贴板时同步交来一条：不写，粘贴照常完成
    *r.port.paste.during_write.lock() = Some(Box::new(move || {
        *out.lock() = Some(engine.place_remote_copy(&[copy]));
    }));
    r.paste(id, PasteMode::Normal, Some(app("com.example.target")))
        .expect("粘贴");
    assert_eq!(*seen.lock(), Some(Err(Skip::Pasting)));
    assert_eq!(
        written_texts(&r),
        vec![Some("本机较早的复制".to_owned())],
        "只有粘贴的那一次写入"
    );
    // 刚粘贴过：剪贴板里是用户选的内容，暂时不替换
    settle_clipboard_only(&r);
    assert_eq!(r.engine.place_remote_copy(&[copy]), Err(Skip::LocalCopy));
}

/// 只让剪贴板序号与最近处理过的一致（不改「最近一次本机复制」的时刻）
fn settle_clipboard_only(r: &Rig) {
    if let Some(seen) = r.engine.last_seen_change() {
        r.port.clipboard.change.store(seen, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 判断之后、写入之前用户又复制了别的：不写（否则那次复制被盖掉，监听随后读到的是带自写标记的内容，连历史里都没有）
#[test]
fn a_copy_made_after_the_decision_is_never_overwritten() {
    let r = realtime_rig();
    let copy = arrive_text(&r, "另一台设备的复制");
    let decided = r.port.clipboard.change.load(std::sync::atomic::Ordering::SeqCst);
    let payload = ClipPayload {
        text: Some("另一台设备的复制".into()),
        ..ClipPayload::default()
    };
    // 用户在判断与写入之间复制了一段
    r.port
        .clipboard
        .change
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        r.engine.write_quietly(&payload, decided).expect("write"),
        QuietWrite::Changed
    );
    assert!(written_texts(&r).is_empty(), "没有写");
    assert!(!r.engine.paste_busy(), "放开了单飞闸门");
    // 序号一致时照常写入
    settle_local(&r);
    assert!(r.engine.place_remote_copy(&[copy]).is_ok());
    assert_eq!(written_texts(&r), vec![Some("另一台设备的复制".to_owned())]);
}

#[test]
fn a_failed_write_is_reported_and_retried_next_time() {
    let r = realtime_rig();
    let copy = arrive_text(&r, "另一台设备的复制");
    *r.port.paste.write_error.lock() = Some(wisp_platform::PlatformError::Other("busy".into()));
    assert_eq!(r.engine.place_remote_copy(&[copy]), Err(Skip::WriteFailed));
    assert!(r.engine.remote_written().is_none());
    assert!(
        r.engine.place_remote_copy(&[copy]).is_ok(),
        "失败不留下「已写入」的记录"
    );
}
