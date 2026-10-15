//! 把各协议的 SSE 事件映射成统一的步骤（开始 / 文本 / 结束 / 失败），并归一化用量；
//! 各协议的结束判定规则见各自的适配器模块。
//!
//! 适配器只做协议层的事；`<think>` 剥离、首 token / 空闲计时、Start 之后补发 Notice、写回能力缓存都在 client.rs。
//! 每个适配器在发出一个终止步骤（Finish / Fail）之后忽略后续一切事件。
//!
//! 协议正文之外的出错形态（MiniMax 的 `base_resp.status_code ≠ 0`、中转站的顶层 `{code, msg}`）在分发给各协议之前
//! 统一识别（[`errors::payload_error`]），不论出现在流中还是 2xx 的整个 JSON 里；2xx 的整个 JSON 既不是出错形态、
//! 也认不出协议形状时归为 protocol，绝不报成「连接中断」。

mod anthropic;
mod gemini;
mod openai_chat;
mod openai_responses;

use serde_json::Value;
use wisp_types::{ErrorCode, LlmError, Protocol, TruncateReason, Usage};

use crate::errors;
use crate::sse::SseEvent;

/// 适配器产出的步骤
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Step {
    /// 流开始（各协议的第一个有效事件）
    Start { upstream_id: Option<String> },
    /// 可见文本增量（还没剥离 `<think>`）
    Text(String),
    /// 推理 / 思考内容（不可见，只用于首 token 计时）
    Token,
    /// 正常结束
    Finish {
        truncated: Option<TruncateReason>,
        usage: Option<Usage>,
    },
    /// 失败结束（已经发出的文本保留）
    Fail(LlmError),
}

/// 四种协议的适配器
pub(crate) enum Adapter {
    Chat(openai_chat::ChatAdapter),
    Responses(openai_responses::ResponsesAdapter),
    Anthropic(anthropic::AnthropicAdapter),
    Gemini(gemini::GeminiAdapter),
}

impl Adapter {
    /// `url`：请求 URL，只用于错误里的 host + path
    #[cfg(test)]
    pub fn new(protocol: Protocol, url: &str) -> Self {
        Self::with_image(protocol, url, false)
    }

    /// `has_image`：请求带图——流中的「不支持图片输入」归为确定的 no_vision
    pub fn with_image(protocol: Protocol, url: &str, has_image: bool) -> Self {
        let mut base = Base::new(protocol, url);
        base.has_image = has_image;
        match protocol {
            Protocol::OpenAiChat => Self::Chat(openai_chat::ChatAdapter::new(base)),
            Protocol::OpenAiResponses => Self::Responses(openai_responses::ResponsesAdapter::new(base)),
            Protocol::AnthropicMessages => Self::Anthropic(anthropic::AnthropicAdapter::new(base)),
            Protocol::Gemini => Self::Gemini(gemini::GeminiAdapter::new(base)),
        }
    }

    /// 处理一个 SSE 事件
    pub fn on_event(&mut self, ev: &SseEvent, out: &mut Vec<Step>) {
        if self.base().done {
            return;
        }
        let data = ev.data.trim();
        if data.is_empty() {
            return;
        }
        if data == "[DONE]" {
            self.on_done_marker(out);
            return;
        }
        match self.base_mut().gate(data) {
            Ok(Some(v)) => {
                // 协议正文之外的出错形态（MiniMax 的 base_resp、中转站的 {code, msg}）：按它归类后结束
                if let Some(e) = errors::payload_error(&v, &self.base().url, self.base().has_image) {
                    self.base_mut().fail(e, out);
                    return;
                }
                self.on_value(ev.event.as_deref(), &v, out);
            }
            Ok(None) => {}
            Err(e) => self.base_mut().fail(e, out),
        }
    }

    /// 2xx 但不是事件流、而是一整个 JSON：端点忽略了 stream:true（按非流式响应解析），或代理忽略 alt=sse 返回
    /// `[{…},{…}]`，或者是出错形态（MiniMax 的 `base_resp`、中转站的 `{code, msg}`）。
    /// 响应体是完整的，所以有文本或看到了结束原因就正常结束；既不是出错形态、也认不出协议形状的 → protocol
    /// （绝不会报成「连接中断」）
    pub fn on_whole_body(&mut self, v: &Value, out: &mut Vec<Step>) {
        let from = out.len();
        match v {
            Value::Array(items) if !items.is_empty() => {
                for item in items {
                    self.on_whole_part(item, out);
                }
            }
            _ => self.on_whole_part(v, out),
        }
        if self.base().done {
            return;
        }
        if self.finishing() || out[from..].iter().any(|s| matches!(s, Step::Text(_))) {
            self.finish_now(out);
        } else {
            let url = self.base().url.clone();
            let e = errors::protocol("the endpoint returned a JSON response without any output", &url);
            self.base_mut().fail(e, out);
        }
    }

