//! llama.cpp 的 `llama-server`：PaddleOCR-VL（vl 槽位）与 Index-Translate（翻译槽位）各一个进程，OpenAI 兼容接口，
//! 只绑 127.0.0.1，带每次启动随机生成的 api-key。
//!
//! - **参数**（[`server_args`]，按 b11669 的 `--help` 与本机实测核对）：`--host 127.0.0.1 --port <p>`、`-m`、`[--mmproj]`、
//!   `-c`、`-np 1`、`--cache-ram 0`、`--no-webui`、`--jinja`、翻译槽位 `--reasoning off`（`--chat-template-kwargs` 在这个版本
//!   已弃用，会打印警告；请求体里的 `chat_template_kwargs` 由调用方照发作兜底）、`--threads`、关闭 GPU 时 `-ngl 0` 与
//!   `--no-mmproj-offload`（否则默认按显存自动卸载）、有显卡时 `--poll 0`、`--offline`、`--sleep-idle-seconds <闲置释放 + 60>`
//!   （即使残留也会卸载权重）、`--log-verbosity 2`（只有错误与警告，归类需要它们）。
//! - **端口**：`--port 0` 可用，但这个日志级别下不打印监听地址。改为 Wisp 先在 127.0.0.1 上绑定 0 端口取得一个空闲端口、
//!   立即释放后传给 `--port`；万一在这之间被别的程序占用（llama-server 报 `couldn't bind`），换一个端口重试，最多 3 次。
//!   监听地址行（更高日志级别或将来的版本会打印）仍会被识别，用来核对端口（[`parse_listen_port`]）。
//! - **api-key** 经环境变量 `LLAMA_API_KEY` 传入（argv 对同一用户的其他进程可见）；调用方以 `Authorization: Bearer` 发送。
//! - **就绪**：每 100 ms `GET /health`（异步 sleep，不占阻塞线程），200 即就绪；进程提前退出时读 stderr 尾部归类错误；
//!   加载超时 120 s。
//! - **输出**：一个读取线程逐行读 stdout / stderr，**只匹配固定格式**（监听端口、致命错误、内存不足、后端初始化失败、缺少
//!   动态库），其余丢弃、绝不写进 wisp.log；只在内存里保留最后 4 KB 用于错误归类。
//! - **线程**：`--threads = clamp(性能核数 / 2, 2, 8)`。
//! - **设备检测**：运行时安装后执行一次 `llama-server --list-devices --offline`（超时 90 s，Windows 的杀毒扫描可能很慢），
//!   解析 `Vulkan0: NVIDIA GeForce RTX 3060 (12288 MiB, 11000 MiB free)` 形式的行（[`parse_list_devices`]）。

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use wisp_types::GpuInfo;

use crate::error::{LocalError, RuntimeFailure};
use crate::launch::{LaunchSpec, Launcher, ManagedChild};
use crate::store::Store;

/// 传 api-key 的环境变量
pub const ENV_API_KEY: &str = "LLAMA_API_KEY";
/// 默认上下文长度（翻译与 VL 相同，可在实测后分别调整）
pub const DEFAULT_CTX: u32 = 8192;
/// 加载超时
pub const LOAD_TIMEOUT: Duration = Duration::from_secs(120);
/// 启动阶段健康检查的间隔
pub const HEALTH_INTERVAL: Duration = Duration::from_millis(100);
/// `--list-devices` 的超时
pub const LIST_DEVICES_TIMEOUT: Duration = Duration::from_secs(90);
/// 内存里保留的输出尾部（只用于错误归类）
pub const TAIL_BYTES: usize = 4096;
/// Windows 的 `STATUS_DLL_NOT_FOUND`（缺少 VC++ 运行库）
pub const STATUS_DLL_NOT_FOUND: u32 = 0xC000_0135;
/// 端口被抢占时最多换几次端口
const PORT_ATTEMPTS: usize = 3;
/// 启动中途放弃时给进程的退出时间（还在加载，没有要保存的状态）
const ABORT_GRACE: Duration = Duration::from_millis(500);
/// 日志级别：错误与警告（不打印每个请求的信息行与模型输出之外的杂项）
const LOG_VERBOSITY: &str = "2";
/// 孤儿兜底：闲置释放之后再过这么久，llama-server 自己卸载权重
const SLEEP_IDLE_EXTRA_S: u32 = 60;

/// 槽位
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Slot {
    /// PaddleOCR-VL-1.6
    Vl,
    /// Index-Translate 2B / 9B
    Translate,
}

