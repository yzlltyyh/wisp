//! 文本规范化、预览与元信息。

/// HTML / RTF 规范化后的上限（超过就丢弃该格式，只保留纯文本）
pub(crate) const MAX_RICH_BYTES: usize = 256 * 1024;
/// `text_preview` 的最大字符数
pub(crate) const PREVIEW_CHARS: usize = 200;

/// 规范化纯文本：只去掉**末尾**空白（首部保留，不改换行）；超过 `max_bytes` 时在字符边界截断。
/// 返回（规范化后的文本，是否截断）
pub(crate) fn normalize_text(text: &str, max_bytes: usize) -> (&str, bool) {
    let trimmed = text.trim_end();
    if trimmed.len() <= max_bytes {
        return (trimmed, false);
    }
    let mut end = max_bytes;
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    (trimmed[..end].trim_end(), true)
}

/// 列表预览：前 `max_chars` 个字符，任何空白（含换行、制表符）与控制字符的连续段压缩为一个空格，去掉首尾空白。
/// 只扫描需要的前缀，长文本不会被整篇遍历
pub(crate) fn make_preview(text: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max_chars.saturating_mul(3)));
    let mut count = 0;
    let mut pending_space = false;
    for c in text.chars() {
        if c.is_whitespace() || c.is_control() {
            pending_space = count > 0;
            continue;
        }
        if pending_space {
            if count + 1 >= max_chars {
                break;
            }
            out.push(' ');
            count += 1;
            pending_space = false;
        }
        out.push(c);
        count += 1;
        if count >= max_chars {
            break;
        }
    }
    out
}

/// 字符数（Unicode 标量值）
pub(crate) fn char_count(text: &str) -> u32 {
    u32::try_from(text.chars().count()).unwrap_or(u32::MAX)
}

/// 行数：换行（`\r\n`、`\n`、`\r` 各算一次）+ 1；空串为 0
pub(crate) fn line_count(text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    let b = text.as_bytes();
    let mut lines: u32 = 1;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\n' => lines = lines.saturating_add(1),
            b'\r' => {
                lines = lines.saturating_add(1);
                if b.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    lines
}

/// 文件路径的显示名（最后一段；Windows 与 POSIX 分隔符都识别，取不到时用路径本身）
pub(crate) fn file_name_of(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    let name = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    if name.is_empty() {
        path.to_owned()
    } else {
        name.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_only_trailing_whitespace() {
        assert_eq!(normalize_text("  hello \n\t ", 1024), ("  hello", false));
        assert_eq!(normalize_text("a\r\nb\n\n", 1024), ("a\r\nb", false));
        assert_eq!(normalize_text("\n\n  x", 1024), ("\n\n  x", false));
        assert_eq!(normalize_text(" \n ", 1024), ("", false));
    }

    #[test]
    fn truncates_on_char_boundary() {
        // 「中」占 3 字节：4 字节上限只能容纳 1 个
        assert_eq!(normalize_text("中文字", 4), ("中", true));
        assert_eq!(normalize_text("abcdef", 3), ("abc", true));
        assert_eq!(normalize_text("ab   cdef", 4), ("ab", true));
        assert_eq!(normalize_text("abc", 3), ("abc", false));
    }

    #[test]
    fn preview_compresses_whitespace() {
        assert_eq!(make_preview("  a \n\n b\t\tc  ", 200), "a b c");
        assert_eq!(make_preview("a\u{0}\u{7}b", 200), "a b");
        assert_eq!(make_preview("", 200), "");
        assert_eq!(make_preview("\n\n", 200), "");
        let long = "字".repeat(500);
        assert_eq!(make_preview(&long, 200).chars().count(), 200);
        // 截断点恰好在空白处时不留尾随空格
        let p = make_preview(&format!("{} b", "a".repeat(199)), 200);
        assert_eq!(p, "a".repeat(199));
        let p = make_preview(&format!("{} bcd", "a".repeat(198)), 200);
        assert_eq!(p, format!("{} b", "a".repeat(198)));
    }

    #[test]
    fn counts() {
        assert_eq!(char_count("a中😀"), 3);
        assert_eq!(line_count(""), 0);
        assert_eq!(line_count("a"), 1);
        assert_eq!(line_count("a\nb"), 2);
        assert_eq!(line_count("a\r\nb\rc\n"), 4);
    }

    #[test]
    fn file_names() {
        assert_eq!(file_name_of("/Users/me/a.txt"), "a.txt");
        assert_eq!(file_name_of("C:\\Users\\me\\b.pdf"), "b.pdf");
        assert_eq!(file_name_of("/Users/me/dir/"), "dir");
        assert_eq!(file_name_of("/"), "/");
    }

    proptest::proptest! {
        /// preview：≤ 上限个字符，无首尾空白、无连续空格、无控制字符
        #[test]
        fn preview_invariants(s in "\\PC{0,300}|[ \\t\\n\\r\\u{0}-\\u{1f}a-z中😀]{0,300}", max in 1usize..250) {
            let p = make_preview(&s, max);
            proptest::prop_assert!(p.chars().count() <= max);
            proptest::prop_assert!(!p.starts_with(' ') && !p.ends_with(' '));
            proptest::prop_assert!(!p.contains("  "));
            proptest::prop_assert!(!p.chars().any(|c| c.is_control() || (c.is_whitespace() && c != ' ')));
        }

        /// 规范化：结果是原文前缀、不超过上限、末尾无空白
        #[test]
        fn normalize_invariants(s in "\\PC{0,200}|[ a中\\n]{0,50}", max in 0usize..400) {
            let (n, truncated) = normalize_text(&s, max);
            proptest::prop_assert!(s.starts_with(n));
            proptest::prop_assert!(n.len() <= max || !truncated);
            proptest::prop_assert!(n.len() <= max.max(s.trim_end().len()));
            proptest::prop_assert_eq!(n, n.trim_end());
        }
    }
}
