//! 图片处理：PNG 流式解码。
//!
//! - 去重键：逐行把像素规范化为 RGBA8 后送入 blake3（含宽高），不分配整帧缓冲——4K 截图也只占一行的内存；
//! - 缩略图：同样逐行解码，按目标网格做面积平均（预乘 alpha，透明边缘不发黑），内存只有一行累加器 + 输出；
//!   不透明 → JPEG q80，带透明 → PNG；
//! - 应用图标：缩到长边 64 px 的 PNG。
//!
//! 内存上限：逐行解码时只有行缓冲，宽度受 [`MAX_WIDTH`] 限制（行缓冲 ≤ 512 KB），总像素受 [`MAX_PIXELS`] 限制（解码耗时）。
//! 隔行扫描（Adam7）的 PNG 必须整帧解码（剪贴板里极少见），单独限制为 [`MAX_INTERLACED_PIXELS`]（整帧 ≤ 32 MB，
//! 与一张 4K 截图的 RGBA 相当）；一张压缩后不到 1 MB 的大尺寸纯色隔行图因此不会让常驻进程瞬间分配数百 MB。

use std::io::{BufRead, Cursor, Seek};

use png::{BitDepth, ColorType, Transformations};

/// 单张图片的最大像素数（解压炸弹防护：20 MB 的 PNG 可以解出远超内存的像素；逐行解码不占内存，但要花时间）
pub(crate) const MAX_PIXELS: u64 = 128 * 1024 * 1024;
/// 最大宽度：逐行解码的行缓冲与宽度成正比（32768 px × 4 B = 128 KB；解码器内部另有同量级的缓冲）。
/// 多屏拼接的截图也远小于这个宽度；高度不限（长截图逐行处理）
pub(crate) const MAX_WIDTH: u32 = 32_768;
/// 隔行图的最大像素数：需要整帧缓冲，8 MiP × 4 B = 32 MB（3840×2160 的 4K 图在内）
pub(crate) const MAX_INTERLACED_PIXELS: u64 = 8 * 1024 * 1024;
/// 缩略图长边
pub(crate) const THUMB_LONG_EDGE: u32 = 256;
/// 缩略图 JPEG 质量
const THUMB_JPEG_QUALITY: u8 = 80;
/// 应用图标长边（列表 16–20 pt，@3x 也够用）
pub(crate) const ICON_LONG_EDGE: u32 = 64;

