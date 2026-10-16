//! 扩展触发的输入钩子。
//!
//! - 每次 `install` 起一个专用线程，在它的 CFRunLoop 上建最多三个 CGEventTap，掩码各取最小集：
//!   - Default（主动过滤）：需要吞掉的 otherMouseDown / Up，只在至少绑定了一个额外鼠标键时才建；
//!   - ListenOnly：浮标与双击需要的其余事件，不经过主动过滤器，中键判定的同步等待不会卡住键盘、滚轮与左键；
//!   - ListenOnly（浮标按键）：浮标「任意按键消失」用的 keyDown，**只在浮标可见期间启用**（`CGEventTapEnable`），
//!     其余时间停用、不收任何按键（常驻的事件掩码里没有 keyDown）。双击已经订阅 keyDown 时共用那个 tap，不另建。
//!
//!   三个 tap 共用一份状态；Handle drop 时可靠地停掉 run loop（见 runloop.rs）、注销全部 tap 并等待线程退出（有上限），
//!   全关时没有任何 tap 与线程（调用方传入合并后的完整 spec，多项共用一个钩子线程）；
//! - tap 被系统禁用（回调超时 / 用户输入）时立即重新 enable 并计数（浮标按键 tap 只在浮标可见时重新启用）；
//! - 回调第一行就过滤不相关的事件与 Wisp 自己注入的事件（`kCGEventSourceUserData` = Wisp 标记）；除中键判定外只做
//!   O(1) 的工作，信号经 sink（调用方转交 channel）送出。中键判定在回调里同步完成，上限 30 ms（含查询光标下窗口的
//!   属主），超时一律放行；
//! - 浮标：每次左键 mouse-up 做一次同步判定（是否落在 Wisp 窗口内、位移与连击、是否按着 ⌘ / Ctrl），通过则发
//!   `BobberCandidate`，选区判定由上层延迟 80 ms 后在工作线程上用 `has_selection` 完成；候选发出后、判定窗口内又有
//!   鼠标按下 → 发 `BobberDismiss(MouseDown)` 取消。
//!   浮标可见期间的按键 / 滚轮 / 外部按下由这里判定消失，超时、移开、切换应用由浮标窗口自己判定（见 bobber.rs）；
//! - 双击修饰键：listen-only 的 flagsChanged（需要输入监控），按 flags 判定（屏蔽 CapsLock 等位），中间夹按键作废；
//!   两次按下之间是否有鼠标按下用 `CGEventSourceSecondsSinceLastEventType` 判断，不必订阅鼠标事件；
//! - 鼠标额外按键：otherMouseDown / Up（2 中键、3 Back、4 Forward）；拦截时按下与抬起都吞掉；长按模式先吞掉，
//!   400 ms 内抬起就重新注入原始的按下与抬起（带 Wisp 标记）。
//! - 连续粘贴的粘贴键（`paste_keys`）：第四个 tap（Default，keyDown / keyUp），只在队列进行中存在。粘贴键在安装时按当前
//!   布局换成键码，回调里只比较键码与修饰位：匹配的按下吞掉并发出 `PasteKey`，同一个键的自动重复与抬起一并吞掉；上层
//!   写好下一条后经 `replay_paste` 带 Wisp 标记重放（回调第一行就放行它）。有它时 keyDown 只由这个 tap 送达（浮标 /
//!   双击的按键判定照常在没有吞掉时进行），同一个事件不会处理两次。屏幕锁定（`com.apple.screenIsLocked`）时发出
//!   `SessionLocked`。

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use objc2_core_foundation::{
    CFAbsoluteTimeGetCurrent, CFDictionary, CFMachPort, CFNumber, CFRetained, CFRunLoop, CFRunLoopSource,
    CFRunLoopTimer, CFRunLoopTimerContext, CFString, CFType, kCFRunLoopCommonModes,
};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventMask, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType, CGWindowID, CGWindowListCopyWindowInfo, CGWindowListOption,
    kCGWindowOwnerPID,
};
use parking_lot::Mutex;
use wisp_types::{ModifierKey, MouseButtonKind, PermissionKind, PermissionState};

use super::Ctx;
use super::ax;
use super::bobber;
use super::displays;
use super::focus::frontmost_pid;
use super::keyboard;
use super::notify::DistributedObserver;
use super::permissions::input_monitoring;
use super::runloop::LoopThread;
use super::util::{guarded, pool};
use crate::common::WISP_INPUT_TAG;
use crate::common::double_tap::mac::{FLAG_COMMAND, FLAG_CONTROL, FlagsEvent, classify_flags_changed};
use crate::common::double_tap::{DoubleTapMachine, TapInput};
use crate::common::gesture::{
    self, BOBBER_PROBE_DEADLINE, BOBBER_PROBE_DELAY, LONG_PRESS, LongPress, LongPressUp, MIDDLE_CLICK_BUDGET,
    MouseUpFacts,
};
use crate::traits::InputHooks;
use crate::types::{DismissReason, HookError, HookHandle, HookSpec, InputSignal, KeyChord, PasteKeys};

/// 等待 tap 线程建好 tap 的上限
const READY_WAIT: Duration = Duration::from_secs(2);
/// 注销时等待 tap 线程退出的上限（正常 < 10 ms；超过则记录错误并放弃等待，不挂起调用方）
const STOP_WAIT: Duration = Duration::from_secs(1);
/// 长按定时器的「远期」（空闲时把下次触发推到很远，不产生唤醒）
const FAR_FUTURE: f64 = 1.0e10;

fn mask_of(t: CGEventType) -> CGEventMask {
    1u64 << t.0
}

/// tap 的槽位（[`TapPlan`] 的三个掩码依次对应）
const SLOT_ACTIVE: usize = 0;
const SLOT_LISTEN: usize = 1;
const SLOT_KEYS: usize = 2;
const SLOT_PASTE: usize = 3;
const SLOTS: usize = 4;

/// CGEventFlags 里四个修饰键的设备无关位（Shift / Control / Option / Command）；粘贴键只按这四位比较
const FLAG_SHIFT_BIT: u64 = 0x0002_0000;
const FLAG_CONTROL_BIT: u64 = 0x0004_0000;
const FLAG_ALTERNATE_BIT: u64 = 0x0008_0000;
const FLAG_COMMAND_BIT: u64 = 0x0010_0000;

/// 屏幕锁定的分布式通知
const SCREEN_LOCKED: &str = "com.apple.screenIsLocked";

/// 安装时解析好的一组粘贴键：当前布局下的键码 + 修饰位（只含上面四位）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PasteKeyCode {
    keycode: u16,
    flags: u64,
    chord: KeyChord,
}

