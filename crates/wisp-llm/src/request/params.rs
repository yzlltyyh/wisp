//! temperature、最大输出字段、thinking / effort 的发送规则，以及推理模型判定。
//!
//! 每次构造请求体时按「能力缓存 + quirks + 模型」重新计算一次；降级只改能力缓存，再重新计算。

use wisp_types::{ImageDetail, MaxTokensField, ModelCaps, ModelEntry, Protocol, Quirks, ReasoningEffort, ThinkingOff};

use crate::client::Purpose;
use crate::vision::{bare_id, is_reasoning_id};

/// 推理模型、或 Anthropic 没有关闭 thinking 时，最大输出的下限
pub(crate) const REASONING_MAX_TOKENS_FLOOR: u32 = 1024;

/// 关闭 / 降低思考的字段（DeepSeek / Glm / MiniMax 生成的 JSON 相同；火山用 glm 变体，硅基用 qwen 变体）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Thinking {
    None,
    /// `thinking: {type: "disabled"}`（DeepSeek / 智谱 / 火山 / MiniMax；Anthropic 只在能力缓存确认支持时发）
    Disabled,
    /// `enable_thinking: false`（通义 / 硅基流动）
    EnableFalse,
    /// Kimi：Chat 上 `reasoning_effort: "low"`，Responses 上 `reasoning: {effort: "low"}`
    KimiLow,
    /// Gemini 3+：`thinkingConfig: {thinkingLevel: "LOW"}`
    GeminiLevelLow,
    /// Gemini 2.5 Flash（含 lite）：`thinkingConfig: {thinkingBudget: 0}`
    GeminiBudgetZero,
    /// `chat_template_kwargs: {enable_thinking: false}`（llama-server 等按 chat template 关思考的服务）
    TemplateKwargsOff,
}

/// 一次请求的参数决定
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Plan {
    /// Some 时发送（已裁剪到 0–2 并保留三位小数）
    pub temperature: Option<f64>,
    pub max_tokens: u32,
    /// 只对 OpenAI Chat 有意义
    pub max_field: MaxTokensField,
    /// OpenAI `reasoning_effort` / Responses `reasoning.effort` / Anthropic `output_config.effort`
    pub effort: Option<&'static str>,
    pub thinking: Thinking,
    /// OpenAI Chat 的 `stream_options.include_usage`
    pub stream_options: bool,
    /// `stream_options.include_obfuscation: false`（只对 api.openai.com）
    pub include_obfuscation: bool,
    /// 不发 system，把它合并进 user（只对带图的 OpenAI 兼容请求）
    pub merge_system: bool,
    /// OpenAI 两种协议图片的 `detail`：官方端点按设置（`ocr.image_detail`），兼容端点不认识的 original 退回 high
    pub image_detail: &'static str,
}

/// 计算参数决定的输入
pub(crate) struct PlanInput<'a> {
    pub protocol: Protocol,
    pub quirks: &'a Quirks,
    pub model: &'a ModelEntry,
    pub caps: &'a ModelCaps,
    pub purpose: Purpose,
    pub temperature: Option<f32>,
    pub max_output_tokens: u32,
    /// 请求 URL 的 host（小写）
    pub host: &'a str,
    pub has_image: bool,
    /// 调用方要求的图片细节档位（`UserContent::Image.detail`；纯文本请求忽略）
    pub image_detail: ImageDetail,
}

/// 官方 OpenAI 端点（`detail: original` 只对它开放）
const OPENAI_OFFICIAL_HOST: &str = "api.openai.com";

/// 图片 `detail` 的取值：low / high / auto 各端点通用；original 只有官方端点接受，兼容端点退回 high
fn image_detail(detail: ImageDetail, host: &str) -> &'static str {
    match detail {
        ImageDetail::Low => "low",
        ImageDetail::High => "high",
        ImageDetail::Auto => "auto",
        ImageDetail::Original if host == OPENAI_OFFICIAL_HOST => "original",
        ImageDetail::Original => "high",
    }
}

