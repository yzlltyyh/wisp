//! `ClipStore`：内部 `Mutex<rusqlite::Connection>` + blobs 根目录。
//!
//! 并发约定：所有数据库访问与 blob 文件的就位（rename）/ 删除都在同一把锁内完成，因此孤儿清理不会删掉「刚写好、行还没插入」
//! 的文件；耗时的解码 / 缩放 / 编码（图片 hash、缩略图）以及 blob 的写入与刷盘都在锁外进行，不阻塞并发的搜索与翻页。
//!
//! 删除的隐私：删除 / 清空 / 保留策略 / OCR 历史的删除都在 `secure_delete = ON` 下执行（被删内容所在的页，
//! 包括整页释放进 freelist 的页和长文本的溢出页，全部清零），之后截断 WAL——库文件与 WAL 中都不留被删的明文。
//! FTS 索引里被删内容的条目同样被抹掉：删得少时逐行（FTS5 secure-delete），删得多时整体合并或重建（见 [`FtsPurge`]），
//! 清空 1000 条约几十毫秒，而不是逐行的数秒。

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use wisp_types::{
    AppRef, ClipFilter, ClipHit, ClipItemDetail, ClipItemMeta, ClipKind, ClipPage, ClipPayload, OcrHistoryItem,
    RawClipboard,
};

use crate::blobs::{self, BlobKind, TMP_PREFIX, asset_path, hash_stem, is_fresh_tmp, stage};
use crate::error::StoreError;
use crate::imaging::{self, ICON_LONG_EDGE, ImageError, THUMB_LONG_EDGE};
use crate::ingest::{FilesRecord, IngestOutcome, IngestPolicy, Prepared, SkipReason, TextRecord, prepare};
use crate::ocr_history::{OcrHistoryNew, mode_from_str, mode_to_str};
use crate::retention::{RetentionPolicy, RetentionReport, age_cutoff, vacuum_due};
use crate::row::{
    META_COLS, ORDER_RECENT, StoredFile, files_columns, filter_sql, kind_to_str, meta_from_row, parse_files,
};
use crate::schema;
use crate::search::{LIKE_SCAN_ROWS, Query, SEARCH_PREFIX_CHARS};

/// 数据库文件名
const DB_FILE: &str = "clipboard.db";
/// 一页最多返回的条数
const MAX_PAGE: u32 = 500;
/// 损坏库备份的文件名前缀（只保留最近一份）
const CORRUPT_BACKUP_PREFIX: &str = "clipboard.db.corrupt-";
/// 恢复标记：运行中遇到数据库损坏时写入，下次打开时据此做深度检查
const RECOVERY_MARKER: &str = "clipboard.db.recover";
/// 清空后顺带 VACUUM 的剩余数据上限：置顶条目很多时不在用户操作里做长耗时的整理（留给每月的 `vacuum_if_due`）
const CLEAR_VACUUM_MAX_BYTES: i64 = 16 * 1024 * 1024;
/// PNG 文件签名
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
/// 一次检索最多为多少条结果生成「…上下文」片段（约 5 屏；其余结果保留原 preview）
const SNIPPET_MAX_ROWS: usize = 64;
/// `get_detail` 最多实时检查多少个文件的 exists（之后的按 true 给出，未检查）：一次复制上万个文件、又在网络卷上时，
/// 逐个 stat 可能阻塞数秒
const DETAIL_EXISTS_CHECKS: usize = 200;
/// 估算 FTS 清理代价时每行正文计入的字节上限（索引只覆盖前 32768 个字符，按每字符至多 3 字节计）
const PURGE_BYTES_CAP: i64 = SEARCH_PREFIX_CHARS as i64 * 3;
/// 估算逐行清理时每行计入的字节上限：长文本里重复的 trigram 多，要改写的段页数在十几 KB 之后基本不再增长
const PURGE_ROW_BYTES_CAP: i64 = 16 * 1024;
/// 保留策略要删除的条目：非置顶，且超期（按 last_used_at）或排在条数上限之外
const RETAIN_COND: &str = "pinned = 0 AND (last_used_at < ?1 OR id IN (
    SELECT id FROM items WHERE pinned = 0 ORDER BY last_used_at DESC, id DESC LIMIT -1 OFFSET ?2))";

/// 剪贴板历史存储（调用方应在 spawn_blocking 中使用）
pub struct ClipStore {
    inner: Mutex<Inner>,
    root: PathBuf,
    /// 本次运行已留下恢复标记
    recovery_marked: AtomicBool,
}

struct Inner {
    conn: Connection,
    /// 已落盘的应用图标（key，不含扩展名）
    icons: HashSet<String>,
}

