//! 低级输入钩子与外部点击监听（`OutsideClick`）。
//!
//! - 所有订阅（外部点击、扩展触发）共用**一个**钩子线程：按订阅的并集决定安装 `WH_MOUSE_LL` / `WH_KEYBOARD_LL`，
//!   订阅全部注销时卸载钩子并结束线程（全关时没有钩子线程）；
//! - 钩子回调运行在钩子线程的消息循环里，只做 O(1) 的判断后把事件送进 channel，由**分发线程**做坐标换算、
//!   浮标判定与回调订阅者，保证回调 < 1 ms；唯一的例外是中键拦截判定（同步查询 UIA，硬上限 30 ms）；
//! - 扩展触发忽略 `LLMHF_INJECTED` / `LLKHF_INJECTED` 与带 `WISP_TAG` 的事件；外部点击只忽略带
//!   `WISP_TAG` 的事件：软件 KVM（Mouse Without Borders、Input Leap）与远程控制软件的点击都是注入的，同样要能关闭面板；
//! - 键盘钩子只在「双击修饰键」开启或浮标可见时安装（浮标的「任意按键消失」）。
//!
//! **自愈**（超时一次后能自动恢复）：回调超过 `LowLevelHooksTimeout`（Win10 1709 起上限 1 s）时，系统会
//! 静默卸载低级钩子且不发任何通知。以下时机会重新安装（先装新的、再卸旧的，中间不处理消息，不丢事件）：
//! 每次重配（含浮标显示 / 隐藏）、睡眠唤醒（钩子线程的隐藏顶层窗口收 `WM_POWERBROADCAST`）、桌面切换（锁屏 / 解锁、
//! UAC，`EVENT_SYSTEM_DESKTOPSWITCH`）。另外每次前台切换时：
//! - 装有鼠标钩子：做一次存活检查——光标位置与鼠标钩子最后见到的位置不一致（「光标动过而钩子没见到」，可靠），
//!   判定失效就重装并记 info 日志（含累计次数，可用于诊断）；
//! - 只有键盘钩子（只开了双击修饰键）：系统没有「只含键盘输入」的最后输入时间（`GetLastInputInfo` 含鼠标），无法
//!   可靠地判定键盘钩子是否失效，因此**每次前台切换都例行重装**。这是有意的设计：重装只是两次系统调用（微秒级，
//!   只在切换应用时发生），不计入重装次数、不记日志。
//!
//! 全部由已有事件驱动，没有任何轮询。

use std::cell::RefCell;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use ::windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use ::windows::Win32::System::Threading::GetCurrentThreadId;
use ::windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use ::windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DefWindowProcW, DestroyWindow, EVENT_SYSTEM_DESKTOPSWITCH, GetCursorPos, HC_ACTION, HHOOK,
    KBDLLHOOKSTRUCT, LLKHF_INJECTED, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND,
    PostQuitMessage, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL,
    WINEVENT_OUTOFCONTEXT, WM_APP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE,
    WM_POWERBROADCAST, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_XBUTTONDOWN, WM_XBUTTONUP, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_POPUP, XBUTTON1, XBUTTON2,
};
use parking_lot::Mutex;

use crate::SelfHitTest;
use crate::error::PlatformError;
use crate::types::{ClickEvent, ClickKind, HookHandle, PointerButton};

use super::monitor;
use super::util::{DpiScope, WISP_TAG, call_guarded};
use super::wnd;

#[cfg(feature = "mouse-hook")]
pub(crate) use extended::BobberGeom;

/// 钩子线程的消息
const WM_APP_RECONFIG: u32 = WM_APP + 10;
const WM_APP_QUIT: u32 = WM_APP + 11;
#[cfg(feature = "mouse-hook")]
const WM_APP_REINJECT: u32 = WM_APP + 12;
/// 无条件重装钩子（wParam 为 [`Rearm`] 的编号）
const WM_APP_REARM: u32 = WM_APP + 13;
/// 存活检查：判定钩子已被系统卸载时重装
const WM_APP_CHECK: u32 = WM_APP + 14;

/// 钩子线程上接收 `WM_POWERBROADCAST` 的隐藏顶层窗口（message-only 窗口收不到广播）
const SINK_CLASS: &str = "WispHookSink";

