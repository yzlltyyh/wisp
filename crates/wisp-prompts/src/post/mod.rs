//! 流式安全的后处理：翻译 / 编辑 / OCR 三种。
//!
//! 位于 `<think>` 剥离（wisp-llm）之后、16 ms 合并之前。前端只做追加，所以这里只能「延迟放行」，不能回改：
//!
//! - **开头**：缓冲到能确定开头是否为需要剥离的内容为止（前缀如「译文：」/ `Translation:` / `Here is the translation:`，
//!   `<translation>` / `<source_text>` 回显标签，包住全文的代码围栏或引号）。一旦确定不可能是这些形式就立即放行。
//!   引号 / 围栏之后最多看 24 个字符找闭合符号：看到闭合符号且其后还有正文 → 不剥（正文自带的引号、
//!   代码块原样保留）；没看到 → 剥，之后由 [`pairing`] 在正文里成对去掉闭合符号。`Here is the translation …:`
//!   这类整行前言会一直缓冲到行尾的冒号（短语之后 ≤ 48 字符）。
//! - **中段**：剥掉了开头引号 / 围栏时，配对的闭合符号无论出现在哪里都一并去掉（判断时滞留 ≤ 40 个字符）；
//!   其余原样放行，只在末尾滞留可能被剥离的空白与闭合标签（空白不限，非空白 ≤ 16 个字符）。
//! - **结束**：剥离末尾空白与闭合标签；整段输出不足以做出判断时按完整文本一次处理。
//! - **OCR**：不套用引号与标签规则；开头额外缓冲到不可能是「无文字」的形式（`<NO_TEXT>`、`` `null` ``、
//!   `Transcription: <NO_TEXT>`……，语法见 [`is_no_text`]）为止，结束时判定 `no_text`，判为无文字时
//!   不放行任何文本。缓冲条件与 `is_no_text` 共用同一套语法，覆盖每种无文字形式的每个前缀（有随机化的性质测试）。
//!
//! 每一步的判断都只依赖文本内容本身（开头的有界前缀、配对符号之后有界长度的文本、末尾「空白 + ≤ 16 个非空白字符」
//! 的后缀），与已经收到多少无关，因此**任意切分方式喂入，拼接结果都相同**（测试逐一验证）。

mod edit_split;
mod ocr;
mod pairing;
mod translate;

pub use edit_split::split_edit_output;
pub use ocr::is_no_text;

use pairing::{Line, Pairing, fence_line};
use wisp_types::OcrMode;

/// 后处理种类
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PostKind {
    Translate,
    Edit,
    Ocr {
        mode: OcrMode,
    },
    /// 只去掉首尾空白（开头的空行与首行缩进、末尾空白），不做任何前缀 / 标签 / 围栏 / 引号剥离。
    /// refine 反思步骤（[`PromptTask::RefineReflect`](crate::PromptTask::RefineReflect)）的输出是审校意见，必须原样
    /// 交给改进步骤
    Raw,
}

/// 开头引号 / 围栏之后找闭合符号的窗口（字符数）
const HEAD_LIMIT: usize = 24;
/// 窗口内找到闭合符号、其后只有空白 / 闭合标签时，最多再看多少字符来确认它包住全文
const TAIL_LOOK: usize = 24;
/// 末尾滞留的最大**非空白**字符数；空白不计数（滞留空白没有可见的代价）
const TAIL_LIMIT: usize = 16;
/// 末尾候选起点的上限：空白段的开头与闭合序列的首字符，各至多 TAIL_LIMIT + 1 个
const MAX_TAIL_STARTS: usize = 2 * TAIL_LIMIT + 2;
/// 开头最多连续剥离几层（前缀、标签、围栏、引号的组合）
const MAX_HEAD_STEPS: usize = 8;
/// 「Here is the translation …:」这类整行前言在短语之后最多再看多少字符找冒号
const MAX_PREAMBLE_REST: usize = 48;
/// 围栏语言标注的最大长度
const MAX_FENCE_LANG: usize = 20;

/// 回显标签（开头剥离）
const OPEN_TAGS: [&str; 2] = ["<translation>", "<source_text>"];
/// 闭合标签（结尾剥离）
const CLOSE_TAGS: [&str; 2] = ["</translation>", "</source_text>"];

/// 前缀组：原文本身以某组的形式开头时整组关闭（跨语言：原文 `Translate: …` 的正确译文就是「翻译：…」）
const GROUP_TRANSLATION: u8 = 1;
const GROUP_EDIT: u8 = 1 << 1;
const GROUP_OCR: u8 = 1 << 2;

/// 一个需要剥离的开头前缀
#[derive(Debug, Clone, Copy)]
struct Prefix {
    /// 短语（ASCII 部分大小写不敏感）
    phrase: &'static str,
    /// true：短语之后同一行内（≤ 48 字符）必须出现冒号，整行剥离；false：短语之后（可隔空格）紧跟冒号
    rest_of_line: bool,
    group: u8,
}

impl Prefix {
    /// 短语 + 冒号（全角、半角都算）
    const fn colon(group: u8, phrase: &'static str) -> Self {
        Self {
            phrase,
            rest_of_line: false,
            group,
        }
    }

    /// 短语 + 同一行内的冒号（整行前言）
    const fn line(group: u8, phrase: &'static str) -> Self {
        Self {
            phrase,
            rest_of_line: true,
            group,
        }
    }
}

