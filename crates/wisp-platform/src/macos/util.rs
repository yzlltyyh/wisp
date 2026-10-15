//! macOS 实现的公共工具：主线程派发、带硬截止时间的执行、只能在主线程使用的对象的搬运、回调的 panic 隔离。

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use objc2::MainThreadMarker;
use objc2::rc::autoreleasepool;

use crate::MainThreadDispatch;

/// 主线程派发器（包装 `PlatformConfig.main_thread`）
#[derive(Clone)]
pub(crate) struct MainThread {
    dispatch: MainThreadDispatch,
}

impl MainThread {
    pub(crate) fn new(dispatch: MainThreadDispatch) -> Self {
        Self { dispatch }
    }

    /// 在主线程上执行 `f` 并等待结果，最多等 `timeout`。已在主线程时直接执行。
    ///
    /// 返回 None 的情况：超时（主线程忙或事件循环尚未运行）、派发器没有把闭包放到主线程上执行（例如测试里的
    /// 同步派发器）。调用方必须为 None 准备退路，绝不能因此阻塞或崩溃
    ///
    /// 超时之后闭包仍可能在主线程上执行：只用于没有副作用的读取。有副作用的操作用 [`MainThread::run_sync_or_cancel`]
    pub(crate) fn run_sync<R, F>(&self, timeout: Duration, f: F) -> Option<R>
    where
        R: Send + 'static,
        F: FnOnce(MainThreadMarker) -> R + Send + 'static,
    {
        if let Some(mtm) = MainThreadMarker::new() {
            return on_main(mtm, f);
        }
        let (tx, rx) = mpsc::sync_channel(1);
        (self.dispatch)(Box::new(move || {
            let out = MainThreadMarker::new().and_then(|mtm| on_main(mtm, f));
            let _ = tx.send(out);
        }));
        rx.recv_timeout(timeout).ok().flatten()
    }

    /// 与 [`MainThread::run_sync`] 相同，但**超时即作废**：返回 None 时保证 `f` 之后不会再执行（适用于有副作用的
    /// 操作，例如激活应用——调用方已经按「超时」处理了，副作用不能在之后悄悄发生）。
    ///
    /// 超时那一刻 `f` 恰好已经开始执行时无法作废，就再等它最多一个 `timeout` 拿到结果（主线程上的短操作）
    pub(crate) fn run_sync_or_cancel<R, F>(&self, timeout: Duration, f: F) -> Option<R>
    where
        R: Send + 'static,
        F: FnOnce(MainThreadMarker) -> R + Send + 'static,
    {
        if let Some(mtm) = MainThreadMarker::new() {
            return on_main(mtm, f);
        }
        cancellable(&self.dispatch, timeout, move || {
            MainThreadMarker::new().and_then(|mtm| on_main(mtm, f))
        })
    }

    /// 在主线程上异步执行 `f`；已在主线程时直接执行。派发器没有把闭包放到主线程上时丢弃（记日志）
    pub(crate) fn run_async<F>(&self, f: F)
    where
        F: FnOnce(MainThreadMarker) + Send + 'static,
    {
        if let Some(mtm) = MainThreadMarker::new() {
            on_main(mtm, f);
            return;
        }
        (self.dispatch)(Box::new(move || match MainThreadMarker::new() {
            Some(mtm) => {
                on_main(mtm, f);
            }
            None => log::debug!("main-thread dispatch ran off the main thread; work dropped"),
        }));
    }
}

/// [`MainThread::run_sync_or_cancel`] 的核心：派发 `task`，`timeout` 内没拿到结果就作废。
/// 闭包执行时先把状态从 PENDING 改成 STARTED，调用方超时时把 PENDING 改成 CANCELLED，二者只有一个能成功
fn cancellable<R, T>(dispatch: &MainThreadDispatch, timeout: Duration, task: T) -> Option<R>
where
    R: Send + 'static,
    T: FnOnce() -> Option<R> + Send + 'static,
{
    const PENDING: u8 = 0;
    const STARTED: u8 = 1;
    const CANCELLED: u8 = 2;
    let state = Arc::new(AtomicU8::new(PENDING));
    let (tx, rx) = mpsc::sync_channel(1);
    let task_state = state.clone();
    dispatch(Box::new(move || {
        // 调用方已经作废：不执行
        if task_state
            .compare_exchange(PENDING, STARTED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let _ = tx.send(task());
    }));
    match rx.recv_timeout(timeout) {
        Ok(out) => out,
        Err(_) => {
            if state
                .compare_exchange(PENDING, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return None;
            }
            // 已经开始执行（或派发器丢弃了闭包、发送端已断开）：等它完成
            rx.recv_timeout(timeout).ok().flatten()
        }
    }
}

/// 主线程上执行一个派发来的闭包：包一层自动释放池；panic 不外传（主线程的派发器由 AppKit 的事件循环驱动，
/// 展开穿过 AppKit 帧通常导致进程 abort）
fn on_main<R>(mtm: MainThreadMarker, f: impl FnOnce(MainThreadMarker) -> R) -> Option<R> {
    guarded("main-thread task", || autoreleasepool(|_| f(mtm)))
}

/// 执行一个会被系统框架回调的闭包（ObjC 块、通知、定时器、C 回调）：捕获 panic 并记日志，绝不让展开越过
/// C / ObjC 边界。返回 None 表示闭包 panic 了
pub(crate) fn guarded<R>(what: &str, f: impl FnOnce() -> R) -> Option<R> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => Some(r),
        Err(_) => {
            log::error!("{what} panicked; contained at the FFI boundary");
            None
        }
    }
}