/// 重装钩子的原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rearm {
    /// 睡眠唤醒（换页最容易让回调超时）
    Resume = 1,
    /// 桌面切换（锁屏 / 解锁、UAC 安全桌面）
    DesktopSwitch = 2,
    /// 存活检查：光标移动过而鼠标钩子没有见到（可靠的判定）
    Stale = 3,
    /// 只有键盘钩子时，每次前台切换例行重装（无法可靠判定键盘钩子是否失效，见模块文档）；不计数、不记日志
    KeyboardRefresh = 4,
}

impl Rearm {
    fn from_wparam(v: usize) -> Self {
        match v {
            1 => Self::Resume,
            2 => Self::DesktopSwitch,
            4 => Self::KeyboardRefresh,
            _ => Self::Stale,
        }
    }
}

type ClickSink = Arc<dyn Fn(ClickEvent) + Send + Sync>;

/// 钩子线程 → 分发线程的事件（钩子回调里只构造这些小结构体）
enum HubEvent {
    Click {
        kind: ClickKind,
        button: PointerButton,
        x: i32,
        y: i32,
    },
    #[cfg(feature = "mouse-hook")]
    Ext(extended::ExtEvent),
}

/// 钩子线程读取的配置（订阅的并集）
#[derive(Clone, Default)]
struct HookConfig {
    clicks: bool,
    #[cfg(feature = "mouse-hook")]
    ext: extended::ExtConfig,
}

impl HookConfig {
    fn wants_mouse(&self) -> bool {
        #[cfg(feature = "mouse-hook")]
        if self.ext.wants_mouse() {
            return true;
        }
        self.clicks
    }

    fn wants_keyboard(&self) -> bool {
        #[cfg(feature = "mouse-hook")]
        if self.ext.wants_keyboard() {
            return true;
        }
        false
    }

    fn is_empty(&self) -> bool {
        !self.wants_mouse() && !self.wants_keyboard()
    }
}

/// 调用方与钩子线程之间的配置槽：写入新配置后投递 `WM_APP_RECONFIG`，钩子线程应用后通过 ack 回报结果
struct Slot {
    cfg: HookConfig,
    ack: Option<mpsc::SyncSender<Result<(), String>>>,
}

struct HookThread {
    tid: u32,
    slot: Arc<Mutex<Slot>>,
    /// 分发线程的入口（前台切换、浮标超时等外部事件也从这里送入）
    events: mpsc::Sender<HubEvent>,
}

struct HubInner {
    next_id: u64,
    clicks: Vec<(u64, ClickSink)>,
    #[cfg(feature = "mouse-hook")]
    ext: extended::ExtSubs,
    thread: Option<HookThread>,
}

/// 钩子总线
pub(crate) struct HookHub {
    inner: Mutex<HubInner>,
    #[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
    self_hit: SelfHitTest,
    #[cfg(feature = "mouse-hook")]
    uia: Arc<super::uia::Uia>,
    /// 浮标（构造 Platform 时绑定；Weak 避免循环引用）
    #[cfg(feature = "mouse-hook")]
    bobber: std::sync::OnceLock<std::sync::Weak<super::bobber::Bobber>>,
}

impl HookHub {
    pub(crate) fn new(uia: Arc<super::uia::Uia>, self_hit: SelfHitTest) -> Arc<Self> {
        #[cfg(not(feature = "mouse-hook"))]
        let _ = uia;
        Arc::new(Self {
            inner: Mutex::new(HubInner {
                next_id: 1,
                clicks: Vec::new(),
                #[cfg(feature = "mouse-hook")]
                ext: extended::ExtSubs::default(),
                thread: None,
            }),
            self_hit,
            #[cfg(feature = "mouse-hook")]
            uia,
            #[cfg(feature = "mouse-hook")]
            bobber: std::sync::OnceLock::new(),
        })
    }

    /// 外部点击监听（`OutsideClick::watch`）
    pub(crate) fn watch_clicks(
        self: &Arc<Self>,
        cb: Box<dyn Fn(ClickEvent) + Send + Sync>,
    ) -> Result<HookHandle, PlatformError> {
        let id = {
            let mut inner = self.inner.lock();
            let id = inner.next_id;
            inner.next_id += 1;
            inner.clicks.push((id, Arc::from(cb)));
            if let Err(e) = self.reconcile(&mut inner) {
                inner.clicks.retain(|(i, _)| *i != id);
                let _ = self.reconcile(&mut inner);
                return Err(PlatformError::Other(e));
            }
            id
        };
        let me = Arc::clone(self);
        Ok(HookHandle::new(move || {
            let mut inner = me.inner.lock();
            inner.clicks.retain(|(i, _)| *i != id);
            if let Err(e) = me.reconcile(&mut inner) {
                log::warn!("hook reconcile after click unsubscribe: {e}");
            }
        }))
    }

