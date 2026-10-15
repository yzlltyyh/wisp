//! OpenAI Responses 协议的请求体。
//!
//! instructions = system；input = [{role:user, content:[input_image（image_url 是**字符串**）, input_text]}]；
//! max_output_tokens；stream:true；**store:false**（不在服务端留存）；推理模型发 reasoning:{effort:"low"}。

use serde_json::{Map, Value, json};

use super::openai_chat::compat_thinking;
use super::{Parts, data_url, insert};
use crate::degrade::{Param, SentParam};

pub(super) fn body(p: &Parts<'_>, sent: &mut Vec<SentParam>) -> Map<String, Value> {
    let plan = p.plan;
    let (user_text, system) = p.user_and_system();
    let mut content = Vec::with_capacity(2);
    if let Some((mime, b64)) = p.image {
        content.push(json!({ "type": "input_image", "image_url": data_url(mime, b64), "detail": plan.image_detail }));
    }
    if !user_text.is_empty() || p.image.is_none() {
        content.push(json!({ "type": "input_text", "text": user_text }));
    }

    let mut m = Map::new();
    insert(&mut m, "model", p.model);
    if let Some(system) = system {
        insert(&mut m, "instructions", system);
    }
    insert(&mut m, "input", json!([{ "role": "user", "content": content }]));
    insert(&mut m, "max_output_tokens", plan.max_tokens);
    insert(&mut m, "stream", true);
    insert(&mut m, "store", false);
    if let Some(t) = plan.temperature {
        insert(&mut m, "temperature", t);
        sent.push(SentParam {
            param: Param::Temperature,
            names: &["temperature"],
        });
    }
    if let Some(effort) = plan.effort {
        insert(&mut m, "reasoning", json!({ "effort": effort }));
        sent.push(SentParam {
            param: Param::Effort,
            names: &["reasoning.effort", "reasoning"],
        });
    }
    compat_thinking(&mut m, plan.thinking, true, sent);
    m
}
