//! 设置校验：超出范围的值夹紧到边界并记 warn 日志（范围见 `wisp_types::defaults::ranges`）；`defaults` 指向不存在的
//! provider 或模型时保留原值，在调用时报 `not_configured`；`Binding` 必须是 default、none 或能被 global-hotkey 解析的组合；
//! Provider 的请求头名必须是合法 token，值里不能有 CR / LF。
//!
//! 每处修正记为一条 [`Fix`]：
//! - [`FixKind::Clamped`]：数值夹紧 / 吸附到允许的取值（无损的修正，`settings_patch` 照常接受并记 warn）；
//! - [`FixKind::Reset`] / [`FixKind::Dropped`]：非法值退回默认值、非法条目被丢弃（有损）。读文件与热加载时只记 warn、
//!   照常使用结果；`settings_patch` 遇到有损修正返回 `invalid_argument`、不落盘（前端发来的非法值是 bug）。
//!
//! 日志与错误里只写字段路径，**不写值本身**（请求头的值可能是凭据）。

use std::collections::HashSet;

use wisp_types::defaults::{self, ranges};
use wisp_types::{Binding, OcrMode, SETTINGS_SCHEMA_VERSION, Settings};

/// 修正的种类
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FixKind {
    /// 夹紧到边界 / 吸附到最近的允许值
    Clamped,
    /// 非法值退回默认值
    Reset,
    /// 非法条目被丢弃
    Dropped,
}

/// 一处修正
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Fix {
    /// 字段路径，例如 `appearance.font_size`、`providers[1].headers`
    pub path: String,
    pub kind: FixKind,
}

impl Fix {
    fn new(path: impl Into<String>, kind: FixKind) -> Self {
        Self {
            path: path.into(),
            kind,
        }
    }

    /// 是否为有损修正（退回默认值 / 丢弃条目）
    pub fn is_lossy(&self) -> bool {
        self.kind != FixKind::Clamped
    }
}

impl std::fmt::Display for Fix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let what = match self.kind {
            FixKind::Clamped => "clamped to the allowed range",
            FixKind::Reset => "invalid value, reset to default",
            FixKind::Dropped => "invalid entry dropped",
        };
        write!(f, "{}: {what}", self.path)
    }
}

/// 剪贴板条数上限（不含置顶）：最低可以改到 10
const CLIPBOARD_MAX_ITEMS: (u32, u32) = (10, 100_000);
/// 单条文本上限
const CLIPBOARD_MAX_TEXT_BYTES: (u32, u32) = (1024, 16 * 1024 * 1024);
/// 单张图片上限
const CLIPBOARD_MAX_IMAGE_BYTES: (u32, u32) = (1024 * 1024, 100 * 1024 * 1024);
/// 三个超时（毫秒）
const TIMEOUT_MS: (u32, u32) = (1_000, 600_000);

/// 就地校验并夹紧；返回被修正的字段路径（用于 warn 日志）
pub fn validate(settings: &mut Settings) -> Vec<String> {
    check(settings).into_iter().map(|f| f.path).collect()
}

/// 就地校验并修正，返回全部修正（含种类）
pub fn check(s: &mut Settings) -> Vec<Fix> {
    let mut fixes = Vec::new();
    s.schema_version = SETTINGS_SCHEMA_VERSION;
    check_hotkeys(s, &mut fixes);
    check_providers(s, &mut fixes);
    check_translate(s, &mut fixes);
    check_ocr(s, &mut fixes);
    check_clipboard(s, &mut fixes);
    check_appearance(s, &mut fixes);
    if let Some(v) = s.advanced.unload_panel_after_idle_min.as_mut() {
        clamp_u32(
            v,
            ranges::UNLOAD_PANEL_AFTER_IDLE_MIN,
            "advanced.unload_panel_after_idle_min",
            &mut fixes,
        );
    }
    fixes
}

