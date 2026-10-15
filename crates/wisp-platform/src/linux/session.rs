//! Linux 会话检测与降级原因。
//!
//! 纯逻辑，只依赖 std：输入是环境变量的快照，输出是显示服务器种类、桌面环境，以及各能力不可用时上报给前端的
//! 原因 key（`errors.platform.<reason>`，文案在前端的 errors.json）。`tests/smoke_linux.rs` 用 `#[path]`
//! 引入本文件，所以在 macOS 开发机上也能跑这些单测。

/// 显示服务器
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayServer {
    /// X11 会话（DISPLAY 可用，且不是 Wayland 会话）
    X11,
    /// Wayland 会话（可能同时有 XWayland 的 DISPLAY）
    Wayland,
    /// 既没有 DISPLAY 也没有 WAYLAND_DISPLAY（无头环境、TTY、CI）
    Headless,
}

/// 桌面环境（只区分降级策略和日志里需要的几类）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Desktop {
    Gnome,
    Kde,
    /// wlroots 系合成器（sway、Hyprland、river、labwc、wayfire…）
    Wlroots,
    Xfce,
    Cinnamon,
    Mate,
    /// 其他已声明的桌面（原样保存 XDG_CURRENT_DESKTOP 的第一段，便于日志）
    Other(String),
    Unknown,
}

/// 会话检测所需的环境变量快照
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionEnv {
    pub xdg_session_type: Option<String>,
    pub wayland_display: Option<String>,
    pub display: Option<String>,
    pub xdg_current_desktop: Option<String>,
    pub desktop_session: Option<String>,
}

impl SessionEnv {
    /// 读取当前进程的环境变量
    pub fn from_process() -> Self {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        Self {
            xdg_session_type: get("XDG_SESSION_TYPE"),
            wayland_display: get("WAYLAND_DISPLAY"),
            display: get("DISPLAY"),
            xdg_current_desktop: get("XDG_CURRENT_DESKTOP"),
            desktop_session: get("DESKTOP_SESSION"),
        }
    }
}

/// 检测结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub server: DisplayServer,
    pub desktop: Desktop,
    /// 有可连接的 X DISPLAY（X11 会话，或 Wayland 下的 XWayland）
    pub has_x_display: bool,
}

impl Session {
    /// 检测当前进程所在的会话
    pub fn detect() -> Self {
        detect(&SessionEnv::from_process())
    }

    /// 日志用的简短描述，例如 `wayland/gnome`
    pub fn describe(&self) -> String {
        let server = match self.server {
            DisplayServer::X11 => "x11",
            DisplayServer::Wayland => "wayland",
            DisplayServer::Headless => "headless",
        };
        let desktop = match &self.desktop {
            Desktop::Gnome => "gnome",
            Desktop::Kde => "kde",
            Desktop::Wlroots => "wlroots",
            Desktop::Xfce => "xfce",
            Desktop::Cinnamon => "cinnamon",
            Desktop::Mate => "mate",
            Desktop::Other(name) => name.as_str(),
            Desktop::Unknown => "unknown",
        };
        format!("{server}/{desktop}")
    }
}

/// 按环境变量判断会话。规则与 desktop 的 `app::runtime::is_wayland_session` 保持一致：
/// WAYLAND_DISPLAY 非空或 XDG_SESSION_TYPE=wayland 即视为 Wayland（XWayland 的 DISPLAY 不改变这一结论）
pub fn detect(env: &SessionEnv) -> Session {
    let non_empty = |v: &Option<String>| v.as_deref().is_some_and(|s| !s.trim().is_empty());
    let session_type = env.xdg_session_type.as_deref().map(str::to_ascii_lowercase);
    let has_x_display = non_empty(&env.display);
    let wayland = non_empty(&env.wayland_display) || session_type.as_deref() == Some("wayland");
    let server = if wayland {
        DisplayServer::Wayland
    } else if has_x_display {
        DisplayServer::X11
    } else {
        DisplayServer::Headless
    };
    Session {
        server,
        desktop: classify_desktop(env.xdg_current_desktop.as_deref(), env.desktop_session.as_deref()),
        has_x_display,
    }
}

