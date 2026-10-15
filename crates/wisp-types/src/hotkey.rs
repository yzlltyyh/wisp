//! 快捷键与扩展触发相关类型。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 五个全局动作
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HotkeyAction {
    TranslateSelection,
    TranslateInput,
    Ocr,
    OcrTranslate,
    Clipboard,
}

impl HotkeyAction {
    /// 全部动作（顺序即设置页与托盘中的顺序）
    pub const ALL: [HotkeyAction; 5] = [
        Self::TranslateSelection,
        Self::TranslateInput,
        Self::Ocr,
        Self::OcrTranslate,
        Self::Clipboard,
    ];

    /// 序列化后的字符串（与 serde 一致）
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TranslateSelection => "translate_selection",
            Self::TranslateInput => "translate_input",
            Self::Ocr => "ocr",
            Self::OcrTranslate => "ocr_translate",
            Self::Clipboard => "clipboard",
        }
    }
}

/// 快捷键绑定：`"default"` | `"none"` | global-hotkey 规范串
/// （修饰键顺序固定为 Control+Alt+Shift+Super，例如 "Alt+KeyD"、"Shift+Super+KeyV"）。
///
/// **比较规则**：组合键一律先解析成（修饰键集合, 主键 code）再比较，**禁止**直接比较字符串——
/// global-hotkey 0.8 的 `HotKey::into_string()` 输出的是小写、`shift+control+alt+super` 顺序的串，与规范串不同。
/// Rust 侧的规范化只有一个入口：`wisp_platform::common::combo::canonical`（解析 + 输出规范串）；
/// 前端对应 `shared/keys/combo.ts`，两边共用同一份 fixture。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Binding(pub String);

impl Binding {
    /// 「按平台默认表」的保留值
    pub const DEFAULT: &'static str = "default";
    /// 「不绑定」的保留值
    pub const NONE: &'static str = "none";

    /// 构造 `"default"`
    pub fn default_binding() -> Self {
        Self(Self::DEFAULT.to_owned())
    }

    /// 构造 `"none"`
    pub fn none() -> Self {
        Self(Self::NONE.to_owned())
    }

    /// 是否为 `"default"`
    pub fn is_default(&self) -> bool {
        self.0 == Self::DEFAULT
    }

    /// 是否为 `"none"`
    pub fn is_none(&self) -> bool {
        self.0 == Self::NONE
    }

    /// 是否为具体的组合键（既不是 default 也不是 none）
    pub fn is_combo(&self) -> bool {
        !self.is_default() && !self.is_none()
    }

    /// 原始字符串
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for Binding {
    fn default() -> Self {
        Self::default_binding()
    }
}

/// 快捷键作用对象
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HotkeyTarget {
    Action { action: HotkeyAction },
    Style { style_id: String },
}

/// 冲突级别
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictLevel {
    Forbidden,
    Warning,
    Info,
}

/// 冲突来源
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictSource {
    Reserved,
    SelfDuplicate,
    StaticApp,
    SystemDynamic,
    Registration,
    Neighbor,
    Layout,
    DeadKey,
}

/// 单条冲突
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub level: ConflictLevel,
    pub source: ConflictSource,
    /// 前端 i18n key，形如 `hotkeys.conflict.<name>`
    pub message_key: String,
    pub params: BTreeMap<String, String>,
}

/// 冲突检查报告
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictReport {
    pub combo: String,
    pub display: String,
    pub conflicts: Vec<Conflict>,
    pub acceptable: bool,
}

/// 注册状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HotkeyState {
    Registered,
    Fallback,
    Failed,
    Unbound,
    Suspended,
    Unavailable,
}

/// 单个快捷键的状态
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotkeyStatus {
    pub target: HotkeyTarget,
    pub binding: Binding,
    /// 实际生效的组合键（回退后可能是备选键）
    pub effective: Option<String>,
    pub display: Option<String>,
    pub state: HotkeyState,
    /// 前端 i18n key（无参数）：`hotkeys.reason.<name>`（回退 / 失败 / 录制中的原因），或平台原因
    /// `errors.platform.wayland_hotkeys` / `errors.platform.no_display`
    pub reason_key: Option<String>,
}

/// 可用于「双击」的修饰键
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModifierKey {
    LeftAlt,
    RightAlt,
    LeftCtrl,
    RightCtrl,
    LeftCmd,
    RightCmd,
}

/// 鼠标额外按键
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButtonKind {
    Middle,
    Back,
    Forward,
}

/// 扩展触发的状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtTriggerState {
    #[default]
    Off,
    Active,
    WaitingPermission,
    Unavailable,
}

/// 三种扩展触发的状态
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ExtTriggerStatus {
    pub bobber: ExtTriggerState,
    pub double_tap: ExtTriggerState,
    pub mouse: ExtTriggerState,
    /// 前端 i18n key（无参数）：`hotkeys.ext.<name>`，或平台层的原因 `errors.platform.*`
    pub reason_key: Option<String>,
}

/// 邻居应用占用的组合键
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NeighborHit {
    pub app_name: String,
    pub combo: String,
    pub display: String,
    pub action: HotkeyAction,
}

/// 键盘布局建议
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct KeyboardAdvice {
    pub has_altgr: bool,
    pub multi_input_lang: bool,
    /// "alt_shift"
    pub recommend_family: Option<String>,
    pub qq_running: bool,
}
