//! X11 ZPixmap → 紧密排列、alpha 恒为 255 的 RGBA8。
//!
//! 常见情形（depth 24/32、32 bpp、小端、R/G/B 掩码 0xff0000/0xff00/0xff，即内存顺序 B G R X）走逐像素定长块的
//! 快速路径；其余格式（大端服务器、16 bpp 的 565、24 bpp 紧凑像素、非常规掩码）走按掩码解码的通用路径。
//! 第 4 字节是未定义的填充（常为 0），一律写 255，避免冻结帧在 WebView 里变成全透明。

/// ZPixmap 的像素格式（来自连接 setup 的 pixmap_formats、image_byte_order 与根窗口 visual 的掩码）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PixelFormat {
    /// 每像素位数：16 / 24 / 32
    pub bits_per_pixel: u8,
    /// 服务器的图像字节序是否为大端（MSBFirst）
    pub msb_first: bool,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
}

impl PixelFormat {
    /// 是否是能解码的格式
    pub(crate) fn supported(&self) -> bool {
        matches!(self.bits_per_pixel, 16 | 24 | 32) && self.red_mask != 0 && self.green_mask != 0 && self.blue_mask != 0
    }
}

/// 把带行跨度的 ZPixmap 数据转换成 RGBA8。`src` 长度不足时缺失的行保持黑色（只在服务器返回的数据被截断时发生）
pub(crate) fn to_rgba(src: &[u8], width: u32, height: u32, stride: usize, fmt: PixelFormat) -> Vec<u8> {
    let w = width as usize;
    let row_out = w * 4;
    let bpp = usize::from(fmt.bits_per_pixel / 8);
    let row_in = w * bpp;
    let mut dst = crate::pixel::opaque_black(width, height);
    if row_out == 0 {
        return dst;
    }
    let fast = !fmt.msb_first && bpp == 4 && fmt.green_mask == 0x00ff00;
    let bgrx = fast && fmt.red_mask == 0xff0000 && fmt.blue_mask == 0x0000ff;
    let rgbx = fast && fmt.red_mask == 0x0000ff && fmt.blue_mask == 0xff0000;
    let red = Channel::new(fmt.red_mask);
    let green = Channel::new(fmt.green_mask);
    let blue = Channel::new(fmt.blue_mask);

    for (y, out) in dst.chunks_exact_mut(row_out).enumerate() {
        let start = y * stride;
        let Some(row) = src.get(start..start + row_in) else {
            break;
        };
        if bgrx {
            for (d, s) in out.chunks_exact_mut(4).zip(row.chunks_exact(4)) {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = 255;
            }
        } else if rgbx {
            for (d, s) in out.chunks_exact_mut(4).zip(row.chunks_exact(4)) {
                d[..3].copy_from_slice(&s[..3]);
                d[3] = 255;
            }
        } else {
            for (d, s) in out.chunks_exact_mut(4).zip(row.chunks_exact(bpp)) {
                let v = read_pixel(s, fmt.msb_first);
                d[0] = red.extract(v);
                d[1] = green.extract(v);
                d[2] = blue.extract(v);
                d[3] = 255;
            }
        }
    }
    dst
}

/// ZPixmap 一行的字节数：按 scanline_pad（位）对齐
pub(crate) fn stride(width: u32, bits_per_pixel: u8, scanline_pad: u8) -> usize {
    let bits = width as usize * usize::from(bits_per_pixel);
    let pad = usize::from(scanline_pad.max(8));
    bits.div_ceil(pad) * pad / 8
}

/// 按字节序读出一个像素值（2 / 3 / 4 字节）
fn read_pixel(bytes: &[u8], msb_first: bool) -> u32 {
    if msb_first {
        bytes.iter().fold(0u32, |v, &b| (v << 8) | u32::from(b))
    } else {
        bytes.iter().rev().fold(0u32, |v, &b| (v << 8) | u32::from(b))
    }
}

