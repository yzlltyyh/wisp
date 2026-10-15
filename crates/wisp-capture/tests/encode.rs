//! 编码测试：PNG / JPEG 编码、长边缩放与按协议的体积回退。

use image::{Rgba, RgbaImage};
use wisp_capture::{Frame, encode_for_llm, encode_png, fit_long_edge};
use wisp_types::{MonitorInfo, PhysPoint, PhysSize, Protocol, Rect};

const MB: usize = 1_000_000;

fn b64_len(n: usize) -> usize {
    n.div_ceil(3) * 4
}

/// 确定性的伪随机噪声图（不可压缩，用来制造超大 PNG）
fn noise(width: u32, height: u32, seed: u64) -> RgbaImage {
    let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    let mut px = Vec::with_capacity((width * height * 4) as usize);
    for _ in 0..width * height {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let v = (state >> 33) as u32;
        px.extend_from_slice(&[v as u8, (v >> 8) as u8, (v >> 16) as u8, 255]);
    }
    RgbaImage::from_raw(width, height, px).expect("尺寸一致")
}

/// 类截图内容：浅色底、深色「文字行」
fn screenshot_like(width: u32, height: u32) -> RgbaImage {
    RgbaImage::from_fn(width, height, |x, y| {
        let line = (y / 18) % 3 != 2 && (y % 18) > 4 && (y % 18) < 14;
        let glyph = (x / 7 + y / 18) % 5 != 0 && (x % 7) < 5;
        if line && glyph {
            Rgba([30, 30, 36, 255])
        } else {
            Rgba([250, 250, 252, 255])
        }
    })
}

#[test]
fn png_round_trips_opaque_as_rgb_and_keeps_alpha_otherwise() {
    let img = screenshot_like(64, 40);
    let enc = encode_png(&img);
    assert_eq!(enc.mime, "image/png");
    assert_eq!((enc.width, enc.height), (64, 40));
    let decoded = image::load_from_memory(&enc.bytes).expect("合法 PNG");
    assert_eq!(decoded.color(), image::ColorType::Rgb8, "不透明图按 RGB 编码");
    assert_eq!(decoded.to_rgba8(), img, "无损");

    let mut translucent = img.clone();
    translucent.put_pixel(3, 3, Rgba([1, 2, 3, 100]));
    let enc = encode_png(&translucent);
    let decoded = image::load_from_memory(&enc.bytes).expect("合法 PNG");
    assert_eq!(decoded.color(), image::ColorType::Rgba8);
    assert_eq!(decoded.to_rgba8(), translucent);
}

#[test]
fn fit_long_edge_4000x3000_to_2048x1536() {
    let img = RgbaImage::from_pixel(4000, 3000, Rgba([120, 130, 140, 255]));
    let out = fit_long_edge(img, 2048);
    assert_eq!(out.dimensions(), (2048, 1536));
    assert!(out.pixels().all(|p| p.0 == [120, 130, 140, 255]), "纯色缩放后不变");

    // 竖图、恰好等于上限、小图都不放大
    assert_eq!(
        fit_long_edge(RgbaImage::new(3000, 4000), 2048).dimensions(),
        (1536, 2048)
    );
    assert_eq!(fit_long_edge(RgbaImage::new(2048, 10), 2048).dimensions(), (2048, 10));
    let small = screenshot_like(300, 200);
    assert_eq!(fit_long_edge(small.clone(), 2048), small, "未超限时原样返回");
}

#[test]
fn downscale_keeps_text_contrast() {
    // 高倍缩小（盒式预缩 + Catmull-Rom）不应把文字糊成一片灰：仍有接近原深色与原浅色的像素
    let img = screenshot_like(4000, 2400);
    let out = fit_long_edge(img, 1000);
    assert_eq!(out.dimensions(), (1000, 600));
    let darkest = out.pixels().map(|p| p.0[0]).min().unwrap_or(255);
    let lightest = out.pixels().map(|p| p.0[0]).max().unwrap_or(0);
    assert!(darkest < 120, "darkest {darkest}");
    assert!(lightest > 230, "lightest {lightest}");
}

#[test]
fn llm_small_screenshot_stays_png() {
    let img = screenshot_like(1200, 800);
    for protocol in [
        Protocol::OpenAiChat,
        Protocol::OpenAiResponses,
        Protocol::AnthropicMessages,
        Protocol::Gemini,
    ] {
        let enc = encode_for_llm(&img, protocol);
        assert_eq!(enc.mime, "image/png", "{protocol:?}");
        assert_eq!((enc.width, enc.height), (1200, 800));
        assert_eq!(enc.bytes, encode_png(&img).bytes);
    }
}

