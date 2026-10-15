//! macOS 原生实现：tauri-nspanel 非激活 NSPanel（Panel 与 Overlay）、自建 NSVisualEffectView、
//! displayLink 逐帧弹簧、Overlay 冻结帧图层、Pinned 交接与展开过渡、ActivationPolicy 切换。
//!
//! ## 面板的 Native 帧模式
//!
//! 窗口矩形 = 卡片矩形。contentView（wry 的 `WryWebViewParent`）自下而上三层：
//! 1. `NSVisualEffectView`（popover / behindWindow / active，`maskImage` = 12 pt 圆角可拉伸图）——材质只在圆角卡片内；
//! 2. 着色层（layer-backed NSView：`--surface-tint` 背景 + 0.5 pt `--hairline` 描边 + 12 pt 圆角）——卡片表面；
//! 3. WKWebView（背景透明，只画内容）。
//!
//! 1、2 随窗口 autoresize，与 `setFrame:display:` 在同一个 CA 事务里提交，所以逐帧改高度时卡片表面永远贴合窗口；
//! WKWebView 的内容由 WebContent 进程异步重排，新露出的一条在一两帧内只有表面、还没有文字（读不出空白条），
//! 而 CSS 画的着色会在这一条上缺失——这是着色层放在原生侧的原因。
//!
//! 几何动画由 [`FrameDriver`] 驱动：窗口的 `displayLinkWithTarget:selector:`（macOS 14，跟随所在屏幕的刷新率，
//! ProMotion 上 120 Hz），只在动画期间存在，静止后立即 `invalidate`（空闲零唤醒）。弹簧与缓动是
//! `windows::sizing` 的纯函数（与前端 `spring.gentle`、`ease.*` 同参）；进入 / 退出由同一个 displayLink 驱动窗口
//! `alphaValue`（材质与原生阴影一起淡入淡出）。每次改尺寸后 `invalidateShadow`，让原生阴影贴合新的圆角形状。
//! 同一个驱动也负责 Pinned「展开」时从面板矩形到目标矩形的矩形弹簧。
//!
//! 所有 AppKit 调用都在主线程执行：对外函数可以在任意线程调用，内部用 `run_on_main_thread` 转发
//! （需要同步结果的，例如 `panel_ready` 内的 show，会等主线程执行完再返回）。

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use objc2::rc::{Retained, Weak};
use objc2::runtime::{AnyObject, Bool, NSObject};
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSAutoresizingMaskOptions, NSBezierPath, NSColor, NSEvent, NSEventMask, NSEventModifierFlags,
    NSEventType, NSImage, NSImageResizingMode, NSPopUpMenuWindowLevel, NSResponder, NSScreen, NSStatusWindowLevel,
    NSTrackingArea, NSTrackingAreaOptions, NSUserInterfaceItemIdentification, NSView, NSVisualEffectBlendingMode,
    NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView, NSWindow, NSWindowButton,
    NSWindowCollectionBehavior, NSWindowOrderingMode, NSWindowStyleMask,
};
use objc2_core_graphics::CGColor;
use objc2_foundation::{NSEdgeInsets, NSObjectProtocol, NSPoint, NSRect, NSRunLoop, NSRunLoopCommonModes, NSSize};
use objc2_quartz_core::{CADisplayLink, CAFrameRateRange};
use tauri::{ActivationPolicy, Manager};
use tauri_nspanel::{ManagerExt as _, WebviewWindowExt as _};
use wisp_types::{FrameMode, Placement, Rect};

use super::{CardSurface, ClickRegion, Fade, ShowAlpha, ShowOpts, ShowTicket, VerticalBound, WidthSwitch};
use crate::error::IpcError;
use crate::windows::panel::ShellEscape;
use crate::windows::placement::DesktopUnit;
use crate::windows::sizing::{self, CubicBezier, PANEL_RADIUS, Spring, SpringParams};

/// 桌面坐标单位：全局逻辑坐标（与 `MonitorInfo.frame` 一致）
pub const DESKTOP_UNIT: DesktopUnit = DesktopUnit::Logical;
/// macOS 不预建 Overlay（Overlay 上屏 p95 远在 150 ms 之内，常驻一个整屏 WebView 换不来可感知的收益；建 WebView 时
/// 激活应用的问题已由 vendor/wry 的补丁从源头修好，不需要靠预建躲开）
pub const PREWARM_OVERLAY: bool = false;
/// 多屏截图时按键焦点由原生层跟随指针（Overlay 上的 NSTrackingArea，见 [`overlay_attach`]），编排器不必轮询光标
pub const OVERLAY_FOLLOWS_POINTER: bool = true;
/// 用过的面板隐藏多久之后回收 WebView（空闲卸载的变体，见 `PanelOptions::recycle_after`）：**默认开，45 s**
/// （常驻空闲零 CPU 优先）。
///
/// 实测（解锁亮屏）：显示过的页面在窗口 orderOut 之后，WebKit 一直重试把 backing store
/// 标记为易失（WebContent 约 15、GPU 约 12 次/s 的中断唤醒，WebView CPU 0.208%），WebContent 也一直多占约 7 MiB；
/// backgroundThrottling 的任何取值都停不到零（throttle 只降频，suspend 要 8 分钟才挂起、GPU 一半情况仍在唤醒）。回收
/// 换掉 WebContent 进程：那一刻起唤醒归零（WebView CPU 0.005%），WebContent 回到约 21.5 MiB；GPU 进程不随之回落，
/// WebView 合计约 43 MiB。回收出来的页面立即在隐藏状态下重新加载、预热，下一次
/// 唤出仍是热的（prepare→shown 比用过的热页面慢约 6–17 ms，远在 100 ms 之内）。45 s 足够盖过一次连续使用的间隔（每次
/// prepare 都会取消计时器，连续使用时不会重建），又能让会话后的唤醒尽早归零。backgroundThrottling 因此保持 `disabled`
/// （回收出来的页面从未显示过，没有这个循环；再加 suspend 只会让长时间隐藏后的唤出多付恢复进程的代价）。
/// debug 构建仍可用 `WISP_PANEL_RECYCLE_SECS=<秒> | 0 | off` 覆盖（测量用）。
///
/// 回收只换 WebContent，GPU 进程不随之回落（用过整屏 Overlay 之后它还会留着几十 MiB），
/// 回收后 WebView 合计约 54 MiB；再往下只能靠空闲卸载（`advanced.unload_panel_after_idle_min`；新配置默认关闭——冷唤出内容首帧
/// 没有进 150 ms，见 `defaults::unload_panel_default`）——最后一个 WebView 销毁后 GPU 进程约 1 分钟内自己退出，只剩 Networking
/// （约 5 MiB）。回收与卸载同时开着时先回收、再在剩下的时长后卸载（`panel::idle_plan`）
pub const PANEL_RECYCLE_AFTER: Option<Duration> = Some(Duration::from_secs(45));
/// 能取到窗口泄漏检查的计数（`window_census`：`NSApp.windows` 与 CGWindowList）
pub const HAS_WINDOW_CENSUS: bool = true;
/// 非激活面板从不激活应用，隐藏后键盘焦点自然回到原应用
pub const RESTORE_FOCUS_ON_HIDE: bool = false;
/// Pinned 用系统窗口的圆角与阴影（`titleBarStyle: Overlay`），不透明，不需要 CSS 阴影边距
pub fn pinned_css_shadow() -> bool {
    false
}

/// 切换激活策略：settings / onboarding 存在期间为 Regular（进入 Dock 与 ⌘Tab），否则 Accessory。
///
/// 切到 Regular 的前后各把 bundle 里的真实图标设为 `NSApp.applicationIconImage` 一次（[`apply_bundle_icon`]）：Wisp 是
/// `LSUIElement` 的 agent 应用，运行中才变成 Regular，系统不会给这时才出现的 Dock 图块关联 bundle 的图标——Dock 与 ⌘Tab
/// 显示的是通用占位图（macOS 26.4 上是白色圆角方块带网格；Finder、`NSWorkspace iconForFile` 与
/// `NSRunningApplication.icon` 给出的都是正确图标，刷新 LaunchServices、重启 Dock 也没用）。显式设置之后由 AppKit 把这张图
/// 交给 Dock。两次投递与 `set_activation_policy` 走同一条主线程队列，顺序不变
pub fn set_regular_activation(app: &tauri::AppHandle, regular: bool) {
    let policy = if regular {
        ActivationPolicy::Regular
    } else {
        ActivationPolicy::Accessory
    };
    if regular {
        post_bundle_icon(app);
    }
    if let Err(e) = app.set_activation_policy(policy) {
        log::warn!("set_activation_policy failed: {e}");
    }
    if regular {
        post_bundle_icon(app);
    }
}

/// 在主线程上执行 [`apply_bundle_icon`]
fn post_bundle_icon(app: &tauri::AppHandle) {
    let posted = app.run_on_main_thread(|| {
        if let Some(mtm) = MainThreadMarker::new() {
            apply_bundle_icon(mtm);
        }
    });
    if let Err(e) = posted {
        log::warn!("run_on_main_thread failed: {e}");
    }
}

/// 诊断（window lab 的 `WISP_WINDOW_LAB_DOCK_ICON`）：[`apply_bundle_icon`] 设上 bundle 图标的次数
#[cfg(any(debug_assertions, feature = "fake-llm"))]
static BUNDLE_ICON_SETS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// bundle 里的应用图标（只在主线程上用；第一次切到 Regular 时加载。内层 None = 不在 .app 里运行或加载失败）
    static BUNDLE_ICON: std::cell::OnceCell<Option<Retained<NSImage>>> = const { std::cell::OnceCell::new() };
}

/// 把 bundle 里的应用图标设为 `NSApp.applicationIconImage`（见 [`set_regular_activation`]）。拿不到图标（`tauri dev`、
/// 单测——不在 .app 里运行）时什么也不做：dev 下 Tauri 自己在 Ready 时设过图标
fn apply_bundle_icon(mtm: MainThreadMarker) {
    let Some(icon) = BUNDLE_ICON.with(|cell| cell.get_or_init(load_bundle_icon).clone()) else {
        return;
    };
    // SAFETY: 主线程（有 MainThreadMarker）；icon 是有效的 NSImage，AppKit 自己 retain
    unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&icon)) };
    #[cfg(any(debug_assertions, feature = "fake-llm"))]
    BUNDLE_ICON_SETS.fetch_add(1, Ordering::Relaxed);
}

/// 从 bundle 加载应用图标：Info.plist 的 `CFBundleIconFile`（Tauri 写的是 `icon.icns`，没有扩展名时补 `.icns`）指向
/// `Contents/Resources` 里的 icns，直接读这个文件。不用 `NSImage imageNamed: NSImageNameApplicationIcon`：它取的就是
/// `applicationIconImage` 当前的值，系统没关联上图标时拿到的正是占位图。读出来的图至少要有一个 ≥ 256 px 的表示
/// （Retina 上 Dock 的大图块是 128 pt @2x；Tauri 生成的 icns 最大 1024 px），否则当作没有、不覆盖
fn load_bundle_icon() -> Option<Retained<NSImage>> {
    let bundle = objc2_foundation::NSBundle::mainBundle();
    let name = bundle
        .objectForInfoDictionaryKey(objc2_foundation::ns_string!("CFBundleIconFile"))
        .and_then(|v| v.downcast::<objc2_foundation::NSString>().ok())
        .map_or_else(|| "icon".to_owned(), |s| s.to_string());
    let file = if std::path::Path::new(&name).extension().is_some() {
        name
    } else {
        format!("{name}.icns")
    };
    let Some(path) = bundle.pathForResource_ofType(Some(&objc2_foundation::NSString::from_str(&file)), None) else {
        log::debug!("dock icon: {file} not in the main bundle (not running from an .app)");
        return None;
    };
    let image = NSImage::initWithContentsOfFile(NSImage::alloc(), &path);
    let largest = image.as_ref().map_or(0, |img| max_rep_px(img));
    match image {
        Some(img) if img.isValid() && largest >= 256 => {
            log::info!("dock icon: using {path} (largest representation {largest} px)");
            Some(img)
        }
        _ => {
            log::warn!("dock icon: {path} is not a usable image (largest representation {largest} px)");
            None
        }
    }
}

/// 图像最大的位图表示的像素宽度
fn max_rep_px(image: &NSImage) -> isize {
    image
        .representations()
        .iter()
        .map(|r| r.pixelsWide())
        .max()
        .unwrap_or(0)
}

/// macOS 的模板覆盖：
/// - **panel**：Native 帧模式（macOS 默认）下窗口即卡片，外阴影由原生 `hasShadow` 提供，
///   而三端共用的模板写的是 `shadow: false`，所以这里 `.shadow(true)`。tauri-nspanel 转换成 NSPanel 后 hasShadow 保留；
///   圆角遮罩 / 逐帧弹簧改变窗口形状或尺寸后由 [`FrameDriver`] 调用 `invalidateShadow`，让阴影贴合圆角卡片。
///   Css 帧模式（退路）阴影由 CSS 画，保持 false，否则会出现双重阴影；
/// - pinned：沿用模板的 `titleBarStyle: Overlay` + `hiddenTitle`（交通灯在创建后由 [`pinned_attach`] 隐藏），系统窗口自带阴影；
/// - settings / onboarding：沿用模板（需要前台，wry 建 WebView 时激活应用正合适）。
///
/// **不激活应用**：panel、overlay、pinned 一律 `focused(false)`。wry 0.57 每建一个顶层 WebView 就调用
/// `NSApplication.activate()`，上游的 `focused` 在 macOS 上不生效；工作区经 `[patch.crates-io]` 用 `vendor/wry` 的最小补丁让
/// macOS 也遵守它（补丁说明见 vendor/README.md）。这几种窗口都隐藏创建、之后以不激活的方式上屏（`orderFrontRegardless`，
/// 非激活 NSPanel 成为 key 不激活应用），建窗口这一步不能把前台从用户正在用的应用抢走、菜单栏随之切换——也就不需要「建完再撤销
/// 激活」这类补救（它会带来「Overlay 失去 key、再设回 key」的竞争）。
/// panel / overlay 的模板本来就写着 `focus: false`，这里再显式写一次，模板改动不会悄悄带回激活
pub fn template_overrides<'a, R: tauri::Runtime, M: tauri::Manager<R>>(
    builder: tauri::WebviewWindowBuilder<'a, R, M>,
    kind: super::TemplateKind,
    frame_mode: wisp_types::FrameMode,
) -> tauri::WebviewWindowBuilder<'a, R, M> {
    match (kind, frame_mode) {
        (super::TemplateKind::Panel, wisp_types::FrameMode::Native) => builder.shadow(true).focused(false),
        (super::TemplateKind::Panel | super::TemplateKind::Overlay | super::TemplateKind::Pinned, _) => {
            builder.focused(false)
        }
        (super::TemplateKind::Regular, _) => builder,
    }
}

/// tauri-nspanel 的面板类（宏会在当前作用域引入一批 `use`，放进子模块避免与本文件的导入冲突）
mod panel_class {
    tauri_nspanel::tauri_panel! {
        panel!(WispPanel {
            config: {
                // 输入 / 剪贴板视图要成为 key；翻译视图显示时不 make key，用户点击后非激活面板自动成为 key
                can_become_key_window: true,
                can_become_main_window: false,
                is_floating_panel: true,
                hides_on_deactivate: false
            }
        })

        panel!(WispOverlayPanel {
            config: {
                // Overlay 成为 key 以接收键盘（Esc、方向键），但不激活应用
                can_become_key_window: true,
                can_become_main_window: false,
                is_floating_panel: true,
                hides_on_deactivate: false
            }
        })
    }
}

/// 主线程同步调用的最长等待（主线程卡住时宁可放弃这一次窗口操作，也不阻塞 tokio worker）
const MAIN_THREAD_TIMEOUT: Duration = Duration::from_millis(500);

/// 取 Tauri 窗口背后的 NSWindow（只能在主线程调用）
fn ns_window(window: &tauri::WebviewWindow, _mtm: MainThreadMarker) -> Result<Retained<NSWindow>, IpcError> {
    let ptr = window.ns_window()?;
    // SAFETY: tauri 返回的是该窗口存活期间有效的 NSWindow 指针；在主线程上 retain，之后由 Retained 管理
    unsafe { Retained::retain(ptr.cast::<NSWindow>()) }.ok_or_else(|| IpcError::internal("NSWindow is null"))
}

/// 在主线程执行 `f` 并等待结果；已在主线程时直接执行。超时（主线程卡住）时 `f` 作废，不会稍后再执行
/// （见 [`super::post_and_wait`]）
fn on_main_sync<T: Send + 'static>(
    window: &tauri::WebviewWindow,
    f: impl FnOnce(MainThreadMarker, &tauri::WebviewWindow, &NSWindow) -> T + Send + 'static,
) -> Result<T, IpcError> {
    if let Some(mtm) = MainThreadMarker::new() {
        let ns = ns_window(window, mtm)?;
        return Ok(f(mtm, window, &ns));
    }
    let w = window.clone();
    super::post_and_wait(
        |job| window.run_on_main_thread(job),
        move || {
            MainThreadMarker::new()
                .ok_or_else(|| IpcError::internal("not on main thread"))
                .and_then(|mtm| ns_window(&w, mtm).map(|ns| f(mtm, &w, &ns)))
        },
        MAIN_THREAD_TIMEOUT,
    )?
}

/// 在主线程执行 `f`，不等待
fn on_main_async(
    window: &tauri::WebviewWindow,
    f: impl FnOnce(MainThreadMarker, &tauri::WebviewWindow, &NSWindow) + Send + 'static,
) {
    let w = window.clone();
    let posted = window.run_on_main_thread(move || {
        let Some(mtm) = MainThreadMarker::new() else { return };
        match ns_window(&w, mtm) {
            Ok(ns) => f(mtm, &w, &ns),
            Err(e) => log::warn!("{}: {}", w.label(), e.message),
        }
    });
    if let Err(e) = posted {
        log::warn!("run_on_main_thread failed: {e}");
    }
}

/// 主屏高度（Cocoa 与左上原点坐标互换用）
fn primary_height(mtm: MainThreadMarker) -> f64 {
    NSScreen::screens(mtm)
        .firstObject()
        .map_or(0.0, |s| s.frame().size.height)
}

