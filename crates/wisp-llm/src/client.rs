//! 请求编排：最多 3 次 HTTP 调用（原始 1 次 + 传输重试 ≤1 次 + 降级 ≤1 次）；
//! 退避 500 ms ±20%，Retry-After ≤ 5 s 时按它等待；只在收到 2xx 响应头之前重试；429 不自动重试。
//!
//! 一次请求的生命周期：
//! 1. 按能力缓存与 params.rs 构造请求体（只构造一次，重试时复用；降级后才重建）；按 Provider 的 `connect_ms`
//!    （以及是否回环地址）取缓存的 reqwest Client；
//! 2. 非 2xx：读完整 body（≤ 64 KB）并归一化；param_unsupported 就降级重试一次并写回能力缓存（测试连接的探针不写）、
//!    补发 `Notice{param_degraded}`，降级不了时终止事件是会出卡片的 upstream；可重试且流还没开始的错误重试一次；
//!    否则发出 `Error`；
//! 3. 2xx：逐块读取、逐块解析，每个文本增量立即发出（不等响应结束）；首 token / 空闲超时；
//!    没有结束标记就断流时发 `Error{network, connection_lost}`，已发出的文本保留；2xx 的整个 JSON 是出错形态
//!    （MiniMax 的 base_resp、中转站的 {code, msg}）时按它归类，认不出形状时归 protocol；
//! 4. 任意时刻 cancel（包括消费方慢、通道满而等待发送时）：先 drop 掉响应体（连接随之关闭），丢弃尚未发出的事件，
//!    再发出 `Error{cancelled}`，之后不再发任何事件。失败结束同样先释放响应体再发 `Error`。
//!
//! 事件序列：`Start → (Notice | Delta)* → Done | Error`。流开始之前就失败（非 2xx、连接失败、取消、
//! 第一个事件就是错误）时也先补一个 `Start`（`upstream_id` 取能拿到的上游 id，不补 Notice），再发 `Error`。
//! 日志只记元数据（id、provider、模型、host + path、状态码、耗时、错误码、上游 id、重试 / 降级次数），
//! 不记正文、Key、请求头的值。
//!
//! reqwest 只在构建 Client 时读取一次系统代理，所以缓存的 Client 有生命周期：用它的请求出了网络类错误（拒绝连接、
//! 超时、DNS、TLS、断流）时立即淘汰，自动重试或下一个请求用新建的 Client；在系统代理能于运行中改变的平台
//! （macOS、Windows）上，闲置超过连接池回收时长（此时连接池已空，丢掉不损失任何复用）的也在下次取用时重建，
//! 从而读到当前的系统代理（用户切换 Clash / VPN 后无需重启）。其余平台（Linux 等）的系统代理只来自进程启动时的
//! 环境变量，重建读不到任何新东西，反而每次都要重新加载整个系统 CA 包，因此不做闲置重建。
//! Client 在锁外构建（构建期间不阻塞并发的取用者），构建完再加锁插入（双重检查）。

use std::collections::HashMap;
use std::future::Future;
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use wisp_types::{
    ErrorCode, LlmError, ModelCaps, ModelEntry, NoticeKind, Protocol, Provider, ProviderTestReport, RequestId,
    StreamEvent, Usage, Vision, VisionSource,
};

use crate::adapter::{Adapter, Step};
use crate::caps::CapsStore;
use crate::degrade::{self, Param};
use crate::errors::{self, HttpCtx};
use crate::request::{self, Job};
use crate::secret::Secret;
use crate::sse::SseParser;
use crate::think::ThinkStripper;
use crate::timing::{Expiry, StreamTimers};
use crate::url::Endpoint;
use crate::vision;

/// 非 2xx 响应体的读取上限
const ERROR_BODY_LIMIT: usize = 64 * 1024;
/// 2xx 但不是事件流、而是一整个 JSON 时的读取上限
const WHOLE_BODY_LIMIT: usize = 8 * 1024 * 1024;
/// 正常结束后把剩余响应体读完（让连接回到连接池复用）的上限
const DRAIN_LIMIT: usize = 64 * 1024;
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
/// 重试退避的基准与抖动（500 ms ±20%）
const BACKOFF_BASE_MS: u64 = 500;
const BACKOFF_JITTER_MS: u64 = 100;
/// 事件通道容量：消费方慢时形成背压（停止读网络），不会无限堆积
const CHANNEL_CAPACITY: usize = 64;
/// 空闲连接在连接池里保留的时长（之后连接与连接池的计时器都释放，常驻时零开销）
const POOL_IDLE: Duration = Duration::from_secs(60);
/// 缓存的 Client 闲置超过这个时长就在下次取用时重建（与连接池回收时长相同：连接池已空，重建不损失复用），
/// 借此读取最新的系统代理（只在 [`PROXY_MAY_CHANGE`] 的平台上）
const CLIENT_IDLE_REBUILD: Duration = POOL_IDLE;
/// 系统代理能否在进程运行中改变。macOS（SCDynamicStore）与 Windows（注册表）上用户随时可以开关 Clash / VPN；
/// 其余平台上 reqwest 只读环境变量 `HTTP(S)_PROXY` / `NO_PROXY`，进程生命周期内不变。而 Linux 上每构建一个 Client，
/// rustls-platform-verifier 都会同步读取并解析整个系统 CA 包——闲置重建在那里只有代价、没有收益
const PROXY_MAY_CHANGE: bool = cfg!(any(target_os = "macos", target_os = "windows"));
/// 同时缓存的 Client 上限（不同的 connect_ms × 是否直连；超出时淘汰最久没用的）
const MAX_CACHED_CLIENTS: usize = 6;

/// 客户端配置
pub struct ClientConfig {
    pub user_agent: String,
    pub caps: Arc<dyn CapsStore>,
}

/// 内部状态：按 (connect_ms, 是否直连) 缓存 reqwest::Client（首次用到时创建）。
/// `connect_timeout` 只能设在 ClientBuilder 上，所以不同 `connect_ms` 的 Provider 各用各的 Client；
/// 回环地址（本机 Ollama / LM Studio）的请求不走系统代理。将来加 Provider 级代理时把代理加进缓存键。
/// 缓存的生命周期见文件头（闲置重建、网络类错误淘汰、数量上限）。
pub(crate) struct Inner {
    pub(crate) cfg: ClientConfig,
    clients: parking_lot::Mutex<ClientCache>,
}

