//! 捕获帧与窗口列表。

use std::sync::{Arc, OnceLock};

use wisp_types::{MonitorId, MonitorInfo, PhysRect, Rect};

use crate::error::CaptureError;

/// 一个显示器的捕获帧（物理像素）
pub struct Frame {
    pub monitor: MonitorInfo,
    pub width: u32,
    pub height: u32,
    /// RGBA8 像素（macOS 上惰性从 CGImage 转出）
    rgba: OnceLock<Pixels>,
    /// 长边减半的 RGBA（惰性生成）
    half: OnceLock<(u32, u32, Arc<Vec<u8>>)>,
    /// macOS：SCK 返回的 CGImage；`from_rgba` 构造的假帧为 None
    #[cfg(target_os = "macos")]
    cg: Option<objc2_core_foundation::CFRetained<objc2_core_graphics::CGImage>>,
}

impl Frame {
    /// 由现成的 RGBA8 像素构造一帧（测试、编排器状态机的 fake 捕获、无屏幕录制权限的冒烟模式用）。
    /// `rgba.len()` 必须等于 `width * height * 4`，否则 panic（只用于测试 / 假数据，调用方保证）。
    /// 这种帧在 macOS 上没有 CGImage（`cg_image()` 为 None），Overlay 退回由 WebView 画冻结帧（`frozen_native = false`）
    pub fn from_rgba(monitor: MonitorInfo, width: u32, height: u32, rgba: Vec<u8>) -> Frame {
        let expected = (width as usize) * (height as usize) * 4;
        assert_eq!(rgba.len(), expected, "Frame::from_rgba：像素长度与尺寸不符");
        let cell = OnceLock::new();
        let _ = cell.set(Pixels {
            data: Arc::new(rgba),
            valid: true,
        });
        Frame {
            monitor,
            width,
            height,
            rgba: cell,
            half: OnceLock::new(),
            #[cfg(target_os = "macos")]
            cg: None,
        }
    }

    /// macOS：由 SCK 返回的 CGImage 构造（宽高取图像本身的像素尺寸）；RGBA 在首次调用 `rgba()` 时才转出
    #[cfg(target_os = "macos")]
    pub(crate) fn from_cg_image(
        monitor: MonitorInfo,
        cg: objc2_core_foundation::CFRetained<objc2_core_graphics::CGImage>,
    ) -> Frame {
        use objc2_core_graphics::CGImage;
        let width = u32::try_from(CGImage::width(Some(&cg))).unwrap_or(0);
        let height = u32::try_from(CGImage::height(Some(&cg))).unwrap_or(0);
        Frame {
            monitor,
            width,
            height,
            rgba: OnceLock::new(),
            half: OnceLock::new(),
            cg: Some(cg),
        }
    }

    /// RGBA8 像素（BGRA→RGBA 在捕获线程一次完成；macOS 上在首次调用时从 CGImage 转出并缓存，不在 Overlay 首帧的关键路径上）。
    ///
    /// macOS 上转换失败（极端情况下的内存不足等）时返回同尺寸的不透明黑帧，保证长度与 `width × height × 4` 一致；
    /// 失败会被记住，[`crate::crop`] 对这样的帧返回错误，不会把黑图交给 OCR / LLM
    pub fn rgba(&self) -> Arc<Vec<u8>> {
        self.pixels().data.clone()
    }

    /// 长边减半的 RGBA（Windows / X11 的 Overlay 首帧先送它，再补全分辨率；首次调用时生成并缓存）
    pub fn rgba_half(&self) -> (u32, u32, Arc<Vec<u8>>) {
        self.half
            .get_or_init(|| {
                let full = self.rgba();
                let (w, h, px) = crate::pixel::downsample_half(&full, self.width, self.height);
                (w, h, Arc::new(px))
            })
            .clone()
    }

    /// 是否带原生冻结帧（三端都有）：macOS 上 SCK 帧带 CGImage 时为 true；`from_rgba`
    /// 构造的假帧、Windows / X11 的帧为 false（Overlay 由 WebView 画冻结帧，`OverlayInit.frozen_native = false`）
    pub fn has_native_image(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.cg.is_some()
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }

    /// macOS：SCK 返回的 CGImage，交给 windows/native/macos.rs 作为 Overlay 底层 CALayer 的 contents（零拷贝）。
    /// `from_rgba` 构造的假帧没有 CGImage，返回 None（此时 `OverlayInit.frozen_native` 必须为 false）
    #[cfg(target_os = "macos")]
    pub fn cg_image(&self) -> Option<objc2_core_foundation::CFRetained<objc2_core_graphics::CGImage>> {
        self.cg.clone()
    }

