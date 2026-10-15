//! 剪贴板命令。

use std::sync::Arc;
use std::time::Instant;

use tauri::State;
use wisp_types::{
    ClipFilter, ClipItemDetail, ClipItemMeta, ClipPage, OcrHistoryItem, PanelView, PasteMode, PasteOutcome, SendTarget,
    SourceApp,
};

use crate::error::IpcError;
use crate::ipc::{MAX_QUERY_BYTES, MAX_TEXT_BYTES, check_len};
use crate::state::AppState;

/// 分页上限
const MAX_PAGE: u32 = 500;

fn check_limit(limit: u32) -> Result<(), IpcError> {
    if limit == 0 || limit > MAX_PAGE {
        return Err(IpcError::invalid_argument(format!(
            "limit must be within 1..={MAX_PAGE}"
        )));
    }
    Ok(())
}

/// 最近条目
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_recent(
    app_state: State<'_, AppState>,
    offset: u32,
    limit: u32,
    filter: ClipFilter,
) -> Result<ClipPage, IpcError> {
    check_limit(limit)?;
    app_state.clipboard.recent(offset, limit, filter).await
}

/// 搜索
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_search(
    app_state: State<'_, AppState>,
    query: String,
    filter: ClipFilter,
    limit: u32,
) -> Result<ClipPage, IpcError> {
    check_len("query", &query, MAX_QUERY_BYTES)?;
    check_limit(limit)?;
    app_state.clipboard.search(query, filter, limit).await
}

/// 详情（Space 预览）
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_get(app_state: State<'_, AppState>, id: i64) -> Result<Option<ClipItemDetail>, IpcError> {
    app_state.clipboard.detail(id).await
}

/// 粘贴：内部负责立即隐藏面板（无退出动画）、还原焦点；只复制时由 Rust 触发 toast 视图。
/// 粘贴目标 = 当前剪贴板面板会话触发时的前台应用（`WindowManager::panel_prev_app`），必须在面板隐藏之前取；
/// `perf.paste` 的 `selected_at` 取命令进入的时刻。
/// 粘贴进行中再来的请求、同一面板会话的第二次粘贴返回 `conflict`（单飞），前端静默忽略
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_paste(
    app_state: State<'_, AppState>,
    id: i64,
    mode: PasteMode,
) -> Result<PasteOutcome, IpcError> {
    let selected_at = Instant::now();
    let windows = &app_state.windows;
    let target = windows
        .panel_state()
        .filter(|p| p.view == PanelView::Clipboard)
        .and_then(|p| windows.panel_prev_app(p.session_id));
    app_state.clipboard.paste(id, mode, target, selected_at).await
}

/// 置顶 / 取消置顶
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_pin(app_state: State<'_, AppState>, id: i64, pinned: bool) -> Result<ClipItemMeta, IpcError> {
    app_state.clipboard.pin(id, pinned).await
}

/// 删除；置顶项要求 force
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_delete(app_state: State<'_, AppState>, id: i64, force: bool) -> Result<(), IpcError> {
    app_state.clipboard.delete(id, force).await
}

/// 清空，返回删除条数
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_clear(app_state: State<'_, AppState>, keep_pinned: bool) -> Result<u32, IpcError> {
    app_state.clipboard.clear(keep_pinned).await
}

/// 暂停 / 恢复记录
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_set_paused(app_state: State<'_, AppState>, paused: bool) -> Result<(), IpcError> {
    app_state.clipboard.set_paused(paused);
    Ok(())
}

/// 原生「另存为」对话框
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_save_image(app_state: State<'_, AppState>, id: i64) -> Result<Option<String>, IpcError> {
    app_state.clipboard.save_image(id).await
}

/// 在文件管理器中显示
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_reveal(app_state: State<'_, AppState>, id: i64) -> Result<(), IpcError> {
    app_state.clipboard.reveal(id).await
}

/// 打开链接
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_open_link(app_state: State<'_, AppState>, id: i64) -> Result<(), IpcError> {
    app_state.clipboard.open_link(id).await
}

/// 送翻译 / OCR
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_send_to(app_state: State<'_, AppState>, id: i64, target: SendTarget) -> Result<(), IpcError> {
    app_state.clipboard.send_to(id, target).await
}

/// OCR 历史列表
#[tauri::command(rename_all = "snake_case")]
pub async fn ocr_history_list(
    app_state: State<'_, AppState>,
    offset: u32,
    limit: u32,
) -> Result<Vec<OcrHistoryItem>, IpcError> {
    check_limit(limit)?;
    app_state.clipboard.ocr_history_list(offset, limit).await
}

/// 删除一条 OCR 历史
#[tauri::command(rename_all = "snake_case")]
pub async fn ocr_history_delete(app_state: State<'_, AppState>, id: i64) -> Result<(), IpcError> {
    app_state.clipboard.ocr_history_delete(id).await
}

/// 清空 OCR 历史
#[tauri::command(rename_all = "snake_case")]
pub async fn ocr_history_clear(app_state: State<'_, AppState>) -> Result<(), IpcError> {
    app_state.clipboard.ocr_history_clear().await
}

/// 正在运行的应用，供剪贴板排除名单的应用选择器（与最近来源合并）。
/// 不含 Wisp 自己，按 id 去重（不区分大小写）、按名称排序；`icon_url` 是 `icon/<key>.png` 资源路径，取不到时为 null
#[tauri::command(rename_all = "snake_case")]
pub async fn apps_running(app_state: State<'_, AppState>) -> Result<Vec<SourceApp>, IpcError> {
    app_state.clipboard.apps_running().await
}

/// 把文本写入系统剪贴板（复制按钮、OCR 自动复制的前端兜底）。前端 `useCopy` 优先走这条命令而不是
/// `navigator.clipboard`（后者在非 key 的 NSPanel、自定义 scheme 与 WebKitGTK 上不可靠）；
/// 写入经 `ClipboardService::copy_text`，会正常进入历史并受抑制规则约束
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_copy_text(app_state: State<'_, AppState>, text: String) -> Result<(), IpcError> {
    check_len("text", &text, MAX_TEXT_BYTES)?;
    // copy_text 同步写系统剪贴板（Windows 的 OpenClipboard 重试最长约 1.5 s）：放进阻塞线程池，不占 tokio worker
    let clipboard = Arc::clone(&app_state.clipboard);
    tauri::async_runtime::spawn_blocking(move || clipboard.copy_text(&text))
        .await
        .map_err(|e| IpcError::internal(format!("copy task failed: {e}")))?
}
