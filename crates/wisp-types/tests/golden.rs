//! 黄金样例契约测试。
//!
//! 为每个 IPC 类型与配置文档构造代表性样例（联合类型的每个变体都有），用
//! `serde_json::to_string_pretty` 序列化后与 `apps/desktop/src/shared/ipc/fixtures/<Type>[.<variant>].json`
//! 比较；同时生成汇总文件 `fixtures/index.ts`（每个样例写成 `export const X = {…} satisfies T.X`，
//! 由 tsc 做字面量级别的类型检查）。
//!
//! 设置环境变量 `UPDATE_GOLDEN=1` 时改为重写这些文件（并删除不再存在的旧样例）。
//! 只在 Rust 侧存在、不跨 IPC 的类型（PhysRect、StyleOverride、StylesFile、GlossaryFile、
//! RawClipboard、ClipPayload、Binding）只做往返测试，不写 fixtures。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use wisp_types::*;

/// 一个黄金样例
struct Sample {
    /// 文件名（不含 .json），形如 `PanelPayload.translate`
    name: String,
    /// index.ts 中 `satisfies` 后面的 TS 类型表达式
    ts_type: String,
    json: String,
}

/// 收集样例，并对每个样例做往返断言
#[derive(Default)]
struct Samples(Vec<Sample>);

impl Samples {
    /// 以同名 TS 类型登记样例：`T.<Type>`
    fn add<V: Serialize + DeserializeOwned + PartialEq + Debug>(&mut self, name: &str, v: V) {
        let ty = name.split('.').next().expect("样例名非空");
        self.add_as(name, &format!("T.{ty}"), v);
    }

    /// 以指定的 TS 类型表达式登记样例
    fn add_as<V: Serialize + DeserializeOwned + PartialEq + Debug>(&mut self, name: &str, ts_type: &str, v: V) {
        let json = roundtrip(&v);
        assert!(!self.0.iter().any(|s| s.name == name), "样例名重复：{name}");
        self.0.push(Sample {
            name: name.to_owned(),
            ts_type: ts_type.to_owned(),
            json,
        });
    }
}

/// 序列化 → 反序列化 → 断言相等，返回 pretty JSON
fn roundtrip<V: Serialize + DeserializeOwned + PartialEq + Debug>(v: &V) -> String {
    let json = serde_json::to_string_pretty(v).expect("序列化失败");
    let back: V = serde_json::from_str(&json).unwrap_or_else(|e| panic!("反序列化失败：{e}\n{json}"));
    assert_eq!(&back, v, "往返不一致：\n{json}");
    json
}

// ---------------------------------------------------------------------------
// 样例构造
// ---------------------------------------------------------------------------

fn s(v: &str) -> String {
    v.to_owned()
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
    Rect { x, y, width, height }
}

fn monitor() -> MonitorInfo {
    MonitorInfo {
        id: s("1"),
        name: s("Built-in Retina Display"),
        physical_origin: PhysPoint { x: 0, y: 0 },
        physical_size: PhysSize {
            width: 3024,
            height: 1964,
        },
        scale_factor: 2.0,
        frame: rect(0.0, 0.0, 1512.0, 982.0),
        work_area: rect(0.0, 33.0, 1512.0, 949.0),
        is_primary: true,
    }
}

fn anchor() -> Anchor {
    Anchor {
        monitor_id: s("1"),
        rect: rect(412.5, 300.0, 180.0, 18.0),
    }
}

fn app_ref() -> AppRef {
    AppRef {
        id: s("com.apple.Safari"),
        alt_id: None,
        name: s("Safari"),
        pid: Some(4242),
        path: Some(s("/Applications/Safari.app")),
        window: None,
    }
}

fn capabilities() -> Capabilities {
    Capabilities {
        global_hotkeys: Capability::yes(),
        selection: Capability::yes(),
        simulated_copy: Capability::yes(),
        paste_inject: Capability::yes(),
        capture_overlay: Capability::yes(),
        clipboard_watch: Capability::yes(),
        input_hooks: Capability::no("errors.platform.input_monitoring_missing"),
    }
}

fn platform_info() -> PlatformInfo {
    PlatformInfo {
        os: OsKind::Macos,
        os_version: s("26.4"),
        arch: s("aarch64"),
        session: SessionKind::Native,
        capabilities: capabilities(),
    }
}

fn model_caps() -> ModelCaps {
    ModelCaps {
        temperature: Some(false),
        max_tokens_field: Some(MaxTokensField::MaxCompletionTokens),
        thinking_param: None,
        effort_param: Some(true),
        stream_options: Some(true),
        include_obfuscation: None,
        anthropic_thinking_disabled: None,
        anthropic_efforts: None,
    }
}

fn model_entry() -> ModelEntry {
    ModelEntry {
        id: s("gpt-5-mini"),
        label: Some(s("GPT-5 mini")),
        vision: Vision::Yes,
        vision_source: Some(VisionSource::Keyword),
        reasoning: true,
        enabled: true,
        missing_upstream: false,
        deprecated: false,
        context_tokens: Some(400_000),
        caps: model_caps(),
        reasoning_effort: Some(ReasoningEffort::Minimal),
    }
}

fn quirks() -> Quirks {
    Quirks {
        thinking_off: ThinkingOff::OpenAiEffort,
        merge_system_into_user: false,
        strip_think_tags: true,
        max_tokens_field: Some(MaxTokensField::MaxCompletionTokens),
        dual_auth_header: None,
        gemini_key_in_query: false,
    }
}

fn provider() -> Provider {
    let mut headers = BTreeMap::new();
    headers.insert(s("x-org"), s("wisp"));
    Provider {
        id: s("openai-main"),
        name: s("OpenAI"),
        protocol: Protocol::OpenAiChat,
        base_url: s("https://api.openai.com/v1"),
        headers,
        quirks: quirks(),
        models: vec![model_entry(), ModelEntry::new("gpt-4.1-nano")],
        timeouts: Timeouts::default(),
        template_id: Some(s("openai")),
    }
}

fn model_ref() -> ModelRef {
    ModelRef {
        provider_id: s("openai-main"),
        model_id: s("gpt-5-mini"),
    }
}

fn usage() -> Usage {
    Usage {
        input_tokens: 412,
        output_tokens: 96,
        reasoning_tokens: Some(0),
        total_tokens: Some(508),
    }
}

fn llm_error() -> LlmError {
    LlmError {
        code: ErrorCode::RateLimited,
        subcode: None,
        message: s("Rate limit reached for gpt-5-mini"),
        retryable: true,
        hint: Some(s("errors.hint.retry_later")),
        status: Some(429),
        url: Some(s("api.openai.com/v1/chat/completions")),
        retry_after_s: Some(20),
        upstream_request_id: Some(s("req_8f2c1a")),
    }
}

fn probe_ok(url: &str, elapsed_ms: u32) -> ProbeResult {
    ProbeResult {
        ok: true,
        status: Some(200),
        elapsed_ms,
        url: s(url),
        error: None,
    }
}

