//! Google Gemini（generateContent）协议的请求体。
//!
//! 字段全部 camelCase：systemInstruction、contents[{role:user, parts:[inlineData{mimeType,data}, text]}]、
//! generationConfig{temperature, maxOutputTokens, thinkingConfig?}；不发 includeThoughts 与 safetySettings。

use serde_json::{Map, Value, json};

use super::params::Thinking;
use super::{Parts, insert};
use crate::degrade::{Param, SentParam};

pub(super) fn body(p: &Parts<'_>, sent: &mut Vec<SentParam>) -> Map<String, Value> {
    let plan = p.plan;
    let mut parts = Vec::with_capacity(2);
    if let Some((mime, b64)) = p.image {
        parts.push(json!({ "inlineData": { "mimeType": mime, "data": b64 } }));
    }
    if !p.user_text.is_empty() || p.image.is_none() {
        parts.push(json!({ "text": p.user_text }));
    }

    let mut config = Map::new();
    if let Some(t) = plan.temperature {
        insert(&mut config, "temperature", t);
        sent.push(SentParam {
            param: Param::Temperature,
            names: &["temperature"],
        });
    }
    insert(&mut config, "maxOutputTokens", plan.max_tokens);
    let thinking = match plan.thinking {
        Thinking::GeminiLevelLow => Some(json!({ "thinkingLevel": "LOW" })),
        Thinking::GeminiBudgetZero => Some(json!({ "thinkingBudget": 0 })),
        Thinking::None
        | Thinking::Disabled
        | Thinking::EnableFalse
        | Thinking::KimiLow
        | Thinking::TemplateKwargsOff => None,
    };
    if let Some(t) = thinking {
        insert(&mut config, "thinkingConfig", t);
        sent.push(SentParam {
            param: Param::Thinking,
            names: &[
                "thinkingconfig",
                "thinking_config",
                "thinkinglevel",
                "thinking_level",
                "thinkingbudget",
                "thinking_budget",
                "thinking",
            ],
        });
    }

    let mut m = Map::new();
    if !p.system.is_empty() {
        insert(&mut m, "systemInstruction", json!({ "parts": [{ "text": p.system }] }));
    }
    insert(&mut m, "contents", json!([{ "role": "user", "parts": parts }]));
    insert(&mut m, "generationConfig", config);
    m
}
