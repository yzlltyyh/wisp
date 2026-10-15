//! 视觉能力判定：各厂商公开模型 id 的关键词判定表。
//! `SUPPORTED`（支持图片）→ Yes；`NOT_SUPPORTED`（不支持或未知）→ 不是 Yes（No 或 Unknown）；未经核实的模型不列。
//! OpenRouter 一行以能力字段为准（见 models.rs 的单测），这里不列。

use wisp_llm::Vision;
use wisp_llm::vision::{is_reasoning_id, keyword_vision};

const SUPPORTED: &[&str] = &[
    // OpenAI
    "gpt-6-astra",
    "gpt-6-sol",
    "gpt-6-luna",
    "gpt-5.6-mini",
    "gpt-5.5",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.4-nano",
    "gpt-5.3-chat-latest",
    "gpt-5.2",
    "gpt-5.2-pro",
    "gpt-5.2-chat-latest",
    "gpt-5.1",
    "gpt-5",
    "gpt-5-mini",
    "gpt-5-nano",
    "gpt-5-chat-latest",
    "gpt-4.1",
    "gpt-4.1-mini",
    "gpt-4.1-nano",
    "gpt-4o",
    "gpt-4o-mini",
    "o3",
    "o3-pro",
    "o4-mini",
    "o1",
    // Anthropic（全部当前模型与遗留模型）
    "claude-fable-5-1",
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "claude-haiku-5-5",
    "claude-opus-5",
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-opus-4-6",
    "claude-opus-4-5",
    "claude-sonnet-4-6",
    "claude-sonnet-4-5",
    "claude-haiku-4-5",
    "claude-3-5-sonnet-20241022",
    "claude-3-haiku-20240307",
    // Gemini（全部对话模型）
    "gemini-3.8-flash",
    "gemini-3.7-flash",
    "gemini-3.6-flash",
    "gemini-3.5-flash",
    "gemini-3.5-flash-lite",
    "gemini-3.1-pro-preview",
    "gemini-3.1-flash-lite",
    "gemini-3-flash-preview",
    "gemini-2.5-pro",
    "gemini-2.5-flash",
    "gemini-2.5-flash-lite",
    "gemini-2.0-flash",
    "gemini-omni-flash",
    "models/gemini-2.5-flash",
    // DeepSeek
    "deepseek-flash",
    "deepseek-v4-flash",
    "deepseek-v4-flash-vision-exp",
    // 通义
    "qwen3-vl-plus",
    "qwen3-vl-flash",
    "qwen3-vl-235b-a22b-instruct",
    "qwen3-vl-30b-a3b-thinking",
    "qwen2.5-vl-72b-instruct",
    "qwen-vl-max",
    "qwen-vl-plus",
    "qwen3.5-plus",
    "qwen3.6-flash",
    "qwen3.7-max",
    "qwen3.8-plus",
    "qvq-max",
    // 智谱
    "glm-5.3-flash",
    "glm-5v-turbo",
    "glm-4.6v",
    "glm-4.6v-flash",
    "glm-4.5v",
    "glm-4.1v-thinking",
    "glm-4.1v-thinking-flash",
    "glm-4.1v-thinking-flashx",
    "glm-4v-flash",
    // 火山方舟
    "doubao-seed-2-1-pro",
    "doubao-seed-2-1-lite",
    "doubao-seed-2-1-turbo",
    "doubao-seed-2-0-pro",
    "doubao-seed-2-0-lite",
    "doubao-seed-2-0-mini",
    "doubao-seed-2-0-code",
    "doubao-seed-evolving",
    "doubao-seed-1-6-250615",
    "doubao-seed-1-6-flash-250715",
    "doubao-1-5-vision-pro",
    "doubao-1-5-thinking-vision-pro",
    "glm-5-3-flash",
    "deepseek-v4-1-flash",
    // Moonshot / Kimi
    "kimi-k3",
    "kimi-k2.7-code",
    "kimi-k2.7-code-highspeed",
    "kimi-k2.6",
    "kimi-latest",
    "moonshot-v1-8k-vision-preview",
    // MiniMax
    "MiniMax-M3",
    // Groq
    "qwen/qwen3.8-27b",
    "meta-llama/llama-4-scout-17b-16e-instruct",
    "meta-llama/llama-4-maverick-17b-128e-instruct",
    // Ollama（本地）
    "qwen3-vl:8b",
    "qwen3.5:9b",
    "qwen3.6",
    "qwen3.8",
    "qwen2.5vl:7b",
    "kimi-k3:latest",
    "kimi-k2.6",
    "kimi-k2.7-code",
    "minicpm-v4.5",
    "minicpm-v4.6",
    "gemma3:27b",
    "gemma4",
    "llava:13b",
    "llava-llama3",
    "llama3.2-vision:11b",
    "mistral-small3.1:24b",
    "mistral-small3.2",
    "granite3.2-vision",
    "glm-ocr",
    "medgemma:4b",
    "minimax-m3",
    "nemotron3",
];

