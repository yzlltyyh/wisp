//! 提示词装配。硬规则只拼到 6 个内置风格、自定义风格和 refine 的改进步骤；
//! 术语表注入翻译和词典式。
//!
//! | 任务 | system | user | temperature | 输出 | 后处理 |
//! | --- | --- | --- | --- | --- | --- |
//! | 翻译 | 风格 + 硬规则（含术语） | 骨架 + 风格 tail | 风格 | 风格 | Translate |
//! | 词典式 | 词典式（technical / casual 追加一行）+ 术语 | 骨架 + 词典 tail | 0.2 | MarkdownLite | Translate |
//! | polish / proofread | 各自模板 | edit.user | 0.4 / 0.2 | MarkdownLite | Edit |
//! | refine 反思 | reflect.system | reflect.user | 0.2 | PlainText | Raw：只 trim（见 [`PromptTask::RefineReflect`]） |
//! | refine 改进 | 风格 + 硬规则（含术语） | improve.user | 0.2 | 风格 | Translate |
//! | OCR 转录 | base + 模式块 | 模式指令 | 0 | PlainText | Ocr |
//! | OCR 直接翻译 | translate | 指令 | 0.2 | PlainText | Ocr |

use std::borrow::Cow;

use wisp_types::{GlossaryEntry, Lang, OcrMode, OutputFormat, Style};

use crate::glossary::render_glossary_block;
use crate::index::PromptProfile;
use crate::lang::{Detection, source_lang_attr, source_lang_name, target_lang_name};
use crate::post::PostKind;
use crate::render::{RenderVars, render_with};
use crate::script::Census;
use crate::templates::{self as t, TemplateVariant, pick};
use crate::tokens::{OCR_MAX_OUTPUT, estimate_from_census, max_tokens_translate};

/// 提示词任务
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PromptTask<'a> {
    Translate {
        style: &'a Style,
        dict: bool,
    },
    Polish,
    Proofread,
    /// refine 第一步：反思。
    ///
    /// 输出是修改意见列表（每行 `"quote" → fix: reason`）或 `OK`，**不是译文**：调用方应非流式聚合原始输出，
    /// 只做 trim——trim 后等于 `OK` 时跳过改进，否则原样作为 [`PromptTask::RefineImprove`] 的 `review`。
    /// 这一步的 [`BuiltPrompt::post`] 是 [`PostKind::Raw`]（只 trim）：翻译后处理会把超过 24 个字符的开头引文当成
    /// 包住全文的引号，连同配对的闭合引号一起去掉，改坏审校意见；`Translation:` 等前缀规则也不适用。
    RefineReflect {
        draft: &'a str,
    },
    RefineImprove {
        style: &'a Style,
        draft: &'a str,
        review: &'a str,
    },
    /// 包含 Translate（直接翻译）模式
    Ocr {
        mode: OcrMode,
    },
}

/// 装配上下文
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PromptCtx<'a> {
    pub text: &'a str,
    pub source: Detection,
    pub target: Lang,
    pub glossary: &'a [GlossaryEntry],
    pub reasoning_model: bool,
}

/// 装配结果
#[derive(Debug, Clone, PartialEq)]
pub struct BuiltPrompt {
    pub system: String,
    pub user_text: String,
    pub temperature: Option<f32>,
    pub max_output_tokens: u32,
    pub output: OutputFormat,
    /// 输出的后处理种类（`PostProcessor::new(post, ctx.text)`）。[`PromptTask::RefineReflect`] 是 `PostKind::Raw`：
    /// 只 trim，审校意见逐字保留（trim 后等于 `OK` 时跳过改进）
    pub post: PostKind,
    /// 是否允许 merge_system_into_user（OCR 只在带图请求时允许）
    pub merge_system_hint: bool,
}

