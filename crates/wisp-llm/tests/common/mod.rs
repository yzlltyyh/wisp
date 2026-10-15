//! 集成测试共用：客户端 / 请求构造、事件收集与序列校验，以及一个脚本化的 HTTP/1.1 服务器。
//!
//! 脚本化服务器按 chunked 编码逐块发送响应体：每一块都是一个独立的 HTTP chunk，hyper 解码时不会把两块合并，
//! 因此「按 1 字节 / 7 字节 / 整块切分」在客户端一侧真实成立；它还能保持连接不关、按信号放行后续数据、
//! 中途断开、观察客户端是否关闭了连接——这些是 wiremock 做不到的。
//!
//! 所有测试进程都**不手动安装** rustls 的 CryptoProvider，以此验证 wisp-llm 的兜底安装。

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;
use wisp_llm::{
    ChatRequest, ClientConfig, LlmClient, MemoryCapsStore, ModelEntry, Protocol, Provider, Purpose, Quirks, Secret,
    StreamEvent, Timeouts, UserContent,
};

// ---------------------------------------------------------------------------
// 客户端与请求

pub fn client() -> (LlmClient, Arc<MemoryCapsStore>) {
    let caps = Arc::new(MemoryCapsStore::new());
    let client = LlmClient::new(ClientConfig {
        user_agent: "Wisp/test".to_owned(),
        caps: caps.clone(),
    });
    (client, caps)
}

pub fn provider(protocol: Protocol, base_url: &str) -> Provider {
    Provider {
        id: "test".to_owned(),
        name: "Test".to_owned(),
        protocol,
        base_url: base_url.to_owned(),
        headers: BTreeMap::new(),
        quirks: Quirks::default(),
        models: Vec::new(),
        timeouts: Timeouts::default(),
        template_id: None,
    }
}

pub fn request(provider: Provider, model: &str, text: &str) -> ChatRequest {
    ChatRequest {
        request_id: "r-1".to_owned(),
        provider,
        key: Some(Secret::new("sk-test".to_owned())),
        model: ModelEntry::new(model),
        system: "You are a translator.".to_owned(),
        user: UserContent::Text(text.to_owned()),
        temperature: Some(0.3),
        max_output_tokens: 556,
        purpose: Purpose::Translate,
    }
}

/// 1×1 PNG（只当作字节，不会被解码）
pub const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89,
];

pub fn image_request(provider: Provider, model: &str) -> ChatRequest {
    let mut req = request(provider, model, "");
    req.user = UserContent::Image {
        bytes: bytes::Bytes::from_static(TINY_PNG),
        mime: "image/png",
        text: "Transcribe the text.".to_owned(),
        detail: wisp_types::ImageDetail::High,
    };
    req.purpose = Purpose::Ocr;
    req.temperature = Some(0.0);
    req.max_output_tokens = 4096;
    req
}

/// 收集到通道关闭为止（整体最多 20 s）
pub async fn collect(rx: mpsc::Receiver<StreamEvent>) -> Vec<StreamEvent> {
    tokio::time::timeout(Duration::from_secs(20), collect_all(rx))
        .await
        .expect("stream did not finish within 20 s")
}

async fn collect_all(mut rx: mpsc::Receiver<StreamEvent>) -> Vec<StreamEvent> {
    let mut out = Vec::new();
    while let Some(ev) = rx.recv().await {
        out.push(ev);
    }
    out
}

pub async fn run(client: &LlmClient, req: ChatRequest) -> Vec<StreamEvent> {
    let events = collect(client.stream(req, CancellationToken::new())).await;
    assert_valid_sequence(&events);
    events
}