/// 一个颜色通道：掩码的位移与位数，以及缩放到 8 位的方式
#[derive(Debug, Clone, Copy)]
struct Channel {
    mask: u32,
    shift: u32,
    bits: u32,
}

impl Channel {
    fn new(mask: u32) -> Channel {
        let shift = if mask == 0 { 0 } else { mask.trailing_zeros() };
        Channel {
            mask,
            shift,
            bits: (mask >> shift).count_ones(),
        }
    }

    fn extract(&self, v: u32) -> u8 {
        if self.bits == 0 {
            return 0;
        }
        let raw = (v & self.mask) >> self.shift;
        if self.bits >= 8 {
            (raw >> (self.bits - 8)) as u8
        } else {
            // 位数不足 8 时按比例放大，使最大值映射到 255（565 的 31 → 255）
            let max = (1u32 << self.bits) - 1;
            ((raw * 255 + max / 2) / max) as u8
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BGRX: PixelFormat = PixelFormat {
        bits_per_pixel: 32,
        msb_first: false,
        red_mask: 0xff0000,
        green_mask: 0x00ff00,
        blue_mask: 0x0000ff,
    };

    #[test]
    fn little_endian_bgrx_with_padding() {
        // 2×2，每行末尾 4 字节填充（stride = 12）
        let src = [
            1, 2, 3, 0, 4, 5, 6, 0, 99, 99, 99, 99, //
            7, 8, 9, 0, 10, 11, 12, 0, 99, 99, 99, 99,
        ];
        let out = to_rgba(&src, 2, 2, 12, BGRX);
        assert_eq!(out, vec![3, 2, 1, 255, 6, 5, 4, 255, 9, 8, 7, 255, 12, 11, 10, 255]);
    }

    #[test]
    fn rgbx_and_big_endian_xrgb() {
        let rgbx = PixelFormat {
            red_mask: 0x0000ff,
            blue_mask: 0xff0000,
            ..BGRX
        };
        assert_eq!(to_rgba(&[1, 2, 3, 0], 1, 1, 4, rgbx), vec![1, 2, 3, 255]);
        // 大端服务器：内存顺序 X R G B
        let xrgb = PixelFormat {
            msb_first: true,
            ..BGRX
        };
        assert_eq!(to_rgba(&[0, 10, 20, 30], 1, 1, 4, xrgb), vec![10, 20, 30, 255]);
    }

    #[test]
    fn rgb565_scales_to_full_range() {
        let fmt = PixelFormat {
            bits_per_pixel: 16,
            msb_first: false,
            red_mask: 0xf800,
            green_mask: 0x07e0,
            blue_mask: 0x001f,
        };
        // 白 0xffff、纯红 0xf800（小端存储）
        let src = [0xff, 0xff, 0x00, 0xf8];
        assert_eq!(to_rgba(&src, 2, 1, 4, fmt), vec![255, 255, 255, 255, 255, 0, 0, 255]);
    }

    #[test]
    fn packed_24bpp() {
        let fmt = PixelFormat {
            bits_per_pixel: 24,
            ..BGRX
        };
        // 2 像素紧凑存储，行按 32 位对齐：6 字节 → 8 字节
        assert_eq!(stride(2, 24, 32), 8);
        let src = [1, 2, 3, 4, 5, 6, 0, 0];
        assert_eq!(to_rgba(&src, 2, 1, 8, fmt), vec![3, 2, 1, 255, 6, 5, 4, 255]);
    }

    #[test]
    fn truncated_data_leaves_black_rows() {
        let out = to_rgba(&[1, 2, 3, 0], 1, 2, 4, BGRX);
        assert_eq!(out, vec![3, 2, 1, 255, 0, 0, 0, 255]);
    }

    #[test]
    fn stride_and_support() {
        assert_eq!(stride(1920, 32, 32), 7680);
        assert_eq!(stride(3, 16, 32), 8);
        assert!(BGRX.supported());
        assert!(
            !PixelFormat {
                bits_per_pixel: 8,
                ..BGRX
            }
            .supported()
        );
    }
}
