//! macOS 后端：SCShareableContent 查询（热键按下时就可经 [`prewarm`] 预先发起）→ 每个 SCDisplay 并行调用
//! SCScreenshotManager.captureImage；过滤器排除 Wisp 自己，`setShowsCursor(false)`，`SCStreamConfiguration`
//! 显式设置 width / height = 点数 × scale（不设时 Retina 屏只拿到低分辨率图）。
//!
//! 冻结帧保留 SCK 返回的 CGImage（显示器原生色彩空间，CALayer 直接显示时与屏幕逐像素一致）；
//! RGBA 惰性生成：首次调用 `Frame::rgba()` 时用 CGBitmapContext 一次完成 BGRA → RGBA 与色彩空间到 sRGB 的转换；
//! 裁剪只转换选区（[`cg_region_to_rgba`]），不必先转出整屏。
//!
//! 排除自己：过滤器按应用排除 `exclude_pid`，同时把它自己的菜单栏状态项窗口列为例外（macOS 14 / 15 上状态项窗口
//! 归应用自己所有，不例外的话冻结帧的菜单栏上会缺 Wisp 的托盘图标；macOS 26 起状态项窗口归 Control Center，
//! 这一步自然为空）。自身应用要从 SCShareableContent 的应用列表里取得，而在屏查询只列出有在屏窗口的应用——
//! 面板隐藏、托盘图标又由 Control Center 托管时 Wisp 不在其中，此时改查一次含屏外窗口的完整列表（见 [`excluded_apps`]）。
//!
//! 常驻状态（都很小，空闲时没有任何线程或定时器）：至多一份 [`prewarm`] 预取的查询结果，最长保留 [`PREWARM_TTL`]，
//! 到期由 GCD 的一次性延时任务释放；自身应用的 `SCRunningApplication` 句柄（几百字节，进程存续期间不变）。
//!
//! 窗口列表用 CGWindowListCopyWindowInfo（OnScreenOnly | ExcludeDesktopElements，按 z 序从前到后），
//! 只保留 layer == 0、不透明度 > 0、不属于自己的窗口。

use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQoS, DispatchQueue, DispatchTime, GlobalQueueIdentifier};
use objc2::rc::Retained;
use objc2::{AnyThread, sel};
use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFRetained, CFString, CFType, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBlendMode, CGColorSpace, CGContext, CGDirectDisplayID, CGDisplayBounds, CGImage,
    CGImageAlphaInfo, CGImageByteOrderInfo, CGInterpolationQuality, CGMainDisplayID, CGPreflightScreenCaptureAccess,
    CGRectMakeWithDictionaryRepresentation, CGWindowListCopyWindowInfo, CGWindowListOption, kCGColorSpaceSRGB,
    kCGNullWindowID, kCGWindowAlpha, kCGWindowBounds, kCGWindowLayer, kCGWindowOwnerName, kCGWindowOwnerPID,
};
use objc2_foundation::{NSArray, NSError, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCCaptureResolutionType, SCContentFilter, SCDisplay, SCRunningApplication, SCScreenshotManager, SCShareableContent,
    SCStreamConfiguration, SCStreamErrorCode, SCWindow,
};
use parking_lot::Mutex;
use wisp_types::{MonitorInfo, PermissionState, PhysRect};

use crate::error::CaptureError;
use crate::frame::{CaptureSet, Frame, WindowRect};
use crate::monitors::{self, RECT_TOLERANCE};
use crate::pixel;
use crate::windows_list::{MonitorSpace, RawWindow, assign_to_monitors};

/// SCShareableContent 查询的超时（冷启动后第一次查询偶尔要几百毫秒）
const CONTENT_TIMEOUT: Duration = Duration::from_secs(3);
/// 自身应用不在在屏列表里时，补查完整列表最多等这么久（实测 ~35 ms）；超时就不排除，不让截图失败
const OWN_APP_LOOKUP_TIMEOUT: Duration = Duration::from_millis(500);
/// 所有显示器截图完成的总超时
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(3);
/// [`prewarm`] 的结果在多长时间内可以被 `capture_all` 直接使用（显示器热插拔后最多晚这么久才被看到）；到期即释放
const PREWARM_TTL: Duration = Duration::from_millis(1500);
/// kCVPixelFormatType_32BGRA（'BGRA'）
const PIXEL_FORMAT_32BGRA: u32 = u32::from_be_bytes(*b"BGRA");
/// 窗口列表里小于它（pt）的窗口不参与窗口检测（菜单栏附属、隐藏的 1×1 辅助窗口等）
const MIN_WINDOW_PT: f64 = 8.0;
/// 菜单栏状态项窗口所在的层级（kCGStatusWindowLevel = NSStatusWindowLevel = 25）
const STATUS_WINDOW_LAYER: isize = 25;
/// 菜单栏高度的上限（pt）：普通屏 24、刘海屏约 37；用于把状态项与同在 25 层的面板区分开
const MENU_BAR_MAX_PT: f64 = 50.0;

/// 跨线程传递的 SCShareableContent。
struct Content(Retained<SCShareableContent>);

