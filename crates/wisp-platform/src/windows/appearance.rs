//! 系统外观：深浅色、任务栏明暗、强调色、降低动效 / 透明度，以及事件驱动的外观变化监听。
//!
//! 监听用一个**隐藏的顶层窗口**（message-only 窗口收不到 `WM_SETTINGCHANGE` 等广播），跑在专用线程上，
//! 空闲时阻塞在 `GetMessageW` 中，零唤醒；突发的多条广播用 80 ms 一次性定时器合并后再回调。
//!
//! `WM_SETTINGCHANGE` 是全系统最频繁的广播之一（环境变量、区域设置、组策略刷新、托盘设置、输入法、工作区与各种
//! `SPI_SET*`），只有与外观相关的几种才启动合并定时器（见 [`setting_affects_appearance`]），上层不会为无关的广播
//! 重新读取外观、向所有 WebView 广播；`WM_DWMCOLORIZATIONCOLORCHANGED` / `WM_THEMECHANGED` / `WM_SYSCOLORCHANGE`
//! 本身就只在外观变化时发出，照常处理。
//!
//! 显示缩放也算在内（托盘按 `SM_CXSMICON` 选图，见 `monitor::tray_icon_size`）：窗口建在 (0, 0)，也就是主显示器上，
//! 并且是 Per-Monitor V2 感知的，主显示器的缩放变化时收到 `WM_DPICHANGED`；换主显示器、改分辨率、接入或拔掉
//! 显示器时收到广播的 `WM_DISPLAYCHANGE`。两者都很少发生，照常合并后回调。

use std::cell::RefCell;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use ::windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use ::windows::Win32::Graphics::Dwm::DwmGetColorizationColor;
use ::windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, DestroyWindow, KillTimer, PostMessageW, PostQuitMessage, SPI_GETCLIENTAREAANIMATION,
    SPI_SETANIMATION, SPI_SETCLIENTAREAANIMATION, SPI_SETHIGHCONTRAST, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetTimer,
    SystemParametersInfoW, WM_CLOSE, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_DWMCOLORIZATIONCOLORCHANGED,
    WM_SETTINGCHANGE, WM_SYSCOLORCHANGE, WM_THEMECHANGED, WM_TIMER, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};
use ::windows::core::BOOL;

use crate::error::PlatformError;
use crate::types::HookHandle;

use super::util::{DpiScope, RawHwnd, call_guarded, reg_dword};
use super::wnd;

const PERSONALIZE: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";
const ACCENT: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\Accent";
const DWM: &str = r"Software\Microsoft\Windows\DWM";

/// 应用是否为深色（`AppsUseLightTheme == 0`；键不存在时按浅色）
pub(crate) fn dark_mode() -> bool {
    reg_dword(PERSONALIZE, "AppsUseLightTheme") == Some(0)
}

/// 任务栏 / 托盘区是否为浅色（`SystemUsesLightTheme == 1`；Windows 10 1903 之前没有该键，任务栏恒为深色）
pub(crate) fn taskbar_light() -> bool {
    reg_dword(PERSONALIZE, "SystemUsesLightTheme") == Some(1)
}

/// 「透明效果」是否关闭
pub(crate) fn reduce_transparency() -> bool {
    reg_dword(PERSONALIZE, "EnableTransparency") == Some(0)
}

