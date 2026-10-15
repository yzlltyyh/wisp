//! 识别 400 是因为哪个参数被拒，给出降级动作。
//!
//! 只在「本次请求实际发送了的可降级参数」里找：错误信息里出现了它的字段名就认为是它被拒。
//! 最大输出字段是**互换**（max_tokens ⇄ max_completion_tokens）而不是删除，而且只认「字段本身不被接受」：
//! 信息里要有「不支持 / 未知 / 不允许」一类的字样，或者点名了另一个字段（`Use 'max_completion_tokens' instead`）；
//! 取值类的措辞（`Expected a value >= 16, but got 1 instead`、`must be at least`、`valid range` …）一律不算——
//! 那是「值不对」，互换字段解决不了，还会把错误的字段名写进能力缓存。Anthropic 的 max_tokens 必填，不参与降级。

use wisp_types::{MaxTokensField, ModelCaps};

/// 可降级的参数
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Param {
    /// temperature → 去掉
    Temperature,
    /// OpenAI Chat 的最大输出字段（值为本次发送的字段）→ 互换
    MaxTokens(MaxTokensField),
    /// reasoning_effort / reasoning.effort / output_config → 去掉
    Effort,
    /// thinking / enable_thinking / thinkingConfig / Kimi 的 reasoning_effort → 去掉
    Thinking,
    /// stream_options → 去掉
    StreamOptions,
    /// stream_options.include_obfuscation → 只去掉这一项
    Obfuscation,
}

/// 本次请求发送了的可降级参数，以及它在错误信息里可能出现的名字（小写）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SentParam {
    pub param: Param,
    pub names: &'static [&'static str],
}

/// 「字段不被支持」一类的字样（只用于最大输出字段的判定）。不含 "instead"：取值错误也常以它结尾
/// （`Expected a value >= 16, but got 1 instead.`），「改用另一个字段」由点名另一个字段名来判断
const REJECTION_WORDS: [&str; 13] = [
    "unsupported",
    "not supported",
    "unknown",
    "unrecognized",
    "unrecognised",
    "not allowed",
    "not permitted",
    "extra",
    "deprecated",
    "does not support",
    "not compatible",
    "incompatible",
    "unexpected",
];

/// 取值类的措辞：出现时说明是「值不对」而不是「字段不被支持」
const VALUE_WORDS: [&str; 22] = [
    ">=",
    "<=",
    "minimum",
    "maximum",
    "at least",
    "at most",
    "greater than",
    "less than",
    "between",
    "expected a value",
    "but got",
    "valid range",
    "out of range",
    "range of",
    "too large",
    "too small",
    "too big",
    "exceed",
    "integer",
    "invalid value",
    "must be a",
    "must be an",
];

/// 最大输出字段在错误信息里的各种写法
const MAX_TOKENS_NAMES: [&str; 4] = [
    "max_tokens",
    "max_completion_tokens",
    "max_output_tokens",
    "maxoutputtokens",
];

/// 信息是不是取值类的错误（「值不对」）
pub(crate) fn is_value_error(haystack: &str) -> bool {
    VALUE_WORDS.iter().any(|w| haystack.contains(w))
}

/// 最大输出的取值被拒（太小 / 超出范围）：提到了最大输出字段（或 minimum / at least），且是取值类的措辞。
/// 测试连接的探针据此不降级、改成 16 再试
pub(crate) fn rejects_small_max_tokens(haystack: &str) -> bool {
    let mentions = MAX_TOKENS_NAMES.iter().any(|n| haystack.contains(n))
        || haystack.contains("minimum")
        || haystack.contains("at least");
    mentions && is_value_error(haystack)
}

/// 从错误信息（已小写）里找出被拒的参数；可能不止一个
pub(crate) fn detect(sent: &[SentParam], haystack: &str) -> Vec<Param> {
    let rejected = REJECTION_WORDS.iter().any(|w| haystack.contains(w));
    let value_error = is_value_error(haystack);
    let mut found: Vec<Param> = Vec::new();
    for s in sent {
        if !s.names.iter().any(|n| contains_word(haystack, n)) {
            continue;
        }
        if let Param::MaxTokens(field) = s.param {
            // 点名了另一个字段（「Use 'max_completion_tokens' instead」）或明确说字段不被接受，且不是取值类的错误
            let other = match field {
                MaxTokensField::MaxTokens => "max_completion_tokens",
                MaxTokensField::MaxCompletionTokens => "max_tokens",
            };
            if value_error || !(rejected || contains_word(haystack, other)) {
                continue;
            }
        }
        if !found.contains(&s.param) {
            found.push(s.param);
        }
    }
    // 信息里同时提到 stream_options 与 include_obfuscation 时，只去掉 include_obfuscation
    if found.contains(&Param::Obfuscation) {
        found.retain(|p| *p != Param::StreamOptions);
    }
    found
}

/// 把降级结论写进能力缓存
pub(crate) fn apply(params: &[Param], caps: &mut ModelCaps) {
    for p in params {
        match *p {
            Param::Temperature => caps.temperature = Some(false),
            Param::MaxTokens(sent) => {
                caps.max_tokens_field = Some(match sent {
                    MaxTokensField::MaxTokens => MaxTokensField::MaxCompletionTokens,
                    MaxTokensField::MaxCompletionTokens => MaxTokensField::MaxTokens,
                });
            }
            Param::Effort => caps.effort_param = Some(false),
            Param::Thinking => caps.thinking_param = Some(false),
            Param::StreamOptions => caps.stream_options = Some(false),
            Param::Obfuscation => caps.include_obfuscation = Some(false),
        }
    }
}

