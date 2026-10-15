//! macOS 平台冒烟测试。
//!
//! 未标 `#[ignore]` 的用例不需要任何权限、不改动系统状态，CI 的 macOS job 上直接运行（测试线程不是主线程，
//! 主线程专属的 API 会走退路，这里同时验证退路不阻塞、不崩溃）。
//! 标 `#[ignore]` 的用例需要系统权限或会改写系统剪贴板，需要在开发机上手动运行：
//! `cargo test -p wisp-platform --test smoke_macos -- --ignored --nocapture --test-threads=1`
#![cfg(target_os = "macos")]
// 手工验证用例需要把结果打印给运行者看（`--nocapture`）
#![allow(clippy::print_stdout)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use wisp_platform::{Platform, PlatformConfig, ReadLimits, SelectionOpts, WriteOpts, common::secrets_markers};
use wisp_types::{AppRef, ClipPayload, OsKind, PermissionState};

fn platform() -> Arc<Platform> {
    Platform::create(PlatformConfig {
        own_pid: std::process::id(),
        // 测试里没有主线程事件循环：同步执行（闭包会发现自己不在主线程上并走退路）
        main_thread: Arc::new(|f: Box<dyn FnOnce() + Send>| f()),
        self_hit: Arc::new(|_, _| false),
        keymaps: Arc::new(parking_lot::RwLock::new(Vec::new())),
        os_version: "26.4".to_owned(),
    })
}

fn finder() -> AppRef {
    AppRef {
        id: "com.apple.finder".to_owned(),
        alt_id: None,
        name: "Finder".to_owned(),
        pid: None,
        path: Some("/System/Library/CoreServices/Finder.app".to_owned()),
        window: None,
    }
}

/// 测试结束（含断言失败）时把剪贴板恢复成测试前的内容
struct ClipboardGuard {
    platform: Arc<Platform>,
    snapshot: Option<wisp_platform::ClipSnapshot>,
}

impl ClipboardGuard {
    fn new(platform: &Arc<Platform>) -> Self {
        let snapshot = platform
            .clipboard
            .snapshot(64 * 1024 * 1024, Duration::from_secs(2))
            .expect("snapshot")
            .expect("clipboard too large to back up; aborting before touching it");
        Self {
            platform: platform.clone(),
            snapshot: Some(snapshot),
        }
    }

    fn restore(mut self) -> u64 {
        let snap = self.snapshot.take().expect("snapshot");
        self.platform.clipboard.restore(snap).expect("restore")
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        if let Some(snap) = self.snapshot.take() {
            let _ = self.platform.clipboard.restore(snap);
        }
    }
}

fn png_dims(png: &[u8]) -> (u32, u32) {
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "not a PNG");
    let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
    let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
    (w, h)
}

#[test]
fn info_reports_macos_capabilities() {
    let p = platform();
    let info = p.info();
    assert_eq!(info.os, OsKind::Macos);
    assert_eq!(info.os_version, "26.4");
    assert!(info.capabilities.global_hotkeys.available);
    assert!(info.capabilities.clipboard_watch.available);
    for cap in [&info.capabilities.selection, &info.capabilities.capture_overlay] {
        assert!(
            cap.available
                || cap
                    .reason_key
                    .as_deref()
                    .is_some_and(|k| k.starts_with("errors.platform."))
        );
    }
}

#[test]
fn monitors_and_cursor_are_consistent() {
    let p = platform();
    let monitors = p.screen.monitors();
    assert!(!monitors.is_empty(), "at least one display");
    assert_eq!(monitors.iter().filter(|m| m.is_primary).count(), 1);
    assert!(monitors[0].is_primary, "primary first");
    for m in &monitors {
        assert!(
            m.id.parse::<u32>().is_ok(),
            "MonitorId is a CGDirectDisplayID: {}",
            m.id
        );
        assert!(m.frame.width > 0.0 && m.frame.height > 0.0);
        assert!(m.scale_factor >= 1.0);
        assert!(m.work_area.width > 0.0 && m.work_area.width <= m.frame.width);
        assert_eq!(m.physical_size.width, (m.frame.width * m.scale_factor).round() as u32);
    }
    let primary = &monitors[0];
    assert_eq!((primary.frame.x, primary.frame.y), (0.0, 0.0));
    let (id, point) = p.screen.cursor();
    let m = monitors
        .iter()
        .find(|m| m.id == id)
        .expect("cursor is on a known monitor");
    assert!(point.x >= -1.0 && point.x <= m.frame.width + 1.0);
    assert!(point.y >= -1.0 && point.y <= m.frame.height + 1.0);
}

