//! 真机测试：真实的 ONNX Runtime 动态库与官方模型，经 `examples/ort_worker` 的真正进程与管道协议。
//!
//! 全部 `#[ignore]`，手动运行：
//!
//! ```text
//! WISP_LOCAL_TEST_DIR=<目录> cargo test -p wisp-local --features ort --test ocr_live -- --ignored --nocapture
//! ```
//!
//! `<目录>` 下：`raw/PP-OCRv6_{tiny,small}_{det,rec}_onnx/`、`raw/PP-DocLayoutV3_onnx/`（官方仓库的文件）与 `rt/` 下解包的
//! ONNX Runtime 发布包（找其中的 `libonnxruntime*.dylib` / `libonnxruntime.so*` / `onnxruntime.dll`）。测试在该目录下临时建一个
//! 按 Wisp 存储布局排好的模型目录（硬链接，测试结束删除）。VL 流水线另需 `WISP_VL_TEST_URL`（一个已启动的 llama-server，
//! 加载 PaddleOCR-VL-1.6；可选 `WISP_VL_TEST_KEY`）。
//!
//! PP-OCR 黄金样例的指标：严格指标「文字一致且 IoU ≥ 0.9」（另报其中四个顶点完全相同的行数）、「文字一致且 IoU ≥ 0.5」、
//! 检测召回（有本地框 IoU ≥ 0.5）与多检（本地框没有对应的官方框）。

#![cfg(feature = "ort")]
// 真机测量的数据直接打印在终端
#![allow(clippy::print_stderr)]

mod worker_support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use image::RgbaImage;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wisp_local::ocr::layout::LayoutDetector;
use wisp_local::ocr::vl::{self, PipelineOptions, VlmBackend, VlmRequest};
use wisp_local::ocr::{PpOcrTier, TextLine, files};
use wisp_local::worker::{OrtWorker, WorkerConfig};
use wisp_types::{ErrorCode, LlmError, LocalModelId, OcrMode, StreamEvent};
use worker_support::{StdLauncher, rss_kb, worker_exe};

/// 测试素材目录
fn test_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("WISP_LOCAL_TEST_DIR").expect("WISP_LOCAL_TEST_DIR"))
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ppocr")
}

/// 按 Wisp 的存储布局排好的临时模型目录（硬链接），丢弃即删除
struct Models {
    root: PathBuf,
}

impl Models {
    fn new() -> Self {
        let raw = test_dir().join("raw");
        let root = test_dir().join(format!("store-ocr-live-{}", std::process::id()));
        let link = |src: PathBuf, dst: PathBuf| {
            std::fs::create_dir_all(dst.parent().expect("parent")).expect("mkdir");
            if dst.exists() {
                return;
            }
            std::fs::hard_link(&src, &dst)
                .or_else(|_| std::fs::copy(&src, &dst).map(drop))
                .expect("link model file");
        };
        for (id, tier) in [
            (LocalModelId::PpOcrV6Tiny, "tiny"),
            (LocalModelId::PpOcrV6Small, "small"),
        ] {
            let dir = root.join(id.as_str());
            link(
                raw.join(format!("PP-OCRv6_{tier}_det_onnx/inference.onnx")),
                dir.join(files::PPOCR_DET),
            );
            link(
                raw.join(format!("PP-OCRv6_{tier}_rec_onnx/inference.onnx")),
                dir.join(files::PPOCR_REC),
            );
            link(
                raw.join(format!("PP-OCRv6_{tier}_rec_onnx/inference.yml")),
                dir.join(files::PPOCR_REC_CONFIG),
            );
        }
        link(
            raw.join("PP-DocLayoutV3_onnx/inference.onnx"),
            root.join(LocalModelId::PaddleOcrVl16.as_str()).join(files::LAYOUT),
        );
        Self { root }
    }

    fn config(&self) -> WorkerConfig {
        WorkerConfig {
            exe: worker_exe(),
            dylib: find_dylib(&test_dir().join("rt")).expect("ONNX Runtime library under rt/"),
            models: self.root.clone(),
            threads: 4,
        }
    }
}

