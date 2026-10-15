//! Windows 平台实现。
//!
//! 线程模型（空闲时零唤醒）：
//! - `wisp-foreground`：`SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` 前台追踪，构造时启动，只在前台切换时被唤醒；
//! - `wisp-clipboard`：message-only 窗口，既是写剪贴板的所有者窗口，也是 `AddClipboardFormatListener` 监听器；
//!   首次写入或开始监听时创建；
//! - `wisp-uia`：UI Automation 专用 MTA 线程，首次取词时创建；
//! - `wisp-hooks` + `wisp-hook-dispatch`：低级鼠标 / 键盘钩子与事件分发，只在有订阅（外部点击、扩展触发）
//!   时存在，订阅全部注销后退出；被系统因回调超时静默卸载后，由重配 / 唤醒 / 解锁 / 前台切换时的检查自动重装；
//! - `wisp-appearance`：外观变化监听（隐藏顶层窗口收 `WM_SETTINGCHANGE` 等广播），有订阅时存在；
//! - `wisp-bobber`：浮标分层窗口，首次显示时创建，浮标功能关闭后销毁。
//!
//! 所有可能被其他进程卡住的调用都有硬超时：UIA 100 ms、剪贴板读写在临时线程上执行并限时、
//! `OpenClipboard` 按各操作的预算每 10 ms 重试、前台切换最多等 100 ms；对其他进程窗口的操作一律异步
//! （`ShowWindowAsync` / `SWP_ASYNCWINDOWPOS`），目标无响应时直接放弃。上层回调一律在 `catch_unwind` 里执行。

mod appearance;
#[cfg(feature = "mouse-hook")]
mod bobber;
#[cfg(feature = "mouse-hook")]
mod bobber_art;
mod clip_format;
mod clip_watch;
mod clipboard;
mod env;
mod focus;
mod geom;
mod hooks;
mod icon;
mod input;
mod keys;
mod monitor;
mod paste;
mod process;
mod selection;
mod suppress;
mod uia;
mod util;
mod wnd;

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
#[cfg(feature = "mouse-hook")]
use wisp_types::Rect;
use wisp_types::{
    AppKeymap, AppRef, Capabilities, Capability, MonitorId, MonitorInfo, OsKind, PermissionKind, PermissionSet,
    PlatformInfo, Point, RawClipboard, SessionKind,
};

use crate::error::PlatformError;
use crate::traits::*;
use crate::types::{ClickEvent, ClipSnapshot, HookHandle, LayoutInfo, ReadLimits, SuppressRange};
#[cfg(feature = "mouse-hook")]
use crate::types::{HookError, HookSpec, InputSignal};
use crate::{Platform, PlatformConfig};

use clipboard::{SnapshotOutcome, WinSnapshot};
use util::run_with_timeout;

/// 读剪贴板的硬超时（延迟渲染会同步等待原所有者）
const READ_TIMEOUT: Duration = Duration::from_millis(1500);
/// 恢复快照的硬超时
const RESTORE_TIMEOUT: Duration = Duration::from_millis(1000);

/// 各 trait 对象共享的状态
pub(crate) struct Core {
    own_pid: u32,
    keymaps: Arc<RwLock<Vec<AppKeymap>>>,
    names: Arc<process::NameCache>,
    focus: Arc<focus::Focus>,
    uia: Arc<uia::Uia>,
    clip: Arc<clip_watch::ClipHub>,
    hooks: Arc<hooks::HookHub>,
    icons: icon::IconCache,
    #[cfg(feature = "mouse-hook")]
    bobber: Arc<bobber::Bobber>,
}

/// 本平台的私有共享状态（`Platform::shared`）
pub(crate) struct Shared {
    /// `PlatformConfig.os_version`（desktop 传入，与 `Boot.os_version` 同源）
    os_version: String,
}

