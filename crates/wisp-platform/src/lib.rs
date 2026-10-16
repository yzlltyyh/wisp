//! Wisp 的操作系统集成层。
//!
//! 把所有操作系统差异封装成 trait，上层只通过 [`Platform`] 门面调用。约束：
//! - 所有可能阻塞的调用（AX、UIA、X11 selection、剪贴板）都必须有硬超时；
//! - 钩子回调只做 O(1) 的工作，然后通过 channel 转交给工作线程；
//! - 各实现内部惰性初始化（UIA 对象、X11 连接等），`Platform::create` 本身不阻塞。
//!
//! 平台实现分别位于 `macos/`、`windows/`、`linux/`；`common/` 为三端共享的
//! 纯逻辑（可在任何平台上单测）。在本 crate 中引用 `windows` crate 时一律写 `::windows::…`，
//! 以免与本地的 `windows` 模块冲突。

use std::sync::Arc;

use wisp_types::{MonitorId, Point};

pub mod child;
pub mod common;
pub mod error;
pub mod pressure;
pub mod process;
pub mod reason;
pub mod system;
pub mod traits;
pub mod types;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
use crate::linux as imp;
#[cfg(target_os = "macos")]
use crate::macos as imp;
#[cfg(target_os = "windows")]
use crate::windows as imp;

pub use error::*;
pub use traits::*;
pub use types::*;
pub use wisp_types::{AppKeymap, PlatformInfo};

/// 把闭包派发到 GUI 主线程的函数
pub type MainThreadDispatch = Arc<dyn Fn(Box<dyn FnOnce() + Send>) + Send + Sync>;

/// 判断某个点是否落在 Wisp 自己的窗口内
pub type SelfHitTest = Arc<dyn Fn(&MonitorId, Point) -> bool + Send + Sync>;

/// 构造 [`Platform`] 所需的配置
pub struct PlatformConfig {
    /// Wisp 自身的进程 id（用于排除自己的窗口 / 剪贴板写入）
    pub own_pid: u32,
    /// 把闭包派发到 GUI 主线程（desktop 传入 app.run_on_main_thread）
    pub main_thread: MainThreadDispatch,
    /// 判断某个点是否落在 Wisp 自己的窗口内（浮标判定用；由 WindowManager 维护矩形表）。
    /// Platform 先于 WindowManager 构造，desktop 传入的闭包读取 `Arc<OnceLock<Weak<WindowManager>>>`，
    /// WindowManager 构造后再绑定；未绑定时返回 false
    pub self_hit: SelfHitTest,
    /// 用户条目在前，内置默认表在后
    pub keymaps: Arc<parking_lot::RwLock<Vec<AppKeymap>>>,
    /// 操作系统版本，原样作为 `PlatformInfo.os_version`。唯一来源是 desktop 的 `app::runtime::os_version()`
    /// （与 `Boot.os_version` 同源），平台层不自行探测
    pub os_version: String,
}

/// 平台门面：上层只通过它访问操作系统能力
pub struct Platform {
    pub selection: Box<dyn SelectedText>,
    pub paste: Box<dyn Paste>,
    pub focus: Box<dyn FocusTracker>,
    pub permissions: Box<dyn Permissions>,
    pub apps: Box<dyn AppInfo>,
    pub screen: Box<dyn CursorScreen>,
    pub clipboard: Box<dyn ClipboardAccess>,
    pub clicks: Box<dyn OutsideClick>,
    pub hotkey_env: Box<dyn HotkeyEnv>,
    #[cfg(feature = "mouse-hook")]
    pub hooks: Box<dyn InputHooks>,
    #[cfg(feature = "mouse-hook")]
    pub bobber: Box<dyn BobberWindow>,
    /// 各平台实现的私有共享状态（`imp::Shared`，由各平台实现各自定义字段）：惰性建立的连接（X11 / UIA）、
    /// 能力探测结果（XTest / XInput 扩展是否存在、Wayland 降级原因、钩子是否已获授权）等。`create` 构造它，并把
    /// 其中的 `Arc` 克隆给需要的 trait 对象；[`Platform::info`] 经 `imp::info(self)` 读取它，因此能力的可用性
    /// 可以反映运行期状态，不必借助全局静态量
    pub(crate) shared: imp::Shared,
}

impl Platform {
    /// 构造当前平台的实现（各实现内部惰性初始化，本函数不阻塞）
    pub fn create(cfg: PlatformConfig) -> Arc<Self> {
        Arc::new(imp::create(cfg))
    }

    /// OS、会话类型、各能力是否可用。实现可以读取 `self.shared` 里的探测结果，但**不得阻塞**：
    /// 还没探测过的能力按「尚未初始化时的最佳判断」报告，不在这里建立连接或弹授权
    pub fn info(&self) -> PlatformInfo {
        imp::info(self)
    }

    /// 退出前的收尾，幂等。Linux：Wisp 仍持有 CLIPBOARD 且有剪贴板管理器时按 freedesktop 规范
    /// 请求 `SAVE_TARGETS`，把内容交给管理器保存（最多阻塞 2 s，只在这两个条件都满足时等待，通常几十毫秒）——否则
    /// 模拟复制恢复后的内容、写入的译文会随进程退出而消失；macOS / Windows 为空操作（剪贴板内容由系统持有）。
    /// desktop 在 `RunEvent::Exit`（托盘「退出」、自动更新重启、SIGTERM）里调用；`Platform` 被丢弃时也会执行同样的事
    pub fn shutdown(&self) {
        imp::shutdown(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_reads_instance_state() {
        let platform = Platform::create(PlatformConfig {
            own_pid: std::process::id(),
            main_thread: Arc::new(|f: Box<dyn FnOnce() + Send>| f()),
            self_hit: Arc::new(|_, _| false),
            keymaps: Arc::new(parking_lot::RwLock::new(Vec::new())),
            os_version: "26.4".to_owned(),
        });
        let info = platform.info();
        // os_version 来自构造参数（与 Boot.os_version 同源），不是平台层自行探测
        assert_eq!(info.os_version, "26.4");
        assert_eq!(info.arch, std::env::consts::ARCH);
    }
}