    /// 整个 JSON 响应体（或 JSON 数组中的一项）
    fn on_whole_part(&mut self, v: &Value, out: &mut Vec<Step>) {
        if self.base().done {
            return;
        }
        let url = self.base().url.clone();
        if let Some(e) = errors::payload_error(v, &url, self.base().has_image) {
            self.base_mut().fail(e, out);
            return;
        }
        let has_error = v.get("error").is_some_and(|e| !e.is_null());
        if !has_error && !self.recognizes(v) {
            let e = errors::unexpected_json(v, protocol_name(self.base().protocol), &url);
            self.base_mut().fail(e, out);
            return;
        }
        match self {
            Self::Chat(a) => a.on_value(v, out),
            Self::Responses(a) => a.on_whole(v, out),
            Self::Anthropic(a) => a.on_whole(v, out),
            Self::Gemini(a) => a.on_value(v, out),
        }
    }

    /// 非流式响应体是不是这个协议的形状（顶层 error 对象另由各适配器处理）
    fn recognizes(&self, v: &Value) -> bool {
        let is = |key: &str, check: fn(&Value) -> bool| v.get(key).is_some_and(check);
        match self {
            Self::Chat(_) => is("choices", Value::is_array),
            Self::Responses(_) => str_at(v, "object") == Some("response") || is("output", Value::is_array),
            Self::Anthropic(_) => str_at(v, "type") == Some("message") || is("content", Value::is_array),
            Self::Gemini(_) => {
                is("candidates", Value::is_array)
                    || is("promptFeedback", Value::is_object)
                    || is("usageMetadata", Value::is_object)
            }
        }
    }

    /// 响应体结束
    pub fn on_eof(&mut self, out: &mut Vec<Step>) {
        if self.base().done {
            return;
        }
        if self.finishing() {
            self.finish_now(out);
        } else {
            let url = self.base().url.clone();
            self.base_mut().fail(errors::connection_lost(&url), out);
        }
    }

    /// 已看到结束标记，正在等 `[DONE]` / usage / EOF（最多再等 2 s：有的端点在结束标记之后才发用量）
    pub fn finishing(&self) -> bool {
        match self {
            Self::Chat(a) => a.finishing(),
            Self::Responses(_) => false,
            Self::Anthropic(a) => a.finishing(),
            Self::Gemini(a) => a.finishing(),
        }
    }

    /// 宽限期到或 EOF：按已有信息正常结束
    pub fn finish_now(&mut self, out: &mut Vec<Step>) {
        match self {
            Self::Chat(a) => a.finish_now(out),
            Self::Responses(a) => a.finish_now(out),
            Self::Anthropic(a) => a.finish_now(out),
            Self::Gemini(a) => a.finish_now(out),
        }
    }

    /// 已经发出终止步骤
    pub fn is_done(&self) -> bool {
        self.base().done
    }

    fn on_done_marker(&mut self, out: &mut Vec<Step>) {
        match self {
            // Chat：[DONE] 就是结束标记（没看到 finish_reason 也算正常结束）
            Self::Chat(a) => a.finish_now(out),
            // 其余协议本不发 [DONE]；代理补发时等同于 EOF
            _ => self.on_eof(out),
        }
    }

    fn on_value(&mut self, event: Option<&str>, v: &Value, out: &mut Vec<Step>) {
        match self {
            Self::Chat(a) => a.on_value(v, out),
            Self::Responses(a) => a.on_value(event, v, out),
            Self::Anthropic(a) => a.on_value(event, v, out),
            Self::Gemini(a) => a.on_value(v, out),
        }
    }

    fn base(&self) -> &Base {
        match self {
            Self::Chat(a) => &a.base,
            Self::Responses(a) => &a.base,
            Self::Anthropic(a) => &a.base,
            Self::Gemini(a) => &a.base,
        }
    }

    fn base_mut(&mut self) -> &mut Base {
        match self {
            Self::Chat(a) => &mut a.base,
            Self::Responses(a) => &mut a.base,
            Self::Anthropic(a) => &mut a.base,
            Self::Gemini(a) => &mut a.base,
        }
    }
}

/// 各适配器共用的状态
pub(crate) struct Base {
    protocol: Protocol,
    url: String,
    started: bool,
    done: bool,
    /// 已经出现过合法 JSON
    valid_seen: bool,
    /// 连续的非 JSON data 数
    bad_run: u8,
    /// 请求带图
    has_image: bool,
}

