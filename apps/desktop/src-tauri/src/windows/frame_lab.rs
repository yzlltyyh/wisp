//! 面板帧模式实验台（**只在 debug 构建或带 `fake-llm` feature 的性能构建里编译**）：不依赖编排器与视图，反复让面板进入 → 流式增长 →
//! 收缩 → 一次性大幅增长 → 退出，用来做 120 fps 录屏核对、逐帧耗时与 CPU 度量。
//!
//! 用法（debug 构建）：
//! ```text
//! WISP_FRAME_LAB=native|native-css-fill|css   # 帧模式；native-css-fill = 着色由 CSS 画（对照组）
//! WISP_FRAME_LAB_SESSIONS=8                   # 会话数（默认 8）
//! WISP_FRAME_LAB_PLACEMENT=alternate          # below | above | alternate（默认交替）
//! WISP_FRAME_LAB_EXIT=1                       # 跑完后以 0 退出；页面没加载 / 找不到显示器 / 会话 30 s 没走完时以 1 退出
//! WISP_FRAME_LAB_RECYCLE=1                    # 每个会话结束后回收面板（空闲卸载的变体），下一个会话跑在
//!                                             # 回收出来的新页面上（WebContent 新进程、JIT 冷）：对照热页面的逐帧统计
//! ```
//! Rust 侧按固定几何驱动 prepare → 等前端 `panel_hide` → 间隔 0.7 s → 下一次；前端看到注入的
//! `window.__WISP_FRAME_LAB__` 后用 `panel/shell/FrameLab.tsx` 代替视图生成内容。macOS 上同时打开逐帧统计
//! （`frame_stats` 日志行）并记录 WebContent 进程 pid（`frame_lab pids` 日志行）供外部采样 CPU。

use std::sync::Arc;
use std::time::Duration;

use tauri::Manager;
use wisp_types::{FrameMode, Lang, PanelPayload, PanelTransition, PanelView, Placement, TaskKind, TranslatePayload};

use super::native;
use super::outside_click::GuardSpec;
use super::panel::{PANEL_LABEL, PanelController, PanelOpen};
use super::placement::{self, PanelGeometry, Screen};
use crate::state::AppState;

/// 实验台里卡片的着色由谁画
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabFill {
    /// 原生着色层（Native 帧模式的定案做法）
    Native,
    /// CSS 画着色（对照组：观察 WKWebView 异步重排造成的底边缺着色条）
    Css,
}

/// 放置方向
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabPlacement {
    Below,
    Above,
    Alternate,
}

/// 实验台配置（来自环境变量）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LabConfig {
    pub frame_mode: FrameMode,
    pub fill: LabFill,
    pub sessions: u32,
    pub placement: LabPlacement,
    pub exit_when_done: bool,
    /// 每个会话结束后回收面板（`WISP_FRAME_LAB_RECYCLE=1`）
    pub recycle: bool,
}

