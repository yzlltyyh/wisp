//! 配置文档宽松反序列化的行为测试：单个字段 / 条目非法只影响它自己，问题列表不含值本身。

use wisp_types::{
    ClipPanelPosition, ConfigIssue, GlossaryFile, HotkeyAction, OsKind, Settings, StylesFile, ThemePref, UpdatePolicy,
    parse_lenient,
};

/// 问题路径（排序后比较：记录顺序按结构体字段的声明顺序，不是 JSON 里的顺序）
fn paths(issues: &[ConfigIssue]) -> Vec<&str> {
    let mut v: Vec<&str> = issues.iter().map(|i| i.path.as_str()).collect();
    v.sort_unstable();
    v
}

#[test]
fn empty_object_is_default_without_issues() {
    let (s, issues) = parse_lenient::<Settings>("{}").expect("parse");
    assert_eq!(s, Settings::default());
    assert!(issues.is_empty(), "{issues:?}");
}

#[test]
fn default_roundtrips_without_issues() {
    let text = serde_json::to_string_pretty(&Settings::default()).expect("serialize");
    let (s, issues) = parse_lenient::<Settings>(&text).expect("parse");
    assert_eq!(s, Settings::default());
    assert!(issues.is_empty(), "{issues:?}");
}

#[test]
fn bad_enum_resets_only_that_field() {
    let (s, issues) = parse_lenient::<Settings>(
        r#"{
            "appearance": { "theme": "Dark", "font_size": 16 },
            "general": { "update_policy": "auto_install", "ui_language": "fr" },
            "clipboard": { "panel_position": "remember", "max_items": -5 }
        }"#,
    )
    .expect("parse");
    assert_eq!(s.appearance.theme, ThemePref::System, "非法值退回默认值");
    assert_eq!(s.appearance.font_size, 16, "同一对象的其他字段照常加载");
    assert_eq!(s.general.update_policy, UpdatePolicy::AutoInstall);
    assert_eq!(s.general.ui_language, Settings::default().general.ui_language);
    assert_eq!(s.clipboard.panel_position, ClipPanelPosition::Remember);
    assert_eq!(s.clipboard.max_items, 1000, "越界整数退回结构默认值（不是 u32 的 0）");
    assert_eq!(
        paths(&issues),
        ["appearance.theme", "clipboard.max_items", "general.ui_language"]
    );
}

#[test]
fn bad_scalar_falls_back_to_struct_default_not_type_default() {
    let (s, issues) =
        parse_lenient::<Settings>(r#"{ "hotkeys": { "copy_wait_ms": "fast" }, "ocr": { "auto_copy": null } }"#)
            .expect("parse");
    assert_eq!(s.hotkeys.copy_wait_ms, 150);
    assert!(s.ocr.auto_copy, "null 写给 bool 字段 → 默认 true");
    assert_eq!(paths(&issues), ["hotkeys.copy_wait_ms", "ocr.auto_copy"]);
}

#[test]
fn non_object_section_resets_that_section() {
    let (s, issues) =
        parse_lenient::<Settings>(r#"{ "general": 5, "advanced": { "log_level": "debug" } }"#).expect("parse");
    assert_eq!(s.general, Settings::default().general);
    assert_eq!(s.advanced.log_level, wisp_types::LogLevel::Debug);
    assert_eq!(paths(&issues), ["general"]);
}

#[test]
fn unknown_binding_keys_are_dropped_individually() {
    let (s, issues) = parse_lenient::<Settings>(
        r#"{ "hotkeys": { "bindings": {
            "translate_selection": "CmdOrCtrl+Shift+T",
            "translate_selction": "Alt+T",
            "future_action": "Alt+F",
            "ocr": 42
        } } }"#,
    )
    .expect("parse");
    assert_eq!(s.hotkeys.bindings.len(), 1);
    assert_eq!(
        s.hotkeys.binding(HotkeyAction::TranslateSelection).0,
        "CmdOrCtrl+Shift+T"
    );
    assert_eq!(
        paths(&issues),
        [
            "hotkeys.bindings.future_action",
            "hotkeys.bindings.ocr",
            "hotkeys.bindings.translate_selction"
        ]
    );
}

