//! 主进程一侧的工作进程客户端：一个专用线程读写管道（不占 tokio worker），请求经通道排队、一问一答。
//! 单次 PP-OCR 很短（< 2 s），调用方取消时只是丢弃结果；版面检测同理。
//!
//! - 读写线程负责 RGBA → RGB 的转换与帧的收发；管道断开（工作进程退出、崩溃）后它结束，排队中的请求一律报 `crashed`。
//! - 另一个线程把 stderr 读空丢弃：工作进程本身不写 stderr，但万一有东西写进去，没人读的管道写满后会把它卡住。
//! - 每个请求有时限（[`REQUEST_TIMEOUT`]）；超时即结束工作进程（读写线程随之解除阻塞），下一次使用由调用方重启。
//! - 错误一律归为固定的种类（[`RuntimeFailure`]），不带路径、不带子进程的输出。

use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use image::RgbaImage;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use super::frame::{self, Request, Response, WorkerError};
use super::{ENV_DISABLE_TELEMETRY, ENV_DYLIB, ENV_MODELS, ENV_THREADS, WORKER_ARG, WORKER_ORT};
use crate::error::{LocalError, RuntimeFailure};
use crate::launch::{LaunchSpec, Launcher, ManagedChild};
use crate::ocr::layout::{LayoutBox, LayoutDetector};
use crate::ocr::{PpOcrTier, TextLine};
use crate::store::Store;

/// 启动后等第一次 `ping` 回应的时限（进程启动；Windows 上杀毒软件第一次扫描可能要几秒）
pub const START_TIMEOUT: Duration = Duration::from_secs(15);
/// 单个请求的时限（含第一次推理时加载动态库与建会话；慢机器上的大图）
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// pid 文件用的槽位名
pub const PID_SLOT: &str = "ort";

/// 启动工作进程的参数
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerConfig {
    /// Wisp 自己的可执行文件（[`Launcher::current_exe`]）
    pub exe: PathBuf,
    /// ONNX Runtime 动态库的绝对路径
    pub dylib: PathBuf,
    /// 模型目录
    pub models: PathBuf,
    /// 推理线程数
    pub threads: u32,
}

impl WorkerConfig {
    /// 对应的启动描述。子进程的环境只有这几项（加上平台必需的几项）
    pub fn launch_spec(&self) -> LaunchSpec {
        let threads = self.threads.clamp(1, 16).to_string();
        let env = vec![
            // 必须在 ONNX Runtime 建环境之前就在环境里：API 级的开关在初始化事件发出之后才生效
            (ENV_DISABLE_TELEMETRY.into(), "1".into()),
            (ENV_DYLIB.into(), self.dylib.clone().into_os_string()),
            (ENV_THREADS.into(), threads.clone().into()),
            (ENV_MODELS.into(), self.models.clone().into_os_string()),
            // oar-ocr 的裁剪在行数较多时用 rayon 的全局线程池：限制在同样的线程数，不按逻辑核数起一大堆线程
            ("RAYON_NUM_THREADS".into(), threads.into()),
            // macOS 的 libmalloc 在这个变量下把大块空闲内存及时还给系统（Wisp 主程序的 Info.plist 也设了它；
            // 子进程的环境是重建的，不会继承）。其他系统忽略它
            ("MallocSpaceEfficient".into(), "1".into()),
        ];
        LaunchSpec {
            program: self.exe.clone(),
            args: vec![WORKER_ARG.into(), WORKER_ORT.into()],
            env,
            current_dir: None,
            guard: false,
            low_priority: true,
        }
    }
}

/// 读写线程收到的一个请求
struct Job {
    header: Vec<u8>,
    image: Option<Arc<RgbaImage>>,
    reply: Box<dyn FnOnce(Result<Response, LocalError>) + Send>,
}

/// 运行中的工作进程
pub struct OrtWorker {
    /// 共享出去是为了在阻塞线程里结束进程（结束时要短暂地等它退出，不能占异步线程）
    child: Arc<Mutex<Box<dyn ManagedChild>>>,
    jobs: Mutex<Option<mpsc::Sender<Job>>>,
    /// 是否已经成功完成过一次推理（之后的超时不再归为「加载超时」）
    warmed: AtomicBool,
}

