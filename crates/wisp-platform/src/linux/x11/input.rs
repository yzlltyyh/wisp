//! XInput2 原始事件：外部点击（OutsideClick）与扩展触发（InputHooks：浮标候选 / 双击修饰键 / 鼠标侧键）。
//!
//! - 原始事件（XI 2.0+）在根窗口上选择，**只按当前需要**订阅：没有任何监听时掩码为 0，X 服务器不会给 Wisp 发任何
//!   输入事件；只开浮标时只收鼠标按键，开双击修饰键或浮标可见时才收键盘事件（不用就零成本）；
//! - 不需要单独的钩子线程：事件由 hub 线程分发，判定只做 O(1) 工作，回调投递到回调线程执行；判定规则（连击、
//!   按逻辑像素计算位移、浮标候选之后的探测取消、修饰键追踪、侧键长按）在纯逻辑模块 `hook_logic` 里；
//! - 发出 `BobberCandidate` 后 200 ms（80 ms 延迟 + 120 ms 期限）内的任何鼠标按下（滚轮除外，浮标还
//!   不可见时也一样）都发 `BobberDismiss(MouseDown)`，上层据此取消进行中的探测——三击的第三下不会让浮标在
//!   按键还按着时弹出，与 macOS / Windows 一致；
//! - XTEST 虚拟设备产生的事件（包括 Wisp 自己的模拟按键）一律忽略，相当于 Windows 的「忽略 INJECTED」；
//! - 安装了浮标钩子时用 XFixes 跟踪 PRIMARY 的占有，并记录每次手势第一次左键按下的服务器时间（`primary_gate`），
//!   供 `has_selection` 区分「刚选中的文字」和「早就留在 PRIMARY 里的旧选区」；
//! - 鼠标侧键（8 = Back，9 = Forward）用根窗口上的被动抓取吞掉；抓取被其他程序占用且侧键是唯一请求
//!   的功能时，`install` 返回带原因的 Unsupported。X11 不拦截中键：X11 下 PRIMARY 几乎总有 owner，
//!   「有选区才拦截」无法成立，拦截会破坏中键粘贴，因此中键绑定在 X11 上不生效（只请求中键时返回 Unsupported）；
//! - 注册（外部点击、钩子）保存在跨连接的 `Registry` 里，`sync` 把它们应用到当前连接（重连后同样调用）。

use std::sync::Arc;

use parking_lot::Mutex;
use wisp_types::Point;
use x11rb::connection::Connection;
use x11rb::protocol::xinput;

use super::{X11, screen};
use crate::types::{ClickEvent, ClickKind, PointerButton};

#[cfg(feature = "mouse-hook")]
use crate::types::{DismissReason, HookSpec, InputSignal};

/// 外部点击回调
pub(crate) type ClickCb = Arc<dyn Fn(ClickEvent) + Send + Sync>;
/// 钩子 sink
#[cfg(feature = "mouse-hook")]
pub(crate) type Sink = Arc<dyn Fn(InputSignal) + Send + Sync>;

/// 一个已注册的钩子（跨连接保存）
#[cfg(feature = "mouse-hook")]
#[derive(Clone)]
pub(crate) struct HookEntry {
    pub id: u64,
    pub spec: HookSpec,
    pub sink: Sink,
}

/// 当前连接上一个钩子的运行状态
#[cfg(feature = "mouse-hook")]
struct HookReg {
    id: u64,
    spec: HookSpec,
    sink: Sink,
    tap: Option<(crate::common::double_tap::DoubleTapMachine, &'static [u32])>,
}

/// 长按模式下进行中的侧键按下（状态机由 `common::gesture::LongPress` 提供）
#[cfg(feature = "mouse-hook")]
struct PendingPress {
    button: u8,
    generation: u64,
    machine: crate::common::gesture::LongPress,
}

#[derive(Default)]
struct Inner {
    /// 外部点击回调（`Registry` 的副本，热路径只锁这一把锁）
    clicks: Vec<ClickCb>,
    selected: u32,
    xtest_devices: Option<Vec<u16>>,
    #[cfg(feature = "mouse-hook")]
    hooks: Vec<HookReg>,
    /// 左键手势：连击计数、按下位置、最近一次候选的时刻（`hook_logic`）
    #[cfg(feature = "mouse-hook")]
    gesture: crate::linux::hook_logic::Gesture,
    #[cfg(feature = "mouse-hook")]
    grabbed: [bool; 2],
    #[cfg(feature = "mouse-hook")]
    held_mods: crate::linux::hook_logic::HeldModifiers,
    #[cfg(feature = "mouse-hook")]
    long_press: Option<PendingPress>,
    #[cfg(feature = "mouse-hook")]
    press_generation: u64,
    /// 是否在用 XFixes 跟踪 PRIMARY 的占有
    #[cfg(feature = "mouse-hook")]
    primary_tracked: bool,
}

/// 输入监听的状态（每个连接一份）
#[derive(Default)]
pub(crate) struct InputState {
    inner: Mutex<Inner>,
}

/// 是否支持原始输入事件
pub(crate) fn available(x: &X11) -> bool {
    x.ext.xinput.is_some()
}

/// XIAllMasterDevices
const ALL_MASTER_DEVICES: u16 = 1;

fn query_xtest_devices(x: &X11) -> Vec<u16> {
    xinput::xi_query_device(&x.conn, 0u16)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| {
            r.infos
                .iter()
                .filter(|d| String::from_utf8_lossy(&d.name).contains("XTEST"))
                .map(|d| d.deviceid)
                .collect()
        })
        .unwrap_or_default()
}