/// 组合键的修饰键 → 四个设备无关位
fn modifier_bits(m: &crate::common::combo::Modifiers) -> u64 {
    let mut bits = 0;
    if m.shift {
        bits |= FLAG_SHIFT_BIT;
    }
    if m.control {
        bits |= FLAG_CONTROL_BIT;
    }
    if m.alt {
        bits |= FLAG_ALTERNATE_BIT;
    }
    if m.super_key {
        bits |= FLAG_COMMAND_BIT;
    }
    bits
}

/// 按当前键盘布局解析粘贴键（安装时在调用方线程上做：字母键码可能要等主线程预建布局表，不能放进 tap 回调）
fn resolve_paste_keys(ctx: &Ctx, keys: &PasteKeys) -> Vec<PasteKeyCode> {
    keys.iter()
        .filter_map(|chord| match keyboard::keycode_for(ctx, &chord.to_combo()) {
            Ok(keycode) => Some(PasteKeyCode {
                keycode,
                flags: modifier_bits(&chord.modifiers),
                chord,
            }),
            Err(e) => {
                log::info!("paste key {} not resolvable in the current layout: {e}", chord.code);
                None
            }
        })
        .collect()
}

/// 浮标按键 tap 的跨线程句柄：浮标显示 / 消失时由任意线程启用 / 停用（`CGEventTapEnable` 线程安全）
pub(crate) struct KeyTap(CFRetained<CFMachPort>);

// SAFETY: 跨线程只调用线程安全的 CGEventTapEnable；对象由 CF 引用计数管理，注销（invalidate）只在 tap 线程上进行
unsafe impl Send for KeyTap {}
// SAFETY: 同上
unsafe impl Sync for KeyTap {}

impl KeyTap {
    pub(crate) fn set_enabled(&self, enabled: bool) {
        if CGEvent::tap_is_enabled(&self.0) != enabled {
            CGEvent::tap_enable(&self.0, enabled);
        }
    }
}

/// 至少绑定了一个额外鼠标键（没有绑定时不拦截、不建 Default tap）
fn mouse_bound(spec: &HookSpec) -> bool {
    spec.mouse
        .is_some_and(|m| m.middle.is_some() || m.back.is_some() || m.forward.is_some())
}

/// InputHooks 实现
pub(crate) struct MacHooks {
    ctx: Arc<Ctx>,
    next_id: AtomicU64,
}

impl MacHooks {
    pub(crate) fn new(ctx: Arc<Ctx>) -> Self {
        Self {
            ctx,
            next_id: AtomicU64::new(1),
        }
    }
}

/// 按下中的额外鼠标键的状态
#[derive(Default)]
struct ButtonState {
    /// 按下已被拦截（非长按模式），抬起也要吞掉
    intercepted: bool,
    long: LongPress,
    /// 长按模式下被吞掉的原始按下事件（未达阈值时重新注入）
    pending_down: Option<CFRetained<CGEvent>>,
}

/// tap 线程上的状态（只在该线程的回调里访问）
struct TapState {
    ctx: Arc<Ctx>,
    spec: HookSpec,
    sink: Arc<dyn Fn(InputSignal) + Send + Sync>,
    /// 本线程的 tap（按槽位；被系统禁用时重新 enable）
    taps: Cell<[*const CFMachPort; SLOTS]>,
    /// 长按定时器（未启用长按时为空）
    timer: Cell<*const CFRunLoopTimer>,
    left_down: Cell<Option<objc2_core_foundation::CGPoint>>,
    candidate_at: Cell<Option<Instant>>,
    double: RefCell<Option<(DoubleTapMachine, ModifierKey)>>,
    buttons: RefCell<[ButtonState; 3]>,
    disabled_count: Cell<u32>,
    /// 连续粘贴的粘贴键（安装时解析好的键码）
    paste: Vec<PasteKeyCode>,
    /// 拦下的那一次按下的键码：它的自动重复与抬起一并吞掉
    paste_held: Cell<Option<u16>>,
}

/// 回调的处理结论
enum Verdict {
    Pass,
    Swallow,
}

fn button_index(kind: MouseButtonKind) -> usize {
    match kind {
        MouseButtonKind::Middle => 0,
        MouseButtonKind::Back => 1,
        MouseButtonKind::Forward => 2,
    }
}

fn button_kind(number: i64) -> Option<MouseButtonKind> {
    match number {
        2 => Some(MouseButtonKind::Middle),
        3 => Some(MouseButtonKind::Back),
        4 => Some(MouseButtonKind::Forward),
        _ => None,
    }
}

impl TapState {
    fn emit(&self, signal: InputSignal) {
        (self.sink)(signal);
    }

    /// 候选发出后的判定窗口内又有鼠标按下：取消这次候选
    fn cancel_pending_candidate(&self) {
        if let Some(at) = self.candidate_at.take()
            && at.elapsed() <= BOBBER_PROBE_DELAY + BOBBER_PROBE_DEADLINE
        {
            self.emit(InputSignal::BobberDismiss(DismissReason::MouseDown));
        }
    }

    fn feed_double(&self, input: TapInput) {
        let fired = self
            .double
            .borrow_mut()
            .as_mut()
            .is_some_and(|(machine, _)| machine.feed(input));
        if fired {
            self.emit(InputSignal::DoubleTap);
        }
    }

    fn any_mouse_down(&self, at: objc2_core_foundation::CGPoint) {
        if self.spec.bobber {
            if self.ctx.bobber.is_visible() {
                let (monitor, local) = displays::locate(at);
                if !self.ctx.bobber.contains(&monitor, local) {
                    bobber::dismiss(&self.ctx, DismissReason::MouseDown);
                }
            }
            self.cancel_pending_candidate();
        }
        self.feed_double(TapInput::MouseDown);
    }