/// 全局逻辑矩形（左上原点、主屏左上为 (0, 0)）→ Cocoa 屏幕坐标（左下原点、y 向上）
fn to_cocoa(rect: Rect, mtm: MainThreadMarker) -> NSRect {
    NSRect::new(
        NSPoint::new(rect.x, primary_height(mtm) - rect.y - rect.height),
        NSSize::new(rect.width, rect.height),
    )
}

/// Cocoa 屏幕坐标 → 全局逻辑矩形
fn from_cocoa(r: NSRect, mtm: MainThreadMarker) -> Rect {
    Rect::new(
        r.origin.x,
        primary_height(mtm) - r.origin.y - r.size.height,
        r.size.width,
        r.size.height,
    )
}

// ---------------------------------------------------------------------------------------------------------------
// 面板

/// 把面板窗口转换为非激活 NSPanel（层级 `NSStatusWindowLevel` 25），Native 帧模式下再装上原生卡片表面。
/// 预热时调用一次（窗口仍隐藏）
pub fn panel_attach(
    window: &tauri::WebviewWindow,
    frame_mode: FrameMode,
    surface: CardSurface,
) -> Result<(), IpcError> {
    on_main_sync(window, move |mtm, window, _| -> Result<(), IpcError> {
        let panel = window.to_panel::<panel_class::WispPanel<tauri::Wry>>()?;
        if let Err(e) = panel.add_style_mask(NSWindowStyleMask::NonactivatingPanel) {
            log::warn!("panel style mask: {e}");
        }
        panel.set_level(NSStatusWindowLevel as i64);
        panel.set_collection_behavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        // to_panel 换了 isa，重新取一次
        let ns = ns_window(window, mtm)?;
        if frame_mode == FrameMode::Native {
            install_card_surface(&ns, surface, mtm)?;
        }
        Ok(())
    })?
}

/// 更新原生卡片表面的颜色（外观变化时；Css 帧模式下没有原生表面，忽略）
pub fn panel_set_surface(window: &tauri::WebviewWindow, surface: CardSurface) {
    on_main_async(window, move |mtm, _, ns| {
        if let Err(e) = install_card_surface(ns, surface, mtm) {
            log::warn!("update card surface: {}", e.message);
        }
    });
}

/// 立即把面板放到 `rect`（桌面坐标），停止正在进行的高度动画。窗口隐藏时也可以调用（prepare 阶段先定宽，
/// 让前端按最终宽度同步测量）。进行中的退出淡出会先立即完成（orderOut 并回调 `done`）：新的 prepare 打断淡出时，
/// 旧内容不会被带到新矩形上闪一下
pub fn panel_set_frame(window: &tauri::WebviewWindow, rect: Rect) -> Result<(), IpcError> {
    on_main_sync(window, move |mtm, _, ns| {
        let driver = FrameDriver::for_window(ns, mtm);
        driver.finish_exit();
        driver.stop_geometry();
        let frame = to_cocoa(rect, mtm);
        let t0 = Instant::now();
        ns.setFrame_display(frame, ns.isVisible());
        ns.invalidateShadow();
        driver.record_immediate(t0.elapsed());
    })
}

/// 进行中的退出淡出立即完成（orderOut 并回调它的 `done`）；没有淡出时什么都不做
pub fn panel_finish_exit(window: &tauri::WebviewWindow) {
    if let Err(e) = on_main_sync(window, |mtm, _, ns| FrameDriver::for_window(ns, mtm).finish_exit()) {
        log::warn!("finish panel exit: {}", e.message);
    }
}

/// 在 `rect` 处不激活应用地显示面板（`orderFrontRegardless`）。`opts.alpha`：Native 帧模式的进入是窗口 alpha 0→1
/// 淡入，ready 超时先以 alpha 0 显示（用户看不到，窗口可见后 WebView 恢复渲染），Css 帧模式直接不透明（进入动画由 CSS 播放）。
/// `opts.key` 为 true 时同时成为 key（输入 / 剪贴板视图，非激活面板成为 key 不激活应用）。
/// 同步：返回时窗口已在屏幕上
pub fn panel_show(
    window: &tauri::WebviewWindow,
    rect: Rect,
    opts: ShowOpts,
    ticket: &ShowTicket,
) -> Result<(), IpcError> {
    let ticket = ticket.clone();
    on_main_sync(window, move |mtm, _, ns| -> Result<(), IpcError> {
        // 决定显示之后又开始了隐藏（它的 orderOut 排在这之后或已经执行）：这次显示作废
        if !ticket.valid() {
            return Err(ShowTicket::superseded());
        }
        let driver = FrameDriver::for_window(ns, mtm);
        // 上一张冷唤出外壳的输入守卫（没等到 ready 就结束的）不再需要
        shell_guard_finish(None);
        // 取消未完成的退出（连同它的 orderOut）与几何动画；上一次冷唤出的外壳没等到 ready 就结束时内容还是透明的，复位
        driver.stop_alpha();
        driver.stop_geometry();
        driver.reset_content(ns);
        ns.setFrame_display(to_cocoa(rect, mtm), false);
        ns.setAlphaValue(match opts.alpha {
            ShowAlpha::Opaque => 1.0,
            ShowAlpha::Transparent | ShowAlpha::FadeIn(_) => 0.0,
        });
        // ready 超时后的 alpha 0 显示：用户看不到它，也不能让它吞掉点击（迟到的 ready 在 panel_reveal 里恢复）
        ns.setIgnoresMouseEvents(opts.alpha == ShowAlpha::Transparent);
        ns.orderFrontRegardless();
        if opts.key {
            make_key_next_turn(ns, ticket.clone());
        }
        ns.invalidateShadow();
        if let ShowAlpha::FadeIn(fade) = opts.alpha {
            driver.fade(1.0, fade, None);
        }
        Ok(())
    })?
}

/// 下一轮主线程事件循环再成为 key（否则输入 / 剪贴板视图的 ready→shown 比翻译视图多 10–30 ms）：与显示在同一个 CA
/// 事务里 makeKeyWindow 时，WebKit 先处理「窗口成为 key、第一响应者、输入框聚焦」再画第一帧；推迟一轮让第一帧先出来。
/// 一轮事件循环只有几毫秒，用户在面板出现之前就开始打字也不会落到别的应用（按键事件同样排在这之后）。期间开始了隐藏
/// （票作废）或窗口已离屏时不再成为 key
fn make_key_next_turn(ns: &NSWindow, ticket: ShowTicket) {
    let weak = Weak::new(ns);
    let block = block2::RcBlock::new(move || {
        if let Some(ns) = weak.load()
            && ticket.valid()
            && ns.isVisible()
        {
            ns.makeKeyWindow();
        }
    });
    // SAFETY: 主线程上调用；块只捕获弱引用与票，在主线程的事件循环里执行一次
    unsafe { NSRunLoop::mainRunLoop().performBlock(&block) };
}

/// 已经在屏幕上（alpha 0，ready 超时后）的面板改到 `rect` 并淡入；票已作废（期间开始了隐藏）时什么都不做
pub fn panel_reveal(
    window: &tauri::WebviewWindow,
    rect: Rect,
    fade: Option<Fade>,
    key: bool,
    ticket: &ShowTicket,
) -> Result<(), IpcError> {
    let ticket = ticket.clone();
    on_main_sync(window, move |mtm, _, ns| -> Result<(), IpcError> {
        if !ticket.valid() {
            return Err(ShowTicket::superseded());
        }
        let driver = FrameDriver::for_window(ns, mtm);
        driver.stop_geometry();
        driver.reset_content(ns);
        ns.setFrame_display(to_cocoa(rect, mtm), true);
        ns.invalidateShadow();
        ns.setIgnoresMouseEvents(false);
        if key {
            ns.makeKeyWindow();
        }
        match fade {
            Some(f) => driver.fade(1.0, f, None),
            None => {
                driver.stop_alpha();
                ns.setAlphaValue(1.0);
            }
        }
        Ok(())
    })?
}

/// 冷唤出的原生首帧（见 `PanelController::shell_for_pending`）：页面还在加载时，在一次主线程往返里把窗口放到
/// `rect`、WKWebView 固定为 `max_height` 高（[`panel_pin_webview`]），不激活地显示卡片外壳——原生材质、着色层、阴影都在
/// 原生层，WKWebView 的内容先保持透明（不是 hidden：hidden 的 WebView 不渲染，页面的第一次排版会被推迟）。
/// 窗口 alpha 按 `fade` 淡入（与热唤出同一条进入曲线）。外壳期间：
/// - `key`（输入 / 剪贴板视图）时**立即**成为 key、第一响应者是 WKWebView（非激活面板：不激活应用、菜单栏不变）——外壳没有
///   要先画出来的内容，不必像 [`panel_show`] 那样推迟一轮；用户看到卡片就开始打字，按键也不会落到之前的前台应用；
/// - 装上输入守卫（[`shell_guard_install`]）：落在外壳上的按键按原顺序缓存、ready 时重放，Esc 调 `on_escape`；鼠标事件
///   照常送到窗口（不穿透到下面的应用），由守卫吞掉（页面里还看不见的控件不会被点到）。
///
/// 同步：返回时窗口已在屏幕上
pub fn panel_show_shell(
    window: &tauri::WebviewWindow,
    rect: Rect,
    max_height: f64,
    fade: Option<Fade>,
    key: bool,
    on_escape: ShellEscape,
    ticket: &ShowTicket,
) -> Result<(), IpcError> {
    let ticket = ticket.clone();
    on_main_sync(window, move |mtm, _, ns| -> Result<(), IpcError> {
        if !ticket.valid() {
            return Err(ShowTicket::superseded());
        }
        let driver = FrameDriver::for_window(ns, mtm);
        driver.finish_exit();
        driver.stop_alpha();
        driver.stop_geometry();
        driver.hide_content(ns);
        ns.setFrame_display(to_cocoa(rect, mtm), false);
        pin_webview_on_main(ns, max_height)?;
        ns.setAlphaValue(if fade.is_some() { 0.0 } else { 1.0 });
        ns.setIgnoresMouseEvents(false);
        ns.orderFrontRegardless();
        // 上屏之后再装（窗口号此时一定有效）；同一次主线程执行里，中间不会有事件被派发
        shell_guard_install(ns, on_escape);
        if key {
            make_key_with_webview(ns);
        }
        ns.invalidateShadow();
        if let Some(fade) = fade {
            driver.fade(1.0, fade, None);
        }
        Ok(())
    })?
}

/// 外壳上的会话 ready：内容按 `fade` 淡入（WKWebView 的 alpha，由同一个 displayLink 驱动），高度从外壳的估计值
/// 用 spring.gentle 弹到 `rect`（固定边按 `placement`）；位置 / 宽度也不同（外壳正在移向新会话的矩形，见
/// [`panel_move_shell`]）时整个矩形接着用矩形弹簧走到位；`animate` 为 false（减少动效）时直接到位。窗口 alpha 若还在淡入
/// 就让它继续。`key` 时成为 key：外壳上已经截住了按键（窗口本来就是 key）就立即，否则与 [`panel_show`] 相同推迟一轮
/// （先让第一帧出来）。最后卸掉输入守卫，把外壳期间截住的按键按原顺序放回事件队列最前面——页面此时已经聚焦了输入框
/// （视图在 prepare 时挂载并自动聚焦），按键照常进入页面。票已作废时什么都不做
pub fn panel_reveal_shell(
    window: &tauri::WebviewWindow,
    rect: Rect,
    placement: Placement,
    fade: Fade,
    key: bool,
    animate: bool,
    ticket: &ShowTicket,
) -> Result<(), IpcError> {
    let ticket = ticket.clone();
    on_main_sync(window, move |mtm, _, ns| -> Result<(), IpcError> {
        if !ticket.valid() {
            return Err(ShowTicket::superseded());
        }
        let driver = FrameDriver::for_window(ns, mtm);
        let target = to_cocoa(rect, mtm);
        let frame = ns.frame();
        let same_column =
            (frame.origin.x - target.origin.x).abs() < 0.5 && (frame.size.width - target.size.width).abs() < 0.5;
        let same_edge = match placement {
            Placement::Above => (frame.origin.y - target.origin.y).abs() < 0.5,
            Placement::Below | Placement::Center => {
                (frame.origin.y + frame.size.height - (target.origin.y + target.size.height)).abs() < 0.5
            }
        };
        if !ns.isVisible() {
            // 防御：控制器认为外壳在屏幕上，窗口却没有上屏（外壳的显示被作废、与交接竞争……）。
            // 直接按最终矩形不激活地上屏、整窗淡入——不从一个从未显示过的旧位置弹过来，也不留下「控制器以为在显示、
            // 实际看不见」的面板
            log::warn!("panel shell reveal: the window was not on screen; ordering it front");
            driver.stop_geometry();
            ns.setFrame_display(target, false);
            ns.setAlphaValue(0.0);
            ns.orderFrontRegardless();
            ns.invalidateShadow();
            driver.fade(1.0, fade, None);
        } else if !animate {
            driver.stop_geometry();
            ns.setFrame_display(target, true);
            ns.invalidateShadow();
        } else if same_column && same_edge && !driver.rect_running() {
            driver.animate_height(ns, rect.height, placement, None, SpringParams::GENTLE);
        } else {
            driver.animate_rect(ns, target);
        }
        ns.setIgnoresMouseEvents(false);
        driver.fade_content(ns, fade);
        if ns.alphaValue() < 1.0 && !driver.alpha_running() {
            driver.fade(1.0, fade, None);
        }
        let buffered = shell_guard_pending();
        if key && !ns.isKeyWindow() {
            if buffered > 0 {
                make_key_with_webview(ns);
            } else {
                make_key_next_turn(ns, ticket.clone());
            }
        }
        let replayed = shell_guard_finish(ns.isKeyWindow().then_some(mtm));
        if replayed > 0 {
            log::debug!("panel shell: replayed {replayed} key events typed before the page was ready");
        }
        Ok(())
    })?
}

/// 外壳在屏幕上、外壳期间来了几何不同的新会话：`max_height` 为 Some 时先重新固定 WKWebView（内容透明，
/// 看不见重排），再把窗口用矩形弹簧移到 `rect`（`animate` 为 false 时直接到位）。异步（排在已投递的主线程操作之后）
pub fn panel_move_shell(window: &tauri::WebviewWindow, rect: Rect, max_height: Option<f64>, animate: bool) {
    on_main_async(window, move |mtm, _, ns| {
        if let Some(h) = max_height
            && let Err(e) = pin_webview_on_main(ns, h)
        {
            log::warn!("panel shell pin webview failed: {}", e.message);
        }
        let driver = FrameDriver::for_window(ns, mtm);
        let target = to_cocoa(rect, mtm);
        if animate && ns.isVisible() {
            driver.animate_rect(ns, target);
        } else {
            driver.stop_geometry();
            ns.setFrame_display(target, ns.isVisible());
            ns.invalidateShadow();
        }
    });
}

/// 成为 key，并确保第一响应者是 WKWebView（按键才进得了页面）。非激活 NSPanel：不激活应用、菜单栏不切换
fn make_key_with_webview(ns: &NSWindow) {
    ns.makeKeyWindow();
    let responder_is_window = ns
        .firstResponder()
        .is_none_or(|r| std::ptr::addr_eq(Retained::as_ptr(&r), std::ptr::from_ref(ns)));
    if responder_is_window && let Some((_, web)) = ns.contentView().and_then(|v| find_webview(&v)) {
        ns.makeFirstResponder(Some(&web));
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 冷唤出外壳的输入守卫（外壳期间不丢字、不让按键与点击落到别的应用）

/// 外壳期间最多缓存的按键事件数（按下与抬起各算一个；外壳通常只在屏幕上一两百毫秒，最多约 2 s）
const SHELL_KEYS_MAX: usize = 256;
/// Esc 的虚拟键码（kVK_Escape）
const KEY_ESCAPE: u16 = 53;

/// 外壳的输入守卫：一个本地事件监视器，只处理落在外壳窗口上的事件（其他窗口的事件原样放行）
struct ShellGuard {
    monitor: Retained<AnyObject>,
    /// 按原顺序缓存的按键（ready 时重放）
    keys: Vec<Retained<NSEvent>>,
    /// 已缓存了按下事件的键码：抬起事件只缓存这些（触发快捷键本身的抬起落在外壳上，不重放）
    down: HashSet<u16>,
    /// 外壳期间按过 Esc：外壳正在收起，之后的按键不再缓存
    escaped: bool,
}

thread_local! {
    /// 当前外壳的输入守卫（只有一个面板窗口；主线程）
    static SHELL_GUARD: RefCell<Option<ShellGuard>> = const { RefCell::new(None) };
}

/// window lab：最近一张外壳截住 / 重放的按键事件数
#[cfg(any(debug_assertions, feature = "fake-llm"))]
static SHELL_KEY_STATS: parking_lot::Mutex<(usize, usize)> = parking_lot::Mutex::new((0, 0));

/// 装上外壳的输入守卫（主线程；已有的先卸掉、不重放）。监视器只看 `ns` 上的事件：
/// - 按键：Esc（无修饰键）不缓存，清空已缓存的按键，下一轮事件循环调用 `on_escape`（不在监视器里重入控制器）；其余按下
///   事件、以及已缓存按下的那些键的抬起事件按原顺序缓存（上限 [`SHELL_KEYS_MAX`]），一律吞掉——WKWebView 此刻没有页面，
///   送进去的按键只会丢失（或响一声）；
/// - 鼠标按下 / 抬起 / 拖动 / 滚轮：吞掉（窗口照常接收鼠标，点击不会穿透到下面的应用，也不会落到页面里看不见的控件上）
fn shell_guard_install(ns: &NSWindow, on_escape: ShellEscape) {
    shell_guard_finish(None);
    #[cfg(any(debug_assertions, feature = "fake-llm"))]
    {
        *SHELL_KEY_STATS.lock() = (0, 0);
    }
    let window_number = ns.windowNumber();
    let mask = NSEventMask::KeyDown
        | NSEventMask::KeyUp
        | NSEventMask::LeftMouseDown
        | NSEventMask::LeftMouseUp
        | NSEventMask::LeftMouseDragged
        | NSEventMask::RightMouseDown
        | NSEventMask::RightMouseUp
        | NSEventMask::RightMouseDragged
        | NSEventMask::OtherMouseDown
        | NSEventMask::OtherMouseUp
        | NSEventMask::OtherMouseDragged
        | NSEventMask::ScrollWheel;
    let block = block2::RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: AppKit 传入的事件在回调期间有效
        let e = unsafe { event.as_ref() };
        if e.windowNumber() != window_number {
            return event.as_ptr();
        }
        if matches!(e.r#type(), NSEventType::KeyDown | NSEventType::KeyUp) && shell_guard_key(e) {
            let on_escape = std::sync::Arc::clone(&on_escape);
            let call = block2::RcBlock::new(move || on_escape());
            // SAFETY: 主线程上调用；块在主线程的事件循环里执行一次
            unsafe { NSRunLoop::mainRunLoop().performBlock(&call) };
        }
        std::ptr::null_mut()
    });
    // SAFETY: 块返回传入的事件本身或 null（吞掉）
    let monitor = unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &block) };
    match monitor {
        Some(monitor) => SHELL_GUARD.with(|slot| {
            *slot.borrow_mut() = Some(ShellGuard {
                monitor,
                keys: Vec::new(),
                down: HashSet::new(),
                escaped: false,
            });
        }),
        None => log::warn!("panel shell: local event monitor unavailable"),
    }
}

