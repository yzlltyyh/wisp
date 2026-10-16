//! PP-OCRv6 的检测与识别（oar-ocr 的 det + rec，ONNX Runtime 动态加载）。只在 ORT 工作进程里执行。
//!
//! 参数以官方 OCR 产线为准，全部显式设置（不用 oar-ocr 的默认值）：检测输入 BGR（oar-ocr 在归一化时换通道），
//! `limit_side_len 64 / limit_type min / max_side_limit = ocr.max_edge_px`，`thresh 0.3 / box_thresh 0.6 /
//! unclip_ratio 1.5 / max_candidates 1000`；识别高 48、宽 320–3200、CTC，字典取自识别模型的 `inference.yml`
//! （blank + 字典 + 空格，前后两项由 oar-ocr 补上）；逐行识别（CPU 上批量反而更慢，也与官方黄金样例的口径一致）；
//! 不启用文本行方向分类与文档预处理。
//!
//! 预处理与后处理在 vendor 的 oar-ocr 补丁里按官方产线（PaddleX 3.7 + OpenCV 4.10 + pyclipper）的算术逐步实现：
//! 检测输入边长的 32 对齐用银行家舍入、缩放用 `cv2.resize` 的定点双线性、DB 后处理用 OpenCV 的最小外接矩形 /
//! 多边形填充打分与 pyclipper 的整数外扩、文本行裁剪用 `cv2.warpPerspective` 的定点双三次。逐像素 / 逐框的单测见
//! `tests/ocr_opencv_parity.rs`；与官方产线输出的黄金样例比对见 `tests/ocr_live.rs`（同一份像素输入时文字与框完全一致）。

use std::path::Path;

use image::RgbImage;
use oar_ocr::core::config::{OrtExecutionProvider, OrtSessionConfig};
use oar_ocr::domain::tasks::TextDetectionConfig;
use oar_ocr::oarocr::{OAROCR, OAROCRBuilder};
use oar_ocr::processors::LimitType;

use super::{PpOcrTier, TextLine, files};
use crate::error::{LocalError, RuntimeFailure};

/// 检测输入短边的下限
pub const LIMIT_SIDE_LEN: u32 = 64;
/// 二值化阈值
pub const DET_THRESH: f32 = 0.3;
/// 框的分数阈值
pub const DET_BOX_THRESH: f32 = 0.6;
/// 外扩比例
pub const DET_UNCLIP_RATIO: f32 = 1.5;
/// 候选框上限（官方 `DBPostProcess` 的默认值，产线不覆盖）
pub const DET_MAX_CANDIDATES: usize = 1000;
/// 识别输入的高度
pub const REC_HEIGHT: u32 = 48;
/// 检测输入长边上限的默认值（与 `ocr.max_edge_px` 的默认值相同）
pub const DEFAULT_MAX_SIDE: u32 = 2048;
/// 长边上限的取值范围：太小时检测不到小字，太大时激活内存超过 1 GB
const MAX_SIDE_RANGE: (u32, u32) = (256, 4096);
/// 短边小于这个值的图片不可能有可读的文字，直接返回空结果（也避开检测模型对极小输入的报错）
const MIN_IMAGE_SIDE: u32 = 8;

/// 一个档位的检测 + 识别会话
pub struct PpOcrEngine {
    tier: PpOcrTier,
    det: std::path::PathBuf,
    rec: std::path::PathBuf,
    dict: String,
    threads: u32,
    max_side: u32,
    ocr: OAROCR,
}

impl std::fmt::Debug for PpOcrEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PpOcrEngine")
            .field("tier", &self.tier)
            .field("max_side", &self.max_side)
            .finish_non_exhaustive()
    }
}

impl PpOcrEngine {
    /// 从模型目录加载（ONNX Runtime 环境已由调用方用 `ort::init_from` 建好；阻塞）。文件缺失时为 `NotInstalled`，
    /// 字典或模型读不出来时为 `Protocol`
    pub fn load(model_dir: &Path, tier: PpOcrTier, threads: u32) -> Result<Self, LocalError> {
        let det = model_dir.join(files::PPOCR_DET);
        let rec = model_dir.join(files::PPOCR_REC);
        let config = model_dir.join(files::PPOCR_REC_CONFIG);
        if !det.is_file() || !rec.is_file() || !config.is_file() {
            return Err(LocalError::NotInstalled);
        }
        let yml = std::fs::read_to_string(&config)?;
        let dict =
            parse_character_dict(&yml).ok_or(LocalError::Protocol("the recognition dictionary is unreadable"))?;
        let dict = dict.join("\n");
        let threads = threads.clamp(1, 16);
        let ocr = build(&det, &rec, &dict, threads, DEFAULT_MAX_SIDE)?;
        Ok(Self {
            tier,
            det,
            rec,
            dict,
            threads,
            max_side: DEFAULT_MAX_SIDE,
            ocr,
        })
    }

