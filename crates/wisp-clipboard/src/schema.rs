//! 数据库迁移（rusqlite_migration，`PRAGMA user_version`）与连接配置。
//!
//! - M1：`items`（基础字段之外另加 `truncated`、`icon_key`、`files_head_json`，以及虚拟生成列 `search_text`）、
//!   `idx_items_recent`、`items_fts USING fts5(search_text, content='items', content_rowid='id',
//!   tokenize='trigram case_sensitive 0')` 及 ai / ad / au 三个触发器（应用代码不手工写 FTS）；
//! - M2：`ocr_history`。
//!
//! **列的顺序**：列表要用的小字段在前，大字段（`text`、`html`、`rtf`、完整的 `files_json`）放在最后。SQLite 读某一列时
//! 要沿着溢出页链走到这一列所在的位置：小字段排在 1 MB 正文后面时，每读一行列表元数据都要先读完约 256 个溢出页
//! （实测 200 条中有 29 条 1 MB 长文本时 `recent(0, 200)` 要 4.7 ms，排在前面后约 0.01 ms）。
//! `files_head_json` 只存前 5 个文件（列表元数据用），完整列表只在详情与粘贴时读取。
//!
//! **检索范围**：FTS 只索引正文的前 [`SEARCH_PREFIX_CHARS`] 个字符（`search_text` = `substr(text, 1, 32768)`，
//! 虚拟列不占存储）。trigram 索引的开销随文本长度超线性增长：多样的 1 MB 英文要 100 ms 以上（期间持有存储锁，
//! 检索和翻页都要等），前 32768 字符约 2–3 ms；短查询的 LIKE 与结果片段也只看同一段前缀，三者一致。
//!
//! 每个迁移都带 down，便于测试「0 → 最新 → 0 → 最新」可重复执行。

use std::sync::LazyLock;

use rusqlite::Connection;
use rusqlite::functions::{Context, FunctionFlags};
use rusqlite::types::ValueRef;
use rusqlite_migration::{M, Migrations};

use crate::error::StoreError;
use crate::search::LikePattern;

/// 检索覆盖的正文前缀（字符）：FTS 索引（M1 的 `search_text`）、短查询的 LIKE、结果片段都只看这一段。
/// 改动时必须同步 M1 里的 `substr(text, 1, 32768)`（单测比对）
pub(crate) const SEARCH_PREFIX_CHARS: u32 = 32_768;

const M1_UP: &str = r"
CREATE TABLE items (
  id INTEGER PRIMARY KEY,
  kind TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  last_used_at INTEGER NOT NULL,
  use_count INTEGER NOT NULL DEFAULT 0,
  pinned INTEGER NOT NULL DEFAULT 0,
  truncated INTEGER NOT NULL DEFAULT 0,
  source_app_id TEXT,
  source_app_name TEXT,
  icon_key TEXT,
  hash BLOB NOT NULL UNIQUE,
  text_preview TEXT,
  meta_json TEXT,
  image_blob TEXT,
  image_w INTEGER,
  image_h INTEGER,
  image_bytes INTEGER,
  thumb_blob TEXT,
  files_head_json TEXT,
  text TEXT,
  html TEXT,
  rtf BLOB,
  files_json TEXT,
  search_text TEXT GENERATED ALWAYS AS (substr(text, 1, 32768)) VIRTUAL
);
CREATE INDEX idx_items_recent ON items(pinned DESC, last_used_at DESC);
CREATE VIRTUAL TABLE items_fts USING fts5(
  search_text,
  content='items',
  content_rowid='id',
  tokenize='trigram case_sensitive 0'
);
INSERT INTO items_fts(items_fts, rank) VALUES ('secure-delete', 1);
INSERT INTO items_fts(items_fts, rank) VALUES ('automerge', 8);
CREATE TRIGGER items_ai AFTER INSERT ON items BEGIN
  INSERT INTO items_fts(rowid, search_text) VALUES (new.id, new.search_text);
