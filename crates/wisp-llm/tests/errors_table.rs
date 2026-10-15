//! 错误映射参数化表：每一行起一个脚本化服务器返回给定的状态码 / 响应头 / 错误体，
//! 断言归一化后的 code、subcode、retryable、retry_after_s 与展示的原文。各行并发执行（5xx 行会自动重试一次）。
//! 另有传输层的三例（拒绝连接 / DNS / TLS）。

mod common;

use common::{Reply, ScriptServer, client, image_request, provider, request};
use futures_util::future::join_all;
use wisp_llm::{ErrorCode, ErrorSubcode, LlmError, Protocol, StreamEvent};

#[derive(Clone)]
struct Row {
    name: &'static str,
    protocol: Protocol,
    status: u16,
    headers: &'static [(&'static str, &'static str)],
    body: &'static str,
    image: bool,
    /// base_url 的路径部分（默认 /v1 或 /v1beta；空串表示不带版本段）
    path: Option<&'static str>,
    code: ErrorCode,
    subcode: Option<ErrorSubcode>,
    retryable: Option<bool>,
    retry_after: Option<u32>,
    message: Option<&'static str>,
    hint: Option<&'static str>,
    /// 期望的 HTTP 调用次数
    calls: usize,
}

const fn row(name: &'static str, protocol: Protocol, status: u16, body: &'static str, code: ErrorCode) -> Row {
    Row {
        name,
        protocol,
        status,
        headers: &[],
        body,
        image: false,
        path: None,
        code,
        subcode: None,
        retryable: None,
        retry_after: None,
        message: None,
        hint: None,
        calls: 1,
    }
}

