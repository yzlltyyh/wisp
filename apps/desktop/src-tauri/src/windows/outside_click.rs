//! 面板可见期间的「关闭监听」：外部点击、鼠标离开 N px、全局 Esc、失焦。
//!
//! - **外部点击**（翻译 / OCR 视图）：面板可见期间经 `platform.clicks.watch()` 安装全局鼠标按下 / 抬起监听（不需要权限），
//!   **按下后抬起、两次都在卡片外、且位移小于 4 px** 才算一次点击，触发 `hide_request(click_outside)`；按下后拖动
//!   ≥ 4 px（用户在原应用里拖选文字）不关闭面板；
//! - **鼠标离开 N px**（可选，`translate.close_on_mouse_leave`）：只在面板可见期间以 30 Hz 轮询光标位置；光标先进入卡片
//!   N px 范围内才「上膛」（面板出现时光标可能本来就离得很远），之后离开超过 N px 触发 `hide_request(mouse_leave)`；
//!   用户拖动面板期间暂停；
//! - **全局 Esc**：翻译 / OCR 视图**不是 key** 的期间经 `HotkeyService::esc_guard(true)` 临时注册，成为 key 后 Esc 由 WebView
//!   自己处理、立即注销；
//! - **失焦**（输入 / 剪贴板视图）：失去 key 时 `hide_request(blur)`（见 panel.rs 的焦点事件处理）。
//!
//! 隐藏时全部注销（HookHandle 被 drop、轮询任务停止、esc_guard(false)），隐藏后进程里没有残留的钩子与 Esc 热键。
//! 判定逻辑是纯函数（[`ClickClassifier`]、[`LeaveTracker`]），带单测；平台回调里只做 O(1) 的比较。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tauri::Manager;
use wisp_platform::{ClickEvent, ClickKind, HookHandle, PointerButton};
use wisp_types::{HideReason, MonitorId, Point, Rect};

use super::placement::distance_to_rect;
use crate::state::AppState;

/// 按下与抬起之间的最大位移（逻辑 px），超过即视为拖动
pub const CLICK_SLOP: f64 = 4.0;
/// 「鼠标离开 N px」的轮询间隔（30 Hz）
pub const MOUSE_LEAVE_POLL: Duration = Duration::from_millis(33);

/// 一次显示期间要装的监听（由视图与设置决定，见 `WindowManager`）
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct GuardSpec {
    /// 外部点击关闭（翻译 / OCR，`translate.close_on_click_outside`）
    pub outside_click: bool,
    /// 非 key 期间注册全局 Esc（翻译 / OCR）
    pub esc: bool,
    /// 鼠标离开 N px 关闭（翻译，`translate.close_on_mouse_leave`）
    pub mouse_leave_px: Option<f64>,
    /// 失去 key 时关闭（输入 / 剪贴板）
    pub blur_hides: bool,
}

/// 卡片所在显示器与卡片矩形（本屏逻辑坐标，与 `ClickEvent.at` 同一坐标系）
pub type CardHit = (MonitorId, Rect);

/// 外部点击判定（纯逻辑）
#[derive(Debug, Default, Clone)]
pub struct ClickClassifier {
    down: Option<(MonitorId, Point, PointerButton)>,
}

impl ClickClassifier {
    /// 喂入一次按下 / 抬起；返回 true 表示刚完成了一次「卡片外的点击」
    pub fn feed(&mut self, ev: &ClickEvent, card: &CardHit) -> bool {
        let outside = |at: &(MonitorId, Point)| at.0 != card.0 || !card.1.contains(at.1);
        match ev.kind {
            ClickKind::Down => {
                self.down = outside(&ev.at).then(|| (ev.at.0.clone(), ev.at.1, ev.button));
                false
            }
            ClickKind::Up => {
                let Some((monitor, p, button)) = self.down.take() else {
                    return false;
                };
                button == ev.button
                    && monitor == ev.at.0
                    && outside(&ev.at)
                    && (p.x - ev.at.1.x).hypot(p.y - ev.at.1.y) < CLICK_SLOP
            }
        }
    }
}