/// Client 缓存键
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ClientKey {
    connect_ms: u32,
    direct: bool,
}

struct CachedClient {
    client: reqwest::Client,
    /// 代次：淘汰时只淘汰「出错的请求用的那一个」，不会误伤并发请求刚重建的新 Client
    generation: u64,
    /// 最近一次取用或请求结束的时刻
    last_used: std::time::Instant,
}

#[derive(Default)]
struct ClientCache {
    map: HashMap<ClientKey, CachedClient>,
    next_generation: u64,
}

impl ClientCache {
    /// 取出可用的 Client 并刷新闲置计时；在 [`PROXY_MAY_CHANGE`] 的平台上先清掉闲置过久的
    fn take(&mut self, key: ClientKey) -> Option<HttpClient> {
        let now = std::time::Instant::now();
        if PROXY_MAY_CHANGE {
            self.map
                .retain(|_, c| now.saturating_duration_since(c.last_used) < CLIENT_IDLE_REBUILD);
        }
        let c = self.map.get_mut(&key)?;
        c.last_used = now;
        Some(HttpClient {
            client: c.client.clone(),
            key,
            generation: c.generation,
        })
    }

    /// 放入新建的 Client（超出上限时淘汰最久没用的）
    fn insert(&mut self, key: ClientKey, client: reqwest::Client) -> HttpClient {
        if self.map.len() >= MAX_CACHED_CLIENTS
            && let Some(oldest) = self.map.iter().min_by_key(|(_, c)| c.last_used).map(|(k, _)| *k)
        {
            self.map.remove(&oldest);
        }
        self.next_generation += 1;
        let generation = self.next_generation;
        self.map.insert(
            key,
            CachedClient {
                client: client.clone(),
                generation,
                last_used: std::time::Instant::now(),
            },
        );
        HttpClient {
            client,
            key,
            generation,
        }
    }
}

/// 从缓存取到的 Client（带上身份，出网络类错误时据此淘汰）
pub(crate) struct HttpClient {
    pub(crate) client: reqwest::Client,
    key: ClientKey,
    generation: u64,
}

/// LLM 客户端（可廉价克隆）
#[derive(Clone)]
pub struct LlmClient {
    pub(crate) inner: Arc<Inner>,
}

/// 一次对话请求
pub struct ChatRequest {
    pub request_id: RequestId,
    pub provider: Provider,
    pub key: Option<Secret>,
    pub model: ModelEntry,
    pub system: String,
    pub user: UserContent,
    /// 由 prompts 给出；是否真正发送由 params.rs 决定
    pub temperature: Option<f32>,
    pub max_output_tokens: u32,
    pub purpose: Purpose,
}

/// 用户消息内容
pub enum UserContent {
    Text(String),
    Image {
        bytes: bytes::Bytes,
        mime: &'static str,
        text: String,
        /// OpenAI 两种协议的图片 `detail`：编排器从 `settings.ocr.image_detail` 填入。
        /// original 只发给官方端点（兼容端点退回 high）；Anthropic / Gemini 没有这个参数
        detail: wisp_types::ImageDetail,
    },
}

/// 请求用途：影响 effort 档位（OCR 在 Anthropic 上用 medium）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Purpose {
    Translate,
    Ocr,
    Probe,
    Reflect,
}

/// 聚合后的完整输出（提升译文的「反思」步骤用）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub text: String,
    pub usage: Option<Usage>,
    pub elapsed_ms: u32,
}

/// 上游 /models 返回的一条模型
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteModel {
    pub id: String,
    pub label: Option<String>,
    pub vision: Option<Vision>,
    pub reasoning: Option<bool>,
    pub deprecated: bool,
    pub retired: bool,
    pub context_tokens: Option<u32>,
    pub anthropic_thinking_disabled: Option<bool>,
    pub anthropic_efforts: Option<Vec<String>>,
}

impl LlmClient {
    /// 构造客户端（不创建任何 HTTP 连接，也不创建 reqwest Client）
    pub fn new(cfg: ClientConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                cfg,
                clients: parking_lot::Mutex::new(ClientCache::default()),
            }),
        }
    }

    /// 发起流式请求。返回的接收端依次产出：Start → (Delta | Notice)* → Done | Error（流开始前就失败时是 Start → Error）。
    /// 内部处理重试、降级、超时和 `<think>` 剥离；cancel 触发后立即发出 Error{code: cancelled}，之后不再发任何事件
    /// （cancel 之前已经进入通道的事件仍会先被收到）。丢弃接收端也会中止请求（连接随之关闭）。必须在 tokio 运行时里调用。
    pub fn stream(&self, req: ChatRequest, cancel: CancellationToken) -> mpsc::Receiver<StreamEvent> {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let inner = Arc::clone(&self.inner);
                handle.spawn(async move {
                    let mut sink = ChannelSink(tx);
                    run(&inner, &req, &cancel, &mut sink).await;
                    // sink 在这里释放：接收端随即收到 None（正常结束后的收尾读取在独立的任务里，不拖住通道）
                });
            }
            Err(_) => {
                let _ = tx.try_send(start_event(&req, None));
                let _ = tx.try_send(StreamEvent::Error(errors::internal(
                    "LlmClient::stream must be called inside a tokio runtime",
                )));
            }
        }
        rx
    }

    /// 聚合整段输出（提升译文的「反思」步骤用）
    pub async fn complete(&self, req: ChatRequest, cancel: CancellationToken) -> Result<Completion, LlmError> {
        let mut rx = self.stream(req, cancel);
        let mut text = String::new();
        while let Some(ev) = rx.recv().await {
            match ev {
                StreamEvent::Delta { text: t } => text.push_str(&t),
                StreamEvent::Done { elapsed_ms, usage, .. } => {
                    return Ok(Completion {
                        text,
                        usage,
                        elapsed_ms,
                    });
                }
                StreamEvent::Error(e) => return Err(e),
                StreamEvent::Start { .. }
                | StreamEvent::Meta { .. }
                | StreamEvent::Phase { .. }
                | StreamEvent::Notice { .. } => {}
            }
        }
        Err(errors::internal("the stream ended without a terminal event"))
    }

    /// 拉取模型列表（各协议的列表端点、分页最多 5 页）
    pub async fn list_models(&self, provider: &Provider, key: Option<&Secret>) -> Result<Vec<RemoteModel>, LlmError> {
        crate::models::list(&self.inner, provider, key).await.result
    }

    /// 测试连接：并行发 GET /models 和一个 max_tokens=1 的流式探针
    pub async fn test_connection(&self, provider: &Provider, key: Option<&Secret>) -> ProviderTestReport {
        crate::probe::test_connection(self, provider, key).await
    }
}

