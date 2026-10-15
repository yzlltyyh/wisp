//! 同步数据模型：记录、日志段、head、vault.json、设备信息，以及载荷的 JSON + deflate 编解码。
//!
//! 加密不在这里（见 [`crate::crypto`]）：这里产出 / 消费的是明文载荷与它的 flags。
//! 读取一律宽容未知字段（serde 默认忽略），并先检查格式版本，版本更新的载荷报 `FormatTooNew` 而不是 `Corrupt`。

use std::borrow::Cow;
use std::fmt;
use std::io::{Read, Write};

use base64::Engine as _;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use wisp_types::OsKind;

use crate::error::SyncError;
use crate::hlc::Hlc;
use crate::layout;

/// vault.json 的 `wisp_sync` 与各载荷的 `v`：本版本读写的格式版本
pub const FORMAT_VERSION: u32 = 1;

/// 载荷 flags：bit0 = 明文经过 raw DEFLATE（RFC 1951）压缩
pub const FLAG_DEFLATE: u8 = 0b0000_0001;

/// 本版本认识的全部 flag 位；出现其他位视为更新的格式
pub const KNOWN_FLAGS: u8 = FLAG_DEFLATE;

/// 解压后明文的上限：普通段 ≤ 1 MiB，单条超大记录独占一段，这个上限只用来挡住解压炸弹
pub const MAX_PLAINTEXT_BYTES: usize = 64 << 20;

/// 小于这个长度的 JSON 不尝试压缩（压缩头的开销抵消收益）
const DEFLATE_MIN_BYTES: usize = 256;

// ---------------------------------------------------------------------------------------------
// 域与记录
// ---------------------------------------------------------------------------------------------

/// 数据域。未知的域原样保留（较新版本写入的数据），进副本但不应用，升级后再应用
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Domain {
    /// settings.json 的投影
    Cfg,
    /// styles.json 的投影
    Sty,
    /// 术语表
    Glo,
    /// API Key（按 provider id）
    Key,
    /// 剪贴板历史
    Clip,
    /// 识别历史
    Ocr,
    /// 本版本不认识的域，保存原始名称
    Unknown(String),
}

impl Domain {
    /// 本版本认识的域
    pub const KNOWN: [Domain; 6] = [
        Domain::Cfg,
        Domain::Sty,
        Domain::Glo,
        Domain::Key,
        Domain::Clip,
        Domain::Ocr,
    ];

    pub fn as_str(&self) -> &str {
        match self {
            Self::Cfg => "cfg",
            Self::Sty => "sty",
            Self::Glo => "glo",
            Self::Key => "key",
            Self::Clip => "clip",
            Self::Ocr => "ocr",
            Self::Unknown(s) => s,
        }
    }

    pub fn parse(s: &str) -> Domain {
        match s {
            "cfg" => Self::Cfg,
            "sty" => Self::Sty,
            "glo" => Self::Glo,
            "key" => Self::Key,
            "clip" => Self::Clip,
            "ocr" => Self::Ocr,
            other => Self::Unknown(other.to_string()),
        }
    }

    pub fn is_known(&self) -> bool {
        !matches!(self, Self::Unknown(_))
    }

    /// 寄存器式的域（每个 key 的胜出值直接写回本机）：cfg / sty / glo / key
    pub fn is_register(&self) -> bool {
        matches!(self, Self::Cfg | Self::Sty | Self::Glo | Self::Key)
    }

    /// 条目式的域（由引擎从 `c` / `p` / `o` / `@clear` 寄存器算出逐条变更）：clip / ocr
    pub fn is_content(&self) -> bool {
        matches!(self, Self::Clip | Self::Ocr)
    }

    /// 应用顺序：cfg → sty / glo → key → clip → ocr（Key 依赖 provider 已存在；未知域排最后且不应用）
    pub fn apply_rank(&self) -> u8 {
        match self {
            Self::Cfg => 0,
            Self::Sty | Self::Glo => 1,
            Self::Key => 2,
            Self::Clip => 3,
            Self::Ocr => 4,
            Self::Unknown(_) => u8::MAX,
        }
    }
}

impl fmt::Display for Domain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for Domain {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Domain {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = Cow::<str>::deserialize(d)?;
        Ok(Domain::parse(&s))
    }
}

