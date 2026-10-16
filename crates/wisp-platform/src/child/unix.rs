//! 受托管子进程的 macOS / Linux 实现：守护进程与 pid 文件。
//!
//! ## 守护进程协议
//!
//! `spawn` 在守护模式下启动 `<当前可执行文件> --wisp-guard [--nice] [--env <名字>]… -- <程序> <参数…>`：
//!
//! - 守护进程自成进程组（组号 = 它的 pid），目标继承这个组；[`kill_tree`] 向整组发 SIGKILL，不依赖目标的 pid 是否已被复用。
//! - 守护进程自己的环境沿用 Wisp 的环境（打包方式可能依赖它找到自己的动态库），目标的环境只含 `--env` 列出的变量
//!   （`Command` 上显式设置的那些；值经守护进程自己的环境传递，不出现在命令行里）。工作目录照搬。
//! - `--nice`：守护进程启动目标之前把自己调到 nice 5，目标继承。
//! - 启动目标后，守护进程在 stdout 写一行 `wisp-guard pid=<n>`，之后把目标的 stdout / stderr 原样转发；启动失败时写
//!   `wisp-guard error=<种类>` 并以 127 退出。`spawn` 读走这一行（逐字节读，不多读），调用方拿到的输出与直接启动时相同。
//! - 守护进程读自己的 stdin 直到 EOF（Wisp 关掉管道、退出或崩溃）→ 向目标发 SIGTERM → 最多等 1 s → SIGKILL；目标先退出时
//!   守护进程以它的退出码退出（被信号结束时为 128 + 信号）。

use std::ffi::{OsStr, OsString, c_int};
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use super::{BoundOptions, GUARD_ARG};

/// 子进程保留的环境变量：临时目录与家目录（库与运行时据此放缓存、临时文件），区域设置（数字格式）
pub(super) const ESSENTIAL_ENV: &[&str] = &["HOME", "TMPDIR", "LANG", "LC_ALL"];

/// 守护进程的选项：降低优先级
const NICE_ARG: &str = "--nice";
/// 守护进程的选项：目标保留的一个环境变量
const ENV_ARG: &str = "--env";
/// 守护进程报告目标 pid 的行首
const PID_LINE: &str = "wisp-guard pid=";
/// 守护进程报告启动失败的行首
const ERROR_LINE: &str = "wisp-guard error=";
/// 等守护进程报告的最长时间（它只需要 fork + exec 一次）
const REPORT_TIMEOUT: Duration = Duration::from_secs(10);
/// 守护进程的 stdin 关闭后，SIGTERM 与 SIGKILL 之间的等待
const TERM_GRACE: Duration = Duration::from_secs(1);
/// nice 值（略低于正常，推理占满 CPU 时前台程序仍流畅）
const NICE: c_int = 5;

const SIGKILL: c_int = 9;
const SIGTERM: c_int = 15;
const PRIO_PROCESS: c_int = 0;

unsafe extern "C" {
    fn kill(pid: c_int, sig: c_int) -> c_int;
    fn setpriority(which: c_int, who: u32, prio: c_int) -> c_int;
}

/// Unix 没有需要随子进程保存的句柄：守护模式下记下进程组，普通模式下什么也不记
pub(super) struct Binding {
    /// 守护进程的进程组号（= 守护进程的 pid）
    group: Option<c_int>,
}

pub(super) fn spawn(cmd: Command, opts: BoundOptions) -> io::Result<(Child, u32, Binding)> {
    if opts.guard {
        let exe = std::env::current_exe()?;
        return spawn_guarded(&exe, &cmd, opts);
    }
    let mut cmd = cmd;
    let child = cmd.spawn()?;
    let pid = child.id();
    Ok((child, pid, Binding { group: None }))
}

