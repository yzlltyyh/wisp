//! 未就绪检查：没有 Provider、没有默认模型、没有视觉模型、缺权限、Wayland 需先复制、钥匙串不可用等；
//! 以及平台层 / 截图失败到 `NotReady` 的映射（规则只在这里实现一次，编排器直接调用）。

use wisp_capture::CaptureError;
use wisp_platform::{SelectionError, reason};
use wisp_types::{KeychainState, ModelRef, NotReady, PermissionKind, Settings, Style, TaskKind};

use super::plan::{Resolved, provider_needs_key, resolve_ocr_model, resolve_text_model};

/// 截图失败的原因 key（`NotReady::CaptureFailed.message`，前端 `locales/*/errors.json` 的 `capture` 分组）
pub mod capture_reason {
    /// 截图后端出错（X11 5 s 硬超时、像素格式转换失败、系统接口报错……）：请重试
    pub const BACKEND: &str = "errors.capture.backend";
    /// 没有可截图的显示器（全部断开 / 睡眠）
    pub const NO_DISPLAYS: &str = "errors.capture.no_displays";
    /// 当前会话不支持截图（X11 缺少扩展、没有截图门户……；Wayland 用 `errors.platform.wayland_capture`）
    pub const UNSUPPORTED: &str = "errors.capture.unsupported";
}

/// 文本任务能否发出请求：没有 Provider → `NoProvider`；请求指定、风格绑定、默认翻译模型都解析不出
/// → `NoDefaultModel`；钥匙串不可用（`Unavailable` / 仅本次会话）且这个 Provider 需要 Key 又没有 Key →
/// `KeychainUnavailable`（发请求之前就能判断；读取时被拒绝则是请求里的 `Error{keychain}`，不在这里）。
/// 返回解析好的模型
pub fn check_text(
    settings: &Settings,
    style: &Style,
    explicit: Option<&ModelRef>,
    task: TaskKind,
    keys: &dyn KeyIndex,
) -> Result<Resolved, NotReady> {
    let resolved = resolve_text_model(settings, explicit, style, task)?;
    check_key(&resolved, keys)?;
    Ok(resolved)
}

/// OCR 能否发出请求：没有 Provider → `NoProvider`；没有可用的视觉模型 → `NoVisionModel`；钥匙串同 [`check_text`]。
/// 截图权限与截图失败不在这里（见 [`capture_not_ready`]）。
///
/// 注意：因此 OCR 视图的 `OcrPayload.not_ready` 也可能是 `KeychainUnavailable`（钥匙串不可用且视觉模型的 Provider
/// 需要 Key 又没有 Key）；前端的 `NotReadyCard` 覆盖全部 `NotReady`
pub fn check_ocr(settings: &Settings, explicit: Option<&ModelRef>, keys: &dyn KeyIndex) -> Result<Resolved, NotReady> {
    let resolved = resolve_ocr_model(settings, explicit)?;
    check_key(&resolved, keys)?;
    Ok(resolved)
}

/// 钥匙串状态与 Key 索引（不读明文）
pub trait KeyIndex {
    fn keychain_state(&self) -> KeychainState;
    fn has_key(&self, provider_id: &str) -> bool;
}

fn check_key(resolved: &Resolved, keys: &dyn KeyIndex) -> Result<(), NotReady> {
    let unavailable = matches!(
        keys.keychain_state(),
        KeychainState::Unavailable | KeychainState::SessionOnly
    );
    if unavailable && !keys.has_key(&resolved.provider.id) && provider_needs_key(&resolved.provider) {
        return Err(NotReady::KeychainUnavailable);
    }
    Ok(())
}

/// 取词失败 → 输入视图顶部的提示（`InputPayload.hint`）。**所有**取词失败都以输入视图打开，这里只决定提示条的内容；None = 不带提示（`Empty` / `Timeout`：用户可能本来就没选中文字；
/// `Other`：平台层的意外错误，记日志，不向用户展示开发者文本）
pub fn selection_hint(e: &SelectionError) -> Option<NotReady> {
    match e {
        SelectionError::Empty | SelectionError::Timeout => None,
        SelectionError::PermissionMissing(permission) => Some(NotReady::PermissionMissing {
            permission: *permission,
        }),
        SelectionError::Elevated => Some(NotReady::Elevated),
        SelectionError::Unsupported(key) if key == reason::WAYLAND_SELECTION => Some(NotReady::WaylandCopyFirst),
        SelectionError::Unsupported(key) => Some(NotReady::SelectionUnavailable {
            reason_key: key.clone(),
        }),
        SelectionError::Other(message) => {
            log::warn!("selection failed: {message}");
            None
        }
    }
}

