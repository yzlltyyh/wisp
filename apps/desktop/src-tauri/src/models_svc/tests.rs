//! 模型服务的槽位生命周期与下载队列测试。
//!
//! - 推理进程换成假的（[`FakeLauncher`]）：ORT 工作进程按真实的帧协议回应 `ping`，llama-server 在约定的端口上回应
//!   `/health`；启动快慢由一道闸门控制（关着时停在「启动中」），进程退出可以随时模拟。显卡检测（`--list-devices`）
//!   报告一块集成显卡，快慢由另一道闸门控制；「是不是 Apple 芯片」可以固定，Apple 芯片上也能走显卡检测的路径。
//! - ORT 槽位的用例跑在暂停的时钟上（`start_paused`）：闲置释放的分钟数可以瞬间走完，启动等待在阻塞线程里，
//!   时钟不会在等它时自己前进。llama 槽位与下载用例要真的网络往返，用真实时钟。
//! - 模型目录是临时目录，按 `Store` 的布局写入假的模型文件与安装记录；下载走本机的模拟服务器（wiremock）。

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use wisp_local::RuntimeFailure;
use wisp_local::catalog::{self, Target};
use wisp_local::download::{Fetcher, Remote, Tuning};
use wisp_local::launch::{LaunchSpec, Launcher, ManagedChild};
use wisp_local::store::{InstalledFile, InstalledManifest, Store};
use wisp_local::worker::frame::{self, Request, Response};
use wisp_platform::pressure::Level;
use wisp_platform::system::MemoryInfo;
use wisp_types::{ErrorCode, FitnessLevel, LlmError, LoadState, LocalModelId, LocalModelState, OfflineSettings};

use super::lifecycle::{Lease, MemoryProbe, StartGuard};
use super::slots::{Failure, Phase};
use super::{ModelsService, SlotKind};
use crate::settings::SettingsService;

const TINY: LocalModelId = LocalModelId::PpOcrV6Tiny;
const SMALL: LocalModelId = LocalModelId::PpOcrV6Small;
const VL: LocalModelId = LocalModelId::PaddleOcrVl16;
const T2B: LocalModelId = LocalModelId::IndexTranslate2b;
const T9B: LocalModelId = LocalModelId::IndexTranslate9b;
const GB: u64 = 1024 * 1024 * 1024;
/// 闲置释放的默认时长（`offline.idle_release_min = 5`）
const IDLE: Duration = Duration::from_secs(5 * 60);

// ---- 假的推理进程 ----

/// 一道闸门：关着时假进程停在启动阶段（ORT 不回 `ping`、llama-server 不开端口）
struct Gate {
    open: Mutex<bool>,
    cv: Condvar,
}

impl Gate {
    fn new() -> Self {
        Self {
            open: Mutex::new(true),
            cv: Condvar::new(),
        }
    }

    fn set(&self, open: bool) {
        *self.open.lock() = open;
        self.cv.notify_all();
    }

    fn wait(&self) {
        let mut g = self.open.lock();
        while !*g {
            self.cv.wait(&mut g);
        }
    }
}

/// 管道的写端；关掉（`close`）或最后一个句柄丢弃后读端读到 EOF
#[derive(Clone)]
struct Out(Arc<Mutex<Option<mpsc::Sender<Vec<u8>>>>>);

impl Out {
    fn close(&self) {
        self.0.lock().take();
    }
}

impl Write for Out {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        match self.0.lock().as_ref() {
            Some(tx) if tx.send(b.to_vec()).is_ok() => Ok(b.len()),
            _ => Err(io::Error::from(io::ErrorKind::BrokenPipe)),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// 管道的读端
struct In {
    rx: mpsc::Receiver<Vec<u8>>,
    buf: Vec<u8>,
}

impl Read for In {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.buf.is_empty() {
            match self.rx.recv() {
                Ok(b) => self.buf = b,
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len());
        out[..n].copy_from_slice(&self.buf[..n]);
        self.buf.drain(..n);
        Ok(n)
    }
}

fn pipe() -> (Out, In) {
    let (tx, rx) = mpsc::channel();
    (Out(Arc::new(Mutex::new(Some(tx)))), In { rx, buf: Vec::new() })
}

/// 一个假进程的状态
struct FakeProc {
    pid: u32,
    exited: Mutex<bool>,
    cv: Condvar,
    /// 进程退出时关掉的输出管道（读它的线程随之结束）
    outputs: Mutex<Vec<Out>>,
}

impl FakeProc {
    fn exited(&self) -> bool {
        *self.exited.lock()
    }

    /// 进程退出（正常结束、被结束或崩溃）
    fn exit(&self) {
        *self.exited.lock() = true;
        self.cv.notify_all();
        for o in self.outputs.lock().drain(..) {
            o.close();
        }
    }

    fn wait(&self, timeout: Duration) -> bool {
        let mut g = self.exited.lock();
        if !*g {
            let _ = self.cv.wait_for(&mut g, timeout);
        }
        *g
    }
}

struct FakeChild {
    proc: Arc<FakeProc>,
    stdin: Option<Box<dyn Write + Send>>,
    stdout: Option<Box<dyn Read + Send>>,
    stderr: Option<Box<dyn Read + Send>>,
}

impl ManagedChild for FakeChild {
    fn pid(&self) -> u32 {
        self.proc.pid
    }
    fn take_stdin(&mut self) -> Option<Box<dyn Write + Send>> {
        self.stdin.take()
    }
    fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
        self.stdout.take()
    }
    fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
        self.stderr.take()
    }
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        Ok(self.proc.exited().then(ExitStatus::default))
    }
    fn wait_timeout(&mut self, timeout: Duration) -> io::Result<Option<ExitStatus>> {
        Ok(self.proc.wait(timeout).then(ExitStatus::default))
    }
    fn kill(&mut self) -> io::Result<()> {
        self.proc.exit();
        Ok(())
    }
}

impl Drop for FakeChild {
    // 与平台层一样：子进程句柄析构即结束进程
    fn drop(&mut self) {
        self.proc.exit();
    }
}

/// 假的显卡检测输出：只有一块集成显卡（Windows 上 Vulkan 报告的名称与共享内存）
const INTEGRATED_GPU_LIST: &str =
    "Available devices:\n  Vulkan0: Intel(R) UHD Graphics 770 (8192 MiB, 7000 MiB free)\n";

struct FakeLauncher {
    gate: Arc<Gate>,
    /// 显卡检测的闸门：关着时检测停在那里
    probe_gate: Arc<Gate>,
    /// 推理进程（不含显卡检测）
    procs: Mutex<Vec<Arc<FakeProc>>>,
    next_pid: AtomicU32,
    /// 每次启动推理进程时还活着的推理进程数（核对同一槽位的新旧进程不共存）
    live_at_launch: Mutex<Vec<usize>>,
    /// 显卡检测的次数
    probes: AtomicU32,
}

