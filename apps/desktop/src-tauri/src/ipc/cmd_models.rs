//! 离线模型命令。只做参数校验并转发给 `ModelsService`。
//!
//! 参数都是无字段枚举（模型 id、运行时 id），反序列化成功即合法。选用哪个模型走 `settings_patch`
//! （`offline.ocr_model` / `offline.translate_model`）；PaddleOCR 云端的 Token 走 `secret_set` / `secret_delete`。
//! 下载进度不走 Channel（下载的生命周期长于设置窗口，多个窗口都要看），由 `offline_models:changed` 事件推送。

use std::sync::Arc;

use tauri::State;
use wisp_types::{LocalModelId, LocalModelsSnapshot, LocalRuntimeId};

use crate::error::IpcError;
use crate::state::AppState;

/// 设置页的完整快照（第一次调用时才读模型库存）
#[tauri::command(rename_all = "snake_case")]
pub async fn offline_models_get(app_state: State<'_, AppState>) -> Result<LocalModelsSnapshot, IpcError> {
    app_state.models.get().await
}

/// 开始（或继续）下载一个模型，需要时一并下载运行时；已在下载或排队时什么也不做。进度经事件推送。
/// `select`（「下载并使用」）：下载并安装成功后按模型类别选用它（写 `offline.ocr_model` / `offline.translate_model`），
/// 下载期间原来的模型照常可用；已经装好时立即选用
#[tauri::command(rename_all = "snake_case")]
pub async fn offline_model_download(
    app_state: State<'_, AppState>,
    model: LocalModelId,
    select: bool,
) -> Result<(), IpcError> {
    let models = Arc::clone(&app_state.models);
    models.download(model, select).await
}

/// 暂停下载（保留已下载的部分，之后可以继续）
#[tauri::command(rename_all = "snake_case")]
pub async fn offline_model_pause(app_state: State<'_, AppState>, model: LocalModelId) -> Result<(), IpcError> {
    app_state.models.pause(model).await
}

/// 取消下载（删除已下载的部分）
#[tauri::command(rename_all = "snake_case")]
pub async fn offline_model_cancel(app_state: State<'_, AppState>, model: LocalModelId) -> Result<(), IpcError> {
    app_state.models.cancel(model).await
}

/// 删除一个已下载的模型（先释放它占用的内存）；它正被选用时同时把对应的 `offline.*_model` 置 null
#[tauri::command(rename_all = "snake_case")]
pub async fn offline_model_delete(app_state: State<'_, AppState>, model: LocalModelId) -> Result<(), IpcError> {
    app_state.models.delete(model).await
}

/// 删除一个推理运行时；仍有已安装的模型依赖它时返回 `invalid_argument`
#[tauri::command(rename_all = "snake_case")]
pub async fn offline_runtime_delete(app_state: State<'_, AppState>, runtime: LocalRuntimeId) -> Result<(), IpcError> {
    app_state.models.delete_runtime(runtime).await
}

/// 立即释放所有空闲模型占用的内存（「释放内存」按钮）
#[tauri::command(rename_all = "snake_case")]
pub async fn offline_release(app_state: State<'_, AppState>) -> Result<(), IpcError> {
    app_state.models.release().await
}

/// 在访达 / 资源管理器 / 文件管理器里打开模型目录
#[tauri::command(rename_all = "snake_case")]
pub async fn offline_reveal(app_state: State<'_, AppState>) -> Result<(), IpcError> {
    app_state.models.reveal().await
}

/// 重新检测显卡，返回新的快照
#[tauri::command(rename_all = "snake_case")]
pub async fn offline_devices_refresh(app_state: State<'_, AppState>) -> Result<LocalModelsSnapshot, IpcError> {
    app_state.models.devices_refresh().await
}
