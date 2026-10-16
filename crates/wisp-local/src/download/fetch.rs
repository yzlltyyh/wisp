//! 单个文件的下载：断点续传、换源、低速检测、边下边算 sha256、进度节流。
//!
//! 写盘与算哈希都在一个专用线程上（[`PartWriter`]）：GB 级文件的 sha256 要十几秒 CPU，不能占 tokio 的工作线程；
//! 异步一侧只收网络数据、经有界通道交给它（通道满时自然形成背压）。

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bytes::Bytes;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_RANGE, ETAG, IF_RANGE, LAST_MODIFIED, RANGE};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use wisp_types::DownloadErrorCode;

use super::{
    MAX_REDIRECTS, PROBE_TIMEOUT, PROGRESS_INTERVAL, Progress, READ_IDLE_TIMEOUT, SLOW_BYTES_PER_SEC, SLOW_WINDOW,
    SPEED_WINDOW, Stage,
};
use crate::error::LocalError;

/// 写盘线程的通道容量（块数）。reqwest 的块一般是 8–64 KB，排满约几 MB
const CHANNEL_CAPACITY: usize = 64;
/// 重新计算已有部分的哈希时每次读的字节数
const HASH_CHUNK: usize = 1 << 20;
/// 进度回调的另一个节流条件：变化 ≥ 1%
const PROGRESS_STEP_PER_MILLE: u64 = 10;
/// 速度采样的最小间隔
const SPEED_SAMPLE: Duration = Duration::from_millis(100);
/// 续传前要重算哈希的部分文件达到这么大时先报「校验中」（GB 级的 `.part` 重算要十几秒，期间不应显示为「下载中 0 B/s」；
/// 小文件一瞬间就算完，不为它切换阶段）
const REHASH_NOTICE_BYTES: u64 = 64 * 1024 * 1024;

/// 下载策略的参数。默认值即模块常量；测试里调小，以便快速覆盖重试与换源
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tuning {
    /// 连续这么久低于 `slow_bytes_per_sec` 就换下一个源（只有还有别的源时）
    pub slow_window: Duration,
    pub slow_bytes_per_sec: u64,
    /// 同一个源上连续失败（没有任何进展）多少次后换源
    pub retries_per_source: u32,
    /// 第一次重试前的等待，之后每次翻倍
    pub retry_backoff: Duration,
    /// 多源探测的超时
    pub probe_timeout: Duration,
    /// 测试模式：不走系统代理，并放行回环地址上的 http。下载正式的源时为 false（与 wisp-llm 一样读系统代理、只接受
    /// https）；测试访问本机的模拟服务器时设为 true——macOS 的系统代理设置里的「例外」不被读取，回环地址也会被送进代理
    pub direct: bool,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            slow_window: SLOW_WINDOW,
            slow_bytes_per_sec: SLOW_BYTES_PER_SEC,
            retries_per_source: 4,
            retry_backoff: Duration::from_secs(1),
            probe_timeout: PROBE_TIMEOUT,
            direct: false,
        }
    }
}

/// 一个远端文件：候选地址（按优先顺序，内容相同）、大小、sha256（小写十六进制）
#[derive(Debug, Clone, Copy)]
pub struct Remote<'a> {
    pub urls: &'a [&'a str],
    pub size: u64,
    pub sha256: &'a str,
}

/// 只允许 https。回环地址上的 http 只在 `loopback_http`（= [`Tuning::direct`]，只有测试访问本机的模拟服务器时打开）时放行：
/// 正式的下载源都是 https，正式构建里不留明文的口子
fn allowed(url: &reqwest::Url, loopback_http: bool) -> bool {
    match url.scheme() {
        "https" => true,
        "http" if loopback_http => match url.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            Some(url::Host::Domain(d)) => d == "localhost",
            None => false,
        },
        _ => false,
    }
}

/// 日志里的地址：host + 路径首段（不含查询参数里的签名、不含完整路径）
pub(crate) fn log_target(url: &reqwest::Url) -> String {
    let seg = url.path_segments().and_then(|mut s| s.next()).unwrap_or("");
    format!("{}/{seg}", url.host_str().unwrap_or("?"))
}

