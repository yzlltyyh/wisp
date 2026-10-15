//! 快捷键与扩展触发：注册与回退、冲突检测、录制、测试、风格快捷键、Esc guard、扩展触发（`mouse-hook` feature）、
//! 键盘布局建议。注册后端：macOS / Windows 是快捷键插件，Linux X11 是平台层的事件驱动 `XGrabKey`；两者都没有
//! （Wayland、没有 X 显示）时所有快捷键报告 `unavailable`，且不得调用 `app.global_shortcut()`。
//!
//! 模块划分：
//! - `plan`：由设置算出每个目标的候选组合，按「首选 → 备选 → 留空」注册（纯逻辑，经 `Registrar` 抽象注册后端）；
//! - `recorder` / `conflicts`：录制规则与分层冲突判定（纯逻辑，静态表在 `data/conflicts.json`，数据驱动的用例在
//!   `data/check_cases.json`）；
//! - `display`：与前端 `formatHotkey` 逐字符一致的显示串；`names`：冲突提示里的目标名；
//! - `press`：按住 / 自动重复的过滤与 150 ms 去抖；`dispatch`：快捷键回调与扩展触发的派发判定（测试回显、录制期间
//!   抑制、Esc、看门狗）；`esc_guard`：全局 Esc 的注册时机；
//! - `triggers`：扩展触发的规格、按权限拆分的安装与派发（浮标探测）；`neighbors`：邻居应用探测；
//! - `service`：把以上接到插件、设置订阅、事件与命令总线上。

pub mod conflicts;
pub mod defaults;
pub mod dispatch;
pub mod display;
pub mod esc_guard;
pub mod names;
pub mod neighbors;
pub mod plan;
pub mod press;
pub mod recorder;
pub mod service;
pub mod triggers;

use tauri::Manager;

pub use service::HotkeyService;

use crate::state::AppState;

/// 全局快捷键插件的回调（在 lib.rs 注册插件时传入；Linux 不注册插件，见 `HotkeyService::on_native_key`）。插件在主线程上、持有自己的锁时
/// 调用这里：只查路由并投递，不阻塞、不调用插件。匹配按 `Shortcut::id()`（由解析后的修饰键与主键算出），不比较
/// `sc.into_string()`（它输出小写、`shift+control+alt+super` 顺序的串，与规范串不同）
pub fn on_shortcut(
    app: &tauri::AppHandle,
    sc: &tauri_plugin_global_shortcut::Shortcut,
    ev: tauri_plugin_global_shortcut::ShortcutEvent,
) {
    if let Some(state) = app.try_state::<AppState>() {
        state.hotkeys.on_shortcut(sc, ev);
    }
}
