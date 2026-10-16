//! 托盘菜单的结构、状态行与事件分发。
//!
//! 菜单结构（快捷键只用于显示，取 `HotkeyStatus.effective`，切换语言后整份重建）：
//! ```text
//! ● 状态行（连续粘贴进行中 > 权限缺失 > 快捷键失败 > 记录已暂停 > 同步需要处理 > 有新版本 > 正常；点击后各自跳转，
//!   连续粘贴的点击即结束它）
//! ─────
//! 划词翻译 ⌥D / 翻译输入… ⌥⇧D / 截图 OCR ⌥S / 截图直接翻译 ⌥⇧S / 剪贴板 ⌥V
//! 显示固定窗口（固定窗口数 > 0 时）
//! ─────
//! ☐ 暂停剪贴板记录 / 清空剪贴板历史…（原生确认对话框）
//! ─────
//! 立即同步（开启了同步时） / 设置… ⌘, / 使用引导… / 检查更新
//! ─────
//! 退出
//! ```
//! 菜单项只投递 `bus::Command`（打开窗口、动作、暂停 / 清空剪贴板、退出）；「显示固定窗口」调用
//! `WindowManager::show_all_pinned`，「立即同步」调用 `SyncService::sync_now`，「检查更新」调用
//! `updater::check_interactive`（应用核心自己的服务）。

use tauri::Manager;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, IconMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind, MessageDialogResult};
use wisp_types::{
    HotkeyAction, HotkeyState, HotkeyStatus, HotkeyTarget, OsKind, PermissionSet, PermissionState, UpdateState,
    UpdateStatus, WindowTarget,
};

use crate::app::i18n::{self, Lang};
use crate::app::sections;
use crate::bus::{Command, TriggerOrigin};
use crate::state::AppState;

use super::icon::{Dot, Variant};

/// 菜单项 id
pub mod ids {
    pub const STATUS: &str = "status";
    pub const TRANSLATE_SELECTION: &str = "translate_selection";
    pub const TRANSLATE_INPUT: &str = "translate_input";
    pub const OCR: &str = "ocr";
    pub const OCR_TRANSLATE: &str = "ocr_translate";
    pub const CLIPBOARD: &str = "clipboard";
    pub const SHOW_PINNED: &str = "show_pinned";
    pub const PAUSE_CLIPBOARD: &str = "pause_clipboard";
    pub const CLEAR_CLIPBOARD: &str = "clear_clipboard";
    pub const SETTINGS: &str = "settings";
    pub const ONBOARDING: &str = "onboarding";
    pub const SYNC_NOW: &str = "sync_now";
    pub const CHECK_UPDATE: &str = "check_update";
    pub const QUIT: &str = "quit";
}

/// 状态行（优先级：连续粘贴进行中 > 权限缺失 > 快捷键失败 > 记录已暂停 > 同步需要处理 > 有新版本 > 正常）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayStatus {
    /// 连续粘贴进行中（粘贴键正被拦下）：最先显示，点击即结束——用户随时看得到、也随时停得下来
    Queue {
        index: u32,
        total: u32,
    },
    /// 缺少权限；`after_update` = 更新后授权失效
    Permission {
        after_update: bool,
    },
    /// 有快捷键未能注册；`count` = 失败的个数（「N 个快捷键未生效」）
    HotkeyFailed {
        count: usize,
    },
    Paused,
    /// 同步遇到需要用户处理的错误（密码错误、服务器数据被清除等），自动同步已暂停。暂时的连接失败不算：
    /// 它会在下一次使用时自动重试，不值得占用状态行
    SyncAttention,
    /// 有新版本；`ready` = 已下载安装、重启后生效
    Update {
        version: String,
        ready: bool,
    },
    Ok,
}

impl TrayStatus {
    /// 托盘图标的状态：权限缺失 / 快捷键失败 → `-attention`，暂停 → `-paused`，有新版本 → 正常图 + 合成的蓝点。
    /// 同步需要处理只在菜单的状态行里用状态点提示：Wisp 的核心功能都还正常，不值得把托盘图标换成警示
    pub const fn icon(&self) -> (Variant, Option<Dot>) {
        match self {
            Self::Permission { .. } | Self::HotkeyFailed { .. } => (Variant::Attention, None),
            Self::Queue { .. } => (Variant::Normal, Some(Dot::Update)),
            Self::Paused => (Variant::Paused, None),
            Self::Update { .. } => (Variant::Normal, Some(Dot::Update)),
            Self::SyncAttention | Self::Ok => (Variant::Normal, None),
        }
    }

