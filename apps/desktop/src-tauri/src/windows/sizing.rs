//! 宽高规则与面板几何动效的纯函数：
//! - Native 帧模式（macOS）：Rust 逐帧推进的弹簧 [`Spring`]（与 `spring.gentle` 同参 260 / 30 / 1）与
//!   窗口 alpha 进入 / 退出用的三次贝塞尔缓动 [`CubicBezier`]（与 `ease.standard` / `ease.exit` 同参）；
//! - Css 帧模式（Windows / Linux，以及 macOS 退路）：窗口高度的阶梯规则 [`CssHeightStepper`]（+48、两次放大至少隔 50 ms）。
//!
//! 动效参数的**唯一来源是前端** `src/shared/motion/tokens.ts`；这里的常量是它的镜像，单测 `motion_tokens_match_frontend`
//! 逐项比对，改动画参数时两边一起改。
//!
//! 翻译宽度 lerp、各视图宽度与初始高度、定位等几何规则见 placement.rs。

use std::time::{Duration, Instant};

/// Css 帧模式下卡片四周留给阴影的透明边距（逻辑 px，下文的 `m`）。
///
/// **唯一来源**：窗口尺寸（`h + 2·m` 的阶梯 resize）、点击穿透用的卡片矩形、前端卡片在窗口内的偏移都由它推出；
/// 前端经 `Boot.shadow_margin` → CSS 变量 `--shadow-margin` 读取，不得在 CSS / TS 里另写一份。
/// Native 帧模式（窗口即卡片）下为 0。调整阴影时只改这里
pub const SHADOW_MARGIN: f64 = 24.0;

/// 面板卡片圆角（逻辑 pt）：Native 帧模式的材质遮罩与着色层圆角，与前端 macOS 的 `--r-panel` 一致
pub const PANEL_RADIUS: f64 = 12.0;

/// Css 帧模式窗口放大的最小步长（逻辑 px）
pub const CSS_GROW_STEP: f64 = 48.0;

/// Css 帧模式两次放大之间的最小间隔
pub const CSS_GROW_MIN_INTERVAL: Duration = Duration::from_millis(50);

/// Css 帧模式：卡片高度稳定这么久后回收窗口的透明富余区（透明富余区最多存在约 300 ms）
pub const CSS_SLACK_RECLAIM: Duration = Duration::from_millis(300);

/// 动效时长（秒），镜像 `tokens.ts` 的 `duration`
pub mod duration {
    /// 退出（窗口 alpha 1→0）
    pub const FAST: f64 = 0.14;
    /// 进入（窗口 alpha 0→1）
    pub const BASE: f64 = 0.22;
}

/// 其余时间常量（秒），镜像 `tokens.ts` 的 `timing`
pub mod timing {
    /// 视图切换的交叉淡入（`timing.crossfade`）。宽度要变的交叉淡入先把旧视图淡出这么久（ViewSwitch 的
    /// `mode="wait"`），旧内容在这之后完全透明，Native 帧模式在这一刻改窗口宽度
    pub const CROSSFADE: f64 = 0.12;
    /// 减少动效时的淡入淡出（`timing.reducedFade`，也是减少动效下交叉淡入的时长）
    pub const REDUCED_FADE: f64 = 0.1;
}

/// 弹簧参数（镜像 `tokens.ts` 的 `spring.*`）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpringParams {
    pub stiffness: f64,
    pub damping: f64,
    pub mass: f64,
}

impl SpringParams {
    /// `spring.gentle`：高度跟随、固定窗口脱离
    pub const GENTLE: Self = Self {
        stiffness: 260.0,
        damping: 30.0,
        mass: 1.0,
    };
    /// `spring.snappy`：面板进入、选中指示器
    pub const SNAPPY: Self = Self {
        stiffness: 520.0,
        damping: 38.0,
        mass: 1.0,
    };

    /// `panel_resize` 指定的弹簧
    pub const fn of(spring: wisp_types::ResizeSpring) -> Self {
        match spring {
            wisp_types::ResizeSpring::Gentle => Self::GENTLE,
            wisp_types::ResizeSpring::Snappy => Self::SNAPPY,
        }
    }
}

/// 弹簧静止判定：与目标的距离（逻辑 px）。0.25 pt 在 2× 屏上是半个物理像素，按像素取整后已看不出差别
pub const SPRING_REST_DELTA: f64 = 0.25;
/// 弹簧静止判定：速度（逻辑 px / s）
pub const SPRING_REST_SPEED: f64 = 2.0;

