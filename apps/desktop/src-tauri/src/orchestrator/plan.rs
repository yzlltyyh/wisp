//! 请求的规划（纯函数，便于单测）：风格与模型的解析、语言检测与方向、词典式判定、
//! 长文本判定、提示词装配、缓存键。

use std::sync::OnceLock;
use std::time::Duration;

use wisp_llm::{ChatRequest, Purpose, Secret, UserContent};
use wisp_prompts::{
    BuiltPrompt, CacheKeyInput, Detection, GlossaryMatcher, PromptCtx, PromptTask, build, cache_key, detect_lang,
    estimate_tokens, target_for, use_dictionary,
};
use wisp_types::{
    DictMode, Lang, LongTextInfo, ModelEntry, ModelRef, NotReady, OcrMode, Provider, RequestId, Settings, StreamEvent,
    Style, TaskKind,
};

use super::registry::ModelInfo;

/// 解析好的模型
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub provider: Provider,
    pub model: ModelEntry,
}

impl Resolved {
    /// `Start` 事件里的模型信息（标签缺省时用模型 id）
    pub fn info(&self) -> ModelInfo {
        ModelInfo {
            provider_id: self.provider.id.clone(),
            model_id: self.model.id.clone(),
            model_label: self
                .model
                .label
                .clone()
                .filter(|l| !l.trim().is_empty())
                .unwrap_or_else(|| self.model.id.clone()),
        }
    }
}

/// 在设置里找模型引用：Provider 必须存在；模型不在列表里时按 id 构造（中转站的别名可以手填）
pub fn lookup(settings: &Settings, r: &ModelRef) -> Option<Resolved> {
    let provider = settings.provider(&r.provider_id)?;
    let model = provider
        .models
        .iter()
        .find(|m| m.id == r.model_id)
        .cloned()
        .unwrap_or_else(|| ModelEntry::new(r.model_id.clone()));
    Some(Resolved {
        provider: provider.clone(),
        model,
    })
}

/// 文本任务的模型：请求指定 > 风格绑定 > 默认翻译模型（引用失效的跳过）
pub fn resolve_text_model(
    settings: &Settings,
    explicit: Option<&ModelRef>,
    style: &Style,
    task: TaskKind,
) -> Result<Resolved, NotReady> {
    let candidates = [explicit, style.model_ref.as_ref(), settings.defaults.translate.as_ref()];
    if let Some(r) = candidates.into_iter().flatten().find_map(|r| lookup(settings, r)) {
        return Ok(r);
    }
    Err(if settings.providers.is_empty() {
        NotReady::NoProvider
    } else {
        NotReady::NoDefaultModel { task }
    })
}

/// OCR 的模型：请求指定 > 默认 OCR 模型。默认模型已被判定为不支持视觉（运行时反馈写成 No）时报 `NoVisionModel`，
/// 让用户换模型；用户在「重新识别 ▾」里明确选的模型照用
pub fn resolve_ocr_model(settings: &Settings, explicit: Option<&ModelRef>) -> Result<Resolved, NotReady> {
    if settings.providers.is_empty() {
        return Err(NotReady::NoProvider);
    }
    if let Some(r) = explicit.and_then(|r| lookup(settings, r)) {
        return Ok(r);
    }
    match settings.defaults.ocr.as_ref().and_then(|r| lookup(settings, r)) {
        Some(r) if r.model.vision != wisp_types::Vision::No => Ok(r),
        _ => Err(NotReady::NoVisionModel),
    }
}

/// 按 id 找风格：找不到时退回设置里的默认风格、再退回 natural、再退回第一个
pub fn resolve_style(styles: &[Style], id: &str, default_id: &str) -> Style {
    styles
        .iter()
        .find(|s| s.id == id)
        .or_else(|| styles.iter().find(|s| s.id == default_id))
        .or_else(|| styles.iter().find(|s| s.id == wisp_types::defaults::DEFAULT_STYLE_ID))
        .or_else(|| styles.first())
        .cloned()
        .unwrap_or_else(|| {
            wisp_prompts::builtin_styles()
                .into_iter()
                .next()
                .unwrap_or_else(|| Style::custom(id, id))
        })
}

