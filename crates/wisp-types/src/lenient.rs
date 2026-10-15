//! 配置文档的宽松反序列化（settings.json / styles.json / glossary.json 允许用户手改）。
//!
//! 规则（`Settings` 及其全部子结构、`Defaults`、`StylesFile`、`GlossaryFile`，以及 `Provider.models`）：
//! - 对象里某个字段的值非法（类型不对、枚举值写错如 `"theme": "Dark"`、整数越界、`null` 写给非可选字段）
//!   → 只有这个字段退回所在结构 `Default` 实现里的值，同一对象的其他字段照常加载；
//! - 数组里的某个条目非法（例如 `providers[2]` 缺 `protocol`）→ 只丢弃这一条；
//! - map 的键无法解析（例如 `hotkeys.bindings` 里拼错或来自将来版本的动作 key）或值非法 → 只丢弃这一项；
//! - 缺字段 → 取默认值；未知字段 → 忽略（向前兼容）；
//! - 只有顶层不是合法 JSON、或顶层不是对象时才返回错误（调用方走「备份 + 默认值」）。
//!
//! 每一处退回 / 丢弃都记一条 [`ConfigIssue`]：只含字段路径与类型信息，**不含值本身**（避免把 headers 里的凭据
//! 之类写进日志）。desktop 的设置服务读文件、热加载与 `settings_patch` 一律用 [`parse_lenient`] / [`from_value_lenient`]：
//! - 读文件 / 热加载：逐条 `log::warn!` 记下问题后照常使用结果（不回写文件，用户改正后自动恢复）；
//! - `settings_patch`（前端发来的值）：问题列表非空即返回 `invalid_argument`，不落盘——前端的非法值是 bug，不能静默改成默认值。
//!
//! 直接 `serde_json::from_str::<Settings>` 也同样宽松，只是问题列表被丢弃。范围校验（例如 `font_size` 12–18）
//! 不在这里，由设置服务的 `validate` 负责。

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

/// 一处被宽松处理的问题：某个字段退回了默认值，或数组 / map 里的某个条目被丢弃
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConfigIssue {
    /// 字段路径，例如 `appearance.theme`、`providers[2]`、`hotkeys.bindings.translate_selction`
    pub path: String,
    /// 原因（只含类型信息，不含值本身），例如 `expected ThemePref, found string (reset to default)`
    pub reason: String,
}

impl fmt::Display for ConfigIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.reason)
    }
}

/// 宽松解析一份配置文档的 JSON 文本。顶层不是合法 JSON 或不是对象时返回 `Err`；否则返回结果与问题列表
pub fn parse_lenient<T: DeserializeOwned>(text: &str) -> Result<(T, Vec<ConfigIssue>), serde_json::Error> {
    let value: Value = serde_json::from_str(text)?;
    from_value_lenient(value)
}

/// 同 [`parse_lenient`]，输入为已解析的 `Value`（例如 JSON Merge Patch 合并之后）
pub fn from_value_lenient<T: DeserializeOwned>(value: Value) -> Result<(T, Vec<ConfigIssue>), serde_json::Error> {
    let guard = CollectGuard::install();
    let result = T::deserialize(value);
    let issues = guard.finish();
    result.map(|v| (v, issues))
}

// ---------------------------------------------------------------------------
// 收集上下文（线程局部；没有安装时宽松行为不变，只是不记录）

#[derive(Default)]
struct Ctx {
    path: Vec<String>,
    issues: Vec<ConfigIssue>,
}

thread_local! {
    static CTX: RefCell<Option<Ctx>> = const { RefCell::new(None) };
}

/// 安装收集上下文；drop 时（包括 panic 展开）恢复之前的上下文，允许嵌套调用
struct CollectGuard {
    prev: Option<Option<Ctx>>,
}

impl CollectGuard {
    fn install() -> Self {
        let prev = CTX.with(|c| c.replace(Some(Ctx::default())));
        Self { prev: Some(prev) }
    }

    fn finish(mut self) -> Vec<ConfigIssue> {
        let prev = self.prev.take().unwrap_or_default();
        CTX.with(|c| c.replace(prev)).map(|ctx| ctx.issues).unwrap_or_default()
    }
}

