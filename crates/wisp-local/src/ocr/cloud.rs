//! PaddleOCR 云端客户端：AI Studio 的异步任务接口与 serving 接口（AI Studio 部署的服务、自托管 PaddleX）。
//!
//! - **地址判别**（[`detect_api`]）：路径以 `/api/v2/ocr/jobs` 结尾 → 任务式；以 `/ocr` 或 `/layout-parsing` 结尾 → serving 式；
//!   其他 → 无法识别（界面提示）。
//! - **鉴权**：任务式 `Authorization: bearer <token>`；serving 式 `Authorization: token <token>`；Token 为空时不发（自托管）。
//! - **任务式**：手写 multipart（不为 reqwest 打开 multipart feature）：`file`（PNG / JPEG）、`model`、`optionalPayload`
//!   （JSON 字符串：关闭文档方向、矫正、文本行方向与可视化；结构化模型另关图表识别与 Markdown 图片）。提交后 300 ms 第一次
//!   查询，之后间隔 ×1.5、上限 2 s，总时限 120 s；完成后用**不带鉴权头**的请求下载 `jsonUrl`（JSONL，先设 32 MB 上限）。
//!   取消时停止轮询（服务端没有撤销接口）。字段类型宽松（数字可能是字符串）。
//! - **serving 式**：JSON `{file: <base64>, fileType: 1, …同样的可选项}`，响应 `{errorCode, errorMsg, result}`（`errorCode`
//!   可能是字符串）。
//! - **结果**：`ocrResults[].prunedResult.{rec_texts, rec_scores, rec_polys}` → [`CloudOutput::Lines`]；
//!   `layoutParsingResults[].{markdown.text, prunedResult.parsing_res_list}` → [`CloudOutput::Document`]。
//! - **错误**：先看响应体的 `code` / `errorCode`，再看 HTTP 状态：401 / 403 且是 Token 类错误 → `auth`；日额度（12001，或 429
//!   带额度字样）→ `quota`；429 → `rate_limited`；任务失败 → `upstream`；超时 → `timeout`；连不上 → `network`。错误信息只用
//!   固定词汇，不回显服务端原文。
//! - **日志**：只记 host 与固定的路径类别（`jobs` / `serving`）、状态码、耗时；不记 jobId、jsonUrl、Token。
//! - **连接测试**（[`CloudClient::test`]）：任务式 `GET <jobs>/wisp-connection-test`，401 / 403 → Token 无效，404 / 400 → 鉴权通过；
//!   serving 式提交一张 32×32 的白图，`errorCode == 0` 或「没有文字」即可。
//!
//! 客户端在第一次识别时创建，空闲时随调用方丢弃（平时不存在）。

mod parse;

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use wisp_types::{ErrorCode, LlmError, OcrMode};

use super::TextLine;
use crate::error::LocalError;

/// 默认的任务式地址
pub const DEFAULT_JOBS_URL: &str = "https://paddleocr.aistudio-app.com/api/v2/ocr/jobs";
/// 第一次查询任务状态的延迟
pub const FIRST_POLL: Duration = Duration::from_millis(300);
/// 查询间隔的上限
pub const MAX_POLL: Duration = Duration::from_secs(2);
/// 一个任务的总时限
pub const JOB_TIMEOUT: Duration = Duration::from_secs(120);
/// 结果 JSONL 的大小上限
pub const MAX_RESULT_BYTES: usize = 32 * 1024 * 1024;
/// 上传文件的大小上限（服务端限制本地上传 ≤ 50 MB）
pub const MAX_UPLOAD_BYTES: usize = 50 * 1024 * 1024;

/// 接口形态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CloudApi {
    /// AI Studio 的异步任务接口
    Jobs,
    /// 同步 serving 接口（`/ocr`：文字识别；`/layout-parsing`：文档解析）
    Serving,
}

/// 按地址判别接口形态；无法识别时为 None
pub fn detect_api(url: &str) -> Option<CloudApi> {
    let path = url::Url::parse(url.trim())
        .ok()?
        .path()
        .trim_end_matches('/')
        .to_ascii_lowercase();
    if path.ends_with("/api/v2/ocr/jobs") {
        Some(CloudApi::Jobs)
    } else if path.ends_with("/ocr") || path.ends_with("/layout-parsing") {
        Some(CloudApi::Serving)
    } else {
        None
    }
}

