//! 剪贴板线程：一个 message-only 窗口，既是 Wisp 写入剪贴板时的所有者窗口，也是变更监听器。
//!
//! - 监听用 `AddClipboardFormatListener`，事件驱动；`WM_CLIPBOARDUPDATE` 到达后用 50 ms 一次性定时器去抖（每次变更
//!   重置，但从第一次未投递的变更算起最多等 250 ms），定时器触发时读序号，按「同内容别名」还原后不新于「已见序号」
//!   则丢弃（去重；Wisp 自己读取时原所有者的延迟渲染也会推进序号，见 `suppress`），落在抑制区间内则丢弃；
//! - 暂停（最后一个 Handle 被 drop）时 `RemoveClipboardFormatListener`，恢复时把「已见序号」设为当前值，
//!   暂停期间的复制不补录；
//! - 线程在首次需要时创建、与进程同寿命，空闲时阻塞在 `GetMessageW` 中，零唤醒。所有者窗口必须有人泵消息：
//!   其他进程 `EmptyClipboard` 时会同步发送 `WM_DESTROYCLIPBOARD`。

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use ::windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use ::windows::Win32::System::DataExchange::{AddClipboardFormatListener, RemoveClipboardFormatListener};
use ::windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, HWND_MESSAGE, KillTimer, PostMessageW, SetTimer, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WM_CLIPBOARDUPDATE, WM_TIMER,
};
use parking_lot::Mutex;

use crate::error::PlatformError;
use crate::types::HookHandle;

use super::clipboard;
use super::suppress::{SeqSuppressor, debounce_delay};
use super::util::{RawHwnd, call_guarded};
use super::wnd;

const CLASS: &str = "WispClipboardSink";
const WM_APP_LISTEN: u32 = WM_APP + 1;
const TIMER_DEBOUNCE: usize = 1;
/// 去抖窗口
const DEBOUNCE: Duration = Duration::from_millis(50);
/// 去抖的最长等待：从第一次未投递的变更算起
const DEBOUNCE_MAX_WAIT: Duration = Duration::from_millis(250);
/// 延迟渲染别名的存活时间：覆盖去抖（最长 250 ms）与线程调度的余量
const RENDER_ALIAS_TTL: Duration = Duration::from_secs(2);

type Watcher = Arc<dyn Fn(u64) + Send + Sync>;

/// 窗口线程与调用方共享的状态
struct Shared {
    watchers: Mutex<Vec<(u64, Watcher)>>,
    suppressor: Mutex<SeqSuppressor>,
    last_seen: AtomicU64,
}

/// 剪贴板线程的门面
pub(crate) struct ClipHub {
    shared: Arc<Shared>,
    window: Mutex<Option<RawHwnd>>,
    next_id: AtomicU64,
}

thread_local! {
    static SHARED: RefCell<Option<Arc<Shared>>> = const { RefCell::new(None) };
    /// 第一次尚未投递的变更到达的时刻（剪贴板线程）
    static PENDING_SINCE: Cell<Option<Instant>> = const { Cell::new(None) };
}

