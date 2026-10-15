//! OpenAI Chat Completions 协议的请求体。
//!
//! messages：role=system（不用 developer）+ user（纯文本为字符串；带图时为 [image_url **对象**, text]）；
//! stream:true + stream_options；max_completion_tokens 或 max_tokens；可选 temperature / reasoning_effort；
//! 再合并 quirks.thinking_off 对应的厂商字段。

use serde_json::{Map, Value, json};
use wisp_types::MaxTokensField;

use super::params::Thinking;
use super::{Parts, data_url, insert};
use crate::degrade::{Param, SentParam};

pub(super) fn body(p: &Parts<'_>, sent: &mut Vec<SentParam>) -> Map<String, Value> {
    let plan = p.plan;
    let (user_text, system) = p.user_and_system();
    let mut messages = Vec::with_capacity(2);
    if let Some(system) = system {
        messages.push(json!({ "role": "system", "content": system }));
    }
    let content = match p.image {
        Some((mime, b64)) => {
            let mut parts = vec![json!({
                "type": "image_url",
                "image_url": { "url": data_url(mime, b64), "detail": plan.image_detail },
            })];
            if !user_text.is_empty() {
                parts.push(json!({ "type": "text", "text": user_text }));
            }
            Value::Array(parts)
        }
        None => Value::String(user_text),
    };
    messages.push(json!({ "role": "user", "content": content }));

    let mut m = Map::new();
    insert(&mut m, "model", p.model);
    insert(&mut m, "messages", messages);
    insert(&mut m, "stream", true);
    if plan.stream_options {
        let mut so = Map::new();
        insert(&mut so, "include_usage", true);
        if plan.include_obfuscation {
            insert(&mut so, "include_obfuscation", false);
            sent.push(SentParam {
                param: Param::Obfuscation,
                names: &["include_obfuscation", "obfuscation"],
            });
        }
        insert(&mut m, "stream_options", so);
        sent.push(SentParam {
            param: Param::StreamOptions,
            names: &["stream_options", "include_usage"],
        });
    }
    match plan.max_field {
        MaxTokensField::MaxCompletionTokens => {
            insert(&mut m, "max_completion_tokens", plan.max_tokens);
            sent.push(SentParam {
                param: Param::MaxTokens(MaxTokensField::MaxCompletionTokens),
                names: &["max_completion_tokens"],
            });
        }
        MaxTokensField::MaxTokens => {
            insert(&mut m, "max_tokens", plan.max_tokens);
            sent.push(SentParam {
                param: Param::MaxTokens(MaxTokensField::MaxTokens),
                names: &["max_tokens"],
            });
        }
    }
    if let Some(t) = plan.temperature {
        insert(&mut m, "temperature", t);
        sent.push(SentParam {
            param: Param::Temperature,
            names: &["temperature"],
        });
    }
    if let Some(effort) = plan.effort {
        insert(&mut m, "reasoning_effort", effort);
        sent.push(SentParam {
            param: Param::Effort,
            names: &["reasoning_effort"],
        });
    }
    compat_thinking(&mut m, plan.thinking, false, sent);
    m
}

/// 兼容端点的关闭思考字段（Chat 与 Responses 共用；Kimi 在 Responses 上写成 reasoning.effort）
pub(super) fn compat_thinking(
    m: &mut Map<String, Value>,
    thinking: Thinking,
    responses: bool,
    sent: &mut Vec<SentParam>,
) {
    match thinking {
        Thinking::Disabled => {
            insert(m, "thinking", json!({ "type": "disabled" }));
            sent.push(SentParam {
                param: Param::Thinking,
                names: &["thinking"],
            });
        }
        Thinking::EnableFalse => {
            insert(m, "enable_thinking", false);
            sent.push(SentParam {
                param: Param::Thinking,
                names: &["enable_thinking"],
            });
        }
        Thinking::KimiLow if responses => {
            insert(m, "reasoning", json!({ "effort": "low" }));
            sent.push(SentParam {
                param: Param::Thinking,
                names: &["reasoning.effort", "reasoning"],
            });
        }
        Thinking::KimiLow => {
            insert(m, "reasoning_effort", "low");
            sent.push(SentParam {
                param: Param::Thinking,
                names: &["reasoning_effort"],
            });
        }
        Thinking::None | Thinking::GeminiLevelLow | Thinking::GeminiBudgetZero => {}
    }
}
