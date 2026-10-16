//! 扩展触发（`mouse-hook` feature）：浮标判定与消失、双击修饰键、
//! 鼠标额外按键的拦截与长按。钩子线程一侧的状态机（`ExtState`）只做 O(1) 工作，
//! 坐标换算、Wisp 窗口命中与 `is_bobber_candidate` 判定都在分发线程上完成。
//!
//! **每次浮标显示只报告一次消失**：消失信号有多个来源（钩子线程的按键 / 滚轮 / 点击 / 移开、浮标线程
//! 的 4 s 超时、前台追踪线程的应用切换），浮标开始淡出后的 140 ms 内它们仍可能先后到达。hub 为当前显示记一个
//! 「尚未消失」的序号（`ExtSubs::live`）：任何来源都要先**认领**这个序号才能派发 `BobberDismiss`，认领之后同一次
//! 显示的其他消失信号一律丢弃；浮标因上层 `hide` 或超时开始淡出时也会立即认领，淡出期间的按键等不会再报告。
//! 钩子线程不能取 hub 的锁（重配时 hub 持锁等待钩子线程的 ack），它发出的消失在分发线程上认领；其他来源在发出时
//! 就认领。
//!
//! **连续粘贴的粘贴键**（`paste_keys`）：键盘钩子里比较虚拟键码与此刻按着的修饰键（`GetAsyncKeyState` 八次查询，
//! O(1)），匹配的按下吞掉（返回非零）并发出 `PasteKey`，同一个键的自动重复与抬起一并吞掉；只放行带 `WISP_TAG` 的
//! 事件（Wisp 自己的重放），软件 KVM 等注入的按键与物理按键一样拦下。桌面切换（锁屏、UAC）时发出 `SessionLocked`。

use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use ::windows::Win32::Foundation::{LPARAM, POINT, WPARAM};
use ::windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
use ::windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, KBDLLHOOKSTRUCT, KillTimer, MSLLHOOKSTRUCT, PostThreadMessageW, SM_CXDOUBLECLK, SM_CYDOUBLECLK,
    SetTimer, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_SYSKEYDOWN, WM_XBUTTONDOWN, WM_XBUTTONUP, XBUTTON1, XBUTTON2,
};
use wisp_types::{ModifierKey, MonitorId, MouseButtonKind, Rect};

use crate::common::combo::Modifiers;
use crate::common::double_tap::{DoubleTapMachine, TapInput};
use crate::common::gesture::{self, LongPress, LongPressUp, MouseUpFacts};
use crate::types::{
    DismissReason, DoubleTapSpec, HookError, HookHandle, HookSpec, InputSignal, KeyChord, MouseSpec, PasteKeys,
};

use super::super::bobber::Bobber;
use super::super::geom;
use super::super::input::{self, ExtraButton};
use super::super::keys::{self, vk};
use super::super::monitor;
use super::super::util::{call_guarded, tick_delta};
use super::{HookConfig, HookHub, HubEvent, WM_APP_REINJECT};

type InputSink = Arc<dyn Fn(InputSignal) + Send + Sync>;

/// mouse-up 之后多久内的 mouse-down 视为「取消正在进行的浮标判定」（80 ms 延迟 + 120 ms 期限，留余量）
const PROBE_WINDOW_MS: u32 = 250;

/// 浮标的当前几何（物理像素），由浮标窗口线程在显示时登记
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BobberGeom {
    /// 每次显示递增，用于判断「是不是同一次显示」
    pub serial: u64,
    pub monitor: MonitorId,
    /// 22 pt 圆形在显示器内的逻辑矩形（`BobberWindow::rect` 与 Wisp 窗口命中判定用；与 macOS 一致，不含
    /// 四周只画投影、点击会穿透的余量）
    pub local: Rect,
    /// 圆心（物理像素）
    pub center: (f64, f64),
    /// 圆形半径（物理像素）
    pub radius: f64,
    /// 消失距离（物理像素，= 80 pt × 缩放）
    pub dismiss: f64,
}

impl BobberGeom {
    fn dist2(&self, p: POINT) -> f64 {
        let dx = f64::from(p.x) - self.center.0;
        let dy = f64::from(p.y) - self.center.1;
        dx * dx + dy * dy
    }

    fn hit(&self, p: POINT) -> bool {
        self.dist2(p) <= (self.radius + 1.0) * (self.radius + 1.0)
    }

    fn too_far(&self, p: POINT) -> bool {
        self.dist2(p) > self.dismiss * self.dismiss
    }
}

/// 扩展触发的合并配置
#[derive(Clone, Default)]
pub(super) struct ExtConfig {
    bobber: bool,
    bobber_visible: Option<BobberGeom>,
    double_tap: Option<DoubleTapSpec>,
    mouse: MouseSpec,
    paste: PasteKeys,
}

impl ExtConfig {
    pub(super) fn wants_mouse(&self) -> bool {
        self.bobber
            || self.bobber_visible.is_some()
            || self.mouse.middle.is_some()
            || self.mouse.back.is_some()
            || self.mouse.forward.is_some()
    }

