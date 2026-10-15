//! 错误归一化。
//!
//! - 先看状态码再解析 body；不依赖 Content-Type（OpenAI 401 是 text/plain，Gemini 400 带 SSE 头）；
//! - code 兼容整数和字符串；依次尝试 error.message / error.code / error.type / code / message / error.status /
//!   details[].reason / type 等路径；HTML 不展示原文；
//! - 2xx 的响应体（或流中的 data）也可能是出错形态：MiniMax 的 `base_resp.status_code ≠ 0`、中转站的顶层 `{code, msg}`，
//!   按厂商错误码 / 状态码 / 关键词归类（auth / quota / rate_limited / content_filter / upstream …），见 [`payload_error`]；
//! - 原文在解析阶段只做宽松的长度上限（[`RAW_MESSAGE_CHARS`]），错误离开 wisp-llm 之前由 [`finalize`] **先脱敏再截断**
//!   到 300 字符，截断点切开 Key 时留下的 Key 前缀同样替换；
//! - 带图请求的 4xx 分三档：明确「不支持图片输入」→ 确定的 no_vision；单张图片处理失败（损坏、解码失败、
//!   类型不符、暂时处理不了）→ upstream；只提到图片 → 不确定的 no_vision（运行时反馈只写给 Unknown 的模型）；
//! - 所在地区不可用（OpenAI 403、Gemini 400 FAILED_PRECONDITION「User location is not supported」）→ auth + region，
//!   与计费 / 免费层有关的 FAILED_PRECONDITION 仍按 quota；
//! - 错误信息只含服务端原文或本地英文简述，**绝不**包含 Key、请求正文或带 query 的 URL；
//! - `hint` 是前端 errors.json 命名空间的 i18n key（文案在前端 locales 里），这里写成完整字面量，便于 check-i18n 扫描。

use std::time::SystemTime;

use reqwest::header::HeaderMap;
use serde_json::Value;
use wisp_types::{ErrorCode, ErrorSubcode, LlmError, Protocol};

use crate::degrade::{self, Param, SentParam};
use crate::url::{Endpoint, display_url};

pub(crate) const HINT_CHECK_KEY: &str = "errors.hint.check_key";
pub(crate) const HINT_REGION: &str = "errors.hint.region";
pub(crate) const HINT_CHECK_MODEL: &str = "errors.hint.check_model";
pub(crate) const HINT_TRY_V1: &str = "errors.hint.try_v1";
pub(crate) const HINT_PICK_VISION_MODEL: &str = "errors.hint.pick_vision_model";
pub(crate) const HINT_SHRINK_IMAGE: &str = "errors.hint.shrink_image";
pub(crate) const HINT_SHORTEN_TEXT: &str = "errors.hint.shorten_text";
pub(crate) const HINT_CHECK_BILLING: &str = "errors.hint.check_billing";
pub(crate) const HINT_RETRY_LATER: &str = "errors.hint.retry_later";
pub(crate) const HINT_RETRY: &str = "errors.hint.retry";
pub(crate) const HINT_SWITCH_MODEL: &str = "errors.hint.switch_model";
pub(crate) const HINT_CHECK_NETWORK: &str = "errors.hint.check_network";
pub(crate) const HINT_ADJUST_TIMEOUT: &str = "errors.hint.adjust_timeout";
pub(crate) const HINT_CHECK_PROTOCOL: &str = "errors.hint.check_protocol";
pub(crate) const HINT_CHECK_SETTINGS: &str = "errors.hint.check_settings";
/// 降级后又被拒了另一个参数：两次结论都已写进能力缓存，点「重试」即可成功
pub(crate) const HINT_PARAM_ADJUSTED: &str = "errors.hint.param_adjusted";
/// 模型拒绝了请求参数，且无法自动调整（识别不出是哪个参数，或同一参数降级后仍被拒）
pub(crate) const HINT_PARAM_REJECTED: &str = "errors.hint.param_rejected";

/// `LlmError.message` 的长度上限（字符），由 [`finalize`] 在脱敏之后施加
pub(crate) const MAX_MESSAGE_CHARS: usize = 300;
/// 解析阶段原文的宽松上限（字符）：远大于任何 Key，又不会把 MB 级的响应体整个放进错误里
pub(crate) const RAW_MESSAGE_CHARS: usize = 4096;
/// 截断标记
const ELLIPSIS: char = '…';
/// 只脱敏不短于这个长度的 Key（太短的「Key」与普通文字撞上的概率太高）
const MIN_REDACT_KEY_BYTES: usize = 8;
/// 截断点留下的 Key 前缀达到这个长度就替换（更短的前缀是 `sk-` 之类的公共前缀，不泄露任何信息）
const MIN_REDACT_PREFIX_BYTES: usize = 4;

/// 构造一个错误：按 code 填默认的 retryable 与 hint（message 只做宽松上限，最终截断见 [`finalize`]）
pub(crate) fn make(code: ErrorCode, message: impl AsRef<str>) -> LlmError {
    let mut e = LlmError::new(code, cap(message.as_ref()));
    e.retryable = matches!(
        code,
        ErrorCode::Network | ErrorCode::Timeout | ErrorCode::Upstream | ErrorCode::RateLimited
    );
    e.hint = default_hint(code).map(str::to_owned);
    e
}

fn default_hint(code: ErrorCode) -> Option<&'static str> {
    Some(match code {
        ErrorCode::Auth => HINT_CHECK_KEY,
        ErrorCode::NotFound => HINT_CHECK_MODEL,
        ErrorCode::NoVision => HINT_PICK_VISION_MODEL,
        ErrorCode::ImageTooLarge => HINT_SHRINK_IMAGE,
        ErrorCode::TooLong => HINT_SHORTEN_TEXT,
        ErrorCode::Quota => HINT_CHECK_BILLING,
        ErrorCode::RateLimited => HINT_RETRY_LATER,
        ErrorCode::Upstream | ErrorCode::ParamUnsupported => HINT_RETRY,
        ErrorCode::ContentFilter => HINT_SWITCH_MODEL,
        ErrorCode::Network => HINT_CHECK_NETWORK,
        ErrorCode::Timeout => HINT_ADJUST_TIMEOUT,
        ErrorCode::Protocol => HINT_CHECK_PROTOCOL,
        ErrorCode::NotConfigured => HINT_CHECK_SETTINGS,
        ErrorCode::Cancelled | ErrorCode::Keychain | ErrorCode::Internal => return None,
    })
}

/// 用户取消（不出卡片）
pub(crate) fn cancelled() -> LlmError {
    make(ErrorCode::Cancelled, "cancelled")
}

/// 首 token / 空闲 / 等待响应头超时
pub(crate) fn timeout(message: impl AsRef<str>, url: &str) -> LlmError {
    with_url(make(ErrorCode::Timeout, message), url)
}

/// 没有结束标记就断流：保留已有文本，显示「连接中断」+「重试」，不自动重试
pub(crate) fn connection_lost(url: &str) -> LlmError {
    let mut e = with_url(
        make(
            ErrorCode::Network,
            "the connection closed before the response was complete",
        ),
        url,
    );
    e.subcode = Some(ErrorSubcode::ConnectionLost);
    e.hint = Some(HINT_RETRY.to_owned());
    e
}