impl Drop for Models {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// 在解包的发布包里找 ONNX Runtime 的主动态库（带版本号的那个文件，不是符号链接）
fn find_dylib(dir: &Path) -> Option<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    let mut best: Option<PathBuf> = None;
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).ok()?.flatten() {
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).ok()?;
            if meta.is_dir() {
                // macOS 发布包附带 .dSYM 调试符号，里面有同名的 DWARF 文件，不能当动态库加载
                if path.extension().is_none_or(|e| e != "dSYM") {
                    stack.push(path);
                }
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            let name = path.file_name()?.to_string_lossy().to_string();
            let is_lib = (name.starts_with("libonnxruntime.") && (name.contains(".dylib") || name.contains(".so")))
                || name == "onnxruntime.dll";
            if is_lib
                && best
                    .as_ref()
                    .is_none_or(|b| b.as_os_str().len() < path.as_os_str().len())
            {
                best = Some(path);
            }
        }
    }
    best
}

fn load_rgba(path: &Path) -> Arc<RgbaImage> {
    Arc::new(image::open(path).expect("image").to_rgba8())
}

/// 采样工作进程的峰值内存（KB），直到 `stop`
fn sample_peak(pid: u32, stop: Arc<AtomicBool>, peak: Arc<AtomicU64>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            if let Some(kb) = rss_kb(pid) {
                peak.fetch_max(kb, Ordering::Relaxed);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    })
}

#[derive(Debug, Deserialize)]
struct Golden {
    lines: Vec<GoldenLine>,
}

#[derive(Debug, Deserialize)]
struct GoldenLine {
    #[serde(rename = "box")]
    poly: [[f32; 2]; 4],
    text: String,
}

fn aabb(p: &[[f32; 2]; 4]) -> [f32; 4] {
    let xs = p.map(|q| q[0]);
    let ys = p.map(|q| q[1]);
    [
        xs.iter().copied().fold(f32::INFINITY, f32::min),
        ys.iter().copied().fold(f32::INFINITY, f32::min),
        xs.iter().copied().fold(f32::NEG_INFINITY, f32::max),
        ys.iter().copied().fold(f32::NEG_INFINITY, f32::max),
    ]
}

fn iou(a: &[f32; 4], b: &[f32; 4]) -> f32 {
    let iw = (a[2].min(b[2]) - a[0].max(b[0])).max(0.0);
    let ih = (a[3].min(b[3]) - a[1].max(b[1])).max(0.0);
    let inter = iw * ih;
    let area = |r: &[f32; 4]| (r[2] - r[0]) * (r[3] - r[1]);
    inter / (area(a) + area(b) - inter).max(1e-6)
}

/// 一张图的比对结果
#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    /// 官方行数
    total: usize,
    /// 文字一致且 IoU ≥ 0.9（严格指标）
    strict: usize,
    /// 其中四个顶点也完全相同
    exact: usize,
    /// 文字一致且 IoU ≥ 0.5
    loose: usize,
    /// 检测召回：有本地框 IoU ≥ 0.5（不看文字）
    detected: usize,
    /// 本地行数与其中没有官方框 IoU ≥ 0.5 的行数（多检）
    ours: usize,
    extra: usize,
}

impl std::ops::AddAssign for Tally {
    fn add_assign(&mut self, o: Self) {
        self.total += o.total;
        self.strict += o.strict;
        self.exact += o.exact;
        self.loose += o.loose;
        self.detected += o.detected;
        self.ours += o.ours;
        self.extra += o.extra;
    }
}

fn pct(n: usize, d: usize) -> f64 {
    if d == 0 { 100.0 } else { n as f64 * 100.0 / d as f64 }
}

/// 逐行比对：每条官方结果找 IoU 最高的本地结果；另查每条本地结果是否对得上某条官方结果。返回计数与差异说明
fn compare(golden: &Golden, ours: &[TextLine]) -> (Tally, Vec<String>) {
    let mut t = Tally {
        total: golden.lines.len(),
        ours: ours.len(),
        ..Tally::default()
    };
    let mut diffs = Vec::new();
    for g in &golden.lines {
        let gb = aabb(&g.poly);
        let best = ours
            .iter()
            .map(|l| (iou(&gb, &aabb(&l.poly)), l))
            .max_by(|a, b| a.0.total_cmp(&b.0));
        match best {
            Some((v, l)) => {
                t.detected += (v >= 0.5) as usize;
                t.loose += (v >= 0.5 && l.text == g.text) as usize;
                if v >= 0.9 && l.text == g.text {
                    t.strict += 1;
                    t.exact += (l.poly == g.poly) as usize;
                } else {
                    diffs.push(format!("  官方 {:?} ↔ 本地 {:?}（IoU {v:.2}）", g.text, l.text));
                }
            }
            None => diffs.push(format!("  官方 {:?} ↔ 本地缺失", g.text)),
        }
    }
    for l in ours {
        let lb = aabb(&l.poly);
        if !golden.lines.iter().any(|g| iou(&lb, &aabb(&g.poly)) >= 0.5) {
            t.extra += 1;
            diffs.push(format!("  本地多出 {:?}", l.text));
        }
    }
    if ours.len() != golden.lines.len() {
        diffs.push(format!("  行数：官方 {} / 本地 {}", golden.lines.len(), ours.len()));
    }
    (t, diffs)
}

