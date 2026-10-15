//! 浮标窗口：原生 NSPanel + CALayer，不用 WebView。
//!
//! - 窗口：无边框、透明、非激活面板（点击不激活 Wisp，源应用的焦点与选区保持不变），浮在弹出菜单层级，加入所有桌面与
//!   全屏空间，不进 Dock / ⌘Tab / 窗口菜单；Exposé 行为只设 Transient（调度中心里隐藏）；约 34×34 pt；
//! - 点击范围：系统只把落在**非透明像素**上的点击交给这个窗口，其余位置的点击直接穿透到下面的应用。窗口里画了像素的
//!   只有圆盘与它的阴影外圈，因此视图把凡是送到它这里的点击都当作浮标点击（[`gesture::bobber_hit`]），阴影外圈上的
//!   点击不会被吞掉；钩子判断「在别处按下」用同一个范围；
//! - 外观：中性底色圆盘 + 强调色的 SF Symbol；悬停 80 ms 过渡为强调色圆盘 + 白色图标（只做高亮，不发请求）；
//! - 动画：出现 scale 0.6→1 + 淡入 140 ms（减速曲线），消失淡出 140 ms；减少动效时只淡入淡出 100 ms；
//! - 消失：点击（立即，不报原因）、4 s 无交互（悬停时暂停计时）、光标离开中心 > 80 px、切换前台应用由这里判定；
//!   按键 / 滚轮 / 外部按下由钩子判定（hooks.rs；按键 tap 只在浮标可见期间启用，由这里随可见状态开关）。
//!   距离与计时只在可见期间以 30 Hz 运行，隐藏后没有任何定时器；
//! - 生命周期：第一次显示时才创建窗口，钩子注销（浮标功能关闭）时销毁，隐藏状态零开销。
//!
//! 所有 AppKit 操作都在主线程上执行；跨线程共享的只有可见状态、圆形矩形、消失回报与按键 tap（`BobberShared`）。
//! ObjC 类名由 objc2 按「模块路径 + 类名 + crate 版本」生成（例如 `wisp_platform::macos::bobber::BobberView0.1.0`），
//! 天然带项目前缀，不会与其他库同名（见单测）。

use std::cell::{Cell, RefCell};
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSColorSpace, NSEvent, NSFontWeightSemibold, NSImage, NSImageSymbolConfiguration,
    NSPanel, NSPopUpMenuWindowLevel, NSScreen, NSTrackingArea, NSTrackingAreaOptions, NSView,
    NSWindowAnimationBehavior, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColor, CGDisplayBounds};
use objc2_foundation::{
    NSArray, NSNumber, NSPoint, NSRect, NSRunLoop, NSRunLoopCommonModes, NSString, NSTimer, ns_string,
};
use objc2_quartz_core::{
    CABasicAnimation, CALayer, CAMediaTiming, CAMediaTimingFunction, CATransaction, kCAGravityResizeAspect,
};
use parking_lot::Mutex;
use wisp_types::{MonitorId, Point, Rect};

use super::Ctx;
use super::displays;
use super::hooks::KeyTap;
use super::util::{MainOnly, guarded};
use crate::common::gesture::{
    self, BOBBER_APPEAR, BOBBER_APPEAR_SCALE, BOBBER_DIAMETER, BOBBER_DISAPPEAR, BOBBER_DISTANCE_POLL, BOBBER_HOVER,
    BOBBER_IDLE_TIMEOUT, BOBBER_REDUCED_FADE, BOBBER_WINDOW,
};
use crate::traits::BobberWindow;
use crate::types::DismissReason;

/// 消失原因的回报（钩子安装时登记，转成 `InputSignal::BobberDismiss`）
pub(crate) type Reporter = Arc<dyn Fn(DismissReason) + Send + Sync>;
type OnClick = Arc<dyn Fn() + Send + Sync>;

/// 浮标图标（SF Symbols；前者不可用时用后者）
const SYMBOLS: [&str; 2] = ["translate", "character.bubble"];
/// 图标字号（pt）
const GLYPH_POINT_SIZE: f64 = 12.5;