    fn handle(&self, etype: CGEventType, ev: &CGEvent) -> Verdict {
        match etype {
            CGEventType::LeftMouseDown => {
                let at = CGEvent::location(Some(ev));
                self.any_mouse_down(at);
                if self.spec.bobber {
                    self.left_down.set(Some(at));
                }
                Verdict::Pass
            }
            CGEventType::LeftMouseUp => {
                if self.spec.bobber {
                    self.bobber_mouse_up(ev);
                }
                Verdict::Pass
            }
            CGEventType::RightMouseDown => {
                self.any_mouse_down(CGEvent::location(Some(ev)));
                Verdict::Pass
            }
            CGEventType::OtherMouseDown => {
                let at = CGEvent::location(Some(ev));
                let verdict = self.other_mouse(ev, true);
                if matches!(verdict, Verdict::Pass) {
                    self.any_mouse_down(at);
                } else {
                    self.feed_double(TapInput::MouseDown);
                }
                verdict
            }
            CGEventType::OtherMouseUp => self.other_mouse(ev, false),
            CGEventType::ScrollWheel => {
                if self.spec.bobber && self.ctx.bobber.is_visible() {
                    bobber::dismiss(&self.ctx, DismissReason::Scroll);
                }
                Verdict::Pass
            }
            CGEventType::KeyDown => {
                if self.paste_key(ev, true) {
                    return Verdict::Swallow;
                }
                if self.spec.bobber && self.ctx.bobber.is_visible() {
                    bobber::dismiss(&self.ctx, DismissReason::KeyPress);
                }
                self.feed_double(TapInput::OtherKey);
                Verdict::Pass
            }
            CGEventType::KeyUp if self.paste_key(ev, false) => Verdict::Swallow,
            CGEventType::FlagsChanged => {
                self.flags_changed(ev);
                Verdict::Pass
            }
            _ => Verdict::Pass,
        }
    }

    /// 连续粘贴：这个按键是不是要拦下的粘贴键（按下时比较键码与修饰位；拦下的那个键的自动重复与抬起同样拦下）。
    /// 只有第一次按下发出 `PasteKey`
    fn paste_key(&self, ev: &CGEvent, down: bool) -> bool {
        if self.paste.is_empty() {
            return false;
        }
        let keycode = u16::try_from(CGEvent::integer_value_field(
            Some(ev),
            CGEventField::KeyboardEventKeycode,
        ))
        .unwrap_or(u16::MAX);
        if self.paste_held.get() == Some(keycode) {
            if !down {
                self.paste_held.set(None);
            }
            return true;
        }
        if !down {
            return false;
        }
        let bits =
            CGEvent::flags(Some(ev)).0 & (FLAG_SHIFT_BIT | FLAG_CONTROL_BIT | FLAG_ALTERNATE_BIT | FLAG_COMMAND_BIT);
        let Some(hit) = self.paste.iter().find(|k| k.keycode == keycode && k.flags == bits) else {
            return false;
        };
        self.paste_held.set(Some(keycode));
        // 按住不放时系统的自动重复：拦下但不再发信号（上面的 paste_held 已经接住它们，这里只是第一次按下）
        if CGEvent::integer_value_field(Some(ev), CGEventField::KeyboardEventAutorepeat) == 0 {
            self.emit(InputSignal::PasteKey { chord: hit.chord });
        }
        true
    }

    /// 浮标判定的同步部分：位移、连击、⌘ / Ctrl、是否落在 Wisp 窗口内
    fn bobber_mouse_up(&self, ev: &CGEvent) {
        let up = CGEvent::location(Some(ev));
        let Some(down) = self.left_down.take() else { return };
        let flags = CGEvent::flags(Some(ev)).0;
        let mut facts = MouseUpFacts {
            drag_px: (up.x - down.x).hypot(up.y - down.y),
            click_count: u32::try_from(CGEvent::integer_value_field(
                Some(ev),
                CGEventField::MouseEventClickState,
            ))
            .unwrap_or(1),
            command_or_control: flags & (FLAG_COMMAND | FLAG_CONTROL) != 0,
            in_own_window: false,
        };
        // 先做不需要显示器信息的判定：普通单击（最常见的 mouse-up）、按着 ⌘ / Ctrl 的在这里就结束，不查显示器与 Wisp
        // 窗口（实测这条路径从 p50 约 9 µs 降到约 0.1 µs）
        if !gesture::is_bobber_candidate(&facts) {
            return;
        }
        let (monitor, local) = displays::locate(up);
        facts.in_own_window = self.ctx.bobber.contains(&monitor, local) || (self.ctx.self_hit)(&monitor, local);
        if gesture::is_bobber_candidate(&facts) {
            self.candidate_at.set(Some(Instant::now()));
            self.emit(InputSignal::BobberCandidate { at: (monitor, local) });
        }
    }

    fn flags_changed(&self, ev: &CGEvent) {
        let mut guard = self.double.borrow_mut();
        let Some((machine, key)) = guard.as_mut() else { return };
        let keycode = u16::try_from(CGEvent::integer_value_field(
            Some(ev),
            CGEventField::KeyboardEventKeycode,
        ))
        .unwrap_or(u16::MAX);
        let now = Instant::now();
        let input = match classify_flags_changed(keycode, CGEvent::flags(Some(ev)).0, *key) {
            FlagsEvent::TargetDown => {
                // 两次按下之间是否有鼠标按下
                if let Some(t1) = machine.pending_since()
                    && mouse_down_since(now.saturating_duration_since(t1))
                {
                    machine.feed(TapInput::MouseDown);
                }
                TapInput::TargetDown(now)
            }
            FlagsEvent::TargetUp => TapInput::TargetUp(now),
            FlagsEvent::Other => TapInput::OtherKey,
        };
        let fired = machine.feed(input);
        drop(guard);
        if fired {
            self.emit(InputSignal::DoubleTap);
        }
    }

    /// 额外鼠标键
    fn other_mouse(&self, ev: &CGEvent, down: bool) -> Verdict {
        let Some(spec) = self.spec.mouse else {
            return Verdict::Pass;
        };
        let number = CGEvent::integer_value_field(Some(ev), CGEventField::MouseEventButtonNumber);
        let Some(kind) = button_kind(number) else {
            return Verdict::Pass;
        };
        let idx = button_index(kind);
        let now = Instant::now();
        if !down {
            let mut buttons = self.buttons.borrow_mut();
            let state = &mut buttons[idx];
            if spec.long_press {
                let pending = state.pending_down.take();
                return match state.long.up(now) {
                    LongPressUp::PassThrough => Verdict::Pass,
                    LongPressUp::Swallow => Verdict::Swallow,
                    LongPressUp::FireAndSwallow => {
                        drop(buttons);
                        if let Some(action) = gesture::mouse_action(&spec, kind) {
                            self.emit(InputSignal::MouseAction { button: kind, action });
                        }
                        Verdict::Swallow
                    }
                    LongPressUp::Replay => {
                        // 未达阈值：按原顺序重新注入按下与抬起（带标记，自己的 tap 会放行）
                        if let Some(down) = pending {
                            replay(&down);
                            if let Some(up) = CGEvent::new_copy(Some(ev)) {
                                replay(&up);
                            }
                        }
                        Verdict::Swallow
                    }
                };
            }
            return if std::mem::take(&mut state.intercepted) {
                Verdict::Swallow
            } else {
                Verdict::Pass
            };
        }
        let Some(action) = gesture::mouse_action(&spec, kind) else {
            return Verdict::Pass;
        };
        if kind == MouseButtonKind::Middle && !middle_should_intercept(ev, self.ctx.own_pid) {
            return Verdict::Pass;
        }
        let mut buttons = self.buttons.borrow_mut();
        let state = &mut buttons[idx];
        if spec.long_press {
            state.long.down(now);
            state.pending_down = CGEvent::new_copy(Some(ev));
            drop(buttons);
            self.arm_timer(LONG_PRESS);
            return Verdict::Swallow;
        }
        state.intercepted = true;
        drop(buttons);
        self.emit(InputSignal::MouseAction { button: kind, action });
        Verdict::Swallow
    }

