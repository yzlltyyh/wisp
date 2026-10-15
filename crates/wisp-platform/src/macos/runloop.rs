//! 专用线程上的 CFRunLoop（输入钩子用）：任意时刻、从任意线程都能可靠地停止，等待线程退出有上限。
//!
//! `CFRunLoopStop` 只对正在运行的 run loop 生效：在线程进入 `CFRunLoopRun` 之前调用会被丢弃（`CFRunLoopRunSpecific`
//! 进入时还会清掉 stopped 标志），线程之后会一直运行，join 永远等不到。这里改用一个 version-0 的「停止源」：
//! 停止方 `CFRunLoopSourceSignal` + `CFRunLoopWakeUp`。被 signal 的源一直保持待处理状态，直到被执行为止；run loop
//! 无论此刻是否已经在运行，都会在（下一轮）循环开头执行它的 perform 回调。回调运行在 run loop 线程上，此时 run loop
//! 一定正在运行，在这里调用 `CFRunLoopStop` 必然生效。停止源同时让模式非空，没有其他源时 `CFRunLoopRun` 也不会立即返回。
//!
//! 线程的全部资源由 `setup` 在线程上创建，返回的守卫在 run loop 停止后、仍在该线程上 drop（注销 tap 等）。

use std::ffi::c_void;
use std::sync::{Arc, mpsc};
use std::thread::{JoinHandle, ThreadId};
use std::time::Duration;

use objc2_core_foundation::{CFRetained, CFRunLoop, CFRunLoopSource, CFRunLoopSourceContext, kCFRunLoopCommonModes};
use parking_lot::Mutex;

use crate::types::HookError;

/// 只用来调用线程安全的 CF 函数（`CFRunLoopSourceSignal` / `CFRunLoopWakeUp` / `CFRunLoopStop`）的跨线程引用
struct Shared<T>(CFRetained<T>);
// SAFETY: 见上；对象本身由 CF 的引用计数管理，跨线程只做上述线程安全的调用
unsafe impl<T> Send for Shared<T> {}
// SAFETY: 同上
unsafe impl<T> Sync for Shared<T> {}

/// 停止控制（安装方与 run loop 线程共享）
struct StopCtl {
    source: Shared<CFRunLoopSource>,
    /// run loop 线程登记的 run loop（登记之前请求停止时只 signal 停止源，线程开始运行后会立即处理它）
    run_loop: Mutex<Option<Shared<CFRunLoop>>>,
}

impl StopCtl {
    fn request(&self) {
        self.source.0.signal();
        if let Some(rl) = self.run_loop.lock().as_ref() {
            rl.0.wake_up();
        }
    }
}

unsafe extern "C-unwind" fn perform_stop(_info: *mut c_void) {
    // 运行在 run loop 线程上、run loop 正在运行：CFRunLoopStop 必然生效
    super::util::guarded("run loop stop source", || {
        if let Some(rl) = CFRunLoop::current() {
            rl.stop();
        }
    });
}

fn stop_source() -> Option<CFRetained<CFRunLoopSource>> {
    let mut context = CFRunLoopSourceContext {
        version: 0,
        info: std::ptr::null_mut(),
        retain: None,
        release: None,
        copyDescription: None,
        equal: None,
        hash: None,
        schedule: None,
        cancel: None,
        perform: Some(perform_stop),
    };
    // SAFETY: context 有效（被复制进源）；perform 签名正确，不使用 info
    unsafe { CFRunLoopSource::new(None, 0, &mut context) }
}

/// 一个运行中的 run loop 线程
pub(crate) struct LoopThread {
    name: String,
    ctl: Arc<StopCtl>,
    thread: ThreadId,
    /// 线程退出（包括 panic 展开）时发送端被 drop，接收端随即返回 Disconnected
    exited: mpsc::Receiver<()>,
    join: Option<JoinHandle<()>>,
}

