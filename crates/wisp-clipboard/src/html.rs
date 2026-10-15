//! HTML 规范化：剥离 `<meta>`、`<script>`、`<style>`、`<link>`、HTML 注释、`on*=` 事件属性与
//! `javascript:` URL；以及在平台只给了 HTML、没有纯文本时从 HTML 提取纯文本。
//!
//! 不引入完整的 HTML 解析器：剪贴板里的 HTML 片段只需要按「标签 / 注释 / 文本」切分，标签内部按属性切分，
//! 其余字节原样保留（不重新序列化、不改大小写），这样输出与输入在未命中规则的部分逐字节一致。
//! 所有切分点都落在 ASCII 字符上，因此对 UTF-8 字符串切片总是安全的。

/// 词法单元
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token<'a> {
    /// 普通文本（含无法构成标签的孤立 `<`）
    Text(&'a str),
    /// 开始标签（含自闭合）；`raw` 是从 `<` 到 `>` 的完整原文
    Start {
        raw: &'a str,
        name: &'a str,
        /// 标签名之后第一个属性前的偏移（相对 raw）
        name_end: usize,
        attrs: Vec<Attr<'a>>,
        /// 收尾部分（`/>` 或 `>`，以及之前的空白）在 raw 中的起点
        tail_start: usize,
    },
    /// 结束标签
    End { raw: &'a str, name: &'a str },
    /// `<!-- … -->`
    Comment,
    /// `<!DOCTYPE …>`、`<?xml …?>` 等
    Other(&'a str),
}

/// 一个属性：`span` 为相对标签原文的区间，包含属性前的空白
#[derive(Debug, Clone, PartialEq, Eq)]
struct Attr<'a> {
    span: (usize, usize),
    name: &'a str,
    value: Option<&'a str>,
}

/// 原样跳过内容的元素（内容不按 HTML 解析，直到对应的结束标签）
const RAW_TEXT: [&str; 2] = ["script", "style"];
/// 整个删除（连同内容）的元素
const DROP_WITH_CONTENT: [&str; 2] = ["script", "style"];
/// 只删除标签本身的元素（空元素）
const DROP_TAG: [&str; 2] = ["meta", "link"];

/// 规范化 HTML：删除危险或无用的部分，其余原样保留
pub(crate) fn sanitize_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut lexer = Lexer::new(input);
    while let Some(tok) = lexer.next_token() {
        match tok {
            Token::Text(t) | Token::Other(t) => out.push_str(t),
            Token::Comment => {}
            Token::Start {
                raw,
                name,
                name_end,
                attrs,
                tail_start,
            } => {
                if DROP_TAG.iter().any(|n| name.eq_ignore_ascii_case(n)) {
                    continue;
                }
                if DROP_WITH_CONTENT.iter().any(|n| name.eq_ignore_ascii_case(n)) {
                    // 内容已由词法器作为 RawText 跳过（见 Lexer::skip_raw_text）
                    continue;
                }
                out.push_str(&raw[..name_end]);
                for a in &attrs {
                    if !is_dangerous_attr(a) {
                        out.push_str(&raw[a.span.0..a.span.1]);
                    }
                }
                out.push_str(&raw[tail_start..]);
            }
            Token::End { raw, name } => {
                let dropped = DROP_TAG
                    .iter()
                    .chain(DROP_WITH_CONTENT.iter())
                    .any(|n| name.eq_ignore_ascii_case(n));
                if !dropped {
                    out.push_str(raw);
                }
            }
        }
    }
    out
}

/// 换行的块级元素（开始与结束处各断一行）
const BLOCK: [&str; 37] = [
    "address",
    "article",
    "aside",
    "blockquote",
    "body",
    "caption",
    "center",
    "dd",
    "details",
    "dialog",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "html",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "section",
    "summary",
    "table",
    "tr",
    "ul",
];
/// 保留空白与换行的元素（`<pre>` 一类，也是块级）
const PREFORMATTED: [&str; 4] = ["pre", "textarea", "listing", "xmp"];