// SAFETY: SCShareableContent 是一次查询得到的不可变快照（displays / windows / applications 都是只读属性），
// SCK 本身就在后台队列上创建并回调它；这里只在别的线程读取这些只读属性，不做任何修改。
unsafe impl Send for Content {}

/// 跨线程保存的自身应用句柄
struct OwnApp(Retained<SCRunningApplication>);

// SAFETY: SCRunningApplication 是只读的数据对象（bundleIdentifier / applicationName / processID），
// 这里只把它交给 SCContentFilter，不修改它。
unsafe impl Send for OwnApp {}

type ContentResult = Result<Content, CaptureError>;

/// 正在进行的自身应用查找（含屏外窗口的完整列表查询）：查找的 pid 与结果的接收端
type OwnAppLookup = (u32, Receiver<Option<OwnApp>>);

/// 预先发起、尚未被消费的查询
struct Prefetch {
    /// 区分前后两次预取：到期清理只清理自己那一次
    generation: u64,
    at: Instant,
    /// 在屏 SCShareableContent
    rx: Receiver<ContentResult>,
    /// 预取时还不知道自身应用的话，同时发起的查找
    own_app: Option<OwnAppLookup>,
}

static PREWARMED: Mutex<Option<Prefetch>> = Mutex::new(None);
static PREWARM_GENERATION: AtomicU64 = AtomicU64::new(0);
/// 自身应用（按 pid 记住；同一进程内不变，跨快照使用有效）
static OWN_APP: Mutex<Option<(u32, OwnApp)>> = Mutex::new(None);

/// 屏幕录制权限预检（绝不弹窗）。CGPreflightScreenCaptureAccess 只区分「可用 / 不可用」，
/// 不可用（包括从未询问过）一律报告 Denied——两种情况对调用方的处理相同：显示引导卡片并发起请求
pub(crate) fn preflight() -> PermissionState {
    if CGPreflightScreenCaptureAccess() {
        PermissionState::Granted
    } else {
        PermissionState::Denied
    }
}

/// 预先发起 SCShareableContent 查询（非阻塞）。没有屏幕录制权限时什么也不做，不会触发系统弹窗。
///
/// 结果最多保留 [`PREWARM_TTL`]：到期由 GCD 的一次性延时任务丢弃（不常驻线程）。还不知道自身应用（本进程）时，
/// 同时发起一次自身应用查找，避免截图时再串行补查
pub(crate) fn prewarm() {
    if !CGPreflightScreenCaptureAccess() {
        return;
    }
    let mut slot = PREWARMED.lock();
    if slot.as_ref().is_some_and(|p| p.at.elapsed() < PREWARM_TTL) {
        return;
    }
    // 过期而未被消费的预取先丢弃（连同可能已经到达的快照）
    let stale = slot.take();
    let own_pid = std::process::id();
    let generation = PREWARM_GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    *slot = Some(Prefetch {
        generation,
        at: Instant::now(),
        rx: request_content(true),
        own_app: remembered_app(own_pid)
            .is_none()
            .then(|| (own_pid, lookup_own_app(own_pid))),
    });
    drop(slot);
    drop(stale);
    schedule_prefetch_expiry(generation);
}

/// [`PREWARM_TTL`] 之后丢弃这一次预取（期间已被消费或被新的预取替换时什么也不做）
fn schedule_prefetch_expiry(generation: u64) {
    let Ok(when) = DispatchTime::try_from(PREWARM_TTL) else {
        return;
    };
    let queue = DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(DispatchQoS::Utility));
    let scheduled = queue.after(when, move || {
        let mut slot = PREWARMED.lock();
        let expired = slot
            .as_ref()
            .is_some_and(|p| p.generation == generation)
            .then(|| slot.take());
        drop(slot);
        drop(expired);
    });
    if let Err(e) = scheduled {
        log::debug!("预取结果的到期清理没有排上（下一次 prewarm / capture 时清理）：{e:?}");
    }
}