fn style() -> Style {
    Style {
        id: s("natural"),
        name: String::new(),
        builtin: true,
        system_template: s("You are a professional translator. Translate {{source_lang}} into {{target_lang}}."),
        tail_instruction: s("Reply with the translation only."),
        temperature: Some(0.3),
        output: OutputFormat::PlainText,
        dictionary_mode: true,
        model_ref: None,
        hotkey: None,
        modified: false,
    }
}

fn custom_style() -> Style {
    Style {
        id: s("custom-3f9a1c2e"),
        name: s("Release notes"),
        builtin: false,
        system_template: s("Translate release notes from {{source_lang}} into {{target_lang}}."),
        tail_instruction: s("Reply with the translation only."),
        temperature: None,
        output: OutputFormat::MarkdownLite,
        dictionary_mode: false,
        model_ref: Some(model_ref()),
        hotkey: Some(s("Control+Alt+KeyR")),
        modified: false,
    }
}

fn source_app() -> SourceApp {
    SourceApp {
        id: s("com.apple.Safari"),
        name: s("Safari"),
        icon_url: Some(s("http://wisp.localhost/icon%2Fa1b2c3.png")),
    }
}

fn file_ref() -> FileRef {
    FileRef {
        path: s("/Users/me/Desktop/report.pdf"),
        name: s("report.pdf"),
        exists: true,
        size: Some(182_044),
    }
}

fn clip_meta() -> ClipItemMeta {
    ClipItemMeta {
        id: 128,
        kind: ClipKind::Text,
        created_at: 1_760_000_000_000,
        last_used_at: 1_760_000_360_000,
        use_count: 3,
        pinned: false,
        source: Some(source_app()),
        preview: s("The quick brown fox jumps over the lazy dog"),
        char_count: Some(43),
        line_count: Some(1),
        truncated: false,
        image: None,
        files: None,
        link: None,
        color: None,
    }
}

fn clip_meta_image() -> ClipItemMeta {
    ClipItemMeta {
        id: 129,
        kind: ClipKind::Image,
        created_at: 1_760_000_100_000,
        last_used_at: 1_760_000_100_000,
        use_count: 1,
        pinned: true,
        source: None,
        preview: String::new(),
        char_count: None,
        line_count: None,
        truncated: false,
        image: Some(ImageMeta {
            width: 1280,
            height: 720,
            bytes: 245_760,
            thumb_url: Some(s("http://wisp.localhost/thumb%2Fd4e5f6.jpg")),
        }),
        files: Some(vec![file_ref()]),
        link: Some(s("https://example.com/")),
        color: Some(s("#3478F6")),
    }
}

fn clip_page() -> ClipPage {
    ClipPage {
        items: vec![
            ClipHit {
                item: clip_meta(),
                highlights: vec![HighlightRange { start: 4, end: 9 }],
            },
            ClipHit {
                item: clip_meta_image(),
                highlights: vec![],
            },
        ],
        pinned_count: 1,
        next_offset: Some(50),
        query: Some(s("quick")),
        filter: ClipFilter::All,
    }
}

fn hotkey_target() -> HotkeyTarget {
    HotkeyTarget::Action {
        action: HotkeyAction::TranslateSelection,
    }
}

fn conflict() -> Conflict {
    let mut params = BTreeMap::new();
    params.insert(s("app"), s("Rectangle"));
    Conflict {
        level: ConflictLevel::Warning,
        source: ConflictSource::StaticApp,
        message_key: s("hotkeys.conflict.static_app"),
        params,
    }
}

fn hotkey_status() -> HotkeyStatus {
    HotkeyStatus {
        target: hotkey_target(),
        binding: Binding::default_binding(),
        effective: Some(s("Alt+KeyD")),
        display: Some(s("⌥D")),
        state: HotkeyState::Registered,
        reason_key: None,
    }
}

fn ext_triggers() -> ExtTriggerStatus {
    ExtTriggerStatus {
        bobber: ExtTriggerState::Off,
        double_tap: ExtTriggerState::WaitingPermission,
        mouse: ExtTriggerState::Off,
        reason_key: Some(s("hotkeys.ext.need_input_monitoring")),
    }
}

fn permission_set() -> PermissionSet {
    PermissionSet {
        accessibility: PermissionState::Granted,
        screen_recording: PermissionState::NotDetermined,
        input_monitoring: PermissionState::Denied,
        lost_after_update: false,
    }
}

fn update_status() -> UpdateStatus {
    UpdateStatus {
        state: UpdateState::Downloading,
        version: Some(s("0.1.1")),
        notes: Some(s("Bug fixes.")),
        progress: Some(0.5),
        error: None,
    }
}

fn runtime_info() -> RuntimeInfo {
    let mut has_key = BTreeMap::new();
    has_key.insert(s("openai-main"), true);
    RuntimeInfo {
        version: s("0.1.0"),
        platform: platform_info(),
        has_key,
        keychain: KeychainState::Available,
        hotkeys: vec![hotkey_status()],
        ext_triggers: ext_triggers(),
        permissions: permission_set(),
        clipboard_paused: false,
        update: UpdateStatus::default(),
        glossary_count: 0,
        settings_issue: Some(settings_issue()),
        orphan_keys: 1,
        autostart_issue: None,
    }
}

fn settings_issue() -> SettingsFileIssue {
    SettingsFileIssue {
        kind: SettingsIssueKind::ResetToDefault,
        file: s("settings.json"),
        backup_path: Some(s(
            "/Users/me/Library/Application Support/wisp/settings.json.bak-1760000000",
        )),
        at_ms: 1_760_000_000_000,
    }
}

fn long_text() -> LongTextInfo {
    LongTextInfo {
        chars: 5200,
        est_tokens: 1300,
        est_seconds: Some(12.5),
    }
}

fn translate_payload() -> TranslatePayload {
    TranslatePayload {
        source_text: s("The quick brown fox jumps over the lazy dog."),
        request_id: Some(s("r-1")),
        task: TaskKind::Translate,
        style_id: s("natural"),
        detected_lang: Some(Lang::En),
        target_lang: Lang::ZhCn,
        dict_mode: false,
        long_text: None,
        not_ready: None,
        selection_method: Some(s("ax")),
    }
}

fn ocr_payload() -> OcrPayload {
    OcrPayload {
        capture_id: s("c-7"),
        request_id: Some(s("r-2")),
        mode: OcrMode::Lines,
        thumb_url: s("http://wisp.localhost/crop%2Fc-7.png"),
        image_size: PhysSize {
            width: 840,
            height: 312,
        },
        target_lang: Lang::ZhCn,
        not_ready: None,
    }
}

fn clipboard_payload() -> ClipboardPayload {
    ClipboardPayload {
        page: clip_page(),
        paused: false,
        hint_bar: true,
        paste_on_double_click: false,
        unavailable: None,
        recording: ClipboardRecording::Recording,
    }
}

fn input_payload() -> InputPayload {
    InputPayload {
        initial_text: String::new(),
        target_lang: None,
        hint: Some(NotReady::NoProvider),
    }
}

fn toast_payload() -> ToastPayload {
    ToastPayload {
        reason: CopiedReason::FocusTimeout,
        paste_key: s("⌘V"),
    }
}