    fn config(inner: &HubInner) -> HookConfig {
        HookConfig {
            clicks: !inner.clicks.is_empty(),
            #[cfg(feature = "mouse-hook")]
            ext: inner.ext.config(),
        }
    }

    /// 按订阅并集启动 / 重配 / 结束钩子线程（调用方持有 inner 锁；钩子线程从不获取该锁，因此等待 ack 不会死锁）
    fn reconcile(self: &Arc<Self>, inner: &mut HubInner) -> Result<(), String> {
        let cfg = Self::config(inner);
        #[cfg(feature = "mouse-hook")]
        self.after_config(&cfg);
        if cfg.is_empty() {
            if let Some(t) = inner.thread.take() {
                // SAFETY: 向钩子线程投递退出消息；线程已退出时失败，忽略
                unsafe {
                    let _ = PostThreadMessageW(t.tid, WM_APP_QUIT, WPARAM(0), LPARAM(0));
                }
            }
            return Ok(());
        }
        if inner.thread.is_none() {
            inner.thread = Some(self.spawn(cfg)?);
            return Ok(());
        }
        let Some(t) = inner.thread.as_ref() else {
            return Ok(());
        };
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        {
            let mut slot = t.slot.lock();
            slot.cfg = cfg;
            slot.ack = Some(ack_tx);
        }
        // SAFETY: 向钩子线程投递重配消息
        if unsafe { PostThreadMessageW(t.tid, WM_APP_RECONFIG, WPARAM(0), LPARAM(0)) }.is_err() {
            // 线程已经不在了：丢弃句柄，下次重新创建
            inner.thread = None;
            return Err("hook thread is gone".into());
        }
        ack_rx
            .recv_timeout(Duration::from_millis(500))
            .map_err(|_| "hook thread did not respond".to_owned())?
    }

    fn spawn(self: &Arc<Self>, cfg: HookConfig) -> Result<HookThread, String> {
        let (ev_tx, ev_rx) = mpsc::channel::<HubEvent>();
        let slot = Arc::new(Mutex::new(Slot { cfg, ack: None }));
        let weak = Arc::downgrade(self);
        std::thread::Builder::new()
            .name("wisp-hook-dispatch".into())
            .spawn(move || {
                // 坐标换算需要物理像素
                let _dpi = DpiScope::per_monitor();
                while let Ok(ev) = ev_rx.recv() {
                    let Some(hub) = weak.upgrade() else { break };
                    // 单个订阅者 panic 不能让分发线程退出（钩子仍装着，之后的事件会全部丢失）
                    call_guarded("hook event dispatch", || hub.dispatch(ev));
                }
            })
            .map_err(|e| format!("spawn hook dispatcher: {e}"))?;

        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<u32, String>>(1);
        let thread_slot = Arc::clone(&slot);
        let thread_tx = ev_tx.clone();
        #[cfg(feature = "mouse-hook")]
        let uia = Arc::clone(&self.uia);
        std::thread::Builder::new()
            .name("wisp-hooks".into())
            .spawn(move || {
                let state = HookState {
                    slot: thread_slot,
                    tx: thread_tx,
                    cfg: HookConfig::default(),
                    mouse_hook: None,
                    kb_hook: None,
                    seen_pt: None,
                    rearms: 0,
                    #[cfg(feature = "mouse-hook")]
                    ext: extended::ExtState::new(uia),
                };
                hook_main(state, ready_tx);
            })
            .map_err(|e| format!("spawn hook thread: {e}"))?;
        let tid = ready_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| "hook thread did not start in time".to_owned())??;
        Ok(HookThread {
            tid,
            slot,
            events: ev_tx,
        })
    }

    /// 前台应用切换（来自前台追踪线程）：请钩子线程做一次存活检查；浮标可见且还没报告过消失时让它消失
    pub(crate) fn app_switched(&self) {
        let (tid, bobber) = {
            #[cfg_attr(not(feature = "mouse-hook"), allow(unused_mut))]
            let mut inner = self.inner.lock();
            #[cfg(feature = "mouse-hook")]
            let bobber = inner.ext.claim_current();
            #[cfg(not(feature = "mouse-hook"))]
            let bobber: Option<u64> = None;
            (inner.thread.as_ref().map(|t| t.tid), bobber)
        };
        if let Some(tid) = tid {
            // SAFETY: 向钩子线程投递检查消息；线程已退出时失败，忽略
            unsafe {
                let _ = PostThreadMessageW(tid, WM_APP_CHECK, WPARAM(0), LPARAM(0));
            }
        }
        #[cfg(feature = "mouse-hook")]
        if let Some(serial) = bobber {
            self.emit(HubEvent::Ext(extended::ExtEvent::Dismiss(extended::Dismissal {
                reason: crate::types::DismissReason::AppSwitched,
                serial: Some(serial),
                claimed: true,
            })));
        }
        #[cfg(not(feature = "mouse-hook"))]
        let _ = bobber;
    }

    /// 从钩子线程之外送入事件（没有钩子线程时丢弃：此时也没有订阅者）
    #[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
    fn emit(&self, ev: HubEvent) {
        let tx = self.inner.lock().thread.as_ref().map(|t| t.events.clone());
        if let Some(tx) = tx {
            let _ = tx.send(ev);
        }
    }

    fn dispatch(&self, ev: HubEvent) {
        match ev {
            HubEvent::Click { kind, button, x, y } => {
                let sinks: Vec<ClickSink> = self.inner.lock().clicks.iter().map(|(_, s)| Arc::clone(s)).collect();
                if sinks.is_empty() {
                    return;
                }
                let at = monitor::phys_to_anchor_point(x, y);
                let event = ClickEvent { kind, at, button };
                for s in sinks {
                    call_guarded("outside-click callback", || s(event.clone()));
                }
            }
            #[cfg(feature = "mouse-hook")]
            HubEvent::Ext(e) => self.dispatch_ext(e),
        }
    }
}