const IMAGES: [&str; 6] = [
    "general_ocr_002",
    "translate-light",
    "ocr-dark",
    "settings-providers-light",
    "clipboard-light",
    "dictionary-dark",
];

fn image_path(name: &str) -> PathBuf {
    if name == "general_ocr_002" {
        fixtures().join("general_ocr_002.jpg")
    } else {
        repo_root().join("docs/images").join(format!("{name}.png"))
    }
}

#[tokio::test]
#[ignore = "需要真实的 ONNX Runtime 动态库与 PP-OCRv6 模型（WISP_LOCAL_TEST_DIR）"]
async fn ppocr_matches_the_official_pipeline_and_meets_latency_targets() {
    let models = Models::new();
    let cfg = models.config();
    let mut all = Tally::default();
    for tier in [PpOcrTier::Tiny, PpOcrTier::Small] {
        let tier_name = match tier {
            PpOcrTier::Tiny => "tiny",
            PpOcrTier::Small => "small",
        };
        // 冷启动：进程启动 + ping + 加载动态库 + 建会话 + 一次识别
        let screenshot = load_rgba(&image_path("translate-light"));
        let t0 = Instant::now();
        let worker = OrtWorker::spawn(&StdLauncher, &cfg).expect("spawn");
        let spawn_ms = t0.elapsed().as_millis();
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicU64::new(0));
        let sampler = sample_peak(worker.pid(), Arc::clone(&stop), Arc::clone(&peak));
        worker
            .ppocr(tier, Arc::clone(&screenshot), 2048)
            .await
            .expect("cold ppocr");
        let cold_ms = t0.elapsed().as_millis();
        // 热路径
        let mut hot = Vec::new();
        for _ in 0..5 {
            let t = Instant::now();
            worker
                .ppocr(tier, Arc::clone(&screenshot), 2048)
                .await
                .expect("hot ppocr");
            hot.push(t.elapsed().as_millis());
        }
        hot.sort_unstable();
        // 黄金样例
        let mut report = Vec::new();
        for name in IMAGES {
            let img = load_rgba(&image_path(name));
            let t = Instant::now();
            let lines = worker.ppocr(tier, img, 2048).await.expect("ppocr");
            let ms = t.elapsed().as_millis();
            let golden: Golden = serde_json::from_slice(
                &std::fs::read(fixtures().join(format!("{name}.{tier_name}.json"))).expect("golden"),
            )
            .expect("golden json");
            let (t, diffs) = compare(&golden, &lines);
            all += t;
            report.push(format!(
                "{name}: {}/{} 行一致（其中框完全相同 {}；IoU ≥ 0.5 时 {}，检测召回 {}，多检 {}），{ms} ms",
                t.strict, t.total, t.exact, t.loose, t.detected, t.extra
            ));
            report.extend(diffs);
        }
        stop.store(true, Ordering::Relaxed);
        let _ = sampler.join();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let idle = rss_kb(worker.pid());
        eprintln!(
            "[{tier_name}] 进程启动 + ping {spawn_ms} ms；冷启动（含首次识别 1200×846）{cold_ms} ms；热路径中位 {} ms（最快 {} ms）；\
             工作进程峰值 {} MB，闲置常驻 {} MB",
            hot[hot.len() / 2],
            hot[0],
            peak.load(Ordering::Relaxed) / 1024,
            idle.map_or("?".to_owned(), |kb| (kb / 1024).to_string()),
        );
        for line in report {
            eprintln!("{line}");
        }
        let pid = worker.pid();
        worker.shutdown(Duration::from_secs(1));
        assert!(rss_kb(pid).is_none(), "工作进程已退出");
    }
    eprintln!(
        "合计：文字一致且 IoU ≥ 0.9 {}/{}（{:.1}%，其中框完全相同 {}）；文字一致且 IoU ≥ 0.5 {}（{:.1}%）；检测召回 {}（{:.1}%）；多检 {}/{}",
        all.strict,
        all.total,
        pct(all.strict, all.total),
        all.exact,
        all.loose,
        pct(all.loose, all.total),
        all.detected,
        pct(all.detected, all.total),
        all.extra,
        all.ours,
    );
    // 预处理与后处理已与官方产线逐像素 / 逐框对齐（见 ocr_opencv_parity），M5 上 6 张图 × 两档全部一致；
    // 余量只留给其他 CPU 上 ONNX Runtime 内核的浮点差异（个别边界框的取舍）
    assert!(
        all.strict * 100 >= all.total * 99,
        "文字一致且 IoU ≥ 0.9 的比例低于 99%：{}/{}",
        all.strict,
        all.total
    );
    assert!(
        all.detected * 1000 >= all.total * 995 && all.extra * 100 <= all.total,
        "检测召回低于 99.5% 或多检超过 1%：召回 {}/{}，多检 {}",
        all.detected,
        all.total,
        all.extra
    );
}