/// 图片处理错误
#[derive(Debug, thiserror::Error)]
pub(crate) enum ImageError {
    #[error("png decode: {0}")]
    Decode(#[from] png::DecodingError),
    #[error("png encode: {0}")]
    Encode(#[from] png::EncodingError),
    #[error("jpeg encode: {0}")]
    Jpeg(#[from] image::ImageError),
    #[error("image too large: {0}x{1}")]
    TooLarge(u32, u32),
    #[error("empty image")]
    Empty,
}

/// 原图信息
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PngInfo {
    pub width: u32,
    pub height: u32,
    /// 解码后 RGBA8 像素（含宽高）的 blake3
    pub hash: [u8; 32],
}

/// 编码好的缩略图
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EncodedThumb {
    pub bytes: Vec<u8>,
    /// `"jpg"` 或 `"png"`
    pub ext: &'static str,
    pub width: u32,
    pub height: u32,
}

/// 解析 PNG：尺寸 + 像素 hash
pub(crate) fn inspect_png(bytes: &[u8]) -> Result<PngInfo, ImageError> {
    inspect_png_from(Cursor::new(bytes))
}

/// 同 [`inspect_png`]，从任意可回退的读取端解码（例如同步下载到本地的原图文件）：逐行读、逐行 hash，
/// 不把整个文件读进内存
pub(crate) fn inspect_png_from<R: BufRead + Seek>(reader: R) -> Result<PngInfo, ImageError> {
    let mut hasher = blake3::Hasher::new();
    let mut header_done = false;
    let (width, height, _) = for_each_rgba_row_in(reader, |w, h, _y, row| {
        if !header_done {
            hasher.update(b"wisp:image\0");
            hasher.update(&w.to_le_bytes());
            hasher.update(&h.to_le_bytes());
            header_done = true;
        }
        hasher.update(row);
    })?;
    Ok(PngInfo {
        width,
        height,
        hash: *hasher.finalize().as_bytes(),
    })
}

/// 生成缩略图：长边 `long_edge`（原图更小时保持原尺寸）；`force_png` 为 true 时总是输出 PNG（应用图标）
pub(crate) fn make_thumbnail(bytes: &[u8], long_edge: u32, force_png: bool) -> Result<EncodedThumb, ImageError> {
    let mut sampler: Option<BoxSampler> = None;
    let (_, _, has_alpha_channel) = for_each_rgba_row(bytes, |w, h, y, row| {
        let s = sampler.get_or_insert_with(|| BoxSampler::new(w, h, long_edge));
        s.push_row(y, row);
    })?;
    let sampler = sampler.ok_or(ImageError::Empty)?;
    encode_thumb(sampler, has_alpha_channel, force_png)
}

/// 面积平均之后的编码：不透明 → JPEG q80（`force_png` 时 PNG RGB），带透明 → PNG RGBA
fn encode_thumb(sampler: BoxSampler, has_alpha_channel: bool, force_png: bool) -> Result<EncodedThumb, ImageError> {
    let (tw, th) = (sampler.tw, sampler.th);
    let opaque = !has_alpha_channel || sampler.min_alpha == u8::MAX;
    let rgba = sampler.finish();
    if opaque && !force_png {
        let rgb: Vec<u8> = rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
        let mut out = Vec::with_capacity(rgb.len() / 8);
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, THUMB_JPEG_QUALITY).encode(
            &rgb,
            tw,
            th,
            image::ExtendedColorType::Rgb8,
        )?;
        return Ok(EncodedThumb {
            bytes: out,
            ext: "jpg",
            width: tw,
            height: th,
        });
    }
    let (data, color) = if opaque {
        let rgb: Vec<u8> = rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
        (rgb, ColorType::Rgb)
    } else {
        (rgba, ColorType::Rgba)
    };
    Ok(EncodedThumb {
        bytes: encode_png(&data, tw, th, color)?,
        ext: "png",
        width: tw,
        height: th,
    })
}

/// 已经解码好的 RGBA8 像素（宽 × 高，逐行紧排）→ 缩略图（长边 `long_edge`）。OCR 历史的缩略图由编排器在编码
/// 截图的同一个阻塞任务里从裁剪帧直接生成（逐行送进同一个面积平均采样器，不复制整帧、不解码），写入时存储层不必再
/// 从送模型用的编码字节解码一遍
pub(crate) fn thumbnail_rgba(width: u32, height: u32, rgba: &[u8], long_edge: u32) -> Result<EncodedThumb, ImageError> {
    let row_len = usize::try_from(width).unwrap_or(usize::MAX).saturating_mul(4);
    if width == 0 || height == 0 || row_len == 0 {
        return Err(ImageError::Empty);
    }
    if rgba.len() / row_len < height as usize {
        return Err(ImageError::Empty);
    }
    let mut sampler = BoxSampler::new(width, height, long_edge);
    for (y, row) in rgba.chunks_exact(row_len).take(height as usize).enumerate() {
        sampler.push_row(y as u32, row);
    }
    encode_thumb(sampler, true, false)
}

/// 生成缩略图，原图可以是 PNG 或 JPEG（OCR 历史的原图在体积过大时是送模型用的 JPEG）。只在调用方没有现成缩略图时
/// 使用（OCR 历史的写入通常带着编排器生成的缩略图，见 [`thumbnail_rgba`]）。PNG 走流式的 [`make_thumbnail`]；JPEG 先只读
/// 文件头核对尺寸（超限不解码），再整图解码为 RGB8（zune-jpeg 没有按比例降采样的解码；OCR 截图长边已限制在 4096 以内，
/// 瞬时最多约 28 MB），逐行送进同一个面积平均采样器（不再有缩放的中间缓冲），输出 JPEG
pub(crate) fn thumbnail_any(bytes: &[u8], long_edge: u32) -> Result<EncodedThumb, ImageError> {
    if sniff_ext(bytes) != Some("jpg") {
        return make_thumbnail(bytes, long_edge, false);
    }
    let (w, h) = image::ImageReader::with_format(Cursor::new(bytes), image::ImageFormat::Jpeg).into_dimensions()?;
    if w == 0 || h == 0 {
        return Err(ImageError::Empty);
    }
    if u64::from(w) * u64::from(h) > MAX_PIXELS || w > MAX_WIDTH {
        return Err(ImageError::TooLarge(w, h));
    }
    let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg)?.into_rgb8();
    let (w, h) = img.dimensions();
    let mut sampler = BoxSampler::new(w, h, long_edge);
    let mut row = vec![u8::MAX; w as usize * 4];
    for (y, src) in img.rows().enumerate() {
        for (dst, px) in row.chunks_exact_mut(4).zip(src) {
            dst[..3].copy_from_slice(&px.0);
        }
        sampler.push_row(y as u32, &row);
    }
    drop(img);
    encode_thumb(sampler, false, false)
}

/// 编码 PNG（缩略图 / 图标 / 测试数据）
pub(crate) fn encode_png(data: &[u8], width: u32, height: u32, color: ColorType) -> Result<Vec<u8>, ImageError> {
    let mut out = Vec::with_capacity(data.len() / 2);
    {
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(color);
        enc.set_depth(BitDepth::Eight);
        enc.set_compression(png::Compression::Balanced);
        let mut writer = enc.write_header()?;
        writer.write_image_data(data)?;
        writer.finish()?;
    }
    Ok(out)
}

/// 逐行解码并把每行规范化为 RGBA8 交给 `f(width, height, y, row)`；返回（宽，高，源图是否带 alpha 通道 / tRNS）
fn for_each_rgba_row(bytes: &[u8], f: impl FnMut(u32, u32, u32, &[u8])) -> Result<(u32, u32, bool), ImageError> {
    for_each_rgba_row_in(Cursor::new(bytes), f)
}

/// [`for_each_rgba_row`] 的实现：读取端可以是内存，也可以是文件
fn for_each_rgba_row_in<R: BufRead + Seek>(
    reader: R,
    mut f: impl FnMut(u32, u32, u32, &[u8]),
) -> Result<(u32, u32, bool), ImageError> {
    let mut decoder = png::Decoder::new(reader);
    decoder.set_transformations(Transformations::normalize_to_color8());
    // 剪贴板 PNG 由系统编码，CRC / Adler 校验只会拖慢解码；结构错误仍会报错
    decoder.ignore_checksums(true);
    decoder.set_ignore_text_chunk(true);
    decoder.set_ignore_iccp_chunk(true);
    let header = decoder.read_header_info()?;
    let (w, h, interlaced) = (header.width, header.height, header.interlaced);
    if w == 0 || h == 0 {
        return Err(ImageError::Empty);
    }
    let pixels = u64::from(w) * u64::from(h);
    if w > MAX_WIDTH || pixels > MAX_PIXELS || (interlaced && pixels > MAX_INTERLACED_PIXELS) {
        return Err(ImageError::TooLarge(w, h));
    }
    if interlaced {
        // 整帧缓冲最多 MAX_INTERLACED_PIXELS × 4 字节（16 位图在 normalize_to_color8 之前的原始数据另计，留足余量）
        decoder.set_limits(png::Limits {
            bytes: usize::try_from(MAX_INTERLACED_PIXELS * 8 + (1 << 20)).unwrap_or(usize::MAX),
        });
    }
    let mut reader = decoder.read_info()?;
    let (color, _) = reader.output_color_type();
    let has_alpha = matches!(color, ColorType::Rgba | ColorType::GrayscaleAlpha);
    let width = w as usize;
    let mut rgba_row = vec![0u8; width * 4];

    if interlaced {
        let mut frame = vec![0u8; reader.output_buffer_size().ok_or(ImageError::TooLarge(w, h))?];
        let info = reader.next_frame(&mut frame)?;
        for (y, line) in frame.chunks_exact(info.line_size).take(h as usize).enumerate() {
            let row = to_rgba(color, line, &mut rgba_row);
            f(w, h, y as u32, row);
        }
        return Ok((w, h, has_alpha));
    }

    let mut y = 0u32;
    while let Some(line) = reader.next_row()? {
        let row = to_rgba(color, line.data(), &mut rgba_row);
        f(w, h, y, row);
        y += 1;
    }
    Ok((w, h, has_alpha))
}

/// 把一行（8 位，已展开调色板 / tRNS）转成 RGBA8；本来就是 RGBA 时直接借用
fn to_rgba<'a>(color: ColorType, line: &'a [u8], buf: &'a mut [u8]) -> &'a [u8] {
    match color {
        ColorType::Rgba => line,
        ColorType::Rgb => {
            for (dst, src) in buf.chunks_exact_mut(4).zip(line.chunks_exact(3)) {
                dst.copy_from_slice(&[src[0], src[1], src[2], u8::MAX]);
            }
            buf
        }
        ColorType::GrayscaleAlpha => {
            for (dst, src) in buf.chunks_exact_mut(4).zip(line.chunks_exact(2)) {
                dst.copy_from_slice(&[src[0], src[0], src[0], src[1]]);
            }
            buf
        }
        // normalize_to_color8 之后不会再出现 Indexed；按灰度处理只是兜底
        ColorType::Grayscale | ColorType::Indexed => {
            for (dst, &g) in buf.chunks_exact_mut(4).zip(line.iter()) {
                dst.copy_from_slice(&[g, g, g, u8::MAX]);
            }
            buf
        }
    }
}

/// 流式面积平均降采样：每个源像素恰好落进一个目标像素，按预乘 alpha 累加
struct BoxSampler {
    sh: u32,
    tw: u32,
    th: u32,
    /// 源列 → 目标列
    col_map: Vec<u32>,
    /// 当前目标行的累加器：Σ(r·a)、Σ(g·a)、Σ(b·a)、Σa、像素数
    acc: Vec<[u64; 5]>,
    current_ty: u32,
    out: Vec<u8>,
    min_alpha: u8,
}

impl BoxSampler {
    fn new(sw: u32, sh: u32, long_edge: u32) -> Self {
        let (tw, th) = fit_within(sw, sh, long_edge);
        let col_map = (0..sw)
            .map(|x| (u64::from(x) * u64::from(tw) / u64::from(sw)) as u32)
            .collect();
        Self {
            sh,
            tw,
            th,
            col_map,
            acc: vec![[0; 5]; tw as usize],
            current_ty: 0,
            out: Vec::with_capacity(tw as usize * th as usize * 4),
            min_alpha: u8::MAX,
        }
    }

