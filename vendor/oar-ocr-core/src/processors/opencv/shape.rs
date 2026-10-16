//! `cv::convexHull`, `cv::minAreaRect`, `cv::boxPoints`, `cv::contourArea` and `cv::arcLength`.
//!
//! The DB post-processing and the crop step both derive their quadrilaterals from
//! `minAreaRect` + `boxPoints`. Mathematically any correct minimum-area rectangle would do, but
//! float rounding and OpenCV's tie-breaking (hull start point, `area <= minarea`, the edge chosen
//! as width) decide the exact corner coordinates, which are then rounded to integers. The code
//! below follows OpenCV's Sklansky hull and rotating-calipers implementation operation by
//! operation, in the same precision (`float` with selected `double` intermediates).

use super::CvPoints;

/// `cv::RotatedRect` as returned by `cv::minAreaRect` (angle in degrees).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RotatedRect {
    /// Centre `(x, y)`
    pub center: [f32; 2],
    /// `(width, height)`
    pub size: [f32; 2],
    /// Rotation in degrees
    pub angle: f32,
}

impl RotatedRect {
    /// The shorter side (`min(bounding_box[1])` in PaddleX).
    pub fn min_side(&self) -> f32 {
        self.size[0].min(self.size[1])
    }
}

trait HullCoord: Copy + PartialOrd + PartialEq + std::ops::Sub<Output = Self> {
    fn zero() -> Self;
    fn sign(self) -> i32;
    fn dot(self) -> f64;
    fn to_f32(self) -> f32;
}

impl HullCoord for i32 {
    fn zero() -> Self {
        0
    }
    fn sign(self) -> i32 {
        self.signum()
    }
    fn dot(self) -> f64 {
        self as f64
    }
    fn to_f32(self) -> f32 {
        self as f32
    }
}

impl HullCoord for f32 {
    fn zero() -> Self {
        0.0
    }
    fn sign(self) -> i32 {
        (self > 0.0) as i32 - (self < 0.0) as i32
    }
    fn dot(self) -> f64 {
        self as f64
    }
    fn to_f32(self) -> f32 {
        self
    }
}

/// `Sklansky_` over the sorted point order. `idx` maps sorted positions to input indices.
fn sklansky<T: HullCoord>(
    pts: &[[T; 2]],
    idx: &[usize],
    start: isize,
    end: isize,
    nsign: i32,
    sign2: i32,
) -> Vec<isize> {
    let p = |i: isize| pts[idx[i as usize]];
    let incr: isize = if end > start { 1 } else { -1 };
    let mut pprev = start;
    let mut pcur = pprev + incr;
    let mut pnext = pcur + incr;
    if start == end || (p(start)[0] == p(end)[0] && p(start)[1] == p(end)[1]) {
        return vec![start];
    }
    let mut stack: Vec<isize> = vec![pprev, pcur, pnext];
    let end = end + incr;
    while pnext != end {
        let cury = p(pcur)[1];
        let nexty = p(pnext)[1];
        let by = nexty - cury;
        if by.sign() != nsign {
            let ax = p(pcur)[0] - p(pprev)[0];
            let bx = p(pnext)[0] - p(pcur)[0];
            let ay = cury - p(pprev)[1];
            // Integer hulls use int64 products, float hulls double products.
            let convexity = ay.dot() * bx.dot() - ax.dot() * by.dot();
            let convexity_sign = (convexity > 0.0) as i32 - (convexity < 0.0) as i32;
            if convexity_sign == sign2 && (ax != T::zero() || ay != T::zero()) {
                pprev = pcur;
                pcur = pnext;
                pnext += incr;
                stack.push(pnext);
            } else if pprev == start {
                pcur = pnext;
                stack[1] = pcur;
                pnext += incr;
                stack[2] = pnext;
            } else {
                let n = stack.len();
                stack[n - 2] = pnext;
                pcur = pprev;
                pprev = stack[n - 4];
                stack.pop();
            }
        } else {
            pnext += incr;
            let n = stack.len();
            stack[n - 1] = pnext;
        }
    }
    stack.pop();
    stack
}

