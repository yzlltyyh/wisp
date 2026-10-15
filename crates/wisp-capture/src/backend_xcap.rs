//! Windows 后端：xcap（默认 GDI；出现整屏发白（xcap#130）时用本 crate 的 `wgc` feature 切到 Windows.Graphics.Capture，
//! 注意 Win10 上会出现黄色捕获边框）。
//!
//! 各显示器在独立线程里并行截取（xcap 的显示器句柄不能跨线程，每个线程按物理原点重新取得自己的显示器）；
//! xcap 已在捕获线程里完成 BGRA → RGBA，这里只把 alpha 统一写成 255（GDI 的第 4 字节未定义）。
//! 坐标全部是物理像素：desktop 进程按 PMv2 声明 DPI 感知，xcap 的显示器矩形与
//! `DWMWA_EXTENDED_FRAME_BOUNDS` 都不受 DPI 虚拟化影响。
//!
//! 窗口列表：EnumWindows（z 序自顶向下）+ IsWindowVisible + 未最小化 + 未被 cloak（其他虚拟桌面、UWP 挂起窗口），
//! 矩形取 DWMWA_EXTENDED_FRAME_BOUNDS（可见边框，不含阴影），排除自己、桌面（Progman / WorkerW）与点击穿透的覆盖层。

use std::collections::HashMap;
use std::ffi::c_void;
use std::time::Instant;

use image::RgbaImage;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GWL_EXSTYLE, GetClassNameW, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, IsIconic,
    IsWindowVisible, WS_EX_LAYERED, WS_EX_TRANSPARENT,
};
use windows::core::{BOOL, PWSTR};
use wisp_types::MonitorInfo;

use crate::error::CaptureError;
use crate::frame::{CaptureSet, Frame, WindowRect};
use crate::monitors::{self, RECT_TOLERANCE};
use crate::pixel;
use crate::windows_list::{MonitorSpace, RawWindow, assign_to_monitors};

/// 小于它（物理 px）的窗口不参与窗口检测
const MIN_WINDOW_PX: i32 = 8;

/// xcap 枚举出的一个显示器（只保留可跨线程的数据）
#[derive(Debug, Clone)]
struct Display {
    name: String,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

fn backend_err(context: &str, e: impl std::fmt::Display) -> CaptureError {
    CaptureError::Backend(format!("xcap {context}: {e}"))
}

/// 阻塞地并行捕获所有显示器
pub(crate) fn capture_all(monitors: &[MonitorInfo], exclude_pid: u32) -> Result<CaptureSet, CaptureError> {
    if monitors.is_empty() {
        return Err(CaptureError::NoDisplays);
    }
    let started = Instant::now();
    let displays = enumerate()?;
    let paired = monitors::pair(
        monitors,
        displays,
        |m, d| m.id == d.name,
        |m, d| {
            monitors::rect_close(
                monitors::physical_rect(m),
                (f64::from(d.x), f64::from(d.y), f64::from(d.width), f64::from(d.height)),
                RECT_TOLERANCE,
            )
        },
    );
    if paired.is_empty() {
        return Err(CaptureError::NoDisplays);
    }

    let (results, windows) = std::thread::scope(|scope| {
        let handles: Vec<_> = paired
            .iter()
            .map(|(_, display)| scope.spawn(move || capture_one(display)))
            .collect();
        // 截图线程工作时，本线程枚举窗口
        let windows = list_windows(&paired, exclude_pid);
        let results: Vec<_> = handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err(CaptureError::Backend("capture thread panicked".to_owned())))
            })
            .collect();
        (results, windows)
    });

    let mut frames = Vec::with_capacity(results.len());
    let mut first_error = None;
    for ((monitor, _), result) in paired.into_iter().zip(results) {
        match result {
            Ok(img) => {
                let (w, h) = img.dimensions();
                let mut rgba = img.into_raw();
                pixel::force_opaque(&mut rgba);
                frames.push(Frame::from_rgba(monitor, w, h, rgba));
            }
            Err(e) => {
                log::warn!("显示器 {} 截图失败：{e}", monitor.id);
                first_error.get_or_insert(e);
            }
        }
    }
    if frames.is_empty() {
        return Err(first_error.unwrap_or(CaptureError::NoDisplays));
    }
    log::debug!(
        "xcap 捕获 {} 个显示器、{} 个窗口矩形，用时 {} ms",
        frames.len(),
        windows.len(),
        started.elapsed().as_millis()
    );
    Ok(CaptureSet { frames, windows })
}

