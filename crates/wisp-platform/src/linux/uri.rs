//! 文件 URI 与剪贴板文件列表格式（纯逻辑，只依赖 std）。
//!
//! X11 剪贴板的文件列表有两种约定：`text/uri-list`（RFC 2483，CRLF 分隔，`#` 开头为注释）与
//! GNOME / Nautilus 的 `x-special/gnome-copied-files`（首行 `copy` 或 `cut`，其后每行一个 URI）。
//! 读取时两者都接受；写入时两者都提供。`RawClipboard.files` / `ClipPayload.files` 里是本地绝对路径。

/// 解析 `text/uri-list`，返回其中 `file://` URI 对应的本地路径（其他协议丢弃）
pub fn paths_from_uri_list(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.trim_end_matches('\r').trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(file_uri_to_path)
        .collect()
}

/// 解析 `x-special/gnome-copied-files`
pub fn paths_from_gnome_copied_files(text: &str) -> Vec<String> {
    let mut lines = text.lines().map(|l| l.trim_end_matches('\r'));
    match lines.next() {
        Some("copy" | "cut") => lines.filter_map(file_uri_to_path).collect(),
        // 少数程序省略了首行动作
        Some(first) => std::iter::once(first)
            .chain(lines)
            .filter_map(file_uri_to_path)
            .collect(),
        None => Vec::new(),
    }
}

/// `file://[localhost]/abs/path` → `/abs/path`（百分号解码）；不是本地文件 URI 时返回 None
pub fn file_uri_to_path(uri: &str) -> Option<String> {
    let rest = uri.trim().strip_prefix("file://")?;
    let path = if rest.starts_with('/') {
        rest
    } else {
        // 带主机名：只接受 localhost
        let (host, path) = rest.split_at(rest.find('/')?);
        if !host.eq_ignore_ascii_case("localhost") {
            return None;
        }
        path
    };
    percent_decode(path)
}

/// `/abs/path` → `file:///abs/path`（按 RFC 3986 对非保留字符以外的字节编码）
pub fn path_to_file_uri(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 8);
    out.push_str("file://");
    for &b in path.as_bytes() {
        let keep = b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~');
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// 写入 `text/uri-list`（CRLF 结尾）
pub fn build_uri_list(paths: &[String]) -> String {
    let mut out = String::new();
    for p in paths {
        out.push_str(&path_to_file_uri(p));
        out.push_str("\r\n");
    }
    out
}

/// 写入 `x-special/gnome-copied-files`（动作 copy；行间用 LF，末尾不带换行，与 Nautilus 一致）
pub fn build_gnome_copied_files(paths: &[String]) -> String {
    let mut out = String::from("copy");
    for p in paths {
        out.push('\n');
        out.push_str(&path_to_file_uri(p));
    }
    out
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let h = bytes.get(i + 1).copied().and_then(hex)?;
            let l = bytes.get(i + 2).copied().and_then(hex)?;
            out.push(h << 4 | l);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    // Linux 路径不要求是 UTF-8，但 Wisp 的数据模型用 String；非 UTF-8 的路径无法正确往返，丢弃
    String::from_utf8(out).ok()
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_list_parsing() {
        let text = "# comment\r\nfile:///home/a/x%20y.txt\r\nhttps://example.com/\r\nfile://localhost/tmp/%E4%B8%AD.png\r\nfile://otherhost/etc/passwd\r\n\r\n";
        assert_eq!(
            paths_from_uri_list(text),
            vec!["/home/a/x y.txt".to_owned(), "/tmp/中.png".to_owned()]
        );
    }

    #[test]
    fn gnome_copied_files() {
        assert_eq!(
            paths_from_gnome_copied_files("copy\nfile:///a/b\nfile:///c%25d"),
            vec!["/a/b".to_owned(), "/c%d".to_owned()]
        );
        assert_eq!(paths_from_gnome_copied_files("file:///only"), vec!["/only".to_owned()]);
        assert!(paths_from_gnome_copied_files("").is_empty());
    }

    #[test]
    fn roundtrip_paths() {
        let paths = vec!["/home/u/报告 v2 (final).pdf".to_owned(), "/tmp/a#b?c".to_owned()];
        let list = build_uri_list(&paths);
        assert!(list.ends_with("\r\n"));
        assert!(!list.contains(' '));
        assert_eq!(paths_from_uri_list(&list), paths);
        let gnome = build_gnome_copied_files(&paths);
        assert!(gnome.starts_with("copy\nfile:///home/u/"));
        assert_eq!(paths_from_gnome_copied_files(&gnome), paths);
    }

    #[test]
    fn rejects_malformed_escapes() {
        assert_eq!(file_uri_to_path("file:///x%zz"), None);
        assert_eq!(file_uri_to_path("file:///x%f"), None);
        assert_eq!(file_uri_to_path("file:///x%ff"), None, "非 UTF-8 路径丢弃");
        assert_eq!(file_uri_to_path("http:///x"), None);
    }
}