impl LoopThread {
    /// 启动线程：在线程上执行 `setup`（创建 tap 等，返回的守卫在 run loop 停止后于该线程上 drop），然后运行 run loop。
    /// `setup` 出错时返回它的错误；`ready_wait` 内没有完成时请求停止并返回错误（线程完成 setup 后立即退出）
    pub(crate) fn spawn<G, S>(name: &str, ready_wait: Duration, setup: S) -> Result<Self, HookError>
    where
        S: FnOnce(&CFRetained<CFRunLoop>) -> Result<G, HookError> + Send + 'static,
    {
        Self::spawn_inner(name, ready_wait, Duration::ZERO, setup)
    }

    /// `pre_run`：setup 之后、进入 run loop 之前的停顿（只有测试用它扩大「已就绪但尚未运行」的窗口）
    fn spawn_inner<G, S>(name: &str, ready_wait: Duration, pre_run: Duration, setup: S) -> Result<Self, HookError>
    where
        S: FnOnce(&CFRetained<CFRunLoop>) -> Result<G, HookError> + Send + 'static,
    {
        let source = stop_source().ok_or_else(|| HookError::Other("failed to create run loop stop source".into()))?;
        let ctl = Arc::new(StopCtl {
            source: Shared(source),
            run_loop: Mutex::new(None),
        });
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), HookError>>(1);
        let (exit_tx, exited) = mpsc::sync_channel::<()>(0);
        let thread_ctl = ctl.clone();
        let join = std::thread::Builder::new()
            .name(name.to_owned())
            .stack_size(512 * 1024)
            .spawn(move || {
                let _exit = exit_tx;
                run_thread(&thread_ctl, ready_tx, pre_run, setup);
            })
            .map_err(|e| HookError::Other(format!("failed to spawn {name} thread: {e}")))?;
        let thread = join.thread().id();
        let me = Self {
            name: name.to_owned(),
            ctl,
            thread,
            exited,
            join: Some(join),
        };
        match ready_rx.recv_timeout(ready_wait) {
            Ok(Ok(())) => Ok(me),
            Ok(Err(e)) => {
                // setup 失败后线程立即退出
                me.stop(ready_wait);
                Err(e)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                me.stop(ready_wait);
                Err(HookError::Other(format!("{name} thread exited during setup")))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // 不再等待：停止请求已登记，线程完成 setup 后一进入 run loop 就会退出并注销
                me.ctl.request();
                log::warn!("{name}: setup did not finish within {ready_wait:?}; thread detached");
                Err(HookError::Other(format!("{name} thread did not start in time")))
            }
        }
    }

    /// 停止 run loop 并等待线程退出，最多等 `wait`（超时记录错误并放弃等待，绝不无限期阻塞调用方）。
    /// 在 run loop 线程自己身上调用时（回调里同步 drop 了 Handle）只发出请求：回调返回后循环随即退出
    pub(crate) fn stop(mut self, wait: Duration) {
        self.ctl.request();
        let Some(join) = self.join.take() else { return };
        if std::thread::current().id() == self.thread {
            return;
        }
        match self.exited.recv_timeout(wait) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                if join.join().is_err() {
                    log::error!("{} thread panicked", self.name);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                log::error!("{} thread did not stop within {wait:?}; detached", self.name);
            }
        }
    }
}

impl Drop for LoopThread {
    fn drop(&mut self) {
        // 没有经过 stop() 就被丢弃时也要让线程退出（不等待）
        if self.join.is_some() {
            self.ctl.request();
        }
    }
}

