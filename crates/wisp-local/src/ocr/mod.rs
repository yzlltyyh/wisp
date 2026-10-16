//! 文字识别引擎：本地 PP-OCRv6（ORT 工作进程）、本地 PaddleOCR-VL-1.6（版面检测 + llama-server）、PaddleOCR 云端。
//!
//! | 引擎 | lines | paragraphs | markdown | 直接翻译 |
//! |---|---|---|---|---|
//! | PP-OCRv6 Tiny / Small | ✓ | ✓（几何合并） | ✗（后端兜底按 paragraphs） | 两步：识别 → 翻译引擎 |
//! | PaddleOCR-VL-1.6 | ✓ | ✓ | ✓（版面 + 分块识别） | 两步 |
//! | 云端 PP-OCRv6 / v5 | ✓ | ✓ | ✗ | 两步 |
//! | 云端 PP-StructureV3 / PaddleOCR-VL | ✓ | ✓ | ✓（服务返回的 Markdown） | 两步 |
//!
//! 引擎只产出文本；后处理、16 ms 合并、无文字判定、识别历史都由编排器的泵完成（与视觉 LLM 相同）。

pub mod assemble;
pub mod cloud;
pub mod layout;
pub mod otsl;
#[cfg(feature = "ort")]
pub mod ppocr;
pub mod vl;

use serde::{Deserialize, Serialize};
use wisp_types::{LocalModelId, OcrMode};

/// 模型目录（`<models>/<model-id>/`）里推理要用的文件的相对路径。目录（`catalog`）登记的文件名必须与这里一致：
/// ORT 工作进程不经请求传路径，按 `WISP_ORT_MODELS` + 模型 id + 这些名字查找
pub mod files {
    /// PP-OCRv6 检测模型（`PP-OCRv6_<tier>_det_onnx/inference.onnx`）
    pub const PPOCR_DET: &str = "det.onnx";
    /// PP-OCRv6 识别模型（`PP-OCRv6_<tier>_rec_onnx/inference.onnx`）
    pub const PPOCR_REC: &str = "rec.onnx";
    /// PP-OCRv6 识别模型的配置（`PP-OCRv6_<tier>_rec_onnx/inference.yml`，内嵌字典）
    pub const PPOCR_REC_CONFIG: &str = "rec.yml";
    /// PP-DocLayoutV3 版面模型（`PP-DocLayoutV3_onnx/inference.onnx`，随 PaddleOCR-VL-1.6 一起下载）
    pub const LAYOUT: &str = "layout.onnx";
}

/// 一行识别结果（PP-OCR 的一个文本框；云端 PP-OCRv6 的结果也换成这个形状）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextLine {
    pub text: String,
    /// 识别置信度（0–1）
    pub score: f32,
    /// 四个顶点（原图像素坐标），顺序为左上、右上、右下、左下
    pub poly: [[f32; 2]; 4],
}

/// PP-OCRv6 的档位
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PpOcrTier {
    Tiny,
    Small,
}

impl PpOcrTier {
    /// 对应的离线模型
    pub const fn model(self) -> LocalModelId {
        match self {
            Self::Tiny => LocalModelId::PpOcrV6Tiny,
            Self::Small => LocalModelId::PpOcrV6Small,
        }
    }

    /// 由离线模型得到档位（不是 PP-OCRv6 时为 None）
    pub const fn of(model: LocalModelId) -> Option<Self> {
        match model {
            LocalModelId::PpOcrV6Tiny => Some(Self::Tiny),
            LocalModelId::PpOcrV6Small => Some(Self::Small),
            LocalModelId::PaddleOcrVl16 | LocalModelId::IndexTranslate2b | LocalModelId::IndexTranslate9b => None,
        }
    }
}

/// 某个引擎是否支持某种 OCR 模式（`translate` 一律不直接支持：直接翻译走「识别 → 翻译」两步）
pub const fn local_supports(model: LocalModelId, mode: OcrMode) -> bool {
    match mode {
        OcrMode::Lines | OcrMode::Paragraphs => true,
        OcrMode::Markdown => matches!(model, LocalModelId::PaddleOcrVl16),
        OcrMode::Translate => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_and_modes() {
        for tier in [PpOcrTier::Tiny, PpOcrTier::Small] {
            assert_eq!(PpOcrTier::of(tier.model()), Some(tier));
            assert!(!local_supports(tier.model(), OcrMode::Markdown));
        }
        assert!(local_supports(LocalModelId::PaddleOcrVl16, OcrMode::Markdown));
        assert!(!local_supports(LocalModelId::PaddleOcrVl16, OcrMode::Translate));
        assert_eq!(PpOcrTier::of(LocalModelId::IndexTranslate2b), None);
    }
}
