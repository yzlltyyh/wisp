//! 语言类型。

use serde::{Deserialize, Serialize};

/// 翻译语言（检测结果与目标语言）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Lang {
    #[serde(rename = "zh-CN")]
    ZhCn,
    #[serde(rename = "en")]
    En,
    #[serde(rename = "ja")]
    Ja,
    #[serde(rename = "ko")]
    Ko,
    #[serde(rename = "other")]
    Other,
}

impl Lang {
    /// 序列化后的字符串（与 serde 一致）
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ZhCn => "zh-CN",
            Self::En => "en",
            Self::Ja => "ja",
            Self::Ko => "ko",
            Self::Other => "other",
        }
    }
}

/// 界面语言偏好
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
pub enum UiLanguage {
    /// 跟随系统：系统语言为 zh-Hans / zh-CN / zh-SG 时用 zh-CN，否则 en-US
    #[default]
    #[serde(rename = "system")]
    System,
    #[serde(rename = "zh-CN")]
    ZhCn,
    #[serde(rename = "en-US")]
    EnUs,
}
