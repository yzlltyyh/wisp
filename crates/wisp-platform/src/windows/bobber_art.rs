//! 浮标的像素绘制（纯逻辑，不调用 Win32）：与应用图标一致的品牌圆盘——柔和投影、品牌蓝圆盘（以 `#3266E3` 为中间色的
//! 轻微竖直渐变）、顶部高光内描边、白色字形（应用图标的小尺寸字形 `wisp-mark-micro`，几何见
//! `common::bobber_glyph`，与 X11 浮标相同）。用有符号距离 + 解析抗锯齿直接画出，100 % 缩放（22 px 圆盘）下依然
//! 清晰。输出 `UpdateLayeredWindow` 需要的预乘 BGRA（自上而下）。
//!
//! 分两层输出（只有图标区域接收点击）：**圆盘层**只有圆盘及其抗锯齿边缘的 alpha 非零（分层窗口按 alpha
//! 做命中测试，其余像素点击穿透）；**投影层**放进一个 `WS_EX_TRANSPARENT` 的窗口，完全不接收点击。投影层在圆盘完全
//! 不透明的内部留空：两层各自淡入淡出时，投影不会从半透明的圆盘下面透出来；完全不透明时叠出的画面与单层绘制一致。
//!
//! 动效：出现时圆形从 0.6 缩放到 1 并淡入（fast = 140 ms，ease.standard），消失只淡出（fast，ease.exit），
//! 减少动效时只做 ≤100 ms 的淡入淡出；悬停 80 ms 提亮，按下缩到 0.96。缩放帧在第一次显示前一次性预渲染（按缩放
//! 等距分布，每一步不到 1 pt），动画期间由 vsync 驱动、每次合成推送一帧，静止时没有任何定时器。

use crate::common::bobber_glyph;
use crate::common::gesture::{
    BOBBER_APPEAR, BOBBER_APPEAR_SCALE, BOBBER_DIAMETER, BOBBER_DISAPPEAR, BOBBER_HOVER, BOBBER_REDUCED_FADE,
    BOBBER_WINDOW,
};

/// 圆盘半径（pt）
const RADIUS: f64 = BOBBER_DIAMETER / 2.0;
/// 窗口边长（pt）：圆形四周的余量只画投影（投影层点击穿透）
const WINDOW: f64 = BOBBER_WINDOW;
/// 按下时的整体缩放
const PRESSED_SCALE: f64 = 0.96;
/// 悬停时向白色提亮的比例
const HOVER_TINT: f64 = 0.12;

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// 出现动画时长（ms）
pub(crate) fn enter_ms() -> f64 {
    ms(BOBBER_APPEAR)
}
/// 消失动画时长（ms）
pub(crate) fn exit_ms() -> f64 {
    ms(BOBBER_DISAPPEAR)
}
/// 减少动效时的淡入淡出时长（ms）
pub(crate) fn reduced_ms() -> f64 {
    ms(BOBBER_REDUCED_FADE)
}
/// 悬停高亮过渡时长（ms）
pub(crate) fn hover_ms() -> f64 {
    ms(BOBBER_HOVER)
}
/// 出现动画预渲染的帧数：缩放从 0.6 到 1 等距分布（第一帧 0.6、最后一帧即静止态），每一步约 0.044，
/// 22 pt 圆盘上不到 1 pt；运行时按缓动后的目标缩放取最近的一帧
pub(crate) const ENTER_FRAMES: usize = 10;
/// 出现时圆形的初始缩放
const ENTER_FROM_SCALE: f64 = BOBBER_APPEAR_SCALE;

/// 非预乘颜色（sRGB 分量 0..1）
#[derive(Debug, Clone, Copy, PartialEq)]
struct Color {
    r: f64,
    g: f64,
    b: f64,
}

impl Color {
    const fn hex(v: u32) -> Self {
        Self {
            r: ((v >> 16) & 0xFF) as f64 / 255.0,
            g: ((v >> 8) & 0xFF) as f64 / 255.0,
            b: (v & 0xFF) as f64 / 255.0,
        }
    }

    fn mix(self, o: Self, t: f64) -> Self {
        Self {
            r: self.r + (o.r - self.r) * t,
            g: self.g + (o.g - self.g) * t,
            b: self.b + (o.b - self.b) * t,
        }
    }
}

