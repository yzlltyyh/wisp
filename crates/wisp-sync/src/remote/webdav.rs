//! WebDAV 远端（reqwest + quick-xml）。
//!
//! 客户端只为同步服务：Basic 认证、不发 Referer、按需创建（闲置后重建，借此读取最新的系统代理），
//! 局域网地址直连不走系统代理。
//!
//! 重定向由本模块逐跳处理，不交给 reqwest：reqwest 的重定向策略看不到请求方法，而它自动跟随时会把
//! 301 / 302 / 303 上的 PUT 改成 GET（返回 200，看起来像写入成功）。规则：
//! - 只有 GET 跟随，最多 3 次；跨主机只允许 https → https；服务器本身是 http 时允许同 scheme 同主机；
//! - 只在仍处于服务器自身的源（scheme + 主机 + 端口）时带 Authorization，离开后不再带回；
//! - 其他方法遇到 3xx 报 [`RemoteError::Redirect`]，请求体不会被重发；
//! - `http://X → https://X` 的跳转报 [`RemoteError::HttpsRequired`]（不带凭据重试，提示改用 https 地址）。
//!
//! 超时：连接 10 s，单次读取空闲 30 s，小请求 30 s，按流传输的文件 30 s + 字节数 ÷ 64 KiB/s。
//! 连接池空闲 30 s 释放。reqwest 的错误先 `.without_url()` 再折叠成 [`RemoteError`] 的固定词汇，
//! 日志与错误里不出现地址、账号、路径与签名过的 CDN 链接。

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use parking_lot::Mutex;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use quick_xml::events::Event;
use reqwest::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, ETAG, HeaderValue, LOCATION};
use reqwest::{Method, Response, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::Instant;
use url::{Host, Url};

use super::{Entry, PutInfo, ROOT_DIR, Remote};
use crate::error::{RemoteError, ServerUrlError, SyncError};

/// 服务器地址的长度上限（字节）
pub const MAX_URL_BYTES: usize = 1024;

/// 建立连接（含 TLS 握手）的上限
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 小请求（PROPFIND / MKCOL / DELETE）的总时长上限，也是任何一次读取的空闲上限
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// 按流传输时假设的最低速率：文件请求的时限在 [`REQUEST_TIMEOUT`] 之上按字节数放宽
const MIN_TRANSFER_BYTES_PER_SEC: u64 = 64 * 1024;
/// 空闲连接在连接池里保留的时长；之后连接与连接池的计时器都释放，同步闲着时不占资源
const POOL_IDLE: Duration = Duration::from_secs(30);
/// 缓存的 Client 闲置超过这个时长就在下次取用时重建：连接池此时已空，重建不损失复用，
/// 还能读到期间变化的系统代理（reqwest 只在构建时读取）
const CLIENT_IDLE_REBUILD: Duration = POOL_IDLE;
/// GET 最多跟随的重定向次数
const MAX_REDIRECTS: usize = 3;
/// PROPFIND 响应的上限（同步目录里只有少量文件，远小于此）
const MAX_LISTING_BYTES: u64 = 16 << 20;
/// 整块读入内存的文件（vault.json、head、日志段）的上限：明文上限加上封装开销的余量
const MAX_GET_BYTES: u64 = crate::model::MAX_PLAINTEXT_BYTES as u64 + (1 << 20);
/// 按流写到本地的文件（blob）的上限：挡住异常的服务器或 CDN 把磁盘写满
const MAX_FILE_BYTES: u64 = 256 << 20;
/// 失败响应的响应体最多读这么多再丢弃（读完的连接才能放回连接池复用）
const DRAIN_LIMIT: usize = 64 * 1024;
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
/// 上传时每次从文件读出的块大小
const UPLOAD_CHUNK_BYTES: usize = 64 * 1024;

const USER_AGENT: &str = concat!("Wisp/", env!("CARGO_PKG_VERSION"));

/// 路径段的百分号编码：只保留 RFC 3986 的非保留字符，其余一律编码
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

/// 只请求用得到的四个属性：有的服务器对 allprop 很慢，或者会附带大量无关属性
const PROPFIND_BODY: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<d:propfind xmlns:d="DAV:"><d:prop>"#,
    r#"<d:resourcetype/><d:getetag/><d:getcontentlength/><d:getlastmodified/>"#,
    r#"</d:prop></d:propfind>"#,
);

/// 校验用户填写的服务器地址：
/// - 不超过 [`MAX_URL_BYTES`]；只允许 http / https；不得带账号密码、query 与 fragment；
/// - `http://` 只允许回环 / 私有 / 链路本地的 IP 字面量、`.local` 与单标签主机名（局域网 NAS），
///   其他主机 ⇒ [`ServerUrlError::HttpsRequired`]（Basic 认证在明文 http 上会泄露应用密码）。
///
/// 返回规范化后的地址：scheme 与主机小写、去掉默认端口、路径以 `/` 结尾，
/// 所以 `https://Example.com/dav` 与 `https://example.com/dav/` 是同一个账号
pub fn validate_server_url(raw: &str) -> Result<Url, ServerUrlError> {
    if raw.len() > MAX_URL_BYTES {
        return Err(ServerUrlError::TooLong);
    }
    let mut url = Url::parse(raw).map_err(|_| ServerUrlError::Invalid)?;
    let https = match url.scheme() {
        "https" => true,
        "http" => false,
        _ => return Err(ServerUrlError::Scheme),
    };
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ServerUrlError::UserInfo);
    }
    if url.query().is_some() {
        return Err(ServerUrlError::Query);
    }
    if url.fragment().is_some() {
        return Err(ServerUrlError::Fragment);
    }
    let host = url.host().ok_or(ServerUrlError::Invalid)?;
    if !https && !is_local_host(&host) {
        return Err(ServerUrlError::HttpsRequired);
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    if url.as_str().len() > MAX_URL_BYTES {
        return Err(ServerUrlError::TooLong);
    }
    Ok(url)
}

