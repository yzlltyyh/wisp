//! XInput2 钩子的判定逻辑（X11；纯逻辑）。
//!
//! `x11/input.rs` 只负责收事件、查询光标、投递信号；连击计数、按逻辑像素计算位移、浮标候选之后的探测窗口、
//! 按下时浮标是否消失、修饰键追踪、双击修饰键的输入分类、侧键长按的决策都在这里。本文件只依赖
//! std 与 `common/` 的公开类型，`tests/smoke_linux.rs` 用 `#[path]` 引入，在任何平台都能单测。

use std::time::{Duration, Instant};

use crate::common::double_tap::TapInput;
use crate::common::gesture::LongPressUp;

/// X11 的双击判定（400 ms / 4 px，按物理像素比较两次按下的位置）
pub const MULTI_CLICK: Duration = Duration::from_millis(400);
pub const MULTI_CLICK_PX: i32 = 4;

/// 滚轮在 X11 上是按钮 4–7
pub fn is_wheel(button: u32) -> bool {
    (4..=7).contains(&button)
}

/// 左键手势的状态：连击计数、按下位置，以及最近一次发出 `BobberCandidate` 的时刻
#[derive(Debug, Default, Clone)]
pub struct Gesture {
    /// 上一次左键按下：(时刻, x, y, 当次连击计数)
    last_press: Option<(Instant, i32, i32, u32)>,
    /// 当前按下的位置（全局物理坐标）与连击计数；抬起时取走
    down: Option<(i32, i32, u32)>,
    /// 最近一次发出候选的时刻（之后一段时间内的鼠标按下要取消探测）
    candidate_at: Option<Instant>,
}

impl Gesture {
    /// 左键按下（全局物理坐标），返回这是第几击（1 = 新手势的第一次按下）
    pub fn left_down(&mut self, now: Instant, x: i32, y: i32) -> u32 {
        let count = match self.last_press {
            Some((t, lx, ly, c))
                if now.saturating_duration_since(t) <= MULTI_CLICK
                    && (x - lx).abs() <= MULTI_CLICK_PX
                    && (y - ly).abs() <= MULTI_CLICK_PX =>
            {
                c.saturating_add(1)
            }
            _ => 1,
        };
        self.last_press = Some((now, x, y, count));
        self.down = Some((x, y, count));
        count
    }

    /// 左键抬起：返回 (位移，逻辑像素；连击计数)。没有对应的按下（例如钩子装在按住期间）返回 None
    pub fn left_up(&mut self, x: i32, y: i32, scale: f64) -> Option<(f64, u32)> {
        let (dx, dy, count) = self.down.take()?;
        Some((drag_logical((dx, dy), (x, y), scale), count))
    }

    /// 已发出 `BobberCandidate`（上层将在 80 ms 后探测选区）
    pub fn candidate_sent(&mut self, now: Instant) {
        self.candidate_at = Some(now);
    }

    /// 任意非滚轮按键按下：若仍在候选之后的探测窗口内，返回 true（需要发 `BobberDismiss(MouseDown)` 取消探测）。
    /// 记录随之清除，同一次候选最多取消一次
    pub fn press_cancels_probe(&mut self, now: Instant, window: Duration) -> bool {
        self.candidate_at
            .take()
            .is_some_and(|at| now.saturating_duration_since(at) <= window)
    }
}

/// 两点间距离（物理像素）换算成逻辑像素（位移 < 4 pt 才算单击）
pub fn drag_logical(down: (i32, i32), up: (i32, i32), scale: f64) -> f64 {
    let dx = f64::from(up.0 - down.0);
    let dy = f64::from(up.1 - down.1);
    dx.hypot(dy) / scale.max(1.0)
}

/// 鼠标按下对浮标的影响
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressDismiss {
    None,
    /// 浮标可见时滚动
    Scroll,
    /// 在浮标外按下，或候选之后的探测窗口内又按下
    MouseDown,
}

/// 鼠标按下时浮标该不该消失。`on_bobber`：按在浮标的圆盘上（那是点击浮标，不消失）；`probe_pending`：
/// 由 [`Gesture::press_cancels_probe`] 得出。滚轮不取消探测（与 macOS / Windows 一致）
pub fn press_dismiss(button: u32, visible: bool, on_bobber: bool, probe_pending: bool) -> PressDismiss {
    if is_wheel(button) {
        return if visible {
            PressDismiss::Scroll
        } else {
            PressDismiss::None
        };
    }
    if on_bobber {
        return PressDismiss::None;
    }
    if visible || probe_pending {
        PressDismiss::MouseDown
    } else {
        PressDismiss::None
    }
}

/// 当前按住的修饰键（键码）；双击修饰键要求目标键是唯一按下的修饰键
#[derive(Debug, Default, Clone)]
pub struct HeldModifiers(Vec<u8>);

impl HeldModifiers {
    /// 除了 `code` 之外是否还有修饰键按着
    pub fn others_held(&self, code: u8) -> bool {
        self.0.iter().any(|&k| k != code)
    }

