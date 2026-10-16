//! 原生窗口能力（应用层里除 lib.rs 的插件注册外唯一允许出现 `cfg(target_os)` 的位置）：NSPanel 转换、NSVisualEffectView、
//! displayLink 弹簧、Win32 扩展样式、WebView2 内存目标、X11 窗口属性等。各平台文件对外提供**同名同签名**的函数与常量
//! （下面「平台接口」一节列出），上层（panel.rs / pinned.rs / overlay.rs / manager.rs）不写任何平台分支。
//!
//! **模板的平台差异**：tauri.conf.json 的窗口模板（`create: false`）只能写一份，按平台不同的属性
//! （pinned 在 Windows 上无装饰 + DWM 圆角 + `shadow: true`、Linux 上无装饰 + 透明；settings 在 Win11 上用 Mica 等）
//! 一律由各平台文件的 `template_overrides(builder, kind, frame_mode)` 在 `WebviewWindowBuilder::from_config(..)` 之后、
//! `build()` 之前补上（macOS Native 帧模式的 panel 打开原生阴影；Linux 不能透明时关闭透明，见各平台文件）。
//! WindowManager 创建**任何** WebView 窗口都必须经过它（不另建 tauri.<platform>.conf.json，避免整份窗口数组重复）。
//!
//! ## 平台接口（三个平台文件都提供）
//!
//! | 名称 | 作用 |
//! | --- | --- |
//! | `DESKTOP_UNIT` | 桌面坐标单位：macOS 全局逻辑坐标，Windows / Linux 物理像素（见 placement.rs） |
//! | `PREWARM_OVERLAY` | 空闲时是否预建隐藏的主显示器 Overlay（Windows 默认开） |
//! | `OVERLAY_FOLLOWS_POINTER` | 多屏截图时按键焦点是否由原生层跟随指针（macOS 的 NSTrackingArea；否则编排器轮询光标作兜底） |
//! | `PANEL_RECYCLE_AFTER` | 用过的面板隐藏多久之后回收 WebView、重建干净的预热页面（空闲卸载的变体；macOS 45 s，Windows / Linux None：macOS 上空闲零 CPU 优先） |
//! | `RESTORE_FOCUS_ON_HIDE` | 隐藏取得过焦点的面板后是否要主动把焦点还给原应用（macOS 的非激活面板天然还原） |
//! | `PINNED_CSS_SHADOW` | Pinned 窗口是否透明并由 CSS 画圆角阴影（只有 Linux；它的 boot 带阴影边距） |
//! | `set_regular_activation` | macOS ActivationPolicy 切换 |
//! | `template_overrides` | 模板的平台差异 |
//! | `panel_*` | 面板的转换、定位、显示 / 隐藏、焦点、点击穿透、WebView2 可见性与内存目标 |
//! | `pinned_*` | Pinned 的平台属性、交接（同一帧显示 pinned、隐藏面板）与展开过渡 |
//! | `float_*` | 悬浮剪贴板：`float_attach`（不抢焦点的窗口属性）、`float_set_on_top`、`float_set_focusable`（搜索框要打字时临时可聚焦）、`float_yield`（发粘贴键前把键盘焦点交还目标应用）、`float_handoff` / `float_show` / `float_hide` |
//! | `overlay_*` | Overlay 的转换、冻结帧图层、显示 / 隐藏 / 销毁 |
//! | `window_frame` / `set_window_frame` | 任意窗口的桌面坐标矩形读写 |
//! | `flush_after_hide` | 截图前隐藏窗口后等合成器出完下一帧 |
//! | `foreground_is_shell` | 前台是否落在桌面 / 任务栏上（失焦关闭后决定是否还原焦点） |
//! | `is_reopen` | 运行事件是否为 macOS 的 `RunEvent::Reopen`（其他平台恒为 false） |
//! | `adopt_window` | 窗口 `build()` 成功后立即调用一次：macOS 接管 tao 0.37.1 创建 NSWindow 时多出、永远不会释放的那次引用（窗口泄漏的根因） |
//! | `destroy_window` | 销毁任意窗口（macOS：主线程 + autorelease pool 内，销毁的窗口与 WebView 当场释放）；三端都先 [`note_destroying`] |
//! | `release_offscreen_drivers` | 窗口销毁后释放已离屏窗口的逐帧驱动（macOS 的 displayLink，防止窗口泄漏） |
//! | `animating` | 有没有窗口正在跑原生逐帧动画（macOS 的 displayLink 计数，不等主线程；其他平台恒为 false）；面板回收据此推迟 |
//! | `describe_window` / `window_census` / `HAS_WINDOW_CENSUS` | 诊断：窗口状态与逐帧驱动；存活的窗口对象（Wisp / AppKit）与窗口服务器里的窗口数（泄漏检查；只有 macOS 能取到） |
//! | `tray_set_accessibility` | 托盘图标的无障碍名称（macOS 的 NSStatusBarButton；其他平台以 tooltip 为准） |
//! | `tray_enable_press` | 托盘按钮的无障碍「按下」打开菜单（macOS 的 target / action；其他平台空操作） |
//! | `overlay_focus` / `overlay_focus_state` | 多屏截图时让光标所在屏的 Overlay 接收键盘；诊断 Overlay 的焦点 |
//! | `overlay_stop_following` | 截图会话 commit 之后这块屏的 Overlay 不再跟随指针成为 key（macOS；其他平台空操作） |
//! | `panel_show_shell` / `panel_reveal_shell` / `panel_move_shell` | 冷唤出的原生外壳（macOS；Windows / Linux 退回普通显示，从不被调用） |
//! | `app_is_active` / `lab_*` | 诊断（window lab）：系统层面是否在前台、交还前台、按键探针、外壳截住的按键、WebKit 进程与 malloc 统计（只有 macOS 取得到） |
//! | `settings_attach` | 设置窗口不进入全屏（macOS 的 collectionBehavior `FullScreenNone`；其他平台空操作） |
//! | `mark_windows_menu` | 应用菜单的「窗口」子菜单设为 NSApp 的 windowsMenu（macOS；其他平台空操作） |
//!
//! 窗口的销毁与新建经 [`note_destroying`] / [`await_destroyed`] / [`settle_destroyed`] 错开（见 [`note_destroying`]：
//! WindowId 是窗口地址，销毁后紧接着新建的窗口可能被旧窗口迟到的 `Destroyed` 误伤）。
//!
//! 需要「在主线程上执行并等结果」的平台调用统一经 [`post_and_wait`]：超时放弃时闭包随之作废，不会在调用方已经当作失败
//! 处理之后才在主线程上执行。面板的显示（`panel_show` / `panel_reveal`）在主线程真正执行之前还要核对一次
//! [`ShowTicket`]：决定显示之后才开始的隐藏让它作废，主线程按 FIFO 执行窗口操作，作废的显示直接跳过。

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
pub use self::linux::*;
#[cfg(target_os = "macos")]
pub use self::macos::*;
#[cfg(target_os = "windows")]
pub use self::windows::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use wisp_types::Rect;

