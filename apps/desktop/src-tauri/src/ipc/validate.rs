//! 命令参数里 DTO 的组合校验。
//!
//! 只做形状校验：字符串长度（UTF-8 字节）、集合条数、浮点数有限性、id 不含控制字符。「是否存在」「取值是否在业务
//! 范围内」（例如 temperature 0–1、font_size 12–18）属于服务的职责，不在这里判断。全部失败都返回 `invalid_argument`，
//! 错误信息只含字段路径与上限，**不含值本身**（参数里可能有凭据或用户文本）。
//!
//! wisp-types 的 DTO 新增字段时，同步在这里补上对应的校验，并补测试。

use wisp_types::{
    AppKeymap, ExcludedApp, GlossaryEntry, HotkeyTarget, ModelEntry, ModelRef, OcrReq, OverlayCommit, PinState,
    Provider, Rect, Style, SyncAccount, TranslateReq, WindowTarget,
};

use super::{MAX_KEY_BYTES, MAX_LIST_LEN, MAX_QUERY_BYTES, MAX_TEXT_BYTES, check_id, check_len, check_list_len};
use crate::error::IpcError;

/// 名称、标签、状态行、术语、快捷键字符串等短文本的上限（同查询，1 KB）
pub const MAX_LABEL_BYTES: usize = MAX_QUERY_BYTES;
/// 风格模板正文（system_template / tail_instruction）的上限（64 KB；内置模板最长约 2 KB）
pub const MAX_TEMPLATE_BYTES: usize = 64 * 1024;
/// Provider 自定义 header 的条数上限
pub const MAX_HEADERS: usize = 64;
/// 单个模型声明的 Anthropic effort 档位条数上限
pub const MAX_EFFORTS: usize = 16;
/// `settings_patch` 的补丁序列化后的上限（1 MB；完整的 settings.json 通常只有几 KB）
pub const MAX_PATCH_BYTES: usize = MAX_TEXT_BYTES;

/// 可选 id
fn check_opt_id(field: &str, value: Option<&str>) -> Result<(), IpcError> {
    value.map_or(Ok(()), |v| check_id(field, v))
}

/// 可选文本
fn check_opt_len(field: &str, value: Option<&str>, max: usize) -> Result<(), IpcError> {
    value.map_or(Ok(()), |v| check_len(field, v, max))
}

/// 矩形：四个分量都有限，宽高非负
pub fn check_rect(field: &str, r: &Rect) -> Result<(), IpcError> {
    if [r.x, r.y, r.width, r.height].iter().all(|v| v.is_finite()) && r.width >= 0.0 && r.height >= 0.0 {
        Ok(())
    } else {
        Err(IpcError::invalid_argument(format!(
            "{field} must have finite coordinates and a non-negative size"
        )))
    }
}

/// `ModelRef`
pub fn validate_model_ref(field: &str, m: &ModelRef) -> Result<(), IpcError> {
    check_id(&format!("{field}.provider_id"), &m.provider_id)?;
    check_id(&format!("{field}.model_id"), &m.model_id)
}

fn check_opt_model_ref(field: &str, m: Option<&ModelRef>) -> Result<(), IpcError> {
    m.map_or(Ok(()), |m| validate_model_ref(field, m))
}

/// `translate_start` 的请求
pub fn validate_translate_req(req: &TranslateReq) -> Result<(), IpcError> {
    check_len("req.text", &req.text, MAX_TEXT_BYTES)?;
    check_opt_len("req.draft", req.draft.as_deref(), MAX_TEXT_BYTES)?;
    check_id("req.style_id", &req.style_id)?;
    check_opt_model_ref("req.model_ref", req.model_ref.as_ref())
}

/// `ocr_start` 的请求
pub fn validate_ocr_req(req: &OcrReq) -> Result<(), IpcError> {
    check_id("req.capture_id", &req.capture_id)?;
    check_opt_model_ref("req.model_ref", req.model_ref.as_ref())
}

