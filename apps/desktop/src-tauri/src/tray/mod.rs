//! 托盘。
//!
//! - [`build`]：setup 第 6 步，完成即「托盘就绪」。Linux 上（探测见 [`host`]）：
//!   - 找不到可加载的 AppIndicator 库时不建托盘（tray-icon 缺库会直接 panic，release 构建 `panic = "abort"`），改为
//!     打开设置窗口并发一条系统通知；
//!   - 建好后在后台确认会话里有 StatusNotifierWatcher（GNOME 没装 AppIndicator 扩展时没有、图标不可见），确认没有时同样
//!     退化。登录时自启的情况下面板 / 扩展可能比 Wisp 晚就绪，先等 [`HOST_GRACE_AT_LOGIN`]；托盘对象保留（只有 XEmbed
//!     托盘的桌面上 libappindicator 会退回 GtkStatusIcon，图标仍可能出现）；冒烟模式不做这项检查；
//! - [`refresh`]：任何线程都可以调用；合并到主线程执行一次，按当前状态更新状态行、暂停勾选、角标与 tooltip，
//!   语言 / 固定窗口项 / 快捷键变化时整份重建菜单。设置变化（`app::reactor`）、`hotkeys:changed`、权限变化、更新器状态
//!   变化都会触发；其他模块在改变了托盘相关状态（暂停剪贴板、固定窗口增减）后也应调用它；
//! - [`flash`]：疑似密钥被跳过时闪一次（`clipboard.secret_tray_flash` 关闭时不闪）；
//! - 点开托盘时顺带刷新一次权限状态并惰性检查更新，没有任何常驻定时器。Windows 上 tray-icon 自己的
//!   「离开检测」定时器（悬停期间 15 ms 一拍）在上游会在光标停住之后漏关、从此常驻（光标停在图标上、或停住后第一下移动
//!   就离开图标时），靠 vendor/tray-icon 的补丁修正：每一拍对照缓存的图标矩形，离开即关，停住约 0.5 s 也关
//!   （单测 `tray_icon_is_patched_so_the_leave_timer_never_outlives_the_hover`）。

pub mod host;
pub mod icon;
pub mod menu;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tauri::Manager;
use tauri::tray::{MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri_plugin_notification::NotificationExt;
use wisp_types::{OsKind, WindowTarget};

use crate::app::i18n;
use crate::state::AppState;
use crate::windows::native;

use self::icon::{Dot, Glyph, IconKey};
use self::menu::{BuiltMenu, MenuModel, TrayStatus};

/// 托盘图标 id
pub const TRAY_ID: &str = "wisp";
/// 闪烁持续时间
pub const FLASH_DURATION: Duration = Duration::from_millis(900);
/// 乐观的暂停状态最多保留多久（剪贴板服务通常在几毫秒内就处理完）
const PAUSE_HINT_TTL: Duration = Duration::from_secs(2);
/// 登录时自启：等托盘宿主（面板 / GNOME 扩展）就绪的宽限
pub const HOST_GRACE_AT_LOGIN: Duration = Duration::from_secs(20);
/// 宽限期内重查托盘宿主的间隔
const HOST_RETRY: Duration = Duration::from_secs(2);

/// 托盘的运行时状态（只在主线程上读写；锁内不做任何菜单 / 托盘操作）
#[derive(Default)]
struct TrayState {
    built: Option<BuiltMenu>,
    model: Option<MenuModel>,
    icon: Option<IconKey>,
}

static TRAY: Mutex<TrayState> = Mutex::new(TrayState {
    built: None,
    model: None,
    icon: None,
});
/// 已经排队了一次主线程刷新
static REFRESH_PENDING: AtomicBool = AtomicBool::new(false);
/// 闪烁的代数（只有最新一次闪烁负责恢复图标）
static FLASH_GEN: AtomicU64 = AtomicU64::new(0);
/// 点击「暂停」后、剪贴板服务处理完之前的乐观值
static PAUSE_HINT: Mutex<Option<(bool, Instant)>> = Mutex::new(None);

/// 构建托盘（setup 第 6 步；在主线程调用）。Linux 无 AppIndicator 库时不建托盘、走退化路径（打开设置 + 通知）并返回 Ok。
/// 菜单按「权限未知」先建好（不在这里同步查询 TCC），权限查询在后台完成后刷新
pub fn build(app: &tauri::AppHandle) -> tauri::Result<()> {
    let linux = OsKind::current() == OsKind::Linux;
    if linux && !host::indicator_library_present() {
        log::warn!("tray unavailable: no loadable AppIndicator library; opening settings instead");
        no_tray_fallback(app);
        return Ok(());
    }
    let model = menu::model(app, None);
    let built = menu::build(app, &model)?;
    let key = icon_key(app, &model.status, None);
    let tray = TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon::render(key)?)
        .icon_as_template(key.glyph.is_template())
        .tooltip(tooltip(&model))
        .menu(&built.menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, ev| menu::on_event(app, &ev))
        .on_tray_icon_event(|tray, ev| on_icon_event(tray.app_handle(), &ev))
        .build(app)?;
    // VoiceOver 读的是状态栏按钮的无障碍名称，不是 tooltip：设成「Wisp — 状态」，随状态 / 语言更新
    native::tray_set_accessibility(&tray, &tooltip(&model));
    // VoiceOver / 键盘（⌃F8 聚焦状态菜单后按空格）对按钮执行 AXPress 时同样弹出菜单（macOS）
    {
        let handle = app.clone();
        native::tray_enable_press(&tray, move || {
            on_opened(&handle);
            if let Some(tray) = handle.tray_by_id(TRAY_ID)
                && let Err(e) = tray.with_inner_tray_icon(|inner| inner.show_menu())
            {
                log::debug!("tray menu not shown on press: {e}");
            }
        });
    }
    {
        let mut state = TRAY.lock();
        state.built = Some(built);
        state.model = Some(model);
        state.icon = Some(key);
    }
    if linux && !crate::app::smoke::enabled() {
        verify_host(app);
    }
    Ok(())
}