/// 一次文本任务的输入
#[derive(Debug, Clone, Copy)]
pub struct TextInput<'a> {
    pub text: &'a str,
    pub task: TaskKind,
    pub style_id: &'a str,
    /// 语言条的手动目标（只对当前会话生效）；None = 自动方向
    pub target: Option<Lang>,
    pub dict_mode: DictMode,
}

/// 文本任务的规划结果（模型由 `not_ready::check_text` 解析，这里不重复）
#[derive(Debug, Clone, PartialEq)]
pub struct TextPlan {
    pub task: TaskKind,
    pub style: Style,
    pub detection: Detection,
    pub target: Lang,
    pub dict: bool,
}

impl TextPlan {
    /// 登记时紧跟 Start 写入的 Meta
    pub fn meta(&self) -> StreamEvent {
        StreamEvent::Meta {
            detected_source_lang: Some(self.detection.lang),
            target_lang: Some(self.target),
            dict_mode: Some(self.dict),
            low_confidence: Some(self.detection.low_confidence),
        }
    }

    /// 是否可以缓存（refine 不缓存，缓存关闭时不缓存）
    pub fn cacheable(&self, settings: &Settings) -> bool {
        settings.translate.cache_enabled && self.task != TaskKind::Refine
    }
}

/// 规划一次文本任务：风格、检测、方向、词典式
pub fn plan_text(settings: &Settings, styles: &[Style], input: &TextInput<'_>) -> TextPlan {
    let t = &settings.translate;
    let style = resolve_style(styles, input.style_id, &t.default_style);
    let detection = detect_lang(input.text);
    let target = match input.task {
        // 润色 / 校对是同语言任务：目标即原文语言（无法判断时按主语言）
        TaskKind::Polish | TaskKind::Proofread => match detection.lang {
            Lang::Other => t.primary_lang,
            lang => lang,
        },
        TaskKind::Translate | TaskKind::Refine => input
            .target
            .unwrap_or_else(|| target_for(&detection, t.primary_lang, t.secondary_lang)),
    };
    let dict = input.task == TaskKind::Translate && use_dictionary(input.text, &style, input.dict_mode);
    TextPlan {
        task: input.task,
        style,
        detection,
        target,
        dict,
    }
}

/// 装配文本任务的提示词（refine 两步另行装配）
pub fn build_text_prompt(plan: &TextPlan, text: &str, glossary: &GlossaryMatcher, reasoning: bool) -> BuiltPrompt {
    let hits = glossary.hits(text);
    let ctx = PromptCtx {
        text,
        source: plan.detection,
        target: plan.target,
        glossary: &hits,
        reasoning_model: reasoning,
    };
    let task = match plan.task {
        TaskKind::Polish => PromptTask::Polish,
        TaskKind::Proofread => PromptTask::Proofread,
        TaskKind::Translate | TaskKind::Refine => PromptTask::Translate {
            style: &plan.style,
            dict: plan.dict,
        },
    };
    build(&task, &ctx)
}

/// refine 第一步（反思）的提示词
pub fn build_reflect_prompt(plan: &TextPlan, text: &str, draft: &str, reasoning: bool) -> BuiltPrompt {
    let ctx = PromptCtx {
        text,
        source: plan.detection,
        target: plan.target,
        glossary: &[],
        reasoning_model: reasoning,
    };
    build(&PromptTask::RefineReflect { draft }, &ctx)
}

/// refine 第二步（改进）的提示词
pub fn build_improve_prompt(
    plan: &TextPlan,
    text: &str,
    draft: &str,
    review: &str,
    glossary: &GlossaryMatcher,
    reasoning: bool,
) -> BuiltPrompt {
    let hits = glossary.hits(text);
    let ctx = PromptCtx {
        text,
        source: plan.detection,
        target: plan.target,
        glossary: &hits,
        reasoning_model: reasoning,
    };
    build(
        &PromptTask::RefineImprove {
            style: &plan.style,
            draft,
            review,
        },
        &ctx,
    )
}

/// 缓存键（refine 不可缓存，调用方先用 [`TextPlan::cacheable`] 判断）
pub fn text_cache_key(plan: &TextPlan, resolved: &Resolved, text: &str, glossary_version: u64) -> u128 {
    cache_key(&CacheKeyInput {
        provider_id: &resolved.provider.id,
        model_id: &resolved.model.id,
        task: plan.task,
        style: (plan.task == TaskKind::Translate).then_some(&plan.style),
        dict: plan.dict,
        source_lang: plan.detection.lang,
        target_lang: plan.target,
        glossary_version,
        text,
    })
}

