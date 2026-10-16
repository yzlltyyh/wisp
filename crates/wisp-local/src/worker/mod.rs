//! ORT 工作进程：PP-OCRv6 与 PP-DocLayoutV3 的推理放在 Wisp 自己的二进制以 `--wisp-worker ort` 启动的子进程里。
//!
//! 为什么是子进程：ONNX Runtime 的动态库加载后无法卸载、环境每个进程只能建一次、丢弃会话后仍残留 40–60 MB；放进子进程，
//! 闲置时结束进程就能彻底归还内存，ORT 崩溃也只影响这一次识别。遥测开关（`ORT_DISABLE_TELEMETRY=1`）只需设在子进程的环境里。
//!
//! - 启动：`<current_exe> --wisp-worker ort`，环境变量 [`ENV_DYLIB`]（ONNX Runtime 动态库的绝对路径）、[`ENV_THREADS`]
//!   （`clamp(性能核数 / 2, 1, 4)`）、[`ENV_MODELS`]（模型目录）、`ORT_DISABLE_TELEMETRY=1`；优先级略低于正常；Windows 加入
//!   Job Object，Unix 不需要守护进程（自身以 stdin EOF 退出）。
//! - 协议：stdin / stdout 上的长度前缀帧（[`frame`]），单请求串行。
//! - 会话按需创建、进程内复用；关闭 CPU 内存 arena 与 memory pattern；intra 线程 = n，inter = 1。
//! - 只把固定的错误种类写在响应里；不写 wisp.log（子进程不初始化日志），stderr 只在动态库都加载不了这类致命情况下写一个
//!   固定的单词。

pub mod client;
pub mod frame;
mod serve;

pub use client::{OrtWorker, WorkerConfig};

/// 工作进程模式的命令行开关（desktop 的 `run()` 在任何初始化之前识别它）
pub const WORKER_ARG: &str = "--wisp-worker";
/// `--wisp-worker` 之后的种类：ONNX Runtime
pub const WORKER_ORT: &str = "ort";
/// ONNX Runtime 动态库的绝对路径
pub const ENV_DYLIB: &str = "WISP_ORT_DYLIB";
/// 推理线程数
pub const ENV_THREADS: &str = "WISP_ORT_THREADS";
/// 模型目录（`models/`），模型文件按 [`crate::store::Store`] 的布局查找
pub const ENV_MODELS: &str = "WISP_ORT_MODELS";
/// 关闭 ONNX Runtime 遥测的环境变量（Microsoft 的官方构建默认开启）
pub const ENV_DISABLE_TELEMETRY: &str = "ORT_DISABLE_TELEMETRY";

/// 工作进程的入口：`argv` 是 `--wisp-worker` 之后的参数（第一个是种类）。读 stdin 的请求直到 EOF 或 `exit`，返回进程退出码。
/// 不初始化 Tauri、日志或任何 GUI；stdout 只写协议帧
pub fn run(argv: &[String]) -> i32 {
    match argv.first().map(String::as_str) {
        Some(WORKER_ORT) => serve::serve_stdio(),
        _ => 2,
    }
}
