//! 显示器枚举与光标位置（`CursorScreen` 的几何部分）。显示器 id 是 GDI 设备名 `\\.\DISPLAYn`（与 wisp-capture
//! 一致）；所有调用都在 Per-Monitor V2 作用域内进行，坐标一律为物理像素，再换算为显示器内逻辑坐标。
//!
//! 友好名称（「Dell U2720Q」）只在 [`monitors`] 里查询，每台显示器一次 `EnumDisplayDevicesW`、**不缓存**：拔掉一台
//! 显示器、换接另一台之后，同一个 `\\.\DISPLAYn` 常被分给新的物理显示器，按设备名缓存会一直显示旧名字。坐标换算、
//! 命中判定等内部路径（光标、外部点击、浮标、选区锚点）不需要名字，不做这次查询。

use ::windows::Win32::Foundation::{LPARAM, POINT, RECT};
use ::windows::Win32::Graphics::Gdi::{
    DISPLAY_DEVICEW, EnumDisplayDevicesW, EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR,
    MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL, MONITORINFO, MONITORINFOEXW, MonitorFromPoint,
};
use ::windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use ::windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, MONITORINFOF_PRIMARY};
use ::windows::core::{BOOL, PCWSTR};
use wisp_types::{MonitorId, MonitorInfo, Point};

use super::geom;
use super::util::{DpiScope, from_wide_buf};

/// 全部显示器（顺序与系统枚举顺序一致），含友好名称
pub(crate) fn monitors() -> Vec<MonitorInfo> {
    enumerate(true)
}

/// 全部显示器；`named` 为 false 时不查友好名称（`name` 用设备名代替，只供内部几何计算）
fn enumerate(named: bool) -> Vec<MonitorInfo> {
    let _dpi = DpiScope::per_monitor();
    let mut handles: Vec<HMONITOR> = Vec::new();

    unsafe extern "system" fn collect(h: HMONITOR, _hdc: HDC, _rc: *mut RECT, data: LPARAM) -> BOOL {
        // SAFETY: data 是下面传入的 `&mut Vec<HMONITOR>`，枚举是同步进行的，期间该 Vec 一直有效且无其他引用
        let v = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
        v.push(h);
        BOOL(1)
    }

    // SAFETY: 回调只在本调用期间被同步调用；LPARAM 指向局部 Vec
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM(&mut handles as *mut Vec<HMONITOR> as isize),
        );
    }
    handles.into_iter().filter_map(|h| describe(h, named)).collect()
}

/// 包含某物理点的显示器；不在任何显示器上时返回 None
pub(crate) fn monitor_at(x: i32, y: i32) -> Option<MonitorInfo> {
    let _dpi = DpiScope::per_monitor();
    // SAFETY: 纯查询
    let h = unsafe { MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONULL) };
    if h.is_invalid() { None } else { describe(h, false) }
}

/// 离某物理点最近的显示器
pub(crate) fn monitor_nearest(x: i32, y: i32) -> Option<MonitorInfo> {
    let _dpi = DpiScope::per_monitor();
    // SAFETY: 纯查询
    let h = unsafe { MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST) };
    if h.is_invalid() { None } else { describe(h, false) }
}

/// 按 id 查找显示器
#[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
pub(crate) fn monitor_by_id(id: &str) -> Option<MonitorInfo> {
    enumerate(false).into_iter().find(|m| m.id == id)
}

/// 光标的物理坐标
pub(crate) fn cursor_phys() -> Option<(i32, i32)> {
    let _dpi = DpiScope::per_monitor();
    let mut pt = POINT::default();
    // SAFETY: 输出参数指向局部变量
    unsafe { GetCursorPos(&mut pt) }.ok()?;
    Some((pt.x, pt.y))
}

/// 光标所在显示器与显示器内逻辑坐标
pub(crate) fn cursor() -> (MonitorId, Point) {
    let Some((x, y)) = cursor_phys() else {
        return fallback_cursor();
    };
    match monitor_nearest(x, y) {
        Some(m) => {
            let p = geom::phys_to_local(&m, f64::from(x), f64::from(y));
            (m.id, p)
        }
        None => fallback_cursor(),
    }
}

/// 物理点 → (显示器 id, 显示器内逻辑坐标)；点不在任何显示器上时取最近的显示器
pub(crate) fn phys_to_anchor_point(x: i32, y: i32) -> (MonitorId, Point) {
    match monitor_nearest(x, y) {
        Some(m) => {
            let p = geom::phys_to_local(&m, f64::from(x), f64::from(y));
            (m.id, p)
        }
        None => (
            MonitorId::new(),
            Point {
                x: f64::from(x),
                y: f64::from(y),
            },
        ),
    }
}

fn fallback_cursor() -> (MonitorId, Point) {
    let id = enumerate(false)
        .into_iter()
        .find(|m| m.is_primary)
        .map(|m| m.id)
        .unwrap_or_default();
    (id, Point::default())
}

fn describe(h: HMONITOR, named: bool) -> Option<MonitorInfo> {
    let mut mi = MONITORINFOEXW::default();
    mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: MONITORINFOEXW 以 MONITORINFO 开头，cbSize 声明了扩展结构的大小，这是该 API 的标准用法
    let ok = unsafe { GetMonitorInfoW(h, &mut mi as *mut MONITORINFOEXW as *mut MONITORINFO) };
    if !ok.as_bool() {
        return None;
    }
    let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
    // SAFETY: 输出参数指向局部变量；失败时保持 96
    if unsafe { GetDpiForMonitor(h, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }.is_err() {
        dpi_x = 96;
    }
    let id = from_wide_buf(&mi.szDevice);
    let name = if named { friendly_name(&id) } else { bare_device(&id) };
    let rc = mi.monitorInfo.rcMonitor;
    let wk = mi.monitorInfo.rcWork;
    Some(geom::monitor_info(
        id,
        name,
        (rc.left, rc.top, rc.right, rc.bottom),
        (wk.left, wk.top, wk.right, wk.bottom),
        dpi_x,
        mi.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
    ))
}

/// 去掉 `\\.\` 前缀的设备名（例如 `DISPLAY1`）
fn bare_device(device: &str) -> String {
    device.trim_start_matches("\\\\.\\").to_owned()
}

/// 显示器的友好名称（`EnumDisplayDevicesW` 的 DeviceString，例如「Dell U2720Q」或「Generic PnP Monitor」）；
/// 取不到时用去掉 `\\.\` 前缀的设备名。每次都实时查询（只在显式枚举显示器时发生，例如截图选屏、设置页），
/// 见模块文档
fn friendly_name(device: &str) -> String {
    let wide = super::util::wide(device);
    let mut dd = DISPLAY_DEVICEW {
        cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
        ..Default::default()
    };
    // SAFETY: 设备名以 NUL 结尾；输出结构体的 cb 已设置
    let ok = unsafe { EnumDisplayDevicesW(PCWSTR(wide.as_ptr()), 0, &mut dd, 0) };
    let name = ok
        .as_bool()
        .then(|| from_wide_buf(&dd.DeviceString).trim().to_owned())
        .filter(|s| !s.is_empty());
    name.unwrap_or_else(|| bare_device(device))
}