fn pin_state() -> PinState {
    PinState {
        kind: PinKind::Translate,
        source_text: s("The quick brown fox jumps over the lazy dog."),
        result_text: s("敏捷的棕色狐狸跳过了懒狗。"),
        task: TaskKind::Translate,
        style_id: s("natural"),
        target_lang: Lang::ZhCn,
        detected_lang: Some(Lang::En),
        request_id: None,
        capture_id: None,
        ocr_text: None,
        ocr_mode: None,
        show_source: true,
        status_line: Some(s("gpt-5-mini · 1.2 s")),
        dict_mode: false,
        model_ref: None,
        status: None,
        thumb_url: None,
        image_size: None,
        expand_width: None,
        on_top: None,
    }
}

fn pin_status() -> PinStatus {
    PinStatus {
        model_label: s("GPT-5 mini"),
        elapsed_ms: Some(1240),
        usage: Some(usage()),
        cached: false,
        cancelled: false,
        truncated: Some(TruncateReason::MaxTokens),
    }
}

fn pin_state_ocr() -> PinState {
    PinState {
        kind: PinKind::Ocr,
        source_text: s(""),
        result_text: s("敏捷的棕色狐狸跳过了懒狗。"),
        task: TaskKind::Translate,
        style_id: s("natural"),
        target_lang: Lang::ZhCn,
        detected_lang: Some(Lang::En),
        request_id: Some(s("r-9")),
        capture_id: Some(s("c-9")),
        ocr_text: Some(s("The quick brown fox jumps over the lazy dog.")),
        ocr_mode: Some(OcrMode::Translate),
        show_source: false,
        status_line: None,
        dict_mode: true,
        model_ref: Some(model_ref()),
        status: Some(PinStatus {
            elapsed_ms: None,
            usage: None,
            cached: true,
            truncated: None,
            ..pin_status()
        }),
        thumb_url: Some(s("http://wisp.localhost/capture%2F9%2Fthumb")),
        image_size: Some(PhysSize {
            width: 1280,
            height: 720,
        }),
        expand_width: Some(640),
        on_top: Some(false),
    }
}

fn ocr_history_item() -> OcrHistoryItem {
    OcrHistoryItem {
        id: 6,
        created_at: 1_760_000_100_000,
        mode: OcrMode::Lines,
        model: s("GPT-5 mini"),
        text: s("Hello"),
        thumb_url: None,
        image_url: None,
    }
}

fn appearance_state() -> AppearanceState {
    AppearanceState {
        theme: ResolvedTheme::Dark,
        accent: s("#3478F6"),
        reduce_motion: false,
        reduce_transparency: false,
        material: ResolvedMaterial::Native,
        font_size: 14,
        os: OsKind::Macos,
        ui_language: s("zh-CN"),
    }
}

fn hotkey_settings() -> HotkeySettings {
    let mut bindings = BTreeMap::new();
    bindings.insert(HotkeyAction::TranslateSelection, Binding(s("Alt+KeyD")));
    bindings.insert(HotkeyAction::Clipboard, Binding::none());
    HotkeySettings {
        bindings,
        bobber: BobberSettings { enabled: true },
        double_tap: DoubleTapSettings {
            enabled: true,
            key: Some(ModifierKey::RightAlt),
            interval_ms: 300,
        },
        mouse: MouseSettings::default(),
        app_keymaps: vec![app_keymap()],
        copy_wait_ms: 150,
    }
}

fn app_keymap() -> AppKeymap {
    AppKeymap {
        os: OsKind::Linux,
        app_id: s("gnome-terminal-server"),
        copy: Some(s("Control+Shift+KeyC")),
        paste: Some(s("Control+Shift+KeyV")),
    }
}

// ---------------------------------------------------------------------------
// 样例清单
// ---------------------------------------------------------------------------

