//! 端到端加密：服务器只见密文。
//!
//! - 主密钥：`PBKDF2-HMAC-SHA256(NFC(口令), salt, iterations) → 32 字节`，只在连接时算一次
//!   （几百毫秒到数秒的 CPU，调用方放进 `spawn_blocking`）；之后由桌面端把主密钥存进钥匙串，不保存口令。
//! - 子密钥（HKDF-SHA256，salt = vault_id）：`wisp-sync/v1/data`（AEAD）与 `wisp-sync/v1/name`（blob 命名）。
//! - 小文件（head、日志段）：`b"WSY1" ‖ kind ‖ flags`（6 字节）后接 `nonce(12) ‖ 密文‖tag`，
//!   ChaCha20-Poly1305、随机 nonce；AAD = 头 ‖ vault_id ‖ 逻辑名（见 [`crate::layout`]），
//!   所以改头、改 flags、换文件名、拿别的 vault 的文件来都会解密失败。
//! - blob：分块流式封装，块大小 64 KiB：头（6）‖ nonce 前缀（7），之后每块 `密文‖tag`；
//!   第 i 块的 nonce = 前缀 ‖ u32be(i) ‖ last(u8)，AAD = 头 ‖ vault_id ‖ 逻辑名。
//!   读取时最后一块必须带 last 标记，防止截断；读写都按块处理，峰值内存约两个块。
//! - 口令校验：用 data 子密钥封装固定明文 `wisp-sync-check`，AAD = `b"WSY1check" ‖ vault_id`。

use std::borrow::Cow;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::num::NonZeroU32;
use std::path::Path;

use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use ring::{hkdf, hmac, pbkdf2};

use crate::error::{PassphraseIssue, SyncError};
use crate::model::hex;

pub use crate::model::KdfParams;

/// 密文文件头的魔数
pub const MAGIC: &[u8; 4] = b"WSY1";
/// 密文文件头长度：魔数 ‖ kind ‖ flags
pub const HEADER_LEN: usize = 6;
/// AEAD nonce 长度
pub const NONCE_LEN: usize = 12;
/// AEAD 认证标签长度
pub const TAG_LEN: usize = 16;
/// blob 分块封装的块大小（明文）
pub const BLOB_CHUNK_BYTES: usize = 64 * 1024;
/// blob 的 nonce 前缀长度（其余 5 字节是块序号与 last 标记）
pub const BLOB_NONCE_PREFIX_LEN: usize = 7;
/// 口令校验封装的固定明文
pub const CHECK_PLAINTEXT: &[u8] = b"wisp-sync-check";
/// 口令校验的 AAD 前缀（之后接 vault_id）
pub const CHECK_AAD_PREFIX: &[u8] = b"WSY1check";
/// 同步口令的最少字符数（按 Unicode 标量计）
pub const MIN_PASSPHRASE_CHARS: usize = 12;
/// HKDF 的 info：数据子密钥
pub const INFO_DATA: &[u8] = b"wisp-sync/v1/data";
/// HKDF 的 info：命名子密钥
pub const INFO_NAME: &[u8] = b"wisp-sync/v1/name";

/// 密文文件的种类（头部第 5 字节）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FileKind {
    Head = 1,
    Segment = 2,
    Blob = 3,
}

impl FileKind {
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// 32 字节主密钥。释放时尽力清零（`ring` 内部派生出的密钥对象无法从外部清零，这里只管自己持有的副本）
#[derive(Clone)]
pub struct MasterKey([u8; 32]);

impl MasterKey {
    pub const LEN: usize = 32;

    pub fn from_bytes(bytes: [u8; 32]) -> MasterKey {
        MasterKey(bytes)
    }

    /// 从钥匙串读出的字节恢复；长度不对 ⇒ `Corrupt`
    pub fn from_slice(bytes: &[u8]) -> Result<MasterKey, SyncError> {
        <[u8; 32]>::try_from(bytes)
            .map(MasterKey)
            .map_err(|_| SyncError::Corrupt)
    }

