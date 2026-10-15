//! 视觉能力判定：用户手动设置（最高）> 运行时反馈 > 能力字段 > 关键词。
//!
//! 关键词规则覆盖各厂商的公开模型命名，另外补齐了名字里不带 vision 字样的视觉模型（Ollama 的 `qwen2.5vl`、`llama3.2-vision`，
//! 火山的 `glm-5-3-flash` / `deepseek-v4-1-flash`、`doubao-seed-evolving`，`nemotron3`）。Rust 的 regex 不支持前瞻，
//! NEGATIVE 的 `(?![a-z])` 改写为 `(?:[^a-z]|$)`；`glm-4-` 按前缀匹配；NEGATIVE 补上 `live`，POSITIVE 补上 `o3-pro`。
//! 正则在第一次判定时才编译（常驻进程启动时不做多余工作）。

use std::sync::LazyLock;

use regex::Regex;
use wisp_types::{Vision, VisionSource};

/// NEGATIVE：命中即 No（优先于 POSITIVE）
static NEGATIVE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    compile(concat!(
        r"(?:^|[-_:])(?:",
        r"tts|transcribe|whisper|realtime|audio|embed|embedding|rerank|reranker|moderation|live|",
        r"image-gen|dall-e|imagen|veo|sora|gpt-image|flash-image|pro-image|",
        r"o1-mini|o3-mini|codex|coder|math|",
        r"deepseek-v4-pro|deepseek-chat|deepseek-reasoner|deepseek-r1|",
        r"minimax-m2|minimax-m1|text-01|",
        r"glm-4\.5(?:-air)?$|glm-4\.6$|glm-4\.7|gpt-oss|llama-?3\.[0-1]|",
        r"qwen3-(?:0\.6|1\.7|4|8|14|32|30|235)b",
        r")(?:[^a-z]|$)",
        // glm-4- 系列（glm-4-plus / glm-4-flash / glm-4-0520 …）按前缀匹配
        r"|(?:^|[-_:])glm-4-",
    ))
});

/// POSITIVE：命中即 Yes
static POSITIVE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    compile(concat!(
        r"gpt-4o|gpt-4\.1|gpt-5|gpt-6|chatgpt-4o|^o1$|^o3$|^o3-pro|^o4|o1-20|o3-20|o4-mini|",
        r"claude-(?:3|4|5|opus|sonnet|haiku|fable|mythos)|",
        r"gemini-(?:1\.5|[2-9]|[1-9][0-9])|gemini-omni|",
        r"deepseek-flash|deepseek-v4[-.0-9]*-flash|vision|",
        r"qwen[0-9.]*-?vl|qvq|qwen3\.[5-9]|qwen3\.[1-9][0-9]|",
        r"glm-4v|glm-4\.[1-9]v|glm-5v|glm-5[.-]3-flash|",
        r"doubao-seed-(?:1-6|1\.6|2|evolving)|doubao.*vision|seed-1-6|",
        r"kimi-k3|kimi-k2\.[6-9]|kimi-latest|moonshot.*vision|",
        r"minimax-m3|",
        r"llama-4|llama-?3\.2-.*vision|llava|minicpm-v|gemma3|gemma4|mistral-small3|pixtral|granite.*vision|",
        r"internvl|phi-4-multimodal|phi-3\.5-vision|molmo|idefics|paligemma|smolvlm|glm-ocr|medgemma|nemotron3",
    ))
});

/// 明显不是对话模型的 id（测试连接挑选探针模型时跳过）
static NON_CHAT: LazyLock<Option<Regex>> = LazyLock::new(|| {
    compile(concat!(
        r"(?:^|[-_:])(?:tts|transcribe|whisper|realtime|audio|embed|embedding|rerank|reranker|moderation|live|",
        r"image-gen|dall-e|imagen|veo|sora|gpt-image|flash-image|pro-image|computer-use)(?:[^a-z]|$)",
        r"|^(?:babbage|davinci|text-embedding|omni-moderation)",
    ))
});

static REASONING: LazyLock<Option<Regex>> = LazyLock::new(|| compile(r"^(?:o[0-9]|gpt-5|gpt-6)"));

fn compile(pattern: &str) -> Option<Regex> {
    // 模式是编译期常量，由单测保证合法；万一失败只记日志并视为「不匹配」，不让常驻进程崩溃
    Regex::new(pattern)
        .inspect_err(|e| log::error!("vision regex failed to compile: {e}"))
        .ok()
}

