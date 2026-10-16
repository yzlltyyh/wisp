//! OCR / 直接翻译流程：截图前隐藏窗口 → capture_all → 创建 Overlay → commit → 裁剪和编码 → 请求 → 飞入；
//! 运行时视觉能力回写（wisp-llm 按请求结果经 CapsStore 写回）；OCR 自动复制；
//! 直接翻译的「查看原文」与两次调用模式；前端发起的重新识别（`ocr_start`）。
//!
//! **截图段与屏幕栅栏**：「隐藏自己的窗口 → 截图 → 登记会话 → 建 Overlay → 原生冻结帧的首次显示」在屏幕栅栏的独占段里
//! 执行（`engine::ScreenGate`）。截图结束后**先**确认 run 仍有效，再登记冻结帧与 Overlay 会话：截图期间被取代的旧截图
//! 只恢复它自己隐藏的窗口、丢掉自己的帧，不碰新会话。被取代 / 取消的 Overlay 的拆除排在独占段之后，新流程在取词之前
//! 等拆除做完（焦点已还给原应用）。
//!
//! **Overlay 显示失败**：只有 `overlay_show` 返回 Ok 的显示器才记为已显示。带原生冻结帧的显示失败时改按
//! Windows / X11 的路径（`frozen_native = false`，页面画冻结帧后 `overlay_ready` 再试一次）；再失败记为失败。全部显示器
//! 都失败、或 4 s 内没有任何显示器真正显示出来时：拆掉（交还焦点 → 销毁 → 恢复 pinned），显示
//! `CaptureFailed{errors.capture.backend}` 卡片——不会留下一个隐藏了一切却看不见、按 ⌥S 也进不去的会话。
//!
//! **焦点**：取消 / 出错时先交还焦点再销毁 Overlay；commit 之后在销毁 Overlay 之后交还（OCR 面板
//! 以非激活方式显示，不受影响；Windows 上 Overlay 调用过 SetForegroundWindow，不交还的话原应用失去键盘焦点）。那一刻
//! 面板若已是 key 窗口（用户点进了面板），不抢它的焦点。
//!
//! **commit 之后**：Overlay 从 commit 起就登记在屏幕栅栏上（直到真正拆完），飞入 / 淡出期间来了新的触发时由
//! `start_run` 立即拆掉，新流程等拆完再读前台应用 / 取词 / 显示面板。
//!
//! **Key 与 Overlay**：读 Key 不会弹系统对话框时，触发时就预读（用户框选的几秒里早已读好）；可能弹框时推迟到 commit
//! 再读（而不是等 Overlay 拆完），框选期间对话框不会被层级 101 的 Overlay 盖住或抢走它的
//! 键盘焦点；commit 之后 Overlay 只剩约 360 ms 的飞入与淡出。
//!
//! **请求先于面板的口径**：commit 时面板在 Overlay **下面**准备并显示，飞入（220 ms）与淡出（140 ms）之后
//! Overlay 拆掉，用户此时才看得见面板。编码与 `panel_prepare` 并行（commit 不等编码就返回飞入目标，飞入不被推迟），
//! 请求在编码完成后立即发出；日志 `request … dispatched trace=T at +X ms` 与 `overlay session … destroyed trace=T at
//! +Y ms` 同一起点，X < Y 即「请求早于面板可见」。
//!
//! **Wayland**（接入系统截图门户之前）：`capture_all` 必然返回 Unsupported，触发时直接显示
//! `errors.platform.wayland_capture` 卡片，不隐藏任何窗口、不结束正在显示的面板会话。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use wisp_capture::{CaptureError, Frame, RgbaImage, WindowRect};
use wisp_llm::{CancellationToken, Purpose, Secret, UserContent};
use wisp_prompts::PostKind;
use wisp_types::{
    Anchor, AppRef, DictMode, ErrorCode, LlmError, MonitorId, NotReady, OcrMode, OcrPayload, OcrReq, OverlayCommit,
    OverlayCommitAck, OverlayEndReason, OverlayInit, OverlayMode, PanelPayload, PanelTransition, PanelView,
    PermissionKind, PermissionState, PhysSize, RequestId, StartAck, StreamEvent, TaskKind,
};

use super::RequestHost;
use super::captures::{Crop, CropImage, ENCODE_WAIT, Encoded, ModelKey, thumb_path};
use super::engine::{
    Core, KeyFetch, KeySource, ScreenHold, Trigger, blocking, log_dispatched, not_ready_error, owner_of,
};
use super::env::Env;
use super::fsm::{Event, Flow, Stage};
use super::not_ready;
use super::plan::{Resolved, TextInput, build_ocr_prompt, default_ocr_mode, plan_text};
use super::registry::{Lane, OpenSpec, Owner, PumpEnd, Sink, pump};
use super::translate::TextJob;
use crate::app::perf::PerfStage;
use crate::assets::{CaptureVariant, capture_asset_path};
use crate::bus::TriggerOrigin;
use crate::clipboard_svc::ocr_history::OcrRecord;
use crate::error::IpcError;
use crate::windows::{AnchorSource, HiddenSet, OverlaySession, PanelShowRequest};

/// commit 之后等 `overlay_done` 的上限（页面异常时也要拆掉 Overlay、恢复 pinned）
const OVERLAY_DONE_WAIT: Duration = Duration::from_millis(2500);
/// 建好 Overlay 之后等它显示的上限（Windows / X11 等页面 `overlay_ready`）
const OVERLAY_SHOW_WAIT: Duration = Duration::from_secs(4);
/// 原生冻结帧已经显示、页面却还没 `overlay_init`：等这么久仍没有就拆掉并提示截图失败（WebContent 卡住或崩溃时，
/// 层级 101 的全屏 Overlay 不能一直留着、没有任何输入处理）。取 1.5 s：正常情况下
/// 页面在冻结帧上屏之后收到 `overlay:session` 立即重新 init，只要几十毫秒；1.5 s 足够盖过一次 WebContent 冷启动，又让
/// 卡死的整屏冻结帧尽早让位
const OVERLAY_INIT_WAIT: Duration = Duration::from_millis(1500);
/// 多屏截图时查看光标在哪块屏的间隔（只在未 commit 的多屏会话里、且窗口层不自己跟随指针时存在）
const OVERLAY_FOLLOW_EVERY: Duration = Duration::from_millis(50);
/// 下一帧
const FRAME: Duration = Duration::from_millis(16);

/// window lab（只在 debug / fake-llm 构建）：Overlay 页面调用 `overlay_init` 的时刻（label, 时刻），最多留最近 32 条。
/// 实验台据此量「Overlay 上屏 → 页面可交互」在 WebKit 各进程都冷着时要多久（核对 `OVERLAY_INIT_WAIT` 的余量）
#[cfg(any(debug_assertions, feature = "fake-llm"))]
static LAB_OVERLAY_INITS: parking_lot::Mutex<Vec<(String, Instant)>> = parking_lot::Mutex::new(Vec::new());

/// window lab：`since` 之后到达的 `overlay_init`（见 [`LAB_OVERLAY_INITS`]）
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub(crate) fn lab_overlay_inits_since(since: Instant) -> Vec<(String, Instant)> {
    LAB_OVERLAY_INITS
        .lock()
        .iter()
        .filter(|(_, at)| *at >= since)
        .cloned()
        .collect()
}

/// 冻结帧在某块显示器上的状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Frozen {
    /// macOS：带原生冻结帧的 `overlay_show` 还没返回，`overlay_init` 先回 None
    Pending,
    /// 已确定：原生冻结帧是否装上
    Known(bool),
}

/// 拆 Overlay 时什么时候交还焦点
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FocusReturn {
    /// 取消 / 出错 / 被取代：先交还焦点，再销毁 Overlay、恢复 pinned
    BeforeDestroy,
    /// commit 之后被提前拆掉（飞入期间按 Esc、来了新的触发）：立即销毁 Overlay 之后交还，再恢复 pinned
    AfterDestroy,
    /// commit 之后的正常收尾（`overlay_done` / 兜底）：同 AfterDestroy，但 macOS 先淡出 Overlay 窗口再销毁
    /// （原生冻结帧不硬切）
    AfterFadeOut,
}

