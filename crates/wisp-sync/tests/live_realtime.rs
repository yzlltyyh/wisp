//! 实时同步的浸泡测试（默认忽略）：两台设备经真实 WebDAV 服务器，B 用桌面端同一个规划器（`wisp_sync::realtime::plan`）
//! 与同一套只读探测（`Engine::peek`，发现变化时 `sync_cycle_with` 复用这次的列表）按各档真实计时地运行；traffic 段里
//! A 按泊松分布复制（默认平均每分钟 1 条，其中约 1/10 是 200 KB 的图片）并像实时模式那样 0.5 s 后推送，测「A 复制 →
//! B 拿到」的延迟。
//!
//! 运行（账号只从环境变量读；测试在地址之下新建独占的 `e2e-<随机 id>/`，结束时——成功、失败或超时——删除）：
//!
//! ```text
//! WISP_SYNC_TEST_URL=https://… WISP_SYNC_TEST_USER=… WISP_SYNC_TEST_PASS=… \
//!   cargo test -p wisp-sync --release --locked --test live_realtime -- --ignored --nocapture
//! ```
//!
//! 可选环境变量：
//! - `WISP_SOAK_PHASES`：各段与秒数，默认 `hot:120,active:120,quiet:120,traffic:300`。段名：`hot`（剪贴板面板开着）、
//!   `active`（30 s 内有输入）、`quiet`（输入空闲 1 分钟）、`away`（离开 10 分钟，应当一个请求都没有）、`traffic`
//!   （活跃，A 在复制；看到改动后的 60 s 规划器自己会进热档）；
//! - `WISP_SOAK_COPY_EVERY_S`：traffic 段 A 平均多少秒复制一次，默认 60；
//! - `WISP_SOAK_A_USER` / `WISP_SOAK_A_PASS`：A 用另一个账号登录同一个服务器（本机回环 Apache 上据此在访问日志里区分两台
//!   设备的请求与字节；真实服务不用设）。
//!
//! 服务商的最小间隔与请求额度按地址的主机名取（与桌面端相同），坚果云上不会比它允许的更频繁。
//!
//! 线程：B 跑在 2 个工作线程的多线程运行时上（与应用一致），工作线程与阻塞线程都叫 `soak-b`；A 跑在另一个线程的
//! 单线程运行时上（`soak-a`），只在复制之后推送。按线程名就能把进程的 CPU 分给两台设备。
//!
//! 输出（`--nocapture`）：开始一行 `SOAK start {…}`（进程号），每段开始一行 `SOAK begin {…}`、结束一行
//! `SOAK phase {…}`，最后一行 `SOAK done {…}`。只有计数、时间与字节，不含地址、账号与内容。

// 浸泡测试的结果就是打印出来的这些行（外部的采样脚本按 `SOAK begin` 对齐窗口）
#![allow(clippy::print_stdout, clippy::print_stderr)]

mod live;
mod sim;

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use live::Account;
use parking_lot::Mutex;
use serde_json::{Value, json};
use sim::{Device, State, noop_progress, now_ms};
use wisp_sync::realtime::{self, Inputs, Plan, Provider, QuotaWindow};
use wisp_sync::remote::memory::Op;
use wisp_sync::{CycleReason, Entry, Peek, PutInfo, Remote, RemoteError, SyncError, WebDav};

/// A 复制之后多久推送（实时模式下桌面端的本机去抖）
const PUSH_DEBOUNCE: Duration = Duration::from_millis(500);
/// traffic 段里图片的比例（百分比）与大小
const IMAGE_PERCENT: u64 = 10;
const IMAGE_BYTES: usize = 200 * 1024;
/// A 开始时已有的历史条数（让首轮与压缩有真实的分量）
const SEED_ITEMS: usize = 200;
/// 「慢」的列目录请求：超过中位数的这么多倍（多出几个往返，通常是连接被服务器断开后重新建立）
const SLOW_FACTOR: u32 = 3;

