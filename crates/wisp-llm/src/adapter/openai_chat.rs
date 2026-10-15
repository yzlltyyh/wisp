//! OpenAI Chat Completions（及兼容端点）的流式事件映射与用量归一化。
//!
//! - 第一个 chunk → Start；`delta.content` 非空才是文本；忽略 reasoning_content / reasoning / obfuscation / logprobs；
//! - `delta.refusal` → content_filter；finish_reason：length → truncated，content_filter → 失败，error → upstream；
//! - 顶层 error 对象（即使是 HTTP 200，OpenRouter）→ 失败；第一个事件就是 error 时没有 Start；
//! - usage chunk 记入用量；OpenRouter 的 usage 块里第二次出现的 finish_reason 不再触发结束；
//! - 结束：`[DONE]`，或看到 finish_reason 之后拿到 usage / EOF / 最多再等 2 s（宽限由 client 计时）。

use serde_json::Value;
use wisp_types::{TruncateReason, Usage};

use super::{Base, Step, non_empty_str, str_at, u32_at};

pub(crate) struct ChatAdapter {
    pub(super) base: Base,
    /// 已看到 finish_reason（内层为截断原因）
    finish: Option<Option<TruncateReason>>,
    usage: Option<Usage>,
}

impl ChatAdapter {
    pub(super) fn new(base: Base) -> Self {
        Self {
            base,
            finish: None,
            usage: None,
        }
    }

    pub(super) fn finishing(&self) -> bool {
        self.finish.is_some()
    }

    pub(super) fn finish_now(&mut self, out: &mut Vec<Step>) {
        let truncated = self.finish.flatten();
        self.base.finish(truncated, self.usage, out);
    }

    /// 一个 chunk（流式）或整个响应（非流式，choices[].message）
    pub(super) fn on_value(&mut self, v: &Value, out: &mut Vec<Step>) {
        if self.base.done {
            return;
        }
        if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
            self.base.stream_error(err, out);
            return;
        }
        self.base.start(str_at(v, "id"), out);
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(usage(u));
        }
        if let Some(choice) = v.get("choices").and_then(|c| c.get(0)) {
            if let Some(d) = choice.get("delta").or_else(|| choice.get("message")) {
                if non_empty_str(d, "refusal").is_some() {
                    self.base.content_filter("the model refused the request", out);
                    return;
                }
                if self.finish.is_none() {
                    if let Some(t) = non_empty_str(d, "content") {
                        out.push(Step::Text(t.to_owned()));
                    }
                    if non_empty_str(d, "reasoning_content").is_some() || non_empty_str(d, "reasoning").is_some() {
                        out.push(Step::Token);
                    }
                }
            }
            if self.finish.is_none()
                && let Some(reason) = non_empty_str(choice, "finish_reason")
            {
                match reason {
                    "length" => self.finish = Some(Some(TruncateReason::MaxTokens)),
                    "content_filter" => {
                        self.base.content_filter("finish_reason=content_filter", out);
                        return;
                    }
                    "error" => {
                        let e = serde_json::json!({ "message": "the provider reported an error in the stream" });
                        self.base.stream_error(&e, out);
                        return;
                    }
                    _ => self.finish = Some(None),
                }
            }
        }
        if self.finish.is_some() && self.usage.is_some() {
            self.finish_now(out);
        }
    }
}

/// prompt / completion / total / completion_tokens_details.reasoning_tokens
fn usage(u: &Value) -> Usage {
    Usage {
        input_tokens: u32_at(u, "prompt_tokens").unwrap_or(0),
        output_tokens: u32_at(u, "completion_tokens").unwrap_or(0),
        reasoning_tokens: u
            .get("completion_tokens_details")
            .and_then(|d| u32_at(d, "reasoning_tokens")),
        total_tokens: u32_at(u, "total_tokens"),
    }
}

#[cfg(test)]
mod tests {
    use wisp_types::{ErrorCode, Protocol};

    use super::super::test_util::{last, run, text};
    use super::*;

    #[test]
    fn deepseek_reasoning_then_content() {
        let s = run(
            Protocol::OpenAiChat,
            "data: {\"id\":\"x\",\"choices\":[{\"delta\":{\"reasoning_content\":\"想\",\"content\":null}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"reasoning_content\":null,\"content\":\"译文\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
             data: [DONE]\n\n",
        );
        assert_eq!(
            s[0],
            Step::Start {
                upstream_id: Some("x".to_owned())
            }
        );
        assert_eq!(s[1], Step::Token);
        assert_eq!(text(&s), "译文");
        assert_eq!(
            *last(&s),
            Step::Finish {
                truncated: None,
                usage: None
            }
        );
    }

    #[test]
    fn refusal_and_content_filter() {
        let s = run(
            Protocol::OpenAiChat,
            "data: {\"choices\":[{\"delta\":{\"refusal\":\"no\"}}]}\n\n",
        );
        assert!(matches!(last(&s), Step::Fail(e) if e.code == ErrorCode::ContentFilter));
        let s = run(
            Protocol::OpenAiChat,
            "data: {\"choices\":[{\"delta\":{\"content\":\"a\"},\"finish_reason\":\"content_filter\"}]}\n\n",
        );
        assert_eq!(text(&s), "a");
        assert!(matches!(last(&s), Step::Fail(e) if e.code == ErrorCode::ContentFilter));
    }

    #[test]
    fn length_is_truncated_and_eof_after_finish_is_done() {
        let s = run(
            Protocol::OpenAiChat,
            "data: {\"choices\":[{\"delta\":{\"content\":\"a\"},\"finish_reason\":\"length\"}]}\n\n",
        );
        assert_eq!(
            *last(&s),
            Step::Finish {
                truncated: Some(TruncateReason::MaxTokens),
                usage: None
            }
        );
    }

    #[test]
    fn non_stream_message_shape() {
        let mut a = super::super::Adapter::new(Protocol::OpenAiChat, "https://h/p");
        let mut out = Vec::new();
        let v = serde_json::json!({
            "id": "c1",
            "choices": [{"message": {"role": "assistant", "content": "整段"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
        });
        a.on_whole_body(&v, &mut out);
        assert_eq!(text(&out), "整段");
        assert!(matches!(last(&out), Step::Finish { usage: Some(u), .. } if u.total_tokens == Some(5)));
    }

    #[test]
    fn bad_json_rules() {
        // 第一个事件就不是 JSON → protocol
        let s = run(Protocol::OpenAiChat, "data: <html>\n\n");
        assert!(matches!(last(&s), Step::Fail(e) if e.code == ErrorCode::Protocol));
        // 单个坏行不报；连续 3 个坏行报
        let ok = "data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n";
        let s = run(Protocol::OpenAiChat, &format!("{ok}data: oops\n\n{ok}data: [DONE]\n\n"));
        assert!(matches!(last(&s), Step::Finish { .. }));
        let s = run(Protocol::OpenAiChat, &format!("{ok}data: x\n\ndata: y\n\ndata: z\n\n"));
        assert!(matches!(last(&s), Step::Fail(e) if e.code == ErrorCode::Protocol));
    }
}
