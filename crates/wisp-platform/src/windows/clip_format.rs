//! 剪贴板数据格式的编解码（纯逻辑，不调用 Win32，可在任意平台单测）：
//! - `CF_UNICODETEXT`：UTF-16LE + NUL，换行在 Windows 上是 CRLF；
//! - `HTML Format`（CF_HTML）：带字节偏移头的 UTF-8；
//! - `CF_HDROP`：`DROPFILES` 头 + 双 NUL 结尾的 UTF-16 路径列表；
//! - `CF_DIB` / `CF_DIBV5`：设备无关位图 ↔ RGBA ↔ PNG；
//! - Wisp 私有标记的载荷。

use std::borrow::Cow;
use std::io::Cursor;

/// 单张图片允许解码的最大像素数（约 8K × 8K），防止异常尺寸耗尽内存
pub(crate) const MAX_IMAGE_PIXELS: u64 = 64 * 1024 * 1024;

// ---------------------------------------------------------------- 文本

/// CRLF / 单独的 CR → LF
pub(crate) fn to_lf(s: &str) -> Cow<'_, str> {
    if !s.contains('\r') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// 单独的 LF → CRLF（已有的 CRLF 不重复转换），用于写入 `CF_UNICODETEXT`
pub(crate) fn to_crlf(s: &str) -> Cow<'_, str> {
    if !s.contains('\n') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + s.len() / 16);
    let mut prev = '\0';
    for c in s.chars() {
        if c == '\n' && prev != '\r' {
            out.push('\r');
        }
        out.push(c);
        prev = c;
    }
    Cow::Owned(out)
}

/// UTF-16LE 字节（可能未对齐、可能没有 NUL）→ 字符串：在第一个 NUL 处或 `max_units` 个码元处停止。
/// 返回 (字符串, 是否因达到上限而截断)
pub(crate) fn utf16le_to_string(bytes: &[u8], max_units: usize) -> (String, bool) {
    let total = bytes.len() / 2;
    let take = total.min(max_units);
    let mut units = Vec::with_capacity(take);
    let mut hit_nul = false;
    for c in bytes.chunks_exact(2).take(take) {
        let u = u16::from_le_bytes([c[0], c[1]]);
        if u == 0 {
            hit_nul = true;
            break;
        }
        units.push(u);
    }
    // 上限之后紧跟的就是 NUL 时不算截断
    let next_is_nul = bytes
        .get(take * 2..take * 2 + 2)
        .is_some_and(|c| c[0] == 0 && c[1] == 0);
    let capped = !hit_nul && total > take && !next_is_nul;
    (String::from_utf16_lossy(&units), capped)
}

