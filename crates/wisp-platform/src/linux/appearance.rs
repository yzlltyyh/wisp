//! 系统外观：明暗、强调色、减少动效、降低透明度。
//!
//! 主要来源是 xdg-desktop-portal 的 Settings 接口（X11 / Wayland 通用）：启动时在后台线程连接会话总线、
//! 一次 `ReadAll` 读齐，之后订阅 `SettingChanged` 信号；X11 下另外监听 XSETTINGS（主题名、动画开关）。
//! 读取都是查缓存（O(1)），变化完全事件驱动，**不轮询**。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex as StdMutex, PoisonError};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::appearance_logic::{self as logic, AppearanceInputs};
use super::dbus::{Bus, CallError};
use super::dbus_wire::{Message, Value};
use super::x11::{LazyX11, call_guarded};
use crate::error::PlatformError;
use crate::types::HookHandle;

const PORTAL_DEST: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SETTINGS_IFACE: &str = "org.freedesktop.portal.Settings";
const NS_APPEARANCE: &str = "org.freedesktop.appearance";
const NS_GNOME: &str = "org.gnome.desktop.interface";
const NS_KDE: &str = "org.kde.kdeglobals.KDE";
const NS_KDE_ICONS: &str = "org.kde.kdeglobals.Icons";
/// 首次读取外观时最多等待 portal 的时间（之后都读缓存）
const FIRST_READ_WAIT: Duration = Duration::from_millis(250);

type Watcher = Arc<dyn Fn() + Send + Sync>;

/// portal 读到的原始值
#[derive(Debug, Clone, Default)]
struct PortalValues {
    color_scheme: Option<u32>,
    accent: Option<(f64, f64, f64)>,
    contrast: Option<u32>,
    reduced_motion: Option<u32>,
    gnome_enable_animations: Option<bool>,
    gnome_gtk_theme: Option<String>,
    gnome_icon_theme: Option<String>,
    kde_animation_factor: Option<f64>,
    kde_icon_theme: Option<String>,
}

impl PortalValues {
    /// 合并一个设置项；返回是否是需要关心的键
    fn apply(&mut self, namespace: &str, key: &str, value: &Value) -> bool {
        match (namespace, key) {
            (NS_APPEARANCE, "color-scheme") => self.color_scheme = value.as_u32(),
            (NS_APPEARANCE, "accent-color") => self.accent = value.as_f64_triple(),
            (NS_APPEARANCE, "contrast") => self.contrast = value.as_u32(),
            (NS_APPEARANCE, "reduced-motion") => self.reduced_motion = value.as_u32(),
            (NS_GNOME, "enable-animations") => self.gnome_enable_animations = value.as_bool(),
            (NS_GNOME, "gtk-theme") => self.gnome_gtk_theme = value.as_str().map(str::to_owned),
            (NS_GNOME, "icon-theme") => self.gnome_icon_theme = value.as_str().map(str::to_owned),
            (NS_KDE, "AnimationDurationFactor") => {
                self.kde_animation_factor = value
                    .as_f64()
                    .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()));
            }
            (NS_KDE_ICONS, "Theme") => self.kde_icon_theme = value.as_str().map(str::to_owned),
            _ => return false,
        }
        true
    }
}

/// 外观服务
pub(crate) struct Appearance {
    portal: Mutex<PortalValues>,
    /// 初次读取是否完成（成功或失败都算完成）
    ready: (StdMutex<bool>, Condvar),
    watchers: Mutex<Vec<(u64, Watcher)>>,
    next_id: AtomicU64,
    bus: Mutex<Option<Arc<Bus>>>,
    x11: Arc<LazyX11>,
    /// X11 会话才读 XSETTINGS（Wayland 下不为此去连接 XWayland）
    use_xsettings: bool,
    /// settings.ini 与 GTK_THEME 的结果（启动时读一次）
    gtk_file: (Option<String>, Option<bool>),
    gtk_env: Option<String>,
}

impl Appearance {
    pub(crate) fn new(x11: Arc<LazyX11>, use_xsettings: bool) -> Arc<Self> {
        Arc::new(Self {
            portal: Mutex::new(PortalValues::default()),
            ready: (StdMutex::new(false), Condvar::new()),
            watchers: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            bus: Mutex::new(None),
            x11,
            use_xsettings,
            gtk_file: read_gtk_settings_file(),
            gtk_env: std::env::var("GTK_THEME").ok().filter(|v| !v.trim().is_empty()),
        })
    }

