//! 受托管子进程（`wisp_platform::child`）的进程级测试。
//!
//! 守护模式需要「当前可执行文件」能以 `--wisp-guard` 运行，所以这个测试不用 libtest（`harness = false`）：`main` 先按第一个
//! 参数分流——`--wisp-guard` 进入守护进程入口（与 Wisp 本体相同），`parent-mode` / `sleep-mode` 是测试用的辅助角色——其余情况
//! 依次运行下面的用例。
//!
//! - 默认用例只用系统自带的程序（`/bin/sh`、`/bin/sleep`、`/usr/bin/env`；Windows 上是测试二进制自己），CI 上直接运行；
//! - 带 `--ignored` 时另外运行真机用例：用 `WISP_LOCAL_TEST_DIR`（`artifacts/local-models` 的绝对路径）里解包好的官方
//!   `llama-server` 与 Index-Translate-2B，验证守护进程与孤儿清扫对真实推理进程有效。
//!
//! `cargo test -p wisp-platform --test child_guard [-- --ignored]`
// 测试的进度与真机数据打印给运行者看
#![allow(clippy::print_stdout)]

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use wisp_platform::child::{self, BoundOptions};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some(child::GUARD_ARG) => std::process::exit(child::guard_main()),
        Some("sleep-mode") => {
            std::thread::sleep(Duration::from_secs(60));
            return;
        }
        Some("parent-mode") => std::process::exit(parent_mode(&args[2..])),
        _ => {}
    }
    let ignored = args.iter().any(|a| a == "--ignored");
    let filter = args.iter().skip(1).find(|a| !a.starts_with('-')).cloned();
    let mut failed = 0usize;
    let mut ran = 0usize;
    for (name, live, test) in cases() {
        if live != ignored || filter.as_deref().is_some_and(|f| !name.contains(f)) {
            continue;
        }
        ran += 1;
        let started = Instant::now();
        let ok = std::panic::catch_unwind(test).is_ok();
        println!(
            "test {name} ... {} ({} ms)",
            if ok { "ok" } else { "FAILED" },
            started.elapsed().as_millis()
        );
        failed += usize::from(!ok);
    }
    println!("child_guard: {ran} run, {failed} failed");
    if failed > 0 {
        std::process::exit(1);
    }
}

type Case = (&'static str, bool, fn());

fn cases() -> Vec<Case> {
    let mut v: Vec<Case> = vec![("start_time_of_missing_pid", false, start_time_of_missing_pid)];
    #[cfg(unix)]
    v.extend(unix::cases());
    #[cfg(windows)]
    v.extend(win::cases());
    v
}

fn start_time_of_missing_pid() {
    // pid 不存在（远大于系统允许的最大值）时为 None
    assert_eq!(child::process_start_time(u32::MAX - 7), None);
    assert!(child::process_start_time(std::process::id()).is_some());
}

/// 辅助角色：`parent-mode <guard 0|1> <pid 文件|-> <程序> <参数…>`——托管启动目标，打印 `target=<pid>`，然后阻塞读 stdin
/// （测试会直接强制结束这个进程，模拟 Wisp 崩溃）
fn parent_mode(args: &[String]) -> i32 {
    let (Some(guard), Some(pid_file), Some(program)) = (args.first(), args.get(1), args.get(2)) else {
        return 2;
    };
    let mut cmd = Command::new(program);
    cmd.args(&args[3..])
        .env_clear()
        .envs(child::essential_env())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let bound = match child::spawn_bound(
        cmd,
        BoundOptions {
            guard: guard == "1",
            low_priority: true,
        },
    ) {
        Ok(b) => b,
        Err(_) => return 3,
    };
    let mut bound = bound;
    drain(&mut bound);
    if pid_file != "-" && child::write_pid_file(std::path::Path::new(pid_file), bound.target_id()).is_err() {
        return 4;
    }
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "target={}", bound.target_id());
    let _ = out.flush();
    drop(out);
    let mut sink = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut sink);
    drop(bound);
    0
}

/// 读空子进程的 stdout / stderr（不读的话管道写满后目标会阻塞在写日志上）
fn drain(c: &mut child::BoundChild) {
    if let Some(mut out) = c.take_stdout() {
        std::thread::spawn(move || std::io::copy(&mut out, &mut std::io::sink()));
    }
    if let Some(mut err) = c.take_stderr() {
        std::thread::spawn(move || std::io::copy(&mut err, &mut std::io::sink()));
    }
}

