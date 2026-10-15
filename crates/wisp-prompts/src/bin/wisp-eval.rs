//! 提示词评测 CLI（feature `eval`）。

use std::process::ExitCode;

#[allow(clippy::print_stderr)] // 命令行工具：错误直接输出到 stderr
fn main() -> ExitCode {
    let args = match wisp_prompts::eval::parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
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