/// 钩子线程的状态（线程局部；钩子回调通过它访问配置与 channel）
struct HookState {
    slot: Arc<Mutex<Slot>>,
    tx: mpsc::Sender<HubEvent>,
    cfg: HookConfig,
    mouse_hook: Option<HHOOK>,
    kb_hook: Option<HHOOK>,
    /// 鼠标钩子最后一次见到的光标位置（物理像素；存活检查用，任何鼠标事件都会更新）
    seen_pt: Option<POINT>,
    /// 因唤醒、桌面切换或判定失效而重装的次数（日志用；不含只有键盘钩子时的例行重装）
    rearms: u64,
    #[cfg(feature = "mouse-hook")]
    ext: extended::ExtState,
}

thread_local! {
    static STATE: RefCell<Option<HookState>> = const { RefCell::new(None) };
}

/// 卸载一个钩子（系统已经静默卸载过的句柄会调用失败，忽略即可）
fn unhook(h: HHOOK) {
    // SAFETY: 卸载本线程安装的钩子
    unsafe {
        let _ = UnhookWindowsHookEx(h);
    }
}

/// 让某个钩子槽符合期望：不需要时卸载；需要但还没装、或 `fresh` 时安装新钩子并卸载旧的（先装后卸，中间不处理消息，
/// 不会漏掉或重复处理事件）。新钩子装不上时保留旧的
fn sync_hook(
    slot: &mut Option<HHOOK>,
    want: bool,
    fresh: bool,
    set: impl FnOnce() -> ::windows::core::Result<HHOOK>,
) -> ::windows::core::Result<()> {
    if !want {
        if let Some(h) = slot.take() {
            unhook(h);
        }
        return Ok(());
    }
    if slot.is_some() && !fresh {
        return Ok(());
    }
    let new = set()?;
    if let Some(old) = slot.replace(new) {
        unhook(old);
    }
    Ok(())
}

/// 光标位置（物理像素：钩子线程是 Per-Monitor V2）
fn cursor_pos() -> Option<POINT> {
    let mut p = POINT::default();
    // SAFETY: 输出参数指向局部变量
    unsafe { GetCursorPos(&mut p) }.ok().map(|_| p)
}

