//! 冒烟模式（`WISP_SMOKE=1`）：托盘就绪 → 等 Panel 页面加载完成并静置 1.5 s → 模拟一次 `TranslateText` 并等 fake
//! 流结束 → 收起面板并静置 1 s → **连续触发 100 次**（prepare→shown 的 p50 / p95、ready 超时次数、卡死次数）→ 打开并关闭设置窗口 →
//! 静置 10 s 测主进程的会话后空闲 → 写 `smoke-report.json` → 退出。不需要任何系统权限，判定交给 `scripts/perf/smoke-gate.mjs`。
//!
//! **隔离**：冒烟模式下 bootstrap 把配置 / 数据目录放到系统临时目录（`Paths::smoke`），钥匙串用进程内存，预置一个假的
//! Provider 与 Key（配合 `WISP_FAKE_LLM=1` 的假模型），不弹引导、不检查更新、不改开机自启——不会碰用户的真实数据。
//!
//! **报告位置**：`WISP_SMOKE_REPORT` 指定的路径；否则为可执行文件所在的 `target/` 目录（`target/debug/wisp-desktop`、
//! `target/debug/bundle/macos/Wisp.app/...` 都会落到 `target/smoke-report.json`）；不在 `target/` 下时写到当前目录的
//! `target/smoke-report.json`。
//!
//! **日志**：冒烟模式下日志插件写到隔离目录（`Paths::smoke().log_dir`，不混进用户的 wisp.log）；结束时复制到报告旁边的
//! `smoke-report.log`（同名、扩展名换成 `.log`，字段 `log_file`），再删除隔离目录，CI 可以一并归档。
//!
//! **新鲜度**：setup 一开始就删掉上一次的报告与日志（本次中途崩溃时不会留下旧结果被当成本次的）；报告带上 `pid` 与
//! `process_started_ms`（本进程启动的 Unix 毫秒），smoke-gate 应只接受 `process_started_ms` 不早于自己启动 Wisp 的时刻的
//! 报告。冒烟模式下 `lib.rs` 不注册 single-instance 插件，本机已有 Wisp 在运行时冒烟进程也照常
//! 运行，不会被转发给已有实例。
//!
//! **报告格式**（`schema = 1`，`scripts/perf/smoke-gate.mjs` 按此读取）。`startup.stages` 是按时间先后排列的数组（同名阶段各记一条）：
//! ```json
//! { "schema": 1, "ok": true, "failures": [], "ts": 1760000000000, "pid": 4242, "process_started_ms": 1759999991000,
//!   "os": "macos", "os_version": "26.4",
//!   "arch": "aarch64", "version": "0.1.0", "debug_build": true, "fake_llm": true, "duration_ms": 9000,
//!   "log_file": "target/smoke-report.log",
//!   "startup": { "tray_ready_ms": 420, "pre_main_ms": 35,
//!                "stages": [ { "name": "settings_loaded", "ms": 35 }, { "name": "services", "ms": 120 },
//!                            { "name": "tray_ready", "ms": 420 } ] },
//!   "panel_prewarm_ms": 600,   // 面板页面加载完成（panel_page_ready）的时刻，不再是窗口建好的时刻
//!   "first_translate": { "shown_ms": 80, "first_token_ms": 150, "finished": true, "elapsed_ms": 900,
//!                        "ready_timeout": false, "prepare_to_shown_ms": 30 },
//!   "triggers": { "count": 100, "shown": 100, "ready_timeouts": 0, "hangs": 0,
//!                 "prepare_to_shown_ms": { "p50": 30, "p95": 48, "max": 61 },
//!                 "trigger_to_shown_ms": { "p50": 35, "p95": 55, "max": 70 },
//!                 "first": { "index": 1, "shown": true, "ready_timeout": false, "hang": false,
//!                            "prepare_to_shown_ms": 41, "trigger_to_shown_ms": 52 },
//!                 "last": { "index": 100, "shown": true, "ready_timeout": false, "hang": false,
//!                           "prepare_to_shown_ms": 29, "trigger_to_shown_ms": 33 } },
//!   "overlay": { "count": 5, "shown": 5, "p50": 80, "p95": 96, "max": 101 },   // 假帧建 Overlay → 第一块屏显示
//!   "settings_window": { "opened": true, "open_ms": 180, "closed": true, "close_ms": 20 },
//!   "windows": { "peak": 3, "end": 1, "labels_end": ["panel"] },
//!   "post_session_idle": { "seconds": 10.0, "cpu_pct": 0.02, "wakeups_per_s": 0.4 },
//!   "memory_metric": "phys_footprint",
//!   "rss_kb": { "after_startup": 52000, "after_triggers": 61000, "end": 60000 } }
//! ```
//! `startup.*_ms` 都从**内核创建进程**的时刻算起（含 exec / dyld / 静态初始化；`pre_main_ms` 是其中到 `run()` 第一条
//! 语句的部分，取不到内核时刻时为 null、计时退回从 `run()` 起算）。`rss_kb` 的口径见 `memory_metric`
//! （`wisp_platform::process::MEMORY_METRIC`：macOS phys_footprint、Windows 工作集、Linux VmRSS），
//! 字段名沿用 `rss_kb` 以兼容 schema 1。
//! 「卡死」= 触发后 [`TRIGGER_DEADLINE`] 内既没有 Shown 也没有 ReadyTimeout。`triggers.first` / `triggers.last` 是第 1 次与
//! 最后一次（第 100 次；提前中止时为最后尝试的那次）触发各自的结果，供「第 1 次与第 100 次都在预算内」判定。
//! 整个流程有 [`WATCHDOG`] 兜底：超时也会写出（标记失败的）报告再退出。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tauri::Manager;
use wisp_types::{HideReason, ModelEntry, ModelRef, Protocol, Provider, Quirks, Timeouts};

