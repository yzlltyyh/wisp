//! 启动顺序（目标：冷启动到托盘就绪 ≤ 800 ms）与运行事件处理。
//!
//! 每个阶段都以「自进程启动起的毫秒数」记入 `PerfMarks::record_startup` 并写一行 `startup …` 日志，
//! 冒烟报告与 `latency-report.mjs` 都读它。托盘就绪之后的工作（Panel 预热、剪贴板缓存预加载、Overlay 预建、更新检查）
//! 全部异步进行，不拖慢托盘出现。

use std::collections::HashSet;
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use tauri::{Listener, Manager};
use wisp_platform::{Platform, PlatformConfig};
use wisp_types::{WindowTarget, event};

use crate::app::{autostart, logging, paths::Paths, perf::PerfMarks, runtime, second_instance, smoke};
use crate::assets::AssetRegistry;
use crate::bus::{Bus, Command};
use crate::clipboard_svc::ClipboardService;
use crate::hotkeys::HotkeyService;
use crate::orchestrator::Orchestrator;
use crate::settings::SettingsService;
use crate::settings::watcher::SettingsWatcher;
use crate::state::{AppState, Deps};
use crate::sync_svc::SyncService;
use crate::windows::WindowManager;
use crate::windows::settings_window::{ONBOARDING_LABEL, SETTINGS_LABEL};

/// Panel 预热完成后、做下一件空闲工作之前的间隔（避开启动时的 CPU 高峰）
const IDLE_GAP: Duration = Duration::from_millis(400);
/// 面板窗口建好之后等页面加载完成（`panel_page_ready` 启动阶段）的上限
const PAGE_READY_WAIT: Duration = Duration::from_secs(10);

