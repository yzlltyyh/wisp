//! 配置投影的行为测试：往返、按系统隔离、只属于本机的字段、学习缓存注回、集合元素的删除与重建、顺序、转义、差异。

use std::collections::BTreeMap;

use proptest::prelude::*;
use serde_json::{Value, json};
use wisp_sync::KeyApply;
use wisp_sync::projection::{
    self, ConfigDoc, canonical, diff, escape_segment, is_default, is_element_key, project, unproject,
    unproject_detailed,
};
use wisp_types::{
    AppKeymap, Binding, DoubleTapSettings, ExcludedApp, GlossaryEntry, GlossaryFile, HotkeyAction, Lang, LogLevel,
    MaterialPref, ModelCaps, ModelEntry, ModelRef, ModifierKey, OsKind, Protocol, Provider, ReasoningEffort, Settings,
    Style, StyleOverride, StylesFile, Timeouts, UiLanguage, Vision, VisionSource, parse_lenient,
};

const ALL_OS: [OsKind; 3] = [OsKind::Macos, OsKind::Windows, OsKind::Linux];

type Changes = Vec<(String, Option<Value>)>;
type Projection = BTreeMap<String, Value>;

fn val<T: serde::Serialize>(x: &T) -> Value {
    canonical(x).expect("canonical")
}

fn set(key: &str, value: Value) -> (String, Option<Value>) {
    (key.to_owned(), Some(value))
}

fn del(key: &str) -> (String, Option<Value>) {
    (key.to_owned(), None)
}

/// 投影里的每个 key 都作为一次设置
fn all_set(map: &BTreeMap<String, Value>) -> Changes {
    map.iter().map(|(k, v)| (k.clone(), Some(v.clone()))).collect()
}

fn all_applied(results: &[KeyApply]) -> bool {
    results.iter().all(|r| *r == KeyApply::Applied)
}

/// `unproject(project(x), base = x) == x`，且每一项都报告已应用
fn assert_round_trip(doc: ConfigDoc, x: &Value, os: OsKind) {
    let p = project(doc, x, os);
    let out = unproject_detailed(doc, x, &all_set(&p), os);
    assert_eq!(&out.value, x, "{doc:?} on {os:?}");
    assert!(all_applied(&out.results), "{doc:?} on {os:?}: {:?}", out.results);
}

/// 反投影的结果能被宽容解析无问题地读回
fn assert_parses<T: serde::de::DeserializeOwned>(v: &Value) -> T {
    let (parsed, issues) = parse_lenient::<T>(&v.to_string()).expect("json");
    assert!(issues.is_empty(), "{issues:?}");
    parsed
}

// ---------------------------------------------------------------------------------------------
// 样例
// ---------------------------------------------------------------------------------------------

fn caps(temperature: bool) -> ModelCaps {
    ModelCaps {
        temperature: Some(temperature),
        effort_param: Some(!temperature),
        anthropic_efforts: Some(vec!["low".to_owned()]),
        ..ModelCaps::default()
    }
}

fn model(id: &str, vision: Vision, source: Option<VisionSource>) -> ModelEntry {
    ModelEntry {
        vision,
        vision_source: source,
        ..ModelEntry::new(id)
    }
}

fn provider(id: &str, models: Vec<ModelEntry>) -> Provider {
    Provider {
        id: id.to_owned(),
        name: format!("{id} name"),
        protocol: Protocol::OpenAiChat,
        base_url: "https://llm.example.invalid/v1".to_owned(),
        headers: BTreeMap::new(),
        quirks: Default::default(),
        models,
        timeouts: Timeouts::default(),
        template_id: None,
    }
}

fn openai() -> Provider {
    let mut p = provider(
        "openai",
        vec![
            ModelEntry {
                caps: caps(false),
                ..model("gpt-4o", Vision::Yes, Some(VisionSource::Capability))
            },
            ModelEntry {
                deprecated: true,
                label: Some("Mini".to_owned()),
                ..model("gpt-4o-mini", Vision::Yes, Some(VisionSource::User))
            },
            ModelEntry {
                reasoning: true,
                reasoning_effort: Some(ReasoningEffort::Minimal),
                caps: caps(true),
                ..model("o3", Vision::No, Some(VisionSource::Runtime))
            },
        ],
    );
    p.headers.insert("X-Org".to_owned(), "org-sample".to_owned());
    p.protocol = Protocol::OpenAiResponses;
    p
}

fn rich_settings(os: OsKind) -> Settings {
    let mut s = Settings::for_os(os);
    s.general.launch_at_login = false;
    s.general.developer_mode = true;
    s.general.ui_language = UiLanguage::EnUs;
    s.hotkeys
        .bindings
        .insert(HotkeyAction::Ocr, Binding("Alt+Shift+KeyO".to_owned()));
    s.hotkeys.bindings.insert(HotkeyAction::Clipboard, Binding::none());
    s.hotkeys.double_tap = DoubleTapSettings {
        enabled: true,
        key: Some(ModifierKey::LeftCmd),
        interval_ms: 400,
    };
    s.hotkeys.copy_wait_ms = 220;
    s.hotkeys.mouse.enabled = true;
    s.hotkeys.app_keymaps = vec![
        AppKeymap {
            os: OsKind::Macos,
            app_id: "com.googlecode.iterm2".to_owned(),
            copy: Some("Super+KeyC".to_owned()),
            paste: None,
        },
        AppKeymap {
            os: OsKind::Windows,
            app_id: "windowsterminal.exe".to_owned(),
            copy: Some("Control+Shift+KeyC".to_owned()),
            paste: Some("Control+Shift+KeyV".to_owned()),
        },
        AppKeymap {
            os: OsKind::Linux,
            app_id: "org/gnome~terminal@x".to_owned(),
            copy: Some("none".to_owned()),
            paste: None,
        },
    ];
    let mut workers = provider(
        "@cf/workers",
        vec![ModelEntry {
            context_tokens: Some(8192),
            missing_upstream: true,
            ..model("@cf/meta/llama-3-8b-instruct", Vision::Unknown, None)
        }],
    );
    workers.protocol = Protocol::Gemini;
    let mut local = provider("local~1", Vec::new());
    local.template_id = Some("ollama".to_owned());
    local.timeouts.connect_ms = 2000;
    s.providers = vec![openai(), workers, local];
    s.defaults.translate = Some(ModelRef {
        provider_id: "openai".to_owned(),
        model_id: "gpt-4o".to_owned(),
    });
    s.translate.primary_lang = Lang::Ja;
    s.translate.default_style = "custom-1a2b3c4d".to_owned();
    s.ocr.history = true;
    s.ocr.history_limit = 120;
    s.clipboard.enabled = false;
    s.clipboard.secret_detection = false;
    s.clipboard.max_items = 5000;
    s.clipboard.max_age_days = Some(30);
    s.clipboard.excluded_apps.retain(|a| a.app_id != "keepass.exe");
    s.clipboard.excluded_apps.push(ExcludedApp {
        os: OsKind::Linux,
        app_id: "org.gnome/Seahorse".to_owned(),
        name: "Seahorse".to_owned(),
    });
    s.appearance.max_height_ratio = 0.45;
    s.appearance.material = MaterialPref::Solid;
    s.appearance.accent = "#3366ff".to_owned();
    s.advanced.log_level = LogLevel::Debug;
    s.advanced.unload_panel_after_idle_min = Some(10);
    s.sync.enabled = true;
    s.sync.server_url = "https://dav.example.invalid/dav".to_owned();
    s.sync.username = "sample-user".to_owned();
    s.sync.content.images = false;
    s
}

fn custom_style(id: &str, hotkey: Option<&str>) -> Style {
    Style {
        system_template: format!("system for {id}"),
        tail_instruction: "tail".to_owned(),
        temperature: Some(0.25),
        hotkey: hotkey.map(str::to_owned),
        ..Style::custom(id, format!("{id} name"))
    }
}

