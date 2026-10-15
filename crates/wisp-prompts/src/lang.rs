//! 语言检测与方向判定；whatlang 只给长拉丁文本兜底。
//!
//! 规则（v1 只分中英，按文字统计为主）：
//! 1. 有假名 → ja，有谚文 → ko（两者都有时取多的一方）；
//! 2. 汉字数 han ≥ 1 且 han ≥ 0.5 × 拉丁词数 → zh；否则 → en；
//! 3. 没有拉丁字母也没有汉字（纯数字、符号、emoji）→ other，低置信度；以西里尔等其他文字为主 → other，低置信度；
//! 4. 中英混排且比值落在阈值 ±30% 以内 → 低置信度（方向仍按阈值判断）；
//! 5. 短文本只按文字判断；较长的纯拉丁文本再用 whatlang 兜底，**只用来降低置信度**，语言仍是 en：
//!    判为非英语（无论可靠与否）或无法判断 → en + 低置信度（例如法语句子 → 目标 zh-CN，
//!    source 写 the source language）。方向因此与 whatlang 的 is_reliable 无关：同为拉丁文字的西语、法语、德语
//!    在任何主次语言设置下都走同一个方向。
//!
//!    例外：whatlang 判为**英语**但 `is_reliable=false` 时不降级。普通英文短句大多如此
//!    （"Please check again when you have time." 置信度 0.24、"The meeting has been moved to Thursday afternoon."
//!    0.90，都不可靠），否则会让大部分日常英文的语言条显示低置信度、提示词写 the source language。

use wisp_types::Lang;

use crate::script::Census;

/// 检测结果
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Detection {
    pub lang: Lang,
    pub low_confidence: bool,
}

/// 汉字 / 拉丁词数的阈值
const HAN_RATIO: f64 = 0.5;
/// 低置信度带：阈值的 ±30%
const AMBIGUITY_BAND: f64 = 0.3;
/// 少于这么多字符的文本不调用 whatlang（短文本只按文字判断）
const STATISTICAL_MIN_CHARS: u32 = 20;
/// 至少这么多拉丁词才调用 whatlang
const STATISTICAL_MIN_WORDS: u32 = 3;
/// 交给 whatlang 的最大字节数（取开头一段）。实测 whatlang 的耗时以固定开销为主（与各语言的 trigram 表逐一比对，
/// 约 60 µs，128 B 与 1 KB 相差不到 10%），截断只是避免长文本额外线性增长；8000 字符纯英文的完整流水线
/// p99 约 0.2 ms，由基准门禁守住 1 ms 的预算
const STATISTICAL_SAMPLE_BYTES: usize = 1024;

/// 检测原文语言
pub fn detect_lang(text: &str) -> Detection {
    let census = Census::of(text);
    detect_from_census(text, &census)
}

pub(crate) fn detect_from_census(text: &str, c: &Census) -> Detection {
    let high = |lang| Detection {
        lang,
        low_confidence: false,
    };
    let low = |lang| Detection {
        lang,
        low_confidence: true,
    };

    if c.kana > 0 || c.hangul > 0 {
        return high(if c.hangul > c.kana { Lang::Ko } else { Lang::Ja });
    }
    if c.han == 0 && c.latin_letters == 0 {
        // 纯数字 / 符号 / emoji，或以其他文字（西里尔、希腊、阿拉伯……）写成
        return low(Lang::Other);
    }
    if c.han == 0 && c.other_letters > c.latin_letters {
        return low(Lang::Other);
    }

    let han = f64::from(c.han);
    let words = f64::from(c.latin_words);
    if c.han > 0 {
        let threshold = HAN_RATIO * words;
        let lang = if han >= threshold { Lang::ZhCn } else { Lang::En };
        // 混排比值在阈值 ±30% 以内为低置信度（只有拉丁词时才谈得上「混排」）
        let ambiguous = c.latin_words > 0 && (han - threshold).abs() <= AMBIGUITY_BAND * threshold;
        return Detection {
            lang,
            low_confidence: ambiguous,
        };
    }

    // 纯拉丁文本：短文本直接视为英文；较长的再用 whatlang 兜底
    if c.chars < STATISTICAL_MIN_CHARS || c.latin_words < STATISTICAL_MIN_WORDS {
        return high(Lang::En);
    }
    match whatlang::detect(sample(text)) {
        Some(info) if info.lang() == whatlang::Lang::Eng => high(Lang::En),
        // 判为别的拉丁语言（可靠与否都一样）或无法判断：仍按英文处理，只降为低置信度
        _ => low(Lang::En),
    }
}

