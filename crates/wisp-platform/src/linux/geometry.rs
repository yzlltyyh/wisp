//! 显示器几何换算（纯逻辑，只依赖 std 与 wisp-types）。
//!
//! X11 的坐标都是物理像素、以虚拟桌面左上角为原点；上层统一使用「显示器 id + 显示器内逻辑坐标」。
//! X11 没有逐显示器缩放：缩放取 GDK 的整数窗口缩放（`GDK_SCALE` / XSETTINGS `Gdk/WindowScalingFactor`），
//! 与 Tauri（GTK）创建窗口时使用的缩放一致，面板定位才不会偏移。

use wisp_types::{MonitorInfo, PhysPoint, PhysSize, Point, Rect};

/// 物理像素矩形（可以为负坐标）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PxRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl PxRect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> i32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    /// 交集（不相交时返回 None）
    pub fn intersect(&self, o: &PxRect) -> Option<PxRect> {
        let x = self.x.max(o.x);
        let y = self.y.max(o.y);
        let r = self.right().min(o.right());
        let b = self.bottom().min(o.bottom());
        (r > x && b > y).then(|| PxRect::new(x, y, r - x, b - y))
    }

    fn area(&self) -> i64 {
        i64::from(self.w.max(0)) * i64::from(self.h.max(0))
    }

    /// 点到矩形的距离平方（点在内部时为 0）
    fn dist2(&self, x: i32, y: i32) -> i64 {
        let dx = if x < self.x {
            self.x - x
        } else if x >= self.right() {
            x - self.right() + 1
        } else {
            0
        };
        let dy = if y < self.y {
            self.y - y
        } else if y >= self.bottom() {
            y - self.bottom() + 1
        } else {
            0
        };
        i64::from(dx) * i64::from(dx) + i64::from(dy) * i64::from(dy)
    }
}

/// 一台显示器（物理像素）
#[derive(Debug, Clone, PartialEq)]
pub struct PhysMonitor {
    /// RandR 输出名（例如 `DP-1`），即 `MonitorId`
    pub id: String,
    /// 可读名称（EDID 名称，取不到时同 id）
    pub name: String,
    pub rect: PxRect,
    /// 工作区（扣除面板 / Dock 后的区域，物理像素）
    pub work_area: PxRect,
    pub primary: bool,
    pub scale: f64,
}

impl PhysMonitor {
    /// 转为上层使用的 `MonitorInfo`
    pub fn to_info(&self) -> MonitorInfo {
        let s = self.scale;
        let wa = self.work_area;
        MonitorInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            physical_origin: PhysPoint {
                x: self.rect.x,
                y: self.rect.y,
            },
            physical_size: PhysSize {
                width: self.rect.w.max(0) as u32,
                height: self.rect.h.max(0) as u32,
            },
            scale_factor: s,
            frame: Rect::new(
                f64::from(self.rect.x) / s,
                f64::from(self.rect.y) / s,
                f64::from(self.rect.w) / s,
                f64::from(self.rect.h) / s,
            ),
            work_area: Rect::new(
                f64::from(wa.x - self.rect.x) / s,
                f64::from(wa.y - self.rect.y) / s,
                f64::from(wa.w) / s,
                f64::from(wa.h) / s,
            ),
            is_primary: self.primary,
        }
    }

    /// 全局物理坐标 → 本显示器内逻辑坐标
    pub fn to_local(&self, x: i32, y: i32) -> Point {
        Point {
            x: f64::from(x - self.rect.x) / self.scale,
            y: f64::from(y - self.rect.y) / self.scale,
        }
    }

    /// 本显示器内逻辑坐标 → 全局物理坐标
    #[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
    pub fn to_global(&self, p: Point) -> (i32, i32) {
        (
            self.rect.x + (p.x * self.scale).round() as i32,
            self.rect.y + (p.y * self.scale).round() as i32,
        )
    }
}

/// 找到包含该点的显示器；都不包含时取最近的一台（指针恰好在显示器缝隙里时）
pub fn monitor_at(monitors: &[PhysMonitor], x: i32, y: i32) -> Option<&PhysMonitor> {
    monitors
        .iter()
        .find(|m| m.rect.contains(x, y))
        .or_else(|| monitors.iter().min_by_key(|m| m.rect.dist2(x, y)))
}

