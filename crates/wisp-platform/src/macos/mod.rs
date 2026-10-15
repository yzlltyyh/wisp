//! macOS 平台实现。
//!
//! 结构：每个 trait 一个子模块，共享状态集中在 [`Ctx`]（`Arc`，由 `create` 克隆给各 trait 对象，`Platform::shared`
//! 也持有一份供 `info` 读取）。约束：
//! - 会跨进程阻塞的调用（AX、剪贴板快照、模拟按键后的等待）都有硬超时，只在调用方的工作线程上执行；
//! - AppKit 中只能在主线程使用的部分（NSScreen、NSApp、NSEvent 监视器、TIS、浮标窗口）经 `PlatformConfig.main_thread`
//!   派发，同步等待一律带超时并有退路；
//! - 空闲时唯一的周期唤醒是剪贴板 4 Hz 的 changeCount 轮询（开启监听时才有）；外观、前台应用、输入源变化全部事件驱动；
//! - 扩展触发全关时没有 tap、没有钩子线程、没有浮标窗口。

mod apps;
mod ax;
mod clicks;
mod displays;
mod ffi;
mod focus;
mod hotkey_env;
mod keyboard;
mod notify;
mod paste;
mod pasteboard;
mod permissions;
mod poll_gate;
mod screen;
mod selection;
mod util;

#[cfg(feature = "mouse-hook")]
mod bobber;
#[cfg(feature = "mouse-hook")]
mod hooks;
#[cfg(feature = "mouse-hook")]
mod runloop;

use std::sync::Arc;

use parking_lot::RwLock;
use wisp_types::{AppKeymap, Capabilities, Capability, OsKind, PermissionState, PlatformInfo, SessionKind};

use crate::{Platform, PlatformConfig, SelfHitTest};

/// 缺少辅助功能权限（取词、模拟复制 / 粘贴）
pub(crate) const REASON_ACCESSIBILITY: &str = crate::reason::ACCESSIBILITY_MISSING;
/// 缺少屏幕录制权限（截图 OCR）
pub(crate) const REASON_SCREEN_RECORDING: &str = crate::reason::SCREEN_RECORDING_MISSING;
/// 编译时未开启 `mouse-hook` feature
#[cfg(not(feature = "mouse-hook"))]
pub(crate) const REASON_HOOKS_NOT_COMPILED: &str = crate::reason::HOOKS_NOT_COMPILED;

/// 各 trait 对象共享的状态
pub(crate) struct Ctx {
    /// Wisp 自身的进程 id
    pub(crate) own_pid: i32,
    pub(crate) main: util::MainThread,
    #[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
    pub(crate) self_hit: SelfHitTest,
    pub(crate) keymaps: Arc<RwLock<Vec<AppKeymap>>>,
    /// 前台应用记录（事件驱动）
    pub(crate) focus: focus::FocusState,
    /// 剪贴板抑制与「已见序号」
    pub(crate) clip: pasteboard::ClipState,
    /// 按当前键盘布局反查的字母键码（主线程上预建，输入源变化时重建）
    pub(crate) layout: keyboard::LayoutCache,
    /// 取词的进程内状态（Electron 已启用名单、AX 总为空的应用、回退互斥）
    pub(crate) selection: selection::SelectionState,
    /// 最近一次在主线程上取得的显示器列表（主线程忙时的退路；显示器配置变化时刷新）
    pub(crate) monitors: displays::MonitorCache,
    /// 权限请求记录（区分「未询问」与「已拒绝」）
    pub(crate) permissions: permissions::PermState,
    /// 浮标窗口与钩子共享的状态
    #[cfg(feature = "mouse-hook")]
    pub(crate) bobber: bobber::BobberShared,
}

/// 本平台的私有共享状态（`Platform::shared`）
pub(crate) struct Shared {
    /// `PlatformConfig.os_version`（desktop 传入，与 `Boot.os_version` 同源）
    os_version: String,
    ctx: Arc<Ctx>,
}