fn is_match(re: &LazyLock<Option<Regex>>, id: &str) -> bool {
    re.as_ref().is_some_and(|r| r.is_match(id))
}

/// 小写并去掉到最后一个 `/` 为止的 provider 前缀（`openai/gpt-5` → `gpt-5`，`models/gemini-x` → `gemini-x`）
pub(crate) fn bare_id(model_id: &str) -> String {
    let lower = model_id.trim().to_ascii_lowercase();
    match lower.rfind('/') {
        Some(i) => lower[i + 1..].to_owned(),
        None => lower,
    }
}

/// 按模型 id 的关键词判定视觉能力（去掉 provider 前缀后匹配）
pub fn keyword_vision(model_id: &str) -> Vision {
    let id = bare_id(model_id);
    if id.is_empty() {
        Vision::Unknown
    } else if is_match(&NEGATIVE, &id) {
        Vision::No
    } else if is_match(&POSITIVE, &id) {
        Vision::Yes
    } else {
        Vision::Unknown
    }
}

/// 是否为推理模型 id：`^(o[0-9]|gpt-5|gpt-6)`（小写、去掉 provider 前缀后匹配）
pub fn is_reasoning_id(model_id: &str) -> bool {
    is_match(&REASONING, &bare_id(model_id))
}

/// 是否明显不是对话模型（嵌入、语音、图像生成等）
pub(crate) fn is_non_chat_id(model_id: &str) -> bool {
    is_match(&NON_CHAT, &bare_id(model_id))
}

/// 刷新模型列表时决定一个模型的视觉能力。
/// `current`：已有条目的 (vision, vision_source)；`capability`：上游能力字段给出的确定值。
/// 用户手动设置与运行时反馈保留不动；其余按「能力字段 > 关键词」重新计算（关键词规则随版本更新）
pub(crate) fn resolve(
    model_id: &str,
    current: Option<(Vision, Option<VisionSource>)>,
    capability: Option<Vision>,
) -> (Vision, Option<VisionSource>) {
    match current {
        Some((v, Some(src @ (VisionSource::User | VisionSource::Runtime)))) => (v, Some(src)),
        _ => match capability {
            Some(v) if v != Vision::Unknown => (v, Some(VisionSource::Capability)),
            _ => match current {
                Some((v, Some(VisionSource::Capability))) => (v, Some(VisionSource::Capability)),
                _ => match keyword_vision(model_id) {
                    Vision::Unknown => (Vision::Unknown, None),
                    v => (v, Some(VisionSource::Keyword)),
                },
            },
        },
    }
}