/// 包住全文的围栏是否剥离
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FencePolicy {
    Off,
    /// 任意语言标注
    AnyLang,
    /// 只剥离这些语言标注（"" 表示没有标注；大小写不敏感）
    Langs(&'static [&'static str]),
}

/// 一种后处理的规则集合（由种类与原文决定）
#[derive(Debug, Clone, Copy)]
struct Rules {
    prefixes: &'static [Prefix],
    /// 被原文关闭的前缀组（位掩码）
    disabled_groups: u8,
    /// 剥离 `<translation>` / `<source_text>` 回显与结尾的闭合标签
    tags: bool,
    /// 剥离包住全文的成对引号
    quotes: bool,
    fence: FencePolicy,
    /// 去掉首行缩进（原文首行没有缩进时）
    trim_indent: bool,
    /// OCR：开头缓冲到不可能是「无文字」的形式为止，结束时判定 no_text
    no_text: bool,
    /// 编辑动作：闭合符号之后紧跟 `\n---\n改动…` 也算包住正文
    edit_tail: bool,
}

/// [`PostKind::Raw`]：全部剥离规则关闭，只剩开头空行 / 首行缩进与末尾空白的处理（等价于整段 trim，且切分无关）
const RAW_RULES: Rules = Rules {
    prefixes: &[],
    disabled_groups: 0,
    tags: false,
    quotes: false,
    fence: FencePolicy::Off,
    trim_indent: true,
    no_text: false,
    edit_tail: false,
};

/// 结束时的结果
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PostFinish {
    pub tail: String,
    pub no_text: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Head,
    Body,
    Finished,
}

/// 开头判定的结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Decision {
    /// 开头被剥离的字节数
    consumed: usize,
    /// 剥离了开头的围栏：反引号数（正文中要成对去掉闭合围栏）
    fence: Option<usize>,
    /// 剥离了开头的引号：对应的闭合引号（正文中要成对去掉）
    quote: Option<char>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scan {
    NeedMore,
    Decided(Decision),
}

/// 某个开头形式的匹配结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Match {
    /// 完整匹配，长度（字节）
    Full(usize),
    /// 已有文本是它的前缀，需要更多文本
    Partial,
    No,
}

/// 流式后处理器
#[derive(Debug, Clone)]
pub struct PostProcessor {
    rules: Rules,
    phase: Phase,
    /// 开头阶段尚未放行的原始文本
    head: String,
    /// 配对跟踪器尚未判定的原始文本（只在等待配对的闭合符号时非空，≤ 16 个字符或一行候选围栏）
    raw: String,
    /// 已判定、尚未放行的文本：末尾可能被剥离的空白与闭合标签
    pending: String,
    pairing: Pairing,
    /// 进入中段时确定的闭合标签集合（之后不再变化）
    closers: Closers,
}

impl PostProcessor {
    /// 新建处理器
    pub fn new(kind: PostKind, source_text: &str) -> Self {
        let rules = match kind {
            PostKind::Translate => translate::rules(source_text, false),
            PostKind::Edit => translate::rules(source_text, true),
            PostKind::Ocr { mode } => ocr::rules(mode),
            PostKind::Raw => RAW_RULES,
        };
        Self {
            rules,
            phase: Phase::Head,
            head: String::new(),
            raw: String::new(),
            pending: String::new(),
            pairing: Pairing::default(),
            closers: Closers::default(),
        }
    }

    /// 返回本次可以放行的文本。
    ///
    /// 开头：确定不是需要剥离的形式就立即放行（不足 24 字符就结束时在 [`finish`](Self::finish) 放行）；以引号 / 围栏
    /// 开头时最多再看其后 24 个字符（看到闭合符号时再看其后 ≤ 24 个字符）；以 `Here is the translation` 类前言开头时
    /// 缓冲到行尾冒号（短语之后 ≤ 48 字符）；OCR 另缓冲到不可能是「无文字」形式为止。末尾滞留可能被剥离的空白与
    /// 闭合标签（空白不限，非空白 ≤ 16 个字符）；剥掉了开头引号 / 围栏、正在判断配对的闭合符号时，另滞留 ≤ 40 个
    /// 字符（直引号向后看到行尾，或一行 ≤ 40 字节的候选围栏）。
    pub fn push(&mut self, delta: &str) -> Option<String> {
        if delta.is_empty() {
            return None;
        }
        match self.phase {
            Phase::Finished => None,
            Phase::Body => self.feed(delta),
            Phase::Head => {
                self.head.push_str(delta);
                if self.rules.no_text && ocr::could_be_no_text(&self.head) {
                    return None;
                }
                match scan_head(&self.rules, &self.head, false) {
                    Scan::NeedMore => None,
                    Scan::Decided(d) => {
                        let head = std::mem::take(&mut self.head);
                        self.enter_body(&head, d);
                        self.feed(&head[d.consumed..])
                    }
                }
            }
        }
    }

