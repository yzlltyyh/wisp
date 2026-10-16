//! 两段文字的拼接：同一视觉行内相邻的片段、跨行续接的文字，以及列表项的判定（列表项总是另起一段）。

use crate::script::{is_east_asian, is_unspaced};

/// 软连字符：排版软件在可断词处预埋、只在断行时显示的连字符
const SOFT_HYPHEN: char = '\u{AD}';
/// 拼接时往上一行末尾回看的最多字符数。合成一行时上一行就是已拼好的全部结果（可达 1 MB），不设上限的话，
/// 一长串标点或不含空白的长行会让每次拼接都从头扫一遍
const LOOKBACK: usize = 64;

/// 东亚文字与字母数字相邻处的书写惯例：中间隔一个空格（「支持 OpenAI 和」），还是直接相连（「支持OpenAI和」）。
/// 跨行续接时，断在这两类文字之间的地方按全文的惯例决定补不补空格。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MixedSpacing {
    /// 直接相连。没有样本、或两种写法一样多时也取这个：多补的空格比少一个空格更显眼
    #[default]
    Tight,
    /// 隔一个空格
    Spaced,
}

impl MixedSpacing {
    /// 从文字推断惯例：在每段文字内部统计「东亚文字的字」（汉字、假名等，不含标点）与「字母或数字」（拉丁、西里尔、
    /// 谚文等）相邻的地方，隔一个半角空格的多于直接相邻的，就是 [`MixedSpacing::Spaced`]。
    ///
    /// 只统计每段之内：段与段（行与行）的边界正是要决定的地方。跨行处新出现的中英边界都按推断结果补或不补空格，
    /// 只会加强已有的惯例，所以对拼接结果再推断一次，得到的还是同一个惯例。
    pub fn infer<'a>(texts: impl IntoIterator<Item = &'a str>) -> Self {
        let (mut spaced, mut tight) = (0usize, 0usize);
        for text in texts {
            let (mut before, mut prev) = (None, None);
            for c in text.chars() {
                if let Some(p) = prev {
                    if is_mixed_pair(p, c) {
                        tight += 1;
                    } else if p == ' '
                        && let Some(b) = before
                        && is_mixed_pair(b, c)
                    {
                        spaced += 1;
                    }
                }
                (before, prev) = (prev, Some(c));
            }
        }
        if spaced > tight { Self::Spaced } else { Self::Tight }
    }
}

/// 把下一行接到 `text` 末尾（跨行续接）。按顺序判断，命中第一条即停：
///
/// 1. `text` 以软连字符结尾：删掉它，直接相连；
/// 2. 以 `-` 或 U+2010 结尾、且前一个字符是字母（不含中日韩文字）：像断词就删掉连字符直接相连（`inter-` 接 `face`
///    得 `interface`），否则保留连字符直接相连（`COVID-19`、`Wi-Fi`、`state-of-the-art`、URL 与路径）。
///    像断词指下一行以小写字母开头，且末尾的词看起来是普通单词：不像链接、路径、标识符或邮箱，词里没有别的连字符，
///    至少两个字母、不全是大写、不含数字。恰好断在复合词的连字符处时（`well-` 接 `known`）会被当成断词；
/// 3. 任一侧已是空白：直接相连；
/// 4. 以紧跟在文字后的连字符或破折号结尾：直接相连（`2020-2021`、`他说——我`）；这些符号前是空白时（`3 -` 接 `7`）
///    补一个空格，与原文的写法对称。行尾的斜杠与下划线不在此列，按下面的规则处理（与西文之间补空格：`src/` 接
///    `tests/` 得 `src/ tests/`）；
/// 5. 任一侧是东亚标点：`，`、`。`、`「` 这类，以及紧挨着东亚文字的中性标点（`他说——`、`等等……`、`然后,`、
///    `“出发”`）。直接相连；
/// 6. 两侧都是不用空格分词的东亚文字：直接相连；谚文按拉丁文处理，不在此列；
/// 7. 一侧是东亚文字、另一侧是字母或数字（中间可以隔着西文语境里的中性标点，如 `100%` 接 `的`、`met.` 接 `我们`）：
///    按 `spacing`（全文惯例，见 [`MixedSpacing::infer`]）；
/// 8. 其余情况：加一个空格。
///
/// 中性标点指中西文共用的通用标点（破折号、弯引号、省略号、间隔号）与 ASCII 标点符号：它们属于哪种书写习惯，
/// 要看越过它们之后紧挨着的文字。
///
/// 除被删掉的软连字符与断词连字符外，不增删任何非空白字符。`next` 为空时不变。
pub fn join_break(text: &mut String, next: &str, spacing: MixedSpacing) {
    match joint(text, next, spacing) {
        Joint::Direct => {}
        Joint::Space => text.push(' '),
        Joint::DropMark => {
            text.pop();
        }
    }
    text.push_str(next);
}

