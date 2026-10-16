//! 窗口层流程实验台（**只在 debug 构建或带 `fake-llm` feature 的性能构建里编译**；release 性能构建也能出回收与
//! 泄漏的证据）：不经编排器，用真实的 `WindowManager` 跑一遍窗口层的
//! 主要路径，供窗口层回归与冒烟：面板 prepare → ready（真实前端）→ 再次触发（换位置）→ 固定交接 →
//! toast → 截图前隐藏 / 恢复 → Overlay 创建、冻结帧、显示与销毁 → 设置窗口打开与位置记忆 → 连续 prepare→shown 计时 →
//! 空闲卸载与重建 → 回收（空闲卸载的变体）与回收后首次唤出的计时 → 窗口泄漏检查。
//! 每一步以 `window_lab step=… ok|fail …` 输出一行日志。
//!
//! 用法（debug 构建，前端已 `pnpm -C apps/desktop build`、二进制带 `tauri/custom-protocol`）：
//! ```text
//! WISP_WINDOW_LAB=1 WISP_WINDOW_LAB_EXIT=1 target/debug/wisp-desktop
//! ```
//! 截图那一步需要屏幕录制权限；没有权限时退回 `Frame::from_rgba` 的假帧（不走原生冻结帧）。
//! 配置与数据在隔离的临时目录（`Paths::window_lab`，内存钥匙串，不弹引导；带 `WISP_WINDOW_LAB_EXIT` 时退出前删除），
//! 不碰用户真实的 `state.json`、剪贴板历史与钥匙串；日志照常写用户的 wisp.log。
//!
//! 可选：`WISP_WINDOW_LAB_RECYCLE_STRESS=<次数>`（连续回收的压力，第 9d 步）；`WISP_WINDOW_LAB_LATE_SHOW_SECS=<秒>`
//! （实验台跑完、面板隐藏这么久之后再唤出两次，记长时间隐藏后的 prepare→显示与首帧，见 [`late_show`]；不能与
//! `WISP_WINDOW_LAB_EXIT` 同时用）；`WISP_WINDOW_LAB_INPUT_PROBE=1`（外部脚本按日志里的 `input_probe=` 行注入按键 /
//! 移动指针）；`WISP_WINDOW_LAB_DRAG=1`（只跑「拖动后继续流式长高」，见 [`drag_suite`]，配合 `WISP_FAKE_LLM=1`）；
//! `WISP_WINDOW_LAB_DOCK_ICON=1`（只打开、关闭一次设置窗口，看切到 Regular 之后的 Dock 图标，见 [`dock_icon`]）。另有两种只做测量的模式（见 [`memory_phases`] / [`cold_rounds`]）：`WISP_WINDOW_LAB_MEMORY=1`（各阶段
//! 的内存，`WISP_WINDOW_LAB_NO_PREWARM=1` 关掉前端的空闲预热做 A/B）与 `WISP_WINDOW_LAB_COLD=1`（卸载且 WebKit 进程全部
//! 退出之后经总线触发的冷唤出与 Overlay 冷启动）。
//!
//! **隔离**：测量与实验台都请用单独的 identifier 构建（`pnpm tauri build --config '{"identifier":"io.github.yzlltyyh.wisp.lab"}'`），
//! 并经 LaunchServices 启动（`open -n`，WebKit 进程按负责进程精确归属）：与同一台机器上正在运行的其他 Wisp 同 identifier 时，
//! 别处启动的 Wisp 会经单实例锁转发进实验台进程（打开引导窗口、推迟回收），污染结果。跑完先查日志里有没有
//! `second instance started`，有就作废这一次。

use std::sync::Arc;
use std::time::{Duration, Instant};

use tauri::Manager;
use wisp_types::{
    AppRef, CopiedReason, HideReason, Lang, OverlayMode, PanelPayload, PanelTransition, PanelView, PinKind, PinState,
    Point, Rect, TaskKind, ToastPayload, TranslatePayload,
};

use super::manager::{AnchorSource, OverlaySession, PanelShowRequest, WindowManager};
use super::panel::PANEL_LABEL;
use crate::state::AppState;

/// 第 9c 步的轮数与每轮回收之后的静置时长
const RECYCLED_ROUNDS: u32 = 12;
const RECYCLED_SETTLE: Duration = Duration::from_millis(1500);
/// 非翻译视图回收后首次唤出的轮数
const VIEW_ROUNDS: u32 = 4;
/// 等一次回收的上限：其他 Wisp 窗口有短时活动（刚建的 pinned、刚关的设置窗口……）时产品会把回收推迟一轮（15 s，
/// `panel::RECYCLE_RETRY`），实验台要等得比它久，否则推迟会被误判为失败（5 s 的上限会造成间歇失败）
const RECYCLE_WAIT: Duration = Duration::from_secs(20);

/// 一组 prepare→shown 耗时的分位数
struct Percentiles {
    n: usize,
    p50: Option<Duration>,
    p95: Option<Duration>,
    max: Option<Duration>,
}

impl Percentiles {
    fn of(mut samples: Vec<Duration>) -> Self {
        samples.sort();
        let pct = |p: f64| {
            samples
                .get(((samples.len().max(1) - 1) as f64 * p).round() as usize)
                .copied()
        };
        Self {
            n: samples.len(),
            p50: pct(0.5),
            p95: pct(0.95),
            max: samples.last().copied(),
        }
    }
}

impl std::fmt::Display for Percentiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "n={} p50={:?} p95={:?} max={:?}",
            self.n, self.p50, self.p95, self.max
        )
    }
}

/// 是否开启
pub fn enabled() -> bool {
    std::env::var("WISP_WINDOW_LAB").is_ok_and(|v| v == "1")
}

fn step(name: &str, ok: bool, detail: impl std::fmt::Display) {
    if ok {
        log::info!("window_lab step={name} ok {detail}");
    } else {
        log::error!("window_lab step={name} fail {detail}");
    }
}

fn translate(text: &str) -> PanelPayload {
    PanelPayload::Translate(TranslatePayload {
        source_text: text.to_owned(),
        request_id: None,
        task: TaskKind::Translate,
        style_id: "natural".to_owned(),
        detected_lang: None,
        target_lang: Lang::ZhCn,
        dict_mode: false,
        long_text: None,
        not_ready: None,
        selection_method: None,
    })
}

fn request(view: PanelView, anchor: AnchorSource, payload: PanelPayload) -> PanelShowRequest {
    PanelShowRequest {
        view,
        payload,
        anchor,
        take_focus: matches!(view, PanelView::Input | PanelView::Clipboard),
        prev_app: None::<AppRef>,
        transition: PanelTransition::Enter,
    }
}