impl std::fmt::Debug for OrtWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrtWorker")
            .field("pid", &self.pid())
            .finish_non_exhaustive()
    }
}

impl OrtWorker {
    /// 启动工作进程并等它响应一次 `ping`（阻塞很短：进程启动 + 一次往返；动态库在第一次推理时才加载）
    pub fn spawn(launcher: &dyn Launcher, cfg: &WorkerConfig) -> Result<Self, LocalError> {
        let mut child = launcher
            .launch(cfg.launch_spec())
            .map_err(|_| LocalError::Runtime(RuntimeFailure::StartFailed))?;
        let pid = child.pid();
        if let Err(e) = launcher.write_pid_file(&Store::new(cfg.models.clone()).pid_file(PID_SLOT), pid) {
            // 只影响崩溃后的清扫
            log::warn!("local: ORT worker pid file not written: {}", e.kind());
        }
        let (Some(stdin), Some(stdout)) = (child.take_stdin(), child.take_stdout()) else {
            let _ = child.kill();
            return Err(LocalError::Runtime(RuntimeFailure::StartFailed));
        };
        if let Some(stderr) = child.take_stderr() {
            let spawned = std::thread::Builder::new()
                .name("wisp-ort-stderr".into())
                .spawn(move || drain(stderr));
            if spawned.is_err() {
                let _ = child.kill();
                return Err(LocalError::Runtime(RuntimeFailure::StartFailed));
            }
        }
        let (tx, rx) = mpsc::channel::<Job>();
        let spawned = std::thread::Builder::new()
            .name("wisp-ort-io".into())
            .spawn(move || io_loop(&rx, stdin, stdout));
        if spawned.is_err() {
            let _ = child.kill();
            return Err(LocalError::Runtime(RuntimeFailure::StartFailed));
        }
        let worker = Self {
            child: Arc::new(Mutex::new(child)),
            jobs: Mutex::new(Some(tx)),
            warmed: AtomicBool::new(false),
        };

        // 第一次往返：确认进程起来了、管道通、协议对
        let (done_tx, done_rx) = mpsc::channel();
        let header = serde_json::to_vec(&Request::Ping).map_err(|_| LocalError::Protocol("cannot encode a request"))?;
        worker.submit(Job {
            header,
            image: None,
            reply: Box::new(move |r| {
                let _ = done_tx.send(r);
            }),
        })?;
        let ok = matches!(done_rx.recv_timeout(START_TIMEOUT), Ok(Ok(Response { ok: true, .. })));
        if !ok {
            let _ = worker.child.lock().kill();
            return Err(LocalError::Runtime(RuntimeFailure::StartFailed));
        }
        log::info!("local: ORT worker started pid={pid}");
        Ok(worker)
    }

    /// 进程 pid
    pub fn pid(&self) -> u32 {
        self.child.lock().pid()
    }

    /// 进程是否还活着（不阻塞）
    pub fn is_alive(&self) -> bool {
        matches!(self.child.lock().try_wait(), Ok(None))
    }

    /// PP-OCRv6 检测 + 识别（RGBA 转 RGB8 后发送）
    pub async fn ppocr(
        &self,
        tier: PpOcrTier,
        image: Arc<RgbaImage>,
        max_side: u32,
    ) -> Result<Vec<TextLine>, LocalError> {
        let (width, height) = image.dimensions();
        let req = Request::Ppocr {
            tier,
            width,
            height,
            max_side,
        };
        let resp = self.request(&req, Some(image)).await?;
        resp.lines
            .ok_or(LocalError::Protocol("the ORT worker sent no text lines"))
    }

    /// 版面检测
    pub async fn layout(&self, image: Arc<RgbaImage>) -> Result<Vec<LayoutBox>, LocalError> {
        let (width, height) = image.dimensions();
        let resp = self.request(&Request::Layout { width, height }, Some(image)).await?;
        resp.boxes
            .ok_or(LocalError::Protocol("the ORT worker sent no layout boxes"))
    }

    /// 停止：关 stdin（工作进程读到 EOF 即退出）、最多等 `grace`，再强制结束（阻塞）
    pub fn shutdown(self, grace: Duration) {
        // 关掉请求通道：读写线程处理完手上的请求就结束，并在结束时放掉 stdin
        drop(self.jobs.lock().take());
        let mut child = self.child.lock();
        let stdin = child.take_stdin();
        crate::launch::stop(child.as_mut(), stdin, grace);
    }

