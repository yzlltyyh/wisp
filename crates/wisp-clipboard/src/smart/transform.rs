//! 变换注册表：「粘贴为…」的每一项是一个 [`Transform`]，登记在 [`TRANSFORMS`] 里。加一种变换 = 在 `TransformId`
//! 里加一个值、写一个实现并登记（单测保证每个 id 恰好有一个实现）。
//!
//! 变换在 Rust 里做：完整正文只有 Rust 有（列表只带预览，长文本可达 1 MB），结果直接交给统一的粘贴管线，
//! 不经过 WebView，也不进历史。变换结果不写日志。

use std::sync::LazyLock;

use regex::Regex;
use wisp_text::script::is_east_asian;
use wisp_types::{ClipKind, TransformId};

use super::recognize::Url;

/// 变换失败（不含内容）
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TransformError {
    /// 这条内容不适用这个变换（例如对不是 JSON 的文本做 JSON 格式化）
    #[error("transform does not apply to this content")]
    NotApplicable,
    /// JSON 嵌套过深
    #[error("json nesting too deep")]
    TooDeep,
}

/// 一种文本变换
pub trait Transform: Sync {
    fn id(&self) -> TransformId;
    /// 是否出现在这条内容的「粘贴为…」里：只有会真的改变内容、且对这种类型有意义时才出现
    fn applies(&self, kind: ClipKind, text: &str) -> bool;
    fn apply(&self, text: &str) -> Result<String, TransformError>;
}

/// 全部变换（与 `TransformId::ALL` 一一对应，顺序即菜单顺序）
pub static TRANSFORMS: &[&dyn Transform] = &[
    &JoinLines,
    &RemoveBlankLines,
    &Trim,
    &Upper,
    &Lower,
    &Title,
    &HalfWidth,
    &FullWidth,
    &JsonPretty,
    &JsonMinify,
    &StripTracking,
];

/// 适用于这条内容的变换（菜单与 ⌘K 子页的数据源）
pub fn applicable(kind: ClipKind, text: &str) -> Vec<TransformId> {
    if !is_textual(kind) {
        return Vec::new();
    }
    TRANSFORMS
        .iter()
        .filter(|t| t.applies(kind, text))
        .map(|t| t.id())
        .collect()
}

/// 按 id 执行变换；不适用时返回 [`TransformError::NotApplicable`]（调用方据此提示，不粘贴原文冒充结果）
pub fn apply(id: TransformId, kind: ClipKind, text: &str) -> Result<String, TransformError> {
    let t = TRANSFORMS
        .iter()
        .find(|t| t.id() == id)
        .ok_or(TransformError::NotApplicable)?;
    if !is_textual(kind) || !t.applies(kind, text) {
        return Err(TransformError::NotApplicable);
    }
    t.apply(text)
}

/// 有正文可变换的类型（图片与文件没有）
fn is_textual(kind: ClipKind) -> bool {
    !matches!(kind, ClipKind::Image | ClipKind::Files)
}

/// 大小写与全角只对文字内容有意义：链接改大小写可能打不开，代码改大小写会改掉标识符，颜色值没有必要
fn is_prose(kind: ClipKind) -> bool {
    matches!(kind, ClipKind::Text | ClipKind::RichText)
}

// ---------------------------------------------------------------------------
// 行与空白

struct JoinLines;

impl Transform for JoinLines {
    fn id(&self) -> TransformId {
        TransformId::JoinLines
    }
    fn applies(&self, _: ClipKind, text: &str) -> bool {
        text.trim().contains('\n')
    }
    /// 每行去掉首尾空白、跳过空行后合并，跨行处的规则与 OCR 的段落拼装相同（[`wisp_text::lines::join_all`]）：
    /// 中日文断行直接相连、韩文与西文补空格、行尾断词还原、中英之间按全文惯例
    fn apply(&self, text: &str) -> Result<String, TransformError> {
        Ok(wisp_text::lines::join_all(text))
    }
}

struct RemoveBlankLines;

