//! 浮标的软件光栅化（纯逻辑，只依赖 std 与同样只依赖 std 的 `common::bobber_glyph`）。
//!
//! 浮标是原生小窗（没有 WebView，内存不到 1 MB）。X11 下没有可用的矢量绘图库，这里用有符号距离场（SDF）+ 解析抗锯齿
//! 直接画出 34 pt 的 ARGB 图像：柔和投影、品牌蓝圆盘（22 pt，以 `#3266E3` 为中间色的轻微竖直渐变）、顶部高光描边、
//! 白色字形（应用图标的小尺寸字形 `wisp-mark-micro`，几何见 `common::bobber_glyph`，与 Windows 浮标相同）。每帧只有
//! 约 4–5 千个像素，字形的距离只在它的包围盒里计算，逐帧重绘出现动画（scale 0.6→1 + 淡入）的成本在 1 ms 以内，
//! 因此不需要预渲染位图。

use crate::common::bobber_glyph;

/// 一帧的绘制参数
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BobberStyle {
    /// 设备像素 / pt
    pub scale: f64,
    pub dark: bool,
    /// 悬停高亮进度 0–1（80 ms 过渡）
    pub hover: f64,
    /// 按下进度 0–1（按下时整体缩到 0.96）
    pub pressed: f64,
    /// 整体缩放（出现动画 0.6→1）
    pub zoom: f64,
    /// 整体不透明度 0–1
    pub opacity: f64,
}

/// 窗口边长（pt）与图标直径（pt）。与 `common::gesture::BOBBER_WINDOW` / `BOBBER_DIAMETER` 相同（本文件只依赖 std，
/// 以便在任何平台上单测；`x11::bobber` 的单测校验两者一致）
pub const WINDOW_PT: f64 = 34.0;
pub const ICON_PT: f64 = 22.0;

/// 圆盘渐变的上下两端（RGB）
const DISC_TOP: u32 = 0x3E6FE5;
const DISC_BOTTOM: u32 = 0x2F60D5;

/// 窗口边长（设备像素）
pub fn window_px(scale: f64) -> u32 {
    (WINDOW_PT * scale).round().max(1.0) as u32
}

/// 窗口内的设备像素 (`dx`, `dy`)（相对窗口左上角）是否落在圆盘上。与输入区域（[`disc_spans`]）一致：
/// 阴影边距可以点穿，命中测试也不能把它算作浮标
pub fn disc_contains(scale: f64, dx: i32, dy: i32) -> bool {
    let size = window_px(scale);
    let center = f64::from(size) / 2.0;
    let r = ICON_PT / 2.0 * scale;
    let (fx, fy) = (f64::from(dx) + 0.5 - center, f64::from(dy) + 0.5 - center);
    fx * fx + fy * fy < r * r
}

#[derive(Clone, Copy)]
struct Rgba {
    r: f64,
    g: f64,
    b: f64,
    a: f64,
}

impl Rgba {
    const fn hex(rgb: u32, a: f64) -> Self {
        Self {
            r: ((rgb >> 16) & 0xFF) as f64 / 255.0,
            g: ((rgb >> 8) & 0xFF) as f64 / 255.0,
            b: (rgb & 0xFF) as f64 / 255.0,
            a,
        }
    }

    fn mix(self, o: Rgba, t: f64) -> Rgba {
        Rgba {
            r: self.r + (o.r - self.r) * t,
            g: self.g + (o.g - self.g) * t,
            b: self.b + (o.b - self.b) * t,
            a: self.a + (o.a - self.a) * t,
        }
    }
}

/// 预乘 alpha 的累加像素
#[derive(Clone, Copy, Default)]
struct Px {
    r: f64,
    g: f64,
    b: f64,
    a: f64,
}

