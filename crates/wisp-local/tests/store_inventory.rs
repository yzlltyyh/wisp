//! 本机存储：库存扫描（安装记录 + 文件大小）、部分文件统计、删除（先进回收区）、清理残留。

use std::path::Path;

use wisp_local::catalog::{self, CATALOG_REVISION};
use wisp_local::store::{INSTALLED_FILE, InstallState, InstalledFile, InstalledManifest, Store};
use wisp_types::{LocalModelId, LocalRuntimeId};

fn manifest(files: &[(&str, u64)], revision: u32) -> InstalledManifest {
    InstalledManifest {
        catalog_revision: revision,
        files: files
            .iter()
            .map(|(name, size)| InstalledFile {
                name: (*name).to_owned(),
                size: *size,
                sha256: "0".repeat(64),
            })
            .collect(),
        installed_at: 1,
    }
}

fn write(path: &Path, len: usize) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(path, vec![1u8; len]).expect("write");
}

#[test]
fn missing_root_is_an_empty_inventory() {
    let dir = tempfile::tempdir().expect("dir");
    let store = Store::new(dir.path().join("models"));
    let inv = store.scan().expect("scan");
    assert!(inv.models.is_empty() && inv.runtimes.is_empty());
    assert_eq!(inv.disk_used, 0);
}

#[test]
fn installed_broken_stale_and_partial() {
    let dir = tempfile::tempdir().expect("dir");
    let store = Store::new(dir.path().join("models"));
    // 已安装：文件大小与记录一致
    let tiny = store.model_dir(LocalModelId::PpOcrV6Tiny);
    write(&tiny.join("det.onnx"), 10);
    write(&tiny.join("rec.onnx"), 20);
    store
        .write_manifest(
            &tiny,
            &manifest(&[("det.onnx", 10), ("rec.onnx", 20)], CATALOG_REVISION),
        )
        .expect("manifest");
    // 损坏：少了一个文件
    let small = store.model_dir(LocalModelId::PpOcrV6Small);
    write(&small.join("det.onnx"), 10);
    store
        .write_manifest(
            &small,
            &manifest(&[("det.onnx", 10), ("rec.onnx", 5)], CATALOG_REVISION),
        )
        .expect("manifest");
    // 旧版本目录装的：有更新
    let it2b = store.model_dir(LocalModelId::IndexTranslate2b);
    write(&it2b.join("model.gguf"), 7);
    store
        .write_manifest(&it2b, &manifest(&[("model.gguf", 7)], CATALOG_REVISION - 1))
        .expect("manifest");
    // 安装记录不是合法的 JSON
    let vl = store.model_dir(LocalModelId::PaddleOcrVl16);
    write(&vl.join("model.gguf"), 3);
    std::fs::write(vl.join(INSTALLED_FILE), b"{not json").expect("write");
    // 只有部分文件（下载到一半）
    write(
        &store
            .partial_dir(LocalModelId::IndexTranslate9b)
            .join("model.gguf.part"),
        1234,
    );
    write(
        &store
            .partial_dir(LocalModelId::IndexTranslate9b)
            .join("model.gguf.part.meta"),
        6,
    );
    // 运行时：当前版本已安装 + 一份旧版本的残留
    let ort = store.runtime_dir(LocalRuntimeId::OnnxRuntime);
    write(&ort.join("libonnxruntime.dylib"), 100);
    store
        .write_manifest(&ort, &manifest(&[("libonnxruntime.dylib", 100)], CATALOG_REVISION))
        .expect("manifest");
    write(&ort.parent().expect("parent").join("1.0.0/old.dylib"), 50);

    let inv = store.scan().expect("scan");
    assert_eq!(
        inv.models[&LocalModelId::PpOcrV6Tiny],
        InstallState::Installed { stale: false }
    );
    assert_eq!(inv.models[&LocalModelId::PpOcrV6Small], InstallState::Broken);
    assert_eq!(
        inv.models[&LocalModelId::IndexTranslate2b],
        InstallState::Installed { stale: true }
    );
    assert_eq!(inv.models[&LocalModelId::PaddleOcrVl16], InstallState::Broken);
    assert_eq!(inv.models[&LocalModelId::IndexTranslate9b], InstallState::NotInstalled);
    assert!(inv.is_installed(LocalModelId::PpOcrV6Tiny) && !inv.is_installed(LocalModelId::PpOcrV6Small));
    assert_eq!(inv.partial[&LocalModelId::IndexTranslate9b], 1240);
    assert!(!inv.partial.contains_key(&LocalModelId::PpOcrV6Tiny));
    assert!(inv.runtime_installed(LocalRuntimeId::OnnxRuntime));
    assert!(!inv.runtime_installed(LocalRuntimeId::LlamaCpp));
    let ort_size = inv.runtimes[&LocalRuntimeId::OnnxRuntime].size;
    assert!(ort_size >= 150, "含旧版本的残留：{ort_size}");
    assert!(inv.disk_used >= 10 + 20 + 10 + 7 + 3 + 1240 + 150);

    // 文件大小被改了 → 损坏
    write(&tiny.join("rec.onnx"), 21);
    assert_eq!(
        store.scan().expect("scan").models[&LocalModelId::PpOcrV6Tiny],
        InstallState::Broken
    );
}