    pub(super) fn wants_keyboard(&self) -> bool {
        self.double_tap.is_some() || self.bobber_visible.is_some() || !self.paste.is_empty()
    }

    /// 连续粘贴进行中（桌面切换时要通知它结束）
    pub(super) fn pasting(&self) -> bool {
        !self.paste.is_empty()
    }
}

/// 扩展触发的订阅
#[derive(Default)]
pub(super) struct ExtSubs {
    subs: Vec<(u64, HookSpec, InputSink)>,
    pub(super) bobber_visible: Option<BobberGeom>,
    /// 当前显示中、还没有报告过消失的那次浮标（`BobberGeom.serial`）
    live: Option<u64>,
}

impl ExtSubs {
    /// 登记浮标的显示 / 隐藏
    fn set_visible(&mut self, geom: Option<BobberGeom>) {
        self.live = geom.as_ref().map(|g| g.serial);
        self.bobber_visible = geom;
    }

    /// 认领某次显示的消失：只有它仍是当前显示、且还没有被认领过时返回 true
    pub(super) fn claim(&mut self, serial: u64) -> bool {
        if self.live == Some(serial) {
            self.live = None;
            true
        } else {
            false
        }
    }

    /// 认领当前显示（来源不知道序号时，例如前台切换）
    pub(super) fn claim_current(&mut self) -> Option<u64> {
        self.live.take()
    }

    /// 订阅的并集：浮标任一订阅开启即开启；双击修饰键与鼠标键取第一个给出参数的订阅
    pub(super) fn config(&self) -> ExtConfig {
        let mut mouse = MouseSpec::default();
        let mut long_press_set = false;
        for (_, spec, _) in &self.subs {
            if let Some(m) = spec.mouse {
                mouse.middle = mouse.middle.or(m.middle);
                mouse.back = mouse.back.or(m.back);
                mouse.forward = mouse.forward.or(m.forward);
                if !long_press_set {
                    mouse.long_press = m.long_press;
                    long_press_set = true;
                }
            }
        }
        ExtConfig {
            bobber: self.subs.iter().any(|(_, s, _)| s.bobber),
            bobber_visible: self.bobber_visible.clone(),
            double_tap: self.subs.iter().find_map(|(_, s, _)| s.double_tap),
            mouse,
            paste: self
                .subs
                .iter()
                .map(|(_, s, _)| s.paste_keys)
                .find(|p| !p.is_empty())
                .unwrap_or_default(),
        }
    }
}

/// 一次消失信号
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Dismissal {
    pub(super) reason: DismissReason,
    /// 针对的那次浮标显示（`BobberGeom.serial`）；None 表示当时没有可见浮标，只是取消正在进行的浮标判定
    pub(super) serial: Option<u64>,
    /// 发出前已经认领（见模块文档）；钩子线程发出的为 false，在分发线程上认领
    pub(super) claimed: bool,
}

impl Dismissal {
    /// 钩子线程发出的消失（在分发线程上认领）
    fn from_hook(reason: DismissReason, serial: Option<u64>) -> Self {
        Self {
            reason,
            serial,
            claimed: false,
        }
    }
}

/// 钩子线程 → 分发线程的扩展事件
pub(super) enum ExtEvent {
    LeftUp {
        down: (i32, i32),
        up: (i32, i32),
        clicks: u32,
        ctrl: bool,
    },
    Dismiss(Dismissal),
    DoubleTap,
    Mouse(MouseButtonKind),
    /// 拦下了一组粘贴键
    PasteKey(KeyChord),
    /// 桌面切换（锁屏、UAC）：连续粘贴结束
    SessionLocked,
}

/// 一组粘贴键：虚拟键码 + 修饰键（配置时由 `KeyChord` 换算好）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PasteVk {
    vk: u16,
    chord: KeyChord,
}

/// 此刻按着的修饰键（左右同义）
fn held_modifier_set() -> Modifiers {
    let held = input::held_modifiers();
    let any = |a: u16, b: u16| held.iter().any(|v| *v == a || *v == b);
    Modifiers {
        control: any(vk::CTRL_L, vk::CTRL_R),
        alt: any(vk::ALT_L, vk::ALT_R),
        shift: any(vk::SHIFT_L, vk::SHIFT_R),
        super_key: any(vk::WIN_L, vk::WIN_R),
    }
}

fn button_index(b: MouseButtonKind) -> usize {
    match b {
        MouseButtonKind::Middle => 0,
        MouseButtonKind::Back => 1,
        MouseButtonKind::Forward => 2,
    }
}

/// 双击修饰键的目标虚拟键（Windows 上 ⌘ 对应 Win 键）
fn modifier_vk(k: ModifierKey) -> u16 {
    match k {
        ModifierKey::LeftAlt => vk::ALT_L,
        ModifierKey::RightAlt => vk::ALT_R,
        ModifierKey::LeftCtrl => vk::CTRL_L,
        ModifierKey::RightCtrl => vk::CTRL_R,
        ModifierKey::LeftCmd => vk::WIN_L,
        ModifierKey::RightCmd => vk::WIN_R,
    }
}