/// 枚举显示器（设备名 `\\.\DISPLAYn` 与物理矩形）
fn enumerate() -> Result<Vec<Display>, CaptureError> {
    let all = xcap::Monitor::all().map_err(|e| backend_err("enumerate monitors", e))?;
    let mut out = Vec::with_capacity(all.len());
    for m in all {
        let item = (|| -> xcap::XCapResult<Display> {
            Ok(Display {
                name: m.name()?,
                x: m.x()?,
                y: m.y()?,
                width: m.width()?,
                height: m.height()?,
            })
        })();
        match item {
            Ok(d) => out.push(d),
            Err(e) => log::warn!("读取显示器信息失败：{e}"),
        }
    }
    Ok(out)
}

/// 在当前线程里截取一个显示器（按物理原点重新取得 xcap 句柄，核对设备名后截图）
fn capture_one(display: &Display) -> Result<RgbaImage, CaptureError> {
    let by_point = xcap::Monitor::from_point(display.x, display.y)
        .ok()
        .filter(|m| m.name().ok().as_deref() == Some(display.name.as_str()));
    let monitor = match by_point {
        Some(m) => m,
        None => xcap::Monitor::all()
            .map_err(|e| backend_err("enumerate monitors", e))?
            .into_iter()
            .find(|m| m.name().ok().as_deref() == Some(display.name.as_str()))
            .ok_or_else(|| CaptureError::Backend(format!("monitor {} disappeared", display.name)))?,
    };
    monitor.capture_image().map_err(|e| backend_err("capture", e))
}

/// 可见的顶层窗口，按 z 序最上层在前，切分到各显示器
fn list_windows(paired: &[(MonitorInfo, Display)], exclude_pid: u32) -> Vec<WindowRect> {
    let mut hwnds: Vec<HWND> = Vec::with_capacity(256);
    // SAFETY: 回调只在 EnumWindows 返回前被同步调用，期间 hwnds 一直有效且没有其他引用
    let enumerated = unsafe { EnumWindows(Some(collect_hwnd), LPARAM(&raw mut hwnds as isize)) };
    if let Err(e) = enumerated {
        log::warn!("EnumWindows 失败，窗口检测不可用：{e}");
        return Vec::new();
    }

    let mut names: HashMap<u32, String> = HashMap::new();
    let mut raws = Vec::with_capacity(hwnds.len() / 2);
    for hwnd in hwnds {
        if !is_candidate(hwnd) {
            continue;
        }
        let mut pid = 0u32;
        // SAFETY: hwnd 来自 EnumWindows；pid 是有效的可写指针
        unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut pid)) };
        if pid == exclude_pid {
            continue;
        }
        let Some(r) = visible_bounds(hwnd) else {
            continue;
        };
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        if w < MIN_WINDOW_PX || h < MIN_WINDOW_PX {
            continue;
        }
        let app_name = names.entry(pid).or_insert_with(|| process_name(pid)).clone();
        raws.push(RawWindow {
            x: f64::from(r.left),
            y: f64::from(r.top),
            width: f64::from(w),
            height: f64::from(h),
            app_name,
        });
    }

    let spaces: Vec<MonitorSpace> = paired
        .iter()
        .map(|(m, d)| MonitorSpace {
            id: m.id.clone(),
            x: f64::from(d.x),
            y: f64::from(d.y),
            width: f64::from(d.width),
            height: f64::from(d.height),
            unit_to_logical: 1.0
                / if m.scale_factor.is_finite() && m.scale_factor > 0.0 {
                    m.scale_factor
                } else {
                    1.0
                },
        })
        .collect();
    assign_to_monitors(&raws, &spaces)
}