/// 一次落在外壳上的按键（监视器里调用）。返回 true 表示这是外壳期间的第一次 Esc（调用方安排 `on_escape`）
fn shell_guard_key(e: &NSEvent) -> bool {
    SHELL_GUARD.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(g) = slot.as_mut() else { return false };
        let code = e.keyCode();
        let down = e.r#type() == NSEventType::KeyDown;
        let modifiers = e.modifierFlags()
            & (NSEventModifierFlags::Command
                | NSEventModifierFlags::Control
                | NSEventModifierFlags::Option
                | NSEventModifierFlags::Shift);
        if down && code == KEY_ESCAPE && modifiers.is_empty() {
            let first = !g.escaped;
            g.escaped = true;
            g.keys.clear();
            g.down.clear();
            return first;
        }
        if g.escaped || g.keys.len() >= SHELL_KEYS_MAX {
            return false;
        }
        let keep = if down {
            g.down.insert(code);
            true
        } else {
            g.down.remove(&code)
        };
        if keep {
            g.keys.push(e.retain());
            #[cfg(any(debug_assertions, feature = "fake-llm"))]
            {
                SHELL_KEY_STATS.lock().0 += 1;
            }
        }
        false
    })
}

/// 外壳的输入守卫里缓存了几个按键事件
fn shell_guard_pending() -> usize {
    SHELL_GUARD.with(|slot| slot.borrow().as_ref().map_or(0, |g| g.keys.len()))
}

/// 卸掉外壳的输入守卫（主线程；没有时什么都不做）。`replay` 为 Some（ready、窗口是 key）时把缓存的按键按原顺序放回
/// 事件队列**最前面**（倒序 `postEvent:atStart:YES`）——之后才到的真实按键排在它们后面，顺序与用户敲的一致；按过 Esc
/// 的外壳不重放。返回重放了几个事件
fn shell_guard_finish(replay: Option<MainThreadMarker>) -> usize {
    let Some(g) = SHELL_GUARD.with(|slot| slot.borrow_mut().take()) else {
        return 0;
    };
    // SAFETY: 由 addLocalMonitorForEventsMatchingMask 返回的监视器
    unsafe { NSEvent::removeMonitor(&g.monitor) };
    let Some(mtm) = replay.filter(|_| !g.escaped) else {
        return 0;
    };
    let app = NSApplication::sharedApplication(mtm);
    for e in g.keys.iter().rev() {
        app.postEvent_atStart(e, true);
    }
    #[cfg(any(debug_assertions, feature = "fake-llm"))]
    {
        SHELL_KEY_STATS.lock().1 = g.keys.len();
    }
    g.keys.len()
}

/// Native 帧模式：把卡片高度改为 `height`。`animate` 为 true 时用弹簧重定目标，保留当前位置与速度——弹簧默认
/// `spring.gentle`，浮层预留引起的长高 / 回收由前端指定 `spring.snappy`（与 Css 帧卡片高度同一规则）；进行中的
/// 运动换参数时位置与速度连续。为 false（系统「减少动效」）时立即到位。Below / Center 时顶边固定，Above 时底边固定——
/// 固定边每一帧都从窗口**当前**的位置取（用户拖动窗口时不被拉回旧位置）。`bound`：拖动过的面板的竖直边界
/// （[`VerticalBound`]），卡片伸出边界时整体推回
pub fn panel_animate_height(
    window: &tauri::WebviewWindow,
    height: f64,
    placement: Placement,
    bound: Option<VerticalBound>,
    animate: bool,
    spring: wisp_types::ResizeSpring,
) {
    on_main_async(window, move |mtm, _, ns| {
        let driver = FrameDriver::for_window(ns, mtm);
        let bound = bound.map(|b| cocoa_bound(b, mtm));
        if animate {
            driver.animate_height(ns, height, placement, bound, SpringParams::of(spring));
        } else {
            // 只停高度 / 矩形动画；已排好的宽度切换（原地替换）照常在它的时刻执行
            driver.stop_resize();
            let frame = ns.frame();
            let h = sizing::snap_to_pixel(height, ns.backingScaleFactor()).max(1.0);
            let y = height_frame_y(frame, Edge::of(placement), h, bound);
            ns.setFrame_display(
                NSRect::new(NSPoint::new(frame.origin.x, y), NSSize::new(frame.size.width, h)),
                true,
            );
            ns.invalidateShadow();
        }
    });
}

/// Native 帧模式：进行中的高度弹簧停在当前高度（用户开始拖动面板）。`performWindowDragWithEvent` 由 WindowServer
/// 移动窗口，拖动期间 `ns.frame()` 可能滞后，逐帧 `setFrame` 会把窗口拽回旧位置或打断拖动；拖动结束时控制器重新
/// `panel_animate_height`，弹簧从窗口当前高度继续。矩形弹簧与已排好的宽度切换不受影响（拖动期间控制器不会再排新的）
pub fn panel_freeze_height(window: &tauri::WebviewWindow) {
    on_main_async(window, |mtm, _, ns| {
        FrameDriver::for_window(ns, mtm).stop_height();
    });
}

/// 桌面坐标的竖直边界 → Cocoa 坐标的 (窗口原点 y 的下限, 窗口顶边的上限)
fn cocoa_bound(b: VerticalBound, mtm: MainThreadMarker) -> (f64, f64) {
    let r = to_cocoa(Rect::new(0.0, b.top, 1.0, (b.bottom - b.top).max(0.0)), mtm);
    (r.origin.y, r.origin.y + r.size.height)
}

/// 高为 `height` 时窗口原点的 y（Cocoa 坐标）：固定边取窗口当前的位置；有边界、且窗口此刻在边界内（容差 1 pt）时，伸出
/// 边界的部分整体推回（顶边优先：卡片比边界还高时顶边贴住上限）
fn height_frame_y(frame: NSRect, edge: Edge, height: f64, bound: Option<(f64, f64)>) -> f64 {
    let y = match edge {
        Edge::Top => frame.origin.y + frame.size.height - height,
        Edge::Bottom => frame.origin.y,
    };
    match bound {
        Some((min_y, max_top))
            if frame.origin.y >= min_y - 1.0 && frame.origin.y + frame.size.height <= max_top + 1.0 =>
        {
            y.max(min_y).min(max_top - height)
        }
        _ => y,
    }
}

/// 隐藏面板。`fade` 为 Some 时先把窗口 alpha 淡到 0（Native 帧模式的退出：fast + ease.exit；减少动效时 100 ms 线性），
/// 再 `orderOut`；否则立即 `orderOut`（Css 帧模式的退出动画已由 CSS 播完；粘贴时不播放退出动画）。
/// `done` 在窗口真正离屏后于主线程调用；被新的 show 打断时不会调用
pub fn panel_hide(window: &tauri::WebviewWindow, fade: Option<Fade>, done: Box<dyn FnOnce() + Send>) {
    on_main_async(window, move |mtm, _, ns| {
        // 冷唤出外壳期间被隐藏：输入守卫连同截住的按键一起丢掉
        shell_guard_finish(None);
        let driver = FrameDriver::for_window(ns, mtm);
        driver.stop_geometry();
        // 冷唤出外壳上的内容淡入还没播完：停在当前值（下一次显示复位），不能让它在离屏窗口上留着 displayLink
        driver.stop_content();
        match fade {
            Some(fade) if ns.isVisible() => {
                let weak = Weak::new(ns);
                let then: Box<dyn FnOnce()> = Box::new(move || {
                    if let Some(ns) = weak.load() {
                        ns.orderOut(None);
                    }
                    done();
                });
                driver.fade(0.0, fade, Some(then));
            }
            _ => {
                driver.stop_alpha();
                ns.orderOut(None);
                done();
            }
        }
    });
}

/// macOS 的面板始终可以成为 key（非激活 NSPanel），不需要切换
pub fn panel_set_focusable(window: &tauri::WebviewWindow, focusable: bool) {
    let _ = (window, focusable);
}

/// 翻译视图被点击后成为 key（非激活面板被点击时 AppKit 通常已经让它成为 key，这里兜底；不激活应用）
pub fn panel_activate(window: &tauri::WebviewWindow) {
    on_main_async(window, |_, _, ns| {
        if ns.isVisible() && !ns.isKeyWindow() {
            ns.makeKeyWindow();
        }
    });
}

/// WebView2 的可见性与内存目标只在 Windows 上有意义。macOS 上为了停掉会话后 WebKit「把 backing store
/// 标记为易失」的无限重试，在这里试过的做法都不可用：
/// - 隐藏 WKWebView、把它缩成 0×0：循环照旧；
/// - 隐藏期间把 `configuration.preferences.inactiveSchedulingPolicy` 切到 Suspend、prepare 前切回 None：运行期改动不影响
///   进程节流，隐藏 11 分钟既不挂起也不降频（推断 WebKit 只在页面接入 WebContent 进程时读这个策略；模板上的设置有效）；
/// - 从窗口摘下（`removeFromSuperview`）：tauri-nspanel 在 WKWebView 装进窗口**之后**才换掉窗口的类（`to_panel`），
///   WebKit 的 `WKWindowVisibilityObserver` 之前在窗口上登记的 KVO（`contentLayoutRect` 等）因此失配，WKWebView 一离开
///   窗口（`viewWillMoveToWindow:nil` → `stopObserving`）就抛 Objective-C 异常（「Cannot remove an observer … because it
///   is not registered as an observer」），进程中止。**任何代码都不能把面板的 WKWebView 从窗口里摘下**
pub fn panel_restore_visibility(window: &tauri::WebviewWindow) {
    let _ = window;
}

/// WebView2 的可见性与内存目标只在 Windows 上有意义（macOS 上试过的做法见 [`panel_restore_visibility`]）
pub fn panel_release_visibility(window: &tauri::WebviewWindow, low_memory: bool) {
    let _ = (window, low_memory);
}

/// Native 帧模式窗口即卡片，没有透明富余区；Css 退路的透明边距靠 AppKit 对透明像素的命中测试放行
pub fn panel_set_click_through(window: &tauri::WebviewWindow, region: Option<ClickRegion>) {
    let _ = (window, region);
}

/// Native 帧模式：把面板的 WKWebView 固定为 `height` 高（本次会话的卡片上限）、贴住窗口顶边，之后窗口逐帧变高 /
/// 变矮时 WebView 的尺寸不再变化，超出窗口的部分被窗口裁掉：
/// - WKWebView 尺寸一变，WebKit 会让包含它的整个 CA 事务等 WebContent 进程按新尺寸重排完再提交（约 1–2 帧），
///   而窗口几何是立即生效的：Above 放置（底边固定、顶边上移）时旧内容贴着新的顶边画，卡片底边在增长期间上下抖动；
/// - 尺寸不变则没有这道等待：原生材质 / 着色层与窗口几何在同一帧提交，WebContent 也不必每帧重排（CPU 更低）。
///
/// 前端相应地按内容自然高度排版（卡片不能用 `height: 100%`、不能把元素钉在视口底部），见 PanelCard 的说明。
/// 窗口隐藏时（prepare 阶段）调用
pub fn panel_pin_webview(window: &tauri::WebviewWindow, height: f64) -> Result<(), IpcError> {
    on_main_sync(window, move |_, _, ns| pin_webview_on_main(ns, height))?
}

/// [`panel_pin_webview`] 的主线程部分（宽度切换在 displayLink 回调里与改窗口宽度合在同一帧调用）
fn pin_webview_on_main(ns: &NSWindow, height: f64) -> Result<(), IpcError> {
    let content = ns
        .contentView()
        .ok_or_else(|| IpcError::internal("panel has no content view"))?;
    let (parent, webview) = find_webview(&content).ok_or_else(|| IpcError::internal("panel has no WKWebView"))?;
    let bounds = parent.bounds();
    let flipped = parent.isFlipped();
    let y = if flipped { 0.0 } else { bounds.size.height - height };
    let pin_top = if flipped {
        NSAutoresizingMaskOptions::ViewMaxYMargin
    } else {
        NSAutoresizingMaskOptions::ViewMinYMargin
    };
    webview.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | pin_top);
    webview.setFrame(NSRect::new(
        NSPoint::new(0.0, y),
        NSSize::new(bounds.size.width, height),
    ));
    Ok(())
}

/// 原地替换的宽度切换（见 [`WidthSwitch`]）：由窗口的 displayLink 在 `delay` 之后的第一帧执行——那一帧把左边与宽度
/// 改到位（高度弹簧在进行时与它合成同一个 `setFrame`），并在同一个 CA 事务里重新固定 WebView。被新的定位 / 显示 /
/// 隐藏打断时作废（`stop_geometry`）；高度变化（`panel_animate_height`）不影响它
pub fn panel_switch_width(window: &tauri::WebviewWindow, switch: WidthSwitch) {
    on_main_async(window, move |mtm, _, ns| {
        let at = objc2_quartz_core::CACurrentMediaTime() + switch.delay.as_secs_f64();
        // 桌面坐标（左上原点）的 x 与 Cocoa 的 x 相同，只有 y 需要换算
        let width = sizing::snap_to_pixel(switch.width, ns.backingScaleFactor()).max(1.0);
        FrameDriver::for_window(ns, mtm).schedule_width(
            ns,
            PendingWidth {
                x: switch.x,
                width,
                at,
                pin: switch.pin,
            },
        );
    });
}

/// contentView 子树里的 WKWebView（wry 的 WryWebView 是它的子类），连同它的父视图
fn find_webview(view: &NSView) -> Option<(Retained<NSView>, Retained<NSView>)> {
    let class = objc2::runtime::AnyClass::get(c"WKWebView")?;
    view.subviews().iter().find_map(|v| {
        if v.isKindOfClass(class) {
            Some((view.retain(), v.clone()))
        } else {
            find_webview(&v)
        }
    })
}

/// 打开逐帧统计（frame lab 用）：每次动画结束时以 info 级别输出一行 `frame_stats`
pub fn set_frame_stats(enabled: bool) {
    STATS_ENABLED.store(enabled, Ordering::Relaxed);
}

/// 面板 WKWebView 的 WebContent 进程 pid（frame lab 度量 CPU 用；私有 API，只在 debug 构建里编译）
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn web_content_pid(window: &tauri::WebviewWindow) -> Option<i32> {
    fn find(view: &NSView) -> Option<i32> {
        let selector = sel!(_webProcessIdentifier);
        if view.respondsToSelector(selector) {
            // SAFETY: WKWebView 的 `_webProcessIdentifier` 返回 pid_t；已用 respondsToSelector 确认存在
            let pid: i32 = unsafe { msg_send![view, _webProcessIdentifier] };
            return Some(pid);
        }
        view.subviews().iter().find_map(|v| find(&v))
    }
    on_main_sync(window, |_, _, ns| ns.contentView().and_then(|v| find(&v)))
        .ok()
        .flatten()
}

// ---------------------------------------------------------------------------------------------------------------
// 任意窗口的矩形

/// 窗口的外框矩形（桌面坐标）
pub fn window_frame(window: &tauri::WebviewWindow) -> Option<Rect> {
    on_main_sync(window, |mtm, _, ns| from_cocoa(ns.frame(), mtm)).ok()
}

/// 鼠标左键或右键（交换主次键后拖动用的是右键）此刻是否按着（物理按键，任意线程）：拖动面板时据此区分「用户按住不动」
/// 与「已经松开」
pub fn primary_button_down() -> bool {
    // 位 0 左键、位 1 右键
    NSEvent::pressedMouseButtons() & 0b11 != 0
}

/// 把窗口外框放到 `rect`（桌面坐标）
pub fn set_window_frame(window: &tauri::WebviewWindow, rect: Rect) -> Result<(), IpcError> {
    on_main_sync(window, move |mtm, _, ns| {
        FrameDriver::for_window(ns, mtm).stop_geometry();
        ns.setFrame_display(to_cocoa(rect, mtm), ns.isVisible());
    })
}

/// 截图前隐藏窗口后：SCK 的过滤器本来就排除了 Wisp 自己的窗口（与截图前隐藏互为双重保险），不必等合成
pub fn flush_after_hide() {}

/// 是否为「重新打开」事件（Finder / 启动台 / Spotlight / Dock 再次打开已在运行的 Wisp，`RunEvent::Reopen`）。
/// 这个变体只在 macOS 上存在，应用层不能写 `cfg(target_os)`，所以由 native 提供
pub fn is_reopen(ev: &tauri::RunEvent) -> bool {
    matches!(ev, tauri::RunEvent::Reopen { .. })
}

/// macOS 的非激活面板从不激活应用，失焦关闭后焦点本来就在用户点中的地方，不需要判断外壳（`RESTORE_FOCUS_ON_HIDE` 为 false）
pub fn foreground_is_shell() -> bool {
    false
}

/// 任意 Wisp 窗口销毁之后（`Destroyed`）：释放窗口已释放或已离屏的逐帧驱动（Rust 直接 `destroy()` 时不经过
/// `CloseRequested`，由这里兜底）
pub fn release_offscreen_drivers(app: &tauri::AppHandle) {
    let posted = app.run_on_main_thread(|| {
        let n = FrameDriver::release_offscreen();
        if n > 0 {
            log::debug!("released {n} offscreen frame drivers");
        }
    });
    if let Err(e) = posted {
        log::debug!("release frame drivers not scheduled: {e}");
    }
}