/// 词典式的 temperature（不随风格变化）
const DICTIONARY_TEMPERATURE: f32 = 0.2;
/// polish 的 temperature
const POLISH_TEMPERATURE: f32 = 0.4;
/// proofread 的 temperature
const PROOFREAD_TEMPERATURE: f32 = 0.2;
/// refine 两步的 temperature
const REFINE_TEMPERATURE: f32 = 0.2;
/// OCR 转录模式的 temperature
const OCR_TRANSCRIBE_TEMPERATURE: f32 = 0.0;
/// OCR 直接翻译模式的 temperature
const OCR_TRANSLATE_TEMPERATURE: f32 = 0.2;

/// 装配提示词（p99 ≤ 1 ms）
pub fn build(task: &PromptTask<'_>, ctx: &PromptCtx<'_>) -> BuiltPrompt {
    build_variant(task, ctx, TemplateVariant::En)
}

/// 按档位装配：`IndexTranslate` 档位只改变翻译（含词典式，按普通翻译装配）的提示词，其余任务按标准档位装配
/// （离线翻译模型不承担这些任务，编排器不会这样调用；这里只是不让它们得到一个错误的格式）
pub fn build_profile(task: &PromptTask<'_>, ctx: &PromptCtx<'_>, profile: PromptProfile) -> BuiltPrompt {
    match (profile, task) {
        (PromptProfile::IndexTranslate, PromptTask::Translate { style, .. }) => crate::index::translate(style, ctx),
        _ => build(task, ctx),
    }
}

/// 按模板语言变体装配；[`build`] 等价于 `TemplateVariant::En`。中文变体缺失的模板回退到英文
pub fn build_variant(task: &PromptTask<'_>, ctx: &PromptCtx<'_>, variant: TemplateVariant) -> BuiltPrompt {
    let text = normalize_source(ctx.text);
    let census = Census::of(&text);
    let input_tokens = estimate_from_census(&census);
    let names = Names {
        source: source_lang_name(&ctx.source),
        source_attr: source_lang_attr(&ctx.source),
        target: target_lang_name(ctx.target),
    };
    let translate_max = max_tokens_translate(input_tokens, ctx.reasoning_model);

    match *task {
        PromptTask::Translate { style, dict: false } => BuiltPrompt {
            system: style_system(style, &names, ctx.glossary, variant),
            user_text: translate_user(&text, effective_tail(style), &names, variant),
            temperature: style.temperature,
            max_output_tokens: translate_max,
            output: style.output,
            post: PostKind::Translate,
            merge_system_hint: false,
        },
        PromptTask::Translate { style, dict: true } => BuiltPrompt {
            system: dictionary_system(style, &names, ctx.glossary, variant),
            user_text: translate_user(
                &text,
                pick("dictionary.tail", t::DICTIONARY_TAIL, variant),
                &names,
                variant,
            ),
            temperature: Some(DICTIONARY_TEMPERATURE),
            max_output_tokens: translate_max,
            output: OutputFormat::MarkdownLite,
            post: PostKind::Translate,
            merge_system_hint: false,
        },
        PromptTask::Polish => edit_prompt(
            pick("polish.system", t::POLISH_SYSTEM, variant),
            POLISH_TEMPERATURE,
            &text,
            &names,
            translate_max,
            variant,
        ),
        PromptTask::Proofread => edit_prompt(
            pick("proofread.system", t::PROOFREAD_SYSTEM, variant),
            PROOFREAD_TEMPERATURE,
            &text,
            &names,
            translate_max,
            variant,
        ),
        PromptTask::RefineReflect { draft } => {
            let draft = normalize_source(draft);
            let vars = names.vars(&text).with_refine(&draft, None);
            BuiltPrompt {
                system: names.render(pick("refine/reflect.system", t::REFINE_REFLECT_SYSTEM, variant), &vars),
                user_text: names.render(pick("refine/reflect.user", t::REFINE_REFLECT_USER, variant), &vars),
                temperature: Some(REFINE_TEMPERATURE),
                // 反思只输出 ≤8 条修改意见或 OK；非流式聚合，给足推理模型的下限
                max_output_tokens: max_tokens_translate(input_tokens, true),
                output: OutputFormat::PlainText,
                // 反思输出只 trim（见 PromptTask::RefineReflect）
                post: PostKind::Raw,
                merge_system_hint: false,
            }
        }
        PromptTask::RefineImprove { style, draft, review } => {
            let draft = normalize_source(draft);
            let review = normalize_source(review);
            let vars = names.vars(&text).with_refine(&draft, Some(&review));
            BuiltPrompt {
                system: style_system(style, &names, ctx.glossary, variant),
                user_text: names.render(pick("refine/improve.user", t::REFINE_IMPROVE_USER, variant), &vars),
                temperature: Some(REFINE_TEMPERATURE),
                max_output_tokens: translate_max,
                output: style.output,
                post: PostKind::Translate,
                merge_system_hint: false,
            }
        }
        PromptTask::Ocr { mode } => ocr_prompt(mode, &names, variant),
    }
}

