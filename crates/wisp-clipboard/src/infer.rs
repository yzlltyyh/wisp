//! 类型推断：Link / Color / Code / Text。轻量启发式，不用模型；类型只影响图标、过滤与预览。

use std::sync::LazyLock;

use regex::Regex;
use wisp_types::ClipKind;

/// 推断文本条目的类型
pub fn infer_kind(text: &str) -> ClipKind {
    let t = text.trim();
    if t.is_empty() {
        return ClipKind::Text;
    }
    if is_link(t) {
        ClipKind::Link
    } else if is_color(t) {
        ClipKind::Color
    } else if is_code(t) {
        ClipKind::Code
    } else {
        ClipKind::Text
    }
}

/// 可直接打开的链接：`www.` 开头的补上 `https://`，其余原样返回（调用方已保证 [`infer_kind`] 为 Link）
pub(crate) fn link_target(text: &str) -> String {
    let t = text.trim();
    if t.len() >= 4 && t.as_bytes()[..4].eq_ignore_ascii_case(b"www.") {
        format!("https://{t}")
    } else {
        t.to_owned()
    }
}

/// 整体是单个 URL：`scheme://…`（scheme 至少 2 个字符，排除 `C:` 盘符）、`mailto:`、`tel:`、`sms:`、`magnet:?`，
/// 或 `www.` 开头的域名；不含空白与控制字符
fn is_link(t: &str) -> bool {
    if t.len() > 8192 || t.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    static SCHEME: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^(?:[a-z][a-z0-9+.\-]{1,31}://[^/?#]|mailto:[^@]+@[^@]+\.[^@]+$|tel:\+?[0-9][0-9\-().]*$|sms:\+?[0-9]|magnet:\?)")
            .expect("静态正则")
    });
    static WWW: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^www\.[a-z0-9\-]+(?:\.[a-z0-9\-]+)*\.[a-z]{2,}(?::[0-9]{1,5})?(?:[/?#].*)?$")
            .expect("静态正则")
    });
    SCHEME.is_match(t) || WWW.is_match(t)
}

/// 单个颜色值：`#RGB` / `#RGBA` / `#RRGGBB` / `#RRGGBBAA`、`rgb()` / `rgba()`、`hsl()` / `hsla()`，
/// 以及 `hwb()` / `lab()` / `lch()` / `oklab()` / `oklch()`（WebView 都能直接渲染色块）
fn is_color(t: &str) -> bool {
    if t.len() > 64 {
        return false;
    }
    static COLOR: LazyLock<Regex> = LazyLock::new(|| {
        let num = r"[+\-]?(?:[0-9]+(?:\.[0-9]*)?|\.[0-9]+)(?:e[+\-]?[0-9]+)?";
        let pct = format!(r"{num}%?");
        let hue = format!(r"{num}(?:deg|grad|rad|turn)?");
        let alpha = format!(r"(?:\s*[,/]\s*{pct})?");
        let sep = r"(?:\s*,\s*|\s+)";
        let pattern = format!(
            r"(?i)^(?:#(?:[0-9a-f]{{3,4}}|[0-9a-f]{{6}}|[0-9a-f]{{8}})|rgba?\(\s*{pct}{sep}{pct}{sep}{pct}{alpha}\s*\)|hsla?\(\s*{hue}{sep}{pct}{sep}{pct}{alpha}\s*\)|(?:hwb|lab|lch|oklab|oklch)\(\s*(?:{hue}|none)(?:\s+(?:{hue}|none)){{2}}{alpha}\s*\))$"
        );
        Regex::new(&pattern).expect("静态正则")
    });
    COLOR.is_match(t)
}

/// 代码：以 `$ ` 开头的命令；单行且像命令的 `> ` 开头；或多行且符号密度 / 结构特征明显
fn is_code(t: &str) -> bool {
    // 只看前 64 KB：分类不需要扫完 1 MB 的长文本
    let t = prefix_at_char_boundary(t, 64 * 1024);
    if let Some(rest) = t.strip_prefix("$ ") {
        return !rest.trim().is_empty();
    }
    let mut lines = t.lines().filter(|l| !l.trim().is_empty());
    let Some(first) = lines.next() else { return false };
    let multi = lines.next().is_some();
    if !multi {
        return first.strip_prefix("> ").is_some_and(looks_like_command);
    }
    multi_line_code_score(t)
}

/// `> ` 之后像一条命令：首个词是小写开头的命令名样式（`npm`、`./run.sh`、`~/bin/x`），且不以句末标点结尾
/// （排除邮件引用「> I agree.」「> Hello world」）
fn looks_like_command(rest: &str) -> bool {
    let rest = rest.trim();
    let Some(cmd) = rest.split_whitespace().next() else {
        return false;
    };
    let starts_ok = cmd
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || matches!(b, b'.' | b'/' | b'~'));
    let cmd_ok = cmd
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/' | b'~'));
    let ends_sentence = rest.ends_with(['.', '?', '!', '。', '？', '！', ',', '，', ':', '：']);
    starts_ok && cmd_ok && !ends_sentence
}

