//! 检索：用户输入放进 MATCH 前先包成 `"` + 内部 `"` 转义成 `""` + `"`，
//! 这样 `AND` / `OR` / `NEAR` / `*` / `-` / `(` 都按字面匹配；不足 3 个字符时 trigram 无法工作，退化为对最近 2000 条的
//! `LIKE`，`%`、`_`、`\` 用 `ESCAPE '\'` 转义。高亮区间按 **UTF-16** 偏移返回（JS 直接 `slice`）。
//!
//! **大小写折叠在各条路径上一致**：FTS5 trigram（`case_sensitive 0`）做 Unicode 单字符折叠；短查询的 `LIKE` 由
//! [`LikePattern`] 实现（`schema::register_functions` 覆盖了只折叠 ASCII 的内置 `like()`），高亮也用同一个
//! [`fold_char`]——「Дж」「дж」「ДЖ」在 1、2、3 字查询下结果一致。
//!
//! **检索范围**：每条正文的前 [`SEARCH_PREFIX_CHARS`] 个字符（FTS 索引、短查询 LIKE、上下文片段相同，见 `schema.rs`）。
//!
//! 匹配不在 preview（前 200 字符）里时，把 preview 换成「… 匹配处前后的上下文」，让结果里能看到命中位置
//! （`ClipStore::search` 只为前 64 条结果这样做，约 5 屏；片段在存储锁之外流式生成，见 [`Query::snippet`]）。

use std::collections::VecDeque;

use wisp_types::HighlightRange;

use crate::normalize::{PREVIEW_CHARS, make_preview};
pub(crate) use crate::schema::SEARCH_PREFIX_CHARS;

/// trigram 的最小查询长度（字符）
pub(crate) const FTS_MIN_CHARS: usize = 3;
/// 短查询 LIKE 扫描的范围：最近 2000 条
pub(crate) const LIKE_SCAN_ROWS: u32 = 2000;
/// 片段中匹配处之前保留的上下文字符数
const SNIPPET_LEAD_CHARS: usize = 24;
/// 每条结果最多返回的高亮区间数
const MAX_HIGHLIGHTS: usize = 32;
/// 片段开头的省略号
const ELLIPSIS: char = '…';

/// 规范化后的查询
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Query {
    /// 去掉首尾空白后的原文（放进 MATCH / LIKE）
    pub text: String,
    /// 用于高亮匹配：空白压缩为单个空格、按 [`fold_char`] 折叠大小写
    folded: Vec<char>,
}