impl Px {
    /// source-over 合成：`c` 为非预乘颜色，`cov` 为覆盖率（已包含颜色自身的 alpha 之外的遮罩）
    fn over(&mut self, c: Rgba, cov: f64) {
        let a = (c.a * cov).clamp(0.0, 1.0);
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

fn smoothstep(e0: f64, e1: f64, x: f64) -> f64 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// 由有符号距离（单位：设备像素，内部为负）得到覆盖率
fn coverage(d: f64) -> f64 {
    (0.5 - d).clamp(0.0, 1.0)
}

/// 画一帧，返回 `window_px × window_px` 的预乘 ARGB 像素，按 X11 32 位 ZPixmap 小端字节序（B, G, R, A）排列
pub fn render(style: &BobberStyle) -> Vec<u8> {
    let size = window_px(style.scale);
    let s = style.scale;
    let center = f64::from(size) / 2.0;
    let zoom = (style.zoom * (1.0 - 0.04 * style.pressed.clamp(0.0, 1.0))).max(0.01);
    let opacity = style.opacity.clamp(0.0, 1.0);
    let radius = ICON_PT / 2.0;
    let glyph_shape = bobber_glyph::glyph();
    // 字形（含一个设备像素的抗锯齿边缘）只可能出现在这个包围盒里（pt），盒外的像素跳过距离计算
    let (gx0, gx1, gy0, gy1) = {
        let (x0, x1, y0, y1) = glyph_shape.bounds();
        let aa = 1.0 / (s * zoom);
        (x0 - aa, x1 + aa, y0 - aa, y1 + aa)
    };

    // 配色：品牌蓝 #3266E3 上下各向白 / 黑偏约 6 %（中部即品牌色）；深色主题下投影更重、描边更亮
    let top = Rgba::hex(DISC_TOP, 1.0);
    let bottom = Rgba::hex(DISC_BOTTOM, 1.0);
    let hover_tint = Rgba::hex(0xFFFFFF, 1.0);
    let shadow_alpha = if style.dark { 0.55 } else { 0.30 };
    let rim_alpha = if style.dark { 0.38 } else { 0.28 };
    let glyph = Rgba::hex(0xFFFFFF, 1.0);
    let hover = style.hover.clamp(0.0, 1.0) * 0.12;

    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for yi in 0..size {
        for xi in 0..size {
            // 像素中心 → 以图标中心为原点的 pt 坐标（再除以缩放实现「围绕中心缩放」）
            let ux = (f64::from(xi) + 0.5 - center) / s / zoom;
            let uy = (f64::from(yi) + 0.5 - center) / s / zoom;
            // 距离换算回设备像素用于抗锯齿
            let to_px = s * zoom;
            let mut px = Px::default();

            // 1. 投影：向下偏 1.2 pt、模糊约 3.5 pt
            let sd = ((ux * ux) + (uy - 1.2).powi(2)).sqrt() - radius;
            let shadow = (1.0 - smoothstep(-1.5, 3.5, sd)) * shadow_alpha;
            px.over(Rgba::hex(0x000000, 1.0), shadow);

            // 2. 圆盘：竖直渐变 + 悬停提亮
            let d = (ux * ux + uy * uy).sqrt() - radius;
            let disc_cov = coverage(d * to_px);
            if disc_cov > 0.0 {
                let t = ((uy + radius) / (2.0 * radius)).clamp(0.0, 1.0);
                let fill = top.mix(bottom, t).mix(hover_tint, hover);
                px.over(fill, disc_cov);
                // 3. 顶部高光描边（0.75 pt 内描边，自上而下淡出）
                let ring = coverage((d.abs() - 0.375) * to_px) * disc_cov;
                let ring_alpha = rim_alpha * (1.0 - t).powf(1.4);
                px.over(Rgba::hex(0xFFFFFF, ring_alpha), ring);
                // 4. 字形
                if (gx0..=gx1).contains(&ux) && (gy0..=gy1).contains(&uy) {
                    let gd = glyph_shape.distance(ux, uy);
                    let gc = coverage(gd * to_px) * disc_cov;
                    px.over(glyph, gc * 0.97);
                }
            }

            let a = (px.a * opacity).clamp(0.0, 1.0);
            let k = opacity;
            let to8 = |v: f64| (v * k * 255.0).round().clamp(0.0, 255.0) as u8;
            out.extend([
                to8(px.b),
                to8(px.g),
                to8(px.r),
                (a * 255.0).round().clamp(0.0, 255.0) as u8,
            ]);
        }
    }
    out
}

/// 没有合成器时的退化：不透明像素（24 位，B, G, R, 0），配合 [`disc_spans`] 的 SHAPE 遮罩使用
pub fn render_opaque(style: &BobberStyle) -> Vec<u8> {
    let mut flat = *style;
    flat.zoom = 1.0;
    flat.opacity = 1.0;
    let mut px = render(&flat);
    let size = window_px(style.scale) as usize;
    let hex = |v: u32| [(v >> 16) as u8, (v >> 8) as u8, v as u8];
    let fallback_bg = if style.dark { hex(DISC_BOTTOM) } else { hex(DISC_TOP) };
    for chunk in px.chunks_exact_mut(4).take(size * size) {
        let a = f64::from(chunk[3]) / 255.0;
        // 把预乘像素合成到圆盘底色上（遮罩外的像素不可见，取什么值都可以）
        for (i, c) in chunk.iter_mut().take(3).enumerate() {
            let bg = f64::from(fallback_bg[2 - i]);
            *c = (f64::from(*c) + bg * (1.0 - a)).round().clamp(0.0, 255.0) as u8;
        }
        chunk[3] = 0;
    }
    px
}

/// 圆盘的 1 位遮罩，按扫描线给出 (x, y, width) 的水平跨度（用于 SHAPE 扩展的 Rectangles 请求）
pub fn disc_spans(scale: f64) -> Vec<(u16, u16, u16)> {
    let size = window_px(scale);
    let center = f64::from(size) / 2.0;
    let r = ICON_PT / 2.0 * scale;
    let mut spans = Vec::new();
    for y in 0..size {
        let dy = f64::from(y) + 0.5 - center;
        if dy.abs() >= r {
            continue;
        }
        let half = (r * r - dy * dy).sqrt();
        let x0 = (center - half).round().max(0.0) as u16;
        let x1 = (center + half).round().min(f64::from(size)) as u16;
        if x1 > x0 {
            spans.push((x0, y as u16, x1 - x0));
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style(scale: f64) -> BobberStyle {
        BobberStyle {
            scale,
            dark: false,
            hover: 0.0,
            pressed: 0.0,
            zoom: 1.0,
            opacity: 1.0,
        }
    }

    fn pixel(buf: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * size + x) * 4) as usize;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    #[test]
    fn sizes_follow_scale() {
        assert_eq!(window_px(1.0), 34);
        assert_eq!(window_px(2.0), 68);
        assert_eq!(render(&style(2.0)).len(), 68 * 68 * 4);
    }

    #[test]
    fn premultiplied_and_shaped() {
        let s = style(2.0);
        let buf = render(&s);
        let size = window_px(2.0);
        // 角落几乎透明（只有极淡的投影）
        assert!(pixel(&buf, size, 0, 0)[3] < 8);
        // 圆盘内部不透明
        let left_inner = pixel(&buf, size, 34 - 18, 34);
        assert_eq!(left_inner[3], 255);
        // 预乘：任何颜色分量都不超过 alpha
        for px in buf.chunks_exact(4) {
            assert!(px[0] <= px[3] && px[1] <= px[3] && px[2] <= px[3], "{px:?}");
        }
        // 字形腹部（源稿 (66, 64)，离轮廓约 2 pt）是白色
        let (bx, by) = bobber_glyph::to_pt(66.0, 64.0);
        let body = pixel(&buf, size, (34.0 + bx * 2.0) as u32, (34.0 + by * 2.0) as u32);
        assert!(body[0] > 230 && body[1] > 230 && body[2] > 230, "{body:?}");
        // 眼睛处露出圆盘的蓝色
        let (ex, ey) = bobber_glyph::to_pt(75.0, 50.0);
        let eye = pixel(&buf, size, (34.0 + ex * 2.0) as u32, (34.0 + ey * 2.0) as u32);
        assert!(eye[0] > eye[2] + 60, "{eye:?}");
    }

    #[test]
    fn opacity_and_zoom_scale_output() {
        let mut s = style(1.0);
        s.opacity = 0.0;
        assert!(render(&s).iter().all(|&b| b == 0));
        s.opacity = 1.0;
        s.zoom = 0.6;
        let buf = render(&s);
        let size = window_px(1.0);
        // 缩小到 0.6 后，原来在圆盘边缘内侧的像素落到了圆盘外
        assert!(pixel(&buf, size, 17 - 10, 17)[3] < 200);
    }

    #[test]
    fn hover_brightens() {
        let mut s = style(1.0);
        let base = render(&s);
        s.hover = 1.0;
        let lit = render(&s);
        let size = window_px(1.0);
        let (a, b) = (pixel(&base, size, 9, 17), pixel(&lit, size, 9, 17));
        assert!(b[1] > a[1] && b[0] >= a[0], "{a:?} → {b:?}");
    }

    #[test]
    fn opaque_fallback_and_mask() {
        let s = style(1.0);
        let px = render_opaque(&s);
        assert!(px.chunks_exact(4).all(|p| p[3] == 0));
        let spans = disc_spans(1.0);
        assert!(!spans.is_empty());
        // 遮罩是以中心对称的圆：中间那行最宽，约等于直径
        let widest = spans.iter().map(|s| s.2).max().unwrap();
        assert!((21..=23).contains(&widest), "{widest}");
        assert!(spans.iter().all(|&(x, _, w)| x + w <= 34));
    }

    #[test]
    fn hit_test_matches_disc_not_shadow_margin() {
        for scale in [1.0, 2.0] {
            let size = window_px(scale) as i32;
            let c = size / 2;
            assert!(disc_contains(scale, c, c), "中心");
            // 阴影边距（窗口四角、四边中点）不算命中
            for (x, y) in [(0, 0), (size - 1, size - 1), (0, c), (c, 0), (size - 1, c)] {
                assert!(!disc_contains(scale, x, y), "({x}, {y}) @ {scale}");
            }
            // 与输入区域的扫描线一致
            for (x0, y, w) in disc_spans(scale) {
                let (x0, y, w) = (i32::from(x0), i32::from(y), i32::from(w));
                assert!(disc_contains(scale, x0 + w / 2, y));
            }
        }
    }
}
