//! 假引擎（feature `fake`）：编排器的流程测试与 `WISP_FAKE_LLM=1` 的开发运行用，不下载、不起进程、不发网络请求。
//!
//! 事件语义与真实引擎相同（见 [`crate::stream`]）：可选的 `Phase{loading_model}` / `Phase{queued}`、按块的 Delta、Done；
//! 可以配置失败与延迟，取消时 `Error{cancelled}`。

use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wisp_types::{LlmError, StreamEvent, StreamPhase};

/// 假引擎的输出
#[derive(Debug, Clone, PartialEq)]
pub struct FakeEngineConfig {
    /// 完整输出
    pub text: String,
    /// 每个 Delta 的字符数
    pub chunk_chars: usize,
    /// 首个 Delta 之前先发的阶段（模拟冷启动 / 云端排队）
    pub phase: Option<StreamPhase>,
    /// 首个 Delta 之前的延迟
    pub first_delay: Duration,
    /// 相邻 Delta 之间的延迟
    pub chunk_delay: Duration,
    /// 有值时不输出文本，直接以这个错误结束
    pub fail: Option<LlmError>,
}

impl Default for FakeEngineConfig {
    fn default() -> Self {
        Self {
            text: "Wisp offline OCR\n离线识别的示例文本".to_owned(),
            chunk_chars: 8,
            phase: Some(StreamPhase::LoadingModel),
            first_delay: Duration::from_millis(300),
            chunk_delay: Duration::from_millis(20),
            fail: None,
        }
    }
}

/// 假引擎
#[derive(Debug, Clone, Default)]
pub struct FakeEngine {
    cfg: FakeEngineConfig,
}

impl FakeEngine {
    /// 以给定输出构造
    pub fn new(cfg: FakeEngineConfig) -> Self {
        Self { cfg }
    }

    /// 当前配置
    pub fn config(&self) -> &FakeEngineConfig {
        &self.cfg
    }

    /// 运行一次（与真实引擎同语义的事件流；必须在 tokio 运行时里调用）
    pub fn stream(&self, cancel: CancellationToken) -> mpsc::Receiver<StreamEvent> {
        let cfg = self.cfg.clone();
        crate::stream::spawn(cancel, move |sink, _cancel| async move {
            if let Some(phase) = cfg.phase {
                sink.phase(phase).await;
            }
            tokio::time::sleep(cfg.first_delay).await;
            if let Some(e) = cfg.fail {
                return Err(e);
            }
            let chars: Vec<char> = cfg.text.chars().collect();
            for (i, chunk) in chars.chunks(cfg.chunk_chars.max(1)).enumerate() {
                if i > 0 {
                    tokio::time::sleep(cfg.chunk_delay).await;
                }
                if !sink.delta(chunk.iter().collect::<String>()).await {
                    break;
                }
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use wisp_types::ErrorCode;

    use super::*;

    async fn collect(mut rx: mpsc::Receiver<StreamEvent>) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        while let Some(ev) = rx.recv().await {
            out.push(ev);
        }
        out
    }

    fn quick(text: &str) -> FakeEngineConfig {
        FakeEngineConfig {
            text: text.to_owned(),
            chunk_chars: 3,
            phase: Some(StreamPhase::Queued),
            first_delay: Duration::from_millis(1),
            chunk_delay: Duration::ZERO,
            fail: None,
        }
    }

    #[tokio::test]
    async fn streams_the_text_in_chunks_after_the_phase() {
        let events = collect(FakeEngine::new(quick("离线识别abc")).stream(CancellationToken::new())).await;
        assert_eq!(
            events[0],
            StreamEvent::Phase {
                phase: StreamPhase::Queued
            }
        );
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "离线识别abc");
        assert!(matches!(events.last(), Some(StreamEvent::Done { .. })));
    }

    #[tokio::test]
    async fn failures_and_cancellation() {
        let mut cfg = quick("x");
        cfg.fail = Some(LlmError::new(ErrorCode::LocalRuntime, "boom"));
        let events = collect(FakeEngine::new(cfg).stream(CancellationToken::new())).await;
        assert!(matches!(events.last(), Some(StreamEvent::Error(e)) if e.code == ErrorCode::LocalRuntime));

        let mut cfg = quick("x");
        cfg.first_delay = Duration::from_secs(30);
        let cancel = CancellationToken::new();
        let rx = FakeEngine::new(cfg).stream(cancel.clone());
        cancel.cancel();
        let events = collect(rx).await;
        assert!(matches!(events.last(), Some(StreamEvent::Error(e)) if e.code == ErrorCode::Cancelled));
    }
}