/// 待插入的一行
enum NewRow<'a> {
    Text(&'a TextRecord),
    /// 文件记录、完整文件列表 JSON、前 5 项的 JSON
    Files(&'a FilesRecord, String, String),
    Image {
        key: String,
        info: imaging::PngInfo,
        bytes: u64,
    },
}

impl ClipStore {
    /// 创建 clipboard.db + blobs/{img,thumb,icons,ocr}，执行迁移。
    ///
    /// 数据库损坏（不是数据库文件、页损坏、迁移 SQL 失败）时把旧文件改名为 `clipboard.db.corrupt-<ms>` 备份并新建，
    /// 应用照常运行；FTS 索引与 items 不一致时只重建索引。库的版本比本程序新（用户降级）
    /// 不是损坏：返回 [`StoreError::SchemaTooNew`]，库文件原样保留，调用方停用历史并提示升级。
    /// 常规打开只做 O(行数) 的轻量自检；上次运行遇到过损坏（留有恢复标记）时才做 `quick_check` 等深度检查
    pub fn open(data_dir: &Path) -> Result<Self, StoreError> {
        fs::create_dir_all(data_dir)?;
        for k in BlobKind::ALL {
            fs::create_dir_all(data_dir.join("blobs").join(k.dir()))?;
        }
        let db_path = data_dir.join(DB_FILE);
        let marker = data_dir.join(RECOVERY_MARKER);
        let deep = marker.exists();
        let conn = match open_db(&db_path, deep) {
            Ok(c) => c,
            Err(e) if e.is_recoverable_by_recreate() => {
                log::error!("clipboard db unusable ({e}); backing up and recreating");
                backup_corrupt(data_dir, &db_path)?;
                open_db(&db_path, false)?
            }
            Err(e) => return Err(e),
        };
        if deep {
            blobs::remove_quietly(&marker);
        }
        let store = Self {
            inner: Mutex::new(Inner {
                conn,
                icons: HashSet::new(),
            }),
            root: data_dir.to_path_buf(),
            recovery_marked: AtomicBool::new(false),
        };
        let icons = store.scan_icons();
        store.inner.lock().icons = icons;
        Ok(store)
    }

    /// 记录管线：隐私规则 → 规范化 → hash → 去重 → 写入（缩略图由 [`Self::make_thumb`] 在后台生成）
    pub fn ingest(&self, raw: &RawClipboard, policy: &IngestPolicy, now_ms: i64) -> Result<IngestOutcome, StoreError> {
        self.guard(|| self.ingest_impl(raw, policy, now_ms))
    }

    fn ingest_impl(&self, raw: &RawClipboard, policy: &IngestPolicy, now_ms: i64) -> Result<IngestOutcome, StoreError> {
        let prepared = match prepare(raw, policy) {
            Ok(p) => p,
            Err(reason) => return Ok(IngestOutcome::Skipped(reason)),
        };
        let source = raw.source.as_ref().filter(|a| !a.id.trim().is_empty());
        match prepared {
            Prepared::Text(t) => self.upsert(&t.hash, NewRow::Text(&t), source, now_ms),
            Prepared::Files(f) => {
                let stored: Vec<StoredFile> = f.paths.iter().map(|p| StoredFile::capture(p)).collect();
                let (json, head) = files_columns(&stored).map_err(|e| StoreError::InvalidInput(e.to_string()))?;
                self.upsert(&f.hash, NewRow::Files(&f, json, head), source, now_ms)
            }
            Prepared::Image(png) => {
                // 解码与 hash 在锁外
                let info = match imaging::inspect_png(png) {
                    Ok(i) => i,
                    Err(ImageError::TooLarge(..)) => return Ok(IngestOutcome::Skipped(SkipReason::TooLarge)),
                    Err(ImageError::Empty) => return Ok(IngestOutcome::Skipped(SkipReason::Empty)),
                    Err(e) => return Err(StoreError::Image(e.to_string())),
                };
                let key = format!("{}.png", hash_stem(&info.hash));
                let hash = info.hash;
                self.upsert_image(&hash, key, info, png, source, now_ms)
            }
        }
    }

    /// 为一条图片记录生成缩略图（长边 256 px；不透明 → `thumb/<hash>.jpg` q80，带 alpha → `.png`）并写回该行。
    /// `ingest` 不等缩略图就返回（`image.thumb_url` 为 null）；由 `ClipboardService` 在后台工作线程调用本方法，
    /// 返回更新后的元数据（`thumb_url` 已填）后，调用方 `RecentCache::update` 并 emit `clipboard:changed` 的 `updated`。
    /// 记录已不存在、不是图片或已经有缩略图时返回 `Ok(None)`（幂等，可重复调用）
    pub fn make_thumb(&self, id: i64) -> Result<Option<ClipItemMeta>, StoreError> {
        self.guard(|| self.make_thumb_impl(id))
    }

    fn make_thumb_impl(&self, id: i64) -> Result<Option<ClipItemMeta>, StoreError> {
        let Some(img_key) = thumb_candidate(&self.inner.lock().conn, id)? else {
            return Ok(None);
        };
        // 读原图、解码、缩放、编码都在锁外
        let bytes = match fs::read(self.blob_path(BlobKind::Img, &img_key)) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                log::warn!("clipboard image blob missing for item {id}");
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        if !bytes.starts_with(&PNG_SIGNATURE) {
            log::warn!(
                "clipboard image blob for item {id} is not a png ({} bytes)",
                bytes.len()
            );
            return Ok(None);
        }
        let thumb =
            imaging::make_thumbnail(&bytes, THUMB_LONG_EDGE, false).map_err(|e| StoreError::Image(e.to_string()))?;
        drop(bytes);
        let stem = img_key.strip_suffix(".png").unwrap_or(&img_key);
        let thumb_key = format!("{stem}.{}", thumb.ext);
        let staged = stage(&self.blob_dir(BlobKind::Thumb), &thumb_key, &thumb.bytes)?;

        let inner = self.inner.lock();
        // 锁外期间条目可能已被删除或已有缩略图（丢弃 staged 时删除临时文件）
        if thumb_candidate(&inner.conn, id)?.as_deref() != Some(img_key.as_str()) {
            return Ok(None);
        }
        staged.commit()?;
        inner
            .conn
            .prepare_cached("UPDATE items SET thumb_blob = ?1 WHERE id = ?2")?
            .execute(params![thumb_key, id])?;
        Ok(Some(read_meta(&inner.conn, id)?))
    }

    /// 还没有缩略图的图片条目（最近使用的在前，最多 `limit` 条）：上次退出 / 崩溃时后台缩略图任务没做完、或生成时遇到
    /// 暂时性错误的条目。调用方在打开数据库后、空闲清理时对它们逐个调用 [`Self::make_thumb`]（幂等）
    pub fn missing_thumbs(&self, limit: u32) -> Result<Vec<i64>, StoreError> {
        self.guard(|| {
            let inner = self.inner.lock();
            let mut stmt = inner.conn.prepare_cached(
                "SELECT id FROM items WHERE kind = 'image' AND image_blob IS NOT NULL AND thumb_blob IS NULL \
                 ORDER BY last_used_at DESC LIMIT ?1",
            )?;
            let ids = stmt
                .query_map(params![limit], |r| r.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(ids)
        })
    }

    /// 最近条目（置顶在前）。`pinned_count` 是满足过滤器的置顶条目总数（与分页无关）。
    ///
    /// 不访问文件系统：文件条目的 `exists` 一律为 true（未检查）。源文件在不可达的网络卷 / 已拔出的磁盘上时 stat
    /// 可能阻塞数秒，因此由调用方对可视批次在后台调用 [`crate::refresh_file_exists`] 并推送 `updated`
    pub fn recent(&self, offset: u32, limit: u32, filter: ClipFilter) -> Result<ClipPage, StoreError> {
        self.guard(|| self.recent_impl(offset, limit, filter))
    }

    fn recent_impl(&self, offset: u32, limit: u32, filter: ClipFilter) -> Result<ClipPage, StoreError> {
        let limit = limit.min(MAX_PAGE);
        let (items, pinned_count, more) = {
            let inner = self.inner.lock();
            let conn = &inner.conn;
            let sql = format!(
                "SELECT {META_COLS} FROM items WHERE {} {ORDER_RECENT} LIMIT ?1 OFFSET ?2",
                filter_sql(filter)
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let mut items: Vec<ClipHit> = stmt
                .query_map(params![i64::from(limit) + 1, i64::from(offset)], |r| {
                    Ok(ClipHit {
                        item: meta_from_row(r)?,
                        highlights: Vec::new(),
                    })
                })?
                .collect::<Result<_, _>>()?;
            let more = items.len() > limit as usize;
            items.truncate(limit as usize);
            (items, pinned_count(conn, filter)?, more)
        };
        let next_offset = more.then(|| offset.saturating_add(limit));
        Ok(ClipPage {
            items,
            pinned_count,
            next_offset,
            query: None,
            filter,
        })
    }

    /// FTS5 trigram 检索；不足 3 个字符时对最近 2000 条做 LIKE。两条路径都只看每条正文的前 32768 个字符，
    /// 大小写折叠一致（见 `search.rs`）。结果平铺：置顶命中在前，其余按最近使用排序；高亮为 preview 中的
    /// UTF-16 区间。匹配不在 preview 里时，前 64 条结果的 preview 换成「…上下文」片段。
    /// 查询为空（或全是空白）时等同于 `recent(0, limit, filter)`。与 [`Self::recent`] 一样不访问文件系统。
    ///
    /// 存储锁只用于取元数据与读正文前缀（每条读完即放开，并发的记录与翻页可以插进来）；高亮与片段在锁外计算
    pub fn search(&self, query: &str, filter: ClipFilter, limit: u32) -> Result<ClipPage, StoreError> {
        self.guard(|| self.search_impl(query, filter, limit))
    }

    fn search_impl(&self, query: &str, filter: ClipFilter, limit: u32) -> Result<ClipPage, StoreError> {
        let Some(q) = Query::parse(query) else {
            let mut page = self.recent_impl(0, limit, filter)?;
            page.query = Some(query.to_owned());
            return Ok(page);
        };
        let limit = limit.min(MAX_PAGE);
        let metas = search_rows(&self.inner.lock().conn, &q, filter, limit)?;
        let mut snippets_left = SNIPPET_MAX_ROWS;
        let mut items = Vec::with_capacity(metas.len());
        for mut item in metas {
            let mut highlights = q.highlight(&item.preview);
            if highlights.is_empty() && item.kind != ClipKind::Image && snippets_left > 0 {
                snippets_left -= 1;
                // 条目可能刚被删除：读不到就保留原 preview
                if let Some((preview, hl)) = self.search_prefix(item.id)?.and_then(|t| q.snippet(&t)) {
                    item.preview = preview;
                    highlights = hl;
                }
            }
            items.push(ClipHit { item, highlights });
        }
        let pinned_count = u32::try_from(items.iter().filter(|h| h.item.pinned).count()).unwrap_or(u32::MAX);
        Ok(ClipPage {
            items,
            pinned_count,
            next_offset: None,
            query: Some(query.to_owned()),
            filter,
        })
    }

    /// 正文中参与检索的前缀（前 32768 个字符；没有正文或条目不存在时为 None）
    fn search_prefix(&self, id: i64) -> Result<Option<String>, StoreError> {
        let inner = self.inner.lock();
        let prefix = inner
            .conn
            .prepare_cached("SELECT substr(text, 1, ?2) FROM items WHERE id = ?1")?
            .query_row(params![id, SEARCH_PREFIX_CHARS], |r| r.get(0))
            .optional()?
            .flatten();
        Ok(prefix)
    }

    /// 条目详情（Space 预览）：全文、是否带 HTML / RTF、原图资源路径、完整文件列表（前 200 个实时检查 exists，
    /// 之后的按 true 给出；元数据里的前 5 个复用同一次检查的结果）
    pub fn get_detail(&self, id: i64) -> Result<Option<ClipItemDetail>, StoreError> {
        self.guard(|| self.get_detail_impl(id))
    }

    fn get_detail_impl(&self, id: i64) -> Result<Option<ClipItemDetail>, StoreError> {
        let detail = {
            let inner = self.inner.lock();
            let sql = format!(
                "SELECT {META_COLS}, text, html IS NOT NULL, rtf IS NOT NULL, image_blob, files_json FROM items WHERE id = ?1"
            );
            inner
                .conn
                .prepare_cached(&sql)?
                .query_row([id], |r| {
                    let meta = meta_from_row(r)?;
                    let text: Option<String> = r.get(19)?;
                    let image_blob: Option<String> = r.get(22)?;
                    let files = (meta.kind == ClipKind::Files).then(|| {
                        parse_files(r.get::<_, Option<String>>(23).ok().flatten().as_deref())
                            .iter()
                            .map(StoredFile::to_ref)
                            .collect::<Vec<_>>()
                    });
                    Ok(ClipItemDetail {
                        text: if matches!(meta.kind, ClipKind::Image | ClipKind::Files) {
                            None
                        } else {
                            text
                        },
                        has_html: r.get(20)?,
                        has_rtf: r.get(21)?,
                        image_url: image_blob.map(|k| asset_path(BlobKind::Img, &k)),
                        files,
                        meta,
                    })
                })
                .optional()?
        };
        Ok(detail.map(|mut d| {
            let full = d.files.as_deref_mut().unwrap_or_default();
            for f in full.iter_mut().take(DETAIL_EXISTS_CHECKS) {
                f.exists = Path::new(&f.path).exists();
            }
            let full = d.files.as_deref().unwrap_or_default();
            if let Some(head) = d.meta.files.as_mut() {
                for (i, f) in head.iter_mut().enumerate() {
                    f.exists = match full.get(i) {
                        Some(g) if g.path == f.path && i < DETAIL_EXISTS_CHECKS => g.exists,
                        _ => Path::new(&f.path).exists(),
                    };
                }
            }
            d
        }))
    }

    /// 粘贴时用；plain = true 时只给纯文本（文件条目给路径逐行拼接的文本；图片条目照常给图片）
    pub fn get_payload(&self, id: i64, plain: bool) -> Result<Option<ClipPayload>, StoreError> {
        self.guard(|| self.get_payload_impl(id, plain))
    }

    fn get_payload_impl(&self, id: i64, plain: bool) -> Result<Option<ClipPayload>, StoreError> {
        type PayloadRow = (
            String,
            Option<String>,
            Option<String>,
            Option<Vec<u8>>,
            Option<String>,
            Option<String>,
        );
        let row: Option<PayloadRow> = {
            let inner = self.inner.lock();
            inner
                .conn
                .prepare_cached("SELECT kind, text, html, rtf, image_blob, files_json FROM items WHERE id = ?1")?
                .query_row([id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
                })
                .optional()?
        };
        let Some((kind, text, html, rtf, image_blob, files_json)) = row else {
            return Ok(None);
        };
        let payload = match crate::row::kind_from_str(&kind) {
            ClipKind::Image => {
                let Some(key) = image_blob else { return Ok(None) };
                match fs::read(self.blob_path(BlobKind::Img, &key)) {
                    // 不是完整的 PNG（例如旧版本在断电时留下的空文件）：按缺失处理，不把坏数据写进剪贴板
                    Ok(png) if !png.starts_with(&PNG_SIGNATURE) => {
                        log::warn!("clipboard image blob for item {id} is not a png ({} bytes)", png.len());
                        return Ok(None);
                    }
                    Ok(png) => ClipPayload {
                        image_png: Some(png),
                        ..ClipPayload::default()
                    },
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        log::warn!("clipboard image blob missing for item {id}");
                        return Ok(None);
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            ClipKind::Files => {
                let paths: Vec<String> = parse_files(files_json.as_deref()).into_iter().map(|f| f.path).collect();
                ClipPayload {
                    text: Some(paths.join("\n")),
                    files: (!plain).then_some(paths),
                    ..ClipPayload::default()
                }
            }
            _ if plain => ClipPayload {
                text,
                ..ClipPayload::default()
            },
            _ => ClipPayload {
                text,
                html,
                rtf,
                ..ClipPayload::default()
            },
        };
        Ok(Some(payload))
    }

    /// 只更新时间和次数，不改来源和 html（粘贴时直接 touch，条目移到顶部）
    pub fn touch(&self, id: i64, now_ms: i64) -> Result<ClipItemMeta, StoreError> {
        self.guard(|| self.touch_impl(id, now_ms))
    }

    fn touch_impl(&self, id: i64, now_ms: i64) -> Result<ClipItemMeta, StoreError> {
        let inner = self.inner.lock();
        let n = inner
            .conn
            .prepare_cached("UPDATE items SET last_used_at = ?1, use_count = use_count + 1 WHERE id = ?2")?
            .execute(params![now_ms, id])?;
        if n == 0 {
            return Err(StoreError::NotFound(id));
        }
        read_meta(&inner.conn, id)
    }

    /// 置顶 / 取消置顶
    pub fn set_pinned(&self, id: i64, pinned: bool) -> Result<ClipItemMeta, StoreError> {
        self.guard(|| self.set_pinned_impl(id, pinned))
    }

    fn set_pinned_impl(&self, id: i64, pinned: bool) -> Result<ClipItemMeta, StoreError> {
        let inner = self.inner.lock();
        let n = inner
            .conn
            .prepare_cached("UPDATE items SET pinned = ?1 WHERE id = ?2")?
            .execute(params![pinned, id])?;
        if n == 0 {
            return Err(StoreError::NotFound(id));
        }
        read_meta(&inner.conn, id)
    }

    /// 删除一条（同时删除 blob）；不存在时返回 false。被删内容在库文件与 WAL 中都不留明文（见模块文档）
    pub fn delete(&self, id: i64) -> Result<bool, StoreError> {
        self.guard(|| self.delete_impl(id))
    }

    fn delete_impl(&self, id: i64) -> Result<bool, StoreError> {
        let mut inner = self.inner.lock();
        let (n, blobs) = delete_items(&mut inner.conn, "id = ?1", params![id])?;
        if n > 0 {
            checkpoint_truncate(&inner.conn);
        }
        self.remove_item_blobs(&blobs);
        Ok(n > 0)
    }

    /// 清空（同时删除 blob），返回删除条数。被删内容在库文件中清零；之后清掉不再被引用的图标，清掉了大部分内容时
    /// 顺带 VACUUM 缩小库文件，最后截断 WAL——库文件与 WAL 中都不留被删的明文
    pub fn clear(&self, keep_pinned: bool) -> Result<u32, StoreError> {
        self.guard(|| self.clear_impl(keep_pinned))
    }

    fn clear_impl(&self, keep_pinned: bool) -> Result<u32, StoreError> {
        let mut inner = self.inner.lock();
        let cond = if keep_pinned { "pinned = 0" } else { "1" };
        let (n, blobs) = delete_items(&mut inner.conn, cond, params![])?;
        self.remove_item_blobs(&blobs);
        self.sweep_locked(&mut inner)?;
        compact_after_clear(&inner.conn);
        checkpoint_truncate(&inner.conn);
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }

    /// 执行保留策略：删除超期（按 last_used_at）与超出条数上限的非置顶条目及其 blob（一个事务）
    pub fn retain(&self, policy: &RetentionPolicy, now_ms: i64) -> Result<RetentionReport, StoreError> {
        self.guard(|| self.retain_impl(policy, now_ms))
    }

    fn retain_impl(&self, policy: &RetentionPolicy, now_ms: i64) -> Result<RetentionReport, StoreError> {
        let mut inner = self.inner.lock();
        let cutoff = age_cutoff(policy, now_ms).unwrap_or(i64::MIN);
        let p = params![cutoff, i64::from(policy.max_items)];
        // 常见情形（没有可删的）只做一次索引上的计数，不进入删除流程
        let due: i64 = inner
            .conn
            .prepare_cached(&format!("SELECT count(*) FROM items WHERE {RETAIN_COND}"))?
            .query_row(p, |r| r.get(0))?;
        if due == 0 {
            return Ok(RetentionReport::default());
        }
        let (n, blobs) = delete_items(&mut inner.conn, RETAIN_COND, p)?;
        if n > 0 {
            checkpoint_truncate(&inner.conn);
        }
        let removed_blobs = self.remove_item_blobs(&blobs);
        Ok(RetentionReport {
            removed_items: u32::try_from(n).unwrap_or(u32::MAX),
            removed_blobs,
        })
    }

    /// 距上次 ≥30 天且 freelist ≥ 总页数 10% 或 ≥ 4 MB 时执行 VACUUM（之前先合并 FTS 段），返回是否执行
    pub fn vacuum_if_due(&self, now_ms: i64, last_vacuum_ms: i64) -> Result<bool, StoreError> {
        self.guard(|| self.vacuum_if_due_impl(now_ms, last_vacuum_ms))
    }

    fn vacuum_if_due_impl(&self, now_ms: i64, last_vacuum_ms: i64) -> Result<bool, StoreError> {
        let inner = self.inner.lock();
        let conn = &inner.conn;
        let pragma =
            |name: &str| -> rusqlite::Result<i64> { conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get(0)) };
        let (free, pages, size) = (pragma("freelist_count")?, pragma("page_count")?, pragma("page_size")?);
        if !vacuum_due(now_ms, last_vacuum_ms, free, pages, size) {
            return Ok(false);
        }
        log::info!("clipboard vacuum: {free}/{pages} free pages");
        conn.execute("INSERT INTO items_fts(items_fts) VALUES ('optimize')", [])?;
        conn.execute_batch("VACUUM")?;
        checkpoint_truncate(conn);
        Ok(true)
    }

    /// 删除不被任何行引用的 blob 文件（含崩溃遗留、超过保留期的临时文件），返回删除数
    pub fn sweep_orphan_blobs(&self) -> Result<u32, StoreError> {
        self.guard(|| self.sweep_orphan_blobs_impl())
    }

    fn sweep_orphan_blobs_impl(&self) -> Result<u32, StoreError> {
        let mut inner = self.inner.lock();
        self.sweep_locked(&mut inner)
    }

    /// blob 文件路径
    pub fn blob_path(&self, kind: BlobKind, key: &str) -> PathBuf {
        self.root.join("blobs").join(kind.dir()).join(key)
    }

    /// 保存应用图标（缩到长边 64 px 的 PNG），返回 icon_key；同时补到该应用已有、还没有图标的条目上。
    /// 资源路径为 `icon/<icon_key>.png`
    pub fn put_icon(&self, app_id: &str, png: &[u8]) -> Result<String, StoreError> {
        self.guard(|| self.put_icon_impl(app_id, png))
    }

    fn put_icon_impl(&self, app_id: &str, png: &[u8]) -> Result<String, StoreError> {
        if app_id.trim().is_empty() {
            return Err(StoreError::InvalidInput("empty app id".into()));
        }
        let icon = imaging::make_thumbnail(png, ICON_LONG_EDGE, true).map_err(|e| StoreError::Image(e.to_string()))?;
        let key = icon_key_for(app_id);
        let staged = stage(&self.blob_dir(BlobKind::Icon), &format!("{key}.png"), &icon.bytes)?;
        let mut inner = self.inner.lock();
        staged.commit()?;
        inner.icons.insert(key.clone());
        inner
            .conn
            .prepare_cached("UPDATE items SET icon_key = ?1 WHERE source_app_id = ?2 AND icon_key IS NULL")?
            .execute(params![key, app_id])?;
        Ok(key)
    }

    /// 该应用的图标是否已经落盘：已有时返回 icon_key，调用方据此决定要不要向平台层取图标再 [`Self::put_icon`]
    pub fn icon_key(&self, app_id: &str) -> Option<String> {
        let key = icon_key_for(app_id);
        self.inner.lock().icons.contains(&key).then_some(key)
    }

    /// 新增 OCR 历史
    pub fn ocr_history_add(&self, e: &OcrHistoryNew<'_>) -> Result<i64, StoreError> {
        self.guard(|| self.ocr_history_add_impl(e))
    }

    fn ocr_history_add_impl(&self, e: &OcrHistoryNew<'_>) -> Result<i64, StoreError> {
        // 调用方没给缩略图时从原图生成（解码、缩放、编码都在锁外；原图坏了不影响记录本身）
        let derived = match (e.thumb, e.image) {
            (None, Some(image)) => match imaging::thumbnail_any(image, imaging::THUMB_LONG_EDGE) {
                Ok(t) => Some(t.bytes),
                Err(err) => {
                    log::warn!("ocr history thumbnail failed: {err}");
                    None
                }
            },
            _ => None,
        };
        let thumb_bytes = e.thumb.or(derived.as_deref());
        let thumb = thumb_bytes.map(ocr_blob_name).transpose()?;
        let image = e.image.map(ocr_blob_name).transpose()?;
        let dir = self.blob_dir(BlobKind::Ocr);
        // 写入与刷盘在锁外；同内容（同名）的文件已存在时不重写
        let files = [(&thumb, thumb_bytes), (&image, e.image)];
        let mut staged = Vec::new();
        for (name, bytes) in files {
            if let (Some(name), Some(bytes)) = (name, bytes)
                && !dir.join(name).exists()
            {
                staged.push(stage(&dir, name, bytes)?);
            }
        }
        let mut inner = self.inner.lock();
        for s in staged {
            // 文件名即内容 hash：并发写入同一张图时覆盖也无妨。临时文件不见了（被外部删除）时不报错，下面在锁内补写
            if let Err(e) = s.commit()
                && e.kind() != io::ErrorKind::NotFound
            {
                return Err(e.into());
            }
        }
        // 锁外检查时还在、随后被删掉的同内容文件（OCR 历史清空、孤儿清理），或上面没能就位的：在锁内补写（少见）
        for (name, bytes) in files {
            if let (Some(name), Some(bytes)) = (name, bytes)
                && !dir.join(name).exists()
            {
                stage(&dir, name, bytes)?.commit()?;
            }
        }
        let tx = inner.conn.transaction()?;
        tx.execute(
            "INSERT INTO ocr_history(created_at, mode, model, text, thumb_blob, image_blob) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![e.created_at, mode_to_str(e.mode), e.model, e.text, thumb, image],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    /// OCR 历史列表（新的在前）
    pub fn ocr_history_list(&self, offset: u32, limit: u32) -> Result<Vec<OcrHistoryItem>, StoreError> {
        self.guard(|| self.ocr_history_list_impl(offset, limit))
    }

    fn ocr_history_list_impl(&self, offset: u32, limit: u32) -> Result<Vec<OcrHistoryItem>, StoreError> {
        let inner = self.inner.lock();
        let mut stmt = inner.conn.prepare_cached(
            "SELECT id, created_at, mode, model, text, thumb_blob, image_blob FROM ocr_history
             ORDER BY created_at DESC, id DESC LIMIT ?1 OFFSET ?2",
        )?;
        let rows = stmt
            .query_map(
                params![i64::from(limit.min(MAX_PAGE)), i64::from(offset)],
                ocr_history_row,
            )?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// 取一条 OCR 历史（新增之后推送 `ocr_history:changed` 的 `inserted` 用）；不存在时为 None
    pub fn ocr_history_get(&self, id: i64) -> Result<Option<OcrHistoryItem>, StoreError> {
        self.guard(|| {
            let inner = self.inner.lock();
            let item = inner
                .conn
                .prepare_cached(
                    "SELECT id, created_at, mode, model, text, thumb_blob, image_blob FROM ocr_history WHERE id = ?1",
                )?
                .query_row([id], ocr_history_row)
                .optional()?;
            Ok(item)
        })
    }

    /// 删除一条 OCR 历史：不再被其他记录引用的图片一并删除；被删内容不留明文（见模块文档）。返回是否真的删掉了
    /// 一条（id 不存在时 false，调用方据此决定要不要推送变化）
    pub fn ocr_history_delete(&self, id: i64) -> Result<bool, StoreError> {
        self.guard(|| self.ocr_history_delete_ids(&[id]).map(|ids| !ids.is_empty()))
    }

    /// 清空 OCR 历史：表与 `blobs/ocr/` 一起清空，被删内容不留明文
    pub fn ocr_history_clear(&self) -> Result<(), StoreError> {
        self.guard(|| self.ocr_history_clear_impl())
    }

    fn ocr_history_clear_impl(&self) -> Result<(), StoreError> {
        let mut inner = self.inner.lock();
        with_secure_delete(&mut inner.conn, |conn| {
            conn.execute("DELETE FROM ocr_history", [])?;
            Ok(())
        })?;
        remove_all_files_except_fresh_tmp(&self.blob_dir(BlobKind::Ocr));
        checkpoint_truncate(&inner.conn);
        Ok(())
    }

    /// 只保留最新的 `max` 条 OCR 历史（设置项 `ocr.history_limit`），返回删除条数
    pub fn ocr_history_retain(&self, max: u32) -> Result<u32, StoreError> {
        self.ocr_history_trim(max)
            .map(|ids| u32::try_from(ids.len()).unwrap_or(u32::MAX))
    }

    /// 同 [`Self::ocr_history_retain`]，返回被裁掉的 id（用于推送 `ocr_history:changed` 的 `removed`）
    pub fn ocr_history_trim(&self, max: u32) -> Result<Vec<i64>, StoreError> {
        self.guard(|| self.ocr_history_trim_impl(max))
    }

    fn ocr_history_trim_impl(&self, max: u32) -> Result<Vec<i64>, StoreError> {
        let ids: Vec<i64> = {
            let inner = self.inner.lock();
            let mut stmt = inner
                .conn
                .prepare("SELECT id FROM ocr_history ORDER BY created_at DESC, id DESC LIMIT -1 OFFSET ?1")?;
            stmt.query_map([i64::from(max)], |r| r.get(0))?
                .collect::<Result<_, _>>()?
        };
        self.ocr_history_delete_ids(&ids)
    }

    /// 在一个事务里删除若干条 OCR 历史，删掉不再被引用的图片；返回实际删除的 id
    fn ocr_history_delete_ids(&self, ids: &[i64]) -> Result<Vec<i64>, StoreError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut inner = self.inner.lock();
        let (removed, orphaned) = with_secure_delete(&mut inner.conn, |conn| {
            let tx = conn.transaction()?;
            let mut removed = Vec::new();
            let mut names = Vec::new();
            {
                let mut sel = tx.prepare("SELECT thumb_blob, image_blob FROM ocr_history WHERE id = ?1")?;
                let mut del = tx.prepare("DELETE FROM ocr_history WHERE id = ?1")?;
                for &id in ids {
                    let blobs: Option<(Option<String>, Option<String>)> =
                        sel.query_row([id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
                    if del.execute([id])? > 0 {
                        removed.push(id);
                    }
                    if let Some((thumb, image)) = blobs {
                        names.extend([thumb, image].into_iter().flatten());
                    }
                }
            }
            names.sort_unstable();
            names.dedup();
            let mut orphaned = Vec::new();
            {
                let mut used =
                    tx.prepare("SELECT EXISTS(SELECT 1 FROM ocr_history WHERE thumb_blob = ?1 OR image_blob = ?1)")?;
                for name in names {
                    if !used.query_row([&name], |r| r.get::<_, bool>(0))? {
                        orphaned.push(name);
                    }
                }
            }
            tx.commit()?;
            Ok((removed, orphaned))
        })?;
        if !removed.is_empty() {
            checkpoint_truncate(&inner.conn);
        }
        let dir = self.blob_dir(BlobKind::Ocr);
        for name in orphaned {
            blobs::remove_quietly(&dir.join(name));
        }
        Ok(removed)
    }

    // ---- 内部 ----

    /// 统一处理运行中的损坏：FTS 索引结构损坏 → 重建索引后重试一次；数据库文件损坏 → 留下恢复标记，
    /// 下次 [`Self::open`] 做深度检查（`quick_check` + FTS `integrity-check`），失败则备份重建
    fn guard<T>(&self, mut op: impl FnMut() -> Result<T, StoreError>) -> Result<T, StoreError> {
        let result = match op() {
            Err(e) if e.is_fts_corruption() => {
                log::warn!("clipboard fts index corrupt ({e}); rebuilding");
                // 先释放锁再重试（op 内部会重新加锁；parking_lot 的锁不可重入）
                let rebuilt = schema::rebuild_fts(&self.inner.lock().conn);
                rebuilt.and_then(|()| op())
            }
            other => other,
        };
        if let Err(e) = &result
            && e.is_db_corruption()
            && !self.recovery_marked.swap(true, Ordering::Relaxed)
        {
            log::error!("clipboard db corruption detected ({e}); will verify on next open");
            if let Err(io) = fs::write(self.root.join(RECOVERY_MARKER), b"") {
                log::warn!("clipboard recovery marker write failed: {io}");
            }
        }
        result
    }

    fn blob_dir(&self, kind: BlobKind) -> PathBuf {
        self.root.join("blobs").join(kind.dir())
    }

    /// 文本 / 文件：去重或插入
    fn upsert(
        &self,
        hash: &[u8; 32],
        row: NewRow<'_>,
        source: Option<&AppRef>,
        now_ms: i64,
    ) -> Result<IngestOutcome, StoreError> {
        let mut inner = self.inner.lock();
        let icon_key = source.map(|s| icon_key_for(&s.id)).filter(|k| inner.icons.contains(k));
        let tx = inner.conn.transaction()?;
        if let Some(meta) = touch_existing(&tx, hash, now_ms)? {
            tx.commit()?;
            return Ok(IngestOutcome::Touched(meta));
        }
        let id = insert_row(&tx, hash, &row, source, icon_key.as_deref(), now_ms)?;
        let meta = read_meta(&tx, id)?;
        tx.commit()?;
        Ok(IngestOutcome::Inserted(meta))
    }

    /// 图片：去重，或写 blob 后插入（文件就位与插入在同一把锁内，孤儿清理不会误删）
    fn upsert_image(
        &self,
        hash: &[u8; 32],
        key: String,
        info: imaging::PngInfo,
        png: &[u8],
        source: Option<&AppRef>,
        now_ms: i64,
    ) -> Result<IngestOutcome, StoreError> {
        let dir = self.blob_dir(BlobKind::Img);
        let path = dir.join(&key);
        // 写入与刷盘（几 MB + fsync）在锁外；同 hash 的文件已存在（重复复制，或尚未清理的孤儿）时不写
        let staged = if path.exists() {
            None
        } else {
            Some(stage(&dir, &key, png)?)
        };
        let mut inner = self.inner.lock();
        let icon_key = source.map(|s| icon_key_for(&s.id)).filter(|k| inner.icons.contains(k));
        let tx = inner.conn.transaction()?;
        if let Some(existing) = find_image(&tx, hash)? {
            // 自愈：原图文件丢失或残缺（断电时 rename 没落盘、被外部删除）时，用这次复制的字节补回（同一 hash 即同样的像素），
            // 否则这一条永远无法粘贴、也生成不了缩略图
            if existing.image_blob.as_deref() == Some(key.as_str()) && !blob_intact(&path, existing.image_bytes) {
                match staged {
                    Some(s) => s.commit()?,
                    None => stage(&dir, &key, png)?.commit()?,
                }
                tx.prepare_cached("UPDATE items SET image_bytes = ?1 WHERE id = ?2")?
                    .execute(params![i64::try_from(png.len()).unwrap_or(i64::MAX), existing.id])?;
                log::warn!(
                    "clipboard image blob for item {} was missing or damaged; restored",
                    existing.id
                );
            }
            // 缩略图文件丢失时清掉引用：返回的元数据没有 thumb_url，调用方照常在后台 make_thumb 重新生成
            if let Some(thumb) = existing.thumb_blob.as_deref()
                && !self.blob_path(BlobKind::Thumb, thumb).is_file()
            {
                tx.prepare_cached("UPDATE items SET thumb_blob = NULL WHERE id = ?1")?
                    .execute([existing.id])?;
            }
            let meta = touch_row(&tx, existing.id, now_ms)?;
            tx.commit()?;
            return Ok(IngestOutcome::Touched(meta));
        }
        // 像素相同的文件已在：直接复用；锁外检查时还在、随后被孤儿清理删掉的，在锁内补写（少见）
        let created = if path.exists() {
            false
        } else {
            match staged {
                Some(s) => s.commit()?,
                None => stage(&dir, &key, png)?.commit()?,
            }
            true
        };
        let row = NewRow::Image {
            key,
            info,
            bytes: png.len() as u64,
        };
        let committed = insert_row(&tx, hash, &row, source, icon_key.as_deref(), now_ms)
            .and_then(|id| read_meta(&tx, id))
            .and_then(|meta| tx.commit().map(|()| meta).map_err(StoreError::from));
        match committed {
            Ok(meta) => Ok(IngestOutcome::Inserted(meta)),
            Err(e) => {
                if created {
                    blobs::remove_quietly(&path);
                }
                Err(e)
            }
        }
    }

    /// 删除条目对应的原图与缩略图，返回实际删除的文件数
    fn remove_item_blobs(&self, blobs: &[(BlobKind, String)]) -> u32 {
        let removed = blobs
            .iter()
            .filter(|(kind, key)| blobs::remove_quietly(&self.blob_path(*kind, key)))
            .count();
        u32::try_from(removed).unwrap_or(u32::MAX)
    }

    /// 孤儿清理（调用方持锁）
    fn sweep_locked(&self, inner: &mut Inner) -> Result<u32, StoreError> {
        let mut referenced: HashSet<(BlobKind, String)> = HashSet::new();
        {
            let conn = &inner.conn;
            let mut stmt = conn.prepare("SELECT image_blob, thumb_blob, icon_key FROM items")?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                if let Some(k) = r.get::<_, Option<String>>(0)? {
                    referenced.insert((BlobKind::Img, k));
                }
                if let Some(k) = r.get::<_, Option<String>>(1)? {
                    referenced.insert((BlobKind::Thumb, k));
                }
                if let Some(k) = r.get::<_, Option<String>>(2)? {
                    referenced.insert((BlobKind::Icon, format!("{k}.png")));
                }
            }
            let mut stmt = conn.prepare("SELECT thumb_blob, image_blob FROM ocr_history")?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                for i in 0..2 {
                    if let Some(k) = r.get::<_, Option<String>>(i)? {
                        referenced.insert((BlobKind::Ocr, k));
                    }
                }
            }
        }
        let mut removed = 0u32;
        for kind in BlobKind::ALL {
            let dir = self.blob_dir(kind);
            let entries = match fs::read_dir(&dir) {
                Ok(e) => e,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    fs::create_dir_all(&dir)?;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            for entry in entries.flatten() {
                if !entry.file_type().is_ok_and(|t| t.is_file()) {
                    continue;
                }
                let Ok(name) = entry.file_name().into_string() else {
                    // 不是 Wisp 写的（Wisp 的文件名都是 ASCII）
                    if blobs::remove_quietly(&entry.path()) {
                        removed += 1;
                    }
                    continue;
                };
                let keep = if name.starts_with(TMP_PREFIX) {
                    // 正在锁外写入的临时文件；崩溃遗留的过了保留期再删
                    entry.metadata().is_ok_and(|m| is_fresh_tmp(&m))
                } else {
                    referenced.contains(&(kind, name.clone()))
                };
                if !keep && blobs::remove_quietly(&entry.path()) {
                    removed += 1;
                    if kind == BlobKind::Icon
                        && let Some(stem) = name.strip_suffix(".png")
                    {
                        inner.icons.remove(stem);
                    }
                }
            }
        }
        if removed > 0 {
            log::info!("clipboard orphan sweep removed {removed} blob(s)");
        }
        Ok(removed)
    }

    /// 启动时登记已有的应用图标
    fn scan_icons(&self) -> HashSet<String> {
        fs::read_dir(self.blob_dir(BlobKind::Icon))
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|n| !n.starts_with(TMP_PREFIX))
                    .filter_map(|n| n.strip_suffix(".png").map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// 打开并准备数据库：配置 →（深度检查时）`quick_check` → 迁移 → FTS 自检
fn open_db(path: &Path, deep: bool) -> Result<Connection, StoreError> {
    let mut conn = Connection::open(path)?;
    schema::configure(&conn)?;
    if deep {
        let check: String = conn.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
        if check != "ok" {
            return Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
                Some(format!("quick_check: {check}")),
            )));
        }
    }
    schema::migrate(&mut conn)?;
    schema::ensure_fts_healthy(&conn, deep)?;
    Ok(conn)
}

/// 把损坏的库（连同 -wal）改名备份，删除 -shm；只保留最近一份备份
fn backup_corrupt(dir: &Path, db_path: &Path) -> Result<(), StoreError> {
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with(CORRUPT_BACKUP_PREFIX) {
                blobs::remove_quietly(&e.path());
            }
        }
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let backup = dir.join(format!("{CORRUPT_BACKUP_PREFIX}{stamp}"));
    if db_path.exists() {
        fs::rename(db_path, &backup)?;
    }
    let wal = sidecar(db_path, "-wal");
    if wal.exists() {
        fs::rename(&wal, sidecar(&backup, "-wal"))?;
    }
    blobs::remove_quietly(&sidecar(db_path, "-shm"));
    Ok(())
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// 应用 id → 图标 key（blake3 前 16 字节的十六进制）
fn icon_key_for(app_id: &str) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"wisp:icon\0");
    h.update(app_id.as_bytes());
    hash_stem(h.finalize().as_bytes())
}

/// OCR 历史图片的文件名：内容 hash + 按魔数识别的扩展名
fn ocr_blob_name(bytes: &[u8]) -> Result<String, StoreError> {
    let ext =
        imaging::sniff_ext(bytes).ok_or_else(|| StoreError::InvalidInput("ocr image must be png or jpeg".into()))?;
    Ok(format!("{}.{ext}", hash_stem(blake3::hash(bytes).as_bytes())))
}

/// `ocr_history` 的一行 → DTO（列顺序：id, created_at, mode, model, text, thumb_blob, image_blob）
fn ocr_history_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<OcrHistoryItem> {
    Ok(OcrHistoryItem {
        id: r.get(0)?,
        created_at: r.get(1)?,
        mode: mode_from_str(&r.get::<_, String>(2)?),
        model: r.get(3)?,
        text: r.get(4)?,
        thumb_url: r.get::<_, Option<String>>(5)?.map(|k| asset_path(BlobKind::Ocr, &k)),
        image_url: r.get::<_, Option<String>>(6)?.map(|k| asset_path(BlobKind::Ocr, &k)),
    })
}

/// 按 hash 找到的已有图片条目（自愈检查用）
struct ExistingImage {
    id: i64,
    image_blob: Option<String>,
    image_bytes: Option<i64>,
    thumb_blob: Option<String>,
}

fn find_image(tx: &Transaction<'_>, hash: &[u8; 32]) -> Result<Option<ExistingImage>, StoreError> {
    let row = tx
        .prepare_cached("SELECT id, image_blob, image_bytes, thumb_blob FROM items WHERE hash = ?1")?
        .query_row([&hash[..]], |r| {
            Ok(ExistingImage {
                id: r.get(0)?,
                image_blob: r.get(1)?,
                image_bytes: r.get(2)?,
                thumb_blob: r.get(3)?,
            })
        })
        .optional()?;
    Ok(row)
}

/// 原图文件完好：存在、大小与入库时一致、以 PNG 签名开头
fn blob_intact(path: &Path, expected_len: Option<i64>) -> bool {
    use std::io::Read as _;
    let Ok(mut f) = fs::File::open(path) else { return false };
    let len_ok = f
        .metadata()
        .is_ok_and(|m| expected_len.is_none_or(|n| u64::try_from(n).is_ok_and(|n| n == m.len())));
    let mut sig = [0u8; 8];
    len_ok && f.read_exact(&mut sig).is_ok() && sig == PNG_SIGNATURE
}

/// 命中已有 hash：只更新时间与次数（不改来源、html、kind），返回更新后的元数据
fn touch_existing(tx: &Transaction<'_>, hash: &[u8; 32], now_ms: i64) -> Result<Option<ClipItemMeta>, StoreError> {
    let existing: Option<i64> = tx
        .prepare_cached("SELECT id FROM items WHERE hash = ?1")?
        .query_row([&hash[..]], |r| r.get(0))
        .optional()?;
    let Some(id) = existing else { return Ok(None) };
    touch_row(tx, id, now_ms).map(Some)
}

/// 更新一行的时间与次数，返回更新后的元数据
fn touch_row(tx: &Transaction<'_>, id: i64, now_ms: i64) -> Result<ClipItemMeta, StoreError> {
    tx.prepare_cached("UPDATE items SET last_used_at = ?1, use_count = use_count + 1 WHERE id = ?2")?
        .execute(params![now_ms, id])?;
    read_meta(tx, id)
}

fn insert_row(
    tx: &Transaction<'_>,
    hash: &[u8; 32],
    row: &NewRow<'_>,
    source: Option<&AppRef>,
    icon_key: Option<&str>,
    now_ms: i64,
) -> Result<i64, StoreError> {
    let (src_id, src_name) = source.map_or((None, None), |s| (Some(s.id.as_str()), Some(s.name.as_str())));
    let mut stmt = tx.prepare_cached(
        "INSERT INTO items(kind, created_at, last_used_at, use_count, pinned, source_app_id, source_app_name, hash,
                           text, text_preview, html, rtf, image_blob, image_w, image_h, image_bytes, files_json,
                           meta_json, truncated, icon_key, files_head_json)
         VALUES (?1, ?2, ?2, 1, 0, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
    )?;
    match row {
        NewRow::Text(t) => {
            let meta = format!("{{\"char_count\":{},\"line_count\":{}}}", t.char_count, t.line_count);
            stmt.execute(params![
                kind_to_str(t.kind),
                now_ms,
                src_id,
                src_name,
                &hash[..],
                t.text,
                t.preview,
                t.html,
                t.rtf,
                None::<String>,
                None::<i64>,
                None::<i64>,
                None::<i64>,
                None::<String>,
                meta,
                t.truncated,
                icon_key,
                None::<String>,
            ])?;
        }
        NewRow::Files(f, json, head) => {
            stmt.execute(params![
                kind_to_str(ClipKind::Files),
                now_ms,
                src_id,
                src_name,
                &hash[..],
                f.text,
                f.preview,
                None::<String>,
                None::<Vec<u8>>,
                None::<String>,
                None::<i64>,
                None::<i64>,
                None::<i64>,
                json,
                None::<String>,
                f.truncated,
                icon_key,
                head,
            ])?;
        }
        NewRow::Image { key, info, bytes } => {
            stmt.execute(params![
                kind_to_str(ClipKind::Image),
                now_ms,
                src_id,
                src_name,
                &hash[..],
                None::<String>,
                "",
                None::<String>,
                None::<Vec<u8>>,
                key,
                i64::from(info.width),
                i64::from(info.height),
                i64::try_from(*bytes).unwrap_or(i64::MAX),
                None::<String>,
                None::<String>,
                false,
                icon_key,
                None::<String>,
            ])?;
        }
    }
    Ok(tx.last_insert_rowid())
}

/// 需要生成缩略图的条目的原图 key（不存在、不是图片或已有缩略图时为 None）
fn thumb_candidate(conn: &Connection, id: i64) -> Result<Option<String>, StoreError> {
    let row: Option<Option<String>> = conn
        .prepare_cached("SELECT image_blob FROM items WHERE id = ?1 AND kind = 'image' AND thumb_blob IS NULL")?
        .query_row([id], |r| r.get(0))
        .optional()?;
    Ok(row.flatten())
}

/// 按 id 读元数据（files 的 exists 未检查）
fn read_meta(conn: &Connection, id: i64) -> Result<ClipItemMeta, StoreError> {
    let sql = format!("SELECT {META_COLS} FROM items WHERE id = ?1");
    conn.prepare_cached(&sql)?
        .query_row([id], meta_from_row)
        .optional()?
        .ok_or(StoreError::NotFound(id))
}

/// 满足过滤器的置顶条目数
fn pinned_count(conn: &Connection, filter: ClipFilter) -> Result<u32, StoreError> {
    let sql = format!("SELECT count(*) FROM items WHERE pinned = 1 AND {}", filter_sql(filter));
    let n: i64 = conn.prepare_cached(&sql)?.query_row([], |r| r.get(0))?;
    Ok(u32::try_from(n).unwrap_or(u32::MAX))
}

/// 检索出元数据行（高亮在调用方计算）
fn search_rows(conn: &Connection, q: &Query, filter: ClipFilter, limit: u32) -> Result<Vec<ClipItemMeta>, StoreError> {
    let f = filter_sql(filter);
    let rows = if q.uses_fts() {
        let sql = format!(
            "SELECT {META_COLS} FROM items
             WHERE id IN (SELECT rowid FROM items_fts WHERE items_fts MATCH ?1) AND {f}
             {ORDER_RECENT} LIMIT ?2"
        );
        conn.prepare_cached(&sql)?
            .query_map(params![q.fts_phrase(), i64::from(limit)], meta_from_row)?
            .collect::<Result<Vec<_>, _>>()?
    } else {
        // like() 由 schema::register_functions 覆盖为 Unicode 大小写折叠；只看正文前缀，长文本不会拖慢短查询
        let sql = format!(
            "SELECT {META_COLS} FROM items
             WHERE id IN (SELECT id FROM items {ORDER_RECENT} LIMIT ?3)
               AND substr(text, 1, ?4) LIKE ?1 ESCAPE '\\' AND {f}
             {ORDER_RECENT} LIMIT ?2"
        );
        conn.prepare_cached(&sql)?
            .query_map(
                params![q.like_pattern(), i64::from(limit), LIKE_SCAN_ROWS, SEARCH_PREFIX_CHARS],
                meta_from_row,
            )?
            .collect::<Result<Vec<_>, _>>()?
    };
    Ok(rows)
}

/// 收集将被删除的行引用的原图与缩略图
fn collect_item_blobs<P: rusqlite::Params>(
    tx: &Transaction<'_>,
    cond: &str,
    p: P,
) -> Result<Vec<(BlobKind, String)>, StoreError> {
    let mut stmt = tx.prepare_cached(&format!("SELECT image_blob, thumb_blob FROM items WHERE {cond}"))?;
    let mut rows = stmt.query(p)?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        if let Some(k) = r.get::<_, Option<String>>(0)? {
            out.push((BlobKind::Img, k));
        }
        if let Some(k) = r.get::<_, Option<String>>(1)? {
            out.push((BlobKind::Thumb, k));
        }
    }
    Ok(out)
}

/// 批量删除时抹掉 FTS 索引里被删内容的方式。
///
/// M1 打开了 FTS5 的 secure-delete：触发器每删一行，都要把这一行的每个 trigram 从所在的段里就地删掉（改写对应的叶子页），
/// 一行要几毫秒到上百毫秒，与该行的 trigram 数和索引大小成正比——逐行清空 1000 条要 1–5 s，1 万条要几十秒，期间一直
/// 持有存储锁。删得多时改用下面两种整体做法，代价只与索引或剩余内容的大小有关。三种做法都不在库里留下被删内容的
/// 索引条目：被改写或整体释放的旧段，其页在 `secure_delete = ON` 下清零
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FtsPurge {
    /// 逐行：触发器在 FTS secure-delete 下直接从段里抹掉条目（删得少时最快，例如单条删除、日常的保留策略）
    PerRow,
    /// 合并：临时关掉 FTS secure-delete，触发器只写删除标记，随后 `optimize` 把所有段合并成一个——删除标记与被删条目
    /// 在合并中一起消失，旧段整体释放（删一部分、剩下很多时最快）
    Merge,
    /// 重建：删行期间在事务内临时去掉 `items_ad` 触发器，之后 `rebuild` 只为剩下的行建索引
    /// （删掉大部分时最快：清空、把条数上限从 1000 改为 10）
    Rebuild,
}

impl FtsPurge {
    /// 按估算的耗时选择最快的做法。系数由 100 / 1000 / 3000 / 1 万条语料实测拟合（M 系列 macOS，单位：毫秒、字节），
    /// 只用来比较三者，不必精确：
    /// - 逐行：与要改写的段页数成正比——被删正文每字节约 5 µs（每行按 [`PURGE_ROW_BYTES_CAP`] 封顶），另有一笔随索引
    ///   变大的起步开销（首批页不在缓存里：约 3 ms + 每字节索引正文 0.011 µs）；几十字节的短条目只要零点几毫秒；
    /// - 合并：为被删内容写删除标记（每字节 0.12 µs）+ 合并整个索引（每字节索引正文 0.044 µs）；
    /// - 重建：为剩下的内容重建索引（每字节 0.1 µs）+ 释放旧索引（每字节索引正文 0.03 µs）
    #[allow(clippy::cast_precision_loss)] // 只做量级比较
    fn choose(del: &TextVolume, total: &TextVolume) -> Self {
        let del_row_bytes = del.row_bytes as f64;
        let del_bytes = del.bytes as f64;
        let total_bytes = total.bytes.max(del.bytes) as f64;
        let keep_bytes = total_bytes - del_bytes;
        let warmup = (del_row_bytes / 1024.0).min(1.0) * (3.0 + total_bytes * 0.000_011);
        let per_row = del_row_bytes * 0.005 + warmup;
        let merge = del_bytes * 0.000_12 + total_bytes * 0.000_044;
        let rebuild = keep_bytes * 0.000_1 + total_bytes * 0.000_03;
        if per_row <= merge && per_row <= rebuild {
            Self::PerRow
        } else if merge <= rebuild {
            Self::Merge
        } else {
            Self::Rebuild
        }
    }
}

/// 一组条目的正文量（估算 FTS 清理代价用）
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct TextVolume {
    /// 有正文的行数
    rows: u64,
    /// 正文字节，每行按 [`PURGE_BYTES_CAP`] 封顶（≈ 索引覆盖的内容）
    bytes: u64,
    /// 正文字节，每行按 [`PURGE_ROW_BYTES_CAP`] 封顶（逐行清理的代价）
    row_bytes: u64,
}

/// 在一个事务里删除满足 `cond` 的条目（`secure_delete = ON`），按 [`FtsPurge::choose`] 抹掉 FTS 索引中的对应条目。
/// 返回（删除条数，这些条目引用的原图与缩略图）
fn delete_items(
    conn: &mut Connection,
    cond: &str,
    p: &[&dyn rusqlite::ToSql],
) -> Result<(usize, Vec<(BlobKind, String)>), StoreError> {
    delete_items_by(conn, cond, p, FtsPurge::choose)
}

/// [`delete_items`] 的实现；`pick` 根据（被删的正文量，全部正文量）选择清理方式（单测借此逐一验证三种方式）
fn delete_items_by(
    conn: &mut Connection,
    cond: &str,
    p: &[&dyn rusqlite::ToSql],
    pick: impl FnOnce(&TextVolume, &TextVolume) -> FtsPurge,
) -> Result<(usize, Vec<(BlobKind, String)>), StoreError> {
    let result = with_secure_delete(conn, |conn| {
        let tx = conn.transaction()?;
        let del = text_volume(&tx, cond, p)?;
        let purge = if del.rows == 0 {
            // 没有正文（图片）：触发器没有可抹的内容
            FtsPurge::PerRow
        } else {
            pick(&del, &text_volume(&tx, "1", params![])?)
        };
        let blobs = collect_item_blobs(&tx, cond, p)?;
        let delete = format!("DELETE FROM items WHERE {cond}");
        let n = match purge {
            FtsPurge::PerRow => tx.execute(&delete, p)?,
            FtsPurge::Merge => {
                schema::set_fts_secure_delete(&tx, false)?;
                let n = tx.execute(&delete, p)?;
                tx.execute("INSERT INTO items_fts(items_fts) VALUES ('optimize')", [])?;
                schema::set_fts_secure_delete(&tx, true)?;
                n
            }
            FtsPurge::Rebuild => {
                // DDL 在事务内：任何一步失败都随回滚恢复触发器；重建时用库里保存的原始定义，不在代码里重复一份
                let trigger: String = tx.query_row(
                    "SELECT sql FROM sqlite_schema WHERE type = 'trigger' AND name = ?1",
                    [schema::FTS_DELETE_TRIGGER],
                    |r| r.get(0),
                )?;
                tx.execute_batch(&format!("DROP TRIGGER {}", schema::FTS_DELETE_TRIGGER))?;
                let n = tx.execute(&delete, p)?;
                tx.execute_batch(&trigger)?;
                schema::rebuild_fts(&tx)?;
                n
            }
        };
        log::debug!("clipboard delete: {n} item(s), fts purge {purge:?}");
        tx.commit()?;
        Ok((n, blobs))
    });
    if result.is_err() {
        // 回滚后 FTS5 会按 cookie 重新读配置；这里再显式恢复一次 secure-delete，确保之后的单条删除不留索引条目
        if let Err(e) = schema::set_fts_secure_delete(conn, true) {
            log::warn!("clipboard fts secure-delete restore failed: {e}");
        }
    }
    result
}

/// 满足 `cond` 的条目的正文量。`octet_length` 只读记录头里的长度，不读正文本身（长文本不会被整段读进来），
/// 1 万条约 1 ms
fn text_volume(tx: &Transaction<'_>, cond: &str, p: &[&dyn rusqlite::ToSql]) -> Result<TextVolume, StoreError> {
    let (rows, bytes, row_bytes): (i64, i64, i64) = tx
        .prepare_cached(&format!(
            "SELECT count(text), coalesce(sum(min(octet_length(text), {PURGE_BYTES_CAP})), 0),
                    coalesce(sum(min(octet_length(text), {PURGE_ROW_BYTES_CAP})), 0)
             FROM items WHERE {cond}"
        ))?
        .query_row(p, |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    let n = |v: i64| u64::try_from(v).unwrap_or(0);
    Ok(TextVolume {
        rows: n(rows),
        bytes: n(bytes),
        row_bytes: n(row_bytes),
    })
}

/// 在 `secure_delete = ON` 下执行一次删除：被删内容所在的页（包括整页释放进 freelist 的页、长文本的溢出页）全部清零。
/// 平时保持 FAST（只清零 b-tree 页内的内容）：ON 会让日常写入（FTS 段合并等释放页的操作）多写一倍的页
fn with_secure_delete<T>(
    conn: &mut Connection,
    f: impl FnOnce(&mut Connection) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    conn.execute_batch("PRAGMA secure_delete = ON")?;
    let result = f(conn);
    if let Err(e) = conn.execute_batch("PRAGMA secure_delete = FAST") {
        log::warn!("clipboard secure_delete restore failed: {e}");
    }
    result
}

/// 截断 WAL：把（已清零的）新页写回库文件并把 WAL 截成 0 字节，WAL 里更早的帧（含被删内容）随之消失。
/// 失败或被读者阻塞时只记日志（下次删除 / 清空时再截断）
fn checkpoint_truncate(conn: &Connection) {
    match conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get::<_, i64>(0)) {
        Ok(0) => {}
        Ok(_) => log::warn!("clipboard wal checkpoint blocked by a reader"),
        Err(e) => log::warn!("clipboard wal checkpoint failed: {e}"),
    }
}

/// 清空后：空闲页达到总页数的 10%（与 `vacuum_if_due` 同一门槛）且剩余数据不大时，合并 FTS 段并 VACUUM，
/// 把库文件缩回实际大小（被删的页此时已清零，这一步只为回收磁盘空间；失败只记日志）
fn compact_after_clear(conn: &Connection) {
    let pragma = |name: &str| -> rusqlite::Result<i64> { conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get(0)) };
    let (Ok(free), Ok(pages), Ok(size)) = (pragma("freelist_count"), pragma("page_count"), pragma("page_size")) else {
        return;
    };
    if free == 0 || free * 10 < pages || (pages - free) * size > CLEAR_VACUUM_MAX_BYTES {
        return;
    }
    let result = conn
        .execute("INSERT INTO items_fts(items_fts) VALUES ('optimize')", [])
        .and_then(|_| conn.execute_batch("VACUUM"));
    if let Err(e) = result {
        log::warn!("clipboard vacuum after clear failed: {e}");
    }
}

/// 删除目录下的全部文件（不删子目录）。保留期内的临时文件除外：它们可能正被并发的写入在锁外写着（与孤儿清理一致），
/// 删掉会让那次写入在就位时失败；崩溃遗留的临时文件过了保留期由孤儿清理删除
fn remove_all_files_except_fresh_tmp(dir: &Path) {
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            if !e.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            let fresh_tmp =
                e.file_name().to_string_lossy().starts_with(TMP_PREFIX) && e.metadata().is_ok_and(|m| is_fresh_tmp(&m));
            if !fresh_tmp {
                blobs::remove_quietly(&e.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::privacy::PrivacyRules;

    fn policy() -> IngestPolicy {
        IngestPolicy {
            record_text: true,
            record_rich_text: true,
            record_images: true,
            record_files: true,
            max_text_bytes: 1 << 20,
            max_image_bytes: 20 << 20,
            privacy: PrivacyRules::default(),
        }
    }

    /// 120 条长短不一的中英文文本，每条带唯一标记 `zqmark{i:03}x`（id = i + 1）
    fn filled_store() -> (tempfile::TempDir, ClipStore) {
        const WORDS: [&str; 12] = [
            "clipboard",
            "history",
            "search",
            "翻译",
            "截图",
            "panel",
            "motion",
            "数据库",
            "index",
            "token",
            "合并",
            "spring",
        ];
        let dir = tempfile::tempdir().expect("临时目录");
        let store = ClipStore::open(dir.path()).expect("打开");
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        for i in 0..120u64 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let words = 4 + (x % 300) as usize;
            let body: Vec<&str> = (0..words)
                .map(|k| WORDS[(x as usize >> (k % 32)) % WORDS.len()])
                .collect();
            let raw = RawClipboard {
                text: Some(format!("zqmark{i:03}x {} {i}", body.join(" "))),
                ..RawClipboard::default()
            };
            store.ingest(&raw, &policy(), 1_000 + i as i64).expect("记录");
        }
        (dir, store)
    }

    fn fts_bytes(conn: &Connection) -> i64 {
        conn.query_row("SELECT coalesce(sum(length(block)), 0) FROM items_fts_data", [], |r| {
            r.get(0)
        })
        .expect("索引大小")
    }

    fn fts_blocks(conn: &Connection) -> i64 {
        conn.query_row("SELECT count(*) FROM items_fts_data", [], |r| r.get(0))
            .expect("段")
    }

    fn assert_fts_settled(conn: &Connection) {
        schema::fts_integrity_check_full(conn).expect("索引与内容一致");
        let on: i64 = conn
            .query_row("SELECT v FROM items_fts_config WHERE k = 'secure-delete'", [], |r| {
                r.get(0)
            })
            .expect("配置");
        assert_eq!(on, 1, "FTS secure-delete 应已恢复");
        let triggers: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type = 'trigger' AND name = ?1",
                [schema::FTS_DELETE_TRIGGER],
                |r| r.get(0),
            )
            .expect("触发器");
        assert_eq!(triggers, 1, "删除触发器应在");
    }

    fn hits(store: &ClipStore, q: &str) -> usize {
        store
            .search(q, wisp_types::ClipFilter::All, 50)
            .expect("搜索")
            .items
            .len()
    }

    #[test]
    fn every_fts_purge_strategy_erases_deleted_entries() {
        for purge in [FtsPurge::PerRow, FtsPurge::Merge, FtsPurge::Rebuild] {
            let (_dir, store) = filled_store();
            let before = fts_bytes(&store.inner.lock().conn);
            let (n, _) =
                delete_items_by(&mut store.inner.lock().conn, "id <= ?1", params![80], |_, _| purge).expect("删除");
            assert_eq!(n, 80, "{purge:?}");
            let guard = store.inner.lock();
            assert_fts_settled(&guard.conn);
            let after = fts_bytes(&guard.conn);
            assert!(after < before, "{purge:?}：{before} → {after}");
            if purge != FtsPurge::PerRow {
                // 整体清理后索引里只剩现存的行：与就地重建的大小相当（有残留的旧段或删除标记时会大得多）
                schema::rebuild_fts(&guard.conn).expect("重建");
                let fresh = fts_bytes(&guard.conn);
                assert!(after * 100 <= fresh * 115 + 8192, "{purge:?}：{after} vs 新建 {fresh}");
            }
            drop(guard);
            assert_eq!(hits(&store, "zqmark005x"), 0, "{purge:?}");
            assert_eq!(hits(&store, "zqmark100x"), 1, "{purge:?}");
            // 之后的单条删除同样不留只含删除标记的段
            let blocks = fts_blocks(&store.inner.lock().conn);
            assert!(store.delete(100).expect("删除"));
            assert!(fts_blocks(&store.inner.lock().conn) <= blocks, "{purge:?}");
            assert_eq!(hits(&store, "zqmark099x"), 0);
        }
    }

    #[test]
    fn failed_bulk_delete_rolls_back_and_restores_fts_settings() {
        let (_dir, store) = filled_store();
        let count = |store: &ClipStore| -> i64 {
            store
                .inner
                .lock()
                .conn
                .query_row("SELECT count(*) FROM items", [], |r| r.get(0))
                .expect("计数")
        };
        // 重建：触发器去掉之后 DELETE 才出错（abs(i64::MIN) 溢出）
        let cond = "id <= 50 AND CASE WHEN EXISTS (SELECT 1 FROM sqlite_schema WHERE name = 'items_ad') THEN 1 \
                    ELSE abs(-9223372036854775807 - 1) END";
        assert!(delete_items_by(&mut store.inner.lock().conn, cond, params![], |_, _| FtsPurge::Rebuild).is_err());
        assert_eq!(count(&store), 120);
        assert_fts_settled(&store.inner.lock().conn);
        // 合并：FTS secure-delete 关掉之后 DELETE 才出错
        let cond = "id <= 50 AND CASE WHEN (SELECT v FROM items_fts_config WHERE k = 'secure-delete') = 1 THEN 1 \
                    ELSE abs(-9223372036854775807 - 1) END";
        assert!(delete_items_by(&mut store.inner.lock().conn, cond, params![], |_, _| FtsPurge::Merge).is_err());
        assert_eq!(count(&store), 120);
        assert_fts_settled(&store.inner.lock().conn);
        let blocks = fts_blocks(&store.inner.lock().conn);
        assert!(store.delete(7).expect("删除"));
        assert!(fts_blocks(&store.inner.lock().conn) <= blocks, "secure-delete 应仍生效");
        assert_eq!(hits(&store, "zqmark006x"), 0);
        assert_eq!(hits(&store, "zqmark007x"), 1);
    }

    #[test]
    fn store_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ClipStore>();
    }

    #[test]
    fn icon_keys_are_stable_and_distinct() {
        assert_eq!(icon_key_for("com.apple.Safari"), icon_key_for("com.apple.Safari"));
        assert_ne!(icon_key_for("com.apple.Safari"), icon_key_for("com.apple.safari"));
        assert_eq!(icon_key_for("x").len(), 32);
    }

    #[test]
    fn fts_purge_picks_the_cheapest_strategy() {
        // 语料平均每行约 1.4 KB：1000 条约 1.4 MB 正文，1 万条约 14 MB（与基准一致）
        let vol = |rows: u64, bytes: u64| TextVolume {
            rows,
            bytes,
            row_bytes: bytes.min(rows * 16 * 1024),
        };
        let k1 = vol(1000, 1_400_000);
        let k10 = vol(10_000, 14_400_000);
        // 单条删除、日常的保留策略（删几条）：逐行
        assert_eq!(FtsPurge::choose(&vol(1, 120), &k1), FtsPurge::PerRow);
        assert_eq!(FtsPurge::choose(&vol(3, 1_000), &k1), FtsPurge::PerRow);
        assert_eq!(FtsPurge::choose(&vol(1, 1_000), &k10), FtsPurge::PerRow);
        assert_eq!(FtsPurge::choose(&vol(30, 30_000), &k10), FtsPurge::PerRow);
        // 1 万条里删一条 1 MB 长文本：逐行（只改写十几 KB 正文对应的段页）比合并整个索引快
        assert_eq!(
            FtsPurge::choose(
                &TextVolume {
                    rows: 1,
                    bytes: 98_304,
                    row_bytes: 16_384
                },
                &k10
            ),
            FtsPurge::PerRow
        );
        // 删一部分、剩下很多：合并
        assert_eq!(FtsPurge::choose(&vol(100, 130_000), &k1), FtsPurge::Merge);
        assert_eq!(FtsPurge::choose(&vol(100, 130_000), &k10), FtsPurge::Merge);
        assert_eq!(FtsPurge::choose(&vol(1000, 1_400_000), &k10), FtsPurge::Merge);
        // 删掉大部分：重建（清空、上限 1000 → 10）
        assert_eq!(FtsPurge::choose(&k1, &k1), FtsPurge::Rebuild);
        assert_eq!(FtsPurge::choose(&vol(990, 1_386_000), &k1), FtsPurge::Rebuild);
        assert_eq!(FtsPurge::choose(&vol(500, 700_000), &k1), FtsPurge::Rebuild);
        assert_eq!(FtsPurge::choose(&k10, &k10), FtsPurge::Rebuild);
    }

    #[test]
    fn sidecar_paths() {
        assert_eq!(
            sidecar(Path::new("/a/clipboard.db"), "-wal"),
            PathBuf::from("/a/clipboard.db-wal")
        );
    }
}