use crate::app::perf::{PerfMarks, TraceSnapshot, duration_ms};
use crate::bus::{Command, TriggerOrigin};
use crate::settings::SettingsService;
use crate::state::AppState;

/// 连续触发的次数
pub const TRIGGERS: usize = 100;
/// 单次触发等待 Shown / ReadyTimeout 的上限（超过记为卡死）
pub const TRIGGER_DEADLINE: Duration = Duration::from_secs(3);
/// 等待面板预热的上限
pub const PREWARM_DEADLINE: Duration = Duration::from_secs(15);
/// 等待第一次翻译（fake 流）结束的上限
pub const FIRST_TRANSLATE_DEADLINE: Duration = Duration::from_secs(20);
/// 整个冒烟流程的上限
pub const WATCHDOG: Duration = Duration::from_secs(240);
/// 冒烟用的假 Provider / 模型
pub const PROVIDER_ID: &str = "smoke";
pub const MODEL_ID: &str = "smoke-model";
/// 会话结束后静置测量的时长
pub const POST_SESSION_IDLE: Duration = Duration::from_secs(10);
/// 面板页面加载完成（`panel_page_ready`）之后、首次翻译之前的静置：页面加载完成时外壳的视图分块与 JIT 还是冷的、
/// 前端的空闲预热渲染还没做，加载完 1 ms 就唤出必然撞上 40 ms 的 ready 超时——真实使用里第一次唤出至少在启动几秒之后。
/// 与 window lab 的回收后首次唤出同一口径（页面加载并静置 1.5 s）
pub const FIRST_TRIGGER_SETTLE: Duration = Duration::from_millis(1500);
/// 首次翻译结束、收起面板之后，连续触发之前的静置：连续触发测的是热页面的再次唤出，启动后的第一次唤出
/// 由首次翻译代表（smoke-gate 同样硬判）。流式收尾与收起动画还在进行时立刻唤出，在没有 GPU 的 CI 虚拟机上页面主线程被
/// 它们占满，第 1 次触发撞上 40 ms 的 ready 兜底（macOS 15 runner 与 Linux Xvfb 上都出现过，都是第 1 次）；真实使用里
/// 收起之后再唤出至少隔着一次按键。第 1 次触发照样是这一页第一次切换会话（JIT 冷的那条路径），smoke-gate 仍按「启动后第一次
/// 唤出」硬判它
pub const HOT_LOOP_SETTLE: Duration = Duration::from_millis(1000);
/// 每次触发翻译的固定文本
const SAMPLE: &str = "Wisp smoke test: the quick brown fox jumps over the lazy dog.";

/// 是否处于冒烟模式
pub fn enabled() -> bool {
    std::env::var("WISP_SMOKE").is_ok_and(|v| v == "1")
}