#[test]
fn llm_oversized_png_falls_back_to_jpeg_q90() {
    // 1400×1400 噪声：PNG ≈ 5.9 MB（base64 ≈ 7.8 MB > 5 MB）→ JPEG
    let img = noise(1400, 1400, 7);
    let png = encode_png(&img);
    assert!(
        b64_len(png.bytes.len()) > 5 * MB,
        "前提：PNG 超过 5 MB（{}）",
        png.bytes.len()
    );
    let enc = encode_for_llm(&img, Protocol::OpenAiChat);
    assert_eq!(enc.mime, "image/jpeg");
    assert_eq!((enc.width, enc.height), (1400, 1400), "JPEG 回退时不缩小");
    assert!(enc.bytes.starts_with(&[0xFF, 0xD8]), "JPEG SOI");
    assert!(b64_len(enc.bytes.len()) <= 20 * MB);
    let decoded = image::load_from_memory(&enc.bytes).expect("合法 JPEG");
    assert_eq!(decoded.width(), 1400);
}

/// 与 `encode_for_llm` 的 JPEG 回退同参数（q90、RGB）的编码长度
fn jpeg_q90_len(img: &RgbaImage) -> usize {
    let rgb = image::DynamicImage::ImageRgba8(img.clone()).to_rgb8();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
        .encode_image(&rgb)
        .expect("JPEG 编码");
    out.len()
}

#[test]
fn anthropic_over_10mb_png_is_compressed_below_limit() {
    // 构造 >10 MB 的 PNG 发给 Anthropic，先被压缩，不会 413
    let img = noise(2400, 2000, 11);
    let png = encode_png(&img);
    assert!(png.bytes.len() > 10 * MB, "前提：PNG 超过 10 MB（{}）", png.bytes.len());
    // 前提：全尺寸 JPEG q90 的 base64 仍超过 Anthropic 的 5 MB，必须走「长边降到 1568」这一步
    let full_jpeg = jpeg_q90_len(&img);
    assert!(
        b64_len(full_jpeg) > 5 * MB,
        "前提：全尺寸 JPEG 超过 5 MB（{full_jpeg}）"
    );

    let enc = encode_for_llm(&img, Protocol::AnthropicMessages);
    assert_eq!(enc.mime, "image/jpeg");
    // 2400×2000 → 长边 1568，短边 2000 × 1568 / 2400 = 1306.67 → 1307；1568 这一级已经达标，不再继续缩小
    assert_eq!((enc.width, enc.height), (1568, 1307));
    assert!(
        b64_len(enc.bytes.len()) <= 5 * MB,
        "base64 {} 字节",
        b64_len(enc.bytes.len())
    );
    let decoded = image::load_from_memory(&enc.bytes).expect("合法 JPEG");
    assert_eq!((decoded.width(), decoded.height()), (1568, 1307));
}

#[test]
fn anthropic_pixel_limit_applies_before_encoding() {
    let img = RgbaImage::from_pixel(9000, 300, Rgba([200, 200, 200, 255]));
    let enc = encode_for_llm(&img, Protocol::AnthropicMessages);
    assert_eq!((enc.width, enc.height), (8000, 267));
    assert_eq!(enc.mime, "image/png");
}

#[test]
fn half_resolution_frame_for_overlay_first_paint() {
    let monitor = MonitorInfo {
        id: "\\\\.\\DISPLAY1".to_owned(),
        name: "fake".to_owned(),
        physical_origin: PhysPoint { x: -2560, y: 0 },
        physical_size: PhysSize {
            width: 2561,
            height: 1441,
        },
        scale_factor: 1.5,
        frame: Rect::new(-1707.33, 0.0, 1707.33, 960.67),
        work_area: Rect::new(0.0, 0.0, 1707.33, 920.0),
        is_primary: false,
    };
    let frame = Frame::from_rgba(monitor, 2561, 1441, screenshot_like(2561, 1441).into_raw());
    let (w, h, half) = frame.rgba_half();
    assert_eq!((w, h), (1281, 721), "奇数边向上取整，不丢最后一列 / 行");
    assert_eq!(half.len(), (w * h * 4) as usize);
    assert!(half.chunks_exact(4).all(|p| p[3] == 255));
}