/// 诊断：窗口的可见性、alpha、遮挡状态与逐帧驱动（displayLink 是否在跑、各动画是否进行中）
pub fn describe_window(window: &tauri::WebviewWindow) -> String {
    on_main_sync(window, |_, _, ns| {
        let key = std::ptr::from_ref(ns) as usize;
        let driver = DRIVERS.with(|map| map.borrow().get(&key).map(|d| d.describe()));
        format!(
            "visible={} alpha={:.2} occlusion_visible={} driver=[{}]",
            ns.isVisible(),
            ns.alphaValue(),
            ns.occlusionState()
                .contains(objc2_app_kit::NSWindowOcclusionState::Visible),
            driver.unwrap_or_else(|| "none".to_owned())
        )
    })
    .unwrap_or_else(|e| format!("unavailable ({})", e.message))
}

/// 诊断（窗口泄漏检查）：主线程上数一遍 `NSApp.windows`（弱引用表，已释放的窗口不在其中）——按类名分成 Wisp 自己的窗口
/// （tao / tauri-nspanel 的类，含 KVO 子类）与 AppKit 自己的窗口（输入法光标窗、状态栏……）；再按窗口号把本进程在窗口服务器里的
/// 窗口（CGWindowList，含离屏）分成属于 Wisp 窗口的与其余的（AppKit 不放进 `NSApp.windows` 的菜单栏副本等）
pub fn window_census(app: &tauri::AppHandle) -> Option<super::WindowCensus> {
    fn count(mtm: MainThreadMarker) -> (Vec<i64>, usize) {
        objc2::rc::autoreleasepool(|_| {
            let (mut wisp, mut system) = (Vec::new(), 0);
            for w in NSApplication::sharedApplication(mtm).windows().iter() {
                if super::is_wisp_window_class(&w.class().name().to_string_lossy()) {
                    wisp.push(i64::try_from(w.windowNumber()).unwrap_or(-1));
                } else {
                    system += 1;
                }
            }
            (wisp, system)
        })
    }
    // 已在主线程时直接数（投递给主线程再等它，就是等自己，必然超时）
    let (wisp, system) = match MainThreadMarker::new() {
        Some(mtm) => count(mtm),
        None => super::post_and_wait(
            |job| app.run_on_main_thread(job),
            || MainThreadMarker::new().map(count),
            MAIN_THREAD_TIMEOUT,
        )
        .ok()
        .flatten()?,
    };
    let server = server_window_numbers()?;
    let server_wisp = server.iter().filter(|n| wisp.contains(n)).count();
    Some(super::WindowCensus {
        wisp: wisp.len(),
        system,
        server_wisp,
        server_other: server.len() - server_wisp,
    })
}

/// 本进程在窗口服务器里的窗口号（含离屏、不含已释放的窗口）
fn server_window_numbers() -> Option<Vec<i64>> {
    use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFString, CFType};
    use objc2_core_graphics::{CGWindowListCopyWindowInfo, CGWindowListOption, kCGNullWindowID};
    let pid = i64::from(std::process::id());
    // SAFETY: kCGWindowOwnerPID / kCGWindowNumber 是 CoreGraphics 导出的常量 CFString
    let (owner_key, number_key): (&CFString, &CFString) = unsafe {
        (
            objc2_core_graphics::kCGWindowOwnerPID,
            objc2_core_graphics::kCGWindowNumber,
        )
    };
    let list = CGWindowListCopyWindowInfo(CGWindowListOption::OptionAll, kCGNullWindowID)?;
    // SAFETY: CGWindowListCopyWindowInfo 返回 CFArray<CFDictionary<CFString, CFType>>
    let list: &CFArray<CFDictionary<CFString, CFType>> = unsafe { list.cast_unchecked() };
    let number = |info: &CFDictionary<CFString, CFType>, key: &CFString| {
        info.get(key)
            .and_then(|v| v.downcast_ref::<CFNumber>().and_then(|n| n.as_i64()))
    };
    Some(
        list.iter()
            .filter(|info| number(info, owner_key) == Some(pid))
            .filter_map(|info| number(&info, number_key))
            .collect(),
    )
}

/// 托盘图标的无障碍名称（VoiceOver 读出的内容）：NSStatusBarButton 的 accessibilityLabel，与提示文字同步
pub fn tray_set_accessibility(tray: &tauri::tray::TrayIcon, label: &str) {
    let label = label.to_owned();
    let result = tray.with_inner_tray_icon(move |inner| {
        let Some(mtm) = MainThreadMarker::new() else { return };
        let Some(item) = inner.ns_status_item() else { return };
        if let Some(button) = item.button(mtm) {
            use objc2_app_kit::NSAccessibility as _;
            let text = objc2_foundation::NSString::from_str(&label);
            button.setAccessibilityLabel(Some(&text));
        }
    });
    if let Err(e) = result {
        log::debug!("tray accessibility label not set: {e}");
    }
}

/// 设置窗口不进入全屏：偏好设置类窗口全屏后版面被拉得很空，不符合 macOS 惯例。
/// collectionBehavior 设 `FullScreenNone`，绿灯只做缩放（仍可调整大小）
pub fn settings_attach(window: &tauri::WebviewWindow) {
    let r = on_main_sync(window, |_, _, ns| {
        let behavior = (ns.collectionBehavior() - NSWindowCollectionBehavior::FullScreenPrimary)
            | NSWindowCollectionBehavior::FullScreenNone;
        ns.setCollectionBehavior(behavior);
    });
    if let Err(e) = r {
        log::debug!("settings window collection behavior not set: {}", e.message);
    }
}

/// 托盘按钮的「按下」动作：tray-icon 0.25 只在 mouseDown 时临时挂上菜单再 performClick，
/// 状态栏按钮本身没有 target / action，对它执行 AXPress（VoiceOver、⌃F8 聚焦状态菜单后按空格）打不开菜单。这里给
/// NSStatusBarButton 设上 target / action：AXPress 走 NSButton 的 performClick → 这个动作 → `on_press`（与左键相同：
/// 弹出托盘菜单）。鼠标点击仍由 tray-icon 盖在按钮上的视图处理（mouseDown 不传给按钮），不会触发两次
pub fn tray_enable_press(tray: &tauri::tray::TrayIcon, on_press: impl Fn() + Send + Sync + 'static) {
    let _ = TRAY_PRESS.set(Box::new(on_press));
    let result = tray.with_inner_tray_icon(move |inner| {
        let Some(mtm) = MainThreadMarker::new() else { return };
        let Some(item) = inner.ns_status_item() else { return };
        let Some(button) = item.button(mtm) else { return };
        let target = TRAY_PRESS_TARGET.with(|t| {
            t.get_or_init(|| {
                let this = TrayPressTarget::alloc(mtm).set_ivars(());
                // SAFETY: NSObject 的 init
                unsafe { msg_send![super(this), init] }
            })
            .clone()
        });
        // SAFETY: target 由线程局部变量持有、与进程同寿（NSControl 的 target 是弱引用）；action 是它实现的方法
        unsafe {
            button.setTarget(Some(&target));
            button.setAction(Some(sel!(onPress:)));
        }
    });
    if let Err(e) = result {
        log::debug!("tray press action not set: {e}");
    }
}

/// 托盘按钮被「按下」（AXPress）时调用的回调
static TRAY_PRESS: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>> = std::sync::OnceLock::new();

thread_local! {
    static TRAY_PRESS_TARGET: std::cell::OnceCell<Retained<TrayPressTarget>> = const { std::cell::OnceCell::new() };
}

define_class!(
    // SAFETY: NSObject 没有子类化要求；没有实例变量，不实现 Drop
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "WispTrayPressTarget"]
    struct TrayPressTarget;

    impl TrayPressTarget {
        #[unsafe(method(onPress:))]
        fn on_press(&self, _sender: Option<&AnyObject>) {
            if let Some(f) = TRAY_PRESS.get() {
                f();
            }
        }
    }
);

/// 把应用菜单里的「窗口」子菜单设为 NSApp 的 windowsMenu（系统自动补上窗口列表等项）
pub fn mark_windows_menu(menu: &tauri::menu::Submenu<tauri::Wry>) {
    if let Err(e) = menu.set_as_windows_menu_for_nsapp() {
        log::debug!("windows menu not set: {e}");
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Pinned

/// Pinned 的平台属性：隐藏三个交通灯（沿用模板的 `titleBarStyle: Overlay` + `hiddenTitle`，借系统圆角、
/// 阴影与边缘缩放）。窗口创建后、显示前调用
pub fn pinned_attach(window: &tauri::WebviewWindow) -> Result<(), IpcError> {
    on_main_sync(window, |_, _, ns| {
        for b in [
            NSWindowButton::CloseButton,
            NSWindowButton::MiniaturizeButton,
            NSWindowButton::ZoomButton,
        ] {
            if let Some(button) = ns.standardWindowButton(b) {
                button.setHidden(true);
            }
        }
    })
}

/// 固定交接：在**同一个主线程回调**里把 pinned 放到 `rect`、`orderFront`，并把面板
/// `orderOut`——两者进入同一个 CA 事务，屏幕上不存在空帧。`expand_to` 为 Some 时随后用 spring.gentle 把 pinned
/// 从 `rect` 逐帧过渡到目标矩形（`animate` 为 false，即系统「减少动效」时直接到位）。pinned 不成为 key（不激活应用，
/// 用户点击它时才激活）
pub fn pinned_handoff(
    pinned: &tauri::WebviewWindow,
    panel: Option<&tauri::WebviewWindow>,
    rect: Rect,
    expand_to: Option<Rect>,
    animate: bool,
) -> Result<(), IpcError> {
    let panel = panel.cloned();
    on_main_sync(pinned, move |mtm, _, pns| -> Result<(), IpcError> {
        pns.setFrame_display(to_cocoa(rect, mtm), true);
        pns.setAlphaValue(1.0);
        pns.orderFrontRegardless();
        if let Some(panel) = panel {
            let ns = ns_window(&panel, mtm)?;
            let driver = FrameDriver::for_window(&ns, mtm);
            driver.stop_alpha();
            driver.stop_geometry();
            driver.stop_content();
            ns.orderOut(None);
        }
        if let Some(to) = expand_to {
            let target = to_cocoa(to, mtm);
            if animate {
                FrameDriver::for_window(pns, mtm).animate_rect(pns, target);
            } else {
                pns.setFrame_display(target, true);
            }
        }
        Ok(())
    })?
}

/// 不激活应用地把 pinned 放到最前。`restore_minimized`：托盘「显示固定窗口」为 true（用户要找回它，最小化的也还原）；
/// 截图后恢复为 false（只恢复截图前被隐藏的那些，最小化的保持原样）
pub fn pinned_show(window: &tauri::WebviewWindow, restore_minimized: bool) -> Result<(), IpcError> {
    on_main_sync(window, move |_, _, ns| {
        if ns.isMiniaturized() {
            if !restore_minimized {
                return;
            }
            ns.deminiaturize(None);
        }
        ns.orderFrontRegardless();
    })
}

/// 立即隐藏 pinned（截图前，不播放动画）
pub fn pinned_hide(window: &tauri::WebviewWindow) -> Result<(), IpcError> {
    on_main_sync(window, |_, _, ns| ns.orderOut(None))
}

// ---------------------------------------------------------------------------------------------------------------
// Overlay

/// Overlay 冻结帧视图的 identifier
const FROZEN_VIEW_ID: &str = "wisp.overlay.frozen";

/// 把 Overlay 转为非激活 NSPanel（`WispOverlayPanel`：可以成为 key、不激活应用），层级 101（`NSPopUpMenuWindowLevel`，
/// 高于菜单栏与面板的 25），collectionBehavior `canJoinAllSpaces | fullScreenAuxiliary | stationary | ignoresCycle`
/// （全屏应用的 Space 里也能出现，不参与 ⌘` 循环），无阴影。窗口创建后、显示前调用
pub fn overlay_attach(window: &tauri::WebviewWindow) -> Result<(), IpcError> {
    on_main_sync(window, |mtm, window, _| -> Result<(), IpcError> {
        let panel = window.to_panel::<panel_class::WispOverlayPanel<tauri::Wry>>()?;
        if let Err(e) = panel.add_style_mask(NSWindowStyleMask::NonactivatingPanel) {
            log::warn!("overlay style mask: {e}");
        }
        panel.set_level(NSPopUpMenuWindowLevel as i64);
        panel.set_collection_behavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        panel.set_has_shadow(false);
        // to_panel 换了 isa，重新取一次
        let ns = ns_window(window, mtm)?;
        install_pointer_tracker(&ns, mtm);
        Ok(())
    })?
}

/// 多屏截图时按键焦点跟随指针：非 key 的 WKWebView 收不到指针移动（WebKit 的跟踪区域只在 key
/// 窗口生效），页面无从报告「指针进来了」，所以由原生层负责。每个 Overlay 的 contentView 最底层放一个不画任何东西、
/// 不参与命中测试的视图，带一个 `activeAlways | inVisibleRect` 的 NSTrackingArea：指针一进入这块屏的 Overlay 就让它成为
/// key（不激活应用）。事件驱动，不需要每 50 ms 查一次光标所在屏的轮询（会话期间也零唤醒，换屏的那一刻立即切换）。
/// 单屏时指针不会「进入」唯一的 Overlay，什么都不发生；初始的 key 由 `overlay_show` 按光标所在屏决定
fn install_pointer_tracker(ns: &NSWindow, mtm: MainThreadMarker) {
    let Some(content) = ns.contentView() else { return };
    if find_subview(&content, TRACKER_VIEW_ID).is_some() {
        return;
    }
    let tracker = OverlayPointerTracker::new(mtm, content.bounds());
    tracker.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    tracker.setIdentifier(Some(&objc2_foundation::NSString::from_str(TRACKER_VIEW_ID)));
    let options = NSTrackingAreaOptions::MouseEnteredAndExited
        | NSTrackingAreaOptions::ActiveAlways
        | NSTrackingAreaOptions::InVisibleRect;
    // SAFETY: owner 是跟踪视图自己（跟踪区域不 retain owner；视图持有自己的跟踪区域，owner 一定活得比它久），
    // 没有 userInfo；inVisibleRect 时 rect 被忽略
    let area = unsafe {
        NSTrackingArea::initWithRect_options_owner_userInfo(
            NSTrackingArea::alloc(),
            NSRect::ZERO,
            options,
            Some(&tracker),
            None,
        )
    };
    tracker.addTrackingArea(&area);
    content.addSubview_positioned_relativeTo(&tracker, NSWindowOrderingMode::Below, None);
}

/// 指针跟踪视图的 identifier
const TRACKER_VIEW_ID: &str = "wisp.overlay.tracker";

/// 指针跟踪视图的实例变量
struct TrackerIvars {
    /// 是否还跟随指针：截图会话 commit 之后关掉（[`overlay_stop_following`]），飞入 / 淡出期间按键焦点留在 commit 的那块屏
    following: Cell<bool>,
}

define_class!(
    // SAFETY: NSView 子类化没有额外要求；只覆盖 mouseEntered: 与 hitTest:，实例变量由 objc2 负责释放，不实现 Drop
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "WispOverlayPointerTracker"]
    #[ivars = TrackerIvars]
    struct OverlayPointerTracker;

    impl OverlayPointerTracker {
        /// 指针进入这个 Overlay（另一块屏）：它还不是 key 就成为 key。会话 commit 之后不再跟随（与编排器的光标轮询、页面的
        /// `overlay_focus` 请求同一规则：只在未 commit 的会话里切换）；淡出中的 Overlay（已不接收鼠标）同样不抢
        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            if !self.ivars().following.get() {
                return;
            }
            if let Some(ns) = self.window()
                && ns.isVisible()
                && !ns.isKeyWindow()
                && !ns.ignoresMouseEvents()
            {
                make_overlay_key(&ns);
                log::debug!("overlay: keyboard focus follows the pointer to another display");
            }
        }

        /// 永远不命中：点击、滚轮都落在上面的 WKWebView
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, _point: NSPoint) -> *mut NSView {
            std::ptr::null_mut()
        }
    }
);

impl OverlayPointerTracker {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TrackerIvars {
            following: Cell::new(true),
        });
        // SAFETY: NSView 的指定初始化方法
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// 窗口里的跟踪视图（[`install_pointer_tracker`] 装的那个）
    fn of(ns: &NSWindow) -> Option<Retained<Self>> {
        let content = ns.contentView()?;
        let view = find_subview(&content, TRACKER_VIEW_ID)?;
        view.downcast::<Self>().ok()
    }
}

/// 截图会话 commit 了：这块屏的 Overlay 不再跟随指针成为 key。commit 之后到 `overlay_done` 之间（飞入，
/// 最长到 done 看门狗）指针移到另一块屏时，按键焦点留在 commit 的那块屏上——Esc 等按键不会落到另一块屏的页面。异步
pub fn overlay_stop_following(window: &tauri::WebviewWindow) {
    on_main_async(window, |_, _, ns| {
        if let Some(tracker) = OverlayPointerTracker::of(ns) {
            tracker.ivars().following.set(false);
        }
    });
}

/// 诊断（window lab）：这块屏的 Overlay 是否还跟随指针（没有跟踪视图时为 None）
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn overlay_follows_pointer_now(window: &tauri::WebviewWindow) -> Option<bool> {
    on_main_sync(window, |_, _, ns| {
        OverlayPointerTracker::of(ns).map(|t| t.ivars().following.get())
    })
    .ok()
    .flatten()
}