struct TapState {
    spec: DoubleTapSpec,
    machine: DoubleTapMachine,
    target: u16,
    target_down: bool,
    /// 其他按着的修饰键（位图，按 MODIFIER_VKS 的下标）
    others: u8,
}

impl TapState {
    fn new(spec: DoubleTapSpec) -> Self {
        Self {
            spec,
            machine: DoubleTapMachine::new(spec.interval),
            target: modifier_vk(spec.key),
            target_down: false,
            others: 0,
        }
    }

    /// 喂入一次按键；返回是否触发
    fn on_key(&mut self, v: u16, down: bool, mouse_hooked: bool) -> bool {
        if v == self.target {
            if down {
                // 按住时的自动重复只算一次
                if !self.target_down {
                    self.target_down = true;
                    if self.others != 0 {
                        self.machine.feed(TapInput::OtherKey);
                    } else {
                        // 没有鼠标钩子时，用「此刻是否按着鼠标键」近似判断中间有没有鼠标按下
                        if !mouse_hooked && input::any_mouse_button_held() {
                            self.machine.feed(TapInput::MouseDown);
                        }
                        self.machine.feed(TapInput::TargetDown(Instant::now()));
                    }
                }
                false
            } else {
                self.target_down = false;
                self.machine.feed(TapInput::TargetUp(Instant::now()))
            }
        } else if let Some(i) = keys::MODIFIER_VKS.iter().position(|(k, _)| *k == v) {
            let bit = 1u8 << i;
            if down {
                self.others |= bit;
                self.machine.feed(TapInput::OtherKey);
            } else {
                self.others &= !bit;
            }
            false
        } else {
            if down {
                self.machine.feed(TapInput::OtherKey);
            }
            false
        }
    }
}

/// 长按模式下被吞掉、尚未抬起的按键
#[derive(Clone, Copy)]
struct Pending {
    button: MouseButtonKind,
    timer: usize,
    press: LongPress,
    /// 按下时的位置（物理像素）：补发的单击落在这里
    at: POINT,
}

/// 等待在消息循环里补发的单击（不在钩子回调中注入）
#[derive(Clone, Copy)]
struct Replay {
    button: MouseButtonKind,
    down: POINT,
    up: POINT,
}

/// 毫秒（饱和到 u32，SetTimer 用）
fn timer_ms(d: Duration) -> u32 {
    u32::try_from(d.as_millis()).unwrap_or(u32::MAX).max(1)
}

/// 钩子线程上的扩展触发状态
pub(super) struct ExtState {
    uia: Arc<super::super::uia::Uia>,
    bobber: bool,
    mouse: MouseSpec,
    dbl_time: u32,
    dbl_cx: i32,
    dbl_cy: i32,
    last_down: Option<(POINT, u32)>,
    clicks: u32,
    down_at: Option<POINT>,
    last_up_tick: Option<u32>,
    visible: Option<BobberGeom>,
    dismissed: bool,
    tap: Option<TapState>,
    swallow: [bool; 3],
    pending: Option<Pending>,
    replay: Option<Replay>,
    /// 连续粘贴的粘贴键（空 = 没有在进行的队列）
    paste: Vec<PasteVk>,
    /// 拦下的那一次按下的虚拟键：它的自动重复与抬起一并吞掉
    paste_held: Option<u16>,
}

impl ExtState {
    pub(super) fn new(uia: Arc<super::super::uia::Uia>) -> Self {
        Self {
            uia,
            bobber: false,
            mouse: MouseSpec::default(),
            dbl_time: 500,
            dbl_cx: 4,
            dbl_cy: 4,
            last_down: None,
            clicks: 0,
            down_at: None,
            last_up_tick: None,
            visible: None,
            dismissed: false,
            tap: None,
            swallow: [false; 3],
            pending: None,
            replay: None,
            paste: Vec::new(),
            paste_held: None,
        }
    }

    pub(super) fn bobber_visible(&self) -> bool {
        self.visible.is_some()
    }

