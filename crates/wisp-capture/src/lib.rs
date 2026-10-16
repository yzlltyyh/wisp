//! Wisp 的屏幕捕获。
//!
//! 职责：并行捕获所有显示器（macOS 用 ScreenCaptureKit，Windows 用 xcap，X11 用 x11rb 的 MIT-SHM / GetImage，
//! Wayland 用 portal）；把后端的显示器与 platform 给出的 `MonitorInfo` 按物理矩形匹配；获取窗口列表；
//! 把逻辑选区换算成物理像素并裁剪；按长边缩放；PNG / JPEG 编码与体积回退；生成冻结帧的原始 RGBA。
//!
//! 公开 API 之外另有可选的优化入口 [`prewarm`]，
//! 以及区分「用户取消」的 [`CaptureError::Cancelled`]（[`CaptureError::is_cancelled`]）。
//!
//! 资源约束：没有常驻线程或周期定时器，空闲时零 CPU。捕获结果全部由调用方持有，`CaptureSet` 被丢弃时整屏像素随之
//! 释放（Overlay 销毁后不保留整屏数据）；截图从不写盘。仅有的常驻状态都在 macOS 后端里：至多一份
//! [`prewarm`] 预取的 SCShareableContent 快照（最长 1.5 s，到期由 GCD 一次性延时任务释放），以及自身应用的
//! SCRunningApplication 句柄（几百字节）。X11 的每次捕获在一个短命线程里执行，以便施加硬超时。

mod crop;
mod encode;
mod error;
mod frame;
mod monitors;
mod pixel;
mod windows_list;
#[cfg(any(target_os = "linux", test))]
mod zpixmap;

#[cfg(target_os = "macos")]
mod backend_macos;
#[cfg(all(target_os = "linux", feature = "wayland"))]
mod backend_portal;
#[cfg(target_os = "linux")]
mod backend_x11;
#[cfg(target_os = "windows")]
mod backend_xcap;

pub use crop::{crop, fit_long_edge, to_physical};
pub use encode::{EncodedImage, decode_image, encode_for_llm, encode_png, fits_protocol, reencode_for_llm};
pub use error::CaptureError;
pub use frame::{CaptureSet, Frame, WindowRect};
/// 裁剪与解码得到的像素（调用方不必为了写出类型名而直接依赖 image）
pub use image::RgbaImage;

use wisp_types::{MonitorInfo, PermissionState};

/// 屏幕录制权限预检（macOS 用 CGPreflightScreenCaptureAccess，绝不弹窗；其他平台返回 NotApplicable）。
///
/// macOS 的预检只能区分「可用 / 不可用」，不可用（含从未询问过）报告为 `Denied`
pub fn preflight() -> PermissionState {
    #[cfg(target_os = "macos")]
    {
        backend_macos::preflight()
    }
    #[cfg(not(target_os = "macos"))]
    {
        PermissionState::NotApplicable
    }
}

/// 可选的预热：截图热键按下时调用（在热键回调里、隐藏面板之前），让后端提前完成与截图内容无关的准备工作。
///
/// macOS：异步发起 SCShareableContent 查询（实测冷 ~94 ms 的捕获降到 ~30 ms），`capture_all` 在 1.5 s 内被调用时
/// 直接使用其结果；结果没被用掉时 1.5 s 后自动释放。还不知道 Wisp 自身的 SCRunningApplication 时顺带把它查出来。
/// 没有屏幕录制权限时什么也不做（不会触发系统弹窗）。其他平台为空操作。非阻塞，可以在任意线程调用
pub fn prewarm() {
    #[cfg(target_os = "macos")]
    backend_macos::prewarm();
}

