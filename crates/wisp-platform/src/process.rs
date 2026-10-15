//! 本进程的运行指标（冷启动计时的起点、常驻内存），供 desktop 的冒烟报告与性能埋点使用。
//!
//! - [`started_at`]：内核记录的进程创建时刻。冷启动「托盘就绪 ≤ 800 ms」要从这里算起，而不是从 `main` / `run()`：
//!   exec、dyld 加载与静态初始化（macOS 首次运行新二进制时还有 Gatekeeper 扫描，实测约 540 ms）都在 `main` 之前，
//!   用户在安装 / 更新后的第一次启动正好看到这一段。
//! - [`memory_kb`]：常驻内存，口径见 [`MEMORY_METRIC`]。macOS 用 `phys_footprint`（与活动监视器的「内存」、
//!   `footprint` 工具一致，不含共享的系统框架页）；`ps -o rss` 会把 AppKit / WebKit 框架的共享页面全部算进来，
//!   一个空的 AppKit 进程就有 60–90 MB，与内存预算的口径不可比。Windows 用工作集（任务管理器「内存」的口径），
//!   Linux 用 VmRSS。
//!
//! 都不阻塞（一次系统调用或读一个 procfs 文件），失败时返回 None。

use std::time::{Duration, SystemTime};

/// [`memory_kb`] 的口径：`phys_footprint`（macOS）| `working_set`（Windows）| `rss`（Linux）
#[cfg(target_os = "macos")]
pub const MEMORY_METRIC: &str = "phys_footprint";
/// [`memory_kb`] 的口径：`phys_footprint`（macOS）| `working_set`（Windows）| `rss`（Linux）
#[cfg(target_os = "windows")]
pub const MEMORY_METRIC: &str = "working_set";
/// [`memory_kb`] 的口径：`phys_footprint`（macOS）| `working_set`（Windows）| `rss`（Linux）
#[cfg(target_os = "linux")]
pub const MEMORY_METRIC: &str = "rss";

/// 本进程被内核创建的时刻；取不到时为 None（调用方退回自己记录的时刻）
pub fn started_at() -> Option<SystemTime> {
    imp::started_at()
}

/// 本进程的常驻内存（KiB，口径见 [`MEMORY_METRIC`]）；取不到时为 None
pub fn memory_kb() -> Option<u64> {
    imp::memory_kb()
}

/// 本进程累计的 CPU 时间（用户态 + 内核态）；取不到时为 None
pub fn cpu_time() -> Option<Duration> {
    imp::cpu_time()
}