    pub(super) fn reconfigure(&mut self, cfg: &ExtConfig) {
        self.bobber = cfg.bobber;
        self.mouse = cfg.mouse;
        self.paste = cfg
            .paste
            .iter()
            .filter_map(|chord| chord.to_combo().win_vk().map(|vk| PasteVk { vk, chord }))
            .collect();
        if self.paste.is_empty() {
            self.paste_held = None;
        }
        if self.visible.as_ref().map(|g| g.serial) != cfg.bobber_visible.as_ref().map(|g| g.serial) {
            self.dismissed = false;
        }
        self.visible = cfg.bobber_visible.clone();
        // SAFETY: 纯查询（用户可能在系统设置里改了双击速度）
        unsafe {
            self.dbl_time = GetDoubleClickTime();
            self.dbl_cx = GetSystemMetrics(SM_CXDOUBLECLK).max(1);
            self.dbl_cy = GetSystemMetrics(SM_CYDOUBLECLK).max(1);
        }
        match (cfg.double_tap, self.tap.as_ref().map(|t| t.spec)) {
            (Some(spec), Some(cur)) if spec == cur => {}
            (Some(spec), _) => self.tap = Some(TapState::new(spec)),
            (None, _) => self.tap = None,
        }
        if !self.bobber {
            self.down_at = None;
            self.last_up_tick = None;
        }
        // 某个键的绑定被取消时，丢弃它悬而未决的长按
        if let Some(p) = self.pending
            && gesture::mouse_action(&self.mouse, p.button).is_none()
        {
            self.cancel_pending();
        }
    }

    fn cancel_pending(&mut self) {
        if let Some(p) = self.pending.take() {
            // SAFETY: 线程定时器，属于本线程
            unsafe {
                let _ = KillTimer(None, p.timer);
            }
        }
    }

    pub(super) fn shutdown(&mut self) {
        self.cancel_pending();
    }