fn all_samples() -> Samples {
    let mut x = Samples::default();

    // ===== geom =====
    x.add("Point", Point { x: 120.5, y: 48.0 });
    x.add(
        "Size",
        Size {
            width: 420.0,
            height: 168.0,
        },
    );
    x.add("Rect", rect(412.5, 300.0, 180.0, 18.0));
    x.add("PhysPoint", PhysPoint { x: -1920, y: 0 });
    x.add(
        "PhysSize",
        PhysSize {
            width: 3024,
            height: 1964,
        },
    );
    x.add("MonitorInfo", monitor());
    x.add("Anchor", anchor());

    // ===== app =====
    x.add("AppRef", app_ref());
    x.add("Capability", Capability::no("errors.platform.wayland"));
    x.add("Capabilities", capabilities());
    x.add("PlatformInfo", platform_info());

    // ===== llm =====
    x.add("Quirks", quirks());
    x.add("ModelCaps", model_caps());
    x.add("ModelEntry", model_entry());
    x.add(
        "Timeouts",
        Timeouts {
            connect_ms: 10_000,
            first_token_ms: 120_000,
            idle_ms: 120_000,
        },
    );
    x.add("Provider", provider());
    x.add("ModelRef", model_ref());
    x.add(
        "Defaults",
        Defaults {
            translate: Some(model_ref()),
            ocr: None,
        },
    );
    x.add("Usage", usage());
    x.add("LlmError", llm_error());
    x.add(
        "StreamEvent.start",
        StreamEvent::Start {
            request_id: s("r-1"),
            provider_id: s("openai-main"),
            model_id: s("gpt-5-mini"),
            model_label: s("GPT-5 mini"),
            upstream_id: Some(s("chatcmpl-9x")),
        },
    );
    x.add(
        "StreamEvent.meta",
        StreamEvent::Meta {
            detected_source_lang: Some(Lang::En),
            target_lang: Some(Lang::ZhCn),
            dict_mode: Some(false),
            low_confidence: Some(false),
        },
    );
    x.add(
        "StreamEvent.phase",
        StreamEvent::Phase {
            phase: StreamPhase::Reviewing,
        },
    );
    x.add(
        "StreamEvent.delta",
        StreamEvent::Delta {
            text: s("敏捷的棕色狐狸"),
        },
    );
    x.add(
        "StreamEvent.notice",
        StreamEvent::Notice {
            kind: NoticeKind::ParamDegraded,
        },
    );
    x.add(
        "StreamEvent.done",
        StreamEvent::Done {
            elapsed_ms: 1240,
            usage: Some(usage()),
            truncated: Some(TruncateReason::MaxTokens),
            cached: false,
            no_text: false,
        },
    );
    x.add("StreamEvent.error", StreamEvent::Error(llm_error()));
    x.add(
        "RegionVariant",
        RegionVariant {
            label_key: s("settings.providers.region.intl"),
            base_url: s("https://api.deepseek.com"),
        },
    );
    x.add(
        "ProviderTemplate",
        ProviderTemplate {
            id: s("openai"),
            name_key: s("settings.providers.template.openai"),
            group: TemplateGroup::Openai,
            provider: provider(),
            key_required: true,
            region_variants: vec![RegionVariant {
                label_key: s("settings.providers.region.default"),
                base_url: s("https://api.openai.com/v1"),
            }],
        },
    );
    x.add(
        "UrlPreview",
        UrlPreview {
            method: s("POST"),
            url: s("https://api.openai.com/v1/chat/completions"),
            version_appended: false,
            forced: false,
            warning: None,
        },
    );
    x.add(
        "ProbeResult",
        ProbeResult {
            ok: false,
            status: Some(401),
            elapsed_ms: 312,
            url: s("api.openai.com/v1/models"),
            error: Some(LlmError {
                hint: None,
                retryable: false,
                status: Some(401),
                retry_after_s: None,
                ..LlmError::new(ErrorCode::Auth, "Incorrect API key provided")
            }),
        },
    );
    x.add(
        "ProviderTestReport",
        ProviderTestReport {
            models: probe_ok("api.openai.com/v1/models", 280),
            stream: probe_ok("api.openai.com/v1/chat/completions", 640),
            overall: TestVerdict::Ok,
            model_used: Some(s("gpt-5-mini")),
            fetched_models: vec![model_entry()],
            notice: None,
        },
    );

    // ===== style =====
    x.add("Style", custom_style());
    x.add(
        "GlossaryEntry",
        GlossaryEntry {
            source: s("Wisp"),
            target: s("Wisp"),
            note: Some(s("产品名，不译")),
        },
    );
    x.add(
        "PromptPreview",
        PromptPreview {
            system: s("You are a professional translator."),
            user: s("<source_text>\nhi\n</source_text>"),
        },
    );

    // ===== clip =====
    x.add("SourceApp", source_app());
    x.add(
        "ImageMeta",
        ImageMeta {
            width: 1280,
            height: 720,
            bytes: 245_760,
            thumb_url: None,
        },
    );
    x.add("FileRef", file_ref());
    x.add("ClipItemMeta", clip_meta_image());
    x.add("HighlightRange", HighlightRange { start: 4, end: 9 });
    x.add(
        "ClipHit",
        ClipHit {
            item: clip_meta(),
            highlights: vec![HighlightRange { start: 4, end: 9 }],
        },
    );
    x.add("ClipPage", clip_page());
    x.add(
        "ClipItemDetail",
        ClipItemDetail {
            meta: clip_meta(),
            text: Some(s("The quick brown fox jumps over the lazy dog")),
            has_html: true,
            has_rtf: false,
            image_url: None,
            files: None,
        },
    );
    x.add("PasteOutcome.pasted", PasteOutcome::Pasted);
    x.add(
        "PasteOutcome.copied",
        PasteOutcome::Copied {
            reason: CopiedReason::Elevated,
        },
    );
    x.add(
        "ClipboardChanged.inserted",
        ClipboardChanged::Inserted { item: clip_meta() },
    );
    x.add(
        "ClipboardChanged.touched",
        ClipboardChanged::Touched {
            item: clip_meta_image(),
        },
    );
    x.add(
        "ClipboardChanged.updated",
        ClipboardChanged::Updated {
            item: clip_meta_image(),
        },
    );
    x.add("ClipboardChanged.removed", ClipboardChanged::Removed { id: 128 });
    x.add(
        "ClipboardChanged.cleared",
        ClipboardChanged::Cleared { keep_pinned: true },
    );
    x.add("ClipboardChanged.paused", ClipboardChanged::Paused { paused: true });
    x.add("ClipboardChanged.secret_skipped", ClipboardChanged::SecretSkipped);
    x.add(
        "OcrHistoryItem",
        OcrHistoryItem {
            id: 5,
            created_at: 1_760_000_000_000,
            mode: OcrMode::Markdown,
            model: s("gpt-5-mini"),
            text: s("# Title\n\nBody"),
            thumb_url: Some(s("http://wisp.localhost/ocrimg%2F5.jpg")),
            image_url: Some(s("http://wisp.localhost/ocrimg%2F5.png")),
        },
    );
    x.add(
        "OcrHistoryChanged.inserted",
        OcrHistoryChanged::Inserted {
            item: ocr_history_item(),
        },
    );
    x.add(
        "OcrHistoryChanged.removed",
        OcrHistoryChanged::Removed { ids: vec![3, 4] },
    );
    x.add("OcrHistoryChanged.cleared", OcrHistoryChanged::Cleared);

    // ===== hotkey =====
    x.add("HotkeyTarget.action", hotkey_target());
    x.add(
        "HotkeyTarget.style",
        HotkeyTarget::Style {
            style_id: s("technical"),
        },
    );
    x.add("Conflict", conflict());
    x.add(
        "ConflictReport",
        ConflictReport {
            combo: s("Control+Alt+KeyD"),
            display: s("⌃⌥D"),
            conflicts: vec![conflict()],
            acceptable: true,
        },
    );
    x.add("HotkeyStatus", hotkey_status());
    x.add("ExtTriggerStatus", ext_triggers());
    x.add(
        "NeighborHit",
        NeighborHit {
            app_name: s("Bob"),
            combo: s("Alt+KeyD"),
            display: s("⌥D"),
            action: HotkeyAction::TranslateSelection,
        },
    );
    x.add(
        "KeyboardAdvice",
        KeyboardAdvice {
            has_altgr: true,
            multi_input_lang: false,
            recommend_family: Some(s("alt_shift")),
            qq_running: false,
        },
    );

    // ===== perm =====
    x.add("PermissionSet", permission_set());

    // ===== ipc：面板 =====
    x.add(
        "PanelPrepare",
        PanelPrepare {
            session_id: 7,
            view: PanelView::Translate,
            payload: PanelPayload::Translate(translate_payload()),
            width: 420.0,
            max_height: 569.4,
            placement: Placement::Below,
            origin_x: 24.0,
            focused: false,
            transition: PanelTransition::Enter,
        },
    );
    x.add("PanelPayload.translate", PanelPayload::Translate(translate_payload()));
    x.add("PanelPayload.ocr", PanelPayload::Ocr(ocr_payload()));
    x.add("PanelPayload.clipboard", PanelPayload::Clipboard(clipboard_payload()));
    x.add("PanelPayload.input", PanelPayload::Input(input_payload()));
    x.add("PanelPayload.toast", PanelPayload::Toast(toast_payload()));
    x.add("ToastPayload", toast_payload());
    x.add(
        "TranslatePayload",
        TranslatePayload {
            request_id: None,
            long_text: Some(long_text()),
            not_ready: Some(NotReady::NoDefaultModel {
                task: TaskKind::Translate,
            }),
            selection_method: None,
            ..translate_payload()
        },
    );
    x.add("LongTextInfo", long_text());
    x.add("NotReady.no_provider", NotReady::NoProvider);
    x.add(
        "NotReady.no_default_model",
        NotReady::NoDefaultModel { task: TaskKind::Polish },
    );
    x.add("NotReady.no_vision_model", NotReady::NoVisionModel);
    x.add(
        "NotReady.permission_missing",
        NotReady::PermissionMissing {
            permission: PermissionKind::ScreenRecording,
        },
    );
    x.add("NotReady.wayland_copy_first", NotReady::WaylandCopyFirst);
    x.add(
        "NotReady.capture_failed",
        NotReady::CaptureFailed {
            message: s("errors.capture.backend"),
        },
    );
    x.add("NotReady.elevated", NotReady::Elevated);
    x.add("NotReady.keychain_unavailable", NotReady::KeychainUnavailable);
    x.add(
        "NotReady.selection_unavailable",
        NotReady::SelectionUnavailable {
            reason_key: s("errors.platform.clipboard_too_large"),
        },
    );
    x.add("OcrPayload", ocr_payload());
    x.add("ClipboardPayload", clipboard_payload());
    x.add("InputPayload", input_payload());
    x.add(
        "PanelShown",
        PanelShown {
            session_id: 7,
            focused: false,
        },
    );
    x.add(
        "PanelHideRequest",
        PanelHideRequest {
            session_id: 7,
            reason: HideReason::ClickOutside,
        },
    );
    x.add(
        "PanelFocusChanged",
        PanelFocusChanged {
            session_id: 7,
            focused: true,
        },
    );
    x.add(
        "LatencyMarks",
        LatencyMarks {
            session_id: 7,
            origin: s("hotkey"),
            method: Some(s("ax")),
            selection_ms: Some(18),
            prepare_ms: 22,
            ready_ms: 31,
            shown_ms: 48,
            first_token_ms: Some(420),
            ready_timeout: false,
        },
    );
    x.add(
        "TranslateReq",
        TranslateReq {
            session_id: 7,
            text: s("The quick brown fox"),
            task: TaskKind::Refine,
            style_id: s("natural"),
            target_lang: Some(Lang::ZhCn),
            model_ref: Some(model_ref()),
            dict_mode: DictMode::Auto,
            bypass_cache: true,
            draft: Some(s("敏捷的棕色狐狸")),
            confirm_long: false,
        },
    );
    x.add(
        "OcrReq",
        OcrReq {
            session_id: 9,
            capture_id: s("c-7"),
            mode: OcrMode::Translate,
            model_ref: None,
            target_lang: Some(Lang::ZhCn),
        },
    );
    x.add(
        "StartAck",
        StartAck {
            request_id: s("r-3"),
            width: Some(480.0),
            long_text: None,
        },
    );
    x.add(
        "StartAck.long_text",
        StartAck {
            request_id: s(""),
            width: Some(480.0),
            long_text: Some(long_text()),
        },
    );
    x.add("PinState", pin_state());
    // 全部可缺省字段都填上的 OCR 固定状态（固定 TS 侧可缺省键的形状）
    x.add("PinState.ocr", pin_state_ocr());
    x.add("PinStatus", pin_status());
    x.add(
        "WindowTarget.settings",
        WindowTarget::Settings {
            section: Some(s("providers")),
            anchor: None,
        },
    );
    // 带锚点的深链
    x.add(
        "WindowTarget.settings_anchor",
        WindowTarget::Settings {
            section: Some(s("ocr")),
            anchor: Some(s("ocr_history")),
        },
    );
    x.add("WindowTarget.onboarding", WindowTarget::Onboarding { step: Some(2) });

    // ===== ipc：Overlay =====
    x.add(
        "OverlayInit",
        OverlayInit {
            session_id: 3,
            monitor: monitor(),
            image_url: s("http://wisp.localhost/capture%2F3%2F1%2Ffull"),
            image_size: PhysSize {
                width: 3024,
                height: 1964,
            },
            windows: vec![rect(100.0, 80.0, 900.0, 600.0), rect(0.0, 0.0, 1512.0, 33.0)],
            mode: OverlayMode::OcrTranslate,
            window_detect: true,
            fly_animation: true,
            reduced_motion: false,
            frozen_native: true,
            half_url: None,
            region_url: Some(s("capture/3/31/region")),
        },
    );
    x.add(
        "OverlaySessionStart",
        OverlaySessionStart {
            session_id: 4,
            monitor_id: s("\\\\.\\DISPLAY1"),
        },
    );
    x.add(
        "OverlaySessionEnd",
        OverlaySessionEnd {
            session_id: 4,
            reason: OverlayEndReason::Retrigger,
        },
    );
    x.add(
        "OverlayCommit",
        OverlayCommit {
            session_id: 3,
            monitor_id: s("1"),
            rect: rect(220.0, 140.0, 420.0, 156.0),
            kind: OverlaySelectionKind::Drag,
        },
    );
    x.add(
        "OverlayCommitAck",
        OverlayCommitAck {
            fly_to: Some(rect(220.0, 308.0, 440.0, 220.0)),
        },
    );

    // ===== ipc：应用 =====
    x.add(
        "SettingsSnapshot",
        SettingsSnapshot {
            settings: Settings::default(),
            styles: vec![style(), custom_style()],
            runtime: runtime_info(),
        },
    );
    x.add("RuntimeInfo", runtime_info());
    x.add(
        "SettingsFileIssue",
        SettingsFileIssue {
            kind: SettingsIssueKind::InvalidJson,
            file: s("styles.json"),
            backup_path: None,
            at_ms: 1_760_000_123_456,
        },
    );
    x.add(
        "SettingsFileIssue.newer_version",
        SettingsFileIssue {
            kind: SettingsIssueKind::NewerVersion,
            file: s("settings.json"),
            backup_path: None,
            at_ms: 1_760_000_234_567,
        },
    );
    x.add(
        "Boot",
        Boot {
            label: s("settings"),
            os: OsKind::Windows,
            os_version: s("10.0.26100"),
            theme: ResolvedTheme::Dark,
            accent: s("#0A84FF"),
            reduce_motion: false,
            reduce_transparency: false,
            material: ResolvedMaterial::Native,
            frame_mode: FrameMode::Css,
            shadow_margin: 24.0,
            font_size: 14,
            ui_language: s("zh-CN"),
            dev: true,
            debug_build: true,
            route: Some(WindowTarget::Settings {
                section: Some(s("providers")),
                anchor: None,
            }),
            prepare: None,
        },
    );
    // 面板冷唤出：暂存的 prepare 随 Boot 送进新页面
    x.add(
        "Boot.panel_cold",
        Boot {
            label: s("panel"),
            os: OsKind::Macos,
            os_version: s("26.4"),
            theme: ResolvedTheme::Light,
            accent: s(""),
            reduce_motion: false,
            reduce_transparency: false,
            material: ResolvedMaterial::Native,
            frame_mode: FrameMode::Native,
            shadow_margin: 0.0,
            font_size: 14,
            ui_language: s("en-US"),
            dev: false,
            debug_build: false,
            route: None,
            prepare: Some(PanelPrepare {
                session_id: 12,
                view: PanelView::Input,
                payload: PanelPayload::Input(input_payload()),
                width: 420.0,
                max_height: 569.4,
                placement: Placement::Center,
                origin_x: 210.0,
                focused: true,
                transition: PanelTransition::Enter,
            }),
        },
    );
    x.add("AppearanceState", appearance_state());
    x.add(
        "AppMeta",
        AppMeta {
            version: s("0.1.0"),
            platform: platform_info(),
            config_dir: s("/Users/me/Library/Application Support/wisp"),
            data_dir: s("/Users/me/Library/Application Support/wisp"),
            log_dir: s("/Users/me/Library/Logs/wisp"),
            dev_mode: false,
        },
    );
    x.add("UpdateStatus", update_status());
    x.add_as(
        "OnboardingHotkey",
        "T.EventMap['onboarding:hotkey']",
        OnboardingHotkey {
            action: HotkeyAction::Ocr,
        },
    );
    x.add_as(
        "HotkeyTestReceived",
        "T.EventMap['hotkey:test_received']",
        HotkeyTestReceived {
            target: HotkeyTarget::Style { style_id: s("casual") },
        },
    );

    // ===== settings=====
    // Settings.json = 首次启动写出的默认内容
    x.add("Settings", Settings::default());
    x.add_as(
        "General",
        "T.Settings['general']",
        General {
            launch_at_login: false,
            start_hidden: true,
            ui_language: UiLanguage::EnUs,
            update_policy: UpdatePolicy::AutoInstall,
            developer_mode: true,
        },
    );
    x.add_as("HotkeySettings", "T.Settings['hotkeys']", hotkey_settings());
    x.add_as(
        "BobberSettings",
        "T.Settings['hotkeys']['bobber']",
        BobberSettings { enabled: true },
    );
    x.add_as(
        "DoubleTapSettings",
        "T.Settings['hotkeys']['double_tap']",
        DoubleTapSettings {
            enabled: true,
            key: Some(ModifierKey::RightCtrl),
            interval_ms: 200,
        },
    );
    x.add_as(
        "MouseSettings",
        "T.Settings['hotkeys']['mouse']",
        MouseSettings {
            enabled: true,
            long_press: true,
            ..MouseSettings::default()
        },
    );
    x.add_as(
        "MouseBinding",
        "T.Settings['hotkeys']['mouse']['middle']",
        MouseBinding {
            enabled: true,
            action: Some(HotkeyAction::TranslateSelection),
        },
    );
    x.add_as(
        "AppKeymap",
        "T.Settings['hotkeys']['app_keymaps'][number]",
        app_keymap(),
    );
    x.add_as(
        "TranslateSettings",
        "T.Settings['translate']",
        TranslateSettings {
            primary_lang: Lang::En,
            secondary_lang: Lang::Ja,
            default_style: s("technical"),
            anchor: TranslateAnchor::Cursor,
            input_anchor: InputAnchor::Center,
            ..TranslateSettings::default()
        },
    );
    x.add_as(
        "OcrSettings",
        "T.Settings['ocr']",
        OcrSettings {
            default_mode: OcrMode::Paragraphs,
            image_detail: ImageDetail::Auto,
            history: true,
            ..OcrSettings::default()
        },
    );
    x.add_as(
        "ClipboardSettings",
        "T.Settings['clipboard']",
        ClipboardSettings {
            max_age_days: Some(30),
            excluded_apps: vec![excluded_app()],
            panel_position: ClipPanelPosition::Remember,
            paste_on: PasteOn::DoubleClick,
            ..ClipboardSettings::default()
        },
    );
    x.add_as(
        "ExcludedApp",
        "T.Settings['clipboard']['excluded_apps'][number]",
        excluded_app(),
    );
    x.add_as(
        "Appearance",
        "T.Settings['appearance']",
        Appearance {
            theme: ThemePref::Dark,
            accent: s("#FF6B35"),
            material: MaterialPref::Solid,
            reduce_motion: ReduceMotionPref::On,
            max_height_ratio: 0.75,
            ..Appearance::default()
        },
    );
    x.add_as(
        "Advanced",
        "T.Settings['advanced']",
        Advanced {
            log_level: LogLevel::Debug,
            webview_low_memory_when_hidden: false,
            unload_panel_after_idle_min: Some(60),
        },
    );

    x
}