    /// 原始字节（只用于写入钥匙串与派生子密钥）
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Drop for MasterKey {
    fn drop(&mut self) {
        self.0.fill(0);
        // 让编译器认为清零后的内容仍会被读取，避免把上面的写入当成死存储优化掉
        std::hint::black_box(&self.0);
    }
}

impl fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MasterKey(..)")
    }
}

/// 由主密钥与 vault_id 派生出的子密钥
pub struct Keys {
    /// ChaCha20-Poly1305 密钥（小文件、blob、口令校验）
    data: LessSafeKey,
    /// HMAC-SHA256 密钥（blob 命名）
    name: hmac::Key,
}

impl Keys {
    /// HKDF-SHA256（salt = vault_id 的 ASCII 字节）派生 data 与 name 两个子密钥。
    /// 同一口令在不同 vault（重建后的新 vault）下得到互不相关的子密钥
    pub fn derive(master: &MasterKey, vault_id: &str) -> Result<Keys, SyncError> {
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, vault_id.as_bytes()).extract(master.as_bytes());
        let data = prk
            .expand(&[INFO_DATA], &aead::CHACHA20_POLY1305)
            .map_err(|_| SyncError::Internal("key derivation failed"))?;
        let name = prk
            .expand(&[INFO_NAME], hmac::HMAC_SHA256)
            .map_err(|_| SyncError::Internal("key derivation failed"))?;
        // 直接从 HKDF 输出构造 ring 的密钥对象，子密钥的原始字节不经过本模块的缓冲区
        Ok(Keys {
            data: LessSafeKey::new(UnboundKey::from(data)),
            name: hmac::Key::from(name),
        })
    }
}

impl fmt::Debug for Keys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Keys(..)")
    }
}

/// 用系统安全随机数填满 `buf`
fn fill_random(buf: &mut [u8]) -> Result<(), SyncError> {
    SystemRandom::new()
        .fill(buf)
        .map_err(|_| SyncError::Internal("secure random source unavailable"))
}

/// NFC 规范化：同一口令在不同系统 / 输入法下可能以组合或分解形式输入，规范化后才得到同一把密钥
fn nfc(s: &str) -> Cow<'_, str> {
    icu_normalizer::ComposingNormalizerBorrowed::new_nfc().normalize(s)
}

/// 新建 vault 用的 KDF 参数：随机 16 字节盐、默认迭代次数
pub fn new_kdf_params() -> Result<KdfParams, SyncError> {
    let mut salt = vec![0u8; KdfParams::DEFAULT_SALT_BYTES];
    fill_random(&mut salt)?;
    Ok(KdfParams {
        alg: KdfParams::ALG.to_string(),
        iterations: KdfParams::DEFAULT_ITERATIONS,
        salt,
    })
}

/// PBKDF2 派生主密钥（口令先做 NFC 规范化）。参数先经 [`KdfParams::validate`]，超出范围 ⇒ `FormatTooNew`。
/// CPU 密集且阻塞，调用方放进 `spawn_blocking`
pub fn derive_master(passphrase: &str, kdf: &KdfParams) -> Result<MasterKey, SyncError> {
    kdf.validate()?;
    let iterations = NonZeroU32::new(kdf.iterations).ok_or(SyncError::FormatTooNew)?;
    let normalized = nfc(passphrase);
    let mut out = [0u8; MasterKey::LEN];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        iterations,
        &kdf.salt,
        normalized.as_bytes(),
        &mut out,
    );
    let key = MasterKey::from_bytes(out);
    out.fill(0);
    std::hint::black_box(&out);
    // 规范化时复制出的口令副本尽力清零（借用的是调用方自己的字符串，由调用方负责）
    if let Cow::Owned(copy) = normalized {
        let mut bytes = copy.into_bytes();
        bytes.fill(0);
        std::hint::black_box(&bytes);
    }
    Ok(key)
}

