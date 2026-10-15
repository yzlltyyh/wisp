//! 词典式触发判定。
//!
//! 全部满足才触发：
//! 1. trim 后为单行；
//! 2. 没有句末标点 `. ! ? 。 ！ ？`（夹在两个字母 / 数字之间的 `.` 不是句末标点，如 `Node.js`、`v1.2`）；
//! 3. 不是 URL（`scheme://` 或 `www.` 开头）或路径（`/` `~/` `./` `../` 开头、盘符开头、含 2 个及以上 `/` 或 `\`）；
//! 4. 字母文字 ≤ 3 个词（空白分词，连字符和撇号不拆词）且 ≤ 30 字符；或不含字母文字的 CJK 非空白字符 ≤ 4；
//!    中英混排、纯数字或符号都不触发；
//! 5. 风格的 `dictionary_mode = true`（见 [`use_dictionary`]）。

use wisp_types::{DictMode, Style};

use crate::script::{CharClass, classify};

/// 拉丁文本的最大词数
const MAX_WORDS: usize = 3;
/// 拉丁文本的最大字符数
const MAX_LATIN_CHARS: usize = 30;
/// CJK 文本的最大非空白字符数
const MAX_CJK_CHARS: usize = 4;
/// 超过这个字节数的输入不可能满足条件（30 个字符 × 4 字节，留出余量），直接返回
const FAST_REJECT_BYTES: usize = 256;

/// 输入是否像单词 / 短语（适合词典式）
pub fn is_dictionary_input(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() || t.len() > FAST_REJECT_BYTES {
        return false;
    }
    if t.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
        return false;
    }
    if has_sentence_punctuation(t) || is_url(t) || is_path(t) {
        return false;
    }

    let (mut alpha, mut cjk, mut non_space) = (0usize, 0usize, 0usize);
    for c in t.chars() {
        match classify(c) {
            CharClass::Latin | CharClass::OtherLetter => alpha += 1,
            CharClass::Han | CharClass::Kana | CharClass::Hangul => cjk += 1,
            CharClass::Digit | CharClass::Symbol => {}
            CharClass::Space => continue,
        }
        non_space += 1;
    }
    match (alpha > 0, cjk > 0) {
        // 中英混排不触发
        (true, true) => false,
        (true, false) => t.chars().count() <= MAX_LATIN_CHARS && t.split_whitespace().count() <= MAX_WORDS,
        (false, true) => non_space <= MAX_CJK_CHARS,
        // 纯数字或符号不触发
        (false, false) => false,
    }
}

/// 本次是否走词典式：强制模式优先，其次看风格开关与输入形态
pub fn use_dictionary(text: &str, style: &Style, force: DictMode) -> bool {
    match force {
        DictMode::Dictionary => true,
        DictMode::Prose => false,
        DictMode::Auto => style.dictionary_mode && is_dictionary_input(text),
    }
}

/// 是否含句末标点。`!` `?` 与全角句末标点一律算；`.` 只有夹在两个 ASCII 字母 / 数字之间时不算
fn has_sentence_punctuation(t: &str) -> bool {
    let mut prev: Option<char> = None;
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '!' | '?' | '。' | '！' | '？' | '｡' => return true,
            '.' | '．' => {
                let inner = prev.is_some_and(|p| p.is_ascii_alphanumeric())
                    && chars.peek().is_some_and(|n| n.is_ascii_alphanumeric());
                if !inner {
                    return true;
                }
            }
            _ => {}
        }
        prev = Some(c);
    }
    false
}

/// `scheme://` 或 `www.` 开头
fn is_url(t: &str) -> bool {
    if t.len() >= 4 && t.as_bytes()[..4].eq_ignore_ascii_case(b"www.") {
        return true;
    }
    let Some(pos) = t.find("://") else {
        return false;
    };
    let scheme = &t[..pos];
    let mut bytes = scheme.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphabetic())
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
}

/// 路径：`/` `~/` `./` `../`（及反斜杠写法）开头、盘符开头，或含 2 个及以上 `/` `\`
fn is_path(t: &str) -> bool {
    const PREFIXES: [&str; 7] = ["/", "~/", "./", "../", "~\\", ".\\", "..\\"];
    if PREFIXES.iter().any(|p| t.starts_with(p)) {
        return true;
    }
    let b = t.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/') {
        return true;
    }
    t.bytes().filter(|&c| c == b'/' || c == b'\\').count() >= 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn punctuation_rules() {
        assert!(has_sentence_punctuation("Hello."));
        assert!(has_sentence_punctuation("Mr. Smith"));
        assert!(has_sentence_punctuation(".env"));
        assert!(has_sentence_punctuation("Yahoo!"));
        assert!(has_sentence_punctuation("真的？"));
        assert!(!has_sentence_punctuation("Node.js"));
        assert!(!has_sentence_punctuation("v1.2.3"));
        assert!(!has_sentence_punctuation("token, coin"));
    }

    #[test]
    fn url_and_path_rules() {
        for t in ["https://a.b", "HTTP://x", "git+ssh://h", "www.example.com", "WWW.x"] {
            assert!(is_url(t), "{t}");
        }
        for t in ["ftp:/x", "a b://c", "://x", "1http://x"] {
            assert!(!is_url(t), "{t}");
        }
        for t in [
            "/usr", "~/x", "./a", "../a", "C:\\x", "d:/x", "a/b/c", "a\\b\\c", "a/b\\c",
        ] {
            assert!(is_path(t), "{t}");
        }
        for t in ["and/or", "TCP/IP", "C:", "a\\b"] {
            assert!(!is_path(t), "{t}");
        }
    }
}
