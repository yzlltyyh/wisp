//! 17 个服务模板及其 quirks / 超时预填（顺序即设置页里的显示顺序）：16 个 LLM 服务，以及 PaddleOCR 云端文字识别
//! （`paddle_ocr` 协议，预置模型、不支持获取模型列表）。本地模板（Ollama / LM Studio）的 `first_token_ms` 和 `idle_ms`
//! 都是 120000；所有 Provider 的 Key 都是可选的。
//!
//! `key_required` 只是界面提示（云端服务通常需要 Key），保存与请求时 Key 一律可选：为空就不发鉴权头。
//! `name_key` / `label_key` 是前端 settings.json 命名空间的 i18n key（文案在前端 locales 里），这里写成完整字面量，
//! 便于 `scripts/check-i18n.mjs` 扫描。

use std::collections::BTreeMap;

use wisp_types::{
    MaxTokensField, ModelEntry, Protocol, Provider, ProviderTemplate, Quirks, RegionVariant, TemplateGroup,
    ThinkingOff, Timeouts,
};

/// 本地模型冷启动可能超过 30 s
const LOCAL_TIMEOUT_MS: u32 = 120_000;

struct Spec {
    id: &'static str,
    name: &'static str,
    name_key: &'static str,
    group: TemplateGroup,
    protocol: Protocol,
    base_url: &'static str,
    thinking_off: ThinkingOff,
    merge_system_into_user: bool,
    strip_think_tags: bool,
    max_tokens_field: Option<MaxTokensField>,
    key_required: bool,
    local: bool,
    regions: &'static [(&'static str, &'static str)],
    /// 预置的模型（没有模型列表端点的服务用）
    models: &'static [&'static str],
}

/// 默认值：OpenAI 兼容、剥离 `<think>`、云端服务
const BASE: Spec = Spec {
    id: "",
    name: "",
    name_key: "",
    group: TemplateGroup::Other,
    protocol: Protocol::OpenAiChat,
    base_url: "",
    thinking_off: ThinkingOff::None,
    merge_system_into_user: false,
    strip_think_tags: true,
    max_tokens_field: None,
    key_required: true,
    local: false,
    regions: &[],
    models: &[],
};

