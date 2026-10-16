//! 守护单测：oar-ocr / oar-ocr-core 走 `vendor/` 的补丁（`image` 与 `imageproc` 不开默认特性、`tokenizers` 收进公式识别的
//! feature）。补丁失效时这些 crate 会回到依赖图里，安装包多出约 0.9 MB、许可清单多出几十项，并带进 C 代码。
//! 升级 oar-ocr 时先看上游是否已经有同样的特性开关（说明见 `vendor/README.md`）。

use std::path::Path;

fn workspace_file(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

/// Cargo.lock 里某个 crate 的全部条目（每条是 `[[package]]` 之后的那一段）
fn lock_entries<'a>(lock: &'a str, name: &str) -> Vec<&'a str> {
    let needle = format!("name = \"{name}\"\n");
    lock.split("[[package]]")
        .filter(|e| e.trim_start().starts_with(&needle))
        .collect()
}

#[test]
fn oar_ocr_is_patched_so_unused_image_codecs_and_c_code_stay_out() {
    let lock = workspace_file("Cargo.lock");
    for name in ["oar-ocr", "oar-ocr-core"] {
        let entries = lock_entries(&lock, name);
        assert_eq!(entries.len(), 1, "{name} 只有一个版本");
        assert!(
            entries[0].contains("version = \"0.10.0\""),
            "{name} 的版本变了：先处理 vendor/ 下的补丁"
        );
        assert!(
            !entries[0].contains("source = "),
            "{name} 不再来自 vendor/（[patch.crates-io] 丢了？）"
        );
    }
    for gone in [
        "tokenizers",
        "onig_sys",
        "rav1e",
        "ravif",
        "exr",
        "tiff",
        "image-webp",
        "ab_glyph",
        "rustfft",
    ] {
        assert!(lock_entries(&lock, gone).is_empty(), "{gone} 回到了依赖图里");
    }
    let core = workspace_file("vendor/oar-ocr-core/Cargo.toml");
    assert!(
        core.contains("formula = [\"dep:tokenizers\"]"),
        "oar-ocr-core 的 formula feature 丢了"
    );
    let ocr = workspace_file("vendor/oar-ocr/Cargo.toml");
    assert!(
        ocr.contains("formula = [\"oar-ocr-core/formula\"]"),
        "oar-ocr 的 formula feature 丢了"
    );
}