/// 解析 XDG_CURRENT_DESKTOP（冒号分隔，例如 `ubuntu:GNOME`、`Budgie:GNOME`、`KDE`、`sway`），
/// 取不到时退回 DESKTOP_SESSION
pub fn classify_desktop(current: Option<&str>, session: Option<&str>) -> Desktop {
    let mut first_named: Option<String> = None;
    for raw in current.into_iter().flat_map(|s| s.split(':')).chain(session) {
        let name = raw.trim();
        if name.is_empty() {
            continue;
        }
        let lower = name.to_ascii_lowercase();
        let known = match lower.as_str() {
            "gnome" | "gnome-classic" | "gnome-flashback" | "ubuntu" | "unity" | "budgie" | "pantheon" | "pop" => {
                Some(Desktop::Gnome)
            }
            "kde" | "plasma" | "plasmawayland" | "plasmax11" => Some(Desktop::Kde),
            "sway" | "hyprland" | "river" | "labwc" | "wayfire" | "wlroots" | "niri" | "dwl" | "cage" | "qtile" => {
                Some(Desktop::Wlroots)
            }
            "xfce" | "xfce4" | "xubuntu" => Some(Desktop::Xfce),
            "x-cinnamon" | "cinnamon" => Some(Desktop::Cinnamon),
            "mate" => Some(Desktop::Mate),
            _ => None,
        };
        match known {
            // ubuntu / budgie 等「品牌名」后面通常还跟着 GNOME，按 GNOME 处理即可
            Some(d) => return d,
            None => {
                if first_named.is_none() {
                    first_named = Some(lower);
                }
            }
        }
    }
    first_named.map_or(Desktop::Unknown, Desktop::Other)
}

/// 需要 X 连接的功能（决定 Wayland / 无头会话下报告哪个原因）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    /// 取词、`has_selection`、前台应用、显示器 / 光标、键盘布局、浮标窗口
    Selection,
    /// 模拟粘贴
    Paste,
    /// 激活其他应用
    Focus,
    /// 剪贴板读取、监听、快照与恢复
    ClipboardWatch,
    /// 写剪贴板（Wayland 下经 XWayland 完成「只复制」）
    ClipboardWrite,
    /// 扩展触发与外部点击
    InputHooks,
}

impl Feature {
    /// Wayland 下不可用时的原因
    pub fn wayland_reason(self) -> &'static str {
        match self {
            Self::Selection => reason::WAYLAND_SELECTION,
            Self::Paste => reason::WAYLAND_PASTE,
            Self::Focus => reason::WAYLAND_FOCUS,
            Self::ClipboardWatch => reason::WAYLAND_CLIPBOARD_WATCH,
            Self::ClipboardWrite => reason::WAYLAND_CLIPBOARD,
            Self::InputHooks => reason::WAYLAND_INPUT_HOOKS,
        }
    }
}

/// Wayland 会话里，合成器是否会把 XWayland 的 CLIPBOARD 同步给 Wayland 应用（不要求 XWayland 窗口获焦）。
/// KWin 的 XWayland 剪贴板桥只在 X11 窗口获焦时接受 X11 一侧的新 owner（「clipboard is only allowed to be acquired
/// when Xwayland has focus」），Wisp 的面板是原生 Wayland 窗口，写入不会生效；Mutter、wlroots 系没有这个限制
pub fn xwayland_clipboard_bridged(desktop: &Desktop) -> bool {
    !matches!(desktop, Desktop::Kde)
}

