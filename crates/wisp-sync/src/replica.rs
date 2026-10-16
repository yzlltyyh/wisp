//! 本机副本 sync.db（SQLite，WAL）。
//!
//! 与 clipboard.db 同目录；同步关闭时不打开，断开时删除。保存：
//! - `meta`：HLC、vault_id、本机 device_id、各域的加入状态、配置影子与指纹、写入编号与序号的分配游标、
//!   vault.json 与本机 head 的签名、上次压缩后的日志字节数、「需要清除」标志；
//! - `records`：每个 key 的胜出记录，及其应用状态（`applied_t` / `evicted_t` / `missing_t`）与「待上传」标记。
//!   cfg / sty / glo 保存值；clip / ocr 只在未应用期间保存值，应用后清空（正文的长期副本只在 clipboard.db）；
//!   key 域从不保存明文（用 data 子密钥加密后保存，只有引擎能解开）；
//! - `cursors`（每台远端设备的 head 签名、已读到的序号、设备信息与最近一次读到的段清单）、
//!   `own_segments`（本机日志账本）、`inflight`（进行中的写入）、`blobs`（本机上传过的 blob）。
//!
//! 改动捕获由桌面端的日志写入线程经 [`Journal`] 写入：每批一个短事务，同时更新配置影子。
//! 引擎自己的读写经 [`Replica`] 完成；两者是同一个库上的两个连接（WAL 下读写互不阻塞）。
//!
//! 所有多行更新都在事务里完成；清除「待上传」标记只按 `(d, k, t)` 精确匹配，推送期间又被改动的 key
//! 有新的 t，保持待上传。

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};
use rusqlite_migration::{M, Migrations};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::SyncError;
use crate::hlc::Hlc;
use crate::model::{BlobRef, DeviceInfo, Domain, PinValue, Record, SegmentEntry, keys, outranks};
use crate::projection::ConfigDoc;

// ---------------------------------------------------------------------------------------------
// 表结构
// ---------------------------------------------------------------------------------------------