/// 阻塞地并行捕获所有显示器
pub(crate) fn capture_all(monitors: &[MonitorInfo], exclude_pid: u32) -> Result<CaptureSet, CaptureError> {
    if monitors.is_empty() {
        return Err(CaptureError::NoDisplays);
    }
    if !CGPreflightScreenCaptureAccess() {
        return Err(CaptureError::PermissionDenied);
    }
    let started = Instant::now();
    let (content, own_app) = take_content(exclude_pid)?;
    let content = content.0;

    // SAFETY: 只读取查询快照的属性
    let displays: Vec<Retained<SCDisplay>> = unsafe { content.displays() }.to_vec();
    let excluded = NSArray::from_retained_slice(&excluded_apps(&content, exclude_pid, own_app));
    let excepted = NSArray::from_retained_slice(&own_status_items(&content, &displays, exclude_pid));

    let paired = monitors::pair(
        monitors,
        displays,
        // SAFETY: 只读属性
        |m, d| m.id.parse::<CGDirectDisplayID>().ok() == Some(unsafe { d.displayID() }),
        |m, d| {
            // SAFETY: 只读属性
            let f = unsafe { d.frame() };
            monitors::rect_close(
                (m.frame.x, m.frame.y, m.frame.width, m.frame.height),
                (f.origin.x, f.origin.y, f.size.width, f.size.height),
                RECT_TOLERANCE,
            )
        },
    );
    if paired.is_empty() {
        return Err(CaptureError::NoDisplays);
    }

    // 先把所有截图请求发出去（SCK 在自己的队列上并行处理），再在本线程取窗口列表，最后统一等待
    let pending: Vec<_> = paired
        .iter()
        .map(|(monitor, display)| {
            (
                monitor.clone(),
                request_screenshot(display, &excluded, &excepted, monitor),
            )
        })
        .collect();
    let spaces: Vec<_> = paired
        .iter()
        // SAFETY: 只读属性
        .map(|(monitor, display)| (monitor.id.clone(), unsafe { display.displayID() }))
        .collect();
    let windows = list_windows(&spaces, exclude_pid);

    let deadline = Instant::now() + CAPTURE_TIMEOUT;
    let mut frames = Vec::with_capacity(pending.len());
    let mut first_error = None;
    for (monitor, rx) in pending {
        let wait = deadline.saturating_duration_since(Instant::now());
        let result = match rx.recv_timeout(wait) {
            Ok(r) => r,
            Err(RecvTimeoutError::Timeout) => Err(CaptureError::Backend("screenshot timed out".to_owned())),
            Err(RecvTimeoutError::Disconnected) => Err(CaptureError::Backend("screenshot handler dropped".to_owned())),
        };
        match result {
            Ok(cg) => frames.push(Frame::from_cg_image(monitor, cg)),
            Err(e) => {
                log::warn!("显示器 {} 截图失败：{e}", monitor.id);
                first_error.get_or_insert(e);
            }
        }
    }
    if frames.is_empty() {
        return Err(first_error.unwrap_or(CaptureError::NoDisplays));
    }
    log::debug!(
        "SCK 捕获 {} 个显示器、{} 个窗口矩形，用时 {} ms",
        frames.len(),
        windows.len(),
        started.elapsed().as_millis()
    );
    Ok(CaptureSet { frames, windows })
}

/// 取一份在屏 SCShareableContent：优先用仍在有效期内的预取结果，否则现查。
///
/// 同时给出正在进行的自身应用查找（预取时发起的；或者现查时还不知道 `exclude_pid` 的应用，就与在屏查询同时发起，
/// 两个查询并行，不串行等待）
fn take_content(exclude_pid: u32) -> Result<(Content, Option<OwnAppLookup>), CaptureError> {
    let prefetched = PREWARMED.lock().take();
    let mut own_app = None;
    if let Some(p) = prefetched
        && p.at.elapsed() < PREWARM_TTL
    {
        own_app = p.own_app.filter(|(pid, _)| *pid == exclude_pid);
        match p.rx.recv_timeout(CONTENT_TIMEOUT) {
            Ok(Ok(content)) => return Ok((content, own_app)),
            Ok(Err(e)) => return Err(e),
            Err(_) => log::warn!("预取的 SCShareableContent 没有返回，重新查询"),
        }
    }
    let rx = request_content(true);
    if own_app.is_none() && remembered_app(exclude_pid).is_none() {
        own_app = Some((exclude_pid, lookup_own_app(exclude_pid)));
    }
    match rx.recv_timeout(CONTENT_TIMEOUT) {
        Ok(r) => r.map(|content| (content, own_app)),
        Err(_) => Err(CaptureError::Backend("SCShareableContent query timed out".to_owned())),
    }
}

/// 过滤器要排除的应用：`exclude_pid` 对应的 SCRunningApplication。
///
/// 依次找：本次在屏快照 → 记住的句柄 → 含屏外窗口的完整列表（`lookup`，没有就现查）。Wisp 此刻没有在屏窗口时
/// 不在在屏快照里（macOS 26 的状态项归 Control Center，面板又已隐藏），但之后新出现的窗口仍要被排除，所以不能
/// 就此放弃。都找不到时记 warn 并返回空（只能依赖调用方在截图前隐藏窗口）
fn excluded_apps(
    content: &SCShareableContent,
    exclude_pid: u32,
    lookup: Option<OwnAppLookup>,
) -> Vec<Retained<SCRunningApplication>> {
    if let Some(app) = find_app(content, exclude_pid) {
        remember_app(exclude_pid, app.clone());
        return vec![app];
    }
    if let Some(app) = remembered_app(exclude_pid) {
        return vec![app];
    }
    let rx = match lookup {
        Some((pid, rx)) if pid == exclude_pid => rx,
        _ => lookup_own_app(exclude_pid),
    };
    match rx.recv_timeout(OWN_APP_LOOKUP_TIMEOUT) {
        Ok(Some(OwnApp(app))) => vec![app],
        Ok(None) => {
            log::warn!(
                "进程 {exclude_pid} 不在 ScreenCaptureKit 的应用列表中，截图过滤器无法排除它的窗口（依赖截图前隐藏窗口）"
            );
            Vec::new()
        }
        Err(_) => {
            log::warn!("查找自身应用超时，本次截图过滤器不排除进程 {exclude_pid}（依赖截图前隐藏窗口）");
            Vec::new()
        }
    }
}

