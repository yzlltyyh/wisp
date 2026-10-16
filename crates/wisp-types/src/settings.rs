//! `settings.json` 的完整 schema。
//!
//! 缺字段时取默认值；数组整体替换（JSON Merge Patch 语义）。默认值见各 `Default` 实现，与默认内容示例逐项一致
//! （黄金样例 `Settings.json` 校验）。
//!
//! 注意 f32 字段（如 `max_height_ratio`）：经 `serde_json::to_value` 中转会被拓宽成 0.6000000238418579。
//! 写盘与 IPC 一律直接从强类型结构序列化（`to_string` / `to_string_pretty` 输出 0.6）；Merge Patch 可以在 Value 上做，
//! 但合并后要先 `from_value` 回 `Settings` 再序列化落盘。
//!
//! **容错边界（settings.json 允许用户手改）**：`Settings` 与全部子结构都由 `lenient_struct!` 生成**宽松**的
//! `Deserialize`（规则见 [`crate::lenient`]）：单个字段非法（枚举值写错如 `"theme": "Dark"`、类型不对）只让这个字段
//! 退回默认值；`providers`、`providers[].models`、`clipboard.excluded_apps`、`hotkeys.app_keymaps` 逐条容错（非法条目
//! 丢弃）；`hotkeys.bindings` 里未知 / 拼错的动作 key 只丢弃那一项。只有顶层不是合法 JSON 对象时才走「备份 + 默认值」。
//! 设置服务读文件与热加载用 [`crate::lenient::parse_lenient`] 拿到问题列表并逐条 `warn`（不写值本身）；`settings_patch` 用
//! [`crate::lenient::from_value_lenient`]，问题列表非空即返回 `invalid_argument`。
//!
//! 新增字段：写进对应的 `lenient_struct!` 定义并补 `Default`；数组 / map 字段用 `=> crate::lenient::vec` / `map`。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::lenient::lenient_struct;

use crate::app::OsKind;
use crate::defaults;
use crate::hotkey::{Binding, HotkeyAction, ModifierKey};
use crate::lang::{Lang, UiLanguage};
use crate::llm::{Defaults, ImageDetail, Provider};
use crate::style::OcrMode;

/// 当前 settings.json 的 schema 版本
pub const SETTINGS_SCHEMA_VERSION: u32 = 1;

lenient_struct! {
    /// `settings.json`
    #[derive(Debug, Clone, PartialEq, Serialize)]
    pub struct Settings {
        /// 1；启动时按版本号迁移（settings/migrate.rs）
        pub schema_version: u32,
        pub general: General,
        pub hotkeys: HotkeySettings,
        /// 默认 []
        pub providers: Vec<Provider> => crate::lenient::vec,
        /// 默认 { translate: null, ocr: null }
        pub defaults: Defaults,
        pub translate: TranslateSettings,
        pub ocr: OcrSettings,
        pub clipboard: ClipboardSettings,
        pub appearance: Appearance,
        pub advanced: Advanced,
        /// 多设备同步（只属于本机，不参与同步）
        pub sync: SyncSettings,
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            general: General::default(),
            hotkeys: HotkeySettings::default(),
            providers: Vec::new(),
            defaults: Defaults::default(),
            translate: TranslateSettings::default(),
            ocr: OcrSettings::default(),
            clipboard: ClipboardSettings::default(),
            appearance: Appearance::default(),
            advanced: Advanced::default(),
            sync: SyncSettings::default(),
        }
    }
}

impl Settings {
    /// 按 id 查找 Provider
    pub fn provider(&self, id: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.id == id)
    }

    /// 某个平台上的**新配置**（首次运行写出 settings.json、损坏后重置为默认值时使用）：
    /// `Settings::default()` 加上平台相关的默认值（目前只有 [`defaults::unload_panel_default`]）。
    /// `default()` 本身保持平台无关（黄金样例 `Settings.json` 在三端 CI 上逐字一致）
    pub fn for_os(os: OsKind) -> Self {
        let mut s = Self::default();
        s.advanced.unload_panel_after_idle_min = defaults::unload_panel_default(os);
        s
    }
}

/// 更新策略
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdatePolicy {
    #[default]
    Notify,
    AutoInstall,
    Off,
}

lenient_struct! {
    /// 通用
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub struct General {
        /// true：开机自启
        pub launch_at_login: bool,
        /// true：静默启动到托盘（首次运行仍会打开引导）
        pub start_hidden: bool,
        /// "system"：系统语言为 zh-Hans / zh-CN / zh-SG 时用 zh-CN，否则 en-US
        pub ui_language: UiLanguage,
        pub update_policy: UpdatePolicy,
        /// false：为 true 时面板底部显示延迟埋点
        pub developer_mode: bool,
    }
}

