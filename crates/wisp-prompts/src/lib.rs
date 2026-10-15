//! Wisp 的提示词。
//!
//! 职责：内置风格（代码里的权威版本）与 override 合并；单遍、白名单式的模板渲染；翻译、词典式、polish、
//! proofread、refine、OCR 的提示词装配；语言检测与方向判定；词典式触发判定；术语匹配；token 估算与最大输出公式；
//! 流式安全的后处理（翻译 / 编辑 / OCR 三种）；缓存键；评测样本 S1–S16 与评测运行器。
//!
//! 模板资产位于 `templates/`，用 `include_str!` 编译进二进制。另有：
//! [`build_variant`] / [`TemplateVariant`] / [`template`]（语言变体接口）、[`unknown_variables`]（自定义风格的
//! 未知变量提示）、[`BUILTIN_STYLE_IDS`] / [`is_builtin_style_id`] / [`diff_override`]。

mod assemble;
mod cache_key;
mod dict;
mod glossary;
mod hash;
mod lang;
mod post;
mod render;
mod script;
mod styles;
mod templates;
mod tokens;

#[cfg(feature = "eval")]
pub mod eval;

pub use assemble::{BuiltPrompt, PromptCtx, PromptTask, build, build_variant};
pub use cache_key::{CacheKeyInput, cache_key};
pub use dict::{is_dictionary_input, use_dictionary};
pub use glossary::GlossaryMatcher;
pub use lang::{Detection, detect_lang, target_for};
pub use post::{PostFinish, PostKind, PostProcessor, is_no_text, split_edit_output};
pub use render::{RenderVars, render, unknown_variables};
pub use styles::{
    BUILTIN_STYLE_IDS, apply_override, builtin_styles, diff_override, is_builtin_style_id, resolve_styles,
};
pub use templates::{TemplateVariant, template, template_names};
pub use tokens::{estimate_tokens, max_tokens_translate, refine_eligible};
