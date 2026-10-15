//! 默认值数据表：平台默认快捷键、预置排除名单、内置风格 id、取值范围等。
//!
//! 这里只放纯数据和纯函数；运行时如何使用（注册、回退、校验）由各 WP 负责。

use crate::app::OsKind;
use crate::hotkey::{HotkeyAction, ModifierKey};
use crate::settings::ExcludedApp;

/// 一个动作在某个平台上的默认快捷键：注册失败时按「首选 → 备选 → 留空」回退，不自动挑第三个键
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HotkeyDefault {
    pub action: HotkeyAction,
    /// global-hotkey 规范串（修饰键顺序 Control+Alt+Shift+Super）
    pub primary: &'static str,
    pub fallback: &'static str,
}

const fn hk(action: HotkeyAction, primary: &'static str, fallback: &'static str) -> HotkeyDefault {
    HotkeyDefault {
        action,
        primary,
        fallback,
    }
}

/// macOS：⌥D / ⌥⇧D / ⌥S / ⌥⇧S / ⌥V（备选 ⌃⌥D / ⌥A / ⌃⌥S / ⌃⌥⇧S / ⇧⌘V；⇧⌘V 在静态冲突表中标黄）
pub const MACOS_HOTKEYS: [HotkeyDefault; 5] = [
    hk(HotkeyAction::TranslateSelection, "Alt+KeyD", "Control+Alt+KeyD"),
    hk(HotkeyAction::TranslateInput, "Alt+Shift+KeyD", "Alt+KeyA"),
    hk(HotkeyAction::Ocr, "Alt+KeyS", "Control+Alt+KeyS"),
    hk(HotkeyAction::OcrTranslate, "Alt+Shift+KeyS", "Control+Alt+Shift+KeyS"),
    hk(HotkeyAction::Clipboard, "Alt+KeyV", "Shift+Super+KeyV"),
];

/// Windows：Ctrl+Alt+D / +Shift+D / Ctrl+Alt+S / +Shift+S / Ctrl+Alt+V；
/// 备选 Alt+Shift 族（输入翻译备选 Ctrl+Alt+A，检测到 QQ 时提示）
pub const WINDOWS_HOTKEYS: [HotkeyDefault; 5] = [
    hk(HotkeyAction::TranslateSelection, "Control+Alt+KeyD", "Alt+Shift+KeyD"),
    hk(
        HotkeyAction::TranslateInput,
        "Control+Alt+Shift+KeyD",
        "Control+Alt+KeyA",
    ),
    hk(HotkeyAction::Ocr, "Control+Alt+KeyS", "Alt+Shift+KeyS"),
    hk(HotkeyAction::OcrTranslate, "Control+Alt+Shift+KeyS", "Alt+Shift+KeyT"),
    hk(HotkeyAction::Clipboard, "Control+Alt+KeyV", "Alt+Shift+KeyV"),
];

/// Linux：首选同 Windows；备选为 Super+Alt 族（按规范顺序写作 Alt+Super），输入翻译备选 Ctrl+Alt+A
pub const LINUX_HOTKEYS: [HotkeyDefault; 5] = [
    hk(HotkeyAction::TranslateSelection, "Control+Alt+KeyD", "Alt+Super+KeyD"),
    hk(
        HotkeyAction::TranslateInput,
        "Control+Alt+Shift+KeyD",
        "Control+Alt+KeyA",
    ),
    hk(HotkeyAction::Ocr, "Control+Alt+KeyS", "Alt+Super+KeyS"),
    hk(HotkeyAction::OcrTranslate, "Control+Alt+Shift+KeyS", "Alt+Super+KeyT"),
    hk(HotkeyAction::Clipboard, "Control+Alt+KeyV", "Alt+Super+KeyV"),
];

/// 某平台的默认快捷键表（顺序同 `HotkeyAction::ALL`）
pub const fn hotkey_table(os: OsKind) -> &'static [HotkeyDefault; 5] {
    match os {
        OsKind::Macos => &MACOS_HOTKEYS,
        OsKind::Windows => &WINDOWS_HOTKEYS,
        OsKind::Linux => &LINUX_HOTKEYS,
    }
}