    /// 状态行文案
    pub fn label(&self, lang: Lang) -> String {
        match self {
            Self::Permission { after_update: false } => i18n::tr(lang, "tray.status.permission").to_owned(),
            Self::Permission { after_update: true } => i18n::tr(lang, "tray.status.permission_after_update").to_owned(),
            Self::HotkeyFailed { count } => {
                let key = if *count == 1 {
                    "tray.status.hotkey_failed_one"
                } else {
                    "tray.status.hotkey_failed"
                };
                i18n::tr_args(lang, key, &[("count", &count.to_string())])
            }
            Self::Queue { index, total } => i18n::tr_args(
                lang,
                "tray.status.queue",
                &[("index", &index.to_string()), ("total", &total.to_string())],
            ),
            Self::Paused => i18n::tr(lang, "tray.status.paused").to_owned(),
            Self::SyncAttention => i18n::tr(lang, "tray.status.sync_attention").to_owned(),
            Self::Update { version, ready: false } => {
                i18n::tr_args(lang, "tray.status.update", &[("version", version)])
            }
            Self::Update { version, ready: true } => {
                i18n::tr_args(lang, "tray.status.update_ready", &[("version", version)])
            }
            Self::Ok => i18n::tr(lang, "tray.status.ok").to_owned(),
        }
    }

    /// 状态行是否可点击（正常状态只是说明，不可点）
    pub const fn actionable(&self) -> bool {
        !matches!(self, Self::Ok)
    }

    /// 状态点颜色
    const fn dot(&self) -> [u8; 3] {
        match self {
            Self::Permission { .. } => [0xEF, 0x44, 0x44],
            Self::HotkeyFailed { .. } | Self::SyncAttention => [0xF5, 0x9E, 0x0B],
            Self::Paused => [0x9C, 0xA3, 0xAF],
            Self::Update { .. } | Self::Queue { .. } => [0x3B, 0x82, 0xF6],
            Self::Ok => [0x22, 0xC5, 0x5E],
        }
    }
}

/// 计算状态行的输入
#[derive(Debug, Clone)]
pub struct StatusInputs<'a> {
    pub permissions: PermissionSet,
    /// 是否开启了双击修饰键（只有这时才需要输入监控权限）
    pub needs_input_monitoring: bool,
    pub hotkeys: &'a [HotkeyStatus],
    pub clipboard_paused: bool,
    /// 同步有需要用户处理的错误（`SyncService::needs_attention`）
    pub sync_attention: bool,
    pub update: &'a UpdateStatus,
    /// 连续粘贴的进度 (已粘贴, 总条数)；没有进行中的队列时为 None
    pub queue: Option<(u32, u32)>,
}

fn missing(state: PermissionState) -> bool {
    matches!(state, PermissionState::Denied | PermissionState::NotDetermined)
}

/// 按优先级得出状态行（纯函数）
pub fn status_of(i: &StatusInputs<'_>) -> TrayStatus {
    if let Some((index, total)) = i.queue {
        return TrayStatus::Queue { index, total };
    }
    let p = &i.permissions;
    if p.lost_after_update {
        return TrayStatus::Permission { after_update: true };
    }
    if missing(p.accessibility)
        || missing(p.screen_recording)
        || (i.needs_input_monitoring && missing(p.input_monitoring))
    {
        return TrayStatus::Permission { after_update: false };
    }
    let failed = i.hotkeys.iter().filter(|h| h.state == HotkeyState::Failed).count();
    if failed > 0 {
        return TrayStatus::HotkeyFailed { count: failed };
    }
    if i.clipboard_paused {
        return TrayStatus::Paused;
    }
    if i.sync_attention {
        return TrayStatus::SyncAttention;
    }
    // 检查进行中、检查 / 下载失败时，更新器仍带着已知新版本的 version：「有新版本」不因一次检查而消失
    match (i.update.state, &i.update.version) {
        (UpdateState::Available | UpdateState::Downloading | UpdateState::Checking | UpdateState::Error, Some(v)) => {
            TrayStatus::Update {
                version: v.clone(),
                ready: false,
            }
        }
        (UpdateState::Ready, Some(v)) => TrayStatus::Update {
            version: v.clone(),
            ready: true,
        },
        _ => TrayStatus::Ok,
    }
}