/// `needle` 作为独立的词出现（前后都不是字母、数字或下划线）
pub(crate) fn contains_word(haystack: &str, needle: &str) -> bool {
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let h = haystack.as_bytes();
    let mut from = 0;
    while let Some(pos) = haystack[from..].find(needle) {
        let start = from + pos;
        let end = start + needle.len();
        let before_ok = start == 0 || !is_word(h[start - 1]);
        let after_ok = end >= h.len() || !is_word(h[end]);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
        while !haystack.is_char_boundary(from) {
            from += 1;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMP: SentParam = SentParam {
        param: Param::Temperature,
        names: &["temperature"],
    };
    const MAX: SentParam = SentParam {
        param: Param::MaxTokens(MaxTokensField::MaxTokens),
        names: &["max_tokens"],
    };
    const MAX_C: SentParam = SentParam {
        param: Param::MaxTokens(MaxTokensField::MaxCompletionTokens),
        names: &["max_completion_tokens"],
    };
    const SO: SentParam = SentParam {
        param: Param::StreamOptions,
        names: &["stream_options", "include_usage"],
    };
    const OBF: SentParam = SentParam {
        param: Param::Obfuscation,
        names: &["include_obfuscation", "obfuscation"],
    };
    const EFFORT: SentParam = SentParam {
        param: Param::Effort,
        names: &["reasoning_effort"],
    };

    #[test]
    fn openai_temperature() {
        let msg = "unsupported value: 'temperature' does not support 0.3 with this model. only the default (1) value is supported.";
        assert_eq!(detect(&[TEMP, MAX_C, EFFORT], msg), vec![Param::Temperature]);
    }

    #[test]
    fn max_tokens_swap_needs_rejection_wording() {
        let msg = "unsupported parameter: 'max_tokens' is not supported with this model. use 'max_completion_tokens' instead.";
        assert_eq!(
            detect(&[TEMP, MAX], msg),
            vec![Param::MaxTokens(MaxTokensField::MaxTokens)]
        );
        // 这次发的是 max_completion_tokens，信息里提到的 max_tokens 不是它
        assert_eq!(detect(&[MAX_C], "max_tokens is too large: 100000"), Vec::<Param>::new());
        // 取值错误不触发互换
        assert_eq!(detect(&[MAX], "max_tokens must be at least 16"), Vec::<Param>::new());
        // 只点名了另一个字段、没有「不支持」字样，也算改用
        assert_eq!(
            detect(
                &[MAX],
                "use 'max_completion_tokens' instead of 'max_tokens' with this model."
            ),
            vec![Param::MaxTokens(MaxTokensField::MaxTokens)]
        );
    }

    #[test]
    fn value_errors_ending_with_instead_do_not_swap_the_field() {
        // 火山方舟 / OpenAI 风格的取值校验：以 instead 结尾，但说的是值不对
        let volcengine = "invalid 'max_tokens': expected a value >= 16, but got 1 instead.";
        assert_eq!(detect(&[TEMP, MAX], volcengine), Vec::<Param>::new());
        let openai =
            "invalid 'max_completion_tokens': integer below minimum value. expected a value >= 1, but got 0 instead.";
        assert_eq!(detect(&[MAX_C], openai), Vec::<Param>::new());
        let deepseek = "invalid max_tokens value, the valid range of max_tokens is [1, 8192]";
        assert_eq!(detect(&[MAX], deepseek), Vec::<Param>::new());
        assert!(rejects_small_max_tokens(volcengine));
        assert!(rejects_small_max_tokens(openai));
        assert!(rejects_small_max_tokens("max_output_tokens must be at least 16"));
        // 字段本身不被支持：不是取值错误
        let unsupported = "unsupported parameter: 'max_tokens' is not supported with this model. use 'max_completion_tokens' instead.";
        assert!(!rejects_small_max_tokens(unsupported));
        assert!(!rejects_small_max_tokens("unknown field `max_completion_tokens`"));
    }

    #[test]
    fn obfuscation_only_removes_itself() {
        let msg = "unrecognized request argument supplied: stream_options.include_obfuscation";
        assert_eq!(detect(&[SO, OBF], msg), vec![Param::Obfuscation]);
        assert_eq!(
            detect(&[SO, OBF], "unknown parameter: stream_options"),
            vec![Param::StreamOptions]
        );
    }

    #[test]
    fn word_boundaries() {
        assert!(contains_word("'thinking' is not allowed", "thinking"));
        assert!(!contains_word("enable_thinking is bad", "thinking"));
        assert!(contains_word("reasoning.effort", "reasoning"));
        assert!(!contains_word("max_tokens_to_sample", "max_tokens"));
        assert!(contains_word("参数 temperature 不支持", "temperature"));
    }

    #[test]
    fn apply_swaps_and_removes() {
        let mut caps = ModelCaps::default();
        apply(
            &[
                Param::Temperature,
                Param::MaxTokens(MaxTokensField::MaxCompletionTokens),
                Param::Effort,
                Param::Thinking,
                Param::Obfuscation,
            ],
            &mut caps,
        );
        assert_eq!(caps.temperature, Some(false));
        assert_eq!(caps.max_tokens_field, Some(MaxTokensField::MaxTokens));
        assert_eq!(caps.effort_param, Some(false));
        assert_eq!(caps.thinking_param, Some(false));
        assert_eq!(caps.include_obfuscation, Some(false));
        assert_eq!(caps.stream_options, None);
    }
}
