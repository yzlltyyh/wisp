//! X11 后端核心：惰性建立的独立连接、扩展探测，以及唯一的事件线程（hub）。
//!
//! 设计要点：
//! - 整个平台层只用**一条** x11rb 纯 Rust 连接（不依赖 libxcb / libxdo），与 GTK / Tauri 的连接互不干扰；
//! - 只有**一个**事件线程 `wisp-x11`，阻塞在 `wait_for_event` 上——空闲时零唤醒、零 CPU。所有异步事件（选区转换、
//!   INCR、XFixes 剪贴板变更、XInput2 原始输入、RandR、XSETTINGS、`_NET_ACTIVE_WINDOW`、全局快捷键的
//!   `XGrabKey` 按键）都在这里分发；
//! - 请求可以从任何线程发出（RustConnection 是 Sync）；需要等待对端应用的操作（选区）通过 channel + 超时等待，
//!   绝不无限阻塞。选区读取按请求方窗口分发，多个读取（剪贴板历史、取词、快照）可以并发，互不阻塞；
//! - 外部回调（剪贴板监听、外部点击、钩子 sink、XSETTINGS 变化）**不在事件线程上调用**：事件线程只把它们投递到
//!   惰性创建的 `wisp-x11-cb` 线程按顺序执行（包在 `catch_unwind` 里），回调即使阻塞、甚至同步取词，也不会拖住
//!   SelectionNotify 等事件的分发；浮标点击在独立的短命线程上调用；
//! - 监听与钩子的注册、剪贴板变更序号与抑制区间保存在跨连接的 [`Registry`] 里：X 连接断开（例如 XWayland 崩溃）
//!   后，下一次调用按 5 s 节流重连，新连接自动接管全部注册，序号接着递增（保持单调）。

pub(crate) mod atoms;
pub(crate) mod clipboard;
pub(crate) mod focus;
pub(crate) mod hotkeys;
pub(crate) mod input;
pub(crate) mod keyboard;
pub(crate) mod owner;
pub(crate) mod props;
pub(crate) mod screen;
pub(crate) mod transfer;

#[cfg(feature = "mouse-hook")]
pub(crate) mod bobber;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex as StdMutex, PoisonError};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use x11rb::NONE;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask, Mapping, PropMode,
    Property, Visualid, Window, WindowClass,
};
use x11rb::protocol::{Event, randr, xfixes, xinput, xtest};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use self::atoms::Atoms;
use crate::linux::primary_gate::{PrimaryGate, Verdict};

/// 连接失败后再次尝试的最小间隔
const RECONNECT_THROTTLE: Duration = Duration::from_secs(5);

/// X 服务器支持的扩展
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Extensions {
    pub xfixes: bool,
    pub xtest: bool,
    /// XInput ≥ 2.0（原始事件）；值为 (major, minor)
    pub xinput: Option<(u16, u16)>,
    /// RandR ≥ 1.3（GetScreenResourcesCurrent / GetOutputPrimary；按输出枚举显示器，与 wisp-capture 一致）
    pub randr: bool,
    pub shape: bool,
}

type Job = Box<dyn FnOnce() + Send>;

/// 外部回调的执行线程（第一次需要时创建，空闲时阻塞在 channel 上，零 CPU）
#[derive(Default)]
struct Callbacks {
    tx: Mutex<Option<mpsc::Sender<Job>>>,
}

impl Callbacks {
    fn post(&self, job: Job) {
        let mut tx = self.tx.lock();
        let job = match tx.as_ref() {
            Some(sender) => match sender.send(job) {
                Ok(()) => return,
                // 线程已经退出（不应发生：每个任务都包在 catch_unwind 里）：重新创建
                Err(mpsc::SendError(job)) => job,
            },
            None => job,
        };
        let (sender, rx) = mpsc::channel::<Job>();
        let spawned = std::thread::Builder::new().name("wisp-x11-cb".into()).spawn(move || {
            for job in rx {
                job();
            }
        });
        match spawned {
            Ok(_) => {
                let _ = sender.send(job);
                *tx = Some(sender);
            }
            Err(e) => {
                // 极端情况（线程数耗尽）：就地执行，至少不丢事件
                log::warn!("创建回调线程失败：{e}");
                drop(tx);
                job();
            }
        }
    }
}