impl FakeLauncher {
    fn new() -> Self {
        Self {
            gate: Arc::new(Gate::new()),
            probe_gate: Arc::new(Gate::new()),
            procs: Mutex::new(Vec::new()),
            // 远大于真实 pid 的范围：万一有人拿它去结束进程也落空
            next_pid: AtomicU32::new(4_000_000),
            live_at_launch: Mutex::new(Vec::new()),
            probes: AtomicU32::new(0),
        }
    }

    fn launches(&self) -> usize {
        self.procs.lock().len()
    }

    fn probes(&self) -> u32 {
        self.probes.load(Ordering::SeqCst)
    }

    fn proc(&self, pid: u32) -> Arc<FakeProc> {
        Arc::clone(self.procs.lock().iter().find(|p| p.pid == pid).expect("launched"))
    }

    fn all_exited(&self) -> bool {
        self.procs.lock().iter().all(|p| p.exited())
    }

    fn new_proc(&self) -> Arc<FakeProc> {
        let proc = self.detached_proc();
        let mut procs = self.procs.lock();
        let live = procs.iter().filter(|p| !p.exited()).count();
        self.live_at_launch.lock().push(live);
        procs.push(Arc::clone(&proc));
        proc
    }

    /// 不计入推理进程的假进程（显卡检测）
    fn detached_proc(&self) -> Arc<FakeProc> {
        Arc::new(FakeProc {
            pid: self.next_pid.fetch_add(1, Ordering::SeqCst),
            exited: Mutex::new(false),
            cv: Condvar::new(),
            outputs: Mutex::new(Vec::new()),
        })
    }

    /// `llama-server --list-devices`：闸门打开后输出一块集成显卡并退出
    fn list_devices(&self) -> FakeChild {
        self.probes.fetch_add(1, Ordering::SeqCst);
        let proc = self.detached_proc();
        let (mut out, stdout) = pipe();
        proc.outputs.lock().push(out.clone());
        let gate = Arc::clone(&self.probe_gate);
        let p = Arc::clone(&proc);
        std::thread::spawn(move || {
            gate.wait();
            let _ = out.write_all(INTEGRATED_GPU_LIST.as_bytes());
            p.exit();
        });
        FakeChild {
            proc,
            stdin: None,
            stdout: Some(Box::new(stdout)),
            stderr: None,
        }
    }

    /// ORT 工作进程：按帧协议一问一答，stdin 读到 EOF 即退出
    fn ort(&self, proc: Arc<FakeProc>) -> FakeChild {
        let (stdin, mut requests) = pipe();
        let (mut replies, stdout) = pipe();
        proc.outputs.lock().push(replies.clone());
        let gate = Arc::clone(&self.gate);
        let p = Arc::clone(&proc);
        std::thread::spawn(move || {
            while let Ok(Some(f)) = frame::read_frame(&mut requests) {
                let resp = match serde_json::from_slice::<Request>(&f.header) {
                    Ok(Request::Ping) => {
                        gate.wait();
                        Response::ok()
                    }
                    Ok(Request::Exit) | Err(_) => break,
                    Ok(_) => Response {
                        ok: true,
                        lines: Some(Vec::new()),
                        boxes: Some(Vec::new()),
                        ..Response::default()
                    },
                };
                let header = serde_json::to_vec(&resp).expect("json");
                if frame::write_frame(&mut replies, &header, &[]).is_err() {
                    break;
                }
            }
            p.exit();
        });
        FakeChild {
            proc,
            stdin: Some(Box::new(stdin)),
            stdout: Some(Box::new(stdout)),
            stderr: None,
        }
    }

    /// llama-server（经守护进程）：闸门打开后在 `--port` 上回应 `/health`，stdin 读到 EOF 即退出
    fn llama(&self, proc: Arc<FakeProc>, args: &[String]) -> FakeChild {
        let (stdin, mut input) = pipe();
        let (out, stdout) = pipe();
        let (err, stderr) = pipe();
        proc.outputs.lock().extend([out, err]);
        let p = Arc::clone(&proc);
        std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            while matches!(input.read(&mut buf), Ok(n) if n > 0) {}
            p.exit();
        });
        let port: Option<u16> = args
            .iter()
            .position(|a| a == "--port")
            .and_then(|i| args.get(i + 1))
            .and_then(|p| p.parse().ok());
        if let Some(port) = port {
            let gate = Arc::clone(&self.gate);
            let p = Arc::clone(&proc);
            std::thread::spawn(move || serve_health(&gate, &p, port));
        }
        FakeChild {
            proc,
            stdin: Some(Box::new(stdin)),
            stdout: Some(Box::new(stdout)),
            stderr: Some(Box::new(stderr)),
        }
    }
}

/// 假 llama-server 的 HTTP：任何请求都回 200（只用到 `/health`）
fn serve_health(gate: &Gate, proc: &FakeProc, port: u16) {
    gate.wait();
    let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", port)) else {
        return;
    };
    let _ = listener.set_nonblocking(true);
    while !proc.exited() {
        match listener.accept() {
            Ok((mut s, _)) => {
                let _ = s.set_nonblocking(false);
                let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut buf) {
                        Ok(n) if n > 0 => req.extend_from_slice(&buf[..n]),
                        _ => break,
                    }
                }
                let body = "{\"status\":\"ok\"}";
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => return,
        }
    }
}

impl Launcher for FakeLauncher {
    fn launch(&self, spec: LaunchSpec) -> io::Result<Box<dyn ManagedChild>> {
        let args: Vec<String> = spec.args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        if args.first().is_some_and(|a| a == "--list-devices") {
            return Ok(Box::new(self.list_devices()));
        }
        let proc = self.new_proc();
        let child = if args.first().is_some_and(|a| a == "--wisp-worker") {
            self.ort(proc)
        } else {
            self.llama(proc, &args)
        };
        Ok(Box::new(child))
    }

    fn current_exe(&self) -> io::Result<PathBuf> {
        Ok(PathBuf::from("/opt/wisp/wisp"))
    }

    fn write_pid_file(&self, path: &Path, pid: u32) -> io::Result<()> {
        std::fs::create_dir_all(path.parent().expect("parent"))?;
        std::fs::write(path, format!("pid={pid}\nstart=0\nexe=/opt/wisp/wisp\n"))
    }
}

// ---- 临时的模型目录与服务 ----

/// 写一组假文件与安装记录（库存只核对安装记录与大小）
fn install_files(store: &Store, dir: &Path, names: &[&str]) {
    let files = names
        .iter()
        .map(|n| {
            let path = dir.join(n);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(&path, b"fake").expect("write");
            InstalledFile {
                name: (*n).to_owned(),
                size: 4,
                sha256: String::new(),
            }
        })
        .collect();
    store
        .write_manifest(
            dir,
            &InstalledManifest {
                catalog_revision: catalog::CATALOG_REVISION,
                files,
                installed_at: 0,
            },
        )
        .expect("manifest");
}

