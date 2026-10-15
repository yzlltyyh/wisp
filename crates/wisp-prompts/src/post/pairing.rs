//! 成对剥离：开头的引号 / 代码围栏剥掉之后，在正文里找到与之配对的闭合符号并一并去掉。
//!
//! 前端只做追加，开头的包裹符号一旦剥掉就收不回来，所以配对的闭合符号不论出现在末尾还是中段都要去掉，
//! 否则会留下孤立的闭合引号或 ```` ``` ````（MarkdownLite 会把它之后的全部文字当成代码块）。编辑动作的
//! 「正文 + `\n---\n改动：…`」也因此自然成立：包住正文的闭合符号位于分隔线之前，同样会被找到并去掉。
//!
//! - **围栏**：第一个不在内层代码块里、独占一行的 ```` ``` ````（缩进 ≤3，反引号数 ≥ 开头围栏，行尾只允许空白、
//!   闭合标签或待配对的闭合引号），连同行尾换行一起去掉；反引号数 ≥ 开头围栏且带语言标注的行（```` ```js ````）
//!   开启内层代码块，下一个独占一行的围栏关闭它。
//! - **弯引号**：按 “ ” 计算嵌套深度，使深度回到 0 的那个 ” 就是配对的闭合引号。
//! - **直引号**：按前后字符判断开 / 闭——后面是空白、标点或流结束 → 闭；前面是空白、开括号类标点且后面紧跟文字 → 开；
//!   前后都紧挨文字（中文里常见）时，深度大于 0 视为关闭内层，深度为 0 则向后看到行尾（最多 40 个字符）：
//!   还有 `"` 就当作内层开引号，否则就是配对的闭合引号。40 个字符覆盖常见的内层引语（`他说"……"然后`）；
//!   更长的内层引语仍会被当成闭合引号，这是「只追加、不回改」与有界滞留之间的取舍。
//!
//! 每个判断只依赖当前位置之后有界长度的文本（≤40 字节的围栏行、≤40 个字符的引号前瞻），
//! 所以「已判定」的位置只取决于文本内容，与切分方式无关；未判定的部分由调用方滞留，长度同样有界。

use std::ops::Range;

use super::{CLOSE_TAGS, Match, match_literals};

/// 候选围栏行（不含换行）的最大字节数；超过就当作普通文本
pub(super) const FENCE_LINE_MAX: usize = 40;
/// 前后都紧挨文字的直引号向后看的字符数（到行尾为止）
const QUOTE_LOOKAHEAD: usize = 40;

/// 配对跟踪器（只在剥掉了开头的引号或围栏时活跃）
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Pairing {
    fence: Option<FenceTrack>,
    quote: Option<QuoteTrack>,
    /// 是否剥离闭合标签（围栏行尾允许出现闭合标签）
    tags: bool,
    /// 下一个字符是否位于行首
    at_line_start: bool,
    /// 上一个原始字符（直引号开 / 闭判断用；正文开头为 None）
    prev: Option<char>,
}

#[derive(Debug, Clone, Copy)]
struct FenceTrack {
    /// 开头围栏的反引号数
    ticks: usize,
    /// 是否位于内层代码块中
    inner: bool,
}

#[derive(Debug, Clone, Copy)]
struct QuoteTrack {
    open: char,
    close: char,
    /// 内层引号的嵌套深度
    depth: u32,
}

impl Pairing {
    /// `fence`：开头围栏的反引号数；`quote`：开头引号对应的闭合引号；`at_line_start`：正文是否从行首开始
    pub(super) fn new(fence: Option<usize>, quote: Option<char>, tags: bool, at_line_start: bool) -> Self {
        Self {
            fence: fence.map(|ticks| FenceTrack { ticks, inner: false }),
            quote: quote.map(|close| QuoteTrack {
                open: if close == '\u{201D}' { '\u{201C}' } else { close },
                close,
                depth: 0,
            }),
            tags,
            at_line_start,
            prev: None,
        }
    }

