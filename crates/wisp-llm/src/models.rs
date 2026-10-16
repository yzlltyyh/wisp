//! 各协议的列表端点、分页（最多 5 页）、解析、Gemini 过滤、合并。
//!
//! - OpenAI 及兼容端点：GET {p}/models（不分页）；OpenRouter 额外有 architecture.input_modalities / supported_parameters；
//! - Anthropic：GET {p}/models?limit=1000（不发 lifecycle，客户端过滤 retired），按 has_more / last_id 翻页；
//! - Gemini：GET {p}/models?pageSize=1000，按 nextPageToken 翻页；只保留支持 generateContent 的对话模型。

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde_json::Value;
use wisp_types::{LlmError, ModelCaps, ModelEntry, Protocol, Provider, Vision};

use crate::client::{Inner, RemoteModel};
use crate::errors::{self, HttpCtx};
use crate::request;
use crate::secret::Secret;
use crate::url::{self, Endpoint};
use crate::vision;

/// 最多翻几页
const MAX_PAGES: usize = 5;
/// 单页响应体上限（OpenRouter 的列表很大）
const LIST_BODY_LIMIT: usize = 16 * 1024 * 1024;
/// 首次拉取时，总数不超过这个值就全部启用
const ENABLE_ALL_THRESHOLD: usize = 20;
/// Gemini：名称含这些片段的不是对话模型
const GEMINI_EXCLUDED: [&str; 7] = ["tts", "live", "transcribe", "embedding", "imagen", "veo", "-image"];

/// 拉取结果，附带最后一次 HTTP 状态（测试连接要显示）
pub(crate) struct ListOutcome {
    pub result: Result<Vec<RemoteModel>, LlmError>,
    pub status: Option<u16>,
}

/// 拉取模型列表
pub(crate) async fn list(inner: &Inner, provider: &Provider, key: Option<&Secret>) -> ListOutcome {
    let mut status = None;
    let result = list_inner(inner, provider, key, &mut status)
        .await
        .map_err(|e| errors::finalize(errors::settle_param_unsupported(e, false), key));
    if let Err(e) = &result {
        log::info!(
            "llm models provider={} failed code={:?} status={:?} url={}",
            provider.id,
            e.code,
            status,
            e.url.as_deref().unwrap_or("-")
        );
    }
    ListOutcome { result, status }
}

async fn list_inner(
    inner: &Inner,
    provider: &Provider,
    key: Option<&Secret>,
    status: &mut Option<u16>,
) -> Result<Vec<RemoteModel>, LlmError> {
    if !provider.protocol.is_llm() {
        return Err(errors::not_llm());
    }
    let n = url::normalize(&provider.base_url, provider.protocol);
    if n.prefix.is_empty() {
        return Err(errors::not_configured("the provider has no base URL"));
    }
    let base_url = n.url(provider.protocol, Endpoint::Models, None);
    let host = url::host_of(&base_url);
    let http = inner.http(provider.timeouts.connect_ms, url::is_loopback_host(&host))?;
    let headers = request::headers(provider, &host, key, "application/json")?;
    let t = &provider.timeouts;
    let total = Duration::from_millis(u64::from(t.connect_ms) + u64::from(t.first_token_ms) + u64::from(t.idle_ms));
    // 网络类错误淘汰这个 Client（系统代理可能变了，见 client.rs 文件头）
    let net_error = |e: &reqwest::Error, url: &str| {
        if !e.is_builder() {
            inner.evict(&http);
        }
        transport_error(e, url, total)
    };

    let mut models = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let page_url = match (&token, provider.protocol) {
            (Some(tok), Protocol::AnthropicMessages) => format!("{base_url}&after_id={}", encode(tok)),
            (Some(tok), Protocol::Gemini) => format!("{base_url}&pageToken={}", encode(tok)),
            _ => base_url.clone(),
        };
        let real_url = request::with_query_key(provider, &page_url, key);
        let resp = http
            .client
            .get(&real_url)
            .headers(headers.clone())
            .timeout(total)
            .send()
            .await
            .map_err(|e| net_error(&e, &page_url))?;
        let code = resp.status().as_u16();
        *status = Some(code);
        let ok = resp.status().is_success();
        let resp_headers = resp.headers().clone();
        let limit = if ok { LIST_BODY_LIMIT } else { 64 * 1024 };
        let body = read_limited(resp, limit).await.map_err(|e| net_error(&e, &page_url))?;
        if !ok {
            let ctx = HttpCtx {
                protocol: provider.protocol,
                endpoint: Endpoint::Models,
                url: &page_url,
                version_hint: false,
                has_image: false,
                sent: &[],
            };
            return Err(errors::classify_http(&ctx, code, &resp_headers, &body).error);
        }
        let v: Value = serde_json::from_slice(&body)
            .map_err(|_| errors::protocol("the /models response is not JSON", &page_url))?;
        // 2xx 却是出错形态（MiniMax 的 base_resp、中转站的 {code, msg}）：按它归类，而不是笼统的「形状不对」
        if let Some(mut e) = errors::payload_error(&v, &page_url, false) {
            e.status = Some(code);
            return Err(e);
        }
        let (page, next) = match provider.protocol {
            Protocol::OpenAiChat | Protocol::OpenAiResponses => (parse_openai(&v), None),
            Protocol::AnthropicMessages => parse_anthropic(&v),
            Protocol::Gemini => parse_gemini(&v),
            // 不是 LLM 服务：上面已经拒绝，不会走到这里
            Protocol::PaddleOcr => (None, None),
        };
        let page = page.ok_or_else(|| errors::protocol("unexpected /models response shape", &page_url))?;
        models.extend(page);
        match next {
            Some(t) if !t.is_empty() => token = Some(t),
            _ => break,
        }
    }
    inner.touch(&http);
    Ok(models)
}