/// 状态行被点击时要做的事
pub fn status_command(status: &TrayStatus) -> Option<Command> {
    match status {
        // 状态行「权限缺失」直达引导第 1 步：引导的 1 是权限页（非 macOS 由前端落到
        // 接入模型）
        TrayStatus::Permission { .. } => Some(Command::OpenWindow(WindowTarget::Onboarding { step: Some(1) })),
        TrayStatus::HotkeyFailed { .. } => Some(Command::OpenWindow(WindowTarget::Settings {
            section: Some(sections::HOTKEYS.to_owned()),
            anchor: None,
        })),
        TrayStatus::Paused => Some(Command::ToggleClipboardPause),
        TrayStatus::SyncAttention => Some(Command::OpenWindow(WindowTarget::Settings {
            section: Some(sections::SYNC.to_owned()),
            anchor: None,
        })),
        TrayStatus::Update { .. } => Some(Command::OpenWindow(WindowTarget::Settings {
            section: Some(sections::ABOUT.to_owned()),
            anchor: None,
        })),
        // 连续粘贴的结束由菜单事件直接交给剪贴板服务（`on_event`），不经总线
        TrayStatus::Queue { .. } | TrayStatus::Ok => None,
    }
}

/// 构建菜单所需的全部状态
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuModel {
    pub lang: Lang,
    pub status: TrayStatus,
    pub paused: bool,
    pub show_pinned: bool,
    /// 开启了同步（显示「立即同步」）
    pub sync_enabled: bool,
    /// 五个动作的实际组合键（global-hotkey 规范串，用作菜单加速键显示；未绑定 / 失败为 None）
    pub accelerators: [Option<String>; 5],
}

impl MenuModel {
    /// 结构相同（语言、固定窗口项、立即同步项、加速键），可以原地更新而不必重建
    pub fn same_structure(&self, other: &Self) -> bool {
        self.lang == other.lang
            && self.show_pinned == other.show_pinned
            && self.sync_enabled == other.sync_enabled
            && self.accelerators == other.accelerators
    }
}

/// 五个动作的加速键（只取已生效的绑定）
pub fn accelerators_of(hotkeys: &[HotkeyStatus]) -> [Option<String>; 5] {
    HotkeyAction::ALL.map(|action| {
        hotkeys
            .iter()
            .find(|h| matches!(&h.target, HotkeyTarget::Action { action: a } if *a == action))
            .filter(|h| matches!(h.state, HotkeyState::Registered | HotkeyState::Fallback))
            .and_then(|h| h.effective.clone())
    })
}

/// 读取当前状态；`paused_override` 为点击「暂停」后、剪贴板服务处理完之前的乐观值
pub fn model(app: &tauri::AppHandle, paused_override: Option<bool>) -> MenuModel {
    let lang = current_lang(app);
    let Some(state) = app.try_state::<AppState>() else {
        return MenuModel {
            lang,
            status: TrayStatus::Ok,
            paused: false,
            show_pinned: false,
            sync_enabled: false,
            accelerators: Default::default(),
        };
    };
    let hotkeys = state.hotkeys.status();
    let paused = paused_override.unwrap_or_else(|| state.clipboard.paused());
    let update = crate::updater::status(app).unwrap_or_default();
    let settings = state.settings.get();
    let status = status_of(&StatusInputs {
        permissions: crate::app::permissions::cached(app),
        needs_input_monitoring: settings.hotkeys.double_tap.enabled,
        hotkeys: &hotkeys,
        clipboard_paused: paused,
        sync_attention: state.sync.needs_attention(),
        update: &update,
        queue: state.clipboard.queue_status().map(|q| (q.index, q.total)),
    });
    MenuModel {
        lang,
        status,
        paused,
        show_pinned: app.webview_windows().keys().any(|l| l.starts_with("pinned-")),
        // 跟着设置走而不是服务的就绪状态：开启后本机部分在后台准备，期间点击只是什么也不做；设置变化本来就会刷新托盘
        sync_enabled: settings.sync.enabled,
        accelerators: accelerators_of(&hotkeys),
    }
}