fn is_local_host(host: &Host<&str>) -> bool {
    match host {
        Host::Ipv4(ip) => is_local_v4(ip),
        Host::Ipv6(ip) => is_local_v6(ip),
        Host::Domain(domain) => {
            let domain = domain.strip_suffix('.').unwrap_or(domain);
            !domain.is_empty() && (!domain.contains('.') || domain.ends_with(".local"))
        }
    }
}

fn is_local_v4(ip: &Ipv4Addr) -> bool {
    ip.is_loopback() || ip.is_private() || ip.is_link_local()
}

fn is_local_v6(ip: &Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_local_v4(&v4);
    }
    ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local()
}

/// 按流传输的时限：30 s + 字节数 ÷ 64 KiB/s
fn transfer_timeout(bytes: u64) -> Duration {
    REQUEST_TIMEOUT + Duration::from_secs(bytes / MIN_TRANSFER_BYTES_PER_SEC)
}

/// reqwest 用的是 rustls-no-provider：创建 Client 之前兜底安装 ring（与应用其他部分的做法相同）。
/// 宿主进程已经装了别的 provider 时保持不动
fn ensure_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

fn build_client(direct: bool) -> Result<reqwest::Client, RemoteError> {
    ensure_crypto_provider();
    let mut builder = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(REQUEST_TIMEOUT)
        .pool_idle_timeout(POOL_IDLE)
        .pool_max_idle_per_host(2)
        // 重定向逐跳手动处理（见模块说明）；自动跟随时 reqwest 还会补 Referer，这里也关掉
        .redirect(reqwest::redirect::Policy::none())
        .referer(false)
        .user_agent(USER_AGENT);
    if direct {
        builder = builder.no_proxy();
    }
    builder
        .build()
        .map_err(|_| RemoteError::Internal("failed to initialise the HTTP client"))
}

/// reqwest 错误 → 固定词汇。只看错误种类与错误链里的 `io::ErrorKind` / 关键字，
/// 从不保留错误本身的 Display（含 URL；DNS 错误可能带主机名）
fn transport_error(err: reqwest::Error) -> RemoteError {
    let err = err.without_url();
    if err.is_builder() {
        return RemoteError::Internal("invalid request");
    }
    if err.is_redirect() {
        return RemoteError::Redirect;
    }
    let mut chain = String::new();
    let mut io_kind = None;
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&err);
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
        return RemoteError::Dns;
    }
    if chain.contains("certificate") || chain.contains("unknownissuer") {
        return RemoteError::Tls;
    }
    if err.is_timeout() || io_kind == Some(K::TimedOut) {
        return RemoteError::Timeout;
    }
    if chain.contains("tls") || chain.contains("handshake") {
        return RemoteError::Tls;
    }
    RemoteError::Connect
}

/// 同源：scheme、主机与端口都相同
fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme() && a.host_str() == b.host_str() && a.port_or_known_default() == b.port_or_known_default()
}

fn location(resp: &Response) -> Option<&str> {
    resp.headers().get(LOCATION).and_then(|v| v.to_str().ok())
}

/// 不跟随的 3xx 报哪种错误：`http://X → https://X` 报 `HttpsRequired`（提示改用 https 地址），其余报 `Redirect`
fn redirect_refusal(from: &Url, location: Option<&str>) -> RemoteError {
    let to = location.and_then(|loc| from.join(loc.trim()).ok());
    match to {
        Some(to) if from.scheme() == "http" && to.scheme() == "https" && from.host_str() == to.host_str() => {
            RemoteError::HttpsRequired
        }
        _ => RemoteError::Redirect,
    }
}

/// GET 的 3xx 能否跟随：跨主机只允许 https → https；同 scheme 同主机总是可以（服务器本身是 http 时只有这种）；
/// 降级到 http、换成别的 scheme、地址里带账号密码的一律不跟随
fn redirect_target(from: &Url, location: Option<&str>) -> Result<Url, RemoteError> {
    let refused = || redirect_refusal(from, location);
    let to = location
        .and_then(|loc| from.join(loc.trim()).ok())
        .ok_or_else(refused)?;
    if !matches!(to.scheme(), "http" | "https") || !to.username().is_empty() || to.password().is_some() {
        return Err(refused());
    }
    let both_https = from.scheme() == "https" && to.scheme() == "https";
    let same_scheme_and_host = from.scheme() == to.scheme() && from.host_str() == to.host_str();
    if both_https || same_scheme_and_host {
        Ok(to)
    } else {
        Err(refused())
    }
}

/// ETag 规范化成不透明的比较值：去掉 `W/` 前缀与引号。PUT 响应头与 PROPFIND 的 `getetag`
/// 在有的服务器上一个带引号一个不带，规范化后才能直接比较
fn normalize_etag(raw: &str) -> Option<String> {
    let s = raw.trim();
    let s = s.strip_prefix("W/").unwrap_or(s);
    let s = s.strip_prefix('"').and_then(|t| t.strip_suffix('"')).unwrap_or(s);
    (!s.is_empty()).then(|| s.to_string())
}

