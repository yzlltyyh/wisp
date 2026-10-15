//! Linux 平台实现：X11 完整支持，Wayland 按会话能力降级（见 `session`）。
//!
//! 结构：
//! - `x11/`：唯一一条 x11rb 连接 + 唯一的事件线程（PRIMARY / CLIPBOARD 读写、XFixes 监听、XTest、XInput2、
//!   RandR、XSETTINGS、浮标）；第一次用到时才连接，X11 会话下在 `create` 时于后台线程预热；
//! - `appearance`：xdg-desktop-portal Settings（自带的最小 D-Bus 客户端，`dbus` / `dbus_wire`）+ XSETTINGS；
//! - `apps`：`.desktop` 索引、图标主题与窗口图标；
//! - 纯逻辑模块（`session`、`xsettings`、`appearance_logic`、`dbus_wire`、`desktop_entry`、`edid`、`geometry`、
//!   `keysym`、`terminal`、`uri`、`bobber_art`、`xfer_logic`、`primary_gate`、`hook_logic`、`fallback_logic`）只依赖
//!   std / wisp-types / `common/`，`tests/smoke_linux.rs` 用 `#[path]` 引入它们，因此在 macOS 开发机上也能跑单测。
//!
//! Wayland 会话：取词、模拟复制 / 粘贴、剪贴板监听、扩展触发、外部点击、截图都返回带原因的
//! `Unsupported`（原因是前端 i18n key，见 `session::reason`）；为了「只复制」，写剪贴板在有 XWayland
//! 时经 XWayland 完成（Mutter / wlroots 把 X11 剪贴板同步给 Wayland 应用）。KWin 只在 XWayland 窗口获焦时才接受
//! X11 剪贴板，而 Wisp 的面板是原生 Wayland 窗口——KDE Wayland 下 `write` 返回带原因的 Unsupported，由上层改用
//! 面板（WebView）在获焦时写剪贴板，绝不在写入不会生效时返回 Ok。每个功能走不走 X 连接、不走时报告哪个原因，
//! 由纯函数 `session::route` / `session::connect_failure` 决定（有表驱动单测）。
//!
//! 退出：`Shared` 被丢弃时，Wisp 若仍持有剪贴板（模拟复制恢复的用户内容、写入的译文），交给剪贴板管理器保存。

mod appearance;
mod appearance_logic;
mod apps;
#[cfg(feature = "mouse-hook")]
mod bobber_art;
mod dbus;
mod dbus_wire;
mod desktop_entry;
mod edid;
mod fallback_logic;
mod geometry;
#[cfg(feature = "mouse-hook")]
mod hook_logic;
mod hotkey_logic;
mod keysym;
mod primary_gate;
mod selection;
mod session;
mod terminal;
mod uri;
mod x11;
mod xfer_logic;
mod xsettings;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::RwLock;
#[cfg(feature = "mouse-hook")]
use wisp_types::Rect;
use wisp_types::{
    AppKeymap, AppRef, Capabilities, Capability, ClipPayload, MonitorId, MonitorInfo, OsKind, PermissionKind,
    PermissionSet, PlatformInfo, Point, RawClipboard, SessionKind,
};

use self::appearance::Appearance;
use self::apps::Apps;
use self::session::{DisplayServer, Feature, Session, reason};
use self::x11::{LazyX11, X11};
use crate::error::PlatformError;
use crate::traits::*;
use crate::types::{
    ClickEvent, ClipSnapshot, HookHandle, LayoutInfo, ReadLimits, Selection, SelectionError, SuppressRange,
};
#[cfg(feature = "mouse-hook")]
use crate::types::{HookError, HookSpec, InputSignal};
use crate::{Platform, PlatformConfig};

/// 退出时等待剪贴板管理器保存的上限（只在 Wisp 持有剪贴板且有管理器时等待，通常几十毫秒）
const MANAGER_HANDOFF: Duration = Duration::from_secs(2);

/// 本平台的私有共享状态（`Platform::shared`）
pub(crate) struct Shared {
    /// `PlatformConfig.os_version`（desktop 传入，与 `Boot.os_version` 同源）
    os_version: String,
    ctx: Arc<Ctx>,
    shut_down: AtomicBool,
}