fn rich_styles() -> StylesFile {
    let mut overrides = BTreeMap::new();
    overrides.insert(
        "natural".to_owned(),
        StyleOverride {
            temperature: Some(Some(0.7)),
            hotkey: Some(Some("Alt+Shift+KeyN".to_owned())),
            ..StyleOverride::default()
        },
    );
    // 只有快捷键的覆盖项
    overrides.insert(
        "technical".to_owned(),
        StyleOverride {
            hotkey: Some(Some("Alt+Shift+KeyT".to_owned())),
            ..StyleOverride::default()
        },
    );
    overrides.insert(
        "casual".to_owned(),
        StyleOverride {
            temperature: Some(None),
            tail_instruction: Some("be casual".to_owned()),
            model_ref: Some(Some(ModelRef {
                provider_id: "openai".to_owned(),
                model_id: "o3".to_owned(),
            })),
            ..StyleOverride::default()
        },
    );
    StylesFile {
        schema_version: 1,
        order: [
            "technical",
            "custom-1a2b3c4d",
            "natural",
            "custom-a/b~c",
            "academic",
            "@custom-x",
        ]
        .map(str::to_owned)
        .to_vec(),
        overrides,
        custom: vec![
            custom_style("custom-1a2b3c4d", Some("Control+Alt+KeyL")),
            custom_style("custom-a/b~c", None),
            custom_style("@custom-x", Some("Control+Alt+KeyX")),
        ],
    }
}

fn entry(source: &str, target: &str, note: Option<&str>) -> GlossaryEntry {
    GlossaryEntry {
        source: source.to_owned(),
        target: target.to_owned(),
        note: note.map(str::to_owned),
    }
}

fn rich_glossary() -> GlossaryFile {
    GlossaryFile {
        schema_version: 1,
        entries: vec![
            entry("Wisp", "微光", Some("产品名")),
            entry("I/O", "输入输出", None),
            entry("@mention", "提及", None),
            entry("~tilde", "波浪号", Some("~")),
        ],
    }
}

fn provider_ids(doc: &Value) -> Vec<String> {
    doc["providers"]
        .as_array()
        .expect("providers")
        .iter()
        .map(|p| p["id"].as_str().expect("id").to_owned())
        .collect()
}

fn model_of<'a>(doc: &'a Value, provider: &str, model: &str) -> &'a Value {
    doc["providers"]
        .as_array()
        .and_then(|ps| ps.iter().find(|p| p["id"] == provider))
        .and_then(|p| p["models"].as_array())
        .and_then(|ms| ms.iter().find(|m| m["id"] == model))
        .expect("model")
}

// ---------------------------------------------------------------------------------------------
// 往返
// ---------------------------------------------------------------------------------------------

#[test]
fn default_documents_round_trip_on_every_os() {
    for os in ALL_OS {
        assert_round_trip(ConfigDoc::Settings, &val(&Settings::for_os(os)), os);
        assert_round_trip(ConfigDoc::Styles, &val(&StylesFile::default()), os);
        assert_round_trip(ConfigDoc::Glossary, &val(&GlossaryFile::default()), os);
    }
}

#[test]
fn rich_documents_round_trip_on_every_os() {
    for os in ALL_OS {
        let s = val(&rich_settings(os));
        assert_round_trip(ConfigDoc::Settings, &s, os);
        assert_round_trip(ConfigDoc::Styles, &val(&rich_styles()), os);
        assert_round_trip(ConfigDoc::Glossary, &val(&rich_glossary()), os);

        // 读回强类型后与原值相同（包括 f32 与学习缓存）
        let changes = all_set(&project(ConfigDoc::Settings, &s, os));
        let back: Settings = assert_parses(&unproject(ConfigDoc::Settings, &s, &changes, os));
        assert_eq!(back, rich_settings(os));
        let styles = val(&rich_styles());
        let changes = all_set(&project(ConfigDoc::Styles, &styles, os));
        let back: StylesFile = assert_parses(&unproject(ConfigDoc::Styles, &styles, &changes, os));
        assert_eq!(back, rich_styles());
    }
}

#[test]
fn default_settings_projection_shape() {
    let p = project(
        ConfigDoc::Settings,
        &val(&Settings::for_os(OsKind::Macos)),
        OsKind::Macos,
    );
    assert_eq!(p["/translate/primary_lang"], json!("zh-CN"));
    assert_eq!(p["/defaults/translate"], Value::Null);
    assert_eq!(p["/hotkeys/mouse"]["middle"]["action"], json!("translate_selection"));
    assert_eq!(p["/appearance/max_height_ratio"], json!(0.6));
    assert_eq!(p["/providers/@order"], json!([]));
    assert_eq!(p["@macos/hotkeys/bindings"], json!({}));
    assert_eq!(p["@macos/hotkeys/copy_wait_ms"], json!(150));
    assert_eq!(p["@macos/appearance/material"], json!("system"));
    assert_eq!(p["@macos/hotkeys/double_tap"]["interval_ms"], json!(300));
    assert_eq!(
        p["/clipboard/excluded_apps/windows:keepass.exe"],
        json!({"os": "windows", "app_id": "keepass.exe", "name": "KeePass"})
    );
    let excluded = p.keys().filter(|k| k.starts_with("/clipboard/excluded_apps/")).count();
    assert_eq!(excluded, 12);
    for key in p.keys() {
        assert!(
            key.starts_with('/') || key.starts_with("@macos/"),
            "只产出本系统的按系统 key：{key}"
        );
    }
    for absent in [
        "/hotkeys/bindings",
        "/hotkeys/copy_wait_ms",
        "/clipboard/excluded_apps",
        "/hotkeys/app_keymaps",
        "/providers",
    ] {
        assert!(!p.contains_key(absent), "{absent}");
    }
}

// ---------------------------------------------------------------------------------------------
// f32
// ---------------------------------------------------------------------------------------------

#[test]
fn f32_fields_project_without_widening() {
    let s = rich_settings(OsKind::Linux);
    let p = project(ConfigDoc::Settings, &val(&s), OsKind::Linux);
    assert_eq!(p["/appearance/max_height_ratio"], json!(0.45));
    // 同一份配置的两次规范化之间没有差异
    let again = project(ConfigDoc::Settings, &val(&s.clone()), OsKind::Linux);
    assert!(diff(&p, &again).is_empty());
    // 直接 to_value 会把 f32 拓宽，这正是投影输入必须经字符串往返的原因
    let widened = project(
        ConfigDoc::Settings,
        &serde_json::to_value(&s).expect("value"),
        OsKind::Linux,
    );
    assert_eq!(
        diff(&p, &widened).iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        ["/appearance/max_height_ratio"]
    );

    let styles = project(ConfigDoc::Styles, &val(&rich_styles()), OsKind::Linux);
    assert_eq!(styles["/overrides/natural"], json!({"temperature": 0.7}));
    assert_eq!(styles["/custom/custom-1a2b3c4d"]["temperature"], json!(0.25));
    assert_eq!(styles["/overrides/casual"]["temperature"], Value::Null);
}

// ---------------------------------------------------------------------------------------------
// 学习缓存
// ---------------------------------------------------------------------------------------------

#[test]
fn learned_model_cache_is_not_projected() {
    let p = project(ConfigDoc::Settings, &val(&rich_settings(OsKind::Macos)), OsKind::Macos);
    let models = p["/providers/openai"]["models"].as_array().expect("models").clone();
    let gpt4o = &models[0];
    for f in ["caps", "missing_upstream", "deprecated", "vision", "vision_source"] {
        assert!(gpt4o.get(f).is_none(), "{f}");
    }
    assert_eq!(gpt4o["enabled"], json!(true));
    // 用户指定的视觉能力照常同步
    let mini = &models[1];
    assert_eq!(mini["vision"], json!("yes"));
    assert_eq!(mini["vision_source"], json!("user"));
    assert!(mini.get("deprecated").is_none());
    assert_eq!(mini["label"], json!("Mini"));
    assert_eq!(models[2]["reasoning_effort"], json!("minimal"));
    let llama = &p["/providers/~2cf~1workers"]["models"][0];
    assert!(llama.get("missing_upstream").is_none());
    assert_eq!(llama["context_tokens"], json!(8192));
}