/// 小文件的密文头：魔数 ‖ kind ‖ flags
fn file_header(kind: FileKind, flags: u8) -> [u8; HEADER_LEN] {
    let mut header = [0u8; HEADER_LEN];
    header[..4].copy_from_slice(MAGIC);
    header[4] = kind.as_u8();
    header[5] = flags;
    header
}

/// AAD = 头 ‖ vault_id ‖ 逻辑名。vault_id 定长（16 位十六进制），拼接不会产生歧义
fn file_aad(header: &[u8], vault_id: &str, name: &str) -> Vec<u8> {
    let mut aad = Vec::with_capacity(header.len() + vault_id.len() + name.len());
    aad.extend_from_slice(header);
    aad.extend_from_slice(vault_id.as_bytes());
    aad.extend_from_slice(name.as_bytes());
    aad
}

/// 封装小文件（head / 日志段）：返回 `头 ‖ nonce ‖ 密文‖tag`。
/// `flags` 原样写进头部（见 [`crate::model::FLAG_DEFLATE`]）；`name` 是逻辑名（`head/<dev>`、`seg/<dev>/<w>`）
pub fn seal_small(
    keys: &Keys,
    kind: FileKind,
    flags: u8,
    vault_id: &str,
    name: &str,
    plaintext: &[u8],
) -> Result<Vec<u8>, SyncError> {
    let mut nonce = [0u8; NONCE_LEN];
    fill_random(&mut nonce)?;
    seal_small_with_nonce(keys, kind, flags, vault_id, name, plaintext, nonce)
}

fn seal_small_with_nonce(
    keys: &Keys,
    kind: FileKind,
    flags: u8,
    vault_id: &str,
    name: &str,
    plaintext: &[u8],
    nonce: [u8; NONCE_LEN],
) -> Result<Vec<u8>, SyncError> {
    if kind == FileKind::Blob {
        return Err(SyncError::Internal("blobs use the chunked format"));
    }
    let header = file_header(kind, flags);
    let aad = file_aad(&header, vault_id, name);
    let mut out = Vec::with_capacity(HEADER_LEN + NONCE_LEN + plaintext.len() + TAG_LEN);
    out.extend_from_slice(&header);
    out.extend_from_slice(&nonce);
    let body = out.len();
    out.extend_from_slice(plaintext);
    let tag = keys
        .data
        .seal_in_place_separate_tag(Nonce::assume_unique_for_key(nonce), Aad::from(&aad), &mut out[body..])
        .map_err(|_| SyncError::Internal("encryption failed"))?;
    out.extend_from_slice(tag.as_ref());
    Ok(out)
}

/// [`open_small`] 的结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    /// 头部的 flags（交给 [`crate::model::inflate`] / 各载荷的 `decode`）
    pub flags: u8,
    pub plaintext: Vec<u8>,
}

/// 解封小文件。长度不足、魔数或 kind 不符、认证失败 ⇒ `Corrupt`
pub fn open_small(keys: &Keys, kind: FileKind, vault_id: &str, name: &str, sealed: &[u8]) -> Result<Opened, SyncError> {
    if kind == FileKind::Blob {
        return Err(SyncError::Internal("blobs use the chunked format"));
    }
    if sealed.len() < HEADER_LEN + NONCE_LEN + TAG_LEN {
        return Err(SyncError::Corrupt);
    }
    let (header, rest) = sealed.split_at(HEADER_LEN);
    if &header[..4] != MAGIC || header[4] != kind.as_u8() {
        return Err(SyncError::Corrupt);
    }
    let (nonce, ciphertext) = rest.split_at(NONCE_LEN);
    let nonce = Nonce::try_assume_unique_for_key(nonce).map_err(|_| SyncError::Corrupt)?;
    let aad = file_aad(header, vault_id, name);
    let mut buf = ciphertext.to_vec();
    let len = keys
        .data
        .open_in_place(nonce, Aad::from(&aad), &mut buf)
        .map_err(|_| SyncError::Corrupt)?
        .len();
    buf.truncate(len);
    Ok(Opened {
        flags: header[5],
        plaintext: buf,
    })
}

