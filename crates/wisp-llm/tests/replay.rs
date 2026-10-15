//! SSE 回放测试：读取 `fixtures/sse/*.sse`，分别按 1 字节、7 字节、整块三种方式
//! 切成 HTTP chunk，经脚本化服务器喂给 `LlmClient::stream`，断言事件序列（三种切分的结果必须完全相同）。
//! 另有两个 HTTP 4xx 样本（OpenAI text/plain 401、Gemini 400 API_KEY_INVALID）、「2xx 却是 HTML」样本，
//! 以及 2xx 的整个 JSON 却是出错形态的样本（MiniMax 的 base_resp、中转站的 {code, msg}）。
//! 测试进程不手动安装 CryptoProvider（验证 wisp-llm 的兜底安装）。

mod common;

use std::path::PathBuf;

use common::{Reply, ScriptServer, assert_valid_sequence, client, provider, request, split};
use wisp_llm::{ErrorCode, ErrorSubcode, Protocol, StreamEvent, TruncateReason, Usage};

/// 期望的结果
enum Expect {
    Done {
        text: &'static str,
        usage: Option<Usage>,
        truncated: bool,
    },
    Error {
        text: &'static str,
        code: ErrorCode,
        subcode: Option<ErrorSubcode>,
    },
}

struct Case {
    file: &'static str,
    protocol: Protocol,
    upstream_id: Option<&'static str>,
    expect: Expect,
}

fn usage(input: u32, output: u32, reasoning: Option<u32>, total: Option<u32>) -> Option<Usage> {
    Some(Usage {
        input_tokens: input,
        output_tokens: output,
        reasoning_tokens: reasoning,
        total_tokens: total,
    })
}