#[test]
fn learned_model_cache_is_reinjected_by_provider_and_model_id() {
    let base = val(&rich_settings(OsKind::Macos));
    // 另一台设备上的 openai：改了 gpt-4o 的名称、撤销了 mini 的用户视觉、删掉 o3、加了 gpt-5；值里夹带的缓存应被忽略
    let remote = json!({
        "id": "openai",
        "name": "OpenAI",
        "protocol": "openai_responses",
        "base_url": "https://llm.example.invalid/v1",
        "headers": {},
        "models": [
            {"id": "gpt-5", "label": null, "reasoning": true, "enabled": true, "context_tokens": null, "reasoning_effort": null},
            {"id": "gpt-4o", "label": "Omni", "reasoning": false, "enabled": false, "context_tokens": null,
             "reasoning_effort": null, "caps": {"temperature": true}, "deprecated": true},
            {"id": "gpt-4o-mini", "label": "Mini", "reasoning": false, "enabled": true, "context_tokens": null,
             "reasoning_effort": null},
        ],
        "timeouts": {"connect_ms": 10000, "first_token_ms": 30000, "idle_ms": 60000},
        "template_id": null
    });
    let out = unproject_detailed(
        ConfigDoc::Settings,
        &base,
        &[set("/providers/openai", remote)],
        OsKind::Macos,
    );
    assert!(all_applied(&out.results));
    let doc = &out.value;
    let old = &base;

    let gpt4o = model_of(doc, "openai", "gpt-4o");
    assert_eq!(gpt4o["label"], json!("Omni"));
    assert_eq!(gpt4o["enabled"], json!(false));
    assert_eq!(gpt4o["caps"], model_of(old, "openai", "gpt-4o")["caps"]);
    assert_eq!(gpt4o["deprecated"], json!(false));
    assert_eq!(gpt4o["vision"], json!("yes"));
    assert_eq!(gpt4o["vision_source"], json!("capability"));

    let mini = model_of(doc, "openai", "gpt-4o-mini");
    assert_eq!(mini["vision"], json!("unknown"));
    assert_eq!(mini["vision_source"], Value::Null);
    assert_eq!(mini["deprecated"], json!(true));

    let gpt5 = model_of(doc, "openai", "gpt-5");
    assert!(gpt5.get("caps").is_none() && gpt5.get("vision").is_none());

    let back: Settings = assert_parses(doc);
    let openai = back.provider("openai").expect("openai");
    assert_eq!(
        openai.models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["gpt-5", "gpt-4o", "gpt-4o-mini"]
    );
    assert_eq!(openai.models[0].caps, ModelCaps::default());
    assert_eq!(openai.models[0].vision, Vision::Unknown);

    // 远端用户指定的视觉能力胜过本机学习到的
    let mut remote = project(ConfigDoc::Settings, &base, OsKind::Macos)["/providers/openai"].clone();
    remote["models"][2]["vision"] = json!("yes");
    remote["models"][2]["vision_source"] = json!("user");
    let doc = unproject(
        ConfigDoc::Settings,
        &base,
        &[set("/providers/openai", remote)],
        OsKind::Macos,
    );
    let o3 = model_of(&doc, "openai", "o3");
    assert_eq!(o3["vision"], json!("yes"));
    assert_eq!(o3["vision_source"], json!("user"));
    assert_eq!(o3["caps"], model_of(&base, "openai", "o3")["caps"]);
}

// ---------------------------------------------------------------------------------------------
// 按系统与只属于本机
// ---------------------------------------------------------------------------------------------

#[test]
fn per_os_keys_only_apply_on_the_same_os() {
    let base = val(&rich_settings(OsKind::Macos));
    let foreign = vec![
        set("@windows/hotkeys/copy_wait_ms", json!(400)),
        set("@linux/appearance/material", json!("system")),
        set("@windows/hotkeys/bindings", json!({"ocr": "Control+Alt+KeyO"})),
        del("@linux/hotkeys/double_tap"),
        set("@freebsd/hotkeys/copy_wait_ms", json!(100)),
        set("@windows/something/new", json!(1)),
    ];
    let out = unproject_detailed(ConfigDoc::Settings, &base, &foreign, OsKind::Macos);
    assert_eq!(out.value, base);
    assert!(all_applied(&out.results), "其他系统的 key 与本机无关，不再重试");

    let doc = unproject(
        ConfigDoc::Settings,
        &base,
        &[set("@macos/hotkeys/copy_wait_ms", json!(400))],
        OsKind::Macos,
    );
    assert_eq!(doc["hotkeys"]["copy_wait_ms"], json!(400));

    // 同一份文档在 Windows 上投影为 @windows key
    let p = project(ConfigDoc::Settings, &base, OsKind::Windows);
    assert_eq!(p["@windows/hotkeys/copy_wait_ms"], json!(220));
    assert!(!p.keys().any(|k| k.starts_with("@macos")));

    // 一台 Windows 设备的全部投影应用到 Mac 上：全局字段跟随，按系统字段保持本机
    let win = val(&rich_settings(OsKind::Windows));
    let mut win = project(ConfigDoc::Settings, &win, OsKind::Windows);
    win.insert("@windows/hotkeys/copy_wait_ms".to_owned(), json!(480));
    win.insert("/translate/primary_lang".to_owned(), json!("ko"));
    let mac = val(&Settings::for_os(OsKind::Macos));
    let doc = unproject(ConfigDoc::Settings, &mac, &all_set(&win), OsKind::Macos);
    assert_eq!(doc["hotkeys"]["copy_wait_ms"], json!(150));
    assert_eq!(doc["hotkeys"]["bindings"], json!({}));
    assert_eq!(doc["appearance"]["material"], json!("system"));
    assert_eq!(doc["translate"]["primary_lang"], json!("ko"));
    assert_eq!(doc["hotkeys"]["double_tap"], mac["hotkeys"]["double_tap"]);
}

#[test]
fn local_fields_are_never_projected_and_always_preserved() {
    let rich = rich_settings(OsKind::Macos);
    let base = val(&rich);
    let p = project(ConfigDoc::Settings, &base, OsKind::Macos);
    for key in p.keys() {
        for local in [
            "/schema_version",
            "/general/launch_at_login",
            "/general/developer_mode",
            "/advanced/",
            "/sync/",
            "/clipboard/enabled",
            "/clipboard/secret_detection",
            "/ocr/history",
        ] {
            assert!(
                !(key.starts_with(local) && (local.ends_with('/') || key == local)),
                "{key}"
            );
        }
    }
    assert!(p.contains_key("/ocr/history_limit"));
    assert!(p.contains_key("/clipboard/max_items"));

    // 另一台设备的默认配置整体覆盖过来，外加直接针对本机字段的改动
    let mut changes = all_set(&project(
        ConfigDoc::Settings,
        &val(&Settings::for_os(OsKind::Macos)),
        OsKind::Macos,
    ));
    let local_changes = [
        set("/schema_version", json!(9)),
        set("/general/launch_at_login", json!(true)),
        set("/general/developer_mode", json!(false)),
        set("/advanced/log_level", json!("error")),
        set("/sync/enabled", json!(false)),
        set("/sync/server_url", json!("")),
        set("/clipboard/enabled", json!(true)),
        del("/clipboard/secret_detection"),
        set("/ocr/history", json!(false)),
    ];
    changes.extend(local_changes.iter().cloned());
    let out = unproject_detailed(ConfigDoc::Settings, &base, &changes, OsKind::Macos);
    assert!(all_applied(&out.results));
    let back: Settings = assert_parses(&out.value);
    assert!(!back.general.launch_at_login);
    assert!(back.general.developer_mode);
    assert_eq!(back.advanced, rich.advanced);
    assert_eq!(back.sync, rich.sync);
    assert!(!back.clipboard.enabled);
    assert!(!back.clipboard.secret_detection);
    assert!(back.ocr.history);
    // 全局字段跟随远端
    assert_eq!(back.translate.primary_lang, Lang::ZhCn);
    assert_eq!(back.ocr.history_limit, 50);
    assert_eq!(back.appearance.max_height_ratio, 0.6);
}

