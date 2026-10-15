//! 应用命令：权限、应用信息、引导完成、外观、打开路径 / 链接、重启 / 退出、诊断导出。

use tauri::{AppHandle, State};
use wisp_types::{AppMeta, AppearanceState, PathKind, PermissionKind, PermissionSet, WindowTarget};

use crate::error::IpcError;
use crate::ipc::validate::validate_opt_window_target;
use crate::ipc::{MAX_QUERY_BYTES, check_len};
use crate::settings::service::run_blocking;
use crate::state::AppState;

/// 权限状态（绝不弹系统对话框；含更新后授权失效的 `lost_after_update`）。查询 TCC、可能写 state.json：放到阻塞线程池
#[tauri::command(rename_all = "snake_case")]
pub async fn permissions_status(app: AppHandle) -> Result<PermissionSet, IpcError> {
    run_blocking(move || Ok(crate::app::permissions::status(&app))).await
}

/// 请求权限，返回最新状态
#[tauri::command(rename_all = "snake_case")]
pub async fn permissions_request(
    app: AppHandle,
    app_state: State<'_, AppState>,
    kind: PermissionKind,
) -> Result<PermissionSet, IpcError> {
    app_state.platform.permissions.request(kind)?;
    run_blocking(move || Ok(crate::app::permissions::status(&app))).await
}

/// 打开系统设置的对应面板
#[tauri::command(rename_all = "snake_case")]
pub async fn permissions_open(app_state: State<'_, AppState>, kind: PermissionKind) -> Result<(), IpcError> {
    Ok(app_state.platform.permissions.open_settings(kind)?)
}

/// 引导页可见时每 2 s 轮询一次，变化时 emit `permissions:changed`；隐藏后传 false 立即停止
#[tauri::command(rename_all = "snake_case")]
pub async fn permissions_watch(app: AppHandle, active: bool) -> Result<(), IpcError> {
    crate::app::permissions::watch(&app, active);
    Ok(())
}

/// 应用元信息
#[tauri::command(rename_all = "snake_case")]
pub async fn app_info(app: AppHandle, app_state: State<'_, AppState>) -> Result<AppMeta, IpcError> {
    Ok(crate::app::app_meta(&app, &app_state))
}

/// 写 state.json 的 onboarding_done = true 与 onboarding_version
#[tauri::command(rename_all = "snake_case")]
pub async fn onboarding_complete(app_state: State<'_, AppState>) -> Result<(), IpcError> {
    let settings = std::sync::Arc::clone(&app_state.settings);
    crate::settings::service::run_blocking(move || settings.complete_onboarding()).await
}

/// 当前外观（读系统明暗 / 强调色要与主线程同步往返，放到阻塞线程池）
#[tauri::command(rename_all = "snake_case")]
pub async fn appearance_get(app: AppHandle) -> Result<AppearanceState, IpcError> {
    crate::settings::service::run_blocking(move || Ok(crate::app::appearance::current(&app))).await
}

/// 在文件管理器中打开配置 / 数据 / 日志目录
#[tauri::command(rename_all = "snake_case")]
pub async fn open_path(app: AppHandle, kind: PathKind) -> Result<(), IpcError> {
    crate::app::files::open_path(&app, kind)
}

/// 用默认浏览器打开链接（只允许 http / https）
#[tauri::command(rename_all = "snake_case")]
pub async fn open_url(app: AppHandle, url: String) -> Result<(), IpcError> {
    check_len("url", &url, MAX_QUERY_BYTES)?;
    crate::app::files::open_url(&app, &url)
}

/// 重启应用。`resume`：重启后直接打开的窗口与路由（引导的某一步、设置的某个分组），
/// 由 Rust 持久化（state.json）、下次启动时取走一次（5 分钟内有效），取代前端的 localStorage 交接（`onboarding/resume.ts`）。
/// 持久化（同步写进 state.json 之后才重启）与下次启动时的消费在 `app::relaunch`
#[tauri::command(rename_all = "snake_case")]
pub async fn app_relaunch(app: AppHandle, resume: Option<WindowTarget>) -> Result<(), IpcError> {
    validate_opt_window_target(resume.as_ref())?;
    crate::app::relaunch::relaunch(app, resume).await
}

/// 退出应用
#[tauri::command(rename_all = "snake_case")]
pub async fn app_quit(app: AppHandle) -> Result<(), IpcError> {
    app.exit(0);
    Ok(())
}

/// 诊断导出（脱敏）
#[tauri::command(rename_all = "snake_case")]
pub async fn diagnostics_export(app: AppHandle) -> Result<Option<String>, IpcError> {
    crate::app::diagnostics::export(&app).await
}