/// 模型是否为文档解析类（输出 Markdown；PP-StructureV3、PaddleOCR-VL 系列）。其余（PP-OCRv6 / v5）只输出文本行
pub fn is_document_model(model: &str) -> bool {
    let m = model.trim().to_ascii_lowercase();
    m.starts_with("pp-structure") || m.starts_with("paddleocr-vl")
}

/// 访问令牌（`Debug` 不输出内容）
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// 包装令牌；空白令牌视为没有（自托管服务不鉴权）
    pub fn new(token: String) -> Option<Self> {
        let t = token.trim();
        (!t.is_empty()).then(|| Self(t.to_owned()))
    }

    /// 令牌内容（只用于请求头）
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(***)")
    }
}

/// 一次识别请求（`Debug` 不输出服务地址、Token 与图片内容）
#[derive(Clone)]
pub struct CloudRequest {
    /// 服务地址（用户填写的完整接口地址）
    pub endpoint: String,
    pub token: Option<Token>,
    /// 模型名（`PP-OCRv6`、`PP-StructureV3`、`PaddleOCR-VL-1.6`……）
    pub model: String,
    /// 编码好的图片（PNG 或 JPEG）
    pub image: Arc<Vec<u8>>,
    pub mime: &'static str,
}

impl std::fmt::Debug for CloudRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudRequest")
            .field("model", &self.model)
            .field("mime", &self.mime)
            .field("image_bytes", &self.image.len())
            .field("token", &self.token.is_some())
            .finish_non_exhaustive()
    }
}

/// 识别结果
#[derive(Debug, Clone, PartialEq)]
pub enum CloudOutput {
    /// 文本行（OCR 类模型）
    Lines(Vec<TextLine>),
    /// 文档解析结果：服务给出的 Markdown，以及按阅读顺序的文本块（lines / paragraphs 模式用）
    Document { markdown: String, blocks: Vec<String> },
}

impl CloudOutput {
    /// 按 OCR 模式取文本（文本行经 [`super::assemble`] 拼装；文档在 markdown 模式用 Markdown，其余用文本块）
    pub fn text(&self, mode: OcrMode) -> String {
        match self {
            Self::Lines(lines) => super::assemble::assemble(lines, mode),
            Self::Document { markdown, blocks } => match mode {
                OcrMode::Markdown => markdown.clone(),
                OcrMode::Lines | OcrMode::Paragraphs | OcrMode::Translate => blocks.join("\n\n"),
            },
        }
    }
}

/// 云端客户端。内部的 reqwest 客户端按地址的种类在第一次请求时创建：公网服务读系统代理（与 wisp-llm 一致）；本机与
/// 局域网的自托管服务（[`is_local_host`]）直连——macOS 上 reqwest 读系统代理时不读「例外」列表，不直连的话截图与 Token
/// 会被送进代理，远端代理也根本连不上局域网地址
pub struct CloudClient {
    proxied: std::sync::OnceLock<reqwest::Client>,
    direct: std::sync::OnceLock<reqwest::Client>,
}

impl std::fmt::Debug for CloudClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CloudClient")
    }
}

/// 提交任务、下载结果的单次请求时限
const SUBMIT_TIMEOUT: Duration = Duration::from_secs(30);
/// 查询任务状态的单次请求时限
const POLL_TIMEOUT: Duration = Duration::from_secs(15);
/// 连接测试的时限
const TEST_TIMEOUT: Duration = Duration::from_secs(20);
/// 查询任务状态时允许连续失败（网络抖动、5xx、限流）的次数
const POLL_RETRIES: u32 = 3;
/// 下载结果时最多跟随的重定向次数（结果地址是对象存储的预签名链接，通常直接 200）
const MAX_REDIRECTS: usize = 3;

impl CloudClient {
    /// 创建客户端（不做 I/O；真正的 HTTP 客户端在第一次请求时按地址创建，不自动跟随重定向——鉴权头不能被带到别的 host）
    pub fn new() -> Result<Self, LocalError> {
        Ok(Self {
            proxied: std::sync::OnceLock::new(),
            direct: std::sync::OnceLock::new(),
        })
    }

