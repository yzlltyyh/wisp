//! 剪贴板命令。

use std::sync::Arc;
use std::time::Instant;

use tauri::{State, WebviewWindow};
use wisp_clipboard::clean_favorite_name;
use wisp_types::{
    ClipDragData, ClipFilter, ClipFloatInit, ClipInspect, ClipItemDetail, ClipItemMeta, ClipPage, OcrHistoryItem,
    PanelView, PasteMode, PasteOutcome, QueueStatus, SendTarget, SourceApp, TransformId,
};

use crate::clipboard_svc::{FLOAT_LABEL, PasteHost, PasteRequest, PasteSource};
use crate::error::IpcError;
use crate::ipc::{MAX_QUERY_BYTES, MAX_TEXT_BYTES, check_len, check_list_len};
use crate::state::AppState;
use crate::windows::float::FloatSeed;

/// 分页上限
const MAX_PAGE: u32 = 500;
/// 合并粘贴 / 连续粘贴一次最多的条数（与分页上限一致：多选只能选已经加载出来的行）
const MAX_PASTE_IDS: usize = MAX_PAGE as usize;

fn check_limit(limit: u32) -> Result<(), IpcError> {
    if limit == 0 || limit > MAX_PAGE {
        return Err(IpcError::invalid_argument(format!(
            "limit must be within 1..={MAX_PAGE}"
        )));
    }
    Ok(())
}

/// 多选的条目 id：非空、不超过 [`MAX_PASTE_IDS`] 条
fn check_ids(ids: &[i64]) -> Result<(), IpcError> {
    if ids.is_empty() {
        return Err(IpcError::invalid_argument("ids must not be empty"));
    }
    check_list_len("ids", ids.len(), MAX_PASTE_IDS)
}

/// 片段文本：非空、不超过 [`MAX_TEXT_BYTES`]
fn check_paste_text(text: &str) -> Result<(), IpcError> {
    check_len("text", text, MAX_TEXT_BYTES)?;
    if text.is_empty() {
        return Err(IpcError::invalid_argument("text must not be empty"));
    }
    Ok(())
}

/// 收藏的名称：按存储层的同一套规则清理（去首尾空白、≤ 64 字符、无控制字符），空白等同于清除
fn check_name(name: Option<&str>) -> Result<Option<String>, IpcError> {
    match name {
        None => Ok(None),
        Some(n) => clean_favorite_name(n).map_err(|e| IpcError::invalid_argument(format!("name: {e}"))),
    }
}

/// 粘贴的宿主：按调用窗口推出（面板要正显示剪贴板视图，见 `PasteHost::of_window`）
fn paste_host(window: &WebviewWindow, app_state: &AppState) -> Result<PasteHost, IpcError> {
    let session = app_state
        .windows
        .panel_state()
        .filter(|p| p.view == PanelView::Clipboard)
        .map(|p| p.session_id);
    PasteHost::of_window(window.label(), session)
}

