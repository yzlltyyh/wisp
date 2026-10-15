//! Pinned 固定窗口：数量 0–N，label 为 `pinned-<id>`。
//!
//! **交接时序**（不出现空帧）：
//! 1. `panel_pin` → [`PinnedRegistry::open`]：在面板同一矩形**隐藏**创建 pinned（模板 `pinned`，平台差异见
//!    `native::template_overrides`；macOS 隐藏交通灯、Windows DWM 圆角），把进行中的流交给新窗口；面板保持可见；
//! 2. pinned 页面 `pinned_init` → `stream_attach` 回放完 → opacity 0 同步渲染并测量 → `pinned_ready(pinned_id, h)`；
//! 3. [`PinnedRegistry::begin_ready`]：在**同一个主线程回调**里显示 pinned 并隐藏面板（`native::pinned_handoff`），返回后
//!    前端播放「浮起」（`presets.pinLift`）。只隐藏**发起固定的那个面板会话**：交接完成之前用户又触发了新的翻译
//!    （⌥D），新会话照常显示，固定窗口在旁边出现。
//!
//! **展开 ⤢**：目标是宽 640、高 min(内容, 屏幕 70%)、在面板所在屏幕居中。Native 帧模式（macOS）在面板矩形处交接后由
//! 原生矩形弹簧（spring.gentle）逐帧过渡到目标；Css 帧模式（Windows / Linux）直接在目标矩形显示，面板由 CSS 淡出（fast）
//! 后自己隐藏，与 pinned 的淡入 + scale 交叉。页面迟迟不 ready 时 [`READY_FALLBACK`] 后照样交接，避免面板卡在屏幕上。
//!
//! 关闭即销毁（前端 `close()`）；不随外部点击、失焦或再次触发关闭。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tauri::{Manager, WebviewWindowBuilder, WindowEvent};
use wisp_types::{Boot, PinState, Rect};

use super::native;
use super::placement::{self, Screen};
use super::settings_window::window_background;
use super::sizing::SHADOW_MARGIN;
use crate::error::IpcError;

/// Pinned 窗口模板的 label（tauri.conf.json，`create: false`）
pub const PINNED_TEMPLATE: &str = "pinned";
/// Pinned 窗口 label 的前缀
pub const PINNED_PREFIX: &str = "pinned-";
/// 页面迟迟不 ready 时的兜底交接时间
pub const READY_FALLBACK: Duration = Duration::from_millis(1500);

/// pinned_id → 窗口 label
pub fn label_of(pinned_id: &str) -> String {
    format!("{PINNED_PREFIX}{pinned_id}")
}

/// 窗口 label → pinned_id（不是 pinned 窗口时为 None）
pub fn id_of(label: &str) -> Option<&str> {
    label.strip_prefix(PINNED_PREFIX)
}

/// 一个固定窗口
#[derive(Debug, Clone)]
struct Entry {
    state: PinState,
    expand: bool,
    /// 所在屏幕与起始卡片矩形（本屏逻辑坐标，= 面板卡片）
    screen: Screen,
    from: Rect,
    /// 发起固定的面板会话（交接时只隐藏它）
    session: Option<u64>,
    /// 已经交接过
    handed_off: bool,
    /// 当前是否置顶（创建时取 state.json 的默认值，之后随 `pinned_set_on_top` 更新；`PinState.on_top`）
    on_top: bool,
}

/// 创建 pinned 的参数
#[derive(Debug, Clone)]
pub struct PinRequest {
    pub state: PinState,
    pub expand: bool,
    /// 面板所在屏幕
    pub screen: Screen,
    /// 起始卡片矩形（本屏逻辑坐标）
    pub from: Rect,
    /// 发起固定的面板会话：交接时只隐藏它（期间来了新会话时新会话不受影响）
    pub session: Option<u64>,
    /// 是否置顶（state.json 记住的默认值）
    pub on_top: bool,
}