fn rows() -> Vec<Row> {
    use ErrorCode as C;
    use Protocol::{AnthropicMessages as A, Gemini as G, OpenAiChat as O, OpenAiResponses as R};
    vec![
        // ---- OpenAI
        Row {
            message: Some("Incorrect API key provided"),
            hint: Some("errors.hint.check_key"),
            retryable: Some(false),
            ..row(
                "openai 401",
                O,
                401,
                r#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error","code":"invalid_api_key"}}"#,
                C::Auth,
            )
        },
        Row {
            subcode: Some(ErrorSubcode::Region),
            hint: Some("errors.hint.region"),
            ..row(
                "openai 403 region",
                O,
                403,
                r#"{"error":{"code":"unsupported_country_region_territory","message":"Country, region, or territory not supported","type":"request_forbidden"}}"#,
                C::Auth,
            )
        },
        row(
            "openai 403",
            R,
            403,
            r#"{"error":{"message":"Permission denied"}}"#,
            C::Auth,
        ),
        Row {
            hint: Some("errors.hint.check_model"),
            ..row(
                "openai 404 model",
                O,
                404,
                r#"{"error":{"message":"The model `gpt-9` does not exist or you do not have access to it.","type":"invalid_request_error","code":"model_not_found"}}"#,
                C::NotFound,
            )
        },
        // base_url 没有版本段、不以 # 结尾，对话端点 HTML 404 → 提示加 / 去 /v1
        Row {
            path: Some(""),
            subcode: Some(ErrorSubcode::VersionHint),
            hint: Some("errors.hint.try_v1"),
            message: Some("the server returned an HTML page (HTTP 404)"),
            ..row(
                "html 404 version hint",
                O,
                404,
                "<html><body>404 Not Found</body></html>",
                C::NotFound,
            )
        },
        Row {
            image: true,
            hint: Some("errors.hint.pick_vision_model"),
            ..row(
                "openai 400 vision",
                O,
                400,
                r#"{"error":{"message":"Invalid content type. image_url is only supported by certain models.","type":"invalid_request_error","param":"messages.[1].content.[0].type"}}"#,
                C::NoVision,
            )
        },
        // DeepSeek 等：消息结构里根本没有图片类型（serde 的 unknown variant）→ 确定的 no_vision
        Row {
            image: true,
            hint: Some("errors.hint.pick_vision_model"),
            ..row(
                "compat 400 unknown variant image_url",
                O,
                400,
                r#"{"error":{"message":"Failed to deserialize the JSON body into the target type: messages[1]: unknown variant `image_url`, expected `text` at line 1 column 312","type":"invalid_request_error"}}"#,
                C::NoVision,
            )
        },
        Row {
            image: true,
            ..row(
                "compat 400 does not support image input",
                O,
                400,
                r#"{"error":{"message":"The model `llama-3.1-8b-instant` does not support image input.","type":"invalid_request_error"}}"#,
                C::NoVision,
            )
        },
        // OpenRouter：不支持视觉的模型返回 404「No endpoints found that support image input」→ 确定的 no_vision，
        // 不是 not_found
        Row {
            image: true,
            retryable: Some(false),
            hint: Some("errors.hint.pick_vision_model"),
            ..row(
                "openrouter 404 no image endpoints",
                O,
                404,
                r#"{"error":{"message":"No endpoints found that support image input","code":404}}"#,
                C::NoVision,
            )
        },
        // llama.cpp server：500「image input is not supported」→ no_vision，不自动重试
        Row {
            image: true,
            retryable: Some(false),
            hint: Some("errors.hint.pick_vision_model"),
            ..row(
                "llama.cpp 500 image input not supported",
                O,
                500,
                r#"{"error":{"code":500,"message":"image input is not supported - hint: if this is unexpected, you may need to provide the mmproj","type":"server_error"}}"#,
                C::NoVision,
            )
        },
        // 不带图时同样的 500 仍是 upstream（自动重试一次）
        Row {
            calls: 2,
            ..row(
                "llama.cpp 500 without image",
                O,
                500,
                r#"{"error":{"code":500,"message":"image input is not supported","type":"server_error"}}"#,
                C::Upstream,
            )
        },
        // 只是图片的传输格式不被支持：5xx 照旧按 upstream（不当作模型不支持视觉）
        Row {
            image: true,
            calls: 2,
            ..row(
                "compat 500 base64 image input",
                O,
                500,
                r#"{"error":{"message":"This endpoint does not support base64 image input","type":"server_error"}}"#,
                C::Upstream,
            )
        },
        // 带图请求的上下文超长：too_long，不是 image_too_large（「maximum」不再抢先）
        Row {
            image: true,
            hint: Some("errors.hint.shorten_text"),
            ..row(
                "openai 400 context length with image tokens",
                O,
                400,
                r#"{"error":{"message":"This model's maximum context length is 8192 tokens. However, your messages resulted in 9120 tokens (including image tokens). Please reduce the length of the messages.","type":"invalid_request_error","param":"messages","code":"context_length_exceeded"}}"#,
                C::TooLong,
            )
        },
        // 只提到图片、看不出原因：仍出 no_vision 卡片（运行时反馈只写给 Unknown 的模型，见 mock_server）
        Row {
            image: true,
            ..row(
                "compat 400 image field rejected",
                O,
                400,
                r#"{"error":{"message":"messages[0].content[0].image_url: extra fields not permitted","type":"invalid_request_error"}}"#,
                C::NoVision,
            )
        },
        // 识别不出是哪个参数、无法降级：终止事件是会出卡片的 upstream（param_unsupported 不出卡片）
        Row {
            retryable: Some(false),
            hint: Some("errors.hint.param_rejected"),
            message: Some("Unsupported parameter: 'foo'"),
            ..row(
                "openai 400 unknown unsupported param",
                O,
                400,
                r#"{"error":{"message":"Unsupported parameter: 'foo' is not supported with this model.","type":"invalid_request_error","param":"foo","code":"unsupported_parameter"}}"#,
                C::Upstream,
            )
        },
        Row {
            hint: Some("errors.hint.shorten_text"),
            ..row(
                "openai 400 too long",
                O,
                400,
                r#"{"error":{"message":"This model's maximum context length is 128000 tokens. However, your messages resulted in 130000 tokens.","type":"invalid_request_error","param":"messages","code":"context_length_exceeded"}}"#,
                C::TooLong,
            )
        },
        Row {
            retryable: Some(false),
            hint: Some("errors.hint.check_billing"),
            ..row(
                "openai 429 quota",
                O,
                429,
                r#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details.","type":"insufficient_quota","code":"insufficient_quota"}}"#,
                C::Quota,
            )
        },
        Row {
            headers: &[("retry-after", "7"), ("x-request-id", "req_7")],
            retry_after: Some(7),
            retryable: Some(true),
            hint: Some("errors.hint.retry_later"),
            ..row(
                "openai 429 retry-after",
                O,
                429,
                r#"{"error":{"message":"Rate limit reached for gpt-5 in organization org-x on tokens per min (TPM).","type":"tokens","code":"rate_limit_exceeded"}}"#,
                C::RateLimited,
            )
        },
        row(
            "openai 429 no header",
            R,
            429,
            r#"{"error":{"message":"Slow down","code":"slow_down"}}"#,
            C::RateLimited,
        ),
        Row {
            calls: 2,
            retryable: Some(true),
            ..row(
                "openai 500",
                O,
                500,
                r#"{"error":{"message":"server error"}}"#,
                C::Upstream,
            )
        },
        Row {
            calls: 2,
            ..row("openai 502", O, 502, "Bad Gateway", C::Upstream)
        },
        Row {
            calls: 2,
            ..row(
                "openai 503",
                R,
                503,
                r#"{"error":{"message":"server is overloaded","code":"server_is_overloaded"}}"#,
                C::Upstream,
            )
        },
        Row {
            calls: 2,
            ..row("openai 504", O, 504, "", C::Upstream)
        },
        // ---- 兼容端点（DeepSeek、火山、硅基、OpenRouter 等）：先按状态码归类，再取原文
        Row {
            message: Some("Token is invalid."),
            ..row(
                "siliconflow 401",
                O,
                401,
                r#"{"code":30014,"message":"Token is invalid.","data":null}"#,
                C::Auth,
            )
        },
        Row {
            message: Some("令牌已过期或验证不正确"),
            ..row(
                "zhipu 401",
                O,
                401,
                r#"{"error":{"code":"401","message":"令牌已过期或验证不正确"}}"#,
                C::Auth,
            )
        },
        row(
            "volcengine 401",
            O,
            401,
            r#"{"error":{"code":"AuthenticationError","message":"The API key in the request is missing or invalid.","param":"","type":"Unauthorized"}}"#,
            C::Auth,
        ),
        Row {
            message: Some("Invalid Authentication"),
            ..row(
                "moonshot 401 without code",
                O,
                401,
                r#"{"error":{"message":"Invalid Authentication","type":"invalid_authentication_error"}}"#,
                C::Auth,
            )
        },
        row(
            "minimax 401 anthropic-style",
            O,
            401,
            r#"{"type":"error","error":{"type":"authorized_error","http_code":"401"}}"#,
            C::Auth,
        ),
        Row {
            message: Some("Authentication Fails (governor)"),
            ..row(
                "deepseek plain text 401",
                O,
                401,
                "Authentication Fails (governor)",
                C::Auth,
            )
        },
        row(
            "openrouter 401 int code",
            O,
            401,
            r#"{"error":{"code":401,"message":"No auth credentials found"}}"#,
            C::Auth,
        ),
        Row {
            hint: Some("errors.hint.check_protocol"),
            ..row("compat 400 plain text", O, 400, "Bad Request", C::Protocol)
        },
        // ---- Anthropic
        row(
            "anthropic 401",
            A,
            401,
            r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"},"request_id":"req_011"}"#,
            C::Auth,
        ),
        row(
            "anthropic 402",
            A,
            402,
            r#"{"type":"error","error":{"type":"billing_error","message":"Your credit balance is too low."}}"#,
            C::Quota,
        ),
        row(
            "anthropic 403",
            A,
            403,
            r#"{"type":"error","error":{"type":"permission_error","message":"Your API key does not have permission to use the specified resource."}}"#,
            C::Auth,
        ),
        row(
            "anthropic 404",
            A,
            404,
            r#"{"type":"error","error":{"type":"not_found_error","message":"model: claude-x"}}"#,
            C::NotFound,
        ),
        Row {
            image: true,
            ..row(
                "anthropic 400 image too large",
                A,
                400,
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.0.content.0.image.source.base64: image exceeds 5 MB maximum: 7340032 bytes > 5242880 bytes"}}"#,
                C::ImageTooLarge,
            )
        },
        // 单张图片本身有问题（与声明的类型不符、无法处理）：与模型是否支持视觉无关 → upstream，不写视觉反馈
        Row {
            image: true,
            retryable: Some(false),
            ..row(
                "anthropic 400 media type",
                A,
                400,
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"Image does not match the provided media type image/png"}}"#,
                C::Upstream,
            )
        },
        Row {
            image: true,
            retryable: Some(false),
            ..row(
                "anthropic 400 could not process image",
                A,
                400,
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"Could not process image"}}"#,
                C::Upstream,
            )
        },
        row(
            "anthropic 400 too long",
            A,
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 215000 tokens > 200000 maximum"}}"#,
            C::TooLong,
        ),
        row(
            "anthropic 400 credit",
            A,
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API."}}"#,
            C::Quota,
        ),
        Row {
            retryable: Some(true),
            ..row(
                "anthropic 409",
                A,
                409,
                r#"{"type":"error","error":{"type":"conflict","message":"conflict"}}"#,
                C::Upstream,
            )
        },
        Row {
            image: true,
            ..row(
                "anthropic 413",
                A,
                413,
                r#"{"type":"error","error":{"type":"request_too_large","message":"Request exceeds the maximum allowed number of bytes."}}"#,
                C::ImageTooLarge,
            )
        },
        Row {
            headers: &[("retry-after", "12")],
            retry_after: Some(12),
            ..row(
                "anthropic 429 retry-after",
                A,
                429,
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"Number of request tokens has exceeded your per-minute rate limit"}}"#,
                C::RateLimited,
            )
        },
        Row {
            retryable: Some(false),
            ..row(
                "anthropic 429 spend cap",
                A,
                429,
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"You have reached your specified API usage limits."}}"#,
                C::Quota,
            )
        },
        Row {
            calls: 2,
            ..row(
                "anthropic 500",
                A,
                500,
                r#"{"type":"error","error":{"type":"api_error","message":"Internal server error"}}"#,
                C::Upstream,
            )
        },
        Row {
            calls: 2,
            ..row(
                "anthropic 504",
                A,
                504,
                r#"{"type":"error","error":{"type":"timeout_error","message":"timeout"}}"#,
                C::Upstream,
            )
        },
        Row {
            calls: 2,
            retryable: Some(true),
            message: Some("Overloaded"),
            ..row(
                "anthropic 529",
                A,
                529,
                r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
                C::Upstream,
            )
        },
        // x-should-retry:false 时不重试
        Row {
            headers: &[("x-should-retry", "false")],
            calls: 1,
            ..row(
                "anthropic 529 should-not-retry",
                A,
                529,
                r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
                C::Upstream,
            )
        },
        // Retry-After > 5 s：不自动重试，交给用户
        Row {
            headers: &[("retry-after", "30")],
            calls: 1,
            retry_after: Some(30),
            ..row(
                "openai 503 long retry-after",
                O,
                503,
                r#"{"error":{"message":"busy"}}"#,
                C::Upstream,
            )
        },
        // ---- Gemini
        Row {
            headers: &[("content-type", "text/event-stream")],
            message: Some("API key not valid. Please pass a valid API key."),
            ..row(
                "gemini 400 api key",
                G,
                400,
                r#"{"error":{"code":400,"message":"API key not valid. Please pass a valid API key.","status":"INVALID_ARGUMENT","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"API_KEY_INVALID","domain":"googleapis.com"}]}}"#,
                C::Auth,
            )
        },
        // Gemini 全系支持图片：这是单张图片处理失败，服务端建议重试 → upstream（可重试），不是 no_vision
        Row {
            image: true,
            retryable: Some(true),
            hint: Some("errors.hint.retry"),
            ..row(
                "gemini 400 image",
                G,
                400,
                r#"{"error":{"code":400,"message":"Unable to process input image. Please retry or report in https://developers.generativeai.google/guide/troubleshooting","status":"INVALID_ARGUMENT"}}"#,
                C::Upstream,
            )
        },
        // 所在地区不可用（中国大陆 / 香港用户最常见）：auth + region，提示地区与代理，而不是「额度用尽」
        Row {
            subcode: Some(ErrorSubcode::Region),
            hint: Some("errors.hint.region"),
            retryable: Some(false),
            ..row(
                "gemini 400 failed precondition location",
                G,
                400,
                r#"{"error":{"code":400,"message":"User location is not supported for the API use.","status":"FAILED_PRECONDITION"}}"#,
                C::Auth,
            )
        },
        // 免费层在该地区不可用、开通计费即可：仍按 quota（引导开通计费）
        Row {
            hint: Some("errors.hint.check_billing"),
            ..row(
                "gemini 400 failed precondition billing",
                G,
                400,
                r#"{"error":{"code":400,"message":"Gemini API free tier is not available in your country. Please enable billing on your project in Google AI Studio.","status":"FAILED_PRECONDITION"}}"#,
                C::Quota,
            )
        },
        Row {
            subcode: Some(ErrorSubcode::Region),
            hint: Some("errors.hint.region"),
            ..row(
                "compat 400 region unavailable",
                O,
                400,
                r#"{"error":{"message":"This service is not available in your region.","type":"invalid_request_error"}}"#,
                C::Auth,
            )
        },
        row(
            "gemini 400 too long",
            G,
            400,
            r#"{"error":{"code":400,"message":"The input token count (1200000) exceeds the maximum number of tokens allowed (1048576).","status":"INVALID_ARGUMENT"}}"#,
            C::TooLong,
        ),
        row(
            "gemini 403",
            G,
            403,
            r#"{"error":{"code":403,"message":"Method doesn't allow unregistered callers.","status":"PERMISSION_DENIED"}}"#,
            C::Auth,
        ),
        row(
            "gemini 404",
            G,
            404,
            r#"{"error":{"code":404,"message":"models/gemini-x is not found for API version v1beta","status":"NOT_FOUND"}}"#,
            C::NotFound,
        ),
        row(
            "gemini 429 quota",
            G,
            429,
            r#"{"error":{"code":429,"message":"You exceeded your current quota. Daily limit reached.","status":"RESOURCE_EXHAUSTED"}}"#,
            C::Quota,
        ),
        row(
            "gemini 429 rate",
            G,
            429,
            r#"{"error":{"code":429,"message":"Too many requests, please slow down.","status":"RESOURCE_EXHAUSTED"}}"#,
            C::RateLimited,
        ),
        row(
            "gemini 499",
            G,
            499,
            r#"{"error":{"code":499,"message":"The operation was cancelled.","status":"CANCELLED"}}"#,
            C::Cancelled,
        ),
        Row {
            calls: 2,
            ..row(
                "gemini 503",
                G,
                503,
                r#"{"error":{"code":503,"message":"The model is overloaded.","status":"UNAVAILABLE"}}"#,
                C::Upstream,
            )
        },
        // 字符串 code（Interactions API 风格）
        Row {
            message: Some("bad request"),
            ..row(
                "gemini string code",
                G,
                400,
                r#"{"error":{"code":"invalid_request","message":"bad request"}}"#,
                C::Upstream,
            )
        },
    ]
}

