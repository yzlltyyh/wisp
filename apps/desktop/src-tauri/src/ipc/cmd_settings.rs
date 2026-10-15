//! 设置、风格与术语表命令。写入类命令在阻塞线程池里执行（原子写含 fsync，不占 tokio worker）。

use std::sync::Arc;

use tauri::{AppHandle, State};
use wisp_types::{GlossaryEntry, PromptPreview, SettingsSnapshot, Style};

use crate::error::IpcError;
use crate::ipc::validate::{validate_glossary, validate_settings_patch, validate_style, validate_style_ids};
use crate::ipc::{MAX_TEXT_BYTES, check_id, check_len};
use crate::settings::service::run_blocking;
use crate::state::AppState;

/// 完整设置快照
#[tauri::command(rename_all = "snake_case")]
pub async fn settings_get(app: AppHandle, app_state: State<'_, AppState>) -> Result<SettingsSnapshot, IpcError> {
    Ok(app_state.settings.snapshot(crate::app::runtime_info(&app, &app_state)))
}

/// JSON Merge Patch（数组整体替换）
#[tauri::command(rename_all = "snake_case")]
pub async fn settings_patch(
    app: AppHandle,
    app_state: State<'_, AppState>,
    patch: serde_json::Value,
) -> Result<SettingsSnapshot, IpcError> {
    validate_settings_patch(&patch)?;
    let settings = Arc::clone(&app_state.settings);
    run_blocking(move || settings.patch(patch).map(|_| ())).await?;
    Ok(app_state.settings.snapshot(crate::app::runtime_info(&app, &app_state)))
}

/// 全部风格
#[tauri::command(rename_all = "snake_case")]
pub async fn styles_list(app_state: State<'_, AppState>) -> Result<Vec<Style>, IpcError> {
    Ok((*app_state.settings.styles()).clone())
}

/// 保存风格
#[tauri::command(rename_all = "snake_case")]
pub async fn style_save(app_state: State<'_, AppState>, style: Style) -> Result<Vec<Style>, IpcError> {
    validate_style(&style)?;
    let settings = Arc::clone(&app_state.settings);
    run_blocking(move || settings.style_save(style).map(|s| (*s).clone())).await
}

/// 删除自定义风格
#[tauri::command(rename_all = "snake_case")]
pub async fn style_delete(app_state: State<'_, AppState>, id: String) -> Result<Vec<Style>, IpcError> {
    check_id("id", &id)?;
    let settings = Arc::clone(&app_state.settings);
    run_blocking(move || settings.style_delete(&id).map(|s| (*s).clone())).await
}

/// 还原内置风格
#[tauri::command(rename_all = "snake_case")]
pub async fn style_reset(app_state: State<'_, AppState>, id: String) -> Result<Vec<Style>, IpcError> {
    check_id("id", &id)?;
    let settings = Arc::clone(&app_state.settings);
    run_blocking(move || settings.style_reset(&id).map(|s| (*s).clone())).await
}

/// 调整顺序
#[tauri::command(rename_all = "snake_case")]
pub async fn styles_reorder(app_state: State<'_, AppState>, ids: Vec<String>) -> Result<Vec<Style>, IpcError> {
    validate_style_ids(&ids)?;
    let settings = Arc::clone(&app_state.settings);
    run_blocking(move || settings.styles_reorder(ids).map(|s| (*s).clone())).await
}

/// 预览最终提示词（调用 wisp-prompts 的 build）
#[tauri::command(rename_all = "snake_case")]
pub async fn style_preview(
    app_state: State<'_, AppState>,
    style: Style,
    sample: String,
) -> Result<PromptPreview, IpcError> {
    validate_style(&style)?;
    check_len("sample", &sample, MAX_TEXT_BYTES)?;
    Ok(app_state.settings.style_preview(&style, &sample))
}

/// 术语表
#[tauri::command(rename_all = "snake_case")]
pub async fn glossary_get(app_state: State<'_, AppState>) -> Result<Vec<GlossaryEntry>, IpcError> {
    Ok((*app_state.settings.glossary_entries()).clone())
}

/// 替换术语表
#[tauri::command(rename_all = "snake_case")]
pub async fn glossary_set(app_state: State<'_, AppState>, entries: Vec<GlossaryEntry>) -> Result<(), IpcError> {
    validate_glossary(&entries)?;
    let settings = Arc::clone(&app_state.settings);
    run_blocking(move || settings.glossary_set(entries)).await
}

/// 从文件导入术语表（原生文件对话框；JSON / CSV / TSV），返回导入条数；用户取消时为 0
#[tauri::command(rename_all = "snake_case")]
pub async fn glossary_import(app: AppHandle) -> Result<u32, IpcError> {
    crate::app::files::glossary_import(&app).await
}

/// 导出术语表（原生文件对话框；`.csv` 为 CSV，其余为 JSON）；用户取消时返回 None
#[tauri::command(rename_all = "snake_case")]
pub async fn glossary_export(app: AppHandle) -> Result<Option<String>, IpcError> {
    crate::app::files::glossary_export(&app).await
}
