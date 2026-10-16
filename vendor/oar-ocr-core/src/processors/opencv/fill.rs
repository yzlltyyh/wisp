//! `cv::fillPoly` for one integer contour on an 8-bit single-channel image (`lineType = 8`,
//! `shift = 0`).
//!
//! OpenCV fills a polygon in two steps: every edge is first drawn as an 8-connected Bresenham
//! line, then the interior is filled scanline by scanline in 16.16 fixed point. PaddleX's
//! `box_score_fast` averages the probability map under such a mask, so the exact set of pixels
//! (including the edge pixels) decides whether a borderline region passes `box_thresh`.

const XY_SHIFT: i64 = 16;
const XY_ONE: i64 = 1 << XY_SHIFT;

#[derive(Debug, Clone, Copy)]
struct PolyEdge {
    y0: i64,
    y1: i64,
    x: i64,
    dx: i64,
}

/// Fills `pts` with `color` into a row-major `width × height` mask.
pub fn fill_poly(mask: &mut [u8], width: usize, height: usize, pts: &[[i32; 2]], color: u8) {
    assert_eq!(
        mask.len(),
        width * height,
        "mask size does not match the dimensions"
    );
    if pts.is_empty() || width == 0 || height == 0 {
        return;
    }
    let mut edges = Vec::with_capacity(pts.len() + 1);
    collect_poly_edges(mask, width, height, pts, color, &mut edges);
    fill_edge_collection(mask, width, height, edges, color);
}

fn collect_poly_edges(
    mask: &mut [u8],
    width: usize,
    height: usize,
    pts: &[[i32; 2]],
    color: u8,
    edges: &mut Vec<PolyEdge>,
) {
    let last = pts[pts.len() - 1];
    let mut pt0 = [(last[0] as i64) << XY_SHIFT, last[1] as i64];
    for v in pts {
        let pt1 = [(v[0] as i64) << XY_SHIFT, v[1] as i64];
        let mut pt0c = pt0;
        let mut pt1c = pt1;
        let mut t0 = [(pt0[0] + (XY_ONE >> 1)) >> XY_SHIFT, pt0[1]];
        let mut t1 = [(pt1[0] + (XY_ONE >> 1)) >> XY_SHIFT, pt1[1]];
        line(mask, width, height, t0, t1, color);

        // use clipped endpoints to create a more accurate PolyEdge
        let outside =
            |t: [i64; 2]| t[0] < 0 || t[0] >= width as i64 || t[1] < 0 || t[1] >= height as i64;
        if outside(t0) || outside(t1) {
            clip_line(width as i64, height as i64, &mut t0, &mut t1);
            if t0[1] != t1[1] {
                pt0c = [t0[0] << XY_SHIFT, t0[1]];
                pt1c = [t1[0] << XY_SHIFT, t1[1]];
            }
        } else {
            pt0c[0] += XY_ONE >> 1;
            pt1c[0] += XY_ONE >> 1;
        }

        if pt0[1] != pt1[1] {
            let dx = (pt1c[0] - pt0c[0]) / (pt1c[1] - pt0c[1]);
            let edge = if pt0[1] < pt1[1] {
                PolyEdge {
                    y0: pt0[1],
                    y1: pt1[1],
                    x: pt0c[0] + (pt0[1] - pt0c[1]) * dx,
                    dx,
                }
            } else {
                PolyEdge {
                    y0: pt1[1],
                    y1: pt0[1],
                    x: pt1c[0] + (pt1[1] - pt1c[1]) * dx,
                    dx,
                }
            };
            edges.push(edge);
        }
        pt0 = pt1;
    }
}