/// 等面板的某个会话真正显示（`panel_state` 可见），最多 `timeout`
async fn wait_shown(wm: &WindowManager, session_id: u64, timeout: Duration) -> Option<Duration> {
    let t0 = Instant::now();
    while t0.elapsed() < timeout {
        if wm.panel_state().is_some_and(|s| s.session_id == session_id) && wm.panel_shown(session_id) {
            return Some(t0.elapsed());
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    None
}

/// 在阻塞线程上 prepare（与编排器的用法一致）；找不到显示器时返回 0（之后的等待自然超时、记为失败）
async fn prepare(wm: &Arc<WindowManager>, req: PanelShowRequest) -> u64 {
    let wm = Arc::clone(wm);
    tauri::async_runtime::spawn_blocking(move || wm.panel_prepare(req))
        .await
        .ok()
        .flatten()
        .unwrap_or(0)
}

/// 在阻塞线程上数窗口（`window_census` 不在主线程时要同步等主线程，最长 500 ms；tokio 运行时只有 2 个
/// worker，不能在 async 任务里直接等）
async fn census(app: &tauri::AppHandle) -> Option<super::native::WindowCensus> {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || super::native::window_census(&app))
        .await
        .ok()
        .flatten()
}

/// 可选的「长时间隐藏后的唤出」（`WISP_WINDOW_LAB_LATE_SHOW_SECS=<秒>`，用来评估 backgroundThrottling
/// 的取值）：实验台跑完、面板隐藏 `secs` 秒之后（中间正好是会话后空闲测量的窗口）唤出两次——第一次落在隐藏了很久的
/// 页面上（`suspend` 策略下 WebContent 隐藏约 8 分钟后被挂起），第二次 3 s 之后、页面是热的，作对照。每次记 prepare
/// 调用本身、prepare 返回到 ready 并显示、到前端第一个 opacity > 0 的帧（`panel_painted`），以及是否走了 40 ms 的
/// ready 超时兜底。`WISP_WINDOW_LAB_LATE_SHOW_ROUNDS=<n>` 重复 n 轮（每轮都先隐藏 `secs` 秒），多取几个样本
async fn late_show(app: &tauri::AppHandle, wm: &Arc<WindowManager>, secs: u64) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let perf = Arc::clone(&state.perf);
    let rounds = std::env::var("WISP_WINDOW_LAB_LATE_SHOW_ROUNDS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(1)
        .max(1);
    for _ in 0..rounds {
        tokio::time::sleep(Duration::from_secs(secs)).await;
        for label in ["after_long_hide", "warm"] {
            if label == "warm" {
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            late_show_once(app, wm, &perf, label, secs).await;
        }
    }
}

/// [`late_show`] 的一次唤出与计时；之后由前端收尾隐藏
async fn late_show_once(
    app: &tauri::AppHandle,
    wm: &Arc<WindowManager>,
    perf: &crate::app::perf::PerfMarks,
    label: &str,
    secs: u64,
) {
    let trace = perf.begin(crate::bus::TriggerOrigin::Cli);
    let t0 = Instant::now();
    let s = prepare(
        wm,
        request(PanelView::Translate, AnchorSource::Cursor, translate("late show")),
    )
    .await;
    perf.bind_session(trace, s);
    perf.mark(trace, crate::app::perf::PerfStage::Prepare);
    let prepare_call = t0.elapsed();
    let shown = wait_shown(wm, s, Duration::from_secs(3)).await;
    // 前端在显示后的下一帧报 panel_painted
    let painted_wait = Instant::now();
    let mut snap = perf.snapshot(trace);
    while snap.as_ref().is_none_or(|t| t.shown_ms.is_none()) && painted_wait.elapsed() < Duration::from_secs(1) {
        tokio::time::sleep(Duration::from_millis(2)).await;
        snap = perf.snapshot(trace);
    }
    perf.finish(trace, app);
    // 下面两项都从调用 prepare 之前算起（含 prepare 调用本身）
    let ready = snap.as_ref().and_then(|t| t.ready_ms);
    let painted = snap.as_ref().and_then(|t| t.shown_ms);
    let timed_out = snap.as_ref().is_some_and(|t| t.ready_timeout);
    step(
        "late_show",
        shown.is_some() && painted.is_some(),
        format!(
            "{label} hidden_for={secs}s session={s} prepare_call={prepare_call:?} after_prepare_to_shown={shown:?} \
             ready_ms={ready:?} painted_ms={painted:?} ready_timeout={timed_out}"
        ),
    );
    wm.panel_request_hide(HideReason::Escape);
    let t0 = Instant::now();
    while wm.panel_state().is_some() && t0.elapsed() < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 外部驱动脚本注入按键的等待上限（`WISP_WINDOW_LAB_INPUT_PROBE`）
const INPUT_PROBE_WAIT: Duration = Duration::from_secs(6);

/// 可选的按键探针（「非激活的 Overlay 收得到键」）：装上本地按键监视器，输出 `window_lab input_probe=key` 请外部脚本注入一个
/// 无害的按键（F18，键码 79），等它送到 key 的 Overlay；之后 Wisp 在系统层面仍不能在前台
async fn lab_probe_keys(app: &tauri::AppHandle, name: &str) {
    const F18: u16 = 79;
    let probe = {
        let app = app.clone();
        move |on: bool| {
            let app = app.clone();
            tauri::async_runtime::spawn_blocking(move || super::native::lab_key_probe(&app, on))
        }
    };
    let _ = probe(true).await;
    log::info!("window_lab input_probe=key code={F18}");
    let t0 = Instant::now();
    let mut keys = Vec::new();
    while keys.is_empty() && t0.elapsed() < INPUT_PROBE_WAIT {
        tokio::time::sleep(Duration::from_millis(50)).await;
        keys = probe(true).await.unwrap_or_default();
    }
    // 再留一点时间让系统层面的激活（如果会发生）落定，再判断菜单栏
    tokio::time::sleep(Duration::from_millis(150)).await;
    keys.extend(probe(false).await.unwrap_or_default().into_iter().skip(keys.len()));
    let active = super::native::app_is_active(app);
    step(
        name,
        keys.iter().any(|&(code, overlay)| code == F18 && overlay) && active == Some(false),
        format!(
            "keys={keys:?} after {} ms; system_active={active:?}",
            t0.elapsed().as_millis()
        ),
    );
}

/// 可选（`WISP_WINDOW_LAB_INPUT_PROBE=1`）：多屏 Overlay 的按键焦点跟随指针（macOS 的 NSTrackingArea）。开发机通常只有
/// 一块屏，把主显示器一分为二当作两块「屏」各建一个 Overlay：左边是 key，输出 `window_lab input_probe=pointer x=… y=…`
/// 请外部脚本把指针移到右半边中央，右边的 Overlay 应当在指针进入时成为 key（不激活应用），按键随之送到它
async fn lab_pointer_follow(app: &tauri::AppHandle, wm: &Arc<WindowManager>, primary: &wisp_types::MonitorInfo) {
    let half = |id: &str, right: bool| {
        let mut m = primary.clone();
        m.id = id.to_owned();
        m.is_primary = !right;
        m.frame.width /= 2.0;
        m.work_area.width /= 2.0;
        m.physical_size.width /= 2;
        if right {
            m.frame.x += m.frame.width;
            m.work_area.x += m.work_area.width;
            m.physical_origin.x += i32::try_from(m.physical_size.width).unwrap_or(0);
        }
        m
    };
    let (left, right) = (half("lab-left", false), half("lab-right", true));
    let session = OverlaySession {
        session_id: 9002,
        monitors: vec![left.clone(), right.clone()],
        mode: OverlayMode::Ocr,
    };
    if let Err(e) = wm.create_overlays(&session) {
        step("overlay_pointer_follow", false, e.message);
        return;
    }
    for m in [&left, &right] {
        if let Err(e) = wm.overlay_show(9002, &m.id, None) {
            step("overlay_pointer_follow", false, e.message);
        }
    }
    let window_of = |id: &str| {
        app.webview_windows()
            .keys()
            .find(|l| wm.overlay_monitor(l).as_deref() == Some(id))
            .and_then(|l| app.get_webview_window(l))
    };
    // 指针移进某一半，等那一半的 Overlay 成为 key：(耗时, (系统层面在前台, key, 第一响应者是 WKWebView))
    let follow = |m: &wisp_types::MonitorInfo| {
        let (id, x, y) = (
            m.id.clone(),
            m.frame.x + m.frame.width / 2.0,
            m.frame.y + m.frame.height / 2.0,
        );
        async move {
            log::info!("window_lab input_probe=pointer x={x} y={y}");
            let t0 = Instant::now();
            while t0.elapsed() < INPUT_PROBE_WAIT {
                tokio::time::sleep(Duration::from_millis(5)).await;
                if let Some(state) = window_of(&id).as_ref().and_then(super::native::overlay_focus_state)
                    && state.1
                {
                    return Some((t0.elapsed(), state));
                }
            }
            None
        }
    };
    // 先让左半边成为 key（指针本来可能停在右半边：新出现在指针下的 Overlay 会直接收到 mouseEntered）
    let to_left = follow(&left).await;
    let to_right = follow(&right).await;
    step(
        "overlay_pointer_follow",
        to_left.is_some() && to_right.is_some_and(|(_, (active, key, responder))| !active && key && responder),
        format!("left={to_left:?} right={to_right:?}"),
    );
    let followed = to_right;
    if followed.is_some() {
        lab_probe_keys(app, "overlay_pointer_follow_keys").await;
        // commit 之后不再跟随：通知窗口层停止跟随，指针回到左半边，右边必须仍是 key、左边不抢
        wm.overlay_stop_following(9002);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let following = window_of(&left.id)
            .as_ref()
            .and_then(super::native::overlay_follows_pointer_now);
        let (x, y) = (
            left.frame.x + left.frame.width / 2.0,
            left.frame.y + left.frame.height / 2.0,
        );
        log::info!("window_lab input_probe=pointer x={x} y={y}");
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let left_state = window_of(&left.id)
            .as_ref()
            .and_then(super::native::overlay_focus_state);
        let right_state = window_of(&right.id)
            .as_ref()
            .and_then(super::native::overlay_focus_state);
        step(
            "overlay_no_follow_after_commit",
            following == Some(false)
                && left_state.is_some_and(|(_, key, _)| !key)
                && right_state.is_some_and(|(active, key, _)| !active && key),
            format!("following={following:?} left={left_state:?} right={right_state:?}"),
        );
    }
    wm.destroy_overlays();
    tokio::time::sleep(Duration::from_millis(300)).await;
}

/// 带 `WISP_WINDOW_LAB_EXIT=1` 时清理隔离目录并退出，返回 true
fn finish(app: &tauri::AppHandle) -> bool {
    if !std::env::var("WISP_WINDOW_LAB_EXIT").is_ok_and(|v| v == "1") {
        return false;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        // 清理隔离目录（日志在用户的日志目录，不在这里）
        if let Some(root) = crate::app::paths::Paths::window_lab().config_dir.parent()
            && let Err(e) = std::fs::remove_dir_all(root)
        {
            log::debug!("window lab: isolated directory not fully removed: {e}");
        }
        app.exit(0);
    });
    true
}

/// 一次带埋点的唤出（与编排器同一套 perf 阶段）：返回快照（prepare_ms ≈ 0：实验台直接 prepare，没有取词）
async fn traced_show(
    app: &tauri::AppHandle,
    wm: &Arc<WindowManager>,
    req: PanelShowRequest,
) -> Option<crate::app::perf::TraceSnapshot> {
    let perf = Arc::clone(&app.try_state::<AppState>()?.perf);
    let trace = perf.begin(crate::bus::TriggerOrigin::Cli);
    let s = prepare(wm, req).await;
    perf.bind_session(trace, s);
    perf.mark(trace, crate::app::perf::PerfStage::Prepare);
    wait_shown(wm, s, Duration::from_secs(3)).await?;
    // 首帧（panel_painted）与成为 key 在显示之后一两帧内到达
    let t0 = Instant::now();
    let wants_key = wm
        .panel_state()
        .is_some_and(|p| matches!(p.view, PanelView::Input | PanelView::Clipboard));
    let mut snap = perf.snapshot(trace);
    while snap
        .as_ref()
        .is_none_or(|t| t.shown_ms.is_none() || (wants_key && t.key_ms.is_none()))
        && t0.elapsed() < Duration::from_secs(1)
    {
        tokio::time::sleep(Duration::from_millis(2)).await;
        snap = perf.snapshot(trace);
    }
    perf.finish(trace, app);
    snap
}

/// 切到 Regular 之后的 Dock 图标（`WISP_WINDOW_LAB_DOCK_ICON=1`，macOS）：打开设置窗口（激活策略切到 Regular），等它显示
/// 之后确认 bundle 的 icns 已设为 `NSApp.applicationIconImage`、读回来是一张完整尺寸的图（agent 应用运行中切到 Regular 时，
/// 不设的话 Dock 显示通用占位图，见 `native::set_regular_activation`）；再关掉设置窗口，确认回到 Accessory。要在 .app 里运行
/// （`tauri build --debug` 出的包，单独的 identifier，经 `open -n` 启动），裸二进制没有 bundle 图标，记为失败。
/// `WISP_WINDOW_LAB_DOCK_ICON_OUT=<文件>`：把 Regular 时的应用图标（TIFF）写到那里，人工查看
async fn dock_icon(app: &tauri::AppHandle, wm: &Arc<WindowManager>) {
    let dump = std::env::var_os("WISP_WINDOW_LAB_DOCK_ICON_OUT").map(std::path::PathBuf::from);
    let label = super::settings_window::SETTINGS_LABEL;
    wm.open_settings(Some("general".to_owned()));
    let t0 = Instant::now();
    while app
        .get_webview_window(label)
        .is_none_or(|w| !w.is_visible().unwrap_or(false))
        && t0.elapsed() < Duration::from_secs(5)
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // 激活策略与图标都在主线程队列里，显示之后再等一会儿
    tokio::time::sleep(Duration::from_millis(500)).await;
    let regular = super::native::lab_app_icon(app, dump);
    step(
        "dock_icon_regular",
        regular
            .as_ref()
            .is_some_and(|(ok, d)| *ok && d.starts_with("policy=0 ")),
        regular.map_or_else(|| "unavailable".to_owned(), |(_, d)| d),
    );
    if let Some(w) = app.get_webview_window(label) {
        super::native::destroy_window(&w);
    }
    tokio::time::sleep(Duration::from_millis(800)).await;
    let after = super::native::lab_app_icon(app, None);
    step(
        "dock_icon_after_close",
        after.as_ref().is_some_and(|(_, d)| d.starts_with("policy=1 ")),
        after.map_or_else(|| "unavailable".to_owned(), |(_, d)| d),
    );
}

/// 拖动后继续流式长高（`WISP_WINDOW_LAB_DRAG=1`；配合 `WISP_FAKE_LLM=1` 才有流式输出，否则是未就绪卡片、不会长高）。
/// 真实前端 + 真实窗口：翻译面板流式输出开始后，按 `panel_drag_start` 的方式进入拖动状态（不交给系统拖动），由实验台
/// 每 30 ms `set_position` 一次、30 步（约 0.9 s，流式输出在这期间继续长高）把窗口挪到目标位置（窗口的移动事件 → 控制器
/// 跟随几何，与系统拖动走同一条路径），之后每 16 ms 采样一次窗口外框，直到流式输出结束、高度弹簧停下。拖动中窗口高度
/// 不变（`held`：变化推迟到松开，`deferred` 是拖动期间推迟的长高），松开后补上：
/// - `drag_follow_grow`：拖到工作区中部——之后继续长高，窗口左上角一直留在拖到的位置（Native 帧模式的逐帧弹簧不把它拉回
///   原锚点），卡片比拖动时高，会话几何与窗口外框一致；
/// - `drag_push_back`：拖到卡片底边贴近工作区底——之后长高伸出工作区时整体上移，底边停在工作区底（留 8），左边不动
async fn drag_suite(app: &tauri::AppHandle, wm: &Arc<WindowManager>) {
    const SOURCE: &str = "The quick brown fox jumps over the lazy dog, and then the panel is dragged while the \
        translation keeps streaming into it.";
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    if std::env::var("WISP_FAKE_LLM").is_ok_and(|v| v == "1") {
        crate::app::smoke::seed(&state.settings);
    }
    let controller = Arc::clone(wm.panel_controller());
    let margin = match controller.frame_mode() {
        wisp_types::FrameMode::Native => 0.0,
        wisp_types::FrameMode::Css => super::sizing::SHADOW_MARGIN,
    };
    for push in [false, true] {
        let name = if push { "drag_push_back" } else { "drag_follow_grow" };
        // 每一轮的原文不同：同一原文第二次命中缓存，译文一次到位、不再流式长高
        let source = format!("{SOURCE} (round {})", u8::from(push) + 1);
        let s = prepare(
            wm,
            request(PanelView::Translate, AnchorSource::Cursor, translate(&source)),
        )
        .await;
        if wait_shown(wm, s, Duration::from_secs(2)).await.is_none() {
            step(name, false, format!("session={s} not shown"));
            continue;
        }
        // 首 token 约 280 ms（假模型）：等流式输出开始
        tokio::time::sleep(Duration::from_millis(380)).await;
        let Some(win) = app.get_webview_window(PANEL_LABEL) else {
            step(name, false, "no panel window");
            return;
        };
        let frame = || super::native::window_frame(&win);
        let (Some(start), Some(snap0)) = (frame(), controller.snapshot()) else {
            step(name, false, "no frame / snapshot");
            continue;
        };
        let unit = super::native::DESKTOP_UNIT;
        let screen = snap0.screen.clone();
        let (top_limit, bottom_limit) = (screen.work.y + 8.0, screen.work.bottom() - 8.0);
        // 目标（卡片左上角，本屏逻辑坐标）：中部；或者卡片底边离工作区底只剩 4
        let card_target = if push {
            Point {
                x: screen.work.x + 260.0,
                y: bottom_limit - snap0.rect.height - 4.0,
            }
        } else {
            Point {
                x: screen.work.x + 200.0,
                y: top_limit + 140.0,
            }
        };
        let target = screen.to_desktop(
            Rect::new(card_target.x - margin, card_target.y - margin, 1.0, 1.0),
            unit,
        );
        if !controller.lab_begin_drag(s) {
            step(name, false, "drag did not start");
            continue;
        }
        // 拖动中窗口高度不变（流式输出照常长高，但窗口的变化推迟到松开：Native 的高度弹簧停在当前高度、Css 的阶梯推迟）
        let mut drag_heights: Vec<f64> = Vec::new();
        for i in 1..=30 {
            let k = f64::from(i) / 30.0;
            let (x, y) = (start.x + (target.x - start.x) * k, start.y + (target.y - start.y) * k);
            let moved = if unit == super::placement::DesktopUnit::Logical {
                win.set_position(tauri::LogicalPosition::new(x, y))
            } else {
                win.set_position(tauri::PhysicalPosition::new(x.round() as i32, y.round() as i32))
            };
            if let Err(e) = moved {
                log::warn!("window_lab drag: set_position failed: {e}");
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
            if let Some(f) = frame() {
                drag_heights.push(f.height);
            }
        }
        let held = drag_heights.iter().copied().fold(f64::NAN, f64::max)
            - drag_heights.iter().copied().fold(f64::NAN, f64::min)
            < 0.5;
        let height_at_drag = controller.snapshot().map_or(0.0, |p| p.rect.height);
        // 拖动期间推迟的长高（卡片的目标高度 − 拖动中窗口的高度，Css 帧模式扣掉上下边距）
        let deferred = height_at_drag + 2.0 * margin - drag_heights.iter().copied().fold(f64::NAN, f64::max);
        // 采样：跟随、停下 200 ms、流式结束与高度弹簧（合计约 1.8 s）
        let mut off_target: Option<Rect> = None;
        let mut samples = 0u32;
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_millis(1800) {
            if let Some(f) = frame() {
                samples += 1;
                let left_moved = (f.x - target.x).abs() > 1.0;
                // 不推回的场景：顶边一直在拖到的位置；推回的场景：顶边只会上移（不会跳回原锚点），左边不动
                let top_moved = if push {
                    f.y > target.y + 1.0
                } else {
                    (f.y - target.y).abs() > 1.0
                };
                if (left_moved || top_moved) && off_target.is_none() {
                    off_target = Some(f);
                }
            }
            tokio::time::sleep(Duration::from_millis(16)).await;
        }
        let (Some(end), Some(snap)) = (frame(), controller.snapshot()) else {
            step(name, false, "no frame / snapshot after the drag");
            continue;
        };
        let card = snap.screen.to_desktop(snap.rect, unit);
        let card_frame = screen.to_desktop(
            Rect::new(
                snap.rect.x - margin,
                snap.rect.y - margin,
                snap.rect.width + 2.0 * margin,
                1.0,
            ),
            unit,
        );
        let geometry_matches = (card_frame.x - end.x).abs() < 1.0
            && (controller.frame_mode() != wisp_types::FrameMode::Native
                || ((card.y - end.y).abs() < 1.0 && (card.height - end.height).abs() < 1.0));
        let grew = snap.rect.height > snap0.rect.height + 0.5;
        let placed = if push {
            (snap.rect.bottom() - bottom_limit).abs() < 1.0 && snap.rect.y < card_target.y - 0.5
        } else {
            (snap.rect.x - card_target.x).abs() < 1.0 && (snap.rect.y - card_target.y).abs() < 1.0
        };
        step(
            name,
            off_target.is_none() && geometry_matches && grew && placed && held,
            format!(
                "session={s} start={start:?} target={target:?} end={end:?} card={:?} height_before={} at_drag={height_at_drag} \
                 after={} samples={samples} off_target={off_target:?} geometry_matches={geometry_matches} grew={grew} \
                 placed={placed} held={held} deferred={deferred} drag_heights={drag_heights:?}",
                snap.rect, snap0.rect.height, snap.rect.height
            ),
        );
        hide_by_frontend(wm).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    step("done", true, "drag mode");
}

/// 用前端的退出流程隐藏面板，等它离屏
async fn hide_by_frontend(wm: &Arc<WindowManager>) {
    wm.panel_request_hide(HideReason::Escape);
    let t0 = Instant::now();
    while wm.panel_state().is_some() && t0.elapsed() < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn input_payload() -> PanelPayload {
    PanelPayload::Input(wisp_types::InputPayload {
        initial_text: String::new(),
        target_lang: None,
        hint: None,
    })
}

fn clipboard_payload() -> PanelPayload {
    PanelPayload::Clipboard(wisp_types::ClipboardPayload {
        page: wisp_types::ClipPage::empty(wisp_types::ClipFilter::All),
        paused: false,
        hint_bar: true,
        paste_on_double_click: false,
        unavailable: None,
        recording: wisp_types::ClipboardRecording::Recording,
        favorites_collapsed: false,
        queue_unavailable: None,
    })
}

/// 实验台对面板页面附加的初始化脚本（`WISP_WINDOW_LAB_NO_PREWARM`，预热开 / 关的 A/B）：页面加载后 5 s 内的
/// `requestIdleCallback` 按变体跳过——前端的空闲预热都排在这段时间里的空闲回调上（WebKit 没有原生实现时退回 `setTimeout`，
/// 与前端自己的退路相同；5 s 之后照常）：
/// - `1` / `all`：全部跳过；
/// - `views`：只跳过带 `timeout` 选项的——视图的 JIT 预热渲染（`shared/perf/prewarm.tsx` 的 `schedulePrewarm`、翻译视图的
///   `prewarmOnce`）；
/// - `fonts`：只跳过不带选项的——字体与 Intl 的 `warmUpWhenIdle`、剪贴板行尾时间格式器。
///
/// 没有设置时返回 None
pub fn panel_init_script() -> Option<String> {
    let skip = match std::env::var("WISP_WINDOW_LAB_NO_PREWARM").ok()?.trim() {
        "1" | "all" => "true",
        "views" => "!!(opts && opts.timeout)",
        "fonts" => "!(opts && opts.timeout)",
        _ => return None,
    };
    Some(format!(
        r"(() => {{
  const t0 = performance.now();
  const native = typeof window.requestIdleCallback === 'function' ? window.requestIdleCallback.bind(window) : null;
  const nativeCancel = typeof window.cancelIdleCallback === 'function' ? window.cancelIdleCallback.bind(window) : null;
  window.requestIdleCallback = (cb, opts) => {{
    if (performance.now() - t0 < 5000 && ({skip})) return 0;
    if (native) return native(cb, opts);
    return setTimeout(() => cb({{ didTimeout: false, timeRemaining: () => 50 }}), 1);
  }};
  window.cancelIdleCallback = (id) => {{
    if (!id) return;
    if (nativeCancel) nativeCancel(id);
    else clearTimeout(id);
  }};
}})();"
    ))
}

/// 预热 A/B 的变体名（日志用）
fn prewarm_variant() -> String {
    std::env::var("WISP_WINDOW_LAB_NO_PREWARM").map_or_else(|_| "on".to_owned(), |v| format!("off:{v}"))
}

/// 一次内存采样（在阻塞线程上取）：主进程 phys_footprint、主进程 malloc 在用，与属于本进程的每个 WebKit 进程
struct MemSample {
    main_kib: Option<u64>,
    malloc_kib: Option<u64>,
    webkit: Vec<super::native::LabProcess>,
}

impl MemSample {
    async fn take(app: &tauri::AppHandle) -> Self {
        let app = app.clone();
        tauri::async_runtime::spawn_blocking(move || Self {
            main_kib: wisp_platform::process::memory_kb(),
            malloc_kib: super::native::lab_malloc_in_use().map(|b| b / 1024),
            webkit: super::native::lab_webkit_processes(&app),
        })
        .await
        .unwrap_or(Self {
            main_kib: None,
            malloc_kib: None,
            webkit: Vec::new(),
        })
    }

    /// 某一类 WebKit 进程的合计（KiB）
    fn kind_kib(&self, kind: &str) -> u64 {
        self.webkit
            .iter()
            .filter(|p| p.kind == kind)
            .map(|p| p.footprint_kib)
            .sum()
    }

    /// 全部 WebKit 进程的合计（内存预算里的「WebView 合计」，KiB）
    fn webview_kib(&self) -> u64 {
        self.webkit.iter().map(|p| p.footprint_kib).sum()
    }

    fn has(&self, kind: &str) -> bool {
        self.webkit.iter().any(|p| p.kind == kind)
    }

    fn line(&self) -> String {
        let others: Vec<String> = self
            .webkit
            .iter()
            .filter(|p| !matches!(p.kind.as_str(), "WebContent" | "GPU" | "Networking"))
            .map(|p| format!("{}:{}", p.kind, p.footprint_kib))
            .collect();
        let contents: Vec<String> = self
            .webkit
            .iter()
            .filter(|p| p.kind == "WebContent")
            .map(|p| p.footprint_kib.to_string())
            .collect();
        format!(
            "main_kib={} malloc_kib={} webcontent_kib={} webcontents=[{}] gpu_kib={} networking_kib={} other=[{}] \
             webview_kib={} pids={:?}",
            self.main_kib.map_or_else(|| "-".to_owned(), |v| v.to_string()),
            self.malloc_kib.map_or_else(|| "-".to_owned(), |v| v.to_string()),
            self.kind_kib("WebContent"),
            contents.join(","),
            self.kind_kib("GPU"),
            self.kind_kib("Networking"),
            others.join(","),
            self.webview_kib(),
            self.webkit.iter().map(|p| (p.pid, p.kind.as_str())).collect::<Vec<_>>(),
        )
    }
}

/// 中位数（空时 None）
fn median(mut v: Vec<u64>) -> Option<u64> {
    v.sort_unstable();
    v.get(v.len() / 2).copied()
}

/// 在一个阶段里采样 `n` 次（间隔 1 s），逐次输出 `window_lab mem phase=… sample=i …`，再输出中位数的汇总行
/// `window_lab mem_summary phase=… webview_mib=… …`（每个阶段采多次，取中位数）。第一次采样之后再用系统的
/// `footprint` 工具取 GPU 进程与每个 WebContent 进程的内存构成（看 GPU 进程的大头是哪一类），输出
/// `window_lab footprint phase=… kind=… pid=… top=[类别:KiB,…]`
async fn sample_phase(app: &tauri::AppHandle, name: &str, n: usize) {
    let mut samples = Vec::with_capacity(n);
    for i in 0..n {
        if i > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let s = MemSample::take(app).await;
        log::info!(
            "window_lab mem phase={name} sample={i} {} load1={}",
            s.line(),
            super::native::lab_load_average().map_or_else(|| "-".to_owned(), |l| format!("{l:.2}"))
        );
        if i == 0 {
            let targets: Vec<(i32, String)> = s
                .webkit
                .iter()
                .filter(|p| matches!(p.kind.as_str(), "GPU" | "WebContent"))
                .map(|p| (p.pid, p.kind.clone()))
                .collect();
            for (pid, kind) in targets {
                let top = tauri::async_runtime::spawn_blocking(move || super::native::lab_footprint_categories(pid, 8))
                    .await
                    .unwrap_or_default();
                if !top.is_empty() {
                    let top: Vec<String> = top.iter().map(|(c, k)| format!("{c}:{k}")).collect();
                    log::info!(
                        "window_lab footprint phase={name} kind={kind} pid={pid} top=[{}]",
                        top.join(", ")
                    );
                }
            }
        }
        samples.push(s);
    }
    let mib = |f: &dyn Fn(&MemSample) -> u64| {
        median(samples.iter().map(f).collect()).map_or_else(|| "-".to_owned(), |k| format!("{:.1}", k as f64 / 1024.0))
    };
    log::info!(
        "window_lab mem_summary phase={name} n={n} webview_mib={} webcontent_mib={} gpu_mib={} networking_mib={} main_mib={} malloc_mib={}",
        mib(&|s| s.webview_kib()),
        mib(&|s| s.kind_kib("WebContent")),
        mib(&|s| s.kind_kib("GPU")),
        mib(&|s| s.kind_kib("Networking")),
        mib(&|s| s.main_kib.unwrap_or(0)),
        mib(&|s| s.malloc_kib.unwrap_or(0)),
    );
}

/// 整屏的冻结帧（有屏幕录制权限时是真的截图：SCK 的 CGImage，Overlay 上的原生冻结帧图层与页面的放大镜都按真实流程
/// 工作；没有权限时退回 2×2 的假帧，`overlay_show` 不装原生冻结帧）
async fn lab_frames(monitors: &[wisp_types::MonitorInfo]) -> (Vec<wisp_capture::Frame>, bool) {
    let captured = tauri::async_runtime::spawn_blocking({
        let monitors = monitors.to_vec();
        move || wisp_capture::capture_all(&monitors, std::process::id())
    })
    .await;
    match captured {
        Ok(Ok(set)) => (set.frames, true),
        other => {
            log::info!("window_lab: capture unavailable ({other:?}), using fake frames (no native frozen frame)");
            let frames = monitors
                .iter()
                .map(|m| wisp_capture::Frame::from_rgba(m.clone(), 2, 2, vec![0; 16]))
                .collect();
            (frames, false)
        }
    }
}

/// 内存测量里的一步真实使用（`WISP_WINDOW_LAB_MEMORY_STEPS`，逗号分隔，按顺序执行，每步之后静置、采样
/// `after_<步骤>`）。完整实验台之后 GPU 进程停在 72–79 MiB，而只用三个面板视图时只有 13–15 MiB——
/// 逐个窗口打开再采样，找出是谁把它留在那里
/// 放大镜的统计（Overlay 页面写出的 `window.__WISP_LAB_OVERLAY__`）：每块小块的往返、画过的帧里缺清晰小块
/// （画底图）/ 连底图都没有（画黑）的帧数、底图的往返、在路上的最多几块；连续划过那一段与全程各一行
fn log_tile_stats(stats: &serde_json::Value) {
    for part in ["continuous", "total"] {
        let Some(s) = stats.get(part) else { continue };
        let v = |k: &str| s.get(k).map_or_else(|| "-".to_owned(), ToString::to_string);
        let t = |k: &str| {
            s.get("tiles")
                .and_then(|t| t.get(k))
                .map_or_else(|| "-".to_owned(), ToString::to_string)
        };
        log::info!(
            "window_lab mem step=ocr magnifier part={part} lens_frames={} partial_frames={} black_frames={} overview_ms={} \
             peak_inflight={} tile_ms n={} p50={} p95={} max={}",
            v("lens"),
            v("partial"),
            v("black"),
            v("overview_ms"),
            v("peak_inflight"),
            t("n"),
            t("p50"),
            t("p95"),
            t("max"),
        );
    }
}

async fn memory_step(app: &tauri::AppHandle, wm: &Arc<WindowManager>, step_name: &str) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    match step_name {
        // 打开一次设置窗口（无白闪首显、停 2 s，像用户看一眼），再关掉（原生关闭按钮 = 销毁）
        "settings" => {
            wm.open_settings(Some("general".to_owned()));
            tokio::time::sleep(Duration::from_millis(2000)).await;
            if let Some(w) = app.get_webview_window(super::settings_window::SETTINGS_LABEL) {
                super::native::destroy_window(&w);
            }
            log::info!("window_lab mem step=settings opened and closed");
        }
        // 设置窗口在各分组之间切换（每页停 1 s；页面懒加载，切换会挂载新的组件树）
        "settings_pages" => {
            for section in [
                "general",
                "translate",
                "providers",
                "hotkeys",
                "clipboard",
                "ocr",
                "appearance",
                "about",
            ] {
                wm.open_settings(Some(section.to_owned()));
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
            if let Some(w) = app.get_webview_window(super::settings_window::SETTINGS_LABEL) {
                super::native::destroy_window(&w);
            }
            log::info!("window_lab mem step=settings_pages visited 8 sections and closed");
        }
        // 固定一次（真实流程：翻译面板 → pin_from_session），停 2.5 s 后关掉固定窗口；`pin_expand` 走「展开」的弹簧过渡
        "pin" | "pin_expand" => {
            let expand = step_name == "pin_expand";
            let t = bus_trigger(
                &state,
                crate::bus::Command::TranslateText {
                    text: "Pin this translation to the screen.".to_owned(),
                    origin: crate::bus::TriggerOrigin::Cli,
                },
                false,
            )
            .await;
            tokio::time::sleep(Duration::from_millis(800)).await;
            let session = t
                .and_then(|t| t.session_id)
                .filter(|&s| wm.panel_state().is_some_and(|p| p.session_id == s));
            match session {
                Some(s) => match wm.pin_from_session(s, lab_pin_state(), expand) {
                    Ok(id) => {
                        tokio::time::sleep(Duration::from_millis(2500)).await;
                        if let Some(w) = app.get_webview_window(&super::pinned::label_of(&id)) {
                            super::native::destroy_window(&w);
                        }
                        log::info!("window_lab mem step={step_name} pinned and closed id={id}");
                    }
                    Err(e) => log::warn!("window_lab mem step={step_name} pin failed: {}", e.message),
                },
                None => log::warn!("window_lab mem step={step_name}: no panel session to pin"),
            }
        }
        // 复制后的 toast（不激活、显示在上次面板位置），自然消失
        "toast" => {
            wm.panel_show_toast(ToastPayload {
                reason: CopiedReason::CopyOnly,
                paste_key: "⌘V".to_owned(),
                queue: None,
            });
            tokio::time::sleep(Duration::from_millis(1600)).await;
            if let Some(s) = wm.panel_state().filter(|s| s.view == PanelView::Toast) {
                wm.panel_hide_now(s.session_id, HideReason::Other);
            }
            log::info!("window_lab mem step=toast shown");
        }
        // 每块屏一个整屏 Overlay（有权限时带真实冻结帧），显示 1.5 s 后销毁——不经过编排器，只看窗口本身；
        // `overlay_fade` 走编排器 done 路径用的淡出销毁（`destroy_overlays_fading`）
        "overlay" | "overlay_fade" => {
            let before = census(app).await;
            let monitors = state.platform.screen.monitors();
            let (frames, real) = lab_frames(&monitors).await;
            let session = OverlaySession {
                session_id: 9200,
                monitors: monitors.clone(),
                mode: OverlayMode::Ocr,
            };
            let shown = {
                let wm = Arc::clone(wm);
                tauri::async_runtime::spawn_blocking(move || {
                    wm.create_overlays(&session).map(|()| {
                        frames
                            .iter()
                            .filter(|f| wm.overlay_show(9200, &f.monitor.id, Some(f)).unwrap_or(false))
                            .count()
                    })
                })
                .await
            };
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let wm2 = Arc::clone(wm);
            let fade = step_name == "overlay_fade";
            let _ = tauri::async_runtime::spawn_blocking(move || {
                if fade {
                    wm2.destroy_overlays_fading();
                } else {
                    wm2.destroy_overlays();
                }
            })
            .await;
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let after = census(app).await;
            log::info!(
                "window_lab mem step={step_name} real_capture={real} native_frames={shown:?} windows {before:?} -> {after:?}"
            );
        }
        // 一次完整的截图 OCR（与 ⌥S 同一个入口：编排器截图、建 Overlay、原生冻结帧、页面 init），由实验台代替用户在主屏
        // 上框选一块区域并 commit；Overlay 由编排器按流程拆除，OCR 结果卡片（假模型）显示 2 s 后由前端收起
        "ocr" => {
            crate::app::smoke::seed(&state.settings);
            let before = census(app).await;
            // 主进程的峰值（要看的是 OCR 的峰值，不只是之后的常驻值）：这一步全程每 5 ms 采一次
            // phys_footprint，另读前后两次生命期最大值（rusage_info_v4，采样之间的瞬时峰值也算在内）
            let lifetime_before = super::native::lab_lifetime_max_footprint_kib();
            let peak = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let sampling = Arc::new(std::sync::atomic::AtomicBool::new(true));
            let sampler = {
                let (peak, sampling) = (Arc::clone(&peak), Arc::clone(&sampling));
                std::thread::spawn(move || {
                    while sampling.load(std::sync::atomic::Ordering::Relaxed) {
                        if let Some(kib) = wisp_platform::process::memory_kb() {
                            peak.fetch_max(kib, std::sync::atomic::Ordering::Relaxed);
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                })
            };
            let t0 = Instant::now();
            state.bus.send(crate::bus::Command::Action {
                action: wisp_types::HotkeyAction::Ocr,
                origin: crate::bus::TriggerOrigin::Cli,
            });
            // 等主屏的 Overlay 页面 init（页面可以交互）
            let primary = state.platform.screen.monitors().into_iter().find(|m| m.is_primary);
            let mut target = None;
            while target.is_none() && t0.elapsed() < Duration::from_secs(6) {
                tokio::time::sleep(Duration::from_millis(20)).await;
                target = crate::orchestrator::ocr::lab_overlay_inits_since(t0)
                    .into_iter()
                    .find_map(|(label, _)| {
                        let monitor = wm.overlay_monitor(&label)?;
                        let session = wm.overlay_session_of(&label)?;
                        primary
                            .as_ref()
                            .is_none_or(|p| p.id == monitor)
                            .then_some((session, monitor, label))
                    });
            }
            let Some((session_id, monitor_id, label)) = target else {
                sampling.store(false, std::sync::atomic::Ordering::Relaxed);
                let _ = sampler.join();
                log::warn!("window_lab mem step=ocr: no overlay came up");
                return;
            };
            // `WISP_WINDOW_LAB_OCR_DRIVE=1`：由页面自己完成——在 Overlay 页面里合成一段指针扫过（放大镜按需拉小块）
            // 与一次拖拽框选（页面自己 overlay_commit、按选区拉飞入的那块像素、飞入、overlay_done），覆盖真实使用的数据流；
            // 否则照旧由实验台直接 commit（页面不知道，没有飞入）
            let drive = std::env::var("WISP_WINDOW_LAB_OCR_DRIVE").is_ok_and(|v| v == "1");
            // 放大镜的统计在页面 init 之后立即打开（低分辨率底图在 Overlay 显示时就开始拉，要赶在它到达之前）
            if drive && let Some(w) = app.get_webview_window(&label) {
                let _ = w.eval(
                    "window.__WISP_LAB_OVERLAY__={tile_ms:[],lens:0,partial:0,black:0,overview_ms:null,peak_inflight:0}",
                );
            }
            tokio::time::sleep(Duration::from_millis(600)).await;
            let acked = if drive {
                // 先连续划过（约 20 逻辑 px / 帧：120 Hz 下约 2400 px/s 的快速一划），再 Z 字形跳着扫（每帧跳到很远的地方，
                // 比真实移动差得多的情形），最后一次拖拽框选。页面放大镜的统计（每块小块的往返、缺块的帧）在
                // 松手时写进 document.title（之后 Overlay 很快拆除）
                const JS: &str = "(function(){var L=window.__WISP_LAB_OVERLAY__||(window.__WISP_LAB_OVERLAY__={tile_ms:[],lens:0,partial:0,black:0,overview_ms:null,peak_inflight:0});\
var r=document.getElementById('root');var W=innerWidth,H=innerHeight,i=0,N=48,k=0,M=60,C=null;\
function ev(t,x,y,bs){r.dispatchEvent(new PointerEvent(t,{clientX:x,clientY:y,button:0,buttons:bs,bubbles:true,cancelable:true,pointerId:1}))}\
function drag(j){if(j<12){ev('pointermove',160+520*j/11,160+180*j/11,1);requestAnimationFrame(function(){drag(j+1)});return}\
ev('pointerup',680,340,0);document.title='lab:'+NONCE+':'+JSON.stringify({continuous:C,total:Z(L)})}\
function Z(x){var t=x.tile_ms.slice().sort(function(a,b){return a-b}),q=function(p){return t.length?t[Math.round((t.length-1)*p)]:null};\
return {lens:x.lens,partial:x.partial,black:x.black,overview_ms:x.overview_ms,peak_inflight:x.peak_inflight,tiles:{n:t.length,p50:q(0.5),p95:q(0.95),max:q(1)}}}\
function sweep(){if(i<N){ev('pointermove',W*(0.1+0.8*i/N),H*(0.15+0.7*((i*7)%N)/N),0);i++;requestAnimationFrame(sweep);return}\
ev('pointermove',160,160,0);ev('pointerdown',160,160,1);drag(0)}\
function line(){if(k<M){ev('pointermove',W*(0.1+0.8*k/M),H*0.5,0);k++;requestAnimationFrame(line);return}C=Z(L);sweep()}\
requestAnimationFrame(line)})()";
                match app.get_webview_window(&label) {
                    Some(w) => {
                        let nonce = lab_nonce();
                        let _ = w.eval(format!("var NONCE={nonce};{JS}"));
                        log::info!(
                            "window_lab mem step=ocr: driving the overlay page (pointer line + sweep + drag) label={label}"
                        );
                        match read_lab_title(&w, nonce, Duration::from_secs(5)).await {
                            Some(stats) => log_tile_stats(&stats),
                            None => log::warn!("window_lab mem step=ocr: no magnifier stats from the overlay page"),
                        }
                    }
                    None => log::warn!("window_lab mem step=ocr: overlay window {label} is gone"),
                }
                None
            } else {
                let commit = wisp_types::OverlayCommit {
                    session_id,
                    monitor_id,
                    rect: wisp_types::Rect::new(160.0, 160.0, 520.0, 180.0),
                    kind: wisp_types::OverlaySelectionKind::Drag,
                };
                Some(state.orchestrator.overlay_commit(commit).await)
            };
            // 页面不知道这次 commit 是实验台发的（没有飞入、不会调用 overlay_done）：编排器的 done 看门狗按流程拆除 Overlay
            let mut shown = false;
            let t1 = Instant::now();
            while t1.elapsed() < Duration::from_secs(8) {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let overlays = app.webview_windows().keys().any(|l| l.starts_with("overlay-"));
                shown |= wm.panel_controller().snapshot().is_some_and(|p| p.shown);
                if shown && !overlays {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(2000)).await;
            hide_by_frontend(wm).await;
            tokio::time::sleep(Duration::from_millis(1500)).await;
            sampling.store(false, std::sync::atomic::Ordering::Relaxed);
            let _ = sampler.join();
            log::info!(
                "window_lab mem step=ocr peak main_kib sampled_max={} (every 5 ms from trigger to hidden) lifetime_max before={} after={}",
                peak.load(std::sync::atomic::Ordering::Relaxed),
                lifetime_before.map_or_else(|| "-".to_owned(), |v| v.to_string()),
                super::native::lab_lifetime_max_footprint_kib().map_or_else(|| "-".to_owned(), |v| v.to_string()),
            );
            let after = census(app).await;
            let fetches = &crate::orchestrator::captures::LAB_FRAME_FETCHES;
            log::info!(
                "window_lab mem step=ocr session={session_id} drive={drive} commit={:?} panel_shown={shown} in {} ms; \
                 frame fetches full={} region={}; windows {before:?} -> {after:?}",
                acked.map(|r| r.map(|a| a.fly_to.is_some())),
                t0.elapsed().as_millis(),
                fetches[0].load(std::sync::atomic::Ordering::Relaxed),
                fetches[1].load(std::sync::atomic::Ordering::Relaxed),
            );
        }
        // 连续使用：`WISP_WINDOW_LAB_MEMORY_SHOWS` 次翻译（经总线触发、假模型流式输出，与划词翻译同一个入口），每次看
        // `WISP_WINDOW_LAB_MEMORY_SHOW_MS`（默认 1200 ms）后由前端走退出流程收起，两次之间隔
        // `WISP_WINDOW_LAB_MEMORY_SHOW_GAP_MS`（默认 600 ms，即约 2 s 一次）——60 ms 一次、立即 hide_now 的
        // 压力形态不代表真实使用
        "shows" => {
            crate::app::smoke::seed(&state.settings);
            let env_ms = |name: &str, default: u64| {
                Duration::from_millis(std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default))
            };
            let shows: u32 = std::env::var("WISP_WINDOW_LAB_MEMORY_SHOWS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(100);
            let view = env_ms("WISP_WINDOW_LAB_MEMORY_SHOW_MS", 1200);
            let gap = env_ms("WISP_WINDOW_LAB_MEMORY_SHOW_GAP_MS", 600);
            let texts = [
                "The quick brown fox jumps over the lazy dog.",
                "Memory grows with every show unless something is retained.",
                "A short sentence.",
                "Translation panels should stay light while hidden.",
            ];
            let mut done = 0;
            for i in 0..shows {
                let t = bus_trigger(
                    &state,
                    crate::bus::Command::TranslateText {
                        text: texts[i as usize % texts.len()].repeat(1 + (i as usize % 3)),
                        origin: crate::bus::TriggerOrigin::Cli,
                    },
                    false,
                )
                .await;
                if t.is_some_and(|t| t.shown_ms.is_some()) {
                    done += 1;
                }
                tokio::time::sleep(view).await;
                hide_by_frontend(wm).await;
                tokio::time::sleep(gap).await;
                if (i + 1) % 25 == 0 {
                    sample_phase(app, &format!("shows_{}", i + 1), 1).await;
                }
            }
            log::info!("window_lab mem step=shows shown={done}/{shows} view={view:?} gap={gap:?}");
        }
        other => log::warn!("window_lab mem step={other}: unknown step"),
    }
}

/// 实验台固定用的卡片内容
fn lab_pin_state() -> PinState {
    PinState {
        kind: PinKind::Translate,
        source_text: "long text".to_owned(),
        result_text: "长文本".to_owned(),
        task: TaskKind::Translate,
        style_id: "natural".to_owned(),
        target_lang: Lang::ZhCn,
        detected_lang: None,
        request_id: None,
        capture_id: None,
        ocr_text: None,
        ocr_mode: None,
        show_source: false,
        status_line: None,
        dict_mode: false,
        model_ref: None,
        status: None,
        thumb_url: None,
        image_size: None,
        expand_width: None,
        on_top: None,
        ocr_layers: None,
    }
}

/// 等一次回收（页面重新加载），最多 `limit`
async fn wait_recycled(controller: &super::panel::PanelController, limit: Duration) -> bool {
    let loads = controller.loaded();
    let before = *loads.borrow();
    let t0 = Instant::now();
    while *loads.borrow() == before && t0.elapsed() < limit {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    *loads.borrow() != before
}

/// 可选的内存测量（`WISP_WINDOW_LAB_MEMORY=1`）：预热后从未显示 → 三个视图各用一次之后（热）→
/// [`memory_step`] 的各步（`WISP_WINDOW_LAB_MEMORY_STEPS`，每步之后采样 `after_<步骤>`）→ 回收之后 → 空闲卸载之后（立即与
/// 60 s 后）。每个阶段在进程内采样 `WISP_WINDOW_LAB_MEMORY_SAMPLES` 次（默认 3，间隔 1 s；主进程、malloc 与每个 WebKit 进程
/// 的 phys_footprint，归属见 `native::lab_webkit_processes`），并取 GPU / WebContent 进程的内存构成。热阶段顺带记下每个
/// 视图在预热过的页面上第一次唤出的 ready / 首帧（预热的收益）。配合 `WISP_WINDOW_LAB_NO_PREWARM=1`
/// （[`panel_init_script`]）做预热开 / 关的 A/B；每个变体建议独立启动 3 次以上。`WISP_WINDOW_LAB_MEMORY_GPU_DECAY=<秒>`：
/// 各步之后再等这么久采样一次 `after_steps_decay`（GPU 进程会不会自己把缓存还回去）
async fn memory_phases(app: &tauri::AppHandle, wm: &Arc<WindowManager>) {
    let controller = Arc::clone(wm.panel_controller());
    let original = controller.options();
    let settle = Duration::from_secs(
        std::env::var("WISP_WINDOW_LAB_MEMORY_SETTLE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10),
    );
    let n: usize = std::env::var("WISP_WINDOW_LAB_MEMORY_SAMPLES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3)
        .max(1);
    let steps: Vec<String> = std::env::var("WISP_WINDOW_LAB_MEMORY_STEPS")
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let prewarm = prewarm_variant();
    log::info!("window_lab mem variant prewarm={prewarm} samples={n} settle={settle:?} steps={steps:?}");
    let fmt = |t: &crate::app::perf::TraceSnapshot| {
        format!(
            "ready_ms={:?} window_ms={:?} key_ms={:?} shown_ms={:?} page={:?} ready_timeout={}",
            t.ready_ms, t.window_ms, t.key_ms, t.shown_ms, t.page, t.ready_timeout
        )
    };
    let views = [
        (
            PanelView::Translate,
            AnchorSource::Cursor,
            translate(&"The quick brown fox jumps. ".repeat(12)),
        ),
        (PanelView::Input, AnchorSource::Center, input_payload()),
        (PanelView::Clipboard, AnchorSource::Center, clipboard_payload()),
    ];
    // 0. 预热之后从未显示
    tokio::time::sleep(settle).await;
    sample_phase(app, "prewarmed", n).await;
    // 1. 三个视图各用一次（回收推迟到测完「热」与各步之后：各步期间的回收另由 RECYCLE 推迟规则处理）
    controller.set_options(super::panel::PanelOptions {
        recycle_after: None,
        unload_after: None,
        ..original
    });
    for (view, anchor, payload) in views.clone() {
        let name = format!("{view:?}").to_lowercase();
        match traced_show(app, wm, request(view, anchor, payload)).await {
            Some(t) => log::info!("window_lab first_show view={name} prewarm={prewarm} {}", fmt(&t)),
            None => log::warn!("window_lab first_show view={name} not shown"),
        }
        tokio::time::sleep(Duration::from_millis(1200)).await;
        hide_by_frontend(wm).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    tokio::time::sleep(settle).await;
    sample_phase(app, "hot", n).await;
    // 1a. 真实使用的各步
    for step_name in &steps {
        memory_step(app, wm, step_name).await;
        tokio::time::sleep(settle).await;
        sample_phase(app, &format!("after_{step_name}"), n).await;
    }
    if !steps.is_empty()
        && let Some(secs) = std::env::var("WISP_WINDOW_LAB_MEMORY_GPU_DECAY")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
    {
        tokio::time::sleep(Duration::from_secs(secs)).await;
        sample_phase(app, "after_steps_decay", n).await;
    }
    // 2. 回收：最后一次使用之后隐藏满 recycle_after（这里取 settle + 5 s）换掉 WebContent；其他窗口刚有活动时产品会推迟
    //    （最多 120 s）
    controller.set_options(super::panel::PanelOptions {
        recycle_after: Some(settle + Duration::from_secs(5)),
        unload_after: None,
        ..original
    });
    let _ = traced_show(app, wm, request(views[0].0, views[0].1.clone(), views[0].2.clone())).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    hide_by_frontend(wm).await;
    let recycled = wait_recycled(&controller, Duration::from_secs(150)).await;
    log::info!("window_lab mem recycle happened={recycled}");
    tokio::time::sleep(settle).await;
    sample_phase(app, "recycled", n).await;
    // 3. 空闲卸载（用一次再隐藏，卸载 1 s 后到期）
    controller.set_options(super::panel::PanelOptions {
        recycle_after: None,
        unload_after: Some(Duration::from_secs(1)),
        ..original
    });
    let _ = traced_show(app, wm, request(views[0].0, views[0].1.clone(), views[0].2.clone())).await;
    hide_by_frontend(wm).await;
    let t0 = Instant::now();
    while app.get_webview_window(PANEL_LABEL).is_some() && t0.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(settle).await;
    sample_phase(app, "unloaded", n).await;
    tokio::time::sleep(Duration::from_secs(60)).await;
    sample_phase(app, "unloaded_60s", n).await;
    controller.set_options(original);
    step("done", true, "memory mode");
}

/// 可选（`WISP_WINDOW_LAB_INPUT_SPLIT=<轮数>`）：输入视图的 ready → 上屏拆分。经总线触发
/// `Action(TranslateInput)`（与快捷键同一个入口），热页面与回收出来的页面各 N 轮，每轮记 ready → 窗口上屏
/// （`window_ms − ready_ms`）、ready → 成为 key（`key_ms − ready_ms`）、ready → 内容首帧（`shown_ms − ready_ms`），以及
/// 触发 → ready、触发 → 首帧；结果行 `window_lab input_split page=… n=… ready_to_window=… ready_to_key=… ready_to_frame=…`
/// （p50 / p95 / max）。两轮之间由前端走退出流程收起
async fn input_split(app: &tauri::AppHandle, wm: &Arc<WindowManager>, rounds: usize) {
    use crate::bus::{Command, TriggerOrigin};
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let controller = Arc::clone(wm.panel_controller());
    let original = controller.options();
    #[derive(Default)]
    struct Split {
        ready_to_window: Vec<u32>,
        ready_to_key: Vec<u32>,
        ready_to_frame: Vec<u32>,
        trigger_to_ready: Vec<u32>,
        trigger_to_frame: Vec<u32>,
        prepare_to_ready: Vec<u32>,
        timeouts: usize,
        pages: Vec<String>,
    }
    fn record(split: &mut Split, page: &str, i: usize, t: Option<&crate::app::perf::TraceSnapshot>) {
        let Some(t) = t else {
            log::warn!("window_lab input_split page={page} round={i} no trace");
            return;
        };
        log::info!(
            "window_lab input_split_round page={page} round={i} prepare_ms={:?} ready_ms={:?} window_ms={:?} key_ms={:?} \
             shown_ms={:?} panel_page={:?} ready_timeout={}",
            t.prepare_ms,
            t.ready_ms,
            t.window_ms,
            t.key_ms,
            t.shown_ms,
            t.page,
            t.ready_timeout
        );
        if let Some(r) = t.ready_ms {
            split.trigger_to_ready.push(r);
            split.ready_to_window.extend(t.window_ms.map(|w| w.saturating_sub(r)));
            split.ready_to_key.extend(t.key_ms.map(|k| k.saturating_sub(r)));
            split.ready_to_frame.extend(t.shown_ms.map(|f| f.saturating_sub(r)));
            split.prepare_to_ready.extend(t.prepare_ms.map(|p| r.saturating_sub(p)));
        }
        split.trigger_to_frame.extend(t.shown_ms);
        split.timeouts += usize::from(t.ready_timeout);
        split.pages.push(format!("{:?}", t.page));
    }
    let report = |page: &str, s: &Split| {
        let f = |v: &[u32]| ms_percentiles(v).to_string();
        log::info!(
            "window_lab input_split page={page} n={} ready_to_window={} ready_to_key={} ready_to_frame={} \
             trigger_to_ready={} prepare_to_ready={} trigger_to_frame={} ready_timeouts={} pages={:?}",
            s.trigger_to_ready.len(),
            f(&s.ready_to_window),
            f(&s.ready_to_key),
            f(&s.ready_to_frame),
            f(&s.trigger_to_ready),
            f(&s.prepare_to_ready),
            f(&s.trigger_to_frame),
            s.timeouts,
            s.pages
                .iter()
                .fold(std::collections::BTreeMap::<&str, usize>::new(), |mut m, p| {
                    *m.entry(p.as_str()).or_default() += 1;
                    m
                }),
        );
    };
    let trigger = || Command::Action {
        action: wisp_types::HotkeyAction::TranslateInput,
        origin: TriggerOrigin::Hotkey,
    };
    // 热页面：先用一次让页面变成「用过的」，之后每轮都是 warm
    controller.set_options(super::panel::PanelOptions {
        recycle_after: None,
        unload_after: None,
        ..original
    });
    let _ = bus_trigger(&state, trigger(), true).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    hide_by_frontend(wm).await;
    let mut warm = Split::default();
    for i in 0..rounds {
        tokio::time::sleep(Duration::from_millis(700)).await;
        let t = bus_trigger(&state, trigger(), true).await;
        record(&mut warm, "warm", i, t.as_ref());
        tokio::time::sleep(Duration::from_millis(500)).await;
        hide_by_frontend(wm).await;
    }
    report("warm", &warm);
    // 回收出来的页面：每轮隐藏 500 ms 后回收，新页面加载后静置 1.5 s（与第 9c 步相同）再触发
    controller.set_options(super::panel::PanelOptions {
        recycle_after: Some(Duration::from_millis(500)),
        unload_after: None,
        ..original
    });
    let mut recycled = Split::default();
    let mut missed = 0;
    for i in 0..rounds {
        if !wait_recycled(&controller, RECYCLE_WAIT).await {
            missed += 1;
        }
        tokio::time::sleep(RECYCLED_SETTLE).await;
        let t = bus_trigger(&state, trigger(), true).await;
        record(&mut recycled, "recycled", i, t.as_ref());
        tokio::time::sleep(Duration::from_millis(500)).await;
        hide_by_frontend(wm).await;
    }
    report("recycled", &recycled);
    controller.set_options(original);
    let p95 = |v: &[u32]| ms_percentiles(v).p95;
    step(
        "input_split",
        warm.trigger_to_ready.len() == rounds && recycled.trigger_to_ready.len() == rounds && missed == 0,
        format!(
            "warm ready→window p95 {:?}, ready→key p95 {:?}, ready→frame p95 {:?}; recycled ready→window p95 {:?}, \
             ready→key p95 {:?}, ready→frame p95 {:?}; recycles missed {missed}",
            p95(&warm.ready_to_window),
            p95(&warm.ready_to_key),
            p95(&warm.ready_to_frame),
            p95(&recycled.ready_to_window),
            p95(&recycled.ready_to_key),
            p95(&recycled.ready_to_frame),
        ),
    );
}

/// 冷唤出之前的等待：面板卸载之后至少等 `min_idle`，并且确认 WebContent 与 GPU 进程都已退出（最后一个
/// WebView 消失后 GPU 进程约 1 分钟才退出——真实的「空闲 10 分钟之后的第一次唤出」也要为它冷启动），最多再多等 2 分钟。
/// 返回 (实际等待, WebKit 是否全冷, 当时的采样)
async fn wait_webkit_cold(app: &tauri::AppHandle, min_idle: Duration) -> (Duration, bool, MemSample) {
    let t0 = Instant::now();
    let limit = min_idle + Duration::from_secs(120);
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if t0.elapsed() < min_idle {
            continue;
        }
        let s = MemSample::take(app).await;
        let cold = !s.has("GPU") && !s.has("WebContent");
        if cold || t0.elapsed() >= limit {
            return (t0.elapsed(), cold, s);
        }
    }
}

/// 经总线触发一次（与快捷键 / CLI 同一个入口：编排器收到命令时开始埋点），等这次追踪的窗口上屏与首帧（要焦点的视图还等
/// 成为 key），最多 4 s
async fn bus_trigger(
    state: &AppState,
    cmd: crate::bus::Command,
    wants_key: bool,
) -> Option<crate::app::perf::TraceSnapshot> {
    let perf = &state.perf;
    let after = perf.last_id();
    state.bus.send(cmd);
    let t0 = Instant::now();
    loop {
        let snap = perf.first_after(after);
        let complete = snap
            .as_ref()
            .is_some_and(|s| s.window_ms.is_some() && s.shown_ms.is_some() && (!wants_key || s.key_ms.is_some()));
        if complete || t0.elapsed() > Duration::from_secs(4) {
            return snap;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// 冷唤出一轮之后读页面自己的时间点（拆分「页面加载 → ready」）：导航时序（响应结束、DOM 可交互、
/// DOMContentLoaded、load）与前端记的 `wisp:entry`（入口开始执行）/ `wisp:first-ready`（第一次发出 ready），都是相对页面
/// timeOrigin 的毫秒；另给 timeOrigin（Unix 毫秒），与日志里 Rust 侧的时刻对齐。经 `document.title` 读出
async fn cold_js_split(app: &tauri::AppHandle, name: &str, round: usize, boot: bool) {
    let Some(w) = app.get_webview_window(PANEL_LABEL) else {
        return;
    };
    let js = "(function(){var n=performance.getEntriesByType('navigation')[0]||{};var m=function(k){var e=performance.getEntriesByName(k)[0];return e?Math.round(e.startTime):null};document.title='lab:'+JSON.stringify({origin:Math.round(performance.timeOrigin),response_end:Math.round(n.responseEnd||0),dom_interactive:Math.round(n.domInteractive||0),dcl:Math.round(n.domContentLoadedEventStart||0),load:Math.round(n.loadEventStart||0),entry:m('wisp:entry'),first_ready:m('wisp:first-ready')})})()";
    if w.eval(js).is_err() {
        return;
    }
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(1) {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let w = w.clone();
        let title = tauri::async_runtime::spawn_blocking(move || super::native::lab_webview_title(&w))
            .await
            .ok()
            .flatten();
        if let Some(t) = title.and_then(|t| t.strip_prefix("lab:").map(str::to_owned)) {
            log::info!("window_lab cold_js view={name} round={round} boot_prepare={boot} {t}");
            return;
        }
    }
}

/// 冷唤出的一组分位数（毫秒）：p50 / p95 / max 与样本数
fn ms_percentiles(v: &[u32]) -> Percentiles {
    Percentiles::of(v.iter().map(|&ms| Duration::from_millis(u64::from(ms))).collect())
}

/// 可选的冷唤出测量（`WISP_WINDOW_LAB_COLD=1`）：每一轮都在面板卸载、**WebContent 与 GPU 进程都已
/// 退出**（卸载后至少 `WISP_WINDOW_LAB_COLD_MIN_IDLE_SECS`，默认 70 s）之后，经总线触发（与快捷键 / CLI 同一个入口），
/// 记「触发 → 卡片上屏（外壳）」「触发 → 内容首帧」、prepare / ready / key、页面类型与当时的负载；翻译与输入视图各
/// `WISP_WINDOW_LAB_COLD_ROUNDS` 轮（默认 30）。同样的条件下再测 `WISP_WINDOW_LAB_OVERLAY_ROUNDS` 轮（默认 20）Overlay 的
/// 「建窗口 / 上屏 → 页面 `overlay_init`」（`OVERLAY_INIT_WAIT` 1.5 s 看门狗的余量）。`WISP_WINDOW_LAB_COLD_LONG_ROUNDS`
/// （默认 0）：最后再做几轮真正空闲 10 分钟之后的唤出，与 70 s 口径对照。
///
/// `cold_show_{translate,input}` 断言冷唤出的阈值：触发 → 卡片上屏 p95 ≤ 150 ms（含原生首帧掩盖手段）；内容首帧
/// 另行报告。`cold_overlay_init`：上屏 → init 的 p99 不到看门狗的一半。
/// `WISP_FAKE_LLM=1` 时预置冒烟的假 Provider（翻译视图显示流式结果；否则显示未就绪卡片，同样是一次完整的唤出）
async fn cold_rounds(app: &tauri::AppHandle, wm: &Arc<WindowManager>, primary: Option<wisp_types::MonitorInfo>) {
    use crate::bus::{Command, TriggerOrigin};
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let env_num = |name: &str, default: u64| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(default)
    };
    let rounds = env_num("WISP_WINDOW_LAB_COLD_ROUNDS", 30) as usize;
    // A/B：奇数轮的新页面不带 Boot.prepare（改动之前的冷唤出路径），偶数轮照常；两组分别报告
    let ab = std::env::var("WISP_WINDOW_LAB_COLD_AB").is_ok_and(|v| v == "1");
    let overlay_rounds = env_num("WISP_WINDOW_LAB_OVERLAY_ROUNDS", 20) as usize;
    let long_rounds = env_num("WISP_WINDOW_LAB_COLD_LONG_ROUNDS", 0) as usize;
    let min_idle = Duration::from_secs(env_num("WISP_WINDOW_LAB_COLD_MIN_IDLE_SECS", 70));
    if std::env::var("WISP_FAKE_LLM").is_ok_and(|v| v == "1") {
        crate::app::smoke::seed(&state.settings);
    }
    let controller = Arc::clone(wm.panel_controller());
    let original = controller.options();
    controller.set_options(super::panel::PanelOptions {
        recycle_after: None,
        unload_after: Some(Duration::from_millis(300)),
        ..original
    });
    log::info!(
        "window_lab cold config rounds={rounds} overlay_rounds={overlay_rounds} long_rounds={long_rounds} min_idle={min_idle:?}"
    );
    // 启动时预热的面板：用一次再隐藏，让它卸载
    let _ = traced_show(
        app,
        wm,
        request(PanelView::Translate, AnchorSource::Cursor, translate("warm")),
    )
    .await;
    hide_by_frontend(wm).await;
    let wait_unloaded = || async {
        let t0 = Instant::now();
        while app.get_webview_window(PANEL_LABEL).is_some() && t0.elapsed() < Duration::from_secs(10) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    wait_unloaded().await;
    #[derive(Default)]
    struct Series {
        window: Vec<u32>,
        painted: Vec<u32>,
        prepare_to_window: Vec<u32>,
        key: Vec<u32>,
        not_cold: usize,
    }
    let mut translate_s = Series::default();
    let mut input_s = Series::default();
    let mut translate_nb = Series::default();
    let mut input_nb = Series::default();
    let (mut init_after_show, mut init_after_create) = (Vec::new(), Vec::new());
    let mut overlay_missing = 0usize;
    let text = "The quick brown fox jumps over the lazy dog. ".repeat(4);
    fn panel_round(
        name: &str,
        i: usize,
        idle: Duration,
        cold: bool,
        load: Option<f64>,
        t: Option<crate::app::perf::TraceSnapshot>,
        series: &mut Series,
    ) {
        let Some(t) = t else {
            log::warn!("window_lab cold view={name} round={i} no trace");
            return;
        };
        log::info!(
            "window_lab cold view={name} round={i} idle_s={} webkit_cold={cold} load1={} trigger_to_window_ms={:?} \
             trigger_to_first_frame_ms={:?} prepare_ms={:?} ready_ms={:?} key_ms={:?} page={:?} ready_timeout={}",
            idle.as_secs(),
            load.map_or_else(|| "-".to_owned(), |l| format!("{l:.2}")),
            t.window_ms,
            t.shown_ms,
            t.prepare_ms,
            t.ready_ms,
            t.key_ms,
            t.page,
            t.ready_timeout
        );
        if !cold {
            series.not_cold += 1;
        }
        series.window.extend(t.window_ms);
        series.painted.extend(t.shown_ms);
        series.key.extend(t.key_ms);
        if let (Some(w), Some(p)) = (t.window_ms, t.prepare_ms) {
            series.prepare_to_window.push(w.saturating_sub(p));
        }
    }
    let total = rounds.max(overlay_rounds);
    for i in 0..total {
        if i < rounds {
            let boot = !ab || i % 2 == 0;
            wm.lab_set_boot_prepare(boot);
            for (name, input) in [("translate", false), ("input", true)] {
                let (idle, cold, sample) = wait_webkit_cold(app, min_idle).await;
                let load = super::native::lab_load_average();
                log::info!(
                    "window_lab cold_before view={name} round={i} boot_prepare={boot} {}",
                    sample.line()
                );
                let cmd = if input {
                    Command::Action {
                        action: wisp_types::HotkeyAction::TranslateInput,
                        origin: TriggerOrigin::Hotkey,
                    }
                } else {
                    Command::TranslateText {
                        text: text.clone(),
                        origin: TriggerOrigin::Cli,
                    }
                };
                let t = bus_trigger(&state, cmd, input).await;
                cold_js_split(app, name, i, boot).await;
                let series = match (input, boot) {
                    (false, true) => &mut translate_s,
                    (true, true) => &mut input_s,
                    (false, false) => &mut translate_nb,
                    (true, false) => &mut input_nb,
                };
                panel_round(name, i, idle, cold, load, t, series);
                tokio::time::sleep(Duration::from_millis(400)).await;
                hide_by_frontend(wm).await;
                wait_unloaded().await;
            }
        }
        if i < overlay_rounds
            && let Some(m) = primary.clone()
        {
            let (idle, cold, sample) = wait_webkit_cold(app, min_idle).await;
            let load = super::native::lab_load_average();
            log::info!("window_lab cold_before view=overlay round={i} {}", sample.line());
            let session_id = 9100 + i as u64;
            let session = OverlaySession {
                session_id,
                monitors: vec![m.clone()],
                mode: OverlayMode::Ocr,
            };
            // 有屏幕录制权限时带真实冻结帧（SCK 的 CGImage：原生冻结帧图层 + 页面的放大镜），与 ⌥S 的流程一致
            let (frames, real_frame) = lab_frames(std::slice::from_ref(&m)).await;
            let frame = frames.into_iter().next().filter(|_| real_frame);
            let t0 = Instant::now();
            let created = {
                let wm = Arc::clone(wm);
                tauri::async_runtime::spawn_blocking(move || wm.create_overlays(&session).map(|()| Instant::now()))
                    .await
                    .ok()
                    .and_then(Result::ok)
            };
            let shown = {
                let wm = Arc::clone(wm);
                let id = m.id.clone();
                tauri::async_runtime::spawn_blocking(move || {
                    wm.overlay_show(session_id, &id, frame.as_ref())
                        .map(|native| (Instant::now(), native))
                })
                .await
                .ok()
                .and_then(Result::ok)
            };
            let native_frame = shown.is_some_and(|(_, native)| native);
            let shown = shown.map(|(at, _)| at);
            let label = app
                .webview_windows()
                .into_keys()
                .find(|l| wm.overlay_session_of(l) == Some(session_id));
            let mut init = None;
            while init.is_none() && t0.elapsed() < Duration::from_secs(3) {
                init = crate::orchestrator::ocr::lab_overlay_inits_since(t0)
                    .into_iter()
                    .find(|(l, _)| label.as_deref() == Some(l.as_str()))
                    .map(|(_, at)| at);
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            let after_show = shown.zip(init).map(|(s, i)| i.saturating_duration_since(s));
            let after_create = init.map(|i| i.saturating_duration_since(t0));
            log::info!(
                "window_lab cold view=overlay round={i} idle_s={} webkit_cold={cold} native_frame={native_frame} load1={} \
                 created_ms={:?} shown_ms={:?} show_to_init_ms={:?} create_to_init_ms={:?}",
                idle.as_secs(),
                load.map_or_else(|| "-".to_owned(), |l| format!("{l:.2}")),
                created.map(|c| c.saturating_duration_since(t0).as_millis()),
                shown.map(|s| s.saturating_duration_since(t0).as_millis()),
                after_show.map(|d| d.as_millis()),
                after_create.map(|d| d.as_millis()),
            );
            match (after_show, after_create) {
                (Some(s), Some(c)) => {
                    init_after_show.push(s);
                    init_after_create.push(c);
                }
                _ => overlay_missing += 1,
            }
            let wm2 = Arc::clone(wm);
            let _ = tauri::async_runtime::spawn_blocking(move || wm2.destroy_overlays()).await;
        }
    }
    wm.lab_set_boot_prepare(true);
    let budget = 150u32;
    let boot_rounds = if ab { rounds.div_ceil(2) } else { rounds };
    // A/B 的对照组（不带 Boot.prepare）只报告、不判定
    for (name, s) in [("translate", &translate_nb), ("input", &input_nb)] {
        if !ab || rounds < 2 {
            break;
        }
        step(
            &format!("cold_show_{name}_no_boot_prepare"),
            true,
            format!(
                "A/B control (Boot.prepare off): trigger→card on screen {}; trigger→first content frame {}; trigger→key {}",
                ms_percentiles(&s.window),
                ms_percentiles(&s.painted),
                ms_percentiles(&s.key)
            ),
        );
    }
    for (name, s) in [("translate", &translate_s), ("input", &input_s)] {
        if rounds == 0 {
            break;
        }
        let w = ms_percentiles(&s.window);
        let p = ms_percentiles(&s.painted);
        let pw = ms_percentiles(&s.prepare_to_window);
        let k = ms_percentiles(&s.key);
        let within = |q: &Percentiles| q.p95.is_some_and(|d| d <= Duration::from_millis(budget.into()));
        step(
            &format!("cold_show_{name}"),
            w.n == boot_rounds && within(&w),
            format!(
                "cold-show budget {budget} ms on trigger→card on screen: {w}; trigger→first content frame {p} (within budget: {}); \
                 prepare→window {pw}; trigger→key {k}; rounds where WebKit was not fully cold: {}",
                within(&p),
                s.not_cold
            ),
        );
    }
    if overlay_rounds > 0 {
        let s = Percentiles::of(init_after_show.clone());
        let c = Percentiles::of(init_after_create);
        let mut sorted = init_after_show;
        sorted.sort();
        let p99 = sorted
            .get(((sorted.len().max(1) - 1) as f64 * 0.99).round() as usize)
            .copied();
        step(
            "cold_overlay_init",
            s.n == overlay_rounds && overlay_missing == 0 && p99.is_some_and(|d| d < Duration::from_millis(750)),
            format!(
                "show→overlay_init {s} p99={p99:?}; create→overlay_init {c}; missing={overlay_missing}; watchdog 1500 ms"
            ),
        );
    }
    // 真正空闲 10 分钟之后（与 70 s 口径对照）
    for j in 0..long_rounds {
        let input = j % 2 == 1;
        let name = if input { "input" } else { "translate" };
        let (idle, cold, sample) = wait_webkit_cold(app, Duration::from_secs(600)).await;
        let load = super::native::lab_load_average();
        log::info!("window_lab cold_before view={name} long_round={j} {}", sample.line());
        let cmd = if input {
            Command::Action {
                action: wisp_types::HotkeyAction::TranslateInput,
                origin: TriggerOrigin::Hotkey,
            }
        } else {
            Command::TranslateText {
                text: text.clone(),
                origin: TriggerOrigin::Cli,
            }
        };
        let t = bus_trigger(&state, cmd, input).await;
        let mut series = Series::default();
        panel_round(
            if input { "input_long" } else { "translate_long" },
            j,
            idle,
            cold,
            load,
            t,
            &mut series,
        );
        tokio::time::sleep(Duration::from_millis(400)).await;
        hide_by_frontend(wm).await;
        wait_unloaded().await;
    }
    controller.set_options(original);
    step("done", true, "cold mode");
}

/// 浮点样本的分位数（`round((n − 1)·p)`，与 [`Percentiles`] 同口径）
fn pct_f(v: &mut [f64], p: f64) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    v.get(((v.len() - 1) as f64 * p).round() as usize).copied()
}

/// 一组毫秒样本的摘要：`n=… p50=… p95=… max=… samples=[…]`（原始样本一并写出，A/B 的离线分析要用）
fn ms_summary(v: &[f64]) -> String {
    let mut s = v.to_vec();
    let f = |x: Option<f64>| x.map_or_else(|| "-".to_owned(), |x| format!("{x:.1}"));
    let raw: Vec<String> = v.iter().map(|x| format!("{x:.1}")).collect();
    format!(
        "n={} p50={} p95={} max={} samples=[{}]",
        v.len(),
        f(pct_f(&mut s, 0.5)),
        f(pct_f(&mut s, 0.95)),
        f(pct_f(&mut s, 1.0)),
        raw.join(",")
    )
}

/// 一段高度运动的指标（Native / Css 两种帧模式的菜单预留轨迹对照）：从 `from` 到 `to`，`samples` 是
/// （相对起点的 ms, 高度）。`tX`：此后一直留在目标的 X 之内的第一个样本时刻（90 % / 98 % 的位移、1 px）；`overshoot`：
/// 越过目标的最大距离（px）。位移不到 1 px 时为 None
fn motion_metrics(from: f64, to: f64, samples: &[(f64, f64)]) -> Option<String> {
    let d = to - from;
    if d.abs() < 1.0 || samples.is_empty() {
        return None;
    }
    let settled = |tol: f64| {
        let mut at = None;
        for &(t, h) in samples {
            if (h - to).abs() <= tol {
                at.get_or_insert(t);
            } else {
                at = None;
            }
        }
        at
    };
    let f = |x: Option<f64>| x.map_or_else(|| "-".to_owned(), |x| format!("{x:.1}"));
    let overshoot = samples
        .iter()
        .map(|&(_, h)| (h - to) * d.signum())
        .fold(0.0_f64, f64::max);
    Some(format!(
        "from={from:.1} to={to:.1} t90_ms={} t98_ms={} t1px_ms={} overshoot_px={overshoot:.2} frames={}",
        f(settled(d.abs() * 0.1)),
        f(settled(d.abs() * 0.02)),
        f(settled(1.0)),
        samples.len()
    ))
}

/// 每次向页面要结果用的序号（[`read_lab_title`]）
fn lab_nonce() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// 读页面写进 `document.title` 的 `lab:<nonce>:<json>`（[`cold_js_split`] 同一个办法），最多等 `limit`。`nonce` 区分每一次：
/// 同一个页面的 title 跨会话保留，不带它会读到上一轮留下的结果。WKWebView 的 `title` 最长约 1000 个字符：页面只写摘要
async fn read_lab_title(window: &tauri::WebviewWindow, nonce: u64, limit: Duration) -> Option<serde_json::Value> {
    let prefix = format!("lab:{nonce}:");
    let t0 = Instant::now();
    let mut last = None;
    while t0.elapsed() < limit {
        tokio::time::sleep(Duration::from_millis(30)).await;
        let w = window.clone();
        let title = tauri::async_runtime::spawn_blocking(move || super::native::lab_webview_title(&w))
            .await
            .ok()
            .flatten();
        if let Some(json) = title.as_deref().and_then(|t| t.strip_prefix(prefix.as_str())) {
            match serde_json::from_str(json) {
                Ok(v) => return Some(v),
                // WKWebView 的 `title` 最长约 1000 个字符，更长的结果被截断：页面那边只写摘要
                Err(e) => {
                    log::warn!("window_lab: page result is not JSON ({e}): {} chars", json.len());
                    return None;
                }
            }
        }
        last = title;
    }
    log::warn!(
        "window_lab: no page result within {limit:?}; last title {:?}",
        last.map(|t| t.chars().take(120).collect::<String>())
    );
    None
}

/// 菜单预留的高度轨迹：翻译面板显示、内容稳定之后，经页面点开翻译的 ⋯ 菜单 / 风格菜单
/// （卡片为菜单预留高度 → 长高），900 ms 后按 Esc 关掉（释放预留 → 回收；菜单打开时卡片其余部分是 inert，再 `click()` 一次
/// 触发器按 HTML 规范什么也不做），各 `WISP_WINDOW_LAB_MENU_ROUNDS` 轮（默认 3）。
/// Native 帧模式记 FrameDriver 逐帧写进窗口的高度与 `panel_resize` 带来的弹簧（[`super::native::lab_take_height_traces`]）；
/// Css 帧模式（`WISP_WINDOW_LAB_FRAME_MODE=css`）由页面逐帧读卡片的高度（Motion 弹簧）。两种都以页面里点击的时刻为零点，
/// 输出 `window_lab menu_spring mode=… menu=… phase=open|close spring=… click_to_motion_ms=… t90_ms=… t98_ms=… …`
async fn menu_spring(app: &tauri::AppHandle, wm: &Arc<WindowManager>) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let rounds: usize = std::env::var("WISP_WINDOW_LAB_MENU_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let native = wm.panel_controller().frame_mode() == wisp_types::FrameMode::Native;
    let mode = if native { "native" } else { "css" };
    super::native::set_frame_stats(true);
    let mut measured = 0usize;
    let mut snappy = 0usize;
    // 依次轮换的菜单（`WISP_WINDOW_LAB_MENUS=style,more` 换顺序或只测一种）
    let menus: Vec<String> = std::env::var("WISP_WINDOW_LAB_MENUS")
        .unwrap_or_else(|_| "more,style".to_owned())
        .split(',')
        .map(|m| m.trim().to_owned())
        .filter(|m| m == "more" || m == "style")
        .collect();
    let menus = if menus.is_empty() {
        vec!["more".to_owned(), "style".to_owned()]
    } else {
        menus
    };
    for i in 0..rounds * menus.len() {
        let menu = menus[i % menus.len()].as_str();
        let selector = if menu == "more" {
            "[data-testid=\"tv-more\"]"
        } else {
            "[data-testid=\"tv-style-chip\"]"
        };
        let t = bus_trigger(
            &state,
            crate::bus::Command::TranslateText {
                text: "Menus reserve room inside the card.".to_owned(),
                origin: crate::bus::TriggerOrigin::Cli,
            },
            false,
        )
        .await;
        if t.is_none_or(|t| t.shown_ms.is_none()) {
            log::warn!("window_lab menu_spring round={i}: panel not shown");
            continue;
        }
        // 假模型流式输出完、高度弹簧静止
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let _ = super::native::lab_take_height_traces();
        let Some(w) = app.get_webview_window(PANEL_LABEL) else {
            continue;
        };
        // 页面逐帧读卡片的高度（Css 帧模式里就是 Motion 的弹簧），两相各自算出指标再写回（title 放不下原始样本）
        let nonce = lab_nonce();
        let js = format!(
            "(function(){{var c=document.querySelector('.panel-card'),b=document.querySelector('{selector}');\
if(!c||!b){{document.title='lab:{nonce}:'+JSON.stringify({{error:'missing'}});return}}\
var S=[],on=true,t0=0,o=performance.timeOrigin,r={{}};\
function h(){{return Math.round(c.getBoundingClientRect().height*100)/100}}\
function f(){{if(!on)return;S.push([Math.round((performance.now()-t0)*10)/10,h()]);requestAnimationFrame(f)}}\
function M(g){{if(g.length<2)return null;var a=g[0][1],z=g[g.length-1][1],d=z-a,i,st=null,os=0;\
function q(tol){{var at=null;for(i=0;i<g.length;i++){{if(Math.abs(g[i][1]-z)<=tol){{if(at===null)at=g[i][0]}}else at=null}}return at}}\
for(i=0;i<g.length;i++){{if(st===null&&Math.abs(g[i][1]-a)>0.5)st=g[i][0];os=Math.max(os,(g[i][1]-z)*Math.sign(d))}}\
return {{from:a,to:z,start:st,t90:q(Math.abs(d)*0.1),t98:q(Math.abs(d)*0.02),t1:q(1),os:Math.round(os*100)/100,n:g.length}}}}\
requestAnimationFrame(function(){{t0=performance.now();r.open_unix=o+t0;b.click();f();\
setTimeout(function(){{r.close_ms=Math.round((performance.now()-t0)*10)/10;r.close_unix=o+performance.now();var e=document.activeElement||document.body;e.dispatchEvent(new KeyboardEvent('keydown',{{key:'Escape',code:'Escape',bubbles:true,cancelable:true}}));\
setTimeout(function(){{on=false;var k=r.close_ms;\
r.open=M(S.filter(function(p){{return p[0]<k}}));\
r.close=M(S.filter(function(p){{return p[0]>=k}}).map(function(p){{return [Math.round((p[0]-k)*10)/10,p[1]]}}));\
document.title='lab:{nonce}:'+JSON.stringify(r)}},900)}},900)}})}})()"
        );
        if w.eval(&js).is_err() {
            continue;
        }
        let Some(page) = read_lab_title(&w, nonce, Duration::from_secs(4)).await else {
            log::warn!("window_lab menu_spring round={i} menu={menu}: no result from the page");
            hide_by_frontend(wm).await;
            continue;
        };
        if page.get("error").is_some() {
            log::warn!("window_lab menu_spring round={i} menu={menu}: {page}");
            hide_by_frontend(wm).await;
            continue;
        }
        let num = |k: &str| page.get(k).and_then(serde_json::Value::as_f64);
        let (Some(open_unix), Some(close_unix)) = (num("open_unix"), num("close_unix")) else {
            hide_by_frontend(wm).await;
            continue;
        };
        // Native：两相的轨迹在页面写结果之前都已结束，一次取走、按开始时刻归到各自的点击（点击 → Rust 收到 panel_resize
        // 有一两帧）
        let traces = if native {
            super::native::lab_take_height_traces()
        } else {
            Vec::new()
        };
        for (phase, click_unix) in [("open", open_unix), ("close", close_unix)] {
            let line = if native {
                traces
                    .iter()
                    .find(|tr| (0.0..600.0).contains(&(tr.start_unix_ms - click_unix)))
                    .map(|tr| {
                        let offset = tr.start_unix_ms - click_unix;
                        let to = tr.targets.last().map_or(tr.from, |t| t.1);
                        let springs: Vec<&str> = tr.targets.iter().map(|t| t.2).collect();
                        if springs.first() == Some(&"snappy") {
                            snappy += 1;
                        }
                        let shifted: Vec<(f64, f64)> = tr.samples.iter().map(|&(t, h)| (t + offset, h)).collect();
                        format!(
                            "spring={} retargets={} click_to_motion_ms={offset:.1} {}",
                            springs.join("+"),
                            tr.targets.len(),
                            motion_metrics(tr.from, to, &shifted).unwrap_or_else(|| "no-motion".to_owned())
                        )
                    })
            } else {
                // 页面逐帧读到的卡片高度（Motion 弹簧），零点是点击；指标由页面算好（与 [`motion_metrics`] 同一套定义）
                page.get(phase).filter(|m| !m.is_null()).map(|m| {
                    let f = |k: &str| {
                        m.get(k)
                            .and_then(serde_json::Value::as_f64)
                            .map_or_else(|| "-".to_owned(), |v| format!("{v:.1}"))
                    };
                    format!(
                        "spring=motion click_to_motion_ms={} from={} to={} t90_ms={} t98_ms={} t1px_ms={} overshoot_px={} \
                         frames={}",
                        f("start"),
                        f("from"),
                        f("to"),
                        f("t90"),
                        f("t98"),
                        f("t1"),
                        f("os"),
                        f("n")
                    )
                })
            };
            match line {
                Some(l) => {
                    measured += 1;
                    log::info!("window_lab menu_spring mode={mode} menu={menu} phase={phase} round={i} {l}");
                }
                None => log::info!(
                    "window_lab menu_spring mode={mode} menu={menu} phase={phase} round={i}: no motion within 600 ms of the \
                     click (page card: {}; height traces relative to it: {:?})",
                    page.get(phase).map_or_else(|| "-".to_owned(), ToString::to_string),
                    traces
                        .iter()
                        .map(|tr| (
                            (tr.start_unix_ms - click_unix).round(),
                            tr.from,
                            tr.targets.last().map(|t| t.1),
                            tr.targets.iter().map(|t| t.2).collect::<Vec<_>>()
                        ))
                        .collect::<Vec<_>>()
                ),
            }
        }
        hide_by_frontend(wm).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    super::native::set_frame_stats(false);
    step(
        "menu_spring",
        measured > 0 && (!native || snappy > 0),
        format!("mode={mode} measured={measured} native_snappy={snappy}"),
    );
}

/// 连续 `n` 次 prepare→shown 计时（两次之间以 Paste 立即隐藏、隔 60 ms；与第 8 步同口径），返回每次的毫秒
async fn prepare_to_shown_series(wm: &Arc<WindowManager>, n: u32) -> Vec<f64> {
    let mut samples = Vec::new();
    for i in 0..n {
        let anchor = if i % 2 == 0 {
            AnchorSource::Cursor
        } else {
            AnchorSource::Center
        };
        let s = prepare(wm, request(PanelView::Translate, anchor, translate("timing"))).await;
        match wait_shown(wm, s, Duration::from_secs(2)).await {
            Some(d) => samples.push(d.as_secs_f64() * 1000.0),
            None => step("latency", false, format!("session={s} not shown")),
        }
        wm.panel_hide_now(s, HideReason::Paste);
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    samples
}

/// 可选（`WISP_WINDOW_LAB_LATENCY=1`，`MallocSpaceEfficient=1` 的延迟复核）：同一个进程里依次测
/// ① `overlay`：真实截图流程的 Overlay 上屏——经总线触发 OCR（SCK 截图、原生冻结帧、页面 init），记 perf 的 `overlay_ms`
///   （触发 → 第一块屏上屏）与触发 → 主屏页面 `overlay_init`，`WISP_WINDOW_LAB_OVERLAY_ROUNDS` 轮（默认 20），每轮由编排器取消；
/// ② `latency`：连续 100 次 prepare → shown（第 8 步同口径）；
/// ③ `input`：输入视图的 ready → 上屏拆分（[`input_split`]，`WISP_WINDOW_LAB_INPUT_SPLIT` 轮，默认 10）；
/// ④ `frames`：面板进入淡入与 Native 高度弹簧的帧间隔——逐帧统计打开，`WISP_WINDOW_LAB_FRAME_SHOWS` 次翻译（默认 12，假模型
///   流式输出让卡片长高），按「这一帧有淡入淡出 / 有高度弹簧」分两组记帧间隔；
/// ⑤ `menu`：菜单预留的高度轨迹（[`menu_spring`]）。
/// `WISP_WINDOW_LAB_LATENCY_STEPS=overlay,latency,…` 只跑其中几步。环境变量不切换分配器（它在进程启动时读环境），由外部脚本
/// 交替带 / 不带 `MallocSpaceEfficient=1` 启动同一个二进制；第一行日志记下这次进程属于哪一组与当时的负载
async fn latency_suite(app: &tauri::AppHandle, wm: &Arc<WindowManager>) {
    use crate::bus::{Command, TriggerOrigin};
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    if std::env::var("WISP_FAKE_LLM").is_ok_and(|v| v == "1") {
        crate::app::smoke::seed(&state.settings);
    }
    let steps: Vec<String> = std::env::var("WISP_WINDOW_LAB_LATENCY_STEPS")
        .unwrap_or_else(|_| "overlay,latency,input,frames,menu".to_owned())
        .split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    let env_num = |name: &str, default: u64| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(default)
    };
    log::info!(
        "window_lab latency config malloc_space_efficient={} frame_mode={:?} steps={steps:?} load1={}",
        std::env::var("MallocSpaceEfficient").is_ok_and(|v| v == "1"),
        wm.panel_controller().frame_mode(),
        super::native::lab_load_average().map_or_else(|| "-".to_owned(), |l| format!("{l:.2}"))
    );
    // 启动时预热的面板先用一次（之后各步都落在热页面上）
    let _ = traced_show(
        app,
        wm,
        request(PanelView::Translate, AnchorSource::Cursor, translate("warm")),
    )
    .await;
    hide_by_frontend(wm).await;
    tokio::time::sleep(Duration::from_millis(800)).await;

    if steps.iter().any(|s| s == "overlay") {
        let rounds = env_num("WISP_WINDOW_LAB_OVERLAY_ROUNDS", 20);
        let primary = state.platform.screen.monitors().into_iter().find(|m| m.is_primary);
        let (mut overlay_ms, mut init_ms) = (Vec::new(), Vec::new());
        let mut missing = 0;
        for i in 0..rounds {
            let after = state.perf.last_id();
            let t0 = Instant::now();
            state.bus.send(Command::Action {
                action: wisp_types::HotkeyAction::Ocr,
                origin: TriggerOrigin::Cli,
            });
            let mut target = None;
            let mut shown = None;
            while (target.is_none() || shown.is_none()) && t0.elapsed() < Duration::from_secs(4) {
                tokio::time::sleep(Duration::from_millis(2)).await;
                shown = shown.or_else(|| state.perf.first_after(after).and_then(|t| t.overlay_ms));
                target = target.or_else(|| {
                    crate::orchestrator::ocr::lab_overlay_inits_since(t0)
                        .into_iter()
                        .find_map(|(label, at)| {
                            let monitor = wm.overlay_monitor(&label)?;
                            let session = wm.overlay_session_of(&label)?;
                            primary
                                .as_ref()
                                .is_none_or(|p| p.id == monitor)
                                .then_some((session, at.saturating_duration_since(t0)))
                        })
                });
            }
            log::info!(
                "window_lab latency overlay round={i} overlay_ms={shown:?} init_ms={:?}",
                target.map(|(_, d)| d.as_secs_f64() * 1000.0)
            );
            match (shown, target) {
                (Some(o), Some((session, d))) => {
                    overlay_ms.push(f64::from(o));
                    init_ms.push(d.as_secs_f64() * 1000.0);
                    state.orchestrator.overlay_cancel(session);
                }
                (_, target) => {
                    missing += 1;
                    if let Some((session, _)) = target {
                        state.orchestrator.overlay_cancel(session);
                    }
                }
            }
            let t1 = Instant::now();
            while app.webview_windows().keys().any(|l| l.starts_with("overlay-"))
                && t1.elapsed() < Duration::from_secs(3)
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            tokio::time::sleep(Duration::from_millis(600)).await;
        }
        log::info!("window_lab latency overlay_ms {}", ms_summary(&overlay_ms));
        log::info!("window_lab latency overlay_init_ms {}", ms_summary(&init_ms));
        step(
            "latency_overlay",
            missing == 0 && !overlay_ms.is_empty(),
            format!("rounds={rounds} missing={missing}"),
        );
    }
    if steps.iter().any(|s| s == "latency") {
        let samples = prepare_to_shown_series(wm, 100).await;
        log::info!("window_lab latency prepare_to_shown_ms {}", ms_summary(&samples));
        step(
            "latency_prepare_to_shown",
            samples.len() == 100,
            format!("n={}", samples.len()),
        );
    }
    if steps.iter().any(|s| s == "input") {
        input_split(app, wm, env_num("WISP_WINDOW_LAB_INPUT_SPLIT", 10) as usize).await;
    }
    if steps.iter().any(|s| s == "frames") {
        let shows = env_num("WISP_WINDOW_LAB_FRAME_SHOWS", 12);
        super::native::set_frame_stats(true);
        let _ = super::native::lab_take_frame_trace();
        let texts = [
            "The quick brown fox jumps over the lazy dog.",
            "Frame pacing of the enter fade and the height spring.",
            "A short sentence.",
            "Translation panels should stay light while hidden.",
        ];
        for i in 0..shows as usize {
            let _ = bus_trigger(
                &state,
                Command::TranslateText {
                    text: texts[i % texts.len()].repeat(1 + i % 3),
                    origin: TriggerOrigin::Cli,
                },
                false,
            )
            .await;
            tokio::time::sleep(Duration::from_millis(1200)).await;
            hide_by_frontend(wm).await;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let trace = super::native::lab_take_frame_trace();
        super::native::set_frame_stats(false);
        let nominal = f64::from(trace.nominal_ms);
        for (kind, v) in [("fade", &trace.fade_ms), ("height", &trace.height_ms)] {
            let mut v: Vec<f64> = v.iter().map(|&x| f64::from(x)).collect();
            let late = v.iter().filter(|&&x| nominal > 0.0 && x > nominal * 1.5).count();
            let f = |x: Option<f64>| x.map_or_else(|| "-".to_owned(), |x| format!("{x:.2}"));
            log::info!(
                "window_lab latency frames kind={kind} n={} nominal_ms={nominal:.2} p50={} p95={} p99={} max={} late={late}",
                v.len(),
                f(pct_f(&mut v, 0.5)),
                f(pct_f(&mut v, 0.95)),
                f(pct_f(&mut v, 0.99)),
                f(pct_f(&mut v, 1.0)),
            );
        }
        let mut cb: Vec<f64> = trace.callback_us.iter().map(|&x| f64::from(x)).collect();
        let f = |x: Option<f64>| x.map_or_else(|| "-".to_owned(), |x| format!("{x:.0}"));
        log::info!(
            "window_lab latency frames kind=callback_us n={} p50={} p95={} p99={} max={}",
            cb.len(),
            f(pct_f(&mut cb, 0.5)),
            f(pct_f(&mut cb, 0.95)),
            f(pct_f(&mut cb, 0.99)),
            f(pct_f(&mut cb, 1.0)),
        );
        step(
            "latency_frames",
            !trace.fade_ms.is_empty() || !super::native::HAS_WINDOW_CENSUS,
            format!(
                "fade frames={} height frames={}",
                trace.fade_ms.len(),
                trace.height_ms.len()
            ),
        );
    }
    if steps.iter().any(|s| s == "menu") {
        menu_spring(app, wm).await;
    }
    step("done", true, "latency mode");
}

/// 冷唤出的外壳期间打字（macOS 的 Native 帧模式）：面板卸载之后唤出输入视图，`panel:prepare` 推迟约 700 ms
/// 送达（模拟冷页面上监听迟迟没注册，外壳在屏幕上多留一会儿）。外壳上屏时它必须已经是 key、第一响应者是 WKWebView、
/// Wisp 在系统层面不在前台；这时往事件队列里送 "wisp" 四个字（与窗口服务器送来的按键走同一条派发路径；带
/// `WISP_WINDOW_LAB_INPUT_PROBE=1` 时再请外部脚本经窗口服务器注入一个 F18，确认真实按键同样落在外壳上）。ready 之后输入框
/// 里必须正好是 "wisp"（经 `document.title` 读出）——没有一个字落到别的应用、也没有丢；之后 Wisp 仍不在前台
async fn lab_cold_keys(app: &tauri::AppHandle, wm: &Arc<WindowManager>) {
    const TEXT: &str = "wisp";
    if wm.frame_mode() != wisp_types::FrameMode::Native {
        return;
    }
    let probe_input = std::env::var("WISP_WINDOW_LAB_INPUT_PROBE").is_ok_and(|v| v == "1");
    let t0 = Instant::now();
    while app.get_webview_window(PANEL_LABEL).is_some() && t0.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    // 只在 Wisp 真的在前台时（终端直接启动的进程第一次显示窗口时会被系统激活）才把前台交还，并等它生效：经 LaunchServices
    // 启动时 Wisp 从来不在前台，平白去激活别的应用反而可能在外壳成为 key 之后才生效、把 key 抢走（失焦即收起输入视图）
    if super::native::app_is_active(app) == Some(true) {
        super::native::lab_resign_active(app);
        let t0 = Instant::now();
        while super::native::app_is_active(app) != Some(false) && t0.elapsed() < Duration::from_secs(2) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    // 带外部注入时多留一点：驱动脚本看到日志再注入要几百毫秒（仍远在 ready 看门狗之内）
    wm.lab_hold_prepare(Duration::from_millis(if probe_input { 900 } else { 700 }));
    let s = prepare(wm, request(PanelView::Input, AnchorSource::Center, input_payload())).await;
    // 等外壳上屏（会话可见、还没 ready）
    let t0 = Instant::now();
    let mut on_shell = false;
    while t0.elapsed() < Duration::from_secs(2) {
        if let Some(snap) = wm.panel_controller().snapshot()
            && snap.session_id == s
            && snap.visible
            && !snap.shown
        {
            on_shell = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let window = app.get_webview_window(PANEL_LABEL);
    let focus = |w: Option<tauri::WebviewWindow>| async move {
        let w = w?;
        tauri::async_runtime::spawn_blocking(move || super::native::lab_panel_focus_state(&w))
            .await
            .ok()
            .flatten()
    };
    let shell_focus = focus(window.clone()).await;
    let posted = match window.clone() {
        Some(w) => tauri::async_runtime::spawn_blocking(move || super::native::lab_post_text(&w, TEXT))
            .await
            .ok()
            .and_then(Result::ok),
        None => None,
    };
    // 合成的按键排在事件队列末尾，由主线程稍后派发给外壳的守卫：等它们都被截住再读计数（主线程慢时——例如
    // NSZombieEnabled——立即读会是 0）
    let wait_captured = |n: usize| async move {
        let t1 = Instant::now();
        while super::native::lab_shell_key_stats().0 < n && t1.elapsed() < INPUT_PROBE_WAIT {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    wait_captured(TEXT.len() * 2).await;
    if probe_input {
        log::info!("window_lab input_probe=key code=79");
        wait_captured(TEXT.len() * 2 + 2).await;
    }
    let (captured, _) = super::native::lab_shell_key_stats();
    let still_shell = wm
        .panel_controller()
        .snapshot()
        .is_some_and(|p| p.session_id == s && !p.shown);
    let shown = wait_shown(wm, s, Duration::from_secs(3)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (_, replayed) = super::native::lab_shell_key_stats();
    let mut typed = None;
    if let Some(w) = window.clone() {
        let _ = w.eval("document.title = 'lab:' + ((document.activeElement && document.activeElement.value) || '')");
        let t1 = Instant::now();
        while t1.elapsed() < Duration::from_secs(1) {
            tokio::time::sleep(Duration::from_millis(30)).await;
            let w = w.clone();
            let title = tauri::async_runtime::spawn_blocking(move || super::native::lab_webview_title(&w))
                .await
                .ok()
                .flatten();
            if let Some(t) = title.and_then(|t| t.strip_prefix("lab:").map(str::to_owned)) {
                typed = Some(t);
                break;
            }
        }
    }
    let after = super::native::app_is_active(app);
    let want_events = TEXT.len() * 2 + if probe_input { 2 } else { 0 };
    let ok = on_shell
        && still_shell
        && shell_focus.is_some_and(|(active, key, responder)| !active && key && responder)
        && posted == Some(TEXT.len())
        && captured >= want_events
        && replayed >= want_events
        && typed.as_deref() == Some(TEXT)
        && after == Some(false);
    step(
        "cold_show_keys",
        ok,
        format!(
            "session={s} shell={on_shell} focus_on_shell(system_active,key,responder_is_web)={shell_focus:?} \
             posted={posted:?} captured={captured} replayed={replayed} (want ≥ {want_events}) keys_before_ready={still_shell} \
             shown={shown:?} input={typed:?} system_active_after={after:?}"
        ),
    );
    hide_by_frontend(wm).await;
    let t0 = Instant::now();
    while app.get_webview_window(PANEL_LABEL).is_some() && t0.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 启动实验台（面板预热完成后调用）
pub fn spawn(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        // 等面板页面加载与 AppState 就绪
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let Some(state) = app.try_state::<AppState>() else {
            log::error!("window_lab: AppState missing");
            return;
        };
        let wm: Arc<WindowManager> = Arc::clone(&state.windows);
        let monitors = state.platform.screen.monitors();
        super::native::lab_remember_frontmost();
        step("monitors", !monitors.is_empty(), format!("count={}", monitors.len()));
        // 窗口泄漏基线：此刻只有预热的面板。实验台销毁的 pinned / Overlay / 设置窗口与卸载前的面板必须真的释放
        // （见第 10 步）
        let census_at_start = census(&app).await;
        // 可选（`WISP_WINDOW_LAB_MEM_TRACE=1`）：完整实验台的每一步之后各采样一次内存（`lab_after_<步骤>`），
        // 找出完整实验台之后 GPU 进程停在 70 MiB 以上是哪一步留下的
        let mem_trace = std::env::var("WISP_WINDOW_LAB_MEM_TRACE").is_ok_and(|v| v == "1");
        if mem_trace {
            sample_phase(&app, "lab_start", 1).await;
        }
        if std::env::var("WISP_WINDOW_LAB_MEMORY").is_ok_and(|v| v == "1") {
            memory_phases(&app, &wm).await;
            finish(&app);
            return;
        }
        if let Some(rounds) = std::env::var("WISP_WINDOW_LAB_INPUT_SPLIT")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0)
        {
            input_split(&app, &wm, rounds).await;
            finish(&app);
            return;
        }
        if std::env::var("WISP_WINDOW_LAB_COLD").is_ok_and(|v| v == "1") {
            let primary = monitors.iter().find(|m| m.is_primary).or(monitors.first()).cloned();
            cold_rounds(&app, &wm, primary).await;
            finish(&app);
            return;
        }
        if std::env::var("WISP_WINDOW_LAB_LATENCY").is_ok_and(|v| v == "1") {
            latency_suite(&app, &wm).await;
            finish(&app);
            return;
        }
        if std::env::var("WISP_WINDOW_LAB_DRAG").is_ok_and(|v| v == "1") {
            drag_suite(&app, &wm).await;
            finish(&app);
            return;
        }
        if std::env::var("WISP_WINDOW_LAB_DOCK_ICON").is_ok_and(|v| v == "1") {
            dock_icon(&app, &wm).await;
            finish(&app);
            return;
        }

        // 1. 光标锚点的翻译面板
        let s1 = prepare(
            &wm,
            request(PanelView::Translate, AnchorSource::Cursor, translate("hello wisp")),
        )
        .await;
        let shown = wait_shown(&wm, s1, Duration::from_secs(2)).await;
        step(
            "prepare_to_shown",
            shown.is_some(),
            format!("session={s1} after={shown:?}"),
        );
        let first = wm.panel_state();
        step("panel_state", first.is_some(), format!("{first:?}"));

        // 2. 再次触发（屏幕中心）：先退出再在新位置进入
        tokio::time::sleep(Duration::from_millis(400)).await;
        let s2 = prepare(
            &wm,
            request(
                PanelView::Translate,
                AnchorSource::Center,
                translate(&"long text ".repeat(40)),
            ),
        )
        .await;
        let shown = wait_shown(&wm, s2, Duration::from_secs(2)).await;
        let second = wm.panel_state().map(|s| s.rect);
        step(
            "retrigger_moves",
            shown.is_some() && second != first.as_ref().map(|s| s.rect),
            format!("session={s2} after={shown:?} rect={second:?}"),
        );

        // 2b. 交叉淡入：锚点换成光标也保持位置；Native 帧模式下窗口矩形与会话几何一致
        tokio::time::sleep(Duration::from_millis(400)).await;
        let mut swap = request(
            PanelView::Translate,
            AnchorSource::Cursor,
            translate("crossfade keeps position"),
        );
        swap.transition = PanelTransition::Crossfade;
        let panel_frame = || {
            app.get_webview_window(PANEL_LABEL)
                .and_then(|w| super::native::window_frame(&w))
        };
        let before = panel_frame();
        let s2 = prepare(&wm, swap).await;
        let shown = wait_shown(&wm, s2, Duration::from_secs(2)).await;
        // Native 帧模式：ready 时旧视图仍不透明，窗口宽度要等它淡出完（timing.crossfade）才切换
        let at_ready = panel_frame();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let switched = panel_frame();
        if wm.frame_mode() == wisp_types::FrameMode::Native {
            let width_changed = before
                .zip(switched)
                .is_some_and(|(b, s)| (b.width - s.width).abs() >= 1.0);
            let deferred = before
                .zip(at_ready)
                .is_some_and(|(b, r)| (b.width - r.width).abs() < 1.0);
            step(
                "crossfade_width_waits_for_transparent_frame",
                !width_changed || deferred,
                format!("before={before:?} at_ready={at_ready:?} after_200ms={switched:?}"),
            );
        }
        // 等高度弹簧与富余区回收结束
        tokio::time::sleep(Duration::from_millis(700)).await;
        let swapped = wm.panel_state();
        let kept = swapped
            .as_ref()
            .zip(second)
            .is_some_and(|(now, before)| now.rect.x == before.x && now.rect.y == before.y);
        let frame = app
            .get_webview_window(PANEL_LABEL)
            .and_then(|w| super::native::window_frame(&w));
        let expected = swapped.as_ref().and_then(|s| {
            let m = monitors.iter().find(|m| m.id == s.monitor_id)?;
            Some(super::placement::Screen::from_info(m).to_desktop(s.rect, super::native::DESKTOP_UNIT))
        });
        let frame_matches = wm.frame_mode() != wisp_types::FrameMode::Native
            || frame.zip(expected).is_some_and(|(f, e)| {
                (f.x - e.x).abs() < 1.0
                    && (f.y - e.y).abs() < 1.0
                    && (f.width - e.width).abs() < 1.0
                    && (f.height - e.height).abs() < 1.0
            });
        step(
            "crossfade_keeps_position",
            shown.is_some() && kept && frame_matches,
            format!(
                "session={s2} rect={:?} frame={frame:?} expected={expected:?}",
                swapped.map(|s| s.rect)
            ),
        );

        if mem_trace {
            sample_phase(&app, "lab_after_translate", 1).await;
        }
        // 3. 固定交接：同一帧显示 pinned、隐藏面板（页面不 ready 时 1.5 s 兜底）
        tokio::time::sleep(Duration::from_millis(400)).await;
        let pin = PinState {
            kind: PinKind::Translate,
            source_text: "long text".to_owned(),
            result_text: "长文本".to_owned(),
            task: TaskKind::Translate,
            style_id: "natural".to_owned(),
            target_lang: Lang::ZhCn,
            detected_lang: None,
            request_id: None,
            capture_id: None,
            ocr_text: None,
            ocr_mode: None,
            show_source: false,
            status_line: None,
            dict_mode: false,
            model_ref: None,
            status: None,
            thumb_url: None,
            image_size: None,
            expand_width: None,
            on_top: None,
            ocr_layers: None,
        };
        match wm.pin_from_session(s2, pin.clone(), false) {
            Ok(id) => {
                tokio::time::sleep(Duration::from_millis(2200)).await;
                let label = super::pinned::label_of(&id);
                let visible = app
                    .get_webview_window(&label)
                    .and_then(|w| w.is_visible().ok())
                    .unwrap_or(false);
                let panel_hidden = wm.panel_state().is_none();
                step(
                    "pin_handoff",
                    visible && panel_hidden,
                    format!("id={id} pinned_visible={visible} panel_hidden={panel_hidden}"),
                );
                // 截图前隐藏 / 恢复
                let hidden = wm.hide_for_capture();
                let gone = app
                    .get_webview_window(&label)
                    .and_then(|w| w.is_visible().ok())
                    .is_some_and(|v| !v);
                step(
                    "hide_for_capture",
                    gone && hidden.pinned.contains(&id),
                    format!("{hidden:?}"),
                );
                wm.restore_after_capture(hidden);
                tokio::time::sleep(Duration::from_millis(200)).await;
                let back = app
                    .get_webview_window(&label)
                    .and_then(|w| w.is_visible().ok())
                    .unwrap_or(false);
                step("restore_after_capture", back, "");
                if let Some(w) = app.get_webview_window(&label) {
                    super::native::destroy_window(&w);
                }
            }
            Err(e) => step("pin_handoff", false, e.message),
        }

        if mem_trace {
            sample_phase(&app, "lab_after_pin", 1).await;
        }
        // 4. 展开固定（macOS：从面板矩形弹簧过渡到 640 宽）
        let s3 = prepare(
            &wm,
            request(PanelView::Translate, AnchorSource::Cursor, translate("expand me")),
        )
        .await;
        if wait_shown(&wm, s3, Duration::from_secs(2)).await.is_some() {
            match wm.pin_from_session(s3, pin, true) {
                Ok(id) => {
                    tokio::time::sleep(Duration::from_millis(2600)).await;
                    let label = super::pinned::label_of(&id);
                    let frame = app
                        .get_webview_window(&label)
                        .and_then(|w| super::native::window_frame(&w));
                    step("pin_expand", frame.is_some(), format!("frame={frame:?}"));
                    if let Some(w) = app.get_webview_window(&label) {
                        super::native::destroy_window(&w);
                    }
                }
                Err(e) => step("pin_expand", false, e.message),
            }
        }

        if mem_trace {
            sample_phase(&app, "lab_after_pin_expand", 1).await;
        }
        // 5. toast：不激活地显示在上次面板位置
        wm.panel_show_toast(ToastPayload {
            reason: CopiedReason::CopyOnly,
            paste_key: "⌘V".to_owned(),
            queue: None,
        });
        tokio::time::sleep(Duration::from_millis(600)).await;
        let toast = wm.panel_state();
        step(
            "toast",
            toast.as_ref().is_some_and(|s| s.view == PanelView::Toast),
            format!("{toast:?}"),
        );
        if let Some(s) = toast {
            wm.panel_hide_now(s.session_id, HideReason::Other);
        }

        if mem_trace {
            sample_phase(&app, "lab_after_toast", 1).await;
        }
        // 6. Overlay：创建、冻结帧、显示（光标所在屏成为 key）、销毁
        tokio::time::sleep(Duration::from_millis(400)).await;
        let frames = tauri::async_runtime::spawn_blocking({
            let monitors = monitors.clone();
            move || wisp_capture::capture_all(&monitors, std::process::id())
        })
        .await;
        let frames = match frames {
            Ok(Ok(set)) => set.frames,
            other => {
                log::info!("window_lab: capture unavailable ({other:?}), using fake frames");
                monitors
                    .iter()
                    .map(|m| wisp_capture::Frame::from_rgba(m.clone(), 2, 2, vec![0; 16]))
                    .collect()
            }
        };
        let session = OverlaySession {
            session_id: 9001,
            monitors: monitors.clone(),
            mode: OverlayMode::Ocr,
        };
        // 建 Overlay 之前 Wisp 是否在前台（系统层面：菜单栏 / 前台应用，见 native::app_is_active）：本来就在前台时不要求
        // 之后不在前台。从终端启动的进程第一次显示面板时会被系统激活（启动激活，与生产环境的登录自启不同），先让出前台，
        // 否则这一步的断言总是空过
        if super::native::app_is_active(&app) == Some(true) {
            super::native::lab_resign_active(&app);
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let active_before = super::native::app_is_active(&app);
        let probe_input = std::env::var("WISP_WINDOW_LAB_INPUT_PROBE").is_ok_and(|v| v == "1");
        let t0 = Instant::now();
        match wm.create_overlays(&session) {
            Ok(()) => {
                let mut native_frames = 0;
                for f in &frames {
                    match wm.overlay_show(9001, &f.monitor.id, Some(f)) {
                        Ok(true) => native_frames += 1,
                        Ok(false) => {}
                        Err(e) => step("overlay_show", false, e.message),
                    }
                }
                let labels: Vec<_> = monitors
                    .iter()
                    .filter_map(|m| {
                        app.webview_windows()
                            .keys()
                            .find(|l| wm.overlay_monitor(l).as_deref() == Some(m.id.as_str()))
                            .cloned()
                    })
                    .collect();
                step(
                    "overlay_create_show",
                    labels.len() == monitors.len(),
                    format!(
                        "labels={labels:?} native_frames={native_frames} in {} ms",
                        t0.elapsed().as_millis()
                    ),
                );
                // 不激活应用（vendor/wry 的补丁让 `focused(false)` 的 WebView 不激活应用）：显示 300 ms 后——Wisp 在系统
                // 层面不在前台（菜单栏没有切换）、光标所在屏的 Overlay 是 key、第一响应者是 WKWebView（按键进得了页面）。
                // 只有 macOS 取得到
                tokio::time::sleep(Duration::from_millis(300)).await;
                let cursor = app.try_state::<AppState>().map(|s| s.platform.screen.cursor().0);
                let key_window = labels
                    .iter()
                    .find(|l| wm.overlay_monitor(l).is_some() && wm.overlay_monitor(l) == cursor)
                    .and_then(|l| app.get_webview_window(l));
                if let Some((active, key, responder)) = key_window.as_ref().and_then(super::native::overlay_focus_state)
                {
                    step(
                        "overlay_focus",
                        (!active || active_before == Some(true)) && key && responder,
                        format!(
                            "system_active={active} (before={active_before:?}) overlay_key={key} \
                             responder_is_webview={responder}"
                        ),
                    );
                }
                // 可选（`WISP_WINDOW_LAB_INPUT_PROBE=1`，由外部驱动脚本按日志里的 `input_probe=` 行注入按键）：按键确实
                // 送到了 key 的 Overlay，且注入之后菜单栏仍不属于 Wisp
                if probe_input && key_window.is_some() {
                    lab_probe_keys(&app, "overlay_keys").await;
                }
                tokio::time::sleep(Duration::from_millis(600)).await;
                wm.destroy_overlays();
                tokio::time::sleep(Duration::from_millis(300)).await;
                let left = app
                    .webview_windows()
                    .keys()
                    .filter(|l| l.starts_with("overlay-"))
                    .count();
                step(
                    "overlay_destroy",
                    left == 0 || super::native::PREWARM_OVERLAY,
                    format!("remaining={left}"),
                );
            }
            Err(e) => step("overlay_create_show", false, e.message),
        }
        if probe_input && let Some(primary) = monitors.iter().find(|m| m.is_primary).or(monitors.first()) {
            lab_pointer_follow(&app, &wm, primary).await;
        }

        if mem_trace {
            sample_phase(&app, "lab_after_overlays", 1).await;
        }
        // 7. 设置窗口：打开（无白闪首显）、移动、关闭后记住位置；再开关两次，窗口数不随次数增长（切换 ActivationPolicy 时
        //    AppKit 会建菜单栏副本窗口，它们是 AppKit 自己缓存的，不能每次多出一批）
        wm.open_settings(Some("general".to_owned()));
        tokio::time::sleep(Duration::from_millis(1600)).await;
        let settings = app.get_webview_window(super::settings_window::SETTINGS_LABEL);
        let visible = settings.as_ref().and_then(|w| w.is_visible().ok()).unwrap_or(false);
        step("settings_open", visible, "");
        if let Some(w) = settings
            && let Some(frame) = super::native::window_frame(&w)
        {
            let moved = wisp_types::Rect::new(frame.x + 40.0, frame.y + 30.0, frame.width, frame.height);
            let _ = super::native::set_window_frame(&w, moved);
            tokio::time::sleep(Duration::from_millis(200)).await;
            super::native::destroy_window(&w);
            tokio::time::sleep(Duration::from_millis(400)).await;
            let saved = state.settings.state().get().settings_window;
            step("settings_geometry_saved", saved.is_some(), format!("{saved:?}"));
        }
        let mut cycles = Vec::new();
        for _ in 0..2 {
            wm.open_settings(Some("general".to_owned()));
            tokio::time::sleep(Duration::from_millis(1200)).await;
            if let Some(w) = app.get_webview_window(super::settings_window::SETTINGS_LABEL) {
                super::native::destroy_window(&w);
            }
            tokio::time::sleep(Duration::from_millis(600)).await;
            cycles.push(census(&app).await);
        }
        match cycles[..] {
            [Some(a), Some(b)] => step(
                "settings_cycles_stable",
                b.server_other <= a.server_other && b.server_wisp <= a.server_wisp && b.wisp <= a.wisp,
                format!("after 2nd close {a:?}, after 3rd close {b:?}"),
            ),
            // macOS 必须取得到计数（主线程超时不能让检查静默跳过）；其他平台没有这项检查
            _ if super::native::HAS_WINDOW_CENSUS => step("settings_cycles_stable", false, "census unavailable"),
            _ => {}
        }

        if mem_trace {
            sample_phase(&app, "lab_after_settings", 1).await;
        }
        // 8. 连续 100 次 prepare→shown 计时（p50 / p95；两次之间先隐藏；与冒烟模式的口径一致）
        let mut samples = Vec::new();
        for i in 0..100u32 {
            let anchor = if i % 2 == 0 {
                AnchorSource::Cursor
            } else {
                AnchorSource::Center
            };
            let s = prepare(&wm, request(PanelView::Translate, anchor, translate("timing"))).await;
            match wait_shown(&wm, s, Duration::from_secs(2)).await {
                Some(d) => samples.push(d),
                None => step("latency", false, format!("session={s} not shown")),
            }
            wm.panel_hide_now(s, HideReason::Paste);
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
        let warm = Percentiles::of(samples);
        step(
            "latency",
            warm.p95.is_some_and(|d| d <= Duration::from_millis(100)),
            warm.to_string(),
        );

        if mem_trace {
            sample_phase(&app, "lab_after_latency100", 1).await;
        }
        // 9. 空闲卸载：隐藏超过设定时长后销毁面板 WebView，下一次唤出先重建
        let controller = Arc::clone(wm.panel_controller());
        let original = controller.options();
        controller.set_options(super::panel::PanelOptions {
            unload_after: Some(Duration::from_millis(300)),
            ..original
        });
        let s = prepare(
            &wm,
            request(PanelView::Translate, AnchorSource::Cursor, translate("unload")),
        )
        .await;
        if wait_shown(&wm, s, Duration::from_secs(2)).await.is_some() {
            wm.panel_hide_now(s, HideReason::Paste);
        }
        tokio::time::sleep(Duration::from_millis(900)).await;
        let unloaded = app.get_webview_window(PANEL_LABEL).is_none();
        step("idle_unload", unloaded, "");
        if mem_trace {
            sample_phase(&app, "lab_after_unload", 1).await;
        }
        // 9a. 冷唤出的外壳期间打字：按键不落到别的应用、不丢字
        lab_cold_keys(&app, &wm).await;
        if mem_trace {
            sample_phase(&app, "lab_after_cold_keys", 1).await;
        }
        // 9b. 回收：用过的面板隐藏 recycle_after 之后销毁 WebView、立即重建干净的预热页面
        //     （换掉 WebContent 进程：会话后的 WebKit 唤醒归零、WebContent 回到约 21.5 MiB；GPU 进程的内存
        //     不随之回落）。这里把延迟缩短到 500 ms。前面几步的 pinned（刚建的 30 s 内）、设置窗口（刚打开 / 移动）会让回收
        //     推迟（`WindowManager::others_busy`），先等它们安静下来
        let t_quiet = Instant::now();
        let mut last_reason = None;
        while t_quiet.elapsed() < Duration::from_secs(40) {
            match wm.others_busy() {
                Some(reason) => last_reason = Some(reason),
                None => break,
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        step(
            "recycle_quiet",
            wm.others_busy().is_none(),
            format!(
                "other windows quiet after {} ms (last reason: {last_reason:?})",
                t_quiet.elapsed().as_millis()
            ),
        );
        controller.set_options(super::panel::PanelOptions {
            recycle_after: Some(Duration::from_millis(500)),
            ..original
        });
        let t0 = Instant::now();
        let s = prepare(
            &wm,
            request(PanelView::Translate, AnchorSource::Cursor, translate("reload")),
        )
        .await;
        let shown = wait_shown(&wm, s, Duration::from_secs(5)).await;
        step(
            "recreate_after_unload",
            shown.is_some(),
            format!("first show after unload in {} ms", t0.elapsed().as_millis()),
        );
        let loads = controller.loaded();
        let loads_before = *loads.borrow();
        let web_content = || {
            app.get_webview_window(PANEL_LABEL)
                .and_then(|w| super::native::web_content_pid(&w))
        };
        let pid_before = web_content();
        // 用户式的收尾：Rust 只发 hide_request（Esc），由前端走退出流程（卸载视图、`data-hidden`、停掉 rAF / 计时器）后调用
        // panel_hide——实验台跑完之后的「会话后空闲」测量以这个状态为准
        wm.panel_request_hide(HideReason::Escape);
        let t0 = Instant::now();
        while wm.panel_state().is_some() && t0.elapsed() < Duration::from_secs(3) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        step(
            "frontend_hide",
            wm.panel_state().is_none(),
            format!("hidden by the frontend exit flow in {} ms", t0.elapsed().as_millis()),
        );
        let t0 = Instant::now();
        while *loads.borrow() == loads_before && t0.elapsed() < RECYCLE_WAIT {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let pid_after = web_content();
        step(
            "recycle_after_hide",
            *loads.borrow() != loads_before && app.get_webview_window(PANEL_LABEL).is_some(),
            format!(
                "page reloaded {} ms after the hide, web content {pid_before:?} -> {pid_after:?}",
                t0.elapsed().as_millis()
            ),
        );

        if mem_trace {
            sample_phase(&app, "lab_after_recycle", 1).await;
        }
        // 9c. 回收后的首次唤出：回收出来的是新的 WebContent 进程，JIT 是冷的。每轮在回收出来的
        //     页面加载并静置之后计一次 prepare→shown（真实场景里回收发生在隐藏 recycle_after 之后，下一次唤出至少再晚
        //     几秒），再隐藏让它被回收，进入下一轮；与第 8 步（用过的热页面）对照。流式输出首帧的逐帧统计见 frame lab 的
        //     `WISP_FRAME_LAB_RECYCLE=1`（需要亮屏，displayLink 才出帧）
        let mut cold = Vec::new();
        let mut recycled = 0;
        for _ in 0..RECYCLED_ROUNDS {
            tokio::time::sleep(RECYCLED_SETTLE).await;
            let loads_before = *loads.borrow();
            let s = prepare(
                &wm,
                request(PanelView::Translate, AnchorSource::Cursor, translate("after recycle")),
            )
            .await;
            match wait_shown(&wm, s, Duration::from_secs(2)).await {
                Some(d) => cold.push(d),
                None => step("recycled_first_show", false, format!("session={s} not shown")),
            }
            wm.panel_request_hide(HideReason::Escape);
            let t0 = Instant::now();
            while *loads.borrow() == loads_before && t0.elapsed() < RECYCLE_WAIT {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            // 没有回收（例如被推迟）时下一轮测的就不是回收出来的页面：记下来，判为失败
            if *loads.borrow() != loads_before {
                recycled += 1;
            }
        }
        let cold = Percentiles::of(cold);
        step(
            "recycled_first_show",
            cold.n == RECYCLED_ROUNDS as usize
                && recycled == RECYCLED_ROUNDS
                && cold.p95.is_some_and(|d| d <= Duration::from_millis(100)),
            format!(
                "recycled {cold} (page recycled after {recycled}/{RECYCLED_ROUNDS} rounds); warm p50={:?} p95={:?}",
                warm.p50, warm.p95
            ),
        );

        if mem_trace {
            sample_phase(&app, "lab_after_recycled_rounds", 1).await;
        }
        // 9c'. 非翻译视图回收后的首次唤出：输入视图与剪贴板视图各测几轮（只有翻译视图做了 JIT 预热渲染，它们在
        //      冷的页面上更慢），报告 prepare→shown 的分位数；全部显示即通过，耗时用于对照第 9c 步
        for (view, anchor, payload) in [
            (
                PanelView::Input,
                AnchorSource::Center,
                PanelPayload::Input(wisp_types::InputPayload {
                    initial_text: String::new(),
                    target_lang: None,
                    hint: None,
                }),
            ),
            (
                PanelView::Clipboard,
                AnchorSource::Center,
                PanelPayload::Clipboard(wisp_types::ClipboardPayload {
                    page: wisp_types::ClipPage::empty(wisp_types::ClipFilter::All),
                    paused: false,
                    hint_bar: true,
                    paste_on_double_click: false,
                    unavailable: None,
                    recording: wisp_types::ClipboardRecording::Recording,
                    favorites_collapsed: false,
                    queue_unavailable: None,
                }),
            ),
        ] {
            let mut samples = Vec::new();
            for _ in 0..VIEW_ROUNDS {
                tokio::time::sleep(RECYCLED_SETTLE).await;
                let loads_before = *loads.borrow();
                let s = prepare(&wm, request(view, anchor.clone(), payload.clone())).await;
                if let Some(d) = wait_shown(&wm, s, Duration::from_secs(2)).await {
                    samples.push(d);
                }
                wm.panel_request_hide(HideReason::Escape);
                let t0 = Instant::now();
                while *loads.borrow() == loads_before && t0.elapsed() < RECYCLE_WAIT {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
            let p = Percentiles::of(samples);
            step(
                &format!("recycled_first_show_{}", format!("{view:?}").to_lowercase()),
                p.n == VIEW_ROUNDS as usize,
                format!("recycled {p}"),
            );
        }

        if mem_trace {
            sample_phase(&app, "lab_after_view_rounds", 1).await;
        }
        // 9d. 可选的回收压力（`WISP_WINDOW_LAB_RECYCLE_STRESS=<次数>`）：连续回收 N 次，记下主进程内存与存活窗口数的变化
        //     （每次回收新建一个窗口：tao / wry / WebKit 上游每个窗口还有约 2.4 KB 的小泄漏）。实验台跑完不退出时
        //     （不设 WISP_WINDOW_LAB_EXIT）可以再用 `heap <pid>` 数残留的 TaoView
        let stress = std::env::var("WISP_WINDOW_LAB_RECYCLE_STRESS")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);
        if stress > 0 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let census_before = census(&app).await;
            let mut done = 0;
            // 每 10 次采样一次（比 phys_footprint 稳定得多的主进程 malloc 在用量、跨回收常驻的 GPU / Networking
            // 进程、当次的 WebContent，与存活窗口数）：(次数, 采样)
            let mut curve: Vec<(u32, MemSample)> = vec![(0, MemSample::take(&app).await)];
            let log_sample = |n: u32, s: &MemSample, windows: Option<usize>| {
                log::info!(
                    "window_lab recycle_stress sample n={n} windows={windows:?} {}",
                    s.line()
                );
            };
            log_sample(0, &curve[0].1, census_before.map(|c| c.wisp));
            // 可选（`WISP_WINDOW_LAB_STRESS_HOLD_SECS=<秒>`）：压力开始前与结束后各停这么久，留给外部的 `heap <pid>` 取快照
            // （按类型数存活对象，两次相减看每次回收留下了什么）；停顿以 `recycle_stress hold=start|end` 行开始
            let hold = std::env::var("WISP_WINDOW_LAB_STRESS_HOLD_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs);
            if let Some(hold) = hold {
                log::info!("window_lab recycle_stress hold=start secs={}", hold.as_secs());
                tokio::time::sleep(hold).await;
            }
            for i in 0..stress {
                if i > 0 && i % 10 == 0 {
                    let sample = MemSample::take(&app).await;
                    let windows = census(&app).await.map(|c| c.wisp);
                    log_sample(i, &sample, windows);
                    curve.push((i, sample));
                }
                let loads_before = *loads.borrow();
                let s = prepare(
                    &wm,
                    request(PanelView::Translate, AnchorSource::Cursor, translate("stress")),
                )
                .await;
                if wait_shown(&wm, s, Duration::from_secs(2)).await.is_none() {
                    continue;
                }
                wm.panel_hide_now(s, HideReason::Paste);
                let t0 = Instant::now();
                while *loads.borrow() == loads_before && t0.elapsed() < RECYCLE_WAIT {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                if *loads.borrow() != loads_before {
                    done += 1;
                }
                // 新页面加载之后再静置一下（与第 9c 步相同），免得下一次 prepare 落在页面初始化中
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
            let census_after = census(&app).await;
            let last = MemSample::take(&app).await;
            log_sample(stress, &last, census_after.map(|c| c.wisp));
            curve.push((stress, last));
            if let Some(hold) = hold {
                log::info!("window_lab recycle_stress hold=end secs={}", hold.as_secs());
                tokio::time::sleep(hold).await;
            }
            // 最小二乘斜率（KiB / 次）：全部样本与后一半样本（前几十次里有缓存 / 分配器的一次性增长）
            let slope = |points: &[(f64, f64)]| -> Option<f64> {
                (points.len() >= 2).then(|| {
                    let k = points.len() as f64;
                    let (sx, sy) = points.iter().fold((0.0, 0.0), |(a, b), &(x, y)| (a + x, b + y));
                    let (mx, my) = (sx / k, sy / k);
                    let num: f64 = points.iter().map(|&(x, y)| (x - mx) * (y - my)).sum();
                    let den: f64 = points.iter().map(|&(x, _)| (x - mx) * (x - mx)).sum();
                    if den > 0.0 { num / den } else { 0.0 }
                })
            };
            let series = |f: &dyn Fn(&MemSample) -> Option<u64>, tail: bool| -> String {
                let points: Vec<(f64, f64)> = curve
                    .iter()
                    .skip(if tail { curve.len() / 2 } else { 0 })
                    .filter_map(|(n, s)| f(s).map(|v| (f64::from(*n), v as f64)))
                    .collect();
                slope(&points).map_or_else(|| "-".to_owned(), |v| format!("{v:.2}"))
            };
            let first = &curve[0].1;
            let end = &curve[curve.len() - 1].1;
            step(
                "recycle_stress",
                done == stress && census_after.zip(census_before).is_none_or(|(a, b)| a.wisp <= b.wisp),
                format!(
                    "recycled {done}/{stress}; samples={}; malloc {:?} -> {:?} KiB, slope all/tail {}/{} KiB/recycle; \
                     main {} {:?} -> {:?} KiB, tail slope {} KiB/recycle; GPU {} -> {} KiB, slope {}; \
                     Networking {} -> {} KiB, slope {}; windows {census_before:?} -> {census_after:?}",
                    curve.len(),
                    first.malloc_kib,
                    end.malloc_kib,
                    series(&|s| s.malloc_kib, false),
                    series(&|s| s.malloc_kib, true),
                    wisp_platform::process::MEMORY_METRIC,
                    first.main_kib,
                    end.main_kib,
                    series(&|s| s.main_kib, true),
                    first.kind_kib("GPU"),
                    end.kind_kib("GPU"),
                    series(&|s| s.has("GPU").then(|| s.kind_kib("GPU")), false),
                    first.kind_kib("Networking"),
                    end.kind_kib("Networking"),
                    series(&|s| s.has("Networking").then(|| s.kind_kib("Networking")), false),
                ),
            );
        }
        controller.set_options(original);

        // 9e. 按产品选项（回收默认关）最后用一次面板、由前端收尾：实验台之后的「会话后空闲」测量测的是
        //     用过的页面在默认选项下的状态（开了 WISP_PANEL_RECYCLE_SECS 时，它会在那之后被回收）
        tokio::time::sleep(Duration::from_millis(600)).await;
        let s = prepare(
            &wm,
            request(PanelView::Translate, AnchorSource::Cursor, translate("final use")),
        )
        .await;
        let shown = wait_shown(&wm, s, Duration::from_secs(2)).await;
        wm.panel_request_hide(HideReason::Escape);
        let t0 = Instant::now();
        while wm.panel_state().is_some() && t0.elapsed() < Duration::from_secs(3) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        step(
            "final_use",
            shown.is_some() && wm.panel_state().is_none(),
            format!(
                "session={s} shown after {shown:?}, hidden by the frontend; recycle_after={:?}",
                original.recycle_after
            ),
        );

        // 10. 窗口泄漏（销毁的窗口曾经仍留在 AppKit / 窗口服务器里，根因见 native::adopt_window）：存活的 Wisp
        //     窗口对象与它们在窗口服务器里的窗口都回到开始时的个数（只剩重建后的面板）。窗口服务器里其余的窗口是 AppKit
        //     自己的（切到 Regular 时建的菜单栏副本等，第 7 步已确认不随次数增长），只记录。macOS 才有这些计数，其他平台跳过
        tokio::time::sleep(Duration::from_millis(1500)).await;
        match (census_at_start, census(&app).await) {
            (Some(start), Some(end)) => step(
                "no_window_leak",
                end.wisp <= start.wisp && end.server_wisp <= start.server_wisp,
                format!("start={start:?} end={end:?}"),
            ),
            // macOS 必须取得到计数：取不到时记为失败，不让泄漏检查静默通过
            (start, end) if super::native::HAS_WINDOW_CENSUS => step(
                "no_window_leak",
                false,
                format!("census unavailable start={start:?} end={end:?}"),
            ),
            _ => {}
        }

        let panel_alive = app.get_webview_window(PANEL_LABEL).is_some();
        step("done", panel_alive, "");
        if finish(&app) {
            return;
        }
        if let Some(secs) = std::env::var("WISP_WINDOW_LAB_LATE_SHOW_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            late_show(&app, &wm, secs).await;
        }
    });
}
