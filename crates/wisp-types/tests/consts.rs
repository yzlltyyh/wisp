//! 由 Rust 数据表生成的前端常量样例（`apps/desktop/src/shared/ipc/fixtures/consts/`）：
//!
//! - `HotkeyDefaults.json`：三端默认快捷键表（`defaults::MACOS_HOTKEYS` / `WINDOWS_HOTKEYS` / `LINUX_HOTKEYS`）：
//!   `{ "<os>": { "<action>": { "preferred": "<首选>", "fallback": "<备选>" } } }`，平台按 macOS / Windows / Linux、动作按
//!   `HotkeyAction::ALL` 的顺序排列（与设置页、托盘的顺序一致）。前端只用它**显示**「Rust 这次尝试的是哪个组合」
//!   （`HotkeyStatus` 在默认绑定注册失败时 `effective` 为空），注册与回退仍由 Rust 决定。
//! - `EntityKinds.json` / `TransformIds.json`：`EntityKind::ALL` / `TransformId::ALL` 的字符串（声明顺序）。前端的
//!   类型联合与展示注册表（图标、文案）各写了一份取值，编译期只能保证它们彼此一致；Rust 新增一种识别或变换时，
//!   前端测试拿这份清单比对，漏补的取值在测试里就会失败，而不是到运行时才因为找不到图标与文案出错。
//!
//! 默认比对文件内容，不一致即失败；`UPDATE_GOLDEN=1 cargo test -p wisp-types --test consts` 重写
//! （或 `node scripts/gen-fixtures.mjs` 一次重写全部由 Rust 生成的样例）。

use std::path::{Path, PathBuf};

use serde::ser::{Serialize, Serializer};
use wisp_types::defaults::hotkey_table;
use wisp_types::{EntityKind, HotkeyAction, OsKind, TransformId};

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

/// 字符串枚举的全部取值，按 `ALL` 的顺序写成 JSON 数组
fn id_list(ids: impl IntoIterator<Item = &'static str>) -> String {
    let ids: Vec<_> = ids.into_iter().collect();
    format!("{}\n", serde_json::to_string_pretty(&ids).expect("序列化失败"))
}

#[test]
fn golden_entity_kinds() {
    sync(
        "EntityKinds.json",
        &id_list(EntityKind::ALL.map(EntityKind::as_str)),
        "UPDATE_GOLDEN=1 cargo test -p wisp-types --test consts",
    );
}

#[test]
fn golden_transform_ids() {
    sync(
        "TransformIds.json",
        &id_list(TransformId::ALL.map(TransformId::as_str)),
        "UPDATE_GOLDEN=1 cargo test -p wisp-types --test consts",
    );
}