/// 运行时反馈是否需要写回：不覆盖用户的手动设置，结论没变时不重复写。
/// `conclusive`：结论是否确定。成功（Yes）总是确定的；no_vision 只有服务端明确说「不支持图片输入」时才确定，
/// 不确定的 No 只写给还是 Unknown 的模型，不覆盖能力字段、关键词或上一次运行时反馈给出的已知结论
pub(crate) fn runtime_update_needed(
    current: Vision,
    source: Option<VisionSource>,
    observed: Vision,
    conclusive: bool,
) -> bool {
    match source {
        Some(VisionSource::User) => false,
        _ if !conclusive => current == Vision::Unknown && observed != Vision::Unknown,
        Some(VisionSource::Runtime | VisionSource::Capability) => current != observed,
        Some(VisionSource::Keyword) | None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regexes_compile() {
        // 编译失败时 compile() 返回 None（视为不匹配），这里确保全部规则都编译成功
        assert!(NEGATIVE.is_some() && POSITIVE.is_some() && NON_CHAT.is_some() && REASONING.is_some());
        assert!(is_match(&NEGATIVE, "tts"));
        assert!(is_match(&POSITIVE, "gpt-4o"));
        assert!(is_match(&NON_CHAT, "text-embedding-3-small"));
        assert!(is_match(&REASONING, "o3"));
    }

    #[test]
    fn prefix_and_case_are_ignored() {
        assert_eq!(keyword_vision("OpenAI/GPT-4o"), Vision::Yes);
        assert_eq!(keyword_vision("models/gemini-2.5-flash"), Vision::Yes);
        assert_eq!(keyword_vision("Pro/deepseek-ai/DeepSeek-R1"), Vision::No);
        assert_eq!(keyword_vision(""), Vision::Unknown);
        assert_eq!(keyword_vision("ep-20250101-abcde"), Vision::Unknown);
    }

    #[test]
    fn negative_wins_over_positive() {
        assert_eq!(keyword_vision("gpt-4o-mini-tts"), Vision::No);
        assert_eq!(keyword_vision("gpt-4o-realtime-preview"), Vision::No);
        assert_eq!(keyword_vision("gemini-2.5-flash-image"), Vision::No);
        assert_eq!(keyword_vision("gemini-live-2.5-flash-preview"), Vision::No);
        assert_eq!(keyword_vision("gpt-5.1-codex"), Vision::No);
        assert_eq!(keyword_vision("glm-4-plus"), Vision::No);
        assert_eq!(keyword_vision("glm-4.5v"), Vision::Yes);
        assert_eq!(keyword_vision("glm-4.6v-flash"), Vision::Yes);
        assert_eq!(keyword_vision("bge-reranker-v2-m3"), Vision::No);
    }

    #[test]
    fn reasoning_ids() {
        assert!(is_reasoning_id("gpt-5.4-mini"));
        assert!(is_reasoning_id("openai/gpt-5"));
        assert!(is_reasoning_id("o4-mini"));
        assert!(is_reasoning_id("GPT-6-astra"));
        assert!(!is_reasoning_id("gpt-4.1"));
        assert!(!is_reasoning_id("gpt-4o"));
        assert!(!is_reasoning_id("omni-moderation-latest"));
    }

    #[test]
    fn resolve_priority() {
        use Vision::{No, Unknown, Yes};
        use VisionSource::{Capability, Keyword, Runtime, User};
        // 用户设置最高：能力字段、关键词都不覆盖
        assert_eq!(resolve("gpt-4o", Some((No, Some(User))), Some(Yes)), (No, Some(User)));
        // 运行时反馈高于能力字段
        assert_eq!(
            resolve("claude-x", Some((No, Some(Runtime))), Some(Yes)),
            (No, Some(Runtime))
        );
        // 能力字段高于关键词
        assert_eq!(resolve("gpt-oss-120b", None, Some(Yes)), (Yes, Some(Capability)));
        assert_eq!(
            resolve("gpt-4o", Some((Yes, Some(Keyword))), Some(No)),
            (No, Some(Capability))
        );
        // 上游这次没给能力字段时保留上一次的能力字段结论
        assert_eq!(
            resolve("relay-x", Some((Yes, Some(Capability))), None),
            (Yes, Some(Capability))
        );
        // 只有关键词
        assert_eq!(resolve("gpt-4o", None, None), (Yes, Some(Keyword)));
        assert_eq!(resolve("whisper-1", Some((Unknown, None)), None), (No, Some(Keyword)));
        assert_eq!(resolve("my-alias", None, Some(Unknown)), (Unknown, None));
    }

    #[test]
    fn runtime_feedback_respects_user() {
        use VisionSource::{Capability, Keyword, Runtime, User};
        assert!(!runtime_update_needed(Vision::Yes, Some(User), Vision::No, true));
        assert!(!runtime_update_needed(Vision::Yes, Some(Capability), Vision::Yes, true));
        assert!(runtime_update_needed(Vision::Yes, Some(Capability), Vision::No, true));
        assert!(!runtime_update_needed(Vision::Yes, Some(Runtime), Vision::Yes, true));
        assert!(runtime_update_needed(Vision::Yes, Some(Runtime), Vision::No, true));
        assert!(runtime_update_needed(Vision::Unknown, None, Vision::Yes, true));
        assert!(runtime_update_needed(Vision::Yes, Some(Keyword), Vision::Yes, true));
    }

    #[test]
    fn inconclusive_no_only_fills_unknown() {
        use VisionSource::{Capability, Keyword, Runtime, User};
        // 已知支持视觉（能力字段 / 关键词 / 上一次成功）的模型不会被一个含糊的图片错误改成 No
        assert!(!runtime_update_needed(Vision::Yes, Some(Capability), Vision::No, false));
        assert!(!runtime_update_needed(Vision::Yes, Some(Keyword), Vision::No, false));
        assert!(!runtime_update_needed(Vision::Yes, Some(Runtime), Vision::No, false));
        assert!(!runtime_update_needed(Vision::Unknown, Some(User), Vision::No, false));
        // 还不知道的模型照常记为 No
        assert!(runtime_update_needed(Vision::Unknown, None, Vision::No, false));
        assert!(!runtime_update_needed(Vision::No, Some(Keyword), Vision::No, false));
    }
}
