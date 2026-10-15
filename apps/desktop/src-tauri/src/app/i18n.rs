//! Rust 侧文案：托盘菜单、状态行、原生对话框、通知，用 `match (lang, key)` 静态表维护。
//!
//! - 界面语言的**唯一解析点**：`AppearanceState.ui_language`（前端据此选语言）与托盘菜单都经 [`resolve`]；
//! - 规则：跟随系统时 zh-Hans / zh-CN / zh-SG（以及不带地区的 zh）用 zh-CN，其他一律 en-US
//!   （繁体 zh-Hant / zh-TW / zh-HK / zh-MO 也用 en-US，v1 没有繁体文案）；
//! - 带参数的文案用 `{name}` 占位，经 [`tr_args`] 替换；
//! - 单测 `every_key_has_both_languages` 保证两种语言的 key 集合一致、没有遗漏。

use wisp_types::UiLanguage;

/// Rust 侧支持的界面语言
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    ZhCn,
    EnUs,
}

impl Lang {
    /// "zh-CN" / "en-US"
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ZhCn => "zh-CN",
            Self::EnUs => "en-US",
        }
    }
}

/// 解析界面语言：`system` 时按系统区域设置
pub fn resolve(pref: UiLanguage) -> Lang {
    match pref {
        UiLanguage::ZhCn => Lang::ZhCn,
        UiLanguage::EnUs => Lang::EnUs,
        UiLanguage::System => system_lang(),
    }
}

/// 系统界面语言（首次调用时读取并缓存：改系统语言后 macOS / Windows 也要求重新登录才完全生效）
fn system_lang() -> Lang {
    static SYSTEM: std::sync::OnceLock<Lang> = std::sync::OnceLock::new();
    *SYSTEM.get_or_init(|| tauri_plugin_os::locale().map_or(Lang::EnUs, |l| lang_from_locale(&l)))
}

/// 区域设置 → 界面语言：zh-Hans* / zh-CN / zh-SG → zh-CN；其余（含繁体、zh-MY 等其他地区）→ en-US。
/// 不带地区与书写系统的 `zh` 按 CLDR 的默认补全（`zh` → `zh-Hans-CN`）视为 zh-Hans。兼容 `zh_CN.UTF-8`（POSIX）、`zh-Hans-HK`（BCP 47，书写系统优先于地区）等写法，大小写不敏感
pub fn lang_from_locale(locale: &str) -> Lang {
    let normalized = locale
        .split(['.', '@'])
        .next()
        .unwrap_or_default()
        .replace('_', "-")
        .to_ascii_lowercase();
    let mut parts = normalized.split('-').filter(|p| !p.is_empty());
    if parts.next() != Some("zh") {
        return Lang::EnUs;
    }
    let rest: Vec<&str> = parts.collect();
    if rest.contains(&"hans") {
        return Lang::ZhCn;
    }
    if rest.contains(&"hant") {
        return Lang::EnUs;
    }
    match rest.first() {
        None => Lang::ZhCn,
        Some(region) if matches!(*region, "cn" | "sg") => Lang::ZhCn,
        Some(_) => Lang::EnUs,
    }
}

