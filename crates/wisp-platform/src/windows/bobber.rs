//! 浮标窗口（`mouse-hook` feature）：原生分层窗口，不用 WebView。
//!
//! - 两个叠在一起的分层窗口：**圆盘窗口**只有圆盘及其抗锯齿边缘的像素不透明（分层窗口按 alpha 做命中测试，
//!   只有图标区域接收点击、显示手形光标）；**投影窗口**加 `WS_EX_TRANSPARENT`，完全点击穿透。圆盘窗口由投影窗口
//!   拥有（owned window 永远在所有者之上），两者同步移动、同步淡入淡出；
//! - 都是 `WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE`，`WM_MOUSEACTIVATE` 返回
//!   `MA_NOACTIVATE`：点击不抢焦点、不改变源应用的选区，不进任务栏 / Alt-Tab；
//! - 内容用 `UpdateLayeredWindow` 推送预乘 BGRA；出现动画的缩放帧预渲染（`bobber_art`）。动画只由 vsync 驱动：
//!   动画期间每次 DWM 合成（`DwmFlush`）推送一帧，60 / 120 / 144 Hz 下帧间隔都均匀；动画结束即回到阻塞的
//!   `GetMessageW`，静止时零唤醒。悬停高亮 80 ms、按下缩到 0.96；4 s 无交互（悬停时暂停计时）淡出；
//! - 窗口在第一次显示时创建；浮标功能关闭（钩子订阅里不再有浮标）后，下一次隐藏时销毁窗口并结束线程，
//!   功能关闭时进程中没有这两个窗口。

use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use ::windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use ::windows::Win32::Graphics::Dwm::{DWM_TIMING_INFO, DwmFlush, DwmGetCompositionTimingInfo};
use ::windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, CreateCompatibleDC,
    CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, HBITMAP, HDC, HGDIOBJ, SelectObject,
};
use ::windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, SetWaitableTimer, TIMER_ALL_ACCESS,
    WaitForSingleObject,
};
use ::windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use ::windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_TOPMOST, IDC_HAND, KillTimer, LoadCursorW,
    MA_NOACTIVATE, MSG, PM_REMOVE, PeekMessageW, PostMessageW, PostQuitMessage, SW_HIDE, SW_SHOWNOACTIVATE,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetTimer, SetWindowPos, ShowWindow, ULW_ALPHA, UpdateLayeredWindow, WM_APP,
    WM_DESTROY, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_QUIT, WM_TIMER, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use ::windows::core::PCWSTR;
use parking_lot::Mutex;
use wisp_types::{MonitorId, Point, Rect};

use super::bobber_art::{self as art, Frames};
use super::geom;
use super::hooks::{BobberGeom, HookHub};
use super::monitor;
use super::util::{DpiScope, OwnedHandle, RawHwnd, call_guarded};
use super::wnd;
use crate::common::gesture::{
    BOBBER_DIAMETER, BOBBER_IDLE_TIMEOUT, BOBBER_MOVE_AWAY_PX, bobber_rect, bobber_window_rect,
};

const CLASS: &str = "WispBobber";
const WM_APP_SHOW: u32 = WM_APP + 20;
const WM_APP_HIDE: u32 = WM_APP + 21;
const WM_APP_RELEASE: u32 = WM_APP + 22;
/// `WM_MOUSELEAVE`（定义在 Win32_UI_Controls，为免多开一个 feature 在此直接给出）
const WM_MOUSELEAVE: u32 = 0x02A3;
const TIMER_IDLE: usize = 2;
/// 取不到显示器刷新率时的帧周期（60 Hz）
const DEFAULT_FRAME: Duration = Duration::from_micros(16_667);

type ClickCallback = Arc<dyn Fn() + Send + Sync>;

/// 无交互自动消失
fn idle_ms() -> u32 {
    u32::try_from(BOBBER_IDLE_TIMEOUT.as_millis()).unwrap_or(4000)
}

struct ShowRequest {
    at: (MonitorId, Point),
    dark: bool,
    reduced: bool,
    on_click: ClickCallback,
}

#[derive(Default)]
struct State {
    /// 圆盘窗口（接收 `WM_APP_*` 的那一个）
    window: Option<RawHwnd>,
    request: Option<ShowRequest>,
    /// 功能已关闭：隐藏后销毁窗口
    release: bool,
}

