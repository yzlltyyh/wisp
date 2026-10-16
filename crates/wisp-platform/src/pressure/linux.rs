//! 内存压力通知的 Linux 实现：PSI 触发器（`/proc/pressure/memory`）。
//!
//! 写入 `some <阈值 µs> <窗口 µs>` 后对同一个文件描述符 `poll(POLLPRI)`：窗口内的停顿累计超过阈值时内核唤醒一次（每个窗口
//! 最多一次）。非特权进程要求窗口是 2 s 的整数倍（否则 EINVAL）；没有 PSI 的内核或容器打开 / 写入失败 → 不支持。
//! 两个触发器：`some`（有任务因内存停顿）报告 [`Level::Warning`]，`full`（全部任务都在停顿）报告 [`Level::Critical`]。
//! 刚安装时内核可能立即报告一次（触发器的初始状态），按 `total=` 的增量过滤：两次之间新增的停顿不到阈值就忽略。
//!
//! 一个线程阻塞在 `poll` 上（没有超时，不轮询）；卸载时关闭一条自用管道的写端，线程被唤醒后退出。

use std::ffi::{c_int, c_short, c_ulong};
use std::fs::{File, OpenOptions};
use std::io::{PipeReader, PipeWriter, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::thread::JoinHandle;

use super::{Callback, Level};

/// PSI 的数据文件
const PSI_MEMORY: &str = "/proc/pressure/memory";
/// 统计窗口（µs）：非特权触发器的最小值
const WINDOW_US: u64 = 2_000_000;
/// `some` 的阈值：2 s 内累计停顿 200 ms（10%）视为内存紧张
const SOME_US: u64 = 200_000;
/// `full` 的阈值：2 s 内全部任务累计停顿 100 ms 视为严重
const FULL_US: u64 = 100_000;

const POLLPRI: c_short = 0x002;
const POLLERR: c_short = 0x008;
const POLLHUP: c_short = 0x010;
const POLLNVAL: c_short = 0x020;
const EINTR: c_int = 4;

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: c_short,
    revents: c_short,
}

unsafe extern "C" {
    fn poll(fds: *mut PollFd, nfds: c_ulong, timeout: c_int) -> c_int;
}