impl HookState {
    /// 按配置槽安装 / 卸载钩子。每次重配都重新安装已有的钩子：重配发生在订阅变化与浮标显示 / 隐藏时，顺带修复被系统
    /// 静默卸载的钩子
    fn apply(&mut self) -> Result<(), String> {
        let (cfg, ack) = {
            let mut slot = self.slot.lock();
            (slot.cfg.clone(), slot.ack.take())
        };
        let r = self.install(&cfg, true);
        #[cfg(feature = "mouse-hook")]
        self.ext.reconfigure(&cfg.ext);
        self.cfg = cfg;
        if let Some(ack) = ack {
            let _ = ack.try_send(r.clone());
        }
        r
    }

    fn install(&mut self, cfg: &HookConfig, fresh: bool) -> Result<(), String> {
        let hmod = wnd::hinstance();
        let had_mouse = self.mouse_hook.is_some();
        let mouse = sync_hook(&mut self.mouse_hook, cfg.wants_mouse(), fresh, || {
            // SAFETY: 全局低级钩子，回调在本线程的消息循环中执行
            unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), Some(hmod), 0) }
        })
        .map_err(|e| format!("SetWindowsHookExW(WH_MOUSE_LL): {e}"));
        let kb = sync_hook(&mut self.kb_hook, cfg.wants_keyboard(), fresh, || {
            // SAFETY: 同上
            unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), Some(hmod), 0) }
        })
        .map_err(|e| format!("SetWindowsHookExW(WH_KEYBOARD_LL): {e}"));
        // 存活检查的基线：从现在起鼠标钩子应当见到全部光标移动
        if self.mouse_hook.is_some() && (fresh || !had_mouse) {
            self.seen_pt = cursor_pos();
        }
        mouse.and(kb)
    }

    fn uninstall(&mut self) {
        for h in [self.mouse_hook.take(), self.kb_hook.take()].into_iter().flatten() {
            unhook(h);
        }
    }

    /// 无条件重装当前需要的钩子
    fn rearm(&mut self, why: Rearm) {
        if self.mouse_hook.is_none() && self.kb_hook.is_none() {
            return;
        }
        let cfg = self.cfg.clone();
        match self.install(&cfg, true) {
            // 例行重装：不计数、不记日志
            Ok(()) if why == Rearm::KeyboardRefresh => {}
            Ok(()) => {
                self.rearms += 1;
                log::info!("input hooks re-armed ({why:?}, #{})", self.rearms);
            }
            Err(e) => log::error!("failed to re-arm input hooks ({why:?}): {e}"),
        }
    }

    /// 前台切换时的检查，返回需要重装的原因：装有鼠标钩子时，光标位置与鼠标钩子最后见到的不一致即判定失效（鼠标
    /// 钩子与键盘钩子在同一线程、同样受回调超时影响，一并重装）；只有键盘钩子时例行重装（见模块文档）
    fn check(&self) -> Option<Rearm> {
        if self.mouse_hook.is_some() {
            let (now, seen) = (cursor_pos()?, self.seen_pt?);
            return ((now.x, now.y) != (seen.x, seen.y)).then_some(Rearm::Stale);
        }
        self.kb_hook.is_some().then_some(Rearm::KeyboardRefresh)
    }

    fn send(&self, ev: HubEvent) {
        let _ = self.tx.send(ev);
    }

    /// 鼠标事件；返回 true 表示吞掉
    fn on_mouse(&mut self, msg: u32, info: &MSLLHOOKSTRUCT) -> bool {
        self.seen_pt = Some(info.pt);
        // 最频繁的事件放在最前面判定：浮标不可见时鼠标移动与任何订阅都无关
        #[cfg(feature = "mouse-hook")]
        let move_relevant = self.ext.bobber_visible();
        #[cfg(not(feature = "mouse-hook"))]
        let move_relevant = false;
        if msg == WM_MOUSEMOVE && !move_relevant {
            return false;
        }
        // Wisp 自己补发的事件一律放行
        if info.dwExtraInfo == WISP_TAG {
            return false;
        }
        // 外部点击：注入的点击（软件 KVM、远程控制）同样是用户的点击
        if self.cfg.clicks
            && let Some((kind, button)) = click_of(msg, info.mouseData)
        {
            self.send(HubEvent::Click {
                kind,
                button,
                x: info.pt.x,
                y: info.pt.y,
            });
        }
        // 扩展触发只认真实的物理输入
        if info.flags & LLMHF_INJECTED != 0 {
            return false;
        }
        self.ext_mouse(msg, info)
    }

    /// 扩展触发对鼠标事件的处理；返回 true 表示吞掉
    #[cfg(feature = "mouse-hook")]
    fn ext_mouse(&mut self, msg: u32, info: &MSLLHOOKSTRUCT) -> bool {
        // SAFETY: 纯查询
        let tid = unsafe { GetCurrentThreadId() };
        let (swallow, events) = self.ext.on_mouse(msg, info, tid);
        for e in events.into_iter().flatten() {
            self.send(HubEvent::Ext(e));
        }
        swallow
    }

    #[cfg(not(feature = "mouse-hook"))]
    fn ext_mouse(&mut self, _msg: u32, _info: &MSLLHOOKSTRUCT) -> bool {
        false
    }

    fn on_key(&mut self, msg: u32, kb: &KBDLLHOOKSTRUCT) {
        if kb.flags.contains(LLKHF_INJECTED) || kb.dwExtraInfo == WISP_TAG {
            return;
        }
        #[cfg(feature = "mouse-hook")]
        {
            let mouse_hooked = self.mouse_hook.is_some();
            for e in self.ext.on_key(msg, kb, mouse_hooked).into_iter().flatten() {
                self.send(HubEvent::Ext(e));
            }
        }
        #[cfg(not(feature = "mouse-hook"))]
        let _ = (msg, kb);
    }

    fn on_thread_msg(&mut self, msg: &MSG) -> bool {
        match msg.message {
            WM_APP_RECONFIG => {
                if let Err(e) = self.apply() {
                    log::error!("hook reconfigure failed: {e}");
                }
                true
            }
            WM_APP_QUIT => {
                // SAFETY: 结束本线程的消息循环
                unsafe { PostQuitMessage(0) };
                true
            }
            WM_APP_REARM => {
                self.rearm(Rearm::from_wparam(msg.wParam.0));
                true
            }
            WM_APP_CHECK => {
                if let Some(why) = self.check() {
                    self.rearm(why);
                }
                true
            }
            #[cfg(feature = "mouse-hook")]
            WM_APP_REINJECT => {
                self.ext.reinject();
                true
            }
            #[cfg(feature = "mouse-hook")]
            ::windows::Win32::UI::WindowsAndMessaging::WM_TIMER => {
                if let Some(e) = self.ext.on_timer(msg.wParam.0) {
                    self.send(HubEvent::Ext(e));
                }
                true
            }
            _ => false,
        }
    }
}

