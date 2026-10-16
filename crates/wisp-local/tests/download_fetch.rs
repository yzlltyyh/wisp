//! 下载器的网络行为：断点续传（Range + If-Range）、200 + Content-Range、服务器不支持续传、重定向与签名过期、断流续传、
//! 哈希不符、换源与低速换源、取消、多源探测、zip 区间提取。全部用本机的模拟服务器（wiremock 与一个按脚本断流的小服务器），
//! 不访问网络。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, header_exists, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};
use wisp_local::LocalError;
use wisp_local::download::{Fetcher, Progress, Ranged, Remote, Tuning};
use wisp_types::DownloadErrorCode;

fn sha256(data: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, data)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 可复现的伪随机内容
fn payload(n: usize) -> Vec<u8> {
    let mut x: u32 = 0x1234_5678;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

fn quick() -> Tuning {
    Tuning {
        retry_backoff: Duration::from_millis(10),
        probe_timeout: Duration::from_millis(500),
        direct: true,
        ..Tuning::default()
    }
}

/// 按 `Range` 头给出 206 的模拟文件；带 ETag，`If-Range` 不符时回 200 整个文件
struct RangeFile {
    data: Vec<u8>,
    etag: &'static str,
    hits: Arc<AtomicUsize>,
}

impl Respond for RangeFile {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        self.hits.fetch_add(1, Ordering::SeqCst);
        let total = self.data.len();
        let range = req
            .headers
            .get("range")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("bytes="))
            .and_then(|r| r.split_once('-'))
            .and_then(|(a, b)| {
                let a: usize = a.parse().ok()?;
                let b: usize = if b.is_empty() { total - 1 } else { b.parse().ok()? };
                Some((a, b.min(total - 1)))
            });
        let if_range_ok = req
            .headers
            .get("if-range")
            .is_none_or(|v| v.to_str().ok() == Some(self.etag));
        match range {
            Some((a, b)) if if_range_ok && a <= b => ResponseTemplate::new(206)
                .insert_header("etag", self.etag)
                .insert_header("content-range", format!("bytes {a}-{b}/{total}").as_str())
                .set_body_bytes(self.data[a..=b].to_vec()),
            _ => ResponseTemplate::new(200)
                .insert_header("etag", self.etag)
                .set_body_bytes(self.data.clone()),
        }
    }
}

fn range_file(data: &[u8]) -> (RangeFile, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    (
        RangeFile {
            data: data.to_vec(),
            etag: "\"abc123\"",
            hits: Arc::clone(&hits),
        },
        hits,
    )
}

struct Run {
    progress: Arc<parking_lot::Mutex<Vec<Progress>>>,
}

impl Run {
    fn new() -> Self {
        Self {
            progress: Arc::new(parking_lot::Mutex::new(Vec::new())),
        }
    }
}

async fn fetch(
    urls: &[&str],
    data: &[u8],
    part: &Path,
    cancel: CancellationToken,
    tuning: Tuning,
    run: &Run,
) -> Result<(), LocalError> {
    let sha = sha256(data);
    let p = Arc::clone(&run.progress);
    let cb = move |x: Progress| p.lock().push(x);
    let mut f = Fetcher::new(&cb, data.len() as u64, 0, cancel, tuning)?;
    f.file(
        &Remote {
            urls,
            size: data.len() as u64,
            sha256: &sha,
        },
        part,
    )
    .await
}

#[tokio::test]
async fn downloads_verifies_and_reports_progress() {
    let server = MockServer::start().await;
    let data = payload(300_000);
    let (rf, _) = range_file(&data);
    Mock::given(method("GET"))
        .and(path("/f"))
        .respond_with(rf)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("dir");
    let part = dir.path().join("x.part");
    let url = format!("{}/f", server.uri());
    let run = Run::new();
    fetch(&[&url], &data, &part, CancellationToken::new(), quick(), &run)
        .await
        .expect("download");
    assert_eq!(std::fs::read(&part).expect("read"), data);
    assert!(!dir.path().join("x.part.meta").exists(), "完成后续传信息删除");
    let p = run.progress.lock();
    assert!(!p.is_empty());
    assert!(p.iter().any(|x| x.done == data.len() as u64));
    // 单个文件不报「校验中」（哈希是边下边算的）：多文件的模型只在全部文件之后由任务报一次，进度条不在文件之间闪烁
    assert!(p.iter().all(|x| x.stage == wisp_local::download::Stage::Downloading));
}