/// 下载任务的 HTTP 客户端：跟随重定向最多 5 次且只到允许的地址，不带 Referer，读空闲超时
pub(crate) fn client(direct: bool) -> Result<reqwest::Client, LocalError> {
    let builder = crate::net::client_builder();
    let builder = if direct { builder.no_proxy() } else { builder };
    builder
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else if allowed(attempt.url(), direct) {
                attempt.follow()
            } else {
                attempt.error("redirect to a disallowed address")
            }
        }))
        .referer(false)
        .read_timeout(READ_IDLE_TIMEOUT)
        .build()
        .map_err(|_| LocalError::Protocol("http client"))
}

/// 写盘失败是否因为磁盘满（或超出配额）：这类错误报 `disk_full`，让界面提示清理空间而不是重试
pub(crate) fn is_disk_full(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded
    )
}

/// 本机文件错误：磁盘满单独归类
pub(crate) fn local_io(e: std::io::Error) -> LocalError {
    if is_disk_full(&e) {
        LocalError::Download(DownloadErrorCode::DiskFull)
    } else {
        LocalError::Io(e)
    }
}

// ---------------------------------------------------------------------------
// 进度

/// 进度的累计、节流与速度（3 s 滑动平均）
pub(crate) struct Meter<'a> {
    cb: &'a (dyn Fn(Progress) + Send + Sync),
    total: u64,
    done: u64,
    last_emit: Option<Instant>,
    last_emit_done: u64,
    samples: VecDeque<(Instant, u64)>,
}

impl<'a> Meter<'a> {
    pub(crate) fn new(cb: &'a (dyn Fn(Progress) + Send + Sync), total: u64, done: u64) -> Self {
        Self {
            cb,
            total,
            done: done.min(total),
            last_emit: None,
            last_emit_done: 0,
            samples: VecDeque::new(),
        }
    }

    pub(crate) fn add(&mut self, n: u64) {
        self.done = self.done.saturating_add(n).min(self.total);
        let now = Instant::now();
        if self
            .samples
            .back()
            .is_none_or(|(t, _)| now.duration_since(*t) >= SPEED_SAMPLE)
        {
            self.samples.push_back((now, self.done));
        }
        while self.samples.len() > 2
            && self
                .samples
                .front()
                .is_some_and(|(t, _)| now.duration_since(*t) > SPEED_WINDOW)
        {
            self.samples.pop_front();
        }
        let step = (self.total * PROGRESS_STEP_PER_MILLE / 1000).max(1);
        let due = self
            .last_emit
            .is_none_or(|t| now.duration_since(t) >= PROGRESS_INTERVAL)
            || self.done.saturating_sub(self.last_emit_done) >= step;
        if due {
            self.emit(Stage::Downloading, now);
        }
    }

    /// 总量变大（区间提取退回整包下载）。只增不减：已报告的进度比例可能下降一点，但已完成的字节数不会倒退
    pub(crate) fn grow_total(&mut self, n: u64) {
        self.total = self.total.saturating_add(n);
    }

    /// 丢弃了已计入的字节（从头重下）
    pub(crate) fn sub(&mut self, n: u64) {
        self.done = self.done.saturating_sub(n);
        self.samples.clear();
    }

    pub(crate) fn verifying(&mut self) {
        self.emit(Stage::Verifying, Instant::now());
    }

    pub(crate) fn done(&self) -> u64 {
        self.done
    }

    fn speed(&self, now: Instant) -> u64 {
        let Some((t0, d0)) = self.samples.front() else {
            return 0;
        };
        let dt = now.duration_since(*t0).as_secs_f64();
        if dt < 0.5 {
            return 0;
        }
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let bps = (self.done.saturating_sub(*d0) as f64 / dt) as u64;
        bps
    }

    fn emit(&mut self, stage: Stage, now: Instant) {
        self.last_emit = Some(now);
        self.last_emit_done = self.done;
        (self.cb)(Progress {
            stage,
            done: self.done,
            total: self.total,
            bytes_per_sec: if stage == Stage::Downloading {
                self.speed(now)
            } else {
                0
            },
        });
    }
}

