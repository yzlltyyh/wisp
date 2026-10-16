//! 翻译专用小模型（Index-Translate）的提示词档位。
//!
//! 这类模型对提示词格式很敏感：官方的请求格式与训练、评测时一致——**只有一条 user 消息、没有 system**，指令是中文，
//! 语言名用中文全称；有术语、格式或风格要求时改用「约束翻译」格式，把要求逐条写成【硬性要求】/【注意】。
//! 所以这里不叠加 Wisp 的风格 system、硬规则与 `<source_text>` 骨架，只把能映射的东西（术语命中、内置风格、
//! 自定义风格的说明、分段）翻成官方格式里的约束。
//!
//! - 无约束：`请将以下{源语言}文本翻译为{目标语言}，直接输出翻译结果，不要进行任何解释。\n\n{原文}`（源语言无法确定时省略）；
//! - 有约束：`请将以下{源语言}文本翻译成{目标语言}，并且严格遵循所有约束要求。\n\n【源文】\n{原文}\n\n【约束要求】\n1. …\n\n只输出译文，不要有任何额外说明。`
//!   （官方模板里这两句的动词就是「为」与「成」，逐字照抄）。约束的顺序同官方：先硬性要求（分段）、再术语对照、最后软性要求（风格）；
//! - temperature 0（贪心解码，官方客户端的设置）；最大输出按原文估算：token × 2 + 256，上限 4096；
//! - 后处理与缓存键照常（译文的后处理去掉模型偶尔加的引号、前缀；缓存键里有 Provider 与模型 id，自然与云端模型分开）。
//!
//! 词典式、润色、校对、提升译文这些任务模型并不擅长：编排器在离线翻译时把词典式退化为普通翻译、提升译文退化为单次翻译，
//! 润色与校对直接提示不支持。万一仍以这个档位装配其他任务，按标准档位装配（见 [`crate::build_profile`]）。

use wisp_types::{GlossaryEntry, Lang, Style};

use crate::assemble::{BuiltPrompt, PromptCtx, normalize_source};
use crate::glossary::MAX_GLOSSARY_HITS;
use crate::lang::Detection;
use crate::post::PostKind;
use crate::tokens::estimate_tokens;

/// 提示词档位：按模型选择装配方式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PromptProfile {
    /// Wisp 自己的风格提示词（通用大模型）
    #[default]
    Standard,
    /// Index-Translate 的官方格式（离线翻译模型）
    IndexTranslate,
}

/// 自定义风格的说明最多取这么多个字符写进【注意】（小模型的提示词要短，太长的风格描述反而干扰翻译）
pub const CUSTOM_STYLE_CHARS: usize = 120;
/// 最大输出的上限
const MAX_OUTPUT: u32 = 4096;
/// 最大输出的余量（短原文也给足标点、换行与少量扩写的空间）
const OUTPUT_SLACK: u32 = 256;

/// 中文语言名（官方模板要求中文全称）。`Other` 没有具体名字
pub fn lang_name_zh(lang: Lang) -> Option<&'static str> {
    match lang {
        Lang::ZhCn => Some("简体中文"),
        Lang::En => Some("英语"),
        Lang::Ja => Some("日语"),
        Lang::Ko => Some("韩语"),
        Lang::Other => None,
    }
}

/// 源语言名：语言可信时才写（低置信度、无法判断时省略，官方模板的 auto 写法）
fn source_name(det: &Detection) -> Option<&'static str> {
    if det.low_confidence {
        None
    } else {
        lang_name_zh(det.lang)
    }
}

/// 目标语言名：目标总是设置里的主 / 次语言；万一是 `Other`，与标准档位一样按简体中文
fn target_name(lang: Lang) -> &'static str {
    lang_name_zh(lang).unwrap_or("简体中文")
}

/// 内置风格对应的一句中文风格要求；`natural`（默认风格）不加约束
fn builtin_requirement(id: &str) -> Option<&'static str> {
    match id {
        "technical" => Some("保持专业严谨的技术文档风格，术语准确且前后一致，业内通用的英文技术术语可保留原文"),
        "academic" => Some("使用严谨、正式的学术书面语，术语规范，不增删、不改变原文的论断与语气"),
        "casual" => Some("使用自然、口语化的日常表达，保留原文的语气、情绪与幽默"),
        "learn" => Some("译文忠实、自然，尽量保留原文的句子结构，便于与原文对照学习"),
        "literal" => Some("尽量直译：逐句对应，保持原文的语序与句子边界，不合并或拆分句子"),
        _ => None,
    }
}

