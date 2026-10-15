//! IPC 载荷：面板、Overlay、Pinned、设置快照、外观、更新等。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::app::{OsKind, PlatformInfo};
use crate::clip::{ClipPage, CopiedReason};
use crate::geom::{MonitorId, MonitorInfo, PhysSize, Rect};
use crate::hotkey::{ExtTriggerStatus, HotkeyAction, HotkeyStatus, HotkeyTarget};
use crate::lang::Lang;
use crate::llm::{ModelRef, RequestId, TruncateReason, Usage};
use crate::perm::{PermissionKind, PermissionSet};
use crate::settings::Settings;
use crate::style::{DictMode, OcrMode, Style, TaskKind};

/// 面板视图
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelView {
    Translate,
    Ocr,
    Clipboard,
    Input,
    Toast,
}

/// 面板相对锚点的放置方向
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    Below,
    Above,
    Center,
}

/// 视图进入方式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelTransition {
    Enter,
    Crossfade,
}

/// `panel:prepare` 事件载荷
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PanelPrepare {
    pub session_id: u64,
    pub view: PanelView,
    pub payload: PanelPayload,
    pub width: f64,
    pub max_height: f64,
    pub placement: Placement,
    /// 锚点 x 相对卡片左边的偏移（逻辑 px），CSS 帧模式的 transform-origin 用
    pub origin_x: f64,
    /// 显示后是否持有键盘焦点（输入 / 剪贴板视图为 true）
    pub focused: bool,
    pub transition: PanelTransition,
}

/// 面板各视图的初始载荷
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PanelPayload {
    Translate(TranslatePayload),
    Ocr(OcrPayload),
    Clipboard(ClipboardPayload),
    Input(InputPayload),
    Toast(ToastPayload),
}

/// toast 视图
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToastPayload {
    pub reason: CopiedReason,
    /// 当前平台 / 目标应用实际的粘贴键显示串，如 "⌘V"、"Ctrl+Shift+V"
    pub paste_key: String,
}

/// 翻译视图
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranslatePayload {
    pub source_text: String,
    /// 为 null 表示尚未发请求（长文本确认 / 未就绪）
    pub request_id: Option<RequestId>,
    pub task: TaskKind,
    pub style_id: String,
    pub detected_lang: Option<Lang>,
    pub target_lang: Lang,
    pub dict_mode: bool,
    pub long_text: Option<LongTextInfo>,
    pub not_ready: Option<NotReady>,
    /// 开发者模式显示：ax / uia / primary / copy
    pub selection_method: Option<String>,
}

/// 长文本确认信息
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LongTextInfo {
    pub chars: u32,
    pub est_tokens: u32,
    pub est_seconds: Option<f32>,
}

/// 未就绪原因
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum NotReady {
    NoProvider,
    NoDefaultModel {
        task: TaskKind,
    },
    NoVisionModel,
    PermissionMissing {
        permission: PermissionKind,
    },
    WaylandCopyFirst,
    /// 截图失败。`message` 是前端 i18n key（`errors.capture.*` 或 `errors.platform.wayland_capture`），**不是**
    /// 开发者文本；映射见 desktop 的 `orchestrator::not_ready::capture_not_ready`
    CaptureFailed {
        message: String,
    },
    Elevated,
    KeychainUnavailable,
    /// 取词路径不可用，原因来自平台层（`SelectionError::Unsupported` 的 i18n key，如
    /// `errors.platform.clipboard_too_large`、`errors.platform.x11_missing_xtest`）：以输入模式打开，作为
    /// `InputPayload.hint` 显示在输入框上方的提示条里（NotReadyBanner）。Wayland 的「先复制」仍用 `WaylandCopyFirst`
    SelectionUnavailable {
        reason_key: String,
    },
}

/// OCR 视图
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OcrPayload {
    pub capture_id: String,
    pub request_id: Option<RequestId>,
    pub mode: OcrMode,
    pub thumb_url: String,
    pub image_size: PhysSize,
    pub target_lang: Lang,
    pub not_ready: Option<NotReady>,
}