impl Slot {
    /// pid 文件名与日志用的名字
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Vl => "vl",
            Self::Translate => "translate",
        }
    }
}

/// 启动一个 llama-server 需要的全部参数
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub slot: Slot,
    /// `llama-server` 可执行文件
    pub server: PathBuf,
    pub model: PathBuf,
    /// 视觉模型的投影器（VL）
    pub mmproj: Option<PathBuf>,
    pub ctx: u32,
    pub threads: u32,
    /// false 时 `-ngl 0`（只用 CPU）
    pub use_gpu: bool,
    /// 有可用的显卡（加 `--poll 0`，避免 CPU 忙等）
    pub has_gpu: bool,
    /// 闲置释放的秒数（`--sleep-idle-seconds` 取它 + 60）
    pub idle_release_s: u32,
}

impl ServerConfig {
    /// 这个槽位的 pid 文件：模型文件在 [`Store`] 布局的模型目录里（`<models>/<模型>/<文件>`），pid 文件放在同一个
    /// `<models>/run/` 下，与模型服务停止槽位时删除的是同一个文件
    pub fn pid_file(&self) -> Option<PathBuf> {
        let root = self.model.parent()?.parent()?;
        Some(Store::new(root.to_path_buf()).pid_file(self.slot.as_str()))
    }
}

/// 由配置生成命令行参数（不含 api-key：它经环境变量传入，不出现在命令行里）
pub fn server_args(cfg: &ServerConfig, port: u16) -> Vec<OsString> {
    let mut a: Vec<OsString> = Vec::with_capacity(32);
    let mut push = |items: &[&str]| a.extend(items.iter().map(OsString::from));
    push(&["--host", "127.0.0.1", "--port", &port.to_string()]);
    push(&["-m"]);
    a.push(cfg.model.clone().into_os_string());
    if let Some(mmproj) = &cfg.mmproj {
        a.push("--mmproj".into());
        a.push(mmproj.clone().into_os_string());
    }
    let mut push = |items: &[&str]| a.extend(items.iter().map(OsString::from));
    // 单并发、不留提示缓存：一次只服务一个请求，省下多槽位与缓存的内存
    push(&["-c", &cfg.ctx.to_string(), "-np", "1", "--cache-ram", "0"]);
    push(&["--no-webui", "--jinja", "--offline"]);
    if cfg.slot == Slot::Translate {
        push(&["--reasoning", "off"]);
    }
    push(&["--threads", &cfg.threads.max(1).to_string()]);
    if !cfg.use_gpu {
        push(&["-ngl", "0"]);
        if cfg.mmproj.is_some() {
            push(&["--no-mmproj-offload"]);
        }
    } else if cfg.has_gpu {
        // 计算在显卡上时 CPU 线程不必忙等
        push(&["--poll", "0"]);
    }
    let sleep = cfg.idle_release_s.saturating_add(SLEEP_IDLE_EXTRA_S).max(1);
    push(&["--sleep-idle-seconds", &sleep.to_string()]);
    push(&["--log-verbosity", LOG_VERBOSITY]);
    a
}

/// 每次启动生成的随机 api-key（32 字节，十六进制）
pub fn new_api_key() -> Result<String, LocalError> {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| LocalError::Protocol("system random number generator failed"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// 输出的尾部（只在内存里，用于错误归类）与识别到的监听端口
#[derive(Default)]
struct Output {
    tail: Mutex<VecDeque<u8>>,
    port: AtomicU16,
}

impl Output {
    fn push_line(&self, line: &[u8]) {
        if let Some(p) = std::str::from_utf8(line).ok().and_then(parse_listen_port) {
            self.port.store(p, Ordering::Relaxed);
        }
        let mut t = self.tail.lock();
        t.extend(line.iter().copied());
        t.push_back(b'\n');
        let extra = t.len().saturating_sub(TAIL_BYTES);
        t.drain(..extra);
    }

    fn tail(&self) -> String {
        let t = self.tail.lock();
        let (a, b) = t.as_slices();
        let mut v = Vec::with_capacity(t.len());
        v.extend_from_slice(a);
        v.extend_from_slice(b);
        String::from_utf8_lossy(&v).into_owned()
    }
}

/// 逐行读一路输出，交给 [`Output`]；进程退出（管道关闭）时结束
fn spawn_reader(r: Box<dyn Read + Send>, out: Arc<Output>, name: &'static str) {
    let _ = std::thread::Builder::new().name(name.into()).spawn(move || {
        let mut r = BufReader::new(r);
        let mut line = Vec::with_capacity(256);
        loop {
            line.clear();
            match r.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    while line.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
                        line.pop();
                    }
                    out.push_line(&line);
                }
            }
        }
    });
}

