//! 事件驱动的系统通知观察者（空闲零唤醒）。
//!
//! 分布式通知（外观、强调色、输入源变化）用 CFNotificationCenter 的分布式中心登记，并指定
//! `DeliverImmediately`：Wisp 是不常激活的 accessory 应用，默认的挂起策略会把通知一直攒到应用被激活，
//! 「跟随系统明暗」就会迟到。回调在主线程的 run loop 上执行。

use std::ffi::c_void;
use std::sync::Arc;

use objc2_core_foundation::{
    CFDictionary, CFNotificationCenter, CFNotificationName, CFNotificationSuspensionBehavior, CFString,
};

use super::util::{MainThread, guarded};

/// 回调类型
pub(crate) type Callback = Arc<dyn Fn() + Send + Sync>;

/// 一组分布式通知的观察者；drop 时注销
pub(crate) struct DistributedObserver {
    /// `Box<Callback>` 的裸指针（作为 observer 标识与回调上下文）
    ctx: usize,
    main: MainThread,
}

// SAFETY: ctx 只是一个地址，注销与释放的线程约束见 Drop
unsafe impl Send for DistributedObserver {}
// SAFETY: 不提供任何 &self 方法访问 ctx
unsafe impl Sync for DistributedObserver {}

unsafe extern "C-unwind" fn on_notification(
    _center: *mut CFNotificationCenter,
    observer: *mut c_void,
    _name: *const CFNotificationName,
    _object: *const c_void,
    _user_info: *const CFDictionary,
) {
    // SAFETY: observer 是 DistributedObserver::new 里泄漏出的 Box<Callback>，在注销之后才释放
    let cb = unsafe { &*observer.cast::<Callback>() };
    let cb = cb.clone();
    // 回调里出现 panic 不能跨越 C 边界
    guarded("distributed notification callback", || cb());
}

impl DistributedObserver {
    /// 登记若干通知名；分布式中心不可用时返回 None
    pub(crate) fn new(main: MainThread, names: &[&str], cb: Callback) -> Option<Self> {
        let center = CFNotificationCenter::distributed_center()?;
        let ctx = Box::into_raw(Box::new(cb)) as usize;
        for name in names {
            let name = CFString::from_str(name);
            // SAFETY: observer 指针在注销前一直有效；回调签名与 CFNotificationCallback 一致
            unsafe {
                center.add_observer(
                    ctx as *const c_void,
                    Some(on_notification),
                    Some(&name),
                    std::ptr::null(),
                    CFNotificationSuspensionBehavior::DeliverImmediately,
                );
            }
        }
        Some(Self { ctx, main })
    }
}

impl Drop for DistributedObserver {
    fn drop(&mut self) {
        let ctx = self.ctx;
        if let Some(center) = CFNotificationCenter::distributed_center() {
            // SAFETY: 注销本观察者登记过的全部通知（线程安全）
            unsafe { center.remove_every_observer(ctx as *const c_void) };
        }
        // 回调只在主线程执行：在主线程上释放上下文，确保没有正在执行的回调还持有它
        self.main.run_async(move |_| {
            // SAFETY: ctx 来自 Box::into_raw，且观察者已注销，之后不会再被使用
            drop(unsafe { Box::from_raw(ctx as *mut Callback) });
        });
    }
}
