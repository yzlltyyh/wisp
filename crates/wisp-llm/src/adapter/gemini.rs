//! Google Gemini（streamGenerateContent?alt=sse）的流式事件映射与用量归一化。
//!
//! - 没有 event 行；第一个 chunk → Start；文本取 `thought != true` 的 text part（一个 chunk 可有多个 part）；
//! - usageMetadata 用最后一次的值；容忍只有 usage 的 chunk；
//! - finishReason：STOP 正常；MAX_TOKENS → truncated；SAFETY / RECITATION / PROHIBITED_CONTENT / BLOCKLIST / SPII /
//!   LANGUAGE / IMAGE_SAFETY 等 → content_filter；OTHER 与未知值：有文本算正常结束，没有文本报 upstream；
//! - promptFeedback.blockReason 且没有 candidates → content_filter；没有 finishReason 就断开 → 连接中断。

use serde_json::Value;
use wisp_types::{ErrorCode, TruncateReason, Usage};

use super::{Base, Step, non_empty_str, str_at, u32_at};
use crate::errors;

pub(crate) struct GeminiAdapter {
    pub(super) base: Base,
    finish: Option<Option<TruncateReason>>,
    usage: Option<Usage>,
    text_seen: bool,
}

impl GeminiAdapter {
    pub(super) fn new(base: Base) -> Self {
        Self {
            base,
            finish: None,
            usage: None,
            text_seen: false,
        }
    }

    pub(super) fn finishing(&self) -> bool {
        self.finish.is_some()
    }

    pub(super) fn finish_now(&mut self, out: &mut Vec<Step>) {
        self.base.finish(self.finish.flatten(), self.usage, out);
    }

    pub(super) fn on_value(&mut self, v: &Value, out: &mut Vec<Step>) {
        if self.base.done {
            return;
        }
        if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
            self.base.stream_error(err, out);
            return;
        }
        self.base.start(str_at(v, "responseId"), out);
        if let Some(u) = v.get("usageMetadata").filter(|u| u.is_object()) {
            self.usage = Some(Usage {
                input_tokens: u32_at(u, "promptTokenCount").unwrap_or(0),
                output_tokens: u32_at(u, "candidatesTokenCount").unwrap_or(0),
                reasoning_tokens: u32_at(u, "thoughtsTokenCount"),
                total_tokens: u32_at(u, "totalTokenCount"),
            });
        }
        let Some(candidate) = v.get("candidates").and_then(|c| c.get(0)) else {
            if let Some(reason) = v.get("promptFeedback").and_then(|f| non_empty_str(f, "blockReason")) {
                self.base.content_filter(&format!("blockReason={reason}"), out);
            }
            return;
        };
        if self.finish.is_none() {
            let parts = candidate
                .get("content")
                .and_then(|c| c.get("parts"))
                .and_then(Value::as_array);
            let mut text = String::new();
            let mut thought = false;
            for part in parts.into_iter().flatten() {
                let Some(t) = non_empty_str(part, "text") else {
                    continue;
                };
                if part.get("thought").and_then(Value::as_bool) == Some(true) {
                    thought = true;
                } else {
                    text.push_str(t);
                }
            }
            if thought {
                out.push(Step::Token);
            }
            if !text.is_empty() {
                self.text_seen = true;
                out.push(Step::Text(text));
            }
        }
        if self.finish.is_none()
            && let Some(reason) = non_empty_str(candidate, "finishReason")
        {
            self.on_finish_reason(reason, out);
        }
    }

    fn on_finish_reason(&mut self, reason: &str, out: &mut Vec<Step>) {
        match reason {
            "STOP" => self.finish = Some(None),
            "MAX_TOKENS" => self.finish = Some(Some(TruncateReason::MaxTokens)),
            "FINISH_REASON_UNSPECIFIED" => {}
            r if is_safety(r) => self.base.content_filter(&format!("finishReason={r}"), out),
            r => {
                if self.text_seen {
                    self.finish = Some(None);
                } else {
                    let e = errors::with_url(
                        errors::make(
                            ErrorCode::Upstream,
                            format!("the model stopped without any output (finishReason={r})"),
                        ),
                        &self.base.url,
                    );
                    self.base.fail(e, out);
                }
            }
        }
    }
}

fn is_safety(reason: &str) -> bool {
    reason == "LANGUAGE"
        || ["SAFETY", "RECITATION", "PROHIBITED", "BLOCKLIST", "SPII"]
            .iter()
            .any(|k| reason.contains(k))
}

#[cfg(test)]
mod tests {
    use wisp_types::Protocol;

    use super::super::test_util::{last, run, text};
    use super::*;

    #[test]
    fn safety_without_text_is_content_filter() {
        let s = run(
            Protocol::Gemini,
            "data: {\"candidates\":[{\"finishReason\":\"SAFETY\",\"index\":0}],\"responseId\":\"r\"}\n\n",
        );
        assert!(matches!(last(&s), Step::Fail(e) if e.code == ErrorCode::ContentFilter));
    }

    #[test]
    fn prompt_feedback_block() {
        let s = run(
            Protocol::Gemini,
            "data: {\"promptFeedback\":{\"blockReason\":\"OTHER\"}}\n\n",
        );
        assert!(matches!(last(&s), Step::Fail(e) if e.code == ErrorCode::ContentFilter));
    }

    #[test]
    fn other_reason_depends_on_text() {
        let s = run(
            Protocol::Gemini,
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"a\"}]},\"finishReason\":\"OTHER\"}]}\n\n",
        );
        assert!(matches!(last(&s), Step::Finish { .. }));
        let s = run(
            Protocol::Gemini,
            "data: {\"candidates\":[{\"finishReason\":\"OTHER\"}]}\n\n",
        );
        assert!(matches!(last(&s), Step::Fail(e) if e.code == ErrorCode::Upstream));
    }

    #[test]
    fn thought_parts_are_skipped_and_usage_only_chunks_tolerated() {
        let s = run(
            Protocol::Gemini,
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hmm\",\"thought\":true},{\"text\":\"A\"},{\"text\":\"B\"}]}}]}\n\n\
             data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"C\"}]},\"finishReason\":\"MAX_TOKENS\"}]}\n\n\
             data: {\"usageMetadata\":{\"promptTokenCount\":1,\"candidatesTokenCount\":2,\"thoughtsTokenCount\":5,\"totalTokenCount\":8}}\n\n",
        );
        assert_eq!(text(&s), "ABC");
        assert!(matches!(
            last(&s),
            Step::Finish { truncated: Some(TruncateReason::MaxTokens), usage: Some(u) }
                if u.reasoning_tokens == Some(5) && u.total_tokens == Some(8)
        ));
    }

    #[test]
    fn eof_without_finish_reason_is_connection_lost() {
        let s = run(
            Protocol::Gemini,
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"半句\"}]}}],\"responseId\":\"g\"}\n\n",
        );
        assert_eq!(text(&s), "半句");
        assert!(matches!(last(&s), Step::Fail(e) if e.code == ErrorCode::Network
                && e.subcode == Some(wisp_types::ErrorSubcode::ConnectionLost)));
    }
}