/// 跨线程共享的浮标状态
#[derive(Default)]
pub(crate) struct BobberShared {
    visible: AtomicBool,
    /// 可见时圆形的位置（显示器内逻辑坐标）
    rect: Mutex<Option<(MonitorId, Rect)>>,
    reporter: Mutex<Option<(u64, Reporter)>>,
    ui: Mutex<Option<MainOnly<Ui>>>,
    /// 每次 show / hide 递增；主线程上的过期操作据此作废
    seq: AtomicU64,
    /// 负责浮标的钩子的按键 tap（浮标可见时启用，其余时间停用）
    key_tap: Mutex<Option<(u64, KeyTap)>>,
}

impl BobberShared {
    pub(crate) fn is_visible(&self) -> bool {
        self.visible.load(Ordering::Acquire)
    }

    /// 某点是否落在可见浮标的点击范围内（圆盘与阴影外圈，与视图的 hitTest 一致）
    pub(crate) fn contains(&self, monitor: &str, p: Point) -> bool {
        self.is_visible()
            && self
                .rect
                .lock()
                .as_ref()
                .is_some_and(|(m, r)| m == monitor && gesture::bobber_hit(*r, p))
    }

    /// 登记负责浮标的钩子的按键 tap（替换旧的，旧的随之停用）
    pub(crate) fn set_key_tap(&self, id: u64, tap: Option<KeyTap>) {
        let mut slot = self.key_tap.lock();
        if let Some((_, old)) = slot.take() {
            old.set_enabled(false);
        }
        if let Some(tap) = tap {
            tap.set_enabled(self.is_visible());
            *slot = Some((id, tap));
        }
    }

    /// 注销按键 tap（只注销 `id` 自己登记的那个）
    pub(crate) fn clear_key_tap(&self, id: u64) {
        let mut slot = self.key_tap.lock();
        if slot.as_ref().is_some_and(|(cur, _)| *cur == id)
            && let Some((_, tap)) = slot.take()
        {
            tap.set_enabled(false);
        }
    }

    /// 让按键 tap 的启用状态跟上当前的可见状态。每次改变可见状态之后调用；在锁内读取可见状态，
    /// 并发的显示 / 隐藏无论先后，最后一次调用都按最新状态设置
    fn sync_keys(&self) {
        let slot = self.key_tap.lock();
        if let Some((_, tap)) = slot.as_ref() {
            tap.set_enabled(self.is_visible());
        }
    }

    pub(crate) fn set_reporter(&self, id: u64, reporter: Reporter) {
        *self.reporter.lock() = Some((id, reporter));
    }

    /// 注销回报；返回被注销的是否就是 `id`（同一时刻只有最近一次安装的钩子负责浮标）
    pub(crate) fn clear_reporter(&self, id: u64) -> bool {
        let mut r = self.reporter.lock();
        if r.as_ref().is_some_and(|(cur, _)| *cur == id) {
            *r = None;
            true
        } else {
            false
        }
    }

    fn reporter(&self) -> Option<Reporter> {
        self.reporter.lock().as_ref().map(|(_, r)| r.clone())
    }
}

impl Drop for BobberShared {
    fn drop(&mut self) {
        // 窗口对象只能在主线程释放；不在主线程时宁可泄漏（只发生在进程退出阶段）
        if let Some(ui) = self.ui.get_mut().take() {
            match MainThreadMarker::new() {
                Some(mtm) => ui.into_inner(mtm).destroy(),
                None => std::mem::forget(ui),
            }
        }
    }
}

/// 让浮标消失并回报原因（任何线程）
pub(crate) fn dismiss(ctx: &Arc<Ctx>, reason: DismissReason) {
    if !ctx.bobber.visible.swap(false, Ordering::AcqRel) {
        return;
    }
    *ctx.bobber.rect.lock() = None;
    ctx.bobber.sync_keys();
    let seq = ctx.bobber.seq.fetch_add(1, Ordering::AcqRel) + 1;
    if let Some(report) = ctx.bobber.reporter() {
        report(reason);
    }
    log::debug!("bobber dismissed: {reason:?}");
    let ctx2 = ctx.clone();
    ctx.main.run_async(move |mtm| {
        with_ui(&ctx2, mtm, |ui| ui.fade_out(&ctx2, seq, true));
    });
}

