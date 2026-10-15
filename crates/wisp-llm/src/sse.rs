//! 字节级行缓冲（兼容 `\r\n` / `\n` / `\r`、半行、EOF）；`event:` / `data:` / `id:` / `retry:` / 注释行。
//!
//! - 按字节累积整行后再解码 UTF-8，多字节字符被切在两个 chunk 之间也不会乱码；
//! - 冒号后只去掉一个空格，`data:{…}` 与 `data: {…}` 等价；多行 data 用 `\n` 连接；空行派发事件；
//! - 以 `:` 开头的注释行忽略（空闲计时由调用方按「收到任何字节」重置，注释行自然也算）；
//! - EOF 时没有换行的最后一行照常处理，未以空行结束的事件也会派发（兼容端点常这样收尾）；
//! - 单行或单个事件超过 [`MAX_EVENT_BYTES`] 视为协议错误，防止异常端点把内存撑爆。

/// 单行 / 单个事件 data 的上限
pub(crate) const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;

/// 一个 SSE 事件
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseEvent {
    /// `event:` 字段（没有时为 None）
    pub event: Option<String>,
    /// 多行 data 以 `\n` 连接
    pub data: String,
}

/// 行或事件过长
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TooLarge;

/// 增量 SSE 解析器
#[derive(Debug, Default)]
pub(crate) struct SseParser {
    line: Vec<u8>,
    /// 上一个 chunk 以 `\r` 结尾：下一个 chunk 开头的 `\n` 属于同一个换行
    skip_lf: bool,
    event: Option<String>,
    data: String,
    has_data: bool,
}

impl SseParser {
    /// 喂入一段字节，把完整的事件追加到 `out`
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<SseEvent>) -> Result<(), TooLarge> {
        let mut rest = chunk;
        if rest.is_empty() {
            return Ok(());
        }
        if self.skip_lf {
            self.skip_lf = false;
            if rest[0] == b'\n' {
                rest = &rest[1..];
            }
        }
        while let Some(pos) = rest.iter().position(|&b| b == b'\n' || b == b'\r') {
            self.extend_line(&rest[..pos])?;
            let terminator = rest[pos];
            rest = &rest[pos + 1..];
            self.process_line(out)?;
            if terminator == b'\r' {
                match rest.first() {
                    Some(b'\n') => rest = &rest[1..],
                    None => self.skip_lf = true,
                    Some(_) => {}
                }
            }
        }
        self.extend_line(rest)
    }

    /// 流结束：处理没有换行的最后一行，并派发尚未以空行结束的事件
    pub fn finish(&mut self, out: &mut Vec<SseEvent>) -> Result<(), TooLarge> {
        if !self.line.is_empty() {
            self.process_line(out)?;
        }
        self.dispatch(out);
        Ok(())
    }

    fn extend_line(&mut self, bytes: &[u8]) -> Result<(), TooLarge> {
        if self.line.len() + bytes.len() > MAX_EVENT_BYTES {
            return Err(TooLarge);
        }
        self.line.extend_from_slice(bytes);
        Ok(())
    }

    fn process_line(&mut self, out: &mut Vec<SseEvent>) -> Result<(), TooLarge> {
        if self.line.is_empty() {
            self.dispatch(out);
            return Ok(());
        }
        if self.line[0] == b':' {
            self.line.clear();
            return Ok(());
        }
        let (field, value) = match self.line.iter().position(|&b| b == b':') {
            Some(i) => {
                let value = &self.line[i + 1..];
                (&self.line[..i], value.strip_prefix(b" ").unwrap_or(value))
            }
            None => (&self.line[..], &[][..]),
        };
        match field {
            b"data" => {
                if self.data.len() + value.len() + 1 > MAX_EVENT_BYTES {
                    return Err(TooLarge);
                }
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(&String::from_utf8_lossy(value));
                self.has_data = true;
            }
            b"event" => self.event = Some(String::from_utf8_lossy(value).into_owned()),
            // id / retry：本客户端不做断线重连，不需要；未知字段按规范忽略
            _ => {}
        }
        self.line.clear();
        Ok(())
    }

    fn dispatch(&mut self, out: &mut Vec<SseEvent>) {
        if !self.has_data {
            self.event = None;
            return;
        }
        self.has_data = false;
        out.push(SseEvent {
            event: self.event.take(),
            data: std::mem::take(&mut self.data),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_split(input: &[u8], size: usize) -> Vec<SseEvent> {
        let mut p = SseParser::default();
        let mut out = Vec::new();
        for chunk in input.chunks(size.max(1)) {
            p.push(chunk, &mut out).expect("not too large");
        }
        p.finish(&mut out).expect("not too large");
        out
    }

    fn ev(event: Option<&str>, data: &str) -> SseEvent {
        SseEvent {
            event: event.map(str::to_owned),
            data: data.to_owned(),
        }
    }

    #[test]
    fn all_line_endings_and_splits_agree() {
        let lf = "event: a\ndata: 1\n\n: comment\ndata:2\ndata: 3\n\ndata: 中文\n\n".as_bytes();
        let crlf = "event: a\r\ndata: 1\r\n\r\n: comment\r\ndata:2\r\ndata: 3\r\n\r\ndata: 中文\r\n\r\n".as_bytes();
        let cr = "event: a\rdata: 1\r\r: comment\rdata:2\rdata: 3\r\rdata: 中文\r\r".as_bytes();
        let expected = vec![ev(Some("a"), "1"), ev(None, "2\n3"), ev(None, "中文")];
        for input in [lf, crlf, cr] {
            for size in [1, 2, 3, 7, input.len()] {
                assert_eq!(parse_split(input, size), expected, "size {size}");
            }
        }
    }

    #[test]
    fn eof_without_newline_still_dispatches() {
        assert_eq!(parse_split(b"data: [DONE]", 4), vec![ev(None, "[DONE]")]);
        assert_eq!(parse_split(b"data: x\n", 1), vec![ev(None, "x")]);
    }

    #[test]
    fn only_one_space_is_stripped_and_unknown_fields_ignored() {
        assert_eq!(
            parse_split(b"data:  two\nid: 7\nretry: 100\nfoo: bar\nnocolon\n\n", 5),
            vec![ev(None, " two")]
        );
    }

    #[test]
    fn event_without_data_is_dropped() {
        assert_eq!(parse_split(b"event: ping\n\ndata: x\n\n", 3), vec![ev(None, "x")]);
    }

    #[test]
    fn oversized_line_is_rejected() {
        let mut p = SseParser::default();
        let mut out = Vec::new();
        let big = vec![b'x'; MAX_EVENT_BYTES + 1];
        assert_eq!(p.push(&big, &mut out), Err(TooLarge));
    }
}