/// 浮标控制器（`BobberWindow` 的实现体）
pub(crate) struct Bobber {
    hub: Arc<HookHub>,
    state: Mutex<State>,
    serial: AtomicU64,
}

impl Bobber {
    pub(crate) fn new(hub: Arc<HookHub>) -> Arc<Self> {
        Arc::new(Self {
            hub,
            state: Mutex::new(State::default()),
            serial: AtomicU64::new(1),
        })
    }

    /// 显示（或移动到新位置重新显示）
    pub(crate) fn show(
        self: &Arc<Self>,
        at: (MonitorId, Point),
        dark: bool,
        reduced_motion: bool,
        on_click: Box<dyn Fn() + Send + Sync>,
    ) {
        {
            let mut st = self.state.lock();
            st.request = Some(ShowRequest {
                at,
                dark,
                reduced: reduced_motion,
                on_click: Arc::from(on_click),
            });
            st.release = false;
        }
        for _ in 0..2 {
            let Some(h) = self.ensure_window() else { return };
            if post(h, WM_APP_SHOW, 0, 0) {
                return;
            }
            // 窗口恰好在销毁：丢掉旧句柄重建
            self.state.lock().window = None;
        }
    }

    /// 隐藏（无论当前是哪一次显示）
    pub(crate) fn hide_now(&self, animated: bool) {
        self.post_hide(animated, 0);
    }

    /// 只隐藏序号为 `serial` 的那次显示：消失信号到达时上层可能已经在新位置显示了下一次，不能把它也藏掉
    pub(crate) fn hide_shown(&self, serial: u64, animated: bool) {
        self.post_hide(animated, serial);
    }

    /// `serial` 为 0 表示不限定（显示序号从 1 开始）
    fn post_hide(&self, animated: bool, serial: u64) {
        let window = self.state.lock().window;
        if let Some(h) = window {
            post(h, WM_APP_HIDE, usize::from(animated), serial as isize);
        }
    }

    /// 浮标功能已关闭：可见时淡出后销毁，隐藏时立即销毁
    pub(crate) fn release(&self) {
        let window = {
            let mut st = self.state.lock();
            st.release = true;
            st.window
        };
        if let Some(h) = window {
            post(h, WM_APP_RELEASE, 0, 0);
        }
    }

    /// 当前矩形
    pub(crate) fn rect(&self) -> Option<(MonitorId, Rect)> {
        self.hub.bobber_rect()
    }

    fn ensure_window(self: &Arc<Self>) -> Option<RawHwnd> {
        let mut st = self.state.lock();
        if let Some(h) = st.window {
            return Some(h);
        }
        let (tx, rx) = mpsc::sync_channel::<Result<RawHwnd, String>>(1);
        let me = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("wisp-bobber".into())
            .spawn(move || window_main(me, tx));
        if let Err(e) = spawned {
            log::error!("failed to start bobber thread: {e}");
            return None;
        }
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(h)) => {
                st.window = Some(h);
                Some(h)
            }
            Ok(Err(e)) => {
                log::error!("bobber window: {e}");
                None
            }
            Err(_) => {
                log::error!("bobber window did not start in time");
                None
            }
        }
    }
}

fn post(h: RawHwnd, msg: u32, wparam: usize, lparam: isize) -> bool {
    // SAFETY: 向浮标线程的窗口投递消息；窗口已销毁时返回错误
    unsafe { PostMessageW(Some(h.hwnd()), msg, WPARAM(wparam), LPARAM(lparam)) }.is_ok()
}

enum Anim {
    Enter { start: Instant },
    Exit { start: Instant, from: f64 },
}

struct Dib {
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    size: i32,
}

/// 一个分层窗口及其离屏位图
struct Surface {
    hwnd: HWND,
    memdc: HDC,
    dib: Option<Dib>,
    /// 最近一次推送的（内容, 不透明度, 位置）：内容与参数都没变时跳过推送
    pushed: Option<(FrameId, u8, (i32, i32))>,
}

/// 推送到窗口的内容标识
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameId {
    Enter(usize),
    Shadow,
    ShadowPressed,
    /// 圆盘的静止 / 悬停 / 按下帧（可能是混合结果，每次都推送）
    Live,
}

