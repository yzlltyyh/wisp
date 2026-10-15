//! 编排器：翻译、输入、OCR、直接翻译四条流程的状态机（取词 → 未就绪检查 → 语言和方向 → 词典式判定 → 长文本确认 →
//! 缓存 → 请求 → 后处理 → 合并 → 通道）；StreamRegistry（日志回放、所有权转移）；翻译 LRU 缓存；截图会话；
//! 命令总线的消费循环。
//!
//! 模块划分：
//! - [`fsm`]：「状态 × 事件」转移表，每次 Rust 发起的流程是一个 run；
//! - `engine.rs`：run 的登记与推进、面板显示、会话收尾、总线分发；
//! - [`translate`] / [`flow_input`] / [`ocr`]：各条流程与前端发起的请求；
//! - [`registry`]：流的日志、回放、取消与所有权，后处理 + 16 ms 合并的泵；
//! - [`cache`]：翻译 LRU 缓存；[`captures`]：冻结帧与裁剪图（`wisp://capture/`、`wisp://crop/`）；
//! - [`plan`]：风格 / 模型解析、语言方向、词典式、长文本、提示词与缓存键（纯函数）；[`not_ready`]：未就绪判定与映射；
//! - [`env`]：与外界的接缝（设置、钥匙串、LLM、平台、窗口层、埋点），单元测试用内存实现驱动状态机。
//!
//! **假模型（WISP_FAKE_LLM）由本模块负责切换**：编排器内部用私有的 `enum Backend { Real, Fake }`（[`backend`]）
//! 发请求，`Fake` 变体只在 `#[cfg(any(test, feature = "fake-llm"))]` 下存在；启动时（`new`）读取环境变量
//! `WISP_FAKE_LLM=1` 决定用哪个。未编入 `fake-llm` feature 的构建（发布版）里设置该变量只记一条 warn 并忽略。
//! `AppState.llm` 始终是真实的 `LlmClient`（Provider 测试、模型列表等不走假模型）。

pub mod backend;
pub mod cache;
pub mod captures;
mod engine;
pub mod env;
pub mod flow_input;
pub mod fsm;
pub mod not_ready;
pub mod ocr;
pub mod plan;
pub mod registry;
#[cfg(test)]
mod tests_flows;
pub mod translate;

use std::sync::Arc;

use tauri::ipc::Channel;
use tokio::sync::mpsc::UnboundedReceiver;
use wisp_types::{OcrReq, OverlayCommit, OverlayCommitAck, OverlayInit, StartAck, StreamEvent, TranslateReq};

use self::captures::{CropSource, FrameSource};
use self::engine::Core;
use self::env::TauriEnv;
use self::registry::Owner;
use crate::bus::Command;
use crate::error::IpcError;
use crate::state::Deps;
use crate::windows::panel::PANEL_LABEL;
use crate::windows::pinned;

/// 流式请求的宿主：由**调用方窗口的 label** 推导，不信任前端自报。
/// - 面板（label `panel`）：`req.session_id` 是发起请求的面板会话（> 0），请求随该会话结束而收尾（`panel_hidden`）；
/// - 固定窗口（label `pinned-<id>`）：`req.session_id` 必须为 0（固定窗口没有面板会话），请求归属这个 pinned 窗口，
///   随窗口关闭（[`Orchestrator::pinned_closed`]）收尾；`StartAck.width` 恒为 null（固定窗口的宽度由用户决定）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestHost {
    Panel { session_id: u64 },
    Pinned { pinned_id: String },
}

impl RequestHost {
    /// 按调用方窗口 label 与请求里的 `session_id` 确定宿主；两者对不上、或窗口不能发起流式请求时为 `invalid_argument`
    pub fn resolve(label: &str, session_id: u64) -> Result<Self, IpcError> {
        if label == PANEL_LABEL {
            return if session_id > 0 {
                Ok(Self::Panel { session_id })
            } else {
                Err(IpcError::invalid_argument("panel requests need the panel session_id"))
            };
        }
        if let Some(pinned_id) = pinned::id_of(label).filter(|id| !id.is_empty()) {
            return if session_id == 0 {
                Ok(Self::Pinned {
                    pinned_id: pinned_id.to_owned(),
                })
            } else {
                Err(IpcError::invalid_argument("pinned requests must pass session_id = 0"))
            };
        }
        Err(IpcError::invalid_argument(format!(
            "window {label} cannot start streaming requests"
        )))
    }
}

/// `stream_attach` / `cancel` / `capture_release` 的调用方（这三条命令不带会话 id，请求与截图 id 自增可猜）：
/// 同样由**调用方窗口的 label** 推导。面板窗口只能碰还没绑定会话的 Rust 发起请求与面板会话的请求 / 截图，pinned 窗口
/// 只能碰归属它的。
///
/// 前端可见的语义不变（只给门面加了这个参数）：`cancel` / `capture_release` 仍然恒为 `()`——调用方不是
/// 宿主（例如面板卸载时去取消已经转给 pinned 的请求）或者根本不是面板 / pinned 窗口时**静默忽略**，前端不会因此收到
/// rejection；`stream_attach` 对这类调用与请求不存在一样返回 not_found
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestCaller {
    Panel,
    Pinned { pinned_id: String },
}