/// 配置文件监听（常驻；进程退出时随之释放）
static WATCHER: Mutex<Option<SettingsWatcher>> = Mutex::new(None);
/// 已经见过的固定窗口（托盘「显示固定窗口」的显隐依据）
static PINNED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Tauri setup：构造全部服务并 `manage(AppState)`，然后建托盘
pub fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    // setup 运行在事件循环所在的主线程上（更新器据此决定 cleanup 是直接执行还是派发到主线程）
    runtime::mark_main_thread();
    let handle = app.handle().clone();
    let smoke_mode = smoke::enabled();
    let lab_mode = window_lab_mode();
    let perf = Arc::new(PerfMarks::new());
    let stage = |name: &'static str| {
        let t = runtime::since_process_start();
        perf.record_startup(name, t);
        log::info!("startup stage={name} elapsed_ms={}", t.as_millis());
    };
    log::info!(
        "wisp {} starting os={} {} arch={} debug={} smoke={smoke_mode}",
        handle.package_info().version,
        std::env::consts::OS,
        runtime::os_version(),
        std::env::consts::ARCH,
        cfg!(debug_assertions),
    );
    // 预检结论（preflight_env 运行时日志插件还没装好，在这里补记）。`launch_env=` 只在 macOS 上有：启动环境（Info.plist
    // LSEnvironment / LaunchAgent）是否生效，从终端直接执行二进制时为 false
    let launch_env = super::autostart::launch_env_applied().map_or_else(String::new, |on| format!(" launch_env={on}"));
    log::info!(
        "preflight transparent_windows={} wayland={} shortcut_plugin={} hotkey_backend={}{launch_env}",
        runtime::transparent_windows(),
        runtime::is_wayland_session(),
        runtime::global_shortcut_supported(),
        runtime::hotkey_backend(),
    );
    if runtime::x11_hotkeys_via_plugin() {
        log::warn!(
            "{}=plugin：Linux 全局快捷键改用 tauri-plugin-global-shortcut（运行期回退；常驻一个每 50 ms 轮询一次的线程）",
            runtime::X11_HOTKEYS_ENV
        );
    }

    // 1. rustls 的 CryptoProvider（wisp-llm 内部另有兜底）
    let _ = rustls::crypto::ring::default_provider().install_default();

    // 2. 路径与设置（损坏时由 SettingsService 备份为 settings.json.bak-<ts> 并改用默认值）。冒烟模式与 window lab 用隔离
    //    目录与内存钥匙串
    if smoke_mode {
        smoke::discard_stale_report();
    }
    let paths = if smoke_mode {
        Paths::smoke()
    } else if lab_mode {
        Paths::window_lab()
    } else {
        Paths::resolve()?
    };
    if let Err(e) = paths.ensure_dirs() {
        log::error!("cannot create app directories: {e}");
    }
    let settings = if smoke_mode {
        let s = SettingsService::load_isolated(&paths);
        smoke::seed(&s);
        s
    } else if lab_mode {
        log::info!(
            "window lab: isolated config / data under {}",
            paths.config_dir.display()
        );
        SettingsService::load_isolated(&paths)
    } else {
        SettingsService::load(&paths)?
    };
    let initial = settings.get();
    logging::apply_level(initial.advanced.log_level);
    stage("settings_loaded");

    // 3. macOS：ActivationPolicy::Accessory（settings / onboarding 打开时由 WindowManager 临时切到 Regular）
    crate::windows::native::set_regular_activation(&handle, false);

    // 4. Platform（self_hit 用 OnceLock 延迟绑定到 WindowManager；键位表与 reactor 共享，设置变化时原地替换）
    let keymaps = Arc::new(RwLock::new(super::merged_keymaps(&initial.hotkeys.app_keymaps)));
    let wm_slot: Arc<OnceLock<Weak<WindowManager>>> = Arc::new(OnceLock::new());
    let platform = {
        let main_handle = handle.clone();
        let slot = Arc::clone(&wm_slot);
        Platform::create(PlatformConfig {
            own_pid: std::process::id(),
            main_thread: Arc::new(move |f: Box<dyn FnOnce() + Send>| {
                if let Err(e) = main_handle.run_on_main_thread(f) {
                    log::error!("run_on_main_thread failed: {e}");
                }
            }),
            self_hit: Arc::new(move |monitor, p| {
                slot.get()
                    .and_then(Weak::upgrade)
                    .is_some_and(|wm| wm.self_hit(monitor, p))
            }),
            keymaps: Arc::clone(&keymaps),
            os_version: runtime::os_version().to_owned(),
        })
    };

    // 5. Deps → 各服务 new(&deps) → 绑定 self_hit → AppState
    let (bus, bus_rx) = Bus::new();
    let deps = Deps {
        app: handle.clone(),
        settings: Arc::clone(&settings),
        platform: Arc::clone(&platform),
        bus: bus.clone(),
        perf: Arc::clone(&perf),
        assets: Arc::new(AssetRegistry::new()),
    };
    let windows = WindowManager::new(&deps);
    let _ = wm_slot.set(Arc::downgrade(&windows));
    let orchestrator = Orchestrator::new(&deps);
    let clipboard = ClipboardService::new(&deps);
    let hotkeys = HotkeyService::new(&deps);
    // 多设备同步：构造不做任何 I/O（同步关闭时也不打开任何资源）
    let sync = SyncService::new(&deps);
    let llm = wisp_llm::LlmClient::new(wisp_llm::ClientConfig {
        user_agent: format!("Wisp/{}", handle.package_info().version),
        caps: settings.caps_store(),
    });
    app.manage(AppState {
        settings: Arc::clone(&settings),
        platform,
        llm,
        windows: Arc::clone(&windows),
        orchestrator: Arc::clone(&orchestrator),
        clipboard: Arc::clone(&clipboard),
        hotkeys: Arc::clone(&hotkeys),
        sync: Arc::clone(&sync),
        perf: Arc::clone(&perf),
        assets: deps.assets,
        bus: bus.clone(),
    });
    Arc::clone(&orchestrator).spawn(bus_rx);
    stage("services");

    // 6. 托盘 → 托盘就绪（Linux 没有 AppIndicator 库、或会话里没有 StatusNotifierWatcher 时退化为打开设置窗口 + 通知；
    //    后者在托盘建好后异步确认，见 tray::build）。托盘先按「权限未知」建好，权限查询在后台进行，结果出来后刷新
    crate::tray::build(&handle)?;
    stage("tray_ready");
    // 同步：已开启时在后台打开本机副本、安装改动捕获、做启动核对；不联网、不读钥匙串
    forward_sync_status(&handle, &sync);
    sync.start_local();
    // macOS 的本地化应用菜单（替换 Tauri 的英文默认菜单）
    super::app_menu::install(&handle, crate::tray::menu::current_lang(&handle));

    // 7. 快捷键（插件未注册时 HotkeyService 直接报告 unavailable）
    let statuses = hotkeys.register_all();
    log::info!("hotkeys registered count={}", statuses.len());

    // 8. 剪贴板监听（不打开数据库）
    if let Err(e) = clipboard.start() {
        log::info!("clipboard watch not started: {}", e.message);
    }

    // 设置变化的统一副作用、配置文件热加载、系统外观监听、托盘跟随快捷键状态、权限基线
    super::reactor::spawn(&handle, keymaps);
    match SettingsWatcher::start(&settings) {
        Ok(w) => *WATCHER.lock() = Some(w),
        Err(e) => log::warn!("settings hot reload unavailable: {}", e.message),
    }
    super::appearance::watch(&handle);
    {
        let h = handle.clone();
        handle.listen_any(event::HOTKEYS_CHANGED, move |_| crate::tray::refresh(&h));
    }
    super::permissions::refresh(&handle);
    stage("services_started");

    // 9. 异步预热 Panel；完成后在空闲时刻依次：剪贴板缓存预加载、Overlay 预建（是否预建由 WindowManager 按平台决定）、
    //    编排器的空闲预热（例如 Windows 的 UIA 冷启动）。这几步都是阻塞调用（创建 WebView 时等主线程、
    //    打开 SQLite 并加载缓存、建 UIA 对象），放到阻塞线程池，不占 tokio worker
    {
        let windows = Arc::clone(&windows);
        let clipboard = Arc::clone(&clipboard);
        let orchestrator = Arc::clone(&orchestrator);
        let perf = Arc::clone(&perf);
        tauri::async_runtime::spawn(async move {
            let w = Arc::clone(&windows);
            let prewarmed = tauri::async_runtime::spawn_blocking(move || w.prewarm_panel()).await;
            match prewarmed {
                Ok(Ok(())) => {
                    perf.record_startup("panel_prewarmed", runtime::since_process_start());
                    log::info!(
                        "startup stage=panel_prewarmed elapsed_ms={}",
                        runtime::since_process_start().as_millis()
                    );
                    // 窗口建好之后页面还要 100–160 ms 才加载完、外壳挂载：这时第一次唤出才不会撞上 ready 超时（冒烟据此
                    // 开始首次翻译，报告的 panel_prewarm_ms 也取这一刻）
                    let mut loaded = windows.panel_loaded();
                    let page_ready = matches!(
                        tokio::time::timeout(PAGE_READY_WAIT, loaded.wait_for(|n| *n > 0)).await,
                        Ok(Ok(_))
                    );
                    if page_ready {
                        perf.record_startup("panel_page_ready", runtime::since_process_start());
                        log::info!(
                            "startup stage=panel_page_ready elapsed_ms={}",
                            runtime::since_process_start().as_millis()
                        );
                    } else {
                        log::warn!("panel page did not load within {PAGE_READY_WAIT:?}");
                    }
                    tokio::time::sleep(IDLE_GAP).await;
                    if let Err(e) = tauri::async_runtime::spawn_blocking(move || clipboard.preload_cache()).await {
                        log::warn!("clipboard cache preload failed: {e}");
                    }
                    tokio::time::sleep(IDLE_GAP).await;
                    match tauri::async_runtime::spawn_blocking(move || windows.prewarm_overlay()).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => log::debug!("overlay prewarm skipped: {}", e.message),
                        Err(e) => log::warn!("overlay prewarm failed: {e}"),
                    }
                    tokio::time::sleep(IDLE_GAP).await;
                    if let Err(e) = tauri::async_runtime::spawn_blocking(move || orchestrator.idle_prewarm()).await {
                        log::warn!("orchestrator idle prewarm failed: {e}");
                    }
                }
                Ok(Err(e)) => {
                    perf.record_startup("panel_prewarm_failed", runtime::since_process_start());
                    log::info!("panel prewarm skipped: {}", e.message);
                }
                Err(e) => {
                    perf.record_startup("panel_prewarm_failed", runtime::since_process_start());
                    log::warn!("panel prewarm failed: {e}");
                }
            }
        });
    }

    // 10. 启动 30 s 后检查更新（一次性，不设常驻定时器；off / debug / 冒烟模式不检查）
    crate::updater::schedule_startup_check(&handle);

    // 11. 重启续接优先：`app_relaunch(resume)` 留下的窗口在 5 分钟内打开一次，不论怎样拉起的（重启沿用原来的
    //     启动参数，可能带 --autostart）。否则首次运行打开引导；关闭了「静默启动」时打开设置。由开机自启拉起
    //     （--autostart）或带 --trigger 时都不弹窗；window lab 的隔离目录是全新配置，不弹引导（实验台自己开关设置窗口并
    //     清点窗口数）
    if !smoke_mode && !lab_mode {
        let launch = second_instance::parse_args(&std::env::args().collect::<Vec<_>>());
        let quiet = matches!(
            launch,
            second_instance::LaunchRequest::Autostart
                | second_instance::LaunchRequest::Trigger(_)
                | second_instance::LaunchRequest::UnknownAction(_)
        );
        if let Some(target) = super::relaunch::take(&settings) {
            bus.send(Command::OpenWindow(target));
        } else if !quiet {
            if settings.needs_onboarding() {
                bus.send(Command::OpenWindow(WindowTarget::Onboarding { step: None }));
            } else if !initial.general.start_hidden {
                bus.send(Command::OpenWindow(WindowTarget::Settings {
                    section: None,
                    anchor: None,
                }));
            }
        }
        second_instance::handle_launch_args(&handle);
    }

    // 12. 同步开机自启（debug 构建 / 冒烟模式 / App Translocation 下跳过，见 autostart.rs）
    autostart::sync(&handle, initial.general.launch_at_login);

    // 13. 冒烟模式
    if smoke_mode {
        smoke::start(&handle);
    }
    stage("setup_done");
    Ok(())
}

