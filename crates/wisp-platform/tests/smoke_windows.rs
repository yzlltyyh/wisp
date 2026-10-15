//! Windows 平台冒烟测试。只读查询类的用例默认运行；需要真实桌面会话、会改动剪贴板或安装全局钩子的
//! 用例标 `#[ignore]`，在 Windows 真机上手工运行：
//! `cargo test -p wisp-platform --test smoke_windows -- --ignored --test-threads=1`
#![cfg(target_os = "windows")]

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wisp_platform::{HookHandle, Platform, PlatformConfig, ReadLimits, SelectionOpts, SuppressRange, WriteOpts};
use wisp_types::{ClipPayload, OsKind, PermissionKind, PermissionState};

fn platform() -> Arc<Platform> {
    Platform::create(PlatformConfig {
        own_pid: std::process::id(),
        main_thread: Arc::new(|f: Box<dyn FnOnce() + Send>| f()),
        self_hit: Arc::new(|_, _| false),
        keymaps: Arc::new(parking_lot::RwLock::new(Vec::new())),
        os_version: "10.0.26100".to_owned(),
    })
}

/// 本进程中线程描述等于 `name` 的线程数（std 给命名线程设置了线程描述，`GetThreadDescription` 可以读回）
fn threads_named(name: &str) -> usize {
    use windows::Win32::Foundation::{CloseHandle, HLOCAL, LocalFree};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows::Win32::System::Threading::{GetThreadDescription, OpenThread, THREAD_QUERY_LIMITED_INFORMATION};

    let pid = std::process::id();
    let mut count = 0;
    // SAFETY: 快照与线程句柄用完即关；线程描述由系统分配，读完用 LocalFree 释放
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0).expect("thread snapshot");
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut ok = Thread32First(snap, &mut entry).is_ok();
        while ok {
            if entry.th32OwnerProcessID == pid
                && let Ok(thread) = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, false, entry.th32ThreadID)
            {
                if let Ok(desc) = GetThreadDescription(thread)
                    && !desc.is_null()
                {
                    if desc.to_string().is_ok_and(|d| d == name) {
                        count += 1;
                    }
                    let _ = LocalFree(Some(HLOCAL(desc.0.cast())));
                }
                let _ = CloseHandle(thread);
            }
            ok = Thread32Next(snap, &mut entry).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    count
}

fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    f()
}

#[test]
fn info_reports_windows_capabilities() {
    let p = platform();
    let info = p.info();
    assert_eq!(info.os, OsKind::Windows);
    assert_eq!(info.os_version, "10.0.26100");
    assert!(info.capabilities.selection.available);
    assert!(info.capabilities.clipboard_watch.available);
    assert!(info.capabilities.paste_inject.available);
    assert_eq!(info.capabilities.input_hooks.available, cfg!(feature = "mouse-hook"));
}

#[test]
fn permissions_are_not_applicable() {
    let p = platform();
    let set = p.permissions.status();
    for kind in [
        PermissionKind::Accessibility,
        PermissionKind::ScreenRecording,
        PermissionKind::InputMonitoring,
    ] {
        assert_eq!(set.get(kind), PermissionState::NotApplicable);
        assert!(p.permissions.request(kind).is_ok());
    }
}

#[test]
fn appearance_and_layout_queries() {
    let p = platform();
    // 只要不 panic、格式正确即可（CI 机器的设置未知）
    let _ = p.screen.dark_mode();
    let _ = p.screen.reduce_motion();
    let _ = p.screen.reduce_transparency();
    assert!(p.screen.taskbar_light().is_some());
    if let Some(accent) = p.screen.system_accent() {
        assert!(accent.starts_with('#') && accent.len() == 7, "{accent}");
    }
    let layouts = p.hotkey_env.keyboard_layouts();
    assert!(layouts.input_language_count >= 1);
    assert!(p.hotkey_env.system_hotkeys().is_empty());
}

#[test]
fn running_apps_include_this_process() {
    let p = platform();
    let apps = p.apps.running_apps();
    assert!(!apps.is_empty());
    let me = std::env::current_exe().expect("current exe");
    let exe = me
        .file_name()
        .and_then(|n| n.to_str())
        .expect("exe name")
        .to_lowercase();
    assert!(apps.iter().any(|a| a.id == exe), "{exe} not in running apps");
}