/// 按当前需要重新计算并应用 XI2 掩码
fn apply_mask(x: &X11, inner: &mut Inner) {
    if !available(x) {
        return;
    }
    #[cfg(feature = "mouse-hook")]
    let (hook_buttons, hook_keys) = {
        let visible = super::bobber::is_visible(x);
        let buttons = inner.hooks.iter().any(|h| h.spec.bobber || h.spec.double_tap.is_some());
        let keys = inner.hooks.iter().any(|h| h.spec.double_tap.is_some());
        (buttons || visible, keys || visible)
    };
    #[cfg(not(feature = "mouse-hook"))]
    let (hook_buttons, hook_keys) = (false, false);
    let need_buttons = !inner.clicks.is_empty() || hook_buttons;
    let need_keys = hook_keys;
    let mut mask = 0u32;
    if need_buttons {
        mask |= u32::from(xinput::XIEventMask::RAW_BUTTON_PRESS) | u32::from(xinput::XIEventMask::RAW_BUTTON_RELEASE);
    }
    if need_keys {
        mask |= u32::from(xinput::XIEventMask::RAW_KEY_PRESS) | u32::from(xinput::XIEventMask::RAW_KEY_RELEASE);
    }
    if mask == inner.selected {
        return;
    }
    if mask != 0 && inner.xtest_devices.is_none() {
        inner.xtest_devices = Some(query_xtest_devices(x));
    }
    let em = xinput::EventMask {
        deviceid: ALL_MASTER_DEVICES,
        mask: vec![xinput::XIEventMask::from(mask)],
    };
    match xinput::xi_select_events(&x.conn, x.root, &[em]) {
        Ok(_) => {
            inner.selected = mask;
            let _ = x.conn.flush();
            log::debug!("XI2 原始事件掩码 → 0x{mask:x}");
        }
        Err(e) => log::warn!("XI2 订阅失败：{e}"),
    }
}

/// 重新计算掩码（浮标显示 / 隐藏时调用）
#[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
pub(crate) fn refresh_mask(x: &X11) {
    let mut inner = x.input.inner.lock();
    apply_mask(x, &mut inner);
}

/// 把 `Registry` 里的外部点击与钩子应用到这个连接（注册 / 注销后、重连后调用）
pub(crate) fn sync(x: &Arc<X11>) {
    let clicks: Vec<ClickCb> = x.reg.clicks.lock().iter().map(|(_, cb)| Arc::clone(cb)).collect();
    #[cfg(feature = "mouse-hook")]
    let entries: Vec<HookEntry> = x.reg.hooks.lock().clone();
    #[cfg(feature = "mouse-hook")]
    let has_bobber;
    {
        let mut inner = x.input.inner.lock();
        inner.clicks = clicks;
        #[cfg(feature = "mouse-hook")]
        {
            hooks::rebuild(&mut inner, entries);
            hooks::apply_grabs(x, &mut inner);
            has_bobber = inner.hooks.iter().any(|h| h.spec.bobber);
            hooks::track_primary(x, &mut inner, has_bobber);
        }
        apply_mask(x, &mut inner);
    }
    #[cfg(feature = "mouse-hook")]
    if !has_bobber {
        // 没有任何钩子要求浮标：销毁浮标窗口（之后再按新掩码订阅）
        super::bobber::destroy(x);
        refresh_mask(x);
    }
}

fn pointer_button(button: u32) -> Option<PointerButton> {
    Some(match button {
        1 => PointerButton::Left,
        2 => PointerButton::Middle,
        3 => PointerButton::Right,
        // 4–7 是滚轮，不算点击
        4..=7 => return None,
        8 => PointerButton::Back,
        9 => PointerButton::Forward,
        _ => PointerButton::Other,
    })
}

/// 光标位置 → (显示器 id, 显示器内逻辑坐标, 缩放)
fn locate(x: &X11, px: i32, py: i32) -> Option<(String, Point, f64)> {
    let monitors = screen::monitors(x);
    let m = crate::linux::geometry::monitor_at(&monitors, px, py)?;
    Some((m.id.clone(), m.to_local(px, py), m.scale))
}