    /// 结束：冲刷滞留文本并判定 no_text
    pub fn finish(&mut self) -> PostFinish {
        let mut tail = String::new();
        match self.phase {
            Phase::Finished => return PostFinish::default(),
            Phase::Body => {}
            Phase::Head => {
                let head = std::mem::take(&mut self.head);
                if self.rules.no_text && is_no_text(&head) {
                    self.phase = Phase::Finished;
                    return PostFinish { tail, no_text: true };
                }
                let d = match scan_head(&self.rules, &head, true) {
                    Scan::Decided(d) => d,
                    // done = true 时总能做出判断；万一没有，按「不剥离」处理
                    Scan::NeedMore => Decision {
                        consumed: 0,
                        fence: None,
                        quote: None,
                    },
                };
                self.enter_body(&head, d);
                // 与「先全部 push 再 finish」走同一条路径，保证结果与切分方式无关
                if let Some(out) = self.feed(&head[d.consumed..]) {
                    tail = out;
                }
            }
        }
        if !self.raw.is_empty() {
            self.decide_raw(true);
        }
        tail.push_str(self.strip_end());
        self.pending.clear();
        self.raw.clear();
        self.phase = Phase::Finished;
        PostFinish { tail, no_text: false }
    }

    fn enter_body(&mut self, head: &str, d: Decision) {
        self.phase = Phase::Body;
        // 正文是否从行首开始：剥掉的开头在最后一个换行之后只有缩进
        let at_line_start = head[..d.consumed].rfind('\n').is_some_and(|i| {
            head[i + 1..d.consumed]
                .trim_start_matches([' ', '\t', '\u{3000}'])
                .is_empty()
        });
        self.pairing = Pairing::new(d.fence, d.quote, self.rules.tags, at_line_start);
        let mut c = Closers::default();
        if self.rules.tags {
            c.push(CLOSE_TAGS[0]);
            c.push(CLOSE_TAGS[1]);
        }
        self.closers = c;
    }

    /// 中段：追加文本，放行除末尾闭合序列与未判定部分之外的文本
    fn feed(&mut self, text: &str) -> Option<String> {
        if text.is_empty() {
            return None;
        }
        if self.pairing.is_active() {
            self.raw.push_str(text);
            self.decide_raw(false);
            return self.emit_pending();
        }
        if !self.pending.is_empty() {
            // 滞留部分以空白结尾、新文本也全是空白：整体仍是合法的滞留后缀（非空白字符数不变），不必重新扫描。
            // 模型输出大段空白时，每个空白字符因此只被扫描常数次
            if self.pending.ends_with(char::is_whitespace) && text.chars().all(char::is_whitespace) {
                self.pending.push_str(text);
                return None;
            }
            self.pending.push_str(text);
            return self.emit_pending();
        }
        // 快速路径：没有滞留文本时直接在 delta 上判断，不经过缓冲区
        let hold = holdback(text, &self.closers);
        self.pending.push_str(&text[text.len() - hold..]);
        let out = &text[..text.len() - hold];
        (!out.is_empty()).then(|| out.to_owned())
    }

    /// 让配对跟踪器判定 `raw`：判定的部分去掉配对符号后移入 `pending`
    fn decide_raw(&mut self, done: bool) {
        let (raw, pending) = (&self.raw, &mut self.pending);
        let mut start = 0usize;
        let decided = self.pairing.scan(raw, done, &mut |r| {
            pending.push_str(&raw[start..r.start]);
            start = r.end;
        });
        pending.push_str(&raw[start..decided]);
        self.raw.drain(..decided);
    }

    /// 放行 `pending` 中除末尾闭合序列之外的部分
    fn emit_pending(&mut self) -> Option<String> {
        let hold = holdback(&self.pending, &self.closers);
        let emit = self.pending.len() - hold;
        if emit == 0 {
            return None;
        }
        let out = self.pending[..emit].to_owned();
        // 滞留部分 ≤ 16 个字符，前移的开销可以忽略
        self.pending.drain(..emit);
        Some(out)
    }

    /// 结束时剥离滞留文本末尾的空白与闭合标签
    fn strip_end(&self) -> &str {
        let mut s = self.pending.as_str();
        loop {
            s = s.trim_end();
            match self
                .rules
                .tags
                .then(|| CLOSE_TAGS.iter().find(|t| ends_with_ignore_ascii_case(s, t)))
                .flatten()
            {
                Some(tag) => s = &s[..s.len() - tag.len()],
                None => return s,
            }
        }
    }
}

/// 最多两个闭合标签，连同字符集合一次算好，避免每个 delta 重复计算
#[derive(Debug, Clone, Copy, Default)]
struct Closers {
    items: [&'static str; 2],
    len: usize,
    /// 闭合序列中出现过的 ASCII 字符（大小写都记）
    ascii: u128,
    /// 闭合序列的首字符（ASCII）
    ascii_start: u128,
}

impl Closers {
    /// 只接受 ASCII 闭合序列
    fn push(&mut self, s: &'static str) {
        if self.len >= self.items.len() || !s.is_ascii() {
            return;
        }
        self.items[self.len] = s;
        self.len += 1;
        for (i, c) in s.chars().enumerate() {
            let bits = (1u128 << (c.to_ascii_lowercase() as u32)) | (1u128 << (c.to_ascii_uppercase() as u32));
            self.ascii |= bits;
            if i == 0 {
                self.ascii_start |= bits;
            }
        }
    }

