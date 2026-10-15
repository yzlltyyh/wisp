//! 前台应用追踪与激活。
//!
//! - `frontmost()`：`NSWorkspace.frontmostApplication`；前台是 Wisp 自己时沿用最近一次的非 Wisp 应用；
//! - 「最近一次的非 Wisp 应用」由 `NSWorkspaceDidActivateApplicationNotification` 事件驱动地记录（无轮询），
//!   因此从固定窗口 / 设置窗口（Wisp 处于激活状态）触发时也能找到原应用；
//! - `activate()`：macOS 14+ 用 `yieldActivationToApplication` + `activateFromApplication:options:`（协作式激活），
//!   更早的系统用 `activateWithOptions(IgnoringOtherApps)`；在主线程上执行。

use std::ptr::NonNull;
use std::sync::{Arc, Weak};
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationOptions, NSRunningApplication, NSWorkspace, NSWorkspaceApplicationKey,
    NSWorkspaceDidActivateApplicationNotification,
};
use objc2_foundation::{NSNotification, NSNotificationCenter};
use parking_lot::Mutex;
use wisp_types::AppRef;

use super::Ctx;
use super::util::{guarded, pool};
use crate::error::PlatformError;
use crate::traits::FocusTracker;

/// 在主线程上完成激活请求的最长等待
const ACTIVATE_WAIT: Duration = Duration::from_millis(250);

/// 前台应用记录
#[derive(Default)]
pub(crate) struct FocusState {
    last: Mutex<Option<AppRef>>,
    observer: Mutex<Option<WorkspaceObserver>>,
}

/// NSNotificationCenter 的块观察者；drop 时注销（`removeObserver:` 线程安全）
pub(crate) struct WorkspaceObserver {
    center: Retained<NSNotificationCenter>,
    token: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}

// SAFETY: 观察者令牌与通知中心只用于 removeObserver:，NSNotificationCenter 的登记与注销是线程安全的
unsafe impl Send for WorkspaceObserver {}

impl WorkspaceObserver {
    /// 在 NSWorkspace 的通知中心登记一个块观察者（回调在发布通知的线程，即主线程上执行）
    pub(crate) fn new(
        name: &objc2_foundation::NSNotificationName,
        block: &block2::DynBlock<dyn Fn(NonNull<NSNotification>)>,
    ) -> Self {
        Self::on(NSWorkspace::sharedWorkspace().notificationCenter(), name, block)
    }

    /// 在指定的通知中心登记
    pub(crate) fn on(
        center: Retained<NSNotificationCenter>,
        name: &objc2_foundation::NSNotificationName,
        block: &block2::DynBlock<dyn Fn(NonNull<NSNotification>)>,
    ) -> Self {
        // SAFETY: 名称与块都有效；块是 'static 的（只捕获 Weak / Arc）
        let token = unsafe { center.addObserverForName_object_queue_usingBlock(Some(name), None, None, block) };
        Self { center, token }
    }
}

impl Drop for WorkspaceObserver {
    fn drop(&mut self) {
        // SAFETY: 令牌是 addObserverForName 返回的观察者对象
        unsafe { self.center.removeObserver(ProtocolObject::as_ref(&*self.token)) };
    }
}

/// 开始事件驱动地记录前台应用（`create` 调用；不阻塞）
pub(crate) fn start_tracking(ctx: &Arc<Ctx>) {
    pool(|| {
        if let Some(app) = NSWorkspace::sharedWorkspace().frontmostApplication()
            && app.processIdentifier() != ctx.own_pid
        {
            *ctx.focus.last.lock() = Some(app_ref(&app));
        }
        let weak: Weak<Ctx> = Arc::downgrade(ctx);
        let block = RcBlock::new(move |n: NonNull<NSNotification>| {
            // 回调在 AppKit 的通知派发帧里：浮标消失会同步调用上层的 sink，panic 不能展开穿过 AppKit
            guarded("app activation observer", || {
                let Some(ctx) = weak.upgrade() else { return };
                // SAFETY: 通知对象在回调期间有效
                let n = unsafe { n.as_ref() };
                let Some(app) = activated_app(n) else { return };
                let pid = app.processIdentifier();
                if pid != ctx.own_pid {
                    *ctx.focus.last.lock() = Some(app_ref(&app));
                }
                #[cfg(feature = "mouse-hook")]
                super::bobber::on_app_activated(&ctx, pid);
            });
        });
        // SAFETY: 系统导出的通知名常量
        let name = unsafe { NSWorkspaceDidActivateApplicationNotification };
        *ctx.focus.observer.lock() = Some(WorkspaceObserver::new(name, &block));
    });
}

/// 激活通知里的 NSRunningApplication
fn activated_app(n: &NSNotification) -> Option<Retained<NSRunningApplication>> {
    let info = n.userInfo()?;
    // SAFETY: 系统导出的键常量
    let key = unsafe { NSWorkspaceApplicationKey };
    let obj = info.objectForKey(key)?;
    obj.downcast::<NSRunningApplication>().ok()
}

