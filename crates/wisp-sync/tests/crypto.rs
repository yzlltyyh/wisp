//! 加密层的对外行为：往返、篡改检测（头部、AAD、密文、nonce）、换名、跨 vault、blob 截断与重排、
//! KDF 参数边界、口令规则与生成。

use std::io;

use proptest::prelude::*;
use wisp_sync::crypto::{
    self, BLOB_CHUNK_BYTES, FileKind, HEADER_LEN, KdfParams, MasterKey, NONCE_LEN, TAG_LEN, check_passphrase,
    derive_master, generate_passphrase, make_check, new_kdf_params, open_blob_file, open_blob_stream, open_small,
    random_id, seal_blob_file, seal_blob_stream, seal_small, sealed_blob_len, verify_check,
};
use wisp_sync::layout;
use wisp_sync::{Keys, PassphraseIssue, SyncError};

const V: &str = "0123456789abcdef";
const V2: &str = "1111111111111111";
const D: &str = "fedcba9876543210";
const CHUNK_SEALED: usize = BLOB_CHUNK_BYTES + TAG_LEN;
const PRELUDE: usize = HEADER_LEN + crypto::BLOB_NONCE_PREFIX_LEN;

fn keys(seed: u8, vault: &str) -> Keys {
    Keys::derive(&MasterKey::from_bytes([seed; 32]), vault).expect("derive keys")
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}

fn seal_blob(k: &Keys, vault: &str, name: &str, plain: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let n = seal_blob_stream(k, vault, name, plain, &mut out).expect("seal blob");
    assert_eq!(n, out.len() as u64);
    out
}

fn open_blob(k: &Keys, vault: &str, name: &str, sealed: &[u8]) -> Result<Vec<u8>, SyncError> {
    let mut out = Vec::new();
    let n = open_blob_stream(k, vault, name, sealed, &mut out)?;
    assert_eq!(n, out.len() as u64);
    Ok(out)
}

fn is_corrupt<T: std::fmt::Debug>(r: Result<T, SyncError>) -> bool {
    matches!(r, Err(SyncError::Corrupt))
}

// ---------------------------------------------------------------------------------------------
// 小文件

#[test]
fn small_files_round_trip_and_keep_flags() {
    let k = keys(1, V);
    let head = layout::head_aad_name(D);
    let seg = layout::segment_aad_name(D, 42);
    for (kind, name, flags, plain) in [
        (FileKind::Head, head.as_str(), 0u8, b"{}".to_vec()),
        (FileKind::Segment, seg.as_str(), 1u8, pattern(100_000)),
        (FileKind::Segment, seg.as_str(), 0u8, Vec::new()),
    ] {
        let sealed = seal_small(&k, kind, flags, V, name, &plain).unwrap();
        assert_eq!(sealed.len(), HEADER_LEN + NONCE_LEN + plain.len() + TAG_LEN);
        assert_eq!(&sealed[..4], crypto::MAGIC);
        let opened = open_small(&k, kind, V, name, &sealed).unwrap();
        assert_eq!(opened.flags, flags);
        assert_eq!(opened.plaintext, plain);
    }
}

#[test]
fn small_files_use_fresh_nonces() {
    let k = keys(1, V);
    let name = layout::head_aad_name(D);
    let a = seal_small(&k, FileKind::Head, 0, V, &name, b"same").unwrap();
    let b = seal_small(&k, FileKind::Head, 0, V, &name, b"same").unwrap();
    assert_ne!(a, b);
}

#[test]
fn small_file_tampering_is_detected() {
    let k = keys(1, V);
    let name = layout::segment_aad_name(D, 7);
    let sealed = seal_small(&k, FileKind::Segment, 1, V, &name, &pattern(300)).unwrap();
    let open = |bytes: &[u8]| open_small(&k, FileKind::Segment, V, &name, bytes);
    assert!(open(&sealed).is_ok());

    // 头部每个字节（魔数、kind、flags）、nonce、密文与 tag 的任意一位被改都失败
    for index in [
        0,
        3,
        4,
        5,
        HEADER_LEN,
        HEADER_LEN + NONCE_LEN - 1,
        HEADER_LEN + NONCE_LEN,
        sealed.len() - 1,
    ] {
        let mut bad = sealed.clone();
        bad[index] ^= 0x01;
        assert!(is_corrupt(open(&bad)), "byte {index}");
    }
    // 只翻 flags 的 deflate 位：flags 在 AAD 里，不能被悄悄改成另一种解析方式
    let mut flipped = sealed.clone();
    flipped[5] = 0;
    assert!(is_corrupt(open(&flipped)));
    // 截断与过短
    assert!(is_corrupt(open(&sealed[..sealed.len() - 1])));
    assert!(is_corrupt(open(&sealed[..HEADER_LEN + NONCE_LEN + TAG_LEN - 1])));
    assert!(is_corrupt(open(&[])));
    // 追加数据
    let mut longer = sealed.clone();
    longer.push(0);
    assert!(is_corrupt(open(&longer)));
}

