//! 前台应用追踪与激活。
//!
//! 追踪线程用 `SetWinEventHook(EVENT_SYSTEM_FOREGROUND, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS)`
//! 持续记录最近一个非 Wisp、非 shell 的前台窗口：从托盘触发时前台是任务栏，`frontmost()` 就退回这条记录。
//! 线程空闲时阻塞在 `GetMessageW` 中，只在前台切换时被唤醒。
//!
//! 激活其他应用时只用不会被目标卡住的调用：`ShowWindowAsync`、`SetWindowPos(SWP_ASYNCWINDOWPOS)`、
//! `SetForegroundWindow`；目标（或 AttachThreadInput 要挂靠的前台线程）无响应时直接放弃。

use std::cell::RefCell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ::windows::Win32::Foundation::{HWND, LPARAM};
use ::windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use ::windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use ::windows::Win32::UI::WindowsAndMessaging::{
    EVENT_SYSTEM_FOREGROUND, EnumWindows, GW_OWNER, GWL_EXSTYLE, GetForegroundWindow, GetWindow, GetWindowLongW,
    HWND_TOP, IsHungAppWindow, IsIconic, IsWindow, IsWindowVisible, SW_RESTORE, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SetForegroundWindow, SetWindowPos, ShowWindowAsync, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WS_EX_TOOLWINDOW,
};
use ::windows::core::BOOL;
use parking_lot::Mutex;
use wisp_types::AppRef;

use crate::error::PlatformError;

use super::input;
use super::process::{self, NameCache};
use super::util::{RawHwnd, call_guarded};
use super::wnd;

/// 前台切换的监听者（浮标的 AppSwitched 消失条件、低级钩子的存活检查）
pub(crate) type SwitchListener = Arc<dyn Fn() + Send + Sync>;

#[derive(Debug, Clone, Copy)]
struct Tracked {
    hwnd: RawHwnd,
}

/// 前台追踪器
pub(crate) struct Focus {
    own_pid: u32,
    names: Arc<NameCache>,
    /// 最近一个有效（非 Wisp、非 shell）的前台顶层窗口
    last_valid: Mutex<Option<Tracked>>,
    /// 最近一次观察到的前台顶层窗口（含 shell），用于判断「是否真的切换了」
    last_seen: Mutex<RawHwnd>,
    listener: Mutex<Option<SwitchListener>>,
}

thread_local! {
    static TRACKER: RefCell<Option<Arc<Focus>>> = const { RefCell::new(None) };
}

impl Focus {
    pub(crate) fn new(own_pid: u32, names: Arc<NameCache>) -> Arc<Self> {
        Arc::new(Self {
            own_pid,
            names,
            last_valid: Mutex::new(None),
            last_seen: Mutex::new(RawHwnd::default()),
            listener: Mutex::new(None),
        })
    }

    /// 启动追踪线程（只在构造 Platform 时调用一次；线程与进程同寿命）
    pub(crate) fn start(self: &Arc<Self>) {
        let me = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("wisp-foreground".into())
            .spawn(move || tracker_main(me));
        if let Err(e) = spawned {
            log::error!("failed to start foreground tracker: {e}");
        }
    }

    /// 设置 / 清除前台切换监听者
    pub(crate) fn set_listener(&self, l: Option<SwitchListener>) {
        *self.listener.lock() = l;
    }

    fn observe(&self, hwnd: HWND) {
        if hwnd.0.is_null() {
            return;
        }
        let root = process::root_owner(hwnd);
        let (_, pid) = process::window_thread_process(root);
        if pid == 0 || pid == self.own_pid {
            return;
        }
        let switched = {
            let mut seen = self.last_seen.lock();
            let changed = *seen != RawHwnd::new(root);
            *seen = RawHwnd::new(root);
            changed
        };
        if !process::is_shell_window(root, pid) {
            *self.last_valid.lock() = Some(Tracked {
                hwnd: RawHwnd::new(root),
            });
        }
        if switched {
            let listener = self.listener.lock().clone();
            if let Some(l) = listener {
                // 运行在 WinEvent 回调里：panic 不能越过 extern "system" 边界
                call_guarded("foreground listener", || l());
            }
        }
    }

