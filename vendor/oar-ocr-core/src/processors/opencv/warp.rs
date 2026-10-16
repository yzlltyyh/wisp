//! `cv::getPerspectiveTransform` and `cv::warpPerspective(..., INTER_CUBIC, BORDER_REPLICATE)` for
//! 8-bit RGB images.
//!
//! OpenCV's warp quantises every source coordinate to 1/32 pixel (`INTER_BITS = 5`) and samples a
//! 4×4 neighbourhood with a precomputed 15-bit fixed-point bicubic table (`A = -0.75`, each
//! 2-D kernel adjusted to sum to exactly 32768). A floating-point Catmull-Rom (`A = -0.5`)
//! sampler, or one that pre-crops the bounding box (so that border replication happens at the
//! crop edge instead of reading the real neighbours), produces visibly different recognition
//! inputs.

use super::cv_round;
use image::RgbImage;
use std::sync::OnceLock;

const INTER_BITS: i32 = 5;
const INTER_TAB_SIZE: i32 = 1 << INTER_BITS;
const INTER_REMAP_COEF_BITS: i32 = 15;
const INTER_REMAP_COEF_SCALE: i32 = 1 << INTER_REMAP_COEF_BITS;

/// `cv::getPerspectiveTransform(src, dst)` (`DECOMP_LU`), row-major 3×3.
///
/// Returns `None` when the system is singular.
pub fn get_perspective_transform(src: &[[f32; 2]; 4], dst: &[[f32; 2]; 4]) -> Option<[f64; 9]> {
    let mut a = [[0f64; 8]; 8];
    let mut b = [0f64; 8];
    for i in 0..4 {
        let (sx, sy) = (src[i][0], src[i][1]);
        let (dx, dy) = (dst[i][0], dst[i][1]);
        a[i][0] = sx as f64;
        a[i + 4][3] = sx as f64;
        a[i][1] = sy as f64;
        a[i + 4][4] = sy as f64;
        a[i][2] = 1.0;
        a[i + 4][5] = 1.0;
        // `-src[i].x*dst[i].x` is a float product in OpenCV
        a[i][6] = (-sx * dx) as f64;
        a[i][7] = (-sy * dx) as f64;
        a[i + 4][6] = (-sx * dy) as f64;
        a[i + 4][7] = (-sy * dy) as f64;
        b[i] = dx as f64;
        b[i + 4] = dy as f64;
    }
    lu_solve(&mut a, &mut b)?;
    Some([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], 1.0])
}

/// `hal::LU` (Gaussian elimination with partial pivoting) for one right-hand side.
fn lu_solve(a: &mut [[f64; 8]; 8], b: &mut [f64; 8]) -> Option<()> {
    let m = 8;
    let eps = f64::EPSILON * 100.0;
    for i in 0..m {
        let mut k = i;
        for j in i + 1..m {
            if a[j][i].abs() > a[k][i].abs() {
                k = j;
            }
        }
        if a[k][i].abs() < eps {
            return None;
        }
        if k != i {
            a.swap(i, k);
            b.swap(i, k);
        }
        let d = -1.0 / a[i][i];
        for j in i + 1..m {
            let alpha = a[j][i] * d;
            for c in i + 1..m {
                a[j][c] += alpha * a[i][c];
            }
            b[j] += alpha * b[i];
        }
    }
    for i in (0..m).rev() {
        let mut s = b[i];
        for k in i + 1..m {
            s -= a[i][k] * b[k];
        }
        b[i] = s / a[i][i];
    }
    Some(())
}