    /// 访问 `url` 用的客户端：本机与局域网地址直连，其余读系统代理
    fn http(&self, url: &url::Url) -> Result<&reqwest::Client, LlmError> {
        let direct = url.host().is_some_and(|h| is_local_host(&h));
        let cell = if direct { &self.direct } else { &self.proxied };
        if let Some(c) = cell.get() {
            return Ok(c);
        }
        let builder = crate::net::client_builder().redirect(reqwest::redirect::Policy::none());
        let builder = if direct { builder.no_proxy() } else { builder };
        let client = builder
            .build()
            .map_err(|_| error(ErrorCode::Internal, "cannot create the HTTP client", None))?;
        Ok(cell.get_or_init(|| client))
    }

    /// 识别一张图片。`on_queued` 在任务提交成功、开始等待结果时调用一次（编排器据此发 `Phase{queued}`）
    pub async fn recognize(
        &self,
        req: CloudRequest,
        on_queued: &(dyn Fn() + Send + Sync),
        cancel: CancellationToken,
    ) -> Result<CloudOutput, LlmError> {
        let endpoint = Endpoint::parse(&req.endpoint)?;
        if req.image.len() > MAX_UPLOAD_BYTES {
            return Err(error(
                ErrorCode::ImageTooLarge,
                "the image is larger than the service accepts",
                None,
            ));
        }
        let t0 = Instant::now();
        let out = tokio::select! {
            () = cancel.cancelled() => Err(cancelled()),
            r = async {
                match endpoint.api {
                    CloudApi::Jobs => self.run_job(&endpoint, &req, on_queued, &cancel).await,
                    CloudApi::Serving => self.run_serving(&endpoint, &req).await,
                }
            } => r,
        };
        log::info!(
            "paddleocr: host={} kind={} ok={} elapsed_ms={}",
            endpoint.host(),
            endpoint.kind(),
            out.is_ok(),
            t0.elapsed().as_millis()
        );
        out
    }

    /// 连接测试（服务商页的「测试」）：只验证地址与 Token，不消耗识别额度
    pub async fn test(&self, endpoint: &str, token: Option<Token>) -> Result<(), LlmError> {
        let endpoint = Endpoint::parse(endpoint)?;
        match endpoint.api {
            CloudApi::Jobs => {
                // 查一个不存在的任务：鉴权在前，Token 不对时 401 / 403；对了则是「任务不存在」（404 / 400）
                let url = endpoint.child("wisp-connection-test")?;
                let resp = self
                    .with_auth(self.http(&url)?.get(url), &endpoint, token.as_ref())
                    .timeout(TEST_TIMEOUT)
                    .send()
                    .await
                    .map_err(network_error)?;
                let status = resp.status().as_u16();
                let body = read_json(resp, MAX_ENVELOPE_BYTES).await.unwrap_or(Value::Null);
                let code = parse::int(body.get("code"));
                log::info!("paddleocr: host={} kind=jobs test status={status}", endpoint.host());
                match (status, code) {
                    (401 | 403, _) | (_, Some(401)) => Err(classify(status, code, &body)),
                    (200..=299 | 400 | 404, _) => Ok(()),
                    _ => Err(classify(status, code, &body)),
                }
            }
            CloudApi::Serving => {
                // 一张 32 × 32 的白图：识别结果为空也算通过
                let png =
                    blank_png().ok_or_else(|| error(ErrorCode::Internal, "cannot encode the test image", None))?;
                let body = serving_body(&png, endpoint.document);
                let resp = self
                    .with_auth(
                        self.http(&endpoint.url)?.post(endpoint.url.clone()),
                        &endpoint,
                        token.as_ref(),
                    )
                    .timeout(TEST_TIMEOUT)
                    .json(&body)
                    .send()
                    .await
                    .map_err(network_error)?;
                let status = resp.status().as_u16();
                let body = read_json(resp, MAX_RESULT_BYTES).await.unwrap_or(Value::Null);
                let code = parse::int(body.get("errorCode"));
                log::info!("paddleocr: host={} kind=serving test status={status}", endpoint.host());
                if (200..300).contains(&status) && code.unwrap_or(0) == 0 {
                    Ok(())
                } else {
                    Err(classify(status, code, &body))
                }
            }
        }
    }