    /// 鼠标事件；返回 (是否吞掉, 要送出的事件)
    pub(super) fn on_mouse(&mut self, msg: u32, info: &MSLLHOOKSTRUCT, tid: u32) -> (bool, [Option<ExtEvent>; 2]) {
        let mut ev: [Option<ExtEvent>; 2] = [None, None];
        let pt = info.pt;
        let is_down = matches!(msg, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN);
        let on_bobber = self.visible.as_ref().is_some_and(|g| g.hit(pt));
        let serial = self.visible.as_ref().map(|g| g.serial);
        if is_down {
            if let Some(t) = self.tap.as_mut() {
                t.machine.feed(TapInput::MouseDown);
            }
            let visible_live = self.visible.is_some() && !self.dismissed;
            let probe_pending = self
                .last_up_tick
                .is_some_and(|t| tick_delta(info.time, t) <= PROBE_WINDOW_MS);
            if !on_bobber && (visible_live || probe_pending) {
                if visible_live {
                    self.dismissed = true;
                }
                self.last_up_tick = None;
                let target = if visible_live { serial } else { None };
                ev[0] = Some(ExtEvent::Dismiss(Dismissal::from_hook(
                    DismissReason::MouseDown,
                    target,
                )));
            }
        }
        match msg {
            WM_MOUSEMOVE => {
                if let Some(g) = self.visible.as_ref()
                    && !self.dismissed
                    && !g.hit(pt)
                    && g.too_far(pt)
                {
                    self.dismissed = true;
                    ev[0] = Some(ExtEvent::Dismiss(Dismissal::from_hook(
                        DismissReason::MovedAway,
                        serial,
                    )));
                }
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                if self.visible.is_some() && !self.dismissed {
                    self.dismissed = true;
                    ev[0] = Some(ExtEvent::Dismiss(Dismissal::from_hook(DismissReason::Scroll, serial)));
                }
            }
            WM_LBUTTONDOWN if self.bobber => {
                let repeat = self.last_down.is_some_and(|(p, t)| {
                    tick_delta(info.time, t) <= self.dbl_time
                        && (pt.x - p.x).abs() <= self.dbl_cx / 2
                        && (pt.y - p.y).abs() <= self.dbl_cy / 2
                });
                self.clicks = if repeat && self.clicks < 3 { self.clicks + 1 } else { 1 };
                self.last_down = Some((pt, info.time));
                self.down_at = Some(pt);
            }
            WM_LBUTTONUP if self.bobber => {
                if let Some(d) = self.down_at.take()
                    && !on_bobber
                {
                    ev[1] = Some(ExtEvent::LeftUp {
                        down: (d.x, d.y),
                        up: (pt.x, pt.y),
                        clicks: self.clicks,
                        ctrl: input::ctrl_held(),
                    });
                    self.last_up_tick = Some(info.time);
                }
            }
            WM_MBUTTONDOWN | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
                let (swallow, e) = self.on_extra_button(msg, info, tid);
                if e.is_some() {
                    ev[1] = e;
                }
                return (swallow, ev);
            }
            _ => {}
        }
        (false, ev)
    }

    /// 中键 / 侧键的拦截；拦截时按下与抬起都吞掉
    fn on_extra_button(&mut self, msg: u32, info: &MSLLHOOKSTRUCT, tid: u32) -> (bool, Option<ExtEvent>) {
        let (button, down) = match msg {
            WM_MBUTTONDOWN => (MouseButtonKind::Middle, true),
            WM_MBUTTONUP => (MouseButtonKind::Middle, false),
            WM_XBUTTONDOWN | WM_XBUTTONUP => {
                let b = match (info.mouseData >> 16) as u16 {
                    XBUTTON1 => MouseButtonKind::Back,
                    XBUTTON2 => MouseButtonKind::Forward,
                    _ => return (false, None),
                };
                (b, msg == WM_XBUTTONDOWN)
            }
            _ => return (false, None),
        };
        let idx = button_index(button);
        if !down {
            if !self.swallow[idx] {
                return (false, None);
            }
            self.swallow[idx] = false;
            if let Some(mut p) = self.pending
                && p.button == button
            {
                self.pending = None;
                // SAFETY: 线程定时器，属于本线程
                unsafe {
                    let _ = KillTimer(None, p.timer);
                }
                match p.press.up(Instant::now()) {
                    // 400 ms 内抬起：在按下的位置补发原始单击（带 Wisp 标记）。在回调返回后的消息循环里发送，
                    // 避免在钩子回调中注入
                    LongPressUp::Replay => {
                        self.replay = Some(Replay {
                            button,
                            down: p.at,
                            up: info.pt,
                        });
                        // SAFETY: 投递给本线程
                        unsafe {
                            let _ = PostThreadMessageW(tid, WM_APP_REINJECT, WPARAM(0), LPARAM(0));
                        }
                    }
                    // 定时器还没来得及触发就已超过阈值：在抬起时触发
                    LongPressUp::FireAndSwallow => return (true, Some(ExtEvent::Mouse(button))),
                    LongPressUp::Swallow | LongPressUp::PassThrough => {}
                }
            }
            return (true, None);
        }
        if gesture::mouse_action(&self.mouse, button).is_none() || self.pending.is_some() || self.swallow[idx] {
            return (false, None);
        }
        // 中键只在「选区非空且光标不在链接上」时拦截；判定超时一律放行
        if button == MouseButtonKind::Middle
            && self.uia.middle_probe(Instant::now() + gesture::MIDDLE_CLICK_BUDGET) != Some(true)
        {
            return (false, None);
        }
        self.swallow[idx] = true;
        if self.mouse.long_press {
            let mut press = LongPress::new();
            press.down(Instant::now());
            // SAFETY: 线程定时器（hwnd 为空），WM_TIMER 投递到本线程的消息队列
            let timer = unsafe { SetTimer(None, 0, timer_ms(gesture::LONG_PRESS), None) };
            if timer != 0 {
                self.pending = Some(Pending {
                    button,
                    timer,
                    press,
                    at: info.pt,
                });
                return (true, None);
            }
        }
        (true, Some(ExtEvent::Mouse(button)))
    }

    /// 线程定时器到期（长按达到 400 ms）
    pub(super) fn on_timer(&mut self, id: usize) -> Option<ExtEvent> {
        // SAFETY: 线程定时器；无论是否是当前的长按都注销，避免周期性触发
        unsafe {
            let _ = KillTimer(None, id);
        }
        let now = Instant::now();
        let p = self.pending.as_mut().filter(|p| p.timer == id)?;
        if p.press.tick(now) {
            return Some(ExtEvent::Mouse(p.button));
        }
        // 定时器比阈值早到（计时精度）：按剩余时间重新布置
        if let Some(rest) = p.press.remaining(now) {
            // SAFETY: 同上
            p.timer = unsafe { SetTimer(None, 0, timer_ms(rest), None) };
        }
        None
    }

    /// 补发长按模式下 400 ms 内抬起的单击（落在按下的位置，光标随后回到抬起的位置）
    pub(super) fn reinject(&mut self) {
        let Some(r) = self.replay.take() else { return };
        let extra = match r.button {
            MouseButtonKind::Middle => ExtraButton::Middle,
            MouseButtonKind::Back => ExtraButton::Back,
            MouseButtonKind::Forward => ExtraButton::Forward,
        };
        if let Err(e) = input::reinject_click(extra, (r.down.x, r.down.y), (r.up.x, r.up.y)) {
            log::warn!("re-inject {:?} click failed: {e}", r.button);
        }
    }

    /// 连续粘贴：这个按键是不是要拦下的粘贴键（返回 (是否吞掉, 要送出的事件)）。只有第一次按下发出 `PasteKey`；拦下的
    /// 那个键的自动重复与抬起同样吞掉。Wisp 自己的重放（`WISP_TAG`）由调用方事先放行
    pub(super) fn on_paste_key(&mut self, msg: u32, kb: &KBDLLHOOKSTRUCT) -> (bool, Option<ExtEvent>) {
        if self.paste.is_empty() {
            return (false, None);
        }
        let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let v = kb.vkCode as u16;
        if self.paste_held == Some(v) {
            if !down {
                self.paste_held = None;
            }
            return (true, None);
        }
        if !down || !self.paste.iter().any(|p| p.vk == v) {
            return (false, None);
        }
        let mods = held_modifier_set();
        let Some(hit) = self.paste.iter().find(|p| p.vk == v && p.chord.modifiers == mods) else {
            return (false, None);
        };
        self.paste_held = Some(v);
        (true, Some(ExtEvent::PasteKey(hit.chord)))
    }

    /// 键盘事件（扩展触发从不吞键；吞掉粘贴键由 [`Self::on_paste_key`] 负责）
    pub(super) fn on_key(&mut self, msg: u32, kb: &KBDLLHOOKSTRUCT, mouse_hooked: bool) -> [Option<ExtEvent>; 2] {
        let mut ev: [Option<ExtEvent>; 2] = [None, None];
        let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let v = kb.vkCode as u16;
        // AltGr 会先合成一个伪左 Ctrl（扫描码带 0x200 位，即 0x21D），忽略它
        if v == vk::CTRL_L && kb.scanCode & 0x200 != 0 {
            return ev;
        }
        if down
            && let Some(g) = self.visible.as_ref()
            && !self.dismissed
        {
            self.dismissed = true;
            ev[0] = Some(ExtEvent::Dismiss(Dismissal::from_hook(
                DismissReason::KeyPress,
                Some(g.serial),
            )));
        }
        if let Some(t) = self.tap.as_mut()
            && t.on_key(v, down, mouse_hooked)
        {
            ev[1] = Some(ExtEvent::DoubleTap);
        }
        ev
    }
}

