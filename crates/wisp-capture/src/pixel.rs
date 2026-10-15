//! 像素级工具：强制不透明、1/2 降采样、RGBA → RGB（X11 ZPixmap 的解码见 `zpixmap.rs`）。
//!
//! 全部是对字节切片的纯函数，三端共用；热点循环写成逐像素的定长块，便于编译器自动向量化。

/// 把 alpha 通道全部写成 255（屏幕捕获结果视为不透明）
pub(crate) fn force_opaque(rgba: &mut [u8]) {
    for px in rgba.chunks_exact_mut(4) {
        px[3] = 255;
    }
}

/// 不透明黑色的 RGBA 缓冲（像素存储缺失时的兜底，保证长度与尺寸一致）
pub(crate) fn opaque_black(width: u32, height: u32) -> Vec<u8> {
    let n = (width as usize) * (height as usize);
    let mut v = vec![0u8; n * 4];
    force_opaque(&mut v);
    v
}

/// 是否所有像素的 alpha 都是 255
pub(crate) fn is_opaque(rgba: &[u8]) -> bool {
    rgba.chunks_exact(4).all(|px| px[3] == 255)
}

/// RGBA8 → RGB8（丢弃 alpha）
pub(crate) fn rgba_to_rgb(rgba: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; rgba.len() / 4 * 3];
    for (d, s) in out.chunks_exact_mut(3).zip(rgba.chunks_exact(4)) {
        d.copy_from_slice(&s[..3]);
    }
    out
}

/// 1/2 降采样后的尺寸：向上取整，奇数边的最后一列 / 行单独成块，不丢内容
pub(crate) fn half_dims(width: u32, height: u32) -> (u32, u32) {
    (width.div_ceil(2).max(1), height.div_ceil(2).max(1))
}

/// 2×2 盒式平均的 1/2 降采样（长边减半，宽高同比）。
///
/// 奇数边界处重复最后一列 / 行参与平均；`src` 长度必须等于 `width * height * 4`，否则返回同尺寸的黑帧。
pub(crate) fn downsample_half(src: &[u8], width: u32, height: u32) -> (u32, u32, Vec<u8>) {
    let (hw, hh) = half_dims(width, height);
    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 || src.len() != w * h * 4 {
        return (hw, hh, opaque_black(hw, hh));
    }
    let row_bytes = w * 4;
    let mut dst = vec![0u8; (hw as usize) * (hh as usize) * 4];
    for (oy, out_row) in dst.chunks_exact_mut(hw as usize * 4).enumerate() {
        let y0 = oy * 2;
        let y1 = (y0 + 1).min(h - 1);
        let r0 = &src[y0 * row_bytes..(y0 + 1) * row_bytes];
        let r1 = &src[y1 * row_bytes..(y1 + 1) * row_bytes];
        for (ox, d) in out_row.chunks_exact_mut(4).enumerate() {
            let x0 = ox * 2 * 4;
            let x1 = ((ox * 2 + 1).min(w - 1)) * 4;
            for c in 0..4 {
                let sum = u16::from(r0[x0 + c]) + u16::from(r0[x1 + c]) + u16::from(r1[x0 + c]) + u16::from(r1[x1 + c]);
                d[c] = ((sum + 2) >> 2) as u8;
            }
        }
    }
    (hw, hh, dst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_averages_2x2_blocks_and_rounds() {
        // 2×2 → 1×1：每个通道取四个像素的平均（四舍五入）
        let src = [
            0, 10, 100, 255, 1, 11, 101, 255, //
            2, 12, 102, 255, 3, 13, 104, 255,
        ];
        let (w, h, out) = downsample_half(&src, 2, 2);
        assert_eq!((w, h), (1, 1));
        assert_eq!(out, vec![2, 12, 102, 255]); // (0+1+2+3+2)/4=2，(100+101+102+104+2)/4=102
    }

    #[test]
    fn half_handles_odd_sizes_without_dropping_edges() {
        // 3×1：右侧单独一列与自己平均
        let src = [10, 0, 0, 255, 20, 0, 0, 255, 200, 0, 0, 255];
        let (w, h, out) = downsample_half(&src, 3, 1);
        assert_eq!((w, h), (2, 1));
        assert_eq!(out, vec![15, 0, 0, 255, 200, 0, 0, 255]);
        assert_eq!(half_dims(1, 1), (1, 1));
        assert_eq!(half_dims(5120, 2880), (2560, 1440));
    }

    #[test]
    fn half_of_mismatched_buffer_is_black_not_panic() {
        let (w, h, out) = downsample_half(&[1, 2, 3], 4, 4);
        assert_eq!((w, h), (2, 2));
        assert_eq!(out.len(), 16);
    }

    #[test]
    fn opacity_and_rgb_helpers() {
        let mut px = vec![1, 2, 3, 0, 4, 5, 6, 128];
        assert!(!is_opaque(&px));
        assert_eq!(rgba_to_rgb(&px), vec![1, 2, 3, 4, 5, 6]);
        force_opaque(&mut px);
        assert!(is_opaque(&px));
        assert_eq!(opaque_black(1, 1), vec![0, 0, 0, 255]);
    }
}