    #[cfg(test)]
    fn of(items: &[&'static str]) -> Self {
        let mut c = Self::default();
        for s in items {
            c.push(s);
        }
        c
    }

    fn as_slice(&self) -> &[&'static str] {
        &self.items[..self.len]
    }

    /// 字符是否可能出现在某个闭合序列里
    fn may_contain(&self, c: char) -> bool {
        c.is_ascii() && self.ascii & (1u128 << (c as u32)) != 0
    }

    /// 字符是否可能是某个闭合序列的首字符
    fn may_start(&self, c: char) -> bool {
        c.is_ascii() && self.ascii_start & (1u128 << (c as u32)) != 0
    }
}

/// 末尾需要滞留的字节数：最长的「(空白 | 完整闭合序列)* 不完整的闭合序列?」后缀，其中非空白字符不超过 16 个。
///
/// 空白不计入上限：闭合标签前后多几个换行（`你好\n\n</translation>\n\n\n`）时，标签仍在滞留范围内，结束时能剥掉；
/// 开头的空白段同样整段滞留，译文末尾不会多出换行。
fn holdback(s: &str, closers: &Closers) -> usize {
    let Some(last) = s.chars().next_back() else {
        return 0;
    };
    // 最常见的情况：最后一个字符既不是空白，也不可能属于任何闭合序列
    if !last.is_whitespace() && !closers.may_contain(last) {
        return 0;
    }
    // 从末尾往前收集候选起点（合法后缀只能从一段空白的开头、或闭合序列的首字符开始）；
    // 遇到不可能属于闭合序列的字符，或非空白字符超过上限时停止
    let mut starts = [0usize; MAX_TAIL_STARTS];
    let mut n = 0;
    let mut visible = 0usize;
    let mut chars = s.char_indices().rev().peekable();
    while let Some((i, c)) = chars.next() {
        let start = if c.is_whitespace() {
            chars.peek().is_none_or(|&(_, p)| !p.is_whitespace())
        } else {
            visible += 1;
            if visible > TAIL_LIMIT || !closers.may_contain(c) {
                break;
            }
            closers.may_start(c)
        };
        if start && n < MAX_TAIL_STARTS {
            starts[n] = i;
            n += 1;
        }
    }
    // 最早的合法起点就是最长的后缀
    starts[..n]
        .iter()
        .rev()
        .find(|&&i| is_closing_prefix(&s[i..], closers.as_slice()))
        .map_or(0, |&i| s.len() - i)
}

/// `s` 是否形如「(空白 | 完整闭合序列)* 不完整的闭合序列?」
fn is_closing_prefix(mut s: &str, closers: &[&str]) -> bool {
    loop {
        s = s.trim_start();
        if s.is_empty() {
            return true;
        }
        let mut advanced = false;
        for c in closers {
            let (cb, sb) = (c.as_bytes(), s.as_bytes());
            if sb.len() >= cb.len() {
                if sb[..cb.len()].eq_ignore_ascii_case(cb) {
                    s = &s[cb.len()..];
                    advanced = true;
                    break;
                }
            } else if cb[..sb.len()].eq_ignore_ascii_case(sb) {
                return true;
            }
        }
        if !advanced {
            return false;
        }
    }
}

/// 开头判定：依次剥离空白行、回显标签、围栏、前缀、引号（任意顺序，最多 8 层）
fn scan_head(rules: &Rules, buf: &str, done: bool) -> Scan {
    let mut pos = 0usize;
    let mut state = HeadState {
        fence: None,
        quote: None,
        prefix_used: false,
    };
    for _ in 0..MAX_HEAD_STEPS {
        match skip_blank_lines(&buf[pos..], done) {
            Some(n) => pos += n,
            None => return Scan::NeedMore,
        }
        let r = &buf[pos..];
        let body = r.trim_start_matches([' ', '\t']);
        let indent = r.len() - body.len();
        let consumed = match first_opener(rules, body, done, state) {
            Opener::NeedMore => return Scan::NeedMore,
            Opener::None => break,
            Opener::Tag(n) => n,
            Opener::Fence(n, ticks) => {
                state.fence = Some(ticks);
                n
            }
            Opener::Prefix(n) => {
                state.prefix_used = true;
                n
            }
            Opener::Quote(n, close) => {
                state.quote = Some(close);
                n
            }
        };
        pos += indent + consumed;
        // 开头形式之后同一行的空白一并去掉
        let r = &buf[pos..];
        pos += r.len() - r.trim_start_matches([' ', '\t']).len();
    }
    if rules.trim_indent {
        let r = &buf[pos..];
        pos += r.len() - r.trim_start_matches([' ', '\t', '\u{3000}']).len();
    }
    Scan::Decided(Decision {
        consumed: pos,
        fence: state.fence,
        quote: state.quote,
    })
}

/// 跳过开头的空白行，返回跳过的字节数；只有空白、还没换行且流未结束时返回 None（等待）
fn skip_blank_lines(r: &str, done: bool) -> Option<usize> {
    let mut n = 0usize;
    loop {
        let rest = &r[n..];
        match rest.find('\n') {
            Some(i) if rest[..i].trim().is_empty() => n += i + 1,
            Some(_) => return Some(n),
            None if !rest.trim().is_empty() => return Some(n),
            None if done => return Some(r.len()),
            None => return None,
        }
    }
}

/// 开头已经剥离的形式
#[derive(Debug, Clone, Copy)]
struct HeadState {
    /// 开头围栏的反引号数
    fence: Option<usize>,
    /// 开头引号对应的闭合引号
    quote: Option<char>,
    prefix_used: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Opener {
    NeedMore,
    None,
    Tag(usize),
    /// 长度、反引号数
    Fence(usize, usize),
    Prefix(usize),
    /// 长度、闭合引号
    Quote(usize, char),
}

fn first_opener(rules: &Rules, body: &str, done: bool, state: HeadState) -> Opener {
    let wait = |m: Match| !done && m == Match::Partial;
    if rules.tags {
        match match_literals(body, &OPEN_TAGS) {
            Match::Full(n) => return Opener::Tag(n),
            m if wait(m) => return Opener::NeedMore,
            _ => {}
        }
    }
    if state.fence.is_none() && rules.fence != FencePolicy::Off {
        match match_fence_open(body, rules.fence) {
            // 以围栏开头时不可能再是前缀或引号：围栏不剥离就到此为止
            Match::Full(n) => return decide_fence(rules, body, n, done, state),
            m if wait(m) => return Opener::NeedMore,
            _ => {}
        }
    }
    if !state.prefix_used {
        match match_prefixes(body, rules.prefixes, rules.disabled_groups) {
            Match::Full(n) => return Opener::Prefix(n),
            m if wait(m) => return Opener::NeedMore,
            _ => {}
        }
    }
    if rules.quotes && state.quote.is_none() {
        return decide_quote(rules, body, done, state);
    }
    Opener::None
}

/// 一组字面量（ASCII 大小写不敏感）的匹配
fn match_literals(s: &str, literals: &[&str]) -> Match {
    let mut partial = false;
    for lit in literals {
        match match_literal(s, lit) {
            Match::Full(n) => return Match::Full(n),
            Match::Partial => partial = true,
            Match::No => {}
        }
    }
    if partial { Match::Partial } else { Match::No }
}

fn match_literal(s: &str, lit: &str) -> Match {
    let (sb, lb) = (s.as_bytes(), lit.as_bytes());
    if sb.len() >= lb.len() {
        if sb[..lb.len()].eq_ignore_ascii_case(lb) {
            Match::Full(lb.len())
        } else {
            Match::No
        }
    } else if lb[..sb.len()].eq_ignore_ascii_case(sb) {
        Match::Partial
    } else {
        Match::No
    }
}

fn is_colon(c: char) -> bool {
    c == ':' || c == '：'
}

/// 前缀匹配：短语 + 冒号（或整行 + 冒号）；`disabled` 中的组跳过
fn match_prefixes(s: &str, prefixes: &[Prefix], disabled: u8) -> Match {
    let mut partial = false;
    for p in prefixes.iter().filter(|p| p.group & disabled == 0) {
        match match_prefix(s, p) {
            Match::Full(n) => return Match::Full(n),
            Match::Partial => partial = true,
            Match::No => {}
        }
    }
    if partial { Match::Partial } else { Match::No }
}

fn match_prefix(s: &str, p: &Prefix) -> Match {
    let n = match match_literal(s, p.phrase) {
        Match::Full(n) => n,
        other => return other,
    };
    let after = &s[n..];
    let mut chars = after.char_indices();
    if p.rest_of_line {
        for (count, (i, c)) in chars.by_ref().enumerate() {
            if is_colon(c) {
                return Match::Full(n + i + c.len_utf8());
            }
            if c == '\n' || count >= MAX_PREAMBLE_REST {
                return Match::No;
            }
        }
        Match::Partial
    } else {
        for (i, c) in chars {
            if is_colon(c) {
                return Match::Full(n + i + c.len_utf8());
            }
            if c != ' ' && c != '\t' {
                return Match::No;
            }
        }
        Match::Partial
    }
}

/// 开头围栏：```` ```lang ```` 独占一行（之后必须是换行）
fn match_fence_open(s: &str, policy: FencePolicy) -> Match {
    let ticks = s.bytes().take_while(|&b| b == b'`').count();
    if ticks < 3 {
        return if ticks == s.len() && ticks > 0 {
            Match::Partial
        } else {
            Match::No
        };
    }
    let after = &s[ticks..];
    let lang_len = after
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'+' | b'-' | b'.' | b'#'))
        .count();
    if lang_len > MAX_FENCE_LANG {
        return Match::No;
    }
    let lang = &after[..lang_len];
    let rest = &after[lang_len..];
    let spaces = rest.bytes().take_while(|b| matches!(b, b' ' | b'\t' | b'\r')).count();
    match rest.as_bytes().get(spaces) {
        None => Match::Partial,
        Some(b'\n') if fence_lang_allowed(policy, lang) => Match::Full(ticks + lang_len + spaces + 1),
        Some(_) => Match::No,
    }
}

fn fence_lang_allowed(policy: FencePolicy, lang: &str) -> bool {
    match policy {
        FencePolicy::Off => false,
        FencePolicy::AnyLang => true,
        FencePolicy::Langs(langs) => langs.iter().any(|l| l.eq_ignore_ascii_case(lang)),
    }
}

/// 前 `HEAD_LIMIT` 个字符之后的字节位置；文本还不够长时为 None
fn window_limit(s: &str) -> Option<usize> {
    s.char_indices().nth(HEAD_LIMIT).map(|(i, _)| i)
}

/// 开头引号是否剥离。只看引号之后的 24 个字符（流结束时看全部）：
/// - 窗口内出现闭合引号：其后只有空白、闭合标签、（已剥离的）闭合围栏或编辑动作的改动说明 → 剥；还有正文 → 不剥；
/// - 窗口内没有闭合引号且文本超出窗口 → 剥，配对的闭合引号由正文跟踪器去掉；
/// - 流结束时仍没有闭合引号 → 不剥（引号不成对）。
fn decide_quote(rules: &Rules, body: &str, done: bool, state: HeadState) -> Opener {
    let Some(first) = body.chars().next() else {
        return Opener::None;
    };
    let close = match first {
        '"' => '"',
        '\u{201C}' => '\u{201D}',
        _ => return Opener::None,
    };
    let open_len = first.len_utf8();
    let inner = &body[open_len..];
    let limit = window_limit(inner);
    match inner[..limit.unwrap_or(inner.len())].find(close) {
        Some(k) => {
            let ctx = TailCtx {
                tags: rules.tags,
                quote: None,
                fence: state.fence.is_some(),
                edit: rules.edit_tail,
            };
            wrap_decision(
                &inner[k + close.len_utf8()..],
                ctx,
                done,
                Opener::Quote(open_len, close),
            )
        }
        None if limit.is_some() => Opener::Quote(open_len, close),
        None if done => Opener::None,
        None => Opener::NeedMore,
    }
}

/// 开头围栏是否剥离（`open` 为开头围栏行含换行的长度）。只看围栏之后的 24 个字符（流结束时看全部）：
/// - 窗口内开始的行里有配对的闭合围栏：其后只有空白、闭合标签、（已剥离的）闭合引号或编辑动作的改动说明 → 剥；
///   还有正文 → 不剥（代码块 + 说明文字原样保留）；
/// - 窗口内没有闭合围栏且文本超出窗口 → 剥，配对的闭合围栏由正文跟踪器去掉；
/// - 流结束时仍没有闭合围栏（输出被截断）→ 剥。
fn decide_fence(rules: &Rules, body: &str, open: usize, done: bool, state: HeadState) -> Opener {
    let ticks = body.bytes().take_while(|&b| b == b'`').count();
    let strip = Opener::Fence(open, ticks);
    let after = &body[open..];
    match find_fence_closer(after, ticks, state.quote, rules.tags, done) {
        Probe::NeedMore => Opener::NeedMore,
        Probe::Beyond | Probe::Missing => strip,
        Probe::Found(end) => {
            let ctx = TailCtx {
                tags: rules.tags,
                quote: state.quote,
                fence: false,
                edit: rules.edit_tail,
            };
            wrap_decision(&after[end..], ctx, done, strip)
        }
    }
}

/// 在开头围栏之后的窗口里找配对的闭合围栏
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    NeedMore,
    /// 找到了：闭合围栏（不含行尾换行）之后的位置
    Found(usize),
    /// 窗口内开始的行里没有
    Beyond,
    /// 流结束了也没有
    Missing,
}

