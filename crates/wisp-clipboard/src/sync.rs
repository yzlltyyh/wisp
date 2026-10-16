//! 多设备同步用的存储接口：导出本机条目、应用远端条目（剪贴板历史与识别历史）。clipboard.db 不加迁移。
//!
//! # 身份
//! - 剪贴板条目：`items.hash`（32 字节 blake3，已有 `UNIQUE` 索引）。它是内容寻址的：两台设备各自复制同一段内容得到同一个
//!   hash，自然合并成一条。远端条目在本机按本机的规范化与上限**重算** hash（图片解码后重算像素 hash），与声称的不符就拒绝——
//!   否则同一内容在本机会有两个身份，之后的去重与删除都对不上；
//! - 识别记录：表里没有唯一列，用派生键 [`ocr_key`]（不加列，最多几百行，现算即可）。
//!
//! # 应用远端条目
//! 分两步，耗时的工作都不占存储锁：[`ClipStore::prepare_remote`] 在锁外做本机策略检查（隐私规则、类型开关、大小上限、
//! 按天数的保留预检）、图片解码与像素 hash、把原图复制成待就位的临时文件；[`ClipStore::apply_remote_many`] 在一个事务里
//! 落库（最多 [`MAX_REMOTE_BATCH`] 条）。内容不可变：已有条目只合并时间（`created_at` 取较早、`last_used_at` 取较晚）与
//! 收藏（远端给出时采用远端的置顶与名称，否则保留本机的）。FTS 索引由已有的触发器维护。
//!
//! # 收藏与验证码
//! - 收藏的名称只属于收藏：远端给出「收藏」时名称随之写入（缺省即清除），取消收藏时名称一并去掉（与本机的规则相同）；
//! - 验证码条目的过期时间是绝对时间，所有设备一致：远端给出时沿用；没给、但本机开启了自动删除且识别为验证码时，
//!   按 `created_at + 保留时长` 计算（各设备结果相同）。已经过期的不接收（[`RejectReason::Retention`]）。
//!
//! 被本机策略拒绝的条目返回原因（[`RejectReason`]，不含内容），调用方据此记为「本机不接收」，同一版本的记录不再重试；
//! 读写文件之类的暂时性失败返回 [`Rejected::Failed`]，稍后重试。
//!
//! 本模块的写入不经过记录管线、不回调任何东西；「远端写入不再被当作本机改动发回去」由调用方按代码路径保证。
//! 这里的类型手写了 `Debug`：只给出种类、时间与长度，不输出正文、来源应用、文件路径与 hash。

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, Transaction, params};
use wisp_types::{AppRef, ClipItemMeta, ClipKind, OcrHistoryItem, OcrMode, RawClipboard};

use crate::blobs::{self, BlobKind, StagedBlob, hash_stem, stage, stage_file};
use crate::error::StoreError;
use crate::imaging::{self, ImageError, PngInfo};
use crate::ingest::{IngestPolicy, Prepared, SkipReason, TextRecord, prepare};
use crate::ocr_history::{mode_from_str, mode_to_str};
use crate::retention::{RetentionPolicy, age_cutoff};
use crate::row::{META_OTP, kind_from_str};
use crate::store::{
    ClipStore, Inner, NewRow, RowTimes, SET_PINNED_SQL, blob_intact, checkpoint_truncate, delete_items, icon_key_for,
    ocr_history_row, read_meta,
};

/// 一次 [`ClipStore::apply_remote_many`] / [`ClipStore::ocr_apply_remote`] 最多写入的条数：一个事务持有存储锁的时间
/// 有上限，记录、粘贴与检索不会被一大批同步挡住
pub const MAX_REMOTE_BATCH: usize = 50;

/// PNG 文件签名
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];

/// 32 字节 hash → 64 位小写十六进制（同步的键 `c/<hash>` 用）
pub fn hash_hex(hash: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in hash {
        s.push(char::from_digit(u32::from(b >> 4), 16).unwrap_or('0'));
        s.push(char::from_digit(u32::from(b & 0x0f), 16).unwrap_or('0'));
    }
    s
}

/// [`hash_hex`] 的逆：必须恰好是 64 位十六进制（大小写均可）
pub fn parse_hash_hex(s: &str) -> Option<[u8; 32]> {
    let b = s.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in b.chunks_exact(2).enumerate() {
        let hi = char::from(pair[0]).to_digit(16)?;
        let lo = char::from(pair[1]).to_digit(16)?;
        out[i] = u8::try_from(hi << 4 | lo).ok()?;
    }
    Some(out)
}

// ---------- 剪贴板：类型 ----------

/// 本机一条剪贴板条目的同步元数据（不含正文），启动核对与加入时枚举用
#[derive(Clone, PartialEq, Eq)]
pub struct SyncRow {
    pub hash: [u8; 32],
    pub kind: ClipKind,
    pub created_at: i64,
    pub last_used_at: i64,
    pub pinned: bool,
    /// 收藏的名称（只有收藏的条目才有）
    pub name: Option<String>,
    /// 验证码条目的过期时间（Unix 毫秒）
    pub expires_at: Option<i64>,
    /// 记录时识别为验证码（与是否开启自动删除无关；「同步验证码」关闭时据此不发布）
    pub otp: bool,
}

impl fmt::Debug for SyncRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncRow")
            .field("kind", &self.kind)
            .field("created_at", &self.created_at)
            .field("last_used_at", &self.last_used_at)
            .field("pinned", &self.pinned)
            .field("named", &self.name.is_some())
            .field("expires_at", &self.expires_at)
            .field("otp", &self.otp)
            .finish_non_exhaustive()
    }
}

/// 条目的来源应用（应用标识按各自的系统：macOS 是 bundle id，Windows 是 exe，Linux 是 WM_CLASS）
#[derive(Clone, PartialEq, Eq)]
pub struct SyncSource {
    pub app_id: String,
    pub app_name: String,
}

impl fmt::Debug for SyncSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SyncSource { .. }")
    }
}

/// 导出条目的原图：本机 blob 文件（不读进内存，由调用方流式上传）
#[derive(Clone, PartialEq, Eq)]
pub struct SyncImage {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    /// 文件字节数
    pub bytes: u64,
}

impl fmt::Debug for SyncImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

/// 本机一条条目的完整内容（推送时按 hash 读取）
#[derive(Clone, PartialEq, Eq)]
pub struct SyncItem {
    pub hash: [u8; 32],
    pub kind: ClipKind,
    pub created_at: i64,
    pub last_used_at: i64,
    pub pinned: bool,
    /// 规范化后的纯文本（图片为 None）
    pub text: Option<String>,
    pub html: Option<String>,
    pub rtf: Option<Vec<u8>>,
    pub image: Option<SyncImage>,
    pub source: Option<SyncSource>,
    pub truncated: bool,
    /// 收藏的名称（只有收藏的条目才有）
    pub name: Option<String>,
    /// 验证码条目的过期时间（Unix 毫秒）
    pub expires_at: Option<i64>,
    /// 记录时识别为验证码
    pub otp: bool,
}