    fn arm_timer(&self, after: Duration) {
        // SAFETY: timer 指针在 tap 线程存活期间有效（线程退出前才释放）
        if let Some(timer) = unsafe { self.timer.get().as_ref() } {
            timer.set_next_fire_date(CFAbsoluteTimeGetCurrent() + after.as_secs_f64());
        }
    }

    /// 长按定时器到点
    fn on_timer(&self) {
        let Some(spec) = self.spec.mouse else { return };
        let now = Instant::now();
        let mut fired = Vec::new();
        let mut next: Option<Duration> = None;
        {
            let mut buttons = self.buttons.borrow_mut();
            for (idx, kind) in [MouseButtonKind::Middle, MouseButtonKind::Back, MouseButtonKind::Forward]
                .into_iter()
                .enumerate()
            {
                let state = &mut buttons[idx];
                if state.long.tick(now) {
                    state.pending_down = None;
                    fired.push(kind);
                } else if let Some(left) = state.long.remaining(now) {
                    next = Some(next.map_or(left, |n| n.min(left)));
                }
            }
        }
        for kind in fired {
            if let Some(action) = gesture::mouse_action(&spec, kind) {
                self.emit(InputSignal::MouseAction { button: kind, action });
            }
        }
        match next {
            Some(left) => self.arm_timer(left),
            None => self.arm_timer(Duration::from_secs_f64(FAR_FUTURE)),
        }
    }
}

/// 某段时间内是否发生过鼠标按下（左 / 右 / 其他键）
fn mouse_down_since(window: Duration) -> bool {
    let secs = window.as_secs_f64();
    [
        CGEventType::LeftMouseDown,
        CGEventType::RightMouseDown,
        CGEventType::OtherMouseDown,
    ]
    .into_iter()
    .any(|t| CGEventSource::seconds_since_last_event_type(CGEventSourceStateID::HIDSystemState, t) < secs)
}

/// 中键是否应该拦截：选区非空且光标不在链接上，判定上限 30 ms（从回调开始计，含查询光标下窗口的属主），
/// 超时一律放行。
///
/// 选区与链接都在同一个进程上判定：光标下窗口的属主（取不到时用前台应用）。属主不是前台应用时放行——要翻译的是前台
/// 应用的选区，在后台窗口上按中键与它无关（例如在后台浏览器窗口的链接上按中键开新标签）；落在 Wisp 自己的窗口上也放行
fn middle_should_intercept(ev: &CGEvent, own_pid: i32) -> bool {
    let started = Instant::now();
    let at = CGEvent::location(Some(ev));
    let left = || MIDDLE_CLICK_BUDGET.saturating_sub(started.elapsed());
    pool(|| {
        let front = frontmost_pid();
        // WindowServer 往返一次（只查这一个窗口），计入预算
        let owner = window_owner_pid(ev);
        let pid = match (owner, front) {
            (Some(o), Some(f)) if o != f => return false,
            (o, f) => match o.or(f) {
                Some(p) if p != own_pid => p,
                _ => return false,
            },
        };
        let budget = left();
        if budget.is_zero() || ax::has_selection_of(Some(pid), budget) != Some(true) {
            return false;
        }
        let budget = left();
        !budget.is_zero() && ax::link_at(at, budget, Some(pid)) == Some(false)
    })
}

/// 鼠标事件发生时光标下窗口的属主进程：WindowServer 在鼠标事件上标注了窗口号，按窗口号查一次窗口信息
/// （只查这一个窗口，开销很小）。查不到时为 None，由调用方改用前台应用
fn window_owner_pid(ev: &CGEvent) -> Option<i32> {
    let window = [
        CGEventField::MouseEventWindowUnderMousePointerThatCanHandleThisEvent,
        CGEventField::MouseEventWindowUnderMousePointer,
    ]
    .into_iter()
    .map(|field| CGEvent::integer_value_field(Some(ev), field))
    .find(|w| *w > 0)?;
    let window = CGWindowID::try_from(window).ok()?;
    let list = CGWindowListCopyWindowInfo(CGWindowListOption::OptionIncludingWindow, window)?;
    // SAFETY: 窗口信息数组的元素都是 CFDictionary
    let list = unsafe { list.cast_unchecked::<CFDictionary>() };
    let info = list.get(0)?;
    // SAFETY: 窗口信息字典的键是 CFString、值是 CF 对象；键常量由系统导出
    let (info, key) = unsafe { (info.cast_unchecked::<CFString, CFType>(), kCGWindowOwnerPID) };
    let pid = info.get(key)?.downcast_ref::<CFNumber>()?.as_i64()?;
    i32::try_from(pid).ok().filter(|p| *p > 0)
}

/// 重新注入一个事件（带 Wisp 标记）
fn replay(ev: &CGEvent) {
    CGEvent::set_integer_value_field(Some(ev), CGEventField::EventSourceUserData, i64::from(WISP_INPUT_TAG));
    CGEvent::post(CGEventTapLocation::SessionEventTap, Some(ev));
}