impl RequestCaller {
    /// 按调用方窗口 label 确定；既不是面板也不是 pinned 的窗口为 None
    pub fn of(label: &str) -> Option<Self> {
        if label == PANEL_LABEL {
            return Some(Self::Panel);
        }
        pinned::id_of(label)
            .filter(|id| !id.is_empty())
            .map(|pinned_id| Self::Pinned {
                pinned_id: pinned_id.to_owned(),
            })
    }

    /// 是否是这个宿主的窗口
    pub(crate) fn may_access(&self, owner: &Owner) -> bool {
        match (self, owner) {
            (Self::Panel, Owner::Unbound | Owner::Panel(_)) => true,
            (Self::Pinned { pinned_id }, Owner::Pinned(p)) => p == pinned_id,
            _ => false,
        }
    }
}

/// 编排器（`AppState.orchestrator`）：对外的门面，实现在 `engine.rs` 等子模块。
/// `stream_attach` / `cancel` / `capture_release` 带调用方参数（[`RequestCaller`]，与 `translate_start` 的
/// `host` 同一做法），返回类型与前端可见的语义不受影响
pub struct Orchestrator {
    core: Arc<Core>,
}

impl Orchestrator {
    /// 构造（不启动任何任务）；把截图资源注册到 `wisp://`（`capture/` 冻结帧、`crop/` 裁剪图）
    pub fn new(d: &Deps) -> Arc<Self> {
        let core = Core::new(Arc::new(TauriEnv::new(d)));
        d.assets
            .register("capture", Arc::new(FrameSource(Arc::clone(&core.captures))));
        d.assets
            .register("crop", Arc::new(CropSource(Arc::clone(&core.captures))));
        Arc::new(Self { core })
    }

    /// 启动命令总线的消费循环与面板会话结束的订阅（AppState 已托管之后调用）
    pub fn spawn(self: Arc<Self>, rx: UnboundedReceiver<Command>) {
        self.core.spawn(rx);
    }

    /// 由面板或固定窗口发起的翻译 / 润色 / 校对 / 提升。`host` 由命令层按调用方窗口确定（[`RequestHost::resolve`]）。
    /// 面板宿主：`StartAck.width` = `WindowManager::view_width(Translate, …)`，与 `panel_card_width(session_id)` 相同时为
    /// null；登记 session → request 之后立即用 `panel_session_live` 核对一次
    pub async fn translate_start(
        &self,
        req: TranslateReq,
        host: RequestHost,
        sink: Channel<StreamEvent>,
    ) -> Result<StartAck, IpcError> {
        self.core.translate_start(req, host, Arc::new(sink)).await
    }

    /// 重新识别、切换模式 / 模型、直接翻译时查看原文（宿主规则同 [`Self::translate_start`]）
    pub async fn ocr_start(
        &self,
        req: OcrReq,
        host: RequestHost,
        sink: Channel<StreamEvent>,
    ) -> Result<StartAck, IpcError> {
        self.core.ocr_start(req, host, Arc::new(sink)).await
    }

    /// 回放已有事件，之后继续推送实时事件。只有请求的宿主窗口可以接上，否则与请求不存在一样 not_found
    pub fn stream_attach(
        &self,
        caller: &RequestCaller,
        request_id: &str,
        sink: Channel<StreamEvent>,
    ) -> Result<(), IpcError> {
        self.core.stream_attach(caller, request_id, Arc::new(sink))
    }

    /// 取消请求（终止事件为 Error{cancelled}）。已结束 / 不存在 / 属于别的窗口时是空操作（后者记 debug 日志）
    pub fn cancel(&self, caller: &RequestCaller, request_id: &str) {
        self.core.cancel(caller, request_id);
    }

    /// 面板关闭时释放内存里的截图。不存在 / 属于别的窗口时是空操作（后者记 debug 日志）
    pub fn capture_release(&self, caller: &RequestCaller, capture_id: &str) {
        self.core.capture_release(caller, capture_id);
    }

    /// Overlay 拉取本窗口当前会话的初始化数据：`label` 是调用方窗口（`overlay-<n>`），经
    /// `WindowManager::overlay_monitor(label)` 得到本次会话绑定的显示器；窗口当前没有会话（预建、空闲中）时返回 `Ok(None)`，
    /// 页面等 `overlay:session` 后再调用（见 `wisp_types::OverlaySessionStart` 的防竞态约定）。
    /// `frozen_native` **只**取该显示器 `WindowManager::overlay_show` 的返回值（原生冻结帧真的装上了）：带原生帧
    /// （macOS）的显示器在 `overlay_show` 返回之前同样回 `Ok(None)`，返回后 emit_to 该窗口 `overlay:session`，页面重新
    /// init；不带原生帧的平台（Windows / X11、假帧）显示之前就确定为 false，直接应答
    pub fn overlay_init(&self, label: &str) -> Result<Option<OverlayInit>, IpcError> {
        self.core.overlay_init(label)
    }

