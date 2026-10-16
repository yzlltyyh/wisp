//! Bit-compatible ports of the OpenCV (and pyclipper) routines that the PaddleOCR / PaddleX
//! Python pipeline relies on.
//!
//! Wisp patch: PaddleOCR's reference outputs come from `cv2.resize` (INTER_LINEAR),
//! `cv2.warpPerspective` (INTER_CUBIC + BORDER_REPLICATE), `cv2.minAreaRect` / `cv2.boxPoints`,
//! `cv2.fillPoly` + `cv2.mean` (DB `box_score_fast`) and `pyclipper` (DB `unclip`). Their
//! fixed-point arithmetic, coordinate conventions and tie-breaking decide whether a box lands one
//! pixel left or right, and whether a borderline region passes `box_thresh` at all. These modules
//! reproduce the generic (non-IPP, non-HAL) code paths of OpenCV 4.10 and Clipper 6.4.2 step by
//! step, so that the Rust pipeline produces the same boxes and the same recognition inputs as the
//! official Python pipeline.
//!
//! Ported from OpenCV 4.10.0 (<https://github.com/opencv/opencv>: `modules/imgproc/src/`
//! `resize.cpp`, `imgwarp.cpp`, `convhull.cpp`, `rotcalipers.cpp`, `drawing.cpp`, `shapedescr.cpp`
//! and `modules/core/src/types.cpp`, whose headers carry the license below) and from Clipper 6.4.2
//! (Boost Software License 1.0, see `clipper.rs`).
//!
//! > License Agreement For Open Source Computer Vision Library
//! >
//! > Copyright (C) 2000-2008, 2017, Intel Corporation, all rights reserved.
//! > Copyright (C) 2009, Willow Garage Inc., all rights reserved.
//! > Copyright (C) 2013, OpenCV Foundation, all rights reserved.
//! > Copyright (C) 2014-2015, Itseez Inc., all rights reserved.
//! > Third party copyrights are property of their respective owners.
//! >
//! > Redistribution and use in source and binary forms, with or without modification,
//! > are permitted provided that the following conditions are met:
//! >
//! >   * Redistribution's of source code must retain the above copyright notice,
//! >     this list of conditions and the following disclaimer.
//! >
//! >   * Redistribution's in binary form must reproduce the above copyright notice,
//! >     this list of conditions and the following disclaimer in the documentation
//! >     and/or other materials provided with the distribution.
//! >
//! >   * The name of the copyright holders may not be used to endorse or promote products
//! >     derived from this software without specific prior written permission.
//! >
//! > This software is provided by the copyright holders and contributors "as is" and
//! > any express or implied warranties, including, but not limited to, the implied
//! > warranties of merchantability and fitness for a particular purpose are disclaimed.
//! > In no event shall the Intel Corporation or contributors be liable for any direct,
//! > indirect, incidental, special, exemplary, or consequential damages
//! > (including, but not limited to, procurement of substitute goods or services;
//! > loss of use, data, or profits; or business interruption) however caused
//! > and on any theory of liability, whether in contract, strict liability,
//! > or tort (including negligence or otherwise) arising in any way out of
//! > the use of this software, even if advised of the possibility of such damage.
//!
//! The bit-exactness is checked against reference outputs of the Python functions in Wisp's
//! `crates/wisp-local/tests/ocr_opencv_parity.rs`.

mod clipper;
mod fill;
mod resize;
mod shape;
mod warp;

pub use clipper::offset_closed_round;
pub use fill::fill_poly;
pub use resize::{resize_linear, resize_linear_rgb};
pub use shape::{RotatedRect, arc_length, box_points, contour_area, convex_hull, min_area_rect};
pub use warp::{get_perspective_transform, invert_3x3, warp_perspective_cubic_replicate};

/// `cvRound` for doubles: round half to even (the default FPU rounding mode).
#[inline]
pub(crate) fn cv_round(v: f64) -> i64 {
    v.round_ties_even() as i64
}

/// `saturate_cast<short>(float)`.
#[inline]
pub(crate) fn saturate_i16(v: f32) -> i16 {
    v.round_ties_even().clamp(i16::MIN as f32, i16::MAX as f32) as i16
}

/// A 2-D point with either integer or float coordinates, as accepted by `convexHull` /
/// `minAreaRect` (OpenCV keeps the arithmetic in the input type until the hull is converted to
/// float, which affects which hull is produced for nearly collinear points).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CvPoints<'a> {
    /// `CV_32SC2` points
    Int(&'a [[i32; 2]]),
    /// `CV_32FC2` points
    Float(&'a [[f32; 2]]),
}

/// Wisp patch: PaddleX `get_mini_boxes` corner order — sort by x (stable), then pick
/// top-left, top-right, bottom-right, bottom-left.
pub fn paddlex_order(points: [[f32; 2]; 4]) -> [[f32; 2]; 4] {
    let mut p = points;
    p.sort_by(|a, b| a[0].partial_cmp(&b[0]).unwrap_or(std::cmp::Ordering::Equal));
    let (i1, i4) = if p[1][1] > p[0][1] { (0, 1) } else { (1, 0) };
    let (i2, i3) = if p[3][1] > p[2][1] { (2, 3) } else { (3, 2) };
    [p[i1], p[i2], p[i3], p[i4]]
}

/// Wisp patch: the decimal value a user meant by an `f32` threshold (`0.6f32` → `0.6f64`), for
/// comparisons that PaddleX performs between a Python float and a double score.
pub fn f32_as_decimal_f64(v: f32) -> f64 {
    v.to_string().parse().unwrap_or(v as f64)
}