impl Transform for RemoveBlankLines {
    fn id(&self) -> TransformId {
        TransformId::RemoveBlankLines
    }
    /// 文中有空行时（开头与结尾的空行也算）；判断不分配，遇到第一个空行就停
    fn applies(&self, _: ClipKind, text: &str) -> bool {
        wisp_text::lines::has_blank_lines(text)
    }
    /// 删掉只含空白的行，其余各行连同换行符原样保留（[`wisp_text::lines::remove_blank_lines`]）
    fn apply(&self, text: &str) -> Result<String, TransformError> {
        Ok(wisp_text::lines::remove_blank_lines(text))
    }
}

struct Trim;

impl Transform for Trim {
    fn id(&self) -> TransformId {
        TransformId::Trim
    }
    fn applies(&self, _: ClipKind, text: &str) -> bool {
        needs_trim(text)
    }
    fn apply(&self, text: &str) -> Result<String, TransformError> {
        Ok(trim_lines(text))
    }
}

/// 与 `trim_lines(text) != text` 等价、但不分配也不必扫完全文：有行带首尾空白（含 `\r`），或开头 / 结尾是空行。
/// 检视时每移动一次光标都可能判断一次，长文本不应每次都复制一份
fn needs_trim(text: &str) -> bool {
    text.starts_with('\n') || text.ends_with('\n') || text.split('\n').any(|l| l.trim() != l)
}

/// 每行去掉首尾空白（含全角空格），去掉开头与结尾的空行；行之间的空行保留
fn trim_lines(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(|l| l.trim_matches(|c: char| c.is_whitespace()))
        .collect();
    let first = lines.iter().position(|l| !l.is_empty()).unwrap_or(lines.len());
    let last = lines.iter().rposition(|l| !l.is_empty()).map_or(first, |i| i + 1);
    lines[first..last].join("\n")
}

// ---------------------------------------------------------------------------
// 大小写

struct Upper;

impl Transform for Upper {
    fn id(&self) -> TransformId {
        TransformId::Upper
    }
    fn applies(&self, kind: ClipKind, text: &str) -> bool {
        is_prose(kind) && text.chars().any(char::is_lowercase)
    }
    fn apply(&self, text: &str) -> Result<String, TransformError> {
        Ok(text.to_uppercase())
    }
}

struct Lower;

impl Transform for Lower {
    fn id(&self) -> TransformId {
        TransformId::Lower
    }
    fn applies(&self, kind: ClipKind, text: &str) -> bool {
        is_prose(kind) && text.chars().any(char::is_uppercase)
    }
    fn apply(&self, text: &str) -> Result<String, TransformError> {
        Ok(text.to_lowercase())
    }
}

struct Title;

impl Transform for Title {
    fn id(&self) -> TransformId {
        TransformId::Title
    }
    fn applies(&self, kind: ClipKind, text: &str) -> bool {
        is_prose(kind) && title_differs(text)
    }
    fn apply(&self, text: &str) -> Result<String, TransformError> {
        Ok(title_case(text))
    }
}

/// 与 `title_case(text) != text` 等价，遇到第一处不同就停（不分配）
fn title_differs(text: &str) -> bool {
    let mut in_word = false;
    for c in text.chars() {
        if c.is_alphanumeric() {
            let same = if in_word {
                c.to_lowercase().eq(std::iter::once(c))
            } else {
                c.to_uppercase().eq(std::iter::once(c))
            };
            if !same {
                return true;
            }
            in_word = true;
        } else {
            in_word = in_word && matches!(c, '\'' | '’');
        }
    }
    false
}

