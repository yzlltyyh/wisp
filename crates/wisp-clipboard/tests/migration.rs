//! 数据库迁移测试：从 0 迁移到最新、可重复执行；表结构与设计一致（FTS 由触发器维护）；损坏的库被备份并重建。

mod common;

use common::{Fixture, NOW, list_files};
use wisp_clipboard::{ClipStore, StoreError};
use wisp_types::ClipFilter;

/// items 的字段（含 truncated、icon_key、files_head_json）。列表要用的小字段在前、大字段在后
/// （见 schema.rs「列的顺序」：列表查询因此不必沿溢出页链读过长正文）
const ITEMS_COLUMNS: [&str; 23] = [
    "id",
    "kind",
    "created_at",
    "last_used_at",
    "use_count",
    "pinned",
    "truncated",
    "source_app_id",
    "source_app_name",
    "icon_key",
    "hash",
    "text_preview",
    "meta_json",
    "image_blob",
    "image_w",
    "image_h",
    "image_bytes",
    "thumb_blob",
    "files_head_json",
    "text",
    "html",
    "rtf",
    "files_json",
];

fn strings(c: &rusqlite::Connection, sql: &str) -> Vec<String> {
    c.prepare(sql)
        .expect("语句")
        .query_map([], |r| r.get(0))
        .expect("查询")
        .collect::<Result<_, _>>()
        .expect("结果")
}

#[test]
fn fresh_database_has_full_schema() {
    let f = Fixture::new();
    let c = f.side_conn();
    let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).expect("版本");
    assert_eq!(v, 2);

    assert_eq!(
        strings(&c, "SELECT name FROM pragma_table_info('items') ORDER BY cid"),
        ITEMS_COLUMNS
    );

    // idx_items_recent ON items(pinned DESC, last_used_at DESC)
    let indexes = strings(&c, "SELECT name FROM pragma_index_list('items')");
    assert!(indexes.iter().any(|n| n == "idx_items_recent"), "{indexes:?}");
    let idx_cols: Vec<(String, i64)> = c
        .prepare("SELECT name, desc FROM pragma_index_xinfo('idx_items_recent') WHERE key = 1 ORDER BY seqno")
        .expect("语句")
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("查询")
        .collect::<Result<_, _>>()
        .expect("索引列");
    assert_eq!(idx_cols, [("pinned".to_owned(), 1), ("last_used_at".to_owned(), 1)]);

    // 三个触发器
    assert_eq!(
        strings(
            &c,
            "SELECT name FROM sqlite_master WHERE type = 'trigger' AND tbl_name = 'items' ORDER BY name"
        ),
        ["items_ad", "items_ai", "items_au"]
    );

    // FTS 表用 trigram，external content；只索引正文前缀（虚拟生成列，不占存储）
    let sql: String = c
        .query_row("SELECT sql FROM sqlite_master WHERE name = 'items_fts'", [], |r| {
            r.get(0)
        })
        .expect("fts");
    assert!(
        sql.contains("trigram case_sensitive 0")
            && sql.contains("content='items'")
            && sql.contains("content_rowid='id'"),
        "{sql}"
    );
    let hidden: Vec<(String, i64)> = c
        .prepare("SELECT name, hidden FROM pragma_table_xinfo('items') WHERE hidden != 0")
        .expect("语句")
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("查询")
        .collect::<Result<_, _>>()
        .expect("生成列");
    assert_eq!(hidden, [("search_text".to_owned(), 2)], "search_text 应是虚拟生成列");

    // M2：ocr_history(id, created_at, mode, model, text, thumb_blob, image_blob)
    assert_eq!(
        strings(&c, "SELECT name FROM pragma_table_info('ocr_history') ORDER BY cid"),
        ["id", "created_at", "mode", "model", "text", "thumb_blob", "image_blob"]
    );

    let mode: String = c.query_row("PRAGMA journal_mode", [], |r| r.get(0)).expect("模式");
    assert_eq!(mode, "wal");

    for d in ["img", "thumb", "icons", "ocr"] {
        assert!(f.dir.path().join("blobs").join(d).is_dir(), "缺少 blobs/{d}");
    }
}

