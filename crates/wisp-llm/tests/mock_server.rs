//! 场景测试：
//! - wiremock：参数降级（各协议的每条路径）与能力缓存、重试 / Retry-After、模型列表分页、测试连接；
//! - 脚本化服务器：首 token / 空闲超时、取消、分块发送且连接不关闭（首个 Delta 不等响应结束）、断流、结束宽限；
//! - TLS 黑洞：不同 `connect_ms` 的 Provider 各自生效。
//!
//! 测试进程不手动安装 CryptoProvider（stream 与 test_connection 都要能工作）。

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Act, End, Reply, ScriptServer, image_request, provider, request};
use serde_json::json;
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};
use wisp_llm::{
    CapsStore, ErrorCode, ErrorSubcode, MaxTokensField, ModelEntry, NoticeKind, Protocol, Secret, StreamEvent,
    TestVerdict, ThinkingOff, Timeouts, Vision, VisionSource, provider_from_template,
};

const CHAT_OK: &str = "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"OK\"}}]}\n\n\
                       data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                       data: [DONE]\n\n";
const RESPONSES_OK: &str = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n\
                            event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"OK\"}\n\n\
                            event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\"}}\n\n";
const ANTHROPIC_OK: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n\
                            event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
                            event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"OK\"}}\n\n\
                            event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
const GEMINI_OK: &str = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"OK\"}]},\"finishReason\":\"STOP\"}],\"responseId\":\"g\"}\n\n";

fn sse(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "text/event-stream")
}

fn err(status: u16, body: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(body)
}

fn body_has(key: &'static str) -> impl Fn(&Request) -> bool + Send + Sync {
    move |r: &Request| String::from_utf8_lossy(&r.body).contains(&format!("\"{key}\""))
}

fn body_lacks(key: &'static str) -> impl Fn(&Request) -> bool + Send + Sync {
    move |r: &Request| !String::from_utf8_lossy(&r.body).contains(&format!("\"{key}\""))
}

async fn bodies(server: &MockServer) -> Vec<serde_json::Value> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.method.as_str() == "POST")
        .map(|r| r.body_json().expect("json body"))
        .collect()
}

fn notices(events: &[StreamEvent]) -> Vec<NoticeKind> {
    events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Notice { kind } => Some(*kind),
            _ => None,
        })
        .collect()
}

fn template_provider(template: &str, base: &str) -> wisp_llm::Provider {
    let mut p = provider_from_template(template, &[]).expect("template");
    p.base_url = base.to_owned();
    p
}

// ---------------------------------------------------------------------------
// 参数降级与能力缓存