/// macOS 原生冻结帧：在 contentView 最底层放一个 layer-backed 视图，`layer.contents` 直接用 SCK 的
/// CGImage（零拷贝，首帧即冻结帧），WebView 透明、只画暗化层 / 选区 / 放大镜。帧没有 CGImage（`Frame::from_rgba` 的假帧）
/// 时返回 false，此时 `OverlayInit.frozen_native` 必须为 false（由 WebView 画冻结帧）。
/// 显示之前调用；同一窗口再次调用时原地替换（复用的 Overlay）
pub fn overlay_set_frozen(window: &tauri::WebviewWindow, frame: &wisp_capture::Frame) -> bool {
    let Some(image) = frame.cg_image() else {
        return false;
    };
    // CGImage 线程安全（Send + Sync），连同这一份引用移进主线程回调；主线程超时未执行时引用随回调一起释放
    let installed = on_main_sync(window, move |mtm, _, ns| -> Result<(), IpcError> {
        let content = ns
            .contentView()
            .ok_or_else(|| IpcError::internal("overlay has no content view"))?;
        let view = match find_subview(&content, FROZEN_VIEW_ID) {
            Some(v) => v,
            None => {
                let view = NSView::initWithFrame(NSView::alloc(mtm), content.bounds());
                view.setWantsLayer(true);
                view.setAutoresizingMask(
                    NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
                );
                view.setIdentifier(Some(&objc2_foundation::NSString::from_str(FROZEN_VIEW_ID)));
                content.addSubview_positioned_relativeTo(&view, NSWindowOrderingMode::Below, None);
                view
            }
        };
        let layer = view
            .layer()
            .ok_or_else(|| IpcError::internal("frozen view has no layer"))?;
        let contents = objc2_core_foundation::CFRetained::as_ptr(&image).cast::<AnyObject>();
        // SAFETY: CGImage 与 id 免费桥接，指针在 image 存活期间有效，作为 contents 时由 CALayer 自行 retain。
        // contentsGravity 默认是 resize：物理像素的帧铺满逻辑尺寸的视图，Retina 下逐像素对应
        unsafe { layer.setContents(Some(contents.as_ref())) };
        view.setHidden(false);
        Ok(())
    });
    match installed {
        Ok(Ok(())) => true,
        Ok(Err(e)) | Err(e) => {
            log::warn!("overlay frozen layer: {}", e.message);
            false
        }
    }
}

/// 显示 Overlay（覆盖 `rect` = 显示器完整区域，含菜单栏）；`key` 为 true 时成为 key 接收键盘，不激活应用
pub fn overlay_show(window: &tauri::WebviewWindow, rect: Rect, key: bool) -> Result<(), IpcError> {
    on_main_sync(window, move |mtm, _, ns| {
        // 新会话：重新跟随指针（macOS 不复用 Overlay，这里只是让状态与会话一一对应）
        if let Some(tracker) = OverlayPointerTracker::of(ns) {
            tracker.ivars().following.set(true);
        }
        ns.setFrame_display(to_cocoa(rect, mtm), false);
        ns.setAlphaValue(1.0);
        ns.orderFrontRegardless();
        if key {
            make_overlay_key(ns);
        }
    })
}

/// 多屏截图时让光标所在屏的 Overlay 成为 key：非激活面板 makeKeyWindow 不激活应用
pub fn overlay_focus(window: &tauri::WebviewWindow) {
    on_main_async(window, |_, _, ns| {
        if ns.isVisible() && !ns.isKeyWindow() {
            make_overlay_key(ns);
        }
    });
}

/// Overlay 成为 key，并确保第一响应者是 WKWebView（按键才进得了页面）。非激活 NSPanel：不激活应用、菜单栏不切换
/// （此后 `NSApp.isActive` 在本进程内为 true——AppKit 把按键派给 key 面板——而系统层面的前台应用不变，见
/// [`app_is_active`]）
fn make_overlay_key(ns: &NSWindow) {
    ns.makeKeyWindow();
    let responder_is_window = ns
        .firstResponder()
        .is_none_or(|r| std::ptr::addr_eq(Retained::as_ptr(&r), std::ptr::from_ref(ns)));
    if responder_is_window && let Some(web) = overlay_webview(ns) {
        ns.makeFirstResponder(Some(&web));
    }
}

/// Overlay 里的 WKWebView：contentView（wry 的父视图）下除冻结帧、指针跟踪这两个自己加的视图之外的那个子视图
fn overlay_webview(ns: &NSWindow) -> Option<Retained<NSView>> {
    let content = ns.contentView()?;
    let ours = [FROZEN_VIEW_ID, TRACKER_VIEW_ID].map(objc2_foundation::NSString::from_str);
    content.subviews().iter().find(|v| {
        v.identifier()
            .is_none_or(|id| !ours.iter().any(|o| id.isEqualToString(o)))
    })
}

/// 系统层面 Wisp 是否在前台：菜单栏属于 Wisp，或 Wisp 是前台应用（主线程上读，KVO 属性在这里才是最新的）。
///
/// **不能用 `NSApp.isActive`**（实测会误判）：非激活面板成为 key 之后它在本进程内即为 true——AppKit
/// 据此把按键派给 key 面板——而 `NSRunningApplication.isActive`、`NSWorkspace.frontmostApplication` 与
/// `menuBarOwningApplication` 都仍是原来的应用，菜单栏也没有切换。「不激活应用」关心的正是后者
#[cfg(any(debug_assertions, feature = "fake-llm"))]
fn system_active_on_main() -> bool {
    objc2::rc::autoreleasepool(|_| {
        let me = objc2_app_kit::NSRunningApplication::currentApplication();
        let pid = me.processIdentifier();
        let ws = objc2_app_kit::NSWorkspace::sharedWorkspace();
        me.isActive()
            || ws.menuBarOwningApplication().map(|a| a.processIdentifier()) == Some(pid)
            || ws.frontmostApplication().map(|a| a.processIdentifier()) == Some(pid)
    })
}

/// 诊断（window lab）：系统层面 Wisp 此刻是否在前台（见 [`system_active_on_main`]；任何线程都可以调用，会等主线程）
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn app_is_active(app: &tauri::AppHandle) -> Option<bool> {
    if MainThreadMarker::new().is_some() {
        return Some(system_active_on_main());
    }
    super::post_and_wait(
        |job| app.run_on_main_thread(job),
        system_active_on_main,
        MAIN_THREAD_TIMEOUT,
    )
    .ok()
}

/// 实验台开始时的前台应用（[`lab_remember_frontmost`] 记下，[`lab_resign_active`] 交还给它）
#[cfg(any(debug_assertions, feature = "fake-llm"))]
static LAB_FRONTMOST: std::sync::OnceLock<Option<i32>> = std::sync::OnceLock::new();

/// 诊断（window lab）：实验台开始、还没有显示任何窗口时记下前台应用
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_remember_frontmost() {
    let pid = objc2::rc::autoreleasepool(|_| {
        objc2_app_kit::NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .map(|a| a.processIdentifier())
            .filter(|&pid| pid != objc2_app_kit::NSRunningApplication::currentApplication().processIdentifier())
    });
    let _ = LAB_FRONTMOST.set(pid);
}

/// 诊断（window lab）：把前台交还给实验台开始时的前台应用（macOS 26 上 `deactivate` 不生效）。从终端启动的实验台进程
/// 第一次显示窗口时会被系统激活（启动激活），Overlay 一步之前先交还，才能断言「建 Overlay 前不在前台 → 之后仍不在
/// 前台」
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_resign_active(app: &tauri::AppHandle) {
    let prev = LAB_FRONTMOST.get().copied().flatten();
    let posted = app.run_on_main_thread(move || {
        let Some(mtm) = MainThreadMarker::new() else { return };
        if let Some(prev) = prev.and_then(objc2_app_kit::NSRunningApplication::runningApplicationWithProcessIdentifier)
        {
            #[allow(deprecated)]
            let _ = prev.activateWithOptions(objc2_app_kit::NSApplicationActivationOptions::empty());
        } else {
            NSApplication::sharedApplication(mtm).deactivate();
        }
    });
    if let Err(e) = posted {
        log::warn!("run_on_main_thread failed: {e}");
    }
}

#[cfg(any(debug_assertions, feature = "fake-llm"))]
thread_local! {
    /// window lab 按键探针的本地事件监视器（[`lab_key_probe`]）
    static KEY_PROBE: RefCell<Option<Retained<AnyObject>>> = const { RefCell::new(None) };
}

/// 按键探针记下的按键：(键码, 是否落在 key 的 Overlay 上)
#[cfg(any(debug_assertions, feature = "fake-llm"))]
static KEY_PROBE_LOG: parking_lot::Mutex<Vec<(u16, bool)>> = parking_lot::Mutex::new(Vec::new());

/// 诊断（window lab 的 `overlay_keys`，「非激活的 Overlay 收得到键」）：`on` 时装上（已装则保留）一个只记录、不改事件的本地
/// keyDown 监视器（送到本进程的按键才会经过它），返回到目前为止记下的 (键码, 是否落在 key 的 Overlay 上)；`off` 时卸掉，
/// 返回并清空记录
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_key_probe(app: &tauri::AppHandle, on: bool) -> Vec<(u16, bool)> {
    let run = move || {
        KEY_PROBE.with(|slot| {
            let mut slot = slot.borrow_mut();
            if on {
                if slot.is_none() {
                    KEY_PROBE_LOG.lock().clear();
                    let block = block2::RcBlock::new(|event: std::ptr::NonNull<NSEvent>| -> *mut NSEvent {
                        // SAFETY: AppKit 传入的事件在回调期间有效
                        let e = unsafe { event.as_ref() };
                        let overlay = MainThreadMarker::new()
                            .and_then(|mtm| e.window(mtm))
                            .is_some_and(|w| w.isKeyWindow() && w.class().name().to_string_lossy().contains("Overlay"));
                        KEY_PROBE_LOG.lock().push((e.keyCode(), overlay));
                        event.as_ptr()
                    });
                    // SAFETY: 块返回传入的事件本身（不吞掉、不替换）
                    *slot =
                        unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyDown, &block) };
                }
                KEY_PROBE_LOG.lock().clone()
            } else {
                if let Some(monitor) = slot.take() {
                    // SAFETY: 由 addLocalMonitorForEventsMatchingMask 返回的监视器
                    unsafe { NSEvent::removeMonitor(&monitor) };
                }
                std::mem::take(&mut *KEY_PROBE_LOG.lock())
            }
        })
    };
    if MainThreadMarker::new().is_some() {
        return run();
    }
    super::post_and_wait(|job| app.run_on_main_thread(job), run, MAIN_THREAD_TIMEOUT).unwrap_or_default()
}

/// 诊断（window lab 的 `cold_show_keys`）：最近一张冷唤出外壳 (截住的按键事件数, ready 时重放的事件数)
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_shell_key_stats() -> (usize, usize) {
    *SHELL_KEY_STATS.lock()
}

/// 诊断（window lab 的 `cold_show_keys`）：面板 (系统层面 Wisp 是否在前台, 面板是否 key, 第一响应者是否 WKWebView)
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_panel_focus_state(window: &tauri::WebviewWindow) -> Option<(bool, bool, bool)> {
    on_main_sync(window, |_, _, ns| {
        let web = ns.contentView().and_then(|v| find_webview(&v)).map(|(_, w)| w);
        let responder_is_web = match (ns.firstResponder(), web) {
            (Some(r), Some(w)) => std::ptr::addr_eq(Retained::as_ptr(&r), Retained::as_ptr(&w)),
            _ => false,
        };
        (system_active_on_main(), ns.isKeyWindow(), responder_is_web)
    })
    .ok()
}

/// 诊断（window lab 的 `cold_show_keys`）：在本进程里合成 `text` 的按键（每个字符一对按下 / 抬起，只支持字母与数字），
/// 以面板窗口为目标放进事件队列末尾——与窗口服务器送来的真实按键走同一条派发路径（本地监视器 → key 窗口 → 第一响应者），
/// 不需要辅助功能授权，也绝不会落到别的应用
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_post_text(window: &tauri::WebviewWindow, text: &str) -> Result<usize, IpcError> {
    /// kVK_ANSI_* 键码（美式布局）
    fn key_code(c: char) -> Option<u16> {
        const LETTERS: [u16; 26] = [
            0, 11, 8, 2, 14, 3, 5, 4, 34, 38, 40, 37, 46, 45, 31, 35, 12, 15, 1, 17, 32, 9, 13, 7, 16, 6,
        ];
        const DIGITS: [u16; 10] = [29, 18, 19, 20, 21, 23, 22, 26, 28, 25];
        match c {
            'a'..='z' => LETTERS.get((c as u32 - 'a' as u32) as usize).copied(),
            '0'..='9' => DIGITS.get((c as u32 - '0' as u32) as usize).copied(),
            _ => None,
        }
    }
    let chars: Vec<(char, u16)> = text.chars().filter_map(|c| key_code(c).map(|k| (c, k))).collect();
    on_main_sync(window, move |mtm, _, ns| -> Result<usize, IpcError> {
        let app = NSApplication::sharedApplication(mtm);
        let number = ns.windowNumber();
        let now = objc2_foundation::NSProcessInfo::processInfo().systemUptime();
        for (c, code) in &chars {
            let s = objc2_foundation::NSString::from_str(&c.to_string());
            for ty in [NSEventType::KeyDown, NSEventType::KeyUp] {
                let event = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
                    ty,
                    NSPoint::ZERO,
                    NSEventModifierFlags::empty(),
                    now,
                    number,
                    None,
                    &s,
                    &s,
                    false,
                    *code,
                )
                .ok_or_else(|| IpcError::internal("cannot synthesize a key event"))?;
                app.postEvent_atStart(&event, false);
            }
        }
        Ok(chars.len())
    })?
}

/// 诊断（window lab 的 `cold_show_keys`）：面板 WKWebView 的 `title`（页面的 `document.title`，WebKit 异步同步过来）
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_webview_title(window: &tauri::WebviewWindow) -> Option<String> {
    on_main_sync(window, |_, _, ns| {
        let (_, web) = ns.contentView().and_then(|v| find_webview(&v))?;
        // SAFETY: WKWebView 的 `title` 属性（NSString，可能为 nil）
        let title: Option<Retained<objc2_foundation::NSString>> = unsafe { msg_send![&*web, title] };
        title.map(|t| t.to_string())
    })
    .ok()
    .flatten()
}

/// window lab 的进程 / 内存采样用到的 libSystem 函数（同用户进程不需要 sudo）
#[cfg(any(debug_assertions, feature = "fake-llm"))]
mod lab_sys {
    use std::ffi::{c_int, c_uint, c_void};

    /// `<sys/resource.h>` 的 `rusage_info_v2`
    #[repr(C)]
    #[derive(Default)]
    pub struct RusageInfoV2 {
        pub uuid: [u8; 16],
        pub user_time: u64,
        pub system_time: u64,
        pub pkg_idle_wkups: u64,
        pub interrupt_wkups: u64,
        pub pageins: u64,
        pub wired_size: u64,
        pub resident_size: u64,
        pub phys_footprint: u64,
        pub proc_start_abstime: u64,
        pub proc_exit_abstime: u64,
        pub child_user_time: u64,
        pub child_system_time: u64,
        pub child_pkg_idle_wkups: u64,
        pub child_interrupt_wkups: u64,
        pub child_pageins: u64,
        pub child_elapsed_abstime: u64,
        pub diskio_bytesread: u64,
        pub diskio_byteswritten: u64,
    }

    /// `<malloc/malloc.h>` 的 `malloc_statistics_t`
    #[repr(C)]
    #[derive(Default)]
    pub struct MallocStatistics {
        pub blocks_in_use: c_uint,
        pub size_in_use: usize,
        pub max_size_in_use: usize,
        pub size_allocated: usize,
    }

    pub const RUSAGE_INFO_V2: c_int = 2;
    pub const RUSAGE_INFO_V4: c_int = 4;

    /// `struct rusage_info_v4`：v2 的字段、v3 的 9 个、v4 的 8 个（只读 `ri_lifetime_max_phys_footprint`）
    #[repr(C)]
    #[derive(Default)]
    pub struct RusageInfoV4 {
        pub v2: RusageInfoV2,
        pub v3: [u64; 9],
        pub logical_writes: u64,
        pub lifetime_max_phys_footprint: u64,
        pub rest: [u64; 6],
    }

    pub fn rusage_v4(pid: c_int) -> Option<RusageInfoV4> {
        let mut info = RusageInfoV4::default();
        // SAFETY: 缓冲区是完整的 rusage_info_v4
        let rc = unsafe { proc_pid_rusage(pid, RUSAGE_INFO_V4, (&raw mut info).cast()) };
        (rc == 0).then_some(info)
    }

    unsafe extern "C" {
        pub fn proc_listallpids(buffer: *mut c_void, buffersize: c_int) -> c_int;
        pub fn proc_name(pid: c_int, buffer: *mut c_void, buffersize: u32) -> c_int;
        pub fn proc_pid_rusage(pid: c_int, flavor: c_int, buffer: *mut c_void) -> c_int;
        pub fn responsibility_get_pid_responsible_for_pid(pid: c_int) -> c_int;
        pub fn malloc_zone_statistics(zone: *mut c_void, stats: *mut MallocStatistics);
        pub fn getloadavg(loadavg: *mut f64, nelem: c_int) -> c_int;
    }

    pub fn rusage(pid: c_int) -> Option<RusageInfoV2> {
        let mut info = RusageInfoV2::default();
        // SAFETY: 缓冲区是完整的 rusage_info_v2
        let rc = unsafe { proc_pid_rusage(pid, RUSAGE_INFO_V2, (&raw mut info).cast()) };
        (rc == 0).then_some(info)
    }