/// 风格要求：内置风格查表；自定义风格取它的说明（风格模板，没有时取尾部指令）的前 [`CUSTOM_STYLE_CHARS`] 个字符，
/// 语言占位符换成中文语言名、空白折叠成一个空格
fn style_requirement(style: &Style, source: Option<&str>, target: &str) -> Option<String> {
    if style.builtin {
        return builtin_requirement(&style.id).map(str::to_owned);
    }
    let raw = if style.system_template.trim().is_empty() {
        &style.tail_instruction
    } else {
        &style.system_template
    };
    let replaced = raw
        .replace("{{source_lang}}", source.unwrap_or("源语言"))
        .replace("{{target_lang}}", target);
    let folded: String = replaced
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(CUSTOM_STYLE_CHARS)
        .collect();
    let folded = folded.trim();
    (!folded.is_empty()).then(|| folded.to_owned())
}

/// 术语对照：`专名/术语对照: A→B、C→D`（官方格式：英文冒号加空格、箭头、中文顿号）。目标为空时保留原词
fn glossary_requirement(entries: &[GlossaryEntry]) -> Option<String> {
    let pairs: Vec<String> = entries
        .iter()
        .take(MAX_GLOSSARY_HITS)
        .filter_map(|e| {
            let source = single_line(&e.source);
            if source.is_empty() {
                return None;
            }
            let target = single_line(&e.target);
            let target = if target.is_empty() { source.clone() } else { target };
            Some(format!("{source}→{target}"))
        })
        .collect();
    (!pairs.is_empty()).then(|| format!("专名/术语对照: {}", pairs.join("、")))
}

fn single_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 最大输出：原文 token × 2 + 256，上限 4096（译文与原文的 token 数大致相当，留一倍余量）
pub fn max_output_tokens(text: &str) -> u32 {
    estimate_tokens(text)
        .saturating_mul(2)
        .saturating_add(OUTPUT_SLACK)
        .min(MAX_OUTPUT)
}

