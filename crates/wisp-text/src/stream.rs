//! 流式文字的增量整理：文字分多次到达、放行后不能再改（界面只做追加），只能把还不能确定的部分滞留到能确定为止。

/// 识别结果的默认整理（流式版本），规则与整段版本 [`squeeze_blanks`](crate::lines::squeeze_blanks) 完全相同：
///
/// - `\r\n` 与单独的 `\r` 统一成 `\n`；
/// - 去掉每行行尾的空白（含 `\t`、全角空格 U+3000、不换行空格 U+00A0 等一切 Unicode 空白）；
/// - 连续两个以上的空行压成一个，开头与结尾的空行去掉；
/// - 行首缩进（包括第一行的缩进）与行内空格原样保留，不拼接行。
///
/// 只滞留「上一个非空白字符之后的空白」：它是行尾空白、空行还是下一行的缩进，要等看到下一个非空白字符才知道。
/// 非空白字符一到就连同它前面该保留的空白一起放行，所以放行的文字之后不会再变；任意切分喂入，拼起来的结果都与
/// 整段处理相同（有性质测试）。滞留量只有一行之内的空白（换行只记个数），模型输出大段空行时也不会累积。
#[derive(Debug, Clone, Default)]
pub struct BlankSqueezer {
    /// 已经放行过非空白字符；在此之前的换行都属于开头空行，直接去掉
    started: bool,
    /// 上一个非空白字符之后的换行数（`\r\n` 算一个），到 2 封顶：再多也只保留一个空行
    breaks: u8,
    /// 上一个字符是 `\r`：紧跟的 `\n` 与它合成同一个换行
    after_cr: bool,
    /// 最后一个换行之后的空白（没有换行时是上一个非空白字符之后的空白）。之后来的是换行，它就是行尾空白，丢掉；
    /// 来的是非空白字符，它就是行首缩进或行内空格，原样放行
    run: String,
}

impl BlankSqueezer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一段文字，把可以放行的部分追加到 `out`
    pub fn push(&mut self, delta: &str, out: &mut String) {
        let mut rest = delta;
        while !rest.is_empty() {
            let ws = rest.find(char::is_whitespace).unwrap_or(rest.len());
            if ws > 0 {
                self.release(out);
                out.push_str(&rest[..ws]);
                rest = &rest[ws..];
            }
            let text = rest.find(|c: char| !c.is_whitespace()).unwrap_or(rest.len());
            for c in rest[..text].chars() {
                self.hold(c);
            }
            rest = &rest[text..];
        }
    }

    /// 结束：滞留的只有行尾空白与结尾空行，按规则全部去掉。之后可以重新使用
    pub fn finish(&mut self) {
        *self = Self::default();
    }

    fn hold(&mut self, c: char) {
        match c {
            '\n' if self.after_cr => self.after_cr = false,
            '\n' | '\r' => {
                self.breaks = (self.breaks + 1).min(2);
                self.run.clear();
                self.after_cr = c == '\r';
            }
            _ => {
                self.run.push(c);
                self.after_cr = false;
            }
        }
    }

    /// 下一个非空白字符到了：放行它前面该保留的换行与空白
    fn release(&mut self, out: &mut String) {
        if self.started {
            match self.breaks {
                0 => {}
                1 => out.push('\n'),
                _ => out.push_str("\n\n"),
            }
        }
        out.push_str(&self.run);
        self.run.clear();
        self.breaks = 0;
        self.after_cr = false;
        self.started = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lines::squeeze_blanks;
    use proptest::prelude::*;

    fn squeeze_chunks(chunks: &[&str]) -> String {
        let mut s = BlankSqueezer::new();
        let mut out = String::new();
        for c in chunks {
            s.push(c, &mut out);
        }
        s.finish();
        out
    }

    #[test]
    fn releases_as_soon_as_text_arrives() {
        let mut s = BlankSqueezer::new();
        let mut out = String::new();
        s.push("  标题  \r", &mut out);
        // 第一行的缩进要等到第一个非空白字符才放行（之前它可能是开头空行的一部分）；行尾空白与 \r 滞留
        assert_eq!(out, "  标题");
        s.push("\n\n \t\n", &mut out);
        assert_eq!(out, "  标题");
        s.push("    正文 第二句", &mut out);
        assert_eq!(out, "  标题\n\n    正文 第二句");
        s.push("\u{3000}\u{A0}", &mut out);
        s.finish();
        assert_eq!(out, "  标题\n\n    正文 第二句");
    }

    #[test]
    fn finish_resets_for_reuse() {
        let mut s = BlankSqueezer::new();
        let mut out = String::new();
        s.push("a\n\n", &mut out);
        s.finish();
        s.push("\n b", &mut out);
        assert_eq!(out, "a b", "重新开始后第二段的开头空行同样去掉，缩进保留");
    }

    #[test]
    fn long_whitespace_runs_are_not_accumulated() {
        let mut s = BlankSqueezer::new();
        let mut out = String::new();
        s.push("x", &mut out);
        for _ in 0..10_000 {
            s.push(" \n", &mut out);
        }
        assert!(s.run.is_empty() && s.breaks == 2);
        s.push("y", &mut out);
        assert_eq!(out, "x\n\ny");
    }

    /// 生成文字用的字符：各种空白与换行占一半，便于凑出行尾空白、空行与缩进
    const ALPHABET: &[char] = &[
        'a', 'Z', '中', '文', '1', '-', '|', ' ', ' ', ' ', '\t', '\u{3000}', '\u{A0}', '\u{2028}', '\n', '\n', '\n',
        '\r', '\r',
    ];

    fn text() -> impl Strategy<Value = String> {
        proptest::collection::vec(proptest::sample::select(ALPHABET), 0..60).prop_map(|v| v.into_iter().collect())
    }

    /// 按 `cuts` 给出的字符位置切开
    fn split_at_chars<'a>(s: &'a str, cuts: &[usize]) -> Vec<&'a str> {
        let bounds: Vec<usize> = s.char_indices().map(|(i, _)| i).chain([s.len()]).collect();
        let mut points: Vec<usize> = cuts.iter().map(|&k| bounds[k % bounds.len()]).collect();
        points.sort_unstable();
        points.dedup();
        let mut out = Vec::new();
        let mut last = 0;
        for p in points {
            out.push(&s[last..p]);
            last = p;
        }
        out.push(&s[last..]);
        out
    }

    proptest! {
        #[test]
        fn any_split_matches_whole(s in text(), cuts in proptest::collection::vec(any::<usize>(), 0..12)) {
            prop_assert_eq!(squeeze_chunks(&split_at_chars(&s, &cuts)), squeeze_blanks(&s));
        }

        #[test]
        fn char_by_char_matches_whole(s in text()) {
            let chars: Vec<String> = s.chars().map(String::from).collect();
            let chunks: Vec<&str> = chars.iter().map(String::as_str).collect();
            prop_assert_eq!(squeeze_chunks(&chunks), squeeze_blanks(&s));
        }
    }
}
