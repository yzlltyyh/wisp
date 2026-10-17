//! 整段文字按行处理的操作。

use crate::join::{MixedSpacing, join_break};

/// 识别结果的默认整理（整段版本），只动空白、不丢信息：
///
/// - `\r\n` 与单独的 `\r` 统一成 `\n`；
/// - 去掉每行行尾的空白（含 `\t`、全角空格 U+3000、不换行空格 U+00A0 等一切 Unicode 空白）；
/// - 连续两个以上的空行压成一个，开头与结尾的空行去掉（结果不以换行结尾）；
/// - 行首缩进（包括第一行的缩进）与行内空格原样保留，不拼接行。
///
/// 与流式版本 [`BlankSqueezer`](crate::stream::BlankSqueezer) 的结果逐字相同。幂等；去掉所有空白后与输入相同。
pub fn squeeze_blanks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    // 上一个非空行之后是否出现过空行（开头的空行不算：那时 out 还是空的）
    let mut gap = false;
    for line in line_contents(text).map(str::trim_end) {
        if line.is_empty() {
            gap |= !out.is_empty();
            continue;
        }
        if !out.is_empty() {
            out.push_str(if gap { "\n\n" } else { "\n" });
        }
        gap = false;
        out.push_str(line);
    }
    out
}

/// 按 `\n`、`\r\n`、单独的 `\r` 断开的物理行：`(内容, 连同换行符的整行)`，最后一行可能没有换行符。
/// 末尾的换行符不算再起一个空行（与 [`str::lines`] 相同），所以 `"a\n"` 只有一行；整行依次拼起来就是原文。
///
/// 本模块按行处理的操作都经这里断行，三种换行符处处含义相同（流式版本的规则也一样）；别处按行判断或处理文字时
/// （例如剪贴板变换「是否有多行」「去掉首尾空白」）也用它，「行」的含义才处处一致。不用 [`str::lines`]：
/// 它不把单独的 `\r` 当换行，同一段老式 Mac 换行的文字在「合成一行」里会留下 `\r`，在其他操作里却被断开
pub fn split_lines(text: &str) -> impl Iterator<Item = (&str, &str)> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let (content_end, line_end) = match rest.find(['\n', '\r']) {
            Some(i) if rest[i..].starts_with("\r\n") => (i, i + 2),
            Some(i) => (i, i + 1),
            None => (rest.len(), rest.len()),
        };
        let (line, tail) = rest.split_at(line_end);
        rest = tail;
        Some((&line[..content_end], line))
    })
}

/// 各行的内容（不含换行符），断行同 [`split_lines`]
fn line_contents(text: &str) -> impl Iterator<Item = &str> {
    split_lines(text).map(|(content, _)| content)
}

/// 只含空白（含全角空格、不换行空格）或什么都没有的行
fn is_blank(content: &str) -> bool {
    content.trim().is_empty()
}

/// 文中是否有空行（只含空白的行，开头与结尾的也算）。与 `remove_blank_lines(text) != text` 等价，但不分配、
/// 遇到第一个空行就停：「粘贴为…」检视时每移动一次光标都要判断一次，长文本不应每次都复制一份
pub fn has_blank_lines(text: &str) -> bool {
    line_contents(text).any(is_blank)
}

/// 去掉全部空行（只含空白的行，包括开头与结尾的空行），其余每一行连同它自己的换行符逐字保留：
/// 行首缩进、行尾空白与 `\r\n` 都不动，也不拼接行。用于段落之间不要空行的场合（段落模式的识别结果每段一行，
/// 去掉空行后段与段紧挨着）。
///
/// 幂等；只删整行，不改动留下的任何一行。
pub fn remove_blank_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (_, line) in split_lines(text).filter(|(content, _)| !is_blank(content)) {
        out.push_str(line);
    }
    out
}