/// 每个单词首字母大写、其余小写；撇号不断词（「don't」→「Don't」）
fn title_case(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_word = false;
    for c in text.chars() {
        if c.is_alphanumeric() {
            if in_word {
                out.extend(c.to_lowercase());
            } else {
                out.extend(c.to_uppercase());
            }
            in_word = true;
        } else {
            in_word = in_word && matches!(c, '\'' | '’');
            out.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 全角 / 半角

struct HalfWidth;

impl Transform for HalfWidth {
    fn id(&self) -> TransformId {
        TransformId::HalfWidth
    }
    fn applies(&self, _: ClipKind, text: &str) -> bool {
        text.chars().any(|c| to_half(c) != c)
    }
    fn apply(&self, text: &str) -> Result<String, TransformError> {
        Ok(text.chars().map(to_half).collect())
    }
}

struct FullWidth;

impl Transform for FullWidth {
    fn id(&self) -> TransformId {
        TransformId::FullWidth
    }
    /// 只对中日韩文字里夹着半角字母数字的内容出现：纯英文几乎不会需要全角，每条都列出来只会让菜单变长
    fn applies(&self, kind: ClipKind, text: &str) -> bool {
        is_prose(kind) && text.chars().any(is_east_asian) && text.chars().any(|c| to_full(c) != c)
    }
    fn apply(&self, text: &str) -> Result<String, TransformError> {
        Ok(text.chars().map(to_full).collect())
    }
}

/// 全角 ASCII（U+FF01–FF5E）与全角空格 → 半角
fn to_half(c: char) -> char {
    match c {
        '\u{3000}' => ' ',
        '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
        _ => c,
    }
}

/// 可见 ASCII（0x21–0x7E）与空格 → 全角
fn to_full(c: char) -> char {
    match c {
        ' ' => '\u{3000}',
        '!'..='~' => char::from_u32(c as u32 + 0xFEE0).unwrap_or(c),
        _ => c,
    }
}

// ---------------------------------------------------------------------------
// JSON（按 token 重排，不经过 serde_json::Value：没有开 preserve_order 时 Map 会按键排序，打乱原来的顺序）

struct JsonPretty;

impl Transform for JsonPretty {
    fn id(&self) -> TransformId {
        TransformId::JsonPretty
    }
    fn applies(&self, kind: ClipKind, text: &str) -> bool {
        json_candidate(kind, text) && json_would_change(text, true)
    }
    fn apply(&self, text: &str) -> Result<String, TransformError> {
        reformat_json(text, true)
    }
}

struct JsonMinify;

impl Transform for JsonMinify {
    fn id(&self) -> TransformId {
        TransformId::JsonMinify
    }
    fn applies(&self, kind: ClipKind, text: &str) -> bool {
        json_candidate(kind, text) && json_would_change(text, false)
    }
    fn apply(&self, text: &str) -> Result<String, TransformError> {
        reformat_json(text, false)
    }
}

/// 值得尝试解析的 JSON：对象或数组（单个字符串 / 数字格式化没有意义）
fn json_candidate(kind: ClipKind, text: &str) -> bool {
    matches!(kind, ClipKind::Text | ClipKind::RichText | ClipKind::Code)
        && matches!(text.trim_start().as_bytes().first(), Some(b'{' | b'['))
}

/// 最大嵌套深度（防止病态输入）
const JSON_MAX_DEPTH: usize = 512;

/// JSON 重排的输出端：执行变换时写进字符串；判断是否适用时只与原文逐段比较，不必为长 JSON 再分配一份
trait JsonOut {
    fn put(&mut self, s: &str);
}

impl JsonOut for String {
    fn put(&mut self, s: &str) {
        self.push_str(s);
    }
}

/// 与原文比较：`same` 表示到目前为止的输出与原文的前缀一致
struct SameAs<'a> {
    expected: &'a [u8],
    pos: usize,
    same: bool,
}

impl JsonOut for SameAs<'_> {
    fn put(&mut self, s: &str) {
        if self.same {
            let end = self.pos + s.len();
            self.same = self.expected.get(self.pos..end) == Some(s.as_bytes());
            self.pos = end;
        }
    }
}

/// 是合法 JSON，且重排（`pretty` 格式化 / 压缩）后与原文不同
fn json_would_change(text: &str, pretty: bool) -> bool {
    let mut cmp = SameAs {
        expected: text.as_bytes(),
        pos: 0,
        same: true,
    };
    reformat_json_into(text, pretty, &mut cmp).is_ok() && !(cmp.same && cmp.pos == text.len())
}

/// 校验并重排 JSON：`pretty` 时两空格缩进、`": "` 分隔，空的 `{}` / `[]` 保持紧凑；否则压缩为一行。
/// 字符串与数字原样保留（不改转义、不改精度），键的顺序不变
fn reformat_json(text: &str, pretty: bool) -> Result<String, TransformError> {
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    reformat_json_into(text, pretty, &mut out)?;
    Ok(out)
}

fn reformat_json_into(text: &str, pretty: bool, out: &mut impl JsonOut) -> Result<(), TransformError> {
    let src = text.trim();
    let mut lx = Lexer {
        b: src.as_bytes(),
        i: 0,
    };
    // 每层是否是对象
    let mut stack: Vec<bool> = Vec::new();
    let newline = |out: &mut dyn JsonOut, depth: usize| {
        if pretty {
            out.put("\n");
            for _ in 0..depth {
                out.put("  ");
            }
        }
    };
    let mut expect_value = true;
    loop {
        let tok = lx.next()?.ok_or(TransformError::NotApplicable)?;
        if expect_value {
            match tok {
                Tok::Open(obj) => {
                    if stack.len() >= JSON_MAX_DEPTH {
                        return Err(TransformError::TooDeep);
                    }
                    if lx.peek_close() == Some(obj) {
                        lx.next()?;
                        out.put(if obj { "{}" } else { "[]" });
                    } else {
                        out.put(if obj { "{" } else { "[" });
                        stack.push(obj);
                        newline(out, stack.len());
                        if obj {
                            write_key(out, &mut lx, src, pretty)?;
                        }
                        // 仍然等一个值
                        continue;
                    }
                }
                Tok::Scalar(s, e) => out.put(&src[s..e]),
                _ => return Err(TransformError::NotApplicable),
            }
        } else {
            match (tok, stack.last().copied()) {
                (Tok::Comma, Some(obj)) => {
                    out.put(",");
                    newline(out, stack.len());
                    if obj {
                        write_key(out, &mut lx, src, pretty)?;
                    }
                    expect_value = true;
                    continue;
                }
                (Tok::Close(close), Some(obj)) if close == obj => {
                    stack.pop();
                    newline(out, stack.len());
                    out.put(if obj { "}" } else { "]" });
                }
                _ => return Err(TransformError::NotApplicable),
            }
        }
        // 刚写完一个完整的值：顶层值之后不能再有内容
        expect_value = false;
        if stack.is_empty() {
            return if lx.next()?.is_none() {
                Ok(())
            } else {
                Err(TransformError::NotApplicable)
            };
        }
    }
}

/// 对象成员的 `"key":`（键必须是字符串）
fn write_key(out: &mut dyn JsonOut, lx: &mut Lexer<'_>, src: &str, pretty: bool) -> Result<(), TransformError> {
    let Some(Tok::Scalar(s, e)) = lx.next()? else {
        return Err(TransformError::NotApplicable);
    };
    if src.as_bytes()[s] != b'"' || lx.next()? != Some(Tok::Colon) {
        return Err(TransformError::NotApplicable);
    }
    out.put(&src[s..e]);
    out.put(if pretty { ": " } else { ":" });
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    /// true = 对象
    Open(bool),
    Close(bool),
    Colon,
    Comma,
    /// 字符串 / 数字 / 字面量在源文本里的字节区间
    Scalar(usize, usize),
}

struct Lexer<'a> {
    b: &'a [u8],
    i: usize,
}