unsafe extern "C-unwind" fn tap_callback(
    _proxy: CGEventTapProxy,
    etype: CGEventType,
    event: NonNull<CGEvent>,
    user: *mut c_void,
) -> *mut CGEvent {
    // SAFETY: user 是 tap 线程上的 Box<TapState>，在 tap 注销之后才释放
    let state = unsafe { &*user.cast::<TapState>() };
    if etype == CGEventType::TapDisabledByTimeout || etype == CGEventType::TapDisabledByUserInput {
        let n = state.disabled_count.get() + 1;
        state.disabled_count.set(n);
        // 几个 tap 共用这个回调：把被禁用的重新 enable；浮标按键 tap 只在浮标可见时才应该启用
        let reenable = guarded("input tap re-enable", || {
            for (slot, tap) in state.taps.get().into_iter().enumerate() {
                // SAFETY: tap 指针在线程存活期间有效（注销之后才清空状态）
                let Some(tap) = (unsafe { tap.as_ref() }) else { continue };
                let wanted = slot != SLOT_KEYS || state.ctx.bobber.is_visible();
                if wanted && !CGEvent::tap_is_enabled(tap) {
                    CGEvent::tap_enable(tap, true);
                }
            }
        });
        if reenable.is_some() {
            log::warn!("input tap disabled by system ({etype:?}), re-enabled (#{n})");
        }
        return event.as_ptr();
    }
    // SAFETY: 事件在回调期间有效
    let ev = unsafe { event.as_ref() };
    if CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUserData) == i64::from(WISP_INPUT_TAG) {
        return event.as_ptr();
    }
    // panic 时放行事件
    match guarded("input tap callback", || state.handle(etype, ev)) {
        Some(Verdict::Swallow) => std::ptr::null_mut(),
        Some(Verdict::Pass) | None => event.as_ptr(),
    }
}

unsafe extern "C-unwind" fn timer_callback(_timer: *mut CFRunLoopTimer, info: *mut c_void) {
    // SAFETY: info 是同一线程上的 TapState
    let state = unsafe { &*info.cast::<TapState>() };
    guarded("input tap timer", || state.on_timer());
}

/// 三个 tap 各自的掩码（0 表示不需要这个 tap）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TapPlan {
    /// Default（主动过滤）：只有需要吞掉的额外鼠标键
    active: CGEventMask,
    /// ListenOnly：浮标与双击需要的其余事件
    listen: CGEventMask,
    /// ListenOnly，平时停用：浮标可见期间的「任意按键消失」
    keys: CGEventMask,
    /// Default（主动过滤）：连续粘贴期间的 keyDown / keyUp（拦下粘贴键）
    paste: CGEventMask,
}

impl TapPlan {
    /// 按槽位排列的 (模式, 掩码)
    fn masks(&self) -> [(CGEventTapOptions, CGEventMask); SLOTS] {
        let mut out = [(CGEventTapOptions::ListenOnly, 0); SLOTS];
        out[SLOT_ACTIVE] = (CGEventTapOptions::Default, self.active);
        out[SLOT_LISTEN] = (CGEventTapOptions::ListenOnly, self.listen);
        out[SLOT_KEYS] = (CGEventTapOptions::ListenOnly, self.keys);
        out[SLOT_PASTE] = (CGEventTapOptions::Default, self.paste);
        out
    }

    fn is_empty(&self) -> bool {
        self.active == 0 && self.listen == 0 && self.keys == 0 && self.paste == 0
    }
}

/// 计算各 tap 的掩码。`monitoring`：是否已有输入监控权限（浮标的按键消失只在有权限时生效）
fn plan(spec: &HookSpec, monitoring: bool) -> TapPlan {
    let key_down = mask_of(CGEventType::KeyDown);
    let intercept = mouse_bound(spec);
    let mut active: CGEventMask = 0;
    let mut listen: CGEventMask = 0;
    let mut keys: CGEventMask = 0;
    if intercept {
        active |= mask_of(CGEventType::OtherMouseDown) | mask_of(CGEventType::OtherMouseUp);
    }
    if spec.bobber {
        listen |= mask_of(CGEventType::LeftMouseDown)
            | mask_of(CGEventType::LeftMouseUp)
            | mask_of(CGEventType::RightMouseDown)
            | mask_of(CGEventType::ScrollWheel);
        // 有鼠标键拦截时，额外键的按下已经经过 Default tap（放行时在那里做浮标消失判定），不重复订阅
        if !intercept {
            listen |= mask_of(CGEventType::OtherMouseDown);
        }
    }
    if spec.double_tap.is_some() {
        listen |= mask_of(CGEventType::FlagsChanged) | key_down;
    }
    let mut paste: CGEventMask = 0;
    if !spec.paste_keys.is_empty() {
        // keyDown 改由连续粘贴的 tap 送达（没吞掉的照常做浮标 / 双击判定），同一个事件不经过两个 tap
        paste = key_down | mask_of(CGEventType::KeyUp);
        listen &= !key_down;
    }
    if spec.bobber && monitoring && listen & key_down == 0 && paste == 0 {
        keys = key_down;
    }
    TapPlan {
        active,
        listen,
        keys,
        paste,
    }
}

/// tap 创建失败时的原因归类：只有确实订阅了键盘事件的 tap 才归因到输入监控
fn creation_error(options: CGEventTapOptions, mask: CGEventMask) -> HookError {
    let keyboard = mask & (mask_of(CGEventType::KeyDown) | mask_of(CGEventType::FlagsChanged)) != 0;
    if keyboard && input_monitoring() != PermissionState::Granted {
        return HookError::PermissionMissing(PermissionKind::InputMonitoring);
    }
    if !ax::trusted() {
        return HookError::PermissionMissing(PermissionKind::Accessibility);
    }
    let mode = if options == CGEventTapOptions::Default {
        "default"
    } else {
        "listen-only"
    };
    HookError::Other(format!("CGEventTapCreate failed ({mode})"))
}

/// tap 线程上安装好的资源。drop（在 tap 线程上、run loop 停止之后）时先注销全部 tap 与定时器，最后释放状态
struct Installed {
    run_loop: CFRetained<CFRunLoop>,
    taps: Vec<(CFRetained<CFMachPort>, CFRetained<CFRunLoopSource>)>,
    timer: Option<CFRetained<CFRunLoopTimer>>,
    /// 回调与定时器的上下文；字段按声明顺序在 drop 函数之后释放，它排在最后
    state: Box<TapState>,
}

impl Drop for Installed {
    fn drop(&mut self) {
        // SAFETY: 系统导出的模式常量
        let common = unsafe { kCFRunLoopCommonModes };
        for (tap, _) in &self.taps {
            CGEvent::tap_enable(tap, false);
        }
        if let Some(timer) = self.timer.as_deref() {
            timer.invalidate();
        }
        for (tap, source) in &self.taps {
            self.run_loop.remove_source(Some(source), common);
            source.invalidate();
            tap.invalidate();
        }
        self.state.taps.set([std::ptr::null(); SLOTS]);
        let disabled = self.state.disabled_count.get();
        if disabled > 0 {
            log::info!("input tap stopped; it was disabled by the system {disabled} time(s)");
        }
    }
}