/// 阻塞调用；各显示器并行捕获，排除 `exclude_pid` 的窗口。
///
/// - `frames` 按 `monitors` 的顺序给出，只包含捕获成功的显示器（后端里找不到或单屏失败的会被跳过并记日志）；
///   一个都没有成功时返回第一个错误（全部是「找不到」时为 `NoDisplays`）；
/// - `windows` 是窗口检测用的可见窗口（显示器内逻辑坐标，按 z 序，最上层在前），已排除 `exclude_pid`；
/// - macOS 没有屏幕录制权限时立即返回 `PermissionDenied`（不弹窗）；Windows / X11 截图前由调用方隐藏自己的窗口，
///   macOS 由 SCK 过滤器直接排除 `exclude_pid` 的全部窗口（菜单栏上它自己的状态项除外，冻结帧与实时菜单栏一致）；
/// - X11：Wayland 会话（XWayland 下只能截到黑屏）返回 `Unsupported`，请改用 `portal_screenshot`；
///   整个调用有 5 s 硬超时，X 服务器无响应时返回 `Backend`
pub fn capture_all(monitors: &[MonitorInfo], exclude_pid: u32) -> Result<CaptureSet, CaptureError> {
    #[cfg(target_os = "macos")]
    {
        backend_macos::capture_all(monitors, exclude_pid)
    }
    #[cfg(target_os = "windows")]
    {
        backend_xcap::capture_all(monitors, exclude_pid)
    }
    #[cfg(target_os = "linux")]
    {
        backend_x11::capture_all(monitors, exclude_pid)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = (monitors, exclude_pid);
        Err(CaptureError::Unsupported(
            "screen capture is not supported on this OS".to_owned(),
        ))
    }
}

/// Wayland：通过 xdg-desktop-portal 截图。
///
/// 交互式（`interactive = true`）：由系统负责框选，返回用户选定区域的图像（物理像素）。
///
/// - 用户取消：返回 [`CaptureError::Cancelled`]，调用方静默结束，不显示错误卡片；
/// - 没有截图门户 / 没有 D-Bus：返回 `Unsupported`（调用方据此显示「当前桌面不支持截图门户」）；
/// - 门户写到磁盘的临时图片读出后立即删除
#[cfg(all(target_os = "linux", feature = "wayland"))]
pub async fn portal_screenshot() -> Result<image::RgbaImage, CaptureError> {
    backend_portal::screenshot().await
}

#[cfg(all(test, any(target_os = "macos", target_os = "windows", target_os = "linux")))]
mod smoke {
    //! 实机冒烟测试：需要真实显示器（macOS 另需屏幕录制权限），默认忽略。
    //! 运行：`cargo test -p wisp-capture -- --ignored capture_smoke --nocapture`

    use std::time::{Duration, Instant};

    use super::*;

    #[cfg(target_os = "macos")]
    use backend_macos as backend;
    #[cfg(target_os = "linux")]
    use backend_x11 as backend;
    #[cfg(target_os = "windows")]
    use backend_xcap as backend;