/// 前台应用切换（focus.rs 的激活通知调用）
pub(crate) fn on_app_activated(ctx: &Arc<Ctx>, pid: i32) {
    if pid != ctx.own_pid && ctx.bobber.is_visible() {
        dismiss(ctx, DismissReason::AppSwitched);
    }
}

/// 浮标功能关闭：销毁窗口
pub(crate) fn teardown(ctx: &Arc<Ctx>) {
    ctx.bobber.visible.store(false, Ordering::Release);
    *ctx.bobber.rect.lock() = None;
    ctx.bobber.sync_keys();
    ctx.bobber.seq.fetch_add(1, Ordering::AcqRel);
    let ctx2 = ctx.clone();
    ctx.main.run_async(move |mtm| {
        let ui = ctx2.bobber.ui.lock().take();
        if let Some(ui) = ui {
            ui.into_inner(mtm).destroy();
        }
    });
}

fn with_ui<R>(ctx: &Arc<Ctx>, mtm: MainThreadMarker, f: impl FnOnce(&Ui) -> R) -> Option<R> {
    let guard = ctx.bobber.ui.lock();
    guard.as_ref().map(|ui| f(ui.get(mtm)))
}

/// 视图的实例变量（只在主线程访问）
struct ViewIvars {
    ctx: Weak<Ctx>,
    on_click: RefCell<Option<OnClick>>,
    hovered: Cell<bool>,
    /// 圆形在视图坐标中的矩形
    circle: Cell<NSRect>,
    layers: RefCell<Option<Layers>>,
    /// 已应用的外观（深色, 强调色）
    applied: RefCell<Option<(bool, String)>>,
}

/// 视图持有的图层
struct Layers {
    /// 整体缩放 / 淡入淡出的容器
    content: Retained<CALayer>,
    disc: Retained<CALayer>,
    glyph: Retained<CALayer>,
    glyph_hover: Retained<CALayer>,
    disc_normal: Retained<CGColor>,
    disc_hover: Retained<CGColor>,
}

define_class!(
    // SAFETY: NSView 没有特殊的子类化要求；本类不实现 Drop
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[ivars = ViewIvars]
    struct BobberView;

    impl BobberView {
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseDownCanMoveWindow))]
        fn mouse_down_can_move_window(&self) -> bool {
            false
        }

        #[unsafe(method_id(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> Option<Retained<NSView>> {
            // 系统只把非透明像素（圆盘与阴影外圈）上的点击交给本窗口：这些点击都算浮标点击，不能返回 nil 把它吞掉
            guarded("bobber hitTest", || {
                // SAFETY: 只读取父视图引用
                let superview = unsafe { self.superview() };
                let local = self.convertPoint_fromView(point, superview.as_deref());
                let c = self.ivars().circle.get();
                let circle = Rect::new(c.origin.x, c.origin.y, c.size.width, c.size.height);
                let inside = gesture::bobber_hit(circle, Point { x: local.x, y: local.y });
                inside.then(|| Retained::into_super(self.retain()))
            })
            .flatten()
        }

        // 以下三个方法由 AppKit 的事件派发调用：panic 一律在这里截住
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            guarded("bobber mouseDown", || self.clicked());
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            guarded("bobber mouseEntered", || self.set_hover(true));
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            guarded("bobber mouseExited", || self.set_hover(false));
        }
    }
);