/// OCR 提示词（`mode = Translate` 即直接翻译）
pub fn build_ocr_prompt(mode: OcrMode, target: Lang, reasoning: bool) -> BuiltPrompt {
    let ctx = PromptCtx {
        text: "",
        source: Detection {
            lang: Lang::Other,
            low_confidence: true,
        },
        target,
        glossary: &[],
        reasoning_model: reasoning,
    };
    build(&PromptTask::Ocr { mode }, &ctx)
}

/// 设置里的默认 OCR 模式（只允许 lines / paragraphs / markdown；手改成 translate 时按 lines）
pub fn default_ocr_mode(settings: &Settings) -> OcrMode {
    match settings.ocr.default_mode {
        OcrMode::Translate => OcrMode::Lines,
        m => m,
    }
}

/// 组装文本请求
pub fn chat_request(
    id: &RequestId,
    resolved: &Resolved,
    key: Option<Secret>,
    built: BuiltPrompt,
    purpose: Purpose,
) -> ChatRequest {
    ChatRequest {
        request_id: id.clone(),
        provider: resolved.provider.clone(),
        key,
        model: resolved.model.clone(),
        system: built.system,
        user: UserContent::Text(built.user_text),
        temperature: built.temperature,
        max_output_tokens: built.max_output_tokens,
        purpose,
    }
}

/// 生成速度的经验值（长文本提示里的「预计耗时」用；有实测后由编排器滚动更新）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Throughput {
    /// 首 token 延迟（秒）
    pub first_token_s: f32,
    /// 输出速度（token / 秒）
    pub tokens_per_s: f32,
}

impl Default for Throughput {
    fn default() -> Self {
        Self {
            first_token_s: 1.0,
            tokens_per_s: 45.0,
        }
    }
}

impl Throughput {
    /// 指数滑动平均（权重 0.3）；样本不可信（太短）时不更新
    pub fn observe(&mut self, first_token: Duration, streaming: Duration, output_tokens: u32) {
        const ALPHA: f32 = 0.3;
        let ft = first_token.as_secs_f32();
        if ft.is_finite() && ft > 0.0 {
            self.first_token_s = self.first_token_s * (1.0 - ALPHA) + ft * ALPHA;
        }
        let secs = streaming.as_secs_f32();
        if output_tokens >= 32 && secs >= 0.25 {
            let tps = output_tokens as f32 / secs;
            if tps.is_finite() && tps > 0.0 {
                self.tokens_per_s = self.tokens_per_s * (1.0 - ALPHA) + tps * ALPHA;
            }
        }
    }
}

/// 超过阈值（按字符数）时的长文本信息：字数、估算 token、预计耗时（不显示金额）
pub fn long_text(text: &str, threshold: u32, speed: Throughput) -> Option<LongTextInfo> {
    let chars = u32::try_from(text.chars().count()).unwrap_or(u32::MAX);
    if chars <= threshold {
        return None;
    }
    let est_tokens = estimate_tokens(text);
    // 译文与原文 token 数大致相当；输出上限 4096
    let out = est_tokens.min(4096) as f32;
    let secs = speed.first_token_s + out / speed.tokens_per_s.max(1.0);
    Some(LongTextInfo {
        chars,
        est_tokens,
        est_seconds: secs.is_finite().then_some((secs * 10.0).round() / 10.0),
    })
}

/// Provider 是否需要 Key：模板声明不需要（Ollama 等本地服务）或指向回环地址时不需要
pub fn provider_needs_key(provider: &Provider) -> bool {
    if let Some(t) = provider.template_id.as_deref()
        && let Some(required) = template_key_required(t)
    {
        return required;
    }
    !is_loopback(&provider.base_url)
}

/// 模板是否声明需要 Key（模板表只在第一次查询时生成一次，之后按 id 查，不再每次分配整张表）
fn template_key_required(template_id: &str) -> Option<bool> {
    static TABLE: OnceLock<Vec<(String, bool)>> = OnceLock::new();
    TABLE
        .get_or_init(|| {
            wisp_llm::templates()
                .into_iter()
                .map(|t| (t.id, t.key_required))
                .collect()
        })
        .iter()
        .find(|(id, _)| id == template_id)
        .map(|(_, required)| *required)
}

