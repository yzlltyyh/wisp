//! 单实例与命令行触发。
//!
//! tauri-plugin-single-instance 最先注册；第二个实例启动时把参数转发到 [`on_second_instance`]（第二个进程随即退出）：
//! - `wisp --trigger <action>` / `--trigger=<action>`（action 为 snake_case 的 `HotkeyAction`，例如 `translate_input`）
//!   → 投递 `Command::Action { origin: Cli }`。Wayland 下把系统快捷键绑定到这条命令即可触发（不常驻任何 HTTP 端口）；
//!   动作名无法识别时发一条系统通知列出可用的动作；
//! - 带 `--autostart`（登录时被重复拉起）→ 忽略；
//! - 其他情况（用户再次打开应用）→ [`bring_to_front`]：引导还没完成时打开 / 聚焦引导，否则打开 / 聚焦设置窗口。
//!
//! **macOS**：在 Finder / 启动台 / Spotlight / Dock 里再次打开 Wisp.app 时系统不会启动第二个进程，而是给现有进程发
//! 「重新打开」事件（`RunEvent::Reopen`，单实例插件的回调不会触发），由 `bootstrap::on_run_event` 转到 [`on_reopen`]，
//! 处理与「再次双击」相同。
//!
//! Wisp 还没运行时直接执行 `wisp --trigger …`：第一个实例在 setup 末尾经 [`handle_launch_args`] 处理同样的参数。

use tauri::Manager;
use tauri_plugin_notification::NotificationExt;
use wisp_types::{HotkeyAction, WindowTarget};

use crate::app::i18n;
use crate::bus::{Command, TriggerOrigin};
use crate::state::AppState;

/// 命令行解析的结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchRequest {
    /// `--trigger <action>`
    Trigger(HotkeyAction),
    /// `--trigger` 后面的动作名无法识别（或缺失）
    UnknownAction(String),
    /// 由开机自启拉起（`--autostart`）
    Autostart,
    /// 没有参数
    Plain,
}

/// 解析命令行（第一个元素是可执行文件路径，忽略）
pub fn parse_args(args: &[String]) -> LaunchRequest {
    let mut iter = args.iter().skip(1);
    let mut autostart = false;
    while let Some(arg) = iter.next() {
        let value = if arg == "--trigger" {
            Some(iter.next().cloned().unwrap_or_default())
        } else {
            arg.strip_prefix("--trigger=").map(str::to_owned)
        };
        if let Some(name) = value {
            return match parse_action(&name) {
                Some(action) => LaunchRequest::Trigger(action),
                None => LaunchRequest::UnknownAction(name),
            };
        }
        if arg == super::autostart::AUTOSTART_ARG {
            autostart = true;
        }
    }
    if autostart {
        LaunchRequest::Autostart
    } else {
        LaunchRequest::Plain
    }
}

/// 动作名（snake_case，大小写与 `-` / `_` 不敏感）
fn parse_action(name: &str) -> Option<HotkeyAction> {
    let normalized = name.trim().to_ascii_lowercase().replace('-', "_");
    HotkeyAction::ALL.into_iter().find(|a| a.as_str() == normalized)
}

/// 第二个实例的回调（单实例插件在主进程中调用）
pub fn on_second_instance(app: &tauri::AppHandle, args: Vec<String>, cwd: String) {
    let _ = cwd;
    let request = parse_args(&args);
    log::info!("second instance started request={}", describe(&request));
    match request {
        LaunchRequest::Autostart => {}
        LaunchRequest::Plain => bring_to_front(app),
        other => dispatch(app, other),
    }
}

/// macOS「重新打开」（`RunEvent::Reopen`）：与再次双击应用图标相同
pub fn on_reopen(app: &tauri::AppHandle) {
    log::info!("app reopened");
    bring_to_front(app);
}

