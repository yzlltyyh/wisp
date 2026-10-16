//! 本地 PP-OCRv6 预处理与后处理和官方 Python 产线（PaddleX 3.7 + OpenCV 4.10 + pyclipper）的逐像素 / 逐框对齐。
//!
//! 参考数据由 `tests/fixtures/ppocr/parity/generate.py` 调用官方产线所用的同一批函数生成（固定种子的小尺寸合成数据）：
//! `cv2.resize`、`DetResizeForTest`、`OCRReisizeNormImg`、`cv2.minAreaRect` / `cv2.boxPoints`、`DBPostProcess.unclip`、
//! `CropByPolys`（透视裁剪）与 `DBPostProcess`（DB 后处理端到端）。不需要模型与 ONNX Runtime。

#![cfg(feature = "ort")]

use std::path::Path;
use std::sync::OnceLock;

use base64::Engine;
use image::{DynamicImage, RgbImage};
use oar_ocr::processors::opencv::{self, CvPoints};
use oar_ocr::processors::{
    DBPostProcess, DBPostProcessConfig, DetResizeForTest, ImageScaleInfo, LimitType, OCRResize, Point,
};
use serde_json::Value;

fn fixture() -> &'static Value {
    static DATA: OnceLock<Value> = OnceLock::new();
    DATA.get_or_init(|| {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ppocr/parity/parity.json");
        serde_json::from_slice(&std::fs::read(path).expect("parity.json")).expect("parity json")
    })
}

fn bytes(v: &Value) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(v.as_str().expect("base64 string"))
        .expect("base64")
}

fn usize_of(v: &Value) -> usize {
    v.as_u64().expect("unsigned integer") as usize
}

fn f32_of(v: &Value) -> f32 {
    v.as_f64().expect("number") as f32
}

/// 展平的点列 `[x0, y0, x1, y1, …]`
fn points_f32(v: &Value) -> Vec<[f32; 2]> {
    let flat: Vec<f32> = v.as_array().expect("points").iter().map(f32_of).collect();
    flat.chunks_exact(2).map(|p| [p[0], p[1]]).collect()
}

/// 展平的整数点列
fn points_i64(v: &Value) -> Vec<[i64; 2]> {
    let flat: Vec<i64> = v
        .as_array()
        .expect("points")
        .iter()
        .map(|c| c.as_i64().expect("integer"))
        .collect();
    flat.chunks_exact(2).map(|p| [p[0], p[1]]).collect()
}

/// 两段字节的最大差与不同的个数
fn diff(a: &[u8], b: &[u8]) -> (u8, usize) {
    assert_eq!(a.len(), b.len(), "长度不同");
    a.iter()
        .zip(b)
        .map(|(x, y)| x.abs_diff(*y))
        .fold((0, 0), |(m, n), d| (m.max(d), n + (d != 0) as usize))
}

#[test]
fn resize_matches_cv2_inter_linear() {
    let mut cases = 0;
    for src in fixture()["resize"].as_array().expect("resize") {
        let (h, w) = (usize_of(&src["h"]), usize_of(&src["w"]));
        let pixels = bytes(&src["src"]);
        for t in src["targets"].as_array().expect("targets") {
            let (dh, dw) = (usize_of(&t["dh"]), usize_of(&t["dw"]));
            let ours = opencv::resize_linear(&pixels, w, h, 3, dw, dh);
            let (max, n) = diff(&ours, &bytes(&t["dst"]));
            assert_eq!((max, n), (0, 0), "{w}×{h} → {dw}×{dh}：最大差 {max}，{n} 个字节不同");
            cases += 1;
        }
    }
    assert!(cases >= 18);
}

#[test]
fn detection_input_sizes_match_det_resize_for_test() {
    for c in fixture()["det_sizes"].as_array().expect("det_sizes") {
        let (h, w) = (usize_of(&c["h"]) as u32, usize_of(&c["w"]) as u32);
        let limit_type = match c["type"].as_str() {
            Some("min") => LimitType::Min,
            Some("max") => LimitType::Max,
            other => panic!("limit type {other:?}"),
        };
        let resize = DetResizeForTest::new(
            None,
            None,
            None,
            Some(usize_of(&c["limit"]) as u32),
            Some(limit_type),
            None,
            Some(usize_of(&c["max_side"]) as u32),
        );
        let (images, _) = resize.apply(vec![DynamicImage::new_rgb8(w, h)], None, None, None);
        let got = (images[0].height(), images[0].width());
        let want = (usize_of(&c["rh"]) as u32, usize_of(&c["rw"]) as u32);
        assert_eq!(got, want, "{w}×{h}（{}）", c["type"]);
    }
}

#[test]
fn recognition_widths_match_ocr_resize_norm_img() {
    let resize = OCRResize::new(None, None);
    for c in fixture()["rec_widths"].as_array().expect("rec_widths") {
        let (h, w) = (usize_of(&c["h"]) as u32, usize_of(&c["w"]) as u32);
        let (tensor_w, resized) = resize.paddle_widths(&[(w, h)]);
        assert_eq!(
            (tensor_w, resized[0]),
            (usize_of(&c["tensor_w"]), usize_of(&c["resized_w"])),
            "{w}×{h}"
        );
    }
}

