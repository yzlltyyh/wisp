//! 裁剪坐标换算测试：多显示器、混合缩放、负坐标原点下逻辑坐标 → 物理像素无偏移。
//!
//! 完成定义：换算矩阵（缩放比 1 / 1.25 / 1.5 / 1.75 / 2 / 3、负原点、边界夹紧）的误差 ≤ 1 物理像素；
//! 实际裁剪出的像素与冻结帧逐一对应。

use wisp_capture::{Frame, crop, to_physical};
use wisp_types::{MonitorInfo, PhysPoint, PhysRect, PhysSize, Rect};

const SCALES: [f64; 6] = [1.0, 1.25, 1.5, 1.75, 2.0, 3.0];

/// 一个显示器：逻辑尺寸 `lw × lh`，缩放 `scale`，虚拟桌面物理原点 `origin`（可以为负）
fn monitor(id: &str, lw: f64, lh: f64, scale: f64, origin: (i32, i32)) -> MonitorInfo {
    MonitorInfo {
        id: id.to_owned(),
        name: id.to_owned(),
        physical_origin: PhysPoint {
            x: origin.0,
            y: origin.1,
        },
        physical_size: PhysSize {
            width: (lw * scale).round() as u32,
            height: (lh * scale).round() as u32,
        },
        scale_factor: scale,
        frame: Rect::new(f64::from(origin.0) / scale, f64::from(origin.1) / scale, lw, lh),
        work_area: Rect::new(0.0, 0.0, lw, lh - 40.0),
        is_primary: origin == (0, 0),
    }
}

/// 每个像素编码自己的物理坐标：R/G = x 的低 / 高字节，B/A 的低 7 位 = y 的低 / 高字节（A 最高位恒 1，保持不透明判定无关）
fn coordinate_frame(m: MonitorInfo) -> Frame {
    let w = m.physical_size.width;
    let h = m.physical_size.height;
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            px.extend_from_slice(&[
                (x & 0xff) as u8,
                (x >> 8) as u8,
                (y & 0xff) as u8,
                0x80 | (y >> 8) as u8,
            ]);
        }
    }
    Frame::from_rgba(m, w, h, px)
}

fn decode(px: &[u8]) -> (u32, u32) {
    (
        u32::from(px[0]) | (u32::from(px[1]) << 8),
        u32::from(px[2]) | (u32::from(px[3] & 0x7f) << 8),
    )
}

/// 断言换算结果的每条边与精确值的误差 ≤ 0.5 物理像素（夹紧的边除外）
fn assert_edges_exact(local: Rect, s: f64, bounds: (u32, u32), r: PhysRect) {
    let exact = [local.x * s, local.right() * s, local.y * s, local.bottom() * s];
    let got = [r.x, r.x + r.width, r.y, r.y + r.height];
    let limit = [bounds.0, bounds.0, bounds.1, bounds.1];
    for i in 0..4 {
        let clamped = exact[i].clamp(0.0, f64::from(limit[i]));
        let err = (f64::from(got[i]) - clamped).abs();
        assert!(
            err <= 0.5 + 1e-9,
            "scale {s} rect {local:?}: edge {i} = {} vs exact {clamped} (err {err})",
            got[i]
        );
    }
}

#[test]
fn formula_matches_spec_examples() {
    // px = round(x·s)，pw = round((x+w)·s) − px
    let r = to_physical(Rect::new(10.3, 20.6, 100.2, 50.5), 1.5, (4000, 4000));
    assert_eq!(
        r,
        PhysRect {
            x: 15,
            y: 31,
            width: 166 - 15,
            height: 107 - 31
        }
    );
    // 2×：整数逻辑坐标恰好翻倍
    assert_eq!(
        to_physical(Rect::new(10.0, 20.0, 100.0, 50.0), 2.0, (3024, 1964)),
        PhysRect {
            x: 20,
            y: 40,
            width: 200,
            height: 100
        }
    );
}

#[test]
fn matrix_error_at_most_half_pixel() {
    // 覆盖小数坐标、半像素边界与贴边选区
    let rects = [
        Rect::new(0.0, 0.0, 1.0, 1.0),
        Rect::new(0.5, 0.5, 0.5, 0.5),
        Rect::new(10.3, 7.7, 333.3, 211.1),
        Rect::new(100.25, 50.75, 0.75, 1.25),
        Rect::new(1919.0, 1079.0, 1.0, 1.0),
        Rect::new(0.0, 0.0, 1920.0, 1080.0),
        Rect::new(1200.4, 600.6, 719.6, 479.4),
        Rect::new(3.333, 6.667, 99.999, 0.001),
    ];
    for &s in &SCALES {
        let bounds = ((1920.0 * s).round() as u32, (1080.0 * s).round() as u32);
        for &local in &rects {
            assert_edges_exact(local, s, bounds, to_physical(local, s, bounds));
        }
        // 网格扫描：1/8 pt 步进
        for i in 0..200 {
            let x = f64::from(i) * 0.125 + 3.0;
            let local = Rect::new(x, x * 0.5, 17.0 + x * 0.3, 9.0 + x * 0.1);
            assert_edges_exact(local, s, bounds, to_physical(local, s, bounds));
        }
    }
}