fn hull_indices<T: HullCoord>(pts: &[[T; 2]], clockwise: bool) -> Vec<usize> {
    let total = pts.len();
    if total == 0 {
        return Vec::new();
    }
    let mut idx: Vec<usize> = (0..total).collect();
    idx.sort_by(|&a, &b| {
        let (pa, pb) = (pts[a], pts[b]);
        pa[0]
            .partial_cmp(&pb[0])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                pa[1]
                    .partial_cmp(&pb[1])
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(a.cmp(&b))
    });
    let (mut miny_ind, mut maxy_ind) = (0usize, 0usize);
    for i in 1..total {
        let y = pts[idx[i]][1];
        if pts[idx[miny_ind]][1] > y {
            miny_ind = i;
        }
        if pts[idx[maxy_ind]][1] < y {
            maxy_ind = i;
        }
    }
    let first = pts[idx[0]];
    let last = pts[idx[total - 1]];
    let mut hull: Vec<usize> = Vec::with_capacity(total);
    if first[0] == last[0] && first[1] == last[1] {
        hull.push(0);
        return hull;
    }
    let p = |i: isize| pts[idx[i as usize]];
    let n = total as isize;

    // upper half
    let mut tl = sklansky(pts, &idx, 0, maxy_ind as isize, -1, 1);
    let mut tr = sklansky(pts, &idx, n - 1, maxy_ind as isize, -1, -1);
    if !clockwise {
        std::mem::swap(&mut tl, &mut tr);
    }
    for &s in tl.iter().take(tl.len().saturating_sub(1)) {
        hull.push(idx[s as usize]);
    }
    for i in (1..tr.len()).rev() {
        hull.push(idx[tr[i] as usize]);
    }
    let stop_idx: isize = if tr.len() > 2 {
        tr[1]
    } else if tl.len() > 2 {
        tl[tl.len() - 2]
    } else {
        -1
    };

    // lower half
    let mut bl = sklansky(pts, &idx, 0, miny_ind as isize, 1, -1);
    let mut br = sklansky(pts, &idx, n - 1, miny_ind as isize, 1, 1);
    if clockwise {
        std::mem::swap(&mut bl, &mut br);
    }
    let (mut bl_count, mut br_count) = (bl.len(), br.len());
    if stop_idx >= 0 {
        let check_idx: isize = if bl_count > 2 {
            bl[1]
        } else if bl_count + br_count > 2 {
            br[2 - bl_count]
        } else {
            -1
        };
        if check_idx == stop_idx
            || (check_idx >= 0
                && p(check_idx)[0] == p(stop_idx)[0]
                && p(check_idx)[1] == p(stop_idx)[1])
        {
            // all the points lie on the same line: the bottom part mirrors the top part
            bl_count = bl_count.min(2);
            br_count = br_count.min(2);
        }
    }
    for &s in bl.iter().take(bl_count.saturating_sub(1)) {
        hull.push(idx[s as usize]);
    }
    for i in (1..br_count).rev() {
        hull.push(idx[br[i] as usize]);
    }

    // Try to make the hull indices an ascending or descending sequence by a cyclic shift.
    let nout = hull.len();
    if nout >= 3 {
        let (mut min_idx, mut max_idx, mut lt) = (0usize, 0usize, 0usize);
        for i in 1..nout {
            let id = hull[i];
            lt += (hull[i - 1] < id) as usize;
            if lt > 1 && lt + 2 <= i {
                break;
            }
            if id < hull[min_idx] {
                min_idx = i;
            }
            if id > hull[max_idx] {
                max_idx = i;
            }
        }
        let mmdist = max_idx.abs_diff(min_idx);
        if (mmdist == 1 || mmdist == nout - 1) && (lt <= 1 || lt + 2 >= nout) {
            let ascending = (max_idx + 1) % nout == min_idx;
            let i0 = if ascending { min_idx } else { max_idx };
            if i0 > 0 {
                let mut shifted = Vec::with_capacity(nout);
                let mut j = i0;
                let mut i = 0;
                while i < nout {
                    let curr = hull[j];
                    shifted.push(curr);
                    let next_j = if j + 1 < nout { j + 1 } else { 0 };
                    if i < nout - 1 && (ascending != (curr < hull[next_j])) {
                        break;
                    }
                    j = next_j;
                    i += 1;
                }
                if i == nout {
                    hull = shifted;
                }
            }
        }
    }
    hull
}

