//! IPC 命令。规则：
//! - 所有命令都写成 `#[tauri::command(rename_all = "snake_case")] async fn …`，返回 `Result<T, IpcError>`；
//! - ipc 层只负责校验参数（长度上限、枚举合法性、数值有限性）并分发调用，不写业务逻辑；
//! - 命令参数名就是前端 `invoke` 参数对象的键（snake_case），不得改名或加下划线前缀。
//!
//! **参数校验**：每条命令的每个参数，凡是字符串、浮点数、集合或含这些字段的 DTO，都必须在
//! 函数体第一步经 `check_*` / `validate::*` 校验（长度、条数、有限性），校验失败返回 `invalid_argument`，不得进入服务。
//! 只有整数、bool 与无字段枚举（反序列化即保证合法）可以不校验。`contract_tests::every_param_is_validated` 静态检查
//! 这一点：新增参数时要么调用校验函数，要么在那里的白名单里说明理由。上限常量与组合校验见下方与 [`validate`]。
//!
//! 命令清单只写在下面的 `commands!` 里一处：它同时生成 `generate_handler!` 与 [`COMMAND_NAMES`]。
//! `contract_tests`（`cargo test -p wisp-desktop --lib ipc::`）解析各 `cmd_*.rs` 与前端 `shared/ipc/commands.ts`
//! / `commands.overlay.ts`，逐项比对：命令集合、每条命令的参数键、流式命令（带 `on_event: Channel`）集合、
//! `rename_all = "snake_case"`。增删改命令或参数时两边必须同步，否则这组测试失败。
//!
//! 枚举非法 / 缺参数这类**反序列化失败**发生在进入命令函数体之前，Tauri 以纯字符串
//! `invalid args `<key>` for command `<cmd>`: …` 拒绝；前端 `WispIpcError.from` 把它归一为 `invalid_argument`。
//!
//! Rust → 前端的事件名一律引用 `wisp_types::event::*` 常量（与 `events.ts` 的 `EVENT_NAMES` 由 wisp-types 的
//! golden 测试逐项比对），不要手写 `"panel:prepare"` 之类的字符串。

pub mod cmd_app;
pub mod cmd_clipboard;
pub mod cmd_hotkeys;
pub mod cmd_models;
pub mod cmd_overlay;
pub mod cmd_provider;
pub mod cmd_settings;
pub mod cmd_stream;
pub mod cmd_sync;
pub mod cmd_update;
pub mod cmd_window;
#[cfg(test)]
mod contract_tests;
pub mod validate;

use crate::error::IpcError;

/// 翻译文本上限（1 MB）
pub const MAX_TEXT_BYTES: usize = 1024 * 1024;
/// 查询字符串上限（1 KB）
pub const MAX_QUERY_BYTES: usize = 1024;
/// API Key 上限（8 KB）；也用于 Provider 自定义 header 的值（常放凭据）
pub const MAX_KEY_BYTES: usize = 8 * 1024;
/// id 类参数上限（256 字节）：request_id、capture_id、pinned_id、provider_id、model_id、monitor_id、template_id、
/// 风格 id、设置分组名等（Windows 的 MonitorId `\\.\DISPLAYn`、`custom-<uuid8>`、厂商模型名都远小于此）
pub const MAX_ID_BYTES: usize = 256;
/// 集合参数的条数上限：`styles_reorder.ids`、`glossary_set.entries`、`Provider.models` 等
/// （术语表的性能预算按 1000 条设计；聚合平台的模型列表约数百条）
pub const MAX_LIST_LEN: usize = 10_000;

/// 校验字符串参数长度（按 UTF-8 **字节**计，恰好等于上限合法）
pub fn check_len(field: &str, value: &str, max: usize) -> Result<(), IpcError> {
    if value.len() > max {
        return Err(IpcError::invalid_argument(format!("{field} exceeds {max} bytes")));
    }
    Ok(())
}

