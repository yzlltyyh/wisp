//! Windows / X11 浮标圆盘上的白色字形：应用图标的小尺寸字形 `wisp-mark-micro`（衔着纸片的小精灵：单眼、头顶卷翘、
//! 身后两道尾迹、实心纸片），源稿在 `apps/desktop/src-tauri/icons/src/wisp-mark-micro.svg`。
//!
//! 两端的浮标都是软件光栅化（Windows 的分层窗口、X11 的 ARGB 像素），没有矢量绘图库可用，所以这里把源稿路径原样
//! 抄成常量，第一次使用时把贝塞尔曲线展平成折线，按「点到轮廓的有符号距离」给出覆盖率需要的距离：调用方用
//! `0.5 - 距离（像素）` 做解析抗锯齿，与圆盘边缘的画法一致。几何与源稿完全相同（不是近似造型），22 pt 圆盘、100 %
//! 缩放下仍能看出头、眼睛、纸片与两道尾迹。纯逻辑，只依赖 std，可在任何平台上单测。

use std::sync::OnceLock;

/// 源稿路径的一段（100 × 100 画布，y 向下）
enum Seg {
    /// 三次贝塞尔：两个控制点 + 终点
    Cubic([(f64, f64); 3]),
    /// 直线到终点
    Line((f64, f64)),
}

/// 身体轮廓的起点（源稿 `M7 42`）
const BODY_START: (f64, f64) = (7.0, 42.0);
/// 身体轮廓（源稿第一条路径，`Z` 闭合回起点）：上尾迹 → 头顶卷翘 → 头 → 嘴部缺口 → 腹部 → 下尾迹 → 两道尾迹之间
const BODY: [Seg; 13] = [
    Seg::Cubic([(17.0, 32.0), (27.0, 36.0), (37.0, 43.0)]),
    Seg::Cubic([(46.0, 49.0), (51.0, 46.0), (61.0, 41.0)]),
    Seg::Cubic([(51.0, 40.0), (45.0, 34.0), (48.0, 25.0)]),
    Seg::Cubic([(49.0, 21.0), (52.0, 18.0), (56.0, 16.0)]),
    Seg::Cubic([(52.0, 28.0), (61.0, 30.0), (70.0, 32.0)]),
    Seg::Cubic([(81.0, 35.0), (87.0, 43.0), (90.0, 53.0)]),
    Seg::Line((83.0, 60.0)),
    Seg::Line((86.0, 68.0)),
    Seg::Cubic([(76.0, 81.0), (60.0, 85.0), (46.0, 78.0)]),
    Seg::Cubic([(34.0, 72.0), (27.0, 58.0), (14.0, 64.0)]),
    Seg::Cubic([(23.0, 54.0), (29.0, 55.0), (40.0, 61.0)]),
    Seg::Cubic([(46.0, 64.0), (51.0, 63.0), (55.0, 60.0)]),
    Seg::Cubic([(41.0, 62.0), (28.0, 44.0), (18.0, 42.0)]),
];
/// 眼睛：身体里挖出的圆孔（源稿 `M70 50 A5 5 …`，evenodd 填充）——圆心与半径
const EYE: (f64, f64, f64) = (75.0, 50.0, 5.0);
/// 纸片：嘴前的实心四边形（源稿第二条路径）
const PAPER: [(f64, f64); 4] = [(87.0, 59.0), (95.0, 56.0), (98.0, 66.0), (90.0, 69.0)];

/// 字形在圆盘里的摆放：源稿坐标的这一点对准圆盘中心（字形包围盒 x 7–98、y 16–81 的中心）
const ORIGIN: (f64, f64) = (52.5, 48.5);
/// 源稿单位 → pt。字形宽约 16 pt；离中心最远的纸片角落约在 0.77 倍半径处，四周留出与应用图标相近的边距
const UNIT_PT: f64 = 0.175;
/// 每段三次贝塞尔展平成的线段数：最长的一段约 9 pt，300 % 缩放下弦高误差也远小于 0.1 px
const CUBIC_STEPS: usize = 16;

/// 展平后的字形（pt，以圆盘中心为原点，y 向下）
pub struct Glyph {
    /// 身体轮廓的闭合折线
    body: Vec<(f64, f64)>,
    /// 眼睛（圆心与半径）
    eye: (f64, f64, f64),
    /// 纸片（凸四边形）
    paper: [(f64, f64); 4],
    /// 包围盒：x0, x1, y0, y1
    bounds: (f64, f64, f64, f64),
}

/// 源稿坐标 → 以圆盘中心为原点的 pt 坐标
pub fn to_pt(u: f64, v: f64) -> (f64, f64) {
    ((u - ORIGIN.0) * UNIT_PT, (v - ORIGIN.1) * UNIT_PT)
}

/// 字形（第一次调用时展平，之后复用）
pub fn glyph() -> &'static Glyph {
    static GLYPH: OnceLock<Glyph> = OnceLock::new();
    GLYPH.get_or_init(Glyph::build)
}