#[test]
fn min_area_rect_and_box_points_match_cv2() {
    for (i, c) in fixture()["min_area_rect"]
        .as_array()
        .expect("min_area_rect")
        .iter()
        .enumerate()
    {
        let pts = points_f32(&c["points"]);
        let rect = if c["int"].as_bool() == Some(true) {
            let ints: Vec<[i32; 2]> = pts.iter().map(|p| [p[0] as i32, p[1] as i32]).collect();
            opencv::min_area_rect(CvPoints::Int(&ints))
        } else {
            opencv::min_area_rect(CvPoints::Float(&pts))
        };
        // 与 cv2 的 float32 结果逐位相同
        let want = (
            [f32_of(&c["center"][0]), f32_of(&c["center"][1])],
            [f32_of(&c["size"][0]), f32_of(&c["size"][1])],
            f32_of(&c["angle"]),
        );
        assert_eq!((rect.center, rect.size, rect.angle), want, "#{i} 的 RotatedRect");
        let corners = opencv::box_points(&rect).to_vec();
        assert_eq!(corners, points_f32(&c["box"]), "#{i} 的 boxPoints");
    }
}

#[test]
fn unclip_matches_pyclipper() {
    for (i, c) in fixture()["unclip"].as_array().expect("unclip").iter().enumerate() {
        let corners = points_f32(&c["box"]);
        let ratio = c["ratio"].as_f64().expect("ratio");
        let distance = opencv::contour_area(&corners) * ratio / opencv::arc_length(&corners);
        let ours = opencv::offset_closed_round(&corners, distance);
        let want: Vec<Vec<[i64; 2]>> = c["paths"].as_array().expect("paths").iter().map(points_i64).collect();
        // 顶点与顺序都相同（起点影响随后 minAreaRect 在等面积矩形之间的取舍）
        assert_eq!(ours, want, "#{i}");
    }
}

#[test]
fn crop_matches_crop_by_polys() {
    let crop = &fixture()["crop"];
    let (h, w) = (usize_of(&crop["h"]) as u32, usize_of(&crop["w"]) as u32);
    let src = RgbImage::from_raw(w, h, bytes(&crop["src"])).expect("crop source");
    for c in crop["cases"].as_array().expect("cases") {
        let quad: Vec<Point> = points_f32(&c["quad"]).iter().map(|p| Point::new(p[0], p[1])).collect();
        let ours = oar_ocr::utils::get_minarea_rect_crop(&src, &quad).expect("crop");
        let (ch, cw) = (usize_of(&c["h"]) as u32, usize_of(&c["w"]) as u32);
        assert_eq!((ours.height(), ours.width()), (ch, cw), "{:?} 的裁剪尺寸", c["quad"]);
        let (max, n) = diff(ours.as_raw(), &bytes(&c["crop"]));
        assert_eq!((max, n), (0, 0), "{:?}：最大差 {max}，{n} 个字节不同", c["quad"]);
    }
}

#[test]
fn db_postprocess_matches_paddlex() {
    let post = DBPostProcess::new(Some(0.3), Some(0.6), Some(1000), Some(1.5), Some(false), None, None);
    let config = DBPostProcessConfig::new(0.3, 0.6, 1.5);
    let mut total = 0;
    for (i, c) in fixture()["db"].as_array().expect("db").iter().enumerate() {
        let (h, w) = (usize_of(&c["h"]), usize_of(&c["w"]));
        let (dh, dw) = (usize_of(&c["dest_h"]), usize_of(&c["dest_w"]));
        let pred: Vec<f32> = bytes(&c["pred_u8"]).iter().map(|&q| q as f32 / 255.0).collect();
        let shape = ImageScaleInfo::new(dh as f32, dw as f32, h as f32 / dh as f32, w as f32 / dw as f32);
        let (boxes, scores) = post.apply_map(&pred, h, w, &shape, Some(&config));
        let ours: Vec<Vec<[i64; 2]>> = boxes
            .iter()
            .map(|b| b.points.iter().map(|p| [p.x as i64, p.y as i64]).collect())
            .collect();
        let want: Vec<Vec<[i64; 2]>> = c["boxes"].as_array().expect("boxes").iter().map(points_i64).collect();
        assert_eq!(ours, want, "#{i} 的框");
        let want_scores: Vec<f64> = c["scores"]
            .as_array()
            .expect("scores")
            .iter()
            .map(|s| s.as_f64().expect("score"))
            .collect();
        for (a, b) in scores.iter().zip(&want_scores) {
            assert!((*a as f64 - b).abs() < 1e-6, "#{i} 的分数：{a} vs {b}");
        }
        total += want.len();
    }
    assert!(total >= 15, "参考数据里的框太少：{total}");
}