/// 跨连接保存的注册信息：断线重连后由新连接接管（剪贴板监听、外部点击、钩子、XSETTINGS 监听、自身窗口命中测试，
/// 以及剪贴板变更序号与抑制区间）
pub(crate) struct Registry {
    self_hit: Option<crate::SelfHitTest>,
    next_id: AtomicU64,
    /// 剪贴板变更序号与抑制区间（每条连接共享同一份，`change_id` 跨重连单调）
    pub(crate) clip: Arc<clipboard::ClipState>,
    pub(crate) clip_watchers: Mutex<Vec<(u64, clipboard::ClipWatcher)>>,
    pub(crate) clicks: Mutex<Vec<(u64, input::ClickCb)>>,
    #[cfg(feature = "mouse-hook")]
    pub(crate) hooks: Mutex<Vec<input::HookEntry>>,
    pub(crate) settings_listeners: Mutex<Vec<(u64, screen::SettingsListener)>>,
    /// 全局快捷键的注册与回调
    pub(crate) hotkeys: hotkeys::Registered,
    callbacks: Callbacks,
}

impl Registry {
    fn new(self_hit: Option<crate::SelfHitTest>) -> Self {
        Self {
            self_hit,
            next_id: AtomicU64::new(1),
            clip: Arc::new(clipboard::ClipState::default()),
            clip_watchers: Mutex::new(Vec::new()),
            clicks: Mutex::new(Vec::new()),
            #[cfg(feature = "mouse-hook")]
            hooks: Mutex::new(Vec::new()),
            settings_listeners: Mutex::new(Vec::new()),
            hotkeys: hotkeys::Registered::default(),
            callbacks: Callbacks::default(),
        }
    }

    /// 新的注册 id
    pub(crate) fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// 自身窗口命中测试（浮标判定）
    #[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
    pub(crate) fn self_hit(&self) -> Option<&crate::SelfHitTest> {
        self.self_hit.as_ref()
    }

    /// 把外部回调投递到回调线程（按投递顺序执行；panic 只记日志）
    pub(crate) fn post(&self, what: &'static str, f: impl FnOnce() + Send + 'static) {
        self.callbacks.post(Box::new(move || call_guarded(what, f)));
    }
}

/// PRIMARY 的占有记录（浮标判定，见 `primary_gate`）：hub 线程更新，取词线程带超时等待
#[derive(Default)]
pub(crate) struct PrimaryWatch {
    gate: StdMutex<PrimaryGate>,
    changed: Condvar,
}

