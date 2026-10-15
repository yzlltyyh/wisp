//! Overlay 截图覆盖层。
//!
//! - 每个显示器一个，label 为 `overlay-<n>`（只有序号：Windows 的 MonitorId 形如 `\\.\DISPLAY1`，不能放进 label）；
//!   label → 显示器 / 会话的对应由本登记表维护，`overlay_init` 经 `WindowManager::overlay_monitor(label)` 反查；
//! - 覆盖显示器的完整区域（含菜单栏 / 任务栏）、无边框、透明、置顶、不进任务栏；macOS 为非激活 NSPanel（层级 101，
//!   `canJoinAllSpaces | fullScreenAuxiliary | stationary`），成为 key 接收键盘但不激活应用；
//! - **macOS 原生冻结帧**：显示前把 SCK 的 CGImage 设为窗口底层视图的 `layer.contents`（零拷贝，首帧即冻结帧）；
//!   Windows / X11 由 WebView canvas 画（先 1/2 降采样帧再全分辨率），页面首帧渲染完成（`overlay_ready`）后再显示；
//! - **预建**（Windows 默认，`native::PREWARM_OVERLAY`）：空闲时隐藏创建一个主显示器的 Overlay；会话复用它时先登记、
//!   再 emit_to 该窗口 `overlay:session`（`OverlaySessionStart`），页面据此重置并重新 `overlay_init`；结束后改为隐藏复用，
//!   其他的销毁（macOS 先 `remove_webview_panel` 再销毁）。
//! - **多屏**（Windows 三屏下也要在 150 ms 内都出现冻结画面）：开启预建的平台上，会话结束时
//!   每块屏的 Overlay 都改为隐藏复用（最多 [`MAX_REUSABLE`] 个），第二次起每块副屏都有现成的窗口，不再每次同步新建
//!   WebView2（每个 100–400 ms）；会话开始时显示器已经不在的空闲窗口销毁（显示器配置变化）。只用过单屏的用户只常驻
//!   主屏那一个。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use tauri::{Emitter, Manager, WebviewWindowBuilder, WindowEvent};
use wisp_types::{Boot, MonitorId, MonitorInfo, OverlaySessionStart, Rect, event};

use super::native;
use super::placement::Screen;
use crate::error::IpcError;

/// 预建平台上最多保留多少个隐藏复用的 Overlay（每块屏一个）
pub const MAX_REUSABLE: usize = 4;

/// Overlay 窗口模板的 label（tauri.conf.json，`create: false`）
pub const OVERLAY_TEMPLATE: &str = "overlay";
/// Overlay 窗口 label 的前缀
pub const OVERLAY_PREFIX: &str = "overlay-";

/// 一个 Overlay 窗口
#[derive(Debug, Clone)]
struct Slot {
    /// 本次会话绑定的显示器（预建、空闲中为 None）
    monitor: Option<MonitorInfo>,
    session_id: Option<u64>,
    /// 预建的那个：会话结束后隐藏复用，不销毁
    prewarmed: bool,
    /// 当前在屏幕上
    shown: bool,
}

/// 复用 / 新建的结果
#[derive(Debug, Clone, PartialEq, Eq)]
enum Plan {
    /// 复用预建的窗口（emit `overlay:session`）
    Reuse(String),
    /// 新建
    Create,
}

/// Overlay 登记表
#[derive(Default)]
pub struct OverlayRegistry {
    slots: Mutex<HashMap<String, Slot>>,
    next: AtomicU64,
}

impl OverlayRegistry {
    /// 空表
    pub fn new() -> Self {
        Self::default()
    }

    /// 显示器的完整区域（桌面坐标）
    pub fn monitor_rect(m: &MonitorInfo) -> Rect {
        Screen::from_info(m).desktop_frame(native::DESKTOP_UNIT)
    }

    fn next_label(&self) -> String {
        format!("{OVERLAY_PREFIX}{}", self.next.fetch_add(1, Ordering::Relaxed))
    }

    /// 为这块显示器选一个窗口：空闲的预建窗口优先给主显示器（或它原本所在的显示器）
    fn plan(&self, m: &MonitorInfo) -> Plan {
        let slots = self.slots.lock();
        let idle = slots
            .iter()
            .filter(|(_, s)| s.prewarmed && s.session_id.is_none())
            .map(|(label, s)| (label, s.monitor.as_ref().map(|x| x.id.as_str())))
            .collect::<Vec<_>>();
        idle.iter()
            .find(|(_, prev)| *prev == Some(m.id.as_str()))
            .or_else(|| m.is_primary.then(|| idle.first()).flatten())
            .map_or(Plan::Create, |(label, _)| Plan::Reuse((*label).clone()))
    }