/// 某功能在这个会话下是否经 X 连接完成。Ok：去连接 X（失败时的原因见 [`connect_failure`]）；
/// Err：不可用，附 `errors.platform.*` 原因
pub fn route(session: &Session, feature: Feature) -> Result<(), &'static str> {
    match session.server {
        DisplayServer::X11 => Ok(()),
        DisplayServer::Headless => Err(reason::NO_DISPLAY),
        DisplayServer::Wayland => match feature {
            // 「只复制」：KWin 不接受后台 XWayland 客户端的剪贴板，写入不会生效，绝不返回 Ok
            Feature::ClipboardWrite if !xwayland_clipboard_bridged(&session.desktop) => {
                Err(reason::WAYLAND_CLIPBOARD_BRIDGE)
            }
            Feature::ClipboardWrite if session.has_x_display => Ok(()),
            _ => Err(feature.wayland_reason()),
        },
    }
}

/// [`route`] 为 Ok、但连接 X 服务器失败时的原因
pub fn connect_failure(session: &Session, feature: Feature) -> &'static str {
    match session.server {
        DisplayServer::X11 => reason::X11_CONNECT_FAILED,
        DisplayServer::Wayland => feature.wayland_reason(),
        DisplayServer::Headless => reason::NO_DISPLAY,
    }
}

/// 前端 i18n key（三端共用，见 [`crate::reason`]）
pub use crate::reason;

#[cfg(test)]
mod tests {
    use super::*;

    fn env(
        session_type: Option<&str>,
        wayland: Option<&str>,
        display: Option<&str>,
        desktop: Option<&str>,
    ) -> SessionEnv {
        SessionEnv {
            xdg_session_type: session_type.map(str::to_owned),
            wayland_display: wayland.map(str::to_owned),
            display: display.map(str::to_owned),
            xdg_current_desktop: desktop.map(str::to_owned),
            desktop_session: None,
        }
    }

    #[test]
    fn x11_session_with_display() {
        let s = detect(&env(Some("x11"), None, Some(":0"), Some("ubuntu:GNOME")));
        assert_eq!(s.server, DisplayServer::X11);
        assert_eq!(s.desktop, Desktop::Gnome);
        assert!(s.has_x_display);
        assert_eq!(s.describe(), "x11/gnome");
    }

    #[test]
    fn wayland_wins_over_xwayland_display() {
        let s = detect(&env(Some("wayland"), Some("wayland-0"), Some(":0"), Some("GNOME")));
        assert_eq!(s.server, DisplayServer::Wayland);
        assert!(s.has_x_display, "XWayland 的 DISPLAY 仍然记录下来");
    }

    #[test]
    fn wayland_display_alone_means_wayland() {
        let s = detect(&env(None, Some("wayland-1"), None, Some("sway")));
        assert_eq!(s.server, DisplayServer::Wayland);
        assert_eq!(s.desktop, Desktop::Wlroots);
        assert!(!s.has_x_display);
    }

    #[test]
    fn session_type_wayland_without_socket_env() {
        let s = detect(&env(Some("Wayland"), None, None, Some("KDE")));
        assert_eq!(s.server, DisplayServer::Wayland);
        assert_eq!(s.desktop, Desktop::Kde);
    }

    #[test]
    fn empty_values_are_ignored() {
        let s = detect(&env(Some("tty"), Some(""), Some("  "), None));
        assert_eq!(s.server, DisplayServer::Headless);
        assert!(!s.has_x_display);
        assert_eq!(s.desktop, Desktop::Unknown);
    }

    #[test]
    fn desktop_classification() {
        assert_eq!(classify_desktop(Some("Budgie:GNOME"), None), Desktop::Gnome);
        assert_eq!(classify_desktop(Some("X-Cinnamon"), None), Desktop::Cinnamon);
        assert_eq!(classify_desktop(Some("XFCE"), None), Desktop::Xfce);
        assert_eq!(classify_desktop(Some("Hyprland"), None), Desktop::Wlroots);
        assert_eq!(classify_desktop(Some("LXQt"), None), Desktop::Other("lxqt".to_owned()));
        assert_eq!(classify_desktop(None, Some("plasma")), Desktop::Kde);
        assert_eq!(classify_desktop(Some(""), None), Desktop::Unknown);
    }

    fn session(server: DisplayServer, desktop: Desktop, has_x_display: bool) -> Session {
        Session {
            server,
            desktop,
            has_x_display,
        }
    }