// ---------------------------------------------------------------------------------------------
// 集合元素
// ---------------------------------------------------------------------------------------------

#[test]
fn providers_delete_and_recreate() {
    let base = val(&rich_settings(OsKind::Macos));
    let openai = project(ConfigDoc::Settings, &base, OsKind::Macos)["/providers/openai"].clone();

    let gone = unproject(ConfigDoc::Settings, &base, &[del("/providers/openai")], OsKind::Macos);
    assert_eq!(provider_ids(&gone), ["@cf/workers", "local~1"]);
    let p = project(ConfigDoc::Settings, &gone, OsKind::Macos);
    assert!(!p.contains_key("/providers/openai"));
    assert_eq!(p["/providers/@order"], json!(["@cf/workers", "local~1"]));

    // 重建：本机已没有它，追加在后，学习缓存为空
    let back = unproject(
        ConfigDoc::Settings,
        &gone,
        &[set("/providers/openai", openai.clone())],
        OsKind::Macos,
    );
    assert_eq!(provider_ids(&back), ["@cf/workers", "local~1", "openai"]);
    assert!(model_of(&back, "openai", "gpt-4o").get("caps").is_none());

    // 同一批里先删后建：元素存在，学习缓存仍按本机原值注回
    let both = unproject(
        ConfigDoc::Settings,
        &base,
        &[del("/providers/openai"), set("/providers/openai", openai)],
        OsKind::Macos,
    );
    assert_eq!(
        model_of(&both, "openai", "gpt-4o")["caps"],
        model_of(&base, "openai", "gpt-4o")["caps"]
    );
    // 再给出顺序就回到原位
    let ordered = unproject(
        ConfigDoc::Settings,
        &both,
        &[set("/providers/@order", json!(["openai", "@cf/workers", "local~1"]))],
        OsKind::Macos,
    );
    assert_eq!(ordered, base);
}

#[test]
fn os_app_sets_are_keyed_by_os_and_app_id() {
    let base = val(&rich_settings(OsKind::Windows));
    let changes = vec![
        del("/clipboard/excluded_apps/macos:com.apple.keychainaccess"),
        set(
            "/clipboard/excluded_apps/windows:keepass.exe",
            json!({"os": "windows", "app_id": "keepass.exe", "name": "KeePass 2"}),
        ),
        // 值里的 os / app_id 与 key 不一致时以 key 为准
        set(
            "/clipboard/excluded_apps/linux:a~1b",
            json!({"os": "macos", "app_id": "zzz", "name": "AB"}),
        ),
        set(
            "/hotkeys/app_keymaps/linux:org~1gnome~0terminal@x",
            json!({"os": "linux", "app_id": "org/gnome~terminal@x", "copy": "Control+Shift+KeyC", "paste": null}),
        ),
        del("/hotkeys/app_keymaps/macos:com.googlecode.iterm2"),
    ];
    let out = unproject_detailed(ConfigDoc::Settings, &base, &changes, OsKind::Windows);
    assert!(all_applied(&out.results));
    let back: Settings = assert_parses(&out.value);
    let apps: Vec<(OsKind, &str)> = back
        .clipboard
        .excluded_apps
        .iter()
        .map(|a| (a.os, a.app_id.as_str()))
        .collect();
    assert!(!apps.contains(&(OsKind::Macos, "com.apple.keychainaccess")));
    // 新元素追加在后（按 key 排序），已有元素保持原位
    assert_eq!(
        &apps[apps.len() - 3..],
        &[
            (OsKind::Linux, "org.gnome/Seahorse"),
            (OsKind::Linux, "a/b"),
            (OsKind::Windows, "keepass.exe")
        ]
    );
    assert_eq!(apps[0], (OsKind::Macos, "com.1password.1password"));
    assert_eq!(apps.len(), 13);
    assert!(
        back.clipboard
            .excluded_apps
            .iter()
            .any(|a| a.app_id == "keepass.exe" && a.name == "KeePass 2")
    );
    let keymaps: Vec<&str> = back.hotkeys.app_keymaps.iter().map(|k| k.app_id.as_str()).collect();
    assert_eq!(keymaps, ["windowsterminal.exe", "org/gnome~terminal@x"]);
    assert_eq!(back.hotkeys.app_keymaps[1].copy.as_deref(), Some("Control+Shift+KeyC"));

    // 删掉后再加回来：追加在后
    let readd = unproject(
        ConfigDoc::Settings,
        &out.value,
        &[set(
            "/clipboard/excluded_apps/macos:com.apple.keychainaccess",
            json!({"name": "Keychain Access"}),
        )],
        OsKind::Windows,
    );
    let last = readd["clipboard"]["excluded_apps"]
        .as_array()
        .and_then(|a| a.last())
        .expect("last")
        .clone();
    assert_eq!(
        last,
        json!({"os": "macos", "app_id": "com.apple.keychainaccess", "name": "Keychain Access"})
    );
}

#[test]
fn provider_order_handling() {
    let mut s = Settings::for_os(OsKind::Linux);
    s.providers = ["a", "b", "c"].iter().map(|id| provider(id, Vec::new())).collect();
    let base = val(&s);
    let elem = |id: &str| val(&provider(id, Vec::new()));

    // order 里有不存在的 id（忽略），存在但不在 order 里的按 id 排序追加
    let doc = unproject(
        ConfigDoc::Settings,
        &base,
        &[set("/providers/@order", json!(["c", "zzz-missing", "a"]))],
        OsKind::Linux,
    );
    assert_eq!(provider_ids(&doc), ["c", "a", "b"]);

    // 没有顺序改动：已有元素原位，新元素按 id 排序追加
    let doc = unproject(
        ConfigDoc::Settings,
        &base,
        &[set("/providers/d2", elem("d2")), set("/providers/d1", elem("d1"))],
        OsKind::Linux,
    );
    assert_eq!(provider_ids(&doc), ["a", "b", "c", "d1", "d2"]);

    // 顺序改动在同一批的新增之后生效，与它在列表里的位置无关
    let doc = unproject(
        ConfigDoc::Settings,
        &base,
        &[
            set("/providers/@order", json!(["d2", "b"])),
            set("/providers/d2", elem("d2")),
            set("/providers/d1", elem("d1")),
        ],
        OsKind::Linux,
    );
    assert_eq!(provider_ids(&doc), ["d2", "b", "a", "c", "d1"]);

    // 顺序的墓碑什么也不做；形状不对的顺序推迟
    let out = unproject_detailed(
        ConfigDoc::Settings,
        &base,
        &[del("/providers/@order"), set("/providers/@order", json!({"a": 1}))],
        OsKind::Linux,
    );
    assert_eq!(out.value, base);
    assert_eq!(out.results, [KeyApply::Applied, KeyApply::Deferred]);

    // 本机没有任何 provider 字段时，只给顺序不会凭空添加
    let mut bare = val(&Settings::for_os(OsKind::Linux));
    bare.as_object_mut().expect("object").remove("providers");
    let doc = unproject(
        ConfigDoc::Settings,
        &bare,
        &[set("/providers/@order", json!(["x"]))],
        OsKind::Linux,
    );
    assert_eq!(doc, bare);
}

