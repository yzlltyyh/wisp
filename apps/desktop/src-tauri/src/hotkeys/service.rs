//! `HotkeyService`：订阅 settings 的变化（包括外部手改 settings.json / styles.json），按差异重新注册；修改快捷键时先注册
//! 新键、再注销旧键。
//!
//! 注册后端：macOS / Windows 用 tauri-plugin-global-shortcut；Linux X11 会话默认用
//! 平台层的 `HotkeyEnv::key_grab`（x11rb `XGrabKey`，按键事件在平台层唯一的 X11 事件线程上分发，空闲零唤醒）——
//! global-hotkey 0.8 的 X11 后端每 50 ms 轮询一次，Linux 上默认不注册插件（`app::runtime::global_shortcut_supported`）；
//! 环境变量 `WISP_X11_HOTKEYS=plugin` 是运行期回退（切回插件）。两者经同一个 [`Registrar`] 接口注册，回调都归到
//! [`HotkeyService::on_key`]（按 `Shortcut::id` 查路由）。
//!
//! 线程与锁：
//! - 插件的 register / unregister 要与主线程同步往返，一律在阻塞线程上调用（启动时的 `register_all` 例外：它本来就在
//!   主线程上，插件就地执行）；
//! - `reg`（注册状态）只在这些阻塞路径上持有，**主线程上的回调（`on_shortcut`、浮标点击）绝不拿它**——否则后台线程
//!   持锁等主线程、主线程等锁，形成死锁。回调只读 `routes` / `dispatch`，临界区里不做任何阻塞调用；
//! - 插件在持有自己的锁时调用 `on_shortcut`：回调里不得注册 / 注销（会重入插件的锁），只投递命令或事件；
//! - 锁的顺序：`reg` → `esc` → 其余（`routes` / `dispatch` / `statuses` / `ext_status`，都是短临界区）；扩展触发的
//!   `ext`（只在阻塞线程上持有，安装钩子期间可能占用数秒）→ `ext_status` / `statuses`：扩展触发在 `ext` 锁内读取最新
//!   设置与权限、安装并发布状态，并发的几次应用按拿到锁的顺序发布，缓存不会停在旧值。

use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use tauri::{Emitter, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcut, GlobalShortcutExt, Shortcut, ShortcutEvent, ShortcutState};
use wisp_platform::{KeyGrab, KeyGrabError};
use wisp_types::{
    Binding, ConflictLevel, ConflictReport, ExtTriggerStatus, HideReason, HotkeyAction, HotkeySettings, HotkeyStatus,
    HotkeyTarget, HotkeyTestReceived, KeyboardAdvice, NeighborHit, OsKind, Style, event,
};

use super::conflicts::{self, CheckEnv, Taken};
use super::dispatch::{Dispatch, KeyStep};
use super::esc_guard::{ESCAPE, EscAction, EscGuard};
use super::neighbors::{self, Running};
use super::plan::{self, Commit, Env, RegError, Registrar};
use super::recorder;
use super::triggers::{self, Desired};
use crate::app::i18n;
use crate::bus::{Bus, Command, TriggerOrigin};
use crate::error::{IpcError, IpcErrorCode};
use crate::settings::SettingsService;
use crate::state::{AppState, Deps};
use crate::windows::settings_window::{ONBOARDING_LABEL, SETTINGS_LABEL};

/// 录制 / 测试期间的看门狗间隔（只在这两种状态下运行）
const WATCHDOG_TICK: Duration = Duration::from_secs(2);
/// 启动后多久做一次邻居探测（错开冷启动的关键路径）
const STARTUP_NEIGHBOR_DELAY: Duration = Duration::from_secs(5);
/// 邻居探测结果的有效期：`hotkey_check` / `keyboard_advice` 在这之内复用最近一次探测（引导页、快捷键页会同时调用几个
/// 命令），超过就重新探测（< 20 ms）——启动之后才打开的 QQ / Easydict 也能被提示
const NEIGHBOR_TTL: Duration = Duration::from_secs(2);
/// `hotkey_status` 顺便重试注册的最小间隔（设置页、引导页频繁取状态时不反复与主线程往返、反复注册注定失败的键）
const RETRY_THROTTLE: Duration = Duration::from_secs(5);
/// 扩展触发因 `HookError::Other`（例如 tap 线程启动超时）没装上时，隔多久自动重试一次（每组输入只重试一次）
#[cfg(feature = "mouse-hook")]
const EXT_RETRY_DELAY: Duration = Duration::from_secs(3);

/// 影响注册结果的输入（没变就不重新注册）
#[derive(Debug, Clone, PartialEq, Eq)]
struct Inputs {
    bindings: BTreeMap<HotkeyAction, Binding>,
    styles: Vec<(String, Option<String>)>,
}

impl Inputs {
    fn of(h: &HotkeySettings, styles: &[Style]) -> Self {
        Self {
            bindings: h.bindings.clone(),
            styles: styles
                .iter()
                .filter(|s| s.hotkey.is_some())
                .map(|s| (s.id.clone(), s.hotkey.clone()))
                .collect(),
        }
    }
}

/// 注册状态（`reg` 锁）
#[derive(Default)]
struct RegState {
    /// 当前持有的注册：规范串 → 目标
    held: BTreeMap<String, HotkeyTarget>,
    /// 录制中（全部注销）
    suspended: bool,
    /// 上次应用的输入
    inputs: Option<Inputs>,
    /// 上次计算时有目标因可能自行消失的原因（被占用 / 被拒绝 / 系统快捷键）没用上首选
    retryable: bool,
    /// 上次在 `hotkey_status` 里顺便重试的时刻（节流）
    last_retry: Option<Instant>,
}