/// 构造本平台的 `Platform`（不阻塞：只启动前台追踪线程，其余一律惰性创建）
pub(crate) fn create(cfg: PlatformConfig) -> Platform {
    let names = Arc::new(process::NameCache::new());
    let focus = focus::Focus::new(cfg.own_pid, Arc::clone(&names));
    let uia = Arc::new(uia::Uia::new());
    let hooks = hooks::HookHub::new(Arc::clone(&uia), cfg.self_hit);
    // 前台切换 → 低级钩子存活检查（自愈）、浮标消失
    {
        let weak = Arc::downgrade(&hooks);
        focus.set_listener(Some(Arc::new(move || {
            if let Some(h) = weak.upgrade() {
                h.app_switched();
            }
        })));
    }
    #[cfg(feature = "mouse-hook")]
    let bobber = {
        let b = bobber::Bobber::new(Arc::clone(&hooks));
        hooks.bind_bobber(Arc::downgrade(&b));
        b
    };
    let core = Arc::new(Core {
        own_pid: cfg.own_pid,
        keymaps: cfg.keymaps,
        names,
        focus,
        uia,
        clip: clip_watch::ClipHub::new(),
        hooks,
        icons: icon::IconCache::new(),
        #[cfg(feature = "mouse-hook")]
        bobber,
    });
    core.focus.start();
    // Windows 实现的原生窗口都在各自的专用线程上，不需要派发到 GUI 主线程
    drop(cfg.main_thread);

    Platform {
        selection: Box::new(selection::WinSelection(Arc::clone(&core))),
        paste: Box::new(paste::WinPaste(Arc::clone(&core))),
        focus: Box::new(WinFocus(Arc::clone(&core))),
        permissions: Box::new(WinPermissions),
        apps: Box::new(WinApps(Arc::clone(&core))),
        screen: Box::new(WinScreen),
        clipboard: Box::new(WinClipboard(Arc::clone(&core))),
        clicks: Box::new(WinClicks(Arc::clone(&core))),
        hotkey_env: Box::new(WinHotkeyEnv),
        #[cfg(feature = "mouse-hook")]
        hooks: Box::new(WinHooks(Arc::clone(&core))),
        #[cfg(feature = "mouse-hook")]
        bobber: Box::new(WinBobber(core)),
        shared: Shared {
            os_version: cfg.os_version,
        },
    }
}

/// 退出前的收尾（`Platform::shutdown`）：Windows 的剪贴板内容在写入时就已交给系统，不需要交接
pub(crate) fn shutdown(_p: &Platform) {}

/// 本平台信息：Windows 上各能力都不依赖授权；不阻塞
pub(crate) fn info(p: &Platform) -> PlatformInfo {
    let input_hooks = if cfg!(feature = "mouse-hook") {
        Capability::yes()
    } else {
        // 与 macOS 同一个 i18n key（只在 --no-default-features 构建中出现）
        Capability::no(crate::reason::HOOKS_NOT_COMPILED)
    };
    PlatformInfo {
        os: OsKind::Windows,
        os_version: p.shared.os_version.clone(),
        arch: std::env::consts::ARCH.to_owned(),
        session: SessionKind::Native,
        capabilities: Capabilities {
            // 全局快捷键由 desktop 的插件负责，平台层不影响
            global_hotkeys: Capability::yes(),
            selection: Capability::yes(),
            simulated_copy: Capability::yes(),
            paste_inject: Capability::yes(),
            capture_overlay: Capability::yes(),
            clipboard_watch: Capability::yes(),
            input_hooks,
        },
    }
}

// ---------------------------------------------------------------- FocusTracker

struct WinFocus(Arc<Core>);

impl FocusTracker for WinFocus {
    fn frontmost(&self) -> Option<AppRef> {
        self.0.focus.frontmost()
    }

    fn activate(&self, app: &AppRef) -> Result<(), PlatformError> {
        self.0.focus.activate(app)
    }

    fn is_elevated(&self, app: &AppRef) -> bool {
        self.0.focus.is_elevated(app)
    }
}

// ---------------------------------------------------------------- Permissions

