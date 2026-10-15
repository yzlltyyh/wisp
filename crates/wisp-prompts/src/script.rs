//! 文字（script）分类与单遍统计：语言检测、词典式判定、token 估算、术语整词边界共用同一套定义。
//!
//! 只做 v1 需要的粗分类（汉字 / 假名 / 谚文 / 拉丁字母 / 数字 / 其他字母），全部按码位区间判断，不分配内存。

/// 字符的粗分类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CharClass {
    /// 汉字（CJK 统一表意文字及扩展区、兼容表意文字、〇）
    Han,
    /// 平假名 / 片假名（不含中日共用的「・」「ー」）
    Kana,
    /// 谚文音节与字母
    Hangul,
    /// 拉丁字母（ASCII、Latin-1 补充、扩展 A/B、IPA、扩展附加、全角拉丁）
    Latin,
    /// 数字（ASCII 与全角）
    Digit,
    /// 其他文字的字母（西里尔、希腊、阿拉伯、泰文……）
    OtherLetter,
    /// 空白
    Space,
    /// 标点、符号、emoji 等
    Symbol,
}

/// 对单个字符分类
pub(crate) fn classify(c: char) -> CharClass {
    if c.is_ascii() {
        return if c.is_ascii_alphabetic() {
            CharClass::Latin
        } else if c.is_ascii_digit() {
            CharClass::Digit
        } else if c.is_ascii_whitespace() {
            CharClass::Space
        } else {
            CharClass::Symbol
        };
    }
    let u = c as u32;
    match u {
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x3007 | 0x20000..=0x323AF => CharClass::Han,
        0x3041..=0x3096
        | 0x309D..=0x309F
        | 0x30A1..=0x30FA
        | 0x30FD..=0x30FF
        | 0x31F0..=0x31FF
        | 0xFF66..=0xFF6F
        | 0xFF71..=0xFF9D => CharClass::Kana,
        0xAC00..=0xD7A3 | 0x1100..=0x11FF | 0x3131..=0x318E | 0xA960..=0xA97F | 0xD7B0..=0xD7FF => CharClass::Hangul,
        // 中日共用的中点与长音符：按符号处理，避免把含「・」的中文判成日文
        0x30FB | 0x30FC => CharClass::Symbol,
        0xFF10..=0xFF19 => CharClass::Digit,
        0xFF21..=0xFF3A | 0xFF41..=0xFF5A => CharClass::Latin,
        0x00C0..=0x024F | 0x0250..=0x02AF | 0x1E00..=0x1EFF if c.is_alphabetic() => CharClass::Latin,
        _ if c.is_whitespace() => CharClass::Space,
        _ if c.is_alphabetic() => CharClass::OtherLetter,
        _ => CharClass::Symbol,
    }
}

/// 是否为 CJK 表意 / 音节文字（汉字、假名、谚文）
pub(crate) fn is_cjk_letter(c: char) -> bool {
    matches!(classify(c), CharClass::Han | CharClass::Kana | CharClass::Hangul)
}

/// token 估算时按「每字 1 个」计的宽字符：CJK 文字，以及 CJK 标点、全角形式、注音
pub(crate) fn is_wide_token_char(c: char) -> bool {
    if is_cjk_letter(c) {
        return true;
    }
    matches!(c as u32, 0x3000..=0x303F | 0xFF00..=0xFFEF | 0xFE30..=0xFE4F | 0x3100..=0x312F | 0x31A0..=0x31BF)
}

/// 术语整词边界与拉丁词计数用的「词字符」：拉丁 / 其他字母、数字、下划线（CJK 不算，中文里紧挨英文照样算词边界）
pub(crate) fn is_word_char(c: char) -> bool {
    c == '_'
        || matches!(
            classify(c),
            CharClass::Latin | CharClass::Digit | CharClass::OtherLetter
        )
}

/// 词内连接符：撇号与连字符不拆词
fn is_word_joiner(c: char) -> bool {
    matches!(c, '\'' | '\u{2019}' | '-' | '\u{2010}' | '\u{2011}')
}