/// 一次截图会话（同一时刻至多一个）
pub(super) struct OverlayState {
    pub(super) session_id: u64,
    pub(super) run: u64,
    pub(super) trace: u64,
    /// 触发时刻（「已发出」日志的 +ms 起点，与埋点同一起点）
    pub(super) started: Instant,
    pub(super) mode: OverlayMode,
    pub(super) prev_app: Option<AppRef>,
    pub(super) hidden: HiddenSet,
    pub(super) monitors: Vec<MonitorId>,
    pub(super) windows: Vec<WindowRect>,
    pub(super) frozen: HashMap<MonitorId, Frozen>,
    /// `overlay_show` 返回 Ok、真正显示出来的显示器
    pub(super) shown: HashSet<MonitorId>,
    /// `overlay_ready` 触发、还在进行的显示
    pub(super) showing: HashSet<MonitorId>,
    /// 显示失败（已无重试路径）的显示器
    pub(super) failed: HashSet<MonitorId>,
    /// 页面已经拿到初始化数据（`overlay_init` 返回了会话）的显示器（原生冻结帧已显示、页面却迟迟不 init 时兜底）
    pub(super) inited: HashSet<MonitorId>,
    pub(super) reduced_motion: bool,
    pub(super) window_detect: bool,
    pub(super) fly_animation: bool,
    pub(super) committed: bool,
    /// commit 时打开的面板会话（`overlay_done` 确认它已显示）
    pub(super) panel_session: Option<u64>,
    /// 截图触发时预读的 Key（commit 时取走；用户框选的几秒里早已读好）。读取可能弹框时不预读，为 None
    pub(super) key: Option<KeyFetch>,
    /// commit 之后在屏幕栅栏上的登记：拆完才撤销，期间新流程等它（`engine::ScreenGate::hold`）
    pub(super) hold: Option<ScreenHold>,
}

/// OCR 请求的参数
pub(super) struct OcrJob {
    pub(super) id: RequestId,
    pub(super) token: CancellationToken,
    pub(super) resolved: Resolved,
    pub(super) mode: OcrMode,
    pub(super) target: wisp_types::Lang,
    pub(super) crop: Arc<Crop>,
    /// 直接翻译且开启了「同时转录原文」：先 paragraphs 转录（缓存在截图上），再走翻译管线
    pub(super) two_step: bool,
    pub(super) run: Option<u64>,
    pub(super) trace: Option<u64>,
    pub(super) started: Option<Instant>,
    pub(super) key: KeySource,
}

/// 识别一张图片的启动参数（截图 commit 与剪贴板图片共用 [`Core::launch_ocr`]）
pub(super) struct OcrLaunch {
    pub(super) run: u64,
    pub(super) trace: u64,
    /// 触发时刻（「已发出」日志与埋点的起点）
    pub(super) started: Instant,
    /// 要识别的像素（截图的裁剪结果，或解码后的剪贴板原图）
    pub(super) img: RgbaImage,
    /// 截图直接翻译（剪贴板图片总是 false）
    pub(super) direct: bool,
    pub(super) resolved: Resolved,
    pub(super) anchor: AnchorSource,
    pub(super) prev: Option<AppRef>,
    pub(super) transition: PanelTransition,
    pub(super) key: KeySource,
}

/// 拆 Overlay 的窗口操作（阻塞线程上执行）。commit 之后的登记（`hold`）随之移进来，窗口操作做完才撤销。
/// 第一件事是 `overlay:end`：页面据此立即收尾、释放冻结帧，必须赶在隐藏 / 销毁、解除窗口与会话的
/// 绑定之前（此时窗口层的登记还在，`AppEnv` 才找得到这个会话的 Overlay）
fn teardown_work(env: Arc<dyn Env>, st: OverlayState, focus: FocusReturn, reason: OverlayEndReason) -> impl FnOnce() {
    let OverlayState {
        session_id,
        trace,
        started,
        prev_app,
        hidden,
        hold,
        ..
    } = st;
    move || {
        env.overlay_session_ended(session_id, reason);
        if focus == FocusReturn::BeforeDestroy
            && let Some(prev) = &prev_app
        {
            env.activate(prev);
        }
        env.destroy_overlays(focus == FocusReturn::AfterFadeOut);
        log::info!(
            "overlay session {session_id} destroyed ({reason:?}) trace={trace} at +{} ms",
            started.elapsed().as_millis()
        );
        if matches!(focus, FocusReturn::AfterDestroy | FocusReturn::AfterFadeOut)
            && let Some(prev) = &prev_app
        {
            // 面板此刻已是 key 窗口（用户点进了面板）：不抢它的焦点
            if env.panel_state().is_some_and(|p| p.key) {
                log::debug!("overlay session {session_id}: the panel holds the focus; not returning it");
            } else {
                env.activate(prev);
            }
        }
        env.restore_after_capture(hidden);
        drop(hold);
    }
}

fn model_key(r: &Resolved) -> ModelKey {
    (r.provider.id.clone(), r.model.id.clone())
}

impl Core {
    // ------------------------------------------------------------------------------------------------
    // 截图 OCR / 直接翻译

    pub(super) fn start_capture(self: &Arc<Self>, mode: OverlayMode, t: Trigger) {
        // Overlay 已经显示、还没 commit 时再按截图快捷键：取消这次截图（否则页面无响应时用户没有出路）
        let shown_uncommitted = self
            .overlay
            .lock()
            .as_ref()
            .filter(|o| !o.committed && !o.shown.is_empty())
            .map(|o| o.session_id);
        if let Some(session_id) = shown_uncommitted {
            log::info!("overlay session {session_id}: capture hotkey pressed again; cancelling");
            self.end_overlay(session_id, OverlayEndReason::Retrigger);
            return;
        }
        // 截图会话进行中（截图中 / Overlay 正在出现）再按截图快捷键：忽略，不重复截图
        // 两把锁分开取（不嵌套）
        let overlay_up = self.overlay.lock().is_some();
        let capturing = self
            .runs
            .lock()
            .map
            .values()
            .any(|r| matches!(r.flow, Flow::Ocr | Flow::DirectTranslate) && r.stage == Stage::Capturing);
        if overlay_up || capturing {
            log::debug!("capture already in progress; trigger ignored");
            return;
        }
        let flow = match mode {
            OverlayMode::Ocr => Flow::Ocr,
            OverlayMode::OcrTranslate => Flow::DirectTranslate,
        };
        let run = self.start_run(flow, t.trace);
        let core = Arc::clone(self);
        tauri::async_runtime::spawn(async move { core.capture_flow(run, t.trace, t.started, mode).await });
    }