/// 原始鼠标按键（hub 线程）。`time` 是服务器时间戳
pub(crate) fn on_raw_button(x: &Arc<X11>, button: u32, press: bool, source: u16, time: u32) {
    let mut clicks: Vec<(ClickCb, ClickEvent)> = Vec::new();
    #[cfg(feature = "mouse-hook")]
    let mut signals: Vec<(Sink, InputSignal)> = Vec::new();
    {
        #[cfg_attr(not(feature = "mouse-hook"), allow(unused_mut))]
        let mut inner = x.input.inner.lock();
        if inner.xtest_devices.as_ref().is_some_and(|d| d.contains(&source)) {
            return;
        }
        #[cfg(feature = "mouse-hook")]
        let (hooks_active, hooks_need_pos) = {
            let visible = super::bobber::is_visible(x);
            let bobber_hook = button == 1 && inner.hooks.iter().any(|h| h.spec.bobber);
            (!inner.hooks.is_empty() || visible, visible || bobber_hook)
        };
        #[cfg(not(feature = "mouse-hook"))]
        let (hooks_active, hooks_need_pos) = (false, false);
        let wants_click = !inner.clicks.is_empty() && pointer_button(button).is_some();
        if !wants_click && !hooks_active {
            return;
        }
        // 只有真正需要位置时才查询光标（一次往返）：只开双击修饰键时，鼠标按下只用来作废序列
        let pos = if wants_click || hooks_need_pos {
            super::screen::pointer(x)
        } else {
            None
        };
        if wants_click
            && let (Some(btn), Some((px, py, _))) = (pointer_button(button), pos)
            && let Some((monitor, at, _)) = locate(x, px, py)
        {
            let ev = ClickEvent {
                kind: if press { ClickKind::Down } else { ClickKind::Up },
                at: (monitor, at),
                button: btn,
            };
            clicks.extend(inner.clicks.iter().map(|cb| (Arc::clone(cb), ev.clone())));
        }
        #[cfg(feature = "mouse-hook")]
        if hooks_active {
            hook_button(x, &mut inner, button, press, pos, time, &mut signals);
        }
        #[cfg(not(feature = "mouse-hook"))]
        let _ = time;
    }
    for (cb, ev) in clicks {
        x.reg.post("外部点击", move || cb(ev));
    }
    #[cfg(feature = "mouse-hook")]
    for (sink, sig) in signals {
        x.reg.post("输入钩子", move || sink(sig));
    }
}

/// 原始键盘事件（hub 线程）
pub(crate) fn on_raw_key(x: &Arc<X11>, keycode: u32, press: bool, repeat: bool, source: u16) {
    #[cfg(feature = "mouse-hook")]
    {
        if repeat {
            return;
        }
        let mut signals: Vec<(Sink, InputSignal)> = Vec::new();
        {
            let mut inner = x.input.inner.lock();
            if inner.xtest_devices.as_ref().is_some_and(|d| d.contains(&source)) {
                return;
            }
            hook_key(x, &mut inner, keycode, press, &mut signals);
        }
        for (sink, sig) in signals {
            x.reg.post("输入钩子", move || sink(sig));
        }
    }
    #[cfg(not(feature = "mouse-hook"))]
    let _ = (x, keycode, press, repeat, source);
}

#[cfg(feature = "mouse-hook")]
pub(crate) use hooks::*;

#[cfg(feature = "mouse-hook")]
mod hooks {
    use std::time::{Duration, Instant};

    use wisp_types::{HotkeyAction, MouseButtonKind};
    use x11rb::connection::Connection;
    use x11rb::protocol::xfixes;
    use x11rb::protocol::xproto::{
        AtomEnum, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ButtonIndex, ConnectionExt as _, EventMask, GrabMode,
        ModMask,
    };
    use x11rb::protocol::xtest;
    use x11rb::{CURRENT_TIME, NONE};

    use super::*;
    use crate::common::double_tap::{DoubleTapMachine, TapInput};
    use crate::common::gesture::{self, MouseUpFacts};
    use crate::linux::hook_logic::{self, Gesture, PressDismiss, SideAction};
    use crate::linux::keysym;
    use crate::linux::session::reason;
    use crate::types::HookError;

    /// 被动抓取的侧键：(按钮号, 在 grabbed 数组中的下标)
    const SIDE_BUTTONS: [(u8, usize); 2] = [(8, 0), (9, 1)];
    /// ControlMask | Mod4Mask（Super）
    const CTRL_OR_SUPER: u16 = (1 << 2) | (1 << 6);
    /// 候选发出后，上层延迟 80 ms 探测选区、必须在 mouse-up 后 120 ms 内完成：这段时间里的鼠标按下要取消探测
    /// （与 macOS / Windows 的窗口相同）
    const PROBE_WINDOW: Duration = gesture::BOBBER_PROBE_DELAY.saturating_add(gesture::BOBBER_PROBE_DEADLINE);

    /// 用 `Registry` 的条目重建本连接的钩子状态（保留已有钩子的双击状态机）
    pub(super) fn rebuild(inner: &mut Inner, entries: Vec<HookEntry>) {
        let mut old = std::mem::take(&mut inner.hooks);
        inner.hooks = entries
            .into_iter()
            .map(|e| {
                let tap = old
                    .iter()
                    .position(|h| h.id == e.id)
                    .and_then(|i| old.swap_remove(i).tap)
                    .or_else(|| {
                        e.spec
                            .double_tap
                            .map(|d| (DoubleTapMachine::new(d.interval), keysym::modifier_key_syms(d.key)))
                    });
                HookReg {
                    id: e.id,
                    spec: e.spec,
                    sink: e.sink,
                    tap,
                }
            })
            .collect();
        if inner.hooks.iter().all(|h| h.spec.mouse.is_none()) {
            inner.long_press = None;
        }
        if !inner.hooks.iter().any(|h| h.spec.double_tap.is_some()) {
            inner.held_mods.clear();
        }
    }