/// 发起自身应用查找：查一次含屏外窗口的完整列表（实测 ~35 ms），找到就记住并经 channel 返回
fn lookup_own_app(pid: u32) -> Receiver<Option<OwnApp>> {
    let (tx, rx) = mpsc::sync_channel::<Option<OwnApp>>(1);
    query_content(false, move |result| {
        let app = result.ok().and_then(|c| find_app(&c.0, pid));
        if let Some(app) = &app {
            remember_app(pid, app.clone());
        }
        let _ = tx.try_send(app.map(OwnApp));
    });
    rx
}

fn find_app(content: &SCShareableContent, pid: u32) -> Option<Retained<SCRunningApplication>> {
    // SAFETY: 只读取查询快照的属性
    unsafe { content.applications() }
        .iter()
        // SAFETY: 只读属性
        .find(|app| i64::from(unsafe { app.processID() }) == i64::from(pid))
}

fn remembered_app(pid: u32) -> Option<Retained<SCRunningApplication>> {
    OWN_APP
        .lock()
        .as_ref()
        .filter(|(p, _)| *p == pid)
        .map(|(_, app)| app.0.clone())
}

fn remember_app(pid: u32, app: Retained<SCRunningApplication>) {
    *OWN_APP.lock() = Some((pid, OwnApp(app)));
}

/// `pid` 自己的菜单栏状态项窗口（作为过滤器的例外留在冻结帧里，与实时菜单栏一致）。
///
/// 判定：层级 25、在屏、整个窗口落在某个显示器顶部的菜单栏带内。Wisp 的面板同在 25 层，但它位于菜单栏之下的
/// 工作区里且远高于菜单栏，不会被误判。macOS 26 起状态项窗口归 Control Center，这里自然返回空
fn own_status_items(
    content: &SCShareableContent,
    displays: &[Retained<SCDisplay>],
    pid: u32,
) -> Vec<Retained<SCWindow>> {
    // SAFETY: 只读属性
    let tops: Vec<f64> = displays.iter().map(|d| unsafe { d.frame() }.origin.y).collect();
    // SAFETY: 只读取查询快照的属性
    unsafe { content.windows() }
        .iter()
        .filter(|w| {
            // SAFETY: 只读属性
            let (layer, on_screen, frame, owner) =
                unsafe { (w.windowLayer(), w.isOnScreen(), w.frame(), w.owningApplication()) };
            // SAFETY: 只读属性
            let owned = owner.is_some_and(|app| i64::from(unsafe { app.processID() }) == i64::from(pid));
            layer == STATUS_WINDOW_LAYER
                && on_screen
                && owned
                && in_menu_bar_band((frame.origin.y, frame.size.height), &tops)
        })
        .collect()
}

/// 冒烟测试用：是否有尚未被消费、也未到期释放的预取结果
#[cfg(test)]
pub(crate) fn prefetch_pending() -> bool {
    PREWARMED.lock().is_some()
}

/// 冒烟测试用：预取结果的保留时长
#[cfg(test)]
pub(crate) const PREFETCH_TTL_FOR_TEST: Duration = PREWARM_TTL;

/// 窗口（顶边 y、高度，全局点坐标，y 向下）是否整个落在某个显示器顶部的菜单栏带内
fn in_menu_bar_band((y, height): (f64, f64), display_tops: &[f64]) -> bool {
    height > 0.0
        && height <= MENU_BAR_MAX_PT
        && display_tops
            .iter()
            .any(|&top| y >= top - 1.0 && y + height <= top + MENU_BAR_MAX_PT)
}

/// 发起在屏（或含屏外窗口的完整）SCShareableContent 查询，结果经 channel 返回
fn request_content(on_screen_only: bool) -> Receiver<ContentResult> {
    let (tx, rx) = mpsc::sync_channel::<ContentResult>(1);
    query_content(on_screen_only, move |result| {
        let _ = tx.try_send(result);
    });
    rx
}

/// 发起 SCShareableContent 查询，`done` 在 SCK 的回调队列上被调用一次
fn query_content(on_screen_only: bool, done: impl Fn(ContentResult) + 'static) {
    // 命令行进程（测试）里 SCK 依赖已建立的 WindowServer 连接；CGMainDisplayID 会建立它，GUI 进程里只是一次廉价调用
    let _ = CGMainDisplayID();
    let block = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        // SAFETY: SCK 回调给出的指针要么为空，要么指向有效对象；retain 后由本进程持有
        let result = match unsafe { Retained::retain(content) } {
            Some(c) => Ok(Content(c)),
            None => Err(error_from(error)),
        };
        done(result);
    });
    // SAFETY: block 的签名与 SCK 要求的 completionHandler 一致；SCK 会持有（复制）block 直到回调完成
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true,
            on_screen_only,
            &block,
        );
    }
}