/// Linux：确认会话里有 StatusNotifierWatcher；登录自启时给 [`HOST_GRACE_AT_LOGIN`] 的宽限。查不到（三种 D-Bus 工具都
/// 不可用）时按「可见」处理，不误报
fn verify_host(app: &tauri::AppHandle) {
    let handle = app.clone();
    let grace = if crate::app::autostart::launched_at_login() {
        HOST_GRACE_AT_LOGIN
    } else {
        Duration::ZERO
    };
    tauri::async_runtime::spawn(async move {
        let started = Instant::now();
        loop {
            let present = tauri::async_runtime::spawn_blocking(host::sni_watcher_present)
                .await
                .ok()
                .flatten();
            match present {
                Some(true) => return,
                None => {
                    log::debug!("tray: cannot query the StatusNotifierWatcher; assuming the tray is visible");
                    return;
                }
                Some(false) if started.elapsed() >= grace => break,
                Some(false) => tokio::time::sleep(HOST_RETRY).await,
            }
        }
        log::warn!(
            "tray: no StatusNotifierWatcher on the session bus (GNOME without the AppIndicator extension?); \
             opening settings instead"
        );
        no_tray_fallback(&handle);
    });
}

/// 按当前状态刷新托盘（任何线程都可调用；合并到主线程执行一次）
pub fn refresh(app: &tauri::AppHandle) {
    if REFRESH_PENDING.swap(true, Ordering::AcqRel) {
        return;
    }
    let handle = app.clone();
    if let Err(e) = app.run_on_main_thread(move || {
        REFRESH_PENDING.store(false, Ordering::Release);
        refresh_now(&handle);
    }) {
        REFRESH_PENDING.store(false, Ordering::Release);
        log::debug!("tray refresh not scheduled: {e}");
    }
}

/// 点击「暂停剪贴板记录」后：记下乐观的目标值（`paused`，由调用方在**发送切换命令之前**算好）并刷新；乐观值过期时
/// 再刷新一次，保证最终显示的是剪贴板服务的实际状态
pub(crate) fn refresh_with_pause_hint(app: &tauri::AppHandle, paused: bool) {
    *PAUSE_HINT.lock() = Some((paused, Instant::now()));
    refresh(app);
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(PAUSE_HINT_TTL + Duration::from_millis(50)).await;
        refresh(&handle);
    });
}

/// 当前状态行（菜单事件里判断点击状态行要做什么）
pub(crate) fn current_status(app: &tauri::AppHandle) -> TrayStatus {
    let cached = TRAY.lock().model.as_ref().map(|m| m.status.clone());
    cached.unwrap_or_else(|| menu::model(app, None).status)
}

/// 乐观的暂停值：剪贴板服务的状态已与之一致、或乐观值过期后清除
fn pause_override(app: &tauri::AppHandle) -> Option<bool> {
    let actual = app.try_state::<AppState>()?.clipboard.paused();
    let mut hint = PAUSE_HINT.lock();
    let (paused, at) = (*hint)?;
    if paused == actual || at.elapsed() > PAUSE_HINT_TTL {
        *hint = None;
        return None;
    }
    Some(paused)
}

