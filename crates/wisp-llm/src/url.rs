//! base_url 规范化与最终 URL 拼接；12 组测试向量见 tests/url_vectors.rs。
//!
//! 规范化（与 Cherry Studio 的 `formatApiHost` 一致）：
//! ① trim 后去掉末尾所有 `/`；② 以 `#` 结尾时 force=true，去掉 `#` 再去掉末尾 `/`；③ 没有 http(s) scheme 时补 `https://`
//! （回环地址补 `http://`）；
//! ④ force，或 path 部分匹配 `(?i)/v\d+(alpha|beta)?(/|$)` 时原样返回；⑤ 否则 Chat / Responses / Anthropic 补 `/v1`，
//! Gemini 补 `/v1beta`。之后各协议只拼固定的资源名。
//!
//! 用户常把完整接口地址粘进 Base URL（`…/v1/chat/completions`）。path 以资源段结尾
//! （`/chat/completions`、`/responses`、`/messages`、`/models/<m>:streamGenerateContent`）时先去掉它再规范化，预览给出
//! `url_suffix_stripped` 提示；查询串（`?api-version=…`）从前缀里拿出来、拼到最终 URL 的末尾（不再拼坏成
//! `…/?a=1/v1/chat/completions`），预览给出 `url_query_kept`；中间的 `#…` 片段丢弃并提示 `url_fragment_dropped`。

use wisp_types::{Protocol, UrlPreview};

/// 端点种类
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Endpoint {
    Chat,
    Models,
}

/// 预览提示：去掉了末尾的接口路径（i18n key）
pub const URL_WARN_SUFFIX_STRIPPED: &str = "settings.providers.url_warning.suffix_stripped";
/// 预览提示：查询串会附加到每个请求的末尾
pub const URL_WARN_QUERY_KEPT: &str = "settings.providers.url_warning.query_kept";
/// 预览提示：地址中间的 `#…` 片段被丢弃
pub const URL_WARN_FRAGMENT_DROPPED: &str = "settings.providers.url_warning.fragment_dropped";

/// 规范化的中间结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Normalized {
    /// 端点前缀（不带末尾 `/`、不含查询串）；输入为空时为空串
    pub prefix: String,
    /// 用户以 `#` 结尾，表示「按我填的路径请求，不追加版本段」
    pub forced: bool,
    /// 规范化时追加了版本段
    pub version_appended: bool,
    /// 用户写在 Base URL 里的查询串（不含 `?`），拼到最终 URL 的末尾
    pub query: Option<String>,
    /// 去掉了末尾的接口路径
    pub suffix_stripped: bool,
    /// 丢弃了地址中间的 `#…` 片段
    pub fragment_dropped: bool,
}

impl Normalized {
    /// 拼出最终 URL（含用户的查询串）；前缀为空时返回空串
    pub(crate) fn url(&self, protocol: Protocol, ep: Endpoint, model: Option<&str>) -> String {
        if self.prefix.is_empty() {
            return String::new();
        }
        let url = join(&self.prefix, protocol, ep, model);
        match &self.query {
            Some(q) => {
                let sep = if url.contains('?') { '&' } else { '?' };
                format!("{url}{sep}{q}")
            }
            None => url,
        }
    }

    /// 预览的提示（只给一条，按严重程度）
    fn warning(&self) -> Option<&'static str> {
        if self.suffix_stripped {
            Some(URL_WARN_SUFFIX_STRIPPED)
        } else if self.fragment_dropped {
            Some(URL_WARN_FRAGMENT_DROPPED)
        } else if self.query.is_some() {
            Some(URL_WARN_QUERY_KEPT)
        } else {
            None
        }
    }
}

/// 规范化用户输入的 base_url
pub fn normalize_base_url(base: &str, protocol: Protocol) -> String {
    normalize(base, protocol).prefix
}

/// 拼出最终请求 URL。Gemini 的对话端点没有给模型时用 `{model}` 占位；输入为空时返回空串
pub fn endpoint_url(base: &str, protocol: Protocol, ep: Endpoint, model: Option<&str>) -> String {
    normalize(base, protocol).url(protocol, ep, model)
}