impl PrimaryWatch {
    /// 修改记录并唤醒等待者
    pub(crate) fn update(&self, f: impl FnOnce(&mut PrimaryGate)) {
        let mut g = self.gate.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut g);
        self.changed.notify_all();
    }

    /// 等到判定不再是 Stale（应用可能在 mouse-up 之后才占有 PRIMARY）或到达 `deadline`；事件驱动，不轮询
    pub(crate) fn wait_verdict(&self, deadline: Instant) -> Verdict {
        let mut g = self.gate.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            let v = g.verdict();
            let now = Instant::now();
            if v != Verdict::Stale || now >= deadline {
                return v;
            }
            g = self
                .changed
                .wait_timeout(g, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn wake(&self) {
        self.changed.notify_all();
    }
}

/// 一条 X11 连接及其全部状态
pub(crate) struct X11 {
    pub conn: RustConnection,
    pub screen: usize,
    pub root: Window,
    #[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
    pub root_visual: Visualid,
    #[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
    pub root_depth: u8,
    /// 隐藏的辅助窗口：选区 owner、XFixes 订阅、时间戳探测（选区读取用 `transfer` 的请求方窗口池）
    pub win: Window,
    pub atoms: Atoms,
    pub ext: Extensions,
    pub own_pid: u32,
    /// 跨连接的注册信息
    pub(crate) reg: Arc<Registry>,
    alive: AtomicBool,
    dynamic_atoms: Mutex<HashMap<String, Atom>>,
    /// 原子 → 名字的缓存（原子在服务器生命周期内不变；只缓存见过的 TARGETS 等，几百个以内）
    atom_names: Mutex<HashMap<Atom, String>>,
    time_waiters: Mutex<Vec<mpsc::Sender<u32>>>,
    pub(crate) reads: transfer::Router,
    pub(crate) owner: owner::Owner,
    /// `Registry::clip` 的克隆
    pub(crate) clip: Arc<clipboard::ClipState>,
    pub(crate) primary: PrimaryWatch,
    pub(crate) keyboard: keyboard::Keyboard,
    pub(crate) focus: focus::FocusState,
    pub(crate) screen_state: screen::ScreenState,
    pub(crate) input: input::InputState,
    #[cfg(feature = "mouse-hook")]
    pub(crate) bobber: bobber::BobberState,
}

impl X11 {
    /// 建立连接、探测扩展、创建辅助窗口并启动事件线程。`reconnect`：之前有过连接（断线重连）
    fn connect(own_pid: u32, reg: Arc<Registry>, reconnect: bool) -> Result<Arc<X11>, String> {
        let started = Instant::now();
        let (conn, screen) = RustConnection::connect(None).map_err(|e| format!("connect: {e}"))?;
        let setup = conn.setup();
        let scr = setup.roots.get(screen).ok_or("invalid screen number")?;
        let (root, root_visual, root_depth) = (scr.root, scr.root_visual, scr.root_depth);
        let atoms = Atoms::new(&conn)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        let ext = probe_extensions(&conn);
        let win = conn.generate_id().map_err(|e| e.to_string())?;
        conn.create_window(
            0,
            win,
            root,
            -10,
            -10,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new()
                .override_redirect(1)
                .event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(|e| e.to_string())?
        .check()
        .map_err(|e| format!("create helper window: {e}"))?;
        // 便于在 xprop / 调试工具中识别
        let _ = conn.change_property8(
            PropMode::REPLACE,
            win,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            b"wisp-platform",
        );
        let _ = conn.change_property32(
            PropMode::REPLACE,
            win,
            atoms._NET_WM_PID,
            AtomEnum::CARDINAL,
            &[own_pid],
        );
        let clip = Arc::clone(&reg.clip);
        let x = Arc::new(X11 {
            conn,
            screen,
            root,
            root_visual,
            root_depth,
            win,
            atoms,
            ext,
            own_pid,
            reg,
            alive: AtomicBool::new(true),
            dynamic_atoms: Mutex::new(HashMap::new()),
            atom_names: Mutex::new(HashMap::new()),
            time_waiters: Mutex::new(Vec::new()),
            reads: transfer::Router::default(),
            owner: owner::Owner::default(),
            clip,
            primary: PrimaryWatch::default(),
            keyboard: keyboard::Keyboard::default(),
            focus: focus::FocusState::default(),
            screen_state: screen::ScreenState::default(),
            input: input::InputState::default(),
            #[cfg(feature = "mouse-hook")]
            bobber: bobber::BobberState::default(),
        });
        x.subscribe_base_events();
        if reconnect {
            x.clip.on_reconnect();
        }
        focus::prime(&x);
        let hub = Arc::clone(&x);
        std::thread::Builder::new()
            .name("wisp-x11".into())
            .spawn(move || run_hub(&hub))
            .map_err(|e| format!("spawn hub: {e}"))?;
        // 接管已有的注册（重连时：外部点击、钩子的 XI2 掩码与侧键抓取、全局快捷键的抓取）
        input::sync(&x);
        hotkeys::sync(&x, hotkeys::Resync::Reconnect);
        log::info!(
            "X11 已连接（{} ms）：xfixes={} xtest={} xinput={:?} randr1.3={} shape={}",
            started.elapsed().as_millis(),
            ext.xfixes,
            ext.xtest,
            ext.xinput,
            ext.randr,
            ext.shape
        );
        Ok(x)
    }

    /// 常驻订阅：根窗口属性（前台窗口、工作区）、剪贴板 owner 变更、显示器变化、XSETTINGS。
    /// 这些事件只在用户操作时产生，空闲时没有任何唤醒
    fn subscribe_base_events(&self) {
        let _ = self.conn.change_window_attributes(
            self.root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        );
        if self.ext.xfixes {
            let mask = xfixes::SelectionEventMask::SET_SELECTION_OWNER
                | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
                | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE;
            let _ = xfixes::select_selection_input(&self.conn, self.win, self.atoms.CLIPBOARD, mask);
        }
        if self.ext.randr {
            let mask =
                randr::NotifyMask::SCREEN_CHANGE | randr::NotifyMask::CRTC_CHANGE | randr::NotifyMask::OUTPUT_CHANGE;
            let _ = randr::select_input(&self.conn, self.root, mask);
        }
        screen::attach_xsettings(self);
        props::flush(self);
    }

    /// 连接是否仍然可用
    pub(crate) fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// 窗口是否由本连接创建（XID 落在本客户端的资源 id 区间里）
    pub(crate) fn is_own_window(&self, w: Window) -> bool {
        let setup = self.conn.setup();
        w != NONE && (w & !setup.resource_id_mask) == setup.resource_id_base
    }

    /// 动态 intern 原子（带缓存）
    pub(crate) fn intern(&self, name: &str) -> Option<Atom> {
        if let Some(&a) = self.dynamic_atoms.lock().get(name) {
            return Some(a);
        }
        let atom = self.conn.intern_atom(false, name.as_bytes()).ok()?.reply().ok()?.atom;
        self.dynamic_atoms.lock().insert(name.to_owned(), atom);
        Some(atom)
    }

    /// 只查询已存在的原子（不存在时不创建）
    pub(crate) fn existing_atom(&self, name: &str) -> Option<Atom> {
        if let Some(&a) = self.dynamic_atoms.lock().get(name) {
            return Some(a);
        }
        let atom = self.conn.intern_atom(true, name.as_bytes()).ok()?.reply().ok()?.atom;
        (atom != u32::from(AtomEnum::NONE)).then_some(atom)
    }

    /// 批量取原子名：未缓存的一次性流水线发出（总共一次往返的延迟），结果缓存。取不到的为 None
    pub(crate) fn atom_names(&self, atoms: &[Atom]) -> Vec<Option<String>> {
        let mut out: Vec<Option<String>> = {
            let cache = self.atom_names.lock();
            atoms.iter().map(|a| cache.get(a).cloned()).collect()
        };
        let cookies: Vec<_> = atoms
            .iter()
            .zip(&out)
            .map(|(&a, known)| {
                if known.is_none() {
                    self.conn.get_atom_name(a).ok()
                } else {
                    None
                }
            })
            .collect();
        for ((slot, cookie), &atom) in out.iter_mut().zip(cookies).zip(atoms) {
            if let Some(reply) = cookie.and_then(|c| c.reply().ok()) {
                let name = String::from_utf8_lossy(&reply.name).into_owned();
                self.atom_names.lock().insert(atom, name.clone());
                *slot = Some(name);
            }
        }
        out
    }

    /// 取得当前服务器时间（ICCCM 要求设置选区 owner 时不要用 CurrentTime）：向辅助窗口追加一个零长度属性，
    /// 由 hub 线程把 PropertyNotify 里的时间戳送回来
    pub(crate) fn server_time(&self, timeout: Duration) -> Option<u32> {
        let (tx, rx) = mpsc::channel();
        self.time_waiters.lock().push(tx);
        self.conn
            .change_property8(PropMode::APPEND, self.win, self.atoms.WISP_TIME, AtomEnum::STRING, &[])
            .ok()?;
        self.conn.flush().ok()?;
        rx.recv_timeout(timeout).ok()
    }

    /// 屏幕的根窗口信息
    pub(crate) fn screen_info(&self) -> Option<&x11rb::protocol::xproto::Screen> {
        self.conn.setup().roots.get(self.screen)
    }

    fn dispatch(self: &Arc<Self>, ev: Event) {
        match ev {
            Event::SelectionNotify(e) => self.reads.deliver_notify(e),
            Event::PropertyNotify(e) => {
                if e.state == Property::DELETE {
                    // 作为 owner 的 INCR 发送：请求方删除属性 = 要下一块（不论是哪个窗口）
                    owner::on_property_deleted(self, e.window, e.atom);
                }
                if e.window == self.win {
                    if e.atom == self.atoms.WISP_TIME {
                        for tx in self.time_waiters.lock().drain(..) {
                            let _ = tx.send(e.time);
                        }
                    }
                    return;
                }
                if self
                    .reads
                    .deliver_property(e.window, e.atom, e.state == Property::NEW_VALUE)
                {
                    return;
                }
                if e.window == self.root {
                    focus::on_root_property(self, e.atom);
                    screen::on_root_property(self, e.atom);
                } else {
                    screen::on_property(self, e.window, e.atom);
                }
            }
            Event::SelectionRequest(e) => owner::handle_request(self, &e),
            Event::SelectionClear(e) => owner::on_clear(self, &e),
            Event::XfixesSelectionNotify(e) => {
                if e.selection == self.atoms.CLIPBOARD {
                    clipboard::on_owner_change(self, &e);
                } else if e.selection == u32::from(AtomEnum::PRIMARY) {
                    self.on_primary_owner(&e);
                } else {
                    screen::on_selection_owner(self, e.selection);
                }
            }
            Event::DestroyNotify(e) => {
                owner::on_destroy(self, e.window);
                screen::on_destroy(self, e.window);
            }
            Event::XinputRawButtonPress(e) => input::on_raw_button(self, e.detail, true, e.sourceid, e.time),
            Event::XinputRawButtonRelease(e) => input::on_raw_button(self, e.detail, false, e.sourceid, e.time),
            Event::XinputRawKeyPress(e) => {
                let repeat = e.flags.contains(xinput::KeyEventFlags::KEY_REPEAT);
                input::on_raw_key(self, e.detail, true, repeat, e.sourceid);
            }
            Event::XinputRawKeyRelease(e) => input::on_raw_key(self, e.detail, false, false, e.sourceid),
            Event::ButtonPress(e) => self.on_core_button(e.event, e.detail, true),
            Event::ButtonRelease(e) => self.on_core_button(e.event, e.detail, false),
            #[cfg(feature = "mouse-hook")]
            Event::EnterNotify(e) => bobber::on_crossing(self, e.event, true),
            #[cfg(feature = "mouse-hook")]
            Event::LeaveNotify(e) => bobber::on_crossing(self, e.event, false),
            #[cfg(feature = "mouse-hook")]
            Event::Expose(e) if e.count == 0 => bobber::on_expose(self, e.window),
            Event::KeyPress(e) => hotkeys::on_press(self, e.detail, u16::from(e.state)),
            Event::KeyRelease(e) => self.on_key_release(e.detail, e.time),
            Event::MappingNotify(e) => {
                self.keyboard.invalidate();
                if e.request == Mapping::KEYBOARD {
                    hotkeys::on_mapping(self);
                }
            }
            Event::RandrScreenChangeNotify(_) | Event::RandrNotify(_) => self.screen_state.invalidate_monitors(),
            Event::Error(e) => log::debug!("X11 错误：{e:?}"),
            _ => {}
        }
    }

    /// 全局快捷键的抬起：没有可检测自动重复时，按住产生的 Release + Press 对整对丢弃。
    /// - 快速路径：紧跟着的事件（已经读到的，或非阻塞地再读一次）是同一 keycode、时间戳相同或差 1 ms 的按下；
    /// - 兜底：这一对被拆在两次读取里时（服务器或本进程正忙），查一次按键状态（`QueryKeymap`，一次往返，只发生在已
    ///   注册组合的抬起上）——键仍按着就是自动重复：这次抬起丢弃，随后到达的按下由按住状态吞掉（`GrabTable::press`）。
    ///   用户在这一次往返（亚毫秒）之内松开又按下同一个键的情况不存在
    fn on_key_release(self: &Arc<Self>, keycode: u8, time: u32) {
        if !hotkeys::is_grabbed(self, keycode) {
            return;
        }
        let next = self.conn.poll_for_event().ok().flatten();
        let next_press = match &next {
            Some(Event::KeyPress(p)) => Some((p.detail, p.time)),
            _ => None,
        };
        if crate::linux::hotkey_logic::is_autorepeat((keycode, time), next_press) {
            return;
        }
        if !self.key_still_down(keycode) {
            hotkeys::on_release(self, keycode);
        }
        if let Some(ev) = next {
            self.dispatch(ev);
        }
    }

    /// 这个键此刻是否仍按着（`QueryKeymap`；查询失败时按已松开处理，宁可多一次抬起也不让按住状态卡住）
    fn key_still_down(&self, keycode: u8) -> bool {
        self.conn
            .query_keymap()
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|r| crate::linux::hotkey_logic::key_down(&r.keys, keycode))
    }

    /// XFixes：PRIMARY 被占有（只在安装了浮标钩子时订阅）。Wisp 自己占有（往 xterm 粘贴）不算用户选区，
    /// 只用来补上占有时没能取到的时间戳
    fn on_primary_owner(&self, e: &xfixes::SelectionNotifyEvent) {
        if e.subtype != xfixes::SelectionEvent::SET_SELECTION_OWNER || e.owner == NONE {
            return;
        }
        if e.owner == self.win {
            owner::learn_stamp(self, u32::from(AtomEnum::PRIMARY), e.selection_timestamp);
        } else {
            self.primary.update(|g| g.on_claim(e.timestamp));
        }
    }

    fn on_core_button(self: &Arc<Self>, window: Window, button: u8, press: bool) {
        #[cfg(feature = "mouse-hook")]
        {
            if bobber::on_button(self, window, button, press) {
                return;
            }
            if window == self.root {
                input::on_grabbed_button(self, button, press);
            }
        }
        #[cfg(not(feature = "mouse-hook"))]
        let _ = (window, button, press);
    }
}

fn run_hub(x: &Arc<X11>) {
    loop {
        match x.conn.wait_for_event() {
            Ok(ev) => {
                // 事件处理本身不应 panic；即便发生也只丢弃这一个事件，事件线程继续运行
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| x.dispatch(ev)));
                if r.is_err() {
                    log::error!("X11 事件处理发生 panic，已忽略该事件");
                }
            }
            Err(e) => {
                log::warn!("X11 连接已断开：{e}");
                x.alive.store(false, Ordering::Release);
                x.reads.close();
                x.time_waiters.lock().clear();
                x.clip.wake_all();
                x.primary.wake();
                break;
            }
        }
    }
}