/// 一维阻尼弹簧，按**解析解**推进（与 Motion 的 spring 生成器是同一组公式）：任意步长都精确、稳定，
/// 主线程偶发卡顿后按真实经过的时间追上，不会像显式积分那样发散或「慢放」。
///
/// 重定目标（[`Spring::retarget`]）保留当前速度：流式输出中目标高度不断变化时曲线连续、没有折角
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spring {
    params: SpringParams,
    value: f64,
    velocity: f64,
    target: f64,
}

impl Spring {
    /// 在 `value` 处静止的弹簧
    pub fn new(params: SpringParams, value: f64) -> Self {
        Self {
            params,
            value,
            velocity: 0.0,
            target: value,
        }
    }

    /// 当前值
    pub fn value(&self) -> f64 {
        self.value
    }

    /// 当前速度（单位 / s；只在单测里观察积分结果）
    #[cfg(test)]
    pub fn velocity(&self) -> f64 {
        self.velocity
    }

    /// 当前目标（只在单测里观察）
    #[cfg(test)]
    pub fn target(&self) -> f64 {
        self.target
    }

    /// 改目标，保留当前位置与速度
    pub fn retarget(&mut self, target: f64) {
        self.target = target;
    }

    /// 换弹簧参数，保留当前位置、速度与目标（菜单预留的 snappy 与内容变化的 gentle 在同一段运动里交替时，
    /// 曲线连续、没有折角——解析解只依赖当前的位移与速度）
    pub fn retune(&mut self, params: SpringParams) {
        self.params = params;
    }

    /// 立即跳到 `value` 并静止（目标同时改为 `value`；只在单测里构造静止状态）
    #[cfg(test)]
    pub fn jump_to(&mut self, value: f64) {
        self.value = value;
        self.velocity = 0.0;
        self.target = value;
    }

    /// 是否已静止（距离与速度都低于阈值）
    pub fn is_at_rest(&self) -> bool {
        (self.value - self.target).abs() < SPRING_REST_DELTA && self.velocity.abs() < SPRING_REST_SPEED
    }

    /// 推进 `dt` 秒；静止后吸附到目标并返回 true。`dt` 非有限或不为正时不推进
    pub fn step(&mut self, dt: f64) -> bool {
        if !(dt.is_finite() && dt > 0.0) {
            return self.is_at_rest();
        }
        let SpringParams {
            stiffness: k,
            damping: c,
            mass: m,
        } = self.params;
        let d0 = self.value - self.target;
        let v0 = self.velocity;
        let w0 = (k / m).sqrt();
        let zeta = c / (2.0 * (k * m).sqrt());
        let (d, v) = if (zeta - 1.0).abs() < 1e-9 {
            // 临界阻尼
            let e = (-w0 * dt).exp();
            let b = v0 + w0 * d0;
            (e * (d0 + b * dt), e * (v0 - w0 * b * dt))
        } else if zeta < 1.0 {
            // 欠阻尼（gentle：ζ ≈ 0.93；snappy：ζ ≈ 0.83）
            let a = zeta * w0;
            let wd = w0 * (1.0 - zeta * zeta).sqrt();
            let b = (v0 + a * d0) / wd;
            let e = (-a * dt).exp();
            let (s, co) = (wd * dt).sin_cos();
            (e * (d0 * co + b * s), e * (v0 * co - (d0 * wd + a * b) * s))
        } else {
            // 过阻尼
            let root = (zeta * zeta - 1.0).sqrt();
            let r1 = -w0 * (zeta - root);
            let r2 = -w0 * (zeta + root);
            let c2 = (v0 - r1 * d0) / (r2 - r1);
            let c1 = d0 - c2;
            let (e1, e2) = ((r1 * dt).exp(), (r2 * dt).exp());
            (c1 * e1 + c2 * e2, r1 * c1 * e1 + r2 * c2 * e2)
        };
        self.value = self.target + d;
        self.velocity = v;
        if self.is_at_rest() {
            self.value = self.target;
            self.velocity = 0.0;
            true
        } else {
            false
        }
    }
}

