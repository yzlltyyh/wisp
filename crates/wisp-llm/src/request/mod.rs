//! 请求体与请求头构造；四协议的快照测试见 tests/request_snapshots.rs。
//!
//! [`Job`] 在一次请求开始时算好一次（URL、host 等）；[`build`] 按当前能力缓存生成请求体，调用方在重试时复用
//! 同一份请求体（`Bytes`，零拷贝），只有降级改了能力缓存时才重新生成。请求体与请求头的值都不写日志。
//!
//! 带图请求的内存：先用占位符序列化出请求体骨架（几百字节），再把图片的 base64 直接编码进最终的请求体缓冲区，
//! 整个请求只有这一份 base64——不另存 base64 字符串，也不经过 `serde_json::Value`（5 MB 的图片约 6.7 MB，
//! 原先同时存在三份）。

mod anthropic;
mod gemini;
mod openai_chat;
mod openai_responses;
pub(crate) mod params;

use std::fmt;

use base64::Engine as _;
use bytes::Bytes;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value};
use wisp_types::{ImageDetail, LlmError, ModelCaps, Protocol, Provider, Quirks};

use crate::client::{ChatRequest, UserContent};
use crate::degrade::SentParam;
use crate::errors;
use crate::secret::Secret;
use crate::url::{self, Endpoint};
use params::{Plan, PlanInput};

/// Anthropic 必须发送的版本头
pub(crate) const ANTHROPIC_VERSION: &str = "2023-06-01";

/// 请求体骨架里图片 base64 的占位符。serde_json 只转义控制字符、引号和反斜杠，所以它序列化后原样出现，
/// 随后被原地换成 base64（base64 字母表不需要 JSON 转义）
const IMAGE_SLOT: &str = "\u{FFFF}wisp-image-base64\u{FFFF}";

/// 一次请求开始时算好的上下文
pub(crate) struct Job {
    /// 实际请求的 URL（Gemini 开了 key_in_query 时含 Key —— 只能交给 reqwest，不能展示或记日志）
    pub url: String,
    /// 不含 Key 的完整 URL
    pub safe_url: String,
    /// 小写 host
    pub host: String,
    /// 回环地址：不走系统代理
    pub direct: bool,
    /// 对话端点 404 时提示加 / 去 /v1
    pub version_hint: bool,
    /// 请求带图
    pub has_image: bool,
}

/// 手写 Debug：`url` 可能含 Key，任何调试输出里都只出现不含 Key 的 `safe_url`
impl fmt::Debug for Job {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Job")
            .field("safe_url", &self.safe_url)
            .field("host", &self.host)
            .field("direct", &self.direct)
            .field("version_hint", &self.version_hint)
            .field("has_image", &self.has_image)
            .finish_non_exhaustive()
    }
}

/// 一次 HTTP 调用的请求（重试时整体复用：`Bytes` 与 `HeaderMap` 的克隆都很廉价）
pub(crate) struct Built {
    pub body: Bytes,
    pub headers: HeaderMap,
    /// 本次发送了的可降级参数
    pub sent: Vec<SentParam>,
}

/// 计算请求上下文；base_url 或模型为空时返回 not_configured
pub(crate) fn prepare(req: &ChatRequest) -> Result<Job, LlmError> {
    let p = &req.provider;
    if req.model.id.trim().is_empty() {
        return Err(errors::not_configured("no model is selected"));
    }
    let n = url::normalize(&p.base_url, p.protocol);
    if n.prefix.is_empty() {
        return Err(errors::not_configured("the provider has no base URL"));
    }
    let safe_url = n.url(p.protocol, Endpoint::Chat, Some(&req.model.id));
    let url = with_query_key(p, &safe_url, req.key.as_ref());
    let host = url::host_of(&safe_url);
    Ok(Job {
        direct: url::is_loopback_host(&host),
        version_hint: !n.forced && n.version_appended,
        host,
        has_image: matches!(req.user, UserContent::Image { .. }),
        url,
        safe_url,
    })
}

/// Gemini 的 `gemini_key_in_query`：Key 放进 `?key=`（中转站常见），否则原样返回
pub(crate) fn with_query_key(p: &Provider, url: &str, key: Option<&Secret>) -> String {
    match key.filter(|k| !k.expose().trim().is_empty()) {
        Some(k) if p.protocol == Protocol::Gemini && p.quirks.gemini_key_in_query => {
            let sep = if url.contains('?') { '&' } else { '?' };
            let encoded: String = ::url::form_urlencoded::byte_serialize(k.expose().trim().as_bytes()).collect();
            format!("{url}{sep}key={encoded}")
        }
        _ => url.to_owned(),
    }
}