fn excluded_app() -> ExcludedApp {
    ExcludedApp {
        os: OsKind::Macos,
        app_id: s("com.bitwarden.desktop"),
        name: s("Bitwarden"),
    }
}

// ---------------------------------------------------------------------------
// 文件比对 / 生成
// ---------------------------------------------------------------------------

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/desktop/src/shared/ipc/fixtures")
}

fn update_mode() -> bool {
    std::env::var("UPDATE_GOLDEN").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// 生成 index.ts：每个样例导出一个 `satisfies` 常量
fn render_index(samples: &Samples) -> String {
    let mut out = String::new();
    out.push_str("// 本文件由 crates/wisp-types/tests/golden.rs 生成，请勿手改。\n");
    out.push_str("// 重新生成：UPDATE_GOLDEN=1 cargo test -p wisp-types --test golden\n");
    out.push_str("// 每个常量都用 `satisfies` 获得上下文类型，字面量不会被拓宽。\n");
    out.push_str("import type * as T from '../types'\n");
    for sample in &samples.0 {
        let ident = sample.name.replace('.', "__");
        out.push('\n');
        out.push_str(&format!(
            "export const {ident} = {} satisfies {}\n",
            sample.json, sample.ts_type
        ));
    }
    out
}

/// 检查或重写一个文件；返回不一致的描述
fn sync_file(path: &Path, expected: &str, update: bool) -> Option<String> {
    let actual = std::fs::read_to_string(path).ok();
    if actual.as_deref() == Some(expected) {
        return None;
    }
    if update {
        std::fs::write(path, expected).unwrap_or_else(|e| panic!("写入 {} 失败：{e}", path.display()));
        return None;
    }
    Some(match actual {
        None => format!("缺失：{}", path.display()),
        Some(_) => format!("不一致：{}", path.display()),
    })
}

#[test]
fn golden_fixtures() {
    let samples = all_samples();
    let dir = fixtures_dir();
    let update = update_mode();
    if update {
        std::fs::create_dir_all(&dir).expect("创建 fixtures 目录失败");
    }

    let mut problems = Vec::new();
    let mut expected_files = BTreeSet::new();
    for sample in &samples.0 {
        let file = format!("{}.json", sample.name);
        problems.extend(sync_file(&dir.join(&file), &format!("{}\n", sample.json), update));
        expected_files.insert(file);
    }
    problems.extend(sync_file(&dir.join("index.ts"), &render_index(&samples), update));

    // 不再存在的旧样例
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".json") && !expected_files.contains(&name) {
                if update {
                    std::fs::remove_file(entry.path()).expect("删除旧样例失败");
                } else {
                    problems.push(format!("多余：{}", entry.path().display()));
                }
            }
        }
    }

    assert!(
        problems.is_empty(),
        "黄金样例与 Rust 类型不一致（确认改动符合契约后运行 UPDATE_GOLDEN=1 cargo test -p wisp-types --test golden）：\n{}",
        problems.join("\n")
    );
}