impl Inner {
    /// 取（或创建）对应 `connect_ms` / 是否直连的 reqwest Client。
    /// 顺带清掉闲置超过 [`CLIENT_IDLE_REBUILD`] 的 Client（它们的连接池已空），让下一次创建读到当前的系统代理。
    /// 需要新建时在锁外构建，不阻塞并发的取用者；构建完加锁时若别的请求已经放进了同一个键的 Client，就用那一个
    pub(crate) fn http(&self, connect_ms: u32, direct: bool) -> Result<HttpClient, LlmError> {
        let key = ClientKey {
            connect_ms: connect_ms.max(1),
            direct,
        };
        if let Some(c) = self.clients.lock().take(key) {
            return Ok(c);
        }
        let client = self.build_client(key)?;
        let mut cache = self.clients.lock();
        if let Some(c) = cache.take(key) {
            // 双重检查：并发的请求先建好了，丢掉自己这个（还没有任何连接）
            return Ok(c);
        }
        Ok(cache.insert(key, client))
    }

    /// 请求结束：刷新闲置计时（长时间的流式请求结束后，连接回到连接池，仍可被下一个请求复用）
    pub(crate) fn touch(&self, http: &HttpClient) {
        let mut cache = self.clients.lock();
        if let Some(c) = cache.map.get_mut(&http.key)
            && c.generation == http.generation
        {
            c.last_used = std::time::Instant::now();
        }
    }

    /// 网络类错误（拒绝连接、超时、DNS、TLS、断流）：淘汰这个请求用的 Client。
    /// 最常见的原因是系统代理变了（开 / 关 Clash、切 VPN），而 reqwest 只在构建 Client 时读取系统代理
    pub(crate) fn evict(&self, http: &HttpClient) {
        let mut cache = self.clients.lock();
        if cache
            .map
            .get(&http.key)
            .is_some_and(|c| c.generation == http.generation)
        {
            cache.map.remove(&http.key);
            log::debug!(
                "llm dropped the cached HTTP client (connect_ms={} direct={}) after a network error",
                http.key.connect_ms,
                http.key.direct
            );
        }
    }

    fn build_client(&self, key: ClientKey) -> Result<reqwest::Client, LlmError> {
        ensure_crypto_provider();
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(u64::from(key.connect_ms)))
            .pool_idle_timeout(POOL_IDLE)
            .pool_max_idle_per_host(2)
            // 不跟随重定向：reqwest 跨域重定向时只会去掉 Authorization，x-api-key / x-goog-api-key / ?key= 会被带到别的主机
            .redirect(reqwest::redirect::Policy::none());
        if reqwest::header::HeaderValue::from_str(&self.cfg.user_agent).is_ok() {
            builder = builder.user_agent(self.cfg.user_agent.clone());
        }
        if key.direct {
            builder = builder.no_proxy();
        }
        builder
            .build()
            .map_err(|_| errors::internal("failed to initialise the HTTP client"))
    }

    #[cfg(test)]
    fn cached_clients(&self) -> usize {
        self.clients.lock().map.len()
    }

    #[cfg(test)]
    fn age_clients(&self, by: Duration) {
        for c in self.clients.lock().map.values_mut() {
            c.last_used = c.last_used.checked_sub(by).unwrap_or(c.last_used);
        }
    }
}

/// reqwest 用的是 rustls-no-provider：创建任何 Client 之前兜底安装 ring（与 updater 插件的做法相同）。
/// 宿主进程已经装了别的 provider 时保持不动
fn ensure_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

// ---------------------------------------------------------------------------
// 事件出口

/// 事件出口：通道（stream）或测试连接的探针
pub(crate) trait Sink: Send {
    /// 发出一个事件；返回 false 表示接收方不再需要后续事件（立即停止请求）
    fn emit(&mut self, ev: StreamEvent) -> impl Future<Output = bool> + Send;
    /// 接收方已经离开
    fn closed(&self) -> impl Future<Output = ()> + Send;
}

struct ChannelSink(mpsc::Sender<StreamEvent>);

impl Sink for ChannelSink {
    async fn emit(&mut self, ev: StreamEvent) -> bool {
        self.0.send(ev).await.is_ok()
    }

    async fn closed(&self) {
        self.0.closed().await;
    }
}

/// Start 事件（模型名缺省时用 id）
fn start_event(req: &ChatRequest, upstream_id: Option<String>) -> StreamEvent {
    let m = &req.model;
    StreamEvent::Start {
        request_id: req.request_id.clone(),
        provider_id: req.provider.id.clone(),
        model_id: m.id.clone(),
        model_label: m
            .label
            .clone()
            .filter(|l| !l.trim().is_empty())
            .unwrap_or_else(|| m.id.clone()),
        upstream_id,
    }
}

/// 执行一次请求，把事件交给 `sink`；返回最后一次 HTTP 响应的状态码
pub(crate) async fn run<S: Sink>(
    inner: &Inner,
    req: &ChatRequest,
    cancel: &CancellationToken,
    sink: &mut S,
) -> Option<u16> {
    let mut ex = Exchange {
        inner,
        req,
        cancel,
        sink,
        t0: Instant::now(),
        status: None,
        retried: false,
        degraded: Vec::new(),
        started: false,
        header_upstream_id: None,
        upstream_id: None,
        emitted_text: false,
        display_url: String::new(),
        http: None,
    };
    ex.run().await;
    if let Some(http) = &ex.http {
        inner.touch(http);
    }
    ex.status
}

/// 停止的原因
enum Stop {
    Cancelled,
    /// 接收方离开，或 sink 不再需要事件
    Gone,
}

/// 读流的结局（由 [`Exchange::read_stream`] 在释放响应体之后发出对应的终止事件）
enum StreamEnd {
    /// 正常结束（之前的 Start / Delta 都已发出）
    Finished {
        truncated: Option<wisp_types::TruncateReason>,
        usage: Option<Usage>,
    },
    /// 被取消，或接收方离开
    Stopped(Stop),
    /// 失败结束（已发出的文本保留）
    Failed(LlmError),
}