/// window lab 是否开启（只在 debug / fake-llm 构建里编译，发布版恒 false）
fn window_lab_mode() -> bool {
    #[cfg(any(debug_assertions, feature = "fake-llm"))]
    {
        crate::windows::window_lab::enabled()
    }
    #[cfg(not(any(debug_assertions, feature = "fake-llm")))]
    {
        false
    }
}

/// 把同步状态的每次变化（状态转换与节流后的进度）转发给设置窗口（`sync:status`）。托盘只在它显示的内容
/// （「同步需要处理」）变化时刷新，进度的节拍不重建菜单。任务只在状态变化时被唤醒，没有轮询
fn forward_sync_status(app: &tauri::AppHandle, sync: &Arc<SyncService>) {
    use tauri::Emitter;

    let mut rx = sync.subscribe_status();
    rx.borrow_and_update();
    let sync = Arc::clone(sync);
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut attention = sync.needs_attention();
        while rx.changed().await.is_ok() {
            let status = rx.borrow_and_update().clone();
            // 设置窗口没开时 emit_to 找不到目标，只是一次查找
            if let Err(e) = app.emit_to(SETTINGS_LABEL, event::SYNC_STATUS, &status) {
                log::debug!("emit sync:status failed: {e}");
            }
            let now = sync.needs_attention();
            if now != attention {
                attention = now;
                crate::tray::refresh(&app);
            }
        }
    });
}

