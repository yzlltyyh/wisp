//! 两段文字的拼接：同一视觉行内相邻的片段、跨行续接的段落，以及列表项的判定（列表项总是另起一段）。

use crate::script::is_unspaced;

/// 把下一行接到段落末尾（跨行续接）：英文行尾的连字符断词还原（`milli-` 接 `seconds` 得 `milliseconds`）；
/// 不用空格分词的东亚文字之间直接相连；其余情况加一个空格。
///
/// 只有「连字符前是 ASCII 字母、下一行以 ASCII 小写字母开头」才算断词。
pub fn join_line(text: &mut String, next: &str) {
    let mut tail = text.chars().rev();
    let hyphenated = matches!(
        (tail.next(), tail.next(), next.chars().next()),
        (Some('-'), Some(a), Some(b)) if a.is_ascii_alphabetic() && b.is_ascii_lowercase()
    );
    if hyphenated {
        text.pop();
    } else if needs_space(text, next, None) {
        text.push(' ');
    }
    text.push_str(next);
}

/// `a` 与 `b` 前后拼接时中间是否要加空格。
///
/// `gap` 是两者在版面上的水平间距与行高之比：同一视觉行内拼接相邻片段时由调用方从文本框算出，跨行续接时为 `None`。
/// 只在一侧是不用空格分词的东亚文字（[`is_unspaced`]）、另一侧不是时才看它：间距超过半个行高（视觉上明显分开，
/// 约半个汉字宽）才加空格，没有间距信息时不加。两侧都是这类文字时不加；都不是时加；任一侧已是空白或为空时不加。
pub fn needs_space(a: &str, b: &str, gap: Option<f32>) -> bool {
    let (Some(x), Some(y)) = (a.chars().next_back(), b.chars().next()) else {
        return false;
    };
    if x.is_whitespace() || y.is_whitespace() {
        return false;
    }
    match (is_unspaced(x), is_unspaced(y)) {
        (true, true) => false,
        (true, false) | (false, true) => gap.is_some_and(|g| g > 0.5),
        (false, false) => true,
    }
}

/// 这一行是否以列表项开头：项目符号（`•`、`●`、`■`、`►`、`✓`、`☐` 等），后跟空格的 `-` / `*` / `+`，
/// 或「1.」「1)」「(1)」「（1）」「1、」「①」这类最多三位的编号。「1.5 GB」「-5 degrees」「*bold*」这类不算。
pub fn starts_list_item(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if matches!(
        first,
        '•' | '·'
            | '●'
            | '○'
            | '◦'
            | '▪'
            | '■'
            | '□'
            | '◆'
            | '►'
            | '▶'
            | '‣'
            | '⁃'
            | '✓'
            | '✔'
            | '☐'
            | '☑'
    ) || ('\u{2460}'..='\u{2473}').contains(&first)
    {
        return true;
    }
    if matches!(first, '-' | '*' | '+') {
        return chars.next() == Some(' ');
    }
    // 1. / 1) / 1、 / (1)
    let rest = text
        .strip_prefix('(')
        .or_else(|| text.strip_prefix('（'))
        .unwrap_or(text);
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if (1..=3).contains(&digits) {
        let after: String = rest.chars().skip(digits).take(2).collect();
        let mut it = after.chars();
        return match (it.next(), it.next()) {
            (Some('.' | ')' | '）'), Some(' ')) => true,
            (Some('、' | '）'), _) => true,
            (Some(')'), _) => text.starts_with('(') || text.starts_with('（'),
            _ => false,
        };
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(prev: &str, next: &str) -> String {
        let mut text = prev.to_owned();
        join_line(&mut text, next);
        text
    }

    #[test]
    fn join_line_cases() {
        for (prev, next, want) in [
            // 英文断词还原；不满足条件的连字符原样保留，并按普通拼接加空格
            ("wakes in milli-", "seconds.", "wakes in milliseconds."),
            ("Since COVID-", "19 cases", "Since COVID- 19 cases"),
            ("using Wi-", "Fi", "using Wi- Fi"),
            ("the value is -", "5", "the value is - 5"),
            ("a 2020-", "2021 plan", "a 2020- 2021 plan"),
            // 东亚文字之间直接相连，与拉丁文之间跨行时也不加
            ("几乎不占", "用内存", "几乎不占用内存"),
            ("几乎不占", "CPU，也", "几乎不占CPU，也"),
            ("support for", "中文", "support for中文"),
            ("他说——", "我们", "他说——我们"),
            // 韩文以空格分词
            ("클립보드에", "복사합니다.", "클립보드에 복사합니다."),
            ("Hello", "world", "Hello world"),
            // 已有空白、或任一侧为空时不再加
            ("ends with space ", "next", "ends with space next"),
            ("", "next", "next"),
            ("prev", "", "prev"),
        ] {
            assert_eq!(joined(prev, next), want, "{prev:?} + {next:?}");
        }
    }

    #[test]
    fn needs_space_uses_gap_only_between_unspaced_and_other_scripts() {
        assert!(needs_space("Hello", "world", None));
        assert!(needs_space("Hello", "world", Some(0.0)));
        assert!(!needs_space("你好", "世界", Some(3.0)));
        assert!(!needs_space("版本", "v0.1", None));
        assert!(!needs_space("版本", "v0.1", Some(0.5)));
        assert!(needs_space("版本", "v0.1", Some(0.6)));
        assert!(needs_space("v0.1", "发布", Some(0.8)));
        assert!(!needs_space("v0.1", "发布", Some(0.1)));
        assert!(needs_space("화면의", "글자", None));
        assert!(!needs_space("a ", "b", None));
        assert!(!needs_space("a", "\tb", None));
        assert!(!needs_space("", "b", None));
    }

    #[test]
    fn list_markers() {
        for yes in [
            "• item",
            "- item",
            "1. item",
            "12) item",
            "(3) item",
            "（3）项",
            "3、项",
            "① 项",
        ] {
            assert!(starts_list_item(yes), "{yes}");
        }
        for no in ["-5 degrees", "1.5 GB", "2026 年", "item", "*bold*"] {
            assert!(!starts_list_item(no), "{no}");
        }
    }
}