impl Surface {
    fn new(hwnd: HWND) -> Self {
        Self {
            hwnd,
            // SAFETY: 与屏幕兼容的内存 DC，线程结束时删除
            memdc: unsafe { CreateCompatibleDC(None) },
            dib: None,
            pushed: None,
        }
    }

    fn ensure_dib(&mut self, size: i32) -> bool {
        if self.dib.as_ref().is_some_and(|d| d.size == size) {
            return true;
        }
        self.drop_dib();
        self.pushed = None;
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size, // 自上而下
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        // SAFETY: bmi 完整初始化；bits 接收像素指针
        let bitmap = match unsafe { CreateDIBSection(Some(self.memdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) } {
            Ok(b) if !bits.is_null() => b,
            Ok(b) => {
                // SAFETY: 释放刚创建的位图
                unsafe {
                    let _ = DeleteObject(HGDIOBJ(b.0));
                }
                return false;
            }
            Err(e) => {
                log::error!("CreateDIBSection failed: {e}");
                return false;
            }
        };
        // SAFETY: 把 DIB 选入内存 DC，保存原对象以便归还
        let old = unsafe { SelectObject(self.memdc, HGDIOBJ(bitmap.0)) };
        self.dib = Some(Dib {
            bitmap,
            old,
            bits: bits as *mut u8,
            size,
        });
        true
    }

    /// DIB 的像素（size × size × 4 字节）
    fn bits(&mut self) -> Option<&mut [u8]> {
        let d = self.dib.as_ref()?;
        let len = (d.size * d.size * 4) as usize;
        // SAFETY: bits 指向 CreateDIBSection 分配的 size×size×4 字节，DIB 存活期间有效，只在本线程访问
        Some(unsafe { std::slice::from_raw_parts_mut(d.bits, len) })
    }

    /// 把 DIB 推送到分层窗口
    fn present(&mut self, id: FrameId, alpha: f64, pos: (i32, i32)) {
        let Some(size) = self.dib.as_ref().map(|d| d.size) else {
            return;
        };
        let a8 = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
        if id != FrameId::Live && self.pushed == Some((id, a8, pos)) {
            return;
        }
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: a8,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let dst = POINT { x: pos.0, y: pos.1 };
        let sz = SIZE { cx: size, cy: size };
        let src = POINT { x: 0, y: 0 };
        // SAFETY: memdc 中选入了 size×size 的 32 位 DIB；所有指针指向局部变量
        let r = unsafe {
            UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&dst),
                Some(&sz),
                Some(self.memdc),
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )
        };
        match r {
            Ok(()) => self.pushed = Some((id, a8, pos)),
            Err(e) => {
                self.pushed = None;
                log::debug!("UpdateLayeredWindow failed: {e}");
            }
        }
    }

    fn drop_dib(&mut self) {
        if let Some(d) = self.dib.take() {
            // SAFETY: 归还原对象后删除 DIB
            unsafe {
                SelectObject(self.memdc, d.old);
                let _ = DeleteObject(HGDIOBJ(d.bitmap.0));
            }
        }
    }

    fn release(mut self) {
        self.drop_dib();
        if !self.memdc.is_invalid() {
            // SAFETY: 删除本线程创建的内存 DC
            unsafe {
                let _ = DeleteDC(self.memdc);
            }
        }
    }
}

/// 浮标线程上的窗口状态
struct Win {
    /// 圆盘窗口：接收点击、悬停与 `WM_APP_*`
    disc: Surface,
    /// 投影窗口：点击穿透，拥有圆盘窗口
    shadow: Surface,
    me: Arc<Bobber>,
    frames: Option<Frames>,
    visible: bool,
    /// 当前这次显示的序号（`BobberGeom.serial`）
    serial: u64,
    pos: (i32, i32),
    alpha: f64,
    reduced: bool,
    anim: Option<Anim>,
    hover: f64,
    hover_from: f64,
    hover_to: f64,
    hover_start: Option<Instant>,
    hovering: bool,
    pressed: bool,
    on_click: Option<ClickCallback>,
}

thread_local! {
    static WIN: RefCell<Option<Win>> = const { RefCell::new(None) };
}