fn install_model(store: &Store, model: LocalModelId) {
    let names: Vec<&str> = catalog::model(model).files.iter().map(|f| f.name).collect();
    install_files(store, &store.model_dir(model), &names);
}

fn install_runtimes(store: &Store, model: LocalModelId) {
    let target = Target::current().expect("target");
    for rt in catalog::model(model).runtimes {
        let main = catalog::runtime_asset(*rt, target).expect("asset").main;
        install_files(store, &store.runtime_dir(*rt), &[main]);
    }
}

/// 内存充裕、没有压力（加载前的检查不随测试机器变化）
fn roomy() -> MemoryProbe {
    MemoryProbe {
        memory: Some(MemoryInfo {
            total: 32 * GB,
            available: 16 * GB,
        }),
        pressure: Some(Level::Normal),
        compressing: false,
    }
}

struct Harness {
    _settings_dir: tempfile::TempDir,
    _models_dir: tempfile::TempDir,
    settings: Arc<SettingsService>,
    svc: Arc<ModelsService>,
    launcher: Arc<FakeLauncher>,
    store: Store,
}

/// 装好这些模型（连同它们需要的推理组件）的服务；不在发布范围的平台上返回 None（用例跳过）
fn harness(models: &[LocalModelId]) -> Option<Harness> {
    Target::current()?;
    let (settings_dir, settings) = crate::settings::service::tests::test_service();
    let models_dir = tempfile::tempdir().expect("tempdir");
    let store = Store::new(models_dir.path().join("models"));
    for m in models {
        install_model(&store, *m);
        install_runtimes(&store, *m);
    }
    let launcher = Arc::new(FakeLauncher::new());
    let dyn_launcher: Arc<dyn Launcher> = Arc::clone(&launcher) as Arc<dyn Launcher>;
    let svc = ModelsService::with_parts(Arc::clone(&settings), None, store.root().to_path_buf(), dyn_launcher);
    *svc.memory_override.lock() = Some(roomy());
    Some(Harness {
        _settings_dir: settings_dir,
        _models_dir: models_dir,
        settings,
        svc,
        launcher,
        store,
    })
}

/// 等条件成立（真实时间的上限；每次让出一下，让其他任务与阻塞线程推进）
async fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(std::time::Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn phase(svc: &ModelsService, kind: SlotKind) -> &'static str {
    match &svc.slots.get(kind).inner.lock().phase {
        Phase::Stopped => "stopped",
        Phase::Starting { .. } => "starting",
        Phase::Ready(_) => "ready",
    }
}

fn ready_pid(svc: &ModelsService, kind: SlotKind) -> Option<u32> {
    match &svc.slots.get(kind).inner.lock().phase {
        Phase::Ready(l) => Some(l.pid),
        _ => None,
    }
}

fn busy(svc: &ModelsService, kind: SlotKind) -> usize {
    svc.slots.get(kind).inner.lock().busy
}

fn spawn_acquire(
    svc: &Arc<ModelsService>,
    kind: SlotKind,
    model: LocalModelId,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<Result<Lease, LlmError>> {
    let svc = Arc::clone(svc);
    tokio::spawn(async move { svc.acquire(kind, model, &cancel, || async {}).await })
}

async fn acquire(svc: &Arc<ModelsService>, kind: SlotKind, model: LocalModelId) -> Lease {
    svc.acquire(kind, model, &CancellationToken::new(), || async {})
        .await
        .expect("lease")
}

fn lease_pid(lease: &Lease) -> u32 {
    lease.ort().map(wisp_local::worker::OrtWorker::pid).expect("ort lease")
}

fn pid_files(store: &Store) -> usize {
    std::fs::read_dir(store.run_dir()).map_or(0, |d| {
        d.flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "pid"))
            .count()
    })
}

// ---- 槽位生命周期（ORT 槽位，暂停的时钟） ----

/// 并发的两个请求只启动一次；两个都拿到同一个进程
#[tokio::test(start_paused = true)]
async fn concurrent_requests_start_the_slot_once() {
    let Some(h) = harness(&[TINY]) else { return };
    h.launcher.gate.set(false);
    let a = spawn_acquire(&h.svc, SlotKind::Ort, TINY, CancellationToken::new());
    let b = spawn_acquire(&h.svc, SlotKind::Ort, SMALL, CancellationToken::new());
    eventually("starting", || {
        h.svc.slots.loads().get(&TINY) == Some(&LoadState::Loading) && h.launcher.launches() == 1
    })
    .await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    h.launcher.gate.set(true);
    let a = a.await.expect("join").expect("lease a");
    let b = b.await.expect("join").expect("lease b");
    assert_eq!(h.launcher.launches(), 1, "单飞：只启动一次");
    assert_eq!(lease_pid(&a), lease_pid(&b));
    assert_eq!(busy(&h.svc, SlotKind::Ort), 2);
    // 两个 PP-OCR 档位都在同一个进程里
    let loads = h.svc.slots.loads();
    assert_eq!(loads.get(&TINY), Some(&LoadState::Loaded));
    assert_eq!(loads.get(&SMALL), Some(&LoadState::Loaded));
    drop((a, b));
    assert_eq!(busy(&h.svc, SlotKind::Ort), 0);
}

/// 请求在启动期间取消：只是不再等，启动照常完成；没人用时由闲置定时器释放
#[tokio::test(start_paused = true)]
async fn cancelling_a_request_does_not_stop_the_start() {
    let Some(h) = harness(&[TINY]) else { return };
    h.launcher.gate.set(false);
    let cancel = CancellationToken::new();
    let a = spawn_acquire(&h.svc, SlotKind::Ort, TINY, cancel.clone());
    eventually("starting", || h.launcher.launches() == 1).await;
    cancel.cancel();
    let err = a.await.expect("join").err().expect("cancelled");
    assert_eq!(err.code, ErrorCode::Cancelled);
    assert_eq!(phase(&h.svc, SlotKind::Ort), "starting", "启动不受影响");
    h.launcher.gate.set(true);
    eventually("ready", || phase(&h.svc, SlotKind::Ort) == "ready").await;
    tokio::time::sleep(IDLE - Duration::from_secs(1)).await;
    assert_eq!(phase(&h.svc, SlotKind::Ort), "ready");
    tokio::time::sleep(Duration::from_secs(2)).await;
    eventually("idle release", || {
        phase(&h.svc, SlotKind::Ort) == "stopped" && h.launcher.all_exited()
    })
    .await;
    eventually("pid file removed", || pid_files(&h.store) == 0).await;
}