impl Shared {
    /// 退出前的收尾（幂等）：Wisp 仍持有 CLIPBOARD 时请剪贴板管理器保存（`SAVE_TARGETS`），否则用户的剪贴板会
    /// 随 Wisp 退出而丢失。由 `Platform::shutdown`（desktop 在 `RunEvent::Exit` 里调用）与 `Drop` 调用
    pub(crate) fn shutdown(&self) {
        if self.shut_down.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(x) = self.ctx.x11.peek() {
            x11::clipboard::hand_off_to_manager(&x, MANAGER_HANDOFF);
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// 各 trait 实现共享的上下文
struct Ctx {
    session: Session,
    own_pid: u32,
    x11: Arc<LazyX11>,
    keymaps: Arc<RwLock<Vec<AppKeymap>>>,
    appearance: Arc<Appearance>,
    apps: Apps,
}

impl Ctx {
    /// 某功能要用的 X 连接（必要时连接 / 重连）。这个会话下不走 X、或连接失败时返回 `errors.platform.*` 原因
    /// （`session::route` / `session::connect_failure`）
    fn x11_for(&self, feature: Feature) -> Result<Arc<X11>, &'static str> {
        session::route(&self.session, feature)?;
        self.x11
            .get()
            .map_err(|_| session::connect_failure(&self.session, feature))
    }

    /// 只在 X11 会话、且已经连接时返回（不阻塞）
    fn x11_ready(&self) -> Option<Arc<X11>> {
        (self.session.server == DisplayServer::X11)
            .then(|| self.x11.peek())
            .flatten()
    }
}

fn unsupported(reason: &'static str) -> PlatformError {
    PlatformError::Unsupported(reason.to_owned())
}

/// 构造本平台的 `Platform`（不阻塞：X11 连接与 portal 读取都在后台线程进行）
pub(crate) fn create(cfg: PlatformConfig) -> Platform {
    let session = Session::detect();
    log::info!("Linux 会话：{}", session.describe());
    let x11 = Arc::new(LazyX11::new(
        cfg.own_pid,
        session.has_x_display,
        Some(cfg.self_hit.clone()),
    ));
    if session.server == DisplayServer::X11 {
        x11.warm_up();
    }
    let appearance = Appearance::new(Arc::clone(&x11), session.server == DisplayServer::X11);
    appearance.start();
    let ctx = Arc::new(Ctx {
        session,
        own_pid: cfg.own_pid,
        x11,
        keymaps: cfg.keymaps,
        appearance,
        apps: Apps::new(),
    });
    let imp = || Box::new(Imp(Arc::clone(&ctx)));
    Platform {
        selection: imp(),
        paste: imp(),
        focus: imp(),
        permissions: imp(),
        apps: imp(),
        screen: imp(),
        clipboard: imp(),
        clicks: imp(),
        hotkey_env: imp(),
        #[cfg(feature = "mouse-hook")]
        hooks: imp(),
        #[cfg(feature = "mouse-hook")]
        bobber: imp(),
        shared: Shared {
            os_version: cfg.os_version,
            ctx: Arc::clone(&ctx),
            shut_down: AtomicBool::new(false),
        },
    }
}

/// 退出前的收尾（`Platform::shutdown`）：仍持有剪贴板时交给剪贴板管理器（SAVE_TARGETS），幂等
pub(crate) fn shutdown(p: &Platform) {
    p.shared.shutdown();
}

/// 本平台信息：只读已有的探测结果，不建立连接、不阻塞
pub(crate) fn info(p: &Platform) -> PlatformInfo {
    let ctx = &p.shared.ctx;
    let (session, capabilities) = match ctx.session.server {
        DisplayServer::Wayland => (
            SessionKind::Wayland,
            Capabilities {
                global_hotkeys: Capability::no(reason::WAYLAND_HOTKEYS),
                selection: Capability::no(reason::WAYLAND_SELECTION),
                simulated_copy: Capability::no(reason::WAYLAND_SIMULATED_COPY),
                paste_inject: Capability::no(reason::WAYLAND_PASTE),
                capture_overlay: Capability::no(reason::WAYLAND_CAPTURE),
                clipboard_watch: Capability::no(reason::WAYLAND_CLIPBOARD_WATCH),
                input_hooks: Capability::no(reason::WAYLAND_INPUT_HOOKS),
            },
        ),
        DisplayServer::Headless => {
            let no = || Capability::no(reason::NO_DISPLAY);
            (
                SessionKind::X11,
                Capabilities {
                    global_hotkeys: no(),
                    selection: no(),
                    simulated_copy: no(),
                    paste_inject: no(),
                    capture_overlay: no(),
                    clipboard_watch: no(),
                    input_hooks: no(),
                },
            )
        }
        DisplayServer::X11 => (SessionKind::X11, x11_capabilities(ctx)),
    };
    PlatformInfo {
        os: OsKind::Linux,
        os_version: p.shared.os_version.clone(),
        arch: std::env::consts::ARCH.to_owned(),
        session,
        capabilities,
    }
}

fn x11_capabilities(ctx: &Ctx) -> Capabilities {
    if ctx.x11.failed() {
        let no = || Capability::no(reason::X11_CONNECT_FAILED);
        return Capabilities {
            global_hotkeys: Capability::yes(),
            selection: no(),
            simulated_copy: no(),
            paste_inject: no(),
            capture_overlay: no(),
            clipboard_watch: no(),
            input_hooks: no(),
        };
    }
    // 还没连接时按「X11 全部可用」报告，连接后按扩展探测结果修正
    let ext = ctx.x11.peek().map(|x| x.ext);
    let has = |f: fn(&x11::Extensions) -> bool| ext.as_ref().is_none_or(f);
    let cap = |ok: bool, why: &str| if ok { Capability::yes() } else { Capability::no(why) };
    let xtest = has(|e| e.xtest);
    let xfixes = has(|e| e.xfixes);
    let xinput = has(|e| e.xinput.is_some());
    Capabilities {
        // 全局快捷键：平台层的事件驱动 XGrabKey（`HotkeyEnv::key_grab`）
        global_hotkeys: Capability::yes(),
        selection: Capability::yes(),
        simulated_copy: if !xtest {
            Capability::no(reason::X11_MISSING_XTEST)
        } else {
            cap(xfixes, reason::X11_MISSING_XFIXES)
        },
        paste_inject: cap(xtest, reason::X11_MISSING_XTEST),
        // 截图由 wisp-capture 的 x11rb 后端负责（RandR + MIT-SHM / GetImage）
        capture_overlay: Capability::yes(),
        clipboard_watch: cap(xfixes, reason::X11_MISSING_XFIXES),
        input_hooks: cap(xinput, reason::X11_MISSING_XINPUT),
    }
}

/// 全部 trait 的实现（每个字段持有同一个上下文的克隆）
struct Imp(Arc<Ctx>);

impl SelectedText for Imp {
    fn get(&self, opts: &SelectionOpts) -> Result<Selection, SelectionError> {
        let x = self
            .0
            .x11_for(Feature::Selection)
            .map_err(|r| SelectionError::Unsupported(r.to_owned()))?;
        selection::get(&x, opts, &self.0.keymaps)
    }

    fn has_selection(&self, deadline: Duration) -> Option<bool> {
        let x = self.0.x11_for(Feature::Selection).ok()?;
        selection::has_selection(&x, deadline)
    }

    fn link_at_cursor(&self, _deadline: Duration) -> Option<bool> {
        // X11 拿不到光标下的语义信息（约定：X11 恒为 Some(false)）
        (self.0.session.server == DisplayServer::X11).then_some(false)
    }
}

impl Paste for Imp {
    fn write(&self, payload: &ClipPayload, opts: WriteOpts) -> Result<u64, PlatformError> {
        // Wayland：经 XWayland 写入（只复制，不模拟粘贴）；没有 XWayland、或合成器不会把它同步给 Wayland 应用时
        // 返回带原因的 Unsupported（上层改由面板在获焦时写剪贴板）
        let x = self.0.x11_for(Feature::ClipboardWrite).map_err(unsupported)?;
        x11::clipboard::write(&x, payload, opts.exclude_from_history || opts.transient).map_err(PlatformError::Other)
    }

    fn paste_into(&self, target: &AppRef, _plain: bool) -> Result<(), PasteError> {
        // plain 只影响写入的内容（调用方已按 plain 写好剪贴板），X11 的粘贴键与之无关
        let x = self
            .0
            .x11_for(Feature::Paste)
            .map_err(|r| PasteError::Unsupported(r.to_owned()))?;
        selection::paste_into(&x, target, &self.0.keymaps)
    }
}

impl FocusTracker for Imp {
    fn frontmost(&self) -> Option<AppRef> {
        let x = self.0.x11_for(Feature::Selection).ok()?;
        x11::focus::frontmost(&x)
    }

    fn activate(&self, app: &AppRef) -> Result<(), PlatformError> {
        let x = self.0.x11_for(Feature::Focus).map_err(unsupported)?;
        let w =
            x11::focus::resolve_window(&x, app).ok_or_else(|| PlatformError::Other("target window is gone".into()))?;
        x11::focus::request_activate(&x, w).map_err(PlatformError::Other)
    }

    fn is_elevated(&self, _app: &AppRef) -> bool {
        false
    }
}

impl Permissions for Imp {
    fn status(&self) -> PermissionSet {
        PermissionSet::not_applicable()
    }

    fn request(&self, _kind: PermissionKind) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported(reason::NO_PERMISSION_MODEL.to_owned()))
    }