    async fn request(&self, req: &Request, image: Option<Arc<RgbaImage>>) -> Result<Response, LocalError> {
        let header = serde_json::to_vec(req).map_err(|_| LocalError::Protocol("cannot encode a request"))?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.submit(Job {
            header,
            image,
            reply: Box::new(move |r| {
                let _ = tx.send(r);
            }),
        })?;
        let resp = match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(r)) => r?,
            // 读写线程已结束（进程退出）
            Ok(Err(_)) => return Err(RuntimeFailure::Crashed.into()),
            Err(_) => {
                // 卡住的进程留着没用，而且会挡住后面的请求。结束进程含一次短暂的等待（回收退出状态），放进阻塞线程
                let child = Arc::clone(&self.child);
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = child.lock().kill();
                })
                .await;
                let failure = if self.warmed.load(Ordering::Relaxed) {
                    RuntimeFailure::Crashed
                } else {
                    RuntimeFailure::LoadTimeout
                };
                return Err(failure.into());
            }
        };
        if resp.ok {
            self.warmed.store(true, Ordering::Relaxed);
            return Ok(resp);
        }
        Err(worker_error(resp.error.unwrap_or(WorkerError::Inference)))
    }

    fn submit(&self, job: Job) -> Result<(), LocalError> {
        let jobs = self.jobs.lock();
        let tx = jobs.as_ref().ok_or(LocalError::Runtime(RuntimeFailure::Crashed))?;
        tx.send(job).map_err(|_| LocalError::Runtime(RuntimeFailure::Crashed))
    }
}

impl LayoutDetector for OrtWorker {
    async fn detect(&self, image: Arc<RgbaImage>, cancel: CancellationToken) -> Result<Vec<LayoutBox>, LocalError> {
        tokio::select! {
            () = cancel.cancelled() => Err(LocalError::Cancelled),
            r = self.layout(image) => r,
        }
    }
}

/// 工作进程报告的错误种类 → 给用户看的原因
fn worker_error(e: WorkerError) -> LocalError {
    match e {
        WorkerError::BadRequest => LocalError::Protocol("the ORT worker rejected the request"),
        WorkerError::Unsupported => LocalError::Protocol("this build cannot run local text recognition"),
        WorkerError::ModelLoad => RuntimeFailure::ModelCorrupt.into(),
        WorkerError::OutOfMemory => RuntimeFailure::OutOfMemory.into(),
        WorkerError::Inference => RuntimeFailure::Crashed.into(),
        // 加载失败的具体原因由工作进程按加载器的报错判断（见 `serve::classify_runtime_load`）
        WorkerError::VcRuntimeMissing => RuntimeFailure::VcRuntimeMissing.into(),
        WorkerError::SystemLibraryMissing => RuntimeFailure::SystemLibraryMissing.into(),
        // 其余（库校验拒绝、文件损坏、被安全软件拦截）
        WorkerError::RuntimeLoad => RuntimeFailure::StartFailed.into(),
    }
}

/// 读写线程：一次一个请求。管道出错即结束，之后的请求得到 `crashed`
fn io_loop(rx: &mpsc::Receiver<Job>, stdin: Box<dyn Write + Send>, stdout: Box<dyn Read + Send>) {
    let mut writer = BufWriter::new(stdin);
    let mut reader = BufReader::new(stdout);
    while let Ok(job) = rx.recv() {
        let payload = job.image.as_deref().map(rgba_to_rgb).unwrap_or_default();
        let sent = frame::write_frame(&mut writer, &job.header, &payload).and_then(|()| writer.flush());
        drop(payload);
        let result = match sent.and_then(|()| frame::read_frame(&mut reader)) {
            Ok(Some(f)) => serde_json::from_slice::<Response>(&f.header)
                .map_err(|_| LocalError::Protocol("the ORT worker sent a malformed response")),
            // 帧边界上的 EOF 或管道错误：进程已经退出
            Ok(None) | Err(_) => Err(RuntimeFailure::Crashed.into()),
        };
        let broken = matches!(result, Err(LocalError::Runtime(_)) | Err(LocalError::Protocol(_)));
        (job.reply)(result);
        if broken {
            // 管道里的内容已经不同步，不能再用
            break;
        }
    }
    // writer 在这里放掉：工作进程读到 EOF 后退出
}

