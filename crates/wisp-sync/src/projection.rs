//! 配置文档的投影与反投影。
//!
//! 同步的单位是「寄存器」（一个 key 一个值，逐 key 做 LWW），而配置文件是嵌套的 JSON 文档。
//! 投影把文档拆成 `key → 值` 的映射，反投影把一组寄存器改动写回本机当前的文档。输入一律是**字符串往返**
//! 得到的 `Value`（[`canonical`]），避免 f32 拓宽成 0.6000000238418579 这类假差异。
//!
//! ## key 的写法
//!
//! - 普通 key 形如 `/<段>/<段>…`；按系统区分的 key 形如 `@<os>/<段>/<段>…`，`<os>` 为 `macos` / `windows` / `linux`
//!   （[`os_tag`]），只与同系统的设备共享。
//! - 每一段按 RFC 6901 转义（`~` → `~0`、`/` → `~1`）；以 `@` 开头的段是保留名（`@order`），
//!   所以 id 开头的 `@` 另写成 `~2`（[`escape_segment`]），id 永远不会与保留名相撞。
//! - 术语表的 key 就是规范化后的 `source` 本身（没有其他种类的 key，不需要转义）。
//!
//! ## settings.json
//!
//! - 每个顶层分组的每个字段是一个 key：`/<group>/<field>`；更深的值整体作为一个寄存器
//!   （`Option<结构体>` 因此天然是原子的，不会拼出半新半旧的模型引用）。
//! - 只属于本机的字段不投影，反投影时保留本机值：`schema_version`、开机启动、开发者模式、`advanced`、`sync`、
//!   剪贴板记录开关、疑似密钥检测、识别历史开关。
//! - 按系统区分的字段：快捷键绑定、双击修饰键、复制等待、窗口材质（`@<os>/hotkeys/bindings` 等）。
//! - `providers`：每个 provider 一个整体寄存器 `/providers/<id>`，删除即墓碑；顺序是另一个寄存器
//!   `/providers/@order`（id 数组）。provider 的值去掉本机学习到的模型能力缓存（`caps`、`missing_upstream`、
//!   `deprecated`，以及不是用户指定的 `vision` / `vision_source`），反投影时按 (provider id, model id) 从本机值注回。
//! - `clipboard.excluded_apps` 与 `hotkeys.app_keymaps`：按 `<os>:<app_id>` 区分元素的无序集合
//!   （`/clipboard/excluded_apps/<os>:<app_id>`），每个元素一个整体寄存器。
//!
//! ## styles.json
//!
//! `/order` 整体；`/overrides/<builtin_id>`（空覆盖 = 墓碑）；`/custom/<id>`（删除即墓碑）。
//! 风格的快捷键是按系统的，从元素中剥离为 `@<os>/style_hotkey/<style_id>`；`builtin` / `modified` 是计算字段，不投影。
//!
//! ## glossary.json
//!
//! 每条术语 `<source>` → `{target, note}`；顺序不同步（本机已有的保持原位，新的追加在后）。
//!
//! ## 本版本不认识的内容
//!
//! 投影只产出本版本认识的 key：未知字段不出现，也就不会被同步删除，本机文件里的未知字段在反投影时原样保留。
//! 收到不认识的 key（或值的形状不对）时不写入，结果报 [`KeyApply::Deferred`]，升级后再应用。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::OnceLock;

use serde::Serialize;
use serde_json::{Map, Value};
use wisp_types::defaults::BUILTIN_STYLE_IDS;
use wisp_types::{OsKind, Settings, StylesFile};

use crate::engine::KeyApply;
use crate::model::Domain;

/// 参与同步的配置文档
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ConfigDoc {
    Settings,
    Styles,
    Glossary,
}

impl ConfigDoc {
    pub const ALL: [ConfigDoc; 3] = [ConfigDoc::Settings, ConfigDoc::Styles, ConfigDoc::Glossary];

    /// 文档对应的同步域
    pub fn domain(self) -> Domain {
        match self {
            Self::Settings => Domain::Cfg,
            Self::Styles => Domain::Sty,
            Self::Glossary => Domain::Glo,
        }
    }

    pub fn from_domain(d: &Domain) -> Option<ConfigDoc> {
        match d {
            Domain::Cfg => Some(Self::Settings),
            Domain::Sty => Some(Self::Styles),
            Domain::Glo => Some(Self::Glossary),
            _ => None,
        }
    }

    /// 稳定的短名（sync.db 里保存影子时使用）
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Settings => "settings",
            Self::Styles => "styles",
            Self::Glossary => "glossary",
        }
    }
}

/// 有序集合的顺序寄存器（id 数组）在 key 里的保留名
pub const ORDER: &str = "@order";

/// 按系统区分的 key 的系统标签（与 `OsKind` 的 serde 取值一致）
pub const fn os_tag(os: OsKind) -> &'static str {
    match os {
        OsKind::Macos => "macos",
        OsKind::Windows => "windows",
        OsKind::Linux => "linux",
    }
}

fn parse_os_tag(tag: &str) -> Option<OsKind> {
    match tag {
        "macos" => Some(OsKind::Macos),
        "windows" => Some(OsKind::Windows),
        "linux" => Some(OsKind::Linux),
        _ => None,
    }
}

/// 强类型配置 → 规范 `Value`：经 JSON 文本往返，f32 以最短十进制形式出现（0.6 而不是 0.6000000238418579），
/// 与从记录里解码出来的值逐位一致
pub fn canonical<T: Serialize + ?Sized>(value: &T) -> Result<Value, serde_json::Error> {
    serde_json::from_slice(&serde_json::to_vec(value)?)
}

// ---------------------------------------------------------------------------------------------
// 段的转义
// ---------------------------------------------------------------------------------------------

/// 把一个 id（或字段名）编码成 key 的一段：RFC 6901 转义（`~` → `~0`、`/` → `~1`），开头的 `@` 写成 `~2`
/// （以 `@` 开头的段留给保留名）。编码一一对应：每个字符串只有一种写法
pub fn escape_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for (i, c) in s.chars().enumerate() {
        match c {
            '~' => out.push_str("~0"),
            '/' => out.push_str("~1"),
            '@' if i == 0 => out.push_str("~2"),
            _ => out.push(c),
        }
    }
    out
}