    /// 首帧渲染完成 → 显示 Overlay
    pub fn overlay_ready(&self, session_id: u64, monitor_id: &str) {
        self.core.overlay_ready(session_id, monitor_id);
    }

    /// 立即裁剪并发出请求，然后返回飞入目标
    pub async fn overlay_commit(&self, c: OverlayCommit) -> Result<OverlayCommitAck, IpcError> {
        self.core.overlay_commit(c).await
    }

    /// commit 时已立即对 Panel 做 prepare / ready，并把 Panel 显示在 Overlay **下面**（Overlay 层级 101 > Panel 25）；
    /// overlay_done 时先确认 Panel 已显示，下一帧再销毁 Overlay（两个窗口之间不出现空帧）
    pub fn overlay_done(&self, session_id: u64) {
        self.core.overlay_done(session_id);
    }

    /// Esc 取消截图
    pub fn overlay_cancel(&self, session_id: u64) {
        self.core.overlay_cancel(session_id);
    }

    /// `overlay_focus` 命令：调用方 Overlay（`label`，已由 ipc 层核对绑定了该会话）请求成为
    /// key——会话还没 commit 时让它所在显示器的 Overlay 成为 key（不激活应用），否则忽略
    pub fn overlay_focus(&self, session_id: u64, label: &str) {
        self.core.overlay_focus_request(session_id, label);
    }

    /// 全局 Esc（Esc guard）按下：有截图会话时取消它（未 commit）或提前收起 Overlay（commit 之后），返回 true；
    /// 没有截图会话时返回 false，由调用方按面板处理
    pub fn escape_overlay(&self) -> bool {
        self.core.escape_overlay()
    }

    /// 固定时转移流的所有权，由新窗口重新 attach
    pub fn transfer_to_pinned(&self, request_id: &str, pinned_id: &str) {
        self.core.transfer_to_pinned(request_id, pinned_id);
    }

    /// pinned 窗口已销毁（✕ / ⌘W / Alt+F4、退出；`WindowManager` 的窗口事件回调里调用，不阻塞）：取消归属这个窗口的
    /// 全部流（转移过来的与在窗口里新发起的），释放它们持有的截图
    pub fn pinned_closed(&self, pinned_id: &str) {
        self.core.pinned_closed(pinned_id);
    }

    /// 启动后的空闲预热（bootstrap 在面板预热、剪贴板缓存与 Overlay 预建之后，于阻塞线程上调用一次）：例如 Windows 上
    /// `platform.selection.has_selection(Duration::ZERO)` 建好 UIA 线程与对象，避免第一次取词因 UIA 初始化
    /// 超过 110 ms 而走复制回退。可以阻塞，不得弹出任何系统授权对话框
    pub fn idle_prewarm(&self) {
        self.core.idle_prewarm();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_host_follows_the_calling_window() {
        assert_eq!(
            RequestHost::resolve("panel", 7).expect("panel"),
            RequestHost::Panel { session_id: 7 }
        );
        assert_eq!(
            RequestHost::resolve("pinned-ab12", 0).expect("pinned"),
            RequestHost::Pinned {
                pinned_id: "ab12".to_owned()
            }
        );
        // 宿主与 session_id 对不上
        assert!(RequestHost::resolve("panel", 0).is_err());
        assert!(RequestHost::resolve("pinned-ab12", 3).is_err());
        // 其他窗口不能发起流式请求
        for label in ["settings", "onboarding", "overlay-0", "pinned-"] {
            assert!(RequestHost::resolve(label, 0).is_err(), "{label}");
        }
    }

    #[test]
    fn request_caller_only_reaches_its_own_streams() {
        let panel = RequestCaller::of("panel").expect("panel");
        let pinned = RequestCaller::of("pinned-ab12").expect("pinned");
        assert!(panel.may_access(&Owner::Unbound) && panel.may_access(&Owner::Panel(3)));
        assert!(
            !panel.may_access(&Owner::Pinned("ab12".into())),
            "面板抢不走 pinned 的流"
        );
        assert!(pinned.may_access(&Owner::Pinned("ab12".into())));
        assert!(!pinned.may_access(&Owner::Pinned("other".into())));
        assert!(!pinned.may_access(&Owner::Panel(3)) && !pinned.may_access(&Owner::Unbound));
        for label in ["settings", "onboarding", "overlay-0", "pinned-"] {
            assert!(RequestCaller::of(label).is_none(), "{label}");
        }
    }
}