async fn run_row(r: Row) -> Result<(), String> {
    let mut reply = Reply::body(
        r.status,
        r.headers
            .iter()
            .find(|(n, _)| *n == "content-type")
            .map_or("application/json", |(_, v)| v),
        r.body,
    );
    for (n, v) in r.headers.iter().filter(|(n, _)| *n != "content-type") {
        reply = reply.header(n, v);
    }
    let server = ScriptServer::start(vec![reply]).await;
    let default_path = if r.protocol == Protocol::Gemini {
        "/v1beta"
    } else {
        "/v1"
    };
    let base = format!("{}{}", server.base, r.path.unwrap_or(default_path));
    let p = provider(r.protocol, &base);
    let model = match r.protocol {
        Protocol::Gemini => "gemini-2.5-flash",
        Protocol::AnthropicMessages => "claude-haiku-4-5",
        _ => "gpt-4.1",
    };
    let req = if r.image {
        image_request(p, model)
    } else {
        request(p, model, "hi")
    };
    let (client, _) = client();
    let events = common::run(&client, req).await;
    let e: &LlmError = common::error_of(&events);
    let mut problems = Vec::new();
    if !matches!(events.as_slice(), [StreamEvent::Start { .. }, StreamEvent::Error(_)]) {
        problems.push(format!("expected Start → Error, got {events:?}"));
    }
    if e.code != r.code {
        problems.push(format!("code {:?} != {:?}", e.code, r.code));
    }
    if r.subcode.is_some() && e.subcode != r.subcode {
        problems.push(format!("subcode {:?} != {:?}", e.subcode, r.subcode));
    }
    if let Some(retryable) = r.retryable
        && e.retryable != retryable
    {
        problems.push(format!("retryable {} != {retryable}", e.retryable));
    }
    if e.retry_after_s != r.retry_after {
        problems.push(format!("retry_after {:?} != {:?}", e.retry_after_s, r.retry_after));
    }
    if let Some(m) = r.message
        && !e.message.starts_with(m)
    {
        problems.push(format!("message {:?} does not start with {m:?}", e.message));
    }
    if let Some(h) = r.hint
        && e.hint.as_deref() != Some(h)
    {
        problems.push(format!("hint {:?} != {h:?}", e.hint));
    }
    if e.status != Some(r.status) {
        problems.push(format!("status {:?}", e.status));
    }
    if !e
        .url
        .as_deref()
        .is_some_and(|u| u.starts_with("127.0.0.1:") && !u.contains('?'))
    {
        problems.push(format!("url {:?}", e.url));
    }
    if server.request_count() != r.calls {
        problems.push(format!("{} HTTP calls, expected {}", server.request_count(), r.calls));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!("{}: {}", r.name, problems.join("; ")))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn error_mapping_table() {
    let results = join_all(rows().into_iter().map(run_row)).await;
    let failures: Vec<String> = results.into_iter().filter_map(Result::err).collect();
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[tokio::test]
async fn anthropic_request_id_from_body() {
    let server = ScriptServer::start(vec![Reply::body(
        401,
        "application/json",
        r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"},"request_id":"req_011CS"}"#,
    )])
    .await;
    let (client, _) = client();
    let events = common::run(
        &client,
        request(
            provider(Protocol::AnthropicMessages, &server.base),
            "claude-haiku-4-5",
            "hi",
        ),
    )
    .await;
    assert_eq!(
        common::error_of(&events).upstream_request_id.as_deref(),
        Some("req_011CS")
    );
}

// ---- 流中的「不支持图片输入」（200 + error chunk，OpenRouter 等）：确定的 no_vision，并写回运行时视觉反馈

#[tokio::test]
async fn stream_error_chunk_says_no_vision() {
    let server = ScriptServer::start(vec![Reply::sse_text(
        "data: {\"error\":{\"message\":\"This model does not support image input\",\"code\":400}}\n\n",
    )])
    .await;
    let (client, caps) = client();
    let base = format!("{}/v1", server.base);
    // 已知支持视觉的模型：只有确定的结论才会覆盖
    let mut req = image_request(provider(Protocol::OpenAiChat, &base), "gpt-4.1");
    req.model.vision = wisp_llm::Vision::Yes;
    req.model.vision_source = Some(wisp_llm::VisionSource::Capability);
    let events = common::run(&client, req).await;
    let e = common::error_of(&events);
    assert_eq!((e.code, e.retryable), (ErrorCode::NoVision, false), "{e:?}");
    assert_eq!(e.hint.as_deref(), Some("errors.hint.pick_vision_model"));
    assert_eq!(server.request_count(), 1);
    assert_eq!(
        caps.vision("test", "gpt-4.1"),
        Some((wisp_llm::Vision::No, wisp_llm::VisionSource::Runtime))
    );
}

// ---- 传输层错误都归为 network，并带上 URL

async fn transport_error(base: &str) -> LlmError {
    let (client, _) = client();
    let events = common::run(&client, request(provider(Protocol::OpenAiChat, base), "gpt-4.1", "hi")).await;
    common::assert_failed_before_stream(&events);
    common::error_of(&events).clone()
}

#[tokio::test]
async fn connection_refused_is_network() {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    };
    let e = transport_error(&format!("http://127.0.0.1:{port}/v1")).await;
    assert_eq!(e.code, ErrorCode::Network);
    assert!(e.retryable);
    assert_eq!(e.hint.as_deref(), Some("errors.hint.check_network"));
    assert_eq!(
        e.url.as_deref(),
        Some(format!("127.0.0.1:{port}/v1/chat/completions").as_str())
    );
    assert_eq!(e.message, "connection refused");
}

#[tokio::test]
async fn dns_failure_is_network() {
    // *.localhost 不走代理；解析不到（或解析到回环但没人监听）都应归为 network
    let e = transport_error("http://wisp-no-such-host.localhost:9/v1").await;
    assert_eq!(e.code, ErrorCode::Network);
    assert_eq!(
        e.url.as_deref(),
        Some("wisp-no-such-host.localhost:9/v1/chat/completions")
    );
}

#[tokio::test]
async fn tls_failure_is_network() {
    // https 连到一个一上来就回明文的服务器：TLS 握手失败
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("https://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        while let Ok((mut s, _)) = listener.accept().await {
            let _ = s
                .write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n")
                .await;
        }
    });
    let e = transport_error(&format!("{base}/v1")).await;
    assert_eq!(e.code, ErrorCode::Network);
    assert!(e.url.as_deref().is_some_and(|u| u.ends_with("/v1/chat/completions")));
    assert!(
        !e.message.contains("https://"),
        "message must not echo the URL: {}",
        e.message
    );
}