/// 运行中的 llama-server
pub struct Server {
    child: Box<dyn ManagedChild>,
    port: u16,
    api_key: String,
    output: Arc<Output>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // api-key 不输出
        f.debug_struct("Server")
            .field("pid", &self.child.pid())
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

/// 在 127.0.0.1 上取一个当前空闲的端口（绑定后立即释放）
fn free_port() -> Result<u16, LocalError> {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(l.local_addr()?.port())
}

/// 一次启动的结果
enum Launch {
    Ready(Server),
    /// 端口在取号与启动之间被占用：换一个再试
    PortTaken,
}

impl Server {
    /// 启动并等到就绪（见模块说明）。`cancel` 触发时停止进程并返回 [`LocalError::Cancelled`]
    pub async fn start(
        launcher: Arc<dyn Launcher>,
        cfg: ServerConfig,
        cancel: CancellationToken,
    ) -> Result<Self, LocalError> {
        let api_key = new_api_key()?;
        let health = crate::net::client_builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .map_err(|_| LocalError::Protocol("http client"))?;
        for _ in 0..PORT_ATTEMPTS {
            match Self::launch_once(&launcher, &cfg, &api_key, &health, &cancel).await? {
                Launch::Ready(s) => return Ok(s),
                Launch::PortTaken => log::info!("local: {} port was taken, retrying", cfg.slot.as_str()),
            }
        }
        Err(RuntimeFailure::StartFailed.into())
    }

    async fn launch_once(
        launcher: &Arc<dyn Launcher>,
        cfg: &ServerConfig,
        api_key: &str,
        health: &reqwest::Client,
        cancel: &CancellationToken,
    ) -> Result<Launch, LocalError> {
        let port = free_port()?;
        let spec = LaunchSpec {
            program: cfg.server.clone(),
            args: server_args(cfg, port),
            env: vec![(ENV_API_KEY.into(), api_key.into())],
            // 工作目录固定为 llama-server 所在目录：不从别处加载后端模块
            current_dir: cfg.server.parent().map(std::path::Path::to_path_buf),
            guard: true,
            low_priority: true,
        };
        let started = Instant::now();
        let l = Arc::clone(launcher);
        let pid_file = cfg.pid_file();
        let mut child = crate::download::blocking(move || {
            let child = l
                .launch(spec)
                .map_err(|_| LocalError::Runtime(RuntimeFailure::StartFailed))?;
            if let Some(path) = pid_file
                && let Err(e) = l.write_pid_file(&path, child.pid())
            {
                log::warn!("local: writing the pid file failed: {}", e.kind());
            }
            Ok(child)
        })
        .await?;
        let output = Arc::new(Output::default());
        if let Some(r) = child.take_stdout() {
            spawn_reader(r, Arc::clone(&output), "wisp-llama-out");
        }
        if let Some(r) = child.take_stderr() {
            spawn_reader(r, Arc::clone(&output), "wisp-llama-err");
        }
        let mut server = Self {
            child,
            port,
            api_key: api_key.to_owned(),
            output,
        };
        let url = format!("http://127.0.0.1:{port}/health");
        loop {
            if cancel.is_cancelled() {
                server.abort().await;
                return Err(LocalError::Cancelled);
            }
            if let Ok(Some(status)) = server.child.try_wait() {
                // 输出读取线程可能还没读完最后几行
                tokio::time::sleep(Duration::from_millis(50)).await;
                let tail = server.output.tail();
                if is_port_conflict(&tail) {
                    return Ok(Launch::PortTaken);
                }
                let failure = classify(status.code(), &tail);
                log::warn!(
                    "local: {} exited during startup ({:?}, code {:?})",
                    cfg.slot.as_str(),
                    failure,
                    status.code()
                );
                return Err(failure.into());
            }
            if started.elapsed() >= LOAD_TIMEOUT {
                server.abort().await;
                return Err(RuntimeFailure::LoadTimeout.into());
            }
            let ok = tokio::select! {
                () = cancel.cancelled() => false,
                r = health.get(&url).send() => r.is_ok_and(|r| r.status().is_success()),
            };
            if ok {
                let seen = server.output.port.load(Ordering::Relaxed);
                if seen != 0 && seen != port {
                    log::warn!("local: {} reported a different port", cfg.slot.as_str());
                }
                log::info!(
                    "local: {} ready in {} ms (pid {})",
                    cfg.slot.as_str(),
                    started.elapsed().as_millis(),
                    server.pid()
                );
                return Ok(Launch::Ready(server));
            }
            tokio::select! {
                () = cancel.cancelled() => {}
                () = tokio::time::sleep(HEALTH_INTERVAL) => {}
            }
        }
    }