#[test]
fn adjacent_selections_tile_without_gaps_or_overlap() {
    for &s in &SCALES {
        let bounds = (10_000, 10_000);
        let a = to_physical(Rect::new(10.1, 0.0, 33.37, 10.0), s, bounds);
        let b = to_physical(Rect::new(10.1 + 33.37, 0.0, 20.0, 10.0), s, bounds);
        assert_eq!(a.x + a.width, b.x, "scale {s}");
    }
}

#[test]
fn clamping_and_degenerate_inputs() {
    let bounds = (3000, 2000);
    // 越界：取与图像的交集
    let r = to_physical(Rect::new(-10.0, -5.0, 30.0, 20.0), 2.0, bounds);
    assert_eq!(
        r,
        PhysRect {
            x: 0,
            y: 0,
            width: 40,
            height: 30
        }
    );
    let r = to_physical(Rect::new(1490.0, 990.0, 100.0, 100.0), 2.0, bounds);
    assert_eq!(
        r,
        PhysRect {
            x: 2980,
            y: 1980,
            width: 20,
            height: 20
        }
    );
    // 完全在外：横向交集为空
    let r = to_physical(Rect::new(5000.0, 10.0, 10.0, 10.0), 1.0, bounds);
    assert_eq!((r.x, r.width, r.height), (3000, 0, 10));
    // 负宽高：反向拖拽的选区
    let r = to_physical(Rect::new(110.0, 60.0, -100.0, -50.0), 1.5, bounds);
    assert_eq!(
        r,
        PhysRect {
            x: 15,
            y: 15,
            width: 150,
            height: 75
        }
    );
    // 非法 scale 按 1 处理；NaN 坐标按 0
    let r = to_physical(Rect::new(1.0, 1.0, 2.0, 2.0), f64::NAN, bounds);
    assert_eq!(
        r,
        PhysRect {
            x: 1,
            y: 1,
            width: 2,
            height: 2
        }
    );
    let r = to_physical(Rect::new(f64::NAN, 0.0, 5.0, 5.0), 1.0, bounds);
    assert_eq!(r.x, 0);
}

#[test]
fn crop_pixels_align_across_mixed_scale_monitors() {
    // 三屏混合缩放：左侧 1.5×（负原点）、主屏 2×（Retina）、右侧 1×，还有 1.25× 的竖屏
    let monitors = [
        monitor("left-1.5", 640.0, 400.0, 1.5, (-960, -120)),
        monitor("main-2", 512.0, 320.0, 2.0, (0, 0)),
        monitor("right-1", 800.0, 600.0, 1.0, (1024, 0)),
        monitor("tall-1.25", 320.0, 512.0, 1.25, (1824, -200)),
    ];
    let selections = [
        Rect::new(0.0, 0.0, 10.0, 10.0),
        Rect::new(13.37, 42.42, 101.01, 33.3),
        Rect::new(200.5, 100.25, 50.75, 60.5),
        Rect::new(300.0, 250.0, 9.0, 9.0),
    ];
    for m in monitors {
        let s = m.scale_factor;
        let frame = coordinate_frame(m.clone());
        for &sel in &selections {
            let img = crop(&frame, sel).expect("裁剪成功");
            let expected = to_physical(sel, s, (frame.width, frame.height));
            assert_eq!(img.dimensions(), (expected.width, expected.height), "{} {sel:?}", m.id);
            // 四个角的像素恰好是换算出的物理坐标
            let corners = [
                (0, 0, expected.x, expected.y),
                (img.width() - 1, 0, expected.x + expected.width - 1, expected.y),
                (0, img.height() - 1, expected.x, expected.y + expected.height - 1),
                (
                    img.width() - 1,
                    img.height() - 1,
                    expected.x + expected.width - 1,
                    expected.y + expected.height - 1,
                ),
            ];
            for (cx, cy, px, py) in corners {
                assert_eq!(decode(&img.get_pixel(cx, cy).0), (px, py), "{} {sel:?}", m.id);
            }
            // 左上角与精确值的偏差 ≤ 0.5 物理像素
            assert!((f64::from(expected.x) - sel.x * s).abs() <= 0.5);
            assert!((f64::from(expected.y) - sel.y * s).abs() <= 0.5);
        }
    }
}

#[test]
fn crop_rejects_empty_and_clamps_partial() {
    let frame = coordinate_frame(monitor("m", 100.0, 50.0, 2.0, (0, 0)));
    assert!(crop(&frame, Rect::new(500.0, 10.0, 10.0, 10.0)).is_err());
    assert!(crop(&frame, Rect::new(10.0, 10.0, 0.0, 10.0)).is_err());
    let img = crop(&frame, Rect::new(95.0, 45.0, 20.0, 20.0)).expect("部分越界仍可裁剪");
    assert_eq!(img.dimensions(), (10, 10));
    assert_eq!(decode(&img.get_pixel(0, 0).0), (190, 90));
}

#[test]
fn crop_follows_actual_frame_size_when_scale_disagrees() {
    // platform 报告 2×，但后端只给出 1× 的图（例如捕获前一刻改了分辨率）：选区仍按冻结帧对齐
    let mut m = monitor("mismatch", 400.0, 300.0, 2.0, (0, 0));
    m.physical_size = PhysSize {
        width: 400,
        height: 300,
    };
    let frame = coordinate_frame(m);
    let img = crop(&frame, Rect::new(100.0, 50.0, 40.0, 30.0)).expect("裁剪成功");
    assert_eq!(img.dimensions(), (40, 30));
    assert_eq!(decode(&img.get_pixel(0, 0).0), (100, 50));
}
