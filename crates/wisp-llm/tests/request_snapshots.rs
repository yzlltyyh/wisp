//! 请求快照（insta）：抓取脚本化服务器实际收到的请求（方法、路径、请求头、请求体）。
//! - 四协议 × 文本 / 图片 × 推理 / 非推理（16 组）；
//! - quirks.thinking_off 的 9 种模板变体各一组，外加按 chat template 关思考（llama-server）；
//! - merge_system_into_user、Anthropic 的 thinking 条件与 OCR effort、鉴权头的各种情况、自定义请求头。
//!
//! 服务器一律回 401（不重试、不降级），确保每组只发出一次请求。

mod common;

use common::{Reply, ScriptServer, client, image_request, request};
use serde_json::{Value, json};
use wisp_llm::{ChatRequest, ModelEntry, Protocol, Purpose, ThinkingOff, provider_from_template};

/// 快照里保留的请求头
const HEADERS: [&str; 10] = [
    "content-type",
    "accept",
    "user-agent",
    "authorization",
    "x-api-key",
    "anthropic-version",
    "x-goog-api-key",
    "openai-organization",
    "http-referer",
    "x-title",
];

async fn capture(mut req: ChatRequest, path_suffix: &str) -> Value {
    let server = ScriptServer::start(vec![Reply::body(401, "application/json", "{}")]).await;
    req.provider.base_url = format!("{}{path_suffix}", server.base);
    let (client, _) = client();
    let _ = common::run(&client, req).await;
    let reqs = server.requests();
    assert_eq!(reqs.len(), 1, "exactly one HTTP call");
    let r = &reqs[0];
    let headers: serde_json::Map<String, Value> = HEADERS
        .iter()
        .filter_map(|h| r.header(h).map(|v| ((*h).to_owned(), Value::String(v.to_owned()))))
        .collect();
    json!({ "method": r.method, "target": r.target, "headers": headers, "body": r.json() })
}

fn from_template(template: &str, model: &str, reasoning: bool) -> ChatRequest {
    let p = provider_from_template(template, &[]).expect("template");
    let mut req = request(p, model, "Hello, world.");
    req.model.reasoning = reasoning;
    req
}

fn image_from_template(template: &str, model: &str, reasoning: bool) -> ChatRequest {
    let p = provider_from_template(template, &[]).expect("template");
    let mut req = image_request(p, model);
    req.model.reasoning = reasoning;
    req
}

fn suffix(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Gemini => "/v1beta",
        _ => "/v1",
    }
}

#[tokio::test]
async fn four_protocols_text_image_reasoning_matrix() {
    let matrix = [
        ("openai", Protocol::OpenAiChat, "gpt-5.4-mini", "gpt-4.1"),
        ("openai-responses", Protocol::OpenAiResponses, "gpt-5.4-mini", "gpt-4.1"),
        (
            "anthropic",
            Protocol::AnthropicMessages,
            "claude-sonnet-5-5",
            "claude-haiku-4-5",
        ),
        ("gemini", Protocol::Gemini, "gemini-3-flash-preview", "gemini-2.0-flash"),
    ];
    for (template, protocol, reasoning_model, plain_model) in matrix {
        for (kind, model, reasoning) in [("reasoning", reasoning_model, true), ("plain", plain_model, false)] {
            let text = capture(from_template(template, model, reasoning), suffix(protocol)).await;
            insta::assert_json_snapshot!(format!("{template}__text__{kind}"), text);
            let image = capture(image_from_template(template, model, reasoning), suffix(protocol)).await;
            insta::assert_json_snapshot!(format!("{template}__image__{kind}"), image);
        }
    }
}

#[tokio::test]
async fn nine_thinking_off_variants() {
    let variants = [
        ("relay", ThinkingOff::None, "some-model"),
        ("deepseek", ThinkingOff::DeepSeek, "deepseek-flash"),
        ("qwen", ThinkingOff::Qwen, "qwen3.6-plus"),
        ("zhipu", ThinkingOff::Glm, "glm-5.3"),
        ("moonshot", ThinkingOff::Kimi, "kimi-k3"),
        ("minimax", ThinkingOff::MiniMax, "MiniMax-M3"),
        ("openai", ThinkingOff::OpenAiEffort, "o4-mini"),
        ("anthropic", ThinkingOff::AnthropicEffort, "claude-opus-5-5"),
        ("gemini", ThinkingOff::GeminiLevel, "gemini-2.5-flash"),
    ];
    for (template, variant, model) in variants {
        let req = from_template(template, model, false);
        assert_eq!(req.provider.quirks.thinking_off, variant, "{template}");
        let protocol = req.provider.protocol;
        let snap = capture(req, suffix(protocol)).await;
        insta::assert_json_snapshot!(format!("thinking_off__{variant:?}"), snap["body"]);
    }
}

