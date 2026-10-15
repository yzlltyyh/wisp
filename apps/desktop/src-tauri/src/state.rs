//! 依赖注入。
//!
//! 每个服务只从 [`Deps`] 构造，构造时**不互相持有 Arc**；服务之间的调用一律在运行时通过
//! `app.state::<AppState>()` 取得对方（消除 Orchestrator ↔ WindowManager ↔ HotkeyService ↔ ClipboardService 的环）。
//! 每个服务：`pub fn new(d: &Deps) -> Arc<Self>`；`LlmClient::new(ClientConfig)` 由 bootstrap 构造。

use std::sync::Arc;

use crate::app::perf::PerfMarks;
use crate::assets::AssetRegistry;
use crate::bus::Bus;
use crate::clipboard_svc::ClipboardService;
use crate::hotkeys::HotkeyService;
use crate::orchestrator::Orchestrator;
use crate::settings::SettingsService;
use crate::sync_svc::SyncService;
use crate::windows::WindowManager;

/// 构造服务所需的公共依赖
pub struct Deps {
    pub app: tauri::AppHandle,
    pub settings: Arc<SettingsService>,
    pub platform: Arc<wisp_platform::Platform>,
    pub bus: Bus,
    pub perf: Arc<PerfMarks>,
    pub assets: Arc<AssetRegistry>,
}

/// 由 `app.manage()` 托管的全局状态
pub struct AppState {
    pub settings: Arc<SettingsService>,
    pub platform: Arc<wisp_platform::Platform>,
    pub llm: wisp_llm::LlmClient,
    pub windows: Arc<WindowManager>,
    pub orchestrator: Arc<Orchestrator>,
    pub clipboard: Arc<ClipboardService>,
    pub hotkeys: Arc<HotkeyService>,
    /// 多设备同步（关闭时不持有任何资源）
    pub sync: Arc<SyncService>,
    pub perf: Arc<PerfMarks>,
    pub assets: Arc<AssetRegistry>,
    pub bus: Bus,
}