/// 在 tap 线程上建 tap、定时器并挂到 run loop（任何一步失败时，已建好的部分随 `Installed` 的 drop 注销）。
/// 浮标按键 tap 建好后保持停用，经 `key_tap` 交给安装方，安装成功后登记到浮标的共享状态
fn install_taps(
    run_loop: &CFRetained<CFRunLoop>,
    ctx: Arc<Ctx>,
    spec: HookSpec,
    sink: Arc<dyn Fn(InputSignal) + Send + Sync>,
    plan: TapPlan,
    paste: Vec<PasteKeyCode>,
    key_tap: &Mutex<Option<KeyTap>>,
) -> Result<Installed, HookError> {
    let state = Box::new(TapState {
        ctx,
        spec,
        sink,
        taps: Cell::new([std::ptr::null(); SLOTS]),
        timer: Cell::new(std::ptr::null()),
        left_down: Cell::new(None),
        candidate_at: Cell::new(None),
        double: RefCell::new(spec.double_tap.map(|d| (DoubleTapMachine::new(d.interval), d.key))),
        buttons: RefCell::new(Default::default()),
        disabled_count: Cell::new(0),
        paste,
        paste_held: Cell::new(None),
    });
    let state_ptr = (&raw const *state).cast_mut().cast::<c_void>();
    let mut installed = Installed {
        run_loop: run_loop.clone(),
        taps: Vec::with_capacity(SLOTS),
        timer: None,
        state,
    };
    // SAFETY: 系统导出的模式常量
    let common = unsafe { kCFRunLoopCommonModes };
    let mut ptrs = [std::ptr::null(); SLOTS];
    for (slot, (options, mask)) in plan.masks().into_iter().enumerate() {
        if mask == 0 {
            continue;
        }
        // SAFETY: 回调签名正确；state 在 tap 注销之后才释放
        let created = unsafe {
            CGEvent::tap_create(
                CGEventTapLocation::SessionEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                options,
                mask,
                Some(tap_callback),
                state_ptr,
            )
        };
        let tap = match created {
            Some(tap) => tap,
            // 浮标按键 tap 只是锦上添花：建不起来（例如输入监控在预检之后被撤销）时不影响其余功能
            None if slot == SLOT_KEYS => {
                log::info!("bobber key tap unavailable; key presses will not dismiss the bobber");
                continue;
            }
            None => return Err(creation_error(options, mask)),
        };
        let Some(source) = CFMachPort::new_run_loop_source(None, Some(&tap), 0) else {
            tap.invalidate();
            return Err(HookError::Other("failed to attach event tap to run loop".into()));
        };
        run_loop.add_source(Some(&source), common);
        ptrs[slot] = &raw const *tap;
        if slot == SLOT_KEYS {
            // tap 创建后默认是启用的：先停用，浮标可见时才启用
            CGEvent::tap_enable(&tap, false);
            *key_tap.lock() = Some(KeyTap(tap.clone()));
        }
        installed.taps.push((tap, source));
    }
    installed.state.taps.set(ptrs);
    // 长按定时器：常驻但空闲时推到远期，不产生唤醒
    if mouse_bound(&spec) && spec.mouse.is_some_and(|m| m.long_press) {
        let mut timer_ctx = CFRunLoopTimerContext {
            version: 0,
            info: state_ptr,
            retain: None,
            release: None,
            copyDescription: None,
        };
        // SAFETY: 回调签名正确；context 被复制进定时器，info 指向的状态在定时器失效之后才释放
        let timer = unsafe {
            CFRunLoopTimer::new(
                None,
                CFAbsoluteTimeGetCurrent() + FAR_FUTURE,
                FAR_FUTURE,
                0,
                0,
                Some(timer_callback),
                &mut timer_ctx,
            )
        }
        .ok_or_else(|| HookError::Other("failed to create long-press timer".into()))?;
        installed.state.timer.set(&raw const *timer);
        run_loop.add_timer(Some(&timer), common);
        installed.timer = Some(timer);
    }
    // 浮标按键 tap 保持停用，其余的启用
    for (slot, tap) in ptrs.into_iter().enumerate() {
        if slot == SLOT_KEYS {
            continue;
        }
        // SAFETY: 指针指向 installed.taps 里仍然存活的 tap（或为空）
        if let Some(tap) = unsafe { tap.as_ref() } {
            CGEvent::tap_enable(tap, true);
        }
    }
    Ok(installed)
}

impl InputHooks for MacHooks {
    fn install(&self, spec: HookSpec, sink: Box<dyn Fn(InputSignal) + Send + Sync>) -> Result<HookHandle, HookError> {
        let pasting = !spec.paste_keys.is_empty();
        // 鼠标规格里一个键都没绑定时等于没有鼠标项
        if !spec.bobber && spec.double_tap.is_none() && !mouse_bound(&spec) && !pasting {
            return Ok(HookHandle::noop());
        }
        // 权限：鼠标键拦截与粘贴键拦截（Default tap）、浮标（AX 取词）需要辅助功能，双击修饰键需要输入监控
        if (mouse_bound(&spec) || spec.bobber || pasting) && !ax::trusted() {
            return Err(HookError::PermissionMissing(PermissionKind::Accessibility));
        }
        let paste = resolve_paste_keys(&self.ctx, &spec.paste_keys);
        if pasting && paste.is_empty() {
            return Err(HookError::Other(
                "no paste key resolvable in the current keyboard layout".into(),
            ));
        }
        let monitoring = input_monitoring() == PermissionState::Granted;
        if spec.double_tap.is_some() && !monitoring {
            return Err(HookError::PermissionMissing(PermissionKind::InputMonitoring));
        }
        let plan = plan(&spec, monitoring);
        if plan.is_empty() {
            return Ok(HookHandle::noop());
        }
        let sink: Arc<dyn Fn(InputSignal) + Send + Sync> = Arc::from(sink);
        let tap_ctx = self.ctx.clone();
        let tap_sink = sink.clone();
        let key_tap: Arc<Mutex<Option<KeyTap>>> = Arc::new(Mutex::new(None));
        let tap_key_slot = key_tap.clone();
        let thread = LoopThread::spawn("wisp-input-tap", READY_WAIT, move |run_loop| {
            install_taps(run_loop, tap_ctx, spec, tap_sink, plan, paste, &tap_key_slot)
        })?;
        // 锁屏即结束连续粘贴：观察者随 Handle 一起注销
        let locked = pasting
            .then(|| {
                let lock_sink = sink.clone();
                DistributedObserver::new(
                    self.ctx.main.clone(),
                    &[SCREEN_LOCKED],
                    Arc::new(move || lock_sink(InputSignal::SessionLocked)),
                )
            })
            .flatten();
        // 安装成功之后才接管浮标的消失回报与按键 tap：安装失败时，仍在工作的旧钩子保持原样（包括它对浮标窗口的所有权）
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if spec.bobber {
            let reporter_sink = sink.clone();
            self.ctx.bobber.set_reporter(
                id,
                Arc::new(move |reason| reporter_sink(InputSignal::BobberDismiss(reason))),
            );
            self.ctx.bobber.set_key_tap(id, key_tap.lock().take());
        }
        log::info!(
            "input tap installed: bobber={} double_tap={} mouse={} paste_keys={} default_mask={:#x} listen_mask={:#x} key_mask={:#x}",
            spec.bobber,
            spec.double_tap.is_some(),
            mouse_bound(&spec),
            pasting,
            plan.active,
            plan.listen,
            plan.keys
        );
        let ctx = self.ctx.clone();
        Ok(HookHandle::new(move || {
            drop(locked);
            if spec.bobber {
                // 先交出按键 tap（之后 tap 线程才注销它），再停线程
                ctx.bobber.clear_key_tap(id);
            }
            thread.stop(STOP_WAIT);
            if spec.bobber && ctx.bobber.clear_reporter(id) {
                // 浮标功能关闭：销毁原生窗口（隐藏状态零开销，功能关闭时进程中没有这个窗口）
                bobber::teardown(&ctx);
            }
            log::info!("input tap removed");
        }))
    }