    /// 创建一个隐藏的 Overlay 窗口并登记（`session` 为 None 时是预建）
    fn create_window(
        &self,
        app: &tauri::AppHandle,
        boot_for: &dyn Fn(&str) -> Boot,
        monitor: &MonitorInfo,
        session_id: Option<u64>,
    ) -> Result<String, IpcError> {
        let label = self.next_label();
        let boot = boot_for(&label);
        let mut config = app
            .config()
            .app
            .windows
            .iter()
            .find(|w| w.label == OVERLAY_TEMPLATE)
            .cloned()
            .ok_or_else(|| IpcError::not_found("window template overlay"))?;
        config.label.clone_from(&label);
        // 页面加载后立即 overlay_init，所以先登记再建窗口
        self.slots.lock().insert(
            label.clone(),
            Slot {
                monitor: Some(monitor.clone()),
                session_id,
                prewarmed: session_id.is_none(),
                shown: false,
            },
        );
        let builder = WebviewWindowBuilder::from_config(app, &config)?
            .initialization_script(super::boot_script(&boot))
            .visible(false);
        let builder = native::template_overrides(builder, native::TemplateKind::Overlay, boot.frame_mode);
        // 上一次截图的 Overlay 刚销毁时，等它的 `Destroyed` 处理完再建：新窗口可能分到它的地址，迟到的 `Destroyed`
        // 会按地址误伤新窗口（见 native::note_destroying）。没有待收尾的销毁时立即返回
        native::await_destroyed(app);
        let built = builder.build();
        let win = match built {
            Ok(w) => w,
            Err(e) => {
                self.slots.lock().remove(&label);
                return Err(e.into());
            }
        };
        // 每次截图都会新建 / 销毁 Overlay：不接管就每次泄漏一个整屏窗口（macOS，见 native::adopt_window）
        native::adopt_window(&win);
        if let Err(e) = native::overlay_attach(&win) {
            log::warn!("overlay attach failed: {}", e.message);
        }
        if let Err(e) = native::set_window_frame(&win, Self::monitor_rect(monitor)) {
            log::warn!("place overlay failed: {}", e.message);
        }
        let handle = app.clone();
        let owned = label.clone();
        win.on_window_event(move |ev| {
            if matches!(ev, WindowEvent::Destroyed) {
                native::release_offscreen_drivers(&handle);
                if let Some(state) = handle.try_state::<crate::state::AppState>() {
                    state.windows.forget_overlay(&owned);
                }
            }
        });
        Ok(label)
    }

    /// 空闲时预建隐藏的主显示器 Overlay（Windows 默认）；已有预建的就不再建
    pub fn prewarm(
        &self,
        app: &tauri::AppHandle,
        boot_for: &dyn Fn(&str) -> Boot,
        primary: &MonitorInfo,
    ) -> Result<(), IpcError> {
        if self.slots.lock().values().any(|s| s.prewarmed) {
            return Ok(());
        }
        let label = self.create_window(app, boot_for, primary, None)?;
        log::info!("overlay prewarmed label={label}");
        Ok(())
    }

    /// 为一次截图会话准备每个显示器的 Overlay（隐藏状态）
    pub fn create(
        &self,
        app: &tauri::AppHandle,
        boot_for: &dyn Fn(&str) -> Boot,
        session_id: u64,
        monitors: &[MonitorInfo],
    ) -> Result<(), IpcError> {
        // 显示器已经不在的空闲窗口（显示器配置变了）：销毁，不再占内存
        let retired = {
            let mut slots = self.slots.lock();
            let gone = retire_disconnected(&slots, monitors);
            for label in &gone {
                slots.remove(label);
            }
            gone
        };
        for label in retired {
            if let Some(win) = app.get_webview_window(&label) {
                log::info!("overlay {label}: its display is gone; destroyed");
                native::overlay_destroy(&win);
            }
        }
        for m in monitors {
            match self.plan(m) {
                Plan::Reuse(label) => {
                    if let Some(slot) = self.slots.lock().get_mut(&label) {
                        slot.monitor = Some(m.clone());
                        slot.session_id = Some(session_id);
                    }
                    if let Some(win) = app.get_webview_window(&label)
                        && let Err(e) = native::set_window_frame(&win, Self::monitor_rect(m))
                    {
                        log::warn!("place overlay failed: {}", e.message);
                    }
                    // 先登记、再通知：页面收到事件后 overlay_init 一定能查到本次会话
                    let start = OverlaySessionStart {
                        session_id,
                        monitor_id: m.id.clone(),
                    };
                    if let Err(e) = app.emit_to(label.as_str(), event::OVERLAY_SESSION, &start) {
                        log::warn!("emit overlay:session failed: {e}");
                    }
                }
                Plan::Create => {
                    self.create_window(app, boot_for, m, Some(session_id))?;
                }
            }
        }
        Ok(())
    }