    /// 任务式：提交 → 轮询 → 下载 JSONL
    async fn run_job(
        &self,
        endpoint: &Endpoint,
        req: &CloudRequest,
        on_queued: &(dyn Fn() + Send + Sync),
        cancel: &CancellationToken,
    ) -> Result<CloudOutput, LlmError> {
        let document = is_document_model(&req.model);
        let (content_type, body) = multipart(req, document)?;
        let resp = self
            .with_auth(
                self.http(&endpoint.url)?.post(endpoint.url.clone()),
                endpoint,
                req.token.as_ref(),
            )
            .timeout(SUBMIT_TIMEOUT)
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(body)
            .send()
            .await
            .map_err(network_error)?;
        let status = resp.status().as_u16();
        let envelope = read_json(resp, MAX_ENVELOPE_BYTES).await.unwrap_or(Value::Null);
        let code = parse::int(envelope.get("code"));
        if !(200..300).contains(&status) || code.unwrap_or(0) != 0 {
            return Err(classify(status, code, &envelope));
        }
        let job = parse::string(envelope.pointer("/data/jobId"))
            .filter(|j| !j.is_empty())
            .ok_or_else(|| error(ErrorCode::Protocol, "the service did not return a job id", None))?;
        on_queued();

        let deadline = Instant::now() + JOB_TIMEOUT;
        let mut delay = FIRST_POLL;
        let mut failures = 0;
        let poll_url = endpoint.child(&job)?;
        let json_url = loop {
            if Instant::now() + delay > deadline {
                return Err(error(
                    ErrorCode::Timeout,
                    "the recognition job did not finish in time",
                    Some("errors.hint.retry"),
                ));
            }
            tokio::time::sleep(delay).await;
            delay = (delay.mul_f32(1.5)).min(MAX_POLL);
            if cancel.is_cancelled() {
                return Err(cancelled());
            }
            match self.poll(endpoint, &poll_url, req.token.as_ref()).await {
                Ok(Poll::Pending) => failures = 0,
                Ok(Poll::Done(url)) => break url,
                Err(e) if e.retryable && failures < POLL_RETRIES => failures += 1,
                Err(e) => return Err(e),
            }
        };
        let lines = self.download_jsonl(&json_url).await?;
        let mut results = Vec::new();
        for line in &lines {
            let code = parse::int(line.get("errorCode"));
            if code.unwrap_or(0) != 0 {
                return Err(classify(200, code, line));
            }
            if let Some(r) = line.get("result") {
                results.push(r);
            }
        }
        parse::output(&results, document)
            .ok_or_else(|| error(ErrorCode::Protocol, "the service returned an unrecognized result", None))
    }

    /// 查询一次任务状态
    async fn poll(&self, endpoint: &Endpoint, url: &url::Url, token: Option<&Token>) -> Result<Poll, LlmError> {
        let resp = self
            .with_auth(self.http(url)?.get(url.clone()), endpoint, token)
            .timeout(POLL_TIMEOUT)
            .send()
            .await
            .map_err(network_error)?;
        let status = resp.status().as_u16();
        let body = read_json(resp, MAX_ENVELOPE_BYTES).await.unwrap_or(Value::Null);
        let code = parse::int(body.get("code"));
        if !(200..300).contains(&status) || code.unwrap_or(0) != 0 {
            return Err(classify(status, code, &body));
        }
        match body.pointer("/data/state").and_then(Value::as_str).unwrap_or_default() {
            "done" => body
                .pointer("/data/resultUrl/jsonUrl")
                .and_then(Value::as_str)
                .and_then(|u| url::Url::parse(u).ok())
                .filter(|u| result_scheme_ok(&endpoint.url, u))
                .map(Poll::Done)
                .ok_or_else(|| error(ErrorCode::Protocol, "the finished job has no result address", None)),
            "failed" => Err(error(ErrorCode::Upstream, "the recognition job failed", None)),
            // pending / running，或者没写状态
            _ => Ok(Poll::Pending),
        }
    }

