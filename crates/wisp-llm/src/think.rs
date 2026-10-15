//! 跨 chunk 剥离 `<think>`、`<thinking>`、`<reasoning>`。
//!
//! - 只在 `quirks.strip_think_tags` 且协议为 openai_chat 时启用（由调用方决定）；
//! - 只有标签出现在**输出开头**（前面只有空白）时才进入丢弃状态，正文中间出现的同名标签原样保留；
//! - 丢弃状态下只保留「闭合标签长度 − 1」字节（≤ 11）的尾部缓冲，用于匹配被切到两个 chunk 的闭合标签；
//! - 退出时去掉紧随的换行或空白：空白里有换行时去掉到最后一个换行为止（保留下一行的缩进），否则去掉一个空白字符。

const TAGS: [(&str, &str); 3] = [
    ("<think>", "</think>"),
    ("<thinking>", "</thinking>"),
    ("<reasoning>", "</reasoning>"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// 未启用：原样放行
    Off,
    /// 到目前为止只见过空白（或开标签的前缀）
    Leading,
    /// 在思考块内，丢弃直到闭合标签
    Inside { close: &'static str },
    /// 刚出思考块，吞掉紧随的空白
    AfterClose,
    /// 正文：原样放行
    Pass,
}

/// `<think>` 剥离状态机
#[derive(Debug)]
pub(crate) struct ThinkStripper {
    state: State,
    buf: String,
}

impl ThinkStripper {
    /// `enabled = false` 时所有文本原样放行
    pub fn new(enabled: bool) -> Self {
        Self {
            state: if enabled { State::Leading } else { State::Off },
            buf: String::new(),
        }
    }

    /// 喂入一段文本，返回本次可以放行的部分（可能为空）
    pub fn push(&mut self, text: String) -> String {
        match self.state {
            State::Off | State::Pass => text,
            State::Leading => {
                self.buf.push_str(&text);
                self.after_leading()
            }
            State::Inside { close } => {
                self.buf.push_str(&text);
                self.after_inside(close)
            }
            State::AfterClose => {
                self.buf.push_str(&text);
                self.after_close()
            }
        }
    }

    /// 流结束：放行仍在缓冲里、确定不是思考内容的文本
    pub fn finish(&mut self) -> String {
        let out = match self.state {
            // 只有空白，或者只是像开标签的前缀（例如 "<thi"），都是正文
            State::Leading => std::mem::take(&mut self.buf),
            // 思考块没闭合 / 闭合后只剩空白：都不输出
            State::Inside { .. } | State::AfterClose | State::Off | State::Pass => String::new(),
        };
        self.buf.clear();
        self.state = match self.state {
            State::Off => State::Off,
            _ => State::Pass,
        };
        out
    }

    fn after_leading(&mut self) -> String {
        let trimmed = self.buf.trim_start();
        if trimmed.is_empty() {
            return String::new();
        }
        let head = trimmed.as_bytes();
        for (open, close) in TAGS {
            if head.len() >= open.len() && head[..open.len()].eq_ignore_ascii_case(open.as_bytes()) {
                // 前 open.len() 个字节都是 ASCII，这里一定是字符边界
                let rest = trimmed[open.len()..].to_owned();
                self.buf = rest;
                self.state = State::Inside { close };
                return self.after_inside(close);
            }
        }
        let maybe_tag = TAGS
            .iter()
            .any(|(open, _)| head.len() < open.len() && open.as_bytes()[..head.len()].eq_ignore_ascii_case(head));
        if maybe_tag {
            return String::new();
        }
        self.state = State::Pass;
        std::mem::take(&mut self.buf)
    }

    fn after_inside(&mut self, close: &'static str) -> String {
        if let Some(pos) = find_ascii_ci(&self.buf, close) {
            let rest = self.buf[pos + close.len()..].to_owned();
            self.buf = rest;
            self.state = State::AfterClose;
            return self.after_close();
        }
        // 只保留可能是闭合标签开头的尾部
        let keep = close.len() - 1;
        if self.buf.len() > keep {
            let mut cut = self.buf.len() - keep;
            while !self.buf.is_char_boundary(cut) {
                cut += 1;
            }
            self.buf.drain(..cut);
        }
        String::new()
    }

    fn after_close(&mut self) -> String {
        let Some(start) = self.buf.find(|c: char| !c.is_whitespace()) else {
            return String::new();
        };
        let ws = &self.buf[..start];
        let keep_from = match ws.rfind(['\n', '\r']) {
            Some(i) => i + 1,
            None => ws.chars().next().map_or(0, char::len_utf8),
        };
        self.state = State::Pass;
        let out = self.buf[keep_from..].to_owned();
        self.buf.clear();
        out
    }
}

/// ASCII 大小写不敏感的子串查找（needle 必须是 ASCII）
fn find_ascii_ci(haystack: &str, needle: &str) -> Option<usize> {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    (0..=h.len() - n.len()).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(enabled: bool, parts: &[&str]) -> String {
        let mut s = ThinkStripper::new(enabled);
        let mut out = String::new();
        for p in parts {
            out.push_str(&s.push((*p).to_owned()));
        }
        out.push_str(&s.finish());
        out
    }

    #[test]
    fn acceptance_vector() {
        assert_eq!(run(true, &["<thi", "nk>abc</th", "ink>\n译文"]), "译文");
    }

    #[test]
    fn disabled_passes_through() {
        assert_eq!(
            run(false, &["<thi", "nk>abc</th", "ink>\n译文"]),
            "<think>abc</think>\n译文"
        );
    }

    #[test]
    fn variants_and_case() {
        assert_eq!(run(true, &["  <thinking>x</thinking> 你好"]), "你好");
        assert_eq!(run(true, &["<REASONING>x\ny</Reasoning>\n\n  code"]), "  code");
        assert_eq!(
            run(true, &["\n<think>\n", "长长的思考", "\n</think>\n\nHello"]),
            "Hello"
        );
    }

    #[test]
    fn tag_not_at_start_is_kept() {
        assert_eq!(run(true, &["答案 <think>保留</think>"]), "答案 <think>保留</think>");
        assert_eq!(run(true, &["<b>粗体</b>"]), "<b>粗体</b>");
    }

    #[test]
    fn every_split_gives_same_result() {
        let input = "<think>让我想想……</think>\n译文：你好，世界";
        let chars: Vec<char> = input.chars().collect();
        for size in 1..=chars.len() {
            let parts: Vec<String> = chars.chunks(size).map(|c| c.iter().collect()).collect();
            let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
            assert_eq!(run(true, &refs), "译文：你好，世界", "size {size}");
        }
    }

    #[test]
    fn unclosed_or_partial() {
        assert_eq!(run(true, &["<think>never closed"]), "");
        assert_eq!(run(true, &["<thi"]), "<thi");
        assert_eq!(run(true, &["   "]), "   ");
    }
}
