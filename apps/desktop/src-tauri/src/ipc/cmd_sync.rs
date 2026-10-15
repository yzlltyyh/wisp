//! 多设备同步命令。只做参数校验并转发给 `SyncService`。
//!
//! WebDAV 密码与同步密码只从前端传进来（连接、测试连接、清除服务器数据时），没有读回它们或主密钥的接口；
//! 错误消息是界面文案的 key，不带地址、账号或服务器返回的内容。

use std::sync::Arc;

use tauri::State;
use wisp_types::{SyncAccount, SyncConnectResult, SyncContent, SyncDevice, SyncProbe, SyncStatus};

use crate::error::IpcError;
use crate::ipc::validate::{validate_opt_sync_account, validate_sync_account};
use crate::ipc::{MAX_KEY_BYTES, check_len};
use crate::state::AppState;
use crate::sync_svc::SyncService;

/// 可选的密码（留空 = 用钥匙串里与地址和账号匹配的密码）
fn check_opt_secret(field: &str, value: Option<&str>) -> Result<(), IpcError> {
    value.map_or(Ok(()), |v| check_len(field, v, MAX_KEY_BYTES))
}

/// 当前同步状态（不含地址、账号与设备名）
#[tauri::command(rename_all = "snake_case")]
pub async fn sync_status(app_state: State<'_, AppState>) -> Result<SyncStatus, IpcError> {
    Ok(app_state.sync.status())
}

/// 已知的设备（不联网；同步未开启时为空）。只在同步页打开时调用
#[tauri::command(rename_all = "snake_case")]
pub async fn sync_devices(app_state: State<'_, AppState>) -> Result<Vec<SyncDevice>, IpcError> {
    app_state.sync.devices().await
}

/// 测试连接：认证、读写与服务器上已有数据的概况。连接与服务器的错误放进报告，不抛出；地址不合法时抛
/// `invalid_argument`
#[tauri::command(rename_all = "snake_case")]
pub async fn sync_probe(
    app_state: State<'_, AppState>,
    account: SyncAccount,
    password: Option<String>,
) -> Result<SyncProbe, IpcError> {
    validate_sync_account(&account)?;
    check_opt_secret("password", password.as_deref())?;
    app_state.sync.probe(account, password).await
}

/// 连接（创建、加入或恢复）。同步密码不对抛 `conflict`（`errors.sync.passphrase`）；新设置的同步密码不满足规则抛
/// `invalid_argument`（`errors.sync.passphrase_weak` / `errors.sync.passphrase_reused`）——是否在新建由服务按服务器上的
/// 情况判断，所以规则不在这里检查
#[tauri::command(rename_all = "snake_case")]
pub async fn sync_connect(
    app_state: State<'_, AppState>,
    account: SyncAccount,
    password: Option<String>,
    passphrase: String,
    content: SyncContent,
) -> Result<SyncConnectResult, IpcError> {
    validate_sync_account(&account)?;
    check_opt_secret("password", password.as_deref())?;
    check_len("passphrase", &passphrase, MAX_KEY_BYTES)?;
    let sync = Arc::clone(&app_state.sync);
    sync.connect(account, password, passphrase, content).await
}

/// 立即同步：发起（或并入正在进行的）一轮，立即返回；进度经 `sync:status` 推送
#[tauri::command(rename_all = "snake_case")]
pub async fn sync_now(app_state: State<'_, AppState>) -> Result<SyncStatus, IpcError> {
    app_state.sync.sync_now()
}

/// 本机停止同步：删除本机副本与保存的凭据（地址与账号保留，方便重连）；服务器上的数据不动
#[tauri::command(rename_all = "snake_case")]
pub async fn sync_disconnect(app_state: State<'_, AppState>) -> Result<(), IpcError> {
    let sync = Arc::clone(&app_state.sync);
    sync.disconnect().await
}

/// 清除服务器上的同步数据（所有设备都会停止同步），本机随之断开；清除的是本机自己的服务器时保存的应用密码留下，
/// 重新连接时可以留空沿用。`account = null` 用设置里的地址与账号
#[tauri::command(rename_all = "snake_case")]
pub async fn sync_reset_remote(
    app_state: State<'_, AppState>,
    account: Option<SyncAccount>,
    password: Option<String>,
) -> Result<(), IpcError> {
    validate_opt_sync_account(account.as_ref())?;
    check_opt_secret("password", password.as_deref())?;
    let sync = Arc::clone(&app_state.sync);
    sync.reset_remote(account, password).await
}

/// 生成一个随机的同步密码
#[tauri::command(rename_all = "snake_case")]
pub async fn sync_generate_passphrase() -> Result<String, IpcError> {
    SyncService::generate_passphrase()
}