/// blob 在记录里的用途。未知用途原样保留
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub enum BlobRole {
    /// 原图（剪贴板图片、识别截图原图）
    #[default]
    Image,
    /// 识别记录的缩略图
    Thumbnail,
    Other(String),
}

impl BlobRole {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Image => "image",
            Self::Thumbnail => "thumb",
            Self::Other(s) => s,
        }
    }

    pub fn parse(s: &str) -> BlobRole {
        match s {
            "image" => Self::Image,
            "thumb" => Self::Thumbnail,
            other => Self::Other(other.to_string()),
        }
    }
}

impl Serialize for BlobRole {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for BlobRole {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = Cow::<str>::deserialize(d)?;
        Ok(BlobRole::parse(&s))
    }
}

/// 记录引用的一个 blob（只有上传者 `dev` 写和删）
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlobRef {
    /// 上传者的 device_id（blob 在 `blobs/<dev>/` 下）
    pub dev: String,
    /// blob 名（32 位十六进制，由内容身份经 HMAC 得出）
    pub name: String,
    /// 明文字节数
    pub bytes: u64,
    #[serde(default)]
    pub role: BlobRole,
}

/// 一条同步记录：某个域里某个 key 在时间 t 的值
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub d: Domain,
    pub k: String,
    pub t: Hlc,
    /// `None` 为墓碑。JSON 里墓碑**省略** `v` 字段；`"v": null` 是值为 null 的寄存器（如未设置的默认模型），
    /// 两者不能混淆，所以反序列化时只要字段出现就是 `Some`
    #[serde(default, deserialize_with = "present_value", skip_serializing_if = "Option::is_none")]
    pub v: Option<Value>,
    /// 引用的 blob（记录级，任何版本都能看见）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub b: Vec<BlobRef>,
}

fn present_value<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

impl Record {
    pub fn is_tombstone(&self) -> bool {
        self.v.is_none()
    }
}

/// LWW 胜负：`(t, device_id)` 字典序大者胜。合并因此满足交换律、结合律与幂等
pub fn outranks(t: Hlc, dev: &str, other_t: Hlc, other_dev: &str) -> bool {
    (t, dev) > (other_t, other_dev)
}

/// `@clear/*`（剪贴板）与 `@clear`（识别历史）寄存器的值；截止点是记录自身的 t，
/// `wall_ms` 只用于本机没有对应寄存器的条目（与本地 `last_used_at` / `created_at` 比较）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClearValue {
    pub wall_ms: i64,
}

// ---------------------------------------------------------------------------------------------
// key 的拼写
// ---------------------------------------------------------------------------------------------

/// clip / ocr 域的 key 拼写与解析
pub mod keys {
    use super::{hex, parse_hex32};

    /// 清空全部剪贴板历史
    pub const CLIP_CLEAR_ALL: &str = "@clear/all";
    /// 清空未置顶的剪贴板历史
    pub const CLIP_CLEAR_UNPINNED: &str = "@clear/unpinned";
    /// 清空识别历史
    pub const OCR_CLEAR: &str = "@clear";

    /// `c/<hash hex>`：剪贴板条目内容
    pub fn clip_content(hash: &[u8; 32]) -> String {
        format!("c/{}", hex(hash))
    }

    /// `p/<hash hex>`：置顶状态
    pub fn clip_pin(hash: &[u8; 32]) -> String {
        format!("p/{}", hex(hash))
    }

    /// `o/<id>`：识别记录
    pub fn ocr_item(id: &str) -> String {
        format!("o/{id}")
    }

    /// clip 域 key 的解析结果
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ClipKey {
        Content([u8; 32]),
        Pin([u8; 32]),
        ClearAll,
        ClearUnpinned,
    }

    pub fn parse_clip(key: &str) -> Option<ClipKey> {
        match key {
            CLIP_CLEAR_ALL => Some(ClipKey::ClearAll),
            CLIP_CLEAR_UNPINNED => Some(ClipKey::ClearUnpinned),
            _ => {
                if let Some(h) = key.strip_prefix("c/") {
                    parse_hex32(h).map(ClipKey::Content)
                } else if let Some(h) = key.strip_prefix("p/") {
                    parse_hex32(h).map(ClipKey::Pin)
                } else {
                    None
                }
            }
        }
    }