/// 明文长度为 `plain_len` 的 blob 封装后的字节数（上传时作为 Content-Length）
pub fn sealed_blob_len(plain_len: u64) -> u64 {
    let chunk = BLOB_CHUNK_BYTES as u64;
    // 空文件也有一个带 last 标记的空块
    let chunks = plain_len.div_ceil(chunk).max(1);
    (HEADER_LEN + BLOB_NONCE_PREFIX_LEN) as u64 + plain_len + chunks * TAG_LEN as u64
}

/// blob 头部 flags 目前没有定义任何位；读到非零值说明来自更新的格式，按损坏处理（只影响这一个 blob）
const BLOB_FLAGS: u8 = 0;
/// blob 文件开头的固定部分：头（6）‖ nonce 前缀（7）
const BLOB_PRELUDE_LEN: usize = HEADER_LEN + BLOB_NONCE_PREFIX_LEN;
/// 一个封装后的完整块
const SEALED_CHUNK_BYTES: usize = BLOB_CHUNK_BYTES + TAG_LEN;

/// 分块封装的状态：nonce 前缀、AAD 与下一块的序号
struct ChunkCipher<'k> {
    key: &'k LessSafeKey,
    prefix: [u8; BLOB_NONCE_PREFIX_LEN],
    aad: Vec<u8>,
    index: u32,
}

impl ChunkCipher<'_> {
    /// 第 i 块的 nonce = 前缀 ‖ u32be(i) ‖ last：块序号防重排，last 标记防截断
    fn nonce(&self, last: bool) -> Nonce {
        let mut nonce = [0u8; NONCE_LEN];
        nonce[..BLOB_NONCE_PREFIX_LEN].copy_from_slice(&self.prefix);
        nonce[BLOB_NONCE_PREFIX_LEN..NONCE_LEN - 1].copy_from_slice(&self.index.to_be_bytes());
        nonce[NONCE_LEN - 1] = u8::from(last);
        Nonce::assume_unique_for_key(nonce)
    }

    fn advance(&mut self) -> Result<(), SyncError> {
        self.index = self
            .index
            .checked_add(1)
            .ok_or(SyncError::Internal("blob has too many chunks"))?;
        Ok(())
    }
}

