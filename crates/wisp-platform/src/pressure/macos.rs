//! 内存压力通知的 macOS 实现：`DISPATCH_SOURCE_TYPE_MEMORYPRESSURE` 的 dispatch source（事件来自内核，没有轮询）。

use std::ffi::c_void;

use dispatch2::{
    DispatchObject, DispatchQoS, DispatchQueue, DispatchRetained, DispatchSource, GlobalQueueIdentifier,
    dispatch_source_memorypressure_flags_t,
};

use super::{Callback, Level};

/// 回调来自 libdispatch：panic 一律在这里截住（展开越过 C 边界会直接 abort）
fn guarded(what: &str, f: impl FnOnce()) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err() {
        log::error!("{what} panicked; contained at the FFI boundary");
    }
}

/// 已安装的监听：丢弃时取消 dispatch source（取消回调里释放上下文）
pub(super) struct Watch {
    source: DispatchRetained<DispatchSource>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.source.cancel();
    }
}

/// 事件回调的上下文（dispatch source 的 context；在取消回调里释放）
struct Ctx {
    cb: Callback,
    /// 读事件的数据位（事件回调只拿到 context 指针）。与 source 之间的循环引用由取消回调打破
    source: DispatchRetained<DispatchSource>,
}

/// 把内核给的标志位换成等级（同一次回调里可能合并了多次变化，取最严重的）
fn level_of(data: usize) -> Option<Level> {
    let has = |flag: dispatch_source_memorypressure_flags_t| data & (flag.0 as usize) != 0;
    if has(dispatch_source_memorypressure_flags_t::DISPATCH_MEMORYPRESSURE_CRITICAL) {
        Some(Level::Critical)
    } else if has(dispatch_source_memorypressure_flags_t::DISPATCH_MEMORYPRESSURE_WARN) {
        Some(Level::Warning)
    } else if has(dispatch_source_memorypressure_flags_t::DISPATCH_MEMORYPRESSURE_NORMAL) {
        Some(Level::Normal)
    } else {
        None
    }
}

extern "C" fn on_event(p: *mut c_void) {
    guarded("memory pressure event", || {
        // SAFETY: p 是 watch() 里泄漏的 Box<Ctx>，在取消回调里才释放；同一个 source 的事件回调不会并发，也不会与取消回调并发
        let ctx = unsafe { &*p.cast::<Ctx>() };
        if let Some(level) = level_of(ctx.source.data()) {
            (ctx.cb)(level);
        }
    });
}

extern "C" fn on_cancel(p: *mut c_void) {
    guarded("memory pressure cancel", || {
        // SAFETY: 取消回调在最后一次事件回调之后执行一次，此后不再使用 p
        drop(unsafe { Box::from_raw(p.cast::<Ctx>()) });
    });
}

/// `kern.memorystatus_vm_pressure_level` 的取值（`<sys/kern_memorystatus.h>` 的 `kVMPressure*`）
fn level_of_sysctl(v: u64) -> Option<Level> {
    match v {
        1 => Some(Level::Normal),
        2 => Some(Level::Warning),
        4 => Some(Level::Critical),
        _ => None,
    }
}

pub(super) fn current() -> Option<Level> {
    crate::system::sysctl_u64("kern.memorystatus_vm_pressure_level").and_then(level_of_sysctl)
}

pub(super) fn watch(cb: Callback) -> Option<Watch> {
    let queue = DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(DispatchQoS::Utility));
    let mask = dispatch_source_memorypressure_flags_t::DISPATCH_MEMORYPRESSURE_NORMAL.0
        | dispatch_source_memorypressure_flags_t::DISPATCH_MEMORYPRESSURE_WARN.0
        | dispatch_source_memorypressure_flags_t::DISPATCH_MEMORYPRESSURE_CRITICAL.0;
    // SAFETY: 内存压力类型常量由 libdispatch 导出；handle 对这种类型无意义（传 0），mask 是它接受的三个标志位
    let source = unsafe {
        DispatchSource::new(
            (&raw const dispatch2::_dispatch_source_type_memorypressure).cast_mut(),
            0,
            usize::try_from(mask).ok()?,
            Some(&queue),
        )
    };
    let ctx = Box::new(Ctx {
        cb,
        source: source.clone(),
    });
    // SAFETY: context 在取消回调里释放；事件与取消回调都只通过 context 访问它
    unsafe { source.set_context(Box::into_raw(ctx).cast::<c_void>()) };
    source.set_event_handler_f(on_event);
    source.set_cancel_handler_f(on_cancel);
    source.activate();
    Some(Watch { source })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_map_to_the_most_severe_level() {
        assert_eq!(level_of(0x1), Some(Level::Normal));
        assert_eq!(level_of(0x2), Some(Level::Warning));
        assert_eq!(level_of(0x4 | 0x2), Some(Level::Critical));
        assert_eq!(level_of(0), None);
    }

    #[test]
    fn sysctl_values_map_to_levels() {
        assert_eq!(level_of_sysctl(1), Some(Level::Normal));
        assert_eq!(level_of_sysctl(2), Some(Level::Warning));
        assert_eq!(level_of_sysctl(4), Some(Level::Critical));
        assert_eq!(level_of_sysctl(0), None);
        // 本机一定能读到（任一等级都可以）
        assert!(current().is_some());
    }

    /// 安装与卸载不出错、不回调（本机没有内存压力时）
    #[test]
    fn install_and_drop() {
        let w = watch(Box::new(|_| {})).expect("watch");
        drop(w);
    }
}