    pub(super) async fn capture_flow(self: Arc<Self>, run: u64, trace: u64, started: Instant, mode: OverlayMode) {
        let settings = self.env.settings();
        let direct = mode == OverlayMode::OcrTranslate;
        // 0. Wayland 会话（接入系统截图门户之前）：`capture_all` 必然返回 Unsupported。不进屏幕栅栏、不隐藏任何窗口
        //    （不让 pinned 闪一下、不结束正在显示的面板会话），直接显示「当前会话不支持截图」卡片
        if self.env.wayland() {
            if self.advance(run, Event::NotReady).is_some() {
                let err =
                    CaptureError::Unsupported("wayland session: the screenshot portal is not wired yet".to_owned());
                let nr = not_ready::capture_not_ready(&err, true).unwrap_or(NotReady::CaptureFailed {
                    message: not_ready::capture_reason::UNSUPPORTED.to_owned(),
                });
                self.screen.settled().await;
                let prev = self.prev_app_for(TriggerOrigin::Hotkey).await;
                self.show_ocr_not_ready(
                    run,
                    trace,
                    nr,
                    direct,
                    prev,
                    AnchorSource::Cursor,
                    PanelTransition::Enter,
                )
                .await;
            }
            return;
        }
        // 1. 未就绪：没配置模型时不截图，直接显示卡片
        let resolved = match not_ready::check_ocr(&settings, None, &self.keys()) {
            Ok(r) => r,
            Err(nr) => {
                if self.advance(run, Event::NotReady).is_some() {
                    self.screen.settled().await;
                    let prev = self.prev_app_for(TriggerOrigin::Hotkey).await;
                    self.show_ocr_not_ready(
                        run,
                        trace,
                        nr,
                        direct,
                        prev,
                        AnchorSource::Cursor,
                        PanelTransition::Enter,
                    )
                    .await;
                }
                return;
            }
        };
        // 2. 屏幕录制权限（macOS 只能区分可用 / 不可用：Denied 与 NotDetermined 同样处理）
        if matches!(
            self.env.screen_permission(),
            PermissionState::Denied | PermissionState::NotDetermined
        ) {
            if self.advance(run, Event::NotReady).is_some() {
                let env = Arc::clone(&self.env);
                tauri::async_runtime::spawn_blocking(move || env.request_screen_permission());
                self.screen.settled().await;
                let prev = self.prev_app_for(TriggerOrigin::Hotkey).await;
                let nr = NotReady::PermissionMissing {
                    permission: PermissionKind::ScreenRecording,
                };
                self.show_ocr_not_ready(
                    run,
                    trace,
                    nr,
                    direct,
                    prev,
                    AnchorSource::Cursor,
                    PanelTransition::Enter,
                )
                .await;
            }
            return;
        }
        // 3. 预读 Key（用户框选要几秒，commit 时早已读好）——只在读取不会弹系统对话框时；可能弹框时等 Overlay 拆完再读
        //    （commit 时再读）。「减少动效」（macOS 读系统偏好）与截图并行读取，不排在截图与建 Overlay 之间
        let key = self.prefetch_key_if_quiet(&resolved.provider);
        let reduce = {
            let env = Arc::clone(&self.env);
            tauri::async_runtime::spawn_blocking(move || env.reduce_motion())
        };
        // 4. 截图段：等被取代的 Overlay 拆完、上一次截图段结束，独占屏幕栅栏
        let guard = self.screen.exclusive().await;
        if self.advance(run, Event::Begin).is_none() {
            return;
        }
        // 记下前台应用 → 隐藏自己的窗口→ 截图（阻塞线程）
        let env = Arc::clone(&self.env);
        let Some((prev, hidden, result)) = blocking(move || {
            let prev = env.frontmost();
            let hidden = env.hide_for_capture();
            let monitors = env.monitors();
            let result = if monitors.is_empty() {
                Err(CaptureError::NoDisplays)
            } else {
                env.capture_all(&monitors)
            };
            (prev, hidden, result)
        })
        .await
        else {
            self.advance(run, Event::Failed);
            self.drop_run(run);
            return;
        };
        let set = match result {
            Ok(set) if !set.frames.is_empty() => set,
            other => {
                self.restore(hidden).await;
                drop(guard);
                let err = other.err().unwrap_or(CaptureError::NoDisplays);
                match not_ready::capture_not_ready(&err, self.env.wayland()) {
                    None => {
                        self.advance(run, Event::CaptureCancelled);
                    }
                    Some(nr) => {
                        if self.advance(run, Event::CaptureFailed).is_some() {
                            self.show_ocr_not_ready(
                                run,
                                trace,
                                nr,
                                direct,
                                prev,
                                AnchorSource::Cursor,
                                PanelTransition::Enter,
                            )
                            .await;
                        }
                    }
                }
                return;
            }
        };
        // 5. 先确认 run 仍有效，再登记冻结帧与 Overlay 会话：截图期间被取代的，只恢复自己隐藏的窗口、丢掉自己的帧
        if self.advance(run, Event::Captured).is_none() {
            log::debug!("run {run}: capture finished after a newer trigger; discarded");
            self.restore(hidden).await;
            return;
        }
        let reduced_motion = reduce.await.unwrap_or(false);
        let session_id = self.next_overlay.fetch_add(1, Ordering::Relaxed) + 1;
        let frames: Vec<(Arc<Frame>, bool)> = set
            .frames
            .into_iter()
            .map(|f| {
                let native = self.env.frame_is_native(&f);
                (Arc::new(f), native)
            })
            .collect();
        let monitors: Vec<MonitorId> = frames.iter().map(|(f, _)| f.monitor.id.clone()).collect();
        let infos: Vec<wisp_types::MonitorInfo> = frames.iter().map(|(f, _)| f.monitor.clone()).collect();
        let frozen: HashMap<MonitorId, Frozen> = frames
            .iter()
            .map(|(f, native)| {
                let state = if *native { Frozen::Pending } else { Frozen::Known(false) };
                (f.monitor.id.clone(), state)
            })
            .collect();
        self.captures
            .set_frames(session_id, frames.iter().map(|(f, _)| Arc::clone(f)).collect());
        let mut state = OverlayState {
            session_id,
            run,
            trace,
            started,
            mode,
            prev_app: prev,
            hidden,
            monitors,
            windows: set.windows,
            frozen,
            shown: HashSet::new(),
            showing: HashSet::new(),
            failed: HashSet::new(),
            inited: HashSet::new(),
            reduced_motion,
            window_detect: settings.ocr.window_detect,
            fly_animation: settings.ocr.fly_animation && !reduced_motion,
            committed: false,
            panel_session: None,
            key,
            hold: None,
        };
        {
            let mut g = self.overlay.lock();
            if let Some(stale) = g.take() {
                // 同一时刻至多一个截图会话，不应出现；万一出现，它隐藏的窗口并入这次会话，结束时一起恢复
                log::warn!(
                    "overlay session {} was still registered when {session_id} started",
                    stale.session_id
                );
                state.hidden.panel |= stale.hidden.panel;
                for id in stale.hidden.pinned {
                    if !state.hidden.pinned.contains(&id) {
                        state.hidden.pinned.push(id);
                    }
                }
            }
            *g = Some(state);
        }
        // 登记与「确认 run 仍有效」之间可能来了新的触发（那时 settle 还找不到这个会话）：自己收尾
        if self.run_stage(run) != Some(Stage::Framing) {
            if let Some(st) = self.take_overlay(session_id) {
                self.teardown_now(st, FocusReturn::BeforeDestroy, OverlayEndReason::Cancelled)
                    .await;
            }
            return;
        }
        let env = Arc::clone(&self.env);
        let session = OverlaySession {
            session_id,
            monitors: infos,
            mode,
        };
        let created = blocking(move || env.create_overlays(&session)).await;
        if !matches!(created, Some(Ok(()))) {
            if let Some(Err(e)) = created {
                log::error!("overlay session {session_id}: creating overlays failed: {}", e.message);
            }
            if let Some(st) = self.take_overlay(session_id) {
                let prev = st.prev_app.clone();
                self.teardown_now(st, FocusReturn::BeforeDestroy, OverlayEndReason::Failed)
                    .await;
                drop(guard);
                if self.advance(run, Event::NotReady).is_some() {
                    let nr = NotReady::CaptureFailed {
                        message: not_ready::capture_reason::BACKEND.to_owned(),
                    };
                    self.show_ocr_not_ready(
                        run,
                        trace,
                        nr,
                        direct,
                        prev,
                        AnchorSource::Cursor,
                        PanelTransition::Enter,
                    )
                    .await;
                }
            }
            return;
        }
        // 6. 带原生冻结帧的显示器（macOS）：各块屏并行显示（首帧即冻结帧），每块屏返回即通知它的页面重新 init
        let natives: Vec<_> = frames
            .iter()
            .filter(|(_, native)| *native)
            .map(|(f, _)| {
                let core = Arc::clone(&self);
                let frame = Arc::clone(f);
                tauri::async_runtime::spawn(async move { core.show_native(session_id, frame).await })
            })
            .collect();
        for task in natives {
            let _ = task.await;
        }
        drop(guard);
        let overlay_ms = self.env.perf().snapshot(trace).and_then(|t| t.overlay_ms);
        log::info!(
            "overlay session {session_id} ({mode:?}) up trace={trace} overlay_ms={}",
            overlay_ms.map_or_else(|| "-".to_owned(), |v| v.to_string())
        );
        // 会话级的出路：全局 Esc、多屏时按键焦点跟随光标、页面迟迟不 init 的兜底
        self.overlay_guards_changed();
        self.spawn_overlay_follow(session_id);
        self.spawn_overlay_init_watchdog(session_id);
        // 7. 兜底：没有任何显示器真正显示出来（页面迟迟不 overlay_ready、显示全部失败）时收尾并提示
        let core = Arc::clone(&self);
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(OVERLAY_SHOW_WAIT).await;
            let stuck = core
                .overlay
                .lock()
                .as_ref()
                .is_some_and(|o| o.session_id == session_id && o.shown.is_empty() && !o.committed);
            if stuck {
                log::warn!("overlay session {session_id}: no overlay became visible; giving up");
                core.abort_overlay(session_id).await;
            }
        });
    }

    /// 带原生冻结帧显示一块屏；成功才记为已显示。失败时改按页面自己画冻结帧的路径（`frozen_native = false`，
    /// 页面 `overlay_ready` 后再试一次 `overlay_show(None)`）
    async fn show_native(self: Arc<Self>, session_id: u64, frame: Arc<Frame>) {
        let env = Arc::clone(&self.env);
        let monitor = frame.monitor.id.clone();
        let shown = {
            let frame = Arc::clone(&frame);
            blocking(move || env.overlay_show(session_id, &frame.monitor.id, Some(&*frame))).await
        };
        let still = {
            let mut g = self.overlay.lock();
            match g.as_mut().filter(|o| o.session_id == session_id) {
                Some(o) => {
                    match shown {
                        Some(Ok(native)) => {
                            o.frozen.insert(monitor.clone(), Frozen::Known(native));
                            if o.shown.is_empty() {
                                // 第一块屏的冻结帧上屏：Overlay 出现的延迟
                                self.env.perf().mark(o.trace, PerfStage::OverlayShown);
                            }
                            o.shown.insert(monitor.clone());
                        }
                        Some(Err(e)) => {
                            log::warn!("overlay {monitor}: native show failed ({}); page path next", e.message);
                            o.frozen.insert(monitor.clone(), Frozen::Known(false));
                        }
                        None => {
                            o.frozen.insert(monitor.clone(), Frozen::Known(false));
                        }
                    }
                    true
                }
                None => false,
            }
        };
        if still {
            self.env.overlay_session_started(session_id, &monitor);
        }
    }

    /// 取走某个 Overlay 会话的状态
    pub(super) fn take_overlay(&self, session_id: u64) -> Option<OverlayState> {
        let taken = {
            let mut g = self.overlay.lock();
            if g.as_ref().is_some_and(|o| o.session_id == session_id) {
                g.take()
            } else {
                None
            }
        };
        if taken.is_some() {
            self.overlay_guards_changed();
        }
        taken
    }

    /// 恢复截图前隐藏的窗口（阻塞线程）
    pub(super) async fn restore(&self, hidden: HiddenSet) {
        if hidden == HiddenSet::default() {
            return;
        }
        let env = Arc::clone(&self.env);
        blocking(move || env.restore_after_capture(hidden)).await;
    }

    /// 拆掉 Overlay：冻结帧立即释放；窗口操作在屏幕栅栏上排队（进行中的截图段之后），返回可等待的句柄。
    /// `reason` 随 `overlay:end` 告诉页面会话为什么结束
    pub(super) fn teardown_overlay(
        &self,
        st: OverlayState,
        focus: FocusReturn,
        reason: OverlayEndReason,
    ) -> tauri::async_runtime::JoinHandle<()> {
        self.captures.clear_frames(st.session_id);
        self.screen
            .spawn(teardown_work(Arc::clone(&self.env), st, focus, reason))
    }

    /// 截图段里（已独占屏幕栅栏）就地拆掉 Overlay
    async fn teardown_now(&self, st: OverlayState, focus: FocusReturn, reason: OverlayEndReason) {
        self.captures.clear_frames(st.session_id);
        blocking(teardown_work(Arc::clone(&self.env), st, focus, reason)).await;
    }

    /// 没有任何显示器的 Overlay 显示出来（全部失败 / 4 s 内没有一块真正显示）：拆掉（交还焦点 → 销毁 → 恢复 pinned），
    /// 按 34.6 显示「截图失败」卡片。会话随即注销，之后再按截图快捷键可以重新进入
    pub(super) async fn abort_overlay(self: &Arc<Self>, session_id: u64) {
        let st = {
            let mut g = self.overlay.lock();
            if g.as_ref().is_some_and(|o| o.session_id == session_id && !o.committed) {
                g.take()
            } else {
                None
            }
        };
        let Some(st) = st else { return };
        let (run, trace, prev) = (st.run, st.trace, st.prev_app.clone());
        let direct = st.mode == OverlayMode::OcrTranslate;
        let _ = self
            .teardown_overlay(st, FocusReturn::BeforeDestroy, OverlayEndReason::Failed)
            .await;
        if self.advance(run, Event::NotReady).is_some() {
            let nr = NotReady::CaptureFailed {
                message: not_ready::capture_reason::BACKEND.to_owned(),
            };
            self.show_ocr_not_ready(
                run,
                trace,
                nr,
                direct,
                prev,
                AnchorSource::Cursor,
                PanelTransition::Enter,
            )
            .await;
        }
    }

    #[allow(clippy::too_many_arguments)] // 与 show_ocr_not_ready_session 同一组参数，拆成结构体只为凑数没有意义
    pub(super) async fn show_ocr_not_ready(
        self: &Arc<Self>,
        run: u64,
        trace: u64,
        nr: NotReady,
        direct: bool,
        prev: Option<AppRef>,
        anchor: AnchorSource,
        transition: PanelTransition,
    ) {
        let settings = self.env.settings();
        let payload = OcrPayload {
            capture_id: String::new(),
            request_id: None,
            mode: if direct {
                OcrMode::Translate
            } else {
                default_ocr_mode(&settings)
            },
            thumb_url: String::new(),
            image_size: PhysSize { width: 0, height: 0 },
            target_lang: settings.translate.primary_lang,
            not_ready: Some(nr),
        };
        let req = PanelShowRequest {
            view: PanelView::Ocr,
            payload: PanelPayload::Ocr(payload),
            anchor,
            take_focus: false,
            prev_app: prev,
            transition,
        };
        self.show(run, trace, req, None, None).await;
    }

    /// Overlay 拉取初始化数据。不阻塞
    pub fn overlay_init(&self, label: &str) -> Result<Option<OverlayInit>, IpcError> {
        #[cfg(any(debug_assertions, feature = "fake-llm"))]
        {
            let mut inits = LAB_OVERLAY_INITS.lock();
            if inits.len() >= 32 {
                inits.remove(0);
            }
            inits.push((label.to_owned(), Instant::now()));
        }
        let Some(monitor) = self.env.overlay_monitor(label) else {
            return Ok(None);
        };
        let mut g = self.overlay.lock();
        let Some(st) = g.as_mut() else {
            return Ok(None);
        };
        let frozen_native = match st.frozen.get(&monitor) {
            Some(Frozen::Known(b)) => *b,
            Some(Frozen::Pending) | None => return Ok(None),
        };
        let Some(frame) = self.captures.frame(st.session_id, &monitor) else {
            return Ok(None);
        };
        st.inited.insert(monitor.clone());
        let windows = if st.window_detect {
            st.windows
                .iter()
                .filter(|w| w.monitor_id == monitor)
                .map(|w| w.rect)
                .collect()
        } else {
            Vec::new()
        };
        Ok(Some(OverlayInit {
            session_id: st.session_id,
            monitor: frame.monitor.clone(),
            image_url: capture_asset_path(st.session_id, &monitor, CaptureVariant::Full),
            image_size: PhysSize {
                width: frame.width,
                height: frame.height,
            },
            windows,
            mode: st.mode,
            window_detect: st.window_detect,
            fly_animation: st.fly_animation,
            reduced_motion: st.reduced_motion,
            frozen_native,
            half_url: (!frozen_native).then(|| capture_asset_path(st.session_id, &monitor, CaptureVariant::Half)),
            // 原生冻结帧：放大镜与飞入按需拉取小块，不再拉整屏 image_url
            region_url: frozen_native.then(|| crate::assets::capture_region_base(st.session_id, &monitor)),
        }))
    }

    /// 首帧渲染完成 → 显示 Overlay（不带原生冻结帧的路径）。`overlay_show` 返回之后才把显示器记为已显示；
    /// 失败记为失败，全部显示器都失败时收尾并提示（[`Self::abort_overlay`]）
    pub fn overlay_ready(self: &Arc<Self>, session_id: u64, monitor_id: &str) {
        let should_show = {
            let mut g = self.overlay.lock();
            match g.as_mut().filter(|o| o.session_id == session_id) {
                Some(o)
                    if o.frozen.get(monitor_id) == Some(&Frozen::Known(false))
                        && !o.shown.contains(monitor_id)
                        && !o.showing.contains(monitor_id)
                        && !o.failed.contains(monitor_id) =>
                {
                    o.showing.insert(monitor_id.to_owned())
                }
                _ => false,
            }
        };
        if !should_show {
            return;
        }
        let core = Arc::clone(self);
        let monitor = monitor_id.to_owned();
        tauri::async_runtime::spawn(async move {
            let env = Arc::clone(&core.env);
            let m = monitor.clone();
            let shown = blocking(move || env.overlay_show(session_id, &m, None)).await;
            let all_failed = {
                let mut g = core.overlay.lock();
                match g.as_mut().filter(|o| o.session_id == session_id) {
                    Some(o) => {
                        o.showing.remove(&monitor);
                        match shown {
                            Some(Ok(_)) => {
                                if o.shown.is_empty() {
                                    core.env.perf().mark(o.trace, PerfStage::OverlayShown);
                                }
                                o.shown.insert(monitor);
                                false
                            }
                            other => {
                                if let Some(Err(e)) = other {
                                    log::warn!("overlay {monitor}: show failed: {}", e.message);
                                }
                                o.failed.insert(monitor);
                                o.failed.len() >= o.monitors.len()
                            }
                        }
                    }
                    None => false,
                }
            };
            if all_failed {
                log::warn!("overlay session {session_id}: every overlay failed to show; giving up");
                core.abort_overlay(session_id).await;
            }
        });
    }

    /// commit：裁剪 → 后台编码并发出请求 → 在 Overlay 下面准备并显示面板 → 返回飞入目标
    pub async fn overlay_commit(self: &Arc<Self>, c: OverlayCommit) -> Result<OverlayCommitAck, IpcError> {
        let (run, trace, started, mode, prev, key) = {
            let mut g = self.overlay.lock();
            let st = g
                .as_mut()
                .filter(|o| o.session_id == c.session_id && !o.committed)
                .ok_or_else(|| IpcError::not_found("overlay session"))?;
            if !st.monitors.contains(&c.monitor_id) {
                return Err(IpcError::not_found("overlay monitor"));
            }
            st.committed = true;
            // 从这一刻起 Overlay 的拆除就登记在屏幕栅栏上（拆除本身要等飞入结束），新流程会等它
            st.hold = Some(self.screen.hold());
            // commit 之后 Esc 交还页面（「只收起 Overlay」）：撤下截图会话的全局 Esc（锁外执行）
            let core = Arc::clone(self);
            tauri::async_runtime::spawn(async move { core.overlay_guards_changed() });
            (
                st.run,
                st.trace,
                st.started,
                st.mode,
                st.prev_app.clone(),
                st.key.take(),
            )
        };
        // commit 之后按键焦点不再跟随指针切屏（macOS 的原生跟随；轮询与页面的 overlay_focus 已经按 committed 停下）。
        // 只投递、不等主线程；放在会话锁外（窗口层要查 Overlay 的登记表）
        self.env.overlay_committed(c.session_id);
        let direct = mode == OverlayMode::OcrTranslate;
        let settings = self.env.settings();
        let anchor = AnchorSource::Rect(Anchor {
            monitor_id: c.monitor_id.clone(),
            rect: c.rect,
        });
        let frame = self.captures.frame(c.session_id, &c.monitor_id);
        let cropped = match frame {
            Some(frame) => {
                let rect = c.rect;
                blocking(move || wisp_capture::crop(&frame, rect)).await
            }
            None => None,
        };
        let readiness = not_ready::check_ocr(&settings, None, &self.keys());
        let (img, resolved) = match (cropped, readiness) {
            (Some(Ok(img)), Ok(resolved)) => (img, resolved),
            (cropped, readiness) => {
                let nr = match (cropped, readiness) {
                    (_, Err(nr)) => nr,
                    (Some(Err(e)), _) => {
                        not_ready::capture_not_ready(&e, self.env.wayland()).unwrap_or(NotReady::CaptureFailed {
                            message: not_ready::capture_reason::BACKEND.to_owned(),
                        })
                    }
                    _ => NotReady::CaptureFailed {
                        message: not_ready::capture_reason::BACKEND.to_owned(),
                    },
                };
                let mut fly_to = None;
                if self.advance(run, Event::NotReady).is_some() {
                    let session = self
                        .show_ocr_not_ready_session(run, trace, nr, direct, prev, anchor)
                        .await;
                    fly_to = self.fly_target(session, &c.monitor_id);
                    self.set_overlay_panel(c.session_id, session);
                }
                self.spawn_overlay_done_watchdog(c.session_id);
                return Ok(OverlayCommitAck { fly_to });
            }
        };
        log::info!(
            "overlay session {}: committed {:?} {}x{} px",
            c.session_id,
            c.kind,
            img.width(),
            img.height()
        );
        // Key：触发时预读的；没有预读（读取可能弹框）时现在就与编码并行地读——commit 之后 Overlay 只剩飞入与淡出
        // （约 360 ms），即使弹出授权框也只被盖住这一小会儿，换来请求先于面板可见（等 Overlay 拆完再读的话，
        // 每次启动后的第一次 OCR 首 token 晚约 400 ms）
        let key = key.map_or(KeySource::Read, KeySource::Prefetched);
        let session = self
            .launch_ocr(OcrLaunch {
                run,
                trace,
                started,
                img,
                direct,
                resolved,
                anchor,
                prev,
                transition: PanelTransition::Enter,
                key,
            })
            .await;
        // 飞入目标：panel_prepare 返回之后立即读 panel_state（窗口重建中没有会话几何时为 null）
        let fly_to = self.fly_target(session, &c.monitor_id);
        self.set_overlay_panel(c.session_id, session);
        self.spawn_overlay_done_watchdog(c.session_id);
        Ok(OverlayCommitAck { fly_to })
    }

    /// 识别一张图片：登记裁剪与请求 → 推进到请求阶段 → 后台缩放、编码并发出请求 → 在面板里显示 OCR 视图。截图的 commit
    /// 与剪贴板图片（[`Core::start_image_ocr`]）共用这一段，之后的重新识别、识别历史、缩略图都与截图相同。
    /// 返回显示出来的面板会话；run 已被取代（或面板没能显示）时为 None，登记的请求与裁剪随之释放
    pub(super) async fn launch_ocr(self: &Arc<Self>, l: OcrLaunch) -> Option<u64> {
        let OcrLaunch {
            run,
            trace,
            started,
            img,
            direct,
            resolved,
            anchor,
            prev,
            transition,
            key,
        } = l;
        let settings = self.env.settings();
        let crop = self.captures.new_crop(
            Owner::Unbound,
            PhysSize {
                width: img.width(),
                height: img.height(),
            },
            direct,
        );
        let target = settings.translate.primary_lang;
        let ocr_mode = if direct {
            OcrMode::Translate
        } else {
            default_ocr_mode(&settings)
        };
        let lane = if direct { Lane::OcrTranslate } else { Lane::Ocr };
        let id = self.registry.new_id();
        let token = self.registry.open(
            &id,
            OpenSpec {
                owner: Owner::Unbound,
                lane,
                model: resolved.info(),
                meta: direct.then_some(StreamEvent::Meta {
                    detected_source_lang: None,
                    target_lang: Some(target),
                    dict_mode: None,
                    low_confidence: None,
                }),
                sink: None,
                trace: Some(trace),
                run: Some(run),
                capture_id: Some(crop.id.clone()),
            },
        );
        self.set_run_request(run, &id);
        if self.advance(run, Event::Committed).is_none() {
            self.registry.remove(&id);
            self.captures.remove(&crop.id);
            return None;
        }
        log::info!(
            "run {run}: recognising {}x{} px → request {id} model={}/{}",
            crop.size.width,
            crop.size.height,
            resolved.provider.id,
            resolved.model.id
        );
        // 编码与请求在后台，与面板的 prepare 并行，不阻塞调用方（截图 commit 不等编码就返回飞入目标）
        {
            let core = Arc::clone(self);
            let job = OcrJob {
                id: id.clone(),
                token,
                resolved: resolved.clone(),
                mode: ocr_mode,
                target,
                crop: Arc::clone(&crop),
                two_step: direct && settings.ocr.direct_translate_transcribe,
                run: Some(run),
                trace: Some(trace),
                started: Some(started),
                key,
            };
            let max_edge = settings.ocr.max_edge_px.clamp(512, 4096);
            let protocol = resolved.provider.protocol;
            // OCR 历史开启时把裁剪帧留给历史用的缩略图：编码完成、请求已经可以发出之后另起阻塞任务生成，
            // 不占请求的关键路径；写入历史时不必再从编码字节解码
            let history_thumb = settings.ocr.history;
            #[cfg(test)]
            let encode_delay = *self.encode_delay.lock();
            tauri::async_runtime::spawn(async move {
                let crop_ref = Arc::clone(&job.crop);
                let encoded = blocking(move || {
                    #[cfg(test)]
                    std::thread::sleep(encode_delay);
                    let img = wisp_capture::fit_long_edge(img, max_edge);
                    let e = wisp_capture::encode_for_llm(&img, protocol);
                    let enc = Encoded {
                        bytes: Arc::new(e.bytes),
                        mime: e.mime,
                        width: e.width,
                        height: e.height,
                    };
                    (enc, history_thumb.then(|| (img.width(), img.height(), img.into_raw())))
                })
                .await;
                match encoded {
                    Some((enc, frame)) => {
                        crop_ref.set_ready(enc);
                        if let Some((w, h, rgba)) = frame {
                            crop_ref.spawn_history_thumb(w, h, rgba);
                        }
                    }
                    None => crop_ref.set_failed(),
                }
                core.execute_ocr(job).await;
            });
        }
        let payload = OcrPayload {
            capture_id: crop.id.clone(),
            request_id: Some(id.clone()),
            mode: ocr_mode,
            thumb_url: thumb_path(&crop.id),
            image_size: crop.size,
            target_lang: target,
            not_ready: None,
        };
        let req = PanelShowRequest {
            view: PanelView::Ocr,
            payload: PanelPayload::Ocr(payload),
            anchor,
            take_focus: false,
            prev_app: prev,
            transition,
        };
        self.show(run, trace, req, Some(id), Some(crop)).await
    }

    /// 识别一张现成的图片（剪贴板图片条目的「识别文字」）：没有截图与框选，图片一到就提交，之后与截图 OCR 相同
    /// （[`Core::launch_ocr`]）。从剪贴板发起时交叉淡入，目标应用沿用剪贴板会话记下的那个
    pub(super) fn start_image_ocr(self: &Arc<Self>, t: Trigger, img: RgbaImage) {
        let run = self.start_run(Flow::Ocr, t.trace);
        let core = Arc::clone(self);
        tauri::async_runtime::spawn(async move { core.image_ocr_flow(run, t, img).await });
    }

    async fn image_ocr_flow(self: Arc<Self>, run: u64, t: Trigger, img: RgbaImage) {
        let settings = self.env.settings();
        let readiness = not_ready::check_ocr(&settings, None, &self.keys());
        let transition = if t.origin == TriggerOrigin::Clipboard {
            PanelTransition::Crossfade
        } else {
            PanelTransition::Enter
        };
        // 读 Key 不会弹框时与编码并行地预读（与截图触发时的预读同理）
        let key = match &readiness {
            Ok(r) => self.prefetch_key_if_quiet(&r.provider),
            Err(_) => None,
        };
        // 被取代的 Overlay 拆完、进行中的截图段做完，再读前台应用、显示面板
        self.screen.settled().await;
        let prev = self.prev_app_for(t.origin).await;
        let anchor = AnchorSource::Cursor;
        match readiness {
            Err(nr) => {
                if self.advance(run, Event::NotReady).is_some() {
                    self.show_ocr_not_ready(run, t.trace, nr, false, prev, anchor, transition)
                        .await;
                }
            }
            Ok(resolved) => {
                let key = key.map_or(KeySource::Read, KeySource::Prefetched);
                self.launch_ocr(OcrLaunch {
                    run,
                    trace: t.trace,
                    started: t.started,
                    img,
                    direct: false,
                    resolved,
                    anchor,
                    prev,
                    transition,
                    key,
                })
                .await;
            }
        }
    }

    pub(super) async fn show_ocr_not_ready_session(
        self: &Arc<Self>,
        run: u64,
        trace: u64,
        nr: NotReady,
        direct: bool,
        prev: Option<AppRef>,
        anchor: AnchorSource,
    ) -> Option<u64> {
        self.show_ocr_not_ready(run, trace, nr, direct, prev, anchor, PanelTransition::Enter)
            .await;
        self.runs.lock().map.get(&run).and_then(|r| r.session)
    }

    pub(super) fn fly_target(&self, session: Option<u64>, monitor_id: &str) -> Option<wisp_types::Rect> {
        let session = session?;
        self.env
            .panel_state()
            .filter(|p| p.session_id == session && p.monitor_id == monitor_id)
            .map(|p| p.rect)
    }

    pub(super) fn set_overlay_panel(&self, overlay_session: u64, panel: Option<u64>) {
        if let Some(o) = self.overlay.lock().as_mut().filter(|o| o.session_id == overlay_session) {
            o.panel_session = panel;
        }
    }

    /// commit 之后：页面异常、迟迟不 `overlay_done` 时也要拆掉 Overlay
    pub(super) fn spawn_overlay_done_watchdog(self: &Arc<Self>, session_id: u64) {
        let core = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(OVERLAY_DONE_WAIT).await;
            let pending = core.overlay.lock().as_ref().is_some_and(|o| o.session_id == session_id);
            if pending {
                log::warn!("overlay session {session_id}: overlay_done never came; tearing down");
                core.finish_overlay(session_id).await;
            }
        });
    }

    /// 飞入 / 淡出结束：确认面板已显示，下一帧销毁 Overlay，交还焦点，恢复 pinned
    pub fn overlay_done(self: &Arc<Self>, session_id: u64) {
        let core = Arc::clone(self);
        tauri::async_runtime::spawn(async move { core.finish_overlay(session_id).await });
    }

    pub(super) async fn finish_overlay(self: &Arc<Self>, session_id: u64) {
        let (committed, trace) = match self.overlay.lock().as_ref().filter(|o| o.session_id == session_id) {
            Some(o) => (o.committed, o.trace),
            None => return,
        };
        if !committed {
            // 没有 commit 的 done（页面异常）：按取消处理
            self.end_overlay(session_id, OverlayEndReason::Cancelled);
            return;
        }
        // 面板在 commit 时已在 Overlay 下面准备；等它真的显示出来，两个窗口之间不出现空帧
        self.wait_shown(trace, Duration::from_millis(600)).await;
        tokio::time::sleep(FRAME).await;
        if let Some(st) = self.take_overlay(session_id) {
            log::debug!("overlay session {session_id} done");
            self.teardown_overlay(st, FocusReturn::AfterFadeOut, OverlayEndReason::Done);
        }
    }

    /// 截图会话是否需要全局 Esc：有会话且还没 commit
    fn overlay_wants_escape(&self) -> bool {
        self.overlay.lock().as_ref().is_some_and(|o| !o.committed)
    }

    /// 会话状态变了（开始 / commit / 结束）：在阻塞线程上把全局 Esc 调成**执行那一刻**的期望值。每次变化都派发一次、
    /// 执行时现读状态，所以即使两次派发的执行顺序颠倒，最后一次执行的结果也总是最新状态
    pub(super) fn overlay_guards_changed(&self) {
        let Some(me) = self.me.upgrade() else { return };
        tauri::async_runtime::spawn_blocking(move || {
            let want = me.overlay_wants_escape();
            me.env.overlay_escape_guard(want);
        });
    }

    /// 全局 Esc 按下（Esc guard）：有截图会话就按页面的 Esc 处理（未 commit 取消，commit 之后只收起 Overlay），返回 true
    pub fn escape_overlay(self: &Arc<Self>) -> bool {
        let session = self.overlay.lock().as_ref().map(|o| o.session_id);
        match session {
            Some(id) => {
                log::info!("overlay session {id}: global Esc");
                self.end_overlay(id, OverlayEndReason::Cancelled);
                true
            }
            None => false,
        }
    }

    /// 多屏截图：按键焦点跟随光标。只有一块屏的 Overlay 是 key，用户把光标移到另一块屏、不点击就按 Esc / ↩ /
    /// ⌘A / 空格时，按键原先仍送到最初那块屏的页面（⌘A 选中的是那块屏的整屏，应当是光标所在屏）。会话未 commit 期间
    /// 每 [`OVERLAY_FOLLOW_EVERY`] 看一次光标所在的屏，换屏时让那块屏的 Overlay 成为 key（不激活应用）；单屏不启动
    fn spawn_overlay_follow(self: &Arc<Self>, session_id: u64) {
        // 窗口层自己跟随指针（macOS 的 NSTrackingArea）：不轮询
        if self.env.overlay_follows_pointer() {
            return;
        }
        let multi = self
            .overlay
            .lock()
            .as_ref()
            .is_some_and(|o| o.session_id == session_id && o.monitors.len() > 1);
        if !multi {
            return;
        }
        let core = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            let mut last: Option<MonitorId> = None;
            loop {
                let live = core
                    .overlay
                    .lock()
                    .as_ref()
                    .filter(|o| o.session_id == session_id && !o.committed)
                    .map(|o| o.shown.clone());
                let Some(shown) = live else { break };
                let env = Arc::clone(&core.env);
                let cursor = blocking(move || env.cursor_monitor()).await.flatten();
                if let Some(m) = cursor
                    && last.as_ref() != Some(&m)
                    && shown.contains(&m)
                {
                    if last.is_some() {
                        let env = Arc::clone(&core.env);
                        let id = m.clone();
                        blocking(move || env.overlay_focus(session_id, &id)).await;
                        log::debug!("overlay session {session_id}: key follows the cursor to {m}");
                    }
                    last = Some(m);
                }
                tokio::time::sleep(OVERLAY_FOLLOW_EVERY).await;
            }
        });
    }

    /// `overlay_focus` 命令：页面报告指针进入了它（而它还不是 key）。未 commit 的会话里让该窗口所在
    /// 显示器的 Overlay 成为 key（与 [`Self::spawn_overlay_follow`] 同一个原生入口）。取舍：macOS
    /// 上非 key 的 WKWebView 收不到指针移动，页面发不出这个请求，由窗口层的 NSTrackingArea 跟随（不轮询）；Windows / X11
    /// 上页面的这次调用是事件驱动的主路径，会话期间 50 ms 的光标轮询暂时保留作兜底（`OVERLAY_FOLLOWS_POINTER`）
    pub fn overlay_focus_request(self: &Arc<Self>, session_id: u64, label: &str) {
        let live = self
            .overlay
            .lock()
            .as_ref()
            .is_some_and(|o| o.session_id == session_id && !o.committed);
        if !live {
            return;
        }
        let Some(monitor) = self.env.overlay_monitor(label) else {
            return;
        };
        let env = Arc::clone(&self.env);
        tauri::async_runtime::spawn_blocking(move || env.overlay_focus(session_id, &monitor));
    }

    /// 原生冻结帧已经显示（`shown` 非空）而页面一直没有 `overlay_init`：拆掉并显示截图失败卡片
    fn spawn_overlay_init_watchdog(self: &Arc<Self>, session_id: u64) {
        let core = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(OVERLAY_INIT_WAIT).await;
            let stuck = core.overlay.lock().as_ref().is_some_and(|o| {
                o.session_id == session_id && !o.committed && !o.shown.is_empty() && o.inited.is_empty()
            });
            if stuck {
                log::warn!("overlay session {session_id}: the page never initialised; giving up");
                core.abort_overlay(session_id).await;
            }
        });
    }

    /// Esc（页面的 `overlay_cancel`）：交还焦点 → 销毁 Overlay → 恢复 pinned；run 取消
    pub fn overlay_cancel(self: &Arc<Self>, session_id: u64) {
        self.end_overlay(session_id, OverlayEndReason::Cancelled);
    }

    /// 结束一个截图会话（页面取消、全局 Esc、再按截图快捷键）：未 commit 时交还焦点 → 销毁 Overlay → 恢复 pinned、run
    /// 取消；commit 之后（飞入期间）只拆 Overlay（同 overlay_done，销毁之后交还焦点）。`reason` 随 `overlay:end` 送给页面
    pub(super) fn end_overlay(self: &Arc<Self>, session_id: u64, reason: OverlayEndReason) {
        let Some(st) = self.take_overlay(session_id) else {
            return;
        };
        if st.committed {
            self.teardown_overlay(st, FocusReturn::AfterDestroy, reason);
            return;
        }
        let run = st.run;
        self.teardown_overlay(st, FocusReturn::BeforeDestroy, reason);
        self.advance(run, Event::Cancel);
    }

    /// 执行 OCR 请求（commit 与 `ocr_start` 共用）
    pub(super) async fn execute_ocr(self: Arc<Self>, mut job: OcrJob) {
        let key = std::mem::replace(&mut job.key, KeySource::Read);
        let end = self.ocr_pipeline(&job, key).await;
        self.record_ocr_history(&job, &end);
        self.request_ended(job.run, job.trace, &end);
    }

    /// OCR 历史（默认关闭）：请求以 Done 结束、识别出了文字时，把结果（translate 模式是
    /// 译文；两次调用的直接翻译同样是译文——这类条目不提供「重新翻译」）、原图（送模型时的编码，
    /// 与请求共用内存）、缩略图（编码截图时从裁剪帧生成的，没有时由存储层从原图生成）、模式与模型显示名交给剪贴板服务
    /// 记下。同一张截图同一模式的重新识别取代上一条：序号在这里（请求结束时）同步领取，「读要取代的条目 → 写库 → 记下
    /// 新条目」在后台任务里由 `Crop::record_history` 串行完成——两次重新识别的写库即使交错，也不会留下两条。
    /// 缓存命中的「查看原文」不经这里（没有发请求）。写入在阻塞线程上进行，不拖慢请求的收尾
    fn record_ocr_history(self: &Arc<Self>, job: &OcrJob, end: &PumpEnd) {
        if !matches!(end, PumpEnd::Done { no_text: false, .. })
            || job.token.is_cancelled()
            || !self.env.settings().ocr.history
        {
            return;
        }
        let Some(text) = self.registry.text(&job.id).filter(|t| !t.trim().is_empty()) else {
            return;
        };
        let image = match job.crop.wait(Duration::ZERO) {
            CropImage::Ready(enc) => Some(Arc::clone(&enc.bytes)),
            CropImage::Pending | CropImage::Failed => None,
        };
        let ticket = job.crop.history_ticket();
        let (mode, model) = (job.mode, job.resolved.info().model_label);
        let env = Arc::clone(&self.env);
        let crop = Arc::clone(&job.crop);
        tauri::async_runtime::spawn_blocking(move || {
            crop.record_history(mode, ticket, |replaces| {
                env.record_ocr_history(OcrRecord {
                    mode,
                    model,
                    text,
                    image,
                    thumb: crop.thumb(),
                    replaces,
                })
            });
        });
    }

    pub(super) async fn ocr_pipeline(self: &Arc<Self>, job: &OcrJob, key: KeySource) -> PumpEnd {
        // 没有预读的 Key 时，读 Key 与等编码并行。不需要 Key、也没存 Key 的 Provider（回环地址的本地模型）不访问钥匙串
        // （[`Self::key_not_needed`]，与翻译的预读同一口径）：钥匙串锁定 / 不可用时也不会走到弹框或失败的路径
        let key = match key {
            KeySource::Read if self.key_not_needed(&job.resolved.provider) => KeySource::Known(None),
            KeySource::Read => KeySource::Prefetched(self.prefetch_key(&job.resolved.provider.id)),
            other => other,
        };
        // 等编码完成（commit 时在后台进行；重新识别时早已完成）
        let crop = Arc::clone(&job.crop);
        let image = match blocking(move || crop.wait(ENCODE_WAIT)).await {
            Some(CropImage::Ready(enc)) => enc,
            _ => {
                if job.token.is_cancelled() {
                    return PumpEnd::Cancelled;
                }
                let err = LlmError::new(ErrorCode::Internal, "the screenshot is no longer available");
                self.registry.emit(&job.id, StreamEvent::Error(err.clone()));
                return PumpEnd::Failed(err);
            }
        };
        // 这张图是按 commit 时的协议编码的：重新识别 / 从菜单换到别的协议（例如 OpenAI 的 20 MB → Anthropic 的 5 MB、长边
        // 8000）超出上限时，按目标协议重新编码一次并替换缓存的那份
        let protocol = job.resolved.provider.protocol;
        let image = if wisp_capture::fits_protocol(image.bytes.len(), image.width, image.height, protocol) {
            image
        } else {
            let bytes = Arc::clone(&image.bytes);
            match blocking(move || wisp_capture::reencode_for_llm(&bytes, protocol))
                .await
                .flatten()
            {
                Some(e) => {
                    log::info!(
                        "request {}: screenshot re-encoded for {protocol:?} ({}x{} → {}x{})",
                        job.id,
                        image.width,
                        image.height,
                        e.width,
                        e.height
                    );
                    let enc = Encoded {
                        bytes: Arc::new(e.bytes),
                        mime: e.mime,
                        width: e.width,
                        height: e.height,
                    };
                    job.crop.set_ready(enc.clone());
                    Arc::new(enc)
                }
                None => image,
            }
        };
        let secret = match self.read_key(&job.id, &job.resolved, key, &job.token).await {
            Ok(s) => s,
            Err(end) => return end,
        };
        if job.token.is_cancelled() {
            return PumpEnd::Cancelled;
        }
        let settings = self.env.settings();
        let reasoning = job.resolved.model.reasoning;
        let first_mode = if job.two_step { OcrMode::Paragraphs } else { job.mode };
        let built = build_ocr_prompt(first_mode, job.target, reasoning);
        let post = built.post;
        // 两次调用模式的第二步还要用同一个 Key
        let (key_now, key_later) = if job.two_step {
            (secret.as_ref().map(|s| Secret::new(s.expose().to_owned())), secret)
        } else {
            (secret, None)
        };
        let req = wisp_llm::ChatRequest {
            request_id: job.id.clone(),
            provider: job.resolved.provider.clone(),
            key: key_now,
            model: job.resolved.model.clone(),
            system: built.system,
            user: UserContent::Image {
                bytes: image.llm_bytes(),
                mime: image.mime,
                text: built.user_text,
                detail: settings.ocr.image_detail,
            },
            temperature: built.temperature,
            max_output_tokens: built.max_output_tokens,
            purpose: Purpose::Ocr,
        };
        let rx = self.env.llm_stream(req, job.token.clone());
        log_dispatched(&job.id, job.trace, job.started, &job.resolved);
        if job.two_step {
            return self.direct_two_step(job, rx, post, key_later).await;
        }
        let (end, _) = pump(&self.registry, &job.id, rx, post, "").await;
        if let PumpEnd::Done { usage, no_text, .. } = &end {
            if let Some(u) = usage {
                self.env
                    .record_usage(&job.resolved.provider.id, &job.resolved.model.id, u);
            }
            let text = self.registry.text(&job.id).unwrap_or_default();
            if !no_text && !text.trim().is_empty() {
                if job.mode == OcrMode::Paragraphs && job.crop.direct {
                    // 直接翻译的「查看原文」：连同模型缓存下来，再次切换时不重复请求
                    job.crop.set_transcript(model_key(&job.resolved), text.clone());
                }
                if settings.ocr.auto_copy && job.mode != OcrMode::Translate && !job.crop.direct {
                    // 识别完成后自动复制（来源 Wisp，正常进入剪贴板历史）
                    let env = Arc::clone(&self.env);
                    let id = job.id.clone();
                    tauri::async_runtime::spawn_blocking(move || match env.copy_text(&text) {
                        Ok(()) => log::debug!("request {id}: OCR result copied"),
                        Err(e) => log::warn!("request {id}: auto copy failed: {}", e.message),
                    });
                }
            }
        }
        end
    }

    /// 直接翻译 + 同时转录原文：第一步 paragraphs 转录不转发（缓存在截图上），第二步把转录文本交给翻译管线
    pub(super) async fn direct_two_step(
        self: &Arc<Self>,
        job: &OcrJob,
        mut rx: tokio::sync::mpsc::Receiver<StreamEvent>,
        post: PostKind,
        secret: Option<Secret>,
    ) -> PumpEnd {
        let mut pp = wisp_prompts::PostProcessor::new(post, "");
        let mut transcript = String::new();
        let outcome = loop {
            match rx.recv().await {
                Some(StreamEvent::Delta { text }) => {
                    if let Some(out) = pp.push(&text) {
                        transcript.push_str(&out);
                    }
                }
                Some(StreamEvent::Done { usage, elapsed_ms, .. }) => {
                    let fin = pp.finish();
                    transcript.push_str(&fin.tail);
                    break Ok((usage, fin.no_text, elapsed_ms));
                }
                Some(StreamEvent::Error(e)) => break Err(e),
                Some(_) => {}
                None => {
                    break Err(LlmError::new(
                        ErrorCode::Internal,
                        "the stream ended without a terminal event",
                    ));
                }
            }
        };
        drop(rx);
        let (usage, no_text, elapsed_ms) = match outcome {
            Err(e) if e.code == ErrorCode::Cancelled || job.token.is_cancelled() => return PumpEnd::Cancelled,
            Err(e) => {
                self.registry.emit(&job.id, StreamEvent::Error(e.clone()));
                return PumpEnd::Failed(e);
            }
            Ok(v) => v,
        };
        if let Some(u) = &usage {
            self.env
                .record_usage(&job.resolved.provider.id, &job.resolved.model.id, u);
        }
        if no_text || transcript.trim().is_empty() {
            let done = StreamEvent::Done {
                elapsed_ms,
                usage,
                truncated: None,
                cached: false,
                no_text: true,
            };
            return if self.registry.emit(&job.id, done) {
                PumpEnd::Done {
                    usage,
                    truncated: None,
                    no_text: true,
                    elapsed_ms,
                }
            } else {
                PumpEnd::Cancelled
            };
        }
        job.crop.set_transcript(model_key(&job.resolved), transcript.clone());
        // 第二步：复用翻译管线（检测、方向、默认风格、词典式、缓存），目标语言沿用截图时的
        let settings = self.env.settings();
        let styles = self.env.styles();
        let plan = plan_text(
            &settings,
            &styles,
            &TextInput {
                text: &transcript,
                task: TaskKind::Translate,
                style_id: &settings.translate.default_style,
                target: Some(job.target),
                dict_mode: DictMode::Auto,
            },
        );
        let resolved = match not_ready::check_text(&settings, &plan.style, None, TaskKind::Translate, &self.keys()) {
            Ok(r) => r,
            Err(nr) => {
                let err = not_ready_error(&nr);
                self.registry.emit(&job.id, StreamEvent::Error(err.clone()));
                return PumpEnd::Failed(err);
            }
        };
        if !self.registry.emit(&job.id, plan.meta()) {
            return PumpEnd::Cancelled;
        }
        // 同一个 Provider 时沿用已读出的 Key，不再访问钥匙串
        let key = if resolved.provider.id == job.resolved.provider.id {
            KeySource::Known(secret)
        } else {
            KeySource::Read
        };
        let text_job = TextJob {
            id: job.id.clone(),
            token: job.token.clone(),
            plan,
            resolved,
            text: Arc::new(transcript),
            bypass_cache: false,
            draft: None,
            run: None,
            trace: job.trace,
            started: job.started,
            key: None,
        };
        self.text_pipeline(&text_job, key).await
    }

    /// 前端发起的 OCR：重新识别、切换模式 / 模型、直接翻译时查看原文（面板或 pinned）
    pub async fn ocr_start(self: &Arc<Self>, req: OcrReq, host: RequestHost, sink: Sink) -> Result<StartAck, IpcError> {
        let owner = owner_of(&host);
        let crop = self
            .captures
            .crop(&req.capture_id)
            .filter(|c| c.owner() == owner)
            .ok_or_else(|| IpcError::not_found("capture"))?;
        let settings = self.env.settings();
        let target = req.target_lang.unwrap_or(settings.translate.primary_lang);
        let lane = if req.mode == OcrMode::Translate {
            Lane::OcrTranslate
        } else {
            Lane::Ocr
        };
        let readiness = not_ready::check_ocr(&settings, req.model_ref.as_ref(), &self.keys());
        let id = self.registry.new_id();
        let meta = (req.mode == OcrMode::Translate).then_some(StreamEvent::Meta {
            detected_source_lang: None,
            target_lang: Some(target),
            dict_mode: None,
            low_confidence: None,
        });
        let token = self.registry.open(
            &id,
            OpenSpec {
                owner: owner.clone(),
                lane,
                model: readiness.as_ref().map(Resolved::info).unwrap_or_default(),
                meta,
                sink: Some(sink),
                trace: None,
                run: None,
                capture_id: Some(crop.id.clone()),
            },
        );
        match readiness {
            Err(nr) => {
                self.registry.emit(&id, StreamEvent::Error(not_ready_error(&nr)));
            }
            Ok(resolved) => {
                // 「查看原文」：转录已缓存时不重复请求；用户在「重新识别 ▾」里明确换了模型时，只有同一个
                // 模型的转录才算命中，否则重新请求并覆盖缓存
                let wanted = req.model_ref.as_ref().map(|_| model_key(&resolved));
                let cached = (crop.direct && req.mode == OcrMode::Paragraphs)
                    .then(|| crop.transcript_for(wanted.as_ref()))
                    .flatten();
                if let Some(transcript) = cached {
                    let _ = self.registry.emit(&id, StreamEvent::Delta { text: transcript })
                        && self.registry.emit(
                            &id,
                            StreamEvent::Done {
                                elapsed_ms: 0,
                                usage: None,
                                truncated: None,
                                cached: true,
                                no_text: false,
                            },
                        );
                } else {
                    log::info!(
                        "request {id} (ocr {:?}) from {owner:?} capture={} model={}/{}",
                        req.mode,
                        crop.id,
                        resolved.provider.id,
                        resolved.model.id
                    );
                    let job = OcrJob {
                        id: id.clone(),
                        token,
                        resolved,
                        mode: req.mode,
                        target,
                        crop,
                        two_step: false,
                        run: None,
                        trace: None,
                        started: None,
                        key: KeySource::Read,
                    };
                    let core = Arc::clone(self);
                    tauri::async_runtime::spawn(async move { core.execute_ocr(job).await });
                }
            }
        }
        if matches!(owner, Owner::Panel(_)) {
            self.reconcile();
        }
        Ok(StartAck {
            request_id: id,
            width: None,
            long_text: None,
        })
    }
}