/// 与正文跟踪器（[`pairing`]）的判断完全一致：内层代码块的闭合行不算
fn find_fence_closer(after: &str, ticks: usize, quote: Option<char>, tags: bool, done: bool) -> Probe {
    let limit = window_limit(after);
    let mut pos = 0usize;
    let mut inner = false;
    loop {
        if limit.is_some_and(|l| pos >= l) {
            return Probe::Beyond;
        }
        if pos == after.len() {
            return if done { Probe::Missing } else { Probe::NeedMore };
        }
        match fence_line(&after[pos..], quote, tags, done) {
            Line::NeedMore => return Probe::NeedMore,
            Line::Bare { ticks: t, core, .. } if t >= ticks => {
                if !inner {
                    return Probe::Found(pos + core);
                }
                inner = false;
            }
            Line::Info { ticks: t, .. } if t >= ticks => inner = true,
            Line::Bare { .. } | Line::Info { .. } | Line::Other => {}
        }
        match after[pos..].find('\n') {
            Some(i) => pos += i + 1,
            // 这一行还没写完：已超出窗口时后面不会再有窗口内开始的行
            None if limit.is_some() => return Probe::Beyond,
            None if done => return Probe::Missing,
            None => return Probe::NeedMore,
        }
    }
}

/// 闭合符号之后允许出现的内容
#[derive(Debug, Clone, Copy)]
struct TailCtx {
    /// 闭合标签
    tags: bool,
    /// 已剥离的开头引号对应的闭合引号
    quote: Option<char>,
    /// 已剥离开头围栏：允许 ≥3 个反引号
    fence: bool,
    /// 编辑动作：允许 `\n---\n改动…`
    edit: bool,
}