/// 生成一次 HTTP 调用的请求体与请求头
pub(crate) fn build(job: &Job, req: &ChatRequest, caps: &ModelCaps) -> Result<Built, LlmError> {
    let p = &req.provider;
    let (user_text, image, image_detail) = match &req.user {
        UserContent::Text(t) => (t.as_str(), None, ImageDetail::High),
        UserContent::Image {
            bytes,
            mime,
            text,
            detail,
        } => (text.as_str(), Some((*mime, bytes.as_ref())), *detail),
    };
    let plan = params::plan(&PlanInput {
        protocol: p.protocol,
        quirks: &p.quirks,
        model: &req.model,
        caps,
        purpose: req.purpose,
        temperature: req.temperature,
        max_output_tokens: req.max_output_tokens,
        host: &job.host,
        has_image: image.is_some(),
        image_detail,
    });
    let parts = |image| Parts {
        model: &req.model.id,
        system: &req.system,
        user_text,
        image,
        plan: &plan,
    };
    let mut sent = Vec::new();
    let body = match image {
        None => serialize(p.protocol, &parts(None), &mut sent)?,
        Some((mime, bytes)) => {
            let skeleton = serialize(p.protocol, &parts(Some((mime, IMAGE_SLOT))), &mut sent)?;
            match splice_image(&skeleton, bytes) {
                Some(body) => body,
                None => {
                    // 占位符出现了不止一次（文本里恰好含有它）：退回把 base64 直接放进 JSON
                    drop(skeleton);
                    sent.clear();
                    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
                    serialize(p.protocol, &parts(Some((mime, &b64))), &mut sent)?
                }
            }
        }
    };
    let headers = headers(p, &job.host, req.key.as_ref(), "text/event-stream")?;
    Ok(Built {
        body: Bytes::from(body),
        headers,
        sent,
    })
}

fn serialize(protocol: Protocol, parts: &Parts<'_>, sent: &mut Vec<SentParam>) -> Result<Vec<u8>, LlmError> {
    let body = match protocol {
        Protocol::OpenAiChat => openai_chat::body(parts, sent),
        Protocol::OpenAiResponses => openai_responses::body(parts, sent),
        Protocol::AnthropicMessages => anthropic::body(parts, sent),
        Protocol::Gemini => gemini::body(parts, sent),
    };
    serde_json::to_vec(&Value::Object(body)).map_err(|e| errors::internal(format!("serialize: {e}")))
}

/// 把骨架里唯一的占位符换成图片的 base64，直接编码进最终缓冲区（只分配一次）。占位符不是恰好一个时返回 None
fn splice_image(skeleton: &[u8], image: &[u8]) -> Option<Vec<u8>> {
    let slot = IMAGE_SLOT.as_bytes();
    let find = |hay: &[u8]| hay.windows(slot.len()).position(|w| w == slot);
    let at = find(skeleton)?;
    let tail = &skeleton[at + slot.len()..];
    if find(tail).is_some() {
        return None;
    }
    let encoded = base64::encoded_len(image.len(), true)?;
    let mut out = Vec::with_capacity(at + encoded + tail.len());
    out.extend_from_slice(&skeleton[..at]);
    out.resize(at + encoded, 0);
    let written = base64::engine::general_purpose::STANDARD
        .encode_slice(image, &mut out[at..])
        .ok()?;
    out.truncate(at + written);
    out.extend_from_slice(tail);
    Some(out)
}

/// 协议构造请求体需要的材料
pub(crate) struct Parts<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub user_text: &'a str,
    /// (mime, base64)
    pub image: Option<(&'a str, &'a str)>,
    pub plan: &'a Plan,
}

impl Parts<'_> {
    /// 合并 system 后的 user 文本（user = system + "\n\n" + user）
    pub fn merged_user_text(&self) -> String {
        match (self.system.is_empty(), self.user_text.is_empty()) {
            (true, _) => self.user_text.to_owned(),
            (false, true) => self.system.to_owned(),
            (false, false) => format!("{}\n\n{}", self.system, self.user_text),
        }
    }

    /// 实际放进 user 的文本与是否单独发送 system
    pub fn user_and_system(&self) -> (String, Option<&str>) {
        if self.plan.merge_system {
            (self.merged_user_text(), None)
        } else {
            (self.user_text.to_owned(), Some(self.system).filter(|s| !s.is_empty()))
        }
    }
}

pub(crate) fn data_url(mime: &str, b64: &str) -> String {
    let mut s = String::with_capacity(mime.len() + b64.len() + 13);
    s.push_str("data:");
    s.push_str(mime);
    s.push_str(";base64,");
    s.push_str(b64);
    s
}

pub(crate) fn insert(map: &mut Map<String, Value>, key: &str, value: impl Into<Value>) {
    map.insert(key.to_owned(), value.into());
}