/// 剪贴板视图
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardPayload {
    pub page: ClipPage,
    pub paused: bool,
    pub hint_bar: bool,
    pub paste_on_double_click: bool,
    /// 历史不可用时的原因（i18n key）：`clipboard.history_too_new`（数据库来自更新版本的
    /// Wisp，本版本停用历史、库文件不动）/ `clipboard.history_unavailable`（打不开数据库）。视图在列表顶部显示横幅，
    /// 而不是普通的空状态「复制点什么就会出现在这里」。正常时省略（TS：`unavailable?: string | null`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
    /// 监听状态。`paused` 保留（= `recording` 为 paused），`disabled`（设置里关了监听）与
    /// `unavailable`（监听装不上：X11 缺 XFIXES、Wayland 没有 data-control）只在这里。正在记录时省略
    /// （TS：`recording?: ClipboardRecording`，缺省 = `{ state: 'recording' }`）
    #[serde(default, skip_serializing_if = "ClipboardRecording::is_recording")]
    pub recording: ClipboardRecording,
}

/// 剪贴板监听状态（`ClipboardPayload.recording`）
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ClipboardRecording {
    /// 正在记录
    #[default]
    Recording,
    /// 托盘「暂停剪贴板监听」
    Paused,
    /// 设置 › 剪贴板里关闭了监听（横幅「剪贴板监听已关闭」+ [打开设置]）
    Disabled,
    /// 监听装不上；`reason_key` 是 i18n key（`errors.platform.*`）
    Unavailable { reason_key: String },
}

impl ClipboardRecording {
    /// 是否正在记录（序列化时省略这个默认值）
    pub fn is_recording(&self) -> bool {
        matches!(self, Self::Recording)
    }
}

/// 输入视图
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputPayload {
    pub initial_text: String,
    pub target_lang: Option<Lang>,
    pub hint: Option<NotReady>,
}

/// 面板隐藏原因
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HideReason {
    Escape,
    CloseButton,
    CommandW,
    ClickOutside,
    MouseLeave,
    Blur,
    Toggle,
    Paste,
    Pin,
    Retrigger,
    Capture,
    Other,
}

/// `panel_ready` 的返回值
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PanelShown {
    pub session_id: u64,
    pub focused: bool,
}

/// `panel:hide_request` 事件载荷
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PanelHideRequest {
    pub session_id: u64,
    pub reason: HideReason,
}

/// `panel:focus_changed` 事件载荷
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PanelFocusChanged {
    pub session_id: u64,
    pub focused: bool,
}

/// `panel:latency` 事件载荷（仅开发者模式）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencyMarks {
    pub session_id: u64,
    pub origin: String,
    pub method: Option<String>,
    pub selection_ms: Option<u32>,
    pub prepare_ms: u32,
    pub ready_ms: u32,
    /// 到前端第一个 opacity > 0 帧
    pub shown_ms: u32,
    pub first_token_ms: Option<u32>,
    pub ready_timeout: bool,
}

/// `translate_start` 请求
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranslateReq {
    pub session_id: u64,
    pub text: String,
    pub task: TaskKind,
    pub style_id: String,
    /// null = 按自动方向
    pub target_lang: Option<Lang>,
    pub model_ref: Option<ModelRef>,
    pub dict_mode: DictMode,
    /// 重新生成（⌘R）时为 true
    pub bypass_cache: bool,
    /// task = refine 时传入当前译文
    pub draft: Option<String>,
    /// 用户已确认长文本。为 false 时，task 为 translate / polish /
    /// proofread 且原文超过 `translate.long_text_threshold` 的请求**不会发出**：`StartAck.long_text` 带回估算、
    /// `request_id` 为空串，视图显示长文本确认卡片，「继续」时以 true 重发。已经确认过的原文（翻译视图里的换风格、⌘R、
    /// 改目标）一律传 true。缺省 false
    #[serde(default)]
    pub confirm_long: bool,
}

/// `ocr_start` 请求
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OcrReq {
    pub session_id: u64,
    pub capture_id: String,
    pub mode: OcrMode,
    pub model_ref: Option<ModelRef>,
    pub target_lang: Option<Lang>,
}

/// 流式请求的确认
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartAck {
    /// 已登记的请求；`long_text` 非 null 时请求没有发出，为空串
    pub request_id: RequestId,
    /// 宽度变化（例如输入 → 翻译）时返回新宽度
    pub width: Option<f64>,
    /// 原文超过长文本阈值、且 `TranslateReq.confirm_long = false`：请求**没有发出**，这里是确认卡片要显示的估算
    /// （与 `TranslatePayload.long_text` 同一来源）
    #[serde(default)]
    pub long_text: Option<LongTextInfo>,
}