/// 读空并丢弃（工作进程的 stderr 不进日志）
fn drain(mut r: Box<dyn Read + Send>) {
    let mut buf = [0u8; 4096];
    while matches!(r.read(&mut buf), Ok(n) if n > 0) {}
}

/// RGBA → 紧凑的 RGB8（丢掉 alpha；截图不透明）
fn rgba_to_rgb(image: &RgbaImage) -> Vec<u8> {
    let raw = image.as_raw();
    let mut out = Vec::with_capacity(raw.len() / 4 * 3);
    for px in raw.chunks_exact(4) {
        out.extend_from_slice(&px[..3]);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::process::ExitStatus;

    use super::*;

    /// 假的子进程：stdin / stdout 接到同一线程里的一个「工作进程」上（真正的 `serve` 逻辑由集成测试覆盖）
    struct FakeChild {
        stdin: Option<Box<dyn Write + Send>>,
        stdout: Option<Box<dyn Read + Send>>,
        killed: Arc<AtomicBool>,
    }

    impl ManagedChild for FakeChild {
        fn pid(&self) -> u32 {
            42
        }
        fn take_stdin(&mut self) -> Option<Box<dyn Write + Send>> {
            self.stdin.take()
        }
        fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
            self.stdout.take()
        }
        fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
            None
        }
        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            Ok(None)
        }
        fn wait_timeout(&mut self, _: Duration) -> io::Result<Option<ExitStatus>> {
            Ok(None)
        }
        fn kill(&mut self) -> io::Result<()> {
            self.killed.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    /// 管道的一端（字节流）
    struct Pipe {
        rx: mpsc::Receiver<Vec<u8>>,
        buf: Vec<u8>,
    }

    impl Read for Pipe {
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

    struct PipeWriter(mpsc::Sender<Vec<u8>>);

    impl Write for PipeWriter {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0
                .send(b.to_vec())
                .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn pipe() -> (PipeWriter, Pipe) {
        let (tx, rx) = mpsc::channel();
        (PipeWriter(tx), Pipe { rx, buf: Vec::new() })
    }

    /// 一个假工作进程：对每个请求调用 `answer`；`answer` 返回 None 时模拟进程退出
    struct FakeLauncher {
        answer: Arc<dyn Fn(Request, Vec<u8>) -> Option<Response> + Send + Sync>,
        killed: Arc<AtomicBool>,
        spec: Mutex<Option<LaunchSpec>>,
    }

    impl Launcher for FakeLauncher {
        fn launch(&self, spec: LaunchSpec) -> io::Result<Box<dyn ManagedChild>> {
            *self.spec.lock() = Some(spec);
            let (to_worker, mut worker_in) = pipe();
            let (mut worker_out, from_worker) = pipe();
            let answer = Arc::clone(&self.answer);
            std::thread::spawn(move || {
                while let Ok(Some(f)) = frame::read_frame(&mut worker_in) {
                    let Ok(req) = serde_json::from_slice::<Request>(&f.header) else {
                        return;
                    };
                    let Some(resp) = answer(req, f.payload) else {
                        return;
                    };
                    let header = serde_json::to_vec(&resp).expect("json");
                    if frame::write_frame(&mut worker_out, &header, &[]).is_err() {
                        return;
                    }
                }
            });
            Ok(Box::new(FakeChild {
                stdin: Some(Box::new(to_worker)),
                stdout: Some(Box::new(from_worker)),
                killed: Arc::clone(&self.killed),
            }))
        }
        fn current_exe(&self) -> io::Result<PathBuf> {
            Ok(PathBuf::from("/opt/wisp/wisp"))
        }
        fn write_pid_file(&self, _: &std::path::Path, _: u32) -> io::Result<()> {
            Ok(())
        }
    }

    fn launcher(answer: impl Fn(Request, Vec<u8>) -> Option<Response> + Send + Sync + 'static) -> FakeLauncher {
        FakeLauncher {
            answer: Arc::new(answer),
            killed: Arc::new(AtomicBool::new(false)),
            spec: Mutex::new(None),
        }
    }

    fn cfg() -> WorkerConfig {
        WorkerConfig {
            exe: PathBuf::from("/opt/wisp/wisp"),
            dylib: PathBuf::from("/data/models/runtimes/onnxruntime/1.28.3/libonnxruntime.dylib"),
            models: PathBuf::from("/data/models"),
            threads: 3,
        }
    }

    #[test]
    fn launch_spec_carries_only_the_worker_environment() {
        let spec = cfg().launch_spec();
        assert_eq!(spec.args, ["--wisp-worker", "ort"]);
        assert!(!spec.guard && spec.low_priority);
        let env: Vec<(String, String)> = spec
            .env
            .iter()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()))
            .collect();
        assert!(env.contains(&("ORT_DISABLE_TELEMETRY".into(), "1".into())));
        assert!(env.contains(&("WISP_ORT_THREADS".into(), "3".into())));
        assert!(env.contains(&("WISP_ORT_MODELS".into(), "/data/models".into())));
    }

    #[tokio::test]
    async fn requests_round_trip_and_pixels_are_rgb() {
        let l = launcher(|req, payload| {
            Some(match req {
                Request::Ping => Response::ok(),
                Request::Ppocr { width, height, .. } => {
                    assert_eq!(payload.len(), (width * height * 3) as usize, "RGBA 转成了 RGB");
                    assert_eq!(&payload[..3], [10, 20, 30]);
                    Response {
                        ok: true,
                        lines: Some(vec![TextLine {
                            text: "hi".into(),
                            score: 0.9,
                            poly: [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
                        }]),
                        ..Response::default()
                    }
                }
                Request::Layout { .. } => Response::err(WorkerError::ModelLoad),
                Request::Exit => return None,
            })
        });
        let worker = OrtWorker::spawn(&l, &cfg()).expect("spawn");
        assert_eq!(worker.pid(), 42);
        let img = Arc::new(RgbaImage::from_pixel(3, 2, image::Rgba([10, 20, 30, 255])));
        let lines = worker
            .ppocr(PpOcrTier::Tiny, Arc::clone(&img), 2048)
            .await
            .expect("ppocr");
        assert_eq!(lines[0].text, "hi");
        let err = worker.layout(img).await.expect_err("layout");
        assert!(matches!(err, LocalError::Runtime(RuntimeFailure::ModelCorrupt)));
        worker.shutdown(Duration::from_millis(10));
    }

    #[tokio::test]
    async fn a_dead_worker_reports_crashed() {
        let l = launcher(|req, _| match req {
            Request::Ping => Some(Response::ok()),
            _ => None,
        });
        let worker = OrtWorker::spawn(&l, &cfg()).expect("spawn");
        let img = Arc::new(RgbaImage::new(2, 2));
        let err = worker
            .ppocr(PpOcrTier::Small, Arc::clone(&img), 1024)
            .await
            .expect_err("dead");
        assert!(matches!(err, LocalError::Runtime(RuntimeFailure::Crashed)));
        // 读写线程已结束：之后的请求立即失败
        let err = worker.layout(img).await.expect_err("dead");
        assert!(matches!(err, LocalError::Runtime(RuntimeFailure::Crashed)));
    }

    #[test]
    fn a_worker_that_never_answers_ping_fails_to_start() {
        let l = launcher(|_, _| None);
        let err = OrtWorker::spawn(&l, &cfg()).expect_err("no ping");
        assert!(matches!(err, LocalError::Runtime(RuntimeFailure::StartFailed)));
        assert!(l.killed.load(Ordering::SeqCst));
    }

    #[test]
    fn error_kinds_map_to_hints() {
        assert!(matches!(
            worker_error(WorkerError::OutOfMemory),
            LocalError::Runtime(RuntimeFailure::OutOfMemory)
        ));
        assert!(matches!(worker_error(WorkerError::BadRequest), LocalError::Protocol(_)));
        assert!(matches!(
            worker_error(WorkerError::RuntimeLoad),
            LocalError::Runtime(RuntimeFailure::StartFailed)
        ));
        assert!(matches!(
            worker_error(WorkerError::VcRuntimeMissing),
            LocalError::Runtime(RuntimeFailure::VcRuntimeMissing)
        ));
        assert!(matches!(
            worker_error(WorkerError::SystemLibraryMissing),
            LocalError::Runtime(RuntimeFailure::SystemLibraryMissing)
        ));
    }
}