END;
CREATE TRIGGER items_ad AFTER DELETE ON items BEGIN
  INSERT INTO items_fts(items_fts, rowid, search_text) VALUES ('delete', old.id, old.search_text);
END;
CREATE TRIGGER items_au AFTER UPDATE OF text ON items BEGIN
  INSERT INTO items_fts(items_fts, rowid, search_text) VALUES ('delete', old.id, old.search_text);
  INSERT INTO items_fts(rowid, search_text) VALUES (new.id, new.search_text);
END;
";

const M1_DOWN: &str = r"
DROP TRIGGER items_au;
DROP TRIGGER items_ad;
DROP TRIGGER items_ai;
DROP TABLE items_fts;
DROP INDEX idx_items_recent;
DROP TABLE items;
";

const M2_UP: &str = r"
CREATE TABLE ocr_history (
  id INTEGER PRIMARY KEY,
  created_at INTEGER NOT NULL,
  mode TEXT NOT NULL,
  model TEXT NOT NULL,
  text TEXT NOT NULL,
  thumb_blob TEXT,
  image_blob TEXT
);
CREATE INDEX idx_ocr_history_created ON ocr_history(created_at DESC);
";

const M2_DOWN: &str = r"
DROP INDEX idx_ocr_history_created;
DROP TABLE ocr_history;
";

/// 全部迁移（按顺序；只能追加，不能修改已发布的条目）
pub(crate) static MIGRATIONS: LazyLock<Migrations<'static>> =
    LazyLock::new(|| Migrations::new(vec![M::up(M1_UP).down(M1_DOWN), M::up(M2_UP).down(M2_DOWN)]));

/// 最新的 schema 版本（= 迁移条数，单测比对）
pub(crate) const LATEST_VERSION: u32 = 2;

/// 删除行时同步 FTS 的触发器（批量删除走「重建」时在事务内临时去掉它，见 `store.rs` 的 `FtsPurge`）
pub(crate) const FTS_DELETE_TRIGGER: &str = "items_ad";

/// 打开 / 关闭 FTS5 的 secure-delete（写在 `items_fts_config` 里，随事务提交或回滚）。
/// 打开时删除直接从段里抹掉条目；关闭时只写删除标记，被删条目要等段合并时才消失
pub(crate) fn set_fts_secure_delete(conn: &Connection, on: bool) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO items_fts(items_fts, rank) VALUES ('secure-delete', ?1)",
        [i64::from(on)],
    )?;
    Ok(())
}