    /// 在后台线程连接 portal（`Platform::create` 不阻塞）
    pub(crate) fn start(self: &Arc<Self>) {
        let me = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("wisp-appearance".into())
            .spawn(move || me.connect_portal());
        if spawned.is_err() {
            self.mark_ready();
        }
    }

    fn connect_portal(self: Arc<Self>) {
        let started = Instant::now();
        let weak = Arc::downgrade(&self);
        let on_signal = Box::new(move |msg: &Message| {
            if let Some(me) = weak.upgrade() {
                me.on_signal(msg);
            }
        });
        let bus = match Bus::connect_session(Duration::from_secs(1), on_signal) {
            Ok(b) => b,
            Err(e) => {
                log::info!("会话总线不可用，外观改用 GTK 主题名判断：{e}");
                self.mark_ready();
                return;
            }
        };
        let rule = format!("type='signal',interface='{SETTINGS_IFACE}',member='SettingChanged',path='{PORTAL_PATH}'");
        if let Err(e) = bus.add_match(&rule, Duration::from_secs(1)) {
            log::debug!("订阅 SettingChanged 失败：{e:?}");
        }
        let namespaces = [NS_APPEARANCE, NS_GNOME, NS_KDE, NS_KDE_ICONS];
        match bus.call(
            PORTAL_DEST,
            PORTAL_PATH,
            SETTINGS_IFACE,
            "ReadAll",
            &[Value::str_array(namespaces)],
            Duration::from_secs(3),
        ) {
            Ok(reply) => {
                let mut portal = self.portal.lock();
                if let Some(all) = reply.body.first() {
                    for (ns, entries) in all.dict_entries() {
                        let Some(ns) = ns.as_str() else { continue };
                        for (k, v) in entries.dict_entries() {
                            if let Some(k) = k.as_str() {
                                portal.apply(ns, k, v);
                            }
                        }
                    }
                }
                log::info!("已读取 portal 外观设置（{} ms）", started.elapsed().as_millis());
            }
            Err(CallError::Remote(name)) if name.ends_with("UnknownMethod") => self.read_one_by_one(&bus),
            Err(e) => log::info!("读取 portal 外观设置失败：{e:?}"),
        }
        *self.bus.lock() = Some(bus);
        self.mark_ready();
        // 初次结果可能晚于界面创建：通知监听者重新读取
        self.notify();
    }

    /// 旧版 portal（Settings v1）只有 `Read`，其返回值多包了一层 variant
    fn read_one_by_one(&self, bus: &Bus) {
        let keys = [
            (NS_APPEARANCE, "color-scheme"),
            (NS_APPEARANCE, "accent-color"),
            (NS_APPEARANCE, "contrast"),
            (NS_GNOME, "enable-animations"),
            (NS_GNOME, "gtk-theme"),
            (NS_GNOME, "icon-theme"),
        ];
        for (ns, key) in keys {
            let reply = bus.call(
                PORTAL_DEST,
                PORTAL_PATH,
                SETTINGS_IFACE,
                "Read",
                &[Value::Str(ns.to_owned()), Value::Str(key.to_owned())],
                Duration::from_millis(500),
            );
            if let Ok(msg) = reply
                && let Some(v) = msg.body.first()
            {
                self.portal.lock().apply(ns, key, v.unwrap_variant());
            }
        }
    }

    fn on_signal(&self, msg: &Message) {
        if msg.member.as_deref() != Some("SettingChanged") || msg.interface.as_deref() != Some(SETTINGS_IFACE) {
            return;
        }
        let (Some(ns), Some(key), Some(value)) = (
            msg.body.first().and_then(Value::as_str),
            msg.body.get(1).and_then(Value::as_str),
            msg.body.get(2),
        ) else {
            return;
        };
        let relevant = self.portal.lock().apply(ns, key, value.unwrap_variant());
        if relevant {
            log::debug!("系统外观变化：{ns} / {key}");
            self.notify();
        }
    }

    fn mark_ready(&self) {
        let (lock, cv) = &self.ready;
        *lock.lock().unwrap_or_else(PoisonError::into_inner) = true;
        cv.notify_all();
    }