fn check_hotkeys(s: &mut Settings, fixes: &mut Vec<Fix>) {
    let h = &mut s.hotkeys;
    clamp_u32(&mut h.copy_wait_ms, ranges::COPY_WAIT_MS, "hotkeys.copy_wait_ms", fixes);
    let interval = h.double_tap.interval_ms;
    if !defaults::DOUBLE_TAP_INTERVALS_MS.contains(&interval) {
        h.double_tap.interval_ms = nearest(interval, &defaults::DOUBLE_TAP_INTERVALS_MS);
        fixes.push(Fix::new("hotkeys.double_tap.interval_ms", FixKind::Clamped));
    }
    for (action, binding) in &mut h.bindings {
        if !binding_is_valid(binding) {
            *binding = Binding::default_binding();
            fixes.push(Fix::new(
                format!("hotkeys.bindings.{}", action.as_str()),
                FixKind::Reset,
            ));
        }
    }
    let before = h.app_keymaps.len();
    h.app_keymaps.retain(|k| !k.app_id.trim().is_empty());
    if h.app_keymaps.len() != before {
        fixes.push(Fix::new("hotkeys.app_keymaps", FixKind::Dropped));
    }
}

/// `default`、`none`，或能被 global-hotkey 解析的组合（例如 `Alt+KeyD`、`Control+Alt+Shift+KeyS`）
pub fn binding_is_valid(b: &Binding) -> bool {
    b.is_default() || b.is_none() || shortcut_parses(b.as_str())
}

/// 组合键串能否被 global-hotkey 解析
pub fn shortcut_parses(combo: &str) -> bool {
    !combo.trim().is_empty() && combo.parse::<tauri_plugin_global_shortcut::Shortcut>().is_ok()
}

fn check_providers(s: &mut Settings, fixes: &mut Vec<Fix>) {
    let mut seen = HashSet::new();
    let mut idx = 0usize;
    s.providers.retain(|p| {
        let i = idx;
        idx += 1;
        let ok = !p.id.trim().is_empty() && p.id.trim() == p.id && !p.id.chars().any(char::is_control);
        if !ok || !seen.insert(p.id.clone()) {
            fixes.push(Fix::new(format!("providers[{i}]"), FixKind::Dropped));
            return false;
        }
        true
    });
    for (i, p) in s.providers.iter_mut().enumerate() {
        let before = p.headers.len();
        p.headers
            .retain(|name, value| header_name_is_valid(name) && header_value_is_valid(value));
        if p.headers.len() != before {
            fixes.push(Fix::new(format!("providers[{i}].headers"), FixKind::Dropped));
        }
        let mut model_ids = HashSet::new();
        let before = p.models.len();
        p.models
            .retain(|m| !m.id.trim().is_empty() && model_ids.insert(m.id.clone()));
        if p.models.len() != before {
            fixes.push(Fix::new(format!("providers[{i}].models"), FixKind::Dropped));
        }
        let t = &mut p.timeouts;
        clamp_u32(
            &mut t.connect_ms,
            TIMEOUT_MS,
            &format!("providers[{i}].timeouts.connect_ms"),
            fixes,
        );
        clamp_u32(
            &mut t.first_token_ms,
            TIMEOUT_MS,
            &format!("providers[{i}].timeouts.first_token_ms"),
            fixes,
        );
        clamp_u32(
            &mut t.idle_ms,
            TIMEOUT_MS,
            &format!("providers[{i}].timeouts.idle_ms"),
            fixes,
        );
    }
}