// ---------------------------------------------------------------------------
// 写盘线程

enum Cmd {
    Data(Bytes),
    /// 从头重下：清空文件与哈希
    Reset,
    /// 刷到磁盘并回报当前长度
    Sync(oneshot::Sender<std::io::Result<u64>>),
    /// 写一个小文件（续传信息）
    Side(PathBuf, Vec<u8>),
}

/// 写完后的结果
pub(crate) struct Finished {
    pub(crate) len: u64,
    pub(crate) sha256: String,
}

/// `.part` 文件的写盘线程（见模块说明）
pub(crate) struct PartWriter {
    tx: Option<mpsc::Sender<Cmd>>,
    done: Option<oneshot::Receiver<std::io::Result<Finished>>>,
}

impl PartWriter {
    /// 打开（或新建）`.part`，重新计算已有部分的哈希（超过 `max_len` 的部分文件视为无效，清空）。返回写盘器与已有长度
    pub(crate) async fn open(path: &Path, max_len: u64) -> Result<(Self, u64), LocalError> {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (ready_tx, ready_rx) = oneshot::channel();
        let (done_tx, done_rx) = oneshot::channel();
        let path = path.to_path_buf();
        std::thread::Builder::new()
            .name("wisp-download-writer".into())
            .spawn(move || {
                let result = writer_main(&path, max_len, rx, ready_tx);
                let _ = done_tx.send(result);
            })
            .map_err(LocalError::Io)?;
        let len = match ready_rx.await {
            Ok(Ok(len)) => len,
            Ok(Err(e)) => return Err(local_io(e)),
            Err(_) => return Err(LocalError::Protocol("download writer")),
        };
        Ok((
            Self {
                tx: Some(tx),
                done: Some(done_rx),
            },
            len,
        ))
    }

    async fn send(&mut self, cmd: Cmd) -> Result<(), LocalError> {
        let Some(tx) = &self.tx else {
            return Err(LocalError::Protocol("download writer"));
        };
        if tx.send(cmd).await.is_ok() {
            return Ok(());
        }
        // 线程已经因为写盘失败退出：取出它的错误
        Err(self.take_error().await)
    }

    async fn take_error(&mut self) -> LocalError {
        self.tx = None;
        match self.done.take() {
            Some(rx) => match rx.await {
                Ok(Err(e)) => local_io(e),
                _ => LocalError::Protocol("download writer"),
            },
            None => LocalError::Protocol("download writer"),
        }
    }

    pub(crate) async fn write(&mut self, data: Bytes) -> Result<(), LocalError> {
        self.send(Cmd::Data(data)).await
    }

    pub(crate) async fn reset(&mut self) -> Result<(), LocalError> {
        self.send(Cmd::Reset).await
    }

    pub(crate) async fn side_file(&mut self, path: PathBuf, bytes: Vec<u8>) -> Result<(), LocalError> {
        self.send(Cmd::Side(path, bytes)).await
    }

    /// 等前面的数据都落盘
    pub(crate) async fn sync(&mut self) -> Result<u64, LocalError> {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::Sync(tx)).await?;
        match rx.await {
            Ok(Ok(len)) => Ok(len),
            Ok(Err(e)) => Err(local_io(e)),
            Err(_) => Err(self.take_error().await),
        }
    }

    /// 关闭并取得长度与 sha256
    pub(crate) async fn finish(mut self) -> Result<Finished, LocalError> {
        self.tx = None;
        match self.done.take() {
            Some(rx) => match rx.await {
                Ok(Ok(f)) => Ok(f),
                Ok(Err(e)) => Err(local_io(e)),
                Err(_) => Err(LocalError::Protocol("download writer")),
            },
            None => Err(LocalError::Protocol("download writer")),
        }
    }
}