/// 鼠标离开判定（纯逻辑）
#[derive(Debug, Clone, Copy)]
pub struct LeaveTracker {
    px: f64,
    armed: bool,
}

impl LeaveTracker {
    /// 离开距离阈值 `px`（逻辑 px）
    pub fn new(px: f64) -> Self {
        Self { px, armed: false }
    }

    /// 喂入一次光标位置；返回 true 表示应当关闭
    pub fn feed(&mut self, cursor: &(MonitorId, Point), card: &CardHit) -> bool {
        let distance = if cursor.0 == card.0 {
            distance_to_rect(card.1, cursor.1)
        } else {
            f64::INFINITY
        };
        if distance <= self.px {
            self.armed = true;
            false
        } else {
            self.armed
        }
    }
}

/// 关闭请求的回调：`(session_id, reason)`
pub type HideSink = Arc<dyn Fn(u64, HideReason) + Send + Sync>;

/// 「鼠标离开」轮询一次的结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeaveStep {
    /// 监听已注销：轮询结束
    Disarmed,
    /// 不关闭（还没离开，或正在拖动）
    Stay,
    /// 离开了：关闭这个会话
    Leave(u64),
}

#[derive(Default)]
struct Inner {
    session_id: u64,
    card: Option<CardHit>,
    classifier: ClickClassifier,
    click: Option<HookHandle>,
    leave_stop: Option<Arc<AtomicBool>>,
    /// 当前是否已向 HotkeyService 注册全局 Esc
    esc_registered: bool,
    /// 本次显示要不要 Esc 守卫
    esc_wanted: bool,
    /// 用户正在拖动面板：「鼠标离开」暂不判定（见 `PanelHost::pause_guards`）
    paused: bool,
}

impl Inner {
    /// 「鼠标离开」轮询的一次判定（纯逻辑，`cursor` 只在要判定时才取）：用户拖动面板期间（`paused`）光标随卡片走，卡片
    /// 矩形的更新总晚一点（拖到另一块屏时整块屏都算离开）——不判定，也不改变上膛状态
    fn leave_step(&self, tracker: &mut LeaveTracker, cursor: impl FnOnce() -> (MonitorId, Point)) -> LeaveStep {
        let Some(card) = &self.card else {
            return LeaveStep::Disarmed;
        };
        if self.paused {
            return LeaveStep::Stay;
        }
        if tracker.feed(&cursor(), card) {
            LeaveStep::Leave(self.session_id)
        } else {
            LeaveStep::Stay
        }
    }
}

/// 监听集合（真实实现：平台的全局点击监听 + 30 Hz 光标轮询 + 全局 Esc）
pub struct Guards {
    app: tauri::AppHandle,
    inner: Arc<Mutex<Inner>>,
}

impl Guards {
    /// 构造（不安装任何监听）
    pub fn new(app: tauri::AppHandle) -> Self {
        Self {
            app,
            inner: Arc::new(Mutex::new(Inner::default())),
        }
    }