    /// 启动中途放弃：在阻塞线程里停止
    async fn abort(self) {
        let _ = crate::download::blocking(move || {
            self.stop(ABORT_GRACE);
            Ok(())
        })
        .await;
    }

    /// OpenAI 兼容接口的 base_url（`http://127.0.0.1:<port>`）
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// 这次启动的 api-key（只交给本进程内的 HTTP 客户端，不进日志）
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// 进程 pid
    pub fn pid(&self) -> u32 {
        self.child.pid()
    }

    /// 进程是否已经意外退出；退出了就给出归类后的原因（不阻塞）
    pub fn exited(&mut self) -> Option<RuntimeFailure> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(classify(status.code(), &self.output.tail())),
            Ok(None) => None,
            Err(_) => Some(RuntimeFailure::Crashed),
        }
    }

    /// 停止：关 stdin（守护进程据此结束它）、最多等 `grace`，再强制结束（阻塞）
    pub fn stop(mut self, grace: Duration) {
        let stdin = self.child.take_stdin();
        crate::launch::stop(self.child.as_mut(), stdin, grace);
    }
}

/// 输出尾部是不是「端口被占用」（`couldn't bind HTTP server socket`）
fn is_port_conflict(tail: &str) -> bool {
    tail.contains("couldn't bind HTTP server socket") || tail.contains("Address already in use")
}

/// 按退出码与输出尾部归类失败原因（只匹配固定的关键字）
pub fn classify(exit_code: Option<i32>, tail: &str) -> RuntimeFailure {
    if exit_code.is_some_and(|c| c.cast_unsigned() == STATUS_DLL_NOT_FOUND) {
        return RuntimeFailure::VcRuntimeMissing;
    }
    let t = tail.to_ascii_lowercase();
    let any = |keys: &[&str]| keys.iter().any(|k| t.contains(k));
    // 系统库（glibc / libstdc++ 太旧、缺 OpenSSL 3、缺 libgomp）：动态链接器在进程启动前就报错
    if any(&[
        "glibc_",
        "glibcxx_",
        "libssl.so",
        "libcrypto.so",
        "libgomp",
        "libstdc++.so",
        "libvulkan.so",
    ]) {
        return RuntimeFailure::SystemLibraryMissing;
    }
    // 自带的动态库缺失或损坏、模型文件读不了：重新下载
    if any(&[
        "library not loaded",
        "error while loading shared libraries",
        "failed to load model",
        "error loading model",
        "gguf_init_from_file",
        "invalid magic",
    ]) {
        return RuntimeFailure::ModelCorrupt;
    }
    if any(&[
        "out of memory",
        "failed to allocate",
        "unable to allocate",
        "bad_alloc",
        "erroroutofdevicememory",
        "erroroutofhostmemory",
        "insufficient memory",
        "cannot allocate memory",
    ]) {
        return RuntimeFailure::OutOfMemory;
    }
    if any(&[
        "ggml_metal_init: error",
        "failed to initialize metal",
        "failed to create metal",
        "metal error",
        "ggml_vulkan: error",
        "vk::",
        "vk_error",
        "errordevicelost",
        "failed to initialize vulkan",
        "failed to initialize the backend",
        "failed to initialize gpu",
    ]) {
        return RuntimeFailure::GpuFailed;
    }
    RuntimeFailure::Crashed
}

