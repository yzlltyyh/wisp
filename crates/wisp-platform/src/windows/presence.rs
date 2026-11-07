//! 用户在不在电脑前、电源状态（`Presence`）。
//!
//! - 输入空闲：`GetLastInputInfo` 与 `GetTickCount` 之差（两者都是 32 位毫秒计数，约 49.7 天回绕一次，按回绕相减）。
//!   锁屏期间不再更新，正好落进「离开」；
//! - 电源：`GetSystemPowerStatus`（`ACLineStatus == 0` 为用电池，`SystemStatusFlag == 1` 为节电模式）；
//! - 事件：一个**隐藏的顶层窗口**（message-only 窗口收不到 `WM_POWERBROADCAST` 广播）跑在专用线程上，空闲时阻塞在
//!   `GetMessageW` 中，零唤醒。`WTSRegisterSessionNotification` 收锁屏 / 解锁，电源广播收睡眠唤醒与插拔电源。
//!   只在订阅期间存在，丢弃订阅即销毁窗口、结束线程。没有锁屏的「回到电脑前」没有事件可收，`reports_return` 为 false，
//!   由调用方在安静 / 离开时低频复查输入空闲（一次系统调用，不联网）；锁屏期间解锁有会话通知（`reports_unlock`），
//!   调用方不必复查。节电模式的开关没有确定会送达的广播（`PBT_APMPOWERSTATUSCHANGE` 只保证插拔电源与电量变化），
//!   `reports_power` 为 false，由调用方低频复查电源状态。
//!
//! 订阅要等新线程建好窗口（通常几毫秒，最多 2 s），会阻塞调用线程：调用方应当在阻塞线程池里调用 `watch`。

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use ::windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use ::windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use ::windows::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use ::windows::Win32::System::SystemInformation::GetTickCount;
use ::windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use ::windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, DestroyWindow, PBT_APMPOWERSTATUSCHANGE, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND,
    PostMessageW, PostQuitMessage, WM_CLOSE, WM_DESTROY, WM_POWERBROADCAST, WM_WTSSESSION_CHANGE, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_POPUP, WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
};

use super::util::{RawHwnd, call_guarded};
use super::wnd;
use crate::traits::Presence;
use crate::types::{PowerState, PresenceCallback, PresenceEvent, PresenceWatch};

const CLASS: &str = "WispPresenceSink";

pub(crate) struct WinPresence;

impl Presence for WinPresence {
    fn input_idle(&self) -> Option<Duration> {
        let mut info = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        // SAFETY: 输出参数指向局部变量，cbSize 已按约定填好
        if !unsafe { GetLastInputInfo(&mut info) }.as_bool() {
            return None;
        }
        // SAFETY: 无参数
        let now = unsafe { GetTickCount() };
        Some(Duration::from_millis(u64::from(idle_ticks(now, info.dwTime))))
    }

    fn power(&self) -> PowerState {
        let mut status = SYSTEM_POWER_STATUS::default();
        // SAFETY: 输出参数指向局部变量
        if unsafe { GetSystemPowerStatus(&mut status) }.is_err() {
            return PowerState::default();
        }
        power_of(status.ACLineStatus, status.SystemStatusFlag)
    }

    fn watch(&self, cb: PresenceCallback) -> Option<Box<dyn PresenceWatch>> {
        match spawn(cb) {
            Ok(w) => Some(Box::new(w)),
            Err(e) => {
                log::debug!("presence watcher not started: {e}");
                None
            }
        }
    }
}

/// 两个 32 位毫秒计数之差（按回绕处理）
pub(crate) fn idle_ticks(now: u32, last_input: u32) -> u32 {
    now.wrapping_sub(last_input)
}

/// `SYSTEM_POWER_STATUS` 的两个字段 → 电源状态（`ACLineStatus` 255 = 未知）
pub(crate) fn power_of(ac_line: u8, status_flag: u8) -> PowerState {
    PowerState {
        on_battery: match ac_line {
            0 => Some(true),
            1 => Some(false),
            _ => None,
        },
        low_power: Some(status_flag == 1),
    }
}

/// 窗口线程里的状态
struct Sink {
    cb: PresenceCallback,
    locked: Arc<AtomicBool>,
}

thread_local! {
    static SINK: RefCell<Option<Sink>> = const { RefCell::new(None) };
}

struct WinWatch {
    hwnd: RawHwnd,
    locked: Arc<AtomicBool>,
    /// 会话通知登记成功（锁屏 / 解锁都会送达）
    sessions: bool,
}

impl PresenceWatch for WinWatch {
    fn reports_return(&self) -> bool {
        false
    }

    fn screen_idle(&self) -> bool {
        self.locked.load(Ordering::Acquire)
    }