#[tokio::test]
async fn echoed_key_never_leaks_even_when_it_straddles_the_cut() {
    // 服务端回显的 Key 正好跨过 300 字符的截断点。先截断再脱敏会留下 Key 的前缀；
    // 现在先在完整原文上脱敏再截断。覆盖非 2xx、流中错误、2xx 整个 JSON 三条路径，以及测试连接与模型列表
    let key = "sk-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrs";
    for at in [270, 285, 296, 299] {
        let message = format!(
            "{} Incorrect API key provided: {key}. Check your settings.",
            "x".repeat(at - 30)
        );
        let error_json =
            serde_json::json!({"error": {"message": message, "type": "invalid_request_error"}}).to_string();
        let stream = format!(
            "data: {{\"id\":\"c\",\"choices\":[{{\"delta\":{{\"content\":\"a\"}}}}]}}\n\ndata: {error_json}\n\n"
        );
        let relay = serde_json::json!({"code": 401, "msg": message}).to_string();
        for reply in [
            Reply::body(401, "application/json", &error_json),
            Reply::sse_text(&stream),
            Reply::body(200, "application/json", &relay),
        ] {
            let server = ScriptServer::start(vec![reply]).await;
            let (client, _) = client();
            let mut req = request(
                provider(Protocol::OpenAiChat, &format!("{}/v1", server.base)),
                "gpt-4.1",
                "hi",
            );
            req.key = Some(wisp_llm::Secret::new(key.to_owned()));
            let events = common::run(&client, req).await;
            let e = common::error_of(&events);
            assert!(!e.message.contains("sk-ABCD"), "at {at}: {}", e.message);
            assert!(e.message.chars().count() <= 301, "at {at}");
        }
        // 测试连接（/models 与探针）
        let server = ScriptServer::start(vec![Reply::body(401, "application/json", &error_json)]).await;
        let (client, _) = client();
        let mut p = provider(Protocol::OpenAiChat, &format!("{}/v1", server.base));
        p.models = vec![wisp_llm::ModelEntry::new("gpt-4.1")];
        let report = client
            .test_connection(&p, Some(&wisp_llm::Secret::new(key.to_owned())))
            .await;
        for e in [report.models.error, report.stream.error] {
            let e = e.expect("error");
            assert!(!e.message.contains("sk-ABCD"), "at {at}: {}", e.message);
        }
    }
}