    /// ocr 域 key 的解析结果
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum OcrKey<'a> {
        /// 识别记录 id（32 位小写十六进制）
        Item(&'a str),
        Clear,
    }

    pub fn parse_ocr(key: &str) -> Option<OcrKey<'_>> {
        if key == OCR_CLEAR {
            return Some(OcrKey::Clear);
        }
        let id = key.strip_prefix("o/")?;
        (id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))).then_some(OcrKey::Item(id))
    }
}

/// 小写十六进制
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0xF)]));
    }
    out
}

/// 解析 64 位小写十六进制为 32 字节；大写或长度不对返回 `None`（key 的拼写必须唯一）
pub fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    fn nibble(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in bytes.chunks_exact(2).enumerate() {
        out[i] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(out)
}

/// 日志段的内容身份：密文 blake3 的前 16 字节（十六进制），写进 head，读者据此发现被替换的段
pub fn segment_hash(ciphertext: &[u8]) -> String {
    hex(&blake3::hash(ciphertext).as_bytes()[..16])
}

// ---------------------------------------------------------------------------------------------
// 载荷
// ---------------------------------------------------------------------------------------------

/// 写入时各配置文件的 schema_version（读者据此判断值是否可能来自更新的版本）
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaVersions {
    #[serde(default)]
    pub settings: u32,
    #[serde(default)]
    pub styles: u32,
    #[serde(default)]
    pub glossary: u32,
}

/// 日志段明文。`[lo, hi]` 是本段覆盖的逻辑序号区间：普通推送 `lo = hi`；
/// 压缩产物覆盖 `[1, max_hi]`，拆成多个部分时每部分同区间、不同 `w`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentPayload {
    pub v: u32,
    /// 作者的 device_id
    pub device: String,
    pub w: u64,
    pub lo: u64,
    pub hi: u64,
    #[serde(default)]
    pub schema: SchemaVersions,
    pub records: Vec<Record>,
}

impl SegmentPayload {
    pub fn encode(&self) -> Result<EncodedPayload, SyncError> {
        encode_payload(self)
    }

    /// 解压并解析；先检查 `v`，更新的格式报 `FormatTooNew`
    pub fn decode(flags: u8, bytes: &[u8]) -> Result<SegmentPayload, SyncError> {
        decode_versioned(flags, bytes)
    }
}

/// head 里列出的一个日志段
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentEntry {
    pub w: u64,
    pub lo: u64,
    pub hi: u64,
    /// 密文字节数
    pub bytes: u64,
    /// [`segment_hash`]
    pub hash: String,
}

/// 设备信息（只出现在加密的 head 里）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub os: OsKind,
    pub app_version: String,
}

/// head 明文：设备信息 + 本设备当前有效的日志段清单
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeadPayload {
    pub v: u32,
    pub vault_id: String,
    pub device: DeviceInfo,
    pub updated_ms: i64,
    pub segments: Vec<SegmentEntry>,
}

impl HeadPayload {
    /// 清单中最大的 `hi`（没有段时为 0）
    pub fn max_hi(&self) -> u64 {
        self.segments.iter().map(|s| s.hi).max().unwrap_or(0)
    }

    pub fn encode(&self) -> Result<EncodedPayload, SyncError> {
        encode_payload(self)
    }

    /// 解压并解析；先检查 `v`，更新的格式报 `FormatTooNew`
    pub fn decode(flags: u8, bytes: &[u8]) -> Result<HeadPayload, SyncError> {
        decode_versioned(flags, bytes)
    }
}

/// 编码后的明文载荷（尚未加密）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedPayload {
    /// 写进密文头的 flags（[`FLAG_DEFLATE`]）
    pub flags: u8,
    pub bytes: Vec<u8>,
}