fn probe_extensions(conn: &RustConnection) -> Extensions {
    let present = |name: &'static str| matches!(conn.extension_information(name), Ok(Some(_)));
    let xfixes = present(xfixes::X11_EXTENSION_NAME)
        && xfixes::query_version(conn, 5, 0)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|r| r.major_version >= 1);
    let xtest = present(xtest::X11_EXTENSION_NAME)
        && xtest::get_version(conn, 2, 2)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some();
    let xinput = if present(xinput::X11_EXTENSION_NAME) {
        xinput::xi_query_version(conn, 2, 2)
            .ok()
            .and_then(|c| c.reply().ok())
            .filter(|r| r.major_version >= 2)
            .map(|r| (r.major_version, r.minor_version))
    } else {
        None
    };
    let randr = present(randr::X11_EXTENSION_NAME)
        && randr::query_version(conn, 1, 5)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|r| (r.major_version, r.minor_version) >= (1, 3));
    let shape = present(x11rb::protocol::shape::X11_EXTENSION_NAME);
    Extensions {
        xfixes,
        xtest,
        xinput,
        randr,
        shape,
    }
}

/// 惰性建立、断线后可重连的 X11 连接。第一次真正需要 X11 的调用才连接（几毫秒）；连接失败后 5 s 内不重试，
/// 避免日志刷屏。注册信息（[`Registry`]）独立于连接保存，重连后由新连接接管
pub(crate) struct LazyX11 {
    own_pid: u32,
    enabled: bool,
    reg: Arc<Registry>,
    /// 当前连接（可能已断开；只在 `get` 里替换）
    slot: Mutex<Option<Arc<X11>>>,
    /// 串行化连接尝试（连接期间持有；`slot` / `failure` 只短暂加锁，`info` 读取它们不会被连接过程阻塞）
    connecting: Mutex<()>,
    failure: Mutex<Option<(Instant, String)>>,
}

