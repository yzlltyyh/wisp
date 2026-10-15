//! OCR 历史的存储（M2 迁移建表 `ocr_history`）。
//!
//! 图片放 `blobs/ocr/`，文件名是内容 hash（同一张图多次识别共用一个文件，删除时按引用计数决定是否删文件）；
//! 资源路径 `ocrimg/<key>`。只有用户开启 OCR 历史时调用方才会写入；关闭并清空后表与 blobs 都被清理。

use wisp_types::OcrMode;

/// 新增一条 OCR 历史的输入
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OcrHistoryNew<'a> {
    pub created_at: i64,
    pub mode: OcrMode,
    pub model: &'a str,
    pub text: &'a str,
    /// 缩略图（JPEG / PNG 字节）。None 且给了原图时由存储层从原图生成（长边 256 px，与剪贴板缩略图同一规格）
    pub thumb: Option<&'a [u8]>,
    /// 原图字节：PNG，或体积过大时送模型用的 JPEG（截图编码为 JPEG 时也能「回看」）
    pub image: Option<&'a [u8]>,
}

/// OCR 历史的缩略图（长边 256 px，与剪贴板缩略图同一规格），交给 [`OcrHistoryNew::thumb`]：从已经解码好的 RGBA8 裁剪帧
/// （宽 × 高，逐行紧排）直接生成——编排器在编码截图的同一个阻塞任务里调用，写入历史时存储层就不必从送模型用的编码字节
/// 再解码一遍。像素数据与尺寸不符时返回 None（写入时退回由存储层从原图生成）
pub fn ocr_thumbnail(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    match crate::imaging::thumbnail_rgba(width, height, rgba, crate::imaging::THUMB_LONG_EDGE) {
        Ok(t) => Some(t.bytes),
        Err(e) => {
            log::warn!("ocr thumbnail failed: {e}");
            None
        }
    }
}

/// OcrMode ↔ 库中的字符串（与 serde 的 snake_case 一致）
pub(crate) fn mode_to_str(m: OcrMode) -> &'static str {
    match m {
        OcrMode::Lines => "lines",
        OcrMode::Paragraphs => "paragraphs",
        OcrMode::Markdown => "markdown",
        OcrMode::Translate => "translate",
    }
}

/// 未知值退回默认模式（向前兼容：新版本写入的模式被旧版本读到）
pub(crate) fn mode_from_str(s: &str) -> OcrMode {
    match s {
        "paragraphs" => OcrMode::Paragraphs,
        "markdown" => OcrMode::Markdown,
        "translate" => OcrMode::Translate,
        _ => OcrMode::Lines,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_round_trip_matches_serde() {
        for m in [
            OcrMode::Lines,
            OcrMode::Paragraphs,
            OcrMode::Markdown,
            OcrMode::Translate,
        ] {
            assert_eq!(mode_from_str(mode_to_str(m)), m);
            assert_eq!(serde_json::to_value(m).expect("序列化"), mode_to_str(m));
        }
        assert_eq!(mode_from_str("future"), OcrMode::Lines);
    }
}
