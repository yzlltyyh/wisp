//! 假模型（冒烟测试和演示用）：固定文本、可配置延迟，不发任何网络请求。
//!
//! 事件语义与 [`LlmClient::stream`](crate::LlmClient::stream) 相同：Start → Delta* → Done；cancel 触发后立即发出
//! `Error{cancelled}`，之后不再发任何事件（开始前就取消时是 Start → Error）；接收端被丢弃时任务随即结束。

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use wisp_types::{StreamEvent, Usage};

use crate::client::{ChatRequest, UserContent};
use crate::errors;

/// 假模型的输出参数
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeConfig {
    /// 完整输出文本
    pub text: String,
    /// 每个 Delta 的字符数
    pub chunk_chars: usize,
    /// 首个 Delta 之前的延迟
    pub first_token_delay: Duration,
    /// 相邻 Delta 之间的延迟
    pub chunk_delay: Duration,
}

impl Default for FakeConfig {
    fn default() -> Self {
        Self {
            text: "敏捷的棕色狐狸跳过了懒狗。".to_owned(),
            chunk_chars: 2,
            first_token_delay: Duration::from_millis(300),
            chunk_delay: Duration::from_millis(20),
        }
    }
}

/// 假模型
#[derive(Debug, Clone, Default)]
pub struct FakeLlm {
    cfg: FakeConfig,
}

impl FakeLlm {
    /// 以给定参数构造
    pub fn new(cfg: FakeConfig) -> Self {
        Self { cfg }
    }

    /// 当前参数
    pub fn config(&self) -> &FakeConfig {
        &self.cfg
    }

    /// 与 `LlmClient::stream` 相同的事件语义（Start → Delta* → Done；取消时 Error{cancelled}）。必须在 tokio 运行时里调用
    pub fn stream(&self, req: ChatRequest, cancel: CancellationToken) -> mpsc::Receiver<StreamEvent> {
        let (tx, rx) = mpsc::channel(16);
        let cfg = self.cfg.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(play(cfg, req, cancel, tx));
            }
            Err(_) => {
                let _ = tx.try_send(start_event(&req));
                let _ = tx.try_send(StreamEvent::Error(errors::internal(
                    "FakeLlm::stream must be called inside a tokio runtime",
                )));
            }
        }
        rx
    }
}

fn start_event(req: &ChatRequest) -> StreamEvent {
    let label = req
        .model
        .label
        .clone()
        .filter(|l| !l.trim().is_empty())
        .unwrap_or_else(|| req.model.id.clone());
    StreamEvent::Start {
        request_id: req.request_id.clone(),
        provider_id: req.provider.id.clone(),
        model_id: req.model.id.clone(),
        model_label: label,
        upstream_id: None,
    }
}

async fn play(cfg: FakeConfig, req: ChatRequest, cancel: CancellationToken, tx: mpsc::Sender<StreamEvent>) {
    let t0 = Instant::now();
    // Start 总是先发（通道是空的，不会阻塞）；开始前就取消时紧跟 Error{cancelled}
    if tx.send(start_event(&req)).await.is_err() {
        return;
    }
    if cancel.is_cancelled() {
        let _ = tx.send(StreamEvent::Error(errors::cancelled())).await;
        return;
    }
    let chars: Vec<char> = cfg.text.chars().collect();
    let chunks: Vec<String> = chars
        .chunks(cfg.chunk_chars.max(1))
        .map(|c| c.iter().collect())
        .collect();
    for (i, chunk) in chunks.into_iter().enumerate() {
        let delay = if i == 0 { cfg.first_token_delay } else { cfg.chunk_delay };
        let slept = tokio::select! {
            biased;
            () = cancel.cancelled() => false,
            () = tx.closed() => return,
            () = tokio::time::sleep(delay) => true,
        };
        if !slept || !send(&tx, StreamEvent::Delta { text: chunk }, &cancel).await {
            let _ = tx.send(StreamEvent::Error(errors::cancelled())).await;
            return;
        }
    }
    let input = match &req.user {
        UserContent::Text(t) => estimate_tokens(&req.system) + estimate_tokens(t),
        UserContent::Image { text, .. } => estimate_tokens(&req.system) + estimate_tokens(text) + 1_000,
    };
    let output = estimate_tokens(&cfg.text);
    let done = StreamEvent::Done {
        elapsed_ms: u32::try_from(t0.elapsed().as_millis()).unwrap_or(u32::MAX),
        usage: Some(Usage {
            input_tokens: input,
            output_tokens: output,
            reasoning_tokens: None,
            total_tokens: Some(input.saturating_add(output)),
        }),
        truncated: None,
        cached: false,
        no_text: cfg.text.trim().is_empty(),
    };
    let _ = tx.send(done).await;
}