    const ALL_FEATURES: [Feature; 6] = [
        Feature::Selection,
        Feature::Paste,
        Feature::Focus,
        Feature::ClipboardWatch,
        Feature::ClipboardWrite,
        Feature::InputHooks,
    ];

    #[test]
    fn x11_routes_everything_through_the_connection() {
        let s = session(DisplayServer::X11, Desktop::Gnome, true);
        for f in ALL_FEATURES {
            assert_eq!(route(&s, f), Ok(()), "{f:?}");
            assert_eq!(connect_failure(&s, f), reason::X11_CONNECT_FAILED);
        }
    }

    #[test]
    fn headless_reports_no_display() {
        let s = session(DisplayServer::Headless, Desktop::Unknown, false);
        for f in ALL_FEATURES {
            assert_eq!(route(&s, f), Err(reason::NO_DISPLAY), "{f:?}");
            assert_eq!(connect_failure(&s, f), reason::NO_DISPLAY);
        }
    }

    #[test]
    fn wayland_degrades_with_reasons() {
        // (桌面, 有 XWayland, 功能, 期望)
        let table: &[(Desktop, bool, Feature, Result<(), &str>)] = &[
            (Desktop::Gnome, true, Feature::Selection, Err(reason::WAYLAND_SELECTION)),
            (Desktop::Gnome, true, Feature::Paste, Err(reason::WAYLAND_PASTE)),
            (Desktop::Gnome, true, Feature::Focus, Err(reason::WAYLAND_FOCUS)),
            (
                Desktop::Gnome,
                true,
                Feature::ClipboardWatch,
                Err(reason::WAYLAND_CLIPBOARD_WATCH),
            ),
            (
                Desktop::Gnome,
                true,
                Feature::InputHooks,
                Err(reason::WAYLAND_INPUT_HOOKS),
            ),
            // 「只复制」：GNOME / wlroots 经 XWayland 写入
            (Desktop::Gnome, true, Feature::ClipboardWrite, Ok(())),
            (Desktop::Wlroots, true, Feature::ClipboardWrite, Ok(())),
            // 没有 XWayland：无法写入
            (
                Desktop::Gnome,
                false,
                Feature::ClipboardWrite,
                Err(reason::WAYLAND_CLIPBOARD),
            ),
            // KDE：KWin 不接受后台 XWayland 客户端的剪贴板（有没有 XWayland 都一样）
            (
                Desktop::Kde,
                true,
                Feature::ClipboardWrite,
                Err(reason::WAYLAND_CLIPBOARD_BRIDGE),
            ),
            (
                Desktop::Kde,
                false,
                Feature::ClipboardWrite,
                Err(reason::WAYLAND_CLIPBOARD_BRIDGE),
            ),
            (Desktop::Kde, true, Feature::Paste, Err(reason::WAYLAND_PASTE)),
            (
                Desktop::Kde,
                true,
                Feature::InputHooks,
                Err(reason::WAYLAND_INPUT_HOOKS),
            ),
        ];
        for (desktop, xwayland, feature, want) in table {
            let s = session(DisplayServer::Wayland, desktop.clone(), *xwayland);
            assert_eq!(
                route(&s, *feature),
                *want,
                "{desktop:?} xwayland={xwayland} {feature:?}"
            );
        }
        // XWayland 连不上时，「只复制」报告没有 XWayland
        let s = session(DisplayServer::Wayland, Desktop::Gnome, true);
        assert_eq!(connect_failure(&s, Feature::ClipboardWrite), reason::WAYLAND_CLIPBOARD);
        for f in ALL_FEATURES {
            let key = f.wayland_reason();
            assert!(key.starts_with("errors.platform.wayland_"), "{f:?} → {key}");
            assert!(reason::ALL.contains(&key));
        }
    }

    #[test]
    fn reason_keys_are_namespaced_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for key in reason::ALL {
            assert!(key.starts_with("errors.platform."), "{key}");
            assert!(seen.insert(*key), "重复的 key：{key}");
        }
    }
}
