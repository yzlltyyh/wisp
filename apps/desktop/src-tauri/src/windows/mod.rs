//! 窗口层：WindowManager、显示时序、两种帧模式、Pinned 交接、Overlay、设置 / 引导窗口、
//! `wisp://` 协议分发、boot 脚本。注意本模块与 `windows` crate 同名：引用自己用 `crate::windows::…`，引用 crate 用 `::windows::…`。
//!
//! | 文件 | 内容 |
//! | --- | --- |
//! | manager.rs | `WindowManager`：请求 → 几何与策略（视图宽度、锚点、关闭监听、焦点），自身窗口矩形表，boot 脚本 |
//! | panel.rs | 面板显示管线的状态机（prepare / ready / resize / hide、超时与重建、空闲卸载）与它的 Tauri 宿主 |
//! | placement.rs | 定位与尺寸的纯函数 |
//! | sizing.rs | 弹簧、缓动、Css 阶梯规则等动效纯函数（与前端令牌对照） |
//! | outside_click.rs | 外部点击 / 鼠标离开 / 全局 Esc / 失焦 |
//! | pinned.rs | Pinned 固定窗口与交接 |
//! | float.rs | 悬浮剪贴板（单例、不抢焦点、关闭即销毁）与面板到它的交接 |
//! | overlay.rs | Overlay 截图覆盖层与预建复用 |
//! | settings_window.rs | 设置 / 引导窗口（无白闪首显、位置记忆、ActivationPolicy） |
//! | protocol.rs | `wisp://` 协议分发 |
//! | native/ | 平台实现（唯一允许 `cfg(target_os)` 的位置） |

pub mod float;
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub mod frame_lab;
pub mod manager;
pub mod native;
pub mod outside_click;
pub mod overlay;
pub mod panel;
pub mod pinned;
pub mod placement;
pub mod protocol;
pub mod settings_window;
pub mod sizing;
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub mod window_lab;

pub use manager::{
    AnchorSource, FrameMode, HiddenSet, OverlaySession, PanelShowRequest, PanelState, WindowManager, boot_script,
};

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use wisp_types::{FrameMode, PanelView, Rect};

    use super::panel::mock::{Call, MockHost};
    use super::panel::{PanelController, PanelEvent, PanelHost};
    use super::placement::{DesktopUnit, PlaceAnchor, Screen, place_panel};

    fn controller(host: &Arc<MockHost>, frame_mode: FrameMode, margin: f64) -> Arc<PanelController> {
        *host.exists.lock() = false;
        let c = PanelController::new(
            Arc::clone(host) as Arc<dyn PanelHost>,
            frame_mode,
            margin,
            false,
            DesktopUnit::Physical,
        );
        c.create().expect("create");
        host.fire(PanelEvent::PageLoaded);
        host.take();
        c
    }

    fn open(session_id: u64) -> super::panel::PanelOpen {
        let screen = Screen {
            id: r"\\.\DISPLAY1".to_owned(),
            frame: Rect::new(0.0, 0.0, 1920.0, 1080.0),
            work: Rect::new(0.0, 0.0, 1920.0, 1040.0),
            scale: 1.25,
            physical_origin: wisp_types::PhysPoint { x: 0, y: 0 },
            primary: true,
        };
        let geometry = place_panel(
            &screen,
            PlaceAnchor::Rect(Rect::new(400.0, 300.0, 120.0, 18.0)),
            420.0,
            120.0,
            624.0,
        );
        super::panel::PanelOpen {
            session_id,
            view: PanelView::Translate,
            payload: wisp_types::PanelPayload::Toast(wisp_types::ToastPayload {
                reason: wisp_types::CopiedReason::CopyOnly,
                paste_key: "Ctrl+V".to_owned(),
                queue: None,
            }),
            geometry,
            focused: false,
            transition: wisp_types::PanelTransition::Enter,
            estimated_height: 120.0,
            prev_app: None,
            guards: super::outside_click::GuardSpec::default(),
        }
    }

    /// Windows：`SetIsVisible(true)` 与内存目标 NORMAL（宿主的 `restore_visibility`）必须早于 emit
    /// `panel:prepare`；隐藏状态下的定位也必须早于 emit（前端按最终宽度同步测量）。隐藏后 `SetIsVisible(false)` + LOW。
    /// 用记录调用顺序的 mock 宿主验证（宿主在 Windows 上把 `restore_visibility` 实现为 WebView2 的这两步）
    #[test]
    fn visibility_order() {
        let host = MockHost::existing();
        let c = controller(&host, FrameMode::Css, 24.0);
        c.set_options(super::panel::PanelOptions {
            low_memory_when_hidden: true,
            ..Default::default()
        });
        for session in 1..=3 {
            c.prepare(open(session));
            let calls = host.take();
            let pos = |want: &Call| calls.iter().position(|c| c == want);
            let restore = pos(&Call::RestoreVisibility).expect("restore visibility");
            let emit = pos(&Call::EmitPrepare(session)).expect("emit prepare");
            let frame = calls
                .iter()
                .position(|c| matches!(c, Call::SetFrame(_)))
                .expect("set frame");
            let focusable = pos(&Call::SetFocusable(false)).expect("set focusable");
            assert!(
                restore < emit,
                "SetIsVisible(true) + NORMAL 必须早于 emit prepare：{calls:?}"
            );
            assert!(restore < frame && frame < emit, "隐藏状态下先定位再 emit：{calls:?}");
            assert!(focusable < emit, "显示前先决定是否可激活：{calls:?}");
            // ready → 同步显示；hide → 隐藏后才降低可见性与内存目标
            c.ready(session, 140.0).expect("shown");
            c.hide_now(session, wisp_types::HideReason::Escape);
            let calls = host.take();
            let hide = calls.iter().position(|c| matches!(c, Call::Hide(_))).expect("hide");
            let release = calls
                .iter()
                .position(|c| *c == Call::ReleaseVisibility(true))
                .expect("release visibility");
            assert!(hide < release, "先隐藏窗口，再 SetIsVisible(false) + LOW：{calls:?}");
        }
    }
}
