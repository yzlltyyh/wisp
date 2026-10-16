//! Windows：`GlobalMemoryStatusEx`、`GetLogicalProcessorInformationEx`、`GetDiskFreeSpaceExW`。

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use ::windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use ::windows::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, GlobalMemoryStatusEx, MEMORYSTATUSEX, RelationProcessorCore,
    SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};
use ::windows::core::PCWSTR;

use super::MemoryInfo;

pub(super) fn memory() -> Option<MemoryInfo> {
    let mut status = MEMORYSTATUSEX {
        dwLength: u32::try_from(std::mem::size_of::<MEMORYSTATUSEX>()).ok()?,
        ..Default::default()
    };
    // SAFETY: status 是按文档初始化了 dwLength 的 MEMORYSTATUSEX，函数只写入这个结构体
    unsafe { GlobalMemoryStatusEx(&mut status) }.ok()?;
    (status.ullTotalPhys > 0).then_some(MemoryInfo {
        total: status.ullTotalPhys,
        available: status.ullAvailPhys.min(status.ullTotalPhys),
    })
}

/// 物理核数（`GetLogicalProcessorInformationEx(RelationProcessorCore)`，每个核一条记录）。混合架构（Intel 大小核）上只数
/// `EfficiencyClass` 最高的那一档——推理线程放到能效核上反而拖慢整体
pub(super) fn performance_cores() -> Option<usize> {
    let mut len = 0u32;
    // SAFETY: 第一次只查询需要的缓冲区大小（buffer 为空），函数只写 len
    let _ = unsafe { GetLogicalProcessorInformationEx(RelationProcessorCore, None, &mut len) };
    let bytes = usize::try_from(len).ok().filter(|n| *n > 0)?;
    // 以 u64 为单位分配，保证记录头的对齐
    let mut buf = vec![0u64; bytes.div_ceil(8)];
    // SAFETY: buf 至少有 len 字节且按 8 字节对齐；函数最多写 len 字节并把实际长度写回 len
    unsafe {
        GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            Some(buf.as_mut_ptr().cast::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>()),
            &mut len,
        )
    }
    .ok()?;
    let end = usize::try_from(len).ok()?.min(bytes);
    // SAFETY: buf 有 bytes 字节，按字节读取
    let raw = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), bytes) };
    let raw = raw.get(..end)?;
    let mut classes = Vec::new();
    let mut off = 0usize;
    // 记录是变长的（比 SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX 的完整大小短）：按字段偏移逐条读——Relationship（i32，偏移 0）、
    // Size（u32，偏移 4）、PROCESSOR_RELATIONSHIP 的 EfficiencyClass（u8，偏移 9）
    while let Some(head) = raw.get(off..off + 10) {
        let relationship = i32::from_ne_bytes(head[0..4].try_into().ok()?);
        let size = usize::try_from(u32::from_ne_bytes(head[4..8].try_into().ok()?))
            .ok()
            .filter(|s| *s >= 10)?;
        if relationship == RelationProcessorCore.0 {
            classes.push(head[9]);
        }
        off += size;
    }
    let top = classes.iter().copied().max()?;
    Some(classes.iter().filter(|c| **c == top).count()).filter(|n| *n > 0)
}

/// `GetDiskFreeSpaceExW` 的 `lpFreeBytesAvailableToCaller`（考虑了磁盘配额）
pub(super) fn disk_free(path: &Path) -> Option<u64> {
    let dir = super::existing_ancestor(path)?;
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut free = 0u64;
    // SAFETY: wide 是以 NUL 结尾的 UTF-16 路径；free 是可写的 u64，其余两个输出不需要
    unsafe { GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&raw mut free), None, None) }.ok()?;
    Some(free)
}

pub(super) fn is_apple_silicon() -> bool {
    false
}