fn etag_header(resp: &Response) -> Option<String> {
    resp.headers()
        .get(ETAG)
        .and_then(|v| v.to_str().ok())
        .and_then(normalize_etag)
}

/// 非成功状态 → 错误（3xx 已在调用前处理）
fn status_error(status: StatusCode) -> RemoteError {
    RemoteError::Status(status.as_u16())
}

/// 读掉并丢弃一小段响应体，让连接能放回连接池；读不完就算了（丢弃时连接被关闭）
async fn drain(mut resp: Response) {
    let _ = tokio::time::timeout(DRAIN_TIMEOUT, async {
        let mut read = 0usize;
        while let Ok(Some(chunk)) = resp.chunk().await {
            read += chunk.len();
            if read > DRAIN_LIMIT {
                break;
            }
        }
    })
    .await;
}

/// 按块读响应体。超过 `limit` ⇒ [`RemoteError::Protocol`]；总时限按声明的长度放宽
/// （未声明时按已收到的字节数逐步放宽，相当于要求平均速率不低于 64 KiB/s）；
/// 单次读取的空闲上限由 Client 的 read_timeout 保证
struct BodyReader {
    started: Instant,
    declared: Option<u64>,
    received: u64,
    limit: u64,
}

impl BodyReader {
    fn new(resp: &Response, limit: u64) -> Result<BodyReader, RemoteError> {
        let declared = resp.content_length();
        if declared.is_some_and(|n| n > limit) {
            return Err(RemoteError::Protocol);
        }
        Ok(BodyReader {
            started: Instant::now(),
            declared,
            received: 0,
            limit,
        })
    }

    async fn next(&mut self, resp: &mut Response) -> Result<Option<Bytes>, RemoteError> {
        let allowance = self
            .declared
            .unwrap_or(0)
            .max(self.received + UPLOAD_CHUNK_BYTES as u64);
        let deadline = self.started + transfer_timeout(allowance);
        let chunk = match tokio::time::timeout_at(deadline, resp.chunk()).await {
            Err(_) => return Err(RemoteError::Timeout),
            Ok(Err(e)) => return Err(transport_error(e)),
            Ok(Ok(chunk)) => chunk,
        };
        if let Some(chunk) = &chunk {
            self.received += chunk.len() as u64;
            if self.received > self.limit {
                return Err(RemoteError::Protocol);
            }
        }
        Ok(chunk)
    }
}

/// 整个响应体读进内存
async fn read_to_bytes(resp: &mut Response, limit: u64) -> Result<Bytes, RemoteError> {
    let mut reader = BodyReader::new(resp, limit)?;
    let mut body = BytesMut::new();
    while let Some(chunk) = reader.next(resp).await? {
        body.extend_from_slice(&chunk);
    }
    Ok(body.freeze())
}

/// 响应体按块写进本地文件
async fn read_to_file(resp: &mut Response, file: &mut tokio::fs::File, limit: u64) -> Result<(), RemoteError> {
    let mut reader = BodyReader::new(resp, limit)?;
    while let Some(chunk) = reader.next(resp).await? {
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(())
}

fn propfind_method() -> Method {
    Method::from_bytes(b"PROPFIND").expect("PROPFIND is a valid method token")
}

fn mkcol_method() -> Method {
    Method::from_bytes(b"MKCOL").expect("MKCOL is a valid method token")
}

struct CachedClient {
    client: reqwest::Client,
    last_used: std::time::Instant,
}

/// WebDAV 远端。持有凭据，`Debug` 不输出地址与账号
pub struct WebDav {
    /// 同步根目录（`<server_url>/wisp/`，以 `/` 结尾）
    base: Url,
    https: bool,
    /// 预先编码好的 `Basic …` 头（标记为敏感，reqwest 的调试输出里不显示）
    auth: HeaderValue,
    /// 局域网服务器直连，不走系统代理（代理通常到不了局域网）
    direct: bool,
    client: Mutex<Option<CachedClient>>,
}

impl fmt::Debug for WebDav {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebDav")
            .field("https", &self.https)
            .finish_non_exhaustive()
    }
}

impl WebDav {
    /// 校验地址并准备客户端；不发任何请求，也不立即创建 HTTP 客户端。根目录是地址之下的 [`ROOT_DIR`]
    pub fn new(server_url: &str, username: &str, password: &str) -> Result<WebDav, SyncError> {
        let base = validate_server_url(server_url).map_err(SyncError::ServerUrl)?;
        let base = base
            .join(&format!("{ROOT_DIR}/"))
            .map_err(|_| SyncError::ServerUrl(ServerUrlError::Invalid))?;
        let direct = base.host().is_some_and(|h| is_local_host(&h));
        Ok(WebDav {
            https: base.scheme() == "https",
            auth: basic_auth(username, password)?,
            direct,
            base,
            client: Mutex::new(None),
        })
    }

    /// 取出（必要时新建）HTTP 客户端
    fn client(&self) -> Result<reqwest::Client, RemoteError> {
        let mut cached = self.client.lock();
        let now = std::time::Instant::now();
        if let Some(c) = cached.as_mut()
            && now.saturating_duration_since(c.last_used) < CLIENT_IDLE_REBUILD
        {
            c.last_used = now;
            return Ok(c.client.clone());
        }
        let client = build_client(self.direct)?;
        *cached = Some(CachedClient {
            client: client.clone(),
            last_used: now,
        });
        Ok(client)
    }