#[test]
fn deleting_moves_through_the_trash() {
    let dir = tempfile::tempdir().expect("dir");
    let store = Store::new(dir.path().join("models"));
    let model = store.model_dir(LocalModelId::IndexTranslate2b);
    write(&model.join("model.gguf"), 10);
    write(&store.partial_dir(LocalModelId::IndexTranslate2b).join("x.part"), 10);
    store.delete_model(LocalModelId::IndexTranslate2b).expect("delete");
    assert!(!model.exists());
    assert!(
        !store.partial_dir(LocalModelId::IndexTranslate2b).exists(),
        "部分文件一并删除"
    );
    // 删除不存在的东西不算错
    store.delete_model(LocalModelId::IndexTranslate2b).expect("again");
    store.discard_partial(LocalModelId::PpOcrV6Tiny).expect("no partial");

    let rt = store.runtime_dir(LocalRuntimeId::LlamaCpp);
    write(&rt.join("llama-server"), 10);
    store.delete_runtime(LocalRuntimeId::LlamaCpp).expect("delete runtime");
    assert!(!rt.parent().expect("parent").exists(), "全部版本一起删");

    // 上次没删完的回收区与解包临时目录：下次启动时清掉，正常的目录不动
    write(&store.root().join(".trash-index_translate_9b-0/model.gguf"), 5);
    write(&store.root().join("runtimes/onnxruntime/.extract-1.28.3/x"), 5);
    write(&store.model_dir(LocalModelId::PpOcrV6Tiny).join("det.onnx"), 5);
    store.purge_trash();
    assert!(!store.root().join(".trash-index_translate_9b-0").exists());
    assert!(!store.root().join("runtimes/onnxruntime/.extract-1.28.3").exists());
    assert!(store.model_dir(LocalModelId::PpOcrV6Tiny).join("det.onnx").exists());
}

#[test]
fn manifests_are_written_atomically_and_round_trip() {
    let dir = tempfile::tempdir().expect("dir");
    let store = Store::new(dir.path().to_path_buf());
    let target = dir.path().join("x");
    let m = manifest(&[("a", 1)], CATALOG_REVISION);
    store.write_manifest(&target, &m).expect("write");
    let back: InstalledManifest =
        serde_json::from_slice(&std::fs::read(target.join(INSTALLED_FILE)).expect("read")).expect("json");
    assert_eq!(back, m);
    assert!(!target.join(format!("{INSTALLED_FILE}.tmp")).exists());
}

#[test]
fn runtime_main_points_into_the_versioned_directory() {
    let store = Store::new("/m".into());
    for t in [
        catalog::Target::MacosArm64,
        catalog::Target::WindowsX64,
        catalog::Target::LinuxX64,
    ] {
        let main = store.runtime_main(LocalRuntimeId::LlamaCpp, t).expect("main");
        assert!(main.starts_with(store.runtime_dir(LocalRuntimeId::LlamaCpp)));
    }
}