/// `panel_pin` 交接的状态
pub fn validate_pin_state(s: &PinState) -> Result<(), IpcError> {
    check_len("state.source_text", &s.source_text, MAX_TEXT_BYTES)?;
    check_len("state.result_text", &s.result_text, MAX_TEXT_BYTES)?;
    check_opt_len("state.ocr_text", s.ocr_text.as_deref(), MAX_TEXT_BYTES)?;
    check_id("state.style_id", &s.style_id)?;
    check_opt_id("state.request_id", s.request_id.as_deref())?;
    check_opt_id("state.capture_id", s.capture_id.as_deref())?;
    check_opt_len("state.status_line", s.status_line.as_deref(), MAX_LABEL_BYTES)?;
    // 面板填写的结构化字段。`expand_width` / `on_top` 由 Rust 在 pinned_init 时覆盖，
    // 传入的值不使用（都是整数 / bool，无需校验）
    check_opt_model_ref("state.model_ref", s.model_ref.as_ref())?;
    check_opt_len("state.thumb_url", s.thumb_url.as_deref(), MAX_LABEL_BYTES)?;
    if let Some(status) = &s.status {
        check_len("state.status.model_label", &status.model_label, MAX_LABEL_BYTES)?;
    }
    Ok(())
}

/// `window_open` 的目标（settings 的分组名只能是 `app::sections::ALL` 之一：拼错时报错，而不是静默打开默认分组）
pub fn validate_window_target(t: &WindowTarget) -> Result<(), IpcError> {
    match t {
        WindowTarget::Settings { section, anchor } => {
            check_opt_id("target.section", section.as_deref())?;
            check_opt_id("target.anchor", anchor.as_deref())?;
            match section.as_deref() {
                Some(s) if !crate::app::sections::ALL.contains(&s) => {
                    return Err(IpcError::invalid_argument(format!(
                        "target.section: unknown settings section {s:?}"
                    )));
                }
                _ => {}
            }
            // 锚点只认 `app::sections::ANCHORS` 里登记的「分组 → 锚点」组合
            match (section.as_deref(), anchor.as_deref()) {
                (_, None) => Ok(()),
                (Some(s), Some(a)) if crate::app::sections::anchor_known(s, a) => Ok(()),
                (_, Some(a)) => Err(IpcError::invalid_argument(format!(
                    "target.anchor: unknown anchor {a:?} for settings section {section:?}"
                ))),
            }
        }
        WindowTarget::Onboarding { .. } => Ok(()),
    }
}

/// 可选的窗口目标（`app_relaunch.resume`）
pub fn validate_opt_window_target(t: Option<&WindowTarget>) -> Result<(), IpcError> {
    t.map_or(Ok(()), validate_window_target)
}

/// `overlay_commit` 的请求
pub fn validate_overlay_commit(c: &OverlayCommit) -> Result<(), IpcError> {
    check_id("commit.monitor_id", &c.monitor_id)?;
    check_rect("commit.rect", &c.rect)
}

/// 快捷键作用对象（按风格绑定时的 style_id）
pub fn validate_hotkey_target(t: &HotkeyTarget) -> Result<(), IpcError> {
    match t {
        HotkeyTarget::Action { .. } => Ok(()),
        HotkeyTarget::Style { style_id } => check_id("target.style_id", style_id),
    }
}

/// `style_save` / `style_preview` 的风格
pub fn validate_style(s: &Style) -> Result<(), IpcError> {
    check_id("style.id", &s.id)?;
    check_len("style.name", &s.name, MAX_LABEL_BYTES)?;
    check_len("style.system_template", &s.system_template, MAX_TEMPLATE_BYTES)?;
    check_len("style.tail_instruction", &s.tail_instruction, MAX_TEMPLATE_BYTES)?;
    if let Some(t) = s.temperature {
        super::check_non_negative("style.temperature", f64::from(t))?;
    }
    check_opt_model_ref("style.model_ref", s.model_ref.as_ref())?;
    check_opt_len("style.hotkey", s.hotkey.as_deref(), MAX_LABEL_BYTES)
}

/// `styles_reorder` 的 id 列表
pub fn validate_style_ids(ids: &[String]) -> Result<(), IpcError> {
    check_list_len("ids", ids.len(), MAX_LIST_LEN)?;
    ids.iter().try_for_each(|id| check_id("ids[]", id))
}