/// 两行之间怎么接
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Joint {
    /// 直接相连
    Direct,
    /// 加一个空格
    Space,
    /// 删掉上一行末尾的软连字符或断词连字符后直接相连
    DropMark,
}

fn joint(prev: &str, next: &str, spacing: MixedSpacing) -> Joint {
    let (Some(x), Some(y)) = (prev.chars().next_back(), next.chars().next()) else {
        return Joint::Direct;
    };
    let head = &prev[..prev.len() - x.len_utf8()];
    let w = head.chars().next_back();
    if x == SOFT_HYPHEN {
        return Joint::DropMark;
    }
    if matches!(x, '-' | '\u{2010}') && w.is_some_and(|w| w.is_alphabetic() && !is_east_asian(w)) {
        return if hyphen_breaks_word(head, next) {
            Joint::DropMark
        } else {
            Joint::Direct
        };
    }
    if x.is_whitespace() || y.is_whitespace() {
        return Joint::Direct;
    }
    if is_connector(x) {
        return if w.is_none_or(char::is_whitespace) {
            Joint::Space
        } else {
            Joint::Direct
        };
    }
    // 越过两侧的中性标点，找到决定书写习惯的字符（空白越不过去）
    let a = prev.chars().rev().take(LOOKBACK).find(|&c| !is_neutral(c));
    let b = next.chars().find(|&c| !is_neutral(c));
    let east_asian_edge = |edge: char, near: Option<char>| {
        is_east_asian_mark(edge) || (is_neutral(edge) && near.is_some_and(is_unspaced))
    };
    if east_asian_edge(x, a) || east_asian_edge(y, b) {
        return Joint::Direct;
    }
    match (a, b) {
        (Some(a), Some(b)) if is_unspaced(a) && is_unspaced(b) => Joint::Direct,
        (Some(a), Some(b)) if is_mixed_pair(a, b) => match spacing {
            MixedSpacing::Tight => Joint::Direct,
            MixedSpacing::Spaced => Joint::Space,
        },
        _ => Joint::Space,
    }
}

/// 行尾的连字符（`head` 是它之前的文字，末尾是字母）是不是断词：下一行以小写字母开头，且末尾的词看起来是普通单词。
/// 这些情况保留连字符：
///
/// - 末尾的词或下一行的首个词像链接、路径、标识符或邮箱（含 `/`、`_`、`@`、`www.`）；
/// - 末尾的词里已经有别的连字符（`state-of-the-`、`my-package-`）；
/// - 连字符前只有一个字母（`e-mail`、`X-ray`：断词至少在两个字母之后）、全是大写（`API-based`）或含数字（`H2O-free`）。
fn hyphen_breaks_word(head: &str, next: &str) -> bool {
    let tail = head
        .char_indices()
        .nth_back(LOOKBACK - 1)
        .map_or(head, |(i, _)| &head[i..]);
    let token = tail.rsplit(char::is_whitespace).next().unwrap_or(tail);
    let next_token = next.split(char::is_whitespace).next().unwrap_or(next);
    let address = |t: &str| t.contains(['/', '_', '@']) || t.contains("www.");
    if address(token) || address(next_token) || token.contains(['-', '\u{2010}']) {
        return false;
    }
    let word = token.trim_start_matches(|c: char| !c.is_alphanumeric());
    let letters = word.chars().filter(|c| c.is_alphabetic()).count();
    letters >= 2
        && word.chars().any(char::is_lowercase)
        && !word.chars().any(char::is_numeric)
        && next.chars().next().is_some_and(char::is_lowercase)
}

