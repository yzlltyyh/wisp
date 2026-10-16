//! 真实网络上的下载（`#[ignore]`）：从 ModelScope / Hugging Face / GitHub 下载钉死的文件，验证续传、区间提取与整套安装流程。
//!
//! `WISP_LOCAL_TEST_DIR=<artifacts/local-models 的绝对路径> cargo test -p wisp-local --test download_live -- --ignored --nocapture --test-threads=1`
//!
//! 临时目录建在 `WISP_LOCAL_TEST_DIR` 下，用完即删；续传用例拿那里已经下载好的原件比对内容。
// 真机数据打印给运行者看
#![allow(clippy::print_stdout)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use wisp_local::LocalError;
use wisp_local::catalog::{self, Target};
use wisp_local::download::{self, Fetcher, Job, Progress, Ranged, Remote, Tuning};
use wisp_local::store::{InstallState, Store};
use wisp_types::{DownloadSource, LocalModelId, LocalRuntimeId};

fn root() -> PathBuf {
    PathBuf::from(std::env::var("WISP_LOCAL_TEST_DIR").expect("WISP_LOCAL_TEST_DIR"))
}

#[tokio::test]
#[ignore = "真实网络：ModelScope 与 GitHub"]
async fn installs_pp_ocrv6_tiny_with_its_runtime() {
    let dir = tempfile::tempdir_in(root()).expect("dir");
    let store = Store::new(dir.path().join("models"));
    let job = Job {
        model: LocalModelId::PpOcrV6Tiny,
        target: Target::current().expect("target"),
        source: DownloadSource::Auto,
    };
    let calls = Arc::new(AtomicU64::new(0));
    let c = Arc::clone(&calls);
    let last = Arc::new(parking_lot::Mutex::new(None::<Progress>));
    let l = Arc::clone(&last);
    let cb = move |p: Progress| {
        c.fetch_add(1, Ordering::Relaxed);
        *l.lock() = Some(p);
    };
    let started = Instant::now();
    download::run(&store, &job, &cb, CancellationToken::new())
        .await
        .expect("download");
    let elapsed = started.elapsed();
    let inv = store.scan().expect("scan");
    assert_eq!(
        inv.models[&LocalModelId::PpOcrV6Tiny],
        InstallState::Installed { stale: false }
    );
    assert!(inv.runtime_installed(LocalRuntimeId::OnnxRuntime));
    assert!(inv.partial.is_empty(), "部分文件已清理");
    let dylib = store
        .runtime_main(LocalRuntimeId::OnnxRuntime, job.target)
        .expect("main");
    assert!(dylib.is_file());
    let total = catalog::model_download_bytes(job.model)
        + catalog::runtime_asset(LocalRuntimeId::OnnxRuntime, job.target).map_or(0, |a| a.size);
    println!(
        "pp_ocrv6_tiny + onnxruntime: {:.1} MB in {:.1} s, {} progress callbacks, disk {:.1} MB",
        total as f64 / 1e6,
        elapsed.as_secs_f64(),
        calls.load(Ordering::Relaxed),
        inv.disk_used as f64 / 1e6
    );
    // 已安装时再跑一次什么也不做
    download::run(&store, &job, &cb, CancellationToken::new())
        .await
        .expect("noop");
}

/// 下到大约 `stop_at` 字节就取消
async fn fetch_until(remote: &Remote<'_>, part: &std::path::Path, stop_at: u64) -> (Result<(), LocalError>, Duration) {
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    let cb = move |p: Progress| {
        if p.done >= stop_at {
            c.cancel();
        }
    };
    let have = std::fs::metadata(part).map_or(0, |m| m.len());
    let mut f = Fetcher::new(&cb, remote.size, have, cancel, Tuning::default()).expect("fetcher");
    let started = Instant::now();
    (f.file(remote, part).await, started.elapsed())
}

fn same_prefix(part: &std::path::Path, original: &std::path::Path) -> u64 {
    use std::io::Read;
    let got = std::fs::read(part).expect("part");
    let mut want = vec![0u8; got.len()];
    std::fs::File::open(original)
        .expect("original")
        .read_exact(&mut want)
        .expect("read");
    assert!(got == want, "部分文件与原件不一致");
    got.len() as u64
}

