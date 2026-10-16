//! 受托管的子进程：离线模型的推理进程（llama-server、Wisp 自己的 ORT 工作进程）必须随 Wisp 一起结束——
//! 切换、取消、失败、正常退出都由模型服务显式停止；崩溃或被强制结束时（发布版 `panic = "abort"`，不会有退出事件）
//! 由下面三层兜底：
//!
//! 1. Windows：子进程加入设置了 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 的 Job Object，Wisp 进程消失时内核关闭最后一个句柄、
//!    结束整棵进程树；另加 `CREATE_NO_WINDOW`（不弹控制台窗口）。
//! 2. macOS / Linux：外部可执行文件经「守护进程」启动——Wisp 自己的二进制以 `--wisp-guard -- <程序> <参数…>` 模式运行
//!    （[`guard_main`]），由它启动真正的程序，并阻塞读自己的 stdin；Wisp 退出或崩溃时管道的写端关闭，守护进程读到 EOF
//!    就结束子进程并退出。全由自有代码保证，不依赖 `PR_SET_PDEATHSIG`（Linux 上创建子进程的线程退出就会误杀）这类
//!    行为各异的机制。Wisp 自己的工作进程同样以 stdin EOF 退出，不需要守护进程。
//! 3. pid 文件：每个推理进程启动后写 `<run 目录>/<槽位>.pid`（[`write_pid_file`]）；下次启动时 [`sweep_orphans`] 只结束
//!    pid、启动时间、可执行文件路径三者都对得上的进程（pid 被系统复用给别的程序时不误杀），然后删除文件。
//!
//! 子进程的优先级略低于正常（Windows `BELOW_NORMAL_PRIORITY_CLASS`；Unix 由守护进程在启动目标之前 `setpriority`
//! 到 nice 5，目标继承），推理占满 CPU 时前台程序仍然流畅。
//!
//! 守护进程协议（Unix）：`<exe> --wisp-guard [--nice] [--env <名字>]… -- <程序> <参数…>`。守护进程自成进程组；目标的环境
//! 只含 `--env` 列出的变量（值经守护进程的环境传入，不出现在命令行里）；启动目标后先在 stdout 写一行 `wisp-guard pid=<n>`
//! （失败时 `wisp-guard error=<种类>` 并以 127 退出），之后原样转发目标的 stdout / stderr；stdin 读到 EOF → SIGTERM →
//! 最多 1 s → SIGKILL；目标先退出时以它的退出码退出。[`spawn_bound`] 读走报告行，调用方看到的输出与直接启动时相同。

use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus};
use std::time::{Duration, Instant};

#[cfg(unix)]
mod unix;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(unix)]
use unix as imp;
#[cfg(target_os = "windows")]
use windows as imp;

/// 守护模式的命令行开关（desktop 的 `run()` 在任何初始化之前识别它并调用 [`guard_main`]）
pub const GUARD_ARG: &str = "--wisp-guard";

/// [`wait_timeout`](BoundChild::wait_timeout) 两次查询之间的间隔：只在停止进程的那一秒里使用，不是常驻轮询
const WAIT_STEP: Duration = Duration::from_millis(10);

/// 启动方式
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct BoundOptions {
    /// macOS / Linux：经守护进程启动（外部可执行文件需要；Wisp 自己的工作进程自己会以 stdin EOF 退出，不需要）。
    /// Windows 忽略（Job Object 已由内核保证）
    pub guard: bool,
    /// 降低优先级（Windows `BELOW_NORMAL_PRIORITY_CLASS`；Unix nice 5）
    pub low_priority: bool,
}

/// 受托管的子进程。stdin / stdout / stderr 可以由调用方取走（取走 stdin 后丢弃它，守护进程与工作进程即自行退出）；
/// [`kill`](Self::kill) 结束整棵进程树；被丢弃时同样结束整棵进程树（不等待）
pub struct BoundChild {
    child: Child,
    /// 真正干活的进程：守护模式下是守护进程启动的目标（由守护进程报告），否则就是 `child`
    target: u32,
    /// 平台的绑定句柄（Windows 的 Job Object）；丢弃即关闭
    _binding: imp::Binding,
    killed: bool,
}

