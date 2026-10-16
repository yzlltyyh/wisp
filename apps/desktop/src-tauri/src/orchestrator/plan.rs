//! 请求的规划（纯函数，便于单测）：风格与模型的解析（含离线模型与 PaddleOCR 云端）、语言检测与方向、词典式判定、
//! 长文本判定、提示词装配、缓存键。
//!
//! **离线模型**：选择只在本机的 `offline` 分组里（`offline.ocr_model` / `offline.translate_model`），优先级是
//! 请求显式指定 > `offline.*` > 风格绑定 > `defaults`。离线翻译模型以合成服务商 [`OFFLINE_PROVIDER_ID`] 出现在请求里
//! （[`offline_translate`]，真正的地址与 key 在模型服务加载好推理进程之后才填）；离线 OCR 模型与 PaddleOCR 云端不是一次
//! LLM 调用，解析为 [`OcrTarget::Engine`]。`@offline` 只能出现在请求的显式指定里：同步来的风格绑定、`defaults` 指向它时
//! 按「引用失效」跳过（校验也会把它们置空）。

use std::sync::OnceLock;
use std::time::Duration;

use wisp_llm::{ChatRequest, Purpose, Secret, UserContent};
use wisp_prompts::{
    BuiltPrompt, CacheKeyInput, Detection, GlossaryMatcher, PromptCtx, PromptProfile, PromptTask, build_profile,
    cache_key, detect_lang, estimate_tokens, target_for, use_dictionary,
};
use wisp_types::defaults::OFFLINE_PROVIDER_ID;
use wisp_types::{
    DictMode, Lang, LocalModelId, LocalModelKind, LongTextInfo, MaxTokensField, ModelCaps, ModelEntry, ModelRef,
    NotReady, OcrLayout, OcrMode, Protocol, Provider, Quirks, RequestId, Settings, StreamEvent, Style, TaskKind,
    ThinkingOff, Timeouts, Vision, VisionSource,
};

use super::captures::ModelKey;
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

    /// 离线翻译模型（合成服务商）时为它的 id
    pub fn offline_model(&self) -> Option<LocalModelId> {
        is_offline(&self.provider)
            .then(|| LocalModelId::parse(&self.model.id))
            .flatten()
    }

    /// 提示词档位：离线翻译模型用 Index-Translate 的官方格式
    pub fn prompt_profile(&self) -> PromptProfile {
        if self.offline_model().is_some() {
            PromptProfile::IndexTranslate
        } else {
            PromptProfile::Standard
        }
    }
}

/// 离线模型的显示名（`Start` 的模型标签、识别历史里的模型名）。都是产品名，不翻译
pub const fn offline_label(id: LocalModelId) -> &'static str {
    match id {
        LocalModelId::PpOcrV6Tiny => "PP-OCRv6 Tiny",
        LocalModelId::PpOcrV6Small => "PP-OCRv6 Small",
        LocalModelId::PaddleOcrVl16 => "PaddleOCR-VL-1.6",
        LocalModelId::IndexTranslate2b => "Index-Translate 2B",
        LocalModelId::IndexTranslate9b => "Index-Translate 9B",
    }
}

/// 本机推理的首字时限：小模型在慢机器上处理长原文、9B 在 CPU 上都可能远超云端的 30 s（加载时间不计入：
/// 请求在推理进程就绪之后才发出）
const OFFLINE_FIRST_TOKEN_MS: u32 = 120_000;

/// 是否为离线模型的合成服务商
pub fn is_offline(provider: &Provider) -> bool {
    provider.id == OFFLINE_PROVIDER_ID
}

/// 离线模型的合成服务商：OpenAI 兼容接口，地址在推理进程就绪后由模型服务填入（这里的回环地址只是占位，让
/// 「不需要 Key」的判定与真实请求一致：编排器不碰钥匙串）
pub fn offline_provider(thinking_off: ThinkingOff) -> Provider {
    Provider {
        id: OFFLINE_PROVIDER_ID.to_owned(),
        name: String::new(),
        protocol: Protocol::OpenAiChat,
        base_url: "http://127.0.0.1".to_owned(),
        headers: Default::default(),
        quirks: Quirks {
            thinking_off,
            // llama-server 接受 `max_tokens`；预置好，不必每次启动都被拒一轮再降级
            max_tokens_field: Some(MaxTokensField::MaxTokens),
            ..Quirks::default()
        },
        models: Vec::new(),
        timeouts: Timeouts {
            first_token_ms: OFFLINE_FIRST_TOKEN_MS,
            ..Timeouts::default()
        },
        template_id: None,
    }
}