/// Windows 没有与 macOS TCC 对应的权限模型：全部 not_applicable，申请是空操作
struct WinPermissions;

impl Permissions for WinPermissions {
    fn status(&self) -> PermissionSet {
        PermissionSet::not_applicable()
    }

    fn request(&self, _kind: PermissionKind) -> Result<(), PlatformError> {
        Ok(())
    }

    fn open_settings(&self, _kind: PermissionKind) -> Result<(), PlatformError> {
        // 与 Linux 同一个 i18n key：没有对应的系统设置页，能力本来就可用
        Err(PlatformError::Unsupported(
            crate::reason::NO_PERMISSION_MODEL.to_owned(),
        ))
    }
}

// ---------------------------------------------------------------- AppInfo

struct WinApps(Arc<Core>);

impl AppInfo for WinApps {
    fn icon_png(&self, app: &AppRef, size_px: u32) -> Option<Vec<u8>> {
        let path = app.path.clone().or_else(|| app.pid.and_then(process::exe_path))?;
        self.0.icons.png(&path, size_px)
    }

    fn display_name(&self, app: &AppRef) -> String {
        if let Some(path) = app.path.as_deref() {
            return self.0.names.display_name(path);
        }
        if !app.name.is_empty() {
            return app.name.clone();
        }
        process::stem(&app.id).to_owned()
    }

    fn running_apps(&self) -> Vec<AppRef> {
        process::running_apps()
    }

    fn user_apps(&self) -> Vec<AppRef> {
        process::user_apps(&self.0.names, self.0.own_pid)
    }
}

// ---------------------------------------------------------------- CursorScreen

struct WinScreen;

impl CursorScreen for WinScreen {
    fn monitors(&self) -> Vec<MonitorInfo> {
        monitor::monitors()
    }

    fn cursor(&self) -> (MonitorId, Point) {
        monitor::cursor()
    }

    fn system_accent(&self) -> Option<String> {
        appearance::accent()
    }

    fn reduce_motion(&self) -> bool {
        appearance::reduce_motion()
    }

    fn reduce_transparency(&self) -> bool {
        appearance::reduce_transparency()
    }

    fn dark_mode(&self) -> bool {
        appearance::dark_mode()
    }

    fn taskbar_light(&self) -> Option<bool> {
        Some(appearance::taskbar_light())
    }

    fn watch_appearance(&self, cb: Box<dyn Fn() + Send + Sync>) -> Result<HookHandle, PlatformError> {
        appearance::watch(cb)
    }
}

// ---------------------------------------------------------------- ClipboardAccess

struct WinClipboard(Arc<Core>);

impl ClipboardAccess for WinClipboard {
    fn change_id(&self) -> u64 {
        clipboard::seq()
    }

    fn read(&self, limits: &ReadLimits) -> Result<RawClipboard, PlatformError> {
        let limits = *limits;
        // 读取引起的延迟渲染登记到剪贴板线程：监听回调不会因这次读取再触发一次
        let hub = Arc::clone(&self.0.clip);
        let out = run_with_timeout("wisp-clip-read", READ_TIMEOUT, move || {
            clipboard::read(limits, Some(&hub))
        })
        .ok_or(PlatformError::Timeout)??;
        let mut raw = out.raw;
        raw.source = self.source_of(out.owner);
        Ok(raw)
    }

    fn snapshot(&self, max_bytes: usize, deadline: Duration) -> Result<Option<ClipSnapshot>, PlatformError> {
        let until = Instant::now() + deadline;
        let hub = Arc::clone(&self.0.clip);
        let r = run_with_timeout("wisp-clip-snapshot", deadline + Duration::from_millis(30), move || {
            clipboard::snapshot(max_bytes, until, Some(&hub))
        });
        match r {
            Some(Ok(SnapshotOutcome::Taken(s, bytes))) => Ok(Some(ClipSnapshot::new(s, bytes))),
            Some(Ok(SnapshotOutcome::Abandoned(why))) => {
                log::info!("clipboard snapshot abandoned: {why}");
                Ok(None)
            }
            Some(Err(e)) => Err(e),
            None => {
                log::info!("clipboard snapshot abandoned: timeout");
                Ok(None)
            }
        }
    }