#[test]
fn small_files_are_bound_to_their_place() {
    let k = keys(1, V);
    let seg1 = layout::segment_aad_name(D, 1);
    let seg2 = layout::segment_aad_name(D, 2);
    let sealed = seal_small(&k, FileKind::Segment, 0, V, &seg1, b"records").unwrap();

    // 服务器把段 1 放到段 2 的位置、或冒充别的设备的段
    assert!(is_corrupt(open_small(&k, FileKind::Segment, V, &seg2, &sealed)));
    let other_dev = layout::segment_aad_name("0000000000000000", 1);
    assert!(is_corrupt(open_small(&k, FileKind::Segment, V, &other_dev, &sealed)));
    // 段当成 head 读（kind 不同）
    assert!(is_corrupt(open_small(&k, FileKind::Head, V, &seg1, &sealed)));
    // 别的 vault 的文件：既不能用本 vault 的 id 打开，也不能换个子密钥打开
    assert!(is_corrupt(open_small(&k, FileKind::Segment, V2, &seg1, &sealed)));
    let k2 = keys(1, V2);
    assert!(is_corrupt(open_small(&k2, FileKind::Segment, V2, &seg1, &sealed)));
    assert!(is_corrupt(open_small(&k2, FileKind::Segment, V, &seg1, &sealed)));
    // 别的口令派生的主密钥
    assert!(is_corrupt(open_small(
        &keys(2, V),
        FileKind::Segment,
        V,
        &seg1,
        &sealed
    )));
}

#[test]
fn small_file_api_rejects_blob_kind() {
    let k = keys(1, V);
    assert!(matches!(
        seal_small(&k, FileKind::Blob, 0, V, "x", b"x"),
        Err(SyncError::Internal(_))
    ));
    assert!(matches!(
        open_small(&k, FileKind::Blob, V, "x", &[0; 64]),
        Err(SyncError::Internal(_))
    ));
}

// ---------------------------------------------------------------------------------------------
// 口令校验字段

#[test]
fn check_value_verifies_only_the_right_key_and_vault() {
    let k = keys(1, V);
    let check = make_check(&k, V).unwrap();
    verify_check(&k, V, &check).unwrap();
    assert!(matches!(
        verify_check(&keys(2, V), V, &check),
        Err(SyncError::Passphrase)
    ));
    assert!(matches!(verify_check(&k, V2, &check), Err(SyncError::Passphrase)));
    let mut bad = check.clone();
    bad[NONCE_LEN] ^= 1;
    assert!(matches!(verify_check(&k, V, &bad), Err(SyncError::Passphrase)));
    assert!(matches!(verify_check(&k, V, &[]), Err(SyncError::Passphrase)));
    assert!(matches!(verify_check(&k, V, &check[..20]), Err(SyncError::Passphrase)));
    // 校验字段与小文件互不通用（AAD 前缀不同）
    let name = layout::head_aad_name(D);
    let sealed = seal_small(&k, FileKind::Head, 0, V, &name, crypto::CHECK_PLAINTEXT).unwrap();
    assert!(matches!(
        verify_check(&k, V, &sealed[HEADER_LEN..]),
        Err(SyncError::Passphrase)
    ));
}

#[test]
fn passphrase_to_check_end_to_end() {
    let kdf = new_kdf_params().unwrap();
    let master = derive_master("plain sailing 2026", &kdf).unwrap();
    let k = Keys::derive(&master, V).unwrap();
    let check = make_check(&k, V).unwrap();

    let again = Keys::derive(&derive_master("plain sailing 2026", &kdf).unwrap(), V).unwrap();
    verify_check(&again, V, &check).unwrap();
    let wrong = Keys::derive(&derive_master("Plain sailing 2026", &kdf).unwrap(), V).unwrap();
    assert!(matches!(verify_check(&wrong, V, &check), Err(SyncError::Passphrase)));
}

