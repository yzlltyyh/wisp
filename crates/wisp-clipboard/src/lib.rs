//! Wisp 的剪贴板历史存储。
//!
//! 职责：SQLite（WAL，`synchronous=NORMAL`，`foreign_keys=ON`）的打开与迁移；记录管线（`RawClipboard` →
//! 隐私规则 → 规范化 → 计算 hash → 去重 → 写入 → 更新缓存）；类型推断；疑似密钥检测与应用排除名单；
//! FTS5 trigram 检索（短查询退化为 LIKE）与高亮区间；保留策略与清理；blobs；最近 200 条的内存缓存；OCR 历史。
//! 不负责读取系统剪贴板（那是 platform 的事）。调用方应在 spawn_blocking 中使用 `ClipStore`。
//!
//! # 资源路径
//! DTO 里的 `*_url`（`SourceApp.icon_url`、`ImageMeta.thumb_url`、`ClipItemDetail.image_url`、`OcrHistoryItem.thumb_url`）
//! 是 `wisp://` 的**资源路径**（`icon/<key>.png`、`thumb/<key>`、`img/<key>`、`ocrimg/<key>`，不带协议与主机），
//! 前端经 `resolveAssetUrl` 转成各平台正确的 URL；`AssetSource` 收到前缀之后的部分，交给 [`ClipStore::blob_path`]。
//!
//! # 调用方约定
//! - **串行化**：`ClipboardService` 用同一把锁（例如 `Mutex<RecentCache>`）包住「改动数据库的 `ClipStore` 调用 +
//!   对应的缓存更新」，两者之间不放开锁；`RecentCache` 的不变式（缓存是数据库排序的前缀）依赖这一点。
//!   只读查询（`recent` / `search` / `get_detail` / `get_payload`）不需要这把锁。
//! - 记录：持锁 `ingest` → `RecentCache::apply`。结果是 `Skipped` 时什么都不做（不取图标：被隐私规则拦下的复制
//!   不能留下任何痕迹）；`Inserted` / `Touched` 且 `meta.source.icon_url` 为 None 时，在后台向平台取该应用的
//!   图标并 `put_icon`（会回填库里该应用所有还没有图标的条目），完成后用 `get_detail(id)` 重新读这一条的元数据，
//!   持锁 `RecentCache::update` 并推送 `updated`（缓存里同一应用更早的条目在下次 `RecentCache::load` 时带上图标）；
//!   平台取不到图标的应用应在内存里记一笔，不要每次复制都重取。
//! - 缩略图：`Inserted` **或** `Touched` 的图片若 `thumb_url` 为 None（例如上次退出前没来得及生成），在后台调用
//!   `make_thumb`（锁外，耗时几十毫秒）；拿到元数据后持锁 `RecentCache::update`（只合并内容字段，不移动条目，
//!   不会把生成期间已删除的条目插回来），推送 `updated` 时用 `RecentCache::get` 取合并后的元数据（不在缓存里时用返回值）。
//! - 粘贴：`get_payload` → 写剪贴板（抑制自身变更）→ 持锁 `touch` + `RecentCache::upsert`；
//! - 收藏（置顶）：持锁 `set_pinned` + `RecentCache::upsert`；命名：持锁 [`ClipStore::set_name`] + `RecentCache::upsert`
//!   （命名即收藏，排序位置可能改变；名称先在 IPC 第一步用 [`clean_favorite_name`] 校验）。收藏会去掉验证码的过期时间，
//!   取消收藏会去掉名称。删除：持锁 `delete` + `RecentCache::remove`；清空：持锁 `clear` + `RecentCache::clear`；
//! - 验证码：记录时（≤ 1 KB 的文本）识别为验证码、且 [`IngestPolicy::otp_ttl_ms`] 开启时，条目带过期时间
//!   （`ClipItemMeta.expires_at`）。打开数据库后、`Inserted` 的条目带过期时间时，用 [`ClipStore::next_expiry`] 排一次性
//!   任务，到点持锁 [`ClipStore::expire_due`] + 逐条 `RecentCache::remove` 并推送 `removed`；系统睡眠会让定时器晚到，
//!   列表与检索结果在返回前用 [`otp_expired`] 过滤掉已过期的；
//! - 检视（芯片条、「粘贴为…」）：[`ClipStore::inspect`] 读正文后交给 [`smart::inspect`]；纯文本（识别结果等）直接用
//!   [`smart::inspect`]。结果不落库，调用方可以按 id 缓存最近几条；
//! - 空闲清理：`retain` → `sweep_orphan_blobs` → `vacuum_if_due` → `RecentCache::load`（`retain` 只返回计数）。
//!   `clear` / `retain` 在一个事务里批量删除并抹掉索引条目，期间持有存储锁：默认上限 1000 条时清空约几十毫秒，
//!   1 万条时约 0.5 s（见 `benches/search.rs`）；`vacuum_if_due` 的临时库在文件里，不随库的大小占内存；
//! - 文件条目：`recent` / `search` / `RecentCache::page` 都不访问文件系统（`exists` 为 true，未检查）。列表显示后，
//!   在后台对可视批次调用 [`refresh_file_exists`]，对返回的 id 推送 `updated`；
//!   `get_detail` 给完整文件列表，只实时检查前 200 个（之后的 `exists` 为 true，未检查）；
//! - 检索范围：只覆盖每条正文的前 [`SEARCH_PREFIX_CHARS`] 个字符（详情与粘贴仍是全文）。`char_count` 超过它的条目，
//!   详情里应注明「仅检索前 N 字」；
//! - 图片自愈：同一张图再次复制（`Touched`）时，若原图文件丢失或残缺会用这次的字节补回；缩略图文件丢失时返回的
//!   `thumb_url` 为 None——按上面「缩略图」一条照常在后台 `make_thumb`；
//! - 打开：`ClipStore::open` 返回 [`StoreError::SchemaTooNew`] 时（用户从新版降级），停用剪贴板历史并提示升级，
//!   不要删除或改名数据目录里的库文件；其余错误按「历史不可用」处理并记日志；
//! - 多设备同步（`sync.rs`）：导出用 [`ClipStore::sync_rows`] / [`ClipStore::sync_export`]（只读，不需要串行锁）；
//!   应用远端条目先在串行锁之外 [`ClipStore::prepare_remote`]（本机隐私规则、类型开关、上限、图片解码与 hash 核对），
//!   再持锁 [`ClipStore::apply_remote_many`]（一个事务，最多 [`MAX_REMOTE_BATCH`] 条）+ 更新缓存；
//!   [`ClipStore::set_favorite_by_hash`]（收藏与名称）、[`ClipStore::set_pinned_by_hash`]、[`ClipStore::delete_by_hashes`]、
//!   [`ClipStore::delete_unsynced_before`] 与本机的收藏 / 删除一样持锁调用。导出的 [`SyncRow`] / [`SyncItem`] 带名称、
//!   过期时间与验证码标记，远端条目的 [`RemoteItem`] 同样带回前两者。识别历史的同步身份是派生键 [`ocr_key`]；
//! - `ClipStore` 的方法都是阻塞的，统一放在 `spawn_blocking` / 工作线程中调用。

