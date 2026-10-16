//! 流式请求命令。

use tauri::State;
use tauri::ipc::Channel;
use wisp_types::{ActionOrigin, HotkeyAction, OcrReq, StartAck, StreamEvent, TranslateReq};

use crate::bus::{Command, TriggerOrigin};
use crate::error::IpcError;
use crate::ipc::validate::{validate_ocr_req, validate_translate_req};
use crate::ipc::{MAX_TEXT_BYTES, check_id, check_len};
use crate::orchestrator::{RequestCaller, RequestHost};
use crate::state::AppState;

/// 回放已有事件，之后继续推送实时事件。只有请求的宿主窗口（按调用方窗口 label 确定，前端无法伪造）可以接上；
/// 别的窗口与请求不存在一样得到 not_found
#[tauri::command(rename_all = "snake_case")]
pub async fn stream_attach(
    app_state: State<'_, AppState>,
    webview_window: tauri::WebviewWindow,
    request_id: String,
    on_event: Channel<StreamEvent>,
) -> Result<(), IpcError> {
    check_id("request_id", &request_id)?;
    let caller = RequestCaller::of(webview_window.label())
        .ok_or_else(|| IpcError::not_found(format!("unknown request {request_id}")))?;
    app_state.orchestrator.stream_attach(&caller, &request_id, on_event)
}

/// 取消请求。只取消调用方窗口自己的请求；别的窗口的请求、已结束 / 不存在的请求都是空操作（返回 `()`，
/// 不因宿主不符而 reject）
#[tauri::command(rename_all = "snake_case")]
pub async fn cancel(
    app_state: State<'_, AppState>,
    webview_window: tauri::WebviewWindow,
    request_id: String,
) -> Result<(), IpcError> {
    check_id("request_id", &request_id)?;
    if let Some(caller) = RequestCaller::of(webview_window.label()) {
        app_state.orchestrator.cancel(&caller, &request_id);
    }
    Ok(())
}

/// 由面板或固定窗口发起的翻译 / 润色 / 校对 / 提升。宿主按调用方窗口确定：面板传自己的会话 id，固定窗口传
/// `session_id = 0`
#[tauri::command(rename_all = "snake_case")]
pub async fn translate_start(
    app_state: State<'_, AppState>,
    webview_window: tauri::WebviewWindow,
    req: TranslateReq,
    on_event: Channel<StreamEvent>,
) -> Result<StartAck, IpcError> {
    validate_translate_req(&req)?;
    let host = RequestHost::resolve(webview_window.label(), req.session_id)?;
    app_state.orchestrator.translate_start(req, host, on_event).await
}

/// 重新识别、切换模式 / 模型、直接翻译时查看原文（宿主规则同 `translate_start`）
#[tauri::command(rename_all = "snake_case")]
pub async fn ocr_start(
    app_state: State<'_, AppState>,
    webview_window: tauri::WebviewWindow,
    req: OcrReq,
    on_event: Channel<StreamEvent>,
) -> Result<StartAck, IpcError> {
    validate_ocr_req(&req)?;
    let host = RequestHost::resolve(webview_window.label(), req.session_id)?;
    app_state.orchestrator.ocr_start(req, host, on_event).await
}

/// 面板关闭时释放内存里的截图。只释放调用方窗口自己的截图；别的窗口的截图（例如已经随固定交接转给 pinned 的）与
/// 不存在的截图都是空操作（返回 `()`）
#[tauri::command(rename_all = "snake_case")]
pub async fn capture_release(
    app_state: State<'_, AppState>,
    webview_window: tauri::WebviewWindow,
    capture_id: String,
) -> Result<(), IpcError> {
    check_id("capture_id", &capture_id)?;
    if let Some(caller) = RequestCaller::of(webview_window.label()) {
        app_state.orchestrator.capture_release(&caller, &capture_id);
    }
    Ok(())
}

/// 面板里切换排版后让剪贴板跟随，返回是否写入。`request_id` 是视图正在显示的那次识别（载荷或 `ocr_start` 给的）。
/// 只在自动复制开着、调用方是面板（固定窗口里切换不带动剪贴板）、不是直接翻译、它仍是这张截图当前这次识别、系统剪贴板
/// 仍是这次识别自动复制的内容（期间没有别的复制）时写入；写入照常进入剪贴板历史，这次自动复制新建、没有收藏的那一条
/// 随之删掉。这次识别被取消、失败或自动复制没写成时返回 false；截图不存在或属于别的窗口时同样返回 false。「用户没有
/// 编辑」由视图负责
#[tauri::command(rename_all = "snake_case")]
pub async fn ocr_recopy(
    app_state: State<'_, AppState>,
    webview_window: tauri::WebviewWindow,
    capture_id: String,
    request_id: String,
    text: String,
) -> Result<bool, IpcError> {
    check_id("capture_id", &capture_id)?;
    check_id("request_id", &request_id)?;
    check_len("text", &text, MAX_TEXT_BYTES)?;
    let Some(caller) = RequestCaller::of(webview_window.label()) else {
        return Ok(false);
    };
    app_state
        .orchestrator
        .ocr_recopy(&caller, &capture_id, &request_id, text)
        .await
}

/// 前端发起的全局动作：引导页「试一试」（origin = onboarding）、剪贴板条目送翻译（origin = clipboard）。
///
/// 语义：
/// - `text` 为 null：等同于按下该动作的快捷键（`Command::Action`），例如「试一试」截图 / 划词；
/// - `text` 非 null：只允许 `translate_selection` / `translate_input`，表示「把这段文本直接送去翻译」
///   （`Command::TranslateText`，不取词）；其他动作带 text 一律 `invalid_argument`；
/// - `origin` 原样进入 `TriggerOrigin`，用于 PerfMarks 与日志。
#[tauri::command(rename_all = "snake_case")]
pub async fn trigger_action(
    app_state: State<'_, AppState>,
    action: HotkeyAction,
    text: Option<String>,
    origin: ActionOrigin,
) -> Result<(), IpcError> {
    if let Some(text) = &text {
        check_len("text", text, MAX_TEXT_BYTES)?;
    }
    let cmd = action_command(action, text, origin.into())?;
    app_state.bus.send(cmd);
    Ok(())
}

/// `trigger_action` 的参数 → 总线命令（只做动作与 text 的组合校验和分派；text 的长度已在命令里校验）
fn action_command(action: HotkeyAction, text: Option<String>, origin: TriggerOrigin) -> Result<Command, IpcError> {
    match (action, text) {
        (_, None) => Ok(Command::Action { action, origin }),
        (HotkeyAction::TranslateSelection | HotkeyAction::TranslateInput, Some(text)) => {
            Ok(Command::TranslateText { text, origin })
        }
        (_, Some(_)) => Err(IpcError::invalid_argument(format!(
            "text is only accepted for translate actions, got {}",
            action.as_str()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatches_by_action() {
        let origin = TriggerOrigin::Clipboard;
        assert_eq!(
            action_command(HotkeyAction::Ocr, None, origin).expect("ocr"),
            Command::Action {
                action: HotkeyAction::Ocr,
                origin
            }
        );
        assert_eq!(
            action_command(HotkeyAction::TranslateSelection, Some("hi".to_owned()), origin).expect("translate"),
            Command::TranslateText {
                text: "hi".to_owned(),
                origin
            }
        );
        let err = action_command(HotkeyAction::Ocr, Some("x".to_owned()), origin).expect_err("ocr + text");
        assert_eq!(err.code, crate::error::IpcErrorCode::InvalidArgument);
    }
}