/// llama-server 等按 chat template 关思考的服务：`chat_template_kwargs.enable_thinking = false`（OpenAI 两种协议都发）
#[tokio::test]
async fn chat_template_kwargs_turns_thinking_off() {
    let mut req = from_template("lmstudio", "index-translate-2b", false);
    req.provider.quirks.thinking_off = ThinkingOff::ChatTemplateKwargs;
    let snap = capture(req, "/v1").await;
    assert_eq!(snap["body"]["chat_template_kwargs"], json!({"enable_thinking": false}));
    assert!(snap["body"].get("enable_thinking").is_none(), "不与通义的顶层字段混用");
    insta::assert_json_snapshot!("thinking_off__ChatTemplateKwargs", snap["body"]);

    let mut req = from_template("relay", "index-translate-2b", false);
    req.provider.protocol = Protocol::OpenAiResponses;
    req.provider.quirks.thinking_off = ThinkingOff::ChatTemplateKwargs;
    let snap = capture(req, "/v1").await;
    assert_eq!(snap["body"]["chat_template_kwargs"], json!({"enable_thinking": false}));

    // 关思考的字段只对 OpenAI 两种协议有意义：Anthropic / Gemini 上不发
    let mut req = from_template("gemini", "gemini-2.0-flash", false);
    req.provider.quirks.thinking_off = ThinkingOff::ChatTemplateKwargs;
    let snap = capture(req, "/v1beta").await;
    assert!(snap["body"].get("chat_template_kwargs").is_none());
}

#[tokio::test]
async fn qwen_merges_system_into_user_only_for_images() {
    let snap = capture(image_from_template("qwen", "qwen3-vl-plus", false), "/v1").await;
    let body = &snap["body"];
    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 1, "带图请求中没有 system");
    let parts = messages[0]["content"].as_array().expect("parts");
    assert_eq!(parts[0]["type"], "image_url", "图片在前");
    let merged = parts[1]["text"].as_str().expect("text");
    assert!(
        merged.starts_with("You are a translator.\n\nTranscribe the text."),
        "{merged}"
    );
    assert_eq!(body["enable_thinking"], false);
    insta::assert_json_snapshot!("qwen__image__merged", snap);

    // 纯文本请求不合并
    let snap = capture(from_template("qwen", "qwen3-vl-plus", false), "/v1").await;
    assert_eq!(snap["body"]["messages"][0]["role"], "system");
}

#[tokio::test]
async fn anthropic_thinking_and_effort_rules() {
    // 能力缓存确认支持时才发 thinking:{type:disabled}；OCR 用 medium
    let mut req = image_from_template("anthropic", "claude-haiku-4-5", false);
    req.model.caps.anthropic_thinking_disabled = Some(true);
    let snap = capture(req, "/v1").await;
    assert_eq!(snap["body"]["thinking"], json!({"type": "disabled"}));
    assert_eq!(snap["body"]["output_config"], json!({"effort": "medium"}));
    assert!(
        snap["body"].get("temperature").is_none(),
        "Anthropic 永不发 temperature"
    );
    assert!(
        snap["body"]["messages"][0]["content"]
            .as_array()
            .is_some_and(|parts| parts.iter().all(|p| p["type"] != "system")),
        "system 不在 messages 中"
    );
    assert_eq!(snap["body"]["system"], "You are a translator.");
    // 缺失或 false 时不发
    let mut req = from_template("anthropic", "claude-opus-5-5", false);
    req.model.caps.anthropic_thinking_disabled = Some(false);
    let snap = capture(req, "/v1").await;
    assert!(snap["body"].get("thinking").is_none());
    assert_eq!(snap["body"]["output_config"], json!({"effort": "low"}));
    assert_eq!(snap["body"]["max_tokens"], 1024, "thinking 没关时最大输出下限 1024");
}

#[tokio::test]
async fn auth_header_variants() {
    // 官方 host 只发 x-api-key；这里的 127.0.0.1 不是官方 host → 默认同时发 Bearer
    let snap = capture(from_template("anthropic", "claude-haiku-4-5", false), "/v1").await;
    assert_eq!(snap["headers"]["x-api-key"], "sk-test");
    assert_eq!(snap["headers"]["authorization"], "Bearer sk-test");
    assert_eq!(snap["headers"]["anthropic-version"], "2023-06-01");
    let mut req = from_template("anthropic", "claude-haiku-4-5", false);
    req.provider.quirks.dual_auth_header = Some(false);
    let snap = capture(req, "/v1").await;
    assert!(snap["headers"].get("authorization").is_none());

    // Gemini：默认 x-goog-api-key；开了 gemini_key_in_query 时放进 ?key=
    let snap = capture(from_template("gemini", "gemini-2.5-flash", false), "/v1beta").await;
    assert_eq!(snap["headers"]["x-goog-api-key"], "sk-test");
    assert_eq!(
        snap["target"],
        "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
    );
    let mut req = from_template("gemini", "gemini-2.5-flash", false);
    req.provider.quirks.gemini_key_in_query = true;
    let snap = capture(req, "/v1beta").await;
    assert!(snap["headers"].get("x-goog-api-key").is_none());
    assert_eq!(
        snap["target"],
        "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse&key=sk-test"
    );

    // 没有 Key（本地模型）：不发任何鉴权头
    let mut req = from_template("ollama", "qwen3-vl:8b", false);
    req.key = None;
    let snap = capture(req, "/v1").await;
    assert!(snap["headers"].get("authorization").is_none());
    let mut req = from_template("ollama", "qwen3-vl:8b", false);
    req.key = Some(wisp_llm::Secret::new("   ".to_owned()));
    let snap = capture(req, "/v1").await;
    assert!(snap["headers"].get("authorization").is_none(), "空白 Key 视同没有");
}