/// HTTP 头名必须是 RFC 9110 的 token（tchar 组成、非空）
pub fn header_name_is_valid(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// HTTP 头的值里不能有 CR / LF / NUL 等控制字符（允许水平制表符）
pub fn header_value_is_valid(value: &str) -> bool {
    !value.chars().any(|c| c.is_control() && c != '\t')
}

fn check_translate(s: &mut Settings, fixes: &mut Vec<Fix>) {
    let t = &mut s.translate;
    clamp_u32(
        &mut t.long_text_threshold,
        ranges::LONG_TEXT_THRESHOLD,
        "translate.long_text_threshold",
        fixes,
    );
    clamp_u32(
        &mut t.mouse_leave_px,
        ranges::MOUSE_LEAVE_PX,
        "translate.mouse_leave_px",
        fixes,
    );
    if t.default_style.trim().is_empty() {
        defaults::DEFAULT_STYLE_ID.clone_into(&mut t.default_style);
        fixes.push(Fix::new("translate.default_style", FixKind::Reset));
    }
}

fn check_ocr(s: &mut Settings, fixes: &mut Vec<Fix>) {
    let o = &mut s.ocr;
    if o.default_mode == OcrMode::Translate {
        // 默认模式只允许 lines / paragraphs / markdown；直接翻译只能由截图直接翻译触发
        o.default_mode = OcrMode::Lines;
        fixes.push(Fix::new("ocr.default_mode", FixKind::Reset));
    }
    clamp_u32(&mut o.max_edge_px, ranges::OCR_MAX_EDGE_PX, "ocr.max_edge_px", fixes);
    clamp_u32(
        &mut o.history_limit,
        ranges::OCR_HISTORY_LIMIT,
        "ocr.history_limit",
        fixes,
    );
}

fn check_clipboard(s: &mut Settings, fixes: &mut Vec<Fix>) {
    let c = &mut s.clipboard;
    clamp_u32(&mut c.max_items, CLIPBOARD_MAX_ITEMS, "clipboard.max_items", fixes);
    clamp_u32(
        &mut c.max_text_bytes,
        CLIPBOARD_MAX_TEXT_BYTES,
        "clipboard.max_text_bytes",
        fixes,
    );
    clamp_u32(
        &mut c.max_image_bytes,
        CLIPBOARD_MAX_IMAGE_BYTES,
        "clipboard.max_image_bytes",
        fixes,
    );
    if let Some(days) = c.max_age_days
        && !defaults::CLIPBOARD_MAX_AGE_DAYS.contains(&days)
    {
        c.max_age_days = Some(nearest(days, &defaults::CLIPBOARD_MAX_AGE_DAYS));
        fixes.push(Fix::new("clipboard.max_age_days", FixKind::Clamped));
    }
    let before = c.excluded_apps.len();
    let mut seen = HashSet::new();
    c.excluded_apps
        .retain(|a| !a.app_id.trim().is_empty() && seen.insert((a.os, a.app_id.clone())));
    if c.excluded_apps.len() != before {
        fixes.push(Fix::new("clipboard.excluded_apps", FixKind::Dropped));
    }
}

fn check_appearance(s: &mut Settings, fixes: &mut Vec<Fix>) {
    let a = &mut s.appearance;
    if !accent_is_valid(&a.accent) {
        defaults::ACCENT_SYSTEM.clone_into(&mut a.accent);
        fixes.push(Fix::new("appearance.accent", FixKind::Reset));
    }
    clamp_u32(&mut a.font_size, ranges::FONT_SIZE, "appearance.font_size", fixes);
    clamp_u32(
        &mut a.translate_min_width,
        ranges::TRANSLATE_MIN_WIDTH,
        "appearance.translate_min_width",
        fixes,
    );
    clamp_u32(
        &mut a.translate_max_width,
        ranges::TRANSLATE_MAX_WIDTH,
        "appearance.translate_max_width",
        fixes,
    );
    if a.translate_max_width < a.translate_min_width {
        a.translate_max_width = a.translate_min_width;
        fixes.push(Fix::new("appearance.translate_max_width", FixKind::Clamped));
    }
    clamp_u32(
        &mut a.clipboard_width,
        ranges::CLIPBOARD_WIDTH,
        "appearance.clipboard_width",
        fixes,
    );
    let (lo, hi) = ranges::MAX_HEIGHT_RATIO;
    if !a.max_height_ratio.is_finite() {
        a.max_height_ratio = wisp_types::Appearance::default().max_height_ratio;
        fixes.push(Fix::new("appearance.max_height_ratio", FixKind::Reset));
    } else if !(lo..=hi).contains(&a.max_height_ratio) {
        a.max_height_ratio = a.max_height_ratio.clamp(lo, hi);
        fixes.push(Fix::new("appearance.max_height_ratio", FixKind::Clamped));
    }
}

/// 强调色：`system`、预设名（blue/purple/…）或 `#RRGGBB`
pub fn accent_is_valid(accent: &str) -> bool {
    accent == defaults::ACCENT_SYSTEM || defaults::ACCENT_PRESETS.contains(&accent) || is_hex_color(accent)
}

/// `#RRGGBB`（大小写不限）
pub fn is_hex_color(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

fn clamp_u32(v: &mut u32, (lo, hi): (u32, u32), path: &str, fixes: &mut Vec<Fix>) {
    let clamped = (*v).clamp(lo, hi);
    if clamped != *v {
        *v = clamped;
        fixes.push(Fix::new(path, FixKind::Clamped));
    }
}

/// 允许值里离 `v` 最近的一个（距离相同时取较小者）
fn nearest(v: u32, allowed: &[u32]) -> u32 {
    allowed.iter().copied().min_by_key(|a| (a.abs_diff(v), *a)).unwrap_or(v)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use wisp_types::{ExcludedApp, HotkeyAction, ModelEntry, OsKind, Protocol, Provider, Quirks, Timeouts};

    use super::*;

    fn provider(id: &str) -> Provider {
        Provider {
            id: id.to_owned(),
            name: String::new(),
            protocol: Protocol::OpenAiChat,
            base_url: "https://api.example.com/v1".to_owned(),
            headers: BTreeMap::new(),
            quirks: Quirks::default(),
            models: Vec::new(),
            timeouts: Timeouts::default(),
            template_id: None,
        }
    }

    #[test]
    fn defaults_are_valid() {
        let mut s = Settings::default();
        assert!(check(&mut s).is_empty());
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn out_of_range_numbers_are_clamped() {
        let mut s = Settings::default();
        s.appearance.font_size = 40;
        s.ocr.max_edge_px = 100;
        s.hotkeys.copy_wait_ms = 10_000;
        s.translate.long_text_threshold = 1;
        s.appearance.max_height_ratio = 0.95;
        // 下限 5（建议值 10 分钟合法）
        s.advanced.unload_panel_after_idle_min = Some(2);
        let fixes = check(&mut s);
        assert_eq!(s.appearance.font_size, 18);
        assert_eq!(s.ocr.max_edge_px, 512);
        assert_eq!(s.hotkeys.copy_wait_ms, 500);
        assert_eq!(s.translate.long_text_threshold, 500);
        assert!((s.appearance.max_height_ratio - 0.8).abs() < f32::EPSILON);
        assert_eq!(s.advanced.unload_panel_after_idle_min, Some(5));
        assert!(fixes.iter().all(|f| f.kind == FixKind::Clamped), "{fixes:?}");
        assert!(fixes.iter().any(|f| f.path == "appearance.font_size"));
    }

    #[test]
    fn enumerated_values_snap_to_the_nearest_allowed() {
        let mut s = Settings::default();
        s.hotkeys.double_tap.interval_ms = 340;
        s.clipboard.max_age_days = Some(45);
        check(&mut s);
        assert_eq!(s.hotkeys.double_tap.interval_ms, 300);
        assert_eq!(s.clipboard.max_age_days, Some(30));
    }

    #[test]
    fn invalid_bindings_reset_to_default() {
        let mut s = Settings::default();
        s.hotkeys
            .bindings
            .insert(HotkeyAction::Ocr, Binding("Alt+NotAKey".to_owned()));
        s.hotkeys
            .bindings
            .insert(HotkeyAction::Clipboard, Binding("Control+Alt+Shift+KeyV".to_owned()));
        s.hotkeys.bindings.insert(HotkeyAction::TranslateInput, Binding::none());
        let fixes = check(&mut s);
        assert_eq!(s.hotkeys.binding(HotkeyAction::Ocr), Binding::default_binding());
        assert_eq!(
            s.hotkeys.binding(HotkeyAction::Clipboard).as_str(),
            "Control+Alt+Shift+KeyV"
        );
        assert!(s.hotkeys.binding(HotkeyAction::TranslateInput).is_none());
        assert_eq!(fixes, vec![Fix::new("hotkeys.bindings.ocr", FixKind::Reset)]);
        assert!(fixes[0].is_lossy());
    }

    #[test]
    fn invalid_headers_and_duplicate_providers_are_dropped() {
        let mut s = Settings::default();
        let mut p = provider("openai-main");
        p.headers.insert("X-Ok".to_owned(), "v".to_owned());
        p.headers.insert("Bad Name".to_owned(), "v".to_owned());
        p.headers.insert("X-Inject".to_owned(), "a\r\nEvil: 1".to_owned());
        p.models = vec![ModelEntry::new("gpt"), ModelEntry::new("gpt"), ModelEntry::new(" ")];
        s.providers = vec![p, provider("openai-main"), provider("")];
        let fixes = check(&mut s);
        assert_eq!(s.providers.len(), 1);
        let p = &s.providers[0];
        assert_eq!(p.headers.keys().collect::<Vec<_>>(), vec!["X-Ok"]);
        assert_eq!(p.models.len(), 1);
        let paths: Vec<&str> = fixes.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"providers[1]"));
        assert!(paths.contains(&"providers[2]"));
        assert!(paths.contains(&"providers[0].headers"));
        assert!(paths.contains(&"providers[0].models"));
        // 日志文本里没有值本身
        assert!(fixes.iter().all(|f| !f.to_string().contains("Evil")));
    }

    #[test]
    fn accent_and_ocr_mode_are_reset_when_invalid() {
        let mut s = Settings::default();
        s.appearance.accent = "#12345".to_owned();
        s.ocr.default_mode = OcrMode::Translate;
        let fixes = check(&mut s);
        assert_eq!(s.appearance.accent, "system");
        assert_eq!(s.ocr.default_mode, OcrMode::Lines);
        assert!(fixes.iter().all(Fix::is_lossy));
        for ok in ["system", "graphite", "#0a84ff", "#0A84FF"] {
            assert!(accent_is_valid(ok), "{ok}");
        }
        for bad in ["", "Blue", "0A84FF", "#0A84FG", "#0A84FF0"] {
            assert!(!accent_is_valid(bad), "{bad}");
        }
    }

    #[test]
    fn max_width_never_below_min_width() {
        let mut s = Settings::default();
        s.appearance.translate_min_width = 480;
        s.appearance.translate_max_width = 440;
        check(&mut s);
        assert_eq!(s.appearance.translate_max_width, 480);
    }

    #[test]
    fn excluded_apps_drop_empty_and_duplicate_entries() {
        let mut s = Settings::default();
        let dup = s.clipboard.excluded_apps[0].clone();
        s.clipboard.excluded_apps.push(dup);
        s.clipboard.excluded_apps.push(ExcludedApp {
            os: OsKind::Linux,
            app_id: "  ".to_owned(),
            name: String::new(),
        });
        let fixes = check(&mut s);
        assert_eq!(s.clipboard.excluded_apps, Settings::default().clipboard.excluded_apps);
        assert_eq!(fixes, vec![Fix::new("clipboard.excluded_apps", FixKind::Dropped)]);
    }

    #[test]
    fn header_token_rules() {
        for ok in ["X-Api-Key", "anthropic-version", "x_custom", "A1!#$%&'*+-.^_`|~"] {
            assert!(header_name_is_valid(ok), "{ok}");
        }
        for bad in ["", "Bad Name", "a:b", "x\n", "名字"] {
            assert!(!header_name_is_valid(bad), "{bad}");
        }
        assert!(header_value_is_valid("Bearer abc\tdef"));
        assert!(!header_value_is_valid("a\rb"));
        assert!(!header_value_is_valid("a\0b"));
    }
}