/// 当前界面语言
pub fn current_lang(app: &tauri::AppHandle) -> Lang {
    let pref = app
        .try_state::<AppState>()
        .map(|s| s.settings.get().general.ui_language)
        .unwrap_or_default();
    i18n::resolve(pref)
}

/// 构建好的菜单与需要原地更新的项
#[derive(Clone)]
pub struct BuiltMenu {
    pub menu: Menu<tauri::Wry>,
    pub status: IconMenuItem<tauri::Wry>,
    pub pause: CheckMenuItem<tauri::Wry>,
}

/// 状态行左侧的彩色圆点（16 × 16 @2x）
pub fn status_dot(status: &TrayStatus) -> Image<'static> {
    const SIZE: u32 = 32;
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    let c = f64::from(SIZE) / 2.0;
    let r = f64::from(SIZE) * 0.2;
    let color = status.dot();
    for y in 0..SIZE {
        for x in 0..SIZE {
            let d = ((f64::from(x) + 0.5 - c).powi(2) + (f64::from(y) + 0.5 - c).powi(2)).sqrt();
            let cover = (r + 0.5 - d).clamp(0.0, 1.0);
            if cover > 0.0 {
                let i = ((y * SIZE + x) * 4) as usize;
                rgba[i..i + 3].copy_from_slice(&color);
                rgba[i + 3] = (cover * 255.0).round() as u8;
            }
        }
    }
    Image::new_owned(rgba, SIZE, SIZE)
}

/// 构建菜单
pub fn build(app: &tauri::AppHandle, m: &MenuModel) -> tauri::Result<BuiltMenu> {
    let t = |key| i18n::tr(m.lang, key);
    let sep = || PredefinedMenuItem::separator(app);
    let status = IconMenuItem::with_id(
        app,
        ids::STATUS,
        m.status.label(m.lang),
        m.status.actionable(),
        Some(status_dot(&m.status)),
        None::<&str>,
    )?;
    let action_ids = [
        (ids::TRANSLATE_SELECTION, "tray.translate_selection"),
        (ids::TRANSLATE_INPUT, "tray.translate_input"),
        (ids::OCR, "tray.ocr"),
        (ids::OCR_TRANSLATE, "tray.ocr_translate"),
        (ids::CLIPBOARD, "tray.clipboard"),
    ];
    let mut actions = Vec::with_capacity(action_ids.len());
    for ((id, key), accel) in action_ids.into_iter().zip(&m.accelerators) {
        actions.push(action_item(app, id, t(key), accel.as_deref())?);
    }
    let show_pinned = MenuItem::with_id(app, ids::SHOW_PINNED, t("tray.show_pinned"), true, None::<&str>)?;
    let pause = CheckMenuItem::with_id(
        app,
        ids::PAUSE_CLIPBOARD,
        t("tray.pause_clipboard"),
        true,
        m.paused,
        None::<&str>,
    )?;
    let clear = MenuItem::with_id(app, ids::CLEAR_CLIPBOARD, t("tray.clear_clipboard"), true, None::<&str>)?;
    // ⌘, / ⌘Q 是 macOS 菜单的惯例显示；Windows / Linux 的托盘菜单里显示 Ctrl+, 反而会让人以为它是全局快捷键
    let mac = OsKind::current() == OsKind::Macos;
    let settings = action_item(app, ids::SETTINGS, t("tray.settings"), mac.then_some("Super+Comma"))?;
    let onboarding = MenuItem::with_id(app, ids::ONBOARDING, t("tray.onboarding"), true, None::<&str>)?;
    let sync_now = MenuItem::with_id(app, ids::SYNC_NOW, t("tray.sync_now"), true, None::<&str>)?;
    let check_update = MenuItem::with_id(app, ids::CHECK_UPDATE, t("tray.check_update"), true, None::<&str>)?;
    let quit = action_item(app, ids::QUIT, t("tray.quit"), mac.then_some("Super+KeyQ"))?;

    let menu = Menu::new(app)?;
    menu.append(&status)?;
    menu.append(&sep()?)?;
    for item in &actions {
        menu.append(item)?;
    }
    if m.show_pinned {
        menu.append(&show_pinned)?;
    }
    menu.append(&sep()?)?;
    menu.append(&pause)?;
    menu.append(&clear)?;
    menu.append(&sep()?)?;
    if m.sync_enabled {
        menu.append(&sync_now)?;
    }
    menu.append(&settings)?;
    menu.append(&onboarding)?;
    menu.append(&check_update)?;
    menu.append(&sep()?)?;
    menu.append(&quit)?;
    Ok(BuiltMenu { menu, status, pause })
}

