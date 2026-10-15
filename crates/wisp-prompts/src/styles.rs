//! 内置风格（6 个，顺序固定）与 override 合并。
//!
//! 内置风格以代码为准；用户对内置风格的修改以 override 的形式存进 styles.json，只保存与默认值不同的字段，
//! 未覆写的字段跟随新版本。`builtin` / `modified` 由这里重新计算，文件里的值不可信。

use std::collections::HashSet;

use wisp_types::{OutputFormat, Style, StyleOverride, StylesFile};

use crate::templates::{
    ACADEMIC_SYSTEM, ACADEMIC_TAIL, CASUAL_SYSTEM, CASUAL_TAIL, LEARN_SYSTEM, LEARN_TAIL, LITERAL_SYSTEM, LITERAL_TAIL,
    NATURAL_SYSTEM, NATURAL_TAIL, TECHNICAL_SYSTEM, TECHNICAL_TAIL,
};

/// 6 个内置风格的 id，顺序即 chip 顺序与 ⌘1–6 的顺序
pub const BUILTIN_STYLE_IDS: [&str; 6] = ["natural", "technical", "academic", "casual", "learn", "literal"];

/// 读取 styles.json 时容忍的 temperature 范围（超出或不是有限数时视为手改出错，回退到风格的默认值）。
///
/// 这是**宽容加载**的范围，取各协议都接受的上限 2；界面上的编辑范围是 0–1，由设置的校验与界面
/// （desktop 的 `settings::validate` 与风格管理页）负责。在 1–2 之间的手改值照常生效，不在这里截断。
const TEMPERATURE_RANGE: std::ops::RangeInclusive<f32> = 0.0..=2.0;

/// 是否为内置风格 id（内置风格不可删除）
pub fn is_builtin_style_id(id: &str) -> bool {
    BUILTIN_STYLE_IDS.contains(&id)
}

fn builtin(id: &str, system: &str, tail: &str, temperature: f32, output: OutputFormat, dictionary_mode: bool) -> Style {
    Style {
        id: id.to_owned(),
        // 内置风格的显示名由前端按 styles.<id> 的 i18n key 给出
        name: String::new(),
        builtin: true,
        system_template: system.to_owned(),
        tail_instruction: tail.to_owned(),
        temperature: Some(temperature),
        output,
        dictionary_mode,
        model_ref: None,
        hotkey: None,
        modified: false,
    }
}

/// 6 个内置风格，顺序固定（natural / technical / academic / casual / learn / literal）
///
/// 参数：natural 0.3 PlainText、technical 0.2 MarkdownLite、academic 0.2 PlainText、
/// casual 0.6 PlainText、learn 0.3 MarkdownLite、literal 0.2 PlainText；literal 不进词典式，其余都进。
pub fn builtin_styles() -> Vec<Style> {
    use OutputFormat::{MarkdownLite, PlainText};
    vec![
        builtin("natural", NATURAL_SYSTEM, NATURAL_TAIL, 0.3, PlainText, true),
        builtin("technical", TECHNICAL_SYSTEM, TECHNICAL_TAIL, 0.2, MarkdownLite, true),
        builtin("academic", ACADEMIC_SYSTEM, ACADEMIC_TAIL, 0.2, PlainText, true),
        builtin("casual", CASUAL_SYSTEM, CASUAL_TAIL, 0.6, PlainText, true),
        builtin("learn", LEARN_SYSTEM, LEARN_TAIL, 0.3, MarkdownLite, true),
        builtin("literal", LITERAL_SYSTEM, LITERAL_TAIL, 0.2, PlainText, false),
    ]
}