/// 运行事件：
/// - 最后一个窗口关闭（ExitRequested 且没有 exit code）时 prevent_exit，保持常驻；`request_restart` 发起的退出
///   （code = `RESTART_EXIT_CODE`）告诉更新器，以便退出时安装暂存的更新后重新打开；
/// - 退出时写掉待写的设置（能力缓存、state.json），Windows 上安装暂存的更新；
/// - macOS 再次打开应用（Finder / 启动台 / Spotlight / Dock，`RunEvent::Reopen`）：与二次启动相同，打开或聚焦设置；
/// - 设置 / 引导窗口获焦：刷新权限、惰性检查更新、发起一轮同步（有节流），state.json 被重置过时核对钥匙串索引；
///   引导窗口关闭：停止权限轮询；
/// - 固定窗口出现 / 销毁：刷新托盘的「显示固定窗口」；
/// - 窗口获焦 / 失焦 / 销毁：告诉更新器「有动静」（Windows 等待空闲安装时重新计时）。
pub fn on_run_event(app: &tauri::AppHandle, ev: tauri::RunEvent) {
    use tauri::RunEvent;
    match ev {
        RunEvent::ExitRequested { api, code: None, .. } => api.prevent_exit(),
        RunEvent::ExitRequested { code: Some(code), .. } => {
            if code == tauri::RESTART_EXIT_CODE {
                crate::updater::note_restart_requested();
            }
        }
        RunEvent::Exit => {
            if let Some(state) = app.try_state::<AppState>() {
                // 悬浮剪贴板还开着：退出不经过窗口的 Destroyed，位置与大小要在写盘之前记下
                state.windows.float_save_on_exit();
                state.settings.flush();
                // 同步：把还在写入队列里的本机改动写进本机副本（只写本地，最多 200 ms）
                state.sync.shutdown_flush();
                // Linux：仍持有剪贴板时交给剪贴板管理器保存（其他平台为空操作）
                state.platform.shutdown();
            }
            log::info!("exit");
            crate::updater::on_exit(app);
        }
        RunEvent::WindowEvent { label, event, .. } => on_window_event(app, &label, &event),
        // 高频或无关的事件直接忽略：MainEventsCleared 每轮事件循环都有，不能走下面按名字识别的路径
        RunEvent::MainEventsCleared
        | RunEvent::Ready
        | RunEvent::Resumed
        | RunEvent::WebviewEvent { .. }
        | RunEvent::MenuEvent(_)
        | RunEvent::TrayIconEvent(_) => {}
        other => {
            if crate::windows::native::is_reopen(&other) {
                second_instance::on_reopen(app);
            }
        }
    }
}