    /// 有浮标钩子时订阅 PRIMARY 的占有变化（XFixes），没有时退订——不用浮标就不为 PRIMARY 唤醒
    pub(super) fn track_primary(x: &X11, inner: &mut Inner, want: bool) {
        let want = want && x.ext.xfixes;
        if want == inner.primary_tracked {
            return;
        }
        let mask = if want {
            xfixes::SelectionEventMask::SET_SELECTION_OWNER
        } else {
            xfixes::SelectionEventMask::from(0u32)
        };
        if xfixes::select_selection_input(&x.conn, x.win, u32::from(AtomEnum::PRIMARY), mask).is_ok() {
            inner.primary_tracked = want;
            inner.gesture = Gesture::default();
            x.primary.update(|g| if want { g.start() } else { g.stop() });
            let _ = x.conn.flush();
        }
    }

    /// 安装钩子（`Registry` 里登记后应用到当前连接），返回注册 id
    pub(crate) fn install(x: &Arc<X11>, spec: HookSpec, sink: Sink) -> Result<u64, HookError> {
        if (spec.bobber || spec.double_tap.is_some()) && !available(x) {
            return Err(HookError::Unsupported(reason::X11_MISSING_XINPUT.to_owned()));
        }
        let side_requested: Vec<u8> = spec
            .mouse
            .map(|m| {
                [(8u8, m.back.is_some()), (9u8, m.forward.is_some())]
                    .into_iter()
                    .filter_map(|(b, on)| on.then_some(b))
                    .collect()
            })
            .unwrap_or_default();
        let middle_requested = spec.mouse.is_some_and(|m| m.middle.is_some());
        let others_requested = spec.bobber || spec.double_tap.is_some();
        if middle_requested {
            log::info!("X11 下不拦截中键（会破坏 PRIMARY 中键粘贴），中键绑定不生效");
            if side_requested.is_empty() && !others_requested {
                return Err(HookError::Unsupported(reason::X11_MIDDLE_BUTTON.to_owned()));
            }
        }
        let id = x.reg.next_id();
        x.reg.hooks.lock().push(HookEntry { id, spec, sink });
        sync(x);
        if !side_requested.is_empty() {
            let grabbed = {
                let inner = x.input.inner.lock();
                side_requested
                    .iter()
                    .filter(|&&b| SIDE_BUTTONS.iter().any(|&(sb, i)| sb == b && inner.grabbed[i]))
                    .count()
            };
            if grabbed == 0 && !others_requested {
                // 侧键是唯一请求的功能，却一个也抓不到（被其他程序占用）：不留下无效的注册
                x.reg.hooks.lock().retain(|h| h.id != id);
                sync(x);
                return Err(HookError::Unsupported(reason::X11_MOUSE_BUTTON_BUSY.to_owned()));
            }
            if grabbed < side_requested.len() {
                log::warn!(
                    "鼠标侧键只抓到 {grabbed}/{} 个（其余被其他程序占用），未抓到的绑定不生效",
                    side_requested.len()
                );
            }
        }
        Ok(id)
    }

    fn side_binding(inner: &Inner, button: u8) -> Option<(HotkeyAction, bool, Sink)> {
        inner.hooks.iter().find_map(|h| {
            let m = h.spec.mouse?;
            let action = match button {
                8 => m.back?,
                9 => m.forward?,
                _ => return None,
            };
            Some((action, m.long_press, Arc::clone(&h.sink)))
        })
    }

    pub(super) fn apply_grabs(x: &X11, inner: &mut Inner) {
        for (button, idx) in SIDE_BUTTONS {
            let want = side_binding(inner, button).is_some();
            if want == inner.grabbed[idx] {
                continue;
            }
            if want {
                let res = x
                    .conn
                    .grab_button(
                        false,
                        x.root,
                        EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE,
                        GrabMode::ASYNC,
                        GrabMode::ASYNC,
                        NONE,
                        NONE,
                        ButtonIndex::from(button),
                        ModMask::ANY,
                    )
                    .map_err(|e| e.to_string())
                    .and_then(|c| c.check().map_err(|e| e.to_string()));
                match res {
                    Ok(()) => inner.grabbed[idx] = true,
                    Err(e) => log::warn!("抓取鼠标按钮 {button} 失败（可能被其他程序占用）：{e}"),
                }
            } else {
                let _ = x.conn.ungrab_button(ButtonIndex::from(button), x.root, ModMask::ANY);
                inner.grabbed[idx] = false;
            }
        }
        let _ = x.conn.flush();
    }

    fn side_kind(button: u8) -> MouseButtonKind {
        if button == 8 {
            MouseButtonKind::Back
        } else {
            MouseButtonKind::Forward
        }
    }

    /// 被动抓取到的侧键（hub 线程；按下与抬起都已被吞掉）。决策见 `hook_logic::side_action`
    pub(crate) fn on_grabbed_button(x: &Arc<X11>, button: u8, press: bool) {
        let mut fire: Option<(Sink, InputSignal)> = None;
        let mut replay = false;
        {
            let mut inner = x.input.inner.lock();
            let Some((action, long_press, sink)) = side_binding(&inner, button) else {
                return;
            };
            let signal = InputSignal::MouseAction {
                button: side_kind(button),
                action,
            };
            let up = if long_press && !press {
                inner
                    .long_press
                    .take()
                    .filter(|lp| lp.button == button)
                    .map(|mut lp| lp.machine.up(Instant::now()))
            } else {
                None
            };
            match hook_logic::side_action(long_press, press, up) {
                SideAction::Fire => fire = Some((sink, signal)),
                SideAction::StartTimer => start_long_press(x, &mut inner, button, sink, signal),
                SideAction::Replay => replay = true,
                SideAction::None => {}
            }
        }
        if let Some((sink, sig)) = fire {
            x.reg.post("输入钩子", move || sink(sig));
        }
        if replay {
            replay_click(x, button);
        }
    }