/// 离线模型的合成模型条目（能力预置：本地服务接受的参数已知，不必在运行时试探）
pub fn offline_model_entry(id: LocalModelId) -> ModelEntry {
    let mut m = ModelEntry::new(id.as_str());
    m.label = Some(offline_label(id).to_owned());
    m.caps = ModelCaps {
        temperature: Some(true),
        max_tokens_field: Some(MaxTokensField::MaxTokens),
        stream_options: Some(true),
        ..ModelCaps::default()
    };
    if id.kind() == LocalModelKind::Ocr {
        m.vision = Vision::Yes;
        m.vision_source = Some(VisionSource::User);
    }
    m
}

/// 离线翻译模型的 `Resolved`（关思考：Index-Translate 基于会思考的模型，模板默认开思考）
pub fn offline_translate(id: LocalModelId) -> Resolved {
    Resolved {
        provider: offline_provider(ThinkingOff::ChatTemplateKwargs),
        model: offline_model_entry(id),
    }
}

/// 请求显式指定的离线模型（`@offline` + 模型 id，且用途相符）
fn explicit_offline(r: &ModelRef, kind: LocalModelKind) -> Option<LocalModelId> {
    (r.provider_id == OFFLINE_PROVIDER_ID)
        .then(|| LocalModelId::parse(&r.model_id))
        .flatten()
        .filter(|m| m.kind() == kind)
}

/// 设置里选用的离线模型（用途不符的值视为没选；校验本来就会把它置空）
fn selected_offline(choice: Option<LocalModelId>, kind: LocalModelKind) -> Option<LocalModelId> {
    choice.filter(|m| m.kind() == kind)
}

/// 在设置里找模型引用：Provider 必须存在；模型不在列表里时按 id 构造（中转站的别名可以手填）。
/// 保留 id（`@` 开头，离线模型）不在设置里，这里自然找不到
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

/// 能翻译的引用（PaddleOCR 云端这类文字识别服务不能）
fn lookup_llm(settings: &Settings, r: &ModelRef) -> Option<Resolved> {
    lookup(settings, r).filter(|r| r.provider.protocol.is_llm())
}

/// 文本任务的模型：请求指定 > 离线翻译模型 > 风格绑定 > 默认翻译模型（引用失效的跳过）
pub fn resolve_text_model(
    settings: &Settings,
    explicit: Option<&ModelRef>,
    style: &Style,
    task: TaskKind,
) -> Result<Resolved, NotReady> {
    if let Some(r) = explicit {
        if let Some(m) = explicit_offline(r, LocalModelKind::Translate) {
            return Ok(offline_translate(m));
        }
        if let Some(found) = lookup_llm(settings, r) {
            return Ok(found);
        }
    }
    if let Some(m) = selected_offline(settings.offline.translate_model, LocalModelKind::Translate) {
        return Ok(offline_translate(m));
    }
    let candidates = [style.model_ref.as_ref(), settings.defaults.translate.as_ref()];
    if let Some(r) = candidates.into_iter().flatten().find_map(|r| lookup_llm(settings, r)) {
        return Ok(r);
    }
    Err(if settings.providers.is_empty() {
        NotReady::NoProvider
    } else {
        NotReady::NoDefaultModel { task }
    })
}

/// 不是一次 LLM 调用的识别引擎
#[derive(Debug, Clone, PartialEq)]
pub enum EngineRef {
    /// 离线 OCR 模型（PP-OCRv6 Tiny / Small、PaddleOCR-VL）
    Local(LocalModelId),
    /// PaddleOCR 云端（`paddle_ocr` 协议的服务商与其中的模型名）
    Cloud { provider: Provider, model: String },
}

/// OCR 的识别目标：视觉 LLM，或识别引擎
#[derive(Debug, Clone, PartialEq)]
pub enum OcrTarget {
    Llm(Resolved),
    Engine(EngineRef),
}