/// 某平台某动作的默认快捷键
pub const fn hotkey_default(os: OsKind, action: HotkeyAction) -> HotkeyDefault {
    let table = hotkey_table(os);
    let idx = match action {
        HotkeyAction::TranslateSelection => 0,
        HotkeyAction::TranslateInput => 1,
        HotkeyAction::Ocr => 2,
        HotkeyAction::OcrTranslate => 3,
        HotkeyAction::Clipboard => 4,
    };
    table[idx]
}

/// 双击修饰键的平台默认值：macOS 右 ⌥，Windows / Linux 右 Ctrl
pub const fn default_double_tap_key(os: OsKind) -> ModifierKey {
    match os {
        OsKind::Macos => ModifierKey::RightAlt,
        OsKind::Windows | OsKind::Linux => ModifierKey::RightCtrl,
    }
}

/// 双击间隔只允许这三个值（毫秒）
pub const DOUBLE_TAP_INTERVALS_MS: [u32; 3] = [200, 300, 400];

/// 内置风格 id，顺序固定
pub const BUILTIN_STYLE_IDS: [&str; 6] = ["natural", "technical", "academic", "casual", "learn", "literal"];

/// 默认风格
pub const DEFAULT_STYLE_ID: &str = "natural";

/// 自定义风格 id 的前缀（后接 uuid 前 8 位）
pub const CUSTOM_STYLE_PREFIX: &str = "custom-";

/// 强调色：跟随系统
pub const ACCENT_SYSTEM: &str = "system";

/// 强调色预设名
pub const ACCENT_PRESETS: [&str; 8] = ["blue", "purple", "pink", "red", "orange", "yellow", "green", "graphite"];

/// 剪贴板保留天数只允许这三个值（null 表示不限）
pub const CLIPBOARD_MAX_AGE_DAYS: [u32; 3] = [7, 30, 90];

/// 预置的剪贴板排除名单（三端条目都在，运行时按 os 过滤；id 需在实现时逐个核实）
pub fn default_excluded_apps() -> Vec<ExcludedApp> {
    const TABLE: [(OsKind, &str, &str); 12] = [
        (OsKind::Macos, "com.1password.1password", "1Password"),
        (OsKind::Macos, "com.agilebits.onepassword7", "1Password 7"),
        (OsKind::Macos, "com.bitwarden.desktop", "Bitwarden"),
        (OsKind::Macos, "org.keepassxc.keepassxc", "KeePassXC"),
        (OsKind::Macos, "com.apple.keychainaccess", "Keychain Access"),
        (OsKind::Windows, "1password.exe", "1Password"),
        (OsKind::Windows, "bitwarden.exe", "Bitwarden"),
        (OsKind::Windows, "keepassxc.exe", "KeePassXC"),
        (OsKind::Windows, "keepass.exe", "KeePass"),
        (OsKind::Linux, "1password", "1Password"),
        (OsKind::Linux, "bitwarden", "Bitwarden"),
        (OsKind::Linux, "keepassxc", "KeePassXC"),
    ];
    TABLE
        .iter()
        .map(|&(os, app_id, name)| ExcludedApp {
            os,
            app_id: app_id.to_owned(),
            name: name.to_owned(),
        })
        .collect()
}

/// 设置项的取值范围（闭区间）。超出范围的值由 settings/validate.rs 夹紧到边界并记 warn 日志
pub mod ranges {
    /// hotkeys.copy_wait_ms
    pub const COPY_WAIT_MS: (u32, u32) = (50, 500);
    /// translate.long_text_threshold
    pub const LONG_TEXT_THRESHOLD: (u32, u32) = (500, 20_000);
    /// translate.mouse_leave_px
    pub const MOUSE_LEAVE_PX: (u32, u32) = (80, 400);
    /// ocr.max_edge_px
    pub const OCR_MAX_EDGE_PX: (u32, u32) = (512, 4096);
    /// ocr.history_limit
    pub const OCR_HISTORY_LIMIT: (u32, u32) = (10, 500);
    /// appearance.font_size
    pub const FONT_SIZE: (u32, u32) = (12, 18);
    /// appearance.translate_min_width
    pub const TRANSLATE_MIN_WIDTH: (u32, u32) = (320, 480);
    /// appearance.translate_max_width
    pub const TRANSLATE_MAX_WIDTH: (u32, u32) = (440, 720);
    /// appearance.clipboard_width
    pub const CLIPBOARD_WIDTH: (u32, u32) = (360, 560);
    /// appearance.max_height_ratio
    pub const MAX_HEIGHT_RATIO: (f32, f32) = (0.4, 0.8);
    /// advanced.unload_panel_after_idle_min（null 表示关闭）。下限 5 分钟，
    /// 以便设成建议值 10 分钟
    pub const UNLOAD_PANEL_AFTER_IDLE_MIN: (u32, u32) = (5, 1440);
}

