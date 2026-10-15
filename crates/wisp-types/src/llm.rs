//! LLM 相关类型：Provider、模型、能力缓存、流式事件与错误。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::lang::Lang;

/// 请求 id：`"r-<自增>"`，全进程唯一
pub type RequestId = String;

/// 四种协议
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Protocol {
    #[serde(rename = "openai_chat")]
    OpenAiChat,
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    #[serde(rename = "anthropic_messages")]
    AnthropicMessages,
    #[serde(rename = "gemini")]
    Gemini,
}

/// 关闭思考的方式（各厂商私有参数）。DeepSeek / Glm / MiniMax 生成的 JSON 相同
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
pub enum ThinkingOff {
    #[default]
    #[serde(rename = "none")]
    None,
    #[serde(rename = "deepseek")]
    DeepSeek,
    #[serde(rename = "qwen")]
    Qwen,
    #[serde(rename = "glm")]
    Glm,
    #[serde(rename = "kimi")]
    Kimi,
    #[serde(rename = "minimax")]
    MiniMax,
    #[serde(rename = "openai_effort")]
    OpenAiEffort,
    #[serde(rename = "anthropic_effort")]
    AnthropicEffort,
    #[serde(rename = "gemini_level")]
    GeminiLevel,
}

/// 最大输出 token 的字段名
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
    MaxCompletionTokens,
    MaxTokens,
}

/// 图片细节档位
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageDetail {
    Low,
    #[default]
    High,
    Original,
    Auto,
}

/// Provider 的兼容性开关
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Quirks {
    /// 默认 none
    pub thinking_off: ThinkingOff,
    /// 默认 false；只对带图请求生效
    pub merge_system_into_user: bool,
    /// 默认 true；只对 openai_chat 生效
    pub strip_think_tags: bool,
    /// null = 按协议默认
    pub max_tokens_field: Option<MaxTokensField>,
    /// null = 按 host 推导：api.anthropic.com 只发 x-api-key，其他 host 同时发 x-api-key 和 Bearer
    pub dual_auth_header: Option<bool>,
    /// 默认 false
    pub gemini_key_in_query: bool,
}

impl Default for Quirks {
    fn default() -> Self {
        Self {
            thinking_off: ThinkingOff::None,
            merge_system_into_user: false,
            strip_think_tags: true,
            max_tokens_field: None,
            dual_auth_header: None,
            gemini_key_in_query: false,
        }
    }
}

/// 视觉能力
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Vision {
    Yes,
    No,
    #[default]
    Unknown,
}

/// 视觉能力判定来源，优先级：user > runtime > capability > keyword
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VisionSource {
    Capability,
    Runtime,
    Keyword,
    User,
}

/// 能力缓存，null 表示未知
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelCaps {
    pub temperature: Option<bool>,
    pub max_tokens_field: Option<MaxTokensField>,
    pub thinking_param: Option<bool>,
    pub effort_param: Option<bool>,
    pub stream_options: Option<bool>,
    pub include_obfuscation: Option<bool>,
    pub anthropic_thinking_disabled: Option<bool>,
    pub anthropic_efforts: Option<Vec<String>>,
}

fn default_true() -> bool {
    true
}

/// 推理档位：OpenAI 系推理模型在 `thinking_off = openai_effort` 时发送的 `reasoning_effort` /
/// Responses `reasoning.effort`。缺省（`ModelEntry.reasoning_effort = null`）为 low；可在模型的高级设置中改成
/// minimal 或 none，被上游拒绝时照常降级（去掉这个参数）。只影响 OpenAI 两种协议，Anthropic 的 effort 由用途决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
}

impl ReasoningEffort {
    /// 请求体里的取值（与 serde 一致）
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
        }
    }
}

/// `ModelEntry.reasoning_effort` 的宽松读取：取值非法（手改 settings.json 写错）时当作未设置（= low），
/// 不让整个模型条目被丢弃
fn lenient_effort<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ReasoningEffort>, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(v).ok())
}