#[test]
fn ids_with_reserved_characters_are_escaped() {
    let mut s = rich_settings(OsKind::Macos);
    s.providers.push(provider("@order", Vec::new()));
    let base = val(&s);
    let p = project(ConfigDoc::Settings, &base, OsKind::Macos);
    for key in [
        "/providers/openai",
        "/providers/~2cf~1workers",
        "/providers/local~01",
        "/providers/~2order",
        "/clipboard/excluded_apps/linux:org.gnome~1Seahorse",
        "/hotkeys/app_keymaps/linux:org~1gnome~0terminal@x",
    ] {
        assert!(p.contains_key(key), "{key}");
    }
    assert_eq!(
        p["/providers/@order"],
        json!(["openai", "@cf/workers", "local~1", "@order"])
    );
    assert_eq!(p["/providers/~2order"]["id"], json!("@order"));
    assert_eq!(escape_segment("@cf/workers"), "~2cf~1workers");
    assert_round_trip(ConfigDoc::Settings, &base, OsKind::Macos);

    let styles = project(ConfigDoc::Styles, &val(&rich_styles()), OsKind::Linux);
    for key in [
        "/custom/custom-a~1b~0c",
        "/custom/~2custom-x",
        "@linux/style_hotkey/~2custom-x",
        "/order",
    ] {
        assert!(styles.contains_key(key), "{key}");
    }

    // 删除一个 id 为「@order」的 provider 不影响顺序寄存器
    let doc = unproject(ConfigDoc::Settings, &base, &[del("/providers/~2order")], OsKind::Macos);
    assert_eq!(provider_ids(&doc), ["openai", "@cf/workers", "local~1"]);
}

// ---------------------------------------------------------------------------------------------
// 风格
// ---------------------------------------------------------------------------------------------

#[test]
fn style_hotkeys_are_split_out_per_os() {
    let base = val(&rich_styles());
    let p = project(ConfigDoc::Styles, &base, OsKind::Macos);
    assert_eq!(p["@macos/style_hotkey/natural"], json!("Alt+Shift+KeyN"));
    assert_eq!(p["@macos/style_hotkey/technical"], json!("Alt+Shift+KeyT"));
    assert_eq!(p["@macos/style_hotkey/custom-1a2b3c4d"], json!("Control+Alt+KeyL"));
    assert!(!p.contains_key("@macos/style_hotkey/custom-a~1b~0c"));
    // 只有快捷键的覆盖项不产出覆盖寄存器
    assert!(!p.contains_key("/overrides/technical"));
    assert_eq!(p["/overrides/natural"], json!({"temperature": 0.7}));
    let custom = p["/custom/custom-1a2b3c4d"].as_object().expect("custom");
    for f in ["hotkey", "builtin", "modified"] {
        assert!(!custom.contains_key(f), "{f}");
    }
    assert_eq!(custom["id"], json!("custom-1a2b3c4d"));

    // Windows 设备收到 Mac 的全部投影：内容跟随，快捷键保持 Windows 本机的
    let mut win = rich_styles();
    win.overrides.get_mut("natural").expect("natural").hotkey = Some(Some("Control+Alt+KeyN".to_owned()));
    win.overrides.remove("technical");
    win.overrides.get_mut("casual").expect("casual").hotkey = Some(Some("Control+Alt+KeyC".to_owned()));
    win.custom[0].hotkey = None;
    win.custom[0].name = "old name".to_owned();
    let win_base = val(&win);
    let out = unproject_detailed(ConfigDoc::Styles, &win_base, &all_set(&p), OsKind::Windows);
    assert!(all_applied(&out.results));
    let back: StylesFile = assert_parses(&out.value);
    assert_eq!(
        back.overrides["natural"].hotkey,
        Some(Some("Control+Alt+KeyN".to_owned()))
    );
    assert_eq!(back.overrides["natural"].temperature, Some(Some(0.7)));
    assert!(!back.overrides.contains_key("technical"));
    assert_eq!(
        back.overrides["casual"].hotkey,
        Some(Some("Control+Alt+KeyC".to_owned()))
    );
    assert_eq!(back.overrides["casual"].tail_instruction.as_deref(), Some("be casual"));
    assert_eq!(back.custom[0].name, "custom-1a2b3c4d name");
    assert_eq!(back.custom[0].hotkey, None);
}

#[test]
fn style_overrides_keep_the_local_hotkey() {
    let base = val(&rich_styles());
    // 远端撤销 natural 的覆盖：本机的快捷键留下
    let doc = unproject(ConfigDoc::Styles, &base, &[del("/overrides/natural")], OsKind::Macos);
    assert_eq!(doc["overrides"]["natural"], json!({"hotkey": "Alt+Shift+KeyN"}));
    // 远端撤销 casual 的覆盖：本机没有快捷键，整条删掉
    let doc = unproject(ConfigDoc::Styles, &base, &[del("/overrides/casual")], OsKind::Macos);
    assert!(doc["overrides"].get("casual").is_none());
    // 远端给 technical 加了覆盖（值里夹带的快捷键被忽略）
    let doc = unproject(
        ConfigDoc::Styles,
        &base,
        &[set(
            "/overrides/technical",
            json!({"dictionary_mode": false, "hotkey": "Alt+KeyZ"}),
        )],
        OsKind::Macos,
    );
    assert_eq!(
        doc["overrides"]["technical"],
        json!({"dictionary_mode": false, "hotkey": "Alt+Shift+KeyT"})
    );
    // 空覆盖等同墓碑
    let doc = unproject(
        ConfigDoc::Styles,
        &base,
        &[set("/overrides/casual", json!({}))],
        OsKind::Macos,
    );
    assert!(doc["overrides"].get("casual").is_none());
    // 本版本不认识的内置风格：推迟
    let out = unproject_detailed(
        ConfigDoc::Styles,
        &base,
        &[set("/overrides/poetic", json!({"temperature": 0.9}))],
        OsKind::Macos,
    );
    assert_eq!(out.value, base);
    assert_eq!(out.results, [KeyApply::Deferred]);
}

#[test]
fn style_hotkey_application() {
    let base = val(&rich_styles());
    let mut fresh_value = val(&custom_style("custom-new", None));
    for f in ["hotkey", "builtin", "modified"] {
        fresh_value.as_object_mut().expect("object").remove(f);
    }

    // 快捷键先于风格本身到达：推迟
    let out = unproject_detailed(
        ConfigDoc::Styles,
        &base,
        &[set("@macos/style_hotkey/custom-new", json!("Alt+KeyQ"))],
        OsKind::Macos,
    );
    assert_eq!(out.value, base);
    assert_eq!(out.results, [KeyApply::Deferred]);

    // 同一批里有风格本身：不论在列表里的先后都能应用
    let out = unproject_detailed(
        ConfigDoc::Styles,
        &base,
        &[
            set("@macos/style_hotkey/custom-new", json!("Alt+KeyQ")),
            set("/custom/custom-new", fresh_value.clone()),
        ],
        OsKind::Macos,
    );
    assert!(all_applied(&out.results));
    let back: StylesFile = assert_parses(&out.value);
    let added = back.custom.last().expect("added");
    assert_eq!(added.id, "custom-new");
    assert_eq!(added.hotkey.as_deref(), Some("Alt+KeyQ"));
    assert_eq!(back.order, rich_styles().order, "顺序是单独的寄存器");

    // 删除不存在的风格的快捷键：已经是这个状态
    let out = unproject_detailed(
        ConfigDoc::Styles,
        &base,
        &[del("@macos/style_hotkey/custom-new")],
        OsKind::Macos,
    );
    assert_eq!(out.results, [KeyApply::Applied]);
    assert_eq!(out.value, base);

    // 删除只剩快捷键的内置覆盖项的快捷键：整条覆盖删掉
    let doc = unproject(
        ConfigDoc::Styles,
        &base,
        &[
            del("@macos/style_hotkey/technical"),
            del("@macos/style_hotkey/custom-1a2b3c4d"),
        ],
        OsKind::Macos,
    );
    assert!(doc["overrides"].get("technical").is_none());
    assert_eq!(doc["custom"][0]["hotkey"], Value::Null);

    // 给没有覆盖项的内置风格设快捷键：创建覆盖项
    let doc = unproject(
        ConfigDoc::Styles,
        &base,
        &[set("@macos/style_hotkey/academic", json!("Alt+KeyA"))],
        OsKind::Macos,
    );
    assert_eq!(doc["overrides"]["academic"], json!({"hotkey": "Alt+KeyA"}));

    // 其他系统的快捷键：与本机无关
    let out = unproject_detailed(
        ConfigDoc::Styles,
        &base,
        &[set("@windows/style_hotkey/natural", json!("Control+KeyN"))],
        OsKind::Macos,
    );
    assert_eq!(out.value, base);
    assert_eq!(out.results, [KeyApply::Applied]);

    // 形状不对的快捷键值、不认识的 key：推迟
    let out = unproject_detailed(
        ConfigDoc::Styles,
        &base,
        &[
            set("@macos/style_hotkey/natural", json!(5)),
            set("@macos/style_hotkeys/natural", json!("Alt+KeyN")),
            set("/future", json!(1)),
        ],
        OsKind::Macos,
    );
    assert_eq!(out.value, base);
    assert!(out.results.iter().all(|r| *r == KeyApply::Deferred));
}