    /// Overlay 窗口当前会话绑定的显示器
    pub fn monitor_of(&self, label: &str) -> Option<MonitorId> {
        let slots = self.slots.lock();
        let s = slots.get(label)?;
        s.session_id?;
        s.monitor.as_ref().map(|m| m.id.clone())
    }

    /// Overlay 窗口当前绑定的会话
    pub fn session_of(&self, label: &str) -> Option<u64> {
        self.slots.lock().get(label)?.session_id
    }

    /// 本次会话里某块显示器的 Overlay：(label, 显示器)
    pub fn find(&self, session_id: u64, monitor_id: &str) -> Option<(String, MonitorInfo)> {
        self.slots.lock().iter().find_map(|(label, s)| {
            let m = s.monitor.as_ref()?;
            (s.session_id == Some(session_id) && m.id == monitor_id).then(|| (label.clone(), m.clone()))
        })
    }

    /// 标记为已显示
    pub fn mark_shown(&self, label: &str, shown: bool) {
        if let Some(s) = self.slots.lock().get_mut(label) {
            s.shown = shown;
        }
    }

    /// 有会话的 Overlay 覆盖着的显示器（自身窗口命中判断用）
    pub fn active_monitors(&self) -> Vec<MonitorId> {
        self.slots
            .lock()
            .values()
            .filter(|s| s.session_id.is_some() && s.shown)
            .filter_map(|s| s.monitor.as_ref().map(|m| m.id.clone()))
            .collect()
    }

    /// 正在屏幕上的 Overlay 的 label
    pub fn shown_labels(&self) -> Vec<String> {
        self.slots
            .lock()
            .iter()
            .filter(|(_, s)| s.shown && s.session_id.is_some())
            .map(|(label, _)| label.clone())
            .collect()
    }

    /// 结束会话：预建的隐藏复用，其他销毁。`fade`：commit 之后的正常收尾，销毁前先淡出窗口（macOS 的原生冻结帧，
    /// `native::overlay_destroy_fading`）
    pub fn destroy_all(&self, app: &tauri::AppHandle, fade: Option<native::Fade>) {
        let (reuse, destroy): (Vec<String>, Vec<String>) = {
            let mut slots = self.slots.lock();
            end_session(&mut slots, native::PREWARM_OVERLAY)
        };
        for label in reuse {
            if let Some(win) = app.get_webview_window(&label) {
                native::overlay_hide(&win);
            }
        }
        for label in destroy {
            if let Some(win) = app.get_webview_window(&label) {
                match fade {
                    Some(fade) => native::overlay_destroy_fading(&win, fade),
                    None => native::overlay_destroy(&win),
                }
            }
        }
    }

    /// 窗口被销毁（含意外关闭）：移除登记
    pub fn forget(&self, label: &str) {
        self.slots.lock().remove(label);
    }
}

/// 会话结束：返回 (隐藏复用的, 销毁的)。`keep_all`（开启预建的平台）时这次会话的每块屏都改为隐藏复用，最多
/// [`MAX_REUSABLE`] 个（预建的优先）；否则只有预建的那个复用
fn end_session(slots: &mut HashMap<String, Slot>, keep_all: bool) -> (Vec<String>, Vec<String>) {
    let mut labels: Vec<String> = slots.keys().cloned().collect();
    // 预建的排前面，其余按 label 顺序（稳定）
    labels.sort_by_key(|l| (!slots[l].prewarmed, l.clone()));
    let mut reuse = Vec::new();
    let mut destroy = Vec::new();
    for label in labels {
        let keep = {
            let s = &slots[&label];
            (s.prewarmed || keep_all) && reuse.len() < MAX_REUSABLE
        };
        if keep {
            if let Some(s) = slots.get_mut(&label) {
                s.prewarmed = true;
                s.session_id = None;
                s.shown = false;
            }
            reuse.push(label);
        } else {
            slots.remove(&label);
            destroy.push(label);
        }
    }
    (reuse, destroy)
}