use super::sizing::CubicBezier;
use crate::error::IpcError;

/// 把 `f` 交给主线程执行（`post` 负责投递，例如 `run_on_main_thread`）并等待结果，最多等 `timeout`。
///
/// 超时时如果 `f` 还没开始执行，就把它**作废**：之后主线程轮到它时直接跳过——否则调用方已经按失败处理（例如 ready
/// 显示超时、清掉了会话），主线程稍后照样把面板显示出来，留下一个没有会话、没有关闭监听、Esc 与外部点击都关不掉的窗口。
/// 如果 `f` 已经在主线程上执行，就等它做完、返回它的结果（它马上就会结束）
pub(crate) fn post_and_wait<T: Send + 'static>(
    post: impl FnOnce(Box<dyn FnOnce() + Send>) -> tauri::Result<()>,
    f: impl FnOnce() -> T + Send + 'static,
    timeout: Duration,
) -> Result<T, IpcError> {
    const PENDING: u8 = 0;
    const RUNNING: u8 = 1;
    const ABANDONED: u8 = 2;
    let state = Arc::new(AtomicU8::new(PENDING));
    let (tx, rx) = mpsc::sync_channel(1);
    let gate = Arc::clone(&state);
    post(Box::new(move || {
        if gate
            .compare_exchange(PENDING, RUNNING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let _ = tx.send(f());
        }
    }))?;
    match rx.recv_timeout(timeout) {
        Ok(v) => Ok(v),
        Err(_)
            if state
                .compare_exchange(PENDING, ABANDONED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok() =>
        {
            Err(IpcError::internal("main thread did not respond"))
        }
        // 已经开始执行：等它做完（发送端随闭包结束而释放，闭包 panic 时这里得到错误）
        Err(_) => rx
            .recv()
            .map_err(|_| IpcError::internal("main thread call did not complete")),
    }
}

/// 新建窗口之前最多等登记过的销毁收尾这么久（正常是同一轮或下一轮事件循环，几毫秒）
const DESTROY_SETTLE_MAX: Duration = Duration::from_millis(300);
/// 等销毁收尾时的轮询间隔
const DESTROY_SETTLE_POLL: Duration = Duration::from_millis(4);

/// 已请求销毁、还没从 Tauri 的窗口表里移除的窗口（label），见 [`note_destroying`]。「窗口是否还在」由调用方注入
/// （生产代码查 Tauri 的窗口表，单测用假的），等待的时长与轮询间隔也是参数，逻辑因此可以单独测试
struct DestroyRegistry(parking_lot::Mutex<Vec<String>>);

impl DestroyRegistry {
    const fn new() -> Self {
        Self(parking_lot::const_mutex(Vec::new()))
    }

    /// 登记（同一个 label 只记一次）
    fn note(&self, label: &str) {
        let mut list = self.0.lock();
        if !list.iter().any(|l| l == label) {
            list.push(label.to_owned());
        }
    }

    /// 还没消失的已登记窗口数（顺便清掉已经消失的）
    fn left(&self, alive: &dyn Fn(&str) -> bool) -> usize {
        let mut list = self.0.lock();
        list.retain(|label| alive(label));
        list.len()
    }

    /// 等不到时放弃并清空登记：不让一个永远不消失的窗口（例如销毁失败）拖慢之后的每一次新建
    fn give_up(&self, left: usize, max: Duration) {
        let labels = std::mem::take(&mut *self.0.lock());
        log::warn!(
            "{left} destroyed window(s) still registered after {} ms: {labels:?}",
            max.as_millis()
        );
    }

    /// 阻塞地等登记过的窗口都消失，最多 `max`；没有登记时立即返回 true。`on_main` 时不等（`Destroyed` 正要由主线程
    /// 处理，等就是死锁）、也不清登记（之后在别的线程上新建时照样要等），返回 false
    fn wait(&self, alive: &dyn Fn(&str) -> bool, on_main: bool, poll: Duration, max: Duration) -> bool {
        if self.left(alive) == 0 {
            return true;
        }
        if on_main {
            log::warn!("window created on the main thread while a destroyed window has not been released yet");
            return false;
        }
        let started = std::time::Instant::now();
        loop {
            std::thread::sleep(poll);
            let left = self.left(alive);
            if left == 0 {
                return true;
            }
            if started.elapsed() >= max {
                self.give_up(left, max);
                return false;
            }
        }
    }

    /// [`Self::wait`] 的异步版本（不占着 worker 线程）
    async fn settle(&self, alive: &(dyn Fn(&str) -> bool + Sync), poll: Duration, max: Duration) -> bool {
        let started = std::time::Instant::now();
        loop {
            let left = self.left(alive);
            if left == 0 {
                return true;
            }
            if started.elapsed() >= max {
                self.give_up(left, max);
                return false;
            }
            tokio::time::sleep(poll).await;
        }
    }
}

/// 进程里唯一的销毁登记
static DESTROYING: DestroyRegistry = DestroyRegistry::new();

/// 窗口是否还在 Tauri 的窗口表里（`Destroyed` 处理完之后才会移除）
fn window_alive(app: &tauri::AppHandle) -> impl Fn(&str) -> bool + Sync + '_ {
    use tauri::Manager;
    move |label| app.get_webview_window(label).is_some()
}