/// JSON 序列化；足够长且压缩后更小时用 raw DEFLATE
pub fn encode_payload<T: Serialize>(value: &T) -> Result<EncodedPayload, SyncError> {
    let json = serde_json::to_vec(value).map_err(|_| SyncError::Internal("payload serialization failed"))?;
    if json.len() < DEFLATE_MIN_BYTES {
        return Ok(EncodedPayload { flags: 0, bytes: json });
    }
    let mut encoder =
        flate2::write::DeflateEncoder::new(Vec::with_capacity(json.len() / 2), flate2::Compression::default());
    encoder.write_all(&json)?;
    let packed = encoder.finish()?;
    Ok(if packed.len() < json.len() {
        EncodedPayload {
            flags: FLAG_DEFLATE,
            bytes: packed,
        }
    } else {
        EncodedPayload { flags: 0, bytes: json }
    })
}

/// 按 flags 还原 JSON 明文：未知 flag 位 ⇒ `FormatTooNew`；解压失败或超过上限 ⇒ `Corrupt`
pub fn inflate(flags: u8, bytes: &[u8]) -> Result<Cow<'_, [u8]>, SyncError> {
    inflate_limited(flags, bytes, MAX_PLAINTEXT_BYTES)
}

fn inflate_limited(flags: u8, bytes: &[u8], limit: usize) -> Result<Cow<'_, [u8]>, SyncError> {
    if flags & !KNOWN_FLAGS != 0 {
        return Err(SyncError::FormatTooNew);
    }
    if flags & FLAG_DEFLATE == 0 {
        if bytes.len() > limit {
            return Err(SyncError::Corrupt);
        }
        return Ok(Cow::Borrowed(bytes));
    }
    let mut out = Vec::new();
    let cap = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    flate2::read::DeflateDecoder::new(bytes)
        .take(cap)
        .read_to_end(&mut out)
        .map_err(|_| SyncError::Corrupt)?;
    if out.len() > limit {
        return Err(SyncError::Corrupt);
    }
    Ok(Cow::Owned(out))
}

/// 还原并解析为任意类型（不检查版本）；JSON 不合法 ⇒ `Corrupt`（serde 的错误消息可能带数据片段，不保留）
pub fn decode_payload<T: DeserializeOwned>(flags: u8, bytes: &[u8]) -> Result<T, SyncError> {
    let plain = inflate(flags, bytes)?;
    serde_json::from_slice(&plain).map_err(|_| SyncError::Corrupt)
}

/// 先读 `v`：缺失或为 0 ⇒ `Corrupt`；大于本版本 ⇒ `FormatTooNew`（新格式的结构可能对不上，不能报成损坏）
fn decode_versioned<T: DeserializeOwned>(flags: u8, bytes: &[u8]) -> Result<T, SyncError> {
    let value: Value = decode_payload(flags, bytes)?;
    check_version(value.get("v"))?;
    serde_json::from_value(value).map_err(|_| SyncError::Corrupt)
}

fn check_version(v: Option<&Value>) -> Result<(), SyncError> {
    match v.and_then(Value::as_u64) {
        None | Some(0) => Err(SyncError::Corrupt),
        Some(v) if v > u64::from(FORMAT_VERSION) => Err(SyncError::FormatTooNew),
        Some(_) => Ok(()),
    }
}

// ---------------------------------------------------------------------------------------------
// vault.json
// ---------------------------------------------------------------------------------------------

/// 主密钥的派生参数（明文写在 vault.json 里）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfParams {
    pub alg: String,
    pub iterations: u32,
    #[serde(with = "b64")]
    pub salt: Vec<u8>,
}

impl KdfParams {
    pub const ALG: &'static str = "pbkdf2-sha256";
    /// 新建 vault 使用的迭代次数
    pub const DEFAULT_ITERATIONS: u32 = 600_000;
    pub const MIN_ITERATIONS: u32 = 600_000;
    /// 上限挡住恶意或损坏的参数让连接卡住几分钟
    pub const MAX_ITERATIONS: u32 = 10_000_000;
    /// 新建 vault 使用的盐长度
    pub const DEFAULT_SALT_BYTES: usize = 16;
    pub const MIN_SALT_BYTES: usize = 16;
    pub const MAX_SALT_BYTES: usize = 64;

    /// 只接受本版本支持的算法与参数范围；否则视为更新的格式，不做任何计算
    pub fn validate(&self) -> Result<(), SyncError> {
        let ok = self.alg == Self::ALG
            && (Self::MIN_ITERATIONS..=Self::MAX_ITERATIONS).contains(&self.iterations)
            && (Self::MIN_SALT_BYTES..=Self::MAX_SALT_BYTES).contains(&self.salt.len());
        if ok { Ok(()) } else { Err(SyncError::FormatTooNew) }
    }
}