/// 向本线程投递一条重装请求
fn post_rearm(why: Rearm) {
    // SAFETY: 投递给本线程自己的消息队列
    unsafe {
        let _ = PostThreadMessageW(GetCurrentThreadId(), WM_APP_REARM, WPARAM(why as usize), LPARAM(0));
    }
}

/// 钩子线程上的唤醒 / 桌面切换通知源：隐藏顶层窗口 + `EVENT_SYSTEM_DESKTOPSWITCH` 的 WinEvent 钩子
struct Watchers {
    sink: Option<HWND>,
    desktop: Option<HWINEVENTHOOK>,
}

impl Watchers {
    fn start() -> Self {
        let sink = if wnd::ensure_class(SINK_CLASS, Some(sink_proc), None) {
            // 顶层、从不显示的工具窗口：能收到 WM_POWERBROADCAST，不进任务栏 / Alt-Tab
            match wnd::create_window(SINK_CLASS, WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, WS_POPUP, None) {
                Ok(h) => Some(h),
                Err(e) => {
                    log::warn!("hook resume watcher unavailable: {e}");
                    None
                }
            }
        } else {
            None
        };
        // SAFETY: 进程外（out-of-context）回调在本线程的消息循环中执行；hmod 为 None
        let desktop = unsafe {
            SetWinEventHook(
                EVENT_SYSTEM_DESKTOPSWITCH,
                EVENT_SYSTEM_DESKTOPSWITCH,
                None,
                Some(on_desktop_switch),
                0,
                0,
                WINEVENT_OUTOFCONTEXT,
            )
        };
        let desktop = (!desktop.0.is_null()).then_some(desktop);
        if desktop.is_none() {
            log::warn!("hook desktop-switch watcher unavailable");
        }
        Self { sink, desktop }
    }

