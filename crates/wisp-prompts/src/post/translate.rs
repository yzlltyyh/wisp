//! 翻译 / 词典式 / 编辑（polish、proofread）/ refine 输出的后处理规则：
//! 剥离开头的「译文：」「Translation:」「Here is the translation:」类前缀、`<translation>` / `<source_text>` 回显、
//! 包住全文的成对引号与代码围栏，以及结尾的闭合标签。
//!
//! 原文本身带有这些形式时对应的规则关闭，避免误删正文：原文首尾有引号（含书名号、角括号）→ 不剥引号；
//! 原文有代码围栏 → 不剥围栏；原文含 `<translation` / `<source_text` → 不剥标签；原文以某组前缀的形式开头
//! （按语义成组：`Translate:` / `Translation:` / 「翻译：」/「译文：」同属一组）→ 整组前缀都不剥——
//! 原文 `Translate: Ctrl+T` 的正确译文就是「翻译：Ctrl+T」。

use super::{FencePolicy, GROUP_EDIT as E, GROUP_TRANSLATION as T, Prefix, Rules};

/// 翻译类输出的前缀（大小写不敏感，全角半角冒号都算）
const TRANSLATE_PREFIXES: &[Prefix] = &[
    Prefix::colon(T, "译文"),
    Prefix::colon(T, "翻译"),
    Prefix::colon(T, "translation"),
    Prefix::line(T, "here is the translation"),
    Prefix::line(T, "here's the translation"),
    Prefix::line(T, "here\u{2019}s the translation"),
];

/// 编辑动作额外的前言（模型偶尔会写 "Here's the improved version:" 之类）；同样要求冒号结尾
const EDIT_PREFIXES: &[Prefix] = &[
    Prefix::colon(T, "译文"),
    Prefix::colon(T, "翻译"),
    Prefix::colon(T, "translation"),
    Prefix::line(T, "here is the translation"),
    Prefix::line(T, "here's the translation"),
    Prefix::line(T, "here\u{2019}s the translation"),
    Prefix::line(E, "here is the edited"),
    Prefix::line(E, "here's the edited"),
    Prefix::line(E, "here\u{2019}s the edited"),
    Prefix::line(E, "here is the improved"),
    Prefix::line(E, "here's the improved"),
    Prefix::line(E, "here\u{2019}s the improved"),
    Prefix::line(E, "here is the corrected"),
    Prefix::line(E, "here's the corrected"),
    Prefix::line(E, "here\u{2019}s the corrected"),
    Prefix::line(E, "here is the revised"),
    Prefix::line(E, "here's the revised"),
    Prefix::line(E, "here\u{2019}s the revised"),
    Prefix::line(E, "here is the polished"),
    Prefix::line(E, "here's the polished"),
    Prefix::line(E, "here\u{2019}s the polished"),
    Prefix::line(E, "here is the proofread"),
    Prefix::line(E, "here's the proofread"),
    Prefix::line(E, "here\u{2019}s the proofread"),
    Prefix::colon(E, "edited text"),
    Prefix::colon(E, "corrected text"),
    Prefix::colon(E, "improved text"),
    Prefix::colon(E, "revised text"),
    Prefix::colon(E, "润色后"),
    Prefix::colon(E, "校对后"),
    Prefix::colon(E, "修改后"),
    Prefix::colon(E, "改写后"),
];

/// 原文首行以这些词开头（ASCII 大小写不敏感）且首行有冒号时，关闭「翻译」组前缀。
/// 覆盖各语言里「翻译 / 译文」的说法：模型会把它们互译（`Translated text: none` → 「译文：无」）。
const TRANSLATION_STARTS: &[&str] = &[
    "translat",
    "here is the translat",
    "here's the translat",
    "here\u{2019}s the translat",
    "翻译",
    "译文",
    "翻訳",
    "번역",
];

/// 原文首行以这些词开头且首行有冒号时，关闭「编辑前言」组前缀（原文 `Here's the plan: …` 润色后
/// 可能是 `Here's the revised plan: …`，不能当前言剥掉）
const EDIT_STARTS: &[&str] = &[
    "here is the ",
    "here's the ",
    "here\u{2019}s the ",
    "edited",
    "corrected",
    "improved",
    "revised",
    "polished",
    "proofread",
    "润色",
    "校对",
    "修改",
    "改写",
];

