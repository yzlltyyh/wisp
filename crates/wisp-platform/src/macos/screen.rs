//! 显示器、光标与系统外观（CursorScreen）。
//!
//! - 光标位置用 `CGEventCreate(NULL)` 的位置（Quartz 全局坐标，任何线程可用）；
//! - 明暗：主线程上 `NSApp.effectiveAppearance`（仅当应用没有自行覆盖 `NSApp.appearance` 时可信），否则读全局偏好
//!   `AppleInterfaceStyle`（「自动」模式下系统同样会更新这个键）；
//! - 强调色：主线程上 `NSColor.controlAccentColor` 转 sRGB；主线程不可用时按 `AppleAccentColor` 偏好查表；
//! - 外观变化监听全部事件驱动，空闲零唤醒：
//!   - 明暗的主信号是对 `NSApp.effectiveAppearance` 的 KVO：它在 AppKit 更新完有效外观之后才触发，回调里读到的
//!     一定是新值（AppKit 自己也是靠 `AppleInterfaceThemeChangedNotification` 更新外观的，与 Wisp 对同一分布式
//!     通知的观察者之间没有先后保证，直接在通知里读可能读到旧值）；
//!   - 分布式通知（明暗、强调色；应用自行覆盖了 `NSApp.appearance` 时 KVO 不会触发，靠它兜底）+ NSWorkspace 的
//!     辅助功能显示选项通知（降低动效 / 透明度）+ 本地的系统颜色变化通知；
//!   - 所有来源都先合并、再推迟到主队列的下一轮回调一次：同一次切换只回调一次，且届时系统各处都已更新完毕。

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication, NSColor, NSColorSpace,
    NSSystemColorsDidChangeNotification, NSWorkspace, NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification,
};
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::CGEvent;
use objc2_foundation::{
    NSArray, NSKeyValueObservingOptions, NSNotification, NSNotificationCenter, NSObjectNSKeyValueObserverRegistration,
    NSString, NSUserDefaults, ns_string,
};
use parking_lot::Mutex;
use wisp_types::{MonitorId, MonitorInfo, Point};

use super::Ctx;
use super::displays;
use super::focus::WorkspaceObserver;
use super::notify::DistributedObserver;
use super::util::{MainOnly, MainThread, guarded, pool};
use crate::error::PlatformError;
use crate::traits::CursorScreen;
use crate::types::HookHandle;

/// 主线程查询外观的最长等待
const MAIN_WAIT: Duration = Duration::from_millis(100);

/// 外观相关的分布式通知
const APPEARANCE_NOTIFICATIONS: &[&str] = &[
    "AppleInterfaceThemeChangedNotification",
    "AppleColorPreferencesChangedNotification",
    "AppleAquaColorVariantChanged",
];

/// 外观变化的转发：合并同一轮里来自各个来源的通知，推迟到主队列的下一轮回调一次
struct Relay {
    cb: Arc<dyn Fn() + Send + Sync>,
    pending: AtomicBool,
}

impl Relay {
    fn poke(self: &Arc<Self>) {
        if self.pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = self.clone();
        DispatchQueue::main().exec_async(move || {
            me.pending.store(false, Ordering::Release);
            guarded("appearance callback", || (me.cb)());
        });
    }
}

/// KVO 观察者的实例变量
struct KvoIvars {
    relay: Arc<Relay>,
}

define_class!(
    // SAFETY: NSObject 没有特殊的子类化要求；本类不实现 Drop
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = KvoIvars]
    struct AppearanceKvo;

    impl AppearanceKvo {
        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn observe_value(
            &self,
            _key_path: Option<&NSString>,
            _object: Option<&AnyObject>,
            _change: Option<&AnyObject>,
            _context: *mut c_void,
        ) {
            guarded("appearance KVO", || self.ivars().relay.poke());
        }
    }
);

impl AppearanceKvo {
    fn new(mtm: MainThreadMarker, relay: Arc<Relay>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(KvoIvars { relay });
        // SAFETY: NSObject 的指定初始化方法
        unsafe { msg_send![super(this), init] }
    }
}