#[test]
#[ignore = "需要真实桌面会话"]
fn monitors_and_cursor() {
    let p = platform();
    let monitors = p.screen.monitors();
    assert!(!monitors.is_empty());
    assert!(monitors.iter().any(|m| m.is_primary));
    for m in &monitors {
        assert!(m.id.starts_with(r"\\.\DISPLAY"), "{}", m.id);
        assert!(m.scale_factor >= 1.0);
        assert!(m.work_area.width > 0.0 && m.work_area.height > 0.0);
    }
    let (id, pt) = p.screen.cursor();
    let m = monitors.iter().find(|m| m.id == id).expect("cursor monitor");
    assert!(pt.x >= 0.0 && pt.x <= m.frame.width + 1.0);
    assert!(pt.y >= 0.0 && pt.y <= m.frame.height + 1.0);
}

#[test]
#[ignore = "需要真实桌面会话（会改动剪贴板）"]
fn clipboard_write_read_and_change_id() {
    let p = platform();
    let before = p.clipboard.change_id();
    let id = p
        .paste
        .write(&ClipPayload::text("Wisp 冒烟测试\nsecond line"), WriteOpts::default())
        .expect("write");
    assert!(id > before);
    assert_eq!(p.clipboard.change_id(), id);
    let raw = p.clipboard.read(&ReadLimits::default()).expect("read");
    assert_eq!(raw.text.as_deref(), Some("Wisp 冒烟测试\nsecond line"));
    assert!(raw.from_wisp);
    assert!(!raw.concealed);
    // 文本超过上限：截断并标注
    p.paste
        .write(&ClipPayload::text("a".repeat(4096)), WriteOpts::default())
        .expect("write");
    let raw = p
        .clipboard
        .read(&ReadLimits {
            max_text_bytes: 1000,
            ..ReadLimits::default()
        })
        .expect("read");
    assert_eq!(raw.text.as_ref().map(String::len), Some(1000));
    assert!(raw.text_truncated);
}

#[test]
#[ignore = "需要真实桌面会话（会改动剪贴板）"]
fn clipboard_snapshot_restore_roundtrip() {
    let p = platform();
    p.paste
        .write(
            &ClipPayload {
                text: Some("original".into()),
                html: Some("<b>original</b>".into()),
                ..ClipPayload::default()
            },
            WriteOpts::default(),
        )
        .expect("write");
    let snap = p
        .clipboard
        .snapshot(10 * 1024 * 1024, Duration::from_millis(500))
        .expect("snapshot")
        .expect("not abandoned");
    p.paste
        .write(&ClipPayload::text("temporary"), WriteOpts::default())
        .expect("write");
    p.clipboard.restore(snap).expect("restore");
    let raw = p.clipboard.read(&ReadLimits::default()).expect("read");
    assert_eq!(raw.text.as_deref(), Some("original"));
    assert_eq!(raw.html.as_deref(), Some("<b>original</b>"));
}

#[test]
#[ignore = "需要真实桌面会话（会改动剪贴板）"]
fn clipboard_watch_debounce_and_suppress() {
    let p = platform();
    let seen = Arc::new(AtomicUsize::new(0));
    let last = Arc::new(AtomicU64::new(0));
    let handle: HookHandle = {
        let seen = Arc::clone(&seen);
        let last = Arc::clone(&last);
        p.clipboard
            .watch(Box::new(move |id| {
                seen.fetch_add(1, Ordering::SeqCst);
                last.store(id, Ordering::SeqCst);
            }))
            .expect("watch")
    };
    // 连续两次写入落在 50 ms 去抖窗口内，合并为一次回调
    p.paste
        .write(&ClipPayload::text("one"), WriteOpts::default())
        .expect("write");
    let id = p
        .paste
        .write(&ClipPayload::text("two"), WriteOpts::default())
        .expect("write");
    assert!(wait_until(Duration::from_secs(2), || seen.load(Ordering::SeqCst) >= 1));
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(seen.load(Ordering::SeqCst), 1);
    assert_eq!(last.load(Ordering::SeqCst), id);

    // 抑制区间内的变更不回调
    let next = p.clipboard.change_id() + 1;
    p.clipboard.suppress(SuppressRange {
        from_change_id: next,
        ttl: Duration::from_millis(500),
    });
    p.paste
        .write(&ClipPayload::text("suppressed"), WriteOpts::default())
        .expect("write");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(seen.load(Ordering::SeqCst), 1);

    // 注销后不再回调
    drop(handle);
    std::thread::sleep(Duration::from_millis(600));
    p.paste
        .write(&ClipPayload::text("after"), WriteOpts::default())
        .expect("write");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(seen.load(Ordering::SeqCst), 1);
}