    pub fn name(pid: c_int) -> Option<String> {
        let mut buf = [0u8; 256];
        // SAFETY: 缓冲区长度如实传入
        let n = unsafe { proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
        let n = usize::try_from(n).ok().filter(|&n| n > 0)?;
        Some(String::from_utf8_lossy(&buf[..n.min(buf.len())]).into_owned())
    }
}

/// 诊断（window lab）：属于本进程的 WebKit 辅助进程（WebContent / GPU / Networking …）与各自的 phys_footprint。
///
/// 归属（精确匹配，不依赖启动方式）：
/// - 本进程每个 WKWebView 报告的三个辅助进程（WebKit 的测试 SPI `_webProcessIdentifier` / `_gpuProcessIdentifier` /
///   `_networkProcessIdentifier`，先 `respondsToSelector` 确认），在主线程上读；
/// - 以前认领过、仍然活着的（pid 与启动时刻都对得上）：GPU / Networking 进程比最后一个 WebView 活得久（约 1 分钟），
///   WebView 全部销毁之后仍按记录认领；
/// - 「负责进程」是本进程的（经 LaunchServices 启动时 WebKit 的 XPC 进程都以本进程为负责进程，与
///   `scripts/perf/macos-webkit.py` 同一方法）。
///
/// 于是从终端直接启动（负责进程是终端——测量 Overlay 时要靠它继承屏幕录制权限）也能精确归属，同一终端里别的 WebKit
/// 应用不会被算进来。只在阻塞线程上调用（要等主线程）
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_webkit_processes(app: &tauri::AppHandle) -> Vec<super::LabProcess> {
    const PREFIX: &str = "com.apple.WebKit.";
    let me = i32::try_from(std::process::id()).unwrap_or(0);
    let windows: Vec<tauri::WebviewWindow> = app.webview_windows().into_values().collect();
    let from_views = super::post_and_wait(
        |job| app.run_on_main_thread(job),
        move || {
            let Some(mtm) = MainThreadMarker::new() else {
                return Vec::new();
            };
            let mut pids: Vec<i32> = Vec::new();
            for w in &windows {
                let Some(web) = ns_window(w, mtm)
                    .ok()
                    .and_then(|ns| ns.contentView())
                    .and_then(|v| find_webview(&v))
                    .map(|(_, web)| web)
                else {
                    continue;
                };
                // SAFETY: WKWebView 的这三个测试 SPI 都是无参数、返回 pid_t 的实例方法；每个都先用 respondsToSelector 确认存在
                unsafe {
                    if web.respondsToSelector(sel!(_webProcessIdentifier)) {
                        pids.push(msg_send![&*web, _webProcessIdentifier]);
                    }
                    if web.respondsToSelector(sel!(_gpuProcessIdentifier)) {
                        pids.push(msg_send![&*web, _gpuProcessIdentifier]);
                    }
                    if web.respondsToSelector(sel!(_networkProcessIdentifier)) {
                        pids.push(msg_send![&*web, _networkProcessIdentifier]);
                    }
                }
            }
            pids.retain(|&pid| pid > 0);
            pids
        },
        MAIN_THREAD_TIMEOUT,
    )
    .unwrap_or_default();
    // SAFETY: 纯查询
    let owner = unsafe { lab_sys::responsibility_get_pid_responsible_for_pid(me) };
    let mut seen = LAB_WEBKIT_SEEN.lock();
    // 先按 pid 认领 WebView 报告的进程（记下启动时刻，防 pid 复用）
    for pid in from_views {
        if let Some(start) = lab_sys::rusage(pid).map(|r| r.proc_start_abstime)
            && !seen.contains(&(pid, start))
        {
            seen.push((pid, start));
        }
    }
    // SAFETY: 先取数量再取列表；多给 64 个余量
    let count = unsafe { lab_sys::proc_listallpids(std::ptr::null_mut(), 0) }.max(0) as usize + 64;
    let mut pids = vec![0i32; count];
    let bytes = i32::try_from(pids.len() * std::mem::size_of::<i32>()).unwrap_or(i32::MAX);
    // SAFETY: 缓冲区长度如实传入
    let n = unsafe { lab_sys::proc_listallpids(pids.as_mut_ptr().cast(), bytes) }.max(0) as usize;
    pids.truncate(n.min(pids.len()));
    let mut out = Vec::new();
    for pid in pids.into_iter().filter(|&p| p > 0 && p != me) {
        let Some(name) = lab_sys::name(pid) else { continue };
        let Some(kind) = name.strip_prefix(PREFIX) else {
            continue;
        };
        let Some(usage) = lab_sys::rusage(pid) else { continue };
        // SAFETY: 纯查询
        let responsible = unsafe { lab_sys::responsibility_get_pid_responsible_for_pid(pid) };
        let ours = seen.contains(&(pid, usage.proc_start_abstime)) || (owner == me && responsible == me);
        if ours {
            if !seen.contains(&(pid, usage.proc_start_abstime)) {
                seen.push((pid, usage.proc_start_abstime));
            }
            out.push(super::LabProcess {
                pid,
                kind: kind.to_owned(),
                footprint_kib: usage.phys_footprint / 1024,
            });
        }
    }
    // 已经退出的不再记着
    seen.retain(|(pid, start)| {
        out.iter().any(|p| p.pid == *pid) || lab_sys::rusage(*pid).is_some_and(|r| r.proc_start_abstime == *start)
    });
    out.sort_by_key(|p| p.pid);
    out
}

/// 已经认领过的 WebKit 辅助进程（pid, 启动时刻），见 [`lab_webkit_processes`]
#[cfg(any(debug_assertions, feature = "fake-llm"))]
static LAB_WEBKIT_SEEN: parking_lot::Mutex<Vec<(i32, u64)>> = parking_lot::Mutex::new(Vec::new());

/// 诊断（window lab）：用系统的 `footprint` 工具（同用户进程不需要 sudo）取一个进程的内存构成，返回按
/// 脏内存从大到小的前 `top` 类（类别名, KiB），例如 GPU 进程的 `IOAccelerator (graphics)` / `IOSurface` / `MALLOC_SMALL`。
/// 取不到时返回空表
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_footprint_categories(pid: i32, top: usize) -> Vec<(String, u64)> {
    let Ok(out) = std::process::Command::new("/usr/bin/footprint")
        .arg("-p")
        .arg(pid.to_string())
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    /// 「15 MB」「3520 KB」「0 B」→ KiB
    fn kib(v: &str, unit: &str) -> Option<u64> {
        let v: f64 = v.parse().ok()?;
        let k = match unit {
            "B" => v / 1024.0,
            "KB" => v,
            "MB" => v * 1024.0,
            "GB" => v * 1024.0 * 1024.0,
            _ => return None,
        };
        Some(k.round() as u64)
    }
    let mut rows: Vec<(String, u64)> = text
        .lines()
        .filter_map(|line| {
            // 「  Dirty  Clean  Reclaimable  Regions  Category」：前三列各是「数值 单位」
            let t: Vec<&str> = line.split_whitespace().collect();
            if t.len() < 8 {
                return None;
            }
            let dirty = kib(t[0], t[1])?;
            let _clean = kib(t[2], t[3])?;
            let _reclaimable = kib(t[4], t[5])?;
            t[6].parse::<u64>().ok()?;
            let category = t[7..].join(" ");
            (category != "TOTAL").then_some((category, dirty))
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.1));
    rows.truncate(top);
    rows
}

/// 诊断（window lab 的 `recycle_stress`）：本进程 malloc 各 zone 正在使用的字节数（`malloc_zone_statistics(NULL)`，与
/// `heap` / `vmmap --summary` 的 malloc 口径一致；比 phys_footprint 稳定得多，看得出每次几 KB 的泄漏）
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_malloc_in_use() -> Option<u64> {
    let mut stats = lab_sys::MallocStatistics::default();
    // SAFETY: zone 为 NULL 表示全部 zone；stats 是完整的结构体
    unsafe { lab_sys::malloc_zone_statistics(std::ptr::null_mut(), &raw mut stats) };
    u64::try_from(stats.size_in_use).ok()
}

/// 诊断（window lab）：本进程 phys_footprint 的生命期最大值（KiB，`rusage_info_v4`）——一次 OCR 前后各读一次，
/// 之后比之前大说明 OCR 那一段创了新高（采样之间的瞬时峰值也算在内）
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_lifetime_max_footprint_kib() -> Option<u64> {
    let me = i32::try_from(std::process::id()).ok()?;
    lab_sys::rusage_v4(me).map(|r| r.lifetime_max_phys_footprint / 1024)
}

/// 诊断（window lab 的 `WISP_WINDOW_LAB_DOCK_ICON`）：(bundle 图标已由 [`apply_bundle_icon`] 设上、且
/// `NSApp.applicationIconImage` 是一张完整尺寸的图, 说明：激活策略、设置次数、当前图标与 Finder 图标最大表示的像素、
/// bundle 路径)。AppKit 设置时会重新绘制一份（读回来的不是同一个对象，TIFF 数据也不同），内容要看 `dump`：给出时把当前
/// 图标的 TIFF 写到那里，人工查看
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_app_icon(app: &tauri::AppHandle, dump: Option<std::path::PathBuf>) -> Option<(bool, String)> {
    let run = move || {
        let mtm = MainThreadMarker::new()?;
        let ns_app = NSApplication::sharedApplication(mtm);
        let current = ns_app.applicationIconImage();
        if let (Some(path), Some(data)) = (dump, current.as_ref().and_then(|img| img.TIFFRepresentation()))
            && !data.writeToFile_atomically(&objc2_foundation::NSString::from_str(&path.to_string_lossy()), true)
        {
            log::warn!("dock icon: could not write {}", path.display());
        }
        let sets = BUNDLE_ICON_SETS.load(Ordering::Relaxed);
        let px = current.as_deref().map_or(0, max_rep_px);
        let path = objc2_foundation::NSBundle::mainBundle().bundlePath();
        let finder = objc2_app_kit::NSWorkspace::sharedWorkspace().iconForFile(&path);
        Some((
            sets > 0 && px >= 256,
            format!(
                "policy={} bundle_icon_sets={sets} icon_max_px={px} finder_icon_max_px={} bundle={path}",
                ns_app.activationPolicy().0,
                max_rep_px(&finder),
            ),
        ))
    };
    super::post_and_wait(|job| app.run_on_main_thread(job), run, MAIN_THREAD_TIMEOUT)
        .ok()
        .flatten()
}

/// 诊断（window lab）：1 分钟平均负载（测量时记录机器是否安静）
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_load_average() -> Option<f64> {
    let mut load = [0f64; 3];
    // SAFETY: 缓冲区长度如实传入
    let n = unsafe { lab_sys::getloadavg(load.as_mut_ptr(), 3) };
    (n > 0).then_some(load[0])
}

/// 诊断（window lab）：(系统层面 Wisp 是否在前台, Overlay 是否 key, 第一响应者是否 WKWebView)
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn overlay_focus_state(window: &tauri::WebviewWindow) -> Option<(bool, bool, bool)> {
    on_main_sync(window, |_, _, ns| {
        let active = system_active_on_main();
        let web = overlay_webview(ns);
        let responder_is_web = match (ns.firstResponder(), web) {
            (Some(r), Some(w)) => std::ptr::addr_eq(Retained::as_ptr(&r), Retained::as_ptr(&w)),
            _ => false,
        };
        (active, ns.isKeyWindow(), responder_is_web)
    })
    .ok()
}

/// Overlay 的层级（101）本来就高于面板（25），不需要重新压到最上面
pub fn overlay_raise(window: &tauri::WebviewWindow) {
    let _ = window;
}

/// 隐藏 Overlay（复用的预建窗口），同时清掉冻结帧（不保留截图像素）
pub fn overlay_hide(window: &tauri::WebviewWindow) {
    on_main_async(window, |_, _, ns| {
        ns.orderOut(None);
        if let Some(content) = ns.contentView()
            && let Some(view) = find_subview(&content, FROZEN_VIEW_ID)
            && let Some(layer) = view.layer()
        {
            // SAFETY: 清空 contents，释放 CGImage
            unsafe { layer.setContents(None) };
        }
    });
}

/// 销毁 Overlay（避免 nspanel 的 Store 残留引用），见 [`destroy_window`]
pub fn overlay_destroy(window: &tauri::WebviewWindow) {
    destroy_window(window);
}

/// commit 之后的正常收尾（`overlay_done`）：先把窗口 alpha 淡到 0 再销毁。冻结帧是原生
/// CALayer，页面只能淡掉暗化层与选区；直接销毁时冻结帧一帧硬切成真实屏幕（Overlay 下面已经显示的面板随之「蹦」出来）。
/// 淡出期间窗口不接收鼠标（点击落到下面的应用）；立即登记为「正在销毁」，期间新建同名 Overlay、截图前隐藏都会等它离开
/// （[`super::await_destroyed`]，最多 300 ms）。displayLink 不出帧（显示器睡眠）时淡出回调不会来：到时间仍兜底销毁，只销毁一次
pub fn overlay_destroy_fading(window: &tauri::WebviewWindow, fade: Fade) {
    super::note_destroying(window.label());
    let done = std::sync::Arc::new(AtomicBool::new(false));
    let finish = {
        let w = window.clone();
        let done = std::sync::Arc::clone(&done);
        move || {
            if !done.swap(true, Ordering::SeqCst) {
                destroy_window(&w);
            }
        }
    };
    let fallback = finish.clone();
    on_main_async(window, move |mtm, _, ns| {
        if !ns.isVisible() {
            finish();
            return;
        }
        ns.setIgnoresMouseEvents(true);
        let driver = FrameDriver::for_window(ns, mtm);
        driver.fade(0.0, fade, Some(Box::new(finish)));
    });
    let wait = Duration::from_secs_f64(fade.duration.max(0.0)) + Duration::from_millis(250);
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(wait).await;
        fallback();
    });
}

/// 销毁面板窗口（空闲卸载、失效重建），见 [`destroy_window`]
pub fn panel_destroy(window: &tauri::WebviewWindow) {
    // 外壳上屏后页面迟迟不加载（按失效重建）：输入守卫随窗口一起卸掉
    let posted = window.run_on_main_thread(|| {
        shell_guard_finish(None);
    });
    if let Err(e) = posted {
        log::warn!("run_on_main_thread failed: {e}");
    }
    destroy_window(window);
}

/// [`destroy_window`] 摘下 WKWebView 之后多久再 `destroy()`：一帧多，足够 WebKit 的 RunLoop 源先跑完；新建同名窗口
/// （等 [`super::await_destroyed`]）最多因此多等这么久
const DESTROY_SETTLE: Duration = Duration::from_millis(20);

/// 销毁一个窗口（面板 / Overlay / 用户关闭的 Pinned、window lab 与冒烟里的设置窗口），在主线程上：
/// - 先释放它的逐帧驱动（invalidate displayLink），再离屏——否则 displayLink 留在 run loop 里，连同驱动一起泄漏；
/// - 经 tauri-nspanel 转换过的窗口先经 `to_window` 变回普通窗口——从 Store 移除（`remove_webview_panel`）、恢复原来的类、
///   `releasedWhenClosed = false`（窗口由 tao 持有并在关闭时释放，AppKit 不得再释放一次，否则会抛 Objective-C 异常、进程
///   直接中止）；
/// - 然后 `destroy()`。它**不会**当场销毁：tauri-runtime-wry 总是经事件循环代理投递 `WindowMessage::Destroy`（在主线程上
///   同步处理会 panic），下一轮事件循环才丢掉 tao 的窗口；tao 再经 `close_async` 异步 `close`，`windowWillClose` 里排队的
///   `Destroyed` 还要再晚一轮才送到 Tauri（此时窗口才从窗口表里消失）。上面这些在 autorelease pool 里做，拆除时 autorelease
///   的对象当场释放，不堆进事件循环外层那个长期不排空的 pool。
///
/// 销毁之前先 [`super::note_destroying`]：`Destroyed` 送到之前新建的窗口可能被它误伤（见那里）
///
/// **WKWebView 先摘下来、隔一小段时间（[`DESTROY_SETTLE`]）再 `destroy()`**（实测到的崩溃：Overlay 上屏后
/// 很快取消、连续几百次里约一次 SIGSEGV 在 `WebKit::WebPageProxy::dispatchSetObscuredContentInsets`）。
/// wry 的 `Drop` 先 `removeFromSuperview`——
/// WKWebView 的 `renewGState` 借 `RunLoop::dispatch` 排一次「更新被遮挡的内容边距」——紧接着在同一次回调里释放 WKWebView、
/// `WebPageProxy::close()`；排着的那次之后在已关闭的页面上执行，页面对象偶尔还活着（别处仍有引用）而内部已经拆掉，访问
/// 空指针。这里离屏之后立即自己把 WKWebView 从父视图摘下（那次更新此刻排上），`destroy()` 推迟一帧多再投递：WebKit 的
/// RunLoop 源在下一轮执行，页面还在；wry 之后的 `removeFromSuperview` 没有父视图、什么也不排。窗口已经离屏，摘视图不可见
pub fn destroy_window(window: &tauri::WebviewWindow) {
    super::note_destroying(window.label());
    let w = window.clone();
    let posted = window.run_on_main_thread(move || {
        let target = objc2::rc::autoreleasepool(|_| {
            let mtm = MainThreadMarker::new();
            if let Some(mtm) = mtm
                && let Ok(ns) = ns_window(&w, mtm)
            {
                // 先释放逐帧驱动（invalidate displayLink）：否则 displayLink 留在 run loop 里，连同驱动与窗口一起泄漏
                FrameDriver::forget(&ns);
                ns.orderOut(None);
            }
            let app = w.app_handle().clone();
            let restored = app.get_webview_panel(w.label()).ok().and_then(|p| p.to_window());
            let target = restored.unwrap_or(w);
            // 摘 WKWebView 必须在恢复成普通窗口之后：它离开窗口时 WebKit 注销自己在窗口上的 KVO，而 NSPanel 子类化之后
            // 窗口的类已换过，在那时注销会抛 NSRangeException（「没有注册过这个观察者」）
            if let Some(mtm) = mtm
                && let Ok(ns) = ns_window(&target, mtm)
                && let Some((_, web)) = ns.contentView().and_then(|v| find_webview(&v))
            {
                web.removeFromSuperview();
            }
            target
        });
        // 隔一小段时间再投递：Tauri 的 `run_on_main_thread` 与 `WindowMessage::Destroy` 都在 tao 的事件循环观察者里排空，
        // 同一次回调里就可能接连执行完，早于 WebKit 下一轮的 RunLoop 源；按时间推迟才能保证它先跑
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(DESTROY_SETTLE).await;
            let posted = target.clone().run_on_main_thread(move || {
                objc2::rc::autoreleasepool(|_| {
                    if let Err(e) = target.destroy() {
                        log::warn!("destroy {} failed: {e}", target.label());
                    }
                });
            });
            if let Err(e) = posted {
                log::warn!("run_on_main_thread failed: {e}");
            }
        });
    });
    if let Err(e) = posted {
        log::warn!("run_on_main_thread failed: {e}");
    }
}