    /// 面板显示后按 `spec` 安装监听；先注销上一次的。`key` 为面板当前是否已是 key
    pub fn arm(&self, session_id: u64, card: CardHit, spec: GuardSpec, key: bool, sink: HideSink) {
        self.disarm();
        let Some(state) = self.app.try_state::<AppState>() else {
            return;
        };
        {
            let mut g = self.inner.lock();
            g.session_id = session_id;
            g.card = Some(card);
            g.classifier = ClickClassifier::default();
            g.esc_wanted = spec.esc;
            g.paused = false;
        }
        if spec.outside_click {
            let inner = Arc::clone(&self.inner);
            let sink = Arc::clone(&sink);
            let watched = state.platform.clicks.watch(Box::new(move |ev| {
                let hit = {
                    let mut g = inner.lock();
                    let Some(card) = g.card.clone() else { return };
                    g.classifier.feed(&ev, &card).then_some(g.session_id)
                };
                if let Some(session) = hit {
                    sink(session, HideReason::ClickOutside);
                }
            }));
            match watched {
                Ok(handle) => self.inner.lock().click = Some(handle),
                Err(e) => log::info!("outside click watch unavailable: {e}"),
            }
        }
        if let Some(px) = spec.mouse_leave_px {
            let stop = Arc::new(AtomicBool::new(false));
            self.inner.lock().leave_stop = Some(Arc::clone(&stop));
            let inner = Arc::clone(&self.inner);
            let platform = Arc::clone(&state.platform);
            tauri::async_runtime::spawn(async move {
                let mut tracker = LeaveTracker::new(px);
                let mut tick = tokio::time::interval(MOUSE_LEAVE_POLL);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tick.tick().await;
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                    match inner.lock().leave_step(&mut tracker, || platform.screen.cursor()) {
                        LeaveStep::Disarmed => return,
                        LeaveStep::Stay => {}
                        LeaveStep::Leave(session) => {
                            sink(session, HideReason::MouseLeave);
                            return;
                        }
                    }
                }
            });
        }
        self.set_key(key);
    }

    /// 卡片矩形变化（高度变化、拖动后）
    pub fn update_card(&self, card: CardHit) {
        let mut g = self.inner.lock();
        if g.card.is_some() {
            g.card = Some(card);
        }
    }

    /// 用户开始 / 结束拖动面板：拖动期间「鼠标离开」不判定
    pub fn set_paused(&self, paused: bool) {
        self.inner.lock().paused = paused;
    }

    /// 面板 key 状态变化：非 key 期间注册全局 Esc，成为 key 后注销
    pub fn set_key(&self, key: bool) {
        let want = {
            let g = self.inner.lock();
            g.esc_wanted && g.card.is_some() && !key
        };
        self.set_esc(want);
    }

    fn set_esc(&self, active: bool) {
        {
            let mut g = self.inner.lock();
            if g.esc_registered == active {
                return;
            }
            g.esc_registered = active;
        }
        if let Some(state) = self.app.try_state::<AppState>() {
            state.hotkeys.esc_guard(active);
        }
    }

    /// 注销全部监听（面板隐藏时）
    pub fn disarm(&self) {
        let (click, stop) = {
            let mut g = self.inner.lock();
            g.card = None;
            g.esc_wanted = false;
            g.paused = false;
            (g.click.take(), g.leave_stop.take())
        };
        if let Some(stop) = stop {
            stop.store(true, Ordering::Release);
        }
        // HookHandle 在锁外 drop（注销可能要等平台线程）
        drop(click);
        self.set_esc(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: ClickKind, monitor: &str, x: f64, y: f64) -> ClickEvent {
        ClickEvent {
            kind,
            at: (monitor.to_owned(), Point { x, y }),
            button: PointerButton::Left,
        }
    }

    fn card() -> CardHit {
        ("1".to_owned(), Rect::new(100.0, 100.0, 400.0, 200.0))
    }

    #[test]
    fn click_outside_counts() {
        let mut c = ClickClassifier::default();
        assert!(!c.feed(&ev(ClickKind::Down, "1", 50.0, 50.0), &card()));
        assert!(c.feed(&ev(ClickKind::Up, "1", 52.0, 51.0), &card()));
        // 抬起没有对应的按下
        assert!(!c.feed(&ev(ClickKind::Up, "1", 52.0, 51.0), &card()));
    }

    #[test]
    fn drag_selection_does_not_close() {
        let mut c = ClickClassifier::default();
        c.feed(&ev(ClickKind::Down, "1", 50.0, 50.0), &card());
        assert!(!c.feed(&ev(ClickKind::Up, "1", 50.0, 54.5), &card()), "≥ 4 px 视为拖选");
        c.feed(&ev(ClickKind::Down, "1", 50.0, 50.0), &card());
        assert!(c.feed(&ev(ClickKind::Up, "1", 52.0, 52.0), &card()), "< 4 px 仍是点击");
    }

    #[test]
    fn clicks_inside_card_or_crossing_do_not_close() {
        let mut c = ClickClassifier::default();
        // 在卡片里按下抬起
        c.feed(&ev(ClickKind::Down, "1", 150.0, 150.0), &card());
        assert!(!c.feed(&ev(ClickKind::Up, "1", 151.0, 150.0), &card()));
        // 卡片外按下、卡片里抬起
        c.feed(&ev(ClickKind::Down, "1", 99.0, 150.0), &card());
        assert!(!c.feed(&ev(ClickKind::Up, "1", 101.0, 150.0), &card()));
        // 另一块屏上的点击算外部
        c.feed(&ev(ClickKind::Down, "2", 150.0, 150.0), &card());
        assert!(c.feed(&ev(ClickKind::Up, "2", 150.0, 150.0), &card()));
        // 跨屏按下抬起视为拖动
        c.feed(&ev(ClickKind::Down, "2", 10.0, 10.0), &card());
        assert!(!c.feed(&ev(ClickKind::Up, "1", 10.0, 10.0), &card()));
        // 按键不一致
        c.feed(&ev(ClickKind::Down, "1", 10.0, 10.0), &card());
        let mut up = ev(ClickKind::Up, "1", 10.0, 10.0);
        up.button = PointerButton::Right;
        assert!(!c.feed(&up, &card()));
    }

    #[test]
    fn mouse_leave_arms_then_fires() {
        let mut t = LeaveTracker::new(160.0);
        let far = ("1".to_owned(), Point { x: 900.0, y: 900.0 });
        let near = ("1".to_owned(), Point { x: 520.0, y: 120.0 });
        // 光标一开始就很远：未上膛，不关闭
        assert!(!t.feed(&far, &card()));
        // 靠近卡片后上膛
        assert!(!t.feed(&near, &card()));
        // 再离开超过 N px：关闭
        assert!(t.feed(&far, &card()));
        // 换到另一块屏也算离开
        let mut t = LeaveTracker::new(160.0);
        t.feed(&near, &card());
        assert!(t.feed(&("2".to_owned(), Point { x: 0.0, y: 0.0 }), &card()));
    }

    /// 用户拖动面板期间（`paused`）「鼠标离开」不判定——光标拖到别处、甚至另一块屏都不关闭，也不读光标；恢复之后照常：
    /// 卡片矩形已随拖动更新，光标就在卡片上，离开超过 N px 才关闭。注销后轮询结束
    #[test]
    fn mouse_leave_is_paused_while_dragging() {
        let mut inner = Inner {
            session_id: 7,
            card: Some(card()),
            ..Inner::default()
        };
        let mut t = LeaveTracker::new(160.0);
        let near = ("1".to_owned(), Point { x: 520.0, y: 120.0 });
        let far = ("2".to_owned(), Point { x: 0.0, y: 0.0 });
        assert_eq!(inner.leave_step(&mut t, || near.clone()), LeaveStep::Stay, "上膛");
        inner.paused = true;
        assert_eq!(inner.leave_step(&mut t, || panic!("拖动中不读光标")), LeaveStep::Stay);
        // 拖动结束：卡片矩形先更新到拖到的位置（另一块屏），再恢复判定
        inner.card = Some(("2".to_owned(), Rect::new(0.0, 0.0, 400.0, 200.0)));
        inner.paused = false;
        assert_eq!(
            inner.leave_step(&mut t, || far.clone()),
            LeaveStep::Stay,
            "光标在卡片上"
        );
        assert_eq!(
            inner.leave_step(&mut t, || ("2".to_owned(), Point { x: 900.0, y: 900.0 })),
            LeaveStep::Leave(7)
        );
        inner.card = None;
        assert_eq!(inner.leave_step(&mut t, || far.clone()), LeaveStep::Disarmed);
    }
}