/// 删掉上一次的报告与日志（setup 一开始调用）：本次中途崩溃或被转发走时，不会留下旧结果被当成本次的
pub fn discard_stale_report() {
    let report = report_path();
    for path in [report.with_extension("log"), report] {
        match std::fs::remove_file(&path) {
            Ok(()) => log::info!("smoke: removed the previous {}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("smoke: cannot remove the previous {}: {e}", path.display()),
        }
    }
}

/// 本进程启动时刻（Unix 毫秒）
fn process_started_ms() -> i64 {
    let uptime = i64::try_from(super::runtime::since_process_start().as_millis()).unwrap_or(0);
    crate::settings::fsio::unix_now_ms().saturating_sub(uptime)
}

/// 预置冒烟用的设置：一个指向本机不可达地址的假 Provider（真实请求会立即失败，fake 模型不发请求）、默认模型与假 Key
pub fn seed(settings: &SettingsService) {
    let provider = Provider {
        id: PROVIDER_ID.to_owned(),
        name: "Smoke".to_owned(),
        protocol: Protocol::OpenAiChat,
        base_url: "http://127.0.0.1:9/v1".to_owned(),
        headers: std::collections::BTreeMap::new(),
        quirks: Quirks::default(),
        models: vec![ModelEntry::new(MODEL_ID)],
        timeouts: Timeouts::default(),
        template_id: None,
    };
    let model = ModelRef {
        provider_id: PROVIDER_ID.to_owned(),
        model_id: MODEL_ID.to_owned(),
    };
    let result = settings.update(&mut |s| {
        s.providers = vec![provider.clone()];
        s.defaults.translate = Some(model.clone());
        s.defaults.ocr = Some(model.clone());
        s.general.launch_at_login = false;
        s.general.update_policy = wisp_types::UpdatePolicy::Off;
        s.clipboard.enabled = false;
    });
    if let Err(e) = result {
        log::warn!("smoke: cannot seed settings: {}", e.message);
    }
    if let Err(e) = settings.keychain().set(PROVIDER_ID, "smoke-key") {
        log::warn!("smoke: cannot seed key: {}", e.message);
    }
    if let Err(e) = settings.complete_onboarding() {
        log::warn!("smoke: cannot mark onboarding done: {}", e.message);
    }
}

/// 启动冒烟流程（在 setup 末尾调用）
pub fn start(app: &tauri::AppHandle) {
    log::info!("smoke mode: starting");
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let started = Instant::now();
        let mut report = Report::default();
        let outcome = tokio::time::timeout(WATCHDOG, run(&handle, &mut report)).await;
        if outcome.is_err() {
            report.fail(format!("watchdog: smoke run exceeded {} s", WATCHDOG.as_secs()));
        }
        report.set("duration_ms", json!(duration_ms(started.elapsed())));
        finish(&handle, report);
    });
}

/// 冒烟报告（逐步填充）
#[derive(Debug, Default)]
struct Report {
    fields: serde_json::Map<String, Value>,
    failures: Vec<String>,
}

impl Report {
    fn set(&mut self, key: &str, value: Value) {
        self.fields.insert(key.to_owned(), value);
    }

    fn fail(&mut self, reason: impl Into<String>) {
        let reason = reason.into();
        log::warn!("smoke: {reason}");
        self.failures.push(reason);
    }
}