    fn open_settings(&self, _kind: PermissionKind) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported(reason::NO_PERMISSION_MODEL.to_owned()))
    }
}

impl AppInfo for Imp {
    fn icon_png(&self, app: &AppRef, size_px: u32) -> Option<Vec<u8>> {
        let theme = self.0.appearance.icon_theme();
        let x = self.0.x11_ready();
        self.0.apps.icon_png(app, size_px, theme.as_deref(), x.as_deref())
    }

    fn display_name(&self, app: &AppRef) -> String {
        self.0.apps.display_name(app)
    }

    fn running_apps(&self) -> Vec<AppRef> {
        let x = self.0.x11_for(Feature::Selection).ok();
        self.0.apps.running_apps(x.as_deref(), self.0.own_pid)
    }
}

impl CursorScreen for Imp {
    fn monitors(&self) -> Vec<MonitorInfo> {
        // Wayland：客户端拿不到全局布局，返回空（上层退回 Tauri 的显示器接口）
        let Ok(x) = self.0.x11_for(Feature::Selection) else {
            return Vec::new();
        };
        x11::screen::monitors(&x)
            .iter()
            .map(geometry::PhysMonitor::to_info)
            .collect()
    }

    fn cursor(&self) -> (MonitorId, Point) {
        let Ok(x) = self.0.x11_for(Feature::Selection) else {
            return (MonitorId::new(), Point::default());
        };
        let monitors = x11::screen::monitors(&x);
        x11::screen::pointer(&x)
            .and_then(|(px, py, _)| geometry::monitor_at(&monitors, px, py).map(|m| (m.id.clone(), m.to_local(px, py))))
            .unwrap_or_default()
    }

