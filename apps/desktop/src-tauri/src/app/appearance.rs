//! 外观解析与广播：读取系统强调色、跟随系统明暗变化、降低透明度 / 动效；WindowManager 生成 boot
//! 脚本时调用 [`current`]。
//!
//! - **跟随系统**：启动时经 `platform.screen.watch_appearance` 订阅（事件驱动、空闲零唤醒）。系统一次切换往往连发
//!   几条通知（明暗、强调色、任务栏明暗），回调只排一次 [`COALESCE`] 后的一次性重算；结果与上次广播的不同才
//!   `WindowManager::broadcast_appearance`（`appearance:changed`），并刷新托盘图标（Windows 任务栏明暗；Windows 的监听还包括
//!   显示缩放与显示器配置的变化，托盘据此按通知区域的新尺寸选图）；
//! - **设置变化**（主题 / 强调色 / 字号 / 材质 / 减少动效 / 界面语言）由 `app::reactor` 调 [`refresh`]（只在这些设置
//!   变了时），同样只在结果变化时广播；
//! - [`current`] / [`refresh`] 会读系统明暗与强调色，macOS 上各是一次主线程同步往返：**不要在 tokio worker 上调用**，
//!   异步上下文里放到阻塞线程池（本模块与 reactor、`appearance_get` 命令都这样做）；
//! - 强调色：`system` → 系统强调色，取不到时为空串（前端沿用 tokens.css 的默认值 #3B82F6 / #60A5FA）；
//!   预设名按明暗各取一档（与 tokens 同一色系，深色更亮以保证对比度）；`#RRGGBB` 原样使用（已由设置校验）；
//! - 材质：设置为 solid、系统开启「降低透明度」、或窗口不能透明（Linux Wayland 等，`runtime::transparent_windows`）
//!   时为 solid。

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tauri::Manager;
use wisp_platform::HookHandle;
use wisp_types::{AppearanceState, MaterialPref, OsKind, ReduceMotionPref, ResolvedMaterial, ResolvedTheme, ThemePref};

use crate::state::AppState;

/// 合并系统外观通知的时间窗
pub const COALESCE: Duration = Duration::from_millis(60);

/// 上次广播的外观
static LAST: Mutex<Option<AppearanceState>> = Mutex::new(None);
/// 系统外观监听的句柄（drop 即停止监听）
static WATCH: Mutex<Option<HookHandle>> = Mutex::new(None);
/// 已排队了一次重算
static PENDING: AtomicBool = AtomicBool::new(false);

/// 预设强调色（浅色, 深色）
pub fn preset_accent(name: &str, theme: ResolvedTheme) -> Option<&'static str> {
    let (light, dark) = match name {
        "blue" => ("#3B82F6", "#60A5FA"),
        "purple" => ("#8B5CF6", "#A78BFA"),
        "pink" => ("#EC4899", "#F472B6"),
        "red" => ("#EF4444", "#F87171"),
        "orange" => ("#F97316", "#FB923C"),
        "yellow" => ("#CA8A04", "#FACC15"),
        "green" => ("#16A34A", "#4ADE80"),
        "graphite" => ("#6B7280", "#9CA3AF"),
        _ => return None,
    };
    Some(match theme {
        ResolvedTheme::Light => light,
        ResolvedTheme::Dark => dark,
    })
}

/// 系统外观（可替换，便于测试）
pub trait SystemAppearance {
    fn dark_mode(&self) -> bool;
    fn system_accent(&self) -> Option<String>;
    fn reduce_motion(&self) -> bool;
    fn reduce_transparency(&self) -> bool;
}

impl SystemAppearance for dyn wisp_platform::CursorScreen + '_ {
    fn dark_mode(&self) -> bool {
        wisp_platform::CursorScreen::dark_mode(self)
    }
    fn system_accent(&self) -> Option<String> {
        wisp_platform::CursorScreen::system_accent(self)
    }
    fn reduce_motion(&self) -> bool {
        wisp_platform::CursorScreen::reduce_motion(self)
    }
    fn reduce_transparency(&self) -> bool {
        wisp_platform::CursorScreen::reduce_transparency(self)
    }
}