/// 登记一个即将销毁的窗口（三个平台的 `destroy_window` / `panel_destroy` / `overlay_destroy` 与半初始化窗口的销毁都先
/// 调用它）。
///
/// 为什么要等：tao 的 `WindowId` 是窗口**地址**（macOS 的 NSWindow 指针、Windows 的 HWND），tauri-runtime-wry 2.12 的
/// `WindowIdStore`（地址 → Tauri 窗口 id）从不删除条目。窗口被释放之后（销毁的 NSWindow 会当场
/// 释放），紧接着新建的窗口可能分到同一个地址，`insert` 覆盖成新窗口的 id；而 tao 在一轮事件循环里先处理用户事件（其中
/// 包括建窗口）、再处理排队的窗口事件——旧窗口迟到的 `Destroyed(地址)` 于是按地址查到**新**窗口，把它从 Tauri 的窗口表
/// 里移除、发出它的 `Destroyed` 并关掉它。所以新建任何窗口之前先 [`await_destroyed`]：等登记过的窗口都从 Tauri 的窗口表里
/// 消失（`Destroyed` 已处理，此后同一地址再被 `insert` 覆盖也无害）。用户点关闭按钮关掉的设置 / 引导窗口不经这里——那是
/// 人的节奏，不会在同一轮事件循环里紧接着新建窗口
pub(crate) fn note_destroying(label: &str) {
    DESTROYING.note(label);
}

/// 新建窗口之前（阻塞线程上）：等登记过的销毁收尾（见 [`note_destroying`]），最多 [`DESTROY_SETTLE_MAX`]。没有登记时
/// 立即返回。在主线程上调用时不等（`Destroyed` 正要由主线程处理，等就是死锁），返回 false
pub(crate) fn await_destroyed(app: &tauri::AppHandle) -> bool {
    let on_main = std::thread::current().name() == Some("main");
    DESTROYING.wait(&window_alive(app), on_main, DESTROY_SETTLE_POLL, DESTROY_SETTLE_MAX)
}

/// [`await_destroyed`] 的异步版本（在 async 任务里新建窗口时用，不占着 worker 线程）
pub(crate) async fn settle_destroyed(app: &tauri::AppHandle) -> bool {
    DESTROYING
        .settle(&window_alive(app), DESTROY_SETTLE_POLL, DESTROY_SETTLE_MAX)
        .await
}

/// window lab 的进程采样（[`lab_webkit_processes`]，只有 macOS 能取到）：属于本进程的一个 WebKit 辅助进程
#[cfg(any(debug_assertions, feature = "fake-llm"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabProcess {
    pub pid: i32,
    /// 进程名去掉 `com.apple.WebKit.` 前缀：`WebContent` / `GPU` / `Networking` …
    pub kind: String,
    /// phys_footprint（KiB，与活动监视器的「内存」同一口径）
    pub footprint_kib: u64,
}

