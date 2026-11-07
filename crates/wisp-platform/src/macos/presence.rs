//! 用户在不在电脑前、电源状态（`Presence`）。
//!
//! - 输入空闲：`CGEventSourceSecondsSinceLastEventType`，与剪贴板轮询的空闲分档是同一个调用（不需要权限，约 0.3 µs）；
//! - 省电：`NSProcessInfo.lowPowerModeEnabled`（「低电量模式」）；用电池：IOKit `IOPSGetTimeRemainingEstimate`
//!   （接着电源时返回「无限」）。两者都是读本机状态，不发 IPC、不阻塞；
//! - 事件：挂在剪贴板轮询的闸门上（`poll_gate`）。它本来就在慢档的 tick 里读输入空闲、收锁屏与解锁通知，换档时顺带
//!   通知，不多一次唤醒。轮询没在运行（剪贴板监听关着或暂停）时 `reports_return` 为 false，调用方自己低频复查。
//!   解锁一定经闸门的 `set_paused` 通知（锁屏状态只由它改），`reports_unlock` 为 true；低电量模式的开关另由
//!   `NSProcessInfoPowerStateDidChangeNotification` 通知（只在订阅期间登记），`reports_power` 为 true。

use std::ptr::NonNull;
use std::sync::Arc;
use std::time::Duration;

use block2::RcBlock;
use objc2_foundation::{
    NSNotification, NSNotificationCenter, NSProcessInfo, NSProcessInfoPowerStateDidChangeNotification,
};
use parking_lot::Mutex;

use super::Ctx;
use super::ffi;
use super::focus::WorkspaceObserver;
use super::poll_gate;
use super::util::{guarded, pool};
use crate::traits::Presence;
use crate::types::{PowerState, PresenceCallback, PresenceEvent, PresenceWatch};

pub(crate) struct MacPresence(Arc<Ctx>);

impl MacPresence {
    pub(crate) fn new(ctx: Arc<Ctx>) -> Self {
        Self(ctx)
    }
}

impl Presence for MacPresence {
    fn input_idle(&self) -> Option<Duration> {
        poll_gate::input_idle()
    }

    fn power(&self) -> PowerState {
        // SAFETY: 无参数的 C 函数，线程安全（读电源管理发布的共享状态）
        let remaining = unsafe { ffi::IOPSGetTimeRemainingEstimate() };
        let on_battery = remaining
            .is_finite()
            .then_some(remaining != ffi::IOPS_TIME_REMAINING_UNLIMITED);
        PowerState {
            on_battery,
            low_power: Some(NSProcessInfo::processInfo().isLowPowerModeEnabled()),
        }
    }

    fn watch(&self, cb: PresenceCallback) -> Option<Box<dyn PresenceWatch>> {
        let power_cb = Arc::clone(&cb);
        let id = self.0.clip.gate().listen(cb);
        // 通知在发布它的线程上送达（任意线程），回调本身只做 O(1) 的唤醒
        let power = pool(|| {
            let block = RcBlock::new(move |_n: NonNull<NSNotification>| {
                guarded("presence power observer", || power_cb(PresenceEvent::PowerChanged));
            });
            // SAFETY: 系统导出的通知名常量（不可变的静态 NSString）
            let name = unsafe { NSProcessInfoPowerStateDidChangeNotification };
            WorkspaceObserver::on(NSNotificationCenter::defaultCenter(), name, &block)
        });
        Some(Box::new(MacWatch {
            ctx: Arc::clone(&self.0),
            id,
            _power: Mutex::new(power),
        }))
    }
}

struct MacWatch {
    ctx: Arc<Ctx>,
    id: u64,
    /// 低电量模式开关的观察者（丢弃即注销）。观察者令牌不是 Sync，放进锁里让订阅对象可以跨线程共享
    _power: Mutex<WorkspaceObserver>,
}

impl PresenceWatch for MacWatch {
    fn reports_return(&self) -> bool {
        self.ctx.clip.gate().has_sources()
    }

    fn screen_idle(&self) -> bool {
        self.ctx.clip.gate().is_paused()
    }

    fn reports_unlock(&self) -> bool {
        true
    }

    fn reports_power(&self) -> bool {
        true
    }
}

impl Drop for MacWatch {
    fn drop(&mut self) {
        self.ctx.clip.gate().unlisten(self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Platform, PlatformConfig};

    /// 跑测试的机器上能取到值（有图形会话时）；只要求查询本身可用、不 panic
    #[test]
    fn power_and_idle_queries_are_usable() {
        let platform = Platform::create(PlatformConfig {
            own_pid: std::process::id(),
            main_thread: Arc::new(|f: Box<dyn FnOnce() + Send>| f()),
            self_hit: Arc::new(|_, _| false),
            keymaps: Arc::new(parking_lot::RwLock::new(Vec::new())),
            os_version: "26.4".to_owned(),
        });
        let p = &platform.presence;
        assert!(p.power().low_power.is_some());
        let _ = p.input_idle();
        let watch = p.watch(Arc::new(|_| {})).expect("macOS 总能订阅");
        assert!(!watch.reports_return(), "没有开始剪贴板轮询时没有事件来源");
        assert!(!watch.screen_idle());
        assert!(watch.reports_unlock() && watch.reports_power());
    }

    /// 低电量模式开关的通知送到订阅者；注销之后不再送到（测试里自己发布这条通知，不改系统设置）
    #[test]
    fn power_state_notifications_reach_the_subscriber_until_dropped() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let platform = Platform::create(PlatformConfig {
            own_pid: std::process::id(),
            main_thread: Arc::new(|f: Box<dyn FnOnce() + Send>| f()),
            self_hit: Arc::new(|_, _| false),
            keymaps: Arc::new(parking_lot::RwLock::new(Vec::new())),
            os_version: "26.4".to_owned(),
        });
        let seen = Arc::new(AtomicUsize::new(0));
        let sink = Arc::clone(&seen);
        let watch = platform
            .presence
            .watch(Arc::new(move |e| {
                if e == PresenceEvent::PowerChanged {
                    sink.fetch_add(1, Ordering::SeqCst);
                }
            }))
            .expect("macOS 总能订阅");
        let post = || {
            pool(|| {
                // SAFETY: 系统导出的通知名常量（不可变的静态 NSString）
                let name = unsafe { NSProcessInfoPowerStateDidChangeNotification };
                // SAFETY: 名称有效、不带对象；默认通知中心的发布是线程安全的，观察者在本线程上同步收到
                unsafe { NSNotificationCenter::defaultCenter().postNotificationName_object(name, None) };
            });
        };
        post();
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        drop(watch);
        post();
        assert_eq!(seen.load(Ordering::SeqCst), 1, "注销之后不再收到");
    }
}