/// 引号字符：原文首或尾是其中之一时不剥离输出的包裹引号（书名号、角括号的译文常是英文引号）
const QUOTE_CHARS: [char; 16] = [
    '"', '\'', '\u{201C}', '\u{201D}', '\u{2018}', '\u{2019}', '「', '」', '『', '』', '«', '»', '《', '》', '〈', '〉',
];

/// 由原文决定翻译 / 编辑的规则
pub(super) fn rules(source: &str, edit: bool) -> Rules {
    let src = source.trim();
    let lower_has = |needle: &str| {
        src.as_bytes()
            .windows(needle.len())
            .any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
    };
    let quoted = src.starts_with(QUOTE_CHARS) || src.ends_with(QUOTE_CHARS);
    let first_line = source.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    Rules {
        prefixes: if edit { EDIT_PREFIXES } else { TRANSLATE_PREFIXES },
        disabled_groups: source_groups(src),
        tags: !lower_has("<translation") && !lower_has("<source_text"),
        quotes: !quoted,
        fence: if src.contains("```") || src.contains("~~~") {
            FencePolicy::Off
        } else {
            FencePolicy::AnyLang
        },
        trim_indent: !first_line.starts_with([' ', '\t', '\u{3000}']),
        no_text: false,
        edit_tail: edit,
        // 译文与润色结果的空行、行尾两个空格的硬换行（Markdown）都可能有意义，不整理
        squeeze: false,
    }
}

/// 原文以哪些前缀组的形式开头（首行去掉引号 / Markdown 标记后以组内的词开头，且首行有冒号）
fn source_groups(src: &str) -> u8 {
    let line = src
        .lines()
        .next()
        .unwrap_or("")
        .trim_start_matches(['"', '\u{201C}', '「', '*', '#', ' ']);
    if !line.contains([':', '：']) {
        return 0;
    }
    let starts = |words: &[&str]| {
        words
            .iter()
            .any(|w| line.len() >= w.len() && line.as_bytes()[..w.len()].eq_ignore_ascii_case(w.as_bytes()))
    };
    let mut groups = 0;
    if starts(TRANSLATION_STARTS) {
        groups |= T;
    }
    if starts(EDIT_STARTS) {
        groups |= E;
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_follow_source() {
        let r = rules("Hello", false);
        assert!(r.tags && r.quotes && r.trim_indent && !r.no_text && !r.edit_tail);
        assert_eq!(r.fence, FencePolicy::AnyLang);
        assert_eq!(r.prefixes.len(), TRANSLATE_PREFIXES.len());
        assert_eq!(r.disabled_groups, 0);

        assert!(!rules("\"hi\"", false).quotes);
        assert!(!rules("「你好」", false).quotes);
        assert!(!rules("《银河系漫游指南》是一部喜剧科幻小说。", false).quotes);
        assert!(!rules("看〈序〉", false).quotes);
        assert!(!rules("say <Translation>", false).tags);
        assert_eq!(rules("```\ncode\n```", false).fence, FencePolicy::Off);
        assert!(!rules("\n    indented", false).trim_indent);
        assert!(rules("Hi", true).edit_tail);
        assert_eq!(rules("Hi", true).prefixes.len(), EDIT_PREFIXES.len());
    }

    #[test]
    fn prefix_groups_close_across_languages() {
        for src in [
            "Translation: x",
            "Translate: Ctrl+T",
            "Translated text: none",
            "**Translator**: Jane",
            "翻译：把文本译成另一种语言",
            "译文：无",
            "翻訳：テキスト",
            "Here is the translation: x",
        ] {
            assert_eq!(source_groups(src) & T, T, "{src:?}");
        }
        for src in ["Translation is hard", "Translate this", "Hello: world", "翻译很难"] {
            assert_eq!(source_groups(src) & T, 0, "{src:?}");
        }
        assert_eq!(source_groups("Here's the plan: ship on Monday."), E);
        assert_eq!(source_groups("润色：这段话"), E);
        assert_eq!(source_groups("Here is the translation: x"), T | E);
    }
}
