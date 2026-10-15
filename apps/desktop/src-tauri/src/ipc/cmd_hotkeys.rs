//! 快捷键命令。只做校验与分发；会与主线程同步往返的调用（插件的注册 / 注销、macOS 系统热键与输入源、进程
//! 枚举）一律经 [`blocking`] 放到阻塞线程池，不占 tokio worker。

use std::sync::Arc;

use tauri::State;
use wisp_types::{Binding, ConflictReport, HotkeyStatus, HotkeyTarget, KeyboardAdvice, NeighborHit};

use crate::error::IpcError;
use crate::hotkeys::HotkeyService;
use crate::ipc::validate::validate_hotkey_target;
use crate::ipc::{MAX_QUERY_BYTES, check_len};
use crate::state::AppState;

/// 在阻塞线程池里调用快捷键服务
async fn blocking<T: Send + 'static>(
    hotkeys: &Arc<HotkeyService>,
    f: impl FnOnce(&HotkeyService) -> T + Send + 'static,
) -> Result<T, IpcError> {
    let hotkeys = Arc::clone(hotkeys);
    tauri::async_runtime::spawn_blocking(move || f(&hotkeys))
        .await
        .map_err(|e| IpcError::internal(format!("hotkey task failed: {e}")))
}

/// 全部快捷键状态（有回退或失败的项时顺便重试注册一次）
#[tauri::command(rename_all = "snake_case")]
pub async fn hotkey_status(app_state: State<'_, AppState>) -> Result<Vec<HotkeyStatus>, IpcError> {
    blocking(&app_state.hotkeys, HotkeyService::status_with_retry).await
}

/// 录制开始：注销全部全局键
#[tauri::command(rename_all = "snake_case")]
pub async fn hotkey_record_begin(app_state: State<'_, AppState>) -> Result<(), IpcError> {
    blocking(&app_state.hotkeys, HotkeyService::record_begin).await
}

/// 录制结束：恢复全部全局键
#[tauri::command(rename_all = "snake_case")]
pub async fn hotkey_record_end(app_state: State<'_, AppState>) -> Result<(), IpcError> {
    blocking(&app_state.hotkeys, HotkeyService::record_end).await
}

/// 冲突检查
#[tauri::command(rename_all = "snake_case")]
pub async fn hotkey_check(
    app_state: State<'_, AppState>,
    target: HotkeyTarget,
    combo: String,
) -> Result<ConflictReport, IpcError> {
    validate_hotkey_target(&target)?;
    check_len("combo", &combo, MAX_QUERY_BYTES)?;
    blocking(&app_state.hotkeys, move |h| h.check(&target, &combo)).await
}

/// 设置绑定：先注册新键、再注销旧键
#[tauri::command(rename_all = "snake_case")]
pub async fn hotkey_set(
    app_state: State<'_, AppState>,
    target: HotkeyTarget,
    binding: Binding,
    force: bool,
) -> Result<Vec<HotkeyStatus>, IpcError> {
    validate_hotkey_target(&target)?;
    check_len("binding", binding.as_str(), MAX_QUERY_BYTES)?;
    blocking(&app_state.hotkeys, move |h| h.set(target, binding, force)).await?
}

/// 全部恢复默认
#[tauri::command(rename_all = "snake_case")]
pub async fn hotkey_reset_defaults(app_state: State<'_, AppState>) -> Result<Vec<HotkeyStatus>, IpcError> {
    blocking(&app_state.hotkeys, HotkeyService::reset_defaults).await
}

/// 测试按钮：开始
#[tauri::command(rename_all = "snake_case")]
pub async fn hotkey_test_begin(app_state: State<'_, AppState>, target: HotkeyTarget) -> Result<(), IpcError> {
    validate_hotkey_target(&target)?;
    app_state.hotkeys.test_begin(target);
    Ok(())
}

/// 测试按钮：结束
#[tauri::command(rename_all = "snake_case")]
pub async fn hotkey_test_end(app_state: State<'_, AppState>) -> Result<(), IpcError> {
    app_state.hotkeys.test_end();
    Ok(())
}

/// 邻居应用占用的组合键（每次调用重新探测一次）
#[tauri::command(rename_all = "snake_case")]
pub async fn neighbor_apps(app_state: State<'_, AppState>) -> Result<Vec<NeighborHit>, IpcError> {
    blocking(&app_state.hotkeys, HotkeyService::neighbors).await
}

/// 键盘布局建议
#[tauri::command(rename_all = "snake_case")]
pub async fn keyboard_advice(app_state: State<'_, AppState>) -> Result<KeyboardAdvice, IpcError> {
    blocking(&app_state.hotkeys, HotkeyService::keyboard_advice).await
}