/// 紧跟在文字后时把两侧直接连起来的符号：连字符与各种破折号。
///
/// 斜杠与下划线刻意不算：以斜杠结尾的行常常本身就是一个完整的条目（`ls -F` 列出的目录、末尾带斜杠的链接），
/// 直接相连会把两个条目粘成一个错误的路径或链接，还很难看出来；断在斜杠后的词（`and/` 接 `or`）多出一个空格，
/// 却一眼可见、删掉即可。「两侧都像普通单词才相连」也挡不住 `src/` 接 `README` 这样的目录列表，所以不做这种猜测。
/// 下划线在断行规则里与字母同类，本来就不是排版断行的位置，以它结尾的行几乎都是完整的标识符。
fn is_connector(c: char) -> bool {
    matches!(c, '-' | '\u{2010}'..='\u{2015}' | '\u{2E3A}' | '\u{2E3B}')
}

/// 中性标点：中西文共用的通用标点（破折号、弯引号、省略号、项目符号、间隔号）与 ASCII 标点符号
fn is_neutral(c: char) -> bool {
    c.is_ascii_punctuation() || matches!(c, '\u{2010}'..='\u{2027}' | '\u{00B7}' | '\u{2E3A}' | '\u{2E3B}')
}

/// 东亚标点：不用空格分词的东亚字符里不是字或数字的那些（`，`、`。`、`「`、`・`、`㎏` 等）
fn is_east_asian_mark(c: char) -> bool {
    is_unspaced(c) && !c.is_alphanumeric()
}

/// 一侧是东亚文字的字（汉字、假名等，不含标点）、另一侧是其他书写系统的字母或数字
fn is_mixed_pair(a: char, b: char) -> bool {
    let unspaced_letter = |c: char| is_unspaced(c) && c.is_alphanumeric();
    let spaced_letter = |c: char| c.is_alphanumeric() && !is_unspaced(c);
    (unspaced_letter(a) && spaced_letter(b)) || (spaced_letter(a) && unspaced_letter(b))
}

/// `a` 与 `b` 前后拼接时中间是否要加空格（同一视觉行内相邻的片段）。
///
/// `gap` 是两者在版面上的水平间距与行高之比，由调用方从文本框算出；没有版面信息时为 `None`。
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

/// 这一行是否以列表项开头：
///
/// - 项目符号（`•`、`●`、`■`、`►`、`✓`、`☐` 等），后跟空格的 `-` / `*` / `+`；
/// - 带圈、带括号、带句点的编号（`①`、`⑴`、`⒈`、`❶`、`➊`、`㈠`、`㊀`、`⒜`、`ⓐ`）；
/// - 最多三位的数字编号：`1.`、`1)`、`1）` 后跟空格，`1、`、`1）`、`(1)`、`（1）`；多级编号 `1.2.3 `、`1.2. `；
/// - 单个字母的编号：`a.`、`a)`、`A）` 后跟空格，`(a)`、`（a）`；
/// - 中文数字编号：`一、`、`十二、`、`(一)`、`（一）`。
///
/// 「1.5 GB」「2026 年」「-5 degrees」「*bold*」「e.g. this」「a lot」这类不算。
pub fn starts_list_item(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if is_bullet(first) || is_enclosed_number(first) {
        return true;
    }
    if matches!(first, '-' | '*' | '+') {
        return chars.next() == Some(' ');
    }
    starts_with_numbering(text)
}

fn is_bullet(c: char) -> bool {
    matches!(
        c,
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
    )
}

/// 自带编号的单个字符
fn is_enclosed_number(c: char) -> bool {
    matches!(
        c as u32,
        0x2460..=0x24FF      // 带圈、带括号、带句点的数字与字母：①–⑳、⑴–⒇、⒈–⒛、⒜–⒵、Ⓐ–ⓩ、⓫–⓿
            | 0x2776..=0x2793 // 黑底与无衬线的带圈数字：❶–❿、➀–➉、➊–➓
            | 0x3220..=0x3229 // 带括号的汉字数字：㈠–㈩
            | 0x3251..=0x325F // 带圈数字 21–35
            | 0x3280..=0x3289 // 带圈汉字数字：㊀–㊉
            | 0x32B1..=0x32BF // 带圈数字 36–50
    )
}