/// 协议默认头 + 鉴权头 + 自定义头（自定义头在最后，可以覆盖或补充）
pub(crate) fn headers(
    p: &Provider,
    host: &str,
    key: Option<&Secret>,
    accept: &'static str,
) -> Result<HeaderMap, LlmError> {
    let mut h = HeaderMap::new();
    h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    h.insert(ACCEPT, HeaderValue::from_static(accept));
    if p.protocol == Protocol::AnthropicMessages {
        h.insert("anthropic-version", HeaderValue::from_static(ANTHROPIC_VERSION));
    }
    if let Some(key) = key.filter(|k| !k.expose().trim().is_empty()) {
        auth(&mut h, p.protocol, &p.quirks, host, key)?;
    }
    for (name, value) in &p.headers {
        let parsed = (
            HeaderName::from_bytes(name.trim().as_bytes()),
            HeaderValue::from_str(value.trim()),
        );
        match parsed {
            (Ok(n), Ok(mut v)) => {
                v.set_sensitive(true);
                h.insert(n, v);
            }
            // 只记名称，绝不记值
            _ => log::warn!(
                "skipping invalid custom header `{}` on provider {}",
                name.escape_debug(),
                p.id
            ),
        }
    }
    Ok(h)
}

/// 鉴权头（Key 为空时不发；Anthropic 中转站默认同时发 x-api-key 与 Bearer）
fn auth(h: &mut HeaderMap, protocol: Protocol, quirks: &Quirks, host: &str, key: &Secret) -> Result<(), LlmError> {
    let invalid = || {
        errors::make(
            wisp_types::ErrorCode::Auth,
            "the API key contains characters that cannot be sent in an HTTP header",
        )
    };
    match protocol {
        Protocol::OpenAiChat | Protocol::OpenAiResponses => {
            h.insert(AUTHORIZATION, key.header_value("Bearer ").ok_or_else(invalid)?);
        }
        Protocol::AnthropicMessages => {
            h.insert("x-api-key", key.header_value("").ok_or_else(invalid)?);
            let dual = quirks.dual_auth_header.unwrap_or(host != "api.anthropic.com");
            if dual {
                h.insert(AUTHORIZATION, key.header_value("Bearer ").ok_or_else(invalid)?);
            }
        }
        Protocol::Gemini => {
            if !quirks.gemini_key_in_query {
                h.insert("x-goog-api-key", key.header_value("").ok_or_else(invalid)?);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Purpose;

    fn image_req(template: &str, text: &str) -> ChatRequest {
        let p = crate::templates::provider_from_template(template, &[]).expect("template");
        ChatRequest {
            request_id: "r".to_owned(),
            provider: p,
            key: Some(Secret::new("sk-secret-key-123".to_owned())),
            model: wisp_types::ModelEntry::new("m"),
            system: "sys".to_owned(),
            user: UserContent::Image {
                bytes: bytes::Bytes::from_static(b"\x89PNG\r\n\x1a\n-not-really-a-png-\xff\xfe"),
                mime: "image/png",
                text: text.to_owned(),
                detail: ImageDetail::High,
            },
            temperature: None,
            max_output_tokens: 64,
            purpose: Purpose::Ocr,
        }
    }

    /// 直接把 base64 放进 JSON 的做法（对照组）
    fn direct(job: &Job, req: &ChatRequest) -> Value {
        let UserContent::Image { bytes, mime, text, .. } = &req.user else {
            unreachable!("image request")
        };
        let plan = params::plan(&PlanInput {
            protocol: req.provider.protocol,
            quirks: &req.provider.quirks,
            model: &req.model,
            caps: &ModelCaps::default(),
            purpose: req.purpose,
            temperature: req.temperature,
            max_output_tokens: req.max_output_tokens,
            host: &job.host,
            has_image: true,
            image_detail: ImageDetail::High,
        });
        let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        let parts = Parts {
            model: &req.model.id,
            system: &req.system,
            user_text: text,
            image: Some((mime, &b64)),
            plan: &plan,
        };
        let body = serialize(req.provider.protocol, &parts, &mut Vec::new()).expect("serialize");
        serde_json::from_slice(&body).expect("json")
    }

    #[test]
    fn spliced_image_body_equals_direct_embedding() {
        for template in ["openai", "openai-responses", "anthropic", "gemini"] {
            let req = image_req(template, "Transcribe.");
            let job = prepare(&req).expect("job");
            let built = build(&job, &req, &ModelCaps::default()).expect("build");
            let spliced: Value = serde_json::from_slice(&built.body).expect("json");
            assert_eq!(spliced, direct(&job, &req), "{template}");
            assert!(!String::from_utf8_lossy(&built.body).contains("wisp-image-base64"));
        }
    }

    #[test]
    fn placeholder_in_text_falls_back_to_direct_embedding() {
        let req = image_req("openai", IMAGE_SLOT);
        let job = prepare(&req).expect("job");
        let built = build(&job, &req, &ModelCaps::default()).expect("build");
        let v: Value = serde_json::from_slice(&built.body).expect("json");
        assert_eq!(v, direct(&job, &req));
        assert!(!built.sent.is_empty());
    }

    #[test]
    fn job_debug_never_shows_the_key_in_query() {
        let mut req = image_req("gemini", "x");
        req.provider.quirks.gemini_key_in_query = true;
        let job = prepare(&req).expect("job");
        assert!(job.url.contains("key=sk-secret-key-123"));
        let dbg = format!("{job:?}");
        assert!(!dbg.contains("sk-secret-key-123"), "{dbg}");
        assert!(dbg.contains(&job.safe_url));
    }
}