impl BobberView {
    fn create(ctx: &Arc<Ctx>, mtm: MainThreadMarker) -> Retained<Self> {
        let side = BOBBER_WINDOW;
        let pad = (BOBBER_WINDOW - BOBBER_DIAMETER) / 2.0;
        let this = Self::alloc(mtm).set_ivars(ViewIvars {
            ctx: Arc::downgrade(ctx),
            on_click: RefCell::new(None),
            hovered: Cell::new(false),
            circle: Cell::new(NSRect::new(
                CGPoint::new(pad, pad),
                CGSize::new(BOBBER_DIAMETER, BOBBER_DIAMETER),
            )),
            layers: RefCell::new(None),
            applied: RefCell::new(None),
        });
        let frame = NSRect::new(CGPoint::new(0.0, 0.0), CGSize::new(side, side));
        // SAFETY: NSView 的指定初始化方法
        let view: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        view.setWantsLayer(true);
        view.build_layers();
        let options = NSTrackingAreaOptions::MouseEnteredAndExited | NSTrackingAreaOptions::ActiveAlways;
        // SAFETY: owner 是本视图（追踪区域不持有 owner）；userInfo 为空
        let area = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                view.ivars().circle.get(),
                options,
                Some(&view),
                None,
            )
        };
        view.addTrackingArea(&area);
        view
    }

    fn build_layers(&self) {
        let Some(root) = self.layer() else { return };
        let c = self.ivars().circle.get();
        let content = CALayer::layer();
        content.setBounds(CGRect::new(CGPoint::new(0.0, 0.0), c.size));
        content.setPosition(CGPoint::new(
            c.origin.x + c.size.width / 2.0,
            c.origin.y + c.size.height / 2.0,
        ));
        content.setAnchorPoint(CGPoint::new(0.5, 0.5));
        let disc = CALayer::layer();
        disc.setFrame(CGRect::new(CGPoint::new(0.0, 0.0), c.size));
        disc.setCornerRadius(c.size.width / 2.0);
        disc.setBorderWidth(0.5);
        // 阴影外圈 ≈ 半径 + 偏移 ≈ 4 pt，落在 [`gesture::BOBBER_HIT_RADIUS`] 之内（点击范围覆盖全部可见像素）
        disc.setShadowOffset(CGSize::new(0.0, -1.0));
        disc.setShadowRadius(3.0);
        content.addSublayer(&disc);
        let inset = 4.5;
        let glyph_frame = CGRect::new(
            CGPoint::new(inset, inset),
            CGSize::new(c.size.width - inset * 2.0, c.size.height - inset * 2.0),
        );
        let glyph = CALayer::layer();
        let glyph_hover = CALayer::layer();
        // SAFETY: 系统导出的 gravity 常量
        let gravity = unsafe { kCAGravityResizeAspect };
        for g in [&glyph, &glyph_hover] {
            g.setFrame(glyph_frame);
            g.setContentsGravity(gravity);
            content.addSublayer(g);
        }
        glyph_hover.setOpacity(0.0);
        content.setOpacity(0.0);
        root.addSublayer(&content);
        let clear = NSColor::clearColor().CGColor();
        *self.ivars().layers.borrow_mut() = Some(Layers {
            content,
            disc,
            glyph,
            glyph_hover,
            disc_normal: clear.clone(),
            disc_hover: clear,
        });
    }

    /// 应用深浅色与强调色（与上次相同则跳过）
    fn apply_appearance(&self, dark: bool, scale: f64) {
        let accent = NSColor::controlAccentColor();
        let accent_srgb = accent
            .colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())
            .unwrap_or_else(|| accent.clone());
        let key = (
            dark,
            format!(
                "{:.3},{:.3},{:.3}",
                accent_srgb.redComponent(),
                accent_srgb.greenComponent(),
                accent_srgb.blueComponent()
            ),
        );
        let mut layers = self.ivars().layers.borrow_mut();
        let Some(l) = layers.as_mut() else { return };
        for g in [&l.glyph, &l.glyph_hover] {
            g.setContentsScale(scale);
        }
        if self.ivars().applied.borrow().as_ref() == Some(&key) {
            return;
        }
        let srgb = |r: f64, g: f64, b: f64, a: f64| NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, a).CGColor();
        let (normal, border, shadow) = if dark {
            (srgb(0.17, 0.17, 0.18, 0.94), srgb(1.0, 1.0, 1.0, 0.14), 0.45)
        } else {
            (srgb(1.0, 1.0, 1.0, 0.97), srgb(0.0, 0.0, 0.0, 0.10), 0.20)
        };
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        l.disc_normal = normal;
        l.disc_hover = accent_srgb.CGColor();
        let hovered = self.ivars().hovered.get();
        l.disc
            .setBackgroundColor(Some(if hovered { &l.disc_hover } else { &l.disc_normal }));
        l.disc.setBorderColor(Some(&border));
        l.disc.setShadowColor(Some(&NSColor::blackColor().CGColor()));
        l.disc.setShadowOpacity(shadow);
        set_contents(&l.glyph, symbol_image(&accent_srgb).as_deref());
        set_contents(&l.glyph_hover, symbol_image(&NSColor::whiteColor()).as_deref());
        CATransaction::commit();
        *self.ivars().applied.borrow_mut() = Some(key);
    }

    /// 悬停高亮（80 ms 过渡）
    fn set_hover(&self, hovered: bool) {
        if self.ivars().hovered.replace(hovered) == hovered {
            return;
        }
        let layers = self.ivars().layers.borrow();
        let Some(l) = layers.as_ref() else { return };
        CATransaction::begin();
        CATransaction::setAnimationDuration(BOBBER_HOVER.as_secs_f64());
        l.disc
            .setBackgroundColor(Some(if hovered { &l.disc_hover } else { &l.disc_normal }));
        l.glyph.setOpacity(if hovered { 0.0 } else { 1.0 });
        l.glyph_hover.setOpacity(if hovered { 1.0 } else { 0.0 });
        CATransaction::commit();
    }

    /// 点击：立即消失并触发回调（不报告消失原因）
    fn clicked(&self) {
        let cb = self.ivars().on_click.borrow_mut().take();
        let Some(ctx) = self.ivars().ctx.upgrade() else { return };
        if ctx.bobber.visible.swap(false, Ordering::AcqRel) {
            *ctx.bobber.rect.lock() = None;
            ctx.bobber.sync_keys();
            let seq = ctx.bobber.seq.fetch_add(1, Ordering::AcqRel) + 1;
            if let Some(mtm) = MainThreadMarker::new() {
                with_ui(&ctx, mtm, |ui| ui.fade_out(&ctx, seq, false));
            }
        }
        if let Some(cb) = cb {
            guarded("bobber click callback", || cb());
        }
    }

    /// 出现动画
    fn animate_in(&self, reduced_motion: bool) {
        let layers = self.ivars().layers.borrow();
        let Some(l) = layers.as_ref() else { return };
        l.content.removeAllAnimations();
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        l.content.setOpacity(1.0);
        l.glyph.setOpacity(1.0);
        l.glyph_hover.setOpacity(0.0);
        l.disc.setBackgroundColor(Some(&l.disc_normal));
        CATransaction::commit();
        let duration = if reduced_motion {
            BOBBER_REDUCED_FADE
        } else {
            BOBBER_APPEAR
        };
        let fade = basic_animation(ns_string!("opacity"), 0.0, 1.0, duration, ease_out());
        l.content.addAnimation_forKey(&fade, Some(ns_string!("wisp.fade")));
        if !reduced_motion {
            // 先快后慢的减速曲线，末端不过冲
            let curve = CAMediaTimingFunction::functionWithControlPoints(0.2, 0.9, 0.25, 1.0);
            let scale = basic_animation(ns_string!("transform.scale"), BOBBER_APPEAR_SCALE, 1.0, duration, curve);
            l.content.addAnimation_forKey(&scale, Some(ns_string!("wisp.scale")));
        }
    }
}