/// window lab：原生逐帧动画的帧间隔（开着逐帧统计时累积、[`lab_take_frame_trace`] 取走；只有 macOS 有原生逐帧动画）。
/// 用于对比「面板进入淡入」与「Native 高度弹簧」各自的帧间隔（例如分配器开关前后）
#[cfg(any(debug_assertions, feature = "fake-llm"))]
#[derive(Debug, Default, Clone)]
pub struct LabFrameTrace {
    /// 名义帧间隔（ms，displayLink.duration）
    pub nominal_ms: f32,
    /// 有淡入淡出（窗口 alpha 或冷唤出外壳上的内容 alpha）在进行的帧，与上一帧的间隔（ms）
    pub fade_ms: Vec<f32>,
    /// 有高度弹簧在进行的帧，与上一帧的间隔（ms）
    pub height_ms: Vec<f32>,
    /// 有淡入淡出或高度弹簧的帧里，帧回调在主线程上用了多久（µs：弹簧推进、`setFrame:display:`、`invalidateShadow`）——
    /// 帧间隔按显示器的节拍取整，只能看出整帧的掉帧；回调耗时能看出不到一帧的变化
    pub callback_us: Vec<f32>,
}

/// window lab：一段原生高度弹簧的轨迹（开着逐帧统计时记录、[`lab_take_height_traces`] 取走；用于测量 Native 帧模式菜单
/// 预留的 snappy 弹簧）。时间都相对这段运动开始（`FrameDriver::animate_height` 收到第一次目标）的时刻
#[cfg(any(debug_assertions, feature = "fake-llm"))]
#[derive(Debug, Clone)]
pub struct LabHeightTrace {
    /// 运动开始的 Unix 时间（ms；与页面的 `performance.timeOrigin + now()` 对齐）
    pub start_unix_ms: f64,
    /// 起点高度
    pub from: f64,
    /// 每次（重新）定目标：(ms, 目标高度, 弹簧 `gentle` / `snappy`)
    pub targets: Vec<(f64, f64, &'static str)>,
    /// 逐帧写进窗口的高度（按物理像素取整之后）：(这一帧上屏的 ms, 高度)
    pub samples: Vec<(f64, f64)>,
}

/// 窗口泄漏检查的计数（[`window_census`]，只有 macOS 能取到）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowCensus {
    /// 存活的 Wisp 窗口对象（tao / tauri-nspanel 创建的，含已离屏、未释放的）
    pub wisp: usize,
    /// 存活的 AppKit 自己的窗口对象（菜单栏副本、输入法光标窗……，不归 Wisp 管理）
    pub system: usize,
    /// 窗口服务器里属于存活 Wisp 窗口的窗口数（含离屏）
    pub server_wisp: usize,
    /// 窗口服务器里本进程的其余窗口（AppKit 自己的：切到 Regular 时建的菜单栏副本、输入法光标窗……）
    pub server_other: usize,
}

/// 窗口对象的类名是否属于 Wisp（tao 的 `TaoWindow`、tauri-nspanel 转换后的 `WispPanel` / `WispOverlayPanel`，以及 KVO
/// 观察时系统派生的 `NSKVONotifying_` 子类）
pub fn is_wisp_window_class(name: &str) -> bool {
    let base = name.strip_prefix("NSKVONotifying_").unwrap_or(name);
    base.starts_with("Tao") || base.starts_with("Wisp")
}

/// 窗口模板的种类（由 label 推导，与前端 `windowKindOf` 一致）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TemplateKind {
    Panel,
    /// `pinned-<id>`
    Pinned,
    /// 悬浮剪贴板 `clipfloat`（与 Pinned 同样是独立的卡片窗口，但从不抢焦点）
    Floating,
    /// settings / onboarding
    Regular,
    /// `overlay-<n>`
    Overlay,
}

impl TemplateKind {
    /// 按 label 分类：panel / settings / onboarding / pinned-<id> / clipfloat / overlay-<n>；未知 label 视为 panel
    pub fn of(label: &str) -> Self {
        match label {
            "settings" | "onboarding" => Self::Regular,
            crate::clipboard_svc::FLOAT_LABEL => Self::Floating,
            l if l.starts_with("pinned-") => Self::Pinned,
            l if l.starts_with("overlay-") => Self::Overlay,
            _ => Self::Panel,
        }
    }
}

/// 一段补间（窗口 alpha 的进入 / 退出）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fade {
    /// 秒
    pub duration: f64,
    pub curve: CubicBezier,
}

/// 显示时窗口的 alpha
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ShowAlpha {
    /// 直接不透明（Css 帧模式：进入动画由 CSS 播放）
    Opaque,
    /// 先以 alpha 0 显示（ready 超时：窗口可见后 WebView 恢复渲染，但用户看不到任何东西）
    Transparent,
    /// alpha 0 → 1 淡入（Native 帧模式的进入）
    FadeIn(Fade),
}

/// 面板的显示选项
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShowOpts {
    pub alpha: ShowAlpha,
    /// 是否立即成为 key / 获得键盘焦点（输入 / 剪贴板视图）。macOS 不激活应用
    pub key: bool,
}

/// 一次面板显示的「票」。控制器在决定显示时（锁内）取票；之后开始的任何隐藏（截图前隐藏、固定交接、Esc……）都先让
/// 已发出的票作废，再把离屏操作投递到主线程。平台层在**主线程上真正执行显示之前**核对它：作废了就跳过显示并返回
/// [`ShowTicket::superseded`]。主线程按 FIFO 执行窗口操作，所以无论两者怎样交错，最后都不会出现「会话已经隐藏、
/// 窗口却留在屏幕上」（没有关闭监听，Esc 与外部点击都关不掉）
#[derive(Debug, Clone)]
pub struct ShowTicket {
    epoch: Arc<AtomicU64>,
    issued: u64,
}