// ---------------------------------------------------------------------------
// 只在 Rust 侧存在的类型：往返与语义测试
// ---------------------------------------------------------------------------

#[test]
fn rust_only_types_roundtrip() {
    roundtrip(&PhysRect {
        x: 10,
        y: 20,
        width: 300,
        height: 200,
    });
    roundtrip(&Binding(s("Shift+Super+KeyV")));
    roundtrip(&RawClipboard {
        change_id: 42,
        source: Some(app_ref()),
        concealed: false,
        from_wisp: false,
        text: Some(s("hello")),
        text_truncated: false,
        html: Some(s("<b>hello</b>")),
        rtf: None,
        image_png: None,
        image_size: Some(PhysSize { width: 2, height: 2 }),
        files: Some(vec![s("/tmp/a.txt")]),
    });
    roundtrip(&ClipPayload::text("hello"));
    roundtrip(&GlossaryFile {
        schema_version: 1,
        entries: vec![GlossaryEntry {
            source: s("A"),
            target: s("甲"),
            note: None,
        }],
    });

    let mut overrides = BTreeMap::new();
    overrides.insert(
        s("natural"),
        StyleOverride {
            temperature: Some(None),
            hotkey: Some(Some(s("Alt+KeyN"))),
            model_ref: Some(None),
            ..StyleOverride::default()
        },
    );
    roundtrip(&StylesFile {
        schema_version: 1,
        order: vec![s("technical"), s("natural")],
        overrides,
        custom: vec![custom_style()],
    });
}