/// 内置 + override + 自定义，按 order 排序；order 中不存在的 id 忽略，新增的风格追加到末尾
///
/// - 内置风格固定 6 个；`modified` 按合并（并修正越界 temperature）之后的结果与默认值是否有差异重算——空的
///   override、只含越界 temperature 的 override、字段值与默认相同的 override 都不算修改；指向非内置 id 的 override 忽略；
/// - 自定义风格：id 为空、与内置风格同名或重复的条目丢弃（保留第一条）；`builtin` / `modified` 一律重算为 false；
/// - 手改出错的 temperature（超出宽容加载范围 0–2 或不是有限数）回退到该风格的默认值；
/// - 排序：先按 `order` 中出现的顺序（重复 id 只算第一次），其余按「内置固定顺序 → 自定义在文件中的顺序」追加。
pub fn resolve_styles(file: &StylesFile) -> Vec<Style> {
    let mut pool: Vec<Style> = builtin_styles()
        .into_iter()
        .map(|base| match file.overrides.get(&base.id) {
            Some(ov) if !ov.is_empty() => {
                let mut s = apply_override(&base, ov);
                if !s.temperature.is_none_or(valid_temperature) {
                    log::warn!(
                        "styles.json: override temperature for {} is out of range, using default",
                        base.id
                    );
                    s.temperature = base.temperature;
                }
                // 「已修改 / 还原默认」只看最终结果：与内置默认完全相同时不算修改
                s.modified = !diff_override(&base, &s).is_empty();
                s
            }
            _ => base,
        })
        .collect();

    for id in file.overrides.keys().filter(|id| !is_builtin_style_id(id)) {
        log::warn!("styles.json: ignoring override for unknown builtin style {id:?}");
    }

    let mut seen: HashSet<&str> = BUILTIN_STYLE_IDS.iter().copied().collect();
    for custom in &file.custom {
        let id = custom.id.trim();
        if id.is_empty() || id != custom.id || !seen.insert(id) {
            log::warn!(
                "styles.json: dropping custom style with empty, reserved or duplicate id {:?}",
                custom.id
            );
            continue;
        }
        let mut s = custom.clone();
        s.builtin = false;
        s.modified = false;
        if !s.temperature.is_none_or(valid_temperature) {
            log::warn!("styles.json: temperature of custom style {id} is out of range, using default");
            s.temperature = Some(wisp_types::CUSTOM_STYLE_TEMPERATURE);
        }
        pool.push(s);
    }

    let mut ordered: Vec<Style> = Vec::with_capacity(pool.len());
    for id in &file.order {
        if let Some(pos) = pool.iter().position(|s| &s.id == id) {
            ordered.push(pool.remove(pos));
        }
    }
    ordered.extend(pool);
    ordered
}

fn valid_temperature(t: f32) -> bool {
    t.is_finite() && TEMPERATURE_RANGE.contains(&t)
}

/// 把 override 应用到内置风格上（结果的 modified = true）
pub fn apply_override(base: &Style, ov: &StyleOverride) -> Style {
    let mut s = base.clone();
    if let Some(v) = &ov.system_template {
        s.system_template.clone_from(v);
    }
    if let Some(v) = &ov.tail_instruction {
        s.tail_instruction.clone_from(v);
    }
    if let Some(v) = ov.temperature {
        s.temperature = v;
    }
    if let Some(v) = ov.output {
        s.output = v;
    }
    if let Some(v) = ov.dictionary_mode {
        s.dictionary_mode = v;
    }
    if let Some(v) = &ov.model_ref {
        s.model_ref.clone_from(v);
    }
    if let Some(v) = &ov.hotkey {
        s.hotkey.clone_from(v);
    }
    s.modified = true;
    s
}