impl ShowTicket {
    /// 按 `epoch` 的当前值出票
    pub fn issue(epoch: &Arc<AtomicU64>) -> Self {
        Self {
            epoch: Arc::clone(epoch),
            issued: epoch.load(Ordering::SeqCst),
        }
    }

    /// 让此前发出的全部票作废
    pub fn revoke_all(epoch: &AtomicU64) {
        epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// 票是否仍然有效
    pub fn valid(&self) -> bool {
        self.epoch.load(Ordering::SeqCst) == self.issued
    }

    /// 票已作废时显示函数返回的错误
    pub fn superseded() -> IpcError {
        IpcError::internal("panel show superseded by a hide")
    }
}

/// Native 帧模式原地替换时的宽度切换：`delay` 之后——宽度要变的交叉淡入里旧视图刚好完全透明的那一刻
/// （前端 ViewSwitch 的 `mode="wait"` 先把旧视图淡出 `timing.crossfade`）——把窗口的左边与宽度改为 `x` / `width`
/// （桌面坐标），高度与固定边保持当时的值（高度弹簧照常进行）。`pin` 为 Some 时在同一帧把 WebView 固定为该高度
/// （Native 帧模式 WebView 固定为会话上限高，见 macos.rs 的 `panel_pin_webview`），改尺寸带来的 WebContent 重排与
/// 宽度变化合在内容透明的这一帧里
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WidthSwitch {
    pub x: f64,
    pub width: f64,
    pub delay: Duration,
    pub pin: Option<f64>,
}

/// Native 帧模式高度弹簧的竖直边界（桌面坐标）：用户拖动过的面板继续长高时，窗口的顶边与底边都要留在这个区间里——
/// 固定边照常不动，伸出边界时整体推回（向下长到底就改为顶边上移）。只在弹簧开始这一帧窗口还在边界内时生效：用户正把
/// 窗口拖到边界外时不和拖动抢位置（拖动结束后由控制器夹回）。没有拖动过的面板为 None（上限本来就保证放得下）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VerticalBound {
    pub top: f64,
    pub bottom: f64,
}

/// Css 帧模式的点击穿透区域（桌面坐标）：窗口内、卡片矩形外的透明区不得吞掉点击
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClickRegion {
    pub window: Rect,
    pub card: Rect,
}

impl ClickRegion {
    /// 桌面坐标点是否落在窗口的透明区（窗口内、卡片外）
    pub fn passes_through(&self, x: f64, y: f64) -> bool {
        let p = wisp_types::Point { x, y };
        self.window.contains(p) && !self.card.contains(p)
    }
}

/// 面板卡片的原生表面（Native 帧模式，只有 macOS 用）：材质 + 着色 + 描边全部由原生层画在窗口矩形内，
/// WebView 背景透明、只画内容。这样逐帧改窗口高度时，卡片表面与窗口同一个 CA 事务更新，不依赖 WebContent 进程
/// 跟上新尺寸（WKWebView 的重排是异步的），底边不会出现缺着色的空白条。
///
/// 颜色是 `shared/styles/tokens.css` 的镜像（`--surface-tint` / `--surface-solid` / `--hairline`），单测
/// `card_surface_matches_tokens` 校验；调整这些令牌时同步这里。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CardSurface {
    /// true：NSVisualEffectView（popover / behindWindow / active）+ 半透明着色；false（material = solid、
    /// 系统「降低透明度」）：只画不透明的 `--surface-solid`
    pub material: bool,
    /// 着色层颜色（sRGB 0–1，含 alpha）
    pub fill: [f64; 4],
    /// 0.5 pt 描边颜色（sRGB 0–1，含 alpha）
    pub hairline: [f64; 4],
}

impl CardSurface {
    /// 按已解析的主题与材质取表面颜色
    pub fn resolve(theme: wisp_types::ResolvedTheme, material: wisp_types::ResolvedMaterial) -> Self {
        use wisp_types::{ResolvedMaterial, ResolvedTheme};
        let rgb = |r: u8, g: u8, b: u8, a: f64| [f64::from(r) / 255.0, f64::from(g) / 255.0, f64::from(b) / 255.0, a];
        let native = material == ResolvedMaterial::Native;
        let (fill, hairline) = match (theme, native) {
            // --surface-tint: rgb(255 255 255 / 0.55)；--hairline: rgb(0 0 0 / 0.08)
            (ResolvedTheme::Light, true) => (rgb(255, 255, 255, 0.55), rgb(0, 0, 0, 0.08)),
            // --surface-solid: #fbfbfc
            (ResolvedTheme::Light, false) => (rgb(0xfb, 0xfb, 0xfc, 1.0), rgb(0, 0, 0, 0.08)),
            // --surface-tint: rgb(30 30 32 / 0.5)；--hairline: rgb(255 255 255 / 0.1)
            (ResolvedTheme::Dark, true) => (rgb(30, 30, 32, 0.5), rgb(255, 255, 255, 0.1)),
            // --surface-solid: #1c1c1f
            (ResolvedTheme::Dark, false) => (rgb(0x1c, 0x1c, 0x1f, 1.0), rgb(255, 255, 255, 0.1)),
        };
        Self {
            material: native,
            fill,
            hairline,
        }
    }
}

