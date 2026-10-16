//! `cv2.resize(src, (w, h))` with the default `INTER_LINEAR` on 8-bit images.
//!
//! OpenCV's 8-bit bilinear resize is not a plain floating-point bilinear filter: sample positions
//! use the half-pixel convention computed in `float`, weights are quantised to 11 bits
//! (`INTER_RESIZE_COEF_BITS`), the horizontal pass keeps 32-bit sums and the vertical pass
//! rescales them with a specific shift/round sequence. Exact 2× downscaling switches to the
//! `INTER_AREA` fast path. image-rs' `Triangle` filter widens its kernel when downscaling
//! (anti-aliasing) and therefore differs by several gray levels, which is enough to move DB
//! boxes and to change recognition results.

use super::saturate_i16;
use image::RgbImage;

const COEF_BITS: i32 = 11;
const COEF_SCALE: i32 = 1 << COEF_BITS;

/// Resizes an interleaved 8-bit image with `cn` channels to `dw × dh`, bit-exact with
/// `cv::resize(..., INTER_LINEAR)` (generic code path, as used for 3-channel images).
///
/// # Panics
///
/// Panics if `src.len() != sw * sh * cn` or any dimension is zero.
pub fn resize_linear(src: &[u8], sw: usize, sh: usize, cn: usize, dw: usize, dh: usize) -> Vec<u8> {
    assert!(
        sw > 0 && sh > 0 && dw > 0 && dh > 0 && cn > 0,
        "empty image"
    );
    assert_eq!(
        src.len(),
        sw * sh * cn,
        "buffer size does not match the dimensions"
    );
    if sw == dw && sh == dh {
        return src.to_vec();
    }

    let inv_scale_x = dw as f64 / sw as f64;
    let inv_scale_y = dh as f64 / sh as f64;
    let scale_x = 1.0 / inv_scale_x;
    let scale_y = 1.0 / inv_scale_y;
    let iscale_x = scale_x.round_ties_even() as i64;
    let iscale_y = scale_y.round_ties_even() as i64;
    let is_area_fast = (scale_x - iscale_x as f64).abs() < f64::EPSILON
        && (scale_y - iscale_y as f64).abs() < f64::EPSILON;
    // "in case of scale_x && scale_y is equal to 2, INTER_AREA (fast) also is equal to INTER_LINEAR"
    if is_area_fast && iscale_x == 2 && iscale_y == 2 {
        return resize_area_fast_2x(src, sw, cn, dw, dh);
    }

    // Horizontal tables (`xofs` in elements, two 11-bit weights per element).
    let width = dw * cn;
    let mut xofs = vec![0usize; width];
    let mut alpha = vec![0i32; width * 2];
    let mut xmax = dw;
    for dx in 0..dw {
        let mut fx = ((dx as f64 + 0.5) * scale_x - 0.5) as f32;
        let mut sx = fx.floor() as i64;
        fx -= sx as f32;
        if sx < 0 {
            fx = 0.0;
            sx = 0;
        }
        if sx + 1 >= sw as i64 {
            xmax = xmax.min(dx);
            if sx >= sw as i64 - 1 {
                fx = 0.0;
                sx = sw as i64 - 1;
            }
        }
        let a0 = saturate_i16((1.0 - fx) * COEF_SCALE as f32) as i32;
        let a1 = saturate_i16(fx * COEF_SCALE as f32) as i32;
        for k in 0..cn {
            let e = dx * cn + k;
            xofs[e] = sx as usize * cn + k;
            alpha[e * 2] = a0;
            alpha[e * 2 + 1] = a1;
        }
    }
    let xmax = xmax * cn;

    // Vertical tables. Unlike x, OpenCV does not clamp the fraction for y; out-of-range rows are
    // clipped when they are fetched.
    let mut yofs = vec![0i64; dh];
    let mut beta = vec![0i32; dh * 2];
    for dy in 0..dh {
        let mut fy = ((dy as f64 + 0.5) * scale_y - 0.5) as f32;
        let sy = fy.floor() as i64;
        fy -= sy as f32;
        yofs[dy] = sy;
        beta[dy * 2] = saturate_i16((1.0 - fy) * COEF_SCALE as f32) as i32;
        beta[dy * 2 + 1] = saturate_i16(fy * COEF_SCALE as f32) as i32;
    }

    let row_stride = sw * cn;
    let hresize = |sy: usize, out: &mut [i32]| {
        let s = &src[sy * row_stride..(sy + 1) * row_stride];
        for dx in 0..xmax {
            let sx = xofs[dx];
            out[dx] = s[sx] as i32 * alpha[dx * 2] + s[sx + cn] as i32 * alpha[dx * 2 + 1];
        }
        for dx in xmax..width {
            out[dx] = s[xofs[dx]] as i32 * COEF_SCALE;
        }
    };
    let clip = |y: i64| -> usize { y.clamp(0, sh as i64 - 1) as usize };

    let mut dst = vec![0u8; width * dh];
    let mut rows = [vec![0i32; width], vec![0i32; width]];
    let mut cached: [Option<usize>; 2] = [None, None];
    for dy in 0..dh {
        let sy0 = clip(yofs[dy]);
        let sy1 = clip(yofs[dy] + 1);
        // Reuse horizontally filtered rows while walking down (pure optimisation: every row is
        // filtered with the same per-row arithmetic).
        if cached[0] != Some(sy0) {
            if cached[1] == Some(sy0) {
                rows.swap(0, 1);
                cached.swap(0, 1);
            } else {
                hresize(sy0, &mut rows[0]);
                cached[0] = Some(sy0);
            }
        }
        if cached[1] != Some(sy1) {
            if sy1 == sy0 {
                let (a, b) = rows.split_at_mut(1);
                b[0].copy_from_slice(&a[0]);
            } else {
                hresize(sy1, &mut rows[1]);
            }
            cached[1] = Some(sy1);
        }
        let (b0, b1) = (beta[dy * 2], beta[dy * 2 + 1]);
        let out = &mut dst[dy * width..(dy + 1) * width];
        for x in 0..width {
            let v = ((b0 * (rows[0][x] >> 4)) >> 16) + ((b1 * (rows[1][x] >> 4)) >> 16) + 2;
            out[x] = (v >> 2).clamp(0, 255) as u8;
        }
    }
    dst
}