    /// 长按模式下的按下：开始计时。400 ms 计时只在按住侧键期间存在（不留常驻定时器）
    fn start_long_press(x: &Arc<X11>, inner: &mut Inner, button: u8, sink: Sink, signal: InputSignal) {
        inner.press_generation += 1;
        let generation = inner.press_generation;
        let mut machine = gesture::LongPress::new();
        machine.down(Instant::now());
        inner.long_press = Some(PendingPress {
            button,
            generation,
            machine,
        });
        let x2 = Arc::clone(x);
        let _ = std::thread::Builder::new()
            .name("wisp-longpress".into())
            .spawn(move || {
                std::thread::sleep(gesture::LONG_PRESS);
                let fire = {
                    let mut inner = x2.input.inner.lock();
                    let lp = inner.long_press.as_mut().filter(|lp| lp.generation == generation);
                    lp.is_some_and(|lp| lp.machine.tick(Instant::now()))
                };
                if fire {
                    x2.reg.post("输入钩子", move || sink(signal));
                }
            });
    }

    /// 长按模式下的短按：暂时解除抓取，用 XTest 补发原始点击，再恢复抓取
    fn replay_click(x: &X11, button: u8) {
        let _ = x.conn.ungrab_button(ButtonIndex::from(button), x.root, ModMask::ANY);
        let _ = xtest::fake_input(&x.conn, BUTTON_PRESS_EVENT, button, CURRENT_TIME, x.root, 0, 0, 0);
        let _ = xtest::fake_input(&x.conn, BUTTON_RELEASE_EVENT, button, CURRENT_TIME, x.root, 0, 0, 0);
        let mut inner = x.input.inner.lock();
        if let Some(idx) = hook_logic::side_slot(button) {
            inner.grabbed[idx] = false;
        }
        apply_grabs(x, &mut inner);
    }

    /// 原始鼠标按键的钩子处理（hub 线程，持有 `Inner` 锁）：双击修饰键作废、浮标消失 / 取消探测、左键手势判定。
    /// 判定规则都在 `hook_logic` 里（可在任何平台单测）
    pub(super) fn hook_button(
        x: &Arc<X11>,
        inner: &mut Inner,
        button: u32,
        press: bool,
        pos: Option<(i32, i32, u16)>,
        time: u32,
        signals: &mut Vec<(Sink, InputSignal)>,
    ) {
        let has_bobber = inner.hooks.iter().any(|h| h.spec.bobber);
        if press {
            // 双击修饰键：中间夹了鼠标按下就作废
            for h in inner.hooks.iter_mut() {
                if let Some((m, _)) = h.tap.as_mut() {
                    m.feed(TapInput::MouseDown);
                }
            }
            if has_bobber {
                let visible = super::super::bobber::is_visible(x);
                let on_bobber = visible && pos.is_some_and(|(px, py, _)| super::super::bobber::contains(x, px, py));
                // 候选发出后、探测完成前又按下了（例如三击的第三下）→ 取消这次探测
                let probe_pending =
                    !hook_logic::is_wheel(button) && inner.gesture.press_cancels_probe(Instant::now(), PROBE_WINDOW);
                let reason = match hook_logic::press_dismiss(button, visible, on_bobber, probe_pending) {
                    PressDismiss::Scroll => Some(DismissReason::Scroll),
                    PressDismiss::MouseDown => Some(DismissReason::MouseDown),
                    PressDismiss::None => None,
                };
                if let Some(reason) = reason {
                    push_bobber(inner, signals, InputSignal::BobberDismiss(reason));
                }
            }
        }
        if button != 1 || !has_bobber {
            return;
        }
        let Some((px, py, mask)) = pos else { return };
        if press {
            let count = inner.gesture.left_down(Instant::now(), px, py);
            // 手势起点（双击 / 三击取第一次按下）：has_selection 要求 PRIMARY 在它之后被重新占有
            if inner.primary_tracked {
                x.primary.update(|g| g.on_left_press(time, count == 1));
            }
            return;
        }
        let located = locate(x, px, py);
        let scale = located.as_ref().map_or(1.0, |(_, _, s)| *s);
        let Some((drag_px, click_count)) = inner.gesture.left_up(px, py, scale) else {
            return;
        };
        let Some((monitor, at, _)) = located else { return };
        let in_own_window = x.reg.self_hit().is_some_and(|f| f(&monitor, at));
        let facts = MouseUpFacts {
            drag_px,
            click_count,
            command_or_control: mask & CTRL_OR_SUPER != 0,
            in_own_window,
        };
        if gesture::is_bobber_candidate(&facts) {
            inner.gesture.candidate_sent(Instant::now());
            push_bobber(inner, signals, InputSignal::BobberCandidate { at: (monitor, at) });
        }
    }

    fn push_bobber(inner: &Inner, signals: &mut Vec<(Sink, InputSignal)>, sig: InputSignal) {
        for h in inner.hooks.iter().filter(|h| h.spec.bobber) {
            signals.push((Arc::clone(&h.sink), sig.clone()));
        }
    }

