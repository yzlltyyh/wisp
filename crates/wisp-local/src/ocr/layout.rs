//! PP-DocLayoutV3 版面检测：推理在 ORT 工作进程里执行（feature `ort`，[`LayoutModel`]），后处理与阅读顺序是纯函数，
//! 在哪一侧都能用。
//!
//! 后处理照 PaddleX 的版面产线（PaddleOCR-VL-1.6 的配置）：阈值 0.3，NMS 同类 0.6 / 异类 0.98，去掉覆盖整页的图片框，
//! 图表、公式、标题这几类大框吞并它们包住的小框，按模型输出的第 7 列（阅读顺序）排序，再去掉互相大面积重叠的框；
//! 页眉、页脚、图片、表格等不参与编号（`order` 为 None，与云端结果的 `block_order: null` 一致）。类别到提示词的映射见
//! [`super::vl`]。

use std::future::Future;
use std::sync::Arc;

use image::RgbaImage;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::error::LocalError;

/// 检测阈值
pub const SCORE_THRESHOLD: f32 = 0.3;
/// 同类框的 NMS 阈值
pub const NMS_SAME: f32 = 0.6;
/// 异类框的 NMS 阈值
pub const NMS_OTHER: f32 = 0.98;
/// 模型输入的边长（直接缩放到 800 × 800，不保持比例）
pub const INPUT_SIZE: u32 = 800;

/// 模型的类别表（`inference.yml` 的 `label_list`，按类别号）
pub const LABELS: [&str; 25] = [
    "abstract",
    "algorithm",
    "aside_text",
    "chart",
    "content",
    "display_formula",
    "doc_title",
    "figure_title",
    "footer",
    "footer_image",
    "footnote",
    "formula_number",
    "header",
    "header_image",
    "image",
    "inline_formula",
    "number",
    "paragraph_title",
    "reference",
    "reference_content",
    "seal",
    "table",
    "text",
    "vertical_text",
    "vision_footnote",
];

/// 不参与阅读顺序编号的类别（PaddleX 的 `SKIP_ORDER_LABELS`）
pub const SKIP_ORDER_LABELS: [&str; 11] = [
    "figure_title",
    "vision_footnote",
    "image",
    "chart",
    "table",
    "header",
    "header_image",
    "footer",
    "footer_image",
    "footnote",
    "aside_text",
];

/// 吞并被自己包住的框的类别（PaddleOCR-VL-1.6 配置里 `layout_merge_bboxes_mode` 为 `large` 的几类）
const LARGE_MERGE_LABELS: [&str; 5] = [
    "chart",
    "display_formula",
    "doc_title",
    "inline_formula",
    "paragraph_title",
];

/// 一个版面区域
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutBox {
    /// 模型的类别名（`text`、`table`、`display_formula`、`image`、`header`……）
    pub label: String,
    pub score: f32,
    /// `[x1, y1, x2, y2]`（原图像素坐标）
    pub bbox: [f32; 4],
    /// 阅读顺序（后处理前是模型输出的顺序号；后处理后从 1 起连续编号，不参与编号的类别为 None）
    pub order: Option<u32>,
}

/// 版面检测器：主进程侧由 [`crate::worker::OrtWorker`] 实现（经工作进程推理），测试里换成假的
pub trait LayoutDetector: Send + Sync {
    /// 检测一张图片的版面区域（已做后处理与排序）
    fn detect(
        &self,
        image: Arc<RgbaImage>,
        cancel: CancellationToken,
    ) -> impl Future<Output = Result<Vec<LayoutBox>, LocalError>> + Send;
}