#[test]
fn custom_styles_delete_and_recreate() {
    let base = val(&rich_styles());
    let p = project(ConfigDoc::Styles, &base, OsKind::Macos);
    let element = p["/custom/custom-1a2b3c4d"].clone();
    let gone = unproject(
        ConfigDoc::Styles,
        &base,
        &[del("/custom/custom-1a2b3c4d")],
        OsKind::Macos,
    );
    let ids: Vec<&str> = gone["custom"]
        .as_array()
        .expect("custom")
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert_eq!(ids, ["custom-a/b~c", "@custom-x"]);
    let back = unproject(
        ConfigDoc::Styles,
        &gone,
        &[set("/custom/custom-1a2b3c4d", element.clone())],
        OsKind::Macos,
    );
    let last = back["custom"].as_array().and_then(|a| a.last()).expect("last");
    assert_eq!(last["id"], json!("custom-1a2b3c4d"));
    assert!(last.get("hotkey").is_none(), "重建的风格没有本机快捷键");
    // 替换已有的自定义风格时保留本机的快捷键与计算字段
    let mut renamed = element;
    renamed["name"] = json!("Renamed");
    let doc = unproject(
        ConfigDoc::Styles,
        &base,
        &[set("/custom/custom-1a2b3c4d", renamed)],
        OsKind::Macos,
    );
    assert_eq!(doc["custom"][0]["name"], json!("Renamed"));
    assert_eq!(doc["custom"][0]["hotkey"], json!("Control+Alt+KeyL"));
    assert_eq!(doc["custom"][0]["builtin"], json!(false));
}

// ---------------------------------------------------------------------------------------------
// 术语表
// ---------------------------------------------------------------------------------------------

#[test]
fn glossary_terms_keep_local_order() {
    let base = val(&rich_glossary());
    let p = project(ConfigDoc::Glossary, &base, OsKind::Macos);
    assert_eq!(p["Wisp"], json!({"target": "微光", "note": "产品名"}));
    assert_eq!(p["I/O"], json!({"target": "输入输出", "note": null}));
    assert!(p.contains_key("@mention") && p.contains_key("~tilde"));

    let out = unproject_detailed(
        ConfigDoc::Glossary,
        &base,
        &[
            set("Zeta", json!({"target": "泽塔", "note": null})),
            set("Wisp", json!({"target": "Wisp", "note": null, "source": "ignored"})),
            del("I/O"),
            set("Alpha", json!({"target": "阿尔法", "note": null})),
            del("not-here"),
            set("", json!({"target": "x", "note": null})),
            set("Beta", json!("not an object")),
        ],
        OsKind::Macos,
    );
    assert_eq!(
        out.results,
        [
            KeyApply::Applied,
            KeyApply::Applied,
            KeyApply::Applied,
            KeyApply::Applied,
            KeyApply::Applied,
            KeyApply::Deferred,
            KeyApply::Deferred,
        ]
    );
    let back: GlossaryFile = assert_parses(&out.value);
    let sources: Vec<&str> = back.entries.iter().map(|e| e.source.as_str()).collect();
    assert_eq!(sources, ["Wisp", "@mention", "~tilde", "Alpha", "Zeta"]);
    assert_eq!(back.entries[0], entry("Wisp", "Wisp", None));
}

// ---------------------------------------------------------------------------------------------
// 本版本不认识的内容
// ---------------------------------------------------------------------------------------------

#[test]
fn unknown_content_is_neither_projected_nor_applied() {
    let mut base = val(&rich_settings(OsKind::Macos));
    base["future_group"] = json!({"x": 1});
    base["future_scalar"] = json!(true);
    base["translate"]["future_field"] = json!("keep me");
    base["providers"][0]["future"] = json!({"nested": 1});
    let p = project(ConfigDoc::Settings, &base, OsKind::Macos);
    assert!(!p.keys().any(|k| k.contains("future_")));
    assert_eq!(p["/providers/openai"]["future"], json!({"nested": 1}));

    let changes = vec![
        set("/future_group/x", json!(2)),
        set("/future_scalar", json!(false)),
        set("/translate/future_field", json!("changed")),
        set("@macos/translate/primary_lang", json!("en")),
        set("/hotkeys/bindings", json!({})),
        set("/clipboard/excluded_apps", json!([])),
        set("/clipboard/excluded_apps/no-colon", json!({})),
        set("/providers/openai", json!("not an object")),
        set("not a key", json!(1)),
        set("/providers/a~3b", json!({})),
        set("/translate/primary_lang", json!("en")),
    ];
    let out = unproject_detailed(ConfigDoc::Settings, &base, &changes, OsKind::Macos);
    let (last, rest) = out.results.split_last().expect("results");
    assert_eq!(*last, KeyApply::Applied);
    assert!(rest.iter().all(|r| *r == KeyApply::Deferred), "{rest:?}");
    let mut expected = base.clone();
    expected["translate"]["primary_lang"] = json!("en");
    assert_eq!(out.value, expected, "本机文件里的未知字段原样保留");

    // 文档不是对象：原样返回，全部推迟
    let out = unproject_detailed(
        ConfigDoc::Settings,
        &json!([1]),
        &[set("/translate/primary_lang", json!("en"))],
        OsKind::Macos,
    );
    assert_eq!(out.value, json!([1]));
    assert_eq!(out.results, [KeyApply::Deferred]);
    assert!(project(ConfigDoc::Styles, &json!("x"), OsKind::Macos).is_empty());
}

#[test]
fn removed_scalar_field_falls_back_to_default() {
    let base = val(&rich_settings(OsKind::Linux));
    let doc = unproject(
        ConfigDoc::Settings,
        &base,
        &[del("/translate/primary_lang")],
        OsKind::Linux,
    );
    assert!(doc["translate"].get("primary_lang").is_none());
    let (back, _) = parse_lenient::<Settings>(&doc.to_string()).expect("json");
    assert_eq!(back.translate.primary_lang, Lang::ZhCn);
    assert_eq!(back.translate.default_style, "custom-1a2b3c4d");
}

// ---------------------------------------------------------------------------------------------
// 默认值与元素判定
// ---------------------------------------------------------------------------------------------

#[test]
fn is_default_uses_builtin_defaults() {
    for os in ALL_OS {
        let p = project(ConfigDoc::Settings, &val(&Settings::for_os(os)), os);
        for (key, value) in &p {
            assert_eq!(
                is_default(ConfigDoc::Settings, key, value, os),
                !is_element_key(ConfigDoc::Settings, key),
                "{key}"
            );
        }
    }
    let os = OsKind::Macos;
    assert!(!is_default(
        ConfigDoc::Settings,
        "/translate/primary_lang",
        &json!("ja"),
        os
    ));
    assert!(is_default(
        ConfigDoc::Settings,
        "/translate/primary_lang",
        &json!("zh-CN"),
        os
    ));
    assert!(is_default(
        ConfigDoc::Settings,
        "/appearance/max_height_ratio",
        &json!(0.6),
        os
    ));
    assert!(!is_default(
        ConfigDoc::Settings,
        "/appearance/max_height_ratio",
        &json!(0.6000000238418579),
        os
    ));
    // 按系统的 key 用 key 自己的系统的默认值
    assert!(is_default(
        ConfigDoc::Settings,
        "@windows/hotkeys/copy_wait_ms",
        &json!(150),
        os
    ));
    assert!(!is_default(
        ConfigDoc::Settings,
        "@freebsd/hotkeys/copy_wait_ms",
        &json!(150),
        os
    ));
    assert!(is_default(ConfigDoc::Settings, "/providers/@order", &json!([]), os));
    assert!(!is_default(ConfigDoc::Settings, "/providers/@order", &json!(["a"]), os));
    assert!(!is_default(
        ConfigDoc::Settings,
        "/general/launch_at_login",
        &json!(true),
        os
    ));
    assert!(!is_default(ConfigDoc::Settings, "/future/x", &Value::Null, os));
    assert!(is_default(ConfigDoc::Styles, "/order", &json!([]), os));
    assert!(!is_default(ConfigDoc::Styles, "/order", &json!(["natural"]), os));
    assert!(!is_default(
        ConfigDoc::Styles,
        "@macos/style_hotkey/natural",
        &Value::Null,
        os
    ));
    assert!(!is_default(
        ConfigDoc::Glossary,
        "Wisp",
        &json!({"target": "", "note": null}),
        os
    ));
}

