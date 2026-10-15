//! Anthropic Messages 的流式事件映射与用量归一化。
//!
//! - 维护 index → block 类型（来自 content_block_start），不默认 0 号是文本；只处理 text 块的 text_delta；
//!   thinking / redacted_thinking / signature / tool_use 一律忽略（Claude 5 默认思考时会先出现一个空 thinking 块）；
//! - message_start → Start（记录 input_tokens）；message_delta：stop_reason max_tokens → truncated、refusal → content_filter，
//!   usage.output_tokens 是累计值；message_stop → 结束；error（overloaded 等）→ 失败；ping 与未知事件忽略。

use serde_json::Value;
use wisp_types::{TruncateReason, Usage};

use super::{Base, Step, non_empty_str, str_at, u32_at};

pub(crate) struct AnthropicAdapter {
    pub(super) base: Base,
    /// (index, 是否 text 块)
    blocks: Vec<(u64, bool)>,
    input: Option<u32>,
    output: Option<u32>,
    truncated: Option<TruncateReason>,
    /// 已看到 stop_reason（个别代理不发 message_stop 就断开，此时按正常结束处理）
    stopped: bool,
}

impl AnthropicAdapter {
    pub(super) fn new(base: Base) -> Self {
        Self {
            base,
            blocks: Vec::new(),
            input: None,
            output: None,
            truncated: None,
            stopped: false,
        }
    }

    pub(super) fn finishing(&self) -> bool {
        self.stopped
    }

    pub(super) fn finish_now(&mut self, out: &mut Vec<Step>) {
        let usage = (self.input.is_some() || self.output.is_some()).then(|| Usage {
            input_tokens: self.input.unwrap_or(0),
            output_tokens: self.output.unwrap_or(0),
            reasoning_tokens: None,
            total_tokens: None,
        });
        self.base.finish(self.truncated, usage, out);
    }

    pub(super) fn on_value(&mut self, event: Option<&str>, v: &Value, out: &mut Vec<Step>) {
        let kind = str_at(v, "type").or(event).unwrap_or("");
        match kind {
            "message_start" => {
                let msg = v.get("message").unwrap_or(&Value::Null);
                self.base.start(str_at(msg, "id"), out);
                if let Some(u) = msg.get("usage") {
                    self.input = u32_at(u, "input_tokens").or(self.input);
                    self.output = u32_at(u, "output_tokens").or(self.output);
                }
            }
            "content_block_start" => {
                self.base.start(None, out);
                let block = v.get("content_block").unwrap_or(&Value::Null);
                let is_text = str_at(block, "type") == Some("text");
                if let Some(index) = v.get("index").and_then(Value::as_u64) {
                    match self.blocks.iter_mut().find(|(i, _)| *i == index) {
                        Some(entry) => entry.1 = is_text,
                        None => self.blocks.push((index, is_text)),
                    }
                }
                if is_text && let Some(t) = non_empty_str(block, "text") {
                    out.push(Step::Text(t.to_owned()));
                }
            }
            "content_block_delta" => {
                let Some(index) = v.get("index").and_then(Value::as_u64) else {
                    return;
                };
                let delta = v.get("delta").unwrap_or(&Value::Null);
                let block_is_text = self.blocks.iter().find(|(i, _)| *i == index).map(|(_, t)| *t);
                match (block_is_text, str_at(delta, "type")) {
                    (Some(true), Some("text_delta")) => {
                        if let Some(t) = non_empty_str(delta, "text") {
                            out.push(Step::Text(t.to_owned()));
                        }
                    }
                    (Some(false), Some("thinking_delta")) if non_empty_str(delta, "thinking").is_some() => {
                        out.push(Step::Token);
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                let delta = v.get("delta").unwrap_or(&Value::Null);
                if let Some(u) = v.get("usage") {
                    self.output = u32_at(u, "output_tokens").or(self.output);
                    self.input = u32_at(u, "input_tokens").filter(|n| *n > 0).or(self.input);
                }
                match non_empty_str(delta, "stop_reason") {
                    Some("refusal") => self.base.content_filter("stop_reason=refusal", out),
                    Some("max_tokens") => {
                        self.truncated = Some(TruncateReason::MaxTokens);
                        self.stopped = true;
                    }
                    Some(_) => self.stopped = true,
                    None => {}
                }
            }
            "message_stop" => self.finish_now(out),
            "error" => self.base.stream_error(v, out),
            // ping 与未知事件：按官方要求向前兼容，忽略
            _ => {}
        }
    }

    /// 非流式响应：`{"type":"message","content":[…],"stop_reason":…,"usage":{…}}`
    pub(super) fn on_whole(&mut self, v: &Value, out: &mut Vec<Step>) {
        if v.get("error").is_some_and(|e| !e.is_null()) {
            self.base.stream_error(v, out);
            return;
        }
        self.base.start(str_at(v, "id"), out);
        for block in v.get("content").and_then(Value::as_array).into_iter().flatten() {
            if str_at(block, "type") == Some("text")
                && let Some(t) = non_empty_str(block, "text")
            {
                out.push(Step::Text(t.to_owned()));
            }
        }
        if let Some(u) = v.get("usage") {
            self.input = u32_at(u, "input_tokens");
            self.output = u32_at(u, "output_tokens");
        }
        match str_at(v, "stop_reason") {
            Some("refusal") => self.base.content_filter("stop_reason=refusal", out),
            Some("max_tokens") => {
                self.truncated = Some(TruncateReason::MaxTokens);
                self.finish_now(out);
            }
            _ => self.finish_now(out),
        }
    }
}

#[cfg(test)]
mod tests {
    use wisp_types::{ErrorCode, Protocol};

    use super::super::test_util::{last, run, text};
    use super::*;

    #[test]
    fn delta_before_block_start_is_ignored() {
        let s = run(
            Protocol::AnthropicMessages,
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n\
             event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ghost\"}}\n\n\
             event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        assert_eq!(text(&s), "");
        assert!(matches!(last(&s), Step::Finish { usage: Some(u), .. } if u.input_tokens == 3));
    }

    #[test]
    fn refusal_and_truncation() {
        let start = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\"}}\n\n";
        let s = run(
            Protocol::AnthropicMessages,
            &format!(
                "{start}event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"refusal\"}}}}\n\n"
            ),
        );
        assert!(matches!(last(&s), Step::Fail(e) if e.code == ErrorCode::ContentFilter));
        // stop_reason 之后没有 message_stop 就 EOF：按正常结束
        let s = run(
            Protocol::AnthropicMessages,
            &format!(
                "{start}event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"max_tokens\"}},\"usage\":{{\"output_tokens\":9}}}}\n\n"
            ),
        );
        assert!(matches!(
            last(&s),
            Step::Finish {
                truncated: Some(TruncateReason::MaxTokens),
                usage: Some(u)
            } if u.output_tokens == 9
        ));
    }

    #[test]
    fn eof_before_stop_reason_is_connection_lost() {
        let s = run(
            Protocol::AnthropicMessages,
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\"}}\n\n\
             event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
             event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"半句\"}}\n\n",
        );
        assert_eq!(text(&s), "半句");
        assert!(matches!(last(&s), Step::Fail(e) if e.code == ErrorCode::Network
                && e.subcode == Some(wisp_types::ErrorSubcode::ConnectionLost)));
    }
}
