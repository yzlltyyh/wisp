//! 浮标判定（位移 / 点击次数 / 修饰键）、浮标几何与消失条件以及鼠标长按逻辑。
//!
//! 全部是纯函数 / 纯状态机，三端的钩子与浮标窗口共用，参数集中在这里，避免各平台各写一份。

use std::time::{Duration, Instant};

use wisp_types::{HotkeyAction, MouseButtonKind, Point, Rect};

use crate::types::MouseSpec;

/// 位移阈值（逻辑 px）：小于它且不是双击 / 三击时不出现浮标
pub const DRAG_THRESHOLD_PX: f64 = 4.0;
/// mouse-up 后延迟多久再读选区
pub const BOBBER_PROBE_DELAY: Duration = Duration::from_millis(80);
/// 读选区必须在 mouse-up 后多久内完成
pub const BOBBER_PROBE_DEADLINE: Duration = Duration::from_millis(120);
/// 鼠标长按阈值
pub const LONG_PRESS: Duration = Duration::from_millis(400);
/// 中键拦截判定（选区 + 链接）在钩子回调里的同步上限，超时一律放行
pub const MIDDLE_CLICK_BUDGET: Duration = Duration::from_millis(30);

/// 浮标圆形直径（逻辑 pt；@2x 为 44 px）
pub const BOBBER_DIAMETER: f64 = 22.0;
/// 浮标窗口边长（逻辑 pt）：圆形四周留出阴影空间
pub const BOBBER_WINDOW: f64 = 34.0;
/// 浮标接收点击的半径（逻辑 pt，从圆心算）：圆盘加上阴影外圈，即窗口的内切圆。窗口其余部分完全透明，点击穿透到
/// 下面的应用；画了像素的地方（圆盘与阴影）都算浮标本身（「图标区域」），不能让阴影上的点击被吞掉
pub const BOBBER_HIT_RADIUS: f64 = BOBBER_WINDOW / 2.0;
/// 浮标相对抬起点的偏移（右下方）
pub const BOBBER_OFFSET: Point = Point { x: 8.0, y: 12.0 };
/// 无交互自动消失（悬停期间暂停计时）
pub const BOBBER_IDLE_TIMEOUT: Duration = Duration::from_secs(4);
/// 光标离开浮标中心超过该距离即消失（逻辑 px）
pub const BOBBER_MOVE_AWAY_PX: f64 = 80.0;
/// 距离判定的轮询间隔：只在浮标可见期间以 30 Hz 运行
pub const BOBBER_DISTANCE_POLL: Duration = Duration::from_millis(33);
/// 出现动画（scale 0.6→1 + 淡入）
pub const BOBBER_APPEAR: Duration = Duration::from_millis(140);
/// 消失动画（淡出）
pub const BOBBER_DISAPPEAR: Duration = Duration::from_millis(140);
/// 减少动效时的淡入 / 淡出时长上限
pub const BOBBER_REDUCED_FADE: Duration = Duration::from_millis(100);
/// 出现动画的起始缩放
pub const BOBBER_APPEAR_SCALE: f64 = 0.6;
/// 悬停高亮的过渡时长
pub const BOBBER_HOVER: Duration = Duration::from_millis(80);

/// 一次 mouse-up 的同步判定输入
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MouseUpFacts {
    /// 按下到抬起的位移（逻辑 px）
    pub drag_px: f64,
    /// 系统报告的连击次数（1 = 单击，2 = 双击，3 = 三击）
    pub click_count: u32,
    /// 是否按着 ⌘ / Ctrl
    pub command_or_control: bool,
    /// 是否落在 Wisp 自己的窗口内
    pub in_own_window: bool,
}

/// 是否值得继续读选区（浮标判定的同步部分）：
/// ① 落在 Wisp 窗口内 → 否；② 按着 ⌘ / Ctrl → 否；③ 位移 < 4 px 且不是双击 / 三击 → 否
pub fn is_bobber_candidate(facts: &MouseUpFacts) -> bool {
    if facts.in_own_window || facts.command_or_control {
        return false;
    }
    facts.drag_px >= DRAG_THRESHOLD_PX || facts.click_count >= 2
}

/// 两点间距离（逻辑 px）
pub fn distance(a: Point, b: Point) -> f64 {
    (a.x - b.x).hypot(a.y - b.y)
}

/// 浮标圆形的位置（显示器内逻辑坐标）：默认在抬起点右下 (8, 12)；某一方向越过 `bounds`（通常是显示器工作区）时
/// 在该方向上翻到抬起点的另一侧 (x−8−22 / y−12−22)，最后再夹回 `bounds` 内，保证四个角都完整可见
pub fn bobber_rect(at: Point, bounds: Rect) -> Rect {
    let d = BOBBER_DIAMETER;
    let mut x = at.x + BOBBER_OFFSET.x;
    let mut y = at.y + BOBBER_OFFSET.y;
    if x + d > bounds.right() {
        x = at.x - BOBBER_OFFSET.x - d;
    }
    if y + d > bounds.bottom() {
        y = at.y - BOBBER_OFFSET.y - d;
    }
    let x = x.clamp(bounds.x, (bounds.right() - d).max(bounds.x));
    let y = y.clamp(bounds.y, (bounds.bottom() - d).max(bounds.y));
    Rect::new(x, y, d, d)
}