#[tokio::test]
async fn custom_headers_are_appended_and_can_override() {
    let mut req = from_template("openrouter", "openai/gpt-5", false);
    req.provider
        .headers
        .insert("HTTP-Referer".to_owned(), "https://wisp.app".to_owned());
    req.provider.headers.insert("X-Title".to_owned(), "Wisp".to_owned());
    req.provider
        .headers
        .insert("OpenAI-Organization".to_owned(), "org-1".to_owned());
    req.provider.headers.insert("Accept".to_owned(), "*/*".to_owned());
    // 含换行的值不能发出去（保存时由 ipc 层拒绝，这里兜底跳过）
    req.provider
        .headers
        .insert("X-Evil".to_owned(), "a\r\nInjected: 1".to_owned());
    let snap = capture(req, "/api/v1").await;
    assert_eq!(snap["headers"]["http-referer"], "https://wisp.app");
    assert_eq!(snap["headers"]["x-title"], "Wisp");
    assert_eq!(snap["headers"]["openai-organization"], "org-1");
    assert_eq!(snap["headers"]["accept"], "*/*", "自定义头覆盖协议默认头");
    assert_eq!(snap["target"], "/api/v1/chat/completions");
}

#[tokio::test]
async fn openai_rules_on_the_wire() {
    // gpt-5.4-mini 没有 temperature，有 max_completion_tokens 与 effort low
    let snap = capture(from_template("openai", "gpt-5.4-mini", false), "/v1").await;
    let b = &snap["body"];
    assert!(b.get("temperature").is_none());
    assert_eq!(b["max_completion_tokens"], 1024);
    assert_eq!(b["reasoning_effort"], "low");
    assert_eq!(b["stream_options"], json!({"include_usage": true}));
    assert_eq!(b["messages"][0]["role"], "system", "用 system 而不是 developer");
    // gpt-4.1：有 temperature
    let snap = capture(from_template("openai", "gpt-4.1", false), "/v1").await;
    assert_eq!(snap["body"]["temperature"], 0.3);
    assert_eq!(snap["body"]["max_completion_tokens"], 556);
    // Responses 总是 store:false；带图时 image_url 是字符串
    let snap = capture(image_from_template("openai-responses", "gpt-4.1", false), "/v1").await;
    assert_eq!(snap["body"]["store"], false);
    assert!(snap["body"]["input"][0]["content"][0]["image_url"].is_string());
    // 火山总是显式发送 max_tokens
    let snap = capture(from_template("volcengine", "doubao-seed-2-1-pro", false), "/api/v3").await;
    assert_eq!(snap["body"]["max_tokens"], 556);
    assert_eq!(snap["body"]["thinking"], json!({"type": "disabled"}));
    // OCR 探针以外的推理模型都有下限；探针保持 1
    let mut req = from_template("openai", "o3", false);
    req.purpose = Purpose::Probe;
    req.max_output_tokens = 1;
    req.temperature = None;
    let snap = capture(req, "/v1").await;
    assert_eq!(snap["body"]["max_completion_tokens"], 1);
}

#[tokio::test]
async fn gemini_body_is_camel_case_without_safety_settings() {
    let mut req = image_from_template("gemini", "gemini-2.5-flash", false);
    req.model = ModelEntry::new("models/gemini-2.5-flash");
    let snap = capture(req, "/v1beta").await;
    let b = &snap["body"];
    assert!(b.get("safetySettings").is_none());
    assert!(b.get("includeThoughts").is_none());
    assert_eq!(b["contents"][0]["parts"][0]["inlineData"]["mimeType"], "image/png");
    assert_eq!(b["generationConfig"]["thinkingConfig"], json!({"thinkingBudget": 0}));
    assert_eq!(b["generationConfig"]["temperature"], 0.0);
    assert_eq!(
        snap["target"],
        "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
    );
}

#[test]
fn template_prefills() {
    // 17 个模板的预填值（name / protocol / base_url / quirks / timeouts / 区域变体 / 预置模型）
    insta::assert_json_snapshot!("templates", wisp_llm::templates());
}