impl Glyph {
    fn build() -> Self {
        let mut body = Vec::with_capacity(BODY.len() * CUBIC_STEPS + 1);
        let mut at = BODY_START;
        body.push(to_pt(at.0, at.1));
        for seg in &BODY {
            match *seg {
                Seg::Line(p) => {
                    body.push(to_pt(p.0, p.1));
                    at = p;
                }
                Seg::Cubic([c1, c2, p]) => {
                    for i in 1..=CUBIC_STEPS {
                        let t = i as f64 / CUBIC_STEPS as f64;
                        let u = 1.0 - t;
                        let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
                        let x = a * at.0 + b * c1.0 + c * c2.0 + d * p.0;
                        let y = a * at.1 + b * c1.1 + c * c2.1 + d * p.1;
                        body.push(to_pt(x, y));
                    }
                    at = p;
                }
            }
        }
        let (ex, ey) = to_pt(EYE.0, EYE.1);
        let paper = PAPER.map(|(u, v)| to_pt(u, v));
        let mut bounds = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
        for &(x, y) in body.iter().chain(paper.iter()) {
            bounds = (bounds.0.min(x), bounds.1.max(x), bounds.2.min(y), bounds.3.max(y));
        }
        Self {
            body,
            eye: (ex, ey, EYE.2 * UNIT_PT),
            paper,
            bounds,
        }
    }

    /// 包围盒（pt）：`(x0, x1, y0, y1)`。盒外的点到字形的距离为正，调用方可以先用它跳过距离计算
    pub fn bounds(&self) -> (f64, f64, f64, f64) {
        self.bounds
    }

    /// 点 (x, y)（pt，以圆盘中心为原点，y 向下）到字形边界的有符号距离（pt，字形内部为负）
    pub fn distance(&self, x: f64, y: f64) -> f64 {
        let (d_body, in_body) = polygon(&self.body, x, y);
        let (ex, ey, er) = self.eye;
        let d_eye = ((x - ex).powi(2) + (y - ey).powi(2)).sqrt() - er;
        // 眼睛是身体上的孔：在身体内、眼睛外才算字形；到边界的距离取两者较近的一条
        let body = if in_body && d_eye > 0.0 {
            -d_body.min(d_eye)
        } else {
            d_body.min(d_eye.abs())
        };
        let (d_paper, in_paper) = polygon(&self.paper, x, y);
        let paper = if in_paper { -d_paper } else { d_paper };
        body.min(paper)
    }
}

/// 点到闭合折线的（无符号距离, 是否在内部）。内部按奇偶规则判断，与源稿的 evenodd 填充一致
fn polygon(pts: &[(f64, f64)], x: f64, y: f64) -> (f64, bool) {
    let mut d2 = f64::MAX;
    let mut inside = false;
    let Some(&last) = pts.last() else {
        return (f64::MAX, false);
    };
    let mut a = last;
    for &b in pts {
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let len2 = dx * dx + dy * dy;
        let t = if len2 > 0.0 {
            (((x - a.0) * dx + (y - a.1) * dy) / len2).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let (cx, cy) = (a.0 + dx * t, a.1 + dy * t);
        d2 = d2.min((x - cx).powi(2) + (y - cy).powi(2));
        // 向右的水平射线与这条边相交一次，内外翻转一次
        if (a.1 > y) != (b.1 > y) && x < a.0 + (y - a.1) / (b.1 - a.1) * dx {
            inside = !inside;
        }
        a = b;
    }
    (d2.sqrt(), inside)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inside(u: f64, v: f64) -> bool {
        let (x, y) = to_pt(u, v);
        glyph().distance(x, y) < 0.0
    }

    #[test]
    fn shape_matches_the_source_artwork() {
        // 头部、腹部、两道尾迹、头顶卷翘、纸片都是实心
        for (u, v) in [
            (66.0, 64.0),
            (70.0, 72.0),
            (25.0, 40.0),
            (30.0, 60.0),
            (52.0, 26.0),
            (93.0, 62.0),
        ] {
            assert!(inside(u, v), "({u}, {v}) 应在字形内");
        }
        // 眼睛是孔；两道尾迹之间、纸片与嘴之间、四角都是空的
        for (u, v) in [
            (75.0, 50.0),
            (20.0, 52.0),
            (86.5, 58.0),
            (5.0, 5.0),
            (95.0, 95.0),
            (40.0, 20.0),
        ] {
            assert!(!inside(u, v), "({u}, {v}) 应在字形外");
        }
    }

    #[test]
    fn distance_is_signed_and_continuous_across_the_edge() {
        let g = glyph();
        // 眼睛中心：距离约为眼睛半径（在孔里，为正）
        let (ex, ey) = to_pt(EYE.0, EYE.1);
        assert!((g.distance(ex, ey) - EYE.2 * UNIT_PT).abs() < 1e-9);
        // 沿水平线穿过眼睛左侧的身体：距离连续变化（相邻采样差不超过步长），符号在边界处翻转
        let step = 0.01;
        let (mut x, y) = to_pt(60.0, 50.0);
        let mut prev = g.distance(x, y);
        assert!(prev < 0.0);
        while x < ex {
            x += step;
            let d = g.distance(x, y);
            assert!((d - prev).abs() <= step + 1e-9, "{prev} → {d} @ {x}");
            prev = d;
        }
        assert!(prev > 0.0);
    }

    #[test]
    fn glyph_fits_inside_the_disc_with_a_margin() {
        // 浮标圆盘直径 22 pt（半径 11 pt）：字形离中心最远的点留出至少 2 pt 的边距，又不至于小到看不清
        let g = glyph();
        let far = g
            .body
            .iter()
            .chain(g.paper.iter())
            .map(|&(x, y)| (x * x + y * y).sqrt())
            .fold(0.0, f64::max);
        assert!(far > 7.5 && far < 9.0, "最远点 {far} pt");
        let (x0, x1, y0, y1) = g.bounds();
        assert!(x1 - x0 > 14.0 && x1 - x0 < 18.0, "宽 {}", x1 - x0);
        // 大致居中
        assert!((x0 + x1).abs() < 0.5 && (y0 + y1).abs() < 0.5, "{:?}", g.bounds());
    }
}