impl fmt::Debug for SyncItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncItem")
            .field("kind", &self.kind)
            .field("created_at", &self.created_at)
            .field("last_used_at", &self.last_used_at)
            .field("pinned", &self.pinned)
            .field("text_len", &self.text.as_ref().map(String::len))
            .field("html", &self.html.is_some())
            .field("rtf", &self.rtf.is_some())
            .field("image", &self.image)
            .field("truncated", &self.truncated)
            .field("named", &self.name.is_some())
            .field("expires_at", &self.expires_at)
            .field("otp", &self.otp)
            .finish_non_exhaustive()
    }
}

/// 一条远端条目（由同步层从记录里解出；图片原图已下载解密到本机文件）
#[derive(Clone, PartialEq, Eq)]
pub struct RemoteItem {
    /// 声称的身份（`c/<hash>` 里的 hash）；本机重算后必须一致
    pub hash: [u8; 32],
    /// 声称的类型：只用来区分图片 / 文件 / 文本类，文本类的具体类型由本机重新推断
    pub kind: ClipKind,
    pub created_at: i64,
    pub last_used_at: i64,
    /// 置顶（`p/<hash>`）；None = 没有置顶记录，保留本机的状态（新条目不置顶）
    pub pinned: Option<bool>,
    pub text: Option<String>,
    pub html: Option<String>,
    pub rtf: Option<Vec<u8>>,
    /// 图片原图（PNG）所在的本机文件；本方法只读取、复制，不删除也不移动它
    pub image_path: Option<PathBuf>,
    pub source: Option<SyncSource>,
    pub truncated: bool,
    /// 收藏的名称：只在 `pinned == Some(true)` 时有意义（None = 没有名称）。不合规的名称按没有名称处理
    pub name: Option<String>,
    /// 验证码条目的过期时间（Unix 毫秒，记录里的绝对时间）
    pub expires_at: Option<i64>,
}

impl fmt::Debug for RemoteItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteItem")
            .field("kind", &self.kind)
            .field("created_at", &self.created_at)
            .field("last_used_at", &self.last_used_at)
            .field("pinned", &self.pinned)
            .field("text_len", &self.text.as_ref().map(String::len))
            .field("image", &self.image_path.is_some())
            .field("truncated", &self.truncated)
            .field("named", &self.name.is_some())
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// 本机拒绝接收一条远端条目的原因（不含内容）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RejectReason {
    /// 本机的隐私规则（排除名单、疑似密钥）、类型开关或大小上限——与本机复制时被跳过的原因相同
    Skipped(SkipReason),
    /// 不同步的类型（文件条目：路径只在原来的那台机器上有意义）
    Unsupported,
    /// 内容与声称的身份不符（按本机的规范化与上限重算的 hash 不同）
    Mismatch,
    /// 内容缺失或无法解析（图片解码失败、没有原图文件、正文为空）
    Invalid,
    /// 按本机的保留策略会立即被清理：未置顶且早于天数上限，或排在条数上限之外；或者是已经过期的验证码
    Retention,
}

/// [`ClipStore::prepare_remote`] / [`ClipStore::ocr_prepare_remote`] 的失败
#[derive(Debug, thiserror::Error)]
pub enum Rejected {
    /// 本机策略不接收：同一版本的记录不必重试
    #[error("rejected by local policy: {0:?}")]
    Policy(RejectReason),
    /// 暂时性失败（读写文件等）：稍后重试
    #[error(transparent)]
    Failed(#[from] StoreError),
}

impl From<std::io::Error> for Rejected {
    fn from(e: std::io::Error) -> Self {
        Self::Failed(StoreError::Io(e))
    }
}

/// 已在锁外完成检查与准备、等待落库的远端条目（[`ClipStore::apply_remote_many`] 的输入）
pub struct PreparedRemote {
    hash: [u8; 32],
    created_at: i64,
    last_used_at: i64,
    pinned: Option<bool>,
    /// 收藏的名称（已清理；只在 `pinned == Some(true)` 时使用）
    name: Option<String>,
    source: Option<AppRef>,
    /// 本机条数上限（不含置顶）：落库时在事务里判断新条目会不会排在上限之外
    max_items: u32,
    body: PreparedBody,
}

enum PreparedBody {
    Text(TextRecord),
    Image {
        key: String,
        info: PngInfo,
        bytes: u64,
        /// 原图来源文件（落库时发现文件被并发清理掉了，从这里重新复制）
        src: PathBuf,
        /// 准备阶段已复制好、尚未就位的原图；同 hash 的文件已在时为 None
        staged: Option<StagedBlob>,
    },
}

impl PreparedRemote {
    /// 条目的 hash（已在本机重算并核对过）
    pub fn hash(&self) -> &[u8; 32] {
        &self.hash
    }
}

impl fmt::Debug for PreparedRemote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match &self.body {
            PreparedBody::Text(t) => t.kind,
            PreparedBody::Image { .. } => ClipKind::Image,
        };
        f.debug_struct("PreparedRemote")
            .field("kind", &kind)
            .field("created_at", &self.created_at)
            .field("last_used_at", &self.last_used_at)
            .field("pinned", &self.pinned)
            .finish_non_exhaustive()
    }
}

/// 一条远端条目落库的结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteApplied {
    /// 本机原来没有：新插入
    Inserted(ClipItemMeta),
    /// 本机已有、合并后时间或置顶有变化；`moved` = 排序键（置顶、最近使用）变了，列表里的位置可能随之改变
    Merged { meta: ClipItemMeta, moved: bool },
    /// 本机已有、合并后没有变化（本地 id）
    Unchanged(i64),
    /// 落库时才能判断的拒绝（条数上限）
    Rejected(RejectReason),
}

// ---------- 识别历史：类型 ----------

/// 识别记录的同步键：`hex(blake3(b"wisp:ocr\0" ‖ created_at ‖ mode ‖ b"\0" ‖ text))` 的前 32 位（16 字节）。
///
/// 字节编码：`created_at` 是 Unix 毫秒的 i64 **小端** 8 字节；`mode` 是 snake_case 的 UTF-8（`lines`、`paragraphs`、
/// `markdown`、`translate`），后接一个 0 字节；`text` 是识别结果全文的 UTF-8（不做任何规范化）。十六进制为小写。
/// 三个字段在记录的生命周期里不变，两台设备对同一条记录算出同一个键
pub fn ocr_key(created_at: i64, mode: OcrMode, text: &str) -> String {
    ocr_key_raw(created_at, mode_to_str(mode), text)
}

/// [`ocr_key`]，模式按库里保存的原字符串（新版本写入、本版本不认识的模式也能算出与写入方一致的键）
pub(crate) fn ocr_key_raw(created_at: i64, mode: &str, text: &str) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"wisp:ocr\0");
    h.update(&created_at.to_le_bytes());
    h.update(mode.as_bytes());
    h.update(b"\0");
    h.update(text.as_bytes());
    hash_stem(h.finalize().as_bytes())
}

