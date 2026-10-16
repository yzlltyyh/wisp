//! Linux 平台测试。
//!
//! 1. 纯逻辑模块（会话检测与各功能的降级路由、XSETTINGS、外观判定、D-Bus 编解码、`.desktop` 索引、EDID、几何换算、
//!    keysym 与键盘映射表、终端识别、文件 URI、浮标光栅化、选区读取 / INCR 状态机与 owner 时间校验、浮标的 PRIMARY
//!    新鲜度判定、XI2 钩子判定（连击 / 位移 / 探测取消 / 修饰键 / 侧键长按）、模拟复制回退与快照计划）只依赖
//!    std / wisp-types / `common/` 的公开类型，这里用 `#[path]` 直接引入并运行它们的单测——因此在 macOS 开发机上
//!    `cargo test -p wisp-platform --test smoke_linux` 也能覆盖这些逻辑（Linux CI 上它们还会在 lib 单测里各跑一遍）；
//! 2. 依赖 X 服务器的用例标 `#[ignore = "需要 X11 会话"]`。它们不需要人工操作：选区的另一方由测试里的独立 x11rb
//!    客户端扮演（owner 与读取方都支持 INCR），所以在 CI 的 Xvfb 下也能跑：
//!    `xvfb-run -a cargo test -p wisp-platform --test smoke_linux -- --ignored --test-threads=1`。

/// `#[path]` 引入的模块经 `crate::common::…` 引用 `common/` 的公开类型（与它们在 lib 里的路径一致）
#[cfg(unix)]
#[allow(unused_imports)]
mod common {
    pub use wisp_platform::common::{bobber_glyph, combo, double_tap, gesture, secrets_markers};
}

/// 同上：`session.rs` 经 `crate::reason` 引用三端共用的原因 key
#[cfg(unix)]
#[allow(unused_imports)]
pub mod reason {
    pub use wisp_platform::reason::*;
}

#[cfg(unix)]
#[allow(dead_code)]
#[path = "../src/linux/appearance_logic.rs"]
mod appearance_logic;
#[cfg(all(unix, feature = "mouse-hook"))]
#[allow(dead_code)]
#[path = "../src/linux/bobber_art.rs"]
mod bobber_art;
#[cfg(unix)]
#[allow(dead_code)]
#[path = "../src/linux/dbus.rs"]
mod dbus;
#[cfg(unix)]
#[allow(dead_code)]
#[path = "../src/linux/dbus_wire.rs"]
mod dbus_wire;
#[cfg(unix)]
#[allow(dead_code)]
#[path = "../src/linux/desktop_entry.rs"]
mod desktop_entry;
#[cfg(unix)]
#[allow(dead_code)]
#[path = "../src/linux/edid.rs"]
mod edid;
#[cfg(unix)]
#[allow(dead_code)]
#[path = "../src/linux/fallback_logic.rs"]
mod fallback_logic;
#[cfg(unix)]
#[allow(dead_code)]
#[path = "../src/linux/geometry.rs"]
mod geometry;
#[cfg(all(unix, feature = "mouse-hook"))]
#[allow(dead_code)]
#[path = "../src/linux/hook_logic.rs"]
mod hook_logic;
#[cfg(unix)]
#[allow(dead_code)]
#[path = "../src/linux/hotkey_logic.rs"]
mod hotkey_logic;
#[cfg(unix)]
#[allow(dead_code)]
#[path = "../src/linux/keysym.rs"]
mod keysym;
#[cfg(unix)]
#[allow(dead_code)]
#[path = "../src/linux/primary_gate.rs"]
mod primary_gate;
#[cfg(unix)]
#[allow(dead_code)]
#[path = "../src/linux/session.rs"]
mod session;
#[cfg(unix)]
#[allow(dead_code)]
#[path = "../src/linux/terminal.rs"]
mod terminal;
#[cfg(unix)]
#[allow(dead_code)]
#[path = "../src/linux/uri.rs"]
mod uri;
#[cfg(unix)]
#[allow(dead_code)]
#[path = "../src/linux/xfer_logic.rs"]
mod xfer_logic;
#[cfg(unix)]
#[allow(dead_code)]
#[path = "../src/linux/xsettings.rs"]
mod xsettings;

/// 通过公开 API 的冒烟测试（只在 Linux 上编译）
#[cfg(target_os = "linux")]
mod live {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use wisp_platform::{Platform, PlatformConfig, ReadLimits, SelectionError, SelectionOpts, WriteOpts};
    use wisp_types::{ClipPayload, OsKind, PermissionState, SessionKind};

    fn platform() -> Arc<Platform> {
        Platform::create(PlatformConfig {
            own_pid: std::process::id(),
            main_thread: Arc::new(|f: Box<dyn FnOnce() + Send>| f()),
            self_hit: Arc::new(|_, _| false),
            keymaps: Arc::new(parking_lot::RwLock::new(Vec::new())),
            os_version: "test".to_owned(),
        })
    }

