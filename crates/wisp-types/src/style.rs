//! 翻译风格、术语表与任务类型。

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};

use crate::clip::TransformId;
use crate::llm::ModelRef;
use crate::macros::str_enum;

/// 输出格式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    #[default]
    PlainText,
    MarkdownLite,
}

/// 自定义风格的默认温度
pub const CUSTOM_STYLE_TEMPERATURE: f32 = 0.3;

fn default_dictionary_mode() -> bool {
    true
}

fn default_custom_temperature() -> Option<f32> {
    Some(CUSTOM_STYLE_TEMPERATURE)
}

/// 翻译风格（内置风格的权威版本在 wisp-prompts 代码里）。
///
/// 除 `id` 外的字段都容忍缺失（styles.json 的 `custom` 可被手改）。缺失时取**自定义风格的默认值**
/// （`dictionary_mode = true`、`output = plain_text`、`temperature = 0.3`，见 [`Style::custom`]），
/// 其余取空串 / false / null。`temperature` 键**缺失**取 0.3，显式写 `null` 才表示「不发 temperature」。
/// `builtin` 与 `modified` 由程序在加载时重新计算，文件里的值不可信。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Style {
    /// 内置：natural/technical/academic/casual/learn/literal；自定义："custom-<uuid8>"
    pub id: String,
    /// 内置风格为 ""，前端按 styles.<id> 显示
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub builtin: bool,
    #[serde(default)]
    pub system_template: String,
    #[serde(default)]
    pub tail_instruction: String,
    /// 缺失时 0.3；显式 null = 不发送
    #[serde(default = "default_custom_temperature")]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub output: OutputFormat,
    /// 缺失时 true
    #[serde(default = "default_dictionary_mode")]
    pub dictionary_mode: bool,
    #[serde(default)]
    pub model_ref: Option<ModelRef>,
    #[serde(default)]
    pub hotkey: Option<String>,
    /// 内置风格是否存在 override（序列化时由程序计算）
    #[serde(default)]
    pub modified: bool,
}

impl Style {
    /// 以自定义风格的默认值构造一个：`dictionary_mode = true`、`PlainText`、`temperature = 0.3`，
    /// 模板为空（新建时由调用方填入，例如 tail 默认用 natural 的），不绑定模型与快捷键。
    /// 与反序列化时缺失字段的取值一致
    pub fn custom(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            builtin: false,
            system_template: String::new(),
            tail_instruction: String::new(),
            temperature: default_custom_temperature(),
            output: OutputFormat::PlainText,
            dictionary_mode: default_dictionary_mode(),
            model_ref: None,
            hotkey: None,
            modified: false,
        }
    }
}

/// 内置风格的覆写（只保存与默认值不同的字段；未覆写的字段写盘时省略）。
///
/// 双层 `Option` 的语义：外层 `None` = 不覆写（JSON 中缺省该键）；`Some(None)` = 覆写为空
/// （JSON 中为 `null`，例如「不发 temperature」）；`Some(Some(v))` = 覆写为 v。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StyleOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_template: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tail_instruction: Option<String>,
    #[serde(deserialize_with = "double_option", skip_serializing_if = "Option::is_none")]
    pub temperature: Option<Option<f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dictionary_mode: Option<bool>,
    #[serde(deserialize_with = "double_option", skip_serializing_if = "Option::is_none")]
    pub model_ref: Option<Option<ModelRef>>,
    #[serde(deserialize_with = "double_option", skip_serializing_if = "Option::is_none")]
    pub hotkey: Option<Option<String>>,
}

impl StyleOverride {
    /// 是否没有任何覆写（此时应从 styles.json 中删除该条）
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// 键存在时（包括值为 null）一律读成外层 `Some`，以区分「缺省」与「显式置空」
fn double_option<'de, D, T>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(de).map(Some)
}

crate::lenient::lenient_struct! {
    /// `styles.json`（宽松反序列化，规则见 `crate::lenient`：非法的 order 项 / override / 自定义风格只丢弃那一条）
    #[derive(Debug, Clone, PartialEq, Serialize)]
    pub struct StylesFile {
        pub schema_version: u32,
        pub order: Vec<String> => crate::lenient::vec,
        pub overrides: BTreeMap<String, StyleOverride> => crate::lenient::map,
        pub custom: Vec<Style> => crate::lenient::vec,
    }
}