    fn system_accent(&self) -> Option<String> {
        self.0.appearance.system_accent()
    }

    fn reduce_motion(&self) -> bool {
        self.0.appearance.reduce_motion()
    }

    fn reduce_transparency(&self) -> bool {
        self.0.appearance.reduce_transparency()
    }

    fn dark_mode(&self) -> bool {
        self.0.appearance.dark_mode()
    }

    fn taskbar_light(&self) -> Option<bool> {
        // Linux 托盘跟随面板主题（StatusNotifierItem），不需要区分
        None
    }

    fn watch_appearance(&self, cb: Box<dyn Fn() + Send + Sync>) -> Result<HookHandle, PlatformError> {
        self.0.appearance.watch(cb)
    }
}

impl ClipboardAccess for Imp {
    fn change_id(&self) -> u64 {
        // 序号保存在跨连接的注册表里（重连后单调递增）；X11 会话下顺带确保已经连接（开始计数）
        let _ = self.0.x11_for(Feature::ClipboardWatch);
        self.0.x11.registry().clip.change_id()
    }

    fn read(&self, limits: &ReadLimits) -> Result<RawClipboard, PlatformError> {
        let x = self.0.x11_for(Feature::ClipboardWatch).map_err(unsupported)?;
        x11::clipboard::read(&x, limits).map_err(PlatformError::Other)
    }

    fn snapshot(&self, max_bytes: usize, deadline: Duration) -> Result<Option<ClipSnapshot>, PlatformError> {
        let x = self.0.x11_for(Feature::ClipboardWatch).map_err(unsupported)?;
        // 过大与超时都返回 None（trait 的约定）；取词的回退流程在平台内部区分两者（「剪贴板内容过大」的提示只针对过大）
        Ok(
            match x11::clipboard::snapshot(&x, max_bytes, deadline).map_err(PlatformError::Other)? {
                x11::clipboard::Snap::Taken(snap, bytes) => Some(ClipSnapshot::new(snap, bytes)),
                x11::clipboard::Snap::TooLarge | x11::clipboard::Snap::Timeout => None,
            },
        )
    }

    fn restore(&self, snap: ClipSnapshot) -> Result<u64, PlatformError> {
        let x = self.0.x11_for(Feature::ClipboardWatch).map_err(unsupported)?;
        let snap = snap
            .into_inner::<x11::clipboard::LinuxSnapshot>()
            .ok_or_else(|| PlatformError::Other("snapshot was not created by the Linux backend".into()))?;
        x11::clipboard::restore(&x, snap).map_err(PlatformError::Other)
    }

    fn watch(&self, cb: Box<dyn Fn(u64) + Send + Sync>) -> Result<HookHandle, PlatformError> {
        let x = self.0.x11_for(Feature::ClipboardWatch).map_err(unsupported)?;
        if !x.ext.xfixes {
            return Err(unsupported(reason::X11_MISSING_XFIXES));
        }
        // 登记在跨连接的注册表里：断线重连后新连接继续回调
        let reg = Arc::clone(self.0.x11.registry());
        let id = reg.next_id();
        reg.clip_watchers.lock().push((id, Arc::from(cb)));
        Ok(HookHandle::new(move || {
            reg.clip_watchers.lock().retain(|(i, _)| *i != id)
        }))
    }

