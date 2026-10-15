//! 构建脚本。

fn main() {
    // Windows 测试二进制的 comctl32 问题：rfd（tauri-plugin-dialog 的依赖）静态导入只有 comctl32 v6 才导出的
    // TaskDialogIndirect，而 tauri-build 嵌入的 manifest 只链接到 bin 目标，lib 单测二进制启动即 STATUS_ENTRYPOINT_NOT_FOUND。
    // 延迟加载 comctl32：测试中从不调用该函数就不会加载；bin 首次调用时在 manifest 的激活上下文中加载 v6。
    // 用环境变量判断目标平台（build.rs 中的 cfg 反映的是宿主平台）。
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os == "windows" && target_env == "msvc" {
        println!("cargo:rustc-link-arg=/DELAYLOAD:comctl32.dll");
        println!("cargo:rustc-link-lib=delayimp");
    }
    tauri_build::build();
}
