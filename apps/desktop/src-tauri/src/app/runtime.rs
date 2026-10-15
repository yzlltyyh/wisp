//! tokio 运行时与创建 WebView 之前的环境预检。
//! 本模块不使用 `cfg(target_os)`，平台差异按 `std::env::consts::OS` 在运行时判断。

use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

/// 进程启动的时刻：内核记录的进程创建时刻（`wisp_platform::process::started_at`），换算到单调时钟。在
/// `preflight_env`（`run()` 的第一条语句）里确定；取不到内核时刻时退回那一刻
static PROCESS_START: OnceLock<Instant> = OnceLock::new();
/// 进程创建到 `run()` 的耗时（exec、dyld、静态初始化；首次运行新二进制时还有 Gatekeeper 扫描）；取不到时为 None
static PRE_MAIN: OnceLock<Option<Duration>> = OnceLock::new();
/// 内核时刻与「现在」之差超过这么多就不可信（系统时钟被调过），退回 `run()` 的时刻
const PRE_MAIN_SANITY: Duration = Duration::from_secs(120);

/// 自进程启动（内核创建进程）以来的耗时（冷启动埋点用；没经过 `preflight_env` 的测试里从首次调用起算）
pub fn since_process_start() -> Duration {
    PROCESS_START.get_or_init(Instant::now).elapsed()
}

/// 进程创建到 `run()` 第一条语句的耗时（冒烟报告的 `startup.pre_main_ms`）；没经过 `preflight_env` 或取不到时为 None
pub fn pre_main() -> Option<Duration> {
    PRE_MAIN.get().copied().flatten()
}

/// 记录进程启动时刻：内核时刻可信时 = 现在 − (现在 − 内核创建时刻)
fn mark_process_start() {
    let now = Instant::now();
    let pre_main = wisp_platform::process::started_at()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .filter(|d| *d < PRE_MAIN_SANITY);
    let start = pre_main.and_then(|d| now.checked_sub(d)).unwrap_or(now);
    let _ = PROCESS_START.set(start);
    let _ = PRE_MAIN.set(pre_main);
}

/// 主线程（事件循环所在线程）的 id，由 `bootstrap::setup` 记下
static MAIN_THREAD: OnceLock<std::thread::ThreadId> = OnceLock::new();

/// 记下当前线程为主线程（`bootstrap::setup` 里调用；setup 运行在事件循环所在的主线程上）
pub fn mark_main_thread() {
    let _ = MAIN_THREAD.set(std::thread::current().id());
}

/// 当前是否在主线程上（还没记录时为 false）。需要「在主线程上同步执行、否则派发过去并等待」的地方据此避免自锁
pub fn on_main_thread() -> bool {
    MAIN_THREAD.get() == Some(&std::thread::current().id())
}

/// 构建常驻的 tokio 运行时：2 个 worker、enable_all、线程名 `wisp-rt`。
/// Runtime 被 `Box::leak` 常驻，返回其 Handle 交给 `tauri::async_runtime::set`。
pub fn build_runtime() -> tokio::runtime::Handle {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("wisp-rt")
        .build()
        .expect("创建 tokio 运行时失败");
    let handle = rt.handle().clone();
    Box::leak(Box::new(rt));
    handle
}

/// `preflight_env` 的结论：窗口能否透明（None = 还没预检，按 true 处理）
static TRANSPARENT_WINDOWS: OnceLock<bool> = OnceLock::new();