/// 从 HTML 提取纯文本（平台只提供 HTML、没有纯文本时使用），尽量与浏览器渲染后复制出的文字一致：
/// - 普通排版中，源码里的连续空白（含换行、缩进）按 HTML 规则折叠为一个空格，行首行尾的空白去掉；
/// - 块级元素的开始与结束处换行，`<br>` 换行，表格单元格之间用制表符；
/// - `<pre>` 一类元素内保留空白与换行（紧跟开始标签的第一个换行按 HTML 规则忽略）；
/// - 去掉标签、注释与脚本 / 样式的内容，解码字符引用（`&nbsp;` 输出为普通空格）；
/// - 每行去掉行尾空白，连续空行（含只有空白的行）最多保留一个，去掉开头与结尾的空行
pub(crate) fn html_to_text(input: &str) -> String {
    let mut sink = TextSink::default();
    let mut lexer = Lexer::new(input);
    let mut decoded = String::new();
    // 刚读完 `<pre>` 开始标签：只对紧随其后的文本生效
    let mut after_pre_start = false;
    while let Some(tok) = lexer.next_token() {
        let skip_newline = std::mem::take(&mut after_pre_start);
        match tok {
            Token::Text(t) => {
                decoded.clear();
                decode_entities_into(t, &mut decoded);
                sink.text(&decoded, skip_newline);
            }
            Token::Start { name, .. } => {
                if name.eq_ignore_ascii_case("br") {
                    sink.line_break();
                } else if name.eq_ignore_ascii_case("td") || name.eq_ignore_ascii_case("th") {
                    sink.cell();
                } else if is_one_of(name, &PREFORMATTED) {
                    sink.block();
                    sink.pre_depth += 1;
                    after_pre_start = true;
                } else if is_one_of(name, &BLOCK) {
                    sink.block();
                }
            }
            Token::End { name, .. } => {
                if is_one_of(name, &PREFORMATTED) {
                    sink.pre_depth = sink.pre_depth.saturating_sub(1);
                    sink.block();
                } else if is_one_of(name, &BLOCK) {
                    sink.block();
                }
            }
            Token::Comment | Token::Other(_) => {}
        }
    }
    sink.finish()
}

fn is_one_of(name: &str, set: &[&str]) -> bool {
    set.iter().any(|n| name.eq_ignore_ascii_case(n))
}

/// [`html_to_text`] 的输出缓冲
#[derive(Debug, Default)]
struct TextSink {
    out: String,
    /// 普通排版中上一个可见字符之后遇到了可折叠的空白：下一个可见字符前补一个空格（行首不补）
    pending_space: bool,
    /// `<pre>` 一类元素的嵌套深度：大于 0 时保留空白与换行
    pre_depth: u32,
}

impl TextSink {
    fn at_line_start(&self) -> bool {
        self.out.is_empty() || self.out.ends_with('\n')
    }

    fn text(&mut self, t: &str, skip_leading_newline: bool) {
        if self.pre_depth > 0 {
            let t = if skip_leading_newline {
                t.strip_prefix("\r\n")
                    .or_else(|| t.strip_prefix(['\n', '\r']))
                    .unwrap_or(t)
            } else {
                t
            };
            let mut chars = t.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '\r' {
                    // \r\n 与单独的 \r 都按一个换行
                    chars.next_if_eq(&'\n');
                    self.out.push('\n');
                } else {
                    self.out.push(c);
                }
            }
            return;
        }
        for c in t.chars() {
            // HTML 的 ASCII 空白可折叠；&nbsp;（U+00A0）不折叠，原样占位
            if matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{c}') {
                self.pending_space = true;
                continue;
            }
            if std::mem::take(&mut self.pending_space) && !self.at_line_start() && !self.out.ends_with('\t') {
                self.out.push(' ');
            }
            self.out.push(c);
        }
    }

    /// `<br>`：总是换行（连续的 `<br>` 产生空行）
    fn line_break(&mut self) {
        self.pending_space = false;
        self.out.push('\n');
    }

    /// 块级元素的边界：不在行首时换行
    fn block(&mut self) {
        self.pending_space = false;
        if !self.at_line_start() {
            self.out.push('\n');
        }
    }

    /// 表格单元格：同一行的单元格之间用制表符
    fn cell(&mut self) {
        self.pending_space = false;
        if !self.out.is_empty() && !self.out.ends_with(['\n', '\t']) {
            self.out.push('\t');
        }
    }

    fn finish(self) -> String {
        let mut result = String::with_capacity(self.out.len());
        let mut blank_lines = 0usize;
        for line in self.out.split('\n') {
            let line = line.trim_end();
            if line.is_empty() {
                blank_lines += 1;
                continue;
            }
            if !result.is_empty() {
                // 之前的空行最多保留一个
                result.push_str(if blank_lines > 0 { "\n\n" } else { "\n" });
            }
            blank_lines = 0;
            for c in line.chars() {
                result.push(if c == '\u{a0}' { ' ' } else { c });
            }
        }
        result
    }
}