// ---------------------------------------------------------------- 总线一侧

impl HookHub {
    /// 绑定浮标（构造 Platform 时调用一次）
    pub(crate) fn bind_bobber(&self, b: Weak<Bobber>) {
        let _ = self.bobber.set(b);
    }

    fn bobber(&self) -> Option<Arc<Bobber>> {
        self.bobber.get().and_then(Weak::upgrade)
    }

    /// `InputHooks::install`
    pub(crate) fn install(
        self: &Arc<Self>,
        spec: HookSpec,
        sink: Box<dyn Fn(InputSignal) + Send + Sync>,
    ) -> Result<HookHandle, HookError> {
        if spec.is_empty() {
            return Ok(HookHandle::noop());
        }
        let id = {
            let mut inner = self.inner.lock();
            let id = inner.next_id;
            inner.next_id += 1;
            inner.ext.subs.push((id, spec, Arc::from(sink)));
            if let Err(e) = self.reconcile(&mut inner) {
                inner.ext.subs.retain(|(i, _, _)| *i != id);
                let _ = self.reconcile(&mut inner);
                return Err(HookError::Other(e));
            }
            id
        };
        let me = Arc::clone(self);
        Ok(HookHandle::new(move || {
            let mut inner = me.inner.lock();
            inner.ext.subs.retain(|(i, _, _)| *i != id);
            if let Err(e) = me.reconcile(&mut inner) {
                log::warn!("hook reconcile after uninstall: {e}");
            }
        }))
    }

    /// 浮标是否被任一订阅启用
    pub(crate) fn bobber_enabled(&self) -> bool {
        self.inner.lock().ext.subs.iter().any(|(_, s, _)| s.bobber)
    }

    /// 浮标显示 / 隐藏时登记几何（键盘钩子与移动距离判定随之开关）；显示时这次显示成为「尚未消失」的那一次
    pub(crate) fn set_bobber_visible(self: &Arc<Self>, geom: Option<BobberGeom>) {
        let mut inner = self.inner.lock();
        if inner.ext.bobber_visible == geom {
            return;
        }
        inner.ext.set_visible(geom);
        if let Err(e) = self.reconcile(&mut inner) {
            log::warn!("hook reconcile for bobber visibility: {e}");
        }
    }

    /// 浮标开始淡出（上层 `hide`、超时、功能关闭）：这次显示不再报告消失。若消失已经由别的来源认领过，这里什么也
    /// 不做
    pub(crate) fn bobber_hiding(&self, serial: u64) {
        self.inner.lock().ext.claim(serial);
    }

    /// 浮标当前矩形
    pub(crate) fn bobber_rect(&self) -> Option<(MonitorId, Rect)> {
        self.inner
            .lock()
            .ext
            .bobber_visible
            .as_ref()
            .map(|g| (g.monitor.clone(), g.local))
    }

    /// 浮标 4 s 无交互（浮标线程在开始淡出之前调用）：认领成功才报告
    pub(crate) fn bobber_timed_out(&self, serial: u64) {
        let claimed = self.inner.lock().ext.claim(serial);
        if claimed {
            self.emit(HubEvent::Ext(ExtEvent::Dismiss(Dismissal {
                reason: DismissReason::Timeout,
                serial: Some(serial),
                claimed: true,
            })));
        }
    }

    /// reconcile 之后：预热 UIA（中键 / 浮标判定有严格时限）；浮标被关闭时销毁浮标窗口
    pub(super) fn after_config(&self, cfg: &HookConfig) {
        if cfg.ext.bobber || cfg.ext.mouse.middle.is_some() {
            self.uia.warm();
        }
        if !cfg.ext.bobber
            && cfg.ext.bobber_visible.is_none()
            && let Some(b) = self.bobber()
        {
            b.release();
        }
    }