mod blobs;
mod cache;
mod error;
mod favorite;
mod html;
mod imaging;
mod infer;
mod ingest;
mod normalize;
mod ocr_history;
mod privacy;
mod retention;
mod row;
mod schema;
mod search;
pub mod smart;
mod store;
mod sync;

pub use blobs::BlobKind;
pub use cache::RecentCache;
pub use error::StoreError;
pub use favorite::{MAX_NAME_CHARS, NameError, clean_name as clean_favorite_name};
pub use infer::infer_kind;
pub use ingest::{IngestOutcome, IngestPolicy, SkipReason};
pub use ocr_history::{OcrHistoryNew, ocr_thumbnail};
pub use privacy::{PrivacyRules, looks_like_secret};
pub use retention::{RetentionPolicy, RetentionReport};
pub use row::refresh_file_exists;
pub use smart::otp::{TTL_MS as OTP_TTL_MS, is_expired as otp_expired};
pub use store::ClipStore;
pub use sync::{
    MAX_REMOTE_BATCH, OcrApplied, OcrSyncItem, OcrSyncRow, PreparedOcr, PreparedRemote, RejectReason, Rejected,
    RemoteApplied, RemoteItem, RemoteOcr, SyncImage, SyncItem, SyncRow, SyncSource, hash_hex, ocr_key, parse_hash_hex,
};

/// 检索覆盖的正文前缀（字符）：FTS 索引、1–2 字查询的 LIKE 与搜索结果的上下文片段都只看每条正文的前这么多个字符
/// （trigram 索引的开销随文本长度超线性增长，见 `schema.rs`）。详情与粘贴不受影响
pub const SEARCH_PREFIX_CHARS: u32 = schema::SEARCH_PREFIX_CHARS;
