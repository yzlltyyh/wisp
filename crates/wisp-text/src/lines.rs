//! 整段文字按行处理的操作。

use crate::join::{MixedSpacing, join_break};

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

    proptest! {
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