fn ease_out() -> Retained<CAMediaTimingFunction> {
    CAMediaTimingFunction::functionWithControlPoints(0.0, 0.0, 0.58, 1.0)
}

fn basic_animation(
    key_path: &NSString,
    from: f64,
    to: f64,
    duration: Duration,
    curve: Retained<CAMediaTimingFunction>,
) -> Retained<CABasicAnimation> {
    let anim = CABasicAnimation::animationWithKeyPath(Some(key_path));
    let (from, to) = (NSNumber::new_f64(from), NSNumber::new_f64(to));
    // SAFETY: opacity / transform.scale 接受 NSNumber
    unsafe {
        anim.setFromValue(Some(&from));
        anim.setToValue(Some(&to));
    }
    anim.setDuration(duration.as_secs_f64());
    anim.setTimingFunction(Some(&curve));
    anim
}

fn set_contents(layer: &CALayer, image: Option<&NSImage>) {
    let obj: Option<&AnyObject> = image.map(|i| i.as_ref());
    // SAFETY: CALayer 在 macOS 上接受 NSImage 作为 contents
    unsafe { layer.setContents(obj) };
}

/// 指定颜色的 SF Symbol 图标
fn symbol_image(color: &NSColor) -> Option<Retained<NSImage>> {
    // SAFETY: 系统导出的字重常量
    let weight = unsafe { NSFontWeightSemibold };
    let size = NSImageSymbolConfiguration::configurationWithPointSize_weight(GLYPH_POINT_SIZE, weight);
    let palette = NSImageSymbolConfiguration::configurationWithPaletteColors(&NSArray::from_slice(&[color]));
    let config = size.configurationByApplyingConfiguration(&palette);
    SYMBOLS.iter().find_map(|name| {
        NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(name), None)
            .and_then(|img| img.imageWithSymbolConfiguration(&config))
    })
}