impl Drop for CollectGuard {
    fn drop(&mut self) {
        if let Some(prev) = self.prev.take() {
            CTX.with(|c| c.replace(prev));
        }
    }
}

/// 在路径上压入一段后执行 `f`（没有收集上下文时不构造路径字符串）
fn with_segment<R>(segment: impl FnOnce() -> String, f: impl FnOnce() -> R) -> R {
    let active = CTX.with(|c| match c.borrow_mut().as_mut() {
        Some(ctx) => {
            ctx.path.push(segment());
            true
        }
        None => false,
    });
    let out = f();
    if active {
        CTX.with(|c| {
            if let Some(ctx) = c.borrow_mut().as_mut() {
                ctx.path.pop();
            }
        });
    }
    out
}

/// 在当前路径上记一条问题
fn record(reason: impl FnOnce() -> String) {
    CTX.with(|c| {
        if let Some(ctx) = c.borrow_mut().as_mut() {
            let mut path = String::new();
            for seg in &ctx.path {
                if !path.is_empty() && !seg.starts_with('[') {
                    path.push('.');
                }
                path.push_str(seg);
            }
            if path.is_empty() {
                path.push('$');
            }
            ctx.issues.push(ConfigIssue { path, reason: reason() });
        }
    });
}

/// JSON 值的种类（问题描述用，不暴露值本身）
fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// 去掉模块路径的类型名：`alloc::vec::Vec<wisp_types::llm::Provider>` → `Vec<Provider>`
fn short_type_name<T: ?Sized>() -> String {
    let full = std::any::type_name::<T>();
    let mut out = String::with_capacity(full.len());
    let mut seg_start = 0;
    let mut chars = full.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ':' && chars.peek() == Some(&':') {
            chars.next();
            out.truncate(seg_start);
        } else {
            out.push(c);
            if !(c.is_alphanumeric() || c == '_') {
                seg_start = out.len();
            }
        }
    }
    out
}

/// map 的键作为路径段（用户手写的 key，截断到 64 个字符）
fn key_segment(key: &str) -> String {
    key.chars().take(64).collect()
}

// ---------------------------------------------------------------------------
// 供 `lenient_struct!` 与 `#[serde(deserialize_with = …)]` 使用的辅助函数

/// 结构体的输入必须是对象；不是对象时报错（由上一层字段退回默认值，顶层则整份失败）
#[doc(hidden)]
pub fn object<'de, D: Deserializer<'de>>(d: D) -> Result<Map<String, Value>, D::Error> {
    Map::deserialize(d)
}

/// 取出 `key` 并反序列化到 `slot`；非法时保留 `slot` 原值（即结构默认值）并记问题
#[doc(hidden)]
pub fn field<T: DeserializeOwned>(map: &mut Map<String, Value>, key: &str, slot: &mut T) {
    field_with(map, key, slot, T::deserialize);
}

/// 同 [`field`]，用自定义的反序列化函数（`vec` / `map` 等）
#[doc(hidden)]
pub fn field_with<T>(
    map: &mut Map<String, Value>,
    key: &str,
    slot: &mut T,
    f: impl FnOnce(Value) -> Result<T, serde_json::Error>,
) {
    let Some(v) = map.remove(key) else { return };
    let found = kind(&v);
    with_segment(
        || key.to_owned(),
        || match f(v) {
            Ok(x) => *slot = x,
            Err(_) => record(|| format!("expected {}, found {found} (reset to default)", short_type_name::<T>())),
        },
    );
}

/// 宽松数组：逐条反序列化，非法条目丢弃并记问题；输入不是数组时报错（由上一层退回默认值）。
/// 可直接用于 `#[serde(default, deserialize_with = "crate::lenient::vec")]`
pub fn vec<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let items = Vec::<Value>::deserialize(d)?;
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.into_iter().enumerate() {
        let found = kind(&item);
        with_segment(
            || format!("[{i}]"),
            || match T::deserialize(item) {
                Ok(x) => out.push(x),
                Err(_) => record(|| format!("expected {}, found {found} (entry dropped)", short_type_name::<T>())),
            },
        );
    }
    Ok(out)
}

/// 给派生 `Deserialize` 的结构体用：`#[serde(deserialize_with = …)]` 拿不到字段名，用它在问题路径上补一段
/// （例如 `Provider.models` → `providers[0].models[1]`）
pub(crate) fn named<'de, D, T>(
    segment: &'static str,
    d: D,
    f: impl FnOnce(D) -> Result<T, D::Error>,
) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
{
    with_segment(|| segment.to_owned(), || f(d))
}