/// 模型条目
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    #[serde(default)]
    pub label: Option<String>,
    /// 缺省为 unknown（手改 settings.json 漏写时不让整个文件失败）
    #[serde(default)]
    pub vision: Vision,
    #[serde(default)]
    pub vision_source: Option<VisionSource>,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 上游列表里已经没有这个模型（标记出来，不自动删除）
    #[serde(default)]
    pub missing_upstream: bool,
    #[serde(default)]
    pub deprecated: bool,
    #[serde(default)]
    pub context_tokens: Option<u32>,
    #[serde(default)]
    pub caps: ModelCaps,
    /// 用户在模型高级设置里选的推理档位；null = 默认 low
    #[serde(default, deserialize_with = "lenient_effort")]
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl ModelEntry {
    /// 以默认字段构造一个模型条目（视觉未知、启用）
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: None,
            vision: Vision::Unknown,
            vision_source: None,
            reasoning: false,
            enabled: true,
            missing_upstream: false,
            deprecated: false,
            context_tokens: None,
            caps: ModelCaps::default(),
            reasoning_effort: None,
        }
    }
}

/// 三个超时（毫秒）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct Timeouts {
    /// 10000
    pub connect_ms: u32,
    /// 30000
    pub first_token_ms: u32,
    /// 60000
    pub idle_ms: u32,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect_ms: 10_000,
            first_token_ms: 30_000,
            idle_ms: 60_000,
        }
    }
}

/// LLM 服务商配置
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provider {
    /// 不可变的 slug，例如 "openai-main"；钥匙串条目为 service="wisp", user=id
    pub id: String,
    /// 缺省为空串（界面退回显示 id）
    #[serde(default)]
    pub name: String,
    pub protocol: Protocol,
    /// 用户输入原样保存（包括末尾的 #）
    pub base_url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub quirks: Quirks,
    /// 逐条容错：某个模型条目非法时只丢弃这一条（`crate::lenient::vec`），不连累整个 Provider
    #[serde(default, deserialize_with = "lenient_models")]
    pub models: Vec<ModelEntry>,
    #[serde(default)]
    pub timeouts: Timeouts,
    #[serde(default)]
    pub template_id: Option<String>,
}

/// `Provider.models` 的逐条容错（问题路径记为 `…models[i]`）
fn lenient_models<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<ModelEntry>, D::Error> {
    crate::lenient::named("models", d, crate::lenient::vec)
}

/// 指向某个 Provider 下的某个模型
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider_id: String,
    pub model_id: String,
}

crate::lenient::lenient_struct! {
    /// 各任务的默认模型（settings.json 的一部分，宽松反序列化：某一项非法只让该项退回 null）
    #[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
    pub struct Defaults {
        pub translate: Option<ModelRef>,
        pub ocr: Option<ModelRef>,
    }
}

/// 用量（结束时汇总）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub reasoning_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
}

/// 错误码（前端按 code 取 i18n 文案）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Auth,
    NotFound,
    NoVision,
    ImageTooLarge,
    ParamUnsupported,
    TooLong,
    Quota,
    RateLimited,
    Upstream,
    ContentFilter,
    Network,
    Timeout,
    Cancelled,
    Protocol,
    NotConfigured,
    Keychain,
    Internal,
}

/// 错误子码
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorSubcode {
    Region,
    /// **保留，不产生**（wisp-llm 与编排器都不发它，前端不需要为它分支）。「模型思考耗尽了
    /// 输出额度」由前端判定：`Done.truncated == max_tokens` 且本次没有收到任何非空白的 Delta，显示在底部状态行
    ThinkingExhausted,
    ConnectionLost,
    HtmlBody,
    VersionHint,
}

/// 归一化后的 LLM 错误
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmError {
    pub code: ErrorCode,
    pub subcode: Option<ErrorSubcode>,
    /// 服务端原文（≤300 字符）或本地英文简述；界面文案由前端按 code 取 i18n
    pub message: String,
    pub retryable: bool,
    /// 前端 i18n key，形如 `errors.hint.<name>`，例如 "errors.hint.try_v1"
    pub hint: Option<String>,
    pub status: Option<u16>,
    /// 只到 host + path，不含 query
    pub url: Option<String>,
    pub retry_after_s: Option<u32>,
    pub upstream_request_id: Option<String>,
}