/// 读满 `want` 字节或读到文件末尾，返回读到的字节数（短于 `want` 即已到末尾）
fn read_up_to(src: &mut impl Read, buf: &mut Vec<u8>, want: usize) -> io::Result<usize> {
    buf.clear();
    buf.resize(want, 0);
    let mut filled = 0;
    while filled < want {
        match src.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    buf.truncate(filled);
    Ok(filled)
}

/// 按流分块封装：从 `src` 读明文，把 `头 ‖ nonce 前缀 ‖ 各块密文‖tag` 写到 `dest`，返回写入的字节数。
/// 预读下一块来判断当前块是不是最后一块；长度恰为块大小整数倍时，最后一个满块带 last 标记，
/// 空输入写一个带 last 标记的空块。峰值内存约两个块
pub fn seal_blob_stream(
    keys: &Keys,
    vault_id: &str,
    name: &str,
    src: impl Read,
    dest: impl Write,
) -> Result<u64, SyncError> {
    let mut prefix = [0u8; BLOB_NONCE_PREFIX_LEN];
    fill_random(&mut prefix)?;
    seal_blob_stream_with_prefix(keys, vault_id, name, src, dest, prefix)
}

fn seal_blob_stream_with_prefix(
    keys: &Keys,
    vault_id: &str,
    name: &str,
    mut src: impl Read,
    mut dest: impl Write,
    prefix: [u8; BLOB_NONCE_PREFIX_LEN],
) -> Result<u64, SyncError> {
    let header = file_header(FileKind::Blob, BLOB_FLAGS);
    let mut cipher = ChunkCipher {
        key: &keys.data,
        prefix,
        aad: file_aad(&header, vault_id, name),
        index: 0,
    };
    dest.write_all(&header)?;
    dest.write_all(&prefix)?;
    let mut written = BLOB_PRELUDE_LEN as u64;

    let mut current = Vec::with_capacity(SEALED_CHUNK_BYTES);
    let mut next = Vec::with_capacity(SEALED_CHUNK_BYTES);
    read_up_to(&mut src, &mut current, BLOB_CHUNK_BYTES)?;
    loop {
        let last = read_up_to(&mut src, &mut next, BLOB_CHUNK_BYTES)? == 0;
        let tag = cipher
            .key
            .seal_in_place_separate_tag(cipher.nonce(last), Aad::from(&cipher.aad), &mut current)
            .map_err(|_| SyncError::Internal("encryption failed"))?;
        current.extend_from_slice(tag.as_ref());
        dest.write_all(&current)?;
        written += current.len() as u64;
        if last {
            break;
        }
        std::mem::swap(&mut current, &mut next);
        cipher.advance()?;
    }
    dest.flush()?;
    Ok(written)
}

/// 按流逐块解密：从 `src` 读 [`seal_blob_stream`] 的输出，把明文写到 `dest`，返回明文字节数。
/// 头部不符、任何一块认证失败、块被重排、缺少 last 标记（被截断）或 last 之后还有数据 ⇒ `Corrupt`；
/// 出错时已写进 `dest` 的部分明文不可用
pub fn open_blob_stream(
    keys: &Keys,
    vault_id: &str,
    name: &str,
    mut src: impl Read,
    mut dest: impl Write,
) -> Result<u64, SyncError> {
    let mut prelude = Vec::with_capacity(BLOB_PRELUDE_LEN);
    if read_up_to(&mut src, &mut prelude, BLOB_PRELUDE_LEN)? < BLOB_PRELUDE_LEN {
        return Err(SyncError::Corrupt);
    }
    let (header, prefix) = prelude.split_at(HEADER_LEN);
    if &header[..4] != MAGIC || header[4] != FileKind::Blob.as_u8() || header[5] != BLOB_FLAGS {
        return Err(SyncError::Corrupt);
    }
    let mut cipher = ChunkCipher {
        key: &keys.data,
        prefix: prefix.try_into().map_err(|_| SyncError::Corrupt)?,
        aad: file_aad(header, vault_id, name),
        index: 0,
    };

    let mut total = 0u64;
    let mut current = Vec::with_capacity(SEALED_CHUNK_BYTES);
    let mut next = Vec::with_capacity(SEALED_CHUNK_BYTES);
    read_up_to(&mut src, &mut current, SEALED_CHUNK_BYTES)?;
    loop {
        // 是否最后一块只由「后面还有没有数据」决定：截断在块边界上的文件会让一个不带 last 标记的块
        // 被当成最后一块，追加的数据会让真正的最后一块被当成中间块——两种情况 nonce 都对不上，认证失败
        let last = read_up_to(&mut src, &mut next, SEALED_CHUNK_BYTES)? == 0;
        let plain = cipher
            .key
            .open_in_place(cipher.nonce(last), Aad::from(&cipher.aad), &mut current)
            .map_err(|_| SyncError::Corrupt)?;
        dest.write_all(plain)?;
        total += plain.len() as u64;
        if last {
            break;
        }
        std::mem::swap(&mut current, &mut next);
        cipher.advance().map_err(|_| SyncError::Corrupt)?;
    }
    dest.flush()?;
    Ok(total)
}

/// 把本地明文文件 `src` 分块封装写到 `dest`（`name` 是逻辑名 `blobs/<dev>/<name>`），返回写入的字节数。
/// 失败时尽力删除 `dest`。阻塞 I/O，调用方放进 `spawn_blocking`
pub fn seal_blob_file(keys: &Keys, vault_id: &str, name: &str, src: &Path, dest: &Path) -> Result<u64, SyncError> {
    let input = File::open(src)?;
    let result = File::create(dest)
        .map_err(SyncError::from)
        .and_then(|output| seal_blob_stream(keys, vault_id, name, input, output));
    if result.is_err() {
        let _ = std::fs::remove_file(dest);
    }
    result
}

/// 把下载的密文文件 `src` 逐块解密写到 `dest`，返回明文字节数。
/// 任何一块认证失败、块序号不连续、缺少 last 标记（被截断）或 last 之后还有数据 ⇒ `Corrupt`；
/// 失败时尽力删除 `dest`（其中的部分明文不可用）。阻塞 I/O，调用方放进 `spawn_blocking`
pub fn open_blob_file(keys: &Keys, vault_id: &str, name: &str, src: &Path, dest: &Path) -> Result<u64, SyncError> {
    let input = File::open(src)?;
    let result = File::create(dest)
        .map_err(SyncError::from)
        .and_then(|output| open_blob_stream(keys, vault_id, name, input, output));
    if result.is_err() {
        let _ = std::fs::remove_file(dest);
    }
    result
}

fn check_aad(vault_id: &str) -> Vec<u8> {
    let mut aad = Vec::with_capacity(CHECK_AAD_PREFIX.len() + vault_id.len());
    aad.extend_from_slice(CHECK_AAD_PREFIX);
    aad.extend_from_slice(vault_id.as_bytes());
    aad
}

/// vault.json 的口令校验字段：`nonce ‖ 密文‖tag`
pub fn make_check(keys: &Keys, vault_id: &str) -> Result<Vec<u8>, SyncError> {
    let mut nonce = [0u8; NONCE_LEN];
    fill_random(&mut nonce)?;
    make_check_with_nonce(keys, vault_id, nonce)
}

fn make_check_with_nonce(keys: &Keys, vault_id: &str, nonce: [u8; NONCE_LEN]) -> Result<Vec<u8>, SyncError> {
    let mut out = Vec::with_capacity(NONCE_LEN + CHECK_PLAINTEXT.len() + TAG_LEN);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(CHECK_PLAINTEXT);
    let tag = keys
        .data
        .seal_in_place_separate_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(check_aad(vault_id)),
            &mut out[NONCE_LEN..],
        )
        .map_err(|_| SyncError::Internal("encryption failed"))?;
    out.extend_from_slice(tag.as_ref());
    Ok(out)
}