/// 被观察的键
fn effective_appearance_key() -> &'static NSString {
    ns_string!("effectiveAppearance")
}

/// `NSApp.effectiveAppearance` 的 KVO 登记状态（只在主线程闭包里读写）
#[derive(Default)]
struct KvoState {
    /// 注销已经执行
    cancelled: bool,
    observer: Option<MainOnly<Retained<AppearanceKvo>>>,
}

/// 在主线程上异步登记 KVO（不阻塞调用方）。登记与注销两个闭包的执行顺序没有保证也没关系：注销先执行时置位
/// `cancelled`，登记看到后直接放弃。派发器没有把闭包放到主线程上时（测试）两者都不执行，只靠通知兜底
fn register_kvo(main: &MainThread, relay: Arc<Relay>) -> Arc<Mutex<KvoState>> {
    let state = Arc::new(Mutex::new(KvoState::default()));
    let slot = state.clone();
    main.run_async(move |mtm| {
        let mut st = slot.lock();
        if st.cancelled {
            return;
        }
        let observer = AppearanceKvo::new(mtm, relay);
        let app = NSApplication::sharedApplication(mtm);
        let target: &NSObject = &app;
        // SAFETY: observer 是 NSObject 子类且实现了 observeValueForKeyPath；注销时传同一个对象与键
        unsafe {
            target.addObserver_forKeyPath_options_context(
                &observer,
                effective_appearance_key(),
                NSKeyValueObservingOptions::New,
                std::ptr::null_mut(),
            );
        }
        st.observer = Some(MainOnly::new(observer, mtm));
    });
    state
}

/// 在主线程上注销 KVO 并释放观察者
fn unregister_kvo(main: &MainThread, state: Arc<Mutex<KvoState>>) {
    main.run_async(move |mtm| {
        let mut st = state.lock();
        st.cancelled = true;
        if let Some(observer) = st.observer.take() {
            let observer = observer.into_inner(mtm);
            let app = NSApplication::sharedApplication(mtm);
            let target: &NSObject = &app;
            // SAFETY: 与登记时同一个观察者与键
            unsafe { target.removeObserver_forKeyPath(&observer, effective_appearance_key()) };
        }
    });
}

/// 当前光标位置（Quartz 全局坐标）
pub(crate) fn cursor_global() -> CGPoint {
    CGEvent::new(None)
        .map(|e| CGEvent::location(Some(&e)))
        .unwrap_or(CGPoint::new(0.0, 0.0))
}

/// CursorScreen 实现
pub(crate) struct MacScreen {
    ctx: Arc<Ctx>,
}

impl MacScreen {
    pub(crate) fn new(ctx: Arc<Ctx>) -> Self {
        Self { ctx }
    }
}

fn hex(r: f64, g: f64, b: f64) -> String {
    let c = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02X}{:02X}{:02X}", c(r), c(g), c(b))
}

/// 主线程：强调色
fn accent_main(_mtm: MainThreadMarker) -> Option<String> {
    let color = NSColor::controlAccentColor().colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())?;
    Some(hex(color.redComponent(), color.greenComponent(), color.blueComponent()))
}

/// 偏好里的强调色编号 → 颜色（与系统各强调色在浅色外观下的 controlAccentColor 一致）
fn accent_from_defaults() -> Option<String> {
    let defaults = NSUserDefaults::standardUserDefaults();
    let key = NSString::from_str("AppleAccentColor");
    if defaults.objectForKey(&key).is_none() {
        // 未设置 = 多色（蓝）
        return Some("#007AFF".to_owned());
    }
    let hex = match defaults.integerForKey(&key) {
        -1 => "#8C8C8C",
        0 => "#FF5257",
        1 => "#F7821B",
        2 => "#FFC600",
        3 => "#62BA46",
        4 => "#007AFF",
        5 => "#A550A7",
        6 => "#F74F9E",
        _ => return None,
    };
    Some(hex.to_owned())
}

