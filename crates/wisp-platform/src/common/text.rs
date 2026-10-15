//! 选区文本规范化：CRLF / CR → LF，去 NUL，保留内部空白，超过 1 MB 截断。

/// 选区文本上限（字节）
pub const MAX_SELECTION_BYTES: usize = 1024 * 1024;

/// 规范化选区文本，返回 (文本, 是否被截断)。截断发生在 UTF-8 字符边界上，结果不超过 [`MAX_SELECTION_BYTES`]。
///
/// 单趟扫描：超长输入只处理到上限为止，不会先整体复制一遍。
pub fn normalize_selection(raw: &str) -> (String, bool) {
    normalize_with_limit(raw, MAX_SELECTION_BYTES)
}

/// 同 [`normalize_selection`]，上限可指定（测试与平台层的小上限场景使用）
pub fn normalize_with_limit(raw: &str, max_bytes: usize) -> (String, bool) {
    let mut out = String::with_capacity(raw.len().min(max_bytes));
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        let mapped = match c {
            '\0' => continue,
            '\r' => {
                // CRLF 与单独的 CR 都变成一个 LF
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                '\n'
            }
            other => other,
        };
        if out.len() + mapped.len_utf8() > max_bytes {
            return (out, true);
        }
        out.push(mapped);
    }
    (out, false)
}

/// 把字节串截断到不超过 `max_bytes` 的 UTF-8 字符边界并解码（非法序列按 U+FFFD 替换）。
/// 返回 (文本, 是否截断)；剪贴板读取使用
pub fn truncate_utf8_lossy(bytes: &[u8], max_bytes: usize) -> (String, bool) {
    if bytes.len() <= max_bytes {
        return (String::from_utf8_lossy(bytes).into_owned(), false);
    }
    let mut end = max_bytes;
    // 回退到字符边界：UTF-8 的后续字节形如 0b10xx_xxxx
    while end > 0 && (bytes[end] & 0xC0) == 0x80 {
        end -= 1;
    }
    (String::from_utf8_lossy(&bytes[..end]).into_owned(), true)
}

/// 判空（只在判空时 trim）
pub fn is_blank(text: &str) -> bool {
    text.trim().is_empty()
}
