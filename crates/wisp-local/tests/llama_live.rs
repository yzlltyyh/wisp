//! 真机：官方 llama-server（b11669）+ 官方 GGUF（`#[ignore]`）。冷 / 热启动到就绪的时间、一次 `/v1/chat/completions`、常驻内存、
//! api-key 生效、停止后不留进程、`--list-devices` 的真实输出。
//!
//! `WISP_LOCAL_TEST_DIR=<artifacts/local-models 的绝对路径> cargo test -p wisp-local --test llama_live -- --ignored --nocapture --test-threads=1`
//!
//! 这里的启动器直接用 `std::process`（不经守护进程；守护进程对真实 llama-server 的效果由 wisp-platform 的 `child_guard`
//! 真机用例验证）。llama-server 只绑 127.0.0.1。
// 真机数据打印给运行者看
#![allow(clippy::print_stdout)]

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use wisp_local::launch::{LaunchSpec, Launcher, ManagedChild};
use wisp_local::llama::{self, Server, ServerConfig, Slot};

fn root() -> PathBuf {
    PathBuf::from(std::env::var("WISP_LOCAL_TEST_DIR").expect("WISP_LOCAL_TEST_DIR"))
}

fn server_bin() -> PathBuf {
    root().join("rt/llama-b11669/llama-b11669/llama-server")
}

struct StdLauncher;

struct StdChild(Child);

impl ManagedChild for StdChild {
    fn pid(&self) -> u32 {
        self.0.id()
    }
    fn take_stdin(&mut self) -> Option<Box<dyn Write + Send>> {
        self.0.stdin.take().map(|s| Box::new(s) as Box<dyn Write + Send>)
    }
    fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
        self.0.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>)
    }
    fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
        self.0.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>)
    }
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.0.try_wait()
    }
    fn wait_timeout(&mut self, timeout: Duration) -> io::Result<Option<ExitStatus>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(s) = self.0.try_wait()? {
                return Ok(Some(s));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn kill(&mut self) -> io::Result<()> {
        let r = self.0.kill();
        let _ = self.0.wait();
        r
    }
}