#[test]
#[ignore = "需要真实桌面会话"]
fn frontmost_and_app_info() {
    let p = platform();
    if let Some(app) = p.focus.frontmost() {
        assert!(app.id.ends_with(".exe"), "{}", app.id);
        assert!(app.pid.is_some());
        assert!(!p.apps.display_name(&app).is_empty());
        let png = p.apps.icon_png(&app, 64).expect("icon");
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));
    }
}

#[test]
#[ignore = "需要真实桌面会话"]
fn selection_probes_respect_deadlines() {
    let p = platform();
    let t = Instant::now();
    let _ = p.selection.has_selection(Duration::from_millis(100));
    let _ = p.selection.link_at_cursor(Duration::from_millis(100));
    assert!(t.elapsed() < Duration::from_millis(400));
    // 不允许回退时绝不模拟复制：剪贴板序号不变
    let before = p.clipboard.change_id();
    let opts = SelectionOpts {
        allow_copy_fallback: false,
        ..SelectionOpts::default()
    };
    let _ = p.selection.get(&opts);
    assert_eq!(p.clipboard.change_id(), before);
}

/// 钩子线程与分发线程都已退出
fn no_hook_threads() -> bool {
    threads_named("wisp-hooks") == 0 && threads_named("wisp-hook-dispatch") == 0
}

#[test]
#[ignore = "需要真实桌面会话（安装全局钩子）"]
fn outside_click_hook_installs_and_releases() {
    let p = platform();
    // 没有任何订阅时进程里没有钩子线程
    assert!(
        wait_until(Duration::from_secs(2), no_hook_threads),
        "hook threads without subscribers"
    );
    let clicks = Arc::new(Mutex::new(Vec::new()));
    let h = {
        let clicks = Arc::clone(&clicks);
        p.clicks
            .watch(Box::new(move |e| clicks.lock().expect("lock").push(e)))
            .expect("watch")
    };
    assert_eq!(threads_named("wisp-hooks"), 1);
    assert_eq!(threads_named("wisp-hook-dispatch"), 1);
    // 第二个订阅复用同一个钩子线程
    let h2 = p.clicks.watch(Box::new(|_| {})).expect("second watch");
    assert_eq!(threads_named("wisp-hooks"), 1);
    drop(h);
    assert_eq!(threads_named("wisp-hooks"), 1, "还有订阅时钩子线程不退出");
    drop(h2);
    // 最后一个 Handle drop 后两个线程都退出
    assert!(
        wait_until(Duration::from_secs(2), no_hook_threads),
        "hook threads left after drop"
    );
    // 线程退出后可以重新订阅
    let h3 = p.clicks.watch(Box::new(|_| {})).expect("watch again");
    assert_eq!(threads_named("wisp-hooks"), 1);
    drop(h3);
    assert!(wait_until(Duration::from_secs(2), no_hook_threads));
}

#[test]
#[ignore = "需要真实桌面会话"]
fn appearance_watch_handle_drops_cleanly() {
    let p = platform();
    assert!(wait_until(Duration::from_secs(2), || threads_named("wisp-appearance") == 0));
    let h = p.screen.watch_appearance(Box::new(|| {})).expect("watch");
    assert_eq!(threads_named("wisp-appearance"), 1);
    drop(h);
    // Handle drop 后监听窗口销毁、线程退出
    assert!(
        wait_until(Duration::from_secs(2), || threads_named("wisp-appearance") == 0),
        "appearance watcher thread left after drop"
    );
}