impl Default for General {
    fn default() -> Self {
        Self {
            launch_at_login: true,
            start_hidden: true,
            ui_language: UiLanguage::System,
            update_policy: UpdatePolicy::Notify,
            developer_mode: false,
        }
    }
}

lenient_struct! {
    /// 快捷键与扩展触发
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub struct HotkeySettings {
        /// 缺失的动作视为 "default"（按平台默认表 + 首选→备选回退）
        pub bindings: BTreeMap<HotkeyAction, Binding> => crate::lenient::map,
        pub bobber: BobberSettings,
        pub double_tap: DoubleTapSettings,
        pub mouse: MouseSettings,
        /// 用户增改的「应用 → 复制键 / 粘贴键」；内置默认表在代码里，用户条目优先
        pub app_keymaps: Vec<AppKeymap> => crate::lenient::vec,
        /// 150（50–500），模拟复制的等待上限
        pub copy_wait_ms: u32,
    }
}

impl Default for HotkeySettings {
    fn default() -> Self {
        Self {
            bindings: BTreeMap::new(),
            bobber: BobberSettings::default(),
            double_tap: DoubleTapSettings::default(),
            mouse: MouseSettings::default(),
            app_keymaps: Vec::new(),
            copy_wait_ms: 150,
        }
    }
}

impl HotkeySettings {
    /// 某个动作的绑定；缺失视为 "default"
    pub fn binding(&self, action: HotkeyAction) -> Binding {
        self.bindings.get(&action).cloned().unwrap_or_default()
    }
}

lenient_struct! {
    /// 浮标
    #[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
    pub struct BobberSettings {
        pub enabled: bool,
    }
}

lenient_struct! {
    /// 双击修饰键
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub struct DoubleTapSettings {
        pub enabled: bool,
        /// null = 平台默认（mac = right_alt，win/linux = right_ctrl，见 `defaults::default_double_tap_key`）
        pub key: Option<ModifierKey>,
        /// 300（只允许 200/300/400）
        pub interval_ms: u32,
    }
}

impl Default for DoubleTapSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            key: None,
            interval_ms: 300,
        }
    }
}

lenient_struct! {
    /// 鼠标额外按键
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub struct MouseSettings {
        pub enabled: bool,
        pub long_press: bool,
        pub middle: MouseBinding,
        pub back: MouseBinding,
        pub forward: MouseBinding,
    }
}

impl Default for MouseSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            long_press: false,
            middle: MouseBinding {
                enabled: true,
                action: Some(HotkeyAction::TranslateSelection),
            },
            back: MouseBinding {
                enabled: false,
                action: Some(HotkeyAction::Clipboard),
            },
            forward: MouseBinding {
                enabled: false,
                action: Some(HotkeyAction::Ocr),
            },
        }
    }
}

lenient_struct! {
    /// 单个鼠标键的绑定
    #[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
    pub struct MouseBinding {
        pub enabled: bool,
        pub action: Option<HotkeyAction>,
    }
}

/// 「应用 → 复制键 / 粘贴键」映射
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppKeymap {
    pub os: OsKind,
    pub app_id: String,
    /// "none" = 不发复制键；null = 用平台默认
    #[serde(default)]
    pub copy: Option<String>,
    #[serde(default)]
    pub paste: Option<String>,
}

/// 翻译锚点
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranslateAnchor {
    /// 选区 > 鼠标 > 中心
    #[default]
    Selection,
    Cursor,
}

/// 输入视图锚点
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputAnchor {
    #[default]
    Cursor,
    Center,
}

lenient_struct! {
    /// 翻译
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub struct TranslateSettings {
        /// "zh-CN"
        pub primary_lang: Lang,
        /// "en"
        pub secondary_lang: Lang,
        /// "natural"
        pub default_style: String,
        /// 3000（500–20000）
        pub long_text_threshold: u32,
        /// true
        pub close_on_click_outside: bool,
        /// false
        pub close_on_mouse_leave: bool,
        /// 160（80–400）
        pub mouse_leave_px: u32,
        pub anchor: TranslateAnchor,
        pub input_anchor: InputAnchor,
        /// true
        pub cache_enabled: bool,
        /// false
        pub show_source_by_default: bool,
    }
}

impl Default for TranslateSettings {
    fn default() -> Self {
        Self {
            primary_lang: Lang::ZhCn,
            secondary_lang: Lang::En,
            default_style: defaults::DEFAULT_STYLE_ID.to_owned(),
            long_text_threshold: 3000,
            close_on_click_outside: true,
            close_on_mouse_leave: false,
            mouse_leave_px: 160,
            anchor: TranslateAnchor::Selection,
            input_anchor: InputAnchor::Cursor,
            cache_enabled: true,
            show_source_by_default: false,
        }
    }
}