#[cfg(test)]
mod tests {
    use wisp_types::{ResolvedMaterial, ResolvedTheme};

    use super::*;

    #[test]
    fn card_surface_matches_tokens() {
        // 原生表面颜色是 tokens.css 的镜像（Native 帧模式下 CSS 卡片背景透明，由原生层画）
        let tokens = include_str!("../../../../src/shared/styles/tokens.css");
        let compact: String = tokens.split_whitespace().collect::<Vec<_>>().join(" ");
        for decl in [
            "--surface-tint: rgb(255 255 255 / 0.55);",
            "--surface-tint: rgb(30 30 32 / 0.5);",
            "--surface-solid: #fbfbfc;",
            "--surface-solid: #1c1c1f;",
            "--hairline: rgb(0 0 0 / 0.08);",
            "--hairline: rgb(255 255 255 / 0.1);",
        ] {
            assert!(
                compact.contains(decl),
                "tokens.css 里找不到 `{decl}`：原生卡片表面与令牌不一致"
            );
        }
        let light = CardSurface::resolve(ResolvedTheme::Light, ResolvedMaterial::Native);
        assert!(light.material);
        assert_eq!(light.fill, [1.0, 1.0, 1.0, 0.55]);
        let dark_solid = CardSurface::resolve(ResolvedTheme::Dark, ResolvedMaterial::Solid);
        assert!(!dark_solid.material);
        assert_eq!(dark_solid.fill[3], 1.0);
        assert!((dark_solid.fill[0] - 28.0 / 255.0).abs() < 1e-12);
    }

    /// macOS 的 `adopt_window` 补偿的是 tao 0.37.1 `create_window` 的引用泄漏（alloc/init 的 +1 之后又 `Retained::retain`）。
    /// tao 版本一变就失败：升级者必须核对上游 `create_window` 是否已改为接管那次引用——已修复时删除 `adopt_window` 里的
    /// 释放（否则过度释放、销毁窗口时崩溃），仍未修复时更新这里的版本；两种情况都要在 macOS 上跑 window lab 的
    /// `no_window_leak`（`NSZombieEnabled=YES`）确认
    #[test]
    fn tao_create_window_leak_is_compensated_for_the_locked_version() {
        let lock = include_str!("../../../../../../Cargo.lock");
        let tao = lock
            .split("[[package]]")
            .find(|p| p.contains("\nname = \"tao\"\n"))
            .and_then(|p| p.lines().find_map(|l| l.strip_prefix("version = ")))
            .map(|v| v.trim_matches('"'));
        assert_eq!(
            tao,
            Some("0.37.1"),
            "tao 版本变了：核对 native/macos.rs 的 adopt_window 是否仍然需要（见函数注释）"
        );
    }

    /// 非激活面板的根治在 `vendor/wry` 的补丁里：macOS 上 `focused(false)` 的 WebView 不再
    /// `NSApplication.activate()`，panel / overlay / pinned 建窗口时不抢前台（macos.rs 的 `template_overrides`）。Cargo.lock 里的
    /// wry 必须来自这份补丁（路径依赖没有 `source`）、版本与它一致。升级 tauri / wry 时这里失败：先看上游是否已让 macOS 遵守
    /// `focused`（支持了就删掉根 Cargo.toml 的 `[patch.crates-io]` 与 vendor/wry），否则把补丁移植到新版本；两种情况都要在
    /// macOS 上跑 window lab 的 `overlay_focus`（`WISP_WINDOW_LAB_INPUT_PROBE=1` 时连同 `overlay_keys`）确认
    #[test]
    fn wry_is_patched_so_unfocused_webviews_do_not_activate_the_app() {
        let lock = include_str!("../../../../../../Cargo.lock");
        let wry = lock
            .split("[[package]]")
            .find(|p| p.contains("\nname = \"wry\"\n"))
            .expect("Cargo.lock 里没有 wry");
        assert!(
            wry.contains("\nversion = \"0.57.0\"\n"),
            "wry 版本变了：移植 vendor/wry 的补丁（见本测试注释）"
        );
        assert!(
            !wry.contains("\nsource = "),
            "wry 没有走 vendor/wry 的补丁：根 Cargo.toml 的 [patch.crates-io] 丢了？"
        );
        let patched = include_str!("../../../../../../vendor/wry/src/wkwebview/mod.rs");
        assert!(
            patched.contains("if attributes.focused {"),
            "vendor/wry 里的 activate 不再受 focused 控制"
        );
        // 自定义协议处理器的引用交给 WebView 的配置，WebView 释放时一起释放
        assert!(
            patched.contains("drop(Retained::<AnyObject>::from_raw(handler));"),
            "vendor/wry 里自定义协议处理器多出的那次引用又没人释放了"
        );
        // 第 5 处：异步协议的响应在主线程完成 did* 调用（ipc:// 的异步命令与窗口销毁的竞态）
        let handler = include_str!("../../../../../../vendor/wry/src/wkwebview/class/url_scheme_handler.rs");
        assert!(
            handler.contains("main_thread::run(Box::new(move || {")
                && handler.contains("dispatch_async_f(std::ptr::addr_of!(_dispatch_main_q)"),
            "vendor/wry 的自定义协议响应又回到了调用方线程（见 vendor/README.md）"
        );
    }