/// 守护模式（见模块说明）。`guard_exe` 是带 `--wisp-guard` 入口的可执行文件（正式运行时就是 Wisp 自己）
pub(super) fn spawn_guarded(guard_exe: &Path, cmd: &Command, opts: BoundOptions) -> io::Result<(Child, u32, Binding)> {
    let mut g = Command::new(guard_exe);
    g.arg(GUARD_ARG);
    if opts.low_priority {
        g.arg(NICE_ARG);
    }
    for (name, value) in cmd.get_envs() {
        if let Some(value) = value {
            g.arg(ENV_ARG).arg(name).env(name, value);
        }
    }
    g.arg("--").arg(cmd.get_program()).args(cmd.get_args());
    if let Some(dir) = cmd.get_current_dir() {
        g.current_dir(dir);
    }
    g.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = g.spawn()?;
    let group = c_int::try_from(child.id()).map_err(|_| io::Error::other("pid out of range"))?;
    match read_report(&mut child) {
        Ok(target) => Ok((child, target, Binding { group: Some(group) })),
        Err(e) => {
            // SAFETY: group 是刚启动、尚未回收的守护进程的进程组；向整组发 SIGKILL
            unsafe { kill(-group, SIGKILL) };
            let _ = child.wait();
            Err(e)
        }
    }
}

/// 读守护进程 stdout 的第一行（另起线程读，超时则放弃），读完把 stdout 还给 `child`
fn read_report(child: &mut Child) -> io::Result<u32> {
    let mut stdout = child.stdout.take().ok_or_else(|| io::Error::other("guard stdout"))?;
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("wisp-guard-report".into())
        .spawn(move || {
            let line = read_line(&mut stdout);
            let _ = tx.send((line, stdout));
        })?;
    let (line, stdout) = rx
        .recv_timeout(REPORT_TIMEOUT)
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "guard did not report"))?;
    child.stdout = Some(stdout);
    parse_report(&line?)
}