/// 「立即释放内存」：启动中的让它启动完、等着的请求照常拿到（不是「已取消」），做完再停；空闲的立即停；忙的做完再停
#[tokio::test(start_paused = true)]
async fn release_lets_starting_and_busy_slots_finish_their_requests() {
    let Some(h) = harness(&[TINY]) else { return };
    // 启动中
    h.launcher.gate.set(false);
    let a = spawn_acquire(&h.svc, SlotKind::Ort, TINY, CancellationToken::new());
    eventually("starting", || h.launcher.launches() == 1).await;
    h.svc.release().await.expect("release");
    h.launcher.gate.set(true);
    let lease = a.await.expect("join").expect("请求照常拿到槽位");
    assert_eq!(phase(&h.svc, SlotKind::Ort), "ready");
    drop(lease);
    eventually("stopped after the request", || {
        phase(&h.svc, SlotKind::Ort) == "stopped" && h.launcher.all_exited()
    })
    .await;
    // 空闲：立即停
    drop(acquire(&h.svc, SlotKind::Ort, TINY).await);
    assert_eq!(phase(&h.svc, SlotKind::Ort), "ready");
    h.svc.release().await.expect("release");
    eventually("idle stopped", || {
        phase(&h.svc, SlotKind::Ort) == "stopped" && h.launcher.all_exited()
    })
    .await;
    // 忙：请求做完再停
    let lease = acquire(&h.svc, SlotKind::Ort, TINY).await;
    h.svc.release().await.expect("release");
    assert_eq!(phase(&h.svc, SlotKind::Ort), "ready");
    assert!(!lease.cancelled().is_cancelled(), "不打断进行中的请求");
    drop(lease);
    eventually("busy stopped", || {
        phase(&h.svc, SlotKind::Ort) == "stopped" && h.launcher.all_exited()
    })
    .await;
    assert_eq!(h.launcher.launches(), 3);
}

/// 删除模型（强制）：启动中的取消启动、等着的请求以「已取消」结束、进程不留；忙的请求被打断
#[tokio::test(start_paused = true)]
async fn deleting_a_model_stops_starting_and_busy_slots() {
    let Some(h) = harness(&[TINY, SMALL]) else { return };
    h.launcher.gate.set(false);
    let a = spawn_acquire(&h.svc, SlotKind::Ort, TINY, CancellationToken::new());
    eventually("starting", || h.launcher.launches() == 1).await;
    h.svc.delete(TINY).await.expect("delete");
    assert_eq!(phase(&h.svc, SlotKind::Ort), "stopped");
    h.launcher.gate.set(true);
    let err = a.await.expect("join").err().expect("cancelled");
    assert_eq!(err.code, ErrorCode::Cancelled);
    eventually("no process left", || h.launcher.all_exited()).await;
    assert!(!h.store.model_dir(TINY).exists());
    // 忙：进行中的请求收到强制停止，放手后进程结束
    let lease = acquire(&h.svc, SlotKind::Ort, SMALL).await;
    let forced = lease.cancelled();
    let svc = Arc::clone(&h.svc);
    let deleting = tokio::spawn(async move { svc.delete(SMALL).await });
    forced.cancelled().await;
    drop(lease);
    deleting.await.expect("join").expect("delete");
    assert_eq!(phase(&h.svc, SlotKind::Ort), "stopped");
    eventually("no process left", || h.launcher.all_exited()).await;
    assert_eq!(pid_files(&h.store), 0);
}

/// 闲置到期前来了新请求：定时器作废，请求结束后重新计时
#[tokio::test(start_paused = true)]
async fn a_new_lease_before_the_idle_deadline_keeps_the_slot() {
    let Some(h) = harness(&[TINY]) else { return };
    drop(acquire(&h.svc, SlotKind::Ort, TINY).await);
    let pid = ready_pid(&h.svc, SlotKind::Ort).expect("ready");
    tokio::time::sleep(IDLE - Duration::from_secs(1)).await;
    let lease = acquire(&h.svc, SlotKind::Ort, TINY).await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(
        ready_pid(&h.svc, SlotKind::Ort),
        Some(pid),
        "原来的期限过了也不停：有人在用"
    );
    drop(lease);
    tokio::time::sleep(IDLE - Duration::from_secs(1)).await;
    assert_eq!(ready_pid(&h.svc, SlotKind::Ort), Some(pid));
    tokio::time::sleep(Duration::from_secs(2)).await;
    eventually("idle release", || phase(&h.svc, SlotKind::Ort) == "stopped").await;
    assert_eq!(h.launcher.launches(), 1);
}

/// 进程在两次请求之间退出（被系统结束、崩溃）：下一次请求重新启动；旧进程的闲置定时器不会停掉新进程
#[tokio::test(start_paused = true)]
async fn a_process_that_died_is_restarted_by_the_next_request() {
    let Some(h) = harness(&[TINY]) else { return };
    let lease = acquire(&h.svc, SlotKind::Ort, TINY).await;
    let first = lease_pid(&lease);
    drop(lease);
    h.launcher.proc(first).exit();
    let lease = acquire(&h.svc, SlotKind::Ort, TINY).await;
    let second = lease_pid(&lease);
    assert_ne!(first, second);
    assert_eq!(h.launcher.launches(), 2);
    // 新进程写了自己的 pid 文件；旧进程的收尾不会把它删掉
    eventually("old one cleaned up", || h.launcher.proc(first).exited()).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    let text = std::fs::read_to_string(h.store.pid_file("ort")).expect("pid file");
    assert!(text.starts_with(&format!("pid={second}\n")));
    tokio::time::sleep(IDLE / 2).await;
    assert_eq!(ready_pid(&h.svc, SlotKind::Ort), Some(second));
    drop(lease);
}

/// 两个 PP-OCR 档位之间切换不重启 ORT 工作进程；不再用离线 OCR 时才停
#[tokio::test(start_paused = true)]
async fn switching_between_ppocr_tiers_keeps_the_worker() {
    let Some(h) = harness(&[TINY, SMALL]) else { return };
    drop(acquire(&h.svc, SlotKind::Ort, TINY).await);
    let pid = ready_pid(&h.svc, SlotKind::Ort).expect("ready");
    let with = |m: Option<LocalModelId>| OfflineSettings {
        ocr_model: m,
        ..OfflineSettings::default()
    };
    h.svc.on_offline_settings(&with(Some(TINY)), &with(Some(SMALL))).await;
    assert_eq!(ready_pid(&h.svc, SlotKind::Ort), Some(pid), "同一个进程服务两个档位");
    h.svc.on_offline_settings(&with(Some(SMALL)), &with(None)).await;
    eventually("stopped", || {
        phase(&h.svc, SlotKind::Ort) == "stopped" && h.launcher.all_exited()
    })
    .await;
}

