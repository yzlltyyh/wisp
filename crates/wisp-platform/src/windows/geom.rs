//! 坐标换算（纯逻辑，不调用 Win32）：物理像素（虚拟桌面，可以为负）↔ 显示器内逻辑坐标；选区矩形合并与校验。浮标的位置与翻转规则在 `common::gesture`。

use wisp_types::{MonitorInfo, PhysPoint, PhysSize, Point, Rect};

/// 物理像素矩形（虚拟桌面坐标，左上闭、右下开）
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PhysRectF {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

impl PhysRectF {
    /// 由 (x, y, w, h) 构造
    pub(crate) fn from_xywh(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self {
            left: x,
            top: y,
            right: x + w,
            bottom: y + h,
        }
    }

    /// 宽
    pub(crate) fn width(&self) -> f64 {
        self.right - self.left
    }

    /// 高
    pub(crate) fn height(&self) -> f64 {
        self.bottom - self.top
    }

    /// 中心点
    pub(crate) fn center(&self) -> (f64, f64) {
        ((self.left + self.right) / 2.0, (self.top + self.bottom) / 2.0)
    }

    /// 并集
    pub(crate) fn union(&self, other: &Self) -> Self {
        Self {
            left: self.left.min(other.left),
            top: self.top.min(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }
}

/// 把 UIA `GetBoundingRectangles` 返回的扁平数组（每 4 个 double 为 x, y, w, h）合并成一个并集矩形。
/// 宽或高不为正、含非有限值的项被忽略；全部无效时返回 None
pub(crate) fn union_of_flat_rects(flat: &[f64]) -> Option<PhysRectF> {
    flat.chunks_exact(4)
        .filter(|c| c.iter().all(|v| v.is_finite()) && c[2] > 0.0 && c[3] > 0.0)
        .map(|c| PhysRectF::from_xywh(c[0], c[1], c[2], c[3]))
        .reduce(|a, b| a.union(&b))
}

/// 选区矩形是否可用作锚点：宽高为正、不是全零、中心落在该显示器内
pub(crate) fn plausible_anchor(rect: &PhysRectF, monitor: &MonitorInfo) -> bool {
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return false;
    }
    if rect.left == 0.0 && rect.top == 0.0 && rect.right == 0.0 && rect.bottom == 0.0 {
        return false;
    }
    let (cx, cy) = rect.center();
    let ox = f64::from(monitor.physical_origin.x);
    let oy = f64::from(monitor.physical_origin.y);
    cx >= ox
        && cy >= oy
        && cx < ox + f64::from(monitor.physical_size.width)
        && cy < oy + f64::from(monitor.physical_size.height)
}

/// 物理点 → 该显示器内的逻辑坐标
pub(crate) fn phys_to_local(monitor: &MonitorInfo, x: f64, y: f64) -> Point {
    let s = sane_scale(monitor.scale_factor);
    Point {
        x: (x - f64::from(monitor.physical_origin.x)) / s,
        y: (y - f64::from(monitor.physical_origin.y)) / s,
    }
}

/// 物理矩形 → 该显示器内的逻辑矩形
pub(crate) fn phys_rect_to_local(monitor: &MonitorInfo, r: &PhysRectF) -> Rect {
    let s = sane_scale(monitor.scale_factor);
    let p = phys_to_local(monitor, r.left, r.top);
    Rect::new(p.x, p.y, r.width() / s, r.height() / s)
}

/// 显示器内逻辑坐标 → 物理点（四舍五入）
#[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
pub(crate) fn local_to_phys(monitor: &MonitorInfo, p: Point) -> (i32, i32) {
    let s = sane_scale(monitor.scale_factor);
    (
        round_i32(f64::from(monitor.physical_origin.x) + p.x * s),
        round_i32(f64::from(monitor.physical_origin.y) + p.y * s),
    )
}

/// 缩放比例的保护：非有限或不为正时按 1 处理
pub(crate) fn sane_scale(s: f64) -> f64 {
    if s.is_finite() && s > 0.0 { s } else { 1.0 }
}

/// 饱和地四舍五入到 i32
pub(crate) fn round_i32(v: f64) -> i32 {
    if v.is_nan() {
        0
    } else {
        v.round().clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
    }
}

/// 物理像素坐标 → `SendInput` 的绝对坐标（`MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`，0..=65535 映射到整个
/// 虚拟桌面）。系统把归一化值 n 换回像素时取 ⌊n × extent / 65536⌋，这里取 ⌈(x − origin) × 65536 / extent⌉，
/// 换回后恰好落在像素 x 上（截断或四舍五入两种换算都成立）；越界时夹到边缘
pub(crate) fn abs_mouse_coord(x: i32, origin: i32, extent: i32) -> i32 {
    if extent <= 0 {
        return 0;
    }
    let offset = i64::from(x) - i64::from(origin);
    if offset <= 0 {
        return 0;
    }
    let extent = i64::from(extent);
    let n = (offset * 65536 + extent - 1) / extent;
    n.min(65535) as i32
}

/// 由显示器的物理几何构造 `MonitorInfo`（`frame` 是全局逻辑坐标 = 物理 / 缩放，只用于排序与命中判断）
pub(crate) fn monitor_info(
    id: String,
    name: String,
    monitor: (i32, i32, i32, i32),
    work: (i32, i32, i32, i32),
    dpi: u32,
    is_primary: bool,
) -> MonitorInfo {
    let scale = if dpi == 0 { 1.0 } else { f64::from(dpi) / 96.0 };
    let (ml, mt, mr, mb) = monitor;
    let (wl, wt, wr, wb) = work;
    let width = u32::try_from(mr.saturating_sub(ml)).unwrap_or(0);
    let height = u32::try_from(mb.saturating_sub(mt)).unwrap_or(0);
    MonitorInfo {
        id,
        name,
        physical_origin: PhysPoint { x: ml, y: mt },
        physical_size: PhysSize { width, height },
        scale_factor: scale,
        frame: Rect::new(
            f64::from(ml) / scale,
            f64::from(mt) / scale,
            f64::from(width) / scale,
            f64::from(height) / scale,
        ),
        work_area: Rect::new(
            f64::from(wl - ml) / scale,
            f64::from(wt - mt) / scale,
            f64::from(wr.saturating_sub(wl)) / scale,
            f64::from(wb.saturating_sub(wt)) / scale,
        ),
        is_primary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(x: i32, y: i32, w: u32, h: u32, scale: f64) -> MonitorInfo {
        let dpi = (scale * 96.0).round() as u32;
        monitor_info(
            "\\\\.\\DISPLAY1".into(),
            "Test".into(),
            (x, y, x + w as i32, y + h as i32),
            (x, y, x + w as i32, y + h as i32 - 48),
            dpi,
            true,
        )
    }

    #[test]
    fn union_merges_and_skips_invalid() {
        let flat = [
            10.0,
            10.0,
            5.0,
            5.0,
            0.0,
            0.0,
            0.0,
            0.0,
            30.0,
            12.0,
            10.0,
            4.0,
            f64::NAN,
            1.0,
            1.0,
            1.0,
        ];
        let r = union_of_flat_rects(&flat).unwrap();
        assert_eq!(
            r,
            PhysRectF {
                left: 10.0,
                top: 10.0,
                right: 40.0,
                bottom: 16.0
            }
        );
        assert!(union_of_flat_rects(&[0.0, 0.0, 0.0, 0.0]).is_none());
        assert!(union_of_flat_rects(&[]).is_none());
        // 不完整的尾部被忽略
        assert!(union_of_flat_rects(&[1.0, 2.0, 3.0]).is_none());
    }

    #[test]
    fn negative_secondary_monitor_at_150_percent() {
        // 副屏在主屏左侧：物理原点 (-2880, 0)，150 %
        let m = mon(-2880, 0, 2880, 1620, 1.5);
        let r = PhysRectF::from_xywh(-2700.0, 300.0, 300.0, 30.0);
        assert!(plausible_anchor(&r, &m));
        let local = phys_rect_to_local(&m, &r);
        assert!((local.x - 120.0).abs() < 1e-9);
        assert!((local.y - 200.0).abs() < 1e-9);
        assert!((local.width - 200.0).abs() < 1e-9);
        assert!((local.height - 20.0).abs() < 1e-9);
        assert_eq!(local_to_phys(&m, Point { x: 120.0, y: 200.0 }), (-2700, 300));
    }

    #[test]
    fn implausible_anchors_are_rejected() {
        let m = mon(0, 0, 1920, 1080, 1.0);
        assert!(!plausible_anchor(&PhysRectF::from_xywh(0.0, 0.0, 0.0, 0.0), &m));
        assert!(!plausible_anchor(&PhysRectF::from_xywh(10.0, 10.0, 0.0, 10.0), &m));
        // 中心在显示器外
        assert!(!plausible_anchor(&PhysRectF::from_xywh(5000.0, 10.0, 10.0, 10.0), &m));
        assert!(plausible_anchor(&PhysRectF::from_xywh(100.0, 100.0, 50.0, 20.0), &m));
    }

    #[test]
    fn absolute_mouse_coords_round_trip_to_the_same_pixel() {
        // 系统的换算：像素 = ⌊n × extent / 65536⌋
        let back = |n: i32, origin: i32, extent: i32| origin + (i64::from(n) * i64::from(extent) / 65536) as i32;
        // 单屏 1920 宽、双屏（副屏在左，虚拟桌面从 -2880 开始，共 4800 宽）、奇数宽度
        for (origin, extent) in [(0, 1920), (-2880, 4800), (0, 3841), (-1, 7)] {
            for x in [
                origin,
                origin + 1,
                origin + extent / 2,
                origin + extent - 2,
                origin + extent - 1,
            ] {
                let n = abs_mouse_coord(x, origin, extent);
                assert!((0..=65535).contains(&n));
                assert_eq!(
                    back(n, origin, extent),
                    x,
                    "x={x} origin={origin} extent={extent} n={n}"
                );
            }
        }
        // 越界夹到边缘；退化的虚拟桌面不 panic
        assert_eq!(abs_mouse_coord(-5000, -2880, 4800), 0);
        assert_eq!(abs_mouse_coord(9999, 0, 1920), 65535);
        assert_eq!(abs_mouse_coord(10, 0, 0), 0);
    }

    #[test]
    fn monitor_info_work_area_is_local_logical() {
        let m = mon(1920, 0, 2560, 1440, 1.25);
        assert_eq!(m.physical_origin, PhysPoint { x: 1920, y: 0 });
        assert!((m.scale_factor - 1.25).abs() < 1e-9);
        assert!((m.work_area.x).abs() < 1e-9);
        assert!((m.work_area.height - 1392.0 / 1.25).abs() < 1e-9);
        assert!((m.frame.x - 1536.0).abs() < 1e-9);
    }
}