    /// 档位
    pub fn tier(&self) -> PpOcrTier {
        self.tier
    }

    /// 识别一张图片（RGB8）；`max_side` 为检测输入的长边上限。上限变了（用户改了设置）时重建会话（阻塞）
    pub fn recognize(&mut self, image: RgbImage, max_side: u32) -> Result<Vec<TextLine>, LocalError> {
        let max_side = max_side.clamp(MAX_SIDE_RANGE.0, MAX_SIDE_RANGE.1);
        if max_side != self.max_side {
            self.ocr = build(&self.det, &self.rec, &self.dict, self.threads, max_side)?;
            self.max_side = max_side;
        }
        if image.width().min(image.height()) < MIN_IMAGE_SIDE {
            return Ok(Vec::new());
        }
        let results = self.ocr.predict(vec![image]).map_err(|e| classify(&e.to_string()))?;
        let Some(result) = results.into_iter().next() else {
            return Ok(Vec::new());
        };
        Ok(result
            .text_regions
            .into_iter()
            .filter_map(|r| {
                let poly = quad(&r.bounding_box.points)?;
                Some(TextLine {
                    text: r.text.as_deref().unwrap_or_default().to_owned(),
                    score: r.confidence.unwrap_or(0.0),
                    poly,
                })
            })
            .collect())
    }
}

/// 建 oar-ocr 的产线。会话：只用 CPU（CoreML 对这两档更慢），关闭内存 arena 与 memory pattern（动态尺寸下每种尺寸都会
/// 缓存一份内存规划），关闭线程忙等；intra 线程 = `threads`，inter = 1
fn build(det: &Path, rec: &Path, dict: &str, threads: u32, max_side: u32) -> Result<OAROCR, LocalError> {
    let session = OrtSessionConfig::new()
        .with_execution_providers(vec![OrtExecutionProvider::CPU])
        .with_intra_threads(threads as usize)
        .with_inter_threads(1)
        .with_memory_pattern(false)
        .add_config_entry("session.intra_op.allow_spinning", "0")
        .add_config_entry("session.inter_op.allow_spinning", "0");
    let det_config = TextDetectionConfig {
        score_threshold: DET_THRESH,
        box_threshold: DET_BOX_THRESH,
        unclip_ratio: DET_UNCLIP_RATIO,
        max_candidates: DET_MAX_CANDIDATES,
        limit_side_len: Some(LIMIT_SIDE_LEN),
        limit_type: Some(LimitType::Min),
        max_side_len: Some(max_side),
    };
    OAROCRBuilder::new(det.to_path_buf(), rec.to_path_buf(), "")
        .character_dict_content(dict)
        .text_detection_config(det_config)
        .ort_session(session)
        .image_batch_size(1)
        .region_batch_size(1)
        .build()
        .map_err(|e| match classify(&e.to_string()) {
            LocalError::Runtime(RuntimeFailure::OutOfMemory) => LocalError::Runtime(RuntimeFailure::OutOfMemory),
            _ => LocalError::Protocol("cannot load the text recognition models"),
        })
}

/// 推理错误的归类：内存分配失败 → `OutOfMemory`，其余 → `Protocol`。错误原文只在这里看一眼，不外传（可能含路径）
fn classify(message: &str) -> LocalError {
    let m = message.to_ascii_lowercase();
    if [
        "bad_alloc",
        "bad allocation",
        "failed to allocate",
        "out of memory",
        "memory allocation",
    ]
    .iter()
    .any(|k| m.contains(k))
    {
        LocalError::Runtime(RuntimeFailure::OutOfMemory)
    } else {
        LocalError::Protocol("text recognition failed")
    }
}

/// oar-ocr 的框 → 四个顶点（左上、右上、右下、左下）。不是四边形时退回外接矩形
fn quad(points: &[oar_ocr::processors::Point]) -> Option<[[f32; 2]; 4]> {
    if points.is_empty() {
        return None;
    }
    if let [a, b, c, d] = points {
        return Some([[a.x, a.y], [b.x, b.y], [c.x, c.y], [d.x, d.y]]);
    }
    let (mut x0, mut y0, mut x1, mut y1) = (f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY);
    for p in points {
        x0 = x0.min(p.x);
        y0 = y0.min(p.y);
        x1 = x1.max(p.x);
        y1 = y1.max(p.y);
    }
    Some([[x0, y0], [x1, y0], [x1, y1], [x0, y1]])
}