/// 端点返回的不是预期的协议
pub(crate) fn protocol(message: impl AsRef<str>, url: &str) -> LlmError {
    with_url(make(ErrorCode::Protocol, message), url)
}

/// 本地配置缺失（base_url / 模型为空）
pub(crate) fn not_configured(message: impl AsRef<str>) -> LlmError {
    make(ErrorCode::NotConfigured, message)
}

/// 内部错误（不该发生）
pub(crate) fn internal(message: impl AsRef<str>) -> LlmError {
    make(ErrorCode::Internal, message)
}

pub(crate) fn with_url(mut e: LlmError, url: &str) -> LlmError {
    if !url.is_empty() {
        e.url = Some(display_url(url));
    }
    e
}

/// 错误离开 wisp-llm 之前的最后一步：先在完整原文上把回显的 Key 换成 `***`（有的中转站会原样
/// 回显 Key），再截断到 [`MAX_MESSAGE_CHARS`]。截断点（包括解析阶段的宽松上限）正好切开 Key 时，留在末尾的
/// Key 前缀（≥ 4 字节）同样换成 `***`。所有出口（stream 的终止事件、测试连接、模型列表）都经过这里
pub(crate) fn finalize(mut e: LlmError, key: Option<&crate::secret::Secret>) -> LlmError {
    let key = key
        .map(|k| k.expose().trim())
        .filter(|k| k.len() >= MIN_REDACT_KEY_BYTES);
    let mut message = std::mem::take(&mut e.message);
    if let Some(k) = key {
        message = redact(&message, k);
    }
    message = truncate(&message);
    if let Some(k) = key {
        message = redact(&message, k);
    }
    e.message = message;
    e
}

/// 替换完整出现的 Key；文本以截断标记结尾时，再检查标记之前是不是 Key 的前缀
fn redact(text: &str, key: &str) -> String {
    let replaced = text.replace(key, "***");
    let Some(body) = replaced.strip_suffix(ELLIPSIS) else {
        return replaced;
    };
    match key_prefix_at_end(body, key) {
        Some(n) => format!("{}***{ELLIPSIS}", &body[..body.len() - n]),
        None => replaced,
    }
}

/// `text` 以 `key` 的前缀（≥ [`MIN_REDACT_PREFIX_BYTES`] 字节）结尾时，返回最长那个前缀的字节长度
fn key_prefix_at_end(text: &str, key: &str) -> Option<usize> {
    (MIN_REDACT_PREFIX_BYTES..=key.len().min(text.len()))
        .rev()
        .find(|&n| key.is_char_boundary(n) && text.ends_with(&key[..n]))
}

/// 截断到 `limit` 个字符（超出时以 `…` 结尾）
fn truncate_to(s: &str, limit: usize) -> String {
    let s = s.trim();
    match s.char_indices().nth(limit) {
        Some((i, _)) => format!("{}{ELLIPSIS}", &s[..i]),
        None => s.to_owned(),
    }
}

/// 最终长度（[`MAX_MESSAGE_CHARS`]）
fn truncate(s: &str) -> String {
    truncate_to(s, MAX_MESSAGE_CHARS)
}

/// 解析阶段的宽松上限（[`RAW_MESSAGE_CHARS`]）
fn cap(s: &str) -> String {
    truncate_to(s, RAW_MESSAGE_CHARS)
}

// ---------------------------------------------------------------------------
// 错误体解析

/// 从各种形状的错误体里抽出来的字段
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ErrorBody {
    pub message: Option<String>,
    /// error.code / code（整数转成十进制字符串）
    pub code: Option<String>,
    /// error.type / type（不含顶层的 "error"）
    pub kind: Option<String>,
    /// Gemini 的 error.status
    pub status: Option<String>,
    /// Gemini 的 error.details[].reason
    pub reasons: Vec<String>,
    pub request_id: Option<String>,
    /// MiniMax 的 `base_resp.status_code`（只记非 0 的值）
    pub vendor_status: Option<i64>,
    /// body 是 JSON
    pub json: bool,
    /// body 是 HTML 页面
    pub html: bool,
    /// 非 JSON 时的纯文本（宽松上限，最终截断见 [`finalize`]）
    pub text: Option<String>,
}

impl ErrorBody {
    /// 给用户看的信息：服务端原文优先
    fn display(&self, status: Option<u16>) -> String {
        if self.html {
            return match status {
                Some(s) => format!("the server returned an HTML page (HTTP {s})"),
                None => "the server returned an HTML page".to_owned(),
            };
        }
        self.message
            .clone()
            .or_else(|| self.kind.clone())
            .or_else(|| self.code.clone())
            .or_else(|| self.status.clone())
            .or_else(|| self.text.clone())
            .unwrap_or_else(|| match status {
                Some(s) => format!("HTTP {s}"),
                None => "unknown error".to_owned(),
            })
    }

    /// 分类用的小写拼接文本
    fn haystack(&self) -> String {
        let mut h = String::new();
        for part in [&self.message, &self.code, &self.kind, &self.status, &self.text]
            .into_iter()
            .flatten()
        {
            h.push_str(part);
            h.push(' ');
        }
        for r in &self.reasons {
            h.push_str(r);
            h.push(' ');
        }
        h.to_lowercase()
    }
}

/// 解析错误体（JSON 优先，否则按纯文本 / HTML）
pub(crate) fn parse_body(body: &[u8]) -> ErrorBody {
    let trimmed = body.trim_ascii();
    if trimmed.is_empty() {
        return ErrorBody::default();
    }
    if let Some(v) = parse_json_lenient(trimmed) {
        let mut b = ErrorBody {
            json: true,
            ..ErrorBody::default()
        };
        extract(&v, &mut b);
        return b;
    }
    let text = String::from_utf8_lossy(trimmed);
    let head: String = text.chars().take(512).collect::<String>().to_ascii_lowercase();
    if head.starts_with("<!doctype") || head.starts_with("<html") || head.contains("<html") || head.contains("<body") {
        return ErrorBody {
            html: true,
            ..ErrorBody::default()
        };
    }
    ErrorBody {
        text: Some(cap(&text)),
        ..ErrorBody::default()
    }
}

/// JSON；也接受被包成 SSE 的 `data: {…}`，以及 Gemini 非 SSE 流式端点返回的 `[{…}]`
fn parse_json_lenient(bytes: &[u8]) -> Option<Value> {
    let v = serde_json::from_slice::<Value>(bytes).ok().or_else(|| {
        let text = std::str::from_utf8(bytes).ok()?;
        text.lines()
            .filter_map(|l| l.trim().strip_prefix("data:"))
            .find_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
    })?;
    match v {
        Value::Array(mut items) if !items.is_empty() => Some(items.swap_remove(0)),
        Value::Object(_) => Some(v),
        _ => None,
    }
}

