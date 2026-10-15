//! 系统外观的判定规则（纯逻辑，只依赖 std）。
//!
//! 数据来源按优先级：xdg-desktop-portal 的 Settings 接口（`org.freedesktop.appearance` 命名空间，
//! 另读 GNOME / KDE 各自的动画开关）→ XSETTINGS（仅 X11）→ `GTK_THEME` 环境变量 → `~/.config/gtk-3.0/settings.ini`。
//! 这里只负责把这些原始值合成 `dark_mode` / `system_accent` / `reduce_motion` / `reduce_transparency`。

/// 合成外观所需的全部原始输入（缺失项为 None）
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AppearanceInputs {
    /// portal `org.freedesktop.appearance` / `color-scheme`：0 无偏好、1 深色、2 浅色
    pub color_scheme: Option<u32>,
    /// portal `accent-color`：sRGB 三元组，各分量 0–1；超出范围表示「未设置」
    pub accent: Option<(f64, f64, f64)>,
    /// portal `contrast`：0 无偏好、1 高对比度
    pub contrast: Option<u32>,
    /// portal `reduced-motion`：0 无偏好、1 减少动效（xdg-desktop-portal 1.20+）
    pub reduced_motion: Option<u32>,
    /// GNOME `org.gnome.desktop.interface` / `enable-animations`（经 portal 读取）
    pub gnome_enable_animations: Option<bool>,
    /// KDE `org.kde.kdeglobals.KDE` / `AnimationDurationFactor`（0 表示关闭动画）
    pub kde_animation_factor: Option<f64>,
    /// XSETTINGS `Gtk/EnableAnimations`
    pub xsettings_enable_animations: Option<bool>,
    /// GTK 主题名：portal 的 `gtk-theme`、XSETTINGS 的 `Net/ThemeName`、`GTK_THEME`、settings.ini，取第一个非空值
    pub gtk_theme: Option<String>,
    /// settings.ini 的 `gtk-application-prefer-dark-theme`
    pub gtk_prefer_dark: Option<bool>,
}

/// 系统当前是否为深色：portal 明确给出偏好时以它为准；否则看 GTK 主题名（`*-dark`、`Adwaita:dark` 等）
pub fn resolve_dark(i: &AppearanceInputs) -> bool {
    match i.color_scheme {
        Some(1) => return true,
        Some(2) => return false,
        _ => {}
    }
    if i.gtk_prefer_dark == Some(true) {
        return true;
    }
    i.gtk_theme.as_deref().is_some_and(theme_is_dark)
}

/// 主题名是否表示深色变体
pub fn theme_is_dark(name: &str) -> bool {
    name.to_ascii_lowercase().contains("dark")
}

/// portal 强调色 → `#RRGGBB`；任一分量不在 [0, 1] 内（portal 用它表示未设置）或不是有限数时返回 None
pub fn accent_hex(rgb: (f64, f64, f64)) -> Option<String> {
    let (r, g, b) = rgb;
    let ok = |v: f64| v.is_finite() && (0.0..=1.0).contains(&v);
    if !(ok(r) && ok(g) && ok(b)) {
        return None;
    }
    let c = |v: f64| (v * 255.0).round().clamp(0.0, 255.0) as u8;
    Some(format!("#{:02X}{:02X}{:02X}", c(r), c(g), c(b)))
}

/// 是否减少动效：portal `reduced-motion` 优先；其次 GNOME / KDE 的动画开关，最后 XSETTINGS
pub fn resolve_reduce_motion(i: &AppearanceInputs) -> bool {
    if let Some(v) = i.reduced_motion {
        return v == 1;
    }
    if let Some(enabled) = i.gnome_enable_animations {
        return !enabled;
    }
    if let Some(factor) = i.kde_animation_factor
        && factor.is_finite()
    {
        return factor <= f64::EPSILON;
    }
    i.xsettings_enable_animations.is_some_and(|enabled| !enabled)
}

/// 是否降低透明度：Linux 没有独立的系统开关，以「高对比度」为准（高对比度下半透明材质会降低可读性）
pub fn resolve_reduce_transparency(i: &AppearanceInputs) -> bool {
    i.contrast == Some(1)
}

/// `GTK_THEME` 环境变量（形如 `Adwaita:dark`）是否要求深色
pub fn gtk_theme_env_is_dark(value: &str) -> bool {
    let mut parts = value.splitn(2, ':');
    let name = parts.next().unwrap_or_default();
    let variant = parts.next().unwrap_or_default();
    variant.eq_ignore_ascii_case("dark") || theme_is_dark(name)
}