impl LazyX11 {
    /// `enabled = false`（没有 DISPLAY）时永远返回错误，不尝试连接
    pub(crate) fn new(own_pid: u32, enabled: bool, self_hit: Option<crate::SelfHitTest>) -> Self {
        Self {
            own_pid,
            enabled,
            reg: Arc::new(Registry::new(self_hit)),
            slot: Mutex::new(None),
            connecting: Mutex::new(()),
            failure: Mutex::new(None),
        }
    }

    /// 注册信息（不需要连接）
    pub(crate) fn registry(&self) -> &Arc<Registry> {
        &self.reg
    }

    fn live(&self) -> Option<Arc<X11>> {
        self.slot.lock().as_ref().filter(|x| x.is_alive()).cloned()
    }

    /// 取得连接（必要时建立；断开后重连）
    pub(crate) fn get(&self) -> Result<Arc<X11>, String> {
        if let Some(x) = self.live() {
            return Ok(x);
        }
        if !self.enabled {
            return Err("no X display".to_owned());
        }
        let _connecting = self.connecting.lock();
        if let Some(x) = self.live() {
            return Ok(x);
        }
        if let Some((at, err)) = self.failure.lock().clone()
            && at.elapsed() < RECONNECT_THROTTLE
        {
            return Err(err);
        }
        let had_connection = self.slot.lock().take().is_some();
        if had_connection {
            log::info!("X11 连接已断开，正在重新连接");
        }
        let reconnect = had_connection || self.failure.lock().is_some();
        match X11::connect(self.own_pid, Arc::clone(&self.reg), reconnect) {
            Ok(x) => {
                self.failure.lock().take();
                *self.slot.lock() = Some(Arc::clone(&x));
                Ok(x)
            }
            Err(e) => {
                log::warn!("连接 X 服务器失败：{e}");
                *self.failure.lock() = Some((Instant::now(), e.clone()));
                Err(e)
            }
        }
    }

    /// 只取已建立且仍然可用的连接（不阻塞、不连接）
    pub(crate) fn peek(&self) -> Option<Arc<X11>> {
        self.live()
    }

    /// 当前是否处于失败状态（连接失败过、或已建立的连接断开了）；`info` 用来报告原因，不阻塞
    pub(crate) fn failed(&self) -> bool {
        if let Some(x) = self.slot.lock().as_ref() {
            return !x.is_alive();
        }
        self.failure.lock().is_some()
    }

    /// 后台预热：在独立线程里建立连接（`Platform::create` 不阻塞）
    pub(crate) fn warm_up(self: &Arc<Self>) {
        if !self.enabled || self.live().is_some() {
            return;
        }
        let me = Arc::clone(self);
        let _ = std::thread::Builder::new().name("wisp-x11-init".into()).spawn(move || {
            let _ = me.get();
        });
    }
}

/// 安全地调用外部回调：panic 只记日志
pub(crate) fn call_guarded(what: &str, f: impl FnOnce()) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err() {
        log::error!("{what} 回调发生 panic");
    }
}
