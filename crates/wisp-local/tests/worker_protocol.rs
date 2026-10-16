//! ORT 工作进程走真正的进程与管道：启动、ping、请求出错时返回固定的错误种类（不崩溃）、关 stdin 即退出。
//! 不需要 ONNX Runtime 与模型（环境变量指向不存在的文件）。

mod worker_support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use image::RgbaImage;
use wisp_local::error::{LocalError, RuntimeFailure};
use wisp_local::ocr::PpOcrTier;
use wisp_local::worker::{OrtWorker, WorkerConfig};
use worker_support::{StdLauncher, worker_exe};

fn missing_config() -> WorkerConfig {
    let base = std::env::temp_dir().join(format!("wisp-worker-protocol-{}", std::process::id()));
    WorkerConfig {
        exe: worker_exe(),
        dylib: base.join("libonnxruntime-missing"),
        models: base.join("models"),
        threads: 1,
    }
}

#[tokio::test]
async fn missing_runtime_is_reported_not_crashed() {
    let worker = OrtWorker::spawn(&StdLauncher, &missing_config()).expect("spawn + ping");
    assert!(worker.is_alive());
    let image = Arc::new(RgbaImage::from_pixel(64, 32, image::Rgba([255, 255, 255, 255])));
    let err = worker
        .ppocr(PpOcrTier::Tiny, Arc::clone(&image), 2048)
        .await
        .expect_err("no runtime");
    if cfg!(feature = "ort") {
        // 动态库文件不存在：提示重新下载
        assert!(
            matches!(err, LocalError::Runtime(RuntimeFailure::ModelCorrupt)),
            "{err:?}"
        );
    } else {
        assert!(matches!(err, LocalError::Protocol(_)), "{err:?}");
    }
    // 出错之后进程照常服务下一个请求
    let err = worker.layout(image).await.expect_err("no runtime");
    assert!(!matches!(err, LocalError::Runtime(RuntimeFailure::Crashed)), "{err:?}");
    assert!(worker.is_alive());

    let t0 = Instant::now();
    worker.shutdown(Duration::from_secs(2));
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "关 stdin 后自己退出，不需要强制结束"
    );
}

#[test]
fn unknown_worker_kind_exits_immediately() {
    let out = std::process::Command::new(worker_exe())
        .args(["--wisp-worker", "bogus"])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty() && out.stderr.is_empty(), "不写任何输出");
}

#[test]
fn a_missing_executable_fails_to_start() {
    let mut cfg = missing_config();
    cfg.exe = PathBuf::from("/nonexistent/wisp-worker");
    // 启动器直接用 cfg.exe（测试启动器的 current_exe 不参与）
    let err = OrtWorker::spawn(&StdLauncher, &cfg).expect_err("no exe");
    assert!(matches!(err, LocalError::Runtime(RuntimeFailure::StartFailed)));
}
