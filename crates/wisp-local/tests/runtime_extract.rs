//! 运行时发布包的解包：只取规则内的文件、保留同目录的版本号软链接与可执行位、拒绝越界路径与越界链接、每条规则都要命中；
//! 带 `--ignored` 时解包 `WISP_LOCAL_TEST_DIR` 里官方的 llama.cpp 与 ONNX Runtime 发布包。
// 真机用例把解包的数据打印给运行者看
#![allow(clippy::print_stdout)]

use std::io::Write;
use std::path::{Path, PathBuf};

use wisp_local::LocalError;
use wisp_local::catalog::{self, ArchiveKind, ExtractRule, Target};
use wisp_local::download::extract::extract;
use wisp_types::{DownloadErrorCode, LocalRuntimeId};

const RULES: [ExtractRule; 3] = [
    ExtractRule {
        prefix: "pkg/bin/server",
        suffix: "",
        dest_dir: "",
        executable: true,
    },
    ExtractRule {
        prefix: "pkg/bin/libcore.0",
        suffix: ".so",
        dest_dir: "",
        executable: false,
    },
    ExtractRule {
        prefix: "pkg/bin/libcpu-",
        suffix: ".so",
        dest_dir: "",
        executable: false,
    },
];

enum Item<'a> {
    File(&'a str, &'a [u8], u32),
    Link(&'a str, &'a str),
}

/// 用 tar::Builder 写一个 .tar.gz；路径直接写进头部（不经 Builder 的路径检查，以便构造恶意的包）
fn tar_gz(items: &[Item<'_>]) -> Vec<u8> {
    let enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut b = tar::Builder::new(enc);
    for item in items {
        let mut h = tar::Header::new_gnu();
        let (name, kind, data, mode, link): (&str, tar::EntryType, &[u8], u32, Option<&str>) = match item {
            Item::File(n, d, m) => (n, tar::EntryType::Regular, d, *m, None),
            Item::Link(n, t) => (n, tar::EntryType::Symlink, &[], 0o777, Some(t)),
        };
        {
            let gnu = h.as_gnu_mut().expect("gnu");
            gnu.name[..name.len()].copy_from_slice(name.as_bytes());
            if let Some(t) = link {
                gnu.linkname[..t.len()].copy_from_slice(t.as_bytes());
            }
        }
        h.set_entry_type(kind);
        h.set_mode(mode);
        h.set_size(data.len() as u64);
        h.set_cksum();
        b.append(&h, data).expect("append");
    }
    b.into_inner().expect("tar").finish().expect("gz")
}

fn write_archive(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, bytes).expect("write");
    p
}

#[test]
fn tar_keeps_only_the_listed_files_links_and_modes() {
    let dir = tempfile::tempdir().expect("dir");
    let archive = write_archive(
        dir.path(),
        "a.tar.gz",
        &tar_gz(&[
            Item::File("pkg/bin/server", b"#!bin", 0o755),
            Item::File("pkg/bin/libcore.0.7.0.so", b"core", 0o644),
            Item::Link("pkg/bin/libcore.0.so", "libcore.0.7.0.so"),
            Item::Link("pkg/bin/libcore.so", "libcore.0.so"),
            Item::File("pkg/bin/libcpu-haswell.so", b"h", 0o4755),
            Item::File("pkg/bin/libcpu-zen4.so", b"z", 0o644),
            Item::File("pkg/bin/other-tool", b"x", 0o755),
            Item::File("pkg/README", b"r", 0o644),
        ]),
    );
    let dest = dir.path().join("out");
    std::fs::create_dir_all(&dest).expect("dest");
    let mut got = extract(&archive, ArchiveKind::TarGz, &RULES, &dest).expect("extract");
    got.sort();
    assert_eq!(
        got,
        [
            "libcore.0.7.0.so",
            "libcore.0.so",
            "libcpu-haswell.so",
            "libcpu-zen4.so",
            "server"
        ]
        .map(PathBuf::from)
        .to_vec()
    );
    assert!(!dest.join("other-tool").exists() && !dest.join("libcore.so").exists());
    let link = std::fs::read_link(dest.join("libcore.0.so")).expect("link");
    assert_eq!(link, Path::new("libcore.0.7.0.so"));
    assert_eq!(std::fs::read(dest.join("libcore.0.so")).expect("follow"), b"core");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &str| std::fs::metadata(dest.join(p)).expect("meta").permissions().mode() & 0o7777;
        assert_eq!(mode("server"), 0o755);
        assert_eq!(mode("libcpu-haswell.so"), 0o755, "setuid 位被去掉");
    }
    // 再解一次（覆盖已有文件与链接）也没问题
    extract(&archive, ArchiveKind::TarGz, &RULES, &dest).expect("again");
}