    fn suppress(&self, range: SuppressRange) {
        // 抑制区间与序号一起跨连接保存，不需要连接
        self.0.x11.registry().clip.suppress(range);
    }
}

impl OutsideClick for Imp {
    fn watch(&self, cb: Box<dyn Fn(ClickEvent) + Send + Sync>) -> Result<HookHandle, PlatformError> {
        let x = self.0.x11_for(Feature::InputHooks).map_err(unsupported)?;
        if !x11::input::available(&x) {
            return Err(unsupported(reason::X11_MISSING_XINPUT));
        }
        let lazy = Arc::clone(&self.0.x11);
        let id = lazy.registry().next_id();
        lazy.registry().clicks.lock().push((id, Arc::from(cb)));
        x11::input::sync(&x);
        Ok(HookHandle::new(move || {
            lazy.registry().clicks.lock().retain(|(i, _)| *i != id);
            if let Some(x) = lazy.peek() {
                x11::input::sync(&x);
            }
        }))
    }
}

impl HotkeyEnv for Imp {
    fn system_hotkeys(&self) -> Vec<String> {
        // 只有 macOS 能读系统热键表；X11 的冲突由注册探测（XGrabKey 的 Access 错误）发现
        Vec::new()
    }

    fn keyboard_layouts(&self) -> LayoutInfo {
        let Ok(x) = self.0.x11_for(Feature::Selection) else {
            return LayoutInfo {
                has_altgr: false,
                input_language_count: 1,
                alt_shift_switches: false,
            };
        };
        let (input_language_count, alt_shift_switches) = x11::keyboard::layout_switching(&x);
        LayoutInfo {
            has_altgr: x11::keyboard::keymap(&x).is_some_and(|m| m.has_altgr()),
            input_language_count,
            alt_shift_switches,
        }
    }

    fn key_grab(&self) -> Option<&dyn KeyGrab> {
        // 只有 X11 会话：Wayland（含 XWayland）下全局抓键对原生 Wayland 窗口无效，没有显示时无从抓取
        (self.0.session.server == DisplayServer::X11).then_some(self as &dyn KeyGrab)
    }
}

impl KeyGrab for Imp {
    fn set_handler(&self, handler: KeyGrabHandler) {
        self.0.x11.registry().hotkeys.set_handler(Arc::from(handler));
    }

    fn register(&self, combo: &str) -> Result<(), KeyGrabError> {
        let x = self
            .0
            .x11_for(Feature::Selection)
            .map_err(|r| KeyGrabError::Rejected(r.to_owned()))?;
        x11::hotkeys::register(&x, combo)
    }

    fn unregister(&self, combo: &str) {
        match self.0.x11.peek() {
            Some(x) => x11::hotkeys::unregister(&x, combo),
            None => self.0.x11.registry().hotkeys.forget(combo),
        }
    }
}

#[cfg(feature = "mouse-hook")]
impl InputHooks for Imp {
    fn install(&self, spec: HookSpec, sink: Box<dyn Fn(InputSignal) + Send + Sync>) -> Result<HookHandle, HookError> {
        if spec.is_empty() {
            return Ok(HookHandle::noop());
        }
        let x = self
            .0
            .x11_for(Feature::InputHooks)
            .map_err(|r| HookError::Unsupported(r.to_owned()))?;
        let id = x11::input::install(&x, spec, Arc::from(sink))?;
        let lazy = Arc::clone(&self.0.x11);
        Ok(HookHandle::new(move || {
            lazy.registry().hooks.lock().retain(|h| h.id != id);
            if let Some(x) = lazy.peek() {
                x11::input::sync(&x);
            }
        }))
    }
}

#[cfg(feature = "mouse-hook")]
impl BobberWindow for Imp {
    fn show(&self, at: (MonitorId, Point), dark: bool, reduced_motion: bool, on_click: Box<dyn Fn() + Send + Sync>) {
        if let Ok(x) = self.0.x11_for(Feature::InputHooks) {
            x11::bobber::show(&x, at, dark, reduced_motion, Arc::from(on_click));
        }
    }

    fn hide(&self, animated: bool) {
        if let Some(x) = self.0.x11_ready() {
            x11::bobber::hide(&x, animated);
        }
    }

    fn rect(&self) -> Option<(MonitorId, Rect)> {
        let x = self.0.x11_ready()?;
        x11::bobber::rect(&x)
    }
}