pub fn text_of(events: &[StreamEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Delta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

pub fn last(events: &[StreamEvent]) -> &StreamEvent {
    events.last().expect("at least one event")
}

pub fn error_of(events: &[StreamEvent]) -> &wisp_llm::LlmError {
    match last(events) {
        StreamEvent::Error(e) => e,
        other => panic!("expected an error, got {other:?}"),
    }
}

/// 合法序列：Start →（Notice | Delta）* → 恰好一个终止事件。流开始前就失败时是 Start → Error
pub fn assert_valid_sequence(events: &[StreamEvent]) {
    assert!(events.len() >= 2, "at least Start and a terminal event: {events:?}");
    assert!(
        matches!(events[0], StreamEvent::Start { .. }),
        "first event must be Start: {events:?}"
    );
    let terminals = events.iter().filter(|e| e.is_terminal()).count();
    assert_eq!(terminals, 1, "exactly one terminal event: {events:?}");
    assert!(last(events).is_terminal(), "terminal event must be last: {events:?}");
    for e in &events[1..events.len() - 1] {
        assert!(
            matches!(e, StreamEvent::Delta { .. } | StreamEvent::Notice { .. }),
            "unexpected event in the middle: {e:?}"
        );
    }
}

/// 流开始前就失败：只有 Start 与 Error 两个事件
pub fn assert_failed_before_stream(events: &[StreamEvent]) {
    assert!(
        matches!(events, [StreamEvent::Start { .. }, StreamEvent::Error(_)]),
        "expected Start → Error, got {events:?}"
    );
}

// ---------------------------------------------------------------------------
// 脚本化服务器

/// 响应体动作
#[derive(Clone)]
pub enum Act {
    /// 作为一个 HTTP chunk 发送
    Send(Vec<u8>),
    Sleep(Duration),
    /// 等待信号再继续
    Wait(Arc<Notify>),
}

/// 发送完动作之后
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// 正常结束 chunked 响应体
    Finish,
    /// 保持连接不关，直到客户端断开
    Hold,
    /// 不发结束块就关闭连接（断流）
    Abort,
}

#[derive(Clone)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub head_delay: Option<Duration>,
    pub acts: Vec<Act>,
    pub end: End,
}

impl Reply {
    pub fn sse(chunks: Vec<Vec<u8>>) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".to_owned(), "text/event-stream".to_owned())],
            head_delay: None,
            acts: chunks.into_iter().map(Act::Send).collect(),
            end: End::Finish,
        }
    }

    pub fn sse_text(text: &str) -> Self {
        Self::sse(vec![text.as_bytes().to_vec()])
    }

    pub fn body(status: u16, content_type: &str, body: &str) -> Self {
        Self {
            status,
            headers: vec![("content-type".to_owned(), content_type.to_owned())],
            head_delay: None,
            acts: vec![Act::Send(body.as_bytes().to_vec())],
            end: End::Finish,
        }
    }

    /// 不发响应头就关闭连接（模拟流开始前的连接失败）
    pub fn drop_connection() -> Self {
        Self {
            status: 0,
            headers: Vec::new(),
            head_delay: None,
            acts: Vec::new(),
            end: End::Abort,
        }
    }

    pub fn head_delay(mut self, d: Duration) -> Self {
        self.head_delay = Some(d);
        self
    }

    pub fn acts(mut self, acts: Vec<Act>) -> Self {
        self.acts = acts;
        self
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    pub fn end(mut self, end: End) -> Self {
        self.end = end;
        self
    }
}

/// 服务器收到的请求
#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("request body is JSON")
    }
}

pub struct ScriptServer {
    pub base: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
    closed: Arc<AtomicUsize>,
    closed_notify: Arc<Notify>,
}