/// 启动 `parent-mode` 并读出它报告的目标 pid
fn spawn_parent(
    guard: bool,
    pid_file: Option<&std::path::Path>,
    program: &str,
    args: &[&str],
) -> (std::process::Child, u32) {
    let exe = std::env::current_exe().expect("exe");
    let mut parent = Command::new(exe)
        .arg("parent-mode")
        .arg(if guard { "1" } else { "0" })
        .arg(pid_file.map_or_else(|| "-".into(), |p| p.as_os_str().to_owned()))
        .arg(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("parent");
    let mut line = String::new();
    BufReader::new(parent.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("report");
    let target = line
        .trim()
        .strip_prefix("target=")
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("unexpected report {line:?}"));
    (parent, target)
}

/// 最多等 `timeout` 让条件成立
fn eventually(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
mod unix {
    use std::ffi::c_int;
    use std::io::Read;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use wisp_platform::child::{self, BoundOptions};

    use super::{Case, drain, eventually, spawn_parent};

    unsafe extern "C" {
        fn kill(pid: c_int, sig: c_int) -> c_int;
    }
    const SIGKILL: c_int = 9;

    pub(super) fn cases() -> Vec<Case> {
        vec![
            (
                "guard_reports_target_and_forwards_output",
                false,
                guard_reports_target_and_forwards_output,
            ),
            ("guard_passes_only_explicit_env", false, guard_passes_only_explicit_env),
            ("guard_lowers_priority", false, guard_lowers_priority),
            ("guard_missing_program", false, guard_missing_program),
            ("stdin_eof_ends_target", false, stdin_eof_ends_target),
            ("kill_and_drop_end_the_tree", false, kill_and_drop_end_the_tree),
            ("parent_crash_ends_target", false, parent_crash_ends_target),
            (
                "sweep_ends_only_matching_orphans",
                false,
                sweep_ends_only_matching_orphans,
            ),
            ("live_llama_server_lifecycle", true, live_llama_server_lifecycle),
        ]
    }

    /// 进程是否还在（kill 0 只检查存在与权限）
    fn alive(pid: u32) -> bool {
        let Ok(pid) = c_int::try_from(pid) else {
            return false;
        };
        // SAFETY: 信号 0 不发送任何信号，只检查进程是否存在
        unsafe { kill(pid, 0) == 0 }
    }

    fn guarded(program: &str, args: &[&str], low_priority: bool) -> child::BoundChild {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        child::spawn_bound(
            cmd,
            BoundOptions {
                guard: true,
                low_priority,
            },
        )
        .expect("spawn")
    }

    fn read_all(r: Option<impl Read>) -> String {
        let mut s = String::new();
        r.expect("pipe").read_to_string(&mut s).expect("read");
        s
    }

    fn guard_reports_target_and_forwards_output() {
        let mut c = guarded("/bin/sh", &["-c", "echo out; echo err >&2; exit 3"], false);
        assert_ne!(c.target_id(), c.id(), "目标是守护进程的子进程");
        assert_eq!(read_all(c.take_stdout()), "out\n", "报告行已被读走，之后原样转发");
        assert_eq!(read_all(c.take_stderr()), "err\n");
        let status = c.wait_timeout(Duration::from_secs(5)).expect("wait").expect("exited");
        assert_eq!(status.code(), Some(3), "以目标的退出码退出");
    }

    fn guard_passes_only_explicit_env() {
        let mut cmd = Command::new("/usr/bin/env");
        cmd.env_clear()
            .env("WISP_TEST_SECRET", "s3cr3t")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut c = child::spawn_bound(
            cmd,
            BoundOptions {
                guard: true,
                low_priority: false,
            },
        )
        .expect("spawn");
        assert_eq!(read_all(c.take_stdout()), "WISP_TEST_SECRET=s3cr3t\n");
        let _ = c.wait_timeout(Duration::from_secs(5));
    }

    fn guard_lowers_priority() {
        let mut c = guarded("/bin/sleep", &["30"], true);
        let out = Command::new("/bin/ps")
            .args(["-o", "nice=", "-p", &c.target_id().to_string()])
            .output()
            .expect("ps");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "5");
        c.kill().expect("kill");
    }

    fn guard_missing_program() {
        let mut cmd = Command::new("/nonexistent/wisp-test-program");
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let err = child::spawn_bound(
            cmd,
            BoundOptions {
                guard: true,
                low_priority: false,
            },
        )
        .expect_err("missing");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    fn stdin_eof_ends_target() {
        let mut c = guarded("/bin/sleep", &["30"], false);
        let target = c.target_id();
        assert!(alive(target));
        let started = Instant::now();
        drop(c.take_stdin());
        let status = c.wait_timeout(Duration::from_secs(5)).expect("wait");
        assert!(status.is_some(), "守护进程读到 EOF 后退出");
        assert!(eventually(Duration::from_secs(2), || !alive(target)), "目标随之结束");
        assert!(started.elapsed() < Duration::from_secs(2), "SIGTERM 即可结束 sleep");
    }

    fn kill_and_drop_end_the_tree() {
        let mut a = guarded("/bin/sleep", &["30"], false);
        let ta = a.target_id();
        a.kill().expect("kill");
        assert!(eventually(Duration::from_secs(2), || !alive(ta)));
        let b = guarded("/bin/sleep", &["30"], false);
        let tb = b.target_id();
        drop(b);
        assert!(eventually(Duration::from_secs(2), || !alive(tb)));
    }

    fn parent_crash_ends_target() {
        let (mut parent, target) = spawn_parent(true, None, "/bin/sleep", &["30"]);
        assert!(alive(target));
        // 模拟 Wisp 崩溃：父进程被 SIGKILL，不执行任何清理
        parent.kill().expect("kill parent");
        let _ = parent.wait();
        assert!(
            eventually(Duration::from_secs(3), || !alive(target)),
            "守护进程的 stdin 关闭后目标结束"
        );
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wisp-child-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn sweep_ends_only_matching_orphans() {
        let dir = temp_dir("sweep");
        // 一个「孤儿」：父进程（parent-mode，非守护模式）写好 pid 文件后被强制结束，sleep 被系统收养
        let (mut parent, orphan) = spawn_parent(false, Some(&dir.join("translate.pid")), "/bin/sleep", &["30"]);
        parent.kill().expect("kill parent");
        let _ = parent.wait();
        assert!(alive(orphan));
        // 一个 pid 对得上、启动时间对不上的记录（pid 被复用的情形）：指向测试进程自己，不能被结束
        std::fs::write(
            dir.join("vl.pid"),
            format!(
                "pid={}\nstart=1\nexe={}\n",
                std::process::id(),
                std::env::current_exe().expect("exe").display()
            ),
        )
        .expect("write");
        std::fs::write(dir.join("broken.pid"), "garbage").expect("write");
        std::fs::write(dir.join("keep.txt"), "x").expect("write");
        child::sweep_orphans(&dir);
        assert!(eventually(Duration::from_secs(2), || !alive(orphan)), "孤儿被结束");
        assert!(alive(std::process::id()));
        let left: Vec<_> = std::fs::read_dir(&dir)
            .expect("dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, vec!["keep.txt".to_owned()], "pid 文件全部删除，其他文件不动");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真机：官方 llama-server + Index-Translate-2B。三种结束方式都不留进程：关守护进程的 stdin、强制结束守护进程的父进程、
    /// 孤儿清扫
    fn live_llama_server_lifecycle() {
        let Some(root) = std::env::var_os("WISP_LOCAL_TEST_DIR").map(PathBuf::from) else {
            println!("  skipped: WISP_LOCAL_TEST_DIR is not set");
            return;
        };
        let server = root.join("rt/llama-b11669/llama-b11669/llama-server");
        let model = root.join("raw/Index-Translate-2B-GGUF/Index-Translate-2B.Q4_K_M.gguf");
        assert!(server.is_file() && model.is_file(), "test files missing");
        let server = server.to_string_lossy().into_owned();
        let model = model.to_string_lossy().into_owned();
        let args = |port: u16| {
            vec![
                "-m".to_owned(),
                model.clone(),
                "--host".to_owned(),
                "127.0.0.1".to_owned(),
                "--port".to_owned(),
                port.to_string(),
                "-c".to_owned(),
                "2048".to_owned(),
                "-np".to_owned(),
                "1".to_owned(),
                "--no-webui".to_owned(),
                "--offline".to_owned(),
            ]
        };
        let free_port = || {
            std::net::TcpListener::bind("127.0.0.1:0")
                .and_then(|l| l.local_addr())
                .map(|a| a.port())
                .expect("port")
        };

        // 1. 关守护进程的 stdin
        let port = free_port();
        let a = args(port);
        let mut c = guarded(&server, &a.iter().map(String::as_str).collect::<Vec<_>>(), true);
        drain(&mut c);
        let target = c.target_id();
        let started = Instant::now();
        assert!(wait_health(port, Duration::from_secs(60)), "llama-server ready");
        println!(
            "  guarded llama-server ready in {} ms (pid {target})",
            started.elapsed().as_millis()
        );
        let rss = rss_kb(target);
        println!("  rss {} MB", rss.unwrap_or(0) / 1024);
        let t = Instant::now();
        drop(c.take_stdin());
        assert!(c.wait_timeout(Duration::from_secs(5)).expect("wait").is_some());
        assert!(eventually(Duration::from_secs(3), || !alive(target)));
        println!("  stdin closed -> llama-server gone in {} ms", t.elapsed().as_millis());

        // 2. 守护进程的父进程被 SIGKILL
        let port = free_port();
        let a = args(port);
        let (mut parent, target) = spawn_parent(true, None, &server, &a.iter().map(String::as_str).collect::<Vec<_>>());
        assert!(wait_health(port, Duration::from_secs(60)));
        let t = Instant::now();
        parent.kill().expect("kill parent");
        let _ = parent.wait();
        assert!(
            eventually(Duration::from_secs(3), || !alive(target)),
            "no orphan after parent SIGKILL"
        );
        println!(
            "  parent SIGKILL -> llama-server gone in {} ms",
            t.elapsed().as_millis()
        );

        // 3. 非守护模式留下的孤儿由清扫结束
        let dir = temp_dir("live-sweep");
        let port = free_port();
        let a = args(port);
        let (mut parent, target) = spawn_parent(
            false,
            Some(&dir.join("translate.pid")),
            &server,
            &a.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        assert!(wait_health(port, Duration::from_secs(60)));
        parent.kill().expect("kill parent");
        let _ = parent.wait();
        assert!(alive(target), "without the guard the server survives its parent");
        let t = Instant::now();
        child::sweep_orphans(&dir);
        assert!(
            eventually(Duration::from_secs(3), || !alive(target)),
            "sweep ends the orphan"
        );
        println!("  sweep -> orphan gone in {} ms", t.elapsed().as_millis());
        let _ = std::fs::remove_dir_all(&dir);
        // 兜底：任何情况下都不留下进程
        if alive(target) {
            // SAFETY: target 是本测试启动的 llama-server
            unsafe { kill(c_int::try_from(target).expect("pid"), SIGKILL) };
        }
    }

    /// 等 `GET /health` 返回 200
    fn wait_health(port: u16, timeout: Duration) -> bool {
        use std::io::Write;
        eventually(timeout, || {
            let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", port)) else {
                return false;
            };
            let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
            if s.write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
                .is_err()
            {
                return false;
            }
            let mut buf = [0u8; 64];
            let n = s.read(&mut buf).unwrap_or(0);
            buf[..n].starts_with(b"HTTP/1.1 200")
        })
    }

    fn rss_kb(pid: u32) -> Option<u64> {
        let out = Command::new("/bin/ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }
}

#[cfg(windows)]
mod win {
    use std::process::{Command, Stdio};
    use std::time::Duration;

    use wisp_platform::child::{self, BoundOptions};

    use super::{Case, eventually, spawn_parent};

    pub(super) fn cases() -> Vec<Case> {
        vec![
            ("job_kill_and_drop", false, job_kill_and_drop),
            ("parent_crash_ends_job", false, parent_crash_ends_job),
        ]
    }

    fn alive(pid: u32) -> bool {
        child::process_start_time(pid).is_some()
    }

    fn sleeper() -> child::BoundChild {
        let mut cmd = Command::new(std::env::current_exe().expect("exe"));
        cmd.arg("sleep-mode")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        child::spawn_bound(
            cmd,
            BoundOptions {
                guard: true,
                low_priority: true,
            },
        )
        .expect("spawn")
    }

    fn job_kill_and_drop() {
        let mut a = sleeper();
        assert_eq!(a.target_id(), a.id(), "Windows 不用守护进程");
        a.kill().expect("kill");
        assert!(a.try_wait().expect("wait").is_some());
        let b = sleeper();
        let pid = b.id();
        drop(b);
        assert!(eventually(Duration::from_secs(2), || !alive(pid)));
    }

    fn parent_crash_ends_job() {
        let exe = std::env::current_exe().expect("exe");
        let exe = exe.to_string_lossy().into_owned();
        let (mut parent, target) = spawn_parent(true, None, &exe, &["sleep-mode"]);
        assert!(alive(target));
        parent.kill().expect("kill parent");
        let _ = parent.wait();
        assert!(
            eventually(Duration::from_secs(3), || !alive(target)),
            "Job 的最后一个句柄关闭后内核结束子进程"
        );
    }
}