    #[test]
    #[ignore = "需要屏幕录制权限与真实显示器"]
    fn capture_smoke_all_displays_full_resolution() {
        #[cfg(target_os = "macos")]
        assert!(
            preflight() == PermissionState::Granted,
            "没有屏幕录制权限：请在 系统设置 → 隐私与安全性 → 屏幕录制 中授权运行测试的终端后重试"
        );
        let monitors = backend::system_monitors();
        assert!(!monitors.is_empty(), "没有活动显示器");

        // 第一次是冷启动，第二次先预热（macOS 预取 SCShareableContent），代表热键触发时的真实耗时
        let cold = Instant::now();
        let set = capture_all(&monitors, std::process::id()).expect("捕获失败");
        let cold_ms = cold.elapsed().as_millis();
        prewarm();
        std::thread::sleep(Duration::from_millis(100));
        let warm = Instant::now();
        let set2 = capture_all(&monitors, std::process::id()).expect("第二次捕获失败");
        let warm_ms = warm.elapsed().as_millis();

        // 整屏 RGBA 尚未转出时，裁剪只转换选区（macOS 走 CGImage 子图）；结果必须与整屏转换后再裁剪逐像素相同
        for frame in &set2.frames {
            let local = wisp_types::Rect::new(37.0, 21.0, 211.0, 97.0);
            let t = Instant::now();
            let lazy = crop(frame, local).expect("选区裁剪失败");
            let lazy_us = t.elapsed().as_micros();
            let full = frame.rgba();
            let eager = crop(frame, local).expect("整屏就绪后的裁剪失败");
            assert_eq!(lazy, eager, "选区转换与整屏转换的像素不一致");
            assert_eq!(full.len(), (frame.width * frame.height * 4) as usize);
            report(&format!("display {}: lazy crop {lazy_us}µs", frame.monitor.id));
        }
        drop(set2);

        assert_eq!(set.frames.len(), monitors.len(), "每个显示器都应有一帧");
        for (frame, monitor) in set.frames.iter().zip(&monitors) {
            assert_eq!(frame.monitor.id, monitor.id);
            let expected_w = (monitor.frame.width * monitor.scale_factor).round() as u32;
            let expected_h = (monitor.frame.height * monitor.scale_factor).round() as u32;
            assert_eq!(
                (frame.width, frame.height),
                (expected_w, expected_h),
                "显示器 {} 应为全分辨率（逻辑尺寸 × {}）",
                monitor.id,
                monitor.scale_factor
            );
            #[cfg(target_os = "macos")]
            assert!(frame.cg_image().is_some(), "SCK 帧必须带 CGImage");

            let t = Instant::now();
            let rgba = frame.rgba();
            let rgba_ms = t.elapsed().as_millis();
            assert_eq!(rgba.len(), (frame.width * frame.height * 4) as usize);
            assert!(rgba.chunks_exact(4).all(|p| p[3] == 255), "冻结帧必须不透明");
            assert!(rgba.chunks_exact(4).any(|p| p[..3] != [0, 0, 0]), "冻结帧不应全黑");
            let t = Instant::now();
            let (hw, hh, half) = frame.rgba_half();
            let half_ms = t.elapsed().as_millis();
            assert_eq!(half.len(), (hw * hh * 4) as usize);
            report(&format!(
                "display {}: {}x{} @{} rgba={rgba_ms}ms half={half_ms}ms",
                monitor.id, frame.width, frame.height, monitor.scale_factor
            ));

            // 裁剪：选区 (10, 20) 起、100×50 逻辑 px → 物理像素尺寸按 scale 换算
            let img = crop(frame, wisp_types::Rect::new(10.0, 20.0, 100.0, 50.0)).expect("裁剪失败");
            let s = monitor.scale_factor;
            assert_eq!(
                img.dimensions(),
                ((100.0 * s).round() as u32, (50.0 * s).round() as u32)
            );
            assert!(encode_png(&img).bytes.starts_with(b"\x89PNG"));
        }
        report(&format!(
            "capture_all cold={cold_ms}ms warm={warm_ms}ms windows={}",
            set.windows.len()
        ));
        for w in set.windows.iter().take(8) {
            report(&format!("  window {:?} on {} @ {:?}", w.app_name, w.monitor_id, w.rect));
        }

        // macOS：没被消费的预取结果到期后自动释放（GCD 一次性延时任务），不会一直留在内存里
        #[cfg(target_os = "macos")]
        {
            prewarm();
            assert!(backend::prefetch_pending(), "prewarm 之后应有一份预取结果");
            std::thread::sleep(backend::PREFETCH_TTL_FOR_TEST + Duration::from_millis(300));
            assert!(!backend::prefetch_pending(), "预取结果到期后应被释放");
            report("prefetch released after TTL");
        }
    }

    /// 冒烟测试的结果写到 stderr（`--nocapture` 时可见）
    fn report(line: &str) {
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), "[capture_smoke] {line}");
    }
}
