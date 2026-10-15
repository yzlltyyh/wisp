//! 编码：PNG / JPEG 与按协议的体积回退。
//!
//! 截图不透明，因此 alpha 恒为 255 的图像一律按 RGB 编码（少 1/4 的原始数据，体积更小、编码更快）。

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
use image::{ExtendedColorType, ImageEncoder, RgbaImage};
use wisp_types::Protocol;

use crate::crop::{fit_dims, resize_rgba};
use crate::pixel;

/// 编码结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedImage {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
}

/// PNG 的 MIME
pub(crate) const MIME_PNG: &str = "image/png";
/// JPEG 的 MIME
pub(crate) const MIME_JPEG: &str = "image/jpeg";

/// PNG 的 deflate 级别（见 [`encode_png`] 的实测取舍）
const PNG_DEFLATE_LEVEL: u8 = 3;
/// 十进制 MB（各厂商文档的「MB」口径不一，取较小的十进制值更保守）
const MB: usize = 1_000_000;
/// base64 后超过它就改用 JPEG
pub(crate) const JPEG_FALLBACK_B64_BYTES: usize = 5 * MB;
/// JPEG 回退的质量
pub(crate) const JPEG_QUALITY: u8 = 90;
/// JPEG 仍超出协议上限时，长边先降到它
pub(crate) const OVERSIZE_LONG_EDGE: u32 = 1568;
/// 继续缩小时的最小长边（再小就失去识别意义；到这里仍超限只可能是上限配置错误）
const MIN_LONG_EDGE: u32 = 256;

/// 各协议对单张图片的硬限制
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ImageLimits {
    /// base64 编码后的最大字节数
    pub max_b64_bytes: usize,
    /// 长边像素上限
    pub max_long_edge: u32,
    /// 32 px patch 数上限（OpenAI patch 型模型超过即拒绝，不会替你缩放）
    pub max_patches_32: Option<u64>,
}

impl ImageLimits {
    /// 按协议取上限。取值原则：多个厂商共用一个协议时取文档中最严的那个，宁可多压缩，也不让请求被拒。
    pub(crate) fn for_protocol(protocol: Protocol) -> ImageLimits {
        match protocol {
            // OpenAI 官方单请求 512 MB；兼容厂商里通义（base64 原图 ≤ 20 MB）是有据可查的最严值。
            // patch 型模型单图 ≤ 30,000 个 32 px patch（约 5500×5500）
            Protocol::OpenAiChat | Protocol::OpenAiResponses => ImageLimits {
                max_b64_bytes: 20 * MB,
                max_long_edge: u32::MAX,
                max_patches_32: Some(30_000),
            },
            // Anthropic：API 单图上限按 5 MB 计（调研记录为 10 MB，但 API 文档长期写的是 5 MB，取较严者），
            // 像素 ≤ 8000×8000（超出直接拒绝）；整个请求 ≤ 32 MB，单图 5 MB 远低于此
            Protocol::AnthropicMessages => ImageLimits {
                max_b64_bytes: 5 * MB,
                max_long_edge: 8000,
                max_patches_32: None,
            },
            // Gemini：inline 数据整个请求 ≤ 100 MB（旧文档 20 MB，取较严者）；无单图像素硬限
            Protocol::Gemini => ImageLimits {
                max_b64_bytes: 20 * MB,
                max_long_edge: u32::MAX,
                max_patches_32: None,
            },
        }
    }

    /// 满足像素限制所需的目标尺寸；已满足时返回 None
    pub(crate) fn pixel_fit(&self, width: u32, height: u32) -> Option<(u32, u32)> {
        let mut dims = fit_dims(width, height, self.max_long_edge);
        if let Some(max_patches) = self.max_patches_32 {
            let (w, h) = dims.unwrap_or((width, height));
            if patches_32(w, h) > max_patches {
                // 按面积比例缩小，再逐步收紧，直到 patch 数达标（ceil 取整可能让第一次估算略超）
                let mut factor = (max_patches as f64 / patches_32(w, h) as f64).sqrt();
                loop {
                    let long = (f64::from(w.max(h)) * factor).floor().max(1.0) as u32;
                    let next = fit_dims(w, h, long).unwrap_or((w, h));
                    if patches_32(next.0, next.1) <= max_patches || long <= 1 {
                        dims = Some(next);
                        break;
                    }
                    factor *= 0.98;
                }
            }
        }
        dims
    }
}