#[test]
fn element_keys() {
    for (doc, key, element) in [
        (ConfigDoc::Settings, "/providers/openai", true),
        (ConfigDoc::Settings, "/providers/~2order", true),
        (ConfigDoc::Settings, "/providers/@order", false),
        (ConfigDoc::Settings, "/clipboard/excluded_apps/macos:x", true),
        (ConfigDoc::Settings, "/hotkeys/app_keymaps/windows:x.exe", true),
        (ConfigDoc::Settings, "/translate/primary_lang", false),
        (ConfigDoc::Settings, "@macos/hotkeys/bindings", false),
        (ConfigDoc::Styles, "/overrides/natural", true),
        (ConfigDoc::Styles, "/custom/custom-1", true),
        (ConfigDoc::Styles, "/order", false),
        (ConfigDoc::Styles, "@macos/style_hotkey/natural", false),
        (ConfigDoc::Glossary, "Wisp", true),
        (ConfigDoc::Glossary, "", false),
    ] {
        assert_eq!(is_element_key(doc, key), element, "{doc:?} {key}");
    }
}

#[test]
fn config_doc_domains() {
    for doc in ConfigDoc::ALL {
        assert_eq!(ConfigDoc::from_domain(&doc.domain()), Some(doc));
    }
    assert_eq!(projection::os_tag(OsKind::Windows), "windows");
}

// ---------------------------------------------------------------------------------------------
// 差异
// ---------------------------------------------------------------------------------------------

fn map(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect()
}

#[test]
fn diff_table() {
    let cases: Vec<(Projection, Projection, Changes)> = vec![
        (map(&[]), map(&[]), vec![]),
        (map(&[("/a", json!(1))]), map(&[("/a", json!(1))]), vec![]),
        (
            map(&[("/a", json!(1))]),
            map(&[("/a", json!(2))]),
            vec![set("/a", json!(2))],
        ),
        (map(&[("/a", json!(1))]), map(&[]), vec![del("/a")]),
        (map(&[]), map(&[("/a", Value::Null)]), vec![set("/a", Value::Null)]),
        (map(&[("/a", Value::Null)]), map(&[]), vec![del("/a")]),
        // 整数与浮点数是不同的值：投影的输入必须先规范化
        (
            map(&[("/a", json!(1))]),
            map(&[("/a", json!(1.0))]),
            vec![set("/a", json!(1.0))],
        ),
        (
            map(&[("/a", json!(1)), ("/c", json!([1])), ("@macos/x", json!({"k": 1}))]),
            map(&[("/b", json!(2)), ("/c", json!([1, 2])), ("@macos/x", json!({"k": 1}))]),
            vec![del("/a"), set("/b", json!(2)), set("/c", json!([1, 2]))],
        ),
    ];
    for (before, after, expected) in cases {
        assert_eq!(diff(&before, &after), expected, "{before:?} → {after:?}");
    }
}

fn arb_map() -> impl Strategy<Value = BTreeMap<String, Value>> {
    let key = prop::sample::select(vec!["/a", "/b", "/c/d", "@macos/e", "/p/@order", "/q/~2x"]);
    let value = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        (0i64..4).prop_map(|n| json!(n)),
        prop::sample::select(vec!["x", "y"]).prop_map(|s| json!([s])),
    ];
    prop::collection::btree_map(key.prop_map(str::to_owned), value, 0..6)
}

// ---------------------------------------------------------------------------------------------
// 随机文档对：把 x 与 y 的投影差异应用到 x 上，得到的文档投影等于 y 的投影
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ModelSpec {
    id: &'static str,
    label: bool,
    vision: u8,
    caps: bool,
    deprecated: bool,
}

fn arb_models() -> impl Strategy<Value = Vec<ModelSpec>> {
    prop::sample::subsequence(vec!["gpt-4o", "@cf/meta/llama", "m~1", "o3"], 0..=4)
        .prop_shuffle()
        .prop_flat_map(|ids| {
            let n = ids.len();
            (
                Just(ids),
                prop::collection::vec((any::<bool>(), 0u8..4, any::<bool>(), any::<bool>()), n),
            )
        })
        .prop_map(|(ids, specs)| {
            ids.into_iter()
                .zip(specs)
                .map(|(id, (label, vision, caps, deprecated))| ModelSpec {
                    id,
                    label,
                    vision,
                    caps,
                    deprecated,
                })
                .collect()
        })
}

fn build_model(spec: &ModelSpec) -> ModelEntry {
    let (vision, source) = match spec.vision {
        0 => (Vision::Unknown, None),
        1 => (Vision::Yes, Some(VisionSource::Capability)),
        2 => (Vision::No, Some(VisionSource::Keyword)),
        _ => (Vision::Yes, Some(VisionSource::User)),
    };
    ModelEntry {
        label: spec.label.then(|| format!("{} label", spec.id)),
        caps: if spec.caps {
            caps(spec.label)
        } else {
            ModelCaps::default()
        },
        deprecated: spec.deprecated,
        missing_upstream: spec.caps && spec.deprecated,
        ..model(spec.id, vision, source)
    }
}

prop_compose! {
    fn arb_settings()(
        providers in prop::sample::subsequence(vec!["openai", "deepseek", "@cf/w", "a~b", "@order"], 0..=5)
            .prop_shuffle(),
        models in prop::collection::vec(arb_models(), 5),
        names in prop::collection::vec(prop::sample::select(vec!["A", "B"]), 5),
        lang in prop::sample::select(vec![Lang::ZhCn, Lang::En, Lang::Ja]),
        copy_wait in 50u32..=500,
        excluded in prop::sample::subsequence(
            {
                let mut all = wisp_types::defaults::default_excluded_apps();
                all.push(ExcludedApp { os: OsKind::Linux, app_id: "x/y".to_owned(), name: "XY".to_owned() });
                all.push(ExcludedApp { os: OsKind::Windows, app_id: "@z.exe".to_owned(), name: String::new() });
                all
            },
            0..=14,
        ).prop_shuffle(),
        ratio in prop::sample::select(vec![0.4f32, 0.45, 0.6, 0.73]),
        local in (any::<bool>(), any::<bool>(), any::<bool>(), prop::sample::select(vec![LogLevel::Info, LogLevel::Debug])),
        binding in prop::option::of(prop::sample::select(vec!["Alt+KeyX", "none", "default"])),
    ) -> Settings {
        let mut s = Settings {
            providers: providers
                .iter()
                .enumerate()
                .map(|(i, id)| {
                    let mut p = provider(id, models[i].iter().map(build_model).collect());
                    p.name = names[i].to_owned();
                    p
                })
                .collect(),
            ..Settings::default()
        };
        s.translate.primary_lang = lang;
        s.hotkeys.copy_wait_ms = copy_wait;
        if let Some(b) = binding {
            s.hotkeys.bindings.insert(HotkeyAction::Ocr, Binding(b.to_owned()));
        }
        s.clipboard.excluded_apps = excluded;
        s.appearance.max_height_ratio = ratio;
        s.general.launch_at_login = local.0;
        s.clipboard.enabled = local.1;
        s.ocr.history = local.2;
        s.advanced.log_level = local.3;
        s.sync.auto = !local.0;
        s
    }
}