    fn sinks(&self, pick: impl Fn(&HookSpec) -> bool) -> Vec<(HookSpec, InputSink)> {
        self.inner
            .lock()
            .ext
            .subs
            .iter()
            .filter(|(_, s, _)| pick(s))
            .map(|(_, s, k)| (*s, Arc::clone(k)))
            .collect()
    }

    pub(super) fn dispatch_ext(&self, e: ExtEvent) {
        match e {
            ExtEvent::LeftUp { down, up, clicks, ctrl } => self.on_left_up(down, up, clicks, ctrl),
            ExtEvent::Dismiss(d) => {
                match d.serial {
                    Some(serial) => {
                        // 同一次显示只派发一次（见模块文档）
                        if !d.claimed && !self.inner.lock().ext.claim(serial) {
                            return;
                        }
                        // 只隐藏这一次显示：上层可能已经在新位置显示了下一次
                        if let Some(b) = self.bobber() {
                            b.hide_shown(serial, true);
                        }
                    }
                    // 取消正在进行的浮标判定：浮标可能恰好正在出现（钩子线程还没收到它的几何），一并隐藏；它开始淡出
                    // 时会认领自己，之后不会再报告消失
                    None => {
                        if let Some(b) = self.bobber() {
                            b.hide_now(true);
                        }
                    }
                }
                for (_, sink) in self.sinks(|s| s.bobber) {
                    call_guarded("input hook callback", || sink(InputSignal::BobberDismiss(d.reason)));
                }
            }
            ExtEvent::DoubleTap => {
                for (_, sink) in self.sinks(|s| s.double_tap.is_some()) {
                    call_guarded("input hook callback", || sink(InputSignal::DoubleTap));
                }
            }
            ExtEvent::PasteKey(chord) => {
                for (_, sink) in self.sinks(|s| !s.paste_keys.is_empty()) {
                    call_guarded("input hook callback", || sink(InputSignal::PasteKey { chord }));
                }
            }
            ExtEvent::SessionLocked => {
                for (_, sink) in self.sinks(|s| !s.paste_keys.is_empty()) {
                    call_guarded("input hook callback", || sink(InputSignal::SessionLocked));
                }
            }
            ExtEvent::Mouse(button) => {
                for (spec, sink) in self.sinks(|s| s.mouse.is_some()) {
                    if let Some(action) = spec.mouse.and_then(|m| gesture::mouse_action(&m, button)) {
                        call_guarded("input hook callback", || {
                            sink(InputSignal::MouseAction { button, action })
                        });
                    }
                }
            }
        }
    }

