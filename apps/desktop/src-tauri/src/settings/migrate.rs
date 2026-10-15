//! 配置文档的版本迁移：按 `schema_version` 逐级迁移到当前版本。
//!
//! 每份文档（settings / styles / glossary / state.json）各有一张迁移表：`steps[i]` 把 v(i+1) 的 JSON 变成 v(i+2)。
//! 迁移在**宽松反序列化之前**、在 `serde_json::Value` 上进行（旧版本的字段名 / 形状在新结构里已经不存在）。
//! 当前四份文档都是 v1，迁移表为空；以后升级 schema 时在对应的表末尾追加一步，并把版本常量加一。
//!
//! 版本号的处理：缺失、非整数或 0 视为 v1（手写的残缺文件）；高于当前版本（来自更新的 Wisp，用户降级）时不迁移、记 warn，
//! 由宽松反序列化尽量读取。
//!
//! **降级后的写回**（[`Newer`]）：来自更新版本的文档，读取时把原文与本版本重新序列化的结果比较，记下本版本不认识的内容
//! （逐层的未知键；数组里按 `id` 对应的元素中的未知键；本版本读不出、被宽松解析丢掉的数组元素）。之后任何写盘（包括
//! 能力缓存的自动写回）都把这些内容合并回去，并**保持原来的 `schema_version`**——用户再升级回去时，新版本看到的仍是
//! 自己的版本号与字段，不会被当成 v1 重新迁移。本版本认识的字段以本版本的值为准（用户在本版本里做的修改照常生效，
//! 删掉的条目不会被「恢复」）。

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::error::IpcError;

/// 一步迁移：把上一版本的文档变成下一版本
pub type Step = fn(Value) -> Value;

/// settings.json 的迁移表（v1 是第一个版本，暂无迁移）
pub const SETTINGS_STEPS: &[Step] = &[];
/// styles.json 的迁移表
pub const STYLES_STEPS: &[Step] = &[];
/// glossary.json 的迁移表
pub const GLOSSARY_STEPS: &[Step] = &[];
/// state.json 的迁移表
pub const STATE_STEPS: &[Step] = &[];

/// styles.json / glossary.json / state.json 的当前版本
pub const DOC_SCHEMA_VERSION: u32 = 1;

/// 迁移结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// 已是当前版本，未改动
    Current,
    /// 从旧版本迁移到了当前版本（调用方应把结果写回磁盘）
    Migrated { from: u32 },
    /// 文件来自更新的版本：原样保留
    Newer { found: u32 },
}

/// 本版本不认识的内容（见模块说明）
#[derive(Debug, Clone, PartialEq)]
pub enum Unknown {
    /// 整个值（本版本没有这个键）
    Value(Value),
    /// 对象里有不认识的键
    Fields(BTreeMap<String, Unknown>),
    /// 数组：`by_id` 为按 `id` 对应的元素里不认识的内容；`unread` 为本版本读不出（宽松解析时被丢掉）的带 `id` 的元素
    Items {
        by_id: BTreeMap<String, Unknown>,
        unread: Vec<Value>,
    },
}

/// 来自更新版本的文档：原来的 `schema_version` 与本版本不认识的内容，写回时合并回去
#[derive(Debug, Clone, PartialEq)]
pub struct Newer {
    pub found: u32,
    pub unknown: Option<Unknown>,
}

impl Newer {
    /// 比较原文 `raw` 与本版本读取后重新序列化的 `ours`，记下本版本不认识的内容
    pub fn capture(found: u32, raw: &Value, ours: &Value) -> Self {
        Self {
            found,
            unknown: diff(raw, ours),
        }
    }

    /// 写回前：把不认识的内容合并进本版本序列化的结果，并保留原来的 `schema_version`
    pub fn restore(&self, ours: &mut Value) {
        if let Some(unknown) = &self.unknown {
            apply(ours, unknown);
        }
        if let Some(obj) = ours.as_object_mut() {
            obj.insert("schema_version".to_owned(), Value::from(self.found));
        }
    }

    /// 读取后的记录：`outcome` 为 `Newer` 时比较原文与本版本的结果，否则为 None
    pub fn from_outcome(outcome: Outcome, raw: Option<&Value>, ours: &Value) -> Option<Self> {
        match (outcome, raw) {
            (Outcome::Newer { found }, Some(raw)) => Some(Self::capture(found, raw, ours)),
            _ => None,
        }
    }
}

fn item_id(v: &Value) -> Option<&str> {
    v.get("id").and_then(Value::as_str)
}