/// 交接所需的信息（由 [`PinnedRegistry::begin_ready`] 给出）
#[derive(Debug, Clone)]
pub struct Handoff {
    pub label: String,
    /// 交接时 pinned 的窗口矩形（桌面坐标）
    pub rect: Rect,
    /// 展开的目标矩形（桌面坐标）
    pub expand_to: Option<Rect>,
    /// 发起固定的面板会话
    pub session: Option<u64>,
}

/// 固定窗口登记表
#[derive(Default)]
pub struct PinnedRegistry {
    entries: Mutex<HashMap<String, Entry>>,
    next: AtomicU64,
}

impl PinnedRegistry {
    /// 空表
    pub fn new() -> Self {
        Self::default()
    }

    /// 卡片矩形 → pinned 的窗口矩形：Linux 透明窗口由 CSS 画圆角阴影，四周留阴影边距；其他平台窗口就是卡片
    fn window_rect(card: Rect) -> Rect {
        if native::pinned_css_shadow() {
            Rect::new(
                card.x - SHADOW_MARGIN,
                card.y - SHADOW_MARGIN,
                card.width + 2.0 * SHADOW_MARGIN,
                card.height + 2.0 * SHADOW_MARGIN,
            )
        } else {
            card
        }
    }

    /// 隐藏创建一个 pinned 窗口，放在 `req.from`（本屏逻辑坐标的卡片矩形），返回 pinned_id。
    /// `boot_for` 按新 label 生成 boot；`before_build(pinned_id)` 在窗口开始加载页面之前调用（流的所有权转移）。
    ///
    /// 窗口一建好页面就开始加载，可能很快就调用 `pinned_init` / `stream_attach`，所以**先登记、再建窗口**（与 Overlay
    /// 一致）：页面任何时候来查都查得到；建窗口失败时撤销登记
    pub fn open(
        &self,
        app: &tauri::AppHandle,
        boot_for: impl FnOnce(&str) -> Boot,
        req: PinRequest,
        before_build: impl FnOnce(&str),
    ) -> Result<String, IpcError> {
        let PinRequest {
            state,
            expand,
            screen,
            from,
            session,
            on_top,
        } = req;
        let id = (self.next.fetch_add(1, Ordering::Relaxed) + 1).to_string();
        let label = label_of(&id);
        let boot = boot_for(&label);
        let mut config = app
            .config()
            .app
            .windows
            .iter()
            .find(|w| w.label == PINNED_TEMPLATE)
            .cloned()
            .ok_or_else(|| IpcError::not_found("window template pinned"))?;
        config.label.clone_from(&label);
        let frame_mode = boot.frame_mode;
        let rect = screen.to_desktop(Self::window_rect(from), native::DESKTOP_UNIT);
        self.entries.lock().insert(
            id.clone(),
            Entry {
                state,
                expand,
                screen,
                from,
                session,
                handed_off: false,
                on_top,
            },
        );
        before_build(&id);
        // 刚销毁的窗口（例如截图后的 Overlay）的 `Destroyed` 处理完再建，免得新窗口分到它的地址而被误伤
        native::await_destroyed(app);
        let built = WebviewWindowBuilder::from_config(app, &config).and_then(|builder| {
            let mut builder = builder
                .initialization_script(super::boot_script(&boot))
                .visible(false)
                .always_on_top(on_top);
            if !native::pinned_css_shadow() {
                // 卡片背景是 --surface-solid：首帧之前窗口与 WebView 透出同一种颜色，不白闪
                builder = builder.background_color(window_background(boot.theme));
            }
            let builder = native::template_overrides(builder, native::TemplateKind::Pinned, frame_mode);
            builder.build()
        });
        let win = match built {
            Ok(win) => win,
            Err(e) => {
                self.entries.lock().remove(&id);
                log::error!("create pinned window id={id} failed: {e}");
                return Err(e.into());
            }
        };
        // 用户关闭之后窗口必须真的释放（macOS 的 tao 引用泄漏，见 native::adopt_window）
        native::adopt_window(&win);
        if let Err(e) = native::pinned_attach(&win) {
            log::warn!("pinned attach failed: {}", e.message);
        }
        if let Err(e) = native::set_window_frame(&win, rect) {
            log::warn!("place pinned window failed: {}", e.message);
        }
        log::info!("pinned window created id={id} expand={expand}");
        Ok(id)
    }