async fn run(app: &tauri::AppHandle, report: &mut Report) {
    let Some(state) = app.try_state::<AppState>() else {
        report.fail("app state is not managed");
        return;
    };
    let perf = std::sync::Arc::clone(&state.perf);

    // 1. 启动阶段
    let startup = perf.startup();
    let tray_ready = startup
        .iter()
        .find(|(name, _)| *name == "tray_ready")
        .map_or(Value::Null, |(_, ms)| json!(ms));
    report.set(
        "startup",
        json!({
            "tray_ready_ms": tray_ready,
            "pre_main_ms": super::runtime::pre_main().map(duration_ms),
            "stages": startup_stages(startup),
        }),
    );
    let rss_after_startup = rss_kb();
    let mut peak_windows = app.webview_windows().len();

    // 2. 等 Panel 预热：窗口建好（panel_prewarmed）之后页面还要 100–160 ms 才加载完，等到页面加载完成
    //    （panel_page_ready）再做首次翻译——否则首次唤出必然撞上 ready 超时，而且这个超时原先没有计入报告
    let prewarm = wait_for(&perf, PREWARM_DEADLINE, |p| {
        p.startup()
            .into_iter()
            .find(|(n, _)| *n == "panel_page_ready" || *n == "panel_prewarm_failed")
    })
    .await;
    match prewarm {
        Some(("panel_page_ready", ms)) => report.set("panel_prewarm_ms", json!(ms)),
        Some((_, _)) => {
            report.set("panel_prewarm_ms", Value::Null);
            report.fail("panel prewarm failed");
        }
        None => {
            report.set("panel_prewarm_ms", Value::Null);
            report.fail("panel prewarm did not finish in time");
        }
    }

    // 3. 一次完整的 TranslateText，等 fake 流结束（页面加载完成后先静置 FIRST_TRIGGER_SETTLE）
    tokio::time::sleep(FIRST_TRIGGER_SETTLE).await;
    let t0 = Instant::now();
    let first = trigger(&state, &perf, FIRST_TRANSLATE_DEADLINE, |s| s.finished).await;
    match &first {
        Some(s) => {
            report.set(
                "first_translate",
                json!({
                    "shown_ms": s.shown_ms,
                    "first_token_ms": s.first_token_ms,
                    "finished": s.finished,
                    "elapsed_ms": duration_ms(t0.elapsed()),
                    // 首次翻译的 ready 超时同样计入（smoke-gate 把它加进 ready_timeouts）
                    "ready_timeout": s.ready_timeout,
                    "prepare_to_shown_ms": s.prepare_to_shown_ms(),
                }),
            );
            if s.shown_ms.is_none() {
                report.fail("first translate: panel was never shown");
            }
            if !s.finished {
                report.fail("first translate: stream did not finish");
            }
        }
        None => {
            report.set("first_translate", Value::Null);
            report.fail("first translate: no perf trace (orchestrator did not handle TranslateText)");
        }
    }
    hide_panel(&state);
    peak_windows = peak_windows.max(app.webview_windows().len());
    tokio::time::sleep(HOT_LOOP_SETTLE).await;

    // 4. 连续触发 100 次（收起之后静置 HOT_LOOP_SETTLE）
    let mut prepare_to_shown = Vec::with_capacity(TRIGGERS);
    let mut trigger_to_shown = Vec::with_capacity(TRIGGERS);
    let (mut shown, mut timeouts, mut hangs, mut untraced) = (0usize, 0usize, 0usize, 0usize);
    let (mut first, mut last) = (Value::Null, Value::Null);
    for i in 0..TRIGGERS {
        let sent = Instant::now();
        let snap = trigger(&state, &perf, TRIGGER_DEADLINE, |s| {
            s.shown_ms.is_some() || s.ready_timeout
        })
        .await;
        let elapsed = duration_ms(sent.elapsed());
        let sample = TriggerSample::of(i + 1, snap.as_ref(), elapsed);
        match snap {
            Some(s) if s.shown_ms.is_some() => {
                shown += 1;
                if s.ready_timeout {
                    timeouts += 1;
                }
                if let Some(d) = s.prepare_to_shown_ms() {
                    prepare_to_shown.push(d);
                }
                trigger_to_shown.push(elapsed);
            }
            Some(s) if s.ready_timeout => timeouts += 1,
            Some(_) => hangs += 1,
            None => {
                hangs += 1;
                untraced += 1;
            }
        }
        if i == 0 {
            first = sample.to_json();
        }
        last = sample.to_json();
        hide_panel(&state);
        peak_windows = peak_windows.max(app.webview_windows().len());
        // 前几次都没有任何追踪：编排器 / 窗口层还没接上，没必要再等 300 s
        if i == 2 && untraced == 3 {
            report.fail("triggers: no perf traces at all; aborting the remaining triggers");
            hangs += TRIGGERS - 3;
            break;
        }
        // 留一帧给隐藏动画收尾，模拟真实的连续触发间隔
        tokio::time::sleep(Duration::from_millis(16)).await;
    }
    report.set(
        "triggers",
        json!({
            "count": TRIGGERS,
            "shown": shown,
            "ready_timeouts": timeouts,
            "hangs": hangs,
            "prepare_to_shown_ms": summary(&mut prepare_to_shown),
            "trigger_to_shown_ms": summary(&mut trigger_to_shown),
            "first": first,
            "last": last,
        }),
    );
    if hangs > 0 {
        report.fail(format!("triggers: {hangs} hang(s)"));
    }
    let rss_after_triggers = rss_kb();

    // 4b. Overlay 上屏（目标 ≤ 150 ms）：用假帧（`Frame::from_rgba`，不需要屏幕录制权限）走一遍
    //     「建 Overlay 窗口 → 显示」，量每次从开始建窗口到第一块屏显示出来的耗时。真实截图另有 SCK 的捕获时间
    //     （热的约 40 ms），由 perf 日志的 overlay_ms 覆盖
    report.set("overlay", overlay_samples(&state).await);
    peak_windows = peak_windows.max(app.webview_windows().len());

    // 5. 打开并关闭设置窗口
    let label = crate::windows::settings_window::SETTINGS_LABEL;
    let t_open = Instant::now();
    state.windows.open_settings(None);
    let opened = poll(Duration::from_secs(8), || {
        app.get_webview_window(label)
            .is_some_and(|w| w.is_visible().unwrap_or(false))
    })
    .await;
    let open_ms = duration_ms(t_open.elapsed());
    peak_windows = peak_windows.max(app.webview_windows().len());
    let mut closed = false;
    let mut close_ms = None;
    if opened {
        let t_close = Instant::now();
        if let Some(w) = app.get_webview_window(label) {
            crate::windows::native::destroy_window(&w);
        }
        closed = poll(Duration::from_secs(5), || app.get_webview_window(label).is_none()).await;
        close_ms = Some(duration_ms(t_close.elapsed()));
        if !closed {
            report.fail("settings window did not close");
        }
    } else {
        report.fail("settings window did not open");
    }
    report.set(
        "settings_window",
        json!({ "opened": opened, "open_ms": opened.then_some(open_ms), "closed": closed, "close_ms": close_ms }),
    );

    let labels: Vec<String> = app.webview_windows().keys().cloned().collect();
    report.set(
        "windows",
        json!({ "peak": peak_windows, "end": labels.len(), "labels_end": labels }),
    );

    // 6. 会话之后的空闲（用过面板之后不能留下周期性的工作）：所有窗口都已隐藏 / 关闭，再静置
    //    POST_SESSION_IDLE，记主进程的 CPU 与空闲唤醒（WebKit 进程由 measure-macos.sh --exercise 覆盖）
    report.set("post_session_idle", post_session_idle().await);
    report.set("memory_metric", json!(wisp_platform::process::MEMORY_METRIC));
    report.set(
        "rss_kb",
        json!({
            "after_startup": rss_after_startup,
            "after_triggers": rss_after_triggers,
            "end": rss_kb(),
        }),
    );
}