#[test]
#[ignore = "需要真实 WebDAV 账号"]
fn realtime_soak() {
    let account = Account::from_env();
    let phases = phases_from_env();
    let copy_every = std::env::var("WISP_SOAK_COPY_EVERY_S")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|s| *s > 0.0)
        .unwrap_or(60.0);
    let a_account = match (std::env::var("WISP_SOAK_A_USER"), std::env::var("WISP_SOAK_A_PASS")) {
        (Ok(user), Ok(pass)) => Account {
            url: account.url.clone(),
            user,
            pass,
        },
        _ => account.clone(),
    };
    let total: u64 = phases.iter().map(|p| p.secs).sum();
    // 准备（建 vault、首轮）与清理另留 15 分钟
    let limit = Duration::from_secs(total + 15 * 60);
    println!(
        "SOAK start {}",
        json!({"pid": std::process::id(), "phases": phases.iter().map(|p| json!({"phase": p.name.as_str(), "secs": p.secs})).collect::<Vec<_>>(), "copy_every_s": copy_every})
    );
    // B 的运行时：2 个工作线程（与应用一致）
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("soak-b")
        .enable_all()
        .build()
        .expect("运行时");
    let b_account = account.clone();
    rt.block_on(live::with_scratch(&account, limit, move |server_url| {
        soak(b_account, a_account, server_url, phases, copy_every)
    }));
}

// ---------------------------------------------------------------------------------------------
// 段
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Name {
    Hot,
    Active,
    Quiet,
    Away,
    Traffic,
}

impl Name {
    fn parse(s: &str) -> Name {
        match s {
            "hot" => Name::Hot,
            "active" => Name::Active,
            "quiet" => Name::Quiet,
            "away" => Name::Away,
            "traffic" => Name::Traffic,
            other => panic!("WISP_SOAK_PHASES 里有未知的段名：{other}"),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Name::Hot => "hot",
            Name::Active => "active",
            Name::Quiet => "quiet",
            Name::Away => "away",
            Name::Traffic => "traffic",
        }
    }