/// 把所有非空行合成一行（`\n`、`\r\n` 与单独的 `\r` 都是换行）：每行去掉首尾空白、跳过空行，相邻两行按
/// [`join_break`] 相连；东亚文字与字母数字之间断行处补不补空格，按全文的惯例推断（[`MixedSpacing::infer`]）。
///
/// 幂等；除被还原的断词连字符与软连字符外，不增删任何非空白字符。
pub fn join_all(text: &str) -> String {
    let spacing = MixedSpacing::infer(line_contents(text));
    let mut out = String::with_capacity(text.len());
    for line in line_contents(text).map(str::trim).filter(|l| !l.is_empty()) {
        join_break(&mut out, line, spacing);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn squeeze_blanks_cases() {
        for (input, want) in [
            ("", ""),
            (" \t\u{3000}\n\r\n\u{A0}", ""),
            ("single", "single"),
            // 行尾空白（空格、制表符、全角空格、不换行空格）去掉，行内空格不动
            ("a  b \t\nc\u{3000}\nd\u{A0}", "a  b\nc\nd"),
            // 空行压成一个；只有空白的行也是空行
            ("段一\n\n\n\n段二\n  \t \n\u{3000}\n段三", "段一\n\n段二\n\n段三"),
            // 开头与结尾的空行去掉，第一行的缩进保留
            ("\n \n\n    indented\n  next\n\n\n", "    indented\n  next"),
            ("\u{3000}\u{3000}首行缩进两格", "\u{3000}\u{3000}首行缩进两格"),
            // \r\n 与单独的 \r 都是一个换行
            ("a\r\nb\r\rc\r\n\r\n\r\nd\r", "a\nb\n\nc\n\nd"),
            // 代码：缩进与单个空行原样，多余的空行压掉
            (
                "fn main() {  \n    let x = 1;\n\n\n    x\n}\n",
                "fn main() {\n    let x = 1;\n\n    x\n}",
            ),
            // 制表符分栏：行内的制表符保留，行尾的去掉
            ("名称\t数量\t\n苹果\t3\t\t\n梨\t\t5", "名称\t数量\n苹果\t3\n梨\t\t5"),
            // 不拼接行：断行的英文、列表、Markdown 硬换行（行尾两个空格）都只去掉行尾空白
            (
                "- item one  \n- item two\nwrapped line  \nnext",
                "- item one\n- item two\nwrapped line\nnext",
            ),
        ] {
            assert_eq!(squeeze_blanks(input), want, "{input:?}");
        }
    }

    #[test]
    fn remove_blank_lines_cases() {
        for (input, want) in [
            // 段落模式的识别结果：每段一行，段与段之间一个空行
            ("第一段。\n\n第二段。\n\n第三段。", "第一段。\n第二段。\n第三段。"),
            // 只含空白（空格、制表符、全角空格、不换行空格）的行也是空行
            ("a\n  \n\t\nb\n\u{3000}\n\u{A0}\nc", "a\nb\nc"),
            // 开头与结尾的空行一并去掉；留下的行连同自己的换行符原样保留
            ("\n\n  indented  \nnext\n\n", "  indented  \nnext\n"),
            ("a\n \n", "a\n"),
            ("a\n  ", "a\n"),
            // 换行符原样保留：\r\n 不改成 \n，单独的 \r 也是换行
            ("a\r\n\r\nb\r\n", "a\r\nb\r\n"),
            ("a\r\rb", "a\rb"),
            // 代码：只删空行，缩进与行尾空白不动
            (
                "fn main() {\n    let x = 1;\n\n    x \n}",
                "fn main() {\n    let x = 1;\n    x \n}",
            ),
            // 全是空白时什么都不剩
            ("   ", ""),
            ("\n\n", ""),
        ] {
            assert_eq!(remove_blank_lines(input), want, "{input:?}");
            assert!(has_blank_lines(input), "{input:?} 有空行");
        }
        // 没有空行：末尾的一个换行符不算空行
        for input in ["", "single", "a\nb", "a\nb\n", "a\r\nb\r\n", "  a\n\tb  "] {
            assert!(!has_blank_lines(input), "{input:?} 没有空行");
            assert_eq!(remove_blank_lines(input), input);
        }
        // 默认整理之后的段落：去掉空行后不再有连续换行
        assert_eq!(
            remove_blank_lines(&squeeze_blanks("段一  \n\n\n段二\r\n\r\n段三\n")),
            "段一\n段二\n段三"
        );
    }

    #[test]
    fn join_all_cases() {
        for (input, want) in [
            ("first line\n  second line  \n\nthird", "first line second line third"),
            ("a\r\nb\r\n", "a b"),
            ("   \n\n", ""),
            ("single", "single"),
            // 中文断行直接相连；全文没有中英间隔空格时中英之间也不加
            (
                "这是一段被\n硬换行的中文\nwith English",
                "这是一段被硬换行的中文with English",
            ),
            // 全文习惯在中英之间加空格时补上
            ("支持 OpenAI 和\nAnthropic 等服务", "支持 OpenAI 和 Anthropic 等服务"),
            ("화면의 글자를\n인식합니다", "화면의 글자를 인식합니다"),
            ("他说——\n我们走吧……\n然后", "他说——我们走吧……然后"),
            (
                "COVID-\n19 and Wi-\nFi in 2020-\n2021",
                "COVID-19 and Wi-Fi in 2020-2021",
            ),
            ("an inter-\nface", "an interface"),
            // 以斜杠结尾的行多半是完整的条目：目录列表、网址列表合并后各自分开
            ("src/\ntests/\ndocs/", "src/ tests/ docs/"),
            ("https://a.com/\nhttps://b.com/", "https://a.com/ https://b.com/"),
            ("Visit:\nhttps://a.com/\nNext", "Visit: https://a.com/ Next"),
            // 单独的 \r 与 \n、\r\n 一样是换行
            ("a\rb\nc", "a b c"),
            ("old\rmac\r\r text\r", "old mac text"),
        ] {
            assert_eq!(join_all(input), want, "{input:?}");
        }
    }

    #[test]
    fn long_runs_without_spaces_stay_linear() {
        // 每次拼接只回看上一行末尾的有限几个字符：整段是标点或不含空白时也不会越拼越慢
        for unit in ["....................\n", "abc-\n", "中文……\n"] {
            let text = unit.repeat(20_000);
            let out = join_all(&text);
            assert!(!out.contains('\n') && out.len() < text.len());
        }
    }

    /// 生成文字用的字符：覆盖各条拼接规则涉及的字符类别
    const ALPHABET: &[char] = &[
        'a', 'b', 'Z', 'é', 'ж', '1', '2', '中', '文', 'あ', 'カ', '한', '글', '-', '\u{2010}', '\u{AD}', '—', '…',
        '“', '”', '·', '，', '。', '「', '/', '_', '@', '.', ',', '(', ')', ' ', ' ', '\u{3000}', '\t', '\n', '\n',
        '\r',
    ];

    fn text() -> impl Strategy<Value = String> {
        proptest::collection::vec(proptest::sample::select(ALPHABET), 0..80).prop_map(|v| v.into_iter().collect())
    }

    /// `out` 去掉空白后，是否是 `input` 去掉空白后只删掉若干连字符与软连字符的结果
    fn same_except_dropped_hyphens(input: &str, out: &str) -> bool {
        let mut want = input.chars().filter(|c| !c.is_whitespace());
        for c in out.chars().filter(|c| !c.is_whitespace()) {
            loop {
                match want.next() {
                    Some(w) if w == c => break,
                    Some('-' | '\u{2010}' | '\u{AD}') => {}
                    _ => return false,
                }
            }
        }
        want.all(|w| matches!(w, '-' | '\u{2010}' | '\u{AD}'))
    }

    /// 空白与换行占一半的文字：便于凑出行尾空白、空行、缩进与 \r\n
    fn spacey_text() -> impl Strategy<Value = String> {
        const CHARS: &[char] = &[
            'a', 'Z', '中', '1', '-', '|', ' ', ' ', '\t', '\u{3000}', '\u{A0}', '\u{2028}', '\n', '\n', '\n', '\r',
        ];
        proptest::collection::vec(proptest::sample::select(CHARS), 0..60).prop_map(|v| v.into_iter().collect())
    }

    /// 按 \r\n、\r、\n 断开的物理行
    fn physical_lines(s: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let mut rest = s;
        while let Some(i) = rest.find(['\r', '\n']) {
            out.push(&rest[..i]);
            let skip = if rest[i..].starts_with("\r\n") { 2 } else { 1 };
            rest = &rest[i + skip..];
        }
        out.push(rest);
        out
    }

    proptest! {
        #[test]
        fn squeeze_blanks_is_idempotent(s in spacey_text()) {
            let once = squeeze_blanks(&s);
            prop_assert_eq!(squeeze_blanks(&once), once);
        }

        #[test]
        fn squeeze_blanks_only_touches_whitespace(s in spacey_text()) {
            let out = squeeze_blanks(&s);
            let visible = |t: &str| t.chars().filter(|c| !c.is_whitespace()).collect::<String>();
            prop_assert_eq!(visible(&out), visible(&s));
        }

        /// 非空行逐行对应（只去掉行尾空白，缩进不变、不拼接），空行至多一个、不在首尾
        #[test]
        fn squeeze_blanks_keeps_lines(s in spacey_text()) {
            let out = squeeze_blanks(&s);
            let kept: Vec<&str> = physical_lines(&s).into_iter().map(str::trim_end).filter(|l| !l.is_empty()).collect();
            let got: Vec<&str> = out.split('\n').filter(|l| !l.is_empty()).collect();
            prop_assert_eq!(got, kept);
            prop_assert!(!out.contains('\r') && !out.contains("\n\n\n"));
            prop_assert!(!out.starts_with('\n') && !out.ends_with('\n'));
            prop_assert!(out.split('\n').all(|l| l.trim_end() == l));
        }

        #[test]
        fn remove_blank_lines_is_idempotent(s in spacey_text()) {
            let once = remove_blank_lines(&s);
            prop_assert_eq!(remove_blank_lines(&once), once.clone());
            prop_assert!(!has_blank_lines(&once));
        }

        /// 「是否适用」与「结果是否不同」一致
        #[test]
        fn has_blank_lines_agrees_with_removal(s in spacey_text()) {
            prop_assert_eq!(has_blank_lines(&s), remove_blank_lines(&s) != s);
        }

        /// 留下的行与输入里的非空行逐行对应、连同换行符逐字相同（只删整行，不拼接、不改动行内容）
        #[test]
        fn remove_blank_lines_keeps_other_lines_verbatim(s in spacey_text()) {
            let out = remove_blank_lines(&s);
            let kept: Vec<&str> = split_lines(&s).filter(|(c, _)| !is_blank(c)).map(|(_, l)| l).collect();
            let got: Vec<&str> = split_lines(&out).map(|(_, l)| l).collect();
            prop_assert_eq!(got, kept);
        }

        #[test]
        fn join_all_is_idempotent(s in text()) {
            let once = join_all(&s);
            prop_assert_eq!(join_all(&once), once.clone());
            prop_assert!(!once.contains(['\n', '\r']));
        }

        /// 内部的断行与参照实现（按 \r\n、\r、\n 断开）一致，整行拼回去与原文相同
        #[test]
        fn split_lines_matches_reference(s in spacey_text()) {
            let contents: Vec<&str> = split_lines(&s).map(|(c, _)| c).collect();
            let mut want = physical_lines(&s);
            // 末尾的换行符不再起一个空行（与 str::lines 相同）
            if want.last() == Some(&"") {
                want.pop();
            }
            prop_assert_eq!(contents, want);
            prop_assert_eq!(split_lines(&s).map(|(_, l)| l).collect::<String>(), s);
        }

        /// 三种换行符（\n、\r\n、单独的 \r）在各个操作里含义相同：换成统一的 \n 不改变结果
        #[test]
        fn line_break_styles_are_equivalent(s in spacey_text(), t in text()) {
            let unify = |x: &str| x.replace("\r\n", "\n").replace('\r', "\n");
            prop_assert_eq!(join_all(&t), join_all(&unify(&t)));
            prop_assert_eq!(squeeze_blanks(&s), squeeze_blanks(&unify(&s)));
            prop_assert_eq!(unify(&remove_blank_lines(&s)), remove_blank_lines(&unify(&s)));
            prop_assert_eq!(has_blank_lines(&s), has_blank_lines(&unify(&s)));
        }

        #[test]
        fn join_all_keeps_visible_characters(s in text()) {
            let out = join_all(&s);
            prop_assert!(same_except_dropped_hyphens(&s, &out), "{:?} -> {:?}", s, out);
        }

        /// 一次拼接只在两行之间加一个空格、什么都不加，或删掉上一行末尾的一个连字符
        #[test]
        fn join_break_only_touches_the_seam(a in text(), b in text(), spaced in any::<bool>()) {
            let spacing = if spaced { MixedSpacing::Spaced } else { MixedSpacing::Tight };
            let mut out = a.clone();
            join_break(&mut out, &b, spacing);
            let direct = format!("{a}{b}");
            let space = format!("{a} {b}");
            let dropped = a
                .strip_suffix(['-', '\u{2010}', '\u{AD}'])
                .map(|head| format!("{head}{b}"));
            prop_assert!(
                out == direct || out == space || dropped.as_ref() == Some(&out),
                "{:?} + {:?} -> {:?}", a, b, out
            );
        }
    }
}