    /// 下载结果（不带鉴权头：结果地址是对象存储的预签名链接，Token 不能发给它）
    async fn download_jsonl(&self, url: &url::Url) -> Result<Vec<Value>, LlmError> {
        let mut url = url.clone();
        let mut hops = 0;
        let resp = loop {
            let resp = self
                .http(&url)?
                .get(url.clone())
                .timeout(SUBMIT_TIMEOUT)
                .send()
                .await
                .map_err(network_error)?;
            if !resp.status().is_redirection() || hops >= MAX_REDIRECTS {
                break resp;
            }
            let next = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|l| url.join(l).ok())
                .filter(|u| result_scheme_ok(&url, u))
                .ok_or_else(|| error(ErrorCode::Protocol, "the result address redirected unexpectedly", None))?;
            url = next;
            hops += 1;
        };
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(classify(status, None, &Value::Null));
        }
        let bytes = read_limited(resp, MAX_RESULT_BYTES).await?;
        let text = String::from_utf8_lossy(&bytes);
        let mut lines = Vec::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            // 坏行直接报错，不给出部分结果
            let v: Value = serde_json::from_str(line)
                .map_err(|_| error(ErrorCode::Protocol, "the service returned a malformed result", None))?;
            lines.push(v);
        }
        if lines.is_empty() {
            return Err(error(ErrorCode::Protocol, "the service returned an empty result", None));
        }
        Ok(lines)
    }

    /// serving 式：一次 JSON 请求
    async fn run_serving(&self, endpoint: &Endpoint, req: &CloudRequest) -> Result<CloudOutput, LlmError> {
        let body = serving_body(&req.image, endpoint.document);
        let resp = self
            .with_auth(
                self.http(&endpoint.url)?.post(endpoint.url.clone()),
                endpoint,
                req.token.as_ref(),
            )
            .timeout(JOB_TIMEOUT)
            .json(&body)
            .send()
            .await
            .map_err(network_error)?;
        let status = resp.status().as_u16();
        let body = read_json(resp, MAX_RESULT_BYTES).await.unwrap_or(Value::Null);
        let code = parse::int(body.get("errorCode"));
        if !(200..300).contains(&status) || code.unwrap_or(0) != 0 {
            return Err(classify(status, code, &body));
        }
        let result = body
            .get("result")
            .ok_or_else(|| error(ErrorCode::Protocol, "the service returned no result", None))?;
        parse::output(&[result], endpoint.document)
            .ok_or_else(|| error(ErrorCode::Protocol, "the service returned an unrecognized result", None))
    }

    /// 加鉴权头：任务式 `bearer`，serving 式 `token`；没有 Token 时不加
    fn with_auth(
        &self,
        rb: reqwest::RequestBuilder,
        endpoint: &Endpoint,
        token: Option<&Token>,
    ) -> reqwest::RequestBuilder {
        match token {
            Some(t) => {
                let scheme = match endpoint.api {
                    CloudApi::Jobs => "bearer",
                    CloudApi::Serving => "token",
                };
                rb.header(reqwest::header::AUTHORIZATION, format!("{scheme} {}", t.expose()))
            }
            None => rb,
        }
    }
}

/// 一次轮询的结果
enum Poll {
    Pending,
    Done(url::Url),
}

/// 状态响应、提交响应这类小 JSON 的大小上限
const MAX_ENVELOPE_BYTES: usize = 1024 * 1024;

/// 用户填写的服务地址
struct Endpoint {
    url: url::Url,
    api: CloudApi,
    /// serving 式的文档解析接口（`/layout-parsing`）；任务式按模型名判断
    document: bool,
}

impl Endpoint {
    fn parse(raw: &str) -> Result<Self, LlmError> {
        let bad = || {
            error(
                ErrorCode::NotConfigured,
                "unrecognized PaddleOCR service address",
                Some("errors.hint.check_settings"),
            )
        };
        let api = detect_api(raw).ok_or_else(bad)?;
        let mut url = url::Url::parse(raw.trim()).map_err(|_| bad())?;
        url.set_query(None);
        url.set_fragment(None);
        let trimmed = url.path().trim_end_matches('/').to_owned();
        url.set_path(&trimmed);
        // http 只允许本机与局域网（自托管的 PaddleX）；公网地址必须 https，否则 Token 与图片会明文传输
        let host_ok = url.host().is_some_and(|h| is_local_host(&h));
        match url.scheme() {
            "https" => {}
            "http" if host_ok => {}
            _ => {
                return Err(error(
                    ErrorCode::NotConfigured,
                    "the service address must use https",
                    Some("errors.hint.check_settings"),
                ));
            }
        }
        let document = api == CloudApi::Serving && url.path().to_ascii_lowercase().ends_with("/layout-parsing");
        Ok(Self { url, api, document })
    }

    /// `<地址>/<段>`
    fn child(&self, segment: &str) -> Result<url::Url, LlmError> {
        let mut u = self.url.clone();
        u.path_segments_mut()
            .map_err(|_| error(ErrorCode::NotConfigured, "unrecognized PaddleOCR service address", None))?
            .push(segment);
        Ok(u)
    }

    /// 日志里只写 host
    fn host(&self) -> String {
        self.url.host_str().unwrap_or("-").to_owned()
    }