const BLACK: Color = Color::hex(0x000000);
const WHITE: Color = Color::hex(0xFFFFFF);
/// 品牌蓝 `#3266E3` 上下各向白 / 黑偏约 6 %：圆盘中部就是应用图标的品牌色，竖直方向留一点体积感
const DISC_TOP: Color = Color::hex(0x3E6FE5);
const DISC_BOTTOM: Color = Color::hex(0x2F60D5);

/// 随系统明暗变化的参数：深色背景上投影更重、高光更亮
#[derive(Debug, Clone, Copy)]
struct Palette {
    shadow_alpha: f64,
    rim_alpha: f64,
}

fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            shadow_alpha: 0.55,
            rim_alpha: 0.38,
        }
    } else {
        Palette {
            shadow_alpha: 0.30,
            rim_alpha: 0.28,
        }
    }
}

/// 预乘 alpha 的累加像素
#[derive(Debug, Clone, Copy, Default)]
struct Px {
    r: f64,
    g: f64,
    b: f64,
    a: f64,
}

impl Px {
    /// source-over 合成：`c` 为非预乘颜色，`alpha` 为本层不透明度 × 覆盖率
    fn over(&mut self, c: Color, alpha: f64) {
        let a = clamp01(alpha);
        if a <= 0.0 {
            return;
        }
        let k = 1.0 - a;
        self.r = c.r * a + self.r * k;
        self.g = c.g * a + self.g * k;
        self.b = c.b * a + self.b * k;
        self.a = a + self.a * k;
    }
}

/// 窗口边长（物理像素）
pub(crate) fn window_px(scale: f64) -> i32 {
    (WINDOW * scale).round().max(8.0) as i32
}

fn clamp01(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}

fn smoothstep(e0: f64, e1: f64, x: f64) -> f64 {
    let t = clamp01((x - e0) / (e1 - e0));
    t * t * (3.0 - 2.0 * t)
}

/// 由有符号距离（物理像素，内部为负）得到覆盖率
fn coverage(d: f64) -> f64 {
    clamp01(0.5 - d)
}

/// 浮标的图层
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Layer {
    /// 圆盘 + 高光 + 字形：接收点击的窗口
    Disc,
    /// 投影：点击穿透的窗口，叠在圆盘窗口下面
    Shadow,
}