/// 带加速键（只用于显示）的菜单项：加速键解析失败（例如平台不支持的键名）时退回无加速键
fn action_item(
    app: &tauri::AppHandle,
    id: &str,
    text: &str,
    accel: Option<&str>,
) -> tauri::Result<MenuItem<tauri::Wry>> {
    if let Some(accel) = accel
        && let Ok(item) = MenuItem::with_id(app, id, text, true, Some(accel))
    {
        return Ok(item);
    }
    MenuItem::with_id(app, id, text, true, None::<&str>)
}

/// 原地更新状态行与暂停勾选（结构不变时；菜单正打开时也能看到变化）
pub fn update_in_place(built: &BuiltMenu, m: &MenuModel) -> tauri::Result<()> {
    built.status.set_text(m.status.label(m.lang))?;
    built.status.set_enabled(m.status.actionable())?;
    built.status.set_icon(Some(status_dot(&m.status)))?;
    if built.pause.is_checked()? != m.paused {
        built.pause.set_checked(m.paused)?;
    }
    Ok(())
}

/// 菜单事件
pub fn on_event(app: &tauri::AppHandle, ev: &MenuEvent) {
    let id = ev.id().as_ref();
    // 菜单事件回调收到所有菜单的事件：应用菜单（macOS）的项由 app::app_menu 处理
    if id.starts_with(crate::app::app_menu::ID_PREFIX) {
        return;
    }
    log::info!("tray menu item={id}");
    let action = |action| Command::Action {
        action,
        origin: TriggerOrigin::Tray,
    };
    let cmd = match id {
        ids::STATUS => match super::current_status(app) {
            TrayStatus::Queue { .. } => {
                if let Some(state) = app.try_state::<AppState>() {
                    state.clipboard.queue_cancel_later();
                }
                return;
            }
            status => match status_command(&status) {
                Some(cmd) => cmd,
                None => return,
            },
        },
        ids::TRANSLATE_SELECTION => action(HotkeyAction::TranslateSelection),
        ids::TRANSLATE_INPUT => action(HotkeyAction::TranslateInput),
        ids::OCR => action(HotkeyAction::Ocr),
        ids::OCR_TRANSLATE => action(HotkeyAction::OcrTranslate),
        ids::CLIPBOARD => action(HotkeyAction::Clipboard),
        ids::SHOW_PINNED => {
            if let Some(state) = app.try_state::<AppState>() {
                state.windows.show_all_pinned();
            }
            return;
        }
        ids::PAUSE_CLIPBOARD => Command::ToggleClipboardPause,
        ids::CLEAR_CLIPBOARD => {
            confirm_clear(app);
            return;
        }
        ids::SETTINGS => Command::OpenWindow(WindowTarget::Settings {
            section: None,
            anchor: None,
        }),
        ids::ONBOARDING => Command::OpenWindow(WindowTarget::Onboarding { step: None }),
        ids::SYNC_NOW => {
            // 成功不打扰；进度与结果在设置页的状态行，需要处理的错误回到托盘状态行
            if let Some(state) = app.try_state::<AppState>()
                && let Err(e) = state.sync.sync_now()
            {
                log::info!("tray sync not started: {}", e.message);
            }
            return;
        }
        ids::CHECK_UPDATE => {
            crate::updater::check_interactive(app);
            return;
        }
        ids::QUIT => Command::Quit,
        _ => return,
    };
    // 暂停由剪贴板服务异步执行：**发送之前**读出当前值算好目标值（发送之后再读，服务可能已经切换完，乐观值就反了）
    let pause_target = (cmd == Command::ToggleClipboardPause)
        .then(|| app.try_state::<AppState>().map(|s| !s.clipboard.paused()))
        .flatten();
    match app.try_state::<AppState>() {
        Some(state) => state.bus.send(cmd),
        // 状态尚未就绪时至少保证能退出
        None if cmd == Command::Quit => app.exit(0),
        None => {}
    }
    if let Some(paused) = pause_target {
        // 先按目标值乐观地刷新状态行与勾选，服务处理完后的刷新给出最终状态
        super::refresh_with_pause_hint(app, paused);
    }
}