/// 重新生成黄金样例时用：把测试实际交给识别的像素（`image` 解码的 JPEG）写成 PNG，供 `generate.py` 读取。
/// JPEG 解码器之间有几个灰度级的差异（libjpeg-turbo 与 zune-jpeg），与识别产线无关，所以官方产线读的是同一份像素
#[test]
#[ignore = "只在重新生成黄金样例时运行：WISP_PPOCR_EXPORT_DIR=<目录>"]
fn export_decoded_inputs_for_the_generator() {
    let dir = PathBuf::from(std::env::var_os("WISP_PPOCR_EXPORT_DIR").expect("WISP_PPOCR_EXPORT_DIR"));
    let img = image::open(image_path("general_ocr_002")).expect("image").to_rgb8();
    img.save(dir.join("general_ocr_002.png")).expect("write png");
}

#[tokio::test]
#[ignore = "需要真实的 ONNX Runtime 动态库与 PP-DocLayoutV3 模型（WISP_LOCAL_TEST_DIR）"]
async fn layout_detection_on_a_document() {
    let models = Models::new();
    let worker = OrtWorker::spawn(&StdLauncher, &models.config()).expect("spawn");
    let path = std::env::var_os("WISP_LAYOUT_TEST_IMAGE")
        .map_or_else(|| repo_root().join("docs/images/ocr-light.png"), PathBuf::from);
    let img = load_rgba(&path);
    let stop = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(AtomicU64::new(0));
    let sampler = sample_peak(worker.pid(), Arc::clone(&stop), Arc::clone(&peak));
    let t0 = Instant::now();
    let boxes = worker
        .detect(Arc::clone(&img), CancellationToken::new())
        .await
        .expect("layout");
    let cold = t0.elapsed().as_millis();
    let t1 = Instant::now();
    let again = worker.layout(Arc::clone(&img)).await.expect("layout");
    let hot = t1.elapsed().as_millis();
    stop.store(true, Ordering::Relaxed);
    let _ = sampler.join();
    eprintln!(
        "版面检测 {}×{}：{} 个区域；冷（含加载 130 MB 模型）{cold} ms，热 {hot} ms；峰值 {} MB",
        img.width(),
        img.height(),
        boxes.len(),
        peak.load(Ordering::Relaxed) / 1024
    );
    for b in &boxes {
        eprintln!(
            "  {:>16} {:.2} {:?} order={:?}",
            b.label,
            b.score,
            b.bbox.map(|v| v as i32),
            b.order
        );
    }
    assert!(!boxes.is_empty());
    assert_eq!(boxes, again, "同一张图两次结果相同");
    worker.shutdown(Duration::from_secs(1));
}

/// 直接用 reqwest 调 llama-server 的 OpenAI 兼容接口（desktop 里由 wisp-llm 实现）
struct HttpVlm {
    url: String,
    key: Option<String>,
    http: reqwest::Client,
}

