//! Provider 与 Key 命令。没有读取 Key 的接口；任何响应中都不得出现 Key。

use std::sync::Arc;

use tauri::{AppHandle, State};
use wisp_types::{
    ModelEntry, ProbeResult, Protocol, Provider, ProviderTemplate, ProviderTestReport, SettingsSnapshot, TestVerdict,
    UrlPreview,
};

use crate::error::IpcError;
use crate::ipc::validate::validate_provider;
use crate::ipc::{MAX_KEY_BYTES, MAX_LIST_LEN, MAX_QUERY_BYTES, check_id, check_len, check_list_len};
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

/// 保存一个已有的服务：只替换这一项，其他服务按 Rust 里最新的内容保留（设置窗口的旧快照不会覆盖别处的改动）。
/// 该服务已被删除（在其他设备上或配置文件里）时 `not_found`，不把它加回来
#[tauri::command(rename_all = "snake_case")]
pub async fn provider_save(
    app: AppHandle,
    app_state: State<'_, AppState>,
    provider: Provider,
) -> Result<SettingsSnapshot, IpcError> {
    validate_provider(&provider)?;
    let settings = Arc::clone(&app_state.settings);
    run_blocking(move || settings.provider_save(provider).map(|_| ())).await?;
    Ok(app_state.settings.snapshot(crate::app::runtime_info(&app, &app_state)))
}

/// 添加服务：追加到末尾（同 id 的服务此时已经存在时替换它），其他服务不动
#[tauri::command(rename_all = "snake_case")]
pub async fn provider_add(
    app: AppHandle,
    app_state: State<'_, AppState>,
    provider: Provider,
) -> Result<SettingsSnapshot, IpcError> {
    validate_provider(&provider)?;
    let settings = Arc::clone(&app_state.settings);
    run_blocking(move || settings.provider_add(provider).map(|_| ())).await?;
    Ok(app_state.settings.snapshot(crate::app::runtime_info(&app, &app_state)))
}

/// 调整服务的顺序：只重排、不改内容；未知 id 忽略，缺少的服务按原顺序排在后面
#[tauri::command(rename_all = "snake_case")]
pub async fn providers_reorder(
    app: AppHandle,
    app_state: State<'_, AppState>,
    ids: Vec<String>,
) -> Result<SettingsSnapshot, IpcError> {
    check_list_len("ids", ids.len(), MAX_LIST_LEN)?;
    ids.iter().try_for_each(|id| check_id("ids[]", id))?;
    let settings = Arc::clone(&app_state.settings);
    run_blocking(move || settings.providers_reorder(&ids).map(|_| ())).await?;
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
    let mut report = if provider.protocol == Protocol::PaddleOcr {
        // 文字识别服务不是 LLM：只验证地址与 Token（不消耗识别额度）
        paddle_ocr_test(&provider, key).await
    } else {
        let provider = app_state.settings.provider_for_probe(provider);
        app_state.llm.test_connection(&provider, key.as_ref()).await
    };
    if unreadable {
        report.notice = Some(TEST_NOTICE_KEYCHAIN_UNREADABLE.to_owned());
    }
    Ok(report)
}

/// PaddleOCR 云端的连接测试，按测试报告的形状给出：没有模型列表这一项（`models` 与 `stream` 是同一次探测的结果）
async fn paddle_ocr_test(provider: &Provider, key: Option<wisp_llm::Secret>) -> ProviderTestReport {
    let started = std::time::Instant::now();
    let token = key.and_then(|k| wisp_local::ocr::cloud::Token::new(k.expose().to_owned()));
    let result = match wisp_local::ocr::cloud::CloudClient::new() {
        Ok(client) => client.test(&provider.base_url, token).await,
        Err(e) => Err(e.to_llm_error()),
    };
    let probe = ProbeResult {
        ok: result.is_ok(),
        status: None,
        elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
        // 只给 host（与 `LlmError.url` 同一口径：不回显完整地址）
        url: url_host(&provider.base_url),
        error: result.err(),
    };
    ProviderTestReport {
        overall: if probe.ok { TestVerdict::Ok } else { TestVerdict::Failed },
        models: probe.clone(),
        stream: probe,
        model_used: None,
        fetched_models: Vec::new(),
        notice: None,
    }
}

/// 地址的 scheme + host（无法解析时为空）
fn url_host(base_url: &str) -> String {
    url::Url::parse(base_url.trim())
        .ok()
        .and_then(|u| u.host_str().map(|h| format!("{}://{h}", u.scheme())))
        .unwrap_or_default()
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
    if provider.protocol == Protocol::PaddleOcr {
        // 文字识别服务没有模型列表接口；前端隐藏了这个按钮，模型表是预置的、可手动增删
        return Err(IpcError::invalid_argument(
            "this provider is a text recognition service and has no model list",
        ));
    }
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

/// 写入 Key（钥匙串访问可能弹出系统授权框，在阻塞线程池里执行）。用户刚授权访问钥匙串：顺带清理之前删 Provider
/// 时没删掉的条目（见 `SettingsService::secret_set`）
#[tauri::command(rename_all = "snake_case")]
pub async fn secret_set(app_state: State<'_, AppState>, provider_id: String, key: String) -> Result<(), IpcError> {
    check_id("provider_id", &provider_id)?;
    check_len("key", &key, MAX_KEY_BYTES)?;
    let settings = Arc::clone(&app_state.settings);
    run_blocking(move || settings.secret_set(&provider_id, &key)).await
}

/// 删除 Key
#[tauri::command(rename_all = "snake_case")]
pub async fn secret_delete(app_state: State<'_, AppState>, provider_id: String) -> Result<(), IpcError> {
    check_id("provider_id", &provider_id)?;
    let settings = Arc::clone(&app_state.settings);
    run_blocking(move || settings.secret_delete(&provider_id)).await
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_urls_keep_only_the_host() {
        assert_eq!(
            url_host("https://paddleocr.aistudio-app.com/api/v2/ocr/jobs?x=1"),
            "https://paddleocr.aistudio-app.com"
        );
        assert_eq!(url_host("http://192.168.1.5:8080/ocr"), "http://192.168.1.5");
        assert_eq!(url_host("not a url"), "");
    }
}