#[tokio::test]
async fn resumes_with_range_and_if_range() {
    let server = MockServer::start().await;
    let data = payload(200_000);
    let (rf, hits) = range_file(&data);
    // 只有带 If-Range 的续传请求能匹配：证明续传带上了上次的 ETag
    Mock::given(method("GET"))
        .and(path("/f"))
        .and(header("range", "bytes=50000-"))
        .and(header("if-range", "\"abc123\""))
        .respond_with(rf)
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("dir");
    let part = dir.path().join("x.part");
    std::fs::write(&part, &data[..50_000]).expect("seed");
    std::fs::write(
        dir.path().join("x.part.meta"),
        format!(
            r#"{{"sha256":"{}","etag":"\"abc123\"","last_modified":null}}"#,
            sha256(&data)
        ),
    )
    .expect("meta");
    let url = format!("{}/f", server.uri());
    fetch(&[&url], &data, &part, CancellationToken::new(), quick(), &Run::new())
        .await
        .expect("resume");
    assert_eq!(std::fs::read(&part).expect("read"), data);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn server_without_range_support_restarts_from_zero() {
    let server = MockServer::start().await;
    let data = payload(100_000);
    Mock::given(method("GET"))
        .and(path("/f"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(data.clone()))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("dir");
    let part = dir.path().join("x.part");
    // 一份「旧的」部分文件（内容随便），服务器不理 Range 时必须丢掉它从头写
    std::fs::write(&part, vec![0u8; 30_000]).expect("seed");
    let url = format!("{}/f", server.uri());
    fetch(&[&url], &data, &part, CancellationToken::new(), quick(), &Run::new())
        .await
        .expect("download");
    assert_eq!(std::fs::read(&part).expect("read"), data);
}

#[tokio::test]
async fn ok_with_content_range_is_accepted() {
    // ModelScope 的小文件：Range: bytes=0-0 也回 200，并带 Content-Range
    let server = MockServer::start().await;
    let data = payload(5_000);
    Mock::given(method("GET"))
        .and(path("/small"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-range", format!("bytes 0-4999/{}", data.len()).as_str())
                .set_body_bytes(data.clone()),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("dir");
    let part = dir.path().join("s.part");
    let url = format!("{}/small", server.uri());
    fetch(&[&url], &data, &part, CancellationToken::new(), quick(), &Run::new())
        .await
        .expect("download");
    assert_eq!(std::fs::read(&part).expect("read"), data);
}

/// 第一次回 403（CDN 签名过期），之后正常
struct ExpiredOnce {
    data: Vec<u8>,
    calls: AtomicUsize,
}

impl Respond for ExpiredOnce {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            ResponseTemplate::new(403)
        } else {
            ResponseTemplate::new(200).set_body_bytes(self.data.clone())
        }
    }
}

#[tokio::test]
async fn follows_redirects_and_re_resolves_after_expired_signature() {
    let server = MockServer::start().await;
    let data = payload(64_000);
    Mock::given(method("GET"))
        .and(path("/resolve/model.gguf"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/cdn/obj?auth_key=signed", server.uri()).as_str()),
        )
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/cdn/obj"))
        .respond_with(ExpiredOnce {
            data: data.clone(),
            calls: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("dir");
    let part = dir.path().join("m.part");
    let url = format!("{}/resolve/model.gguf", server.uri());
    fetch(&[&url], &data, &part, CancellationToken::new(), quick(), &Run::new())
        .await
        .expect("download");
    assert_eq!(std::fs::read(&part).expect("read"), data);
}

#[tokio::test]
async fn falls_back_to_the_next_source() {
    let server = MockServer::start().await;
    let data = payload(10_000);
    Mock::given(method("GET"))
        .and(path("/a"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let (rf, _) = range_file(&data);
    Mock::given(method("GET"))
        .and(path("/b"))
        .respond_with(rf)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("dir");
    let part = dir.path().join("x.part");
    let (a, b) = (format!("{}/a", server.uri()), format!("{}/b", server.uri()));
    fetch(&[&a, &b], &data, &part, CancellationToken::new(), quick(), &Run::new())
        .await
        .expect("download");
    assert_eq!(std::fs::read(&part).expect("read"), data);
}

#[tokio::test]
async fn checksum_mismatch_retries_once_then_fails() {
    let data = payload(10_000);
    let mut wrong = data.clone();
    wrong[123] ^= 0xff;
    let bad = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wrong.clone()))
        .expect(2)
        .mount(&bad)
        .await;
    let dir = tempfile::tempdir().expect("dir");
    let part = dir.path().join("x.part");
    let bad_url = format!("{}/bad", bad.uri());
    let err = fetch(
        &[&bad_url],
        &data,
        &part,
        CancellationToken::new(),
        quick(),
        &Run::new(),
    )
    .await
    .expect_err("mismatch");
    assert!(
        matches!(err, LocalError::Download(DownloadErrorCode::ChecksumMismatch)),
        "{err:?}"
    );
    assert!(!part.exists(), "不符的文件被删除");
}

#[tokio::test]
async fn checksum_mismatch_on_one_source_switches_to_the_other() {
    let data = payload(10_000);
    let mut wrong = data.clone();
    wrong[7] ^= 0xff;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/bad"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wrong))
        .expect(1)
        .mount(&server)
        .await;
    let (rf, hits) = range_file(&data);
    Mock::given(method("GET"))
        .and(path("/good"))
        .respond_with(rf)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("dir");
    let part = dir.path().join("x.part");
    let (bad, good) = (format!("{}/bad", server.uri()), format!("{}/good", server.uri()));
    fetch(
        &[&bad, &good],
        &data,
        &part,
        CancellationToken::new(),
        quick(),
        &Run::new(),
    )
    .await
    .expect("second source");
    assert_eq!(std::fs::read(&part).expect("read"), data);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn persistent_server_errors_fail_with_server_code() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let data = payload(1_000);
    let dir = tempfile::tempdir().expect("dir");
    let url = format!("{}/x", server.uri());
    let err = fetch(
        &[&url],
        &data,
        &dir.path().join("x.part"),
        CancellationToken::new(),
        quick(),
        &Run::new(),
    )
    .await
    .expect_err("503");
    assert!(
        matches!(err, LocalError::Download(DownloadErrorCode::Server)),
        "{err:?}"
    );
}

#[tokio::test]
async fn unreachable_source_is_a_network_error() {
    // 一个刚释放的端口：连接被拒绝
    let port = TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("port")
        .port();
    let dir = tempfile::tempdir().expect("dir");
    let url = format!("http://127.0.0.1:{port}/x");
    let err = fetch(
        &[&url],
        &payload(10),
        &dir.path().join("x.part"),
        CancellationToken::new(),
        quick(),
        &Run::new(),
    )
    .await
    .expect_err("refused");
    assert!(
        matches!(err, LocalError::Download(DownloadErrorCode::Network)),
        "{err:?}"
    );
}

#[tokio::test]
async fn http_to_a_public_host_is_refused() {
    let data = payload(10);
    let dir = tempfile::tempdir().expect("dir");
    let err = fetch(
        &["http://example.invalid/x"],
        &data,
        &dir.path().join("x.part"),
        CancellationToken::new(),
        quick(),
        &Run::new(),
    )
    .await
    .expect_err("http");
    assert!(
        matches!(err, LocalError::Download(DownloadErrorCode::Server)),
        "{err:?}"
    );
}

/// 按脚本应答的极简 HTTP/1.1 服务器：每个连接取一条脚本——`(起点, 发多少字节, 每块间隔)`，发完就断开
/// （声明的 Content-Length 是到文件末尾的长度，所以提前断开就是「断流」）
fn scripted_server(data: Vec<u8>, script: Vec<(Option<usize>, Duration)>) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let conns = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&conns);
    std::thread::spawn(move || {
        for (i, stream) in listener.incoming().enumerate() {
            let Ok(mut s) = stream else { break };
            c.fetch_add(1, Ordering::SeqCst);
            let mut req = Vec::new();
            let mut buf = [0u8; 1024];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => req.extend_from_slice(&buf[..n]),
                }
            }
            let text = String::from_utf8_lossy(&req).to_ascii_lowercase();
            let start = text
                .lines()
                .find_map(|l| l.strip_prefix("range: bytes="))
                .and_then(|r| r.trim().trim_end_matches('-').parse::<usize>().ok())
                .unwrap_or(0);
            let (limit, gap) = script.get(i).copied().unwrap_or((None, Duration::ZERO));
            let total = data.len();
            let head = if start > 0 {
                format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{}/{total}\r\nETag: \"e\"\r\nConnection: close\r\n\r\n",
                    total - start,
                    total - 1
                )
            } else {
                format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nETag: \"e\"\r\nConnection: close\r\n\r\n")
            };
            let _ = s.write_all(head.as_bytes());
            let end = limit.map_or(total, |n| (start + n).min(total));
            for chunk in data[start..end].chunks(4096) {
                if s.write_all(chunk).is_err() {
                    break;
                }
                if !gap.is_zero() {
                    std::thread::sleep(gap);
                }
            }
            let _ = s.flush();
        }
    });
    (format!("http://{addr}/f"), conns)
}