impl Drop for StdChild {
    fn drop(&mut self) {
        // 兜底：测试失败时也不留进程
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

impl Launcher for StdLauncher {
    fn launch(&self, spec: LaunchSpec) -> io::Result<Box<dyn ManagedChild>> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args).env_clear();
        for name in ["HOME", "TMPDIR", "LANG"] {
            if let Some(v) = std::env::var_os(name) {
                cmd.env(name, v);
            }
        }
        cmd.envs(spec.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(d) = &spec.current_dir {
            cmd.current_dir(d);
        }
        Ok(Box::new(StdChild(cmd.spawn()?)))
    }
    fn current_exe(&self) -> io::Result<PathBuf> {
        std::env::current_exe()
    }
    fn write_pid_file(&self, _: &Path, _: u32) -> io::Result<()> {
        // 模型在测试素材目录里：不往那里写 pid 文件
        Ok(())
    }
}

fn config(slot: Slot, model: &str, mmproj: Option<&str>) -> ServerConfig {
    ServerConfig {
        slot,
        server: server_bin(),
        model: root().join(model),
        mmproj: mmproj.map(|m| root().join(m)),
        ctx: llama::DEFAULT_CTX,
        threads: 4,
        use_gpu: true,
        has_gpu: true,
        idle_release_s: 300,
    }
}

fn rss_mb(pid: u32) -> u64 {
    Command::new("/bin/ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u64>().ok())
        .map_or(0, |kb| kb / 1024)
}

fn alive(pid: u32) -> bool {
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|s| s.success())
}

/// 一次翻译请求（官方提示词格式，温度 0，关思考）；返回（首包前的耗时、总耗时、timings JSON）
async fn translate(server: &Server, text: &str) -> (Duration, serde_json::Value) {
    let client = reqwest::Client::builder().no_proxy().build().expect("client");
    let body = serde_json::json!({
        "messages": [{"role": "user", "content": format!("请将以下文本翻译为中文，直接输出翻译结果，不要进行任何解释。\n\n{text}")}],
        "temperature": 0,
        "max_tokens": 128,
        "chat_template_kwargs": {"enable_thinking": false},
    });
    let started = Instant::now();
    let resp = client
        .post(format!("{}/v1/chat/completions", server.base_url()))
        .bearer_auth(server.api_key())
        .json(&body)
        .send()
        .await
        .expect("send");
    assert!(resp.status().is_success(), "{}", resp.status());
    let v: serde_json::Value = resp.json().await.expect("json");
    let elapsed = started.elapsed();
    let out = v["choices"][0]["message"]["content"].as_str().unwrap_or("").to_owned();
    assert!(!out.trim().is_empty(), "empty translation");
    println!("  output: {out}");
    (elapsed, v["timings"].clone())
}

async fn start_timed(cfg: ServerConfig) -> (Server, Duration) {
    let started = Instant::now();
    let s = Server::start(Arc::new(StdLauncher), cfg, CancellationToken::new())
        .await
        .expect("start");
    (s, started.elapsed())
}

#[tokio::test]
#[ignore = "真机：运行官方 llama-server 与 Index-Translate-2B"]
async fn index_translate_2b_lifecycle() {
    let cfg = config(
        Slot::Translate,
        "raw/Index-Translate-2B-GGUF/Index-Translate-2B.Q4_K_M.gguf",
        None,
    );
    let (server, cold) = start_timed(cfg.clone()).await;
    let pid = server.pid();
    println!("2B start #1: {} ms (pid {pid})", cold.as_millis());
    // api-key 生效：不带 key 的请求被拒
    let anon = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client")
        .post(format!("{}/v1/chat/completions", server.base_url()))
        .json(&serde_json::json!({"messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .expect("send");
    assert_eq!(anon.status().as_u16(), 401, "requests without the key are rejected");
    let (t1, timings) = translate(&server, "Wisp stays out of your way: it idles at almost zero CPU.").await;
    println!("  first request {} ms, timings {timings}", t1.as_millis());
    let (t2, timings) = translate(&server, "Translations appear right next to the cursor.").await;
    println!("  second request {} ms, timings {timings}", t2.as_millis());
    println!("  rss {} MB", rss_mb(pid));
    let t = Instant::now();
    server.stop(Duration::from_secs(1));
    println!("  stop {} ms", t.elapsed().as_millis());
    assert!(!alive(pid), "no process left after stop");

    let (server, warm) = start_timed(cfg).await;
    println!("2B start #2 (page cache warm): {} ms", warm.as_millis());
    let pid = server.pid();
    let (t3, _) = translate(&server, "Hello, world.").await;
    println!(
        "  first request after warm start {} ms, rss {} MB",
        t3.as_millis(),
        rss_mb(pid)
    );
    server.stop(Duration::from_secs(1));
    assert!(!alive(pid));
}

#[tokio::test]
#[ignore = "真机：运行官方 llama-server 与 Index-Translate-9B"]
async fn index_translate_9b_start_and_memory() {
    let cfg = config(
        Slot::Translate,
        "raw/Index-Translate-9B-GGUF/Index-Translate-9B.Q4_K_M.gguf",
        None,
    );
    let (server, t) = start_timed(cfg).await;
    let pid = server.pid();
    println!("9B start: {} ms", t.as_millis());
    let (t1, timings) = translate(&server, "Wisp stays out of your way: it idles at almost zero CPU.").await;
    println!(
        "  request {} ms, timings {timings}, rss {} MB",
        t1.as_millis(),
        rss_mb(pid)
    );
    server.stop(Duration::from_secs(1));
    assert!(!alive(pid));
}

#[tokio::test]
#[ignore = "真机：运行官方 llama-server 与 PaddleOCR-VL-1.6"]
async fn paddleocr_vl_start_and_memory() {
    let cfg = config(
        Slot::Vl,
        "raw/PaddleOCR-VL-1.6-GGUF/PaddleOCR-VL-1.6-GGUF.gguf",
        Some("raw/PaddleOCR-VL-1.6-GGUF/PaddleOCR-VL-1.6-GGUF-mmproj.gguf"),
    );
    let (server, t) = start_timed(cfg).await;
    let pid = server.pid();
    println!("VL start: {} ms, rss {} MB", t.as_millis(), rss_mb(pid));
    server.stop(Duration::from_secs(1));
    assert!(!alive(pid));
}

#[tokio::test]
#[ignore = "真机：运行官方 llama-server"]
async fn cancel_during_load_leaves_no_process() {
    let cfg = config(
        Slot::Translate,
        "raw/Index-Translate-9B-GGUF/Index-Translate-9B.Q4_K_M.gguf",
        None,
    );
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        c.cancel();
    });
    let r = Server::start(Arc::new(StdLauncher), cfg, cancel).await;
    assert!(matches!(r, Err(wisp_local::LocalError::Cancelled)), "{r:?}");
    let left = Command::new("/usr/bin/pgrep")
        .args(["-f", &server_bin().to_string_lossy()])
        .output()
        .expect("pgrep");
    assert!(left.stdout.is_empty(), "no llama-server left");
}

#[tokio::test]
#[ignore = "真机：运行官方 llama-server"]
async fn missing_model_is_classified() {
    let mut cfg = config(Slot::Translate, "raw/does-not-exist.gguf", None);
    cfg.model = root().join("raw/does-not-exist.gguf");
    let r = Server::start(Arc::new(StdLauncher), cfg, CancellationToken::new()).await;
    println!("missing model -> {r:?}");
    assert!(matches!(
        r,
        Err(wisp_local::LocalError::Runtime(
            wisp_local::RuntimeFailure::ModelCorrupt
        ))
    ));
}

#[tokio::test]
#[ignore = "真机：运行官方 llama-server --list-devices"]
async fn list_devices_on_this_machine() {
    let raw = Command::new(server_bin())
        .args(["--list-devices", "--offline"])
        .env_clear()
        .output()
        .expect("run");
    let text: Vec<OsString> = vec![
        String::from_utf8_lossy(&raw.stdout).into_owned().into(),
        String::from_utf8_lossy(&raw.stderr).into_owned().into(),
    ];
    println!("--list-devices stdout+stderr:\n{text:?}");
    let gpus = llama::list_devices(Arc::new(StdLauncher), server_bin())
        .await
        .expect("list");
    println!("parsed: {gpus:?}");
}
