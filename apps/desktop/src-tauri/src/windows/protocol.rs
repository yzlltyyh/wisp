//! `wisp://` 异步 URI scheme：在阻塞线程上调用 `AssetRegistry::respond`（CORS / OPTIONS / percent-decode 见 assets.rs），
//! **回到主线程再把响应交给 WebView**。
//!
//! 为什么要回主线程（实测到的崩溃）：wry 的响应闭包先检查「这个 URL scheme 任务还有效」，再调用
//! `WKURLSchemeTask` 的 `didReceiveResponse:` / `didReceiveData:` / `didFinish`。在阻塞线程上调用时，检查与调用之间主线程
//! 可能正好销毁了这个 WebView（截图 Overlay 上屏后很快按 Esc、面板回收……），WebKit 先 `stopURLSchemeTask`，随后的
//! `didReceiveResponse:` 抛出 Objective-C 异常（任务已停止）。release 构建是 `panic = "abort"`，异常穿过 Rust 栈帧时直接
//! abort 整个进程（wry 外面那层 `objc2::exception::catch` 的 C 帧在 Rust 帧之外，abort 模式下接不住）；同时响应闭包持有
//! WebView 的引用，WebView 先被销毁时最后一次释放会落在阻塞线程上（WebKit 要求在主线程）。`stopURLSchemeTask` 与 WebView 的
//! 销毁都在主线程上，响应也放到主线程：检查与调用之间不会再插进销毁，WebView 的最后一次释放也一定在主线程。重活（读帧、
//! 裁剪、色彩转换）仍在阻塞线程上，主线程上只剩组装响应（`NSData` 接管缓冲，不拷贝）。
//!
//! 同一类竞态在 Tauri 的 `ipc://`（异步命令在完成的线程上响应）里同样存在，由 `vendor/wry` 的第 5 处补丁统一处理：wry 的响应
//! 闭包不在主线程时自己投递到主线程。这里的跳转保留——到 wry 时已在主线程，原地执行，不多一次投递；
//! 在 Windows / Linux 上同样让响应与窗口销毁在同一个线程上排队。

use tauri::Manager;

use crate::state::AppState;

/// 注册 `wisp://` 协议
pub fn register(builder: tauri::Builder<tauri::Wry>) -> tauri::Builder<tauri::Wry> {
    builder.register_asynchronous_uri_scheme_protocol("wisp", |ctx, request, responder| {
        let app = ctx.app_handle().clone();
        tauri::async_runtime::spawn_blocking(move || {
            let response = match app.try_state::<AppState>() {
                Some(state) => state.assets.respond(&request),
                None => crate::assets::unavailable_response(),
            };
            // 事件循环已经结束（退出中）时闭包连同响应一起被丢弃：WebView 也正在销毁，没有谁在等它
            if let Err(e) = app.run_on_main_thread(move || responder.respond(response)) {
                log::debug!("wisp:// response dropped, main thread unavailable: {e}");
            }
        });
    })
}