    fn push_row(&mut self, y: u32, row: &[u8]) {
        let ty = (u64::from(y) * u64::from(self.th) / u64::from(self.sh)) as u32;
        while ty > self.current_ty {
            self.flush_row();
        }
        for (px, &tx) in row.chunks_exact(4).zip(self.col_map.iter()) {
            let a = u64::from(px[3]);
            self.min_alpha = self.min_alpha.min(px[3]);
            let cell = &mut self.acc[tx as usize];
            cell[0] += u64::from(px[0]) * a;
            cell[1] += u64::from(px[1]) * a;
            cell[2] += u64::from(px[2]) * a;
            cell[3] += a;
            cell[4] += 1;
        }
    }

    fn flush_row(&mut self) {
        for cell in &mut self.acc {
            let [ra, ga, ba, a, n] = *cell;
            if n == 0 || a == 0 {
                self.out.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                // 四舍五入：先反预乘颜色，再平均 alpha
                let c = |v: u64| ((v + a / 2) / a).min(255) as u8;
                let alpha = ((a + n / 2) / n).min(255) as u8;
                self.out.extend_from_slice(&[c(ra), c(ga), c(ba), alpha]);
            }
            *cell = [0; 5];
        }
        self.current_ty += 1;
    }

    fn finish(mut self) -> Vec<u8> {
        while self.current_ty < self.th {
            self.flush_row();
        }
        debug_assert_eq!(self.out.len(), self.tw as usize * self.th as usize * 4);
        self.out
    }
}

/// 等比缩放到长边不超过 `long_edge`（不放大），各边至少 1 px
pub(crate) fn fit_within(w: u32, h: u32, long_edge: u32) -> (u32, u32) {
    let long = w.max(h);
    if long <= long_edge {
        return (w, h);
    }
    let scale = |v: u32| ((u64::from(v) * u64::from(long_edge) + u64::from(long) / 2) / u64::from(long)).max(1) as u32;
    (scale(w), scale(h))
}

/// 识别缩略图字节的格式，返回扩展名
pub(crate) fn sniff_ext(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']) {
        Some("png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("jpg")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba_png(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                data.extend_from_slice(&f(x, y));
            }
        }
        encode_png(&data, w, h, ColorType::Rgba).expect("编码")
    }

    #[test]
    fn hash_is_over_pixels_not_encoding() {
        // 同样的像素：RGBA 编码 vs RGB 编码（不透明）→ 同一 hash
        let rgba = rgba_png(5, 3, |x, y| [x as u8 * 40, y as u8 * 80, 7, 255]);
        let mut rgb = Vec::new();
        for y in 0..3u32 {
            for x in 0..5u32 {
                rgb.extend_from_slice(&[x as u8 * 40, y as u8 * 80, 7]);
            }
        }
        let rgb_png = encode_png(&rgb, 5, 3, ColorType::Rgb).expect("编码");
        assert_ne!(rgba, rgb_png);
        let a = inspect_png(&rgba).expect("解码");
        let b = inspect_png(&rgb_png).expect("解码");
        assert_eq!((a.width, a.height), (5, 3));
        assert_eq!(a.hash, b.hash);
        // 像素相同但宽高不同 → 不同
        let flat = rgba_png(15, 1, |_, _| [0, 0, 0, 255]);
        let tall = rgba_png(1, 15, |_, _| [0, 0, 0, 255]);
        assert_ne!(
            inspect_png(&flat).expect("解码").hash,
            inspect_png(&tall).expect("解码").hash
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(inspect_png(b"not a png").is_err());
        assert!(make_thumbnail(&[0x89, b'P', b'N', b'G'], 256, false).is_err());
    }

    #[test]
    fn thumbnail_opaque_is_jpeg_with_long_edge_256() {
        let png = rgba_png(1024, 512, |x, _| [(x % 256) as u8, 100, 200, 255]);
        let t = make_thumbnail(&png, THUMB_LONG_EDGE, false).expect("缩略图");
        assert_eq!((t.ext, t.width, t.height), ("jpg", 256, 128));
        assert_eq!(sniff_ext(&t.bytes), Some("jpg"));
    }

    #[test]
    fn thumbnail_with_alpha_is_png_without_dark_fringe() {
        // 左半透明（颜色是黑色），右半不透明白色：预乘平均后边界不应发黑
        let png = rgba_png(
            512,
            512,
            |x, _| if x < 256 { [0, 0, 0, 0] } else { [255, 255, 255, 255] },
        );
        let t = make_thumbnail(&png, THUMB_LONG_EDGE, false).expect("缩略图");
        assert_eq!((t.ext, t.width, t.height), ("png", 256, 256));
        let mut dec = png::Decoder::new(Cursor::new(&t.bytes[..]));
        dec.set_transformations(Transformations::normalize_to_color8());
        let mut r = dec.read_info().expect("解码");
        let mut buf = vec![0; r.output_buffer_size().expect("尺寸")];
        r.next_frame(&mut buf).expect("帧");
        for px in buf.chunks_exact(4) {
            if px[3] > 0 {
                assert_eq!(&px[..3], &[255, 255, 255], "可见像素应保持白色");
            }
        }
    }

    #[test]
    fn rgba_frames_make_the_same_thumbnails_without_decoding() {
        // 不透明 → JPEG，尺寸与从 PNG 解码生成的一致
        let (w, h) = (1000u32, 500u32);
        let px = |x: u32, _y: u32| [(x % 256) as u8, 100, 200, 255];
        let rgba: Vec<u8> = (0..h).flat_map(|y| (0..w).flat_map(move |x| px(x, y))).collect();
        let t = thumbnail_rgba(w, h, &rgba, THUMB_LONG_EDGE).expect("缩略图");
        assert_eq!((t.ext, t.width, t.height), ("jpg", 256, 128));
        let from_png = make_thumbnail(&rgba_png(w, h, px), THUMB_LONG_EDGE, false).expect("缩略图");
        assert_eq!((from_png.width, from_png.height), (t.width, t.height));
        // 带透明 → PNG
        let mut clear = rgba.clone();
        clear[3] = 0;
        assert_eq!(
            thumbnail_rgba(w, h, &clear, THUMB_LONG_EDGE).expect("缩略图").ext,
            "png"
        );
        // 像素数据不足、空图
        assert!(thumbnail_rgba(w, h, &rgba[..rgba.len() - 1], THUMB_LONG_EDGE).is_err());
        assert!(thumbnail_rgba(0, h, &rgba, THUMB_LONG_EDGE).is_err());
    }

    #[test]
    fn jpeg_originals_are_checked_by_header_then_box_sampled() {
        let (w, h) = (1200u32, 600u32);
        let rgb: Vec<u8> = (0..h)
            .flat_map(|_| (0..w).flat_map(|x| [(x % 256) as u8, 40, 220]))
            .collect();
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90)
            .encode(&rgb, w, h, image::ExtendedColorType::Rgb8)
            .expect("编码");
        let t = thumbnail_any(&jpeg, THUMB_LONG_EDGE).expect("缩略图");
        assert_eq!((t.ext, t.width, t.height), ("jpg", 256, 128));
        // 坏的 JPEG：文件头不完整 → 错误；扫描数据被截断 → zune-jpeg 容错补齐，不 panic
        assert!(thumbnail_any(&jpeg[..16], THUMB_LONG_EDGE).is_err());
        let _ = thumbnail_any(&jpeg[..jpeg.len() / 3], THUMB_LONG_EDGE);
    }

    /// 测量用：OCR 历史缩略图的三条来源在 4K 截图上的耗时——从裁剪帧直接生成（编排器编码截图时）、从送模型的
    /// PNG 流式解码、从送模型的 JPEG 整图解码。手动运行：
    /// `cargo test -p wisp-clipboard --release --lib imaging::tests::ocr_thumbnail_cost -- --ignored --nocapture`
    #[test]
    #[ignore = "测量用，手动运行"]
    #[allow(clippy::print_stderr)]
    fn ocr_thumbnail_cost() {
        use std::time::Instant;
        let (w, h) = (3840u32, 2160u32);
        let px = |x: u32, y: u32| [(x * 7 % 251) as u8, (y * 3 % 241) as u8, ((x ^ y) % 256) as u8, 255];
        let rgba: Vec<u8> = (0..h).flat_map(|y| (0..w).flat_map(move |x| px(x, y))).collect();
        let png = rgba_png(w, h, px);
        let rgb: Vec<u8> = rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90)
            .encode(&rgb, w, h, image::ExtendedColorType::Rgb8)
            .expect("编码");
        let time = |f: &dyn Fn()| {
            let mut v: Vec<f64> = (0..7)
                .map(|_| {
                    let t = Instant::now();
                    f();
                    t.elapsed().as_secs_f64() * 1e3
                })
                .collect();
            v.sort_by(f64::total_cmp);
            v[3]
        };
        let a = time(&|| drop(thumbnail_rgba(w, h, &rgba, THUMB_LONG_EDGE).expect("缩略图")));
        let b = time(&|| drop(thumbnail_any(&png, THUMB_LONG_EDGE).expect("缩略图")));
        let c = time(&|| drop(thumbnail_any(&jpeg, THUMB_LONG_EDGE).expect("缩略图")));
        eprintln!(
            "ocr thumbnail 3840x2160 median of 7: from crop frame {a:.1} ms; from PNG {:.1} MB {b:.1} ms; from JPEG {:.1} MB {c:.1} ms",
            png.len() as f64 / 1e6,
            jpeg.len() as f64 / 1e6
        );
    }

    #[test]
    fn small_images_keep_size() {
        let png = rgba_png(40, 20, |_, _| [1, 2, 3, 255]);
        let t = make_thumbnail(&png, THUMB_LONG_EDGE, false).expect("缩略图");
        assert_eq!((t.width, t.height), (40, 20));
        let icon = make_thumbnail(&png, ICON_LONG_EDGE, true).expect("图标");
        assert_eq!(icon.ext, "png");
    }

    #[test]
    fn fit_rules() {
        assert_eq!(fit_within(3840, 2160, 256), (256, 144));
        assert_eq!(fit_within(2160, 3840, 256), (144, 256));
        assert_eq!(fit_within(10000, 1, 256), (256, 1));
        assert_eq!(fit_within(100, 50, 256), (100, 50));
    }

    /// 只有文件头的 PNG（尺寸检查发生在读到图像数据之前）：IHDR + 空 IDAT + IEND，CRC 正确
    fn header_only_png(w: u32, h: u32, interlaced: bool) -> Vec<u8> {
        fn crc32(data: &[u8]) -> u32 {
            let mut crc = u32::MAX;
            for &b in data {
                crc ^= u32::from(b);
                for _ in 0..8 {
                    crc = if crc & 1 == 1 {
                        (crc >> 1) ^ 0xedb8_8320
                    } else {
                        crc >> 1
                    };
                }
            }
            !crc
        }
        fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            let start = out.len();
            out.extend_from_slice(kind);
            out.extend_from_slice(data);
            let crc = crc32(&out[start..]);
            out.extend_from_slice(&crc.to_be_bytes());
        }
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        // 8 位灰度、deflate、自适应过滤、是否隔行
        ihdr.extend_from_slice(&[8, 0, 0, 0, u8::from(interlaced)]);
        let mut out = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
        chunk(&mut out, b"IHDR", &ihdr);
        // 空的 zlib 流（stored 块，长度 0）
        chunk(
            &mut out,
            b"IDAT",
            &[0x78, 0x01, 0x01, 0x00, 0x00, 0xff, 0xff, 0x00, 0x00, 0x00, 0x01],
        );
        chunk(&mut out, b"IEND", &[]);
        out
    }

    #[test]
    fn large_interlaced_and_overwide_images_are_rejected_before_allocating() {
        // 11585² ≈ 134 MP 的隔行图：旧上限下要分配约 512 MB 的整帧缓冲
        for (w, h, interlaced) in [(11_585, 11_585, true), (4_000, 4_000, true), (100_000_000, 1, false)] {
            let png = header_only_png(w, h, interlaced);
            assert!(
                matches!(inspect_png(&png), Err(ImageError::TooLarge(..))),
                "{w}x{h} interlaced={interlaced}"
            );
            assert!(matches!(
                make_thumbnail(&png, THUMB_LONG_EDGE, false),
                Err(ImageError::TooLarge(..))
            ));
        }
        // 上限之内的隔行图照常进入解码（这里的空图像数据会在解码阶段报错，而不是 TooLarge）
        assert!(matches!(
            inspect_png(&header_only_png(2_000, 2_000, true)),
            Err(ImageError::Decode(_))
        ));
    }

    #[test]
    fn interlaced_png_is_supported() {
        // 由 Python zlib 手工生成的 Adam7 隔行 PNG（png crate 的编码器不支持隔行写入），像素为 (x, y, 3, 255)
        let interlaced = include_bytes!("../tests/fixtures/interlaced_7x9.png");
        let plain = rgba_png(7, 9, |x, y| [x as u8, y as u8, 3, 255]);
        let a = inspect_png(interlaced).expect("隔行");
        assert_eq!((a.width, a.height), (7, 9));
        assert_eq!(a.hash, inspect_png(&plain).expect("逐行").hash);
        let t = make_thumbnail(interlaced, THUMB_LONG_EDGE, false).expect("缩略图");
        assert_eq!((t.width, t.height, t.ext), (7, 9, "jpg"));
    }
}