lenient_struct! {
    /// OCR
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub struct OcrSettings {
        /// "lines"（只允许 lines / paragraphs / markdown）
        pub default_mode: OcrMode,
        /// 2048（512–4096）
        pub max_edge_px: u32,
        /// true
        pub auto_copy: bool,
        /// false
        pub history: bool,
        /// 50（10–500）
        pub history_limit: u32,
        /// true
        pub window_detect: bool,
        /// true（减少动效时强制视为关闭）
        pub fly_animation: bool,
        /// false：直接翻译时是否同时转录原文（两次调用）
        pub direct_translate_transcribe: bool,
        /// "high"
        pub image_detail: ImageDetail,
    }
}

impl Default for OcrSettings {
    fn default() -> Self {
        Self {
            default_mode: OcrMode::Lines,
            max_edge_px: 2048,
            auto_copy: true,
            history: false,
            history_limit: 50,
            window_detect: true,
            fly_animation: true,
            direct_translate_transcribe: false,
            image_detail: ImageDetail::High,
        }
    }
}

/// 剪贴板面板位置
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipPanelPosition {
    #[default]
    Center,
    Cursor,
    Remember,
}

/// 剪贴板条目的粘贴触发方式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PasteOn {
    #[default]
    SingleClick,
    DoubleClick,
}

lenient_struct! {
    /// 剪贴板
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub struct ClipboardSettings {
        /// true：监听开关（托盘的「暂停」不改这个字段，暂停状态存 state.json）
        pub enabled: bool,
        pub record_text: bool,
        pub record_rich_text: bool,
        pub record_images: bool,
        pub record_files: bool,
        /// 1000（不含置顶）
        pub max_items: u32,
        /// null（不限）| 7 | 30 | 90
        pub max_age_days: Option<u32>,
        /// 1048576
        pub max_text_bytes: u32,
        /// 20971520
        pub max_image_bytes: u32,
        /// 预置密码管理器（三端条目都在，按 os 字段过滤使用）
        pub excluded_apps: Vec<ExcludedApp> => crate::lenient::vec,
        /// true
        pub secret_detection: bool,
        /// true
        pub secret_tray_flash: bool,
        pub panel_position: ClipPanelPosition,
        pub paste_on: PasteOn,
        /// true
        pub show_hint_bar: bool,
        /// 智能识别（芯片条、验证码自动删除）
        pub smart: SmartSettings,
    }
}

impl Default for ClipboardSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            record_text: true,
            record_rich_text: true,
            record_images: true,
            record_files: true,
            max_items: 1000,
            max_age_days: None,
            max_text_bytes: 1_048_576,
            max_image_bytes: 20_971_520,
            excluded_apps: defaults::default_excluded_apps(),
            secret_detection: true,
            secret_tray_flash: true,
            panel_position: ClipPanelPosition::Center,
            paste_on: PasteOn::SingleClick,
            show_hint_bar: true,
            smart: SmartSettings::default(),
        }
    }
}

lenient_struct! {
    /// 剪贴板的智能识别：哪些种类的片段显示为芯片，以及验证码条目是否自动删除。随设置同步
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
    pub struct SmartSettings {
        /// true：邮箱、电话、链接、IP
        pub contacts: bool,
        /// true：验证码
        pub codes: bool,
        /// true：快递单号、订单号、金额、日期时间
        pub numbers: bool,
        /// false：身份证号、银行卡号（敏感内容，默认不识别）
        pub identity: bool,
        /// true：验证码条目 10 分钟后在本机自动删除（收藏的除外）
        pub otp_auto_delete: bool,
    }
}

impl Default for SmartSettings {
    fn default() -> Self {
        Self {
            contacts: true,
            codes: true,
            numbers: true,
            identity: false,
            otp_auto_delete: true,
        }
    }
}

impl SmartSettings {
    /// 这一种片段是否要识别（按设置里的四个分组）
    pub const fn allows(&self, kind: crate::clip::EntityKind) -> bool {
        use crate::clip::EntityKind as K;
        match kind {
            K::Email | K::Phone | K::Url | K::Ip => self.contacts,
            K::Otp => self.codes,
            K::Tracking | K::Order | K::Amount | K::Datetime => self.numbers,
            K::IdCard | K::BankCard => self.identity,
        }
    }
}

/// 剪贴板排除名单条目（mac 按 bundle id，Win 按 exe，Linux 按 WM_CLASS）
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExcludedApp {
    pub os: OsKind,
    pub app_id: String,
    /// 缺省为空串（界面退回显示 app_id）
    #[serde(default)]
    pub name: String,
}