/// [`escape_segment`] 的逆运算。空段、以 `@` 开头的段（保留名）与含非法转义的段返回 `None`
pub fn unescape_segment(s: &str) -> Option<String> {
    if s.is_empty() || s.starts_with('@') {
        return None;
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '~' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            '0' => out.push('~'),
            '1' => out.push('/'),
            // 只有段首的 `@` 才这样写，别处出现说明不是本编码产生的，拒绝以保持一一对应
            '2' if out.is_empty() => out.push('@'),
            _ => return None,
        }
    }
    Some(out)
}

/// `/a/b…`（每段转义）
fn path_key<'a>(segments: impl IntoIterator<Item = &'a str>) -> String {
    let mut key = String::new();
    for s in segments {
        key.push('/');
        key.push_str(&escape_segment(s));
    }
    key
}

/// `@<os>/a/b…`
fn os_key<'a>(os: OsKind, segments: impl IntoIterator<Item = &'a str>) -> String {
    format!("@{}{}", os_tag(os), path_key(segments))
}

/// key 的一段
#[derive(Debug, PartialEq, Eq)]
enum Seg<'a> {
    /// 以 `@` 开头的保留名，原样
    Reserved(&'a str),
    /// 解码后的名字或 id
    Name(String),
}

/// 拆开 key：`/a/b` → (None, [a, b])；`@os/a/b` → (Some(os), [a, b])。形状不对（含非法转义）返回 `None`
fn split_key(key: &str) -> Option<(Option<&str>, Vec<Seg<'_>>)> {
    let (tag, path) = match key.strip_prefix('@') {
        Some(rest) => {
            let (tag, path) = rest.split_once('/')?;
            if tag.is_empty() {
                return None;
            }
            (Some(tag), path)
        }
        None => (None, key.strip_prefix('/')?),
    };
    let segs = path
        .split('/')
        .map(|raw| {
            if raw.starts_with('@') {
                Some(Seg::Reserved(raw))
            } else {
                unescape_segment(raw).map(Seg::Name)
            }
        })
        .collect::<Option<Vec<_>>>()?;
    Some((tag, segs))
}

// ---------------------------------------------------------------------------------------------
// 公共入口
// ---------------------------------------------------------------------------------------------

/// 把文档投影为 `key → 值`。只产出本系统（`os`）的按系统 key；只属于本机的字段不出现。
/// 文档不是预期的形状时尽力投影能识别的部分（未识别的部分不出现，也就不会被同步删除）
pub fn project(doc: ConfigDoc, value: &Value, os: OsKind) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    if let Some(root) = value.as_object() {
        match doc {
            ConfigDoc::Settings => project_settings(root, os, &mut out),
            ConfigDoc::Styles => project_styles(root, os, &mut out),
            ConfigDoc::Glossary => project_glossary(root, &mut out),
        }
    }
    out
}

/// [`unproject_detailed`] 的结果
#[derive(Debug, Clone, PartialEq)]
pub struct Unprojected {
    /// 新文档（还要经过桌面端的迁移、宽容解析与校验才写入文件）
    pub value: Value,
    /// 与传入的改动一一对应：
    /// - `Applied`：已写入，或与这台设备无关（其他系统的 key、只属于本机的字段），以后也不需要再应用；
    /// - `Deferred`：本版本不认识的 key、值的形状不对，或引用的风格本机还没有（例如快捷键先于自定义风格到达）。
    ///   这些改动没有写入，保持「未应用」，以后（或升级后）重试
    pub results: Vec<KeyApply>,
}

/// 把一组寄存器改动应用到本机文档 `base` 上，返回新文档（`base` 不变）：
/// - `Some(值)` 设置该 key，`None`（墓碑）删除集合元素 / 删除字段（之后的宽容解析把它恢复为缺省值）；
/// - 其他系统的 `@<os>` key、只属于本机的字段、无法识别的 key 被忽略；
/// - 有序集合按 `@order` 排列存在的元素，存在但不在 order 里的按 id 排序追加在后；
///   没有 `@order` 改动时已有元素保持原位，新元素按 id 排序追加在后；
/// - provider 的学习缓存从 `base` 中按 (provider id, model id) 注回；风格的快捷键与计算字段从 `base` 中保留。
///
/// 返回值还要经过桌面端的迁移、宽容解析与校验才写入文件。需要逐项结果时用 [`unproject_detailed`]
pub fn unproject(doc: ConfigDoc, base: &Value, changes: &[(String, Option<Value>)], os: OsKind) -> Value {
    unproject_detailed(doc, base, changes, os).value
}

/// 同 [`unproject`]，另外报告每项改动是否已应用（见 [`Unprojected::results`]）
pub fn unproject_detailed(
    doc: ConfigDoc,
    base: &Value,
    changes: &[(String, Option<Value>)],
    os: OsKind,
) -> Unprojected {
    let Some(root) = base.as_object() else {
        return Unprojected {
            value: base.clone(),
            results: vec![KeyApply::Deferred; changes.len()],
        };
    };
    let (value, results) = match doc {
        ConfigDoc::Settings => unproject_settings(root, changes, os),
        ConfigDoc::Styles => unproject_styles(root, changes, os),
        ConfigDoc::Glossary => unproject_glossary(root, changes),
    };
    Unprojected {
        value: Value::Object(value),
        results,
    }
}

/// 两份投影之间的差异：新增或变化的 key 为 `Some(新值)`，消失的 key 为 `None`（墓碑），按 key 排序
pub fn diff(before: &BTreeMap<String, Value>, after: &BTreeMap<String, Value>) -> Vec<(String, Option<Value>)> {
    let mut out = Vec::new();
    let mut b = before.iter().peekable();
    let mut a = after.iter().peekable();
    loop {
        let step = match (b.peek(), a.peek()) {
            (None, None) => break,
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some((bk, _)), Some((ak, _))) => bk.cmp(ak),
        };
        match step {
            std::cmp::Ordering::Less => {
                if let Some((k, _)) = b.next() {
                    out.push((k.clone(), None));
                }
            }
            std::cmp::Ordering::Greater => {
                if let Some((k, v)) = a.next() {
                    out.push((k.clone(), Some(v.clone())));
                }
            }
            std::cmp::Ordering::Equal => {
                if let (Some((_, bv)), Some((k, av))) = (b.next(), a.next())
                    && bv != av
                {
                    out.push((k.clone(), Some(av.clone())));
                }
            }
        }
    }
    out
}

