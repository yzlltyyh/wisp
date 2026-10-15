//! token 估算与最大输出公式。

use crate::script::Census;

/// 翻译类请求的最大输出上限
const MAX_OUTPUT_CAP: u32 = 4096;
/// 推理模型（或会思考的协议）的最大输出下限
const REASONING_FLOOR: u32 = 1024;
/// OCR 请求固定的最大输出
pub(crate) const OCR_MAX_OUTPUT: u32 = 4096;

/// CJK 每字计 1，其余字符每 4 个计 1，向上取整
///
/// 「CJK」包括汉字、假名、谚文，以及 CJK 标点和全角形式；其余字符（含空白）每 4 个计 1。
pub fn estimate_tokens(text: &str) -> u32 {
    estimate_from_census(&Census::of(text))
}

pub(crate) fn estimate_from_census(c: &Census) -> u32 {
    let other = c.chars - c.wide;
    c.wide + other.div_ceil(4)
}

/// min(4096, 3×in+256)；推理模型下限 1024
pub fn max_tokens_translate(input_tokens: u32, reasoning: bool) -> u32 {
    let v = input_tokens.saturating_mul(3).saturating_add(256).min(MAX_OUTPUT_CAP);
    if reasoning { v.max(REASONING_FLOOR) } else { v }
}

/// 词数 + 汉字数/1.5 ≥ 40
///
/// 等价于 3 × 词数 + 2 × 汉字数 ≥ 120（整数运算）：纯英文 ≥ 40 词、纯中文 ≥ 60 字，混排按比例折算。
pub fn refine_eligible(text: &str) -> bool {
    refine_eligible_census(&Census::of(text))
}

pub(crate) fn refine_eligible_census(c: &Census) -> bool {
    3 * u64::from(c.latin_words) + 2 * u64::from(c.han) >= 120
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_rules() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("a"), 1);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(estimate_tokens("你好"), 2);
        assert_eq!(estimate_tokens("你好，world"), 3 + 2);
        assert_eq!(estimate_tokens("こんにちは"), 5);
        assert_eq!(estimate_tokens(&"x".repeat(8000)), 2000);
    }

    #[test]
    fn max_tokens_formula() {
        assert_eq!(max_tokens_translate(10, false), 286);
        assert_eq!(max_tokens_translate(100, false), 556);
        assert_eq!(max_tokens_translate(2000, false), 4096);
        assert_eq!(max_tokens_translate(u32::MAX, false), 4096);
        assert_eq!(max_tokens_translate(0, false), 256);
        // 推理模型下限 1024
        assert_eq!(max_tokens_translate(10, true), 1024);
        assert_eq!(max_tokens_translate(500, true), 1756);
        assert_eq!(max_tokens_translate(2000, true), 4096);
    }

    #[test]
    fn refine_threshold() {
        let words = |n: usize| vec!["word"; n].join(" ");
        assert!(!refine_eligible(&words(30)));
        assert!(!refine_eligible(&words(39)));
        assert!(refine_eligible(&words(40)));
        assert!(refine_eligible(&words(120)));
        assert!(!refine_eligible(&"字".repeat(59)));
        assert!(refine_eligible(&"字".repeat(60)));
        // 混排：20 词 + 30 字 = 20 + 20 = 40
        assert!(refine_eligible(&format!("{} {}", words(20), "字".repeat(30))));
        assert!(!refine_eligible(&format!("{} {}", words(20), "字".repeat(29))));
    }
}
