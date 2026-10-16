//! 识别与翻译的事件流：离线 PP-OCR（ORT 工作进程）、离线 PaddleOCR-VL（版面检测 + llama-server）、PaddleOCR 云端、
//! 离线翻译（合成服务商转发给 wisp-llm）。
//!
//! 语义与 `wisp_llm::LlmClient::stream` 相同（编排器的泵不区分流从哪里来）：Start 由编排器发；冷启动时先发
//! `Phase{loading_model}`、云端排队时 `Phase{queued}`；最后恰好一个 `Done` / `Error`。离线引擎失败时以错误结束，
//! **绝不改用云端**。识别内容、截图、Token、服务地址都不进日志。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{Notify, mpsc};
use tokio_util::bytes::Bytes;
use tokio_util::sync::CancellationToken;
use wisp_capture::RgbaImage;
use wisp_llm::{ChatRequest, LlmClient, Purpose, Secret, UserContent};
use wisp_local::ocr::cloud::{CloudClient, CloudRequest, Token};
use wisp_local::ocr::layout::{LayoutBox, LayoutDetector};
use wisp_local::ocr::vl::{self, PipelineOptions, VlmBackend, VlmRequest};
use wisp_local::ocr::{PpOcrTier, assemble};
use wisp_local::stream::EngineSink;
use wisp_local::{LocalError, RuntimeFailure};
use wisp_types::{
    ErrorCode, ImageDetail, LlmError, LocalModelId, LocalModelKind, OcrMode, Provider, StreamEvent, StreamPhase,
    ThinkingOff,
};

use super::ModelsService;
use super::lifecycle::Lease;
use super::slots::SlotKind;
use crate::orchestrator::plan::{offline_model_entry, offline_provider};

/// 离线翻译流的通道容量（与 wisp-llm 同一量级）
const CHANNEL: usize = 32;

fn cancelled() -> LlmError {
    LlmError::new(ErrorCode::Cancelled, "cancelled")
}

/// 冷启动的阶段提示只发一次（PaddleOCR-VL 同时等两个槽位）
struct Announce<'a> {
    sink: &'a EngineSink,
    sent: AtomicBool,
}

impl Announce<'_> {
    async fn loading(&self) {
        if !self.sent.swap(true, Ordering::AcqRel) {
            self.sink.phase(StreamPhase::LoadingModel).await;
        }
    }
}

impl ModelsService {
    /// 离线 OCR：PP-OCRv6 走 ORT 工作进程（检测 + 识别 → 几何拼装），PaddleOCR-VL 走版面检测 + 逐区域识别。
    /// 输入是缩放后的像素（不经 PNG）
    pub fn ocr_local(
        self: &Arc<Self>,
        model: LocalModelId,
        mode: OcrMode,
        image: Arc<RgbaImage>,
        max_side: u32,
        llm: LlmClient,
        cancel: CancellationToken,
    ) -> mpsc::Receiver<StreamEvent> {
        let svc = Arc::clone(self);
        wisp_local::stream::spawn(cancel, move |sink, cancel| async move {
            if model.kind() != LocalModelKind::Ocr {
                return Err(LlmError::new(ErrorCode::NotConfigured, "not an offline OCR model"));
            }
            let announce = Announce {
                sink: &sink,
                sent: AtomicBool::new(false),
            };
            let t0 = std::time::Instant::now();
            match PpOcrTier::of(model) {
                Some(tier) => {
                    let lease = svc
                        .acquire(SlotKind::Ort, model, &cancel, || announce.loading())
                        .await?;
                    let worker = lease.ort().ok_or_else(|| RuntimeFailure::Crashed.to_llm_error())?;
                    let forced = lease.cancelled();
                    let lines = tokio::select! {
                        () = cancel.cancelled() => return Err(cancelled()),
                        () = forced.cancelled() => return Err(stopped()),
                        r = worker.ppocr(tier, image, max_side) => r.map_err(|e| svc.engine_error(lease.classify(e.to_llm_error())))?,
                    };
                    log::info!(
                        "offline: {} recognised {} lines in {} ms",
                        model.as_str(),
                        lines.len(),
                        t0.elapsed().as_millis()
                    );
                    sink.delta(assemble::assemble(&lines, mode)).await;
                }
                None => {
                    // 两个槽位并行准备；一个失败就不再等另一个（它的启动照常在后台完成，之后闲置释放）
                    let (ort, vl) = tokio::try_join!(
                        svc.acquire(SlotKind::Ort, model, &cancel, || announce.loading()),
                        svc.acquire(SlotKind::Vl, model, &cancel, || announce.loading()),
                    )?;
                    let (base_url, api_key) = vl
                        .endpoint()
                        .map(|(u, k)| (u.to_owned(), k.to_owned()))
                        .ok_or_else(|| RuntimeFailure::Crashed.to_llm_error())?;
                    let layout = OrtLayout { lease: ort };
                    let vlm = LlamaVlm { llm, base_url, api_key };
                    // 强制停止（删除模型、退出）同样结束流水线
                    let forced = vl.cancelled();
                    let run_cancel = cancel.child_token();
                    // 不在显卡上推理时，全空的结果按「无文字」处理，不提示关闭 GPU 加速
                    let opts = PipelineOptions::for_mode(mode).with_gpu(svc.gpu_in_use());
                    let result = tokio::select! {
                        () = forced.cancelled() => { run_cancel.cancel(); Err(stopped()) }
                        r = vl::run(image, opts, &layout, &vlm, &sink, &run_cancel) => r,
                    };
                    let result = result.map_err(|e| svc.engine_error(vl.classify(e)));
                    log::info!(
                        "offline: {} document pipeline finished in {} ms ({})",
                        model.as_str(),
                        t0.elapsed().as_millis(),
                        if result.is_ok() { "ok" } else { "failed" }
                    );
                    drop(vl);
                    result?;
                }
            }
            Ok(())
        })
    }