impl Query {
    /// 去掉首尾空白；为空时返回 None
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        let text = raw.trim();
        if text.is_empty() {
            return None;
        }
        let compact = make_preview(text, usize::MAX);
        let folded = compact.chars().map(fold_char).collect();
        Some(Self {
            text: text.to_owned(),
            folded,
        })
    }

    /// 是否走 FTS（≥ 3 个字符）；否则走 LIKE
    pub(crate) fn uses_fts(&self) -> bool {
        self.text.chars().count() >= FTS_MIN_CHARS
    }

    /// FTS5 短语：整体加 `"`，内部 `"` 写成 `""`
    pub(crate) fn fts_phrase(&self) -> String {
        fts_phrase(&self.text)
    }

    /// LIKE 模式：`%` + 转义后的查询 + `%`，配合 `ESCAPE '\'`
    pub(crate) fn like_pattern(&self) -> String {
        format!("%{}%", escape_like(&self.text))
    }

    /// 在 preview 中找出全部（不重叠的）匹配区间
    pub(crate) fn highlight(&self, preview: &str) -> Vec<HighlightRange> {
        let hay = FoldedText::new(preview);
        hay.find_all(&self.folded, MAX_HIGHLIGHTS)
            .into_iter()
            .map(|(a, b)| hay.utf16_range(a, b))
            .collect()
    }

    /// 匹配不在 preview 里时：从正文前缀中找第一处匹配，生成「… 上下文」形式的新 preview 及其高亮。
    ///
    /// 流式进行：在空白压缩后的字符流上用 KMP 找第一处匹配，只保留匹配处之前一小段上下文，找到后再往后取够一个
    /// preview 的长度就停——耗时与匹配位置成正比，不为整段正文分配缓冲（正文可达 32768 个字符）
    pub(crate) fn snippet(&self, text: &str) -> Option<(String, Vec<HighlightRange>)> {
        let m = self.folded.len();
        if m == 0 {
            return None;
        }
        let fail = kmp_fail(&self.folded);
        // 最近读到的 SNIPPET_LEAD_CHARS + m 个字符（原样，未折叠）：匹配结束时，匹配本身与之前的上下文都在里面
        let window_cap = SNIPPET_LEAD_CHARS + m;
        let mut window: VecDeque<char> = VecDeque::with_capacity(window_cap);
        let mut stream = Compact::new(text.chars());
        let mut read = 0usize;
        let mut k = 0usize;
        let found = stream.by_ref().any(|c| {
            if window.len() == window_cap {
                window.pop_front();
            }
            window.push_back(c);
            read += 1;
            let f = fold_char(c);
            while k > 0 && self.folded[k] != f {
                k = fail[k - 1];
            }
            if self.folded[k] == f {
                k += 1;
            }
            k == m
        });
        if !found {
            return None;
        }
        // 下标都是压缩后字符流中的位置；window 覆盖 [read - window.len(), read)
        let window_start = read - window.len();
        let match_char = read - m;
        let mut start = match_char.saturating_sub(SNIPPET_LEAD_CHARS);
        if start > 0 {
            // 尽量不把一个词切成两半：上下文里有空格就从空格后开始
            if let Some(k) = (start..match_char).position(|i| window[i - window_start] == ' ') {
                start += k + 1;
            }
        }
        let mut body: String = window.iter().skip(start - window_start).collect();
        // 再往后取到 preview 的长度（多取一个，让截断处的空白处理与 make_preview 一致）
        body.extend(stream.take((PREVIEW_CHARS + 1).saturating_sub(read - start)));
        let preview = if start > 0 {
            let mut p = String::with_capacity(body.len().min(PREVIEW_CHARS * 4) + 3);
            p.push(ELLIPSIS);
            p.push_str(&make_preview(&body, PREVIEW_CHARS - 1));
            p
        } else {
            make_preview(&body, PREVIEW_CHARS)
        };
        let hl = self.highlight(&preview);
        (!hl.is_empty()).then_some((preview, hl))
    }
}

/// 空白压缩后的字符流，与 `make_preview(text, usize::MAX)` 逐字符一致：空白与控制字符的连续段变成一个空格，
/// 去掉首尾的空白
struct Compact<I> {
    chars: I,
    /// 已输出过可见字符
    started: bool,
    /// 上一个可见字符之后遇到了空白
    pending_space: bool,
    /// 先输出空格时暂存的可见字符
    held: Option<char>,
}

impl<I: Iterator<Item = char>> Compact<I> {
    fn new(chars: I) -> Self {
        Self {
            chars,
            started: false,
            pending_space: false,
            held: None,
        }
    }
}

impl<I: Iterator<Item = char>> Iterator for Compact<I> {
    type Item = char;

    fn next(&mut self) -> Option<char> {
        if let Some(c) = self.held.take() {
            return Some(c);
        }
        for c in self.chars.by_ref() {
            if c.is_whitespace() || c.is_control() {
                self.pending_space = self.started;
                continue;
            }
            self.started = true;
            if std::mem::take(&mut self.pending_space) {
                self.held = Some(c);
                return Some(' ');
            }
            return Some(c);
        }
        None
    }
}

/// KMP 的失配表：`fail[i]` = `p[..=i]` 的最长真前缀兼后缀的长度
fn kmp_fail<T: PartialEq>(p: &[T]) -> Vec<usize> {
    let mut fail = vec![0; p.len()];
    let mut k = 0;
    for i in 1..p.len() {
        while k > 0 && p[i] != p[k] {
            k = fail[k - 1];
        }
        if p[i] == p[k] {
            k += 1;
        }
        fail[i] = k;
    }
    fail
}

/// FTS5 短语转义
pub(crate) fn fts_phrase(q: &str) -> String {
    let mut s = String::with_capacity(q.len() + 2);
    s.push('"');
    for c in q.chars() {
        if c == '"' {
            s.push('"');
        }
        s.push(c);
    }
    s.push('"');
    s
}