/// 字符串 → UTF-16LE 字节并以 NUL 结尾
pub(crate) fn utf16le_z(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity((s.len() + 1) * 2);
    for u in s.encode_utf16() {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out.extend_from_slice(&[0, 0]);
    out
}

/// 在 UTF-8 字符边界上截断到不超过 `max` 字节，返回是否发生了截断
pub(crate) fn truncate_utf8(s: &mut String, max: usize) -> bool {
    if s.len() <= max {
        return false;
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    true
}

// ---------------------------------------------------------------- CF_HTML

const START_MARKER: &str = "<!--StartFragment-->";
const END_MARKER: &str = "<!--EndFragment-->";

/// 把 HTML 片段包装成 CF_HTML（偏移量是相对数据开头的 UTF-8 字节数，固定 10 位数字）
pub(crate) fn build_cf_html(fragment: &str) -> Vec<u8> {
    const HEADER_TEMPLATE: &str = "Version:0.9\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\nStartFragment:0000000000\r\nEndFragment:0000000000\r\n";
    let prefix = format!("<html><body>\r\n{START_MARKER}");
    let suffix = format!("{END_MARKER}\r\n</body></html>");
    let start_html = HEADER_TEMPLATE.len();
    let start_fragment = start_html + prefix.len();
    let end_fragment = start_fragment + fragment.len();
    let end_html = end_fragment + suffix.len();
    let header = format!(
        "Version:0.9\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\nStartFragment:{start_fragment:010}\r\nEndFragment:{end_fragment:010}\r\n"
    );
    debug_assert_eq!(header.len(), start_html);
    let mut out = Vec::with_capacity(end_html + 1);
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(prefix.as_bytes());
    out.extend_from_slice(fragment.as_bytes());
    out.extend_from_slice(suffix.as_bytes());
    out.push(0);
    out
}

/// 解析 CF_HTML，取出片段（StartFragment..EndFragment）。偏移无效时退回注释标记，再退回 StartHTML..EndHTML。
/// 片段超过 `max_bytes` 时返回 None（HTML 超过上限就丢弃该格式）
pub(crate) fn parse_cf_html(data: &[u8], max_bytes: usize) -> Option<String> {
    let len = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    let data = &data[..len];
    let header_end = find(data, b"<").unwrap_or(data.len()).min(1024);
    let header = String::from_utf8_lossy(&data[..header_end]);
    let field = |name: &str| -> Option<usize> {
        header.lines().find_map(|line| {
            let (k, v) = line.split_once(':')?;
            if k.trim() != name {
                return None;
            }
            let v: i64 = v.trim().parse().ok()?;
            usize::try_from(v).ok()
        })
    };
    let valid = |s: usize, e: usize| s <= e && e <= data.len();
    let range = match (field("StartFragment"), field("EndFragment")) {
        (Some(s), Some(e)) if valid(s, e) => Some((s, e)),
        _ => None,
    }
    .or_else(|| {
        let s = find(data, START_MARKER.as_bytes())? + START_MARKER.len();
        let e = find(&data[s..], END_MARKER.as_bytes())? + s;
        Some((s, e))
    })
    .or_else(|| match (field("StartHTML"), field("EndHTML")) {
        (Some(s), Some(e)) if valid(s, e) => Some((s, e)),
        _ => None,
    })?;
    let bytes = &data[range.0..range.1];
    if bytes.len() > max_bytes {
        return None;
    }
    Some(String::from_utf8_lossy(bytes).into_owned())
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------- CF_HDROP

/// 构造 `DROPFILES`（20 字节头，fWide = TRUE）+ 双 NUL 结尾的 UTF-16 路径列表
pub(crate) fn build_dropfiles(paths: &[String]) -> Vec<u8> {
    const HEADER: u32 = 20;
    let mut out = Vec::with_capacity(HEADER as usize + paths.iter().map(|p| (p.len() + 1) * 2).sum::<usize>() + 2);
    out.extend_from_slice(&HEADER.to_le_bytes()); // pFiles
    out.extend_from_slice(&0i32.to_le_bytes()); // pt.x
    out.extend_from_slice(&0i32.to_le_bytes()); // pt.y
    out.extend_from_slice(&0i32.to_le_bytes()); // fNC
    out.extend_from_slice(&1i32.to_le_bytes()); // fWide
    for p in paths {
        let p = p.replace('/', "\\");
        for u in p.encode_utf16() {
            out.extend_from_slice(&u.to_le_bytes());
        }
        out.extend_from_slice(&[0, 0]);
    }
    out.extend_from_slice(&[0, 0]);
    out
}

/// 解析宽字符版 `DROPFILES`；ANSI 版本返回 None（调用方改用 `DragQueryFileW`）
pub(crate) fn parse_dropfiles_wide(data: &[u8]) -> Option<Vec<String>> {
    if data.len() < 20 {
        return None;
    }
    let p_files = u32::from_le_bytes(data[0..4].try_into().ok()?) as usize;
    let wide = i32::from_le_bytes(data[16..20].try_into().ok()?) != 0;
    if !wide || p_files > data.len() {
        return None;
    }
    let mut out = Vec::new();
    let mut cur: Vec<u16> = Vec::new();
    for c in data[p_files..].chunks_exact(2) {
        let u = u16::from_le_bytes([c[0], c[1]]);
        if u == 0 {
            if cur.is_empty() {
                break;
            }
            out.push(String::from_utf16_lossy(&cur));
            cur.clear();
        } else {
            cur.push(u);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------- DIB / PNG

/// RGBA 图像（自上而下，非预乘）
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rgba {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

fn rd_u16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn rd_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn rd_i32(b: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// 按掩码取出通道值并缩放到 8 位
fn mask_channel(px: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let bits = (mask >> shift).count_ones();
    let v = (px & mask) >> shift;
    if bits >= 8 {
        (v >> (bits - 8)) as u8
    } else {
        let max = (1u32 << bits) - 1;
        ((v * 255 + max / 2) / max) as u8
    }
}

/// DIB 解码结果：位图像素，或 `BI_PNG` 内嵌的 PNG 原始字节
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DibImage {
    Pixels(Rgba),
    Png(Vec<u8>),
}

/// 未压缩位图转 PNG 前的粗筛倍数（与 macOS 对 TIFF 的处理一致）：解码后的 RGBA 超过 PNG 上限的这么多倍时直接放弃
pub(crate) const DIB_PREFILTER_FACTOR: u64 = 4;

/// 从 DIB 头读取尺寸（宽, 高）；头不完整或尺寸非法时返回 None
pub(crate) fn dib_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let header = rd_u32(data, 0)? as usize;
    if header < 40 || header > data.len() {
        return None;
    }
    let width = rd_i32(data, 4)?;
    let height = rd_i32(data, 8)?;
    (width > 0 && height != 0).then(|| (width as u32, height.unsigned_abs()))
}

/// 未压缩位图（32 位，宽 × 高 × 4）是否在预算之内：不超过 `max_png_bytes × DIB_PREFILTER_FACTOR`，像素数不超过
/// [`MAX_IMAGE_PIXELS`]。读写两个方向共用这一个上限：读取时据此决定是否拷出 DIB 解码，写入时据此决定是否附带
/// DIBV5——Wisp 写出的 DIB 自己一定读得回来，常驻进程也不会因为一份未压缩副本出现上百 MB 的内存尖峰
pub(crate) fn dib_within_budget(width: u32, height: u32, max_png_bytes: usize) -> bool {
    let pixels = u64::from(width) * u64::from(height);
    let budget = u64::try_from(max_png_bytes)
        .unwrap_or(u64::MAX)
        .saturating_mul(DIB_PREFILTER_FACTOR);
    pixels <= MAX_IMAGE_PIXELS && pixels.saturating_mul(4) <= budget
}

/// 读剪贴板时是否值得把这份 DIB 拷出来解码：只看头部尺寸（[`dib_within_budget`]）。在拷贝之前判断，过大的图片
/// 既不占用剪贴板做无用功，也不会让常驻进程出现几百 MB 的内存尖峰
pub(crate) fn dib_worth_decoding(data: &[u8], max_png_bytes: usize) -> bool {
    dib_dimensions(data).is_some_and(|(w, h)| dib_within_budget(w, h, max_png_bytes))
}

/// 解析 `CF_DIB` / `CF_DIBV5`（`BITMAPINFOHEADER` / V4 / V5 + 可选掩码 + 调色板 + 像素）。
/// 支持 1 / 4 / 8 位调色板、16 / 24 / 32 位（`BI_RGB` / `BI_BITFIELDS` / `BI_ALPHABITFIELDS`）与 `BI_PNG`；
/// RLE、JPEG 与 OS/2 核心头返回 None
pub(crate) fn decode_dib(data: &[u8]) -> Option<DibImage> {
    const BI_RGB: u32 = 0;
    const BI_BITFIELDS: u32 = 3;
    const BI_PNG: u32 = 5;
    const BI_ALPHABITFIELDS: u32 = 6;

    let header = rd_u32(data, 0)? as usize;
    if header < 40 || header > data.len() {
        return None;
    }
    let width = rd_i32(data, 4)?;
    let height = rd_i32(data, 8)?;
    let bpp = rd_u16(data, 14)?;
    let compression = rd_u32(data, 16)?;
    let size_image = rd_u32(data, 20)? as usize;
    let clr_used = rd_u32(data, 32)? as usize;
    if width <= 0 || height == 0 {
        return None;
    }
    let w = width as u32;
    let h = height.unsigned_abs();
    let top_down = height < 0;
    if u64::from(w) * u64::from(h) > MAX_IMAGE_PIXELS {
        return None;
    }

    // 掩码：V2+（≥52 字节）的头内含 RGB 掩码，V3+（≥56）含 A 掩码；40 字节头 + BITFIELDS 时掩码紧跟在头后面
    let mut extra = 0usize;
    let (mut rm, mut gm, mut bm, mut am) = (0u32, 0u32, 0u32, 0u32);
    if compression == BI_BITFIELDS || compression == BI_ALPHABITFIELDS {
        if header >= 52 {
            rm = rd_u32(data, 40)?;
            gm = rd_u32(data, 44)?;
            bm = rd_u32(data, 48)?;
            if header >= 56 {
                am = rd_u32(data, 52)?;
            }
        } else {
            rm = rd_u32(data, header)?;
            gm = rd_u32(data, header + 4)?;
            bm = rd_u32(data, header + 8)?;
            extra = 12;
            if compression == BI_ALPHABITFIELDS {
                am = rd_u32(data, header + 12)?;
                extra = 16;
            }
        }
    } else if header >= 56 && compression == BI_RGB && bpp == 32 {
        // V4 / V5 + BI_RGB：头里的 A 掩码仍有意义
        am = rd_u32(data, 52)?;
    }

    let palette_len = if bpp == 0 {
        // BI_PNG / BI_JPEG：没有调色板
        0
    } else if bpp <= 8 {
        let n = if clr_used == 0 {
            1usize << bpp
        } else {
            clr_used.min(256)
        };
        n * 4
    } else {
        // 16 / 24 / 32 位 BI_RGB 也可能带一个（不使用的）颜色表
        clr_used.min(256) * 4
    };
    let palette_at = header + extra;
    let pixels_at = palette_at + palette_len;

    if compression == BI_PNG {
        let end = if size_image == 0 {
            data.len()
        } else {
            (pixels_at + size_image).min(data.len())
        };
        let png = data.get(pixels_at..end)?;
        return png.starts_with(PNG_SIGNATURE).then(|| DibImage::Png(png.to_vec()));
    }
    if compression != BI_RGB && compression != BI_BITFIELDS && compression != BI_ALPHABITFIELDS {
        return None;
    }

    let stride = ((u64::from(w) * u64::from(bpp)).div_ceil(32) * 4) as usize;
    let need = stride.checked_mul(h as usize)?;
    let pixel_data = data.get(pixels_at..pixels_at.checked_add(need)?)?;
    let palette = data.get(palette_at..pixels_at)?;

    if compression == BI_RGB {
        match bpp {
            16 => {
                rm = 0x7C00;
                gm = 0x03E0;
                bm = 0x001F;
            }
            32 => {
                rm = 0x00FF_0000;
                gm = 0x0000_FF00;
                bm = 0x0000_00FF;
                if am == 0 {
                    am = 0xFF00_0000;
                }
            }
            _ => {}
        }
    }

    let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
    let mut any_alpha = false;
    for y in 0..h as usize {
        let src_row = if top_down { y } else { h as usize - 1 - y };
        let row = &pixel_data[src_row * stride..(src_row + 1) * stride];
        let dst = &mut out[y * w as usize * 4..(y + 1) * w as usize * 4];
        for x in 0..w as usize {
            let (r, g, b, a) = match bpp {
                1 | 4 | 8 => {
                    let bits = bpp as usize;
                    let bit = x * bits;
                    let byte = row[bit / 8];
                    let shift = 8 - bits - (bit % 8);
                    let idx = ((byte >> shift) & ((1u16 << bits) - 1) as u8) as usize;
                    let p = palette.get(idx * 4..idx * 4 + 4).unwrap_or(&[0, 0, 0, 0]);
                    (p[2], p[1], p[0], 255)
                }
                16 => {
                    let px = u32::from(u16::from_le_bytes([row[x * 2], row[x * 2 + 1]]));
                    let a = if am != 0 { mask_channel(px, am) } else { 255 };
                    (mask_channel(px, rm), mask_channel(px, gm), mask_channel(px, bm), a)
                }
                24 => (row[x * 3 + 2], row[x * 3 + 1], row[x * 3], 255),
                32 => {
                    let px = u32::from_le_bytes([row[x * 4], row[x * 4 + 1], row[x * 4 + 2], row[x * 4 + 3]]);
                    let a = if am != 0 { mask_channel(px, am) } else { 255 };
                    (mask_channel(px, rm), mask_channel(px, gm), mask_channel(px, bm), a)
                }
                _ => return None,
            };
            if a != 0 {
                any_alpha = true;
            }
            dst[x * 4..x * 4 + 4].copy_from_slice(&[r, g, b, a]);
        }
    }
    // 32 位位图的第 4 字节经常是未定义的 0：全为 0 时按不透明处理
    if !any_alpha {
        for px in out.chunks_exact_mut(4) {
            px[3] = 255;
        }
    }
    Some(DibImage::Pixels(Rgba {
        width: w,
        height: h,
        pixels: out,
    }))
}

/// `BITMAPV5HEADER` 的长度
const DIBV5_HEADER: usize = 124;

/// `BITMAPV5HEADER`：32 位 `BI_BITFIELDS`（BGRA 掩码）+ sRGB，高度为正（自下而上），非预乘 alpha
fn dibv5_header(width: u32, height: u32) -> [u8; DIBV5_HEADER] {
    const LCS_SRGB: u32 = 0x7352_4742; // 'sRGB'
    const LCS_GM_IMAGES: u32 = 4;
    let image_bytes = u32::try_from(u64::from(width) * u64::from(height) * 4).unwrap_or(0);
    let mut h = Vec::with_capacity(DIBV5_HEADER);
    h.extend_from_slice(&(DIBV5_HEADER as u32).to_le_bytes());
    h.extend_from_slice(&(width as i32).to_le_bytes());
    h.extend_from_slice(&(height as i32).to_le_bytes()); // 正数：自下而上
    h.extend_from_slice(&1u16.to_le_bytes()); // planes
    h.extend_from_slice(&32u16.to_le_bytes()); // bpp
    h.extend_from_slice(&3u32.to_le_bytes()); // BI_BITFIELDS
    h.extend_from_slice(&image_bytes.to_le_bytes());
    h.extend_from_slice(&2835i32.to_le_bytes()); // 72 DPI
    h.extend_from_slice(&2835i32.to_le_bytes());
    h.extend_from_slice(&0u32.to_le_bytes()); // clr used
    h.extend_from_slice(&0u32.to_le_bytes()); // clr important
    h.extend_from_slice(&0x00FF_0000u32.to_le_bytes()); // R
    h.extend_from_slice(&0x0000_FF00u32.to_le_bytes()); // G
    h.extend_from_slice(&0x0000_00FFu32.to_le_bytes()); // B
    h.extend_from_slice(&0xFF00_0000u32.to_le_bytes()); // A
    h.extend_from_slice(&LCS_SRGB.to_le_bytes());
    h.extend_from_slice(&[0u8; 36]); // endpoints
    h.extend_from_slice(&[0u8; 12]); // gamma
    h.extend_from_slice(&LCS_GM_IMAGES.to_le_bytes());
    h.extend_from_slice(&[0u8; 12]); // profile data / size / reserved
    let mut out = [0u8; DIBV5_HEADER];
    out.copy_from_slice(&h);
    out
}

/// `CF_DIBV5`（V5 头 + 32 位像素）的总字节数；尺寸非法或超出寻址范围时为 None
pub(crate) fn dibv5_len(width: u32, height: u32) -> Option<usize> {
    if width == 0 || height == 0 || width > i32::MAX as u32 || height > i32::MAX as u32 {
        return None;
    }
    let pixels = usize::try_from(u64::from(width) * u64::from(height)).ok()?;
    pixels.checked_mul(4)?.checked_add(DIBV5_HEADER)
}

/// PNG → `CF_DIBV5`，直接写进 `out`（长度必须等于 [`dibv5_len`]）。先把整帧解码到像素区的开头，再原地展开成
/// BGRA、上下翻转成自下而上：全程只有 `out` 这一份像素缓冲，不保留中间的 RGBA，峰值内存约为一份 DIB。
/// 隔行扫描（Adam7）、调色板、灰度、16 位的 PNG 都适用。PNG 无法解码时返回 None
pub(crate) fn png_into_dibv5(png: &[u8], out: &mut [u8]) -> Option<()> {
    let (w, h) = png_dimensions(png)?;
    if out.len() != dibv5_len(w, h)? {
        return None;
    }
    let pixels = u64::from(w) * u64::from(h);
    if pixels > MAX_IMAGE_PIXELS {
        return None;
    }
    // 解码器自身的内存上限（行缓冲、辅助块等）；整帧输出写进调用方给的缓冲区，不计在内
    let limit = usize::try_from(pixels.saturating_mul(2) + 1024 * 1024).ok()?;
    let mut decoder = png::Decoder::new_with_limits(Cursor::new(png), png::Limits { bytes: limit });
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let (color, depth) = reader.output_color_type();
    if depth != png::BitDepth::Eight {
        return None;
    }
    let bpp = match color {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => return None,
    };
    let n = (w as usize) * (h as usize);
    let (header, px) = out.split_at_mut(DIBV5_HEADER);
    if reader.output_buffer_size()? != n * bpp {
        return None;
    }
    let info = reader.next_frame(&mut px[..n * bpp]).ok()?;
    if (info.width, info.height) != (w, h) {
        return None;
    }
    header.copy_from_slice(&dibv5_header(w, h));

    // 原地展开为 BGRA。源像素 k 位于 [k·bpp, k·bpp + bpp)，目标像素 i 位于 [4i, 4i + 4)：从最后一个像素往前处理时，
    // 写入位置不会早于任何尚未读取的源像素（k < i ⇒ k·bpp + bpp ≤ i·bpp ≤ 4i），每个像素先读后写
    match bpp {
        4 => px.chunks_exact_mut(4).for_each(|p| p.swap(0, 2)),
        3 => {
            for i in (0..n).rev() {
                let (r, g, b) = (px[i * 3], px[i * 3 + 1], px[i * 3 + 2]);
                px[i * 4..i * 4 + 4].copy_from_slice(&[b, g, r, 255]);
            }
        }
        2 => {
            for i in (0..n).rev() {
                let (v, a) = (px[i * 2], px[i * 2 + 1]);
                px[i * 4..i * 4 + 4].copy_from_slice(&[v, v, v, a]);
            }
        }
        _ => {
            for i in (0..n).rev() {
                let v = px[i];
                px[i * 4..i * 4 + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
    }
    // 自上而下 → 自下而上：逐行对调
    let stride = w as usize * 4;
    let rows = h as usize;
    for y in 0..rows / 2 {
        let (upper, lower) = px.split_at_mut((rows - 1 - y) * stride);
        upper[y * stride..(y + 1) * stride].swap_with_slice(&mut lower[..stride]);
    }
    Some(())
}

/// RGBA → `CF_DIBV5`（测试用的参照实现：生产路径用 [`png_into_dibv5`] 直接写进全局内存）
#[cfg(test)]
pub(crate) fn encode_dibv5(img: &Rgba) -> Vec<u8> {
    let w = img.width as usize;
    let h = img.height as usize;
    let mut out = Vec::with_capacity(DIBV5_HEADER + w * h * 4);
    out.extend_from_slice(&dibv5_header(img.width, img.height));
    for y in (0..h).rev() {
        let row = &img.pixels[y * w * 4..(y + 1) * w * 4];
        for px in row.chunks_exact(4) {
            out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
    }
    out
}

/// PNG 文件签名
pub(crate) const PNG_SIGNATURE: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// 从 PNG 头（IHDR）读取尺寸；不是合法 PNG 时返回 None
pub(crate) fn png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if !data.starts_with(PNG_SIGNATURE) || data.get(12..16)? != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(data.get(16..20)?.try_into().ok()?);
    let h = u32::from_be_bytes(data.get(20..24)?.try_into().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

/// RGBA → PNG（快速压缩档：剪贴板图片在关键路径上，压缩率由存储层按需再处理）
pub(crate) fn encode_png(img: &Rgba) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(img.pixels.len() / 3 + 1024);
    {
        let mut enc = png::Encoder::new(&mut out, img.width, img.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(&img.pixels).ok()?;
        writer.finish().ok()?;
    }
    Some(out)
}

/// PNG → RGBA（任意颜色类型 / 位深都展开成 8 位 RGBA；测试用的参照实现）
#[cfg(test)]
pub(crate) fn decode_png(data: &[u8]) -> Option<Rgba> {
    let (w, h) = png_dimensions(data)?;
    let pixels = u64::from(w) * u64::from(h);
    if pixels > MAX_IMAGE_PIXELS {
        return None;
    }
    let limit = usize::try_from(pixels * 8 + 1024 * 1024).ok()?;
    let mut decoder = png::Decoder::new_with_limits(Cursor::new(data), png::Limits { bytes: limit });
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    let n = (w as usize) * (h as usize);
    let src = &buf[..info.buffer_size()];
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => src.to_vec(),
        png::ColorType::Rgb => src.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => src.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => src.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    (rgba.len() == n * 4).then_some(Rgba {
        width: w,
        height: h,
        pixels: rgba,
    })
}

// ---------------------------------------------------------------- Wisp 标记

/// Wisp 写入剪贴板的原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MarkerKind {
    /// `Paste::write`：Wisp 自己的内容（其中的排除历史等格式是 Wisp 加的）
    Write,
    /// 模拟复制回退后恢复的用户原内容
    Restore,
}

/// 标记载荷
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Marker {
    pub kind: MarkerKind,
    /// 恢复的原内容本身带隐私标记（读取时仍按隐私内容处理）
    pub private: bool,
}

const MARKER_MAGIC: &[u8; 4] = b"WISP";

/// 编码标记载荷
pub(crate) fn encode_marker(m: Marker) -> Vec<u8> {
    let kind = match m.kind {
        MarkerKind::Write => 1u8,
        MarkerKind::Restore => 2u8,
    };
    let mut v = MARKER_MAGIC.to_vec();
    v.extend_from_slice(&[1, kind, u8::from(m.private)]);
    v
}

/// 解码标记载荷；内容不可识别（例如旧版本写入的空载荷）时按 `Write` 处理
pub(crate) fn decode_marker(data: &[u8]) -> Marker {
    match data {
        [b'W', b'I', b'S', b'P', 1, kind, flags, ..] => Marker {
            kind: if *kind == 2 {
                MarkerKind::Restore
            } else {
                MarkerKind::Write
            },
            private: flags & 1 != 0,
        },
        _ => Marker {
            kind: MarkerKind::Write,
            private: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_endings() {
        assert_eq!(to_lf("a\r\nb\rc\n"), "a\nb\nc\n");
        assert!(matches!(to_lf("plain"), Cow::Borrowed(_)));
        assert_eq!(to_crlf("a\nb\r\nc"), "a\r\nb\r\nc");
        assert_eq!(to_crlf("\n\n"), "\r\n\r\n");
        assert_eq!(to_lf(&to_crlf("x\ny\n")), "x\ny\n");
    }

    #[test]
    fn utf16_roundtrip_and_caps() {
        let z = utf16le_z("中文 abc");
        assert_eq!(utf16le_to_string(&z, usize::MAX), ("中文 abc".to_owned(), false));
        // 没有 NUL、达到上限：标记截断
        let raw: Vec<u8> = "abcdef".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(utf16le_to_string(&raw, 3), ("abc".to_owned(), true));
        // 恰好在上限处有 NUL：不算截断
        assert_eq!(utf16le_to_string(&utf16le_z("abc"), 3), ("abc".to_owned(), false));
        // 奇数长度（未对齐的尾字节）被忽略
        assert_eq!(utf16le_to_string(&[0x41, 0x00, 0x42], 10).0, "A");
    }

    #[test]
    fn truncate_on_char_boundary() {
        let mut s = "中文字".to_owned(); // 每个字 3 字节
        assert!(truncate_utf8(&mut s, 7));
        assert_eq!(s, "中文");
        let mut s = "abc".to_owned();
        assert!(!truncate_utf8(&mut s, 3));
    }

    #[test]
    fn cf_html_roundtrip() {
        let frag = "<b>你好</b> world";
        let data = build_cf_html(frag);
        assert_eq!(parse_cf_html(&data, 1 << 20).as_deref(), Some(frag));
        // 头部偏移与实际内容一致
        let text = String::from_utf8_lossy(&data);
        let start: usize = text
            .lines()
            .find_map(|l| l.strip_prefix("StartFragment:"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(&data[start..start + frag.len()], frag.as_bytes());
        // 超过上限丢弃
        assert_eq!(parse_cf_html(&data, 4), None);
    }

    #[test]
    fn cf_html_with_bad_offsets_falls_back_to_markers() {
        let data = b"Version:0.9\r\nStartHTML:-1\r\nEndHTML:-1\r\nStartFragment:999999\r\nEndFragment:5\r\n<html><body><!--StartFragment--><i>x</i><!--EndFragment--></body></html>";
        assert_eq!(parse_cf_html(data, 1024).as_deref(), Some("<i>x</i>"));
        assert_eq!(parse_cf_html(b"garbage without markers", 1024), None);
    }

    #[test]
    fn dropfiles_roundtrip() {
        let paths = vec![r"C:\a\b.txt".to_owned(), "D:/中文/c.png".to_owned()];
        let data = build_dropfiles(&paths);
        assert_eq!(
            parse_dropfiles_wide(&data).unwrap(),
            vec![r"C:\a\b.txt".to_owned(), r"D:\中文\c.png".to_owned()]
        );
        let mut ansi = data.clone();
        ansi[16] = 0; // fWide = FALSE
        assert_eq!(parse_dropfiles_wide(&ansi), None);
        assert_eq!(parse_dropfiles_wide(&[0u8; 4]), None);
    }

    fn sample() -> Rgba {
        // 2×2：红、绿 / 蓝、半透明白
        Rgba {
            width: 2,
            height: 2,
            pixels: vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 128],
        }
    }

    #[test]
    fn dibv5_roundtrip_keeps_alpha() {
        let img = sample();
        let dib = encode_dibv5(&img);
        assert_eq!(dib.len(), 124 + 16);
        assert_eq!(decode_dib(&dib), Some(DibImage::Pixels(img)));
    }

    #[test]
    fn dib_24bit_bottom_up_with_padding() {
        // 1×2，24 位：每行 3 字节像素 + 1 字节填充；自下而上（第一行存的是底部像素）
        let mut d = Vec::new();
        d.extend_from_slice(&40u32.to_le_bytes());
        d.extend_from_slice(&1i32.to_le_bytes());
        d.extend_from_slice(&2i32.to_le_bytes());
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&24u16.to_le_bytes());
        d.extend_from_slice(&[0u8; 24]); // compression .. clr important
        d.extend_from_slice(&[255, 0, 0, 0]); // 底部：蓝
        d.extend_from_slice(&[0, 0, 255, 0]); // 顶部：红
        let Some(DibImage::Pixels(img)) = decode_dib(&d) else {
            panic!("decode failed")
        };
        assert_eq!(img.pixels, vec![255, 0, 0, 255, 0, 0, 255, 255]);
    }

    #[test]
    fn dib_32bit_zero_alpha_is_opaque_and_bitfields_after_header() {
        let mut d = Vec::new();
        d.extend_from_slice(&40u32.to_le_bytes());
        d.extend_from_slice(&1i32.to_le_bytes());
        d.extend_from_slice(&(-1i32).to_le_bytes()); // 自上而下
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&32u16.to_le_bytes());
        d.extend_from_slice(&3u32.to_le_bytes()); // BI_BITFIELDS，掩码跟在 40 字节头后
        d.extend_from_slice(&[0u8; 20]);
        d.extend_from_slice(&0x00FF_0000u32.to_le_bytes());
        d.extend_from_slice(&0x0000_FF00u32.to_le_bytes());
        d.extend_from_slice(&0x0000_00FFu32.to_le_bytes());
        d.extend_from_slice(&[0x10, 0x20, 0x30, 0x00]); // B G R X
        let Some(DibImage::Pixels(img)) = decode_dib(&d) else {
            panic!("decode failed")
        };
        assert_eq!(img.pixels, vec![0x30, 0x20, 0x10, 255]);
    }

    #[test]
    fn dib_32bit_bi_rgb_undefined_alpha() {
        let mut d = Vec::new();
        d.extend_from_slice(&40u32.to_le_bytes());
        d.extend_from_slice(&2i32.to_le_bytes());
        d.extend_from_slice(&(-1i32).to_le_bytes());
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&32u16.to_le_bytes());
        d.extend_from_slice(&[0u8; 24]); // BI_RGB
        d.extend_from_slice(&[1, 2, 3, 0, 4, 5, 6, 0]);
        let Some(DibImage::Pixels(img)) = decode_dib(&d) else {
            panic!("decode failed")
        };
        assert_eq!(img.pixels, vec![3, 2, 1, 255, 6, 5, 4, 255]);
        // 有任意非零 alpha 时按 alpha 使用
        let n = d.len();
        d[n - 1] = 0x80;
        let Some(DibImage::Pixels(img)) = decode_dib(&d) else {
            panic!("decode failed")
        };
        assert_eq!(img.pixels, vec![3, 2, 1, 0, 6, 5, 4, 0x80]);
    }

    #[test]
    fn dib_8bit_palette() {
        let mut d = Vec::new();
        d.extend_from_slice(&40u32.to_le_bytes());
        d.extend_from_slice(&2i32.to_le_bytes());
        d.extend_from_slice(&1i32.to_le_bytes());
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&8u16.to_le_bytes());
        d.extend_from_slice(&0u32.to_le_bytes());
        d.extend_from_slice(&[0u8; 12]);
        d.extend_from_slice(&2u32.to_le_bytes()); // clr used = 2
        d.extend_from_slice(&0u32.to_le_bytes());
        d.extend_from_slice(&[0, 0, 0, 0, 255, 255, 255, 0]); // 黑、白
        d.extend_from_slice(&[1, 0, 0, 0]); // 白、黑 + 填充
        let Some(DibImage::Pixels(img)) = decode_dib(&d) else {
            panic!("decode failed")
        };
        assert_eq!(img.pixels, vec![255, 255, 255, 255, 0, 0, 0, 255]);
    }

    /// 只有 40 字节头的 DIB（宽 w、高 h）
    fn dib_header(w: i32, h: i32) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&40u32.to_le_bytes());
        d.extend_from_slice(&w.to_le_bytes());
        d.extend_from_slice(&h.to_le_bytes());
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&32u16.to_le_bytes());
        d.extend_from_slice(&[0u8; 24]);
        d
    }

    #[test]
    fn dib_prefilter_uses_header_dimensions_only() {
        const MB: usize = 1024 * 1024;
        assert_eq!(dib_dimensions(&dib_header(3840, -2160)), Some((3840, 2160)));
        assert_eq!(dib_dimensions(&encode_dibv5(&sample())), Some((2, 2)));
        assert_eq!(dib_dimensions(&dib_header(0, 10)), None);
        assert_eq!(dib_dimensions(&dib_header(10, 0)), None);
        assert_eq!(dib_dimensions(&[1, 2, 3]), None);
        // 默认上限 20 MB：4K 截图（约 33 MB RGBA）照常解码；8K（约 133 MB RGBA）与异常尺寸在拷贝前就放弃
        assert!(dib_worth_decoding(&dib_header(3840, 2160), 20 * MB));
        assert!(dib_worth_decoding(&dib_header(5120, -2880), 20 * MB));
        assert!(!dib_worth_decoding(&dib_header(7680, 4320), 20 * MB));
        assert!(!dib_worth_decoding(&dib_header(i32::MAX, i32::MAX), usize::MAX));
        // 上限调小时同步收紧
        assert!(!dib_worth_decoding(&dib_header(1920, 1080), MB));
        assert!(dib_worth_decoding(&dib_header(1024, 1024), MB));
        assert!(!dib_worth_decoding(&[0u8; 8], 20 * MB));
    }

    #[test]
    fn dib_rejects_truncated_and_unsupported() {
        let img = sample();
        let dib = encode_dibv5(&img);
        assert_eq!(decode_dib(&dib[..dib.len() - 1]), None);
        let mut rle = dib.clone();
        rle[16] = 1; // BI_RLE8
        assert_eq!(decode_dib(&rle), None);
        assert_eq!(decode_dib(&[1, 2, 3]), None);
    }

    #[test]
    fn png_roundtrip_and_dimensions() {
        let img = sample();
        let png = encode_png(&img).unwrap();
        assert_eq!(png_dimensions(&png), Some((2, 2)));
        assert_eq!(decode_png(&png), Some(img));
        assert_eq!(png_dimensions(b"not a png"), None);
    }

    /// 按指定颜色类型 / 位深编码一张逐行扫描的 PNG
    fn png_of(w: u32, h: u32, color: png::ColorType, depth: png::BitDepth, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(color);
            enc.set_depth(depth);
            if color == png::ColorType::Indexed {
                enc.set_palette(vec![255, 0, 0, 0, 0, 255]);
            }
            let mut writer = enc.write_header().unwrap();
            writer.write_image_data(data).unwrap();
            writer.finish().unwrap();
        }
        out
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &b in bytes {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }

    /// 手工拼一张 Adam7 隔行扫描的 RGBA PNG（png 编码器不支持写隔行图）：IDAT 用不压缩的 deflate 块
    fn adam7_png(img: &Rgba) -> Vec<u8> {
        const PASSES: [(usize, usize, usize, usize); 7] = [
            (0, 0, 8, 8),
            (4, 0, 8, 8),
            (0, 4, 4, 8),
            (2, 0, 4, 4),
            (0, 2, 2, 4),
            (1, 0, 2, 2),
            (0, 1, 1, 2),
        ];
        let (w, h) = (img.width as usize, img.height as usize);
        let mut raw = Vec::new();
        for (x0, y0, dx, dy) in PASSES {
            let xs: Vec<usize> = (x0..w).step_by(dx).collect();
            if xs.is_empty() {
                continue;
            }
            for y in (y0..h).step_by(dy) {
                raw.push(0); // 过滤类型 None
                for &x in &xs {
                    raw.extend_from_slice(&img.pixels[(y * w + x) * 4..(y * w + x) * 4 + 4]);
                }
            }
        }
        let mut z = vec![0x78, 0x01, 0x01];
        z.extend_from_slice(&(raw.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(raw.len() as u16)).to_le_bytes());
        z.extend_from_slice(&raw);
        let (mut a, mut b) = (1u32, 0u32);
        for &v in &raw {
            a = (a + u32::from(v)) % 65521;
            b = (b + a) % 65521;
        }
        z.extend_from_slice(&((b << 16) | a).to_be_bytes());
        let mut out = PNG_SIGNATURE.to_vec();
        let mut chunk = |kind: &[u8], data: &[u8]| {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            let mut body = kind.to_vec();
            body.extend_from_slice(data);
            out.extend_from_slice(&body);
            out.extend_from_slice(&crc32(&body).to_be_bytes());
        };
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&img.width.to_be_bytes());
        ihdr.extend_from_slice(&img.height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 1]); // 8 位 RGBA，Adam7
        chunk(b"IHDR", &ihdr);
        chunk(b"IDAT", &z);
        chunk(b"IEND", &[]);
        out
    }

    /// 生产路径（PNG 直接写成 DIBV5）与参照实现（PNG → RGBA → DIBV5）逐字节一致
    fn assert_matches_reference(png: &[u8]) {
        let (w, h) = png_dimensions(png).unwrap();
        let mut out = vec![0xAAu8; dibv5_len(w, h).unwrap()];
        png_into_dibv5(png, &mut out).expect("decodable");
        assert_eq!(out, encode_dibv5(&decode_png(png).unwrap()));
        assert_eq!(decode_dib(&out), Some(DibImage::Pixels(decode_png(png).unwrap())));
    }

    #[test]
    fn png_into_dibv5_matches_reference() {
        // RGBA（偶数 / 奇数行数，覆盖逐行对调的中间行）
        assert_matches_reference(&encode_png(&sample()).unwrap());
        let tall = Rgba {
            width: 1,
            height: 3,
            pixels: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
        };
        assert_matches_reference(&encode_png(&tall).unwrap());
        // RGB、灰度 + alpha、灰度、调色板、16 位
        assert_matches_reference(&png_of(
            2,
            2,
            png::ColorType::Rgb,
            png::BitDepth::Eight,
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
        ));
        assert_matches_reference(&png_of(
            3,
            1,
            png::ColorType::GrayscaleAlpha,
            png::BitDepth::Eight,
            &[10, 200, 20, 100, 30, 0],
        ));
        assert_matches_reference(&png_of(
            3,
            2,
            png::ColorType::Grayscale,
            png::BitDepth::Eight,
            &[0, 64, 128, 192, 255, 7],
        ));
        assert_matches_reference(&png_of(2, 1, png::ColorType::Indexed, png::BitDepth::Eight, &[0, 1]));
        assert_matches_reference(&png_of(
            1,
            2,
            png::ColorType::Rgb,
            png::BitDepth::Sixteen,
            &[0xFF, 0x00, 0x80, 0x01, 0x00, 0xFF, 0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC],
        ));
        // Adam7 隔行扫描（5 × 3，各趟子图宽高不一）
        let interlaced = Rgba {
            width: 5,
            height: 3,
            pixels: (0..60).map(|v| (v * 4) as u8).collect(),
        };
        let png = adam7_png(&interlaced);
        assert_eq!(
            decode_png(&png).as_ref(),
            Some(&interlaced),
            "手工拼的隔行 PNG 本身应可解码"
        );
        assert_matches_reference(&png);
        // 长度不符、不是 PNG
        let png = encode_png(&sample()).unwrap();
        assert_eq!(png_into_dibv5(&png, &mut [0u8; 10]), None);
        assert_eq!(png_into_dibv5(b"not a png", &mut [0u8; 140]), None);
    }

    #[test]
    fn dib_budget_is_shared_by_read_and_write() {
        // 默认 20 MB 上限 × 4 = 80 MB RGBA：5K（5120 × 2880）以内附带 DIB，8K 只写 PNG
        let max = 20 * 1024 * 1024;
        assert!(dib_within_budget(5120, 2880, max));
        assert!(!dib_within_budget(7680, 4320, max));
        assert!(!dib_within_budget(u32::MAX, u32::MAX, usize::MAX));
        assert_eq!(dibv5_len(2, 2), Some(124 + 16));
        assert_eq!(dibv5_len(0, 2), None);
    }

    #[test]
    fn dib_with_embedded_png() {
        let png = encode_png(&sample()).unwrap();
        let mut d = Vec::new();
        d.extend_from_slice(&40u32.to_le_bytes());
        d.extend_from_slice(&2i32.to_le_bytes());
        d.extend_from_slice(&2i32.to_le_bytes());
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&0u16.to_le_bytes());
        d.extend_from_slice(&5u32.to_le_bytes()); // BI_PNG
        d.extend_from_slice(&(png.len() as u32).to_le_bytes());
        d.extend_from_slice(&[0u8; 16]);
        d.extend_from_slice(&png);
        assert_eq!(decode_dib(&d), Some(DibImage::Png(png)));
    }

    #[test]
    fn marker_roundtrip() {
        for m in [
            Marker {
                kind: MarkerKind::Write,
                private: false,
            },
            Marker {
                kind: MarkerKind::Restore,
                private: true,
            },
        ] {
            assert_eq!(decode_marker(&encode_marker(m)), m);
        }
        assert_eq!(decode_marker(&[]).kind, MarkerKind::Write);
    }
}