#[cfg(feature = "mouse-hook")]
#[test]
#[ignore = "需要真实桌面会话（安装全局钩子、显示浮标）"]
fn input_hooks_and_bobber() {
    use wisp_platform::{DismissReason, DoubleTapSpec, HookSpec, InputSignal, MouseSpec};
    use wisp_types::{HotkeyAction, ModifierKey};

    let p = platform();
    // 空规格不安装任何钩子，返回空句柄
    assert!(p.hooks.install(HookSpec::default(), Box::new(|_| {})).is_ok());
    let spec = HookSpec {
        bobber: true,
        double_tap: Some(DoubleTapSpec {
            key: ModifierKey::RightCtrl,
            interval: Duration::from_millis(300),
        }),
        mouse: Some(MouseSpec {
            long_press: false,
            middle: Some(HotkeyAction::TranslateSelection),
            back: None,
            forward: None,
        }),
    };
    // 运行期间请不要动鼠标、键盘：这里要断言浮标的消失信号
    let signals = Arc::new(Mutex::new(Vec::<InputSignal>::new()));
    let h = {
        let signals = Arc::clone(&signals);
        p.hooks
            .install(spec, Box::new(move |s| signals.lock().unwrap().push(s)))
            .expect("install")
    };
    let dismissals = || {
        signals
            .lock()
            .unwrap()
            .iter()
            .filter_map(|s| match s {
                InputSignal::BobberDismiss(r) => Some(*r),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let (monitor, at) = p.screen.cursor();
    p.bobber.show((monitor.clone(), at), false, false, Box::new(|| {}));
    assert!(wait_until(Duration::from_secs(1), || p.bobber.rect().is_some()));
    let (id, rect) = p.bobber.rect().expect("rect");
    assert_eq!(id, monitor);
    // rect() 是 22 pt 的圆形（与 macOS 一致），不含只画投影的余量
    assert!((rect.width - wisp_platform::common::gesture::BOBBER_DIAMETER).abs() < 1e-9);
    assert!((rect.height - wisp_platform::common::gesture::BOBBER_DIAMETER).abs() < 1e-9);
    assert_eq!(threads_named("wisp-bobber"), 1);
    assert_eq!(threads_named("wisp-hooks"), 1);
    // 上层主动隐藏：淡出期间以及之后都不报告消失（消失信号只用于浮标自己消失的情形）
    p.bobber.hide(true);
    assert!(wait_until(Duration::from_secs(1), || p.bobber.rect().is_none()));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(dismissals(), Vec::new());
    // 4 s 无交互：恰好一次 Timeout，淡出期间不会再多出别的消失信号（真实按键的去重见模块内用例
    // `one_dismissal_per_showing_even_during_fade_out`；注入的按键会被钩子过滤，这里无法模拟）
    p.bobber.show((monitor.clone(), at), false, false, Box::new(|| {}));
    assert!(wait_until(Duration::from_secs(1), || p.bobber.rect().is_some()));
    assert!(wait_until(Duration::from_secs(6), || !dismissals().is_empty()));
    assert!(wait_until(Duration::from_secs(1), || p.bobber.rect().is_none()));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(dismissals(), vec![DismissReason::Timeout]);
    drop(h);
    // 扩展触发全部关闭：钩子线程退出，浮标窗口销毁、浮标线程退出
    assert!(
        wait_until(Duration::from_secs(2), no_hook_threads),
        "hook threads left after uninstall"
    );
    assert!(
        wait_until(Duration::from_secs(2), || threads_named("wisp-bobber") == 0),
        "bobber thread left after the feature was turned off"
    );
}

/// 以 `own_pid` 作为「Wisp 自己」创建平台（user_apps 不列 Wisp 自己；这里用 0，把本测试进程当成一个普通的第三方应用）
fn platform_as(own_pid: u32) -> Arc<Platform> {
    Platform::create(PlatformConfig {
        own_pid,
        main_thread: Arc::new(|f: Box<dyn FnOnce() + Send>| f()),
        self_hit: Arc::new(|_, _| false),
        keymaps: Arc::new(parking_lot::RwLock::new(Vec::new())),
        os_version: "10.0.26100".to_owned(),
    })
}

/// 本进程里的一个顶层窗口（预定义的 STATIC 类，不用注册窗口类），离开作用域时销毁
struct TestWindow(windows::Win32::Foundation::HWND);

impl TestWindow {
    fn new(title: &str, visible: bool) -> Self {
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, SW_SHOWNOACTIVATE, ShowWindow, WINDOW_EX_STYLE, WS_OVERLAPPEDWINDOW,
        };
        use windows::core::{HSTRING, w};
        // SAFETY: 预定义窗口类；没有父窗口与菜单；句柄由 Drop 销毁
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                &HSTRING::from(title),
                WS_OVERLAPPEDWINDOW,
                40,
                40,
                240,
                120,
                None,
                None,
                None,
                None,
            )
        }
        .expect("CreateWindowExW");
        if visible {
            // SAFETY: 窗口由本线程创建；不激活（不抢 runner 上的前台）
            let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
        }
        Self(hwnd)
    }

    fn handle(&self) -> u64 {
        self.0.0 as usize as u64
    }

    fn minimize(&self) {
        use windows::Win32::UI::WindowsAndMessaging::{SW_SHOWMINNOACTIVE, ShowWindow};
        // SAFETY: 同上
        let _ = unsafe { ShowWindow(self.0, SW_SHOWMINNOACTIVE) };
    }
}