/// 用户再次「打开」Wisp：引导窗口开着或引导还没完成时把引导带到前台，否则打开 / 聚焦设置窗口
pub fn bring_to_front(app: &tauri::AppHandle) {
    let onboarding = app
        .get_webview_window(crate::windows::settings_window::ONBOARDING_LABEL)
        .is_some()
        || app
            .try_state::<AppState>()
            .is_some_and(|s| s.settings.needs_onboarding());
    let target = if onboarding {
        WindowTarget::Onboarding { step: None }
    } else {
        WindowTarget::Settings {
            section: None,
            anchor: None,
        }
    };
    send(app, Command::OpenWindow(target));
}

/// 第一个实例：处理启动参数里的 `--trigger`（在 setup 末尾调用）
pub fn handle_launch_args(app: &tauri::AppHandle) {
    let args: Vec<String> = std::env::args().collect();
    let request = parse_args(&args);
    if matches!(request, LaunchRequest::Trigger(_) | LaunchRequest::UnknownAction(_)) {
        log::info!("launch request={}", describe(&request));
        dispatch(app, request);
    }
}

fn dispatch(app: &tauri::AppHandle, request: LaunchRequest) {
    match request {
        LaunchRequest::Trigger(action) => send(
            app,
            Command::Action {
                action,
                origin: TriggerOrigin::Cli,
            },
        ),
        LaunchRequest::UnknownAction(name) => notify_unknown(app, &name),
        LaunchRequest::Autostart | LaunchRequest::Plain => {}
    }
}

fn send(app: &tauri::AppHandle, cmd: Command) {
    if let Some(state) = app.try_state::<AppState>() {
        state.bus.send(cmd);
    }
}

/// 日志里的描述（动作名截断，避免把任意长的参数写进日志）
fn describe(r: &LaunchRequest) -> String {
    match r {
        LaunchRequest::Trigger(a) => format!("trigger:{}", a.as_str()),
        LaunchRequest::UnknownAction(name) => format!("unknown_action:{}", name.chars().take(32).collect::<String>()),
        LaunchRequest::Autostart => "autostart".to_owned(),
        LaunchRequest::Plain => "plain".to_owned(),
    }
}

fn notify_unknown(app: &tauri::AppHandle, name: &str) {
    let lang = crate::tray::menu::current_lang(app);
    let actions = HotkeyAction::ALL.map(HotkeyAction::as_str).join(", ");
    let shown: String = name.chars().take(32).collect();
    log::warn!("--trigger: unknown action {shown:?}");
    if let Err(e) = app
        .notification()
        .builder()
        .title(i18n::tr(lang, "notify.cli.unknown_action.title"))
        .body(i18n::tr_args(
            lang,
            "notify.cli.unknown_action.body",
            &[("action", &shown), ("actions", &actions)],
        ))
        .show()
    {
        log::debug!("notification failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        std::iter::once("wisp")
            .chain(v.iter().copied())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn parses_trigger_argument() {
        assert_eq!(
            parse_args(&args(&["--trigger", "ocr_translate"])),
            LaunchRequest::Trigger(HotkeyAction::OcrTranslate)
        );
        assert_eq!(
            parse_args(&args(&["--trigger=translate-input"])),
            LaunchRequest::Trigger(HotkeyAction::TranslateInput)
        );
        assert_eq!(
            parse_args(&args(&["--autostart", "--trigger", "Clipboard"])),
            LaunchRequest::Trigger(HotkeyAction::Clipboard)
        );
    }

    #[test]
    fn unknown_or_missing_actions_are_reported() {
        assert_eq!(
            parse_args(&args(&["--trigger", "bogus"])),
            LaunchRequest::UnknownAction("bogus".to_owned())
        );
        assert_eq!(
            parse_args(&args(&["--trigger"])),
            LaunchRequest::UnknownAction(String::new())
        );
    }

    #[test]
    fn autostart_and_plain_launches() {
        assert_eq!(parse_args(&args(&["--autostart"])), LaunchRequest::Autostart);
        assert_eq!(parse_args(&args(&[])), LaunchRequest::Plain);
        // 可执行文件路径本身不参与解析
        assert_eq!(parse_args(&["--trigger".to_owned()]), LaunchRequest::Plain);
    }

    #[test]
    fn log_description_is_bounded() {
        let long = "x".repeat(500);
        assert!(describe(&LaunchRequest::UnknownAction(long)).len() < 64);
    }
}