/// 浮标窗口的矩形：以圆形为中心、边长 [`BOBBER_WINDOW`]
pub fn bobber_window_rect(circle: Rect) -> Rect {
    let pad = (BOBBER_WINDOW - circle.width) / 2.0;
    Rect::new(circle.x - pad, circle.y - pad, BOBBER_WINDOW, BOBBER_WINDOW)
}

/// 某点是否落在浮标的点击范围内（`circle` 为圆形的矩形，两者同一坐标系）：钩子判断「在别处按下」与浮标视图的
/// 命中测试共用它，保证两边对同一次点击的结论一致
pub fn bobber_hit(circle: Rect, p: Point) -> bool {
    let center = Point {
        x: circle.x + circle.width / 2.0,
        y: circle.y + circle.height / 2.0,
    };
    distance(center, p) <= BOBBER_HIT_RADIUS
}

/// 光标是否离开浮标中心太远
pub fn moved_away(circle: Rect, cursor: Point) -> bool {
    let center = Point {
        x: circle.x + circle.width / 2.0,
        y: circle.y + circle.height / 2.0,
    };
    distance(center, cursor) > BOBBER_MOVE_AWAY_PX
}

/// 鼠标额外按键对应的动作：未绑定（None）表示放行
pub fn mouse_action(spec: &MouseSpec, button: MouseButtonKind) -> Option<HotkeyAction> {
    match button {
        MouseButtonKind::Middle => spec.middle,
        MouseButtonKind::Back => spec.back,
        MouseButtonKind::Forward => spec.forward,
    }
}

/// 长按状态机（长按 ≥400 ms 才触发）：
/// 按下时先吞掉；400 ms 到点仍按着 → 触发，之后的抬起也吞掉；400 ms 内抬起 → 重新注入原始的按下与抬起
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LongPress {
    phase: LongPressPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum LongPressPhase {
    #[default]
    Idle,
    Pending {
        down_at: Instant,
    },
    Fired,
}

/// 长按状态机对抬起事件的处理结论
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LongPressUp {
    /// 没有进行中的按下（例如按下时未拦截）：原样放行
    PassThrough,
    /// 未达阈值：吞掉这次抬起，并重新注入原始的按下 + 抬起
    Replay,
    /// 已触发过：吞掉
    Swallow,
    /// 已达阈值但定时器还没来得及触发：补发动作并吞掉
    FireAndSwallow,
}

impl LongPress {
    /// 新建（空闲）
    pub const fn new() -> Self {
        Self {
            phase: LongPressPhase::Idle,
        }
    }

    /// 按下（已决定拦截）：开始计时
    pub fn down(&mut self, now: Instant) {
        self.phase = LongPressPhase::Pending { down_at: now };
    }

    /// 定时检查：到达阈值时返回 true（只返回一次）
    pub fn tick(&mut self, now: Instant) -> bool {
        match self.phase {
            LongPressPhase::Pending { down_at } if now.saturating_duration_since(down_at) >= LONG_PRESS => {
                self.phase = LongPressPhase::Fired;
                true
            }
            _ => false,
        }
    }

    /// 距离触发还要等多久（仅在计时中有值）
    pub fn remaining(&self, now: Instant) -> Option<Duration> {
        match self.phase {
            LongPressPhase::Pending { down_at } => {
                Some(LONG_PRESS.saturating_sub(now.saturating_duration_since(down_at)))
            }
            _ => None,
        }
    }

    /// 抬起：返回处理结论并复位
    pub fn up(&mut self, now: Instant) -> LongPressUp {
        let out = match self.phase {
            LongPressPhase::Idle => LongPressUp::PassThrough,
            LongPressPhase::Fired => LongPressUp::Swallow,
            LongPressPhase::Pending { down_at } if now.saturating_duration_since(down_at) >= LONG_PRESS => {
                LongPressUp::FireAndSwallow
            }
            LongPressPhase::Pending { .. } => LongPressUp::Replay,
        };
        self.phase = LongPressPhase::Idle;
        out
    }

    /// 是否处于计时或已触发（即按键当前被拦截）
    pub fn is_active(&self) -> bool {
        self.phase != LongPressPhase::Idle
    }

    /// 放弃（钩子注销、事件流异常时）
    pub fn reset(&mut self) {
        self.phase = LongPressPhase::Idle;
    }
}