/// 接管 tao 0.37.1 创建 NSWindow 时多出、且永远不会被释放的那一次引用——销毁的窗口一直留在 AppKit 与窗口服务器里的根因
/// （不处理的话，反复开关窗口之后 CGWindowList 里本进程的窗口数会从 2 涨到 7）。
///
/// `tao::platform_impl::macos::window::create_window` 先 `alloc` + `initWithContentRect:…`（返回 +1 的裸指针），再用
/// `Retained::retain(ns_window_ptr)` 包装——又 +1，而原来那 +1 没有任何人接管；随后 `setReleasedWhenClosed(false)`，
/// `close` 也不会替它释放。tao / Tauri 销毁时放掉自己的全部引用之后，NSWindow 仍多一次引用：窗口对象、它的视图树
/// （TaoView、WryWebViewParent……）与窗口服务器里的窗口（backing store）永远留着，每打开一次 Pinned / Overlay / 设置窗口、
/// 每卸载一次面板就多一个。这一次引用没有对应的指针，`leaks --traceTree` 找不到持有者；在 NSWindow 上跟踪
/// retain / release / retainWeakReference 的调用栈可见：空闲窗口的引用基线比 tao 与委托的两份 `Retained` 多 1，销毁后恰好
/// 剩这 1；纯 AppKit 窗口销毁即释放，经 Tauri 创建的窗口无论 `destroy` / `close`、在哪个线程、有无页面都泄漏。
///
/// 每个窗口在 `build()` 成功后**立即调用、且只调用一次**：此后窗口的生命周期完全由 tao / Tauri 的引用决定，销毁时当场释放
/// （`NSZombieEnabled=YES` 下验证没有过度释放）。上游修复之后必须删除这里的释放，否则会过度释放而崩溃——单测
/// `tao_create_window_leak_is_compensated_for_the_locked_version` 在 tao 版本变化时失败，提醒重新核对。
///
/// 异步投递到主线程、不等结果（没有人需要结果；它在 Overlay 创建这样的热路径上）。闭包执行时：
/// - 窗口还在 Tauri 的窗口表里 → tao 的 `UnownedWindow` 与委托各持有一份 `Retained`，释放多出的这一次不会让它当场释放；
/// - 窗口已经销毁（tao 已丢掉窗口，`ns_window()` 取不到）→ 什么都不释放，这个窗口照旧泄漏，但不会过度释放；
/// - 主线程串行执行，闭包运行期间窗口不会被并发销毁；按 Tauri 的窗口 id 而不是地址找窗口，不会找错
pub fn adopt_window(window: &tauri::WebviewWindow) {
    on_main_async(window, |_, _, ns| {
        objc2::rc::autoreleasepool(|_| {
            let ptr = std::ptr::from_ref(ns).cast_mut();
            // SAFETY: 接管并释放 tao 遗留的 alloc/init 那一次引用（见上）。`ns` 由 on_main_async 另外 retain 了一份，
            // 窗口仍在 Tauri 的窗口表里（否则 ns_window 取不到、走不到这里），tao 与委托的两份 `Retained` 还在，
            // 这里的 release 不会让它释放
            drop(unsafe { Retained::<NSWindow>::from_raw(ptr) });
        });
    });
}

// ---------------------------------------------------------------------------------------------------------------
// 原生卡片表面

/// NSVisualEffectView 与着色层的 identifier（重复安装时据此找到旧视图原地更新）
const MATERIAL_VIEW_ID: &str = "wisp.card.material";
const FILL_VIEW_ID: &str = "wisp.card.fill";

/// 12 pt 圆角的可拉伸遮罩图：capInsets = 圆角半径，中间 1 pt 拉伸，任意尺寸下圆角不变形
fn rounded_mask(radius: f64) -> Retained<NSImage> {
    let edge = 2.0 * radius + 1.0;
    let block = block2::RcBlock::new(move |rect: NSRect| -> Bool {
        NSColor::blackColor().set();
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, radius, radius).fill();
        Bool::YES
    });
    let image = NSImage::imageWithSize_flipped_drawingHandler(NSSize::new(edge, edge), false, &block);
    image.setCapInsets(NSEdgeInsets {
        top: radius,
        left: radius,
        bottom: radius,
        right: radius,
    });
    image.setResizingMode(NSImageResizingMode::Stretch);
    image
}

fn cg_color(c: [f64; 4]) -> objc2_core_foundation::CFRetained<CGColor> {
    CGColor::new_srgb(c[0], c[1], c[2], c[3])
}

fn find_subview(parent: &NSView, id: &str) -> Option<Retained<NSView>> {
    parent
        .subviews()
        .iter()
        .find(|v| v.identifier().is_some_and(|i| i.to_string() == id))
}

/// 在 contentView 底部装上（或原地更新）材质视图与着色层（见模块注释）
fn install_card_surface(ns: &NSWindow, surface: CardSurface, mtm: MainThreadMarker) -> Result<(), IpcError> {
    let content = ns
        .contentView()
        .ok_or_else(|| IpcError::internal("panel has no content view"))?;
    let bounds = content.bounds();
    let resize = NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable;

    let material = match find_subview(&content, MATERIAL_VIEW_ID) {
        Some(v) => v,
        None => {
            let effect = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), bounds);
            effect.setMaterial(NSVisualEffectMaterial::Popover);
            effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
            effect.setState(NSVisualEffectState::Active);
            effect.setMaskImage(Some(&rounded_mask(PANEL_RADIUS)));
            effect.setAutoresizingMask(resize);
            effect.setIdentifier(Some(&objc2_foundation::NSString::from_str(MATERIAL_VIEW_ID)));
            content.addSubview_positioned_relativeTo(&effect, NSWindowOrderingMode::Below, None);
            Retained::into_super(effect)
        }
    };
    material.setHidden(!surface.material);

    let fill = match find_subview(&content, FILL_VIEW_ID) {
        Some(v) => v,
        None => {
            let view = NSView::initWithFrame(NSView::alloc(mtm), bounds);
            view.setWantsLayer(true);
            view.setAutoresizingMask(resize);
            view.setIdentifier(Some(&objc2_foundation::NSString::from_str(FILL_VIEW_ID)));
            content.addSubview_positioned_relativeTo(&view, NSWindowOrderingMode::Above, Some(&material));
            view
        }
    };
    if let Some(layer) = fill.layer() {
        layer.setCornerRadius(PANEL_RADIUS);
        layer.setBackgroundColor(Some(&cg_color(surface.fill)));
        layer.setBorderWidth(0.5);
        layer.setBorderColor(Some(&cg_color(surface.hairline)));
    }
    ns.setHasShadow(true);
    ns.invalidateShadow();
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// 逐帧驱动

static STATS_ENABLED: AtomicBool = AtomicBool::new(false);
/// 正在跑的 displayLink 数（所有窗口；只在动画期间存在）。[`animating`] 读它，不需要主线程往返
static LIVE_LINKS: AtomicUsize = AtomicUsize::new(0);

/// 停掉一个 displayLink（它持有 target，必须 invalidate）并更新计数
fn retire_link(link: &CADisplayLink) {
    link.invalidate();
    LIVE_LINKS.fetch_sub(1, Ordering::Relaxed);
}

/// 有没有 Wisp 窗口正在跑原生逐帧动画（displayLink：面板 / pinned 的几何、淡入淡出、展开过渡）。面板回收据此推迟
/// （见 `WindowManager::others_busy`）；任意线程可调用
pub fn animating() -> bool {
    LIVE_LINKS.load(Ordering::Relaxed) > 0
}

/// 每段动画的逐帧统计（frame lab）
#[derive(Default)]
struct FrameStats {
    started: Option<Instant>,
    /// 相邻两帧 targetTimestamp 的间隔（ms）
    intervals: Vec<f32>,
    /// 名义帧间隔（ms，displayLink.duration）
    nominal: f32,
    /// `setFrame:display:` 耗时（µs）
    set_frame: Vec<f32>,
    /// `invalidateShadow` 耗时（µs）
    shadow: Vec<f32>,
    /// 帧回调总耗时（µs）
    callback: Vec<f32>,
}

impl FrameStats {
    fn summary(&mut self, what: &str) {
        if self.intervals.is_empty() && self.set_frame.is_empty() {
            return;
        }
        let pct = |v: &mut Vec<f32>, p: f32| -> f32 {
            if v.is_empty() {
                return 0.0;
            }
            v.sort_by(f32::total_cmp);
            let i = ((v.len() - 1) as f32 * p).round() as usize;
            v[i.min(v.len() - 1)]
        };
        let mean = |v: &[f32]| {
            if v.is_empty() {
                0.0
            } else {
                v.iter().sum::<f32>() / v.len() as f32
            }
        };
        let late = self
            .intervals
            .iter()
            .filter(|&&ms| self.nominal > 0.0 && ms > self.nominal * 1.5)
            .count();
        let wall = self.started.map_or(0.0, |t| t.elapsed().as_secs_f32() * 1000.0);
        let busy_us: f32 = self.callback.iter().sum();
        log::info!(
            "frame_stats kind={what} frames={} wall_ms={wall:.0} nominal_ms={:.2} interval_ms(p50/p95/max)={:.2}/{:.2}/{:.2} \
             late_frames={late} set_frame_us(mean/p95/max)={:.0}/{:.0}/{:.0} shadow_us(mean/p95/max)={:.0}/{:.0}/{:.0} \
             callback_us(mean/p95)={:.0}/{:.0} main_thread_busy={:.2}%",
            self.intervals.len(),
            self.nominal,
            pct(&mut self.intervals, 0.5),
            pct(&mut self.intervals, 0.95),
            pct(&mut self.intervals, 1.0),
            mean(&self.set_frame),
            pct(&mut self.set_frame, 0.95),
            pct(&mut self.set_frame, 1.0),
            mean(&self.shadow),
            pct(&mut self.shadow, 0.95),
            pct(&mut self.shadow, 1.0),
            mean(&self.callback),
            pct(&mut self.callback, 0.95),
            if wall > 0.0 { busy_us / 10.0 / wall } else { 0.0 },
        );
        *self = Self::default();
    }
}

/// window lab 的逐帧轨迹（只在开着逐帧统计时记录；分配器开关前后的延迟对比与 Native 菜单预留的 snappy 测量用）
#[cfg(any(debug_assertions, feature = "fake-llm"))]
mod lab_trace {
    use parking_lot::Mutex;

    use super::super::{LabFrameTrace, LabHeightTrace};

    static FRAMES: Mutex<Option<LabFrameTrace>> = Mutex::new(None);
    static HEIGHTS: Mutex<Vec<LabHeightTrace>> = Mutex::new(Vec::new());
    /// 留住的轨迹上限（实验台每步都会取走；忘了取时不无限增长）
    const MAX_TRACES: usize = 256;
    const MAX_FRAMES: usize = 100_000;

    /// 一帧：与上一帧的间隔，以及这一帧里在进行的动画
    pub fn frame(dt_ms: f32, nominal_ms: f32, fade: bool, height: bool) {
        let mut g = FRAMES.lock();
        let t = g.get_or_insert_with(LabFrameTrace::default);
        t.nominal_ms = nominal_ms;
        if fade && t.fade_ms.len() < MAX_FRAMES {
            t.fade_ms.push(dt_ms);
        }
        if height && t.height_ms.len() < MAX_FRAMES {
            t.height_ms.push(dt_ms);
        }
    }

    /// 一帧回调的耗时（有淡入淡出或高度弹簧的帧）
    pub fn callback(us: f32) {
        let mut g = FRAMES.lock();
        let t = g.get_or_insert_with(LabFrameTrace::default);
        if t.callback_us.len() < MAX_FRAMES {
            t.callback_us.push(us);
        }
    }

    pub fn height(trace: LabHeightTrace) {
        let mut g = HEIGHTS.lock();
        if g.len() < MAX_TRACES {
            g.push(trace);
        }
    }

    pub fn take_frames() -> LabFrameTrace {
        FRAMES.lock().take().unwrap_or_default()
    }

    pub fn take_heights() -> Vec<LabHeightTrace> {
        std::mem::take(&mut *HEIGHTS.lock())
    }

    /// 弹簧参数 → 名字（只认两档）
    pub fn spring_name(p: crate::windows::sizing::SpringParams) -> &'static str {
        if p == crate::windows::sizing::SpringParams::SNAPPY {
            "snappy"
        } else {
            "gentle"
        }
    }

    /// 现在的 Unix 时间（ms）
    pub fn unix_ms() -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
    }
}

/// window lab：取走累积的帧间隔（见 [`super::LabFrameTrace`]；要先 [`set_frame_stats`]）
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_take_frame_trace() -> super::LabFrameTrace {
    lab_trace::take_frames()
}

/// window lab：取走记录的高度弹簧轨迹（见 [`super::LabHeightTrace`]；要先 [`set_frame_stats`]）
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub fn lab_take_height_traces() -> Vec<super::LabHeightTrace> {
    lab_trace::take_heights()
}

/// 高度弹簧：`edge` 是固定边（每一帧都从窗口当前的位置取，用户拖动窗口时不被拉回旧位置）
struct HeightMotion {
    spring: Spring,
    edge: Edge,
    /// 拖动过的面板的竖直边界（Cocoa 坐标的原点 y 下限、顶边上限，见 [`VerticalBound`]）
    bound: Option<(f64, f64)>,
    /// 最近一次真正写进窗口的高度（已按物理像素取整）
    applied: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edge {
    /// 顶边固定（Below / Center）：y = top − h
    Top,
    /// 底边固定（Above）：y 不变
    Bottom,
}

impl Edge {
    fn of(placement: Placement) -> Self {
        match placement {
            Placement::Above => Self::Bottom,
            Placement::Below | Placement::Center => Self::Top,
        }
    }
}

/// 矩形弹簧（Pinned 展开）：x、顶边（Cocoa 坐标的 y + h）、宽、高各一条 spring.gentle
struct RectMotion {
    x: Spring,
    top: Spring,
    width: Spring,
    height: Spring,
    applied: NSRect,
}

/// 排好的宽度切换（原地替换，见 [`WidthSwitch`]）：`x` 与 Cocoa 坐标相同，`at` 是 CACurrentMediaTime 时钟（秒）
struct PendingWidth {
    x: f64,
    width: f64,
    at: f64,
    pin: Option<f64>,
}

/// 窗口 alpha 补间
struct AlphaMotion {
    from: f64,
    to: f64,
    /// 开始时刻（CACurrentMediaTime 时钟，秒）
    start: f64,
    duration: f64,
    curve: CubicBezier,
    /// 补间结束后在主线程执行（退出：orderOut + 通知）；被打断时直接丢弃
    then: Option<Box<dyn FnOnce()>>,
}

/// WKWebView 内容的 alpha 补间（冷唤出外壳上的内容淡入）
struct ContentMotion {
    view: Weak<NSView>,
    from: f64,
    start: f64,
    duration: f64,
    curve: CubicBezier,
}

#[derive(Default)]
struct Motion {
    link: Option<Retained<CADisplayLink>>,
    last_ts: Option<f64>,
    height: Option<HeightMotion>,
    rect: Option<RectMotion>,
    width: Option<PendingWidth>,
    alpha: Option<AlphaMotion>,
    content: Option<ContentMotion>,
    stats: FrameStats,
    /// window lab：进行中的高度弹簧的轨迹（运动开始的 CACurrentMediaTime 秒，轨迹）
    #[cfg(any(debug_assertions, feature = "fake-llm"))]
    lab_height: Option<(f64, super::LabHeightTrace)>,
}

impl Motion {
    /// 没有任何进行中的动画（displayLink 可以停）
    fn idle(&self) -> bool {
        self.height.is_none()
            && self.rect.is_none()
            && self.width.is_none()
            && self.alpha.is_none()
            && self.content.is_none()
    }
}

/// FrameDriver 的实例变量
struct DriverIvars {
    window: Weak<NSWindow>,
    motion: RefCell<Motion>,
    /// 回调重入保护（`then` 回调里可能再次操作同一个窗口）
    ticking: Cell<bool>,
}

define_class!(
    // SAFETY: NSObject 没有子类化要求；FrameDriver 不实现 Drop（实例变量由 objc2 负责释放）
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "WispPanelFrameDriver"]
    #[ivars = DriverIvars]
    struct FrameDriver;

    impl FrameDriver {
        /// displayLink 回调（每个显示帧一次，只在有动画时存在）
        #[unsafe(method(onFrame:))]
        fn on_frame(&self, link: &CADisplayLink) {
            self.tick(link);
        }
    }
);

thread_local! {
    /// 每个窗口一个驱动（键为 NSWindow 地址）。窗口销毁时由 [`FrameDriver::forget`] / [`release_offscreen_drivers`]
    /// 移除；万一漏掉，下次新建驱动时也会清掉窗口已释放的条目
    static DRIVERS: RefCell<HashMap<usize, Retained<FrameDriver>>> = RefCell::new(HashMap::new());
}

impl FrameDriver {
    fn for_window(ns: &NSWindow, mtm: MainThreadMarker) -> Retained<Self> {
        let key = std::ptr::from_ref(ns) as usize;
        DRIVERS.with(|map| {
            let mut map = map.borrow_mut();
            if let Some(d) = map.get(&key)
                && d.ivars().window.load().is_some()
            {
                return d.clone();
            }
            // 顺带清掉已销毁窗口（Pinned / Overlay）留下的驱动
            map.retain(|_, d| d.ivars().window.load().is_some());
            let this = Self::alloc(mtm).set_ivars(DriverIvars {
                window: Weak::new(ns),
                motion: RefCell::new(Motion::default()),
                ticking: Cell::new(false),
            });
            // SAFETY: NSObject 的 init
            let driver: Retained<Self> = unsafe { msg_send![super(this), init] };
            map.insert(key, driver.clone());
            driver
        })
    }

    /// 动画开始时确保 displayLink 在跑
    fn ensure_link(&self, ns: &NSWindow) {
        let mut m = self.ivars().motion.borrow_mut();
        if m.link.is_some() {
            return;
        }
        // SAFETY: target 是 self（displayLink 持有它直到 invalidate），selector `onFrame:` 由 define_class 定义，
        // 签名为 (self, _cmd, CADisplayLink*)
        let link = unsafe { ns.displayLinkWithTarget_selector(self, sel!(onFrame:)) };
        link.setPreferredFrameRateRange(CAFrameRateRange {
            minimum: 60.0,
            maximum: 120.0,
            preferred: 120.0,
        });
        // SAFETY: 主线程的 run loop；common modes 保证菜单跟踪 / 拖动期间也继续出帧
        unsafe { link.addToRunLoop_forMode(&NSRunLoop::mainRunLoop(), NSRunLoopCommonModes) };
        LIVE_LINKS.fetch_add(1, Ordering::Relaxed);
        m.link = Some(link);
        m.last_ts = None;
        if STATS_ENABLED.load(Ordering::Relaxed) && m.stats.started.is_none() {
            m.stats.started = Some(Instant::now());
        }
    }