#[test]
fn appearance_queries_do_not_block() {
    let p = platform();
    let started = Instant::now();
    let _ = p.screen.dark_mode();
    let _ = p.screen.reduce_motion();
    let _ = p.screen.reduce_transparency();
    assert_eq!(p.screen.taskbar_light(), None);
    if let Some(accent) = p.screen.system_accent() {
        assert!(accent.len() == 7 && accent.starts_with('#'), "{accent}");
        assert!(accent[1..].chars().all(|c| c.is_ascii_hexdigit()));
    }
    let handle = p.screen.watch_appearance(Box::new(|| {})).expect("watch");
    drop(handle);
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn permissions_status_never_prompts() {
    let p = platform();
    let started = Instant::now();
    let s = p.permissions.status();
    let again = p.permissions.status();
    // 只做本地预检：快、无副作用，连续两次结果一致（弹窗会改变状态或阻塞）
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(s, again);
    // 本进程从未调用 request()：辅助功能与屏幕录制只能是「已授权」或「未询问」，绝不会是 Denied
    // （Denied 只在本进程请求过之后仍未授权时出现）
    for state in [s.accessibility, s.screen_recording] {
        assert!(
            matches!(state, PermissionState::Granted | PermissionState::NotDetermined),
            "{state:?}"
        );
    }
    assert_ne!(s.input_monitoring, PermissionState::NotApplicable);
    assert!(!s.lost_after_update);
    // 与 info() 的能力表一致
    let caps = p.info().capabilities;
    assert_eq!(caps.selection.available, s.accessibility == PermissionState::Granted);
    assert_eq!(
        caps.capture_overlay.available,
        s.screen_recording == PermissionState::Granted
    );
}

#[test]
fn app_icon_and_names() {
    let p = platform();
    let png = p.apps.icon_png(&finder(), 64).expect("Finder icon");
    assert_eq!(png_dims(&png), (64, 64));
    // 第二次命中缓存，结果一致
    assert_eq!(p.apps.icon_png(&finder(), 64).as_deref(), Some(png.as_slice()));
    assert_eq!(png_dims(&p.apps.icon_png(&finder(), 32).expect("32 px")), (32, 32));
    assert!(!p.apps.display_name(&finder()).is_empty());
    let missing = AppRef {
        id: "io.invalid.does-not-exist".to_owned(),
        alt_id: None,
        name: "Ghost".to_owned(),
        pid: None,
        path: None,
        window: None,
    };
    assert_eq!(p.apps.display_name(&missing), "Ghost");
    assert!(p.apps.icon_png(&missing, 32).is_none());
    let apps = p.apps.running_apps();
    assert!(apps.iter().all(|a| !a.id.is_empty() && a.pid.is_some()));
}

#[test]
fn hotkey_env_falls_back_off_main_thread() {
    let p = platform();
    let started = Instant::now();
    // 测试线程不是主线程：CopySymbolicHotKeys / TIS 不能调用，返回空表与默认值，且不阻塞
    let hotkeys = p.hotkey_env.system_hotkeys();
    assert!(
        hotkeys
            .iter()
            .all(|h| wisp_platform::common::combo::canonical(h).as_deref() == Some(h.as_str()))
    );
    let layouts = p.hotkey_env.keyboard_layouts();
    assert!(!layouts.has_altgr);
    assert!(layouts.input_language_count >= 1);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn selection_probes_respect_deadlines() {
    let p = platform();
    let deadline = Duration::from_millis(50);
    if p.permissions.status().accessibility != PermissionState::Granted {
        // CI 上没有辅助功能权限：探测必须立即返回 None（不碰 AX、不等待）。deadline 本身在这里测不到，
        // 有权限的机器上才会走下面的分支
        let started = Instant::now();
        assert_eq!(p.selection.has_selection(deadline), None);
        assert_eq!(p.selection.link_at_cursor(deadline), None);
        assert!(started.elapsed() < Duration::from_millis(20), "{:?}", started.elapsed());
        println!("selection_probes_respect_deadlines: accessibility not granted; deadline path not exercised");
        return;
    }
    // 有权限：每个探测各自受 deadline 约束（预算不足 0.1 s 时不经过 systemWide，直接用前台应用的应用元素，
    // 消息超时设为剩余预算）。余量只留给 AX 超时本身的抖动。
    // 先各探测一次再计时：进程里第一次 AX 调用要先连上辅助功能服务（负载高时约 80 ms，与预算无关），
    // 整个测试二进制一起跑时偶发超出；常驻应用里这一次发生在启动后的空闲预热
    let _ = p.selection.has_selection(deadline);
    let _ = p.selection.link_at_cursor(deadline);
    let slack = Duration::from_millis(30);
    let started = Instant::now();
    let _ = p.selection.has_selection(deadline);
    let has = started.elapsed();
    let started = Instant::now();
    let _ = p.selection.link_at_cursor(deadline);
    let link = started.elapsed();
    assert!(has < deadline + slack, "has_selection took {has:?}");
    assert!(link < deadline + slack, "link_at_cursor took {link:?}");
}

#[test]
fn clipboard_read_is_bounded() {
    // 只读不写：从 pboard 取字节在辅助线程上执行，有硬截止时间（0.5 s），超时返回 Timeout 而不是挂住调用方
    let p = platform();
    let started = Instant::now();
    if let Err(e) = p.clipboard.read(&ReadLimits::default()) {
        assert_eq!(e, wisp_platform::PlatformError::Timeout, "unexpected error: {e}");
    }
    // 截止时间之外只剩本进程内的解码（TIFF 转 PNG 最多几百毫秒）
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "read took {:?}",
        started.elapsed()
    );
}

#[test]
fn clipboard_watch_starts_and_stops_cleanly() {
    // 不改写剪贴板：验证监听在 drop 之后真正停止——取消回调释放了上下文（回调闭包随之释放），之后不会再有任何回调
    let p = platform();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let handle = p
        .clipboard
        .watch(Box::new(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        }))
        .expect("watch");
    assert_eq!(
        Arc::strong_count(&calls),
        2,
        "the callback holds the counter while watching"
    );
    // 跑过至少一个周期（250 ms）
    std::thread::sleep(Duration::from_millis(300));
    drop(handle);
    // 取消回调在 libdispatch 的队列上异步执行：等它释放上下文
    let started = Instant::now();
    while Arc::strong_count(&calls) > 1 {
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "watch context was not released after drop"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    // 上下文已释放，之后两个周期内回调次数不变
    let after_drop = calls.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(calls.load(Ordering::SeqCst), after_drop, "callback fired after drop");
}

// ------------------------------------------------------------------ 需要权限 / 会改写剪贴板

#[test]
#[ignore = "会改写系统剪贴板（测试结束时恢复）"]
fn clipboard_round_trip_and_restore() {
    let p = platform();
    let guard = ClipboardGuard::new(&p);

    // 富文本：纯文本 + HTML + RTF，带 Wisp 标记
    let payload = ClipPayload {
        text: Some("Wisp 冒烟测试".to_owned()),
        html: Some("<b>Wisp</b> 冒烟测试".to_owned()),
        rtf: Some(b"{\\rtf1\\ansi Wisp}".to_vec()),
        ..ClipPayload::default()
    };
    let id = p.paste.write(&payload, WriteOpts::default()).expect("write");
    assert_eq!(p.clipboard.change_id(), id);
    let raw = p.clipboard.read(&ReadLimits::default()).expect("read");
    assert_eq!(raw.change_id, id);
    assert!(raw.from_wisp);
    assert!(!raw.concealed);
    assert_eq!(raw.text.as_deref(), Some("Wisp 冒烟测试"));
    assert!(raw.html.as_deref().is_some_and(|h| h.contains("<b>Wisp</b>")));
    assert!(raw.rtf.is_some());

    // 截断：上限 4 字节
    let limits = ReadLimits {
        max_text_bytes: 4,
        ..ReadLimits::default()
    };
    let raw = p.clipboard.read(&limits).expect("read");
    assert_eq!(raw.text.as_deref(), Some("Wisp"));
    assert!(raw.text_truncated);

    // 「不记录」写入：带 TransientType，监听方读到 concealed
    p.paste
        .write(
            &ClipPayload::text("secret"),
            WriteOpts {
                transient: true,
                exclude_from_history: false,
            },
        )
        .expect("write transient");
    let raw = p.clipboard.read(&ReadLimits::default()).expect("read");
    assert!(raw.concealed && raw.text.is_none());

    // 文件：每个文件一个 item
    let files = vec!["/System/Library/CoreServices/Finder.app".to_owned(), "/tmp".to_owned()];
    let payload = ClipPayload {
        files: Some(files.clone()),
        ..ClipPayload::default()
    };
    p.paste.write(&payload, WriteOpts::default()).expect("write files");
    let raw = p.clipboard.read(&ReadLimits::default()).expect("read");
    let got: Vec<String> = raw
        .files
        .unwrap_or_default()
        .into_iter()
        .map(|f| f.trim_end_matches('/').to_owned())
        .collect();
    assert_eq!(got, files);

    // 图片：PNG（同时写入 TIFF），读回尺寸
    let icon = p.apps.icon_png(&finder(), 48).expect("icon");
    p.paste
        .write(
            &ClipPayload {
                image_png: Some(icon),
                ..ClipPayload::default()
            },
            WriteOpts::default(),
        )
        .expect("write image");
    let raw = p.clipboard.read(&ReadLimits::default()).expect("read");
    let size = raw.image_size.expect("image size");
    assert_eq!((size.width, size.height), (48, 48));

    // 恢复：剪贴板回到测试前的内容
    let restored = guard.restore();
    assert_eq!(p.clipboard.change_id(), restored);
}

/// 恢复后逐类型一致：text + rtf + 自定义类型 → 快照 → 改写 → 恢复，类型集合与恢复前完全相同，
/// 系统不会从 RTF 另外合成一个内容不同的 UTF-16 纯文本
#[test]
#[ignore = "会改写系统剪贴板（测试结束时恢复）"]
fn restore_keeps_the_exact_type_set() {
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardWriting};
    use objc2_foundation::{NSArray, NSData, NSString};

    fn types(pb: &NSPasteboard) -> Vec<String> {
        let mut t: Vec<String> = pb
            .types()
            .map(|a| a.iter().map(|x| x.to_string()).collect())
            .unwrap_or_default();
        t.sort();
        t
    }

    let p = platform();
    let guard = ClipboardGuard::new(&p);
    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    let item = NSPasteboardItem::new();
    let plain = "wisp plain marker";
    // 按原始字节写（多数应用的写法；用 setString 写时系统已经顺带推导出 UTF-16 类型）
    assert!(item.setData_forType(
        &NSData::with_bytes(plain.as_bytes()),
        &NSString::from_str("public.utf8-plain-text")
    ));
    let rtf = b"{\\rtf1\\ansi wisp rtf}";
    assert!(item.setData_forType(&NSData::with_bytes(rtf), &NSString::from_str("public.rtf")));
    assert!(item.setData_forType(
        &NSData::with_bytes(b"custom"),
        &NSString::from_str("io.github.yzlltyyh.wisp.test")
    ));
    let objs: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = vec![ProtocolObject::from_retained(item)];
    assert!(pb.writeObjects(&NSArray::from_retained_slice(&objs)));
    let before = types(&pb);
    let snap = p
        .clipboard
        .snapshot(64 * 1024 * 1024, Duration::from_secs(2))
        .expect("snapshot")
        .expect("small clipboard");
    p.paste
        .write(&ClipPayload::text("overwritten"), WriteOpts::default())
        .expect("write");
    p.clipboard.restore(snap).expect("restore");
    let after = types(&pb);
    println!("before={before:?}\nafter={after:?}");
    assert_eq!(before, after);
    if let Some(utf16) = pb.stringForType(&NSString::from_str("public.utf16-external-plain-text")) {
        assert_eq!(utf16.to_string(), plain, "UTF-16 纯文本必须来自原纯文本，而不是 RTF");
    }
    guard.restore();
}