/// `glossary_set` 的条目
pub fn validate_glossary(entries: &[GlossaryEntry]) -> Result<(), IpcError> {
    check_list_len("entries", entries.len(), MAX_LIST_LEN)?;
    entries.iter().try_for_each(|e| {
        check_len("entries[].source", &e.source, MAX_LABEL_BYTES)?;
        check_len("entries[].target", &e.target, MAX_LABEL_BYTES)?;
        check_opt_len("entries[].note", e.note.as_deref(), MAX_LABEL_BYTES)
    })
}

/// `excluded_app_upsert` 的应用：应用 id（bundle id、可执行文件名或 WM_CLASS）按 id 校验，名称按短文本
pub fn validate_excluded_app(a: &ExcludedApp) -> Result<(), IpcError> {
    check_id("entry.app_id", &a.app_id)?;
    check_len("entry.name", &a.name, MAX_LABEL_BYTES)
}

/// `app_keymap_upsert` 的按键映射：应用 id 按 id 校验，两个组合键按短文本
pub fn validate_app_keymap(k: &AppKeymap) -> Result<(), IpcError> {
    check_id("entry.app_id", &k.app_id)?;
    check_opt_len("entry.copy", k.copy.as_deref(), MAX_LABEL_BYTES)?;
    check_opt_len("entry.paste", k.paste.as_deref(), MAX_LABEL_BYTES)
}

fn validate_model_entry(m: &ModelEntry) -> Result<(), IpcError> {
    check_id("provider.models[].id", &m.id)?;
    check_opt_len("provider.models[].label", m.label.as_deref(), MAX_LABEL_BYTES)?;
    if let Some(efforts) = &m.caps.anthropic_efforts {
        check_list_len("provider.models[].caps.anthropic_efforts", efforts.len(), MAX_EFFORTS)?;
        efforts
            .iter()
            .try_for_each(|e| check_id("provider.models[].caps.anthropic_efforts[]", e))?;
    }
    Ok(())
}

/// `provider_test` / `provider_models` 的 Provider（草稿）。id 不能以 `@` 开头（保留给离线模型等内部服务商）
pub fn validate_provider(p: &Provider) -> Result<(), IpcError> {
    check_id("provider.id", &p.id)?;
    if crate::settings::validate::is_reserved_provider_id(&p.id) {
        return Err(IpcError::invalid_argument("provider.id must not start with @"));
    }
    check_len("provider.name", &p.name, MAX_LABEL_BYTES)?;
    check_len("provider.base_url", &p.base_url, MAX_QUERY_BYTES)?;
    check_list_len("provider.headers", p.headers.len(), MAX_HEADERS)?;
    for (name, value) in &p.headers {
        check_id("provider.headers{name}", name)?;
        check_len("provider.headers{value}", value, MAX_KEY_BYTES)?;
    }
    check_list_len("provider.models", p.models.len(), MAX_LIST_LEN)?;
    p.models.iter().try_for_each(validate_model_entry)?;
    check_opt_id("provider.template_id", p.template_id.as_deref())
}

/// 同步账号：地址按 WebDAV 地址的规则检查（≤ 1 KB；只允许 http / https；不带账号密码、查询与片段；`http://` 只允许
/// 局域网地址）。地址的错误直接给出界面文案的 key（`errors.sync.https_required` / `errors.sync.invalid_url`），
/// 设置页把它显示在地址输入框下面；账号只检查长度与控制字符（是否为空、能否用于 Basic 认证由服务判断）
pub fn validate_sync_account(a: &SyncAccount) -> Result<(), IpcError> {
    wisp_sync::validate_server_url(&a.server_url)
        .map_err(|e| IpcError::invalid_argument(format!("errors.sync.{}", e.reason())))?;
    check_len("account.username", &a.username, MAX_LABEL_BYTES)?;
    if a.username.chars().any(char::is_control) {
        return Err(IpcError::invalid_argument(
            "account.username must not contain control characters",
        ));
    }
    Ok(())
}