/// 弹簧从 `from` 静止出发、到 `to` 静止所需的时间（按 120 Hz 推进解析解，最多 2 s）：Rust 据此估计前端同参弹簧
/// （例如卡片高度的 spring.gentle）何时停下
pub fn settle_duration(params: SpringParams, from: f64, to: f64) -> Duration {
    const DT: f64 = 1.0 / 120.0;
    const LIMIT: f64 = 2.0;
    let mut spring = Spring::new(params, from);
    spring.retarget(to);
    let mut t = DT;
    while t < LIMIT && !spring.step(DT) {
        t += DT;
    }
    Duration::from_secs_f64(t.min(LIMIT))
}

/// CSS `cubic-bezier(x1, y1, x2, y2)` 缓动（x1、x2 ∈ [0, 1]）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CubicBezier {
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
}

impl CubicBezier {
    /// `ease.standard`：进入
    pub const STANDARD: Self = Self::new(0.2, 0.0, 0.0, 1.0);
    /// `ease.exit`：退出
    pub const EXIT: Self = Self::new(0.4, 0.0, 1.0, 1.0);
    /// 线性（减少动效时的淡入淡出）
    pub const LINEAR: Self = Self::new(0.0, 0.0, 1.0, 1.0);

    /// 构造（调用方保证 x1、x2 ∈ [0, 1]）
    pub const fn new(x1: f64, y1: f64, x2: f64, y2: f64) -> Self {
        Self { x1, y1, x2, y2 }
    }

    /// 一维三次贝塞尔：B(s) = 3(1-s)²s·p1 + 3(1-s)s²·p2 + s³
    fn sample(p1: f64, p2: f64, s: f64) -> f64 {
        let u = 1.0 - s;
        3.0 * u * u * s * p1 + 3.0 * u * s * s * p2 + s * s * s
    }

    fn sample_dx(&self, s: f64) -> f64 {
        let u = 1.0 - s;
        3.0 * u * u * self.x1 + 6.0 * u * s * (self.x2 - self.x1) + 3.0 * s * s * (1.0 - self.x2)
    }

    /// 时间进度 `t` ∈ [0, 1] → 缓动后的进度（牛顿迭代解 x(s) = t，导数过小或不收敛时改用二分）
    pub fn ease(&self, t: f64) -> f64 {
        if t.is_nan() || t <= 0.0 {
            return 0.0;
        }
        if t >= 1.0 {
            return 1.0;
        }
        let mut s = t;
        for _ in 0..8 {
            let x = Self::sample(self.x1, self.x2, s) - t;
            if x.abs() < 1e-7 {
                return Self::sample(self.y1, self.y2, s);
            }
            let dx = self.sample_dx(s);
            if dx.abs() < 1e-6 {
                break;
            }
            s = (s - x / dx).clamp(0.0, 1.0);
        }
        let (mut lo, mut hi) = (0.0, 1.0);
        s = t;
        for _ in 0..40 {
            let x = Self::sample(self.x1, self.x2, s);
            if (x - t).abs() < 1e-7 {
                break;
            }
            if x < t {
                lo = s;
            } else {
                hi = s;
            }
            s = 0.5 * (lo + hi);
        }
        Self::sample(self.y1, self.y2, s)
    }
}

/// 把逻辑长度对齐到物理像素（`scale` 为 backing scale factor）；scale 非法时原样返回
pub fn snap_to_pixel(v: f64, scale: f64) -> f64 {
    if scale.is_finite() && scale > 0.0 {
        (v * scale).round() / scale
    } else {
        v
    }
}

/// [`CssHeightStepper::next`] 的结论
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CssResize {
    /// 不需要改窗口高度
    Keep,
    /// 立即把窗口高度改为该值（逻辑 px）
    Set(f64),
    /// 需要放大，但距离上次放大不足 [`CSS_GROW_MIN_INTERVAL`]：等这么久后用最新的卡片高度重新计算
    Defer(Duration),
}

/// Css 帧模式的窗口高度阶梯。前端上报**卡片目标高度** `h`（不含阴影边距），窗口高度：
/// - 变高：`new = min(max(h + 2·m, cur + 48), max_h)`，两次放大之间至少隔 50 ms（多出来的是透明富余区，
///   卡片在窗口内用 spring.gentle 跟随，不会被窗口裁切）；
/// - 变矮：前端先播完卡片收缩动画再上报，窗口一次缩到 `h + 2·m`；
/// - 卡片变高但富余区仍放得下、或卡片没有变矮：不动窗口（流式输出期间不必每次上报都改窗口）。
///
/// 阶梯留下的富余区由面板控制器在卡片高度稳定 [`CSS_SLACK_RECLAIM`] 后一次回收（panel.rs），不在这里处理。
#[derive(Debug, Clone, Default)]
pub struct CssHeightStepper {
    last_grow: Option<Instant>,
    last_card: Option<f64>,
}

