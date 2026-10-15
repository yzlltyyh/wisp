//! 权限状态的汇总、广播与轮询，以及「更新后授权失效」的检测。
//!
//! - [`status`]：平台的 `Permissions::status()`（**绝不弹窗**）+ 本模块判定的 `lost_after_update`；结果与上次不同就
//!   emit `permissions:changed`、记一条日志，并通知设置快照与托盘刷新；
//! - [`watch`]：引导页可见时每 2 s 查一次，隐藏后立即停（空闲时不轮询）；轮询期间如果引导与
//!   设置窗口都已关闭也会自行停止，不会因为前端漏发 `permissions_watch(false)` 而常驻；
//! - [`refresh`]：设置窗口获焦、托盘点开、Panel 显示前各查一次（Panel 由窗口层 / 编排器调用）。查询 TCC 与写 state.json
//!   都是阻塞 IO，一律放到阻塞线程池（并发的多次刷新合并为一次），不占 tokio worker，也不占主线程；
//! - [`cached`]：托盘菜单、设置快照读最近一次的结果，**从不阻塞**（还没查过时先按「无需授权」显示并安排一次刷新，
//!   结果出来后托盘与设置快照随之更新）——托盘在冷启动关键路径上建好，不等 TCC；
//! - 更新后授权失效：ad-hoc 签名的 cdhash 每个版本都变，更新后 TCC 授权会失效。state.json 记下「上次看到已授权」的权限与
//!   当时的版本（`permissions_seen_granted` / `permissions_seen_version`），版本变了且授权从有到无 → `lost_after_update`，
//!   托盘状态行与引导页据此提示「更新后需要重新授权」；重新授权后自动清除。同一版本内用户主动撤销不算。

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tauri::{Emitter, Manager};
use wisp_types::{PermissionSet, PermissionState, event};

use crate::settings::state_store::PermissionsSeen;
use crate::state::AppState;

/// 引导页可见时的轮询间隔
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// 最近一次广播的权限状态
static CACHE: Mutex<Option<PermissionSet>> = Mutex::new(None);
/// 轮询任务
static WATCH: Mutex<Option<tauri::async_runtime::JoinHandle<()>>> = Mutex::new(None);
/// 已有一次后台刷新在排队 / 进行（合并并发的刷新请求）
static REFRESHING: AtomicBool = AtomicBool::new(false);

/// 判定的结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evaluation {
    pub set: PermissionSet,
    /// 需要写回 state.json 的「已见授权」与版本（没变化时为 None）
    pub seen: Option<(PermissionsSeen, String)>,
}

/// 由平台原始状态与 state.json 里的记录判定 `lost_after_update`（纯函数）
pub fn evaluate(raw: PermissionSet, seen: PermissionsSeen, seen_version: &str, version: &str) -> Evaluation {
    let granted = |s: PermissionState| s == PermissionState::Granted;
    let lost_one = |was: bool, now: PermissionState| {
        was && !granted(now)
            && now != PermissionState::NotApplicable
            && !seen_version.is_empty()
            && seen_version != version
    };
    let lost = lost_one(seen.accessibility, raw.accessibility) || lost_one(seen.screen_recording, raw.screen_recording);
    let next = if lost {
        // 失效期间保留「曾经授权过」的记录与旧版本号，直到用户重新授权
        (
            PermissionsSeen {
                accessibility: seen.accessibility || granted(raw.accessibility),
                screen_recording: seen.screen_recording || granted(raw.screen_recording),
            },
            seen_version.to_owned(),
        )
    } else {
        (
            PermissionsSeen {
                accessibility: granted(raw.accessibility),
                screen_recording: granted(raw.screen_recording),
            },
            version.to_owned(),
        )
    };
    let changed = next.0 != seen || next.1 != seen_version;
    Evaluation {
        set: PermissionSet {
            lost_after_update: lost,
            ..raw
        },
        seen: changed.then_some(next),
    }
}

/// 当前权限状态（不弹窗）：判定 `lost_after_update`，变化时广播。**阻塞**（查询 TCC、可能写 state.json）：
/// 异步上下文里经 [`refresh`] 或 `run_blocking` 调用
pub fn status(app: &tauri::AppHandle) -> PermissionSet {
    let Some(state) = app.try_state::<AppState>() else {
        return PermissionSet::not_applicable();
    };
    let raw = state.platform.permissions.status();
    let (seen, seen_version) = state
        .settings
        .state()
        .read(|s| (s.permissions_seen_granted, s.permissions_seen_version.clone()));
    let version = app.package_info().version.to_string();
    let eval = evaluate(raw, seen, &seen_version, &version);
    if let Some((seen, version)) = eval.seen
        && let Err(e) = state.settings.state().update(&mut |s| {
            s.permissions_seen_granted = seen;
            s.permissions_seen_version.clone_from(&version);
        })
    {
        log::warn!("cannot persist permission state: {}", e.message);
    }
    publish(app, &state, eval.set);
    eval.set
}

/// 最近一次的权限状态（**不阻塞**）。还没查过时安排一次后台刷新，并先返回「无需授权」（`not_applicable`，托盘状态行
/// 显示正常）；刷新完成后托盘与设置快照会随之更新
pub fn cached(app: &tauri::AppHandle) -> PermissionSet {
    let cached = *CACHE.lock();
    cached.unwrap_or_else(|| {
        refresh(app);
        PermissionSet::not_applicable()
    })
}