const SPECS: [Spec; 17] = [
    // 推理模型不发 temperature、用 max_completion_tokens；官方端点不会内联 <think>
    Spec {
        id: "openai",
        name: "OpenAI",
        name_key: "settings.providers.template.openai",
        group: TemplateGroup::Openai,
        base_url: "https://api.openai.com/v1",
        thinking_off: ThinkingOff::OpenAiEffort,
        strip_think_tags: false,
        max_tokens_field: Some(MaxTokensField::MaxCompletionTokens),
        ..BASE
    },
    // 总是发 store:false
    Spec {
        id: "openai-responses",
        name: "OpenAI Responses",
        name_key: "settings.providers.template.openai-responses",
        group: TemplateGroup::Openai,
        protocol: Protocol::OpenAiResponses,
        base_url: "https://api.openai.com/v1",
        thinking_off: ThinkingOff::OpenAiEffort,
        strip_think_tags: false,
        ..BASE
    },
    // 不发 temperature，用 output_config.effort
    Spec {
        id: "anthropic",
        name: "Anthropic",
        name_key: "settings.providers.template.anthropic",
        group: TemplateGroup::Anthropic,
        protocol: Protocol::AnthropicMessages,
        base_url: "https://api.anthropic.com",
        thinking_off: ThinkingOff::AnthropicEffort,
        strip_think_tags: false,
        ..BASE
    },
    Spec {
        id: "gemini",
        name: "Google Gemini",
        name_key: "settings.providers.template.gemini",
        group: TemplateGroup::Gemini,
        protocol: Protocol::Gemini,
        base_url: "https://generativelanguage.googleapis.com",
        thinking_off: ThinkingOff::GeminiLevel,
        strip_think_tags: false,
        ..BASE
    },
    // 规范化时补 /v1；thinking.type=disabled
    Spec {
        id: "deepseek",
        name: "DeepSeek",
        name_key: "settings.providers.template.deepseek",
        base_url: "https://api.deepseek.com",
        thinking_off: ThinkingOff::DeepSeek,
        max_tokens_field: Some(MaxTokensField::MaxTokens),
        ..BASE
    },
    Spec {
        id: "openrouter",
        name: "OpenRouter",
        name_key: "settings.providers.template.openrouter",
        base_url: "https://openrouter.ai/api/v1",
        max_tokens_field: Some(MaxTokensField::MaxCompletionTokens),
        ..BASE
    },
    // 带图请求把 system 合并进 user
    Spec {
        id: "qwen",
        name: "通义千问（百炼）",
        name_key: "settings.providers.template.qwen",
        base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        thinking_off: ThinkingOff::Qwen,
        merge_system_into_user: true,
        max_tokens_field: Some(MaxTokensField::MaxTokens),
        regions: &[
            (
                "settings.providers.region.cn",
                "https://dashscope.aliyuncs.com/compatible-mode/v1",
            ),
            (
                "settings.providers.region.intl",
                "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
            ),
            // 需要用户把 {WorkspaceId} 换成自己的业务空间 id
            (
                "settings.providers.region.workspace",
                "https://{WorkspaceId}.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
            ),
        ],
        ..BASE
    },
    // base 已有版本段 /v4
    Spec {
        id: "zhipu",
        name: "智谱",
        name_key: "settings.providers.template.zhipu",
        base_url: "https://open.bigmodel.cn/api/paas/v4",
        thinking_off: ThinkingOff::Glm,
        max_tokens_field: Some(MaxTokensField::MaxTokens),
        ..BASE
    },
    // thinking.disabled 用 glm 变体；max_tokens 默认只有 4k，必须显式发送（请求总会带上）
    Spec {
        id: "volcengine",
        name: "火山方舟",
        name_key: "settings.providers.template.volcengine",
        base_url: "https://ark.cn-beijing.volces.com/api/v3",
        thinking_off: ThinkingOff::Glm,
        max_tokens_field: Some(MaxTokensField::MaxTokens),
        ..BASE
    },
    // reasoning_effort:"low"
    Spec {
        id: "moonshot",
        name: "Moonshot / Kimi",
        name_key: "settings.providers.template.moonshot",
        base_url: "https://api.moonshot.cn/v1",
        thinking_off: ThinkingOff::Kimi,
        max_tokens_field: Some(MaxTokensField::MaxTokens),
        regions: &[
            ("settings.providers.region.cn", "https://api.moonshot.cn/v1"),
            ("settings.providers.region.intl", "https://api.moonshot.ai/v1"),
        ],
        ..BASE
    },
    // enable_thinking:false 用 qwen 变体
    Spec {
        id: "siliconflow",
        name: "硅基流动",
        name_key: "settings.providers.template.siliconflow",
        base_url: "https://api.siliconflow.cn/v1",
        thinking_off: ThinkingOff::Qwen,
        max_tokens_field: Some(MaxTokensField::MaxTokens),
        ..BASE
    },
    // content 内联 <think>
    Spec {
        id: "minimax",
        name: "MiniMax",
        name_key: "settings.providers.template.minimax",
        base_url: "https://api.minimaxi.com/v1",
        thinking_off: ThinkingOff::MiniMax,
        max_tokens_field: Some(MaxTokensField::MaxCompletionTokens),
        regions: &[
            ("settings.providers.region.cn", "https://api.minimaxi.com/v1"),
            ("settings.providers.region.intl", "https://api.minimax.io/v1"),
        ],
        ..BASE
    },
    Spec {
        id: "groq",
        name: "Groq",
        name_key: "settings.providers.template.groq",
        base_url: "https://api.groq.com/openai/v1",
        max_tokens_field: Some(MaxTokensField::MaxCompletionTokens),
        ..BASE
    },
    // 自动补 /v1；Key 可选；超时 120 s
    Spec {
        id: "ollama",
        name: "Ollama",
        name_key: "settings.providers.template.ollama",
        group: TemplateGroup::Local,
        base_url: "http://localhost:11434",
        max_tokens_field: Some(MaxTokensField::MaxTokens),
        key_required: false,
        local: true,
        ..BASE
    },
    Spec {
        id: "lmstudio",
        name: "LM Studio",
        name_key: "settings.providers.template.lmstudio",
        group: TemplateGroup::Local,
        base_url: "http://localhost:1234",
        max_tokens_field: Some(MaxTokensField::MaxTokens),
        key_required: false,
        local: true,
        ..BASE
    },
    // 四种协议任选，base_url 填站点根；先用 max_completion_tokens，被拒时自动互换
    Spec {
        id: "relay",
        name: "中转站",
        name_key: "settings.providers.template.relay",
        max_tokens_field: Some(MaxTokensField::MaxCompletionTokens),
        ..BASE
    },
    // 文字识别服务（不是 LLM）：默认是 AI Studio 的异步任务接口；也可以填 AI Studio 部署的服务或自托管 PaddleX 的
    // serving 地址（`…/ocr`、`…/layout-parsing`）。Token 是 AI Studio 的访问令牌
    Spec {
        id: "paddleocr",
        name: "PaddleOCR",
        name_key: "settings.providers.template.paddleocr",
        group: TemplateGroup::Ocr,
        protocol: Protocol::PaddleOcr,
        base_url: "https://paddleocr.aistudio-app.com/api/v2/ocr/jobs",
        strip_think_tags: false,
        models: &["PP-OCRv6", "PP-StructureV3", "PaddleOCR-VL-1.6"],
        ..BASE
    },
];