/// 由设置与系统外观解析（纯函数）
pub fn resolve(
    settings: &wisp_types::Settings,
    system: &(impl SystemAppearance + ?Sized),
    transparent_windows: bool,
) -> AppearanceState {
    let a = &settings.appearance;
    let theme = match a.theme {
        ThemePref::Dark => ResolvedTheme::Dark,
        ThemePref::Light => ResolvedTheme::Light,
        ThemePref::System if system.dark_mode() => ResolvedTheme::Dark,
        ThemePref::System => ResolvedTheme::Light,
    };
    let accent = if a.accent.starts_with('#') {
        a.accent.to_ascii_uppercase()
    } else if let Some(preset) = preset_accent(&a.accent, theme) {
        preset.to_owned()
    } else {
        // "system"（以及无法识别的值）：系统强调色；取不到时为空串，前端沿用 tokens 的默认强调色
        system
            .system_accent()
            .map(|c| c.to_ascii_uppercase())
            .unwrap_or_default()
    };
    let reduce_motion = match a.reduce_motion {
        ReduceMotionPref::On => true,
        ReduceMotionPref::Off => false,
        ReduceMotionPref::System => system.reduce_motion(),
    };
    let reduce_transparency = system.reduce_transparency();
    let material = if a.material == MaterialPref::Solid || reduce_transparency || !transparent_windows {
        ResolvedMaterial::Solid
    } else {
        ResolvedMaterial::Native
    };
    AppearanceState {
        theme,
        accent,
        reduce_motion,
        reduce_transparency,
        material,
        font_size: a.font_size,
        os: OsKind::current(),
        ui_language: super::i18n::resolve(settings.general.ui_language).as_str().to_owned(),
    }
}

/// 当前外观（**阻塞**：见模块说明）
pub fn current(app: &tauri::AppHandle) -> AppearanceState {
    match app.try_state::<AppState>() {
        Some(state) => resolve(
            &state.settings.get(),
            &*state.platform.screen,
            super::runtime::transparent_windows(),
        ),
        None => fallback(),
    }
}

fn fallback() -> AppearanceState {
    AppearanceState {
        theme: ResolvedTheme::Light,
        accent: String::new(),
        reduce_motion: false,
        reduce_transparency: false,
        material: ResolvedMaterial::Native,
        font_size: 14,
        os: OsKind::current(),
        ui_language: super::i18n::resolve(wisp_types::UiLanguage::System).as_str().to_owned(),
    }
}

/// 重新解析；与上次广播的不同时广播 `appearance:changed`。返回是否广播了（**阻塞**：见模块说明）
pub fn refresh(app: &tauri::AppHandle) -> bool {
    let Some(state) = app.try_state::<AppState>() else {
        return false;
    };
    let next = current(app);
    {
        let mut last = LAST.lock();
        if last.as_ref() == Some(&next) {
            return false;
        }
        let first = last.is_none();
        *last = Some(next.clone());
        if first {
            // 启动时建立基线：窗口都还没创建（创建时 boot 脚本已经带上当前外观），不必广播
            return false;
        }
    }
    log::info!(
        "appearance theme={:?} material={:?} reduce_motion={} reduce_transparency={} lang={}",
        next.theme,
        next.material,
        next.reduce_motion,
        next.reduce_transparency,
        next.ui_language
    );
    state.windows.broadcast_appearance(&next);
    true
}