/// 是否按推理模型处理：手动 / 能力字段标记的 reasoning，或 id 匹配 `^(o[0-9]|gpt-5|gpt-6)`
pub(crate) fn is_reasoning(model: &ModelEntry) -> bool {
    model.reasoning || is_reasoning_id(&model.id)
}

pub(crate) fn plan(i: &PlanInput<'_>) -> Plan {
    let reasoning = is_reasoning(i.model);
    let openai = matches!(i.protocol, Protocol::OpenAiChat | Protocol::OpenAiResponses);
    let thinking_allowed = i.caps.thinking_param != Some(false);
    let effort_allowed = i.caps.effort_param != Some(false);

    let thinking = if !thinking_allowed {
        Thinking::None
    } else {
        match i.protocol {
            Protocol::OpenAiChat | Protocol::OpenAiResponses => match i.quirks.thinking_off {
                ThinkingOff::DeepSeek | ThinkingOff::Glm | ThinkingOff::MiniMax => Thinking::Disabled,
                ThinkingOff::Qwen => Thinking::EnableFalse,
                ThinkingOff::Kimi => Thinking::KimiLow,
                ThinkingOff::ChatTemplateKwargs => Thinking::TemplateKwargsOff,
                ThinkingOff::None
                | ThinkingOff::OpenAiEffort
                | ThinkingOff::AnthropicEffort
                | ThinkingOff::GeminiLevel => Thinking::None,
            },
            // 只有 /v1/models 报告 thinking.types.disabled.supported == true 时才发，其他情况（包括中转站）一律不发
            Protocol::AnthropicMessages => {
                if i.caps.anthropic_thinking_disabled == Some(true) {
                    Thinking::Disabled
                } else {
                    Thinking::None
                }
            }
            Protocol::Gemini => {
                if i.quirks.thinking_off == ThinkingOff::GeminiLevel {
                    gemini_thinking(&i.model.id)
                } else {
                    Thinking::None
                }
            }
            // 不是 LLM 服务：请求在构造阶段已被拒绝
            Protocol::PaddleOcr => Thinking::None,
        }
    };

    let effort = if !effort_allowed {
        None
    } else {
        match i.protocol {
            // 推理模型 + openai_effort 时发 low（OCR 也用 low）；模型高级设置里可改成 minimal / none
            Protocol::OpenAiChat | Protocol::OpenAiResponses => (i.quirks.thinking_off == ThinkingOff::OpenAiEffort
                && reasoning)
                .then(|| i.model.reasoning_effort.map_or("low", ReasoningEffort::as_str)),
            // 翻译用 low，OCR 用 medium；能力缓存列出了可用档位时按它挑
            Protocol::AnthropicMessages => {
                if i.quirks.thinking_off == ThinkingOff::AnthropicEffort {
                    let wanted = if i.purpose == Purpose::Ocr { "medium" } else { "low" };
                    pick_effort(wanted, i.caps.anthropic_efforts.as_deref())
                } else {
                    None
                }
            }
            Protocol::Gemini | Protocol::PaddleOcr => None,
        }
    };

    let temperature = match i.protocol {
        // Claude 5 起 temperature 已废弃，非 1.0 返回 400：永远不发
        Protocol::AnthropicMessages => None,
        // 推理模型只接受默认值
        Protocol::OpenAiChat | Protocol::OpenAiResponses if reasoning => None,
        _ => i.temperature,
    }
    .filter(|_| i.caps.temperature != Some(false))
    .filter(|t| t.is_finite())
    .map(|t| (f64::from(t.clamp(0.0, 2.0)) * 1000.0).round() / 1000.0);

    let anthropic_thinking_on = i.protocol == Protocol::AnthropicMessages && thinking != Thinking::Disabled;
    let mut max_tokens = i.max_output_tokens.max(1);
    if i.purpose != Purpose::Probe && (reasoning || anthropic_thinking_on) {
        max_tokens = max_tokens.max(REASONING_MAX_TOKENS_FLOOR);
    }

    let max_field = i
        .caps
        .max_tokens_field
        .or(i.quirks.max_tokens_field)
        .unwrap_or(MaxTokensField::MaxCompletionTokens);

    let chat = i.protocol == Protocol::OpenAiChat;
    let stream_options = chat && i.caps.stream_options != Some(false);
    let include_obfuscation =
        stream_options && i.host == OPENAI_OFFICIAL_HOST && i.caps.include_obfuscation != Some(false);

    Plan {
        temperature,
        max_tokens,
        max_field,
        effort,
        thinking,
        stream_options,
        include_obfuscation,
        merge_system: openai && i.has_image && i.quirks.merge_system_into_user,
        image_detail: image_detail(i.image_detail, i.host),
    }
}