/// run loop 线程的主体
fn run_thread<G, S>(ctl: &StopCtl, ready_tx: mpsc::SyncSender<Result<(), HookError>>, pre_run: Duration, setup: S)
where
    S: FnOnce(&CFRetained<CFRunLoop>) -> Result<G, HookError>,
{
    let Some(run_loop) = CFRunLoop::current() else {
        let _ = ready_tx.send(Err(HookError::Other("no run loop on hook thread".into())));
        return;
    };
    // SAFETY: 系统导出的模式常量
    let common = unsafe { kCFRunLoopCommonModes };
    run_loop.add_source(Some(&ctl.source.0), common);
    *ctl.run_loop.lock() = Some(Shared(run_loop.clone()));
    let guard = setup(&run_loop);
    let ready = guard.as_ref().map(|_| ()).map_err(Clone::clone);
    // 安装方可能已经放弃等待：那时停止请求已经登记，下面的 run 一进入就会返回
    let _ = ready_tx.send(ready);
    if let Ok(guard) = guard {
        if !pre_run.is_zero() {
            std::thread::sleep(pre_run);
        }
        CFRunLoop::run();
        // run loop 已停止：在本线程上注销 tap 等资源
        drop(guard);
    }
    *ctl.run_loop.lock() = None;
    run_loop.remove_source(Some(&ctl.source.0), common);
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    use super::*;

    /// 线程上的守卫：drop（run loop 停止后）时计数
    struct Guard(Arc<AtomicUsize>);

    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    const WAIT: Duration = Duration::from_secs(2);

    fn wait_for(counter: &AtomicUsize, n: usize) -> bool {
        let until = Instant::now() + WAIT;
        while Instant::now() < until {
            if counter.load(Ordering::SeqCst) >= n {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        counter.load(Ordering::SeqCst) >= n
    }

    #[test]
    fn stop_right_after_spawn_never_hangs() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let rounds = 200;
        for _ in 0..rounds {
            let d = dropped.clone();
            let t = LoopThread::spawn("wisp-test-loop", WAIT, move |_| Ok(Guard(d))).expect("spawn");
            let started = Instant::now();
            t.stop(WAIT);
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "stop took {:?}",
                started.elapsed()
            );
        }
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            rounds,
            "every thread tore down its guard"
        );
    }

    #[test]
    fn stop_before_run_loop_starts_is_not_lost() {
        // 停止请求落在「已就绪、尚未进入 CFRunLoopRun」的窗口里：CFRunLoopStop 在这里会被丢弃，停止源不会
        let dropped = Arc::new(AtomicUsize::new(0));
        let d = dropped.clone();
        let t = LoopThread::spawn_inner("wisp-test-loop", WAIT, Duration::from_millis(80), move |_| Ok(Guard(d)))
            .expect("spawn");
        let started = Instant::now();
        t.stop(WAIT);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "stop took {:?}",
            started.elapsed()
        );
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn idle_loop_keeps_running_until_stopped() {
        // 只有停止源时模式也不为空：run loop 不会自行返回，空闲时也不会醒来
        let dropped = Arc::new(AtomicUsize::new(0));
        let d = dropped.clone();
        let t = LoopThread::spawn("wisp-test-loop", WAIT, move |_| Ok(Guard(d))).expect("spawn");
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(dropped.load(Ordering::SeqCst), 0, "loop exited on its own");
        t.stop(WAIT);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn setup_error_is_reported_and_thread_exits() {
        let err = LoopThread::spawn::<(), _>("wisp-test-loop", WAIT, |_| Err(HookError::Other("boom".into())));
        assert_eq!(err.err(), Some(HookError::Other("boom".into())));
    }

    #[test]
    fn abandoned_slow_setup_still_exits() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let d = dropped.clone();
        let started = Instant::now();
        // setup 要 600 ms，等待上限 20 ms：spawn 必须在 setup 结束之前放弃。门限 400 ms 给建线程与调度留足余量（macOS runner
        // 是 3 核虚拟机、与整套单测并发，原来的 150 / 140 ms 在那里会超时）
        let res = LoopThread::spawn("wisp-test-loop", Duration::from_millis(20), move |_| {
            std::thread::sleep(Duration::from_millis(600));
            Ok(Guard(d))
        });
        assert!(res.is_err());
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "spawn did not give up in time: {:?}",
            started.elapsed()
        );
        assert!(wait_for(&dropped, 1), "detached thread never exited");
    }

    #[test]
    fn dropping_without_stop_still_ends_the_thread() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let d = dropped.clone();
        let t = LoopThread::spawn("wisp-test-loop", WAIT, move |_| Ok(Guard(d))).expect("spawn");
        drop(t);
        assert!(wait_for(&dropped, 1));
    }
}