    fn animate_height(
        &self,
        ns: &NSWindow,
        height: f64,
        placement: Placement,
        bound: Option<(f64, f64)>,
        params: SpringParams,
    ) {
        let height = height.max(1.0);
        {
            let mut m = self.ivars().motion.borrow_mut();
            m.rect = None;
            if let Some(h) = &mut m.height {
                h.spring.retune(params);
                h.spring.retarget(height);
                h.edge = Edge::of(placement);
                h.bound = bound;
                #[cfg(any(debug_assertions, feature = "fake-llm"))]
                if STATS_ENABLED.load(Ordering::Relaxed)
                    && let Some((t0, trace)) = &mut m.lab_height
                {
                    let at = (objc2_quartz_core::CACurrentMediaTime() - *t0) * 1000.0;
                    trace.targets.push((at, height, lab_trace::spring_name(params)));
                }
            } else {
                let frame = ns.frame();
                let current = frame.size.height;
                if (current - height).abs() < sizing::SPRING_REST_DELTA {
                    return;
                }
                let mut spring = Spring::new(params, current);
                spring.retarget(height);
                m.height = Some(HeightMotion {
                    spring,
                    edge: Edge::of(placement),
                    bound,
                    applied: current,
                });
                #[cfg(any(debug_assertions, feature = "fake-llm"))]
                {
                    m.lab_height = STATS_ENABLED.load(Ordering::Relaxed).then(|| {
                        (
                            objc2_quartz_core::CACurrentMediaTime(),
                            super::LabHeightTrace {
                                start_unix_ms: lab_trace::unix_ms(),
                                from: current,
                                targets: vec![(0.0, height, lab_trace::spring_name(params))],
                                samples: Vec::new(),
                            },
                        )
                    });
                }
            }
        }
        self.ensure_link(ns);
    }

    /// 矩形弹簧到 `target`（Cocoa 坐标），从窗口当前矩形出发
    fn animate_rect(&self, ns: &NSWindow, target: NSRect) {
        {
            let mut m = self.ivars().motion.borrow_mut();
            m.height = None;
            let from = ns.frame();
            let spring = |from: f64, to: f64| {
                let mut s = Spring::new(SpringParams::GENTLE, from);
                s.retarget(to);
                s
            };
            m.rect = Some(RectMotion {
                x: spring(from.origin.x, target.origin.x),
                top: spring(from.origin.y + from.size.height, target.origin.y + target.size.height),
                width: spring(from.size.width, target.size.width),
                height: spring(from.size.height, target.size.height),
                applied: from,
            });
        }
        self.ensure_link(ns);
    }

    /// 排一次宽度切换（替换尚未执行的上一次）；displayLink 在到期后的第一帧执行
    fn schedule_width(&self, ns: &NSWindow, pending: PendingWidth) {
        {
            let mut m = self.ivars().motion.borrow_mut();
            m.rect = None;
            m.width = Some(pending);
        }
        self.ensure_link(ns);
    }

    fn fade(&self, to: f64, fade: Fade, then: Option<Box<dyn FnOnce()>>) {
        let Some(ns) = self.ivars().window.load() else { return };
        {
            let mut m = self.ivars().motion.borrow_mut();
            m.alpha = Some(AlphaMotion {
                from: ns.alphaValue(),
                to,
                start: objc2_quartz_core::CACurrentMediaTime(),
                duration: fade.duration,
                curve: fade.curve,
                then,
            });
        }
        self.ensure_link(&ns);
    }

    /// 窗口 alpha 是否还在补间（外壳的进入淡入还没播完）
    fn alpha_running(&self) -> bool {
        self.ivars().motion.borrow().alpha.is_some()
    }

    /// 矩形弹簧是否在跑（外壳正移向新会话的矩形）
    fn rect_running(&self) -> bool {
        self.ivars().motion.borrow().rect.is_some()
    }

    /// 冷唤出外壳：WKWebView 的内容先完全透明（停掉进行中的内容补间）
    fn hide_content(&self, ns: &NSWindow) {
        self.ivars().motion.borrow_mut().content = None;
        if let Some((_, web)) = ns.contentView().and_then(|v| find_webview(&v)) {
            web.setAlphaValue(0.0);
        }
    }

    /// 内容从当前 alpha 淡入到 1（外壳上的会话 ready）
    fn fade_content(&self, ns: &NSWindow, fade: Fade) {
        let Some((_, web)) = ns.contentView().and_then(|v| find_webview(&v)) else {
            return;
        };
        let from = web.alphaValue();
        if from >= 1.0 {
            return;
        }
        self.ivars().motion.borrow_mut().content = Some(ContentMotion {
            view: Weak::new(&web),
            from,
            start: objc2_quartz_core::CACurrentMediaTime(),
            duration: fade.duration,
            curve: fade.curve,
        });
        self.ensure_link(ns);
    }

    /// 停掉内容补间，内容停在当前 alpha（隐藏开始时；下一次显示由 [`Self::reset_content`] 复位）
    fn stop_content(&self) {
        let dropped = self.ivars().motion.borrow_mut().content.take();
        drop(dropped);
        self.release_link_if_idle();
    }

    /// 普通的显示：内容不透明（上一次外壳没等到 ready 就结束时它还是透明的）
    fn reset_content(&self, ns: &NSWindow) {
        self.ivars().motion.borrow_mut().content = None;
        if let Some((_, web)) = ns.contentView().and_then(|v| find_webview(&v))
            && web.alphaValue() < 1.0
        {
            web.setAlphaValue(1.0);
        }
    }

    /// 停止全部几何动画（窗口停在当前帧），连同还没执行的宽度切换：新的定位 / 显示 / 隐藏以它们自己的矩形为准
    fn stop_geometry(&self) {
        {
            let mut m = self.ivars().motion.borrow_mut();
            m.height = None;
            m.rect = None;
            m.width = None;
        }
        self.release_link_if_idle();
    }

    /// 只停高度与矩形动画，保留排好的宽度切换（减少动效时高度直接到位，宽度仍要等旧内容透明）
    fn stop_resize(&self) {
        {
            let mut m = self.ivars().motion.borrow_mut();
            m.height = None;
            m.rect = None;
        }
        self.release_link_if_idle();
    }

    /// 只停高度弹簧（停在最近一次写进窗口的高度）；矩形弹簧、宽度切换、alpha 照常
    fn stop_height(&self) {
        {
            let mut m = self.ivars().motion.borrow_mut();
            m.height = None;
            #[cfg(any(debug_assertions, feature = "fake-llm"))]
            if let Some((_, trace)) = m.lab_height.take() {
                lab_trace::height(trace);
            }
        }
        self.release_link_if_idle();
    }

    /// 停止 alpha 补间并丢弃它的 `then`（被新的 show 打断的退出不再 orderOut）
    fn stop_alpha(&self) {
        let dropped = self.ivars().motion.borrow_mut().alpha.take();
        drop(dropped);
        self.release_link_if_idle();
    }

    /// 进行中的退出淡出（带 `then` 的 alpha 补间）立即完成：执行它的 `then`（orderOut + 通知）。
    /// 进入淡入（没有 `then`）不受影响
    fn finish_exit(&self) {
        let then = {
            let mut m = self.ivars().motion.borrow_mut();
            if m.alpha.as_ref().is_some_and(|a| a.then.is_some()) {
                m.alpha.take().and_then(|a| a.then)
            } else {
                None
            }
        };
        self.release_link_if_idle();
        if let Some(then) = then {
            then();
        }
    }

    /// 没有任何动画时立即 `invalidate` displayLink。不能等 `tick` 自己发现空闲：调用方停下动画后往往马上 `orderOut`
    /// （固定交接、立即隐藏、销毁），离屏窗口的 displayLink 不再出帧，`tick` 永远不会再来——displayLink 留在
    /// run loop 里，连同它的 target（本驱动）一起泄漏（窗口销毁后仍留在 AppKit 与窗口服务器里）
    fn release_link_if_idle(&self) {
        let link = {
            let mut m = self.ivars().motion.borrow_mut();
            if !m.idle() {
                return;
            }
            m.last_ts = None;
            m.link.take()
        };
        if let Some(link) = link {
            retire_link(&link);
        }
    }

    /// 驱动不再使用（窗口销毁 / 离屏）：进行中的退出淡出照常收尾（执行 `then`，隐藏的完成通知不会丢），其余动画丢弃，
    /// displayLink 立即 `invalidate`
    fn release(&self) {
        self.finish_exit();
        let (link, dropped) = {
            let mut m = self.ivars().motion.borrow_mut();
            m.height = None;
            m.rect = None;
            m.width = None;
            m.content = None;
            m.last_ts = None;
            (m.link.take(), m.alpha.take())
        };
        drop(dropped);
        if let Some(link) = link {
            retire_link(&link);
        }
    }

    /// 释放某个窗口的驱动（主线程；窗口销毁之前调用）
    fn forget(ns: &NSWindow) {
        let key = std::ptr::from_ref(ns) as usize;
        // 先从表里取出再释放：`release` 执行的 `then` 可能经 `for_window` 再次访问 DRIVERS
        let driver = DRIVERS.with(|map| map.borrow_mut().remove(&key));
        if let Some(driver) = driver {
            driver.release();
        }
    }

    /// 释放窗口已释放或已离屏的驱动（主线程）。离屏窗口的动画已无意义；之后再用到时按需重建
    fn release_offscreen() -> usize {
        let stale: Vec<Retained<Self>> = DRIVERS.with(|map| {
            let mut map = map.borrow_mut();
            let keys: Vec<usize> = map
                .iter()
                .filter(|(_, d)| d.ivars().window.load().is_none_or(|w| !w.isVisible()))
                .map(|(k, _)| *k)
                .collect();
            keys.iter().filter_map(|k| map.remove(k)).collect()
        });
        for d in &stale {
            d.release();
        }
        stale.len()
    }

    /// 诊断：displayLink 是否在跑、各动画是否进行中
    fn describe(&self) -> String {
        let m = self.ivars().motion.borrow();
        format!(
            "link={} height={} rect={} width={} alpha={}",
            m.link.is_some(),
            m.height.is_some(),
            m.rect.is_some(),
            m.width.is_some(),
            m.alpha.is_some()
        )
    }

    fn record_immediate(&self, cost: Duration) {
        if STATS_ENABLED.load(Ordering::Relaxed) {
            log::info!("frame_stats kind=immediate set_frame_us={}", cost.as_micros());
        }
    }

    fn tick(&self, link: &CADisplayLink) {
        if self.ivars().ticking.replace(true) {
            return;
        }
        let t_cb = Instant::now();
        let stats = STATS_ENABLED.load(Ordering::Relaxed);
        let mut finished: Option<Box<dyn FnOnce()>> = None;
        let mut idle = true;
        if let Some(ns) = self.ivars().window.load() {
            let mut guard = self.ivars().motion.borrow_mut();
            let m = &mut *guard;
            let now = link.targetTimestamp();
            let dt = m.last_ts.map_or(link.targetTimestamp() - link.timestamp(), |t| now - t);
            m.last_ts = Some(now);
            if stats {
                m.stats.nominal = (link.duration() * 1000.0) as f32;
                m.stats.intervals.push((dt * 1000.0) as f32);
                #[cfg(any(debug_assertions, feature = "fake-llm"))]
                lab_trace::frame(
                    (dt * 1000.0) as f32,
                    m.stats.nominal,
                    m.alpha.is_some() || m.content.is_some(),
                    m.height.is_some(),
                );
            }
            #[cfg(any(debug_assertions, feature = "fake-llm"))]
            let lab_tracked = m.alpha.is_some() || m.content.is_some() || m.height.is_some();
            let scale = ns.backingScaleFactor();

            // 到期的宽度切换：这一帧的 setFrame 同时改左边与宽度（WebView 先在同一个 CA 事务里重新固定）
            let switch = m.width.take_if(|w| now >= w.at);
            if let Some(pin) = switch.as_ref().and_then(|w| w.pin)
                && let Err(e) = pin_webview_on_main(&ns, pin)
            {
                log::warn!("panel pin webview failed: {}", e.message);
            }
            let horizontal = |frame: NSRect| match &switch {
                Some(w) => (w.x, w.width),
                None => (frame.origin.x, frame.size.width),
            };
            let mut width_applied = switch.is_none();

            if let Some(h) = &mut m.height {
                let done = h.spring.step(dt);
                let height = sizing::snap_to_pixel(h.spring.value(), scale).max(1.0);
                if height != h.applied || !width_applied {
                    let frame = ns.frame();
                    let (x, width) = horizontal(frame);
                    let y = height_frame_y(frame, h.edge, height, h.bound);
                    let t0 = Instant::now();
                    ns.setFrame_display(NSRect::new(NSPoint::new(x, y), NSSize::new(width, height)), true);
                    let t1 = Instant::now();
                    ns.invalidateShadow();
                    if stats {
                        m.stats.set_frame.push((t1 - t0).as_secs_f32() * 1e6);
                        m.stats.shadow.push(t1.elapsed().as_secs_f32() * 1e6);
                    }
                    h.applied = height;
                    width_applied = true;
                }
                #[cfg(any(debug_assertions, feature = "fake-llm"))]
                if stats && let Some((t0, trace)) = &mut m.lab_height {
                    trace.samples.push(((now - *t0) * 1000.0, height));
                }
                if done {
                    m.height = None;
                    #[cfg(any(debug_assertions, feature = "fake-llm"))]
                    if let Some((_, trace)) = m.lab_height.take() {
                        lab_trace::height(trace);
                    }
                }
            }
            if !width_applied {
                // 高度静止：只改左边与宽度，高度与固定边不动
                let frame = ns.frame();
                let (x, width) = horizontal(frame);
                ns.setFrame_display(
                    NSRect::new(NSPoint::new(x, frame.origin.y), NSSize::new(width, frame.size.height)),
                    true,
                );
                ns.invalidateShadow();
            }

            if let Some(r) = &mut m.rect {
                // 四条弹簧都要推进（不能短路）
                let mut done = true;
                for spring in [&mut r.x, &mut r.top, &mut r.width, &mut r.height] {
                    done &= spring.step(dt);
                }
                let snap = |v: f64| sizing::snap_to_pixel(v, scale);
                let width = snap(r.width.value()).max(1.0);
                let height = snap(r.height.value()).max(1.0);
                let frame = NSRect::new(
                    NSPoint::new(snap(r.x.value()), snap(r.top.value()) - height),
                    NSSize::new(width, height),
                );
                if frame != r.applied {
                    ns.setFrame_display(frame, true);
                    r.applied = frame;
                }
                if done {
                    m.rect = None;
                }
            }

            if let Some(a) = &mut m.alpha {
                let progress = if a.duration > 0.0 {
                    ((now - a.start) / a.duration).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                ns.setAlphaValue(a.from + (a.to - a.from) * a.curve.ease(progress));
                if progress >= 1.0 {
                    finished = m.alpha.take().and_then(|a| a.then);
                }
            }

            if let Some(c) = &m.content {
                let progress = if c.duration > 0.0 {
                    ((now - c.start) / c.duration).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                match c.view.load() {
                    Some(view) => view.setAlphaValue(c.from + (1.0 - c.from) * c.curve.ease(progress)),
                    None => m.content = None,
                }
                if progress >= 1.0 {
                    m.content = None;
                }
            }

            idle = m.idle();
            if stats {
                m.stats.callback.push(t_cb.elapsed().as_secs_f32() * 1e6);
                #[cfg(any(debug_assertions, feature = "fake-llm"))]
                if lab_tracked {
                    lab_trace::callback(t_cb.elapsed().as_secs_f32() * 1e6);
                }
            }
            if idle {
                if let Some(link) = m.link.take() {
                    retire_link(&link);
                }
                m.last_ts = None;
                if stats {
                    m.stats.summary("animation");
                }
            }
        }
        if idle && let Some(link) = self.ivars().motion.borrow_mut().link.take() {
            // 窗口已释放：停掉 displayLink（它持有 target）
            retire_link(&link);
        }
        self.ivars().ticking.set(false);
        if let Some(then) = finished {
            then();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(y: f64, h: f64) -> NSRect {
        NSRect::new(NSPoint::new(100.0, y), NSSize::new(420.0, h))
    }

    /// 高度弹簧的固定边取窗口当前的位置（用户拖动后不被拉回旧位置）；有边界且窗口在边界内时伸出的部分整体推回
    #[test]
    fn height_frame_follows_the_current_edge_and_bound() {
        // Cocoa 坐标：原点在左下，窗口 y = 100、高 200 → 顶边 300
        assert_eq!(height_frame_y(rect(100.0, 200.0), Edge::Top, 250.0, None), 50.0);
        assert_eq!(height_frame_y(rect(100.0, 200.0), Edge::Bottom, 250.0, None), 100.0);
        // 边界：原点 y ≥ 80、顶边 ≤ 700。向下长到底 → 底边贴住 80，顶边上移
        let bound = Some((80.0, 700.0));
        assert_eq!(height_frame_y(rect(100.0, 200.0), Edge::Top, 250.0, bound), 80.0);
        // Above 向上长到顶 → 顶边贴住 700，底边下移
        assert_eq!(height_frame_y(rect(400.0, 200.0), Edge::Bottom, 350.0, bound), 350.0);
        // 比边界还高：顶边优先
        assert_eq!(height_frame_y(rect(100.0, 200.0), Edge::Top, 800.0, bound), -100.0);
        // 窗口此刻在边界外（用户正把它拖到边界外）：不推
        assert_eq!(height_frame_y(rect(20.0, 200.0), Edge::Top, 250.0, bound), -30.0);
    }

    /// Dock 图标：不在 .app 里运行（单测、`tauri dev`）时 main bundle 里没有 icns，不加载、也就不覆盖 dev 下 Tauri 设的图标；
    /// 在 .app 里的加载与设置由 window lab 的 `WISP_WINDOW_LAB_DOCK_ICON` 验证
    #[test]
    fn bundle_icon_is_absent_outside_an_app_bundle() {
        assert!(load_bundle_icon().is_none());
    }
}