    fn restore(&self, snap: ClipSnapshot) -> Result<u64, PlatformError> {
        let snap: WinSnapshot = snap
            .into_inner()
            .ok_or_else(|| PlatformError::Other("snapshot was not taken on Windows".into()))?;
        let owner = self.0.clip.owner();
        run_with_timeout("wisp-clip-restore", RESTORE_TIMEOUT, move || {
            clipboard::restore(snap, owner, clipboard::OPEN_BUDGET)
        })
        .unwrap_or(Err(PlatformError::Timeout))
    }

    fn watch(&self, cb: Box<dyn Fn(u64) + Send + Sync>) -> Result<HookHandle, PlatformError> {
        self.0.clip.watch(cb)
    }

    fn suppress(&self, range: SuppressRange) {
        // ttl 可以是任意值（含 Duration::MAX 表示「一直抑制」）：区间按登记时刻 + ttl 判断过期，不会溢出；区间数量
        // 有上限，超出时丢弃最早的（`suppress::MAX_ENTRIES`）
        self.0
            .clip
            .suppress_open(range.from_change_id.saturating_sub(1), range.ttl);
    }
}

impl WinClipboard {
    /// 来源应用：所有者窗口 → 进程；所有者是 Wisp 自己时为 None；没有所有者时用前台应用
    fn source_of(&self, owner: Option<util::RawHwnd>) -> Option<AppRef> {
        match owner.filter(|h| !h.is_null()) {
            Some(h) => {
                let (_, pid) = process::window_thread_process(h.hwnd());
                if pid == 0 || pid == self.0.own_pid {
                    return None;
                }
                self.0.names.app_for_pid(pid, None)
            }
            None => self.0.focus.frontmost(),
        }
    }
}

// ---------------------------------------------------------------- OutsideClick

struct WinClicks(Arc<Core>);

impl OutsideClick for WinClicks {
    fn watch(&self, cb: Box<dyn Fn(ClickEvent) + Send + Sync>) -> Result<HookHandle, PlatformError> {
        self.0.hooks.watch_clicks(cb)
    }
}

// ---------------------------------------------------------------- HotkeyEnv

struct WinHotkeyEnv;

impl HotkeyEnv for WinHotkeyEnv {
    fn system_hotkeys(&self) -> Vec<String> {
        // Windows 的系统热键冲突由 RegisterHotKey 的返回值直接给出（ERROR_HOTKEY_ALREADY_REGISTERED）
        Vec::new()
    }

    fn keyboard_layouts(&self) -> LayoutInfo {
        env::keyboard_layouts()
    }
}

// ---------------------------------------------------------------- InputHooks / BobberWindow

#[cfg(feature = "mouse-hook")]
struct WinHooks(Arc<Core>);

#[cfg(feature = "mouse-hook")]
impl InputHooks for WinHooks {
    fn install(&self, spec: HookSpec, sink: Box<dyn Fn(InputSignal) + Send + Sync>) -> Result<HookHandle, HookError> {
        self.0.hooks.install(spec, sink)
    }
}

#[cfg(feature = "mouse-hook")]
struct WinBobber(Arc<Core>);

#[cfg(feature = "mouse-hook")]
impl BobberWindow for WinBobber {
    fn show(&self, at: (MonitorId, Point), dark: bool, reduced_motion: bool, on_click: Box<dyn Fn() + Send + Sync>) {
        self.0.bobber.show(at, dark, reduced_motion, on_click);
    }

    fn hide(&self, animated: bool) {
        self.0.bobber.hide_now(animated);
    }

    fn rect(&self) -> Option<(MonitorId, Rect)> {
        self.0.bobber.rect()
    }
}
