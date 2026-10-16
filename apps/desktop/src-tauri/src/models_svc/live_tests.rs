//! 真机集成验证（`#[ignore]`）：不启动 GUI，直接经模型服务跑真实的推理进程。
//!
//! - 翻译：翻译槽位冷启动（`Phase{loading_model}`）→ 合成服务商（地址与随机 key 由服务填入）→ wisp-llm 流式翻译；再来一次走
//!   热路径；
//! - 文字识别：ORT 槽位（Wisp 自己的二进制以工作进程模式运行）→ PP-OCRv6 Small；冷、热各一次；
//! - 生命周期：闲置释放之后进程消失、pid 文件删除；重新加载后 `shutdown` 不留进程。
//!
//! 需要的东西都只从环境变量读：`WISP_LOCAL_TEST_DIR`（下载好的官方原件：`raw/` 与解包后的 `rt/`，布局见 `.gitignore`
//! 忽略的测试素材目录）、`WISP_TEST_DESKTOP_EXE`（可选：带 `--wisp-worker` 入口的 Wisp 可执行文件，缺省取与测试二进制同一
//! target 目录下的 `wisp-desktop`，需要先 `cargo build -p wisp-desktop`）。模型目录按 `Store` 的布局用硬链接在
//! `WISP_LOCAL_TEST_DIR/store-live/` 下搭好（不占额外空间），测试结束时删除。测得的数据写到
//! `WISP_LOCAL_TEST_DIR/live-metrics.json`（只有耗时、内存与计数，不含识别与翻译的内容）。
//!
//! 测试里的启动器不经守护进程直接启动 llama-server（守护进程是 Wisp 自己的二进制的另一种模式，测试二进制没有这个入口；
//! 守护进程本身由平台层的测试覆盖），停止时关 stdin、等 1 s 后强制结束。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use wisp_llm::{CancellationToken, LlmClient, Purpose};
use wisp_local::catalog::{self, Target};
use wisp_local::launch::{LaunchSpec, Launcher, ManagedChild};
use wisp_local::store::{INSTALLED_FILE, InstalledFile, InstalledManifest, Store};
use wisp_prompts::{Detection, PromptCtx, PromptProfile, PromptTask};
use wisp_types::{Lang, LocalModelId, LocalRuntimeId, OcrMode, RequestId, StreamEvent, StreamPhase};

use super::{ModelsService, PlatformLauncher, SlotKind};
use crate::orchestrator::plan::{chat_request, offline_translate};

/// 推理进程（llama-server）不经守护进程启动；ORT 工作进程用真正的 Wisp 二进制
struct TestLauncher {
    exe: PathBuf,
}

impl Launcher for TestLauncher {
    fn launch(&self, mut spec: LaunchSpec) -> std::io::Result<Box<dyn ManagedChild>> {
        spec.guard = false;
        PlatformLauncher.launch(spec)
    }

    fn current_exe(&self) -> std::io::Result<PathBuf> {
        Ok(self.exe.clone())
    }

    fn write_pid_file(&self, path: &Path, pid: u32) -> std::io::Result<()> {
        PlatformLauncher.write_pid_file(path, pid)
    }
}

/// 硬链接（符号链接按它指向的文件链接）
fn link(src: &Path, dst: &Path) {
    let real = std::fs::canonicalize(src).expect("source exists");
    std::fs::create_dir_all(dst.parent().expect("parent")).expect("mkdir");
    let _ = std::fs::remove_file(dst);
    std::fs::hard_link(&real, dst).expect("hard link");
}

fn manifest(store: &Store, dir: &Path, names: &[&str]) {
    let files = names
        .iter()
        .map(|n| InstalledFile {
            name: (*n).to_owned(),
            size: std::fs::metadata(dir.join(n)).expect("file").len(),
            sha256: String::new(),
        })
        .collect();
    store
        .write_manifest(
            dir,
            &InstalledManifest {
                catalog_revision: catalog::CATALOG_REVISION,
                files,
                installed_at: 0,
            },
        )
        .expect("manifest");
    assert!(dir.join(INSTALLED_FILE).is_file());
}