#[test]
#[ignore = "会改写系统剪贴板（测试结束时恢复）"]
fn clipboard_watch_reports_and_suppresses() {
    let p = platform();
    let guard = ClipboardGuard::new(&p);
    let (tx, rx) = mpsc::channel();
    let handle = p
        .clipboard
        .watch(Box::new(move |id| {
            let _ = tx.send(id);
        }))
        .expect("watch");
    let id = p
        .paste
        .write(&ClipPayload::text("watch me"), WriteOpts::default())
        .expect("write");
    // 跑测试时多半没人碰键盘：轮询可能已按用户空闲放慢到 1 s / 5 s，先让它立即读一次
    p.clipboard.poll_now();
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)).expect("change reported"), id);
    // 抑制区间内的变更不上报
    p.clipboard.suppress(wisp_platform::SuppressRange {
        from_change_id: id + 1,
        ttl: Duration::from_secs(2),
    });
    p.paste
        .write(&ClipPayload::text("suppressed"), WriteOpts::default())
        .expect("write");
    assert!(
        rx.recv_timeout(Duration::from_millis(700)).is_err(),
        "suppressed change leaked"
    );
    // 恢复写入不上报（已见序号直接推进）
    guard.restore();
    assert!(rx.recv_timeout(Duration::from_millis(700)).is_err(), "restore leaked");
    drop(handle);
}

