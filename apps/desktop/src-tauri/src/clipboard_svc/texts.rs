//! 剪贴板服务自己发出的 Rust 侧文案（原生「另存为」对话框、通知），与 `app/i18n.rs` 同样用 `match (lang, key)` 的静态表。
//!
//! 这几条只有剪贴板服务用，放在本模块；以后可以并入 `app/i18n.rs`。语言的解析仍走唯一的 `app::i18n::resolve`。

use crate::app::i18n::Lang;

/// 文案条目
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Text {
    /// 「另存为」对话框标题
    SaveImageTitle,
    /// 「另存为」对话框的文件类型名
    ImageFilter,
    /// 剪贴板历史因数据库来自更新版本而停用：通知标题
    HistoryTooNewTitle,
    /// 同上：通知正文
    HistoryTooNewBody,
}

impl Text {
    /// 全部条目（单测逐个检查两种语言）
    #[cfg(test)]
    pub(crate) const ALL: [Self; 4] = [
        Self::SaveImageTitle,
        Self::ImageFilter,
        Self::HistoryTooNewTitle,
        Self::HistoryTooNewBody,
    ];
}

/// 取文案
pub(crate) fn tr(lang: Lang, text: Text) -> &'static str {
    use Lang::{EnUs, ZhCn};
    use Text::*;
    match (lang, text) {
        (ZhCn, SaveImageTitle) => "存储图片",
        (EnUs, SaveImageTitle) => "Save Image",
        (ZhCn, ImageFilter) => "PNG 图片",
        (EnUs, ImageFilter) => "PNG Image",
        (ZhCn, HistoryTooNewTitle) => "剪贴板历史已暂停",
        (EnUs, HistoryTooNewTitle) => "Clipboard history is paused",
        (ZhCn, HistoryTooNewBody) => "历史记录由更新版本的 Wisp 创建。请升级 Wisp 后继续使用，已有记录会完好保留。",
        (EnUs, HistoryTooNewBody) => {
            "Your history was created by a newer version of Wisp. Update Wisp to keep using it — existing items are preserved."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_text_has_both_languages() {
        for t in Text::ALL {
            let zh = tr(Lang::ZhCn, t);
            let en = tr(Lang::EnUs, t);
            assert!(!zh.trim().is_empty() && !en.trim().is_empty(), "{t:?}");
            assert_ne!(zh, en, "{t:?}");
            assert!(en.is_ascii() || en.contains('—'), "英文文案混入了中文：{t:?}");
        }
    }
}
