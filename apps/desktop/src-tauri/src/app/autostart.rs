//! 开机自启：Windows 注册表 Run / Linux `~/.config/autostart` 经 tauri-plugin-autostart；
//! macOS 自己写 LaunchAgent。启动参数带 `--autostart`（据此静默启动、不弹窗）。
//!
//! 以 `general.launch_at_login` 为准，启动时与设置变化时同步系统里的登记（先读后写，一致时不动系统）。以下情况**不**登记，
//! 原因经 `RuntimeInfo.autostart_issue` 告诉设置窗口（开关下方显示原因与处理办法）：
//! - debug 构建（开发时不把 `target/debug` 里的二进制登记成登录项；需要测试时设 `WISP_DEV_AUTOSTART=1`）——只记日志；
//! - 冒烟模式——只记日志；
//! - macOS 上从 App Translocation 的随机路径运行（未签名应用直接从下载目录打开时，路径重启后就失效）→ `translocated`；
//! - 写入系统失败 → `failed`。
//!
//! **macOS 的 LaunchAgent**：插件生成的 plist 以应用名「Wisp」作 Label、`AssociatedBundleIdentifiers` 为空，macOS 13+ 的
//! 「登录项 · 允许在后台」与「已添加后台项目」通知里显示成裸可执行文件名并标为身份不明。这里改写
//! `~/Library/LaunchAgents/<bundle id>.plist`：Label 用反向域名的 bundle id，`AssociatedBundleIdentifiers` 填应用的 bundle id
//! （系统据此显示应用名与图标），参数照旧带 `--autostart`。旧版插件写的 `Wisp.plist`（含 `--autostart`）同步时一并删除。
//! 没有改用 SMAppService：它拉起应用时不带参数，区分不了「登录时自启」与「用户手动打开」。
//!
//! **启动环境**：经 LaunchServices 打开时由 `Info.plist` 的 `LSEnvironment` 设置
//! [`LAUNCH_ENV`]（`MallocSpaceEfficient=1`：macOS 26 的 xzone malloc 不再把释放的大块留作「空闲但脏」的段 2–3 分钟，
//! 空闲基线与截图之后的主进程都更小）；LaunchAgent 由 launchd 直接执行、不经 LaunchServices，同样的变量写进 plist 的
//! `EnvironmentVariables`。两处必须一致（单测核对 Info.plist）。分配器在进程启动时读取环境变量，不能在运行期设置。
//!
//! 系统调用是阻塞的（Windows 注册表、macOS 写 plist），放到阻塞线程池里执行。

use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use tauri::Manager;
use tauri_plugin_autostart::ManagerExt;
use wisp_types::OsKind;

/// 启动参数：由开机自启拉起
pub const AUTOSTART_ARG: &str = "--autostart";

/// macOS 的启动环境（见模块注释「启动环境」）：`Info.plist` 的 `LSEnvironment` 与 LaunchAgent 的 `EnvironmentVariables`
pub const LAUNCH_ENV: &[(&str, &str)] = &[("MallocSpaceEfficient", "1")];

/// 本进程是否带着 [`LAUNCH_ENV`] 启动（启动日志的诊断：从终端直接执行二进制时为 false）。只有 macOS 设启动环境
/// （Info.plist / LaunchAgent），Windows / Linux 恒为 None——日志里不打印这一项，免得被读成「没生效」
pub fn launch_env_applied() -> Option<bool> {
    cfg!(target_os = "macos").then(|| {
        LAUNCH_ENV
            .iter()
            .all(|(k, v)| std::env::var_os(k).is_some_and(|cur| cur == *v))
    })
}

/// 设置窗口的提示：从 App Translocation 运行，需要先把 Wisp 移到「应用程序」（i18n key）
pub const ISSUE_TRANSLOCATED: &str = "settings.general.autostart_issue.translocated";
/// 设置窗口的提示：写入系统登录项失败（i18n key）
pub const ISSUE_FAILED: &str = "settings.general.autostart_issue.failed";

/// 最近一次同步的问题（`RuntimeInfo.autostart_issue`）
static ISSUE: Mutex<Option<&'static str>> = Mutex::new(None);

/// 本次是否由开机自启拉起
pub fn launched_at_login() -> bool {
    std::env::args().any(|a| a == AUTOSTART_ARG)
}

/// 不登记的原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    Smoke,
    DebugBuild,
    Translocated,
}

impl Skip {
    fn describe(self) -> &'static str {
        match self {
            Self::Smoke => "smoke mode",
            Self::DebugBuild => "debug build (set WISP_DEV_AUTOSTART=1 to test)",
            Self::Translocated => "running from App Translocation; move Wisp to Applications first",
        }
    }
}

/// 不登记的原因（None 表示可以登记）
pub fn skip_reason() -> Option<Skip> {
    if super::smoke::enabled() {
        return Some(Skip::Smoke);
    }
    if cfg!(debug_assertions) && std::env::var_os("WISP_DEV_AUTOSTART").is_none() {
        return Some(Skip::DebugBuild);
    }
    let exe = std::env::current_exe().ok()?;
    if exe.to_string_lossy().contains("/AppTranslocation/") {
        return Some(Skip::Translocated);
    }
    None
}