/// 已安装的监听：丢弃时关闭管道写端并等线程退出
pub(super) struct Watch {
    stop: Option<PipeWriter>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// 一个触发器：文件描述符、上报的等级、阈值、上次的累计停顿
struct Trigger {
    file: File,
    level: Level,
    kind: &'static str,
    threshold_us: u64,
    last_total: Option<u64>,
}

fn open_trigger(kind: &'static str, threshold_us: u64, level: Level) -> Option<Trigger> {
    let mut file = OpenOptions::new().read(true).write(true).open(PSI_MEMORY).ok()?;
    // 内核要求整条触发器一次写完（含结尾的 NUL）
    file.write_all(format!("{kind} {threshold_us} {WINDOW_US}\0").as_bytes())
        .ok()?;
    Some(Trigger {
        file,
        level,
        kind,
        threshold_us,
        last_total: read_total(kind),
    })
}

/// 即时查询的阈值（最近 10 s 的平均停顿比例，%）：与触发器的阈值同一口径（`some` 10%、`full` 5%）
const SOME_AVG10_WARN: f64 = 10.0;
const FULL_AVG10_CRITICAL: f64 = 5.0;

/// 即时查询：读一次 `/proc/pressure/memory` 的 `avg10`
pub(super) fn current() -> Option<Level> {
    level_from_avg10(&std::fs::read_to_string(PSI_MEMORY).ok()?)
}

fn level_from_avg10(text: &str) -> Option<Level> {
    let avg10 = |kind: &str| {
        let line = text.lines().find(|l| l.split_whitespace().next() == Some(kind))?;
        line.split_whitespace()
            .find_map(|f| f.strip_prefix("avg10="))
            .and_then(|v| v.parse::<f64>().ok())
    };
    let some = avg10("some")?;
    // 部分内核或容器只有 some 一行
    if avg10("full").is_some_and(|f| f >= FULL_AVG10_CRITICAL) {
        return Some(Level::Critical);
    }
    Some(if some >= SOME_AVG10_WARN {
        Level::Warning
    } else {
        Level::Normal
    })
}

/// `/proc/pressure/memory` 里某一行（`some` / `full`）的 `total=`（µs）
fn read_total(kind: &str) -> Option<u64> {
    parse_total(&std::fs::read_to_string(PSI_MEMORY).ok()?, kind)
}

fn parse_total(text: &str, kind: &str) -> Option<u64> {
    let line = text.lines().find(|l| l.split_whitespace().next() == Some(kind))?;
    line.split_whitespace()
        .find_map(|f| f.strip_prefix("total="))
        .and_then(|v| v.parse().ok())
}

impl Trigger {
    /// 内核报告了一次：新增的停顿够阈值才算数（过滤安装时的那次报告）
    fn accept(&mut self) -> bool {
        let now = read_total(self.kind);
        let ok = match (self.last_total, now) {
            (Some(prev), Some(now)) => now.saturating_sub(prev) >= self.threshold_us,
            _ => true,
        };
        if now.is_some() {
            self.last_total = now;
        }
        ok
    }
}

pub(super) fn watch(cb: Callback) -> Option<Watch> {
    let mut triggers = vec![open_trigger("some", SOME_US, Level::Warning)?];
    // 部分内核或容器只接受 some；full 可选
    if let Some(full) = open_trigger("full", FULL_US, Level::Critical) {
        triggers.push(full);
    }
    let (reader, writer) = std::io::pipe().ok()?;
    let thread = std::thread::Builder::new()
        .name("wisp-mem-pressure".into())
        .spawn(move || run(triggers, &reader, &cb))
        .ok()?;
    Some(Watch {
        stop: Some(writer),
        thread: Some(thread),
    })
}

fn run(mut triggers: Vec<Trigger>, stop: &PipeReader, cb: &Callback) {
    let stop_fd: RawFd = stop.as_raw_fd();
    loop {
        let mut fds: Vec<PollFd> = triggers
            .iter()
            .map(|t| PollFd {
                fd: t.file.as_raw_fd(),
                events: POLLPRI,
                revents: 0,
            })
            .chain(std::iter::once(PollFd {
                fd: stop_fd,
                events: 0,
                revents: 0,
            }))
            .collect();
        let Ok(n) = c_ulong::try_from(fds.len()) else {
            return;
        };
        // SAFETY: fds 是 n 个有效的 pollfd；-1 = 一直等到有事件
        let rc = unsafe { poll(fds.as_mut_ptr(), n, -1) };
        if rc < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(EINTR) {
                continue;
            }
            return;
        }
        // 管道写端关闭（卸载）或出错：退出
        if fds.last().is_some_and(|f| f.revents != 0) {
            return;
        }
        let mut worst: Option<Level> = None;
        for (t, f) in triggers.iter_mut().zip(&fds) {
            if f.revents & (POLLERR | POLLHUP | POLLNVAL) != 0 {
                // 触发器失效（例如所在的 cgroup 被删除）：不再监听
                return;
            }
            if f.revents & POLLPRI != 0 && t.accept() {
                worst = worst.max(Some(t.level));
            }
        }
        if let Some(level) = worst {
            cb(level);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_are_parsed_per_line() {
        let text =
            "some avg10=0.00 avg60=0.00 avg300=0.00 total=12345\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=678\n";
        assert_eq!(parse_total(text, "some"), Some(12345));
        assert_eq!(parse_total(text, "full"), Some(678));
        assert_eq!(parse_total("cpu x", "some"), None);
    }

    #[test]
    fn current_level_follows_avg10() {
        let calm = "some avg10=0.12 avg60=0.05 avg300=0.01 total=1\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
        assert_eq!(level_from_avg10(calm), Some(Level::Normal));
        let tight = "some avg10=23.50 avg60=4.00 avg300=1.00 total=9\nfull avg10=1.20 avg60=0.10 avg300=0.00 total=2\n";
        assert_eq!(level_from_avg10(tight), Some(Level::Warning));
        let severe =
            "some avg10=60.00 avg60=20.00 avg300=5.00 total=9\nfull avg10=12.00 avg60=3.00 avg300=1.00 total=5\n";
        assert_eq!(level_from_avg10(severe), Some(Level::Critical));
        assert_eq!(
            level_from_avg10("some avg10=11.00 avg60=0 avg300=0 total=1\n"),
            Some(Level::Warning)
        );
        assert_eq!(level_from_avg10(""), None);
    }
}