/// 本机一条识别记录的同步元数据
#[derive(Clone, PartialEq, Eq)]
pub struct OcrSyncRow {
    pub id: i64,
    pub key: String,
    pub created_at: i64,
    pub mode: OcrMode,
}

impl fmt::Debug for OcrSyncRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OcrSyncRow")
            .field("id", &self.id)
            .field("created_at", &self.created_at)
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

/// 本机一条识别记录的完整内容（图片给本机文件路径）
#[derive(Clone, PartialEq, Eq)]
pub struct OcrSyncItem {
    pub key: String,
    pub created_at: i64,
    pub mode: OcrMode,
    pub model: String,
    pub text: String,
    pub thumb_path: Option<PathBuf>,
    pub image_path: Option<PathBuf>,
}

impl fmt::Debug for OcrSyncItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OcrSyncItem")
            .field("created_at", &self.created_at)
            .field("mode", &self.mode)
            .field("text_len", &self.text.len())
            .field("thumb", &self.thumb_path.is_some())
            .field("image", &self.image_path.is_some())
            .finish_non_exhaustive()
    }
}

/// 一条远端识别记录（缩略图 / 原图已下载到本机文件；本方法只读取、复制，不删除也不移动它们）
#[derive(Clone, PartialEq, Eq)]
pub struct RemoteOcr {
    /// 声称的键；本机按 [`ocr_key`] 重算后必须一致
    pub key: String,
    pub created_at: i64,
    pub mode: OcrMode,
    pub model: String,
    pub text: String,
    pub thumb_path: Option<PathBuf>,
    pub image_path: Option<PathBuf>,
}

impl fmt::Debug for RemoteOcr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteOcr")
            .field("created_at", &self.created_at)
            .field("mode", &self.mode)
            .field("text_len", &self.text.len())
            .field("thumb", &self.thumb_path.is_some())
            .field("image", &self.image_path.is_some())
            .finish_non_exhaustive()
    }
}

/// 已在锁外准备好的远端识别记录（[`ClipStore::ocr_apply_remote`] 的输入）
pub struct PreparedOcr {
    key: String,
    created_at: i64,
    mode: OcrMode,
    model: String,
    text: String,
    thumb: Option<OcrFile>,
    image: Option<OcrFile>,
}

impl PreparedOcr {
    /// 记录的同步键（已核对）
    pub fn key(&self) -> &str {
        &self.key
    }
}

impl fmt::Debug for PreparedOcr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedOcr")
            .field("created_at", &self.created_at)
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

/// 一个待就位的识别历史图片（文件名是内容 hash，与本机记录时的命名一致）
struct OcrFile {
    name: String,
    src: OcrSrc,
    staged: Option<StagedBlob>,
}

enum OcrSrc {
    Path(PathBuf),
    Bytes(Vec<u8>),
}

/// 一条远端识别记录落库的结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OcrApplied {
    Inserted(OcrHistoryItem),
    /// 本机已有同一键的记录（本地 id）
    Existing(i64),
    Rejected(RejectReason),
}

// ---------- 剪贴板：实现 ----------

impl ClipStore {
    /// 条目的 hash（同步的身份）；不存在时为 None。删除之前取，删除之后行就没了
    pub fn hash_of(&self, id: i64) -> Result<Option<[u8; 32]>, StoreError> {
        Ok(self.ident_of(id)?.map(|(hash, _)| hash))
    }

    /// 条目是否在记录时被识别为验证码（`meta_json.$.otp`，与是否收藏、是否开启自动删除无关）；不存在时为 false。
    /// 「同步验证码」关闭时验证码条目的内容不发布，收藏之后据此补发
    pub fn is_otp(&self, id: i64) -> Result<bool, StoreError> {
        self.guard(|| {
            let inner = self.inner.lock();
            let otp: Option<i64> = inner
                .conn
                .prepare_cached(&format!(
                    "SELECT coalesce(json_extract(meta_json, '{META_OTP}'), 0) FROM items WHERE id = ?1"
                ))?
                .query_row([id], |r| r.get(0))
                .optional()?;
            Ok(otp.is_some_and(|v| v != 0))
        })
    }