/// 创建任何 WebView 之前的环境预检。**必须是 `run()` 的第一步**（早于 `build_runtime`）：
/// 这里会 `std::env::set_var`（edition 2024 中为 unsafe），只有在进程还是单线程时才可靠。
/// - Linux 检测到 Nvidia 专有驱动时设置 `WEBKIT_DISABLE_DMABUF_RENDERER=1`（用户已设置的值不覆盖）；
/// - Wayland 会话、或用户设置了 `WEBKIT_DISABLE_COMPOSITING_MODE`（关闭合成后透明不可用）时**关闭窗口透明**：
///   结论由 [`transparent_windows`] 提供，`windows::native::template_overrides` 据此把 panel / pinned / overlay
///   建成不透明窗口，`WindowManager::boot_for` 把 `Boot.shadow_margin` 置 0（卡片铺满窗口、纯色、直角）。
pub fn preflight_env() {
    // 冷启动埋点的起点（托盘就绪 ≤ 800 ms 从内核创建进程算起）
    mark_process_start();
    if std::env::consts::OS != "linux" {
        let _ = TRANSPARENT_WINDOWS.set(true);
        return;
    }
    let nvidia = std::path::Path::new("/proc/driver/nvidia/version").exists();
    if nvidia && std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        // SAFETY: preflight_env 是 run() 的第一条语句，早于 tokio 运行时、Tauri Builder 与任何插件 / WebView，
        // 此时进程里只有主线程，没有其他线程在读写环境变量
        unsafe { std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1") };
    }
    let compositing_disabled = std::env::var_os("WEBKIT_DISABLE_COMPOSITING_MODE").is_some_and(|v| !v.is_empty());
    // 此时日志插件还没装好，结论由 bootstrap 在 setup 里按 transparent_windows() 记一条 info
    let _ = TRANSPARENT_WINDOWS.set(!is_wayland_session() && !compositing_disabled);
}

/// 窗口能否透明：macOS / Windows 恒为 true；Linux 由 [`preflight_env`] 决定（Wayland 或关闭合成时为 false）
pub fn transparent_windows() -> bool {
    TRANSPARENT_WINDOWS.get().copied().unwrap_or(true)
}

/// 操作系统版本（例如 macOS `26.4`、Windows `10.0.26100`、Ubuntu `24.04`）的**唯一来源**：`Boot.os_version` 与
/// `PlatformInfo.os_version`（经 `PlatformConfig.os_version` 传给 wisp-platform）都取这里，前后端看到的值一致。
/// 首次调用时计算并缓存（setup 一开始就会调用，处在冷启动的关键路径上）：
/// - macOS / Windows：`tauri_plugin_os::version()`（读 SystemVersion.plist / RtlGetVersion，微秒级）；
/// - Linux：直接解析 os-release（[`os_release_version`]）。`tauri_plugin_os` 在 Linux 上会先起 `lsb_release` 子进程
///   （Ubuntu 上是 Python 脚本，50–200 ms），不能放在托盘就绪之前
pub fn os_version() -> &'static str {
    static OS_VERSION: OnceLock<String> = OnceLock::new();
    OS_VERSION.get_or_init(|| {
        if std::env::consts::OS == "linux" {
            ["/etc/os-release", "/usr/lib/os-release"]
                .iter()
                .find_map(|p| std::fs::read_to_string(p).ok())
                .and_then(|text| os_release_version(&text))
                .unwrap_or_else(|| "unknown".to_owned())
        } else {
            tauri_plugin_os::version().to_string()
        }
    })
}

/// 从 os-release 的内容里取版本：`VERSION_ID`，没有时取 `BUILD_ID`（Arch 等滚动发行版为 `rolling`）；去掉引号，空值视为没有
pub fn os_release_version(text: &str) -> Option<String> {
    let field = |key: &str| {
        text.lines()
            .find_map(|line| line.trim().strip_prefix(key)?.strip_prefix('='))
            .map(|v| v.trim().trim_matches(['"', '\'']).trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    field("VERSION_ID").or_else(|| field("BUILD_ID"))
}

/// 是否为 Wayland 会话（Wayland 下窗口不透明、全局快捷键等能力降级）
pub fn is_wayland_session() -> bool {
    std::env::consts::OS == "linux"
        && (std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
            || std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v.eq_ignore_ascii_case("wayland")))
}

/// Linux X11 全局快捷键后端的运行期回退的环境变量
pub const X11_HOTKEYS_ENV: &str = "WISP_X11_HOTKEYS";

/// 环境变量的值是否要求切回 tauri-plugin-global-shortcut（`plugin`，不区分大小写、忽略首尾空白）
pub fn wants_plugin_hotkeys(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.trim().eq_ignore_ascii_case("plugin"))
}