// ---------------------------------------------------------------------------------------------
// KDF 参数

#[test]
fn kdf_params_out_of_range_are_rejected_before_any_work() {
    let base = new_kdf_params().unwrap();
    assert_eq!(base.alg, KdfParams::ALG);
    assert_eq!(base.iterations, KdfParams::DEFAULT_ITERATIONS);
    assert_eq!(base.salt.len(), KdfParams::DEFAULT_SALT_BYTES);
    base.validate().unwrap();
    assert_ne!(new_kdf_params().unwrap().salt, base.salt);

    let with = |f: &dyn Fn(&mut KdfParams)| {
        let mut p = base.clone();
        f(&mut p);
        derive_master("whatever passphrase", &p)
    };
    for bad in [
        with(&|p| p.iterations = KdfParams::MIN_ITERATIONS - 1),
        with(&|p| p.iterations = KdfParams::MAX_ITERATIONS + 1),
        with(&|p| p.iterations = u32::MAX),
        with(&|p| p.iterations = 0),
        with(&|p| p.salt = vec![0; KdfParams::MIN_SALT_BYTES - 1]),
        with(&|p| p.salt = vec![0; KdfParams::MAX_SALT_BYTES + 1]),
        with(&|p| p.alg = "argon2id".into()),
    ] {
        assert!(matches!(bad, Err(SyncError::FormatTooNew)));
    }
    with(&|p| p.salt = vec![7; KdfParams::MAX_SALT_BYTES]).unwrap();
}

#[test]
fn master_key_depends_on_salt_and_passphrase() {
    let a = new_kdf_params().unwrap();
    let b = new_kdf_params().unwrap();
    let ka = derive_master("one two three four", &a).unwrap();
    assert_eq!(
        derive_master("one two three four", &a).unwrap().as_bytes(),
        ka.as_bytes()
    );
    assert_ne!(
        derive_master("one two three four", &b).unwrap().as_bytes(),
        ka.as_bytes()
    );
    assert_ne!(
        derive_master("one two three five", &a).unwrap().as_bytes(),
        ka.as_bytes()
    );
}

// ---------------------------------------------------------------------------------------------
// blob

fn blob_name() -> String {
    layout::blob_aad_name(D, &"ab".repeat(16))
}

#[test]
fn blobs_round_trip_at_chunk_boundaries() {
    let k = keys(3, V);
    let name = blob_name();
    for len in [
        0,
        1,
        BLOB_CHUNK_BYTES - 1,
        BLOB_CHUNK_BYTES,
        BLOB_CHUNK_BYTES + 1,
        3 * BLOB_CHUNK_BYTES,
        200_000,
    ] {
        let plain = pattern(len);
        let sealed = seal_blob(&k, V, &name, &plain);
        assert_eq!(sealed.len() as u64, sealed_blob_len(len as u64), "len {len}");
        assert_eq!(open_blob(&k, V, &name, &sealed).unwrap(), plain, "len {len}");
    }
}

#[test]
fn blob_truncation_is_detected() {
    let k = keys(3, V);
    let name = blob_name();
    // 两个满块 + 一个短块；以及恰好两个满块（最后一个满块带 last 标记）
    for len in [2 * BLOB_CHUNK_BYTES + 100, 2 * BLOB_CHUNK_BYTES] {
        let sealed = seal_blob(&k, V, &name, &pattern(len));
        // 截在块边界上：剩下的最后一块没有 last 标记
        for cut in [PRELUDE + CHUNK_SEALED, PRELUDE + 2 * CHUNK_SEALED] {
            if cut < sealed.len() {
                assert!(
                    is_corrupt(open_blob(&k, V, &name, &sealed[..cut])),
                    "len {len} cut {cut}"
                );
            }
        }
        // 截在块中间、只剩头、空文件、少一个字节
        for cut in [
            PRELUDE + 100,
            PRELUDE + CHUNK_SEALED + 5,
            PRELUDE,
            PRELUDE - 1,
            0,
            sealed.len() - 1,
        ] {
            assert!(
                is_corrupt(open_blob(&k, V, &name, &sealed[..cut])),
                "len {len} cut {cut}"
            );
        }
    }
}