impl VlmBackend for HttpVlm {
    fn recognize(&self, req: VlmRequest, _cancel: CancellationToken) -> mpsc::Receiver<StreamEvent> {
        use base64::Engine;
        let (tx, rx) = mpsc::channel(4);
        let data = base64::engine::general_purpose::STANDARD.encode(req.png.as_slice());
        let body = serde_json::json!({
            "model": "PaddleOCR-VL-1.6",
            "messages": [{"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{data}")}},
                {"type": "text", "text": req.prompt},
            ]}],
            "temperature": 0,
            "max_tokens": req.max_tokens,
            "stream": false,
        });
        let mut rb = self
            .http
            .post(format!("{}/v1/chat/completions", self.url.trim_end_matches('/')))
            .json(&body);
        if let Some(k) = &self.key {
            rb = rb.bearer_auth(k);
        }
        tokio::spawn(async move {
            let ev = match rb.send().await {
                Ok(resp) if resp.status().is_success() => match resp.json::<serde_json::Value>().await {
                    Ok(v) => {
                        let text = v
                            .pointer("/choices/0/message/content")
                            .and_then(|c| c.as_str())
                            .unwrap_or_default();
                        let _ = tx.send(StreamEvent::Delta { text: text.to_owned() }).await;
                        StreamEvent::Done {
                            elapsed_ms: 0,
                            usage: None,
                            truncated: None,
                            cached: false,
                            no_text: false,
                        }
                    }
                    Err(_) => StreamEvent::Error(LlmError::new(ErrorCode::Protocol, "bad json")),
                },
                Ok(resp) => StreamEvent::Error(LlmError::new(ErrorCode::Upstream, format!("status {}", resp.status()))),
                Err(_) => StreamEvent::Error(LlmError::new(ErrorCode::Network, "network")),
            };
            let _ = tx.send(ev).await;
        });
        rx
    }
}

#[tokio::test]
#[ignore = "需要已启动的 llama-server（WISP_VL_TEST_URL）与 PP-DocLayoutV3（WISP_LOCAL_TEST_DIR）"]
async fn vl_pipeline_against_a_running_llama_server() {
    let url = std::env::var("WISP_VL_TEST_URL").expect("WISP_VL_TEST_URL");
    let vlm = Arc::new(HttpVlm {
        url,
        key: std::env::var("WISP_VL_TEST_KEY").ok().filter(|k| !k.is_empty()),
        // 走 crate 自己的构造器（装好 rustls 的 CryptoProvider）；llama-server 在本机，不经系统代理
        http: wisp_local::net::client_builder()
            .no_proxy()
            .build()
            .expect("http client"),
    });
    let models = Models::new();
    let worker = Arc::new(OrtWorker::spawn(&StdLauncher, &models.config()).expect("spawn"));
    let path = std::env::var_os("WISP_LAYOUT_TEST_IMAGE")
        .map_or_else(|| repo_root().join("docs/images/translate-light.png"), PathBuf::from);
    let img = load_rgba(&path);
    for mode in [OcrMode::Markdown, OcrMode::Lines] {
        let (w, v, i) = (Arc::clone(&worker), Arc::clone(&vlm), Arc::clone(&img));
        let t0 = Instant::now();
        let mut rx = wisp_local::stream::spawn(CancellationToken::new(), move |sink, cancel| async move {
            vl::run(i, PipelineOptions::for_mode(mode), &*w, &*v, &sink, &cancel).await
        });
        let mut first_ms = None;
        let mut deltas = 0;
        let mut text = String::new();
        let mut err = None;
        while let Some(ev) = rx.recv().await {
            match ev {
                StreamEvent::Delta { text: t } => {
                    first_ms.get_or_insert(t0.elapsed().as_millis());
                    deltas += 1;
                    text.push_str(&t);
                }
                StreamEvent::Error(e) => err = Some(e),
                _ => {}
            }
        }
        eprintln!(
            "VL {mode:?}：{} ms，首段 {:?} ms，{deltas} 段，{} 字",
            t0.elapsed().as_millis(),
            first_ms,
            text.chars().count()
        );
        if std::env::var_os("WISP_VL_TEST_PRINT").is_some() {
            eprintln!("{text}");
        }
        assert!(err.is_none(), "{err:?}");
        assert!(!text.trim().is_empty());
    }
    if let Ok(worker) = Arc::try_unwrap(worker) {
        worker.shutdown(Duration::from_secs(1));
    }
}
