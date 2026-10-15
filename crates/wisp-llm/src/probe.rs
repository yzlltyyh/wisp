//! 测试连接：并行发 GET /models 和一个最大输出为 1 的流式探针。
//!
//! - 探针模型依次取：`provider.models` 中第一个已启用的**对话**模型（跳过嵌入、语音、图像生成等；调用方把默认翻译模型
//!   放在最前面即可满足「默认翻译模型优先」）→ 已启用的全是非对话模型时取第一个已启用的 → /models 返回的第一个
//!   对话模型 → 都没有时只测 /models；
//! - 探针发送 'ping' + stream:true + 最大输出 1，其余参数按正常规则（能力缓存、降级、重试照常生效）；
//!   400 且信息含 max_tokens / minimum 时改成 16 再试一次。最大输出的**取值**被拒（`Expected a value >= 16, but got 1
//!   instead`）不当作字段不被支持去降级；探针在请求内得出的降级结论**不写回**能力缓存（刻意构造的极小请求，结论不一定
//!   适用于正式请求，见 client.rs 的 `persist_caps`）；
//! - 成功的判定：2xx 且在首 token 超时内解析出 Start（没有文本或被截断也算成功），解析出 Start 后立即断开；
//!   流开始前就失败时事件序列是 Start → Error，以 Error 为准；
//! - 结论：两项都成功 → ok；/models 失败而流式成功 → partial（标黄）；没有可测的模型而 /models 成功 → partial；其余 failed。

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use wisp_types::{
    ErrorCode, LlmError, ModelEntry, ProbeResult, Provider, ProviderTestReport, StreamEvent, TestVerdict,
};

use crate::client::{ChatRequest, LlmClient, Purpose, Sink, UserContent, run};
use crate::errors;
use crate::models::{self, entry_from_remote, merge_models};
use crate::secret::Secret;
use crate::url::{Endpoint, endpoint_url};
use crate::vision::is_non_chat_id;

/// 探针被拒后改用的最大输出
const PROBE_RETRY_MAX_TOKENS: u32 = 16;

pub(crate) async fn test_connection(
    client: &LlmClient,
    provider: &Provider,
    key: Option<&Secret>,
) -> ProviderTestReport {
    let inner = &client.inner;
    let models_task = async {
        let t0 = Instant::now();
        let outcome = models::list(inner, provider, key).await;
        (outcome, elapsed_ms(t0))
    };
    let configured = pick_configured(&provider.models).cloned();
    let ((list, list_ms), stream) = match configured {
        Some(model) => {
            let (list, stream) = tokio::join!(models_task, probe(client, provider, key, model));
            (list, Some(stream))
        }
        None => {
            let (list, list_ms) = models_task.await;
            let picked = list.result.as_ref().ok().and_then(|remote| {
                remote
                    .iter()
                    .find(|m| !m.retired && !is_non_chat_id(&m.id))
                    .map(|m| entry_from_remote(m, true))
            });
            let stream = match picked {
                Some(model) => Some(probe(client, provider, key, model).await),
                None => None,
            };
            ((list, list_ms), stream)
        }
    };

    let models_url = endpoint_url(&provider.base_url, provider.protocol, Endpoint::Models, None);
    let list_error = list
        .result
        .as_ref()
        .err()
        .map(|e| with_version_hint(e.clone(), provider));
    let (models_result, fetched_models) = match list.result {
        Ok(remote) => (
            ProbeResult {
                ok: true,
                status: list.status,
                elapsed_ms: list_ms,
                url: models_url,
                error: None,
            },
            merge_models(&provider.models, remote, &[]),
        ),
        Err(e) => {
            let status = list.status.or(e.status);
            (
                ProbeResult {
                    ok: false,
                    status,
                    elapsed_ms: list_ms,
                    url: models_url,
                    // 流式探针没跑（或也失败了）时，/models 的 404 才可能是版本段的问题
                    error: Some(if stream.as_ref().is_some_and(|(r, _)| r.ok) {
                        e
                    } else {
                        list_error.clone().unwrap_or(e)
                    }),
                },
                Vec::new(),
            )
        }
    };
    let (stream_result, model_used) = match stream {
        Some((result, model)) => (result, Some(model)),
        None => (
            ProbeResult {
                ok: false,
                // 探针没有执行：/models 失败时沿用它的错误（Key 被拒 → auth、404 → 提示 /v1），界面据此标红 Key
                // 或给出版本段提示；/models 成功却没有对话模型时才是「没有可测的模型」
                status: list_error.as_ref().and_then(|e| e.status),
                elapsed_ms: 0,
                url: endpoint_url(&provider.base_url, provider.protocol, Endpoint::Chat, None),
                error: Some(list_error.unwrap_or_else(|| {
                    errors::not_configured("no model is available to test streaming; add a model id manually")
                })),
            },
            None,
        ),
    };
    let overall = match (models_result.ok, stream_result.ok) {
        (true, true) => TestVerdict::Ok,
        (false, true) => TestVerdict::Partial,
        (true, false) if model_used.is_none() => TestVerdict::Partial,
        _ => TestVerdict::Failed,
    };
    log::info!(
        "llm test provider={} models={:?} stream={:?} overall={overall:?}",
        provider.id,
        models_result.status,
        stream_result.status
    );
    ProviderTestReport {
        models: models_result,
        stream: stream_result,
        overall,
        model_used,
        fetched_models,
        notice: None,
    }
}