/// 校验口令：解不开或明文不符 ⇒ `Passphrase`
pub fn verify_check(keys: &Keys, vault_id: &str, check: &[u8]) -> Result<(), SyncError> {
    if check.len() < NONCE_LEN + TAG_LEN {
        return Err(SyncError::Passphrase);
    }
    let (nonce, ciphertext) = check.split_at(NONCE_LEN);
    let nonce = Nonce::try_assume_unique_for_key(nonce).map_err(|_| SyncError::Passphrase)?;
    let mut buf = ciphertext.to_vec();
    let plain = keys
        .data
        .open_in_place(nonce, Aad::from(check_aad(vault_id)), &mut buf)
        .map_err(|_| SyncError::Passphrase)?;
    if plain == CHECK_PLAINTEXT {
        Ok(())
    } else {
        Err(SyncError::Passphrase)
    }
}

/// blob 名：`hex(HMAC(name_key, 内容身份))` 的前 32 个字符。剪贴板图片的内容身份是条目 hash。
/// 服务器看不出两台设备是否上传了同一张图，也无法由名字反推内容
pub fn blob_name(keys: &Keys, identity: &[u8]) -> String {
    let tag = hmac::sign(&keys.name, identity);
    let mut name = hex(tag.as_ref());
    name.truncate(32);
    name
}

/// 16 位小写十六进制随机 id（vault_id、device_id）
pub fn random_id() -> Result<String, SyncError> {
    let mut bytes = [0u8; 8];
    fill_random(&mut bytes)?;
    Ok(hex(&bytes))
}