/// 「空闲卸载面板」开关打开时的分钟数（设置页开关的初值；可按冷唤出的实测耗时调整，前端
/// `GeneralPage.tsx` 的 `UNLOAD_RANGE.initial` 与之一致）
pub const UNLOAD_PANEL_SUGGESTED_MIN: u32 = 10;

/// 新配置（首次运行写出 settings.json、损坏后重置）里 `advanced.unload_panel_after_idle_min` 的平台默认值。
///
/// `Settings::default()` 与黄金样例保持平台无关（恒 null）；平台差异只在这里，经 [`crate::Settings::for_os`] 进入新配置。
/// 已有配置文件里的值（包括显式的 null）不受影响。
///
/// - **macOS：关**（冷唤出的「内容首帧」p95 ≤ 150 ms 时才值得默认开启）。实测
///   （release，卸载且 WebContent / GPU 进程都已退出、≥ 70 s 之后经总线触发，Boot.prepare 与对照组交替）：卡片上屏仍由原生外壳在
///   150 ms 内完成，内容首帧 p95 仍在 150 ms 以上——剩下的是 WebKit 拉起新的 WebContent 进程（触发 → 导航开始约 70 ms）与首屏 JS
///   的模块求值（约 80 ms）。开启时把这一分支改为 `Some(UNLOAD_PANEL_SUGGESTED_MIN)`——长时间空闲时
///   WebView 合计由回收后的约 55–58 MiB 降到约 6 MiB（WebContent 退出、GPU 进程约 1 分钟内跟着退出，只剩 Networking）；
/// - Windows / Linux：关（这两端是 Css 帧模式，没有原生外壳可以先显示）
pub const fn unload_panel_default(os: OsKind) -> Option<u32> {
    match os {
        OsKind::Macos | OsKind::Windows | OsKind::Linux => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 规范修饰键顺序
    const MOD_ORDER: [&str; 4] = ["Control", "Alt", "Shift", "Super"];

    fn assert_canonical(combo: &str) {
        let parts: Vec<&str> = combo.split('+').collect();
        let (key, mods) = parts.split_last().expect("非空组合");
        assert!(key.starts_with("Key") && key.len() == 4, "主键应为 KeyX：{combo}");
        let idx: Vec<usize> = mods
            .iter()
            .map(|m| MOD_ORDER.iter().position(|o| o == m).expect("未知修饰键"))
            .collect();
        assert!(!idx.is_empty(), "至少一个修饰键：{combo}");
        assert!(idx.windows(2).all(|w| w[0] < w[1]), "修饰键顺序不规范：{combo}");
    }

    #[test]
    fn tables_are_canonical_and_unique() {
        for os in [OsKind::Macos, OsKind::Windows, OsKind::Linux] {
            let table = hotkey_table(os);
            let mut all = Vec::new();
            for (i, d) in table.iter().enumerate() {
                assert_eq!(d.action, HotkeyAction::ALL[i], "顺序应与 HotkeyAction::ALL 一致");
                assert_eq!(hotkey_default(os, d.action), *d);
                assert_canonical(d.primary);
                assert_canonical(d.fallback);
                assert_ne!(d.primary, d.fallback);
                all.push(d.primary);
                all.push(d.fallback);
            }
            let mut dedup = all.clone();
            dedup.sort_unstable();
            dedup.dedup();
            assert_eq!(dedup.len(), all.len(), "{os:?} 的默认表存在重复组合");
        }
    }

    #[test]
    fn excluded_apps_cover_all_os() {
        let apps = default_excluded_apps();
        for os in [OsKind::Macos, OsKind::Windows, OsKind::Linux] {
            assert!(apps.iter().any(|a| a.os == os));
        }
    }
}