/// 渲染时用到的语言名
struct Names {
    source: &'static str,
    /// 限定词之后作定语的 source（低置信度时为 "source-language"）
    source_attr: &'static str,
    target: &'static str,
}

impl Names {
    fn vars<'a>(&self, text: &'a str) -> RenderVars<'a> {
        RenderVars {
            source_lang: self.source,
            target_lang: self.target,
            text,
            glossary: "",
            tail_instruction: "",
            draft: None,
            review: None,
        }
    }

    /// 渲染内置模板（单遍、白名单，同 [`crate::render`]）；`{{source_lang}}` 紧跟限定词时用定语形式
    fn render(&self, template: &str, vars: &RenderVars<'_>) -> String {
        let extra = vars.text.len() + vars.glossary.len() + vars.tail_instruction.len();
        render_with(template, extra, |name, before| match name {
            "source_lang" if follows_determiner(before) => Some(self.source_attr),
            _ => vars.get(name),
        })
    }

    /// 渲染风格模板（内置或自定义风格的 system / tail）：只替换语言名，其余 `{{x}}` 原样保留
    fn render_style(&self, template: &str) -> String {
        render_with(template, 0, |name, before| match name {
            "source_lang" if follows_determiner(before) => Some(self.source_attr),
            "source_lang" => Some(self.source),
            "target_lang" => Some(self.target),
            _ => None,
        })
    }
}

/// 这些词之后的 `{{source_lang}}` 用作定语（"a single {{source_lang}} word"、"the {{source_lang}} text"、
/// "a bilingual {{source_lang}}–{{target_lang}} language tutor"）
const DETERMINERS: [&str; 20] = [
    "a",
    "an",
    "the",
    "this",
    "that",
    "these",
    "those",
    "each",
    "every",
    "any",
    "some",
    "your",
    "its",
    "their",
    "our",
    "single",
    "bilingual",
    "native",
    "original",
    "same",
];

/// 模板中占位符之前紧挨的是不是「限定词 + 空格」
fn follows_determiner(before: &str) -> bool {
    let Some(head) = before.strip_suffix(' ') else {
        return false;
    };
    let start = head
        .char_indices()
        .rev()
        .find(|(_, c)| !c.is_ascii_alphabetic())
        .map_or(0, |(i, c)| i + c.len_utf8());
    let word = &head[start..];
    DETERMINERS.iter().any(|d| d.eq_ignore_ascii_case(word))
}

trait WithRefine<'a> {
    fn with_refine(self, draft: &'a str, review: Option<&'a str>) -> Self;
}

impl<'a> WithRefine<'a> for RenderVars<'a> {
    fn with_refine(self, draft: &'a str, review: Option<&'a str>) -> Self {
        RenderVars {
            draft: Some(draft),
            review,
            ..self
        }
    }
}

/// 风格的 tail；自定义风格没填 tail 时用 natural 的
fn effective_tail(style: &Style) -> &str {
    if style.tail_instruction.trim().is_empty() {
        t::NATURAL_TAIL
    } else {
        &style.tail_instruction
    }
}