    /// 当前前台应用：已排除 Wisp 自己和 shell；前台是它们时沿用最近一次有效的前台应用
    pub(crate) fn frontmost(&self) -> Option<AppRef> {
        if let Some(root) = self.foreground_valid_root() {
            *self.last_valid.lock() = Some(Tracked {
                hwnd: RawHwnd::new(root),
            });
            return self.names.app_for_window(root);
        }
        let last = (*self.last_valid.lock())?;
        if !is_window(last.hwnd.hwnd()) {
            return None;
        }
        self.names.app_for_window(last.hwnd.hwnd())
    }

    /// 当前前台顶层窗口（非 Wisp、非 shell 时）
    pub(crate) fn foreground_valid_root(&self) -> Option<HWND> {
        // SAFETY: 纯查询
        let fg = unsafe { GetForegroundWindow() };
        if fg.0.is_null() {
            return None;
        }
        let root = process::root_owner(fg);
        let (_, pid) = process::window_thread_process(root);
        (pid != 0 && pid != self.own_pid && !process::is_shell_window(root, pid)).then_some(root)
    }

    /// 激活应用：恢复最小化窗口 → 设为前台（必要时 AttachThreadInput / 掩码键）→ 等待 ≤100 ms。
    /// 目标无响应时立即返回 `Timeout`
    pub(crate) fn activate(&self, app: &AppRef) -> Result<(), PlatformError> {
        let hwnd = target_window(app).ok_or_else(|| PlatformError::Other("target window is gone".into()))?;
        if is_hung(hwnd) {
            return Err(PlatformError::Timeout);
        }
        // SAFETY: 纯查询；`ShowWindowAsync` 只向目标线程投递请求，目标卡住也不会阻塞本线程
        unsafe {
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindowAsync(hwnd, SW_RESTORE);
            }
        }
        request_foreground(hwnd);
        if wait_foreground(hwnd, Duration::from_millis(100)) {
            Ok(())
        } else {
            Err(PlatformError::Timeout)
        }
    }

    /// 目标应用是否已提权（UIPI 会拦截注入）
    pub(crate) fn is_elevated(&self, app: &AppRef) -> bool {
        let pid = app
            .pid
            .or_else(|| target_window(app).map(|h| process::window_thread_process(h).1))
            .unwrap_or(0);
        pid != 0 && process::is_elevated(pid)
    }
}

fn tracker_main(focus: Arc<Focus>) {
    TRACKER.with(|t| *t.borrow_mut() = Some(Arc::clone(&focus)));
    // SAFETY: 进程外（out-of-context）回调在本线程的消息循环中执行；hmod 为 None
    let hook = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(on_win_event),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    };
    if hook.0.is_null() {
        log::error!(
            "SetWinEventHook(EVENT_SYSTEM_FOREGROUND) failed: {}",
            ::windows::core::Error::from_thread()
        );
        return;
    }
    // 用当前前台做初值
    // SAFETY: 纯查询
    focus.observe(unsafe { GetForegroundWindow() });
    wnd::run_message_loop(|_| false);
    // SAFETY: 注销本线程安装的钩子
    unsafe {
        let _ = UnhookWinEvent(hook);
    }
}

unsafe extern "system" fn on_win_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    // OBJID_WINDOW = 0：只关心窗口本身
    if event != EVENT_SYSTEM_FOREGROUND || id_object != 0 {
        return;
    }
    TRACKER.with(|t| {
        if let Some(f) = t.borrow().as_ref() {
            f.observe(hwnd);
        }
    });
}

/// 窗口句柄是否仍有效
pub(crate) fn is_window(hwnd: HWND) -> bool {
    // SAFETY: 纯查询
    !hwnd.0.is_null() && unsafe { IsWindow(Some(hwnd)) }.as_bool()
}

/// 窗口所属线程是否已无响应（5 s 内没有处理消息；`IsHungAppWindow` 本身不向目标发送消息）
pub(crate) fn is_hung(hwnd: HWND) -> bool {
    // SAFETY: 纯查询
    !hwnd.0.is_null() && unsafe { IsHungAppWindow(hwnd) }.as_bool()
}

/// 某窗口（按顶层所有者比较）是否已在前台
pub(crate) fn is_foreground(hwnd: HWND) -> bool {
    // SAFETY: 纯查询
    let fg = unsafe { GetForegroundWindow() };
    !fg.0.is_null() && process::root_owner(fg) == process::root_owner(hwnd)
}