/// `key` 的值是否等于内置默认值（加入时据此决定以底值 `Hlc(0)` 还是 `Hlc(1)` 发布）。
/// 默认值取自 `Settings::for_os` / `StylesFile::default()` / 空术语表的投影；按系统的 key 用 key 自己的系统的默认值。
/// 集合元素、术语与本版本无法识别的 key 一律返回 `false`
pub fn is_default(doc: ConfigDoc, key: &str, value: &Value, os: OsKind) -> bool {
    if is_element_key(doc, key) {
        return false;
    }
    let os = match key.strip_prefix('@') {
        Some(rest) => match rest.split_once('/').and_then(|(tag, _)| parse_os_tag(tag)) {
            Some(os) => os,
            None => return false,
        },
        None => os,
    };
    default_projection(doc, os).get(key) == Some(value)
}

/// `key` 是否是集合元素（provider、自定义风格、风格覆盖、排除应用、按键映射、术语）。
/// 加入时集合元素与标量的规则不同：元素以服务器存活值为准、服务器没有就发布本机元素；
/// 标量按默认值与底值比较
pub fn is_element_key(doc: ConfigDoc, key: &str) -> bool {
    match doc {
        ConfigDoc::Settings => matches!(
            parse_settings_key(key),
            SettingsKey::Provider(_) | SettingsKey::OsApp { .. }
        ),
        ConfigDoc::Styles => matches!(parse_styles_key(key), StylesKey::Override(_) | StylesKey::Custom(_)),
        ConfigDoc::Glossary => !key.is_empty(),
    }
}

/// 内置默认配置的投影（按需计算一次后缓存；只在加入与创建时用到）
fn default_projection(doc: ConfigDoc, os: OsKind) -> &'static BTreeMap<String, Value> {
    static SETTINGS: [OnceLock<BTreeMap<String, Value>>; 3] = [const { OnceLock::new() }; 3];
    static STYLES: OnceLock<BTreeMap<String, Value>> = OnceLock::new();
    static EMPTY: BTreeMap<String, Value> = BTreeMap::new();
    let build = |value: Result<Value, serde_json::Error>| project(doc, &value.unwrap_or(Value::Null), os);
    match doc {
        ConfigDoc::Settings => {
            let slot = match os {
                OsKind::Macos => &SETTINGS[0],
                OsKind::Windows => &SETTINGS[1],
                OsKind::Linux => &SETTINGS[2],
            };
            slot.get_or_init(|| build(canonical(&Settings::for_os(os))))
        }
        // 默认风格文件没有快捷键，投影与系统无关
        ConfigDoc::Styles => STYLES.get_or_init(|| build(canonical(&StylesFile::default()))),
        ConfigDoc::Glossary => &EMPTY,
    }
}

// ---------------------------------------------------------------------------------------------
// 按 id 区分元素的数组
// ---------------------------------------------------------------------------------------------

/// 元素的 id：`field` 是非空字符串时取它
fn string_field(obj: &Map<String, Value>, field: &str) -> Option<String> {
    obj.get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn id_of(v: &Value) -> Option<String> {
    v.as_object().and_then(|o| string_field(o, "id"))
}

fn source_of(v: &Value) -> Option<String> {
    v.as_object().and_then(|o| string_field(o, GLOSSARY_SOURCE))
}

/// 排除应用 / 按键映射的元素 id：`<os>:<app_id>`（os 不含冒号，按第一个冒号拆开）
fn os_app_of(v: &Value) -> Option<String> {
    let obj = v.as_object()?;
    let os = string_field(obj, "os")?;
    let app_id = string_field(obj, "app_id")?;
    (!os.contains(':')).then(|| format!("{os}:{app_id}"))
}

/// 数组里第一个 id 为 `id` 的对象
fn find_by<'a>(items: Option<&'a Value>, id: &str, of: fn(&Value) -> Option<String>) -> Option<&'a Map<String, Value>> {
    items?
        .as_array()?
        .iter()
        .find(|v| of(v).as_deref() == Some(id))
        .and_then(Value::as_object)
}

/// 反投影期间按 id 区分元素的数组的工作副本：已有元素原地替换，新元素按 id 排序追加在后。
/// 同一 id 出现多次（只可能来自手改的文件）时投影只认第一个，所以替换时保留第一个的位置、去掉其余的，删除时全部去掉
struct Keyed {
    slots: Vec<Option<Value>>,
    index: HashMap<String, Vec<usize>>,
    added: BTreeMap<String, Value>,
    /// 原文档里有这个数组（没有且没有新元素时不凭空添加）
    present: bool,
    touched: bool,
}

impl Keyed {
    /// 从文档的 `field` 取出数组；字段不存在时为空集合，存在但不是数组时返回 `None`（这类改动推迟）
    fn take(doc: &mut Map<String, Value>, field: &str, of: fn(&Value) -> Option<String>) -> Option<Keyed> {
        let (items, present) = match doc.get_mut(field) {
            None => (Vec::new(), false),
            Some(Value::Array(items)) => (std::mem::take(items), true),
            Some(_) => return None,
        };
        let mut index: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, v) in items.iter().enumerate() {
            if let Some(id) = of(v) {
                index.entry(id).or_default().push(i);
            }
        }
        Some(Keyed {
            slots: items.into_iter().map(Some).collect(),
            index,
            added: BTreeMap::new(),
            present,
            touched: false,
        })
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut Value> {
        self.touched = true;
        match self.index.get(id).and_then(|p| p.first()) {
            Some(&i) => self.slots[i].as_mut(),
            None => self.added.get_mut(id),
        }
    }

    fn set(&mut self, id: &str, value: Value) {
        self.touched = true;
        match self.index.get_mut(id) {
            Some(positions) => {
                for &i in &positions[1..] {
                    self.slots[i] = None;
                }
                positions.truncate(1);
                self.slots[positions[0]] = Some(value);
            }
            None => {
                self.added.insert(id.to_owned(), value);
            }
        }
    }

