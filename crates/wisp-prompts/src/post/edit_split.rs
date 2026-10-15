//! polish / proofread 输出的切分：以最后一个 "\n---\n" 切分为（正文, 改动）。
//! 用例与 TS 的 `splitEditOutput` 共用 `tests/fixtures/edit_split.json`。
//!
//! 规则：
//! 0. 先把 `\r\n` 规范为 `\n`（部分兼容端点与本地模型输出 CRLF）；返回的正文与改动都是规范化之后的文本；
//! 1. 找最后一个 `\n---\n`（分隔线独占一行；输出以 `\n---` 结尾时也算，此时改动为空）；
//! 2. 分隔线之后的内容（trim 后，可带 `**` 加粗）以「改动」或 `Changes`（大小写不敏感）开头才切分——
//!    正文里自带的 Markdown 分隔线不会被误切成改动；
//! 3. 切分时正文去掉末尾空白，改动 trim；改动为空时返回 `None`（正文仍不含分隔线）；
//! 4. 不满足条件时整段都是正文（规范化之后）原样返回。

use std::borrow::Cow;

/// 分隔线
const SEPARATOR: &str = "\n---\n";

/// 以最后一个 "\n---\n" 切分
pub fn split_edit_output(text: &str) -> (String, Option<String>) {
    let text: Cow<'_, str> = if text.contains("\r\n") {
        Cow::Owned(text.replace("\r\n", "\n"))
    } else {
        Cow::Borrowed(text)
    };
    let (body, changes) = if let Some(i) = text.rfind(SEPARATOR) {
        (&text[..i], &text[i + SEPARATOR.len()..])
    } else if let Some(body) = text.trim_end().strip_suffix("\n---") {
        (body, "")
    } else {
        return (text.into_owned(), None);
    };
    let changes = changes.trim();
    if !changes.is_empty() && !is_changes_line(changes) {
        return (text.into_owned(), None);
    }
    let body = body.trim_end().to_owned();
    (body, (!changes.is_empty()).then(|| changes.to_owned()))
}

/// 改动说明是否以「改动」/ Changes 开头（允许 Markdown 加粗）
fn is_changes_line(s: &str) -> bool {
    let s = s.trim_start_matches(['*', '_', ' ']);
    s.starts_with("改动")
        || s.as_bytes()
            .get(..7)
            .is_some_and(|b| b.eq_ignore_ascii_case(b"changes"))
}