struct Exchange<'a, S: Sink> {
    inner: &'a Inner,
    req: &'a ChatRequest,
    cancel: &'a CancellationToken,
    sink: &'a mut S,
    t0: Instant,
    status: Option<u16>,
    retried: bool,
    degraded: Vec<Param>,
    started: bool,
    /// 2xx 响应头里的上游请求 id
    header_upstream_id: Option<String>,
    /// 最终的上游 id（响应头优先，其次是响应体里的 id / 错误体里的 request_id），写进 Start 与日志
    upstream_id: Option<String>,
    emitted_text: bool,
    display_url: String,
    /// 拿到响应的那次 HTTP 调用所用的 Client（请求结束时刷新闲置计时；网络类错误时淘汰）
    http: Option<HttpClient>,
}

impl<S: Sink> Exchange<'_, S> {
    async fn run(&mut self) {
        let p = &self.req.provider;
        log::debug!(
            "llm {} start provider={} model={} protocol={:?} purpose={:?}",
            self.req.request_id,
            p.id,
            self.req.model.id,
            p.protocol,
            self.req.purpose
        );
        if self.cancel.is_cancelled() {
            return self.finish_cancelled().await;
        }
        let job = match request::prepare(self.req) {
            Ok(job) => job,
            Err(e) => return self.finish_error(e).await,
        };
        self.display_url = crate::url::display_url(&job.safe_url);
        let mut caps = merged_caps(self.inner.cfg.caps.as_ref(), self.req);
        // 请求体只构造一次，重试时复用（带图时可能有数 MB）；降级改了能力缓存才重建
        let mut built = match request::build(&job, self.req, &caps) {
            Ok(b) => b,
            Err(e) => return self.finish_error(e).await,
        };
        let header_wait =
            Duration::from_millis(u64::from(p.timeouts.connect_ms) + u64::from(p.timeouts.first_token_ms));

        loop {
            // 每次调用都重新取 Client：上一次的网络类错误已经淘汰了旧的，自动重试就会读到当前的系统代理
            let http = match self.inner.http(p.timeouts.connect_ms, job.direct) {
                Ok(c) => c,
                Err(e) => return self.finish_error(e).await,
            };
            let send = http
                .client
                .post(&job.url)
                .headers(built.headers.clone())
                .body(built.body.clone())
                .send();
            let resp = tokio::select! {
                biased;
                () = self.cancel.cancelled() => return self.finish_cancelled().await,
                () = self.sink.closed() => return self.log_end("receiver_gone"),
                r = tokio::time::timeout(header_wait, send) => r,
            };
            let resp = match resp {
                Err(_) => {
                    self.inner.evict(&http);
                    let e = errors::timeout(
                        format!("no response from the server within {} s", header_wait.as_secs().max(1)),
                        &job.safe_url,
                    );
                    return self.finish_error(e).await;
                }
                Ok(Err(err)) => {
                    let (e, retry) = errors::transport(&err, &job.safe_url);
                    if !err.is_builder() {
                        self.inner.evict(&http);
                    }
                    if retry && !self.retried {
                        self.retried = true;
                        log::info!("llm {} retrying after a connection error", self.req.request_id);
                        match self.backoff(None).await {
                            Ok(()) => continue,
                            Err(stop) => return self.stopped(stop).await,
                        }
                    }
                    return self.finish_error(e).await;
                }
                Ok(Ok(r)) => r,
            };
            self.http = Some(http);
            let status = resp.status().as_u16();
            self.status = Some(status);
            if !resp.status().is_success() {
                let headers = resp.headers().clone();
                let body = match self.read_body(resp, ERROR_BODY_LIMIT).await {
                    Ok(b) => b,
                    Err(stop) => return self.stopped(stop).await,
                };
                let ctx = HttpCtx {
                    protocol: p.protocol,
                    endpoint: Endpoint::Chat,
                    url: &job.safe_url,
                    version_hint: job.version_hint,
                    has_image: job.has_image,
                    sent: &built.sent,
                };
                let c = errors::classify_http(&ctx, status, &headers, &body);
                // 探针（max_tokens=1）被「取值太小」拒绝：交给探针改成 16 重试，不当作字段不被支持去降级
                let probe_floor = self.req.purpose == Purpose::Probe
                    && status == 400
                    && degrade::rejects_small_max_tokens(&c.error.message.to_lowercase());
                if c.error.code == ErrorCode::ParamUnsupported
                    && !probe_floor
                    && self.degraded.is_empty()
                    && !c.degrade.is_empty()
                {
                    degrade::apply(&c.degrade, &mut caps);
                    log::info!(
                        "llm {} degrading parameters {:?} after HTTP {status}",
                        self.req.request_id,
                        c.degrade
                    );
                    self.degraded = c.degrade;
                    // 先释放旧请求体再构造新的，带图时不会同时存在两份
                    built.body = bytes::Bytes::new();
                    built = match request::build(&job, self.req, &caps) {
                        Ok(b) => b,
                        Err(e) => return self.finish_error(e).await,
                    };
                    continue;
                }
                if c.auto_retry && !self.retried {
                    self.retried = true;
                    log::info!("llm {} retrying after HTTP {status}", self.req.request_id);
                    match self.backoff(c.error.retry_after_s).await {
                        Ok(()) => continue,
                        Err(stop) => return self.stopped(stop).await,
                    }
                }
                let vision_conclusive = c.vision_conclusive;
                let mut e = c.error;
                if e.code == ErrorCode::ParamUnsupported {
                    // 降级不了：识别不出是哪个参数、同一参数降级后仍被拒、或降级后又被拒了另一个参数（每个请求最多降级一次）。
                    // 最后一种把两次结论都写进能力缓存，用户点 [重试] 一次即可成功。终止事件改为会出卡片的 upstream
                    // （param_unsupported 只以降级成功后的弱提示出现）
                    let fresh: Vec<Param> = c.degrade.into_iter().filter(|x| !self.degraded.contains(x)).collect();
                    let adjusted = !self.degraded.is_empty() && !fresh.is_empty() && !probe_floor;
                    if adjusted {
                        let mut all = self.degraded.clone();
                        all.extend(fresh);
                        self.persist_caps(&all);
                    }
                    e = errors::settle_param_unsupported(e, adjusted);
                }
                if e.code == ErrorCode::NoVision {
                    self.vision_feedback(Vision::No, vision_conclusive);
                }
                return self.finish_error(e).await;
            }
            if !self.degraded.is_empty() {
                let degraded = self.degraded.clone();
                self.persist_caps(&degraded);
            }
            self.header_upstream_id = errors::upstream_request_id(resp.headers());
            drop(built);
            return self.read_stream(resp, &job).await;
        }
    }

    /// 2xx：逐块读取并解析。响应体只由这里持有，任何终止都**先释放响应体**（连接随之关闭，上游停止生成、停止计费）
    /// 再发终止事件：消费方暂时不读、通道已满时，发送终止事件会阻塞，但那时连接早已断开
    async fn read_stream(&mut self, resp: reqwest::Response, job: &Job) {
        let strip = self.req.provider.quirks.strip_think_tags && self.req.provider.protocol == Protocol::OpenAiChat;
        let mut think = ThinkStripper::new(strip);
        let mut body = resp.bytes_stream();
        match self.pump(&mut body, job, &mut think).await {
            StreamEnd::Finished { truncated, usage } => {
                // 剩余响应体（通常只剩 chunked 的结束块）交给独立任务读完，让连接回到连接池；它同样响应取消。
                // 本任务发完 Done 随即结束、释放通道，接收端立刻收到 None
                tokio::spawn(drain(body, self.cancel.clone()));
                match self.finish_done(&mut think, truncated, usage).await {
                    Ok(()) => self.log_end("done"),
                    Err(stop) => self.stopped(stop).await,
                }
            }
            StreamEnd::Stopped(stop) => {
                drop(body);
                self.stopped(stop).await;
            }
            StreamEnd::Failed(e) => {
                drop(body);
                if self.started
                    && let Err(stop) = self.delta(think.finish()).await
                {
                    return self.stopped(stop).await;
                }
                if e.code == ErrorCode::NoVision {
                    // 流中的 no_vision 只在服务端明确说「不支持图片输入」时产生（errors::body_error），结论是确定的
                    let conclusive = errors::is_conclusive_no_vision(&e.message.to_lowercase());
                    self.vision_feedback(Vision::No, conclusive);
                }
                self.finish_error(e).await;
            }
        }
    }

    /// 读取、解析响应体并发出 Start / Delta，直到出现终止原因；终止事件由 [`Self::read_stream`] 在释放响应体之后发出
    async fn pump<B>(&mut self, body: &mut B, job: &Job, think: &mut ThinkStripper) -> StreamEnd
    where
        B: futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin + Send,
    {
        let protocol = self.req.provider.protocol;
        let mut adapter = Adapter::with_image(protocol, &job.safe_url, job.has_image);
        let mut timers = StreamTimers::new(&self.req.provider.timeouts, Instant::now());
        let mut parser = SseParser::default();
        let mut mode = Mode::Sniff;
        let mut any_event = false;
        let mut events = Vec::new();
        let mut steps = Vec::new();

        let mut eof = false;
        loop {
            let deadline = timers.deadline();
            // 先显式检查到期：即使数据源源不断（例如只有心跳），计时器也不会被饿死
            let next = if Instant::now() >= deadline {
                None
            } else {
                tokio::select! {
                    biased;
                    () = self.cancel.cancelled() => return StreamEnd::Stopped(Stop::Cancelled),
                    () = self.sink.closed() => return StreamEnd::Stopped(Stop::Gone),
                    chunk = body.next() => Some(chunk),
                    () = tokio::time::sleep_until(deadline) => None,
                }
            };
            eof |= matches!(next, Some(None));
            match next {
                None => match timers.expiry(Instant::now()) {
                    Expiry::Grace => adapter.finish_now(&mut steps),
                    Expiry::FirstToken => {
                        let msg = format!("the model produced no output within {} s", timers.first_token_secs());
                        steps.push(Step::Fail(errors::timeout(msg, &job.safe_url)));
                    }
                    Expiry::Idle => {
                        // 首 token 之后整条连接静默：常见于网络切换后的死连接，淘汰 Client
                        self.evict_client();
                        let msg = format!("the stream stalled for {} s", timers.idle_secs());
                        steps.push(Step::Fail(errors::timeout(msg, &job.safe_url)));
                    }
                },
                Some(Some(Ok(bytes))) => {
                    timers.on_bytes(Instant::now());
                    match &mut mode {
                        Mode::Sniff => match sniff(&bytes) {
                            Sniffed::Undecided => {}
                            Sniffed::Html => {
                                let mut e = errors::protocol("the endpoint returned an HTML page", &job.safe_url);
                                e.subcode = Some(wisp_types::ErrorSubcode::HtmlBody);
                                steps.push(Step::Fail(e));
                            }
                            Sniffed::Json => mode = Mode::Json(bytes.to_vec()),
                            Sniffed::Sse => {
                                mode = Mode::Sse;
                                if parser.push(&bytes, &mut events).is_err() {
                                    steps.push(Step::Fail(too_large(&job.safe_url)));
                                }
                            }
                        },
                        Mode::Sse => {
                            if parser.push(&bytes, &mut events).is_err() {
                                steps.push(Step::Fail(too_large(&job.safe_url)));
                            }
                        }
                        Mode::Json(buf) => {
                            if buf.len() + bytes.len() > WHOLE_BODY_LIMIT {
                                steps.push(Step::Fail(too_large(&job.safe_url)));
                            } else {
                                buf.extend_from_slice(&bytes);
                            }
                        }
                    }
                }
                Some(None) => match std::mem::replace(&mut mode, Mode::Sse) {
                    Mode::Json(buf) => whole_json(&mut adapter, &buf, &job.safe_url, &mut steps),
                    Mode::Sniff => steps.push(Step::Fail(errors::protocol(
                        "the endpoint returned an empty response",
                        &job.safe_url,
                    ))),
                    Mode::Sse => {
                        if parser.finish(&mut events).is_err() {
                            steps.push(Step::Fail(too_large(&job.safe_url)));
                        }
                        any_event |= !events.is_empty();
                        for ev in events.drain(..) {
                            adapter.on_event(&ev, &mut steps);
                        }
                        if any_event {
                            adapter.on_eof(&mut steps);
                        } else {
                            steps.push(Step::Fail(errors::protocol(
                                format!(
                                    "the endpoint did not return a {} event stream; check the template and base URL",
                                    crate::adapter::protocol_name(protocol)
                                ),
                                &job.safe_url,
                            )));
                        }
                    }
                },
                Some(Some(Err(_))) => {
                    // 读到一半连接断了：看到结束标记就按正常结束，否则「连接中断」（不自动重试）
                    if adapter.finishing() {
                        adapter.finish_now(&mut steps);
                    } else {
                        self.evict_client();
                        steps.push(Step::Fail(errors::connection_lost(&job.safe_url)));
                    }
                }
            }
            any_event |= !events.is_empty();
            for ev in events.drain(..) {
                adapter.on_event(&ev, &mut steps);
                if adapter.is_done() {
                    break;
                }
            }
            events.clear();

            for step in steps.drain(..) {
                let flow = match step {
                    Step::Start { upstream_id } => self.ensure_start(upstream_id).await,
                    Step::Token => {
                        timers.on_token();
                        Ok(())
                    }
                    Step::Text(t) => {
                        timers.on_token();
                        let out = think.push(t);
                        self.delta(out).await
                    }
                    Step::Finish { truncated, usage } => return StreamEnd::Finished { truncated, usage },
                    Step::Fail(e) => return StreamEnd::Failed(e),
                };
                if let Err(stop) = flow {
                    // 被取消或接收方离开：本批剩下的步骤一个都不再发出
                    return StreamEnd::Stopped(stop);
                }
            }
            if eof {
                // 每个适配器在 EOF 时都会给出终止步骤；走到这里说明状态机出了意外，兜底结束而不是空转
                return StreamEnd::Failed(errors::internal("the stream ended without a terminal event"));
            }
            if adapter.finishing() {
                timers.start_grace(Instant::now());
            }
        }
    }

    /// 发出一个事件。cancel 优先：消费方慢、通道满而在等待发送时也能立即打断，被打断的事件不会进入通道
    async fn emit(&mut self, ev: StreamEvent) -> Result<(), Stop> {
        let cancel = self.cancel;
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(Stop::Cancelled),
            sent = self.sink.emit(ev) => if sent { Ok(()) } else { Err(Stop::Gone) },
        }
    }

    /// 发出 Start（只发一次），随后补发重试 / 降级的提示
    async fn ensure_start(&mut self, body_upstream_id: Option<String>) -> Result<(), Stop> {
        if self.started {
            return Ok(());
        }
        self.upstream_id = self.header_upstream_id.clone().or(body_upstream_id);
        // 发送成功后才算开始：Start 本身被取消打断时，由 finish_cancelled 补发，序列仍以 Start 开头
        self.emit(start_event(self.req, self.upstream_id.clone())).await?;
        self.started = true;
        if self.retried {
            self.emit(StreamEvent::Notice {
                kind: NoticeKind::Retried,
            })
            .await?;
        }
        if !self.degraded.is_empty() {
            self.emit(StreamEvent::Notice {
                kind: NoticeKind::ParamDegraded,
            })
            .await?;
        }
        Ok(())
    }

    /// 流开始前就失败或被取消：补发一个 Start（不补 Notice），让序列仍是 Start → Error。
    /// 此时通道里什么都还没有，发送不会阻塞；接收方已经离开时发送失败也无妨，随后的 Error 同样会失败
    async fn synthetic_start(&mut self, upstream_id: Option<String>) {
        if self.started {
            return;
        }
        self.started = true;
        self.upstream_id = self
            .upstream_id
            .take()
            .or_else(|| self.header_upstream_id.clone())
            .or(upstream_id);
        let _ = self.sink.emit(start_event(self.req, self.upstream_id.clone())).await;
    }

    /// 发出一段文本（空串不发）
    async fn delta(&mut self, text: String) -> Result<(), Stop> {
        if text.is_empty() {
            return Ok(());
        }
        self.ensure_start(None).await?;
        if !text.trim().is_empty() {
            self.emitted_text = true;
        }
        self.emit(StreamEvent::Delta { text }).await
    }

    /// 正常结束：放行 `<think>` 剥离器里剩下的正文，发出 Done
    async fn finish_done(
        &mut self,
        think: &mut ThinkStripper,
        truncated: Option<wisp_types::TruncateReason>,
        usage: Option<Usage>,
    ) -> Result<(), Stop> {
        self.delta(think.finish()).await?;
        self.ensure_start(None).await?;
        self.vision_feedback(Vision::Yes, true);
        let done = StreamEvent::Done {
            elapsed_ms: self.elapsed_ms(),
            usage,
            truncated,
            cached: false,
            no_text: !self.emitted_text,
        };
        self.emit(done).await
    }

    /// 读取非 2xx 的响应体（≤ limit；读超时按空闲超时）
    async fn read_body(&mut self, resp: reqwest::Response, limit: usize) -> Result<Vec<u8>, Stop> {
        let idle = Duration::from_millis(u64::from(self.req.provider.timeouts.idle_ms.max(1)));
        let mut body = resp.bytes_stream();
        let mut buf = Vec::new();
        while buf.len() < limit {
            let chunk = tokio::select! {
                biased;
                () = self.cancel.cancelled() => return Err(Stop::Cancelled),
                () = self.sink.closed() => return Err(Stop::Gone),
                c = tokio::time::timeout(idle, body.next()) => c,
            };
            match chunk {
                Ok(Some(Ok(bytes))) => {
                    let room = limit - buf.len();
                    buf.extend_from_slice(&bytes[..bytes.len().min(room)]);
                }
                _ => break,
            }
        }
        Ok(buf)
    }

    /// 重试前等待：Retry-After（≤ 5 s，调用方已保证）或 500 ms ±20%
    async fn backoff(&mut self, retry_after_s: Option<u32>) -> Result<(), Stop> {
        let wait = match retry_after_s {
            Some(s) => Duration::from_secs(u64::from(s)),
            None => jittered_backoff(),
        };
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => Err(Stop::Cancelled),
            () = self.sink.closed() => Err(Stop::Gone),
            () = tokio::time::sleep(wait) => Ok(()),
        }
    }

    async fn stopped(&mut self, stop: Stop) {
        match stop {
            Stop::Cancelled => self.finish_cancelled().await,
            Stop::Gone => self.log_end("receiver_gone"),
        }
    }

    async fn finish_cancelled(&mut self) {
        self.synthetic_start(None).await;
        self.sink.emit(StreamEvent::Error(errors::cancelled())).await;
        self.log_end("cancelled");
    }

    async fn finish_error(&mut self, e: LlmError) {
        // 先在完整原文上脱敏再截断
        let e = errors::finalize(e, self.req.key.as_ref());
        self.synthetic_start(e.upstream_request_id.clone()).await;
        let code = e.code;
        let upstream = e.upstream_request_id.clone().or_else(|| self.upstream_id.clone());
        if let Err(Stop::Cancelled) = self.emit(StreamEvent::Error(e)).await {
            // 消费方慢、通道满，等待发送错误时被取消：终止事件改为 cancelled
            return self.finish_cancelled().await;
        }
        log::info!(
            "llm {} failed code={code:?} status={:?} url={} upstream={} elapsed={}ms retried={} degraded={}",
            self.req.request_id,
            self.status,
            self.display_url,
            upstream.as_deref().unwrap_or("-"),
            self.elapsed_ms(),
            self.retried,
            !self.degraded.is_empty()
        );
    }

    fn log_end(&self, outcome: &str) {
        log::info!(
            "llm {} {outcome} provider={} model={} status={:?} url={} upstream={} elapsed={}ms retried={} degraded={}",
            self.req.request_id,
            self.req.provider.id,
            self.req.model.id,
            self.status,
            self.display_url,
            self.upstream_id.as_deref().unwrap_or("-"),
            self.elapsed_ms(),
            self.retried,
            !self.degraded.is_empty()
        );
    }

    fn elapsed_ms(&self) -> u32 {
        u32::try_from(self.t0.elapsed().as_millis()).unwrap_or(u32::MAX)
    }

    /// 读流时出了网络类故障：淘汰这次用的 Client（下一个请求重建，读到当前的系统代理）
    fn evict_client(&self) {
        if let Some(http) = &self.http {
            self.inner.evict(http);
        }
    }

    /// 把降级结论写回能力缓存（desktop 合并去抖后写进 settings.json）。
    /// 测试连接的探针不写：它的请求是刻意构造的极小请求（max_tokens=1、'ping'），得出的结论不一定适用于正式请求
    fn persist_caps(&self, params: &[Param]) {
        if self.req.purpose == Purpose::Probe {
            return;
        }
        let mut apply = |caps: &mut ModelCaps| degrade::apply(params, caps);
        self.inner
            .cfg
            .caps
            .update(&self.req.provider.id, &self.req.model.id, &mut apply);
    }

    /// 视觉能力的运行时反馈：只对带图请求；不覆盖用户的手动设置。
    /// `conclusive = false`（服务端只是提到了图片，没有明确说不支持图片输入）时只把 Unknown 写成 No，
    /// 不覆盖已知支持视觉的模型
    fn vision_feedback(&self, observed: Vision, conclusive: bool) {
        if !matches!(self.req.user, UserContent::Image { .. }) {
            return;
        }
        let m = &self.req.model;
        if vision::runtime_update_needed(m.vision, m.vision_source, observed, conclusive) {
            self.inner
                .cfg
                .caps
                .set_vision(&self.req.provider.id, &m.id, observed, VisionSource::Runtime);
        }
    }
}