fn window_main(me: Arc<Bobber>, ready: mpsc::SyncSender<Result<RawHwnd, String>>) {
    let _dpi = DpiScope::per_monitor();
    // SAFETY: 加载系统预定义光标，无需释放
    let cursor = unsafe { LoadCursorW(None, IDC_HAND) }.ok();
    if !wnd::ensure_class(CLASS, Some(wndproc), cursor) {
        let _ = ready.send(Err("register class failed".into()));
        return;
    }
    let base = WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE;
    // 先建投影窗口（完全点击穿透），再建由它拥有的圆盘窗口：owned window 永远在所有者之上
    let shadow = match wnd::create_window(CLASS, base | WS_EX_TRANSPARENT, WS_POPUP, None) {
        Ok(h) => h,
        Err(e) => {
            let _ = ready.send(Err(e.to_string()));
            return;
        }
    };
    let disc = match wnd::create_window(CLASS, base, WS_POPUP, Some(shadow)) {
        Ok(h) => h,
        Err(e) => {
            // SAFETY: 本线程刚创建的窗口
            unsafe {
                let _ = DestroyWindow(shadow);
            }
            let _ = ready.send(Err(e.to_string()));
            return;
        }
    };
    WIN.with(|w| {
        *w.borrow_mut() = Some(Win {
            disc: Surface::new(disc),
            shadow: Surface::new(shadow),
            me,
            frames: None,
            visible: false,
            serial: 0,
            pos: (0, 0),
            alpha: 0.0,
            reduced: false,
            anim: None,
            hover: 0.0,
            hover_from: 0.0,
            hover_to: 0.0,
            hover_start: None,
            hovering: false,
            pressed: false,
            on_click: None,
        });
    });
    let _ = ready.send(Ok(RawHwnd::new(disc)));
    run_loop();
    WIN.with(|w| {
        if let Some(win) = w.borrow_mut().take() {
            win.disc.release();
            win.shadow.release();
        }
    });
}

/// 浮标线程的消息循环：静止时阻塞在 `GetMessageW`（零唤醒）；动画期间每个 vsync 推一帧，帧间处理完全部消息
fn run_loop() {
    let mut pacer = Pacer::new();
    let mut msg = MSG::default();
    loop {
        // SAFETY: 标准消息循环；msg 指向局部变量
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            if msg.message == WM_QUIT {
                return;
            }
            // SAFETY: 消息来自本线程的队列
            unsafe {
                DispatchMessageW(&msg);
            }
        }
        if with_win(|w| w.animating()).unwrap_or(false) {
            let paced = pacer.wait_frame();
            with_win(|w| if paced { w.tick() } else { w.skip_animation() });
            continue;
        }
        pacer.reset();
        // SAFETY: 同上
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 == 0 || r.0 == -1 {
            return;
        }
        // SAFETY: 同上
        unsafe {
            DispatchMessageW(&msg);
        }
    }
}

/// 在本线程的窗口状态上执行（正处于同步重入时返回 None）
fn with_win<R>(f: impl FnOnce(&mut Win) -> R) -> Option<R> {
    WIN.with(|w| match w.try_borrow_mut() {
        Ok(mut g) => g.as_mut().map(f),
        Err(_) => None,
    })
}

/// 动画节拍：每帧等到 DWM 的下一次合成（vsync）。`DwmFlush` 失败或明显提前返回（DWM 没有待合成的内容、远程会话等）
/// 时，用高精度可等待定时器补足到「上一帧 + 一个刷新周期」，绝不空转
struct Pacer {
    timer: Option<OwnedHandle>,
    timer_tried: bool,
    period: Duration,
    last: Option<Instant>,
}

impl Pacer {
    fn new() -> Self {
        Self {
            timer: None,
            timer_tried: false,
            period: DEFAULT_FRAME,
            last: None,
        }
    }

    /// 动画结束：下一段动画重新取刷新率、重新对齐
    fn reset(&mut self) {
        self.last = None;
    }

    /// 等到下一帧。返回 false 表示无法节流（连可等待定时器都建不出来），调用方应直接跳到动画终点
    fn wait_frame(&mut self) -> bool {
        if self.last.is_none() {
            self.period = refresh_period();
        }
        // SAFETY: 无参数；阻塞到 DWM 下一次合成完成（最多一个刷新周期）
        let flushed = unsafe { DwmFlush() }.is_ok();
        let now = Instant::now();
        let floor = match self.last {
            Some(last) if !flushed || now < last + self.period / 2 => Some(last + self.period),
            None if !flushed => Some(now + self.period),
            _ => None,
        };
        if let Some(until) = floor
            && !self.wait_until(until)
        {
            return false;
        }
        self.last = Some(Instant::now());
        true
    }