    /// 原始键盘事件的钩子处理（hub 线程，持有 `Inner` 锁）：浮标消失、双击修饰键
    pub(super) fn hook_key(
        x: &Arc<X11>,
        inner: &mut Inner,
        keycode: u32,
        press: bool,
        signals: &mut Vec<(Sink, InputSignal)>,
    ) {
        let Ok(code) = u8::try_from(keycode) else { return };
        if press && super::super::bobber::is_visible(x) {
            push_bobber(inner, signals, InputSignal::BobberDismiss(DismissReason::KeyPress));
        }
        if !inner.hooks.iter().any(|h| h.tap.is_some()) {
            return;
        }
        let Some(map) = super::super::keyboard::keymap(x) else {
            return;
        };
        let sym = map.keysym(code, 0);
        let others_held = inner.held_mods.others_held(code);
        let is_lock = keysym::is_lock(sym);
        let now = Instant::now();
        for h in inner.hooks.iter_mut() {
            let Some((machine, targets)) = h.tap.as_mut() else {
                continue;
            };
            let Some(input) = hook_logic::tap_input(targets.contains(&sym), press, others_held, is_lock, now) else {
                continue;
            };
            if machine.feed(input) {
                signals.push((Arc::clone(&h.sink), InputSignal::DoubleTap));
            }
        }
        inner.held_mods.update(code, keysym::is_modifier(sym), press);
    }

    /// 前台应用切换（hub 线程）：浮标可见时消失
    pub(crate) fn on_app_switched(x: &X11) {
        if super::super::bobber::is_visible(x) {
            emit_bobber(x, InputSignal::BobberDismiss(DismissReason::AppSwitched));
        }
    }

    /// 浮标相关信号（浮标自己的计时器：4 s 无交互、离开中心过远；前台应用切换）
    pub(crate) fn emit_bobber(x: &X11, sig: InputSignal) {
        let sinks: Vec<Sink> = {
            let inner = x.input.inner.lock();
            inner
                .hooks
                .iter()
                .filter(|h| h.spec.bobber)
                .map(|h| Arc::clone(&h.sink))
                .collect()
        };
        for s in sinks {
            let sig = sig.clone();
            x.reg.post("输入钩子", move || s(sig));
        }
    }
}

/// 扩展触发在 X11 上的开销测量（用 Xvfb + XTest 直接测 XI2 原始事件的真实路径）。不是回归
/// 用例，手动运行（需要 X 服务器；Xvfb 即可）：
/// `xvfb-run -a cargo test -p wisp-platform --release --lib linux::x11::input::measure -- --ignored --nocapture --test-threads=1`
///
/// 三项扩展触发全开（浮标 + 双击右 Alt + 侧键 Back / Forward）。测四样东西，结果以一行 JSON 打到 stderr：
/// 1. **空闲**：装好钩子后静置 `WISP_MEASURE_IDLE_S`（默认 60）秒，本进程每个线程的自愿 / 非自愿切换与 CPU
///    （`/proc/self/task/*/{status,stat}`）——XI2 原始事件按需订阅，没有输入时 X 服务器什么都不发，平台层的线程应当
///    一次都不醒；
/// 2. **端到端延迟**：另一个客户端用 XTest 注入（X 服务器 → XI2 原始事件 / 被动抓取 → 本进程的 X11 事件线程 → 判定 →
///    回调线程 → sink），从发出抬起 / 第二次松开修饰键 / 侧键按下的那一刻量到 sink 收到信号：mouse-up → 浮标候选、
///    双击右 Alt → DoubleTap、侧键 → MouseAction。XTest 设备的事件在产品里一律忽略（Wisp 自己的模拟按键也走 XTest），
///    测量期间把 XTest 设备表清空，让它们像物理设备一样被处理；
/// 3. **处理函数耗时**：在测试线程上直接调用与事件线程相同的处理函数（`on_raw_button` / `on_raw_key` /
///    `on_grabbed_button`），含其中真实的 X 往返（`QueryPointer`、显示器查询）与投递到回调线程；
/// 4. 订阅的 XI2 掩码（三项全开时只订阅按键与键盘的原始事件，不订阅移动）。
#[cfg(all(test, feature = "mouse-hook"))]
mod measure {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use wisp_types::{HotkeyAction, ModifierKey};
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{
        BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ConnectionExt as _, KEY_PRESS_EVENT, KEY_RELEASE_EVENT,
        MOTION_NOTIFY_EVENT,
    };
    use x11rb::protocol::xtest::ConnectionExt as _;
    use x11rb::rust_connection::RustConnection;

    use super::*;
    use crate::types::{DoubleTapSpec, HookSpec, InputSignal, MouseSpec};

    /// 右 Alt 的 evdev 键码（Xvfb 默认的 evdev / us 键盘映射里是 Alt_R）
    const RALT: u8 = 108;
    /// 字母 a（普通按键）
    const KEY_A: u8 = 38;

    /// 注入输入的独立客户端
    struct Driver {
        conn: RustConnection,
        root: u32,
    }