#[test]
fn style_override_distinguishes_absent_and_null() {
    let ov: StyleOverride =
        serde_json::from_str(r#"{ "temperature": null, "output": "markdown_lite" }"#).expect("解析失败");
    assert_eq!(ov.temperature, Some(None), "显式 null 表示覆写为空");
    assert_eq!(ov.model_ref, None, "缺省表示不覆写");
    assert_eq!(ov.output, Some(OutputFormat::MarkdownLite));
    let json = serde_json::to_value(&ov).expect("序列化失败");
    assert_eq!(
        json,
        serde_json::json!({ "temperature": null, "output": "markdown_lite" })
    );
    assert!(StyleOverride::default().is_empty());
}

#[test]
fn settings_tolerate_missing_fields() {
    let empty: Settings = serde_json::from_str("{}").expect("解析失败");
    assert_eq!(empty, Settings::default());

    let partial: Settings = serde_json::from_str(
        r#"{ "general": { "developer_mode": true }, "hotkeys": { "mouse": { "enabled": true } } }"#,
    )
    .expect("解析失败");
    assert!(partial.general.developer_mode);
    assert!(partial.general.launch_at_login, "未出现的字段取默认值");
    assert!(partial.hotkeys.mouse.enabled);
    assert_eq!(partial.hotkeys.mouse.middle, MouseSettings::default().middle);
    assert_eq!(partial.hotkeys.binding(HotkeyAction::Ocr), Binding::default_binding());
}

/// 下面的默认内容示例（excluded_apps 以预置名单代替占位）必须与 `Settings::default()` 一致
#[test]
fn settings_default_matches_contract_example() {
    let mut example = serde_json::json!({ "schema_version": 1,
      "general": { "launch_at_login": true, "start_hidden": true, "ui_language": "system", "update_policy": "notify", "developer_mode": false },
      "hotkeys": { "bindings": {}, "bobber": { "enabled": false }, "double_tap": { "enabled": false, "key": null, "interval_ms": 300 },
        "mouse": { "enabled": false, "long_press": false, "middle": { "enabled": true, "action": "translate_selection" }, "back": { "enabled": false, "action": "clipboard" }, "forward": { "enabled": false, "action": "ocr" } },
        "app_keymaps": [], "copy_wait_ms": 150 },
      "providers": [], "defaults": { "translate": null, "ocr": null },
      "translate": { "primary_lang": "zh-CN", "secondary_lang": "en", "default_style": "natural", "long_text_threshold": 3000, "close_on_click_outside": true, "close_on_mouse_leave": false, "mouse_leave_px": 160, "anchor": "selection", "input_anchor": "cursor", "cache_enabled": true, "show_source_by_default": false },
      "ocr": { "default_mode": "lines", "max_edge_px": 2048, "auto_copy": true, "history": false, "history_limit": 50, "window_detect": true, "fly_animation": true, "direct_translate_transcribe": false, "image_detail": "high" },
      "clipboard": { "enabled": true, "record_text": true, "record_rich_text": true, "record_images": true, "record_files": true, "max_items": 1000, "max_age_days": null, "max_text_bytes": 1048576, "max_image_bytes": 20971520, "excluded_apps": [], "secret_detection": true, "secret_tray_flash": true, "panel_position": "center", "paste_on": "single_click", "show_hint_bar": true },
      "appearance": { "theme": "system", "accent": "system", "font_size": 14, "translate_min_width": 360, "translate_max_width": 520, "clipboard_width": 420, "max_height_ratio": 0.6, "material": "system", "reduce_motion": "system" },
      "advanced": { "log_level": "info", "webview_low_memory_when_hidden": true, "unload_panel_after_idle_min": null } });
    example["clipboard"]["excluded_apps"] =
        serde_json::to_value(wisp_types::defaults::default_excluded_apps()).expect("序列化失败");
    // 注意：f32 经 `serde_json::to_value` 会被拓宽成 0.6000000238418579；落盘与 IPC 都走 `to_string`（输出 0.6），
    // 所以这里先序列化成字符串再解析，与实际产物一致
    let text = serde_json::to_string(&Settings::default()).expect("序列化失败");
    let actual: serde_json::Value = serde_json::from_str(&text).expect("解析失败");
    assert_eq!(actual, example);
}

#[test]
fn stream_event_error_is_flat() {
    let v =
        serde_json::to_value(StreamEvent::Error(LlmError::new(ErrorCode::Cancelled, "cancelled"))).expect("序列化失败");
    assert_eq!(v["type"], "error");
    assert_eq!(v["code"], "cancelled");
    assert!(
        v.get("subcode").is_some_and(serde_json::Value::is_null),
        "Option 字段序列化为 null"
    );
}

/// Rust 的事件名常量与前端 `events.ts` 的 `EVENT_NAMES` 一一对应（同序）
#[test]
fn event_names_match_frontend() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/desktop/src/shared/ipc/events.ts");
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("读取 {} 失败：{e}", path.display()));
    let start = src.find("EVENT_NAMES = [").expect("events.ts 中找不到 EVENT_NAMES");
    let body = &src[start..];
    let end = body.find(']').expect("EVENT_NAMES 未闭合");
    let ts: Vec<&str> = body[..end].split('\'').skip(1).step_by(2).collect();
    assert_eq!(
        ts,
        wisp_types::event::ALL.to_vec(),
        "events.ts 与 wisp_types::event::ALL 不一致"
    );
}