    /// 是否还有待配对的闭合符号
    pub(super) fn is_active(&self) -> bool {
        self.fence.is_some() || self.quote.is_some()
    }

    /// 从头扫描 `text`（上次停下位置之后的全部未判定文本），返回已判定的字节数；
    /// 需要去掉的范围（相对 `text`、按顺序、都在已判定部分之内）通过 `drop` 报告。
    /// `done` 为 true 时流已结束，总是全部判定。
    pub(super) fn scan(&mut self, text: &str, done: bool, drop: &mut dyn FnMut(Range<usize>)) -> usize {
        let mut pos = 0usize;
        while pos < text.len() {
            if !self.is_active() {
                return text.len();
            }
            let rest = &text[pos..];
            if self.at_line_start
                && let Some(fence) = self.fence.as_mut()
            {
                match fence_line(rest, self.quote.map(|q| q.close), self.tags, done) {
                    Line::NeedMore => return pos,
                    Line::Bare { ticks, core, end, tail } if ticks >= fence.ticks => {
                        if fence.inner {
                            // 内层代码块的闭合行：原样保留，行尾的内容照常处理
                            fence.inner = false;
                            self.skip(&rest[..core]);
                            pos += core;
                        } else if tail {
                            // 行尾还有闭合标签 / 闭合引号：只去掉反引号部分，其余照常处理
                            self.fence = None;
                            drop(pos..pos + core);
                            self.skip(&rest[..core]);
                            pos += core;
                        } else {
                            self.fence = None;
                            drop(pos..pos + end);
                            self.skip(&rest[..end]);
                            pos += end;
                        }
                        continue;
                    }
                    Line::Info { ticks, core } => {
                        if ticks >= fence.ticks {
                            fence.inner = true;
                        }
                        self.skip(&rest[..core]);
                        pos += core;
                        continue;
                    }
                    // 反引号不够的围栏行与普通行都是正文
                    Line::Bare { .. } | Line::Other => {}
                }
            }
            let Some(c) = rest.chars().next() else { break };
            let len = c.len_utf8();
            if let Some(q) = self.quote.as_mut() {
                if c == q.open && q.open != q.close {
                    q.depth += 1;
                } else if c == q.close {
                    let role = if q.open == q.close {
                        match straight_quote_role(self.prev, &rest[len..], q.depth, done) {
                            Some(role) => role,
                            None => return pos,
                        }
                    } else {
                        Role::Close
                    };
                    match role {
                        Role::Open => q.depth += 1,
                        Role::Close if q.depth > 0 => q.depth -= 1,
                        Role::Close => {
                            self.quote = None;
                            drop(pos..pos + len);
                        }
                    }
                }
            }
            self.prev = Some(c);
            self.at_line_start = c == '\n';
            pos += len;
        }
        pos
    }

    /// 越过一段不含引号的文本，只更新行首与前一字符
    fn skip(&mut self, s: &str) {
        if let Some(c) = s.chars().next_back() {
            self.prev = Some(c);
            self.at_line_start = c == '\n';
        }
    }
}

/// 直引号的角色
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Open,
    Close,
}

/// 直引号是开还是闭；需要更多文本时返回 None
fn straight_quote_role(prev: Option<char>, after: &str, depth: u32, done: bool) -> Option<Role> {
    let Some(next) = after.chars().next() else {
        return done.then_some(Role::Close);
    };
    if next.is_whitespace() {
        return Some(Role::Close);
    }
    if prev.is_none_or(|p| p.is_whitespace() || is_open_punct(p)) {
        return Some(Role::Open);
    }
    if is_close_punct(next) || depth > 0 {
        return Some(Role::Close);
    }
    // 前后都紧挨文字且没有未闭合的内层引号：同一行稍后（≤ 40 个字符）还有直引号 → 内层开引号，否则 → 配对的闭合引号
    let mut seen = 0usize;
    for c in after.chars() {
        match c {
            '"' => return Some(Role::Open),
            '\n' => return Some(Role::Close),
            _ => {}
        }
        seen += 1;
        if seen == QUOTE_LOOKAHEAD {
            return Some(Role::Close);
        }
    }
    done.then_some(Role::Close)
}