/// LIKE 转义：`\` → `\\`、`%` → `\%`、`_` → `\_`
pub(crate) fn escape_like(q: &str) -> String {
    let mut s = String::with_capacity(q.len() + 4);
    for c in q.chars() {
        if matches!(c, '\\' | '%' | '_') {
            s.push('\\');
        }
        s.push(c);
    }
    s
}

/// 大小写折叠：Unicode 单字符映射（取 `char::to_lowercase` 的第一个字符），与 FTS5 的 `case_sensitive 0` 一致。
/// 一一对应，因此折叠后的下标就是原字符的下标
#[inline]
pub(crate) fn fold_char(c: char) -> char {
    match c {
        _ if c.is_ascii() => c.to_ascii_lowercase(),
        // CJK 符号与标点、假名、CJK 统一表意文字（U+3000–U+9FFF）与谚文音节没有大小写，免去查表（中文短查询的热点）
        '\u{3000}'..='\u{9FFF}' | '\u{AC00}'..='\u{D7AF}' => c,
        _ => c.to_lowercase().next().unwrap_or(c),
    }
}

/// 折叠大小写后的文本，记录每个字符的 UTF-16 区间
struct FoldedText {
    folded: Vec<char>,
    /// 第 i 个字符的 UTF-16 区间 [utf16[i].0, utf16[i].1)
    utf16: Vec<(u32, u32)>,
}

impl FoldedText {
    fn new(s: &str) -> Self {
        let mut folded = Vec::with_capacity(s.len());
        let mut utf16 = Vec::with_capacity(s.len());
        let mut pos = 0u32;
        for c in s.chars() {
            let len = c.len_utf16() as u32;
            folded.push(fold_char(c));
            utf16.push((pos, pos + len));
            pos += len;
        }
        Self { folded, utf16 }
    }

    /// 不重叠的匹配（字符下标的半开区间），最多 `max` 个
    fn find_all(&self, needle: &[char], max: usize) -> Vec<(usize, usize)> {
        let (n, m) = (self.folded.len(), needle.len());
        let mut out = Vec::new();
        if m == 0 || m > n {
            return out;
        }
        let mut i = 0;
        while i + m <= n && out.len() < max {
            if self.folded[i] == needle[0] && self.folded[i..i + m] == *needle {
                out.push((i, i + m));
                i += m;
            } else {
                i += 1;
            }
        }
        out
    }

    fn utf16_range(&self, a: usize, b: usize) -> HighlightRange {
        HighlightRange {
            start: self.utf16[a].0,
            end: self.utf16[b - 1].1,
        }
    }
}