/// 在阻塞线程池里查一次（设置窗口获焦、托盘点开、Panel 显示前）；已有刷新在排队时合并
pub fn refresh(app: &tauri::AppHandle) {
    if REFRESHING.swap(true, Ordering::AcqRel) {
        return;
    }
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        // 先清标记再查：查询期间到来的刷新请求会再排一次，不会漏掉最新的变化
        REFRESHING.store(false, Ordering::Release);
        status(&handle);
    });
}

/// 与上次不同时：日志 + `permissions:changed` + 设置快照 / 托盘刷新
fn publish(app: &tauri::AppHandle, state: &AppState, set: PermissionSet) {
    let previous = CACHE.lock().replace(set);
    if previous == Some(set) {
        return;
    }
    log::info!(
        "permissions accessibility={:?} screen_recording={:?} input_monitoring={:?} lost_after_update={}",
        set.accessibility,
        set.screen_recording,
        set.input_monitoring,
        set.lost_after_update
    );
    if previous.is_none() {
        // 第一次读取建立基线：不发 permissions:changed，但托盘与设置快照之前用的是占位值（见 `cached`），需要更新
        crate::tray::refresh(app);
        state.settings.notify_changed();
        return;
    }
    if let Err(e) = app.emit(event::PERMISSIONS_CHANGED, set) {
        log::warn!("emit permissions:changed failed: {e}");
    }
    state.settings.notify_changed();
}

/// 引导页可见时开始 / 停止 2 s 轮询（`permissions_watch` 命令）
pub fn watch(app: &tauri::AppHandle, active: bool) {
    let mut slot = WATCH.lock();
    if let Some(task) = slot.take() {
        task.abort();
    }
    if !active {
        log::debug!("permission polling stopped");
        return;
    }
    log::debug!("permission polling started");
    let handle = app.clone();
    *slot = Some(tauri::async_runtime::spawn(async move {
        loop {
            let h = handle.clone();
            if let Err(e) = tauri::async_runtime::spawn_blocking(move || status(&h)).await {
                log::warn!("permission poll failed: {e}");
            }
            tokio::time::sleep(POLL_INTERVAL).await;
            let any_window = [
                crate::windows::settings_window::ONBOARDING_LABEL,
                crate::windows::settings_window::SETTINGS_LABEL,
            ]
            .iter()
            .any(|l| handle.get_webview_window(l).is_some());
            if !any_window {
                log::debug!("permission polling stopped: no onboarding / settings window");
                break;
            }
        }
    }));
}

/// 窗口销毁时停止轮询（引导页关闭）
pub fn stop_watch() {
    if let Some(task) = WATCH.lock().take() {
        task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(ax: PermissionState, sr: PermissionState) -> PermissionSet {
        PermissionSet {
            accessibility: ax,
            screen_recording: sr,
            input_monitoring: PermissionState::NotDetermined,
            lost_after_update: false,
        }
    }

    use PermissionState::{Denied, Granted, NotApplicable, NotDetermined};

    #[test]
    fn first_grant_is_recorded_with_the_version() {
        let e = evaluate(set(Granted, NotDetermined), PermissionsSeen::default(), "", "0.1.0");
        assert!(!e.set.lost_after_update);
        assert_eq!(
            e.seen,
            Some((
                PermissionsSeen {
                    accessibility: true,
                    screen_recording: false
                },
                "0.1.0".to_owned()
            ))
        );
        // 没有变化时不写 state.json
        let seen = PermissionsSeen {
            accessibility: true,
            screen_recording: false,
        };
        assert_eq!(evaluate(set(Granted, NotDetermined), seen, "0.1.0", "0.1.0").seen, None);
    }

    #[test]
    fn grant_lost_after_an_update_is_detected_until_regranted() {
        let seen = PermissionsSeen {
            accessibility: true,
            screen_recording: true,
        };
        let e = evaluate(set(Denied, Granted), seen, "0.1.0", "0.1.1");
        assert!(e.set.lost_after_update);
        // 失效期间保留旧版本号，下次检查仍然提示
        let (seen2, v) = e.seen.unwrap_or((seen, "0.1.0".to_owned()));
        assert_eq!(v, "0.1.0");
        let e = evaluate(set(Denied, Granted), seen2, &v, "0.1.1");
        assert!(e.set.lost_after_update);
        // 重新授权后清除，并记下新版本
        let e = evaluate(set(Granted, Granted), seen2, &v, "0.1.1");
        assert!(!e.set.lost_after_update);
        assert_eq!(e.seen.map(|s| s.1), Some("0.1.1".to_owned()));
    }

    #[test]
    fn revoking_within_the_same_version_is_not_an_update_problem() {
        let seen = PermissionsSeen {
            accessibility: true,
            screen_recording: false,
        };
        let e = evaluate(set(Denied, NotDetermined), seen, "0.1.0", "0.1.0");
        assert!(!e.set.lost_after_update);
        assert_eq!(e.seen.map(|s| s.0.accessibility), Some(false));
    }

    #[test]
    fn non_macos_platforms_never_report_lost() {
        let seen = PermissionsSeen {
            accessibility: true,
            screen_recording: true,
        };
        let e = evaluate(PermissionSet::not_applicable(), seen, "0.1.0", "0.2.0");
        assert!(!e.set.lost_after_update);
        assert_eq!(e.set.accessibility, NotApplicable);
    }
}