/// 「动画效果」是否关闭（设置 → 辅助功能 → 视觉效果，对应 `SPI_GETCLIENTAREAANIMATION`）
pub(crate) fn reduce_motion() -> bool {
    let mut enabled = BOOL(1);
    // SAFETY: 输出参数是一个 BOOL，与该 action 的约定一致
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some(&mut enabled as *mut BOOL as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    ok.is_ok() && !enabled.as_bool()
}

/// 系统强调色 "#RRGGBB"：优先 `Explorer\Accent\AccentColorMenu`（ABGR），其次 `DWM\AccentColor`（ABGR），
/// 最后退回 `DwmGetColorizationColor`（ARGB）
pub(crate) fn accent() -> Option<String> {
    if let Some(abgr) = reg_dword(ACCENT, "AccentColorMenu").or_else(|| reg_dword(DWM, "AccentColor")) {
        return Some(abgr_to_hex(abgr));
    }
    let mut argb = 0u32;
    let mut opaque = BOOL(0);
    // SAFETY: 两个输出参数都指向局部变量
    unsafe { DwmGetColorizationColor(&mut argb, &mut opaque) }.ok()?;
    Some(argb_to_hex(argb))
}

fn abgr_to_hex(v: u32) -> String {
    let r = v & 0xFF;
    let g = (v >> 8) & 0xFF;
    let b = (v >> 16) & 0xFF;
    format!("#{r:02X}{g:02X}{b:02X}")
}

fn argb_to_hex(v: u32) -> String {
    format!("#{:06X}", v & 0x00FF_FFFF)
}

const CLASS: &str = "WispAppearanceSink";
const TIMER_COALESCE: usize = 1;
const COALESCE_MS: u32 = 80;

/// 与外观有关的 `WM_SETTINGCHANGE` 区域名（lParam）：深浅色 / 强调色 / 透明效果（设置 → 个性化 → 颜色），主题
const APPEARANCE_AREAS: [&str; 2] = ["ImmersiveColorSet", "WindowsThemeElement"];
/// 区域名最多读这么多个 UTF-16 单元（关心的名字都很短；更长的一定不是）
const MAX_AREA_UNITS: usize = 64;

/// 这条 `WM_SETTINGCHANGE` 是否与外观有关：wParam 是动画效果 / 高对比度的 `SPI_SET*`，或 lParam 是
/// [`APPEARANCE_AREAS`] 之一
fn setting_affects_appearance(wparam: WPARAM, lparam: LPARAM) -> bool {
    let spi = u32::try_from(wparam.0).unwrap_or(0);
    if [SPI_SETCLIENTAREAANIMATION, SPI_SETANIMATION, SPI_SETHIGHCONTRAST]
        .iter()
        .any(|a| a.0 == spi)
    {
        return true;
    }
    // SAFETY: WM_SETTINGCHANGE 的 lParam 为空，或指向系统为本进程封送的、以 NUL 结尾的区域名，消息处理期间有效
    let area = unsafe { area_name(lparam) };
    area.is_some_and(|a| APPEARANCE_AREAS.iter().any(|n| a.eq_ignore_ascii_case(n)))
}

/// lParam 指向的区域名：读到第一个 NUL 为止，最多 [`MAX_AREA_UNITS`] 个单元（不会越过 NUL 读取）；空指针或超长时
/// 为 None。
///
/// # Safety
/// `lparam` 必须为 0，或指向以 NUL 结尾的 UTF-16 字符串
unsafe fn area_name(lparam: LPARAM) -> Option<String> {
    let p = lparam.0 as *const u16;
    if p.is_null() {
        return None;
    }
    let mut units = Vec::with_capacity(32);
    for i in 0..MAX_AREA_UNITS {
        // SAFETY: 由调用方保证字符串以 NUL 结尾；遇到 NUL 立即停止，不会越界
        let c = unsafe { p.add(i).read_unaligned() };
        if c == 0 {
            return Some(String::from_utf16_lossy(&units));
        }
        units.push(c);
    }
    None
}

thread_local! {
    static CALLBACK: RefCell<Option<Arc<dyn Fn() + Send + Sync>>> = const { RefCell::new(None) };
}

/// 监听外观变化：回调在监听线程上执行，不带参数；返回的 Handle 被 drop 时销毁窗口并结束线程
pub(crate) fn watch(cb: Box<dyn Fn() + Send + Sync>) -> Result<HookHandle, PlatformError> {
    let cb: Arc<dyn Fn() + Send + Sync> = Arc::from(cb);
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<RawHwnd, String>>(1);
    std::thread::Builder::new()
        .name("wisp-appearance".into())
        .spawn(move || {
            if !wnd::ensure_class(CLASS, Some(wndproc), None) {
                let _ = ready_tx.send(Err("register class failed".into()));
                return;
            }
            // 只有 Per-Monitor 感知的窗口才收得到 `WM_DPICHANGED`：进程一般已经是 V2（tao 启动时设置），这里在建窗前
            // 再明确一次，不依赖调用顺序；作用域随线程一直保留，窗口的感知方式在创建时就定下了
            let _dpi = DpiScope::per_monitor();
            // 顶层、从不显示的工具窗口：能收到广播，不进任务栏 / Alt-Tab
            let hwnd = match wnd::create_window(CLASS, WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, WS_POPUP, None) {
                Ok(h) => h,
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return;
                }
            };
            CALLBACK.with(|c| *c.borrow_mut() = Some(cb));
            let _ = ready_tx.send(Ok(RawHwnd::new(hwnd)));
            wnd::run_message_loop(|_| false);
            CALLBACK.with(|c| *c.borrow_mut() = None);
        })
        .map_err(|e| PlatformError::Other(format!("spawn appearance watcher: {e}")))?;
    let hwnd = match ready_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => return Err(PlatformError::Other(format!("appearance watcher: {e}"))),
        Err(_) => return Err(PlatformError::Timeout),
    };
    Ok(HookHandle::new(move || {
        // SAFETY: 向监听窗口投递关闭消息；窗口已销毁时调用失败，忽略即可
        unsafe {
            let _ = PostMessageW(Some(hwnd.hwnd()), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }))
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_SETTINGCHANGE if !setting_affects_appearance(wparam, lparam) => LRESULT(0),
        WM_SETTINGCHANGE
        | WM_DWMCOLORIZATIONCOLORCHANGED
        | WM_THEMECHANGED
        | WM_SYSCOLORCHANGE
        | WM_DPICHANGED
        | WM_DISPLAYCHANGE => {
            // SAFETY: 定时器属于本窗口；重复 SetTimer 会重置计时，从而合并突发广播
            unsafe {
                SetTimer(Some(hwnd), TIMER_COALESCE, COALESCE_MS, None);
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_COALESCE => {
            // SAFETY: 一次性定时器，触发后立即注销
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_COALESCE);
            }
            let cb = CALLBACK.with(|c| c.borrow().clone());
            if let Some(cb) = cb {
                // 运行在窗口过程里：panic 不能越过 extern "system" 边界
                call_guarded("appearance watch callback", || cb());
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // SAFETY: 本线程创建的窗口
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: 结束本线程的消息循环
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: 其余消息交给默认处理
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_conversions() {
        // AccentColorMenu 是 ABGR：0xFFD77800 → R=00 G=78 B=D7
        assert_eq!(abgr_to_hex(0xFFD7_7800), "#0078D7");
        assert_eq!(argb_to_hex(0xC40078D7), "#0078D7");
    }

    #[test]
    fn only_appearance_settings_wake_the_watcher() {
        let area = |s: &str| super::super::util::wide(s);
        let check = |spi: u32, name: Option<&[u16]>| {
            let lparam = LPARAM(name.map_or(0, |n| n.as_ptr() as isize));
            setting_affects_appearance(WPARAM(spi as usize), lparam)
        };
        // 深浅色 / 强调色 / 透明效果、主题（大小写不敏感）
        assert!(check(0, Some(&area("ImmersiveColorSet"))));
        assert!(check(0, Some(&area("immersivecolorset"))));
        assert!(check(0, Some(&area("WindowsThemeElement"))));
        // 动画效果、高对比度
        assert!(check(SPI_SETCLIENTAREAANIMATION.0, None));
        assert!(check(SPI_SETHIGHCONTRAST.0, Some(&area(""))));
        // 无关的广播：环境变量、区域设置、组策略、托盘设置、工作区、空广播、超长区域名
        for name in ["Environment", "intl", "Policy", "TraySettings", "ImmersiveColorSetX"] {
            assert!(!check(0, Some(&area(name))), "{name}");
        }
        assert!(!check(0x002F, Some(&area(""))), "SPI_SETWORKAREA");
        assert!(!check(0, None));
        assert!(!check(0, Some(&area(&"x".repeat(200)))));
    }
}