/// 装配一次翻译
pub(crate) fn translate(style: &Style, ctx: &PromptCtx<'_>) -> BuiltPrompt {
    let text = normalize_source(ctx.text);
    let source = source_name(&ctx.source);
    let target = target_name(ctx.target);

    let mut hard: Vec<String> = Vec::new();
    if text.contains('\n') {
        // 多段原文：分段与换行是官方列出的硬性约束之一（小模型偶尔把几段并成一段）
        hard.push("保留源文的分段与换行，不要合并或拆分段落".to_owned());
    }
    hard.extend(glossary_requirement(ctx.glossary));
    let soft: Vec<String> = style_requirement(style, source, target).into_iter().collect();

    let src = source.unwrap_or("");
    let user_text = if hard.is_empty() && soft.is_empty() {
        format!("请将以下{src}文本翻译为{target}，直接输出翻译结果，不要进行任何解释。\n\n{text}")
    } else {
        let mut out =
            format!("请将以下{src}文本翻译成{target}，并且严格遵循所有约束要求。\n\n【源文】\n{text}\n\n【约束要求】");
        let items = hard
            .iter()
            .map(|r| ("【硬性要求】", r))
            .chain(soft.iter().map(|r| ("【注意】", r)));
        for (i, (prefix, req)) in items.enumerate() {
            out.push_str(&format!("\n{}. {prefix}{req}", i + 1));
        }
        out.push_str("\n\n只输出译文，不要有任何额外说明。");
        out
    };
    BuiltPrompt {
        system: String::new(),
        user_text,
        temperature: Some(0.0),
        max_output_tokens: max_output_tokens(&text),
        // 模型只输出译文本身；按风格的输出格式显示（纯文本在 Markdown 渲染下原样显示）
        output: style.output,
        post: PostKind::Translate,
        merge_system_hint: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PromptTask, build, build_profile, builtin_styles};

    fn style(id: &str) -> Style {
        builtin_styles()
            .into_iter()
            .find(|s| s.id == id)
            .expect("builtin style")
    }

    fn known(lang: Lang) -> Detection {
        Detection {
            lang,
            low_confidence: false,
        }
    }

    fn ctx<'a>(text: &'a str, source: Detection, target: Lang, glossary: &'a [GlossaryEntry]) -> PromptCtx<'a> {
        PromptCtx {
            text,
            source,
            target,
            glossary,
            reasoning_model: false,
        }
    }

    fn index(task: &PromptTask<'_>, c: &PromptCtx<'_>) -> BuiltPrompt {
        build_profile(task, c, PromptProfile::IndexTranslate)
    }

    fn translate(style: &Style) -> PromptTask<'_> {
        PromptTask::Translate { style, dict: false }
    }

    #[test]
    fn plain_translation_uses_the_official_single_user_message() {
        let natural = style("natural");
        let p = index(
            &translate(&natural),
            &ctx("  Hello world.\n", known(Lang::En), Lang::ZhCn, &[]),
        );
        assert_eq!(p.system, "", "没有 system 消息");
        assert_eq!(
            p.user_text,
            "请将以下英语文本翻译为简体中文，直接输出翻译结果，不要进行任何解释。\n\n  Hello world."
        );
        assert_eq!(p.temperature, Some(0.0));
        assert_eq!(p.post, PostKind::Translate);
        assert!(!p.merge_system_hint);
        assert_eq!(p.output, natural.output);
    }

    #[test]
    fn uncertain_sources_are_omitted() {
        let natural = style("natural");
        let low = Detection {
            lang: Lang::En,
            low_confidence: true,
        };
        let p = index(
            &translate(&natural),
            &ctx("Bonjour tout le monde", low, Lang::ZhCn, &[]),
        );
        assert!(
            p.user_text.starts_with("请将以下文本翻译为简体中文，"),
            "{}",
            p.user_text
        );
        let other = Detection {
            lang: Lang::Other,
            low_confidence: false,
        };
        let p = index(&translate(&natural), &ctx("12345", other, Lang::En, &[]));
        assert!(p.user_text.starts_with("请将以下文本翻译为英语，"), "{}", p.user_text);
    }

    #[test]
    fn every_language_has_a_chinese_name() {
        assert_eq!(lang_name_zh(Lang::ZhCn), Some("简体中文"));
        assert_eq!(lang_name_zh(Lang::En), Some("英语"));
        assert_eq!(lang_name_zh(Lang::Ja), Some("日语"));
        assert_eq!(lang_name_zh(Lang::Ko), Some("韩语"));
        assert_eq!(lang_name_zh(Lang::Other), None);
        let natural = style("natural");
        for (target, name) in [
            (Lang::ZhCn, "简体中文"),
            (Lang::En, "英语"),
            (Lang::Ja, "日语"),
            (Lang::Ko, "韩语"),
            (Lang::Other, "简体中文"),
        ] {
            let p = index(&translate(&natural), &ctx("x", known(Lang::En), target, &[]));
            assert!(
                p.user_text.contains(&format!("翻译为{name}，")),
                "{target:?}: {}",
                p.user_text
            );
        }
        let p = index(
            &translate(&natural),
            &ctx("こんにちは", known(Lang::Ja), Lang::ZhCn, &[]),
        );
        assert!(p.user_text.starts_with("请将以下日语文本翻译为简体中文"));
        let p = index(&translate(&natural), &ctx("안녕하세요", known(Lang::Ko), Lang::En, &[]));
        assert!(p.user_text.starts_with("请将以下韩语文本翻译为英语"));
        let p = index(&translate(&natural), &ctx("你好", known(Lang::ZhCn), Lang::En, &[]));
        assert!(p.user_text.starts_with("请将以下简体中文文本翻译为英语"));
    }

    #[test]
    fn constraints_follow_the_official_order_and_format() {
        let technical = style("technical");
        let glossary = vec![
            GlossaryEntry {
                source: "carbon fiber".to_owned(),
                target: "碳纤维".to_owned(),
                note: None,
            },
            GlossaryEntry {
                source: "Wisp".to_owned(),
                target: String::new(),
                note: Some("只在界面显示".to_owned()),
            },
        ];
        let p = index(
            &translate(&technical),
            &ctx(
                "Carbon fiber resists cracks.\n\nWisp ships it.",
                known(Lang::En),
                Lang::ZhCn,
                &glossary,
            ),
        );
        let want = "请将以下英语文本翻译成简体中文，并且严格遵循所有约束要求。\n\n【源文】\nCarbon fiber resists cracks.\n\nWisp ships it.\n\n【约束要求】\n\
            1. 【硬性要求】保留源文的分段与换行，不要合并或拆分段落\n\
            2. 【硬性要求】专名/术语对照: carbon fiber→碳纤维、Wisp→Wisp\n\
            3. 【注意】保持专业严谨的技术文档风格，术语准确且前后一致，业内通用的英文技术术语可保留原文\n\n\
            只输出译文，不要有任何额外说明。";
        assert_eq!(p.user_text, want);
        assert!(!p.user_text.contains("只在界面显示"), "备注不注入");
        // 只有术语命中：一条硬性要求
        let natural = style("natural");
        let p = index(
            &translate(&natural),
            &ctx("carbon fiber", known(Lang::En), Lang::ZhCn, &glossary[..1]),
        );
        assert!(
            p.user_text
                .contains("【约束要求】\n1. 【硬性要求】专名/术语对照: carbon fiber→碳纤维\n\n只输出")
        );
        // 只有风格：一条【注意】
        let casual = style("casual");
        let p = index(&translate(&casual), &ctx("lol ok", known(Lang::En), Lang::ZhCn, &[]));
        assert!(
            p.user_text
                .contains("【约束要求】\n1. 【注意】使用自然、口语化的日常表达")
        );
    }

    #[test]
    fn every_builtin_style_maps_to_one_requirement_except_natural() {
        for s in builtin_styles() {
            let p = index(&translate(&s), &ctx("Hello there.", known(Lang::En), Lang::ZhCn, &[]));
            let constrained = p.user_text.contains("【约束要求】");
            assert_eq!(constrained, s.id != "natural", "{}: {}", s.id, p.user_text);
            if constrained {
                assert_eq!(p.user_text.matches("【注意】").count(), 1, "{}", s.id);
            }
        }
        // 用户改过的内置风格仍按 id 映射，不把英文的风格模板塞进来
        let mut modified = style("academic");
        modified.system_template = "Write like a lawyer.".to_owned();
        modified.modified = true;
        let p = index(&translate(&modified), &ctx("Hi.", known(Lang::En), Lang::ZhCn, &[]));
        assert!(p.user_text.contains("学术书面语") && !p.user_text.contains("lawyer"));
    }

    #[test]
    fn custom_styles_contribute_the_first_120_chars_of_their_description() {
        let mut custom = Style::custom("custom-1", "Pirate");
        custom.system_template = format!(
            "  Translate into   {{{{target_lang}}}} like a pirate from {{{{source_lang}}}}.\n{}",
            "Arr! ".repeat(60)
        );
        let p = index(&translate(&custom), &ctx("Hello.", known(Lang::En), Lang::ZhCn, &[]));
        let line = p
            .user_text
            .lines()
            .find(|l| l.starts_with("1. 【注意】"))
            .expect("style line");
        let req = line.trim_start_matches("1. 【注意】");
        assert!(
            req.starts_with("Translate into 简体中文 like a pirate from 英语. Arr!"),
            "{req}"
        );
        assert_eq!(req.chars().count(), CUSTOM_STYLE_CHARS);
        // 风格模板为空时取尾部指令；两者都空时不加约束
        let mut tail_only = Style::custom("custom-2", "Short");
        tail_only.tail_instruction = "Keep it short.".to_owned();
        let p = index(&translate(&tail_only), &ctx("Hello.", known(Lang::En), Lang::ZhCn, &[]));
        assert!(p.user_text.contains("1. 【注意】Keep it short."));
        let empty = Style::custom("custom-3", "Empty");
        let p = index(&translate(&empty), &ctx("Hello.", known(Lang::En), Lang::ZhCn, &[]));
        assert!(!p.user_text.contains("【约束要求】"));
    }

    #[test]
    fn max_tokens_are_estimated_from_the_source() {
        assert_eq!(max_output_tokens(""), 256);
        assert_eq!(max_output_tokens("你好"), 2 * 2 + 256);
        assert_eq!(max_output_tokens(&"x".repeat(400)), 100 * 2 + 256);
        assert_eq!(max_output_tokens(&"字".repeat(5000)), 4096);
        let natural = style("natural");
        let p = index(&translate(&natural), &ctx("你好", known(Lang::ZhCn), Lang::En, &[]));
        assert_eq!(p.max_output_tokens, 260);
    }

    #[test]
    fn dictionary_and_other_tasks() {
        let natural = style("natural");
        let c = ctx("serendipity", known(Lang::En), Lang::ZhCn, &[]);
        // 词典式按普通翻译装配
        assert_eq!(
            index(
                &PromptTask::Translate {
                    style: &natural,
                    dict: true
                },
                &c
            ),
            index(&translate(&natural), &c)
        );
        // 其余任务按标准档位
        for task in [
            PromptTask::Polish,
            PromptTask::Proofread,
            PromptTask::Ocr {
                mode: wisp_types::OcrMode::Lines,
            },
        ] {
            assert_eq!(index(&task, &c), build(&task, &c));
        }
        // 标准档位与 build 相同
        assert_eq!(
            build_profile(&translate(&natural), &c, PromptProfile::Standard),
            build(&translate(&natural), &c)
        );
        assert_eq!(PromptProfile::default(), PromptProfile::Standard);
    }
}