/// 风格 system：render(风格) + "\n\n" + render(硬规则，含术语区块)，然后 trim_end
///
/// 风格模板只替换 `{{source_lang}}` / `{{target_lang}}`：`{{text}}` 不展开——原文只出现在 user 消息的
/// `<source_text>` 里，不可信内容不会被提升到 system 角色；`{{glossary}}` / `{{tail_instruction}}` 同样原样保留
/// （术语只放在硬规则末尾、tail 属于 user 消息），设置页用 [`crate::unknown_variables`] 提示。
/// 风格模板为空（自定义风格只填了 tail）时 system 只有硬规则。
fn style_system(style: &Style, names: &Names, glossary: &[GlossaryEntry], variant: TemplateVariant) -> String {
    let block = render_glossary_block(pick("glossary_block", t::GLOSSARY_BLOCK, variant), glossary);
    let body = names.render_style(&style.system_template);
    let rules = names.render(
        pick("hard_rules", t::HARD_RULES, variant),
        &RenderVars {
            glossary: &block,
            ..names.vars("")
        },
    );
    let body = body.trim_end();
    let mut system = String::with_capacity(body.len() + rules.len() + 2);
    if !body.trim_start().is_empty() {
        system.push_str(body);
        system.push_str("\n\n");
    }
    system.push_str(&rules);
    truncate_trailing_whitespace(&mut system);
    system
}

/// 词典式 system：基础模板（technical / casual 在 Rules 末尾追加一行）+ 命中时的术语区块
fn dictionary_system(style: &Style, names: &Names, glossary: &[GlossaryEntry], variant: TemplateVariant) -> String {
    let extra = match style.id.as_str() {
        "technical" => Some(pick(
            "dictionary.technical.extra",
            t::DICTIONARY_TECHNICAL_EXTRA,
            variant,
        )),
        "casual" => Some(pick("dictionary.casual.extra", t::DICTIONARY_CASUAL_EXTRA, variant)),
        _ => None,
    };
    let mut system = names.render(
        pick("dictionary.system", t::DICTIONARY_SYSTEM, variant),
        &names.vars(""),
    );
    truncate_trailing_whitespace(&mut system);
    if let Some(extra) = extra {
        system.push('\n');
        system.push_str(extra);
    }
    let block = render_glossary_block(pick("glossary_block", t::GLOSSARY_BLOCK, variant), glossary);
    if !block.is_empty() {
        system.push_str("\n\n");
        system.push_str(&block);
    }
    system
}

/// 翻译 / 词典式的 user 消息：`<source_text>\n{text}\n</source_text>\n\n{tail}`
///
/// tail 是风格模板（自定义风格可编辑），与风格 system 一样只替换语言名
fn translate_user(text: &str, tail: &str, names: &Names, variant: TemplateVariant) -> String {
    let tail = names.render_style(tail);
    let vars = RenderVars {
        tail_instruction: tail.trim(),
        ..names.vars(text)
    };
    names.render(pick("user_skeleton", t::USER_SKELETON, variant), &vars)
}

/// polish / proofread：只用各自的模板，不拼硬规则、不注入术语、不走词典式
fn edit_prompt(
    system: &str,
    temperature: f32,
    text: &str,
    names: &Names,
    max_output_tokens: u32,
    variant: TemplateVariant,
) -> BuiltPrompt {
    let vars = names.vars(text);
    BuiltPrompt {
        system: names.render(system, &vars),
        user_text: names.render(pick("edit.user", t::EDIT_USER, variant), &vars),
        temperature: Some(temperature),
        max_output_tokens,
        output: OutputFormat::MarkdownLite,
        post: PostKind::Edit,
        merge_system_hint: false,
    }
}