/// 渲染某一层的一帧：边长 `size` 像素的预乘 BGRA（自上而下）。`scale` 为 DPI 缩放，`content` 为整体缩放（出现
/// 动画 0.6..=1、按下 0.96，围绕中心缩放），`hover` 为悬停高亮程度（0..=1，只影响圆盘层）
pub(crate) fn render(size: i32, scale: f64, content: f64, hover: f64, dark: bool, layer: Layer) -> Vec<u8> {
    let p = palette(dark);
    let size = size.max(1) as usize;
    let mut out = vec![0u8; size * size * 4];
    let center = size as f64 / 2.0;
    let zoom = content.max(0.01);
    // pt 距离 → 物理像素距离（抗锯齿按物理像素计算）
    let to_px = scale * zoom;
    let tint = clamp01(hover) * HOVER_TINT;
    let glyph = bobber_glyph::glyph();
    // 字形（含一个物理像素的抗锯齿边缘）只可能出现在这个包围盒里（pt），盒外的像素跳过距离计算
    let (gx0, gx1, gy0, gy1) = {
        let (x0, x1, y0, y1) = glyph.bounds();
        let aa = 1.0 / to_px;
        (x0 - aa, x1 + aa, y0 - aa, y1 + aa)
    };

    for y in 0..size {
        for x in 0..size {
            // 像素中心 → 以圆盘中心为原点的 pt 坐标（除以 zoom 实现围绕中心缩放）
            let ux = (x as f64 + 0.5 - center) / to_px;
            let uy = (y as f64 + 0.5 - center) / to_px;
            let mut px = Px::default();
            let d = (ux * ux + uy * uy).sqrt() - RADIUS;

            if layer == Layer::Shadow {
                // 1. 投影：向下偏 1.2 pt、向外柔和衰减约 3.5 pt；圆盘完全不透明的内部（距边缘 ≥ 1.5 px）留空，
                //    那里的覆盖率恰好为 1，完全不透明时与单层绘制完全一致
                let inner = coverage(d * to_px + 1.0);
                let sd = (ux * ux + (uy - 1.2).powi(2)).sqrt() - RADIUS;
                px.over(
                    BLACK,
                    (1.0 - smoothstep(-1.5, 3.5, sd)) * p.shadow_alpha * (1.0 - inner),
                );
            }

            // 2. 圆盘：竖直渐变 + 悬停提亮
            let disc = if layer == Layer::Disc { coverage(d * to_px) } else { 0.0 };
            if disc > 0.0 {
                let t = clamp01((uy + RADIUS) / (2.0 * RADIUS));
                px.over(DISC_TOP.mix(DISC_BOTTOM, t).mix(WHITE, tint), disc);
                // 3. 顶部高光内描边（0.75 pt，自上而下淡出）
                let ring = coverage((d.abs() - 0.375) * to_px) * disc;
                px.over(WHITE, ring * p.rim_alpha * (1.0 - t).powf(1.4));
                // 4. 字形
                if (gx0..=gx1).contains(&ux) && (gy0..=gy1).contains(&uy) {
                    let gd = glyph.distance(ux, uy);
                    px.over(WHITE, coverage(gd * to_px) * disc * 0.97);
                }
            }

            // 低于 1/255 的像素置为全透明（圆盘层：分层窗口只有 alpha 为 0 的像素才会让点击穿透）
            let a8 = (clamp01(px.a) * 255.0).round() as u8;
            if a8 == 0 {
                continue;
            }
            let to8 = |v: f64| (clamp01(v) * 255.0).round().min(f64::from(a8)) as u8;
            let i = (y * size + x) * 4;
            out[i] = to8(px.b);
            out[i + 1] = to8(px.g);
            out[i + 2] = to8(px.r);
            out[i + 3] = a8;
        }
    }
    out
}