/// 按 `Store` 的布局搭模型目录：llama.cpp 与 ONNX Runtime 的运行时、Index-Translate 2B、PP-OCRv6 Small
fn prepare(base: &Path, root: &Path) -> Store {
    let target = Target::current().expect("a supported target");
    let store = Store::new(root.to_path_buf());
    // llama.cpp：发布包解出的全部顶层文件（llama-server 与它的动态库）
    let llama_src = base.join("rt/llama-b11669/llama-b11669");
    let llama_dir = store.runtime_dir(LocalRuntimeId::LlamaCpp);
    for entry in std::fs::read_dir(&llama_src).expect("llama.cpp runtime") {
        let entry = entry.expect("entry");
        if std::fs::metadata(entry.path()).is_ok_and(|m| m.is_file()) {
            link(&entry.path(), &llama_dir.join(entry.file_name()));
        }
    }
    let llama_main = catalog::runtime_asset(LocalRuntimeId::LlamaCpp, target)
        .expect("asset")
        .main;
    manifest(&store, &llama_dir, &[llama_main]);
    // ONNX Runtime：只要动态库
    let ort_asset = catalog::runtime_asset(LocalRuntimeId::OnnxRuntime, target).expect("asset");
    let ort_dir = store.runtime_dir(LocalRuntimeId::OnnxRuntime);
    let ort_src = base
        .join("rt/ort-1.28.3")
        .read_dir()
        .expect("ort runtime")
        .flatten()
        .map(|e| e.path().join("lib").join(ort_asset.main))
        .find(|p| p.is_file())
        .expect("ort dylib");
    link(&ort_src, &ort_dir.join(ort_asset.main));
    manifest(&store, &ort_dir, &[ort_asset.main]);
    // 模型：下载地址里的仓库名与文件名对应 raw/ 下的原件
    for model in [LocalModelId::IndexTranslate2b, LocalModelId::PpOcrV6Small] {
        let dir = store.model_dir(model);
        let mut names = Vec::new();
        for f in catalog::model(model).files {
            let rest = f.model_scope.split("/models/").nth(1).expect("ms url");
            let mut parts = rest.splitn(5, '/');
            let (_org, repo, _resolve, _rev, file) = (
                parts.next(),
                parts.next().expect("repo"),
                parts.next(),
                parts.next(),
                parts.next().expect("file"),
            );
            link(&base.join("raw").join(repo).join(file), &dir.join(f.name));
            names.push(f.name);
        }
        manifest(&store, &dir, &names);
    }
    store
}

/// 进程还在不在（`ps -p`）
fn alive(pid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-p", &pid.to_string()])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// 常驻内存（KB，`ps -o rss=`）
fn rss_kb(pid: u32) -> Option<u64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// 读完一条流：（首个 Delta 的耗时, 总耗时, 文本长度, 阶段, 终止事件）
async fn drain(
    mut rx: mpsc::Receiver<StreamEvent>,
) -> (Option<Duration>, Duration, usize, Vec<StreamPhase>, StreamEvent) {
    let t0 = Instant::now();
    let mut first = None;
    let mut chars = 0;
    let mut phases = Vec::new();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(180), rx.recv())
            .await
            .expect("stream timed out")
            .expect("stream ended without a terminal event");
        match &ev {
            StreamEvent::Delta { text } if !text.is_empty() => {
                first.get_or_insert_with(|| t0.elapsed());
                chars += text.chars().count();
            }
            StreamEvent::Phase { phase } => phases.push(*phase),
            _ => {}
        }
        if ev.is_terminal() {
            return (first, t0.elapsed(), chars, phases, ev);
        }
    }
}

/// 毫秒数（JSON 用 u64）
fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn slot_pid(svc: &ModelsService, kind: SlotKind) -> Option<u32> {
    match &svc.slots.get(kind).inner.lock().phase {
        super::slots::Phase::Ready(l) => Some(l.pid),
        _ => None,
    }
}

const SOURCE: &str = "Wisp stays out of your way: it runs with almost no CPU, wakes up in milliseconds, \
    and streams the translation right next to your cursor.";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "需要真实的推理运行时与模型（WISP_LOCAL_TEST_DIR）与 Wisp 可执行文件，会启动 llama-server 与 ORT 工作进程"]