/// 闭合符号之后内容的分类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tail {
    /// 还有正文：开头的符号没有包住全文
    Content,
    /// 目前只有合法内容，但最后一项还不完整（例如半个闭合标签）：流在这里结束就不算
    Prefix,
    /// 目前只有合法内容且都完整：流在这里结束就算包住全文
    Complete,
    /// 编辑动作的分隔线 + 改动说明：确定包住了正文
    EditChanges,
}

fn classify_tail(s: &str, ctx: TailCtx) -> Tail {
    let mut rest = s;
    let mut line_start = false;
    loop {
        let Some(c) = rest.chars().next() else {
            return Tail::Complete;
        };
        if c.is_whitespace() {
            line_start = c == '\n';
            rest = &rest[c.len_utf8()..];
            continue;
        }
        let at_line_start = std::mem::take(&mut line_start);
        if ctx.tags && c == '<' {
            match match_literals(rest, &CLOSE_TAGS) {
                Match::Full(n) => rest = &rest[n..],
                Match::Partial => return Tail::Prefix,
                Match::No => return Tail::Content,
            }
        } else if ctx.quote == Some(c) {
            rest = &rest[c.len_utf8()..];
        } else if ctx.fence && c == '`' {
            let n = rest.bytes().take_while(|&b| b == b'`').count();
            if n >= 3 {
                rest = &rest[n..];
            } else {
                return if n == rest.len() { Tail::Prefix } else { Tail::Content };
            }
        } else if ctx.edit && c == '-' && at_line_start {
            return edit_changes(rest);
        } else {
            return Tail::Content;
        }
    }
}