/// `on_shortcut` 的路由（快捷键 id → 目标）
#[derive(Default)]
struct Routes {
    by_id: HashMap<u32, HotkeyTarget>,
    esc_id: Option<u32>,
}

/// 注册后端：快捷键插件（macOS / Windows），或平台层的 `KeyGrab`（Linux X11）
enum Backend<'a> {
    Plugin(&'a tauri::AppHandle),
    Native(&'a dyn KeyGrab),
}

impl Registrar for Backend<'_> {
    fn register(&self, combo: &str) -> Result<(), RegError> {
        match self {
            Self::Plugin(app) => {
                let Ok(sc) = Shortcut::from_str(combo) else {
                    return Err(RegError::Rejected);
                };
                match app.global_shortcut().register(sc) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        let message = e.to_string();
                        log::info!("hotkey {combo} not registered: {message}");
                        Err(plan::classify(&message))
                    }
                }
            }
            Self::Native(grab) => grab.register(combo).map_err(|e| {
                log::info!("hotkey {combo} not registered: {e}");
                native_reg_error(&e)
            }),
        }
    }

    fn unregister(&self, combo: &str) {
        match self {
            Self::Plugin(app) => {
                if let Ok(sc) = Shortcut::from_str(combo)
                    && let Err(e) = app.global_shortcut().unregister(sc)
                {
                    log::warn!("hotkey {combo} unregister failed: {e}");
                }
            }
            Self::Native(grab) => grab.unregister(combo),
        }
    }
}

/// 平台后端的注册失败 → 注册计划的分类（`BadAccess` = 被其他程序占用，其余一律「被拒绝」）
fn native_reg_error(e: &KeyGrabError) -> RegError {
    match e {
        KeyGrabError::Taken => RegError::Taken,
        KeyGrabError::Rejected(_) => RegError::Rejected,
    }
}

/// 平台层的快捷键后端（Linux X11）；`WISP_X11_HOTKEYS=plugin`（运行期回退，`app::runtime::x11_hotkeys_via_plugin`）时
/// 不用它、改走插件
fn native_grab(platform: &wisp_platform::Platform) -> Option<&dyn KeyGrab> {
    if crate::app::runtime::x11_hotkeys_via_plugin() {
        return None;
    }
    platform.hotkey_env.key_grab()
}

/// 快捷键 id（插件按 id 分发）
fn shortcut_id(combo: &str) -> Option<u32> {
    Shortcut::from_str(combo).ok().map(|s| s.id())
}

/// 扩展触发的运行期状态（未编入 `mouse-hook` 时没有钩子，只剩串行锁）
#[cfg(feature = "mouse-hook")]
type ExtRuntime = triggers::Runtime;
#[cfg(not(feature = "mouse-hook"))]
type ExtRuntime = ();

/// 快捷键服务
pub struct HotkeyService {
    app: tauri::AppHandle,
    settings: Arc<SettingsService>,
    platform: Arc<wisp_platform::Platform>,
    bus: Bus,
    os: OsKind,
    reg: Mutex<RegState>,
    /// `reg.suspended` 的无锁副本（Esc guard 在 `wisp-panel-guards` 线程上读）
    suspended: AtomicBool,
    routes: RwLock<Routes>,
    dispatch: Mutex<Dispatch>,
    statuses: RwLock<Vec<HotkeyStatus>>,
    esc: Mutex<EscGuard>,
    /// 扩展触发的运行期状态；也是「读取设置 → 安装 → 发布状态」的串行锁
    ext: Mutex<ExtRuntime>,
    ext_status: RwLock<ExtTriggerStatus>,
    /// 最近一次邻居探测的时刻与结果
    neighbors: Mutex<Option<(Instant, Vec<Running>)>>,
    started: AtomicBool,
    watchdog: AtomicBool,
}

impl HotkeyService {
    /// 构造（不注册任何快捷键）。有平台层快捷键后端（Linux X11）时在这里接上它的按键回调
    pub fn new(d: &Deps) -> Arc<Self> {
        if let Some(grab) = native_grab(&d.platform) {
            let app = d.app.clone();
            grab.set_handler(Box::new(move |combo, pressed| {
                if let Some(state) = app.try_state::<AppState>() {
                    state.hotkeys.on_native_key(combo, pressed);
                }
            }));
        }
        Arc::new(Self {
            app: d.app.clone(),
            settings: Arc::clone(&d.settings),
            platform: Arc::clone(&d.platform),
            bus: d.bus.clone(),
            os: OsKind::current(),
            reg: Mutex::new(RegState::default()),
            suspended: AtomicBool::new(false),
            routes: RwLock::new(Routes::default()),
            dispatch: Mutex::new(Dispatch::default()),
            statuses: RwLock::new(Vec::new()),
            esc: Mutex::new(EscGuard::default()),
            ext: Mutex::default(),
            ext_status: RwLock::new(ExtTriggerStatus::default()),
            neighbors: Mutex::new(None),
            started: AtomicBool::new(false),
            watchdog: AtomicBool::new(false),
        })
    }

    // ------------------------------------------------------------------ 注册