    /// 条目的 hash 与类型（文件条目不同步，调用方据此跳过）；不存在时为 None
    pub fn ident_of(&self, id: i64) -> Result<Option<([u8; 32], ClipKind)>, StoreError> {
        self.guard(|| {
            let inner = self.inner.lock();
            let row: Option<(Vec<u8>, String)> = inner
                .conn
                .prepare_cached("SELECT hash, kind FROM items WHERE id = ?1")?
                .query_row([id], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            Ok(row.and_then(|(h, k)| Some((to_hash(&h)?, kind_from_str(&k)))))
        })
    }

    /// 全部条目的同步元数据（不含正文），最近使用的在前。1000 条约 1 ms
    pub fn sync_rows(&self) -> Result<Vec<SyncRow>, StoreError> {
        self.guard(|| {
            let inner = self.inner.lock();
            let mut stmt = inner.conn.prepare_cached(&format!(
                "SELECT hash, kind, created_at, last_used_at, pinned, {SYNC_META_COLS}
                 FROM items ORDER BY last_used_at DESC, id DESC"
            ))?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)? != 0,
                        sync_meta(r, 5)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows
                .into_iter()
                .filter_map(|(h, kind, created_at, last_used_at, pinned, (name, expires_at, otp))| {
                    Some(SyncRow {
                        hash: to_hash(&h)?,
                        kind: kind_from_str(&kind),
                        created_at,
                        last_used_at,
                        pinned,
                        name,
                        expires_at,
                        otp,
                    })
                })
                .collect())
        })
    }

    /// 按 hash 读取一条条目的完整内容（推送时用）。图片给原图文件的路径，不读进内存。
    /// 文件条目（不同步）、不存在的条目、原图文件已丢失或残缺的图片条目返回 None——这些都不发布
    pub fn sync_export(&self, hash: &[u8; 32]) -> Result<Option<SyncItem>, StoreError> {
        self.guard(|| self.sync_export_impl(hash))
    }

    fn sync_export_impl(&self, hash: &[u8; 32]) -> Result<Option<SyncItem>, StoreError> {
        type ExportRow = (
            String,
            i64,
            i64,
            bool,
            Option<String>,
            Option<String>,
            Option<Vec<u8>>,
            Option<String>,
            (Option<i64>, Option<i64>, Option<i64>),
            (Option<String>, Option<String>),
            bool,
            SyncMeta,
        );
        let row: Option<ExportRow> = {
            let inner = self.inner.lock();
            inner
                .conn
                .prepare_cached(&format!(
                    "SELECT kind, created_at, last_used_at, pinned, text, html, rtf, image_blob, image_w, image_h,
                            image_bytes, source_app_id, source_app_name, truncated, {SYNC_META_COLS}
                     FROM items WHERE hash = ?1"
                ))?
                .query_row([&hash[..]], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get::<_, i64>(3)? != 0,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        (r.get(8)?, r.get(9)?, r.get(10)?),
                        (r.get(11)?, r.get(12)?),
                        r.get::<_, i64>(13)? != 0,
                        sync_meta(r, 14)?,
                    ))
                })
                .optional()?
        };
        let Some((kind, created_at, last_used_at, pinned, text, html, rtf, image_blob, dims, src, truncated, meta)) =
            row
        else {
            return Ok(None);
        };
        let kind = kind_from_str(&kind);
        let (text, image) = match kind {
            ClipKind::Files => return Ok(None),
            ClipKind::Image => {
                let Some(key) = image_blob else { return Ok(None) };
                let path = self.blob_path(BlobKind::Img, &key);
                // 文件检查在锁外：最多读 8 字节，丢失或残缺的原图上传了也没用
                if !blob_intact(&path, dims.2) {
                    return Ok(None);
                }
                let to_u32 = |v: Option<i64>| v.and_then(|v| u32::try_from(v).ok()).unwrap_or(0);
                let bytes = fs::metadata(&path).map_or(0, |m| m.len());
                (
                    None,
                    Some(SyncImage {
                        path,
                        width: to_u32(dims.0),
                        height: to_u32(dims.1),
                        bytes,
                    }),
                )
            }
            _ => match text {
                Some(t) if !t.is_empty() => (Some(t), None),
                _ => return Ok(None),
            },
        };
        let source = match src {
            (Some(app_id), name) if !app_id.trim().is_empty() => Some(SyncSource {
                app_id,
                app_name: name.unwrap_or_default(),
            }),
            _ => None,
        };
        Ok(Some(SyncItem {
            hash: *hash,
            kind,
            created_at,
            last_used_at,
            pinned,
            text,
            html: if kind == ClipKind::Image { None } else { html },
            rtf: if kind == ClipKind::Image { None } else { rtf },
            image,
            source,
            truncated,
            name: meta.0,
            expires_at: meta.1,
            otp: meta.2,
        }))
    }

    /// 应用远端条目的第一步（在存储锁之外调用，图片要解码，可能耗时上百毫秒）：
    /// 1. 按天数的保留预检（未置顶且 `last_used_at` 早于天数上限的不接收）；
    /// 2. 本机隐私规则（排除名单、疑似密钥）、类型开关、大小上限——与本机复制走同一套 `prepare`；文件条目不接收；
    /// 3. 按本机的规范化重算 hash（图片：从文件逐行解码重算像素 hash），与声称的不符就拒绝；
    /// 4. 图片：把原图复制成待就位的临时文件（同 hash 的原图已在本机时不复制）。
    ///
    /// 条数上限要在落库的事务里与其他行比较，由 [`Self::apply_remote_many`] 判断
    pub fn prepare_remote(
        &self,
        item: RemoteItem,
        policy: &IngestPolicy,
        retention: &RetentionPolicy,
        now_ms: i64,
    ) -> Result<PreparedRemote, Rejected> {
        use RejectReason as R;
        let reject = |r: RejectReason| Err(Rejected::Policy(r));
        if !item.pinned.unwrap_or(false)
            && age_cutoff(retention, now_ms).is_some_and(|cutoff| item.last_used_at < cutoff)
        {
            return reject(R::Retention);
        }
        let source = item
            .source
            .as_ref()
            .filter(|s| !s.app_id.trim().is_empty())
            .map(|s| AppRef {
                id: s.app_id.clone(),
                alt_id: None,
                name: s.app_name.clone(),
                pid: None,
                path: None,
                window: None,
            });
        let body = match item.kind {
            ClipKind::Files => return reject(R::Unsupported),
            ClipKind::Image => {
                if policy.privacy.excludes(source.as_ref()) {
                    return reject(R::Skipped(SkipReason::ExcludedApp));
                }
                if !policy.record_images {
                    return reject(R::Skipped(SkipReason::KindDisabled));
                }
                let Some(path) = item.image_path else {
                    return reject(R::Invalid);
                };
                self.prepare_remote_image(&item.hash, path, policy)?
            }
            _ => {
                let raw = RawClipboard {
                    source: source.clone(),
                    text: item.text,
                    text_truncated: item.truncated,
                    html: item.html,
                    rtf: item.rtf,
                    ..RawClipboard::default()
                };
                match prepare(&raw, policy) {
                    Ok(Prepared::Text(mut t)) if t.hash == item.hash => {
                        // 收藏的不过期；本机关闭了自动删除时不写过期时间
                        let favorite = item.pinned == Some(true);
                        t.expires_at = policy
                            .otp_ttl_ms
                            .and(item.expires_at.or_else(|| policy.otp_expiry(t.otp, item.created_at)))
                            .filter(|_| !favorite);
                        if t.expires_at.is_some_and(|exp| exp <= now_ms) {
                            return reject(R::Retention);
                        }
                        PreparedBody::Text(t)
                    }
                    Ok(Prepared::Text(_)) => return reject(R::Mismatch),
                    Ok(Prepared::Image(_) | Prepared::Files(_)) | Err(SkipReason::Empty) => return reject(R::Invalid),
                    Err(reason) => return reject(R::Skipped(reason)),
                }
            }
        };
        Ok(PreparedRemote {
            hash: item.hash,
            created_at: item.created_at,
            last_used_at: item.last_used_at,
            pinned: item.pinned,
            name: item
                .name
                .as_deref()
                .and_then(|n| crate::favorite::clean_name(n).ok().flatten()),
            source,
            max_items: retention.max_items,
            body,
        })
    }

    fn prepare_remote_image(
        &self,
        claimed: &[u8; 32],
        path: PathBuf,
        policy: &IngestPolicy,
    ) -> Result<PreparedBody, Rejected> {
        use RejectReason as R;
        let len = fs::metadata(&path)?.len();
        if len == 0 {
            return Err(Rejected::Policy(R::Invalid));
        }
        if usize::try_from(len).ok().is_none_or(|n| n > policy.max_image_bytes) {
            return Err(Rejected::Policy(R::Skipped(SkipReason::TooLarge)));
        }
        if blobs::read_head(&path, PNG_SIGNATURE.len())? != PNG_SIGNATURE {
            return Err(Rejected::Policy(R::Invalid));
        }
        let info = match imaging::inspect_png_from(BufReader::new(fs::File::open(&path)?)) {
            Ok(i) => i,
            Err(ImageError::TooLarge(..)) => return Err(Rejected::Policy(R::Skipped(SkipReason::TooLarge))),
            Err(_) => return Err(Rejected::Policy(R::Invalid)),
        };
        if &info.hash != claimed {
            return Err(Rejected::Policy(R::Mismatch));
        }
        let key = format!("{}.png", hash_stem(&info.hash));
        let staged = if self.blob_path(BlobKind::Img, &key).exists() {
            None
        } else {
            Some(stage_file(&self.blob_dir(BlobKind::Img), &key, &path)?)
        };
        Ok(PreparedBody::Image {
            key,
            info,
            bytes: len,
            src: path,
            staged,
        })
    }

    /// 应用远端条目的第二步：在一个事务里写入（最多 [`MAX_REMOTE_BATCH`] 条），结果与输入一一对应。
    /// 已有条目合并时间与置顶（等价于 `INSERT … ON CONFLICT(hash) DO UPDATE SET created_at = min(…),
    /// last_used_at = max(…), pinned = coalesce(远端, 本机)`，正文不变）；新条目按它在别的设备上的时间与置顶插入，
    /// 未置顶且会排在本机条数上限之外的不插入（[`RemoteApplied::Rejected`]）。原图文件在同一把锁内就位，
    /// 已有条目的原图丢失或残缺时顺带补回
    pub fn apply_remote_many(&self, mut items: Vec<PreparedRemote>) -> Result<Vec<RemoteApplied>, StoreError> {
        if items.len() > MAX_REMOTE_BATCH {
            return Err(StoreError::InvalidInput(format!(
                "at most {MAX_REMOTE_BATCH} remote items per transaction"
            )));
        }
        if items.is_empty() {
            return Ok(Vec::new());
        }
        self.guard(|| self.apply_remote_many_impl(&mut items))
    }

    fn apply_remote_many_impl(&self, items: &mut [PreparedRemote]) -> Result<Vec<RemoteApplied>, StoreError> {
        let dir = self.blob_dir(BlobKind::Img);
        let mut inner = self.inner.lock();
        let Inner { conn, icons } = &mut *inner;
        let tx = conn.transaction()?;
        let mut created = Vec::new();
        let result = apply_rows(&tx, icons, &dir, items, &mut created).and_then(|out| {
            tx.commit()?;
            Ok(out)
        });
        if result.is_err() {
            // 事务已回滚：这次新放进去的原图没有任何行引用
            for path in &created {
                blobs::remove_quietly(path);
            }
        }
        result
    }

    /// 按 hash 置顶 / 取消置顶（名称不变；取消置顶时名称随之去掉）；条目不存在时为 None
    pub fn set_pinned_by_hash(&self, hash: &[u8; 32], pinned: bool) -> Result<Option<ClipItemMeta>, StoreError> {
        self.guard(|| {
            let inner = self.inner.lock();
            let Some(id) = id_of_hash(&inner.conn, hash)? else {
                return Ok(None);
            };
            inner
                .conn
                .prepare_cached(SET_PINNED_SQL)?
                .execute(params![pinned, id])?;
            read_meta(&inner.conn, id).map(Some)
        })
    }

    /// 按 hash 应用远端的收藏状态（`p/<hash>` 的值）：收藏时名称换成 `name`（None 或不合规时清除），取消收藏时名称
    /// 一并去掉；条目不存在时为 None
    pub fn set_favorite_by_hash(
        &self,
        hash: &[u8; 32],
        pinned: bool,
        name: Option<&str>,
    ) -> Result<Option<ClipItemMeta>, StoreError> {
        let name = name.and_then(|n| crate::favorite::clean_name(n).ok().flatten());
        self.guard(|| {
            let inner = self.inner.lock();
            let Some(id) = id_of_hash(&inner.conn, hash)? else {
                return Ok(None);
            };
            write_favorite(&inner.conn, id, pinned, name.as_deref())?;
            read_meta(&inner.conn, id).map(Some)
        })
    }

    /// 按 hash 删除（一个事务，连同 blob），返回实际删除的本地 id。被删内容在库文件与 WAL 中都不留明文（与 `delete` 相同）
    pub fn delete_by_hashes(&self, hashes: &[[u8; 32]]) -> Result<Vec<i64>, StoreError> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }
        self.guard(|| {
            let mut inner = self.inner.lock();
            let mut ids = Vec::with_capacity(hashes.len());
            for hash in hashes {
                if let Some(id) = id_of_hash(&inner.conn, hash)? {
                    ids.push(id);
                }
            }
            ids.sort_unstable();
            ids.dedup();
            self.delete_ids_locked(&mut inner, &ids)?;
            Ok(ids)
        })
    }

    /// 远端「清空」对本机那些同步层不认识的条目的作用（文件条目、超限图片这类没有同步记录的行）：删除
    /// `last_used_at <= wall_ms` 的条目（`keep_pinned` 时不删置顶的），`exclude` 里的 hash 除外（它们由调用方按各自的
    /// 同步记录逐条判断）。时间比较与删除在同一把锁内完成，并发的粘贴把条目挪到「现在」后就不会被删。返回删除的本地 id
    pub fn delete_unsynced_before(
        &self,
        wall_ms: i64,
        keep_pinned: bool,
        exclude: &[[u8; 32]],
    ) -> Result<Vec<i64>, StoreError> {
        let exclude: HashSet<&[u8; 32]> = exclude.iter().collect();
        self.guard(|| {
            let mut inner = self.inner.lock();
            let mut ids = Vec::new();
            {
                let mut stmt = inner
                    .conn
                    .prepare_cached("SELECT id, hash FROM items WHERE last_used_at <= ?1 AND (?2 = 0 OR pinned = 0)")?;
                let mut rows = stmt.query(params![wall_ms, keep_pinned])?;
                while let Some(r) = rows.next()? {
                    let hash: Vec<u8> = r.get(1)?;
                    if to_hash(&hash).is_some_and(|h| exclude.contains(&h)) {
                        continue;
                    }
                    ids.push(r.get::<_, i64>(0)?);
                }
            }
            self.delete_ids_locked(&mut inner, &ids)?;
            Ok(ids)
        })
    }

    /// 在一个事务里删除这些 id（调用方持锁），连同它们的 blob；之后截断 WAL
    pub(crate) fn delete_ids_locked(&self, inner: &mut Inner, ids: &[i64]) -> Result<(), StoreError> {
        if ids.is_empty() {
            return Ok(());
        }
        let list = serde_json::to_string(ids).map_err(|e| StoreError::InvalidInput(e.to_string()))?;
        let (n, blobs) = delete_items(
            &mut inner.conn,
            "id IN (SELECT value FROM json_each(?1))",
            params![list],
        )?;
        if n > 0 {
            checkpoint_truncate(&inner.conn);
        }
        self.remove_item_blobs(&blobs);
        Ok(())
    }
}