/// 截图失败 → OCR 视图的未就绪卡片（`OcrPayload.not_ready`）。None = 静默结束（用户取消）。
/// `wayland`：当前是 Wayland 会话（`PlatformInfo.session == Wayland`），此时 `Unsupported` 一律提示这一版在 Wayland 会话下
/// 无法截图、需要截图时切换到 X11 会话（还没有接入截图门户，重试也不会成功——OCR 视图不给「重新截图」）。
/// 开发者文本（`Backend` / `Unsupported` 携带的字符串）只进日志，`CaptureFailed.message` 只放 i18n key
pub fn capture_not_ready(e: &CaptureError, wayland: bool) -> Option<NotReady> {
    let key = match e {
        CaptureError::Cancelled => return None,
        CaptureError::PermissionDenied => {
            return Some(NotReady::PermissionMissing {
                permission: PermissionKind::ScreenRecording,
            });
        }
        CaptureError::NoDisplays => capture_reason::NO_DISPLAYS,
        CaptureError::Unsupported(detail) => {
            log::warn!("capture unsupported: {detail}");
            if wayland {
                reason::WAYLAND_CAPTURE
            } else {
                capture_reason::UNSUPPORTED
            }
        }
        CaptureError::Backend(detail) => {
            log::warn!("capture failed: {detail}");
            capture_reason::BACKEND
        }
    };
    Some(NotReady::CaptureFailed {
        message: key.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_errors_map_to_input_hints() {
        assert_eq!(selection_hint(&SelectionError::Empty), None);
        assert_eq!(selection_hint(&SelectionError::Timeout), None);
        assert_eq!(
            selection_hint(&SelectionError::Other("AXError -25204".to_owned())),
            None
        );
        assert_eq!(
            selection_hint(&SelectionError::PermissionMissing(PermissionKind::Accessibility)),
            Some(NotReady::PermissionMissing {
                permission: PermissionKind::Accessibility
            })
        );
        assert_eq!(selection_hint(&SelectionError::Elevated), Some(NotReady::Elevated));
        assert_eq!(
            selection_hint(&SelectionError::Unsupported(reason::WAYLAND_SELECTION.to_owned())),
            Some(NotReady::WaylandCopyFirst)
        );
        for key in [
            reason::CLIPBOARD_TOO_LARGE,
            reason::X11_MISSING_XTEST,
            reason::NO_DISPLAY,
        ] {
            assert_eq!(
                selection_hint(&SelectionError::Unsupported(key.to_owned())),
                Some(NotReady::SelectionUnavailable {
                    reason_key: key.to_owned()
                })
            );
        }
    }

    #[test]
    fn capture_errors_carry_i18n_keys_only() {
        let failed = |key: &str| {
            Some(NotReady::CaptureFailed {
                message: key.to_owned(),
            })
        };
        assert_eq!(capture_not_ready(&CaptureError::Cancelled, false), None);
        assert_eq!(
            capture_not_ready(&CaptureError::PermissionDenied, false),
            Some(NotReady::PermissionMissing {
                permission: PermissionKind::ScreenRecording
            })
        );
        assert_eq!(
            capture_not_ready(&CaptureError::NoDisplays, false),
            failed(capture_reason::NO_DISPLAYS)
        );
        let backend = CaptureError::Backend("X11 capture timed out".to_owned());
        assert_eq!(capture_not_ready(&backend, false), failed(capture_reason::BACKEND));
        let wayland = CaptureError::Unsupported("wayland session: use portal_screenshot".to_owned());
        assert_eq!(capture_not_ready(&wayland, true), failed(reason::WAYLAND_CAPTURE));
        assert_eq!(capture_not_ready(&wayland, false), failed(capture_reason::UNSUPPORTED));
    }

    struct Keys(KeychainState, bool);

    impl KeyIndex for Keys {
        fn keychain_state(&self) -> KeychainState {
            self.0
        }
        fn has_key(&self, _: &str) -> bool {
            self.1
        }
    }

    #[test]
    fn keychain_unavailable_only_when_a_key_is_needed_and_missing() {
        let s = crate::orchestrator::plan::tests::configured();
        let styles = wisp_prompts::builtin_styles();
        let style = &styles[0];
        let ok = |k: Keys| check_text(&s, style, None, TaskKind::Translate, &k).map(|r| r.model.id);
        assert_eq!(ok(Keys(KeychainState::Available, false)), Ok("m".to_owned()));
        assert_eq!(ok(Keys(KeychainState::SessionOnly, true)), Ok("m".to_owned()));
        assert_eq!(
            ok(Keys(KeychainState::Unavailable, false)),
            Err(NotReady::KeychainUnavailable)
        );
        // 本地服务不需要 Key
        let mut local = s.clone();
        local.providers[0].base_url = "http://localhost:11434/v1".into();
        assert!(
            check_text(
                &local,
                style,
                None,
                TaskKind::Translate,
                &Keys(KeychainState::Unavailable, false)
            )
            .is_ok()
        );
        assert_eq!(
            check_ocr(&s, None, &Keys(KeychainState::Unavailable, false)),
            Err(NotReady::KeychainUnavailable)
        );
        let mut empty = s.clone();
        empty.providers.clear();
        assert_eq!(
            check_ocr(&empty, None, &Keys(KeychainState::Available, false)),
            Err(NotReady::NoProvider)
        );
    }
}
