//! Linux：`/proc/meminfo` 与 cgroup v2 的内存上限、`/sys/devices/system/cpu` 的物理核、`statvfs`。

use std::collections::BTreeSet;
use std::ffi::{CString, c_char, c_int, c_ulong};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::MemoryInfo;

/// `struct statvfs`（glibc），只读到 `f_bavail`。Linux 构建只面向 64 位系统，`unsigned long` 与 `fsblkcnt_t` 都是 64 位
/// （下面的断言让 32 位构建直接编译失败，而不是读错字段）。尾部预留足够的空间让 libc 写完整个结构体
#[repr(C)]
struct StatVfs {
    f_bsize: u64,
    f_frsize: u64,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    rest: [u64; 16],
}

const _: () = assert!(std::mem::size_of::<c_ulong>() == 8);

unsafe extern "C" {
    fn statvfs(path: *const c_char, buf: *mut StatVfs) -> c_int;
}

pub(super) fn memory() -> Option<MemoryInfo> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let (total, available) = parse_meminfo(&meminfo)?;
    // 容器或 systemd 限制了内存时，能用的是限制值减去已用量（读不到 cgroup 时按物理内存算）
    let limit = read_u64("/sys/fs/cgroup/memory.max");
    let used = read_u64("/sys/fs/cgroup/memory.current");
    Some(apply_cgroup(total, available, limit, used))
}

/// 物理核数：`/sys/devices/system/cpu/cpu<N>/topology/` 的 (封装号, 核号) 去重。容器或 taskset 限制了可用的 CPU 时，
/// 不超过本进程能用的逻辑核数
pub(super) fn performance_cores() -> Option<usize> {
    let mut cores = BTreeSet::new();
    for entry in std::fs::read_dir("/sys/devices/system/cpu").ok()?.flatten() {
        let name = entry.file_name();
        let Some(n) = name.to_str().and_then(|s| s.strip_prefix("cpu")) else {
            continue;
        };
        if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let topo = entry.path().join("topology");
        let read = |f: &str| std::fs::read_to_string(topo.join(f)).ok().map(|s| s.trim().to_owned());
        // 离线的 CPU 没有 topology；读不到核号的不计
        if let Some(core) = read("core_id") {
            cores.insert((read("physical_package_id").unwrap_or_default(), core));
        }
    }
    let usable = std::thread::available_parallelism().map_or(usize::MAX, usize::from);
    Some(cores.len().min(usable)).filter(|n| *n > 0)
}

/// `statvfs` 的 `f_bavail * f_frsize`（非 root 用户可用的块）
pub(super) fn disk_free(path: &Path) -> Option<u64> {
    let dir = super::existing_ancestor(path)?;
    let c = CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: StatVfs 只含整数，全零是合法值
    let mut buf: StatVfs = unsafe { std::mem::zeroed() };
    // SAFETY: c 是以 NUL 结尾的路径；buf 的前几个字段按 glibc 的 struct statvfs 布局声明，且整体比它大
    if unsafe { statvfs(c.as_ptr(), &raw mut buf) } != 0 {
        return None;
    }
    let frsize = if buf.f_frsize == 0 { buf.f_bsize } else { buf.f_frsize };
    Some(buf.f_bavail.saturating_mul(frsize))
}

pub(super) fn is_apple_silicon() -> bool {
    false
}

/// cgroup 文件里的一个数字（`max` 表示不限，返回 None）
fn read_u64(path: &str) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// `/proc/meminfo` 的 `MemTotal` 与 `MemAvailable`（kB → 字节）
fn parse_meminfo(text: &str) -> Option<(u64, u64)> {
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            .map(|kb| kb.saturating_mul(1024))
    };
    let total = field("MemTotal")?;
    let available = field("MemAvailable")?;
    Some((total, available.min(total)))
}

fn apply_cgroup(total: u64, available: u64, limit: Option<u64>, used: Option<u64>) -> MemoryInfo {
    match limit.filter(|l| *l < total) {
        Some(limit) => MemoryInfo {
            total: limit,
            available: available.min(limit.saturating_sub(used.unwrap_or(0))),
        },
        None => MemoryInfo { total, available },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo_and_cgroup() {
        let text = "MemTotal:       16384000 kB\nMemFree:         1000 kB\nMemAvailable:    8192000 kB\n";
        assert_eq!(parse_meminfo(text), Some((16_384_000 * 1024, 8_192_000 * 1024)));
        assert_eq!(parse_meminfo("MemFree: 1 kB\n"), None);
        let gib = 1024 * 1024 * 1024;
        assert_eq!(
            apply_cgroup(16 * gib, 8 * gib, Some(4 * gib), Some(gib)),
            MemoryInfo {
                total: 4 * gib,
                available: 3 * gib
            }
        );
        assert_eq!(
            apply_cgroup(16 * gib, 8 * gib, None, Some(gib)),
            MemoryInfo {
                total: 16 * gib,
                available: 8 * gib
            }
        );
    }
}