/// 构造本平台的 `Platform`（不阻塞：只建立共享状态、登记事件观察者，并向主线程异步派发显示器与键盘布局的预取）
pub(crate) fn create(cfg: PlatformConfig) -> Platform {
    let ctx = Arc::new(Ctx {
        own_pid: i32::try_from(cfg.own_pid).unwrap_or(i32::MAX),
        main: util::MainThread::new(cfg.main_thread),
        self_hit: cfg.self_hit,
        keymaps: cfg.keymaps,
        focus: focus::FocusState::default(),
        clip: pasteboard::ClipState::default(),
        layout: keyboard::LayoutCache::default(),
        selection: selection::SelectionState::default(),
        monitors: displays::MonitorCache::default(),
        permissions: permissions::PermState::default(),
        #[cfg(feature = "mouse-hook")]
        bobber: bobber::BobberShared::default(),
    });
    focus::start_tracking(&ctx);
    displays::start_tracking(&ctx);
    keyboard::start_tracking(&ctx);
    Platform {
        selection: Box::new(selection::MacSelection::new(ctx.clone())),
        paste: Box::new(paste::MacPaste::new(ctx.clone())),
        focus: Box::new(focus::MacFocus::new(ctx.clone())),
        permissions: Box::new(permissions::MacPermissions::new(ctx.clone())),
        apps: Box::new(apps::MacApps::new(ctx.clone())),
        screen: Box::new(screen::MacScreen::new(ctx.clone())),
        clipboard: Box::new(pasteboard::MacClipboard::new(ctx.clone())),
        clicks: Box::new(clicks::MacClicks::new(ctx.clone())),
        hotkey_env: Box::new(hotkey_env::MacHotkeyEnv::new(ctx.clone())),
        #[cfg(feature = "mouse-hook")]
        hooks: Box::new(hooks::MacHooks::new(ctx.clone())),
        #[cfg(feature = "mouse-hook")]
        bobber: Box::new(bobber::MacBobber::new(ctx.clone())),
        shared: Shared {
            os_version: cfg.os_version,
            ctx,
        },
    }
}

/// 退出前的收尾（`Platform::shutdown`）：macOS 的剪贴板内容由 pboard 持有，不需要交接
pub(crate) fn shutdown(_p: &Platform) {}

/// 本平台信息：只做本地的快速查询（TCC 预检），不弹窗、不阻塞
pub(crate) fn info(p: &Platform) -> PlatformInfo {
    let perms = permissions::current(&p.shared.ctx.permissions);
    let accessibility = if perms.accessibility == PermissionState::Granted {
        Capability::yes()
    } else {
        Capability::no(REASON_ACCESSIBILITY)
    };
    let capture = if perms.screen_recording == PermissionState::Granted {
        Capability::yes()
    } else {
        Capability::no(REASON_SCREEN_RECORDING)
    };
    #[cfg(feature = "mouse-hook")]
    let input_hooks = Capability::yes();
    #[cfg(not(feature = "mouse-hook"))]
    let input_hooks = Capability::no(REASON_HOOKS_NOT_COMPILED);
    PlatformInfo {
        os: OsKind::Macos,
        os_version: p.shared.os_version.clone(),
        arch: std::env::consts::ARCH.to_owned(),
        session: SessionKind::Native,
        capabilities: Capabilities {
            // 全局快捷键由 desktop 的插件负责（Carbon 热键不需要权限）
            global_hotkeys: Capability::yes(),
            selection: accessibility.clone(),
            simulated_copy: accessibility.clone(),
            paste_inject: accessibility,
            capture_overlay: capture,
            // changeCount 轮询不需要任何权限
            clipboard_watch: Capability::yes(),
            // 各项扩展触发所需的权限在 install 时逐项检查
            input_hooks,
        },
    }
}