/// vault.json：同步数据的入口（明文）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultHeader {
    pub wisp_sync: u32,
    pub vault_id: String,
    pub created_ms: i64,
    pub kdf: KdfParams,
    /// 口令校验：用 data 子密钥封装的固定明文（见 [`crate::crypto::make_check`]）
    #[serde(with = "b64")]
    pub check: Vec<u8>,
}

#[derive(Deserialize)]
struct RawVault {
    vault_id: String,
    #[serde(default)]
    created_ms: i64,
    kdf: Value,
    check: String,
}

impl VaultHeader {
    /// 解析并校验 vault.json：
    /// - `wisp_sync` 大于本版本、KDF 算法或参数超出范围 ⇒ `FormatTooNew`（不做任何计算与写入）；
    /// - JSON 不合法、缺字段、vault_id 不是 16 位小写十六进制、check 不是 base64 ⇒ `Corrupt`
    pub fn parse(bytes: &[u8]) -> Result<VaultHeader, SyncError> {
        let value: Value = serde_json::from_slice(bytes).map_err(|_| SyncError::Corrupt)?;
        match value.get("wisp_sync").and_then(Value::as_u64) {
            None | Some(0) => return Err(SyncError::Corrupt),
            Some(v) if v > u64::from(FORMAT_VERSION) => return Err(SyncError::FormatTooNew),
            Some(_) => {}
        }
        let raw: RawVault = serde_json::from_value(value).map_err(|_| SyncError::Corrupt)?;
        let kdf: KdfParams = serde_json::from_value(raw.kdf).map_err(|_| SyncError::FormatTooNew)?;
        kdf.validate()?;
        if !layout::is_valid_id(&raw.vault_id) {
            return Err(SyncError::Corrupt);
        }
        let check = base64::engine::general_purpose::STANDARD
            .decode(raw.check)
            .map_err(|_| SyncError::Corrupt)?;
        Ok(VaultHeader {
            wisp_sync: FORMAT_VERSION,
            vault_id: raw.vault_id,
            created_ms: raw.created_ms,
            kdf,
            check,
        })
    }

    pub fn to_json(&self) -> Result<Vec<u8>, SyncError> {
        serde_json::to_vec_pretty(self).map_err(|_| SyncError::Internal("vault serialization failed"))
    }
}