/// 请求把窗口设为前台：先直接 `SetForegroundWindow`；被前台锁拒绝时临时 `AttachThreadInput` 到当前前台线程再试
/// （前台线程无响应时跳过这一步：挂靠到卡死线程的输入队列上，本线程的输入状态也会被拖住）；仍失败时注入一次掩码键
/// （让本进程成为最后一次输入的来源）后再试。激活是异步的，调用方用 [`wait_foreground`] 确认
pub(crate) fn request_foreground(hwnd: HWND) {
    if is_foreground(hwnd) {
        return;
    }
    // SAFETY: 标准窗口操作
    if unsafe { SetForegroundWindow(hwnd) }.as_bool() {
        return;
    }
    // SAFETY: 纯查询
    let fg = unsafe { GetForegroundWindow() };
    let (fg_tid, _) = process::window_thread_process(fg);
    // SAFETY: 纯查询
    let me = unsafe { GetCurrentThreadId() };
    if fg_tid != 0 && fg_tid != me && !is_hung(fg) {
        // AttachThreadInput 要求两个线程都有消息队列；调用方通常是没有窗口的工作线程
        wnd::ensure_message_queue();
        // SAFETY: 临时把本线程的输入状态挂到前台线程上，操作完立即解除。Z 序调整用 SWP_ASYNCWINDOWPOS 投递给目标
        // 线程（`BringWindowToTop` 对其他线程的窗口是同步发送消息，目标卡住时会把本线程一起卡住）
        unsafe {
            if AttachThreadInput(me, fg_tid, true).as_bool() {
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOP),
                    0,
                    0,
                    0,
                    0,
                    SWP_ASYNCWINDOWPOS | SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
                let ok = SetForegroundWindow(hwnd).as_bool();
                let _ = AttachThreadInput(me, fg_tid, false);
                if ok {
                    return;
                }
            }
        }
    }
    if input::send_mask_key() {
        // SAFETY: 标准窗口操作
        unsafe {
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

/// 每 5 ms 检查一次，直到窗口到达前台或超时
pub(crate) fn wait_foreground(hwnd: HWND, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if is_foreground(hwnd) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// 应用的目标窗口：`AppRef.window` 仍有效（且属于同一进程）时用它，否则按 pid 找最上层的可见主窗口
pub(crate) fn target_window(app: &AppRef) -> Option<HWND> {
    exact_window(app).or_else(|| app.pid.and_then(main_window_of))
}

/// `AppRef.window` 记录的窗口（仍有效、且属于同一进程时；句柄被系统复用给别的进程时视为失效）
pub(crate) fn exact_window(app: &AppRef) -> Option<HWND> {
    let hwnd = RawHwnd::from_u64(app.window?).hwnd();
    if !is_window(hwnd) {
        return None;
    }
    let (_, pid) = process::window_thread_process(hwnd);
    // UWP：窗口属于宿主进程 ApplicationFrameHost.exe，pid 是应用进程，二者不同也接受
    let same = app.pid.is_none_or(|p| p == pid)
        || process::exe_path(pid)
            .is_some_and(|p| process::file_name(&p).eq_ignore_ascii_case("ApplicationFrameHost.exe"));
    same.then_some(hwnd)
}

/// 某进程按 Z 序最上层的可见、无所有者、非工具窗口的顶层窗口
fn main_window_of(pid: u32) -> Option<HWND> {
    struct Ctx {
        pid: u32,
        found: Option<RawHwnd>,
    }
    unsafe extern "system" fn visit(hwnd: HWND, data: LPARAM) -> BOOL {
        // SAFETY: data 指向下面的局部 Ctx，枚举同步进行
        let ctx = unsafe { &mut *(data.0 as *mut Ctx) };
        let (_, pid) = process::window_thread_process(hwnd);
        if pid != ctx.pid {
            return BOOL(1);
        }
        // SAFETY: 纯查询
        let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
        // SAFETY: 纯查询
        let owned = unsafe { GetWindow(hwnd, GW_OWNER) }.is_ok_and(|o| !o.0.is_null());
        // SAFETY: 纯查询
        let ex = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32;
        if visible && !owned && ex & WS_EX_TOOLWINDOW.0 == 0 {
            ctx.found = Some(RawHwnd::new(hwnd));
            return BOOL(0);
        }
        BOOL(1)
    }
    let mut ctx = Ctx { pid, found: None };
    // SAFETY: 回调只在本调用期间同步执行；提前结束时 EnumWindows 返回错误，忽略即可
    unsafe {
        let _ = EnumWindows(Some(visit), LPARAM(&mut ctx as *mut Ctx as isize));
    }
    ctx.found.map(RawHwnd::hwnd)
}