const M1_UP: &str = r"
CREATE TABLE meta (
    k TEXT PRIMARY KEY NOT NULL,
    v TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE records (
    d TEXT NOT NULL,
    k TEXT NOT NULL,
    dev TEXT NOT NULL,
    t INTEGER NOT NULL,
    tomb INTEGER NOT NULL DEFAULT 0,
    v TEXT,
    b TEXT,
    aux TEXT,
    item_ms INTEGER,
    src_hi INTEGER,
    applied_t INTEGER,
    evicted_t INTEGER,
    missing_t INTEGER,
    gone INTEGER NOT NULL DEFAULT 0,
    pending INTEGER NOT NULL DEFAULT 0,
    deferred INTEGER NOT NULL DEFAULT 0,
    prev TEXT,
    PRIMARY KEY (d, k)
) WITHOUT ROWID;
CREATE INDEX records_pending ON records(dev) WHERE pending = 1;
CREATE INDEX records_unapplied ON records(d) WHERE applied_t IS NOT t;

CREATE TABLE cursors (
    dev TEXT PRIMARY KEY NOT NULL,
    sig TEXT,
    read_hi INTEGER NOT NULL DEFAULT 0,
    name TEXT,
    os TEXT,
    app_version TEXT,
    updated_ms INTEGER,
    segments TEXT,
    present INTEGER NOT NULL DEFAULT 1
) WITHOUT ROWID;

CREATE TABLE own_segments (
    w INTEGER PRIMARY KEY,
    lo INTEGER NOT NULL,
    hi INTEGER NOT NULL,
    bytes INTEGER NOT NULL,
    hash TEXT NOT NULL
);

CREATE TABLE inflight (
    w INTEGER PRIMARY KEY,
    lo INTEGER NOT NULL,
    hi INTEGER NOT NULL,
    bytes INTEGER NOT NULL,
    hash TEXT NOT NULL,
    records TEXT NOT NULL,
    compaction INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE blobs (
    name TEXT PRIMARY KEY NOT NULL,
    uploaded_ms INTEGER
) WITHOUT ROWID;
";

static MIGRATIONS: LazyLock<Migrations<'static>> = LazyLock::new(|| Migrations::new(vec![M::up(M1_UP)]));

/// 最新的 schema 版本（= 迁移条数）
const LATEST_VERSION: i64 = 1;

// meta 的 key
pub(crate) const META_HLC: &str = "hlc";
pub(crate) const META_VAULT_ID: &str = "vault_id";
pub(crate) const META_DEVICE_ID: &str = "device_id";
/// 「1」= 连接（创建 / 加入）已完成。未完成的副本不能用于恢复，重新连接时按加入处理
pub(crate) const META_JOINED: &str = "joined";
pub(crate) const META_VAULT_SIG: &str = "vault_sig";
pub(crate) const META_OWN_SIG: &str = "own_sig";
pub(crate) const META_NEXT_W: &str = "next_w";
pub(crate) const META_NEXT_SEQ: &str = "next_seq";
pub(crate) const META_COMPACT_BYTES: &str = "compact_bytes";
pub(crate) const META_PURGE: &str = "purge";
pub(crate) const META_LAST_OK: &str = "last_ok_ms";
pub(crate) const META_DEVICE_COUNT: &str = "device_count";
/// 上次逐条评估剪贴板 / 识别历史时的清空寄存器签名；变化说明清空状态变了，需要重新评估全部条目
pub(crate) const META_CLIP_EVAL: &str = "eval/clip";
pub(crate) const META_OCR_EVAL: &str = "eval/ocr";
/// 上次检查保留策略（是否该为它压缩）时的策略签名；剪贴板 / 识别记录有任何变化时删除，
/// 签名相同说明结论不变，空闲的一轮不必再读取整个域
pub(crate) const META_RETENTION_EVAL: &str = "eval/retention";

/// 本版本认识的剪贴板 / 识别历史 key 的 SQL 条件（与 [`keys::parse_clip`] / [`keys::parse_ocr`] 认的形态一致）
static CONTENT_KEY_COND: LazyLock<String> = LazyLock::new(|| {
    format!(
        "((d = 'clip' AND (substr(k, 1, 2) IN ('c/', 'p/') OR k IN ('{}', '{}')))
          OR (d = 'ocr' AND (substr(k, 1, 2) = 'o/' OR k = '{}')))",
        keys::CLIP_CLEAR_ALL,
        keys::CLIP_CLEAR_UNPINNED,
        keys::OCR_CLEAR
    )
});

/// 某个域的加入状态（「pending」= 尚未把本机记录并入副本）
pub(crate) fn meta_join_key(d: &Domain) -> String {
    format!("join/{}", d.as_str())
}

/// 剪贴板 / 识别历史在这份 vault 里还从未加入过（本机从没向这份数据发布过该域）。
/// 这时加入时已有的清空不作用于本机独有的条目；断开后以同一 id 重连（读回了本机的旧日志）不算
pub(crate) fn meta_first_join_key(d: &Domain) -> String {
    format!("first_join/{}", d.as_str())
}

fn meta_shadow_key(doc: ConfigDoc) -> String {
    format!("shadow/{}", doc.as_str())
}

/// rusqlite_migration 的错误消息可能带 SQL 片段，只保留 SQLite 错误本身或固定描述
fn migration_error(e: rusqlite_migration::Error) -> SyncError {
    match e {
        rusqlite_migration::Error::RusqliteError { err, .. } => SyncError::Db(err),
        rusqlite_migration::Error::MigrationDefinition(_) => SyncError::Internal("replica migration definition"),
        _ => SyncError::Internal("replica schema is newer than this version"),
    }
}

/// 连接级配置与迁移：两个连接（引擎、日志写入线程）都走这里
fn prepare_connection(conn: &mut Connection) -> Result<(), SyncError> {
    // WAL：日志写入线程与同步循环各用一个连接，读写互不阻塞；写写之间由 busy_timeout 排队
    let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    // 正文与解密前的 Key 密文在应用后会被清空，删除时把页内内容一并清零
    conn.pragma_update(None, "secure_delete", "FAST")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let found: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if found > LATEST_VERSION {
        return Err(SyncError::Internal("replica schema is newer than this version"));
    }
    MIGRATIONS.to_latest(conn).map_err(migration_error)?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// 行与辅助类型（crate 内部）
// ---------------------------------------------------------------------------------------------

/// `records` 的一行：某个 key 的胜出记录与它的本机状态
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Row {
    pub d: Domain,
    pub k: String,
    /// 作者的 device_id
    pub dev: String,
    pub t: Hlc,
    pub tomb: bool,
    /// 保存的值：JSON 文本；key 域是 base64 密文；clip / ocr 已应用后为空
    pub v: Option<String>,
    pub b: Vec<BlobRef>,
    /// key 域：origin（不是秘密，应用 provider 时据此判断 Key 是否还能用）
    pub aux: Option<String>,
    /// clip `c`：last_used_at；ocr `o`：created_at（保留策略排名用）
    pub item_ms: Option<i64>,
    /// 作者日志中包含这条记录的段的 `hi`（需要重新取回正文时据此找段）
    pub src_hi: Option<u64>,
    pub applied_t: Option<Hlc>,
    pub evicted_t: Option<Hlc>,
    pub missing_t: Option<Hlc>,
    /// clip / ocr：引擎已按清空判定式让本机删除了这一条
    pub gone: bool,
    pub pending: bool,
    /// 本机记录的值在推送时经 `LocalStore::export` 读取
    pub deferred: bool,
}

const ROW_COLS: &str =
    "d, k, dev, t, tomb, v, b, aux, item_ms, src_hi, applied_t, evicted_t, missing_t, gone, pending, deferred";

fn opt_hlc(v: Option<i64>) -> Option<Hlc> {
    v.map(Hlc::from_i64)
}

fn u64_of(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn i64_of(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn read_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    let d: String = r.get(0)?;
    let b: Option<String> = r.get(6)?;
    Ok(Row {
        d: Domain::parse(&d),
        k: r.get(1)?,
        dev: r.get(2)?,
        t: Hlc::from_i64(r.get(3)?),
        tomb: r.get::<_, i64>(4)? != 0,
        v: r.get(5)?,
        b: b.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default(),
        aux: r.get(7)?,
        item_ms: r.get(8)?,
        src_hi: r.get::<_, Option<i64>>(9)?.map(u64_of),
        applied_t: opt_hlc(r.get(10)?),
        evicted_t: opt_hlc(r.get(11)?),
        missing_t: opt_hlc(r.get(12)?),
        gone: r.get::<_, i64>(13)? != 0,
        pending: r.get::<_, i64>(14)? != 0,
        deferred: r.get::<_, i64>(15)? != 0,
    })
}

impl Row {
    /// 本机已按这个版本处理过（应用、或判定为本机不存在）
    pub fn is_applied(&self) -> bool {
        self.applied_t == Some(self.t)
    }

    /// 需要处理：未应用，且这个版本没有被本机规则拒绝、也没有被标记为缺内容
    pub fn is_dirty(&self) -> bool {
        !self.is_applied() && self.evicted_t != Some(self.t) && self.missing_t != Some(self.t)
    }

    /// 解析保存的 JSON 值（key 域的密文不在这里解）
    pub fn json(&self) -> Option<Value> {
        self.v.as_deref().and_then(|s| serde_json::from_str(s).ok())
    }
}

/// 一台远端设备的读取状态
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Cursor {
    pub dev: String,
    /// head 的签名（ETag，或大小 + 修改时间）；不同说明 head 变了
    pub sig: Option<String>,
    pub read_hi: u64,
    pub info: Option<DeviceInfo>,
    pub updated_ms: Option<i64>,
    /// 最近一次读到的 head 段清单
    pub segments: Vec<SegmentEntry>,
    /// 最近一次列目录时 head 还在
    pub present: bool,
}

/// 进行中的写入（PUT 之前持久化）
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Inflight {
    pub entry: SegmentEntry,
    /// 本段携带的本机记录 `(d, k, t)`；成功后据此精确清除「待上传」
    pub records: Vec<(String, String, i64)>,
    /// 压缩产物（没有要清除的待上传记录；完成时替换整个账本）
    pub compaction: bool,
}

/// 推送成功后随账本一起写回的附加信息
#[derive(Debug, Clone, Default)]
pub(crate) struct PushedExtra {
    /// key 域：加密后的值与 origin
    pub key_values: Vec<(String, i64, String, Option<String>)>,
    /// clip / ocr：条目自身的时间（导出时才知道）
    pub item_times: Vec<(Domain, String, i64, i64)>,
}

/// 合并一批远端记录的结果
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MergeStats {
    /// 成为新胜出者的记录数
    pub won: u32,
    /// 是否取代了本机作者已发布的记录（触发清除式压缩）
    pub purge: bool,
    /// 观察到的最大 t
    pub max_t: Hlc,
}

/// 合并时把 key 域的值加密保存的函数：`(key, 值) → 存储文本`
pub(crate) type KeySealer<'a> = &'a dyn Fn(&str, &Value) -> Result<String, SyncError>;

fn meta_get(conn: &Connection, k: &str) -> Result<Option<String>, SyncError> {
    Ok(conn
        .query_row("SELECT v FROM meta WHERE k = ?1", [k], |r| r.get(0))
        .optional()?)
}

fn meta_set(conn: &Connection, k: &str, v: &str) -> Result<(), SyncError> {
    conn.execute(
        "INSERT INTO meta (k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        params![k, v],
    )?;
    Ok(())
}

fn meta_del(conn: &Connection, k: &str) -> Result<(), SyncError> {
    conn.execute("DELETE FROM meta WHERE k = ?1", [k])?;
    Ok(())
}

fn meta_u64(conn: &Connection, k: &str) -> Result<Option<u64>, SyncError> {
    Ok(meta_get(conn, k)?.and_then(|s| s.parse().ok()))
}

/// 持久化时钟：只增不减
fn bump_hlc(conn: &Connection, t: Hlc) -> Result<(), SyncError> {
    let current = meta_u64(conn, META_HLC)?.unwrap_or(0);
    if t.0 > current {
        meta_set(conn, META_HLC, &t.0.to_string())?;
    }
    Ok(())
}

fn is_clear_key(d: &Domain, k: &str) -> bool {
    match d {
        Domain::Clip => matches!(k, keys::CLIP_CLEAR_ALL | keys::CLIP_CLEAR_UNPINNED),
        Domain::Ocr => k == keys::OCR_CLEAR,
        _ => false,
    }
}

/// clip `c/` 与 ocr `o/` 的值里条目自身的时间
fn item_time(d: &Domain, k: &str, v: &Value) -> Option<i64> {
    match d {
        Domain::Clip if k.starts_with("c/") => v.get("last_used_at").and_then(Value::as_i64),
        Domain::Ocr if k.starts_with("o/") => v.get("created_at").and_then(Value::as_i64),
        _ => None,
    }
}

fn is_item_key(d: &Domain, k: &str) -> bool {
    matches!(d, Domain::Clip if k.starts_with("c/")) || matches!(d, Domain::Ocr if k.starts_with("o/"))
}

fn blobs_text(b: &[BlobRef]) -> Result<Option<String>, SyncError> {
    if b.is_empty() {
        return Ok(None);
    }
    serde_json::to_string(b)
        .map(Some)
        .map_err(|_| SyncError::Internal("blob refs serialization failed"))
}

fn json_text(v: &Value) -> Result<String, SyncError> {
    serde_json::to_string(v).map_err(|_| SyncError::Internal("value serialization failed"))
}

/// 按 LWW 合并一条远端（或本机旧日志里的）记录。返回是否成为新的胜出者，以及是否取代了本机作者已发布的记录
fn merge_one(
    tx: &Transaction<'_>,
    own: &str,
    author: &str,
    rec: &Record,
    src_hi: u64,
    seal: KeySealer<'_>,
) -> Result<(bool, bool), SyncError> {
    let d = rec.d.as_str();
    let existing: Option<(String, i64, i64)> = tx
        .query_row(
            "SELECT dev, t, pending FROM records WHERE d = ?1 AND k = ?2",
            params![d, rec.k],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    if let Some((dev, t, _)) = &existing
        && !outranks(rec.t, author, Hlc::from_i64(*t), dev)
    {
        return Ok((false, false));
    }
    // 本机作者已发布的记录被墓碑、清空或新的 Key 取代：本机日志里留着无用（可能是敏感）的内容，需要清除式压缩
    let superseded_own = existing
        .as_ref()
        .is_some_and(|(dev, _, pending)| dev == own && *pending == 0 && author != own);
    let purge = (superseded_own && (rec.is_tombstone() || rec.d == Domain::Key))
        || (author != own && is_clear_key(&rec.d, &rec.k));
    let (v, aux) = match &rec.v {
        None => (None, None),
        Some(value) if rec.d == Domain::Key => (
            Some(seal(&rec.k, value)?),
            value.get("origin").and_then(Value::as_str).map(str::to_string),
        ),
        Some(value) => (Some(json_text(value)?), None),
    };
    let item_ms = rec.v.as_ref().and_then(|v| item_time(&rec.d, &rec.k, v));
    tx.execute(
        "INSERT INTO records (d, k, dev, t, tomb, v, b, aux, item_ms, src_hi, applied_t, evicted_t, missing_t,
                              gone, pending, deferred)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, NULL, NULL, 0, 0, 0)
         ON CONFLICT(d, k) DO UPDATE SET dev = excluded.dev, t = excluded.t, tomb = excluded.tomb, v = excluded.v,
             b = excluded.b, aux = excluded.aux, item_ms = excluded.item_ms, src_hi = excluded.src_hi,
             applied_t = NULL, evicted_t = NULL, missing_t = NULL, gone = 0, pending = 0, deferred = 0, prev = NULL",
        params![
            d,
            rec.k,
            author,
            rec.t.to_i64(),
            i64::from(rec.is_tombstone()),
            v,
            blobs_text(&rec.b)?,
            aux,
            item_ms,
            i64_of(src_hi),
        ],
    )?;
    Ok((true, purge))
}

/// 写入一条本机改动（LWW：比副本里的胜出者旧的直接丢弃——远端已经赢了，应用时会写回本机）
/// 本机执行了一次清空：本机存储已经删掉了清空前的条目（「保留置顶」时保留本机当时置顶的）。
/// 把这些条目在副本里标为「本机已不存在」，并要求下一轮重新评估全部条目——之后按判定式仍应存在的
/// （例如本机当时还没应用的置顶）会被重新写回本机。不论这次清空在 LWW 中是否胜出都要做：本机已经删了
fn note_local_clear(tx: &Transaction<'_>, own: &str, entry: &JournalEntry) -> Result<(), SyncError> {
    let t = entry.t.to_i64();
    match (&entry.d, entry.k.as_str()) {
        (Domain::Clip, keys::CLIP_CLEAR_ALL) => {
            tx.execute(
                "UPDATE records SET gone = 1
                 WHERE d = 'clip' AND substr(k, 1, 2) = 'c/' AND tomb = 0 AND (t < ?1 OR (t = ?1 AND dev < ?2))",
                params![t, own],
            )?;
        }
        (Domain::Clip, keys::CLIP_CLEAR_UNPINNED) => {
            // 本机当时是否置顶只能从「已应用的置顶寄存器」得知；寄存器还没应用的按「未置顶、已被删」处理（保守）。
            // 寄存器的值与 `PinValue::parse` 同一口径：裸 `true` 或 `{"pin": true, …}` 对象
            tx.execute(
                "UPDATE records SET gone = 1
                 WHERE d = 'clip' AND substr(k, 1, 2) = 'c/' AND tomb = 0 AND (t < ?1 OR (t = ?1 AND dev < ?2))
                   AND NOT EXISTS (
                       SELECT 1 FROM records AS p
                       WHERE p.d = 'clip' AND p.k = 'p/' || substr(records.k, 3)
                         AND p.tomb = 0 AND p.applied_t IS p.t
                         AND (p.v = 'true'
                              OR CASE WHEN json_valid(p.v) THEN json_type(p.v, '$.pin') END = 'true'))",
                params![t, own],
            )?;
        }
        (Domain::Ocr, keys::OCR_CLEAR) => {
            tx.execute(
                "UPDATE records SET gone = 1
                 WHERE d = 'ocr' AND substr(k, 1, 2) = 'o/' AND tomb = 0 AND (t < ?1 OR (t = ?1 AND dev < ?2))",
                params![t, own],
            )?;
        }
        _ => return Ok(()),
    }
    let eval = if entry.d == Domain::Clip {
        META_CLIP_EVAL
    } else {
        META_OCR_EVAL
    };
    meta_del(tx, eval)
}

fn record_own(tx: &Transaction<'_>, own: &str, entry: &JournalEntry) -> Result<bool, SyncError> {
    let d = entry.d.as_str();
    if entry.d.is_content() {
        meta_del(tx, META_RETENTION_EVAL)?;
    }
    if matches!(entry.value, JournalValue::Set(_)) && is_clear_key(&entry.d, &entry.k) {
        note_local_clear(tx, own, entry)?;
    }
    let existing: Option<(String, i64)> = tx
        .query_row(
            "SELECT dev, t FROM records WHERE d = ?1 AND k = ?2",
            params![d, entry.k],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((dev, t)) = &existing
        && !outranks(entry.t, own, Hlc::from_i64(*t), dev)
    {
        return Ok(false);
    }
    // 记下被取代的、其他设备也知道的胜出记录（远端的，或本机已发布的）：这条本机记录如果最终发布不了
    // （推送时本机已没有这一条），副本要退回到它，否则会留下一条只有本机知道的胜出者
    let prev: Option<String> = tx
        .query_row(
            "SELECT CASE WHEN dev <> ?3 OR src_hi IS NOT NULL
                    THEN json_object('dev', dev, 't', t, 'tomb', tomb, 'b', b, 'item_ms', item_ms, 'src_hi', src_hi)
                    ELSE prev END
             FROM records WHERE d = ?1 AND k = ?2",
            params![d, entry.k, own],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let (tomb, v, deferred) = match &entry.value {
        // API Key 的明文不进 sync.db：推送时再从钥匙串读取
        JournalValue::Set(_) if entry.d == Domain::Key => (false, None, true),
        JournalValue::Set(value) => (false, Some(json_text(value)?), false),
        JournalValue::Delete => (true, None, false),
        JournalValue::Deferred => (false, None, true),
    };
    let item_ms = match &entry.value {
        JournalValue::Set(value) => item_time(&entry.d, &entry.k, value),
        _ => None,
    }
    .or_else(|| (!tomb && is_item_key(&entry.d, &entry.k)).then(|| i64_of(entry.t.wall_ms())));
    tx.execute(
        "INSERT INTO records (d, k, dev, t, tomb, v, b, aux, item_ms, src_hi, applied_t, evicted_t, missing_t,
                              gone, pending, deferred, prev)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, ?7, NULL, ?4, NULL, NULL, ?5, 1, ?8, ?9)
         ON CONFLICT(d, k) DO UPDATE SET dev = excluded.dev, t = excluded.t, tomb = excluded.tomb, v = excluded.v,
             b = NULL, aux = NULL, item_ms = excluded.item_ms, src_hi = NULL, applied_t = excluded.t,
             evicted_t = NULL, missing_t = NULL, gone = excluded.gone, pending = 1, deferred = excluded.deferred,
             prev = excluded.prev",
        params![
            d,
            entry.k,
            own,
            entry.t.to_i64(),
            i64::from(tomb),
            v,
            item_ms,
            i64::from(deferred),
            prev
        ],
    )?;
    // 删除、清空与 Key 的改动会让本机日志里留下应当尽快消失的内容
    if tomb || entry.d == Domain::Key || is_clear_key(&entry.d, &entry.k) {
        meta_set(tx, META_PURGE, "1")?;
    }
    // 本机写下一条条目内容（复制或再次复制）：本机这一条可能是刚新建的（之前被删过），它的置顶状态是默认的「未置顶」，
    // 不一定等于置顶寄存器。把寄存器标为未应用，下一轮按寄存器的值改本机的置顶状态
    if !tomb
        && entry.d == Domain::Clip
        && let Some(h) = entry.k.strip_prefix("c/")
    {
        tx.execute(
            "UPDATE records SET applied_t = NULL WHERE d = 'clip' AND k = ?1 AND tomb = 0",
            [format!("p/{h}")],
        )?;
    }
    // 对一条早于「清空（保留置顶）」的条目置顶 / 取消置顶：它因此重新算作存在，但清空过的设备已经删了它、
    // 原作者也可能在压缩时丢掉了正文。由这台设备以同一个 t 重新发布内容（推送时从本机读取）
    if !tomb
        && entry.d == Domain::Clip
        && let Some(h) = entry.k.strip_prefix("p/")
    {
        let content_key = format!("c/{h}");
        let content: Option<(String, i64, i64)> = tx
            .query_row(
                "SELECT dev, t, tomb FROM records WHERE d = 'clip' AND k = ?1",
                [&content_key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let clear: Option<(String, i64)> = tx
            .query_row(
                "SELECT dev, t FROM records WHERE d = 'clip' AND k = ?1 AND tomb = 0",
                [keys::CLIP_CLEAR_UNPINNED],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let (Some((c_dev, c_t, 0)), Some((cu_dev, cu_t))) = (content, clear)
            && c_dev != own
            && !outranks(Hlc::from_i64(c_t), &c_dev, Hlc::from_i64(cu_t), &cu_dev)
        {
            record_own(
                tx,
                own,
                &JournalEntry {
                    d: Domain::Clip,
                    k: content_key,
                    t: entry.t,
                    value: JournalValue::Deferred,
                },
            )?;
        }
    }
    // 删除一条置顶的剪贴板条目时一并取消置顶：之后再次复制的是一条未置顶的新条目，
    // 否则旧的置顶寄存器会让它在其他设备上显示为置顶
    if tomb
        && entry.d == Domain::Clip
        && let Some(h) = entry.k.strip_prefix("c/")
    {
        let pin_key = format!("p/{h}");
        let pin: Option<(i64, Option<String>)> = tx
            .query_row(
                "SELECT tomb, v FROM records WHERE d = 'clip' AND k = ?1",
                [&pin_key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let pinned = pin
            .and_then(|(tomb, v)| if tomb == 0 { v } else { None })
            .and_then(|v| serde_json::from_str::<Value>(&v).ok())
            .and_then(|v| PinValue::parse(&v))
            .is_some_and(|p| p.pin);
        if pinned {
            record_own(
                tx,
                own,
                &JournalEntry {
                    d: Domain::Clip,
                    k: pin_key,
                    t: entry.t,
                    value: JournalValue::Set(PinValue::default().to_value()),
                },
            )?;
        }
    }
    Ok(true)
}

fn apply_shadow(tx: &Transaction<'_>, update: ShadowUpdate) -> Result<(), SyncError> {
    match update {
        ShadowUpdate::Replace { doc, fingerprint, map } => {
            save_shadow(tx, doc, &Shadow { fingerprint, map })?;
        }
        ShadowUpdate::Patch { doc, changes } => {
            let mut shadow = load_shadow(tx, doc)?.unwrap_or(Shadow {
                fingerprint: String::new(),
                map: BTreeMap::new(),
            });
            for (k, v) in changes {
                match v {
                    Some(v) => {
                        shadow.map.insert(k, v);
                    }
                    None => {
                        shadow.map.remove(&k);
                    }
                }
            }
            save_shadow(tx, doc, &shadow)?;
        }
    }
    Ok(())
}

/// 被本机记录取代的胜出记录（`records.prev`，JSON）
#[derive(Deserialize)]
struct StoredPrev {
    dev: String,
    t: i64,
    tomb: i64,
    b: Option<String>,
    item_ms: Option<i64>,
    src_hi: Option<i64>,
}

#[derive(Serialize, Deserialize)]
struct StoredShadow {
    fingerprint: String,
    map: BTreeMap<String, Value>,
}

fn load_shadow(conn: &Connection, doc: ConfigDoc) -> Result<Option<Shadow>, SyncError> {
    let Some(text) = meta_get(conn, &meta_shadow_key(doc))? else {
        return Ok(None);
    };
    let stored: StoredShadow = serde_json::from_str(&text).map_err(|_| SyncError::Corrupt)?;
    Ok(Some(Shadow {
        fingerprint: stored.fingerprint,
        map: stored.map,
    }))
}

fn save_shadow(conn: &Connection, doc: ConfigDoc, shadow: &Shadow) -> Result<(), SyncError> {
    let stored = StoredShadow {
        fingerprint: shadow.fingerprint.clone(),
        map: shadow.map.clone(),
    };
    let text = serde_json::to_string(&stored).map_err(|_| SyncError::Internal("shadow serialization failed"))?;
    meta_set(conn, &meta_shadow_key(doc), &text)
}

/// 把副本的一行转成对外的 [`Record`]：key 域只给出 origin（`{"origin": …}`），
/// clip / ocr 已应用（值已清空）的存活记录给出 `Value::Null`
fn row_to_record(row: Row) -> Record {
    let v = if row.tomb {
        None
    } else if row.d == Domain::Key {
        Some(match row.aux {
            Some(origin) => serde_json::json!({ "origin": origin }),
            None => Value::Null,
        })
    } else {
        Some(row.json().unwrap_or(Value::Null))
    };
    Record {
        d: row.d,
        k: row.k,
        t: row.t,
        v,
        b: row.b,
    }
}

fn entries_json(entries: &[SegmentEntry]) -> Result<String, SyncError> {
    serde_json::to_string(entries).map_err(|_| SyncError::Internal("segment list serialization failed"))
}

// ---------------------------------------------------------------------------------------------
// Replica：引擎使用的连接
// ---------------------------------------------------------------------------------------------

/// 本机副本
pub struct Replica {
    conn: Connection,
}

impl std::fmt::Debug for Replica {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Replica(..)")
    }
}

impl Replica {
    /// 打开（不存在则创建）sync.db：WAL、busy_timeout，并建表 / 迁移。阻塞，调用方放进工作线程
    pub fn open(path: &Path) -> Result<Replica, SyncError> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// 内存数据库（测试用）
    pub fn open_in_memory() -> Result<Replica, SyncError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Replica, SyncError> {
        prepare_connection(&mut conn)?;
        Ok(Replica { conn })
    }

    /// 只读地查看一个已存在的 sync.db 属于哪个 vault（连接前判断能否「恢复」）；文件不存在、
    /// 连接没有完成或库无法读取时返回 `None`，不创建文件
    pub fn peek_vault_id(path: &Path) -> Result<Option<String>, SyncError> {
        if !path.exists() {
            return Ok(None);
        }
        let conn =
            Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        conn.busy_timeout(std::time::Duration::from_secs(2))?;
        // 表还不存在（创建到一半的库）按「没有」处理
        let joined = meta_get(&conn, META_JOINED).unwrap_or(None);
        if joined.as_deref() != Some("1") {
            return Ok(None);
        }
        Ok(meta_get(&conn, META_VAULT_ID).unwrap_or(None))
    }

    /// 关闭连接（WAL 检查点在这里完成）
    pub fn close(self) -> Result<(), SyncError> {
        self.conn.close().map_err(|(_, e)| SyncError::Db(e))
    }

    // ---- meta ----

    pub(crate) fn meta(&self, k: &str) -> Result<Option<String>, SyncError> {
        meta_get(&self.conn, k)
    }

    pub(crate) fn set_meta(&self, k: &str, v: &str) -> Result<(), SyncError> {
        meta_set(&self.conn, k, v)
    }

    pub(crate) fn del_meta(&self, k: &str) -> Result<(), SyncError> {
        meta_del(&self.conn, k)
    }

    pub(crate) fn meta_u64(&self, k: &str) -> Result<Option<u64>, SyncError> {
        meta_u64(&self.conn, k)
    }

    pub(crate) fn persist_hlc(&self, t: Hlc) -> Result<(), SyncError> {
        bump_hlc(&self.conn, t)
    }

    /// 时钟的恢复值：持久化的 HLC 与副本里最大的 t 取大（日志写入线程可能比 meta 先写下更大的 t）
    pub(crate) fn max_hlc(&self) -> Result<Hlc, SyncError> {
        let stored = meta_u64(&self.conn, META_HLC)?.unwrap_or(0);
        let max_t: Option<i64> = self.conn.query_row("SELECT MAX(t) FROM records", [], |r| r.get(0))?;
        Ok(Hlc(stored.max(max_t.map_or(0, u64_of))))
    }

    /// 清空副本，改为属于 `vault_id`（加入 / 创建新的 vault）；时钟与影子保留
    pub(crate) fn reset_for_vault(&mut self, vault_id: &str, device_id: &str) -> Result<(), SyncError> {
        let tx = self.conn.transaction()?;
        tx.execute_batch(
            "DELETE FROM records; DELETE FROM cursors; DELETE FROM own_segments; DELETE FROM inflight;
             DELETE FROM blobs;
             DELETE FROM meta WHERE k NOT IN ('hlc') AND k NOT LIKE 'shadow/%';",
        )?;
        meta_set(&tx, META_VAULT_ID, vault_id)?;
        meta_set(&tx, META_DEVICE_ID, device_id)?;
        meta_set(&tx, META_NEXT_W, "1")?;
        meta_set(&tx, META_NEXT_SEQ, "1")?;
        for d in [
            Domain::Cfg,
            Domain::Sty,
            Domain::Glo,
            Domain::Key,
            Domain::Clip,
            Domain::Ocr,
        ] {
            meta_set(&tx, &meta_join_key(&d), "pending")?;
        }
        for d in [Domain::Clip, Domain::Ocr] {
            meta_set(&tx, &meta_first_join_key(&d), "1")?;
        }
        tx.commit()?;
        Ok(())
    }

    /// 副本里是否既没有发布过或收到过的记录，也没有本机日志（断开后重连、数据目录丢失）。
    /// 本机尚未发布的改动不算：连接时清空副本之后、核对本机 head 之前，日志写入线程仍可能记下新的复制或改动，
    /// 它们不说明这是一份被复制的数据目录
    pub(crate) fn is_blank(&self, own: &str) -> Result<bool, SyncError> {
        let n: i64 = self.conn.query_row(
            "SELECT (SELECT COUNT(*) FROM records WHERE NOT (dev = ?1 AND pending = 1 AND src_hi IS NULL))
                  + (SELECT COUNT(*) FROM own_segments) + (SELECT COUNT(*) FROM inflight)",
            [own],
            |r| r.get(0),
        )?;
        Ok(n == 0)
    }

    // ---- 记录 ----

    pub(crate) fn row(&self, d: &Domain, k: &str) -> Result<Option<Row>, SyncError> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {ROW_COLS} FROM records WHERE d = ?1 AND k = ?2"),
                params![d.as_str(), k],
                read_row,
            )
            .optional()?)
    }

    pub(crate) fn rows(&self, d: &Domain) -> Result<Vec<Row>, SyncError> {
        let mut stmt = self
            .conn
            .prepare_cached(&format!("SELECT {ROW_COLS} FROM records WHERE d = ?1"))?;
        let rows = stmt.query_map([d.as_str()], read_row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 某个域里需要应用的行（未应用、未被拒绝、未标记缺内容）
    pub(crate) fn dirty_rows(&self, d: &Domain) -> Result<Vec<Row>, SyncError> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {ROW_COLS} FROM records WHERE d = ?1 AND applied_t IS NOT t
               AND evicted_t IS NOT t AND missing_t IS NOT t"
        ))?;
        let rows = stmt.query_map([d.as_str()], read_row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 本机作者的待上传记录
    pub(crate) fn pending_rows(&self, own: &str) -> Result<Vec<Row>, SyncError> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {ROW_COLS} FROM records WHERE pending = 1 AND dev = ?1"
        ))?;
        let rows = stmt.query_map([own], read_row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 是否还有某些域的待上传记录
    pub(crate) fn has_pending(&self, own: &str, domains: &[Domain]) -> Result<bool, SyncError> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT DISTINCT d FROM records WHERE pending = 1 AND dev = ?1")?;
        let found = stmt
            .query_map([own], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(found.iter().any(|d| domains.contains(&Domain::parse(d))))
    }

    pub(crate) fn has_any_pending(&self) -> Result<bool, SyncError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM records WHERE pending = 1", [], |r| r.get(0))?;
        Ok(n > 0)
    }

    /// 合并一台设备一批段里的记录，并在同一个事务里推进它的游标
    pub(crate) fn merge_device(
        &mut self,
        own: &str,
        author: &str,
        segments: &[(u64, Vec<Record>)],
        cursor: Option<(u64, Option<&str>)>,
        seal: KeySealer<'_>,
    ) -> Result<MergeStats, SyncError> {
        let tx = self.conn.transaction()?;
        let mut stats = MergeStats::default();
        let mut content_changed = false;
        for (src_hi, records) in segments {
            for rec in records {
                stats.max_t = stats.max_t.max(rec.t);
                let (won, purge) = merge_one(&tx, own, author, rec, *src_hi, seal)?;
                if won {
                    stats.won += 1;
                    content_changed |= rec.d.is_content();
                }
                stats.purge |= purge;
            }
        }
        if content_changed {
            meta_del(&tx, META_RETENTION_EVAL)?;
        }
        if let Some((read_hi, sig)) = cursor {
            tx.execute(
                "INSERT INTO cursors (dev, sig, read_hi) VALUES (?1, ?2, ?3)
                 ON CONFLICT(dev) DO UPDATE SET sig = excluded.sig, read_hi = excluded.read_hi, present = 1",
                params![author, sig, i64_of(read_hi)],
            )?;
        }
        if stats.purge {
            meta_set(&tx, META_PURGE, "1")?;
        }
        bump_hlc(&tx, stats.max_t)?;
        tx.commit()?;
        Ok(stats)
    }

    /// 写入一批本机记录（连接与加入时按规则选定的 t）；比副本里的胜出者旧的被丢弃，返回实际写入的条数
    pub(crate) fn record_own(&mut self, own: &str, entries: &[JournalEntry]) -> Result<u32, SyncError> {
        let tx = self.conn.transaction()?;
        let mut stored = 0;
        let mut max_t = Hlc::default();
        for entry in entries {
            max_t = max_t.max(entry.t);
            if record_own(&tx, own, entry)? {
                stored += 1;
            }
        }
        bump_hlc(&tx, max_t)?;
        tx.commit()?;
        Ok(stored)
    }

    /// 标记已应用（精确到作者与 t）。`drop_value`：clip / ocr 的正文在应用后不再保存
    pub(crate) fn mark_applied(
        &mut self,
        items: &[(Domain, String, Hlc, String)],
        drop_value: bool,
        gone: bool,
    ) -> Result<(), SyncError> {
        let tx = self.conn.transaction()?;
        {
            let sql = if drop_value {
                "UPDATE records SET applied_t = t, v = NULL, gone = ?5 WHERE d = ?1 AND k = ?2 AND t = ?3 AND dev = ?4"
            } else {
                "UPDATE records SET applied_t = t, gone = ?5 WHERE d = ?1 AND k = ?2 AND t = ?3 AND dev = ?4"
            };
            let mut stmt = tx.prepare_cached(sql)?;
            for (d, k, t, dev) in items {
                stmt.execute(params![d.as_str(), k, t.to_i64(), dev, i64::from(gone)])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// 被本机规则拒绝：记下这个版本，同一 key 出现更新的版本才重新尝试；正文不再保存
    pub(crate) fn mark_evicted(&mut self, items: &[(Domain, String, Hlc, String)]) -> Result<(), SyncError> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "UPDATE records SET evicted_t = t, v = CASE WHEN d IN ('clip', 'ocr') THEN NULL ELSE v END
                 WHERE d = ?1 AND k = ?2 AND t = ?3 AND dev = ?4",
            )?;
            for (d, k, t, dev) in items {
                stmt.execute(params![d.as_str(), k, t.to_i64(), dev])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// 缺内容（引用的 blob 不存在、正文无法取回）：等这条记录的 t 变化再试，不每轮重试
    pub(crate) fn mark_missing(&mut self, items: &[(Domain, String, Hlc, String)]) -> Result<(), SyncError> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt =
                tx.prepare_cached("UPDATE records SET missing_t = t WHERE d = ?1 AND k = ?2 AND t = ?3 AND dev = ?4")?;
            for (d, k, t, dev) in items {
                stmt.execute(params![d.as_str(), k, t.to_i64(), dev])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// 把某个域的全部记录重新标为「未应用」（拒绝与缺内容的标记一并清除）
    pub(crate) fn mark_domain_unapplied(&self, d: &Domain) -> Result<(), SyncError> {
        self.conn.execute(
            "UPDATE records SET applied_t = NULL, evicted_t = NULL, missing_t = NULL WHERE d = ?1",
            [d.as_str()],
        )?;
        Ok(())
    }

    /// 本机推送时发现本机已经没有这条记录（导出返回空）：不发布，只清除待上传标记
    pub(crate) fn clear_pending_exact(&mut self, items: &[(String, String, i64)], own: &str) -> Result<(), SyncError> {
        let tx = self.conn.transaction()?;
        if items.iter().any(|(d, _, _)| Domain::parse(d).is_content()) {
            meta_del(&tx, META_RETENTION_EVAL)?;
        }
        for (d, k, t) in items {
            let prev: Option<Option<String>> = tx
                .query_row(
                    "SELECT prev FROM records WHERE d = ?1 AND k = ?2 AND t = ?3 AND dev = ?4 AND pending = 1",
                    params![d, k, t, own],
                    |r| r.get(0),
                )
                .optional()?;
            match prev {
                // 已经被更新的改动取代：与这次无关
                None => {}
                // 退回到被它取代的胜出记录；正文不在副本里，标为未应用，需要时从作者的日志取回
                Some(Some(prev)) => {
                    let p: StoredPrev = serde_json::from_str(&prev).map_err(|_| SyncError::Corrupt)?;
                    tx.execute(
                        "UPDATE records SET dev = ?3, t = ?4, tomb = ?5, v = NULL, b = ?6, aux = NULL, item_ms = ?7,
                             src_hi = ?8, applied_t = NULL, evicted_t = NULL, missing_t = NULL, gone = 1, pending = 0,
                             deferred = 0, prev = NULL
                         WHERE d = ?1 AND k = ?2",
                        params![d, k, p.dev, p.t, p.tomb, p.b, p.item_ms, p.src_hi],
                    )?;
                }
                // 之前没有任何设备写过这个 key：这条记录就当没有发生过
                Some(None) => {
                    tx.execute("DELETE FROM records WHERE d = ?1 AND k = ?2", params![d, k])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// 克隆检测：本机换了新的 device_id。待上传记录、以及所在段已在服务器上丢失（`lost` 是这些段的 `[lo, hi]`）的
    /// 已发布记录，以新 id、新 t 重新盖章并待上传；旧 id 作为远端设备从头读取（两台机器可能用过同样的编号，
    /// 从头读才不会漏掉另一台写的段，合并是幂等的）；本机日志账本、进行中的写入与 blob 账本属于旧 id，全部清空
    pub(crate) fn rebase_identity(
        &mut self,
        old: &str,
        new: &str,
        lost: &[(u64, u64)],
        restamp: &mut dyn FnMut() -> Hlc,
    ) -> Result<(), SyncError> {
        let tx = self.conn.transaction()?;
        let candidates: Vec<(String, String, Option<i64>, i64)> = {
            let mut stmt = tx.prepare("SELECT d, k, src_hi, pending FROM records WHERE dev = ?1")?;
            stmt.query_map([old], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut max_t = Hlc::default();
        {
            // 值没有保存在副本里的记录（剪贴板正文、Key 等）推送时重新从本机读取
            let mut stmt = tx.prepare(
                "UPDATE records SET dev = ?3, t = ?4, applied_t = ?4, pending = 1, src_hi = NULL,
                     deferred = CASE WHEN v IS NULL AND tomb = 0 THEN 1 ELSE deferred END
                 WHERE d = ?1 AND k = ?2",
            )?;
            for (d, k, src_hi, pending) in &candidates {
                let in_lost = src_hi.is_some_and(|hi| {
                    let hi = u64_of(hi);
                    lost.iter().any(|(lo, top)| *lo <= hi && hi <= *top)
                });
                if *pending == 0 && !in_lost {
                    continue;
                }
                let t = restamp();
                max_t = max_t.max(t);
                stmt.execute(params![d, k, new, t.to_i64()])?;
            }
        }
        tx.execute(
            "INSERT INTO cursors (dev, sig, read_hi) VALUES (?1, NULL, 0)
             ON CONFLICT(dev) DO UPDATE SET sig = NULL, read_hi = 0, present = 1",
            [old],
        )?;
        tx.execute_batch("DELETE FROM own_segments; DELETE FROM inflight; DELETE FROM blobs;")?;
        meta_del(&tx, META_RETENTION_EVAL)?;
        meta_set(&tx, META_DEVICE_ID, new)?;
        meta_set(&tx, META_NEXT_W, "1")?;
        meta_set(&tx, META_NEXT_SEQ, "1")?;
        meta_del(&tx, META_OWN_SIG)?;
        meta_set(&tx, META_COMPACT_BYTES, "0")?;
        bump_hlc(&tx, max_t)?;
        tx.commit()?;
        Ok(())
    }

    // ---- 游标 ----

    pub(crate) fn cursors(&self) -> Result<Vec<Cursor>, SyncError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT dev, sig, read_hi, name, os, app_version, updated_ms, segments, present FROM cursors",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let name: Option<String> = r.get(3)?;
                let os: Option<String> = r.get(4)?;
                let app_version: Option<String> = r.get(5)?;
                let segments: Option<String> = r.get(7)?;
                let dev: String = r.get(0)?;
                let info = match (
                    name,
                    os.and_then(|o| serde_json::from_value(Value::String(o)).ok()),
                    app_version,
                ) {
                    (Some(name), Some(os), Some(app_version)) => Some(DeviceInfo {
                        id: dev.clone(),
                        name,
                        os,
                        app_version,
                    }),
                    _ => None,
                };
                Ok(Cursor {
                    dev,
                    sig: r.get(1)?,
                    read_hi: u64_of(r.get(2)?),
                    info,
                    updated_ms: r.get(6)?,
                    segments: segments.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default(),
                    present: r.get::<_, i64>(8)? != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub(crate) fn cursor(&self, dev: &str) -> Result<Option<Cursor>, SyncError> {
        Ok(self.cursors()?.into_iter().find(|c| c.dev == dev))
    }

    /// 记下读到的 head：设备信息、段清单与时间（不动游标）
    pub(crate) fn note_head(
        &self,
        dev: &str,
        info: &DeviceInfo,
        updated_ms: i64,
        segments: &[SegmentEntry],
    ) -> Result<(), SyncError> {
        let os = serde_json::to_value(info.os)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string));
        self.conn.execute(
            "INSERT INTO cursors (dev, read_hi, name, os, app_version, updated_ms, segments, present)
             VALUES (?1, 0, ?2, ?3, ?4, ?5, ?6, 1)
             ON CONFLICT(dev) DO UPDATE SET name = excluded.name, os = excluded.os,
                 app_version = excluded.app_version, updated_ms = excluded.updated_ms,
                 segments = excluded.segments, present = 1",
            params![
                dev,
                info.name,
                os,
                info.app_version,
                updated_ms,
                entries_json(segments)?
            ],
        )?;
        Ok(())
    }

    /// 只更新 head 签名（head 没有变化，或读取后游标不需要推进）
    pub(crate) fn set_cursor_sig(&self, dev: &str, sig: Option<&str>) -> Result<(), SyncError> {
        self.conn.execute(
            "INSERT INTO cursors (dev, sig, read_hi) VALUES (?1, ?2, 0)
             ON CONFLICT(dev) DO UPDATE SET sig = excluded.sig, present = 1",
            params![dev, sig],
        )?;
        Ok(())
    }

    pub(crate) fn reset_cursor(&self, dev: &str) -> Result<(), SyncError> {
        self.conn
            .execute("UPDATE cursors SET read_hi = 0, sig = NULL WHERE dev = ?1", [dev])?;
        Ok(())
    }

    /// 列目录时不再出现的设备标为不在场（设备列表不再显示；它的记录仍在副本里）
    pub(crate) fn set_present(&mut self, present: &[String]) -> Result<(), SyncError> {
        let tx = self.conn.transaction()?;
        tx.execute("UPDATE cursors SET present = 0", [])?;
        {
            let mut stmt = tx.prepare_cached("UPDATE cursors SET present = 1 WHERE dev = ?1")?;
            for dev in present {
                stmt.execute([dev])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ---- 本机日志账本与进行中的写入 ----

    pub(crate) fn ledger(&self) -> Result<Vec<SegmentEntry>, SyncError> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT w, lo, hi, bytes, hash FROM own_segments ORDER BY w")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(SegmentEntry {
                    w: u64_of(r.get(0)?),
                    lo: u64_of(r.get(1)?),
                    hi: u64_of(r.get(2)?),
                    bytes: u64_of(r.get(3)?),
                    hash: r.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub(crate) fn inflights(&self) -> Result<Vec<Inflight>, SyncError> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT w, lo, hi, bytes, hash, records, compaction FROM inflight ORDER BY w")?;
        let rows = stmt
            .query_map([], |r| {
                let records: String = r.get(5)?;
                Ok(Inflight {
                    entry: SegmentEntry {
                        w: u64_of(r.get(0)?),
                        lo: u64_of(r.get(1)?),
                        hi: u64_of(r.get(2)?),
                        bytes: u64_of(r.get(3)?),
                        hash: r.get(4)?,
                    },
                    records: serde_json::from_str(&records).unwrap_or_default(),
                    compaction: r.get::<_, i64>(6)? != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 分配写入编号与逻辑序号：一经分配就持久化，永不复用（失败重试使用新的编号）
    pub(crate) fn allocate(&mut self, parts: u64, seqs: u64) -> Result<(u64, u64), SyncError> {
        let tx = self.conn.transaction()?;
        let w = meta_u64(&tx, META_NEXT_W)?.unwrap_or(1).max(1);
        let seq = meta_u64(&tx, META_NEXT_SEQ)?.unwrap_or(1).max(1);
        meta_set(&tx, META_NEXT_W, &(w + parts).to_string())?;
        meta_set(&tx, META_NEXT_SEQ, &(seq + seqs).to_string())?;
        tx.commit()?;
        Ok((w, seq))
    }

    /// 让分配游标越过已存在的编号（读入本机旧日志、核对 head 之后）
    pub(crate) fn advance_allocation(&mut self, min_w: u64, min_seq: u64) -> Result<(), SyncError> {
        let tx = self.conn.transaction()?;
        let w = meta_u64(&tx, META_NEXT_W)?.unwrap_or(1);
        let seq = meta_u64(&tx, META_NEXT_SEQ)?.unwrap_or(1);
        meta_set(&tx, META_NEXT_W, &w.max(min_w).to_string())?;
        meta_set(&tx, META_NEXT_SEQ, &seq.max(min_seq).to_string())?;
        tx.commit()?;
        Ok(())
    }

    /// 在 PUT 之前持久化进行中的写入
    pub(crate) fn begin_inflight(&mut self, items: &[Inflight]) -> Result<(), SyncError> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO inflight (w, lo, hi, bytes, hash, records, compaction)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for item in items {
                let records = serde_json::to_string(&item.records)
                    .map_err(|_| SyncError::Internal("inflight serialization failed"))?;
                stmt.execute(params![
                    i64_of(item.entry.w),
                    i64_of(item.entry.lo),
                    i64_of(item.entry.hi),
                    i64_of(item.entry.bytes),
                    item.entry.hash,
                    records,
                    i64::from(item.compaction),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// 推送成功（head 已写入）：写本机日志账本、精确清除待上传、删除进行中的写入、记下附加信息
    pub(crate) fn commit_push(
        &mut self,
        own: &str,
        done: &Inflight,
        extra: &PushedExtra,
        own_sig: Option<&str>,
    ) -> Result<(), SyncError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO own_segments (w, lo, hi, bytes, hash) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                i64_of(done.entry.w),
                i64_of(done.entry.lo),
                i64_of(done.entry.hi),
                i64_of(done.entry.bytes),
                done.entry.hash
            ],
        )?;
        {
            let mut stmt = tx.prepare_cached(
                "UPDATE records SET pending = 0, src_hi = ?5, prev = NULL WHERE d = ?1 AND k = ?2 AND t = ?3 AND dev = ?4",
            )?;
            for (d, k, t) in &done.records {
                stmt.execute(params![d, k, t, own, i64_of(done.entry.hi)])?;
            }
            let mut stmt = tx.prepare_cached(
                "UPDATE records SET v = ?4, aux = ?5 WHERE d = 'key' AND k = ?1 AND t = ?2 AND dev = ?3",
            )?;
            for (k, t, sealed, origin) in &extra.key_values {
                stmt.execute(params![k, t, own, sealed, origin])?;
            }
            let mut stmt =
                tx.prepare_cached("UPDATE records SET item_ms = ?5 WHERE d = ?1 AND k = ?2 AND t = ?3 AND dev = ?4")?;
            for (d, k, t, ms) in &extra.item_times {
                stmt.execute(params![d.as_str(), k, t, own, ms])?;
            }
        }
        tx.execute("DELETE FROM inflight WHERE w = ?1", [i64_of(done.entry.w)])?;
        // 新发布的本机条目开始参与「已发布记录超出保留策略」的判断
        if done.records.iter().any(|(d, _, _)| Domain::parse(d).is_content()) {
            meta_del(&tx, META_RETENTION_EVAL)?;
        }
        match own_sig {
            Some(sig) => meta_set(&tx, META_OWN_SIG, sig)?,
            None => meta_del(&tx, META_OWN_SIG)?,
        }
        tx.commit()?;
        Ok(())
    }

    /// 首轮核对本机 head 之后：账本换成 head 列出的段；head 列出的进行中写入补做提交，其余进行中的写入丢弃
    pub(crate) fn reconcile_own_head(
        &mut self,
        own: &str,
        listed: &[SegmentEntry],
        own_sig: Option<&str>,
    ) -> Result<(), SyncError> {
        let inflights = self.inflights()?;
        let tx = self.conn.transaction()?;
        for item in &inflights {
            let listed_here = listed.iter().any(|e| e.w == item.entry.w && e.hash == item.entry.hash);
            if listed_here {
                let mut stmt = tx.prepare_cached(
                    "UPDATE records SET pending = 0, src_hi = ?5, prev = NULL WHERE d = ?1 AND k = ?2 AND t = ?3 AND dev = ?4",
                )?;
                for (d, k, t) in &item.records {
                    stmt.execute(params![d, k, t, own, i64_of(item.entry.hi)])?;
                }
            }
        }
        tx.execute_batch("DELETE FROM inflight; DELETE FROM own_segments;")?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO own_segments (w, lo, hi, bytes, hash) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for e in listed {
                stmt.execute(params![
                    i64_of(e.w),
                    i64_of(e.lo),
                    i64_of(e.hi),
                    i64_of(e.bytes),
                    e.hash
                ])?;
            }
        }
        match own_sig {
            Some(sig) => meta_set(&tx, META_OWN_SIG, sig)?,
            None => meta_del(&tx, META_OWN_SIG)?,
        }
        tx.commit()?;
        Ok(())
    }

    /// 压缩完成：账本换成新产物，清除标志归零，记下压缩后的字节数。`kept` 是留在日志里的本机记录 `(d, k, t)`，
    /// 它们的出处改为压缩产物覆盖的 `max_hi`；其余本机记录已不在日志里（出处清空，不再触发保留策略的压缩）。
    /// `republish` 是只存在于服务器上已丢失的本机段里、仍然胜出的本机记录：以同一个 t 重新标为待上传
    pub(crate) fn commit_compaction(
        &mut self,
        own: &str,
        parts: &[SegmentEntry],
        kept: &[(String, String, i64)],
        republish: &[(String, String, i64)],
        max_hi: u64,
        own_sig: Option<&str>,
    ) -> Result<(), SyncError> {
        let tx = self.conn.transaction()?;
        {
            // 值没有保存在副本里的记录（剪贴板正文、Key 等）推送时重新从本机读取
            let mut stmt = tx.prepare_cached(
                "UPDATE records SET pending = 1, deferred = CASE WHEN v IS NULL AND tomb = 0 THEN 1 ELSE deferred END
                 WHERE d = ?1 AND k = ?2 AND t = ?3 AND dev = ?4",
            )?;
            for (d, k, t) in republish {
                stmt.execute(params![d, k, t, own])?;
            }
        }
        tx.execute(
            "UPDATE records SET src_hi = NULL WHERE dev = ?1 AND src_hi IS NOT NULL",
            [own],
        )?;
        {
            let mut stmt =
                tx.prepare_cached("UPDATE records SET src_hi = ?5 WHERE d = ?1 AND k = ?2 AND t = ?3 AND dev = ?4")?;
            for (d, k, t) in kept {
                stmt.execute(params![d, k, t, own, i64_of(max_hi)])?;
            }
        }
        tx.execute_batch("DELETE FROM own_segments;")?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO own_segments (w, lo, hi, bytes, hash) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for e in parts {
                stmt.execute(params![
                    i64_of(e.w),
                    i64_of(e.lo),
                    i64_of(e.hi),
                    i64_of(e.bytes),
                    e.hash
                ])?;
                tx.execute("DELETE FROM inflight WHERE w = ?1", [i64_of(e.w)])?;
            }
        }
        let bytes: u64 = parts.iter().map(|e| e.bytes).sum();
        meta_set(&tx, META_COMPACT_BYTES, &bytes.to_string())?;
        meta_set(&tx, META_PURGE, "0")?;
        match own_sig {
            Some(sig) => meta_set(&tx, META_OWN_SIG, sig)?,
            None => meta_del(&tx, META_OWN_SIG)?,
        }
        tx.commit()?;
        Ok(())
    }

    // ---- blob 账本（只记本机上传的） ----

    pub(crate) fn blob_uploaded(&self, name: &str) -> Result<bool, SyncError> {
        let v: Option<Option<i64>> = self
            .conn
            .query_row("SELECT uploaded_ms FROM blobs WHERE name = ?1", [name], |r| r.get(0))
            .optional()?;
        Ok(matches!(v, Some(Some(_))))
    }

    /// 上传前先记下意图：上传成功但响应丢失时，压缩后的回收仍能找到并删除它
    pub(crate) fn blob_intent(&self, name: &str) -> Result<(), SyncError> {
        self.conn.execute(
            "INSERT INTO blobs (name, uploaded_ms) VALUES (?1, NULL) ON CONFLICT(name) DO NOTHING",
            [name],
        )?;
        Ok(())
    }

    pub(crate) fn blob_done(&self, name: &str, now_ms: i64) -> Result<(), SyncError> {
        self.conn.execute(
            "INSERT INTO blobs (name, uploaded_ms) VALUES (?1, ?2)
             ON CONFLICT(name) DO UPDATE SET uploaded_ms = excluded.uploaded_ms",
            params![name, now_ms],
        )?;
        Ok(())
    }

    pub(crate) fn blob_names(&self) -> Result<Vec<String>, SyncError> {
        let mut stmt = self.conn.prepare_cached("SELECT name FROM blobs")?;
        let rows = stmt.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub(crate) fn blob_forget(&self, name: &str) -> Result<(), SyncError> {
        self.conn.execute("DELETE FROM blobs WHERE name = ?1", [name])?;
        Ok(())
    }

    /// 回收前先把 blob 记为「未上传」（行留着，之后的回收还能重试删除）：DELETE 的响应丢失、或删除后
    /// 来不及忘掉它时，服务器上可能已经没有这个文件；同一内容再次被引用时就会重新上传，而不是指向已删除的文件
    pub(crate) fn blob_deleting(&self, name: &str) -> Result<(), SyncError> {
        self.conn
            .execute("UPDATE blobs SET uploaded_ms = NULL WHERE name = ?1", [name])?;
        Ok(())
    }

    /// 本机作者在 clip / ocr 域里是否还有待上传记录（有的话 blob 引用还没确定，本轮不回收）
    pub(crate) fn content_pending(&self, own: &str) -> Result<bool, SyncError> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM records WHERE pending = 1 AND dev = ?1 AND d IN ('clip', 'ocr')",
            [own],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// 本机日志里（按副本的出处）是否还有本机作者在该域的记录
    pub(crate) fn has_own_published(&self, own: &str, d: &Domain) -> Result<bool, SyncError> {
        let found: i64 = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM records WHERE d = ?1 AND dev = ?2 AND src_hi IS NOT NULL)",
            params![d.as_str(), own],
            |r| r.get(0),
        )?;
        Ok(found != 0)
    }

    /// 本机作者已发布的记录 `(d, k, t, 出处 hi)`
    pub(crate) fn own_published(&self, own: &str) -> Result<Vec<(String, String, i64, u64)>, SyncError> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT d, k, t, src_hi FROM records WHERE dev = ?1 AND src_hi IS NOT NULL")?;
        let rows = stmt
            .query_map([own], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, u64_of(r.get(3)?))))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 剪贴板 / 识别历史域里是否有需要应用的行（与 [`Self::dirty_rows`] 同一条件，只看有没有）。
    ///
    /// 只统计本版本认识的 key（[`CONTENT_KEY_COND`]）：应用时 [`Self::content_view`] 忽略不认识的 key，它们永远不会被
    /// 标为已应用。若也算进来，更新的版本一旦写下新形态的 key，本机每一轮都会判定「有改动」而把整个域重新评估一遍
    pub(crate) fn has_dirty(&self, d: &Domain) -> Result<bool, SyncError> {
        let found: i64 = self.conn.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM records WHERE d = ?1 AND applied_t IS NOT t
                     AND evicted_t IS NOT t AND missing_t IS NOT t AND {})",
                *CONTENT_KEY_COND
            ),
            [d.as_str()],
            |r| r.get(0),
        )?;
        Ok(found != 0)
    }

    /// 内容开关关闭（或本机关闭了识别历史）的域不在本机应用：不保留其他设备条目的正文。
    /// 行与出处留着，重新打开后从作者的日志里取回正文；置顶与清空寄存器的值仍要用，不动
    pub(crate) fn drop_item_bodies(&self, d: &Domain) -> Result<(), SyncError> {
        self.conn.execute(
            "UPDATE records SET v = NULL
             WHERE d = ?1 AND applied_t IS NOT t AND v IS NOT NULL AND substr(k, 1, 2) IN ('c/', 'o/')",
            [d.as_str()],
        )?;
        Ok(())
    }

    /// 剪贴板 / 识别历史的一次加入完成，在一个事务里：
    /// - `present` 中本机作者、已不在本机日志里（内容开关关闭期间被压缩移除）的记录，以原来的 t 重新标为待上传；
    /// - 写入加入得出的本机记录；
    /// - `clears` 中的清空寄存器标为已应用（首次加入时，加入前已有的清空不再按墙钟作用于本机独有的条目）；
    /// - 该域的加入标为完成，之后再加入（重新打开内容开关）不再算首次
    pub(crate) fn finish_content_join(
        &mut self,
        own: &str,
        d: &Domain,
        entries: &[JournalEntry],
        present: &[String],
        clears: &[(Domain, String, Hlc, String)],
    ) -> Result<(), SyncError> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "UPDATE records SET pending = 1,
                     deferred = CASE WHEN v IS NULL AND tomb = 0 THEN 1 ELSE deferred END
                 WHERE d = ?1 AND k = ?2 AND dev = ?3 AND pending = 0 AND src_hi IS NULL AND tomb = 0",
            )?;
            for k in present {
                stmt.execute(params![d.as_str(), k, own])?;
            }
        }
        let mut max_t = Hlc::default();
        for entry in entries {
            max_t = max_t.max(entry.t);
            record_own(&tx, own, entry)?;
        }
        {
            let mut stmt =
                tx.prepare_cached("UPDATE records SET applied_t = t WHERE d = ?1 AND k = ?2 AND t = ?3 AND dev = ?4")?;
            for (dd, k, t, dev) in clears {
                stmt.execute(params![dd.as_str(), k, t.to_i64(), dev])?;
            }
        }
        meta_set(&tx, &meta_join_key(d), "done")?;
        meta_del(&tx, &meta_first_join_key(d))?;
        bump_hlc(&tx, max_t)?;
        tx.commit()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Journal：桌面端日志写入线程使用的连接
// ---------------------------------------------------------------------------------------------

/// 一条本机改动
#[derive(Debug, Clone, PartialEq)]
pub struct JournalEntry {
    pub d: Domain,
    pub k: String,
    /// 捕获时刻分配的时间戳（[`crate::hlc::Clock::now`]）
    pub t: Hlc,
    pub value: JournalValue,
}

/// 本机改动的值
#[derive(Debug, Clone, PartialEq)]
pub enum JournalValue {
    /// 新值（配置类寄存器、置顶状态、`@clear` 等，值在捕获时已知）。key 域的值不会被保存，按 `Deferred` 处理
    Set(Value),
    /// 删除（写墓碑）
    Delete,
    /// 值在推送时再经 [`LocalStore::export`](crate::engine::LocalStore::export) 读取：
    /// 剪贴板 / 识别条目的正文不进通道，API Key 的明文不进 sync.db
    Deferred,
}

/// 配置影子的更新：影子是「上次已知的本机文档投影」，启动时与文件比对以发现应用关闭期间的手改
#[derive(Debug, Clone, PartialEq)]
pub enum ShadowUpdate {
    /// 随改动一起更新影子中的若干 key（`None` 删除该 key）
    Patch {
        doc: ConfigDoc,
        changes: Vec<(String, Option<Value>)>,
    },
    /// 整体替换影子与指纹（安装钩子时的基线、启动核对、升级 / 降级后的静默重建）
    Replace {
        doc: ConfigDoc,
        fingerprint: String,
        map: BTreeMap<String, Value>,
    },
}

/// 保存的配置影子
#[derive(Debug, Clone, PartialEq)]
pub struct Shadow {
    /// 应用版本 + 各文件 schema_version；与当前不同说明发生过升级 / 降级
    pub fingerprint: String,
    pub map: BTreeMap<String, Value>,
}

/// 改动日志的写入接口（桌面端日志写入线程持有）。克隆共享同一个连接；
/// 与同步循环使用不同的连接，写入只占用很短的事务
#[derive(Clone)]
pub struct Journal {
    conn: Arc<Mutex<Connection>>,
}

impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Journal(..)")
    }
}

impl Journal {
    /// 打开 sync.db 上的第二个连接（库通常已由 [`Replica::open`] 创建；迁移可重复执行）
    pub fn open(path: &Path) -> Result<Journal, SyncError> {
        let mut conn = Connection::open(path)?;
        prepare_connection(&mut conn)?;
        Ok(Journal {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// 在一个事务里写入一批本机改动（标记为待上传，同一 key 的旧待上传值被取代）与影子更新，
    /// 并持久化其中最大的 t（重启后时钟不倒退）。
    ///
    /// LWW：比副本里的胜出者旧的改动被丢弃——那个 key 的远端值已经赢了，应用时会写回本机。
    /// 剪贴板 `c/<hash>`、识别记录 `o/<id>` 与 API Key 应记为 [`JournalValue::Deferred`]
    pub fn record(&self, entries: Vec<JournalEntry>, shadows: Vec<ShadowUpdate>) -> Result<(), SyncError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let own = meta_get(&tx, META_DEVICE_ID)?.ok_or(SyncError::Internal("device id unknown"))?;
        let mut max_t = Hlc::default();
        for entry in &entries {
            max_t = max_t.max(entry.t);
            record_own(&tx, &own, entry)?;
        }
        for update in shadows {
            apply_shadow(&tx, update)?;
        }
        bump_hlc(&tx, max_t)?;
        tx.commit()?;
        Ok(())
    }

    /// 读取某个文档的影子；从未保存过返回 `None`
    pub fn shadow(&self, doc: ConfigDoc) -> Result<Option<Shadow>, SyncError> {
        load_shadow(&self.conn.lock(), doc)
    }

    /// 副本里某个 key 的胜出记录（启动核对时判断「本机值 ≠ 影子 且 ≠ 胜出值」）。
    /// key 域的值只给出 `{"origin": …}`（不含 Key）；clip / ocr 已应用（正文已清空）的存活记录值为 `null`
    pub fn winner(&self, d: &Domain, key: &str) -> Result<Option<Record>, SyncError> {
        let conn = self.conn.lock();
        let row = conn
            .query_row(
                &format!("SELECT {ROW_COLS} FROM records WHERE d = ?1 AND k = ?2"),
                params![d.as_str(), key],
                read_row,
            )
            .optional()?;
        Ok(row.map(row_to_record))
    }

    /// 副本里某个域全部胜出记录的 key 与值（启动核对用；取值规则同 [`Self::winner`]）
    pub fn winners(&self, d: &Domain) -> Result<Vec<Record>, SyncError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare_cached(&format!("SELECT {ROW_COLS} FROM records WHERE d = ?1 ORDER BY k"))?;
        let rows = stmt.query_map([d.as_str()], read_row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().map(row_to_record).collect())
    }

    /// provider 的胜出 Key 记录设置时的 origin（`scheme://host:port`）；没有记录、是墓碑或未知时为 `None`。
    /// 应用远端 provider 时据此判断本机的 Key 是否仍能发往新的地址
    pub fn key_origin(&self, provider_id: &str) -> Result<Option<String>, SyncError> {
        let conn = self.conn.lock();
        let v: Option<(i64, Option<String>)> = conn
            .query_row(
                "SELECT tomb, aux FROM records WHERE d = 'key' AND k = ?1",
                [provider_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(v.and_then(|(tomb, aux)| if tomb == 0 { aux } else { None }))
    }
}

/// 剪贴板 / 识别历史按清空判定式需要的寄存器视图（引擎与压缩共用）
#[derive(Debug, Clone, Default)]
pub(crate) struct ContentView {
    /// `c/<hash>` 或 `o/<id>` → 行
    pub items: HashMap<String, Row>,
    /// `p/<hash>` → 行
    pub pins: HashMap<String, Row>,
    pub clear_all: Option<Row>,
    pub clear_unpinned: Option<Row>,
}

impl Replica {
    /// 读取 clip 或 ocr 域的全部寄存器
    pub(crate) fn content_view(&self, d: &Domain) -> Result<ContentView, SyncError> {
        let mut view = ContentView::default();
        for row in self.rows(d)? {
            match (d, row.k.as_str()) {
                (Domain::Clip, keys::CLIP_CLEAR_ALL) | (Domain::Ocr, keys::OCR_CLEAR) => view.clear_all = Some(row),
                (Domain::Clip, keys::CLIP_CLEAR_UNPINNED) => view.clear_unpinned = Some(row),
                (Domain::Clip, k) if k.starts_with("p/") => {
                    view.pins.insert(k[2..].to_string(), row);
                }
                (Domain::Clip, k) if k.starts_with("c/") => {
                    view.items.insert(k[2..].to_string(), row);
                }
                (Domain::Ocr, k) if k.starts_with("o/") => {
                    view.items.insert(k[2..].to_string(), row);
                }
                _ => {}
            }
        }
        Ok(view)
    }

    /// 只读取 clip 或 ocr 域的清空寄存器（按主键，不扫描整个域）
    pub(crate) fn clear_view(&self, d: &Domain) -> Result<ContentView, SyncError> {
        let mut view = ContentView::default();
        match d {
            Domain::Clip => {
                view.clear_all = self.row(d, keys::CLIP_CLEAR_ALL)?;
                view.clear_unpinned = self.row(d, keys::CLIP_CLEAR_UNPINNED)?;
            }
            Domain::Ocr => view.clear_all = self.row(d, keys::OCR_CLEAR)?,
            _ => {}
        }
        Ok(view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OWN: &str = "aaaaaaaaaaaaaaaa";
    const PEER: &str = "bbbbbbbbbbbbbbbb";

    fn replica() -> Replica {
        let mut r = Replica::open_in_memory().unwrap();
        r.reset_for_vault("0123456789abcdef", OWN).unwrap();
        r
    }

    fn rec(d: Domain, k: &str, t: u64, v: Option<Value>) -> Record {
        Record {
            d,
            k: k.into(),
            t: Hlc(t),
            v,
            b: Vec::new(),
        }
    }

    fn no_seal(_: &str, _: &Value) -> Result<String, SyncError> {
        Ok("sealed".into())
    }

    fn entry(d: Domain, k: &str, t: u64, value: JournalValue) -> JournalEntry {
        JournalEntry {
            d,
            k: k.into(),
            t: Hlc(t),
            value,
        }
    }

    #[test]
    fn merge_is_lww_and_idempotent() {
        let mut r = replica();
        let seg = vec![(1, vec![rec(Domain::Cfg, "/a", 5, Some(json!(1)))])];
        let stats = r
            .merge_device(OWN, PEER, &seg, Some((1, Some("e1"))), &no_seal)
            .unwrap();
        assert_eq!(stats.won, 1);
        // 重复合并无效果
        let stats = r
            .merge_device(OWN, PEER, &seg, Some((1, Some("e1"))), &no_seal)
            .unwrap();
        assert_eq!(stats.won, 0);
        // 更旧的不覆盖
        let old = vec![(2, vec![rec(Domain::Cfg, "/a", 4, Some(json!(0)))])];
        r.merge_device(OWN, "cccccccccccccccc", &old, None, &no_seal).unwrap();
        let row = r.row(&Domain::Cfg, "/a").unwrap().unwrap();
        assert_eq!((row.t, row.dev.as_str(), row.json()), (Hlc(5), PEER, Some(json!(1))));
        assert!(row.is_dirty());
        // 同 t 时按 device_id 决胜
        let tie = vec![(3, vec![rec(Domain::Cfg, "/a", 5, Some(json!(2)))])];
        r.merge_device(OWN, "cccccccccccccccc", &tie, None, &no_seal).unwrap();
        assert_eq!(r.row(&Domain::Cfg, "/a").unwrap().unwrap().json(), Some(json!(2)));
        assert_eq!(r.cursor(PEER).unwrap().unwrap().read_hi, 1);
    }

    #[test]
    fn own_changes_are_pending_and_lose_to_newer_winners() {
        let mut r = replica();
        r.merge_device(
            OWN,
            PEER,
            &[(1, vec![rec(Domain::Cfg, "/a", 10, Some(json!(1)))])],
            None,
            &no_seal,
        )
        .unwrap();
        // 捕获早于远端记录的本机改动被丢弃
        assert_eq!(
            r.record_own(OWN, &[entry(Domain::Cfg, "/a", 9, JournalValue::Set(json!(0)))])
                .unwrap(),
            0
        );
        assert_eq!(
            r.record_own(OWN, &[entry(Domain::Cfg, "/a", 11, JournalValue::Set(json!(3)))])
                .unwrap(),
            1
        );
        let row = r.row(&Domain::Cfg, "/a").unwrap().unwrap();
        assert!(row.pending && row.is_applied() && row.dev == OWN);
        // Key 的明文不进库
        r.record_own(
            OWN,
            &[entry(
                Domain::Key,
                "openai",
                12,
                JournalValue::Set(json!({"key": "sk"})),
            )],
        )
        .unwrap();
        let key = r.row(&Domain::Key, "openai").unwrap().unwrap();
        assert!(key.v.is_none() && key.deferred);
        assert_eq!(r.meta(META_PURGE).unwrap().as_deref(), Some("1"));
        assert!(r.max_hlc().unwrap() >= Hlc(12));
    }

    #[test]
    fn exact_pending_clear_keeps_newer_edits() {
        let mut r = replica();
        r.record_own(OWN, &[entry(Domain::Cfg, "/a", 10, JournalValue::Set(json!(1)))])
            .unwrap();
        let (w, seq) = r.allocate(1, 1).unwrap();
        let flight = Inflight {
            entry: SegmentEntry {
                w,
                lo: seq,
                hi: seq,
                bytes: 1,
                hash: "h".into(),
            },
            records: vec![("cfg".into(), "/a".into(), 10 << 16)],
            compaction: false,
        };
        r.begin_inflight(std::slice::from_ref(&flight)).unwrap();
        // 推送期间又改了一次
        r.record_own(OWN, &[entry(Domain::Cfg, "/a", 20 << 16, JournalValue::Set(json!(2)))])
            .unwrap();
        r.commit_push(OWN, &flight, &PushedExtra::default(), Some("e")).unwrap();
        assert!(r.row(&Domain::Cfg, "/a").unwrap().unwrap().pending);
        assert_eq!(r.ledger().unwrap().len(), 1);
        assert!(r.inflights().unwrap().is_empty());
        // 编号不复用
        let (w2, seq2) = r.allocate(1, 1).unwrap();
        assert!(w2 > w && seq2 > seq);
    }

    #[test]
    fn tombstone_keeps_value_absent_and_null_value_present() {
        let mut r = replica();
        r.merge_device(
            OWN,
            PEER,
            &[(
                1,
                vec![
                    rec(Domain::Cfg, "/n", 1, Some(Value::Null)),
                    rec(Domain::Cfg, "/t", 1, None),
                ],
            )],
            None,
            &no_seal,
        )
        .unwrap();
        let journal_like: Vec<Record> = r.rows(&Domain::Cfg).unwrap().into_iter().map(row_to_record).collect();
        let n = journal_like.iter().find(|x| x.k == "/n").unwrap();
        let t = journal_like.iter().find(|x| x.k == "/t").unwrap();
        assert_eq!(n.v, Some(Value::Null));
        assert!(t.is_tombstone());
    }

    #[test]
    fn journal_and_peek_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sync.db");
        assert_eq!(Replica::peek_vault_id(&path).unwrap(), None);
        let mut r = Replica::open(&path).unwrap();
        r.reset_for_vault("0123456789abcdef", OWN).unwrap();
        // 连接未完成时不可恢复
        assert_eq!(Replica::peek_vault_id(&path).unwrap(), None);
        r.set_meta(META_JOINED, "1").unwrap();
        assert_eq!(
            Replica::peek_vault_id(&path).unwrap().as_deref(),
            Some("0123456789abcdef")
        );

        let j = Journal::open(&path).unwrap();
        j.record(
            vec![entry(
                Domain::Glo,
                "hello",
                7,
                JournalValue::Set(json!({"target": "你好"})),
            )],
            vec![ShadowUpdate::Patch {
                doc: ConfigDoc::Glossary,
                changes: vec![("hello".into(), Some(json!({"target": "你好"})))],
            }],
        )
        .unwrap();
        let w = j.winner(&Domain::Glo, "hello").unwrap().unwrap();
        assert_eq!(w.v, Some(json!({"target": "你好"})));
        let shadow = j.shadow(ConfigDoc::Glossary).unwrap().unwrap();
        assert_eq!(shadow.map.len(), 1);
        j.record(
            Vec::new(),
            vec![ShadowUpdate::Replace {
                doc: ConfigDoc::Glossary,
                fingerprint: "v2".into(),
                map: BTreeMap::new(),
            }],
        )
        .unwrap();
        assert_eq!(j.shadow(ConfigDoc::Glossary).unwrap().unwrap().fingerprint, "v2");
        assert!(r.row(&Domain::Glo, "hello").unwrap().unwrap().pending);
        r.close().unwrap();
    }

    #[test]
    fn key_origin_is_kept_without_the_secret() {
        let mut r = replica();
        r.merge_device(
            OWN,
            PEER,
            &[(
                1,
                vec![rec(
                    Domain::Key,
                    "openai",
                    3,
                    Some(json!({"key": "sk-secret", "origin": "https://api.openai.com:443"})),
                )],
            )],
            None,
            &no_seal,
        )
        .unwrap();
        let row = r.row(&Domain::Key, "openai").unwrap().unwrap();
        assert_eq!(row.v.as_deref(), Some("sealed"));
        assert_eq!(row.aux.as_deref(), Some("https://api.openai.com:443"));
        let record = row_to_record(row);
        assert!(!record.v.unwrap().to_string().contains("sk-secret"));
    }

    #[test]
    fn unpublished_own_records_revert_to_the_previous_winner() {
        let mut r = replica();
        r.merge_device(
            OWN,
            PEER,
            &[(7, vec![rec(Domain::Clip, "c/x", 5, Some(json!({"last_used_at": 1})))])],
            None,
            &no_seal,
        )
        .unwrap();
        r.record_own(OWN, &[entry(Domain::Clip, "c/x", 10, JournalValue::Deferred)])
            .unwrap();
        r.record_own(OWN, &[entry(Domain::Clip, "c/new", 11, JournalValue::Deferred)])
            .unwrap();
        // 推送时本机已经没有这两条：不发布
        r.clear_pending_exact(
            &[("clip".into(), "c/x".into(), 10), ("clip".into(), "c/new".into(), 11)],
            OWN,
        )
        .unwrap();
        let back = r.row(&Domain::Clip, "c/x").unwrap().unwrap();
        assert_eq!((back.dev.as_str(), back.t, back.src_hi), (PEER, Hlc(5), Some(7)));
        assert!(back.is_dirty() && back.gone && !back.pending);
        assert!(r.row(&Domain::Clip, "c/new").unwrap().is_none());
    }

    #[test]
    fn dirty_check_and_clear_view_read_only_what_they_need() {
        let mut r = replica();
        assert!(!r.has_dirty(&Domain::Clip).unwrap());
        r.merge_device(
            OWN,
            PEER,
            &[(
                1,
                vec![
                    rec(Domain::Clip, "c/x", 5, Some(json!({"last_used_at": 1}))),
                    rec(Domain::Clip, keys::CLIP_CLEAR_ALL, 6, Some(json!({"wall_ms": 1}))),
                ],
            )],
            None,
            &no_seal,
        )
        .unwrap();
        assert!(r.has_dirty(&Domain::Clip).unwrap());
        let view = r.clear_view(&Domain::Clip).unwrap();
        assert_eq!(view.clear_all.map(|c| c.t), Some(Hlc(6)));
        assert!(view.clear_unpinned.is_none() && view.items.is_empty());
        let marks: Vec<_> = r
            .rows(&Domain::Clip)
            .unwrap()
            .iter()
            .map(|row| (row.d.clone(), row.k.clone(), row.t, row.dev.clone()))
            .collect();
        r.mark_applied(&marks, true, false).unwrap();
        assert!(!r.has_dirty(&Domain::Clip).unwrap());
        // 更新的版本写下的、本版本不认识的 key：应用时被忽略、永远不会标为已应用，也就不能算作「有改动」，
        // 否则每一轮都要把整个域重新评估一遍
        r.merge_device(
            OWN,
            PEER,
            &[(
                2,
                vec![
                    rec(Domain::Clip, "n/x", 7, Some(json!("name"))),
                    rec(Domain::Ocr, "c/x", 8, Some(json!({}))),
                ],
            )],
            None,
            &no_seal,
        )
        .unwrap();
        assert!(!r.has_dirty(&Domain::Clip).unwrap());
        assert!(!r.has_dirty(&Domain::Ocr).unwrap());
    }

    #[test]
    fn unpublished_own_changes_do_not_make_the_replica_non_blank() {
        let mut r = replica();
        assert!(r.is_blank(OWN).unwrap());
        // 连接过程中日志写入线程记下的本机改动
        r.record_own(OWN, &[entry(Domain::Clip, "c/x", 10, JournalValue::Deferred)])
            .unwrap();
        assert!(r.is_blank(OWN).unwrap());
        // 收到的记录、本机日志都说明副本不是空的
        r.merge_device(
            OWN,
            PEER,
            &[(1, vec![rec(Domain::Cfg, "/a", 5, Some(json!(1)))])],
            None,
            &no_seal,
        )
        .unwrap();
        assert!(!r.is_blank(OWN).unwrap());
    }

    #[test]
    fn rebase_identity_restamps_pending() {
        let mut r = replica();
        r.record_own(OWN, &[entry(Domain::Cfg, "/a", 10, JournalValue::Set(json!(1)))])
            .unwrap();
        r.record_own(OWN, &[entry(Domain::Cfg, "/b", 11, JournalValue::Set(json!(1)))])
            .unwrap();
        let flight = Inflight {
            entry: SegmentEntry {
                w: 1,
                lo: 1,
                hi: 1,
                bytes: 1,
                hash: "h".into(),
            },
            records: vec![("cfg".into(), "/b".into(), 11)],
            compaction: false,
        };
        r.begin_inflight(std::slice::from_ref(&flight)).unwrap();
        r.commit_push(OWN, &flight, &PushedExtra::default(), None).unwrap();
        let mut next = 100u64;
        r.rebase_identity(OWN, PEER, &[], &mut || {
            next += 1;
            Hlc(next)
        })
        .unwrap();
        let a = r.row(&Domain::Cfg, "/a").unwrap().unwrap();
        let b = r.row(&Domain::Cfg, "/b").unwrap().unwrap();
        assert_eq!((a.dev.as_str(), a.t, a.pending), (PEER, Hlc(101), true));
        assert_eq!((b.dev.as_str(), b.t), (OWN, Hlc(11)));
        assert_eq!(r.meta(META_DEVICE_ID).unwrap().as_deref(), Some(PEER));
        assert!(r.cursor(OWN).unwrap().is_some());
    }
}