    fn has_x11() -> bool {
        std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty())
            && std::env::var_os("WAYLAND_DISPLAY").is_none_or(|v| v.is_empty())
    }

    /// 可预测、可校验的大段文本（`n` 字节，ASCII）
    fn big_text(n: usize) -> String {
        let line = "wisp-incr 0123456789 abcdefghijklmnopqrstuvwxyz\n";
        line.repeat(n / line.len() + 1)[..n].to_owned()
    }

    /// 测试里扮演「另一个应用」的独立 X 客户端（与 Wisp 的连接无关）
    #[cfg(test)]
    mod peer {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;
        use std::thread::JoinHandle;
        use std::time::{Duration, Instant};

        use wisp_types::AppRef;
        use x11rb::connection::{Connection, RequestConnection as _};
        use x11rb::protocol::Event;
        use x11rb::protocol::randr::{self, ConnectionExt as _};
        use x11rb::protocol::xproto::{
            Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask, InputFocus,
            PropMode, Property, SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, SelectionRequestEvent, Window,
            WindowClass,
        };
        use x11rb::rust_connection::RustConnection;
        use x11rb::wrapper::ConnectionExt as _;
        use x11rb::{CURRENT_TIME, NONE};

        /// 超过它就用 INCR 分块（故意比 Wisp 的 256 KB 小，让中等大小的数据也走 INCR）
        const CHUNK: usize = 64 * 1024;

        fn atom(conn: &RustConnection, name: &str) -> Atom {
            conn.intern_atom(false, name.as_bytes()).unwrap().reply().unwrap().atom
        }

        fn window(conn: &RustConnection, screen: usize) -> Window {
            let root = conn.setup().roots[screen].root;
            let win = conn.generate_id().unwrap();
            conn.create_window(
                0,
                win,
                root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )
            .unwrap()
            .check()
            .unwrap();
            win
        }

        /// 选区 owner：提供 TARGETS 与 UTF8_STRING；数据超过 64 KB 时用 INCR，每块之间可以人为延迟
        pub struct Owner {
            stop: Arc<AtomicBool>,
            handle: Option<JoinHandle<()>>,
        }

        impl Owner {
            pub fn start(selection: &'static str, data: Vec<u8>, chunk_delay: Duration) -> Owner {
                let stop = Arc::new(AtomicBool::new(false));
                let (ready_tx, ready_rx) = mpsc::channel();
                let s2 = Arc::clone(&stop);
                let handle = std::thread::spawn(move || serve(selection, &data, chunk_delay, &s2, &ready_tx));
                ready_rx
                    .recv_timeout(Duration::from_secs(3))
                    .expect("测试 owner 没有成功占有选区");
                Owner {
                    stop,
                    handle: Some(handle),
                }
            }
        }

        impl Drop for Owner {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::SeqCst);
                if let Some(h) = self.handle.take() {
                    let _ = h.join();
                }
            }
        }

        fn serve(selection: &str, data: &[u8], delay: Duration, stop: &AtomicBool, ready: &mpsc::Sender<()>) {
            let (conn, screen) = RustConnection::connect(None).unwrap();
            let win = window(&conn, screen);
            let (sel, utf8, targets, incr) = (
                atom(&conn, selection),
                atom(&conn, "UTF8_STRING"),
                atom(&conn, "TARGETS"),
                atom(&conn, "INCR"),
            );
            conn.set_selection_owner(win, sel, CURRENT_TIME).unwrap();
            assert_eq!(conn.get_selection_owner(sel).unwrap().reply().unwrap().owner, win);
            ready.send(()).unwrap();
            // 进行中的 INCR：(请求方, 属性, 已发送的偏移)
            let mut sending: Vec<(Window, Atom, usize)> = Vec::new();
            while !stop.load(Ordering::SeqCst) {
                let Some(ev) = conn.poll_for_event().unwrap() else {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                };
                match ev {
                    Event::SelectionRequest(e) => {
                        let prop = if e.property == NONE { e.target } else { e.property };
                        let ok = if e.target == targets {
                            conn.change_property32(
                                PropMode::REPLACE,
                                e.requestor,
                                prop,
                                AtomEnum::ATOM,
                                &[targets, utf8],
                            )
                            .unwrap();
                            true
                        } else if e.target == utf8 && data.len() > CHUNK {
                            conn.change_window_attributes(
                                e.requestor,
                                &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
                            )
                            .unwrap();
                            conn.change_property32(PropMode::REPLACE, e.requestor, prop, incr, &[data.len() as u32])
                                .unwrap();
                            sending.push((e.requestor, prop, 0));
                            true
                        } else if e.target == utf8 {
                            conn.change_property8(PropMode::REPLACE, e.requestor, prop, utf8, data)
                                .unwrap();
                            true
                        } else {
                            false
                        };
                        let notify = SelectionNotifyEvent {
                            response_type: SELECTION_NOTIFY_EVENT,
                            sequence: 0,
                            time: e.time,
                            requestor: e.requestor,
                            selection: e.selection,
                            target: e.target,
                            property: if ok { prop } else { NONE },
                        };
                        conn.send_event(false, e.requestor, EventMask::NO_EVENT, notify)
                            .unwrap();
                        conn.flush().unwrap();
                    }
                    Event::PropertyNotify(e) if e.state == Property::DELETE => {
                        if let Some(i) = sending.iter().position(|&(w, p, _)| w == e.window && p == e.atom) {
                            std::thread::sleep(delay);
                            let (w, p, off) = sending[i];
                            let end = (off + CHUNK).min(data.len());
                            conn.change_property8(PropMode::REPLACE, w, p, utf8, &data[off..end])
                                .unwrap();
                            conn.flush().unwrap();
                            if end == off {
                                sending.swap_remove(i);
                            } else {
                                sending[i].2 = end;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        /// 应答一个 SelectionRequest：TARGETS 与 UTF8_STRING（小数据，不用 INCR）；`data` 为 None 时一律拒绝
        fn answer(conn: &RustConnection, e: &SelectionRequestEvent, data: Option<&[u8]>, utf8: Atom, targets: Atom) {
            let prop = if e.property == NONE { e.target } else { e.property };
            let ok = match data {
                Some(_) if e.target == targets => {
                    conn.change_property32(PropMode::REPLACE, e.requestor, prop, AtomEnum::ATOM, &[targets, utf8])
                        .unwrap();
                    true
                }
                Some(d) if e.target == utf8 => {
                    conn.change_property8(PropMode::REPLACE, e.requestor, prop, utf8, d)
                        .unwrap();
                    true
                }
                _ => false,
            };
            notify(conn, e, if ok { prop } else { NONE });
        }

        fn notify(conn: &RustConnection, e: &SelectionRequestEvent, property: Atom) {
            let ev = SelectionNotifyEvent {
                response_type: SELECTION_NOTIFY_EVENT,
                sequence: 0,
                time: e.time,
                requestor: e.requestor,
                selection: e.selection,
                target: e.target,
                property,
            };
            conn.send_event(false, e.requestor, EventMask::NO_EVENT, ev).unwrap();
            conn.flush().unwrap();
        }

        /// 产生 `keysym` 的键码（任意列）
        fn keycode_for(conn: &RustConnection, keysym: u32) -> Option<u8> {
            let (min, max) = (conn.setup().min_keycode, conn.setup().max_keycode);
            let r = conn.get_keyboard_mapping(min, max - min + 1).ok()?.reply().ok()?;
            let per = usize::from(r.keysyms_per_keycode).max(1);
            let i = r.keysyms.chunks(per).position(|c| c.contains(&keysym))?;
            Some(min + u8::try_from(i).ok()?)
        }

        /// 让选区没有 owner（任何客户端都可以这样做）
        pub fn clear_selection(selection: &str) {
            let (conn, _) = RustConnection::connect(None).unwrap();
            let sel = atom(&conn, selection);
            conn.set_selection_owner(NONE, sel, CURRENT_TIME)
                .unwrap()
                .check()
                .unwrap();
        }

        /// 扮演「有输入焦点的普通应用」的窗口（WM_CLASS 可指定）：Ctrl+C 时以 `selected` 占有 CLIPBOARD（为 None
        /// 时什么也不做，相当于没有选中文字）；Ctrl+V 读 CLIPBOARD、Shift+Insert 读 PRIMARY，把读到的内容发到 channel
        pub struct App {
            pub window: Window,
            class: &'static str,
            instance: &'static str,
            pasted: mpsc::Receiver<Option<Vec<u8>>>,
            stop: Arc<AtomicBool>,
            handle: Option<JoinHandle<()>>,
        }

        impl App {
            pub fn start(instance: &'static str, class: &'static str, selected: Option<&'static [u8]>) -> App {
                let stop = Arc::new(AtomicBool::new(false));
                let (ready_tx, ready_rx) = mpsc::channel();
                let (paste_tx, paste_rx) = mpsc::channel();
                let s2 = Arc::clone(&stop);
                let handle = std::thread::spawn(move || run_app(instance, class, selected, &s2, &ready_tx, &paste_tx));
                let window = ready_rx
                    .recv_timeout(Duration::from_secs(3))
                    .expect("测试应用的窗口没有就绪");
                App {
                    window,
                    class,
                    instance,
                    pasted: paste_rx,
                    stop,
                    handle: Some(handle),
                }
            }

            /// 与 Wisp 记录前台应用时得到的 AppRef 形状相同（id = class 小写，alt_id = instance）
            pub fn app_ref(&self) -> AppRef {
                AppRef {
                    id: self.class.to_ascii_lowercase(),
                    alt_id: Some(self.instance.to_owned()),
                    name: self.class.to_owned(),
                    pid: None,
                    path: None,
                    window: Some(u64::from(self.window)),
                }
            }

            /// 等待一次粘贴，返回读到的内容
            pub fn wait_pasted(&self, timeout: Duration) -> Option<Vec<u8>> {
                self.pasted.recv_timeout(timeout).ok().flatten()
            }
        }

        impl Drop for App {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::SeqCst);
                if let Some(h) = self.handle.take() {
                    let _ = h.join();
                }
            }
        }

        fn run_app(
            instance: &str,
            class: &str,
            selected: Option<&'static [u8]>,
            stop: &AtomicBool,
            ready: &mpsc::Sender<Window>,
            pasted: &mpsc::Sender<Option<Vec<u8>>>,
        ) {
            let (conn, screen) = RustConnection::connect(None).unwrap();
            let root = conn.setup().roots[screen].root;
            let win = conn.generate_id().unwrap();
            conn.create_window(
                0,
                win,
                root,
                10,
                10,
                200,
                100,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new().event_mask(EventMask::KEY_PRESS | EventMask::STRUCTURE_NOTIFY),
            )
            .unwrap()
            .check()
            .unwrap();
            // 不设 _NET_WM_PID：测试进程与 Wisp 是同一个进程，设了会被当成 Wisp 自己的窗口
            let wm_class = format!("{instance}\0{class}\0");
            conn.change_property8(
                PropMode::REPLACE,
                win,
                AtomEnum::WM_CLASS,
                AtomEnum::STRING,
                wm_class.as_bytes(),
            )
            .unwrap();
            conn.map_window(win).unwrap();
            conn.flush().unwrap();
            // 等到窗口映射（没有窗口管理器时立即映射）后再设输入焦点
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                match conn.poll_for_event().unwrap() {
                    Some(Event::MapNotify(e)) if e.window == win => break,
                    Some(_) => {}
                    None => std::thread::sleep(Duration::from_millis(1)),
                }
            }
            conn.set_input_focus(InputFocus::PARENT, win, CURRENT_TIME)
                .unwrap()
                .check()
                .unwrap();
            let (kc_c, kc_v, kc_insert) = (
                keycode_for(&conn, 0x63),
                keycode_for(&conn, 0x76),
                keycode_for(&conn, 0xff63),
            );
            let (clipboard, utf8, targets) = (
                atom(&conn, "CLIPBOARD"),
                atom(&conn, "UTF8_STRING"),
                atom(&conn, "TARGETS"),
            );
            ready.send(win).unwrap();
            while !stop.load(Ordering::SeqCst) {
                let Some(ev) = conn.poll_for_event().unwrap() else {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                };
                match ev {
                    Event::KeyPress(e) => {
                        let state = u16::from(e.state);
                        let (shift, ctrl) = (state & 1 != 0, state & 4 != 0);
                        let key = Some(e.detail);
                        let paste_from = if ctrl && !shift && key == kc_v {
                            Some("CLIPBOARD")
                        } else if shift && !ctrl && key == kc_insert {
                            Some("PRIMARY")
                        } else {
                            None
                        };
                        if ctrl && !shift && key == kc_c && selected.is_some() {
                            conn.set_selection_owner(win, clipboard, e.time).unwrap();
                            conn.flush().unwrap();
                        } else if let Some(sel) = paste_from {
                            // 读取要等对端应答：放到独立的连接与线程上
                            let tx = pasted.clone();
                            std::thread::spawn(move || {
                                let _ = tx.send(read(sel, "UTF8_STRING", Duration::from_secs(2)));
                            });
                        }
                    }
                    Event::SelectionRequest(e) => answer(&conn, &e, selected, utf8, targets),
                    _ => {}
                }
            }
        }

        /// 剪贴板管理器（freedesktop 规范）：占有 CLIPBOARD_MANAGER；收到 SAVE_TARGETS 请求时读出 CLIPBOARD 的
        /// UTF8_STRING 保存下来，再回应请求方
        pub struct Manager {
            saved: mpsc::Receiver<Option<Vec<u8>>>,
            stop: Arc<AtomicBool>,
            handle: Option<JoinHandle<()>>,
        }

        impl Manager {
            pub fn start() -> Manager {
                let stop = Arc::new(AtomicBool::new(false));
                let (ready_tx, ready_rx) = mpsc::channel();
                let (saved_tx, saved_rx) = mpsc::channel();
                let s2 = Arc::clone(&stop);
                let handle = std::thread::spawn(move || run_manager(&s2, &ready_tx, &saved_tx));
                ready_rx
                    .recv_timeout(Duration::from_secs(3))
                    .expect("测试剪贴板管理器没有成功占有 CLIPBOARD_MANAGER");
                Manager {
                    saved: saved_rx,
                    stop,
                    handle: Some(handle),
                }
            }

            /// 等待一次保存，返回保存到的内容
            pub fn saved(&self, timeout: Duration) -> Option<Vec<u8>> {
                self.saved.recv_timeout(timeout).ok().flatten()
            }
        }

        impl Drop for Manager {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::SeqCst);
                if let Some(h) = self.handle.take() {
                    let _ = h.join();
                }
            }
        }

        fn run_manager(stop: &AtomicBool, ready: &mpsc::Sender<()>, saved: &mpsc::Sender<Option<Vec<u8>>>) {
            let (conn, screen) = RustConnection::connect(None).unwrap();
            let win = window(&conn, screen);
            let (manager, save_targets) = (atom(&conn, "CLIPBOARD_MANAGER"), atom(&conn, "SAVE_TARGETS"));
            conn.set_selection_owner(win, manager, CURRENT_TIME).unwrap();
            assert_eq!(conn.get_selection_owner(manager).unwrap().reply().unwrap().owner, win);
            ready.send(()).unwrap();
            while !stop.load(Ordering::SeqCst) {
                let Some(ev) = conn.poll_for_event().unwrap() else {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                };
                if let Event::SelectionRequest(e) = ev {
                    if e.target != save_targets {
                        notify(&conn, &e, NONE);
                        continue;
                    }
                    // 读取要等 Wisp 应答：放到独立的连接与线程上，读完再回应
                    let tx = saved.clone();
                    std::thread::spawn(move || {
                        let data = read("CLIPBOARD", "UTF8_STRING", Duration::from_secs(2));
                        let _ = tx.send(data);
                        let (c2, _) = RustConnection::connect(None).unwrap();
                        notify(&c2, &e, e.property);
                        // 断开前做一次往返：客户端写完请求立即断开时，X 服务器可能先看到挂断、直接关闭该客户端而丢弃
                        // 缓冲区里还没读的请求（Xvfb 上实测约 9 成概率丢掉这条 SendEvent，Wisp 随之等满 2 s 超时）
                        c2.sync().unwrap();
                    });
                }
            }
        }

        /// 已连接、有 CRTC 的 RandR 输出名，查询方式与 wisp-capture 的 X11 后端（`backend_x11::list_outputs`）相同；
        /// 没有 RandR 时为 ["default"]
        pub fn output_names() -> Vec<String> {
            let (conn, screen) = RustConnection::connect(None).unwrap();
            let root = conn.setup().roots[screen].root;
            let whole = || vec!["default".to_owned()];
            if conn
                .extension_information(randr::X11_EXTENSION_NAME)
                .ok()
                .flatten()
                .is_none()
            {
                return whole();
            }
            let Some(res) = conn
                .randr_query_version(1, 3)
                .ok()
                .and_then(|c| c.reply().ok())
                .and_then(|_| conn.randr_get_screen_resources_current(root).ok())
                .and_then(|c| c.reply().ok())
            else {
                return whole();
            };
            let ts = res.config_timestamp;
            let names: Vec<String> = res
                .outputs
                .iter()
                .filter_map(|&o| conn.randr_get_output_info(o, ts).ok()?.reply().ok())
                .filter(|i| i.crtc != 0 && i.connection == randr::Connection::CONNECTED)
                .filter_map(|i| {
                    let crtc = conn.randr_get_crtc_info(i.crtc, ts).ok()?.reply().ok()?;
                    (crtc.width > 0 && crtc.height > 0).then(|| String::from_utf8_lossy(&i.name).into_owned())
                })
                .collect();
            if names.is_empty() { whole() } else { names }
        }

        /// 以独立客户端读取选区（支持 INCR），用来验证 Wisp 作为 owner 的发送
        pub fn read(selection: &str, target: &str, timeout: Duration) -> Option<Vec<u8>> {
            let (conn, screen) = RustConnection::connect(None).unwrap();
            let win = window(&conn, screen);
            let (sel, tgt, prop, incr) = (
                atom(&conn, selection),
                atom(&conn, target),
                atom(&conn, "WISP_TEST_PROP"),
                atom(&conn, "INCR"),
            );
            conn.convert_selection(win, sel, tgt, prop, CURRENT_TIME).unwrap();
            conn.flush().unwrap();
            let deadline = Instant::now() + timeout;
            let take = |conn: &RustConnection| {
                conn.get_property(true, win, prop, AtomEnum::ANY, 0, u32::MAX / 4)
                    .unwrap()
                    .reply()
                    .unwrap()
            };
            let mut incr_data: Option<Vec<u8>> = None;
            while Instant::now() < deadline {
                let Some(ev) = conn.poll_for_event().unwrap() else {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                };
                match ev {
                    Event::SelectionNotify(e) => {
                        if e.property == NONE {
                            return None;
                        }
                        let r = take(&conn);
                        conn.flush().unwrap();
                        if r.type_ != incr {
                            return Some(r.value);
                        }
                        incr_data = Some(Vec::new());
                    }
                    Event::PropertyNotify(e) if e.atom == prop && e.state == Property::NEW_VALUE => {
                        let Some(buf) = incr_data.as_mut() else { continue };
                        let r = take(&conn);
                        conn.flush().unwrap();
                        if r.value.is_empty() {
                            return incr_data;
                        }
                        buf.extend_from_slice(&r.value);
                    }
                    _ => {}
                }
            }
            None
        }
    }

    /// 在任何环境（包括无头 CI）都能跑：info 不阻塞，不可用的能力都带 errors.platform.* 原因
    #[test]
    fn info_reports_reasons_for_unavailable_capabilities() {
        let p = platform();
        let started = Instant::now();
        let info = p.info();
        assert!(started.elapsed() < Duration::from_millis(50), "info 不得阻塞");
        assert_eq!(info.os, OsKind::Linux);
        assert_eq!(info.os_version, "test");
        let caps = &info.capabilities;
        for c in [
            &caps.global_hotkeys,
            &caps.selection,
            &caps.simulated_copy,
            &caps.paste_inject,
            &caps.capture_overlay,
            &caps.clipboard_watch,
            &caps.input_hooks,
        ] {
            if c.available {
                assert!(c.reason_key.is_none());
            } else {
                let key = c.reason_key.as_deref().unwrap_or_default();
                assert!(key.starts_with("errors.platform."), "{key}");
            }
        }
        if info.session == SessionKind::Wayland {
            assert!(!caps.selection.available && !caps.paste_inject.available);
        }
    }

    #[test]
    fn permissions_are_not_applicable() {
        let p = platform();
        let s = p.permissions.status();
        assert_eq!(s.accessibility, PermissionState::NotApplicable);
        assert_eq!(s.screen_recording, PermissionState::NotApplicable);
        assert_eq!(s.input_monitoring, PermissionState::NotApplicable);
    }

    #[test]
    #[ignore = "需要 X11 会话"]
    fn monitors_cursor_and_appearance() {
        assert!(has_x11(), "需要 X11 会话（DISPLAY）");
        let p = platform();
        let monitors = p.screen.monitors();
        assert!(!monitors.is_empty());
        assert_eq!(monitors.iter().filter(|m| m.is_primary).count(), 1);
        for m in &monitors {
            assert!(m.scale_factor >= 1.0, "{m:?}");
            assert!(m.frame.width > 0.0 && m.frame.height > 0.0, "{m:?}");
            // 工作区在显示器范围内（显示器内逻辑坐标）
            assert!(m.work_area.x >= 0.0 && m.work_area.y >= 0.0, "{m:?}");
            assert!(
                m.work_area.x + m.work_area.width <= m.frame.width + 0.5
                    && m.work_area.y + m.work_area.height <= m.frame.height + 0.5,
                "{m:?}"
            );
        }
        let (id, at) = p.screen.cursor();
        let m = monitors.iter().find(|m| m.id == id).expect("光标所在显示器不在列表中");
        assert!(at.x >= 0.0 && at.y >= 0.0 && at.x <= m.frame.width && at.y <= m.frame.height);
        // 外观读取查缓存：首次最多等 portal 250 ms，之后稳定且不阻塞
        let dark = p.screen.dark_mode();
        let started = Instant::now();
        assert_eq!(p.screen.dark_mode(), dark);
        let _ = p.screen.reduce_motion();
        assert!(started.elapsed() < Duration::from_millis(20), "外观读取应命中缓存");
        if let Some(accent) = p.screen.system_accent() {
            assert!(
                accent.len() == 7 && accent.starts_with('#') && accent[1..].chars().all(|c| c.is_ascii_hexdigit()),
                "{accent}"
            );
        }
    }

    #[test]
    #[ignore = "需要 X11 会话"]
    fn clipboard_write_read_snapshot_restore() {
        assert!(has_x11(), "需要 X11 会话（DISPLAY）");
        let p = platform();
        let original = p
            .clipboard
            .snapshot(10 * 1024 * 1024, Duration::from_millis(200))
            .unwrap();
        let before = p.clipboard.change_id();
        let id = p
            .paste
            .write(&ClipPayload::text("wisp smoke 中文 ✓"), WriteOpts::default())
            .unwrap();
        assert!(id > before, "写入后序号应递增");
        let raw = p.clipboard.read(&ReadLimits::default()).unwrap();
        assert!(raw.from_wisp);
        assert!(!raw.concealed);
        assert_eq!(raw.text.as_deref(), Some("wisp smoke 中文 ✓"));
        // 其他应用读到的内容一致
        assert_eq!(
            peer::read("CLIPBOARD", "UTF8_STRING", Duration::from_secs(2)).as_deref(),
            Some("wisp smoke 中文 ✓".as_bytes())
        );
        if let Some(snap) = original {
            let after = p.clipboard.restore(snap).unwrap();
            assert!(after > id);
        }
    }

    #[test]
    #[ignore = "需要 X11 会话"]
    fn restore_keeps_exclude_from_history() {
        assert!(has_x11(), "需要 X11 会话（DISPLAY）");
        let p = platform();
        let secret = WriteOpts {
            exclude_from_history: true,
            ..WriteOpts::default()
        };
        p.paste.write(&ClipPayload::text("hunter2"), secret).unwrap();
        assert!(p.clipboard.read(&ReadLimits::default()).unwrap().concealed);
        let snap = p
            .clipboard
            .snapshot(1024 * 1024, Duration::from_millis(200))
            .unwrap()
            .expect("Wisp 自己持有的剪贴板一定能快照");
        p.paste
            .write(&ClipPayload::text("public"), WriteOpts::default())
            .unwrap();
        p.clipboard.restore(snap).unwrap();
        let raw = p.clipboard.read(&ReadLimits::default()).unwrap();
        assert!(raw.concealed, "恢复后仍应按隐私内容处理");
        assert!(raw.text.is_none());
        assert_eq!(
            peer::read("CLIPBOARD", "UTF8_STRING", Duration::from_secs(2)).as_deref(),
            Some(b"hunter2".as_slice()),
            "其他应用仍能粘贴恢复后的内容"
        );
    }

    #[test]
    #[ignore = "需要 X11 会话"]
    fn large_clipboard_roundtrips_via_incr_both_ways() {
        assert!(has_x11(), "需要 X11 会话（DISPLAY）");
        let p = platform();
        // Wisp 作为 owner：700 KB > 256 KB，必须用 INCR 发送
        let text = big_text(700 * 1024);
        p.paste
            .write(&ClipPayload::text(text.clone()), WriteOpts::default())
            .unwrap();
        let got = peer::read("CLIPBOARD", "UTF8_STRING", Duration::from_secs(5)).expect("INCR 读取失败");
        assert_eq!(got.len(), text.len());
        assert!(got == text.as_bytes());
        // Wisp 作为读取方：外部 owner 用 64 KB 一块的 INCR 发送 600 KB
        let text = big_text(600 * 1024);
        let _owner = peer::Owner::start("CLIPBOARD", text.clone().into_bytes(), Duration::ZERO);
        let raw = p.clipboard.read(&ReadLimits::default()).unwrap();
        assert!(!raw.from_wisp);
        assert!(!raw.text_truncated);
        assert_eq!(raw.text.as_deref().map(str::len), Some(text.len()));
        assert!(raw.text.as_deref() == Some(text.as_str()));
    }

    #[test]
    #[ignore = "需要 X11 会话"]
    fn reads_primary_selection_from_another_client() {
        assert!(has_x11(), "需要 X11 会话（DISPLAY）");
        let p = platform();
        let _owner = peer::Owner::start("PRIMARY", "  wisp primary ✓ 中文 ".as_bytes().to_vec(), Duration::ZERO);
        let opts = SelectionOpts {
            allow_copy_fallback: false,
            ..SelectionOpts::default()
        };
        let sel = p.selection.get(&opts).unwrap();
        assert_eq!(sel.text.trim(), "wisp primary ✓ 中文");
        assert_eq!(sel.method, wisp_platform::CaptureMethod::Primary);
        assert!(sel.elapsed_ms < 100, "{} ms", sel.elapsed_ms);
        // 没有安装浮标钩子：has_selection 只看 PRIMARY 是否非空
        assert_eq!(p.selection.has_selection(Duration::from_millis(100)), Some(true));
        assert_eq!(p.selection.link_at_cursor(Duration::from_millis(10)), Some(false));
    }

    #[test]
    #[ignore = "需要 X11 会话"]
    fn slow_clipboard_read_does_not_block_primary() {
        assert!(has_x11(), "需要 X11 会话（DISPLAY）");
        let p = platform();
        // 剪贴板 owner 很慢：2 MB 的 INCR，每块之间停 10 ms（整体 ≥ 300 ms）
        let _clip = peer::Owner::start(
            "CLIPBOARD",
            big_text(2 * 1024 * 1024).into_bytes(),
            Duration::from_millis(10),
        );
        let _primary = peer::Owner::start("PRIMARY", b"quick".to_vec(), Duration::ZERO);
        let p2 = Arc::clone(&p);
        let history = std::thread::spawn(move || p2.clipboard.read(&ReadLimits::default()));
        std::thread::sleep(Duration::from_millis(30));
        // 历史读取进行中：取词与快照不必等它
        let started = Instant::now();
        let opts = SelectionOpts {
            allow_copy_fallback: false,
            ..SelectionOpts::default()
        };
        let sel = p.selection.get(&opts).unwrap();
        assert_eq!(sel.text, "quick");
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "{:?}",
            started.elapsed()
        );
        // 文本上限 1 MB：INCR 读到上限即截断（对端剩余的数据随请求方窗口一起丢弃）
        let raw = history.join().unwrap().unwrap();
        assert!(raw.text_truncated);
        assert_eq!(raw.text.map(|t| t.len()), Some(ReadLimits::default().max_text_bytes));
    }

    #[test]
    #[ignore = "需要 X11 会话"]
    fn clipboard_watch_is_event_driven() {
        assert!(has_x11(), "需要 X11 会话（DISPLAY）");
        let p = platform();
        let seen = Arc::new(AtomicU64::new(0));
        let s2 = Arc::clone(&seen);
        let handle = p
            .clipboard
            .watch(Box::new(move |id| s2.store(id, Ordering::SeqCst)))
            .unwrap();
        // 外部应用的复制
        let _owner = peer::Owner::start("CLIPBOARD", b"from peer".to_vec(), Duration::ZERO);
        let deadline = Instant::now() + Duration::from_secs(1);
        while seen.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(seen.load(Ordering::SeqCst), p.clipboard.change_id());
        // Wisp 自己的普通写入也回调（from_wisp = true，由上层去重）
        let id = p
            .paste
            .write(&ClipPayload::text("watch"), WriteOpts::default())
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while seen.load(Ordering::SeqCst) < id && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(seen.load(Ordering::SeqCst), id);
        drop(handle);
        // 抑制区间内的变更不回调
        seen.store(0, Ordering::SeqCst);
        let s3 = Arc::clone(&seen);
        let handle = p
            .clipboard
            .watch(Box::new(move |id| s3.store(id, Ordering::SeqCst)))
            .unwrap();
        p.clipboard.suppress(wisp_platform::SuppressRange {
            from_change_id: p.clipboard.change_id() + 1,
            ttl: Duration::from_millis(300),
        });
        let _ = p
            .paste
            .write(&ClipPayload::text("suppressed"), WriteOpts::default())
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(seen.load(Ordering::SeqCst), 0);
        // 排除历史的写入不回调
        std::thread::sleep(Duration::from_millis(300));
        let secret = WriteOpts {
            exclude_from_history: true,
            ..WriteOpts::default()
        };
        let _ = p.paste.write(&ClipPayload::text("secret"), secret).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(seen.load(Ordering::SeqCst), 0);
        drop(handle);
    }

    /// 等到 Wisp 看到序号超过 `after`（XFixes 事件由 hub 线程异步处理）
    fn wait_change_id(p: &Platform, after: u64) -> u64 {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let id = p.clipboard.change_id();
            if id > after || Instant::now() >= deadline {
                return id;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    #[ignore = "需要 X11 会话"]
    fn copy_fallback_reads_restores_and_suppresses() {
        assert!(has_x11(), "需要 X11 会话（DISPLAY）");
        let p = platform();
        // 没有 PRIMARY 选区 → 走模拟复制；剪贴板里原本是另一个应用复制的内容
        peer::clear_selection("PRIMARY");
        let c0 = p.clipboard.change_id();
        let _original = peer::Owner::start("CLIPBOARD", b"original clipboard".to_vec(), Duration::ZERO);
        let c1 = wait_change_id(&p, c0);
        assert!(c1 > c0);
        let seen = Arc::new(AtomicU64::new(0));
        let s2 = Arc::clone(&seen);
        let _watch = p
            .clipboard
            .watch(Box::new(move |id| s2.store(id, Ordering::SeqCst)))
            .unwrap();
        // 有焦点的应用：收到 Ctrl+C 时把选中的文字放进剪贴板
        let app = peer::App::start("wisppeer", "WispPeer", Some("selected by fallback ✓".as_bytes()));
        // 历史关闭：必须先备份成功才会发复制键
        let opts = SelectionOpts {
            history_enabled: false,
            ..SelectionOpts::default()
        };
        let sel = p.selection.get(&opts).unwrap();
        assert_eq!(sel.text, "selected by fallback ✓");
        assert_eq!(sel.method, wisp_platform::CaptureMethod::Copy);
        assert_eq!(sel.source.as_ref().map(|s| s.id.as_str()), Some("wisppeer"));
        // ⑦ 剪贴板恢复为原内容（现在由 Wisp 代为持有），其他应用粘贴到的仍是原内容
        assert_eq!(
            peer::read("CLIPBOARD", "UTF8_STRING", Duration::from_secs(2)).as_deref(),
            Some(b"original clipboard".as_slice())
        );
        let raw = p.clipboard.read(&ReadLimits::default()).unwrap();
        assert_eq!(raw.text.as_deref(), Some("original clipboard"));
        // 目标应用的复制、Wisp 的恢复各一次变更，都不进入历史
        let c2 = p.clipboard.change_id();
        assert!(c2 >= c1 + 2, "{c1} → {c2}");
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(seen.load(Ordering::SeqCst), 0, "模拟复制与恢复不应回调历史监听");
        drop(app);

        // 没有选中文字：应用对 Ctrl+C 没有反应 → 序号不变、剪贴板不动，返回 Empty
        let _empty = peer::App::start("wisppeer", "WispPeer", None);
        let started = Instant::now();
        assert!(matches!(p.selection.get(&opts), Err(SelectionError::Empty)));
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(p.clipboard.change_id(), c2);
        assert_eq!(
            peer::read("CLIPBOARD", "UTF8_STRING", Duration::from_secs(2)).as_deref(),
            Some(b"original clipboard".as_slice())
        );
        assert_eq!(seen.load(Ordering::SeqCst), 0);
    }

    #[test]
    #[ignore = "需要 X11 会话"]
    fn paste_into_activates_the_target_and_feeds_primary_terminals() {
        assert!(has_x11(), "需要 X11 会话（DISPLAY）");
        let p = platform();
        let editor = peer::App::start("wisppeer", "WispPeer", None);
        // 后启动的「xterm」拿到了焦点：粘贴到 editor 需要先激活它
        let xterm = peer::App::start("xterm", "XTerm", None);
        p.paste
            .write(&ClipPayload::text("translated ✓"), WriteOpts::default())
            .unwrap();
        // 没有 EWMH 窗口管理器（Xvfb）时自己设输入焦点；Ctrl+V 读 CLIPBOARD
        p.paste.paste_into(&editor.app_ref(), false).unwrap();
        assert_eq!(
            editor.wait_pasted(Duration::from_secs(2)).as_deref(),
            Some("translated ✓".as_bytes())
        );
        // xterm 的粘贴键是 Shift+Insert，读的是 PRIMARY：Wisp 发键前以同一份内容占有 PRIMARY
        p.paste.paste_into(&xterm.app_ref(), false).unwrap();
        assert_eq!(
            xterm.wait_pasted(Duration::from_secs(2)).as_deref(),
            Some("translated ✓".as_bytes())
        );
        // 目标窗口已经关闭
        let gone = editor.app_ref();
        drop(editor);
        std::thread::sleep(Duration::from_millis(50));
        assert!(matches!(
            p.paste.paste_into(&gone, false),
            Err(wisp_platform::PasteError::TargetGone)
        ));
    }

    #[test]
    #[ignore = "需要 X11 会话"]
    fn monitor_ids_are_randr_output_names() {
        assert!(has_x11(), "需要 X11 会话（DISPLAY）");
        let p = platform();
        let ids: Vec<String> = p.screen.monitors().into_iter().map(|m| m.id).collect();
        // 与 wisp-capture 的 X11 后端用同一套 RandR 查询：Overlay、锚点、浮标按 MonitorId 查找显示器时两边一致
        assert_eq!(ids, peer::output_names());
    }

    #[test]
    #[ignore = "需要 X11 会话"]
    fn exit_hands_the_clipboard_to_the_manager() {
        assert!(has_x11(), "需要 X11 会话（DISPLAY）");
        let p = platform();
        p.paste
            .write(&ClipPayload::text("keep me ✓"), WriteOpts::default())
            .unwrap();
        let manager = peer::Manager::start();
        // 丢弃 Platform = 退出：Wisp 仍持有剪贴板，请管理器保存
        let started = Instant::now();
        drop(p);
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        assert_eq!(
            manager.saved(Duration::from_millis(500)).as_deref(),
            Some("keep me ✓".as_bytes())
        );
    }

    #[test]
    #[ignore = "需要 X11 会话"]
    fn frontmost_layouts_and_app_info() {
        assert!(has_x11(), "需要 X11 会话（DISPLAY）");
        let p = platform();
        // 没有窗口管理器（例如 Xvfb）时可能没有前台应用
        if let Some(app) = p.focus.frontmost() {
            assert!(!app.id.is_empty());
            assert_eq!(app.id, app.id.to_ascii_lowercase());
            assert!(!p.apps.display_name(&app).trim().is_empty());
            if let Some(png) = p.apps.icon_png(&app, 64) {
                assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
                let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
                let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
                assert!(w > 0 && h > 0 && w.max(h) <= 80, "{w}x{h}");
            }
        }
        for app in p.apps.running_apps() {
            assert!(!app.id.is_empty());
            assert_ne!(app.pid, Some(std::process::id()), "不包含 Wisp 自己");
        }
        let layouts = p.hotkey_env.keyboard_layouts();
        assert!(layouts.input_language_count >= 1);
        assert!(p.hotkey_env.system_hotkeys().is_empty());
    }

    /// 全局快捷键用例的公共部分：扮演「另一个应用」的独立 X 客户端（XTest 发按键、试抓同一组合、
    /// 改键盘映射、断开 Wisp 的连接），以及收集平台层回调的通道
    #[cfg(test)]
    mod keys {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        use x11rb::connection::Connection as _;
        use x11rb::errors::ReplyError;
        use x11rb::protocol::ErrorKind;
        use x11rb::protocol::xproto::{
            ConnectionExt as _, CreateWindowAux, EventMask, GrabMode, InputFocus, KEY_PRESS_EVENT, KEY_RELEASE_EVENT,
            ModMask, Window, WindowClass,
        };
        use x11rb::protocol::{Event, xtest};
        use x11rb::rust_connection::RustConnection;

        pub const XK_CONTROL_L: u32 = 0xffe3;
        pub const XK_ALT_L: u32 = 0xffe9;
        pub const CTRL_ALT: u16 = (1 << 2) | (1 << 3);

        /// 收集 `KeyGrab` 回调
        pub struct Calls {
            rx: mpsc::Receiver<(String, bool)>,
        }

        impl Calls {
            pub fn attach(grab: &dyn wisp_platform::KeyGrab) -> Calls {
                let (tx, rx) = mpsc::channel::<(String, bool)>();
                let tx = parking_lot::Mutex::new(tx);
                grab.set_handler(Box::new(move |combo, pressed| {
                    let _ = tx.lock().send((combo.to_owned(), pressed));
                }));
                Calls { rx }
            }

            /// 下一个回调（`want = None`：300 ms 内不应有回调）
            pub fn expect(&self, want: Option<(&str, bool)>) {
                let wait = Duration::from_millis(if want.is_some() { 2000 } else { 300 });
                let got = self.rx.recv_timeout(wait).ok();
                assert_eq!(got.as_ref().map(|(c, p)| (c.as_str(), *p)), want);
            }

            /// 丢掉已经到达的回调
            pub fn drain(&self) {
                while self.rx.try_recv().is_ok() {}
            }
        }

        /// 另一个 X 客户端
        pub struct Peer {
            pub conn: RustConnection,
            pub root: Window,
            pub min: u8,
            pub per: usize,
            pub syms: Vec<u32>,
        }

        impl Peer {
            pub fn connect() -> Peer {
                let (conn, screen) = RustConnection::connect(None).expect("连接 X 服务器");
                let root = conn.setup().roots[screen].root;
                let min = conn.setup().min_keycode;
                let mut p = Peer {
                    conn,
                    root,
                    min,
                    per: 0,
                    syms: Vec::new(),
                };
                p.reload();
                p
            }

            /// 重新读取键盘映射
            pub fn reload(&mut self) {
                let (min, max) = (self.conn.setup().min_keycode, self.conn.setup().max_keycode);
                let m = self
                    .conn
                    .get_keyboard_mapping(min, max - min + 1)
                    .unwrap()
                    .reply()
                    .unwrap();
                self.per = usize::from(m.keysyms_per_keycode);
                self.syms = m.keysyms;
            }

            /// 第一列就是这个 keysym 的 keycode
            pub fn code_of(&self, sym: u32) -> u8 {
                let i = self
                    .syms
                    .chunks(self.per)
                    .position(|c| c.first() == Some(&sym))
                    .expect("布局里有这个键");
                self.min + u8::try_from(i).unwrap()
            }

            /// 完全空闲（没有任何 keysym）的 keycode，从大到小
            pub fn spares(&self) -> Vec<u8> {
                let mut out: Vec<u8> = self
                    .syms
                    .chunks(self.per)
                    .enumerate()
                    .filter(|(_, c)| c.iter().all(|&s| s == 0))
                    .map(|(i, _)| self.min + u8::try_from(i).unwrap())
                    .collect();
                out.reverse();
                out
            }

            /// 一个 keycode 的整行 keysym
            pub fn row(&self, code: u8) -> Vec<u32> {
                let i = usize::from(code - self.min) * self.per;
                self.syms[i..i + self.per].to_vec()
            }

            /// 改一个 keycode 的整行映射（触发 `MappingNotify(Keyboard)`）
            pub fn set_row(&mut self, code: u8, row: &[u32]) {
                let per = u8::try_from(self.per).unwrap();
                self.conn
                    .change_keyboard_mapping(1, code, per, row)
                    .unwrap()
                    .check()
                    .unwrap();
                self.reload();
            }

            /// 把一个 keycode 映射成单个 keysym（0 = 清空）
            pub fn remap(&mut self, code: u8, sym: u32) {
                let mut row = vec![0u32; self.per];
                if sym != 0 {
                    row[0] = sym;
                    row[1] = sym;
                }
                self.set_row(code, &row);
            }

            pub fn key(&self, code: u8, press: bool) {
                let ty = if press { KEY_PRESS_EVENT } else { KEY_RELEASE_EVENT };
                xtest::fake_input(&self.conn, ty, code, x11rb::CURRENT_TIME, self.root, 0, 0, 0).unwrap();
                self.conn.flush().unwrap();
            }

            /// 按下 Ctrl+Alt+`code` 再全部抬起
            pub fn chord(&self, code: u8) {
                let (ctrl, alt) = (self.code_of(XK_CONTROL_L), self.code_of(XK_ALT_L));
                for (c, p) in [
                    (ctrl, true),
                    (alt, true),
                    (code, true),
                    (code, false),
                    (alt, false),
                    (ctrl, false),
                ] {
                    self.key(c, p);
                }
            }

            /// 试着在根窗口上抓取（4 种锁定位组合都抓）：Ok = 没有别人抓着（随即撤销），Err = 被占用（BadAccess）
            pub fn try_grab(&self, code: u8, mods: u16) -> Result<(), ()> {
                let mut ok = Vec::new();
                for lock in [0u16, 1 << 1, 1 << 4, (1 << 1) | (1 << 4)] {
                    let r = self
                        .conn
                        .grab_key(
                            false,
                            self.root,
                            ModMask::from(mods | lock),
                            code,
                            GrabMode::ASYNC,
                            GrabMode::ASYNC,
                        )
                        .unwrap()
                        .check();
                    match r {
                        Ok(()) => ok.push(lock),
                        Err(ReplyError::X11Error(e)) if e.error_kind == ErrorKind::Access => {
                            for l in ok {
                                self.conn.ungrab_key(code, self.root, ModMask::from(mods | l)).unwrap();
                            }
                            self.conn.flush().unwrap();
                            return Err(());
                        }
                        Err(e) => panic!("grab_key: {e}"),
                    }
                }
                for l in ok {
                    self.conn.ungrab_key(code, self.root, ModMask::from(mods | l)).unwrap();
                }
                self.conn.flush().unwrap();
                Ok(())
            }

            /// 等到 Wisp 释放了这个组合（重新抓取在 Wisp 的回调线程上异步完成）。**只用于 Wisp 接下来不会再抓的
            /// keycode**：试抓是真的抓取，若 Wisp 恰好在这一瞬间抓同一组合会得到 BadAccess
            pub fn wait_released(&self, code: u8, mods: u16) {
                let until = Instant::now() + Duration::from_secs(3);
                while self.try_grab(code, mods).is_err() {
                    assert!(Instant::now() < until, "keycode {code} 一直被抓着");
                    std::thread::sleep(Duration::from_millis(10));
                }
            }

            /// 建一个有输入焦点的窗口，返回它（用来确认服务器确实对 XTest 按键做了自动重复）
            pub fn focus_window(&self) -> Window {
                let win = self.conn.generate_id().unwrap();
                self.conn
                    .create_window(
                        0,
                        win,
                        self.root,
                        0,
                        0,
                        10,
                        10,
                        0,
                        WindowClass::INPUT_OUTPUT,
                        0,
                        &CreateWindowAux::new()
                            .override_redirect(1)
                            .event_mask(EventMask::KEY_PRESS | EventMask::KEY_RELEASE),
                    )
                    .unwrap()
                    .check()
                    .unwrap();
                self.conn.map_window(win).unwrap().check().unwrap();
                self.conn
                    .set_input_focus(InputFocus::POINTER_ROOT, win, x11rb::CURRENT_TIME)
                    .unwrap()
                    .check()
                    .unwrap();
                win
            }

            /// 在 `dur` 内收到的按下 / 抬起事件数
            pub fn count_keys(&self, dur: Duration) -> (usize, usize) {
                let until = Instant::now() + dur;
                let (mut press, mut release) = (0, 0);
                while Instant::now() < until {
                    match self.conn.poll_for_event().unwrap() {
                        Some(Event::KeyPress(_)) => press += 1,
                        Some(Event::KeyRelease(_)) => release += 1,
                        Some(_) => {}
                        None => std::thread::sleep(Duration::from_millis(5)),
                    }
                }
                (press, release)
            }

            /// 全部 Wisp 平台层的辅助窗口（`WM_NAME = wisp-platform`）。同一个测试进程里前面的用例留下的 Platform 仍然
            /// 连着（事件线程持有连接），因此用「创建 Platform 前后的差集」找出这个用例自己的那一个
            pub fn helper_windows(&self) -> Vec<Window> {
                let tree = self.conn.query_tree(self.root).unwrap().reply().unwrap();
                tree.children
                    .into_iter()
                    .filter(|&w| {
                        self.conn
                            .get_property(false, w, x11rb::protocol::xproto::AtomEnum::WM_NAME, 0u32, 0, 64)
                            .ok()
                            .and_then(|c| c.reply().ok())
                            .is_some_and(|r| r.value == b"wisp-platform")
                    })
                    .collect()
            }

            /// 断开拥有这个窗口的客户端（KillClient），模拟 X 连接意外断开
            pub fn kill_client_of(&self, w: Window) {
                self.conn.kill_client(w).unwrap().check().unwrap();
            }
        }
    }

    /// 平台层的 X11 全局快捷键——被动抓取的按键经平台层唯一的 X11 事件线程分发，按住只
    /// 回调一次；已被别的客户端抓取的组合报告 `Taken`；注销后不再回调、也不留抓取（别的客户端随即可以抓同一组合）。
    /// 按键由独立的 XTest 客户端发出（Xvfb 下可跑）
    #[test]
    #[ignore = "需要 X11 会话"]
    fn global_hotkeys_are_grabbed_and_event_driven() {
        use wisp_platform::KeyGrabError;
        use x11rb::protocol::xproto::{ConnectionExt as _, GrabMode, ModMask};

        assert!(has_x11(), "需要 X11 会话（DISPLAY）");
        let p = platform();
        let grab = p.hotkey_env.key_grab().expect("X11 会话下有平台层的快捷键后端");
        let calls = keys::Calls::attach(grab);
        // 宽松写法与重复注册都可以
        grab.register("ctrl+alt+keyj").expect("注册");
        grab.register("Control+Alt+KeyJ").expect("重复注册视为成功");

        let peer = keys::Peer::connect();
        let (ctrl, alt, j) = (
            peer.code_of(keys::XK_CONTROL_L),
            peer.code_of(keys::XK_ALT_L),
            peer.code_of(u32::from(b'j')),
        );
        assert_eq!(peer.try_grab(j, keys::CTRL_ALT), Err(()), "Wisp 抓着这个组合");

        peer.key(ctrl, true);
        peer.key(alt, true);
        peer.key(j, true);
        calls.expect(Some(("Control+Alt+KeyJ", true)));
        // 重复的按下不再回调
        peer.key(j, true);
        calls.expect(None);
        peer.key(j, false);
        calls.expect(Some(("Control+Alt+KeyJ", false)));
        peer.key(alt, false);
        peer.key(ctrl, false);
        // 只按 Ctrl+J：修饰位不符，不回调
        peer.key(ctrl, true);
        peer.key(j, true);
        peer.key(j, false);
        peer.key(ctrl, false);
        calls.expect(None);

        // 已被别的客户端抓取的组合：Taken，且不留半个抓取
        let k = peer.code_of(u32::from(b'k'));
        peer.conn
            .grab_key(
                false,
                peer.root,
                ModMask::CONTROL | ModMask::M1,
                k,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )
            .unwrap()
            .check()
            .unwrap();
        assert_eq!(grab.register("Control+Alt+KeyK"), Err(KeyGrabError::Taken));
        peer.conn
            .ungrab_key(k, peer.root, ModMask::CONTROL | ModMask::M1)
            .unwrap()
            .check()
            .unwrap();
        assert_eq!(peer.try_grab(k, keys::CTRL_ALT), Ok(()), "Taken 之后没有留下半个抓取");
        assert!(matches!(
            grab.register("Control+Alt+NoSuchKey"),
            Err(KeyGrabError::Rejected(_))
        ));

        // 注销之后不再回调，也不留抓取：别的客户端可以抓同一组合（4 种锁定位组合都可以）
        grab.unregister("Control+Alt+KeyJ");
        peer.chord(j);
        calls.expect(None);
        assert_eq!(peer.try_grab(j, keys::CTRL_ALT), Ok(()), "注销后没有遗留的抓取");
    }

    /// 服务器真实的自动重复（XKB 默认：按住 660 ms 后每 40 ms 一次成对的 Release + Press）：按住超过重复延迟，只回调
    /// 一次按下、一次抬起。先用一个有焦点的普通窗口确认服务器确实对 XTest 按键做了自动重复，用例才有意义
    #[test]
    #[ignore = "需要 X11 会话"]
    fn global_hotkeys_fold_server_autorepeat() {
        assert!(has_x11(), "需要 X11 会话（DISPLAY）");
        let peer = keys::Peer::connect();
        let (ctrl, alt, j) = (
            peer.code_of(keys::XK_CONTROL_L),
            peer.code_of(keys::XK_ALT_L),
            peer.code_of(u32::from(b'j')),
        );
        let hold = Duration::from_millis(1500);

        // 环境自检：没有抓取时，按住 J 1.5 s，焦点窗口收到多次按下（自动重复）
        let _win = peer.focus_window();
        peer.key(j, true);
        std::thread::sleep(hold);
        peer.key(j, false);
        let (press, release) = peer.count_keys(Duration::from_millis(300));
        assert!(press >= 5, "服务器没有自动重复（按下 {press} 次、抬起 {release} 次）");
        assert_eq!(press, release, "自动重复是成对的");

        let p = platform();
        let grab = p.hotkey_env.key_grab().expect("X11 会话下有平台层的快捷键后端");
        let calls = keys::Calls::attach(grab);
        grab.register("Control+Alt+KeyJ").expect("注册");
        peer.key(ctrl, true);
        peer.key(alt, true);
        peer.key(j, true);
        calls.expect(Some(("Control+Alt+KeyJ", true)));
        std::thread::sleep(hold);
        calls.expect(None);
        peer.key(j, false);
        calls.expect(Some(("Control+Alt+KeyJ", false)));
        peer.key(alt, false);
        peer.key(ctrl, false);
        calls.expect(None);
        // 再按一次照常触发（按住状态已经复位）
        peer.chord(j);
        calls.expect(Some(("Control+Alt+KeyJ", true)));
        calls.expect(Some(("Control+Alt+KeyJ", false)));
        grab.unregister("Control+Alt+KeyJ");
    }

    /// 键盘映射变化（`MappingNotify(Keyboard)`，setxkbmap 换布局时同样如此）：组合跟着 keysym 换到新的 keycode，旧
    /// keycode 不再被抓着；与已注册组合无关的映射变化（Wisp 自己发送按键时的临时映射就是这样）不撤销再抓，按住期间
    /// 发生也不丢抬起
    #[test]
    #[ignore = "需要 X11 会话"]
    fn global_hotkeys_follow_keyboard_mapping_changes() {
        assert!(has_x11(), "需要 X11 会话（DISPLAY）");
        let p = platform();
        let grab = p.hotkey_env.key_grab().expect("X11 会话下有平台层的快捷键后端");
        let calls = keys::Calls::attach(grab);
        grab.register("Control+Alt+KeyJ").expect("注册");
        let mut peer = keys::Peer::connect();
        let (ctrl, alt, j) = (
            peer.code_of(keys::XK_CONTROL_L),
            peer.code_of(keys::XK_ALT_L),
            peer.code_of(u32::from(b'j')),
        );
        let spares = peer.spares();
        let (spare, other) = (spares[0], spares[1]);
        let j_row = peer.row(j);

        // 1. 无关的映射变化（另一个空闲 keycode 临时映射成别的字符再还原），发生在按住期间：抬起照常送达、只送一次
        peer.key(ctrl, true);
        peer.key(alt, true);
        peer.key(j, true);
        calls.expect(Some(("Control+Alt+KeyJ", true)));
        peer.remap(other, 0x20ac);
        peer.remap(other, 0);
        std::thread::sleep(Duration::from_millis(200));
        calls.expect(None);
        peer.key(j, false);
        calls.expect(Some(("Control+Alt+KeyJ", false)));
        peer.key(alt, false);
        peer.key(ctrl, false);
        calls.expect(None);
        peer.chord(j);
        calls.expect(Some(("Control+Alt+KeyJ", true)));
        calls.expect(Some(("Control+Alt+KeyJ", false)));

        // 2. J 搬到另一个 keycode（原来的键清空）：旧 keycode 被释放、新 keycode 被抓上，按新键触发，按旧键不触发。
        //    同一次重新抓取里先撤销旧的、紧接着抓新的；等旧的释放之后再留一点余量，才去试抓新的（避免与它的抓取撞上）
        peer.remap(spare, u32::from(b'j'));
        peer.remap(j, 0);
        peer.wait_released(j, keys::CTRL_ALT);
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(peer.try_grab(spare, keys::CTRL_ALT), Err(()), "新 keycode 被抓上");
        calls.drain();
        peer.chord(spare);
        calls.expect(Some(("Control+Alt+KeyJ", true)));
        calls.expect(Some(("Control+Alt+KeyJ", false)));
        peer.chord(j);
        calls.expect(None);

        // 3. 还原映射：抓取回到原来的 keycode
        peer.set_row(j, &j_row);
        peer.remap(spare, 0);
        peer.wait_released(spare, keys::CTRL_ALT);
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(peer.try_grab(j, keys::CTRL_ALT), Err(()), "抓取回到原来的 keycode");
        peer.chord(j);
        calls.expect(Some(("Control+Alt+KeyJ", true)));
        calls.expect(Some(("Control+Alt+KeyJ", false)));

        // 注销后不留抓取
        grab.unregister("Control+Alt+KeyJ");
        assert_eq!(peer.try_grab(j, keys::CTRL_ALT), Ok(()));
    }

    /// X 连接意外断开（这里用另一个客户端 KillClient 模拟）：下一次调用重连，新连接接管全部快捷键；断开时正按着的组合
    /// 补发一次抬起；重连之后注销同样不留抓取
    #[test]
    #[ignore = "需要 X11 会话"]
    fn global_hotkeys_survive_reconnect() {
        assert!(has_x11(), "需要 X11 会话（DISPLAY）");
        let peer = keys::Peer::connect();
        let before = peer.helper_windows();
        let p = platform();
        let grab = p.hotkey_env.key_grab().expect("X11 会话下有平台层的快捷键后端");
        let calls = keys::Calls::attach(grab);
        grab.register("Control+Alt+KeyJ").expect("注册");
        grab.register("Control+Alt+KeyL").expect("注册");
        let ours: Vec<_> = peer
            .helper_windows()
            .into_iter()
            .filter(|w| !before.contains(w))
            .collect();
        assert_eq!(ours.len(), 1, "这个用例的 Platform 有且只有一个辅助窗口");
        let (ctrl, alt, j, l) = (
            peer.code_of(keys::XK_CONTROL_L),
            peer.code_of(keys::XK_ALT_L),
            peer.code_of(u32::from(b'j')),
            peer.code_of(u32::from(b'l')),
        );
        peer.key(ctrl, true);
        peer.key(alt, true);
        peer.key(j, true);
        calls.expect(Some(("Control+Alt+KeyJ", true)));

        peer.kill_client_of(ours[0]);
        // 连接断开后抓取随之消失（Wisp 还没重连，试抓不会与它撞上）
        peer.key(j, false);
        peer.key(alt, false);
        peer.key(ctrl, false);
        peer.wait_released(l, keys::CTRL_ALT);
        // 任何一次需要 X 的调用都会重连（这里取键盘布局；事件线程发现断开之前用的还是旧连接，所以重试）。重连在这次调用
        // 里同步完成并接管两个组合——试抓与它在同一个线程上先后发生，不会撞上
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            let _ = p.hotkey_env.keyboard_layouts();
            if peer.try_grab(l, keys::CTRL_ALT).is_err() {
                break;
            }
            assert!(Instant::now() < until, "没有重连 / 没有重新抓取");
            std::thread::sleep(Duration::from_millis(20));
        }
        // 断开时按着的 J：补发抬起
        calls.expect(Some(("Control+Alt+KeyJ", false)));
        assert_eq!(peer.try_grab(j, keys::CTRL_ALT), Err(()), "新连接接管了 J");
        peer.chord(l);
        calls.expect(Some(("Control+Alt+KeyL", true)));
        calls.expect(Some(("Control+Alt+KeyL", false)));
        grab.unregister("Control+Alt+KeyJ");
        grab.unregister("Control+Alt+KeyL");
        assert_eq!(peer.try_grab(j, keys::CTRL_ALT), Ok(()));
        assert_eq!(peer.try_grab(l, keys::CTRL_ALT), Ok(()));
    }
}