/// 本进程累计「把 CPU 从空闲唤醒」的次数（与 top 的 IDLEW、活动监视器的「空闲唤醒」同一口径）。只有 macOS 能取到
pub fn idle_wakeups() -> Option<u64> {
    imp::idle_wakeups()
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{c_int, c_void};
    use std::time::{Duration, SystemTime};

    /// `<sys/proc_info.h>`：`PROC_PIDTBSDINFO` 返回 `struct proc_bsdinfo`（136 字节），`pbi_start_tvsec` /
    /// `pbi_start_tvusec` 在偏移 120 / 128（与 scripts/perf/macos-webkit.py 一致）
    const PROC_PIDTBSDINFO: c_int = 3;
    const BSDINFO_SIZE: usize = 136;
    const START_OFFSET: usize = 120;
    /// `<sys/resource.h>`：`RUSAGE_INFO_V2`
    const RUSAGE_INFO_V2: c_int = 2;

    /// `struct rusage_info_v2`（`<sys/resource.h>`），只读 `ri_phys_footprint`
    #[repr(C)]
    #[derive(Default)]
    struct RusageInfoV2 {
        uuid: [u8; 16],
        user_time: u64,
        system_time: u64,
        pkg_idle_wkups: u64,
        interrupt_wkups: u64,
        pageins: u64,
        wired_size: u64,
        resident_size: u64,
        phys_footprint: u64,
        proc_start_abstime: u64,
        proc_exit_abstime: u64,
        child_user_time: u64,
        child_system_time: u64,
        child_pkg_idle_wkups: u64,
        child_interrupt_wkups: u64,
        child_pageins: u64,
        child_elapsed_abstime: u64,
        diskio_bytesread: u64,
        diskio_byteswritten: u64,
    }

    /// `mach_timebase_info_data_t`：rusage_info 的 CPU 时间以 mach 绝对时间计（Apple 芯片上 1 tick ≈ 41.67 ns）
    #[repr(C)]
    #[derive(Default)]
    struct TimebaseInfo {
        numer: u32,
        denom: u32,
    }

    // libproc 与 mach（libSystem 的一部分，总是链接）
    unsafe extern "C" {
        fn proc_pidinfo(pid: c_int, flavor: c_int, arg: u64, buffer: *mut c_void, buffersize: c_int) -> c_int;
        fn proc_pid_rusage(pid: c_int, flavor: c_int, buffer: *mut c_void) -> c_int;
        fn mach_timebase_info(info: *mut TimebaseInfo) -> c_int;
    }

    fn rusage() -> Option<RusageInfoV2> {
        let mut info = RusageInfoV2::default();
        // SAFETY: info 是 rusage_info_v2 布局的可写结构体，RUSAGE_INFO_V2 让内核按这个布局填写
        let rc = unsafe { proc_pid_rusage(pid()?, RUSAGE_INFO_V2, std::ptr::from_mut(&mut info).cast()) };
        (rc == 0).then_some(info)
    }

    pub fn cpu_time() -> Option<Duration> {
        let info = rusage()?;
        let mut tb = TimebaseInfo::default();
        // SAFETY: tb 是可写的 mach_timebase_info_data_t
        if unsafe { mach_timebase_info(&raw mut tb) } != 0 || tb.denom == 0 {
            return None;
        }
        let ticks = u128::from(info.user_time) + u128::from(info.system_time);
        let nanos = ticks * u128::from(tb.numer) / u128::from(tb.denom);
        Some(Duration::from_nanos(u64::try_from(nanos).ok()?))
    }

    pub fn idle_wakeups() -> Option<u64> {
        rusage().map(|i| i.pkg_idle_wkups)
    }

    fn pid() -> Option<c_int> {
        c_int::try_from(std::process::id()).ok()
    }

    pub fn started_at() -> Option<SystemTime> {
        let mut buf = [0u8; BSDINFO_SIZE];
        let size = c_int::try_from(BSDINFO_SIZE).ok()?;
        // SAFETY: buf 是 BSDINFO_SIZE 字节的可写缓冲区，大小如实传入；proc_pidinfo 最多写这么多字节
        let n = unsafe { proc_pidinfo(pid()?, PROC_PIDTBSDINFO, 0, buf.as_mut_ptr().cast(), size) };
        if usize::try_from(n).ok()? != BSDINFO_SIZE {
            return None;
        }
        let read = |at: usize| {
            buf.get(at..at + 8)
                .and_then(|b| b.try_into().ok())
                .map(u64::from_ne_bytes)
        };
        let (sec, usec) = (read(START_OFFSET)?, read(START_OFFSET + 8)?);
        SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(sec) + Duration::from_micros(usec))
    }

    pub fn memory_kb() -> Option<u64> {
        rusage().map(|i| i.phys_footprint / 1024)
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use std::time::{Duration, SystemTime};

    use ::windows::Win32::Foundation::FILETIME;
    use ::windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use ::windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    /// FILETIME（1601-01-01 起的 100 ns 数）与 Unix 纪元之差
    const UNIX_EPOCH_AS_FILETIME: u64 = 116_444_736_000_000_000;

    pub fn started_at() -> Option<SystemTime> {
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        // SAFETY: 伪句柄 GetCurrentProcess 总是有效；四个输出参数都是可写的 FILETIME
        unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user) }.ok()?;
        let ticks = filetime_ticks(created);
        let since_epoch = ticks.checked_sub(UNIX_EPOCH_AS_FILETIME)?;
        SystemTime::UNIX_EPOCH.checked_add(Duration::from_nanos(since_epoch.saturating_mul(100)))
    }

    fn filetime_ticks(t: FILETIME) -> u64 {
        (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
    }

    pub fn cpu_time() -> Option<Duration> {
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        // SAFETY: 同 started_at
        unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user) }.ok()?;
        let ticks = filetime_ticks(kernel).saturating_add(filetime_ticks(user));
        Some(Duration::from_nanos(ticks.saturating_mul(100)))
    }

    /// Windows 没有按进程统计的空闲唤醒次数
    pub fn idle_wakeups() -> Option<u64> {
        None
    }

    pub fn memory_kb() -> Option<u64> {
        let mut counters = PROCESS_MEMORY_COUNTERS::default();
        let cb = u32::try_from(std::mem::size_of::<PROCESS_MEMORY_COUNTERS>()).ok()?;
        // SAFETY: counters 是可写的 PROCESS_MEMORY_COUNTERS，cb 是它的大小
        unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, cb) }.ok()?;
        u64::try_from(counters.WorkingSetSize / 1024).ok()
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::time::{Duration, SystemTime};

    /// `/proc/<pid>/stat` 的 starttime 以 USER_HZ 计（Linux 各架构的用户态 ABI 都固定为 100）
    const USER_HZ: f64 = 100.0;

    /// `/proc/self/stat` 的第 22 个字段（starttime，开机以来的时钟滴答数）。comm 字段可能含空格与括号，
    /// 从最后一个 `)` 之后开始数：其后第一个字段是第 3 个（state）
    pub(super) fn parse_start_ticks(stat: &str) -> Option<u64> {
        let rest = &stat[stat.rfind(')')? + 1..];
        rest.split_whitespace().nth(22 - 3)?.parse().ok()
    }

    pub fn started_at() -> Option<SystemTime> {
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        let uptime = std::fs::read_to_string("/proc/uptime").ok()?;
        let uptime: f64 = uptime.split_whitespace().next()?.parse().ok()?;
        #[allow(clippy::cast_precision_loss)] // 滴答数远小于 2^52
        let started = parse_start_ticks(&stat)? as f64 / USER_HZ;
        let age = Duration::try_from_secs_f64((uptime - started).max(0.0)).ok()?;
        SystemTime::now().checked_sub(age)
    }

    /// `/proc/self/stat` 的 utime + stime（第 14、15 个字段，USER_HZ 滴答）
    pub(super) fn parse_cpu_ticks(stat: &str) -> Option<u64> {
        let rest = &stat[stat.rfind(')')? + 1..];
        let mut fields = rest.split_whitespace().skip(14 - 3);
        let utime: u64 = fields.next()?.parse().ok()?;
        let stime: u64 = fields.next()?.parse().ok()?;
        Some(utime + stime)
    }

    pub fn cpu_time() -> Option<Duration> {
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        #[allow(clippy::cast_precision_loss)] // 滴答数远小于 2^52
        let secs = parse_cpu_ticks(&stat)? as f64 / USER_HZ;
        Duration::try_from_secs_f64(secs).ok()
    }

    /// Linux 没有按进程统计的空闲唤醒次数（powertop 之类需要 root）
    pub fn idle_wakeups() -> Option<u64> {
        None
    }

    pub fn memory_kb() -> Option<u64> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        status
            .lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .and_then(|v| v.split_whitespace().next()?.parse().ok())
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;

    #[test]
    fn process_start_is_in_the_recent_past() {
        let started = started_at().expect("进程启动时刻");
        let age = SystemTime::now().duration_since(started).expect("不晚于现在");
        // 测试进程刚启动不久（留足 CI 的余量）
        assert!(age < Duration::from_secs(3600), "{age:?}");
    }

    #[test]
    fn memory_is_measured() {
        let kb = memory_kb().expect("常驻内存");
        assert!(kb > 100, "{kb} KiB");
        assert!(!MEMORY_METRIC.is_empty());
    }

    #[test]
    fn cpu_time_advances_with_work() {
        let before = cpu_time().expect("CPU 时间");
        let t0 = std::time::Instant::now();
        let mut x = 0u64;
        while t0.elapsed() < Duration::from_millis(60) {
            x = std::hint::black_box(x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1));
        }
        let after = cpu_time().expect("CPU 时间");
        assert!(after > before, "{before:?} → {after:?}");
        // 换算单位正确：60 ms 的忙循环不会被算成几秒（mach 绝对时间没有换算时会差 24 倍）
        assert!(after - before < Duration::from_secs(2), "{:?}", after - before);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn idle_wakeups_are_counted_on_macos() {
        assert!(idle_wakeups().is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stat_start_ticks_survive_odd_process_names() {
        let stat = "4242 (a) b (c)) S 1 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 10";
        assert_eq!(imp::parse_start_ticks(stat), Some(987_654));
        assert_eq!(imp::parse_cpu_ticks(stat), Some(1 + 2));
    }
}