/// 逐字节读到换行（不多读），最多 128 字节
fn read_line(r: &mut impl Read) -> io::Result<String> {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    while out.len() < 128 {
        match r.read(&mut byte) {
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' => return String::from_utf8(out).map_err(|_| io::Error::other("guard report")),
            Ok(_) => out.push(byte[0]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "guard exited before reporting",
    ))
}

/// 解析守护进程的报告行
pub(super) fn parse_report(line: &str) -> io::Result<u32> {
    if let Some(pid) = line.strip_prefix(PID_LINE) {
        return pid
            .trim()
            .parse()
            .ok()
            .filter(|p| *p > 0)
            .ok_or_else(|| io::Error::other("guard report"));
    }
    if let Some(kind) = line.strip_prefix(ERROR_LINE) {
        let kind = match kind.trim() {
            "not_found" => io::ErrorKind::NotFound,
            "permission_denied" => io::ErrorKind::PermissionDenied,
            _ => io::ErrorKind::Other,
        };
        return Err(io::Error::new(kind, "guard could not start the program"));
    }
    Err(io::Error::other("unexpected guard report"))
}

/// 结束整棵进程树：守护模式向守护进程的进程组发 SIGKILL（守护进程与目标一起结束），否则直接结束子进程
pub(super) fn kill_tree(child: &mut Child, binding: &Binding) -> io::Result<()> {
    match binding.group {
        Some(group) if matches!(child.try_wait(), Ok(None)) => {
            // SAFETY: group 是尚未回收的守护进程的进程组号（守护进程未被回收时这个号不会被复用）
            if unsafe { kill(-group, SIGKILL) } == 0 {
                Ok(())
            } else {
                child.kill()
            }
        }
        _ => child.kill(),
    }
}

/// 守护进程的命令行
#[derive(Debug, PartialEq, Eq)]
pub(super) struct GuardArgs {
    pub(super) nice: bool,
    pub(super) env: Vec<OsString>,
    pub(super) program: OsString,
    pub(super) args: Vec<OsString>,
}

/// 解析 `--wisp-guard` 之后的参数
pub(super) fn parse_guard_args(mut it: impl Iterator<Item = OsString>) -> Option<GuardArgs> {
    let mut nice = false;
    let mut env = Vec::new();
    loop {
        let a = it.next()?;
        match a.to_str() {
            Some(NICE_ARG) => nice = true,
            Some(ENV_ARG) => env.push(it.next()?),
            Some("--") => break,
            _ => return None,
        }
    }
    let program = it.next()?;
    Some(GuardArgs {
        nice,
        env,
        program,
        args: it.collect(),
    })
}

pub(super) fn guard_main() -> i32 {
    let mut argv = std::env::args_os().skip(1);
    if argv.next().as_deref() != Some(OsStr::new(GUARD_ARG)) {
        return 2;
    }
    let Some(args) = parse_guard_args(argv) else {
        return 2;
    };
    if args.nice {
        // SAFETY: 只调整本进程（who = 0）的优先级；失败（例如已经更低）不影响运行
        unsafe { setpriority(PRIO_PROCESS, 0, NICE) };
    }
    let mut cmd = Command::new(&args.program);
    cmd.args(&args.args).env_clear();
    for name in &args.env {
        if let Some(v) = std::env::var_os(name) {
            cmd.env(name, v);
        }
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut out = io::stdout().lock();
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let kind = match e.kind() {
                io::ErrorKind::NotFound => "not_found",
                io::ErrorKind::PermissionDenied => "permission_denied",
                _ => "other",
            };
            let _ = writeln!(out, "{ERROR_LINE}{kind}");
            let _ = out.flush();
            return 127;
        }
    };
    let pid = child.id();
    if writeln!(out, "{PID_LINE}{pid}").and_then(|()| out.flush()).is_err() {
        let _ = child.kill();
        let _ = child.wait();
        return 1;
    }
    drop(out);
    let forwarders = [
        child.stdout.take().map(|s| forward(s, io::stdout())),
        child.stderr.take().map(|s| forward(s, io::stderr())),
    ];
    // 目标退出（被回收）之后不再向它的 pid 发信号：回收与置位之间只隔几条指令，内核顺序分配 pid，不会在这么短的时间里复用
    let reaped = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = mpsc::channel::<()>();
    {
        let reaped = Arc::clone(&reaped);
        // 等目标退出：转发完剩余的输出后以它的退出码退出整个守护进程
        let _ = std::thread::Builder::new()
            .name("wisp-guard-wait".into())
            .spawn(move || {
                let status = child.wait();
                reaped.store(true, Ordering::SeqCst);
                let _ = done_tx.send(());
                for f in forwarders.into_iter().flatten() {
                    let _ = f.join();
                }
                std::process::exit(exit_code(status.ok()));
            });
    }
    // 读 stdin 直到 EOF：Wisp 关掉了管道、退出或崩溃
    let mut sink = [0u8; 256];
    let mut stdin = io::stdin().lock();
    loop {
        match stdin.read(&mut sink) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let Ok(target) = c_int::try_from(pid) else {
        return 1;
    };
    let signal = |sig: c_int| {
        if !reaped.load(Ordering::SeqCst) {
            // SAFETY: target 是本进程尚未回收的子进程
            unsafe { kill(target, sig) };
        }
    };
    signal(SIGTERM);
    if done_rx.recv_timeout(TERM_GRACE).is_err() {
        signal(SIGKILL);
        // 等待线程回收后会结束整个进程；万一转发线程被别的进程持有的管道卡住，再等一会儿就直接退出
        let _ = done_rx.recv_timeout(TERM_GRACE);
    }
    std::thread::sleep(TERM_GRACE);
    0
}

/// 把目标的一路输出原样转发到守护进程自己的对应输出
fn forward(mut from: impl Read + Send + 'static, mut to: impl Write + Send + 'static) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match from.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    // 写不出去（Wisp 不再读）时继续读完丢弃，免得目标阻塞在写管道上
                    let _ = to.write_all(&buf[..n]).and_then(|()| to.flush());
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    })
}

/// 守护进程的退出码：目标的退出码；被信号结束时为 128 + 信号
fn exit_code(status: Option<ExitStatus>) -> i32 {
    match status {
        Some(s) => s.code().or_else(|| s.signal().map(|sig| 128 + sig)).unwrap_or(1),
        None => 1,
    }
}

// ---------------------------------------------------------------------------
// pid 文件与清扫

/// pid 文件的内容：三行 `pid=` / `start=` / `exe=`
#[derive(Debug, PartialEq, Eq)]
pub(super) struct PidRecord {
    pub(super) pid: u32,
    pub(super) start: u64,
    pub(super) exe: Vec<u8>,
}

pub(super) fn encode_record(r: &PidRecord) -> Vec<u8> {
    let mut out = format!("pid={}\nstart={}\nexe=", r.pid, r.start).into_bytes();
    out.extend_from_slice(&r.exe);
    out.push(b'\n');
    out
}