/// [`ClipStore::apply_remote_many`] 的逐行处理（在调用方的事务里；新放进去的原图路径记进 `created`，失败时由调用方删除）
fn apply_rows(
    tx: &Transaction<'_>,
    icons: &HashSet<String>,
    dir: &Path,
    items: &mut [PreparedRemote],
    created: &mut Vec<PathBuf>,
) -> Result<Vec<RemoteApplied>, StoreError> {
    let mut out = Vec::with_capacity(items.len());
    for item in items.iter_mut() {
        type Existing = (i64, i64, i64, bool, Option<String>, Option<i64>, Option<String>);
        let existing: Option<Existing> = tx
            .prepare_cached(
                "SELECT id, created_at, last_used_at, pinned, image_blob, image_bytes, json_extract(meta_json, '$.name')
                 FROM items WHERE hash = ?1",
            )?
            .query_row([&item.hash[..]], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get::<_, i64>(3)? != 0,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })
            .optional()?;
        if let Some((id, created_at, last_used_at, pinned, image_blob, image_bytes, name)) = existing {
            if let PreparedBody::Image {
                key,
                bytes,
                src,
                staged,
                ..
            } = &mut item.body
                && image_blob.as_deref() == Some(key.as_str())
                && !blob_intact(&dir.join(key.as_str()), image_bytes)
            {
                // 自愈：本机的原图丢失或残缺（同一 hash 即同样的像素），用这次同步来的补回
                place_image(dir, key, src, staged)?;
                tx.prepare_cached("UPDATE items SET image_bytes = ?1 WHERE id = ?2")?
                    .execute(params![i64::try_from(*bytes).unwrap_or(i64::MAX), id])?;
            }
            let merged = (
                created_at.min(item.created_at),
                last_used_at.max(item.last_used_at),
                item.pinned.unwrap_or(pinned),
            );
            // 远端给出收藏状态时名称随之而来；没给时保留本机的
            let merged_name = match item.pinned {
                Some(true) => item.name.clone(),
                Some(false) => None,
                None => name.clone(),
            };
            if merged == (created_at, last_used_at, pinned) && merged_name == name {
                out.push(RemoteApplied::Unchanged(id));
                continue;
            }
            tx.prepare_cached("UPDATE items SET created_at = ?1, last_used_at = ?2 WHERE id = ?3")?
                .execute(params![merged.0, merged.1, id])?;
            if item.pinned.is_some() {
                write_favorite(tx, id, merged.2, merged_name.as_deref())?;
            }
            out.push(RemoteApplied::Merged {
                meta: read_meta(tx, id)?,
                moved: merged.1 != last_used_at || merged.2 != pinned,
            });
            continue;
        }
        let pinned = item.pinned.unwrap_or(false);
        if !pinned {
            // 排在它前面的未置顶条目已经达到条数上限：插入后会被本机的保留策略立即清理（同一毫秒时新行的 id 更大、排在前面）
            let ahead: i64 = tx
                .prepare_cached("SELECT count(*) FROM items WHERE pinned = 0 AND last_used_at > ?1")?
                .query_row([item.last_used_at], |r| r.get(0))?;
            if ahead >= i64::from(item.max_items) {
                out.push(RemoteApplied::Rejected(RejectReason::Retention));
                continue;
            }
        }
        let row = match &mut item.body {
            PreparedBody::Text(t) => NewRow::Text(t),
            PreparedBody::Image {
                key,
                info,
                bytes,
                src,
                staged,
            } => {
                let path = dir.join(key.as_str());
                if !path.exists() {
                    place_image(dir, key, src, staged)?;
                    created.push(path);
                }
                NewRow::Image {
                    key: key.clone(),
                    info: *info,
                    bytes: *bytes,
                }
            }
        };
        let icon_key = item
            .source
            .as_ref()
            .map(|s| icon_key_for(&s.id))
            .filter(|k| icons.contains(k));
        let id = crate::store::insert_row(
            tx,
            &item.hash,
            &row,
            item.source.as_ref(),
            icon_key.as_deref(),
            RowTimes {
                created_at: item.created_at.min(item.last_used_at),
                last_used_at: item.last_used_at,
                pinned,
            },
        )?;
        if pinned && item.name.is_some() {
            write_favorite(tx, id, true, item.name.as_deref())?;
        }
        out.push(RemoteApplied::Inserted(read_meta(tx, id)?));
    }
    Ok(out)
}

