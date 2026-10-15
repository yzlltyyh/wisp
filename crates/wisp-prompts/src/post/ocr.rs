//! OCR 输出的后处理与「未检测到文字」判定。
//!
//! 不套用翻译的引号与回显标签规则，只做：
//! - 无文字判定（所有模式）：开头缓冲到不可能是这些形式为止，结束时判定；
//! - lines / paragraphs / translate：语言标注为空、text、markdown、md 的围栏包住全文时去掉；
//!   markdown 模式只去掉 markdown / md 围栏（保留合法的代码块）；
//! - 去掉开头的「Here is the transcription:」「Transcription:」「识别结果：」「译文：」（直接翻译还有「Translation:」等）；
//! - 保留首行缩进（lines 模式的缩进有意义）。

use wisp_types::OcrMode;

use super::{FencePolicy, GROUP_OCR as O, GROUP_TRANSLATION as T, Match, Prefix, Rules, match_prefixes};

/// 转录模式的前缀
const OCR_PREFIXES: &[Prefix] = &[
    Prefix::line(O, "here is the transcription"),
    Prefix::line(O, "here's the transcription"),
    Prefix::line(O, "here\u{2019}s the transcription"),
    Prefix::colon(O, "transcription"),
    Prefix::colon(O, "识别结果"),
    Prefix::colon(T, "译文"),
];

/// 直接翻译模式的前缀（转录模式前缀的超集）
const OCR_TRANSLATE_PREFIXES: &[Prefix] = &[
    Prefix::line(O, "here is the transcription"),
    Prefix::line(O, "here's the transcription"),
    Prefix::line(O, "here\u{2019}s the transcription"),
    Prefix::colon(O, "transcription"),
    Prefix::colon(O, "识别结果"),
    Prefix::colon(T, "译文"),
    Prefix::colon(T, "翻译"),
    Prefix::colon(T, "translation"),
    Prefix::line(T, "here is the translation"),
    Prefix::line(T, "here's the translation"),
    Prefix::line(T, "here\u{2019}s the translation"),
];

/// 无文字判定认可的前缀：所有 OCR 模式前缀的并集。判定与模式无关——`Translation: <NO_TEXT>` 在转录模式下
/// 同样表示没有文字（模式自己的前缀规则决定的是「有文字时剥不剥」）
const NO_TEXT_PREFIXES: &[Prefix] = OCR_TRANSLATE_PREFIXES;

/// 纯文本类模式可以剥离的围栏语言标注（text 的常见同义写法 plaintext / txt 一并接受）
const PLAIN_FENCE_LANGS: &[&str] = &["", "text", "plaintext", "txt", "markdown", "md"];
/// markdown 模式可以剥离的围栏语言标注
const MARKDOWN_FENCE_LANGS: &[&str] = &["markdown", "md"];

/// 无文字的哨兵与常见变体（比较时大小写不敏感，忽略末尾句点）
const NO_TEXT_FORMS: [&str; 4] = ["<no_text>", "no_text", "null", "no text found"];

pub(super) fn rules(mode: OcrMode) -> Rules {
    let (prefixes, langs) = match mode {
        OcrMode::Markdown => (OCR_PREFIXES, MARKDOWN_FENCE_LANGS),
        OcrMode::Translate => (OCR_TRANSLATE_PREFIXES, PLAIN_FENCE_LANGS),
        OcrMode::Lines | OcrMode::Paragraphs => (OCR_PREFIXES, PLAIN_FENCE_LANGS),
    };
    Rules {
        prefixes,
        // 截图没有「原文」，前缀不会被原文关闭
        disabled_groups: 0,
        tags: false,
        quotes: false,
        fence: FencePolicy::Langs(langs),
        trim_indent: false,
        no_text: true,
        edit_tail: false,
    }
}

/// 输出是否表示「图中没有文字」
///
/// trim 后去掉包装层再看核心：
/// - 包装层：包住全文的代码围栏（任意语言标注，闭合围栏可以缺失）至多一层；OCR 各模式会剥离的开头前缀
///   （`Transcription:`、`Here is the transcription:`、`识别结果：`、`译文：`、`Translation:` 等，所有模式的并集）
///   至多一个；两者顺序不限，之后还可以有一对单个反引号；
/// - 核心：精确等于 `<NO_TEXT>` / `NO_TEXT` / `null` / `No text found`（大小写不敏感，忽略末尾句点）；或长度 ≤ 2
///   且不含字母、数字、汉字（如 `□`、空输出）。
///
/// 例如 `` `<NO_TEXT>` ``、`Transcription: <NO_TEXT>`、```` ```text\n译文：<NO_TEXT>\n``` ```` 都是无文字。
pub fn is_no_text(output: &str) -> bool {
    matches_no_text(output, false)
}