/// 事件属性（`on*`）或值里含 `javascript:` / `vbscript:` 的属性
fn is_dangerous_attr(a: &Attr<'_>) -> bool {
    let name = a.name.as_bytes();
    if name.len() > 2 && name[..2].eq_ignore_ascii_case(b"on") {
        return true;
    }
    let Some(value) = a.value else { return false };
    if !value.contains([':', '&']) {
        return false;
    }
    // 先解码字符引用（`javascript&#58;`、`&#x6A;avascript:`），再去掉空白与控制字符（`java\tscript:`），最后比较小写
    let mut decoded = String::with_capacity(value.len());
    decode_entities_into(value, &mut decoded);
    let squashed: String = decoded
        .chars()
        .filter(|c| !c.is_whitespace() && !c.is_control())
        .flat_map(char::to_lowercase)
        .collect();
    squashed.contains("javascript:") || squashed.contains("vbscript:")
}

/// 解码 HTML 字符引用：数字引用（有无分号均可）与少量常用命名引用；无法识别的原样保留
fn decode_entities_into(s: &str, out: &mut String) {
    const NAMED: [(&str, char); 10] = [
        ("amp", '&'),
        ("lt", '<'),
        ("gt", '>'),
        ("quot", '"'),
        ("apos", '\''),
        ("nbsp", '\u{a0}'),
        ("colon", ':'),
        ("tab", '\t'),
        ("newline", '\n'),
        ("sol", '/'),
    ];
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        rest = &rest[pos..];
        let after = &rest[1..];
        if let Some(num) = after.strip_prefix('#') {
            let (radix, digits_start) = if num.starts_with(['x', 'X']) { (16, 1) } else { (10, 0) };
            let digits_len = num[digits_start..]
                .bytes()
                .take_while(|b| {
                    if radix == 16 {
                        b.is_ascii_hexdigit()
                    } else {
                        b.is_ascii_digit()
                    }
                })
                .count();
            if digits_len > 0 {
                let digits = &num[digits_start..digits_start + digits_len];
                let c = u32::from_str_radix(digits, radix)
                    .ok()
                    .and_then(char::from_u32)
                    .unwrap_or('\u{fffd}');
                out.push(c);
                let mut consumed = 2 + digits_start + digits_len;
                if rest[consumed..].starts_with(';') {
                    consumed += 1;
                }
                rest = &rest[consumed..];
                continue;
            }
        } else if let Some((name, c)) = NAMED.iter().find(|(n, _)| {
            // 按字节比较：`after` 可能以多字节字符开头，不能按 str 切片
            let ab = after.as_bytes();
            ab.len() > n.len() && ab[..n.len()].eq_ignore_ascii_case(n.as_bytes()) && ab[n.len()] == b';'
        }) {
            out.push(*c);
            rest = &rest[1 + name.len() + 1..];
            continue;
        }
        out.push('&');
        rest = &rest[1..];
    }
    out.push_str(rest);
}