fn on_window_event(app: &tauri::AppHandle, label: &str, event: &tauri::WindowEvent) {
    match event {
        tauri::WindowEvent::Focused(true) if label == SETTINGS_LABEL || label == ONBOARDING_LABEL => {
            super::permissions::refresh(app);
            crate::updater::maybe_check(app);
            if let Some(state) = app.try_state::<AppState>() {
                state.settings.verify_key_index();
                // 同步：打开设置正是要改东西的时候，先取回其他设备的改动（节流在服务里判断）
                state.sync.trigger(crate::sync_svc::CycleReason::SettingsFocused);
            }
        }
        tauri::WindowEvent::Destroyed if label == ONBOARDING_LABEL => super::permissions::stop_watch(),
        _ => {}
    }
    // 获焦 / 失焦 / 销毁是用户活动（Windows 等待空闲安装更新时据此重新计时；没有在等时只是一次原子读）。
    // Moved / Resized 拖动时可达 60–120 Hz，不算
    if matches!(event, tauri::WindowEvent::Focused(_) | tauri::WindowEvent::Destroyed) {
        crate::updater::note_activity();
    }
    if label.starts_with("pinned-") && pinned_set_changed(label, matches!(event, tauri::WindowEvent::Destroyed)) {
        crate::tray::refresh(app);
    }
}

/// 记录固定窗口的出现 / 销毁，返回集合是否变化。拖动固定窗口时每个 Moved / Resized 事件都会走到这里：已经见过的窗口
/// 只做一次查找，不分配
fn pinned_set_changed(label: &str, destroyed: bool) -> bool {
    let mut pinned = PINNED.lock();
    let set = pinned.get_or_insert_with(HashSet::new);
    if destroyed {
        set.remove(label)
    } else if set.contains(label) {
        false
    } else {
        set.insert(label.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_reopen_counts_as_reopen() {
        assert!(!crate::windows::native::is_reopen(&tauri::RunEvent::Ready));
        assert!(!crate::windows::native::is_reopen(&tauri::RunEvent::MainEventsCleared));
        assert!(!crate::windows::native::is_reopen(&tauri::RunEvent::Exit));
    }

    #[test]
    fn pinned_windows_are_tracked_once() {
        let label = "pinned-test-tracking";
        assert!(pinned_set_changed(label, false), "第一次出现");
        for _ in 0..100 {
            assert!(!pinned_set_changed(label, false), "拖动时的后续事件不算变化");
        }
        assert!(pinned_set_changed(label, true), "销毁");
        assert!(!pinned_set_changed(label, true));
    }
}