/// 固定窗口的内容类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinKind {
    Translate,
    Ocr,
}

/// 固定窗口的状态
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinState {
    pub kind: PinKind,
    pub source_text: String,
    pub result_text: String,
    pub task: TaskKind,
    pub style_id: String,
    pub target_lang: Lang,
    pub detected_lang: Option<Lang>,
    /// 要转给固定窗口的请求（`transfer_to_pinned`）。流已结束也可以填：OCR 视图据此
    /// 把截图连同请求一起转给 pinned（重新识别、「查看原文」在 pinned 里仍可用）；翻译视图只在流式进行中填
    pub request_id: Option<RequestId>,
    pub capture_id: Option<String>,
    pub ocr_text: Option<String>,
    pub ocr_mode: Option<OcrMode>,
    pub show_source: bool,
    pub status_line: Option<String>,
    // ---- 以下字段全部 `#[serde(default, skip_serializing_if = …)]`，
    // 默认值不出现在 JSON 里（旧样例与旧前端不受影响）；TS 镜像写成可缺省的键。前 5 个由面板视图在 `panel_pin` 时填写，
    // 后 2 个由 Rust 在 `pinned_init` 返回时填写（`panel_pin` 传入的值被忽略）
    /// 词典模式（翻译视图的 `dict_mode`，固定窗口里换风格 / 重新生成时沿用）
    #[serde(default, skip_serializing_if = "is_false")]
    pub dict_mode: bool,
    /// 本次结果实际使用的模型（固定窗口里重新生成 / 换风格时沿用；null = 按默认模型）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_ref: Option<ModelRef>,
    /// 结构化的状态行（代替解析 `status_line`；两者都给时固定窗口以它为准，`status_line` 留给旧前端）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<PinStatus>,
    /// OCR：截图缩略图的资源路径（= `OcrPayload.thumb_url`；固定窗口回退缩略图用，前端经 `resolveAssetUrl`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumb_url: Option<String>,
    /// OCR：原图尺寸（物理像素，= `OcrPayload.image_size`；固定窗口在图片加载前按它的宽高比占位）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_size: Option<PhysSize>,
    /// Rust 填写：「展开」⤢ 打开的固定窗口在首帧测量时用的卡片宽度（逻辑 px，= `placement::pinned_expand_rect` 的宽度，
    /// 已按工作区收窄）；普通固定为 null。取代前端的 localStorage 交接（`pinned/expand.ts`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expand_width: Option<u32>,
    /// Rust 填写：窗口当前是否置顶（新窗口取 state.json 的 `pinned_on_top_default`，之后随 `pinned_set_on_top` 更新）；
    /// 取代前端 localStorage 记住的初值
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_top: Option<bool>,
}

/// 固定窗口的结构化状态行（`PinState.status`）：与面板 StatusLine 的各段一一对应
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinStatus {
    /// 模型显示名（`StreamEvent::Start.model_label`）
    pub model_label: String,
    /// 请求耗时；命中缓存（`cached`）时为 null，不显示耗时
    pub elapsed_ms: Option<u32>,
    pub usage: Option<Usage>,
    /// 结果来自缓存（状态行附「来自缓存」）
    pub cached: bool,
    /// 已取消（状态行只显示「已取消」）
    pub cancelled: bool,
    /// 输出被截断的原因（状态行的弱提示）
    pub truncated: Option<TruncateReason>,
}

/// serde `skip_serializing_if` 用：false 不写出（签名由 serde 规定为 `&T`）
fn is_false(v: &bool) -> bool {
    !*v
}

/// 可从前端打开的窗口；同时是 settings / onboarding 窗口的路由：首次创建时写入 `Boot.route`，
/// 窗口已打开时由 `window:navigate` 事件（emit_to 该窗口）送达
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WindowTarget {
    Settings {
        section: Option<String>,
        /// 分组内的锚点（子视图 / 区块 id），只允许 desktop `app::sections::ANCHORS` 里登记的
        /// 「分组 → 锚点」组合（目前只有 `ocr` → `ocr_history`：OCR 面板 ⋯ 菜单的「识别历史」），拼错时
        /// `invalid_argument`。缺省 / null 表示分组顶部；省略时不出现在 JSON 里（TS：`anchor?: string | null`）
        #[serde(default, skip_serializing_if = "Option::is_none")]
        anchor: Option<String>,
    },
    Onboarding {
        step: Option<u8>,
    },
}