#[tokio::test]
async fn interrupted_stream_resumes_where_it_stopped() {
    let data = payload(400_000);
    // 第一个连接只发 150 KB 就断开（模拟 HTTP/2 流没有正常关闭），第二个连接从断点续传
    let (url, conns) = scripted_server(
        data.clone(),
        vec![(Some(150_000), Duration::ZERO), (None, Duration::ZERO)],
    );
    let dir = tempfile::tempdir().expect("dir");
    let part = dir.path().join("x.part");
    fetch(&[&url], &data, &part, CancellationToken::new(), quick(), &Run::new())
        .await
        .expect("resume");
    assert_eq!(std::fs::read(&part).expect("read"), data);
    assert_eq!(conns.load(Ordering::SeqCst), 2, "断流后只续传一次");
}

#[tokio::test]
async fn slow_source_is_abandoned_for_the_next_one() {
    let data = payload(200_000);
    // 第一个源每 4 KB 停 100 ms（约 40 KB/s）
    let (slow, _) = scripted_server(data.clone(), vec![(None, Duration::from_millis(100)); 4]);
    let server = MockServer::start().await;
    let (rf, hits) = range_file(&data);
    Mock::given(method("GET"))
        .and(path("/fast"))
        .respond_with(rf)
        .mount(&server)
        .await;
    let fast = format!("{}/fast", server.uri());
    let tuning = Tuning {
        slow_window: Duration::from_millis(400),
        slow_bytes_per_sec: 100_000,
        ..quick()
    };
    let dir = tempfile::tempdir().expect("dir");
    let part = dir.path().join("x.part");
    fetch(
        &[&slow, &fast],
        &data,
        &part,
        CancellationToken::new(),
        tuning,
        &Run::new(),
    )
    .await
    .expect("switch");
    assert_eq!(std::fs::read(&part).expect("read"), data);
    assert_eq!(hits.load(Ordering::SeqCst), 1, "在第二个源上续传");
}

