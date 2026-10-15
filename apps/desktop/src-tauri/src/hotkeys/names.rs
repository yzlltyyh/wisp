//! 冲突提示里的目标名称（`hotkeys.conflict.self_duplicate` 的 `{action}` 参数）：前端把参数原样插进文案，名称必须由
//! Rust 按界面语言给出。
//!
//! 唯一来源是前端 locales（编译期 `include_str!`）：动作名取 `hotkeys.json` 的 `action.<id>`，
//! 内置风格名取 `styles.json` 的 `<id>`；自定义风格用用户起的名字。这样设置页、风格菜单与冲突提示里的名字永远一致。

use std::sync::OnceLock;

use serde_json::Value;
use wisp_types::{HotkeyAction, HotkeyTarget, Style};

use crate::app::i18n::Lang;

const HOTKEYS_ZH: &str = include_str!("../../../src/locales/zh-CN/hotkeys.json");
const HOTKEYS_EN: &str = include_str!("../../../src/locales/en-US/hotkeys.json");
const STYLES_ZH: &str = include_str!("../../../src/locales/zh-CN/styles.json");
const STYLES_EN: &str = include_str!("../../../src/locales/en-US/styles.json");

/// 解析后的四份文案（首次使用时解析一次）
struct Tables {
    hotkeys_zh: Value,
    hotkeys_en: Value,
    styles_zh: Value,
    styles_en: Value,
}

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let parse = |s: &str| serde_json::from_str(s).unwrap_or(Value::Null);
        Tables {
            hotkeys_zh: parse(HOTKEYS_ZH),
            hotkeys_en: parse(HOTKEYS_EN),
            styles_zh: parse(STYLES_ZH),
            styles_en: parse(STYLES_EN),
        }
    })
}

fn lookup<'a>(root: &'a Value, path: &[&str]) -> Option<&'a str> {
    path.iter().try_fold(root, |v, k| v.get(*k)).and_then(Value::as_str)
}

/// 动作的显示名
pub fn action_name(lang: Lang, action: HotkeyAction) -> String {
    let t = tables();
    let root = match lang {
        Lang::ZhCn => &t.hotkeys_zh,
        Lang::EnUs => &t.hotkeys_en,
    };
    lookup(root, &["action", action.as_str()]).map_or_else(|| action.as_str().to_owned(), str::to_owned)
}

/// 风格的显示名：自定义风格用自己的名字，内置风格用前端文案，都取不到时用 id
pub fn style_name(lang: Lang, style_id: &str, styles: &[Style]) -> String {
    let style = styles.iter().find(|s| s.id == style_id);
    if let Some(s) = style
        && !s.name.trim().is_empty()
    {
        return s.name.trim().to_owned();
    }
    let t = tables();
    let root = match lang {
        Lang::ZhCn => &t.styles_zh,
        Lang::EnUs => &t.styles_en,
    };
    lookup(root, &[style_id]).map_or_else(|| style_id.to_owned(), str::to_owned)
}

/// 任一目标的显示名
pub fn target_name(lang: Lang, target: &HotkeyTarget, styles: &[Style]) -> String {
    match target {
        HotkeyTarget::Action { action } => action_name(lang, *action),
        HotkeyTarget::Style { style_id } => style_name(lang, style_id, styles),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_and_builtin_style_has_both_languages() {
        for action in HotkeyAction::ALL {
            for lang in [Lang::ZhCn, Lang::EnUs] {
                assert_ne!(
                    action_name(lang, action),
                    action.as_str(),
                    "{lang:?} 缺少 action.{}",
                    action.as_str()
                );
            }
        }
        for id in wisp_types::defaults::BUILTIN_STYLE_IDS {
            for lang in [Lang::ZhCn, Lang::EnUs] {
                assert_ne!(style_name(lang, id, &[]), id, "{lang:?} 缺少 styles.{id}");
            }
        }
        assert_eq!(action_name(Lang::ZhCn, HotkeyAction::TranslateSelection), "划词翻译");
        assert_eq!(style_name(Lang::EnUs, "academic", &[]), "Academic");
    }

    #[test]
    fn custom_styles_use_their_own_name() {
        let custom = Style::custom("custom-1a2b3c4d", "  术语表  ");
        assert_eq!(style_name(Lang::EnUs, "custom-1a2b3c4d", &[custom]), "术语表");
        assert_eq!(style_name(Lang::ZhCn, "custom-missing", &[]), "custom-missing");
    }
}