    fn reports_unlock(&self) -> bool {
        self.sessions
    }
}

impl Drop for WinWatch {
    fn drop(&mut self) {
        // SAFETY: 向监听窗口投递关闭消息；窗口已销毁时调用失败，忽略即可
        unsafe {
            let _ = PostMessageW(Some(self.hwnd.hwnd()), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

fn spawn(cb: PresenceCallback) -> Result<WinWatch, String> {
    let locked = Arc::new(AtomicBool::new(false));
    let sink_locked = Arc::clone(&locked);
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(RawHwnd, bool), String>>(1);
    std::thread::Builder::new()
        .name("wisp-presence".into())
        .spawn(move || {
            if !wnd::ensure_class(CLASS, Some(wndproc), None) {
                let _ = ready_tx.send(Err("register class failed".into()));
                return;
            }
            // 顶层、从不显示的工具窗口：能收到电源广播，不进任务栏 / Alt-Tab
            let hwnd = match wnd::create_window(CLASS, WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, WS_POPUP, None) {
                Ok(h) => h,
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return;
                }
            };
            // SAFETY: 本线程创建的窗口；注销在 WM_CLOSE 里
            let sessions = match unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) } {
                Ok(()) => true,
                Err(e) => {
                    // 没有会话通知时仍有电源广播：照常运行
                    log::debug!("session notifications unavailable: {e}");
                    false
                }
            };
            SINK.with(|s| {
                *s.borrow_mut() = Some(Sink {
                    cb,
                    locked: sink_locked,
                });
            });
            let _ = ready_tx.send(Ok((RawHwnd::new(hwnd), sessions)));
            wnd::run_message_loop(|_| false);
            SINK.with(|s| *s.borrow_mut() = None);
        })
        .map_err(|e| format!("spawn presence watcher: {e}"))?;
    match ready_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok((hwnd, sessions))) => Ok(WinWatch { hwnd, locked, sessions }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("presence watcher did not start".into()),
    }
}

/// 在窗口线程上通知订阅者（panic 不能越过窗口过程的边界）
fn deliver(event: PresenceEvent, locked: Option<bool>) {
    let target = SINK.with(|s| {
        s.borrow().as_ref().map(|sink| {
            if let Some(l) = locked {
                sink.locked.store(l, Ordering::Release);
            }
            Arc::clone(&sink.cb)
        })
    });
    if let Some(cb) = target {
        call_guarded("presence callback", || cb(event));
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_WTSSESSION_CHANGE => {
            match u32::try_from(wparam.0).unwrap_or(0) {
                WTS_SESSION_LOCK => deliver(PresenceEvent::Locked, Some(true)),
                WTS_SESSION_UNLOCK => deliver(PresenceEvent::Unlocked, Some(false)),
                _ => {}
            }
            LRESULT(0)
        }
        WM_POWERBROADCAST => {
            match u32::try_from(wparam.0).unwrap_or(0) {
                PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => deliver(PresenceEvent::Woke, None),
                PBT_APMPOWERSTATUSCHANGE => deliver(PresenceEvent::PowerChanged, None),
                _ => {}
            }
            // TRUE：同意（对已经不再询问的新系统无影响）
            LRESULT(1)
        }
        WM_CLOSE => {
            // SAFETY: 本线程创建、本线程注册过会话通知的窗口
            unsafe {
                let _ = WTSUnRegisterSessionNotification(hwnd);
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: 结束本线程的消息循环
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: 其余消息交给默认处理
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_ticks_handle_wraparound() {
        assert_eq!(idle_ticks(5_000, 2_000), 3_000);
        assert_eq!(idle_ticks(1_000, u32::MAX - 999), 2_000, "GetTickCount 回绕之后");
        assert_eq!(idle_ticks(7, 7), 0);
    }

    #[test]
    fn power_status_fields() {
        assert_eq!(
            power_of(0, 1),
            PowerState {
                on_battery: Some(true),
                low_power: Some(true)
            }
        );
        assert_eq!(power_of(1, 0).on_battery, Some(false));
        assert_eq!(power_of(255, 0).on_battery, None, "未知");
        assert_eq!(power_of(1, 0).low_power, Some(false));
    }

    /// 真实的查询：能取到值、不 panic，隐藏窗口线程能启动
    #[test]
    #[ignore = "需要交互式 Windows 会话"]
    fn live_queries_are_usable() {
        let p = WinPresence;
        let _ = p.power();
        let _ = p.input_idle();
        let watch = p.watch(Arc::new(|_| {})).expect("隐藏窗口线程能启动");
        assert!(!watch.reports_return());
        assert!(!watch.screen_idle());
    }
}