fn scalar(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn extract(v: &Value, b: &mut ErrorBody) {
    let err = v.get("error");
    match err {
        Some(Value::Object(_)) => {
            let e = err.unwrap_or(&Value::Null);
            b.message = scalar(e.get("message"));
            b.code = scalar(e.get("code")).or_else(|| scalar(e.get("http_code")));
            b.kind = scalar(e.get("type"));
            b.status = scalar(e.get("status"));
            if let Some(details) = e.get("details").and_then(Value::as_array) {
                b.reasons = details
                    .iter()
                    .filter_map(|d| d.get("reason").and_then(Value::as_str))
                    .map(str::to_owned)
                    .collect();
            }
            b.request_id = scalar(e.get("request_id"));
        }
        Some(Value::String(s)) if !s.trim().is_empty() => b.message = Some(s.trim().to_owned()),
        _ => {}
    }
    if b.message.is_none() {
        b.message = MESSAGE_KEYS
            .iter()
            .find_map(|k| scalar(v.get(*k)))
            .or_else(|| scalar(v.get("base_resp").and_then(|r| r.get("status_msg"))));
    }
    b.vendor_status = vendor_status(v);
    if b.code.is_none() {
        b.code = CODE_KEYS
            .iter()
            .find_map(|k| scalar(v.get(*k)))
            .or_else(|| b.vendor_status.map(|c| c.to_string()));
    }
    if b.kind.is_none() {
        b.kind = scalar(v.get("type")).filter(|t| t != "error");
    }
    if b.request_id.is_none() {
        b.request_id = scalar(v.get("request_id"));
    }
    if let Some(m) = b.message.take() {
        b.message = Some(cap(&m));
    }
}

/// 顶层的信息字段（各家中转 / 兼容端点的写法）
const MESSAGE_KEYS: [&str; 6] = ["message", "msg", "error_msg", "error_message", "errmsg", "detail"];
/// 顶层的错误码字段
const CODE_KEYS: [&str; 3] = ["code", "error_code", "errcode"];

/// MiniMax 的 `base_resp.status_code`（整数或数字字符串）；0 表示成功，返回 None
fn vendor_status(v: &Value) -> Option<i64> {
    int_of(v.get("base_resp")?.get("status_code")?).filter(|c| *c != 0)
}

fn int_of(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// HTTP 非 2xx

/// 归一化 HTTP 错误需要的请求上下文
pub(crate) struct HttpCtx<'a> {
    pub protocol: Protocol,
    pub endpoint: Endpoint,
    /// 完整请求 URL（只用来生成 host + path）
    pub url: &'a str,
    /// 对话端点 404 时提示加 / 去 /v1（base_url 不含版本段且不以 # 结尾）
    pub version_hint: bool,
    pub has_image: bool,
    /// 本次请求发送的可降级参数
    pub sent: &'a [SentParam],
}

/// 归一化结果
#[derive(Debug, Clone)]
pub(crate) struct Classified {
    pub error: LlmError,
    /// code = param_unsupported 时被拒的参数（可能为空：识别不出是哪个）
    pub degrade: Vec<Param>,
    /// code = no_vision 时：服务端是否明确说了「不支持图片输入」（只有这时才覆盖已知支持视觉的模型）
    pub vision_conclusive: bool,
    /// 流开始前可以自动重试（500/502/503/504/529 与 overloaded_error；尊重 x-should-retry: false）
    pub auto_retry: bool,
}

const RETRYABLE_STATUS: [u16; 5] = [500, 502, 503, 504, 529];

fn any_word(h: &str, words: &[&str]) -> bool {
    words.iter().any(|w| h.contains(w))
}

/// 归一化非 2xx 响应
pub(crate) fn classify_http(ctx: &HttpCtx<'_>, status: u16, headers: &HeaderMap, body: &[u8]) -> Classified {
    let b = parse_body(body);
    let hay = b.haystack();
    let retry_after = retry_after_secs(headers);
    let mut degrade_params = Vec::new();
    let mut subcode = None;

    let mut vision_conclusive = false;
    let mut transient = false;
    let code = match status {
        300..=399 => ErrorCode::Protocol,
        401 => ErrorCode::Auth,
        402 => ErrorCode::Quota,
        403 => {
            if any_word(&hay, &REGION_WORDS) {
                subcode = Some(ErrorSubcode::Region);
            }
            ErrorCode::Auth
        }
        // 带图请求的 404 / 5xx 也可能是「不支持图片输入」：OpenRouter 对不支持视觉的模型返回 404「No endpoints found that
        // support image input」，llama.cpp server 返回 500「image input is not supported」。确定的 no_vision，不自动重试
        404 | 500..=599 if ctx.has_image && is_conclusive_no_vision(&hay) => {
            vision_conclusive = true;
            ErrorCode::NoVision
        }
        404 => ErrorCode::NotFound,
        408 => ErrorCode::Timeout,
        409 => ErrorCode::Upstream,
        413 => {
            if ctx.has_image {
                ErrorCode::ImageTooLarge
            } else {
                ErrorCode::TooLong
            }
        }
        429 => classify_429(ctx.protocol, &hay, retry_after.is_some()),
        499 => ErrorCode::Cancelled,
        500..=599 => ErrorCode::Upstream,
        _ => {
            let v = classify_4xx(ctx, &b, &hay);
            degrade_params = v.degrade;
            subcode = v.subcode;
            vision_conclusive = v.vision_conclusive;
            transient = v.transient;
            v.code
        }
    };

    // Anthropic 的 overloaded_error 不论状态码都按 upstream（可重试）
    let overloaded = hay.contains("overloaded");
    let code = if overloaded && matches!(code, ErrorCode::Upstream | ErrorCode::Protocol) && status >= 400 {
        ErrorCode::Upstream
    } else {
        code
    };

    let message = if (300..400).contains(&status) {
        let target = headers
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(display_url)
            .unwrap_or_default();
        format!("the server redirected the request (HTTP {status}) {target}")
    } else {
        b.display(Some(status))
    };

    let mut e = make(code, message);
    e.status = Some(status);
    e.url = Some(display_url(ctx.url));
    e.retry_after_s = retry_after;
    e.upstream_request_id = upstream_request_id(headers).or_else(|| b.request_id.clone());
    if b.html {
        e.subcode = Some(ErrorSubcode::HtmlBody);
    }
    if let Some(s) = subcode {
        e.subcode = Some(s);
        if s == ErrorSubcode::Region {
            e.hint = Some(HINT_REGION.to_owned());
        }
    }
    if code == ErrorCode::NotFound && ctx.endpoint == Endpoint::Chat && ctx.version_hint {
        e.subcode = Some(ErrorSubcode::VersionHint);
        e.hint = Some(HINT_TRY_V1.to_owned());
    }
    // 4xx 归为 upstream 的（未知的请求错误）重试也不会成功；409 / 408 是暂时性的，服务端明确建议重试的也是
    if code == ErrorCode::Upstream && (400..500).contains(&status) && status != 409 && status != 408 && !transient {
        e.retryable = false;
    }

    let should_retry_header = headers
        .get("x-should-retry")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_ascii_lowercase());
    let auto_retry = (RETRYABLE_STATUS.contains(&status) || (overloaded && status >= 500))
        && code != ErrorCode::NoVision
        && should_retry_header.as_deref() != Some("false")
        && retry_after.is_none_or(|s| s <= 5);

    Classified {
        error: e,
        degrade: degrade_params,
        vision_conclusive: vision_conclusive && code == ErrorCode::NoVision,
        auto_retry,
    }
}