fn is_loopback(base_url: &str) -> bool {
    let rest = base_url.trim().split_once("://").map_or(base_url.trim(), |(_, r)| r);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let host = if let Some(v6) = host.strip_prefix('[') {
        v6.split(']').next().unwrap_or("")
    } else {
        host.split(':').next().unwrap_or("")
    };
    let host = host.to_ascii_lowercase();
    host == "localhost" || host.ends_with(".localhost") || host.starts_with("127.") || host == "::1"
}

#[cfg(test)]
pub(crate) mod tests {
    use wisp_types::{Protocol, Quirks, Timeouts, Vision};

    use super::*;

    pub fn provider(id: &str, models: &[&str]) -> Provider {
        Provider {
            id: id.to_owned(),
            name: id.to_owned(),
            protocol: Protocol::OpenAiChat,
            base_url: "https://api.example.com/v1".to_owned(),
            headers: Default::default(),
            quirks: Quirks::default(),
            models: models.iter().map(|m| ModelEntry::new(*m)).collect(),
            timeouts: Timeouts::default(),
            template_id: None,
        }
    }

    pub fn model_ref(p: &str, m: &str) -> ModelRef {
        ModelRef {
            provider_id: p.to_owned(),
            model_id: m.to_owned(),
        }
    }

    pub fn configured() -> Settings {
        let mut s = Settings {
            providers: vec![provider("p", &["m", "vision"])],
            ..Settings::default()
        };
        s.defaults.translate = Some(model_ref("p", "m"));
        s.defaults.ocr = Some(model_ref("p", "vision"));
        s
    }