/// 加载前的内存检查：不够时不启动进程，报内存不足
#[tokio::test(start_paused = true)]
async fn not_enough_memory_refuses_to_start() {
    let Some(h) = harness(&[TINY]) else { return };
    *h.svc.memory_override.lock() = Some(MemoryProbe {
        memory: Some(MemoryInfo {
            total: GB,
            available: 100 * 1024 * 1024,
        }),
        pressure: None,
        compressing: false,
    });
    let err = h
        .svc
        .acquire(SlotKind::Ort, TINY, &CancellationToken::new(), || async {})
        .await
        .err()
        .expect("refused");
    assert_eq!(err.code, ErrorCode::LocalRuntime);
    assert_eq!(err.hint.as_deref(), Some("errors.hint.local.out_of_memory"));
    assert_eq!(h.launcher.launches(), 0);
}

/// 退出：所有进程结束、pid 文件删除；之后不再接受请求与下载
#[tokio::test(start_paused = true)]
async fn shutdown_leaves_no_process_and_no_pid_file() {
    let Some(h) = harness(&[TINY]) else { return };
    let lease = acquire(&h.svc, SlotKind::Ort, TINY).await;
    drop(lease);
    assert_eq!(pid_files(&h.store), 1);
    let svc = Arc::clone(&h.svc);
    tokio::task::spawn_blocking(move || svc.shutdown())
        .await
        .expect("shutdown");
    assert!(h.launcher.all_exited(), "没有残留进程");
    assert_eq!(pid_files(&h.store), 0);
    assert_eq!(phase(&h.svc, SlotKind::Ort), "stopped");
    assert!(
        h.svc
            .acquire(SlotKind::Ort, TINY, &CancellationToken::new(), || async {})
            .await
            .is_err()
    );
    assert!(h.svc.download(SMALL, false).await.is_err());
    // 幂等
    h.svc.shutdown();
}

// ---- llama 槽位（真实时钟：健康检查是真的 HTTP） ----

/// 翻译槽位换模型：停掉旧进程再启动新的；设置里换走了正在加载的模型时停掉它
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switching_translation_models_replaces_the_process() {
    let Some(h) = harness(&[T2B, T9B]) else { return };
    let lease = acquire(&h.svc, SlotKind::Translate, T2B).await;
    assert!(lease.endpoint().is_some());
    let first = ready_pid(&h.svc, SlotKind::Translate).expect("ready");
    drop(lease);
    let lease = acquire(&h.svc, SlotKind::Translate, T9B).await;
    let second = ready_pid(&h.svc, SlotKind::Translate).expect("ready");
    assert_ne!(first, second);
    assert!(h.launcher.proc(first).exited(), "旧模型的进程已经结束");
    assert_eq!(h.launcher.launches(), 2);
    let text = std::fs::read_to_string(h.store.pid_file("translate")).expect("pid file");
    assert!(text.starts_with(&format!("pid={second}\n")));
    drop(lease);
    let with = |m: Option<LocalModelId>| OfflineSettings {
        translate_model: m,
        ..OfflineSettings::default()
    };
    h.svc.on_offline_settings(&with(Some(T9B)), &with(Some(T2B))).await;
    eventually("stopped", || {
        phase(&h.svc, SlotKind::Translate) == "stopped" && h.launcher.all_exited()
    })
    .await;
    assert_eq!(pid_files(&h.store), 0);
}

/// 安装后预热：用得上显卡时在后台启动一次（期间显示「加载中」），就绪后立即停止
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warm_up_starts_once_and_stops_when_ready() {
    let Some(h) = harness(&[T2B]) else { return };
    let gpu = h.svc.warm_up_worthwhile();
    h.launcher.gate.set(false);
    h.svc.spawn_warm_up(T2B);
    if !gpu {
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(h.launcher.launches(), 0, "没有显卡时不预热");
        return;
    }
    eventually("loading", || h.svc.slots.loads().get(&T2B) == Some(&LoadState::Loading)).await;
    h.launcher.gate.set(true);
    eventually("warmed and stopped", || {
        h.launcher.launches() == 1 && phase(&h.svc, SlotKind::Translate) == "stopped" && h.launcher.all_exited()
    })
    .await;
    // 槽位已经在用时不预热（不打扰正在使用的进程）
    let lease = acquire(&h.svc, SlotKind::Translate, T2B).await;
    h.svc.spawn_warm_up(T2B);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(phase(&h.svc, SlotKind::Translate), "ready");
    assert_eq!(h.launcher.launches(), 2);
    drop(lease);
}

// ---- 下载队列（本机的模拟下载源） ----

const A_LEN: u64 = 1000;
const A_SHA: &str = "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3";
const B_LEN: u64 = 4000;
const B_SHA: &str = "488c3c0aa47c6eb3e4f0154f6d0ff747ceaa57d0a6ebaaf00c0a03ce5cadbb0b";

/// 假的下载：每个模型两个文件（`a.bin` 立即返回，`b.bin` 由用例决定快慢），断点续传落在模型的部分文件目录里，
/// 下完后按目录写入假的模型文件与安装记录。`order` 记下请求过模型文件的任务。整个任务不碰推理组件（当作已经装好）；
/// 「只装推理组件」那一段（先检测显卡再下模型）写入推理组件的安装记录
fn fake_download(server: String, order: Arc<Mutex<Vec<LocalModelId>>>) -> super::queue::TestDownload {
    Arc::new(move |store, job, progress, cancel, runtimes_only| {
        let server = server.clone();
        let order = Arc::clone(&order);
        Box::pin(async move {
            if runtimes_only {
                install_runtimes(&store, job.model);
                return Ok(());
            }
            order.lock().push(job.model);
            let dir = store.partial_dir(job.model);
            std::fs::create_dir_all(&dir)?;
            let tuning = Tuning {
                retries_per_source: 1,
                retry_backoff: Duration::from_millis(10),
                direct: true,
                ..Tuning::default()
            };
            let mut f = Fetcher::new(&*progress, A_LEN + B_LEN, 0, cancel, tuning)?;
            for (name, size, sha256) in [("a.bin", A_LEN, A_SHA), ("b.bin", B_LEN, B_SHA)] {
                let staged = dir.join(name);
                if std::fs::metadata(&staged).is_ok_and(|m| m.len() == size) {
                    continue;
                }
                let url = format!("{server}/{}/{name}", job.model.as_str());
                let part = dir.join(format!("{name}.part"));
                f.file(
                    &Remote {
                        urls: &[url.as_str()],
                        size,
                        sha256,
                    },
                    &part,
                )
                .await?;
                std::fs::rename(&part, &staged)?;
            }
            install_model(&store, job.model);
            store.discard_partial(job.model)?;
            Ok(())
        })
    })
}