impl WindowTarget {
    /// 设置窗口的某个分组（无锚点）
    pub fn settings(section: Option<&str>) -> Self {
        Self::Settings {
            section: section.map(str::to_owned),
            anchor: None,
        }
    }
}

/// Overlay 模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayMode {
    Ocr,
    OcrTranslate,
}

/// Overlay 选区方式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlaySelectionKind {
    Drag,
    Window,
    Fullscreen,
}

/// `overlay_init` 的返回值。
///
/// `overlay_init` 没有前端参数：Rust 用调用方窗口的 label（`overlay-<n>`）反查本次会话绑定的显示器
/// （Windows 的 MonitorId 形如 `\\.\DISPLAY1`，不能放进 label）。该窗口当前没有会话时（预建、空闲中）返回 null，
/// 页面等待 `overlay:session` 事件后再调用一次。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverlayInit {
    pub session_id: u64,
    pub monitor: MonitorInfo,
    pub image_url: String,
    pub image_size: PhysSize,
    /// 本显示器内的窗口矩形（逻辑坐标，按 z 序，最上层在前）
    pub windows: Vec<Rect>,
    pub mode: OverlayMode,
    pub window_detect: bool,
    pub fly_animation: bool,
    pub reduced_motion: bool,
    /// true（macOS）：冻结帧已由原生 CALayer 显示，前端不画冻结帧，image_url 只给放大镜用
    pub frozen_native: bool,
    /// Windows / X11：先画的 1/2 降采样帧；macOS 为 null
    pub half_url: Option<String>,
    /// `frozen_native` 时放大镜与飞入按需拉取的小块：资源路径的前缀
    /// `capture/<session>/<monitor-hex>/region`，前端在后面接 `/<x>/<y>/<w>/<h>`（本帧的物理像素，必须在帧内；要更小的
    /// 输出时再接 `/<max_px>`——1..=4 MP 的面积上限，放大镜的低分辨率底图用）再交给 `resolveAssetUrl`，响应同样是带
    /// `x-wisp-width` / `x-wisp-height` 的原始 RGBA（大块按比例降采样到约 4 MP 或 `max_px`）。
    /// 页面不再为放大镜拉取整屏 `image_url`；不是原生冻结帧（页面自己画冻结帧）时为 null
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region_url: Option<String>,
}

/// `overlay:session` 事件载荷：复用预建的 Overlay（Windows 默认空闲预建一个隐藏的主显示器 Overlay）开始新会话时，
/// emit_to 该窗口。页面收到后重置自身状态并重新调用 `overlay_init`。新建的 Overlay 不发这个事件（页面加载后直接 init）。
///
/// 防竞态约定：页面先订阅本事件、再调用 `overlay_init`；init 返回 null 时等待事件；按 `session_id` 去重。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OverlaySessionStart {
    pub session_id: u64,
    pub monitor_id: MonitorId,
}

/// `overlay:end` 事件的原因
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayEndReason {
    /// 正常结束：本屏或另一块屏 commit 之后拆除（飞入 / 淡出结束、`overlay_done` 看门狗）
    Done,
    /// 取消：页面里的 Esc / 右键（`overlay_cancel`）、全局 Esc（Esc guard）、另一块屏的页面取消、托盘 / CLI 打断
    Cancelled,
    /// 会话期间再按截图快捷键（取消本次截图）
    Retrigger,
    /// 失败：页面没能初始化（原生冻结帧显示后迟迟没有 `overlay_init`）、没有一块屏显示出来
    Failed,
}

/// `overlay:end` 事件载荷：Rust 结束截图会话、拆除 Overlay **之前** emit_to 绑定了该会话的每个 `overlay-<n>`。
/// 页面按 `session_id` 核对后立即收尾（结束交互、释放冻结帧缓冲区与位图、去掉 data-on），**不再回调**
/// `overlay_cancel` / `overlay_done`。会话由页面自己结束（页面里的 Esc / 右键 / commit 后的 done）时同样会收到，重复收尾
/// 必须无害。Windows 复用的预建 Overlay 靠它在隐藏之后释放内存（否则冻结帧要留到下一次会话）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OverlaySessionEnd {
    pub session_id: u64,
    pub reason: OverlayEndReason,
}