/// 设置页的 URL 预览
pub fn url_preview(protocol: Protocol, base: &str, model: Option<&str>) -> UrlPreview {
    let n = normalize(base, protocol);
    UrlPreview {
        method: "POST".to_owned(),
        url: n.url(protocol, Endpoint::Chat, model),
        version_appended: n.version_appended,
        forced: n.forced,
        warning: n.warning().map(str::to_owned),
    }
}

/// 规范化，同时给出是否强制、是否追加了版本段
pub(crate) fn normalize(base: &str, protocol: Protocol) -> Normalized {
    let mut s = base.trim().trim_end_matches('/');
    let forced = s.ends_with('#');
    if forced {
        s = s[..s.len() - 1].trim_end_matches('/');
    }
    // 中间的 `#…` 是片段，不会发给服务端
    let fragment_dropped = match s.find('#') {
        Some(i) => {
            s = &s[..i];
            true
        }
        None => false,
    };
    let mut query = None;
    if let Some(i) = s.find('?') {
        // `alt=sse` 由 Gemini 端点自己带上（粘贴完整的 Gemini 地址时不重复）
        let q: Vec<&str> = s[i + 1..]
            .split('&')
            .filter(|kv| !kv.is_empty() && !kv.to_ascii_lowercase().starts_with("alt="))
            .collect();
        if !q.is_empty() {
            query = Some(q.join("&"));
        }
        s = &s[..i];
    }
    s = s.trim_end_matches('/');
    let (stripped, suffix_stripped) = strip_resource_suffix(s);
    s = stripped.trim_end_matches('/');
    if s.is_empty() {
        return Normalized {
            prefix: String::new(),
            forced,
            version_appended: false,
            query: None,
            suffix_stripped: false,
            fragment_dropped: false,
        };
    }
    let mut prefix = if has_scheme(s) {
        s.to_owned()
    } else if is_loopback_host(&host_of(s)) {
        // 本机服务（Ollama / LM Studio）几乎都是明文 http；裸写 `localhost:11434` 时补 https 必然连不上
        format!("http://{s}")
    } else {
        format!("https://{s}")
    };
    let version_appended = !forced && !has_version_segment(path_of(&prefix));
    if version_appended {
        prefix.push_str(match protocol {
            Protocol::OpenAiChat | Protocol::OpenAiResponses | Protocol::AnthropicMessages => "/v1",
            Protocol::Gemini => "/v1beta",
        });
    }
    Normalized {
        prefix,
        forced,
        version_appended,
        query,
        suffix_stripped,
        fragment_dropped,
    }
}

/// 末尾是接口路径（四种协议的对话端点）时去掉它；返回 (剩下的部分, 是否去掉了)。只看 path，不碰 host
fn strip_resource_suffix(s: &str) -> (&str, bool) {
    let path_start = s.split_once("://").map_or(0, |(scheme, rest)| {
        scheme.len() + 3 + rest.find('/').unwrap_or(rest.len())
    });
    let path = &s[path_start..];
    let lower = path.to_ascii_lowercase();
    // Gemini：…/models/<model>:streamGenerateContent 或 :generateContent
    if let Some(colon) = lower.rfind(':')
        && matches!(&lower[colon..], ":streamgeneratecontent" | ":generatecontent")
        && let Some(models) = lower[..colon].rfind("/models/")
        && !lower[models + "/models/".len()..colon].contains('/')
    {
        return (&s[..path_start + models], true);
    }
    for suffix in ["/chat/completions", "/responses", "/messages"] {
        if lower.ends_with(suffix) {
            return (&s[..s.len() - suffix.len()], true);
        }
    }
    (s, false)
}

/// 在规范化后的前缀上拼资源路径
pub(crate) fn join(prefix: &str, protocol: Protocol, ep: Endpoint, model: Option<&str>) -> String {
    match (protocol, ep) {
        (Protocol::OpenAiChat, Endpoint::Chat) => format!("{prefix}/chat/completions"),
        (Protocol::OpenAiResponses, Endpoint::Chat) => format!("{prefix}/responses"),
        (Protocol::AnthropicMessages, Endpoint::Chat) => format!("{prefix}/messages"),
        (Protocol::Gemini, Endpoint::Chat) => {
            let model = match model.map(gemini_model_segment) {
                Some(m) if !m.is_empty() => m,
                _ => "{model}".to_owned(),
            };
            format!("{prefix}/models/{model}:streamGenerateContent?alt=sse")
        }
        (Protocol::OpenAiChat | Protocol::OpenAiResponses, Endpoint::Models) => format!("{prefix}/models"),
        (Protocol::AnthropicMessages, Endpoint::Models) => format!("{prefix}/models?limit=1000"),
        (Protocol::Gemini, Endpoint::Models) => format!("{prefix}/models?pageSize=1000"),
    }
}

