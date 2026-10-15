//! 重启续接：`app_relaunch(resume)` 把重启后要打开的窗口与路由（例如引导的
//! `{ kind: 'onboarding', step: 1 }`——用户刚在系统设置里授了权，需要重启才生效）连同时间戳**同步写进 state.json**，
//! 再 `request_restart`；下次启动时 bootstrap 取走一次（[`take`]），按 `Boot.route` 打开该窗口。
//!
//! - 只取一次：读到就清掉，不论是否过期（崩溃、手动重启都不会反复弹出同一个窗口）；
//! - 5 分钟内有效（[`RESUME_TTL_MS`]）：重启失败、用户隔了很久才手动打开时不再续接；时间戳在未来（改过系统时间）也不算；
//! - 优先于启动时的默认窗口：引导未完成时打开续接的那一步而不是引导首页；由开机自启 / `--trigger` 拉起（平时不弹窗）
//!   时同样打开——续接是用户刚刚明确要求的；
//! - 写入失败只记日志，照常重启（重启是用户的明确操作，续接是锦上添花）。
//!
//! 取代前端 `onboarding/resume.ts` 的 localStorage 交接（WebView 的存储在重启前后不保证落盘，且只有引导窗口能读）。

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::Manager;
use wisp_types::WindowTarget;

use crate::error::IpcError;
use crate::settings::SettingsService;
use crate::state::AppState;

/// 续接请求的有效期
pub const RESUME_TTL_MS: i64 = 5 * 60 * 1000;

/// state.json 里的续接请求（`StateFile.resume`）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeRequest {
    pub target: WindowTarget,
    /// 写入时刻（Unix 毫秒）
    pub at_ms: i64,
}

/// 续接请求此刻是否仍然有效（纯函数）
pub fn still_valid(req: &ResumeRequest, now_ms: i64) -> bool {
    (0..=RESUME_TTL_MS).contains(&now_ms.saturating_sub(req.at_ms))
}

/// 当前 Unix 毫秒
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// 把续接目标连同此刻的时间戳**同步写进 state.json**（`StateFile.resume`，`StateStore::update` 落盘之后才返回）。
/// **阻塞**（一次写盘）。拆出来是为了不经 `AppHandle` 也能测：`app_relaunch` → 本函数 → state.json → 下次启动的 [`take`]
pub fn persist(settings: &SettingsService, target: WindowTarget) -> Result<(), IpcError> {
    let req = ResumeRequest {
        target,
        at_ms: now_ms(),
    };
    settings.state().update(&mut |s| s.resume = Some(req.clone()))
}

/// `app_relaunch`（`ipc/cmd_app.rs`）：有续接目标时先 [`persist`]（阻塞线程上、等写完），再请求重启
/// （`RESTART_EXIT_CODE`，更新器据此在退出时安装暂存的更新）。目标已由 ipc 层校验（`validate_opt_window_target`）。
/// 写入失败只记日志、照常重启（重启是用户的明确操作，续接是锦上添花）
pub async fn relaunch(app: tauri::AppHandle, resume: Option<WindowTarget>) -> Result<(), IpcError> {
    if let Some(target) = resume
        && let Some(state) = app.try_state::<AppState>()
    {
        let settings = Arc::clone(&state.settings);
        let saved = tauri::async_runtime::spawn_blocking(move || persist(&settings, target)).await;
        match saved {
            Ok(Ok(())) => log::info!("relaunch: resume target saved"),
            Ok(Err(e)) => log::warn!("relaunch: saving the resume target failed: {}", e.message),
            Err(e) => log::warn!("relaunch: saving the resume target failed: {e}"),
        }
    }
    app.request_restart();
    Ok(())
}

