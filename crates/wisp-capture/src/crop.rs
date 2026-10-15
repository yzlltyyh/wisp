//! 逻辑选区 → 物理像素换算（误差 ≤ 1 物理像素）、裁剪与按长边缩放。
//!
//! 坐标约定：选区是「显示器内的逻辑坐标」（Overlay 页面里的 CSS px，原点是该显示器左上角），
//! 帧是该显示器的物理像素图像。每个显示器有自己的 scale，一次只涉及一个显示器，因此显示器在虚拟桌面里的
//! 原点（可以为负）不参与换算——混合缩放、负原点下都没有累积偏移。

use image::RgbaImage;
use image::imageops::{self, FilterType};
use wisp_types::{PhysRect, Rect};

use crate::error::CaptureError;
use crate::frame::Frame;
use crate::pixel;

/// 帧的实际像素宽与 `scale_factor × 逻辑宽` 的相对偏差超过它时，改用实际比例换算（见 [`effective_scale`]）
const SCALE_MISMATCH_TOLERANCE: f64 = 0.01;

/// 按下面的公式换算并夹紧到 `bounds`（物理宽, 物理高）：
/// `px = round(x·s)`，`py = round(y·s)`，`pw = round((x+w)·s) − px`，`ph = round((y+h)·s) − py`。
///
/// 左右（上下）两条边各自独立取整，因此任何一条边的误差都不超过 0.5 物理像素，相邻选区之间也不会出现缝隙或重叠。
/// 负宽高按反向选区处理；越界部分被裁掉（结果是选区与图像的交集，可能为空）；`scale` 非有限或 ≤ 0 时按 1 处理；
/// NaN 坐标按 0 处理，±∞ 夹紧到边界。
pub fn to_physical(local: Rect, scale: f64, bounds: (u32, u32)) -> PhysRect {
    let s = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
    let (x0, x1) = ordered(local.x, local.x + local.width);
    let (y0, y1) = ordered(local.y, local.y + local.height);
    let left = round_clamped(x0 * s, bounds.0);
    let right = round_clamped(x1 * s, bounds.0);
    let top = round_clamped(y0 * s, bounds.1);
    let bottom = round_clamped(y1 * s, bounds.1);
    PhysRect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    }
}

/// 从帧中裁出逻辑选区（逐行拷贝，不做重采样，像素与冻结帧逐一对应）。
///
/// 选区与图像的交集为空时返回 `Backend` 错误（Overlay 的提交校验会先挡住空选区，这里只兜底）；
/// 帧的像素转换失败（macOS 的 CGImage → RGBA）时也返回 `Backend` 错误，而不是交出一张黑图。
/// macOS 上整屏 RGBA 还没转出时只转换选区这一块
pub fn crop(frame: &Frame, local: Rect) -> Result<RgbaImage, CaptureError> {
    let scale = effective_scale(frame);
    let r = to_physical(local, scale, (frame.width, frame.height));
    if r.width == 0 || r.height == 0 {
        return Err(CaptureError::Backend(format!(
            "selection is empty after clamping to the {}×{} frame",
            frame.width, frame.height
        )));
    }
    let out = frame.region_rgba(r)?;
    RgbaImage::from_raw(r.width, r.height, out)
        .ok_or_else(|| CaptureError::Backend("cropped buffer size mismatch".to_owned()))
}

/// 长边超过 `max_edge` 时等比缩小（长边恰好等于 `max_edge`，短边四舍五入且至少 1 px）；否则原样返回、不拷贝。
/// `max_edge == 0` 视为不限制
pub fn fit_long_edge(img: RgbaImage, max_edge: u32) -> RgbaImage {
    match fit_dims(img.width(), img.height(), max_edge) {
        Some((w, h)) => resize_rgba(&img, w, h),
        None => img,
    }
}

/// 长边上限为 `max_edge` 时的目标尺寸；不需要缩小时返回 None
pub(crate) fn fit_dims(width: u32, height: u32, max_edge: u32) -> Option<(u32, u32)> {
    let long = width.max(height);
    if max_edge == 0 || long <= max_edge || width == 0 || height == 0 {
        return None;
    }
    let short = u64::from(width.min(height));
    let (long, max) = (u64::from(long), u64::from(max_edge));
    // 整数运算避免浮点误差：short × max / long，四舍五入；结果 ≤ max_edge，必然放得进 u32
    let scaled = u32::try_from(((short * max + long / 2) / long).max(1)).unwrap_or(max_edge);
    Some(if width >= height {
        (max_edge, scaled)
    } else {
        (scaled, max_edge)
    })
}