/// `overlay_commit` 请求
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverlayCommit {
    pub session_id: u64,
    pub monitor_id: MonitorId,
    pub rect: Rect,
    pub kind: OverlaySelectionKind,
}

/// `overlay_commit` 的返回值
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverlayCommitAck {
    /// 面板的目标矩形（本显示器内逻辑坐标）；null 表示不飞入
    pub fly_to: Option<Rect>,
}

/// 设置快照（`settings_get` / `settings:changed`）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsSnapshot {
    pub settings: Settings,
    pub styles: Vec<Style>,
    pub runtime: RuntimeInfo,
}

/// 运行时信息
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeInfo {
    pub version: String,
    pub platform: PlatformInfo,
    pub has_key: BTreeMap<String, bool>,
    pub keychain: KeychainState,
    pub hotkeys: Vec<HotkeyStatus>,
    pub ext_triggers: ExtTriggerStatus,
    pub permissions: PermissionSet,
    pub clipboard_paused: bool,
    pub update: UpdateStatus,
    pub glossary_count: u32,
    /// 配置文件（settings / styles / glossary.json）最近一次读取失败的情况；设置窗口据此提示 `settings.invalid_file`
    pub settings_issue: Option<SettingsFileIssue>,
    /// 已删除的 Provider 中、Key 还没能从系统凭据存储删掉的个数（删除时用户拒绝了授权或钥匙串已锁定）。
    /// 这些 Key 在之后任何一次显式的钥匙串操作时重试清理；设置窗口据此提示
    pub orphan_keys: u32,
    /// 「开机自启」开着、却没能写进系统时的原因（i18n key）：
    /// `settings.general.autostart_issue.translocated`（从 App Translocation 运行，先把 Wisp 移到「应用程序」）/
    /// `settings.general.autostart_issue.failed`（写入失败）。通用页在开关下方显示原因与处理办法。
    /// 没有问题时省略（TS：`autostart_issue?: string | null`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autostart_issue: Option<String>,
}

/// 配置文件问题的种类
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsIssueKind {
    /// 热加载时 JSON 非法：保留旧值继续运行（文件原样保留，用户改正后自动恢复）
    InvalidJson,
    /// 启动时无法读取或解析：已备份为 `<name>.bak-<unix>` 并改用默认值
    ResetToDefault,
    /// 文件由更新版本的 Wisp 写出（`schema_version` 更高，用户降级了）：本版本不认识的设置不生效，但保存时原样保留，
    /// 文件改回当前版本后清除
    NewerVersion,
}

/// 配置文件问题（`RuntimeInfo.settings_issue`）
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SettingsFileIssue {
    pub kind: SettingsIssueKind,
    /// 出问题的文件名，例如 "settings.json"
    pub file: String,
    /// `reset_to_default` 时的备份文件完整路径；`invalid_json` 时为 null
    pub backup_path: Option<String>,
    /// 发现问题的时间（Unix 毫秒）
    pub at_ms: i64,
}

/// 钥匙串状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeychainState {
    Available,
    SessionOnly,
    Unavailable,
}

/// 解析后的外观（boot 脚本与 `appearance:changed`）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppearanceState {
    pub theme: ResolvedTheme,
    /// "#RRGGBB"；空串表示沿用 tokens.css 的默认强调色（系统强调色取不到时，与 `Boot.accent` 一致）
    pub accent: String,
    pub reduce_motion: bool,
    pub reduce_transparency: bool,
    pub material: ResolvedMaterial,
    pub font_size: u32,
    pub os: OsKind,
    /// "zh-CN" | "en-US"
    pub ui_language: String,
}

/// 解析后的主题
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedTheme {
    Light,
    Dark,
}

/// 解析后的材质
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedMaterial {
    Native,
    Solid,
}

/// 应用元信息（避免与 wisp-platform 的 AppInfo trait 同名）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppMeta {
    pub version: String,
    pub platform: PlatformInfo,
    pub config_dir: String,
    pub data_dir: String,
    pub log_dir: String,
    pub dev_mode: bool,
}

