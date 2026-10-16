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
    // \r\n 先合成一个换行，否则按 \r、\n 分别断行时中间会多出一个空行
    let text = text.replace("\r\n", "\n");
    let mut out = String::with_capacity(text.len());
    // 上一个非空行之后是否出现过空行（开头的空行不算：那时 out 还是空的）
    let mut gap = false;
    for line in text.split(['\n', '\r']).map(str::trim_end) {
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

/// 把所有非空行合成一行：每行去掉首尾空白、跳过空行，相邻两行按 [`join_break`] 相连；东亚文字与字母数字之间
/// 断行处补不补空格，按全文的惯例推断（[`MixedSpacing::infer`]）。
///
/// 幂等；除被还原的断词连字符与软连字符外，不增删任何非空白字符。
pub fn join_all(text: &str) -> String {
    let spacing = MixedSpacing::infer(text.lines());
    let mut out = String::with_capacity(text.len());
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
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
        fn join_all_is_idempotent(s in text()) {
            let once = join_all(&s);
            prop_assert_eq!(join_all(&once), once.clone());
            prop_assert!(!once.contains('\n'));
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