/// /models 404 且 base_url 的版本段是自动追加的（没有 `#`、原文不含版本段）：提示加 / 去 /v1。
/// 对话端点的 404 在 errors.rs 里已经这样处理；没有可测的模型时 /models 是唯一的请求，同一提示挂在它上面
fn with_version_hint(mut e: LlmError, provider: &Provider) -> LlmError {
    let n = crate::url::normalize(&provider.base_url, provider.protocol);
    if e.code == ErrorCode::NotFound && e.status == Some(404) && !n.forced && n.version_appended {
        e.subcode = Some(wisp_types::ErrorSubcode::VersionHint);
        e.hint = Some(errors::HINT_TRY_V1.to_owned());
    }
    e
}

/// 流式探针；返回 (结果, 用到的模型 id)
async fn probe(
    client: &LlmClient,
    provider: &Provider,
    key: Option<&Secret>,
    model: ModelEntry,
) -> (ProbeResult, String) {
    let t0 = Instant::now();
    let url = endpoint_url(&provider.base_url, provider.protocol, Endpoint::Chat, Some(&model.id));
    let mut max_tokens = 1;
    loop {
        let req = ChatRequest {
            request_id: format!("probe-{}", provider.id),
            provider: provider.clone(),
            key: key.map(Secret::duplicate),
            model: model.clone(),
            system: String::new(),
            user: UserContent::Text("ping".to_owned()),
            temperature: None,
            max_output_tokens: max_tokens,
            purpose: Purpose::Probe,
        };
        let mut sink = ProbeSink::default();
        let status = run(&client.inner, &req, &CancellationToken::new(), &mut sink).await;
        // 流开始前失败时 Start 之后紧跟 Error：有 Error 就以它为准
        let error = match (sink.error, sink.started) {
            (Some(e), _) => Some(e),
            (None, true) => None,
            (None, false) => Some(no_terminal()),
        };
        if let Some(e) = &error
            && max_tokens == 1
            && rejects_tiny_max_tokens(e)
        {
            max_tokens = PROBE_RETRY_MAX_TOKENS;
            continue;
        }
        let result = ProbeResult {
            ok: error.is_none(),
            status,
            elapsed_ms: elapsed_ms(t0),
            url,
            error,
        };
        return (result, model.id);
    }
}

/// 已配置的探针模型：第一个已启用的对话模型；已启用的全是非对话模型（嵌入、语音…）时退回第一个已启用的。
/// Ollama 按修改时间列出模型，`nomic-embed-text` 之类的嵌入模型常排在最前，首次获取（≤20 个全部启用）后会被选中
fn pick_configured(models: &[ModelEntry]) -> Option<&ModelEntry> {
    let mut enabled = models.iter().filter(|m| m.enabled);
    let first = enabled.clone().next();
    enabled.find(|m| !is_non_chat_id(&m.id)).or(first)
}

/// 400 且信息含 max_tokens 或 minimum（Responses 的 max_output_tokens 最小为 16）
fn rejects_tiny_max_tokens(e: &LlmError) -> bool {
    if e.status != Some(400) {
        return false;
    }
    let m = e.message.to_ascii_lowercase();
    [
        "max_tokens",
        "max_output_tokens",
        "max_completion_tokens",
        "maxoutputtokens",
        "minimum",
        "at least",
    ]
    .iter()
    .any(|w| m.contains(w))
}

fn no_terminal() -> LlmError {
    errors::make(ErrorCode::Internal, "the probe ended without a result")
}

fn elapsed_ms(t0: Instant) -> u32 {
    u32::try_from(t0.elapsed().as_millis()).unwrap_or(u32::MAX)
}

/// 探针的事件出口：看到 Start 就够了，立即停止（断开连接）；流开始前失败时随后还会收到 Error
#[derive(Default)]
struct ProbeSink {
    started: bool,
    error: Option<LlmError>,
}

impl Sink for ProbeSink {
    async fn emit(&mut self, ev: StreamEvent) -> bool {
        match ev {
            StreamEvent::Start { .. } => {
                self.started = true;
                false
            }
            StreamEvent::Error(e) => {
                self.error = Some(e);
                true
            }
            _ => true,
        }
    }

    async fn closed(&self) {
        std::future::pending::<()>().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, enabled: bool) -> ModelEntry {
        let mut m = ModelEntry::new(id);
        m.enabled = enabled;
        m
    }

    #[test]
    fn probe_model_skips_non_chat_models() {
        let models = [
            entry("qwen3:8b", false),
            entry("nomic-embed-text:latest", true),
            entry("whisper-1", true),
            entry("llama3.2:latest", true),
        ];
        assert_eq!(pick_configured(&models).map(|m| m.id.as_str()), Some("llama3.2:latest"));
        // 已启用的全是非对话模型：退回第一个已启用的（总比不测强）
        let models = [entry("gpt-4.1", false), entry("text-embedding-3-small", true)];
        assert_eq!(
            pick_configured(&models).map(|m| m.id.as_str()),
            Some("text-embedding-3-small")
        );
        assert!(pick_configured(&[entry("gpt-4.1", false)]).is_none());
    }
}