/// `open_path` 的目录种类
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathKind {
    Config,
    Data,
    Log,
}

/// 更新器状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateState {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available,
    Downloading,
    Ready,
    Error,
    Disabled,
}

/// 更新器状态详情
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct UpdateStatus {
    pub state: UpdateState,
    pub version: Option<String>,
    pub notes: Option<String>,
    pub progress: Option<f32>,
    pub error: Option<String>,
}

/// 提示词预览
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptPreview {
    pub system: String,
    pub user: String,
}

/// 剪贴板条目的「发送到」目标
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendTarget {
    Translate,
    Ocr,
}

/// `onboarding:hotkey` 事件载荷
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OnboardingHotkey {
    pub action: HotkeyAction,
}

/// `hotkey:test_received` 事件载荷
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HotkeyTestReceived {
    pub target: HotkeyTarget,
}

/// `trigger_action` 的来源（前端发起的动作）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionOrigin {
    /// 引导页「试一试」
    Onboarding,
    /// 剪贴板面板里把条目送去翻译
    Clipboard,
    /// 面板里的按钮，例如截图失败卡片的「重新截图」（`action = ocr` / `ocr_translate`，text 为 null）
    Panel,
    /// 设置 › OCR › 识别历史里的「重新翻译」（`action = translate_selection`，text = 该条识别结果）
    OcrHistory,
}

/// 面板帧模式：macOS = native（窗口即卡片，原生材质与逐帧弹簧）；Windows / Linux（以及 macOS 原生帧不可用时）
/// = css（透明窗口 + CSS 卡片 + 阴影边距）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameMode {
    Native,
    Css,
}

impl FrameMode {
    /// 序列化后的字符串（与 serde 一致）
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Css => "css",
        }
    }
}

/// 注入到每个 WebView 的 `window.__WISP_BOOT__`。Rust 在创建窗口时生成（`windows::boot_script`），
/// TS 镜像为 `types.ts` 的 `Boot`（`shared/ipc/boot.ts` 读取并补齐缺省值）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Boot {
    /// 窗口 label：panel / settings / onboarding / pinned-<id> / overlay-<n>
    pub label: String,
    pub os: OsKind,
    pub os_version: String,
    pub theme: ResolvedTheme,
    /// "#RRGGBB"；空串表示沿用 tokens.css 的默认强调色
    pub accent: String,
    pub reduce_motion: bool,
    pub reduce_transparency: bool,
    pub material: ResolvedMaterial,
    pub frame_mode: FrameMode,
    /// Css 帧模式下卡片四周留给阴影的透明边距（逻辑 px；Native 帧模式为 0）。唯一来源是 Rust（`windows::sizing`），
    /// 窗口尺寸、点击穿透的卡片矩形与 CSS 变量 `--shadow-margin` 都由它推出
    pub shadow_margin: f64,
    pub font_size: u32,
    /// "zh-CN" | "en-US"
    pub ui_language: String,
    /// 是否显示开发者信息（延迟埋点、FrameMonitor）= `settings.general.developer_mode || debug_build`。
    /// 这是创建窗口时的值；之后前端在 `settings:changed` 时用 `applyDeveloperMode` 原地更新
    pub dev: bool,
    /// debug 构建（`cfg!(debug_assertions)`）
    pub debug_build: bool,
    /// 初始路由：settings / onboarding 窗口首次创建时的目标（分组 / 引导步骤）；其他窗口为 null。
    /// 窗口已打开时的跳转走 `window:navigate` 事件
    pub route: Option<WindowTarget>,
    /// 面板冷唤出（空闲卸载 / 失效重建之后、页面还没加载时就有暂存的 prepare）时随初始化脚本送进新页面的那次 prepare：
    /// 前端在第一次渲染时直接当作第一条 `panel:prepare` 处理（同步测量 +
    /// `panel_ready`），不必等页面加载完成、`listen` 的往返与 Rust 的补发。其他窗口与预热创建的面板没有这个键
    /// （序列化时省略；TS 侧可缺省）。Rust 照常在页面加载后 emit 同一会话的 prepare 作兜底，前端按会话去重
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepare: Option<PanelPrepare>,
}