/// 由编辑后的风格计算最小 override：只包含与内置默认值不同的字段。
///
/// 结果为空（[`StyleOverride::is_empty`]）表示与默认值完全相同，调用方应从 styles.json 中删除该条 override。
/// `id` / `name` / `builtin` / `modified` 不属于 override。
pub fn diff_override(base: &Style, edited: &Style) -> StyleOverride {
    fn changed<T: PartialEq + Clone>(base: &T, edited: &T) -> Option<T> {
        (base != edited).then(|| edited.clone())
    }
    StyleOverride {
        system_template: changed(&base.system_template, &edited.system_template),
        tail_instruction: changed(&base.tail_instruction, &edited.tail_instruction),
        temperature: changed(&base.temperature, &edited.temperature),
        output: changed(&base.output, &edited.output),
        dictionary_mode: changed(&base.dictionary_mode, &edited.dictionary_mode),
        model_ref: changed(&base.model_ref, &edited.model_ref),
        hotkey: changed(&base.hotkey, &edited.hotkey),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use wisp_types::ModelRef;

    use super::*;

    fn builtin_by_id(id: &str) -> Option<Style> {
        builtin_styles().into_iter().find(|s| s.id == id)
    }

    #[test]
    fn builtin_parameters_match_prompt_021_026() {
        let styles = builtin_styles();
        let got: Vec<(&str, Option<f32>, OutputFormat, bool)> = styles
            .iter()
            .map(|s| (s.id.as_str(), s.temperature, s.output, s.dictionary_mode))
            .collect();
        assert_eq!(
            got,
            vec![
                ("natural", Some(0.3), OutputFormat::PlainText, true),
                ("technical", Some(0.2), OutputFormat::MarkdownLite, true),
                ("academic", Some(0.2), OutputFormat::PlainText, true),
                ("casual", Some(0.6), OutputFormat::PlainText, true),
                ("learn", Some(0.3), OutputFormat::MarkdownLite, true),
                ("literal", Some(0.2), OutputFormat::PlainText, false),
            ]
        );
        for s in &styles {
            assert!(s.builtin && !s.modified && s.name.is_empty() && s.model_ref.is_none() && s.hotkey.is_none());
            assert!(!s.system_template.is_empty() && !s.tail_instruction.is_empty());
        }
        assert_eq!(
            styles.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            BUILTIN_STYLE_IDS
        );
        assert!(is_builtin_style_id("literal") && !is_builtin_style_id("custom-1"));
        assert_eq!(builtin_by_id("casual").map(|s| s.temperature), Some(Some(0.6)));
        assert!(builtin_by_id("nope").is_none());
    }

    #[test]
    fn override_roundtrip_with_diff() {
        let base = builtin_by_id("natural").expect("natural");
        let mut edited = base.clone();
        edited.temperature = Some(0.5);
        edited.model_ref = Some(ModelRef {
            provider_id: "p".to_owned(),
            model_id: "m".to_owned(),
        });
        let ov = diff_override(&base, &edited);
        assert_eq!(ov.temperature, Some(Some(0.5)));
        assert!(ov.system_template.is_none() && ov.tail_instruction.is_none() && ov.output.is_none());
        let applied = apply_override(&base, &ov);
        assert_eq!(applied.temperature, Some(0.5));
        assert_eq!(applied.model_ref, edited.model_ref);
        assert!(applied.modified);
        // 还原：与默认相同时 diff 为空
        assert!(diff_override(&base, &base).is_empty());
        // 显式「不发 temperature」
        edited.temperature = None;
        let ov = diff_override(&base, &edited);
        assert_eq!(ov.temperature, Some(None));
        assert_eq!(apply_override(&base, &ov).temperature, None);
    }

    #[test]
    fn resolve_orders_and_filters() {
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "natural".to_owned(),
            StyleOverride {
                temperature: Some(Some(0.5)),
                ..StyleOverride::default()
            },
        );
        overrides.insert("academic".to_owned(), StyleOverride::default());
        overrides.insert("ghost".to_owned(), StyleOverride::default());
        overrides.insert(
            "casual".to_owned(),
            StyleOverride {
                temperature: Some(Some(9.0)),
                ..StyleOverride::default()
            },
        );
        // 字段值与默认相同的 override
        overrides.insert(
            "learn".to_owned(),
            StyleOverride {
                temperature: Some(Some(0.3)),
                dictionary_mode: Some(true),
                ..StyleOverride::default()
            },
        );
        // 越界 temperature + 真实修改
        overrides.insert(
            "technical".to_owned(),
            StyleOverride {
                temperature: Some(Some(f32::NAN)),
                tail_instruction: Some("Reply in a code block.".to_owned()),
                ..StyleOverride::default()
            },
        );
        let mut c1 = Style::custom("custom-aaaa0001", "A");
        c1.builtin = true;
        c1.modified = true;
        let c2 = Style::custom("custom-aaaa0002", "B");
        let dup = Style::custom("custom-aaaa0001", "dup");
        let reserved = Style::custom("natural", "fake");
        let empty = Style::custom("", "empty");
        let mut bad_temp = Style::custom("custom-aaaa0003", "C");
        bad_temp.temperature = Some(-1.0);
        let file = StylesFile {
            schema_version: 1,
            order: vec![
                "custom-aaaa0002".to_owned(),
                "literal".to_owned(),
                "missing".to_owned(),
                "literal".to_owned(),
            ],
            overrides,
            custom: vec![c1, c2, dup, reserved, empty, bad_temp],
        };
        let styles = resolve_styles(&file);
        let ids: Vec<&str> = styles.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "custom-aaaa0002",
                "literal",
                "natural",
                "technical",
                "academic",
                "casual",
                "learn",
                "custom-aaaa0001",
                "custom-aaaa0003",
            ]
        );
        let by = |id: &str| styles.iter().find(|s| s.id == id).expect(id);
        assert_eq!(by("natural").temperature, Some(0.5));
        assert!(by("natural").modified);
        assert!(!by("academic").modified, "空 override 不算修改");
        assert_eq!(by("casual").temperature, Some(0.6), "越界 temperature 回退默认");
        assert!(
            !by("casual").modified,
            "只含越界 temperature 的 override 回退后与默认相同，不算修改"
        );
        assert!(!by("learn").modified, "字段值与默认相同的 override 不算修改");
        assert_eq!(by("technical").temperature, Some(0.2));
        assert_eq!(by("technical").tail_instruction, "Reply in a code block.");
        assert!(by("technical").modified);
        // 1–2 之间的手改值在宽容加载范围内，照常生效
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "natural".to_owned(),
            StyleOverride {
                temperature: Some(Some(1.5)),
                ..StyleOverride::default()
            },
        );
        let lenient = resolve_styles(&StylesFile {
            overrides,
            ..StylesFile::default()
        });
        assert_eq!(lenient[0].temperature, Some(1.5));
        assert!(lenient[0].modified);
        assert!(!by("custom-aaaa0001").builtin && !by("custom-aaaa0001").modified);
        assert_eq!(by("custom-aaaa0001").name, "A");
        assert_eq!(
            by("custom-aaaa0003").temperature,
            Some(wisp_types::CUSTOM_STYLE_TEMPERATURE)
        );
    }

    #[test]
    fn default_file_resolves_to_builtins() {
        let styles = resolve_styles(&StylesFile::default());
        assert_eq!(styles, builtin_styles());
    }
}