    fn input<'a>(text: &'a str) -> TextInput<'a> {
        TextInput {
            text,
            task: TaskKind::Translate,
            style_id: "natural",
            target: None,
            dict_mode: DictMode::Auto,
        }
    }

    #[test]
    fn direction_dictionary_and_manual_target() {
        let s = configured();
        let styles = wisp_prompts::builtin_styles();
        let en = plan_text(&s, &styles, &input("The meeting has been moved to Thursday afternoon."));
        assert_eq!((en.detection.lang, en.target, en.dict), (Lang::En, Lang::ZhCn, false));
        let zh = plan_text(&s, &styles, &input("会议改到周四下午了。"));
        assert_eq!((zh.detection.lang, zh.target), (Lang::ZhCn, Lang::En));
        // 单词走词典式；literal 风格关闭词典式
        assert!(plan_text(&s, &styles, &input("serendipity")).dict);
        let mut literal = input("serendipity");
        literal.style_id = "literal";
        assert!(!plan_text(&s, &styles, &literal).dict);
        // 语言条手动改目标
        let mut manual = input("hello world, how are you today?");
        manual.target = Some(Lang::En);
        assert_eq!(plan_text(&s, &styles, &manual).target, Lang::En);
        // 润色：同语言
        let mut polish = input("会议改到周四下午了。");
        polish.task = TaskKind::Polish;
        let p = plan_text(&s, &styles, &polish);
        assert_eq!((p.target, p.dict), (Lang::ZhCn, false));
        // 未知风格退回默认风格
        let mut unknown = input("hi");
        unknown.style_id = "custom-gone";
        assert_eq!(plan_text(&s, &styles, &unknown).style.id, "natural");
    }

    #[test]
    fn model_resolution_order_and_not_ready() {
        let mut s = configured();
        let styles = wisp_prompts::builtin_styles();
        let style = resolve_style(&styles, "natural", "natural");
        assert_eq!(
            resolve_text_model(&s, None, &style, TaskKind::Translate).map(|r| r.model.id),
            Ok("m".to_owned())
        );
        // 请求指定 > 风格绑定 > 默认
        let mut bound = style.clone();
        bound.model_ref = Some(model_ref("p", "vision"));
        assert_eq!(
            resolve_text_model(&s, None, &bound, TaskKind::Translate).map(|r| r.model.id),
            Ok("vision".to_owned())
        );
        let explicit = model_ref("p", "alias-not-listed");
        assert_eq!(
            resolve_text_model(&s, Some(&explicit), &bound, TaskKind::Translate).map(|r| r.model.id),
            Ok("alias-not-listed".to_owned())
        );
        // 引用失效的跳过
        let gone = model_ref("deleted", "x");
        assert_eq!(
            resolve_text_model(&s, Some(&gone), &style, TaskKind::Translate).map(|r| r.model.id),
            Ok("m".to_owned())
        );
        s.defaults.translate = None;
        assert_eq!(
            resolve_text_model(&s, None, &style, TaskKind::Polish),
            Err(NotReady::NoDefaultModel { task: TaskKind::Polish })
        );
        s.providers.clear();
        assert_eq!(
            resolve_text_model(&s, None, &style, TaskKind::Translate),
            Err(NotReady::NoProvider)
        );
    }

    #[test]
    fn ocr_model_requires_vision() {
        let mut s = configured();
        assert_eq!(resolve_ocr_model(&s, None).map(|r| r.model.id), Ok("vision".to_owned()));
        s.providers[0].models[1].vision = Vision::No;
        assert_eq!(resolve_ocr_model(&s, None), Err(NotReady::NoVisionModel));
        // 用户在下拉里明确选的照用
        let pick = model_ref("p", "vision");
        assert!(resolve_ocr_model(&s, Some(&pick)).is_ok());
        s.defaults.ocr = None;
        assert_eq!(resolve_ocr_model(&s, None), Err(NotReady::NoVisionModel));
        s.providers.clear();
        assert_eq!(resolve_ocr_model(&s, None), Err(NotReady::NoProvider));
    }

    #[test]
    fn cache_key_separates_dictionary_and_styles() {
        let s = configured();
        let styles = wisp_prompts::builtin_styles();
        let g = GlossaryMatcher::default();
        let natural = plan_text(&s, &styles, &input("token"));
        let mut literal_in = input("token");
        literal_in.style_id = "literal";
        let literal = plan_text(&s, &styles, &literal_in);
        let r = resolve_text_model(&s, None, &natural.style, TaskKind::Translate).expect("model");
        let k1 = text_cache_key(&natural, &r, "token", g.version());
        let k2 = text_cache_key(&literal, &r, "token", g.version());
        assert_ne!(k1, k2, "词典式和正文分开缓存");
        // 只差首尾空白的两次选择共用缓存
        assert_eq!(k1, text_cache_key(&natural, &r, "token\n", g.version()));
        assert!(natural.cacheable(&s));
    }

    #[test]
    fn long_text_threshold_and_estimate() {
        let speed = Throughput::default();
        assert!(long_text(&"字".repeat(3000), 3000, speed).is_none());
        let info = long_text(&"字".repeat(3001), 3000, speed).expect("long");
        assert_eq!(info.chars, 3001);
        assert_eq!(info.est_tokens, 3001);
        assert!(info.est_seconds.is_some_and(|s| s > 10.0));
    }

    #[test]
    fn keyless_providers() {
        let mut p = provider("ollama", &["llama"]);
        p.base_url = "http://localhost:11434/v1".into();
        assert!(!provider_needs_key(&p));
        p.base_url = "127.0.0.1:1234".into();
        assert!(!provider_needs_key(&p));
        p.base_url = "http://[::1]:8080".into();
        assert!(!provider_needs_key(&p));
        p.base_url = "https://api.openai.com/v1".into();
        assert!(provider_needs_key(&p));
        // 模板声明优先于地址
        let ollama = wisp_llm::templates()
            .into_iter()
            .find(|t| !t.key_required)
            .expect("a keyless template");
        p.template_id = Some(ollama.id.clone());
        assert!(!provider_needs_key(&p));
        assert_eq!(template_key_required("no-such-template"), None);
    }

    #[test]
    fn throughput_ema() {
        let mut t = Throughput::default();
        t.observe(Duration::from_millis(500), Duration::from_secs(2), 200);
        assert!(t.tokens_per_s > 45.0 && t.tokens_per_s < 100.0);
        assert!(t.first_token_s < 1.0);
        let before = t;
        t.observe(Duration::from_millis(500), Duration::from_millis(100), 10);
        assert!(
            (t.tokens_per_s - before.tokens_per_s).abs() < f32::EPSILON,
            "太短的样本不更新速度"
        );
    }
}