/// 流式开头：已收到的文本是否还可能是「无文字」的某种形式（是则继续缓冲）。
///
/// 与 [`is_no_text`] 共用同一套语法，保证覆盖 `is_no_text` 为真的每个字符串的每个前缀（测试按随机生成的
/// 无文字形式逐个前缀验证），否则逐字符喂入时会提前放行、`no_text` 也判不出来。
pub(super) fn could_be_no_text(raw: &str) -> bool {
    matches_no_text(raw, true)
}

/// 「无文字」形式的识别；`partial` 为 true 时判断 `s` 是否为某个无文字形式的前缀。
///
/// 前缀模式只会多认（多缓冲几个字符），不会少认：流结束时总以完整判定为准。
fn matches_no_text(s: &str, partial: bool) -> bool {
    let mut s = if partial { s.trim_start() } else { s.trim() };
    let (mut fenced, mut prefixed) = (false, false);
    loop {
        let ticks = s.bytes().take_while(|&b| b == b'`').count();
        // 只有反引号：「`」「``」「```」可能是行内代码或围栏的开头
        if partial && ticks > 0 && ticks == s.len() {
            return ticks <= 3;
        }
        if !fenced && ticks == 3 {
            let after = &s[3..];
            let Some(nl) = after.find('\n') else {
                // 围栏开头行还没写完（完整文本里没有换行就不是围栏）
                return partial && is_fence_lang(after.trim());
            };
            if !is_fence_lang(after[..nl].trim()) {
                return false;
            }
            let body = after[nl + 1..].trim_end();
            s = if partial {
                // 末尾可能是不完整的闭合围栏（也可能紧跟在行内反引号之后）：反引号全部去掉，行内的闭合反引号本来就可选
                body.trim_end_matches('`')
            } else {
                body.strip_suffix("```").unwrap_or(body)
            }
            .trim();
            fenced = true;
            continue;
        }
        if !prefixed {
            match match_prefixes(s, NO_TEXT_PREFIXES, 0) {
                Match::Full(n) => {
                    s = s[n..].trim_start();
                    prefixed = true;
                    continue;
                }
                Match::Partial if partial => return true,
                Match::Partial | Match::No => {}
            }
        }
        break;
    }
    core_matches(unwrap_inline(s, partial), partial)
}

/// 去掉包住核心的一对单个反引号（`` `<NO_TEXT>` ``）；前缀模式下闭合反引号可以还没出现
fn unwrap_inline(s: &str, partial: bool) -> &str {
    let Some(inner) = s.strip_prefix('`').filter(|i| !i.starts_with('`')) else {
        return s;
    };
    if partial {
        let inner = inner.trim_end();
        inner.strip_suffix('`').unwrap_or(inner)
    } else {
        inner.strip_suffix('`').unwrap_or(s)
    }
}

/// 核心：哨兵与常见变体，或 ≤ 2 个不含字母、数字、汉字的字符；`partial` 时只要求是某个变体的前缀
fn core_matches(core: &str, partial: bool) -> bool {
    let c = core.trim();
    if c.chars().nth(2).is_none() && !c.chars().any(char::is_alphanumeric) {
        return true;
    }
    let c = c.strip_suffix('.').unwrap_or(c).trim_end();
    NO_TEXT_FORMS.iter().any(|f| {
        if partial {
            let (cb, fb) = (c.as_bytes(), f.as_bytes());
            cb.len() <= fb.len() && fb[..cb.len()].eq_ignore_ascii_case(cb)
        } else {
            c.eq_ignore_ascii_case(f)
        }
    })
}