unsafe extern "system" fn collect_hwnd(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: lparam 是 list_windows 传入的 `*mut Vec<HWND>`，EnumWindows 返回前一直有效、只有这里在用
    let list = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
    list.push(hwnd);
    TRUE
}

/// 可见、未最小化、未被 cloak、不是桌面或点击穿透覆盖层的窗口
fn is_candidate(hwnd: HWND) -> bool {
    // SAFETY: hwnd 来自 EnumWindows，这些查询对已销毁的句柄也只是返回失败
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return false;
        }
        let mut cloaked = 0u32;
        let ok = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&raw mut cloaked).cast::<c_void>(),
            size_of::<u32>() as u32,
        );
        if ok.is_ok() && cloaked != 0 {
            return false;
        }
        // 扩展样式是 32 位标志，GetWindowLongPtrW 返回的 isize 只用低 32 位
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TRANSPARENT.0 != 0 && ex & WS_EX_LAYERED.0 != 0 {
            return false;
        }
        let mut class = [0u16; 32];
        let len = GetClassNameW(hwnd, &mut class);
        let class = String::from_utf16_lossy(&class[..usize::try_from(len).unwrap_or(0)]);
        !matches!(class.as_str(), "Progman" | "WorkerW")
    }
}

/// 可见边框（DWMWA_EXTENDED_FRAME_BOUNDS，不含 Win10/11 的透明调整边框与阴影）；DWM 不可用时退回 GetWindowRect
fn visible_bounds(hwnd: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    // SAFETY: r 是有效的可写缓冲，大小与属性类型一致
    let dwm = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&raw mut r).cast::<c_void>(),
            size_of::<RECT>() as u32,
        )
    };
    if dwm.is_ok() {
        return Some(r);
    }
    // SAFETY: 同上
    unsafe { GetWindowRect(hwnd, &raw mut r) }.ok().map(|()| r)
}

/// 进程的可执行文件名（不含扩展名），取不到时为空串
fn process_name(pid: u32) -> String {
    // SAFETY: OpenProcess 失败返回错误；成功时得到的句柄在下面关闭
    let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return String::new();
    };
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: buf 可写 len 个 u16；成功时 len 被改写为实际长度（不含结尾 0）
    let ok = unsafe { QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &raw mut len) };
    // SAFETY: handle 由上面的 OpenProcess 返回，只关闭一次
    let _ = unsafe { CloseHandle(handle) };
    if ok.is_err() {
        return String::new();
    }
    let path = String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]);
    std::path::Path::new(&path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 冒烟测试用：按 xcap 枚举显示器，构造与 platform 层同口径的 `MonitorInfo`
/// （先把测试进程设为 PMv2 DPI 感知，与 desktop 进程一致，否则坐标会被 DPI 虚拟化）
#[cfg(test)]
pub(crate) fn system_monitors() -> Vec<MonitorInfo> {
    use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
    use wisp_types::{PhysPoint, PhysSize, Rect};

    // SAFETY: 只修改本测试进程的 DPI 感知模式；已经设置过时返回错误，忽略即可
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let Ok(all) = xcap::Monitor::all() else {
        return Vec::new();
    };
    all.into_iter()
        .filter_map(|m| {
            let (name, x, y) = (m.name().ok()?, m.x().ok()?, m.y().ok()?);
            let (w, h) = (m.width().ok()?, m.height().ok()?);
            let scale = f64::from(m.scale_factor().unwrap_or(1.0));
            let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
            let (lw, lh) = (f64::from(w) / scale, f64::from(h) / scale);
            Some(MonitorInfo {
                id: name.clone(),
                name,
                physical_origin: PhysPoint { x, y },
                physical_size: PhysSize { width: w, height: h },
                scale_factor: scale,
                frame: Rect::new(f64::from(x) / scale, f64::from(y) / scale, lw, lh),
                work_area: Rect::new(0.0, 0.0, lw, lh),
                is_primary: m.is_primary().unwrap_or(false),
            })
        })
        .collect()
}