/// Overlay 上屏的采样次数
const OVERLAY_SAMPLES: usize = 5;

/// 用假帧建 Overlay 并显示 [`OVERLAY_SAMPLES`] 次：`{ count, shown, p50, p95, max }`（毫秒，从建窗口到第一块屏显示）
async fn overlay_samples(state: &AppState) -> Value {
    let windows = std::sync::Arc::clone(&state.windows);
    let monitors = state.platform.screen.monitors();
    if monitors.is_empty() {
        return Value::Null;
    }
    let mut samples = Vec::with_capacity(OVERLAY_SAMPLES);
    for i in 0..OVERLAY_SAMPLES {
        let session_id = 90_000 + i as u64;
        let w = std::sync::Arc::clone(&windows);
        let monitors = monitors.clone();
        let took = tauri::async_runtime::spawn_blocking(move || {
            let t0 = Instant::now();
            let session = crate::windows::OverlaySession {
                session_id,
                monitors: monitors.clone(),
                mode: wisp_types::OverlayMode::Ocr,
            };
            w.create_overlays(&session).ok()?;
            let first = &monitors[0];
            let frame = wisp_capture::Frame::from_rgba(first.clone(), 2, 2, vec![0; 16]);
            let shown = w.overlay_show(session_id, &first.id, Some(&frame)).is_ok();
            let ms = duration_ms(t0.elapsed());
            w.destroy_overlays();
            shown.then_some(ms)
        })
        .await
        .ok()
        .flatten();
        if let Some(ms) = took {
            samples.push(ms);
        }
        // 等这次的 Overlay 销毁完再建下一次（与真实的连续截图间隔相当）
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    let shown = samples.len();
    let mut s = summary(&mut samples);
    if let Some(o) = s.as_object_mut() {
        o.insert("count".to_owned(), json!(OVERLAY_SAMPLES));
        o.insert("shown".to_owned(), json!(shown));
    }
    s
}

/// 单次触发的结果（报告里的 `triggers.first` / `triggers.last`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerSample {
    /// 第几次（从 1 起）
    pub index: usize,
    pub shown: bool,
    pub ready_timeout: bool,
    /// 既没有 Shown 也没有 ReadyTimeout（或根本没有追踪）
    pub hang: bool,
    pub prepare_to_shown_ms: Option<u32>,
    /// 发出触发到 Shown 的毫秒数（没有 Shown 时为 None）
    pub trigger_to_shown_ms: Option<u32>,
}