fn hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn writer_main(
    path: &Path,
    max_len: u64,
    mut rx: mpsc::Receiver<Cmd>,
    ready: oneshot::Sender<std::io::Result<u64>>,
) -> std::io::Result<Finished> {
    let opened = (|| -> std::io::Result<(std::fs::File, ring::digest::Context, u64)> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let mut len = file.metadata()?.len();
        if len > max_len {
            file.set_len(0)?;
            len = 0;
        }
        let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
        file.seek(SeekFrom::Start(0))?;
        let mut buf = vec![0u8; HASH_CHUNK];
        let mut left = len;
        while left > 0 {
            let want = usize::try_from(left.min(HASH_CHUNK as u64)).unwrap_or(HASH_CHUNK);
            let n = file.read(&mut buf[..want])?;
            if n == 0 {
                break;
            }
            ctx.update(&buf[..n]);
            left -= n as u64;
        }
        // 文件比 metadata 说的短（极少见）：按实际读到的长度截断
        let len = len - left;
        file.set_len(len)?;
        file.seek(SeekFrom::Start(len))?;
        Ok((file, ctx, len))
    })();
    let (mut file, mut ctx, mut len) = match opened {
        Ok(v) => {
            let _ = ready.send(Ok(v.2));
            v
        }
        Err(e) => {
            let _ = ready.send(Err(std::io::Error::new(e.kind(), "open partial file")));
            return Err(e);
        }
    };
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            Cmd::Data(b) => {
                file.write_all(&b)?;
                ctx.update(&b);
                len += b.len() as u64;
            }
            Cmd::Reset => {
                file.set_len(0)?;
                file.seek(SeekFrom::Start(0))?;
                ctx = ring::digest::Context::new(&ring::digest::SHA256);
                len = 0;
            }
            Cmd::Sync(reply) => {
                let r = file.flush().and_then(|()| file.sync_data()).map(|()| len);
                let _ = reply.send(r);
            }
            Cmd::Side(p, bytes) => {
                // 续传信息只是优化：写不了不影响下载
                let _ = std::fs::write(p, bytes);
            }
        }
    }
    file.flush()?;
    file.sync_data()?;
    Ok(Finished {
        len,
        sha256: hex(ctx.finish().as_ref()),
    })
}

// ---------------------------------------------------------------------------
// 续传信息（`<文件>.part.meta`）

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PartMeta {
    /// 这份部分文件属于哪个 sha256（目录换了文件就作废）
    sha256: String,
    /// 强 ETag（`If-Range` 用）
    etag: Option<String>,
    last_modified: Option<String>,
}

impl PartMeta {
    /// `If-Range` 的值：强 ETag 优先，其次 Last-Modified
    fn validator(&self) -> Option<&str> {
        self.etag.as_deref().or(self.last_modified.as_deref())
    }

    fn update_from(&mut self, headers: &reqwest::header::HeaderMap) {
        let text = |h| headers.get(h).and_then(|v| v.to_str().ok()).map(str::to_owned);
        // 弱 ETag 不能用于 If-Range
        self.etag = text(ETAG).filter(|e| !e.starts_with("W/"));
        self.last_modified = text(LAST_MODIFIED);
    }
}

fn meta_path(part: &Path) -> PathBuf {
    let mut p = part.as_os_str().to_owned();
    p.push(".meta");
    PathBuf::from(p)
}

/// 解析 `Content-Range: bytes <start>-<end>/<total>`；总长未知（`*`）时 total 为 None
pub(crate) fn parse_content_range(v: &str) -> Option<(u64, u64, Option<u64>)> {
    let rest = v.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let total = if total.trim() == "*" {
        None
    } else {
        Some(total.trim().parse().ok()?)
    };
    let (start, end) = (start.trim().parse().ok()?, end.trim().parse().ok()?);
    (start <= end).then_some((start, end, total))
}

// ---------------------------------------------------------------------------
// 下载器

/// 一次请求的结果
enum Attempt {
    /// 文件已完整
    Complete,
    /// 太慢，换下一个源
    Slow,
    /// 可以重试的失败（断流、超时、5xx、签名过期）；`progressed` = 这次收到过数据
    Transient { code: DownloadErrorCode, progressed: bool },
    /// 这个源不能用（404 等）
    Fatal(DownloadErrorCode),
}

/// 一个下载任务的 HTTP 客户端、进度与取消令牌
pub struct Fetcher<'a> {
    client: reqwest::Client,
    pub(crate) meter: Meter<'a>,
    cancel: CancellationToken,
    tuning: Tuning,
}