    /// 一块物理像素区域的 RGBA8（紧密排列；`region` 必须在帧内，由 `to_physical` 夹紧保证）。
    ///
    /// 整屏 RGBA 已就绪时逐行拷贝；macOS 上尚未转出整屏时只转换这一块（CGImage 子图，结果与整屏转换后裁剪逐像素相同），
    /// 省去整屏转换的时间与内存。像素转换失败时返回 `Backend` 错误
    pub(crate) fn region_rgba(&self, region: PhysRect) -> Result<Vec<u8>, CaptureError> {
        #[cfg(target_os = "macos")]
        if self.rgba.get().is_none()
            && let Some(cg) = &self.cg
        {
            return crate::backend_macos::cg_region_to_rgba(cg, region).ok_or_else(|| {
                log::error!(
                    "CGImage 区域 → RGBA 转换失败（显示器 {}，区域 {:?}）",
                    self.monitor.id,
                    region
                );
                CaptureError::Backend("failed to convert the captured image to RGBA".to_owned())
            });
        }
        let pixels = self.pixels();
        if !pixels.valid {
            return Err(CaptureError::Backend(
                "failed to convert the captured image to RGBA".to_owned(),
            ));
        }
        copy_region(&pixels.data, self.width, region)
            .ok_or_else(|| CaptureError::Backend("frame pixel buffer is shorter than its size".to_owned()))
    }

    /// 按需取一块物理像素区域：Overlay 放大镜的小块与飞入用的选区预览。`region` 必须在帧内、
    /// 宽高非零，否则返回 None；面积超过 `max_px` 时等比缩小（宽高各至少 1）。返回 `(宽, 高, 紧密排列的 RGBA8)`。
    ///
    /// macOS 上整屏 RGBA 还没转出时只转换（并缩放）这一块——原生冻结帧的 Overlay 不再为放大镜拉取整屏帧，主进程里
    /// 不会再出现整屏 RGBA 与交给 `wisp://` 响应的拷贝这两块整屏缓冲区（macOS 26 的 malloc 要 2–3 分钟才归还释放的大块）。
    /// 像素转换失败时同样返回 None（[`Self::region_rgba`] 的错误已记日志）
    pub fn region_preview(&self, region: PhysRect, max_px: u64) -> Option<(u32, u32, Vec<u8>)> {
        let fits = |start: u32, len: u32, max: u32| start.checked_add(len).is_some_and(|end| end <= max);
        if region.width == 0
            || region.height == 0
            || !fits(region.x, region.width, self.width)
            || !fits(region.y, region.height, self.height)
        {
            return None;
        }
        let (w, h) = preview_dims(region.width, region.height, max_px);
        if (w, h) == (region.width, region.height) {
            return self.region_rgba(region).ok().map(|px| (w, h, px));
        }
        #[cfg(target_os = "macos")]
        if self.rgba.get().is_none()
            && let Some(cg) = &self.cg
        {
            return crate::backend_macos::cg_region_scaled_to_rgba(cg, region, w, h).map(|px| (w, h, px));
        }
        let full = image::RgbaImage::from_raw(region.width, region.height, self.region_rgba(region).ok()?)?;
        Some((w, h, crate::crop::resize_rgba(&full, w, h).into_raw()))
    }

    fn pixels(&self) -> &Pixels {
        self.rgba.get_or_init(|| self.materialize_rgba())
    }

    /// 生成 RGBA 缓冲：macOS 从 CGImage 转出（转换到 sRGB）；其他情况在构造时就已就绪，走不到这里。
    /// 转换失败时退回同尺寸的不透明黑帧并标记为无效
    fn materialize_rgba(&self) -> Pixels {
        #[cfg(target_os = "macos")]
        if let Some(cg) = &self.cg {
            match crate::backend_macos::cg_image_to_rgba(cg, self.width, self.height) {
                Some(px) => {
                    return Pixels {
                        data: Arc::new(px),
                        valid: true,
                    };
                }
                None => log::error!(
                    "CGImage → RGBA 转换失败（显示器 {}，{}×{}），冻结帧退回黑帧，裁剪将报错",
                    self.monitor.id,
                    self.width,
                    self.height
                ),
            }
        }
        Pixels {
            data: Arc::new(crate::pixel::opaque_black(self.width, self.height)),
            valid: false,
        }
    }
}