    /// 网络类错误之后丢掉缓存的客户端：最常见的原因是系统代理变了（开关代理工具、切 VPN）
    fn note_error(&self, err: &RemoteError) {
        if matches!(
            err,
            RemoteError::Connect | RemoteError::Dns | RemoteError::Tls | RemoteError::Timeout
        ) {
            *self.client.lock() = None;
        }
    }

    /// 相对路径 → 完整地址。逐段百分号编码；`dir` 为 true（或根目录）时以 `/` 结尾——
    /// 有的服务器（如 nginx）只接受以 `/` 结尾的目录 MKCOL / DELETE
    fn url_for(&self, path: &str, dir: bool) -> Result<Url, RemoteError> {
        let mut s = self.base.as_str().to_string();
        let mut first = true;
        for segment in path.split('/').filter(|p| !p.is_empty()) {
            if segment == "." || segment == ".." {
                return Err(RemoteError::Internal("invalid remote path"));
            }
            if !first {
                s.push('/');
            }
            first = false;
            s.extend(utf8_percent_encode(segment, SEGMENT));
        }
        if dir && !first {
            s.push('/');
        }
        Url::parse(&s).map_err(|_| RemoteError::Internal("invalid remote path"))
    }

    /// 发出一个不跟随重定向的请求（带认证）；3xx ⇒ `Redirect` 或 `HttpsRequired`
    async fn send(
        &self,
        method: Method,
        url: Url,
        timeout: Duration,
        build: impl FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
    ) -> Result<Response, RemoteError> {
        let client = self.client()?;
        let request = build(client.request(method, url.clone()))
            .header(AUTHORIZATION, self.auth.clone())
            .timeout(timeout);
        let resp = request.send().await.map_err(|e| self.fail(transport_error(e)))?;
        if resp.status().is_redirection() {
            let err = redirect_refusal(&url, location(&resp));
            drain(resp).await;
            return Err(err);
        }
        Ok(resp)
    }

    /// GET，按规则逐跳跟随重定向；返回最终的响应（状态码由调用方判断）。
    /// 请求头阶段的时限是 30 s，响应体的时限由 [`BodyReader`] 按大小放宽
    async fn get_following(&self, path: &str) -> Result<Response, RemoteError> {
        let client = self.client()?;
        let mut url = self.url_for(path, false)?;
        let mut with_auth = true;
        for hop in 0..=MAX_REDIRECTS {
            let mut request = client.get(url.clone());
            if with_auth {
                request = request.header(AUTHORIZATION, self.auth.clone());
            }
            let resp = match tokio::time::timeout(REQUEST_TIMEOUT, request.send()).await {
                Err(_) => return Err(self.fail(RemoteError::Timeout)),
                Ok(Err(e)) => return Err(self.fail(transport_error(e))),
                Ok(Ok(resp)) => resp,
            };
            if !resp.status().is_redirection() {
                return Ok(resp);
            }
            let next = if hop == MAX_REDIRECTS {
                Err(RemoteError::Redirect)
            } else {
                redirect_target(&url, location(&resp))
            };
            drain(resp).await;
            let next = next?;
            // 凭据只发给服务器自身；跳到 CDN 等别的源之后不再带上，即使之后又跳回来
            with_auth = with_auth && same_origin(&next, &self.base);
            url = next;
        }
        Err(RemoteError::Redirect)
    }

    fn fail(&self, err: RemoteError) -> RemoteError {
        self.note_error(&err);
        err
    }

    async fn propfind(&self, path: &str, depth: u8) -> Result<Option<Vec<RawEntry>>, RemoteError> {
        let url = self.url_for(path, depth > 0)?;
        let mut resp = self
            .send(propfind_method(), url.clone(), REQUEST_TIMEOUT, |r| {
                r.header("Depth", if depth == 0 { "0" } else { "1" })
                    .header(CONTENT_TYPE, "application/xml; charset=utf-8")
                    .body(PROPFIND_BODY)
            })
            .await?;
        match resp.status() {
            StatusCode::MULTI_STATUS | StatusCode::OK => {}
            StatusCode::NOT_FOUND | StatusCode::GONE => {
                drain(resp).await;
                return Ok(None);
            }
            status => {
                drain(resp).await;
                return Err(status_error(status));
            }
        }
        let body = read_to_bytes(&mut resp, MAX_LISTING_BYTES)
            .await
            .map_err(|e| self.fail(e))?;
        parse_multistatus(&body, &url).map(Some)
    }

    /// 单个资源的属性（`Depth: 0`）；不存在返回 `None`
    pub async fn stat(&self, path: &str) -> Result<Option<Entry>, RemoteError> {
        let Some(raw) = self.propfind(path, 0).await? else {
            return Ok(None);
        };
        let want = request_path(&self.url_for(path, false)?);
        let found = raw
            .iter()
            .find(|e| same_resource(&e.path, &want))
            .or_else(|| (raw.len() == 1).then(|| &raw[0]));
        Ok(found.filter(|e| !e.missing).map(|e| e.to_entry(last_segment(&want))))
    }

