//! Wisp 桌面应用的装配入口。
//!
//! 本文件只做装配：运行时、插件、协议、setup、命令与运行事件。
//! 约定：`#[cfg(target_os)]` 在应用层只允许出现在本文件的插件注册与 `windows/native/`；
//! 引用自己的 `windows` 模块一律写 `crate::windows::…`，引用 `windows` crate 一律写 `::windows::…`。

pub mod app;
pub mod assets;
pub mod bus;
pub mod clipboard_svc;
pub mod error;
pub mod hotkeys;
pub mod ipc;
pub mod orchestrator;
pub mod settings;
pub mod state;
pub mod tray;
pub mod updater;
pub mod windows;

/// 应用入口
pub fn run() {
    // 第一步、进程仍是单线程时：Linux 检测到 Nvidia 时设置 WEBKIT_DISABLE_DMABUF_RENDERER=1（set_var 只有单线程时才安全），
    // Wayland / 关闭合成时关闭窗口透明（结论见 runtime::transparent_windows）。必须早于 build_runtime 拉起 tokio worker
    app::runtime::preflight_env();
    // tokio 运行时限制为 2 个 worker，必须在 Builder 之前设置；build_runtime 内部 Box::leak 常驻 Runtime 并返回 Handle
    tauri::async_runtime::set(app::runtime::build_runtime());
    let builder = tauri::Builder::default();
    // 必须第一个注册。冒烟模式（WISP_SMOKE=1，数据在隔离的临时目录）不注册：否则本机已有 Wisp 在运行时冒烟进程会被
    // 转发给它并以 0 退出，报告根本不会写出
    let builder = if app::smoke::enabled() {
        builder
    } else {
        builder.plugin(tauri_plugin_single_instance::init(|app, args, cwd| {
            app::second_instance::on_second_instance(app, args, cwd)
        }))
    };
    // manage WebviewPanelManager；to_panel / get_webview_panel 依赖它
    #[cfg(target_os = "macos")]
    let builder = builder.plugin(tauri_nspanel::init());
    let builder = builder
        // tauri-plugin-log：写入 log_dir/wisp/wisp.log，2 MB × 5（路径用 dirs crate 在 Builder 之前算出）
        .plugin(app::logging::plugin())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_autostart::Builder::new().args(["--autostart"]).build());
    // 纯 Wayland 且没有 DISPLAY 时 GlobalHotKeyManager::new() 会失败并让 build() 出错，因此按运行时检测决定是否注册；
    // 未注册时 HotkeyService 报告 unavailable，且不得调用 app.global_shortcut()
    let builder = if app::runtime::global_shortcut_supported() {
        builder.plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(hotkeys::on_shortcut)
                .build(),
        )
    } else {
        builder
    };
    // wisp:// 异步 URI scheme
    let builder = crate::windows::protocol::register(builder);
    builder
        // 返回 Result；内部负责 manage(AppState)
        .setup(app::bootstrap::setup)
        // ipc/mod.rs 中的 generate_handler![...]
        .invoke_handler(ipc::handler())
        .build(tauri::generate_context!())
        .expect("构建 Tauri 应用失败")
        // ExitRequested 时调用 prevent_exit，保持常驻
        .run(app::bootstrap::on_run_event);
}