/// 帧的 RGBA 像素与它是否真实（转换失败时为黑帧占位）
struct Pixels {
    data: Arc<Vec<u8>>,
    valid: bool,
}

/// [`Frame::region_preview`] 的输出尺寸：面积不超过 `max_px` 时原样；否则按 √(max_px / 面积) 等比缩小（向下取整，宽高各至少 1）
fn preview_dims(width: u32, height: u32, max_px: u64) -> (u32, u32) {
    let area = u64::from(width) * u64::from(height);
    if max_px == 0 || area <= max_px {
        return (width, height);
    }
    let k = (max_px as f64 / area as f64).sqrt();
    let scale = |v: u32| ((f64::from(v) * k).floor() as u32).clamp(1, v);
    let (w, h) = (scale(width), scale(height));
    // 极端的长条（一边按比例缩到 1 以下又夹回 1）：另一边再按面积上限收一次
    let cap = |other: u32, v: u32| u32::try_from(max_px / u64::from(other)).unwrap_or(v).clamp(1, v);
    (cap(h, w), cap(w, h))
}

/// 从宽 `width` 的紧密 RGBA 缓冲中逐行拷出 `region`；缓冲不够长时返回 None
fn copy_region(rgba: &[u8], width: u32, region: PhysRect) -> Option<Vec<u8>> {
    let src_stride = width as usize * 4;
    let row_bytes = region.width as usize * 4;
    let mut out = Vec::with_capacity(row_bytes * region.height as usize);
    for y in region.y..region.y + region.height {
        let start = y as usize * src_stride + region.x as usize * 4;
        out.extend_from_slice(rgba.get(start..start + row_bytes)?);
    }
    Some(out)
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("monitor", &self.monitor.id)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

/// 窗口矩形：显示器内逻辑坐标，按 z 序排列，最上层在前
#[derive(Debug, Clone, PartialEq)]
pub struct WindowRect {
    pub monitor_id: MonitorId,
    pub rect: Rect,
    pub app_name: String,
}

/// 一次捕获的结果
#[derive(Debug)]
pub struct CaptureSet {
    pub frames: Vec<Frame>,
    pub windows: Vec<WindowRect>,
}

#[cfg(test)]
mod tests {
    use wisp_types::{PhysPoint, PhysSize, Rect};

    use super::*;

    #[test]
    fn from_rgba_keeps_pixels_without_copy() {
        let monitor = MonitorInfo {
            id: "1".to_owned(),
            name: "fake".to_owned(),
            physical_origin: PhysPoint { x: 0, y: 0 },
            physical_size: PhysSize { width: 2, height: 1 },
            scale_factor: 1.0,
            frame: Rect {
                x: 0.0,
                y: 0.0,
                width: 2.0,
                height: 1.0,
            },
            work_area: Rect {
                x: 0.0,
                y: 0.0,
                width: 2.0,
                height: 1.0,
            },
            is_primary: true,
        };
        let frame = Frame::from_rgba(monitor, 2, 1, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        let a = frame.rgba();
        let b = frame.rgba();
        assert!(Arc::ptr_eq(&a, &b), "多次取像素共享同一块内存");
        assert_eq!(a.as_slice(), &[1, 2, 3, 4, 5, 6, 7, 8]);
        #[cfg(target_os = "macos")]
        assert!(frame.cg_image().is_none());
    }

    /// macOS：CGImage → RGBA 转换失败时，冻结帧退回黑帧，但裁剪报错（不把黑图交给 OCR / LLM）
    #[cfg(target_os = "macos")]
    #[test]
    fn failed_conversion_makes_crop_fail_instead_of_returning_black() {
        let monitor = MonitorInfo {
            id: "1".to_owned(),
            name: "fake".to_owned(),
            physical_origin: PhysPoint { x: 0, y: 0 },
            physical_size: PhysSize { width: 3, height: 2 },
            scale_factor: 1.0,
            frame: Rect::new(0.0, 0.0, 3.0, 2.0),
            work_area: Rect::new(0.0, 0.0, 3.0, 2.0),
            is_primary: true,
        };
        // 帧声称 3×2，图像实际 2×2：整屏与越界区域的转换都会失败
        let broken = || {
            let mut bgra = vec![200u8; 2 * 2 * 4];
            Frame {
                monitor: monitor.clone(),
                width: 3,
                height: 2,
                rgba: OnceLock::new(),
                half: OnceLock::new(),
                cg: Some(crate::backend_macos::bgra_image(2, 2, &mut bgra)),
            }
        };

        // 尚未转出整屏：只转换选区，失败即报错
        let frame = broken();
        let err = crate::crop(&frame, Rect::new(2.0, 0.0, 1.0, 1.0)).expect_err("区域转换失败应报错");
        assert!(matches!(err, CaptureError::Backend(_)), "{err:?}");

        // 已经转出整屏（失败、退回黑帧）：rgba() 仍给出合法长度的不透明黑帧，裁剪报错
        let frame = broken();
        let rgba = frame.rgba();
        assert_eq!(rgba.len(), 3 * 2 * 4);
        assert!(rgba.chunks_exact(4).all(|p| p == [0, 0, 0, 255]));
        let err = crate::crop(&frame, Rect::new(0.0, 0.0, 1.0, 1.0)).expect_err("黑帧占位不能被裁剪");
        assert!(matches!(err, CaptureError::Backend(_)), "{err:?}");
    }

    #[test]
    fn region_preview_is_exact_inside_and_scaled_when_large() {
        let monitor = MonitorInfo {
            id: "1".to_owned(),
            name: "fake".to_owned(),
            physical_origin: PhysPoint { x: 0, y: 0 },
            physical_size: PhysSize { width: 4, height: 2 },
            scale_factor: 1.0,
            frame: Rect::new(0.0, 0.0, 4.0, 2.0),
            work_area: Rect::new(0.0, 0.0, 4.0, 2.0),
            is_primary: true,
        };
        let px: Vec<u8> = (0..4 * 2).flat_map(|i| [i as u8 * 10, 0, 0, 255]).collect();
        let frame = Frame::from_rgba(monitor, 4, 2, px);
        let r = |x, y, width, height| PhysRect { x, y, width, height };
        // 帧内的小块：逐像素（第二行的第 2、3 个像素）
        let (w, h, part) = frame.region_preview(r(1, 1, 2, 1), 100).expect("小块");
        assert_eq!((w, h), (2, 1));
        assert_eq!(part, vec![50, 0, 0, 255, 60, 0, 0, 255]);
        // 越界、空区域：拒绝
        assert!(frame.region_preview(r(3, 0, 2, 1), 100).is_none());
        assert!(frame.region_preview(r(0, 2, 1, 1), 100).is_none());
        assert!(frame.region_preview(r(0, 0, 0, 1), 100).is_none());
        assert!(
            frame.region_preview(r(u32::MAX, 0, 2, 1), 100).is_none(),
            "溢出不 panic"
        );
        // 面积超过上限：等比缩小，长度与尺寸一致
        let (w, h, small) = frame.region_preview(r(0, 0, 4, 2), 2).expect("缩小");
        assert_eq!((w, h), (2, 1));
        assert_eq!(small.len(), 2 * 4);
        let (w, h) = preview_dims(3840, 2160, 4_000_000);
        assert!(
            u64::from(w) * u64::from(h) <= 4_000_000 && w >= 2660 && h >= 1495,
            "{w}×{h}"
        );
        assert_eq!(preview_dims(10_000, 1, 100), (100, 1));
        assert_eq!(preview_dims(256, 256, 4_000_000), (256, 256));
    }

    #[test]
    fn frame_and_capture_set_are_send_sync() {
        // 编排器把 CaptureSet 交给异步任务、资源协议在任意线程读取像素
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Frame>();
        assert_send_sync::<CaptureSet>();
        assert_send_sync::<WindowRect>();
    }

    #[test]
    fn rgba_half_is_cached_and_halves_both_edges() {
        let monitor = MonitorInfo {
            id: "1".to_owned(),
            name: "fake".to_owned(),
            physical_origin: PhysPoint { x: -3, y: 0 },
            physical_size: PhysSize { width: 4, height: 2 },
            scale_factor: 2.0,
            frame: Rect::new(0.0, 0.0, 2.0, 1.0),
            work_area: Rect::new(0.0, 0.0, 2.0, 1.0),
            is_primary: false,
        };
        let px: Vec<u8> = (0..4 * 2).flat_map(|i| [i as u8 * 10, 0, 0, 255]).collect();
        let frame = Frame::from_rgba(monitor, 4, 2, px);
        let (w, h, a) = frame.rgba_half();
        let (_, _, b) = frame.rgba_half();
        assert_eq!((w, h), (2, 1));
        assert!(Arc::ptr_eq(&a, &b), "降采样结果缓存复用");
        // 左上 2×2 块：像素 0、1、4、5 → (0+10+40+50)/4 = 25
        assert_eq!(&a[..4], &[25, 0, 0, 255]);
    }
}