/// 围栏语言标注允许的字符（不含反引号，所以「````」不是围栏）
fn is_fence_lang(lang: &str) -> bool {
    lang.len() <= super::MAX_FENCE_LANG
        && lang
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'+' | b'-' | b'.' | b'#'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_text_table() {
        let yes = [
            "<NO_TEXT>",
            "  <NO_TEXT>\n",
            "```\n<NO_TEXT>\n```",
            "```text\n<NO_TEXT>\n```",
            "```\n<NO_TEXT>```",
            "`<NO_TEXT>`",
            "`<NO_TEXT>`\n",
            "`<NO_TEXT>` ",
            "`<NO_TEXT>`\r\n",
            "`null`\n",
            "<no_text>",
            "<NO_TEXT>.",
            "□",
            "□□",
            "--",
            "",
            "   ",
            "null",
            "NULL.",
            "No text found.",
            "no text found",
            "NO_TEXT",
            // OCR 前缀之后的哨兵（处理器会剥掉前缀，剩下的就是哨兵）
            "Transcription: <NO_TEXT>",
            "Here is the transcription:\n<NO_TEXT>",
            "Here is the transcription of the image:\n\n`<NO_TEXT>`",
            "译文：<NO_TEXT>",
            "识别结果：null",
            "Translation: No text found.",
            "```text\nTranscription: <NO_TEXT>\n```",
            "Transcription:\n```\n<NO_TEXT>\n```",
            "Transcription:",
        ];
        for s in yes {
            assert!(is_no_text(s), "{s:?} 应判为无文字");
        }
        let no = [
            "OK",
            "A1",
            "a",
            "1",
            "□□□",
            "No text",
            "nullable",
            "<NO_TEXT> here",
            "中",
            "```\ncode\n```",
            "```",
            "````\n<NO_TEXT>\n````",
            "`<NO_TEXT>` x",
            "Transcription: hello",
            "Transcription: <NO_TEXT> and more",
            "Transcription: Translation: <NO_TEXT>",
            "```\n```\n<NO_TEXT>\n```\n```",
            "Notes: <NO_TEXT>",
        ];
        for s in no {
            assert!(!is_no_text(s), "{s:?} 应判为有文字");
        }
    }

    #[test]
    fn could_be_no_text_prefixes() {
        for s in [
            "",
            " ",
            "<",
            "<NO_",
            "<NO_TEXT>",
            "n",
            "nu",
            "No text f",
            "□",
            "`",
            "```",
            "```text",
            "```\n<NO",
            "```\n<NO_TEXT>\n``",
            "`<NO",
            "`□",
            "`<NO_TEXT>`\n",
            "`<NO_TEXT>` ",
            "Trans",
            "Transcription: <NO",
            "Here is the transcription of",
            "Here is the transcription:\n<NO_TEXT>\n",
            "译文：",
            "```text\nTranscription: <NO_TEXT>\n``",
            "Transcription:\n```\n<NO",
        ] {
            assert!(could_be_no_text(s), "{s:?}");
        }
        for s in [
            "Hello",
            "nul l",
            "<div>",
            "□□□",
            "```python\nprint",
            "No texts",
            "12",
            "````",
            "``````□",
            "<NO_TEXT>`",
            "`<NO_TEXT>` x",
            "Transcription: H",
            "Here is the menu",
            "Notes: <NO_TEXT>",
        ] {
            assert!(!could_be_no_text(s), "{s:?}");
        }
    }

    /// 前缀闭包：随机拼接无文字语法的各个部分（含不合法的组合），凡是 is_no_text 为真的，每个前缀都满足 could_be_no_text
    #[test]
    fn could_be_no_text_covers_every_prefix() {
        const PARTS: &[&[&str]] = &[
            &["", " ", "\n", "\r\n", "\t", "\n\n"],
            &[
                "",
                "Transcription: ",
                "Here is the transcription of the image:\n",
                "识别结果：",
                "译文：",
                "Translation:",
            ],
            &["", "```\n", "```text\n", "```md \r\n", "```", "``"],
            &["", " ", "\n", "Transcription:", "翻译："],
            &["", "`", "``"],
            &[
                "<NO_TEXT>",
                "no_text",
                "Null",
                "No text found",
                "□",
                "--",
                "",
                "<NO_TEXT> ",
            ],
            &["", ".", " ."],
            &["", "`", "``"],
            &["", " ", "\n", "\r\n"],
            &["", "```", "\n```", "\n``", "\n```\n"],
            &["", " ", "\n"],
        ];
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut accepted = 0;
        for _ in 0..20_000 {
            let mut s = String::new();
            for part in PARTS {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                s.push_str(part[(state % part.len() as u64) as usize]);
            }
            if !is_no_text(&s) {
                continue;
            }
            accepted += 1;
            for (i, _) in s.char_indices().chain(std::iter::once((s.len(), ' '))) {
                assert!(could_be_no_text(&s[..i]), "{:?} 是无文字形式 {s:?} 的前缀", &s[..i]);
            }
        }
        assert!(accepted > 1_000, "只有 {accepted} 个组合被认作无文字");
    }
}