    fn replay_paste(&self, chord: KeyChord) -> Result<(), HookError> {
        // 带 Wisp 标记、只带组合键要求的修饰位（不发修饰键自己的按下 / 抬起）：用户仍按着的 ⌘ 不受影响
        keyboard::post_combo(&self.ctx, &chord.to_combo()).map_err(HookError::Other)
    }
}

#[cfg(test)]
mod tests {
    use wisp_types::{HotkeyAction, ModifierKey};

    use super::*;
    use crate::types::{DoubleTapSpec, MouseSpec};

    fn has(mask: CGEventMask, t: CGEventType) -> bool {
        mask & mask_of(t) != 0
    }

    fn mouse(middle: bool, back: bool, long_press: bool) -> MouseSpec {
        MouseSpec {
            middle: middle.then_some(HotkeyAction::TranslateSelection),
            back: back.then_some(HotkeyAction::TranslateSelection),
            forward: None,
            long_press,
        }
    }

    #[test]
    fn plan_keeps_listeners_out_of_the_active_tap() {
        let spec = HookSpec {
            bobber: true,
            double_tap: Some(DoubleTapSpec {
                key: ModifierKey::RightAlt,
                interval: Duration::from_millis(300),
            }),
            mouse: Some(mouse(true, false, false)),
            ..HookSpec::default()
        };
        let p = plan(&spec, true);
        // 主动过滤器只看额外鼠标键
        assert_eq!(
            p.active,
            mask_of(CGEventType::OtherMouseDown) | mask_of(CGEventType::OtherMouseUp)
        );
        for t in [
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
            CGEventType::RightMouseDown,
            CGEventType::ScrollWheel,
            CGEventType::KeyDown,
            CGEventType::FlagsChanged,
        ] {
            assert!(has(p.listen, t), "{t:?} should be listen-only");
            assert!(!has(p.active, t), "{t:?} must not pass the active filter");
        }
        // 双击已经订阅 keyDown：浮标的按键消失共用它，不另建按键 tap
        assert_eq!(p.keys, 0);
        assert_eq!(p.active & p.listen, 0, "no event type is delivered twice");
    }

    #[test]
    fn bobber_keys_live_in_their_own_on_demand_tap() {
        let bobber = HookSpec {
            bobber: true,
            ..HookSpec::default()
        };
        let p = plan(&bobber, true);
        assert_eq!(p.active, 0, "bobber never needs an active tap");
        assert!(has(p.listen, CGEventType::OtherMouseDown));
        // 常驻的 listen tap 不收按键；按键只进浮标可见时才启用的 tap
        assert!(!has(p.listen, CGEventType::KeyDown), "no resident keyboard listener");
        assert_eq!(p.keys, mask_of(CGEventType::KeyDown));
        // 没有输入监控时这一条不生效，也就不建按键 tap
        assert_eq!(plan(&bobber, false).keys, 0);
    }

    #[test]
    fn unbound_mouse_spec_subscribes_nothing() {
        let unbound = HookSpec {
            mouse: Some(mouse(false, false, true)),
            ..HookSpec::default()
        };
        assert!(!mouse_bound(&unbound));
        assert!(plan(&unbound, true).is_empty(), "no buttons bound: no tap at all");
        // 与浮标同开时，额外键的按下仍在 listen tap 里（只用于浮标消失），不建 Default tap
        let with_bobber = HookSpec {
            bobber: true,
            mouse: Some(mouse(false, false, false)),
            ..HookSpec::default()
        };
        let p = plan(&with_bobber, false);
        assert_eq!(p.active, 0);
        assert!(has(p.listen, CGEventType::OtherMouseDown));
        let bound = HookSpec {
            mouse: Some(mouse(false, true, true)),
            ..HookSpec::default()
        };
        let p = plan(&bound, true);
        assert_eq!(p.listen, 0, "mouse buttons alone need no listen-only tap");
        assert_eq!(p.keys, 0);
        assert_ne!(p.active, 0);
    }

    #[test]
    fn paste_keys_take_key_down_into_their_own_active_tap() {
        use crate::common::combo::{Combo, KeyChord};
        let chord = KeyChord::of(&Combo::parse("Super+KeyV").expect("combo")).expect("chord");
        let only = HookSpec {
            paste_keys: PasteKeys::new([chord]),
            ..HookSpec::default()
        };
        let p = plan(&only, true);
        assert_eq!(p.paste, mask_of(CGEventType::KeyDown) | mask_of(CGEventType::KeyUp));
        assert_eq!((p.active, p.listen, p.keys), (0, 0, 0), "nothing else is subscribed");
        // 与双击修饰键同装：keyDown 只经粘贴键的 tap 送达（不吞的照常做双击判定），listen tap 只留 flagsChanged
        let both = HookSpec {
            double_tap: Some(DoubleTapSpec {
                key: ModifierKey::RightAlt,
                interval: Duration::from_millis(300),
            }),
            bobber: true,
            ..only
        };
        let p = plan(&both, true);
        assert!(!has(p.listen, CGEventType::KeyDown));
        assert!(has(p.listen, CGEventType::FlagsChanged));
        assert_eq!(p.keys, 0, "the paste tap already sees every keyDown");
        assert!(has(p.paste, CGEventType::KeyDown));
        // 修饰位只看四个设备无关位
        assert_eq!(
            modifier_bits(&chord.modifiers),
            FLAG_COMMAND_BIT,
            "⌘V compares the command bit only"
        );
    }