/// 位于行首的 `-` 开始：`split_edit_output` 会把它之后切成改动说明时为 EditChanges
fn edit_changes(s: &str) -> Tail {
    let dashes = s.bytes().take_while(|&b| b == b'-').count();
    if dashes < 3 {
        return if dashes == s.len() { Tail::Prefix } else { Tail::Content };
    }
    if dashes > 3 {
        return Tail::Content;
    }
    let rest = &s[3..];
    let rest = rest.strip_prefix('\r').unwrap_or(rest);
    // 输出以分隔线结尾：`split_edit_output` 同样切掉（改动为空）
    if rest.is_empty() {
        return Tail::Complete;
    }
    let Some(rest) = rest.strip_prefix('\n') else {
        return Tail::Content;
    };
    let label = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '*' || c == '_');
    if label.is_empty() {
        return Tail::Complete;
    }
    match match_literals(label, &["改动", "changes"]) {
        Match::Full(_) => Tail::EditChanges,
        Match::Partial => Tail::Prefix,
        Match::No => Tail::Content,
    }
}

/// 由闭合符号之后的内容决定开头的包裹符号是否剥离。
///
/// 只看闭合符号之后的前 24 个字符：判断只取决于这一段固定的内容（而不是「到目前为止收到了多少」），
/// 切分方式不同时结论才一致；这 24 个字符里仍只有空白 / 闭合标签、文本却还在继续 → 按没有包住全文处理。
fn wrap_decision(following: &str, ctx: TailCtx, done: bool, strip: Opener) -> Opener {
    let limit = following.char_indices().nth(TAIL_LOOK).map(|(i, _)| i);
    match classify_tail(&following[..limit.unwrap_or(following.len())], ctx) {
        Tail::Content => Opener::None,
        Tail::EditChanges => strip,
        Tail::Complete | Tail::Prefix if limit.is_some() => Opener::None,
        Tail::Complete if done => strip,
        Tail::Prefix if done => Opener::None,
        Tail::Complete | Tail::Prefix => Opener::NeedMore,
    }
}

