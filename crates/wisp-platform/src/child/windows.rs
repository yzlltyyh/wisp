//! 受托管子进程的 Windows 实现：Job Object（`KILL_ON_JOB_CLOSE`）与 pid 文件。
//!
//! 子进程以 `CREATE_SUSPENDED` 启动，加入 Job 之后才恢复主线程：它在加入之前没有机会再派生进程（否则孙进程不在 Job 里）。
//! Job 句柄只有 Wisp 持有（不可继承）；Wisp 退出或崩溃时内核关闭最后一个句柄，结束 Job 里的整棵进程树。

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use ::windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use ::windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use ::windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
    TerminateJobObject,
};
use ::windows::Win32::System::Threading::{
    BELOW_NORMAL_PRIORITY_CLASS, CREATE_NO_WINDOW, CREATE_SUSPENDED, GetProcessTimes, OpenProcess, OpenThread,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, QueryFullProcessImageNameW, ResumeThread,
    THREAD_SUSPEND_RESUME, TerminateProcess,
};
use ::windows::core::{PCWSTR, PWSTR};

use super::BoundOptions;

/// 子进程保留的环境变量：系统目录（加载系统 DLL、Vulkan 驱动需要）与临时目录
pub(super) const ESSENTIAL_ENV: &[&str] = &["SystemRoot", "windir", "SystemDrive", "TEMP", "TMP", "LOCALAPPDATA"];

/// FILETIME（1601-01-01 起的 100 ns 数）与 Unix 纪元之差
const UNIX_EPOCH_AS_FILETIME: u64 = 116_444_736_000_000_000;
/// `ResumeThread` 失败的返回值
const RESUME_FAILED: u32 = u32::MAX;

/// 一个内核句柄，丢弃即关闭
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: 句柄由本结构体独占，只关闭一次
        let _ = unsafe { CloseHandle(self.0) };
    }
}

// SAFETY: 内核句柄是不透明值，可以在线程间传递与关闭
unsafe impl Send for OwnedHandle {}
// SAFETY: 同上；共享时只用于 TerminateJobObject 这类线程安全的系统调用
unsafe impl Sync for OwnedHandle {}

/// 子进程所在的 Job Object（关闭最后一个句柄时内核结束整棵进程树）。创建或加入 Job 失败时为 None（只剩显式停止与 pid 文件）
pub(super) struct Binding {
    job: Option<OwnedHandle>,
}

/// 新建一个设置了 `KILL_ON_JOB_CLOSE` 的 Job
fn create_job() -> io::Result<OwnedHandle> {
    // SAFETY: 不带安全属性（句柄不可继承）、不带名字
    let job = OwnedHandle(unsafe { CreateJobObjectW(None, PCWSTR::null()) }.map_err(io::Error::other)?);
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let size = u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).map_err(io::Error::other)?;
    // SAFETY: info 是按文档初始化的 JOBOBJECT_EXTENDED_LIMIT_INFORMATION，大小如实传入
    unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            std::ptr::from_ref(&info).cast(),
            size,
        )
    }
    .map_err(io::Error::other)?;
    Ok(job)
}

/// 恢复进程里全部被挂起的线程（`CREATE_SUSPENDED` 只挂起主线程；`std::process::Child` 不暴露线程句柄，按线程快照找）
fn resume_threads(pid: u32) -> io::Result<()> {
    // SAFETY: 线程快照；返回的句柄由 OwnedHandle 关闭
    let snap = OwnedHandle(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }.map_err(io::Error::other)?);
    let mut entry = THREADENTRY32 {
        dwSize: u32::try_from(std::mem::size_of::<THREADENTRY32>()).map_err(io::Error::other)?,
        ..Default::default()
    };
    let mut resumed = 0usize;
    // SAFETY: entry 是设置了 dwSize 的 THREADENTRY32，快照句柄有效
    let mut ok = unsafe { Thread32First(snap.0, &mut entry) }.is_ok();
    while ok {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: 打开目标进程的线程句柄，只用来恢复它
            if let Ok(h) = unsafe { OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID) } {
                let h = OwnedHandle(h);
                // SAFETY: h 是有效的线程句柄
                if unsafe { ResumeThread(h.0) } != RESUME_FAILED {
                    resumed += 1;
                }
            }
        }
        // SAFETY: 同上
        ok = unsafe { Thread32Next(snap.0, &mut entry) }.is_ok();
    }
    if resumed == 0 {
        return Err(io::Error::other("could not resume the child process"));
    }
    Ok(())
}

pub(super) fn spawn(mut cmd: Command, opts: BoundOptions) -> io::Result<(Child, u32, Binding)> {
    // 守护进程在 Windows 上不需要：Job Object 由内核保证随 Wisp 结束
    let mut flags = CREATE_NO_WINDOW.0 | CREATE_SUSPENDED.0;
    if opts.low_priority {
        flags |= BELOW_NORMAL_PRIORITY_CLASS.0;
    }
    cmd.creation_flags(flags);
    // Job 创建失败（极少见，例如系统资源耗尽）时仍然启动：只是失去崩溃兜底，显式停止与 pid 文件照常
    let job = match create_job() {
        Ok(j) => Some(j),
        Err(e) => {
            log::warn!("child: job object unavailable: {e}");
            None
        }
    };
    let mut child = cmd.spawn()?;
    let pid = child.id();
    let job = job.filter(|j| {
        let process = HANDLE(child.as_raw_handle());
        // SAFETY: j 是有效的 Job 句柄，process 是刚启动（挂起中）的子进程句柄
        let assigned = unsafe { AssignProcessToJobObject(j.0, process) };
        if let Err(e) = &assigned {
            log::warn!("child: could not assign the process to the job: {e}");
        }
        assigned.is_ok()
    });
    if let Err(e) = resume_threads(pid) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }
    Ok((child, pid, Binding { job }))
}