/// 直引号前面出现这些字符时倾向于开引号
fn is_open_punct(c: char) -> bool {
    matches!(
        c,
        '(' | '['
            | '{'
            | '<'
            | '='
            | ':'
            | ','
            | '/'
            | '-'
            | '*'
            | '_'
            | '~'
            | '`'
            | '—'
            | '–'
            | '“'
            | '‘'
            | '「'
            | '『'
            | '《'
            | '〈'
            | '（'
            | '【'
            | '«'
            | '‹'
            | '：'
            | '，'
            | '、'
    )
}

/// 直引号后面出现这些字符时是闭引号
fn is_close_punct(c: char) -> bool {
    matches!(
        c,
        '.' | ','
            | ';'
            | ':'
            | '!'
            | '?'
            | ')'
            | ']'
            | '}'
            | '\''
            | '*'
            | '_'
            | '~'
            | '`'
            | '-'
            | '…'
            | '—'
            | '–'
            | '’'
            | '，'
            | '。'
            | '！'
            | '？'
            | '；'
            | '：'
            | '、'
            | '）'
            | '】'
            | '」'
            | '』'
            | '》'
            | '〉'
            | '»'
            | '›'
    )
}

/// 一行开头的围栏判断
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Line {
    /// 还不能确定（行没写完）
    NeedMore,
    /// 不是围栏行
    Other,
    /// 独占一行的围栏：`core` 为缩进 + 反引号 + 行尾空白的字节数；`end` 为到行尾（含换行）的字节数；
    /// `tail` 为 true 时反引号之后同一行还有闭合标签 / 闭合引号
    Bare {
        ticks: usize,
        core: usize,
        end: usize,
        tail: bool,
    },
    /// 带语言标注（或其他内容）的围栏行：`core` 为缩进 + 反引号 + 空白的字节数
    Info { ticks: usize, core: usize },
}

/// 判断以 `s` 开头的这一行是不是围栏行（`s` 位于行首）
pub(super) fn fence_line(s: &str, quote_close: Option<char>, tags: bool, done: bool) -> Line {
    let b = s.as_bytes();
    let indent = b.iter().take_while(|&&c| c == b' ').count();
    if indent == b.len() {
        return if indent <= 3 && !done {
            Line::NeedMore
        } else {
            Line::Other
        };
    }
    if indent > 3 || b[indent] != b'`' {
        return Line::Other;
    }
    let ticks = b[indent..].iter().take_while(|&&c| c == b'`').count();
    let after_ticks = indent + ticks;
    if after_ticks > FENCE_LINE_MAX {
        return Line::Other;
    }
    if after_ticks == b.len() {
        return match (done, ticks >= 3) {
            (false, _) => Line::NeedMore,
            (true, true) => Line::Bare {
                ticks,
                core: after_ticks,
                end: after_ticks,
                tail: false,
            },
            (true, false) => Line::Other,
        };
    }
    if ticks < 3 {
        return Line::Other;
    }
    let ws = b[after_ticks..]
        .iter()
        .take_while(|&&c| matches!(c, b' ' | b'\t' | b'\r'))
        .count();
    let core = after_ticks + ws;
    if core > FENCE_LINE_MAX {
        return Line::Other;
    }
    match b.get(core) {
        None if done => Line::Bare {
            ticks,
            core,
            end: core,
            tail: false,
        },
        None => Line::NeedMore,
        Some(b'\n') => Line::Bare {
            ticks,
            core,
            end: core + 1,
            tail: false,
        },
        Some(_) => match line_tail(&s[core..], quote_close, tags, done, FENCE_LINE_MAX - core) {
            LineTail::Closing(len) => Line::Bare {
                ticks,
                core,
                end: core + len,
                tail: true,
            },
            LineTail::Other => Line::Info { ticks, core },
            LineTail::NeedMore => Line::NeedMore,
        },
    }
}