    fn stop(self) {
        // SAFETY: 都由本线程创建 / 注册，只释放一次
        unsafe {
            if let Some(h) = self.desktop {
                let _ = UnhookWinEvent(h);
            }
            if let Some(w) = self.sink {
                let _ = DestroyWindow(w);
            }
        }
    }
}

unsafe extern "system" fn sink_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_POWERBROADCAST {
        if matches!(wparam.0 as u32, PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND) {
            post_rearm(Rearm::Resume);
        }
        // TRUE：同意（对查询类事件）
        return LRESULT(1);
    }
    // SAFETY: 其余消息交给默认处理
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

unsafe extern "system" fn on_desktop_switch(
    _hook: HWINEVENTHOOK,
    event: u32,
    _hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if event == EVENT_SYSTEM_DESKTOPSWITCH {
        post_rearm(Rearm::DesktopSwitch);
    }
}

fn hook_main(mut state: HookState, ready: mpsc::SyncSender<Result<u32, String>>) {
    // 钩子给出的坐标是物理像素；存活检查的 GetCursorPos 与补发单击的虚拟桌面范围也按物理像素取
    let _dpi = DpiScope::per_monitor();
    wnd::ensure_message_queue();
    // SAFETY: 纯查询
    let tid = unsafe { GetCurrentThreadId() };
    if let Err(e) = state.apply() {
        state.uninstall();
        let _ = ready.send(Err(e));
        return;
    }
    let watchers = Watchers::start();
    STATE.with(|s| *s.borrow_mut() = Some(state));
    let _ = ready.send(Ok(tid));
    wnd::run_message_loop(|msg| {
        STATE.with(|s| match s.try_borrow_mut() {
            Ok(mut g) => g.as_mut().is_some_and(|st| st.on_thread_msg(msg)),
            Err(_) => false,
        })
    });
    watchers.stop();
    STATE.with(|s| {
        if let Some(mut st) = s.borrow_mut().take() {
            st.uninstall();
            #[cfg(feature = "mouse-hook")]
            st.ext.shutdown();
        }
    });
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: HC_ACTION 时 lparam 指向系统提供的 MSLLHOOKSTRUCT，回调期间有效
        let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        // panic 不能越过 extern "system" 边界：兜住后放行该事件
        let swallow = call_guarded("mouse hook", || {
            STATE.with(|s| match s.try_borrow_mut() {
                Ok(mut g) => g.as_mut().is_some_and(|st| st.on_mouse(wparam.0 as u32, info)),
                Err(_) => false,
            })
        })
        .unwrap_or(false);
        if swallow {
            return LRESULT(1);
        }
    }
    // SAFETY: 按约定传给下一个钩子
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: HC_ACTION 时 lparam 指向系统提供的 KBDLLHOOKSTRUCT，回调期间有效
        let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        call_guarded("keyboard hook", || {
            STATE.with(|s| {
                if let Ok(mut g) = s.try_borrow_mut()
                    && let Some(st) = g.as_mut()
                {
                    st.on_key(wparam.0 as u32, kb);
                }
            })
        });
    }
    // SAFETY: 按约定传给下一个钩子（键盘钩子从不吞键）
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// 鼠标消息 → (按下 / 抬起, 按键)
fn click_of(msg: u32, mouse_data: u32) -> Option<(ClickKind, PointerButton)> {
    let x = || match (mouse_data >> 16) as u16 {
        XBUTTON1 => PointerButton::Back,
        XBUTTON2 => PointerButton::Forward,
        _ => PointerButton::Other,
    };
    Some(match msg {
        WM_LBUTTONDOWN => (ClickKind::Down, PointerButton::Left),
        WM_LBUTTONUP => (ClickKind::Up, PointerButton::Left),
        WM_RBUTTONDOWN => (ClickKind::Down, PointerButton::Right),
        WM_RBUTTONUP => (ClickKind::Up, PointerButton::Right),
        WM_MBUTTONDOWN => (ClickKind::Down, PointerButton::Middle),
        WM_MBUTTONUP => (ClickKind::Up, PointerButton::Middle),
        WM_XBUTTONDOWN => (ClickKind::Down, x()),
        WM_XBUTTONUP => (ClickKind::Up, x()),
        _ => return None,
    })
}

#[cfg(feature = "mouse-hook")]
mod extended;
