//! 系统权限。
//!
//! - `status()` 绝不弹窗：`AXIsProcessTrusted`、`CGPreflightScreenCaptureAccess`、`IOHIDCheckAccess(ListenEvent)`；
//! - 辅助功能与屏幕录制的预检只有「已授权 / 未授权」两种结果：本进程请求过之后仍未授权报 Denied，否则 NotDetermined；
//!   输入监控的 IOHIDCheckAccess 本身能区分三种状态；
//! - `request()` 才会触发系统对话框（屏幕录制的系统弹窗每个应用只出现一次，之后只能去系统设置）；
//! - `open_settings()` 打开「隐私与安全性」对应页面（macOS 13+ 的系统设置仍接受旧的 URL 形式；失败时尝试新形式）。
//! - `lost_after_update`（签名身份变化时更新后授权失效：0.1.1 及更早是 ad-hoc 签名，每个版本的 cdhash 都不同；以后换证书
//!   也一样）由 desktop 结合持久化状态判断，平台层恒报 false。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use objc2_app_kit::NSWorkspace;
use objc2_application_services::{AXIsProcessTrustedWithOptions, kAXTrustedCheckOptionPrompt};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFString, CFType};
use objc2_core_graphics::{CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess};
use objc2_foundation::{NSString, NSURL};
use wisp_types::{PermissionKind, PermissionSet, PermissionState};

use super::Ctx;
use super::ax;
use super::ffi;
use super::util::pool;
use crate::error::PlatformError;
use crate::traits::Permissions;

/// 本进程内的请求记录
#[derive(Default)]
pub(crate) struct PermState {
    requested_accessibility: AtomicBool,
    requested_screen: AtomicBool,
}

/// 输入监控的当前状态
pub(crate) fn input_monitoring() -> PermissionState {
    // SAFETY: 无副作用的查询
    match unsafe { ffi::IOHIDCheckAccess(ffi::IOHID_REQUEST_LISTEN_EVENT) } {
        ffi::IOHID_ACCESS_GRANTED => PermissionState::Granted,
        ffi::IOHID_ACCESS_DENIED => PermissionState::Denied,
        _ => PermissionState::NotDetermined,
    }
}

fn binary(granted: bool, requested: &AtomicBool) -> PermissionState {
    if granted {
        PermissionState::Granted
    } else if requested.load(Ordering::Relaxed) {
        PermissionState::Denied
    } else {
        PermissionState::NotDetermined
    }
}

/// 三项权限的当前状态（不弹窗）
pub(crate) fn current(state: &PermState) -> PermissionSet {
    PermissionSet {
        accessibility: binary(ax::trusted(), &state.requested_accessibility),
        screen_recording: binary(CGPreflightScreenCaptureAccess(), &state.requested_screen),
        input_monitoring: input_monitoring(),
        lost_after_update: false,
    }
}

/// 系统设置中的页面锚点
fn settings_anchor(kind: PermissionKind) -> &'static str {
    match kind {
        PermissionKind::Accessibility => "Privacy_Accessibility",
        PermissionKind::ScreenRecording => "Privacy_ScreenCapture",
        PermissionKind::InputMonitoring => "Privacy_ListenEvent",
    }
}

fn open_url(url: &str) -> bool {
    pool(|| NSURL::URLWithString(&NSString::from_str(url)).is_some_and(|u| NSWorkspace::sharedWorkspace().openURL(&u)))
}

/// Permissions 实现
pub(crate) struct MacPermissions {
    ctx: Arc<Ctx>,
}

impl MacPermissions {
    pub(crate) fn new(ctx: Arc<Ctx>) -> Self {
        Self { ctx }
    }
}

impl Permissions for MacPermissions {
    fn status(&self) -> PermissionSet {
        current(&self.ctx.permissions)
    }

    fn request(&self, kind: PermissionKind) -> Result<(), PlatformError> {
        match kind {
            PermissionKind::Accessibility => {
                self.ctx
                    .permissions
                    .requested_accessibility
                    .store(true, Ordering::Relaxed);
                // SAFETY: 系统导出的键常量
                let key: &CFString = unsafe { kAXTrustedCheckOptionPrompt };
                let value: &CFType = CFBoolean::new(true);
                let options = CFDictionary::<CFString, CFType>::from_slices(&[key], &[value]);
                // SAFETY: options 是有效的 CFDictionary；带 prompt 时系统弹出授权提示（异步，不阻塞）
                let _ = unsafe { AXIsProcessTrustedWithOptions(Some(options.as_opaque())) };
                Ok(())
            }
            PermissionKind::ScreenRecording => {
                self.ctx.permissions.requested_screen.store(true, Ordering::Relaxed);
                // 系统只弹一次；返回值是当前是否已授权，授权后通常需要重启进程才生效
                let _ = CGRequestScreenCaptureAccess();
                Ok(())
            }
            PermissionKind::InputMonitoring => {
                // SAFETY: 首次调用时弹出系统对话框，之后直接返回当前状态
                let _ = unsafe { ffi::IOHIDRequestAccess(ffi::IOHID_REQUEST_LISTEN_EVENT) };
                Ok(())
            }
        }
    }

    fn open_settings(&self, kind: PermissionKind) -> Result<(), PlatformError> {
        let anchor = settings_anchor(kind);
        let legacy = format!("x-apple.systempreferences:com.apple.preference.security?{anchor}");
        let modern = format!("x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?{anchor}");
        if open_url(&legacy) || open_url(&modern) {
            Ok(())
        } else {
            Err(PlatformError::Other(format!(
                "failed to open system settings ({anchor})"
            )))
        }
    }
}