    impl Driver {
        fn connect() -> Self {
            let (conn, screen) = x11rb::connect(None).expect("connect X");
            let root = conn.setup().roots[screen].root;
            Self { conn, root }
        }
        fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) {
            self.conn
                .xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, x, y, 0)
                .expect("fake input");
            self.conn.flush().expect("flush");
        }
        fn motion(&self, x: i16, y: i16) {
            self.fake(MOTION_NOTIFY_EVENT, 0, x, y);
        }
        fn button(&self, b: u8, press: bool) {
            self.fake(
                if press {
                    BUTTON_PRESS_EVENT
                } else {
                    BUTTON_RELEASE_EVENT
                },
                b,
                0,
                0,
            );
        }
        fn key(&self, code: u8, press: bool) {
            self.fake(if press { KEY_PRESS_EVENT } else { KEY_RELEASE_EVENT }, code, 0, 0);
        }
        /// 一次往返：此前注入的事件都已被服务器处理
        fn sync(&self) {
            self.conn.get_input_focus().expect("sync").reply().expect("sync reply");
        }
    }

    /// 每个线程：(名字, 自愿切换, 非自愿切换, CPU tick)
    fn threads() -> Vec<(String, u64, u64, u64)> {
        let mut out = Vec::new();
        for e in std::fs::read_dir("/proc/self/task").expect("tasks").flatten() {
            let dir = e.path();
            let name = std::fs::read_to_string(dir.join("comm"))
                .unwrap_or_default()
                .trim()
                .to_owned();
            let status = std::fs::read_to_string(dir.join("status")).unwrap_or_default();
            let field = |k: &str| {
                status
                    .lines()
                    .find_map(|l| l.strip_prefix(k))
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .unwrap_or(0)
            };
            let stat = std::fs::read_to_string(dir.join("stat")).unwrap_or_default();
            // comm 可能含空格：从最后一个 ')' 之后数字段（utime / stime 是第 14、15 个字段）
            let rest = stat.rsplit_once(')').map_or("", |(_, r)| r);
            let f: Vec<&str> = rest.split_whitespace().collect();
            let ticks = f.get(11).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0)
                + f.get(12).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            out.push((
                format!(
                    "{name}#{}",
                    dir.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                ),
                field("voluntary_ctxt_switches:"),
                field("nonvoluntary_ctxt_switches:"),
                ticks,
            ));
        }
        out
    }

    /// 微秒，(p50, p99, max)
    fn pct(v: &mut [Duration]) -> (f64, f64, f64) {
        v.sort_unstable();
        let at = |q: f64| {
            let i = ((v.len() as f64 - 1.0) * q).round() as usize;
            v[i.min(v.len() - 1)].as_secs_f64() * 1e6
        };
        (at(0.5), at(0.99), v.last().map_or(0.0, |d| d.as_secs_f64() * 1e6))
    }

    fn json_pct(name: &str, v: &mut [Duration]) -> String {
        let (p50, p99, max) = pct(v);
        format!(
            "\"{name}\":{{\"n\":{},\"p50_us\":{p50:.1},\"p99_us\":{p99:.1},\"max_us\":{max:.1}}}",
            v.len()
        )
    }

    /// 等 sink 收到满足 `want` 的信号，返回收到的时刻
    fn wait_for(
        what: &str,
        rx: &mpsc::Receiver<(Instant, InputSignal)>,
        want: impl Fn(&InputSignal) -> bool,
    ) -> Instant {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok((at, sig)) if want(&sig) => return at,
                Ok(_) => {}
                Err(_) => panic!("2 s 内没有收到期望的信号：{what}"),
            }
        }
    }

    #[test]
    #[ignore = "测量用（扩展触发的开销，X11），手动运行"]
    #[allow(clippy::print_stderr, clippy::too_many_lines)]
    fn x11_hook_costs() {
        let p = crate::Platform::create(crate::PlatformConfig {
            own_pid: std::process::id(),
            main_thread: Arc::new(|f: Box<dyn FnOnce() + Send>| f()),
            self_hit: Arc::new(|_, _| false),
            keymaps: Arc::new(parking_lot::RwLock::new(Vec::new())),
            os_version: "measure".to_owned(),
        });
        let (tx, rx) = mpsc::channel::<(Instant, InputSignal)>();
        let tx = Mutex::new(tx);
        let spec = HookSpec {
            bobber: true,
            double_tap: Some(DoubleTapSpec {
                key: ModifierKey::RightAlt,
                interval: Duration::from_millis(300),
            }),
            mouse: Some(MouseSpec {
                long_press: false,
                middle: None,
                back: Some(HotkeyAction::TranslateSelection),
                forward: Some(HotkeyAction::Ocr),
            }),
        };
        let _hook = p
            .hooks
            .install(
                spec,
                Box::new(move |sig| {
                    let _ = tx.lock().send((Instant::now(), sig));
                }),
            )
            .expect("install hooks");
        let x = p.shared.ctx.x11.get().expect("X11");
        let mask = x.input.inner.lock().selected;

        // 1. 空闲
        let idle_s: u64 = std::env::var("WISP_MEASURE_IDLE_S")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60);
        std::thread::sleep(Duration::from_millis(500));
        let before = threads();
        std::thread::sleep(Duration::from_secs(idle_s));
        let after = threads();
        let mut idle = Vec::new();
        for (name, vol, invol, ticks) in &after {
            let (v0, i0, t0) = before
                .iter()
                .find(|b| &b.0 == name)
                .map_or((0, 0, 0), |b| (b.1, b.2, b.3));
            idle.push(format!(
                "{{\"thread\":\"{name}\",\"voluntary\":{},\"nonvoluntary\":{},\"cpu_ticks\":{}}}",
                vol - v0,
                invol - i0,
                ticks - t0
            ));
        }

        // 2. 端到端（XTest 注入；测量期间把 XTest 设备当成物理设备）
        x.input.inner.lock().xtest_devices = Some(Vec::new());
        let d = Driver::connect();
        let drain = || while rx.try_recv().is_ok() {};
        let mut bobber = Vec::new();
        for i in 0..100i16 {
            let y = 100 + (i % 10) * 30;
            d.motion(200, y);
            d.button(1, true);
            // 原始事件不带坐标，处理函数按下 / 抬起时各查一次指针位置（QueryPointer）：等事件线程处理完按下再拖动，
            // 否则一口气注入的移动会先于按下的处理落地，按下点就成了终点（真人拖选的间隔远大于这几毫秒）
            d.sync();
            std::thread::sleep(Duration::from_millis(30));
            d.motion(320, y);
            d.motion(420, y);
            d.sync();
            std::thread::sleep(Duration::from_millis(20));
            drain();
            let t0 = Instant::now();
            d.button(1, false);
            let at = wait_for("BobberCandidate", &rx, |s| {
                matches!(s, InputSignal::BobberCandidate { .. })
            });
            bobber.push(at - t0);
            // 越过双击间隔与探测窗口，下一次是新的手势
            std::thread::sleep(Duration::from_millis(450));
        }
        let mut double_tap = Vec::new();
        for _ in 0..50 {
            drain();
            d.key(RALT, true);
            d.key(RALT, false);
            d.key(RALT, true);
            d.sync();
            std::thread::sleep(Duration::from_millis(10));
            let t0 = Instant::now();
            d.key(RALT, false);
            let at = wait_for("DoubleTap", &rx, |s| matches!(s, InputSignal::DoubleTap));
            double_tap.push(at - t0);
            std::thread::sleep(Duration::from_millis(400));
        }
        let mut side = Vec::new();
        for _ in 0..100 {
            drain();
            let t0 = Instant::now();
            d.button(8, true);
            let at = wait_for("MouseAction", &rx, |s| matches!(s, InputSignal::MouseAction { .. }));
            side.push(at - t0);
            d.button(8, false);
            d.sync();
            std::thread::sleep(Duration::from_millis(30));
        }

        // 3. 处理函数耗时（测试线程上直接调用；事件线程此时没有事件）
        let time = |f: &dyn Fn()| {
            let t = Instant::now();
            f();
            t.elapsed()
        };
        const SRC: u16 = 9999;
        let (mut left_down, mut drag_up, mut click_up, mut right, mut wheel) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let (mut ralt, mut plain_key, mut side_fn) = (Vec::new(), Vec::new(), Vec::new());
        for i in 0..2000u32 {
            let y = i16::try_from(100 + (i % 10) * 30).unwrap_or(100);
            d.motion(200, y);
            d.sync();
            left_down.push(time(&|| on_raw_button(&x, 1, true, SRC, i * 1000)));
            if i % 2 == 0 {
                d.motion(420, y);
                d.sync();
                drag_up.push(time(&|| on_raw_button(&x, 1, false, SRC, i * 1000 + 300)));
            } else {
                click_up.push(time(&|| on_raw_button(&x, 1, false, SRC, i * 1000 + 50)));
            }
            right.push(time(&|| on_raw_button(&x, 3, true, SRC, i * 1000 + 400)));
            wheel.push(time(&|| on_raw_button(&x, 5, true, SRC, i * 1000 + 500)));
            ralt.push(time(&|| on_raw_key(&x, u32::from(RALT), i % 2 == 0, false, SRC)));
            plain_key.push(time(&|| on_raw_key(&x, u32::from(KEY_A), true, false, SRC)));
            side_fn.push(time(&|| on_grabbed_button(&x, 8, true)));
            side_fn.push(time(&|| on_grabbed_button(&x, 8, false)));
            // 越过双击间隔：上面的按下 / 抬起不连成多击
            gesture_gap(&x);
        }
        drain();
        eprintln!(
            "{{\"xi2_mask\":\"0x{mask:x}\",\"idle_s\":{idle_s},\"idle_threads\":[{}],\"e2e\":{{{},{},{}}},\"handler\":{{{},{},{},{},{},{},{},{}}}}}",
            idle.join(","),
            json_pct("mouse_up_to_bobber_candidate", &mut bobber),
            json_pct("double_tap_release_to_signal", &mut double_tap),
            json_pct("side_button_press_to_signal", &mut side),
            json_pct("left_down", &mut left_down),
            json_pct("left_up_drag_candidate", &mut drag_up),
            json_pct("left_up_plain_click", &mut click_up),
            json_pct("right_down", &mut right),
            json_pct("wheel", &mut wheel),
            json_pct("right_alt_down_up", &mut ralt),
            json_pct("plain_key", &mut plain_key),
            json_pct("side_button_grabbed", &mut side_fn),
        );
    }

    /// 让左键手势的连击计数归零（直接调用时不必真的等 500 ms）
    fn gesture_gap(x: &Arc<X11>) {
        x.input.inner.lock().gesture = crate::linux::hook_logic::Gesture::default();
    }
}
