//! 各平台内置的「应用 → 复制键 / 粘贴键」默认表以及匹配函数。
//!
//! 表项的 `copy` / `paste` 是 global-hotkey 规范串；`"none"` 表示不发复制键（例如 xterm：选中即进 PRIMARY，
//! 发 Ctrl+Shift+C 反而会中断前台程序）；`null` 表示该项沿用后面的条目或平台默认。合并表的顺序是用户条目在前、
//! 内置表在后，**逐字段**取第一个有值的条目：用户只改了某终端的粘贴键时，复制键仍按内置表处理。
//!
//! 匹配规则：macOS 比较 bundle id；Windows 比较 exe 文件名（`.exe` 可省略）；Linux 比较 WM_CLASS 的 class 或
//! instance（`AppRef.id` / `AppRef.alt_id`）。一律不区分大小写。

use wisp_types::{AppKeymap, AppRef, OsKind};

use super::combo::Combo;

/// `copy` / `paste` 的保留值：不发送按键
pub const KEY_NONE: &str = "none";

/// 平台默认的复制键
pub fn default_copy(os: OsKind) -> &'static str {
    match os {
        OsKind::Macos => "Super+KeyC",
        OsKind::Windows | OsKind::Linux => "Control+KeyC",
    }
}

/// 平台默认的粘贴键
pub fn default_paste(os: OsKind) -> &'static str {
    match os {
        OsKind::Macos => "Super+KeyV",
        OsKind::Windows | OsKind::Linux => "Control+KeyV",
    }
}

/// 某平台的内置默认表（用户条目优先，内置表在后）
pub fn builtin(os: OsKind) -> Vec<AppKeymap> {
    const TERMINAL_COPY: &str = "Control+Shift+KeyC";
    const TERMINAL_PASTE: &str = "Control+Shift+KeyV";
    let entry = |app_id: &str, copy: &str, paste: &str| AppKeymap {
        os,
        app_id: app_id.to_owned(),
        copy: Some(copy.to_owned()),
        paste: Some(paste.to_owned()),
    };
    match os {
        // macOS 一律 ⌘C / ⌘V（终端也遵循），不需要条目
        OsKind::Macos => Vec::new(),
        OsKind::Windows => vec![
            entry("WindowsTerminal.exe", TERMINAL_COPY, "Control+KeyV"),
            entry("OpenConsole.exe", TERMINAL_COPY, "Control+KeyV"),
            entry("conhost.exe", TERMINAL_COPY, "Control+KeyV"),
            entry("mintty.exe", "Control+Insert", "Shift+Insert"),
        ],
        OsKind::Linux => {
            let mut v: Vec<AppKeymap> = [
                "gnome-terminal-server",
                "gnome-terminal",
                "org.gnome.Console",
                "kgx",
                "konsole",
                "xfce4-terminal",
                "tilix",
                "terminator",
                "Alacritty",
                "kitty",
                "wezterm",
                "org.wezfurlong.wezterm",
            ]
            .iter()
            .map(|id| entry(id, TERMINAL_COPY, TERMINAL_PASTE))
            .collect();
            v.extend(["xterm", "UXTerm"].iter().map(|id| entry(id, KEY_NONE, "Shift+Insert")));
            v
        }
    }
}

/// 在合并后的表中查找某应用的**第一个**条目（用户条目在前，先命中者优先），只比较 `app_id` 一个标识。
///
/// 它不做逐字段合并：用户条目只填了 `paste`、`copy` 为 null 时，返回的是用户条目，`copy` 就成了「没有值」，
/// 而实际生效的复制键来自内置表。要取实际发送的按键请用 [`copy_key`] / [`paste_key`]；要按应用找条目（Linux 同时
/// 比较 WM_CLASS 的 class 与 instance）请用 [`resolve_app`]
#[deprecated(note = "整条目匹配、不比较 alt_id：取按键用 copy_key / paste_key，按应用找条目用 resolve_app")]
pub fn resolve<'a>(keymaps: &'a [AppKeymap], os: OsKind, app_id: &str) -> Option<&'a AppKeymap> {
    keymaps.iter().find(|k| k.os == os && ids_match(os, &k.app_id, app_id))
}

/// 在合并后的表中查找匹配某应用的第一个条目（规则与 [`copy_key`] / [`paste_key`] 相同：Windows 忽略 `.exe`，
/// Linux 同时比较 `AppRef.id` 与 `AppRef.alt_id`）。用于设置页展示「哪一条规则命中了这个应用」；
/// 取实际发送的按键请用 [`copy_key`] / [`paste_key`]（逐字段回落）
pub fn resolve_app<'a>(keymaps: &'a [AppKeymap], os: OsKind, app: &AppRef) -> Option<&'a AppKeymap> {
    keymaps.iter().find(|k| k.os == os && app_matches(os, &k.app_id, app))
}

/// 某应用应发送的按键
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyChoice {
    /// 发送该组合键
    Send(Combo),
    /// 不发送（`"none"`）
    Skip,
}

/// 复制键：逐字段在合并表中查找，没有条目时用平台默认；条目里的串无法解析时记日志并退回平台默认
pub fn copy_key(keymaps: &[AppKeymap], os: OsKind, app: Option<&AppRef>) -> KeyChoice {
    pick(keymaps, os, app, |k| k.copy.as_deref(), default_copy(os))
}

/// 粘贴键：规则同 [`copy_key`]
pub fn paste_key(keymaps: &[AppKeymap], os: OsKind, app: Option<&AppRef>) -> KeyChoice {
    pick(keymaps, os, app, |k| k.paste.as_deref(), default_paste(os))
}

fn pick(
    keymaps: &[AppKeymap],
    os: OsKind,
    app: Option<&AppRef>,
    field: impl Fn(&AppKeymap) -> Option<&str>,
    fallback: &str,
) -> KeyChoice {
    let chosen = app.and_then(|app| {
        keymaps
            .iter()
            .filter(|k| k.os == os && app_matches(os, &k.app_id, app))
            .find_map(&field)
    });
    let value = chosen.unwrap_or(fallback);
    if value.trim().eq_ignore_ascii_case(KEY_NONE) {
        return KeyChoice::Skip;
    }
    match Combo::parse(value) {
        Some(c) => KeyChoice::Send(c),
        None => {
            log::warn!("keymap: unparsable combo {value:?}, falling back to {fallback}");
            Combo::parse(fallback).map_or(KeyChoice::Skip, KeyChoice::Send)
        }
    }
}

/// 条目的 app_id 是否匹配某个应用（Linux 同时比较 class 与 instance）
pub fn app_matches(os: OsKind, entry_id: &str, app: &AppRef) -> bool {
    ids_match(os, entry_id, &app.id) || app.alt_id.as_deref().is_some_and(|alt| ids_match(os, entry_id, alt))
}

fn ids_match(os: OsKind, a: &str, b: &str) -> bool {
    match os {
        OsKind::Windows => strip_exe(a).eq_ignore_ascii_case(strip_exe(b)),
        OsKind::Macos | OsKind::Linux => a.eq_ignore_ascii_case(b),
    }
}

fn strip_exe(s: &str) -> &str {
    let s = s.trim();
    match s.len().checked_sub(4) {
        Some(cut) if s.is_char_boundary(cut) && s[cut..].eq_ignore_ascii_case(".exe") => &s[..cut],
        _ => s,
    }
}