/// 主线程上的窗口与定时器
pub(crate) struct Ui {
    panel: Retained<NSPanel>,
    view: Retained<BobberView>,
    timer: RefCell<Option<Retained<NSTimer>>>,
    /// 本次显示是否要求减少动效（消失动画沿用）
    reduced_motion: Cell<bool>,
}

impl Ui {
    fn new(ctx: &Arc<Ctx>, mtm: MainThreadMarker) -> Self {
        let rect = NSRect::new(CGPoint::new(0.0, 0.0), CGSize::new(BOBBER_WINDOW, BOBBER_WINDOW));
        let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            rect,
            style,
            NSBackingStoreType::Buffered,
            false,
        );
        // SAFETY: 由 Retained 管理生命周期，关闭时不应再自行释放
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(false);
        panel.setLevel(NSPopUpMenuWindowLevel);
        // Exposé 行为（managed / transient / stationary）三选一：选 Transient，调度中心与台前调度里不显示浮标
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::IgnoresCycle
                | NSWindowCollectionBehavior::Transient,
        );
        panel.setHidesOnDeactivate(false);
        panel.setFloatingPanel(true);
        panel.setBecomesKeyOnlyIfNeeded(true);
        panel.setAnimationBehavior(NSWindowAnimationBehavior::None);
        panel.setExcludedFromWindowsMenu(true);
        let view = BobberView::create(ctx, mtm);
        panel.setContentView(Some(&view));
        Self {
            panel,
            view,
            timer: RefCell::new(None),
            reduced_motion: Cell::new(false),
        }
    }

    fn stop_timer(&self) {
        if let Some(t) = self.timer.borrow_mut().take() {
            t.invalidate();
        }
    }

    /// 显示在 `circle`（Quartz 全局坐标）处
    #[allow(clippy::too_many_arguments)]
    fn present(&self, ctx: &Arc<Ctx>, circle: CGRect, dark: bool, reduced_motion: bool, on_click: OnClick, scale: f64) {
        self.stop_timer();
        self.reduced_motion.set(reduced_motion);
        *self.view.ivars().on_click.borrow_mut() = Some(on_click);
        self.view.ivars().hovered.set(false);
        self.view.apply_appearance(dark, scale);
        let window = gesture::bobber_window_rect(Rect::new(
            circle.origin.x,
            circle.origin.y,
            circle.size.width,
            circle.size.height,
        ));
        let frame = displays::quartz_rect_to_cocoa(CGRect::new(
            CGPoint::new(window.x, window.y),
            CGSize::new(window.width, window.height),
        ));
        self.panel.setFrame_display(frame, false);
        self.panel.setAlphaValue(1.0);
        self.view.animate_in(reduced_motion);
        self.panel.orderFrontRegardless();
        self.start_timer(ctx, circle);
    }

    /// 可见期间的 30 Hz 定时器：无交互计时（悬停时暂停）与离开距离判定
    fn start_timer(&self, ctx: &Arc<Ctx>, circle: CGRect) {
        let weak = Arc::downgrade(ctx);
        let view = self.view.retain();
        let idle = Rc::new(Cell::new(Duration::ZERO));
        let last = Rc::new(Cell::new(Instant::now()));
        let circle = Rect::new(circle.origin.x, circle.origin.y, circle.size.width, circle.size.height);
        let block = RcBlock::new(move |_t: NonNull<NSTimer>| {
            // 定时器块由 AppKit 的 run loop 调用，消失时会同步调用上层的 sink：panic 不能展开穿过 AppKit
            guarded("bobber timer", || {
                let Some(ctx) = weak.upgrade() else { return };
                let now = Instant::now();
                let dt = now.saturating_duration_since(last.replace(now));
                if view.ivars().hovered.get() {
                    return;
                }
                idle.set(idle.get() + dt);
                if idle.get() >= BOBBER_IDLE_TIMEOUT {
                    dismiss(&ctx, DismissReason::Timeout);
                    return;
                }
                let cursor = displays::cocoa_to_quartz(NSEvent::mouseLocation());
                if gesture::moved_away(
                    circle,
                    Point {
                        x: cursor.x,
                        y: cursor.y,
                    },
                ) {
                    dismiss(&ctx, DismissReason::MovedAway);
                }
            });
        });
        // SAFETY: 块是 'static 的；定时器在隐藏 / 销毁时 invalidate
        let timer =
            unsafe { NSTimer::timerWithTimeInterval_repeats_block(BOBBER_DISTANCE_POLL.as_secs_f64(), true, &block) };
        timer.setTolerance(0.005);
        // SAFETY: 主线程的 run loop 与系统导出的模式常量；加入 common 模式，菜单追踪等期间也能运行
        unsafe { NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
        *self.timer.borrow_mut() = Some(timer);
    }

    /// 消失：动画淡出后 orderOut；`seq` 用于丢弃被更新的 show 取代的完成回调
    fn fade_out(&self, ctx: &Arc<Ctx>, seq: u64, animated: bool) {
        self.stop_timer();
        *self.view.ivars().on_click.borrow_mut() = None;
        let layers = self.view.ivars().layers.borrow();
        let Some(l) = layers.as_ref() else { return };
        if !animated {
            l.content.removeAllAnimations();
            CATransaction::begin();
            CATransaction::setDisableActions(true);
            l.content.setOpacity(0.0);
            CATransaction::commit();
            self.panel.orderOut(None);
            return;
        }
        let duration = if self.reduced_motion.get() {
            BOBBER_REDUCED_FADE
        } else {
            BOBBER_DISAPPEAR
        };
        let weak = Arc::downgrade(ctx);
        let panel = self.panel.retain();
        let done = RcBlock::new(move || {
            guarded("bobber fade-out completion", || {
                let current = weak.upgrade().map(|c| c.bobber.seq.load(Ordering::Acquire));
                if current == Some(seq) || current.is_none() {
                    panel.orderOut(None);
                }
            });
        });
        CATransaction::begin();
        CATransaction::setAnimationDuration(duration.as_secs_f64());
        CATransaction::setAnimationTimingFunction(Some(&CAMediaTimingFunction::functionWithControlPoints(
            0.42, 0.0, 1.0, 1.0,
        )));
        // SAFETY: 块是 'static 的，在动画结束后于主线程调用一次
        unsafe { CATransaction::setCompletionBlock(Some(&done)) };
        l.content.setOpacity(0.0);
        CATransaction::commit();
    }

    fn destroy(self) {
        self.stop_timer();
        self.panel.orderOut(None);
        self.panel.close();
    }
}