/// `cv::invert(DECOMP_LU)` of a 3×3 matrix (closed form, as OpenCV does for `n <= 3`).
pub fn invert_3x3(m: &[f64; 9]) -> Option<[f64; 9]> {
    let s = |r: usize, c: usize| m[r * 3 + c];
    let det = s(0, 0) * (s(1, 1) * s(2, 2) - s(1, 2) * s(2, 1))
        - s(0, 1) * (s(1, 0) * s(2, 2) - s(1, 2) * s(2, 0))
        + s(0, 2) * (s(1, 0) * s(2, 1) - s(1, 1) * s(2, 0));
    if det == 0.0 {
        return None;
    }
    let d = 1.0 / det;
    Some([
        (s(1, 1) * s(2, 2) - s(1, 2) * s(2, 1)) * d,
        (s(0, 2) * s(2, 1) - s(0, 1) * s(2, 2)) * d,
        (s(0, 1) * s(1, 2) - s(0, 2) * s(1, 1)) * d,
        (s(1, 2) * s(2, 0) - s(1, 0) * s(2, 2)) * d,
        (s(0, 0) * s(2, 2) - s(0, 2) * s(2, 0)) * d,
        (s(0, 2) * s(1, 0) - s(0, 0) * s(1, 2)) * d,
        (s(1, 0) * s(2, 1) - s(1, 1) * s(2, 0)) * d,
        (s(0, 1) * s(2, 0) - s(0, 0) * s(2, 1)) * d,
        (s(0, 0) * s(1, 1) - s(0, 1) * s(1, 0)) * d,
    ])
}

/// `interpolateCubic` (`A = -0.75`).
fn interpolate_cubic(x: f32) -> [f32; 4] {
    const A: f32 = -0.75;
    let c0 = ((A * (x + 1.0) - 5.0 * A) * (x + 1.0) + 8.0 * A) * (x + 1.0) - 4.0 * A;
    let c1 = ((A + 2.0) * x - (A + 3.0)) * x * x + 1.0;
    let c2 = ((A + 2.0) * (1.0 - x) - (A + 3.0)) * (1.0 - x) * (1.0 - x) + 1.0;
    let c3 = 1.0 - c0 - c1 - c2;
    [c0, c1, c2, c3]
}

/// `initInterTab2D(INTER_CUBIC, fixpt = true)`: 32×32 fractional positions × 16 coefficients.
fn bicubic_tab() -> &'static [[i32; 16]] {
    static TAB: OnceLock<Vec<[i32; 16]>> = OnceLock::new();
    TAB.get_or_init(|| {
        let scale = 1.0f32 / INTER_TAB_SIZE as f32;
        let tab1d: Vec<[f32; 4]> = (0..INTER_TAB_SIZE)
            .map(|i| interpolate_cubic(i as f32 * scale))
            .collect();
        let mut out = Vec::with_capacity((INTER_TAB_SIZE * INTER_TAB_SIZE) as usize);
        for i in 0..INTER_TAB_SIZE as usize {
            for j in 0..INTER_TAB_SIZE as usize {
                let mut itab = [0i32; 16];
                let mut isum = 0;
                for k1 in 0..4 {
                    let vy = tab1d[i][k1];
                    for k2 in 0..4 {
                        let v = vy * tab1d[j][k2];
                        let q = (v * INTER_REMAP_COEF_SCALE as f32).round_ties_even() as i32;
                        let q = q.clamp(i16::MIN as i32, i16::MAX as i32);
                        itab[k1 * 4 + k2] = q;
                        isum += q;
                    }
                }
                if isum != INTER_REMAP_COEF_SCALE {
                    let diff = isum - INTER_REMAP_COEF_SCALE;
                    let (mut mk1, mut mk2, mut big1, mut big2) = (2usize, 2usize, 2usize, 2usize);
                    for k1 in 2..4 {
                        for k2 in 2..4 {
                            if itab[k1 * 4 + k2] < itab[mk1 * 4 + mk2] {
                                mk1 = k1;
                                mk2 = k2;
                            } else if itab[k1 * 4 + k2] > itab[big1 * 4 + big2] {
                                big1 = k1;
                                big2 = k2;
                            }
                        }
                    }
                    if diff < 0 {
                        itab[big1 * 4 + big2] -= diff;
                    } else {
                        itab[mk1 * 4 + mk2] -= diff;
                    }
                }
                out.push(itab);
            }
        }
        out
    })
}

