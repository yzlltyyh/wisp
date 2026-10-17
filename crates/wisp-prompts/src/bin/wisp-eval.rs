//! 提示词评测 CLI（feature `eval`）。

use std::process::ExitCode;

#[allow(clippy::print_stderr, clippy::print_stdout)] // 命令行工具：帮助写到 stdout，错误写到 stderr
fn main() -> ExitCode {
    let args = match wisp_prompts::eval::parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        // 主动要帮助不算出错：照惯例写到 stdout、退出码 0，便于 `| less` 或脚本探测
        Err(e) if e == wisp_prompts::eval::HELP => {
            println!("{e}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("wisp-eval: {e}");
            return ExitCode::from(2);
        }
    };
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("wisp-eval: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(wisp_prompts::eval::run(args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wisp-eval: {e}");
            ExitCode::FAILURE
        }
    }
}