async fn paste(
    app_state: &AppState,
    source: PasteSource,
    mode: PasteMode,
    host: PasteHost,
    selected_at: Instant,
) -> Result<PasteOutcome, IpcError> {
    let req = PasteRequest {
        source,
        mode,
        host,
        selected_at,
    };
    app_state.clipboard.paste(req).await
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

/// 粘贴一个条目（`transform` 给出时粘贴「粘贴为…」变换后的文本，结果不进历史）。所有粘贴走同一条管线
/// （`clipboard_svc::paste`）：宿主由**调用窗口**推出——面板（必须正显示剪贴板视图：立即隐藏、无退出动画、只复制时由
/// Rust 弹 toast）或悬浮剪贴板（不隐藏，先交还键盘焦点）；`perf.paste` 的 `selected_at` 取命令进入的时刻。
/// 粘贴进行中再来的请求、同一面板会话的第二次粘贴返回 `conflict`（单飞），前端静默忽略
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_paste(
    webview_window: WebviewWindow,
    app_state: State<'_, AppState>,
    id: i64,
    mode: PasteMode,
    transform: Option<TransformId>,
) -> Result<PasteOutcome, IpcError> {
    let selected_at = Instant::now();
    let host = paste_host(&webview_window, &app_state)?;
    let source = PasteSource::Item { id, transform };
    paste(&app_state, source, mode, host, selected_at).await
}

/// 粘贴一段文字（芯片条的片段、识别结果里的片段）：写入带自写抑制、不进历史；`from` 是片段所在的条目（touch 它）
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_paste_text(
    webview_window: WebviewWindow,
    app_state: State<'_, AppState>,
    text: String,
    from: Option<i64>,
    mode: PasteMode,
) -> Result<PasteOutcome, IpcError> {
    check_paste_text(&text)?;
    let selected_at = Instant::now();
    let host = paste_host(&webview_window, &app_state)?;
    paste(
        &app_state,
        PasteSource::Fragment { from, text },
        mode,
        host,
        selected_at,
    )
    .await
}

/// 多选的条目按给出的顺序以换行合并后粘贴（合并结果不进历史；各条目被 touch，第一条排在最上面）
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_paste_many(
    webview_window: WebviewWindow,
    app_state: State<'_, AppState>,
    ids: Vec<i64>,
    mode: PasteMode,
) -> Result<PasteOutcome, IpcError> {
    check_ids(&ids)?;
    let selected_at = Instant::now();
    let host = paste_host(&webview_window, &app_state)?;
    paste(&app_state, PasteSource::Many { ids }, mode, host, selected_at).await
}

/// 条目的检视：识别出的片段（芯片条）与可用的「粘贴为…」变换，一次返回
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_inspect(app_state: State<'_, AppState>, id: i64) -> Result<ClipInspect, IpcError> {
    app_state.clipboard.inspect(id).await
}

/// 纯文本（识别结果等）的检视
#[tauri::command(rename_all = "snake_case")]
pub async fn text_inspect(app_state: State<'_, AppState>, text: String) -> Result<ClipInspect, IpcError> {
    check_len("text", &text, MAX_TEXT_BYTES)?;
    app_state.clipboard.text_inspect(text).await
}

/// 收藏 / 取消收藏
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_pin(app_state: State<'_, AppState>, id: i64, pinned: bool) -> Result<ClipItemMeta, IpcError> {
    app_state.clipboard.pin(id, pinned).await
}

/// 给收藏命名：去掉首尾空白，最多 64 个字符，不允许控制字符；空白或 null 清除名称。命名即收藏
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_set_name(
    app_state: State<'_, AppState>,
    id: i64,
    name: Option<String>,
) -> Result<ClipItemMeta, IpcError> {
    let name = check_name(name.as_deref())?;
    app_state.clipboard.set_name(id, name).await
}

/// 列表顶部「收藏」分组的折叠状态（只属于本机，存 state.json，随 `ClipboardPayload` 下发）
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_set_favorites_collapsed(
    app_state: State<'_, AppState>,
    collapsed: bool,
) -> Result<(), IpcError> {
    app_state.clipboard.set_favorites_collapsed(collapsed);
    Ok(())
}

/// 拖出条目时的拖拽数据（按下时先取好，拖拽开始的回调里同步写入）：一条时是纯文本、富文本的 HTML、链接的地址；
/// 多条时按选择顺序以换行合并的纯文本
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_drag_data(app_state: State<'_, AppState>, ids: Vec<i64>) -> Result<ClipDragData, IpcError> {
    check_ids(&ids)?;
    app_state.clipboard.drag_data(ids).await
}

// ---------- 连续粘贴与悬浮剪贴板 ----------

/// 连续粘贴：立即经调用窗口的宿主（剪贴板面板 / 悬浮剪贴板）粘贴第一条，之后在任何应用里每按一次粘贴键粘贴下一条
/// （进度经 `clipboard:queue` 推送；Esc、10 分钟不操作、锁屏时结束）。不可用（Wayland 等）时返回 `platform` + 原因 key
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_queue_start(
    webview_window: WebviewWindow,
    app_state: State<'_, AppState>,
    ids: Vec<i64>,
) -> Result<QueueStatus, IpcError> {
    check_ids(&ids)?;
    let selected_at = Instant::now();
    let host = paste_host(&webview_window, &app_state)?;
    app_state.clipboard.queue_start(ids, host, selected_at).await
}

/// 结束连续粘贴（没有进行中的队列时什么都不做）
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_queue_cancel(app_state: State<'_, AppState>) -> Result<(), IpcError> {
    app_state.clipboard.queue_cancel().await
}

/// 只有悬浮剪贴板窗口能调用的命令
fn require_float(window: &WebviewWindow) -> Result<(), IpcError> {
    if window.label() == FLOAT_LABEL {
        Ok(())
    } else {
        Err(IpcError::invalid_argument(format!(
            "window {} is not the floating clipboard",
            window.label()
        )))
    }
}

/// 从剪贴板面板交接到悬浮剪贴板（继承查询、筛选与光标）：隐藏创建窗口，页面 reveal 时同一帧显示它、隐藏面板。
/// 只接受正显示剪贴板视图的面板调用
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_float_open(
    webview_window: WebviewWindow,
    app_state: State<'_, AppState>,
    query: String,
    filter: ClipFilter,
    selected_id: Option<i64>,
) -> Result<(), IpcError> {
    check_len("query", &query, MAX_QUERY_BYTES)?;
    let PasteHost::Panel { session } = paste_host(&webview_window, &app_state)? else {
        return Err(IpcError::invalid_argument(
            "only the clipboard panel opens the floating clipboard",
        ));
    };
    let seed = FloatSeed {
        query,
        filter,
        selected_id,
    };
    let windows = Arc::clone(&app_state.windows);
    tauri::async_runtime::spawn_blocking(move || windows.float_open(session, seed))
        .await
        .map_err(|e| IpcError::internal(format!("float open task failed: {e}")))?
}

/// 关闭（销毁）悬浮剪贴板
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_float_close(app_state: State<'_, AppState>) -> Result<(), IpcError> {
    app_state.windows.float_close();
    Ok(())
}

/// 悬浮剪贴板就绪（先订阅事件再调用）：此后 `clipboard:changed` 同时推给它；返回继承的视图状态、置顶与首页
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_float_ready(
    webview_window: WebviewWindow,
    app_state: State<'_, AppState>,
) -> Result<ClipFloatInit, IpcError> {
    require_float(&webview_window)?;
    // 先标记就绪再取首页：取页之后到达的改动照样推送（与首页重叠的由前端按 id 幂等处理）
    let (seed, on_top) = app_state.windows.float_page_ready();
    let settings = app_state.settings.get();
    let clipboard = Arc::clone(&app_state.clipboard);
    let payload = tauri::async_runtime::spawn_blocking(move || clipboard.payload(&settings))
        .await
        .map_err(|e| IpcError::internal(format!("float payload task failed: {e}")))?;
    Ok(ClipFloatInit {
        query: seed.query,
        filter: seed.filter,
        selected_id: seed.selected_id,
        on_top,
        payload,
    })
}

/// 悬浮剪贴板的首帧已同步渲染：同一帧显示它、隐藏发起的面板（没有空帧）
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_float_reveal(
    webview_window: WebviewWindow,
    app_state: State<'_, AppState>,
) -> Result<(), IpcError> {
    require_float(&webview_window)?;
    let windows = Arc::clone(&app_state.windows);
    tauri::async_runtime::spawn_blocking(move || windows.float_reveal())
        .await
        .map_err(|e| IpcError::internal(format!("float reveal task failed: {e}")))
}

/// 悬浮剪贴板置顶 / 取消置顶（记为下次打开的默认值）
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_float_set_on_top(
    webview_window: WebviewWindow,
    app_state: State<'_, AppState>,
    on_top: bool,
) -> Result<(), IpcError> {
    require_float(&webview_window)?;
    app_state.windows.float_set_on_top(on_top)
}

/// 悬浮剪贴板的搜索框获得 / 失去焦点：平时它不抢焦点，打字期间临时可聚焦（粘贴前由粘贴管线交还）
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_float_focus(
    webview_window: WebviewWindow,
    app_state: State<'_, AppState>,
    focused: bool,
) -> Result<(), IpcError> {
    require_float(&webview_window)?;
    let windows = Arc::clone(&app_state.windows);
    tauri::async_runtime::spawn_blocking(move || windows.float_focus(focused))
        .await
        .map_err(|e| IpcError::internal(format!("float focus task failed: {e}")))?
}

/// 指针移入悬浮剪贴板：取回其他设备的改动（同步层自己节流）
#[tauri::command(rename_all = "snake_case")]
pub async fn clipboard_float_hover(
    webview_window: WebviewWindow,
    app_state: State<'_, AppState>,
) -> Result<(), IpcError> {
    require_float(&webview_window)?;
    app_state.windows.float_hover();
    Ok(())
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

/// 送翻译 / 识别文字（图片条目）
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::IpcErrorCode;

    #[test]
    fn names_are_cleaned_before_reaching_the_service() {
        assert_eq!(check_name(None).expect("清除"), None);
        assert_eq!(check_name(Some("  Wi-Fi  ")).expect("名称").as_deref(), Some("Wi-Fi"));
        assert_eq!(check_name(Some("   ")).expect("空白"), None, "空白等同于清除");
        assert_eq!(
            check_name(Some(&"名".repeat(64)))
                .expect("恰好 64 个字符")
                .map(|n| n.chars().count()),
            Some(64)
        );
        for bad in ["名".repeat(65), "line\nbreak".to_owned(), "tab\there".to_owned()] {
            let err = check_name(Some(&bad)).expect_err(&bad);
            assert_eq!(err.code, IpcErrorCode::InvalidArgument);
        }
    }

    #[test]
    fn paste_arguments_are_bounded() {
        assert!(check_ids(&[1, 2]).is_ok());
        assert!(check_ids(&[]).is_err(), "空的多选");
        assert!(check_ids(&vec![1; MAX_PASTE_IDS]).is_ok());
        assert!(check_ids(&vec![1; MAX_PASTE_IDS + 1]).is_err());
        assert!(check_paste_text("482915").is_ok());
        assert!(check_paste_text("").is_err());
        assert!(check_paste_text(&"x".repeat(MAX_TEXT_BYTES + 1)).is_err());
    }
}