async fn mount(server: &MockServer, model: LocalModelId, b_delay: Duration) {
    Mock::given(method("GET"))
        .and(path(format!("/{}/a.bin", model.as_str())))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'a'; A_LEN as usize]))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/{}/b.bin", model.as_str())))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(vec![b'b'; B_LEN as usize])
                .set_delay(b_delay),
        )
        .mount(server)
        .await;
}

/// 卡住 `b.bin`（用例里暂停 / 取消 / 删除用）
const HOLD: Duration = Duration::from_secs(60);

async fn state_of(svc: &ModelsService, model: LocalModelId) -> LocalModelState {
    svc.get()
        .await
        .expect("snapshot")
        .models
        .into_iter()
        .find(|m| m.id == model)
        .expect("model")
        .state
}

async fn until_state(svc: &ModelsService, model: LocalModelId, what: &str, cond: impl Fn(&LocalModelState) -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let s = state_of(svc, model).await;
        if cond(&s) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}: {s:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn staged(store: &Store, model: LocalModelId) -> bool {
    store.partial_dir(model).join("a.bin").is_file()
}

/// 同一时刻只下载一个、按点击的先后；暂停后显示「已暂停」并开始下一个；「下载并使用」装好后才选用；继续时跳过已下完的文件
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_queue_runs_one_download_at_a_time_and_selects_after_install() {
    let Some(h) = harness(&[]) else { return };
    let server = MockServer::start().await;
    mount(&server, TINY, HOLD).await;
    mount(&server, SMALL, Duration::ZERO).await;
    let order = Arc::new(Mutex::new(Vec::new()));
    *h.svc.download_override.lock() = Some(fake_download(server.uri(), Arc::clone(&order)));

    h.svc.download(TINY, false).await.expect("download");
    h.svc.download(SMALL, true).await.expect("download");
    assert_eq!(state_of(&h.svc, SMALL).await, LocalModelState::Queued);
    eventually("tiny half way", || staged(&h.store, TINY)).await;
    assert!(matches!(
        state_of(&h.svc, TINY).await,
        LocalModelState::Downloading { .. }
    ));
    assert_eq!(h.settings.get().offline.ocr_model, None, "下载期间不切换");

    h.svc.pause(TINY).await.expect("pause");
    until_state(&h.svc, SMALL, "small installed", |s| *s == LocalModelState::Installed).await;
    let paused = state_of(&h.svc, TINY).await;
    assert!(
        matches!(paused, LocalModelState::Paused { done, total } if done > 0 && done <= total),
        "{paused:?}"
    );
    assert_eq!(*order.lock(), vec![TINY, SMALL]);
    // 先重读库存（编排器马上能用它）再写设置
    eventually("selected after install", || {
        h.settings.get().offline.ocr_model == Some(SMALL)
    })
    .await;

    // 继续：已经下完并校验过的 a.bin 不再请求
    server.reset().await;
    mount(&server, TINY, Duration::ZERO).await;
    h.svc.download(TINY, false).await.expect("resume");
    until_state(&h.svc, TINY, "tiny installed", |s| *s == LocalModelState::Installed).await;
    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        requests.iter().all(|r| r.url.path().ends_with("/b.bin")),
        "只续传剩下的文件"
    );
    assert_eq!(h.settings.get().offline.ocr_model, Some(SMALL), "没要求选用的不改选择");
    assert!(!h.store.partial_dir(TINY).exists());
}

/// 取消删除部分文件、清掉「装好后选用」；失败的任务可以重试
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_discards_partials_and_failures_can_be_retried() {
    let Some(h) = harness(&[]) else { return };
    let server = MockServer::start().await;
    mount(&server, TINY, HOLD).await;
    *h.svc.download_override.lock() = Some(fake_download(server.uri(), Arc::new(Mutex::new(Vec::new()))));

    h.svc.download(TINY, true).await.expect("download");
    eventually("tiny half way", || staged(&h.store, TINY)).await;
    h.svc.cancel(TINY).await.expect("cancel");
    until_state(&h.svc, TINY, "cancelled", |s| *s == LocalModelState::NotInstalled).await;
    eventually("partials removed", || !h.store.partial_dir(TINY).exists()).await;

    // 源出错：失败，可以重试
    server.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    h.svc.download(TINY, false).await.expect("download");
    until_state(&h.svc, TINY, "failed", |s| matches!(s, LocalModelState::Failed { .. })).await;
    server.reset().await;
    mount(&server, TINY, Duration::ZERO).await;
    h.svc.download(TINY, false).await.expect("retry");
    until_state(&h.svc, TINY, "installed", |s| *s == LocalModelState::Installed).await;
    assert_eq!(h.settings.get().offline.ocr_model, None, "取消时已清掉「装好后选用」");
}

/// 删除正在下载的模型：等下载任务收尾（关掉部分文件）再删，删完什么都不剩
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_a_downloading_model_waits_for_the_download_to_stop() {
    let Some(h) = harness(&[]) else { return };
    let server = MockServer::start().await;
    mount(&server, TINY, HOLD).await;
    *h.svc.download_override.lock() = Some(fake_download(server.uri(), Arc::new(Mutex::new(Vec::new()))));
    h.svc.download(TINY, false).await.expect("download");
    eventually("tiny half way", || staged(&h.store, TINY)).await;
    h.svc.delete(TINY).await.expect("delete");
    assert!(h.svc.state.lock().downloads.current.is_none(), "下载任务已经收尾");
    assert!(!h.store.partial_dir(TINY).exists());
    assert!(!h.store.model_dir(TINY).exists());
    assert_eq!(state_of(&h.svc, TINY).await, LocalModelState::NotInstalled);
}

/// 「下载并使用」一个已经装好的模型：立即选用
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selecting_an_installed_model_applies_at_once() {
    let Some(h) = harness(&[T2B, VL]) else { return };
    h.svc.download(T2B, true).await.expect("select");
    h.svc.download(VL, true).await.expect("select");
    let offline = h.settings.get().offline;
    assert_eq!(offline.translate_model, Some(T2B));
    assert_eq!(offline.ocr_model, Some(VL));
    assert_eq!(h.launcher.launches(), 0, "选用本身不加载模型");
}

// ---- 槽位：新旧进程、预热、异常收尾、失败记录 ----

/// 同一槽位的新进程总在旧进程退出之后才启动：放手即停（释放内存时忙着的槽位）之后马上来的请求，等旧进程走完再启动
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_process_starts_only_after_the_old_one_exited() {
    let Some(h) = harness(&[TINY]) else { return };
    for _ in 0..8 {
        let lease = acquire(&h.svc, SlotKind::Ort, TINY).await;
        h.svc.release().await.expect("release");
        drop(lease);
        drop(acquire(&h.svc, SlotKind::Ort, TINY).await);
        h.svc.release().await.expect("release");
    }
    assert_eq!(h.launcher.launches(), 16);
    let live = h.launcher.live_at_launch.lock().clone();
    assert!(live.iter().all(|n| *n == 0), "启动时还有旧进程活着：{live:?}");
    eventually("all stopped", || h.launcher.all_exited()).await;
}