impl std::fmt::Debug for Fetcher<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fetcher")
            .field("tuning", &self.tuning)
            .finish_non_exhaustive()
    }
}

impl<'a> Fetcher<'a> {
    /// `total` / `done`：整个任务的字节数与已有的部分（进度按它们算）
    pub fn new(
        progress: &'a (dyn Fn(Progress) + Send + Sync),
        total: u64,
        done: u64,
        cancel: CancellationToken,
        tuning: Tuning,
    ) -> Result<Self, LocalError> {
        Ok(Self {
            client: client(tuning.direct)?,
            meter: Meter::new(progress, total, done),
            cancel,
            tuning,
        })
    }

    /// 可取消的等待
    pub(crate) async fn pause(&self, d: Duration) -> Result<(), LocalError> {
        tokio::select! {
            () = self.cancel.cancelled() => Err(LocalError::Cancelled),
            () = tokio::time::sleep(d) => Ok(()),
        }
    }

    /// 探测各候选地址：并发发 `Range: bytes=0-0`，按最先成功的排序（其余保持原顺序垫后）。全部失败时保持原顺序
    pub async fn rank(&self, urls: &[&str]) -> Vec<usize> {
        let probes = urls.iter().enumerate().map(|(i, url)| {
            let req = self.client.get(*url).header(RANGE, "bytes=0-0");
            let timeout = self.tuning.probe_timeout;
            Box::pin(async move {
                match tokio::time::timeout(timeout, req.send()).await {
                    Ok(Ok(r)) if r.status().is_success() => Ok(i),
                    _ => Err(()),
                }
            })
        });
        let mut order: Vec<usize> = (0..urls.len()).collect();
        if urls.len() < 2 {
            return order;
        }
        let winner = tokio::select! {
            () = self.cancel.cancelled() => None,
            r = futures_util::future::select_ok(probes) => r.ok().map(|(i, _)| i),
        };
        if let Some(w) = winner {
            order.retain(|i| *i != w);
            order.insert(0, w);
        }
        order
    }

    /// 下载一个文件到 `part`（续传已有的部分），完成并校验通过后返回；失败时保留 `.part` 以便下次续传（哈希不符时删除）
    pub async fn file(&mut self, remote: &Remote<'_>, part: &Path) -> Result<(), LocalError> {
        let meta_file = meta_path(part);
        let meta = std::fs::read(&meta_file)
            .ok()
            .and_then(|b| serde_json::from_slice::<PartMeta>(&b).ok());
        let before = std::fs::metadata(part).map_or(0, |m| m.len());
        if meta.as_ref().is_some_and(|m| m.sha256 != remote.sha256) {
            // 目录换了文件：旧的部分文件作废
            let _ = std::fs::remove_file(part);
            self.meter.sub(before);
        }
        let mut meta = meta.filter(|m| m.sha256 == remote.sha256).unwrap_or_else(|| PartMeta {
            sha256: remote.sha256.to_owned(),
            ..PartMeta::default()
        });
        let mut retried_checksum = false;
        let mut urls: Vec<&str> = remote.urls.to_vec();
        loop {
            let on_disk = std::fs::metadata(part).map_or(0, |m| m.len());
            if on_disk >= REHASH_NOTICE_BYTES {
                self.meter.verifying();
            }
            let (mut w, mut len) = PartWriter::open(part, remote.size).await?;
            if len < on_disk {
                self.meter.sub(on_disk - len);
            }
            let result = self.fill(&urls, remote, &mut w, &mut len, &mut meta, &meta_file).await;
            if let Err(e) = result {
                let _ = w
                    .side_file(meta_file.clone(), serde_json::to_vec(&meta).unwrap_or_default())
                    .await;
                let _ = w.finish().await;
                return Err(e);
            }
            // 哈希是边下边算的，这里只等写盘线程收尾；「校验中」由调用方在全部文件之后报一次（多文件的模型不在文件之间闪烁）
            let fin = w.finish().await?;
            if fin.len == remote.size && fin.sha256 == remote.sha256 {
                let _ = std::fs::remove_file(&meta_file);
                return Ok(());
            }
            log::warn!("download: checksum mismatch ({} B)", fin.len);
            let _ = std::fs::remove_file(part);
            let _ = std::fs::remove_file(&meta_file);
            self.meter.sub(fin.len);
            meta = PartMeta {
                sha256: remote.sha256.to_owned(),
                ..PartMeta::default()
            };
            if retried_checksum {
                return Err(LocalError::Download(DownloadErrorCode::ChecksumMismatch));
            }
            retried_checksum = true;
            // 换个源再试一次（只有一个源时就同一个源再来一次）
            urls.rotate_left(1);
        }
    }

