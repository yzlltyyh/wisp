//! 由 Rust 生成的前端常量样例：`apps/desktop/src/shared/ipc/fixtures/consts/BuiltinStyles.json`。
//!
//! 内容是 `builtin_styles()`（6 个内置风格，顺序即 chip 顺序）按 `Style` 的 serde 形状序列化的结果——与 `styles_list`
//! 在用户没有任何 override 时返回的内置部分逐字一致。浏览器 mock（`shared/ipc/mock/fixtures.ts` 的 `styles()`）用它代替
//! 手写的占位风格，模板正文、temperature、输出格式、词典式开关都与 Rust 一致。
//!
//! 默认比对文件内容，不一致即失败；`UPDATE_GOLDEN=1 cargo test -p wisp-prompts --test fixtures_consts` 重写
//! （或 `node scripts/gen-fixtures.mjs` 一次重写全部由 Rust 生成的样例）。

use std::path::Path;

use wisp_prompts::builtin_styles;

#[test]
fn golden_builtin_styles() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/desktop/src/shared/ipc/fixtures/consts");
    let path = dir.join("BuiltinStyles.json");
    let expected = format!(
        "{}\n",
        serde_json::to_string_pretty(&builtin_styles()).expect("序列化失败")
    );
    if std::env::var("UPDATE_GOLDEN").is_ok_and(|v| !v.is_empty() && v != "0") {
        std::fs::create_dir_all(&dir).expect("创建 fixtures/consts 目录失败");
        std::fs::write(&path, &expected).unwrap_or_else(|e| panic!("写入 {} 失败：{e}", path.display()));
        return;
    }
    let actual = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        actual == expected,
        "fixtures/consts/BuiltinStyles.json 与 wisp_prompts::builtin_styles() 不一致（确认改动后运行 \
         UPDATE_GOLDEN=1 cargo test -p wisp-prompts --test fixtures_consts，或 node scripts/gen-fixtures.mjs）"
    );
}
