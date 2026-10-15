//! URL 测试向量：
//! - 12 组 base_url 规范化向量（openai/v1/、deepseek、ollama、v4、v3、compatible-mode/v1、anthropic、
//!   deepseek/anthropic、gemini、custom/path#、relay.example.com、V1Beta）；
//! - 4 种协议 × 3 种写法（无版本段 / 已有版本段 / `#` 结尾）共 12 组最终 URL；
//! - 模型列表端点与 URL 预览。

use wisp_llm::{Endpoint, Protocol, endpoint_url, normalize_base_url, url_preview};

#[test]
fn twelve_normalization_vectors() {
    use Protocol::{AnthropicMessages as A, Gemini as G, OpenAiChat as C};
    let vectors: [(&str, Protocol, &str); 12] = [
        ("https://api.openai.com/v1/", C, "https://api.openai.com/v1"),
        ("https://api.deepseek.com", C, "https://api.deepseek.com/v1"),
        ("http://localhost:11434", C, "http://localhost:11434/v1"),
        (
            "https://open.bigmodel.cn/api/paas/v4",
            C,
            "https://open.bigmodel.cn/api/paas/v4",
        ),
        (
            "https://ark.cn-beijing.volces.com/api/v3",
            C,
            "https://ark.cn-beijing.volces.com/api/v3",
        ),
        (
            "https://dashscope.aliyuncs.com/compatible-mode/v1",
            C,
            "https://dashscope.aliyuncs.com/compatible-mode/v1",
        ),
        ("https://api.anthropic.com", A, "https://api.anthropic.com/v1"),
        (
            "https://api.deepseek.com/anthropic",
            A,
            "https://api.deepseek.com/anthropic/v1",
        ),
        (
            "https://generativelanguage.googleapis.com",
            G,
            "https://generativelanguage.googleapis.com/v1beta",
        ),
        ("https://example.com/custom/path#", C, "https://example.com/custom/path"),
        ("  relay.example.com/  ", C, "https://relay.example.com/v1"),
        (
            "https://proxy.example.com/V1Beta",
            G,
            "https://proxy.example.com/V1Beta",
        ),
    ];
    for (input, protocol, expected) in vectors {
        assert_eq!(normalize_base_url(input, protocol), expected, "{input} / {protocol:?}");
    }
}

#[test]
fn four_protocols_times_three_spellings() {
    use Protocol::{AnthropicMessages as A, Gemini as G, OpenAiChat as C, OpenAiResponses as R};
    let site = "https://relay.example.com";
    let m = Some("gemini-3-flash-preview");
    let cases: [(&str, Protocol, &str); 12] = [
        // 无 #、路径无版本段：补版本段
        (
            "https://relay.example.com",
            C,
            "https://relay.example.com/v1/chat/completions",
        ),
        ("https://relay.example.com", R, "https://relay.example.com/v1/responses"),
        ("https://relay.example.com", A, "https://relay.example.com/v1/messages"),
        (
            "https://relay.example.com",
            G,
            "https://relay.example.com/v1beta/models/gemini-3-flash-preview:streamGenerateContent?alt=sse",
        ),
        // 无 #、路径已含版本段：不再追加
        (
            "https://relay.example.com/v1",
            C,
            "https://relay.example.com/v1/chat/completions",
        ),
        (
            "https://relay.example.com/v1",
            R,
            "https://relay.example.com/v1/responses",
        ),
        (
            "https://relay.example.com/v1",
            A,
            "https://relay.example.com/v1/messages",
        ),
        (
            "https://relay.example.com/v1beta",
            G,
            "https://relay.example.com/v1beta/models/gemini-3-flash-preview:streamGenerateContent?alt=sse",
        ),
        // 有 #：按原样只拼资源名
        (
            "https://relay.example.com/openai#",
            C,
            "https://relay.example.com/openai/chat/completions",
        ),
        (
            "https://relay.example.com/openai/#",
            R,
            "https://relay.example.com/openai/responses",
        ),
        (
            "https://relay.example.com/claude#",
            A,
            "https://relay.example.com/claude/messages",
        ),
        (
            "https://relay.example.com/gemini#",
            G,
            "https://relay.example.com/gemini/models/gemini-3-flash-preview:streamGenerateContent?alt=sse",
        ),
    ];
    for (base, protocol, expected) in cases {
        assert_eq!(
            endpoint_url(base, protocol, Endpoint::Chat, m),
            expected,
            "{base} / {protocol:?}"
        );
    }
    // 同一个站点根在四种协议下都正确
    assert_eq!(
        endpoint_url(site, A, Endpoint::Chat, None),
        "https://relay.example.com/v1/messages"
    );
}

#[test]
fn template_urls() {
    let cases = [
        (
            "https://api.openai.com/v1",
            Protocol::OpenAiChat,
            "https://api.openai.com/v1/chat/completions",
        ),
        (
            "https://api.openai.com/v1",
            Protocol::OpenAiResponses,
            "https://api.openai.com/v1/responses",
        ),
        (
            "https://api.anthropic.com",
            Protocol::AnthropicMessages,
            "https://api.anthropic.com/v1/messages",
        ),
        (
            "https://open.bigmodel.cn/api/paas/v4",
            Protocol::OpenAiChat,
            "https://open.bigmodel.cn/api/paas/v4/chat/completions",
        ),
        (
            "https://api.groq.com/openai/v1",
            Protocol::OpenAiChat,
            "https://api.groq.com/openai/v1/chat/completions",
        ),
        (
            "http://localhost:11434",
            Protocol::OpenAiChat,
            "http://localhost:11434/v1/chat/completions",
        ),
        (
            "http://localhost:1234",
            Protocol::OpenAiChat,
            "http://localhost:1234/v1/chat/completions",
        ),
    ];
    for (base, protocol, expected) in cases {
        assert_eq!(endpoint_url(base, protocol, Endpoint::Chat, None), expected);
    }
    assert_eq!(
        endpoint_url(
            "https://generativelanguage.googleapis.com",
            Protocol::Gemini,
            Endpoint::Chat,
            Some("models/gemini-2.5-flash")
        ),
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse",
        "Gemini 模型名去掉 models/ 前缀"
    );
}

#[test]
fn models_endpoints() {
    let cases = [
        (Protocol::OpenAiChat, "https://api.openai.com/v1/models"),
        (Protocol::OpenAiResponses, "https://api.openai.com/v1/models"),
        (
            Protocol::AnthropicMessages,
            "https://api.openai.com/v1/models?limit=1000",
        ),
        (Protocol::Gemini, "https://api.openai.com/v1/models?pageSize=1000"),
    ];
    for (protocol, expected) in cases {
        assert_eq!(
            endpoint_url("https://api.openai.com/v1", protocol, Endpoint::Models, None),
            expected
        );
    }
}

#[test]
fn preview_reports_version_and_force() {
    // 本机地址没写 scheme 时补 http（Ollama / LM Studio 都是明文 http）
    let p = url_preview(Protocol::OpenAiChat, "localhost:11434", None);
    assert_eq!(p.method, "POST");
    assert_eq!(p.url, "http://localhost:11434/v1/chat/completions");
    assert!(p.version_appended && !p.forced);

    let p = url_preview(Protocol::OpenAiChat, "http://localhost:11434#", None);
    assert_eq!(p.url, "http://localhost:11434/chat/completions");
    assert!(!p.version_appended && p.forced);

    let p = url_preview(Protocol::Gemini, "https://generativelanguage.googleapis.com", None);
    assert_eq!(
        p.url,
        "https://generativelanguage.googleapis.com/v1beta/models/{model}:streamGenerateContent?alt=sse"
    );
}