#[test]
#[ignore = "需要辅助功能权限：运行前在任意应用里选中一段文字并保持该应用在前台"]
fn selection_from_frontmost_app() {
    let p = platform();
    let target = p.focus.frontmost();
    println!("frontmost: {target:?}");
    let before = p.clipboard.change_id();
    let result = p.selection.get(&SelectionOpts {
        target,
        ..SelectionOpts::default()
    });
    println!("selection: {result:?}");
    let sel = result.expect("selection");
    assert!(!sel.text.trim().is_empty());
    // 回退路径恢复了剪贴板（序号前进了，但内容一致由人工确认）
    println!(
        "method={} elapsed={}ms change {before} -> {}",
        sel.method.as_str(),
        sel.elapsed_ms,
        p.clipboard.change_id()
    );
}

#[test]
fn concealed_type_constants_match_nspasteboard_org() {
    // 写入时使用的「不记录」类型必须在读取侧的隐私列表里，否则 Wisp 自己的临时写入会进历史
    assert!(secrets_markers::MACOS_CONCEALED_TYPES.contains(&"org.nspasteboard.TransientType"));
    assert!(secrets_markers::MACOS_CONCEALED_TYPES.contains(&"org.nspasteboard.ConcealedType"));
}

/// 扩展触发（`mouse-hook` feature）
#[cfg(feature = "mouse-hook")]
mod hooks {
    use wisp_platform::{HookSpec, InputSignal};