impl std::fmt::Debug for BoundChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundChild")
            .field("pid", &self.child.id())
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl BoundChild {
    /// 直接子进程的 pid（守护模式下是守护进程）
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// 真正干活的进程的 pid（写 pid 文件用；不是守护模式时等于 [`id`](Self::id)）
    pub fn target_id(&self) -> u32 {
        self.target
    }

    /// 取走 stdin（构造 `Command` 时要设为 `Stdio::piped()`）
    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    /// 取走 stdout
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    /// 取走 stderr
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    /// 是否已经退出（不阻塞）
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// 最多等 `timeout` 让进程自己退出（停止时先关 stdin、再等它，超时才强制结束）。阻塞调用线程
    pub fn wait_timeout(&mut self, timeout: Duration) -> io::Result<Option<ExitStatus>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(Some(status));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            std::thread::sleep(WAIT_STEP.min(deadline - now));
        }
    }

    /// 结束整棵进程树并回收直接子进程（幂等）
    pub fn kill(&mut self) -> io::Result<()> {
        if self.killed {
            return Ok(());
        }
        self.killed = true;
        imp::kill_tree(&mut self.child, &self._binding)?;
        // 回收僵尸进程；kill 之后通常立即返回
        let _ = self.wait_timeout(Duration::from_millis(200));
        Ok(())
    }
}

impl Drop for BoundChild {
    fn drop(&mut self) {
        if !self.killed && matches!(self.child.try_wait(), Ok(None)) {
            let _ = imp::kill_tree(&mut self.child, &self._binding);
        }
    }
}

/// 启动受托管的子进程。`cmd` 由调用方设好程序、参数、环境变量、工作目录与 stdio（需要读写的设为 `Stdio::piped()`）。
///
/// 守护模式（Unix）下改为启动 `<当前可执行文件> --wisp-guard … -- <程序> <参数…>`（见模块说明）：目标的环境**只有** `cmd`
/// 上显式设置的变量（调用方本来就应先 `env_clear` 再补必要的几项），工作目录照搬；守护进程的 stdin / stdout / stderr 总是
/// 管道（协议需要），`cmd` 自己的 stdio 设置被忽略
pub fn spawn_bound(cmd: Command, opts: BoundOptions) -> io::Result<BoundChild> {
    let (child, target, binding) = imp::spawn(cmd, opts)?;
    Ok(BoundChild {
        child,
        target,
        _binding: binding,
        killed: false,
    })
}

/// 守护模式的入口（`<exe> --wisp-guard -- <程序> <参数…>`）：启动目标、报告它的 pid、转发它的输出，读 stdin 直到 EOF 后
/// 结束目标并退出；目标先退出时以它的退出码退出。不初始化日志与任何 GUI。返回进程退出码
pub fn guard_main() -> i32 {
    imp::guard_main()
}

/// 为一个推理进程写 pid 文件（pid、启动时间、可执行文件路径）。失败只影响崩溃后的清扫，调用方记日志即可
pub fn write_pid_file(path: &Path, pid: u32) -> io::Result<()> {
    imp::write_pid_file(path, pid)
}

/// 清扫上次崩溃留下的推理进程：读 `pid_dir` 里的每个 `*.pid`，pid、启动时间、可执行文件路径都匹配时结束它，
/// 然后删除文件。阻塞（读几个小文件、几次系统调用），在阻塞线程里调用
pub fn sweep_orphans(pid_dir: &Path) {
    imp::sweep_orphans(pid_dir);
}

/// 推理进程最少需要的环境变量（取自 Wisp 自己的环境）。调用方清空子进程的环境（不把 Wisp 的环境整体传下去，例如
/// AppImage 设置的 `LD_LIBRARY_PATH` 会让外部程序加载错的库）后补上这几项，再加自己的变量
pub fn essential_env() -> Vec<(OsString, OsString)> {
    imp::ESSENTIAL_ENV
        .iter()
        .filter_map(|name| std::env::var_os(name).map(|v| (OsString::from(name), v)))
        .collect()
}

/// 进程的启动时间（Unix 秒；Windows 为 FILETIME 换算的 Unix 秒）；进程不存在或取不到时为 None
pub fn process_start_time(pid: u32) -> Option<u64> {
    imp::process_start_time(pid)
}

/// 原子地写一个小文件：先写同目录的临时文件再改名（崩溃时不会留下半截的 pid 文件）；目录不存在时先建
fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("pid file has no parent"))?;
    std::fs::create_dir_all(dir)?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// 清扫的公共部分：对目录里每个 `*.pid` 调用 `check`（返回 Some(true) = 结束了一个孤儿，Some(false) = 进程已不在或不是它，
/// None = 文件内容无法识别），然后删除文件。日志只写数量，不写路径与 pid 之外的内容
fn sweep_dir(pid_dir: &Path, mut check: impl FnMut(&[u8]) -> Option<bool>) {
    let Ok(entries) = std::fs::read_dir(pid_dir) else {
        return;
    };
    let (mut killed, mut stale) = (0usize, 0usize);
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("pid") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        match check(&bytes) {
            Some(true) => killed += 1,
            Some(false) | None => stale += 1,
        }
        let _ = std::fs::remove_file(&path);
    }
    if killed + stale > 0 {
        log::info!("child: swept pid files (orphans ended: {killed}, stale: {stale})");
    }
}