    fn remove(&mut self, id: &str) {
        self.touched = true;
        for i in self.index.remove(id).unwrap_or_default() {
            self.slots[i] = None;
        }
        self.added.remove(id);
    }

    /// 写回文档的 `field`（`take` 时留下的空数组原地替换，字段位置不变）
    fn put(self, doc: &mut Map<String, Value>, field: &str, reorder: Option<&[String]>) {
        if !(self.present || self.touched) {
            return;
        }
        let mut items: Vec<Value> = self
            .slots
            .into_iter()
            .flatten()
            .chain(self.added.into_values())
            .collect();
        if let Some(order) = reorder {
            apply_order(&mut items, order);
        }
        if self.present || !items.is_empty() {
            doc.insert(field.to_owned(), Value::Array(items));
        }
    }
}

/// 按 `order` 排列存在的元素；不在 order 里的按 id 排序追加在后（稳定排序，同 id 的保持相对顺序）
fn apply_order(items: &mut [Value], order: &[String]) {
    let mut rank: HashMap<&str, usize> = HashMap::with_capacity(order.len());
    for (i, id) in order.iter().enumerate() {
        rank.entry(id.as_str()).or_insert(i);
    }
    items.sort_by_cached_key(|v| {
        let id = id_of(v);
        match id.as_deref().and_then(|id| rank.get(id)) {
            Some(&r) => (0, r, String::new()),
            None => (1, 0, id.unwrap_or_default()),
        }
    });
}

/// 顺序寄存器的值：字符串数组（非字符串项忽略）；不是数组时返回 `None`
fn order_ids(value: &Value) -> Option<Vec<String>> {
    Some(
        value
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
    )
}

// ---------------------------------------------------------------------------------------------
// settings.json
// ---------------------------------------------------------------------------------------------

const PROVIDERS: &str = "providers";
const SCHEMA_VERSION: &str = "schema_version";

/// 整组只属于本机的分组
const LOCAL_GROUPS: [&str; 2] = ["advanced", "sync"];

/// 只属于本机的字段：开机启动会安装系统自启项；开发者模式、剪贴板记录开关、疑似密钥检测、识别历史开关
/// 是每台设备自己的选择（例如工作电脑不记录剪贴板）
const LOCAL_FIELDS: [(&str, &str); 5] = [
    ("general", "launch_at_login"),
    ("general", "developer_mode"),
    ("clipboard", "enabled"),
    ("clipboard", "secret_detection"),
    ("ocr", "history"),
];

/// 按系统区分的字段：修饰键在各系统上是不同的物理键、默认表也不同；复制等待按系统调校；材质只在部分系统上有意义
const PER_OS_FIELDS: [(&str, &str); 4] = [
    ("hotkeys", "bindings"),
    ("hotkeys", "double_tap"),
    ("hotkeys", "copy_wait_ms"),
    ("appearance", "material"),
];

/// 按 `<os>:<app_id>` 区分元素的无序集合。两台设备各自添加的条目都要保留（排除名单丢一条就会记录密码管理器的内容）
const OS_APP_SETS: [(&str, &str); 2] = [("clipboard", "excluded_apps"), ("hotkeys", "app_keymaps")];

/// 模型条目里本机学习到的能力缓存：可以重新学习或重新拉取，不应与用户的编辑竞争
const LEARNED_MODEL_FIELDS: [&str; 3] = ["caps", "missing_upstream", "deprecated"];
const VISION_FIELDS: [&str; 2] = ["vision", "vision_source"];
const VISION_SOURCE_USER: &str = "user";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldClass {
    Global,
    PerOs,
    Local,
    OsAppSet,
    Unknown,
}

/// 本版本认识的 settings 分组与字段（取自默认配置的序列化结果，新增字段自动包含在内）
fn settings_schema() -> &'static BTreeMap<String, BTreeSet<String>> {
    static SCHEMA: OnceLock<BTreeMap<String, BTreeSet<String>>> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        let mut groups = BTreeMap::new();
        if let Ok(Value::Object(root)) = canonical(&Settings::default()) {
            for (group, value) in root {
                if let Value::Object(fields) = value {
                    groups.insert(group, fields.into_iter().map(|(k, _)| k).collect());
                }
            }
        }
        groups
    })
}

fn classify_field(group: &str, field: &str) -> FieldClass {
    if LOCAL_GROUPS.contains(&group) || LOCAL_FIELDS.contains(&(group, field)) {
        FieldClass::Local
    } else if PER_OS_FIELDS.contains(&(group, field)) {
        FieldClass::PerOs
    } else if OS_APP_SETS.contains(&(group, field)) {
        FieldClass::OsAppSet
    } else if settings_schema().get(group).is_some_and(|f| f.contains(field)) {
        FieldClass::Global
    } else {
        FieldClass::Unknown
    }
}

fn provider_order_key() -> String {
    format!("{}/{ORDER}", path_key([PROVIDERS]))
}

fn has_user_vision(model: &Map<String, Value>) -> bool {
    model.get("vision_source").and_then(Value::as_str) == Some(VISION_SOURCE_USER)
}

fn strip_model(model: &mut Map<String, Value>) {
    for f in LEARNED_MODEL_FIELDS {
        model.remove(f);
    }
    if !has_user_vision(model) {
        for f in VISION_FIELDS {
            model.remove(f);
        }
    }
}

/// provider 去掉学习缓存后的值
fn strip_provider(provider: &Map<String, Value>) -> Map<String, Value> {
    let mut p = provider.clone();
    if let Some(Value::Array(models)) = p.get_mut("models") {
        for m in models.iter_mut().filter_map(Value::as_object_mut) {
            strip_model(m);
        }
    }
    p
}