fn cases() -> Vec<Case> {
    use Expect::{Done, Error};
    use Protocol::{AnthropicMessages as Anthropic, Gemini, OpenAiChat as Chat, OpenAiResponses as Responses};
    vec![
        // Start、Delta('Hello')、Done{17/9/26}
        Case {
            file: "openai_chat.sse",
            protocol: Chat,
            upstream_id: Some("chatcmpl-123"),
            expect: Done {
                text: "Hello",
                usage: usage(17, 9, Some(0), Some(26)),
                truncated: false,
            },
        },
        // Start、Delta('Hi')、Done{37/11/48}
        Case {
            file: "openai_responses.sse",
            protocol: Responses,
            upstream_id: Some("resp_67c9fdcecf488190bdd9a0409de3a1ec07b8b0ad4e5eb654"),
            expect: Done {
                text: "Hi",
                usage: usage(37, 11, Some(0), Some(48)),
                truncated: false,
            },
        },
        // 错误事件 → upstream
        Case {
            file: "openai_responses_error.sse",
            protocol: Responses,
            upstream_id: None,
            expect: Error {
                text: "",
                code: ErrorCode::Upstream,
                subcode: None,
            },
        },
        Case {
            file: "openai_responses_incomplete.sse",
            protocol: Responses,
            upstream_id: Some("resp_inc"),
            expect: Done {
                text: "Part",
                usage: usage(9, 16, Some(12), Some(25)),
                truncated: true,
            },
        },
        // rate_limit_exceeded → rate_limited
        Case {
            file: "openai_responses_failed.sse",
            protocol: Responses,
            upstream_id: Some("resp_fail"),
            expect: Error {
                text: "",
                code: ErrorCode::RateLimited,
                subcode: None,
            },
        },
        // 'Hello!' 25/15（含 ping）
        Case {
            file: "anthropic.sse",
            protocol: Anthropic,
            upstream_id: Some("msg_1nZdL29xx5MUA1yADyHTEsnR8uuvGzszyY"),
            expect: Done {
                text: "Hello!",
                usage: usage(25, 15, None, None),
                truncated: false,
            },
        },
        // Claude 5 默认思考：空 thinking 块之后才是 index 1 的文本
        Case {
            file: "anthropic_thinking.sse",
            protocol: Anthropic,
            upstream_id: Some("msg_think"),
            expect: Done {
                text: "你好，世界",
                usage: usage(12, 7, None, None),
                truncated: false,
            },
        },
        // 流中 overloaded_error → upstream（可重试），已有文本保留
        Case {
            file: "anthropic_error.sse",
            protocol: Anthropic,
            upstream_id: Some("msg_err"),
            expect: Error {
                text: "Hel",
                code: ErrorCode::Upstream,
                subcode: None,
            },
        },
        // 完整文本，用量 2/9/11
        Case {
            file: "gemini.sse",
            protocol: Gemini,
            upstream_id: Some("_GLlaMO0Gbec1e8PptCfkAQ"),
            expect: Done {
                text: "Hello! How can I help you today?",
                usage: usage(2, 9, None, Some(11)),
                truncated: false,
            },
        },
        // SAFETY 且没有文本 → content_filter
        Case {
            file: "gemini_safety.sse",
            protocol: Gemini,
            upstream_id: Some("gem-safe"),
            expect: Error {
                text: "",
                code: ErrorCode::ContentFilter,
                subcode: None,
            },
        },
        // OpenRouter：注释心跳；usage 块里第二次出现的 finish_reason 不会再发一个 Done
        Case {
            file: "openrouter_usage.sse",
            protocol: Chat,
            upstream_id: Some("gen-1"),
            expect: Done {
                text: "Bonjour",
                usage: usage(10, 4, None, Some(14)),
                truncated: false,
            },
        },
        // OpenRouter：200 + error chunk 作为第一个事件 → 失败（没有 Start）
        Case {
            file: "openrouter_error.sse",
            protocol: Chat,
            upstream_id: None,
            expect: Error {
                text: "",
                code: ErrorCode::Upstream,
                subcode: None,
            },
        },
        // DeepSeek：reasoning_content 先出现（content:null），丢弃
        Case {
            file: "deepseek_reasoning.sse",
            protocol: Chat,
            upstream_id: Some("ds-1"),
            expect: Done {
                text: "你好！",
                usage: usage(8, 20, Some(17), Some(28)),
                truncated: false,
            },
        },
        // MiniMax：content 内联 <think>，跨 chunk 剥离
        Case {
            file: "minimax_think.sse",
            protocol: Chat,
            upstream_id: Some("mm-1"),
            expect: Done {
                text: "敏捷的棕色狐狸",
                usage: usage(30, 25, None, Some(55)),
                truncated: false,
            },
        },
        // 没有 [DONE]：看到 finish_reason 后 EOF
        Case {
            file: "chat_no_done.sse",
            protocol: Chat,
            upstream_id: Some("c-1"),
            expect: Done {
                text: "Hi there",
                usage: None,
                truncated: false,
            },
        },
        // 没有任何结束标记就 EOF → network / connection_lost，文本保留
        Case {
            file: "chat_eof_no_finish.sse",
            protocol: Chat,
            upstream_id: Some("c-2"),
            expect: Error {
                text: "半句话",
                code: ErrorCode::Network,
                subcode: Some(ErrorSubcode::ConnectionLost),
            },
        },
        Case {
            file: "chat_length.sse",
            protocol: Chat,
            upstream_id: Some("c-3"),
            expect: Done {
                text: "Once upon",
                usage: usage(5, 2, None, Some(7)),
                truncated: true,
            },
        },
        // \r\n、data: 无空格、注释行、最后一行没有换行；1 字节切分时多字节字符被切开也不乱码
        Case {
            file: "chat_crlf_nospace.sse",
            protocol: Chat,
            upstream_id: Some("c-4"),
            expect: Done {
                text: "多字节字符😀",
                usage: None,
                truncated: false,
            },
        },
        // 单个坏行不报错
        Case {
            file: "chat_one_bad_line.sse",
            protocol: Chat,
            upstream_id: Some("c-5"),
            expect: Done {
                text: "ab",
                usage: None,
                truncated: false,
            },
        },
        // 连续 3 个坏行 → protocol
        Case {
            file: "chat_three_bad_lines.sse",
            protocol: Chat,
            upstream_id: Some("c-6"),
            expect: Error {
                text: "a",
                code: ErrorCode::Protocol,
                subcode: None,
            },
        },
        // MiniMax：第一个 data 就是 base_resp 错误（choices:null，1008 余额不足）→ quota，而不是「连接中断」
        Case {
            file: "minimax_base_resp_first.sse",
            protocol: Chat,
            upstream_id: None,
            expect: Error {
                text: "",
                code: ErrorCode::Quota,
                subcode: None,
            },
        },
        // MiniMax：输出到一半 base_resp 报 1027（输出涉敏）→ content_filter，已有文本保留
        Case {
            file: "minimax_base_resp_mid.sse",
            protocol: Chat,
            upstream_id: Some("mm-2"),
            expect: Error {
                text: "敏捷的棕色",
                code: ErrorCode::ContentFilter,
                subcode: None,
            },
        },
        // Anthropic：message_delta / message_stop 之前就 EOF → network / connection_lost，文本保留
        Case {
            file: "anthropic_eof_no_stop.sse",
            protocol: Anthropic,
            upstream_id: Some("msg_eof"),
            expect: Error {
                text: "半句话",
                code: ErrorCode::Network,
                subcode: Some(ErrorSubcode::ConnectionLost),
            },
        },
        // Gemini：没有 finishReason 就 EOF → network / connection_lost，文本保留
        Case {
            file: "gemini_eof_no_finish.sse",
            protocol: Gemini,
            upstream_id: Some("gem-eof"),
            expect: Error {
                text: "半句话",
                code: ErrorCode::Network,
                subcode: Some(ErrorSubcode::ConnectionLost),
            },
        },
    ]
}

