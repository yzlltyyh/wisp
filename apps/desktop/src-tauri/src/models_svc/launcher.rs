//! `wisp_local::launch::Launcher` 的实现：把平台无关的启动描述交给 `wisp_platform::child::spawn_bound`
//! （Windows 的 Job Object、Unix 的守护进程、降低优先级），子进程的环境只保留必要的几项。

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

use wisp_local::launch::{LaunchSpec, Launcher, ManagedChild};
use wisp_platform::child::{BoundChild, BoundOptions};

/// 用平台层启动推理进程
#[derive(Debug, Clone, Copy, Default)]
pub struct PlatformLauncher;

impl Launcher for PlatformLauncher {
    fn launch(&self, spec: LaunchSpec) -> io::Result<Box<dyn ManagedChild>> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .env_clear()
            .envs(wisp_platform::child::essential_env())
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = &spec.current_dir {
            cmd.current_dir(dir);
        }
        let child = wisp_platform::child::spawn_bound(
            cmd,
            BoundOptions {
                guard: spec.guard,
                low_priority: spec.low_priority,
            },
        )?;
        Ok(Box::new(Bound(child)))
    }

    fn current_exe(&self) -> io::Result<PathBuf> {
        std::env::current_exe()
    }

    fn write_pid_file(&self, path: &Path, pid: u32) -> io::Result<()> {
        wisp_platform::child::write_pid_file(path, pid)
    }
}

/// 平台层的受托管子进程
struct Bound(BoundChild);

impl ManagedChild for Bound {
    fn pid(&self) -> u32 {
        self.0.target_id()
    }

    fn take_stdin(&mut self) -> Option<Box<dyn Write + Send>> {
        self.0.take_stdin().map(|s| Box::new(s) as Box<dyn Write + Send>)
    }

    fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
        self.0.take_stdout().map(|s| Box::new(s) as Box<dyn Read + Send>)
    }

    fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
        self.0.take_stderr().map(|s| Box::new(s) as Box<dyn Read + Send>)
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.0.try_wait()
    }

    fn wait_timeout(&mut self, timeout: Duration) -> io::Result<Option<ExitStatus>> {
        self.0.wait_timeout(timeout)
    }

    fn kill(&mut self) -> io::Result<()> {
        self.0.kill()
    }
}