/// 连接级配置：WAL、`synchronous=NORMAL`、`foreign_keys=ON`，以及与内存 / 体积 / 隐私相关的几项：
/// `secure_delete=FAST`（日常写入只清零 b-tree 页内被删的内容，不增加 I/O；删除 / 清空时由 `ClipStore` 临时切到 `ON`，
/// 连整页释放的页和溢出页一起清零）、WAL 文件上限 4 MB、页缓存 2 MB；并注册 Unicode 版 `like()`（见 [`register_functions`]）。
///
/// `temp_store = FILE`：临时库放在系统临时目录的文件里（只在页缓存放不下时才真正落盘）。VACUUM 要把整个库复制进
/// 临时库：放在内存里时，常驻进程的峰值内存约等于库的大小（实测 110 MB 的库 VACUUM 时 RSS 从 3.5 MB 升到 126 MB）
pub(crate) fn configure(conn: &Connection) -> Result<(), StoreError> {
    // WAL 必须在事务外设置；返回值是实际模式（内存库为 memory），不强求
    let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    conn.execute_batch(
        "PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = FILE;
         PRAGMA secure_delete = FAST;
         PRAGMA journal_size_limit = 4194304;
         PRAGMA cache_size = -2048;",
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    conn.set_prepared_statement_cache_capacity(64);
    register_functions(conn)?;
    Ok(())
}

/// 用 Rust 实现覆盖内置的 `like(pattern, text [, escape])`：语义不变（`%`、`_`、`ESCAPE`），大小写折叠从「只折叠 ASCII」
/// 扩展为与 FTS5 trigram（`case_sensitive 0`）和高亮一致的 Unicode 单字符折叠——否则 1–2 字查询（走 LIKE）搜不到
/// 「Дж」对应的「дж」，3 字以上（走 FTS）却能搜到，结果随查询长度跳变。SQLite 的 ICU 扩展也是这样覆盖 `like()` 的；
/// 代价是 LIKE 不能再用索引做前缀优化，本库的 LIKE 只用于短查询的全扫描，不受影响
pub(crate) fn register_functions(conn: &Connection) -> Result<(), StoreError> {
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC | FunctionFlags::SQLITE_INNOCUOUS;
    conn.create_scalar_function("like", 2, flags, sql_like)?;
    conn.create_scalar_function("like", 3, flags, sql_like)?;
    Ok(())
}

/// `like(pattern, text [, escape])`：任一参数为 NULL 时结果为 NULL；编译后的模式缓存在语句的辅助数据里
fn sql_like(ctx: &Context<'_>) -> rusqlite::Result<Option<bool>> {
    let escape = if ctx.len() == 3 {
        match ctx.get_raw(2) {
            ValueRef::Null => return Ok(None),
            v => {
                let e = v.as_str().map_err(|e| rusqlite::Error::UserFunctionError(e.into()))?;
                let mut chars = e.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => Some(c),
                    _ => {
                        return Err(rusqlite::Error::UserFunctionError(
                            "ESCAPE expression must be a single character".into(),
                        ));
                    }
                }
            }
        }
    } else {
        None
    };
    if matches!(ctx.get_raw(0), ValueRef::Null) || matches!(ctx.get_raw(1), ValueRef::Null) {
        return Ok(None);
    }
    let pattern = ctx.get_or_create_aux(0, |v| -> Result<LikePattern, rusqlite::Error> {
        Ok(LikePattern::compile(&value_text(v), escape))
    })?;
    if pattern.escape() != escape {
        // 同一条语句里换了 ESCAPE（不会发生在本库的 SQL 中）：不用缓存
        return Ok(Some(
            LikePattern::compile(&value_text(ctx.get_raw(0)), escape).matches(&value_text(ctx.get_raw(1))),
        ));
    }
    Ok(Some(pattern.matches(&value_text(ctx.get_raw(1)))))
}

/// 参数转成文本（数值按 SQLite 的文本形式；非法 UTF-8 做有损转换）
fn value_text(v: ValueRef<'_>) -> std::borrow::Cow<'_, str> {
    match v {
        ValueRef::Text(t) | ValueRef::Blob(t) => String::from_utf8_lossy(t),
        ValueRef::Integer(i) => i.to_string().into(),
        ValueRef::Real(f) => f.to_string().into(),
        ValueRef::Null => "".into(),
    }
}

/// 迁移到最新版本（可重复执行）。库的版本比本程序新（用户降级）时返回 [`StoreError::SchemaTooNew`]，不动库文件
pub(crate) fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    let found: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if found > i64::from(LATEST_VERSION) {
        return Err(StoreError::SchemaTooNew {
            found,
            supported: LATEST_VERSION,
        });
    }
    MIGRATIONS.to_latest(conn)?;
    Ok(())
}

/// FTS 自检：失败时用 `'rebuild'` 重建索引，不删库。返回是否进行了重建。
///
/// - 常规打开（`deep = false`）：只比对行数（external content 表的每一行在 docsize 中都有一条记录），O(行数)、
///   1 万条约 1 ms；触发器与 items 写入在同一事务中，常规路径不会失步，行数比对足以发现绕过触发器的写入；
/// - 深度检查（`deep = true`，上次运行中遇到过损坏时）：再加 `integrity-check`（索引结构，开销与索引大小成正比）。
///
/// 运行中索引结构损坏会在读写时以 SQLITE_CORRUPT_VTAB 暴露，由 `ClipStore` 当场重建并重试
pub(crate) fn ensure_fts_healthy(conn: &Connection, deep: bool) -> Result<bool, StoreError> {
    if fts_healthy(conn, deep) {
        return Ok(false);
    }
    log::warn!("clipboard fts index inconsistent; rebuilding");
    rebuild_fts(conn)?;
    Ok(true)
}