#[test]
fn reopening_is_idempotent_and_keeps_data() {
    let f = Fixture::new();
    f.text("hello 你好", NOW);
    let Fixture { dir, store } = f;
    drop(store);
    for _ in 0..3 {
        let store = ClipStore::open(dir.path()).expect("重复打开");
        let page = store.recent(0, 10, ClipFilter::All).expect("列表");
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].item.preview, "hello 你好");
    }
}

#[test]
fn migrates_existing_version_zero_file() {
    // 已经存在、但 user_version = 0 的库
    let dir = tempfile::tempdir().expect("临时目录");
    {
        let c = rusqlite::Connection::open(dir.path().join("clipboard.db")).expect("建库");
        c.execute_batch("CREATE TABLE unrelated(x);").expect("初始化");
    }
    let store = ClipStore::open(dir.path()).expect("迁移");
    let c = rusqlite::Connection::open(dir.path().join("clipboard.db")).expect("连接");
    let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).expect("版本");
    assert_eq!(v, 2);
    drop(store);
}

#[test]
fn corrupt_database_is_backed_up_and_recreated() {
    let dir = tempfile::tempdir().expect("临时目录");
    std::fs::write(dir.path().join("clipboard.db"), [0x5a_u8; 8192]).expect("写垃圾");
    let store = ClipStore::open(dir.path()).expect("损坏的库不应导致失败");
    assert!(store.recent(0, 10, ClipFilter::All).expect("新库可用").items.is_empty());
    let files = list_files(dir.path());
    assert!(
        files.iter().any(|n| n.starts_with("clipboard.db.corrupt-")),
        "应有备份：{files:?}"
    );

    // 再损坏一次：只保留最近一份备份
    drop(store);
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(dir.path().join(format!("clipboard.db{suffix}")));
    }
    std::fs::write(dir.path().join("clipboard.db"), [0xa5_u8; 8192]).expect("写垃圾");
    let store = ClipStore::open(dir.path()).expect("再次重建");
    let backups = list_files(dir.path())
        .into_iter()
        .filter(|n| n.starts_with("clipboard.db.corrupt-"))
        .count();
    assert_eq!(backups, 1);
    drop(store);
}

#[test]
fn newer_schema_is_reported_and_left_untouched() {
    // 用户从新版降级：不是损坏，不能改名或重建（否则再升级回去时历史就没了）
    let f = Fixture::new();
    f.text("history from the newer version", NOW);
    let Fixture { dir, store } = f;
    drop(store);
    {
        let c = rusqlite::Connection::open(dir.path().join("clipboard.db")).expect("连接");
        c.execute_batch("PRAGMA user_version = 99;").expect("版本");
    }
    let before = list_files(dir.path());
    for _ in 0..2 {
        match ClipStore::open(dir.path()) {
            Err(StoreError::SchemaTooNew { found: 99, supported }) => assert_eq!(supported, 2),
            other => panic!("应返回 SchemaTooNew：{:?}", other.map(|_| ())),
        }
    }
    assert_eq!(list_files(dir.path()), before, "不应改名、备份或新建文件");
    // 再「升级」回来（这里把版本号改回去模拟）：历史完好
    {
        let c = rusqlite::Connection::open(dir.path().join("clipboard.db")).expect("连接");
        c.execute_batch("PRAGMA user_version = 2;").expect("版本");
    }
    let store = ClipStore::open(dir.path()).expect("打开");
    assert_eq!(store.recent(0, 10, ClipFilter::All).expect("列表").items.len(), 1);
}