/// Gemini 的思考档位按代际：3–9 或两位数代际（含 omni）用 thinkingLevel LOW；
/// 2.5 Flash（含 lite）用 thinkingBudget 0；2.5 Pro 等其余不发
pub(crate) fn gemini_thinking(model_id: &str) -> Thinking {
    let id = bare_id(model_id);
    let Some(rest) = id.strip_prefix("gemini-") else {
        return Thinking::None;
    };
    if rest.starts_with("omni") {
        return Thinking::GeminiLevelLow;
    }
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    match digits.parse::<u32>() {
        Ok(major) if major >= 3 => Thinking::GeminiLevelLow,
        _ if rest.starts_with("2.5-flash") => Thinking::GeminiBudgetZero,
        _ => Thinking::None,
    }
}

/// 在 Anthropic 报告的可用档位里挑：想要的优先，否则取最低的可用档；列表为空表示不支持 effort
fn pick_effort(wanted: &'static str, available: Option<&[String]>) -> Option<&'static str> {
    const ORDER: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
    let Some(list) = available else {
        return Some(wanted);
    };
    let has = |e: &str| list.iter().any(|x| x.eq_ignore_ascii_case(e));
    if has(wanted) {
        return Some(wanted);
    }
    ORDER.into_iter().find(|e| has(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(
        protocol: Protocol,
        quirks: &'a Quirks,
        model: &'a ModelEntry,
        caps: &'a ModelCaps,
        host: &'a str,
    ) -> PlanInput<'a> {
        PlanInput {
            protocol,
            quirks,
            model,
            caps,
            purpose: Purpose::Translate,
            temperature: Some(0.3),
            max_output_tokens: 556,
            host,
            has_image: false,
            image_detail: ImageDetail::High,
        }
    }

    fn openai_quirks() -> Quirks {
        Quirks {
            thinking_off: ThinkingOff::OpenAiEffort,
            strip_think_tags: false,
            max_tokens_field: Some(MaxTokensField::MaxCompletionTokens),
            ..Quirks::default()
        }
    }

    #[test]
    fn reasoning_model_on_openai() {
        let q = openai_quirks();
        let m = ModelEntry::new("gpt-5.4-mini");
        let caps = ModelCaps::default();
        let p = plan(&input(Protocol::OpenAiChat, &q, &m, &caps, "api.openai.com"));
        assert_eq!(p.temperature, None);
        assert_eq!(p.effort, Some("low"));
        assert_eq!(p.max_field, MaxTokensField::MaxCompletionTokens);
        assert_eq!(p.max_tokens, REASONING_MAX_TOKENS_FLOOR);
        assert!(p.stream_options && p.include_obfuscation);
    }

    /// 模型高级设置里的推理档位原样发送，缺省 low；非推理模型不发
    #[test]
    fn reasoning_effort_follows_the_model_setting() {
        let q = openai_quirks();
        let caps = ModelCaps::default();
        for (setting, sent) in [
            (None, Some("low")),
            (Some(ReasoningEffort::Minimal), Some("minimal")),
            (Some(ReasoningEffort::None), Some("none")),
            (Some(ReasoningEffort::Low), Some("low")),
        ] {
            let mut m = ModelEntry::new("gpt-5.4-mini");
            m.reasoning_effort = setting;
            for protocol in [Protocol::OpenAiChat, Protocol::OpenAiResponses] {
                assert_eq!(plan(&input(protocol, &q, &m, &caps, "api.openai.com")).effort, sent);
            }
        }
        let mut m = ModelEntry::new("gpt-4.1");
        m.reasoning_effort = Some(ReasoningEffort::Minimal);
        assert_eq!(
            plan(&input(Protocol::OpenAiChat, &q, &m, &caps, "api.openai.com")).effort,
            None
        );
    }

    /// original 只发给官方端点，兼容端点退回 high；其余档位原样
    #[test]
    fn image_detail_original_only_on_the_official_endpoint() {
        let q = openai_quirks();
        let m = ModelEntry::new("gpt-4.1");
        let caps = ModelCaps::default();
        let detail = |d: ImageDetail, host: &str| {
            let mut i = input(Protocol::OpenAiChat, &q, &m, &caps, host);
            i.has_image = true;
            i.image_detail = d;
            plan(&i).image_detail
        };
        assert_eq!(detail(ImageDetail::Original, "api.openai.com"), "original");
        assert_eq!(detail(ImageDetail::Original, "dashscope.aliyuncs.com"), "high");
        assert_eq!(detail(ImageDetail::Low, "relay.example.com"), "low");
        assert_eq!(detail(ImageDetail::Auto, "api.openai.com"), "auto");
        assert_eq!(detail(ImageDetail::High, "api.openai.com"), "high");
    }

    #[test]
    fn non_reasoning_model_on_openai() {
        let q = openai_quirks();
        let m = ModelEntry::new("gpt-4.1");
        let caps = ModelCaps::default();
        let p = plan(&input(Protocol::OpenAiChat, &q, &m, &caps, "relay.example.com"));
        assert_eq!(p.temperature, Some(0.3));
        assert_eq!(p.effort, None);
        assert_eq!(p.max_tokens, 556);
        assert!(p.stream_options && !p.include_obfuscation);
    }

    #[test]
    fn caps_override_everything() {
        let q = Quirks {
            thinking_off: ThinkingOff::DeepSeek,
            max_tokens_field: Some(MaxTokensField::MaxTokens),
            ..Quirks::default()
        };
        let m = ModelEntry::new("deepseek-flash");
        let caps = ModelCaps {
            temperature: Some(false),
            max_tokens_field: Some(MaxTokensField::MaxCompletionTokens),
            thinking_param: Some(false),
            stream_options: Some(false),
            ..ModelCaps::default()
        };
        let p = plan(&input(Protocol::OpenAiChat, &q, &m, &caps, "api.deepseek.com"));
        assert_eq!(p.temperature, None);
        assert_eq!(p.thinking, Thinking::None);
        assert_eq!(p.max_field, MaxTokensField::MaxCompletionTokens);
        assert!(!p.stream_options);
    }

    #[test]
    fn anthropic_rules() {
        let q = Quirks {
            thinking_off: ThinkingOff::AnthropicEffort,
            ..Quirks::default()
        };
        let m = ModelEntry::new("claude-sonnet-5-5");
        let mut caps = ModelCaps::default();
        let mut i = input(Protocol::AnthropicMessages, &q, &m, &caps, "api.anthropic.com");
        i.purpose = Purpose::Ocr;
        let p = plan(&i);
        assert_eq!(
            (p.temperature, p.effort, p.thinking),
            (None, Some("medium"), Thinking::None)
        );
        // thinking 没关时下限 1024
        assert_eq!(p.max_tokens, REASONING_MAX_TOKENS_FLOOR);

        caps.anthropic_thinking_disabled = Some(true);
        caps.anthropic_efforts = Some(vec!["medium".to_owned(), "high".to_owned()]);
        let p = plan(&input(Protocol::AnthropicMessages, &q, &m, &caps, "api.anthropic.com"));
        assert_eq!(
            (p.effort, p.thinking, p.max_tokens),
            (Some("medium"), Thinking::Disabled, 556)
        );

        caps.anthropic_efforts = Some(Vec::new());
        let p = plan(&input(Protocol::AnthropicMessages, &q, &m, &caps, "api.anthropic.com"));
        assert_eq!(p.effort, None);
    }

    #[test]
    fn gemini_generations() {
        assert_eq!(gemini_thinking("gemini-3-flash-preview"), Thinking::GeminiLevelLow);
        assert_eq!(gemini_thinking("models/gemini-3.8-flash"), Thinking::GeminiLevelLow);
        assert_eq!(gemini_thinking("gemini-12-ultra"), Thinking::GeminiLevelLow);
        assert_eq!(gemini_thinking("gemini-omni-flash"), Thinking::GeminiLevelLow);
        assert_eq!(gemini_thinking("gemini-2.5-flash"), Thinking::GeminiBudgetZero);
        assert_eq!(gemini_thinking("gemini-2.5-flash-lite"), Thinking::GeminiBudgetZero);
        assert_eq!(gemini_thinking("gemini-2.5-pro"), Thinking::None);
        assert_eq!(gemini_thinking("gemini-2.0-flash"), Thinking::None);
        assert_eq!(gemini_thinking("gemma-3"), Thinking::None);
    }

    #[test]
    fn temperature_matrix() {
        // 风格 × 协议 × 推理模型
        let q = Quirks {
            thinking_off: ThinkingOff::GeminiLevel,
            ..Quirks::default()
        };
        let caps = ModelCaps::default();
        let cases = [
            (Protocol::OpenAiChat, "gpt-4.1", false, Some(0.6)),
            (Protocol::OpenAiChat, "gpt-4.1", true, None),
            (Protocol::OpenAiResponses, "o3", false, None),
            (Protocol::OpenAiResponses, "gpt-4o", false, Some(0.6)),
            (Protocol::AnthropicMessages, "claude-haiku-4-5", false, None),
            (Protocol::Gemini, "gemini-2.5-pro", true, Some(0.6)),
        ];
        for (protocol, id, reasoning, expected) in cases {
            let mut m = ModelEntry::new(id);
            m.reasoning = reasoning;
            let mut i = input(protocol, &q, &m, &caps, "h");
            i.temperature = Some(0.6);
            assert_eq!(plan(&i).temperature, expected, "{protocol:?} {id} {reasoning}");
        }
    }

    #[test]
    fn temperature_is_clamped_and_rounded() {
        let q = Quirks::default();
        let m = ModelEntry::new("gpt-4.1");
        let caps = ModelCaps::default();
        let mut i = input(Protocol::OpenAiChat, &q, &m, &caps, "h");
        i.temperature = Some(0.2);
        assert_eq!(plan(&i).temperature, Some(0.2));
        i.temperature = Some(3.5);
        assert_eq!(plan(&i).temperature, Some(2.0));
        i.temperature = Some(f32::NAN);
        assert_eq!(plan(&i).temperature, None);
    }

    #[test]
    fn probe_keeps_tiny_max_tokens() {
        let q = openai_quirks();
        let m = ModelEntry::new("o3");
        let caps = ModelCaps::default();
        let mut i = input(Protocol::OpenAiChat, &q, &m, &caps, "h");
        i.purpose = Purpose::Probe;
        i.max_output_tokens = 1;
        assert_eq!(plan(&i).max_tokens, 1);
    }

    #[test]
    fn merge_only_for_openai_image_requests() {
        let q = Quirks {
            merge_system_into_user: true,
            ..Quirks::default()
        };
        let m = ModelEntry::new("qwen3-vl-plus");
        let caps = ModelCaps::default();
        let mut i = input(Protocol::OpenAiChat, &q, &m, &caps, "h");
        assert!(!plan(&i).merge_system);
        i.has_image = true;
        assert!(plan(&i).merge_system);
        i.protocol = Protocol::Gemini;
        assert!(!plan(&i).merge_system);
    }
}