impl Spec {
    fn provider(&self, id: String) -> Provider {
        let timeouts = if self.local {
            Timeouts {
                first_token_ms: LOCAL_TIMEOUT_MS,
                idle_ms: LOCAL_TIMEOUT_MS,
                ..Timeouts::default()
            }
        } else {
            Timeouts::default()
        };
        Provider {
            id,
            name: self.name.to_owned(),
            protocol: self.protocol,
            base_url: self.base_url.to_owned(),
            headers: BTreeMap::new(),
            quirks: Quirks {
                thinking_off: self.thinking_off,
                merge_system_into_user: self.merge_system_into_user,
                strip_think_tags: self.strip_think_tags,
                max_tokens_field: self.max_tokens_field,
                dual_auth_header: None,
                gemini_key_in_query: false,
            },
            models: self.models.iter().map(|id| ModelEntry::new(*id)).collect(),
            timeouts,
            template_id: Some(self.id.to_owned()),
        }
    }

    fn template(&self) -> ProviderTemplate {
        ProviderTemplate {
            id: self.id.to_owned(),
            name_key: self.name_key.to_owned(),
            group: self.group,
            provider: self.provider(self.id.to_owned()),
            key_required: self.key_required,
            region_variants: self
                .regions
                .iter()
                .map(|(label_key, base_url)| RegionVariant {
                    label_key: (*label_key).to_owned(),
                    base_url: (*base_url).to_owned(),
                })
                .collect(),
        }
    }
}

/// 全部模板（17 个，顺序固定）
pub fn templates() -> Vec<ProviderTemplate> {
    SPECS.iter().map(Spec::template).collect()
}

/// 由模板生成 Provider，id 为不与 `existing_ids` 冲突的不可变 slug（`openai`、`openai-2`、`openai-3`……）；
/// 模板不存在时返回 None
pub fn provider_from_template(template_id: &str, existing_ids: &[&str]) -> Option<Provider> {
    let spec = SPECS.iter().find(|s| s.id == template_id)?;
    let taken = |id: &str| existing_ids.iter().any(|e| e.eq_ignore_ascii_case(id));
    let id = if taken(spec.id) {
        (2u32..)
            .map(|n| format!("{}-{n}", spec.id))
            .find(|candidate| !taken(candidate))?
    } else {
        spec.id.to_owned()
    };
    Some(spec.provider(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_in_order() {
        let ids: Vec<String> = templates().into_iter().map(|t| t.id).collect();
        assert_eq!(
            ids,
            [
                "openai",
                "openai-responses",
                "anthropic",
                "gemini",
                "deepseek",
                "openrouter",
                "qwen",
                "zhipu",
                "volcengine",
                "moonshot",
                "siliconflow",
                "minimax",
                "groq",
                "ollama",
                "lmstudio",
                "relay",
                "paddleocr",
            ]
        );
    }

    /// PaddleOCR 是文字识别服务：单独的分组，预置模型，地址就是完整的接口地址
    #[test]
    fn paddleocr_template() {
        let t = templates()
            .into_iter()
            .find(|t| t.id == "paddleocr")
            .expect("paddleocr");
        assert_eq!(t.group, TemplateGroup::Ocr);
        assert_eq!(t.provider.protocol, Protocol::PaddleOcr);
        assert!(t.key_required);
        let models: Vec<&str> = t.provider.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(models, ["PP-OCRv6", "PP-StructureV3", "PaddleOCR-VL-1.6"]);
        // 只有 PaddleOCR 是非 LLM 协议，也只有它在 ocr 分组
        for t in templates() {
            assert_eq!(t.provider.protocol.is_llm(), t.group != TemplateGroup::Ocr, "{}", t.id);
        }
    }

    #[test]
    fn keys_follow_namespace_convention() {
        for t in templates() {
            assert_eq!(t.name_key, format!("settings.providers.template.{}", t.id));
            assert_eq!(t.provider.template_id.as_deref(), Some(t.id.as_str()));
            for r in &t.region_variants {
                assert!(r.label_key.starts_with("settings.providers.region."));
            }
            if let Some(first) = t.region_variants.first() {
                assert_eq!(first.base_url, t.provider.base_url, "第一个区域变体就是默认 base_url");
            }
        }
    }

    #[test]
    fn local_templates_have_long_timeouts_and_optional_key() {
        for t in templates().into_iter().filter(|t| t.group == TemplateGroup::Local) {
            assert!(!t.key_required);
            assert_eq!(t.provider.timeouts.first_token_ms, 120_000);
            assert_eq!(t.provider.timeouts.idle_ms, 120_000);
            assert_eq!(t.provider.timeouts.connect_ms, 10_000);
        }
    }

    #[test]
    fn unique_slug_ids() {
        let p = provider_from_template("openai", &[]).map(|p| p.id);
        assert_eq!(p.as_deref(), Some("openai"));
        let p = provider_from_template("openai", &["openai", "OpenAI-2"]).map(|p| p.id);
        assert_eq!(p.as_deref(), Some("openai-3"));
        assert!(provider_from_template("nope", &[]).is_none());
    }
}