    /// tao 0.37.1 的 macOS `TaoView` / `TaoWindowDelegate` 覆写了 `dealloc` 却不调用 `[super dealloc]`：每销毁一个窗口（面板
    /// 回收 / 卸载、Overlay、固定窗口、设置窗口）就留下视图与委托对象本身连同 NSView 持有的一整套对象（两次 heap 快照
    /// 相减可以看到）。`vendor/tao` 的补丁补上 super 调用；这里守住补丁：Cargo.lock 的 tao 必须
    /// 来自 vendor（没有 `source`）、版本与补丁一致、两处 super 调用都在。升级 tauri / tao 时这里失败：先看上游
    /// （tauri-apps/tao）是否已修复——修复了就删掉根 Cargo.toml `[patch.crates-io]` 的 tao 与 vendor/tao，否则把补丁移植到新
    /// 版本；两种情况都要在 macOS 上跑 window lab（`WISP_WINDOW_LAB_RECYCLE_STRESS=220`，并用 `NSZombieEnabled=YES` 跑一次）
    /// 确认 malloc 斜率与没有过度释放。`adopt_window` 补偿的是另一处泄漏（`create_window` 的引用），与本补丁无关
    #[test]
    fn tao_is_patched_so_destroyed_windows_free_their_views() {
        let lock = include_str!("../../../../../../Cargo.lock");
        let tao = lock
            .split("[[package]]")
            .find(|p| p.contains("\nname = \"tao\"\n"))
            .expect("Cargo.lock 里没有 tao");
        assert!(
            tao.contains("\nversion = \"0.37.1\"\n"),
            "tao 版本变了：核对上游是否已修复 dealloc，移植或删除 vendor/tao 的补丁（见本测试注释）"
        );
        assert!(
            !tao.contains("\nsource = "),
            "tao 没有走 vendor/tao 的补丁：根 Cargo.toml 的 [patch.crates-io] 丢了？"
        );
        let view = include_str!("../../../../../../vendor/tao/src/platform_impl/macos/view.rs");
        assert!(
            view.contains("msg_send![super(this, class!(NSView)), dealloc]"),
            "vendor/tao 的 TaoView::dealloc 不再调用 [super dealloc]"
        );
        let delegate = include_str!("../../../../../../vendor/tao/src/platform_impl/macos/window_delegate.rs");
        assert!(
            delegate.contains("msg_send![super(this, class!(NSResponder)), dealloc]"),
            "vendor/tao 的 TaoWindowDelegate::dealloc 不再调用 [super dealloc]"
        );
    }