#[tokio::test]
async fn cancel_keeps_the_partial_file() {
    let data = payload(400_000);
    let (url, _) = scripted_server(data.clone(), vec![(None, Duration::from_millis(30)); 2]);
    let dir = tempfile::tempdir().expect("dir");
    let part = dir.path().join("x.part");
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        c.cancel();
    });
    let err = fetch(&[&url], &data, &part, cancel, quick(), &Run::new())
        .await
        .expect_err("cancelled");
    assert!(matches!(err, LocalError::Cancelled), "{err:?}");
    let kept = std::fs::read(&part).expect("part");
    assert!(!kept.is_empty() && kept.len() < data.len());
    assert_eq!(&data[..kept.len()], kept.as_slice(), "已写的部分与原文一致");
    assert!(dir.path().join("x.part.meta").exists(), "续传信息保留");
}

#[tokio::test]
async fn probing_ranks_the_fastest_source_first() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/slow"))
        .and(header_exists("range"))
        .respond_with(ResponseTemplate::new(206).set_delay(Duration::from_millis(300)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fast"))
        .respond_with(ResponseTemplate::new(206))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/down"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let cb = |_: Progress| {};
    let f = Fetcher::new(&cb, 1, 0, CancellationToken::new(), quick()).expect("fetcher");
    let (slow, fast, down) = (
        format!("{}/slow", server.uri()),
        format!("{}/fast", server.uri()),
        format!("{}/down", server.uri()),
    );
    assert_eq!(f.rank(&[&slow, &fast]).await, vec![1, 0]);
    assert_eq!(f.rank(&[&down, &slow]).await, vec![1, 0], "失败的源排到后面");
    assert_eq!(f.rank(&[&down, &down]).await, vec![0, 1], "全部失败时保持原顺序");
}

fn zip_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in entries {
            z.start_file(*name, opts).expect("start");
            z.write_all(data).expect("write");
        }
        z.finish().expect("finish");
    }
    buf.into_inner()
}

