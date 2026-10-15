//! 窗口列表（窗口检测用）：显示器内逻辑坐标，按 z 序排列，最上层在前。
//!
//! 各后端只负责按 z 序（最上层在前）枚举可见窗口，给出「后端坐标空间」里的矩形：macOS 是全局点（pt），
//! Windows / X11 是虚拟桌面的物理像素。这里把它们统一切分到各显示器并换算成该显示器内的逻辑坐标——
//! 跨屏窗口在每个显示器上各得一条（与该显示器的交集），这样每个 Overlay 只拿到自己那一块。

use wisp_types::{MonitorId, Rect};

use crate::frame::WindowRect;

/// 后端枚举出的一个窗口（后端坐标空间，z 序由调用方的 Vec 顺序表达）
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RawWindow {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub app_name: String,
}

/// 一个显示器在后端坐标空间里的矩形，以及「后端单位 → 逻辑 px」的比例
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MonitorSpace {
    pub id: MonitorId,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// macOS 为 1（点即逻辑 px）；Windows / X11 为 1 / scale_factor
    pub unit_to_logical: f64,
}

/// 切分后小于它（逻辑 px）的窗口碎片丢弃：跨屏窗口探进相邻显示器的一两个像素无法被单击命中，只会干扰高亮
const MIN_FRAGMENT_LOGICAL: f64 = 4.0;

/// 把按 z 序排列的窗口切分到各显示器，换算为显示器内的逻辑坐标；输出保持输入的 z 序
pub(crate) fn assign_to_monitors(windows: &[RawWindow], monitors: &[MonitorSpace]) -> Vec<WindowRect> {
    let mut out = Vec::with_capacity(windows.len());
    for w in windows {
        if !(w.width.is_finite() && w.height.is_finite() && w.width > 0.0 && w.height > 0.0) {
            continue;
        }
        for m in monitors {
            let k = m.unit_to_logical;
            if !(k.is_finite() && k > 0.0) {
                continue;
            }
            let left = w.x.max(m.x);
            let top = w.y.max(m.y);
            let right = (w.x + w.width).min(m.x + m.width);
            let bottom = (w.y + w.height).min(m.y + m.height);
            let (lw, lh) = ((right - left) * k, (bottom - top) * k);
            if lw < MIN_FRAGMENT_LOGICAL || lh < MIN_FRAGMENT_LOGICAL {
                continue;
            }
            out.push(WindowRect {
                monitor_id: m.id.clone(),
                rect: Rect::new((left - m.x) * k, (top - m.y) * k, lw, lh),
                app_name: w.app_name.clone(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(x: f64, y: f64, w: f64, h: f64, name: &str) -> RawWindow {
        RawWindow {
            x,
            y,
            width: w,
            height: h,
            app_name: name.to_owned(),
        }
    }

    #[test]
    fn splits_spanning_window_and_keeps_z_order() {
        // 左屏 1×（物理 0..1920），右屏 2×（物理 1920..5760，逻辑宽 1920）
        let monitors = [
            MonitorSpace {
                id: "L".into(),
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
                unit_to_logical: 1.0,
            },
            MonitorSpace {
                id: "R".into(),
                x: 1920.0,
                y: 0.0,
                width: 3840.0,
                height: 2160.0,
                unit_to_logical: 0.5,
            },
        ];
        let windows = [
            raw(1820.0, 100.0, 300.0, 200.0, "top"),
            raw(2000.0, 200.0, 400.0, 400.0, "below"),
        ];
        let out = assign_to_monitors(&windows, &monitors);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].monitor_id, "L");
        assert_eq!(out[0].rect, Rect::new(1820.0, 100.0, 100.0, 200.0));
        assert_eq!(out[1].monitor_id, "R");
        assert_eq!(out[1].rect, Rect::new(0.0, 50.0, 100.0, 100.0));
        assert_eq!(out[1].app_name, "top");
        assert_eq!(out[2].app_name, "below");
        assert_eq!(out[2].rect, Rect::new(40.0, 100.0, 200.0, 200.0));
    }

    #[test]
    fn negative_origin_and_slivers() {
        // 主屏左侧的副屏原点为负
        let monitors = [MonitorSpace {
            id: "neg".into(),
            x: -1440.0,
            y: -200.0,
            width: 1440.0,
            height: 900.0,
            unit_to_logical: 1.0,
        }];
        let windows = [
            raw(-1000.0, -100.0, 500.0, 300.0, "a"),
            // 只探进 2 pt 的碎片被丢弃
            raw(-2.0, 0.0, 800.0, 600.0, "sliver"),
            // 非法尺寸被忽略
            raw(0.0, 0.0, f64::NAN, 10.0, "nan"),
        ];
        let out = assign_to_monitors(&windows, &monitors);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rect, Rect::new(440.0, 100.0, 500.0, 300.0));
    }
}