/// 不超过 `max` 字节的最长前缀（落在字符边界上）
pub(crate) fn prefix_at_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// 多行文本是否像代码
fn multi_line_code_score(t: &str) -> bool {
    static KEYWORD: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"^\s*(?:fn|pub|def|class|func|function|return|import|from|export|const|let|var|if|else|elif|for|while|switch|case|public|private|protected|static|struct|impl|use|mod|package|interface|enum|async|await|try|catch|#include|#define|#import|SELECT|FROM|WHERE|INSERT|UPDATE|DELETE|CREATE|ALTER|JOIN|ORDER|GROUP)\b",
        )
        .expect("静态正则")
    });
    const OPERATORS: [&str; 14] = [
        "=>", "->", "::", "&&", "||", "!=", "==", "+=", "</", "/>", "//", "/*", ":=", "#!",
    ];

    let mut lines = 0u32;
    let mut strong_end = 0u32;
    let mut indented = 0u32;
    let mut keyword = 0u32;
    let mut ops = 0u32;
    let mut sym = 0u32;
    let mut nonspace = 0u32;
    for (i, line) in t.lines().enumerate() {
        let trimmed = line.trim_end();
        if trimmed.trim_start().is_empty() {
            continue;
        }
        lines += 1;
        if trimmed.ends_with([';', '{', '}']) {
            strong_end += 1;
        }
        if i > 0 && (line.starts_with("  ") || line.starts_with('\t')) {
            indented += 1;
        }
        if KEYWORD.is_match(trimmed) {
            keyword += 1;
        }
        for op in OPERATORS {
            ops += u32::try_from(trimmed.matches(op).count()).unwrap_or(u32::MAX);
        }
        for c in trimmed.chars() {
            if !c.is_whitespace() {
                nonspace += 1;
                if matches!(c, '{' | '}' | '[' | ']' | '(' | ')' | ';' | '=' | '<' | '>') {
                    sym += 1;
                }
            }
        }
    }
    if lines < 2 || nonspace == 0 {
        return false;
    }
    let density = f64::from(sym) / f64::from(nonspace);
    let ratio = |n: u32| f64::from(n) / f64::from(lines);
    let structural = ratio(strong_end) >= 0.3 || ratio(indented) >= 0.25 || ratio(keyword) >= 0.3 || ops >= 2;
    density >= 0.12 || (density >= 0.04 && structural)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(kind: ClipKind, positives: &[&str], negatives: &[&str]) {
        for p in positives {
            assert_eq!(infer_kind(p), kind, "应判为 {kind:?}：{p:?}");
        }
        for n in negatives {
            assert_ne!(infer_kind(n), kind, "不应判为 {kind:?}：{n:?}");
        }
    }

    #[test]
    fn links() {
        check(
            ClipKind::Link,
            &[
                "https://example.com",
                "http://localhost:3000/path?q=1#x",
                "  https://例子.测试/路径  ",
                "mailto:someone@example.com",
                "ftp://files.example.org/pub",
                "www.example.com/docs",
                "vscode://file/Users/me/a.rs",
                "tel:+1-555-0100",
                "magnet:?xt=urn:btih:abc",
            ],
            &[
                "https://example.com and more",
                "see https://example.com",
                "example.com",
                "http://",
                "C://Windows",
                "mailto:",
                "just text",
                "https://a.com\nhttps://b.com",
                "www.example",
            ],
        );
        assert_eq!(link_target("www.example.com"), "https://www.example.com");
        assert_eq!(link_target(" https://a.b "), "https://a.b");
    }

    #[test]
    fn colors() {
        check(
            ClipKind::Color,
            &[
                "#fff",
                "#3478F6",
                "#3478f6cc",
                "#abcd",
                "rgb(52, 120, 246)",
                "rgba(52,120,246,0.5)",
                "rgb(52 120 246 / 50%)",
                "hsl(210, 90%, 58%)",
                "hsla(210deg 90% 58% / .8)",
                "oklch(0.7 0.15 250)",
                "  #FFFFFF  ",
            ],
            &[
                "#ggg",
                "#12345",
                "fff",
                "rgb(1, 2)",
                "color: #fff",
                "#fff #000",
                "hsl(a, b, c)",
                "rgb(1,2,3",
                "#",
                "red",
            ],
        );
    }

    #[test]
    fn code() {
        check(
            ClipKind::Code,
            &[
                "fn main() {\n    println!(\"hi\");\n}",
                "const add = (a, b) => {\n  return a + b;\n};",
                "def foo(x):\n    if x > 1:\n        return x * 2\n    return None",
                "{\n  \"name\": \"wisp\",\n  \"version\": \"0.1.0\"\n}",
                "SELECT id, name\nFROM users\nWHERE age > 18;",
                ".btn {\n  color: red;\n  padding: 4px 8px;\n}",
                "<div class=\"a\">\n  <span>hi</span>\n</div>",
                "$ npm install",
                "$ cargo build\n$ cargo test",
                "> npm run dev",
                "#include <stdio.h>\nint main(void) { return 0; }",
            ],
            &[
                "Hello team,\n\nThe meeting (scheduled for Monday) has moved to Tuesday. Please update your calendars.",
                "今天天气很好。\n我们去公园散步吧（如果不下雨的话）。",
                "- buy milk\n- call mom\n- fix bike",
                "1 Infinite Loop\nCupertino, CA 95014",
                "9:00 - standup\n10:30 - review (room 2)",
                "> This is a quoted reply.",
                "> I think so.\n> Yes, agreed.",
                "> Hello world",
                "let x = 1;",
                "let it be\nlet it go\nlet it snow",
                "Roses are red,\n  violets are blue,\n  sugar is sweet,\n  and so are you.",
                "$",
                "Name\tAge\nBob\t30\nAlice\t28",
            ],
        );
    }

    #[test]
    fn plain_text() {
        for t in ["hello", "你好，世界", "", "   ", "a (b) c", "1 + 1 = 2"] {
            assert_eq!(infer_kind(t), ClipKind::Text, "{t:?}");
        }
    }
}