/// 主题偏好
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemePref {
    #[default]
    System,
    Light,
    Dark,
}

/// 材质偏好
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterialPref {
    #[default]
    System,
    Solid,
}

/// 减少动效偏好
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReduceMotionPref {
    #[default]
    System,
    On,
    Off,
}

lenient_struct! {
    /// 外观
    #[derive(Debug, Clone, PartialEq, Serialize)]
    pub struct Appearance {
        pub theme: ThemePref,
        /// "system" | 预设名（blue/purple/pink/red/orange/yellow/green/graphite）| "#RRGGBB"
        pub accent: String,
        /// 14（12–18）
        pub font_size: u32,
        /// 360（320–480）
        pub translate_min_width: u32,
        /// 520（440–720）
        pub translate_max_width: u32,
        /// 420（360–560）
        pub clipboard_width: u32,
        /// 0.6（0.4–0.8）
        pub max_height_ratio: f32,
        /// 系统开启「降低透明度」时强制为 solid
        pub material: MaterialPref,
        pub reduce_motion: ReduceMotionPref,
    }
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: ThemePref::System,
            accent: defaults::ACCENT_SYSTEM.to_owned(),
            font_size: 14,
            translate_min_width: 360,
            translate_max_width: 520,
            clipboard_width: 420,
            max_height_ratio: 0.6,
            material: MaterialPref::System,
            reduce_motion: ReduceMotionPref::System,
        }
    }
}

/// 日志级别
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
}

lenient_struct! {
    /// 高级
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub struct Advanced {
        /// "info"
        pub log_level: LogLevel,
        /// true
        pub webview_low_memory_when_hidden: bool,
        /// null（关）| 5–1440：面板隐藏超过 N 分钟后销毁 Panel WebView，下次唤出前重建（WebView 内存超标时的兜底；下限 5，
        /// 新配置的平台默认值见 `defaults::unload_panel_default`）
        pub unload_panel_after_idle_min: Option<u32>,
    }
}

impl Default for Advanced {
    fn default() -> Self {
        Self {
            log_level: LogLevel::Info,
            webview_low_memory_when_hidden: true,
            unload_panel_after_idle_min: None,
        }
    }
}

lenient_struct! {
    /// 多设备同步。整组只属于本机（不参与同步本身）：地址与账号指向这台设备自己的 WebDAV 凭据，开关可以每台设备不同。
    /// `enabled` / `server_url` / `username` 只由连接、断开与清除服务器数据的命令写入；`auto` 与 `content` 由设置页修改。
    /// `Debug` 手写：地址与账号不进日志
    #[derive(Clone, PartialEq, Eq, Serialize)]
    pub struct SyncSettings {
        /// false
        pub enabled: bool,
        /// ""：用户填写的 WebDAV 地址（http / https）
        pub server_url: String,
        /// ""
        pub username: String,
        /// true：改动后自动上传、使用 Wisp 时取回其他设备的改动；false 时只在「立即同步」时同步
        pub auto: bool,
        pub content: SyncContent,
    }
}

impl Default for SyncSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            server_url: String::new(),
            username: String::new(),
            auto: true,
            content: SyncContent::default(),
        }
    }
}

impl std::fmt::Debug for SyncSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 地址与账号只说明有没有填写，不输出内容
        let filled = |v: &str| if v.is_empty() { "" } else { "***" };
        f.debug_struct("SyncSettings")
            .field("enabled", &self.enabled)
            .field("server_url", &filled(&self.server_url))
            .field("username", &filled(&self.username))
            .field("auto", &self.auto)
            .field("content", &self.content)
            .finish()
    }
}

lenient_struct! {
    /// 同步哪些内容（也是 `sync_connect` 的参数：连接前可以先调整）
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
    pub struct SyncContent {
        /// true：设置、模型服务、风格与术语表
        pub settings: bool,
        /// true：API Key（`settings` 关闭时无效——Key 按模型服务存放，脱离模型服务同步没有意义）
        pub api_keys: bool,
        /// true：剪贴板历史
        pub clipboard: bool,
        /// true：剪贴板图片与识别截图（单张不超过 `defaults::SYNC_MAX_IMAGE_BYTES`）
        pub images: bool,
        /// true：识别历史（只在本机开启了识别历史时生效）
        pub ocr_history: bool,
        /// true：剪贴板里的验证码条目（`clipboard` 关闭时无效）。关闭后本机的验证码不上传，其他设备的也不接收
        pub verification_codes: bool,
    }
}

impl Default for SyncContent {
    fn default() -> Self {
        Self {
            settings: true,
            api_keys: true,
            clipboard: true,
            images: true,
            ocr_history: true,
            verification_codes: true,
        }
    }
}