impl TriggerSample {
    /// 由一次触发的追踪快照得出；`elapsed_ms` 为发出触发到等待结束的耗时
    pub fn of(index: usize, snap: Option<&TraceSnapshot>, elapsed_ms: u32) -> Self {
        let shown = snap.is_some_and(|s| s.shown_ms.is_some());
        let ready_timeout = snap.is_some_and(|s| s.ready_timeout);
        Self {
            index,
            shown,
            ready_timeout,
            hang: !shown && !ready_timeout,
            prepare_to_shown_ms: snap.and_then(TraceSnapshot::prepare_to_shown_ms),
            trigger_to_shown_ms: shown.then_some(elapsed_ms),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "index": self.index,
            "shown": self.shown,
            "ready_timeout": self.ready_timeout,
            "hang": self.hang,
            "prepare_to_shown_ms": self.prepare_to_shown_ms,
            "trigger_to_shown_ms": self.trigger_to_shown_ms,
        })
    }
}

/// 发一次 `TranslateText` 并等待满足条件的追踪；没有任何追踪时返回 None，超时返回最后的快照
async fn trigger(
    state: &AppState,
    perf: &PerfMarks,
    deadline: Duration,
    done: impl Fn(&TraceSnapshot) -> bool,
) -> Option<TraceSnapshot> {
    let after = perf.last_id();
    let started = tokio::time::Instant::now();
    state.bus.send(Command::TranslateText {
        text: SAMPLE.to_owned(),
        origin: TriggerOrigin::Cli,
    });
    // 编排器没有为这次触发开始任何追踪：不必等满整个期限
    wait_for(perf, TRIGGER_DEADLINE.min(deadline), |p| p.first_after(after)).await?;
    let remaining = deadline.saturating_sub(started.elapsed());
    let found = wait_for(perf, remaining, |p| p.first_after(after).filter(|s| done(s))).await;
    found.or_else(|| perf.first_after(after))
}

/// 等到 `probe` 返回 Some（每次有新的埋点就重新检查），超时返回 None
async fn wait_for<T>(perf: &PerfMarks, deadline: Duration, probe: impl Fn(&PerfMarks) -> Option<T>) -> Option<T> {
    let mut rx = perf.subscribe();
    let until = tokio::time::Instant::now() + deadline;
    loop {
        rx.borrow_and_update();
        if let Some(v) = probe(perf) {
            return Some(v);
        }
        match tokio::time::timeout_at(until, rx.changed()).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) | Err(_) => return probe(perf),
        }
    }
}

/// 轮询某个条件（只在冒烟模式里用）
async fn poll(deadline: Duration, cond: impl Fn() -> bool) -> bool {
    let until = Instant::now() + deadline;
    while Instant::now() < until {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    cond()
}

fn hide_panel(state: &AppState) {
    if let Some(p) = state.windows.panel_state() {
        state.windows.panel_hide_now(p.session_id, HideReason::Other);
    }
}

/// 启动阶段 → 按时间先后排列的 `[{ "name", "ms" }]`（同一毫秒内保持记录顺序；同名阶段各记一条，不互相覆盖）
pub fn startup_stages(mut stages: Vec<(&'static str, u32)>) -> Value {
    stages.sort_by_key(|(_, ms)| *ms);
    Value::Array(
        stages
            .into_iter()
            .map(|(name, ms)| json!({ "name": name, "ms": ms }))
            .collect(),
    )
}

/// p50 / p95 / max（最近秩法）；没有样本时为 null
pub fn summary(samples: &mut [u32]) -> Value {
    if samples.is_empty() {
        return json!({ "p50": null, "p95": null, "max": null });
    }
    samples.sort_unstable();
    json!({
        "p50": percentile(samples, 0.50),
        "p95": percentile(samples, 0.95),
        "max": samples.last(),
    })
}

/// 最近秩百分位（`sorted` 已升序且非空）
pub fn percentile(sorted: &[u32], q: f64) -> u32 {
    let n = sorted.len();
    let rank = (q * n as f64).ceil() as usize;
    sorted[rank.clamp(1, n) - 1]
}

/// 写报告并退出
fn finish(app: &tauri::AppHandle, mut report: Report) {
    let version = app.package_info().version.to_string();
    report.set("schema", json!(1));
    report.set("ok", json!(report.failures.is_empty()));
    report.set("failures", json!(report.failures));
    report.set("ts", json!(crate::settings::fsio::unix_now_ms()));
    report.set("pid", json!(std::process::id()));
    report.set("process_started_ms", json!(process_started_ms()));
    report.set("os", json!(std::env::consts::OS));
    report.set("os_version", json!(super::runtime::os_version()));
    report.set("arch", json!(std::env::consts::ARCH));
    report.set("version", json!(version));
    report.set("debug_build", json!(cfg!(debug_assertions)));
    report.set(
        "fake_llm",
        json!(std::env::var("WISP_FAKE_LLM").is_ok_and(|v| v == "1")),
    );
    let path = report_path();
    let log_file = keep_log(&path);
    report.set("log_file", log_file.map_or(Value::Null, |p| json!(p.to_string_lossy())));
    let bytes = serde_json::to_vec_pretty(&Value::Object(report.fields)).unwrap_or_default();
    let code = match path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| crate::settings::fsio::write_atomic(&path, &bytes))
    {
        Ok(()) => {
            log::info!("smoke report written to {}", path.display());
            0
        }
        Err(e) => {
            log::error!("smoke report could not be written to {}: {e}", path.display());
            1
        }
    };
    // 清理隔离目录（日志已复制到报告旁边；Windows 上日志文件仍被日志插件打开，删不掉的部分留给系统清理临时目录）
    if let Some(root) = super::paths::Paths::smoke().config_dir.parent()
        && let Err(e) = std::fs::remove_dir_all(root)
    {
        log::debug!("smoke: isolated directory not fully removed: {e}");
    }
    app.exit(code);
}

/// 把隔离目录里的冒烟日志复制到报告旁边（`<报告名>.log`），返回复制后的路径；失败只记日志
fn keep_log(report: &Path) -> Option<PathBuf> {
    log::logger().flush();
    let source = super::paths::Paths::smoke()
        .log_dir
        .join(format!("{}.log", super::logging::FILE_STEM));
    let target = report.with_extension("log");
    if let Some(dir) = target.parent()
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        log::warn!("smoke: cannot create {}: {e}", dir.display());
        return None;
    }
    match std::fs::copy(&source, &target) {
        Ok(_) => Some(target),
        Err(e) => {
            log::warn!("smoke: cannot copy the log to {}: {e}", target.display());
            None
        }
    }
}

