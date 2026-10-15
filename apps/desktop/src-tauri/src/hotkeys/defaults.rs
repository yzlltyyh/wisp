//! 当前平台的默认快捷键表（数据在 `wisp_types::defaults`）。

use wisp_types::defaults::{HotkeyDefault, hotkey_table};
use wisp_types::{HotkeyAction, OsKind};

/// 当前平台的默认表
pub fn table() -> &'static [HotkeyDefault; 5] {
    hotkey_table(OsKind::current())
}

/// 当前平台某动作的默认快捷键
pub fn for_action(action: HotkeyAction) -> HotkeyDefault {
    wisp_types::defaults::hotkey_default(OsKind::current(), action)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::str::FromStr;

    use tauri_plugin_global_shortcut::{Code, Modifiers, Shortcut};
    use wisp_types::defaults::hotkey_table;

    use super::*;

    /// 三份默认表的首选键与备选键都能被 global-hotkey 真正解析，且同一张表内（解析后的 修饰键 + 主键）不重复
    #[test]
    fn tables_parse_and_have_no_duplicates() {
        for os in [OsKind::Macos, OsKind::Windows, OsKind::Linux] {
            let mut seen: HashSet<(Modifiers, Code)> = HashSet::new();
            for d in hotkey_table(os) {
                for combo in [d.primary, d.fallback] {
                    let sc = Shortcut::from_str(combo)
                        .unwrap_or_else(|e| panic!("{os:?} {:?}: {combo} 无法解析：{e}", d.action));
                    assert!(
                        seen.insert((sc.mods, sc.key)),
                        "{os:?} 的默认表里 {combo} 重复（按解析结果比较）"
                    );
                }
            }
            assert_eq!(seen.len(), 10, "{os:?}：5 个动作 × 首选 / 备选");
        }
    }

    /// 解析结果与规范串一一对应：global-hotkey 的 `into_string()` 输出与规范串不同，比较必须基于解析结果
    #[test]
    fn canonical_strings_round_trip_through_parse() {
        let a = Shortcut::from_str("Control+Alt+Shift+KeyS").expect("规范串");
        let b = Shortcut::from_str("shift+control+alt+KeyS").expect("into_string 风格");
        assert_eq!((a.mods, a.key), (b.mods, b.key));
        assert_ne!(
            a.into_string(),
            "Control+Alt+Shift+KeyS",
            "into_string 不是规范串，禁止直接比较字符串"
        );
    }
}