impl Default for StylesFile {
    fn default() -> Self {
        Self {
            schema_version: 1,
            order: Vec::new(),
            overrides: BTreeMap::new(),
            custom: Vec::new(),
        }
    }
}

/// 术语表条目。只用 `source` 匹配原文；`note` 只在界面上显示
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GlossaryEntry {
    pub source: String,
    pub target: String,
    #[serde(default)]
    pub note: Option<String>,
}

crate::lenient::lenient_struct! {
    /// `glossary.json`（宽松反序列化：非法条目只丢弃那一条）
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub struct GlossaryFile {
        pub schema_version: u32,
        pub entries: Vec<GlossaryEntry> => crate::lenient::vec,
    }
}

impl Default for GlossaryFile {
    fn default() -> Self {
        Self {
            schema_version: 1,
            entries: Vec::new(),
        }
    }
}

/// OCR 模式（translate = 直接翻译）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OcrMode {
    #[default]
    Lines,
    Paragraphs,
    Markdown,
    Translate,
}

impl OcrMode {
    /// 识别结果能否切换排版：只有逐行与段落。结构化结果里的空行与行尾两个空格是 Markdown 语法，直接翻译的输出是译文，
    /// 两者都原样交付
    pub const fn has_layout(self) -> bool {
        matches!(self, Self::Lines | Self::Paragraphs)
    }
}

str_enum! {
    /// OCR 结果的排版：在识别文本上做的一次纯文本处理，只在逐行 / 段落模式下生效（[`OcrMode::has_layout`]）。
    /// 每个选项都作用在识别文本本身，不叠加在上一次的排版结果上。除「原样」外，每个选项都是「粘贴为…」里的一种变换
    /// （[`OcrLayout::transform`]），规则只有剪贴板变换注册表那一份
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
    pub enum OcrLayout {
        /// 原样：保留识别出的换行与空行
        #[default]
        Keep = "keep",
        /// 去掉空行，其余各行原样保留
        RemoveBlankLines = "remove_blank_lines",
        /// 合并为一行：会丢掉分段，只能在结果面板里按需切换，不能设为默认
        JoinLines = "join_lines",
    }
}

impl OcrLayout {
    /// 对应的「粘贴为…」变换；「原样」不做处理，为 None
    pub const fn transform(self) -> Option<TransformId> {
        match self {
            Self::Keep => None,
            Self::RemoveBlankLines => Some(TransformId::RemoveBlankLines),
            Self::JoinLines => Some(TransformId::JoinLines),
        }
    }

    /// 能否作为设置里的默认排版。合并为一行会丢掉全部分段，每次识别都自动这样做容易误伤，所以只能按需切换
    pub const fn can_be_default(self) -> bool {
        !matches!(self, Self::JoinLines)
    }
}

/// 文本任务类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    #[default]
    Translate,
    Polish,
    Proofread,
    Refine,
}

/// 词典式开关
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DictMode {
    #[default]
    Auto,
    Dictionary,
    Prose,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ocr_layouts_map_to_transforms_and_only_lossless_ones_can_be_default() {
        for l in OcrLayout::ALL {
            assert_eq!(serde_json::to_value(l).expect("json"), l.as_str());
        }
        assert_eq!(OcrLayout::default(), OcrLayout::Keep);
        assert_eq!(OcrLayout::Keep.transform(), None);
        // 除「原样」外，排版的取值就是对应变换的取值（前端与 `text_transform` 共用这一个 id）
        for l in OcrLayout::ALL.into_iter().filter(|l| *l != OcrLayout::Keep) {
            assert_eq!(l.transform().map(TransformId::as_str), Some(l.as_str()), "{l:?}");
        }
        let defaults: Vec<OcrLayout> = OcrLayout::ALL.into_iter().filter(|l| l.can_be_default()).collect();
        assert_eq!(defaults, [OcrLayout::Keep, OcrLayout::RemoveBlankLines]);
    }

    #[test]
    fn only_lines_and_paragraphs_have_a_layout() {
        assert!(OcrMode::Lines.has_layout());
        assert!(OcrMode::Paragraphs.has_layout());
        assert!(!OcrMode::Markdown.has_layout());
        assert!(!OcrMode::Translate.has_layout());
    }
}