/// 校验 id 类参数：按 UTF-8 字节不超过 [`MAX_ID_BYTES`]，且不含控制字符（防止换行等写进日志）。
/// 只校验形状；空串与「是否存在」由服务判断（例如不存在返回 `not_found`）
pub fn check_id(field: &str, value: &str) -> Result<(), IpcError> {
    check_len(field, value, MAX_ID_BYTES)?;
    if value.chars().any(char::is_control) {
        return Err(IpcError::invalid_argument(format!(
            "{field} must not contain control characters"
        )));
    }
    Ok(())
}

/// 校验集合参数的条数（恰好等于上限合法）
pub fn check_list_len(field: &str, len: usize, max: usize) -> Result<(), IpcError> {
    if len > max {
        return Err(IpcError::invalid_argument(format!("{field} exceeds {max} items")));
    }
    Ok(())
}

/// 校验数值参数是有限的非负数（窗口高度等）。NaN / Inf / 负数一路传到原生 `setFrame` / `SetWindowPos`，
/// macOS 上 NSWindow 遇到 NaN frame 会抛异常直接崩溃，所以在 ipc 层拦下
pub fn check_non_negative(field: &str, value: f64) -> Result<(), IpcError> {
    if !value.is_finite() || value < 0.0 {
        return Err(IpcError::invalid_argument(format!(
            "{field} must be a non-negative finite number"
        )));
    }
    Ok(())
}

/// 命令清单：同时生成 [`COMMAND_NAMES`] 与 [`handler`]（两者同源，不会漂移）
macro_rules! commands {
    ($($module:ident :: $name:ident),* $(,)?) => {
        /// 全部命令名（与 [`handler`] 同源；`contract_tests` 与前端 `COMMAND_NAMES` 逐项比对）
        pub const COMMAND_NAMES: &[&str] = &[$(stringify!($name)),*];

        /// 全部命令（`tauri::generate_handler!`）
        pub fn handler() -> impl Fn(tauri::ipc::Invoke) -> bool + Send + Sync + 'static {
            tauri::generate_handler![$($module::$name),*]
        }
    };
}