/// RFC 4648 base32 字母表：不含 0 / 1 / 8 / 9，避免与 O / I / B 混淆
const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
/// 生成口令的字符数（每个 5 bit，共 120 bit）
const GENERATED_CHARS: usize = 24;
const GENERATED_GROUP: usize = 4;

/// 生成随机同步口令：24 个 base32 字符（120 bit 熵），每 4 个一组用 `-` 连接
pub fn generate_passphrase() -> Result<String, SyncError> {
    // 15 字节正好 120 bit，切成 24 个 5 bit，每个字符均匀分布
    let mut bytes = [0u8; GENERATED_CHARS * 5 / 8];
    fill_random(&mut bytes)?;
    let mut out = String::with_capacity(GENERATED_CHARS + GENERATED_CHARS / GENERATED_GROUP);
    let mut acc = 0u32;
    let mut bits = 0u32;
    let mut count = 0usize;
    for byte in bytes {
        acc = (acc << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            if count > 0 && count.is_multiple_of(GENERATED_GROUP) {
                out.push('-');
            }
            out.push(char::from(BASE32[((acc >> bits) & 0x1F) as usize]));
            count += 1;
        }
        acc &= (1 << bits) - 1;
    }
    bytes.fill(0);
    std::hint::black_box(&bytes);
    Ok(out)
}

/// 比较用的折叠形式：NFC、去掉首尾空白、小写
fn fold(s: &str) -> String {
    nfc(s.trim()).to_lowercase()
}

