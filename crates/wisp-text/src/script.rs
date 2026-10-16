//! 字符按书写习惯分类。

/// 是否是不用空格分词的东亚文字：汉字（基本区、扩展 A 及扩展 B 以后、兼容汉字）、CJK 部首、平假名与片假名、注音、
/// CJK 符号与标点、竖排标点，以及全角与半角形式。这些字符彼此相邻时不加空格。
///
/// 不含谚文：韩文以空格分词，与拉丁文一样，折行续接时要补空格。
#[inline]
pub fn is_unspaced(c: char) -> bool {
    matches!(
        c as u32,
        0x2E80..=0x2FDF      // CJK 部首
            | 0x3000..=0x303F // CJK 符号与标点
            | 0x3040..=0x30FF // 平假名、片假名
            | 0x3100..=0x312F // 注音
            | 0x31F0..=0x31FF // 片假名扩展
            | 0x3400..=0x4DBF // 扩展 A
            | 0x4E00..=0x9FFF // 基本汉字
            | 0xF900..=0xFAFF // 兼容汉字
            | 0xFE30..=0xFE4F // 竖排标点
            | 0xFF00..=0xFFEF // 全角与半角形式
            | 0x20000..=0x3134F // 扩展 B 及以后
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn east_asian_scripts_without_word_spaces() {
        for c in [
            '汉', '㐀', '𠀀', '豈', 'あ', 'カ', 'ㇰ', 'ㄅ', '⺀', '、', '。', '「', '（', '，', 'Ａ', '１', '｡', '︵',
        ] {
            assert!(is_unspaced(c), "{c:?}");
        }
    }

    #[test]
    fn spaced_scripts_and_general_punctuation() {
        // 谚文（音节、字母、兼容字母）按空格分词；通用标点里的破折号、省略号不属于东亚文字
        for c in ['한', 'ᄀ', 'ㄱ', 'a', 'Z', '1', 'é', 'Ж', ' ', '-', '—', '…', '“', '•'] {
            assert!(!is_unspaced(c), "{c:?}");
        }
    }
}