    /// 离线翻译：取得翻译槽位（冷启动先发 `Phase{loading_model}`），把地址与这次启动的随机 key 填进请求，转发 wisp-llm
    /// 的事件。推理进程在请求中途退出时把错误换成归类后的原因（例如内存不足、崩溃）
    pub fn stream_translate(
        self: &Arc<Self>,
        mut req: ChatRequest,
        llm: LlmClient,
        cancel: CancellationToken,
    ) -> mpsc::Receiver<StreamEvent> {
        let (tx, rx) = mpsc::channel(CHANNEL);
        let model = LocalModelId::parse(&req.model.id).filter(|m| m.kind() == LocalModelKind::Translate);
        let Some(model) = model else {
            let _ = tx.try_send(StreamEvent::Error(LlmError::new(
                ErrorCode::NotConfigured,
                "not an offline translation model",
            )));
            return rx;
        };
        let svc = Arc::clone(self);
        super::spawn(async move {
            let loading = tx.clone();
            let lease = svc
                .acquire(SlotKind::Translate, model, &cancel, || async move {
                    let _ = loading
                        .send(StreamEvent::Phase {
                            phase: StreamPhase::LoadingModel,
                        })
                        .await;
                })
                .await;
            let lease = match lease {
                Ok(l) => l,
                Err(e) => {
                    let _ = tx.send(StreamEvent::Error(e)).await;
                    return;
                }
            };
            let Some((base_url, api_key)) = lease.endpoint().map(|(u, k)| (u.to_owned(), k.to_owned())) else {
                let _ = tx
                    .send(StreamEvent::Error(RuntimeFailure::Crashed.to_llm_error()))
                    .await;
                return;
            };
            req.provider.base_url = base_url;
            req.key = Some(Secret::new(api_key));
            // 强制停止（换模型、删除、退出）时结束这次请求
            let upstream = cancel.child_token();
            let forced = lease.cancelled();
            let mut events = llm.stream(req, upstream.clone());
            loop {
                let ev = tokio::select! {
                    () = forced.cancelled() => {
                        upstream.cancel();
                        let _ = tx.send(StreamEvent::Error(stopped())).await;
                        break;
                    }
                    ev = events.recv() => ev,
                };
                let Some(ev) = ev else { break };
                let terminal = ev.is_terminal();
                let ev = match ev {
                    StreamEvent::Error(e) => StreamEvent::Error(lease.classify(e)),
                    other => other,
                };
                if tx.send(ev).await.is_err() || terminal {
                    break;
                }
            }
            drop(lease);
        });
        rx
    }
}

