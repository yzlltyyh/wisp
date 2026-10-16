//! 把离线 / 云端引擎包装成与 `wisp_llm::LlmClient::stream` 同语义的事件流，编排器的泵（后处理、16 ms 合并、无文字判定、
//! 识别历史）因此不必区分流从哪里来。
//!
//! 语义：**Start 由编排器在登记请求时发**，引擎只发 `Phase`（冷启动 `loading_model`、云端 `queued`）、`Notice`（例如结果不完整）、
//! `Delta` 与恰好一个 `Done` / `Error`。取消令牌触发后立即发 `Error{cancelled}`、之后不再发任何事件；接收端被丢弃时任务随即结束。
//! `Done.no_text` 恒为 false（由编排器在后处理之后判定）。

use std::future::Future;

use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use wisp_types::{ErrorCode, LlmError, NoticeKind, StreamEvent, StreamPhase};

/// 通道容量（与 wisp-llm 相同的量级：引擎的 Delta 很少，主要是 VL 的逐区域输出）
pub const CHANNEL_CAPACITY: usize = 16;

/// 引擎写事件的一端
#[derive(Debug, Clone)]
pub struct EngineSink {
    tx: mpsc::Sender<StreamEvent>,
}

impl EngineSink {
    /// 阶段提示；接收端已丢弃时返回 false
    pub async fn phase(&self, phase: StreamPhase) -> bool {
        self.tx.send(StreamEvent::Phase { phase }).await.is_ok()
    }

    /// 提示（界面在结果旁说明，例如部分区域没能识别）；接收端已丢弃时返回 false
    pub async fn notice(&self, kind: NoticeKind) -> bool {
        self.tx.send(StreamEvent::Notice { kind }).await.is_ok()
    }

    /// 文本增量（空串不发）；接收端已丢弃时返回 false
    pub async fn delta(&self, text: impl Into<String>) -> bool {
        let text = text.into();
        if text.is_empty() {
            return !self.tx.is_closed();
        }
        self.tx.send(StreamEvent::Delta { text }).await.is_ok()
    }

    /// 接收端是否已丢弃（引擎据此提前结束）
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    /// 直接包一个通道（单测里不经 [`spawn`] 调用引擎函数）
    #[cfg(test)]
    pub(crate) fn for_test(tx: mpsc::Sender<StreamEvent>) -> Self {
        Self { tx }
    }
}

/// 在当前 tokio 运行时上运行一个引擎任务，返回事件流。`job` 用 sink 发 Phase / Delta；返回 `Ok` 时自动补 `Done`，
/// 返回 `Err` 时发 `Error`。不在 tokio 运行时里调用时立即得到 `Error{internal}`
pub fn spawn<F, Fut>(cancel: CancellationToken, job: F) -> mpsc::Receiver<StreamEvent>
where
    F: FnOnce(EngineSink, CancellationToken) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), LlmError>> + Send + 'static,
{
    let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        let _ = tx.try_send(StreamEvent::Error(LlmError::new(
            ErrorCode::Internal,
            "engine streams must be started inside a tokio runtime",
        )));
        return rx;
    };
    handle.spawn(async move {
        let t0 = Instant::now();
        let sink = EngineSink { tx: tx.clone() };
        let outcome = tokio::select! {
            biased;
            () = cancel.cancelled() => Err(cancelled()),
            () = tx.closed() => return,
            r = job(sink, cancel.clone()) => r,
        };
        // 任务正常结束的同时被取消：以取消为准（编排器已经把这个请求记为取消）
        let last = match outcome {
            Ok(()) if !cancel.is_cancelled() => StreamEvent::Done {
                elapsed_ms: u32::try_from(t0.elapsed().as_millis()).unwrap_or(u32::MAX),
                usage: None,
                truncated: None,
                cached: false,
                no_text: false,
            },
            Ok(()) => StreamEvent::Error(cancelled()),
            Err(e) => StreamEvent::Error(e),
        };
        let _ = tx.send(last).await;
    });
    rx
}

fn cancelled() -> LlmError {
    LlmError::new(ErrorCode::Cancelled, "cancelled")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    async fn collect(mut rx: mpsc::Receiver<StreamEvent>) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        while let Some(ev) = rx.recv().await {
            out.push(ev);
        }
        out
    }

    #[tokio::test]
    async fn ok_ends_with_done() {
        let rx = spawn(CancellationToken::new(), |sink, _| async move {
            sink.phase(StreamPhase::LoadingModel).await;
            sink.delta("hello").await;
            sink.delta("").await;
            Ok(())
        });
        let events = collect(rx).await;
        assert_eq!(events.len(), 3);
        assert_eq!(
            events[0],
            StreamEvent::Phase {
                phase: StreamPhase::LoadingModel
            }
        );
        assert_eq!(
            events[1],
            StreamEvent::Delta {
                text: "hello".to_owned()
            }
        );
        assert!(matches!(
            events[2],
            StreamEvent::Done {
                no_text: false,
                cached: false,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn errors_and_cancellation_are_terminal() {
        let rx = spawn(CancellationToken::new(), |_, _| async move {
            Err(LlmError::new(ErrorCode::LocalRuntime, "boom"))
        });
        let events = collect(rx).await;
        assert!(matches!(&events[..], [StreamEvent::Error(e)] if e.code == ErrorCode::LocalRuntime));

        let cancel = CancellationToken::new();
        let rx = spawn(cancel.clone(), |sink, _| async move {
            sink.delta("partial").await;
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(())
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
        let events = collect(rx).await;
        assert!(matches!(events.last(), Some(StreamEvent::Error(e)) if e.code == ErrorCode::Cancelled));
        assert_eq!(events.iter().filter(|e| e.is_terminal()).count(), 1);
    }
}
