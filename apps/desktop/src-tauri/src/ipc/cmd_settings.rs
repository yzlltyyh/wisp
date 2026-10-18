//! 设置、风格与术语表命令。写入类命令在阻塞线程池里执行（原子写含 fsync，不占 tokio worker）。

use std::sync::Arc;

use tauri::{AppHandle, State};
use wisp_types::{AppKeymap, ExcludedApp, GlossaryEntry, OsKind, PromptPreview, SettingsSnapshot, Style};

use crate::error::IpcError;
use crate::ipc::validate::{
    MAX_LABEL_BYTES, validate_app_keymap, validate_excluded_app, validate_glossary, validate_settings_patch,
    validate_style, validate_style_ids,
};
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
    check_sync_patch(&patch)?;
    let settings = Arc::clone(&app_state.settings);
    run_blocking(move || settings.patch(patch).map(|_| ())).await?;
    Ok(app_state.settings.snapshot(crate::app::runtime_info(&app, &app_state)))
}

/// 同步的开关、地址与账号只由连接 / 断开同步的命令写入（要与凭据存储里的条目保持一致）；设置页经补丁只能改
/// `sync.auto` 与 `sync.content`。`sync: null`（整组恢复默认）同样会改到它们，一并拒绝
fn check_sync_patch(patch: &serde_json::Value) -> Result<(), IpcError> {
    let Some(sync) = patch.get("sync") else {
        return Ok(());
    };
    let Some(fields) = sync.as_object() else {
        return Err(IpcError::invalid_argument("sync can only be changed field by field"));
    };
    for field in ["enabled", "server_url", "username"] {
        if fields.contains_key(field) {
            return Err(IpcError::invalid_argument(format!(
                "sync.{field} is managed by the sync commands"
            )));
        }
    }
    Ok(())
}

/// 加入或更新一个剪贴板排除应用：按系统与应用 id 只改这一项（设置窗口里的旧名单不会把别处刚加进来的应用写回去）
#[tauri::command(rename_all = "snake_case")]
pub async fn excluded_app_upsert(
    app: AppHandle,
    app_state: State<'_, AppState>,
    entry: ExcludedApp,
) -> Result<SettingsSnapshot, IpcError> {
    validate_excluded_app(&entry)?;
    let settings = Arc::clone(&app_state.settings);
    run_blocking(move || settings.excluded_app_upsert(entry).map(|_| ())).await?;
    Ok(app_state.settings.snapshot(crate::app::runtime_info(&app, &app_state)))
}

/// 从剪贴板排除名单移除一个应用（已经不在了视为成功）
#[tauri::command(rename_all = "snake_case")]
pub async fn excluded_app_remove(
    app: AppHandle,
    app_state: State<'_, AppState>,
    os: OsKind,
    app_id: String,
) -> Result<SettingsSnapshot, IpcError> {
    check_id("app_id", &app_id)?;
    let settings = Arc::clone(&app_state.settings);
    run_blocking(move || settings.excluded_app_remove(os, &app_id).map(|_| ())).await?;
    Ok(app_state.settings.snapshot(crate::app::runtime_info(&app, &app_state)))
}

/// 加入或更新一条应用的复制 / 粘贴键：按系统与应用 id 只改这一项
#[tauri::command(rename_all = "snake_case")]
pub async fn app_keymap_upsert(
    app: AppHandle,
    app_state: State<'_, AppState>,
    entry: AppKeymap,
) -> Result<SettingsSnapshot, IpcError> {
    validate_app_keymap(&entry)?;
    let settings = Arc::clone(&app_state.settings);
    run_blocking(move || settings.app_keymap_upsert(entry).map(|_| ())).await?;
    Ok(app_state.settings.snapshot(crate::app::runtime_info(&app, &app_state)))
}

/// 移除一条应用的复制 / 粘贴键（已经不在了视为成功）
#[tauri::command(rename_all = "snake_case")]
pub async fn app_keymap_remove(
    app: AppHandle,
    app_state: State<'_, AppState>,
    os: OsKind,
    app_id: String,
) -> Result<SettingsSnapshot, IpcError> {
    check_id("app_id", &app_id)?;
    let settings = Arc::clone(&app_state.settings);
    run_blocking(move || settings.app_keymap_remove(os, &app_id).map(|_| ())).await?;
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

/// 写入一条术语（只改这一条；`previous_source` 为这一行改动前的原文，改了原文时用它找到原来的条目）
#[tauri::command(rename_all = "snake_case")]
pub async fn glossary_upsert(
    app_state: State<'_, AppState>,
    entry: GlossaryEntry,
    previous_source: Option<String>,
) -> Result<(), IpcError> {
    validate_glossary(std::slice::from_ref(&entry))?;
    check_len(
        "previous_source",
        previous_source.as_deref().unwrap_or_default(),
        MAX_LABEL_BYTES,
    )?;
    let settings = Arc::clone(&app_state.settings);
    run_blocking(move || settings.glossary_upsert(entry, previous_source.as_deref())).await
}

/// 删除一条术语（按原文；已经不在了视为成功）
#[tauri::command(rename_all = "snake_case")]
pub async fn glossary_remove(app_state: State<'_, AppState>, source: String) -> Result<(), IpcError> {
    check_len("source", &source, MAX_LABEL_BYTES)?;
    let settings = Arc::clone(&app_state.settings);
    run_blocking(move || settings.glossary_remove(&source)).await
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::check_sync_patch;
    use crate::error::IpcErrorCode;

    #[test]
    fn settings_patch_cannot_touch_the_sync_account() {
        for ok in [
            json!({"appearance": {"font_size": 15}}),
            json!({"sync": {"auto": false}}),
            json!({"sync": {"content": {"images": false}}}),
        ] {
            assert!(check_sync_patch(&ok).is_ok(), "{ok}");
        }
        for bad in [
            json!({"sync": {"enabled": true}}),
            json!({"sync": {"server_url": "https://dav.example.com"}}),
            json!({"sync": {"username": "someone", "auto": true}}),
            json!({"sync": null}),
            json!({"sync": true}),
        ] {
            assert_eq!(
                check_sync_patch(&bad).expect_err("rejected").code,
                IpcErrorCode::InvalidArgument,
                "{bad}"
            );
        }
    }
}
