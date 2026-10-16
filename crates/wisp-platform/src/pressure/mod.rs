//! 系统内存压力：通知（事件驱动，没有轮询）与即时查询。
//!
//! 通知只在至少一个离线模型加载着时安装（desktop 的模型服务负责），全部释放后丢弃 [`Watch`] 即卸载；平时不存在任何监听、
//! 线程或定时器。回调在平台的通知线程上执行，只做 O(1) 的工作（例如把等级转发进通道）。
//!
//! [`current`] 是一次性的查询（一次系统调用或读一个 procfs 文件，不建监听、不缓存），给加载模型前的内存检查用：
//! 「可用内存」在 macOS 上低估了能腾出来的量（压缩器还能再压缩），系统自己的压力等级更能说明此刻加载一个大模型会不会让
//! 整机卡顿。
//!
//! - macOS：`DISPATCH_SOURCE_TYPE_MEMORYPRESSURE`（warn / critical）；
//! - Windows：`CreateMemoryResourceNotification(LowMemoryResourceNotification)` + `RegisterWaitForSingleObject`
//!   （只有「低内存」一档，报告为 [`Level::Warning`]）；
//! - Linux：PSI 触发器（`/proc/pressure/memory`，窗口取 2 s 的整数倍），由一个阻塞在 poll 上的线程等待；
//!   刚安装时内核可能立即报告一次，按 `total` 的增量过滤掉。没有 PSI（旧内核、部分容器）时不支持。

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

/// 内存压力等级
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Level {
    /// 压力解除
    Normal,
    /// 内存紧张：释放空闲的模型
    Warning,
    /// 内存严重不足：释放空闲的模型，正在运行的请求结束后立即释放
    Critical,
}

/// 已安装的监听；丢弃即卸载（之后不再回调）
pub struct Watch {
    _imp: imp::Watch,
}

impl std::fmt::Debug for Watch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("pressure::Watch")
    }
}

/// 安装内存压力监听；本系统不支持或安装失败时返回 None（调用方只是失去这一层保护，闲置释放照常工作）
pub fn watch(cb: impl Fn(Level) + Send + Sync + 'static) -> Option<Watch> {
    imp::watch(Box::new(cb)).map(|w| Watch { _imp: w })
}

/// 此刻的内存压力等级（即时查询，不安装监听、不起线程）。本系统不支持或读取失败时为 None（调用方按「未知」处理，
/// 不要当作 Normal 以外的结论）。
///
/// - macOS：`sysctl kern.memorystatus_vm_pressure_level`（1 = 正常、2 = 紧张、4 = 严重，与活动监视器「内存压力」同源）；
/// - Windows：`QueryMemoryResourceNotification` 查询低内存通知对象的当前状态（只有「低内存」一档，报告为 [`Level::Warning`]）；
/// - Linux：`/proc/pressure/memory` 最近 10 s 的平均停顿比例（`full` ≥ 5% 为严重、`some` ≥ 10% 为紧张，与通知的阈值一致）；
///   没有 PSI 时为 None。
pub fn current() -> Option<Level> {
    imp::current()
}

/// 平台实现收到的回调
pub(crate) type Callback = Box<dyn Fn(Level) + Send + Sync + 'static>;