/// 收到的 provider 值 → 写进本机文档的元素：去掉值里可能带的学习缓存，再按 model id 从本机原来的元素注回
fn provider_element(id: &str, value: &Map<String, Value>, local: Option<&Map<String, Value>>) -> Value {
    let mut p = strip_provider(value);
    p.insert("id".to_owned(), Value::String(id.to_owned()));
    let local_models = local.and_then(|l| l.get("models"));
    if let Some(Value::Array(models)) = p.get_mut("models") {
        for m in models.iter_mut().filter_map(Value::as_object_mut) {
            let Some(lm) = m
                .get("id")
                .and_then(Value::as_str)
                .and_then(|mid| find_by(local_models, mid, id_of))
            else {
                continue;
            };
            for f in LEARNED_MODEL_FIELDS {
                if let Some(v) = lm.get(f) {
                    m.insert(f.to_owned(), v.clone());
                }
            }
            if has_user_vision(m) {
                continue;
            }
            if has_user_vision(lm) {
                // 用户指定的视觉能力在别的设备上被撤销：回到未知，由本机重新学习
                m.insert("vision".to_owned(), Value::String("unknown".to_owned()));
                m.insert("vision_source".to_owned(), Value::Null);
            } else {
                for f in VISION_FIELDS {
                    if let Some(v) = lm.get(f) {
                        m.insert(f.to_owned(), v.clone());
                    }
                }
            }
        }
    }
    Value::Object(p)
}

fn project_settings(root: &Map<String, Value>, os: OsKind, out: &mut BTreeMap<String, Value>) {
    for (group, value) in root {
        let Some(fields) = value.as_object() else { continue };
        for (field, v) in fields {
            match classify_field(group, field) {
                FieldClass::Global => {
                    out.insert(path_key([group.as_str(), field.as_str()]), v.clone());
                }
                FieldClass::PerOs => {
                    out.insert(os_key(os, [group.as_str(), field.as_str()]), v.clone());
                }
                FieldClass::OsAppSet => {
                    for item in v.as_array().into_iter().flatten() {
                        if let Some(id) = os_app_of(item) {
                            // 重复的元素只认第一个
                            out.entry(path_key([group.as_str(), field.as_str(), id.as_str()]))
                                .or_insert_with(|| item.clone());
                        }
                    }
                }
                FieldClass::Local | FieldClass::Unknown => {}
            }
        }
    }
    if let Some(items) = root.get(PROVIDERS).and_then(Value::as_array) {
        let mut order = Vec::new();
        for item in items {
            let (Some(id), Some(obj)) = (id_of(item), item.as_object()) else {
                continue;
            };
            let key = path_key([PROVIDERS, id.as_str()]);
            if out.contains_key(&key) {
                continue;
            }
            out.insert(key, Value::Object(strip_provider(obj)));
            order.push(Value::String(id));
        }
        out.insert(provider_order_key(), Value::Array(order));
    }
}

#[derive(Debug, PartialEq, Eq)]
enum SettingsKey {
    /// `/<group>/<field>`（全局字段）
    Field {
        group: String,
        field: String,
    },
    /// `@<os>/…`：`field` 只在形状是本版本认识的按系统字段时为 `Some`
    PerOs {
        os: String,
        field: Option<(String, String)>,
    },
    /// `/providers/<id>`
    Provider(String),
    /// `/providers/@order`
    ProviderOrder,
    /// `/<group>/<field>/<os>:<app_id>`，`set` 是 [`OS_APP_SETS`] 的下标
    OsApp {
        set: usize,
        os: String,
        app_id: String,
    },
    /// 只属于本机的字段
    Local,
    Unknown,
}

fn parse_settings_key(key: &str) -> SettingsKey {
    let Some((tag, segs)) = split_key(key) else {
        return SettingsKey::Unknown;
    };
    if let Some(tag) = tag {
        let field = match segs.as_slice() {
            [Seg::Name(g), Seg::Name(f)] if PER_OS_FIELDS.contains(&(g.as_str(), f.as_str())) => {
                Some((g.clone(), f.clone()))
            }
            _ => None,
        };
        return SettingsKey::PerOs {
            os: tag.to_owned(),
            field,
        };
    }
    match segs.as_slice() {
        [Seg::Name(n)] if n == SCHEMA_VERSION => SettingsKey::Local,
        [Seg::Name(p), Seg::Reserved(ORDER)] if p == PROVIDERS => SettingsKey::ProviderOrder,
        [Seg::Name(p), Seg::Name(id)] if p == PROVIDERS => SettingsKey::Provider(id.clone()),
        [Seg::Name(g), Seg::Name(f)] => match classify_field(g, f) {
            FieldClass::Global => SettingsKey::Field {
                group: g.clone(),
                field: f.clone(),
            },
            FieldClass::Local => SettingsKey::Local,
            FieldClass::PerOs | FieldClass::OsAppSet | FieldClass::Unknown => SettingsKey::Unknown,
        },
        [Seg::Name(g), Seg::Name(f), Seg::Name(id)] => {
            let set = OS_APP_SETS.iter().position(|&(sg, sf)| sg == g && sf == f);
            match (set, id.split_once(':')) {
                (Some(set), Some((os, app_id))) if !os.is_empty() && !app_id.is_empty() => SettingsKey::OsApp {
                    set,
                    os: os.to_owned(),
                    app_id: app_id.to_owned(),
                },
                _ => SettingsKey::Unknown,
            }
        }
        _ => SettingsKey::Unknown,
    }
}

/// 设置 / 删除一个分组里的字段。分组存在但不是对象时推迟
fn set_field(doc: &mut Map<String, Value>, group: &str, field: &str, value: Option<&Value>) -> KeyApply {
    let Some(value) = value else {
        return match doc.get_mut(group) {
            None => KeyApply::Applied,
            Some(Value::Object(fields)) => {
                fields.remove(field);
                KeyApply::Applied
            }
            Some(_) => KeyApply::Deferred,
        };
    };
    match doc.entry(group.to_owned()).or_insert_with(|| Value::Object(Map::new())) {
        Value::Object(fields) => {
            fields.insert(field.to_owned(), value.clone());
            KeyApply::Applied
        }
        _ => KeyApply::Deferred,
    }
}

/// 取出分组里的集合字段（分组不存在时视为空集合）
fn take_group_set(doc: &mut Map<String, Value>, group: &str, field: &str) -> Option<Keyed> {
    match doc.get_mut(group) {
        None => Keyed::take(&mut Map::new(), field, os_app_of),
        Some(Value::Object(fields)) => Keyed::take(fields, field, os_app_of),
        Some(_) => None,
    }
}