pub(super) fn decode_record(bytes: &[u8]) -> Option<PidRecord> {
    let mut lines = bytes.splitn(3, |b| *b == b'\n');
    let field = |line: Option<&[u8]>, key: &str| -> Option<String> {
        let s = std::str::from_utf8(line?).ok()?;
        s.strip_prefix(key).map(str::to_owned)
    };
    let pid = field(lines.next(), "pid=")?.parse().ok()?;
    let start = field(lines.next(), "start=")?.parse().ok()?;
    let exe = lines.next()?.strip_prefix(b"exe=")?;
    let exe = exe.strip_suffix(b"\n").unwrap_or(exe);
    (!exe.is_empty()).then(|| PidRecord {
        pid,
        start,
        exe: exe.to_vec(),
    })
}

pub(super) fn write_pid_file(path: &Path, pid: u32) -> io::Result<()> {
    let start = process_start_time(pid).ok_or_else(|| io::Error::other("process start time"))?;
    let exe = process_exe(pid).ok_or_else(|| io::Error::other("process path"))?;
    let record = PidRecord {
        pid,
        start,
        exe: exe.into_os_string().into_vec(),
    };
    super::write_atomically(path, &encode_record(&record))
}

pub(super) fn sweep_orphans(pid_dir: &Path) {
    super::sweep_dir(pid_dir, |bytes| {
        let r = decode_record(bytes)?;
        let alive_same = process_start_time(r.pid) == Some(r.start)
            && process_exe(r.pid).is_some_and(|p| p.as_os_str().as_bytes() == r.exe.as_slice());
        if !alive_same {
            return Some(false);
        }
        let pid = c_int::try_from(r.pid).ok()?;
        // SAFETY: pid、启动时间与可执行文件都与记录一致，是上次留下的推理进程
        Some(unsafe { kill(pid, SIGKILL) } == 0)
    });
}

#[cfg(target_os = "macos")]
mod proc_info {
    use std::ffi::{OsString, c_int, c_void};
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;

    /// `<sys/proc_info.h>`：`PROC_PIDTBSDINFO` 返回 `struct proc_bsdinfo`（136 字节），`pbi_start_tvsec` 在偏移 120
    const PROC_PIDTBSDINFO: c_int = 3;
    const BSDINFO_SIZE: usize = 136;
    const START_OFFSET: usize = 120;
    /// `<libproc.h>`：`PROC_PIDPATHINFO_MAXSIZE`
    const PATH_MAX: usize = 4096;

    unsafe extern "C" {
        fn proc_pidinfo(pid: c_int, flavor: c_int, arg: u64, buffer: *mut c_void, buffersize: c_int) -> c_int;
        fn proc_pidpath(pid: c_int, buffer: *mut c_void, buffersize: u32) -> c_int;
    }

    pub(super) fn start_time(pid: u32) -> Option<u64> {
        let pid = c_int::try_from(pid).ok().filter(|p| *p > 0)?;
        let mut buf = [0u8; BSDINFO_SIZE];
        let size = c_int::try_from(BSDINFO_SIZE).ok()?;
        // SAFETY: buf 是 BSDINFO_SIZE 字节的可写缓冲区，大小如实传入
        let n = unsafe { proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, buf.as_mut_ptr().cast(), size) };
        if usize::try_from(n).ok()? != BSDINFO_SIZE {
            return None;
        }
        buf.get(START_OFFSET..START_OFFSET + 8)
            .and_then(|b| b.try_into().ok())
            .map(u64::from_ne_bytes)
    }

    pub(super) fn exe(pid: u32) -> Option<PathBuf> {
        let pid = c_int::try_from(pid).ok().filter(|p| *p > 0)?;
        let mut buf = vec![0u8; PATH_MAX];
        let cap = u32::try_from(buf.len()).ok()?;
        // SAFETY: buf 可写 cap 字节；返回写入的字节数（不含 NUL）
        let n = unsafe { proc_pidpath(pid, buf.as_mut_ptr().cast::<c_void>(), cap) };
        let n = usize::try_from(n).ok().filter(|n| *n > 0 && *n <= buf.len())?;
        buf.truncate(n);
        Some(PathBuf::from(OsString::from_vec(buf)))
    }
}

#[cfg(target_os = "linux")]
mod proc_info {
    use std::path::PathBuf;