fn fill_edge_collection(
    mask: &mut [u8],
    width: usize,
    height: usize,
    mut edges: Vec<PolyEdge>,
    color: u8,
) {
    let total = edges.len();
    if total < 2 {
        return;
    }
    let (mut y_min, mut y_max) = (i64::MAX, i64::MIN);
    let (mut x_min, mut x_max) = (i64::MAX, -1i64);
    for e in &edges {
        let x1 = e.x + (e.y1 - e.y0) * e.dx;
        y_min = y_min.min(e.y0);
        y_max = y_max.max(e.y1);
        x_min = x_min.min(e.x).min(x1);
        x_max = x_max.max(e.x).max(x1);
    }
    if y_max < 0 || y_min >= height as i64 || x_max < 0 || x_min >= (width as i64) << XY_SHIFT {
        return;
    }
    edges.sort_by(|a, b| a.y0.cmp(&b.y0).then(a.x.cmp(&b.x)).then(a.dx.cmp(&b.dx)));
    // sentinel (y0 = INT_MAX) and the list head
    edges.push(PolyEdge {
        y0: i32::MAX as i64,
        y1: 0,
        x: 0,
        dx: 0,
    });
    let head = total + 1;
    let mut next: Vec<Option<usize>> = vec![None; total + 2];
    let mut i = 0usize;
    let y_max = y_max.min(height as i64);

    let mut y = edges[0].y0;
    while y < y_max {
        let mut draw = false;
        let clipline = y < 0;
        let mut prelast = head;
        let mut last = next[head];
        while last.is_some() || edges[i].y0 == y {
            if let Some(l) = last
                && edges[l].y1 == y
            {
                // exclude edge if y reaches its lower point
                next[prelast] = next[l];
                last = next[l];
                continue;
            }
            let keep_prelast = prelast;
            if let Some(l) = last
                && (edges[i].y0 > y || edges[l].x < edges[i].x)
            {
                // go to the next edge in active list
                prelast = l;
                last = next[l];
            } else if i < total {
                // insert new edge into active list if y reaches its upper point
                next[prelast] = Some(i);
                next[i] = last;
                prelast = i;
                i += 1;
            } else {
                break;
            }

            if draw {
                if !clipline {
                    let (kx, px) = (edges[keep_prelast].x, edges[prelast].x);
                    let (mut x1, mut x2) = if kx > px {
                        (px >> XY_SHIFT, kx >> XY_SHIFT)
                    } else {
                        (kx >> XY_SHIFT, px >> XY_SHIFT)
                    };
                    if x1 < width as i64 && x2 >= 0 {
                        x1 = x1.max(0);
                        x2 = x2.min(width as i64 - 1);
                        if x1 <= x2 {
                            let row = y as usize * width;
                            mask[row + x1 as usize..=row + x2 as usize].fill(color);
                        }
                    }
                }
                edges[keep_prelast].x += edges[keep_prelast].dx;
                edges[prelast].x += edges[prelast].dx;
            }
            draw = !draw;
        }

        // sort edges (using bubble sort)
        let mut keep_prelast: Option<usize> = None;
        loop {
            let mut prelast = head;
            let mut last = next[head];
            let mut last_exchange: Option<usize> = None;
            while let Some(l) = last {
                if Some(l) == keep_prelast {
                    break;
                }
                let Some(te) = next[l] else {
                    break;
                };
                if edges[l].x > edges[te].x {
                    // swap edges
                    next[prelast] = Some(te);
                    next[l] = next[te];
                    next[te] = Some(l);
                    prelast = te;
                    last_exchange = Some(prelast);
                } else {
                    prelast = l;
                    last = Some(te);
                }
            }
            let Some(exchange) = last_exchange else {
                break;
            };
            keep_prelast = Some(exchange);
            if keep_prelast == next[head] || exchange == head {
                break;
            }
        }
        y += 1;
    }
}

