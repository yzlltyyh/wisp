//! 用户在不在电脑前、电源状态（`Presence`）。
//!
//! - 输入空闲：`CGEventSourceSecondsSinceLastEventType`，与剪贴板轮询的空闲分档是同一个调用（不需要权限，约 0.3 µs）；
//! - 省电：`NSProcessInfo.lowPowerModeEnabled`（「低电量模式」）；用电池：IOKit `IOPSGetTimeRemainingEstimate`
//!   （接着电源时返回「无限」）。两者都是读本机状态，不发 IPC、不阻塞；
//! - 事件：挂在剪贴板轮询的闸门上（`poll_gate`）。它本来就在慢档的 tick 里读输入空闲、收锁屏与解锁通知，换档时顺带
//!   通知，不多一次唤醒。轮询没在运行（剪贴板监听关着或暂停）时 `reports_return` 为 false，调用方自己低频复查。

use std::sync::Arc;
use std::time::Duration;

use objc2_foundation::NSProcessInfo;

use super::Ctx;
use super::ffi;
use super::poll_gate;
use crate::traits::Presence;
use crate::types::{PowerState, PresenceCallback, PresenceWatch};

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
        let id = self.0.clip.gate().listen(cb);
        Some(Box::new(MacWatch {
            ctx: Arc::clone(&self.0),
            id,
        }))
    }
}

struct MacWatch {
    ctx: Arc<Ctx>,
    id: u64,
}

impl PresenceWatch for MacWatch {
    fn reports_return(&self) -> bool {
        self.ctx.clip.gate().has_sources()
    }

    fn screen_idle(&self) -> bool {
        self.ctx.clip.gate().is_paused()
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
    }
}