    fn kind(&self) -> &'static str {
        match self.api {
            CloudApi::Jobs => "jobs",
            CloudApi::Serving => "serving",
        }
    }
}

/// 本机或局域网的地址：回环、私有网段（IPv4 的 10/8、172.16/12、192.168/16，IPv6 的 fc00::/7）、运营商级 NAT 的
/// 100.64/10（Tailscale 等组网工具给设备分配的就是这一段）、链路本地、`localhost`、mDNS 的 `.local`、家庭网络的
/// `.home.arpa` / `.lan` / `.internal`，以及不带点的单段主机名（`http://nas:8080`，只能在局域网里解析）。这类地址允许 http
/// （自托管的 PaddleX 通常没有证书），并且直连、不走系统代理
pub fn is_local_host(host: &url::Host<&str>) -> bool {
    match host {
        url::Host::Ipv4(ip) => {
            let [a, b, ..] = ip.octets();
            let shared = a == 100 && (b & 0xc0) == 64;
            ip.is_loopback() || ip.is_private() || ip.is_link_local() || shared
        }
        url::Host::Ipv6(ip) => {
            let first = ip.segments()[0];
            ip.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
        url::Host::Domain(d) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            !d.is_empty()
                && (!d.contains('.')
                    || [".localhost", ".local", ".home.arpa", ".lan", ".internal"]
                        .iter()
                        .any(|suffix| d.ends_with(suffix)))
        }
    }
}

/// 结果地址（及其重定向）的 scheme 不能比服务地址低：https 的服务给出的结果只接受 https（识别出的文字不能明文传输），
/// 本机 / 局域网的 http 服务可以给 http 的结果地址
fn result_scheme_ok(service: &url::Url, result: &url::Url) -> bool {
    match result.scheme() {
        "https" => true,
        "http" => service.scheme() == "http",
        _ => false,
    }
}

/// 两种接口共用的可选项：关闭文档方向、矫正与可视化；文字识别另关文本行方向（截图里几乎没有倒置的行），
/// 文档解析另关图表与印章识别（截图里用不到，且图表识别很慢）
fn optional_payload(document: bool) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert("useDocOrientationClassify".into(), Value::Bool(false));
    m.insert("useDocUnwarping".into(), Value::Bool(false));
    m.insert("visualize".into(), Value::Bool(false));
    if document {
        m.insert("useChartRecognition".into(), Value::Bool(false));
        m.insert("useSealRecognition".into(), Value::Bool(false));
    } else {
        m.insert("useTextlineOrientation".into(), Value::Bool(false));
    }
    m
}

fn serving_body(image: &[u8], document: bool) -> Value {
    use base64::Engine;
    let mut m = optional_payload(document);
    m.insert(
        "file".into(),
        Value::String(base64::engine::general_purpose::STANDARD.encode(image)),
    );
    m.insert("fileType".into(), Value::from(1));
    Value::Object(m)
}

/// 手写的 multipart/form-data（不为这一处打开 reqwest 的 multipart feature）
fn multipart(req: &CloudRequest, document: bool) -> Result<(String, Vec<u8>), LlmError> {
    let boundary = boundary()?;
    let payload = Value::Object(optional_payload(document)).to_string();
    let filename = if req.mime == "image/jpeg" {
        "capture.jpg"
    } else {
        "capture.png"
    };
    let mut body = Vec::with_capacity(req.image.len() + 1024);
    let mut text_part = |name: &str, value: &str| {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    };
    text_part("model", req.model.trim());
    text_part("optionalPayload", &payload);
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {}\r\n\r\n",
            req.mime
        )
        .as_bytes(),
    );
    body.extend_from_slice(&req.image);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    Ok((format!("multipart/form-data; boundary={boundary}"), body))
}

/// 随机的 multipart 分隔符
fn boundary() -> Result<String, LlmError> {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| error(ErrorCode::Internal, "no randomness available", None))?;
    Ok(format!(
        "----wisp{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ))
}

/// 32 × 32 的白色 PNG（连接测试用）
fn blank_png() -> Option<Vec<u8>> {
    use image::ImageEncoder;
    let mut out = Vec::new();
    let pixels = vec![255u8; 32 * 32 * 3];
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&pixels, 32, 32, image::ExtendedColorType::Rgb8)
        .ok()?;
    Some(out)
}