#[test]
fn fts_desync_is_rebuilt_on_open() {
    let f = Fixture::new();
    let m = f.text("needle in a haystack", NOW);
    // 绕过触发器：直接从索引删掉这一行（模拟外部工具修改或旧版本缺陷）
    f.side_conn()
        .execute(
            "INSERT INTO items_fts(items_fts, rowid, search_text) VALUES ('delete', ?1, 'needle in a haystack')",
            [m.id],
        )
        .expect("破坏索引");
    let Fixture { dir, store } = f;
    drop(store);
    let store = ClipStore::open(dir.path()).expect("重新打开");
    let page = store.search("needle", ClipFilter::All, 10).expect("搜索");
    assert_eq!(page.items.len(), 1, "重建后应能搜到");
    let c = rusqlite::Connection::open(dir.path().join("clipboard.db")).expect("连接");
    c.execute(
        "INSERT INTO items_fts(items_fts, rank) VALUES ('integrity-check', 1)",
        [],
    )
    .expect("integrity-check 通过");
}

#[test]
fn locked_directory_errors_are_reported_not_swallowed() {
    // 数据目录是一个文件：无法创建，应返回错误而不是 panic
    let dir = tempfile::tempdir().expect("临时目录");
    let file = dir.path().join("not-a-dir");
    std::fs::write(&file, b"x").expect("写文件");
    assert!(ClipStore::open(&file).is_err());
}

#[test]
fn runtime_fts_corruption_is_rebuilt_transparently() {
    let f = Fixture::new();
    for i in 0..60 {
        f.text(&format!("entry {i} with shared words"), NOW + i);
    }
    // 把 FTS 的段数据清零（保留 id ≤ 10 的平均值与结构记录）：读写索引时报 SQLITE_CORRUPT_VTAB
    f.side_conn()
        .execute(
            "UPDATE items_fts_data SET block = zeroblob(length(block)) WHERE id > 10",
            [],
        )
        .expect("破坏索引");
    let p = f
        .store
        .search("shared words", ClipFilter::All, 100)
        .expect("搜索应自动重建索引后成功");
    assert_eq!(p.items.len(), 60);
    f.text("written after corruption", NOW + 100);
    assert_eq!(
        f.store
            .search("after corruption", ClipFilter::All, 10)
            .expect("搜索")
            .items
            .len(),
        1
    );
    f.side_conn()
        .execute(
            "INSERT INTO items_fts(items_fts, rank) VALUES ('integrity-check', 1)",
            [],
        )
        .expect("integrity-check");
}

#[test]
fn recovery_marker_triggers_deep_check_once() {
    let f = Fixture::new();
    f.text("keep me", NOW);
    let Fixture { dir, store } = f;
    drop(store);
    let marker = dir.path().join("clipboard.db.recover");
    std::fs::write(&marker, b"").expect("写标记");
    let store = ClipStore::open(dir.path()).expect("深度检查后打开");
    assert!(!marker.exists(), "检查通过后清除标记");
    assert_eq!(
        store.recent(0, 10, ClipFilter::All).expect("列表").items.len(),
        1,
        "健康的库保持原样"
    );
}

#[test]
fn damaged_pages_are_recovered_on_open() {
    let f = Fixture::new();
    for i in 0..400 {
        f.text(
            &format!("{i} {}", "padding text to spread rows over many pages ".repeat(4)),
            NOW + i,
        );
    }
    let Fixture { dir, store } = f;
    drop(store);
    let db = dir.path().join("clipboard.db");
    let mut bytes = std::fs::read(&db).expect("读库");
    assert!(bytes.len() > 64 * 4096);
    // 保留第 1 页（文件头与 schema），其余页全部写坏
    for b in &mut bytes[4096..] {
        *b = 0xA5;
    }
    std::fs::write(&db, &bytes).expect("写坏");
    let store = ClipStore::open(dir.path()).expect("损坏页不应导致失败");
    assert!(store.recent(0, 10, ClipFilter::All).expect("新库可用").items.is_empty());
    assert!(
        list_files(dir.path())
            .iter()
            .any(|n| n.starts_with("clipboard.db.corrupt-"))
    );
}