/// OCR：转录模式 system = 基础块 + "\n\n" + 模式块；直接翻译用独立模板
fn ocr_prompt(mode: OcrMode, names: &Names, variant: TemplateVariant) -> BuiltPrompt {
    let vars = names.vars("");
    let (system, user, temperature) = match mode {
        OcrMode::Translate => (
            names.render(pick("ocr/translate", t::OCR_TRANSLATE, variant), &vars),
            names.render(pick("ocr/translate.user", t::OCR_TRANSLATE_USER, variant), &vars),
            OCR_TRANSLATE_TEMPERATURE,
        ),
        transcribe => {
            let (block, block_name, user, user_name) = match transcribe {
                OcrMode::Paragraphs => (
                    t::OCR_PARAGRAPHS,
                    "ocr/paragraphs",
                    t::OCR_PARAGRAPHS_USER,
                    "ocr/paragraphs.user",
                ),
                OcrMode::Markdown => (
                    t::OCR_MARKDOWN,
                    "ocr/markdown",
                    t::OCR_MARKDOWN_USER,
                    "ocr/markdown.user",
                ),
                _ => (t::OCR_LINES, "ocr/lines", t::OCR_LINES_USER, "ocr/lines.user"),
            };
            let base = pick("ocr/base", t::OCR_BASE, variant);
            let block = pick(block_name, block, variant);
            let mut system = String::with_capacity(base.len() + block.len() + 2);
            system.push_str(base);
            system.push_str("\n\n");
            system.push_str(block);
            (
                names.render(&system, &vars),
                names.render(pick(user_name, user, variant), &vars),
                OCR_TRANSCRIBE_TEMPERATURE,
            )
        }
    };
    BuiltPrompt {
        system,
        user_text: user,
        temperature: Some(temperature),
        max_output_tokens: OCR_MAX_OUTPUT,
        // 结构化（markdown）模式在编辑区显示源码，不渲染
        output: OutputFormat::PlainText,
        post: PostKind::Ocr { mode },
        // OCR 请求总带图：允许 merge_system_into_user
        merge_system_hint: true,
    }
}

fn truncate_trailing_whitespace(s: &mut String) {
    let len = s.trim_end().len();
    s.truncate(len);
}

/// 原文规范化：去掉首尾空行和每行行尾空白（含 `\r`），保留首行缩进和内部换行。
///
/// 已经规范的文本不分配内存。
pub(crate) fn normalize_source(text: &str) -> Cow<'_, str> {
    // 第一行非空行的起点、最后一个非空行的终点（行尾空白之前）
    let mut start: Option<usize> = None;
    let mut end = 0usize;
    let mut dirty = false;
    let mut offset = 0usize;
    for line in text.split('\n') {
        let trimmed = line.trim_end();
        if !trimmed.is_empty() {
            if start.is_none() {
                start = Some(offset);
            }
            end = offset + trimmed.len();
            if trimmed.len() != line.len() {
                dirty = true;
            }
        } else if start.is_some() && !line.is_empty() {
            // 内部的空白行：需要清掉其中的空白字符
            dirty = true;
        }
        offset += line.len() + 1;
    }
    let Some(start) = start else {
        return Cow::Borrowed("");
    };
    let region = &text[start..end];
    if !dirty {
        return Cow::Borrowed(region);
    }
    let mut out = String::with_capacity(region.len());
    for (i, line) in region.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line.trim_end());
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_source_rules() {
        let cases = [
            ("hello", "hello"),
            ("\n\n  indented\nnext  \n\n", "  indented\nnext"),
            ("a\r\nb\r\n", "a\nb"),
            ("a\n   \nb", "a\n\nb"),
            ("a\n\n\nb", "a\n\n\nb"),
            ("   \n\t\n", ""),
            ("", ""),
            ("  x  ", "  x"),
            ("末尾全角空格\u{3000}", "末尾全角空格"),
        ];
        for (input, want) in cases {
            assert_eq!(normalize_source(input), want, "{input:?}");
        }
        assert!(matches!(normalize_source("a\nb"), Cow::Borrowed(_)));
        assert!(matches!(normalize_source("\n a\nb\n"), Cow::Borrowed(_)));
    }

    #[test]
    fn empty_custom_tail_falls_back_to_natural() {
        let s = Style::custom("custom-x", "X");
        assert_eq!(effective_tail(&s), t::NATURAL_TAIL);
    }
}