    /// 用可等待定时器等到 `until`（一次性的有界等待，不处理消息：最多一个刷新周期）
    fn wait_until(&mut self, until: Instant) -> bool {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        let Some(timer) = self.timer() else { return false };
        // 负值表示相对时间，单位 100 ns
        let due = -i64::try_from(left.as_nanos() / 100).unwrap_or(i64::MAX).max(1);
        let cap = u32::try_from(left.as_millis()).unwrap_or(u32::MAX).saturating_add(5);
        // SAFETY: 定时器句柄有效；due 指向局部变量；等待有硬上限
        unsafe {
            if SetWaitableTimer(timer, &due, 0, None, None, false).is_err() {
                return false;
            }
            let _ = WaitForSingleObject(timer, cap);
        }
        true
    }

    fn timer(&mut self) -> Option<::windows::Win32::Foundation::HANDLE> {
        if !self.timer_tried {
            self.timer_tried = true;
            // 高精度定时器需要 Windows 10 1803+，不支持时退回普通精度
            let created = [CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, 0]
                .into_iter()
                .find_map(|flags| {
                    // SAFETY: 匿名定时器，成功时句柄交给守卫关闭
                    unsafe { CreateWaitableTimerExW(None, PCWSTR::null(), flags, TIMER_ALL_ACCESS.0) }.ok()
                });
            match created {
                Some(h) => self.timer = Some(OwnedHandle(h)),
                None => log::warn!(
                    "bobber frame timer unavailable: {}",
                    ::windows::core::Error::from_thread()
                ),
            }
        }
        self.timer.as_ref().map(|t| t.0)
    }
}

/// DWM 的合成周期（显示器刷新率）；取不到时按 60 Hz
fn refresh_period() -> Duration {
    let mut info = DWM_TIMING_INFO {
        cbSize: std::mem::size_of::<DWM_TIMING_INFO>() as u32,
        ..Default::default()
    };
    // SAFETY: cbSize 已设置；hwnd 必须为空（Windows 8.1 起只支持查询整个桌面）
    if unsafe { DwmGetCompositionTimingInfo(HWND::default(), &mut info) }.is_ok() {
        // 结构体是 packed：按值取出字段
        let rate = info.rateRefresh;
        let (num, den) = (rate.uiNumerator, rate.uiDenominator);
        if num > 0 && den > 0 {
            let hz = f64::from(num) / f64::from(den);
            if (24.0..=500.0).contains(&hz) {
                return Duration::from_secs_f64(1.0 / hz);
            }
        }
    }
    DEFAULT_FRAME
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        // 点击不激活窗口：源应用保持前台、选区不丢
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_DESTROY => {
            // SAFETY: 结束本线程的消息循环（两个窗口都会走到这里，重复投递 WM_QUIT 无害）
            unsafe { PostQuitMessage(0) };
            return LRESULT(0);
        }
        _ => {}
    }
    // 只有圆盘窗口处理消息；本线程内的同步重入（ShowWindow / DestroyWindow 等发送的消息）交给默认处理
    let handled = with_win(|win| win.disc.hwnd == hwnd && win.handle(msg, wparam, lparam)).unwrap_or(false);
    if handled {
        LRESULT(0)
    } else {
        // SAFETY: 默认处理
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }
}