/// 检查新设置的同步口令：去掉首尾空白后至少 [`MIN_PASSPHRASE_CHARS`] 个字符（NFC 后按 Unicode 标量计）；
/// 不得与 `forbidden` 中任何一个相同（忽略大小写与首尾空白）。
/// `forbidden` 传 WebDAV 密码、账号与服务器主机名；空字符串忽略
pub fn check_passphrase(passphrase: &str, forbidden: &[&str]) -> Result<(), PassphraseIssue> {
    if nfc(passphrase.trim()).chars().count() < MIN_PASSPHRASE_CHARS {
        return Err(PassphraseIssue::TooShort);
    }
    let folded = fold(passphrase);
    let reused = forbidden
        .iter()
        .map(|f| f.trim())
        .filter(|f| !f.is_empty())
        .any(|f| fold(f) == folded);
    if reused { Err(PassphraseIssue::Reused) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_blob_len_counts_tags_per_chunk() {
        let fixed = (HEADER_LEN + BLOB_NONCE_PREFIX_LEN) as u64;
        assert_eq!(sealed_blob_len(0), fixed + 16);
        assert_eq!(sealed_blob_len(1), fixed + 1 + 16);
        assert_eq!(sealed_blob_len(65_536), fixed + 65_536 + 16);
        assert_eq!(sealed_blob_len(65_537), fixed + 65_537 + 32);
    }

    // 以下向量由独立的参考实现（标准库 PBKDF2 / HKDF / HMAC 与按 RFC 8439 手写的 ChaCha20-Poly1305）算出，
    // 同时冻结了文件格式：头部布局、AAD 的拼接顺序与 blob 的 nonce 构造，改动任何一处都会让这里失败
    const V: &str = "0123456789abcdef";
    const D: &str = "fedcba9876543210";

    fn hex_decode(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn fixed_keys() -> Keys {
        Keys::derive(&MasterKey::from_bytes([0x42; 32]), V).unwrap()
    }

    fn kdf(salt: Vec<u8>) -> KdfParams {
        KdfParams {
            alg: KdfParams::ALG.into(),
            iterations: KdfParams::DEFAULT_ITERATIONS,
            salt,
        }
    }

    #[test]
    fn pbkdf2_vector_and_nfc() {
        let salt: Vec<u8> = (0..16).collect();
        let key = derive_master("correct horse battery staple", &kdf(salt.clone())).unwrap();
        assert_eq!(
            hex(key.as_bytes()),
            "ef177144eec9420cbc1093d2a8b344a92bc506d0d4ec9c028dd19f8324d8c1e6"
        );
        // 组合形式与分解形式的同一口令得到同一把密钥
        let want = "4403d248917d4446f12cecaf81dbb1009a67a5d27deddaf234c3acf7b3812225";
        let composed = derive_master("caf\u{e9} au lait \u{2713}", &kdf(salt.clone())).unwrap();
        let decomposed = derive_master("cafe\u{301} au lait \u{2713}", &kdf(salt)).unwrap();
        assert_eq!(hex(composed.as_bytes()), want);
        assert_eq!(hex(decomposed.as_bytes()), want);
    }

    #[test]
    fn hkdf_and_blob_name_vector() {
        assert_eq!(
            blob_name(&fixed_keys(), b"identity"),
            "848e7a2da6246cdcd6c79622efb11efb"
        );
    }

    #[test]
    fn small_file_vector() {
        let keys = fixed_keys();
        let name = format!("head/{D}");
        let sealed = seal_small_with_nonce(&keys, FileKind::Head, 1, V, &name, b"hello wisp", [7; NONCE_LEN]).unwrap();
        assert_eq!(
            hex(&sealed),
            "5753593101010707070707070707070707072157dcbeac2b2e17cd1f20714b9f8c45ac9cfae06404c8e2ff36"
        );
        let opened = open_small(&keys, FileKind::Head, V, &name, &sealed).unwrap();
        assert_eq!((opened.flags, opened.plaintext.as_slice()), (1, &b"hello wisp"[..]));
    }

    #[test]
    fn check_vector() {
        let keys = fixed_keys();
        let check = make_check_with_nonce(&keys, V, [9; NONCE_LEN]).unwrap();
        assert_eq!(
            hex(&check),
            "09090909090909090909090957c38a97eba2b19a68ff304b4b2ad8681ba1faf1e63a3bf8ffc6b293301a01"
        );
        verify_check(&keys, V, &check).unwrap();
    }

    #[test]
    fn blob_vectors() {
        let keys = fixed_keys();
        let name = format!("blobs/{D}/{}", "ab".repeat(16));
        let plain: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
        let mut sealed = Vec::new();
        let n = seal_blob_stream_with_prefix(&keys, V, &name, plain.as_slice(), &mut sealed, [5; 7]).unwrap();
        assert_eq!(n, 70_045);
        assert_eq!(n, sealed_blob_len(70_000));
        assert_eq!(
            hex(ring::digest::digest(&ring::digest::SHA256, &sealed).as_ref()),
            "da78b13f51cc54c45312964e4107abf8b3a689c6f3240135fbcd2022c5024f22"
        );
        let mut out = Vec::new();
        assert_eq!(
            open_blob_stream(&keys, V, &name, sealed.as_slice(), &mut out).unwrap(),
            70_000
        );
        assert_eq!(out, plain);

        let mut empty = Vec::new();
        seal_blob_stream_with_prefix(&keys, V, &name, &b""[..], &mut empty, [5; 7]).unwrap();
        assert_eq!(
            empty,
            hex_decode("57535931030005050505050505c3cf07bc7a35d7e269fde5d5353f946d")
        );
    }

    #[test]
    fn generated_passphrase_alphabet_is_uniform_base32() {
        let p = generate_passphrase().unwrap();
        assert_eq!(p.len(), 29);
        let groups: Vec<&str> = p.split('-').collect();
        assert_eq!(groups.len(), 6);
        assert!(
            groups
                .iter()
                .all(|g| g.len() == 4 && g.bytes().all(|b| BASE32.contains(&b)))
        );
    }

    #[test]
    fn master_key_debug_is_redacted() {
        let key = MasterKey::from_bytes([9; 32]);
        assert_eq!(format!("{key:?}"), "MasterKey(..)");
        assert!(MasterKey::from_slice(&[1; 31]).is_err());
        assert_eq!(MasterKey::from_slice(&[1; 32]).unwrap().as_bytes(), &[1; 32]);
    }
}