async fn live_translation_and_ppocr_through_the_models_service() {
    let base = PathBuf::from(std::env::var("WISP_LOCAL_TEST_DIR").expect("WISP_LOCAL_TEST_DIR"));
    let exe = std::env::var_os("WISP_TEST_DESKTOP_EXE").map_or_else(
        || {
            let me = std::env::current_exe().expect("test binary");
            me.parent()
                .and_then(Path::parent)
                .expect("target dir")
                .join("wisp-desktop")
        },
        PathBuf::from,
    );
    assert!(exe.is_file(), "build the app first: cargo build -p wisp-desktop");
    let root = base.join("store-live");
    let _ = std::fs::remove_dir_all(&root);
    let store = prepare(&base, &root);

    let (_cfg, settings) = crate::settings::service::tests::test_service();
    let svc = ModelsService::with_parts(
        Arc::clone(&settings),
        None,
        root.clone(),
        Arc::new(TestLauncher { exe }),
    );
    *svc.idle_override.lock() = Some(Duration::from_secs(4));
    let llm = LlmClient::new(wisp_llm::ClientConfig {
        user_agent: "Wisp/test".to_owned(),
        caps: settings.caps_store(),
    });
    let mut metrics = serde_json::Map::new();

    // ---- 翻译：冷启动 → 热路径 ----
    let translate = |svc: &Arc<ModelsService>| {
        let resolved = offline_translate(LocalModelId::IndexTranslate2b);
        let style = wisp_prompts::builtin_styles().into_iter().next().expect("natural");
        let built = wisp_prompts::build_profile(
            &PromptTask::Translate {
                style: &style,
                dict: false,
            },
            &PromptCtx {
                text: SOURCE,
                source: Detection {
                    lang: Lang::En,
                    low_confidence: false,
                },
                target: Lang::ZhCn,
                glossary: &[],
                reasoning_model: false,
            },
            PromptProfile::IndexTranslate,
        );
        let req = chat_request(&RequestId::new(), &resolved, None, built, Purpose::Translate);
        svc.stream_translate(req, llm.clone(), CancellationToken::new())
    };
    let (first, total, chars, phases, end) = drain(translate(&svc)).await;
    assert!(matches!(end, StreamEvent::Done { .. }), "{end:?}");
    assert_eq!(phases, vec![StreamPhase::LoadingModel], "冷启动先报加载中");
    assert!(chars > 10, "有译文");
    metrics.insert("translate_cold_first_delta_ms".into(), first.map(ms).into());
    metrics.insert("translate_cold_total_ms".into(), ms(total).into());
    let (first, total, _, phases, end) = drain(translate(&svc)).await;
    assert!(matches!(end, StreamEvent::Done { .. }), "{end:?}");
    assert!(phases.is_empty(), "热路径不再加载");
    metrics.insert("translate_warm_first_delta_ms".into(), first.map(ms).into());
    metrics.insert("translate_warm_total_ms".into(), ms(total).into());
    let llama_pid = slot_pid(&svc, SlotKind::Translate).expect("translate slot loaded");
    metrics.insert("llama_server_rss_mb".into(), rss_kb(llama_pid).map(|k| k / 1024).into());
    assert!(store.pid_file("translate").is_file(), "pid 文件");

    // ---- 文字识别：PP-OCRv6 Small 冷 / 热 ----
    let png = std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../docs/images/translate-light.png"))
        .expect("test image");
    let image = Arc::new(wisp_capture::decode_image(&png).expect("decode"));
    let ocr = |svc: &Arc<ModelsService>| {
        svc.ocr_local(
            LocalModelId::PpOcrV6Small,
            OcrMode::Lines,
            Arc::clone(&image),
            2048,
            llm.clone(),
            CancellationToken::new(),
        )
    };
    let (_, total, chars, phases, end) = drain(ocr(&svc)).await;
    assert!(matches!(end, StreamEvent::Done { .. }), "{end:?}");
    assert_eq!(phases, vec![StreamPhase::LoadingModel]);
    assert!(chars > 20, "识别出了文字");
    metrics.insert("ppocr_small_cold_ms".into(), ms(total).into());
    let (_, total, _, phases, end) = drain(ocr(&svc)).await;
    assert!(matches!(end, StreamEvent::Done { .. }), "{end:?}");
    assert!(phases.is_empty());
    metrics.insert("ppocr_small_warm_ms".into(), ms(total).into());
    metrics.insert(
        "ocr_image".into(),
        format!("{}x{}", image.width(), image.height()).into(),
    );
    let ort_pid = slot_pid(&svc, SlotKind::Ort).expect("ort slot loaded");
    metrics.insert("ort_worker_rss_mb".into(), rss_kb(ort_pid).map(|k| k / 1024).into());

    // ---- 闲置释放：进程消失、pid 文件删除、压力监听卸载 ----
    let t0 = Instant::now();
    while (alive(llama_pid) || alive(ort_pid)) && t0.elapsed() < Duration::from_secs(20) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    metrics.insert("idle_release_observed_ms".into(), ms(t0.elapsed()).into());
    assert!(!alive(llama_pid), "llama-server 闲置后退出");
    assert!(!alive(ort_pid), "ORT 工作进程闲置后退出");
    assert!(!store.pid_file("translate").exists() && !store.pid_file("ort").exists());
    assert!(!svc.slots.any_active());
    assert!(svc.pressure.lock().is_none(), "没有槽位时不监听内存压力");

    // ---- 重新加载后退出：不留进程 ----
    let (_, total, _, _, end) = drain(translate(&svc)).await;
    assert!(matches!(end, StreamEvent::Done { .. }), "{end:?}");
    metrics.insert("translate_reload_total_ms".into(), ms(total).into());
    let (_, _, _, _, end) = drain(ocr(&svc)).await;
    assert!(matches!(end, StreamEvent::Done { .. }), "{end:?}");
    let pids = [
        slot_pid(&svc, SlotKind::Translate).expect("loaded"),
        slot_pid(&svc, SlotKind::Ort).expect("loaded"),
    ];
    let t0 = Instant::now();
    let shutdown = Arc::clone(&svc);
    tokio::task::spawn_blocking(move || shutdown.shutdown())
        .await
        .expect("shutdown");
    metrics.insert("shutdown_ms".into(), ms(t0.elapsed()).into());
    assert!(pids.iter().all(|p| !alive(*p)), "退出后没有残留进程");
    assert!(
        !store.run_dir().read_dir().is_ok_and(|mut d| d.next().is_some()),
        "pid 文件已删除"
    );

    std::fs::write(
        base.join("live-metrics.json"),
        serde_json::to_vec_pretty(&serde_json::Value::Object(metrics)).expect("json"),
    )
    .expect("write metrics");
    std::fs::remove_dir_all(&root).expect("clean up the test store");
}