    async fn put_response(&self, resp: Response) -> Result<PutInfo, RemoteError> {
        let status = resp.status();
        if status.is_success() {
            let etag = etag_header(&resp);
            drain(resp).await;
            return Ok(PutInfo { etag });
        }
        drain(resp).await;
        Err(status_error(status))
    }
}

/// `Basic base64(用户名:密码)`；用户名不得含 `:`（Basic 认证无法表达）
fn basic_auth(username: &str, password: &str) -> Result<HeaderValue, SyncError> {
    use base64::Engine as _;
    if username.contains(':') {
        return Err(SyncError::Internal("username cannot contain a colon"));
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
    let mut value =
        HeaderValue::from_str(&format!("Basic {encoded}")).map_err(|_| SyncError::Internal("invalid credentials"))?;
    value.set_sensitive(true);
    Ok(value)
}

impl Remote for WebDav {
    async fn list(&self, dir: &str) -> Result<Option<Vec<Entry>>, RemoteError> {
        let Some(raw) = self.propfind(dir, 1).await? else {
            return Ok(None);
        };
        let own = request_path(&self.url_for(dir, true)?);
        Ok(children(raw, &own))
    }

    async fn get(&self, path: &str) -> Result<Option<Bytes>, RemoteError> {
        let mut resp = self.get_following(path).await?;
        match resp.status() {
            StatusCode::OK => {}
            StatusCode::NOT_FOUND | StatusCode::GONE => {
                drain(resp).await;
                return Ok(None);
            }
            status => {
                drain(resp).await;
                return Err(status_error(status));
            }
        }
        let body = read_to_bytes(&mut resp, MAX_GET_BYTES)
            .await
            .map_err(|e| self.fail(e))?;
        Ok(Some(body))
    }

    async fn get_to_file(&self, path: &str, dest: &Path) -> Result<bool, RemoteError> {
        let mut resp = self.get_following(path).await?;
        match resp.status() {
            StatusCode::OK => {}
            StatusCode::NOT_FOUND | StatusCode::GONE => {
                drain(resp).await;
                return Ok(false);
            }
            status => {
                drain(resp).await;
                return Err(status_error(status));
            }
        }
        let mut file = tokio::fs::File::create(dest).await?;
        if let Err(e) = read_to_file(&mut resp, &mut file, MAX_FILE_BYTES).await {
            drop(file);
            let _ = tokio::fs::remove_file(dest).await;
            return Err(self.fail(e));
        }
        Ok(true)
    }

    async fn put(&self, path: &str, body: Bytes) -> Result<PutInfo, RemoteError> {
        let url = self.url_for(path, false)?;
        let timeout = transfer_timeout(body.len() as u64);
        let resp = self.send(Method::PUT, url, timeout, |r| r.body(body)).await?;
        self.put_response(resp).await
    }

    async fn put_file(&self, path: &str, src: &Path, len: u64) -> Result<PutInfo, RemoteError> {
        let file = tokio::fs::File::open(src).await?;
        if file.metadata().await?.len() != len {
            return Err(RemoteError::Io(std::io::ErrorKind::InvalidData));
        }
        let url = self.url_for(path, false)?;
        // 边读边发，不把文件整块读进内存；显式给出 Content-Length，不用分块传输编码
        // （不少 WebDAV 服务器与 CDN 拒绝没有长度的 PUT）
        let stream = tokio_util::io::ReaderStream::with_capacity(file.take(len), UPLOAD_CHUNK_BYTES);
        let body = reqwest::Body::wrap_stream(stream);
        let resp = self
            .send(Method::PUT, url, transfer_timeout(len), |r| {
                r.header(CONTENT_LENGTH, len).body(body)
            })
            .await?;
        self.put_response(resp).await
    }

    async fn delete(&self, path: &str) -> Result<(), RemoteError> {
        let url = self.url_for(path, path.ends_with('/') || path.is_empty())?;
        let resp = self.send(Method::DELETE, url, REQUEST_TIMEOUT, |r| r).await?;
        let status = resp.status();
        drain(resp).await;
        match status {
            // 207：目录里有成员没删掉
            StatusCode::MULTI_STATUS => Err(status_error(status)),
            s if s.is_success() => Ok(()),
            StatusCode::NOT_FOUND | StatusCode::GONE => Ok(()),
            s => Err(status_error(s)),
        }
    }

    async fn mkcol(&self, path: &str) -> Result<(), RemoteError> {
        let url = self.url_for(path, true)?;
        let resp = self.send(mkcol_method(), url, REQUEST_TIMEOUT, |r| r).await?;
        let status = resp.status();
        drain(resp).await;
        match status {
            // 405：目录已存在
            s if s.is_success() => Ok(()),
            StatusCode::METHOD_NOT_ALLOWED => Ok(()),
            s => Err(status_error(s)),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// PROPFIND 多状态响应
// ---------------------------------------------------------------------------------------------

/// 多状态响应里的一项（路径已解码、去掉结尾的 `/`）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RawEntry {
    path: String,
    /// 这一项带着 404 / 410 的状态：有的服务器对不存在的目录回 207，在多状态里报告 404
    missing: bool,
    is_dir: bool,
    etag: Option<String>,
    size: Option<u64>,
    modified_ms: Option<i64>,
}

impl RawEntry {
    fn to_entry(&self, fallback_name: &str) -> Entry {
        let name = last_segment(&self.path);
        Entry {
            name: if name.is_empty() { fallback_name } else { name }.to_string(),
            is_dir: self.is_dir,
            etag: self.etag.clone(),
            size: if self.is_dir { None } else { self.size },
            modified_ms: self.modified_ms,
        }
    }
}

/// 请求地址的路径：百分号解码、去掉结尾的 `/`
fn request_path(url: &Url) -> String {
    decode_path(url.path())
}

fn decode_path(raw: &str) -> String {
    let decoded = percent_decode_str(raw).decode_utf8_lossy();
    decoded.trim_end_matches('/').to_string()
}

fn last_segment(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

/// 两个解码后的路径指的是同一个资源。除完全相等外，也接受按段对齐的后缀：
/// 反向代理后的服务器有时报告的 href 少了（或多了）代理加的前缀
fn same_resource(a: &str, b: &str) -> bool {
    a == b || segment_suffix(a, b) || segment_suffix(b, a)
}

fn segment_suffix(long: &str, short: &str) -> bool {
    short.starts_with('/') && short.len() > 1 && long.len() > short.len() && long.ends_with(short)
}

/// 从 Depth 1 的结果里去掉目录自身，转成直接子项；目录自身被报告为不存在时返回 `None`
fn children(raw: Vec<RawEntry>, own: &str) -> Option<Vec<Entry>> {
    let mut own_index = raw.iter().position(|e| same_resource(&e.path, own));
    // 路径完全对不上时（少见的代理改写），按惯例第一项就是目录自身
    if own_index.is_none()
        && raw
            .first()
            .is_some_and(|e| e.is_dir && last_segment(&e.path) == last_segment(own))
    {
        own_index = Some(0);
    }
    if own_index.is_some_and(|i| raw[i].missing) {
        return None;
    }
    let entries = raw
        .into_iter()
        .enumerate()
        .filter(|(i, e)| Some(*i) != own_index && !e.missing)
        .map(|(_, e)| e.to_entry(""))
        .filter(|e| !e.name.is_empty())
        .collect();
    Some(entries)
}

/// `HTTP/1.1 200 OK` → 200
fn parse_status_line(s: &str) -> Option<u16> {
    s.split_whitespace().nth(1)?.parse().ok()
}

fn parse_http_date_ms(s: &str) -> Option<i64> {
    let t = httpdate::parse_http_date(s.trim()).ok()?;
    let ms = t.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis();
    i64::try_from(ms).ok()
}

#[derive(Default)]
struct Props {
    is_dir: bool,
    etag: Option<String>,
    size: Option<u64>,
    modified_ms: Option<i64>,
}

#[derive(Default)]
struct ResponseAcc {
    href: Option<String>,
    status: Option<u16>,
    props: Props,
}

#[derive(Default)]
struct PropstatAcc {
    status: Option<u16>,
    props: Props,
}

/// 解析多状态响应。不看命名空间、只看本地名（`D:` / `d:` / `lp1:` / 无前缀都一样），
/// 本地名比较忽略大小写；支持 CDATA、实体与字符引用；href 可以是绝对 URL 或绝对路径（相对 `base` 解析）。
/// 只采纳状态为 2xx（或未给出状态）的 propstat；整个 response 带 404 / 410 的标为不存在，带其他非 2xx 状态的跳过
fn parse_multistatus(body: &[u8], base: &Url) -> Result<Vec<RawEntry>, RemoteError> {
    let text = String::from_utf8_lossy(body);
    let mut reader = quick_xml::Reader::from_str(&text);
    reader.config_mut().trim_text(false);

    let mut stack: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut saw_root = false;
    let mut response: Option<ResponseAcc> = None;
    let mut propstat: Option<PropstatAcc> = None;
    let mut out = Vec::new();

    loop {
        let event = reader.read_event().map_err(|_| RemoteError::Protocol)?;
        match event {
            Event::Start(e) => {
                let name = e.local_name().into_inner().to_ascii_lowercase();
                match name.as_str() {
                    "multistatus" => saw_root = true,
                    "response" => response = Some(ResponseAcc::default()),
                    "propstat" => propstat = Some(PropstatAcc::default()),
                    "collection" => mark_collection(&stack, &mut propstat),
                    _ => {}
                }
                stack.push(name);
                buf.clear();
            }
            Event::Empty(e) => {
                match e.local_name().into_inner().to_ascii_lowercase().as_str() {
                    "multistatus" => saw_root = true,
                    "collection" => mark_collection(&stack, &mut propstat),
                    _ => {}
                }
                buf.clear();
            }
            Event::Text(t) => buf.push_str(&t.xml10_content()),
            Event::CData(t) => buf.push_str(&t.into_inner()),
            Event::GeneralRef(r) => {
                if let Ok(Some(ch)) = r.resolve_char_ref() {
                    buf.push(ch);
                } else {
                    match &*r {
                        "amp" => buf.push('&'),
                        "lt" => buf.push('<'),
                        "gt" => buf.push('>'),
                        "quot" => buf.push('"'),
                        "apos" => buf.push('\''),
                        // 未声明的实体：不展开（也就不存在实体膨胀攻击）
                        _ => {}
                    }
                }
            }
            Event::End(_) => {
                let name = stack.pop().unwrap_or_default();
                let parent = stack.last().map(String::as_str);
                let value = buf.trim();
                let in_prop = parent == Some("prop");
                match name.as_str() {
                    "href" if parent == Some("response") => {
                        if let Some(r) = response.as_mut() {
                            r.href = Some(value.to_string());
                        }
                    }
                    "status" if parent == Some("propstat") => {
                        if let Some(p) = propstat.as_mut() {
                            p.status = parse_status_line(value);
                        }
                    }
                    "status" if parent == Some("response") => {
                        if let Some(r) = response.as_mut() {
                            r.status = parse_status_line(value);
                        }
                    }
                    "getetag" if in_prop => {
                        if let Some(p) = propstat.as_mut() {
                            p.props.etag = normalize_etag(value);
                        }
                    }
                    "getcontentlength" if in_prop => {
                        if let Some(p) = propstat.as_mut() {
                            p.props.size = value.parse().ok();
                        }
                    }
                    "getlastmodified" if in_prop => {
                        if let Some(p) = propstat.as_mut() {
                            p.props.modified_ms = parse_http_date_ms(value);
                        }
                    }
                    "propstat" => {
                        if let (Some(p), Some(r)) = (propstat.take(), response.as_mut())
                            && p.status.is_none_or(|s| (200..300).contains(&s))
                        {
                            merge_props(&mut r.props, p.props);
                        }
                    }
                    "response" => {
                        let missing = |s: u16| s == 404 || s == 410;
                        if let Some(r) = response.take()
                            && r.status.is_none_or(|s| (200..300).contains(&s) || missing(s))
                            && let Some(href) = r.href
                            && let Ok(url) = base.join(&href)
                        {
                            out.push(RawEntry {
                                path: decode_path(url.path()),
                                missing: r.status.is_some_and(missing),
                                is_dir: r.props.is_dir,
                                etag: r.props.etag,
                                size: r.props.size,
                                modified_ms: r.props.modified_ms,
                            });
                        }
                    }
                    _ => {}
                }
                buf.clear();
            }
            // 文档提前结束（还有没闭合的元素）：响应被截断，不能当作完整的列表
            Event::Eof if !stack.is_empty() => return Err(RemoteError::Protocol),
            Event::Eof => break,
            Event::Comment(_) | Event::Decl(_) | Event::PI(_) | Event::DocType(_) => {}
        }
    }
    if !saw_root {
        return Err(RemoteError::Protocol);
    }
    Ok(out)
}

/// `<resourcetype><collection/></resourcetype>`：只认 resourcetype 里的 collection
fn mark_collection(stack: &[String], propstat: &mut Option<PropstatAcc>) {
    if stack.last().map(String::as_str) == Some("resourcetype")
        && let Some(p) = propstat.as_mut()
    {
        p.props.is_dir = true;
    }
}

fn merge_props(into: &mut Props, from: Props) {
    into.is_dir |= from.is_dir;
    if from.etag.is_some() {
        into.etag = from.etag;
    }
    if from.size.is_some() {
        into.size = from.size;
    }
    if from.modified_ms.is_some() {
        into.modified_ms = from.modified_ms;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_https_and_local_http() {
        for ok in [
            "https://dav.example.com/remote.php/dav/files/u",
            "https://example.com",
            "http://192.168.1.10:5005/webdav",
            "http://10.0.0.2",
            "http://172.16.3.4",
            "http://127.0.0.1:8080",
            "http://169.254.1.1",
            "http://[::1]:8080",
            "http://[fd00::1]",
            "http://[fe80::1]",
            "http://nas:5005",
            "http://localhost",
            "http://diskstation.local/",
            "http://diskstation.local./",
        ] {
            assert!(validate_server_url(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn rejects_bad_urls() {
        use ServerUrlError::*;
        let cases = [
            ("http://dav.example.com", HttpsRequired),
            ("http://8.8.8.8", HttpsRequired),
            ("http://[2001:db8::1]", HttpsRequired),
            ("http://[::ffff:8.8.8.8]", HttpsRequired),
            ("ftp://example.com", Scheme),
            ("https://user:pass@example.com", UserInfo),
            ("https://user@example.com", UserInfo),
            ("https://example.com/?a=1", Query),
            ("https://example.com/?", Query),
            ("https://example.com/#x", Fragment),
            ("not a url", Invalid),
            ("file:///etc/passwd", Scheme),
        ];
        for (raw, want) in cases {
            assert_eq!(validate_server_url(raw).unwrap_err(), want, "{raw}");
        }
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_BYTES));
        assert_eq!(validate_server_url(&long).unwrap_err(), TooLong);
    }

    #[test]
    fn server_url_is_normalized() {
        let n = |raw: &str| validate_server_url(raw).unwrap().to_string();
        assert_eq!(n("https://example.com/webdav"), "https://example.com/webdav/");
        assert_eq!(n("https://example.com/webdav/"), "https://example.com/webdav/");
        assert_eq!(n("HTTPS://Example.COM:443/dav"), "https://example.com/dav/");
        assert_eq!(n("https://example.com"), "https://example.com/");
        assert_eq!(n("http://NAS:5005/dav"), "http://nas:5005/dav/");
    }

    #[test]
    fn root_is_wisp_under_the_given_path() {
        let a = WebDav::new("https://example.com/webdav", "someone", "hunter2").unwrap();
        assert_eq!(a.base.as_str(), "https://example.com/webdav/wisp/");
        let b = WebDav::new("https://example.com/webdav/", "u", "p").unwrap();
        assert_eq!(b.base.as_str(), "https://example.com/webdav/wisp/");
        let c = WebDav::new("https://example.com", "u", "p").unwrap();
        assert_eq!(c.base.as_str(), "https://example.com/wisp/");
        // Debug 不泄露地址与账号
        let dbg = format!("{a:?}");
        assert!(!dbg.contains("example") && !dbg.contains("someone") && !dbg.contains("hunter2"));
        assert!(!a.direct);
        assert!(WebDav::new("http://192.168.1.2/dav", "u", "p").unwrap().direct);
        assert!(WebDav::new("https://u:p@example.com", "u", "p").is_err());
        assert!(WebDav::new("https://example.com", "a:b", "p").is_err());
    }

    #[test]
    fn paths_are_encoded_per_segment() {
        let dav = WebDav::new("https://example.com/my files/", "u", "p").unwrap();
        assert_eq!(
            dav.url_for("", true).unwrap().as_str(),
            "https://example.com/my%20files/wisp/"
        );
        assert_eq!(
            dav.url_for("", false).unwrap().as_str(),
            "https://example.com/my%20files/wisp/"
        );
        assert_eq!(
            dav.url_for("v/log/d", true).unwrap().as_str(),
            "https://example.com/my%20files/wisp/v/log/d/"
        );
        assert_eq!(
            dav.url_for("a b/c#d?e", false).unwrap().as_str(),
            "https://example.com/my%20files/wisp/a%20b/c%23d%3Fe"
        );
        assert!(dav.url_for("../x", false).is_err());
    }

    #[test]
    fn futures_are_send() {
        fn assert_send<T: Send>(_: T) {}
        let dav = WebDav::new("https://example.com", "u", "p").unwrap();
        let tmp = std::path::PathBuf::from("unused");
        assert_send(dav.stat(""));
        assert_send(dav.list(""));
        assert_send(dav.get(""));
        assert_send(dav.get_to_file("", &tmp));
        assert_send(dav.put("", Bytes::new()));
        assert_send(dav.put_file("", &tmp, 0));
        assert_send(dav.delete(""));
        assert_send(dav.mkcol(""));
    }

    #[test]
    fn missing_collection_reported_inside_multistatus() {
        let base = Url::parse("https://example.com/dav/wisp/x/").unwrap();
        let body = br#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/dav/wisp/x/</D:href><D:status>HTTP/1.1 404 Not Found</D:status></D:response></D:multistatus>"#;
        let raw = parse_multistatus(body, &base).unwrap();
        assert_eq!(children(raw, "/dav/wisp/x"), None);
        let body = br#"<D:multistatus xmlns:D="DAV:">
            <D:response><D:href>/dav/wisp/x/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
            <D:response><D:href>/dav/wisp/x/gone</D:href><D:status>HTTP/1.1 404 Not Found</D:status></D:response>
            <D:response><D:href>/dav/wisp/x/hidden</D:href><D:status>HTTP/1.1 403 Forbidden</D:status></D:response>
            <D:response><D:href>/dav/wisp/x/ok</D:href></D:response>
        </D:multistatus>"#;
        let names: Vec<String> = children(parse_multistatus(body, &base).unwrap(), "/dav/wisp/x")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["ok"]);
    }

    #[test]
    fn etags_are_normalized() {
        assert_eq!(normalize_etag("\"abc\"").as_deref(), Some("abc"));
        assert_eq!(normalize_etag("W/\"abc\"").as_deref(), Some("abc"));
        assert_eq!(normalize_etag(" abc ").as_deref(), Some("abc"));
        assert_eq!(normalize_etag("\"\""), None);
        assert_eq!(normalize_etag(""), None);
    }

    #[test]
    fn redirect_rules() {
        let check = |from: &str, to: &str| redirect_target(&Url::parse(from).unwrap(), Some(to));
        let refuse = |from: &str, to: Option<&str>| redirect_refusal(&Url::parse(from).unwrap(), to);
        assert!(check("https://a.example/x", "https://cdn.example/y?sig=1").is_ok());
        assert!(check("https://a.example/x", "/other").is_ok());
        assert_eq!(
            check("https://a.example/x", "http://cdn.example/y"),
            Err(RemoteError::Redirect)
        );
        assert_eq!(
            check("https://a.example/x", "http://a.example/y"),
            Err(RemoteError::Redirect)
        );
        assert_eq!(
            check("http://nas/x", "http://nas:8080/y").map(|u| u.port()),
            Ok(Some(8080))
        );
        assert_eq!(check("http://nas/x", "http://other/y"), Err(RemoteError::Redirect));
        assert_eq!(check("http://nas/x", "https://other/y"), Err(RemoteError::Redirect));
        assert_eq!(check("http://nas/x", "https://nas/y"), Err(RemoteError::HttpsRequired));
        assert_eq!(
            check("https://a.example/x", "ftp://a.example/y"),
            Err(RemoteError::Redirect)
        );
        assert_eq!(
            check("https://a.example/x", "https://u:p@cdn.example/y"),
            Err(RemoteError::Redirect)
        );
        assert_eq!(
            refuse("http://nas/x", Some("https://nas/x")),
            RemoteError::HttpsRequired
        );
        assert_eq!(
            refuse("http://nas/x", Some("https://nas:5006/x")),
            RemoteError::HttpsRequired
        );
        assert_eq!(
            refuse("https://a.example/x", Some("https://a.example/y")),
            RemoteError::Redirect
        );
        assert_eq!(refuse("https://a.example/x", None), RemoteError::Redirect);
    }
}