fn classify_429(protocol: Protocol, hay: &str, has_retry_after: bool) -> ErrorCode {
    match protocol {
        // Anthropic：月度 spend cap 触发的 429 不带 retry-after，重试无效
        Protocol::AnthropicMessages => {
            if has_retry_after {
                ErrorCode::RateLimited
            } else {
                ErrorCode::Quota
            }
        }
        Protocol::Gemini => {
            if any_word(hay, &["quota", "daily"]) {
                ErrorCode::Quota
            } else {
                ErrorCode::RateLimited
            }
        }
        Protocol::OpenAiChat | Protocol::OpenAiResponses => {
            if any_word(
                hay,
                &[
                    "insufficient_quota",
                    "credit_balance",
                    "spend_limit",
                    "usage_limit",
                    "billing",
                    "exceeded your current quota",
                    "insufficient balance",
                    "arrearage",
                    "余额",
                    "欠费",
                ],
            ) {
                ErrorCode::Quota
            } else {
                ErrorCode::RateLimited
            }
        }
    }
}

const TOO_LONG_WORDS: [&str; 11] = [
    "context_length_exceeded",
    "prompt is too long",
    "maximum number of tokens",
    "context length",
    "maximum context",
    "context window",
    "too many tokens",
    "string_above_max_length",
    "input is too long",
    "reduce the length",
    "token count",
];
const AUTH_WORDS: [&str; 12] = [
    "api_key_invalid",
    "api key not valid",
    "invalid api key",
    "incorrect api key",
    "invalid_api_key",
    "invalid x-api-key",
    "unauthorized",
    "authentication",
    "token is invalid",
    "invalid token",
    "令牌",
    "鉴权",
];
const IMAGE_WORDS: [&str; 9] = [
    "image",
    "vision",
    "multimodal",
    "media_type",
    "mime",
    "inline_data",
    "inlinedata",
    "picture",
    "图片",
];
/// 服务端明确说「这个模型不支持图片输入」：no_vision 的确定结论，运行时反馈据此把模型标为不支持视觉
const NO_VISION_WORDS: [&str; 26] = [
    "not support image",
    "doesn't support image",
    "not support vision",
    "doesn't support vision",
    "vision is not supported",
    "vision not supported",
    "image input is not supported",
    "image input not supported",
    "image inputs are not supported",
    "images are not supported",
    "image_url is not supported",
    "input_image is not supported",
    "image content is not supported",
    "only supported by certain models",
    "not a multimodal model",
    "is not multimodal",
    "not support multimodal",
    "multimodal input is not supported",
    "multimodal is not supported",
    "only supports text",
    "only text input",
    "missing data required for image input",
    "不支持图片",
    "不支持图像",
    "不支持视觉",
    "不支持多模态",
];
/// 这一张图片本身处理失败（损坏、解码失败、格式与声明不符、服务端暂时处理不了）：与模型是否支持视觉无关，
/// 归为 upstream，不写运行时视觉反馈
const IMAGE_FAILURE_WORDS: [&str; 13] = [
    "unable to process",
    "could not process",
    "couldn't process",
    "cannot process",
    "failed to process",
    "invalid image",
    "image is invalid",
    "not a valid image",
    "corrupt",
    "decode",
    "malformed",
    "does not match the provided media type",
    "unsupported image format",
];
/// 服务端明确建议重试
const RETRY_WORDS: [&str; 3] = ["retry", "try again", "temporarily"];
/// 所在地区不可用（OpenAI 403、Gemini 400 FAILED_PRECONDITION「User location is not supported」）
const REGION_WORDS: [&str; 7] = ["country", "region", "territory", "location", "地区", "国家", "所在地"];
/// 与计费 / 免费层有关（Gemini「free tier is not available in your country. Please enable billing」应引导开通计费）
const BILLING_WORDS: [&str; 7] = ["billing", "free tier", "paid", "credit", "计费", "付费", "余额"];
/// 「不可用」类字样（与地区词同时出现才算地区限制）
const UNAVAILABLE_WORDS: [&str; 7] = [
    "not supported",
    "unsupported",
    "not available",
    "unavailable",
    "not allowed",
    "不支持",
    "不可用",
];
const TOO_LARGE_WORDS: [&str; 9] = [
    "too large",
    "too big",
    "exceeds",
    "exceeded",
    "maximum",
    "size limit",
    "larger than",
    "dimensions",
    "pixels",
];
const MODEL_NOT_FOUND_WORDS: [&str; 7] = [
    "model_not_found",
    "model not found",
    "does not exist",
    "no such model",
    "unknown model",
    "invalid model",
    "模型不存在",
];

/// 4xx（400 / 422 / 405 等）的细分结果
struct Verdict4xx {
    code: ErrorCode,
    degrade: Vec<Param>,
    subcode: Option<ErrorSubcode>,
    /// code = no_vision 时结论是否确定
    vision_conclusive: bool,
    /// 归为 upstream 且服务端明确建议重试（例如 Gemini 的「Unable to process input image. Please retry」）
    transient: bool,
}

impl Verdict4xx {
    fn code(code: ErrorCode) -> Self {
        Self {
            code,
            degrade: Vec::new(),
            subcode: None,
            vision_conclusive: false,
            transient: false,
        }
    }
}

/// 400 / 422 / 405 等：按信息内容细分
fn classify_4xx(ctx: &HttpCtx<'_>, b: &ErrorBody, hay: &str) -> Verdict4xx {
    let (code, degrade) = classify_4xx_code(ctx, b, hay);
    let mut v = Verdict4xx {
        degrade,
        ..Verdict4xx::code(code)
    };
    match code {
        ErrorCode::Auth if is_region_block(b, hay) => v.subcode = Some(ErrorSubcode::Region),
        ErrorCode::NoVision => v.vision_conclusive = is_conclusive_no_vision(hay),
        ErrorCode::Upstream => v.transient = any_word(hay, &RETRY_WORDS),
        _ => {}
    }
    v
}

/// 地区限制：Gemini 的 FAILED_PRECONDITION 只要提到地区就算（与计费有关的除外），其余要同时出现「不可用」类字样
fn is_region_block(b: &ErrorBody, hay: &str) -> bool {
    if any_word(hay, &BILLING_WORDS) || !any_word(hay, &REGION_WORDS) {
        return false;
    }
    b.status.as_deref() == Some("FAILED_PRECONDITION") || any_word(hay, &UNAVAILABLE_WORDS)
}

/// 只是图片的**传输格式**不被支持（「does not support base64 image input」「does not support image urls」）：
/// 换一种传法也许就行，与模型能否看图无关——不当作确定的 no_vision
const IMAGE_TRANSPORT_WORDS: [&str; 4] = ["base64 image", "image url", "image link", "remote image"];

/// 服务端明确说不支持图片输入（含 serde 的 `unknown variant `image_url``：端点的消息结构里根本没有图片类型；
/// OpenRouter 的「No endpoints found that support image input」）。`hay` 须为小写
pub(crate) fn is_conclusive_no_vision(hay: &str) -> bool {
    if any_word(hay, &IMAGE_TRANSPORT_WORDS) {
        return false;
    }
    any_word(hay, &NO_VISION_WORDS)
        || hay.contains("support image input")
        || (hay.contains("unknown variant") && hay.contains("image"))
}