impl OcrTarget {
    /// `Start` 事件里的模型信息
    pub fn info(&self) -> ModelInfo {
        match self {
            Self::Llm(r) => r.info(),
            Self::Engine(EngineRef::Local(m)) => ModelInfo {
                provider_id: OFFLINE_PROVIDER_ID.to_owned(),
                model_id: m.as_str().to_owned(),
                model_label: offline_label(*m).to_owned(),
            },
            Self::Engine(EngineRef::Cloud { provider, model }) => {
                let label = provider
                    .models
                    .iter()
                    .find(|e| e.id == *model)
                    .and_then(|e| e.label.clone())
                    .filter(|l| !l.trim().is_empty())
                    .unwrap_or_else(|| model.clone());
                ModelInfo {
                    provider_id: provider.id.clone(),
                    model_id: model.clone(),
                    model_label: label,
                }
            }
        }
    }

    /// 能不能一次调用直接出译文（视觉 LLM 能；识别引擎只识别，直接翻译要走「识别 → 翻译」两步）
    pub fn can_translate(&self) -> bool {
        matches!(self, Self::Llm(_))
    }

    /// 是否支持某种模式
    pub fn supports(&self, mode: OcrMode) -> bool {
        match self {
            Self::Llm(_) => true,
            Self::Engine(EngineRef::Local(m)) => wisp_local::ocr::local_supports(*m, mode),
            Self::Engine(EngineRef::Cloud { model, .. }) => match mode {
                OcrMode::Lines | OcrMode::Paragraphs => true,
                OcrMode::Markdown => wisp_local::ocr::cloud::is_document_model(model),
                OcrMode::Translate => false,
            },
        }
    }

    /// 实际使用的模式：不支持的转录模式（PP-OCR 的 markdown）退回 paragraphs。界面已经禁用这些模式，这里是兜底
    pub fn effective_mode(&self, mode: OcrMode) -> OcrMode {
        if mode == OcrMode::Translate || self.supports(mode) {
            mode
        } else {
            OcrMode::Paragraphs
        }
    }

    /// 发请求需要哪个服务商的 Key（离线引擎不需要）
    pub fn key_provider(&self) -> Option<&Provider> {
        match self {
            Self::Llm(r) => Some(&r.provider),
            Self::Engine(EngineRef::Cloud { provider, .. }) => Some(provider),
            Self::Engine(EngineRef::Local(_)) => None,
        }
    }

    /// 转录缓存用的模型键
    pub fn model_key(&self) -> ModelKey {
        let info = self.info();
        (info.provider_id, info.model_id)
    }

    /// 离线 OCR 模型
    pub fn local(&self) -> Option<LocalModelId> {
        match self {
            Self::Engine(EngineRef::Local(m)) => Some(*m),
            _ => None,
        }
    }
}

/// 设置里的引用 → OCR 目标：`paddle_ocr` 服务商是识别引擎（不看 vision 字段：模板里的值是 unknown，且非用户指定的
/// vision 不随同步）；其余是视觉 LLM
fn ocr_target_of(r: Resolved) -> OcrTarget {
    if r.provider.protocol == Protocol::PaddleOcr {
        OcrTarget::Engine(EngineRef::Cloud {
            provider: r.provider,
            model: r.model.id,
        })
    } else {
        OcrTarget::Llm(r)
    }
}