fn refresh_now(app: &tauri::AppHandle) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let model = menu::model(app, pause_override(app));
    let (built, old_model, old_icon) = {
        let state = TRAY.lock();
        (state.built.clone(), state.model.clone(), state.icon)
    };

    // 菜单：结构变化（语言 / 固定窗口项 / 加速键）时重建，否则原地更新
    let mut new_built = built.clone();
    match (&built, &old_model) {
        (Some(b), Some(old)) if old.same_structure(&model) => {
            if *old != model
                && let Err(e) = menu::update_in_place(b, &model)
            {
                log::warn!("tray menu update failed: {e}");
            }
        }
        _ => match menu::build(app, &model) {
            Ok(b) => {
                if let Err(e) = tray.set_menu(Some(b.menu.clone())) {
                    log::warn!("tray set_menu failed: {e}");
                }
                new_built = Some(b);
            }
            Err(e) => log::warn!("tray menu build failed: {e}"),
        },
    }

    // 图标：底图（任务栏明暗）、状态图或圆点变化时才重新设置
    let key = icon_key(app, &model.status, None);
    if old_icon != Some(key) {
        set_icon(&tray, key);
    }
    let label_changed = old_model
        .as_ref()
        .is_none_or(|m| m.status != model.status || m.lang != model.lang);
    // macOS 的应用菜单与托盘同一时机跟随界面语言（语言没变时是空操作）
    crate::app::app_menu::refresh(app, model.lang);
    if label_changed {
        native::tray_set_accessibility(&tray, &tooltip(&model));
    }
    if label_changed && let Err(e) = tray.set_tooltip(Some(tooltip(&model))) {
        log::debug!("tray set_tooltip failed: {e}");
    }

    let mut state = TRAY.lock();
    state.built = new_built;
    state.model = Some(model);
    state.icon = Some(key);
}

fn set_icon(tray: &tauri::tray::TrayIcon, key: IconKey) {
    match icon::render(key) {
        Ok(img) => {
            if let Err(e) = tray.set_icon(Some(img)) {
                log::warn!("tray set_icon failed: {e}");
            }
            if let Err(e) = tray.set_icon_as_template(key.glyph.is_template()) {
                log::debug!("tray set_icon_as_template failed: {e}");
            }
        }
        Err(e) => log::warn!("tray icon render failed: {e}"),
    }
}

/// tooltip：正常时只显示 Wisp，异常时附上状态
fn tooltip(m: &MenuModel) -> String {
    let base = i18n::tr(m.lang, "tray.tooltip");
    match m.status {
        TrayStatus::Ok => base.to_owned(),
        ref s => format!("{base} — {}", s.label(m.lang)),
    }
}

fn current_glyph(app: &tauri::AppHandle) -> Glyph {
    let light = app
        .try_state::<AppState>()
        .and_then(|s| s.platform.screen.taskbar_light());
    Glyph::for_platform(OsKind::current(), light)
}

/// 当前图标：平台底图 + 状态（-paused / -attention）+ 圆点（`dot` 覆盖状态自带的圆点，闪烁时用）
fn icon_key(app: &tauri::AppHandle, status: &TrayStatus, dot: Option<Dot>) -> IconKey {
    let (variant, status_dot) = status.icon();
    IconKey {
        glyph: current_glyph(app),
        variant,
        dot: dot.or(status_dot),
    }
}

/// 托盘闪一次（疑似密钥被跳过时；设置里关闭后不闪）
pub fn flash(app: &tauri::AppHandle) {
    let enabled = app
        .try_state::<AppState>()
        .is_some_and(|s| s.settings.get().clipboard.secret_tray_flash);
    if !enabled {
        return;
    }
    let generation = FLASH_GEN.fetch_add(1, Ordering::AcqRel) + 1;
    let handle = app.clone();
    let scheduled = app.run_on_main_thread(move || {
        if let Some(tray) = handle.tray_by_id(TRAY_ID) {
            set_icon(&tray, icon_key(&handle, &current_status(&handle), Some(Dot::Flash)));
            // 记成「未知」，恢复时一定会重新设置图标
            TRAY.lock().icon = None;
        }
        let restore = handle.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(FLASH_DURATION).await;
            if FLASH_GEN.load(Ordering::Acquire) == generation {
                refresh(&restore);
            }
        });
    });
    if let Err(e) = scheduled {
        log::debug!("tray flash not scheduled: {e}");
    }
}

/// 点开托盘：刷新权限状态（托盘菜单打开时查一次）并惰性检查更新；这也是一次用户活动
/// （Windows 等待空闲安装更新时据此重新计时）
fn on_icon_event(app: &tauri::AppHandle, ev: &TrayIconEvent) {
    if let TrayIconEvent::Click {
        button_state: MouseButtonState::Down,
        ..
    } = ev
    {
        on_opened(app);
    }
}

/// 托盘菜单即将打开（鼠标按下或无障碍「按下」）
fn on_opened(app: &tauri::AppHandle) {
    crate::app::permissions::refresh(app);
    crate::updater::maybe_check(app);
    crate::updater::note_activity();
}