/// `cv::convexHull(points, hull, clockwise, returnPoints = true)`, converted to float.
pub fn convex_hull(points: CvPoints<'_>, clockwise: bool) -> Vec<[f32; 2]> {
    match points {
        CvPoints::Int(p) => hull_indices(p, clockwise)
            .into_iter()
            .map(|i| [p[i][0].to_f32(), p[i][1].to_f32()])
            .collect(),
        CvPoints::Float(p) => hull_indices(p, clockwise)
            .into_iter()
            .map(|i| p[i])
            .collect(),
    }
}

/// `rotatingCalipers(..., CALIPERS_MINAREARECT, out)`: corner, edge vector 1, edge vector 2.
fn rotating_calipers(points: &[[f32; 2]]) -> [[f32; 2]; 3] {
    let n = points.len();
    let mut minarea = f32::MAX;
    let mut vect = vec![[0f32; 2]; n];
    let mut inv_vect_length = vec![0f32; n];
    let (mut left, mut bottom, mut right, mut top) = (0usize, 0usize, 0usize, 0usize);
    let mut pt0 = points[0];
    let (mut left_x, mut right_x, mut top_y, mut bottom_y) = (pt0[0], pt0[0], pt0[1], pt0[1]);
    for i in 0..n {
        if pt0[0] < left_x {
            left_x = pt0[0];
            left = i;
        }
        if pt0[0] > right_x {
            right_x = pt0[0];
            right = i;
        }
        if pt0[1] > top_y {
            top_y = pt0[1];
            top = i;
        }
        if pt0[1] < bottom_y {
            bottom_y = pt0[1];
            bottom = i;
        }
        let pt = points[if i + 1 < n { i + 1 } else { 0 }];
        let dx = (pt[0] - pt0[0]) as f64;
        let dy = (pt[1] - pt0[1]) as f64;
        vect[i] = [dx as f32, dy as f32];
        inv_vect_length[i] = (1.0 / (dx * dx + dy * dy).sqrt()) as f32;
        pt0 = pt;
    }

    // convex hull orientation
    let mut orientation = 0f32;
    {
        let mut ax = vect[n - 1][0] as f64;
        let mut ay = vect[n - 1][1] as f64;
        for v in vect.iter() {
            let bx = v[0] as f64;
            let by = v[1] as f64;
            let convexity = ax * by - ay * bx;
            if convexity != 0.0 {
                orientation = if convexity > 0.0 { 1.0 } else { -1.0 };
                break;
            }
            ax = bx;
            ay = by;
        }
    }
    let mut base_a = orientation;
    let mut base_b = 0f32;
    let mut seq = [bottom, right, top, left];
    // (leftist point, base_a, width, base_b, height, bottom point)
    let mut best: (usize, f32, f32, f32, f32, usize) = (0, 0.0, 0.0, 0.0, 0.0, 0);

    let rot90_cw = |v: [f32; 2]| [v[1], -v[0]];
    let first_vec_is_right = |v1: [f32; 2], v2: [f32; 2]| {
        let t = rot90_cw(v1);
        t[0] * v2[0] + t[1] * v2[1] < 0.0
    };
    for _ in 0..n {
        let rot_vect = [
            vect[seq[0]],
            rot90_cw(vect[seq[1]]),
            [-vect[seq[2]][0], -vect[seq[2]][1]],
            [-vect[seq[3]][1], vect[seq[3]][0]],
        ];
        let mut main_element = 0usize;
        for i in 1..4 {
            if first_vec_is_right(rot_vect[i], rot_vect[main_element]) {
                main_element = i;
            }
        }
        let pindex = seq[main_element];
        let lead_x = vect[pindex][0] * inv_vect_length[pindex];
        let lead_y = vect[pindex][1] * inv_vect_length[pindex];
        match main_element {
            0 => {
                base_a = lead_x;
                base_b = lead_y;
            }
            1 => {
                base_a = lead_y;
                base_b = -lead_x;
            }
            2 => {
                base_a = -lead_x;
                base_b = -lead_y;
            }
            _ => {
                base_a = -lead_y;
                base_b = lead_x;
            }
        }
        seq[main_element] += 1;
        if seq[main_element] == n {
            seq[main_element] = 0;
        }

        let dx = points[seq[1]][0] - points[seq[3]][0];
        let dy = points[seq[1]][1] - points[seq[3]][1];
        let width = dx * base_a + dy * base_b;
        let dx = points[seq[2]][0] - points[seq[0]][0];
        let dy = points[seq[2]][1] - points[seq[0]][1];
        let height = -dx * base_b + dy * base_a;
        let area = width * height;
        if area <= minarea {
            minarea = area;
            best = (seq[3], base_a, width, base_b, height, seq[0]);
        }
    }

    let (li, a1, w, b1, h, bi) = best;
    let a2 = -b1;
    let b2 = a1;
    let c1 = a1 * points[li][0] + points[li][1] * b1;
    let c2 = a2 * points[bi][0] + points[bi][1] * b2;
    let idet = 1.0f32 / (a1 * b2 - a2 * b1);
    let px = (c1 * b2 - c2 * b1) * idet;
    let py = (a1 * c2 - a2 * c1) * idet;
    [[px, py], [a1 * w, b1 * w], [a2 * h, b2 * h]]
}