/// 最近一次同步的问题（i18n key）；开关关闭或登记成功时为 None
pub fn issue() -> Option<String> {
    (*ISSUE.lock()).map(str::to_owned)
}

/// 记下问题；变化时通知设置窗口刷新快照（`SettingsService::notify_changed`，不改设置本身，不会再次触发同步）
fn set_issue(app: &tauri::AppHandle, issue: Option<&'static str>) {
    let changed = {
        let mut cur = ISSUE.lock();
        let changed = *cur != issue;
        *cur = issue;
        changed
    };
    if changed && let Some(state) = app.try_state::<crate::state::AppState>() {
        state.settings.notify_changed();
    }
}

/// 把系统里的登记同步为 `enabled`（异步执行，结果经 [`issue`] 呈现）
pub fn sync(app: &tauri::AppHandle, enabled: bool) {
    if let Some(reason) = skip_reason() {
        log::info!(
            "autostart sync skipped ({}) launch_at_login={enabled}",
            reason.describe()
        );
        let issue = (enabled && reason == Skip::Translocated).then_some(ISSUE_TRANSLOCATED);
        set_issue(app, issue);
        return;
    }
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let result = if OsKind::current() == OsKind::Macos {
            let identifier = handle.config().identifier.clone();
            sync_launch_agent(&identifier, enabled)
        } else {
            sync_plugin(&handle, enabled)
        };
        match result {
            Ok(changed) => {
                if changed {
                    log::info!("autostart {}", if enabled { "enabled" } else { "disabled" });
                } else {
                    log::debug!("autostart already {}", if enabled { "enabled" } else { "disabled" });
                }
                set_issue(&handle, None);
            }
            Err(e) => {
                log::warn!("autostart update failed: {e}");
                set_issue(&handle, enabled.then_some(ISSUE_FAILED));
            }
        }
    });
}

/// Windows / Linux：经插件。返回是否改动了系统
fn sync_plugin(app: &tauri::AppHandle, enabled: bool) -> Result<bool, String> {
    let manager = app.autolaunch();
    match manager.is_enabled() {
        Ok(current) if current == enabled => Ok(false),
        _ => {
            let result = if enabled { manager.enable() } else { manager.disable() };
            result.map(|()| true).map_err(|e| e.to_string())
        }
    }
}

// ------------------------------------------------------------------------------------------------
// macOS LaunchAgent

/// 旧版（tauri-plugin-autostart 的 LaunchAgent 模式）写的文件名：`<应用名>.plist`
const LEGACY_PLIST: &str = "Wisp.plist";

fn launch_agents_dir() -> Result<PathBuf, String> {
    dirs::home_dir()
        .map(|h| h.join("Library").join("LaunchAgents"))
        .ok_or_else(|| "cannot resolve the home directory".to_owned())
}

/// macOS：写 / 删 `~/Library/LaunchAgents/<identifier>.plist`。返回是否改动了系统
fn sync_launch_agent(identifier: &str, enabled: bool) -> Result<bool, String> {
    let dir = launch_agents_dir()?;
    let exe = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|e| format!("cannot resolve the executable: {e}"))?;
    sync_launch_agent_in(&dir, identifier, &exe, enabled)
}

fn sync_launch_agent_in(dir: &Path, identifier: &str, exe: &Path, enabled: bool) -> Result<bool, String> {
    let file = dir.join(format!("{identifier}.plist"));
    let mut changed = remove_legacy(dir);
    if enabled {
        let wanted = launch_agent_plist(identifier, &exe.to_string_lossy());
        if std::fs::read_to_string(&file).ok().as_deref() != Some(wanted.as_str()) {
            std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            std::fs::write(&file, wanted).map_err(|e| format!("cannot write {}: {e}", file.display()))?;
            changed = true;
        }
    } else if file.exists() {
        std::fs::remove_file(&file).map_err(|e| format!("cannot remove {}: {e}", file.display()))?;
        changed = true;
    }
    Ok(changed)
}

/// 删除旧版插件写的 `Wisp.plist`（只删带 `--autostart` 的，不碰同名的其他文件）。返回是否删除了
fn remove_legacy(dir: &Path) -> bool {
    let legacy = dir.join(LEGACY_PLIST);
    let ours = std::fs::read_to_string(&legacy).is_ok_and(|s| s.contains(AUTOSTART_ARG));
    ours && std::fs::remove_file(&legacy).is_ok()
}