fn classify_4xx_code(ctx: &HttpCtx<'_>, b: &ErrorBody, hay: &str) -> (ErrorCode, Vec<Param>) {
    if b.reasons.iter().any(|r| r == "API_KEY_INVALID") || any_word(hay, &AUTH_WORDS) {
        return (ErrorCode::Auth, Vec::new());
    }
    // 地区限制先于额度判断：Gemini 对中国大陆 / 香港等地区返回 400 FAILED_PRECONDITION「User location is not supported」，
    // 应提示地区与代理，而不是「额度用尽或未开通计费」
    if is_region_block(b, hay) {
        return (ErrorCode::Auth, Vec::new());
    }
    if b.status.as_deref() == Some("FAILED_PRECONDITION")
        || any_word(
            hay,
            &["spend limit", "spending limit", "credit balance", "insufficient_quota"],
        )
    {
        return (ErrorCode::Quota, Vec::new());
    }
    if ctx.has_image && (any_word(hay, &IMAGE_WORDS) || is_conclusive_no_vision(hay)) {
        // 「maximum context length is 8192 tokens … (including image tokens)」是上下文超长，不是图片太大：
        // 先认上下文长度，再看「太大 / maximum」
        let code = if any_word(hay, &TOO_LONG_WORDS) {
            ErrorCode::TooLong
        } else if any_word(hay, &TOO_LARGE_WORDS) {
            ErrorCode::ImageTooLarge
        } else if is_conclusive_no_vision(hay) {
            ErrorCode::NoVision
        } else if any_word(hay, &IMAGE_FAILURE_WORDS) {
            // 这一张图处理失败，不代表模型不支持视觉
            ErrorCode::Upstream
        } else {
            // 只提到了图片：仍按 no_vision 出卡片，但结论不确定（不覆盖已知支持视觉的模型）
            ErrorCode::NoVision
        };
        return (code, Vec::new());
    }
    if any_word(hay, &TOO_LONG_WORDS) {
        return (ErrorCode::TooLong, Vec::new());
    }
    let params = degrade::detect(ctx.sent, hay);
    if !params.is_empty() {
        return (ErrorCode::ParamUnsupported, params);
    }
    if any_word(
        hay,
        &[
            "unsupported parameter",
            "unsupported value",
            "unknown parameter",
            "unrecognized request argument",
        ],
    ) {
        return (ErrorCode::ParamUnsupported, Vec::new());
    }
    if any_word(hay, &MODEL_NOT_FOUND_WORDS) {
        return (ErrorCode::NotFound, Vec::new());
    }
    if !b.json {
        // HTML、纯文本：多半是 base_url 指错了地方
        return (ErrorCode::Protocol, Vec::new());
    }
    (ErrorCode::Upstream, Vec::new())
}

// ---------------------------------------------------------------------------
// 流中的错误事件与 2xx 响应体里的出错形态

/// 流中出现的错误对象（OpenRouter 的 200 + error chunk、Responses 的 error / response.failed、Anthropic 的 error 事件、
/// Gemini 代理的 error 对象）。`err` 是 error 对象本身，或带 code / message 的整个事件
pub(crate) fn stream_error(err: &Value, url: &str, has_image: bool) -> LlmError {
    let mut b = ErrorBody {
        json: true,
        ..ErrorBody::default()
    };
    match err {
        Value::Object(_) if err.get("error").is_some() => extract(err, &mut b),
        Value::Object(_) => {
            // 已经是 error 对象：包一层复用同一套路径
            let wrapped = serde_json::json!({ "error": err });
            extract(&wrapped, &mut b);
        }
        Value::String(s) => b.message = Some(cap(s)),
        _ => {}
    }
    body_error(&b, url, has_image)
}

/// 2xx 的响应体（或流中的一个 data）在协议正文之外表达失败的形态。识别出来时按厂商错误码 / 状态码 /
/// 关键词归类，**不会**被当成「没有结束标记就断流」报成 network：
/// - MiniMax：`{"choices":null, "base_resp":{"status_code":1004, "status_msg":"login fail: …"}}`。`status_code ≠ 0` 即失败，
///   不论有没有 choices（成功的最后一块也带 `base_resp`，status_code 为 0）；
/// - 中转站 / 兼容端点常见的顶层 `{code, msg | message}`、`{success:false, message}`、`{object:"error", message}`：
///   有信息字段、有失败标记、且没有任何协议正文字段。
///
/// 顶层 `error` 对象不在这里处理：它的形状因协议而异，由各协议的适配器交给 [`stream_error`]
pub(crate) fn payload_error(v: &Value, url: &str, has_image: bool) -> Option<LlmError> {
    let obj = v.as_object()?;
    if vendor_status(v).is_none() && !is_bare_failure(obj) {
        return None;
    }
    let mut b = ErrorBody {
        json: true,
        ..ErrorBody::default()
    };
    extract(v, &mut b);
    Some(body_error(&b, url, has_image))
}

/// 协议正文字段：出现任何一个（且不为 null）就不是「裸」的出错形态
const PAYLOAD_KEYS: [&str; 8] = [
    "choices",
    "candidates",
    "output",
    "content",
    "delta",
    "response",
    "usageMetadata",
    "promptFeedback",
];

/// 顶层 `{code, msg}` 一类：有信息字段，有失败标记（code 不是成功值、success:false、object / type / status 为 error），
/// 且没有协议正文
fn is_bare_failure(obj: &serde_json::Map<String, Value>) -> bool {
    if PAYLOAD_KEYS.iter().any(|k| obj.get(*k).is_some_and(|x| !x.is_null())) {
        return false;
    }
    let has_message = MESSAGE_KEYS.iter().any(|k| {
        obj.get(*k)
            .and_then(Value::as_str)
            .is_some_and(|m| !m.trim().is_empty())
    });
    if !has_message {
        return false;
    }
    let failed_code = CODE_KEYS
        .iter()
        .filter_map(|k| obj.get(*k))
        .any(|c| !c.is_null() && !is_success_code(c));
    let is_error_word = |k: &str| {
        obj.get(k)
            .and_then(Value::as_str)
            .is_some_and(|t| matches!(t.to_ascii_lowercase().as_str(), "error" | "fail" | "failed" | "failure"))
    };
    failed_code
        || obj.get("success").and_then(Value::as_bool) == Some(false)
        || is_error_word("object")
        || is_error_word("type")
        || is_error_word("status")
}

/// 成功值：0 / 200（整数或字符串）、"success" / "ok"
fn is_success_code(c: &Value) -> bool {
    match c {
        Value::Number(n) => matches!(n.as_i64(), Some(0 | 200)),
        Value::String(s) => {
            let s = s.trim();
            matches!(s, "0" | "200") || s.eq_ignore_ascii_case("success") || s.eq_ignore_ascii_case("ok")
        }
        Value::Bool(b) => *b,
        _ => false,
    }
}