    /// `/proc/<pid>/stat` 的 starttime 以 USER_HZ 计（Linux 各架构的用户态 ABI 都固定为 100）
    const USER_HZ: u64 = 100;

    /// `/proc/<pid>/stat` 的第 22 个字段（开机以来的时钟滴答数）。comm 可能含空格与括号，从最后一个 `)` 之后数
    pub(super) fn parse_start_ticks(stat: &str) -> Option<u64> {
        let rest = &stat[stat.rfind(')')? + 1..];
        rest.split_whitespace().nth(22 - 3)?.parse().ok()
    }

    /// `/proc/stat` 的 `btime`（开机时刻，Unix 秒）
    pub(super) fn parse_btime(stat: &str) -> Option<u64> {
        stat.lines()
            .find_map(|l| l.strip_prefix("btime "))
            .and_then(|v| v.trim().parse().ok())
    }

    pub(super) fn start_time(pid: u32) -> Option<u64> {
        if pid == 0 {
            return None;
        }
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let boot = parse_btime(&std::fs::read_to_string("/proc/stat").ok()?)?;
        Some(boot + parse_start_ticks(&stat)? / USER_HZ)
    }

    pub(super) fn exe(pid: u32) -> Option<PathBuf> {
        // 可执行文件已被删除时链接带「 (deleted)」后缀，与记录对不上 → 不会被误杀
        std::fs::read_link(format!("/proc/{pid}/exe")).ok()
    }
}

pub(super) fn process_start_time(pid: u32) -> Option<u64> {
    proc_info::start_time(pid)
}

fn process_exe(pid: u32) -> Option<PathBuf> {
    proc_info::exe(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_args_round_trip() {
        let args = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            parse_guard_args(args(&["--nice", "--env", "LLAMA_API_KEY", "--", "/bin/x", "-m", "a b"]).into_iter()),
            Some(GuardArgs {
                nice: true,
                env: args(&["LLAMA_API_KEY"]),
                program: OsString::from("/bin/x"),
                args: args(&["-m", "a b"]),
            })
        );
        assert_eq!(
            parse_guard_args(args(&["--", "/bin/x"]).into_iter()).map(|a| a.nice),
            Some(false)
        );
        assert_eq!(parse_guard_args(args(&["/bin/x"]).into_iter()), None, "必须有 --");
        assert_eq!(parse_guard_args(args(&["--bogus", "--", "/bin/x"]).into_iter()), None);
        assert_eq!(parse_guard_args(args(&["--"]).into_iter()), None, "缺程序");
    }

    #[test]
    fn reports_are_parsed() {
        assert_eq!(parse_report("wisp-guard pid=4242").expect("pid"), 4242);
        assert_eq!(
            parse_report("wisp-guard error=not_found").map_err(|e| e.kind()),
            Err(io::ErrorKind::NotFound)
        );
        assert!(parse_report("wisp-guard pid=0").is_err());
        assert!(parse_report("hello").is_err());
    }

    #[test]
    fn pid_records_round_trip() {
        let r = PidRecord {
            pid: 77,
            start: 1_790_000_000,
            exe: b"/Users/x/Library/Application Support/wisp/models/runtimes/llama.cpp/b11669/llama-server".to_vec(),
        };
        assert_eq!(decode_record(&encode_record(&r)), Some(r));
        assert_eq!(decode_record(b"pid=1\nstart=2\nexe=\n"), None);
        assert_eq!(decode_record(b"garbage"), None);
    }

    #[test]
    fn own_start_time_and_path() {
        let me = std::process::id();
        let start = process_start_time(me).expect("start");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        assert!(start <= now && now - start < 24 * 3600, "{start} vs {now}");
        assert_eq!(
            process_exe(me).map(|p| p.canonicalize().ok()),
            Some(std::env::current_exe().and_then(|p| p.canonicalize()).ok())
        );
        assert_eq!(process_start_time(0), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_stat_fields() {
        let stat = "1234 (a (b) c) S 1 1234 1234 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 98765 1000 100";
        assert_eq!(proc_info::parse_start_ticks(stat), Some(98765));
        assert_eq!(
            proc_info::parse_btime("cpu 1 2\nbtime 1790000000\n"),
            Some(1_790_000_000)
        );
    }
}