impl LlmError {
    /// 只带 code 与 message 的错误（其余字段为空）
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            subcode: None,
            message: message.into(),
            retryable: false,
            hint: None,
            status: None,
            url: None,
            retry_after_s: None,
            upstream_request_id: None,
        }
    }
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for LlmError {}

/// 截断原因
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TruncateReason {
    MaxTokens,
}

/// 流中的提示事件类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeKind {
    ParamDegraded,
    Retried,
    CacheHit,
}

/// refine 两步法的阶段
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamPhase {
    Reviewing,
    Improving,
}

/// 流式事件。合法序列：Start →（Meta | Phase | Notice | Delta）* → 恰好一个 Done 或 Error；
/// Meta 可以出现在 Start 之前（缓存命中时立即告知方向）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    Start {
        request_id: RequestId,
        provider_id: String,
        model_id: String,
        model_label: String,
        upstream_id: Option<String>,
    },
    Meta {
        detected_source_lang: Option<Lang>,
        target_lang: Option<Lang>,
        dict_mode: Option<bool>,
        low_confidence: Option<bool>,
    },
    Phase {
        phase: StreamPhase,
    },
    /// 经过后处理和 16 ms 合并之后的增量文本，前端只做追加
    Delta {
        text: String,
    },
    Notice {
        kind: NoticeKind,
    },
    Done {
        elapsed_ms: u32,
        usage: Option<Usage>,
        truncated: Option<TruncateReason>,
        cached: bool,
        no_text: bool,
    },
    /// 内部标签的 newtype 变体：{"type":"error", "code":…, …}
    Error(LlmError),
}

impl StreamEvent {
    /// 是否为终止事件（Done / Error）
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done { .. } | Self::Error(_))
    }
}

/// Provider 模板分组
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemplateGroup {
    Openai,
    Anthropic,
    Gemini,
    Other,
    Local,
}

/// 区域变体（例如国内 / 国际站）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionVariant {
    /// 前端 i18n key，形如 `settings.providers.region.<name>`
    pub label_key: String,
    pub base_url: String,
}

/// 厂商模板
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderTemplate {
    pub id: String,
    /// 前端 i18n key，形如 `settings.providers.template.<id>`
    pub name_key: String,
    pub group: TemplateGroup,
    pub provider: Provider,
    pub key_required: bool,
    pub region_variants: Vec<RegionVariant>,
}

/// 最终请求 URL 的预览
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UrlPreview {
    pub method: String,
    pub url: String,
    pub version_appended: bool,
    pub forced: bool,
    /// 输入需要提醒的地方（i18n key，`settings.providers.url_warning.*`）——去掉了末尾的接口路径、
    /// 查询串会附加到请求末尾、中间的 `#` 片段被丢弃。没有时省略（TS：`warning?: string | null`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// 单项探测结果
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeResult {
    pub ok: bool,
    pub status: Option<u16>,
    pub elapsed_ms: u32,
    pub url: String,
    pub error: Option<LlmError>,
}

/// 测试连接的总体结论
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestVerdict {
    Ok,
    /// /models 失败但流式探针成功（标黄）
    Partial,
    Failed,
}

/// 测试连接报告
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderTestReport {
    pub models: ProbeResult,
    pub stream: ProbeResult,
    pub overall: TestVerdict,
    pub model_used: Option<String>,
    pub fetched_models: Vec<ModelEntry>,
    /// 测试本身之外需要告诉用户的情况（i18n key）。目前只有
    /// `settings.providers.test_notice.keychain_unreadable`——系统凭据存储读不出来，本次没带 Key 测试（Provider 不需要 Key、
    /// 或没有存过 Key）。没有时省略（TS：`notice?: string | null`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}