/// 为一个显示器发起截图（排除 `excluded` 应用的窗口，`excepted` 中的窗口除外）
fn request_screenshot(
    display: &SCDisplay,
    excluded: &NSArray<SCRunningApplication>,
    excepted: &NSArray<SCWindow>,
    monitor: &MonitorInfo,
) -> Receiver<Result<CFRetained<CGImage>, CaptureError>> {
    // SAFETY: 参数都是有效对象
    let filter = unsafe {
        SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
            SCContentFilter::alloc(),
            display,
            excluded,
            excepted,
        )
    };
    // 菜单栏也要出现在冻结帧里（菜单栏区域也能框选）；includeMenuBar 是 macOS 14.2+ 的属性
    if filter.respondsToSelector(sel!(setIncludeMenuBar:)) {
        // SAFETY: 已确认对象响应该 selector
        unsafe { filter.setIncludeMenuBar(true) };
    }
    let (width, height) = pixel_size(&filter, display, monitor);

    // SAFETY: 新建配置对象并设置简单的标量属性
    let config = unsafe { SCStreamConfiguration::new() };
    // SAFETY: 同上；captureResolution 是 macOS 14.0+ 的属性，最低系统版本就是 14.0
    unsafe {
        config.setWidth(width);
        config.setHeight(height);
        config.setShowsCursor(false);
        config.setPixelFormat(PIXEL_FORMAT_32BGRA);
        config.setCaptureResolution(SCCaptureResolutionType::Best);
    }

    let (tx, rx) = mpsc::sync_channel(1);
    let tx: SyncSender<Result<CFRetained<CGImage>, CaptureError>> = tx;
    let block = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
        let result = match NonNull::new(image) {
            // SAFETY: SCK 回调给出的 CGImage 在回调期间有效；retain 后由本进程持有
            Some(ptr) => Ok(unsafe { CFRetained::retain(ptr) }),
            None => Err(error_from(error)),
        };
        let _ = tx.try_send(result);
    });
    // SAFETY: filter / config 有效；block 的签名与 completionHandler 一致，SCK 会复制并持有它
    unsafe {
        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(&filter, &config, Some(&block));
    }
    rx
}

/// 输出图像的像素尺寸 = 显示器点数 × 像素比。优先用 SCK 自己对该过滤器给出的 contentRect / pointPixelScale，
/// 取不到时退回 SCDisplay 的点数 × platform 给出的 scale_factor
fn pixel_size(filter: &SCContentFilter, display: &SCDisplay, monitor: &MonitorInfo) -> (usize, usize) {
    // SAFETY: infoForFilter 是同步的类方法，只读取过滤器描述的内容
    let info = unsafe { SCShareableContent::infoForFilter(filter) };
    // SAFETY: 只读属性
    let (scale, rect) = unsafe { (f64::from(info.pointPixelScale()), info.contentRect()) };
    let from_info = (scale.is_finite() && scale > 0.0 && rect.size.width > 0.0 && rect.size.height > 0.0)
        .then_some((rect.size.width * scale, rect.size.height * scale));
    let (w, h) = from_info.unwrap_or_else(|| {
        let s = if monitor.scale_factor.is_finite() && monitor.scale_factor > 0.0 {
            monitor.scale_factor
        } else {
            1.0
        };
        // SAFETY: 只读属性
        let (pw, ph) = unsafe { (display.width(), display.height()) };
        (pw as f64 * s, ph as f64 * s)
    });
    (w.round().max(1.0) as usize, h.round().max(1.0) as usize)
}

/// 把 SCK / NSError 归一为 CaptureError（用户拒绝授权 → PermissionDenied）
fn error_from(error: *mut NSError) -> CaptureError {
    // SAFETY: 回调给出的 NSError 指针要么为空，要么有效
    let Some(error) = (unsafe { Retained::retain(error) }) else {
        return CaptureError::Backend("ScreenCaptureKit returned neither a result nor an error".to_owned());
    };
    if error.code() == SCStreamErrorCode::UserDeclined.0 {
        return CaptureError::PermissionDenied;
    }
    CaptureError::Backend(format!(
        "{} ({}): {}",
        error.domain(),
        error.code(),
        error.localizedDescription()
    ))
}