/// 清空剪贴板历史前的原生确认框：[清空（保留收藏）]（默认）/ [全部清空] / [取消]。同步剪贴板历史时注明其他设备上的
/// 记录也会一并清除（清空会同步到其他设备）
fn confirm_clear(app: &tauri::AppHandle) {
    let lang = current_lang(app);
    let t = |key| i18n::tr(lang, key);
    let handle = app.clone();
    let keep = t("dialog.clear_clipboard.ok").to_owned();
    let all = t("dialog.clear_clipboard.all").to_owned();
    let synced = app.try_state::<AppState>().is_some_and(|s| {
        let sync = &s.settings.get().sync;
        sync.enabled && sync.content.clipboard
    });
    let message = if synced {
        "dialog.clear_clipboard.message_synced"
    } else {
        "dialog.clear_clipboard.message"
    };
    app.dialog()
        .message(t(message))
        .title(t("dialog.clear_clipboard.title"))
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::YesNoCancelCustom(
            keep.clone(),
            all.clone(),
            t("dialog.cancel").to_owned(),
        ))
        .show_with_result(move |result| {
            if let Some(keep_pinned) = clear_choice(&result, &keep, &all)
                && let Some(state) = handle.try_state::<AppState>()
            {
                state.bus.send(Command::ClearClipboard { keep_pinned });
            }
        });
}