fn expect_bad(items: &[Item<'_>]) {
    let dir = tempfile::tempdir().expect("dir");
    let archive = write_archive(dir.path(), "a.tar.gz", &tar_gz(items));
    let dest = dir.path().join("out");
    std::fs::create_dir_all(&dest).expect("dest");
    let err = extract(&archive, ArchiveKind::TarGz, &RULES, &dest).expect_err("bad archive");
    assert!(
        matches!(err, LocalError::Download(DownloadErrorCode::Extract)),
        "{err:?}"
    );
    assert!(!dir.path().join("evil").exists());
}

const OK_REST: [Item<'static>; 2] = [
    Item::File("pkg/bin/libcore.0.7.0.so", b"c", 0o644),
    Item::File("pkg/bin/libcpu-x64.so", b"x", 0o644),
];

#[test]
fn hostile_or_incomplete_archives_are_rejected() {
    // 链接指向目录外
    expect_bad(&[
        Item::File("pkg/bin/server", b"s", 0o755),
        Item::Link("pkg/bin/libcore.0.so", "../../evil"),
        OK_REST[1].clone_item(),
    ]);
    // 链接悬空
    expect_bad(&[
        Item::File("pkg/bin/server", b"s", 0o755),
        Item::Link("pkg/bin/libcore.0.so", "libcore.0.9.so"),
        OK_REST[1].clone_item(),
    ]);
    // 规则要求可执行，包里却没有可执行位
    expect_bad(&[
        Item::File("pkg/bin/server", b"s", 0o644),
        OK_REST[0].clone_item(),
        OK_REST[1].clone_item(),
    ]);
    // 缺了规则要的文件
    expect_bad(&[Item::File("pkg/bin/server", b"s", 0o755), OK_REST[0].clone_item()]);
    // 不是 gzip
    let dir = tempfile::tempdir().expect("dir");
    let archive = write_archive(dir.path(), "x.tar.gz", b"definitely not gzip");
    assert!(extract(&archive, ArchiveKind::TarGz, &RULES, dir.path()).is_err());
}

#[test]
fn traversal_paths_are_never_selected() {
    // `../evil` 与绝对路径都不匹配任何规则（规则前缀是包内的普通相对路径），被跳过而不是写到目录外
    let dir = tempfile::tempdir().expect("dir");
    let archive = write_archive(
        dir.path(),
        "a.tar.gz",
        &tar_gz(&[
            Item::File("../evil", b"e", 0o644),
            Item::File("pkg/bin/server", b"s", 0o755),
            OK_REST[0].clone_item(),
            OK_REST[1].clone_item(),
            Item::File("pkg/bin/libcpu-../../evil.so", b"e", 0o644),
        ]),
    );
    let dest = dir.path().join("out");
    std::fs::create_dir_all(&dest).expect("dest");
    let got = extract(&archive, ArchiveKind::TarGz, &RULES, &dest).expect("extract");
    assert_eq!(got.len(), 3);
    assert!(!dir.path().join("evil").exists() && !dir.path().join("evil.so").exists());
}

impl Item<'static> {
    fn clone_item(&self) -> Item<'static> {
        match self {
            Item::File(a, b, c) => Item::File(a, b, *c),
            Item::Link(a, b) => Item::Link(a, b),
        }
    }
}