impl Win {
    fn handle(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> bool {
        match msg {
            WM_APP_SHOW => {
                let req = self.me.state.lock().request.take();
                if let Some(req) = req {
                    self.show(req);
                }
                true
            }
            WM_APP_HIDE => {
                // lParam 非零时只隐藏那一次显示（见 `Bobber::hide_shown`）
                if lparam.0 == 0 || lparam.0 == self.serial as isize {
                    self.hide(wparam.0 != 0);
                }
                true
            }
            WM_APP_RELEASE => {
                if self.visible {
                    self.hide(true);
                } else {
                    self.destroy();
                }
                true
            }
            WM_TIMER if wparam.0 == TIMER_IDLE => {
                self.kill(TIMER_IDLE);
                if self.visible && !self.hovering && !matches!(self.anim, Some(Anim::Exit { .. })) {
                    // 先认领并报告超时，再开始淡出（淡出开始时会把这次显示标为已消失，顺序反过来就报告不出去了）
                    self.me.hub.bobber_timed_out(self.serial);
                    self.hide(true);
                }
                true
            }
            WM_MOUSEMOVE => {
                if self.visible && !self.hovering && !matches!(self.anim, Some(Anim::Exit { .. })) {
                    self.hovering = true;
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: self.disc.hwnd,
                        dwHoverTime: 0,
                    };
                    // SAFETY: 结构体已完整初始化
                    unsafe {
                        let _ = TrackMouseEvent(&mut tme);
                    }
                    // 悬停时暂停 4 s 计时
                    self.kill(TIMER_IDLE);
                    self.animate_hover(1.0);
                }
                true
            }
            WM_MOUSELEAVE => {
                if self.hovering {
                    self.hovering = false;
                    // 按下后拖出浮标：取消这次点击（抬起时不触发）
                    self.pressed = false;
                    if self.visible && !matches!(self.anim, Some(Anim::Exit { .. })) {
                        self.set_timer(TIMER_IDLE, idle_ms());
                        self.animate_hover(0.0);
                    }
                }
                true
            }
            WM_LBUTTONDOWN => {
                let pressed = self.visible && self.in_circle(lparam);
                if pressed != self.pressed {
                    self.pressed = pressed;
                    // 按下反馈立即可见（缩到 0.96，不等下一帧）
                    self.paint();
                }
                true
            }
            WM_LBUTTONUP => {
                let clicked = self.pressed
                    && self.visible
                    && !matches!(self.anim, Some(Anim::Exit { .. }))
                    && self.in_circle(lparam);
                self.pressed = false;
                if clicked {
                    // 点击后立即消失，再用浮标出现时取到的选区触发翻译
                    let cb = self.on_click.clone();
                    self.finish_hide();
                    if let Some(cb) = cb {
                        // 运行在窗口过程里：panic 不能越过 extern "system" 边界
                        call_guarded("bobber click callback", || cb());
                    }
                }
                true
            }
            _ => false,
        }
    }

    fn in_circle(&self, lparam: LPARAM) -> bool {
        let Some(f) = self.frames.as_ref() else { return false };
        let x = f64::from(lparam.0 as u16 as i16);
        let y = f64::from((lparam.0 >> 16) as u16 as i16);
        let c = f64::from(f.size) / 2.0;
        let r = BOBBER_DIAMETER / 2.0 * f.scale + 1.0;
        (x + 0.5 - c).powi(2) + (y + 0.5 - c).powi(2) <= r * r
    }

    /// 是否有正在进行的动画（出现 / 消失 / 悬停过渡）
    fn animating(&self) -> bool {
        self.visible && (self.anim.is_some() || self.hover_start.is_some())
    }