    /// 记录一次按键（非修饰键不记录）
    pub fn update(&mut self, code: u8, is_modifier: bool, press: bool) {
        if !is_modifier {
            return;
        }
        if press {
            if !self.0.contains(&code) {
                self.0.push(code);
            }
        } else {
            self.0.retain(|&k| k != code);
        }
    }

    /// 清空（不再有双击修饰键的钩子时）
    pub fn clear(&mut self) {
        self.0.clear();
    }
}

/// 一次键盘事件喂给双击修饰键状态机的输入；None 表示不影响判定（锁定键、非目标键的抬起）。
/// 目标键按下时若还按着别的修饰键，算作「其他键」（Ctrl+Shift 不应触发双击 Ctrl）
pub fn tap_input(is_target: bool, press: bool, others_held: bool, is_lock: bool, now: Instant) -> Option<TapInput> {
    if is_target {
        return Some(match (press, others_held) {
            (true, false) => TapInput::TargetDown(now),
            (true, true) => TapInput::OtherKey,
            (false, _) => TapInput::TargetUp(now),
        });
    }
    // Caps Lock 等锁定键不影响判定（打开 Caps Lock 时也能触发）；其他键只在按下时作废序列
    (press && !is_lock).then_some(TapInput::OtherKey)
}

/// 被动抓取的侧键（8 = Back，9 = Forward）在抓取状态数组里的下标
pub fn side_slot(button: u8) -> Option<usize> {
    match button {
        8 => Some(0),
        9 => Some(1),
        _ => None,
    }
}

/// 侧键事件的处理
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SideAction {
    /// 什么也不做（按键已被抓取吞掉）
    None,
    /// 触发绑定的动作
    Fire,
    /// 长按模式下的按下：开始 400 ms 计时
    StartTimer,
    /// 长按模式下的短按：解除抓取后用 XTest 补发原始点击（浏览器照常后退 / 前进）
    Replay,
}

