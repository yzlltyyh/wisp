//! styles.json 的读写：内置风格的权威版本在代码里；override 只保存与默认值不同的字段；「还原默认」就是删掉对应的 override。
//!
//! 合并与排序由 wisp-prompts 的 `resolve_styles` 完成，override 的差分用 `wisp_prompts::diff_override`；本模块负责
//! 文件读写、规范化与 `builtin` / `modified` 标志的重新计算（文件里的值不可信）。

use std::path::Path;

use wisp_types::{Style, StyleOverride, StylesFile};

use crate::error::IpcError;
use crate::settings::{fsio, migrate};

/// 读取 styles.json（不存在时返回默认值；损坏时备份并返回默认值）
pub fn load(path: &Path) -> Result<StylesFile, IpcError> {
    Ok(load_with_backup(path)?.value)
}

/// 读取 styles.json，同时返回损坏时的备份路径与宽松反序列化的问题列表
pub fn load_with_backup(path: &Path) -> Result<fsio::Loaded<StylesFile>, IpcError> {
    let mut loaded = fsio::load_document::<StylesFile>(path, |v| {
        migrate::run(v, migrate::DOC_SCHEMA_VERSION, migrate::STYLES_STEPS)
    })?;
    loaded.value.schema_version = migrate::DOC_SCHEMA_VERSION;
    Ok(loaded)
}

/// 原子写入 styles.json
pub fn save(path: &Path, file: &StylesFile) -> Result<(), IpcError> {
    fsio::write_atomic(path, &encode(file)?)?;
    Ok(())
}

/// 序列化（带缩进、换行结尾）
pub fn encode(file: &StylesFile) -> Result<Vec<u8>, IpcError> {
    Ok(fsio::to_pretty_json(file)?)
}

/// 是否为内置风格 id
pub fn is_builtin(id: &str) -> bool {
    wisp_prompts::is_builtin_style_id(id)
}

/// 由内置风格的默认版本与编辑后的版本得出 override：只保留不同的字段（算法与 wisp-prompts 同源）
pub fn override_diff(base: &Style, edited: &Style) -> StyleOverride {
    wisp_prompts::diff_override(base, edited)
}

/// 解析：`wisp_prompts::resolve_styles` 之后重新计算 `builtin` / `modified`（文件里的值不可信）
pub fn resolve(file: &StylesFile) -> Vec<Style> {
    let mut styles = wisp_prompts::resolve_styles(file);
    for s in &mut styles {
        s.builtin = is_builtin(&s.id);
        s.modified = s.builtin && file.overrides.get(&s.id).is_some_and(|o| !o.is_empty());
        if s.builtin {
            // 内置风格的显示名由前端按 styles.<id> 取 i18n
            s.name.clear();
        }
    }
    styles
}

/// 规范化：去掉空的 override、对 order 去重并删掉不存在的 id（内置 id 始终有效）
pub fn normalize(file: &mut StylesFile) {
    file.schema_version = migrate::DOC_SCHEMA_VERSION;
    file.overrides.retain(|id, o| is_builtin(id) && !o.is_empty());
    let mut seen = std::collections::HashSet::new();
    let custom_ids: std::collections::HashSet<&str> = file.custom.iter().map(|s| s.id.as_str()).collect();
    file.order
        .retain(|id| (is_builtin(id) || custom_ids.contains(id.as_str())) && seen.insert(id.clone()));
}

#[cfg(test)]
mod tests {
    use wisp_types::{ModelRef, OutputFormat};

    use super::*;

    fn base() -> Style {
        Style {
            id: "natural".to_owned(),
            name: String::new(),
            builtin: true,
            system_template: "SYS".to_owned(),
            tail_instruction: "TAIL".to_owned(),
            temperature: Some(0.3),
            output: OutputFormat::PlainText,
            dictionary_mode: true,
            model_ref: None,
            hotkey: None,
            modified: false,
        }
    }

    #[test]
    fn diff_of_identical_style_is_empty() {
        assert!(override_diff(&base(), &base()).is_empty());
    }

    #[test]
    fn diff_keeps_only_changed_fields() {
        let mut edited = base();
        edited.temperature = Some(0.5);
        edited.model_ref = Some(ModelRef {
            provider_id: "p".to_owned(),
            model_id: "m".to_owned(),
        });
        let ov = override_diff(&base(), &edited);
        assert_eq!(ov.temperature, Some(Some(0.5)));
        assert!(ov.model_ref.is_some());
        assert!(ov.system_template.is_none());
        assert!(ov.tail_instruction.is_none());
        // 只写出被覆写的键
        let json = serde_json::to_value(&ov).expect("json");
        assert_eq!(json.as_object().map(|o| o.len()), Some(2));
    }

    #[test]
    fn explicit_null_temperature_is_an_override() {
        let mut edited = base();
        edited.temperature = None;
        let ov = override_diff(&base(), &edited);
        assert_eq!(ov.temperature, Some(None));
        assert_eq!(
            serde_json::to_value(&ov).expect("json"),
            serde_json::json!({"temperature": null})
        );
    }

    #[test]
    fn normalize_drops_empty_overrides_and_unknown_order() {
        let mut file = StylesFile::default();
        file.overrides.insert("natural".to_owned(), StyleOverride::default());
        file.overrides.insert(
            "bogus".to_owned(),
            StyleOverride {
                dictionary_mode: Some(false),
                ..StyleOverride::default()
            },
        );
        file.custom.push(Style::custom("custom-1a2b3c4d", "Mine"));
        file.order = vec![
            "custom-1a2b3c4d".to_owned(),
            "natural".to_owned(),
            "missing".to_owned(),
            "natural".to_owned(),
        ];
        normalize(&mut file);
        assert!(file.overrides.is_empty());
        assert_eq!(file.order, vec!["custom-1a2b3c4d".to_owned(), "natural".to_owned()]);
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("styles.json");
        let mut file = StylesFile::default();
        file.custom.push(Style::custom("custom-00000001", "Mine"));
        file.overrides.insert(
            "natural".to_owned(),
            StyleOverride {
                temperature: Some(Some(0.5)),
                ..StyleOverride::default()
            },
        );
        save(&path, &file).expect("save");
        let loaded = load(&path).expect("load");
        assert_eq!(loaded, file);
    }

    #[test]
    fn resolve_recomputes_flags() {
        let mut file = StylesFile::default();
        let mut custom = Style::custom("custom-00000002", "Mine");
        custom.builtin = true;
        custom.modified = true;
        file.custom.push(custom);
        let styles = resolve(&file);
        let c = styles.iter().find(|s| s.id == "custom-00000002").expect("custom");
        assert!(!c.builtin);
        assert!(!c.modified);
        for s in styles.iter().filter(|s| s.builtin) {
            assert!(!s.modified, "没有 override 的内置风格 modified = false");
        }
    }
}