/// 主线程：某显示器的工作区（显示器内逻辑坐标）与缩放；查不到时用整屏与 2x
fn screen_metrics(mtm: MainThreadMarker, monitor: &str) -> Option<(Rect, CGRect, f64)> {
    let display = displays::display_of(monitor)?;
    let bounds = CGDisplayBounds(display);
    if bounds.size.width <= 0.0 {
        return None;
    }
    let primary_h = displays::primary_height();
    let screen = NSScreen::screens(mtm)
        .iter()
        .find(|s| displays::screen_display_id(s) == Some(display));
    let (work, scale) = match screen {
        Some(s) => {
            let vf = s.visibleFrame();
            let top = primary_h - (vf.origin.y + vf.size.height);
            (
                Rect::new(
                    vf.origin.x - bounds.origin.x,
                    top - bounds.origin.y,
                    vf.size.width,
                    vf.size.height,
                ),
                s.backingScaleFactor(),
            )
        }
        None => (Rect::new(0.0, 0.0, bounds.size.width, bounds.size.height), 2.0),
    };
    Some((work, bounds, scale))
}

/// BobberWindow 实现
pub(crate) struct MacBobber {
    ctx: Arc<Ctx>,
}

impl MacBobber {
    pub(crate) fn new(ctx: Arc<Ctx>) -> Self {
        Self { ctx }
    }
}

