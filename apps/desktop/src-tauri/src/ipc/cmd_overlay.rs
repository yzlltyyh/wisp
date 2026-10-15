//! Overlay 命令。
//!
//! 纵深防御：ready / commit / done / cancel 都核对**调用方窗口**——必须是绑定了这次截图
//! 会话的 `overlay-<n>` 窗口（commit / ready 还要求是它自己那块显示器）。面板、pinned、设置窗口提交或取消不了当前
//! Overlay，Overlay A 也提交不了显示器 B；对不上时与会话不存在一样返回 not_found。

use tauri::State;
use wisp_types::{MonitorId, OverlayCommit, OverlayCommitAck, OverlayInit};

use crate::error::IpcError;
use crate::ipc::check_id;
use crate::ipc::validate::validate_overlay_commit;
use crate::state::AppState;

/// 调用方窗口是否绑定了这次会话（`monitor` 为 Some 时还要求是这块显示器）
fn caller_owns(
    bound_session: Option<u64>,
    bound_monitor: Option<&MonitorId>,
    session_id: u64,
    monitor: Option<&str>,
) -> bool {
    bound_session == Some(session_id) && monitor.is_none_or(|m| bound_monitor.is_some_and(|b| b == m))
}

/// 核对调用方；不是这次会话的 Overlay 窗口时 not_found
fn check_caller(
    app_state: &AppState,
    window: &tauri::WebviewWindow,
    session_id: u64,
    monitor: Option<&str>,
) -> Result<(), IpcError> {
    let label = window.label();
    let bound_session = app_state.windows.overlay_session_of(label);
    let bound_monitor = app_state.windows.overlay_monitor(label);
    if caller_owns(bound_session, bound_monitor.as_ref(), session_id, monitor) {
        Ok(())
    } else {
        log::debug!("overlay command from {label} rejected: not bound to session {session_id}");
        Err(IpcError::not_found("overlay session"))
    }
}

/// Overlay 拉取本窗口当前会话的初始化数据。没有前端参数：按调用方窗口的 label（`overlay-<n>`）反查绑定的显示器
/// （Windows 的 MonitorId 是 `\\.\DISPLAYn`，不能放进 label）。窗口当前没有会话（预建、空闲中）时返回 None，
/// 页面等 `overlay:session` 事件再调一次（先订阅、后调用，按 session_id 去重，见 `OverlaySessionStart`）
#[tauri::command(rename_all = "snake_case")]
pub async fn overlay_init(
    app_state: State<'_, AppState>,
    webview_window: tauri::WebviewWindow,
) -> Result<Option<OverlayInit>, IpcError> {
    app_state.orchestrator.overlay_init(webview_window.label())
}

/// 首帧渲染完成 → 显示 Overlay
#[tauri::command(rename_all = "snake_case")]
pub async fn overlay_ready(
    app_state: State<'_, AppState>,
    webview_window: tauri::WebviewWindow,
    session_id: u64,
    monitor_id: String,
) -> Result<(), IpcError> {
    check_id("monitor_id", &monitor_id)?;
    check_caller(&app_state, &webview_window, session_id, Some(&monitor_id))?;
    app_state.orchestrator.overlay_ready(session_id, &monitor_id);
    Ok(())
}

/// 立即裁剪并发出请求，然后返回飞入目标
#[tauri::command(rename_all = "snake_case")]
pub async fn overlay_commit(
    app_state: State<'_, AppState>,
    webview_window: tauri::WebviewWindow,
    commit: OverlayCommit,
) -> Result<OverlayCommitAck, IpcError> {
    validate_overlay_commit(&commit)?;
    check_caller(&app_state, &webview_window, commit.session_id, Some(&commit.monitor_id))?;
    app_state.orchestrator.overlay_commit(commit).await
}

/// 飞入 / 淡出结束
#[tauri::command(rename_all = "snake_case")]
pub async fn overlay_done(
    app_state: State<'_, AppState>,
    webview_window: tauri::WebviewWindow,
    session_id: u64,
) -> Result<(), IpcError> {
    check_caller(&app_state, &webview_window, session_id, None)?;
    app_state.orchestrator.overlay_done(session_id);
    Ok(())
}

/// Esc 取消
#[tauri::command(rename_all = "snake_case")]
pub async fn overlay_cancel(
    app_state: State<'_, AppState>,
    webview_window: tauri::WebviewWindow,
    session_id: u64,
) -> Result<(), IpcError> {
    check_caller(&app_state, &webview_window, session_id, None)?;
    app_state.orchestrator.overlay_cancel(session_id);
    Ok(())
}

/// 多屏截图时调用方 Overlay 请求成为 key（指针进入它、而它还不是 key 时）。不激活应用；
/// 会话已 commit / 已结束时为空操作。调用方必须是绑定了该会话的 `overlay-<n>`（与 ready / commit 同一核对）
#[tauri::command(rename_all = "snake_case")]
pub async fn overlay_focus(
    app_state: State<'_, AppState>,
    webview_window: tauri::WebviewWindow,
    session_id: u64,
) -> Result<(), IpcError> {
    check_caller(&app_state, &webview_window, session_id, None)?;
    app_state.orchestrator.overlay_focus(session_id, webview_window.label());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::caller_owns;

    #[test]
    fn only_the_bound_overlay_window_may_drive_its_session() {
        let m1 = "m1".to_owned();
        // 本会话、本显示器
        assert!(caller_owns(Some(7), Some(&m1), 7, Some("m1")));
        assert!(caller_owns(Some(7), Some(&m1), 7, None));
        // Overlay A 提交显示器 B
        assert!(!caller_owns(Some(7), Some(&m1), 7, Some("m2")));
        // 别的会话（上一次截图留下的页面）
        assert!(!caller_owns(Some(8), Some(&m1), 7, None));
        // 面板 / pinned / 设置窗口：没有绑定任何会话
        assert!(!caller_owns(None, None, 7, None));
        assert!(!caller_owns(None, None, 7, Some("m1")));
    }
}
