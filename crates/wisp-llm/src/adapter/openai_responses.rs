//! OpenAI Responses 的流式事件映射与用量归一化。
//!
//! 按事件类型分发：created → Start；output_text.delta → 文本（按顺序拼接）；completed → 结束（usage 含 reasoning_tokens）；
//! incomplete：max_output_tokens → truncated，content_filter → 失败并保留已有文本；failed / error → 失败；
//! 其余事件忽略。没有 `[DONE]`。

use serde_json::Value;
use wisp_types::{TruncateReason, Usage};

use super::{Base, Step, non_empty_str, str_at, u32_at};

pub(crate) struct ResponsesAdapter {
    pub(super) base: Base,
}

impl ResponsesAdapter {
    pub(super) fn new(base: Base) -> Self {
        Self { base }
    }

    pub(super) fn finish_now(&mut self, out: &mut Vec<Step>) {
        self.base.finish(None, None, out);
    }

    pub(super) fn on_value(&mut self, event: Option<&str>, v: &Value, out: &mut Vec<Step>) {
        let kind = str_at(v, "type").or(event).unwrap_or("");
        let response = v.get("response").unwrap_or(&Value::Null);
        match kind {
            "response.created" | "response.in_progress" => self.base.start(str_at(response, "id"), out),
            "response.output_text.delta" => {
                self.base.start(None, out);
                if let Some(t) = non_empty_str(v, "delta") {
                    out.push(Step::Text(t.to_owned()));
                }
            }
            k if k.starts_with("response.reasoning") && k.ends_with(".delta") => {
                self.base.start(None, out);
                out.push(Step::Token);
            }
            "response.completed" => {
                self.base.start(str_at(response, "id"), out);
                self.base.finish(None, usage(response), out);
            }
            "response.incomplete" => self.incomplete(response, out),
            "response.failed" => {
                let err = response.get("error").filter(|e| !e.is_null()).unwrap_or(v);
                self.base.stream_error(err, out);
            }
            "error" => self.base.stream_error(v, out),
            _ => {}
        }
    }

    fn incomplete(&mut self, response: &Value, out: &mut Vec<Step>) {
        let reason = response
            .get("incomplete_details")
            .and_then(|d| str_at(d, "reason"))
            .unwrap_or("");
        if reason == "content_filter" {
            self.base.content_filter("incomplete: content_filter", out);
        } else {
            self.base.start(str_at(response, "id"), out);
            self.base.finish(Some(TruncateReason::MaxTokens), usage(response), out);
        }
    }

    /// 非流式响应：`{"object":"response","status":…,"output":[…],"usage":…}`
    pub(super) fn on_whole(&mut self, v: &Value, out: &mut Vec<Step>) {
        if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
            self.base.stream_error(err, out);
            return;
        }
        self.base.start(str_at(v, "id"), out);
        let texts = v
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| item.get("content").and_then(Value::as_array))
            .flatten()
            .filter(|c| str_at(c, "type") == Some("output_text"))
            .filter_map(|c| non_empty_str(c, "text"));
        for t in texts {
            out.push(Step::Text(t.to_owned()));
        }
        match str_at(v, "status") {
            Some("incomplete") => self.incomplete(v, out),
            Some("failed") => self.base.stream_error(v, out),
            _ => self.base.finish(None, usage(v), out),
        }
    }
}

/// input / output / total / output_tokens_details.reasoning_tokens
fn usage(response: &Value) -> Option<Usage> {
    let u = response.get("usage").filter(|u| u.is_object())?;
    Some(Usage {
        input_tokens: u32_at(u, "input_tokens").unwrap_or(0),
        output_tokens: u32_at(u, "output_tokens").unwrap_or(0),
        reasoning_tokens: u
            .get("output_tokens_details")
            .and_then(|d| u32_at(d, "reasoning_tokens")),
        total_tokens: u32_at(u, "total_tokens"),
    })
}

#[cfg(test)]
mod tests {
    use wisp_types::{ErrorCode, Protocol};

    use super::super::test_util::{last, run, text};
    use super::*;

    fn ev(kind: &str, json: &str) -> String {
        format!("event: {kind}\ndata: {json}\n\n")
    }

    #[test]
    fn incomplete_variants() {
        let base = ev(
            "response.created",
            r#"{"type":"response.created","response":{"id":"r1"}}"#,
        ) + &ev(
            "response.output_text.delta",
            r#"{"type":"response.output_text.delta","delta":"Hi"}"#,
        );
        let s = run(
            Protocol::OpenAiResponses,
            &(base.clone()
                + &ev(
                    "response.incomplete",
                    r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#,
                )),
        );
        assert_eq!(text(&s), "Hi");
        assert_eq!(
            *last(&s),
            Step::Finish {
                truncated: Some(TruncateReason::MaxTokens),
                usage: None
            }
        );
        let s = run(
            Protocol::OpenAiResponses,
            &(base
                + &ev(
                    "response.incomplete",
                    r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"}}}"#,
                )),
        );
        assert_eq!(text(&s), "Hi");
        assert!(matches!(last(&s), Step::Fail(e) if e.code == ErrorCode::ContentFilter));
    }

    #[test]
    fn failed_maps_error_codes() {
        let cases = [
            ("rate_limit_exceeded", ErrorCode::RateLimited),
            ("server_error", ErrorCode::Upstream),
            ("invalid_prompt", ErrorCode::ContentFilter),
            ("something_else", ErrorCode::Upstream),
        ];
        for (code, expected) in cases {
            let s = run(
                Protocol::OpenAiResponses,
                &ev(
                    "response.failed",
                    &format!(
                        r#"{{"type":"response.failed","response":{{"error":{{"code":"{code}","message":"m"}}}}}}"#
                    ),
                ),
            );
            assert!(matches!(last(&s), Step::Fail(e) if e.code == expected), "{code}");
        }
    }

    #[test]
    fn eof_without_completed_is_connection_lost() {
        let s = run(
            Protocol::OpenAiResponses,
            &ev(
                "response.output_text.delta",
                r#"{"type":"response.output_text.delta","delta":"x"}"#,
            ),
        );
        assert!(matches!(last(&s), Step::Fail(e) if e.code == ErrorCode::Network
                && e.subcode == Some(wisp_types::ErrorSubcode::ConnectionLost)));
    }
}