/// `Vec<u8>` ⇄ 标准 base64 字符串
mod b64 {
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = std::borrow::Cow::<str>::deserialize(d)?;
        base64::engine::general_purpose::STANDARD
            .decode(s.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(d: Domain, k: &str, t: u64, v: Option<Value>) -> Record {
        Record {
            d,
            k: k.into(),
            t: Hlc(t),
            v,
            b: Vec::new(),
        }
    }

    #[test]
    fn tombstone_and_null_value_are_distinct() {
        let tomb = record(Domain::Cfg, "/a", 5, None);
        let null = record(Domain::Cfg, "/a", 5, Some(Value::Null));
        let tomb_json = serde_json::to_value(&tomb).unwrap();
        let null_json = serde_json::to_value(&null).unwrap();
        assert!(tomb_json.get("v").is_none());
        assert_eq!(null_json.get("v"), Some(&Value::Null));
        assert_eq!(serde_json::from_value::<Record>(tomb_json).unwrap(), tomb);
        assert_eq!(serde_json::from_value::<Record>(null_json).unwrap(), null);
        assert!(tomb.is_tombstone() && !null.is_tombstone());
    }

    #[test]
    fn unknown_domain_and_role_round_trip() {
        let mut r = record(Domain::parse("future"), "x", 1, Some(json!(1)));
        r.b.push(BlobRef {
            dev: "d".into(),
            name: "n".into(),
            bytes: 3,
            role: BlobRole::parse("audio"),
        });
        let back: Record = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
        assert_eq!(back.d, Domain::Unknown("future".into()));
        assert!(!back.d.is_known());
        // 缺省 role 是原图
        let b: BlobRef = serde_json::from_value(json!({"dev": "d", "name": "n", "bytes": 1})).unwrap();
        assert_eq!(b.role, BlobRole::Image);
    }

    #[test]
    fn domain_classes_and_order() {
        for d in Domain::KNOWN {
            assert_eq!(Domain::parse(d.as_str()), d);
        }
        assert!(Domain::Key.is_register() && !Domain::Key.is_content());
        assert!(Domain::Clip.is_content() && !Domain::Clip.is_register());
        assert!(Domain::Cfg.apply_rank() < Domain::Sty.apply_rank());
        assert!(Domain::Glo.apply_rank() < Domain::Key.apply_rank());
        assert!(Domain::Key.apply_rank() < Domain::Clip.apply_rank());
        assert!(Domain::Clip.apply_rank() < Domain::Ocr.apply_rank());
    }

    #[test]
    fn lww_ordering() {
        assert!(outranks(Hlc(2), "a", Hlc(1), "z"));
        assert!(outranks(Hlc(1), "b", Hlc(1), "a"));
        assert!(!outranks(Hlc(1), "a", Hlc(1), "a"));
    }

    fn segment(n: usize) -> SegmentPayload {
        SegmentPayload {
            v: FORMAT_VERSION,
            device: "fedcba9876543210".into(),
            w: 3,
            lo: 1,
            hi: 9,
            schema: SchemaVersions {
                settings: 1,
                styles: 1,
                glossary: 1,
            },
            records: (0..n)
                .map(|i| {
                    record(
                        Domain::Glo,
                        &format!("term {i}"),
                        i as u64,
                        Some(json!({"target": "x"})),
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn segment_round_trip_small_and_compressed() {
        let small = segment(1);
        let enc = small.encode().unwrap();
        assert_eq!(enc.flags, 0);
        assert_eq!(SegmentPayload::decode(enc.flags, &enc.bytes).unwrap(), small);

        let big = segment(200);
        let enc = big.encode().unwrap();
        assert_eq!(enc.flags, FLAG_DEFLATE);
        assert_eq!(SegmentPayload::decode(enc.flags, &enc.bytes).unwrap(), big);
    }

    #[test]
    fn head_round_trip_and_max_hi() {
        let head = HeadPayload {
            v: FORMAT_VERSION,
            vault_id: "0123456789abcdef".into(),
            device: DeviceInfo {
                id: "fedcba9876543210".into(),
                name: "n".into(),
                os: OsKind::Linux,
                app_version: "0.2.0".into(),
            },
            updated_ms: 1,
            segments: vec![
                SegmentEntry {
                    w: 1,
                    lo: 1,
                    hi: 4,
                    bytes: 10,
                    hash: "h".into(),
                },
                SegmentEntry {
                    w: 2,
                    lo: 5,
                    hi: 5,
                    bytes: 10,
                    hash: "h".into(),
                },
            ],
        };
        let enc = head.encode().unwrap();
        let back = HeadPayload::decode(enc.flags, &enc.bytes).unwrap();
        assert_eq!(back, head);
        assert_eq!(back.max_hi(), 5);
    }

    #[test]
    fn newer_payloads_are_format_too_new() {
        let v2 = serde_json::to_vec(&json!({"v": 2, "something": "else"})).unwrap();
        assert!(matches!(SegmentPayload::decode(0, &v2), Err(SyncError::FormatTooNew)));
        assert!(matches!(HeadPayload::decode(0, &v2), Err(SyncError::FormatTooNew)));
        let enc = segment(1).encode().unwrap();
        assert!(matches!(
            SegmentPayload::decode(0b10, &enc.bytes),
            Err(SyncError::FormatTooNew)
        ));
        assert!(matches!(
            SegmentPayload::decode(0, b"{\"v\":1}"),
            Err(SyncError::Corrupt)
        ));
        assert!(matches!(
            SegmentPayload::decode(0, b"not json"),
            Err(SyncError::Corrupt)
        ));
        assert!(matches!(
            SegmentPayload::decode(FLAG_DEFLATE, b"\xff\xff\xff"),
            Err(SyncError::Corrupt)
        ));
    }

    #[test]
    fn inflate_is_bounded() {
        let enc = encode_payload(&vec![0u8; 10_000]).unwrap();
        assert_eq!(enc.flags, FLAG_DEFLATE);
        assert!(inflate_limited(enc.flags, &enc.bytes, 100_000).is_ok());
        assert!(matches!(
            inflate_limited(enc.flags, &enc.bytes, 1_000),
            Err(SyncError::Corrupt)
        ));
    }

    fn vault_json(wisp_sync: u64, iterations: u64, salt_len: usize, alg: &str) -> Vec<u8> {
        let salt = base64::engine::general_purpose::STANDARD.encode(vec![7u8; salt_len]);
        serde_json::to_vec(&json!({
            "wisp_sync": wisp_sync,
            "vault_id": "0123456789abcdef",
            "created_ms": 1,
            "kdf": {"alg": alg, "iterations": iterations, "salt": salt},
            "check": "AAAA",
        }))
        .unwrap()
    }

    #[test]
    fn vault_kdf_bounds() {
        let ok = |it, salt| VaultHeader::parse(&vault_json(1, it, salt, KdfParams::ALG));
        assert!(ok(600_000, 16).is_ok());
        assert!(ok(10_000_000, 64).is_ok());
        assert!(matches!(ok(599_999, 16), Err(SyncError::FormatTooNew)));
        assert!(matches!(ok(10_000_001, 16), Err(SyncError::FormatTooNew)));
        assert!(matches!(ok(1 << 40, 16), Err(SyncError::FormatTooNew)));
        assert!(matches!(ok(600_000, 15), Err(SyncError::FormatTooNew)));
        assert!(matches!(ok(600_000, 65), Err(SyncError::FormatTooNew)));
        assert!(matches!(
            VaultHeader::parse(&vault_json(1, 600_000, 16, "argon2id")),
            Err(SyncError::FormatTooNew)
        ));
        assert!(matches!(
            VaultHeader::parse(&vault_json(2, 600_000, 16, KdfParams::ALG)),
            Err(SyncError::FormatTooNew)
        ));
    }

    #[test]
    fn vault_round_trip_and_corruption() {
        let header = VaultHeader::parse(&vault_json(1, 600_000, 16, KdfParams::ALG)).unwrap();
        assert_eq!(header.check, vec![0, 0, 0]);
        let again = VaultHeader::parse(&header.to_json().unwrap()).unwrap();
        assert_eq!(again, header);

        assert!(matches!(VaultHeader::parse(b"{}"), Err(SyncError::Corrupt)));
        assert!(matches!(VaultHeader::parse(b"<html>"), Err(SyncError::Corrupt)));
        let mut bad_id: Value = serde_json::from_slice(&vault_json(1, 600_000, 16, KdfParams::ALG)).unwrap();
        bad_id["vault_id"] = json!("../../x");
        assert!(matches!(
            VaultHeader::parse(&serde_json::to_vec(&bad_id).unwrap()),
            Err(SyncError::Corrupt)
        ));
    }

    #[test]
    fn clip_and_ocr_keys() {
        let h = [0xabu8; 32];
        let c = keys::clip_content(&h);
        assert_eq!(c.len(), 66);
        assert_eq!(keys::parse_clip(&c), Some(keys::ClipKey::Content(h)));
        assert_eq!(keys::parse_clip(&keys::clip_pin(&h)), Some(keys::ClipKey::Pin(h)));
        assert_eq!(keys::parse_clip(keys::CLIP_CLEAR_ALL), Some(keys::ClipKey::ClearAll));
        assert_eq!(
            keys::parse_clip(keys::CLIP_CLEAR_UNPINNED),
            Some(keys::ClipKey::ClearUnpinned)
        );
        assert_eq!(keys::parse_clip(&c.to_uppercase()), None);
        assert_eq!(keys::parse_clip("c/zz"), None);
        let id = "0123456789abcdef0123456789abcdef";
        assert_eq!(keys::parse_ocr(&keys::ocr_item(id)), Some(keys::OcrKey::Item(id)));
        assert_eq!(keys::parse_ocr(keys::OCR_CLEAR), Some(keys::OcrKey::Clear));
        assert_eq!(keys::parse_ocr("o/short"), None);
        assert_eq!(parse_hex32(&hex(&h)), Some(h));
        assert_eq!(segment_hash(b"abc").len(), 32);
    }
}