/// base64（带填充）后的长度
pub(crate) fn base64_len(n: usize) -> usize {
    n.div_ceil(3) * 4
}

fn patches_32(width: u32, height: u32) -> u64 {
    u64::from(width.div_ceil(32)) * u64::from(height.div_ceil(32))
}

/// 编码为 PNG（不透明图像按 RGB 编码；deflate 级别 3）。
///
/// 级别取舍（M 系列 release 实测，3024×1964 Retina 全屏截图）：fast 档 5.7 MB / 70 ms，级别 3 为 1.06 MB / 82 ms，
/// 默认档（级别 6）1.03 MB / 165 ms。级别 3 只比默认档大 3%，耗时减半；fast 档体积是它的 5 倍，上传更慢。
///
/// 宽或高为 0 时无法编码，返回空的 `bytes`（调用方的裁剪不会产生空图，这里只兜底，不 panic）
pub fn encode_png(img: &RgbaImage) -> EncodedImage {
    let (width, height) = img.dimensions();
    let mut bytes = Vec::new();
    let encoder = PngEncoder::new_with_quality(
        &mut bytes,
        CompressionType::Level(PNG_DEFLATE_LEVEL),
        PngFilter::Adaptive,
    );
    let result = if pixel::is_opaque(img.as_raw()) {
        let rgb = pixel::rgba_to_rgb(img.as_raw());
        encoder.write_image(&rgb, width, height, ExtendedColorType::Rgb8)
    } else {
        encoder.write_image(img.as_raw(), width, height, ExtendedColorType::Rgba8)
    };
    if let Err(e) = result {
        log::error!("PNG 编码失败（{width}×{height}）：{e}");
        bytes.clear();
    }
    EncodedImage {
        bytes,
        mime: MIME_PNG,
        width,
        height,
    }
}

/// 编码为 JPEG（丢弃 alpha）
pub(crate) fn encode_jpeg(img: &RgbaImage, quality: u8) -> EncodedImage {
    let (width, height) = img.dimensions();
    let rgb = pixel::rgba_to_rgb(img.as_raw());
    let mut bytes = Vec::new();
    let encoder = JpegEncoder::new_with_quality(&mut bytes, quality);
    if let Err(e) = encoder.write_image(&rgb, width, height, ExtendedColorType::Rgb8) {
        log::error!("JPEG 编码失败（{width}×{height}）：{e}");
        bytes.clear();
    }
    EncodedImage {
        bytes,
        mime: MIME_JPEG,
        width,
        height,
    }
}

/// 送 LLM 用：默认 PNG；base64 后大于 5 MB 改用 JPEG q90；仍超出该协议的单图上限则把长边降到 1568。
///
/// 另外两道防护（正常流程里图片已按长边 2048 缩过，不会触发）：先满足协议的像素硬限制（Anthropic 长边 ≤ 8000、
/// OpenAI patch 数 ≤ 30,000）；长边 1568 仍超限时继续按 3/4 缩小直到达标。返回的宽高是最终编码的尺寸
pub fn encode_for_llm(img: &RgbaImage, protocol: Protocol) -> EncodedImage {
    encode_with_limits(img, ImageLimits::for_protocol(protocol))
}

/// 已经编码好的图（`bytes`，宽 × 高）是否满足这个协议的单图上限（OCR 换模型时按目标协议核对，见 [`reencode_for_llm`]）
pub fn fits_protocol(bytes_len: usize, width: u32, height: u32, protocol: Protocol) -> bool {
    let limits = ImageLimits::for_protocol(protocol);
    base64_len(bytes_len) <= limits.max_b64_bytes && limits.pixel_fit(width, height).is_none()
}