impl Base {
    fn new(protocol: Protocol, url: &str) -> Self {
        Self {
            protocol,
            url: url.to_owned(),
            started: false,
            done: false,
            valid_seen: false,
            bad_run: 0,
            has_image: false,
        }
    }

    /// data 不是 JSON 也不是 `[DONE]` 时记一次警告；第一个事件就不是 JSON、或连续 3 次时报 protocol
    fn gate(&mut self, data: &str) -> Result<Option<Value>, LlmError> {
        match serde_json::from_str::<Value>(data) {
            Ok(v) => {
                self.valid_seen = true;
                self.bad_run = 0;
                Ok(Some(v))
            }
            Err(_) => {
                self.bad_run = self.bad_run.saturating_add(1);
                if !self.valid_seen || self.bad_run >= 3 {
                    Err(errors::protocol(
                        format!(
                            "the endpoint did not return a {} event stream; check the template and base URL",
                            protocol_name(self.protocol)
                        ),
                        &self.url,
                    ))
                } else {
                    log::warn!("skipping a non-JSON SSE data line ({} so far)", self.bad_run);
                    Ok(None)
                }
            }
        }
    }

    fn start(&mut self, upstream_id: Option<&str>, out: &mut Vec<Step>) {
        if !self.started {
            self.started = true;
            out.push(Step::Start {
                upstream_id: upstream_id.filter(|s| !s.is_empty()).map(str::to_owned),
            });
        }
    }

    fn finish(&mut self, truncated: Option<TruncateReason>, usage: Option<Usage>, out: &mut Vec<Step>) {
        if !self.done {
            self.done = true;
            out.push(Step::Finish { truncated, usage });
        }
    }

    fn fail(&mut self, e: LlmError, out: &mut Vec<Step>) {
        if !self.done {
            self.done = true;
            out.push(Step::Fail(e));
        }
    }

    fn content_filter(&mut self, detail: &str, out: &mut Vec<Step>) {
        let e = errors::with_url(
            errors::make(ErrorCode::ContentFilter, format!("the response was blocked ({detail})")),
            &self.url,
        );
        self.fail(e, out);
    }

    fn stream_error(&mut self, err: &Value, out: &mut Vec<Step>) {
        let e = errors::stream_error(err, &self.url, self.has_image);
        self.fail(e, out);
    }
}

pub(crate) fn protocol_name(p: Protocol) -> &'static str {
    match p {
        Protocol::OpenAiChat => "OpenAI Chat Completions",
        Protocol::OpenAiResponses => "OpenAI Responses",
        Protocol::AnthropicMessages => "Anthropic Messages",
        Protocol::Gemini => "Gemini",
    }
}

fn u32_at(v: &Value, key: &str) -> Option<u32> {
    v.get(key)
        .and_then(Value::as_u64)
        .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
}

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn non_empty_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    str_at(v, key).filter(|s| !s.is_empty())
}

#[cfg(test)]
pub(crate) mod test_util {
    use super::*;
    use crate::sse::SseParser;

    /// 把一整段 SSE 文本喂给适配器，返回步骤（测试用）
    pub fn run(protocol: Protocol, sse: &str) -> Vec<Step> {
        let mut parser = SseParser::default();
        let mut events = Vec::new();
        parser.push(sse.as_bytes(), &mut events).expect("size");
        parser.finish(&mut events).expect("size");
        let mut a = Adapter::new(protocol, "https://h/p");
        let mut out = Vec::new();
        for e in &events {
            a.on_event(e, &mut out);
        }
        a.on_eof(&mut out);
        out
    }