/// 确认框的结果 → `keep_pinned`（None = 取消）。三个自定义按钮时插件按按钮文字回传（`Custom(label)`），
/// 某些后端回传 Yes / No
fn clear_choice(result: &MessageDialogResult, keep: &str, all: &str) -> Option<bool> {
    match result {
        MessageDialogResult::Yes | MessageDialogResult::Ok => Some(true),
        MessageDialogResult::No => Some(false),
        MessageDialogResult::Custom(label) if label == keep => Some(true),
        MessageDialogResult::Custom(label) if label == all => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use wisp_types::Binding;

    use super::*;

    #[test]
    fn hotkey_failures_show_their_count() {
        assert_eq!(
            TrayStatus::HotkeyFailed { count: 2 }.label(Lang::ZhCn),
            "2 个快捷键未生效 · 点击查看"
        );
        assert_eq!(
            TrayStatus::HotkeyFailed { count: 1 }.label(Lang::EnUs),
            "1 shortcut isn't working · Click to review"
        );
        assert_eq!(
            TrayStatus::HotkeyFailed { count: 3 }.label(Lang::EnUs),
            "3 shortcuts aren't working · Click to review"
        );
    }

    #[test]
    fn clear_dialog_offers_keep_pinned_clear_all_and_cancel() {
        let (keep, all) = ("Clear (Keep Favorites)", "Clear All");
        let c = |r: MessageDialogResult| clear_choice(&r, keep, all);
        assert_eq!(c(MessageDialogResult::Custom(keep.to_owned())), Some(true));
        assert_eq!(c(MessageDialogResult::Custom(all.to_owned())), Some(false));
        assert_eq!(c(MessageDialogResult::Custom("Cancel".to_owned())), None);
        assert_eq!(c(MessageDialogResult::Yes), Some(true));
        assert_eq!(c(MessageDialogResult::No), Some(false));
        assert_eq!(c(MessageDialogResult::Cancel), None);
    }

    fn hk(action: HotkeyAction, state: HotkeyState, effective: Option<&str>) -> HotkeyStatus {
        HotkeyStatus {
            target: HotkeyTarget::Action { action },
            binding: Binding::default_binding(),
            effective: effective.map(str::to_owned),
            display: None,
            state,
            reason_key: None,
        }
    }

    fn granted() -> PermissionSet {
        PermissionSet {
            accessibility: PermissionState::Granted,
            screen_recording: PermissionState::Granted,
            input_monitoring: PermissionState::NotDetermined,
            lost_after_update: false,
        }
    }

    fn inputs<'a>(hotkeys: &'a [HotkeyStatus], update: &'a UpdateStatus) -> StatusInputs<'a> {
        StatusInputs {
            permissions: granted(),
            needs_input_monitoring: false,
            hotkeys,
            clipboard_paused: false,
            sync_attention: false,
            update,
            queue: None,
        }
    }

    #[test]
    fn status_line_priority_order() {
        let failed = [hk(HotkeyAction::Ocr, HotkeyState::Failed, None)];
        let update = UpdateStatus {
            state: UpdateState::Available,
            version: Some("0.2.0".to_owned()),
            ..UpdateStatus::default()
        };
        let mut i = inputs(&failed, &update);
        i.clipboard_paused = true;
        i.permissions.accessibility = PermissionState::Denied;
        assert_eq!(status_of(&i), TrayStatus::Permission { after_update: false });
        i.permissions = granted();
        assert_eq!(status_of(&i), TrayStatus::HotkeyFailed { count: 1 });
        i.hotkeys = &[];
        i.sync_attention = true;
        assert_eq!(status_of(&i), TrayStatus::Paused);
        i.clipboard_paused = false;
        assert_eq!(status_of(&i), TrayStatus::SyncAttention);
        i.sync_attention = false;
        assert_eq!(
            status_of(&i),
            TrayStatus::Update {
                version: "0.2.0".to_owned(),
                ready: false
            }
        );
        let none = UpdateStatus::default();
        i.update = &none;
        assert_eq!(status_of(&i), TrayStatus::Ok);
        // 连续粘贴进行中排在最前（粘贴键正被拦下，用户要随时看得到、停得下来）
        i.permissions.accessibility = PermissionState::Denied;
        i.queue = Some((2, 5));
        let queue = status_of(&i);
        assert_eq!(queue, TrayStatus::Queue { index: 2, total: 5 });
        assert!(queue.actionable());
        assert_eq!(status_command(&queue), None, "点击由菜单事件直接结束队列");
        assert_eq!(queue.label(Lang::ZhCn), "连续粘贴 2 / 5 · 点击结束");
    }

    #[test]
    fn permission_rules() {
        let none = UpdateStatus::default();
        let mut i = inputs(&[], &none);
        // 非 macOS：not_applicable 不算缺失
        i.permissions = PermissionSet::not_applicable();
        assert_eq!(status_of(&i), TrayStatus::Ok);
        // 输入监控只在开启双击修饰键时才需要
        i.permissions = granted();
        assert_eq!(status_of(&i), TrayStatus::Ok);
        i.needs_input_monitoring = true;
        assert_eq!(status_of(&i), TrayStatus::Permission { after_update: false });
        // 更新后授权失效优先
        i.permissions.lost_after_update = true;
        assert_eq!(status_of(&i), TrayStatus::Permission { after_update: true });
    }

    #[test]
    fn unavailable_and_fallback_hotkeys_are_not_failures() {
        let none = UpdateStatus::default();
        let list = [
            hk(HotkeyAction::Ocr, HotkeyState::Unavailable, None),
            hk(HotkeyAction::Clipboard, HotkeyState::Fallback, Some("Alt+Shift+KeyV")),
        ];
        assert_eq!(status_of(&inputs(&list, &none)), TrayStatus::Ok);
    }

    #[test]
    fn known_update_survives_checks_and_failures() {
        // 检查进行中 / 检查失败时更新器仍带着已知新版本：托盘的「有新版本」不消失
        for state in [UpdateState::Checking, UpdateState::Error, UpdateState::Downloading] {
            let update = UpdateStatus {
                state,
                version: Some("0.3.0".to_owned()),
                ..UpdateStatus::default()
            };
            assert_eq!(
                status_of(&inputs(&[], &update)),
                TrayStatus::Update {
                    version: "0.3.0".to_owned(),
                    ready: false
                },
                "{state:?}"
            );
        }
        // 没有已知新版本时的检查中 / 失败：正常
        for state in [UpdateState::Checking, UpdateState::Error, UpdateState::UpToDate] {
            let update = UpdateStatus {
                state,
                ..UpdateStatus::default()
            };
            assert_eq!(status_of(&inputs(&[], &update)), TrayStatus::Ok, "{state:?}");
        }
    }

    #[test]
    fn update_ready_and_icons() {
        let ready = UpdateStatus {
            state: UpdateState::Ready,
            version: Some("1.0.0".to_owned()),
            ..UpdateStatus::default()
        };
        let s = status_of(&inputs(&[], &ready));
        assert_eq!(
            s,
            TrayStatus::Update {
                version: "1.0.0".to_owned(),
                ready: true
            }
        );
        assert_eq!(s.icon(), (Variant::Normal, Some(Dot::Update)));
        assert_eq!(TrayStatus::Paused.icon(), (Variant::Paused, None));
        assert_eq!(TrayStatus::HotkeyFailed { count: 2 }.icon(), (Variant::Attention, None));
        assert_eq!(
            TrayStatus::Permission { after_update: true }.icon(),
            (Variant::Attention, None)
        );
        assert_eq!(TrayStatus::Ok.icon(), (Variant::Normal, None));
        // 同步需要处理只用状态点，托盘图标不变
        assert_eq!(TrayStatus::SyncAttention.icon(), (Variant::Normal, None));
        assert!(TrayStatus::SyncAttention.actionable());
        assert!(!TrayStatus::Ok.actionable());
        assert!(s.label(Lang::ZhCn).contains("1.0.0"));
    }

    #[test]
    fn status_clicks_jump_to_the_right_place() {
        assert_eq!(
            status_command(&TrayStatus::HotkeyFailed { count: 1 }),
            Some(Command::OpenWindow(WindowTarget::Settings {
                section: Some("hotkeys".to_owned()),
                anchor: None,
            }))
        );
        assert_eq!(status_command(&TrayStatus::Paused), Some(Command::ToggleClipboardPause));
        assert_eq!(
            status_command(&TrayStatus::SyncAttention),
            Some(Command::OpenWindow(WindowTarget::Settings {
                section: Some("sync".to_owned()),
                anchor: None,
            }))
        );
        assert_eq!(
            status_command(&TrayStatus::Update {
                version: "1".to_owned(),
                ready: false
            }),
            Some(Command::OpenWindow(WindowTarget::Settings {
                section: Some("about".to_owned()),
                anchor: None,
            }))
        );
        assert_eq!(
            status_command(&TrayStatus::Permission { after_update: false }),
            Some(Command::OpenWindow(WindowTarget::Onboarding { step: Some(1) })),
            "直达权限页"
        );
        assert_eq!(status_command(&TrayStatus::Ok), None);
    }

    #[test]
    fn accelerators_follow_effective_bindings_in_menu_order() {
        let list = [
            hk(HotkeyAction::Clipboard, HotkeyState::Registered, Some("Alt+KeyV")),
            hk(
                HotkeyAction::TranslateSelection,
                HotkeyState::Fallback,
                Some("Alt+Shift+KeyD"),
            ),
            hk(HotkeyAction::Ocr, HotkeyState::Failed, Some("Alt+KeyS")),
        ];
        let acc = accelerators_of(&list);
        assert_eq!(acc[0].as_deref(), Some("Alt+Shift+KeyD"));
        assert_eq!(acc[2], None, "注册失败的不显示");
        assert_eq!(acc[4].as_deref(), Some("Alt+KeyV"));
    }

    #[test]
    fn status_dot_is_a_filled_circle() {
        let img = status_dot(&TrayStatus::Ok);
        let center = ((16 * 32 + 16) * 4) as usize;
        assert_eq!(img.rgba()[center + 3], 255);
        assert_eq!(img.rgba()[3], 0, "角落透明");
    }
}