#[tokio::test]
#[ignore = "真实网络：ModelScope"]
async fn interrupted_large_download_resumes_from_modelscope() {
    let spec = catalog::model(LocalModelId::PaddleOcrVl16)
        .files
        .iter()
        .find(|f| f.name == catalog::VL_MMPROJ_FILE)
        .expect("mmproj");
    let original = root().join("raw/PaddleOCR-VL-1.6-GGUF/PaddleOCR-VL-1.6-GGUF-mmproj.gguf");
    let dir = tempfile::tempdir_in(root()).expect("dir");
    let part = dir.path().join("mmproj.gguf.part");
    let remote = Remote {
        urls: &[spec.model_scope],
        size: spec.size,
        sha256: spec.sha256,
    };
    let (r, t) = fetch_until(&remote, &part, 20 << 20).await;
    assert!(matches!(r, Err(LocalError::Cancelled)), "{r:?}");
    let first = same_prefix(&part, &original);
    println!("first leg: {:.1} MB in {:.1} s", first as f64 / 1e6, t.as_secs_f64());
    assert!(first >= 20 << 20);
    assert!(dir.path().join("mmproj.gguf.part.meta").exists());
    // 续传：从断点继续（不会从头再来），内容仍与原件一致
    let (r, t) = fetch_until(&remote, &part, first + (10 << 20)).await;
    assert!(matches!(r, Err(LocalError::Cancelled)), "{r:?}");
    let second = same_prefix(&part, &original);
    println!(
        "resumed leg: {:.1} MB more in {:.1} s",
        (second - first) as f64 / 1e6,
        t.as_secs_f64()
    );
    assert!(second >= first + (10 << 20));
}

#[tokio::test]
#[ignore = "真实网络：Hugging Face（国内需要系统代理）"]
async fn hugging_face_source_works() {
    let spec = &catalog::model(LocalModelId::PpOcrV6Tiny).files[0];
    let dir = tempfile::tempdir_in(root()).expect("dir");
    let part = dir.path().join("det.onnx.part");
    let cb = |_: Progress| {};
    let mut f = Fetcher::new(&cb, spec.size, 0, CancellationToken::new(), Tuning::default()).expect("fetcher");
    let order = f.rank(&[spec.model_scope, spec.hugging_face]).await;
    println!("auto probe order (0 = ModelScope, 1 = Hugging Face): {order:?}");
    let started = Instant::now();
    f.file(
        &Remote {
            urls: &[spec.hugging_face],
            size: spec.size,
            sha256: spec.sha256,
        },
        &part,
    )
    .await
    .expect("hf");
    println!(
        "hugging face: {:.1} MB in {:.1} s",
        spec.size as f64 / 1e6,
        started.elapsed().as_secs_f64()
    );
}

#[tokio::test]
#[ignore = "真实网络：GitHub"]
async fn windows_onnxruntime_dll_by_range() {
    let asset = catalog::runtime_asset(LocalRuntimeId::OnnxRuntime, Target::WindowsX64).expect("asset");
    let entry = asset
        .extract
        .iter()
        .find(|r| r.suffix.is_empty())
        .map(|r| r.prefix)
        .expect("entry");
    let dir = tempfile::tempdir_in(root()).expect("dir");
    let dest = dir.path().join(asset.main);
    let fetched = Arc::new(AtomicU64::new(0));
    let fc = Arc::clone(&fetched);
    let cb = move |p: Progress| fc.store(p.done, Ordering::Relaxed);
    let mut f = Fetcher::new(&cb, asset.size, 0, CancellationToken::new(), Tuning::default()).expect("fetcher");
    let started = Instant::now();
    let r = f
        .zip_entry(asset.url, asset.size, entry, asset.entry_sha256.expect("sha"), &dest)
        .await
        .expect("ranged");
    assert_eq!(r, Ranged::Done);
    println!(
        "onnxruntime.dll: fetched {:.2} MB of {:.1} MB in {:.1} s, dll {:.1} MB",
        fetched.load(Ordering::Relaxed) as f64 / 1e6,
        asset.size as f64 / 1e6,
        started.elapsed().as_secs_f64(),
        std::fs::metadata(&dest).expect("dll").len() as f64 / 1e6
    );
}