    fn show(&mut self, req: ShowRequest) {
        let mon = monitor::monitor_by_id(&req.at.0).or_else(|| {
            let (x, y) = monitor::cursor_phys()?;
            monitor::monitor_nearest(x, y)
        });
        let Some(mon) = mon else {
            log::warn!("bobber: monitor {:?} not found", req.at.0);
            return;
        };
        let scale = geom::sane_scale(mon.scale_factor);
        let rebuild = self
            .frames
            .as_ref()
            .is_none_or(|f| f.dark != req.dark || (f.scale - scale).abs() > 1e-6);
        if rebuild {
            self.frames = Some(Frames::build(scale, req.dark));
        }
        let size = self.frames.as_ref().map_or(1, |f| f.size);
        if !self.disc.ensure_dib(size) || !self.shadow.ensure_dib(size) {
            return;
        }
        // 圆形在抬起点右下方，越界时翻转（common::gesture，三端一致）；窗口在圆形四周留出阴影余量
        let circle = bobber_rect(req.at.1, mon.work_area);
        let window = bobber_window_rect(circle);
        let origin = wisp_types::Point {
            x: window.x,
            y: window.y,
        };
        let (x, y) = geom::local_to_phys(&mon, origin);
        self.pos = (x, y);
        self.reduced = req.reduced;
        self.on_click = Some(req.on_click);
        self.visible = true;
        self.hovering = false;
        self.pressed = false;
        self.hover = 0.0;
        self.hover_start = None;
        self.alpha = 0.0;
        self.anim = Some(Anim::Enter { start: Instant::now() });
        // 帧可能刚按新的明暗 / 缩放重建过：两层都重新写入
        self.disc.pushed = None;
        self.shadow.pushed = None;
        self.paint();
        // SAFETY: 本线程的窗口；不激活。先投影后圆盘，最后把两者提到最上层（圆盘在投影之上）
        unsafe {
            let _ = ShowWindow(self.shadow.hwnd, SW_SHOWNOACTIVATE);
            let _ = ShowWindow(self.disc.hwnd, SW_SHOWNOACTIVATE);
            for h in [self.shadow.hwnd, self.disc.hwnd] {
                let _ = SetWindowPos(
                    h,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
        self.set_timer(TIMER_IDLE, idle_ms());
        let half = f64::from(size) / 2.0;
        self.serial = self.me.serial.fetch_add(1, Ordering::Relaxed);
        let geom = BobberGeom {
            serial: self.serial,
            monitor: mon.id.clone(),
            local: circle,
            center: (f64::from(x) + half, f64::from(y) + half),
            radius: BOBBER_DIAMETER / 2.0 * scale,
            dismiss: BOBBER_MOVE_AWAY_PX * scale,
        };
        self.me.hub.set_bobber_visible(Some(geom));
    }

    fn hide(&mut self, animated: bool) {
        if !self.visible {
            if self.me.state.lock().release {
                self.destroy();
            }
            return;
        }
        if !animated {
            self.finish_hide();
            return;
        }
        if matches!(self.anim, Some(Anim::Exit { .. })) {
            return;
        }
        self.kill(TIMER_IDLE);
        // 开始淡出：这次显示不再报告消失（上层主动隐藏，或消失已经报告过）。淡出期间的按键、滚轮、点击、应用切换
        // 不会让上层对同一次浮标再收到一次 BobberDismiss
        self.me.hub.bobber_hiding(self.serial);
        self.anim = Some(Anim::Exit {
            start: Instant::now(),
            from: self.alpha,
        });
    }

    fn finish_hide(&mut self) {
        self.kill(TIMER_IDLE);
        if self.visible {
            // SAFETY: 本线程的窗口
            unsafe {
                let _ = ShowWindow(self.disc.hwnd, SW_HIDE);
                let _ = ShowWindow(self.shadow.hwnd, SW_HIDE);
            }
        }
        self.visible = false;
        self.anim = None;
        self.hover_start = None;
        self.hovering = false;
        self.on_click = None;
        self.me.hub.set_bobber_visible(None);
        let release = self.me.state.lock().release;
        if release || !self.me.hub.bobber_enabled() {
            self.destroy();
        }
    }

    /// 销毁窗口并结束线程（WM_DESTROY → PostQuitMessage）
    fn destroy(&mut self) {
        {
            // 「检查有无待处理的显示请求」与「解除登记」必须在同一把锁内完成：否则 `show` 可能在两步之间拿到
            // 旧句柄并投递请求，或者另起一个新窗口而让旧窗口永远留在进程里
            let mut st = self.me.state.lock();
            if st.request.is_some() {
                // 已经有新的显示请求在路上：保留窗口，处理完它
                return;
            }
            if st.window == Some(RawHwnd::new(self.disc.hwnd)) {
                st.window = None;
            }
        }
        // SAFETY: 本线程的窗口；销毁所有者时系统先销毁它拥有的圆盘窗口。只销毁一次（destroy 之后 WIN 被清空）
        unsafe {
            let _ = DestroyWindow(self.shadow.hwnd);
        }
    }

    fn animate_hover(&mut self, to: f64) {
        self.hover_from = self.hover;
        self.hover_to = to;
        self.hover_start = Some(Instant::now());
    }

    /// 无法按帧节流时（极少见）：直接跳到各段动画的终点
    fn skip_animation(&mut self) {
        if let Some(Anim::Exit { .. }) = self.anim {
            self.finish_hide();
            return;
        }
        self.anim = None;
        if self.hover_start.take().is_some() {
            self.hover = self.hover_to;
        }
        self.paint();
    }

    /// 推进一帧（由 vsync 节拍驱动）
    fn tick(&mut self) {
        let now = Instant::now();
        if let Some(start) = self.hover_start {
            let t = now.duration_since(start).as_secs_f64() * 1000.0 / art::hover_ms();
            let e = if self.reduced {
                t.min(1.0)
            } else {
                art::ease_standard(t)
            };
            self.hover = self.hover_from + (self.hover_to - self.hover_from) * e;
            if t >= 1.0 {
                self.hover = self.hover_to;
                self.hover_start = None;
            }
        }
        match self.anim {
            Some(Anim::Enter { start }) => {
                let dur = if self.reduced {
                    art::reduced_ms()
                } else {
                    art::enter_ms()
                };
                if now.duration_since(start).as_secs_f64() * 1000.0 >= dur {
                    self.anim = None;
                }
            }
            Some(Anim::Exit { start, .. }) => {
                let dur = if self.reduced {
                    art::reduced_ms()
                } else {
                    art::exit_ms()
                };
                if now.duration_since(start).as_secs_f64() * 1000.0 >= dur {
                    self.finish_hide();
                    return;
                }
            }
            None => {}
        }
        self.paint();
    }

    /// 选出当前帧并推送到两个窗口
    fn paint(&mut self) {
        let Win {
            disc,
            shadow,
            frames,
            anim,
            reduced,
            hover,
            pressed,
            pos,
            alpha,
            ..
        } = self;
        let Some(frames) = frames.as_ref() else { return };
        let Some(disc_bits) = disc.bits() else { return };
        let now = Instant::now();
        let (hover, pressed, reduced) = (*hover, *pressed, *reduced);
        // 静止 / 悬停 / 按下：圆盘按悬停程度混合，投影只随按下变化
        let static_disc = |bits: &mut [u8]| {
            if pressed {
                bits.copy_from_slice(&frames.pressed);
            } else if hover <= 0.0 {
                bits.copy_from_slice(&frames.normal);
            } else if hover >= 1.0 {
                bits.copy_from_slice(&frames.hover);
            } else {
                art::blend(&frames.normal, &frames.hover, hover, bits);
            }
        };
        let static_shadow = if pressed {
            (FrameId::ShadowPressed, &frames.shadow_pressed[..])
        } else {
            (FrameId::Shadow, &frames.shadow[..])
        };
        let (a, disc_id, (shadow_id, shadow_px)) = match *anim {
            Some(Anim::Enter { start }) => {
                let ms = now.duration_since(start).as_secs_f64() * 1000.0;
                if reduced {
                    static_disc(disc_bits);
                    ((ms / art::reduced_ms()).clamp(0.0, 1.0), FrameId::Live, static_shadow)
                } else {
                    let p = (ms / art::enter_ms()).clamp(0.0, 1.0);
                    let i = art::enter_index(p);
                    let (d, s) = frames.enter_frame(i);
                    disc_bits.copy_from_slice(d);
                    (art::ease_standard(p), FrameId::Enter(i), (FrameId::Enter(i), s))
                }
            }
            Some(Anim::Exit { start, from }) => {
                let ms = now.duration_since(start).as_secs_f64() * 1000.0;
                static_disc(disc_bits);
                let a = if reduced {
                    from * (1.0 - (ms / art::reduced_ms()).clamp(0.0, 1.0))
                } else {
                    from * (1.0 - art::ease_exit((ms / art::exit_ms()).clamp(0.0, 1.0)))
                };
                (a, FrameId::Live, static_shadow)
            }
            None => {
                static_disc(disc_bits);
                (1.0, FrameId::Live, static_shadow)
            }
        };
        *alpha = a;
        let pos = *pos;
        // 投影内容没变时不必重写位图（present 也会跳过没有变化的推送）
        let stale = shadow.pushed.map(|(id, ..)| id) != Some(shadow_id);
        if let Some(bits) = shadow.bits() {
            if stale {
                bits.copy_from_slice(shadow_px);
            }
            shadow.present(shadow_id, a, pos);
        }
        disc.present(disc_id, a, pos);
    }

    fn set_timer(&self, id: usize, ms: u32) {
        // SAFETY: 本窗口的定时器；重复设置会重置
        unsafe {
            SetTimer(Some(self.disc.hwnd), id, ms, None);
        }
    }

    fn kill(&self, id: usize) {
        // SAFETY: 本窗口的定时器；未设置时失败，忽略
        unsafe {
            let _ = KillTimer(Some(self.disc.hwnd), id);
        }
    }
}