/// 从识别模型的 `inference.yml` 取出字典（`PostProcess.character_dict` 列表，顺序即类别号减一）。
///
/// 只解析这一个列表，不引入 YAML 依赖：PaddleX 导出的文件用 PyYAML 写成，列表项是单行的普通标量、单引号标量（`''` 表示
/// 一个单引号）或双引号标量（反斜杠转义）。读不出来或列表为空时为 None
pub fn parse_character_dict(yml: &str) -> Option<Vec<String>> {
    let mut lines = yml.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));
    let key_indent = loop {
        let line = lines.next()?;
        let trimmed = line.trim_start();
        if trimmed.trim_end() == "character_dict:" {
            break line.len() - trimmed.len();
        }
    };
    let mut dict = Vec::new();
    for line in lines {
        let trimmed = line.trim_start_matches(' ');
        let indent = line.len() - trimmed.len();
        let Some(item) = trimmed.strip_prefix("- ") else {
            break;
        };
        if indent < key_indent {
            break;
        }
        dict.push(yaml_scalar(item)?);
    }
    (!dict.is_empty()).then_some(dict)
}

/// 一个单行 YAML 标量
fn yaml_scalar(s: &str) -> Option<String> {
    if let Some(inner) = s.strip_prefix('\'') {
        return Some(inner.strip_suffix('\'')?.replace("''", "'"));
    }
    if let Some(inner) = s.strip_prefix('"') {
        let inner = inner.strip_suffix('"')?;
        let mut out = String::new();
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            let hex = |chars: &mut std::str::Chars<'_>, n: usize| -> Option<char> {
                let digits: String = chars.by_ref().take(n).collect();
                (digits.len() == n).then(|| u32::from_str_radix(&digits, 16).ok().and_then(char::from_u32))?
            };
            out.push(match chars.next()? {
                '0' => '\0',
                'a' => '\u{7}',
                'b' => '\u{8}',
                't' | '\t' => '\t',
                'n' => '\n',
                'v' => '\u{b}',
                'f' => '\u{c}',
                'r' => '\r',
                'e' => '\u{1b}',
                ' ' => ' ',
                '"' => '"',
                '/' => '/',
                '\\' => '\\',
                'N' => '\u{85}',
                '_' => '\u{a0}',
                'L' => '\u{2028}',
                'P' => '\u{2029}',
                'x' => hex(&mut chars, 2)?,
                'u' => hex(&mut chars, 4)?,
                'U' => hex(&mut chars, 8)?,
                _ => return None,
            });
        }
        return Some(out);
    }
    (!s.is_empty()).then(|| s.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const YML: &str = "Global:\n  model_name: PP-OCRv6_tiny_rec\nPostProcess:\n  name: CTCLabelDecode\n  character_dict:\n  - '!'\n  - '\"'\n  - ''''\n  - $\n  - '-'\n  - A\n  - 中\n  - \"\\u3000\"\n  - \"\\\\\"\n  - \"\\x41\\t\"\nPreProcess:\n  transform_ops: []\n";

    #[test]
    fn dictionary_from_inference_yml() {
        let dict = parse_character_dict(YML).expect("dict");
        assert_eq!(dict, ["!", "\"", "'", "$", "-", "A", "中", "\u{3000}", "\\", "A\t"]);
    }

    #[test]
    fn dictionary_with_crlf_and_end_of_file() {
        let yml = "PostProcess:\r\n  character_dict:\r\n  - a\r\n  - b\r\n";
        assert_eq!(parse_character_dict(yml).expect("dict"), ["a", "b"]);
    }

    #[test]
    fn missing_or_broken_dictionary_is_none() {
        assert_eq!(parse_character_dict("Global:\n  model_name: x\n"), None);
        assert_eq!(
            parse_character_dict("PostProcess:\n  character_dict:\nPreProcess: {}\n"),
            None
        );
        assert_eq!(
            parse_character_dict("PostProcess:\n  character_dict:\n  - 'unterminated\n"),
            None
        );
        assert_eq!(
            parse_character_dict("PostProcess:\n  character_dict:\n  - \"\\q\"\n"),
            None
        );
    }

    #[test]
    fn quads_and_error_classes() {
        use oar_ocr::processors::Point;
        let p = |x, y| Point { x, y };
        assert_eq!(
            quad(&[p(0.0, 0.0), p(10.0, 0.0), p(10.0, 5.0), p(0.0, 5.0)]),
            Some([[0.0, 0.0], [10.0, 0.0], [10.0, 5.0], [0.0, 5.0]])
        );
        assert_eq!(
            quad(&[p(1.0, 2.0), p(9.0, 1.0), p(8.0, 7.0)]),
            Some([[1.0, 1.0], [9.0, 1.0], [9.0, 7.0], [1.0, 7.0]])
        );
        assert_eq!(quad(&[]), None);
        assert!(matches!(
            classify("onnxruntime: std::bad_alloc"),
            LocalError::Runtime(RuntimeFailure::OutOfMemory)
        ));
        assert!(matches!(classify("shape mismatch"), LocalError::Protocol(_)));
    }
}