fn put_group_set(doc: &mut Map<String, Value>, group: &str, field: &str, set: Keyed) {
    if !(set.present || set.touched) {
        return;
    }
    if let Value::Object(fields) = doc.entry(group.to_owned()).or_insert_with(|| Value::Object(Map::new())) {
        set.put(fields, field, None);
    }
}

fn unproject_settings(
    base: &Map<String, Value>,
    changes: &[(String, Option<Value>)],
    os: OsKind,
) -> (Map<String, Value>, Vec<KeyApply>) {
    let mut doc = base.clone();
    let mut results = vec![KeyApply::Deferred; changes.len()];
    let local_tag = os_tag(os);
    let mut providers = Keyed::take(&mut doc, PROVIDERS, id_of);
    let mut sets: Vec<Option<Keyed>> = OS_APP_SETS
        .iter()
        .map(|&(g, f)| take_group_set(&mut doc, g, f))
        .collect();
    // 顺序最后处理：先让本批的新增 / 删除落定
    let mut order: Option<Vec<String>> = None;
    let mut order_changes = Vec::new();

    for (i, (key, value)) in changes.iter().enumerate() {
        let value = value.as_ref();
        results[i] = match parse_settings_key(key) {
            SettingsKey::Field { group, field } => set_field(&mut doc, &group, &field, value),
            SettingsKey::PerOs { os, .. } if os != local_tag => KeyApply::Applied,
            SettingsKey::PerOs {
                field: Some((group, field)),
                ..
            } => set_field(&mut doc, &group, &field, value),
            SettingsKey::Provider(id) => match (providers.as_mut(), value) {
                (Some(list), None) => {
                    list.remove(&id);
                    KeyApply::Applied
                }
                (Some(list), Some(Value::Object(v))) => {
                    let local = find_by(base.get(PROVIDERS), &id, id_of);
                    list.set(&id, provider_element(&id, v, local));
                    KeyApply::Applied
                }
                _ => KeyApply::Deferred,
            },
            SettingsKey::ProviderOrder => {
                order_changes.push(i);
                continue;
            }
            SettingsKey::OsApp { set, os, app_id } => match (sets[set].as_mut(), value) {
                (Some(list), None) => {
                    list.remove(&format!("{os}:{app_id}"));
                    KeyApply::Applied
                }
                (Some(list), Some(Value::Object(v))) => {
                    let mut item = v.clone();
                    item.insert("os".to_owned(), Value::String(os.clone()));
                    item.insert("app_id".to_owned(), Value::String(app_id.clone()));
                    list.set(&format!("{os}:{app_id}"), Value::Object(item));
                    KeyApply::Applied
                }
                _ => KeyApply::Deferred,
            },
            SettingsKey::Local => KeyApply::Applied,
            SettingsKey::PerOs { field: None, .. } | SettingsKey::Unknown => KeyApply::Deferred,
        };
    }

    for i in order_changes {
        results[i] = match (&providers, &changes[i].1) {
            (Some(_), None) => KeyApply::Applied,
            (Some(_), Some(v)) => match order_ids(v) {
                Some(ids) => {
                    order = Some(ids);
                    KeyApply::Applied
                }
                None => KeyApply::Deferred,
            },
            (None, _) => KeyApply::Deferred,
        };
    }

    if let Some(mut list) = providers {
        list.touched |= order.is_some();
        list.put(&mut doc, PROVIDERS, order.as_deref());
    }
    for (set, &(group, field)) in sets.into_iter().zip(OS_APP_SETS.iter()) {
        if let Some(set) = set {
            put_group_set(&mut doc, group, field, set);
        }
    }
    (doc, results)
}

// ---------------------------------------------------------------------------------------------
// styles.json
// ---------------------------------------------------------------------------------------------

const STYLE_ORDER: &str = "order";
const OVERRIDES: &str = "overrides";
const CUSTOM: &str = "custom";
const HOTKEY: &str = "hotkey";
const STYLE_HOTKEY: &str = "style_hotkey";

/// 自定义风格里不投影的字段：快捷键按系统单独同步；`builtin` / `modified` 由程序在加载时重新计算
const CUSTOM_LOCAL_FIELDS: [&str; 3] = [HOTKEY, "builtin", "modified"];

fn is_builtin_style(id: &str) -> bool {
    BUILTIN_STYLE_IDS.contains(&id)
}

fn project_styles(root: &Map<String, Value>, os: OsKind, out: &mut BTreeMap<String, Value>) {
    if let Some(order) = root.get(STYLE_ORDER).filter(|v| v.is_array()) {
        out.insert(path_key([STYLE_ORDER]), order.clone());
    }
    if let Some(overrides) = root.get(OVERRIDES).and_then(Value::as_object) {
        for (id, ov) in overrides {
            let Some(ov) = ov.as_object() else { continue };
            if id.is_empty() {
                continue;
            }
            let mut stripped = ov.clone();
            if let Some(hotkey) = stripped.remove(HOTKEY) {
                out.insert(os_key(os, [STYLE_HOTKEY, id.as_str()]), hotkey);
            }
            // 只剩快捷键的覆盖项在全局层面等于没有覆盖
            if !stripped.is_empty() {
                out.insert(path_key([OVERRIDES, id.as_str()]), Value::Object(stripped));
            }
        }
    }
    for item in root.get(CUSTOM).and_then(Value::as_array).into_iter().flatten() {
        let (Some(id), Some(style)) = (id_of(item), item.as_object()) else {
            continue;
        };
        let key = path_key([CUSTOM, id.as_str()]);
        if out.contains_key(&key) {
            continue;
        }
        let mut stripped = style.clone();
        for f in CUSTOM_LOCAL_FIELDS {
            stripped.remove(f);
        }
        // 自定义风格的 hotkey 为 null 就是「没有快捷键」，不产出寄存器
        if let Some(hotkey) = style.get(HOTKEY).filter(|h| !h.is_null()) {
            out.insert(os_key(os, [STYLE_HOTKEY, id.as_str()]), hotkey.clone());
        }
        out.insert(key, Value::Object(stripped));
    }
}

#[derive(Debug, PartialEq, Eq)]
enum StylesKey {
    Order,
    Override(String),
    Custom(String),
    /// `@<os>/style_hotkey/<id>`：`id` 只在形状正确时为 `Some`
    Hotkey {
        os: String,
        id: Option<String>,
    },
    Local,
    Unknown,
}