/// 只在主线程上创建、访问与释放的对象的搬运箱：可以跨线程移动，但只有持有 [`MainThreadMarker`] 时才能取用。
/// 释放必须发生在主线程（由使用方保证：总是在主线程闭包里 drop）
pub(crate) struct MainOnly<T>(T);

// SAFETY: 内部对象只能经 `get(mtm)` / `into_inner(mtm)` 在主线程取用，跨线程移动的只是所有权；
// 使用方保证在主线程上释放
unsafe impl<T> Send for MainOnly<T> {}

impl<T> MainOnly<T> {
    pub(crate) fn new(value: T, _mtm: MainThreadMarker) -> Self {
        Self(value)
    }

    #[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
    pub(crate) fn get(&self, _mtm: MainThreadMarker) -> &T {
        &self.0
    }

    pub(crate) fn into_inner(self, _mtm: MainThreadMarker) -> T {
        self.0
    }
}

/// 在新线程上执行 `f`，最多等 `deadline`；超时返回 None，线程在后台自行结束（只用于不会产生副作用的读取，
/// 例如可能被延迟提供数据的应用拖住的剪贴板快照）
pub(crate) fn with_deadline<R, F>(name: &str, deadline: Duration, f: F) -> Option<R>
where
    R: Send + 'static,
    F: FnOnce() -> R + Send + 'static,
{
    let (tx, rx) = mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name(name.to_owned())
        .stack_size(256 * 1024)
        .spawn(move || {
            // 闭包 panic 时线程结束、发送端被 drop，调用方立即拿到 None（panic 只结束这个辅助线程，不越过 FFI 边界）
            let out = autoreleasepool(|_| f());
            let _ = tx.send(out);
        });
    if let Err(e) = spawned {
        log::warn!("{name}: failed to spawn helper thread: {e}");
        return None;
    }
    rx.recv_timeout(deadline).ok()
}

/// 在当前线程的自动释放池里执行（工作线程上调用 Cocoa API 时必须包一层，否则自动释放的对象会一直堆积到线程退出）
pub(crate) fn pool<R>(f: impl FnOnce() -> R) -> R {
    autoreleasepool(|_| f())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;

    use parking_lot::Mutex;

    use super::*;

    type Parked = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

    /// 把闭包先存起来、由测试决定何时执行的派发器
    fn parking_dispatcher() -> (MainThreadDispatch, Parked) {
        let parked: Parked = Arc::new(Mutex::new(None));
        let slot = parked.clone();
        let dispatch: MainThreadDispatch = Arc::new(move |f| *slot.lock() = Some(f));
        (dispatch, parked)
    }

    #[test]
    fn cancelled_task_never_runs_after_timeout() {
        let (dispatch, parked) = parking_dispatcher();
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        let out = cancellable(&dispatch, Duration::from_millis(20), move || {
            flag.store(true, Ordering::SeqCst);
            Some(())
        });
        assert_eq!(out, None, "timed out");
        // 主线程这时才空下来，执行派发来的闭包：已经作废，副作用不能发生
        let f = parked.lock().take().expect("closure was dispatched");
        f();
        assert!(
            !ran.load(Ordering::SeqCst),
            "side effect happened after the caller gave up"
        );
    }

    #[test]
    fn task_that_runs_in_time_returns_its_result() {
        let dispatch: MainThreadDispatch = Arc::new(|f| {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(5));
                f();
            });
        });
        assert_eq!(cancellable(&dispatch, Duration::from_millis(500), || Some(7)), Some(7));
    }

    #[test]
    fn task_already_started_at_timeout_is_awaited() {
        // 闭包在超时之前开始执行、在超时之后才结束：调用方不能报超时之后让副作用照常发生，而是等它完成。
        // 原先派发线程睡 5 ms 再执行、超时 30 ms、闭包 45 ms：3 核虚拟机上并行跑测试时派发线程被拖过 30 ms，闭包还没开始
        // 就被作废。现在派发器等闭包真的开始执行之后才返回（cancellable 在派发之后才开始计时，所以
        // 闭包一定在超时之前开始）；闭包时长取超时的 1.5 倍，落在「第一次超时之后、第二次等待结束之前」，两侧各留半个超时
        // 的余量，不再依赖线程多快被调度
        const TIMEOUT: Duration = Duration::from_millis(400);
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let started_rx = Mutex::new(started_rx);
        let dispatch: MainThreadDispatch = Arc::new(move |f| {
            std::thread::spawn(f);
            started_rx
                .lock()
                .recv_timeout(Duration::from_secs(10))
                .expect("the dispatched closure starts");
        });
        let began = Instant::now();
        let out = cancellable(&dispatch, TIMEOUT, move || {
            let _ = started_tx.send(());
            std::thread::sleep(TIMEOUT * 3 / 2);
            Some("done")
        });
        assert_eq!(out, Some("done"));
        assert!(began.elapsed() >= TIMEOUT, "the result arrived after the first timeout");
        assert!(began.elapsed() < Duration::from_secs(5), "the caller does not hang");
    }

    #[test]
    fn dropped_closure_reports_none_quickly() {
        let dispatch: MainThreadDispatch = Arc::new(drop);
        let started = Instant::now();
        assert_eq!(cancellable(&dispatch, Duration::from_millis(200), || Some(1)), None);
        assert!(
            started.elapsed() < Duration::from_millis(150),
            "disconnect is detected immediately"
        );
    }

    #[test]
    fn guarded_contains_panics() {
        assert_eq!(guarded("test", || 5), Some(5));
        assert_eq!(guarded("test", || -> i32 { panic!("boom") }), None);
    }
}