/// 从 llama-server 的输出行里取监听端口（只认 `listening on http://127.0.0.1:<端口>` 这一种固定格式）
pub fn parse_listen_port(line: &str) -> Option<u16> {
    let rest = &line[line.find("listening on http://127.0.0.1:")? + "listening on http://127.0.0.1:".len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok().filter(|p| *p != 0)
}

/// 软件渲染器（Linux 的 llvmpipe / lavapipe、SwiftShader）：Vulkan 会把系统内存报成「显存」，不是真正的显卡
const SOFTWARE_RENDERERS: &[&str] = &["llvmpipe", "lavapipe", "swiftshader", "microsoft basic render"];

/// 解析 `llama-server --list-devices` 的输出：`Available devices:` 之后形如
/// `  Vulkan0: NVIDIA GeForce RTX 3060 (12288 MiB, 11000 MiB free)` 的行（不列 CPU；没有设备时是 `(none)`）
pub fn parse_list_devices(output: &str) -> Vec<GpuInfo> {
    let mut out = Vec::new();
    let mut in_list = false;
    for line in output.lines() {
        if line.trim() == "Available devices:" {
            in_list = true;
            continue;
        }
        if !in_list {
            continue;
        }
        let Some(gpu) = parse_device_line(line) else {
            // 列表结束（之后可能还有别的输出）
            if !line.starts_with("  ") {
                in_list = false;
            }
            continue;
        };
        let lower = gpu.name.to_ascii_lowercase();
        if !SOFTWARE_RENDERERS.iter().any(|s| lower.contains(s)) {
            out.push(gpu);
        }
    }
    out
}

/// `  <设备名>: <描述> (<总量> MiB, <空闲> MiB free)`
fn parse_device_line(line: &str) -> Option<GpuInfo> {
    let line = line.trim();
    let (dev, rest) = line.split_once(": ")?;
    if dev.is_empty() || dev.contains(char::is_whitespace) {
        return None;
    }
    let rest = rest.strip_suffix(" MiB free)")?;
    let open = rest.rfind(" (")?;
    let (desc, sizes) = (rest[..open].trim(), &rest[open + 2..]);
    let (total, _free) = sizes.split_once(" MiB, ")?;
    let total: u64 = total.trim().parse().ok()?;
    // 0 MiB 的是 BLAS 这类不带显存的加速后端
    // 是不是核显要结合系统内存判断（见 is_integrated_gpu），由组装设备信息的一方填写
    (!desc.is_empty() && total > 0).then(|| GpuInfo {
        name: desc.to_owned(),
        vram_total_mb: u32::try_from(total).unwrap_or(u32::MAX),
        integrated: false,
    })
}

/// 一块显卡是不是集成显卡（核显）。核显没有自己的显存，Vulkan 报告的「显存」是划给它的系统内存（常见 8–16 GB），
/// 按显存评估会把它当成高端独显；设备适配因此按 CPU 档评估它。
///
/// 判断顺序：先认 AMD 的 Vega APU（Raven Ridge / Picasso 的核显叫 `Radeon RX Vega 11 Graphics`，带着独显的 `RX`；
/// 独显的 Vega 56 / 64 名称里没有 `Graphics`），再认独显的型号（NVIDIA 全系、AMD 的 RX / Pro / W 系列、Intel Arc 的
/// A / B 系列），再认核显的名称（Intel 的 UHD / HD / Iris 与不带型号的 Arc Graphics、Lunar / Arrow Lake 的 Arc 1xxV / 1xxT；
/// AMD 的 `Radeon(TM) Graphics`、Vega N Graphics、7xxM / 8xxM、8060S 这类 APU 名称），都不认识时看显存：不少于系统内存的
/// 45%（核显通常分到一半，Windows 报告的系统内存略小于标称值，留一点余量）就当作核显。名称来自驱动，Windows 与
/// Linux（Mesa 会在名称后附代号，例如 `(RADV PHOENIX)`、`(ADL GT2)`）都按同一套关键字识别
pub fn is_integrated_gpu(gpu: &GpuInfo, total_memory: Option<u64>) -> bool {
    let name = gpu.name.to_ascii_lowercase();
    let words: Vec<&str> = name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let has = |w: &str| words.contains(&w);
    // 型号形如 a770 / b580 / 780m / 8060s：字母前缀或后缀 + 数字
    let model_with = |prefix: &[char], suffix: &[char]| {
        words.iter().any(|w| {
            let (head, tail) = (w.chars().next(), w.chars().last());
            let digits = w.trim_start_matches(prefix).trim_end_matches(suffix);
            digits.len() >= 3
                && digits.bytes().all(|b| b.is_ascii_digit())
                && (prefix.is_empty() || head.is_some_and(|c| prefix.contains(&c)))
                && (suffix.is_empty() || tail.is_some_and(|c| suffix.contains(&c)))
        })
    };
    let nvidia = has("nvidia") || has("geforce") || has("quadro") || has("tesla") || has("rtx") || has("gtx");
    let amd = has("amd") || has("radeon");
    let intel = has("intel");
    if amd && has("vega") && has("graphics") {
        return true;
    }
    let discrete = nvidia
        || (amd && (has("rx") || has("pro") || has("firepro") || has("instinct") || has("vii")))
        || (intel && has("arc") && (model_with(&['a', 'b'], &[]) || has("pro")));
    if discrete {
        return false;
    }
    let integrated = (intel && (has("uhd") || has("hd") || has("iris") || has("arc") || has("graphics")))
        || (amd && (has("graphics") || has("vega") || model_with(&[], &['m', 's'])));
    if integrated {
        return true;
    }
    let vram = u64::from(gpu.vram_total_mb) * 1024 * 1024;
    total_memory.is_some_and(|total| total > 0 && vram.saturating_mul(100) >= total.saturating_mul(45))
}

/// 设备检测的启动描述：不需要守护进程（命令很快结束，超时由调用方强制结束）
pub fn list_devices_spec(server: PathBuf) -> LaunchSpec {
    let dir = server.parent().map(std::path::Path::to_path_buf);
    LaunchSpec {
        program: server,
        args: vec!["--list-devices".into(), "--offline".into()],
        env: Vec::new(),
        current_dir: dir,
        guard: false,
        low_priority: false,
    }
}

/// 运行 `llama-server --list-devices --offline` 检测显卡（最多 [`LIST_DEVICES_TIMEOUT`]）
pub async fn list_devices(launcher: Arc<dyn Launcher>, server: PathBuf) -> Result<Vec<GpuInfo>, LocalError> {
    crate::download::blocking(move || {
        let mut child = launcher
            .launch(list_devices_spec(server))
            .map_err(|_| LocalError::Runtime(RuntimeFailure::StartFailed))?;
        drop(child.take_stdin());
        let read_all = |r: Option<Box<dyn Read + Send>>| {
            std::thread::spawn(move || {
                let mut s = Vec::new();
                if let Some(mut r) = r {
                    let _ = r.read_to_end(&mut s);
                }
                String::from_utf8_lossy(&s).into_owned()
            })
        };
        let (out, err) = (read_all(child.take_stdout()), read_all(child.take_stderr()));
        let status = child.wait_timeout(LIST_DEVICES_TIMEOUT)?;
        let Some(status) = status else {
            let _ = child.kill();
            return Err(LocalError::Runtime(RuntimeFailure::LoadTimeout));
        };
        let text = format!("{}\n{}", out.join().unwrap_or_default(), err.join().unwrap_or_default());
        if !status.success() {
            let mut from = text.len().saturating_sub(TAIL_BYTES);
            while !text.is_char_boundary(from) {
                from += 1;
            }
            return Err(LocalError::Runtime(classify(status.code(), &text[from..])));
        }
        Ok(parse_list_devices(&text))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_keys_are_random_hex() {
        let a = new_api_key().expect("key");
        let b = new_api_key().expect("key");
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn missing_vc_runtime_is_recognised() {
        let code = STATUS_DLL_NOT_FOUND.cast_signed();
        assert_eq!(classify(Some(code), ""), RuntimeFailure::VcRuntimeMissing);
    }

    fn cfg(slot: Slot, use_gpu: bool, has_gpu: bool) -> ServerConfig {
        ServerConfig {
            slot,
            server: PathBuf::from("/m/runtimes/llama.cpp/b11669/llama-server"),
            model: PathBuf::from("/m/paddleocr_vl_1_6/model.gguf"),
            mmproj: (slot == Slot::Vl).then(|| PathBuf::from("/m/paddleocr_vl_1_6/mmproj.gguf")),
            ctx: 8192,
            threads: 4,
            use_gpu,
            has_gpu,
            idle_release_s: 300,
        }
    }

    fn args(c: &ServerConfig) -> Vec<String> {
        server_args(c, 41234)
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn has_pair(a: &[String], k: &str, v: &str) -> bool {
        a.windows(2).any(|w| w[0] == k && w[1] == v)
    }

    #[test]
    fn server_args_follow_the_measured_setup() {
        let t = args(&cfg(Slot::Translate, true, true));
        assert!(has_pair(&t, "--host", "127.0.0.1") && has_pair(&t, "--port", "41234"));
        assert!(has_pair(&t, "-c", "8192") && has_pair(&t, "-np", "1") && has_pair(&t, "--cache-ram", "0"));
        assert!(has_pair(&t, "--reasoning", "off"));
        assert!(has_pair(&t, "--threads", "4") && has_pair(&t, "--poll", "0"));
        assert!(has_pair(&t, "--sleep-idle-seconds", "360"));
        assert!(has_pair(&t, "--log-verbosity", "2"));
        for flag in ["--no-webui", "--jinja", "--offline"] {
            assert!(t.iter().any(|a| a == flag), "{flag}");
        }
        assert!(!t.iter().any(|a| a == "--chat-template-kwargs"), "这个版本已弃用");
        assert!(!t.iter().any(|a| a == "-ngl" || a == "--mmproj"));
        assert!(
            !t.iter().any(|a| a.contains("api") || a.contains("key")),
            "api-key 不进命令行"
        );

        let v = args(&cfg(Slot::Vl, false, true));
        assert!(has_pair(&v, "--mmproj", "/m/paddleocr_vl_1_6/mmproj.gguf"));
        assert!(has_pair(&v, "-ngl", "0") && v.iter().any(|a| a == "--no-mmproj-offload"));
        assert!(
            !v.iter().any(|a| a == "--reasoning" || a == "--poll"),
            "VL 不关思考；只用 CPU 时不加 --poll 0"
        );
        let cpu = args(&cfg(Slot::Translate, true, false));
        assert!(!cpu.iter().any(|a| a == "--poll"));
    }

    #[test]
    fn pid_file_lives_next_to_the_models() {
        assert_eq!(
            cfg(Slot::Vl, true, true).pid_file(),
            Some(PathBuf::from("/m/run/vl.pid"))
        );
    }

    #[test]
    fn failures_are_classified_by_fixed_keywords() {
        use RuntimeFailure::*;
        let cases = [
            (
                "./llama-server: /lib/x86_64-linux-gnu/libc.so.6: version `GLIBC_2.34' not found",
                SystemLibraryMissing,
            ),
            (
                "error while loading shared libraries: libgomp.so.1: cannot open shared object file",
                SystemLibraryMissing,
            ),
            ("dyld[123]: Library not loaded: @rpath/libmtmd.0.dylib", ModelCorrupt),
            (
                "llama_model_load: error loading model: tensor data is not within the file bounds",
                ModelCorrupt,
            ),
            (
                "ggml_backend_cpu_buffer_type_alloc_buffer: failed to allocate buffer of size 6000000000",
                OutOfMemory,
            ),
            (
                "ggml_vulkan: Device memory allocation of size 1 failed. ErrorOutOfDeviceMemory",
                OutOfMemory,
            ),
            ("ggml_metal_init: error: failed to create command queue", GpuFailed),
            (
                "terminate called after throwing an instance of 'vk::DeviceLostError'",
                GpuFailed,
            ),
            ("something else entirely", Crashed),
        ];
        for (tail, want) in cases {
            assert_eq!(classify(Some(1), tail), want, "{tail}");
        }
        assert_eq!(classify(None, ""), Crashed, "被信号结束");
        assert!(is_port_conflict(
            "srv    load_model: couldn't bind HTTP server socket, hostname: 127.0.0.1, port: 41234"
        ));
    }

    #[test]
    fn listen_line() {
        assert_eq!(
            parse_listen_port("main: server is listening on http://127.0.0.1:41234 - starting the main loop"),
            Some(41234)
        );
        assert_eq!(
            parse_listen_port("srv  init: listening on http://127.0.0.1:9931"),
            Some(9931)
        );
        assert_eq!(parse_listen_port("listening on http://0.0.0.0:80"), None);
        assert_eq!(parse_listen_port("listening on http://127.0.0.1:0"), None);
    }

    /// 输出格式取自 b11669 的 `common_print_available_devices`（`  %s: %s (%zu MiB, %zu MiB free)`）
    #[test]
    fn device_lists() {
        let windows = "load_backend: loaded Vulkan backend from C:\\x\\ggml-vulkan.dll\n\
                       load_backend: loaded CPU backend from C:\\x\\ggml-cpu-haswell.dll\n\
                       Available devices:\n  Vulkan0: NVIDIA GeForce RTX 3060 (12288 MiB, 11000 MiB free)\n\
                       \x20 Vulkan1: Intel(R) UHD Graphics 770 (8159 MiB, 8000 MiB free)\n";
        let gpus = parse_list_devices(windows);
        assert_eq!(
            gpus,
            vec![
                GpuInfo {
                    name: "NVIDIA GeForce RTX 3060".to_owned(),
                    vram_total_mb: 12288,
                    integrated: false,
                },
                GpuInfo {
                    name: "Intel(R) UHD Graphics 770".to_owned(),
                    vram_total_mb: 8159,
                    integrated: false,
                },
            ]
        );
        let linux = "Available devices:\n  Vulkan0: llvmpipe (LLVM 15.0.7, 256 bits) (32000 MiB, 32000 MiB free)\n";
        assert!(parse_list_devices(linux).is_empty(), "软件渲染器不算显卡");
        assert!(parse_list_devices("Available devices:\n  (none)\n").is_empty());
        assert!(parse_list_devices("").is_empty());
        let mac = "Available devices:\n  MTL0: Apple M5 (24576 MiB, 24575 MiB free)\n  BLAS: Accelerate (0 MiB, 0 MiB free)\n";
        assert_eq!(
            parse_list_devices(mac)
                .iter()
                .map(|g| g.name.as_str())
                .collect::<Vec<_>>(),
            ["Apple M5"]
        );
    }

    /// 典型的 `--list-devices` 输出：Windows 驱动给的名称，Linux（Mesa）在名称后附代号
    #[test]
    fn integrated_gpus_are_recognised() {
        const GB: u64 = 1024 * 1024 * 1024;
        let output = "Available devices:\n\
            \x20 Vulkan0: NVIDIA GeForce RTX 4060 Laptop GPU (8188 MiB, 7000 MiB free)\n\
            \x20 Vulkan1: Intel(R) UHD Graphics 770 (8159 MiB, 8000 MiB free)\n\
            \x20 Vulkan2: Intel(R) Iris(R) Xe Graphics (TGL GT2) (7834 MiB, 7834 MiB free)\n\
            \x20 Vulkan3: Intel(R) Arc(TM) Graphics (18185 MiB, 18000 MiB free)\n\
            \x20 Vulkan4: Intel(R) Arc(TM) 140V GPU (16GB) (18185 MiB, 18000 MiB free)\n\
            \x20 Vulkan5: Intel(R) Arc(TM) A770 Graphics (16032 MiB, 16000 MiB free)\n\
            \x20 Vulkan6: Intel(R) Arc(tm) B580 Graphics (BMG G21) (12216 MiB, 12000 MiB free)\n\
            \x20 Vulkan7: AMD Radeon(TM) Graphics (2048 MiB, 1900 MiB free)\n\
            \x20 Vulkan8: AMD Radeon 780M (RADV PHOENIX) (16384 MiB, 15000 MiB free)\n\
            \x20 Vulkan9: AMD Radeon(TM) Vega 8 Graphics (2048 MiB, 2000 MiB free)\n\
            \x20 Vulkan10: AMD Radeon RX 7900 XTX (RADV NAVI31) (24560 MiB, 24000 MiB free)\n\
            \x20 Vulkan11: AMD Radeon RX Vega 56 (8176 MiB, 8000 MiB free)\n\
            \x20 Vulkan12: AMD Radeon Pro W7900 (45056 MiB, 45000 MiB free)\n\
            \x20 Vulkan13: AMD Radeon(TM) RX Vega 11 Graphics (2048 MiB, 2000 MiB free)\n\
            \x20 Vulkan14: AMD Radeon RX Vega 10 Graphics (RADV RAVEN) (3072 MiB, 3000 MiB free)\n\
            \x20 Vulkan15: Radeon RX Vega (8176 MiB, 8000 MiB free)\n";
        let gpus = parse_list_devices(output);
        assert_eq!(gpus.len(), 16);
        let integrated: Vec<bool> = gpus.iter().map(|g| is_integrated_gpu(g, Some(16 * GB))).collect();
        assert_eq!(
            integrated,
            [
                false, true, true, true, true, false, false, true, true, true, false, false, false, true, true, false
            ],
            "{:?}",
            gpus.iter().map(|g| g.name.as_str()).collect::<Vec<_>>()
        );
        // 不认识的名称：显存占到系统内存的一半左右就是共享内存
        let unknown = GpuInfo {
            name: "Qualcomm(R) Adreno(TM) X1-85 GPU".to_owned(),
            vram_total_mb: 8_000,
            integrated: false,
        };
        assert!(is_integrated_gpu(&unknown, Some(16 * GB)));
        assert!(!is_integrated_gpu(&unknown, Some(64 * GB)));
        assert!(!is_integrated_gpu(&unknown, None));
        // 独显的显存再大也不算：16 GB 内存的机器上的 12 GB RTX 3060
        let rtx = GpuInfo {
            name: "NVIDIA GeForce RTX 3060".to_owned(),
            vram_total_mb: 12_288,
            integrated: false,
        };
        assert!(!is_integrated_gpu(&rtx, Some(16 * GB)));
    }
}
