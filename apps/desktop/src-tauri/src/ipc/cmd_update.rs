//! 更新命令。

use tauri::AppHandle;
use wisp_types::UpdateStatus;

use crate::error::IpcError;

/// 立即检查更新
#[tauri::command(rename_all = "snake_case")]
pub async fn update_check(app: AppHandle) -> Result<UpdateStatus, IpcError> {
    crate::updater::check(&app).await
}

/// 下载并安装
#[tauri::command(rename_all = "snake_case")]
pub async fn update_install(app: AppHandle) -> Result<(), IpcError> {
    crate::updater::install(&app).await
}

/// 当前状态
#[tauri::command(rename_all = "snake_case")]
pub async fn update_status(app: AppHandle) -> Result<UpdateStatus, IpcError> {
    crate::updater::status(&app)
}