/// 对模型的原始输出做后处理（阈值、NMS、去整页图片框、大类吞并、排序、去重叠、重新编号）。`width` / `height` 是原图尺寸
pub fn postprocess(raw: Vec<LayoutBox>, width: u32, height: u32) -> Vec<LayoutBox> {
    let (w, h) = (width as f32, height as f32);

    // 坐标取整（numpy 的 round 是四舍六入五成双）；阈值
    let mut boxes: Vec<LayoutBox> = raw
        .into_iter()
        .filter(|b| {
            b.score > SCORE_THRESHOLD && LABELS.contains(&b.label.as_str()) && b.bbox.iter().all(|v| v.is_finite())
        })
        .map(|mut b| {
            b.bbox = b.bbox.map(f32::round_ties_even);
            b
        })
        .collect();

    boxes = nms(boxes);

    // 覆盖几乎整页的图片框通常是把整张截图当成了「图片」，去掉（全是这种框时保留原样）
    if boxes.len() > 1 {
        let ratio = if w > h { 0.82 } else { 0.93 };
        let page = w * h;
        let kept: Vec<LayoutBox> = boxes
            .iter()
            .filter(|b| {
                if b.label != "image" {
                    return true;
                }
                let [x1, y1, x2, y2] = b.bbox;
                let area = (x2.min(w) - x1.max(0.0)) * (y2.min(h) - y1.max(0.0));
                area <= ratio * page
            })
            .cloned()
            .collect();
        if !kept.is_empty() {
            boxes = kept;
        }
    }

    // 大类吞并：被这几类的框包住（自身面积的 90% 以上在里面）的框去掉
    let contained: Vec<bool> = (0..boxes.len())
        .map(|i| {
            boxes.iter().enumerate().any(|(j, outer)| {
                j != i
                    && LARGE_MERGE_LABELS.contains(&outer.label.as_str())
                    && is_contained(&boxes[i].bbox, &outer.bbox)
            })
        })
        .collect();
    let mut boxes: Vec<LayoutBox> = boxes
        .into_iter()
        .zip(contained)
        .filter_map(|(b, c)| (!c).then_some(b))
        .collect();

    // 按模型给出的阅读顺序排序
    boxes.sort_by_key(|b| b.order.unwrap_or(u32::MAX));

    // 裁到图内、去掉退化的框
    let mut boxes: Vec<LayoutBox> = boxes
        .into_iter()
        .filter_map(|mut b| {
            let [x1, y1, x2, y2] = b.bbox;
            b.bbox = [
                x1.max(0.0).trunc(),
                y1.max(0.0).trunc(),
                x2.min(w).trunc(),
                y2.min(h).trunc(),
            ];
            (b.bbox[2] > b.bbox[0] && b.bbox[3] > b.bbox[1]).then_some(b)
        })
        .collect();

    boxes = filter_overlaps(boxes);

    let mut next = 1;
    for b in &mut boxes {
        if SKIP_ORDER_LABELS.contains(&b.label.as_str()) {
            b.order = None;
        } else {
            b.order = Some(next);
            next += 1;
        }
    }
    boxes
}

/// 按分数从高到低的 NMS（同类与异类阈值不同；IoU 照 PaddleX 的像素口径，边长加 1）
fn nms(mut boxes: Vec<LayoutBox>) -> Vec<LayoutBox> {
    boxes.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut kept: Vec<LayoutBox> = Vec::new();
    for b in boxes {
        let suppressed = kept.iter().any(|k| {
            let threshold = if k.label == b.label { NMS_SAME } else { NMS_OTHER };
            pixel_iou(&k.bbox, &b.bbox) >= threshold
        });
        if !suppressed {
            kept.push(b);
        }
    }
    kept
}

fn pixel_iou(a: &[f32; 4], b: &[f32; 4]) -> f32 {
    let iw = (a[2].min(b[2]) - a[0].max(b[0]) + 1.0).max(0.0);
    let ih = (a[3].min(b[3]) - a[1].max(b[1]) + 1.0).max(0.0);
    let inter = iw * ih;
    let area = |r: &[f32; 4]| (r[2] - r[0] + 1.0) * (r[3] - r[1] + 1.0);
    let union = area(a) + area(b) - inter;
    if union > 0.0 { inter / union } else { 0.0 }
}

/// 交集面积
fn intersection(a: &[f32; 4], b: &[f32; 4]) -> f32 {
    (a[2].min(b[2]) - a[0].max(b[0])).max(0.0) * (a[3].min(b[3]) - a[1].max(b[1])).max(0.0)
}

fn area(r: &[f32; 4]) -> f32 {
    ((r[2] - r[0]) * (r[3] - r[1])).abs()
}

/// `inner` 的面积至少 90% 落在 `outer` 里
fn is_contained(inner: &[f32; 4], outer: &[f32; 4]) -> bool {
    let a = area(inner);
    a > 0.0 && intersection(inner, outer) / a >= 0.9
}

