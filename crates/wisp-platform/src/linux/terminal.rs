//! 终端模拟器识别（纯逻辑，只依赖 std）。
//!
//! 「应用 → 复制键 / 粘贴键」映射表由用户条目与内置默认表组成，正常情况下终端都能在表中命中。
//! 这里是表外的安全网：对没有条目、但看起来是终端的应用，**绝不发送 Ctrl+C**（会向前台进程发 SIGINT），
//! 粘贴改用 Ctrl+Shift+V。

/// 常见终端的 WM_CLASS（instance 或 class，比较时不区分大小写）
const KNOWN: &[&str] = &[
    "gnome-terminal",
    "gnome-terminal-server",
    "org.gnome.console",
    "kgx",
    "org.gnome.ptyxis",
    "ptyxis",
    "konsole",
    "xfce4-terminal",
    "mate-terminal",
    "lxterminal",
    "qterminal",
    "tilix",
    "terminator",
    "alacritty",
    "kitty",
    "wezterm",
    "org.wezfurlong.wezterm",
    "foot",
    "footclient",
    "xterm",
    "uxterm",
    "urxvt",
    "rxvt",
    "st",
    "st-256color",
    "sakura",
    "terminology",
    "guake",
    "tilda",
    "yakuake",
    "cool-retro-term",
    "deepin-terminal",
    "io.elementary.terminal",
    "com.system76.cosmicterm",
    "ghostty",
    "com.mitchellh.ghostty",
    "contour",
    "rio",
    "blackbox",
    "com.raggesilver.blackbox",
    "warp",
    "dev.warp.warp",
    "hyper",
    "tabby",
    "termite",
    "zutty",
    "eterm",
    "aterm",
];

/// 根据 WM_CLASS 的 class / instance 判断是否为终端
pub fn is_terminal(class: Option<&str>, instance: Option<&str>) -> bool {
    [class, instance].into_iter().flatten().any(|name| {
        let lower = name.trim().to_ascii_lowercase();
        if lower.is_empty() {
            return false;
        }
        KNOWN.contains(&lower.as_str())
            || lower.ends_with("terminal")
            || lower.ends_with("-term")
            || lower.ends_with("term-server")
            || lower.contains("terminal-")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_terminals() {
        assert!(is_terminal(Some("Gnome-terminal"), Some("gnome-terminal-server")));
        assert!(is_terminal(Some("kitty"), None));
        assert!(is_terminal(None, Some("Alacritty")));
        assert!(is_terminal(Some("XTerm"), Some("xterm")));
        assert!(is_terminal(Some("St"), Some("st-256color")));
        assert!(is_terminal(Some("Some-Terminal"), None));
        assert!(is_terminal(Some("org.gnome.Console"), Some("kgx")));
    }

    #[test]
    fn ignores_regular_apps() {
        assert!(!is_terminal(Some("firefox"), Some("Navigator")));
        assert!(!is_terminal(Some("Code"), Some("code")));
        assert!(!is_terminal(Some("Steam"), Some("steam")));
        assert!(!is_terminal(Some(""), None));
        assert!(!is_terminal(None, None));
    }
}
