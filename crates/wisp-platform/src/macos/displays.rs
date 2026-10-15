//! 显示器几何。
//!
//! 统一使用 Quartz 全局坐标：主屏左上角为原点、y 向下、单位为 pt（= 逻辑 px）。AX 的 `AXBoundsForRange`、
//! `CGEvent` 的位置、`CGDisplayBounds` 都是这个坐标系；Cocoa（NSScreen / NSEvent.mouseLocation）以主屏左下角为原点、
//! y 向上，进出时用主屏高度翻转。`MonitorId` = CGDirectDisplayID 的十进制串。
//!
//! 显示器列表（含精确的工作区与名称）只能在主线程上经 NSScreen 取得，因此缓存起来，`monitors()` **优先用缓存**
//! （显示面板的路径上不必等主线程）：
//! - 显示器配置变化（`NSApplicationDidChangeScreenParametersNotification`，主线程发布，含 Dock 移动）时立即重取；
//! - Dock 改大小 / 自动隐藏不一定触发上面的通知，另外登记 Dock 的偏好变化通知（`com.apple.dock.prefchanged`），
//!   收到后把缓存标记为待刷新，下一次使用时在主线程上重取（那时 Dock 的动画已结束，visibleFrame 是新值）；
//! - 每次使用缓存前还会用 `CGDisplayBounds` 核对每个显示器的 id、几何与主屏，不一致就不用缓存。
//!
//! 缓存不可用时才同步等主线程（≤ 150 ms），主线程忙时退回纯 CG 的近似值（工作区按整屏），绝不返回过期的位置。

use std::ptr::NonNull;
use std::sync::{Arc, Weak};
use std::time::Duration;

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{NSApplicationDidChangeScreenParametersNotification, NSScreen};
use objc2_core_foundation::{CGPoint, CGRect};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayIsMain, CGDisplayMirrorsDisplay,
    CGDisplayMode, CGError, CGGetActiveDisplayList, CGGetDisplaysWithPoint, CGGetOnlineDisplayList, CGMainDisplayID,
    kCGNullDirectDisplay,
};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSNumber, NSRect, ns_string};
use parking_lot::Mutex;
use wisp_types::{Anchor, MonitorId, MonitorInfo, PhysPoint, PhysSize, Point, Rect};

use super::Ctx;
use super::focus::WorkspaceObserver;
use super::notify::DistributedObserver;
use super::util::guarded;

/// 缓存不可用时，主线程取显示器列表的最长等待
const MAIN_WAIT: Duration = Duration::from_millis(150);
/// Dock 偏好变化的分布式通知（大小、位置、自动隐藏、放大）
const DOCK_PREFS_CHANGED: &str = "com.apple.dock.prefchanged";

/// CGDirectDisplayID → MonitorId
pub(crate) fn monitor_id(display: CGDirectDisplayID) -> MonitorId {
    display.to_string()
}

/// MonitorId → CGDirectDisplayID
#[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
pub(crate) fn display_of(id: &str) -> Option<CGDirectDisplayID> {
    id.parse().ok()
}

/// 当前活动的显示器（主屏在前）。
///
/// 所有显示器都在睡眠时（显示器空闲睡眠、合盖接外屏的瞬间、远程桌面连着而本机屏幕已熄灭）活动列表为空，
/// 但 NSScreen 仍报告这些屏幕、窗口照样可以摆放：此时退回在线列表（含睡眠中的显示器），再不行退回主屏。
/// 否则 `monitors()` 的纯 CG 退路会返回空列表，缓存也永远对不上（每次都同步等主线程）
pub(crate) fn active_displays() -> Vec<CGDirectDisplayID> {
    let mut v = display_list(CGGetActiveDisplayList);
    if v.as_ref().is_some_and(Vec::is_empty) {
        // 在线列表含硬件镜像集里的从属显示器，去掉它们，免得同一块区域出现两次
        v = display_list(CGGetOnlineDisplayList).map(|l| {
            l.into_iter()
                .filter(|d| CGDisplayMirrorsDisplay(*d) == kCGNullDirectDisplay)
                .collect()
        });
    }
    let mut v = v.filter(|v| !v.is_empty()).unwrap_or_else(|| vec![CGMainDisplayID()]);
    v.sort_by_key(|d| !CGDisplayIsMain(*d));
    v
}