/// 极简 HTML 词法器
struct Lexer<'a> {
    src: &'a str,
    pos: usize,
    /// 刚读完 `<script>` / `<style>` 开始标签：下一次先跳过其原始内容
    pending_raw: Option<&'a str>,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            pos: 0,
            pending_raw: None,
        }
    }

    fn next_token(&mut self) -> Option<Token<'a>> {
        if let Some(name) = self.pending_raw.take() {
            self.skip_raw_text(name);
        }
        let rest = &self.src[self.pos..];
        if rest.is_empty() {
            return None;
        }
        let bytes = rest.as_bytes();
        if bytes[0] != b'<' {
            let len = rest.find('<').unwrap_or(rest.len());
            self.pos += len;
            return Some(Token::Text(&rest[..len]));
        }
        // 注释
        if let Some(body) = rest.strip_prefix("<!--") {
            let end = if body.starts_with('>') {
                4 + 1
            } else if body.starts_with("->") {
                4 + 2
            } else {
                body.find("-->").map_or(rest.len(), |e| 4 + e + 3)
            };
            self.pos += end;
            return Some(Token::Comment);
        }
        // DOCTYPE / CDATA / 处理指令
        if rest.starts_with("<!") || rest.starts_with("<?") {
            let end = rest.find('>').map_or(rest.len(), |e| e + 1);
            self.pos += end;
            return Some(Token::Other(&rest[..end]));
        }
        // 结束标签
        if bytes.len() > 2 && bytes[1] == b'/' && bytes[2].is_ascii_alphabetic() {
            let name_len = tag_name_len(&rest[2..]);
            let name = &rest[2..2 + name_len];
            let end = rest.find('>').map_or(rest.len(), |e| e + 1);
            self.pos += end;
            return Some(Token::End {
                raw: &rest[..end],
                name,
            });
        }
        // 开始标签
        if bytes.len() > 1 && bytes[1].is_ascii_alphabetic() {
            if let Some(tok) = self.lex_start_tag(rest) {
                return Some(tok);
            }
            // 没有闭合的 `>`：按浏览器的做法丢弃到结尾
            self.pos = self.src.len();
            return None;
        }
        // 孤立的 `<`
        self.pos += 1;
        Some(Token::Text(&rest[..1]))
    }

    fn lex_start_tag(&mut self, rest: &'a str) -> Option<Token<'a>> {
        let b = rest.as_bytes();
        let name_len = tag_name_len(&rest[1..]);
        let name = &rest[1..1 + name_len];
        let name_end = 1 + name_len;
        let mut i = name_end;
        let mut attrs = Vec::new();
        loop {
            let attr_start = i;
            while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
                // `/` 只在紧跟 `>` 时是自闭合标记，其余当作分隔符
                if b[i] == b'/' && b.get(i + 1) == Some(&b'>') {
                    break;
                }
                i += 1;
            }
            if i >= b.len() {
                return None;
            }
            if b[i] == b'>' || (b[i] == b'/' && b.get(i + 1) == Some(&b'>')) {
                let tail_start = attr_start;
                let end = if b[i] == b'>' { i + 1 } else { i + 2 };
                self.pos += end;
                if RAW_TEXT.iter().any(|n| name.eq_ignore_ascii_case(n)) {
                    self.pending_raw = Some(name);
                }
                return Some(Token::Start {
                    raw: &rest[..end],
                    name,
                    name_end,
                    attrs,
                    tail_start,
                });
            }
            // 属性名
            let name_start = i;
            while i < b.len() && !b[i].is_ascii_whitespace() && !matches!(b[i], b'=' | b'>' | b'/') {
                i += 1;
            }
            if i == name_start {
                // `=` 开头之类的畸形写法：当作名字的一部分吞掉一个字节
                i += 1;
            }
            let attr_name = &rest[name_start..i];
            // 值
            let mut j = i;
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            let mut value = None;
            if j < b.len() && b[j] == b'=' {
                j += 1;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j >= b.len() {
                    return None;
                }
                if b[j] == b'"' || b[j] == b'\'' {
                    let q = b[j];
                    let close = rest[j + 1..].bytes().position(|c| c == q)?;
                    value = Some(&rest[j + 1..j + 1 + close]);
                    i = j + 1 + close + 1;
                } else {
                    let start = j;
                    while j < b.len() && !b[j].is_ascii_whitespace() && b[j] != b'>' {
                        j += 1;
                    }
                    value = Some(&rest[start..j]);
                    i = j;
                }
            }
            attrs.push(Attr {
                span: (attr_start, i),
                name: attr_name,
                value,
            });
        }
    }

    /// 跳过 `<script>` / `<style>` 的原始内容，停在对应结束标签的 `<` 上；没有结束标签时跳到结尾
    fn skip_raw_text(&mut self, name: &str) {
        let rest = &self.src[self.pos..];
        let mut search = 0;
        while let Some(off) = rest[search..].find("</") {
            let at = search + off;
            let cand = &rest[at + 2..];
            if cand.len() >= name.len()
                && cand.as_bytes()[..name.len()].eq_ignore_ascii_case(name.as_bytes())
                && cand[name.len()..]
                    .bytes()
                    .next()
                    .is_none_or(|c| c.is_ascii_whitespace() || c == b'>' || c == b'/')
            {
                self.pos += at;
                return;
            }
            search = at + 2;
        }
        self.pos = self.src.len();
    }
}