fn parse_styles_key(key: &str) -> StylesKey {
    let Some((tag, segs)) = split_key(key) else {
        return StylesKey::Unknown;
    };
    if let Some(tag) = tag {
        let id = match segs.as_slice() {
            [Seg::Name(k), Seg::Name(id)] if k == STYLE_HOTKEY => Some(id.clone()),
            _ => None,
        };
        return StylesKey::Hotkey { os: tag.to_owned(), id };
    }
    match segs.as_slice() {
        [Seg::Name(n)] if n == STYLE_ORDER => StylesKey::Order,
        [Seg::Name(n)] if n == SCHEMA_VERSION => StylesKey::Local,
        [Seg::Name(n), Seg::Name(id)] if n == OVERRIDES => StylesKey::Override(id.clone()),
        [Seg::Name(n), Seg::Name(id)] if n == CUSTOM => StylesKey::Custom(id.clone()),
        _ => StylesKey::Unknown,
    }
}

/// 取出 `overrides` 对象（不存在时为空）；存在但不是对象时返回 `None`
fn take_overrides(doc: &mut Map<String, Value>) -> Option<(Map<String, Value>, bool)> {
    match doc.get_mut(OVERRIDES) {
        None => Some((Map::new(), false)),
        Some(Value::Object(map)) => Some((std::mem::take(map), true)),
        Some(_) => None,
    }
}

fn apply_override(
    overrides: &mut Map<String, Value>,
    id: &str,
    value: Option<&Value>,
    base: &Map<String, Value>,
) -> KeyApply {
    // 更新的版本新增的内置风格：本机的规范化会丢掉它，先推迟，升级后再应用
    if !is_builtin_style(id) {
        return KeyApply::Deferred;
    }
    let mut next = match value {
        Some(Value::Object(v)) => v.clone(),
        Some(_) => return KeyApply::Deferred,
        None => Map::new(),
    };
    next.remove(HOTKEY);
    let local_hotkey = base
        .get(OVERRIDES)
        .and_then(Value::as_object)
        .and_then(|m| m.get(id))
        .and_then(Value::as_object)
        .and_then(|o| o.get(HOTKEY));
    if let Some(hotkey) = local_hotkey {
        next.insert(HOTKEY.to_owned(), hotkey.clone());
    }
    if next.is_empty() {
        overrides.remove(id);
    } else {
        overrides.insert(id.to_owned(), Value::Object(next));
    }
    KeyApply::Applied
}

fn apply_custom(list: &mut Keyed, id: &str, value: Option<&Value>, base: &Map<String, Value>) -> KeyApply {
    match value {
        None => {
            list.remove(id);
            KeyApply::Applied
        }
        Some(Value::Object(v)) => {
            let mut next = v.clone();
            for f in CUSTOM_LOCAL_FIELDS {
                next.remove(f);
            }
            next.insert("id".to_owned(), Value::String(id.to_owned()));
            if let Some(local) = find_by(base.get(CUSTOM), id, id_of) {
                for f in CUSTOM_LOCAL_FIELDS {
                    if let Some(v) = local.get(f) {
                        next.insert(f.to_owned(), v.clone());
                    }
                }
            }
            list.set(id, Value::Object(next));
            KeyApply::Applied
        }
        Some(_) => KeyApply::Deferred,
    }
}

/// 本系统的风格快捷键：写进自定义风格的 `hotkey`，或内置风格覆盖项的 `hotkey`
fn apply_style_hotkey(
    custom: Option<&mut Keyed>,
    overrides: Option<&mut Map<String, Value>>,
    id: &str,
    value: Option<&Value>,
) -> KeyApply {
    let hotkey = match value {
        Some(v @ (Value::String(_) | Value::Null)) => Some(v.clone()),
        Some(_) => return KeyApply::Deferred,
        None => None,
    };
    if let Some(style) = custom.and_then(|list| list.get_mut(id)).and_then(Value::as_object_mut) {
        style.insert(HOTKEY.to_owned(), hotkey.unwrap_or(Value::Null));
        return KeyApply::Applied;
    }
    if is_builtin_style(id) {
        let Some(overrides) = overrides else {
            return KeyApply::Deferred;
        };
        match hotkey {
            Some(hotkey) => match overrides
                .entry(id.to_owned())
                .or_insert_with(|| Value::Object(Map::new()))
            {
                Value::Object(ov) => {
                    ov.insert(HOTKEY.to_owned(), hotkey);
                }
                _ => return KeyApply::Deferred,
            },
            None => {
                if let Some(Value::Object(ov)) = overrides.get_mut(id) {
                    ov.remove(HOTKEY);
                    if ov.is_empty() {
                        overrides.remove(id);
                    }
                }
            }
        }
        return KeyApply::Applied;
    }
    // 风格本机还没有：删除快捷键已经是现状；设置快捷键要等风格本身到达后再应用
    match hotkey {
        None => KeyApply::Applied,
        Some(_) => KeyApply::Deferred,
    }
}

fn unproject_styles(
    base: &Map<String, Value>,
    changes: &[(String, Option<Value>)],
    os: OsKind,
) -> (Map<String, Value>, Vec<KeyApply>) {
    let mut doc = base.clone();
    let mut results = vec![KeyApply::Deferred; changes.len()];
    let local_tag = os_tag(os);
    let mut custom = Keyed::take(&mut doc, CUSTOM, id_of);
    let mut overrides = take_overrides(&mut doc);
    // 快捷键最后处理：同一批里的自定义风格要先落定
    let mut hotkeys = Vec::new();

    for (i, (key, value)) in changes.iter().enumerate() {
        let value = value.as_ref();
        results[i] = match parse_styles_key(key) {
            StylesKey::Order => match value {
                Some(v @ Value::Array(_)) => {
                    doc.insert(STYLE_ORDER.to_owned(), v.clone());
                    KeyApply::Applied
                }
                Some(_) => KeyApply::Deferred,
                None => {
                    doc.remove(STYLE_ORDER);
                    KeyApply::Applied
                }
            },
            StylesKey::Override(id) => match overrides.as_mut() {
                Some((map, _)) => apply_override(map, &id, value, base),
                None => KeyApply::Deferred,
            },
            StylesKey::Custom(id) => match custom.as_mut() {
                Some(list) => apply_custom(list, &id, value, base),
                None => KeyApply::Deferred,
            },
            StylesKey::Hotkey { os, .. } if os != local_tag => KeyApply::Applied,
            StylesKey::Hotkey { id: Some(id), .. } => {
                hotkeys.push((i, id));
                continue;
            }
            StylesKey::Local => KeyApply::Applied,
            StylesKey::Hotkey { id: None, .. } | StylesKey::Unknown => KeyApply::Deferred,
        };
    }
    for (i, id) in hotkeys {
        results[i] = apply_style_hotkey(
            custom.as_mut(),
            overrides.as_mut().map(|(map, _)| map),
            &id,
            changes[i].1.as_ref(),
        );
    }

    if let Some(list) = custom {
        list.put(&mut doc, CUSTOM, None);
    }
    if let Some((map, present)) = overrides
        && (present || !map.is_empty())
    {
        doc.insert(OVERRIDES.to_owned(), Value::Object(map));
    }
    (doc, results)
}