/// 读响应体，超过上限即报错
async fn read_limited(mut resp: reqwest::Response, limit: usize) -> Result<Vec<u8>, LlmError> {
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(network_error)? {
        if out.len() + chunk.len() > limit {
            return Err(error(ErrorCode::Protocol, "the service response is too large", None));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// 读 JSON 响应体（读不出或不是 JSON 时为 None：调用方按 HTTP 状态归类）
async fn read_json(resp: reqwest::Response, limit: usize) -> Option<Value> {
    let bytes = read_limited(resp, limit).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 服务端错误的归类：先看响应体里的错误码，再看 HTTP 状态。只用固定的英文简述，不回显服务端原文
fn classify(status: u16, code: Option<i64>, body: &Value) -> LlmError {
    let quota_words = |b: &Value| {
        let msg = ["msg", "errorMsg", "message"]
            .iter()
            .filter_map(|k| b.get(*k).and_then(Value::as_str))
            .collect::<String>()
            .to_lowercase();
        ["额度", "页数", "上限", "quota", "daily", "limit exceeded"]
            .iter()
            .any(|w| msg.contains(w))
    };
    let status_opt = (!(200..300).contains(&status)).then_some(status);
    let mut e = match code.filter(|c| *c != 0 && *c != i64::from(status)) {
        Some(12001) => error(ErrorCode::Quota, "the daily page quota of the service is used up", None),
        Some(12002 | 10010) => error(
            ErrorCode::RateLimited,
            "the service is busy",
            Some("errors.hint.retry_later"),
        ),
        Some(401) => error(
            ErrorCode::Auth,
            "the access token was rejected",
            Some("errors.hint.check_key"),
        ),
        Some(10007) => error(
            ErrorCode::NotFound,
            "the service does not know this model",
            Some("errors.hint.check_model"),
        ),
        Some(10003) => error(
            ErrorCode::ImageTooLarge,
            "the image is larger than the service accepts",
            Some("errors.hint.shrink_image"),
        ),
        Some(11001..=11003) => error(
            ErrorCode::Upstream,
            "the recognition job failed",
            Some("errors.hint.retry"),
        ),
        Some(10001..=10009) => error(ErrorCode::Protocol, "the service rejected the request", None),
        Some(500..=599) => error(
            ErrorCode::Upstream,
            "the service had an internal error",
            Some("errors.hint.retry"),
        ),
        _ => by_status(status, quota_words(body)),
    };
    if matches!(
        e.code,
        ErrorCode::RateLimited | ErrorCode::Upstream | ErrorCode::Timeout | ErrorCode::Network
    ) {
        e.retryable = true;
    }
    e.status = status_opt;
    e
}

fn by_status(status: u16, quota: bool) -> LlmError {
    match status {
        401 | 403 => error(
            ErrorCode::Auth,
            "the access token was rejected",
            Some("errors.hint.check_key"),
        ),
        429 if quota => error(ErrorCode::Quota, "the daily page quota of the service is used up", None),
        429 => error(
            ErrorCode::RateLimited,
            "the service is busy",
            Some("errors.hint.retry_later"),
        ),
        413 => error(
            ErrorCode::ImageTooLarge,
            "the image is larger than the service accepts",
            Some("errors.hint.shrink_image"),
        ),
        404 => error(
            ErrorCode::NotFound,
            "the service address was not found",
            Some("errors.hint.check_settings"),
        ),
        408 | 504 => error(ErrorCode::Timeout, "the service timed out", Some("errors.hint.retry")),
        500..=599 => error(
            ErrorCode::Upstream,
            "the service had an internal error",
            Some("errors.hint.retry"),
        ),
        200..=299 => error(ErrorCode::Protocol, "the service returned an error", None),
        _ => error(ErrorCode::Protocol, "the service rejected the request", None),
    }
}

fn network_error(e: reqwest::Error) -> LlmError {
    let mut err = if e.is_timeout() {
        error(
            ErrorCode::Timeout,
            "the service did not respond in time",
            Some("errors.hint.retry"),
        )
    } else {
        error(
            ErrorCode::Network,
            "cannot reach the PaddleOCR service",
            Some("errors.hint.check_network"),
        )
    };
    err.retryable = true;
    err
}

fn error(code: ErrorCode, message: &str, hint: Option<&str>) -> LlmError {
    let mut e = LlmError::new(code, message);
    e.hint = hint.map(str::to_owned);
    e
}

fn cancelled() -> LlmError {
    LocalError::Cancelled.to_llm_error()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_detection() {
        assert_eq!(detect_api(DEFAULT_JOBS_URL), Some(CloudApi::Jobs));
        assert_eq!(
            detect_api("https://abc123.aistudio-app.com/ocr"),
            Some(CloudApi::Serving)
        );
        assert_eq!(
            detect_api("http://192.168.1.5:8080/layout-parsing/"),
            Some(CloudApi::Serving)
        );
        assert_eq!(detect_api("https://example.com/v1"), None);
        assert_eq!(detect_api("not a url"), None);
    }

    #[test]
    fn document_models() {
        assert!(is_document_model("PP-StructureV3"));
        assert!(is_document_model("PaddleOCR-VL-1.6"));
        assert!(!is_document_model("PP-OCRv6"));
    }

    #[test]
    fn local_hosts_allow_http_and_go_direct() {
        let local = |u: &str| {
            url::Url::parse(u)
                .expect("url")
                .host()
                .is_some_and(|h| is_local_host(&h))
        };
        for u in [
            "http://127.0.0.1:8080/ocr",
            "http://[::1]:8080/ocr",
            "http://localhost/ocr",
            "http://192.168.1.5/ocr",
            "http://10.0.0.2/ocr",
            "http://172.20.1.1/ocr",
            "http://169.254.3.4/ocr",
            "http://[fd12::1]/ocr",
            "http://[fe80::1]/ocr",
            "http://nas:8080/ocr",
            "http://NAS./ocr",
            "http://paddle.local/ocr",
            "http://box.home.arpa/ocr",
            "http://ocr.lan/ocr",
            "http://100.64.0.1/ocr",
            "http://100.101.102.103:8080/ocr",
            "http://100.127.255.254/ocr",
        ] {
            assert!(local(u), "{u}");
        }
        for u in [
            "https://paddleocr.aistudio-app.com/api/v2/ocr/jobs",
            "http://8.8.8.8/ocr",
            "http://100.63.255.255/ocr",
            "http://100.128.0.1/ocr",
            "http://172.32.0.1/ocr",
            "http://example.com/ocr",
            "http://[2001:db8::1]/ocr",
        ] {
            assert!(!local(u), "{u}");
        }
    }

    #[test]
    fn endpoints_need_https_unless_local() {
        assert!(Endpoint::parse("http://nas:8080/ocr").is_ok(), "单段主机名是局域网地址");
        assert!(Endpoint::parse("http://192.168.1.5:8080/layout-parsing").is_ok());
        assert!(Endpoint::parse("https://abc.aistudio-app.com/ocr").is_ok());
        let e = Endpoint::parse("http://ocr.example.com/ocr")
            .err()
            .expect("public http");
        assert_eq!(e.code, ErrorCode::NotConfigured);
    }

    #[test]
    fn result_addresses_keep_the_service_scheme() {
        let u = |s: &str| url::Url::parse(s).expect("url");
        let https = u("https://paddleocr.aistudio-app.com/api/v2/ocr/jobs");
        let http = u("http://192.168.1.5:8080/api/v2/ocr/jobs");
        assert!(result_scheme_ok(&https, &u("https://bucket.example.com/r.jsonl")));
        assert!(!result_scheme_ok(&https, &u("http://bucket.example.com/r.jsonl")));
        assert!(result_scheme_ok(&http, &u("http://192.168.1.5:8080/r.jsonl")));
        assert!(!result_scheme_ok(&https, &u("ftp://bucket.example.com/r.jsonl")));
    }

    #[test]
    fn requests_do_not_print_the_address_or_the_image() {
        let req = CloudRequest {
            endpoint: "https://secret-host.example.com/ocr?k=v".to_owned(),
            token: Token::new("secret-token".to_owned()),
            model: "PP-OCRv6".to_owned(),
            image: Arc::new(vec![1, 2, 3]),
            mime: "image/png",
        };
        let text = format!("{req:?}");
        assert!(!text.contains("secret"), "{text}");
        assert!(text.contains("image_bytes: 3"), "{text}");
    }

    #[test]
    fn tokens_are_redacted() {
        let t = Token::new("  secret  ".to_owned()).expect("token");
        assert_eq!(t.expose(), "secret");
        assert_eq!(format!("{t:?}"), "Token(***)");
        assert!(Token::new("   ".to_owned()).is_none());
    }
}