/// Linux X11 是否用 tauri-plugin-global-shortcut 注册全局快捷键（**运行期回退**，默认否）。
///
/// 默认用平台层的事件驱动 `XGrabKey`（`wisp_platform::HotkeyEnv::key_grab`，空闲零唤醒）；环境变量
/// `WISP_X11_HOTKEYS=plugin` 切回插件（global-hotkey 0.8 的 X11 后端：功能等价，但常驻一个每 50 ms 轮询一次的线程）——
/// 给平台层后端在某个桌面环境上出问题时的用户与排障留一条不用换版本的退路。启动时读一次（之后不变）
pub fn x11_hotkeys_via_plugin() -> bool {
    static VIA_PLUGIN: OnceLock<bool> = OnceLock::new();
    *VIA_PLUGIN.get_or_init(|| {
        std::env::consts::OS == "linux" && wants_plugin_hotkeys(std::env::var(X11_HOTKEYS_ENV).ok().as_deref())
    })
}

/// 是否注册 tauri-plugin-global-shortcut：macOS / Windows 是；Linux 默认**不注册**，
/// 只有 `WISP_X11_HOTKEYS=plugin` 且是 X11 会话（有 DISPLAY、不是 Wayland）时才注册。
///
/// global-hotkey 0.8 的 X11 后端在自己的线程里每 50 ms 轮询一次（常驻 20 次 / 秒唤醒，违背空闲零唤醒的目标），而且插件一
/// 注册就会起这个线程——哪怕一个快捷键都没注册。Linux X11 会话默认用平台层的事件驱动 `XGrabKey`
/// （`wisp_platform::HotkeyEnv::key_grab`，HotkeyService 自动选用）；Wayland 会话（含 XWayland）本来就不注册
/// （Wayland 下全局快捷键要经 portal），没有 DISPLAY 时插件的 `GlobalHotKeyManager::new()` 会让 build() 失败。
/// lib.rs 按它决定是否注册插件
pub fn global_shortcut_supported() -> bool {
    std::env::consts::OS != "linux"
        || (x11_hotkeys_via_plugin()
            && !is_wayland_session()
            && std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty()))
}

/// 预检日志里的全局快捷键后端：`plugin`（tauri-plugin-global-shortcut）/ `x11_grab`（平台层 XGrabKey，Linux 默认）/
/// `none`（Wayland、没有 X 显示）
pub fn hotkey_backend() -> &'static str {
    if global_shortcut_supported() {
        "plugin"
    } else if !is_wayland_session() && std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty()) {
        "x11_grab"
    } else {
        "none"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_release_version_prefers_version_id() {
        let ubuntu = "NAME=\"Ubuntu\"\nVERSION=\"24.04.1 LTS (Noble Numbat)\"\nVERSION_ID=\"24.04\"\nID=ubuntu\n";
        assert_eq!(os_release_version(ubuntu).as_deref(), Some("24.04"));
        let fedora = "NAME=Fedora Linux\nVERSION_ID=41\nVERSION_CODENAME=\"\"\n";
        assert_eq!(os_release_version(fedora).as_deref(), Some("41"));
        // Arch：没有 VERSION_ID，用 BUILD_ID
        let arch = "NAME=\"Arch Linux\"\nID=arch\nBUILD_ID=rolling\n";
        assert_eq!(os_release_version(arch).as_deref(), Some("rolling"));
        // 前缀相同的其他键不会被误认（VERSION_ID_LIKE 之类）、空值视为没有
        assert_eq!(os_release_version("VERSION_IDX=9\nVERSION_ID=''\n"), None);
        assert_eq!(os_release_version(""), None);
    }

    #[test]
    fn plugin_hotkeys_only_on_explicit_request() {
        assert!(wants_plugin_hotkeys(Some("plugin")));
        assert!(wants_plugin_hotkeys(Some(" Plugin\n")));
        assert!(!wants_plugin_hotkeys(Some("native")));
        assert!(!wants_plugin_hotkeys(Some("")));
        assert!(!wants_plugin_hotkeys(None));
        if std::env::consts::OS != "linux" {
            // macOS / Windows 不受这个开关影响：一律用插件
            assert!(!x11_hotkeys_via_plugin());
            assert!(global_shortcut_supported());
            assert_eq!(hotkey_backend(), "plugin");
        }
    }

    #[test]
    fn os_version_is_cached_and_non_empty() {
        let first = os_version();
        assert!(!first.is_empty());
        assert!(std::ptr::eq(first, os_version()));
    }
}
