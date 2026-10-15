//! macOS 的应用菜单。
//!
//! 设置 / 引导窗口打开时应用切到 Regular，菜单栏出现应用菜单。不设置时 Tauri 给的是默认菜单（Wisp / File / Edit / View /
//! Window / Help，全英文，About 打开系统通用关于面板，Quit 实际被 `prevent_exit` 拦下没有作用）。这里自建一份本地化的：
//!
//! ```text
//! Wisp：关于 Wisp（打开设置 · 关于）/ 设置… ⌘, / 隐藏 Wisp ⌘H / 隐藏其他 ⌥⌘H / 全部显示 / 退出 Wisp ⌘Q
//! 编辑：撤销 / 重做 / 剪切 / 拷贝 / 粘贴 / 全选（文本框需要这些键位，面板的输入框也靠它们）
//! 窗口：最小化 ⌘M / 关闭窗口 ⌘W（NSApp 的 windowsMenu，系统自动补上窗口列表）
//! ```
//!
//! - 只在 macOS 设置（Windows / Linux 上 `set_menu` 会给每个窗口加菜单栏，Tauri 在这两个平台也不建默认菜单）；
//! - 文案走 `app/i18n.rs`，界面语言变化时与托盘一起重建（[`refresh`] 由 `tray::refresh_now` 在主线程调用）；
//! - 「退出 Wisp ⌘Q」只在设置 / 引导窗口存在（菜单栏可见）时退出：面板是非激活窗口，按键焦点在面板里时用户看到的是
//!   别的应用的菜单栏，⌘Q 多半是想退出那个应用，这时什么也不做（与原来默认菜单被 `prevent_exit` 拦下的效果一致）；
//! - 菜单项 id 都以 `app_menu.` 开头：Tauri 的菜单事件回调会收到所有菜单（含托盘）的事件，托盘的回调忽略这个前缀。

use parking_lot::Mutex;
use tauri::Manager;
use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use wisp_types::{OsKind, WindowTarget};

use crate::app::i18n::{self, Lang};
use crate::bus::Command;
use crate::state::AppState;
use crate::windows::settings_window::{ONBOARDING_LABEL, SETTINGS_LABEL};

/// 菜单项 id 的前缀
pub const ID_PREFIX: &str = "app_menu.";
const ID_ABOUT: &str = "app_menu.about";
const ID_SETTINGS: &str = "app_menu.settings";
const ID_QUIT: &str = "app_menu.quit";

/// 当前菜单的语言（相同则不重建）
static BUILT: Mutex<Option<Lang>> = Mutex::new(None);

/// 是否需要应用菜单（只有 macOS）
fn supported() -> bool {
    OsKind::current() == OsKind::Macos
}

/// 安装应用菜单并注册事件回调（setup 中、主线程上调用一次）
pub fn install(app: &tauri::AppHandle, lang: Lang) {
    if !supported() {
        return;
    }
    app.on_menu_event(|app, ev| on_event(app, &ev));
    rebuild(app, lang);
}

/// 界面语言变化时重建（主线程上调用；语言没变时什么也不做）
pub fn refresh(app: &tauri::AppHandle, lang: Lang) {
    if !supported() || *BUILT.lock() == Some(lang) {
        return;
    }
    rebuild(app, lang);
}

fn rebuild(app: &tauri::AppHandle, lang: Lang) {
    match build(app, lang) {
        Ok(menu) => match app.set_menu(menu) {
            Ok(_) => *BUILT.lock() = Some(lang),
            Err(e) => log::warn!("app menu not set: {e}"),
        },
        Err(e) => log::warn!("app menu build failed: {e}"),
    }
}

fn build(app: &tauri::AppHandle, lang: Lang) -> tauri::Result<Menu<tauri::Wry>> {
    let t = |key| i18n::tr(lang, key);
    let sep = || PredefinedMenuItem::separator(app);
    let app_menu = Submenu::with_items(
        app,
        "Wisp",
        true,
        &[
            &MenuItem::with_id(app, ID_ABOUT, t("app_menu.about"), true, None::<&str>)?,
            &sep()?,
            &MenuItem::with_id(app, ID_SETTINGS, t("app_menu.settings"), true, Some("CmdOrCtrl+,"))?,
            &sep()?,
            &PredefinedMenuItem::hide(app, Some(t("app_menu.hide")))?,
            &PredefinedMenuItem::hide_others(app, Some(t("app_menu.hide_others")))?,
            &PredefinedMenuItem::show_all(app, Some(t("app_menu.show_all")))?,
            &sep()?,
            &MenuItem::with_id(app, ID_QUIT, t("app_menu.quit"), true, Some("CmdOrCtrl+Q"))?,
        ],
    )?;
    let edit = Submenu::with_items(
        app,
        t("app_menu.edit"),
        true,
        &[
            &PredefinedMenuItem::undo(app, Some(t("app_menu.undo")))?,
            &PredefinedMenuItem::redo(app, Some(t("app_menu.redo")))?,
            &sep()?,
            &PredefinedMenuItem::cut(app, Some(t("app_menu.cut")))?,
            &PredefinedMenuItem::copy(app, Some(t("app_menu.copy")))?,
            &PredefinedMenuItem::paste(app, Some(t("app_menu.paste")))?,
            &PredefinedMenuItem::select_all(app, Some(t("app_menu.select_all")))?,
        ],
    )?;
    let window = Submenu::with_items(
        app,
        t("app_menu.window"),
        true,
        &[
            &PredefinedMenuItem::minimize(app, Some(t("app_menu.minimize")))?,
            &PredefinedMenuItem::close_window(app, Some(t("app_menu.close_window")))?,
        ],
    )?;
    crate::windows::native::mark_windows_menu(&window);
    Menu::with_items(app, &[&app_menu, &edit, &window])
}

/// 设置或引导窗口存在（应用此时是 Regular，菜单栏可见）
fn menu_bar_visible(app: &tauri::AppHandle) -> bool {
    app.get_webview_window(SETTINGS_LABEL).is_some() || app.get_webview_window(ONBOARDING_LABEL).is_some()
}

fn on_event(app: &tauri::AppHandle, ev: &MenuEvent) {
    let id = ev.id().as_ref();
    if !id.starts_with(ID_PREFIX) {
        return;
    }
    log::info!("app menu item={id}");
    let cmd = match id {
        ID_ABOUT => Command::OpenWindow(WindowTarget::Settings {
            section: Some(super::sections::ABOUT.to_owned()),
            anchor: None,
        }),
        ID_SETTINGS => Command::OpenWindow(WindowTarget::Settings {
            section: None,
            anchor: None,
        }),
        ID_QUIT if menu_bar_visible(app) => Command::Quit,
        ID_QUIT => {
            log::debug!("app menu quit ignored: no settings / onboarding window (the menu bar belongs to another app)");
            return;
        }
        _ => return,
    };
    match app.try_state::<AppState>() {
        Some(state) => state.bus.send(cmd),
        None if cmd == Command::Quit => app.exit(0),
        None => {}
    }
}