/// `resizeAreaFast_` for an exact 2× downscale: the rounded mean of each 2×2 block.
fn resize_area_fast_2x(src: &[u8], sw: usize, cn: usize, dw: usize, dh: usize) -> Vec<u8> {
    let stride = sw * cn;
    let mut dst = vec![0u8; dw * dh * cn];
    for dy in 0..dh {
        let s0 = &src[dy * 2 * stride..];
        let s1 = &src[(dy * 2 + 1) * stride..];
        let out = &mut dst[dy * dw * cn..(dy + 1) * dw * cn];
        for dx in 0..dw {
            for k in 0..cn {
                let i = dx * 2 * cn + k;
                let sum = s0[i] as u32 + s0[i + cn] as u32 + s1[i] as u32 + s1[i + cn] as u32;
                out[dx * cn + k] = ((sum + 2) >> 2) as u8;
            }
        }
    }
    dst
}

/// [`resize_linear`] for an RGB image.
pub fn resize_linear_rgb(img: &RgbImage, width: u32, height: u32) -> RgbImage {
    if img.width() == width && img.height() == height {
        return img.clone();
    }
    let data = resize_linear(
        img.as_raw(),
        img.width() as usize,
        img.height() as usize,
        3,
        width as usize,
        height as usize,
    );
    RgbImage::from_raw(width, height, data).expect("resize_linear returns width * height * 3 bytes")
}
