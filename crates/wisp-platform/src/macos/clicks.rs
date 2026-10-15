//! 外部点击监听：`NSEvent.addGlobalMonitorForEventsMatchingMask`，只订阅鼠标按下 / 抬起，不需要任何权限。
//! 全局监视器只报告发往**其他**应用的事件，点在 Wisp 自己窗口上的不会出现。监视器的登记与注销都在主线程上完成，
//! Handle drop 后立即注销（面板隐藏后不留任何监听）。

use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSEvent, NSEventMask, NSEventType};
use objc2_core_graphics::CGEvent;
use parking_lot::Mutex;

use super::Ctx;
use super::displays;
use super::util::{MainOnly, guarded};
use crate::error::PlatformError;
use crate::traits::OutsideClick;
use crate::types::{ClickEvent, ClickKind, HookHandle, PointerButton};

/// 主线程登记监视器的最长等待
const INSTALL_WAIT: Duration = Duration::from_millis(500);

/// OutsideClick 实现
pub(crate) struct MacClicks {
    ctx: Arc<Ctx>,
}

impl MacClicks {
    pub(crate) fn new(ctx: Arc<Ctx>) -> Self {
        Self { ctx }
    }
}

/// NSEvent → ClickEvent（不相关的事件返回 None）
fn click_of(ev: &NSEvent) -> Option<ClickEvent> {
    let (kind, button) = match ev.r#type() {
        NSEventType::LeftMouseDown => (ClickKind::Down, PointerButton::Left),
        NSEventType::LeftMouseUp => (ClickKind::Up, PointerButton::Left),
        NSEventType::RightMouseDown => (ClickKind::Down, PointerButton::Right),
        NSEventType::RightMouseUp => (ClickKind::Up, PointerButton::Right),
        t @ (NSEventType::OtherMouseDown | NSEventType::OtherMouseUp) => {
            let kind = if t == NSEventType::OtherMouseDown {
                ClickKind::Down
            } else {
                ClickKind::Up
            };
            let button = match ev.buttonNumber() {
                2 => PointerButton::Middle,
                3 => PointerButton::Back,
                4 => PointerButton::Forward,
                _ => PointerButton::Other,
            };
            (kind, button)
        }
        _ => return None,
    };
    let at = ev
        .CGEvent()
        .map(|cg| CGEvent::location(Some(&cg)))
        .unwrap_or_else(|| displays::cocoa_to_quartz(NSEvent::mouseLocation()));
    Some(ClickEvent {
        kind,
        at: displays::locate(at),
        button,
    })
}

/// 已登记的监视器（只在主线程上存取与注销）
type MonitorSlot = Arc<Mutex<Option<MainOnly<Retained<AnyObject>>>>>;

fn remove_monitor(slot: &MonitorSlot, mtm: objc2::MainThreadMarker) {
    if let Some(m) = slot.lock().take() {
        let m = m.into_inner(mtm);
        // SAFETY: m 是 addGlobalMonitorForEventsMatchingMask 返回的监视器对象
        unsafe { NSEvent::removeMonitor(&m) };
    }
}

impl OutsideClick for MacClicks {
    fn watch(&self, cb: Box<dyn Fn(ClickEvent) + Send + Sync>) -> Result<HookHandle, PlatformError> {
        let cb: Arc<dyn Fn(ClickEvent) + Send + Sync> = Arc::from(cb);
        let slot: MonitorSlot = Arc::new(Mutex::new(None));
        let cancelled = Arc::new(AtomicBool::new(false));

        let install_slot = slot.clone();
        let install_cancelled = cancelled.clone();
        let installed = self.ctx.main.run_sync(INSTALL_WAIT, move |mtm| {
            if install_cancelled.load(Ordering::Acquire) {
                return false;
            }
            let block = RcBlock::new(move |ev: NonNull<NSEvent>| {
                // 回调在 AppKit 的事件派发帧里调用上层的 cb：panic 不能展开穿过 AppKit
                guarded("outside-click monitor", || {
                    // SAFETY: 事件对象在回调期间有效
                    let ev = unsafe { ev.as_ref() };
                    if let Some(click) = click_of(ev) {
                        cb(click);
                    }
                });
            });
            let mask = NSEventMask::LeftMouseDown
                | NSEventMask::LeftMouseUp
                | NSEventMask::RightMouseDown
                | NSEventMask::RightMouseUp
                | NSEventMask::OtherMouseDown
                | NSEventMask::OtherMouseUp;
            match NSEvent::addGlobalMonitorForEventsMatchingMask_handler(mask, &block) {
                Some(m) => {
                    *install_slot.lock() = Some(MainOnly::new(m, mtm));
                    true
                }
                None => false,
            }
        });

        let main = self.ctx.main.clone();
        let release = move || {
            cancelled.store(true, Ordering::Release);
            let slot = slot.clone();
            main.run_async(move |mtm| remove_monitor(&slot, mtm));
        };
        match installed {
            Some(true) => Ok(HookHandle::new(release)),
            Some(false) => Err(PlatformError::Other("addGlobalMonitorForEvents returned nil".into())),
            None => {
                // 主线程没来得及登记：撤销（若之后才登记上，排在后面的注销会把它移除）
                release();
                Err(PlatformError::Timeout)
            }
        }
    }
}