/// 全局偏好里的明暗
fn dark_from_defaults() -> bool {
    NSUserDefaults::standardUserDefaults()
        .stringForKey(&NSString::from_str("AppleInterfaceStyle"))
        .is_some_and(|s| s.to_string().eq_ignore_ascii_case("dark"))
}

/// 主线程：NSApp 的有效外观（应用自行覆盖了外观时返回 None，交给偏好判断）
fn dark_main(mtm: MainThreadMarker) -> Option<bool> {
    let app = NSApplication::sharedApplication(mtm);
    if app.appearance().is_some() {
        return None;
    }
    // SAFETY: 系统导出的外观名常量
    let (aqua, dark) = unsafe { (NSAppearanceNameAqua, NSAppearanceNameDarkAqua) };
    let names = NSArray::from_slice(&[aqua, dark]);
    let best = app.effectiveAppearance().bestMatchFromAppearancesWithNames(&names)?;
    Some(best.isEqualToString(dark))
}

impl CursorScreen for MacScreen {
    fn monitors(&self) -> Vec<MonitorInfo> {
        pool(|| displays::monitors(&self.ctx))
    }

    fn cursor(&self) -> (MonitorId, Point) {
        displays::locate(cursor_global())
    }

    fn system_accent(&self) -> Option<String> {
        self.ctx
            .main
            .run_sync(MAIN_WAIT, accent_main)
            .flatten()
            .or_else(|| pool(accent_from_defaults))
    }

    fn reduce_motion(&self) -> bool {
        pool(|| NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion())
    }

    fn reduce_transparency(&self) -> bool {
        pool(|| NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceTransparency())
    }

    fn dark_mode(&self) -> bool {
        self.ctx
            .main
            .run_sync(MAIN_WAIT, dark_main)
            .flatten()
            .unwrap_or_else(|| pool(dark_from_defaults))
    }

    fn taskbar_light(&self) -> Option<bool> {
        // macOS 的菜单栏图标用模板图，系统自动适配
        None
    }

    fn tray_icon_size(&self) -> Option<u32> {
        // 菜单栏图标固定 18 pt，模板图按 @2x 出 36 px，由系统按屏幕的缩放显示，不需要按缩放选图
        None
    }

    fn watch_appearance(&self, cb: Box<dyn Fn() + Send + Sync>) -> Result<HookHandle, PlatformError> {
        let relay = Arc::new(Relay {
            cb: Arc::from(cb),
            pending: AtomicBool::new(false),
        });
        let poke: Arc<dyn Fn() + Send + Sync> = {
            let relay = relay.clone();
            Arc::new(move || relay.poke())
        };
        pool(|| {
            let distributed =
                DistributedObserver::new(self.ctx.main.clone(), APPEARANCE_NOTIFICATIONS, poke.clone())
                    .ok_or_else(|| PlatformError::Unsupported("distributed notification center unavailable".into()))?;
            let local_poke = poke.clone();
            let block = RcBlock::new(move |_n: NonNull<NSNotification>| {
                guarded("appearance observer", || local_poke());
            });
            // SAFETY: 系统导出的通知名常量
            let a11y = unsafe { NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification };
            let workspace = WorkspaceObserver::new(a11y, &block);
            // SAFETY: 同上
            let colors = unsafe { NSSystemColorsDidChangeNotification };
            let system_colors = WorkspaceObserver::on(NSNotificationCenter::defaultCenter(), colors, &block);
            let kvo = register_kvo(&self.ctx.main, relay);
            let main = self.ctx.main.clone();
            Ok(HookHandle::new(move || {
                drop(distributed);
                drop(workspace);
                drop(system_colors);
                unregister_kvo(&main, kvo);
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use objc2::ClassType;

    use super::*;

    #[test]
    fn kvo_class_name_is_namespaced() {
        // objc2 默认以「模块路径 + 类名 + crate 版本」注册类，不会与其他库的同名类冲突
        let name = AppearanceKvo::class()
            .name()
            .to_str()
            .expect("utf-8 class name")
            .to_owned();
        assert!(
            name.starts_with("wisp_platform::macos::screen::AppearanceKvo"),
            "{name}"
        );
    }
}
