//! Provider 与 Key 命令。没有读取 Key 的接口；任何响应中都不得出现 Key。

use std::sync::Arc;

use tauri::{AppHandle, State};
use wisp_types::{ModelEntry, Protocol, Provider, ProviderTemplate, ProviderTestReport, SettingsSnapshot, UrlPreview};

use crate::error::IpcError;
use crate::ipc::validate::validate_provider;
use crate::ipc::{MAX_KEY_BYTES, MAX_QUERY_BYTES, check_id, check_len};
use crate::settings::service::{
    TEST_NOTICE_KEYCHAIN_UNREADABLE, probe_key, provider_models as provider_models_merged, run_blocking,
};
use crate::state::AppState;

/// 16 个厂商模板
#[tauri::command(rename_all = "snake_case")]
pub async fn provider_templates() -> Result<Vec<ProviderTemplate>, IpcError> {
    Ok(wisp_llm::templates())
}

/// 由模板生成唯一 id 的 Provider（此时尚未保存）
#[tauri::command(rename_all = "snake_case")]
pub async fn provider_new(app_state: State<'_, AppState>, template_id: String) -> Result<Provider, IpcError> {
    check_id("template_id", &template_id)?;
    app_state.settings.provider_new(&template_id)
}

/// 删除 Provider（先删钥匙串条目，再从设置中移除；同时清掉指向它的默认模型与风格绑定）。钥匙串删除失败不阻止删除
/// Provider：条目留待之后清理（见 `SettingsService::provider_delete`）
#[tauri::command(rename_all = "snake_case")]
pub async fn provider_delete(
    app: AppHandle,
    app_state: State<'_, AppState>,
    provider_id: String,
) -> Result<SettingsSnapshot, IpcError> {
    check_id("provider_id", &provider_id)?;
    let settings = Arc::clone(&app_state.settings);
    run_blocking(move || settings.provider_delete(&provider_id).map(|_| ())).await?;
    Ok(app_state.settings.snapshot(crate::app::runtime_info(&app, &app_state)))
}

/// 测试连接（草稿也可以测）。`key`：草稿里还没保存的 Key，只用于这一次请求、不经钥匙串；null 时按
/// provider.id 从钥匙串读取（读不出来时见 [`crate::settings::service::key_for_probe`]）。探针模型先取默认翻译模型
/// （`SettingsService::provider_for_probe` 把它排到 models 首位）
#[tauri::command(rename_all = "snake_case")]
pub async fn provider_test(
    app_state: State<'_, AppState>,
    provider: Provider,
    key: Option<String>,
) -> Result<ProviderTestReport, IpcError> {
    validate_provider(&provider)?;
    check_opt_key(key.as_deref())?;
    let (key, unreadable) = probe_key(Arc::clone(&app_state.settings), &provider, key).await?;
    let provider = app_state.settings.provider_for_probe(provider);
    let mut report = app_state.llm.test_connection(&provider, key.as_ref()).await;
    if unreadable {
        report.notice = Some(TEST_NOTICE_KEYCHAIN_UNREADABLE.to_owned());
    }
    Ok(report)
}

/// 拉取模型列表并与 provider.models 合并；`key` 的含义同 `provider_test`
#[tauri::command(rename_all = "snake_case")]
pub async fn provider_models(
    app_state: State<'_, AppState>,
    provider: Provider,
    key: Option<String>,
) -> Result<Vec<ModelEntry>, IpcError> {
    validate_provider(&provider)?;
    check_opt_key(key.as_deref())?;
    provider_models_merged(Arc::clone(&app_state.settings), &app_state.llm, provider, key).await
}

/// 草稿 Key（≤ 8 KB，与 `secret_set` 同一上限）
fn check_opt_key(key: Option<&str>) -> Result<(), IpcError> {
    key.map_or(Ok(()), |k| check_len("key", k, MAX_KEY_BYTES))
}

/// 最终请求 URL 预览
#[tauri::command(rename_all = "snake_case")]
pub async fn llm_preview_url(
    protocol: Protocol,
    base_url: String,
    model_id: Option<String>,
) -> Result<UrlPreview, IpcError> {
    check_len("base_url", &base_url, MAX_QUERY_BYTES)?;
    if let Some(model_id) = &model_id {
        check_id("model_id", model_id)?;
    }
    Ok(wisp_llm::url_preview(protocol, &base_url, model_id.as_deref()))
}

/// 写入 Key（钥匙串访问可能弹出系统授权框，在阻塞线程池里执行）
#[tauri::command(rename_all = "snake_case")]
pub async fn secret_set(app_state: State<'_, AppState>, provider_id: String, key: String) -> Result<(), IpcError> {
    check_id("provider_id", &provider_id)?;
    check_len("key", &key, MAX_KEY_BYTES)?;
    let settings = Arc::clone(&app_state.settings);
    run_blocking(move || {
        settings.keychain().set(&provider_id, &key)?;
        // 用户刚授权访问钥匙串：顺带清理之前删 Provider 时没删掉的条目
        settings.purge_orphan_keys();
        Ok(())
    })
    .await
}

/// 删除 Key
#[tauri::command(rename_all = "snake_case")]
pub async fn secret_delete(app_state: State<'_, AppState>, provider_id: String) -> Result<(), IpcError> {
    check_id("provider_id", &provider_id)?;
    let settings = Arc::clone(&app_state.settings);
    run_blocking(move || {
        settings.keychain().delete(&provider_id)?;
        settings.purge_orphan_keys();
        Ok(())
    })
    .await
}

/// 清除某个模型的能力缓存
#[tauri::command(rename_all = "snake_case")]
pub async fn model_caps_reset(
    app_state: State<'_, AppState>,
    provider_id: String,
    model_id: String,
) -> Result<(), IpcError> {
    check_id("provider_id", &provider_id)?;
    check_id("model_id", &model_id)?;
    let settings = Arc::clone(&app_state.settings);
    run_blocking(move || settings.caps_reset(&provider_id, &model_id)).await
}