    /// 浮标判定的同步部分（Wisp 窗口、⌘ / Ctrl、位移与连击）；通过后由上层延迟 80 ms 读选区长度
    fn on_left_up(&self, down: (i32, i32), up: (i32, i32), clicks: u32, ctrl: bool) {
        let sinks = self.sinks(|s| s.bobber);
        if sinks.is_empty() {
            return;
        }
        let Some(m) = monitor::monitor_nearest(up.0, up.1) else {
            return;
        };
        let scale = geom::sane_scale(m.scale_factor);
        let dx = f64::from(up.0 - down.0);
        let dy = f64::from(up.1 - down.1);
        let drag_px = (dx * dx + dy * dy).sqrt() / scale;
        let local = geom::phys_to_local(&m, f64::from(up.0), f64::from(up.1));
        let in_bobber = self
            .bobber_rect()
            .is_some_and(|(id, r)| id == m.id && r.contains(local));
        let in_own_window = in_bobber || (self.self_hit)(&m.id, local);
        let facts = MouseUpFacts {
            drag_px,
            click_count: clicks,
            command_or_control: ctrl,
            in_own_window,
        };
        if !gesture::is_bobber_candidate(&facts) {
            return;
        }
        for (_, sink) in sinks {
            call_guarded("input hook callback", || {
                sink(InputSignal::BobberCandidate {
                    at: (m.id.clone(), local),
                })
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use ::windows::Win32::UI::WindowsAndMessaging::KBDLLHOOKSTRUCT_FLAGS;

    use super::super::super::uia::Uia;
    use super::*;

    fn geom(serial: u64) -> BobberGeom {
        BobberGeom {
            serial,
            monitor: r"\\.\DISPLAY1".into(),
            local: Rect::default(),
            center: (100.0, 100.0),
            radius: 11.0,
            dismiss: 80.0,
        }
    }

    fn key(vk: u32) -> KBDLLHOOKSTRUCT {
        KBDLLHOOKSTRUCT {
            vkCode: vk,
            scanCode: 0x1E,
            flags: KBDLLHOOKSTRUCT_FLAGS(0),
            time: 0,
            dwExtraInfo: 0,
        }
    }

    /// 钩子线程一侧的状态机：浮标可见时按键发出带这次显示序号、尚未认领的消失信号
    fn hook_side(uia: &Arc<Uia>, g: &BobberGeom) -> ExtState {
        let mut st = ExtState::new(Arc::clone(uia));
        st.reconfigure(&ExtConfig {
            bobber: true,
            bobber_visible: Some(g.clone()),
            ..ExtConfig::default()
        });
        st
    }

    #[test]
    fn each_showing_is_claimed_once() {
        let mut subs = ExtSubs::default();
        subs.set_visible(Some(geom(1)));
        assert!(subs.claim(1));
        assert!(!subs.claim(1), "同一次显示只认领一次");
        assert_eq!(subs.claim_current(), None);
        subs.set_visible(Some(geom(2)));
        assert!(!subs.claim(1), "上一次显示迟到的消失信号不认领");
        assert_eq!(subs.claim_current(), Some(2));
        assert!(!subs.claim(2));
        subs.set_visible(Some(geom(3)));
        subs.set_visible(None);
        assert!(!subs.claim(3), "隐藏之后不再报告");
    }

    #[test]
    fn hook_side_tags_dismissals_with_the_showing() {
        let uia = Arc::new(Uia::new());
        let mut st = hook_side(&uia, &geom(7));
        let [d, _] = st.on_key(WM_KEYDOWN, &key(0x41), true);
        assert!(matches!(
            d,
            Some(ExtEvent::Dismiss(Dismissal {
                reason: DismissReason::KeyPress,
                serial: Some(7),
                claimed: false,
            }))
        ));
        let [d, _] = st.on_key(WM_KEYDOWN, &key(0x42), true);
        assert!(d.is_none(), "钩子线程对同一次显示也只发一次");
    }

    /// 端到端（真实的钩子线程、分发线程与浮标窗口）：同一次浮标只报告一次消失；淡出期间的按键、应用切换不会再报告
    /// 一次。注入的按键会被钩子过滤，这里用钩子线程的状态机产出与真实按键相同的事件，经
    /// 分发线程送达订阅者
    #[test]
    #[ignore = "需要真实桌面会话（安装全局钩子、显示浮标）"]
    fn one_dismissal_per_showing_even_during_fade_out() {
        let uia = Arc::new(Uia::new());
        let hub = HookHub::new(Arc::clone(&uia), Arc::new(|_: &MonitorId, _| false));
        let bobber = Bobber::new(Arc::clone(&hub));
        hub.bind_bobber(Arc::downgrade(&bobber));
        let got = Arc::new(parking_lot::Mutex::new(Vec::<InputSignal>::new()));
        let _handle = {
            let got = Arc::clone(&got);
            hub.install(
                HookSpec {
                    bobber: true,
                    ..HookSpec::default()
                },
                Box::new(move |s| got.lock().push(s)),
            )
            .expect("install")
        };
        let dismissals = || {
            got.lock()
                .iter()
                .filter_map(|s| match s {
                    InputSignal::BobberDismiss(r) => Some(*r),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let wait = |what: &str, f: &dyn Fn() -> bool| {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !f() {
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let mut last_serial = 0;
        let mut show = || {
            let (mon, at) = monitor::cursor();
            bobber.show((mon, at), false, false, Box::new(|| {}));
            wait("bobber to show", &|| {
                hub.inner
                    .lock()
                    .ext
                    .bobber_visible
                    .as_ref()
                    .is_some_and(|g| g.serial > last_serial)
            });
            let g = hub.inner.lock().ext.bobber_visible.clone().expect("visible");
            last_serial = g.serial;
            g
        };
        let key_press = |g: &BobberGeom| {
            let mut st = hook_side(&uia, g);
            for e in st.on_key(WM_KEYDOWN, &key(0x41), true).into_iter().flatten() {
                hub.emit(HubEvent::Ext(e));
            }
        };
        let settle = || std::thread::sleep(Duration::from_millis(400));

        // 1. 超时（浮标线程的路径：先认领并报告，再开始淡出）→ 淡出期间按键、切换应用：只有一次 Timeout
        let g = show();
        hub.bobber_timed_out(g.serial);
        bobber.hide_shown(g.serial, true);
        key_press(&g);
        hub.app_switched();
        settle();
        assert_eq!(dismissals(), vec![DismissReason::Timeout]);

        // 2. 上层主动隐藏 → 淡出期间按键、切换应用：上层自己隐藏的，不报告
        got.lock().clear();
        let g = show();
        bobber.hide_now(true);
        wait("fade-out to start", &|| hub.inner.lock().ext.live.is_none());
        key_press(&g);
        hub.app_switched();
        settle();
        assert_eq!(dismissals(), Vec::new());

        // 3. 按键 → 再按键、切换应用、超时：只有第一次按键
        got.lock().clear();
        let g = show();
        key_press(&g);
        wait("the first dismissal", &|| !dismissals().is_empty());
        key_press(&g);
        hub.app_switched();
        hub.bobber_timed_out(g.serial);
        settle();
        assert_eq!(dismissals(), vec![DismissReason::KeyPress]);
        assert!(hub.bobber_rect().is_none(), "消失后浮标已隐藏");
    }
}