    fn wait_ready(&self) {
        let (lock, cv) = &self.ready;
        let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        if *guard {
            return;
        }
        let _ = cv
            .wait_timeout_while(guard, FIRST_READ_WAIT, |ready| !*ready)
            .unwrap_or_else(PoisonError::into_inner);
    }

    fn notify(&self) {
        let watchers: Vec<Watcher> = self.watchers.lock().iter().map(|(_, w)| Arc::clone(w)).collect();
        for w in watchers {
            call_guarded("外观监听", || w());
        }
    }

    fn inputs(&self) -> AppearanceInputs {
        self.wait_ready();
        let p = self.portal.lock().clone();
        let xs = self.xsettings();
        let gtk_theme = p
            .gnome_gtk_theme
            .clone()
            .or_else(|| xs.as_ref().and_then(|s| s.theme_name().map(str::to_owned)))
            .or_else(|| self.gtk_env.clone())
            .or_else(|| self.gtk_file.0.clone());
        let env_dark = self.gtk_env.as_deref().is_some_and(logic::gtk_theme_env_is_dark);
        AppearanceInputs {
            color_scheme: p.color_scheme,
            accent: p.accent,
            contrast: p.contrast,
            reduced_motion: p.reduced_motion,
            gnome_enable_animations: p.gnome_enable_animations,
            kde_animation_factor: p.kde_animation_factor,
            xsettings_enable_animations: xs.as_ref().and_then(|s| s.enable_animations()),
            gtk_theme,
            gtk_prefer_dark: if env_dark { Some(true) } else { self.gtk_file.1 },
        }
    }

    pub(crate) fn dark_mode(&self) -> bool {
        logic::resolve_dark(&self.inputs())
    }

    pub(crate) fn system_accent(&self) -> Option<String> {
        self.inputs().accent.and_then(logic::accent_hex)
    }

    pub(crate) fn reduce_motion(&self) -> bool {
        logic::resolve_reduce_motion(&self.inputs())
    }

    pub(crate) fn reduce_transparency(&self) -> bool {
        logic::resolve_reduce_transparency(&self.inputs())
    }

    /// 当前图标主题（portal → XSETTINGS）
    pub(crate) fn icon_theme(&self) -> Option<String> {
        let p = self.portal.lock().clone();
        p.gnome_icon_theme
            .or(p.kde_icon_theme)
            .or_else(|| self.xsettings().and_then(|s| s.icon_theme_name().map(str::to_owned)))
    }

    fn xsettings(&self) -> Option<super::xsettings::XSettings> {
        if !self.use_xsettings {
            return None;
        }
        self.x11.get().ok().and_then(|x| x.screen_state.xsettings())
    }

    /// 监听外观变化（portal 信号 + XSETTINGS），事件驱动
    pub(crate) fn watch(self: &Arc<Self>, cb: Box<dyn Fn() + Send + Sync>) -> Result<HookHandle, PlatformError> {
        let cb: Watcher = Arc::from(cb);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.watchers.lock().push((id, Arc::clone(&cb)));
        // X11：XSETTINGS 变化（主题名、动画开关）也触发。登记在跨连接的注册表里，不需要现在就连接 X
        let xs = self.use_xsettings.then(|| {
            let reg = Arc::clone(self.x11.registry());
            let lid = reg.next_id();
            reg.settings_listeners.lock().push((lid, Arc::clone(&cb)));
            (reg, lid)
        });
        let me = Arc::downgrade(self);
        Ok(HookHandle::new(move || {
            if let Some(me) = me.upgrade() {
                me.watchers.lock().retain(|(i, _)| *i != id);
            }
            if let Some((reg, lid)) = xs {
                reg.settings_listeners.lock().retain(|(i, _)| *i != lid);
            }
        }))
    }
}

/// `$XDG_CONFIG_HOME/gtk-4.0/settings.ini` → `gtk-3.0/settings.ini`
fn read_gtk_settings_file() -> (Option<String>, Option<bool>) {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")));
    let Some(base) = base else {
        return (None, None);
    };
    for dir in ["gtk-4.0", "gtk-3.0"] {
        if let Ok(text) = std::fs::read_to_string(base.join(dir).join("settings.ini")) {
            let parsed = logic::parse_gtk_settings_ini(&text);
            if parsed.0.is_some() || parsed.1.is_some() {
                return parsed;
            }
        }
    }
    (None, None)
}