/// 去掉互相大面积重叠的框（PaddleX 的 `filter_overlap_boxes`，矩形口径）：参考文献框不要；边长不足 6 像素的不要；
/// 行内公式与别的框重叠过半时去掉行内公式；其余重叠超过较小者面积 70% 时去掉较小的（图片、表格、印章、图表与别的类别
/// 重叠时都保留，表格与文字类重叠时仍按面积取舍）
fn filter_overlaps(boxes: Vec<LayoutBox>) -> Vec<LayoutBox> {
    let boxes: Vec<LayoutBox> = boxes.into_iter().filter(|b| b.label != "reference").collect();
    let mut dropped = vec![false; boxes.len()];
    let visual = ["image", "table", "seal", "chart"];
    for i in 0..boxes.len() {
        let [x1, y1, x2, y2] = boxes[i].bbox;
        if x2 - x1 < 6.0 || y2 - y1 < 6.0 {
            dropped[i] = true;
        }
        for j in i + 1..boxes.len() {
            if dropped[i] || dropped[j] {
                continue;
            }
            let (a, b) = (&boxes[i], &boxes[j]);
            let small = area(&a.bbox).min(area(&b.bbox));
            let overlap = if small > 0.0 {
                intersection(&a.bbox, &b.bbox) / small
            } else {
                0.0
            };
            if (a.label == "inline_formula" || b.label == "inline_formula") && overlap > 0.5 {
                dropped[i] |= a.label == "inline_formula";
                dropped[j] |= b.label == "inline_formula";
                continue;
            }
            if overlap > 0.7 {
                let differ = a.label != b.label;
                let any_visual = visual.contains(&a.label.as_str()) || visual.contains(&b.label.as_str());
                if differ && any_visual {
                    let has_table = a.label == "table" || b.label == "table";
                    let all_visual = visual.contains(&a.label.as_str()) && visual.contains(&b.label.as_str());
                    if !has_table || all_visual {
                        continue;
                    }
                }
                if area(&a.bbox) >= area(&b.bbox) {
                    dropped[j] = true;
                } else {
                    dropped[i] = true;
                }
            }
        }
    }
    boxes
        .into_iter()
        .zip(dropped)
        .filter_map(|(b, d)| (!d).then_some(b))
        .collect()
}

/// PP-DocLayoutV3 的推理会话（只在 ORT 工作进程里使用）
#[cfg(feature = "ort")]
pub struct LayoutModel {
    session: ort::session::Session,
}

#[cfg(feature = "ort")]
impl std::fmt::Debug for LayoutModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LayoutModel")
    }
}

#[cfg(feature = "ort")]
impl LayoutModel {
    /// 加载模型（ONNX Runtime 环境已由调用方用 `ort::init_from` 建好）。关闭 CPU 内存 arena 与 memory pattern、
    /// 关闭线程忙等：工作进程闲置时不占 CPU，峰值过后内存能还给系统
    pub fn load(model: &std::path::Path, threads: u32) -> Result<Self, LocalError> {
        let fail = |_| LocalError::Protocol("cannot load the layout model");
        let session = ort::session::Session::builder()
            .map_err(|_| LocalError::Protocol("cannot load the layout model"))?
            .with_execution_providers([ort::ep::CPU::default().with_arena_allocator(false).build()])
            .map_err(fail)?
            .with_intra_threads(threads.max(1) as usize)
            .map_err(fail)?
            .with_inter_threads(1)
            .map_err(fail)?
            .with_memory_pattern(false)
            .map_err(fail)?
            .with_intra_op_spinning(false)
            .map_err(fail)?
            .with_inter_op_spinning(false)
            .map_err(fail)?
            .commit_from_file(model)
            .map_err(|_| LocalError::Protocol("cannot load the layout model"))?;
        Ok(Self { session })
    }

