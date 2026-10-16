//! 拖出条目（HTML5 拖拽）时放进拖拽数据的内容：拖拽开始的回调必须同步写数据，前端在按下时经
//! `clipboard_drag_data` 先取好。内容与粘贴同一个口径（`paste.rs`）：一条时是纯文本（文件条目是仍存在的路径，
//! 逐行）、富文本另带 HTML、链接另带地址；多条时按选择顺序以换行合并（与合并粘贴相同）。图片与文件的原生拖出不在
//! 这里（WebView 的拖拽数据只有文本类型能到达别的应用）。

use wisp_types::{ClipDragData, ClipKind};

use crate::error::IpcError;

use super::engine::{Engine, store_error};
use super::paste::{merged_text, paste_payload};

impl Engine {
    /// 拖出 `ids`（按选择顺序）的拖拽数据。**会阻塞**（读库）
    pub(crate) fn drag_data(&self, ids: &[i64]) -> Result<ClipDragData, IpcError> {
        let store = self.open_store("drag")?;
        if let [id] = ids {
            let (kind, _) = store
                .text_of(*id)
                .map_err(store_error)?
                .ok_or_else(|| IpcError::not_found("clipboard item not found"))?;
            if kind == ClipKind::Image {
                return Err(IpcError::invalid_argument("images are not dragged as text"));
            }
            let payload = store
                .get_payload(*id, false)
                .map_err(store_error)?
                .ok_or_else(|| IpcError::not_found("clipboard item is gone"))?;
            let payload = paste_payload(payload, false)?;
            let text = payload.text.unwrap_or_default();
            let uri = (kind == ClipKind::Link)
                .then(|| text.trim().to_owned())
                .filter(|u| !u.is_empty());
            let html = if kind == ClipKind::RichText { payload.html } else { None };
            return Ok(ClipDragData { text, html, uri });
        }
        Ok(ClipDragData {
            text: merged_text(&store, ids)?,
            html: None,
            uri: None,
        })
    }
}