/// 高质量缩小：缩小倍数超过 2 时先用 2×2 盒式平均逐级减半（快且抗混叠），
/// 最后一步用 Catmull-Rom（锐利，适合文字）精确缩放到目标尺寸。放大（不应发生）直接用 Catmull-Rom
pub(crate) fn resize_rgba(src: &RgbaImage, width: u32, height: u32) -> RgbaImage {
    let width = width.max(1);
    let height = height.max(1);
    if src.width() == width && src.height() == height {
        return src.clone();
    }
    let (mut cur_w, mut cur_h) = src.dimensions();
    let mut reduced: Option<Vec<u8>> = None;
    while cur_w / 2 >= width && cur_h / 2 >= height {
        let buf = reduced.as_deref().unwrap_or(src.as_raw());
        let (w, h, half) = pixel::downsample_half(buf, cur_w, cur_h);
        cur_w = w;
        cur_h = h;
        reduced = Some(half);
    }
    let Some(img) = reduced.and_then(|buf| RgbaImage::from_raw(cur_w, cur_h, buf)) else {
        return imageops::resize(src, width, height, FilterType::CatmullRom);
    };
    if img.dimensions() == (width, height) {
        img
    } else {
        imageops::resize(&img, width, height, FilterType::CatmullRom)
    }
}

/// 帧的换算比例：通常就是显示器的 `scale_factor`。
///
/// 防护：如果帧的实际像素宽与 `scale_factor × 逻辑宽` 不符（后端给出的分辨率与平台层的缩放比不一致，
/// 例如显示器配置在捕获前一刻改变），逻辑选区实际覆盖的仍是整张冻结帧，此时改用「像素宽 / 逻辑宽」，保证选区与冻结帧对齐
pub(crate) fn effective_scale(frame: &Frame) -> f64 {
    let s = frame.monitor.scale_factor;
    let logical_w = frame.monitor.frame.width;
    let valid_s = s.is_finite() && s > 0.0;
    if !(logical_w.is_finite() && logical_w > 0.0) || frame.width == 0 {
        return if valid_s { s } else { 1.0 };
    }
    let ratio = f64::from(frame.width) / logical_w;
    if valid_s && ((ratio - s).abs() / s) <= SCALE_MISMATCH_TOLERANCE {
        s
    } else {
        log::warn!(
            "显示器 {} 的帧宽 {} px 与 scale_factor {} × 逻辑宽 {} 不符，按实际比例 {:.4} 换算",
            frame.monitor.id,
            frame.width,
            s,
            logical_w,
            ratio
        );
        ratio
    }
}

fn ordered(a: f64, b: f64) -> (f64, f64) {
    if a <= b { (a, b) } else { (b, a) }
}

/// 四舍五入（.5 远离 0）后夹紧到 `[0, max]`；NaN → 0
fn round_clamped(v: f64, max: u32) -> u32 {
    if v.is_nan() {
        return 0;
    }
    let r = v.round();
    if r <= 0.0 {
        0
    } else if r >= f64::from(max) {
        max
    } else {
        // 已确认 0 < r < max ≤ u32::MAX，转换无截断
        r as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_dims_matches_spec_examples() {
        assert_eq!(fit_dims(4000, 3000, 2048), Some((2048, 1536)));
        assert_eq!(fit_dims(3000, 4000, 2048), Some((1536, 2048)));
        assert_eq!(fit_dims(2048, 100, 2048), None);
        assert_eq!(fit_dims(100, 100, 0), None);
        assert_eq!(fit_dims(10000, 1, 2048), Some((2048, 1)));
    }

    #[test]
    fn round_clamped_handles_non_finite() {
        assert_eq!(round_clamped(f64::NAN, 10), 0);
        assert_eq!(round_clamped(f64::INFINITY, 10), 10);
        assert_eq!(round_clamped(f64::NEG_INFINITY, 10), 0);
        assert_eq!(round_clamped(2.5, 10), 3);
        assert_eq!(round_clamped(2.49, 10), 2);
    }

    #[test]
    fn resize_uses_box_prefilter_for_large_ratios() {
        // 1000×10 → 100×1：多级减半后再精确缩放，结果尺寸准确，纯色保持不变
        let src = RgbaImage::from_pixel(1000, 10, image::Rgba([10, 20, 30, 255]));
        let out = resize_rgba(&src, 100, 1);
        assert_eq!(out.dimensions(), (100, 1));
        assert!(out.pixels().all(|p| p.0 == [10, 20, 30, 255]));
        // 恰好 1/2：只走盒式平均
        let exact = resize_rgba(&RgbaImage::from_pixel(8, 8, image::Rgba([1, 2, 3, 255])), 4, 4);
        assert_eq!(exact.dimensions(), (4, 4));
        assert!(exact.pixels().all(|p| p.0 == [1, 2, 3, 255]));
    }
}