    /// 检测一张图片（RGB8）；返回已做后处理与排序的区域
    pub fn detect(&mut self, image: &image::RgbImage) -> Result<Vec<LayoutBox>, LocalError> {
        use ort::value::Tensor;

        let (w, h) = image.dimensions();
        if w == 0 || h == 0 {
            return Ok(Vec::new());
        }
        // 预处理：RGB，双三次缩放到 800 × 800（不保持比例），只除以 255、不做均值方差，HWC → CHW
        let side = INPUT_SIZE as usize;
        let resized = image::imageops::resize(image, INPUT_SIZE, INPUT_SIZE, image::imageops::FilterType::CatmullRom);
        let mut chw = vec![0f32; 3 * side * side];
        for (i, px) in resized.pixels().enumerate() {
            for c in 0..3 {
                chw[c * side * side + i] = f32::from(px.0[c]) / 255.0;
            }
        }
        let infer = |_| LocalError::Protocol("layout inference failed");
        let image_t = Tensor::from_array(([1usize, 3, side, side], chw)).map_err(infer)?;
        let shape_t = Tensor::from_array(([1usize, 2], vec![INPUT_SIZE as f32, INPUT_SIZE as f32])).map_err(infer)?;
        // scale_factor 是 [高方向, 宽方向] 的缩放比
        let scale_t = Tensor::from_array((
            [1usize, 2],
            vec![INPUT_SIZE as f32 / h as f32, INPUT_SIZE as f32 / w as f32],
        ))
        .map_err(infer)?;
        let outputs = self
            .session
            .run(ort::inputs!["image" => image_t, "im_shape" => shape_t, "scale_factor" => scale_t])
            .map_err(infer)?;
        // 第一个输出 [M, 7]：类别号、分数、x1、y1、x2、y2（已映射回原图）、阅读顺序
        let (shape, data) = outputs[0].try_extract_tensor::<f32>().map_err(infer)?;
        let cols = shape.last().copied().unwrap_or(0);
        if cols < 6 {
            return Err(LocalError::Protocol("unexpected layout model output"));
        }
        let cols = cols as usize;
        let raw: Vec<LayoutBox> = data
            .chunks_exact(cols)
            .filter_map(|r| {
                let class = r[0];
                if !(0.0..LABELS.len() as f32).contains(&class) {
                    return None;
                }
                Some(LayoutBox {
                    label: LABELS[class as usize].to_owned(),
                    score: r[1],
                    bbox: [r[2], r[3], r[4], r[5]],
                    order: (cols > 6 && r[6].is_finite() && r[6] >= 0.0).then(|| r[6].round() as u32),
                })
            })
            .collect();
        Ok(postprocess(raw, w, h))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(label: &str, score: f32, bbox: [f32; 4], order: u32) -> LayoutBox {
        LayoutBox {
            label: label.to_owned(),
            score,
            bbox,
            order: Some(order),
        }
    }

    fn labels(boxes: &[LayoutBox]) -> Vec<&str> {
        boxes.iter().map(|b| b.label.as_str()).collect()
    }

    #[test]
    fn threshold_nms_and_reading_order() {
        let raw = vec![
            b("text", 0.9, [10.0, 100.0, 500.0, 200.0], 3),
            b("paragraph_title", 0.8, [10.0, 10.0, 300.0, 40.0], 1),
            // 低于阈值
            b("text", 0.2, [10.0, 300.0, 500.0, 400.0], 4),
            // 与第一个同类、IoU 很高：被 NMS 去掉
            b("text", 0.85, [12.0, 102.0, 498.0, 198.0], 5),
            b("header", 0.7, [10.0, 0.0, 100.0, 5.0], 0),
            b("text", 0.6, [10.0, 50.0, 500.0, 90.0], 2),
        ];
        let out = postprocess(raw, 600, 800);
        assert_eq!(
            labels(&out),
            ["paragraph_title", "text", "text"],
            "页眉高不足 6 像素被去掉"
        );
        assert_eq!(
            out.iter().map(|b| b.order).collect::<Vec<_>>(),
            [Some(1), Some(2), Some(3)]
        );
        assert_eq!(out[1].bbox, [10.0, 50.0, 500.0, 90.0]);
    }

    #[test]
    fn full_page_images_are_dropped_and_skip_labels_have_no_order() {
        let raw = vec![
            b("image", 0.9, [0.0, 0.0, 1000.0, 600.0], 1),
            b("header", 0.8, [10.0, 10.0, 300.0, 40.0], 2),
            b("text", 0.8, [10.0, 100.0, 900.0, 300.0], 3),
            b("table", 0.8, [10.0, 320.0, 900.0, 580.0], 4),
        ];
        let out = postprocess(raw, 1000, 600);
        assert_eq!(labels(&out), ["header", "text", "table"]);
        assert_eq!(out.iter().map(|b| b.order).collect::<Vec<_>>(), [None, Some(1), None]);

        // 只有一张整页图片时保留
        let only = postprocess(vec![b("image", 0.9, [0.0, 0.0, 1000.0, 600.0], 1)], 1000, 600);
        assert_eq!(labels(&only), ["image"]);
    }

    #[test]
    fn large_classes_swallow_contained_boxes() {
        let raw = vec![
            b("display_formula", 0.9, [100.0, 100.0, 400.0, 200.0], 1),
            b("text", 0.5, [120.0, 110.0, 380.0, 190.0], 2),
            b("text", 0.9, [100.0, 300.0, 400.0, 400.0], 3),
        ];
        let out = postprocess(raw, 500, 500);
        assert_eq!(labels(&out), ["display_formula", "text"]);
    }

    #[test]
    fn overlapping_boxes_keep_the_larger_one_but_keep_visual_pairs() {
        let raw = vec![
            b("text", 0.9, [100.0, 100.0, 400.0, 200.0], 1),
            b("content", 0.5, [110.0, 110.0, 390.0, 195.0], 2),
            b("image", 0.9, [100.0, 300.0, 400.0, 450.0], 3),
            b("figure_title", 0.9, [110.0, 310.0, 390.0, 440.0], 4),
            b("inline_formula", 0.9, [500.0, 500.0, 540.0, 520.0], 5),
            b("text", 0.9, [495.0, 495.0, 700.0, 525.0], 6),
            b("reference", 0.9, [10.0, 600.0, 400.0, 700.0], 7),
        ];
        let out = postprocess(raw, 800, 800);
        assert_eq!(labels(&out), ["text", "image", "figure_title", "text"]);
    }

    #[test]
    fn boxes_are_clipped_to_the_image() {
        let out = postprocess(vec![b("text", 0.9, [-5.4, 10.6, 120.5, 999.0], 1)], 100, 200);
        assert_eq!(out[0].bbox, [0.0, 11.0, 100.0, 200.0]);
    }
}