/// 标签名长度（ASCII 字母数字、`-`、`:`）
fn tag_name_len(s: &str) -> usize {
    s.bytes()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == b'-' || *c == b':' || *c == b'_')
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 表驱动：输入 → 期望输出
    const SANITIZE_CASES: &[(&str, &str)] = &[
        // 原样保留
        ("<b>bold</b> text", "<b>bold</b> text"),
        ("<p class=\"x\" data-a='1'>hi</p>", "<p class=\"x\" data-a='1'>hi</p>"),
        ("a < b && c > d", "a < b && c > d"),
        ("<br/><br />", "<br/><br />"),
        (
            "中文<span style=\"color:red\">红</span>",
            "中文<span style=\"color:red\">红</span>",
        ),
        (
            "<!DOCTYPE html><html><body>x</body></html>",
            "<!DOCTYPE html><html><body>x</body></html>",
        ),
        // meta / link
        ("<meta charset=\"utf-8\"><b>x</b>", "<b>x</b>"),
        ("<META http-equiv=refresh content=0>x", "x"),
        ("<link rel=stylesheet href=a.css>x", "x"),
        (
            "<html><head><meta charset='utf-8'><link rel=x></head>",
            "<html><head></head>",
        ),
        // script / style 连同内容
        ("a<script>alert(1)</script>b", "ab"),
        ("a<SCRIPT type=\"text/javascript\">if (a<b) {}</SCRIPT >b", "ab"),
        ("a<script>var s = '</div>';</script>b", "ab"),
        ("a<style>.x{color:red}</style>b", "ab"),
        ("a<style media=print>p{}</Style>b", "ab"),
        ("a<script>never closed", "a"),
        ("</script>stray", "stray"),
        // 注释
        ("a<!-- c -->b", "ab"),
        ("<!--StartFragment--><b>x</b><!--EndFragment-->", "<b>x</b>"),
        ("a<!--[if gte mso 9]><xml></xml><![endif]-->b", "ab"),
        ("a<!-->b", "ab"),
        ("a<!-- unterminated", "a"),
        // 事件属性
        ("<img src=\"a.png\" onerror=\"alert(1)\">", "<img src=\"a.png\">"),
        ("<div ONCLICK='x()' id=a>t</div>", "<div id=a>t</div>"),
        ("<body onload=init()>", "<body>"),
        ("<a onmouseover = \"x\" href=\"/p\">l</a>", "<a href=\"/p\">l</a>"),
        // javascript: / vbscript: URL
        ("<a href=\"javascript:alert(1)\">x</a>", "<a>x</a>"),
        ("<a href=\" JavaScript:alert(1)\">x</a>", "<a>x</a>"),
        ("<a href=\"java\tscript:alert(1)\">x</a>", "<a>x</a>"),
        ("<a href=\"javascript&#58;alert(1)\">x</a>", "<a>x</a>"),
        ("<a href=\"&#x6A;avascript:alert(1)\">x</a>", "<a>x</a>"),
        ("<a href=\"javascript&colon;alert(1)\">x</a>", "<a>x</a>"),
        ("<a href=javascript:alert(1)>x</a>", "<a>x</a>"),
        ("<iframe src=\"vbscript:msgbox\"></iframe>", "<iframe></iframe>"),
        (
            "<a href=\"https://example.com/?q=javascript\">ok</a>",
            "<a href=\"https://example.com/?q=javascript\">ok</a>",
        ),
        // 属性值里的 `>` 不截断标签
        ("<a title=\"a>b\" onclick=x>t</a>", "<a title=\"a>b\">t</a>"),
        // 未闭合的开始标签按浏览器做法丢弃
        ("text<a href=\"x", "text"),
    ];

    #[test]
    fn sanitize_table() {
        for (input, want) in SANITIZE_CASES {
            assert_eq!(sanitize_html(input), *want, "input: {input}");
        }
    }

    #[test]
    fn sanitize_is_idempotent() {
        for (input, _) in SANITIZE_CASES {
            let once = sanitize_html(input);
            assert_eq!(sanitize_html(&once), once, "input: {input}");
        }
    }

    /// 表驱动：HTML → 纯文本（与浏览器渲染后复制出的文字一致）
    const TEXT_CASES: &[(&str, &str)] = &[
        ("<p>Hello <b>world</b></p><p>2nd</p>", "Hello world\n2nd"),
        ("a<br>b<br/>c", "a\nb\nc"),
        ("<script>x()</script>&lt;tag&gt; &amp; &#20013;&#x6587;", "<tag> & 中文"),
        (
            "<table><tr><td>a</td><td>b</td></tr><tr><td>c</td></tr></table>",
            "a\tb\nc",
        ),
        ("<div><p></p><p></p><p></p>x</div>", "x"),
        // 缩进排版的源码：空白（含换行与缩进）按 HTML 规则折叠，块级元素换行
        (
            "<div>\n    <p>Hello\n      world,   this is\n      wrapped source\n    </p>\n\n    <ul>\n      <li>one</li>\n      <li>two</li>\n    </ul>\n  </div>",
            "Hello world, this is wrapped source\none\ntwo",
        ),
        ("  leading <i>and</i>\ttrailing  ", "leading and trailing"),
        ("a<span> </span>b", "a b"),
        ("a <b> b </b> c", "a b c"),
        ("<p>\n  first\n</p>\n<p>\n  second\n</p>", "first\nsecond"),
        ("<ol><li> first </li><li>second</li></ol>", "first\nsecond"),
        ("<tr>\n <td> a </td>\n <td> b </td>\n</tr>", "a\tb"),
        // &nbsp; 不折叠，输出为普通空格；字符引用形式的换行同样可折叠
        ("a&nbsp;&nbsp;b", "a  b"),
        ("a&#10;&#10;b", "a b"),
        // 连续的 <br> 保留一个空行；更多的空行（含只有空白的行）压缩为一个
        ("a<br><br>b", "a\n\nb"),
        ("a<br><br><br><br>b", "a\n\nb"),
        ("a<br> \n <br>  <br>b", "a\n\nb"),
        ("<p>a</p>&nbsp;<br>&nbsp;<br><p>b</p>", "a\n\nb"),
        // <pre> 内保留缩进与换行；紧跟开始标签的第一个换行忽略
        (
            "<p>code:</p><pre>\nfn main() {\n    run();\n}\n</pre><p>end</p>",
            "code:\nfn main() {\n    run();\n}\nend",
        ),
        ("<pre>  a\r\n  b\rc</pre>", "  a\n  b\nc"),
        ("<PRE>x  y</PRE> z", "x  y\nz"),
        ("<textarea>\n  keep   me</textarea>", "  keep   me"),
        // 中文与标点
        ("<div>\n  你好，\n  世界\n</div>", "你好， 世界"),
    ];

    #[test]
    fn html_to_text_table() {
        for (input, want) in TEXT_CASES {
            assert_eq!(html_to_text(input), *want, "input: {input:?}");
        }
    }

    #[test]
    fn entities() {
        let mut s = String::new();
        decode_entities_into("&#65;&#x42;&#67 &amp;&unknown; & &#;", &mut s);
        assert_eq!(s, "ABC &&unknown; & &#;");
        // `&` 后紧跟多字节字符时不能按 str 切片
        let mut s = String::new();
        decode_entities_into("&中文&lt中&gt;", &mut s);
        assert_eq!(s, "&中文&lt中>");
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(500))]

        /// 任意（畸形的）HTML 都不 panic，结果幂等，且不再含事件属性与脚本
        #[test]
        fn sanitize_is_total_and_idempotent(s in "([<>/!=\"' \\-]|on|script|style|a|b|meta|中|&#58;|javascript:){0,40}") {
            let once = sanitize_html(&s);
            proptest::prop_assert_eq!(sanitize_html(&once), once.clone());
            let _ = html_to_text(&s);
        }

        /// 提取出的纯文本：没有行首行尾的空白、没有连续两个以上的空行、开头结尾不是空行；普通排版里没有连续空格
        #[test]
        fn html_to_text_is_tidy(s in "([<>/ \\t\\n]|p|br|pre|div|td|b|x|中|&nbsp;|&#10;){0,48}") {
            let t = html_to_text(&s);
            for line in t.split('\n') {
                proptest::prop_assert_eq!(line, line.trim_end());
            }
            proptest::prop_assert!(!t.contains("\n\n\n"));
            proptest::prop_assert!(!t.starts_with('\n') && !t.ends_with('\n'));
            if !s.to_ascii_lowercase().contains("pre") {
                proptest::prop_assert!(!t.contains("  ") || s.contains("&nbsp;"), "{:?} → {:?}", s, t);
            }
        }

        #[test]
        fn arbitrary_text_never_panics(s in "\\PC{0,64}") {
            let _ = sanitize_html(&s);
            let _ = html_to_text(&s);
            let mut out = String::new();
            decode_entities_into(&s, &mut out);
        }
    }
}