/// 空闲的复用窗口里，显示器已经不在本次显示器列表里的
fn retire_disconnected(slots: &HashMap<String, Slot>, monitors: &[MonitorInfo]) -> Vec<String> {
    slots
        .iter()
        .filter(|(_, s)| s.prewarmed && s.session_id.is_none())
        .filter(|(_, s)| {
            s.monitor
                .as_ref()
                .is_some_and(|m| !monitors.iter().any(|x| x.id == m.id))
        })
        .map(|(l, _)| l.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use wisp_types::{PhysPoint, PhysSize};

    use super::*;

    fn monitor(id: &str, primary: bool) -> MonitorInfo {
        MonitorInfo {
            id: id.to_owned(),
            name: id.to_owned(),
            physical_origin: PhysPoint { x: 0, y: 0 },
            physical_size: PhysSize {
                width: 1920,
                height: 1080,
            },
            scale_factor: 1.0,
            frame: Rect::new(0.0, 0.0, 1920.0, 1080.0),
            work_area: Rect::new(0.0, 0.0, 1920.0, 1040.0),
            is_primary: primary,
        }
    }

    #[test]
    fn prewarmed_overlay_is_reused_for_primary_only() {
        let reg = OverlayRegistry::new();
        reg.slots.lock().insert(
            "overlay-0".to_owned(),
            Slot {
                monitor: Some(monitor(r"\\.\DISPLAY1", true)),
                session_id: None,
                prewarmed: true,
                shown: false,
            },
        );
        // 空闲中：overlay_init 返回 null（没有会话）
        assert_eq!(reg.monitor_of("overlay-0"), None);
        assert_eq!(
            reg.plan(&monitor(r"\\.\DISPLAY1", true)),
            Plan::Reuse("overlay-0".to_owned())
        );
        assert_eq!(reg.plan(&monitor(r"\\.\DISPLAY2", false)), Plan::Create);
        // 绑定会话后不再被复用
        if let Some(s) = reg.slots.lock().get_mut("overlay-0") {
            s.session_id = Some(7);
        }
        assert_eq!(reg.plan(&monitor(r"\\.\DISPLAY1", true)), Plan::Create);
        assert_eq!(reg.monitor_of("overlay-0").as_deref(), Some(r"\\.\DISPLAY1"));
        assert_eq!(
            reg.find(7, r"\\.\DISPLAY1").map(|(l, _)| l),
            Some("overlay-0".to_owned())
        );
        assert!(reg.active_monitors().is_empty(), "还没显示");
        reg.mark_shown("overlay-0", true);
        assert_eq!(reg.active_monitors(), vec![r"\\.\DISPLAY1".to_owned()]);
    }

    fn slot(id: &str, session: Option<u64>, prewarmed: bool) -> Slot {
        Slot {
            monitor: Some(monitor(id, id.ends_with('1'))),
            session_id: session,
            prewarmed,
            shown: session.is_some(),
        }
    }

    /// 开启预建的平台上，多屏会话结束后每块屏的 Overlay 都留着复用（最多 MAX_REUSABLE 个）；不开启时只留预建的
    #[test]
    fn every_display_keeps_a_reusable_overlay_when_prewarming() {
        let mut slots: HashMap<String, Slot> = HashMap::new();
        slots.insert("overlay-0".into(), slot(r"\\.\DISPLAY1", Some(7), true));
        slots.insert("overlay-1".into(), slot(r"\\.\DISPLAY2", Some(7), false));
        slots.insert("overlay-2".into(), slot(r"\\.\DISPLAY3", Some(7), false));
        let mut keep = slots.clone();
        let (reuse, destroy) = end_session(&mut keep, true);
        assert_eq!(reuse.len(), 3);
        assert!(destroy.is_empty());
        assert!(keep.values().all(|s| s.prewarmed && s.session_id.is_none() && !s.shown));
        let reg = OverlayRegistry::new();
        *reg.slots.lock() = keep;
        assert_eq!(
            reg.plan(&monitor(r"\\.\DISPLAY3", false)),
            Plan::Reuse("overlay-2".to_owned()),
            "副屏复用自己的窗口"
        );
        // 不预建（macOS / Linux）：只留预建的（这里没有），其余销毁
        let mut once = slots.clone();
        let (reuse, destroy) = end_session(&mut once, false);
        assert_eq!(reuse, vec!["overlay-0".to_owned()]);
        assert_eq!(destroy.len(), 2);
        // 上限
        let mut many: HashMap<String, Slot> = (0..6)
            .map(|i| (format!("overlay-{i}"), slot(&format!("D{i}"), Some(1), false)))
            .collect();
        let (reuse, destroy) = end_session(&mut many, true);
        assert_eq!((reuse.len(), destroy.len()), (MAX_REUSABLE, 6 - MAX_REUSABLE));
        // 显示器拔掉了：空闲的那个退役
        let idle: HashMap<String, Slot> = [("overlay-1".to_owned(), slot("D2", None, true))].into();
        assert_eq!(
            retire_disconnected(&idle, &[monitor("D1", true)]),
            vec!["overlay-1".to_owned()]
        );
        assert!(retire_disconnected(&idle, &[monitor("D2", false)]).is_empty());
    }

    #[test]
    fn labels_are_sequence_numbers() {
        let reg = OverlayRegistry::new();
        assert_eq!(reg.next_label(), "overlay-0");
        assert_eq!(reg.next_label(), "overlay-1");
        assert_eq!(reg.monitor_of("overlay-9"), None);
    }
}