/// 原图就位（调用方持存储锁）：用准备阶段复制好的临时文件；没有（同 hash 的文件当时在、随后被孤儿清理删掉，或上一次
/// 尝试已经用掉）时从来源文件重新复制（少见）
fn place_image(dir: &Path, key: &str, src: &Path, staged: &mut Option<StagedBlob>) -> Result<(), StoreError> {
    match staged.take() {
        Some(s) => s.commit()?,
        None => stage_file(dir, key, src)?.commit()?,
    }
    Ok(())
}

/// 写入收藏状态（调用方持锁 / 在事务里）：置顶规则见 [`SET_PINNED_SQL`]；收藏时名称换成 `name`（None 时清除）
fn write_favorite(conn: &rusqlite::Connection, id: i64, pinned: bool, name: Option<&str>) -> Result<(), StoreError> {
    conn.prepare_cached(SET_PINNED_SQL)?.execute(params![pinned, id])?;
    if pinned {
        conn.prepare_cached(
            "UPDATE items SET meta_json = CASE
                WHEN ?1 IS NULL THEN json_remove(meta_json, '$.name')
                ELSE json_set(coalesce(meta_json, '{}'), '$.name', ?1) END
             WHERE id = ?2",
        )?
        .execute(params![name, id])?;
    }
    Ok(())
}

/// 同步关心的 `meta_json` 字段：名称、过期时间、验证码标记（[`sync_meta`] 按这个顺序读取）
const SYNC_META_COLS: &str = "json_extract(meta_json, '$.name'), json_extract(meta_json, '$.exp'), coalesce(json_extract(meta_json, '$.otp'), 0)";

/// （名称，过期时间，是否验证码）
type SyncMeta = (Option<String>, Option<i64>, bool);