impl BobberWindow for MacBobber {
    fn show(&self, at: (MonitorId, Point), dark: bool, reduced_motion: bool, on_click: Box<dyn Fn() + Send + Sync>) {
        let (monitor, point) = at;
        // 先按缓存的工作区给出近似位置（rect() 立即可用），主线程上再按 NSScreen 精确计算
        let approx_area = displays::work_area_of(&self.ctx, &monitor);
        if let Some(area) = approx_area {
            *self.ctx.bobber.rect.lock() = Some((monitor.clone(), gesture::bobber_rect(point, area)));
        }
        self.ctx.bobber.visible.store(true, Ordering::Release);
        self.ctx.bobber.sync_keys();
        let seq = self.ctx.bobber.seq.fetch_add(1, Ordering::AcqRel) + 1;
        let on_click: OnClick = Arc::from(on_click);
        let ctx = self.ctx.clone();
        self.ctx.main.run_async(move |mtm| {
            if ctx.bobber.seq.load(Ordering::Acquire) != seq {
                return;
            }
            let Some((work, bounds, scale)) = screen_metrics(mtm, &monitor) else {
                log::debug!("bobber: unknown monitor {monitor}");
                ctx.bobber.visible.store(false, Ordering::Release);
                *ctx.bobber.rect.lock() = None;
                ctx.bobber.sync_keys();
                return;
            };
            let circle = gesture::bobber_rect(point, work);
            *ctx.bobber.rect.lock() = Some((monitor, circle));
            let global = CGRect::new(
                CGPoint::new(bounds.origin.x + circle.x, bounds.origin.y + circle.y),
                CGSize::new(circle.width, circle.height),
            );
            let mut guard = ctx.bobber.ui.lock();
            let ui = guard.get_or_insert_with(|| MainOnly::new(Ui::new(&ctx, mtm), mtm));
            ui.get(mtm).present(&ctx, global, dark, reduced_motion, on_click, scale);
        });
    }

    fn hide(&self, animated: bool) {
        if !self.ctx.bobber.visible.swap(false, Ordering::AcqRel) {
            return;
        }
        *self.ctx.bobber.rect.lock() = None;
        self.ctx.bobber.sync_keys();
        let seq = self.ctx.bobber.seq.fetch_add(1, Ordering::AcqRel) + 1;
        let ctx = self.ctx.clone();
        self.ctx.main.run_async(move |mtm| {
            with_ui(&ctx, mtm, |ui| ui.fade_out(&ctx, seq, animated));
        });
    }

    fn rect(&self) -> Option<(MonitorId, Rect)> {
        if !self.ctx.bobber.is_visible() {
            return None;
        }
        self.ctx.bobber.rect.lock().clone()
    }
}

#[cfg(test)]
mod tests {
    use objc2::ClassType;

    use super::*;

    #[test]
    fn objc_class_names_are_namespaced() {
        // objc2 默认以「模块路径 + 类名 + crate 版本」注册类：进程内唯一，不会与其他库的同名类冲突
        let name = BobberView::class()
            .name()
            .to_str()
            .expect("utf-8 class name")
            .to_owned();
        assert!(name.starts_with("wisp_platform::macos::bobber::BobberView"), "{name}");
    }
}