fn fixture(rel: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(rel);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// 去掉耗时后用于比较的形状
fn shape(events: &[StreamEvent]) -> Vec<String> {
    events
        .iter()
        .map(|e| match e {
            StreamEvent::Done {
                usage,
                truncated,
                cached,
                no_text,
                ..
            } => format!("done usage={usage:?} truncated={truncated:?} cached={cached} no_text={no_text}"),
            // 每次切分用不同端口的服务器：比较时去掉 url
            StreamEvent::Error(e) => {
                let mut e = e.clone();
                e.url = None;
                format!("{e:?}")
            }
            other => format!("{other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn every_fixture_replays_identically_with_any_split() {
    let all_files: Vec<String> =
        std::fs::read_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sse"))
            .expect("fixtures dir")
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect();
    let cases = cases();
    for f in &all_files {
        assert!(cases.iter().any(|c| c.file == f), "fixture {f} has no expectation");
    }

    for case in cases {
        let bytes = fixture(&format!("sse/{}", case.file));
        let mut shapes = Vec::new();
        for size in [1, 7, usize::MAX] {
            let server = ScriptServer::start(vec![Reply::sse(split(&bytes, size))]).await;
            let (client, _) = client();
            let mut p = provider(case.protocol, &format!("{}/v1", server.base));
            p.quirks.strip_think_tags = true;
            let events = common::collect(client.stream(request(p, "m", "hi"), Default::default())).await;
            assert_valid_sequence(&events);
            check(&case, &events, size);
            shapes.push(shape(&events));
        }
        assert_eq!(shapes[0], shapes[2], "{}: 1-byte split differs from whole", case.file);
        assert_eq!(shapes[1], shapes[2], "{}: 7-byte split differs from whole", case.file);
    }
}

fn check(case: &Case, events: &[StreamEvent], size: usize) {
    let ctx = format!("{} (split {size})", case.file);
    let text = common::text_of(events);
    if let StreamEvent::Start { upstream_id, .. } = &events[0] {
        assert_eq!(upstream_id.as_deref(), case.upstream_id, "{ctx}: upstream id");
    }
    match (&case.expect, common::last(events)) {
        (
            Expect::Done {
                text: t,
                usage: u,
                truncated: tr,
            },
            StreamEvent::Done {
                usage,
                truncated,
                cached,
                no_text,
                ..
            },
        ) => {
            assert_eq!(text, *t, "{ctx}: text");
            assert_eq!(usage, u, "{ctx}: usage");
            assert_eq!(truncated.is_some(), *tr, "{ctx}: truncated");
            if *tr {
                assert_eq!(*truncated, Some(TruncateReason::MaxTokens));
            }
            assert!(!cached);
            assert_eq!(*no_text, t.is_empty(), "{ctx}: no_text");
        }
        (Expect::Error { text: t, code, subcode }, StreamEvent::Error(e)) => {
            assert_eq!(text, *t, "{ctx}: retained text");
            assert_eq!(e.code, *code, "{ctx}: code ({e:?})");
            if subcode.is_some() {
                assert_eq!(e.subcode, *subcode, "{ctx}: subcode");
            }
            if *code == ErrorCode::Upstream || *code == ErrorCode::Network {
                assert!(e.retryable, "{ctx}: retryable");
            }
            assert!(
                e.url.as_deref().is_some_and(|u| u.starts_with("127.0.0.1:")),
                "{ctx}: url host+path"
            );
        }
        (_, other) => panic!("{ctx}: unexpected terminal {other:?}"),
    }
}

#[tokio::test]
async fn openai_401_with_text_plain_is_auth() {
    let body = String::from_utf8(fixture("http/openai_401_text_plain.body")).expect("utf8");
    let server = ScriptServer::start(vec![
        Reply::body(401, "text/plain", &body).header("x-request-id", "req_8f2c1a"),
    ])
    .await;
    let (client, _) = client();
    let events = common::run(
        &client,
        request(provider(Protocol::OpenAiChat, &server.base), "gpt-5.4-mini", "hi"),
    )
    .await;
    common::assert_failed_before_stream(&events);
    assert!(
        matches!(&events[0], StreamEvent::Start { upstream_id: Some(id), .. } if id == "req_8f2c1a"),
        "流开始前失败也先有 Start，并带上游 id：{events:?}"
    );
    let e = common::error_of(&events);
    assert_eq!(e.code, ErrorCode::Auth);
    assert_eq!(e.status, Some(401));
    assert!(e.message.starts_with("Incorrect API key provided"));
    assert_eq!(e.hint.as_deref(), Some("errors.hint.check_key"));
    assert_eq!(e.upstream_request_id.as_deref(), Some("req_8f2c1a"));
    assert!(!e.retryable);
    assert_eq!(server.request_count(), 1, "401 不重试");
}

#[tokio::test]
async fn gemini_400_api_key_invalid_with_sse_content_type_is_auth() {
    let body = String::from_utf8(fixture("http/gemini_400_api_key_invalid.body")).expect("utf8");
    let server = ScriptServer::start(vec![Reply::body(400, "text/event-stream", &body)]).await;
    let (client, _) = client();
    let events = common::run(
        &client,
        request(provider(Protocol::Gemini, &server.base), "gemini-2.5-flash", "hi"),
    )
    .await;
    let e = common::error_of(&events);
    assert_eq!(e.code, ErrorCode::Auth);
    assert_eq!(e.message, "API key not valid. Please pass a valid API key.");
    assert_eq!(e.hint.as_deref(), Some("errors.hint.check_key"));
    assert_eq!(server.request_count(), 1);
}

#[tokio::test]
async fn html_page_with_200_is_protocol_error() {
    let body = String::from_utf8(fixture("http/html_200.body")).expect("utf8");
    let server = ScriptServer::start(vec![Reply::body(200, "text/html", &body)]).await;
    let (client, _) = client();
    let events = common::run(
        &client,
        request(provider(Protocol::OpenAiChat, &server.base), "gpt-4.1", "hi"),
    )
    .await;
    let e = common::error_of(&events);
    assert_eq!(e.code, ErrorCode::Protocol);
    assert_eq!(e.subcode, Some(ErrorSubcode::HtmlBody));
    assert_eq!(e.hint.as_deref(), Some("errors.hint.check_protocol"));
}

#[tokio::test]
async fn non_stream_json_response_is_accepted() {
    // 端点忽略了 stream:true，直接返回整个 JSON：仍然得到完整文本与用量
    let body = r#"{"id":"chatcmpl-9","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"整段译文"},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":3,"total_tokens":7}}"#;
    let server = ScriptServer::start(vec![Reply::body(200, "application/json", body)]).await;
    let (client, _) = client();
    let events = common::run(
        &client,
        request(provider(Protocol::OpenAiChat, &server.base), "gpt-4.1", "hi"),
    )
    .await;
    assert_eq!(common::text_of(&events), "整段译文");
    assert!(matches!(
        common::last(&events),
        StreamEvent::Done { usage: Some(u), .. } if u.total_tokens == Some(7)
    ));
}

#[tokio::test]
async fn minimax_200_with_base_resp_is_auth_not_connection_lost() {
    // MiniMax 填错 Key：HTTP 200 + 整个 JSON，choices 为 null，base_resp 1004 login fail（填错 Key 应显示 auth）
    let body = String::from_utf8(fixture("http/minimax_200_base_resp.body")).expect("utf8");
    for content_type in ["application/json", "text/event-stream"] {
        let server = ScriptServer::start(vec![Reply::body(200, content_type, &body)]).await;
        let (client, _) = client();
        let mut p = wisp_llm::provider_from_template("minimax", &[]).expect("template");
        p.base_url = format!("{}/v1", server.base);
        let events = common::run(&client, request(p, "MiniMax-M2.7", "hi")).await;
        common::assert_failed_before_stream(&events);
        let e = common::error_of(&events);
        assert_eq!(e.code, ErrorCode::Auth, "{content_type}: {e:?}");
        assert_eq!(e.subcode, None);
        assert!(!e.retryable);
        assert_eq!(e.hint.as_deref(), Some("errors.hint.check_key"));
        assert!(
            e.message.starts_with("login fail: Please carry the API secret key"),
            "{}",
            e.message
        );
        assert_eq!(server.request_count(), 1, "auth 不重试");
    }
}

#[tokio::test]
async fn relay_200_with_code_and_msg_is_classified() {
    let body = String::from_utf8(fixture("http/relay_200_code_msg.body")).expect("utf8");
    let server = ScriptServer::start(vec![Reply::body(200, "application/json", &body)]).await;
    let (client, _) = client();
    let events = common::run(
        &client,
        request(
            provider(Protocol::OpenAiChat, &format!("{}/v1", server.base)),
            "gpt-4.1",
            "hi",
        ),
    )
    .await;
    common::assert_failed_before_stream(&events);
    let e = common::error_of(&events);
    assert_eq!(e.code, ErrorCode::Auth);
    assert!(e.message.starts_with("无效的令牌"), "{}", e.message);
}

#[tokio::test]
async fn unrecognised_json_with_200_is_protocol_not_network() {
    // 形状认不出的 2xx JSON 一律归 protocol（多半是模板或 base_url 不对），绝不报「连接中断」
    for (protocol, body) in [
        (Protocol::OpenAiChat, r#"{"status":"ok","data":{"items":[]}}"#),
        (Protocol::OpenAiResponses, r#"{"result":"queued"}"#),
        (Protocol::AnthropicMessages, r#"{"detail":"Not Found"}"#),
        (Protocol::Gemini, r#"{"kind":"unknown"}"#),
        (Protocol::OpenAiChat, "[]"),
    ] {
        let server = ScriptServer::start(vec![Reply::body(200, "application/json", body)]).await;
        let (client, _) = client();
        let events = common::run(
            &client,
            request(provider(protocol, &format!("{}/v1", server.base)), "m", "hi"),
        )
        .await;
        let e = common::error_of(&events);
        assert_eq!(e.code, ErrorCode::Protocol, "{protocol:?} {body}: {e:?}");
        assert_eq!(e.subcode, None);
        assert_eq!(e.hint.as_deref(), Some("errors.hint.check_protocol"));
    }
    // 有信息字段时显示服务端原文
    let server = ScriptServer::start(vec![Reply::body(200, "application/json", r#"{"detail":"Not Found"}"#)]).await;
    let (client, _) = client();
    let events = common::run(
        &client,
        request(
            provider(Protocol::AnthropicMessages, &format!("{}/v1", server.base)),
            "m",
            "hi",
        ),
    )
    .await;
    assert_eq!(common::error_of(&events).message, "Not Found");
}