/// 从第 `at` 列开始读 [`SYNC_META_COLS`]
fn sync_meta(r: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<SyncMeta> {
    Ok((r.get(at)?, r.get(at + 1)?, r.get::<_, i64>(at + 2)? != 0))
}

fn id_of_hash(conn: &rusqlite::Connection, hash: &[u8; 32]) -> Result<Option<i64>, StoreError> {
    Ok(conn
        .prepare_cached("SELECT id FROM items WHERE hash = ?1")?
        .query_row([&hash[..]], |r| r.get(0))
        .optional()?)
}

fn to_hash(bytes: &[u8]) -> Option<[u8; 32]> {
    bytes.try_into().ok()
}

// ---------- 识别历史：实现 ----------

impl ClipStore {
    /// 一条识别记录的同步键（[`ocr_key`]）；不存在时为 None
    pub fn ocr_key_of(&self, id: i64) -> Result<Option<String>, StoreError> {
        self.guard(|| {
            let inner = self.inner.lock();
            let row: Option<(i64, String, String)> = inner
                .conn
                .prepare_cached("SELECT created_at, mode, text FROM ocr_history WHERE id = ?1")?
                .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()?;
            Ok(row.map(|(at, mode, text)| ocr_key_raw(at, &mode, &text)))
        })
    }

    /// 全部识别记录的同步元数据（新的在前）。键现算：识别历史最多 500 条，每条只 hash 一次正文
    pub fn ocr_sync_rows(&self) -> Result<Vec<OcrSyncRow>, StoreError> {
        let rows = self.guard(|| ocr_rows(&self.inner.lock().conn))?;
        Ok(rows
            .into_iter()
            .map(|(id, created_at, mode, text)| OcrSyncRow {
                id,
                key: ocr_key_raw(created_at, &mode, &text),
                created_at,
                mode: mode_from_str(&mode),
            })
            .collect())
    }

    /// 按同步键读取一条识别记录的完整内容；不存在时为 None。要逐条算键（O(条数)），批量导出时先用 [`Self::ocr_sync_rows`]
    /// 拿到 id，再逐条 [`Self::ocr_sync_export_by_id`]
    pub fn ocr_sync_export(&self, key: &str) -> Result<Option<OcrSyncItem>, StoreError> {
        let id = self.ocr_sync_rows()?.into_iter().find(|r| r.key == key).map(|r| r.id);
        match id {
            Some(id) => self.ocr_sync_export_by_id(id),
            None => Ok(None),
        }
    }

    /// 按本地 id 读取一条识别记录的完整内容（图片给本机文件路径，文件不在时为 None）；不存在时为 None
    pub fn ocr_sync_export_by_id(&self, id: i64) -> Result<Option<OcrSyncItem>, StoreError> {
        type Row = (i64, String, String, String, Option<String>, Option<String>);
        let row: Option<Row> = self.guard(|| {
            let inner = self.inner.lock();
            Ok(inner
                .conn
                .prepare_cached(
                    "SELECT created_at, mode, model, text, thumb_blob, image_blob FROM ocr_history WHERE id = ?1",
                )?
                .query_row([id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
                })
                .optional()?)
        })?;
        let Some((created_at, mode, model, text, thumb, image)) = row else {
            return Ok(None);
        };
        let file = |name: Option<String>| name.map(|n| self.blob_path(BlobKind::Ocr, &n)).filter(|p| p.is_file());
        Ok(Some(OcrSyncItem {
            key: ocr_key_raw(created_at, &mode, &text),
            created_at,
            mode: mode_from_str(&mode),
            model,
            text,
            thumb_path: file(thumb),
            image_path: file(image),
        }))
    }

    /// 应用远端识别记录的第一步（在存储锁之外调用）：核对键、把图片复制成待就位的临时文件（文件名是内容 hash，
    /// 本机已有同名文件时不复制）；没有缩略图、但有原图时从原图生成。图片不是 PNG / JPEG 时当作没有
    pub fn ocr_prepare_remote(&self, item: RemoteOcr) -> Result<PreparedOcr, Rejected> {
        use RejectReason as R;
        if item.text.trim().is_empty() {
            return Err(Rejected::Policy(R::Invalid));
        }
        if ocr_key(item.created_at, item.mode, &item.text) != item.key {
            return Err(Rejected::Policy(R::Mismatch));
        }
        let dir = self.blob_dir(BlobKind::Ocr);
        let image = match item.image_path.as_deref() {
            Some(p) => stage_ocr_file(&dir, p)?,
            None => None,
        };
        let thumb = match item.thumb_path.as_deref() {
            Some(p) => stage_ocr_file(&dir, p)?,
            None => None,
        };
        let thumb = match (thumb, image.as_ref()) {
            (Some(t), _) => Some(t),
            (
                None,
                Some(OcrFile {
                    src: OcrSrc::Path(p), ..
                }),
            ) => derive_ocr_thumb(&dir, p)?,
            (None, _) => None,
        };
        Ok(PreparedOcr {
            key: item.key,
            created_at: item.created_at,
            mode: item.mode,
            model: item.model,
            text: item.text,
            thumb,
            image,
        })
    }

    /// 应用远端识别记录的第二步：一个事务（最多 [`MAX_REMOTE_BATCH`] 条），结果与输入一一对应。按同步键去重（本机已有、
    /// 或同一批里已出现过的返回 [`OcrApplied::Existing`]）；会排在 `limit` 条之外的不插入（[`RejectReason::Retention`]）。
    /// 不裁剪：调用方随后按上限调用 [`Self::ocr_history_trim`]
    pub fn ocr_apply_remote(&self, mut items: Vec<PreparedOcr>, limit: u32) -> Result<Vec<OcrApplied>, StoreError> {
        if items.len() > MAX_REMOTE_BATCH {
            return Err(StoreError::InvalidInput(format!(
                "at most {MAX_REMOTE_BATCH} remote items per transaction"
            )));
        }
        if items.is_empty() {
            return Ok(Vec::new());
        }
        self.guard(|| self.ocr_apply_remote_impl(&mut items, limit))
    }

    fn ocr_apply_remote_impl(&self, items: &mut [PreparedOcr], limit: u32) -> Result<Vec<OcrApplied>, StoreError> {
        let dir = self.blob_dir(BlobKind::Ocr);
        let mut inner = self.inner.lock();
        let mut known: HashMap<String, i64> = ocr_rows(&inner.conn)?
            .into_iter()
            .map(|(id, at, mode, text)| (ocr_key_raw(at, &mode, &text), id))
            .collect();
        let tx = inner.conn.transaction()?;
        let mut created = Vec::new();
        let result = ocr_apply_rows(&tx, &dir, items, limit, &mut known, &mut created).and_then(|out| {
            tx.commit()?;
            Ok(out)
        });
        if result.is_err() {
            for path in &created {
                blobs::remove_quietly(path);
            }
        }
        result
    }

    /// 按同步键删除识别记录（找键与删除在同一次持锁里），不再被引用的图片一并删除；返回实际删除的本地 id
    pub fn ocr_delete_by_keys(&self, keys: &[String]) -> Result<Vec<i64>, StoreError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let wanted: HashSet<&str> = keys.iter().map(String::as_str).collect();
        self.guard(|| {
            let mut inner = self.inner.lock();
            let ids: Vec<i64> = ocr_rows(&inner.conn)?
                .into_iter()
                .filter(|(_, at, mode, text)| wanted.contains(ocr_key_raw(*at, mode, text).as_str()))
                .map(|(id, ..)| id)
                .collect();
            Ok(self
                .ocr_history_delete_locked(&mut inner, &ids, false)?
                .into_iter()
                .map(|(id, _)| id)
                .collect())
        })
    }

    /// 删除一条识别记录，返回它的同步键（与删除在同一个事务里读取）；不存在时为 None
    pub fn ocr_history_delete_keyed(&self, id: i64) -> Result<Option<String>, StoreError> {
        self.guard(|| {
            Ok(self
                .ocr_history_delete_rows(&[id], true)?
                .into_iter()
                .next()
                .and_then(|(_, key)| key))
        })
    }
}