const NOT_SUPPORTED: &[&str] = &[
    // OpenAI
    "o3-mini",
    "o1-mini",
    "gpt-4o-mini-tts",
    "gpt-4o-transcribe",
    "gpt-4o-realtime-preview",
    "gpt-4o-audio-preview",
    "text-embedding-3-large",
    "gpt-image-1",
    "whisper-1",
    "dall-e-3",
    // Gemini
    "gemini-2.5-flash-preview-tts",
    "gemini-live-2.5-flash-preview",
    "gemini-2.5-flash-native-audio-preview-09-2025",
    "gemini-2.5-flash-image",
    "gemini-embedding-001",
    "embedding-001",
    "imagen-4.0-generate-001",
    "veo-3.0-generate-preview",
    // DeepSeek
    "deepseek-v4-pro",
    "deepseek-chat",
    "deepseek-reasoner",
    // 通义
    "qwen-coder-plus",
    "qwen3-coder-plus",
    "qwen-math-plus",
    "qwen-audio-turbo",
    // 智谱
    "glm-4.7",
    "glm-4.6",
    "glm-4.5",
    "glm-4.5-air",
    "glm-4-plus",
    "glm-4-flash",
    // 火山方舟
    "doubao-embedding-large",
    // Moonshot / Kimi
    "kimi-k2",
    "kimi-k2-thinking",
    "moonshot-v1-8k",
    "moonshot-v1-32k",
    "moonshot-v1-128k",
    // MiniMax
    "MiniMax-M2.5",
    "MiniMax-M2",
    "MiniMax-M1",
    "MiniMax-Text-01",
    // Groq
    "llama-3.3-70b-versatile",
    "llama-3.1-8b-instant",
    "openai/gpt-oss-120b",
    "openai/gpt-oss-20b",
    "whisper-large-v3",
    // Ollama
    "gpt-oss:20b",
    "deepseek-r1:8b",
    "llama3.1:8b",
    "qwen3:8b",
];

#[test]
fn supported_ids_are_yes() {
    let wrong: Vec<(&str, Vision)> = SUPPORTED
        .iter()
        .map(|id| (*id, keyword_vision(id)))
        .filter(|(_, v)| *v != Vision::Yes)
        .collect();
    assert!(wrong.is_empty(), "should be Yes: {wrong:?}");
}

#[test]
fn unsupported_ids_are_not_yes() {
    let wrong: Vec<&str> = NOT_SUPPORTED
        .iter()
        .copied()
        .filter(|id| keyword_vision(id) == Vision::Yes)
        .collect();
    assert!(wrong.is_empty(), "should not be Yes: {wrong:?}");
}

#[test]
fn explicit_negatives() {
    // NEGATIVE 命中即 No（不是 Unknown）
    for id in [
        "o3-mini",
        "gpt-4o-mini-tts",
        "gemini-2.5-flash-image",
        "gemini-live-2.5-flash-preview",
        "deepseek-v4-pro",
        "MiniMax-M2.5",
        "glm-4-plus",
        "gpt-oss:20b",
        "text-embedding-3-large",
    ] {
        assert_eq!(keyword_vision(id), Vision::No, "{id}");
    }
    // 名字里看不出来的别名与接入点 → Unknown（火山的 ep- 接入点为 unknown）
    for id in ["ep-20250918-x7k2p", "my-relay-alias", "kimi-k2", "qwen3:8b"] {
        assert_eq!(keyword_vision(id), Vision::Unknown, "{id}");
    }
}

#[test]
fn reasoning_regex() {
    assert!(is_reasoning_id("gpt-5.4-mini"));
    assert!(is_reasoning_id("openai/gpt-5"));
    assert!(is_reasoning_id("o3"));
    assert!(!is_reasoning_id("gpt-4.1"));
    assert!(!is_reasoning_id("claude-opus-5-5"));
}
