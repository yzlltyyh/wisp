//! ORT 工作进程的独立入口：与 desktop 以 `--wisp-worker ort` 启动的模式执行同一个函数（`wisp_local::worker::run`）。
//! 集成测试与真机测量用它走真正的进程与管道协议，而不必启动 Wisp 本体（正式版占着单实例）。

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    // 与 desktop 相同的命令行：`--wisp-worker ort`；也接受省略开关的 `ort`
    let rest = match argv.first().map(String::as_str) {
        Some(wisp_local::worker::WORKER_ARG) => &argv[1..],
        _ => &argv[..],
    };
    std::process::exit(wisp_local::worker::run(rest));
}