/// 选定显示器的工作区：
/// 1. `_GTK_WORKAREAS_D<n>`（Mutter 等按显示器给出的工作区列表）中与显示器交集最大的一块；
/// 2. 否则 `_NET_WORKAREA`（整个虚拟桌面的一块）与显示器的交集；
/// 3. 都没有时就是整台显示器
pub fn pick_work_area(monitor: PxRect, gtk_workareas: &[PxRect], net_workarea: Option<PxRect>) -> PxRect {
    let best = gtk_workareas
        .iter()
        .filter_map(|wa| monitor.intersect(wa))
        .max_by_key(PxRect::area);
    if let Some(wa) = best {
        return wa;
    }
    net_workarea
        .and_then(|wa| monitor.intersect(&wa))
        // 交集过小（例如 _NET_WORKAREA 只覆盖主屏）时视为不可信
        .filter(|wa| wa.area() * 2 >= monitor.area())
        .unwrap_or(monitor)
}

/// 整数缩放：`GDK_SCALE` 环境变量优先，其次 XSETTINGS，默认 1
pub fn gdk_scale(env_value: Option<&str>, xsettings: Option<u32>) -> f64 {
    let from_env = env_value
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|v| (1..=8).contains(v));
    f64::from(from_env.or(xsettings).unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(id: &str, rect: PxRect, scale: f64) -> PhysMonitor {
        PhysMonitor {
            id: id.into(),
            name: id.into(),
            rect,
            work_area: rect,
            primary: false,
            scale,
        }
    }

    #[test]
    fn info_conversion_with_scale() {
        let mut m = mon("DP-1", PxRect::new(3840, 0, 3840, 2160), 2.0);
        m.work_area = PxRect::new(3840, 64, 3840, 2096);
        let info = m.to_info();
        assert_eq!(info.frame, Rect::new(1920.0, 0.0, 1920.0, 1080.0));
        assert_eq!(info.work_area, Rect::new(0.0, 32.0, 1920.0, 1048.0));
        assert_eq!(info.physical_origin, PhysPoint { x: 3840, y: 0 });
        assert_eq!(m.to_local(3840 + 200, 100), Point { x: 100.0, y: 50.0 });
        assert_eq!(m.to_global(Point { x: 100.0, y: 50.0 }), (4040, 100));
    }

    #[test]
    fn monitor_lookup_falls_back_to_nearest() {
        let ms = vec![
            mon("A", PxRect::new(0, 0, 1920, 1080), 1.0),
            mon("B", PxRect::new(1920, 200, 1280, 1024), 1.0),
        ];
        assert_eq!(monitor_at(&ms, 10, 10).unwrap().id, "A");
        assert_eq!(monitor_at(&ms, 2000, 300).unwrap().id, "B");
        // 位于 B 上方的缝隙里
        assert_eq!(monitor_at(&ms, 2500, 100).unwrap().id, "B");
        assert!(monitor_at(&[], 0, 0).is_none());
    }

    #[test]
    fn work_area_selection() {
        let m = PxRect::new(1920, 0, 1920, 1080);
        let gtk = [PxRect::new(0, 32, 1920, 1048), PxRect::new(1920, 0, 1920, 1040)];
        assert_eq!(pick_work_area(m, &gtk, None), PxRect::new(1920, 0, 1920, 1040));
        let net = Some(PxRect::new(0, 32, 3840, 1048));
        assert_eq!(pick_work_area(m, &[], net), PxRect::new(1920, 32, 1920, 1048));
        // _NET_WORKAREA 只覆盖另一台显示器
        assert_eq!(pick_work_area(m, &[], Some(PxRect::new(0, 0, 1920, 1080))), m);
        assert_eq!(pick_work_area(m, &[], None), m);
    }

    #[test]
    fn scale_resolution() {
        assert_eq!(gdk_scale(Some("2"), Some(1)), 2.0);
        assert_eq!(gdk_scale(Some("abc"), Some(2)), 2.0);
        assert_eq!(gdk_scale(Some("0"), None), 1.0);
        assert_eq!(gdk_scale(None, None), 1.0);
    }
}