    /// 这一段里「距最近一次输入多久」
    fn input_idle_ms(self) -> u64 {
        match self {
            Name::Hot | Name::Active | Name::Traffic => 1_000,
            Name::Quiet => 60_000,
            Name::Away => 10 * 60_000,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Phase {
    name: Name,
    secs: u64,
}

fn phases_from_env() -> Vec<Phase> {
    let spec = std::env::var("WISP_SOAK_PHASES").unwrap_or_else(|_| "hot:120,active:120,quiet:120,traffic:300".into());
    spec.split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            let (name, secs) = s.trim().split_once(':').expect("段的写法是 名字:秒数");
            Phase {
                name: Name::parse(name),
                secs: secs.parse().expect("秒数"),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// 计时的远端：记下每个请求的种类与耗时（微秒）
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
struct Timed<R> {
    inner: R,
    log: Arc<Mutex<Vec<(Op, u32)>>>,
}

impl<R> Timed<R> {
    fn new(inner: R) -> Timed<R> {
        Timed {
            inner,
            log: Arc::default(),
        }
    }

    fn take(&self) -> Vec<(Op, u32)> {
        std::mem::take(&mut *self.log.lock())
    }

    async fn time<T>(&self, op: Op, fut: impl Future<Output = Result<T, RemoteError>>) -> Result<T, RemoteError> {
        let started = Instant::now();
        let out = fut.await;
        let us = u32::try_from(started.elapsed().as_micros()).unwrap_or(u32::MAX);
        self.log.lock().push((op, us));
        out
    }
}

impl<R: Remote> Remote for Timed<R> {
    async fn list(&self, dir: &str) -> Result<Option<Vec<Entry>>, RemoteError> {
        self.time(Op::List, self.inner.list(dir)).await
    }

    async fn get(&self, path: &str) -> Result<Option<Bytes>, RemoteError> {
        self.time(Op::Get, self.inner.get(path)).await
    }

    async fn get_to_file(&self, path: &str, dest: &Path) -> Result<bool, RemoteError> {
        self.time(Op::Get, self.inner.get_to_file(path, dest)).await
    }

    async fn put(&self, path: &str, body: Bytes) -> Result<PutInfo, RemoteError> {
        self.time(Op::Put, self.inner.put(path, body)).await
    }

    async fn put_file(&self, path: &str, src: &Path, len: u64) -> Result<PutInfo, RemoteError> {
        self.time(Op::Put, self.inner.put_file(path, src, len)).await
    }

    async fn delete(&self, path: &str) -> Result<(), RemoteError> {
        self.time(Op::Delete, self.inner.delete(path)).await
    }

    async fn mkcol(&self, path: &str) -> Result<(), RemoteError> {
        self.time(Op::Mkcol, self.inner.mkcol(path)).await
    }
}

type LiveDevice = Device<Timed<WebDav>>;

fn device(account: &Account, server_url: &str, label: &str, n: u64) -> LiveDevice {
    let dav = WebDav::new(server_url, &account.user, &account.pass).expect("服务器地址");
    // 与桌面端实时同步运行时一样：连接池保留 90 s，安静档的间隔之内不重新握手
    dav.set_keep_alive(true);
    Device::with_remote(Timed::new(dav), label, n, State::with_defaults())
}

/// 引擎错误 → 带步骤名的失败。错误只含固定词汇（种类、HTTP 状态码），不含地址、账号与内容
fn step<T>(what: &str, result: Result<T, SyncError>) -> T {
    result.unwrap_or_else(|e| panic!("{what}：{e}（{}，{:?}）", e.kind().as_str(), e.detail()))
}

// ---------------------------------------------------------------------------------------------
// 场景
// ---------------------------------------------------------------------------------------------

async fn soak(b_account: Account, a_account: Account, server_url: String, phases: Vec<Phase>, copy_every: f64) {
    let mut a = device(&a_account, &server_url, "a", 0xa);
    let mut b = device(&b_account, &server_url, "b", 0xb);
    let mut rng = Rng::new(now_ms().unsigned_abs() | 1);
    for i in 0..SEED_ITEMS {
        a.store.copy(&format!("soak seed {i} {:016x}", rng.next()));
    }
    step("准备：A 连接", a.connect().await);
    step("准备：A 首轮", a.sync_reason(CycleReason::Connect).await);
    step("准备：A 上传", a.sync_fully().await);
    step("准备：B 连接", b.connect().await);
    step("准备：B 首轮", b.sync_reason(CycleReason::Connect).await);
    step("准备：B 取回", b.sync_fully().await);
    step("准备：A 再同步", a.sync_fully().await);
    // 准备阶段的请求不计入
    a.remote.inner.take();
    b.remote.inner.take();

    let host = wisp_sync::validate_server_url(&server_url)
        .ok()
        .and_then(|u| u.host_str().map(Provider::from_host))
        .unwrap_or(Provider::Other);
    let t0 = Instant::now();
    let mut windows = Vec::new();
    let mut at = t0;
    for p in &phases {
        let end = at + Duration::from_secs(p.secs);
        windows.push((p.name, at, end));
        at = end;
    }

    // A：另一个线程上的单线程运行时，只在 traffic 段复制并推送
    let (tx, rx) = tokio::sync::oneshot::channel();
    let a_windows: Vec<(Instant, Instant)> = windows
        .iter()
        .filter(|(n, ..)| *n == Name::Traffic)
        .map(|(_, s, e)| (*s, *e))
        .collect();
    let a_seed = rng.next();
    std::thread::Builder::new()
        .name("soak-a".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .thread_name("soak-a")
                .enable_all()
                .build()
                .expect("运行时");
            let tally = rt.block_on(pusher(a, a_windows, copy_every, a_seed));
            let _ = tx.send(tally);
        })
        .expect("A 的线程");

    let mut poller = Poller {
        dev: b,
        provider: host,
        quota: QuotaWindow::default(),
        last_poll: None,
        last_change: None,
        failures: 0,
        rng,
        jitter: 0.0,
    };
    let mut all_latency = Vec::new();
    let mut summary = Vec::new();
    for (name, start, end) in &windows {
        tokio::time::sleep_until((*start).into()).await;
        let secs = end.duration_since(*start).as_secs();
        println!(
            "SOAK begin {}",
            json!({"phase": name.as_str(), "secs": secs, "at_ms": now_ms()})
        );
        let cpu0 = process_cpu_ms();
        let tally = poller.run_phase(*name, *end).await;
        let cpu = cpu0.zip(process_cpu_ms()).map(|(a, b)| b.saturating_sub(a));
        all_latency.extend_from_slice(&tally.latency);
        let line = tally.json(*name, secs, cpu);
        println!("SOAK phase {line}");
        summary.push(line);
    }
    let a_tally = rx.await.expect("A 的结果");
    let mut latency = all_latency;
    latency.sort_unstable();
    println!(
        "SOAK done {}",
        json!({
            "provider": format!("{host:?}"),
            "latency_ms": stats(&latency),
            "a": a_tally,
            "phases": summary.len(),
        })
    );
    assert_eq!(
        a_tally["copies"].as_u64().unwrap_or(0),
        a_tally["pushed_ok"].as_u64().unwrap_or(0),
        "A 的推送应当全部成功：{a_tally}"
    );
    let expected = a_tally["copies"].as_u64().unwrap_or(0);
    assert!(
        latency.len() as u64 >= expected,
        "B 应当拿到 A 的每一条复制：{} / {expected}",
        latency.len()
    );
}

// ---------------------------------------------------------------------------------------------
// B：规划器驱动的探测（与桌面端 `sync_svc/realtime.rs` 的循环同一套输入与动作）
// ---------------------------------------------------------------------------------------------

struct Poller {
    dev: LiveDevice,
    provider: Provider,
    quota: QuotaWindow,
    last_poll: Option<i64>,
    last_change: Option<i64>,
    failures: u32,
    rng: Rng,
    jitter: f64,
}

#[derive(Default)]
struct Tally {
    polls: u32,
    unchanged: u32,
    changed: u32,
    busy: u32,
    failures: u32,
    cycles: u32,
    bytes_up: u64,
    bytes_down: u64,
    /// 本段里 B 拿到的每条 A 的复制：拿到的时刻 − A 记下它的时刻（毫秒）
    latency: Vec<i64>,
    ops: Vec<(Op, u32)>,
}

impl Tally {
    fn json(&self, name: Name, secs: u64, cpu_ms: Option<u64>) -> Value {
        let mut lists: Vec<u32> = self
            .ops
            .iter()
            .filter(|(op, _)| *op == Op::List)
            .map(|(_, us)| *us)
            .collect();
        lists.sort_unstable();
        let median = lists.get(lists.len() / 2).copied().unwrap_or(0);
        let slow = lists
            .iter()
            .filter(|us| median > 0 && **us > median.saturating_mul(SLOW_FACTOR))
            .count();
        let count = |want: Op| self.ops.iter().filter(|(op, _)| *op == want).count();
        let mut latency = self.latency.clone();
        latency.sort_unstable();
        let hours = secs.max(1) as f64 / 3600.0;
        json!({
            "phase": name.as_str(),
            "secs": secs,
            "polls": self.polls,
            "unchanged": self.unchanged,
            "changed": self.changed,
            "busy": self.busy,
            "failures": self.failures,
            "cycles": self.cycles,
            "requests": self.ops.len(),
            "requests_per_h": (self.ops.len() as f64 / hours).round(),
            "by_op": {"list": count(Op::List), "get": count(Op::Get), "put": count(Op::Put), "delete": count(Op::Delete), "mkcol": count(Op::Mkcol)},
            "mean_interval_s": if self.polls > 0 { (secs as f64 / f64::from(self.polls) * 10.0).round() / 10.0 } else { 0.0 },
            "list_ms": {
                "p50": f64::from(median) / 1000.0,
                "p90": f64::from(lists.get(lists.len() * 9 / 10).copied().unwrap_or(0)) / 1000.0,
                "max": f64::from(lists.last().copied().unwrap_or(0)) / 1000.0,
                "slow": slow,
            },
            "payload_bytes": {"up": self.bytes_up, "down": self.bytes_down},
            "latency_ms": stats(&latency),
            "process_cpu_ms": cpu_ms,
        })
    }
}

impl Poller {
    fn inputs(&self, name: Name, now: i64) -> Inputs {
        let devices = 2;
        Inputs {
            wanted: true,
            now_ms: now,
            last_poll_ms: self.last_poll,
            input_idle_ms: Some(name.input_idle_ms()),
            screen_idle: false,
            last_activity_ms: None,
            hot_view: name == Name::Hot,
            last_change_ms: self.last_change,
            last_push_ms: None,
            on_battery: false,
            low_power: false,
            metered: false,
            floor_ms: self.provider.floor_ms(devices),
            quota_used: self.quota.used(now),
            quota_limit: self.provider.budget(devices),
            quota_frees_in_ms: self.quota.frees_in_ms(now),
            failures: self.failures,
            retry_after_ms: None,
            slow_until_ms: None,
            // macOS 上剪贴板记录开着时有「回到电脑前」事件：安静与离开档不必本机复查
            return_events: true,
            jitter: self.jitter,
        }
    }

    async fn run_phase(&mut self, name: Name, until: Instant) -> Tally {
        let mut t = Tally::default();
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match realtime::plan(&self.inputs(name, now_ms())) {
                Plan::Poll { .. } => {
                    self.last_poll = Some(now_ms());
                    self.jitter = self.rng.unit();
                    self.poll(&mut t).await;
                }
                Plan::Wait { ms, .. } => tokio::time::sleep(Duration::from_millis(ms).min(left)).await,
                Plan::Park { .. } => tokio::time::sleep(left).await,
                Plan::Stop => break,
            }
        }
        t.ops = self.dev.remote.inner.take();
        t
    }

    async fn poll(&mut self, t: &mut Tally) {
        t.polls += 1;
        self.quota.note(now_ms(), 1);
        let peek = self.dev.engine().peek(&self.dev.remote).await;
        match peek {
            Ok(Peek::Unchanged) => {
                t.unchanged += 1;
                self.failures = 0;
            }
            Ok(Peek::Changed(listing)) => {
                t.changed += 1;
                let cycle = self
                    .dev
                    .engine()
                    .sync_cycle_with(
                        &self.dev.remote,
                        &self.dev.store,
                        CycleReason::Poll,
                        &self.dev.options,
                        noop_progress(),
                        Some(listing),
                    )
                    .await;
                match cycle {
                    Ok(report) => {
                        let done = now_ms();
                        t.cycles += 1;
                        t.bytes_up += report.bytes_up;
                        t.bytes_down += report.bytes_down;
                        self.quota.note(done, report.requests);
                        if report.pulled > 0 {
                            self.last_change = Some(done);
                        }
                        t.latency
                            .extend(report.remote_clips.iter().map(|c| done - c.recorded_ms));
                        self.failures = 0;
                    }
                    Err(e) => {
                        eprintln!("SOAK b-cycle-failed {}", e.kind().as_str());
                        t.failures += 1;
                        self.failures += 1;
                    }
                }
            }
            Ok(Peek::Busy) => t.busy += 1,
            Ok(Peek::Unsupported) => panic!("服务器不提供可比较的签名，实时同步在这个服务器上不可用"),
            Err(e) => {
                eprintln!("SOAK b-peek-failed {}", e.kind().as_str());
                t.failures += 1;
                self.failures += 1;
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// A：traffic 段里按泊松分布复制，0.5 s 后推送
// ---------------------------------------------------------------------------------------------

async fn pusher(mut a: LiveDevice, windows: Vec<(Instant, Instant)>, copy_every: f64, seed: u64) -> Value {
    let mut rng = Rng::new(seed | 1);
    let (mut copies, mut images, mut pushed_ok, mut push_failures) = (0u32, 0u32, 0u32, 0u32);
    let mut push_ms = Vec::new();
    for (start, end) in windows {
        let mut next = start + rng.exp(copy_every);
        while next < end {
            tokio::time::sleep_until(next.into()).await;
            copies += 1;
            if rng.below(100) < IMAGE_PERCENT {
                images += 1;
                let mut png = vec![0u8; IMAGE_BYTES];
                rng.fill(&mut png);
                a.store.copy_image(&png);
            } else {
                a.store.copy(&format!("soak copy {copies} {:016x}", rng.next()));
            }
            tokio::time::sleep(PUSH_DEBOUNCE).await;
            let started = Instant::now();
            // 一次暂时性失败重试一次（与桌面端的退避相比已经很积极）
            let mut result = a.sync().await;
            if result.is_err() {
                push_failures += 1;
                result = a.sync().await;
            }
            match result {
                Ok(_) => {
                    pushed_ok += 1;
                    push_ms.push(i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX));
                }
                Err(e) => eprintln!("SOAK a-push-failed {}", e.kind().as_str()),
            }
            next += rng.exp(copy_every);
        }
    }
    let ops = a.remote.inner.take();
    push_ms.sort_unstable();
    json!({
        "copies": copies,
        "images": images,
        "pushed_ok": pushed_ok,
        "push_retries": push_failures,
        "requests": ops.len(),
        "push_ms": stats(&push_ms),
    })
}

// ---------------------------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------------------------

fn stats(sorted: &[i64]) -> Value {
    if sorted.is_empty() {
        return json!({"n": 0});
    }
    let at = |p: usize| sorted[(sorted.len() * p / 100).min(sorted.len() - 1)];
    json!({
        "n": sorted.len(),
        "p50": at(50),
        "p95": at(95),
        "max": sorted[sorted.len() - 1],
    })
}

/// 整个进程到此刻的 CPU 时间（毫秒，用户 + 系统），经 `ps -o time=`（macOS 与 Linux 都有）。进程里有 A 与 B 两台设备：
/// 只有 traffic 段 A 才有活，其余各段这就是 B 的开销；按线程分开要用外部的采样脚本
fn process_cpu_ms() -> Option<u64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "time=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    parse_cputime(String::from_utf8_lossy(&out.stdout).trim())
}

/// `[[dd-]hh:]mm:ss[.cc]` → 毫秒
fn parse_cputime(s: &str) -> Option<u64> {
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().ok()?, r),
        None => (0, s),
    };
    let mut total = 0.0f64;
    for part in rest.split(':') {
        total = total * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(days * 86_400_000 + (total * 1000.0).round() as u64)
}

/// xorshift 随机数（错开请求、生成复制内容与图片字节，不需要密码学强度）
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// [-1, 1]
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    }

    /// 平均 `mean_s` 秒的指数分布（泊松过程的间隔）
    fn exp(&mut self, mean_s: f64) -> Duration {
        let u = ((self.next() >> 11) as f64 + 1.0) / ((1u64 << 53) as f64 + 1.0);
        Duration::from_secs_f64(-u.ln() * mean_s)
    }

    fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let v = self.next().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }
}

#[test]
fn cputime_is_parsed_in_every_ps_format() {
    assert_eq!(parse_cputime("0:00.05"), Some(50));
    assert_eq!(parse_cputime("1:02.50"), Some(62_500));
    assert_eq!(parse_cputime("01:02:03"), Some(3_723_000));
    assert_eq!(parse_cputime("2-00:00:01"), Some(2 * 86_400_000 + 1000));
    assert_eq!(parse_cputime(""), None);
}
