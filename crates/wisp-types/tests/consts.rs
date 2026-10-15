//! 由 Rust 数据表生成的前端常量样例：`apps/desktop/src/shared/ipc/fixtures/consts/HotkeyDefaults.json`。
//!
//! 内容是三端默认快捷键表（`defaults::MACOS_HOTKEYS` / `WINDOWS_HOTKEYS` / `LINUX_HOTKEYS`）：
//! `{ "<os>": { "<action>": { "preferred": "<首选>", "fallback": "<备选>" } } }`，平台按 macOS / Windows / Linux、动作按
//! `HotkeyAction::ALL` 的顺序排列（与设置页、托盘的顺序一致）。前端只用它**显示**「Rust 这次尝试的是哪个组合」
//! （`HotkeyStatus` 在默认绑定注册失败时 `effective` 为空），注册与回退仍由 Rust 决定。
//!
//! 默认比对文件内容，不一致即失败；`UPDATE_GOLDEN=1 cargo test -p wisp-types --test consts` 重写
//! （或 `node scripts/gen-fixtures.mjs` 一次重写全部由 Rust 生成的样例）。

use std::path::{Path, PathBuf};

use serde::ser::{Serialize, Serializer};
use wisp_types::defaults::hotkey_table;
use wisp_types::{HotkeyAction, OsKind};

/// 按给定顺序序列化成 JSON 对象（serde_json 的 Map 默认按键排序，这里要保留平台 / 动作的固定顺序）
struct Ordered<K, V>(Vec<(K, V)>);

impl<K: Serialize, V: Serialize> Serialize for Ordered<K, V> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.0.iter().map(|(k, v)| (k, v)))
    }
}

#[derive(serde::Serialize)]
struct Combo {
    preferred: &'static str,
    fallback: &'static str,
}

fn consts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/desktop/src/shared/ipc/fixtures/consts")
}

fn update_mode() -> bool {
    std::env::var("UPDATE_GOLDEN").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// 写出或比对一个样例文件
fn sync(name: &str, expected: &str, how: &str) {
    let path = consts_dir().join(name);
    if update_mode() {
        std::fs::create_dir_all(consts_dir()).expect("创建 fixtures/consts 目录失败");
        std::fs::write(&path, expected).unwrap_or_else(|e| panic!("写入 {} 失败：{e}", path.display()));
        return;
    }
    let actual = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        actual == expected,
        "fixtures/consts/{name} 与 Rust 数据不一致（确认改动后运行 {how}，或 node scripts/gen-fixtures.mjs）"
    );
}

#[test]
fn golden_hotkey_defaults() {
    let table = Ordered(
        [OsKind::Macos, OsKind::Windows, OsKind::Linux]
            .into_iter()
            .map(|os| {
                let rows = hotkey_table(os);
                let actions = Ordered(
                    HotkeyAction::ALL
                        .into_iter()
                        .map(|action| {
                            let row = rows
                                .iter()
                                .find(|r| r.action == action)
                                .unwrap_or_else(|| panic!("{os:?} 的默认表缺少 {action:?}"));
                            (
                                action,
                                Combo {
                                    preferred: row.primary,
                                    fallback: row.fallback,
                                },
                            )
                        })
                        .collect(),
                );
                (os, actions)
            })
            .collect(),
    );
    let expected = format!("{}\n", serde_json::to_string_pretty(&table).expect("序列化失败"));
    sync(
        "HotkeyDefaults.json",
        &expected,
        "UPDATE_GOLDEN=1 cargo test -p wisp-types --test consts",
    );
}