/// 解析 `~/.config/gtk-3.0/settings.ini`（或 gtk-4.0）的 `[Settings]` 段，返回 (主题名, 是否偏好深色)
pub fn parse_gtk_settings_ini(text: &str) -> (Option<String>, Option<bool>) {
    let mut in_settings = false;
    let mut theme = None;
    let mut prefer_dark = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            in_settings = line.eq_ignore_ascii_case("[settings]");
            continue;
        }
        if !in_settings {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        match key.trim() {
            "gtk-theme-name" if !value.is_empty() => theme = Some(value.to_owned()),
            "gtk-application-prefer-dark-theme" => {
                prefer_dark = match value.to_ascii_lowercase().as_str() {
                    "1" | "true" | "yes" => Some(true),
                    "0" | "false" | "no" => Some(false),
                    _ => None,
                };
            }
            _ => {}
        }
    }
    (theme, prefer_dark)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portal_color_scheme_wins() {
        let mut i = AppearanceInputs {
            color_scheme: Some(2),
            gtk_theme: Some("Adwaita-dark".to_owned()),
            ..Default::default()
        };
        assert!(!resolve_dark(&i), "portal 明确浅色时忽略主题名");
        i.color_scheme = Some(1);
        i.gtk_theme = Some("Adwaita".to_owned());
        assert!(resolve_dark(&i));
    }

    #[test]
    fn falls_back_to_theme_name() {
        let mut i = AppearanceInputs {
            color_scheme: Some(0),
            gtk_theme: Some("Yaru-dark".to_owned()),
            ..Default::default()
        };
        assert!(resolve_dark(&i));
        i.gtk_theme = Some("Breeze".to_owned());
        assert!(!resolve_dark(&i));
        i.gtk_prefer_dark = Some(true);
        assert!(resolve_dark(&i));
        assert!(!resolve_dark(&AppearanceInputs::default()));
    }

    #[test]
    fn accent_conversion() {
        assert_eq!(accent_hex((0.207, 0.517, 0.894)).as_deref(), Some("#3584E4"));
        assert_eq!(accent_hex((1.0, 1.0, 1.0)).as_deref(), Some("#FFFFFF"));
        assert_eq!(accent_hex((-1.0, 0.5, 0.5)), None, "超出范围表示未设置");
        assert_eq!(accent_hex((f64::NAN, 0.5, 0.5)), None);
    }

    #[test]
    fn reduce_motion_priority() {
        let mut i = AppearanceInputs {
            gnome_enable_animations: Some(false),
            ..Default::default()
        };
        assert!(resolve_reduce_motion(&i));
        i.reduced_motion = Some(0);
        assert!(!resolve_reduce_motion(&i), "portal reduced-motion 优先");
        let kde = AppearanceInputs {
            kde_animation_factor: Some(0.0),
            ..Default::default()
        };
        assert!(resolve_reduce_motion(&kde));
        let kde_on = AppearanceInputs {
            kde_animation_factor: Some(0.5),
            xsettings_enable_animations: Some(false),
            ..Default::default()
        };
        assert!(!resolve_reduce_motion(&kde_on));
        let xs = AppearanceInputs {
            xsettings_enable_animations: Some(false),
            ..Default::default()
        };
        assert!(resolve_reduce_motion(&xs));
        assert!(!resolve_reduce_motion(&AppearanceInputs::default()));
    }

    #[test]
    fn reduce_transparency_follows_high_contrast() {
        let i = AppearanceInputs {
            contrast: Some(1),
            ..Default::default()
        };
        assert!(resolve_reduce_transparency(&i));
        assert!(!resolve_reduce_transparency(&AppearanceInputs::default()));
    }

    #[test]
    fn gtk_theme_env() {
        assert!(gtk_theme_env_is_dark("Adwaita:dark"));
        assert!(gtk_theme_env_is_dark("Arc-Dark"));
        assert!(!gtk_theme_env_is_dark("Adwaita"));
    }

    #[test]
    fn settings_ini() {
        let text = "# comment\n[Settings]\ngtk-theme-name = Materia-dark\ngtk-application-prefer-dark-theme=1\n[Other]\ngtk-theme-name=Nope\n";
        assert_eq!(
            parse_gtk_settings_ini(text),
            (Some("Materia-dark".to_owned()), Some(true))
        );
        assert_eq!(parse_gtk_settings_ini("gtk-theme-name=X"), (None, None));
        assert_eq!(
            parse_gtk_settings_ini("[Settings]\ngtk-application-prefer-dark-theme=false"),
            (None, Some(false))
        );
    }
}