/// 发送一个事件；被取消或接收端离开时返回 false
async fn send(tx: &mpsc::Sender<StreamEvent>, ev: StreamEvent, cancel: &CancellationToken) -> bool {
    tokio::select! {
        biased;
        () = cancel.cancelled() => false,
        r = tx.send(ev) => r.is_ok(),
    }
}

/// 与 wisp-prompts 的估算口径一致：CJK 每字计 1，其余字符每 4 个计 1，向上取整
fn estimate_tokens(text: &str) -> u32 {
    let (cjk, other) = text.chars().fold(
        (0u32, 0u32),
        |(c, o), ch| {
            if is_cjk(ch) { (c + 1, o) } else { (c, o + 1) }
        },
    );
    cjk + other.div_ceil(4)
}

fn is_cjk(c: char) -> bool {
    matches!(u32::from(c), 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF)
}

#[cfg(test)]
mod tests {
    use wisp_types::{ErrorCode, ModelEntry};

    use super::*;
    use crate::client::Purpose;

    fn request() -> ChatRequest {
        ChatRequest {
            request_id: "r-1".to_owned(),
            provider: crate::templates::provider_from_template("openai", &[]).expect("template"),
            key: None,
            model: ModelEntry::new("fake-model"),
            system: "s".to_owned(),
            user: UserContent::Text("hello".to_owned()),
            temperature: None,
            max_output_tokens: 64,
            purpose: Purpose::Translate,
        }
    }

    fn fast() -> FakeLlm {
        FakeLlm::new(FakeConfig {
            text: "你好世界".to_owned(),
            chunk_chars: 1,
            first_token_delay: Duration::from_millis(5),
            chunk_delay: Duration::from_millis(1),
        })
    }

    #[tokio::test]
    async fn plays_full_sequence() {
        let mut rx = fast().stream(request(), CancellationToken::new());
        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev);
        }
        assert!(matches!(events.first(), Some(StreamEvent::Start { model_label, .. }) if model_label == "fake-model"));
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "你好世界");
        assert!(matches!(
            events.last(),
            Some(StreamEvent::Done {
                no_text: false,
                usage: Some(_),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn cancel_ends_with_cancelled_only() {
        let cancel = CancellationToken::new();
        let llm = FakeLlm::new(FakeConfig {
            first_token_delay: Duration::from_secs(30),
            ..FakeConfig::default()
        });
        let mut rx = llm.stream(request(), cancel.clone());
        assert!(matches!(rx.recv().await, Some(StreamEvent::Start { .. })));
        cancel.cancel();
        let ev = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("prompt");
        assert!(matches!(ev, Some(StreamEvent::Error(e)) if e.code == ErrorCode::Cancelled));
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn cancelled_before_start_is_still_a_valid_sequence() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut rx = fast().stream(request(), cancel);
        assert!(matches!(rx.recv().await, Some(StreamEvent::Start { .. })));
        assert!(matches!(rx.recv().await, Some(StreamEvent::Error(e)) if e.code == ErrorCode::Cancelled));
        assert!(rx.recv().await.is_none());
    }
}
