//! LLM 后端：真实的 `LlmClient`，或冒烟 / 演示用的假模型（`WISP_FAKE_LLM=1`）。
//!
//! 假模型只在编入 `fake-llm` feature（以及单元测试）时存在；发布版里设置该变量只记一条 warn 并忽略。
//! `AppState.llm` 始终是真实客户端（Provider 测试、模型列表不走假模型）。

use tokio::sync::mpsc;
use wisp_llm::{CancellationToken, ChatRequest, LlmClient};
use wisp_types::{ErrorCode, LlmError, StreamEvent};

/// 编排器发请求用的后端
pub enum Backend {
    Real,
    #[cfg(any(test, feature = "fake-llm"))]
    Fake,
}

impl Backend {
    /// 按 `WISP_FAKE_LLM` 选择（构造时读一次）
    pub fn from_env() -> Self {
        let wanted = std::env::var("WISP_FAKE_LLM").is_ok_and(|v| v == "1");
        #[cfg(any(test, feature = "fake-llm"))]
        if wanted {
            log::info!("orchestrator: using the fake LLM (WISP_FAKE_LLM=1)");
            return Self::Fake;
        }
        #[cfg(not(any(test, feature = "fake-llm")))]
        if wanted {
            log::warn!("WISP_FAKE_LLM=1 ignored: this build does not include the fake-llm feature");
        }
        Self::Real
    }

    /// 发起流式请求
    pub fn stream(&self, llm: &LlmClient, req: ChatRequest, cancel: CancellationToken) -> mpsc::Receiver<StreamEvent> {
        match self {
            Self::Real => llm.stream(req, cancel),
            #[cfg(any(test, feature = "fake-llm"))]
            Self::Fake => fake::stream(req, cancel),
        }
    }

    /// 应用状态还没就绪时（只可能出现在启动的极早期）：Start → Error{internal}
    pub fn unavailable(&self, req: ChatRequest) -> mpsc::Receiver<StreamEvent> {
        let (tx, rx) = mpsc::channel(2);
        let _ = tx.try_send(StreamEvent::Start {
            request_id: req.request_id.clone(),
            provider_id: req.provider.id.clone(),
            model_id: req.model.id.clone(),
            model_label: req.model.id.clone(),
            upstream_id: None,
        });
        let _ = tx.try_send(StreamEvent::Error(LlmError::new(
            ErrorCode::Internal,
            "the LLM client is not ready",
        )));
        rx
    }
}

/// 假模型：按请求内容给出像样的输出（翻译方向、OCR、反思），固定节奏流式输出
#[cfg(any(test, feature = "fake-llm"))]
pub mod fake {
    use std::time::Duration;

    use tokio::sync::mpsc;
    use wisp_llm::fake::{FakeConfig, FakeLlm};
    use wisp_llm::{CancellationToken, ChatRequest, Purpose, UserContent};
    use wisp_types::StreamEvent;

    /// 首 token 延迟（接近真实服务的体感）
    pub const FIRST_TOKEN: Duration = Duration::from_millis(280);
    /// 相邻 Delta 的间隔
    pub const CHUNK: Duration = Duration::from_millis(24);

    const ZH: &str = "敏捷的棕色狐狸跳过了那只懒狗。这是 Wisp 的演示译文：请求在面板出现之前就已发出，\
        文字以 16 毫秒为一批流式到达。";
    const EN: &str = "The quick brown fox jumps over the lazy dog. This is a Wisp demo translation: the request \
        was sent before the panel appeared, and text streams in 16 ms batches.";
    const OCR: &str = "Wisp — LLM OCR\n截图识别的演示文本\nThe quick brown fox jumps over the lazy dog.";

    /// 流式输出
    pub fn stream(req: ChatRequest, cancel: CancellationToken) -> mpsc::Receiver<StreamEvent> {
        let text = output_for(&req);
        FakeLlm::new(FakeConfig {
            text,
            chunk_chars: 2,
            first_token_delay: FIRST_TOKEN,
            chunk_delay: CHUNK,
        })
        .stream(req, cancel)
    }

    /// 按用途与原文挑一段输出：反思一律 OK；OCR 给固定转录；翻译按原文是否含中日韩文字决定译成英文还是中文
    pub fn output_for(req: &ChatRequest) -> String {
        match (req.purpose, &req.user) {
            (Purpose::Reflect, _) => "OK".to_owned(),
            (Purpose::Ocr, _) | (_, UserContent::Image { .. }) => OCR.to_owned(),
            (_, UserContent::Text(t)) => {
                let cjk = t
                    .chars()
                    .any(|c| matches!(u32::from(c), 0x3040..=0x30FF | 0x3400..=0x9FFF | 0xAC00..=0xD7AF));
                if cjk { EN.to_owned() } else { ZH.to_owned() }
            }
        }
    }
}