/// 调用 `CGGet*DisplayList`；出错时 None
fn display_list(
    get: unsafe extern "C-unwind" fn(u32, *mut CGDirectDisplayID, *mut u32) -> CGError,
) -> Option<Vec<CGDirectDisplayID>> {
    let mut ids = [0 as CGDirectDisplayID; 32];
    let mut count = 0u32;
    // SAFETY: 缓冲区长度与 max_displays 一致，count 指向有效的 u32
    let err = unsafe { get(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
    (err == CGError::Success).then(|| ids[..(count as usize).min(ids.len())].to_vec())
}

/// 包含某个全局点的显示器；点落在所有显示器之外（例如屏幕间的缝隙）时取最近的一个
pub(crate) fn display_at(p: CGPoint) -> CGDirectDisplayID {
    let mut id: CGDirectDisplayID = 0;
    let mut count = 0u32;
    // SAFETY: 缓冲区为 1 个元素，与 max_displays 一致
    let err = unsafe { CGGetDisplaysWithPoint(p, 1, &mut id, &mut count) };
    if err == CGError::Success && count > 0 {
        return id;
    }
    active_displays()
        .into_iter()
        .min_by(|a, b| {
            let da = distance_to_rect(p, CGDisplayBounds(*a));
            let db = distance_to_rect(p, CGDisplayBounds(*b));
            da.total_cmp(&db)
        })
        .unwrap_or_else(|| CGMainDisplayID())
}

fn distance_to_rect(p: CGPoint, r: CGRect) -> f64 {
    let dx = (r.origin.x - p.x).max(0.0).max(p.x - (r.origin.x + r.size.width));
    let dy = (r.origin.y - p.y).max(0.0).max(p.y - (r.origin.y + r.size.height));
    dx.hypot(dy)
}

/// 全局点 → (显示器, 显示器内逻辑坐标)
pub(crate) fn locate(p: CGPoint) -> (MonitorId, Point) {
    let d = display_at(p);
    let b = CGDisplayBounds(d);
    (
        monitor_id(d),
        Point {
            x: p.x - b.origin.x,
            y: p.y - b.origin.y,
        },
    )
}

/// 全局矩形 → 锚点：宽高都 > 0、不是全零、中心落在某个显示器内，否则 None
pub(crate) fn anchor_of(r: CGRect) -> Option<Anchor> {
    let valid = r.size.width > 0.0
        && r.size.height > 0.0
        && r.origin.x.is_finite()
        && r.origin.y.is_finite()
        && r.size.width.is_finite()
        && r.size.height.is_finite()
        && !(r.origin.x == 0.0 && r.origin.y == 0.0 && r.size.width <= 1.0 && r.size.height <= 1.0);
    if !valid {
        return None;
    }
    let center = CGPoint::new(r.origin.x + r.size.width / 2.0, r.origin.y + r.size.height / 2.0);
    let mut id: CGDirectDisplayID = 0;
    let mut count = 0u32;
    // SAFETY: 缓冲区为 1 个元素，与 max_displays 一致
    let err = unsafe { CGGetDisplaysWithPoint(center, 1, &mut id, &mut count) };
    if err != CGError::Success || count == 0 {
        return None;
    }
    let b = CGDisplayBounds(id);
    Some(Anchor {
        monitor_id: monitor_id(id),
        rect: Rect::new(
            r.origin.x - b.origin.x,
            r.origin.y - b.origin.y,
            r.size.width,
            r.size.height,
        ),
    })
}

/// 主屏高度（Cocoa ↔ Quartz 翻转用）
pub(crate) fn primary_height() -> f64 {
    CGDisplayBounds(CGMainDisplayID()).size.height
}

/// Cocoa 全局点 → Quartz 全局点
pub(crate) fn cocoa_to_quartz(p: CGPoint) -> CGPoint {
    CGPoint::new(p.x, primary_height() - p.y)
}

/// Quartz 全局矩形 → Cocoa 全局矩形（NSWindow.setFrame 用）
#[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
pub(crate) fn quartz_rect_to_cocoa(r: CGRect) -> NSRect {
    NSRect::new(
        CGPoint::new(r.origin.x, primary_height() - (r.origin.y + r.size.height)),
        r.size,
    )
}

/// 最近一次在主线程上取得的显示器列表，以及让它保持新鲜的观察者
#[derive(Default)]
pub(crate) struct MonitorCache {
    state: Mutex<CacheState>,
    screen_observer: Mutex<Option<WorkspaceObserver>>,
    dock_observer: Mutex<Option<DistributedObserver>>,
}

#[derive(Default)]
struct CacheState {
    list: Vec<MonitorInfo>,
    /// 工作区可能已变（Dock 偏好变化）：下一次使用前先在主线程上重取
    dirty: bool,
}

impl MonitorCache {
    fn store(&self, list: Vec<MonitorInfo>) {
        let mut st = self.state.lock();
        st.list = list;
        st.dirty = false;
    }
}

/// 开始跟踪显示器配置变化（`create` 调用；不阻塞）。显示器配置的通知在主线程上发布，回调里直接按 NSScreen 重取；
/// Dock 偏好变化只标记待刷新。另外在主线程上异步预取一次，让第一次显示面板 / 浮标时就有精确的工作区
pub(crate) fn start_tracking(ctx: &Arc<Ctx>) {
    let weak: Weak<Ctx> = Arc::downgrade(ctx);
    let block = RcBlock::new(move |_n: NonNull<NSNotification>| {
        guarded("screen parameters observer", || {
            let Some(ctx) = weak.upgrade() else { return };
            match MainThreadMarker::new() {
                Some(mtm) => ctx.monitors.store(monitors_main(mtm)),
                None => ctx.monitors.store(Vec::new()),
            }
            log::debug!("display configuration changed");
        });
    });
    // SAFETY: 系统导出的通知名常量
    let name = unsafe { NSApplicationDidChangeScreenParametersNotification };
    *ctx.monitors.screen_observer.lock() = Some(WorkspaceObserver::on(
        NSNotificationCenter::defaultCenter(),
        name,
        &block,
    ));
    let weak: Weak<Ctx> = Arc::downgrade(ctx);
    *ctx.monitors.dock_observer.lock() = DistributedObserver::new(
        ctx.main.clone(),
        &[DOCK_PREFS_CHANGED],
        Arc::new(move || {
            if let Some(ctx) = weak.upgrade() {
                ctx.monitors.state.lock().dirty = true;
            }
        }),
    );
    let weak: Weak<Ctx> = Arc::downgrade(ctx);
    ctx.main.run_async(move |mtm| {
        if let Some(ctx) = weak.upgrade() {
            ctx.monitors.store(monitors_main(mtm));
        }
    });
}

/// 两个矩形是否相同（容忍半个点的浮点误差）
fn same_rect(a: Rect, b: CGRect) -> bool {
    (a.x - b.origin.x).abs() < 0.5
        && (a.y - b.origin.y).abs() < 0.5
        && (a.width - b.size.width).abs() < 0.5
        && (a.height - b.size.height).abs() < 0.5
}

/// 缓存里的某个显示器与当前的 CG 几何是否一致
fn still_valid(m: &MonitorInfo, display: CGDirectDisplayID) -> bool {
    m.id == monitor_id(display)
        && same_rect(m.frame, CGDisplayBounds(display))
        && m.is_primary == CGDisplayIsMain(display)
}

/// 缓存是否与当前的显示器集合及几何完全一致
fn cache_matches(cached: &[MonitorInfo], ids: &[CGDirectDisplayID]) -> bool {
    !cached.is_empty() && cached.len() == ids.len() && ids.iter().all(|d| cached.iter().any(|m| still_valid(m, *d)))
}

/// 显示器列表：缓存有效（未标记待刷新，且与当前的 CG 几何一致）就直接返回，不等主线程；否则在主线程上经 NSScreen
/// 重取（≤ 150 ms），主线程不可用时用纯 CG 的近似值（工作区按整屏）
pub(crate) fn monitors(ctx: &Ctx) -> Vec<MonitorInfo> {
    let ids = active_displays();
    {
        let st = ctx.monitors.state.lock();
        if !st.dirty && cache_matches(&st.list, &ids) {
            return st.list.clone();
        }
    }
    if let Some(list) = ctx.main.run_sync(MAIN_WAIT, monitors_main).filter(|l| !l.is_empty()) {
        ctx.monitors.store(list.clone());
        return list;
    }
    {
        // 主线程忙：待刷新的缓存只在几何仍一致时才用（工作区可能差几个点，仍好过整屏近似）
        let st = ctx.monitors.state.lock();
        if cache_matches(&st.list, &ids) {
            return st.list.clone();
        }
    }
    ids.into_iter().map(monitor_cg).collect()
}

/// 主线程：NSScreen 提供工作区（扣除菜单栏与 Dock）、本地化名称与缩放
pub(crate) fn monitors_main(mtm: MainThreadMarker) -> Vec<MonitorInfo> {
    let primary_h = primary_height();
    let mut list: Vec<MonitorInfo> = NSScreen::screens(mtm)
        .iter()
        .filter_map(|screen| {
            let id = screen_display_id(&screen)?;
            let bounds = CGDisplayBounds(id);
            let scale = screen.backingScaleFactor();
            let vf = screen.visibleFrame();
            let vf_top = primary_h - (vf.origin.y + vf.size.height);
            Some(MonitorInfo {
                id: monitor_id(id),
                name: screen.localizedName().to_string(),
                physical_origin: phys_point(bounds.origin, scale),
                physical_size: phys_size(bounds, scale),
                scale_factor: scale,
                frame: cg_to_rect(bounds),
                work_area: Rect::new(
                    vf.origin.x - bounds.origin.x,
                    vf_top - bounds.origin.y,
                    vf.size.width,
                    vf.size.height,
                ),
                is_primary: CGDisplayIsMain(id),
            })
        })
        .collect();
    list.sort_by_key(|m| !m.is_primary);
    list
}

/// NSScreen 对应的 CGDirectDisplayID（`deviceDescription["NSScreenNumber"]`）
pub(crate) fn screen_display_id(screen: &NSScreen) -> Option<CGDirectDisplayID> {
    let desc = screen.deviceDescription();
    let obj = desc.objectForKey(ns_string!("NSScreenNumber"))?;
    let num: Retained<NSNumber> = obj.downcast().ok()?;
    Some(num.unsignedIntValue())
}

/// 纯 CG 的近似（非主线程且主线程不可用时）：工作区按整屏处理
fn monitor_cg(id: CGDirectDisplayID) -> MonitorInfo {
    let bounds = CGDisplayBounds(id);
    let mode = CGDisplayCopyDisplayMode(id);
    let pixel_w = CGDisplayMode::pixel_width(mode.as_deref()) as f64;
    let scale = if bounds.size.width > 0.0 && pixel_w > 0.0 {
        (pixel_w / bounds.size.width).max(1.0)
    } else {
        1.0
    };
    MonitorInfo {
        id: monitor_id(id),
        name: format!("Display {id}"),
        physical_origin: phys_point(bounds.origin, scale),
        physical_size: phys_size(bounds, scale),
        scale_factor: scale,
        frame: cg_to_rect(bounds),
        work_area: Rect::new(0.0, 0.0, bounds.size.width, bounds.size.height),
        is_primary: CGDisplayIsMain(id),
    }
}

/// 某显示器的工作区（显示器内逻辑坐标）：缓存的几何与当前一致时用缓存，否则用整屏
#[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
pub(crate) fn work_area_of(ctx: &Ctx, monitor: &str) -> Option<Rect> {
    let d = display_of(monitor)?;
    let b = CGDisplayBounds(d);
    if b.size.width <= 0.0 {
        return None;
    }
    let cached = ctx
        .monitors
        .state
        .lock()
        .list
        .iter()
        .find(|m| still_valid(m, d))
        .map(|m| m.work_area);
    Some(cached.unwrap_or_else(|| Rect::new(0.0, 0.0, b.size.width, b.size.height)))
}

fn cg_to_rect(r: CGRect) -> Rect {
    Rect::new(r.origin.x, r.origin.y, r.size.width, r.size.height)
}

fn phys_point(p: CGPoint, scale: f64) -> PhysPoint {
    PhysPoint {
        x: (p.x * scale).round() as i32,
        y: (p.y * scale).round() as i32,
    }
}

fn phys_size(r: CGRect, scale: f64) -> PhysSize {
    PhysSize {
        width: (r.size.width * scale).round().max(0.0) as u32,
        height: (r.size.height * scale).round().max(0.0) as u32,
    }
}