/// 取开头最多 `STATISTICAL_SAMPLE_BYTES` 字节（落在字符边界上）
fn sample(text: &str) -> &str {
    let text = text.trim_start();
    if text.len() <= STATISTICAL_SAMPLE_BYTES {
        return text;
    }
    let mut end = STATISTICAL_SAMPLE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// 自动方向：原文是主语言时译成次语言，否则译成主语言
///
/// 低置信度不改变方向：混排落在阈值附近、whatlang 判为非英语的拉丁文本都按 `lang` 判断；
/// `Other`（纯数字 / 符号 / emoji、西里尔等其他文字）不等于任何主语言，译成主语言。
pub fn target_for(det: &Detection, primary: Lang, secondary: Lang) -> Lang {
    if det.lang == primary { secondary } else { primary }
}

/// 提示词里用的英文语言名；`Other` 没有具体名字
pub(crate) fn lang_name(lang: Lang) -> Option<&'static str> {
    match lang {
        Lang::ZhCn => Some("Simplified Chinese"),
        Lang::En => Some("English"),
        Lang::Ja => Some("Japanese"),
        Lang::Ko => Some("Korean"),
        Lang::Other => None,
    }
}

/// 低置信度或语言未知时 source 的写法
pub(crate) const UNKNOWN_SOURCE_LANG: &str = "the source language";
/// 同上，用在限定词之后作定语时的写法（"a single source-language word"、"the source-language text"）
pub(crate) const UNKNOWN_SOURCE_LANG_ATTR: &str = "source-language";

/// `{{source_lang}}` 的渲染值：低置信度或 other 时为 "the source language"
pub(crate) fn source_lang_name(det: &Detection) -> &'static str {
    match lang_name(det.lang) {
        Some(name) if !det.low_confidence => name,
        _ => UNKNOWN_SOURCE_LANG,
    }
}

/// `{{source_lang}}` 紧跟在限定词之后（模板写成 "a single {{source_lang}} word"）时的渲染值：
/// 语言已知时与 [`source_lang_name`] 相同；低置信度时为 "source-language"，避免 "a single the source language word"
pub(crate) fn source_lang_attr(det: &Detection) -> &'static str {
    match lang_name(det.lang) {
        Some(name) if !det.low_confidence => name,
        _ => UNKNOWN_SOURCE_LANG_ATTR,
    }
}

/// `{{target_lang}}` 的渲染值：总是具体语言名（目标不会是 other；万一是，按主目标语言 zh-CN 处理）
pub(crate) fn target_lang_name(target: Lang) -> &'static str {
    lang_name(target).unwrap_or("Simplified Chinese")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_respects_char_boundaries() {
        let s = "汉".repeat(1000);
        let cut = sample(&s);
        assert!(cut.len() <= STATISTICAL_SAMPLE_BYTES);
        assert!(cut.chars().all(|c| c == '汉'));
    }

    #[test]
    fn names() {
        let d = |lang, low| Detection {
            lang,
            low_confidence: low,
        };
        assert_eq!(source_lang_name(&d(Lang::ZhCn, false)), "Simplified Chinese");
        assert_eq!(source_lang_name(&d(Lang::En, false)), "English");
        assert_eq!(source_lang_name(&d(Lang::Ja, false)), "Japanese");
        assert_eq!(source_lang_name(&d(Lang::Ko, false)), "Korean");
        assert_eq!(source_lang_name(&d(Lang::En, true)), "the source language");
        assert_eq!(source_lang_name(&d(Lang::Other, false)), "the source language");
        assert_eq!(target_lang_name(Lang::En), "English");
        assert_eq!(target_lang_name(Lang::Other), "Simplified Chinese");
    }
}