/// 编译后的 LIKE 模式（`%` 任意串、`_` 任一字符、ESCAPE 之后的字符按字面），大小写按 [`fold_char`] 折叠
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LikePattern {
    tokens: Vec<LikeToken>,
    escape: Option<char>,
    /// `%字面串%` 形式（本库短查询的唯一形式）：直接做子串查找，不分配
    contains: Option<Needle>,
    /// 以孤立的转义符结尾：永不匹配
    never: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LikeToken {
    Lit(char),
    One,
    Any,
}

impl LikePattern {
    pub(crate) fn compile(pattern: &str, escape: Option<char>) -> Self {
        let mut tokens = Vec::with_capacity(pattern.len());
        let mut never = false;
        let mut chars = pattern.chars();
        while let Some(c) = chars.next() {
            let t = if Some(c) == escape {
                // 末尾孤立的转义符：与 SQLite 一致，永不匹配
                let Some(n) = chars.next() else {
                    never = true;
                    break;
                };
                LikeToken::Lit(fold_char(n))
            } else {
                match c {
                    '%' => LikeToken::Any,
                    '_' => LikeToken::One,
                    _ => LikeToken::Lit(fold_char(c)),
                }
            };
            if !(t == LikeToken::Any && tokens.last() == Some(&LikeToken::Any)) {
                tokens.push(t);
            }
        }
        let contains = match tokens.as_slice() {
            [LikeToken::Any, mid @ .., LikeToken::Any] if mid.iter().all(|t| matches!(t, LikeToken::Lit(_))) => {
                Some(Needle::new(mid.iter().filter_map(|t| match t {
                    LikeToken::Lit(c) => Some(*c),
                    _ => None,
                })))
            }
            _ => None,
        };
        Self {
            tokens,
            escape,
            contains,
            never,
        }
    }

    pub(crate) fn escape(&self) -> Option<char> {
        self.escape
    }

    pub(crate) fn matches(&self, text: &str) -> bool {
        if self.never {
            return false;
        }
        if let Some(needle) = &self.contains {
            return needle.found_in_str(text);
        }
        let t: Vec<char> = text.chars().map(fold_char).collect();
        wildcard_match(&self.tokens, &t)
    }
}

/// 子串查找（KMP，流式读入，不缓存正文）。两条精确的快速路径：
/// - 查询只由「没有大小写、也没有别的字符折叠成它」的字符组成（中日韩文字、谚文、ASCII 数字与标点）：
///   折叠不改变任何可能匹配的字符，直接对原文做字节子串查找（中文短查询的常见情形）；
/// - 查询是 ASCII、正文也是 ASCII：逐字节做 ASCII 折叠比较，免去字符解码
#[derive(Debug, Clone, PartialEq, Eq)]
struct Needle {
    chars: Vec<char>,
    fail: Vec<usize>,
    /// 可以直接做字节子串查找时的查询原文
    exact: Option<String>,
    /// 查询（已折叠）的 ASCII 字节；查询含非 ASCII 字符时为 None
    ascii: Option<Vec<u8>>,
}

/// 没有大小写、也没有其他字符折叠成它的字符
fn is_caseless(c: char) -> bool {
    if c.is_ascii() {
        !c.is_ascii_alphabetic()
    } else {
        matches!(c, '\u{3000}'..='\u{9FFF}' | '\u{AC00}'..='\u{D7AF}')
    }
}

impl Needle {
    fn new(chars: impl Iterator<Item = char>) -> Self {
        let chars: Vec<char> = chars.collect();
        let exact = chars.iter().all(|c| is_caseless(*c)).then(|| chars.iter().collect());
        let ascii = chars
            .iter()
            .all(char::is_ascii)
            .then(|| chars.iter().map(|c| *c as u8).collect());
        let fail = kmp_fail(&chars);
        Self {
            chars,
            fail,
            exact,
            ascii,
        }
    }

    fn found_in_str(&self, text: &str) -> bool {
        if let Some(exact) = &self.exact {
            return text.contains(exact.as_str());
        }
        if let Some(ascii) = &self.ascii
            && text.is_ascii()
        {
            return self.kmp(ascii.as_slice(), text.bytes().map(|b| b.to_ascii_lowercase()));
        }
        self.kmp(self.chars.as_slice(), text.chars().map(fold_char))
    }

    fn kmp<T: PartialEq + Copy>(&self, needle: &[T], hay: impl Iterator<Item = T>) -> bool {
        let m = needle.len();
        if m == 0 {
            return true;
        }
        let mut k = 0;
        for c in hay {
            while k > 0 && needle[k] != c {
                k = self.fail[k - 1];
            }
            if needle[k] == c {
                k += 1;
                if k == m {
                    return true;
                }
            }
        }
        false
    }
}

/// 通配匹配（`Any` 回溯到最近一个 `%`，O(n·m)）
fn wildcard_match(p: &[LikeToken], t: &[char]) -> bool {
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        match p.get(pi) {
            Some(LikeToken::Any) => {
                star = Some((pi, ti));
                pi += 1;
            }
            Some(LikeToken::One) => {
                pi += 1;
                ti += 1;
            }
            Some(LikeToken::Lit(c)) if *c == t[ti] => {
                pi += 1;
                ti += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    pi = sp + 1;
                    ti = st + 1;
                    star = Some((sp, st + 1));
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|t| *t == LikeToken::Any)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按 UTF-16 区间切片（模拟 JS 的 `String.prototype.slice`）
    fn js_slice(s: &str, r: HighlightRange) -> String {
        let units: Vec<u16> = s.encode_utf16().collect();
        String::from_utf16(&units[r.start as usize..r.end as usize]).expect("合法 UTF-16")
    }

    #[test]
    fn phrase_escaping() {
        assert_eq!(fts_phrase("abc"), "\"abc\"");
        assert_eq!(fts_phrase("a\"b"), "\"a\"\"b\"");
        assert_eq!(fts_phrase("\"\""), "\"\"\"\"\"\"");
        assert_eq!(fts_phrase("AND OR NEAR * - ("), "\"AND OR NEAR * - (\"");
    }

    #[test]
    fn like_escaping() {
        assert_eq!(escape_like("50%"), "50\\%");
        assert_eq!(escape_like("c_d"), "c\\_d");
        assert_eq!(escape_like("a\\b"), "a\\\\b");
        assert_eq!(Query::parse("%").expect("q").like_pattern(), "%\\%%");
    }

    #[test]
    fn query_routing() {
        assert!(Query::parse("   ").is_none());
        assert!(!Query::parse("中文").expect("q").uses_fts());
        assert!(Query::parse("中文字").expect("q").uses_fts());
        assert!(!Query::parse(" ab ").expect("q").uses_fts());
        assert!(Query::parse("😀😀😀").expect("q").uses_fts());
    }

    #[test]
    fn highlights_are_utf16_and_case_insensitive() {
        let q = Query::parse("Wisp").expect("q");
        let preview = "😀 wisp 与 WISP，wispy";
        let hl = q.highlight(preview);
        assert_eq!(hl.len(), 3);
        for r in &hl {
            assert_eq!(js_slice(preview, *r).to_lowercase(), "wisp");
        }
        // 😀 占 2 个 UTF-16 单元
        assert_eq!(hl[0], HighlightRange { start: 3, end: 7 });

        let q = Query::parse("中文").expect("q");
        let preview = "𠮷中文🎉中文";
        let hl = q.highlight(preview);
        assert_eq!(
            hl.iter().map(|r| js_slice(preview, *r)).collect::<Vec<_>>(),
            ["中文", "中文"]
        );
        assert_eq!(hl[0].start, 2);
    }

    #[test]
    fn highlight_matches_compressed_whitespace() {
        let q = Query::parse("foo   bar").expect("q");
        let hl = q.highlight("x foo bar y");
        assert_eq!(hl, [HighlightRange { start: 2, end: 9 }]);
    }

    #[test]
    fn highlight_folds_one_to_one_like_fts() {
        // 'İ'（U+0130）按单字符折叠为 'i'（与 FTS5 一致），区间落在原字符边界上
        let q = Query::parse("ix").expect("q");
        assert_eq!(q.highlight("aİxb"), [HighlightRange { start: 1, end: 3 }]);
        // 西里尔字母、希腊字母、带重音的拉丁字母
        let q = Query::parse("ДЖ").expect("q");
        assert_eq!(q.highlight("Джон"), [HighlightRange { start: 0, end: 2 }]);
        let q = Query::parse("é").expect("q");
        assert_eq!(q.highlight("ÉCOLE"), [HighlightRange { start: 0, end: 1 }]);
    }

    #[test]
    fn fold_fast_path_matches_unicode_lowercase() {
        for c in ('\u{3000}'..='\u{9FFF}').chain('\u{AC00}'..='\u{D7AF}') {
            assert_eq!(c.to_lowercase().next(), Some(c), "{c:?} 有小写映射，不能走快速路径");
        }
        assert_eq!(fold_char('Ａ'), 'ａ', "全角拉丁字母仍要折叠");
        assert_eq!(fold_char('Ж'), 'ж');
    }

    #[test]
    fn like_patterns() {
        let m = |p: &str, t: &str| LikePattern::compile(p, Some('\\')).matches(t);
        assert!(m("%дж%", "Джон"));
        assert!(m("%%", ""));
        assert!(m("a%c", "abbbc") && !m("a%c", "abbbd"));
        assert!(m("a_c", "aXc") && !m("a_c", "ac"));
        assert!(m("%a%b%", "xxaxxbxx") && !m("%a%b%", "xxbxxaxx"));
        assert!(m("50\\%", "50%") && !m("50\\%", "500"));
        assert!(!m("abc\\", "abc"), "末尾孤立的转义符不匹配");
        assert!(LikePattern::compile("%ab%", None).contains.is_some());
        assert!(LikePattern::compile("a%b", None).contains.is_none());
        // KMP：部分匹配后回退
        assert!(m("%aab%", "aaab") && m("%abab%", "abaabab"));
        // 三条路径的结果一致：中文（字节查找）、ASCII（逐字节折叠）、混合（逐字符折叠）
        assert!(m("%中国%", "我爱中国") && !m("%中国%", "中 国"));
        assert!(m("%QU%", "the quick") && m("%qu%", "THE QUICK"));
        assert!(m("%qu%", "Джон QUICK") && m("%дж%", "ДЖОН quick"));
        assert!(
            m("%k%", "\u{212A}elvin"),
            "开尔文符号折叠为 k，ASCII 查询也要命中非 ASCII 正文"
        );
        assert!(
            LikePattern::compile("%的%", None)
                .contains
                .as_ref()
                .is_some_and(|n| n.exact.is_some())
        );
        assert!(
            LikePattern::compile("%a1%", None)
                .contains
                .as_ref()
                .is_some_and(|n| n.exact.is_none())
        );
    }

    #[test]
    fn snippet_moves_preview_to_match() {
        let text = format!(
            "{}\n\nthe needle is here and more text after",
            "lorem ipsum dolor ".repeat(30)
        );
        let q = Query::parse("NEEDLE").expect("q");
        assert!(q.highlight(&make_preview(&text, PREVIEW_CHARS)).is_empty());
        let (preview, hl) = q.snippet(&text).expect("片段");
        assert!(preview.starts_with('…'), "{preview}");
        assert!(preview.chars().count() <= PREVIEW_CHARS);
        assert_eq!(hl.len(), 1);
        assert_eq!(js_slice(&preview, hl[0]), "needle");
        // 不切断单词
        assert!(!preview.starts_with("…orem") && !preview.contains("…m "), "{preview}");
        assert!(q.snippet("nothing to see").is_none());
    }

    /// 旧的整段实现（压缩 → 折叠 → 找第一处匹配），作为流式 [`Query::snippet`] 的参照
    fn snippet_reference(q: &Query, text: &str) -> Option<(String, Vec<HighlightRange>)> {
        let compact = make_preview(text, usize::MAX);
        let hay = FoldedText::new(&compact);
        let (first, _) = *hay.find_all(&q.folded, 1).first()?;
        let chars: Vec<char> = compact.chars().collect();
        let mut start = first.saturating_sub(SNIPPET_LEAD_CHARS);
        if start > 0
            && let Some(k) = chars[start..first].iter().position(|c| *c == ' ')
        {
            start += k + 1;
        }
        let body: String = chars[start..].iter().collect();
        let preview = if start > 0 {
            format!("{ELLIPSIS}{}", make_preview(&body, PREVIEW_CHARS - 1))
        } else {
            make_preview(&body, PREVIEW_CHARS)
        };
        let hl = q.highlight(&preview);
        (!hl.is_empty()).then_some((preview, hl))
    }

    #[test]
    fn snippet_handles_match_at_edges_and_long_queries() {
        let q = Query::parse("abc").expect("q");
        assert_eq!(q.snippet("abc"), snippet_reference(&q, "abc"));
        assert_eq!(q.snippet("  \n abc"), snippet_reference(&q, "  \n abc"));
        let long_query = "word ".repeat(60);
        let q = Query::parse(&long_query).expect("q");
        let text = format!("{}{}tail", "x ".repeat(300), long_query);
        // 比 preview 还长的查询放不进片段：与整段实现一样不换 preview
        assert_eq!(q.snippet(&text), snippet_reference(&q, &text));
        assert!(q.snippet(&text).is_none());
        let q = Query::parse(&"word ".repeat(30)).expect("q");
        assert_eq!(q.snippet(&text), snippet_reference(&q, &text));
        assert!(q.snippet(&text).is_some());
        // 部分匹配后回退（KMP）
        let q = Query::parse("aab").expect("q");
        let text = format!("{}aaab", "z".repeat(250));
        let (preview, hl) = q.snippet(&text).expect("片段");
        assert_eq!(js_slice(&preview, hl[0]), "aab");
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        fn like(conn: &rusqlite::Connection, text: &str, pattern: &str) -> bool {
            conn.query_row("SELECT ?1 LIKE ?2 ESCAPE '\\'", [text, pattern], |r| r.get(0))
                .expect("LIKE")
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(300))]

            /// 高亮区间：有序、不重叠、落在 UTF-16 字符边界内
            #[test]
            fn highlight_ranges_are_valid(preview in "\\PC{0,80}", q in "\\PC{1,6}") {
                if let Some(query) = Query::parse(&q) {
                    let units: Vec<u16> = preview.encode_utf16().collect();
                    let mut last_end = 0;
                    for r in query.highlight(&preview) {
                        prop_assert!(r.start < r.end && r.end as usize <= units.len());
                        prop_assert!(r.start >= last_end);
                        prop_assert!(String::from_utf16(&units[r.start as usize..r.end as usize]).is_ok());
                        last_end = r.end;
                    }
                }
            }

            /// preview 中确实出现的查询一定被高亮，且切片（忽略大小写）等于查询
            #[test]
            fn contained_query_is_highlighted(a in "\\PC{0,30}", q in "[^\\s\\p{C}]{1,6}", b in "\\PC{0,30}") {
                let preview = format!("{a}{q}{b}");
                let query = Query::parse(&q).expect("非空");
                let hl = query.highlight(&preview);
                prop_assert!(!hl.is_empty());
                let units: Vec<u16> = preview.encode_utf16().collect();
                for r in hl {
                    let slice = String::from_utf16(&units[r.start as usize..r.end as usize]).expect("UTF-16");
                    let fold = |s: &str| s.chars().map(fold_char).collect::<String>();
                    prop_assert_eq!(fold(&slice), fold(&q));
                }
            }

            /// 流式压缩与 make_preview 的整段压缩逐字符一致
            #[test]
            fn compact_stream_matches_make_preview(s in "\\PC{0,80}|[ \\t\\n\\r\\u{0}-\\u{1f}a中]{0,80}") {
                prop_assert_eq!(Compact::new(s.chars()).collect::<String>(), make_preview(&s, usize::MAX));
            }

            /// 流式片段与整段实现的结果完全一致（匹配位置、上下文的起点、截断与高亮）
            #[test]
            fn streaming_snippet_matches_reference(
                pre in "[a-c \\nДд中]{0,400}",
                q in "[a-cДд中 ]{1,5}",
                post in "[a-c \\nДд中]{0,300}",
                hit in proptest::bool::ANY,
            ) {
                if let Some(query) = Query::parse(&q) {
                    let text = if hit { format!("{pre}{q}{post}") } else { format!("{pre}{post}") };
                    prop_assert_eq!(query.snippet(&text), snippet_reference(&query, &text));
                }
            }

            /// LIKE 转义后与「按 fold_char 折叠后的子串」完全等价：% _ \ 都按字面匹配，非 ASCII 字母同样不区分大小写
            #[test]
            fn like_escape_is_literal(text in "[a-cA-Cдж ДЖé É%_\\\\ ]{0,12}", q in "[a-cA-CдДжЖéÉ%_\\\\]{1,3}") {
                let conn = rusqlite::Connection::open_in_memory().expect("内存库");
                crate::schema::register_functions(&conn).expect("注册 like");
                let pattern = format!("%{}%", escape_like(&q));
                let fold = |s: &str| s.chars().map(fold_char).collect::<String>();
                let expect = fold(&text).contains(&fold(&q));
                prop_assert_eq!(like(&conn, &text, &pattern), expect);
            }

            /// 任意查询放进 MATCH 都不报语法错误，且正文包含查询时一定命中
            #[test]
            fn fts_phrase_never_errors(q in "[a-z\"*()^:+\\- ]{3,8}|AND|NOT|NEAR|OR NOT", prefix in "[a-z ]{0,5}") {
                let conn = rusqlite::Connection::open_in_memory().expect("内存库");
                conn.execute_batch("CREATE VIRTUAL TABLE t USING fts5(x, tokenize='trigram case_sensitive 0')")
                    .expect("建表");
                let text = format!("{prefix}{q} tail");
                conn.execute("INSERT INTO t(x) VALUES (?1)", [&text]).expect("插入");
                let n: i64 = conn
                    .query_row("SELECT count(*) FROM t WHERE t MATCH ?1", [fts_phrase(&q)], |r| r.get(0))
                    .expect("MATCH 不应报错");
                prop_assert_eq!(n, 1);
            }
        }
    }
}