    /// 假的 Tauri 窗口表：`gone` 里的 label 已经消失
    fn table(gone: &'static [&'static str]) -> impl Fn(&str) -> bool + Sync {
        move |label| !gone.contains(&label)
    }

    fn registered(r: &DestroyRegistry) -> Vec<String> {
        r.0.lock().clone()
    }

    const POLL: Duration = Duration::from_millis(1);
    const MAX: Duration = Duration::from_millis(40);

    #[test]
    fn destroy_registry_clears_windows_that_are_gone() {
        let r = DestroyRegistry::new();
        // 没有登记：不等
        assert!(r.wait(&table(&[]), false, POLL, MAX));
        r.note("pinned-p1");
        r.note("pinned-p1");
        r.note("overlay-0");
        assert_eq!(registered(&r), ["pinned-p1", "overlay-0"], "同一个 label 只登记一次");
        // 只消失了一个：清掉它，另一个还在
        assert_eq!(r.left(&table(&["overlay-0"])), 1);
        assert_eq!(registered(&r), ["pinned-p1"]);
        // 都消失了：立即返回 true，登记清空
        let t0 = std::time::Instant::now();
        assert!(r.wait(&table(&["pinned-p1", "overlay-0"]), false, POLL, MAX));
        assert!(t0.elapsed() < MAX, "已经消失的窗口不需要等");
        assert!(registered(&r).is_empty());
    }

    #[test]
    fn destroy_registry_waits_until_the_window_leaves() {
        let r = DestroyRegistry::new();
        r.note("settings");
        // 第 3 次查询时窗口才离开窗口表（模拟 `Destroyed` 在下一轮事件循环里处理完）
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let alive = |_: &str| calls.fetch_add(1, Ordering::Relaxed) < 2;
        assert!(r.wait(&alive, false, POLL, Duration::from_secs(5)));
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        assert!(registered(&r).is_empty());
    }

    #[test]
    fn destroy_registry_gives_up_and_forgets_after_the_limit() {
        let r = DestroyRegistry::new();
        r.note("panel");
        let t0 = std::time::Instant::now();
        // 永远不消失（例如销毁失败）：等满上限后放弃
        assert!(!r.wait(&table(&[]), false, POLL, MAX));
        assert!(t0.elapsed() >= MAX);
        // 放弃时清空登记：之后的新建不再被它拖慢
        assert!(registered(&r).is_empty());
        let t1 = std::time::Instant::now();
        assert!(r.wait(&table(&[]), false, POLL, MAX));
        assert!(t1.elapsed() < MAX);
    }

    #[test]
    fn destroy_registry_never_waits_on_the_main_thread() {
        let r = DestroyRegistry::new();
        r.note("pinned-p2");
        let t0 = std::time::Instant::now();
        // 主线程上不等（等就是死锁），也不清登记：之后在阻塞线程上新建时照样要等
        assert!(!r.wait(&table(&[]), true, POLL, Duration::from_secs(5)));
        assert!(t0.elapsed() < Duration::from_secs(1));
        assert_eq!(registered(&r), ["pinned-p2"]);
        // 没有未消失的登记时，主线程上也直接返回 true
        assert!(r.wait(&table(&["pinned-p2"]), true, POLL, MAX));
    }

    #[test]
    fn destroy_registry_async_settle() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        let r = DestroyRegistry::new();
        rt.block_on(async {
            assert!(r.settle(&table(&[]), POLL, MAX).await, "没有登记：立即返回");
            r.note("onboarding");
            let calls = std::sync::atomic::AtomicUsize::new(0);
            let alive = |_: &str| calls.fetch_add(1, Ordering::Relaxed) < 3;
            assert!(r.settle(&alive, POLL, Duration::from_secs(5)).await);
            assert!(registered(&r).is_empty());
            r.note("onboarding");
            let t0 = std::time::Instant::now();
            assert!(!r.settle(&table(&[]), POLL, MAX).await, "等不到时放弃");
            assert!(t0.elapsed() >= MAX);
            assert!(registered(&r).is_empty(), "放弃时清空登记");
        });
    }

    #[test]
    fn wisp_window_classes() {
        for name in ["TaoWindow", "NSKVONotifying_TaoWindow", "WispPanel", "WispOverlayPanel"] {
            assert!(is_wisp_window_class(name), "{name}");
        }
        for name in [
            "NSMenuBarReplicantWindow",
            "TUINSWindow",
            "NSStatusBarWindow",
            "NSKVONotifying_NSWindow",
        ] {
            assert!(!is_wisp_window_class(name), "{name}");
        }
    }

    #[test]
    fn template_kind_from_label() {
        assert_eq!(TemplateKind::of("panel"), TemplateKind::Panel);
        assert_eq!(TemplateKind::of("settings"), TemplateKind::Regular);
        assert_eq!(TemplateKind::of("onboarding"), TemplateKind::Regular);
        assert_eq!(TemplateKind::of("pinned-p3"), TemplateKind::Pinned);
        assert_eq!(TemplateKind::of("clipfloat"), TemplateKind::Floating);
        assert_eq!(TemplateKind::of("overlay-0"), TemplateKind::Overlay);
    }

    #[test]
    fn post_and_wait_runs_or_abandons() {
        use std::sync::atomic::AtomicBool;

        // 立即执行
        let v = post_and_wait(
            |job| {
                job();
                Ok(())
            },
            || 7,
            Duration::from_millis(50),
        );
        assert_eq!(v.ok(), Some(7));

        // 主线程卡住：超时放弃，之后轮到它时不再执行
        let ran = Arc::new(AtomicBool::new(false));
        let parked = Arc::new(parking_lot::Mutex::new(None::<Box<dyn FnOnce() + Send>>));
        let flag = Arc::clone(&ran);
        let slot = Arc::clone(&parked);
        let r = post_and_wait(
            move |job| {
                *slot.lock() = Some(job);
                Ok(())
            },
            move || flag.store(true, Ordering::SeqCst),
            Duration::from_millis(20),
        );
        assert!(r.is_err());
        let job = parked.lock().take().expect("posted");
        job();
        assert!(!ran.load(Ordering::SeqCst), "超时作废的闭包不得再执行");

        // 超时前已经开始执行：等它做完并拿到结果
        let r = post_and_wait(
            |job| {
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(5));
                    job();
                });
                Ok(())
            },
            || {
                std::thread::sleep(Duration::from_millis(60));
                "done"
            },
            Duration::from_millis(20),
        );
        assert_eq!(r.ok(), Some("done"));
    }

    #[test]
    fn show_ticket_is_revoked_by_later_hides_only() {
        let epoch = Arc::new(AtomicU64::new(0));
        let ticket = ShowTicket::issue(&epoch);
        assert!(ticket.valid());
        ShowTicket::revoke_all(&epoch);
        assert!(!ticket.valid(), "出票之后开始的隐藏让它作废");
        assert!(!ticket.clone().valid());
        let fresh = ShowTicket::issue(&epoch);
        assert!(fresh.valid(), "之后出的票不受此前的隐藏影响");
    }

    #[test]
    fn click_region_passes_only_transparent_margin() {
        let r = ClickRegion {
            window: Rect::new(0.0, 0.0, 100.0, 100.0),
            card: Rect::new(24.0, 24.0, 52.0, 40.0),
        };
        assert!(r.passes_through(5.0, 5.0));
        assert!(r.passes_through(50.0, 80.0), "卡片下方的阶梯富余区");
        assert!(!r.passes_through(30.0, 30.0));
        assert!(!r.passes_through(150.0, 5.0), "窗口外不归本窗口管");
    }
}