fn ends_with_ignore_ascii_case(s: &str, suffix: &str) -> bool {
    s.len() >= suffix.len() && s.as_bytes()[s.len() - suffix.len()..].eq_ignore_ascii_case(suffix.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holdback_examples() {
        let tags = Closers::of(&CLOSE_TAGS);
        assert_eq!(holdback("你好", &tags), 0);
        assert_eq!(holdback("你好 ", &tags), 1);
        assert_eq!(holdback("你好\n</transl", &tags), "\n</transl".len());
        assert_eq!(holdback("你好\n</TRANSL", &tags), "\n</TRANSL".len());
        assert_eq!(holdback("你好</translation>", &tags), "</translation>".len());
        assert_eq!(holdback("a <b", &tags), 0, "不是闭合标签的前缀");
        assert_eq!(holdback("the cat", &tags), 0);
        assert_eq!(holdback("x\"", &Closers::default()), 0);
        // 空白不计入上限：整段空白都滞留
        assert_eq!(holdback(&format!("x{}", " ".repeat(40)), &Closers::default()), 40);
        assert_eq!(
            holdback("你好\n</translation>\n\n\n", &tags),
            "\n</translation>\n\n\n".len()
        );
        assert_eq!(
            holdback("你好\r\n\r\n</translation>\r\n\r\n", &tags),
            "\r\n\r\n</translation>\r\n\r\n".len()
        );
        assert_eq!(holdback("你好\n\n\n</transl", &tags), "\n\n\n</transl".len());
        // 非空白字符超过 16 个：两个完整的闭合标签只滞留后一个（连同其前的空白）
        assert_eq!(
            holdback("x</translation>\n</source_text>", &tags),
            "\n</source_text>".len()
        );
        // 不完整的闭合序列之后不能再有空白
        assert_eq!(holdback("x</tra  ", &tags), 2);
        assert_eq!(holdback("", &tags), 0);
        assert!(tags.may_contain('T') && tags.may_contain('t') && tags.may_start('<') && !tags.may_start('t'));
        assert!(!tags.may_contain('”'));
    }

    #[test]
    fn closing_prefix_grammar() {
        let c = &["</translation>", "</source_text>"];
        assert!(is_closing_prefix("", c));
        assert!(is_closing_prefix(" \n", c));
        assert!(is_closing_prefix("</TRANSLATION>\n</sou", c));
        assert!(!is_closing_prefix("</b>", c));
    }

    #[test]
    fn prefix_matching() {
        let p = Prefix::colon(GROUP_TRANSLATION, "translation");
        assert_eq!(match_prefix("Translation: hi", &p), Match::Full(12));
        assert_eq!(
            match_prefix("TRANSLATION ：hi", &p),
            Match::Full("TRANSLATION ：".len())
        );
        assert_eq!(match_prefix("Transla", &p), Match::Partial);
        assert_eq!(match_prefix("Translation", &p), Match::Partial);
        assert_eq!(match_prefix("Translation is", &p), Match::No);
        assert_eq!(match_prefix("Translations:", &p), Match::No);
        let p = Prefix::line(GROUP_TRANSLATION, "here is the translation");
        assert_eq!(
            match_prefix("Here is the translation into Chinese:\nx", &p),
            Match::Full(37)
        );
        assert_eq!(match_prefix("Here is the translation of the deal.\n", &p), Match::No);
        assert_eq!(match_prefix("Here is the translation of", &p), Match::Partial);
        let p = Prefix::colon(GROUP_TRANSLATION, "译文");
        assert_eq!(match_prefix("译文：你好", &p), Match::Full("译文：".len()));
        assert_eq!(match_prefix("译", &p), Match::Partial);
        assert_eq!(match_prefix("译者", &p), Match::No);
        // 关闭的组不参与匹配
        assert_eq!(match_prefixes("译文：你好", &[p], GROUP_TRANSLATION), Match::No);
        assert_eq!(
            match_prefixes("译文：你好", &[p], GROUP_EDIT | GROUP_OCR),
            Match::Full("译文：".len())
        );
    }

    #[test]
    fn fence_matching() {
        assert_eq!(match_fence_open("```\nx", FencePolicy::AnyLang), Match::Full(4));
        assert_eq!(
            match_fence_open("```markdown \r\nx", FencePolicy::AnyLang),
            Match::Full(14)
        );
        assert_eq!(match_fence_open("``", FencePolicy::AnyLang), Match::Partial);
        assert_eq!(match_fence_open("```py", FencePolicy::AnyLang), Match::Partial);
        assert_eq!(match_fence_open("```js x\n", FencePolicy::AnyLang), Match::No);
        assert_eq!(match_fence_open("`code`", FencePolicy::AnyLang), Match::No);
        assert_eq!(match_fence_open("```python\n", FencePolicy::Langs(&["md"])), Match::No);
        assert_eq!(match_fence_open("```MD\n", FencePolicy::Langs(&["md"])), Match::Full(6));
    }

    #[test]
    fn fence_closer_probe() {
        // 窗口内找到闭合围栏，返回其后的位置
        assert_eq!(
            find_fence_closer("npm i\n```\nthen", 3, None, true, false),
            Probe::Found(9)
        );
        // 内层代码块的闭合行不算
        assert_eq!(
            find_fence_closer("```js\nx\n```\n```", 3, None, true, true),
            Probe::Found(15)
        );
        // 第一行就超出窗口
        let long = format!("{}\n```", "x".repeat(30));
        assert_eq!(find_fence_closer(&long, 3, None, true, false), Probe::Beyond);
        assert_eq!(find_fence_closer("short", 3, None, true, false), Probe::NeedMore);
        assert_eq!(find_fence_closer("short", 3, None, true, true), Probe::Missing);
        assert_eq!(find_fence_closer("x\n``", 3, None, true, false), Probe::NeedMore);
    }

    #[test]
    fn tail_classification() {
        let ctx = TailCtx {
            tags: true,
            quote: None,
            fence: false,
            edit: false,
        };
        assert_eq!(classify_tail("", ctx), Tail::Complete);
        assert_eq!(classify_tail(" \n</translation>\n", ctx), Tail::Complete);
        assert_eq!(classify_tail("</transl", ctx), Tail::Prefix);
        assert_eq!(classify_tail(" and more", ctx), Tail::Content);
        assert_eq!(classify_tail("\n```", ctx), Tail::Content);
        let fenced = TailCtx { fence: true, ..ctx };
        assert_eq!(classify_tail("\n```", fenced), Tail::Complete);
        assert_eq!(classify_tail("\n``", fenced), Tail::Prefix);
        assert_eq!(classify_tail("\n```js", fenced), Tail::Content);
        let edit = TailCtx { edit: true, ..ctx };
        assert_eq!(classify_tail("\n\n---\n改动：无", edit), Tail::EditChanges);
        assert_eq!(classify_tail("\n---\r\n**Changes**: x", edit), Tail::EditChanges);
        assert_eq!(classify_tail("\n--", edit), Tail::Prefix);
        assert_eq!(classify_tail("\n---", edit), Tail::Complete);
        assert_eq!(classify_tail("\n---\n改", edit), Tail::Prefix);
        assert_eq!(classify_tail("\n---\nmore text", edit), Tail::Content);
        assert_eq!(classify_tail("\n----\n改动：无", edit), Tail::Content);
        assert_eq!(
            classify_tail("---\n改动：无", edit),
            Tail::Content,
            "分隔线必须位于行首"
        );
        assert_eq!(
            classify_tail("\n---\n改动：无", ctx),
            Tail::Content,
            "只有编辑动作认分隔线"
        );
    }
}