/// CGWindowList 的窗口列表，切分到各显示器（`spaces`：显示器 id 与 CGDirectDisplayID）
fn list_windows(spaces: &[(String, CGDirectDisplayID)], exclude_pid: u32) -> Vec<WindowRect> {
    let option = CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements;
    let Some(list) = CGWindowListCopyWindowInfo(option, kCGNullWindowID) else {
        log::warn!("CGWindowListCopyWindowInfo 返回空，窗口检测不可用");
        return Vec::new();
    };
    // SAFETY: CGWindowListCopyWindowInfo 文档规定返回 CFDictionary 的数组，键是 CFString
    let list: &CFArray<CFDictionary<CFString, CFType>> = unsafe { list.cast_unchecked() };
    // SAFETY: 这些键是 CoreGraphics 导出的常量 CFString，进程生命周期内有效
    let (k_layer, k_pid, k_alpha, k_bounds, k_owner) = unsafe {
        (
            kCGWindowLayer,
            kCGWindowOwnerPID,
            kCGWindowAlpha,
            kCGWindowBounds,
            kCGWindowOwnerName,
        )
    };

    let mut raws = Vec::with_capacity(list.len());
    for info in list.iter() {
        let number = |key: &CFString| {
            info.get(key)
                .and_then(|v| v.downcast_ref::<CFNumber>().and_then(|n| n.as_f64()))
        };
        if number(k_layer) != Some(0.0) {
            continue;
        }
        if number(k_pid).is_some_and(|pid| pid == f64::from(exclude_pid)) {
            continue;
        }
        if number(k_alpha).is_some_and(|a| a <= 0.0) {
            continue;
        }
        let Some(bounds) = info.get(k_bounds).and_then(|v| v.downcast::<CFDictionary>().ok()) else {
            continue;
        };
        let mut rect = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(0.0, 0.0));
        // SAFETY: bounds 是 kCGWindowBounds 对应的矩形字典表示；rect 是有效的可写指针
        if !unsafe { CGRectMakeWithDictionaryRepresentation(Some(&bounds), &mut rect) } {
            continue;
        }
        if rect.size.width < MIN_WINDOW_PT || rect.size.height < MIN_WINDOW_PT {
            continue;
        }
        let app_name = info
            .get(k_owner)
            .and_then(|v| v.downcast_ref::<CFString>().map(ToString::to_string))
            .unwrap_or_default();
        raws.push(RawWindow {
            x: rect.origin.x,
            y: rect.origin.y,
            width: rect.size.width,
            height: rect.size.height,
            app_name,
        });
    }

    // CGWindowBounds 与 CGDisplayBounds 同处全局点坐标（主屏左上为原点，y 向下），点即逻辑 px
    let monitors: Vec<MonitorSpace> = spaces
        .iter()
        .map(|(id, display)| {
            let b = CGDisplayBounds(*display);
            MonitorSpace {
                id: id.clone(),
                x: b.origin.x,
                y: b.origin.y,
                width: b.size.width,
                height: b.size.height,
                unit_to_logical: 1.0,
            }
        })
        .collect();
    assign_to_monitors(&raws, &monitors)
}

/// CGImage → 紧密排列的 sRGB RGBA8（alpha 恒为 255）。
///
/// 用 CGBitmapContext 绘制一次：通道重排与色彩空间转换（显示器原生 → sRGB）由 CoreGraphics（vImage / ColorSync）
/// 一步完成，直接写进目标缓冲，没有中间拷贝。尺寸不符或创建上下文失败时返回 None
pub(crate) fn cg_image_to_rgba(image: &CGImage, width: u32, height: u32) -> Option<Vec<u8>> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || CGImage::width(Some(image)) != w || CGImage::height(Some(image)) != h {
        return None;
    }
    draw_to_rgba(image, w, h, CGInterpolationQuality::None)
}

/// 把整张 `image` 画进一块 `w × h` 的 sRGB RGBA8 缓冲（`Copy` 混合、alpha 置 255）：尺寸与图像相同时是逐像素的格式转换，
/// 不同时由 CoreGraphics 按 `quality` 缩放
fn draw_to_rgba(image: &CGImage, w: usize, h: usize, quality: CGInterpolationQuality) -> Option<Vec<u8>> {
    if w == 0 || h == 0 {
        return None;
    }
    // SAFETY: kCGColorSpaceSRGB 是 CoreGraphics 导出的常量
    let srgb = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))?;
    let mut buf = vec![0u8; w * h * 4];
    let info = CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0;
    // SAFETY: buf 至少 w * h * 4 字节、行跨度 w * 4，在 ctx 存活期间不移动、不被别处访问；ctx 在本函数内释放
    let ctx = unsafe { CGBitmapContextCreate(buf.as_mut_ptr().cast(), w, h, 8, w * 4, Some(&srgb), info) }?;
    CGContext::set_blend_mode(Some(&ctx), CGBlendMode::Copy);
    CGContext::set_interpolation_quality(Some(&ctx), quality);
    let rect = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as f64, h as f64));
    CGContext::draw_image(Some(&ctx), rect, Some(image));
    drop(ctx);
    pixel::force_opaque(&mut buf);
    Some(buf)
}

/// CGImage 中一块物理像素区域 → 紧密排列的 sRGB RGBA8（裁剪用，不必先转出整屏）。
///
/// `CGImageCreateWithImageInRect` 与原图共享像素数据（不拷贝），再按 [`cg_image_to_rgba`] 的同一条路径转换，
/// 因此结果与「整屏转换后再裁剪」逐像素相同。区域为空、越界或转换失败时返回 None
pub(crate) fn cg_region_to_rgba(image: &CGImage, region: PhysRect) -> Option<Vec<u8>> {
    let (iw, ih) = (CGImage::width(Some(image)), CGImage::height(Some(image)));
    let fits =
        |start: u32, len: u32, max: usize| (start as usize).checked_add(len as usize).is_some_and(|end| end <= max);
    if region.width == 0
        || region.height == 0
        || !fits(region.x, region.width, iw)
        || !fits(region.y, region.height, ih)
    {
        return None;
    }
    if (region.x, region.y) == (0, 0) && (region.width as usize, region.height as usize) == (iw, ih) {
        return cg_image_to_rgba(image, region.width, region.height);
    }
    // 子图矩形使用图像自身的像素坐标（原点在左上角）
    let rect = CGRect::new(
        CGPoint::new(f64::from(region.x), f64::from(region.y)),
        CGSize::new(f64::from(region.width), f64::from(region.height)),
    );
    let sub = CGImage::with_image_in_rect(Some(image), rect)?;
    cg_image_to_rgba(&sub, region.width, region.height)
}