/// 报告路径（见模块说明）
pub fn report_path() -> PathBuf {
    report_path_for(
        std::env::var_os("WISP_SMOKE_REPORT"),
        std::env::current_exe().ok().as_deref(),
        &std::env::current_dir().unwrap_or_default(),
    )
}

/// [`report_path`] 的实现：`explicit` 为 `WISP_SMOKE_REPORT`，`exe` 为可执行文件路径，`cwd` 为当前目录
pub fn report_path_for(explicit: Option<std::ffi::OsString>, exe: Option<&Path>, cwd: &Path) -> PathBuf {
    if let Some(p) = explicit.filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    exe.and_then(|e| e.ancestors().find(|a| a.file_name().is_some_and(|n| n == "target")))
        .map_or_else(|| cwd.join("target"), Path::to_path_buf)
        .join("smoke-report.json")
}

/// 静置 [`POST_SESSION_IDLE`]：主进程的 CPU 占用（单核百分比）与每秒空闲唤醒（只有 macOS 有，其他平台 null）。
/// 静置前先等 2 s，让隐藏 / 关闭窗口的收尾（退出动画、WebView 内存目标切换）做完
async fn post_session_idle() -> Value {
    use wisp_platform::process::{cpu_time, idle_wakeups};
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (cpu0, wake0, t0) = (cpu_time(), idle_wakeups(), Instant::now());
    tokio::time::sleep(POST_SESSION_IDLE).await;
    let secs = t0.elapsed().as_secs_f64();
    let cpu_pct = cpu0
        .zip(cpu_time())
        .map(|(a, b)| round2(b.saturating_sub(a).as_secs_f64() / secs * 100.0));
    #[allow(clippy::cast_precision_loss)] // 唤醒次数远小于 2^52
    let wakeups_per_s = wake0
        .zip(idle_wakeups())
        .map(|(a, b)| round2(b.saturating_sub(a) as f64 / secs));
    json!({ "seconds": round2(secs), "cpu_pct": cpu_pct, "wakeups_per_s": wakeups_per_s })
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// 主进程常驻内存（KiB，口径见 `wisp_platform::process::MEMORY_METRIC`）；取不到时为 None
pub fn rss_kb() -> Option<u64> {
    wisp_platform::process::memory_kb()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_use_nearest_rank() {
        let mut v: Vec<u32> = (1..=100).collect();
        let s = summary(&mut v);
        assert_eq!(s["p50"], 50);
        assert_eq!(s["p95"], 95);
        assert_eq!(s["max"], 100);
        assert_eq!(percentile(&[7], 0.95), 7);
        assert_eq!(summary(&mut [])["p95"], Value::Null);
        let mut unsorted = vec![30, 10, 20];
        assert_eq!(summary(&mut unsorted)["p50"], 20);
    }

    #[test]
    fn rss_is_measured_on_this_platform() {
        assert!(rss_kb().is_some_and(|kb| kb > 1000));
    }

    #[test]
    fn report_path_lands_in_target() {
        let cwd = Path::new("/work");
        let report = Path::new("target").join("smoke-report.json");
        // cargo 构建的二进制与 .app 包都在 target/ 下
        let exe = Path::new("/repo/target/debug/wisp-desktop");
        assert_eq!(report_path_for(None, Some(exe), cwd), Path::new("/repo").join(&report));
        let bundled = Path::new("/repo/target/release/bundle/macos/Wisp.app/Contents/MacOS/wisp-desktop");
        assert_eq!(
            report_path_for(None, Some(bundled), cwd),
            Path::new("/repo").join(&report)
        );
        // 不在 target/ 下：当前目录的 target/
        assert_eq!(
            report_path_for(None, Some(Path::new("/opt/wisp/wisp-desktop")), cwd),
            cwd.join(&report)
        );
        assert_eq!(report_path_for(None, None, cwd), cwd.join(&report));
        // WISP_SMOKE_REPORT 优先（空值忽略）
        assert_eq!(
            report_path_for(Some("/tmp/r.json".into()), Some(exe), cwd),
            PathBuf::from("/tmp/r.json")
        );
        assert_eq!(
            report_path_for(Some("".into()), Some(exe), cwd),
            Path::new("/repo").join(&report)
        );
    }

    #[test]
    fn startup_stages_are_an_ordered_list() {
        let stages = startup_stages(vec![
            ("tray_ready", 420),
            ("settings_loaded", 35),
            ("panel_prewarm_failed", 600),
            ("services", 120),
            ("panel_prewarm_failed", 900),
        ]);
        let names: Vec<&str> = stages
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|s| s["name"].as_str())
            .collect();
        assert_eq!(
            names,
            [
                "settings_loaded",
                "services",
                "tray_ready",
                "panel_prewarm_failed",
                "panel_prewarm_failed"
            ]
        );
        assert_eq!(stages[0]["ms"], 35);
        assert_eq!(stages[4]["ms"], 900, "同名阶段不互相覆盖");
    }

    fn snap(prepare_ms: Option<u32>, shown_ms: Option<u32>, ready_timeout: bool) -> TraceSnapshot {
        TraceSnapshot {
            id: 1,
            origin: TriggerOrigin::Cli,
            session_id: Some(1),
            method: None,
            selection_ms: None,
            prepare_ms,
            ready_ms: None,
            shown_ms,
            first_token_ms: None,
            ready_timeout,
            overlay_ms: None,
            selection_result: None,
            window_ms: None,
            key_ms: None,
            page: None,
            finished: false,
            elapsed_now_ms: 0,
        }
    }

    #[test]
    fn first_and_last_trigger_samples_are_reported() {
        let shown = snap(Some(12), Some(40), false);
        let s = TriggerSample::of(1, Some(&shown), 52);
        assert!(s.shown && !s.hang && !s.ready_timeout);
        assert_eq!(s.prepare_to_shown_ms, Some(28));
        assert_eq!(s.trigger_to_shown_ms, Some(52));
        let json = s.to_json();
        assert_eq!(json["index"], 1);
        assert_eq!(json["trigger_to_shown_ms"], 52);
        // 卡死 / 没有追踪：延迟为 null，标记 hang
        let hung = TriggerSample::of(100, None, 3000);
        assert!(hung.hang && !hung.shown);
        assert_eq!(hung.to_json()["trigger_to_shown_ms"], Value::Null);
        // ready 超时但最终显示：两者都记
        let late = snap(Some(10), Some(90), true);
        let s = TriggerSample::of(100, Some(&late), 95);
        assert!(s.shown && s.ready_timeout && !s.hang);
    }

    #[test]
    fn process_start_time_is_in_the_past() {
        let started = process_started_ms();
        let now = crate::settings::fsio::unix_now_ms();
        assert!(started <= now && started > now - 24 * 60 * 60 * 1000);
    }

    #[test]
    fn seeded_settings_are_complete() {
        let (_dir, svc) = crate::settings::service::tests::test_service();
        seed(&svc);
        let s = svc.get();
        assert!(s.provider(PROVIDER_ID).is_some());
        assert_eq!(
            s.defaults.translate.as_ref().map(|m| m.model_id.as_str()),
            Some(MODEL_ID)
        );
        assert!(svc.keychain().has(PROVIDER_ID));
        assert!(!svc.needs_onboarding());
    }
}