#[tokio::test]
async fn temperature_is_degraded_once_and_remembered() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_has("temperature"))
        .respond_with(err(
            400,
            json!({"error": {"message": "Unsupported value: 'temperature' does not support 0.3 with this model. Only the default (1) value is supported.", "type": "invalid_request_error", "param": "temperature", "code": "unsupported_value"}}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_lacks("temperature"))
        .respond_with(sse(CHAT_OK))
        .mount(&server)
        .await;
    let (client, caps) = common::client();
    let p = template_provider("openai", &format!("{}/v1", server.uri()));

    let events = common::run(&client, request(p.clone(), "gpt-4.1", "hi")).await;
    assert_eq!(common::text_of(&events), "OK");
    assert_eq!(
        notices(&events),
        vec![NoticeKind::ParamDegraded],
        "成功后弱提示「已自动调整参数重试」"
    );
    let sent = bodies(&server).await;
    assert_eq!(sent.len(), 2);
    assert!(sent[0].get("temperature").is_some() && sent[1].get("temperature").is_none());
    // 模板生成的 Provider id 是 openai
    assert_eq!(caps.get("openai", "gpt-4.1").temperature, Some(false), "写入能力缓存");

    // 第二次只发一次请求
    let events = common::run(&client, request(p.clone(), "gpt-4.1", "hi")).await;
    assert!(notices(&events).is_empty());
    assert_eq!(bodies(&server).await.len(), 3);

    // 「重启后」：能力缓存已经持久化在 ModelEntry.caps 里，新的客户端第一次就不发 temperature
    let (fresh, _) = client_fresh();
    let mut req = request(p, "gpt-4.1", "hi");
    req.model.caps.temperature = Some(false);
    let _ = common::run(&fresh, req).await;
    assert_eq!(bodies(&server).await.len(), 4);
}

fn client_fresh() -> (wisp_llm::LlmClient, Arc<wisp_llm::MemoryCapsStore>) {
    common::client()
}

#[tokio::test]
async fn max_tokens_field_is_swapped_not_removed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_has("max_tokens"))
        .respond_with(err(
            400,
            json!({"error": {"message": "Unsupported parameter: 'max_tokens' is not supported with this model. Use 'max_completion_tokens' instead.", "type": "invalid_request_error", "param": "max_tokens", "code": "unsupported_parameter"}}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_has("max_completion_tokens"))
        .respond_with(sse(CHAT_OK))
        .mount(&server)
        .await;
    let (client, caps) = common::client();
    let mut p = provider(Protocol::OpenAiChat, &format!("{}/v1", server.uri()));
    p.quirks.max_tokens_field = Some(MaxTokensField::MaxTokens);
    let events = common::run(&client, request(p, "o1-relay-alias", "hi")).await;
    assert!(matches!(common::last(&events), StreamEvent::Done { .. }));
    let sent = bodies(&server).await;
    assert_eq!(sent[1]["max_completion_tokens"], sent[0]["max_tokens"]);
    assert_eq!(
        caps.get("test", "o1-relay-alias").max_tokens_field,
        Some(MaxTokensField::MaxCompletionTokens)
    );
}

#[tokio::test]
async fn openai_stream_options_and_effort_paths() {
    // stream_options 被拒 → 去掉
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_has("stream_options"))
        .respond_with(err(
            400,
            json!({"error": {"message": "Unknown parameter: 'stream_options'."}}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(sse(CHAT_OK))
        .mount(&server)
        .await;
    let (client, caps) = common::client();
    let p = provider(Protocol::OpenAiChat, &format!("{}/v1", server.uri()));
    let _ = common::run(&client, request(p, "llama-3", "hi")).await;
    assert!(bodies(&server).await[1].get("stream_options").is_none());
    assert_eq!(caps.get("test", "llama-3").stream_options, Some(false));

    // reasoning_effort 被拒 → 去掉
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_has("reasoning_effort"))
        .respond_with(err(400, json!({"error": {"message": "Unsupported parameter: 'reasoning_effort' is not supported with this model.", "param": "reasoning_effort"}})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(sse(CHAT_OK))
        .mount(&server)
        .await;
    let (client, caps) = common::client();
    let p = template_provider("openai", &format!("{}/v1", server.uri()));
    let _ = common::run(&client, request(p, "o4-mini", "hi")).await;
    let sent = bodies(&server).await;
    assert_eq!(sent[0]["reasoning_effort"], "low");
    assert!(sent[1].get("reasoning_effort").is_none());
    assert_eq!(caps.get("openai", "o4-mini").effort_param, Some(false));

    // Responses 的 reasoning.effort 被拒 → 去掉 reasoning
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_has("reasoning"))
        .respond_with(err(400, json!({"error": {"message": "Unsupported parameter: 'reasoning.effort' is not supported with this model.", "param": "reasoning.effort"}})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(sse(RESPONSES_OK))
        .mount(&server)
        .await;
    let (client, _) = common::client();
    let p = template_provider("openai-responses", &format!("{}/v1", server.uri()));
    let events = common::run(&client, request(p, "gpt-5.4-mini", "hi")).await;
    assert_eq!(common::text_of(&events), "OK");
    assert!(bodies(&server).await[1].get("reasoning").is_none());
}

#[tokio::test]
async fn compat_thinking_fields_are_removed() {
    for (template, field) in [
        ("deepseek", "thinking"),
        ("siliconflow", "enable_thinking"),
        ("moonshot", "reasoning_effort"),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_has(field))
            .respond_with(err(
                400,
                json!({"error": {"message": format!("Unrecognized request argument supplied: {field}")}}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(sse(CHAT_OK))
            .mount(&server)
            .await;
        let (client, caps) = common::client();
        let p = template_provider(template, &format!("{}/v1", server.uri()));
        let events = common::run(&client, request(p, "some-model", "hi")).await;
        assert_eq!(common::text_of(&events), "OK", "{template}");
        let sent = bodies(&server).await;
        assert!(
            sent[0].get(field).is_some() && sent[1].get(field).is_none(),
            "{template}"
        );
        assert_eq!(
            caps.get(template, "some-model").thinking_param,
            Some(false),
            "{template}"
        );
    }
}

#[tokio::test]
async fn anthropic_degrade_paths() {
    // thinking:{type:disabled} 被拒 → 去掉 thinking（max_tokens 不能去掉）
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_has("thinking"))
        .respond_with(err(
            400,
            json!({"type": "error", "error": {"type": "invalid_request_error", "message": "thinking.type: 'disabled' is not supported for this model"}}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(sse(ANTHROPIC_OK))
        .mount(&server)
        .await;
    let (client, caps) = common::client();
    let p = template_provider("anthropic", &server.uri());
    let mut req = request(p, "claude-opus-5-5", "hi");
    req.model.caps.anthropic_thinking_disabled = Some(true);
    let events = common::run(&client, req).await;
    assert_eq!(common::text_of(&events), "OK");
    let sent = bodies(&server).await;
    assert!(sent[0].get("thinking").is_some() && sent[1].get("thinking").is_none());
    assert!(sent[1].get("max_tokens").is_some());
    assert_eq!(caps.get("anthropic", "claude-opus-5-5").thinking_param, Some(false));

    // output_config 被拒 → 去掉
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_has("output_config"))
        .respond_with(err(
            400,
            json!({"type": "error", "error": {"type": "invalid_request_error", "message": "output_config: Extra inputs are not permitted"}}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(sse(ANTHROPIC_OK))
        .mount(&server)
        .await;
    let (client, _) = common::client();
    let p = template_provider("anthropic", &server.uri());
    let events = common::run(&client, request(p, "relay-claude", "hi")).await;
    assert_eq!(common::text_of(&events), "OK");
    assert!(bodies(&server).await[1].get("output_config").is_none());
}

#[tokio::test]
async fn gemini_thinking_config_is_removed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_has("thinkingConfig"))
        .respond_with(err(
            400,
            json!({"error": {"code": 400, "message": "Thinking level is not supported for this model.", "status": "INVALID_ARGUMENT"}}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(sse(GEMINI_OK))
        .mount(&server)
        .await;
    let (client, caps) = common::client();
    let p = template_provider("gemini", &server.uri());
    let events = common::run(&client, request(p, "gemini-3-flash-preview", "hi")).await;
    assert_eq!(common::text_of(&events), "OK");
    let sent = bodies(&server).await;
    assert!(sent[0]["generationConfig"].get("thinkingConfig").is_some());
    assert!(sent[1]["generationConfig"].get("thinkingConfig").is_none());
    assert_eq!(caps.get("gemini", "gemini-3-flash-preview").thinking_param, Some(false));
}

#[tokio::test]
async fn at_most_one_degrade_per_request() {
    // 同一个参数降级后仍被拒：最多 2 次调用，不重复写缓存，不可重试；终止事件是会出卡片的 upstream
    // （param_unsupported 只以降级成功后的弱提示出现）
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(err(
            400,
            json!({"error": {"message": "Unsupported value: 'temperature' does not support 0.3"}}),
        ))
        .mount(&server)
        .await;
    let (client, caps) = common::client();
    let p = provider(Protocol::OpenAiChat, &format!("{}/v1", server.uri()));
    let events = common::run(&client, request(p, "gpt-4.1", "hi")).await;
    let e = common::error_of(&events);
    assert_eq!(
        (e.code, e.hint.as_deref(), e.status),
        (ErrorCode::Upstream, Some("errors.hint.param_rejected"), Some(400))
    );
    assert_eq!(bodies(&server).await.len(), 2);
    assert!(!e.retryable);
    assert_eq!(caps.get("test", "gpt-4.1").temperature, None, "降级没有成功，不写缓存");

    // 降级后又被拒了另一个参数：两次结论都写进缓存，下一次一次就成功
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_has("temperature"))
        .respond_with(err(
            400,
            json!({"error": {"message": "Unsupported parameter: 'temperature'"}}),
        ))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_has("thinking"))
        .respond_with(err(400, json!({"error": {"message": "Unknown parameter: 'thinking'"}})))
        .with_priority(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(sse(CHAT_OK))
        .with_priority(3)
        .mount(&server)
        .await;
    let (client, caps) = common::client();
    let p = template_provider("deepseek", &format!("{}/v1", server.uri()));
    let events = common::run(&client, request(p.clone(), "deepseek-flash", "hi")).await;
    let e = common::error_of(&events);
    assert_eq!(
        (e.code, e.retryable, e.hint.as_deref()),
        (ErrorCode::Upstream, true, Some("errors.hint.param_adjusted"))
    );
    let stored = caps.get("deepseek", "deepseek-flash");
    assert_eq!((stored.temperature, stored.thinking_param), (Some(false), Some(false)));
    let events = common::run(&client, request(p, "deepseek-flash", "hi")).await;
    assert_eq!(common::text_of(&events), "OK");
    assert_eq!(bodies(&server).await.len(), 3);
}

// ---------------------------------------------------------------------------
// 重试

#[tokio::test]
async fn retry_once_on_503_then_succeed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(err(503, json!({"error": {"message": "overloaded"}})))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(sse(CHAT_OK))
        .with_priority(2)
        .mount(&server)
        .await;
    let (client, _) = common::client();
    let t0 = Instant::now();
    let events = common::run(
        &client,
        request(provider(Protocol::OpenAiChat, &server.uri()), "gpt-4.1", "hi"),
    )
    .await;
    let elapsed = t0.elapsed();
    assert_eq!(common::text_of(&events), "OK");
    assert_eq!(notices(&events), vec![NoticeKind::Retried]);
    assert_eq!(bodies(&server).await.len(), 2);
    assert!(elapsed >= Duration::from_millis(390), "退避 500 ms ±20%: {elapsed:?}");
}

#[tokio::test]
async fn retry_after_header_is_honoured_when_short() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(err(503, json!({"error": {"message": "busy"}})).insert_header("retry-after", "1"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(sse(CHAT_OK))
        .with_priority(2)
        .mount(&server)
        .await;
    let (client, _) = common::client();
    let t0 = Instant::now();
    let events = common::run(
        &client,
        request(provider(Protocol::OpenAiChat, &server.uri()), "gpt-4.1", "hi"),
    )
    .await;
    assert_eq!(common::text_of(&events), "OK");
    let elapsed = t0.elapsed();
    assert!(
        elapsed >= Duration::from_millis(950) && elapsed < Duration::from_secs(4),
        "{elapsed:?}"
    );
}

#[tokio::test]
async fn persistent_503_retries_once_then_fails() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(err(503, json!({"error": {"message": "busy"}})))
        .mount(&server)
        .await;
    let (client, _) = common::client();
    let events = common::run(
        &client,
        request(provider(Protocol::OpenAiChat, &server.uri()), "gpt-4.1", "hi"),
    )
    .await;
    let e = common::error_of(&events);
    assert_eq!((e.code, e.retryable), (ErrorCode::Upstream, true));
    assert_eq!(bodies(&server).await.len(), 2);
    // 流没有开始：Start 之后直接是 Error，不补 Notice{retried}
    common::assert_failed_before_stream(&events);
}

#[tokio::test]
async fn connection_failure_before_headers_is_retried() {
    let server = ScriptServer::start(vec![Reply::drop_connection(), Reply::sse_text(CHAT_OK)]).await;
    let (client, _) = common::client();
    let events = common::run(
        &client,
        request(provider(Protocol::OpenAiChat, &server.base), "gpt-4.1", "hi"),
    )
    .await;
    assert_eq!(common::text_of(&events), "OK");
    assert_eq!(notices(&events), vec![NoticeKind::Retried]);
    assert_eq!(server.request_count(), 2);
}

#[tokio::test]
async fn rate_limit_is_not_retried_automatically() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(err(429, json!({"error": {"message": "Rate limit"}})).insert_header("retry-after", "2"))
        .mount(&server)
        .await;
    let (client, _) = common::client();
    let events = common::run(
        &client,
        request(provider(Protocol::OpenAiChat, &server.uri()), "gpt-4.1", "hi"),
    )
    .await;
    let e = common::error_of(&events);
    assert_eq!((e.code, e.retry_after_s), (ErrorCode::RateLimited, Some(2)));
    assert_eq!(bodies(&server).await.len(), 1);
}

// ---------------------------------------------------------------------------
// 超时

fn short(first_token_ms: u32, idle_ms: u32) -> Timeouts {
    Timeouts {
        connect_ms: 2_000,
        first_token_ms,
        idle_ms,
    }
}

#[tokio::test]
async fn pings_do_not_reset_the_first_token_timer() {
    let ping = b"event: ping\ndata: {\"type\": \"ping\"}\n\n".to_vec();
    let mut acts = vec![Act::Send(
        b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\"}}\n\n".to_vec(),
    )];
    for _ in 0..60 {
        acts.push(Act::Sleep(Duration::from_millis(50)));
        acts.push(Act::Send(ping.clone()));
    }
    let server = ScriptServer::start(vec![Reply::sse(Vec::new()).acts(acts).end(End::Hold)]).await;
    let mut p = provider(Protocol::AnthropicMessages, &server.base);
    p.timeouts = short(400, 5_000);
    let (client, _) = common::client();
    let t0 = Instant::now();
    let events = common::run(&client, request(p, "claude-haiku-4-5", "hi")).await;
    let elapsed = t0.elapsed();
    let e = common::error_of(&events);
    assert_eq!(e.code, ErrorCode::Timeout);
    assert_eq!(e.hint.as_deref(), Some("errors.hint.adjust_timeout"));
    assert!(e.retryable);
    assert!(
        elapsed < Duration::from_millis(2_000),
        "只有 ping 时按首 token 超时，而不是空闲超时: {elapsed:?}"
    );
    assert!(server.wait_closed(Duration::from_secs(2)).await, "超时后连接被关闭");
}

#[tokio::test]
async fn idle_timeout_after_first_token_keeps_text() {
    let server = ScriptServer::start(vec![
        Reply::sse_text("data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"半句\"}}]}\n\n")
            .end(End::Hold),
    ])
    .await;
    let mut p = provider(Protocol::OpenAiChat, &server.base);
    p.timeouts = short(5_000, 300);
    let (client, _) = common::client();
    let t0 = Instant::now();
    let events = common::run(&client, request(p, "gpt-4.1", "hi")).await;
    assert_eq!(common::text_of(&events), "半句", "已发出的文本保留");
    assert_eq!(common::error_of(&events).code, ErrorCode::Timeout);
    assert!(t0.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn header_wait_is_bounded() {
    let server = ScriptServer::start(vec![Reply::sse_text(CHAT_OK).head_delay(Duration::from_secs(30))]).await;
    let mut p = provider(Protocol::OpenAiChat, &server.base);
    p.timeouts = Timeouts {
        connect_ms: 200,
        first_token_ms: 300,
        idle_ms: 60_000,
    };
    let (client, _) = common::client();
    let t0 = Instant::now();
    let events = common::run(&client, request(p, "gpt-4.1", "hi")).await;
    assert_eq!(common::error_of(&events).code, ErrorCode::Timeout);
    assert!(t0.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn finish_reason_without_done_finishes_after_grace() {
    // 看到 finish_reason 之后最多再等 2 s 的 [DONE] / usage / EOF
    let server = ScriptServer::start(vec![
        Reply::sse_text(
            "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":\"stop\"}]}\n\n",
        )
        .end(End::Hold),
    ])
    .await;
    let (client, _) = common::client();
    let t0 = Instant::now();
    let events = common::run(
        &client,
        request(provider(Protocol::OpenAiChat, &server.base), "gpt-4.1", "hi"),
    )
    .await;
    let elapsed = t0.elapsed();
    assert!(matches!(common::last(&events), StreamEvent::Done { .. }));
    assert_eq!(common::text_of(&events), "Hi");
    assert!(
        elapsed >= Duration::from_millis(1_900) && elapsed < Duration::from_secs(4),
        "{elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// 取消与逐块处理

const ALL_PROTOCOLS: [Protocol; 4] = [
    Protocol::OpenAiChat,
    Protocol::OpenAiResponses,
    Protocol::AnthropicMessages,
    Protocol::Gemini,
];

/// 各协议的一段文本增量（`text` 不含需要 JSON 转义的字符）
fn piece(protocol: Protocol, text: &str) -> String {
    match protocol {
        Protocol::OpenAiChat => {
            format!("data: {{\"id\":\"c\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\n")
        }
        Protocol::OpenAiResponses => format!(
            "event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"{text}\"}}\n\n"
        ),
        Protocol::AnthropicMessages => format!(
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n"
        ),
        Protocol::Gemini => format!(
            "data: {{\"candidates\":[{{\"content\":{{\"parts\":[{{\"text\":\"{text}\"}}]}}}}],\"responseId\":\"g\"}}\n\n"
        ),
    }
}

/// 流开始（各协议的开头事件）+ 第一段文本，不含结束标记
fn opening(protocol: Protocol, text: &str) -> String {
    let head = match protocol {
        Protocol::OpenAiChat | Protocol::Gemini => "",
        Protocol::OpenAiResponses => {
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n"
        }
        Protocol::AnthropicMessages => {
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n\
             event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n"
        }
    };
    format!("{head}{}", piece(protocol, text))
}

/// 最后一段文本 + 结束标记
fn closing(protocol: Protocol, text: &str) -> String {
    let tail = match protocol {
        Protocol::OpenAiChat => {
            "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
        }
        Protocol::OpenAiResponses => {
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\"}}\n\n"
        }
        Protocol::AnthropicMessages => {
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n\
             event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        }
        Protocol::Gemini => {
            "data: {\"candidates\":[{\"content\":{\"parts\":[]},\"finishReason\":\"STOP\"}],\"responseId\":\"g\"}\n\n"
        }
    };
    format!("{}{tail}", piece(protocol, text))
}

#[tokio::test]
async fn cancel_mid_stream_is_immediate_and_closes_the_connection() {
    // 四种协议各自在流中途取消
    for protocol in ALL_PROTOCOLS {
        let server = ScriptServer::start(vec![Reply::sse_text(&opening(protocol, "Hel")).end(End::Hold)]).await;
        let (client, _) = common::client();
        let cancel = CancellationToken::new();
        let mut rx = client.stream(request(provider(protocol, &server.base), "m", "hi"), cancel.clone());
        assert!(
            matches!(rx.recv().await, Some(StreamEvent::Start { .. })),
            "{protocol:?}"
        );
        assert!(
            matches!(rx.recv().await, Some(StreamEvent::Delta { text }) if text == "Hel"),
            "{protocol:?}"
        );
        let t0 = Instant::now();
        cancel.cancel();
        let ev = tokio::time::timeout(Duration::from_millis(50), rx.recv())
            .await
            .unwrap_or_else(|_| panic!("{protocol:?}: cancelled within 50 ms"));
        assert!(
            matches!(ev, Some(StreamEvent::Error(e)) if e.code == ErrorCode::Cancelled && e.status.is_none()),
            "{protocol:?}"
        );
        assert!(t0.elapsed() < Duration::from_millis(50), "{protocol:?}");
        assert!(rx.recv().await.is_none(), "{protocol:?}: 取消之后不再有任何事件");
        assert!(
            server.wait_closed(Duration::from_secs(1)).await,
            "{protocol:?}: 服务器观察到连接关闭"
        );
    }
}

#[tokio::test]
async fn cancel_releases_the_connection_even_if_the_consumer_never_reads() {
    // 消费方一个事件都不读：通道被填满，wisp-llm 阻塞在发送上。取消后必须先释放响应体（服务器立刻观察到连接关闭、
    // 上游停止生成），终止事件 Error{cancelled} 留在后面等消费方来取
    for protocol in ALL_PROTOCOLS {
        let mut body = opening(protocol, "d0 ");
        for i in 1..300 {
            body.push_str(&piece(protocol, &format!("d{i} ")));
        }
        let server = ScriptServer::start(vec![Reply::sse_text(&body).end(End::Hold)]).await;
        let (client, _) = common::client();
        let cancel = CancellationToken::new();
        let rx = client.stream(request(provider(protocol, &server.base), "m", "hi"), cancel.clone());
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel();
        assert!(
            server.wait_closed(Duration::from_millis(500)).await,
            "{protocol:?}: 消费方不读时，取消后连接仍未关闭"
        );
        let events = common::collect(rx).await;
        assert!(
            matches!(common::last(&events), StreamEvent::Error(e) if e.code == ErrorCode::Cancelled),
            "{protocol:?}"
        );
        let deltas = events.iter().filter(|e| matches!(e, StreamEvent::Delta { .. })).count();
        assert!(deltas < CHANNEL_CAPACITY, "{protocol:?}: {deltas}");
    }
}

#[tokio::test]
async fn cancel_before_headers() {
    let server = ScriptServer::start(vec![Reply::sse_text(CHAT_OK).head_delay(Duration::from_secs(10))]).await;
    let (client, _) = common::client();
    let cancel = CancellationToken::new();
    let rx = client.stream(
        request(provider(Protocol::OpenAiChat, &server.base), "gpt-4.1", "hi"),
        cancel.clone(),
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    let t0 = Instant::now();
    cancel.cancel();
    let events = common::collect(rx).await;
    assert!(t0.elapsed() < Duration::from_millis(50));
    common::assert_failed_before_stream(&events);
    assert_eq!(common::error_of(&events).code, ErrorCode::Cancelled);
}

#[tokio::test]
async fn first_delta_arrives_before_the_response_ends() {
    // 分块发送且连接不关闭。服务器写出第一个完整事件后 ≤ 50 ms 内必须收到首个 Delta，不等响应结束
    for protocol in ALL_PROTOCOLS {
        let go = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let server = ScriptServer::start(vec![Reply::sse(Vec::new()).acts(vec![
            Act::Wait(go.clone()),
            Act::Send(opening(protocol, "你").into_bytes()),
            Act::Wait(release.clone()),
            Act::Send(closing(protocol, "好").into_bytes()),
        ])])
        .await;
        let (client, _) = common::client();
        let mut rx = client.stream(
            request(provider(protocol, &server.base), "m", "hi"),
            CancellationToken::new(),
        );
        // 让客户端先拿到响应头、开始等响应体
        tokio::time::sleep(Duration::from_millis(100)).await;
        let t0 = Instant::now();
        go.notify_one();
        let first = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match rx.recv().await {
                    Some(StreamEvent::Delta { text }) => return text,
                    Some(_) => {}
                    None => panic!("{protocol:?}: stream ended early"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{protocol:?}: first delta while the server is still holding the response"));
        let elapsed = t0.elapsed();
        assert_eq!(first, "你", "{protocol:?}");
        assert!(
            elapsed <= Duration::from_millis(50),
            "{protocol:?}: 首个事件写出后 {elapsed:?} 才收到 Delta"
        );
        release.notify_one();
        let rest = common::collect(rx).await;
        assert_eq!(common::text_of(&rest), "好", "{protocol:?}");
        assert!(matches!(common::last(&rest), StreamEvent::Done { .. }), "{protocol:?}");
    }
}

#[tokio::test]
async fn connection_lost_mid_stream_is_not_retried() {
    let server = ScriptServer::start(vec![
        Reply::sse_text("data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"一半\"}}]}\n\n")
            .end(End::Abort),
        Reply::sse_text(CHAT_OK),
    ])
    .await;
    let (client, _) = common::client();
    let events = common::run(
        &client,
        request(provider(Protocol::OpenAiChat, &server.base), "gpt-4.1", "hi"),
    )
    .await;
    assert_eq!(common::text_of(&events), "一半");
    let e = common::error_of(&events);
    assert_eq!(
        (e.code, e.subcode, e.retryable),
        (ErrorCode::Network, Some(ErrorSubcode::ConnectionLost), true)
    );
    assert_eq!(server.request_count(), 1, "流开始后不再自动重试");
}

#[tokio::test]
async fn dropping_the_receiver_aborts_the_request() {
    let server = ScriptServer::start(vec![
        Reply::sse_text("data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"}}]}\n\n")
            .end(End::Hold),
    ])
    .await;
    let (client, _) = common::client();
    let mut rx = client.stream(
        request(provider(Protocol::OpenAiChat, &server.base), "gpt-4.1", "hi"),
        CancellationToken::new(),
    );
    assert!(matches!(rx.recv().await, Some(StreamEvent::Start { .. })));
    drop(rx);
    assert!(server.wait_closed(Duration::from_secs(1)).await);
}

/// wisp-llm 的事件通道容量（client.rs 的 CHANNEL_CAPACITY）
const CHANNEL_CAPACITY: usize = 64;

#[tokio::test]
async fn cancel_under_backpressure_sends_nothing_more() {
    // 服务器一口气发 300 个增量，消费方读了 Start 之后停下来（通道被填满，wisp-llm 阻塞在发送上），然后取消：
    // 取消之后不能再有新的 Delta 进入通道——收到的只能是取消之前就已经在通道里的（≤ 通道容量），然后是 Error{cancelled}
    let mut body = String::new();
    for i in 0..300 {
        body.push_str(&format!(
            "data: {{\"id\":\"c\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"d{i} \"}}}}]}}\n\n"
        ));
    }
    body.push_str(
        "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
    );
    let server = ScriptServer::start(vec![Reply::sse_text(&body)]).await;
    let (client, _) = common::client();
    let cancel = CancellationToken::new();
    let mut rx = client.stream(
        request(provider(Protocol::OpenAiChat, &server.base), "gpt-4.1", "hi"),
        cancel.clone(),
    );
    assert!(matches!(rx.recv().await, Some(StreamEvent::Start { .. })));
    tokio::time::sleep(Duration::from_millis(300)).await;
    cancel.cancel();
    let after = common::collect(rx).await;
    let deltas = after.iter().filter(|e| matches!(e, StreamEvent::Delta { .. })).count();
    assert!(
        deltas <= CHANNEL_CAPACITY,
        "取消后又发出了 {deltas} 个 Delta（通道容量 {CHANNEL_CAPACITY}）"
    );
    assert!(matches!(common::last(&after), StreamEvent::Error(e) if e.code == ErrorCode::Cancelled));
    // 收到的增量是连续的前缀：没有任何一个是取消之后才放进通道的
    let expected: String = (0..deltas).map(|i| format!("d{i} ")).collect();
    assert_eq!(common::text_of(&after), expected);
}

#[tokio::test]
async fn channel_closes_right_after_done_even_if_the_response_stays_open() {
    // 服务器发完 [DONE] 却不结束响应（中转站常见）：Done 之后接收端应立即收到 None，而不是等收尾读取的 1 s
    let server = ScriptServer::start(vec![Reply::sse_text(CHAT_OK).end(End::Hold)]).await;
    let (client, _) = common::client();
    let mut rx = client.stream(
        request(provider(Protocol::OpenAiChat, &server.base), "gpt-4.1", "hi"),
        CancellationToken::new(),
    );
    loop {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("event")
        {
            Some(StreamEvent::Done { .. }) => break,
            Some(_) => {}
            None => panic!("closed before Done"),
        }
    }
    let t0 = Instant::now();
    let next = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("closed");
    assert!(next.is_none());
    assert!(
        t0.elapsed() < Duration::from_millis(200),
        "Done 之后 {:?} 才关闭通道",
        t0.elapsed()
    );
}

// ---------------------------------------------------------------------------
// 连接超时按 Provider 生效

#[tokio::test]
async fn connect_timeout_is_per_provider() {
    let base = common::tls_blackhole().await;
    let (client, _) = common::client();
    let run_with = |connect_ms: u32| {
        let mut p = provider(Protocol::OpenAiChat, &format!("{base}/v1"));
        p.timeouts.connect_ms = connect_ms;
        let client = client.clone();
        async move {
            let t0 = Instant::now();
            let events = common::run(&client, request(p, "gpt-4.1", "hi")).await;
            (t0.elapsed(), common::error_of(&events).clone())
        }
    };
    let ((fast, e1), (slow, e2)) = tokio::join!(run_with(300), run_with(1_500));
    for e in [&e1, &e2] {
        assert_eq!(e.code, ErrorCode::Network);
        assert_eq!(e.message, "connection timed out");
    }
    // 连接失败会自动重试一次（500 ms ±20% 退避）：300 ms × 2 + 退避 ≈ 1 s；1500 ms × 2 + 退避 ≈ 3.5 s
    assert!(
        fast >= Duration::from_millis(900) && fast < Duration::from_millis(2_200),
        "fast {fast:?}"
    );
    assert!(
        slow >= Duration::from_millis(3_300) && slow < Duration::from_millis(5_500),
        "slow {slow:?}"
    );
}

// ---------------------------------------------------------------------------
// 视觉的运行时反馈、complete()、未配置

#[tokio::test]
async fn ocr_runtime_feedback_writes_vision() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(CHAT_OK))
        .mount(&server)
        .await;
    let (client, caps) = common::client();
    let p = provider(Protocol::OpenAiChat, &format!("{}/v1", server.uri()));
    let _ = common::run(&client, image_request(p.clone(), "relay-alias")).await;
    assert_eq!(
        caps.vision("test", "relay-alias"),
        Some((Vision::Yes, VisionSource::Runtime))
    );

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(err(
            400,
            json!({"error": {"message": "This model does not support image input."}}),
        ))
        .mount(&server)
        .await;
    let p = provider(Protocol::OpenAiChat, &format!("{}/v1", server.uri()));
    let events = common::run(&client, image_request(p.clone(), "text-only")).await;
    assert_eq!(common::error_of(&events).code, ErrorCode::NoVision);
    assert_eq!(
        caps.vision("test", "text-only"),
        Some((Vision::No, VisionSource::Runtime))
    );

    // 用户手动设置的不覆盖
    let mut req = image_request(p, "user-set");
    req.model.vision = Vision::Yes;
    req.model.vision_source = Some(VisionSource::User);
    let _ = common::run(&client, req).await;
    assert_eq!(caps.vision("test", "user-set"), None);
}

#[tokio::test]
async fn image_errors_only_mark_text_only_on_a_clear_signal() {
    let (client, caps) = common::client();
    let run_with =
        |status: u16, body: serde_json::Value, model: &'static str, vision: Vision, source: Option<VisionSource>| {
            let client = client.clone();
            async move {
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .respond_with(err(status, body))
                    .mount(&server)
                    .await;
                let protocol = if model.starts_with("gemini") {
                    Protocol::Gemini
                } else {
                    Protocol::OpenAiChat
                };
                let base = if protocol == Protocol::Gemini {
                    format!("{}/v1beta", server.uri())
                } else {
                    format!("{}/v1", server.uri())
                };
                let mut req = image_request(provider(protocol, &base), model);
                req.model.vision = vision;
                req.model.vision_source = source;
                let events = common::run(&client, req).await;
                common::error_of(&events).clone()
            }
        };

    // Gemini 单张图片处理失败（建议重试）：upstream、可重试，不碰视觉能力
    let e = run_with(
        400,
        json!({"error": {"code": 400, "message": "Unable to process input image. Please retry or report in https://developers.generativeai.google/guide/troubleshooting", "status": "INVALID_ARGUMENT"}}),
        "gemini-3-flash",
        Vision::Yes,
        Some(VisionSource::Capability),
    )
    .await;
    assert_eq!((e.code, e.retryable), (ErrorCode::Upstream, true));
    assert_eq!(caps.vision("test", "gemini-3-flash"), None);

    // 只提到图片的含糊错误：仍出 no_vision 卡片，但不覆盖能力字段给出的 Yes
    let vague = json!({"error": {"message": "messages[0].content[0].image_url: extra fields not permitted"}});
    let e = run_with(
        400,
        vague.clone(),
        "relay-vision",
        Vision::Yes,
        Some(VisionSource::Capability),
    )
    .await;
    assert_eq!(e.code, ErrorCode::NoVision);
    assert_eq!(caps.vision("test", "relay-vision"), None);
    // ……对还不知道的模型照常记为 No
    let e = run_with(400, vague, "relay-unknown", Vision::Unknown, None).await;
    assert_eq!(e.code, ErrorCode::NoVision);
    assert_eq!(
        caps.vision("test", "relay-unknown"),
        Some((Vision::No, VisionSource::Runtime))
    );

    // 明确的「不支持图片输入」：即使能力字段说支持也以运行时为准
    let e = run_with(
        400,
        json!({"error": {"message": "Failed to deserialize the JSON body into the target type: messages[1]: unknown variant `image_url`, expected `text`"}}),
        "deepseek-chat",
        Vision::Yes,
        Some(VisionSource::Capability),
    )
    .await;
    assert_eq!(e.code, ErrorCode::NoVision);
    assert_eq!(
        caps.vision("test", "deepseek-chat"),
        Some((Vision::No, VisionSource::Runtime))
    );
}

#[tokio::test]
async fn complete_aggregates_text_and_usage() {
    let server = MockServer::start().await;
    let body = "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"}}]}\n\n\
                data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\", world\"},\"finish_reason\":\"stop\"}]}\n\n\
                data: {\"id\":\"c\",\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":4,\"total_tokens\":7}}\n\n\
                data: [DONE]\n\n";
    Mock::given(method("POST")).respond_with(sse(body)).mount(&server).await;
    let (client, _) = common::client();
    let p = provider(Protocol::OpenAiChat, &server.uri());
    let c = client
        .complete(request(p, "gpt-4.1", "hi"), CancellationToken::new())
        .await
        .expect("completion");
    assert_eq!(c.text, "Hello, world");
    assert_eq!(c.usage.map(|u| u.total_tokens), Some(Some(7)));
}

#[tokio::test]
async fn missing_configuration_is_reported_without_a_request() {
    let (client, _) = common::client();
    let events = common::run(&client, request(provider(Protocol::OpenAiChat, "  "), "gpt-4.1", "hi")).await;
    let e = common::error_of(&events);
    assert_eq!(e.code, ErrorCode::NotConfigured);
    assert_eq!(e.hint.as_deref(), Some("errors.hint.check_settings"));
}

// ---------------------------------------------------------------------------
// 模型列表

#[tokio::test]
async fn anthropic_models_paginate_with_after_id() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(query_param("limit", "1000"))
        .and(query_param_is_missing("after_id"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "claude-opus-5-5", "display_name": "Claude Opus 5.5", "capabilities": {"image_input": {"supported": true}}}],
            "has_more": true, "last_id": "claude-opus-5-5"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(query_param("after_id", "claude-opus-5-5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "claude-2", "lifecycle": "retired"}, {"id": "claude-haiku-4-5"}],
            "has_more": false, "last_id": "claude-haiku-4-5"
        })))
        .mount(&server)
        .await;
    let (client, _) = common::client();
    let mut p = template_provider("anthropic", &server.uri());
    p.headers.insert("anthropic-beta".to_owned(), "x-2026".to_owned());
    let key = Secret::new("sk-ant".to_owned());
    let models = client.list_models(&p, Some(&key)).await.expect("models");
    assert_eq!(
        models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["claude-opus-5-5", "claude-2", "claude-haiku-4-5"]
    );
    assert!(models[1].retired, "retired 在客户端过滤（merge_models 丢弃）");
    let reqs = server.received_requests().await.unwrap_or_default();
    assert_eq!(reqs.len(), 2);
    for r in &reqs {
        assert_eq!(r.headers.get("x-api-key").and_then(|v| v.to_str().ok()), Some("sk-ant"));
        assert_eq!(
            r.headers.get("anthropic-version").and_then(|v| v.to_str().ok()),
            Some("2023-06-01")
        );
        assert_eq!(
            r.headers.get("anthropic-beta").and_then(|v| v.to_str().ok()),
            Some("x-2026"),
            "自定义头也用于 /models"
        );
        assert!(r.url.query().is_some_and(|q| !q.contains("lifecycle")));
    }
}

#[tokio::test]
async fn gemini_models_paginate_and_stop_after_five_pages() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1beta/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [{"name": "models/gemini-3-flash-preview", "supportedGenerationMethods": ["generateContent"]}],
            "nextPageToken": "again"
        })))
        .mount(&server)
        .await;
    let (client, _) = common::client();
    let p = template_provider("gemini", &server.uri());
    let models = client.list_models(&p, None).await.expect("models");
    assert_eq!(models.len(), 5);
    let reqs = server.received_requests().await.unwrap_or_default();
    assert_eq!(reqs.len(), 5, "最多 5 页");
    assert!(reqs[0].url.query() == Some("pageSize=1000"));
    assert!(reqs[1].url.query().is_some_and(|q| q.contains("pageToken=again")));
}