/// 单遍统计结果
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Census {
    pub han: u32,
    pub kana: u32,
    pub hangul: u32,
    pub latin_letters: u32,
    /// 含至少一个拉丁字母的词（拉丁字母 / 数字 / 下划线组成的连续段，撇号与连字符位于词内时不拆词）
    pub latin_words: u32,
    pub other_letters: u32,
    pub digits: u32,
    /// 全部字符数
    pub chars: u32,
    /// 按「每字 1 个 token」计的宽字符数
    pub wide: u32,
}

impl Census {
    /// 统计一段文本（O(n)，不分配）
    pub fn of(text: &str) -> Self {
        let mut s = Self::default();
        // 当前词的状态：是否在词内、词内是否出现过拉丁字母、上一个字符是否为连接符
        let mut in_word = false;
        let mut word_has_latin = false;
        let mut pending_joiner = false;
        for c in text.chars() {
            s.chars += 1;
            if is_wide_token_char(c) {
                s.wide += 1;
            }
            let class = classify(c);
            match class {
                CharClass::Han => s.han += 1,
                CharClass::Kana => s.kana += 1,
                CharClass::Hangul => s.hangul += 1,
                CharClass::Latin => s.latin_letters += 1,
                CharClass::Digit => s.digits += 1,
                CharClass::OtherLetter => s.other_letters += 1,
                CharClass::Space | CharClass::Symbol => {}
            }
            let word_char = c == '_' || matches!(class, CharClass::Latin | CharClass::Digit);
            if word_char {
                if !in_word {
                    in_word = true;
                    word_has_latin = false;
                }
                pending_joiner = false;
                if class == CharClass::Latin {
                    word_has_latin = true;
                }
            } else if in_word && !pending_joiner && is_word_joiner(c) {
                // 连接符只有后面紧跟词字符时才算词内，先挂起
                pending_joiner = true;
            } else if in_word {
                if word_has_latin {
                    s.latin_words += 1;
                }
                in_word = false;
                pending_joiner = false;
            }
        }
        if in_word && word_has_latin {
            s.latin_words += 1;
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_ranges() {
        let cases = [
            ('a', CharClass::Latin),
            ('Z', CharClass::Latin),
            ('é', CharClass::Latin),
            ('Ａ', CharClass::Latin),
            ('7', CharClass::Digit),
            ('７', CharClass::Digit),
            ('内', CharClass::Han),
            ('〇', CharClass::Han),
            ('𠀀', CharClass::Han),
            ('の', CharClass::Kana),
            ('カ', CharClass::Kana),
            ('ｶ', CharClass::Kana),
            ('・', CharClass::Symbol),
            ('ー', CharClass::Symbol),
            ('한', CharClass::Hangul),
            ('ㄱ', CharClass::Hangul),
            ('Ж', CharClass::OtherLetter),
            ('λ', CharClass::OtherLetter),
            (' ', CharClass::Space),
            ('\u{3000}', CharClass::Space),
            ('，', CharClass::Symbol),
            ('💀', CharClass::Symbol),
            ('×', CharClass::Symbol),
        ];
        for (c, want) in cases {
            assert_eq!(classify(c), want, "{c:?}");
        }
    }

    #[test]
    fn census_counts_words_by_script_runs() {
        let s = Census::of("我们用 Rust 写 core，前端用 React + Motion，IPC 走 Tauri 的 invoke，目前 latency 还 ok。");
        assert_eq!(s.han, 12);
        assert_eq!(s.latin_words, 9);

        // 连字符与撇号不拆词；纯数字段不算拉丁词；下划线连接标识符
        let s = Census::of("state-of-the-art isn't v1.2 2024 snake_case -x x-");
        assert_eq!(s.latin_words, 6, "{s:?}");

        // 中文里紧挨的英文单独成词
        let s = Census::of("使用token进行认证");
        assert_eq!(s.latin_words, 1);
        assert_eq!(s.han, 6);
    }

    #[test]
    fn wide_chars_include_cjk_punctuation() {
        let s = Census::of("你好，世界！ab");
        assert_eq!(s.wide, 6);
        assert_eq!(s.chars, 8);
    }
}
