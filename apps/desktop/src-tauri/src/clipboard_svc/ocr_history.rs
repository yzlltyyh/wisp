//! OCR 历史：写入、保留策略与变化推送。存储在剪贴板历史的同一个
//! 数据库里（`ocr_history` 表 + `blobs/ocr/`，wisp-clipboard），本模块只做策略：
//!
//! - **默认关闭**：只有 `ocr.history` 打开时才写入（编排器在 OCR 请求以 Done 结束、且不是 no_text 时调用
//!   [`Engine::ocr_history_record`]）；关闭开关不删除已有记录（设置页另有「关闭并清空」）；
//! - **保留策略**：每次写入后按 `ocr.history_limit`（10–500，默认 50）裁掉最旧的；调小上限后由空闲清理
//!   （`retention.rs`）补裁。同一张截图用同一模式重新识别时，新结果**取代**旧记录（`replaces`）——用户重新识别通常是因为
//!   上一次的结果不好，留两条只会让列表里出现近乎重复的条目；换了模式的识别是不同的结果，各留一条；
//! - **推送**：每次变化 emit `ocr_history:changed`（`OcrHistoryChanged`）给设置窗口：`inserted`（带完整条目，列表原地
//!   插入）→ 裁剪 / 取代掉的 `removed`；`ocr_history_delete` / `ocr_history_clear` 同样推送（设置窗口自己发起的删除也
//!   会收到，前端按 id 去重）。
//!
//! 原图：截图送模型时的编码（PNG；base64 超过 5 MB 时是 JPEG q90，长边已按 `ocr.max_edge_px` 缩过），与请求共用同一块
//! 内存，写入时不再编码；缩略图（长边 256 px）由编排器在编码截图时从裁剪帧顺手生成（`OcrRecord::thumb`），没有时
//! （例如识别之后才打开历史开关、再重新识别）才由存储层从原图生成。
//!
//! translate 模式（含两次调用的直接翻译）记的是**译文**：这类条目不提供「重新翻译」（把译文再翻一遍没有意义），复制
//! 得到的就是译文。

use std::sync::Arc;

use wisp_clipboard::{OcrHistoryNew, ocr_key};
use wisp_types::{OcrHistoryChanged, OcrMode};

use crate::error::IpcError;

use super::engine::{Engine, store_error};

/// 一次要记进历史的 OCR 结果
#[derive(Debug, Clone)]
pub struct OcrRecord {
    pub mode: OcrMode,
    /// 模型显示名（`StreamEvent::Start.model_label`）
    pub model: String,
    /// 识别结果全文（translate 模式是译文，这类条目不提供「重新翻译」）
    pub text: String,
    /// 原图（截图送模型时的编码字节，PNG 或 JPEG）；没有时只记文字
    pub image: Option<Arc<Vec<u8>>>,
    /// 缩略图（编排器编码截图时从裁剪帧生成的 JPEG / PNG，`wisp_clipboard::ocr_thumbnail`）；None 时由存储层从原图生成
    pub thumb: Option<Arc<Vec<u8>>>,
    /// 被这条记录取代的旧记录（同一张截图、同一模式的上一次识别）
    pub replaces: Option<i64>,
}

impl Engine {
    /// 记一条 OCR 历史并按上限裁剪、推送变化。`ocr.history` 关闭、或结果为空白时什么都不做，返回 None。
    /// **会阻塞**（数据库、写图片文件、从原图生成缩略图，几毫秒到几十毫秒），只在阻塞线程上调用
    pub(crate) fn ocr_history_record(&self, rec: &OcrRecord) -> Result<Option<i64>, IpcError> {
        let settings = self.host.settings();
        if !settings.ocr.history || rec.text.trim().is_empty() {
            return Ok(None);
        }
        let store = self.open_store("ocr_history")?;
        let _ocr = self.ocr_serial.lock();
        let created_at = self.host.now_ms();
        let id = store
            .ocr_history_add(&OcrHistoryNew {
                created_at,
                mode: rec.mode,
                model: &rec.model,
                text: &rec.text,
                thumb: rec.thumb.as_deref().map(Vec::as_slice),
                image: rec.image.as_deref().map(Vec::as_slice),
            })
            .map_err(store_error)?;
        let sink = self.sync_sink();
        if let Some(sink) = &sink {
            sink.ocr_add(&ocr_key(created_at, rec.mode, &rec.text));
        }
        match store.ocr_history_get(id) {
            Ok(Some(item)) => self.host.emit_ocr_history(&OcrHistoryChanged::Inserted { item }),
            Ok(None) => {}
            Err(e) => log::warn!("read back ocr history item failed: {e}"),
        }
        let mut removed = Vec::new();
        if let Some(old) = rec.replaces.filter(|&old| old != id) {
            // 被取代的旧记录对同步而言是一次删除（与用户删除一样写墓碑）；键与删除在同一个事务里取
            match store.ocr_history_delete_keyed(old) {
                Ok(Some(key)) => {
                    removed.push(old);
                    if let Some(sink) = &sink {
                        sink.ocr_delete(&key);
                    }
                }
                Ok(None) => {}
                Err(e) => log::warn!("replace ocr history item failed: {e}"),
            }
        }
        match store.ocr_history_trim(settings.ocr.history_limit) {
            Ok(ids) => removed.extend(ids),
            Err(e) => log::warn!("ocr history trim failed: {e}"),
        }
        if !removed.is_empty() {
            self.host.emit_ocr_history(&OcrHistoryChanged::Removed { ids: removed });
        }
        Ok(Some(id))
    }

    /// 删除一条 OCR 历史并推送 `removed`（id 不存在时不推送）。**会阻塞**
    pub(crate) fn ocr_history_delete(&self, id: i64) -> Result<(), IpcError> {
        let store = self.open_store("ocr_history")?;
        let _ocr = self.ocr_serial.lock();
        if let Some(key) = store.ocr_history_delete_keyed(id).map_err(store_error)? {
            if let Some(sink) = self.sync_sink() {
                sink.ocr_delete(&key);
            }
            self.host
                .emit_ocr_history(&OcrHistoryChanged::Removed { ids: vec![id] });
        }
        Ok(())
    }

    /// 清空 OCR 历史（表与 `blobs/ocr/` 一起清理）并推送 `cleared`。**会阻塞**
    pub(crate) fn ocr_history_clear(&self) -> Result<(), IpcError> {
        let store = self.open_store("ocr_history")?;
        let _ocr = self.ocr_serial.lock();
        store.ocr_history_clear().map_err(store_error)?;
        if let Some(sink) = self.sync_sink() {
            sink.ocr_clear();
        }
        self.host.emit_ocr_history(&OcrHistoryChanged::Cleared);
        Ok(())
    }

    /// 空闲清理里按上限补裁（调小 `ocr.history_limit` 之后），推送 `removed`；返回裁掉的条数
    pub(crate) fn ocr_history_trim(&self, store: &wisp_clipboard::ClipStore, limit: u32) -> u32 {
        match store.ocr_history_trim(limit) {
            Ok(ids) if ids.is_empty() => 0,
            Ok(ids) => {
                let n = u32::try_from(ids.len()).unwrap_or(u32::MAX);
                self.host.emit_ocr_history(&OcrHistoryChanged::Removed { ids });
                n
            }
            Err(e) => {
                log::warn!("ocr history retention failed: {e}");
                0
            }
        }
    }
}