/// 围栏之后同一行的内容
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineTail {
    /// 只有空白、闭合标签与闭合引号；值为到行尾（含换行）的字节数
    Closing(usize),
    /// 还有别的内容（语言标注等）
    Other,
    NeedMore,
}

fn line_tail(s: &str, quote: Option<char>, tags: bool, done: bool, budget: usize) -> LineTail {
    let mut i = 0usize;
    loop {
        if i > budget {
            return LineTail::Other;
        }
        let rest = &s[i..];
        let Some(c) = rest.chars().next() else {
            return if done { LineTail::Closing(i) } else { LineTail::NeedMore };
        };
        match c {
            '\n' => return LineTail::Closing(i + 1),
            ' ' | '\t' | '\r' => i += 1,
            _ if Some(c) == quote => i += c.len_utf8(),
            '<' if tags => match match_literals(rest, &CLOSE_TAGS) {
                Match::Full(n) => i += n,
                Match::Partial if !done => return LineTail::NeedMore,
                _ => return LineTail::Other,
            },
            _ => return LineTail::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一次扫描全部文本，返回（去掉配对符号后的文本, 已判定的字节数）
    fn apply(p: &mut Pairing, text: &str, done: bool) -> (String, usize) {
        let mut out = String::new();
        let mut start = 0usize;
        let decided = p.scan(text, done, &mut |r: Range<usize>| {
            out.push_str(&text[start..r.start]);
            start = r.end;
        });
        out.push_str(&text[start..decided]);
        (out, decided)
    }

    fn quote(close: char) -> Pairing {
        Pairing::new(None, Some(close), true, false)
    }

    #[test]
    fn straight_quotes_pair_by_context() {
        let cases = [
            // 末尾的闭合引号
            ("你好\"", "你好"),
            // 中段的闭合引号（开头引号没有包住全文）
            (
                "The Hitchhiker's Guide to the Galaxy\" is a comedy.",
                "The Hitchhiker's Guide to the Galaxy is a comedy.",
            ),
            // 内层引号成对保留
            ("He said \"hi\" to me.\"", "He said \"hi\" to me."),
            ("他说\"你好\"就走了。\"", "他说\"你好\"就走了。"),
            ("**\"Bold\"** title\"", "**\"Bold\"** title"),
            // 前后紧挨文字、同一行后面没有引号：就是配对的闭合引号
            (
                "很长的书名超过二十四个字符的那种\"是一本书。",
                "很长的书名超过二十四个字符的那种是一本书。",
            ),
            ("你好\"</translation>", "你好</translation>"),
            // 编辑动作：闭合引号在分隔线之前
            ("Body.\"\n\n---\n改动：无", "Body.\n\n---\n改动：无"),
            // 没有闭合引号
            ("no closer here", "no closer here"),
        ];
        for (raw, want) in cases {
            let (out, decided) = apply(&mut quote('"'), raw, true);
            assert_eq!((out.as_str(), decided), (want, raw.len()), "{raw:?}");
        }
    }

    #[test]
    fn curly_quotes_track_depth() {
        let (out, _) = apply(&mut quote('\u{201D}'), "他说“你好”就走了。”", true);
        assert_eq!(out, "他说“你好”就走了。");
        let (out, _) = apply(&mut quote('\u{201D}'), "书名”是一本书", true);
        assert_eq!(out, "书名是一本书");
    }

    #[test]
    fn straight_quote_waits_for_context() {
        // 末尾的直引号要等下一个字符（或流结束）才能判断
        let mut p = quote('"');
        assert_eq!(apply(&mut p, "abc\"", false), ("abc".to_owned(), 3));
        // 前后紧挨文字：向后看不足 40 个字符（且没到行尾）时等待
        let mut p = quote('"');
        assert_eq!(apply(&mut p, "书名\"是一本", false), ("书名".to_owned(), "书名".len()));
        let mut p = quote('"');
        let after = "是一本非常非常非常非常有趣的书，".repeat(3);
        let long = format!("书名\"{after}");
        let (out, decided) = apply(&mut p, &long, false);
        assert_eq!((out, decided), (format!("书名{after}"), long.len()));
        assert!(!p.is_active());
        // 行尾到了就能判断
        let mut p = quote('"');
        assert_eq!(apply(&mut p, "书名\"是一本书\n下一行", false).0, "书名是一本书\n下一行");
        // 内层引语长于 15 个字符（不超过 40）：仍认作内层开引号
        let mut p = quote('"');
        let raw = "他说\"非常非常非常非常非常长的内层引号内容\"然后结束。\"";
        assert_eq!(
            apply(&mut p, raw, true).0,
            "他说\"非常非常非常非常非常长的内层引号内容\"然后结束。"
        );
    }

    #[test]
    fn fences_pair_by_line() {
        let fence = || Pairing::new(Some(3), None, true, true);
        let cases = [
            ("你好\n```", "你好\n"),
            ("npm i\n```\n然后运行", "npm i\n然后运行"),
            ("a\n  ```  \nb", "a\nb"),
            ("x\n```</translation>", "x\n</translation>"),
            // 内层代码块
            ("# T\n```js\ncode\n```\nmore\n```", "# T\n```js\ncode\n```\nmore\n"),
            // 反引号不够、不独占一行、缩进超过 3：都不是闭合围栏
            ("a\n``\nb", "a\n``\nb"),
            ("a ```", "a ```"),
            ("a\n    ```\nb", "a\n    ```\nb"),
        ];
        for (raw, want) in cases {
            let (out, decided) = apply(&mut fence(), raw, true);
            assert_eq!((out.as_str(), decided), (want, raw.len()), "{raw:?}");
        }
        // 四个反引号的开头围栏：三个反引号的行是正文
        let mut p = Pairing::new(Some(4), None, false, true);
        assert_eq!(apply(&mut p, "```\nx\n````\ny", true).0, "```\nx\ny");
        // 行没写完时等待
        let mut p = fence();
        assert_eq!(apply(&mut p, "a\n``", false), ("a\n".to_owned(), 2));
    }

    #[test]
    fn fence_and_quote_together() {
        let mut p = Pairing::new(Some(3), Some('"'), true, true);
        assert_eq!(apply(&mut p, "hi\n```\"", true).0, "hi\n");
        let mut p = Pairing::new(Some(3), Some('"'), true, true);
        assert_eq!(apply(&mut p, "你好\"\n```", true).0, "你好\n");
    }

    #[test]
    fn fence_line_shapes() {
        assert_eq!(
            fence_line("```\nx", None, false, false),
            Line::Bare {
                ticks: 3,
                core: 3,
                end: 4,
                tail: false
            }
        );
        assert_eq!(
            fence_line("```js\n", None, false, false),
            Line::Info { ticks: 3, core: 3 }
        );
        assert_eq!(fence_line("``` ", None, false, false), Line::NeedMore);
        assert_eq!(fence_line("```</tr", None, true, false), Line::NeedMore);
        assert_eq!(
            fence_line("```</tr", None, true, true),
            Line::Info { ticks: 3, core: 3 }
        );
        assert_eq!(fence_line("hello", None, false, false), Line::Other);
        assert_eq!(fence_line("  ", None, false, false), Line::NeedMore);
        assert_eq!(fence_line(&"`".repeat(50), None, false, false), Line::Other);
    }
}