/// 预热与真实请求共享一次启动：预热最后放手时不停（用户刚用过），照常闲置后再释放
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_warm_up_shared_with_a_request_keeps_the_process() {
    let Some(h) = harness(&[T2B]) else { return };
    *h.svc.apple_override.lock() = Some(true);
    h.launcher.gate.set(false);
    h.svc.spawn_warm_up(T2B);
    eventually("loading", || h.svc.slots.loads().get(&T2B) == Some(&LoadState::Loading)).await;
    let request = spawn_acquire(&h.svc, SlotKind::Translate, T2B, CancellationToken::new());
    eventually("the request waits for the same start", || {
        h.svc.slots.get(SlotKind::Translate).inner.lock().wanted
    })
    .await;
    h.launcher.gate.set(true);
    let lease = request.await.expect("join").expect("lease");
    drop(lease);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(phase(&h.svc, SlotKind::Translate), "ready", "照常闲置，不立即停");
    assert_eq!(busy(&h.svc, SlotKind::Translate), 0, "预热已经放手");
    assert_eq!(h.launcher.launches(), 1);
    h.svc.release().await.expect("release");
    eventually("stopped", || h.launcher.all_exited()).await;
}

/// 启动任务没有正常收尾（任务被丢弃）：槽位回到「已停止」，等着的请求得到启动失败而不是在原地空转；
/// 被强制停止时登记的停止流程也随之注销，下一次请求照常启动
#[tokio::test(start_paused = true)]
async fn an_abandoned_start_does_not_leave_the_slot_loading() {
    let Some(h) = harness(&[TINY]) else { return };
    let (tx, rx) = watch::channel(None);
    {
        let mut g = h.svc.slots.get(SlotKind::Ort).inner.lock();
        g.phase = Phase::Starting {
            model: TINY,
            attempt: 7,
            done: rx,
            cancel: CancellationToken::new(),
        };
        g.next_generation = 8;
    }
    let waiting = spawn_acquire(&h.svc, SlotKind::Ort, TINY, CancellationToken::new());
    eventually("waiting", || tx.receiver_count() == 2).await;
    drop(StartGuard {
        svc: Arc::clone(&h.svc),
        kind: SlotKind::Ort,
        model: TINY,
        attempt: 7,
        tx: Some(tx),
    });
    let err = waiting.await.expect("join").err().expect("start failed");
    assert_eq!(err.hint.as_deref(), Some(RuntimeFailure::StartFailed.hint_key()));
    assert_eq!(phase(&h.svc, SlotKind::Ort), "stopped");
    assert_eq!(h.launcher.launches(), 0);
    // 被强制停止的启动：收尾时注销登记的停止流程
    let (tx, rx) = watch::channel(None);
    {
        let mut g = h.svc.slots.get(SlotKind::Ort).inner.lock();
        g.phase = Phase::Stopped;
        g.abandoned.insert(9);
        g.stopping += 1;
        drop(rx);
    }
    drop(StartGuard {
        svc: Arc::clone(&h.svc),
        kind: SlotKind::Ort,
        model: TINY,
        attempt: 9,
        tx: Some(tx),
    });
    {
        let g = h.svc.slots.get(SlotKind::Ort).inner.lock();
        assert_eq!(g.stopping, 0);
        assert!(g.abandoned.is_empty());
    }
    drop(acquire(&h.svc, SlotKind::Ort, TINY).await);
    assert_eq!(h.launcher.launches(), 1);
}

/// 重新下载、删除之后，旧的启动失败记录不再拦住请求（翻译模型没加载时也清）
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reinstalling_or_deleting_clears_the_failure_record() {
    let Some(h) = harness(&[]) else { return };
    install_runtimes(&h.store, TINY);
    let block = |kind: SlotKind, model: LocalModelId| {
        h.svc.slots.get(kind).inner.lock().failure = Some(Failure {
            model,
            error: RuntimeFailure::ModelCorrupt.to_llm_error(),
            at: Instant::now(),
            repeats: 2,
        });
    };
    block(SlotKind::Ort, TINY);
    let server = MockServer::start().await;
    mount(&server, TINY, Duration::ZERO).await;
    *h.svc.download_override.lock() = Some(fake_download(server.uri(), Arc::new(Mutex::new(Vec::new()))));
    h.svc.download(TINY, false).await.expect("download");
    until_state(&h.svc, TINY, "installed", |s| *s == LocalModelState::Installed).await;
    drop(acquire(&h.svc, SlotKind::Ort, TINY).await);
    assert_eq!(h.launcher.launches(), 1, "重新下载之后照常启动");
    // 删除没加载的翻译模型：它的失败记录同样清掉
    install_model(&h.store, T9B);
    block(SlotKind::Translate, T9B);
    h.svc.delete(T9B).await.expect("delete");
    assert!(h.svc.slots.get(SlotKind::Translate).inner.lock().failure.is_none());
}

// ---- 下载：先检测显卡、装好后选用 ----

/// 显卡待检测：先只装推理组件、检测显卡，只有核显时（较慢）停在「已暂停」，一个模型文件都没请求；引导页同时排的
/// PaddleOCR-VL 也停下来等确认。确认后再下载就装好；只有核显时不预热
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_gpu_check_runs_before_the_model_and_holds_slow_downloads() {
    let Some(h) = harness(&[]) else { return };
    *h.svc.apple_override.lock() = Some(false);
    let server = MockServer::start().await;
    mount(&server, T9B, Duration::ZERO).await;
    let order = Arc::new(Mutex::new(Vec::new()));
    *h.svc.download_override.lock() = Some(fake_download(server.uri(), Arc::clone(&order)));
    let fitness = |id: LocalModelId| {
        let svc = Arc::clone(&h.svc);
        async move {
            svc.get()
                .await
                .expect("snapshot")
                .models
                .into_iter()
                .find(|m| m.id == id)
                .expect("model")
                .fitness
                .level
        }
    };
    assert_eq!(fitness(T9B).await, FitnessLevel::Unknown);

    h.svc.download(T9B, false).await.expect("download");
    h.svc.download(VL, false).await.expect("download");
    until_state(&h.svc, T9B, "9B held", |s| matches!(s, LocalModelState::Paused { .. })).await;
    until_state(&h.svc, VL, "VL held", |s| matches!(s, LocalModelState::Paused { .. })).await;
    assert!(order.lock().is_empty(), "模型文件一个都没请求");
    assert_eq!(h.launcher.probes(), 1, "检测一次，VL 用同一个结论");
    assert_eq!(fitness(T9B).await, FitnessLevel::Slow);
    assert_eq!(fitness(VL).await, FitnessLevel::Slow);

    // 用户看过结论后继续：不再停
    h.svc.download(T9B, false).await.expect("resume");
    until_state(&h.svc, T9B, "9B installed", |s| *s == LocalModelState::Installed).await;
    assert_eq!(*order.lock(), vec![T9B]);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(h.launcher.launches(), 0, "只有核显：不预热");
}