/// 启动时取走续接请求（只取一次：读到就清掉）；过期或时间戳异常时返回 None。**阻塞**（有请求时同步写一次盘）
pub fn take(settings: &SettingsService) -> Option<WindowTarget> {
    let req = settings.state().read(|s| s.resume.clone())?;
    if let Err(e) = settings.state().update(&mut |s| s.resume = None) {
        log::warn!("relaunch: clearing the resume target failed: {}", e.message);
    }
    let now = now_ms();
    if still_valid(&req, now) && crate::ipc::validate::validate_window_target(&req.target).is_ok() {
        log::info!("relaunch: resuming {:?}", req.target);
        Some(req.target)
    } else {
        log::info!(
            "relaunch: resume target dropped (age {} ms)",
            now.saturating_sub(req.at_ms)
        );
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::service::tests::test_service;

    fn req(at_ms: i64) -> ResumeRequest {
        ResumeRequest {
            target: WindowTarget::Onboarding { step: Some(1) },
            at_ms,
        }
    }

    #[test]
    fn resume_is_valid_for_five_minutes_only() {
        let t = 1_760_000_000_000;
        assert!(still_valid(&req(t), t));
        assert!(still_valid(&req(t), t + RESUME_TTL_MS));
        assert!(!still_valid(&req(t), t + RESUME_TTL_MS + 1));
        // 时间戳在未来（系统时间被往回调过）：不续接
        assert!(!still_valid(&req(t), t - 1));
        assert!(!still_valid(&req(i64::MIN), i64::MAX));
    }

    /// `app_relaunch` 的写入路径（[`relaunch`] 里除了 `request_restart` 之外的全部）：写进 state.json 的文件本身，
    /// 「重启」（重新加载）之后 [`take`] 取回同一个目标，只取一次（走完 app_relaunch → state.json → take 的完整路径）
    #[test]
    fn persisted_resume_reaches_the_next_start() {
        let (_dir, svc) = test_service();
        let target = WindowTarget::Onboarding { step: Some(1) };
        let before = now_ms();
        persist(&svc, target.clone()).expect("写入 state.json");
        // 落盘的是文件本身，不只是内存里的副本
        let on_disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(svc.state().path()).expect("读 state.json")).expect("JSON");
        let saved: ResumeRequest =
            serde_json::from_value(on_disk.get("resume").cloned().expect("state.json 里有 resume")).expect("形状");
        assert_eq!(saved.target, target);
        assert!(saved.at_ms >= before && saved.at_ms <= now_ms(), "时间戳是写入时刻");
        // 设置窗口的分组 + 锚点同样能续接（validate_window_target 认得的组合）
        let reloaded = SettingsService::load_for_tests(svc.paths());
        assert_eq!(take(&reloaded), Some(target));
        assert_eq!(take(&reloaded), None, "只取一次");
        let settings_target = WindowTarget::Settings {
            section: Some("ocr".to_owned()),
            anchor: Some(crate::app::sections::ANCHOR_OCR_HISTORY.to_owned()),
        };
        persist(&reloaded, settings_target.clone()).expect("再写一次");
        assert_eq!(
            take(&SettingsService::load_for_tests(svc.paths())),
            Some(settings_target)
        );
    }

    #[test]
    fn resume_is_taken_once_and_survives_a_restart() {
        let (_dir, svc) = test_service();
        assert_eq!(take(&svc), None);
        let target = WindowTarget::Onboarding { step: Some(1) };
        svc.state()
            .update(&mut |s| {
                s.resume = Some(ResumeRequest {
                    target: target.clone(),
                    at_ms: now_ms(),
                })
            })
            .expect("写入");
        // 重启 = 重新读 state.json
        let reloaded = SettingsService::load_for_tests(svc.paths());
        assert_eq!(take(&reloaded), Some(target));
        assert_eq!(take(&reloaded), None, "只取一次");
        assert_eq!(
            SettingsService::load_for_tests(svc.paths())
                .state()
                .read(|s| s.resume.clone()),
            None,
            "取走时同步清掉（落盘）"
        );
        // 过期的：清掉、不续接
        svc.state()
            .update(&mut |s| s.resume = Some(req(now_ms() - RESUME_TTL_MS - 1_000)))
            .expect("写入");
        assert_eq!(take(&svc), None);
        assert_eq!(svc.state().read(|s| s.resume.clone()), None);
        // 未登记的设置锚点（手改的 state.json）：不续接
        svc.state()
            .update(&mut |s| {
                s.resume = Some(ResumeRequest {
                    target: WindowTarget::Settings {
                        section: Some("ocr".to_owned()),
                        anchor: Some("nope".to_owned()),
                    },
                    at_ms: now_ms(),
                });
            })
            .expect("写入");
        assert_eq!(take(&svc), None);
    }
}