    use super::*;

    #[test]
    fn empty_hook_spec_installs_nothing() {
        let p = platform();
        let handle = p.hooks.install(HookSpec::default(), Box::new(|_| {})).expect("noop");
        drop(handle);
        assert_eq!(p.bobber.rect(), None);
        // 鼠标规格里一个键都没绑定：等于没有鼠标项，不建 tap、不要求权限（CI 上没有辅助功能权限也成功）
        let unbound = HookSpec {
            mouse: Some(wisp_platform::MouseSpec {
                long_press: true,
                middle: None,
                back: None,
                forward: None,
            }),
            ..HookSpec::default()
        };
        let handle = p
            .hooks
            .install(unbound, Box::new(|_| {}))
            .expect("unbound mouse spec is a no-op");
        drop(handle);
    }

    #[test]
    #[ignore = "需要输入监控权限：运行后 5 秒内连按两次右 ⌥"]
    fn double_tap_right_option() {
        let p = platform();
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let spec = HookSpec {
            double_tap: Some(wisp_platform::DoubleTapSpec {
                key: wisp_types::ModifierKey::RightAlt,
                interval: Duration::from_millis(300),
            }),
            ..HookSpec::default()
        };
        let handle = p
            .hooks
            .install(
                spec,
                Box::new(move |s| {
                    if s == InputSignal::DoubleTap {
                        h.fetch_add(1, Ordering::Relaxed);
                    }
                }),
            )
            .expect("install");
        std::thread::sleep(Duration::from_secs(5));
        drop(handle);
        assert!(hits.load(Ordering::Relaxed) >= 1, "no double tap detected");
    }

    #[test]
    #[ignore = "需要辅助功能权限：运行后 5 秒内在其他应用里拖选一段文字"]
    fn bobber_candidate_on_drag_select() {
        let p = platform();
        let (tx, rx) = mpsc::channel();
        let handle = p
            .hooks
            .install(
                HookSpec {
                    bobber: true,
                    ..HookSpec::default()
                },
                Box::new(move |s| {
                    let _ = tx.send(s);
                }),
            )
            .expect("install");
        let signal = rx.recv_timeout(Duration::from_secs(5)).expect("candidate");
        println!("signal: {signal:?}");
        assert!(matches!(signal, InputSignal::BobberCandidate { .. }));
        std::thread::sleep(wisp_platform::common::gesture::BOBBER_PROBE_DELAY);
        println!(
            "has_selection: {:?}",
            p.selection
                .has_selection(wisp_platform::common::gesture::BOBBER_PROBE_DEADLINE)
        );
        drop(handle);
    }
}