#[tokio::test]
async fn model_list_errors_are_normalised() {
    // DeepSeek 没有 Key：/models 返回纯文本 → auth
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401).set_body_raw("Authentication Fails (governor)", "text/plain"))
        .mount(&server)
        .await;
    let (client, _) = common::client();
    let e = client
        .list_models(&template_provider("deepseek", &server.uri()), None)
        .await
        .expect_err("auth");
    assert_eq!(e.code, ErrorCode::Auth);
    assert_eq!(e.message, "Authentication Fails (governor)");

    // /models 不存在 → not_found（界面允许手动添加模型 id）
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let e = client
        .list_models(&template_provider("minimax", &server.uri()), None)
        .await
        .expect_err("404");
    assert_eq!(e.code, ErrorCode::NotFound);
}

// ---------------------------------------------------------------------------
// 测试连接

async fn mount_models(server: &MockServer, protocol: Protocol) {
    let body = match protocol {
        Protocol::Gemini => {
            json!({"models": [{"name": "models/gemini-2.5-flash", "supportedGenerationMethods": ["generateContent"]}]})
        }
        _ => json!({"data": [{"id": "text-embedding-3-small"}, {"id": "gpt-4o"}]}),
    };
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

#[tokio::test]
async fn test_connection_succeeds_for_all_four_protocols() {
    for (template, ok_body) in [
        ("openai", CHAT_OK),
        ("openai-responses", RESPONSES_OK),
        ("anthropic", ANTHROPIC_OK),
        ("gemini", GEMINI_OK),
    ] {
        let server = MockServer::start().await;
        let mut p = template_provider(template, &server.uri());
        mount_models(&server, p.protocol).await;
        Mock::given(method("POST"))
            .respond_with(sse(ok_body))
            .mount(&server)
            .await;
        p.models = vec![ModelEntry::new("configured-model")];
        let (client, _) = common::client();
        let report = client.test_connection(&p, Some(&Secret::new("k".to_owned()))).await;
        assert_eq!(report.overall, TestVerdict::Ok, "{template}: {report:?}");
        assert_eq!(report.model_used.as_deref(), Some("configured-model"));
        assert_eq!((report.models.status, report.stream.status), (Some(200), Some(200)));
        assert!(report.stream.url.starts_with("http://127.0.0.1:"));
        assert!(!report.fetched_models.is_empty());
        // 探针：最大输出 1，stream:true，'ping'
        let probe = bodies(&server).await.remove(0);
        let body = probe.to_string();
        assert!(body.contains("ping"), "{template}: {body}");
        assert!(
            [
                "\"max_completion_tokens\":1",
                "\"max_output_tokens\":1",
                "\"max_tokens\":1",
                "\"maxOutputTokens\":1"
            ]
            .iter()
            .any(|s| body.contains(s)),
            "{template}: {body}"
        );
    }
}

#[tokio::test]
async fn test_connection_partial_and_failed() {
    // /models 失败但流式成功 → partial（标黄）
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(sse(CHAT_OK))
        .mount(&server)
        .await;
    let mut p = template_provider("minimax", &format!("{}/v1", server.uri()));
    p.models = vec![ModelEntry::new("MiniMax-M3")];
    let (client, _) = common::client();
    let report = client.test_connection(&p, None).await;
    assert_eq!(report.overall, TestVerdict::Partial);
    assert_eq!(report.models.error.as_ref().map(|e| e.code), Some(ErrorCode::NotFound));
    assert!(report.stream.ok);

    // Key 错 → auth，failed
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(err(401, json!({"error": {"message": "bad key"}})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(err(401, json!({"error": {"message": "bad key"}})))
        .mount(&server)
        .await;
    let mut p = template_provider("openai", &format!("{}/v1", server.uri()));
    p.models = vec![ModelEntry::new("gpt-4.1")];
    let report = client.test_connection(&p, Some(&Secret::new("wrong".to_owned()))).await;
    assert_eq!(report.overall, TestVerdict::Failed);
    assert_eq!(report.stream.error.as_ref().map(|e| e.code), Some(ErrorCode::Auth));
    assert_eq!(report.stream.status, Some(401));

    // 2xx 但是 HTML 页面 → protocol
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("<html></html>", "text/html"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("<!doctype html><html></html>", "text/html"))
        .mount(&server)
        .await;
    let mut p = template_provider("relay", &server.uri());
    p.models = vec![ModelEntry::new("gpt-4o")];
    let report = client.test_connection(&p, None).await;
    assert_eq!(report.overall, TestVerdict::Failed);
    assert_eq!(report.stream.error.as_ref().map(|e| e.code), Some(ErrorCode::Protocol));
}

#[tokio::test]
async fn probe_picks_a_chat_model_from_the_list_when_none_configured() {
    let server = MockServer::start().await;
    mount_models(&server, Protocol::OpenAiChat).await;
    Mock::given(method("POST"))
        .respond_with(sse(CHAT_OK))
        .mount(&server)
        .await;
    let (client, _) = common::client();
    let p = template_provider("openai", &format!("{}/v1", server.uri()));
    let report = client.test_connection(&p, None).await;
    assert_eq!(report.overall, TestVerdict::Ok);
    assert_eq!(report.model_used.as_deref(), Some("gpt-4o"), "跳过嵌入模型");
    assert_eq!(report.fetched_models.len(), 2);

    // 都没有：只测 /models 并提示
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .mount(&server)
        .await;
    let p = template_provider("lmstudio", &server.uri());
    let report = client.test_connection(&p, None).await;
    assert_eq!(report.overall, TestVerdict::Partial);
    assert_eq!(report.model_used, None);
    assert_eq!(
        report.stream.error.as_ref().map(|e| e.code),
        Some(ErrorCode::NotConfigured)
    );
}

#[tokio::test]
async fn probe_without_models_reuses_the_models_error() {
    // 新建服务还没有模型（模板的 models 为空），/models 返回 401：探针没有执行，stream 沿用 auth（界面据此标红 Key）
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(err(401, json!({"error": {"message": "Incorrect API key provided"}})))
        .mount(&server)
        .await;
    let (client, _) = common::client();
    let p = template_provider("openai", &format!("{}/v1", server.uri()));
    assert!(p.models.is_empty());
    let report = client.test_connection(&p, None).await;
    assert_eq!(report.overall, TestVerdict::Failed);
    assert_eq!(report.model_used, None);
    assert_eq!(report.stream.error.as_ref().map(|e| e.code), Some(ErrorCode::Auth));
    assert_eq!(report.stream.status, Some(401));
    assert_eq!(report.stream.elapsed_ms, 0, "探针没有执行");
    assert_eq!(
        report.stream.error.as_ref().and_then(|e| e.hint.as_deref()),
        Some("errors.hint.check_key")
    );
    assert!(bodies(&server).await.is_empty(), "没有发出对话请求");

    // Base URL 少了版本段以外的路径（/models 404）、版本段是自动追加的：两项都带「尝试添加或去掉 /v1」
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let p = template_provider("openai", &server.uri());
    let report = client.test_connection(&p, None).await;
    assert_eq!(report.overall, TestVerdict::Failed);
    for r in [&report.models, &report.stream] {
        let e = r.error.as_ref().expect("error");
        assert_eq!(e.code, ErrorCode::NotFound);
        assert_eq!(e.hint.as_deref(), Some("errors.hint.try_v1"), "{r:?}");
        assert_eq!(e.subcode, Some(ErrorSubcode::VersionHint));
    }
    assert_eq!(report.stream.status, Some(404));

    // 用户写了版本段（没有追加）：404 不提示 /v1
    let p = template_provider("openai", &format!("{}/v1", server.uri()));
    let report = client.test_connection(&p, None).await;
    assert_eq!(
        report.stream.error.as_ref().and_then(|e| e.hint.as_deref()),
        Some("errors.hint.check_model")
    );
}

#[tokio::test]
async fn probe_success_keeps_the_plain_models_404() {
    // 流式成功（版本段是对的）：/models 的 404 不挂 /v1 提示
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(sse(CHAT_OK))
        .mount(&server)
        .await;
    let mut p = template_provider("openai", &server.uri());
    p.models = vec![ModelEntry::new("gpt-4.1")];
    let (client, _) = common::client();
    let report = client.test_connection(&p, None).await;
    assert_eq!(report.overall, TestVerdict::Partial);
    assert_eq!(
        report.models.error.as_ref().and_then(|e| e.hint.as_deref()),
        Some("errors.hint.check_model")
    );
}

#[tokio::test]
async fn probe_retries_with_sixteen_when_minimum_is_rejected() {
    let server = MockServer::start().await;
    mount_models(&server, Protocol::OpenAiResponses).await;
    Mock::given(method("POST"))
        .and(|r: &Request| String::from_utf8_lossy(&r.body).contains("\"max_output_tokens\":1,"))
        .respond_with(err(400, json!({"error": {"message": "Invalid 'max_output_tokens': integer below minimum value. Expected a value >= 16, but got 1 instead.", "type": "invalid_request_error", "param": "max_output_tokens", "code": "integer_below_min_value"}})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(|r: &Request| String::from_utf8_lossy(&r.body).contains("\"max_output_tokens\":16,"))
        .respond_with(sse(RESPONSES_OK))
        .mount(&server)
        .await;
    let (client, _) = common::client();
    let mut p = template_provider("openai-responses", &format!("{}/v1", server.uri()));
    p.models = vec![ModelEntry::new("gpt-5.4-mini")];
    let report = client.test_connection(&p, None).await;
    assert_eq!(report.overall, TestVerdict::Ok, "{report:?}");
    assert_eq!(bodies(&server).await.len(), 2);
}

#[tokio::test]
async fn test_connection_on_minimax_with_a_wrong_key_reports_auth() {
    // MiniMax 填错 Key：对话端点返回 HTTP 200 + base_resp 1004（填错 Key 应显示 auth，而不是「连接中断」）
    let base_resp = r#"{"id":"","choices":null,"created":0,"model":"","object":"","usage":null,"base_resp":{"status_code":1004,"status_msg":"login fail: Please carry the API secret key in the 'Authorization' field of the request header"}}"#;
    for models_reply in [
        // /v1/models 返回 Anthropic 风格的 401（实测）
        err(
            401,
            json!({"type": "error", "error": {"type": "authorized_error", "message": "invalid api key", "http_code": "401"}}),
        ),
        // 或同样是 200 + base_resp
        ResponseTemplate::new(200).set_body_raw(base_resp, "application/json"),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(models_reply)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(base_resp, "application/json"))
            .mount(&server)
            .await;
        let mut p = template_provider("minimax", &format!("{}/v1", server.uri()));
        p.models = vec![ModelEntry::new("MiniMax-M2.7")];
        let (client, _) = common::client();
        let report = client
            .test_connection(&p, Some(&Secret::new("wrong-minimax-key".to_owned())))
            .await;
        assert_eq!(report.overall, TestVerdict::Failed, "{report:?}");
        let e = report.stream.error.as_ref().expect("stream error");
        assert_eq!((e.code, e.subcode), (ErrorCode::Auth, None), "{e:?}");
        assert_eq!(e.hint.as_deref(), Some("errors.hint.check_key"));
        assert_eq!(report.stream.status, Some(200));
        assert_eq!(report.models.error.as_ref().map(|e| e.code), Some(ErrorCode::Auth));
    }
}

/// 请求体里某个数值字段的值
fn sent_number(r: &Request, key: &str) -> Option<u64> {
    serde_json::from_slice::<serde_json::Value>(&r.body)
        .ok()?
        .get(key)?
        .as_u64()
}

#[tokio::test]
async fn probe_value_error_retries_with_sixteen_and_keeps_the_field() {
    // 火山方舟拒绝 max_tokens=1：「Expected a value >= 16, but got 1 instead.」是取值错误，不是字段不被支持。
    // 探针改成 16 再试，仍发 max_tokens，能力缓存不被写成 max_completion_tokens
    let server = MockServer::start().await;
    mount_models(&server, Protocol::OpenAiChat).await;
    Mock::given(method("POST"))
        .and(|r: &Request| sent_number(r, "max_tokens") == Some(1))
        .respond_with(err(
            400,
            json!({"error": {"code": "InvalidParameter", "message": "The parameter `max_tokens` specified in the request are not valid: Expected a value >= 16, but got 1 instead. Request id: 0217600000000001", "param": "max_tokens", "type": "BadRequest"}}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(|r: &Request| sent_number(r, "max_tokens").is_some_and(|n| n >= 16))
        .respond_with(sse(CHAT_OK))
        .mount(&server)
        .await;
    let (client, caps) = common::client();
    let mut p = template_provider("volcengine", &format!("{}/v1", server.uri()));
    p.models = vec![ModelEntry::new("doubao-seed-1-6-flash-250828")];
    let report = client.test_connection(&p, None).await;
    assert_eq!(report.overall, TestVerdict::Ok, "{report:?}");
    let sent = bodies(&server).await;
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1]["max_tokens"], 16);
    assert!(sent[1].get("max_completion_tokens").is_none(), "{}", sent[1]);
    assert_eq!(
        caps.get(&p.id, "doubao-seed-1-6-flash-250828"),
        wisp_llm::ModelCaps::default()
    );

    // 正式请求照常发 max_tokens（火山必须显式发送）
    let events = common::run(&client, request(p.clone(), "doubao-seed-1-6-flash-250828", "hi")).await;
    assert_eq!(common::text_of(&events), "OK");
    let last = bodies(&server).await.pop().expect("request");
    assert!(
        last.get("max_tokens").is_some() && last.get("max_completion_tokens").is_none(),
        "{last}"
    );
}

#[tokio::test]
async fn probe_degrade_is_not_written_to_the_caps_store() {
    // 探针在请求内照常降级（测试连接仍然成功），但结论不写回能力缓存：正式请求自己再判断一次
    let server = MockServer::start().await;
    mount_models(&server, Protocol::OpenAiChat).await;
    Mock::given(method("POST"))
        .and(body_has("max_tokens"))
        .respond_with(err(
            400,
            json!({"error": {"message": "Unsupported parameter: 'max_tokens' is not supported with this model. Use 'max_completion_tokens' instead.", "type": "invalid_request_error", "param": "max_tokens", "code": "unsupported_parameter"}}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(sse(CHAT_OK))
        .mount(&server)
        .await;
    let (client, caps) = common::client();
    let mut p = template_provider("volcengine", &format!("{}/v1", server.uri()));
    p.models = vec![ModelEntry::new("doubao-x")];
    let report = client.test_connection(&p, None).await;
    assert_eq!(report.overall, TestVerdict::Ok, "{report:?}");
    assert_eq!(bodies(&server).await.len(), 2, "探针在请求内降级了一次");
    assert_eq!(caps.get(&p.id, "doubao-x").max_tokens_field, None, "探针的结论不写回");

    let events = common::run(&client, request(p.clone(), "doubao-x", "hi")).await;
    assert_eq!(notices(&events), vec![NoticeKind::ParamDegraded]);
    assert_eq!(
        caps.get(&p.id, "doubao-x").max_tokens_field,
        Some(MaxTokensField::MaxCompletionTokens),
        "正式请求的结论照常写回"
    );
}

#[tokio::test]
async fn reasoning_model_truncated_without_text_still_counts_as_success() {
    let server = MockServer::start().await;
    mount_models(&server, Protocol::OpenAiChat).await;
    let body = "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n\
                data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}\n\n\
                data: [DONE]\n\n";
    Mock::given(method("POST")).respond_with(sse(body)).mount(&server).await;
    let (client, _) = common::client();
    let mut p = template_provider("openai", &format!("{}/v1", server.uri()));
    p.models = vec![ModelEntry::new("o3")];
    p.quirks.thinking_off = ThinkingOff::OpenAiEffort;
    let report = client.test_connection(&p, None).await;
    assert_eq!(report.overall, TestVerdict::Ok);
}

#[tokio::test]
async fn probe_skips_an_embedding_model_listed_first() {
    // Ollama 按修改时间列模型，嵌入模型常排第一；首次获取后全部启用。探针应选对话模型
    let server = MockServer::start().await;
    mount_models(&server, Protocol::OpenAiChat).await;
    Mock::given(method("POST"))
        .and(|r: &Request| String::from_utf8_lossy(&r.body).contains("\"model\":\"llama3.2:latest\""))
        .respond_with(sse(CHAT_OK))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(err(
            400,
            json!({"error": {"message": "\"nomic-embed-text:latest\" does not support chat"}}),
        ))
        .with_priority(2)
        .mount(&server)
        .await;
    let (client, _) = common::client();
    let mut p = template_provider("ollama", &format!("{}/v1", server.uri()));
    p.models = vec![
        ModelEntry::new("nomic-embed-text:latest"),
        ModelEntry::new("llama3.2:latest"),
    ];
    let report = client.test_connection(&p, None).await;
    assert_eq!(report.overall, TestVerdict::Ok, "{report:?}");
    assert_eq!(report.model_used.as_deref(), Some("llama3.2:latest"));
}

// ---------------------------------------------------------------------------
// 日志：两个 id 都能在日志里找到；Gemini 没有 request-id 响应头，上游 id 来自响应体的 responseId

struct CaptureLog(std::sync::Mutex<Vec<String>>);

impl log::Log for CaptureLog {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        if let Ok(mut lines) = self.0.lock() {
            lines.push(record.args().to_string());
        }
    }

    fn flush(&self) {}
}

static LOGS: CaptureLog = CaptureLog(std::sync::Mutex::new(Vec::new()));

fn log_lines(request_id: &str) -> Vec<String> {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let _ = log::set_logger(&LOGS);
        log::set_max_level(log::LevelFilter::Info);
    });
    let needle = format!("llm {request_id} ");
    LOGS.0
        .lock()
        .map(|l| l.iter().filter(|x| x.starts_with(&needle)).cloned().collect())
        .unwrap_or_default()
}

#[tokio::test]
async fn logs_carry_the_upstream_id_from_the_body() {
    let _ = log_lines("init");
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"OK\"}]},\"finishReason\":\"STOP\"}],\"responseId\":\"g-resp-7\"}\n\n",
        ))
        .mount(&server)
        .await;
    let (client, _) = common::client();
    let mut req = request(
        provider(Protocol::Gemini, &format!("{}/v1beta", server.uri())),
        "gemini-3-flash",
        "hi",
    );
    req.request_id = "r-log-gemini".to_owned();
    let events = common::run(&client, req).await;
    assert!(matches!(&events[0], StreamEvent::Start { upstream_id: Some(id), .. } if id == "g-resp-7"));
    let lines = log_lines("r-log-gemini");
    assert!(
        lines
            .iter()
            .any(|l| l.contains(" done ") && l.contains("upstream=g-resp-7")),
        "{lines:?}"
    );
    assert!(
        lines.iter().all(|l| !l.contains("sk-test") && !l.contains("hi\"")),
        "{lines:?}"
    );
}