    /// 依次用各个源把文件补全
    async fn fill(
        &mut self,
        urls: &[&str],
        remote: &Remote<'_>,
        w: &mut PartWriter,
        len: &mut u64,
        meta: &mut PartMeta,
        meta_file: &Path,
    ) -> Result<(), LocalError> {
        let mut last = DownloadErrorCode::Network;
        for (i, url) in urls.iter().enumerate() {
            let has_next = i + 1 < urls.len();
            let mut failures = 0u32;
            let mut backoff = self.tuning.retry_backoff;
            while *len < remote.size {
                if self.cancel.is_cancelled() {
                    return Err(LocalError::Cancelled);
                }
                match self.attempt(url, remote, w, len, meta, meta_file, has_next).await? {
                    Attempt::Complete => break,
                    Attempt::Slow => {
                        log::info!("download: switching source (slow)");
                        break;
                    }
                    Attempt::Fatal(code) => {
                        last = code;
                        break;
                    }
                    Attempt::Transient { code, progressed } => {
                        last = code;
                        if progressed {
                            failures = 0;
                            backoff = self.tuning.retry_backoff;
                            continue;
                        }
                        failures += 1;
                        if failures >= self.tuning.retries_per_source {
                            break;
                        }
                        self.pause(backoff).await?;
                        backoff = backoff.saturating_mul(2);
                    }
                }
            }
            if *len >= remote.size {
                return Ok(());
            }
        }
        if remote.size == 0 {
            return Ok(());
        }
        Err(LocalError::Download(last))
    }

    /// 发一次请求并把响应体写下去
    #[allow(clippy::too_many_arguments)]
    async fn attempt(
        &mut self,
        url: &str,
        remote: &Remote<'_>,
        w: &mut PartWriter,
        len: &mut u64,
        meta: &mut PartMeta,
        meta_file: &Path,
        has_next: bool,
    ) -> Result<Attempt, LocalError> {
        if !reqwest::Url::parse(url).is_ok_and(|u| allowed(&u, self.tuning.direct)) {
            return Ok(Attempt::Fatal(DownloadErrorCode::Server));
        }
        let mut req = self.client.get(url);
        if *len > 0 {
            req = req.header(RANGE, format!("bytes={len}-"));
            if let Some(v) = meta.validator() {
                req = req.header(IF_RANGE, v);
            }
        }
        let started = Instant::now();
        let resp = tokio::select! {
            () = self.cancel.cancelled() => return Err(LocalError::Cancelled),
            r = req.send() => r,
        };
        let mut resp = match resp {
            Ok(r) => r,
            Err(e) => {
                log::warn!(
                    "download: request failed ({})",
                    if e.is_timeout() { "timeout" } else { "connect" }
                );
                return Ok(Attempt::Transient {
                    code: DownloadErrorCode::Network,
                    progressed: false,
                });
            }
        };
        let status = resp.status();
        let target = log_target(resp.url());
        log::info!("download: {target} -> {}", status.as_u16());
        let range = resp
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_content_range);
        let start = match status {
            StatusCode::PARTIAL_CONTENT => match range {
                Some((s, _, total)) if s == *len && total.is_none_or(|t| t == remote.size) => s,
                _ => {
                    // 区间对不上（服务器给了别的部分）：从头再来
                    self.restart(w, len).await?;
                    return Ok(Attempt::Transient {
                        code: DownloadErrorCode::Server,
                        progressed: false,
                    });
                }
            },
            StatusCode::OK => {
                // 200：整个文件（服务器不支持续传或 If-Range 发现文件变了）；ModelScope 的小文件会带 Content-Range
                if range.is_some_and(|(s, _, total)| s != 0 || total.is_some_and(|t| t != remote.size)) {
                    return Ok(Attempt::Fatal(DownloadErrorCode::Server));
                }
                if *len > 0 {
                    self.restart(w, len).await?;
                }
                0
            }
            StatusCode::RANGE_NOT_SATISFIABLE => {
                self.restart(w, len).await?;
                return Ok(Attempt::Transient {
                    code: DownloadErrorCode::Server,
                    progressed: false,
                });
            }
            // 重定向后的 CDN 签名过期等：下次请求会从源地址重新解析
            StatusCode::UNAUTHORIZED
            | StatusCode::FORBIDDEN
            | StatusCode::GONE
            | StatusCode::REQUEST_TIMEOUT
            | StatusCode::TOO_MANY_REQUESTS => {
                return Ok(Attempt::Transient {
                    code: DownloadErrorCode::Server,
                    progressed: false,
                });
            }
            s if s.is_server_error() => {
                return Ok(Attempt::Transient {
                    code: DownloadErrorCode::Server,
                    progressed: false,
                });
            }
            _ => return Ok(Attempt::Fatal(DownloadErrorCode::Server)),
        };
        debug_assert_eq!(start, *len);
        meta.update_from(resp.headers());
        w.side_file(meta_file.to_path_buf(), serde_json::to_vec(&*meta).unwrap_or_default())
            .await?;

