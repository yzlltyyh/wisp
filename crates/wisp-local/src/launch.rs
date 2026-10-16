//! 启动受托管子进程的接口。
//!
//! 推理进程必须随 Wisp 结束（切换、取消、失败、退出、崩溃都不留进程），这需要平台机制（Windows 的 Job Object、
//! Unix 的守护进程、pid 文件）。这些都在 `wisp-platform::child`；本 crate 只描述「要启动什么」（[`LaunchSpec`]），由 desktop
//! 实现 [`Launcher`] 并注入——本 crate 因此不依赖平台层、不写 `cfg(target_os)`，测试里可以换成假的实现。

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;

/// 要启动的进程（平台无关的描述）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// 子进程的环境变量**只有**这些（加上平台运行必需的几项，例如 Windows 的 `SystemRoot`）：不把 Wisp 的环境整体传下去
    pub env: Vec<(OsString, OsString)>,
    /// 工作目录（llama-server 设为它自己所在的目录，防止从当前目录加载被植入的后端模块）
    pub current_dir: Option<PathBuf>,
    /// 外部可执行文件经守护进程启动（macOS / Linux；Wisp 自己的工作进程不需要）
    pub guard: bool,
    /// 降低优先级
    pub low_priority: bool,
}

/// 受托管的子进程。stdin / stdout / stderr 一律是管道
pub trait ManagedChild: Send {
    /// 真正干活的进程的 pid（写 pid 文件、日志用）
    fn pid(&self) -> u32;
    /// 取走 stdin。丢弃它即通知子进程（守护进程 / 工作进程）退出
    fn take_stdin(&mut self) -> Option<Box<dyn Write + Send>>;
    /// 取走 stdout
    fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>>;
    /// 取走 stderr
    fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>>;
    /// 是否已经退出（不阻塞）
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>>;
    /// 最多等 `timeout` 让它自己退出（阻塞）
    fn wait_timeout(&mut self, timeout: Duration) -> io::Result<Option<ExitStatus>>;
    /// 结束整棵进程树（幂等；阻塞很短）
    fn kill(&mut self) -> io::Result<()>;
}

/// 启动器（desktop 用 `wisp_platform::child::spawn_bound` 实现）
pub trait Launcher: Send + Sync {
    /// 启动进程
    fn launch(&self, spec: LaunchSpec) -> io::Result<Box<dyn ManagedChild>>;

    /// Wisp 自己的可执行文件（ORT 工作进程以 `<它> --wisp-worker ort` 启动）
    fn current_exe(&self) -> io::Result<PathBuf>;

    /// 为推理进程写 pid 文件（崩溃后的清扫用）；失败只影响清扫，调用方记日志即可
    fn write_pid_file(&self, path: &Path, pid: u32) -> io::Result<()>;
}

/// 停止一个子进程：关 stdin、最多等 `grace`，还没退出就强制结束（阻塞，最多约 `grace` + 200 ms）
pub fn stop(child: &mut dyn ManagedChild, stdin: Option<Box<dyn Write + Send>>, grace: Duration) {
    drop(stdin);
    match child.wait_timeout(grace) {
        Ok(Some(_)) => {}
        Ok(None) | Err(_) => {
            if let Err(e) = child.kill() {
                log::warn!("local: kill pid={} failed: {e}", child.pid());
            }
        }
    }
}