/// 流中 / 2xx 响应体里的错误归类：MiniMax 的 base_resp 错误码 → 数值 code 当作 HTTP 状态 → 关键词。
/// 带图请求里服务端明确说「不支持图片输入」的（OpenRouter 的 200 + error chunk 等）先归为确定的 no_vision
fn body_error(b: &ErrorBody, url: &str, has_image: bool) -> LlmError {
    let hay = b.haystack();
    let vendor = b.vendor_status.and_then(minimax_code);
    let code = match vendor {
        Some((code, _)) => code,
        None if has_image && is_conclusive_no_vision(&hay) => ErrorCode::NoVision,
        None => match b.code.as_deref().and_then(|c| c.parse::<u16>().ok()) {
            Some(n @ 100..=599) => status_code(n, &hay),
            _ => keyword_code(&hay),
        },
    };
    let mut e = with_url(make(code, b.display(None)), url);
    if let Some((_, Some(retryable))) = vendor {
        e.retryable = retryable;
    }
    e.upstream_request_id = b.request_id.clone();
    e
}

/// MiniMax 的 `base_resp.status_code`（官方错误码表）；第二项覆盖默认的 retryable。表外的码按关键词归类
fn minimax_code(code: i64) -> Option<(ErrorCode, Option<bool>)> {
    Some(match code {
        // 未知错误、服务内部错误、下游服务错误
        1000 | 1013 | 1024 | 1033 => (ErrorCode::Upstream, None),
        1001 => (ErrorCode::Timeout, None),
        // RPM / TPM / 连接数 / 请求增速限流
        1002 | 1039 | 1041 | 2045 => (ErrorCode::RateLimited, None),
        // 鉴权失败（login fail）、无效的 API Key
        1004 | 2049 => (ErrorCode::Auth, None),
        // 余额不足、超出用量上限
        1008 | 2056 => (ErrorCode::Quota, None),
        // 输入 / 输出内容涉敏
        1026 | 1027 => (ErrorCode::ContentFilter, None),
        // 非法字符过多、参数错误：重试也不会成功
        1042 | 2013 => (ErrorCode::Upstream, Some(false)),
        _ => return None,
    })
}

/// 流中错误的数值 code 当作 HTTP 状态处理
fn status_code(status: u16, hay: &str) -> ErrorCode {
    match status {
        401 | 403 => ErrorCode::Auth,
        402 => ErrorCode::Quota,
        404 => ErrorCode::NotFound,
        408 => ErrorCode::Timeout,
        413 => ErrorCode::TooLong,
        429 => {
            if any_word(hay, &["quota", "credit", "billing", "balance", "余额"]) {
                ErrorCode::Quota
            } else {
                ErrorCode::RateLimited
            }
        }
        _ => keyword_code(hay),
    }
}

/// 字符串 code / type / 信息的映射（也用于中转站 2xx 的 `{code, msg}`）
fn keyword_code(hay: &str) -> ErrorCode {
    if any_word(
        hay,
        &["rate_limit", "rate limit", "too many requests", "限流", "请求频率"],
    ) {
        ErrorCode::RateLimited
    } else if any_word(
        hay,
        &[
            "insufficient_quota",
            "quota",
            "billing",
            "credit",
            "insufficient balance",
            "insufficient_balance",
            "arrearage",
            "余额",
            "欠费",
        ],
    ) {
        ErrorCode::Quota
    } else if any_word(hay, &["context_length", "too_long", "too long"]) {
        ErrorCode::TooLong
    } else if any_word(
        hay,
        &[
            "content_filter",
            "policy",
            "invalid_prompt",
            "safety",
            "refusal",
            "moderation",
            "new_sensitive",
            "sensitive content",
            "敏感",
            "涉敏",
        ],
    ) {
        ErrorCode::ContentFilter
    } else if any_word(
        hay,
        &[
            "authentication",
            "api_key",
            "api key",
            "permission",
            "unauthorized",
            "login fail",
            "invalid token",
            "token is invalid",
            "令牌",
            "鉴权",
            "认证",
        ],
    ) {
        ErrorCode::Auth
    } else if hay.contains("not_found") {
        ErrorCode::NotFound
    } else {
        // overloaded_error / server_error / api_error / timeout_error / 未知
        ErrorCode::Upstream
    }
}

/// 2xx 的 JSON 既不是出错形态、也不是这个协议的响应：归为 protocol（多半是模板或 base_url 不对），
/// 信息优先用服务端原文
pub(crate) fn unexpected_json(v: &Value, protocol_name: &str, url: &str) -> LlmError {
    let mut b = ErrorBody {
        json: true,
        ..ErrorBody::default()
    };
    if v.is_object() {
        extract(v, &mut b);
    }
    let message = b.message.unwrap_or_else(|| {
        format!("the endpoint returned JSON that is not a {protocol_name} response; check the template and base URL")
    });
    protocol(message, url)
}

/// 终止的 param_unsupported 改为会出卡片的 upstream（param_unsupported 只以降级成功后的
/// `Notice{param_degraded}` 弱提示出现，不出卡片）。降级不了——识别不出是哪个参数、同一参数降级后仍被拒、
/// 降级后又被拒了另一个参数——时请求失败，必须让用户看到：
/// - `adjusted = true`：新被拒的参数已经写进能力缓存，点「重试」一次即可成功；
/// - `adjusted = false`：重试也不会成功（换模型或检查模板）
pub(crate) fn settle_param_unsupported(mut e: LlmError, adjusted: bool) -> LlmError {
    if e.code != ErrorCode::ParamUnsupported {
        return e;
    }
    e.code = ErrorCode::Upstream;
    e.retryable = adjusted;
    e.hint = Some(
        if adjusted {
            HINT_PARAM_ADJUSTED
        } else {
            HINT_PARAM_REJECTED
        }
        .to_owned(),
    );
    e
}

// ---------------------------------------------------------------------------
// 传输层

/// 传输层错误（DNS / TLS / 拒绝连接 / 连接超时 / 中途断开）。返回 (错误, 是否值得在流开始前自动重试)
pub(crate) fn transport(err: &reqwest::Error, url: &str) -> (LlmError, bool) {
    let (what, retry) = describe_transport(err);
    let mut e = with_url(make(ErrorCode::Network, what), url);
    if err.is_builder() {
        e = with_url(not_configured("the request URL is invalid; check the base URL"), url);
        return (e, false);
    }
    e.retryable = true;
    (e, retry)
}

/// 按错误链描述原因（只用固定的英文短语，绝不带上 reqwest 的 Display——它包含完整 URL，可能有 ?key=）
fn describe_transport(err: &reqwest::Error) -> (&'static str, bool) {
    let mut chain = String::new();
    let mut io_kind = None;
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(s) = source {
        if let Some(io) = s.downcast_ref::<std::io::Error>() {
            io_kind.get_or_insert(io.kind());
        }
        chain.push_str(&s.to_string().to_ascii_lowercase());
        chain.push(' ');
        source = s.source();
    }
    use std::io::ErrorKind as K;
    if chain.contains("dns error") || chain.contains("failed to lookup") || chain.contains("nodename nor servname") {
        return ("DNS lookup failed", false);
    }
    if chain.contains("certificate") || chain.contains("invalidcertificate") || chain.contains("unknownissuer") {
        return ("TLS certificate verification failed", false);
    }
    if err.is_timeout() || io_kind == Some(K::TimedOut) {
        return ("connection timed out", true);
    }
    match io_kind {
        Some(K::ConnectionRefused) => ("connection refused", true),
        Some(K::ConnectionReset | K::ConnectionAborted | K::BrokenPipe | K::UnexpectedEof) => {
            ("connection reset", true)
        }
        _ if chain.contains("tls") || chain.contains("handshake") => ("TLS handshake failed", true),
        _ => ("connection failed", true),
    }
}