// ---------------------------------------------------------------------------------------------
// glossary.json
// ---------------------------------------------------------------------------------------------

const ENTRIES: &str = "entries";
const GLOSSARY_SOURCE: &str = "source";

fn project_glossary(root: &Map<String, Value>, out: &mut BTreeMap<String, Value>) {
    for item in root.get(ENTRIES).and_then(Value::as_array).into_iter().flatten() {
        let (Some(source), Some(entry)) = (source_of(item), item.as_object()) else {
            continue;
        };
        out.entry(source).or_insert_with(|| {
            let mut value = entry.clone();
            value.remove(GLOSSARY_SOURCE);
            Value::Object(value)
        });
    }
}

fn unproject_glossary(
    base: &Map<String, Value>,
    changes: &[(String, Option<Value>)],
) -> (Map<String, Value>, Vec<KeyApply>) {
    let mut doc = base.clone();
    let Some(mut entries) = Keyed::take(&mut doc, ENTRIES, source_of) else {
        return (doc, vec![KeyApply::Deferred; changes.len()]);
    };
    let results = changes
        .iter()
        .map(|(source, value)| match value {
            _ if source.is_empty() => KeyApply::Deferred,
            None => {
                entries.remove(source);
                KeyApply::Applied
            }
            Some(Value::Object(v)) => {
                let mut entry = v.clone();
                entry.insert(GLOSSARY_SOURCE.to_owned(), Value::String(source.clone()));
                entries.set(source, Value::Object(entry));
                KeyApply::Applied
            }
            Some(_) => KeyApply::Deferred,
        })
        .collect();
    entries.put(&mut doc, ENTRIES, None);
    (doc, results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_escaping_round_trips() {
        for (raw, escaped) in [
            ("openai", "openai"),
            ("a/b", "a~1b"),
            ("~x", "~0x"),
            ("@cf", "~2cf"),
            ("@cf/meta@x", "~2cf~1meta@x"),
            ("~2", "~02"),
            ("~1/", "~01~1"),
            ("a@b", "a@b"),
        ] {
            assert_eq!(escape_segment(raw), escaped);
            assert_eq!(unescape_segment(escaped).as_deref(), Some(raw));
        }
    }

    #[test]
    fn unescape_rejects_reserved_and_malformed() {
        for bad in ["", "@order", "@macos", "~", "~3", "a~", "a~2b", "x~2"] {
            assert_eq!(unescape_segment(bad), None, "{bad}");
        }
    }

    #[test]
    fn key_parsing() {
        assert_eq!(parse_settings_key("/providers/@order"), SettingsKey::ProviderOrder);
        assert_eq!(
            parse_settings_key("/providers/~2order"),
            SettingsKey::Provider("@order".to_owned())
        );
        assert_eq!(parse_settings_key("/providers/@other"), SettingsKey::Unknown);
        assert_eq!(parse_settings_key("/schema_version"), SettingsKey::Local);
        assert_eq!(parse_settings_key("/advanced/log_level"), SettingsKey::Local);
        assert_eq!(parse_settings_key("/hotkeys/bindings"), SettingsKey::Unknown);
        assert_eq!(parse_settings_key("/clipboard/excluded_apps"), SettingsKey::Unknown);
        assert_eq!(parse_settings_key("/translate/nope"), SettingsKey::Unknown);
        assert_eq!(parse_settings_key("translate/primary_lang"), SettingsKey::Unknown);
        assert_eq!(parse_settings_key("@/hotkeys/bindings"), SettingsKey::Unknown);
        assert_eq!(
            parse_settings_key("/clipboard/excluded_apps/linux:a~1b"),
            SettingsKey::OsApp {
                set: 0,
                os: "linux".to_owned(),
                app_id: "a/b".to_owned()
            }
        );
        assert_eq!(
            parse_settings_key("/clipboard/excluded_apps/linux"),
            SettingsKey::Unknown
        );
        assert_eq!(
            parse_settings_key("@windows/hotkeys/copy_wait_ms"),
            SettingsKey::PerOs {
                os: "windows".to_owned(),
                field: Some(("hotkeys".to_owned(), "copy_wait_ms".to_owned()))
            }
        );
        assert_eq!(
            parse_settings_key("@windows/translate/primary_lang"),
            SettingsKey::PerOs {
                os: "windows".to_owned(),
                field: None
            }
        );
        assert_eq!(
            parse_styles_key("@linux/style_hotkey/~2x"),
            StylesKey::Hotkey {
                os: "linux".to_owned(),
                id: Some("@x".to_owned())
            }
        );
    }

    #[test]
    fn schema_covers_settings_groups() {
        let schema = settings_schema();
        for group in [
            "general",
            "hotkeys",
            "defaults",
            "translate",
            "ocr",
            "clipboard",
            "appearance",
        ] {
            assert!(schema.contains_key(group), "{group}");
        }
        for (g, f) in LOCAL_FIELDS
            .iter()
            .chain(PER_OS_FIELDS.iter())
            .chain(OS_APP_SETS.iter())
        {
            assert!(schema.get(*g).is_some_and(|s| s.contains(*f)), "{g}.{f}");
        }
    }
}