#[test]
fn zip_extracts_listed_files_and_checks_crc() {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in [
            ("pkg/bin/server", &b"exe"[..]),
            ("pkg/bin/libcore.0.dll.so", b"no"),
            ("pkg/bin/libcore.0.so", b"core"),
            ("pkg/bin/libcpu-a.so", b"a"),
            ("pkg/bin/tool.exe", b"t"),
        ] {
            z.start_file(name, opts).expect("start");
            z.write_all(data).expect("write");
        }
        z.finish().expect("finish");
    }
    let dir = tempfile::tempdir().expect("dir");
    let bytes = buf.into_inner();
    let archive = write_archive(dir.path(), "a.zip", &bytes);
    let dest = dir.path().join("out");
    std::fs::create_dir_all(&dest).expect("dest");
    let mut got = extract(&archive, ArchiveKind::Zip, &RULES, &dest).expect("extract");
    got.sort();
    assert_eq!(got.len(), 4, "{got:?}");
    assert_eq!(std::fs::read(dest.join("libcore.0.so")).expect("read"), b"core");
    assert!(!dest.join("tool.exe").exists());

    // 改坏压缩数据：CRC 校验失败
    let mut broken = bytes.clone();
    // 第一个条目（pkg/bin/server）的压缩数据从本地头 + 文件名 + 扩展字段之后开始
    let lfh = broken.windows(4).position(|w| w == b"PK\x03\x04").expect("lfh");
    let n = usize::from(u16::from_le_bytes([broken[lfh + 26], broken[lfh + 27]]));
    let m = usize::from(u16::from_le_bytes([broken[lfh + 28], broken[lfh + 29]]));
    broken[lfh + 30 + n + m] ^= 0x55;
    broken[lfh + 31 + n + m] ^= 0x55;
    let archive = write_archive(dir.path(), "b.zip", &broken);
    assert!(extract(&archive, ArchiveKind::Zip, &RULES, &dest).is_err());
}

/// 真机：解包官方的 macOS 发布包（`WISP_LOCAL_TEST_DIR` = artifacts/local-models 的绝对路径）
#[test]
#[ignore = "需要 WISP_LOCAL_TEST_DIR 里下载好的官方发布包"]
fn official_macos_archives() {
    let root = PathBuf::from(std::env::var("WISP_LOCAL_TEST_DIR").expect("WISP_LOCAL_TEST_DIR"));
    let dir = tempfile::tempdir_in(&root).expect("dir");
    for (rt, file) in [
        (LocalRuntimeId::LlamaCpp, "llama-b11669-bin-macos-arm64.tar.gz"),
        (LocalRuntimeId::OnnxRuntime, "onnxruntime-osx-arm64-1.28.3.tgz"),
    ] {
        let asset = catalog::runtime_asset(rt, Target::MacosArm64).expect("asset");
        let archive = root.join("raw/runtime").join(file);
        let dest = dir.path().join(rt.as_str());
        std::fs::create_dir_all(&dest).expect("dest");
        let started = std::time::Instant::now();
        let files = extract(&archive, asset.archive, asset.extract, &dest).expect("extract");
        let total: u64 = files
            .iter()
            .filter_map(|f| std::fs::symlink_metadata(dest.join(f)).ok())
            .filter(|m| m.is_file())
            .map(|m| m.len())
            .sum();
        println!(
            "{}: {} entries, {:.1} MB in {} ms",
            rt.as_str(),
            files.len(),
            total as f64 / 1e6,
            started.elapsed().as_millis()
        );
        assert!(dest.join(asset.main).is_file());
        #[cfg(unix)]
        if rt == LocalRuntimeId::LlamaCpp {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dest.join("llama-server"))
                .expect("meta")
                .permissions()
                .mode();
            assert!(mode & 0o111 != 0, "llama-server 可执行");
            // 依赖里写的是带主版本号的软链接名
            for link in ["libllama.0.dylib", "libggml-metal.0.dylib", "libmtmd.0.dylib"] {
                assert!(std::fs::read_link(dest.join(link)).is_ok(), "{link}");
                assert!(std::fs::metadata(dest.join(link)).is_ok(), "{link} 指向解出的文件");
            }
            assert!(!dest.join("llama-cli").exists() && !dest.join("libllama.dylib").exists());
        }
    }
}