/// 可选的同步账号（`sync_reset_remote` 不传时用设置里的账号）
pub fn validate_opt_sync_account(a: Option<&SyncAccount>) -> Result<(), IpcError> {
    a.map_or(Ok(()), validate_sync_account)
}

/// 统计序列化字节数，不分配缓冲区
struct ByteCounter(usize);

impl std::io::Write for ByteCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `settings_patch` 的补丁：必须是 JSON 对象，序列化后不超过 [`MAX_PATCH_BYTES`]。
/// 字段级的合法性（类型、枚举、范围）由 `SettingsService::patch` 经 `from_value_lenient` + `validate` 判断
pub fn validate_settings_patch(patch: &serde_json::Value) -> Result<(), IpcError> {
    if !patch.is_object() {
        return Err(IpcError::invalid_argument("patch must be a JSON object"));
    }
    let mut counter = ByteCounter(0);
    serde_json::to_writer(&mut counter, patch).map_err(|_| IpcError::invalid_argument("patch is not serializable"))?;
    if counter.0 > MAX_PATCH_BYTES {
        return Err(IpcError::invalid_argument(format!(
            "patch exceeds {MAX_PATCH_BYTES} bytes"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use wisp_types::{
        DictMode, GlossaryEntry, HotkeyAction, Lang, ModelEntry, OcrMode, OverlaySelectionKind, PhysSize, PinKind,
        PinStatus, Protocol, TaskKind,
    };

    use super::*;
    use crate::error::IpcErrorCode;
    use crate::ipc::MAX_ID_BYTES;

    /// 对合法 Provider 的一次破坏性修改
    type Mutation = Box<dyn Fn(&mut Provider)>;

    fn is_invalid(r: Result<(), IpcError>) -> bool {
        r.is_err_and(|e| e.code == IpcErrorCode::InvalidArgument)
    }

    fn provider() -> Provider {
        Provider {
            id: "openai-main".to_owned(),
            name: "OpenAI".to_owned(),
            protocol: Protocol::OpenAiChat,
            base_url: "https://api.openai.com/v1".to_owned(),
            headers: BTreeMap::from([("x-org".to_owned(), "org-1".to_owned())]),
            quirks: Default::default(),
            models: vec![ModelEntry::new("gpt-4.1")],
            timeouts: Default::default(),
            template_id: Some("openai".to_owned()),
        }
    }

    #[test]
    fn provider_limits() {
        assert!(validate_provider(&provider()).is_ok());
        let long_id = "m".repeat(MAX_ID_BYTES + 1);
        let cases: Vec<Mutation> = vec![
            Box::new(|p| p.id = "a\nb".to_owned()),
            Box::new(|p| p.base_url = "h".repeat(MAX_QUERY_BYTES + 1)),
            Box::new(|p| p.name = "n".repeat(MAX_LABEL_BYTES + 1)),
            Box::new(|p| {
                p.headers = (0..=MAX_HEADERS).map(|i| (format!("x-{i}"), String::new())).collect();
            }),
            Box::new(|p| {
                p.headers
                    .insert("authorization".to_owned(), "k".repeat(MAX_KEY_BYTES + 1));
            }),
            Box::new(move |p| p.models.push(ModelEntry::new(long_id.clone()))),
            Box::new(|p| p.models = vec![ModelEntry::new("m"); MAX_LIST_LEN + 1]),
            Box::new(|p| p.template_id = Some("t\u{0}".to_owned())),
            Box::new(|p| p.models[0].caps.anthropic_efforts = Some(vec!["low".to_owned(); MAX_EFFORTS + 1])),
        ];
        for (i, mutate) in cases.iter().enumerate() {
            let mut p = provider();
            mutate(&mut p);
            assert!(is_invalid(validate_provider(&p)), "case {i}");
        }
        // 恰好等于上限合法
        let mut p = provider();
        p.headers = (0..MAX_HEADERS)
            .map(|i| (format!("x-{i}"), "v".repeat(MAX_KEY_BYTES)))
            .collect();
        p.base_url = "h".repeat(MAX_QUERY_BYTES);
        assert!(validate_provider(&p).is_ok());
    }

    #[test]
    fn style_limits() {
        let ok = Style::custom("custom-1a2b3c4d", "我的风格");
        assert!(validate_style(&ok).is_ok());
        let mut s = ok.clone();
        s.system_template = "x".repeat(MAX_TEMPLATE_BYTES);
        assert!(validate_style(&s).is_ok(), "恰好等于上限合法");
        s.system_template.push('x');
        assert!(is_invalid(validate_style(&s)));
        for t in [f32::NAN, f32::INFINITY, -0.1] {
            let s = Style {
                temperature: Some(t),
                ..ok.clone()
            };
            assert!(is_invalid(validate_style(&s)), "{t}");
        }
        let s = Style {
            temperature: None,
            ..ok.clone()
        };
        assert!(validate_style(&s).is_ok(), "null = 不发送");
        let s = Style {
            model_ref: Some(ModelRef {
                provider_id: "p".repeat(MAX_ID_BYTES + 1),
                model_id: "m".to_owned(),
            }),
            ..ok
        };
        assert!(is_invalid(validate_style(&s)));
    }

    #[test]
    fn list_limits() {
        assert!(validate_style_ids(&["natural".to_owned(), "custom-1".to_owned()]).is_ok());
        assert!(is_invalid(validate_style_ids(&vec![String::new(); MAX_LIST_LEN + 1])));
        assert!(is_invalid(validate_style_ids(&["a\tb".to_owned()])));
        let entry = |source: &str| GlossaryEntry {
            source: source.to_owned(),
            target: "令牌".to_owned(),
            note: None,
        };
        assert!(validate_glossary(&[entry("token")]).is_ok());
        assert!(is_invalid(validate_glossary(&[entry(
            &"t".repeat(MAX_LABEL_BYTES + 1)
        )])));
        assert!(is_invalid(validate_glossary(&vec![entry("t"); MAX_LIST_LEN + 1])));
    }

    #[test]
    fn app_list_entries() {
        let app = ExcludedApp {
            os: wisp_types::OsKind::Macos,
            app_id: "com.example.vault".to_owned(),
            name: "Vault".to_owned(),
        };
        assert!(validate_excluded_app(&app).is_ok());
        for bad in [
            ExcludedApp {
                app_id: "a".repeat(MAX_ID_BYTES + 1),
                ..app.clone()
            },
            ExcludedApp {
                app_id: "com.example\nvault".to_owned(),
                ..app.clone()
            },
            ExcludedApp {
                name: "n".repeat(MAX_LABEL_BYTES + 1),
                ..app.clone()
            },
        ] {
            assert!(is_invalid(validate_excluded_app(&bad)), "{bad:?}");
        }
        let keymap = AppKeymap {
            os: wisp_types::OsKind::Linux,
            app_id: "kitty".to_owned(),
            copy: None,
            paste: Some("Control+Shift+KeyV".to_owned()),
        };
        assert!(validate_app_keymap(&keymap).is_ok());
        for bad in [
            AppKeymap {
                app_id: "k\u{0}".to_owned(),
                ..keymap.clone()
            },
            AppKeymap {
                copy: Some("c".repeat(MAX_LABEL_BYTES + 1)),
                ..keymap.clone()
            },
            AppKeymap {
                paste: Some("p".repeat(MAX_LABEL_BYTES + 1)),
                ..keymap.clone()
            },
        ] {
            assert!(is_invalid(validate_app_keymap(&bad)), "{bad:?}");
        }
    }

    #[test]
    fn requests_and_targets() {
        let req = TranslateReq {
            session_id: 1,
            text: "hello".to_owned(),
            task: TaskKind::Translate,
            style_id: "natural".to_owned(),
            target_lang: None,
            model_ref: None,
            dict_mode: DictMode::Auto,
            bypass_cache: false,
            draft: None,
            confirm_long: false,
        };
        assert!(validate_translate_req(&req).is_ok());
        let over = "a".repeat(MAX_TEXT_BYTES + 1);
        assert!(is_invalid(validate_translate_req(&TranslateReq {
            text: over.clone(),
            ..req.clone()
        })));
        assert!(is_invalid(validate_translate_req(&TranslateReq {
            draft: Some(over.clone()),
            ..req.clone()
        })));
        assert!(is_invalid(validate_translate_req(&TranslateReq {
            style_id: "s".repeat(MAX_ID_BYTES + 1),
            ..req
        })));

        let ocr = OcrReq {
            session_id: 1,
            capture_id: "c-1".to_owned(),
            mode: OcrMode::Lines,
            model_ref: None,
            target_lang: None,
        };
        assert!(validate_ocr_req(&ocr).is_ok());
        assert!(is_invalid(validate_ocr_req(&OcrReq {
            capture_id: "c\n".to_owned(),
            ..ocr
        })));

        let pin = PinState {
            kind: PinKind::Translate,
            source_text: "a".to_owned(),
            result_text: "b".to_owned(),
            task: TaskKind::Translate,
            style_id: "natural".to_owned(),
            target_lang: Lang::ZhCn,
            detected_lang: None,
            request_id: Some("r-1".to_owned()),
            capture_id: None,
            ocr_text: None,
            ocr_mode: None,
            show_source: false,
            status_line: None,
            dict_mode: false,
            model_ref: None,
            status: None,
            thumb_url: None,
            image_size: None,
            expand_width: None,
            on_top: None,
        };
        assert!(validate_pin_state(&pin).is_ok());
        assert!(is_invalid(validate_pin_state(&PinState {
            result_text: over,
            ..pin.clone()
        })));
        assert!(is_invalid(validate_pin_state(&PinState {
            status_line: Some("s".repeat(MAX_LABEL_BYTES + 1)),
            ..pin.clone()
        })));
        // 结构化字段
        let full = PinState {
            dict_mode: true,
            model_ref: Some(ModelRef {
                provider_id: "openai-main".to_owned(),
                model_id: "gpt-5-mini".to_owned(),
            }),
            status: Some(PinStatus {
                model_label: "GPT-5 mini".to_owned(),
                elapsed_ms: Some(820),
                usage: None,
                cached: false,
                cancelled: false,
                truncated: None,
            }),
            thumb_url: Some("capture/1/thumb".to_owned()),
            image_size: Some(PhysSize {
                width: 800,
                height: 600,
            }),
            expand_width: Some(640),
            on_top: Some(true),
            ..pin.clone()
        };
        assert!(validate_pin_state(&full).is_ok());
        assert!(is_invalid(validate_pin_state(&PinState {
            model_ref: Some(ModelRef {
                provider_id: "p\n".to_owned(),
                model_id: "m".to_owned(),
            }),
            ..full.clone()
        })));
        assert!(is_invalid(validate_pin_state(&PinState {
            thumb_url: Some("t".repeat(MAX_LABEL_BYTES + 1)),
            ..full.clone()
        })));
        let mut long_label = full;
        if let Some(st) = long_label.status.as_mut() {
            st.model_label = "m".repeat(MAX_LABEL_BYTES + 1);
        }
        assert!(is_invalid(validate_pin_state(&long_label)));

        assert!(
            validate_window_target(&WindowTarget::Settings {
                section: None,
                anchor: None
            })
            .is_ok()
        );
        assert!(is_invalid(validate_window_target(&WindowTarget::Settings {
            section: Some("s".repeat(MAX_ID_BYTES + 1)),
            anchor: None,
        })));
        assert!(validate_window_target(&WindowTarget::Onboarding { step: Some(3) }).is_ok());

        assert!(
            validate_hotkey_target(&HotkeyTarget::Action {
                action: HotkeyAction::Ocr
            })
            .is_ok()
        );
        assert!(is_invalid(validate_hotkey_target(&HotkeyTarget::Style {
            style_id: "a\rb".to_owned(),
        })));

        let commit = OverlayCommit {
            session_id: 1,
            monitor_id: r"\\.\DISPLAY1".to_owned(),
            rect: Rect::new(10.0, 10.0, 100.0, 50.0),
            kind: OverlaySelectionKind::Drag,
        };
        assert!(validate_overlay_commit(&commit).is_ok());
        for rect in [
            Rect::new(f64::NAN, 0.0, 1.0, 1.0),
            Rect::new(0.0, 0.0, f64::INFINITY, 1.0),
            Rect::new(0.0, 0.0, 1.0, -1.0),
        ] {
            assert!(is_invalid(validate_overlay_commit(&OverlayCommit {
                rect,
                ..commit.clone()
            })));
        }
    }

    #[test]
    fn settings_sections_are_checked() {
        for section in crate::app::sections::ALL {
            let t = WindowTarget::Settings {
                section: Some(section.to_owned()),
                anchor: None,
            };
            assert!(validate_window_target(&t).is_ok(), "{section}");
        }
        assert!(
            validate_window_target(&WindowTarget::Settings {
                section: None,
                anchor: None
            })
            .is_ok()
        );
        let typo = WindowTarget::Settings {
            section: Some("provider".to_owned()),
            anchor: None,
        };
        assert!(is_invalid(validate_window_target(&typo)));
    }

    /// 锚点只认登记过的「分组 → 锚点」组合；没有分组时不能带锚点
    #[test]
    fn settings_anchors_are_checked() {
        for (section, anchor) in crate::app::sections::ANCHORS {
            let t = WindowTarget::Settings {
                section: Some(section.to_owned()),
                anchor: Some(anchor.to_owned()),
            };
            assert!(validate_window_target(&t).is_ok(), "{section} / {anchor}");
        }
        for (section, anchor) in [
            (Some("ocr"), "history"),
            (Some("translate"), "ocr_history"),
            (None, "ocr_history"),
            (Some("ocr"), "ocr_history\n"),
        ] {
            let t = WindowTarget::Settings {
                section: section.map(str::to_owned),
                anchor: Some(anchor.to_owned()),
            };
            assert!(is_invalid(validate_window_target(&t)), "{section:?} / {anchor}");
        }
    }

    #[test]
    fn sync_account_rules() {
        let account = |url: &str, user: &str| SyncAccount {
            server_url: url.to_owned(),
            username: user.to_owned(),
        };
        let message = |r: Result<(), IpcError>| r.expect_err("invalid").message;
        assert!(validate_sync_account(&account("https://dav.example.com/dav", "me")).is_ok());
        // 局域网地址可以用 http
        assert!(validate_sync_account(&account("http://192.168.1.2:5005/", "me")).is_ok());
        assert!(validate_sync_account(&account("http://nas.local/dav/", "me")).is_ok());
        assert_eq!(
            message(validate_sync_account(&account("http://dav.example.com/", "me"))),
            "errors.sync.https_required"
        );
        for bad in [
            "",
            "dav.example.com",
            "ftp://dav.example.com/",
            "https://u:p@dav.example.com/",
            "https://dav.example.com/?a=1",
            "https://dav.example.com/#x",
        ] {
            assert_eq!(
                message(validate_sync_account(&account(bad, "me"))),
                "errors.sync.invalid_url",
                "{bad}"
            );
        }
        let long = format!("https://dav.example.com/{}", "a".repeat(1024));
        assert_eq!(
            message(validate_sync_account(&account(&long, "me"))),
            "errors.sync.invalid_url"
        );
        // 账号：长度与控制字符；错误信息不带值本身
        let err = message(validate_sync_account(&account(
            "https://dav.example.com/",
            &"a".repeat(MAX_LABEL_BYTES + 1),
        )));
        assert!(err.starts_with("account.username exceeds"), "{err}");
        assert!(is_invalid(validate_sync_account(&account(
            "https://dav.example.com/",
            "me\nyou"
        ))));
        assert!(validate_opt_sync_account(None).is_ok());
        assert!(is_invalid(validate_opt_sync_account(Some(&account(
            "http://dav.example.com/",
            "me"
        )))));
    }

    #[test]
    fn settings_patch_shape_and_size() {
        assert!(validate_settings_patch(&serde_json::json!({ "general": { "autostart": true } })).is_ok());
        for bad in [serde_json::json!([]), serde_json::json!(null), serde_json::json!("x")] {
            assert!(is_invalid(validate_settings_patch(&bad)), "{bad}");
        }
        let big = serde_json::json!({ "x": "a".repeat(MAX_PATCH_BYTES) });
        assert!(is_invalid(validate_settings_patch(&big)));
    }
}