/// 订阅系统外观变化（setup 中调用一次）
pub fn watch(app: &tauri::AppHandle) {
    refresh(app);
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let handle = app.clone();
    let result = state.platform.screen.watch_appearance(Box::new(move || {
        if PENDING.swap(true, Ordering::AcqRel) {
            return;
        }
        let handle = handle.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(COALESCE).await;
            PENDING.store(false, Ordering::Release);
            // 读系统外观是主线程同步往返（macOS），放到阻塞线程池，不占 tokio worker
            let h = handle.clone();
            if let Err(e) = tauri::async_runtime::spawn_blocking(move || refresh(&h)).await {
                log::warn!("appearance refresh failed: {e}");
            }
            // 任务栏明暗与通知区域图标尺寸（Windows 显示缩放）不在 AppearanceState 里，托盘图标单独判断
            // （没变化时 refresh 不重新设置图标）
            crate::tray::refresh(&handle);
        });
    }));
    match result {
        Ok(hook) => *WATCH.lock() = Some(hook),
        Err(e) => log::warn!("cannot watch system appearance: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use wisp_types::Settings;

    use super::*;

    struct Fake {
        dark: bool,
        accent: Option<&'static str>,
        motion: bool,
        transparency: bool,
    }

    impl SystemAppearance for Fake {
        fn dark_mode(&self) -> bool {
            self.dark
        }
        fn system_accent(&self) -> Option<String> {
            self.accent.map(str::to_owned)
        }
        fn reduce_motion(&self) -> bool {
            self.motion
        }
        fn reduce_transparency(&self) -> bool {
            self.transparency
        }
    }

    const PLAIN: Fake = Fake {
        dark: false,
        accent: None,
        motion: false,
        transparency: false,
    };

    #[test]
    fn system_theme_follows_the_os() {
        let s = Settings::default();
        assert_eq!(resolve(&s, &PLAIN, true).theme, ResolvedTheme::Light);
        let dark = Fake { dark: true, ..PLAIN };
        assert_eq!(resolve(&s, &dark, true).theme, ResolvedTheme::Dark);
        let mut forced = Settings::default();
        forced.appearance.theme = ThemePref::Light;
        assert_eq!(resolve(&forced, &dark, true).theme, ResolvedTheme::Light);
    }

    #[test]
    fn accent_rules() {
        let mut s = Settings::default();
        // system 且取不到：空串（前端用 tokens 默认值）
        assert_eq!(resolve(&s, &PLAIN, true).accent, "");
        let sys = Fake {
            accent: Some("#007aff"),
            ..PLAIN
        };
        assert_eq!(resolve(&s, &sys, true).accent, "#007AFF");
        s.appearance.accent = "purple".to_owned();
        assert_eq!(resolve(&s, &sys, true).accent, "#8B5CF6");
        let dark = Fake { dark: true, ..PLAIN };
        assert_eq!(resolve(&s, &dark, true).accent, "#A78BFA", "深色用更亮的一档");
        s.appearance.accent = "#12ab34".to_owned();
        assert_eq!(resolve(&s, &sys, true).accent, "#12AB34");
        for preset in wisp_types::defaults::ACCENT_PRESETS {
            assert!(preset_accent(preset, ResolvedTheme::Light).is_some(), "{preset}");
            assert!(preset_accent(preset, ResolvedTheme::Dark).is_some(), "{preset}");
        }
    }

    #[test]
    fn material_and_motion() {
        let mut s = Settings::default();
        assert_eq!(resolve(&s, &PLAIN, true).material, ResolvedMaterial::Native);
        assert_eq!(
            resolve(&s, &PLAIN, false).material,
            ResolvedMaterial::Solid,
            "不能透明时为纯色"
        );
        let reduced = Fake {
            transparency: true,
            motion: true,
            ..PLAIN
        };
        let a = resolve(&s, &reduced, true);
        assert_eq!(a.material, ResolvedMaterial::Solid);
        assert!(a.reduce_transparency && a.reduce_motion);
        s.appearance.reduce_motion = ReduceMotionPref::Off;
        assert!(!resolve(&s, &reduced, true).reduce_motion);
        s.appearance.material = MaterialPref::Solid;
        assert_eq!(resolve(&s, &PLAIN, true).material, ResolvedMaterial::Solid);
    }

    #[test]
    fn language_is_resolved() {
        let mut s = Settings::default();
        s.general.ui_language = wisp_types::UiLanguage::EnUs;
        assert_eq!(resolve(&s, &PLAIN, true).ui_language, "en-US");
    }
}