/// `panel_resize` 指定的窗口高度弹簧：只影响 Native 帧模式的原生逐帧弹簧（Css 帧的
/// 窗口阶梯不变，卡片高度在前端用同名的 Motion 预设）。缺省（`None`）= `Gentle`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResizeSpring {
    /// `spring.gentle`：内容变化（流式输出、视图切换）
    #[default]
    Gentle,
    /// `spring.snappy`：浮层（菜单）预留引起的长高与释放预留后的回收——菜单按下就出现，卡片尽快让出位置
    Snappy,
}

/// Rust → 前端的事件名。Rust 侧一律引用这些常量，不手写字符串；
/// 与 `apps/desktop/src/shared/ipc/events.ts` 的 `EVENT_NAMES` 一一对应（wisp-types 的 golden 测试比对）。
pub mod event {
    /// `PanelPrepare` → panel
    pub const PANEL_PREPARE: &str = "panel:prepare";
    /// `PanelHideRequest` → panel
    pub const PANEL_HIDE_REQUEST: &str = "panel:hide_request";
    /// `PanelFocusChanged` → panel
    pub const PANEL_FOCUS_CHANGED: &str = "panel:focus_changed";
    /// `LatencyMarks` → panel（仅开发者模式）
    pub const PANEL_LATENCY: &str = "panel:latency";
    /// `ClipboardChanged` → panel
    pub const CLIPBOARD_CHANGED: &str = "clipboard:changed";
    /// `SettingsSnapshot` → 全部
    pub const SETTINGS_CHANGED: &str = "settings:changed";
    /// `AppearanceState` → 全部
    pub const APPEARANCE_CHANGED: &str = "appearance:changed";
    /// `PermissionSet` → settings、onboarding、panel
    pub const PERMISSIONS_CHANGED: &str = "permissions:changed";
    /// `Vec<HotkeyStatus>` → settings、onboarding
    pub const HOTKEYS_CHANGED: &str = "hotkeys:changed";
    /// `HotkeyTestReceived` → settings
    pub const HOTKEY_TEST_RECEIVED: &str = "hotkey:test_received";
    /// `UpdateStatus` → settings
    pub const UPDATE_STATUS: &str = "update:status";
    /// `OnboardingHotkey` → onboarding
    pub const ONBOARDING_HOTKEY: &str = "onboarding:hotkey";
    /// `OverlaySessionStart` → 被复用的预建 overlay-<n>
    pub const OVERLAY_SESSION: &str = "overlay:session";
    /// `WindowTarget` → 已打开的 settings / onboarding（深链跳转）
    pub const WINDOW_NAVIGATE: &str = "window:navigate";
    /// `()`（null）→ settings / onboarding：用户点了原生关闭按钮（交通灯 / 标题栏 ×）。前端提交延迟写入、等在途命令返回、
    /// 放弃草稿 Key 之后自己 `window.close()`；1.5 s 内没有关就由 Rust 照样关闭
    pub const WINDOW_CLOSE_REQUESTED: &str = "window:close_requested";
    /// `OverlaySessionEnd` → 绑定了该会话的每个 overlay-<n>：Rust 结束截图会话、拆除 Overlay 之前
    pub const OVERLAY_END: &str = "overlay:end";
    /// `OcrHistoryChanged` → settings：OCR 历史新增 / 删除 / 裁剪 / 清空
    pub const OCR_HISTORY_CHANGED: &str = "ocr_history:changed";

    /// 全部事件名（顺序与 events.ts 的 `EVENT_NAMES` 一致）
    pub const ALL: [&str; 17] = [
        PANEL_PREPARE,
        PANEL_HIDE_REQUEST,
        PANEL_FOCUS_CHANGED,
        PANEL_LATENCY,
        CLIPBOARD_CHANGED,
        SETTINGS_CHANGED,
        APPEARANCE_CHANGED,
        PERMISSIONS_CHANGED,
        HOTKEYS_CHANGED,
        HOTKEY_TEST_RECEIVED,
        UPDATE_STATUS,
        ONBOARDING_HOTKEY,
        OVERLAY_SESSION,
        WINDOW_NAVIGATE,
        WINDOW_CLOSE_REQUESTED,
        OVERLAY_END,
        OCR_HISTORY_CHANGED,
    ];
}