fn arb_override() -> impl Strategy<Value = StyleOverride> {
    (
        prop::option::of(prop::option::of(prop::sample::select(vec![0.1f32, 0.7]))),
        prop::option::of(prop::option::of(prop::sample::select(vec!["Alt+KeyN", "Alt+KeyM"]))),
        prop::option::of(any::<bool>()),
    )
        .prop_map(|(temperature, hotkey, dictionary_mode)| StyleOverride {
            temperature,
            hotkey: hotkey.map(|h| h.map(str::to_owned)),
            dictionary_mode,
            ..StyleOverride::default()
        })
}

prop_compose! {
    fn arb_styles()(
        order in prop::sample::subsequence(
            vec!["natural", "technical", "academic", "custom-1", "custom-a/b", "@c"], 0..=6
        ).prop_shuffle(),
        overrides in prop::collection::btree_map(
            prop::sample::select(vec!["natural", "technical", "casual"]), arb_override(), 0..=3
        ),
        custom in prop::sample::subsequence(vec!["custom-1", "custom-a/b", "@c"], 0..=3).prop_shuffle(),
        hotkeys in prop::collection::vec(prop::option::of(prop::sample::select(vec!["Alt+KeyQ", "Alt+KeyW"])), 3),
        temps in prop::collection::vec(prop::option::of(prop::sample::select(vec![0.3f32, 0.55])), 3),
    ) -> StylesFile {
        StylesFile {
            schema_version: 1,
            order: order.into_iter().map(str::to_owned).collect(),
            overrides: overrides.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
            custom: custom
                .into_iter()
                .enumerate()
                .map(|(i, id)| Style { temperature: temps[i], ..custom_style(id, hotkeys[i]) })
                .collect(),
        }
    }
}

prop_compose! {
    fn arb_glossary()(
        sources in prop::sample::subsequence(vec!["Wisp", "I/O", "@x", "~t", "a b"], 0..=5).prop_shuffle(),
        targets in prop::collection::vec(prop::sample::select(vec!["甲", "乙"]), 5),
        notes in prop::collection::vec(prop::option::of(prop::sample::select(vec!["n1", "n2"])), 5),
    ) -> GlossaryFile {
        GlossaryFile {
            schema_version: 1,
            entries: sources
                .into_iter()
                .enumerate()
                .map(|(i, s)| entry(s, targets[i], notes[i]))
                .collect(),
        }
    }
}

fn arb_os() -> impl Strategy<Value = OsKind> {
    prop::sample::select(ALL_OS.to_vec())
}

/// 应用差异后：每项都已应用、投影等于目标、原文档的往返成立
fn check_pair(doc: ConfigDoc, x: &Value, y: &Value, os: OsKind) -> Result<Value, TestCaseError> {
    let px = project(doc, x, os);
    let py = project(doc, y, os);
    let out = unproject_detailed(doc, x, &diff(&px, &py), os);
    prop_assert!(all_applied(&out.results), "{:?}", out.results);
    prop_assert_eq!(project(doc, &out.value, os), py);
    prop_assert_eq!(&unproject(doc, x, &all_set(&px), os), x);
    Ok(out.value)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn diff_then_apply_reaches_target(before in arb_map(), after in arb_map()) {
        let changes = diff(&before, &after);
        let mut applied = before.clone();
        for (k, v) in &changes {
            match v {
                Some(v) => { applied.insert(k.clone(), v.clone()); }
                None => { applied.remove(k); }
            }
        }
        prop_assert_eq!(&applied, &after);
        prop_assert!(changes.windows(2).all(|w| w[0].0 < w[1].0), "按 key 排序且不重复");
        prop_assert!(diff(&after, &after).is_empty());
    }

    #[test]
    fn settings_projection_differences_apply(x in arb_settings(), y in arb_settings(), os in arb_os()) {
        let applied = check_pair(ConfigDoc::Settings, &val(&x), &val(&y), os)?;
        let (back, issues) = parse_lenient::<Settings>(&applied.to_string()).expect("json");
        prop_assert!(issues.is_empty(), "{:?}", issues);
        // 只属于本机的字段来自 x
        prop_assert_eq!(back.general.launch_at_login, x.general.launch_at_login);
        prop_assert_eq!(back.clipboard.enabled, x.clipboard.enabled);
        prop_assert_eq!(back.ocr.history, x.ocr.history);
        prop_assert_eq!(&back.advanced, &x.advanced);
        prop_assert_eq!(&back.sync, &x.sync);
        // 内容来自 y，学习缓存按 (provider id, model id) 来自 x
        prop_assert_eq!(
            back.providers.iter().map(|p| &p.id).collect::<Vec<_>>(),
            y.providers.iter().map(|p| &p.id).collect::<Vec<_>>()
        );
        for (p, yp) in back.providers.iter().zip(&y.providers) {
            prop_assert_eq!(&p.name, &yp.name);
            let xp = x.provider(&p.id);
            for (m, ym) in p.models.iter().zip(&yp.models) {
                prop_assert_eq!(&m.id, &ym.id);
                prop_assert_eq!(&m.label, &ym.label);
                let xm = xp.and_then(|xp| xp.models.iter().find(|xm| xm.id == m.id));
                // 投影没变的 provider 不产生改动，整体保持本机值（含学习缓存）
                if xp.is_some_and(|xp| project_provider(xp) == project_provider(yp)) {
                    prop_assert_eq!(Some(m), xm);
                    continue;
                }
                let (caps, deprecated, missing) = match xm {
                    Some(xm) => (xm.caps.clone(), xm.deprecated, xm.missing_upstream),
                    None => (ModelCaps::default(), false, false),
                };
                prop_assert_eq!(&m.caps, &caps);
                prop_assert_eq!(m.deprecated, deprecated);
                prop_assert_eq!(m.missing_upstream, missing);
                let vision = if ym.vision_source == Some(VisionSource::User) {
                    (ym.vision, ym.vision_source)
                } else {
                    match xm {
                        Some(xm) if xm.vision_source != Some(VisionSource::User) => (xm.vision, xm.vision_source),
                        _ => (Vision::Unknown, None),
                    }
                };
                prop_assert_eq!((m.vision, m.vision_source), vision);
            }
        }
    }

    #[test]
    fn styles_projection_differences_apply(x in arb_styles(), y in arb_styles(), os in arb_os()) {
        let applied = check_pair(ConfigDoc::Styles, &val(&x), &val(&y), os)?;
        let (_, issues) = parse_lenient::<StylesFile>(&applied.to_string()).expect("json");
        prop_assert!(issues.is_empty(), "{:?}", issues);
    }

    #[test]
    fn glossary_projection_differences_apply(x in arb_glossary(), y in arb_glossary(), os in arb_os()) {
        let applied = check_pair(ConfigDoc::Glossary, &val(&x), &val(&y), os)?;
        let (back, issues) = parse_lenient::<GlossaryFile>(&applied.to_string()).expect("json");
        prop_assert!(issues.is_empty(), "{:?}", issues);
        // 本机已有、目标里也有的术语保持本机的相对顺序
        let kept: Vec<&str> = x
            .entries
            .iter()
            .map(|e| e.source.as_str())
            .filter(|s| y.entries.iter().any(|e| e.source == *s))
            .collect();
        let prefix: Vec<&str> = back.entries.iter().take(kept.len()).map(|e| e.source.as_str()).collect();
        prop_assert_eq!(prefix, kept);
    }
}

/// 单个 provider 的投影值（去掉学习缓存后）
fn project_provider(p: &Provider) -> Value {
    let s = Settings {
        providers: vec![p.clone()],
        ..Settings::default()
    };
    project(ConfigDoc::Settings, &val(&s), OsKind::Macos)
        .remove(&format!("/providers/{}", escape_segment(&p.id)))
        .expect("provider")
}