#[test]
fn blob_extension_and_reordering_are_detected() {
    let k = keys(3, V);
    let name = blob_name();
    let len = 3 * BLOB_CHUNK_BYTES + 10;
    let sealed = seal_blob(&k, V, &name, &pattern(len));
    let chunk = |i: usize| &sealed[PRELUDE + i * CHUNK_SEALED..(PRELUDE + (i + 1) * CHUNK_SEALED).min(sealed.len())];

    // last 之后追加数据（垃圾、或重复一个真实的块）
    let mut extended = sealed.clone();
    extended.push(0);
    assert!(is_corrupt(open_blob(&k, V, &name, &extended)));
    let mut duplicated = sealed.clone();
    duplicated.extend_from_slice(chunk(0));
    assert!(is_corrupt(open_blob(&k, V, &name, &duplicated)));

    // 交换前两块
    let mut swapped = sealed[..PRELUDE].to_vec();
    swapped.extend_from_slice(chunk(1));
    swapped.extend_from_slice(chunk(0));
    swapped.extend_from_slice(&sealed[PRELUDE + 2 * CHUNK_SEALED..]);
    assert_eq!(swapped.len(), sealed.len());
    assert!(is_corrupt(open_blob(&k, V, &name, &swapped)));

    // 去掉中间一块（后面的块序号对不上）
    let mut dropped = sealed[..PRELUDE + CHUNK_SEALED].to_vec();
    dropped.extend_from_slice(&sealed[PRELUDE + 2 * CHUNK_SEALED..]);
    assert!(is_corrupt(open_blob(&k, V, &name, &dropped)));

    // 拼接另一个 blob 的块（nonce 前缀不同）
    let other = seal_blob(&k, V, &name, &pattern(len));
    let mut spliced = sealed[..PRELUDE + CHUNK_SEALED].to_vec();
    spliced.extend_from_slice(&other[PRELUDE + CHUNK_SEALED..]);
    assert!(is_corrupt(open_blob(&k, V, &name, &spliced)));
}

#[test]
fn blob_header_and_binding_tampering_is_detected() {
    let k = keys(3, V);
    let name = blob_name();
    let sealed = seal_blob(&k, V, &name, &pattern(1000));
    for index in [0, 4, 5, HEADER_LEN, PRELUDE - 1, PRELUDE, sealed.len() - 1] {
        let mut bad = sealed.clone();
        bad[index] ^= 0x01;
        assert!(is_corrupt(open_blob(&k, V, &name, &bad)), "byte {index}");
    }
    let other_name = layout::blob_aad_name(D, &"cd".repeat(16));
    assert!(is_corrupt(open_blob(&k, V, &other_name, &sealed)));
    assert!(is_corrupt(open_blob(&k, V2, &name, &sealed)));
    assert!(is_corrupt(open_blob(&keys(3, V2), V2, &name, &sealed)));
    assert!(is_corrupt(open_blob(&keys(4, V), V, &name, &sealed)));
    // 小文件不能当 blob 读，反之亦然
    let small = seal_small(&k, FileKind::Segment, 0, V, &name, &pattern(1000)).unwrap();
    assert!(is_corrupt(open_blob(&k, V, &name, &small)));
    assert!(is_corrupt(open_small(&k, FileKind::Segment, V, &name, &sealed)));
}

#[test]
fn blob_files_round_trip_and_failed_opens_leave_no_plaintext() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("plain");
    let sealed_path = dir.path().join("sealed");
    let out = dir.path().join("out");
    let plain = pattern(150_000);
    std::fs::write(&src, &plain).unwrap();

    let k = keys(5, V);
    let name = blob_name();
    let written = seal_blob_file(&k, V, &name, &src, &sealed_path).unwrap();
    assert_eq!(written, sealed_blob_len(plain.len() as u64));
    assert_eq!(std::fs::metadata(&sealed_path).unwrap().len(), written);
    assert_eq!(
        open_blob_file(&k, V, &name, &sealed_path, &out).unwrap(),
        plain.len() as u64
    );
    assert_eq!(std::fs::read(&out).unwrap(), plain);

    // 截断的密文：报 Corrupt，并删除已写出的部分明文
    let sealed = std::fs::read(&sealed_path).unwrap();
    std::fs::write(&sealed_path, &sealed[..PRELUDE + CHUNK_SEALED]).unwrap();
    std::fs::remove_file(&out).unwrap();
    assert!(is_corrupt(open_blob_file(&k, V, &name, &sealed_path, &out)));
    assert!(!out.exists());

    // 源文件不存在：io 错误，不创建目标
    let missing = dir.path().join("missing");
    let target = dir.path().join("target");
    assert!(matches!(
        seal_blob_file(&k, V, &name, &missing, &target),
        Err(SyncError::Io(io::ErrorKind::NotFound))
    ));
    assert!(!target.exists());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn blob_round_trip_any_length(len in 0usize..(4 * BLOB_CHUNK_BYTES + 3)) {
        let k = keys(6, V);
        let name = blob_name();
        let plain = pattern(len);
        let sealed = seal_blob(&k, V, &name, &plain);
        prop_assert_eq!(sealed.len() as u64, sealed_blob_len(len as u64));
        prop_assert_eq!(open_blob(&k, V, &name, &sealed).unwrap(), plain);
    }
}