    /// 扩展触发的开销测量：tap 回调里 Rust 侧的处理耗时（不含 WindowServer 把事件投递给 tap 的时间）。
    /// 三项全开（浮标 + 双击修饰键 + 「后退」键拦截），用合成事件（`CGEventCreate*`：只创建、不投递，不需要任何权限）
    /// 直接喂给 `TapState::handle`——与真实回调走同一段代码（含显示器定位、Wisp 窗口命中、sink 投递），按事件类型统计
    /// P50 / P99 / 最大值。手动运行：
    /// `cargo test -p wisp-platform --release --lib macos::hooks::tests::callback_cost -- --ignored --nocapture`
    #[test]
    #[ignore = "测量用（扩展触发的回调耗时），手动运行"]
    #[allow(clippy::print_stderr)]
    fn callback_cost() {
        use objc2_core_foundation::CGPoint;
        use objc2_core_graphics::{CGEventFlags, CGMouseButton, CGScrollEventUnit};

        let ctx = Arc::new(Ctx {
            own_pid: i32::try_from(std::process::id()).unwrap_or(i32::MAX),
            main: super::super::util::MainThread::new(Arc::new(|f: Box<dyn FnOnce() + Send>| f())),
            self_hit: Arc::new(|_, _| false),
            keymaps: Arc::default(),
            focus: super::super::focus::FocusState::default(),
            clip: super::super::pasteboard::ClipState::default(),
            layout: super::super::keyboard::LayoutCache::default(),
            selection: super::super::selection::SelectionState::default(),
            monitors: super::super::displays::MonitorCache::default(),
            permissions: super::super::permissions::PermState::default(),
            bobber: super::super::bobber::BobberShared::default(),
        });
        let (tx, rx) = std::sync::mpsc::channel::<InputSignal>();
        let tx = Mutex::new(tx);
        let spec = HookSpec {
            bobber: true,
            double_tap: Some(DoubleTapSpec {
                key: ModifierKey::RightAlt,
                interval: Duration::from_millis(300),
            }),
            mouse: Some(mouse(false, true, false)),
            ..HookSpec::default()
        };
        let state = TapState {
            ctx,
            spec,
            sink: Arc::new(move |sig| {
                let _ = tx.lock().send(sig);
            }),
            taps: Cell::new([std::ptr::null(); SLOTS]),
            timer: Cell::new(std::ptr::null()),
            left_down: Cell::new(None),
            candidate_at: Cell::new(None),
            double: RefCell::new(spec.double_tap.map(|d| (DoubleTapMachine::new(d.interval), d.key))),
            buttons: RefCell::new(Default::default()),
            disabled_count: Cell::new(0),
            paste: Vec::new(),
            paste_held: Cell::new(None),
        };
        let mouse_ev = |t: CGEventType, x: f64, button: CGMouseButton| {
            CGEvent::new_mouse_event(None, t, CGPoint::new(x, 400.0), button).expect("mouse event")
        };
        let down = mouse_ev(CGEventType::LeftMouseDown, 300.0, CGMouseButton::Left);
        let up = mouse_ev(CGEventType::LeftMouseUp, 420.0, CGMouseButton::Left);
        let right = mouse_ev(CGEventType::RightMouseDown, 300.0, CGMouseButton::Right);
        let back_down = mouse_ev(CGEventType::OtherMouseDown, 300.0, CGMouseButton::Center);
        CGEvent::set_integer_value_field(Some(&back_down), CGEventField::MouseEventButtonNumber, 3);
        let back_up = mouse_ev(CGEventType::OtherMouseUp, 300.0, CGMouseButton::Center);
        CGEvent::set_integer_value_field(Some(&back_up), CGEventField::MouseEventButtonNumber, 3);
        let scroll = CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Pixel, 1, -3, 0, 0).expect("scroll");
        let key = CGEvent::new_keyboard_event(None, 0, true).expect("key");
        // 右 Option（keycode 61）按下 / 抬起
        let ralt_down = CGEvent::new_keyboard_event(None, 61, true).expect("flags");
        CGEvent::set_type(Some(&ralt_down), CGEventType::FlagsChanged);
        CGEvent::set_flags(Some(&ralt_down), CGEventFlags(0x0008_0000 | 0x40));
        let ralt_up = CGEvent::new_keyboard_event(None, 61, false).expect("flags");
        CGEvent::set_type(Some(&ralt_up), CGEventType::FlagsChanged);
        CGEvent::set_flags(Some(&ralt_up), CGEventFlags(0));

        let click_up = mouse_ev(CGEventType::LeftMouseUp, 301.0, CGMouseButton::Left);
        let cases: [(&str, Vec<(CGEventType, &CGEvent)>); 6] = [
            (
                "浮标：左键按下 + 拖选抬起（判定并发出候选）",
                vec![(CGEventType::LeftMouseDown, &down), (CGEventType::LeftMouseUp, &up)],
            ),
            (
                "浮标：普通单击（不是候选）",
                vec![
                    (CGEventType::LeftMouseDown, &down),
                    (CGEventType::LeftMouseUp, &click_up),
                ],
            ),
            (
                "右键按下 / 滚轮",
                vec![
                    (CGEventType::RightMouseDown, &right),
                    (CGEventType::ScrollWheel, &scroll),
                ],
            ),
            (
                "双击修饰键：右 Option 按下 / 抬起",
                vec![
                    (CGEventType::FlagsChanged, &ralt_down),
                    (CGEventType::FlagsChanged, &ralt_up),
                ],
            ),
            ("普通按键（双击判定作废）", vec![(CGEventType::KeyDown, &key)]),
            (
                "后退键拦截：按下 / 抬起（吞掉并发出动作）",
                vec![
                    (CGEventType::OtherMouseDown, &back_down),
                    (CGEventType::OtherMouseUp, &back_up),
                ],
            ),
        ];
        const N: usize = 20_000;
        for (name, events) in cases {
            let mut samples: Vec<u128> = Vec::with_capacity(N * events.len());
            for _ in 0..N {
                for (t, ev) in &events {
                    let started = Instant::now();
                    let _ = state.handle(*t, ev);
                    samples.push(started.elapsed().as_nanos());
                }
            }
            while rx.try_recv().is_ok() {}
            samples.sort_unstable();
            let pick = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
            eprintln!(
                "O3 callback {name}: n={} p50={} ns p99={} ns p99.9={} ns max={} ns",
                samples.len(),
                pick(0.5),
                pick(0.99),
                pick(0.999),
                samples[samples.len() - 1]
            );
        }
    }
}
