//! 本机硬件概况：内存总量与可用量、性能核数、磁盘可用空间、是否为 Apple 芯片。
//!
//! 只在用到离线模型时调用（设备适配、加载前的内存检查、下载前的空间检查），都是一次系统调用或读几个 procfs 文件，
//! 不阻塞、不缓存、不起线程。取不到时返回 None（调用方按「未知」处理），不猜测。

use std::path::Path;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
use linux as imp;
#[cfg(target_os = "macos")]
use macos as imp;
#[cfg(target_os = "windows")]
use windows as imp;

/// 整数型 sysctl（内存压力的即时查询也用它）
#[cfg(target_os = "macos")]
pub(crate) use macos::sysctl_u64;

/// 内存概况（字节）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryInfo {
    /// 物理内存总量。Linux 在 cgroup v2 限制了内存时取限制值与物理内存的较小者
    pub total: u64,
    /// 现在可用的内存（不换出其他程序的活动内存就能拿到的部分）。macOS 为空闲 + 可清除 + 不活跃页与文件缓存页中较大的
    /// 一项（不含压缩器还能腾出的空间，偏保守，加载前的检查应结合 [`crate::pressure::current`]）；Windows 为 `ullAvailPhys`
    /// （含备用列表）；Linux 为 `MemAvailable` 与 cgroup 剩余额度的较小者
    pub available: u64,
}

/// 内存总量与可用量；取不到时为 None
pub fn memory() -> Option<MemoryInfo> {
    imp::memory()
}

/// 性能核数：macOS 为 `hw.perflevel0.physicalcpu`（不含能效核），其他系统为物理核数。用来定推理线程数
/// （超线程与能效核对矩阵运算帮助不大，反而与前台程序争抢）。取不到时退回逻辑核数，至少为 1
pub fn performance_cores() -> usize {
    imp::performance_cores().unwrap_or_else(|| std::thread::available_parallelism().map_or(1, usize::from))
}

/// `path` 所在磁盘的可用空间（字节，当前用户可写的部分）。路径还不存在时按最近的已存在的上级目录计算（模型目录在第一次
/// 下载之前并不存在，设置页仍要显示可用空间）；取不到时为 None
pub fn disk_free(path: &Path) -> Option<u64> {
    imp::disk_free(path)
}

/// `path` 本身或最近的已存在的上级目录
fn existing_ancestor(path: &Path) -> Option<&Path> {
    path.ancestors().find(|p| !p.as_os_str().is_empty() && p.exists())
}

/// 是否为 Apple 芯片（统一内存，llama.cpp 用 Metal；设备适配按内存总量判断，不看显存）
pub fn is_apple_silicon() -> bool {
    imp::is_apple_silicon()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn performance_cores_is_positive() {
        assert!(performance_cores() >= 1);
    }

    /// 本机能取到的值必须自洽（取不到时为 None，不编造）
    #[test]
    fn memory_is_consistent_when_available() {
        if let Some(m) = memory() {
            assert!(m.total > 0);
            assert!(m.available <= m.total);
        }
    }

    #[test]
    fn disk_free_falls_back_to_an_existing_ancestor() {
        let dir = std::env::temp_dir();
        assert_eq!(
            existing_ancestor(&dir.join("wisp-no-such-dir/models")),
            Some(dir.as_path())
        );
        assert!(disk_free(&dir.join("wisp-no-such-dir/models")).is_some_and(|b| b > 0));
    }
}