/// 手改配置时漏写非身份字段不应让整个文件失败
#[test]
fn nested_entries_tolerate_missing_fields() {
    let m: ModelEntry = serde_json::from_str(r#"{ "id": "gpt-4.1" }"#).expect("ModelEntry 解析失败");
    assert_eq!(m.vision, Vision::Unknown);
    assert!(m.enabled);
    assert_eq!(m.reasoning_effort, None);
    // 推理档位写错（手改 settings.json）只让这个字段退回缺省，不丢弃整个模型条目
    let m: ModelEntry =
        serde_json::from_str(r#"{ "id": "o4-mini", "reasoning_effort": "Lowest" }"#).expect("ModelEntry 解析失败");
    assert_eq!(m.reasoning_effort, None);
    let m: ModelEntry =
        serde_json::from_str(r#"{ "id": "o4-mini", "reasoning_effort": "none" }"#).expect("ModelEntry 解析失败");
    assert_eq!(m.reasoning_effort, Some(ReasoningEffort::None));
    let p: Provider = serde_json::from_str(r#"{ "id": "p1", "protocol": "openai_chat", "base_url": "https://x" }"#)
        .expect("Provider 解析失败");
    assert_eq!(p.name, "");
    let e: ExcludedApp =
        serde_json::from_str(r#"{ "os": "macos", "app_id": "com.example" }"#).expect("ExcludedApp 解析失败");
    assert_eq!(e.name, "");
    let st: Style =
        serde_json::from_str(r#"{ "id": "custom-1a2b3c4d", "system_template": "x" }"#).expect("Style 解析失败");
    assert!(!st.builtin && !st.modified);
    assert_eq!(st.output, OutputFormat::PlainText);
    // 缺失字段取自定义风格的默认值：词典式开、temperature 0.3
    assert!(st.dictionary_mode, "dictionary_mode 缺失时应为 true");
    assert_eq!(st.temperature, Some(0.3), "temperature 缺失时应为 0.3");
    assert_eq!(st.name, "");
    assert_eq!((st.model_ref.as_ref(), st.hotkey.as_ref()), (None, None));
    assert_eq!(
        Style {
            system_template: "x".to_owned(),
            ..Style::custom("custom-1a2b3c4d", "")
        },
        st,
        "缺失字段的取值与 Style::custom 一致"
    );
    // 显式 null 才表示「不发 temperature」；显式 false 照常生效
    let st: Style = serde_json::from_str(r#"{ "id": "custom-1", "temperature": null, "dictionary_mode": false }"#)
        .expect("Style 解析失败");
    assert_eq!(st.temperature, None, "显式 null = 不发送");
    assert!(!st.dictionary_mode);
    let f: StylesFile = serde_json::from_str(r#"{ "custom": [{ "id": "custom-1" }] }"#).expect("StylesFile 解析失败");
    assert_eq!(f.custom.len(), 1);
}

/// `ClipboardPayload` 的新字段默认值不出现在 JSON 里（旧前端与 fixtures 不受影响），非默认值的形状固定
#[test]
fn clipboard_payload_optional_fields() {
    let mut p = clipboard_payload();
    let plain = serde_json::to_value(&p).expect("json");
    assert!(plain.get("unavailable").is_none() && plain.get("recording").is_none());
    p.unavailable = Some("clipboard.history_too_new".to_owned());
    p.recording = ClipboardRecording::Unavailable {
        reason_key: "errors.platform.x11_missing_xfixes".to_owned(),
    };
    let v = serde_json::to_value(&p).expect("json");
    assert_eq!(v["unavailable"], "clipboard.history_too_new");
    assert_eq!(
        v["recording"],
        serde_json::json!({"state": "unavailable", "reason_key": "errors.platform.x11_missing_xfixes"})
    );
    assert_eq!(roundtrip_value(&p), p);
    for (r, state) in [
        (ClipboardRecording::Paused, "paused"),
        (ClipboardRecording::Disabled, "disabled"),
    ] {
        assert_eq!(
            serde_json::to_value(&r).expect("json"),
            serde_json::json!({ "state": state })
        );
    }
    // 旧载荷（没有这两个字段）照常反序列化为默认值
    let back: ClipboardPayload = serde_json::from_value(plain).expect("old payload");
    assert_eq!(back.recording, ClipboardRecording::Recording);
    assert_eq!(back.unavailable, None);
}

/// 合并安全的新字段——默认值不出现在 JSON 里（旧样例 / 旧前端不受影响），旧载荷照常反序列化
#[test]
fn optional_fields_are_omitted_by_default() {
    // PinState：7 个新字段缺省时都不写出；旧载荷（没有这些键）反序列化为默认值
    let plain = serde_json::to_value(pin_state()).expect("json");
    for key in [
        "dict_mode",
        "model_ref",
        "status",
        "thumb_url",
        "image_size",
        "expand_width",
        "on_top",
    ] {
        assert!(plain.get(key).is_none(), "PinState.{key} 的默认值不应写出");
    }
    let back: PinState = serde_json::from_value(plain).expect("old payload");
    assert_eq!(back, pin_state());
    let full = pin_state_ocr();
    let v = serde_json::to_value(&full).expect("json");
    assert_eq!(v["dict_mode"], true);
    assert_eq!(v["expand_width"], 640);
    assert_eq!(v["on_top"], false, "Some(false) 照常写出（与缺省的 null 区分）");
    assert_eq!(v["status"]["cached"], true);
    assert_eq!(roundtrip_value(&full), full);

    // WindowTarget：没有锚点时不写出 anchor 键（旧前端发来的 `{kind, section}` 照常解析）
    let t = WindowTarget::settings(Some("providers"));
    assert_eq!(
        serde_json::to_value(&t).expect("json"),
        serde_json::json!({ "kind": "settings", "section": "providers" })
    );
    let old: WindowTarget =
        serde_json::from_value(serde_json::json!({ "kind": "settings", "section": null })).expect("old");
    assert_eq!(old, WindowTarget::settings(None));

    // ActionOrigin 的新取值
    assert_eq!(serde_json::to_value(ActionOrigin::Panel).expect("json"), "panel");
    assert_eq!(
        serde_json::to_value(ActionOrigin::OcrHistory).expect("json"),
        "ocr_history"
    );

    // OverlayEndReason 的全部取值
    for (r, name) in [
        (OverlayEndReason::Done, "done"),
        (OverlayEndReason::Cancelled, "cancelled"),
        (OverlayEndReason::Retrigger, "retrigger"),
        (OverlayEndReason::Failed, "failed"),
    ] {
        assert_eq!(serde_json::to_value(r).expect("json"), name);
    }
}

/// 新配置的平台默认值经 `Settings::for_os` 进入；`default()` 保持平台无关
#[test]
fn settings_for_os_applies_platform_defaults_only() {
    for os in [OsKind::Macos, OsKind::Windows, OsKind::Linux] {
        let s = Settings::for_os(os);
        assert_eq!(
            s.advanced.unload_panel_after_idle_min,
            wisp_types::defaults::unload_panel_default(os)
        );
        let mut rest = s.clone();
        rest.advanced.unload_panel_after_idle_min = None;
        assert_eq!(rest, Settings::default(), "{os:?}：除平台默认值外与 default() 一致");
    }
    assert_eq!(Settings::default().advanced.unload_panel_after_idle_min, None);
    let (lo, hi) = wisp_types::defaults::ranges::UNLOAD_PANEL_AFTER_IDLE_MIN;
    assert!((lo..=hi).contains(&wisp_types::defaults::UNLOAD_PANEL_SUGGESTED_MIN));
}

fn roundtrip_value<V: Serialize + DeserializeOwned>(v: &V) -> V {
    serde_json::from_value(serde_json::to_value(v).expect("json")).expect("back")
}