        let mut progressed = false;
        let mut window = (Instant::now(), 0u64);
        loop {
            let chunk = tokio::select! {
                () = self.cancel.cancelled() => {
                    w.sync().await?;
                    return Err(LocalError::Cancelled);
                }
                c = resp.chunk() => c,
            };
            match chunk {
                Ok(Some(b)) => {
                    let n = b.len() as u64;
                    if *len + n > remote.size {
                        // 比钉死的大小还多：内容不对，从头再来
                        self.restart(w, len).await?;
                        return Ok(Attempt::Fatal(DownloadErrorCode::Server));
                    }
                    w.write(b).await?;
                    *len += n;
                    progressed = true;
                    self.meter.add(n);
                    window.1 += n;
                    let elapsed = window.0.elapsed();
                    if elapsed >= self.tuning.slow_window {
                        let bps = window.1 as f64 / elapsed.as_secs_f64();
                        #[allow(clippy::cast_precision_loss)]
                        let slow = bps < self.tuning.slow_bytes_per_sec as f64;
                        if slow && has_next && *len < remote.size {
                            w.sync().await?;
                            return Ok(Attempt::Slow);
                        }
                        window = (Instant::now(), 0);
                    }
                }
                Ok(None) => {
                    w.sync().await?;
                    log::info!(
                        "download: {target} {} B in {} ms",
                        *len - start,
                        started.elapsed().as_millis()
                    );
                    return Ok(if *len == remote.size {
                        Attempt::Complete
                    } else {
                        // 连接提前结束
                        Attempt::Transient {
                            code: DownloadErrorCode::Network,
                            progressed,
                        }
                    });
                }
                Err(_) => {
                    // 断流（例如 HTTP/2 流没有正常关闭、读超时）：保留已写的部分，下次从这里续传
                    w.sync().await?;
                    log::warn!("download: {target} stream interrupted after {} B", *len - start);
                    return Ok(Attempt::Transient {
                        code: DownloadErrorCode::Network,
                        progressed,
                    });
                }
            }
        }
    }

    async fn restart(&mut self, w: &mut PartWriter, len: &mut u64) -> Result<(), LocalError> {
        w.reset().await?;
        self.meter.sub(*len);
        *len = 0;
        Ok(())
    }

    /// 取一段字节（`start..=end`），要求 206 且区间相符；服务器不支持 Range（回 200）时返回 None（调用方退回整包下载）
    pub(crate) async fn range(
        &mut self,
        url: &str,
        start: u64,
        end: u64,
        total: u64,
    ) -> Result<Option<Vec<u8>>, LocalError> {
        if !reqwest::Url::parse(url).is_ok_and(|u| allowed(&u, self.tuning.direct)) {
            return Err(LocalError::Download(DownloadErrorCode::Server));
        }
        let req = self.client.get(url).header(RANGE, format!("bytes={start}-{end}"));
        let resp = tokio::select! {
            () = self.cancel.cancelled() => return Err(LocalError::Cancelled),
            r = req.send() => r.map_err(|_| LocalError::Download(DownloadErrorCode::Network))?,
        };
        let mut resp = resp;
        match resp.status() {
            StatusCode::PARTIAL_CONTENT => {}
            StatusCode::OK => return Ok(None),
            s if s.is_server_error() || s == StatusCode::FORBIDDEN => {
                return Err(LocalError::Download(DownloadErrorCode::Server));
            }
            _ => return Ok(None),
        }
        let ok = resp
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_content_range)
            .is_some_and(|(s, e, t)| s == start && e == end && t.is_none_or(|t| t == total));
        if !ok {
            return Ok(None);
        }
        let want = usize::try_from(end - start + 1).map_err(|_| LocalError::Protocol("range too large"))?;
        let mut out = Vec::with_capacity(want);
        loop {
            let chunk = tokio::select! {
                () = self.cancel.cancelled() => return Err(LocalError::Cancelled),
                c = resp.chunk() => c.map_err(|_| LocalError::Download(DownloadErrorCode::Network))?,
            };
            let Some(b) = chunk else { break };
            if out.len() + b.len() > want {
                return Err(LocalError::Download(DownloadErrorCode::Server));
            }
            out.extend_from_slice(&b);
            self.meter.add(b.len() as u64);
        }
        if out.len() != want {
            return Err(LocalError::Download(DownloadErrorCode::Network));
        }
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range() {
        assert_eq!(parse_content_range("bytes 0-0/55571"), Some((0, 0, Some(55571))));
        assert_eq!(parse_content_range("bytes 100-199/*"), Some((100, 199, None)));
        assert_eq!(parse_content_range("bytes 5-1/10"), None);
        assert_eq!(parse_content_range("items 0-1/2"), None);
    }

    #[test]
    fn only_https_or_loopback_http_in_tests() {
        let ok = |s: &str, direct: bool| allowed(&reqwest::Url::parse(s).expect("url"), direct);
        assert!(ok("https://cdn-lfs-cn-1.modelscope.cn/x", false));
        assert!(!ok("http://127.0.0.1:1234/x", false), "正式下载不接受明文");
        assert!(!ok("http://localhost:1234/x", false));
        assert!(ok("http://127.0.0.1:1234/x", true));
        assert!(ok("http://localhost:1234/x", true));
        assert!(!ok("http://example.com/x", true));
        assert!(!ok("ftp://example.com/x", true));
    }

    #[test]
    fn log_target_drops_path_and_query() {
        let u = reqwest::Url::parse("https://cdn.example.com/prod/lfs/ab/cd?auth_key=secret").expect("url");
        assert_eq!(log_target(&u), "cdn.example.com/prod");
    }

    #[test]
    fn meter_throttles_and_reports_speed() {
        let seen = std::sync::Mutex::new(Vec::new());
        let cb = |p: Progress| seen.lock().expect("lock").push(p);
        let mut m = Meter::new(&cb, 1000, 0);
        for _ in 0..100 {
            m.add(1);
        }
        let n = seen.lock().expect("lock").len();
        // 第一次立即报告，之后每 1%（10 字节）一次
        assert!((10..=12).contains(&n), "{n}");
        m.sub(50);
        assert_eq!(m.done(), 50);
        // 总量只增不减：退回整包后已完成的字节不倒退，之后能走到新的总量
        m.grow_total(500);
        assert_eq!(m.done(), 50);
        m.add(1_450);
        assert_eq!(m.done(), 1_500);
        assert_eq!(
            seen.lock().expect("lock").last().map(|p| (p.done, p.total)),
            Some((1_500, 1_500))
        );
        m.verifying();
        assert_eq!(
            seen.lock().expect("lock").last().map(|p| p.stage),
            Some(Stage::Verifying)
        );
    }
}