/// Gemini 模型名：去掉 `models/` 前缀后做 path 编码（只保留 RFC 3986 的 unreserved 字符）
pub(crate) fn gemini_model_segment(model: &str) -> String {
    let m = model.trim();
    let m = m.strip_prefix("models/").unwrap_or(m);
    let mut out = String::with_capacity(m.len());
    for b in m.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            out.push('%');
            out.push(char::from(HEX[usize::from(b >> 4)]));
            out.push(char::from(HEX[usize::from(b & 0x0f)]));
        }
    }
    out
}

/// 只到 host + path 的展示用 URL（不含 scheme、query、fragment、userinfo；`LlmError.url` 用）
pub(crate) fn display_url(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = &rest[..rest.find(['?', '#']).unwrap_or(rest.len())];
    match rest.find('/') {
        Some(slash) => {
            let (authority, path) = rest.split_at(slash);
            format!("{}{path}", strip_userinfo(authority))
        }
        None => strip_userinfo(rest).to_owned(),
    }
}

fn strip_userinfo(authority: &str) -> &str {
    authority.rsplit_once('@').map_or(authority, |(_, host)| host)
}

/// URL 的 host（小写，不含端口与方括号）
pub(crate) fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = strip_userinfo(rest.split(['/', '?', '#']).next().unwrap_or(""));
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or("")
    } else {
        match authority.rsplit_once(':') {
            Some((h, port)) if port.bytes().all(|b| b.is_ascii_digit()) => h,
            _ => authority,
        }
    };
    host.to_ascii_lowercase()
}

/// 是否为本机回环地址。这类请求不走系统代理：代理既不需要、通常也无法转发到用户本机的 Ollama / LM Studio，
/// 而 macOS 的系统代理例外列表不会被 reqwest 读取
pub(crate) fn is_loopback_host(host: &str) -> bool {
    host == "localhost"
        || host.ends_with(".localhost")
        || host.parse::<std::net::Ipv4Addr>().is_ok_and(|ip| ip.is_loopback())
        || host.parse::<std::net::Ipv6Addr>().is_ok_and(|ip| ip.is_loopback())
}

fn has_scheme(s: &str) -> bool {
    let head = s.get(..8).unwrap_or(s).to_ascii_lowercase();
    head.starts_with("http://") || head.starts_with("https://")
}

/// 只取 path 部分（scheme://authority 之后、query 之前）
fn path_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = &rest[..rest.find(['?', '#']).unwrap_or(rest.len())];
    rest.find('/').map_or("", |i| &rest[i..])
}