/// CGImage 中一块物理像素区域，缩放到 `out_w × out_h`（Overlay 飞入用的选区预览）：子图与原图共享像素，
/// 由 CoreGraphics 一次完成裁剪、缩放与色彩转换，不经过整块原尺寸的中间缓冲。尺寸与区域相同时等同
/// [`cg_region_to_rgba`]。区域为空、越界或转换失败时返回 None
pub(crate) fn cg_region_scaled_to_rgba(image: &CGImage, region: PhysRect, out_w: u32, out_h: u32) -> Option<Vec<u8>> {
    if (out_w, out_h) == (region.width, region.height) {
        return cg_region_to_rgba(image, region);
    }
    let (iw, ih) = (CGImage::width(Some(image)), CGImage::height(Some(image)));
    let fits =
        |start: u32, len: u32, max: usize| (start as usize).checked_add(len as usize).is_some_and(|end| end <= max);
    if region.width == 0
        || region.height == 0
        || !fits(region.x, region.width, iw)
        || !fits(region.y, region.height, ih)
    {
        return None;
    }
    let rect = CGRect::new(
        CGPoint::new(f64::from(region.x), f64::from(region.y)),
        CGSize::new(f64::from(region.width), f64::from(region.height)),
    );
    let sub = CGImage::with_image_in_rect(Some(image), rect)?;
    draw_to_rgba(&sub, out_w as usize, out_h as usize, CGInterpolationQuality::Medium)
}

/// 冒烟测试用：按 CoreGraphics 枚举当前所有活动显示器，构造与 platform 层同口径的 `MonitorInfo`
#[cfg(test)]
pub(crate) fn system_monitors() -> Vec<MonitorInfo> {
    use objc2_core_graphics::{CGDisplayCopyDisplayMode, CGDisplayMode, CGGetActiveDisplayList};
    use wisp_types::{PhysPoint, PhysSize, Rect};

    let mut ids = [0 as CGDirectDisplayID; 16];
    let mut count = 0u32;
    // SAFETY: ids 有 16 个元素的可写空间，count 是有效指针
    let err = unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
    if err.0 != 0 {
        return Vec::new();
    }
    let main = CGMainDisplayID();
    ids[..count as usize]
        .iter()
        .map(|&id| {
            let b = CGDisplayBounds(id);
            let mode = CGDisplayCopyDisplayMode(id);
            let pixel_w = CGDisplayMode::pixel_width(mode.as_deref());
            let scale = if b.size.width > 0.0 && pixel_w > 0 {
                pixel_w as f64 / b.size.width
            } else {
                1.0
            };
            let frame = Rect::new(b.origin.x, b.origin.y, b.size.width, b.size.height);
            MonitorInfo {
                id: id.to_string(),
                name: format!("display {id}"),
                physical_origin: PhysPoint {
                    x: (b.origin.x * scale).round() as i32,
                    y: (b.origin.y * scale).round() as i32,
                },
                physical_size: PhysSize {
                    width: (b.size.width * scale).round() as u32,
                    height: (b.size.height * scale).round() as u32,
                },
                scale_factor: scale,
                frame,
                work_area: Rect::new(0.0, 0.0, b.size.width, b.size.height),
                is_primary: id == main,
            }
        })
        .collect()
}