/// `cv::clipLine(Size2l, Point2l&, Point2l&)`.
fn clip_line(width: i64, height: i64, p1: &mut [i64; 2], p2: &mut [i64; 2]) -> bool {
    let right = width - 1;
    let bottom = height - 1;
    if width <= 0 || height <= 0 {
        return false;
    }
    let code = |p: &[i64; 2]| -> i32 {
        (p[0] < 0) as i32
            + (p[0] > right) as i32 * 2
            + (p[1] < 0) as i32 * 4
            + (p[1] > bottom) as i32 * 8
    };
    let mut c1 = code(p1);
    let mut c2 = code(p2);
    if (c1 & c2) == 0 && (c1 | c2) != 0 {
        if c1 & 12 != 0 {
            let a = if c1 < 8 { 0 } else { bottom };
            p1[0] += ((a - p1[1]) as f64 * (p2[0] - p1[0]) as f64 / (p2[1] - p1[1]) as f64) as i64;
            p1[1] = a;
            c1 = (p1[0] < 0) as i32 + (p1[0] > right) as i32 * 2;
        }
        if c2 & 12 != 0 {
            let a = if c2 < 8 { 0 } else { bottom };
            p2[0] += ((a - p2[1]) as f64 * (p2[0] - p1[0]) as f64 / (p2[1] - p1[1]) as f64) as i64;
            p2[1] = a;
            c2 = (p2[0] < 0) as i32 + (p2[0] > right) as i32 * 2;
        }
        if (c1 & c2) == 0 && (c1 | c2) != 0 {
            if c1 != 0 {
                let a = if c1 == 1 { 0 } else { right };
                p1[1] +=
                    ((a - p1[0]) as f64 * (p2[1] - p1[1]) as f64 / (p2[0] - p1[0]) as f64) as i64;
                p1[0] = a;
                c1 = 0;
            }
            if c2 != 0 {
                let a = if c2 == 1 { 0 } else { right };
                p2[1] +=
                    ((a - p2[0]) as f64 * (p2[1] - p1[1]) as f64 / (p2[0] - p1[0]) as f64) as i64;
                p2[0] = a;
                c2 = 0;
            }
        }
    }
    (c1 | c2) == 0
}

/// `Line(img, pt1, pt2, color, 8)`: an 8-connected `LineIterator` walked left to right.
fn line(mask: &mut [u8], width: usize, height: usize, pt1: [i64; 2], pt2: [i64; 2], color: u8) {
    let (mut p1, mut p2) = (pt1, pt2);
    let inside =
        |p: &[i64; 2]| p[0] >= 0 && p[0] < width as i64 && p[1] >= 0 && p[1] < height as i64;
    if (!inside(&p1) || !inside(&p2)) && !clip_line(width as i64, height as i64, &mut p1, &mut p2) {
        return;
    }
    let (mut delta_x, mut delta_y) = (1i64, 1i64);
    let mut dx = p2[0] - p1[0];
    let mut dy = p2[1] - p1[1];
    if dx < 0 {
        // leftToRight
        dx = -dx;
        dy = -dy;
        p1 = p2;
    }
    if dy < 0 {
        dy = -dy;
        delta_y = -1;
    }
    let vert = dy > dx;
    if vert {
        std::mem::swap(&mut dx, &mut dy);
        std::mem::swap(&mut delta_x, &mut delta_y);
    }
    let mut err = dx - (dy + dy);
    let plus_delta = dx + dx;
    let minus_delta = -(dy + dy);
    let (mut minus_shift, mut plus_shift, mut minus_step, mut plus_step) =
        (delta_x, 0i64, 0i64, delta_y);
    let count = dx + 1;
    if vert {
        std::mem::swap(&mut plus_step, &mut plus_shift);
        std::mem::swap(&mut minus_step, &mut minus_shift);
    }
    let mut p = p1;
    for _ in 0..count {
        mask[p[1] as usize * width + p[0] as usize] = color;
        let m = if err < 0 { -1i64 } else { 0 };
        err += minus_delta + (plus_delta & m);
        p[0] += minus_shift + (plus_shift & m);
        p[1] += minus_step + (plus_step & m);
    }
}
