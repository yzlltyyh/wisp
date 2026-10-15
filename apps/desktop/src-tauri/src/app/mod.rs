//! 应用核心：启动顺序、运行时、路径、日志、外观、Rust 侧文案、性能埋点、单实例、冒烟模式、诊断导出、
//! 权限状态、开机自启与设置变化的统一副作用（reactor）。

pub mod app_menu;
pub mod appearance;
pub mod autostart;
pub mod bootstrap;
pub mod diagnostics;
pub mod files;
pub mod i18n;
pub mod logging;
pub mod paths;
pub mod perf;
pub mod permissions;
pub mod reactor;
pub mod relaunch;
pub mod runtime;
pub mod second_instance;
pub mod session_bus;
pub mod smoke;

/// 设置窗口的分组 id（`WindowTarget::Settings { section }` 的取值，托盘与更新器深链用；设置窗口的路由以此为准）
pub mod sections {
    pub const GENERAL: &str = "general";
    pub const HOTKEYS: &str = "hotkeys";
    pub const PROVIDERS: &str = "providers";
    pub const TRANSLATE: &str = "translate";
    pub const OCR: &str = "ocr";
    pub const CLIPBOARD: &str = "clipboard";
    pub const APPEARANCE: &str = "appearance";
    pub const ABOUT: &str = "about";
    /// 全部分组（顺序即导航顺序）。前端镜像是 `shared/ipc/sections.ts` 的 `SETTINGS_SECTIONS`，由下面的黄金样例
    /// 比对
    pub const ALL: [&str; 8] = [
        GENERAL, HOTKEYS, PROVIDERS, TRANSLATE, OCR, CLIPBOARD, APPEARANCE, ABOUT,
    ];

    /// OCR 分组里的「识别历史」（OCR 面板 ⋯ 菜单的入口直达这里）
    pub const ANCHOR_OCR_HISTORY: &str = "ocr_history";
    /// 分组内的锚点（`WindowTarget::Settings.anchor`）：全部合法的「分组 → 锚点」组合。前端镜像是
    /// `shared/ipc/sections.ts` 的 `SETTINGS_ANCHORS`，由下面的黄金样例 `fixtures/consts/SettingsAnchors.json` 比对。
    /// 新增锚点时两边一起加，`ipc::validate::validate_window_target` 拒绝未登记的组合
    pub const ANCHORS: [(&str, &str); 1] = [(OCR, ANCHOR_OCR_HISTORY)];

    /// 「分组 → 锚点」是否已登记
    pub fn anchor_known(section: &str, anchor: &str) -> bool {
        ANCHORS.iter().any(|&(s, a)| s == section && a == anchor)
    }

    #[cfg(test)]
    mod tests {
        use std::path::Path;

        /// 写出 / 比对 `fixtures/consts/SettingsAnchors.json`（`{ 分组: [锚点…] }`，前端 `sections.test.ts` 用它比对
        /// SETTINGS_ANCHORS）；`UPDATE_GOLDEN=1` 时重写
        #[test]
        fn golden_settings_anchors() {
            let path =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../src/shared/ipc/fixtures/consts/SettingsAnchors.json");
            let mut map: std::collections::BTreeMap<&str, Vec<&str>> = std::collections::BTreeMap::new();
            for (section, anchor) in super::ANCHORS {
                assert!(super::ALL.contains(&section), "锚点 {anchor} 的分组 {section} 不存在");
                map.entry(section).or_default().push(anchor);
            }
            let expected = format!("{}\n", serde_json::to_string_pretty(&map).expect("序列化失败"));
            if std::env::var("UPDATE_GOLDEN").is_ok_and(|v| !v.is_empty() && v != "0") {
                std::fs::create_dir_all(path.parent().expect("父目录")).expect("创建目录失败");
                std::fs::write(&path, &expected).expect("写入失败");
                return;
            }
            let actual = std::fs::read_to_string(&path).unwrap_or_default();
            assert_eq!(
                actual, expected,
                "fixtures/consts/SettingsAnchors.json 与 app::sections::ANCHORS 不一致（确认后 UPDATE_GOLDEN=1 cargo test -p wisp-desktop --lib app::）"
            );
            assert!(super::anchor_known("ocr", "ocr_history"));
            assert!(!super::anchor_known("translate", "ocr_history"));
        }