/// `cv::warpPerspective(src, M, (dst_w, dst_h), flags = INTER_CUBIC, borderMode = BORDER_REPLICATE)`,
/// with `m` the forward (source → destination) transform as returned by
/// [`get_perspective_transform`].
///
/// Returns `None` if `m` is singular or a dimension is zero.
pub fn warp_perspective_cubic_replicate(
    src: &RgbImage,
    m: &[f64; 9],
    dst_w: u32,
    dst_h: u32,
) -> Option<RgbImage> {
    if dst_w == 0 || dst_h == 0 || src.width() == 0 || src.height() == 0 {
        return None;
    }
    let mi = invert_3x3(m)?;
    let tab = bicubic_tab();
    let (sw, sh) = (src.width() as i64, src.height() as i64);
    let s = src.as_raw();
    let stride = sw as usize * 3;
    let mut dst = RgbImage::new(dst_w, dst_h);
    let (width, height) = (dst_w as usize, dst_h as usize);
    let out = dst.as_mut();

    // Same block decomposition as `WarpPerspectiveInvoker`: the row start values are computed per
    // block, and on 64-bit-SIMD builds the first `bw / 16 * 16` columns of a block row use fused
    // multiply-adds (`v_muladd`) while the rest use the scalar expression.
    const BLOCK_SZ: usize = 32;
    let mut bh0 = (BLOCK_SZ / 2).min(height);
    let bw0 = (BLOCK_SZ * BLOCK_SZ / bh0).min(width);
    bh0 = (BLOCK_SZ * BLOCK_SZ / bw0).min(height);
    let its = INTER_TAB_SIZE as f64;
    let clamp_int = |v: f64| v.clamp(i32::MIN as f64, i32::MAX as f64);

    let mut y = 0;
    while y < height {
        let mut x = 0;
        while x < width {
            let bw = bw0.min(width - x);
            let bh = bh0.min(height - y);
            for y1 in 0..bh {
                let yy = (y + y1) as f64;
                let xf = x as f64;
                let x0 = mi[0] * xf + mi[1] * yy + mi[2];
                let y0 = mi[3] * xf + mi[4] * yy + mi[5];
                let w0 = mi[6] * xf + mi[7] * yy + mi[8];
                let simd_end = if bw >= 16 { bw / 16 * 16 } else { 0 };
                for x1 in 0..bw {
                    let xx1 = x1 as f64;
                    let (fx, fy) = if x1 < simd_end {
                        let w = mi[6].mul_add(xx1, w0);
                        let w = if w != 0.0 { its / w } else { 0.0 };
                        (
                            clamp_int(mi[0].mul_add(xx1, x0) * w),
                            clamp_int(mi[3].mul_add(xx1, y0) * w),
                        )
                    } else {
                        let w = w0 + mi[6] * xx1;
                        let w = if w != 0.0 { its / w } else { 0.0 };
                        (
                            clamp_int((x0 + mi[0] * xx1) * w),
                            clamp_int((y0 + mi[3] * xx1) * w),
                        )
                    };
                    let xi = cv_round(fx) as i32;
                    let yi = cv_round(fy) as i32;
                    let sx =
                        ((xi >> INTER_BITS).clamp(i16::MIN as i32, i16::MAX as i32) - 1) as i64;
                    let sy =
                        ((yi >> INTER_BITS).clamp(i16::MIN as i32, i16::MAX as i32) - 1) as i64;
                    let fxy = ((yi & (INTER_TAB_SIZE - 1)) * INTER_TAB_SIZE
                        + (xi & (INTER_TAB_SIZE - 1))) as usize;
                    let w = &tab[fxy];
                    let xs: [usize; 4] =
                        std::array::from_fn(|i| (sx + i as i64).clamp(0, sw - 1) as usize * 3);
                    let ys: [usize; 4] =
                        std::array::from_fn(|i| (sy + i as i64).clamp(0, sh - 1) as usize * stride);
                    let o = ((y + y1) * width + x + x1) * 3;
                    for k in 0..3 {
                        let mut sum = 0i32;
                        for r in 0..4 {
                            let row = ys[r] + k;
                            sum += s[row + xs[0]] as i32 * w[r * 4]
                                + s[row + xs[1]] as i32 * w[r * 4 + 1]
                                + s[row + xs[2]] as i32 * w[r * 4 + 2]
                                + s[row + xs[3]] as i32 * w[r * 4 + 3];
                        }
                        out[o + k] = ((sum + (1 << (INTER_REMAP_COEF_BITS - 1)))
                            >> INTER_REMAP_COEF_BITS)
                            .clamp(0, 255) as u8;
                    }
                }
            }
            x += bw0;
        }
        y += bh0;
    }
    Some(dst)
}