    /// Pinned 页面启动时拉取的初始状态。Rust 填上 `expand_width`（展开时首帧测量用的卡片
    /// 宽度，与 `placement::pinned_expand_rect` 同一来源）与 `on_top`（窗口当前是否置顶），覆盖 `panel_pin` 传入的值
    pub fn state(&self, pinned_id: &str) -> Option<PinState> {
        self.entries.lock().get(pinned_id).map(|e| {
            let mut state = e.state.clone();
            state.expand_width = e.expand.then(|| expand_width_px(&e.screen));
            state.on_top = Some(e.on_top);
            state
        })
    }

    /// 记下窗口当前的置顶状态（`pinned_set_on_top` 成功之后；页面重载后 `pinned_init` 据此给出正确的初值）
    pub fn set_on_top(&self, pinned_id: &str, on_top: bool) {
        if let Some(e) = self.entries.lock().get_mut(pinned_id) {
            e.on_top = on_top;
        }
    }

    /// `pinned_ready`：标记为已交接并给出交接矩形；未知或已交接过时返回 None。
    /// `content_height` 是前端测量的卡片高度（展开时定高，min(内容, 屏幕 70%)）
    pub fn begin_ready(&self, pinned_id: &str, content_height: f64) -> Option<Handoff> {
        let mut entries = self.entries.lock();
        let e = entries.get_mut(pinned_id).filter(|e| !e.handed_off)?;
        e.handed_off = true;
        let unit = native::DESKTOP_UNIT;
        let rect = e.screen.to_desktop(Self::window_rect(e.from), unit);
        let expand_to = e.expand.then(|| {
            let target = placement::pinned_expand_rect(&e.screen, content_height);
            e.screen.to_desktop(Self::window_rect(target), unit)
        });
        Some(Handoff {
            label: label_of(pinned_id),
            rect,
            expand_to,
            session: e.session,
        })
    }

    /// 窗口已销毁：移除登记
    pub fn remove(&self, pinned_id: &str) {
        if self.entries.lock().remove(pinned_id).is_some() {
            log::info!("pinned window closed id={pinned_id}");
        }
    }

    /// 全部已交接（在屏幕上或被截图暂时隐藏）的 pinned_id
    pub fn shown_ids(&self) -> Vec<String> {
        self.entries
            .lock()
            .iter()
            .filter(|(_, e)| e.handed_off)
            .map(|(id, _)| id.clone())
            .collect()
    }
}

/// 展开宽度（逻辑 px，取整；工作区再窄也不小于固定窗口的最小宽度）
fn expand_width_px(screen: &Screen) -> u32 {
    let w = placement::pinned_expand_width(screen).round();
    if w.is_finite() && w > 0.0 { w as u32 } else { 0 }
}

/// 给新建的 pinned 窗口挂上生命周期回调：销毁时移除登记（`on_closed`）、移动 / 缩放时更新自身矩形表（`on_moved`）
pub fn watch_window(
    win: &tauri::WebviewWindow,
    on_closed: impl Fn() + Send + Sync + 'static,
    on_moved: impl Fn() + Send + Sync + 'static,
) {
    let window = win.clone();
    win.on_window_event(move |ev| match ev {
        // 用户关闭（✕ / ⌘W / Alt+F4）：由 Wisp 销毁——先释放逐帧驱动（展开过渡的 displayLink 不能留在离屏窗口上），
        // 再在主线程上销毁
        WindowEvent::CloseRequested { api, .. } => {
            api.prevent_close();
            native::destroy_window(&window);
        }
        WindowEvent::Destroyed => {
            // Rust 直接 destroy() 时不经过 CloseRequested：销毁后再兜底释放一次
            native::release_offscreen_drivers(window.app_handle());
            on_closed();
        }
        WindowEvent::Moved(_) | WindowEvent::Resized(_) => on_moved(),
        _ => {}
    });
}

/// 取 pinned 窗口
pub fn window(app: &tauri::AppHandle, pinned_id: &str) -> Option<tauri::WebviewWindow> {
    app.get_webview_window(&label_of(pinned_id))
}