fn diff(raw: &Value, ours: &Value) -> Option<Unknown> {
    match (raw, ours) {
        (Value::Object(r), Value::Object(o)) => {
            let mut fields = BTreeMap::new();
            for (k, rv) in r {
                if k == "schema_version" {
                    continue;
                }
                match o.get(k) {
                    None => {
                        fields.insert(k.clone(), Unknown::Value(rv.clone()));
                    }
                    Some(ov) => {
                        if let Some(u) = diff(rv, ov) {
                            fields.insert(k.clone(), u);
                        }
                    }
                }
            }
            (!fields.is_empty()).then_some(Unknown::Fields(fields))
        }
        (Value::Array(r), Value::Array(o)) => {
            let mut by_id = BTreeMap::new();
            let mut unread = Vec::new();
            for re in r {
                let Some(id) = item_id(re) else { continue };
                match o.iter().find(|oe| item_id(oe) == Some(id)) {
                    Some(oe) => {
                        if let Some(u) = diff(re, oe) {
                            by_id.insert(id.to_owned(), u);
                        }
                    }
                    None => unread.push(re.clone()),
                }
            }
            (!by_id.is_empty() || !unread.is_empty()).then_some(Unknown::Items { by_id, unread })
        }
        _ => None,
    }
}

fn apply(ours: &mut Value, unknown: &Unknown) {
    match (ours, unknown) {
        (Value::Object(o), Unknown::Fields(fields)) => apply_fields(o, fields),
        (Value::Array(items), Unknown::Items { by_id, unread }) => {
            for item in items.iter_mut() {
                if let Some(u) = item_id(item).and_then(|id| by_id.get(id)) {
                    apply(item, u);
                }
            }
            for extra in unread {
                let id = item_id(extra);
                if !items.iter().any(|i| item_id(i) == id) {
                    items.push(extra.clone());
                }
            }
        }
        _ => {}
    }
}

fn apply_fields(o: &mut Map<String, Value>, fields: &BTreeMap<String, Unknown>) {
    for (k, u) in fields {
        match u {
            Unknown::Value(v) => {
                if !o.contains_key(k) {
                    o.insert(k.clone(), v.clone());
                }
            }
            nested => {
                if let Some(child) = o.get_mut(k) {
                    apply(child, nested);
                }
            }
        }
    }
}

/// 把任意旧版本的 settings JSON 迁移到当前版本（`wisp_types::SETTINGS_SCHEMA_VERSION`）
pub fn migrate(value: Value) -> Result<Value, IpcError> {
    Ok(run(value, wisp_types::SETTINGS_SCHEMA_VERSION, SETTINGS_STEPS).0)
}

