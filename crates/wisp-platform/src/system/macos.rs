//! macOS：`hw.memsize` 与 `host_statistics64(HOST_VM_INFO64)`、`hw.perflevel0.physicalcpu`、`statfs`。

use std::ffi::{CString, c_char, c_int, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::MemoryInfo;

/// `<mach/host_info.h>`：`HOST_VM_INFO64`
const HOST_VM_INFO64: c_int = 4;
/// `<mach/kern_return.h>`：`KERN_SUCCESS`
const KERN_SUCCESS: c_int = 0;

/// `struct vm_statistics64`（`<mach/vm_statistics.h>`），只读前面几项页数。结构体尾部预留了空间：内核按调用方给的
/// `count` 与自己的版本取较小者填写，新系统在末尾追加字段时不会越界
#[repr(C)]
#[derive(Default)]
struct VmStatistics64 {
    free_count: u32,
    active_count: u32,
    inactive_count: u32,
    wire_count: u32,
    zero_fill_count: u64,
    reactivations: u64,
    pageins: u64,
    pageouts: u64,
    faults: u64,
    cow_faults: u64,
    lookups: u64,
    hits: u64,
    purges: u64,
    purgeable_count: u32,
    speculative_count: u32,
    decompressions: u64,
    compressions: u64,
    swapins: u64,
    swapouts: u64,
    compressor_page_count: u32,
    throttled_count: u32,
    external_page_count: u32,
    internal_page_count: u32,
    total_uncompressed_pages_in_compressor: u64,
    reserved: [u64; 8],
}

/// `struct statfs`（`<sys/mount.h>`，arm64 上只有 64 位 inode 版本），只读 `f_bsize` 与 `f_bavail`
#[repr(C)]
struct StatFs {
    f_bsize: u32,
    f_iosize: i32,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_fsid: [i32; 2],
    f_owner: u32,
    f_type: u32,
    f_flags: u32,
    f_fssubtype: u32,
    f_fstypename: [c_char; 16],
    f_mntonname: [c_char; 1024],
    f_mntfromname: [c_char; 1024],
    f_flags_ext: u32,
    f_reserved: [u32; 7],
}

// libSystem（总是链接）
unsafe extern "C" {
    fn sysctlbyname(
        name: *const c_char,
        oldp: *mut c_void,
        oldlenp: *mut usize,
        newp: *mut c_void,
        newlen: usize,
    ) -> c_int;
    fn mach_host_self() -> u32;
    fn host_statistics64(host: u32, flavor: c_int, info: *mut c_void, count: *mut u32) -> c_int;
    fn mach_port_deallocate(task: u32, name: u32) -> c_int;
    static mach_task_self_: u32;
    fn statfs(path: *const c_char, buf: *mut StatFs) -> c_int;
}

/// 读一个整数型的 sysctl（`hw.memsize` 是 8 字节，`hw.perflevel0.physicalcpu` 是 4 字节；按返回的长度解释）
pub(crate) fn sysctl_u64(name: &str) -> Option<u64> {
    let name = CString::new(name).ok()?;
    let mut buf = [0u8; 8];
    let mut len = buf.len();
    // SAFETY: name 是以 NUL 结尾的 C 字符串；buf 可写 len 字节，内核写入后把实际长度写回 len；不设置新值
    let rc = unsafe {
        sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    match len {
        4 => Some(u64::from(u32::from_ne_bytes(buf[..4].try_into().ok()?))),
        8 => Some(u64::from_ne_bytes(buf)),
        _ => None,
    }
}

pub(super) fn memory() -> Option<MemoryInfo> {
    let total = sysctl_u64("hw.memsize").filter(|t| *t > 0)?;
    let page = sysctl_u64("hw.pagesize").filter(|p| *p > 0)?;
    let mut stats = VmStatistics64::default();
    let mut count = u32::try_from(std::mem::size_of::<VmStatistics64>() / std::mem::size_of::<u32>()).ok()?;
    // SAFETY: mach_host_self 返回本机 host 端口的发送权（用完释放）；stats 可写 count 个 32 位整数，内核按 count 与
    // 自身版本的较小者填写并把实际数量写回
    let rc = unsafe {
        let host = mach_host_self();
        let rc = host_statistics64(
            host,
            HOST_VM_INFO64,
            std::ptr::from_mut(&mut stats).cast(),
            &raw mut count,
        );
        mach_port_deallocate(mach_task_self_, host);
        rc
    };
    if rc != KERN_SUCCESS {
        return None;
    }
    let pages = available_pages(&stats);
    Some(MemoryInfo {
        total,
        available: pages.saturating_mul(page).min(total),
    })
}

/// 不压缩、不换出其他程序的活动内存就能拿到的页：空闲（含 speculative）+ 可清除（purgeable）+「不活跃页」与「文件缓存页」
/// 中较大的一项。不活跃页里的匿名页压缩代价很小、文件缓存页（不论活跃与否）可以直接丢弃再从磁盘读回；两者有重叠
/// （不活跃的文件页），分不开，取较大的一项不会重复计算。压缩器占用的页与 wired 页不算。
///
/// 这仍是保守的估计：系统在需要时还能压缩活动的匿名页（大模型的权重是 mmap 的文件页，也能被换出再读回），所以加载前的检查
/// 不应只看它，还要结合 [`crate::pressure::current`]
fn available_pages(stats: &VmStatistics64) -> u64 {
    u64::from(stats.free_count)
        .saturating_add(u64::from(stats.purgeable_count))
        .saturating_add(u64::from(stats.inactive_count.max(stats.external_page_count)))
}

pub(super) fn performance_cores() -> Option<usize> {
    // Apple 芯片按性能档分组（perflevel0 是最快的一档）；Intel Mac 没有这一项，退回物理核数
    let n = sysctl_u64("hw.perflevel0.physicalcpu").or_else(|| sysctl_u64("hw.physicalcpu"))?;
    usize::try_from(n).ok().filter(|n| *n > 0)
}

pub(super) fn disk_free(path: &Path) -> Option<u64> {
    let dir = super::existing_ancestor(path)?;
    let c = CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: StatFs 只含整数与字节数组，全零是合法值
    let mut buf: StatFs = unsafe { std::mem::zeroed() };
    // SAFETY: c 是以 NUL 结尾的路径；buf 是按 <sys/mount.h> 布局声明的可写 struct statfs
    if unsafe { statfs(c.as_ptr(), &raw mut buf) } != 0 {
        return None;
    }
    Some(buf.f_bavail.saturating_mul(u64::from(buf.f_bsize)))
}

/// Wisp 的 macOS 版本只发布 Apple 芯片构建；以运行时的架构判断（Rosetta 下运行的 x86_64 构建视为 Intel）
pub(super) fn is_apple_silicon() -> bool {
    std::env::consts::ARCH == "aarch64"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statfs_layout_matches_the_sdk() {
        // <sys/mount.h>：64 位 inode 版本的 struct statfs 是 2168 字节
        assert_eq!(std::mem::size_of::<StatFs>(), 2168);
    }

    #[test]
    fn file_cache_counts_when_it_exceeds_inactive_pages() {
        let base = VmStatistics64 {
            free_count: 100,
            inactive_count: 300,
            purgeable_count: 10,
            external_page_count: 200,
            ..VmStatistics64::default()
        };
        assert_eq!(available_pages(&base), 410);
        let cached = VmStatistics64 {
            external_page_count: 900,
            ..base
        };
        assert_eq!(available_pages(&cached), 1010);
    }

    #[test]
    fn readings_are_plausible() {
        let m = memory().expect("memory");
        assert!(m.total >= 4 << 30, "{m:?}");
        assert!(m.available > 0 && m.available <= m.total, "{m:?}");
        let cores = performance_cores().expect("cores");
        assert!((1..=64).contains(&cores));
        let free = disk_free(Path::new("/tmp/definitely/not/here")).expect("disk");
        assert!(free > 0);
    }
}