/// NSRunningApplication → AppRef
pub(crate) fn app_ref(app: &NSRunningApplication) -> AppRef {
    let pid = app.processIdentifier();
    let bundle_path = app.bundleURL().and_then(|u| u.path()).map(|p| p.to_string());
    let exe_path = app.executableURL().and_then(|u| u.path()).map(|p| p.to_string());
    let id = app
        .bundleIdentifier()
        .map(|s| s.to_string())
        .or_else(|| {
            exe_path
                .as_deref()
                .and_then(|p| p.rsplit('/').next())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| format!("pid:{pid}"));
    let name = app.localizedName().map(|s| s.to_string()).unwrap_or_else(|| id.clone());
    AppRef {
        id,
        alt_id: None,
        name,
        pid: u32::try_from(pid).ok(),
        path: bundle_path.or(exe_path),
        window: None,
    }
}

/// 当前前台进程的 pid（不排除 Wisp）
pub(crate) fn frontmost_pid() -> Option<i32> {
    pool(|| {
        NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .map(|a| a.processIdentifier())
    })
}

/// 按 AppRef 找到正在运行的应用：优先 pid（同时校验 bundle id，防止 pid 被复用），否则按 bundle id 取第一个
pub(crate) fn running_app(app: &AppRef) -> Option<Retained<NSRunningApplication>> {
    if let Some(pid) = app.pid.and_then(|p| i32::try_from(p).ok()) {
        if let Some(running) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) {
            let same = running
                .bundleIdentifier()
                .is_none_or(|b| b.to_string().eq_ignore_ascii_case(&app.id));
            if same && !running.isTerminated() {
                return Some(running);
            }
        }
        return None;
    }
    let id = objc2_foundation::NSString::from_str(&app.id);
    NSRunningApplication::runningApplicationsWithBundleIdentifier(&id)
        .iter()
        .find(|a| !a.isTerminated())
}

/// FocusTracker 实现
pub(crate) struct MacFocus {
    ctx: Arc<Ctx>,
}

impl MacFocus {
    pub(crate) fn new(ctx: Arc<Ctx>) -> Self {
        Self { ctx }
    }
}

/// 激活某个应用（在主线程上执行）；返回系统是否接受了请求。
///
/// 主线程 [`ACTIVATE_WAIT`] 内没有空出来就返回 Timeout，并且**作废**这次激活：调用方据此改走「只复制」并提示用户，
/// 之后主线程空下来也不会再把目标应用拉到前台（否则用户被告知只复制了，前台却突然切走）
pub(crate) fn activate_app(ctx: &Ctx, app: &AppRef) -> Result<(), PlatformError> {
    let target = pool(|| running_app(app)).ok_or_else(|| PlatformError::Other("target app is not running".into()))?;
    let pid = target.processIdentifier();
    let accepted = ctx.main.run_sync_or_cancel(ACTIVATE_WAIT, move |mtm| {
        let Some(target) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) else {
            return false;
        };
        if objc2::available!(macos = 14.0) {
            let me = NSApplication::sharedApplication(mtm);
            me.yieldActivationToApplication(&target);
            let current = NSRunningApplication::currentApplication();
            if target.activateFromApplication_options(&current, NSApplicationActivationOptions::empty()) {
                return true;
            }
            target.activateWithOptions(NSApplicationActivationOptions::empty())
        } else {
            #[allow(deprecated)]
            target.activateWithOptions(NSApplicationActivationOptions::ActivateIgnoringOtherApps)
        }
    });
    match accepted {
        Some(true) => Ok(()),
        Some(false) => Err(PlatformError::Other("activation request was refused".into())),
        None => Err(PlatformError::Timeout),
    }
}

impl FocusTracker for MacFocus {
    fn frontmost(&self) -> Option<AppRef> {
        pool(|| {
            let current = NSWorkspace::sharedWorkspace().frontmostApplication();
            match current {
                Some(app) if app.processIdentifier() != self.ctx.own_pid => {
                    let r = app_ref(&app);
                    *self.ctx.focus.last.lock() = Some(r.clone());
                    Some(r)
                }
                _ => self.ctx.focus.last.lock().clone(),
            }
        })
    }

    fn activate(&self, app: &AppRef) -> Result<(), PlatformError> {
        activate_app(&self.ctx, app)
    }

    fn is_elevated(&self, _app: &AppRef) -> bool {
        // macOS 没有 UIPI 式的提权隔离
        false
    }
}

/// 当前前台应用（不排除 Wisp 自己；剪贴板来源用，是近似值：拿不到真正的写入方）
pub(crate) fn frontmost_any(_ctx: &Ctx) -> Option<AppRef> {
    pool(|| {
        NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .map(|a| app_ref(&a))
    })
}

/// 前台应用（排除 Wisp 自己时沿用最近的非 Wisp 应用）
pub(crate) fn frontmost_excluding(ctx: &Ctx) -> Option<AppRef> {
    pool(|| match NSWorkspace::sharedWorkspace().frontmostApplication() {
        Some(app) if app.processIdentifier() != ctx.own_pid => Some(app_ref(&app)),
        _ => ctx.focus.last.lock().clone(),
    })
}
