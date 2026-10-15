//! Wisp 的 LLM 客户端。
//!
//! 职责：四种协议的请求构造、流式解析与适配、`<think>` 标签剥离、错误归一化、base_url 规范化、
//! 模型列表获取与合并、视觉能力判定、测试连接、参数降级与能力缓存、超时、重试、取消、16 ms 合并器，
//! 以及 16 个厂商模板。不负责提示词内容（wisp-prompts）、Key 存储（desktop 的 keychain），不依赖 Tauri。
//!
//! ## 使用约定（给 desktop 的编排器 / 设置服务）
//!
//! - `stream` 的事件序列总是 `Start → (Notice | Delta)* → Done | Error`。流开始之前就失败（非 2xx、连接失败、
//!   取消、第一个事件就是错误）时也先有一个 `Start`（`upstream_id` 取能拿到的上游 id，后面不跟 Notice），紧接着是 `Error`；
//!   `Notice{retried | param_degraded}` 只在流真正开始时紧跟在 `Start` 之后。
//!   Delta 是协议适配与 `<think>` 剥离之后、**尚未**做 16 ms 合并的增量（合并由调用方用 [`Coalescer`] 驱动）。
//! - 取消：cancel 之后 wisp-llm 先释放响应体（连接立即关闭，上游停止生成——即使消费方暂时不读、通道已满），
//!   不再往通道里放任何 Delta（包括正在等待发送的那一个），随后发出 `Error{cancelled}`；但 cancel **之前**已经进入
//!   通道的事件（最多 64 个）仍会先被收到。编排器发出 cancel 后应当丢弃收到的 Delta，只等终止事件
//!   （「取消后不再有任何字节发到前端」由编排器这一层保证）。
//! - 通道在终止事件之后立即关闭（正常结束后读完剩余响应体、让连接回到连接池的收尾在独立任务里进行）。
//! - `param_unsupported` 只以降级成功后的 `Notice{param_degraded}` 弱提示出现，**不会**作为终止事件的 code
//!   （它不出错误卡片）。降级不了时终止事件是会出卡片的 `upstream`（status 400，message 为服务端原文）：
//!   降级后又被拒了另一个参数时 `retryable: true` + `hint: errors.hint.param_adjusted`（两个结论都已写进能力缓存，
//!   点「重试」即可成功）；识别不出是哪个参数、或同一参数降级后仍被拒时 `retryable: false` +
//!   `hint: errors.hint.param_rejected`。
//! - 2xx 的响应体也可能是出错形态（MiniMax 的 `base_resp.status_code ≠ 0`、中转站的顶层 `{code, msg}`）：按厂商错误码 /
//!   状态码 / 关键词归为 auth / quota / rate_limited / content_filter / upstream 等；形状认不出的 2xx JSON 归 protocol，
//!   不会报成「连接中断」。
//! - `test_connection` 用 `provider.models` 里**第一个已启用的对话模型**做探针（跳过嵌入、语音等）：调用方把默认翻译
//!   模型排在最前面，即「默认翻译模型优先」。
//! - 能力缓存：降级结论经 [`CapsStore::update`] 写回；带图请求的运行时视觉反馈经 [`CapsStore::set_vision`] 写回
//!   （不覆盖用户的手动设置；服务端没有明确说「不支持图片输入」的 no_vision 只写给视觉还是 Unknown 的模型；
//!   单张图片处理失败——损坏、解码失败、「Unable to process input image」——归为 upstream，不写视觉反馈）。
//!   请求时以 `CapsStore::get` 为准，缺的项退回 `ModelEntry.caps`。
//! - 系统代理：切换代理（Clash / VPN）后无需重启。缓存的 HTTP Client 出网络类错误时立即淘汰（自动重试或下一个请求用
//!   新的 Client 读取当前的系统代理）；在 macOS / Windows 上闲置超过 60 s 的也重建。Linux 的系统代理只来自进程启动时的
//!   环境变量，不做闲置重建（重建读不到新东西，却要重新加载整个系统 CA 包）。
//! - `LlmError.hint` 是前端 `errors.hint.*` 的 i18n key；`LlmError.message` 只含服务端原文或本地英文简述（≤ 300 字符），
//!   回显的 Key 在截断**之前**就已替换为 `***`（截断点切开 Key 时留下的前缀同样替换）；`LlmError.url` 只到 host + path。

pub use wisp_types::llm::*;

mod adapter;
mod caps;
mod client;
mod coalesce;
mod degrade;
mod errors;
mod models;
mod probe;
mod request;
mod secret;
mod sse;
mod templates;
mod think;
mod timing;
mod url;
pub mod vision;

#[cfg(feature = "fake")]
pub mod fake;

pub use caps::{CapsStore, MemoryCapsStore};
pub use client::{ChatRequest, ClientConfig, Completion, LlmClient, Purpose, RemoteModel, UserContent};
pub use coalesce::Coalescer;
pub use models::merge_models;
pub use secret::Secret;
pub use templates::{provider_from_template, templates};
/// `LlmClient::stream` / `complete` 的取消令牌：下游（编排器、wisp-eval）直接用这个类型，不必自己依赖 tokio-util
pub use tokio_util::sync::CancellationToken;
pub use url::{Endpoint, endpoint_url, normalize_base_url, url_preview};