/// 中文数字编号里用到的字
fn is_han_numeral(c: char) -> bool {
    matches!(
        c,
        '〇' | '零' | '一' | '二' | '三' | '四' | '五' | '六' | '七' | '八' | '九' | '十' | '百'
    )
}

/// 编号的主体
#[derive(Clone, Copy)]
enum Label {
    Digits,
    Letter,
    Han,
}

/// 数字、单个字母或中文数字的编号（见 [`starts_list_item`]）
fn starts_with_numbering(text: &str) -> bool {
    let (paren, rest) = match text.strip_prefix(['(', '（']) {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (label, count) = match rest.chars().next() {
        Some(c) if c.is_ascii_digit() => (Label::Digits, rest.chars().take_while(char::is_ascii_digit).count()),
        // 只认单个字母：后面紧跟的若还是字母，就是普通单词，下面的分隔符检查会把它排除
        Some(c) if c.is_ascii_alphabetic() => (Label::Letter, 1),
        Some(c) if is_han_numeral(c) => (Label::Han, rest.chars().take_while(|&c| is_han_numeral(c)).count()),
        _ => return false,
    };
    if count > 3 {
        return false;
    }
    let mut after = rest.chars().skip(count);
    let (a, b) = (after.next(), after.next());
    if paren {
        return matches!(a, Some(')' | '）'));
    }
    match label {
        Label::Digits => match (a, b) {
            (Some('.'), Some(d)) if d.is_ascii_digit() => is_multilevel(rest),
            (Some('.' | ')' | '）'), Some(' ')) | (Some('、' | '）'), _) => true,
            _ => false,
        },
        Label::Letter => matches!((a, b), (Some('.' | ')' | '）'), Some(' '))),
        Label::Han => a == Some('、'),
    }
}

/// 多级编号：至少三级（`1.2.3 `），或两级后再跟一个点（`1.2. `），每级最多三位，后跟空格。
/// 两级且没有结尾点的（`1.5 GB`、`3.14 is`）是小数，不算
fn is_multilevel(text: &str) -> bool {
    let mut levels = 0;
    let mut rest = text;
    loop {
        let n = rest.bytes().take_while(u8::is_ascii_digit).count();
        if !(1..=3).contains(&n) {
            return false;
        }
        levels += 1;
        rest = &rest[n..];
        match rest.strip_prefix('.') {
            Some(r) if r.starts_with(|c: char| c.is_ascii_digit()) => rest = r,
            Some(r) => return levels >= 2 && r.starts_with(' '),
            None => return levels >= 3 && rest.starts_with(' '),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(prev: &str, next: &str, spacing: MixedSpacing) -> String {
        let mut text = prev.to_owned();
        join_break(&mut text, next, spacing);
        text
    }

    #[test]
    fn join_break_cases() {
        use MixedSpacing::{Spaced, Tight};
        for (prev, next, spacing, want) in [
            // 软连字符总是删掉
            ("hyphen\u{AD}", "ation", Tight, "hyphenation"),
            ("Silben\u{AD}", "Trennung", Tight, "SilbenTrennung"),
            // 断词还原：连字符前是字母、下一行小写开头（含拉丁字母以外的字母）
            ("wakes in milli-", "seconds.", Tight, "wakes in milliseconds."),
            ("shared inter\u{2010}", "face", Tight, "shared interface"),
            ("Grü-", "ße", Tight, "Grüße"),
            ("при-", "мер", Tight, "пример"),
            ("(inter-", "face)", Tight, "(interface)"),
            // 已知会错：复合词恰好断在连字符处
            ("a well-", "known VPN", Tight, "a wellknown VPN"),
            // 下一行不是小写开头：保留连字符直接相连
            ("Since COVID-", "19 cases", Tight, "Since COVID-19 cases"),
            ("using Wi-", "Fi", Tight, "using Wi-Fi"),
            ("Jean-", "Paul", Tight, "Jean-Paul"),
            // 词里已有连字符、像链接路径标识符、单个字母、全大写、含数字：保留
            ("state-of-the-", "art", Tight, "state-of-the-art"),
            (
                "see https://example.com/long-",
                "path",
                Tight,
                "see https://example.com/long-path",
            ),
            ("run my_tool-", "name", Tight, "run my_tool-name"),
            ("mail me@long-", "domain.com", Tight, "mail me@long-domain.com"),
            ("go to www.long-", "name.com", Tight, "go to www.long-name.com"),
            ("open long-", "path/to/file", Tight, "open long-path/to/file"),
            ("send an e-", "mail", Tight, "send an e-mail"),
            ("an X-", "ray", Tight, "an X-ray"),
            ("an API-", "based", Tight, "an API-based"),
            ("H2O-", "free", Tight, "H2O-free"),
            // 连字符、破折号紧跟文字：直接相连；前面是空白：补空格
            ("a 2020-", "2021 plan", Tight, "a 2020-2021 plan"),
            ("pages 10–", "20", Tight, "pages 10–20"),
            ("it was—", "as expected—fine", Tight, "it was—as expected—fine"),
            ("中文-", "English", Tight, "中文-English"),
            ("the value is -", "5", Tight, "the value is - 5"),
            ("range 3 –", "7", Tight, "range 3 – 7"),
            ("-", "item", Tight, "- item"),
            // 东亚标点与紧挨东亚文字的通用标点：直接相连
            ("他说——", "我们", Tight, "他说——我们"),
            ("他说——", "OK", Spaced, "他说——OK"),
            ("等等……", "然后", Tight, "等等……然后"),
            ("迈克尔·", "杰克逊", Tight, "迈克尔·杰克逊"),
            ("他说：“", "走吧。”", Tight, "他说：“走吧。”"),
            ("我们", "“出发”", Spaced, "我们“出发”"),
            ("你好，", "world", Spaced, "你好，world"),
            ("Wisp", "（测试版）", Spaced, "Wisp（测试版）"),
            ("「引号」", "Hello", Spaced, "「引号」Hello"),
            // 紧挨东亚文字的 ASCII 标点同样直接相连（用半角标点写的中文）
            ("然后,", "我们", Spaced, "然后,我们"),
            ("(注意)", "这里", Spaced, "(注意)这里"),
            ("我们", "\"走吧\"", Spaced, "我们\"走吧\""),
            // 西文语境里的中性标点：与西文之间加空格，与东亚文字之间按惯例
            ("Wait…", "then", Tight, "Wait… then"),
            ("Wait…", "然后", Tight, "Wait…然后"),
            ("Wait…", "然后", Spaced, "Wait… 然后"),
            ("占用100%", "的时候", Tight, "占用100%的时候"),
            ("占用 100%", "的时候", Spaced, "占用 100% 的时候"),
            ("met.", "我们", Spaced, "met. 我们"),
            ("Hello,", "世界", Tight, "Hello,世界"),
            ("你好", "(hi)", Tight, "你好(hi)"),
            ("你好", "(hi)", Spaced, "你好 (hi)"),
            ("我们", "- item", Tight, "我们 - item"),
            // 东亚文字之间直接相连（含扩展 B 与兼容字符）
            ("几乎不占", "用内存", Tight, "几乎不占用内存"),
            ("𠮷野", "家", Tight, "𠮷野家"),
            ("重さ5", "㎏", Spaced, "重さ5㎏"),
            ("クリップ", "ボード", Tight, "クリップボード"),
            // 东亚文字与字母数字之间按惯例
            ("几乎不占", "CPU，也", Tight, "几乎不占CPU，也"),
            ("几乎不占", "CPU，也", Spaced, "几乎不占 CPU，也"),
            ("support for", "中文", Tight, "support for中文"),
            ("support for", "中文", Spaced, "support for 中文"),
            ("第", "3 章", Tight, "第3 章"),
            ("韓國", "사람", Spaced, "韓國 사람"),
            // 韩文以空格分词，与拉丁文一样
            ("클립보드에", "복사합니다.", Tight, "클립보드에 복사합니다."),
            ("Wisp는", "백그라운드에서", Tight, "Wisp는 백그라운드에서"),
            ("Hello", "world", Tight, "Hello world"),
            ("Hello.", "World", Tight, "Hello. World"),
            // 已有空白、或任一侧为空时不再加
            ("ends with space ", "next", Tight, "ends with space next"),
            ("prev", " next", Tight, "prev next"),
            ("", "next", Tight, "next"),
            ("prev", "", Tight, "prev"),
            ("hyphen\u{AD}", "", Tight, "hyphen\u{AD}"),
        ] {
            assert_eq!(joined(prev, next, spacing), want, "{prev:?} + {next:?} ({spacing:?})");
        }
    }

    /// 行尾的斜杠与下划线不是连接符：以它们结尾的行常常是一个完整的条目（目录、末尾带斜杠的链接），与下一行之间
    /// 照常补空格，不把两个条目粘成一个
    #[test]
    fn slash_and_underscore_do_not_glue_lines() {
        use MixedSpacing::{Spaced, Tight};
        for (prev, next, spacing, want) in [
            // 目录列表（`ls -F`）、网址列表
            ("src/", "tests/", Tight, "src/ tests/"),
            ("src/", "README", Tight, "src/ README"),
            (
                "https://a.com/",
                "https://b.com/",
                Tight,
                "https://a.com/ https://b.com/",
            ),
            ("Visit: https://a.com/", "Next", Tight, "Visit: https://a.com/ Next"),
            ("see /usr/local/", "bin", Tight, "see /usr/local/ bin"),
            // 断在斜杠或下划线后的词同样补空格：多一个空格一眼可见，粘错的路径与链接却很难发现
            ("and/", "or", Tight, "and/ or"),
            ("input/", "output", Spaced, "input/ output"),
            ("snake_", "case", Tight, "snake_ case"),
            ("__init__", "self", Tight, "__init__ self"),
            // 前面是空白时与原来一样补空格；紧挨东亚文字时按东亚标点直接相连
            ("a /", "b", Tight, "a / b"),
            ("目录/", "文件", Tight, "目录/文件"),
            ("目录/", "src", Spaced, "目录/src"),
        ] {
            assert_eq!(joined(prev, next, spacing), want, "{prev:?} + {next:?} ({spacing:?})");
        }
    }

    #[test]
    fn mixed_spacing_follows_the_majority() {
        use MixedSpacing::{Spaced, Tight};
        for (texts, want) in [
            (vec![], Tight),
            (vec!["纯中文，没有字母", "plain English only"], Tight),
            (vec!["支持 OpenAI 和", "Anthropic 等服务"], Spaced),
            (vec!["支持OpenAI和", "Anthropic等服务"], Tight),
            // 一样多时直接相连
            (vec!["使用 Wisp时"], Tight),
            (vec!["使用 Wisp 时", "版本v1"], Spaced),
            // 只认一个半角空格；标点、全角空格、跨段的边界不算
            (vec!["使用  Wisp　时，A"], Tight),
            (vec!["中文", "English"], Tight),
            // 谚文算字母一侧；数字也算
            (vec!["韓國 사람", "第 3 章"], Spaced),
        ] {
            assert_eq!(MixedSpacing::infer(texts.iter().copied()), want, "{texts:?}");
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
            "3） 项",
            "(3) item",
            "（3）项",
            "3、项",
            "① 项",
            "⑴ 项",
            "⒈ 项",
            "❶ 项",
            "➊ item",
            "㈠ 项",
            "㉑ 项",
            "⒜ item",
            "a. item",
            "B) item",
            "c） 项",
            "(a) item",
            "（b）项",
            "I. Introduction",
            "一、项",
            "十二、项",
            "(一) 项",
            "（三）项",
            "1.2.3 Heading",
            "1.2. Heading",
            "10.2.11 Heading",
        ] {
            assert!(starts_list_item(yes), "{yes}");
        }
        for no in [
            "-5 degrees",
            "1.5 GB",
            "3.14 is pi",
            "2026 年",
            "2026.10.17 release",
            "1.2.3",
            "1.2.3a",
            "1234. item",
            "item",
            "*bold*",
            "a lot of",
            "a.m. tomorrow",
            "e.g. this",
            "I am",
            "Ab. item",
            "(ab) item",
            "一个人",
            "十分、",
            "一二三四、",
            "(see below)",
            "",
        ] {
            assert!(!starts_list_item(no), "{no}");
        }
    }
}