#[cfg(test)]
mod tests {
    use wisp_types::{Lang, PhysPoint, PinKind, TaskKind};

    use super::*;

    fn pin_state() -> PinState {
        PinState {
            kind: PinKind::Translate,
            source_text: "a".to_owned(),
            result_text: "b".to_owned(),
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
        }
    }

    fn screen() -> Screen {
        Screen {
            id: "1".to_owned(),
            frame: Rect::new(0.0, 0.0, 1440.0, 900.0),
            work: Rect::new(0.0, 25.0, 1440.0, 805.0),
            scale: 2.0,
            physical_origin: PhysPoint { x: 0, y: 0 },
            primary: true,
        }
    }

    #[test]
    fn labels_round_trip() {
        assert_eq!(label_of("7"), "pinned-7");
        assert_eq!(id_of("pinned-7"), Some("7"));
        assert_eq!(id_of("panel"), None);
    }

    #[test]
    fn ready_hands_off_once_and_expands() {
        let reg = PinnedRegistry::new();
        reg.entries.lock().insert(
            "1".to_owned(),
            Entry {
                state: pin_state(),
                expand: true,
                screen: screen(),
                from: Rect::new(300.0, 230.0, 420.0, 200.0),
                session: Some(7),
                handed_off: false,
                on_top: true,
            },
        );
        assert_eq!(reg.state("1").map(|s| s.result_text), Some("b".to_owned()));
        // Rust 填上展开宽度与当前置顶状态
        let st = reg.state("1").expect("state");
        assert_eq!(st.expand_width, Some(640));
        assert_eq!(st.on_top, Some(true));
        reg.set_on_top("1", false);
        assert_eq!(reg.state("1").and_then(|s| s.on_top), Some(false));
        assert!(reg.shown_ids().is_empty());
        let h = reg.begin_ready("1", 300.0).expect("handoff");
        assert_eq!(h.label, "pinned-1");
        assert_eq!(h.session, Some(7), "交接只针对发起固定的面板会话");
        if !native::pinned_css_shadow() {
            assert_eq!(
                h.rect,
                screen().to_desktop(Rect::new(300.0, 230.0, 420.0, 200.0), native::DESKTOP_UNIT)
            );
            let to = h.expand_to.expect("expand");
            let local = screen().from_desktop(to, native::DESKTOP_UNIT);
            assert!((local.width - 640.0).abs() < 1e-6);
            assert!((local.height - 300.0).abs() < 1e-6);
        }
        assert!(reg.begin_ready("1", 300.0).is_none(), "只交接一次");
        assert!(reg.begin_ready("nope", 300.0).is_none());
        assert_eq!(reg.shown_ids(), vec!["1".to_owned()]);
        reg.remove("1");
        assert!(reg.state("1").is_none());
    }

    /// 普通固定没有展开宽度；面板传来的 expand_width / on_top 一律被 Rust 的值覆盖
    #[test]
    fn plain_pin_has_no_expand_width_and_ignores_frontend_values() {
        let reg = PinnedRegistry::new();
        let mut state = pin_state();
        state.expand_width = Some(999);
        state.on_top = Some(true);
        reg.entries.lock().insert(
            "2".to_owned(),
            Entry {
                state,
                expand: false,
                screen: Screen {
                    work: Rect::new(0.0, 25.0, 500.0, 805.0),
                    ..screen()
                },
                from: Rect::new(10.0, 40.0, 420.0, 200.0),
                session: None,
                handed_off: false,
                on_top: false,
            },
        );
        let st = reg.state("2").expect("state");
        assert_eq!(st.expand_width, None);
        assert_eq!(st.on_top, Some(false));
        // 窄工作区：展开宽度按工作区收窄（与 placement::pinned_expand_rect 一致）
        let narrow = Screen {
            work: Rect::new(0.0, 25.0, 500.0, 805.0),
            ..screen()
        };
        assert_eq!(
            f64::from(expand_width_px(&narrow)),
            placement::pinned_expand_rect(&narrow, 300.0).width
        );
    }
}