#[test]
fn array_entries_are_dropped_individually() {
    let (s, issues) = parse_lenient::<Settings>(
        r#"{
            "providers": [
                { "id": "ok", "protocol": "openai_chat", "base_url": "https://api.example.com/v1",
                  "models": [ { "id": "m1" }, { "label": "missing id" }, { "id": "m3", "vision": "maybe" } ] },
                { "id": "broken", "base_url": "https://x" },
                "not an object"
            ],
            "clipboard": { "excluded_apps": [ { "os": "macos", "app_id": "com.example" }, { "os": "beos", "app_id": "x" } ] },
            "hotkeys": { "app_keymaps": [ { "os": "windows", "app_id": "a.exe" }, { "app_id": "no-os" } ] }
        }"#,
    )
    .expect("parse");
    assert_eq!(s.providers.len(), 1);
    let ids: Vec<&str> = s.providers[0].models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["m1"], "非法模型条目只丢弃自己，Provider 保留");
    assert_eq!(s.clipboard.excluded_apps.len(), 1);
    assert_eq!(s.clipboard.excluded_apps[0].os, OsKind::Macos);
    assert_eq!(s.hotkeys.app_keymaps.len(), 1);
    assert_eq!(
        paths(&issues),
        [
            "clipboard.excluded_apps[1]",
            "hotkeys.app_keymaps[1]",
            "providers[0].models[1]",
            "providers[0].models[2]",
            "providers[1]",
            "providers[2]",
        ]
    );
}

#[test]
fn issues_never_contain_values() {
    let (_, issues) = parse_lenient::<Settings>(
        r#"{
            "appearance": { "accent": 1234567 },
            "providers": [ { "id": "p", "protocol": "openai_chat", "base_url": 7,
                             "headers": { "Authorization": "Bearer sk-SECRET-123" } } ]
        }"#,
    )
    .expect("parse");
    assert_eq!(issues.len(), 2, "{issues:?}");
    for issue in &issues {
        let text = issue.to_string();
        assert!(!text.contains("SECRET"), "{text}");
        assert!(!text.contains("1234567"), "{text}");
    }
    let accent = issues
        .iter()
        .find(|i| i.path == "appearance.accent")
        .expect("accent issue");
    assert_eq!(accent.reason, "expected String, found number (reset to default)");
    let provider = issues
        .iter()
        .find(|i| i.path == "providers[0]")
        .expect("provider issue");
    assert_eq!(provider.reason, "expected Provider, found object (entry dropped)");
}

#[test]
fn plain_serde_is_equally_lenient() {
    let s: Settings = serde_json::from_str(r#"{ "appearance": { "theme": "Dark", "font_size": 15 } }"#).expect("parse");
    assert_eq!(s.appearance.theme, ThemePref::System);
    assert_eq!(s.appearance.font_size, 15);
    assert!(serde_json::from_str::<Settings>("[]").is_err(), "顶层不是对象仍然失败");
}

#[test]
fn styles_and_glossary_files_are_lenient() {
    let (styles, issues) = parse_lenient::<StylesFile>(
        r#"{
            "order": ["natural", 3, "learn"],
            "overrides": { "natural": { "temperature": null }, "learn": { "output": "html" } },
            "custom": [ { "id": "custom-1", "name": "Mine" }, { "name": "no id" } ]
        }"#,
    )
    .expect("parse");
    assert_eq!(styles.schema_version, 1);
    assert_eq!(styles.order, ["natural", "learn"]);
    assert_eq!(styles.overrides.len(), 1);
    assert_eq!(styles.overrides["natural"].temperature, Some(None));
    assert_eq!(styles.custom.len(), 1);
    assert_eq!(paths(&issues), ["custom[1]", "order[1]", "overrides.learn"]);

    let (glossary, issues) =
        parse_lenient::<GlossaryFile>(r#"{ "entries": [ { "source": "a", "target": "b" }, { "source": "x" } ] }"#)
            .expect("parse");
    assert_eq!(glossary.entries.len(), 1);
    assert_eq!(paths(&issues), ["entries[1]"]);
}

#[test]
fn sync_section_is_lenient_and_issues_hold_no_values() {
    let (s, issues) = parse_lenient::<Settings>(
        r#"{ "sync": { "enabled": true, "server_url": 42, "username": "someone", "auto": "yes",
            "content": { "images": false, "clipboard": "off" } } }"#,
    )
    .expect("parse");
    assert!(s.sync.enabled);
    assert_eq!(s.sync.server_url, "", "类型不对 → 默认空串");
    assert_eq!(s.sync.username, "someone");
    assert!(s.sync.auto, "非法值退回默认 true");
    assert!(!s.sync.content.images);
    assert!(s.sync.content.clipboard);
    assert_eq!(
        paths(&issues),
        ["sync.auto", "sync.content.clipboard", "sync.server_url"]
    );
    let text = format!("{issues:?}");
    assert!(!text.contains("someone"), "{text}");
    // Debug 不输出地址与账号
    assert!(!format!("{:?}", s.sync).contains("someone"));
}