/// `cv::minAreaRect`.
pub fn min_area_rect(points: CvPoints<'_>) -> RotatedRect {
    let hull = convex_hull(points, false);
    let n = hull.len();
    let mut rect = RotatedRect::default();
    let angle: f32;
    if n > 2 {
        let out = rotating_calipers(&hull);
        rect.center = [
            out[0][0] + (out[1][0] + out[2][0]) * 0.5,
            out[0][1] + (out[1][1] + out[2][1]) * 0.5,
        ];
        let (o1, o2) = (out[1].map(f64::from), out[2].map(f64::from));
        rect.size = [
            (o1[0] * o1[0] + o1[1] * o1[1]).sqrt() as f32,
            (o2[0] * o2[0] + o2[1] * o2[1]).sqrt() as f32,
        ];
        angle = o1[1].atan2(o1[0]) as f32;
    } else if n == 2 {
        rect.center = [
            (hull[0][0] + hull[1][0]) * 0.5,
            (hull[0][1] + hull[1][1]) * 0.5,
        ];
        let dx = (hull[1][0] - hull[0][0]) as f64;
        let dy = (hull[1][1] - hull[0][1]) as f64;
        rect.size = [(dx * dx + dy * dy).sqrt() as f32, 0.0];
        angle = dy.atan2(dx) as f32;
    } else {
        if n == 1 {
            rect.center = hull[0];
        }
        angle = 0.0;
    }
    rect.angle = ((angle * 180.0) as f64 / std::f64::consts::PI) as f32;
    rect
}

/// `cv::boxPoints` (`RotatedRect::points`).
pub fn box_points(rect: &RotatedRect) -> [[f32; 2]; 4] {
    let angle = rect.angle as f64 * std::f64::consts::PI / 180.0;
    let b = angle.cos() as f32 * 0.5;
    let a = angle.sin() as f32 * 0.5;
    let [cx, cy] = rect.center;
    let [w, h] = rect.size;
    let p0 = [cx - a * h - b * w, cy + b * h - a * w];
    let p1 = [cx + a * h - b * w, cy - b * h - a * w];
    [
        p0,
        p1,
        [2.0 * cx - p0[0], 2.0 * cy - p0[1]],
        [2.0 * cx - p1[0], 2.0 * cy - p1[1]],
    ]
}

/// `cv::contourArea(points, oriented = false)` for float points.
pub fn contour_area(points: &[[f32; 2]]) -> f64 {
    let Some(&last) = points.last() else {
        return 0.0;
    };
    let mut prev = last;
    let mut a00 = 0f64;
    for &p in points {
        a00 += prev[0] as f64 * p[1] as f64 - prev[1] as f64 * p[0] as f64;
        prev = p;
    }
    (a00 * 0.5).abs()
}

/// `cv::arcLength(points, closed = true)` for float points.
pub fn arc_length(points: &[[f32; 2]]) -> f64 {
    if points.len() <= 1 {
        return 0.0;
    }
    let mut prev = points[points.len() - 1];
    let mut perimeter = 0f64;
    for &p in points {
        let dx = p[0] - prev[0];
        let dy = p[1] - prev[1];
        perimeter += (dx * dx + dy * dy).sqrt() as f64;
        prev = p;
    }
    perimeter
}