/// 重建 FTS 索引
pub(crate) fn rebuild_fts(conn: &Connection) -> Result<(), StoreError> {
    conn.execute("INSERT INTO items_fts(items_fts) VALUES ('rebuild')", [])?;
    Ok(())
}

fn fts_healthy(conn: &Connection, deep: bool) -> bool {
    if deep
        && conn
            .execute(
                "INSERT INTO items_fts(items_fts, rank) VALUES ('integrity-check', 0)",
                [],
            )
            .is_err()
    {
        return false;
    }
    let counts: rusqlite::Result<(i64, i64)> = conn.query_row(
        "SELECT (SELECT count(*) FROM items), (SELECT count(*) FROM items_fts_docsize)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    );
    matches!(counts, Ok((a, b)) if a == b)
}

/// 完整的一致性检查：把索引与 content 表逐行比对（开销与文本总量成正比，测试用）
#[cfg(test)]
pub(crate) fn fts_integrity_check_full(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO items_fts(items_fts, rank) VALUES ('integrity-check', 1)",
        [],
    )
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Connection {
        let mut c = Connection::open_in_memory().expect("内存库");
        configure(&c).expect("配置");
        migrate(&mut c).expect("迁移");
        c
    }

    #[test]
    fn migrations_are_valid() {
        MIGRATIONS.validate().expect("迁移定义合法");
        let c = fresh();
        let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).expect("版本");
        assert_eq!(v, i64::from(LATEST_VERSION));
        assert!(
            M1_UP.contains(&format!("substr(text, 1, {SEARCH_PREFIX_CHARS})")),
            "M1 的检索前缀与 SEARCH_PREFIX_CHARS 不一致"
        );
    }

    #[test]
    fn temporary_data_is_kept_on_disk() {
        // VACUUM 的临时库不能放在内存里（峰值内存会约等于库的大小）
        let c = fresh();
        let t: i64 = c.query_row("PRAGMA temp_store", [], |r| r.get(0)).expect("temp_store");
        assert_eq!(t, 1);
    }

    #[test]
    fn fts_secure_delete_survives_a_rolled_back_toggle() {
        let mut c = fresh();
        assert!(M1_UP.contains(&format!("CREATE TRIGGER {FTS_DELETE_TRIGGER} AFTER DELETE ON items")));
        for i in 0..20 {
            c.execute(
                "INSERT INTO items(kind, created_at, last_used_at, hash, text) VALUES ('text', 1, 1, ?1, ?2)",
                rusqlite::params![vec![i as u8; 4], format!("entry {i} with some searchable words")],
            )
            .expect("插入");
        }
        let config = |c: &Connection| -> i64 {
            c.query_row("SELECT v FROM items_fts_config WHERE k = 'secure-delete'", [], |r| {
                r.get(0)
            })
            .expect("配置")
        };
        assert_eq!(config(&c), 1);
        {
            let tx = c.transaction().expect("事务");
            set_fts_secure_delete(&tx, false).expect("关闭");
            assert_eq!(config(&tx), 0);
        } // 未提交：回滚
        assert_eq!(config(&c), 1);
        // 行为上也恢复了：secure-delete 下删除是就地改写段，不会新增一个只含删除标记的段
        let blocks = |c: &Connection| -> i64 {
            c.query_row("SELECT count(*) FROM items_fts_data", [], |r| r.get(0))
                .expect("段")
        };
        let before = blocks(&c);
        c.execute("DELETE FROM items WHERE id = 3", []).expect("删除");
        assert!(blocks(&c) <= before, "回滚后 FTS 仍按关闭 secure-delete 的方式删除");
        fts_integrity_check_full(&c).expect("索引一致");
    }

    #[test]
    fn newer_schema_is_rejected_without_touching_it() {
        let mut c = Connection::open_in_memory().expect("内存库");
        c.execute_batch("PRAGMA user_version = 9;").expect("版本");
        assert!(matches!(
            migrate(&mut c),
            Err(StoreError::SchemaTooNew {
                found: 9,
                supported: LATEST_VERSION
            })
        ));
        let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).expect("版本");
        assert_eq!(v, 9);
    }

    #[test]
    fn like_is_unicode_case_insensitive_and_keeps_sql_semantics() {
        let c = fresh();
        let like = |text: &str, pattern: &str| -> Option<bool> {
            c.query_row("SELECT ?1 LIKE ?2 ESCAPE '\\'", [text, pattern], |r| r.get(0))
                .expect("LIKE")
        };
        assert_eq!(like("Джон", "%дж%"), Some(true));
        assert_eq!(like("ÉCOLE", "%é%"), Some(true));
        assert_eq!(like("Σίσυφος", "σ%"), Some(true));
        assert_eq!(like("abc", "a_c"), Some(true));
        assert_eq!(like("abc", "a\\_c"), Some(false));
        assert_eq!(like("a_c", "a\\_c"), Some(true));
        assert_eq!(like("50%", "%0\\%"), Some(true));
        assert_eq!(like("abc", "%d%"), Some(false));
        let null: Option<bool> = c.query_row("SELECT NULL LIKE 'a'", [], |r| r.get(0)).expect("NULL");
        assert_eq!(null, None);
        // 两参数形式与 NOT LIKE
        let n: i64 = c
            .query_row("SELECT ('ÄBC' LIKE 'äb%') + ('x' NOT LIKE 'X')", [], |r| r.get(0))
            .expect("LIKE");
        assert_eq!(n, 1);
        assert!(
            c.query_row("SELECT 'a' LIKE 'a' ESCAPE 'xy'", [], |r| r.get::<_, bool>(0))
                .is_err()
        );
    }

    #[test]
    fn migrations_round_trip_repeatedly() {
        let mut c = Connection::open_in_memory().expect("内存库");
        configure(&c).expect("配置");
        for _ in 0..3 {
            migrate(&mut c).expect("0 → 最新");
            migrate(&mut c).expect("重复执行无副作用");
            c.execute(
                "INSERT INTO items(kind, created_at, last_used_at, hash, text) VALUES ('text', 1, 1, x'02', 'x')",
                [],
            )
            .expect("可写");
            MIGRATIONS.to_version(&mut c, 0).expect("最新 → 0");
            let tables: i64 = c
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name IN ('items', 'ocr_history')",
                    [],
                    |r| r.get(0),
                )
                .expect("查询");
            assert_eq!(tables, 0);
        }
    }

    #[test]
    fn fts_rebuild_repairs_desync() {
        let c = fresh();
        c.execute(
            "INSERT INTO items(kind, created_at, last_used_at, hash, text) VALUES ('text', 1, 1, x'01', 'hello world')",
            [],
        )
        .expect("插入");
        assert!(!ensure_fts_healthy(&c, true).expect("自检"));
        // 绕过触发器制造不一致：从索引里删掉这一行
        c.execute(
            "INSERT INTO items_fts(items_fts, rowid, search_text) VALUES ('delete', 1, 'hello world')",
            [],
        )
        .expect("删除索引");
        assert!(ensure_fts_healthy(&c, false).expect("自检"));
        fts_integrity_check_full(&c).expect("重建后一致");
        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM items_fts WHERE items_fts MATCH '\"llo\"'",
                [],
                |r| r.get(0),
            )
            .expect("查询");
        assert_eq!(n, 1);
    }
}
