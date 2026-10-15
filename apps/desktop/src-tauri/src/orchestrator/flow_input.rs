//! 输入模式与剪贴板面板的打开：以 input 视图显示面板，提交后由前端 `translate_start` 在原位置交叉淡入为翻译视图
//! （宽度变化时 `StartAck.width` 返回新宽度，见 `translate.rs` 的 `ack_width`）；取词为空 / 失败时同样以输入视图打开，
//! 原因放进 `InputPayload.hint`。剪贴板面板由剪贴板服务接管，这里只负责打开（再按一次关闭）。
//!
//! **「再按一次关闭」的判定**：`panel_state().view` 是 prepare 时的视图，视图内切换（输入 → 翻译，`switchView`）之后
//! 不会更新。输入视图因此按编排器自己的状态判定：当前面板会话所属的 run 仍在 Editing（还没提交过，见 [`super::fsm`]）
//! 才算「输入视图正显示」；提交之后再按输入快捷键是打开一个新的输入框。剪贴板视图没有视图内切换（条目送翻译是 Rust
//! 发起的新会话），按 `panel_state().view` 判定即可。

use std::sync::Arc;

use wisp_types::{AppRef, HideReason, InputAnchor, InputPayload, NotReady, PanelPayload, PanelTransition, PanelView};

use super::engine::{Core, Trigger, blocking};
use super::fsm::{Event, Flow, Stage};
use crate::windows::{AnchorSource, PanelShowRequest};

impl Core {
    // ------------------------------------------------------------------------------------------------
    // 输入模式与剪贴板面板

    pub(super) fn start_input(self: &Arc<Self>, t: Trigger) {
        // 再按一次：输入视图正显示（还没提交）时关闭
        if self.input_view_showing() {
            self.env.panel_request_hide(HideReason::Toggle);
            return;
        }
        let (origin, trace) = (t.origin, t.trace);
        let run = self.start_run(Flow::Input, trace);
        let core = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            core.screen.settled().await;
            let prev = core.prev_app_for(origin).await;
            if core.advance(run, Event::Opened).is_some() {
                core.open_input(run, trace, prev, None).await;
            }
        });
    }

    /// 当前显示的面板会话是还没提交的输入视图（专用快捷键打开的，或取词失败改开的）
    pub(super) fn input_view_showing(&self) -> bool {
        self.env
            .panel_state()
            .and_then(|p| self.run_of_session(p.session_id))
            .is_some_and(|(_, flow, stage)| matches!(flow, Flow::Input | Flow::Translate) && stage == Stage::Editing)
    }

    /// 以输入视图打开（专用快捷键；取词为空 / 失败时带提示）
    pub(super) async fn open_input(
        self: &Arc<Self>,
        run: u64,
        trace: u64,
        prev: Option<AppRef>,
        hint: Option<NotReady>,
    ) {
        let settings = self.env.settings();
        let anchor = match settings.translate.input_anchor {
            InputAnchor::Cursor => AnchorSource::Cursor,
            InputAnchor::Center => AnchorSource::Center,
        };
        let req = PanelShowRequest {
            view: PanelView::Input,
            payload: PanelPayload::Input(InputPayload {
                initial_text: String::new(),
                target_lang: None,
                hint,
            }),
            anchor,
            take_focus: true,
            prev_app: prev,
            transition: PanelTransition::Enter,
        };
        self.show(run, trace, req, None, None).await;
    }

    pub(super) fn start_clipboard(self: &Arc<Self>, t: Trigger) {
        if self.env.panel_state().is_some_and(|p| p.view == PanelView::Clipboard) {
            self.env.panel_request_hide(HideReason::Toggle);
            return;
        }
        let trace = t.trace;
        let run = self.start_run(Flow::Clipboard, trace);
        let core = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            // 前台应用与首屏数据都在阻塞线程上取：`first_page` 来自内存缓存，但缓存预加载完成之前唤出时会先打开数据库
            // （约十几毫秒）
            core.screen.settled().await;
            let env = Arc::clone(&core.env);
            let Some((prev, payload)) = blocking(move || (env.frontmost(), env.clipboard_payload())).await else {
                core.drop_run(run);
                return;
            };
            if core.advance(run, Event::Opened).is_none() {
                return;
            }
            let settings = core.env.settings();
            let anchor = match settings.clipboard.panel_position {
                wisp_types::ClipPanelPosition::Center => AnchorSource::Center,
                wisp_types::ClipPanelPosition::Cursor => AnchorSource::Cursor,
                wisp_types::ClipPanelPosition::Remember => AnchorSource::Remembered,
            };
            let req = PanelShowRequest {
                view: PanelView::Clipboard,
                payload: PanelPayload::Clipboard(payload),
                anchor,
                take_focus: true,
                prev_app: prev,
                transition: PanelTransition::Enter,
            };
            core.show(run, trace, req, None, None).await;
        });
    }
}
