//! 窗口命令。
//!
//! 会等主线程的命令（`panel_ready` / `panel_resize` 的同步显示与定位、`panel_pin` 创建 WebView 窗口、`pinned_ready` 的
//! 交接）放进阻塞线程池执行再 await：常驻运行时只有 2 个 worker，直接在 worker 上等主线程（最长 500 ms，
//! 创建 WebView2 要 100–400 ms）会拖慢同一运行时上的 LLM 流式输出与各计时器。其余命令只投递、不等待，直接执行。

use std::sync::Arc;

use tauri::State;
use wisp_types::{HideReason, PanelShown, PinState, ResizeSpring, WindowTarget};

use crate::error::IpcError;
use crate::ipc::validate::{validate_pin_state, validate_window_target};
use crate::ipc::{check_id, check_non_negative};
use crate::state::AppState;
use crate::windows::WindowManager;

/// 在阻塞线程池里执行会等主线程的窗口操作
async fn blocking<T: Send + 'static>(
    windows: &Arc<WindowManager>,
    f: impl FnOnce(&WindowManager) -> T + Send + 'static,
) -> Result<T, IpcError> {
    let windows = Arc::clone(windows);
    tauri::async_runtime::spawn_blocking(move || f(&windows))
        .await
        .map_err(|e| IpcError::internal(format!("window task failed: {e}")))
}

/// 前端已用 opacity 0 同步渲染并测量完成 → Rust 在命令内同步 show，返回后前端立即播放进入动画。
/// `content_height` 必须是有限的非负数（NaN / Inf 传到原生 setFrame 会崩溃）
#[tauri::command(rename_all = "snake_case")]
pub async fn panel_ready(
    app_state: State<'_, AppState>,
    session_id: u64,
    content_height: f64,
) -> Result<Option<PanelShown>, IpcError> {
    check_non_negative("content_height", content_height)?;
    blocking(&app_state.windows, move |w| w.panel_ready(session_id, content_height)).await
}

/// 前端第一个 opacity > 0 的帧（rAF 内调用）→ perf 的 Shown 埋点
#[tauri::command(rename_all = "snake_case")]
pub async fn panel_painted(app_state: State<'_, AppState>, session_id: u64) -> Result<(), IpcError> {
    app_state.windows.panel_painted(session_id);
    Ok(())
}

/// 卡片的目标高度（逻辑 px）。`spring`（可缺省 = gentle）：Native 帧模式窗口高度的弹簧——浮层预留引起的长高与释放后的
/// 回收由前端指定 snappy；Css 帧模式的窗口阶梯不受影响
#[tauri::command(rename_all = "snake_case")]
pub async fn panel_resize(
    app_state: State<'_, AppState>,
    session_id: u64,
    content_height: f64,
    spring: Option<ResizeSpring>,
) -> Result<(), IpcError> {
    check_non_negative("content_height", content_height)?;
    blocking(&app_state.windows, move |w| {
        w.panel_resize(session_id, content_height, spring)
    })
    .await
}

/// 视图内切换（输入 → 翻译等，`StartAck.width`）的新卡片宽度（逻辑 px）：前端在 ViewSwitch 交换的那次提交——旧内容
/// 已完全透明、新视图还是 opacity 0——调用，Rust 立即改窗口宽度（位置保持）。
/// 宽度没变时为空操作（Rust 发起的交叉淡入已在 ready 时处理）。`width` 必须是有限的非负数
#[tauri::command(rename_all = "snake_case")]
pub async fn panel_set_width(app_state: State<'_, AppState>, session_id: u64, width: f64) -> Result<(), IpcError> {
    check_non_negative("width", width)?;
    blocking(&app_state.windows, move |w| w.panel_set_width(session_id, width)).await
}

/// 退出动画结束后调用
#[tauri::command(rename_all = "snake_case")]
pub async fn panel_hide(app_state: State<'_, AppState>, session_id: u64, reason: HideReason) -> Result<(), IpcError> {
    // 隐藏原因（失焦 / 外部点击 / Esc / Toggle / 固定 / 粘贴…）在诊断日志里可区分
    log::debug!("panel hide session={session_id} reason={reason:?} origin=frontend");
    app_state.windows.panel_hide_now(session_id, reason);
    Ok(())
}

/// 翻译视图被点击后成为 key（Windows 上去掉 NOACTIVATE）
#[tauri::command(rename_all = "snake_case")]
pub async fn panel_activate(app_state: State<'_, AppState>, session_id: u64) -> Result<(), IpcError> {
    app_state.windows.panel_activate(session_id);
    Ok(())
}

/// 用户从面板的头部 / 空白处开始拖动（所有面板视图；剪贴板在「记住位置」模式下隐藏时记住位置）
#[tauri::command(rename_all = "snake_case")]
pub async fn panel_drag_start(app_state: State<'_, AppState>, session_id: u64) -> Result<(), IpcError> {
    app_state.windows.panel_drag_start(session_id);
    Ok(())
}

/// 脱离到 Pinned 窗口，返回 pinned_id。Rust 在面板同一矩形创建**隐藏**的 pinned；交接在该窗口调用
/// `pinned_ready` 时完成（同一帧 show pinned、隐藏 panel），面板在此之前保持可见
#[tauri::command(rename_all = "snake_case")]
pub async fn panel_pin(
    app_state: State<'_, AppState>,
    session_id: u64,
    state: PinState,
    expand: bool,
) -> Result<String, IpcError> {
    validate_pin_state(&state)?;
    blocking(&app_state.windows, move |w| {
        w.pin_from_session(session_id, state, expand)
    })
    .await?
}

/// Pinned 窗口启动时拉取初始状态
#[tauri::command(rename_all = "snake_case")]
pub async fn pinned_init(app_state: State<'_, AppState>, pinned_id: String) -> Result<PinState, IpcError> {
    check_id("pinned_id", &pinned_id)?;
    app_state
        .windows
        .pinned_state(&pinned_id)
        .ok_or_else(|| IpcError::not_found("pinned window"))
}

/// Pinned 窗口完成首帧准备：`pinned_init` → `stream_attach` 回放完毕 → 用 opacity 0 同步渲染并测量 → 调用本命令。
/// Rust 在命令内同一帧 show pinned 并隐藏 panel（不出现空帧），展开模式按 `content_height` 定高
/// （min(内容, 屏幕 70%)）；返回后前端播放 `presets.pinLift`
#[tauri::command(rename_all = "snake_case")]
pub async fn pinned_ready(
    app_state: State<'_, AppState>,
    pinned_id: String,
    content_height: f64,
) -> Result<(), IpcError> {
    check_id("pinned_id", &pinned_id)?;
    check_non_negative("content_height", content_height)?;
    blocking(&app_state.windows, move |w| w.pinned_ready(&pinned_id, content_height)).await
}

/// 切换 Pinned 窗口置顶
#[tauri::command(rename_all = "snake_case")]
pub async fn pinned_set_on_top(
    app_state: State<'_, AppState>,
    pinned_id: String,
    on_top: bool,
) -> Result<(), IpcError> {
    check_id("pinned_id", &pinned_id)?;
    app_state.windows.pinned_set_on_top(&pinned_id, on_top)
}

/// 打开或聚焦设置 / 引导窗口
#[tauri::command(rename_all = "snake_case")]
pub async fn window_open(app_state: State<'_, AppState>, target: WindowTarget) -> Result<(), IpcError> {
    validate_window_target(&target)?;
    app_state.windows.open_target(target);
    Ok(())
}