impl LabConfig {
    /// 读环境变量；没有设置 `WISP_FRAME_LAB` 时返回 None
    pub fn from_env() -> Option<Self> {
        let mode = std::env::var("WISP_FRAME_LAB").ok()?;
        let (frame_mode, fill) = match mode.trim() {
            "native" => (FrameMode::Native, LabFill::Native),
            "native-css-fill" => (FrameMode::Native, LabFill::Css),
            "css" => (FrameMode::Css, LabFill::Css),
            other => {
                log::warn!("WISP_FRAME_LAB={other} 无法识别（native | native-css-fill | css），忽略");
                return None;
            }
        };
        let sessions = std::env::var("WISP_FRAME_LAB_SESSIONS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8);
        let placement = match std::env::var("WISP_FRAME_LAB_PLACEMENT").as_deref() {
            Ok("below") => LabPlacement::Below,
            Ok("above") => LabPlacement::Above,
            _ => LabPlacement::Alternate,
        };
        let exit_when_done = std::env::var("WISP_FRAME_LAB_EXIT").is_ok_and(|v| v == "1");
        let recycle = std::env::var("WISP_FRAME_LAB_RECYCLE").is_ok_and(|v| v == "1");
        Some(Self {
            frame_mode,
            fill,
            sessions,
            placement,
            exit_when_done,
            recycle,
        })
    }

    /// 注入面板的附加初始化脚本（前端据此挂载 FrameLab）
    pub fn init_script(&self) -> String {
        let fill = match self.fill {
            LabFill::Native => "native",
            LabFill::Css => "css",
        };
        format!(r#"window.__WISP_FRAME_LAB__=Object.freeze({{fill:"{fill}",marker:true}});"#)
    }

    /// 原生着色层是否要画（对照组由 CSS 画，原生着色层与描边透明）
    pub fn surface(&self, surface: native::CardSurface) -> native::CardSurface {
        match self.fill {
            LabFill::Native => surface,
            LabFill::Css => native::CardSurface {
                fill: [0.0; 4],
                hairline: [0.0; 4],
                ..surface
            },
        }
    }

    fn placement_of(&self, i: u32) -> Placement {
        match self.placement {
            LabPlacement::Below => Placement::Below,
            LabPlacement::Above => Placement::Above,
            LabPlacement::Alternate if i.is_multiple_of(2) => Placement::Below,
            LabPlacement::Alternate => Placement::Above,
        }
    }
}

/// 实验台几何（主屏的本屏逻辑坐标）：左上区域；Below 顶边 y = 140，Above 底边 y = 900
fn geometry(screen: &Screen, placement: Placement) -> PanelGeometry {
    PanelGeometry {
        screen: screen.clone(),
        x: 240.0,
        edge_y: if placement == Placement::Above { 900.0 } else { 140.0 },
        placement,
        width: 420.0,
        max_height: 560.0,
        cap: 560.0,
        origin_x: 24.0,
    }
}

/// `WISP_FRAME_LAB_RECYCLE=1` 时会话结束后多久回收
const RECYCLE_AFTER: Duration = Duration::from_millis(300);

/// 实验台失败：`WISP_FRAME_LAB_EXIT=1` 时以 1 退出，外层脚本（run_lab.sh、CI）立即失败而不是一直挂着
fn fail(cfg: &LabConfig, app: &tauri::AppHandle) {
    if cfg.exit_when_done {
        app.exit(1);
    }
}

/// 启动实验台循环（预热完成后调用）
pub fn spawn(
    cfg: LabConfig,
    app: tauri::AppHandle,
    panel: Arc<PanelController>,
    next_session: impl Fn() -> u64 + Send + 'static,
) {
    tauri::async_runtime::spawn(async move {
        let mut loaded = panel.loaded();
        let page_loaded = tokio::time::timeout(Duration::from_secs(15), loaded.wait_for(|v| *v > 0))
            .await
            .is_ok_and(|r| r.is_ok());
        if !page_loaded {
            log::error!("frame_lab: panel page did not load");
            fail(&cfg, &app);
            return;
        }
        // 等 FrameLab 的懒加载 chunk 与字体就绪
        tokio::time::sleep(Duration::from_millis(1200)).await;
        native::set_frame_stats(true);
        if let Some(win) = app.get_webview_window(PANEL_LABEL) {
            log::info!(
                "frame_lab pids app={} web_content={:?} frame_mode={} fill={:?}",
                std::process::id(),
                native::web_content_pid(&win),
                cfg.frame_mode.as_str(),
                cfg.fill
            );
        }
        let screen = app.try_state::<AppState>().and_then(|state| {
            let screens: Vec<Screen> = state.platform.screen.monitors().iter().map(Screen::from_info).collect();
            placement::primary_screen(&screens).cloned()
        });
        let Some(screen) = screen else {
            log::error!("frame_lab: no monitor");
            fail(&cfg, &app);
            return;
        };
        if cfg.recycle {
            // 会话结束（隐藏）后很快回收；每个会话之前等回收出来的页面加载完，再等与首次相同的就绪时间
            panel.set_options(super::panel::PanelOptions {
                recycle_after: Some(RECYCLE_AFTER),
                ..panel.options()
            });
        }
        // 等窗口真正离屏（hidden 在隐藏开始时就会通知）
        let mut hidden = panel.offscreen();
        // 上一个会话结束时的页面加载次数（回收模式据此等回收出来的页面）
        let mut loads_at_end = *loaded.borrow();
        for i in 0..cfg.sessions {
            if cfg.recycle && i > 0 {
                let reloaded = tokio::time::timeout(Duration::from_secs(10), loaded.wait_for(|v| *v > loads_at_end))
                    .await
                    .is_ok_and(|r| r.is_ok());
                if !reloaded {
                    log::error!("frame_lab: panel was not recycled; panel: {}", panel.describe());
                    fail(&cfg, &app);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(1200)).await;
                if let Some(win) = app.get_webview_window(PANEL_LABEL) {
                    log::info!("frame_lab recycled web_content={:?}", native::web_content_pid(&win));
                }
            }
            let session_id = next_session();
            let placement = cfg.placement_of(i);
            let open = PanelOpen {
                session_id,
                view: PanelView::Translate,
                payload: PanelPayload::Translate(TranslatePayload {
                    source_text: "frame lab".to_owned(),
                    request_id: None,
                    task: TaskKind::Translate,
                    style_id: "frame-lab".to_owned(),
                    detected_lang: None,
                    target_lang: Lang::ZhCn,
                    dict_mode: false,
                    long_text: None,
                    not_ready: None,
                    selection_method: None,
                }),
                geometry: geometry(&screen, placement),
                focused: false,
                transition: PanelTransition::Enter,
                estimated_height: 120.0,
                prev_app: None,
                guards: GuardSpec::default(),
            };
            // fresh = 启动时预热、从未显示过的页面；recycled = 回收出来的页面（同样从未显示过）；warm = 用过的页面
            let page = match (i, cfg.recycle) {
                (0, _) => "fresh",
                (_, true) => "recycled",
                (_, false) => "warm",
            };
            log::info!("frame_lab session={session_id} placement={placement:?} page={page} begin");
            panel.prepare(open);
            let finished = tokio::time::timeout(Duration::from_secs(30), hidden.wait_for(|v| *v == session_id))
                .await
                .is_ok_and(|r| r.is_ok());
            if !finished {
                // 记下卡在哪里：窗口没在屏幕上 / alpha 停在 0、displayLink 没在跑（显示器睡眠或锁屏时不出帧）、
                // 会话停在哪个阶段
                let window = app
                    .get_webview_window(PANEL_LABEL)
                    .map_or_else(|| "missing".to_owned(), |w| native::describe_window(&w));
                log::error!(
                    "frame_lab session={session_id} did not finish in 30 s; window: {window}; panel: {}",
                    panel.describe()
                );
                fail(&cfg, &app);
                return;
            }
            loads_at_end = *loaded.borrow();
            log::info!("frame_lab session={session_id} end");
            tokio::time::sleep(Duration::from_millis(700)).await;
        }
        log::info!("frame_lab done sessions={}", cfg.sessions);
        if cfg.exit_when_done {
            tokio::time::sleep(Duration::from_millis(300)).await;
            app.exit(0);
        }
    });
}