    /// 快捷键能否使用：Wayland 会话不注册（Wayland 下不调用插件，标记为需要 portal）；Linux X11 用平台层后端；
    /// 插件未注册（没有 X 显示）时不得调用 `app.global_shortcut()`
    fn availability(&self) -> Result<(), &'static str> {
        if crate::app::runtime::is_wayland_session() {
            return Err(wisp_platform::reason::WAYLAND_HOTKEYS);
        }
        if native_grab(&self.platform).is_some() {
            return Ok(());
        }
        if self.app.try_state::<GlobalShortcut<tauri::Wry>>().is_none() {
            return Err(if self.os == OsKind::Linux {
                wisp_platform::reason::NO_DISPLAY
            } else {
                wisp_platform::reason::WAYLAND_HOTKEYS
            });
        }
        Ok(())
    }

    fn registrar(&self) -> Backend<'_> {
        match native_grab(&self.platform) {
            Some(grab) => Backend::Native(grab),
            None => Backend::Plugin(&self.app),
        }
    }

    /// macOS 系统快捷键（CopySymbolicHotKeys，规范串）；其他平台为空。**阻塞**（macOS 与主线程同步往返）
    fn system_hotkeys(&self) -> Vec<String> {
        if self.os == OsKind::Macos {
            self.platform.hotkey_env.system_hotkeys()
        } else {
            Vec::new()
        }
    }

    /// 按当前设置重新注册（`force` = 即使输入没变也重做）。**阻塞**
    fn reconcile(&self, force: bool) -> Vec<HotkeyStatus> {
        let mut reg = self.reg.lock();
        let settings = self.settings.get();
        let styles = self.settings.styles();
        let inputs = Inputs::of(&settings.hotkeys, &styles);
        if !force && reg.inputs.as_ref() == Some(&inputs) {
            return self.status();
        }
        let wants = plan::wants(self.os, &settings.hotkeys, &styles);
        let statuses = match self.availability() {
            Err(reason_key) => {
                reg.retryable = false;
                plan::unavailable(&wants, reason_key)
            }
            Ok(()) if reg.suspended => {
                // 录制中：不注册，结束录制时整体重做
                reg.inputs = None;
                return self.status();
            }
            Ok(()) => {
                let system = self.system_hotkeys();
                let env = Env {
                    os: self.os,
                    system: &system,
                };
                let applied = plan::apply(&wants, &mut reg.held, &env, &self.registrar());
                reg.retryable = applied.retryable;
                applied.statuses
            }
        };
        reg.inputs = Some(inputs);
        self.install_routes(&reg.held);
        self.publish(statuses)
    }

    /// 由持有的注册重建路由
    fn install_routes(&self, held: &BTreeMap<String, HotkeyTarget>) {
        let by_id: HashMap<u32, HotkeyTarget> = held
            .iter()
            .filter_map(|(combo, target)| shortcut_id(combo).map(|id| (id, target.clone())))
            .collect();
        self.routes.write().by_id = by_id;
        self.dispatch.lock().reset_keys();
    }

    /// 更新状态缓存；变化时 emit `hotkeys:changed` 并让设置快照重新广播
    fn publish(&self, statuses: Vec<HotkeyStatus>) -> Vec<HotkeyStatus> {
        let changed = {
            let mut cur = self.statuses.write();
            if *cur == statuses {
                false
            } else {
                cur.clone_from(&statuses);
                true
            }
        };
        if changed {
            log::info!("hotkeys: {}", summarize(&statuses));
            self.emit_changed(&statuses);
            self.settings.notify_changed();
        }
        statuses
    }

    /// `hotkeys:changed` 只发给设置与引导窗口（Rust 侧的 `listen_any`——托盘——同样收得到）
    fn emit_changed(&self, statuses: &[HotkeyStatus]) {
        for label in [SETTINGS_LABEL, ONBOARDING_LABEL] {
            if let Err(e) = self.app.emit_to(label, event::HOTKEYS_CHANGED, statuses) {
                log::warn!("emit hotkeys:changed failed: {e}");
            }
        }
    }

    /// 按设置注册全部快捷键（首选 → 备选 → 留空）；启动时调用一次，同时开始订阅设置变化、按设置
    /// 安装扩展触发（全部关闭时什么都不做），并在启动后的空闲时刻做一次邻居探测
    pub fn register_all(&self) -> Vec<HotkeyStatus> {
        let statuses = self.reconcile(true);
        if !self.started.swap(true, Ordering::AcqRel) {
            self.spawn_watch();
            let desired = Desired::from_settings(self.os, &self.settings.get().hotkeys)
                .in_session(crate::app::runtime::is_wayland_session());
            if !desired.is_empty() {
                let app = self.app.clone();
                tauri::async_runtime::spawn_blocking(move || {
                    if let Some(state) = app.try_state::<AppState>() {
                        state.hotkeys.apply_ext_triggers();
                    }
                });
            }
            self.spawn_startup_neighbor_probe();
        }
        statuses
    }

    /// 订阅设置快照的修订号（设置、风格、热加载、权限变化都会递增），在阻塞线程上按差异处理
    fn spawn_watch(&self) {
        let mut rx = self.settings.changes();
        rx.borrow_and_update();
        let app = self.app.clone();
        tauri::async_runtime::spawn(async move {
            while rx.changed().await.is_ok() {
                // 合并同一时刻的多次变化
                tokio::task::yield_now().await;
                rx.borrow_and_update();
                let Some(svc) = app.try_state::<AppState>().map(|s| Arc::clone(&s.hotkeys)) else {
                    continue;
                };
                if let Err(e) = tauri::async_runtime::spawn_blocking(move || svc.on_settings_changed()).await {
                    log::warn!("hotkeys: settings change handling failed: {e}");
                }
            }
        });
    }

    /// 设置（或权限、风格）变化：快捷键按差异重新注册；扩展触发在规格或权限变化时重装（**阻塞**）
    fn on_settings_changed(&self) {
        self.reconcile(false);
        self.apply_ext_triggers();
    }

    /// 启动后一次性的邻居探测（只在启动时探测一次，不轮询）
    fn spawn_startup_neighbor_probe(&self) {
        let app = self.app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(STARTUP_NEIGHBOR_DELAY).await;
            let r = tauri::async_runtime::spawn_blocking(move || {
                let Some(state) = app.try_state::<AppState>() else {
                    return;
                };
                let hits = state.hotkeys.neighbors();
                for h in &hits {
                    log::info!("hotkeys: {} also uses {} ({})", h.app_name, h.combo, h.action.as_str());
                }
            })
            .await;
            if let Err(e) = r {
                log::warn!("neighbor probe failed: {e}");
            }
        });
    }

    /// 当前全部状态（只读缓存，不阻塞）
    pub fn status(&self) -> Vec<HotkeyStatus> {
        self.statuses.read().clone()
    }

    /// 设置页 / 引导页取状态时顺便自愈：有目标因**可能自行消失**的原因（被其他程序占用 / 被拒绝 / macOS 系统快捷键）
    /// 没用上首选时整体重试一次（例如占用首选键的程序已经退出，此时改回首选键），且 [`RETRY_THROTTLE`] 内最多一次；
    /// 原因是永久性的（重复、保留、无法识别——只会因设置变化而改变）或刚重试过时只读缓存。**可能阻塞**
    pub fn status_with_retry(&self) -> Vec<HotkeyStatus> {
        if self.suspended.load(Ordering::Acquire) || self.availability().is_err() {
            return self.status();
        }
        let due = {
            let mut reg = self.reg.lock();
            let now = Instant::now();
            let due = retry_due(reg.retryable, reg.last_retry, now);
            if due {
                reg.last_retry = Some(now);
            }
            due
        };
        if due { self.reconcile(true) } else { self.status() }
    }

    // ------------------------------------------------------------------ 录制与测试

    /// 录制开始：注销全部全局键（录制中按 ⌥D 不会弹出面板）。**阻塞**
    pub fn record_begin(&self) {
        {
            let mut reg = self.reg.lock();
            if reg.suspended {
                return;
            }
            reg.suspended = true;
            self.suspended.store(true, Ordering::Release);
            self.dispatch.lock().recording_since = Some(Instant::now());
            if self.availability().is_ok() {
                plan::release_all(&mut reg.held, &self.registrar());
            }
            reg.held.clear();
            self.routes.write().by_id.clear();
            self.sync_esc();
            let paused = plan::suspended(&self.status());
            self.publish(paused);
        }
        log::info!("hotkeys: suspended while recording");
        self.ensure_watchdog();
    }

    /// 录制结束：恢复全部全局键（按最新设置重新注册）。**阻塞**
    pub fn record_end(&self) {
        {
            let mut reg = self.reg.lock();
            if !reg.suspended {
                return;
            }
            reg.suspended = false;
            reg.inputs = None;
            self.suspended.store(false, Ordering::Release);
        }
        self.dispatch.lock().recording_since = None;
        self.reconcile(true);
        self.sync_esc();
        log::info!("hotkeys: resumed after recording");
    }

    /// 测试按钮：按下对应组合时 emit `hotkey:test_received`，测试期间不执行任何动作
    pub fn test_begin(&self, target: HotkeyTarget) {
        log::debug!("hotkeys: test begin {target:?}");
        self.dispatch.lock().test = Some((target, Instant::now()));
        self.ensure_watchdog();
    }

    /// 结束测试
    pub fn test_end(&self) {
        if self.dispatch.lock().test.take().is_some() {
            log::debug!("hotkeys: test end");
        }
    }

    /// 录制 / 测试期间的看门狗：设置与引导窗口都不在了、或超过上限时自动结束，快捷键不会一直处于暂停
    fn ensure_watchdog(&self) {
        if self.watchdog.swap(true, Ordering::AcqRel) {
            return;
        }
        let app = self.app.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(WATCHDOG_TICK).await;
                let Some(svc) = app.try_state::<AppState>().map(|s| Arc::clone(&s.hotkeys)) else {
                    break;
                };
                let window_open = [SETTINGS_LABEL, ONBOARDING_LABEL]
                    .iter()
                    .any(|l| app.get_webview_window(l).is_some());
                let step = svc.dispatch.lock().watchdog(window_open, Instant::now());
                if step.idle {
                    // 先放下标记再复查：复查前开始的录制 / 测试由本任务继续看管，之后开始的由它们自己拉起新的看门狗
                    svc.watchdog.store(false, Ordering::Release);
                    let idle = {
                        let d = svc.dispatch.lock();
                        d.recording_since.is_none() && d.test.is_none()
                    };
                    if idle || svc.watchdog.swap(true, Ordering::AcqRel) {
                        break;
                    }
                    continue;
                }
                if step.end_test {
                    svc.test_end();
                }
                if step.end_recording {
                    log::warn!("hotkeys: recording did not end properly, resuming shortcuts");
                    let s = Arc::clone(&svc);
                    if let Err(e) = tauri::async_runtime::spawn_blocking(move || s.record_end()).await {
                        log::warn!("hotkeys: resume failed: {e}");
                    }
                }
            }
        });
    }

    // ------------------------------------------------------------------ 冲突与设置

    /// 冲突判定的运行期环境（**阻塞**：macOS 读系统热键、Windows 读键盘布局）
    fn check_env(&self) -> CheckEnv {
        let settings = self.settings.get();
        let styles = self.settings.styles();
        let lang = i18n::resolve(settings.general.ui_language);
        let mut taken: Vec<Taken> = Vec::new();
        let mut push = |target: &HotkeyTarget, combo: &str| {
            if let Some(c) = recorder::normalize(combo)
                && !taken.iter().any(|t| &t.target == target && t.combo == c)
            {
                taken.push(Taken {
                    target: target.clone(),
                    combo: c,
                    name: super::names::target_name(lang, target, &styles),
                });
            }
        };
        // 正在生效的组合 + 显式绑定（即使暂时注册失败，也仍是用户为它选的键）
        for s in self.status() {
            if let Some(e) = &s.effective {
                push(&s.target, e);
            }
        }
        for w in plan::wants(self.os, &settings.hotkeys, &styles) {
            if !w.is_default
                && let Some(c) = w.candidates.first()
            {
                push(&w.target, c);
            }
        }
        let running = self.recent_neighbors();
        CheckEnv {
            os: Some(self.os),
            has_altgr: self.os == OsKind::Windows && self.platform.hotkey_env.keyboard_layouts().has_altgr,
            system: self.system_hotkeys(),
            taken,
            neighbors: running
                .iter()
                .flat_map(|r| r.combos.iter().map(|c| (r.name.clone(), c.clone())))
                .collect(),
        }
    }

    /// 冲突检查（静态表、系统动态、注册探测、邻居应用、布局 / 死键、与自身重复）。**阻塞**
    pub fn check(&self, target: &HotkeyTarget, combo: &str) -> ConflictReport {
        let env = self.check_env();
        let mut report = conflicts::analyze(&env, target, combo);
        if report.acceptable {
            self.probe_registration(&mut report);
        }
        report
    }

    /// 第 3 层：注册探测。不是 Wisp 已持有的组合时真的注册一次再注销：Windows 1409 / X11 BadAccess →
    /// 被其他程序占用（`registration`）；其他失败（macOS 只能拿到这一种）→ 系统拒绝注册（`rejected`），同样标红
    fn probe_registration(&self, report: &mut ConflictReport) {
        if self.availability().is_err() {
            return;
        }
        let reg = self.reg.lock();
        if reg.held.contains_key(&report.combo) {
            return;
        }
        let registrar = self.registrar();
        match registrar.register(&report.combo) {
            Ok(()) => registrar.unregister(&report.combo),
            Err(err) => {
                drop(reg);
                let mut conflicts = std::mem::take(&mut report.conflicts);
                conflicts.push(conflicts::registration_conflict(err));
                *report = conflicts::finish(
                    std::mem::take(&mut report.combo),
                    std::mem::take(&mut report.display),
                    conflicts,
                );
            }
        }
    }

    /// 设置绑定：先注册新键、成功后再注销旧键，失败则保留旧值；`force` 跳过 warning 级冲突。**阻塞**
    ///
    /// 录制中调用时先结束录制、恢复全部全局键，再走正常路径：录制器在停止录制的同时提交（`hotkey_record_end` 由
    /// 组件的清理异步发出，可能晚于 `hotkey_check` / `hotkey_set` 送达），新键当场真正注册，失败当场以 `conflict`
    /// 返回、旧值保持，返回值就是新绑定生效后的状态。之后迟到的 `hotkey_record_end` 是空操作
    ///
    /// 被拒绝时返回 `IpcError { code: conflict, message }`，`message` 是**不带参数即可完整显示**的 i18n key（见
    /// [`conflicts::rejection_key`]）；需要完整报告（含占位符参数）时前端重新调用 `hotkey_check`
    pub fn set(&self, target: HotkeyTarget, binding: Binding, force: bool) -> Result<Vec<HotkeyStatus>, IpcError> {
        if matches!(target, HotkeyTarget::Style { .. }) && binding.is_default() {
            return Err(IpcError::invalid_argument("styles have no default shortcut"));
        }
        self.end_recording_for_change("set");
        let binding = if binding.is_combo() {
            let report = self.check(&target, binding.as_str());
            let blocking = report
                .conflicts
                .iter()
                .find(|c| c.level == ConflictLevel::Forbidden || (!force && c.level == ConflictLevel::Warning));
            if let Some(c) = blocking {
                return Err(IpcError::new(IpcErrorCode::Conflict, conflicts::rejection_key(c)));
            }
            Binding(report.combo)
        } else {
            binding
        };
        {
            let mut reg = self.reg.lock();
            // 走到这里仍在录制，只可能是另一个录制器在这期间刚开始录制：这时不能恢复全局键，只写设置，那次录制结束时
            // 整体注册（返回的 Suspended 状态如实反映「全部暂停」）；Wayland 下不调用插件
            let register_now = !reg.suspended && self.availability().is_ok();
            let r = plan::commit_binding(
                &mut reg.held,
                &target,
                &binding,
                register_now,
                &self.registrar(),
                || self.persist(&target, &binding),
            );
            match r {
                Ok(()) => {}
                Err(Commit::Register(err)) => {
                    return Err(IpcError::new(
                        IpcErrorCode::Conflict,
                        conflicts::registration_conflict(err).message_key,
                    ));
                }
                Err(Commit::Persist(e)) => return Err(e),
            }
        }
        log::info!("hotkeys: {target:?} set to {}", binding.as_str());
        Ok(self.reconcile(true))
    }

    /// 改键前结束仍在进行的录制（录制中只写设置、不注册，返回值会是过期的 Suspended 状态）
    fn end_recording_for_change(&self, what: &str) {
        if self.suspended.load(Ordering::Acquire) {
            log::info!("hotkeys: {what} while recording, ending the recording first");
            self.record_end();
        }
    }

    /// 写入设置（动作 → settings.json；风格 → styles.json）
    fn persist(&self, target: &HotkeyTarget, binding: &Binding) -> Result<(), IpcError> {
        match target {
            HotkeyTarget::Action { action } => {
                let action = *action;
                self.settings
                    .update(&mut |s| {
                        if binding.is_default() {
                            s.hotkeys.bindings.remove(&action);
                        } else {
                            s.hotkeys.bindings.insert(action, binding.clone());
                        }
                    })
                    .map(|_| ())
            }
            HotkeyTarget::Style { style_id } => {
                let mut style = self
                    .settings
                    .styles()
                    .iter()
                    .find(|s| &s.id == style_id)
                    .cloned()
                    .ok_or_else(|| IpcError::not_found("style"))?;
                style.hotkey = binding.is_combo().then(|| binding.0.clone());
                self.settings.style_save(style).map(|_| ())
            }
        }
    }

    /// 五个动作全部恢复默认（风格快捷键没有默认值，不受影响）；录制中调用时先结束录制（同 [`Self::set`]）。**阻塞**
    pub fn reset_defaults(&self) -> Vec<HotkeyStatus> {
        self.end_recording_for_change("reset");
        if let Err(e) = self.settings.update(&mut |s| s.hotkeys.bindings.clear()) {
            log::warn!("hotkeys: reset to defaults failed: {}", e.message);
        }
        self.reconcile(true)
    }

    // ------------------------------------------------------------------ 触发

    /// 全局快捷键插件的回调（主线程）。只做查表、过滤与投递，不阻塞、不调用插件
    pub(crate) fn on_shortcut(&self, sc: &Shortcut, ev: ShortcutEvent) {
        self.on_key(sc.id(), ev.state == ShortcutState::Pressed);
    }

    /// 平台层快捷键后端的回调（Linux X11：平台层的回调线程上）。`combo` 是注册时的规范串，按与插件相同的
    /// `Shortcut::id` 查路由
    pub(crate) fn on_native_key(&self, combo: &str, pressed: bool) {
        match shortcut_id(combo) {
            Some(id) => self.on_key(id, pressed),
            None => log::debug!("hotkeys: native key for an unparsable combo ignored"),
        }
    }

    /// 两种后端共用的按键处理：查表、按住 / 去抖过滤、投递。不阻塞、不注册 / 注销
    fn on_key(&self, id: u32, pressed: bool) {
        let (target, is_esc) = {
            let r = self.routes.read();
            (r.by_id.get(&id).cloned(), r.esc_id == Some(id))
        };
        let step = self.dispatch.lock().on_key(id, target, is_esc, pressed, Instant::now());
        match step {
            KeyStep::Fire(target) => self.send(&target, TriggerOrigin::Hotkey),
            KeyStep::Echo(target) => self.emit_test_received(target),
            KeyStep::Esc => {
                // 不在按键回调里同步处理（隐藏面板最终会注销 Esc，不能重入插件 / 平台后端）。有截图会话时先取消它
                let app = self.app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Some(state) = app.try_state::<AppState>()
                        && !state.orchestrator.escape_overlay()
                    {
                        state.windows.panel_request_hide(HideReason::Escape);
                    }
                });
            }
            KeyStep::Ignore => {}
        }
    }

    fn emit_test_received(&self, target: HotkeyTarget) {
        log::debug!("hotkeys: test received {target:?}");
        let payload = HotkeyTestReceived { target };
        for label in [SETTINGS_LABEL, ONBOARDING_LABEL] {
            if let Err(e) = self.app.emit_to(label, event::HOTKEY_TEST_RECEIVED, &payload) {
                log::warn!("emit hotkey:test_received failed: {e}");
            }
        }
    }

    /// 扩展触发的派发入口（浮标点击、双击修饰键、鼠标键）：同样受去抖与录制 / 测试期间的抑制
    #[cfg(feature = "mouse-hook")]
    pub(crate) fn fire(&self, target: &HotkeyTarget, origin: TriggerOrigin) {
        if self.dispatch.lock().on_trigger(target, Instant::now()) {
            self.send(target, origin);
        } else {
            log::debug!("hotkeys: {} ignored (recording / testing / debounce)", origin.as_str());
        }
    }

    /// 投递到编排器（重入规则——替换、toggle、Overlay 存在时忽略——由编排器负责）
    fn send(&self, target: &HotkeyTarget, origin: TriggerOrigin) {
        let cmd = match target {
            HotkeyTarget::Action { action } => Command::Action {
                action: *action,
                origin,
            },
            HotkeyTarget::Style { style_id } => Command::TranslateWithStyle {
                style_id: style_id.clone(),
                origin,
            },
        };
        log::debug!("hotkeys: dispatch {cmd:?}");
        self.bus.send(cmd);
    }

    // ------------------------------------------------------------------ Esc guard

    /// 翻译面板非 key 期间临时注册全局 Esc。从 `wisp-panel-guards` 线程调用，**阻塞**（插件与主线程往返）
    pub fn esc_guard(&self, active: bool) {
        self.esc.lock().wanted = active;
        self.sync_esc();
    }

    /// 截图会话（Overlay 显示中、还没 commit）期间注册全局 Esc。编排器在会话开始 / commit /
    /// 结束时调用，**阻塞**（插件与主线程往返）
    pub fn overlay_esc_guard(&self, active: bool) {
        self.esc.lock().overlay = active;
        self.sync_esc();
    }

    /// 让 Esc 的注册状态符合期望。注册与注销时都忘掉 Esc 的按住状态：按 Esc 关闭面板时 Esc 往往还没松开就被注销，
    /// 插件不会再送来 Released；不清掉的话，面板很快再次以非 key 状态出现时第一次按 Esc 会被当成按住重复而忽略
    fn sync_esc(&self) {
        let mut g = self.esc.lock();
        let available = self.availability().is_ok();
        let esc_id = shortcut_id(ESCAPE);
        match g.next(self.suspended.load(Ordering::Acquire), available) {
            EscAction::Register => match self.registrar().register(ESCAPE) {
                Ok(()) => {
                    g.registered = true;
                    self.forget_esc(esc_id);
                    self.routes.write().esc_id = esc_id;
                }
                Err(e) => log::warn!("hotkeys: global Esc not registered: {e:?}"),
            },
            EscAction::Unregister => {
                self.routes.write().esc_id = None;
                self.registrar().unregister(ESCAPE);
                self.forget_esc(esc_id);
                g.registered = false;
            }
            EscAction::Nothing => {}
        }
    }

    fn forget_esc(&self, esc_id: Option<u32>) {
        if let Some(id) = esc_id {
            self.dispatch.lock().forget_key(id);
        }
    }

    // ------------------------------------------------------------------ 扩展触发

    /// 按当前设置安装 / 卸载扩展触发（`mouse-hook` feature；全部关闭时进程中没有任何钩子线程）。**有副作用、阻塞**：
    /// 只由 HotkeyService 自己的设置订阅（规格或权限变化时）、启动时与延迟重试调用，结果缓存起来供
    /// [`Self::ext_trigger_status`] 读取。
    ///
    /// 整个过程在 `ext` 锁内：设置与权限在锁内读取（拿着锁之前读到的设置可能已经过期——例如延迟重试与设置订阅
    /// 同时到来，后拿到锁的一方会用旧设置把新钩子换回去），状态也在锁内发布（锁顺序 `ext` → `ext_status`，发布本身
    /// 不阻塞），所以缓存的状态总是最后一次应用的结果
    fn apply_ext_triggers(&self) -> ExtTriggerStatus {
        #[cfg_attr(not(feature = "mouse-hook"), allow(unused_mut, unused_variables))]
        let mut rt = self.ext.lock();
        let desired = Desired::from_settings(self.os, &self.settings.get().hotkeys)
            .in_session(crate::app::runtime::is_wayland_session());
        #[cfg(feature = "mouse-hook")]
        let status = {
            let perms = if desired.is_empty() {
                triggers::PermKey::from(wisp_types::PermissionSet::not_applicable())
            } else {
                triggers::PermKey::from(crate::app::permissions::cached(&self.app))
            };
            let status = match rt.reuse(&desired, perms) {
                Some(o) => o.status,
                None => rt.apply(&self.app, desired, perms).status,
            };
            if rt.take_retry() {
                self.schedule_ext_retry();
            }
            status
        };
        #[cfg(not(feature = "mouse-hook"))]
        let status = triggers::not_compiled(&desired);
        self.publish_ext(status.clone());
        status
    }

    /// 扩展触发因暂时性错误（`HookError::Other`）没全部装上：[`EXT_RETRY_DELAY`] 后按当时的设置重试一次（一次性的定时
    /// 任务，不轮询；同一组输入只重试一次，之后等设置或权限变化）
    #[cfg(feature = "mouse-hook")]
    fn schedule_ext_retry(&self) {
        let app = self.app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(EXT_RETRY_DELAY).await;
            let r = tauri::async_runtime::spawn_blocking(move || {
                let Some(state) = app.try_state::<AppState>() else {
                    return;
                };
                let svc = &state.hotkeys;
                if !svc.ext.lock().prepare_retry() {
                    return;
                }
                log::info!("ext triggers: retrying after a failed install");
                svc.apply_ext_triggers();
            })
            .await;
            if let Err(e) = r {
                log::warn!("ext triggers: retry failed: {e}");
            }
        });
    }

    /// 扩展触发状态变化：emit `hotkeys:changed` 并让设置快照重新广播。在 `ext` 锁内调用；这里只做
    /// 不阻塞的事（写缓存、emit、递增设置修订号——设置订阅随后在另一个阻塞线程上处理）
    fn publish_ext(&self, status: ExtTriggerStatus) {
        let changed = {
            let mut cur = self.ext_status.write();
            if *cur == status {
                false
            } else {
                *cur = status;
                true
            }
        };
        if changed {
            self.emit_changed(&self.status());
            self.settings.notify_changed();
        }
    }

    /// 扩展触发的当前状态（只读缓存值，无副作用；`runtime_info` / 设置快照用）
    pub fn ext_trigger_status(&self) -> ExtTriggerStatus {
        self.ext_status.read().clone()
    }

    // ------------------------------------------------------------------ 邻居与布局

    /// 最近 [`NEIGHBOR_TTL`] 内的邻居探测结果；过期或还没探测过时现在探测（**阻塞**，< 20 ms）
    fn recent_neighbors(&self) -> Vec<Running> {
        if let Some((at, r)) = self.neighbors.lock().as_ref()
            && neighbors::fresh(*at, Instant::now(), NEIGHBOR_TTL)
        {
            return r.clone();
        }
        self.probe_neighbors()
    }

    /// 重新探测一次并记下结果（**阻塞**）
    fn probe_neighbors(&self) -> Vec<Running> {
        let running = neighbors::probe(&self.platform);
        *self.neighbors.lock() = Some((Instant::now(), running.clone()));
        running
    }

    /// 邻居应用占用的组合键（打开引导页 / 快捷键页时调用，每次重新探测）。**阻塞**
    pub fn neighbors(&self) -> Vec<NeighborHit> {
        let running = self.probe_neighbors();
        neighbors::hits(self.os, &running, &self.status())
    }

    /// 键盘布局建议：AltGr 布局且只有一种输入语言时推荐 Alt+Shift 族；Alt+Shift 会切换输入语言时
    /// 不推荐（`multi_input_lang`）；Windows / Linux 上检测到 QQ 时提示 Ctrl+Alt+A。**阻塞**
    pub fn keyboard_advice(&self) -> KeyboardAdvice {
        let layout = self.platform.hotkey_env.keyboard_layouts();
        let qq_running = self.os != OsKind::Macos && neighbors::is_running(&self.recent_neighbors(), "QQ");
        advice_for(self.os, layout, qq_running)
    }

    /// 组合键的显示串（托盘菜单用），与前端 `formatHotkey` 共用 fixture
    pub fn display(&self, combo: &str) -> String {
        super::display::display(combo, self.os)
    }
}