/// Linux 没有托盘时的退化：打开设置窗口并发一条通知
fn no_tray_fallback(app: &tauri::AppHandle) {
    if let Some(state) = app.try_state::<AppState>() {
        state.bus.send(crate::bus::Command::OpenWindow(WindowTarget::Settings {
            section: None,
            anchor: None,
        }));
    }
    let lang = menu::current_lang(app);
    if let Err(e) = app
        .notification()
        .builder()
        .title(i18n::tr(lang, "notify.no_tray.title"))
        .body(i18n::tr(lang, "notify.no_tray.body"))
        .show()
    {
        log::warn!("no-tray notification failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tooltip_mentions_problems_only() {
        let m = MenuModel {
            lang: i18n::Lang::EnUs,
            status: TrayStatus::Ok,
            paused: false,
            show_pinned: false,
            accelerators: Default::default(),
        };
        assert_eq!(tooltip(&m), "Wisp");
        let m = MenuModel {
            status: TrayStatus::HotkeyFailed { count: 2 },
            ..m
        };
        assert!(tooltip(&m).starts_with("Wisp — "));
    }

    /// tray-icon 走 vendor/tray-icon 的补丁（见 vendor/README.md）：Cargo.lock 里的 tray-icon 来自 vendor
    /// （没有 `source`）、版本与补丁一致，Windows 的离开检测定时器每一拍都核对光标（对照最后一次 WM_MOUSEMOVE 缓存的
    /// 图标矩形，不每拍跨进程问壳层）、离开即关、停住若干拍也关。升级 tauri / tray-icon 时这里失败：先看上游
    /// （tauri-apps/tray-icon）是否已修复——修复了就删除根 Cargo.toml `[patch.crates-io]` 的 tray-icon 与 vendor/tray-icon，
    /// 否则把补丁移植到新版本；两种情况都在 Windows 上跑 `scripts/perf/windows-tray-probe.ps1`（悬停、停住、移开之后
    /// UI 线程的唤醒，见脚本说明）
    #[test]
    fn tray_icon_is_patched_so_the_leave_timer_never_outlives_the_hover() {
        let lock = include_str!("../../../../../Cargo.lock");
        let tray = lock
            .split("[[package]]")
            .find(|p| p.contains("\nname = \"tray-icon\"\n"))
            .expect("Cargo.lock 里没有 tray-icon");
        assert!(
            tray.contains("\nversion = \"0.25.1\"\n"),
            "tray-icon 版本变了：核对上游是否已修复离开检测定时器，移植或删除 vendor/tray-icon 的补丁（见本测试注释）"
        );
        assert!(
            !tray.contains("\nsource = "),
            "tray-icon 没有走 vendor/tray-icon 的补丁：根 Cargo.toml 的 [patch.crates-io] 丢了？"
        );
        let windows = include_str!("../../../../../vendor/tray-icon/src/platform_impl/windows/mod.rs");
        let timer = windows
            .split("WM_TIMER if wparam as u32 == WM_USER_LEAVE_TIMER_ID")
            .nth(1)
            .and_then(|rest| rest.split("return 0;").next())
            .expect("vendor/tray-icon 没有离开检测定时器的处理");
        assert!(
            !timer.contains("if let Some(position) = userdata.last_position.take()"),
            "vendor/tray-icon 又只在第一拍核对光标（光标停住之后定时器漏关）"
        );
        assert!(
            timer.contains("if !inside {") && timer.contains("KillTimer(hwnd, WM_USER_LEAVE_TIMER_ID as _);"),
            "vendor/tray-icon 的离开检测定时器不再在光标离开时关闭"
        );
        assert!(
            timer.contains("userdata.leave_rect.is_some_and(") && !timer.contains("get_tray_rect("),
            "vendor/tray-icon 的离开检测定时器又在每一拍跨进程读图标矩形（应对照 WM_MOUSEMOVE 时缓存的矩形）"
        );
        assert!(
            timer.contains("if userdata.leave_ticks >= LEAVE_TIMER_STILL_TICKS {"),
            "vendor/tray-icon 的离开检测定时器在光标停在图标上时不再停表（会一直 64 次/秒）"
        );
        let still_ticks = windows
            .split("const LEAVE_TIMER_STILL_TICKS: u32 = ")
            .nth(1)
            .and_then(|rest| rest.split(';').next())
            .and_then(|n| n.trim().parse::<u32>().ok())
            .expect("vendor/tray-icon 没有 LEAVE_TIMER_STILL_TICKS");
        assert!(
            (1..=64).contains(&still_ticks),
            "停表前的拍数应在 1–64（约 1 s 以内）：{still_ticks}"
        );
        assert!(
            windows.contains("userdata.leave_rect = Some(icon_rect);") && windows.contains("userdata.leave_ticks = 0;"),
            "vendor/tray-icon 的 WM_MOUSEMOVE 不再缓存图标矩形、重置停表计数"
        );
    }
}