/// LaunchAgent 的 plist：Label 与 `AssociatedBundleIdentifiers` 都是应用的 bundle id（登录项里显示应用名与图标）；
/// `EnvironmentVariables` 与 Info.plist 的 `LSEnvironment` 相同（[`LAUNCH_ENV`]）
fn launch_agent_plist(identifier: &str, exe: &str) -> String {
    let id = xml_escape(identifier);
    let exe = xml_escape(exe);
    let env: String = LAUNCH_ENV
        .iter()
        .map(|(k, v)| {
            format!(
                "\t\t<key>{}</key>\n\t\t<string>{}</string>\n",
                xml_escape(k),
                xml_escape(v)
            )
        })
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
\t<key>Label</key>\n\
\t<string>{id}</string>\n\
\t<key>AssociatedBundleIdentifiers</key>\n\
\t<array>\n\
\t\t<string>{id}</string>\n\
\t</array>\n\
\t<key>ProgramArguments</key>\n\
\t<array>\n\
\t\t<string>{exe}</string>\n\
\t\t<string>{AUTOSTART_ARG}</string>\n\
\t</array>\n\
\t<key>EnvironmentVariables</key>\n\
\t<dict>\n\
{env}\t</dict>\n\
\t<key>RunAtLoad</key>\n\
\t<true/>\n\
\t<key>ProcessType</key>\n\
\t<string>Interactive</string>\n\
\t<key>LimitLoadToSessionType</key>\n\
\t<string>Aqua</string>\n\
</dict>\n\
</plist>\n"
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "io.github.yzlltyyh.wisp";

    #[test]
    fn plist_uses_the_bundle_id_and_keeps_the_autostart_arg() {
        let p = launch_agent_plist(ID, "/Applications/Wisp & Co.app/Contents/MacOS/wisp-desktop");
        assert!(p.contains("<key>Label</key>\n\t<string>io.github.yzlltyyh.wisp</string>"));
        assert!(p.contains(
            "<key>AssociatedBundleIdentifiers</key>\n\t<array>\n\t\t<string>io.github.yzlltyyh.wisp</string>"
        ));
        assert!(p.contains("<string>/Applications/Wisp &amp; Co.app/Contents/MacOS/wisp-desktop</string>"));
        assert!(p.contains("<string>--autostart</string>"));
        // 启动环境与 Info.plist 的 LSEnvironment 相同
        assert!(p.contains(
            "<key>EnvironmentVariables</key>\n\t<dict>\n\t\t<key>MallocSpaceEfficient</key>\n\t\t<string>1</string>\n\t</dict>"
        ));
    }

    /// 启动日志的 `launch_env=` 只在 macOS 上有（Windows / Linux 不设启动环境，打印 false 会被读成「没生效」）
    #[test]
    fn launch_env_is_reported_only_on_macos() {
        assert_eq!(launch_env_applied().is_some(), cfg!(target_os = "macos"));
    }

    /// Info.plist 的 `LSEnvironment` 与 [`LAUNCH_ENV`] 逐项一致（经 LaunchServices 打开与 LaunchAgent 自启是同一个启动环境）
    #[test]
    fn info_plist_launch_environment_matches_the_launch_agent() {
        let plist = include_str!("../../Info.plist");
        let start = plist
            .find("<key>LSEnvironment</key>")
            .expect("Info.plist 缺少 LSEnvironment");
        let dict = &plist[start..];
        let dict = &dict[dict.find("<dict>").expect("<dict>")..dict.find("</dict>").expect("</dict>")];
        let pairs: Vec<(String, String)> = dict
            .split("<key>")
            .skip(1)
            .map(|part| {
                let key = part.split("</key>").next().unwrap_or_default().trim().to_owned();
                let value = part
                    .split("<string>")
                    .nth(1)
                    .and_then(|v| v.split("</string>").next())
                    .unwrap_or_default()
                    .to_owned();
                (key, value)
            })
            .collect();
        let expected: Vec<(String, String)> = LAUNCH_ENV
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        assert_eq!(pairs, expected);
    }

    #[test]
    fn launch_agent_is_written_removed_and_migrated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = Path::new("/Applications/Wisp.app/Contents/MacOS/wisp-desktop");
        // 旧版插件留下的 Wisp.plist：迁移时删除；同名但不是 Wisp 写的不碰
        std::fs::write(dir.path().join(LEGACY_PLIST), "<string>--autostart</string>").expect("legacy");
        assert!(sync_launch_agent_in(dir.path(), ID, exe, true).expect("enable"));
        assert!(!dir.path().join(LEGACY_PLIST).exists());
        let file = dir.path().join(format!("{ID}.plist"));
        assert!(std::fs::read_to_string(&file).expect("plist").contains(ID));
        // 内容一致时不改动系统
        assert!(!sync_launch_agent_in(dir.path(), ID, exe, true).expect("again"));
        assert!(sync_launch_agent_in(dir.path(), ID, exe, false).expect("disable"));
        assert!(!file.exists());
        assert!(!sync_launch_agent_in(dir.path(), ID, exe, false).expect("disable again"));
        std::fs::write(dir.path().join(LEGACY_PLIST), "someone else's agent").expect("other");
        sync_launch_agent_in(dir.path(), ID, exe, true).expect("enable");
        assert!(dir.path().join(LEGACY_PLIST).exists());
    }
}