/// 三次贝塞尔缓动（P0 = (0,0)，P3 = (1,1)）：给定时间进度求输出进度
pub(crate) fn cubic_bezier(x1: f64, y1: f64, x2: f64, y2: f64, t: f64) -> f64 {
    let t = clamp01(t);
    let bez = |a: f64, b: f64, s: f64| {
        let u = 1.0 - s;
        3.0 * u * u * s * a + 3.0 * u * s * s * b + s * s * s
    };
    // x(s) 单调递增，二分求 s
    let (mut lo, mut hi) = (0.0f64, 1.0f64);
    for _ in 0..32 {
        let mid = (lo + hi) / 2.0;
        if bez(x1, x2, mid) < t {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    bez(y1, y2, (lo + hi) / 2.0)
}

/// ease.standard = cubic-bezier(0.2, 0, 0, 1)
pub(crate) fn ease_standard(t: f64) -> f64 {
    cubic_bezier(0.2, 0.0, 0.0, 1.0, t)
}

/// ease.exit = cubic-bezier(0.4, 0, 1, 1)
pub(crate) fn ease_exit(t: f64) -> f64 {
    cubic_bezier(0.4, 0.0, 1.0, 1.0, t)
}

/// 出现动画第 i 帧（0..ENTER_FRAMES）的圆形缩放：从 0.6 到 1 等距
pub(crate) fn enter_scale(i: usize) -> f64 {
    let t = i.min(ENTER_FRAMES - 1) as f64 / (ENTER_FRAMES - 1) as f64;
    ENTER_FROM_SCALE + (1.0 - ENTER_FROM_SCALE) * t
}

/// 出现动画在时间进度 p（0..=1）时应显示的帧下标：按 ease.standard 缓动后的缩放取最近的一帧
pub(crate) fn enter_index(p: f64) -> usize {
    let i = (ease_standard(clamp01(p)) * (ENTER_FRAMES - 1) as f64).round();
    (i.max(0.0) as usize).min(ENTER_FRAMES - 1)
}

/// 两帧预乘像素按 t 线性插值
pub(crate) fn blend(a: &[u8], b: &[u8], t: f64, out: &mut [u8]) {
    let t = clamp01(t);
    for ((o, &x), &y) in out.iter_mut().zip(a).zip(b) {
        *o = (f64::from(x) + (f64::from(y) - f64::from(x)) * t).round() as u8;
    }
}

/// 预渲染的整套帧
pub(crate) struct Frames {
    pub dark: bool,
    pub size: i32,
    pub scale: f64,
    /// 出现动画的缩放帧（圆盘层，0..ENTER_FRAMES - 1；最后一帧即 `normal`，不重复保存）
    enter: Vec<Vec<u8>>,
    /// 出现动画的缩放帧（投影层，同上；最后一帧即 `shadow`）
    enter_shadow: Vec<Vec<u8>>,
    pub normal: Vec<u8>,
    pub hover: Vec<u8>,
    /// 按下（悬停态缩到 0.96）
    pub pressed: Vec<u8>,
    /// 静止态与悬停态的投影
    pub shadow: Vec<u8>,
    /// 按下时的投影
    pub shadow_pressed: Vec<u8>,
}

impl Frames {
    /// 为某个缩放与明暗渲染全部帧
    pub(crate) fn build(scale: f64, dark: bool) -> Self {
        let size = window_px(scale);
        let layer = |content: f64, hover: f64, layer: Layer| render(size, scale, content, hover, dark, layer);
        let steps = 0..ENTER_FRAMES - 1;
        Self {
            dark,
            size,
            scale,
            enter: steps.clone().map(|i| layer(enter_scale(i), 0.0, Layer::Disc)).collect(),
            enter_shadow: steps.map(|i| layer(enter_scale(i), 0.0, Layer::Shadow)).collect(),
            normal: layer(1.0, 0.0, Layer::Disc),
            hover: layer(1.0, 1.0, Layer::Disc),
            pressed: layer(PRESSED_SCALE, 1.0, Layer::Disc),
            shadow: layer(1.0, 0.0, Layer::Shadow),
            shadow_pressed: layer(PRESSED_SCALE, 1.0, Layer::Shadow),
        }
    }

    /// 出现动画第 i 帧（0..ENTER_FRAMES）的（圆盘层, 投影层）
    pub(crate) fn enter_frame(&self, i: usize) -> (&[u8], &[u8]) {
        match (self.enter.get(i), self.enter_shadow.get(i)) {
            (Some(d), Some(s)) => (d, s),
            _ => (&self.normal, &self.shadow),
        }
    }

    /// 估算内存占用（字节）
    #[cfg(test)]
    pub(crate) fn bytes(&self) -> usize {
        self.normal.len() * (self.enter.len() + self.enter_shadow.len() + 5)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px_at(buf: &[u8], size: i32, x: i32, y: i32) -> [u8; 4] {
        let i = ((y * size + x) * 4) as usize;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    /// 像素中心到窗口中心的距离（物理像素）
    fn dist(size: i32, x: i32, y: i32) -> f64 {
        let c = f64::from(size) / 2.0;
        ((f64::from(x) + 0.5 - c).powi(2) + (f64::from(y) + 0.5 - c).powi(2)).sqrt()
    }

    /// 圆盘层叠在投影层上（预乘 source-over），即 DWM 合成两个窗口的结果
    fn composite(disc: &[u8], shadow: &[u8]) -> Vec<u8> {
        disc.chunks_exact(4)
            .zip(shadow.chunks_exact(4))
            .flat_map(|(d, s)| {
                let k = 1.0 - f64::from(d[3]) / 255.0;
                let mut out = [0u8; 4];
                for c in 0..4 {
                    out[c] = (f64::from(d[c]) + f64::from(s[c]) * k).round().min(255.0) as u8;
                }
                out
            })
            .collect()
    }

    #[test]
    fn easing_endpoints_and_monotonic() {
        assert!(ease_standard(0.0).abs() < 1e-6);
        assert!((ease_standard(1.0) - 1.0).abs() < 1e-6);
        assert!((ease_exit(1.0) - 1.0).abs() < 1e-6);
        let mut prev = 0.0;
        for i in 1..=20 {
            let v = ease_standard(f64::from(i) / 20.0);
            assert!(v >= prev);
            prev = v;
        }
        // standard 是「先快后慢」：中点已过半
        assert!(ease_standard(0.5) > 0.5);
        assert!(ease_exit(0.5) < 0.5);
    }

    #[test]
    fn enter_frames_are_evenly_spaced_and_follow_the_easing() {
        assert!((enter_scale(0) - ENTER_FROM_SCALE).abs() < 1e-9);
        assert!((enter_scale(ENTER_FRAMES - 1) - 1.0).abs() < 1e-9);
        // 等距：相邻两帧的缩放差相同，且在 22 pt 圆盘上不到 1 pt
        let step = enter_scale(1) - enter_scale(0);
        assert!(step * BOBBER_DIAMETER < 1.0, "step {step}");
        for i in 1..ENTER_FRAMES {
            assert!((enter_scale(i) - enter_scale(i - 1) - step).abs() < 1e-9);
        }
        // 帧下标随时间单调不减，从第一帧走到最后一帧
        assert_eq!(enter_index(0.0), 0);
        assert_eq!(enter_index(1.0), ENTER_FRAMES - 1);
        let mut prev = 0;
        for i in 0..=100 {
            let idx = enter_index(f64::from(i) / 100.0);
            assert!(idx >= prev);
            prev = idx;
        }
    }

    #[test]
    fn disc_layer_receives_clicks_only_inside_the_circle() {
        for scale in [1.0, 1.25, 1.5, 2.0, 3.0] {
            for dark in [false, true] {
                let size = window_px(scale);
                let disc = render(size, scale, 1.0, 0.0, dark, Layer::Disc);
                assert_eq!(disc.len(), (size * size * 4) as usize);
                let r = RADIUS * scale;
                for y in 0..size {
                    for x in 0..size {
                        let a = px_at(&disc, size, x, y)[3];
                        // 圆形（含 0.5 px 抗锯齿边缘）之外 alpha 必须为 0：分层窗口在那里点击穿透
                        if dist(size, x, y) > r + 0.5 {
                            assert_eq!(a, 0, "scale {scale}: ({x},{y}) outside the disc must be click-through");
                        }
                        // 圆形内部完全不透明
                        if dist(size, x, y) < r - 1.0 {
                            assert_eq!(a, 255, "scale {scale}: ({x},{y})");
                        }
                    }
                }
                // 圆盘左下部（避开字形）是品牌蓝（B 通道最大）
                let off = (6.0 * scale) as i32;
                let p = px_at(&disc, size, size / 2 - off, size / 2 + off / 2);
                assert!(p[0] > p[2], "disc should be blue: {p:?}");
                // 预乘：任何通道都不超过 alpha
                for px in disc.chunks_exact(4) {
                    assert!(px[0] <= px[3] && px[1] <= px[3] && px[2] <= px[3]);
                }
            }
        }
    }

    #[test]
    fn shadow_layer_sits_under_the_disc_edge_only() {
        for scale in [1.0, 2.0] {
            for dark in [false, true] {
                let size = window_px(scale);
                let shadow = render(size, scale, 1.0, 0.0, dark, Layer::Shadow);
                let r = RADIUS * scale;
                for y in 0..size {
                    for x in 0..size {
                        let p = px_at(&shadow, size, x, y);
                        // 纯黑投影：预乘后颜色通道为 0
                        assert_eq!(&p[..3], &[0, 0, 0]);
                        // 圆盘完全不透明的内部没有投影
                        if dist(size, x, y) < r - 1.5 {
                            assert_eq!(p[3], 0, "scale {scale}: ({x},{y})");
                        }
                    }
                }
                // 圆盘正下方可以看到投影；四角全透明
                let below = size / 2 + (r + 1.5 * scale).round() as i32;
                assert!(px_at(&shadow, size, size / 2, below)[3] > 0);
                assert_eq!(px_at(&shadow, size, 0, 0)[3], 0);
                assert_eq!(px_at(&shadow, size, size - 1, size - 1)[3], 0);
            }
        }
    }

    #[test]
    fn layers_composite_without_seams() {
        // 两层叠出的画面：圆盘内部就是圆盘本身，圆盘外就是投影，边缘没有缝隙（alpha 单调地从圆盘过渡到投影）
        let scale = 2.0;
        let size = window_px(scale);
        let disc = render(size, scale, 1.0, 0.0, false, Layer::Disc);
        let shadow = render(size, scale, 1.0, 0.0, false, Layer::Shadow);
        let both = composite(&disc, &shadow);
        let c = size / 2;
        let r = (RADIUS * scale) as i32;
        // 沿正下方的半径：圆盘内部不透明 → 边缘 → 投影逐渐变淡
        let alphas: Vec<u8> = (c + r - 3..size).map(|y| px_at(&both, size, c, y)[3]).collect();
        assert_eq!(alphas[0], 255);
        let peak = alphas.iter().position(|&a| a < 255).expect("edge");
        for w in alphas[peak..].windows(2) {
            assert!(w[1] <= w[0], "alpha should fade outward: {alphas:?}");
        }
        assert!(
            alphas[peak] > 40,
            "no light seam between the disc and its shadow: {alphas:?}"
        );
    }

    #[test]
    fn glyph_is_legible_at_100_percent() {
        // 100 % 缩放（22 px 圆盘）：字形腹部（源稿 (66, 64)，离轮廓约 2 pt）是近白色的不透明像素
        let scale = 1.0;
        let size = window_px(scale);
        let buf = render(size, scale, 1.0, 0.0, false, Layer::Disc);
        let c = f64::from(size) / 2.0;
        let (bx, by) = bobber_glyph::to_pt(66.0, 64.0);
        let p = px_at(&buf, size, (c + bx * scale) as i32, (c + by * scale) as i32);
        assert_eq!(p[3], 255);
        assert!(p[0] > 235 && p[1] > 235 && p[2] > 235, "body should be white: {p:?}");
        // 眼睛处露出圆盘的蓝色（B 通道明显高于 R）
        let (ex, ey) = bobber_glyph::to_pt(75.0, 50.0);
        let e = px_at(&buf, size, (c + ex * scale) as i32, (c + ey * scale) as i32);
        assert!(e[0] > e[2] + 40, "eye should show the disc: {e:?}");
        // 字形像素的数量足以辨认（头部 + 尾迹 + 纸片）
        let white = buf
            .chunks_exact(4)
            .filter(|p| p[3] == 255 && p[2] > 220 && p[1] > 220)
            .count();
        assert!(white > 40, "glyph pixels: {white}");
    }

    #[test]
    fn hover_brightens_and_pressed_shrinks() {
        let f = Frames::build(2.0, false);
        let size = f.size;
        let off = (6.0 * f.scale) as i32;
        let (x, y) = (size / 2 - off, size / 2 + off / 2);
        let n = px_at(&f.normal, size, x, y);
        let h = px_at(&f.hover, size, x, y);
        assert!(h[2] > n[2], "hover should be lighter: {n:?} → {h:?}");
        // 按下缩小：圆盘边缘附近的像素变得更透明
        let edge_x = size / 2 + (RADIUS * f.scale - 0.5) as i32;
        let normal_edge = px_at(&f.hover, size, edge_x, size / 2)[3];
        let pressed_edge = px_at(&f.pressed, size, edge_x, size / 2)[3];
        assert!(pressed_edge < normal_edge, "{normal_edge} → {pressed_edge}");
        // 投影随按下一起缩小（悬停态与静止态共用同一份投影）
        assert_ne!(f.shadow, f.shadow_pressed);
    }

    #[test]
    fn frames_and_blend() {
        let f = Frames::build(1.25, false);
        assert_eq!(f.enter.len(), ENTER_FRAMES - 1);
        assert_eq!(f.enter_shadow.len(), ENTER_FRAMES - 1);
        // 最后一帧就是静止态
        let (d, s) = f.enter_frame(ENTER_FRAMES - 1);
        assert_eq!(d, &f.normal[..]);
        assert_eq!(s, &f.shadow[..]);
        assert!(f.bytes() < 1024 * 1024, "浮标内存应远小于 1 MB");
        // 出现动画的第一帧明显小于静止态
        let opaque = |b: &[u8]| b.chunks_exact(4).filter(|p| p[3] == 255).count();
        assert!(opaque(f.enter_frame(0).0) < opaque(&f.normal));
        let mut out = vec![0u8; f.normal.len()];
        blend(&f.normal, &f.hover, 0.0, &mut out);
        assert_eq!(out, f.normal);
        blend(&f.normal, &f.hover, 1.0, &mut out);
        assert_eq!(out, f.hover);
    }
}
