//! 集成测试共用：用 `std::process` 实现的启动器（只在测试里用；desktop 用 wisp-platform 的受托管子进程），
//! 以及 `examples/ort_worker` 的位置。

#![allow(dead_code)]

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use wisp_local::launch::{LaunchSpec, Launcher, ManagedChild};

/// `cargo test` 会一并编译 examples：工作进程可执行文件在 `target/<profile>/examples/` 下
pub fn worker_exe() -> PathBuf {
    let exe = std::env::current_exe().expect("current exe");
    let profile_dir = exe.parent().and_then(Path::parent).expect("target/<profile>");
    let name = format!("ort_worker{}", std::env::consts::EXE_SUFFIX);
    let path = profile_dir.join("examples").join(name);
    assert!(
        path.is_file(),
        "先编译工作进程：cargo build -p wisp-local --features ort --example ort_worker"
    );
    path
}

pub struct StdLauncher;

pub struct StdChild(Child);

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
        let t0 = Instant::now();
        loop {
            if let Some(s) = self.0.try_wait()? {
                return Ok(Some(s));
            }
            if t0.elapsed() >= timeout {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn kill(&mut self) -> io::Result<()> {
        match self.0.kill() {
            Ok(()) => {
                let _ = self.0.wait();
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => Ok(()),
            Err(e) => Err(e),
        }
    }
}

impl Drop for StdChild {
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

impl Launcher for StdLauncher {
    fn launch(&self, spec: LaunchSpec) -> io::Result<Box<dyn ManagedChild>> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .env_clear()
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // 平台运行必需的几项（与 desktop 的启动器一致的意图）
        for key in ["SystemRoot", "windir", "TEMP", "TMP", "TMPDIR", "HOME"] {
            if let Some(v) = std::env::var_os(key) {
                cmd.env(key, v);
            }
        }
        if let Some(dir) = &spec.current_dir {
            cmd.current_dir(dir);
        }
        Ok(Box::new(StdChild(cmd.spawn()?)))
    }
    fn current_exe(&self) -> io::Result<PathBuf> {
        Ok(worker_exe())
    }
    fn write_pid_file(&self, _path: &Path, _pid: u32) -> io::Result<()> {
        // 测试不写 pid 文件（进程随测试结束）
        Ok(())
    }
}

/// 进程的常驻内存（KB）；读不到（没有 `ps` 的系统）时为 None
pub fn rss_kb(pid: u32) -> Option<u64> {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}
