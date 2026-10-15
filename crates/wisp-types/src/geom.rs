//! 几何类型。跨模块的位置一律用「显示器 id + 该显示器内的逻辑坐标」（`Anchor`）。

use serde::{Deserialize, Serialize};

/// 逻辑坐标点（逻辑 px）
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// 逻辑尺寸（逻辑 px）
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

/// 逻辑矩形（逻辑 px）
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    /// 构造矩形
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self { x, y, width, height }
    }

    /// 右边界 x
    pub fn right(&self) -> f64 {
        self.x + self.width
    }

    /// 下边界 y
    pub fn bottom(&self) -> f64 {
        self.y + self.height
    }

    /// 点是否落在矩形内（左闭右开）
    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.x < self.right() && p.y >= self.y && p.y < self.bottom()
    }
}

/// 物理像素坐标点（虚拟桌面中可以为负）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct PhysPoint {
    pub x: i32,
    pub y: i32,
}

/// 物理像素尺寸
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct PhysSize {
    pub width: u32,
    pub height: u32,
}

/// 物理像素矩形（相对某一帧图像的左上角，因此非负）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct PhysRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// 显示器稳定 id：macOS = CGDirectDisplayID（十进制）；Windows = 设备名 `\\.\DISPLAYn`；X11 = RandR 输出名。
///
/// 含反斜杠等字符，**不能原样放进窗口 label 或 `wisp://` 资源路径**：Overlay 窗口 label 只用序号 `overlay-<n>`
/// （label → 显示器由 `WindowManager::overlay_monitor` 反查）；冻结帧路径由 desktop 的 `assets::capture_asset_path`
/// 编码为十六进制
pub type MonitorId = String;

/// 显示器信息
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub id: MonitorId,
    pub name: String,
    /// 虚拟桌面中的物理像素坐标，可以为负
    pub physical_origin: PhysPoint,
    pub physical_size: PhysSize,
    pub scale_factor: f64,
    /// 全局逻辑坐标（macOS 是精确值；Win/Linux = 物理 / 缩放，只用于排序和命中判断）
    pub frame: Rect,
    /// 本显示器内的逻辑坐标（已扣除菜单栏、Dock、任务栏）
    pub work_area: Rect,
    pub is_primary: bool,
}

/// 锚点：显示器 id + 显示器内的逻辑矩形
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Anchor {
    pub monitor_id: MonitorId,
    /// 显示器内的逻辑坐标
    pub rect: Rect,
}