fn transport_error(e: &reqwest::Error, url: &str, total: Duration) -> LlmError {
    if e.is_timeout() && !e.is_connect() {
        return errors::timeout(format!("no complete response within {} s", total.as_secs()), url);
    }
    errors::transport(e, url).0
}

async fn read_limited(mut resp: reqwest::Response, limit: usize) -> Result<Vec<u8>, reqwest::Error> {
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        let room = limit.saturating_sub(buf.len());
        buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if buf.len() >= limit {
            break;
        }
    }
    Ok(buf)
}

fn encode(s: &str) -> String {
    ::url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn u32_of(v: &Value, key: &str) -> Option<u32> {
    v.get(key)
        .and_then(Value::as_u64)
        .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
}

fn remote(id: &str) -> RemoteModel {
    RemoteModel {
        id: id.to_owned(),
        label: None,
        vision: None,
        reasoning: None,
        deprecated: false,
        retired: false,
        context_tokens: None,
        anthropic_thinking_disabled: None,
        anthropic_efforts: None,
    }
}

/// OpenAI 及兼容端点：`{"data":[{"id":…}]}`（个别端点直接返回数组）
pub(crate) fn parse_openai(v: &Value) -> Option<Vec<RemoteModel>> {
    let items = v.get("data").and_then(Value::as_array).or_else(|| v.as_array())?;
    Some(
        items
            .iter()
            .filter_map(|item| {
                let id = str_of(item, "id")?;
                let mut m = remote(id);
                m.label = str_of(item, "name")
                    .or_else(|| str_of(item, "display_name"))
                    .map(str::to_owned);
                // OpenRouter：architecture.input_modalities 含 image 即支持视觉
                if let Some(mods) = item
                    .get("architecture")
                    .and_then(|a| a.get("input_modalities"))
                    .and_then(Value::as_array)
                {
                    let image = mods.iter().any(|m| m.as_str() == Some("image"));
                    m.vision = Some(if image { Vision::Yes } else { Vision::No });
                }
                if let Some(params) = item.get("supported_parameters").and_then(Value::as_array) {
                    m.reasoning = Some(
                        params
                            .iter()
                            .any(|p| matches!(p.as_str(), Some("reasoning" | "include_reasoning"))),
                    );
                }
                m.context_tokens = u32_of(item, "context_length").or_else(|| u32_of(item, "context_window"));
                Some(m)
            })
            .collect(),
    )
}

/// Anthropic：`{"data":[…],"has_more":…,"last_id":…}`
pub(crate) fn parse_anthropic(v: &Value) -> (Option<Vec<RemoteModel>>, Option<String>) {
    let Some(items) = v.get("data").and_then(Value::as_array) else {
        return (None, None);
    };
    let models = items
        .iter()
        .filter_map(|item| {
            let id = str_of(item, "id")?;
            let mut m = remote(id);
            m.label = str_of(item, "display_name").map(str::to_owned);
            match str_of(item, "lifecycle") {
                Some("retired") => m.retired = true,
                Some("deprecated") => m.deprecated = true,
                _ => {}
            }
            m.context_tokens = u32_of(item, "max_input_tokens");
            if let Some(caps) = item.get("capabilities") {
                m.vision = supported(caps.get("image_input")).map(|yes| if yes { Vision::Yes } else { Vision::No });
                let thinking = caps.get("thinking");
                m.reasoning = supported(thinking);
                m.anthropic_thinking_disabled =
                    supported(thinking.and_then(|t| t.get("types")).and_then(|t| t.get("disabled")));
                m.anthropic_efforts = caps.get("effort").and_then(efforts);
            }
            Some(m)
        })
        .collect();
    let next = if v.get("has_more").and_then(Value::as_bool) == Some(true) {
        str_of(v, "last_id").map(str::to_owned)
    } else {
        None
    };
    (Some(models), next)
}

/// `{"supported": bool}` 或直接是 bool
fn supported(v: Option<&Value>) -> Option<bool> {
    let v = v?;
    v.as_bool().or_else(|| v.get("supported").and_then(Value::as_bool))
}

/// Anthropic 的 effort 档位：`{"low":{"supported":true},…}` 或 `["low",…]`
fn efforts(v: &Value) -> Option<Vec<String>> {
    const ORDER: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
    if let Some(list) = v.as_array() {
        return Some(list.iter().filter_map(Value::as_str).map(str::to_owned).collect());
    }
    let obj = v.as_object()?;
    if obj.get("supported").and_then(Value::as_bool) == Some(false) {
        return Some(Vec::new());
    }
    let mut levels: Vec<String> = obj
        .iter()
        .filter(|(k, val)| k.as_str() != "supported" && supported(Some(val)) == Some(true))
        .map(|(k, _)| k.clone())
        .collect();
    levels.sort_by_key(|l| ORDER.iter().position(|o| o == l).unwrap_or(ORDER.len()));
    Some(levels)
}

/// Gemini：`{"models":[…],"nextPageToken":…}`；只保留对话模型，它们都支持图片
pub(crate) fn parse_gemini(v: &Value) -> (Option<Vec<RemoteModel>>, Option<String>) {
    let Some(items) = v.get("models").and_then(Value::as_array) else {
        // 空列表时字段可能整个缺失
        return (v.is_object().then(Vec::new), None);
    };
    let models = items
        .iter()
        .filter_map(|item| {
            let name = str_of(item, "name")?;
            let id = name.strip_prefix("models/").unwrap_or(name);
            let chat = item
                .get("supportedGenerationMethods")
                .and_then(Value::as_array)
                .is_some_and(|ms| {
                    ms.iter()
                        .any(|m| matches!(m.as_str(), Some("generateContent" | "streamGenerateContent")))
                });
            let lower = id.to_ascii_lowercase();
            if !chat || GEMINI_EXCLUDED.iter().any(|x| lower.contains(x)) {
                return None;
            }
            let mut m = remote(id);
            m.label = str_of(item, "displayName").map(str::to_owned);
            m.vision = Some(Vision::Yes);
            m.reasoning = item.get("thinking").and_then(Value::as_bool);
            m.context_tokens = u32_of(item, "inputTokenLimit");
            Some(m)
        })
        .collect();
    (Some(models), str_of(v, "nextPageToken").map(str::to_owned))
}

/// 由上游条目生成新的 ModelEntry（视觉按「能力字段 > 关键词」，推理按能力字段或 id 正则）
pub(crate) fn entry_from_remote(r: &RemoteModel, enabled: bool) -> ModelEntry {
    let (vision, vision_source) = vision::resolve(&r.id, None, r.vision);
    ModelEntry {
        id: r.id.clone(),
        label: r.label.clone(),
        vision,
        vision_source,
        reasoning: r.reasoning == Some(true) || vision::is_reasoning_id(&r.id),
        enabled,
        missing_upstream: false,
        deprecated: r.deprecated,
        context_tokens: r.context_tokens,
        caps: ModelCaps {
            anthropic_thinking_disabled: r.anthropic_thinking_disabled,
            anthropic_efforts: r.anthropic_efforts.clone(),
            ..ModelCaps::default()
        },
        reasoning_effort: None,
    }
}

/// 合并本地模型与上游列表：保留用户设置；上游消失的标记 missing_upstream 而不删除；
/// `referenced` 中的模型（被默认模型或风格引用）一定保留
pub fn merge_models(existing: &[ModelEntry], remote: Vec<RemoteModel>, referenced: &[&str]) -> Vec<ModelEntry> {
    let mut seen = HashSet::new();
    let remote: Vec<RemoteModel> = remote
        .into_iter()
        .filter(|r| !r.retired && !r.id.trim().is_empty() && seen.insert(r.id.clone()))
        .collect();
    let first_fetch = existing.is_empty();
    let enable_all = first_fetch && remote.len() <= ENABLE_ALL_THRESHOLD;
    let by_id: HashMap<&str, &RemoteModel> = remote.iter().map(|r| (r.id.as_str(), r)).collect();
    let existing_ids: HashSet<&str> = existing.iter().map(|e| e.id.as_str()).collect();

    let mut out = Vec::with_capacity(existing.len() + remote.len());
    for e in existing {
        let mut m = e.clone();
        match by_id.get(e.id.as_str()) {
            Some(r) => {
                m.missing_upstream = false;
                m.deprecated = r.deprecated;
                if m.label.is_none() {
                    m.label.clone_from(&r.label);
                }
                (m.vision, m.vision_source) = vision::resolve(&m.id, Some((m.vision, m.vision_source)), r.vision);
                m.context_tokens = r.context_tokens.or(m.context_tokens);
                if r.anthropic_thinking_disabled.is_some() {
                    m.caps.anthropic_thinking_disabled = r.anthropic_thinking_disabled;
                }
                if r.anthropic_efforts.is_some() {
                    m.caps.anthropic_efforts.clone_from(&r.anthropic_efforts);
                }
            }
            // 上游消失（或本来就是手动添加的）：只标记，不删除
            None => m.missing_upstream = true,
        }
        out.push(m);
    }
    for r in &remote {
        if existing_ids.contains(r.id.as_str()) {
            continue;
        }
        let enabled = enable_all || referenced.contains(&r.id.as_str());
        out.push(entry_from_remote(r, enabled));
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wisp_types::VisionSource;

    use super::*;

    #[test]
    fn openrouter_capabilities() {
        let v = json!({"data": [
            {"id": "openai/gpt-5", "name": "GPT-5", "architecture": {"input_modalities": ["text", "image"]},
             "supported_parameters": ["reasoning", "max_tokens"], "context_length": 400000},
            {"id": "deepseek/deepseek-chat", "architecture": {"input_modalities": ["text"]}, "supported_parameters": ["temperature"]},
            {"id": "plain"}
        ]});
        let m = parse_openai(&v).expect("list");
        assert_eq!(m.len(), 3);
        assert_eq!(
            (m[0].vision, m[0].reasoning, m[0].context_tokens),
            (Some(Vision::Yes), Some(true), Some(400_000))
        );
        assert_eq!((m[1].vision, m[1].reasoning), (Some(Vision::No), Some(false)));
        assert_eq!((m[2].vision, m[2].reasoning, m[2].label.as_deref()), (None, None, None));
    }

    #[test]
    fn anthropic_capabilities_and_lifecycle() {
        let v = json!({"data": [
            {"id": "claude-opus-5-5", "display_name": "Claude Opus 5.5", "max_input_tokens": 1000000,
             "capabilities": {"image_input": {"supported": true},
                              "thinking": {"supported": true, "types": {"adaptive": {"supported": true}, "disabled": {"supported": false}}},
                              "effort": {"supported": true, "max": {"supported": true}, "low": {"supported": true}, "medium": {"supported": false}}}},
            {"id": "claude-2", "lifecycle": "retired"},
            {"id": "claude-haiku-4-5", "lifecycle": "deprecated", "capabilities": {"image_input": {"supported": true},
             "thinking": {"supported": true, "types": {"disabled": {"supported": true}}}}}
        ], "has_more": true, "last_id": "claude-haiku-4-5"});
        let (m, next) = parse_anthropic(&v);
        let m = m.expect("list");
        assert_eq!(next.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(m[0].label.as_deref(), Some("Claude Opus 5.5"));
        assert_eq!(m[0].anthropic_thinking_disabled, Some(false));
        assert_eq!(m[0].anthropic_efforts, Some(vec!["low".to_owned(), "max".to_owned()]));
        assert!(m[1].retired);
        assert!(m[2].deprecated);
        assert_eq!(m[2].anthropic_thinking_disabled, Some(true));
    }

    #[test]
    fn gemini_filter() {
        let v = json!({"models": [
            {"name": "models/gemini-3-flash-preview", "displayName": "Gemini 3 Flash", "supportedGenerationMethods": ["generateContent", "countTokens"], "thinking": true, "inputTokenLimit": 1048576},
            {"name": "models/gemini-2.5-flash-preview-tts", "supportedGenerationMethods": ["generateContent"]},
            {"name": "models/text-embedding-004", "supportedGenerationMethods": ["embedContent"]},
            {"name": "models/gemini-embedding-001", "supportedGenerationMethods": ["embedContent", "generateContent"]},
            {"name": "models/imagen-4.0-generate-001", "supportedGenerationMethods": ["predict"]},
            {"name": "models/gemini-2.5-flash-image", "supportedGenerationMethods": ["generateContent"]},
            {"name": "models/gemini-live-2.5-flash", "supportedGenerationMethods": ["bidiGenerateContent", "generateContent"]}
        ], "nextPageToken": "abc"});
        let (m, next) = parse_gemini(&v);
        let m = m.expect("list");
        assert_eq!(next.as_deref(), Some("abc"));
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].id, "gemini-3-flash-preview");
        assert_eq!((m[0].vision, m[0].reasoning), (Some(Vision::Yes), Some(true)));
    }

    fn r(id: &str) -> RemoteModel {
        remote(id)
    }

    #[test]
    fn first_fetch_enables_all_when_small() {
        let merged = merge_models(&[], vec![r("gpt-4o"), r("gpt-5.4-mini"), r("gpt-4o")], &[]);
        assert_eq!(merged.len(), 2);
        assert!(merged.iter().all(|m| m.enabled));
        assert_eq!(merged[0].vision, Vision::Yes);
        assert_eq!(merged[0].vision_source, Some(VisionSource::Keyword));
        assert!(merged[1].reasoning);
    }

    #[test]
    fn first_fetch_large_enables_only_referenced() {
        let remote: Vec<RemoteModel> = (0..25).map(|i| r(&format!("m{i}"))).collect();
        let merged = merge_models(&[], remote, &["m7"]);
        assert_eq!(
            merged
                .iter()
                .filter(|m| m.enabled)
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            ["m7"]
        );
    }

    #[test]
    fn refresh_keeps_user_settings_and_marks_missing() {
        let mut user = ModelEntry::new("gpt-4o");
        user.vision = Vision::No;
        user.vision_source = Some(VisionSource::User);
        user.enabled = false;
        user.label = Some("我的".to_owned());
        user.reasoning = true;
        let manual = ModelEntry::new("custom-alias");
        let mut cap = r("gpt-4o");
        cap.vision = Some(Vision::Yes);
        cap.label = Some("GPT-4o".to_owned());
        let merged = merge_models(&[user.clone(), manual], vec![cap, r("new-model")], &[]);
        assert_eq!(merged.len(), 3);
        let m0 = &merged[0];
        assert_eq!(
            (
                m0.vision,
                m0.vision_source,
                m0.enabled,
                m0.label.as_deref(),
                m0.reasoning
            ),
            (Vision::No, Some(VisionSource::User), false, Some("我的"), true)
        );
        assert!(merged[1].missing_upstream, "上游没有的只标记不删除");
        assert!(!merged[2].enabled, "之后刷新时新模型默认不启用");
    }

    #[test]
    fn retired_models_are_dropped() {
        let mut gone = r("claude-2");
        gone.retired = true;
        let merged = merge_models(&[], vec![gone, r("claude-haiku-4-5")], &[]);
        assert_eq!(
            merged.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["claude-haiku-4-5"]
        );
    }
}