/// 把为别的协议编码过的图（PNG / JPEG 字节）按 `protocol` 的上限重新编码（重新识别 / 从菜单换到 Claude 模型时，
/// 按 OpenAI / Gemini 的 20 MB 编码的 JPEG 可能超过 Anthropic 的 5 MB）。解码失败时返回 None
pub fn reencode_for_llm(bytes: &[u8], protocol: Protocol) -> Option<EncodedImage> {
    let img = image::load_from_memory(bytes).ok()?.into_rgba8();
    Some(encode_for_llm(&img, protocol))
}

/// [`encode_for_llm`] 的本体：按给定上限编码（单测可以用小上限确定性地走完每一级回退）
pub(crate) fn encode_with_limits(img: &RgbaImage, limits: ImageLimits) -> EncodedImage {
    if img.width() == 0 || img.height() == 0 {
        return encode_png(img);
    }
    let fitted = limits
        .pixel_fit(img.width(), img.height())
        .map(|(w, h)| resize_rgba(img, w, h));
    let src = fitted.as_ref().unwrap_or(img);

    let png = encode_png(src);
    if !png.bytes.is_empty() && base64_len(png.bytes.len()) <= JPEG_FALLBACK_B64_BYTES.min(limits.max_b64_bytes) {
        return png;
    }
    drop(png);

    let jpeg = encode_jpeg(src, JPEG_QUALITY);
    if !jpeg.bytes.is_empty() && base64_len(jpeg.bytes.len()) <= limits.max_b64_bytes {
        return jpeg;
    }

    let long = src.width().max(src.height());
    let mut edge = if long > OVERSIZE_LONG_EDGE {
        OVERSIZE_LONG_EDGE
    } else {
        long * 3 / 4
    };
    let mut last = jpeg;
    while edge >= MIN_LONG_EDGE {
        let Some((w, h)) = fit_dims(src.width(), src.height(), edge) else {
            break;
        };
        let smaller = encode_jpeg(&resize_rgba(src, w, h), JPEG_QUALITY);
        let fits = base64_len(smaller.bytes.len()) <= limits.max_b64_bytes;
        last = smaller;
        if fits {
            break;
        }
        edge = edge * 3 / 4;
    }
    if base64_len(last.bytes.len()) > limits.max_b64_bytes {
        log::warn!(
            "图片缩到 {}×{} 仍超出单图上限（base64 {} > {} 字节），按此发送",
            last.width,
            last.height,
            base64_len(last.bytes.len()),
            limits.max_b64_bytes
        );
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reencoding_follows_the_target_protocol_limits() {
        // 9000 px 长边：OpenAI 的像素限制允许（patch 数够），Anthropic 要求长边 ≤ 8000
        assert!(fits_protocol(1000, 9000, 100, Protocol::OpenAiChat));
        assert!(!fits_protocol(1000, 9000, 100, Protocol::AnthropicMessages));
        assert!(!fits_protocol(6 * MB, 100, 100, Protocol::AnthropicMessages));
        assert!(fits_protocol(6 * MB, 100, 100, Protocol::Gemini));
        let img = RgbaImage::from_pixel(9000, 4, image::Rgba([10, 20, 30, 255]));
        let wide = encode_png(&img);
        let again = reencode_for_llm(&wide.bytes, Protocol::AnthropicMessages).expect("decode");
        assert!(again.width <= 8000, "{}", again.width);
        assert!(fits_protocol(
            again.bytes.len(),
            again.width,
            again.height,
            Protocol::AnthropicMessages
        ));
        assert!(reencode_for_llm(b"not an image", Protocol::Gemini).is_none());
    }

    #[test]
    fn base64_len_matches_padding_rules() {
        assert_eq!(base64_len(0), 0);
        assert_eq!(base64_len(1), 4);
        assert_eq!(base64_len(3), 4);
        assert_eq!(base64_len(4), 8);
        assert_eq!(base64_len(3_750_000), 5_000_000);
    }

    #[test]
    fn pixel_limits_per_protocol() {
        let anthropic = ImageLimits::for_protocol(Protocol::AnthropicMessages);
        assert_eq!(anthropic.pixel_fit(2048, 1536), None);
        assert_eq!(anthropic.pixel_fit(16000, 4000), Some((8000, 2000)));

        let openai = ImageLimits::for_protocol(Protocol::OpenAiChat);
        assert_eq!(openai.pixel_fit(2048, 1536), None);
        let (w, h) = openai.pixel_fit(8000, 8000).expect("超过 30,000 patch 必须缩小");
        assert!(patches_32(w, h) <= 30_000, "{w}×{h}");
        assert!(w >= 5000, "只缩到刚好达标，不过度缩小：{w}");

        let gemini = ImageLimits::for_protocol(Protocol::Gemini);
        assert_eq!(gemini.pixel_fit(10000, 10000), None);
    }

    /// 确定性的伪随机噪声图（不可压缩）
    fn noise(width: u32, height: u32, seed: u64) -> RgbaImage {
        let mut state = seed;
        let px = (0..width * height)
            .flat_map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let v = (state >> 33) as u32;
                [v as u8, (v >> 8) as u8, (v >> 16) as u8, 255]
            })
            .collect();
        RgbaImage::from_raw(width, height, px).expect("尺寸一致")
    }

    fn unlimited_pixels(max_b64_bytes: usize) -> ImageLimits {
        ImageLimits {
            max_b64_bytes,
            max_long_edge: u32::MAX,
            max_patches_32: None,
        }
    }

    /// 长边缩到 `edge` 后 JPEG q90 的 base64 长度
    fn jpeg_b64_at(img: &RgbaImage, edge: u32) -> usize {
        let (w, h) = fit_dims(img.width(), img.height(), edge).unwrap_or(img.dimensions());
        base64_len(encode_jpeg(&resize_rgba(img, w, h), JPEG_QUALITY).bytes.len())
    }

    #[test]
    fn shrinks_by_three_quarters_until_it_fits() {
        // 2000×1500：1568 → 1176 → 882 → 661。把上限放在 882 与 661 两级之间，必须走满四级才达标，且停在第一个达标的那一级
        let img = noise(2000, 1500, 3);
        let (at_882, at_661) = (jpeg_b64_at(&img, 882), jpeg_b64_at(&img, 661));
        assert!(at_661 < at_882, "{at_661} / {at_882}");
        let limits = unlimited_pixels((at_882 + at_661) / 2);
        assert!(jpeg_b64_at(&img, 2000) > limits.max_b64_bytes, "前提：全尺寸 JPEG 超限");

        let enc = encode_with_limits(&img, limits);
        assert_eq!(enc.mime, MIME_JPEG);
        assert_eq!((enc.width, enc.height), (661, 496));
        assert!(base64_len(enc.bytes.len()) <= limits.max_b64_bytes);
        assert_eq!(base64_len(enc.bytes.len()), at_661, "结果就是该级的 JPEG");
    }

    #[test]
    fn short_images_start_shrinking_from_three_quarters() {
        // 长边 1200 ≤ 1568：不先跳到 1568，而是直接从 900 开始逐级 ×3/4
        let img = noise(1200, 800, 5);
        let at_900 = jpeg_b64_at(&img, 900);
        let limits = unlimited_pixels(at_900);
        assert!(jpeg_b64_at(&img, 1200) > limits.max_b64_bytes, "前提：原尺寸 JPEG 超限");
        let enc = encode_with_limits(&img, limits);
        assert_eq!((enc.width, enc.height), (900, 600));
    }

    #[test]
    fn stops_at_the_minimum_edge_when_nothing_fits() {
        // 上限小到任何尺寸都放不下：1568 → … → 278（再缩就低于 256），返回最小的那一级，不 panic、不死循环
        let img = noise(1600, 1200, 9);
        let enc = encode_with_limits(&img, unlimited_pixels(1));
        assert_eq!(enc.mime, MIME_JPEG);
        assert_eq!(enc.width.max(enc.height), 278);
        assert!(!enc.bytes.is_empty());
    }

    #[test]
    fn zero_sized_image_does_not_panic() {
        let empty = RgbaImage::new(0, 0);
        let png = encode_png(&empty);
        assert!(png.bytes.is_empty());
        assert_eq!(png.mime, MIME_PNG);
    }
}