// ---------------------------------------------------------------------------
// 响应头

/// `retry-after-ms` / `retry-after`（秒数或 HTTP 日期）→ 秒（向上取整）
pub(crate) fn retry_after_secs(headers: &HeaderMap) -> Option<u32> {
    let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim);
    if let Some(ms) = get("retry-after-ms").and_then(|v| v.parse::<f64>().ok())
        && ms.is_finite()
        && ms >= 0.0
    {
        return Some(secs_ceil(ms / 1000.0));
    }
    let v = get("retry-after")?;
    if let Ok(secs) = v.parse::<f64>() {
        return (secs.is_finite() && secs >= 0.0).then(|| secs_ceil(secs));
    }
    let at = httpdate::parse_http_date(v).ok()?;
    let wait = at.duration_since(SystemTime::now()).map_or(0.0, |d| d.as_secs_f64());
    Some(secs_ceil(wait))
}

/// 向上取整到秒；f64 → u32 的 `as` 转换是饱和的（负数与 NaN 得 0，过大得 u32::MAX）
fn secs_ceil(secs: f64) -> u32 {
    secs.ceil() as u32
}

/// 上游请求 id（x-request-id / request-id）
pub(crate) fn upstream_request_id(headers: &HeaderMap) -> Option<String> {
    ["x-request-id", "request-id"]
        .iter()
        .find_map(|h| headers.get(*h).and_then(|v| v.to_str().ok()))
        .map(|s| s.trim().chars().take(128).collect::<String>())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_paths() {
        let b = parse_body(
            br#"{"error":{"message":"bad key","type":"invalid_request_error","code":"invalid_api_key"},"status":401}"#,
        );
        assert_eq!(b.message.as_deref(), Some("bad key"));
        assert_eq!(b.code.as_deref(), Some("invalid_api_key"));
        let b = parse_body(br#"{"code":30014,"message":"Token is invalid."}"#);
        assert_eq!(
            (b.code.as_deref(), b.message.as_deref()),
            (Some("30014"), Some("Token is invalid."))
        );
        let b = parse_body(br#"{"type":"error","error":{"type":"authorized_error","http_code":"401"}}"#);
        assert_eq!(b.display(Some(401)), "authorized_error");
        let b = parse_body(br#"[{"error":{"code":400,"message":"x","status":"INVALID_ARGUMENT","details":[{"reason":"API_KEY_INVALID"}]}}]"#);
        assert_eq!(b.reasons, vec!["API_KEY_INVALID"]);
        let b = parse_body(b"Authentication Fails (governor)");
        assert_eq!(b.text.as_deref(), Some("Authentication Fails (governor)"));
        let b = parse_body(b"<!DOCTYPE html><html><body>404</body></html>");
        assert!(b.html);
        assert_eq!(b.display(Some(404)), "the server returned an HTML page (HTTP 404)");
    }

    #[test]
    fn echoed_keys_are_redacted() {
        let key = crate::secret::Secret::new("sk-relay-1234567890".to_owned());
        let e = make(ErrorCode::Auth, "invalid token sk-relay-1234567890 for user");
        assert_eq!(finalize(e, Some(&key)).message, "invalid token *** for user");
        let short = crate::secret::Secret::new("abc".to_owned());
        let e = make(ErrorCode::Auth, "abc");
        assert_eq!(finalize(e, Some(&short)).message, "abc");
    }

    #[test]
    fn keys_straddling_the_cut_are_redacted_before_truncation() {
        // 48 字符的 Key 从第 285 个字符开始：旧实现先截断到 300 再脱敏，留下 Key 的前 15 个字符
        let key_text = "sk-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrs";
        let key = crate::secret::Secret::new(key_text.to_owned());
        for at in [250, 270, 285, 296, 299, 300] {
            let raw = format!("{}{key_text} is not a valid key{}", "x".repeat(at), "y".repeat(400));
            let body = format!(r#"{{"error":{{"message":"{raw}"}}}}"#);
            let e = parse_body(body.as_bytes());
            let msg = finalize(make(ErrorCode::Auth, e.message.expect("message")), Some(&key)).message;
            assert!(!msg.contains("sk-ABCD"), "at {at}: {msg}");
            assert!(msg.chars().count() <= MAX_MESSAGE_CHARS + 1, "at {at}");
        }
        // 解析阶段的宽松上限切开了 Key，而脱敏后整段又短于 300：留下的前缀同样替换
        let raw = key_text.repeat(RAW_MESSAGE_CHARS / key_text.len() + 1);
        let msg = finalize(make(ErrorCode::Auth, &raw), Some(&key)).message;
        assert_eq!(
            msg,
            format!("{}…", "***".repeat(RAW_MESSAGE_CHARS / key_text.len() + 1))
        );
        // 服务端本来就只回显了 Key 的一部分、又正好落在截断点：长度 ≥ 4 的前缀也替换
        let raw = format!("{}sk-ABCDEFG{}", "z".repeat(MAX_MESSAGE_CHARS - 10), "w".repeat(50));
        let msg = finalize(make(ErrorCode::Auth, &raw), Some(&key)).message;
        assert!(msg.ends_with("***…"), "{msg}");
        // 截断点前面只是 `sk-` 这样的公共前缀：不动
        let raw = format!("{}sk-{}", "z".repeat(MAX_MESSAGE_CHARS - 3), "w".repeat(50));
        let msg = finalize(make(ErrorCode::Auth, &raw), Some(&key)).message;
        assert!(msg.ends_with("sk-…"), "{msg}");
    }

    #[test]
    fn long_text_is_truncated() {
        let long = "x".repeat(10_000);
        let b = parse_body(long.as_bytes());
        assert_eq!(b.text.as_ref().map(|t| t.chars().count()), Some(RAW_MESSAGE_CHARS + 1));
        let e = finalize(make(ErrorCode::Protocol, b.text.expect("text")), None);
        assert_eq!(e.message.chars().count(), MAX_MESSAGE_CHARS + 1);
        assert!(e.message.ends_with('…'));
    }

    #[test]
    fn payload_error_shapes() {
        // MiniMax：200 + base_resp
        let v = serde_json::json!({
            "id": "", "choices": null, "created": 0, "model": "", "object": "", "usage": null,
            "base_resp": {"status_code": 1004, "status_msg": "login fail: Please carry the API secret key in the 'Authorization' field of the request header"}
        });
        let e = payload_error(&v, "https://api.minimaxi.com/v1/chat/completions", false).expect("error");
        assert_eq!((e.code, e.retryable), (ErrorCode::Auth, false));
        assert!(e.message.starts_with("login fail"));
        assert_eq!(e.hint.as_deref(), Some(HINT_CHECK_KEY));
        assert_eq!(e.url.as_deref(), Some("api.minimaxi.com/v1/chat/completions"));
        let mm = |code: i64, msg: &str| {
            let v = serde_json::json!({"choices": null, "base_resp": {"status_code": code, "status_msg": msg}});
            payload_error(&v, "", false).expect("error")
        };
        assert_eq!(mm(1008, "insufficient balance").code, ErrorCode::Quota);
        assert_eq!(mm(1002, "rate limit exceeded(RPM)").code, ErrorCode::RateLimited);
        assert_eq!(mm(1027, "output new_sensitive").code, ErrorCode::ContentFilter);
        assert_eq!(mm(2049, "invalid api key").code, ErrorCode::Auth);
        assert_eq!(
            (mm(2013, "invalid params").code, mm(2013, "x").retryable),
            (ErrorCode::Upstream, false)
        );
        assert_eq!(
            (mm(1000, "unknown error").code, mm(1000, "x").retryable),
            (ErrorCode::Upstream, true)
        );
        // 表外的码按关键词
        assert_eq!(mm(9999, "token is invalid").code, ErrorCode::Auth);
        // 成功的最后一块带 status_code 0：不是错误
        let ok = serde_json::json!({"choices": [{"delta": {}, "finish_reason": "stop"}], "base_resp": {"status_code": 0, "status_msg": ""}});
        assert!(payload_error(&ok, "", false).is_none());
        // 即使有 choices，base_resp 报错仍以它为准（输出涉敏）
        let v = serde_json::json!({"choices": [{"delta": {"content": ""}}], "base_resp": {"status_code": "1027", "status_msg": "output new_sensitive"}});
        assert_eq!(
            payload_error(&v, "", false).expect("error").code,
            ErrorCode::ContentFilter
        );

        // 中转站的顶层 {code, msg}
        let relay = |v: serde_json::Value| payload_error(&v, "", false).map(|e| e.code);
        assert_eq!(
            relay(serde_json::json!({"code": 401, "msg": "无效的令牌"})),
            Some(ErrorCode::Auth)
        );
        assert_eq!(
            relay(serde_json::json!({"code": "10001", "message": "invalid token"})),
            Some(ErrorCode::Auth)
        );
        assert_eq!(
            relay(serde_json::json!({"code": 429, "msg": "余额不足"})),
            Some(ErrorCode::Quota)
        );
        assert_eq!(
            relay(serde_json::json!({"code": 500, "msg": "upstream timeout"})),
            Some(ErrorCode::Upstream)
        );
        assert_eq!(
            relay(serde_json::json!({"success": false, "message": "当前分组上游负载已饱和"})),
            Some(ErrorCode::Upstream)
        );
        assert_eq!(
            relay(
                serde_json::json!({"object": "error", "message": "model is overloaded", "type": "ServiceUnavailableError"})
            ),
            Some(ErrorCode::Upstream)
        );
        // 成功值、带协议正文、没有信息字段：都不是
        assert_eq!(
            relay(serde_json::json!({"code": 0, "msg": "success", "data": {}})),
            None
        );
        assert_eq!(relay(serde_json::json!({"code": "200", "message": "ok"})), None);
        assert_eq!(
            relay(serde_json::json!({"code": 0, "message": "Success", "choices": [{"delta": {"content": "a"}}]})),
            None
        );
        assert_eq!(relay(serde_json::json!({"code": 500})), None);
        assert_eq!(
            relay(serde_json::json!({"type": "message_start", "message": {"id": "m"}})),
            None
        );
        assert_eq!(
            relay(serde_json::json!({"type": "response.output_text.delta", "delta": "x"})),
            None
        );
    }

    #[test]
    fn terminal_param_unsupported_becomes_a_carded_upstream() {
        let e = make(ErrorCode::ParamUnsupported, "Unknown parameter: 'thinking'");
        let adjusted = settle_param_unsupported(e.clone(), true);
        assert_eq!(
            (adjusted.code, adjusted.retryable, adjusted.hint.as_deref()),
            (ErrorCode::Upstream, true, Some(HINT_PARAM_ADJUSTED))
        );
        let rejected = settle_param_unsupported(e, false);
        assert_eq!(
            (rejected.code, rejected.retryable, rejected.hint.as_deref()),
            (ErrorCode::Upstream, false, Some(HINT_PARAM_REJECTED))
        );
        let other = settle_param_unsupported(make(ErrorCode::Auth, "x"), true);
        assert_eq!(other.code, ErrorCode::Auth);
    }

    #[test]
    fn retry_after_forms() {
        let mut h = HeaderMap::new();
        h.insert("retry-after", "7".parse().expect("header"));
        assert_eq!(retry_after_secs(&h), Some(7));
        h.insert("retry-after", "1.2".parse().expect("header"));
        assert_eq!(retry_after_secs(&h), Some(2));
        h.insert("retry-after-ms", "250".parse().expect("header"));
        assert_eq!(retry_after_secs(&h), Some(1));
        let mut h = HeaderMap::new();
        let future = SystemTime::now() + std::time::Duration::from_secs(30);
        h.insert("retry-after", httpdate::fmt_http_date(future).parse().expect("header"));
        assert!(retry_after_secs(&h).is_some_and(|s| (29..=31).contains(&s)));
        h.insert("retry-after", "garbage".parse().expect("header"));
        assert_eq!(retry_after_secs(&h), None);
    }

    #[test]
    fn stream_error_codes() {
        let e = stream_error(
            &serde_json::json!({"code": "rate_limit_exceeded", "message": "slow"}),
            "",
            false,
        );
        assert_eq!(e.code, ErrorCode::RateLimited);
        let e = stream_error(
            &serde_json::json!({"code": "server_error", "message": "boom"}),
            "",
            false,
        );
        assert_eq!((e.code, e.retryable), (ErrorCode::Upstream, true));
        let e = stream_error(
            &serde_json::json!({"code": "invalid_prompt", "message": "flagged"}),
            "",
            false,
        );
        assert_eq!(e.code, ErrorCode::ContentFilter);
        let e = stream_error(
            &serde_json::json!({"error": {"code": 502, "message": "Provider returned error"}}),
            "",
            false,
        );
        assert_eq!(e.code, ErrorCode::Upstream);
        let e = stream_error(
            &serde_json::json!({"error": {"code": 401, "message": "No auth"}}),
            "",
            false,
        );
        assert_eq!(e.code, ErrorCode::Auth);
        let e = stream_error(
            &serde_json::json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
            "",
            false,
        );
        assert_eq!(
            (e.code, e.retryable, e.message.as_str()),
            (ErrorCode::Upstream, true, "Overloaded")
        );
        // 字符串 code
        let e = stream_error(
            &serde_json::json!({"error": {"code": "invalid_request", "message": "bad"}}),
            "",
            false,
        );
        assert_eq!(e.code, ErrorCode::Upstream);
        // 带图请求：流中的「不支持图片输入」是确定的 no_vision；不带图时照旧
        let no_vision =
            serde_json::json!({"error": {"code": 400, "message": "This model does not support image input"}});
        let e = stream_error(&no_vision, "", true);
        assert_eq!((e.code, e.retryable), (ErrorCode::NoVision, false));
        assert!(is_conclusive_no_vision(&e.message.to_lowercase()));
        assert_ne!(stream_error(&no_vision, "", false).code, ErrorCode::NoVision);
        // 只是传输格式不被支持：不算确定的 no_vision
        let e = stream_error(
            &serde_json::json!({"error": {"code": 500, "message": "This endpoint does not support base64 image input"}}),
            "",
            true,
        );
        assert_ne!(e.code, ErrorCode::NoVision);
    }
}