impl CssHeightStepper {
    /// 空状态
    pub fn new() -> Self {
        Self::default()
    }

    /// 新会话：窗口已按卡片高度 `card` 显示（ready 时）
    pub fn reset(&mut self, card: f64) {
        self.last_grow = None;
        self.last_card = Some(card);
    }

    /// 根据最新上报的卡片高度计算窗口高度。`cur` 为当前窗口高度，`margin` 为阴影边距，`max_window` 为窗口上限
    pub fn next(&mut self, card: f64, cur: f64, margin: f64, max_window: f64, now: Instant) -> CssResize {
        let need = (card + 2.0 * margin).min(max_window);
        let shrinking = self.last_card.is_some_and(|last| card < last);
        if need > cur {
            if let Some(last) = self.last_grow {
                let since = now.saturating_duration_since(last);
                if since < CSS_GROW_MIN_INTERVAL {
                    return CssResize::Defer(CSS_GROW_MIN_INTERVAL - since);
                }
            }
            self.last_grow = Some(now);
            self.last_card = Some(card);
            CssResize::Set(need.max(cur + CSS_GROW_STEP).min(max_window))
        } else if shrinking && need < cur {
            self.last_card = Some(card);
            CssResize::Set(need)
        } else {
            self.last_card = Some(card);
            CssResize::Keep
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settle_time(params: SpringParams, from: f64, to: f64, dt: f64) -> f64 {
        let mut s = Spring::new(params, from);
        s.retarget(to);
        let mut t = 0.0;
        while !s.step(dt) {
            t += dt;
            assert!(t < 5.0, "弹簧 5 s 内必须静止");
        }
        t + dt
    }

    /// 换参数保留位置与速度（曲线连续），之后按新参数收敛到同一目标；snappy 比 gentle 先停下
    #[test]
    fn retune_keeps_position_and_velocity() {
        let mut s = Spring::new(SpringParams::GENTLE, 120.0);
        s.retarget(320.0);
        for _ in 0..6 {
            s.step(1.0 / 120.0);
        }
        let (v, vel) = (s.value(), s.velocity());
        s.retune(SpringParams::SNAPPY);
        assert_eq!((s.value(), s.velocity(), s.target()), (v, vel, 320.0));
        let mut t = 0.0;
        while !s.step(1.0 / 120.0) {
            t += 1.0 / 120.0;
            assert!(t < 2.0, "snappy 没有收敛");
        }
        assert_eq!(s.value(), 320.0);
        assert_eq!(SpringParams::of(wisp_types::ResizeSpring::Snappy), SpringParams::SNAPPY);
        assert_eq!(
            SpringParams::of(wisp_types::ResizeSpring::default()),
            SpringParams::GENTLE
        );
    }

    #[test]
    fn spring_settles_on_target_without_large_overshoot() {
        let mut s = Spring::new(SpringParams::GENTLE, 120.0);
        s.retarget(320.0);
        let mut max = f64::MIN;
        for _ in 0..600 {
            s.step(1.0 / 120.0);
            max = max.max(s.value());
        }
        assert_eq!(s.value(), 320.0);
        assert!(s.is_at_rest());
        // ζ ≈ 0.93：过冲不到 1%
        assert!(max - 320.0 < 2.0, "过冲 {max}");
    }

    #[test]
    fn spring_is_step_size_independent() {
        // 解析解：120 Hz、60 Hz 与一次大步长推进到同一时刻，状态一致
        let run = |dt: f64, n: usize| {
            let mut s = Spring::new(SpringParams::GENTLE, 0.0);
            s.retarget(100.0);
            for _ in 0..n {
                s.step(dt);
            }
            (s.value(), s.velocity())
        };
        let (a, va) = run(1.0 / 120.0, 12);
        let (b, vb) = run(1.0 / 60.0, 6);
        let (c, vc) = run(0.1, 1);
        assert!((a - b).abs() < 1e-9 && (a - c).abs() < 1e-9, "{a} {b} {c}");
        assert!((va - vb).abs() < 1e-9 && (va - vc).abs() < 1e-9);
    }

    #[test]
    fn spring_retarget_keeps_velocity() {
        let mut s = Spring::new(SpringParams::GENTLE, 0.0);
        s.retarget(200.0);
        s.step(0.05);
        let v = s.velocity();
        assert!(v > 0.0);
        s.retarget(400.0);
        assert_eq!(s.velocity(), v);
        assert!(!s.is_at_rest());
        s.step(1.0 / 120.0);
        assert!(s.velocity() > 0.0, "改目标后继续朝新目标运动");
    }

    #[test]
    fn spring_gentle_settles_in_reasonable_time() {
        // gentle 约 0.4–0.6 s 静止；snappy 更快
        let gentle = settle_time(SpringParams::GENTLE, 120.0, 320.0, 1.0 / 120.0);
        let snappy = settle_time(SpringParams::SNAPPY, 120.0, 320.0, 1.0 / 120.0);
        assert!((0.3..0.8).contains(&gentle), "gentle {gentle}");
        assert!(snappy < gentle, "snappy {snappy} gentle {gentle}");
    }

    #[test]
    fn settle_duration_tracks_distance() {
        let none = settle_duration(SpringParams::GENTLE, 200.0, 200.0);
        assert!(none <= Duration::from_millis(9), "{none:?}");
        let small = settle_duration(SpringParams::GENTLE, 120.0, 150.0);
        let large = settle_duration(SpringParams::GENTLE, 120.0, 480.0);
        assert!(small < large, "{small:?} {large:?}");
        let gentle = settle_duration(SpringParams::GENTLE, 120.0, 320.0).as_secs_f64();
        assert!((0.3..0.8).contains(&gentle), "gentle {gentle}");
        // 与逐步推进的结果一致；收缩与放大对称
        assert!((gentle - settle_time(SpringParams::GENTLE, 120.0, 320.0, 1.0 / 120.0)).abs() < 1e-9);
        assert_eq!(
            settle_duration(SpringParams::GENTLE, 320.0, 120.0),
            settle_duration(SpringParams::GENTLE, 120.0, 320.0)
        );
    }

    #[test]
    fn spring_handles_degenerate_dt() {
        let mut s = Spring::new(SpringParams::GENTLE, 10.0);
        s.retarget(20.0);
        assert!(!s.step(0.0));
        assert!(!s.step(f64::NAN));
        assert_eq!(s.value(), 10.0);
        assert!(s.step(10.0), "超长步长直接静止");
        assert_eq!(s.value(), 20.0);
        s.jump_to(5.0);
        assert!(s.is_at_rest());
        assert_eq!((s.value(), s.target(), s.velocity()), (5.0, 5.0, 0.0));
    }

    #[test]
    fn spring_damping_regimes_are_monotonic_when_not_underdamped() {
        // 临界 / 过阻尼分支也收敛到目标，且不回摆
        for damping in [2.0 * 260f64.sqrt(), 60.0] {
            let mut s = Spring::new(
                SpringParams {
                    stiffness: 260.0,
                    damping,
                    mass: 1.0,
                },
                0.0,
            );
            s.retarget(50.0);
            let mut prev = 0.0;
            for _ in 0..1200 {
                s.step(1.0 / 120.0);
                assert!(s.value() >= prev - 1e-9, "临界 / 过阻尼不应回摆");
                prev = s.value();
            }
            assert_eq!(s.value(), 50.0);
        }
    }

    #[test]
    fn cubic_bezier_matches_css_reference_points() {
        for b in [CubicBezier::STANDARD, CubicBezier::EXIT] {
            assert_eq!(b.ease(0.0), 0.0);
            assert_eq!(b.ease(1.0), 1.0);
            assert_eq!(b.ease(f64::NAN), 0.0);
            let mut prev = 0.0;
            for i in 1..=100 {
                let y = b.ease(f64::from(i) / 100.0);
                assert!(y >= prev - 1e-9, "单调");
                prev = y;
            }
        }
        let linear = CubicBezier::new(0.0, 0.0, 1.0, 1.0);
        for t in [0.1, 0.33, 0.5, 0.9] {
            assert!((linear.ease(t) - t).abs() < 1e-6);
        }
        // standard 前快后慢、exit 前慢后快
        assert!(CubicBezier::STANDARD.ease(0.5) > 0.8);
        assert!(CubicBezier::EXIT.ease(0.5) < 0.5);
        // CSS `ease`（0.25, 0.1, 0.25, 1）在 t = 0.5 处 ≈ 0.8024（浏览器参考值）
        let ease = CubicBezier::new(0.25, 0.1, 0.25, 1.0);
        assert!((ease.ease(0.5) - 0.8024).abs() < 1e-3, "{}", ease.ease(0.5));
    }

    #[test]
    fn snap_to_pixel_rounds_to_device_pixels() {
        assert_eq!(snap_to_pixel(100.3, 2.0), 100.5);
        assert_eq!(snap_to_pixel(100.2, 2.0), 100.0);
        assert_eq!(snap_to_pixel(100.4, 1.0), 100.0);
        assert!((snap_to_pixel(100.4, 1.5) - 151.0 / 1.5).abs() < 1e-12);
        assert_eq!(snap_to_pixel(7.3, 0.0), 7.3);
        assert_eq!(snap_to_pixel(7.3, f64::NAN), 7.3);
    }

    #[test]
    fn css_stepper_grows_in_steps_and_throttles() {
        let t0 = Instant::now();
        let m = SHADOW_MARGIN;
        let mut s = CssHeightStepper::new();
        s.reset(120.0);
        let cur = 120.0 + 2.0 * m;
        // 卡片 +10：至少放大 48
        assert_eq!(s.next(130.0, cur, m, 900.0, t0), CssResize::Set(cur + CSS_GROW_STEP));
        let cur = cur + CSS_GROW_STEP;
        // 富余区放得下：不动
        assert_eq!(
            s.next(150.0, cur, m, 900.0, t0 + Duration::from_millis(10)),
            CssResize::Keep
        );
        // 放不下但距上次放大不足 50 ms：推迟
        assert_eq!(
            s.next(200.0, cur, m, 900.0, t0 + Duration::from_millis(20)),
            CssResize::Defer(Duration::from_millis(30))
        );
        // 50 ms 后：一次放大到 h + 2m（大于 cur + 48）
        assert_eq!(
            s.next(260.0, cur, m, 900.0, t0 + Duration::from_millis(50)),
            CssResize::Set(260.0 + 2.0 * m)
        );
    }

    #[test]
    fn css_stepper_clamps_to_max_and_shrinks_once() {
        let t0 = Instant::now();
        let m = SHADOW_MARGIN;
        let mut s = CssHeightStepper::new();
        s.reset(100.0);
        assert_eq!(s.next(600.0, 148.0, m, 500.0, t0), CssResize::Set(500.0));
        // 已到上限：再高也不动
        assert_eq!(
            s.next(700.0, 500.0, m, 500.0, t0 + Duration::from_secs(1)),
            CssResize::Keep
        );
        // 变矮（前端收缩动画已结束）：一次缩到位
        assert_eq!(
            s.next(80.0, 500.0, m, 500.0, t0 + Duration::from_secs(2)),
            CssResize::Set(80.0 + 2.0 * m)
        );
        // 卡片没有变矮时不回收阶梯富余区
        let mut s = CssHeightStepper::new();
        s.reset(100.0);
        assert_eq!(s.next(101.0, 148.0, m, 900.0, t0), CssResize::Set(196.0));
        assert_eq!(
            s.next(102.0, 196.0, m, 900.0, t0 + Duration::from_secs(1)),
            CssResize::Keep
        );
    }

    #[test]
    fn motion_tokens_match_frontend() {
        // 动效参数的唯一来源是 tokens.ts；这里的镜像必须一致
        let tokens = include_str!("../../../src/shared/motion/tokens.ts");
        let compact: String = tokens.chars().filter(|c| !c.is_whitespace()).collect();
        for (name, p) in [("gentle", SpringParams::GENTLE), ("snappy", SpringParams::SNAPPY)] {
            let decl = format!(
                "{name}:{{type:'spring',stiffness:{},damping:{},mass:{}}}",
                p.stiffness, p.damping, p.mass
            );
            assert!(compact.contains(&decl), "tokens.ts 里找不到 `{decl}`");
        }
        for decl in [
            format!("fast:{}", duration::FAST),
            format!("base:{}", duration::BASE),
            format!("crossfade:{}", timing::CROSSFADE),
            format!("reducedFade:{}", timing::REDUCED_FADE),
        ] {
            assert!(compact.contains(&decl), "tokens.ts 里找不到 `{decl}`");
        }
        for (name, b) in [("standard", CubicBezier::STANDARD), ("exit", CubicBezier::EXIT)] {
            let decl = format!("{name}:[{},{},{},{}]", b.x1, b.y1, b.x2, b.y2);
            assert!(compact.contains(&decl), "tokens.ts 里找不到 `{decl}`");
        }
    }
}