/// 由布局信息得出建议。`multi_input_lang` 的含义是「Alt+Shift 会切换输入语言，不建议用 Alt+Shift 组合」：只在 Windows（多种输入语言时系统默认左 Alt+Shift 切换）与 XKB 选项确认 Alt+Shift 切换布局的 X11 上为
/// true；macOS 恒为 false——⌥⇧ 不切换输入源，Wisp 自己的默认键 ⌥⇧D / ⌥⇧S 正是这一族
fn advice_for(os: OsKind, layout: wisp_platform::LayoutInfo, qq_running: bool) -> KeyboardAdvice {
    let multi = layout.input_language_count > 1;
    let alt_shift_taken = match os {
        OsKind::Macos => false,
        OsKind::Windows => multi,
        OsKind::Linux => multi && layout.alt_shift_switches,
    };
    KeyboardAdvice {
        has_altgr: os != OsKind::Macos && layout.has_altgr,
        multi_input_lang: alt_shift_taken,
        recommend_family: (layout.has_altgr && os != OsKind::Macos && !alt_shift_taken).then(|| "alt_shift".to_owned()),
        qq_running,
    }
}

/// `hotkey_status` 是否该顺便重试注册：上次计算有可能自愈的失败，且距上次重试至少 [`RETRY_THROTTLE`]
fn retry_due(retryable: bool, last: Option<Instant>, now: Instant) -> bool {
    retryable && last.is_none_or(|t| now.saturating_duration_since(t) >= RETRY_THROTTLE)
}