/// 处理本线程的待处理消息：窗口线程久不取消息会被 `IsHungAppWindow` 判为无响应（`prepare_target` 据此拒绝）
fn pump() {
    use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage};
    let mut msg = MSG::default();
    // SAFETY: 只处理本线程的消息队列
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

impl Drop for TestWindow {
    fn drop(&mut self) {
        // SAFETY: 本线程创建的窗口
        let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::DestroyWindow(self.0) };
    }
}

/// 排除名单的应用选择器（`user_apps`）也列出只有隐藏窗口的「后台应用」（最小化到托盘的密码管理器、托盘常驻的
/// 剪贴板工具），`window` 为 None；有可见窗口时走任务栏那一批、带窗口句柄；Wisp 自己不列。预激活与粘贴对隐藏 / 最小化 /
/// 已关闭的目标窗口一律不动（`prepare_target` Err、`paste_into` TargetGone），不会去 SetForegroundWindow 一个看不见的窗口
#[test]
fn user_apps_list_tray_apps_and_paste_skips_hidden_targets() {
    use wisp_platform::PasteError;
    let me_pid = std::process::id();
    let me = std::env::current_exe()
        .expect("current exe")
        .file_name()
        .and_then(|n| n.to_str())
        .expect("exe name")
        .to_lowercase();
    let p = platform_as(0);

    // 1. 只有一个隐藏的顶层窗口（托盘常驻应用的样子）：作为后台应用列出，没有可激活的窗口
    let hidden = TestWindow::new("wisp smoke: tray app", false);
    pump();
    let apps = p.apps.user_apps();
    let entry = apps
        .iter()
        .find(|a| a.id == me)
        .unwrap_or_else(|| panic!("{me} (hidden window only) not in user_apps: {apps:?}"));
    assert_eq!(entry.pid, Some(me_pid));
    assert_eq!(entry.window, None, "后台应用没有可激活的窗口");
    assert!(!entry.name.is_empty());
    // Wisp 自己不列
    assert!(
        platform().apps.user_apps().iter().all(|a| a.id != me),
        "own process must not be listed"
    );

    // 2. 预激活 / 粘贴到隐藏的窗口：不动
    let target = |hwnd: u64| wisp_types::AppRef {
        id: me.clone(),
        alt_id: None,
        name: entry.name.clone(),
        pid: Some(me_pid),
        path: None,
        window: Some(hwnd),
    };
    assert!(p.paste.prepare_target(&target(hidden.handle())).is_err());
    assert!(matches!(
        p.paste.paste_into(&target(hidden.handle()), false),
        Err(PasteError::TargetGone)
    ));

    // 3. 有可见窗口：走任务栏那一批，带窗口句柄（去重：同一个 exe 只出现一次）
    let visible = TestWindow::new("wisp smoke: visible app", true);
    pump();
    let apps = p.apps.user_apps();
    let mine: Vec<_> = apps.iter().filter(|a| a.id == me).collect();
    assert_eq!(mine.len(), 1, "{mine:?}");
    assert_eq!(mine[0].window, Some(visible.handle()));
    assert!(p.paste.prepare_target(&target(visible.handle())).is_ok());

    // 4. 最小化的窗口：同样不动
    visible.minimize();
    pump();
    assert!(p.paste.prepare_target(&target(visible.handle())).is_err());
    assert!(matches!(
        p.paste.paste_into(&target(visible.handle()), false),
        Err(PasteError::TargetGone)
    ));

    // 5. 已关闭的窗口：目标失效
    let closed = visible.handle();
    drop(visible);
    drop(hidden);
    assert!(p.paste.prepare_target(&target(closed)).is_err());
    assert!(matches!(
        p.paste.paste_into(&target(closed), false),
        Err(PasteError::TargetGone)
    ));
}