impl ScriptServer {
    /// 第 i 个请求用 `replies[i]`，超出时重复最后一个
    pub async fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let closed = Arc::new(AtomicUsize::new(0));
        let closed_notify = Arc::new(Notify::new());
        let counter = Arc::new(AtomicUsize::new(0));
        let replies = Arc::new(replies);
        {
            let (requests, closed, closed_notify) = (requests.clone(), closed.clone(), closed_notify.clone());
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let _ = stream.set_nodelay(true);
                    let (replies, counter) = (replies.clone(), counter.clone());
                    let (requests, closed, closed_notify) = (requests.clone(), closed.clone(), closed_notify.clone());
                    tokio::spawn(async move {
                        serve(stream, &replies, &counter, &requests).await;
                        closed.fetch_add(1, Ordering::SeqCst);
                        closed_notify.notify_waiters();
                    });
                }
            });
        }
        Self {
            base,
            requests,
            closed,
            closed_notify,
        }
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().clone()
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().len()
    }

    /// 等到至少有一条连接被关闭（客户端断开或服务器脚本结束）
    pub async fn wait_closed(&self, within: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let notified = self.closed_notify.notified();
            if self.closed.load(Ordering::SeqCst) > 0 {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.closed.load(Ordering::SeqCst) > 0;
            }
        }
    }
}

async fn serve(mut stream: TcpStream, replies: &[Reply], counter: &AtomicUsize, requests: &Mutex<Vec<Recorded>>) {
    let mut buf = Vec::new();
    loop {
        let Some(req) = read_request(&mut stream, &mut buf).await else {
            return;
        };
        requests.lock().push(req);
        let idx = counter.fetch_add(1, Ordering::SeqCst);
        let Some(reply) = replies.get(idx.min(replies.len().saturating_sub(1))).cloned() else {
            return;
        };
        if reply.status == 0 {
            return;
        }
        if write_reply(&mut stream, &reply).await.is_err() {
            return;
        }
        match reply.end {
            End::Finish => {}
            End::Abort => return,
            End::Hold => {
                // 一直读到客户端关闭连接
                let mut sink = [0u8; 1024];
                while matches!(stream.read(&mut sink).await, Ok(n) if n > 0) {}
                return;
            }
        }
    }
}

async fn write_reply(stream: &mut TcpStream, reply: &Reply) -> std::io::Result<()> {
    if let Some(d) = reply.head_delay {
        tokio::time::sleep(d).await;
    }
    let mut head = format!("HTTP/1.1 {} X\r\n", reply.status);
    for (n, v) in &reply.headers {
        head.push_str(&format!("{n}: {v}\r\n"));
    }
    head.push_str("transfer-encoding: chunked\r\n\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await?;
    for act in &reply.acts {
        match act {
            Act::Send(bytes) if !bytes.is_empty() => {
                let mut chunk = format!("{:x}\r\n", bytes.len()).into_bytes();
                chunk.extend_from_slice(bytes);
                chunk.extend_from_slice(b"\r\n");
                stream.write_all(&chunk).await?;
                stream.flush().await?;
            }
            Act::Send(_) => {}
            Act::Sleep(d) => tokio::time::sleep(*d).await,
            Act::Wait(n) => n.notified().await,
        }
    }
    if reply.end == End::Finish {
        stream.write_all(b"0\r\n\r\n").await?;
        stream.flush().await?;
    }
    Ok(())
}

async fn read_request(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Option<Recorded> {
    let mut tmp = [0u8; 8192];
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let n = stream.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?.to_owned();
    let mut parts = request_line.split(' ');
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let len: usize = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    buf.drain(..head_end + 4);
    while buf.len() < len {
        let n = stream.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body: Vec<u8> = buf.drain(..len).collect();
    Some(Recorded {
        method,
        target,
        headers,
        body,
    })
}

/// 只接受 TCP 连接、从不回应的「黑洞」：https 请求会卡在 TLS 握手，直到客户端的连接超时
pub async fn tls_blackhole() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("https://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((s, _)) = listener.accept().await {
            held.push(s);
        }
    });
    base
}

/// 把字节按 `size` 切块（`usize::MAX` 表示整块）
pub fn split(bytes: &[u8], size: usize) -> Vec<Vec<u8>> {
    bytes
        .chunks(size.clamp(1, bytes.len().max(1)))
        .map(<[u8]>::to_vec)
        .collect()
}
