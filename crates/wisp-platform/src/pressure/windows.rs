//! 内存压力通知的 Windows 实现：`CreateMemoryResourceNotification` + `RegisterWaitForSingleObject`（系统线程池等待，
//! 不占自己的线程）。
//!
//! 低内存通知对象在「内存低」期间一直处于有信号状态，直接用重复等待会在这段时间里反复回调。因此交替登记两个一次性等待：
//! 等「内存低」→ 报告 [`Level::Warning`]，改等「内存充足」（系统默认阈值约为低内存阈值的三倍，形成回差）→ 报告
//! [`Level::Normal`]，再改回等「内存低」。

use std::ffi::c_void;

use ::windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use ::windows::Win32::System::Memory::{
    CreateMemoryResourceNotification, HighMemoryResourceNotification, LowMemoryResourceNotification,
    QueryMemoryResourceNotification,
};
use ::windows::Win32::System::Threading::{
    INFINITE, RegisterWaitForSingleObject, UnregisterWaitEx, WT_EXECUTEONLYONCE,
};
use ::windows::core::BOOL;
use parking_lot::Mutex;

use super::{Callback, Level};

/// 已安装的监听；丢弃时注销当前的等待（等正在执行的回调结束）并关闭两个通知句柄
pub(super) struct Watch {
    inner: Box<Inner>,
}

struct Inner {
    cb: Callback,
    low: HANDLE,
    high: HANDLE,
    state: Mutex<State>,
}

struct State {
    /// 当前登记的等待；卸载后为 None
    wait: Option<HANDLE>,
    /// 当前等的是「内存低」（否则是「内存充足」）
    waiting_low: bool,
    stopped: bool,
}

// SAFETY: 句柄是内核对象的不透明值，可以在线程间传递；可变部分由 Mutex 保护
unsafe impl Send for Inner {}
// SAFETY: 同上
unsafe impl Sync for Inner {}

/// 登记一次性等待；context 指向 Inner（Watch 存活期间有效）
fn register(inner: &Inner, waiting_low: bool) -> Option<HANDLE> {
    let mut wait = HANDLE::default();
    let object = if waiting_low { inner.low } else { inner.high };
    // SAFETY: object 是有效的通知句柄；context 指向的 Inner 在注销全部等待之前不会释放（见 Drop）
    unsafe {
        RegisterWaitForSingleObject(
            &mut wait,
            object,
            Some(on_signal),
            Some(std::ptr::from_ref(inner).cast::<c_void>()),
            INFINITE,
            WT_EXECUTEONLYONCE,
        )
    }
    .ok()?;
    Some(wait)
}

unsafe extern "system" fn on_signal(ctx: *mut c_void, _timed_out: bool) {
    // 线程池回调：panic 不能越过 FFI 边界
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: ctx 是 register() 传入的 &Inner；Drop 在释放 Inner 之前以阻塞方式注销当前等待，而切换等待只发生在下面的锁内
        let inner = unsafe { &*ctx.cast::<Inner>() };
        let mut st = inner.state.lock();
        if st.stopped {
            return;
        }
        // 一次性等待触发后仍要注销以释放它；在自己的回调里只能用非阻塞方式
        if let Some(w) = st.wait.take() {
            // SAFETY: w 是本回调所属的等待句柄
            let _ = unsafe { UnregisterWaitEx(w, None) };
        }
        let level = if st.waiting_low { Level::Warning } else { Level::Normal };
        (inner.cb)(level);
        st.waiting_low = !st.waiting_low;
        st.wait = register(inner, st.waiting_low);
    }));
}

/// 即时查询：临时建一个低内存通知对象，读它此刻的状态后关闭（不登记等待）
pub(super) fn current() -> Option<Level> {
    // SAFETY: 只创建通知对象并返回句柄，失败时返回错误
    let low = unsafe { CreateMemoryResourceNotification(LowMemoryResourceNotification) }.ok()?;
    let mut state = BOOL::default();
    // SAFETY: low 是刚创建的有效通知句柄；state 是可写的 BOOL
    let queried = unsafe { QueryMemoryResourceNotification(low, &raw mut state) };
    // SAFETY: low 只在这里使用，之后不再访问
    let _ = unsafe { CloseHandle(low) };
    queried.ok()?;
    Some(if state.as_bool() { Level::Warning } else { Level::Normal })
}

pub(super) fn watch(cb: Callback) -> Option<Watch> {
    // SAFETY: 两个调用都只创建通知对象并返回句柄，失败时返回错误
    let low = unsafe { CreateMemoryResourceNotification(LowMemoryResourceNotification) }.ok()?;
    // SAFETY: 同上
    let high = match unsafe { CreateMemoryResourceNotification(HighMemoryResourceNotification) } {
        Ok(h) => h,
        Err(_) => {
            // SAFETY: low 是刚创建、尚未登记的句柄
            let _ = unsafe { CloseHandle(low) };
            return None;
        }
    };
    let inner = Box::new(Inner {
        cb,
        low,
        high,
        state: Mutex::new(State {
            wait: None,
            waiting_low: true,
            stopped: false,
        }),
    });
    {
        let mut st = inner.state.lock();
        st.wait = register(&inner, true);
        if st.wait.is_none() {
            st.stopped = true;
        }
    }
    let watch = Watch { inner };
    if watch.inner.state.lock().stopped {
        return None;
    }
    Some(watch)
}

impl Drop for Watch {
    fn drop(&mut self) {
        let wait = {
            let mut st = self.inner.state.lock();
            st.stopped = true;
            st.wait.take()
        };
        if let Some(w) = wait {
            // SAFETY: w 是登记过的等待；INVALID_HANDLE_VALUE 让函数等到这个等待正在执行的回调结束才返回，此后 Inner 不再被访问
            let _ = unsafe { UnregisterWaitEx(w, Some(INVALID_HANDLE_VALUE)) };
        }
        // SAFETY: 不再有等待引用这两个句柄
        unsafe {
            let _ = CloseHandle(self.inner.low);
            let _ = CloseHandle(self.inner.high);
        }
    }
}