commands![
    // ---- 窗口----
    cmd_window::panel_ready,
    cmd_window::panel_painted,
    cmd_window::panel_resize,
    cmd_window::panel_set_width,
    cmd_window::panel_hide,
    cmd_window::panel_activate,
    cmd_window::panel_drag_start,
    cmd_window::panel_pin,
    cmd_window::pinned_init,
    cmd_window::pinned_ready,
    cmd_window::pinned_set_on_top,
    cmd_window::window_open,
    // ---- 流----
    cmd_stream::stream_attach,
    cmd_stream::cancel,
    cmd_stream::translate_start,
    cmd_stream::ocr_start,
    cmd_stream::capture_release,
    cmd_stream::trigger_action,
    // ---- Overlay----
    cmd_overlay::overlay_init,
    cmd_overlay::overlay_ready,
    cmd_overlay::overlay_commit,
    cmd_overlay::overlay_done,
    cmd_overlay::overlay_cancel,
    cmd_overlay::overlay_focus,
    // ---- 剪贴板----
    cmd_clipboard::clipboard_recent,
    cmd_clipboard::clipboard_search,
    cmd_clipboard::clipboard_get,
    cmd_clipboard::clipboard_paste,
    cmd_clipboard::clipboard_paste_text,
    cmd_clipboard::clipboard_paste_many,
    cmd_clipboard::clipboard_inspect,
    cmd_clipboard::text_inspect,
    cmd_clipboard::text_transform,
    cmd_clipboard::clipboard_pin,
    cmd_clipboard::clipboard_set_name,
    cmd_clipboard::clipboard_set_favorites_collapsed,
    cmd_clipboard::clipboard_drag_data,
    cmd_clipboard::clipboard_queue_start,
    cmd_clipboard::clipboard_queue_cancel,
    cmd_clipboard::clipboard_float_open,
    cmd_clipboard::clipboard_float_close,
    cmd_clipboard::clipboard_float_ready,
    cmd_clipboard::clipboard_float_reveal,
    cmd_clipboard::clipboard_float_set_on_top,
    cmd_clipboard::clipboard_float_focus,
    cmd_clipboard::clipboard_float_hover,
    cmd_clipboard::clipboard_delete,
    cmd_clipboard::clipboard_clear,
    cmd_clipboard::clipboard_set_paused,
    cmd_clipboard::clipboard_save_image,
    cmd_clipboard::clipboard_reveal,
    cmd_clipboard::clipboard_open_link,
    cmd_clipboard::clipboard_send_to,
    cmd_clipboard::clipboard_copy_text,
    cmd_clipboard::ocr_history_list,
    cmd_clipboard::ocr_history_delete,
    cmd_clipboard::ocr_history_clear,
    cmd_clipboard::apps_running,
    // ---- 设置----
    cmd_settings::settings_get,
    cmd_settings::settings_patch,
    cmd_settings::excluded_app_upsert,
    cmd_settings::excluded_app_remove,
    cmd_settings::app_keymap_upsert,
    cmd_settings::app_keymap_remove,
    cmd_settings::styles_list,
    cmd_settings::style_save,
    cmd_settings::style_delete,
    cmd_settings::style_reset,
    cmd_settings::styles_reorder,
    cmd_settings::style_preview,
    cmd_settings::glossary_get,
    cmd_settings::glossary_set,
    cmd_settings::glossary_upsert,
    cmd_settings::glossary_remove,
    cmd_settings::glossary_import,
    cmd_settings::glossary_export,
    // ---- Provider----
    cmd_provider::provider_templates,
    cmd_provider::provider_new,
    cmd_provider::provider_delete,
    cmd_provider::provider_save,
    cmd_provider::provider_add,
    cmd_provider::providers_reorder,
    cmd_provider::provider_test,
    cmd_provider::provider_models,
    cmd_provider::llm_preview_url,
    cmd_provider::secret_set,
    cmd_provider::secret_delete,
    cmd_provider::model_caps_reset,
    // ---- 应用----
    cmd_app::permissions_status,
    cmd_app::permissions_request,
    cmd_app::permissions_open,
    cmd_app::permissions_watch,
    cmd_app::app_info,
    cmd_app::onboarding_complete,
    cmd_app::appearance_get,
    cmd_app::open_path,
    cmd_app::open_url,
    cmd_app::app_relaunch,
    cmd_app::app_quit,
    cmd_app::diagnostics_export,
    // ---- 更新----
    cmd_update::update_check,
    cmd_update::update_install,
    cmd_update::update_status,
    // ---- 同步----
    cmd_sync::sync_status,
    cmd_sync::sync_devices,
    cmd_sync::sync_probe,
    cmd_sync::sync_connect,
    cmd_sync::sync_now,
    cmd_sync::sync_disconnect,
    cmd_sync::sync_reset_remote,
    cmd_sync::sync_generate_passphrase,
    // ---- 离线模型----
    cmd_models::offline_models_get,
    cmd_models::offline_model_download,
    cmd_models::offline_model_pause,
    cmd_models::offline_model_cancel,
    cmd_models::offline_model_delete,
    cmd_models::offline_runtime_delete,
    cmd_models::offline_release,
    cmd_models::offline_reveal,
    cmd_models::offline_devices_refresh,
    // ---- 快捷键----
    cmd_hotkeys::hotkey_status,
    cmd_hotkeys::hotkey_record_begin,
    cmd_hotkeys::hotkey_record_end,
    cmd_hotkeys::hotkey_check,
    cmd_hotkeys::hotkey_set,
    cmd_hotkeys::hotkey_reset_defaults,
    cmd_hotkeys::hotkey_test_begin,
    cmd_hotkeys::hotkey_test_end,
    cmd_hotkeys::neighbor_apps,
    cmd_hotkeys::keyboard_advice,
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::IpcErrorCode;

    #[test]
    fn check_len_boundaries() {
        // 恰好等于上限合法，上限 + 1 拒绝
        assert!(check_len("q", &"a".repeat(MAX_QUERY_BYTES), MAX_QUERY_BYTES).is_ok());
        let err = check_len("q", &"a".repeat(MAX_QUERY_BYTES + 1), MAX_QUERY_BYTES).expect_err("over");
        assert_eq!(err.code, IpcErrorCode::InvalidArgument);
        assert!(err.message.contains("q exceeds 1024 bytes"), "{}", err.message);
        assert!(check_len("text", &"a".repeat(MAX_TEXT_BYTES), MAX_TEXT_BYTES).is_ok());
        assert!(check_len("text", &"a".repeat(MAX_TEXT_BYTES + 1), MAX_TEXT_BYTES).is_err());
        assert!(check_len("empty", "", 0).is_ok());
        assert!(check_len("one", "a", 0).is_err());
    }

    #[test]
    fn check_len_counts_utf8_bytes() {
        // 「你好」= 6 字节（2 个字符）
        assert!(check_len("q", "你好", 6).is_ok());
        assert!(check_len("q", "你好", 5).is_err());
        // 多字节字符跨越上限：1022 个 ASCII + 一个 3 字节字符 = 1025 字节
        let straddle = format!("{}你", "a".repeat(MAX_QUERY_BYTES - 2));
        assert_eq!(straddle.len(), MAX_QUERY_BYTES + 1);
        assert!(check_len("q", &straddle, MAX_QUERY_BYTES).is_err());
        // emoji（4 字节）恰好凑满上限
        let exact = format!("{}😀", "a".repeat(MAX_KEY_BYTES - 4));
        assert_eq!(exact.len(), MAX_KEY_BYTES);
        assert!(check_len("key", &exact, MAX_KEY_BYTES).is_ok());
    }

    #[test]
    fn check_id_boundaries() {
        assert!(check_id("request_id", &"r".repeat(MAX_ID_BYTES)).is_ok());
        let err = check_id("request_id", &"r".repeat(MAX_ID_BYTES + 1)).expect_err("over");
        assert_eq!(err.code, IpcErrorCode::InvalidArgument);
        assert!(err.message.contains("request_id exceeds 256 bytes"), "{}", err.message);
        // 三端真实形状的 id 都合法（Windows 的 MonitorId 含反斜杠）
        for ok in [
            r"\\.\DISPLAY1",
            "69733568",
            "DP-1",
            "custom-1a2b3c4d",
            "openai-main",
            "pinned-3",
            "",
        ] {
            assert!(check_id("id", ok).is_ok(), "{ok}");
        }
        for bad in ["a\nb", "a\0b", "\u{1b}[31m", "a\u{7f}"] {
            assert_eq!(
                check_id("id", bad).expect_err("control").code,
                IpcErrorCode::InvalidArgument,
                "{bad:?}"
            );
        }
        // 多字节：85 个「你」= 255 字节合法，86 个 = 258 字节拒绝
        assert!(check_id("id", &"你".repeat(85)).is_ok());
        assert!(check_id("id", &"你".repeat(86)).is_err());
    }

    #[test]
    fn check_list_len_boundaries() {
        assert!(check_list_len("ids", MAX_LIST_LEN, MAX_LIST_LEN).is_ok());
        let err = check_list_len("ids", MAX_LIST_LEN + 1, MAX_LIST_LEN).expect_err("over");
        assert_eq!(err.code, IpcErrorCode::InvalidArgument);
        assert!(err.message.contains("ids exceeds 10000 items"), "{}", err.message);
        assert!(check_list_len("empty", 0, 0).is_ok());
    }

    #[test]
    fn check_non_negative_rejects_nan_inf_negative() {
        for ok in [0.0, 0.5, 120.0, 1e6] {
            assert!(check_non_negative("content_height", ok).is_ok(), "{ok}");
        }
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.5, -1e9] {
            let err = check_non_negative("content_height", bad).expect_err("bad");
            assert_eq!(err.code, IpcErrorCode::InvalidArgument, "{bad}");
        }
        // -0.0 不小于 0，视为 0
        assert!(check_non_negative("content_height", -0.0).is_ok());
    }
}