/// 测试用：构造一张内存布局与 SCK 输出相同的 BGRA（小端 32 位、alpha 在前、预乘）sRGB 图像
#[cfg(test)]
pub(crate) fn bgra_image(width: usize, height: usize, bgra: &mut [u8]) -> CFRetained<CGImage> {
    use objc2_core_graphics::CGBitmapContextCreateImage;

    assert!(bgra.len() >= width * height * 4, "像素缓冲不够长");
    // SAFETY: kCGColorSpaceSRGB 是 CoreGraphics 导出的常量
    let srgb = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })).expect("sRGB");
    let info = CGImageAlphaInfo::PremultipliedFirst.0 | CGImageByteOrderInfo::Order32Little.0;
    // SAFETY: bgra 至少 width * height * 4 字节（上面已断言），在 ctx 存活期间不移动；CGBitmapContextCreateImage 会复制像素
    let ctx =
        unsafe { CGBitmapContextCreate(bgra.as_mut_ptr().cast(), width, height, 8, width * 4, Some(&srgb), info) }
            .expect("bitmap context");
    CGBitmapContextCreateImage(Some(&ctx)).expect("image")
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn bgra_cg_image_converts_to_rgba_in_order() {
        // 2×1：左像素 B=10 G=20 R=30，右像素 B=200 G=100 R=50（同为 sRGB，不发生色彩转换，数值精确）
        let mut bgra = vec![10, 20, 30, 255, 200, 100, 50, 255];
        let image = bgra_image(2, 1, &mut bgra);
        let rgba = cg_image_to_rgba(&image, 2, 1).expect("转换成功");
        assert_eq!(rgba, vec![30, 20, 10, 255, 50, 100, 200, 255]);
    }

    #[test]
    fn cg_conversion_rejects_size_mismatch() {
        let mut bgra = vec![0u8; 4 * 4];
        let image = bgra_image(2, 2, &mut bgra);
        assert!(cg_image_to_rgba(&image, 3, 2).is_none());
        assert!(cg_image_to_rgba(&image, 0, 0).is_none());
    }

    #[test]
    fn region_conversion_matches_full_conversion() {
        // 4×3，每个像素各不相同：任意子区域的转换结果都应与整屏转换后裁剪逐字节相同（原点在左上角）
        let mut bgra: Vec<u8> = (0..12u8).flat_map(|i| [i * 20, i * 7 + 1, 255 - i * 9, 255]).collect();
        let image = bgra_image(4, 3, &mut bgra);
        let full = cg_image_to_rgba(&image, 4, 3).expect("整屏转换");
        let region = PhysRect {
            x: 1,
            y: 1,
            width: 2,
            height: 2,
        };
        let part = cg_region_to_rgba(&image, region).expect("区域转换");
        let mut expected = Vec::new();
        for y in 1..3 {
            expected.extend_from_slice(&full[(y * 4 + 1) * 4..(y * 4 + 3) * 4]);
        }
        assert_eq!(part, expected);
        // 第 (1, 1) 个像素：i = 5 → B=100 G=36 R=210
        assert_eq!(&part[..4], &[210, 36, 100, 255]);

        let whole = PhysRect {
            x: 0,
            y: 0,
            width: 4,
            height: 3,
        };
        assert_eq!(cg_region_to_rgba(&image, whole).expect("整图区域"), full);
        let outside = PhysRect {
            x: 3,
            y: 0,
            width: 2,
            height: 1,
        };
        assert!(cg_region_to_rgba(&image, outside).is_none(), "越界区域");
        let empty = PhysRect {
            x: 0,
            y: 0,
            width: 0,
            height: 1,
        };
        assert!(cg_region_to_rgba(&image, empty).is_none(), "空区域");
        // 缩放版：尺寸与区域相同时逐像素等于裁剪；缩小时长度对、alpha 恒 255；越界同样拒绝
        assert_eq!(
            cg_region_scaled_to_rgba(&image, region, region.width, region.height).expect("同尺寸"),
            cg_region_to_rgba(&image, region).expect("区域")
        );
        let small = cg_region_scaled_to_rgba(&image, whole, 2, 1).expect("缩小");
        assert_eq!(small.len(), 2 * 4);
        assert!(small.chunks_exact(4).all(|p| p[3] == 255));
        assert!(cg_region_scaled_to_rgba(&image, outside, 1, 1).is_none(), "越界区域");
    }

    #[test]
    fn status_items_are_told_apart_from_panels() {
        // 主屏顶边 y = 0，副屏在上方（顶边 y = -1080）
        let tops = [0.0, -1080.0];
        assert!(in_menu_bar_band((0.0, 24.0), &tops), "普通屏菜单栏状态项");
        assert!(in_menu_bar_band((0.0, 37.0), &tops), "刘海屏菜单栏状态项");
        assert!(in_menu_bar_band((-1080.0, 24.0), &tops), "副屏菜单栏");
        assert!(
            !in_menu_bar_band((40.0, 300.0), &tops),
            "菜单栏下方的面板（同在 25 层）"
        );
        assert!(!in_menu_bar_band((0.0, 500.0), &tops), "贴顶的大窗口");
        assert!(!in_menu_bar_band((300.0, 24.0), &tops), "屏幕中部的小窗口");
        assert!(!in_menu_bar_band((0.0, 0.0), &tops));
    }

    #[test]
    fn frame_from_cg_image_is_lazy_and_native() {
        let mut bgra = vec![1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255];
        let image = bgra_image(2, 2, &mut bgra);
        let monitor = MonitorInfo {
            id: "1".to_owned(),
            name: "test".to_owned(),
            physical_origin: wisp_types::PhysPoint { x: 0, y: 0 },
            physical_size: wisp_types::PhysSize { width: 2, height: 2 },
            scale_factor: 2.0,
            frame: wisp_types::Rect::new(0.0, 0.0, 1.0, 1.0),
            work_area: wisp_types::Rect::new(0.0, 0.0, 1.0, 1.0),
            is_primary: true,
        };
        let frame = Frame::from_cg_image(monitor, image);
        assert_eq!((frame.width, frame.height), (2, 2));
        assert!(frame.cg_image().is_some(), "SCK 帧保留 CGImage，供原生图层零拷贝显示");
        assert_eq!(&frame.rgba()[..8], &[3, 2, 1, 255, 6, 5, 4, 255]);
        let (w, h, half) = frame.rgba_half();
        assert_eq!((w, h), (1, 1));
        // (3+6+9+12+2)/4 = 8，(2+5+8+11+2)/4 = 7，(1+4+7+10+2)/4 = 6
        assert_eq!(half.as_slice(), &[8, 7, 6, 255]);
    }
}
