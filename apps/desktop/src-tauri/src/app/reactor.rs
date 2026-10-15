//! 设置变化的统一副作用。
//!
//! `SettingsService` 的每次变化（`patch` / `update` / 风格 / 术语表 / 热加载 / 文件问题 / 钥匙串状态，以及其他模块调用
//! `SettingsService::notify_changed` 表示「快照里的运行时信息变了」）都会递增修订号；本模块订阅它，在一个任务里统一处理：
//!
//! 1. 日志级别（`advanced.log_level`，即时生效）；
//! 2. 平台层的应用键位表（`hotkeys.app_keymaps`，`PlatformConfig.keymaps` 共享的那份）；
//! 3. 开机自启（`general.launch_at_login`）与更新策略（`general.update_policy`）；
//! 4. 外观（主题、强调色、字号、材质、减少动效、界面语言）：只在这些设置变了时重新解析（读系统外观要与主线程同步
//!    往返，放到阻塞线程池；能力缓存写回、钥匙串索引、权限、更新器状态等运行时信息的变化不碰它），结果变化才广播
//!    `appearance:changed`；
//! 5. 托盘（状态行、语言、快捷键显示）；
//! 6. 向所有窗口 emit `settings:changed`（完整的 `SettingsSnapshot`）。
//!
//! 同一时刻的多次变化合并成一次处理（watch 通道天然合并，处理前再让出一次调度）。快捷键的重新注册、剪贴板设置的应用
//! 由 HotkeyService / ClipboardService 各自订阅 `SettingsService::subscribe` 完成，这里不重复。

use std::sync::Arc;

use parking_lot::RwLock;
use tauri::{Emitter, Manager};
use wisp_types::{AppKeymap, Settings, event};

use crate::state::AppState;

/// 启动设置变化的处理任务（setup 中调用一次）
pub fn spawn(app: &tauri::AppHandle, keymaps: Arc<RwLock<Vec<AppKeymap>>>) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let settings = Arc::clone(&state.settings);
    let mut rx = settings.changes();
    rx.borrow_and_update();
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut previous = settings.get();
        while rx.changed().await.is_ok() {
            // 合并同一时刻的多次变化（例如删除 Provider 时的钥匙串 + 设置 + 风格三次通知）
            tokio::task::yield_now().await;
            rx.borrow_and_update();
            let current = settings.get();
            let changes = apply(&handle, &previous, &current, &keymaps);
            if changes.appearance {
                // 重新解析外观要读系统明暗 / 强调色（macOS 上各是一次主线程同步往返），放到阻塞线程池，不占 tokio worker。
                // 系统外观的变化由 appearance::watch 的回调处理，这里只管外观相关的设置
                let h = handle.clone();
                if let Err(e) = tauri::async_runtime::spawn_blocking(move || super::appearance::refresh(&h)).await {
                    log::warn!("appearance refresh failed: {e}");
                }
            }
            crate::tray::refresh(&handle);
            broadcast(&handle);
            previous = current;
        }
    });
}

/// 处理一次变化中不需要等待的副作用，返回差异（外观、托盘与广播由调用方接着做）
fn apply(app: &tauri::AppHandle, prev: &Settings, cur: &Settings, keymaps: &RwLock<Vec<AppKeymap>>) -> Changes {
    let changes = Changes::between(prev, cur);
    if changes.log_level {
        super::logging::apply_level(cur.advanced.log_level);
        log::info!("log level set to {:?}", cur.advanced.log_level);
    }
    if changes.keymaps {
        *keymaps.write() = super::merged_keymaps(&cur.hotkeys.app_keymaps);
    }
    if changes.launch_at_login {
        super::autostart::sync(app, cur.general.launch_at_login);
    }
    if changes.update_policy {
        crate::updater::on_policy_changed(app, cur.general.update_policy);
    }
    changes
}

/// 向所有窗口广播 `settings:changed`
pub fn broadcast(app: &tauri::AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let snapshot = state.settings.snapshot(super::runtime_info(app, &state));
    if let Err(e) = app.emit(event::SETTINGS_CHANGED, &snapshot) {
        log::warn!("emit settings:changed failed: {e}");
    }
}

/// 两份设置之间、需要额外处理的差异
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Changes {
    pub log_level: bool,
    pub keymaps: bool,
    pub launch_at_login: bool,
    pub update_policy: bool,
    /// 外观设置（主题、强调色、字号、材质、减少动效……）或界面语言
    pub appearance: bool,
}

impl Changes {
    /// 比较两份设置
    pub fn between(prev: &Settings, cur: &Settings) -> Self {
        Self {
            log_level: prev.advanced.log_level != cur.advanced.log_level,
            keymaps: prev.hotkeys.app_keymaps != cur.hotkeys.app_keymaps,
            launch_at_login: prev.general.launch_at_login != cur.general.launch_at_login,
            update_policy: prev.general.update_policy != cur.general.update_policy,
            appearance: prev.appearance != cur.appearance || prev.general.ui_language != cur.general.ui_language,
        }
    }
}

#[cfg(test)]
mod tests {
    use wisp_types::{LogLevel, UpdatePolicy};

    use super::*;

    #[test]
    fn detects_only_relevant_changes() {
        let a = Settings::default();
        assert_eq!(Changes::between(&a, &a), Changes::default());
        let mut b = a.clone();
        b.advanced.log_level = LogLevel::Debug;
        b.general.update_policy = UpdatePolicy::Off;
        b.appearance.font_size = 16;
        let c = Changes::between(&a, &b);
        assert!(c.log_level && c.update_policy && c.appearance);
        assert!(!c.launch_at_login && !c.keymaps);
        let mut d = a.clone();
        d.general.launch_at_login = !a.general.launch_at_login;
        let c = Changes::between(&a, &d);
        assert!(c.launch_at_login && !c.appearance, "与外观无关的变化不重新解析外观");
        let mut e = a.clone();
        e.general.ui_language = wisp_types::UiLanguage::EnUs;
        assert!(Changes::between(&a, &e).appearance, "界面语言也在 AppearanceState 里");
        let mut f = a.clone();
        f.providers.push(crate::settings::service::tests::provider("p", &["m"]));
        assert_eq!(
            Changes::between(&a, &f),
            Changes::default(),
            "能力缓存写回等设置变化不触发外观"
        );
    }
}