/// 显卡检测期间照常响应暂停（按最后的进度显示「已暂停」）与取消
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pausing_or_cancelling_during_the_gpu_check_is_immediate() {
    let Some(h) = harness(&[]) else { return };
    *h.svc.apple_override.lock() = Some(false);
    h.launcher.probe_gate.set(false);
    let server = MockServer::start().await;
    let order = Arc::new(Mutex::new(Vec::new()));
    *h.svc.download_override.lock() = Some(fake_download(server.uri(), Arc::clone(&order)));

    h.svc.download(T9B, false).await.expect("download");
    eventually("probing", || h.launcher.probes() == 1).await;
    assert!(
        matches!(
            state_of(&h.svc, T9B).await,
            LocalModelState::Downloading { bytes_per_sec: 0, .. }
        ),
        "检测期间不显示过时的速度"
    );
    h.svc.pause(T9B).await.expect("pause");
    until_state(&h.svc, T9B, "paused", |s| matches!(s, LocalModelState::Paused { .. })).await;

    h.svc.download(T9B, false).await.expect("resume");
    eventually("probing again", || h.launcher.probes() == 2).await;
    h.svc.cancel(T9B).await.expect("cancel");
    until_state(&h.svc, T9B, "cancelled", |s| *s == LocalModelState::NotInstalled).await;
    assert!(order.lock().is_empty());
    h.launcher.probe_gate.set(true);
}

/// 「下载并使用」：装好时设置仍是登记时的样子才选用——下载期间用户改选了别的就不覆盖；同一类别只认最后一次登记
/// （另一个模型已经装好、立即选用，或者排在后面）
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selecting_after_install_respects_later_choices() {
    let Some(h) = harness(&[]) else { return };
    // 翻译模型不走显卡检测（与测试机器无关）；推理组件已经装好（装好的模型「下载并使用」时立即选用）
    *h.svc.apple_override.lock() = Some(true);
    install_runtimes(&h.store, SMALL);
    let server = MockServer::start().await;
    mount(&server, SMALL, HOLD).await;
    let order = Arc::new(Mutex::new(Vec::new()));
    *h.svc.download_override.lock() = Some(fake_download(server.uri(), Arc::clone(&order)));
    let set_ocr = |m: Option<LocalModelId>| {
        h.settings.update(&mut |s| s.offline.ocr_model = m).expect("settings");
    };
    // 下载期间改选了别的：装好后不覆盖
    h.svc.download(SMALL, true).await.expect("download");
    eventually("small half way", || staged(&h.store, SMALL)).await;
    set_ocr(Some(TINY));
    h.svc.pause(SMALL).await.expect("pause");
    until_state(&h.svc, SMALL, "paused", |s| matches!(s, LocalModelState::Paused { .. })).await;
    server.reset().await;
    mount(&server, SMALL, Duration::ZERO).await;
    h.svc.download(SMALL, false).await.expect("resume");
    until_state(&h.svc, SMALL, "small installed", |s| *s == LocalModelState::Installed).await;
    assert_eq!(h.settings.get().offline.ocr_model, Some(TINY), "用户后来的选择");

    // 先登记一个在下载的，再「下载并使用」一个已经装好的：立即选用后者，前者装好时不再覆盖
    mount(&server, TINY, HOLD).await;
    h.svc.download(TINY, true).await.expect("download");
    eventually("tiny half way", || staged(&h.store, TINY)).await;
    h.svc.download(SMALL, true).await.expect("select installed");
    assert_eq!(h.settings.get().offline.ocr_model, Some(SMALL));
    h.svc.pause(TINY).await.expect("pause");
    until_state(&h.svc, TINY, "paused", |s| matches!(s, LocalModelState::Paused { .. })).await;
    server.reset().await;
    mount(&server, TINY, Duration::ZERO).await;
    h.svc.download(TINY, false).await.expect("resume");
    until_state(&h.svc, TINY, "tiny installed", |s| *s == LocalModelState::Installed).await;
    assert_eq!(h.settings.get().offline.ocr_model, Some(SMALL));

    // 两个都在下载：只认后点的那个
    mount(&server, T2B, HOLD).await;
    mount(&server, T9B, Duration::ZERO).await;
    h.svc.download(T2B, true).await.expect("download");
    h.svc.download(T9B, true).await.expect("download");
    eventually("2B half way", || staged(&h.store, T2B)).await;
    h.svc.pause(T2B).await.expect("pause");
    until_state(&h.svc, T9B, "9B installed", |s| *s == LocalModelState::Installed).await;
    eventually("9B selected", || h.settings.get().offline.translate_model == Some(T9B)).await;
    server.reset().await;
    mount(&server, T2B, Duration::ZERO).await;
    h.svc.download(T2B, false).await.expect("resume");
    until_state(&h.svc, T2B, "2B installed", |s| *s == LocalModelState::Installed).await;
    assert_eq!(h.settings.get().offline.translate_model, Some(T9B));
}

/// 启动期间被要求停止、就绪那一刻还有请求在等，但它随后取消了、谁也没来取租约：不把进程留到闲置到期（默认 5 分钟），
/// 很快收尾
#[tokio::test(start_paused = true)]
async fn a_start_asked_to_stop_does_not_linger_when_its_waiter_leaves() {
    let Some(h) = harness(&[TINY]) else { return };
    h.launcher.gate.set(false);
    let cancel = CancellationToken::new();
    let request = spawn_acquire(&h.svc, SlotKind::Ort, TINY, cancel.clone());
    eventually("starting", || h.launcher.launches() == 1).await;
    // 模拟「就绪那一刻接收端还在」：多拿一份接收端，代替那个正要离开的请求
    let late = match &h.svc.slots.get(SlotKind::Ort).inner.lock().phase {
        Phase::Starting { done, .. } => done.clone(),
        _ => panic!("not starting"),
    };
    h.svc.release().await.expect("release");
    cancel.cancel();
    assert!(request.await.expect("join").is_err());
    h.launcher.gate.set(true);
    eventually("ready", || phase(&h.svc, SlotKind::Ort) == "ready").await;
    drop(late);
    tokio::time::sleep(Duration::from_secs(2)).await;
    eventually("stopped soon", || {
        phase(&h.svc, SlotKind::Ort) == "stopped" && h.launcher.all_exited()
    })
    .await;
}