/// 结束 Job 里的整棵进程树；没有 Job 时只结束子进程
pub(super) fn kill_tree(child: &mut Child, binding: &Binding) -> io::Result<()> {
    if let Some(job) = &binding.job {
        // SAFETY: job 是有效的 Job 句柄
        if unsafe { TerminateJobObject(job.0, 1) }.is_ok() {
            return Ok(());
        }
    }
    child.kill()
}

/// Windows 不使用守护进程
pub(super) fn guard_main() -> i32 {
    2
}

/// 打开一个进程（只查询，或查询 + 结束）
fn open_process(pid: u32, terminate: bool) -> Option<OwnedHandle> {
    let access = if terminate {
        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE
    } else {
        PROCESS_QUERY_LIMITED_INFORMATION
    };
    // SAFETY: 只打开句柄，权限按需申请；返回的句柄由 OwnedHandle 关闭
    unsafe { OpenProcess(access, false, pid) }.ok().map(OwnedHandle)
}

fn start_time_of(h: &OwnedHandle) -> Option<u64> {
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: h 是有查询权限的进程句柄；四个输出参数都是可写的 FILETIME
    unsafe { GetProcessTimes(h.0, &mut created, &mut exited, &mut kernel, &mut user) }.ok()?;
    let ticks = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    Some(ticks.checked_sub(UNIX_EPOCH_AS_FILETIME)? / 10_000_000)
}

fn exe_of(h: &OwnedHandle) -> Option<PathBuf> {
    let mut buf = vec![0u16; 32_768];
    let mut len = u32::try_from(buf.len()).ok()?;
    // SAFETY: buf 可写 len 个 UTF-16 单元，函数把实际长度（不含 NUL）写回 len
    unsafe { QueryFullProcessImageNameW(h.0, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) }.ok()?;
    buf.truncate(usize::try_from(len).ok()?);
    Some(PathBuf::from(OsString::from_wide(&buf)))
}

pub(super) fn process_start_time(pid: u32) -> Option<u64> {
    start_time_of(&open_process(pid, false)?)
}

/// pid 文件：三行 `pid=` / `start=` / `exe=`（路径按 UTF-8 保存；Windows 路径本身是 UTF-16，非法代理对按替换字符保存，
/// 与读取时同样处理，比较结果一致）
pub(super) fn write_pid_file(path: &Path, pid: u32) -> io::Result<()> {
    let h = open_process(pid, false).ok_or_else(|| io::Error::other("open process"))?;
    let start = start_time_of(&h).ok_or_else(|| io::Error::other("process start time"))?;
    let exe = exe_of(&h).ok_or_else(|| io::Error::other("process path"))?;
    let text = format!("pid={pid}\nstart={start}\nexe={}\n", exe.to_string_lossy());
    super::write_atomically(path, text.as_bytes())
}

fn decode_record(bytes: &[u8]) -> Option<(u32, u64, String)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut lines = text.splitn(3, '\n');
    let pid = lines.next()?.strip_prefix("pid=")?.parse().ok()?;
    let start = lines.next()?.strip_prefix("start=")?.parse().ok()?;
    let exe = lines.next()?.strip_prefix("exe=")?.trim_end_matches(['\r', '\n']);
    (!exe.is_empty()).then(|| (pid, start, exe.to_owned()))
}

pub(super) fn sweep_orphans(pid_dir: &Path) {
    super::sweep_dir(pid_dir, |bytes| {
        let (pid, start, exe) = decode_record(bytes)?;
        let Some(h) = open_process(pid, true) else {
            return Some(false);
        };
        let same = start_time_of(&h) == Some(start)
            && exe_of(&h).is_some_and(|p| {
                // 路径大小写不敏感
                let a: Vec<u16> = p.as_os_str().encode_wide().collect();
                let b: Vec<u16> = std::ffi::OsStr::new(&exe).encode_wide().collect();
                String::from_utf16_lossy(&a).to_lowercase() == String::from_utf16_lossy(&b).to_lowercase()
            });
        if !same {
            return Some(false);
        }
        // SAFETY: h 有结束权限，且 pid、启动时间与路径都与记录一致
        Some(unsafe { TerminateProcess(h.0, 1) }.is_ok())
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_decode() {
        assert_eq!(
            decode_record(b"pid=12\nstart=34\nexe=C:\\x\\llama-server.exe\r\n"),
            Some((12, 34, "C:\\x\\llama-server.exe".to_owned()))
        );
        assert_eq!(decode_record(b"pid=12\n"), None);
    }
}