impl ClipHub {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            shared: Arc::new(Shared {
                watchers: Mutex::new(Vec::new()),
                suppressor: Mutex::new(SeqSuppressor::default()),
                last_seen: AtomicU64::new(0),
            }),
            window: Mutex::new(None),
            next_id: AtomicU64::new(1),
        })
    }

    /// 所有者窗口（按需创建剪贴板线程）；创建失败时返回 None，写入退化为无所有者
    pub(crate) fn owner(&self) -> Option<RawHwnd> {
        let mut guard = self.window.lock();
        if let Some(h) = *guard {
            return Some(h);
        }
        match spawn_thread(Arc::clone(&self.shared)) {
            Ok(h) => {
                *guard = Some(h);
                Some(h)
            }
            Err(e) => {
                log::error!("clipboard thread: {e}");
                None
            }
        }
    }

    /// 开始监听：回调在剪贴板线程上执行，只携带序号。
    ///
    /// 「列表由空变非空 → 开始监听」「由非空变空 → 停止监听」的投递与列表修改在同一把锁内完成（`PostMessageW` 不阻塞），
    /// 开始监听投递失败时什么都不登记，不会留下没有 Handle、永远注销不掉的条目
    pub(crate) fn watch(self: &Arc<Self>, cb: Box<dyn Fn(u64) + Send + Sync>) -> Result<HookHandle, PlatformError> {
        let hwnd = self
            .owner()
            .ok_or_else(|| PlatformError::Other("clipboard listener window is unavailable".into()))?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut w = self.shared.watchers.lock();
            if w.is_empty() {
                post_listen(hwnd, true)?;
            }
            w.push((id, Arc::from(cb)));
        }
        let me = Arc::clone(self);
        Ok(HookHandle::new(move || {
            let mut w = me.shared.watchers.lock();
            w.retain(|(i, _)| *i != id);
            if w.is_empty()
                && let Err(e) = post_listen(hwnd, false)
            {
                log::warn!("failed to stop clipboard listener: {e}");
            }
        }))
    }

    /// 登记开放抑制区间 (lo, ∞)
    pub(crate) fn suppress_open(&self, lo_exclusive: u64, ttl: Duration) -> u64 {
        self.shared.suppressor.lock().open(lo_exclusive, ttl, Instant::now())
    }

    /// 收窄抑制区间为 (lo, hi]
    pub(crate) fn suppress_close(&self, token: u64, hi_inclusive: u64, ttl: Duration) {
        self.shared
            .suppressor
            .lock()
            .close(token, hi_inclusive, ttl, Instant::now());
        // 恢复后把「已见序号」直接推进到当前值：即使去抖定时器晚到也不会再报告这两次变更
        self.shared.last_seen.fetch_max(hi_inclusive, Ordering::AcqRel);
    }

    /// 抑制区间保持开放并重新计时（恢复超时，见 `SeqSuppressor::extend`）
    pub(crate) fn suppress_extend(&self, token: u64, ttl: Duration) {
        self.shared.suppressor.lock().extend(token, ttl, Instant::now());
    }

    /// 撤销抑制区间
    pub(crate) fn suppress_cancel(&self, token: u64) {
        self.shared.suppressor.lock().cancel(token);
    }

    /// Wisp 刚打开剪贴板读取（打开时序号 `opened_at`）：在关闭之前，序号的任何变化都只可能是原所有者响应
    /// `WM_RENDERFORMAT`、为延迟渲染的格式调用了 `SetClipboardData`（Wisp 开着剪贴板时其他程序无法清空或写入），
    /// 内容没有变。登记开放的同内容别名，返回交给 [`Self::read_ended`] 的 token
    pub(crate) fn read_began(&self, opened_at: u64) -> u64 {
        self.shared
            .suppressor
            .lock()
            .alias_open(opened_at, RENDER_ALIAS_TTL, Instant::now())
    }

    /// 读取结束（必须在关闭剪贴板之前调用）：别名收窄为 (opened_at, closed_at]。这次读取引起的
    /// `WM_CLIPBOARDUPDATE` 不会再触发一次回调
    pub(crate) fn read_ended(&self, token: u64, opened_at: u64, closed_at: u64) {
        self.shared
            .suppressor
            .lock()
            .alias_close(token, opened_at, closed_at, RENDER_ALIAS_TTL, Instant::now());
        if closed_at != opened_at {
            log::debug!(
                "clipboard changes ({opened_at}, {closed_at}] were delayed rendering caused by Wisp's own read"
            );
        }
    }
}

fn post_listen(hwnd: RawHwnd, on: bool) -> Result<(), PlatformError> {
    // SAFETY: 向本进程的剪贴板窗口投递消息
    unsafe { PostMessageW(Some(hwnd.hwnd()), WM_APP_LISTEN, WPARAM(usize::from(on)), LPARAM(0)) }
        .map_err(|e| PlatformError::Other(format!("post to clipboard window: {e}")))
}