/// path 中是否有版本段：`/v` + 数字 + 可选 alpha / beta，其后为 `/` 或结尾（不区分大小写）
pub(crate) fn has_version_segment(path: &str) -> bool {
    path.split('/').any(|seg| {
        let seg = seg.to_ascii_lowercase();
        let Some(rest) = seg.strip_prefix('v') else {
            return false;
        };
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        digits > 0 && matches!(&rest[digits..], "" | "alpha" | "beta")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_segment_rules() {
        assert!(has_version_segment("/v1"));
        assert!(has_version_segment("/api/paas/v4"));
        assert!(has_version_segment("/V1Beta"));
        assert!(has_version_segment("/v2alpha/x"));
        assert!(!has_version_segment("/v1beta1"));
        assert!(!has_version_segment("/api/vx"));
        assert!(!has_version_segment("/version"));
        assert!(!has_version_segment(""));
    }

    #[test]
    fn display_url_strips_scheme_query_and_userinfo() {
        assert_eq!(
            display_url("https://api.openai.com/v1/chat/completions"),
            "api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            display_url("https://g.example.com/v1beta/models/x:streamGenerateContent?alt=sse&key=abc"),
            "g.example.com/v1beta/models/x:streamGenerateContent"
        );
        assert_eq!(display_url("http://user:pw@host:8080/p"), "host:8080/p");
    }

    #[test]
    fn host_and_loopback() {
        assert_eq!(host_of("http://LocalHost:11434/v1"), "localhost");
        assert_eq!(host_of("http://[::1]:1234/v1"), "::1");
        assert_eq!(host_of("https://api.anthropic.com/v1/messages"), "api.anthropic.com");
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("127.8.0.1"));
        assert!(is_loopback_host("::1"));
        assert!(!is_loopback_host("192.168.1.2"));
        assert!(!is_loopback_host("api.openai.com"));
    }

    #[test]
    fn empty_input_stays_empty() {
        assert_eq!(normalize_base_url("  / ", Protocol::OpenAiChat), "");
        assert_eq!(endpoint_url("", Protocol::Gemini, Endpoint::Chat, Some("m")), "");
        assert_eq!(url_preview(Protocol::OpenAiChat, "#", None).url, "");
    }

    #[test]
    fn gemini_segment_encodes() {
        assert_eq!(gemini_model_segment("models/gemini-2.5-flash"), "gemini-2.5-flash");
        assert_eq!(gemini_model_segment("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn pasted_endpoint_urls_are_trimmed_to_the_base() {
        use Protocol::{AnthropicMessages as A, Gemini as G, OpenAiChat as C, OpenAiResponses as R};
        let cases: [(&str, Protocol, &str); 6] = [
            (
                "https://x.example.com/api/v1/chat/completions",
                C,
                "https://x.example.com/api/v1/chat/completions",
            ),
            (
                "https://x.example.com/v1/responses/",
                R,
                "https://x.example.com/v1/responses",
            ),
            (
                "https://api.anthropic.com/v1/messages",
                A,
                "https://api.anthropic.com/v1/messages",
            ),
            // 没有版本段的完整地址：去掉资源段后照常补 /v1
            (
                "https://relay.example.com/chat/completions",
                C,
                "https://relay.example.com/v1/chat/completions",
            ),
            (
                "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse",
                G,
                "https://generativelanguage.googleapis.com/v1beta/models/m:streamGenerateContent?alt=sse",
            ),
            // host 里出现同样的字样不受影响
            (
                "https://messages.example.com",
                A,
                "https://messages.example.com/v1/messages",
            ),
        ];
        for (base, protocol, expected) in cases {
            let p = url_preview(protocol, base, Some("m"));
            assert_eq!(p.url, expected, "{base}");
        }
        let p = url_preview(C, "https://x.example.com/api/v1/chat/completions", None);
        assert_eq!(p.warning.as_deref(), Some(URL_WARN_SUFFIX_STRIPPED));
        assert_eq!(url_preview(C, "https://x.example.com/v1", None).warning, None);
    }

    #[test]
    fn query_and_fragment_do_not_break_the_url() {
        let p = url_preview(Protocol::OpenAiChat, "https://x.example.com/?a=1", None);
        assert_eq!(p.url, "https://x.example.com/v1/chat/completions?a=1");
        assert_eq!(p.warning.as_deref(), Some(URL_WARN_QUERY_KEPT));
        assert_eq!(
            endpoint_url("https://x.example.com/v1?a=1", Protocol::Gemini, Endpoint::Models, None),
            "https://x.example.com/v1/models?pageSize=1000&a=1"
        );
        let p = url_preview(Protocol::OpenAiChat, "https://x.example.com/v1#frag", None);
        assert_eq!(p.url, "https://x.example.com/v1/chat/completions");
        assert_eq!(p.warning.as_deref(), Some(URL_WARN_FRAGMENT_DROPPED));
        // 末尾的 # 仍是「按原样」标记
        let p = url_preview(Protocol::OpenAiChat, "https://x.example.com/openai#", None);
        assert_eq!(p.url, "https://x.example.com/openai/chat/completions");
        assert!(p.forced && p.warning.is_none());
    }

    #[test]
    fn scheme_detection_is_case_insensitive() {
        assert_eq!(
            normalize_base_url("HTTP://Host", Protocol::OpenAiChat),
            "HTTP://Host/v1"
        );
        assert_eq!(
            normalize_base_url("httpbin.org", Protocol::OpenAiChat),
            "https://httpbin.org/v1"
        );
    }
}