/// 一行日志概括全部状态（不含正文，只有键名与状态）
fn summarize(statuses: &[HotkeyStatus]) -> String {
    statuses
        .iter()
        .map(|s| {
            let name = match &s.target {
                HotkeyTarget::Action { action } => action.as_str().to_owned(),
                HotkeyTarget::Style { style_id } => format!("style:{style_id}"),
            };
            match &s.effective {
                Some(e) => format!("{name}={:?}({e})", s.state),
                None => format!("{name}={:?}", s.state),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use wisp_types::HotkeyState;

    use super::*;

    #[test]
    fn macos_never_warns_about_alt_shift() {
        use wisp_platform::LayoutInfo;
        // ABC + 拼音：两种输入源，但 ⌥⇧ 不切换输入源（Wisp 的默认键就是 ⌥⇧D / ⌥⇧S）
        for layout in [
            LayoutInfo {
                has_altgr: false,
                input_language_count: 2,
                alt_shift_switches: false,
            },
            LayoutInfo {
                has_altgr: true,
                input_language_count: 3,
                alt_shift_switches: true,
            },
        ] {
            let a = advice_for(OsKind::Macos, layout, false);
            assert!(!a.multi_input_lang && !a.has_altgr, "{a:?}");
            assert_eq!(a.recommend_family, None);
        }
        // Windows：多种输入语言 → 左 Alt+Shift 是切换键
        let multi = LayoutInfo {
            has_altgr: true,
            input_language_count: 2,
            alt_shift_switches: true,
        };
        let a = advice_for(OsKind::Windows, multi, false);
        assert!(a.multi_input_lang && a.recommend_family.is_none());
        let single = LayoutInfo {
            input_language_count: 1,
            alt_shift_switches: false,
            ..multi
        };
        assert_eq!(
            advice_for(OsKind::Windows, single, false).recommend_family.as_deref(),
            Some("alt_shift")
        );
        // X11：只有 XKB 选项确认 Alt+Shift 切换布局时才提示
        let gnome = LayoutInfo {
            alt_shift_switches: false,
            ..multi
        };
        assert!(!advice_for(OsKind::Linux, gnome, false).multi_input_lang);
        assert!(advice_for(OsKind::Linux, multi, false).multi_input_lang);
    }

    #[test]
    fn inputs_ignore_styles_without_hotkeys() {
        let a = Style::custom("custom-a0000000", "a");
        let mut b = Style::custom("custom-b0000000", "b");
        let h = HotkeySettings::default();
        assert_eq!(Inputs::of(&h, std::slice::from_ref(&a)), Inputs::of(&h, &[]));
        b.hotkey = Some("Alt+KeyB".into());
        assert_ne!(Inputs::of(&h, &[a, b]), Inputs::of(&h, &[]));
    }

    #[test]
    fn summary_has_no_payload() {
        let s = HotkeyStatus {
            target: HotkeyTarget::Action {
                action: HotkeyAction::Ocr,
            },
            binding: Binding::default_binding(),
            effective: Some("Alt+KeyS".into()),
            display: Some("⌥S".into()),
            state: HotkeyState::Registered,
            reason_key: None,
        };
        assert_eq!(summarize(&[s]), "ocr=Registered(Alt+KeyS)");
    }

    /// 取状态时的重试：只在有可能自愈的失败时进行，且 5 s 内最多一次
    #[test]
    fn status_retry_is_throttled() {
        let t0 = Instant::now();
        assert!(
            !retry_due(false, None, t0),
            "没有可自愈的失败（全部生效，或原因是重复 / 保留）"
        );
        assert!(retry_due(true, None, t0));
        assert!(!retry_due(true, Some(t0), t0 + Duration::from_millis(4999)));
        assert!(retry_due(true, Some(t0), t0 + RETRY_THROTTLE));
    }

    #[test]
    fn shortcut_ids_match_the_plugin() {
        // 路由用的 id 必须与插件分发时的 Shortcut::id 一致（同一组合的不同写法也一致）
        let a = shortcut_id("Control+Alt+Shift+KeyS").expect("可解析");
        let b = Shortcut::from_str("shift+control+alt+KeyS").expect("可解析").id();
        assert_eq!(a, b);
        assert!(shortcut_id(ESCAPE).is_some());
    }
}