        /// 写出 / 比对 `fixtures/consts/SettingsSections.json`（前端 `sections.test.ts` 用它比对 SETTINGS_SECTIONS）；
        /// `UPDATE_GOLDEN=1` 时重写
        #[test]
        fn golden_settings_sections() {
            let path =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../src/shared/ipc/fixtures/consts/SettingsSections.json");
            let expected = format!("{}\n", serde_json::to_string_pretty(&super::ALL).expect("序列化失败"));
            if std::env::var("UPDATE_GOLDEN").is_ok_and(|v| !v.is_empty() && v != "0") {
                std::fs::create_dir_all(path.parent().expect("父目录")).expect("创建目录失败");
                std::fs::write(&path, &expected).expect("写入失败");
                return;
            }
            let actual = std::fs::read_to_string(&path).unwrap_or_default();
            assert_eq!(
                actual, expected,
                "fixtures/consts/SettingsSections.json 与 app::sections::ALL 不一致（确认后 UPDATE_GOLDEN=1 cargo test -p wisp-desktop --lib app::）"
            );
        }
    }
}

use std::collections::BTreeMap;

use wisp_types::{AppKeymap, AppMeta, OsKind, RuntimeInfo, UpdateStatus};

use crate::state::AppState;

/// 应用元信息（`app_info` 命令）：版本、平台信息与三个目录
pub fn app_meta(app: &tauri::AppHandle, state: &AppState) -> AppMeta {
    let paths = state.settings.paths();
    AppMeta {
        version: app.package_info().version.to_string(),
        platform: state.platform.info(),
        config_dir: paths.config_dir.to_string_lossy().into_owned(),
        data_dir: paths.data_dir.to_string_lossy().into_owned(),
        log_dir: paths.log_dir.to_string_lossy().into_owned(),
        dev_mode: state.settings.get().general.developer_mode,
    }
}

/// 汇总运行时信息（`settings_get` / `settings:changed` 的 runtime 部分）
pub fn runtime_info(app: &tauri::AppHandle, state: &AppState) -> RuntimeInfo {
    let settings = state.settings.get();
    let keychain = state.settings.keychain();
    let has_key: BTreeMap<String, bool> = settings
        .providers
        .iter()
        .map(|p| (p.id.clone(), keychain.has(&p.id)))
        .collect();
    RuntimeInfo {
        version: app.package_info().version.to_string(),
        platform: state.platform.info(),
        has_key,
        keychain: keychain.state(),
        hotkeys: state.hotkeys.status(),
        // 只读缓存值：apply_ext_triggers 会安装 / 卸载钩子，只能由 HotkeyService 自己在设置变化时调用
        ext_triggers: state.hotkeys.ext_trigger_status(),
        permissions: permissions::cached(app),
        clipboard_paused: state.clipboard.paused(),
        update: crate::updater::status(app).unwrap_or_else(|_| UpdateStatus::default()),
        glossary_count: u32::try_from(state.settings.glossary_entries().len()).unwrap_or(u32::MAX),
        settings_issue: state.settings.file_issue(),
        orphan_keys: state.settings.orphan_key_count(),
        autostart_issue: settings.general.launch_at_login.then(autostart::issue).flatten(),
    }
}

/// 平台层使用的「应用 → 复制 / 粘贴键」表：用户条目在前，内置默认表在后（`PlatformConfig.keymaps` 的约定）
pub fn merged_keymaps(user: &[AppKeymap]) -> Vec<AppKeymap> {
    let mut merged = user.to_vec();
    merged.extend(wisp_platform::common::keymap::builtin(OsKind::current()));
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_keymaps_come_first() {
        let user = AppKeymap {
            os: OsKind::current(),
            app_id: "my-terminal".to_owned(),
            copy: Some("Control+Shift+KeyC".to_owned()),
            paste: None,
        };
        let merged = merged_keymaps(std::slice::from_ref(&user));
        assert_eq!(merged.first(), Some(&user));
        assert_eq!(
            merged.len(),
            1 + wisp_platform::common::keymap::builtin(OsKind::current()).len()
        );
    }

    #[test]
    fn section_ids_are_unique() {
        let mut ids = sections::ALL.to_vec();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), sections::ALL.len());
    }
}
