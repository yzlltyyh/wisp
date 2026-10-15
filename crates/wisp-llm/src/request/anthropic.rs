//! Anthropic Messages 协议的请求体。
//!
//! system 放在顶层（不放进 messages）；图片为 base64 source（裸 base64 + media_type）；max_tokens 必填；
//! output_config.effort：翻译 low、OCR medium；**永远不发 temperature**；只有能力缓存确认支持时才发 thinking:{type:disabled}。

use serde_json::{Map, Value, json};

use super::params::Thinking;
use super::{Parts, insert};
use crate::degrade::{Param, SentParam};

pub(super) fn body(p: &Parts<'_>, sent: &mut Vec<SentParam>) -> Map<String, Value> {
    let plan = p.plan;
    let content = match p.image {
        Some((mime, b64)) => {
            let mut parts = vec![json!({
                "type": "image",
                "source": { "type": "base64", "media_type": mime, "data": b64 },
            })];
            if !p.user_text.is_empty() {
                parts.push(json!({ "type": "text", "text": p.user_text }));
            }
            Value::Array(parts)
        }
        None => Value::String(p.user_text.to_owned()),
    };

    let mut m = Map::new();
    insert(&mut m, "model", p.model);
    insert(&mut m, "max_tokens", plan.max_tokens);
    if !p.system.is_empty() {
        insert(&mut m, "system", p.system);
    }
    insert(&mut m, "messages", json!([{ "role": "user", "content": content }]));
    insert(&mut m, "stream", true);
    if let Some(effort) = plan.effort {
        insert(&mut m, "output_config", json!({ "effort": effort }));
        sent.push(SentParam {
            param: Param::Effort,
            names: &["output_config", "effort"],
        });
    }
    if plan.thinking == Thinking::Disabled {
        insert(&mut m, "thinking", json!({ "type": "disabled" }));
        // 400 且信息含 temperature / thinking / disabled 时去掉 thinking
        sent.push(SentParam {
            param: Param::Thinking,
            names: &["thinking", "disabled", "temperature"],
        });
    }
    m
}