impl Lexer<'_> {
    fn skip_ws(&mut self) {
        while self
            .b
            .get(self.i)
            .is_some_and(|c| matches!(c, b' ' | b'\t' | b'\n' | b'\r'))
        {
            self.i += 1;
        }
    }

    /// 下一个 token 是否是右括号（是对象的为 true）
    fn peek_close(&mut self) -> Option<bool> {
        self.skip_ws();
        match self.b.get(self.i) {
            Some(b'}') => Some(true),
            Some(b']') => Some(false),
            _ => None,
        }
    }

    fn next(&mut self) -> Result<Option<Tok>, TransformError> {
        self.skip_ws();
        let Some(&c) = self.b.get(self.i) else { return Ok(None) };
        let start = self.i;
        self.i += 1;
        let tok = match c {
            b'{' => Tok::Open(true),
            b'[' => Tok::Open(false),
            b'}' => Tok::Close(true),
            b']' => Tok::Close(false),
            b':' => Tok::Colon,
            b',' => Tok::Comma,
            b'"' => {
                self.string()?;
                Tok::Scalar(start, self.i)
            }
            b'-' | b'0'..=b'9' => {
                self.i = start;
                self.number()?;
                Tok::Scalar(start, self.i)
            }
            b't' | b'f' | b'n' => {
                let lit: &[u8] = match c {
                    b't' => b"true",
                    b'f' => b"false",
                    _ => b"null",
                };
                if !self.b[start..].starts_with(lit) {
                    return Err(TransformError::NotApplicable);
                }
                self.i = start + lit.len();
                Tok::Scalar(start, self.i)
            }
            _ => return Err(TransformError::NotApplicable),
        };
        Ok(Some(tok))
    }

    /// 字符串（开头的引号已读）：校验转义，不允许未转义的控制字符
    fn string(&mut self) -> Result<(), TransformError> {
        loop {
            let c = *self.b.get(self.i).ok_or(TransformError::NotApplicable)?;
            self.i += 1;
            match c {
                b'"' => return Ok(()),
                b'\\' => {
                    let e = *self.b.get(self.i).ok_or(TransformError::NotApplicable)?;
                    self.i += 1;
                    match e {
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {}
                        b'u' => {
                            let hex = self.b.get(self.i..self.i + 4).ok_or(TransformError::NotApplicable)?;
                            if !hex.iter().all(u8::is_ascii_hexdigit) {
                                return Err(TransformError::NotApplicable);
                            }
                            self.i += 4;
                        }
                        _ => return Err(TransformError::NotApplicable),
                    }
                }
                0x00..=0x1f => return Err(TransformError::NotApplicable),
                _ => {}
            }
        }
    }

    /// 数字：`-? (0 | [1-9][0-9]*) (. [0-9]+)? ([eE] [+-]? [0-9]+)?`
    fn number(&mut self) -> Result<(), TransformError> {
        let digits = |l: &mut Self| {
            let s = l.i;
            while l.b.get(l.i).is_some_and(u8::is_ascii_digit) {
                l.i += 1;
            }
            l.i - s
        };
        if self.b.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        match self.b.get(self.i) {
            Some(b'0') => self.i += 1,
            Some(b'1'..=b'9') => {
                digits(self);
            }
            _ => return Err(TransformError::NotApplicable),
        }
        if self.b.get(self.i) == Some(&b'.') {
            self.i += 1;
            if digits(self) == 0 {
                return Err(TransformError::NotApplicable);
            }
        }
        if matches!(self.b.get(self.i), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if digits(self) == 0 {
                return Err(TransformError::NotApplicable);
            }
        }
        // 数字后面紧跟字母等（「01」「1x」）不是合法 JSON
        if self
            .b
            .get(self.i)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'.')
        {
            return Err(TransformError::NotApplicable);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 链接

struct StripTracking;

impl Transform for StripTracking {
    fn id(&self) -> TransformId {
        TransformId::StripTracking
    }
    fn applies(&self, _: ClipKind, text: &str) -> bool {
        Url::iter(text).any(|(s, e)| strip_url(&text[s..e]).is_some())
    }
    fn apply(&self, text: &str) -> Result<String, TransformError> {
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        for (s, e) in Url::iter(text) {
            out.push_str(&text[at..s]);
            match strip_url(&text[s..e]) {
                Some(clean) => out.push_str(&clean),
                None => out.push_str(&text[s..e]),
            }
            at = e;
        }
        out.push_str(&text[at..]);
        Ok(out)
    }
}

/// 去掉一个链接里的跟踪参数；没有可去的返回 None
fn strip_url(url: &str) -> Option<String> {
    // 参数名：utm_* 系列，以及各广告 / 社交平台的点击标识
    static TRACKING: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            r"(?i)^(?:utm_[a-z_]+|fbclid|gclid|gclsrc|dclid|gbraid|wbraid|msclkid|yclid|twclid|ttclid|li_fat_id",
            r"|igshid|igsh|mc_cid|mc_eid|_hsenc|_hsmi|mkt_tok|vero_id|spm|scm|spm_id_from|vd_source|share_source",
            r"|share_medium|share_from|share_plat|share_session_id|xsec_source|from_spmid)$",
        ))
        .expect("静态正则")
    });
    let (head, fragment) = match url.split_once('#') {
        Some((h, f)) => (h, Some(f)),
        None => (url, None),
    };
    let (base, query) = head.split_once('?')?;
    let params: Vec<&str> = query.split('&').filter(|p| !p.is_empty()).collect();
    let kept: Vec<&str> = params
        .iter()
        .copied()
        .filter(|p| !TRACKING.is_match(p.split('=').next().unwrap_or_default()))
        .collect();
    if kept.len() == params.len() {
        return None;
    }
    let mut out = base.to_owned();
    if !kept.is_empty() {
        out.push('?');
        out.push_str(&kept.join("&"));
    }
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(f);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn run(id: TransformId, text: &str) -> String {
        apply(id, ClipKind::Text, text).unwrap_or_else(|e| panic!("{id:?} 应适用于 {text:?}：{e}"))
    }

    fn not_applicable(id: TransformId, kind: ClipKind, text: &str) {
        assert_eq!(
            apply(id, kind, text),
            Err(TransformError::NotApplicable),
            "{id:?} 不应适用于 {text:?}"
        );
        assert!(!applicable(kind, text).contains(&id));
    }

    #[test]
    fn registry_covers_every_id_once_in_menu_order() {
        let ids: Vec<TransformId> = TRANSFORMS.iter().map(|t| t.id()).collect();
        assert_eq!(ids, TransformId::ALL, "顺序与 TransformId::ALL 一致");
        assert_eq!(ids.iter().collect::<HashSet<_>>().len(), ids.len());
    }

    #[test]
    fn images_and_files_have_no_transforms() {
        assert!(applicable(ClipKind::Image, "").is_empty());
        assert!(applicable(ClipKind::Files, "/a\n/b").is_empty());
        not_applicable(TransformId::JoinLines, ClipKind::Files, "/a\n/b");
    }

    #[test]
    fn join_lines() {
        for (input, want) in [
            ("first line\n  second line  \n\nthird", "first line second line third"),
            // 中日文断行直接相连
            ("这是一段被\n硬换行的中文", "这是一段被硬换行的中文"),
            ("クリップ\nボードにコピー", "クリップボードにコピー"),
            // 韩文以空格分词，断行处补空格
            ("클립보드에\n복사합니다.", "클립보드에 복사합니다."),
            // 紧挨中文的破折号、省略号之后不多出空格
            ("他说——\n我们走吧", "他说——我们走吧"),
            ("等等……\n然后呢", "等等……然后呢"),
            ("Wait…\nthen go", "Wait… then go"),
            ("用半角标点写的中文,\n照样直接相连", "用半角标点写的中文,照样直接相连"),
            // 行尾连字符：断词还原，复合词与编号保留连字符且不加空格
            ("an inter-\nface", "an interface"),
            ("COVID-\n19 and Wi-\nFi", "COVID-19 and Wi-Fi"),
            ("from 2020-\n2021", "from 2020-2021"),
            (
                "see https://example.com/long-\npath",
                "see https://example.com/long-path",
            ),
            // 中英之间断行：全文没有间隔空格时直接相连，有就补上
            (
                "这是一段被\n硬换行的中文\nwith English",
                "这是一段被硬换行的中文with English",
            ),
            ("使用 Wisp 时会\nCPU 占用很低", "使用 Wisp 时会 CPU 占用很低"),
            // 目录列表、网址列表：以斜杠结尾的行各自分开，不粘成一个路径或链接
            ("src/\ntests/\ndocs/", "src/ tests/ docs/"),
            ("https://a.com/\nhttps://b.com/", "https://a.com/ https://b.com/"),
            ("Visit:\nhttps://a.com/\nNext", "Visit: https://a.com/ Next"),
        ] {
            assert_eq!(run(TransformId::JoinLines, input), want, "{input:?}");
            assert_ne!(want, input, "适用时结果与原文不同");
        }
        not_applicable(TransformId::JoinLines, ClipKind::Text, "single line");
        not_applicable(TransformId::JoinLines, ClipKind::Text, "  trailing newline\n");
    }

    #[test]
    fn remove_blank_lines() {
        assert_eq!(
            run(
                TransformId::RemoveBlankLines,
                "第一段。\n\n第二段。\n \u{3000}\n第三段。"
            ),
            "第一段。\n第二段。\n第三段。"
        );
        // 留下的行原样：缩进、行尾空白与 \r\n 都不动（与「去掉首尾空白」分工）
        assert_eq!(
            run(TransformId::RemoveBlankLines, "  a \r\n\r\n\tb\r\n"),
            "  a \r\n\tb\r\n"
        );
        // 代码条目同样提供
        assert_eq!(
            apply(TransformId::RemoveBlankLines, ClipKind::Code, "fn a() {}\n\nfn b() {}"),
            Ok("fn a() {}\nfn b() {}".to_owned())
        );
        // 末尾的一个换行符不是空行
        not_applicable(TransformId::RemoveBlankLines, ClipKind::Text, "a\nb\n");
        not_applicable(TransformId::RemoveBlankLines, ClipKind::Text, "single line");
    }

    #[test]
    fn trim() {
        assert_eq!(run(TransformId::Trim, "\n\n  a  \n\u{3000}b\t\n\n  c\n\n"), "a\nb\n\nc");
        not_applicable(TransformId::Trim, ClipKind::Text, "a\nb");
    }

    #[test]
    fn case() {
        assert_eq!(run(TransformId::Upper, "Straße ok"), "STRASSE OK");
        assert_eq!(run(TransformId::Lower, "Hello WORLD 你好"), "hello world 你好");
        assert_eq!(
            run(TransformId::Title, "hello WORLD, don't stop"),
            "Hello World, Don't Stop"
        );
        not_applicable(TransformId::Upper, ClipKind::Text, "ALREADY 123");
        not_applicable(TransformId::Lower, ClipKind::Text, "已经是中文 abc");
        not_applicable(TransformId::Title, ClipKind::Text, "Already Title");
        // 链接与颜色不改大小写
        not_applicable(TransformId::Lower, ClipKind::Link, "https://Example.com/A");
        not_applicable(TransformId::Upper, ClipKind::Color, "#3478f6");
        not_applicable(TransformId::Upper, ClipKind::Code, "let x = 1;");
    }

    #[test]
    fn width() {
        assert_eq!(run(TransformId::HalfWidth, "ＡＢＣ１２３！\u{3000}ｘ"), "ABC123! x");
        assert_eq!(run(TransformId::FullWidth, "型号 A1 版"), "型号\u{3000}Ａ１\u{3000}版");
        // 往返
        let s = "价格 ¥ 12, OK?";
        let full = run(TransformId::FullWidth, s);
        assert_eq!(run(TransformId::HalfWidth, &full), s);
        not_applicable(TransformId::HalfWidth, ClipKind::Text, "plain ascii");
        // 纯英文不提供全角；中日韩文字里夹着半角字符时提供（含韩文）
        not_applicable(TransformId::FullWidth, ClipKind::Text, "plain ascii");
        assert_eq!(run(TransformId::FullWidth, "모델 A1"), "모델\u{3000}Ａ１");
        assert_eq!(run(TransformId::FullWidth, "かな A"), "かな\u{3000}Ａ");
    }

    #[test]
    fn json_keeps_key_order_and_scalars() {
        let src = r#"{"z":1,"a":[1,2.50,{"k":"v\"\u00e9"}],"empty":{},"list":[],"n":null,"t":true,"e":-1.5e+10}"#;
        let pretty = run(TransformId::JsonPretty, src);
        assert_eq!(
            pretty,
            "{\n  \"z\": 1,\n  \"a\": [\n    1,\n    2.50,\n    {\n      \"k\": \"v\\\"\\u00e9\"\n    }\n  ],\n  \"empty\": {},\n  \"list\": [],\n  \"n\": null,\n  \"t\": true,\n  \"e\": -1.5e+10\n}"
        );
        // 压缩是格式化的逆
        assert_eq!(run(TransformId::JsonMinify, &pretty), src);
        // 已经是对应格式的不重复提供
        not_applicable(TransformId::JsonMinify, ClipKind::Text, src);
        not_applicable(TransformId::JsonPretty, ClipKind::Text, &pretty);
        assert_eq!(run(TransformId::JsonPretty, "[ ]"), "[]");
        // 判断是否适用时不分配，结果与「重排后是否不同」一致（包括首尾空白、输出比原文短或长）
        for text in [
            src,
            pretty.as_str(),
            " {\"a\":1}",
            "{\"a\":1}\n",
            "[]",
            "[ ]",
            "{\"a\": 1}",
            "[1,2]",
        ] {
            for p in [true, false] {
                let differs = reformat_json(text, p).is_ok_and(|out| out != text);
                assert_eq!(json_would_change(text, p), differs, "{text:?} pretty={p}");
            }
        }
    }

    #[test]
    fn json_rejects_invalid_input() {
        for bad in [
            "{",
            "{\"a\" 1}",
            "{\"a\":1,}",
            "[1 2]",
            "{a:1}",
            "[01]",
            "[1.]",
            "[\"\u{1}\"]",
            "[\"\\x\"]",
            "{\"a\":1} trailing",
            "[tru]",
            "plain text",
            "\"just a string\"",
        ] {
            not_applicable(TransformId::JsonPretty, ClipKind::Text, bad);
            not_applicable(TransformId::JsonMinify, ClipKind::Text, bad);
        }
        let deep = format!("{}{}", "[".repeat(JSON_MAX_DEPTH + 1), "]".repeat(JSON_MAX_DEPTH + 1));
        assert_eq!(reformat_json(&deep, true), Err(TransformError::TooDeep));
    }

    #[test]
    fn strip_tracking() {
        assert_eq!(
            run(
                TransformId::StripTracking,
                "看看 https://example.com/p?id=7&utm_source=x&UTM_Medium=y&fbclid=abc#top 这个"
            ),
            "看看 https://example.com/p?id=7#top 这个"
        );
        assert_eq!(
            apply(
                TransformId::StripTracking,
                ClipKind::Link,
                "https://www.bilibili.com/video/BV1x?spm_id_from=333&vd_source=ff"
            ),
            Ok("https://www.bilibili.com/video/BV1x".to_owned())
        );
        not_applicable(
            TransformId::StripTracking,
            ClipKind::Link,
            "https://example.com/search?q=utm_source",
        );
        not_applicable(TransformId::StripTracking, ClipKind::Text, "no links here");
    }

    #[test]
    fn cheap_checks_agree_with_the_transforms() {
        for text in [
            "a\nb",
            " a",
            "a \nb",
            "a\r\nb",
            "\n\na",
            "a\n",
            "a\n  \nb",
            "a\n\nb",
            "\u{3000}全角",
            "",
            "Hello World",
            "hello",
            "Don't Stop",
            "don't",
            "ǅungla",
            "Straße",
            "ÉCOLE normale",
            "已经 Title 123",
        ] {
            assert_eq!(needs_trim(text), trim_lines(text) != text, "trim {text:?}");
            assert_eq!(title_differs(text), title_case(text) != text, "title {text:?}");
        }
    }

    #[test]
    fn applicable_lists_in_menu_order() {
        assert_eq!(
            applicable(ClipKind::Text, " Hello\nWorld"),
            [
                TransformId::JoinLines,
                TransformId::Trim,
                TransformId::Upper,
                TransformId::Lower
            ]
        );
        assert_eq!(
            applicable(ClipKind::Text, "Hello\n\nworld"),
            [
                TransformId::JoinLines,
                TransformId::RemoveBlankLines,
                TransformId::Upper,
                TransformId::Lower,
                TransformId::Title
            ]
        );
        assert_eq!(applicable(ClipKind::Code, "{\"a\":1}"), [TransformId::JsonPretty]);
    }
}