fn spawn_thread(shared: Arc<Shared>) -> Result<RawHwnd, String> {
    let (tx, rx) = mpsc::sync_channel::<Result<RawHwnd, String>>(1);
    std::thread::Builder::new()
        .name("wisp-clipboard".into())
        .spawn(move || {
            if !wnd::ensure_class(CLASS, Some(wndproc), None) {
                let _ = tx.send(Err("register class failed".into()));
                return;
            }
            let hwnd = match wnd::create_window(CLASS, WINDOW_EX_STYLE(0), WINDOW_STYLE(0), Some(HWND_MESSAGE)) {
                Ok(h) => h,
                Err(e) => {
                    let _ = tx.send(Err(e.to_string()));
                    return;
                }
            };
            SHARED.with(|s| *s.borrow_mut() = Some(shared));
            let _ = tx.send(Ok(RawHwnd::new(hwnd)));
            wnd::run_message_loop(|_| false);
        })
        .map_err(|e| e.to_string())?;
    rx.recv_timeout(Duration::from_secs(2))
        .map_err(|_| "clipboard thread did not start in time".to_owned())?
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_APP_LISTEN => {
            if wparam.0 != 0 {
                SHARED.with(|s| {
                    if let Some(s) = s.borrow().as_ref() {
                        // 恢复监听：暂停期间的变更不补录
                        s.last_seen.store(clipboard::seq(), Ordering::Release);
                    }
                });
                // SAFETY: 本线程创建的窗口
                if let Err(e) = unsafe { AddClipboardFormatListener(hwnd) } {
                    log::error!("AddClipboardFormatListener failed: {e}");
                }
            } else {
                // SAFETY: 同上；未注册时调用失败，忽略
                unsafe {
                    let _ = RemoveClipboardFormatListener(hwnd);
                    let _ = KillTimer(Some(hwnd), TIMER_DEBOUNCE);
                }
                PENDING_SINCE.with(|p| p.set(None));
            }
            LRESULT(0)
        }
        WM_CLIPBOARDUPDATE => {
            let now = Instant::now();
            let first = PENDING_SINCE.with(|p| {
                let first = p.get().unwrap_or(now);
                p.set(Some(first));
                first
            });
            let delay = debounce_delay(first, now, DEBOUNCE, DEBOUNCE_MAX_WAIT);
            if delay.is_zero() {
                flush(hwnd);
            } else {
                let ms = u32::try_from(delay.as_millis()).unwrap_or(u32::MAX).max(1);
                // SAFETY: 重设一次性去抖定时器
                unsafe {
                    SetTimer(Some(hwnd), TIMER_DEBOUNCE, ms, None);
                }
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_DEBOUNCE => {
            flush(hwnd);
            LRESULT(0)
        }
        // SAFETY: 其余消息（含 WM_DESTROYCLIPBOARD / WM_RENDERFORMAT，Wisp 从不延迟渲染）交给默认处理
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// 结束本轮去抖并投递
fn flush(hwnd: HWND) {
    // SAFETY: 一次性定时器；未设置时调用失败，忽略
    unsafe {
        let _ = KillTimer(Some(hwnd), TIMER_DEBOUNCE);
    }
    PENDING_SINCE.with(|p| p.set(None));
    deliver();
}

fn deliver() {
    let Some(shared) = SHARED.with(|s| s.borrow().clone()) else {
        return;
    };
    let seq = clipboard::seq();
    let now = Instant::now();
    let (content, ignored) = {
        let mut s = shared.suppressor.lock();
        (s.canonical(seq, now), s.should_ignore(seq, now))
    };
    let prev = shared.last_seen.fetch_max(seq, Ordering::AcqRel);
    // 去重：内容真正变化时的序号（按延迟渲染别名还原）已经投递过，例如上层收到回调后读取剪贴板、原所有者因此渲染了
    // 延迟格式。打开时那次变更还没投递的话照常投递，不会丢
    if content <= prev {
        return;
    }
    if ignored {
        log::debug!("clipboard change {seq} suppressed (caused by Wisp)");
        return;
    }
    let watchers: Vec<Watcher> = shared.watchers.lock().iter().map(|(_, w)| Arc::clone(w)).collect();
    for w in watchers {
        // 运行在窗口过程里：panic 不能越过 extern "system" 边界
        call_guarded("clipboard watch callback", || w(seq));
    }
}