/// 2xx 响应体的形态（按第一个非空白字节判断，不依赖 Content-Type）
enum Mode {
    Sniff,
    Sse,
    /// 端点忽略了 stream:true，返回了一整个 JSON（或 JSON 数组）
    Json(Vec<u8>),
}

enum Sniffed {
    Undecided,
    Sse,
    Json,
    Html,
}

fn sniff(bytes: &[u8]) -> Sniffed {
    match bytes.iter().find(|b| !b.is_ascii_whitespace()) {
        None => Sniffed::Undecided,
        Some(b'{' | b'[') => Sniffed::Json,
        Some(b'<') => Sniffed::Html,
        Some(_) => Sniffed::Sse,
    }
}

/// 2xx 的整个 JSON 响应体：非流式响应、JSON 数组，或出错形态（MiniMax 的 base_resp、中转站的 {code, msg}），见
/// [`Adapter::on_whole_body`]
fn whole_json(adapter: &mut Adapter, buf: &[u8], url: &str, steps: &mut Vec<Step>) {
    match serde_json::from_slice::<serde_json::Value>(buf) {
        Ok(v) => adapter.on_whole_body(&v, steps),
        Err(_) => steps.push(Step::Fail(errors::protocol(
            "the endpoint returned a response that is neither an event stream nor JSON",
            url,
        ))),
    }
}