/// OCR 的目标：请求指定 > 离线 OCR 模型 > 默认 OCR 模型。默认模型已被判定为不支持视觉（运行时反馈写成 No）时报
/// `NoVisionModel`，让用户换模型；用户在「重新识别 ▾」里明确选的模型照用
pub fn resolve_ocr_target(settings: &Settings, explicit: Option<&ModelRef>) -> Result<OcrTarget, NotReady> {
    if let Some(r) = explicit {
        if let Some(m) = explicit_offline(r, LocalModelKind::Ocr) {
            return Ok(OcrTarget::Engine(EngineRef::Local(m)));
        }
        if let Some(found) = lookup(settings, r) {
            return Ok(ocr_target_of(found));
        }
    }
    if let Some(m) = selected_offline(settings.offline.ocr_model, LocalModelKind::Ocr) {
        return Ok(OcrTarget::Engine(EngineRef::Local(m)));
    }
    if settings.providers.is_empty() {
        return Err(NotReady::NoProvider);
    }
    match settings.defaults.ocr.as_ref().and_then(|r| lookup(settings, r)) {
        Some(r) if r.provider.protocol == Protocol::PaddleOcr => Ok(ocr_target_of(r)),
        Some(r) if r.model.vision != Vision::No => Ok(OcrTarget::Llm(r)),
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

/// 离线翻译模型只负责翻译：词典式退化为普通翻译，提升译文退化为一次普通翻译（小模型不适合反思改写）。
/// 润色 / 校对在解析阶段就报 `OfflineTaskUnsupported`，不会走到这里
pub fn adapt_for(plan: &mut TextPlan, resolved: &Resolved) {
    if resolved.offline_model().is_some() {
        plan.dict = false;
        if plan.task == TaskKind::Refine {
            plan.task = TaskKind::Translate;
        }
    }
}

/// 装配文本任务的提示词（refine 两步另行装配）。`profile`：离线翻译模型用 Index-Translate 的官方格式
pub fn build_text_prompt(
    plan: &TextPlan,
    text: &str,
    glossary: &GlossaryMatcher,
    reasoning: bool,
    profile: PromptProfile,
) -> BuiltPrompt {
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
    build_profile(&task, &ctx, profile)
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
    wisp_prompts::build(&PromptTask::RefineReflect { draft }, &ctx)
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
    wisp_prompts::build(
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
    wisp_prompts::build(&PromptTask::Ocr { mode }, &ctx)
}

/// 设置里的默认 OCR 模式（只允许 lines / paragraphs / markdown；手改成 translate 时按 lines）
pub fn default_ocr_mode(settings: &Settings) -> OcrMode {
    match settings.ocr.default_mode {
        OcrMode::Translate => OcrMode::Lines,
        m => m,
    }
}

/// 设置里的默认排版（只允许 keep / remove_blank_lines；设置服务的校验已经拦下合并为一行，这里再兜一次底）
pub fn default_ocr_layout(settings: &Settings) -> OcrLayout {
    let layout = settings.ocr.layout;
    if layout.can_be_default() {
        layout
    } else {
        OcrLayout::Keep
    }
}

/// 一次识别的会话排版：直接翻译的截图（包括它的「查看原文」）恒为原样；其余取视图带回的排版（`requested`，重新识别
/// 时），没有时取设置里的默认。截图 commit、未就绪卡片与 `ocr_start` 都经这里，两条路径的规则只有这一份
pub fn session_ocr_layout(settings: &Settings, direct: bool, requested: Option<OcrLayout>) -> OcrLayout {
    if direct {
        OcrLayout::Keep
    } else {
        requested.unwrap_or_else(|| default_ocr_layout(settings))
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

    fn llm(t: Result<OcrTarget, NotReady>) -> Result<String, NotReady> {
        t.map(|t| match t {
            OcrTarget::Llm(r) => r.model.id,
            other => panic!("expected an LLM target, got {other:?}"),
        })
    }

    #[test]
    fn ocr_model_requires_vision() {
        let mut s = configured();
        assert_eq!(llm(resolve_ocr_target(&s, None)), Ok("vision".to_owned()));
        s.providers[0].models[1].vision = Vision::No;
        assert_eq!(resolve_ocr_target(&s, None), Err(NotReady::NoVisionModel));
        // 用户在下拉里明确选的照用
        let pick = model_ref("p", "vision");
        assert!(resolve_ocr_target(&s, Some(&pick)).is_ok());
        s.defaults.ocr = None;
        assert_eq!(resolve_ocr_target(&s, None), Err(NotReady::NoVisionModel));
        s.providers.clear();
        assert_eq!(resolve_ocr_target(&s, None), Err(NotReady::NoProvider));
    }

    fn paddle() -> Provider {
        let mut p = provider("paddle", &["PP-OCRv6", "PP-StructureV3"]);
        p.protocol = Protocol::PaddleOcr;
        p.base_url = "https://paddleocr.aistudio-app.com/api/v2/ocr/jobs".to_owned();
        p
    }

    #[test]
    fn offline_choices_come_before_bindings_and_defaults() {
        let mut s = configured();
        let styles = wisp_prompts::builtin_styles();
        let mut bound = resolve_style(&styles, "natural", "natural");
        bound.model_ref = Some(model_ref("p", "vision"));
        s.offline.translate_model = Some(LocalModelId::IndexTranslate2b);
        let r = resolve_text_model(&s, None, &bound, TaskKind::Translate).expect("offline");
        assert_eq!(r.offline_model(), Some(LocalModelId::IndexTranslate2b));
        assert_eq!(r.provider.id, OFFLINE_PROVIDER_ID);
        assert_eq!(r.prompt_profile(), PromptProfile::IndexTranslate);
        assert_eq!(r.provider.quirks.thinking_off, ThinkingOff::ChatTemplateKwargs);
        assert!(!provider_needs_key(&r.provider), "合成服务商不碰钥匙串");
        assert_eq!(r.info().model_label, "Index-Translate 2B");
        // 请求显式指定优先于离线选择
        let explicit = model_ref("p", "m");
        assert_eq!(
            resolve_text_model(&s, Some(&explicit), &bound, TaskKind::Translate).map(|r| r.model.id),
            Ok("m".to_owned())
        );
        // 显式指定离线模型（面板菜单）：没有任何服务商时也可以
        let mut bare = Settings::default();
        let pick = model_ref(OFFLINE_PROVIDER_ID, "index_translate_9b");
        assert_eq!(
            resolve_text_model(&bare, Some(&pick), &bound, TaskKind::Translate).map(|r| r.offline_model()),
            Ok(Some(LocalModelId::IndexTranslate9b))
        );
        // 用途不符的显式指定无效
        let wrong = model_ref(OFFLINE_PROVIDER_ID, "pp_ocrv6_small");
        assert_eq!(
            resolve_text_model(&bare, Some(&wrong), &bound, TaskKind::Translate),
            Err(NotReady::NoProvider)
        );
        bare.offline.translate_model = Some(LocalModelId::IndexTranslate9b);
        assert!(resolve_text_model(&bare, None, &bound, TaskKind::Translate).is_ok());
        // 同步来的风格绑定 / defaults 指向 @offline：按引用失效跳过
        s.offline.translate_model = None;
        bound.model_ref = Some(model_ref(OFFLINE_PROVIDER_ID, "index_translate_2b"));
        s.defaults.translate = Some(model_ref(OFFLINE_PROVIDER_ID, "index_translate_2b"));
        assert_eq!(
            resolve_text_model(&s, None, &bound, TaskKind::Translate),
            Err(NotReady::NoDefaultModel {
                task: TaskKind::Translate
            })
        );
    }

    #[test]
    fn paddle_ocr_providers_recognise_but_never_translate() {
        let mut s = configured();
        s.providers.push(paddle());
        s.defaults.ocr = Some(model_ref("paddle", "PP-StructureV3"));
        let target = resolve_ocr_target(&s, None).expect("cloud");
        assert!(matches!(&target, OcrTarget::Engine(EngineRef::Cloud { model, .. }) if model == "PP-StructureV3"));
        assert!(!target.can_translate());
        assert!(target.supports(OcrMode::Markdown), "文档解析模型支持 markdown");
        assert_eq!(target.key_provider().map(|p| p.id.as_str()), Some("paddle"));
        assert_eq!(target.info().provider_id, "paddle");
        let ocr_only = OcrTarget::Engine(EngineRef::Cloud {
            provider: paddle(),
            model: "PP-OCRv6".to_owned(),
        });
        assert_eq!(ocr_only.effective_mode(OcrMode::Markdown), OcrMode::Paragraphs);
        // 不能当翻译模型用：引用失效
        let styles = wisp_prompts::builtin_styles();
        let style = resolve_style(&styles, "natural", "natural");
        s.defaults.translate = Some(model_ref("paddle", "PP-OCRv6"));
        assert_eq!(
            resolve_text_model(&s, None, &style, TaskKind::Translate),
            Err(NotReady::NoDefaultModel {
                task: TaskKind::Translate
            })
        );
        let explicit = model_ref("paddle", "PP-OCRv6");
        assert!(resolve_text_model(&s, Some(&explicit), &style, TaskKind::Translate).is_err());
    }

    #[test]
    fn offline_ocr_targets() {
        let mut s = configured();
        s.offline.ocr_model = Some(LocalModelId::PpOcrV6Small);
        let t = resolve_ocr_target(&s, None).expect("offline");
        assert_eq!(t.local(), Some(LocalModelId::PpOcrV6Small));
        assert_eq!(t.key_provider(), None);
        assert!(!t.can_translate());
        assert_eq!(t.effective_mode(OcrMode::Markdown), OcrMode::Paragraphs);
        assert_eq!(t.effective_mode(OcrMode::Lines), OcrMode::Lines);
        assert_eq!(t.info().model_label, "PP-OCRv6 Small");
        assert_eq!(
            t.model_key(),
            (OFFLINE_PROVIDER_ID.to_owned(), "pp_ocrv6_small".to_owned())
        );
        // 显式指定的视觉模型优先；显式指定的 VL
        let pick = model_ref("p", "vision");
        assert!(matches!(resolve_ocr_target(&s, Some(&pick)), Ok(OcrTarget::Llm(_))));
        let vl = model_ref(OFFLINE_PROVIDER_ID, "paddleocr_vl_1_6");
        let t = resolve_ocr_target(&s, Some(&vl)).expect("vl");
        assert!(t.supports(OcrMode::Markdown));
        // 翻译模型不能当 OCR 用（设置里手改出的错值同样无效）
        s.offline.ocr_model = Some(LocalModelId::IndexTranslate2b);
        assert!(matches!(resolve_ocr_target(&s, None), Ok(OcrTarget::Llm(_))));
        // 没有任何服务商、只选了离线 OCR 模型
        let mut bare = Settings::default();
        bare.offline.ocr_model = Some(LocalModelId::PpOcrV6Tiny);
        assert_eq!(
            resolve_ocr_target(&bare, None).map(|t| t.local()),
            Ok(Some(LocalModelId::PpOcrV6Tiny))
        );
    }

    #[test]
    fn offline_plans_drop_dictionary_and_refine() {
        let s = configured();
        let styles = wisp_prompts::builtin_styles();
        let mut plan = plan_text(&s, &styles, &input("serendipity"));
        assert!(plan.dict);
        let cloud = resolve_text_model(&s, None, &plan.style, TaskKind::Translate).expect("cloud");
        adapt_for(&mut plan, &cloud);
        assert!(plan.dict, "云端模型不变");
        adapt_for(&mut plan, &offline_translate(LocalModelId::IndexTranslate2b));
        assert!(!plan.dict);
        plan.task = TaskKind::Refine;
        adapt_for(&mut plan, &offline_translate(LocalModelId::IndexTranslate2b));
        assert_eq!(plan.task, TaskKind::Translate);
        // 离线模型用官方格式的提示词：一条 user 消息、temperature 0
        let g = GlossaryMatcher::default();
        let built = build_text_prompt(&plan, "serendipity", &g, false, PromptProfile::IndexTranslate);
        assert!(built.system.is_empty());
        assert_eq!(built.temperature, Some(0.0));
        assert!(built.user_text.starts_with("请将以下"));
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

    /// 会话排版：直接翻译的截图无论视图带回什么都是原样（重新识别、「查看原文」与 commit 一致）；其余优先视图带回的
    /// 排版，没有时取设置，设置里不允许的合并为一行退回原样
    #[test]
    fn session_layout_is_keep_for_direct_translation_on_every_path() {
        let mut s = configured();
        s.ocr.layout = OcrLayout::RemoveBlankLines;
        for requested in [None, Some(OcrLayout::RemoveBlankLines), Some(OcrLayout::JoinLines)] {
            assert_eq!(
                session_ocr_layout(&s, true, requested),
                OcrLayout::Keep,
                "{requested:?}"
            );
        }
        assert_eq!(session_ocr_layout(&s, false, None), OcrLayout::RemoveBlankLines);
        assert_eq!(
            session_ocr_layout(&s, false, Some(OcrLayout::JoinLines)),
            OcrLayout::JoinLines
        );
        s.ocr.layout = OcrLayout::JoinLines;
        assert_eq!(session_ocr_layout(&s, false, None), OcrLayout::Keep);
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