/// 侧键（被动抓取，按下与抬起都已被吞掉）的决策。`up` 是 `common::gesture::LongPress` 在抬起时的
/// 结果；按下时、或没有进行中的长按时为 None
pub fn side_action(long_press: bool, press: bool, up: Option<LongPressUp>) -> SideAction {
    match (long_press, press) {
        (false, true) => SideAction::Fire,
        (false, false) => SideAction::None,
        (true, true) => SideAction::StartTimer,
        (true, false) => match up {
            Some(LongPressUp::Replay) => SideAction::Replay,
            // 已到阈值但计时线程还没来得及触发：这里补发动作
            Some(LongPressUp::FireAndSwallow) => SideAction::Fire,
            Some(LongPressUp::Swallow | LongPressUp::PassThrough) | None => SideAction::None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROBE: Duration = Duration::from_millis(200);

    fn ms(base: Instant, n: u64) -> Instant {
        base + Duration::from_millis(n)
    }

    #[test]
    fn click_counting_window_and_distance() {
        let t0 = Instant::now();
        let mut g = Gesture::default();
        assert_eq!(g.left_down(t0, 100, 100), 1);
        assert_eq!(g.left_up(100, 100, 1.0), Some((0.0, 1)));
        assert_eq!(g.left_down(ms(t0, 300), 103, 96), 2, "400 ms / 4 px 内算双击");
        assert_eq!(g.left_down(ms(t0, 650), 104, 98), 3, "从上一次按下起算，三击");
        assert_eq!(g.left_down(ms(t0, 1100), 104, 98), 1, "超过 400 ms 重新计数");
        assert_eq!(g.left_down(ms(t0, 1200), 109, 98), 1, "超过 4 px 重新计数");
    }

    #[test]
    fn drag_is_measured_in_logical_pixels() {
        let mut g = Gesture::default();
        g.left_down(Instant::now(), 0, 0);
        // 物理 6 px，缩放 2 → 逻辑 3 px（< 4，算单击）
        assert_eq!(g.left_up(6, 0, 2.0), Some((3.0, 1)));
        assert_eq!(g.left_up(6, 0, 2.0), None, "抬起只消费一次按下");
        assert!((drag_logical((0, 0), (3, 4), 1.0) - 5.0).abs() < 1e-9);
        // 缩放小于 1 的异常值按 1 处理
        assert!((drag_logical((0, 0), (3, 4), 0.0) - 5.0).abs() < 1e-9);
    }

    #[test]
    fn probe_is_cancelled_by_mouse_down_within_window() {
        let t0 = Instant::now();
        let mut g = Gesture::default();
        assert!(!g.press_cancels_probe(t0, PROBE), "没有候选时不取消");
        g.candidate_sent(t0);
        assert!(g.press_cancels_probe(ms(t0, 60), PROBE));
        assert!(!g.press_cancels_probe(ms(t0, 70), PROBE), "同一次候选只取消一次");
        g.candidate_sent(t0);
        assert!(!g.press_cancels_probe(ms(t0, 201), PROBE), "窗口过后的按下与探测无关");
    }

    #[test]
    fn triple_click_cancels_the_double_click_probe() {
        // 第二次抬起发出候选（双击），80 ms 内第三次按下：必须先取消双击的探测，三击抬起后再发新的候选
        let t0 = Instant::now();
        let mut g = Gesture::default();
        g.left_down(t0, 10, 10);
        g.left_up(10, 10, 1.0);
        g.left_down(ms(t0, 120), 10, 10);
        assert_eq!(g.left_up(10, 10, 1.0), Some((0.0, 2)));
        g.candidate_sent(ms(t0, 140));
        let pending = g.press_cancels_probe(ms(t0, 200), PROBE);
        assert_eq!(press_dismiss(1, false, false, pending), PressDismiss::MouseDown);
        assert_eq!(g.left_down(ms(t0, 200), 10, 10), 3);
        assert_eq!(g.left_up(10, 10, 1.0), Some((0.0, 3)));
    }

    #[test]
    fn press_dismiss_rules() {
        // 滚轮：只在浮标可见时让它消失，不取消探测
        assert_eq!(press_dismiss(4, true, false, false), PressDismiss::Scroll);
        assert_eq!(press_dismiss(5, false, false, true), PressDismiss::None);
        // 浮标外按下（任何按键）
        assert_eq!(press_dismiss(1, true, false, false), PressDismiss::MouseDown);
        assert_eq!(press_dismiss(3, true, false, false), PressDismiss::MouseDown);
        // 点在浮标上是点击浮标
        assert_eq!(press_dismiss(1, true, true, true), PressDismiss::None);
        // 浮标还没出现：右键 / 中键 / 左键都取消进行中的探测
        assert_eq!(press_dismiss(3, false, false, true), PressDismiss::MouseDown);
        assert_eq!(press_dismiss(2, false, false, true), PressDismiss::MouseDown);
        assert_eq!(press_dismiss(1, false, false, false), PressDismiss::None);
    }

    #[test]
    fn held_modifiers_and_tap_inputs() {
        let now = Instant::now();
        let (lctrl, rctrl, shift) = (37u8, 105u8, 50u8);
        let mut held = HeldModifiers::default();
        held.update(rctrl, true, true);
        assert!(!held.others_held(rctrl));
        assert_eq!(
            tap_input(true, true, held.others_held(rctrl), false, now),
            Some(TapInput::TargetDown(now))
        );
        // 按住 Shift 再按右 Ctrl：不是「唯一按下」
        held.update(shift, true, true);
        assert!(held.others_held(rctrl));
        assert_eq!(tap_input(true, true, true, false, now), Some(TapInput::OtherKey));
        held.update(shift, true, false);
        held.update(rctrl, true, false);
        assert!(!held.others_held(lctrl));
        // 非修饰键不记录
        held.update(38, false, true);
        assert!(!held.others_held(rctrl));
        // 目标键抬起总是 TargetUp
        assert_eq!(tap_input(true, false, true, false, now), Some(TapInput::TargetUp(now)));
        // 锁定键、其他键的抬起不影响；其他键按下作废序列
        assert_eq!(tap_input(false, true, false, true, now), None);
        assert_eq!(tap_input(false, false, false, false, now), None);
        assert_eq!(tap_input(false, true, false, false, now), Some(TapInput::OtherKey));
        held.update(lctrl, true, true);
        held.clear();
        assert!(!held.others_held(rctrl));
    }

    #[test]
    fn side_button_decisions() {
        assert_eq!(side_slot(8), Some(0));
        assert_eq!(side_slot(9), Some(1));
        assert_eq!(side_slot(2), None);
        // 普通模式：按下即触发，抬起忽略
        assert_eq!(side_action(false, true, None), SideAction::Fire);
        assert_eq!(side_action(false, false, None), SideAction::None);
        // 长按模式：按下开始计时；短按补发原始点击；已触发的长按吞掉；计时线程来不及时补发动作
        assert_eq!(side_action(true, true, None), SideAction::StartTimer);
        assert_eq!(side_action(true, false, Some(LongPressUp::Replay)), SideAction::Replay);
        assert_eq!(side_action(true, false, Some(LongPressUp::Swallow)), SideAction::None);
        assert_eq!(
            side_action(true, false, Some(LongPressUp::FireAndSwallow)),
            SideAction::Fire
        );
        assert_eq!(side_action(true, false, None), SideAction::None);
    }

    #[test]
    fn long_press_end_to_end_with_common_state_machine() {
        use crate::common::gesture::{LONG_PRESS, LongPress};
        let t0 = Instant::now();
        // 短按 → 补发
        let mut m = LongPress::new();
        m.down(t0);
        assert_eq!(
            side_action(true, false, Some(m.up(t0 + LONG_PRESS / 2))),
            SideAction::Replay
        );
        // 长按：计时到点触发，抬起吞掉
        let mut m = LongPress::new();
        m.down(t0);
        assert!(m.tick(t0 + LONG_PRESS));
        assert_eq!(
            side_action(true, false, Some(m.up(t0 + LONG_PRESS * 2))),
            SideAction::None
        );
        // 长按但计时线程还没跑：抬起时补发动作
        let mut m = LongPress::new();
        m.down(t0);
        assert_eq!(
            side_action(true, false, Some(m.up(t0 + LONG_PRESS + Duration::from_millis(5)))),
            SideAction::Fire
        );
    }
}