/// 通用迁移：`current` 为当前版本，`steps` 为该文档的迁移表（`steps.len() == current - 1`）。
/// 迁移后的文档 `schema_version` 一律写成 `current`（`Newer` 除外）
pub fn run(mut value: Value, current: u32, steps: &[Step]) -> (Value, Outcome) {
    let Some(obj) = value.as_object() else {
        return (value, Outcome::Current);
    };
    let found = obj
        .get("schema_version")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .filter(|v| *v >= 1)
        .unwrap_or(1);
    if found > current {
        log::warn!(
            "config written by a newer version (schema_version={found}, supported={current}); loading leniently"
        );
        return (value, Outcome::Newer { found });
    }
    let mut version = found;
    while version < current {
        let idx = usize::try_from(version - 1).unwrap_or(usize::MAX);
        match steps.get(idx) {
            Some(step) => value = step(value),
            None => {
                // 迁移表缺一步：不猜测，交给宽松反序列化（缺字段取默认值）
                log::warn!("no migration step from schema_version={version}; loading leniently");
                break;
            }
        }
        version += 1;
    }
    if let Some(obj) = value.as_object_mut() {
        obj.insert("schema_version".to_owned(), Value::from(current));
    }
    if found < current {
        log::info!("config migrated schema_version {found} -> {current}");
        (value, Outcome::Migrated { from: found })
    } else {
        (value, Outcome::Current)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn rename_theme(mut v: Value) -> Value {
        if let Some(a) = v.get_mut("appearance").and_then(Value::as_object_mut)
            && let Some(t) = a.remove("color_scheme")
        {
            a.insert("theme".to_owned(), t);
        }
        v
    }

    fn add_flag(mut v: Value) -> Value {
        if let Some(o) = v.as_object_mut() {
            o.insert("flag".to_owned(), Value::Bool(true));
        }
        v
    }

    #[test]
    fn current_version_is_untouched() {
        let v = json!({"schema_version": 1, "a": 1});
        let (out, outcome) = run(v.clone(), 1, &[]);
        assert_eq!(outcome, Outcome::Current);
        assert_eq!(out, v);
    }

    #[test]
    fn missing_or_zero_version_is_treated_as_v1() {
        let (out, outcome) = run(json!({"a": 1}), 1, &[]);
        assert_eq!(outcome, Outcome::Current);
        assert_eq!(out["schema_version"], 1);
        let (_, outcome) = run(json!({"schema_version": 0}), 1, &[]);
        assert_eq!(outcome, Outcome::Current);
        let (_, outcome) = run(json!({"schema_version": "x"}), 1, &[]);
        assert_eq!(outcome, Outcome::Current);
    }

    #[test]
    fn old_versions_are_migrated_step_by_step() {
        let steps: &[Step] = &[rename_theme, add_flag];
        let (out, outcome) = run(
            json!({"schema_version": 1, "appearance": {"color_scheme": "dark"}}),
            3,
            steps,
        );
        assert_eq!(outcome, Outcome::Migrated { from: 1 });
        assert_eq!(
            out,
            json!({"schema_version": 3, "appearance": {"theme": "dark"}, "flag": true})
        );
        // 从中间版本开始只跑剩下的步骤
        let (out, outcome) = run(
            json!({"schema_version": 2, "appearance": {"color_scheme": "x"}}),
            3,
            steps,
        );
        assert_eq!(outcome, Outcome::Migrated { from: 2 });
        assert_eq!(out["appearance"]["color_scheme"], "x");
        assert_eq!(out["flag"], true);
    }

    #[test]
    fn newer_versions_are_kept_as_is() {
        let v = json!({"schema_version": 9, "future": true});
        let (out, outcome) = run(v.clone(), 1, &[]);
        assert_eq!(outcome, Outcome::Newer { found: 9 });
        assert_eq!(out, v);
    }

    /// 降级场景：新版本写的文件被旧版本读取、修改并写回，新版本才有的字段与版本号都保留，旧版本的修改照常生效
    #[test]
    fn newer_documents_keep_unknown_fields_when_written_back() {
        let raw = json!({
            "schema_version": 3,
            "general": {"ui_language": "en-US", "future_switch": true},
            "future_section": {"x": 1},
            "providers": [
                {"id": "a", "name": "A", "future_auth": "oauth"},
                {"id": "b", "name": "B", "protocol": "future-protocol"},
                {"id": "c", "name": "C"}
            ],
            "overrides": {"natural": {"tail": "x", "future": 1}, "bad": {"temperature": "hot"}}
        });
        // 本版本读到的内容：不认识的键没有，读不出的 Provider b 与 override bad 被宽松解析丢掉
        let ours0 = json!({
            "schema_version": 1,
            "general": {"ui_language": "en-US"},
            "providers": [{"id": "a", "name": "A"}, {"id": "c", "name": "C"}],
            "overrides": {"natural": {"tail": "x"}}
        });
        let newer = Newer::capture(3, &raw, &ours0);
        // 用户在本版本里：改了语言、改了 A 的名字、删掉了 C、删掉了 natural 的 override
        let mut ours = json!({
            "schema_version": 1,
            "general": {"ui_language": "zh-CN"},
            "providers": [{"id": "a", "name": "A2"}],
            "overrides": {}
        });
        newer.restore(&mut ours);
        assert_eq!(ours["schema_version"], 3, "保持原来的版本号，升级回去时不会被重新迁移");
        assert_eq!(ours["general"]["ui_language"], "zh-CN", "本版本的修改生效");
        assert_eq!(ours["general"]["future_switch"], true);
        assert_eq!(ours["future_section"], json!({"x": 1}));
        let providers = ours["providers"].as_array().expect("array");
        assert_eq!(providers[0], json!({"id": "a", "name": "A2", "future_auth": "oauth"}));
        assert!(providers.iter().any(|p| p["id"] == "b"), "本版本读不出的条目原样保留");
        assert!(!providers.iter().any(|p| p["id"] == "c"), "用户删掉的不会被恢复");
        assert_eq!(ours["overrides"].get("natural"), None, "用户删掉的 override 不会被恢复");
        assert_eq!(
            ours["overrides"]["bad"],
            json!({"temperature": "hot"}),
            "读不出的 override 保留"
        );
    }

    #[test]
    fn current_documents_have_nothing_to_preserve() {
        let v = json!({"schema_version": 1, "a": 1});
        assert_eq!(Newer::from_outcome(Outcome::Current, Some(&v), &v), None);
        let n = Newer::from_outcome(Outcome::Newer { found: 2 }, Some(&v), &v).expect("newer");
        assert_eq!(n.unknown, None);
        let mut out = json!({"schema_version": 1, "a": 2});
        n.restore(&mut out);
        assert_eq!(out, json!({"schema_version": 2, "a": 2}));
    }

    #[test]
    fn settings_migrate_sets_current_version() {
        let out = migrate(json!({"general": {}})).expect("migrate");
        assert_eq!(out["schema_version"], wisp_types::SETTINGS_SCHEMA_VERSION);
    }
}