/// 全部识别记录的（id，created_at，库里的 mode 原字符串，text），新的在前
fn ocr_rows(conn: &rusqlite::Connection) -> Result<Vec<(i64, i64, String, String)>, StoreError> {
    let mut stmt =
        conn.prepare_cached("SELECT id, created_at, mode, text FROM ocr_history ORDER BY created_at DESC, id DESC")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

/// [`ClipStore::ocr_apply_remote`] 的逐行处理（在调用方的事务里）
fn ocr_apply_rows(
    tx: &Transaction<'_>,
    dir: &Path,
    items: &mut [PreparedOcr],
    limit: u32,
    known: &mut HashMap<String, i64>,
    created: &mut Vec<PathBuf>,
) -> Result<Vec<OcrApplied>, StoreError> {
    let mut out = Vec::with_capacity(items.len());
    for item in items.iter_mut() {
        if let Some(&id) = known.get(&item.key) {
            out.push(OcrApplied::Existing(id));
            continue;
        }
        // 与 `ocr_history_trim` 同一口径（created_at 新的在前，同一毫秒新行的 id 更大、排在前面）
        let ahead: i64 = tx
            .prepare_cached("SELECT count(*) FROM ocr_history WHERE created_at > ?1")?
            .query_row([item.created_at], |r| r.get(0))?;
        if ahead >= i64::from(limit) {
            out.push(OcrApplied::Rejected(RejectReason::Retention));
            continue;
        }
        let mut names = [None, None];
        for (slot, file) in names.iter_mut().zip([&mut item.thumb, &mut item.image]) {
            let Some(file) = file.as_mut() else { continue };
            let path = dir.join(&file.name);
            if !path.exists() {
                match (file.staged.take(), &file.src) {
                    (Some(s), _) => s.commit()?,
                    (None, OcrSrc::Path(p)) => stage_file(dir, &file.name, p)?.commit()?,
                    (None, OcrSrc::Bytes(b)) => stage(dir, &file.name, b)?.commit()?,
                }
                created.push(path);
            }
            *slot = Some(file.name.clone());
        }
        let [thumb, image] = names;
        tx.execute(
            "INSERT INTO ocr_history(created_at, mode, model, text, thumb_blob, image_blob) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![item.created_at, mode_to_str(item.mode), item.model, item.text, thumb, image],
        )?;
        let id = tx.last_insert_rowid();
        let row = tx
            .prepare_cached(
                "SELECT id, created_at, mode, model, text, thumb_blob, image_blob FROM ocr_history WHERE id = ?1",
            )?
            .query_row([id], ocr_history_row)?;
        known.insert(item.key.clone(), id);
        out.push(OcrApplied::Inserted(row));
    }
    Ok(out)
}

/// 识别历史图片的待就位文件：文件名 = 内容 hash + 按魔数识别的扩展名（与本机记录时的命名一致，同一张图共用一个文件）。
/// 不是 PNG / JPEG 时返回 None（这张图当作没有）
fn stage_ocr_file(dir: &Path, src: &Path) -> Result<Option<OcrFile>, Rejected> {
    let head = blobs::read_head(src, PNG_SIGNATURE.len())?;
    let Some(ext) = imaging::sniff_ext(&head) else {
        return Ok(None);
    };
    let (hash, _) = blobs::hash_file(src)?;
    let name = format!("{}.{ext}", hash_stem(&hash));
    let staged = if dir.join(&name).exists() {
        None
    } else {
        Some(stage_file(dir, &name, src)?)
    };
    Ok(Some(OcrFile {
        name,
        src: OcrSrc::Path(src.to_path_buf()),
        staged,
    }))
}

/// 远端记录只带了原图：从原图生成缩略图（与本机记录时同一规格）；生成失败时没有缩略图
fn derive_ocr_thumb(dir: &Path, image: &Path) -> Result<Option<OcrFile>, Rejected> {
    let bytes = fs::read(image)?;
    let Ok(thumb) = imaging::thumbnail_any(&bytes, imaging::THUMB_LONG_EDGE) else {
        return Ok(None);
    };
    drop(bytes);
    let name = crate::store::ocr_blob_name(&thumb.bytes)?;
    let staged = if dir.join(&name).exists() {
        None
    } else {
        Some(stage(dir, &name, &thumb.bytes)?)
    };
    Ok(Some(OcrFile {
        name,
        src: OcrSrc::Bytes(thumb.bytes),
        staged,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_hex_round_trips() {
        let mut h = [0u8; 32];
        for (i, b) in h.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(37);
        }
        let s = hash_hex(&h);
        assert_eq!(s.len(), 64);
        assert!(s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert_eq!(parse_hash_hex(&s), Some(h));
        assert_eq!(parse_hash_hex(&s.to_uppercase()), Some(h));
        assert_eq!(parse_hash_hex(&s[..62]), None);
        assert_eq!(parse_hash_hex(&format!("{}zz", &s[..62])), None);
        assert_eq!(&s[..32], hash_stem(&h), "前 32 位与 blob 文件名主干一致");
    }

    #[test]
    fn ocr_key_byte_encoding_is_fixed() {
        // 写死的编码：b"wisp:ocr\0" ‖ created_at（i64 小端）‖ mode ‖ 0 ‖ text
        let mut bytes = b"wisp:ocr\0".to_vec();
        bytes.extend_from_slice(&1_760_000_000_123_i64.to_le_bytes());
        bytes.extend_from_slice(b"markdown\0");
        bytes.extend_from_slice("识别 text".as_bytes());
        let expected = hash_stem(blake3::hash(&bytes).as_bytes());
        assert_eq!(ocr_key(1_760_000_000_123, OcrMode::Markdown, "识别 text"), expected);
        assert_eq!(expected.len(), 32);
        // 任一字段不同，键就不同
        assert_ne!(ocr_key(1_760_000_000_124, OcrMode::Markdown, "识别 text"), expected);
        assert_ne!(ocr_key(1_760_000_000_123, OcrMode::Lines, "识别 text"), expected);
        assert_ne!(ocr_key(1_760_000_000_123, OcrMode::Markdown, "识别 text "), expected);
        // 模式与正文之间有分隔：不会因为拼接而相撞
        assert_ne!(
            ocr_key_raw(1, "lines", "x"),
            ocr_key_raw(1, "line", "sx"),
            "mode 与 text 的边界"
        );
    }

    #[test]
    fn sync_meta_columns_use_the_meta_json_keys() {
        use crate::row::{META_EXP, META_NAME, META_OTP};
        for key in [META_NAME, META_EXP, META_OTP] {
            assert!(SYNC_META_COLS.contains(key), "{key}");
        }
    }

    #[test]
    fn debug_output_never_contains_content() {
        let item = RemoteItem {
            hash: [7; 32],
            kind: ClipKind::Text,
            created_at: 1,
            last_used_at: 2,
            pinned: None,
            text: Some("top secret text".into()),
            html: Some("<b>top secret html</b>".into()),
            rtf: None,
            image_path: Some(PathBuf::from("/private/spool/name.png")),
            source: Some(SyncSource {
                app_id: "com.example.Secret".into(),
                app_name: "Secret App".into(),
            }),
            truncated: false,
            name: Some("Secret favorite".into()),
            expires_at: Some(3),
        };
        let s = format!("{item:?}");
        for needle in ["secret", "spool", "Secret", "0707"] {
            assert!(!s.contains(needle), "{s}");
        }
        let ocr = RemoteOcr {
            key: "k".repeat(32),
            created_at: 1,
            mode: OcrMode::Lines,
            model: "m".into(),
            text: "hidden words".into(),
            thumb_path: None,
            image_path: Some(PathBuf::from("/private/spool/x.png")),
        };
        let s = format!("{ocr:?}");
        assert!(
            !s.contains("hidden") && !s.contains("spool") && !s.contains("kkkk"),
            "{s}"
        );
    }
}