    pub fn text(steps: &[Step]) -> String {
        steps
            .iter()
            .filter_map(|s| match s {
                Step::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    pub fn last(steps: &[Step]) -> &Step {
        steps.last().expect("at least one step")
    }
}

#[cfg(test)]
mod tests {
    use wisp_types::{ErrorCode, ErrorSubcode};

    use super::test_util::{last, run, text};
    use super::*;

    fn whole(protocol: Protocol, v: &Value) -> Vec<Step> {
        let mut a = Adapter::new(protocol, "https://h/p");
        let mut out = Vec::new();
        a.on_whole_body(v, &mut out);
        out
    }

    fn fail_code(steps: &[Step]) -> Option<ErrorCode> {
        match last(steps) {
            Step::Fail(e) => Some(e.code),
            _ => None,
        }
    }

    #[test]
    fn whole_json_error_shapes_are_classified_for_every_protocol() {
        let minimax = serde_json::json!({
            "id": "", "choices": null,
            "base_resp": {"status_code": 1004, "status_msg": "login fail: Please carry the API secret key"}
        });
        let relay = serde_json::json!({"code": 401, "msg": "invalid token"});
        for protocol in [
            Protocol::OpenAiChat,
            Protocol::OpenAiResponses,
            Protocol::AnthropicMessages,
            Protocol::Gemini,
        ] {
            assert_eq!(
                fail_code(&whole(protocol, &minimax)),
                Some(ErrorCode::Auth),
                "{protocol:?}"
            );
            assert_eq!(
                fail_code(&whole(protocol, &relay)),
                Some(ErrorCode::Auth),
                "{protocol:?}"
            );
            // 认不出的形状 → protocol（旧实现：Chat / Gemini 报 network + connection_lost，Responses / Anthropic 报成功的空 Done）
            let odd = whole(protocol, &serde_json::json!({"foo": 1}));
            assert!(
                matches!(last(&odd), Step::Fail(e) if e.code == ErrorCode::Protocol && e.subcode.is_none()),
                "{protocol:?}: {odd:?}"
            );
        }
    }

    #[test]
    fn whole_json_is_complete_so_missing_finish_markers_are_not_connection_lost() {
        // 非流式 Chat 响应没有 finish_reason：响应体是完整的，按正常结束
        let s = whole(
            Protocol::OpenAiChat,
            &serde_json::json!({"choices": [{"message": {"content": "整段"}, "finish_reason": null}]}),
        );
        assert_eq!(text(&s), "整段");
        assert!(matches!(last(&s), Step::Finish { .. }));
        // Gemini 代理忽略 alt=sse 返回 JSON 数组、最后一项没有 finishReason
        let s = whole(
            Protocol::Gemini,
            &serde_json::json!([
                {"candidates": [{"content": {"parts": [{"text": "A"}]}}], "responseId": "g"},
                {"candidates": [{"content": {"parts": [{"text": "B"}]}}]}
            ]),
        );
        assert_eq!(text(&s), "AB");
        assert!(matches!(last(&s), Step::Finish { .. }));
        // 形状对、却什么都没有：protocol
        let s = whole(Protocol::OpenAiChat, &serde_json::json!({"choices": []}));
        assert_eq!(fail_code(&s), Some(ErrorCode::Protocol));
        let s = whole(Protocol::OpenAiChat, &serde_json::json!([]));
        assert_eq!(fail_code(&s), Some(ErrorCode::Protocol));
    }

    #[test]
    fn base_resp_errors_in_the_stream() {
        // 第一个 data 就是错误：没有 Start（client 会补），直接失败
        let s = run(
            Protocol::OpenAiChat,
            "data: {\"id\":\"\",\"choices\":null,\"base_resp\":{\"status_code\":1008,\"status_msg\":\"insufficient balance\"}}\n\n",
        );
        assert_eq!(s.len(), 1);
        assert_eq!(fail_code(&s), Some(ErrorCode::Quota));
        // 输出到一半：已有文本保留，随后 content_filter
        let s = run(
            Protocol::OpenAiChat,
            "data: {\"id\":\"m\",\"choices\":[{\"delta\":{\"content\":\"半\"}}]}\n\n\
             data: {\"id\":\"m\",\"choices\":null,\"base_resp\":{\"status_code\":1027,\"status_msg\":\"output new_sensitive\"}}\n\n",
        );
        assert_eq!(text(&s), "半");
        assert_eq!(fail_code(&s), Some(ErrorCode::ContentFilter));
        // 成功的最后一块带 status_code 0：正常结束
        let s = run(
            Protocol::OpenAiChat,
            "data: {\"id\":\"m\",\"choices\":[{\"delta\":{\"content\":\"好\"}}]}\n\n\
             data: {\"id\":\"m\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"base_resp\":{\"status_code\":0,\"status_msg\":\"\"}}\n\n\
             data: [DONE]\n\n",
        );
        assert_eq!(text(&s), "好");
        assert!(matches!(last(&s), Step::Finish { .. }));
        // 中转站在流里塞了一个 {code, msg}：upstream，而不是等到 EOF 报「连接中断」
        let s = run(
            Protocol::OpenAiChat,
            "data: {\"code\":500,\"msg\":\"upstream channel error\"}\n\n",
        );
        assert!(
            matches!(last(&s), Step::Fail(e) if e.code == ErrorCode::Upstream && e.subcode != Some(ErrorSubcode::ConnectionLost))
        );
    }
}