/// 取文案；未知 key 原样返回（便于发现遗漏，单测会拦下）
pub fn tr(lang: Lang, key: &'static str) -> &'static str {
    lookup(lang, key).unwrap_or(key)
}

/// 取文案并替换 `{name}` 占位
pub fn tr_args(lang: Lang, key: &'static str, args: &[(&str, &str)]) -> String {
    let mut out = tr(lang, key).to_owned();
    for (name, value) in args {
        out = out.replace(&format!("{{{name}}}"), value);
    }
    out
}

/// 全部 key（单测用：两种语言都必须有文案）
#[cfg(test)]
const KEYS: &[&str] = &[
    "tray.tooltip",
    "tray.status.ok",
    "tray.status.permission",
    "tray.status.permission_after_update",
    "tray.status.hotkey_failed",
    "tray.status.hotkey_failed_one",
    "tray.status.paused",
    "tray.status.update",
    "tray.status.update_ready",
    "tray.translate_selection",
    "tray.translate_input",
    "tray.ocr",
    "tray.ocr_translate",
    "tray.clipboard",
    "tray.show_pinned",
    "tray.pause_clipboard",
    "tray.clear_clipboard",
    "tray.settings",
    "tray.onboarding",
    "tray.check_update",
    "tray.quit",
    "dialog.clear_clipboard.title",
    "dialog.clear_clipboard.message",
    "dialog.clear_clipboard.ok",
    "dialog.clear_clipboard.all",
    "dialog.cancel",
    "dialog.ok",
    "dialog.update.latest.title",
    "dialog.update.latest.message",
    "dialog.update.failed.title",
    "dialog.update.failed.message",
    "dialog.glossary.import",
    "dialog.glossary.export",
    "dialog.glossary.filter",
    "dialog.diagnostics.title",
    "dialog.diagnostics.filter",
    "notify.no_tray.title",
    "notify.no_tray.body",
    "notify.cli.unknown_action.title",
    "notify.cli.unknown_action.body",
    "notify.update_ready.title",
    "notify.update_ready.body",
    "notify.update_installing.title",
    "notify.update_installing.body",
    "app_menu.about",
    "app_menu.settings",
    "app_menu.hide",
    "app_menu.hide_others",
    "app_menu.show_all",
    "app_menu.quit",
    "app_menu.edit",
    "app_menu.undo",
    "app_menu.redo",
    "app_menu.cut",
    "app_menu.copy",
    "app_menu.paste",
    "app_menu.select_all",
    "app_menu.window",
    "app_menu.minimize",
    "app_menu.close_window",
];

fn lookup(lang: Lang, key: &str) -> Option<&'static str> {
    use Lang::{EnUs, ZhCn};
    Some(match (lang, key) {
        // ---- 托盘 ----
        (ZhCn | EnUs, "tray.tooltip") => "Wisp",
        (ZhCn, "tray.status.ok") => "Wisp 正在运行",
        (EnUs, "tray.status.ok") => "Wisp is running",
        (ZhCn, "tray.status.permission") => "缺少系统权限 · 点击授权",
        (EnUs, "tray.status.permission") => "Permissions needed · Click to grant",
        (ZhCn, "tray.status.permission_after_update") => "更新后需要重新授权 · 点击查看",
        (EnUs, "tray.status.permission_after_update") => "Re-grant permissions after update · Click to fix",
        // 带失败个数（英文区分单复数）
        (ZhCn, "tray.status.hotkey_failed" | "tray.status.hotkey_failed_one") => "{count} 个快捷键未生效 · 点击查看",
        (EnUs, "tray.status.hotkey_failed") => "{count} shortcuts aren't working · Click to review",
        (EnUs, "tray.status.hotkey_failed_one") => "1 shortcut isn't working · Click to review",
        (ZhCn, "tray.status.paused") => "剪贴板记录已暂停 · 点击恢复",
        (EnUs, "tray.status.paused") => "Clipboard recording paused · Click to resume",
        (ZhCn, "tray.status.update") => "有新版本 {version} · 点击查看",
        (EnUs, "tray.status.update") => "Wisp {version} is available · Click to view",
        (ZhCn, "tray.status.update_ready") => "新版本 {version} 已就绪 · 重启后生效",
        (EnUs, "tray.status.update_ready") => "Wisp {version} is ready · Restart to apply",
        (ZhCn, "tray.translate_selection") => "划词翻译",
        (EnUs, "tray.translate_selection") => "Translate Selection",
        (ZhCn, "tray.translate_input") => "翻译输入…",
        (EnUs, "tray.translate_input") => "Translate Text…",
        (ZhCn, "tray.ocr") => "截图 OCR",
        (EnUs, "tray.ocr") => "Capture Text (OCR)",
        (ZhCn, "tray.ocr_translate") => "截图直接翻译",
        (EnUs, "tray.ocr_translate") => "Capture & Translate",
        (ZhCn, "tray.clipboard") => "剪贴板",
        (EnUs, "tray.clipboard") => "Clipboard History",
        (ZhCn, "tray.show_pinned") => "显示固定窗口",
        (EnUs, "tray.show_pinned") => "Show Pinned Windows",
        (ZhCn, "tray.pause_clipboard") => "暂停剪贴板记录",
        (EnUs, "tray.pause_clipboard") => "Pause Clipboard Recording",
        (ZhCn, "tray.clear_clipboard") => "清空剪贴板历史…",
        (EnUs, "tray.clear_clipboard") => "Clear Clipboard History…",
        (ZhCn, "tray.settings") => "设置…",
        (EnUs, "tray.settings") => "Settings…",
        (ZhCn, "tray.onboarding") => "使用引导…",
        (EnUs, "tray.onboarding") => "Getting Started…",
        (ZhCn, "tray.check_update") => "检查更新",
        (EnUs, "tray.check_update") => "Check for Updates",
        (ZhCn, "tray.quit") => "退出",
        (EnUs, "tray.quit") => "Quit Wisp",
        // ---- 原生对话框 ----
        (ZhCn, "dialog.clear_clipboard.title") => "清空剪贴板历史？",
        (EnUs, "dialog.clear_clipboard.title") => "Clear clipboard history?",
        (ZhCn, "dialog.clear_clipboard.message") => {
            "可以只删除未置顶的记录（置顶的条目保留），也可以全部删除。此操作无法撤销。"
        }
        (EnUs, "dialog.clear_clipboard.message") => {
            "Delete only unpinned items and keep pinned ones, or delete everything. This can't be undone."
        }
        (ZhCn, "dialog.clear_clipboard.ok") => "清空（保留置顶）",
        (EnUs, "dialog.clear_clipboard.ok") => "Clear (Keep Pinned)",
        (ZhCn, "dialog.clear_clipboard.all") => "全部清空",
        (EnUs, "dialog.clear_clipboard.all") => "Clear All",
        (ZhCn, "dialog.cancel") => "取消",
        (EnUs, "dialog.cancel") => "Cancel",
        (ZhCn, "dialog.ok") => "好",
        (EnUs, "dialog.ok") => "OK",
        (ZhCn, "dialog.update.latest.title") => "已是最新版本",
        (EnUs, "dialog.update.latest.title") => "You're up to date",
        (ZhCn, "dialog.update.latest.message") => "Wisp {version} 是当前最新版本。",
        (EnUs, "dialog.update.latest.message") => "Wisp {version} is the latest version.",
        (ZhCn, "dialog.update.failed.title") => "无法检查更新",
        (EnUs, "dialog.update.failed.title") => "Couldn't check for updates",
        (ZhCn, "dialog.update.failed.message") => "请检查网络连接后重试。",
        (EnUs, "dialog.update.failed.message") => "Check your network connection and try again.",
        (ZhCn, "dialog.glossary.import") => "导入术语表",
        (EnUs, "dialog.glossary.import") => "Import Glossary",
        (ZhCn, "dialog.glossary.export") => "导出术语表",
        (EnUs, "dialog.glossary.export") => "Export Glossary",
        (ZhCn, "dialog.glossary.filter") => "术语表",
        (EnUs, "dialog.glossary.filter") => "Glossary",
        (ZhCn, "dialog.diagnostics.title") => "导出诊断信息",
        (EnUs, "dialog.diagnostics.title") => "Export Diagnostics",
        (ZhCn, "dialog.diagnostics.filter") => "ZIP 压缩包",
        (EnUs, "dialog.diagnostics.filter") => "ZIP archive",
        // ---- 通知 ----
        (ZhCn, "notify.no_tray.title") => "Wisp 已在后台运行",
        (EnUs, "notify.no_tray.title") => "Wisp is running in the background",
        (ZhCn, "notify.no_tray.body") => {
            "当前桌面没有可用的系统托盘（GNOME 需要安装 AppIndicator 扩展），已打开设置窗口。全部功能仍可通过快捷键使用。"
        }
        (EnUs, "notify.no_tray.body") => {
            "No system tray is available (on GNOME, install the AppIndicator extension), so Settings was opened instead. All features remain available via shortcuts."
        }
        (ZhCn, "notify.cli.unknown_action.title") => "无法识别的动作",
        (EnUs, "notify.cli.unknown_action.title") => "Unknown action",
        (ZhCn, "notify.cli.unknown_action.body") => "--trigger 不支持「{action}」。可用的动作：{actions}",
        (EnUs, "notify.cli.unknown_action.body") => "--trigger does not accept \"{action}\". Available: {actions}",
        (ZhCn, "notify.update_ready.title") => "Wisp 新版本已安装",
        (EnUs, "notify.update_ready.title") => "Wisp update installed",
        (ZhCn, "notify.update_ready.body") => "{version} 将在下次启动时生效。",
        (EnUs, "notify.update_ready.body") => "Version {version} will be used the next time Wisp starts.",
        (ZhCn, "notify.update_installing.title") => "正在更新 Wisp",
        (EnUs, "notify.update_installing.title") => "Updating Wisp",
        (ZhCn, "notify.update_installing.body") => "正在安装 {version}，完成后会自动重新打开。",
        (EnUs, "notify.update_installing.body") => "Installing version {version}. Wisp will reopen when it's done.",
        // ---- macOS 应用菜单（app_menu.rs；用词与系统应用一致） ----
        (ZhCn, "app_menu.about") => "关于 Wisp",
        (EnUs, "app_menu.about") => "About Wisp",
        (ZhCn, "app_menu.settings") => "设置…",
        (EnUs, "app_menu.settings") => "Settings…",
        (ZhCn, "app_menu.hide") => "隐藏 Wisp",
        (EnUs, "app_menu.hide") => "Hide Wisp",
        (ZhCn, "app_menu.hide_others") => "隐藏其他",
        (EnUs, "app_menu.hide_others") => "Hide Others",
        (ZhCn, "app_menu.show_all") => "全部显示",
        (EnUs, "app_menu.show_all") => "Show All",
        (ZhCn, "app_menu.quit") => "退出 Wisp",
        (EnUs, "app_menu.quit") => "Quit Wisp",
        (ZhCn, "app_menu.edit") => "编辑",
        (EnUs, "app_menu.edit") => "Edit",
        (ZhCn, "app_menu.undo") => "撤销",
        (EnUs, "app_menu.undo") => "Undo",
        (ZhCn, "app_menu.redo") => "重做",
        (EnUs, "app_menu.redo") => "Redo",
        (ZhCn, "app_menu.cut") => "剪切",
        (EnUs, "app_menu.cut") => "Cut",
        (ZhCn, "app_menu.copy") => "拷贝",
        (EnUs, "app_menu.copy") => "Copy",
        (ZhCn, "app_menu.paste") => "粘贴",
        (EnUs, "app_menu.paste") => "Paste",
        (ZhCn, "app_menu.select_all") => "全选",
        (EnUs, "app_menu.select_all") => "Select All",
        (ZhCn, "app_menu.window") => "窗口",
        (EnUs, "app_menu.window") => "Window",
        (ZhCn, "app_menu.minimize") => "最小化",
        (EnUs, "app_menu.minimize") => "Minimize",
        (ZhCn, "app_menu.close_window") => "关闭窗口",
        (EnUs, "app_menu.close_window") => "Close Window",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_has_both_languages() {
        for key in KEYS {
            for lang in [Lang::ZhCn, Lang::EnUs] {
                assert!(lookup(lang, key).is_some(), "{key} 缺少 {} 文案", lang.as_str());
            }
        }
    }

    #[test]
    fn english_strings_contain_no_cjk() {
        for key in KEYS {
            let s = tr(Lang::EnUs, key);
            assert!(
                !s.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                "{key} 的英文文案里有中文：{s}"
            );
        }
    }

    #[test]
    fn placeholders_are_replaced() {
        let s = tr_args(Lang::ZhCn, "tray.status.update", &[("version", "0.2.0")]);
        assert_eq!(s, "有新版本 0.2.0 · 点击查看");
        assert!(!tr_args(Lang::EnUs, "tray.status.update", &[("version", "0.2.0")]).contains('{'));
    }

    #[test]
    fn unknown_key_is_returned_verbatim() {
        assert_eq!(tr(Lang::EnUs, "tray.nope"), "tray.nope");
    }

    #[test]
    fn locale_rules_map_to_supported_languages() {
        for zh in [
            "zh",
            "zh-CN",
            "zh_CN.UTF-8",
            "zh-Hans",
            "zh-Hans-CN",
            "ZH-sg",
            "zh-Hans-HK",
        ] {
            assert_eq!(lang_from_locale(zh), Lang::ZhCn, "{zh}");
        }
        for en in [
            "en-US",
            "en",
            "ja-JP",
            "zh-TW",
            "zh-Hant",
            "zh-Hant-TW",
            "zh_HK",
            "zh-MO",
            "zh-MY",
            "",
            "C",
            "POSIX",
        ] {
            assert_eq!(lang_from_locale(en), Lang::EnUs, "{en}");
        }
    }

    #[test]
    fn explicit_preference_wins() {
        assert_eq!(resolve(UiLanguage::ZhCn), Lang::ZhCn);
        assert_eq!(resolve(UiLanguage::EnUs), Lang::EnUs);
    }
}