impl ModelsService {
    /// 请求失败的收尾：模型文件缺失或损坏（ORT 在第一次识别时才读模型文件）时重读库存，设置页随之显示为损坏
    fn engine_error(self: &Arc<Self>, e: LlmError) -> LlmError {
        if Self::is_model_corrupt(&e) {
            self.on_model_corrupt();
        }
        e
    }
}

/// 强制停止（删除模型、退出）打断了请求
fn stopped() -> LlmError {
    let mut e = RuntimeFailure::Crashed.to_llm_error();
    e.message = "the offline model was stopped".to_owned();
    e.retryable = true;
    e
}

/// 版面检测：ORT 槽位的租约
struct OrtLayout {
    lease: Lease,
}

impl LayoutDetector for OrtLayout {
    async fn detect(&self, image: Arc<RgbaImage>, cancel: CancellationToken) -> Result<Vec<LayoutBox>, LocalError> {
        let worker = self.lease.ort().ok_or(LocalError::Runtime(RuntimeFailure::Crashed))?;
        let forced = self.lease.cancelled();
        tokio::select! {
            () = forced.cancelled() => Err(LocalError::Cancelled),
            r = worker.detect(image, cancel) => r,
        }
    }
}

/// PaddleOCR-VL 的识别：经 wisp-llm 访问 vl 槽位的 llama-server（OpenAI 兼容接口，先图后文，无 system）
struct LlamaVlm {
    llm: LlmClient,
    base_url: String,
    api_key: String,
}

/// `Arc<Vec<u8>>` → `Bytes` 的零拷贝包装
struct SharedPng(Arc<Vec<u8>>);

impl AsRef<[u8]> for SharedPng {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

impl VlmBackend for LlamaVlm {
    fn recognize(&self, req: VlmRequest, cancel: CancellationToken) -> mpsc::Receiver<StreamEvent> {
        let mut provider = offline_provider(ThinkingOff::None);
        provider.base_url.clone_from(&self.base_url);
        let chat = ChatRequest {
            request_id: wisp_types::RequestId::new(),
            provider,
            key: Some(Secret::new(self.api_key.clone())),
            model: offline_model_entry(LocalModelId::PaddleOcrVl16),
            system: String::new(),
            user: UserContent::Image {
                bytes: Bytes::from_owner(SharedPng(req.png)),
                mime: "image/png",
                text: req.prompt.to_owned(),
                detail: ImageDetail::High,
            },
            temperature: Some(0.0),
            max_output_tokens: req.max_tokens,
            purpose: Purpose::Ocr,
        };
        self.llm.stream(chat, cancel)
    }
}

/// PaddleOCR 云端：手写 multipart / JSON 的请求在 wisp-local；这里只把编排器给的图片与 Token 交过去，排队时发
/// `Phase{queued}`。客户端每次识别新建、用完即丢（平时不存在）
pub fn ocr_cloud(
    provider: &Provider,
    model: String,
    image: Arc<Vec<u8>>,
    mime: &'static str,
    key: Option<Secret>,
    mode: OcrMode,
    cancel: CancellationToken,
) -> mpsc::Receiver<StreamEvent> {
    let endpoint = provider.base_url.clone();
    let token = key.and_then(|k| Token::new(k.expose().to_owned()));
    wisp_local::stream::spawn(cancel, move |sink, cancel| async move {
        let client = CloudClient::new().map_err(|e| e.to_llm_error())?;
        let req = CloudRequest {
            endpoint,
            token,
            model,
            image,
            mime,
        };
        let queued = Notify::new();
        let on_queued = || queued.notify_one();
        let recognize = client.recognize(req, &on_queued, cancel.clone());
        tokio::pin!(recognize);
        let mut announced = false;
        let output = loop {
            tokio::select! {
                r = &mut recognize => break r?,
                () = queued.notified(), if !announced => {
                    announced = true;
                    sink.phase(StreamPhase::Queued).await;
                }
            }
        };
        sink.delta(output.text(mode)).await;
        Ok(())
    })
}