/// 宽松 map：键无法解析（未知枚举值等）或值非法的项丢弃并记问题；输入不是对象时报错（由上一层退回默认值）。
/// 可直接用于 `#[serde(default, deserialize_with = "crate::lenient::map")]`
pub fn map<'de, D, K, V>(d: D) -> Result<BTreeMap<K, V>, D::Error>
where
    D: Deserializer<'de>,
    K: DeserializeOwned + Ord,
    V: DeserializeOwned,
{
    let entries = Map::<String, Value>::deserialize(d)?;
    let mut out = BTreeMap::new();
    for (k, v) in entries {
        let segment = key_segment(&k);
        let found = kind(&v);
        with_segment(
            || segment,
            || {
                let Ok(key) = K::deserialize(Value::String(k)) else {
                    record(|| format!("unknown key for {} (entry dropped)", short_type_name::<K>()));
                    return;
                };
                match V::deserialize(v) {
                    Ok(val) => {
                        out.insert(key, val);
                    }
                    Err(_) => {
                        record(|| format!("expected {}, found {found} (entry dropped)", short_type_name::<V>()));
                    }
                }
            },
        );
    }
    Ok(out)
}

/// 定义一个配置结构体，并为它生成宽松的 `Deserialize`（规则见模块文档）。
///
/// - 结构体必须实现 `Default`（字段非法 / 缺失时的取值来源），自身只派生 `Serialize`，**不要**再派生 `Deserialize`；
/// - 字段默认用 [`field`]；写成 `pub x: Vec<T> => crate::lenient::vec` 时改用指定函数（数组 / map 的逐条容错）；
/// - 字段由宏统一展开，增删字段不会与反序列化逻辑脱节。
macro_rules! lenient_struct {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident {
            $(
                $(#[$fmeta:meta])*
                $fvis:vis $field:ident : $fty:ty $(=> $with:path)?
            ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        $vis struct $name {
            $(
                $(#[$fmeta])*
                $fvis $field: $fty,
            )*
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(d: D) -> ::core::result::Result<Self, D::Error> {
                #[allow(unused_mut)]
                let mut map = $crate::lenient::object(d)?;
                #[allow(unused_mut)]
                let mut out = <Self as ::core::default::Default>::default();
                $(
                    $crate::lenient::lenient_struct!(@field map, out, $field $(, $with)?);
                )*
                ::core::result::Result::Ok(out)
            }
        }
    };
    (@field $map:ident, $out:ident, $field:ident) => {
        $crate::lenient::field(&mut $map, stringify!($field), &mut $out.$field)
    };
    (@field $map:ident, $out:ident, $field:ident, $with:path) => {
        $crate::lenient::field_with(&mut $map, stringify!($field), &mut $out.$field, $with)
    };
}
pub(crate) use lenient_struct;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_type_names() {
        assert_eq!(short_type_name::<Vec<String>>(), "Vec<String>");
        assert_eq!(
            short_type_name::<BTreeMap<String, crate::style::StyleOverride>>(),
            "BTreeMap<String, StyleOverride>"
        );
        assert_eq!(short_type_name::<u32>(), "u32");
    }

    #[test]
    fn non_object_top_level_is_an_error() {
        assert!(parse_lenient::<crate::settings::Settings>("[]").is_err());
        assert!(parse_lenient::<crate::settings::Settings>("{ not json").is_err());
        assert!(parse_lenient::<crate::settings::Settings>("null").is_err());
    }

    #[test]
    fn guard_restores_outer_context() {
        let outer = CollectGuard::install();
        let (_, inner) = parse_lenient::<crate::style::GlossaryFile>(r#"{"entries": [1]}"#).expect("inner");
        assert_eq!(inner.len(), 1);
        record(|| "outer".to_owned());
        let issues = outer.finish();
        assert_eq!(issues.len(), 1, "内层的问题不泄漏到外层");
        assert_eq!(issues[0].reason, "outer");
        // 没有上下文时不记录
        record(|| "dropped".to_owned());
        assert!(CTX.with(|c| c.borrow().is_none()));
    }
}