// ---------------------------------------------------------------------------------------------
// 命名与随机值

#[test]
fn blob_names_are_keyed_and_stable() {
    let k = keys(7, V);
    let a = crypto::blob_name(&k, b"item-hash");
    assert!(layout::is_valid_blob_name(&a));
    assert_eq!(crypto::blob_name(&keys(7, V), b"item-hash"), a);
    assert_ne!(crypto::blob_name(&k, b"item-hash2"), a);
    assert_ne!(crypto::blob_name(&keys(8, V), b"item-hash"), a);
    assert_ne!(crypto::blob_name(&keys(7, V2), b"item-hash"), a);
}

#[test]
fn random_ids_are_valid_and_distinct() {
    let a = random_id().unwrap();
    let b = random_id().unwrap();
    assert!(layout::is_valid_id(&a) && layout::is_valid_id(&b));
    assert_ne!(a, b);
}

#[test]
fn keys_do_not_leak_in_debug() {
    let k = keys(9, V);
    assert_eq!(format!("{k:?}"), "Keys(..)");
}

// ---------------------------------------------------------------------------------------------
// 口令规则

#[test]
fn generated_passphrases_follow_the_format_and_rules() {
    let a = generate_passphrase().unwrap();
    let b = generate_passphrase().unwrap();
    assert_ne!(a, b);
    for p in [&a, &b] {
        let groups: Vec<&str> = p.split('-').collect();
        assert_eq!(groups.len(), 6, "{p}");
        assert!(
            groups
                .iter()
                .all(|g| g.len() == 4 && g.bytes().all(|c| c.is_ascii_uppercase() || (b'2'..=b'7').contains(&c)))
        );
        check_passphrase(p, &["password", "user", "dav.example.com"]).unwrap();
    }
}

#[test]
fn passphrase_rules() {
    let forbidden = ["App-Password-123", "someone@example.com", "dav.example.com"];
    assert_eq!(check_passphrase("short", &forbidden), Err(PassphraseIssue::TooShort));
    assert_eq!(
        check_passphrase("elevenchars", &forbidden),
        Err(PassphraseIssue::TooShort)
    );
    assert_eq!(check_passphrase("twelve chars", &forbidden), Ok(()));
    // 首尾空白不算长度
    assert_eq!(
        check_passphrase("   elevenchars   ", &forbidden),
        Err(PassphraseIssue::TooShort)
    );
    assert_eq!(
        check_passphrase("            ", &forbidden),
        Err(PassphraseIssue::TooShort)
    );
    // 按字符而不是字节计数：12 个汉字可以，11 个不行
    assert_eq!(check_passphrase("春眠不觉晓处处闻啼鸟夜来", &forbidden), Ok(()));
    assert_eq!(
        check_passphrase("春眠不觉晓处处闻啼鸟夜", &forbidden),
        Err(PassphraseIssue::TooShort)
    );
    // 与账号凭据相同（忽略大小写与首尾空白）
    assert_eq!(
        check_passphrase("app-password-123", &forbidden),
        Err(PassphraseIssue::Reused)
    );
    assert_eq!(
        check_passphrase("SOMEONE@EXAMPLE.COM", &forbidden),
        Err(PassphraseIssue::Reused)
    );
    assert_eq!(
        check_passphrase(" Dav.Example.Com ", &forbidden),
        Err(PassphraseIssue::Reused)
    );
    // 只是包含不算相同；空的禁用项忽略
    assert_eq!(check_passphrase("App-Password-1234", &forbidden), Ok(()));
    assert_eq!(check_passphrase("a perfectly fine one", &["", "  "]), Ok(()));
    // 组合与分解形式视为相同
    assert_eq!(
        check_passphrase("caf\u{e9} au lait 42", &["cafe\u{301} au lait 42"]),
        Err(PassphraseIssue::Reused)
    );
    assert_eq!(PassphraseIssue::Reused.reason(), "passphrase_reused");
}