#[tokio::test]
async fn zip_entry_is_fetched_by_range_only() {
    let dll = payload(500_000);
    let big = payload(2_000_000);
    let archive = zip_with(&[
        ("ort/include/x.h", b"header"),
        ("ort/lib/onnxruntime.dll", &dll),
        ("ort/lib/onnxruntime.pdb", &big),
    ]);
    let server = MockServer::start().await;
    let (rf, _) = range_file(&archive);
    Mock::given(method("GET"))
        .and(path("/ort.zip"))
        .respond_with(rf)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("dir");
    let dest = dir.path().join("onnxruntime.dll");
    let seen = Arc::new(parking_lot::Mutex::new(0u64));
    let s2 = Arc::clone(&seen);
    let cb = move |p: Progress| *s2.lock() = p.done;
    let mut f = Fetcher::new(&cb, archive.len() as u64, 0, CancellationToken::new(), quick()).expect("fetcher");
    let url = format!("{}/ort.zip", server.uri());
    let r = f
        .zip_entry(
            &url,
            archive.len() as u64,
            "ort/lib/onnxruntime.dll",
            &sha256(&dll),
            &dest,
        )
        .await
        .expect("ranged");
    assert_eq!(r, Ranged::Done);
    assert_eq!(std::fs::read(&dest).expect("read"), dll);
    let fetched = *seen.lock();
    assert!(fetched < archive.len() as u64 / 2, "只取了一个条目（{fetched} B）");

    // 哈希不符
    let err = f
        .zip_entry(
            &url,
            archive.len() as u64,
            "ort/lib/onnxruntime.dll",
            &"0".repeat(64),
            &dest,
        )
        .await
        .expect_err("mismatch");
    assert!(matches!(err, LocalError::Download(DownloadErrorCode::ChecksumMismatch)));
    // 包里没有这个条目
    assert_eq!(
        f.zip_entry(&url, archive.len() as u64, "ort/lib/missing.dll", &sha256(&dll), &dest)
            .await
            .expect("missing"),
        Ranged::Unsupported
    );
}

#[tokio::test]
async fn zip_entry_falls_back_when_range_is_ignored() {
    let archive = zip_with(&[("a.dll", b"abc")]);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(archive.clone()))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("dir");
    let cb = |_: Progress| {};
    let mut f = Fetcher::new(&cb, 1, 0, CancellationToken::new(), quick()).expect("fetcher");
    let url = format!("{}/a.zip", server.uri());
    let r = f
        .zip_entry(
            &url,
            archive.len() as u64,
            "a.dll",
            &sha256(b"abc"),
            &dir.path().join("a.dll"),
        )
        .await
        .expect("fallback");
    assert_eq!(r, Ranged::Unsupported);
}