fn too_large(url: &str) -> LlmError {
    errors::protocol("the endpoint sent an oversized event", url)
}

/// 正常结束后把剩余的响应体读完（通常只剩 chunked 的结束块），让 HTTP/1.1 连接回到连接池。
/// 在独立任务里运行：不占用事件通道，接收端在 Done 之后立刻收到 None
async fn drain<St>(mut body: St, cancel: CancellationToken)
where
    St: futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin,
{
    let mut read = 0usize;
    let deadline = Instant::now() + DRAIN_TIMEOUT;
    while read < DRAIN_LIMIT {
        let chunk = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            c = tokio::time::timeout_at(deadline, body.next()) => c,
        };
        match chunk {
            Ok(Some(Ok(b))) => read += b.len(),
            _ => return,
        }
    }
}

/// 500 ms ±20%
fn jittered_backoff() -> Duration {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(Instant::now().elapsed().as_nanos());
    let r = h.finish() % (2 * BACKOFF_JITTER_MS + 1);
    Duration::from_millis(BACKOFF_BASE_MS - BACKOFF_JITTER_MS + r)
}

/// CapsStore 的结论优先，缺的项退回请求里带的 `ModelEntry.caps`
fn merged_caps(store: &dyn CapsStore, req: &ChatRequest) -> ModelCaps {
    let s = store.get(&req.provider.id, &req.model.id);
    let m = &req.model.caps;
    ModelCaps {
        temperature: s.temperature.or(m.temperature),
        max_tokens_field: s.max_tokens_field.or(m.max_tokens_field),
        thinking_param: s.thinking_param.or(m.thinking_param),
        effort_param: s.effort_param.or(m.effort_param),
        stream_options: s.stream_options.or(m.stream_options),
        include_obfuscation: s.include_obfuscation.or(m.include_obfuscation),
        anthropic_thinking_disabled: s.anthropic_thinking_disabled.or(m.anthropic_thinking_disabled),
        anthropic_efforts: s.anthropic_efforts.or_else(|| m.anthropic_efforts.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::MemoryCapsStore;

    fn client() -> LlmClient {
        LlmClient::new(ClientConfig {
            user_agent: "Wisp/test".to_owned(),
            caps: Arc::new(MemoryCapsStore::new()),
        })
    }

    #[test]
    fn clients_are_lazy_and_cached_per_connect_ms() {
        let c = client();
        assert_eq!(c.inner.cached_clients(), 0, "启动后首次请求前不存在 Client");
        let a = c.inner.http(1_000, false).expect("client");
        let b = c.inner.http(1_000, false).expect("client");
        assert_eq!(a.generation, b.generation, "同一个 Client");
        assert_eq!(c.inner.cached_clients(), 1);
        c.inner.http(2_000, false).expect("client");
        c.inner.http(2_000, true).expect("client");
        assert_eq!(c.inner.cached_clients(), 3);
    }

    #[test]
    fn idle_clients_are_rebuilt_only_where_the_system_proxy_can_change() {
        let c = client();
        let a = c.inner.http(1_000, false).expect("client");
        c.inner.http(2_000, false).expect("client");
        c.inner.age_clients(CLIENT_IDLE_REBUILD + Duration::from_secs(1));
        let b = c.inner.http(1_000, false).expect("client");
        if PROXY_MAY_CHANGE {
            assert_ne!(
                a.generation, b.generation,
                "闲置超过连接池回收时长后重建，读取当前的系统代理"
            );
            assert_eq!(c.inner.cached_clients(), 1, "其余闲置的 Client 一并释放");
        } else {
            // 系统代理只来自环境变量：重建读不到新东西，还要重新加载整个 CA 包
            assert_eq!(a.generation, b.generation, "不做闲置重建");
            assert_eq!(c.inner.cached_clients(), 2);
        }
        // 请求结束时刷新闲置计时
        c.inner.age_clients(CLIENT_IDLE_REBUILD - Duration::from_secs(1));
        c.inner.touch(&b);
        c.inner.age_clients(Duration::from_secs(2));
        assert_eq!(c.inner.http(1_000, false).expect("client").generation, b.generation);
    }

    #[test]
    fn concurrent_first_use_shares_one_client() {
        // Client 在锁外构建；同时有多个请求第一次取用同一个键时，插入前的双重检查保证最终只有一个被缓存、
        // 所有请求用的都是它
        let c = client();
        let barrier = std::sync::Barrier::new(8);
        let generations: Vec<u64> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        c.inner.http(1_000, false).expect("client").generation
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().expect("thread")).collect()
        });
        assert!(generations.iter().all(|g| *g == generations[0]), "{generations:?}");
        assert_eq!(c.inner.cached_clients(), 1);
        assert_eq!(c.inner.clients.lock().next_generation, 1, "后建好的直接丢弃，不进缓存");
    }

    #[test]
    fn eviction_only_removes_the_client_that_failed() {
        let c = client();
        let old = c.inner.http(1_000, false).expect("client");
        c.inner.evict(&old);
        assert_eq!(c.inner.cached_clients(), 0);
        let fresh = c.inner.http(1_000, false).expect("client");
        assert_ne!(old.generation, fresh.generation);
        // 并发请求拿着旧 Client 出错：不误伤刚重建的新 Client
        c.inner.evict(&old);
        assert_eq!(c.inner.cached_clients(), 1);
        assert_eq!(c.inner.http(1_000, false).expect("client").generation, fresh.generation);
    }

    #[test]
    fn cache_size_is_bounded() {
        let c = client();
        for i in 0..20u32 {
            c.inner.http(1_000 + i, i % 2 == 0).expect("client");
        }
        assert_eq!(c.inner.cached_clients(), MAX_CACHED_CLIENTS);
    }

    #[tokio::test]
    async fn network_error_invalidates_the_cached_client_before_the_retry() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().expect("addr").port()
        };
        let c = client();
        let mut req = text_request(&format!("http://127.0.0.1:{port}/v1"));
        req.provider.timeouts.connect_ms = 1_000;
        let mut rx = c.stream(req, CancellationToken::new());
        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev);
        }
        assert!(
            matches!(events.as_slice(), [StreamEvent::Start { .. }, StreamEvent::Error(e)] if e.code == ErrorCode::Network)
        );
        let cache = c.inner.clients.lock();
        // 第一次连接被拒 → 淘汰 → 自动重试时新建了第二个 Client（读取当时的系统代理）→ 又被拒 → 再淘汰
        assert_eq!(cache.next_generation, 2, "重试用的是新建的 Client");
        assert!(cache.map.is_empty(), "出错的 Client 不留在缓存里");
    }

    #[test]
    fn backoff_is_within_twenty_percent() {
        for _ in 0..200 {
            let d = jittered_backoff();
            assert!(
                d >= Duration::from_millis(400) && d <= Duration::from_millis(600),
                "{d:?}"
            );
        }
    }

    #[test]
    fn sniffing() {
        assert!(matches!(sniff(b"  \n"), Sniffed::Undecided));
        assert!(matches!(sniff(b"\ndata: {}"), Sniffed::Sse));
        assert!(matches!(sniff(b": ping"), Sniffed::Sse));
        assert!(matches!(sniff(b" {\"id\":1}"), Sniffed::Json));
        assert!(matches!(sniff(b"<!DOCTYPE html>"), Sniffed::Html));
    }

    fn text_request(base_url: &str) -> ChatRequest {
        let mut provider = crate::templates::provider_from_template("openai", &[]).expect("template");
        provider.id = "p".to_owned();
        provider.base_url = base_url.to_owned();
        ChatRequest {
            request_id: "r-1".to_owned(),
            provider,
            key: None,
            model: ModelEntry::new("m"),
            system: String::new(),
            user: UserContent::Text("x".to_owned()),
            temperature: None,
            max_output_tokens: 16,
            purpose: Purpose::Translate,
        }
    }

    #[test]
    fn store_caps_win_over_entry_caps() {
        let store = MemoryCapsStore::new();
        store.update("p", "m", &mut |c| c.temperature = Some(false));
        let mut req = text_request("https://api.openai.com/v1");
        req.model.caps.temperature = Some(true);
        req.model.caps.effort_param = Some(false);
        let caps = merged_caps(&store, &req);
        assert_eq!(caps.temperature, Some(false));
        assert_eq!(caps.effort_param, Some(false));
    }
}
