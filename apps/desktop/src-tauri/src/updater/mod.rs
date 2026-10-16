//! 更新器：tauri-plugin-updater 读 GitHub Release 的 `latest.json`，minisign 验签。
//!
//! - **何时检查**：启动 30 s 后检查一次（一次性 sleep）；之后不设常驻定时器，托盘点开、设置窗口获焦、面板显示时调用
//!   [`maybe_check`]，距上次**成功**检查（state.json 的 `last_update_check_ms`）超过 24 h 才真正发请求。检查失败（例如开机时
//!   还没联网）不算「已检查」，[`FAILURE_BACKOFF`] 之后的下一次惰性检查会重试；
//! - **同一时刻只有一个检查**：手动检查（托盘「检查更新」、设置页按钮）遇到进行中的自动检查时**等它结束、用它的结果**，
//!   不会把中间态 `checking` 当成「已是最新」；遇到下载 / 安装时如实报告 `downloading` / `ready`；
//! - **状态不倒退**：检查期间（`checking`）与手动检查失败（`error`）时仍带着已知新版本的 version / notes（托盘「有新版本」、
//!   关于页的版本与变更日志不会因一次检查而消失）；**自动检查失败是静默的**——恢复检查前的状态，只记日志并退避；
//! - **策略**（`general.update_policy`）：`notify`（默认）只提示——托盘状态行显示「有新版本 x.y.z」并加角标，点击跳到
//!   设置 → 关于（显示变更日志与「下载并安装」按钮）；`off` 时自动检查**不发任何请求**；托盘「检查更新」与设置页的按钮是
//!   用户的显式操作，任何策略下都会检查；`auto_install` 发现新版本后在后台处理，按平台与安装方式（[`auto_plan`]）：
//!   - macOS（.app 所在目录可写）/ Linux AppImage（文件所在目录可写）：下载并安装，下次启动生效，发一条通知；
//!   - Windows（NSIS，当前用户安装）：安装包下载并验签后**落盘**到 `data_dir/updates/`
//!     （不常驻内存），等到空闲时发一条通知，以 passive 模式安装并自动重新打开，不打断正在进行的操作。「空闲」是事件驱动
//!     判断的（见 `spawn_idle_install`）：最后一次动静（触发、窗口获焦 / 失焦 / 销毁、托盘点开）之后安静 [`IDLE_QUIET`]
//!     且没有可见窗口；空闲之前用户退出 / 重启 Wisp 时在退出时安装（重启时装完重新打开）；
//!   - 需要管理员权限的安装（Linux deb / rpm 要经 pkexec 输入密码；macOS .app 所在目录不可写）：**降级为只提示**，等用户在
//!     设置页点「下载并安装」时再进行，绝不在后台弹密码框；
//! - **落盘安装包的完整性**：tauri-plugin-updater 只在 `download()` 里验签，`install(bytes)` 不验。因此暂存文件**只在本进程
//!   内**使用，且安装前按下载验签时记在内存里的 blake3 核对内容；进程重启后不复用（启动时清掉），重新下载并验签——别的
//!   进程写进 `updates/` 的文件永远不会被执行；
//! - **安装**（[`install`]，设置页「下载并安装」）：下载时以 `update:status` 推送进度；Windows 安装器启动后进程立即退出
//!   并在安装完成后重启，macOS / Linux 替换后状态为 `ready`，重启后生效（`app_relaunch`）。安装是阻塞操作（解包并替换
//!   .app、等待 pkexec 密码框），一律在阻塞线程池里执行，不占 tokio worker；记录检查时间（写 state.json + fsync）同样如此；
//! - Windows 安装器启动、进程退出之前：先写掉待写设置，再在**主线程**上 `cleanup_before_exit`（移除托盘图标，不留幽灵图标）；
//! - debug 构建与冒烟模式不做自动检查（手动检查照常），避免开发时访问网络；
//! - 状态变化 emit_to 设置窗口 `update:status`，并刷新托盘；状态切换（非进度）时同时通知设置快照（`runtime.update`）；
//! - 本机更新演练（[`rehearsal`]）：端点只由编译时的配置决定，正式构建没有运行时覆盖端点或公钥的入口；演练构建
//!   （`tauri build --config` 叠加 `plugins.wispUpdaterRehearsal`）在启动时多记一行配置并支持钥匙串探针。

pub mod rehearsal;

use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tauri::{Emitter, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};
use tokio::sync::{Notify, watch};
use wisp_types::{OsKind, UpdatePolicy, UpdateState, UpdateStatus, event};

use crate::error::{IpcError, IpcErrorCode};
use crate::settings::fsio;
use crate::state::AppState;

/// 启动后第一次检查的延迟
pub const STARTUP_DELAY: Duration = Duration::from_secs(30);
/// 惰性检查的间隔
pub const RECHECK_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
/// 自动检查失败后多久才再试
pub const FAILURE_BACKOFF: Duration = Duration::from_secs(60 * 60);
/// 单次请求的超时
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// 启动 30 s 的检查：这么久之内已经检查过（例如引导窗口获焦时的惰性检查刚刚成功）就跳过，不重复联网
pub const STARTUP_SKIP_IF_CHECKED_WITHIN: Duration = Duration::from_secs(10 * 60);
/// Windows 自动安装：最后一次动静之后安静多久才算空闲（一次性定时，只在「已暂存、待安装」期间存在，有动静就重新计时）
pub const IDLE_QUIET: Duration = Duration::from_secs(30);
/// Windows 自动安装：安静期满时仍有窗口可见（固定窗口常驻、或隐藏面板时没有产生窗口事件），之后若一直没有动静，隔多久
/// 再看一次
pub const IDLE_RECHECK: Duration = Duration::from_secs(10 * 60);
/// [`IDLE_RECHECK`] 的总次数上限（每个暂存的安装包）：用完后只等事件，或在退出时安装——不会无限期地定时唤醒
pub const IDLE_RECHECKS: u32 = 6;
/// 暂存安装包的子目录（`data_dir/updates/`）
pub const STAGED_DIR: &str = "updates";

/// 已落盘、等待安装的 Windows 安装包（只在本进程内有效）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Staged {
    pub version: String,
    pub path: PathBuf,
    /// 下载并验签通过的内容的 blake3（只存在内存里；安装前据此核对文件没有被替换）
    pub hash: fsio::ContentHash,
}

/// 更新器的内部状态
struct Inner {
    status: UpdateStatus,
    /// 最近一次检查到的新版本（安装用）
    update: Option<Update>,
    /// Windows 自动安装：已落盘、等待空闲 / 退出时安装的安装包
    staged: Option<Staged>,
}

static STATE: Mutex<Inner> = Mutex::new(Inner {
    status: UpdateStatus {
        state: UpdateState::Idle,
        version: None,
        notes: None,
        progress: None,
        error: None,
    },
    update: None,
    staged: None,
});

/// 进行中的操作：[`IDLE`] / [`CHECKING`] / [`APPLYING`]（下载、安装）。检查与安装互斥
static ACTIVITY: AtomicU8 = AtomicU8::new(IDLE);
const IDLE: u8 = 0;
const CHECKING: u8 = 1;
const APPLYING: u8 = 2;

/// 每次检查结束时发布它的结果：手动检查遇到进行中的检查时订阅它、拿同一个结果
static CHECK_DONE: LazyLock<watch::Sender<Option<Result<UpdateStatus, IpcError>>>> =
    LazyLock::new(|| watch::Sender::new(None));
/// 自动检查失败后的退避截止时刻（Unix 毫秒；0 = 没有退避）
static RETRY_AFTER_MS: AtomicI64 = AtomicI64::new(0);
/// 本次退出由 `request_restart` 发起（退出时安装暂存的更新后要重新打开）
static RESTART_REQUESTED: AtomicBool = AtomicBool::new(false);
/// 等待空闲安装的任务正在运行
static IDLE_WAITER: AtomicBool = AtomicBool::new(false);
/// 等待空闲安装期间的「有动静」通知（见 [`note_activity`]）
static IDLE_NUDGE: Notify = Notify::const_new();

/// 检查的触发方式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// 启动 30 s 后 / 24 h 惰性检查
    Automatic,
    /// 托盘「检查更新」、设置页按钮
    Manual,
}

/// 发现新版本后要做的事
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowUp {
    /// 只提示
    Notify,
    /// 后台处理（按 [`auto_plan`]）
    AutoInstall,
}

/// 当前的安装方式（决定能否不经用户确认地安装）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Package {
    MacApp,
    AppImage,
    Deb,
    Rpm,
    Nsis,
    Msi,
    /// 开发构建、便携运行等
    Unknown,
}

/// 自动安装的方式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoPlan {
    /// 下载并立即安装，下次启动生效（macOS / AppImage）
    InstallNow,
    /// 下载并落盘，空闲时安装并重启（Windows NSIS）
    InstallWhenIdle,
    /// 安装需要管理员权限或无法自动安装：只提示，等用户点按钮（原因只用于日志）
    NotifyOnly(&'static str),
}

/// 托盘「检查更新」得到结果后要给用户看的反馈
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckFeedback {
    /// 有新版本 / 正在下载 / 已下载待重启（或仍在检查）：打开设置的「关于」（那里显示实时状态）
    OpenAbout,
    /// 已是最新
    UpToDate,
    /// 检查失败
    Failed,
}

/// 手动检查的结果 → 反馈（纯函数）。只有真正的「已是最新」才说已是最新，中间态不当成结果
pub fn feedback(result: &Result<UpdateStatus, IpcError>) -> CheckFeedback {
    match result {
        Ok(s) => match s.state {
            UpdateState::Available | UpdateState::Downloading | UpdateState::Ready | UpdateState::Checking => {
                CheckFeedback::OpenAbout
            }
            UpdateState::UpToDate | UpdateState::Idle | UpdateState::Disabled => CheckFeedback::UpToDate,
            UpdateState::Error => CheckFeedback::Failed,
        },
        Err(_) => CheckFeedback::Failed,
    }
}

/// 自动检查是否到期（纯函数）：策略不是 off、距上次成功检查超过 24 h（或系统时间被调回过去），且不在失败退避期内
pub fn due(policy: UpdatePolicy, now_ms: i64, last_check_ms: i64, retry_after_ms: i64) -> bool {
    let interval = i64::try_from(RECHECK_AFTER.as_millis()).unwrap_or(i64::MAX);
    policy != UpdatePolicy::Off
        && now_ms >= retry_after_ms
        && (last_check_ms <= 0 || now_ms.saturating_sub(last_check_ms) >= interval || now_ms < last_check_ms)
}

/// 发现新版本后的动作（纯函数）：只有自动检查且策略为自动安装时才自动处理
pub fn follow_up(policy: UpdatePolicy, trigger: Trigger) -> FollowUp {
    match (policy, trigger) {
        (UpdatePolicy::AutoInstall, Trigger::Automatic) => FollowUp::AutoInstall,
        _ => FollowUp::Notify,
    }
}

/// 自动安装的方式（纯函数）。`writable`：安装目标（.app / AppImage）所在目录当前用户可写
pub fn auto_plan(os: OsKind, package: Package, writable: bool) -> AutoPlan {
    match (os, package) {
        (OsKind::Windows, Package::Nsis) => AutoPlan::InstallWhenIdle,
        (OsKind::Windows, Package::Msi) => AutoPlan::NotifyOnly("MSI installs need administrator rights"),
        (OsKind::Macos, Package::MacApp) | (OsKind::Linux, Package::AppImage) if writable => AutoPlan::InstallNow,
        (OsKind::Macos, Package::MacApp) => {
            AutoPlan::NotifyOnly("the app folder is not writable; installing needs an administrator password")
        }
        (OsKind::Linux, Package::AppImage) => AutoPlan::NotifyOnly("the AppImage folder is not writable"),
        (OsKind::Linux, Package::Deb | Package::Rpm) => {
            AutoPlan::NotifyOnly("system packages need administrator rights to install")
        }
        _ => AutoPlan::NotifyOnly("not an installed build"),
    }
}

/// 当前构建的安装方式（tauri-bundler 打包时写入；AppImage 另看 `APPIMAGE` 环境变量）
pub fn current_package() -> Package {
    use tauri::utils::config::BundleType;
    match tauri::utils::platform::bundle_type() {
        Some(BundleType::App | BundleType::Dmg) => Package::MacApp,
        Some(BundleType::AppImage) => Package::AppImage,
        Some(BundleType::Deb) => Package::Deb,
        Some(BundleType::Rpm) => Package::Rpm,
        Some(BundleType::Nsis) => Package::Nsis,
        Some(BundleType::Msi) => Package::Msi,
        None if std::env::var_os("APPIMAGE").is_some() => Package::AppImage,
        None => Package::Unknown,
    }
}

/// 安装目标所在目录（替换 .app / AppImage 需要在这里改名）当前用户能否写入；不能写时安装需要管理员密码
fn install_target_writable(package: Package) -> bool {
    let target = match package {
        Package::MacApp => std::env::current_exe().ok().and_then(|exe| {
            exe.ancestors()
                .find(|a| a.extension().is_some_and(|e| e == "app"))
                .map(Path::to_path_buf)
        }),
        Package::AppImage => std::env::var_os("APPIMAGE").map(PathBuf::from),
        _ => None,
    };
    target.as_deref().and_then(Path::parent).is_some_and(dir_writable)
}

/// 目录可写：试建并删除一个探测文件（比看权限位可靠：ACL、只读卷、App Translocation 都能识别）
fn dir_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".wisp-write-test-{}", std::process::id()));
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(file) => {
            drop(file);
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// 自动检查是否被环境禁止（debug 构建、冒烟模式）
fn automatic_disabled() -> Option<&'static str> {
    if crate::app::smoke::enabled() {
        return Some("smoke mode");
    }
    if cfg!(debug_assertions) && std::env::var_os("WISP_DEV_UPDATER").is_none() {
        return Some("debug build (set WISP_DEV_UPDATER=1 to test)");
    }
    None
}

fn policy(app: &tauri::AppHandle) -> UpdatePolicy {
    app.try_state::<AppState>()
        .map_or(UpdatePolicy::Notify, |s| s.settings.get().general.update_policy)
}

/// 当前状态（策略为 off 且没有进行中的操作时报告 `disabled`）
pub fn status(app: &tauri::AppHandle) -> Result<UpdateStatus, IpcError> {
    let status = STATE.lock().status.clone();
    if status.state == UpdateState::Idle && policy(app) == UpdatePolicy::Off {
        return Ok(UpdateStatus {
            state: UpdateState::Disabled,
            ..UpdateStatus::default()
        });
    }
    Ok(status)
}

/// 变更日志（空白视为没有）
fn notes_of(update: &Update) -> Option<String> {
    update.body.clone().filter(|b| !b.trim().is_empty())
}

/// 带着已知新版本（version / notes）的状态：检查期间、检查失败时托盘与关于页不丢掉「有新版本」
fn with_known_update(state: UpdateState, error: Option<String>) -> UpdateStatus {
    let inner = STATE.lock();
    let (version, notes) = inner
        .update
        .as_ref()
        .map_or((None, None), |u| (Some(u.version.clone()), notes_of(u)));
    UpdateStatus {
        state,
        version,
        notes,
        progress: None,
        error,
    }
}

/// 更新状态并广播；`transition` 为 true（状态切换而不是进度）时同时通知设置快照与托盘
fn set_status(app: &tauri::AppHandle, status: UpdateStatus, transition: bool) {
    STATE.lock().status = status;
    publish(app, transition);
}

/// 广播当前状态
fn publish(app: &tauri::AppHandle, transition: bool) {
    let shown = match self::status(app) {
        Ok(s) => s,
        Err(_) => STATE.lock().status.clone(),
    };
    if let Err(e) = app.emit_to(
        crate::windows::settings_window::SETTINGS_LABEL,
        event::UPDATE_STATUS,
        &shown,
    ) {
        log::debug!("emit update:status failed: {e}");
    }
    if transition {
        if let Some(state) = app.try_state::<AppState>() {
            state.settings.notify_changed();
        }
        crate::tray::refresh(app);
    }
}

/// 启动 30 s 后的一次性检查（setup 第 10 步）；Windows 上顺带清掉上次运行留下的暂存安装包（暂存文件不跨进程复用）
pub fn schedule_startup_check(app: &tauri::AppHandle) {
    // 演练构建的钩子（正式构建里只是一次配置表查找）
    rehearsal::on_startup(app);
    if OsKind::current() == OsKind::Windows
        && let Some(dir) = staged_dir(app)
    {
        tauri::async_runtime::spawn_blocking(move || prune_staged(&dir, |_, _| true));
    }
    if let Some(reason) = automatic_disabled() {
        log::info!("startup update check skipped: {reason}");
        return;
    }
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(STARTUP_DELAY).await;
        if policy(&handle) == UpdatePolicy::Off {
            return;
        }
        let last = handle
            .try_state::<AppState>()
            .map_or(0, |s| s.settings.state().read(|st| st.last_update_check_ms));
        let retry_after = RETRY_AFTER_MS.load(Ordering::Acquire);
        if checked_recently(fsio::unix_now_ms(), last, retry_after) {
            log::debug!("startup update check skipped: checked moments ago");
            return;
        }
        // 结局（含失败）由 check_inner 记一行
        let _ = run_check(&handle, Trigger::Automatic).await;
    });
}

/// 启动 30 s 的检查是否可以跳过（纯函数）：[`STARTUP_SKIP_IF_CHECKED_WITHIN`] 之内成功检查过，或刚失败、还在
/// [`FAILURE_BACKOFF`] 退避期内（失败只在本进程内记，见 `RETRY_AFTER_MS`）
pub fn checked_recently(now_ms: i64, last_check_ms: i64, retry_after_ms: i64) -> bool {
    let window = i64::try_from(STARTUP_SKIP_IF_CHECKED_WITHIN.as_millis()).unwrap_or(i64::MAX);
    now_ms < retry_after_ms
        || (last_check_ms > 0 && now_ms >= last_check_ms && now_ms.saturating_sub(last_check_ms) < window)
}

/// 惰性检查：距上次成功检查超过 24 h（且不在失败退避期内）才在后台检查（托盘点开、窗口获焦、面板显示时调用；非常便宜）
pub fn maybe_check(app: &tauri::AppHandle) {
    if automatic_disabled().is_some() || ACTIVITY.load(Ordering::Acquire) != IDLE {
        return;
    }
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let last = state.settings.state().read(|s| s.last_update_check_ms);
    let retry_after = RETRY_AFTER_MS.load(Ordering::Acquire);
    if !due(policy(app), fsio::unix_now_ms(), last, retry_after) {
        return;
    }
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        // 结局（含失败）由 check_inner 记一行
        let _ = run_check(&handle, Trigger::Automatic).await;
    });
}

/// 立即检查（设置页「检查更新」，`update_check` 命令）。已有检查在进行时等它结束并返回它的结果
pub async fn check(app: &tauri::AppHandle) -> Result<UpdateStatus, IpcError> {
    run_check(app, Trigger::Manual).await
}

/// 托盘「检查更新」：检查后有新版本 → 打开设置的「关于」；已是最新 / 失败 → 原生对话框说明（见 [`feedback`]）
pub fn check_interactive(app: &tauri::AppHandle) {
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let result = run_check(&handle, Trigger::Manual).await;
        let lang = crate::tray::menu::current_lang(&handle);
        let t = |key| crate::app::i18n::tr(lang, key);
        match feedback(&result) {
            CheckFeedback::OpenAbout => {
                if let Some(state) = handle.try_state::<AppState>() {
                    state
                        .windows
                        .open_settings(Some(crate::app::sections::ABOUT.to_owned()));
                }
            }
            CheckFeedback::UpToDate => {
                let version = handle.package_info().version.to_string();
                show_message(
                    &handle,
                    t("dialog.update.latest.title"),
                    &crate::app::i18n::tr_args(lang, "dialog.update.latest.message", &[("version", &version)]),
                );
            }
            CheckFeedback::Failed => show_message(
                &handle,
                t("dialog.update.failed.title"),
                t("dialog.update.failed.message"),
            ),
        }
    });
}

fn show_message(app: &tauri::AppHandle, title: &str, message: &str) {
    use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
    app.dialog()
        .message(message)
        .title(title)
        .kind(MessageDialogKind::Info)
        .show(|_| {});
}

/// 检查一次。同一时刻只有一个检查：手动检查遇到进行中的检查时等它结束、返回它的结果；自动检查遇到任何进行中的操作都
/// 直接返回当前状态（调用方不看结果）；手动检查遇到下载 / 安装时返回当前状态（`downloading` / `ready`）
async fn run_check(app: &tauri::AppHandle, trigger: Trigger) -> Result<UpdateStatus, IpcError> {
    // 先订阅再抢占：抢占失败时，进行中的那次一定在订阅之后才发布结果，不会错过
    let mut finished = CHECK_DONE.subscribe();
    match ACTIVITY.compare_exchange(IDLE, CHECKING, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => {}
        Err(CHECKING) if trigger == Trigger::Manual => return join_running_check(app, &mut finished).await,
        Err(_) => return status(app),
    }
    let result = check_inner(app, trigger).await;
    ACTIVITY.store(IDLE, Ordering::Release);
    CHECK_DONE.send_replace(Some(result.clone()));
    let status = result?;
    if status.state == UpdateState::Available && follow_up(policy(app), trigger) == FollowUp::AutoInstall {
        let handle = app.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = apply(&handle, Apply::Automatic).await {
                log::warn!("automatic update install failed: {}", e.message);
            }
        });
    }
    Ok(status)
}

/// 手动检查遇到进行中的检查：等它结束，用它的结果。那次若是自动检查且失败了（自动检查失败是静默的），这里替用户把
/// 失败显示出来——这是用户的显式操作
async fn join_running_check(
    app: &tauri::AppHandle,
    finished: &mut watch::Receiver<Option<Result<UpdateStatus, IpcError>>>,
) -> Result<UpdateStatus, IpcError> {
    log::info!("update check trigger=Manual: waiting for the check already in progress");
    let outcome = match finished.changed().await {
        Ok(()) => finished.borrow_and_update().clone(),
        Err(_) => None,
    };
    match outcome {
        Some(Ok(status)) => Ok(status),
        Some(Err(e)) => {
            show_failure(app, &e.message);
            Err(e)
        }
        None => status(app),
    }
}

/// 把检查失败显示出来（`error`，保留已知新版本）；已经开始了新一轮检查时不覆盖它的 `checking`
fn show_failure(app: &tauri::AppHandle, message: &str) {
    let failed = with_known_update(UpdateState::Error, Some(message.to_owned()));
    {
        let mut inner = STATE.lock();
        if inner.status.state == UpdateState::Checking {
            return;
        }
        inner.status = failed;
    }
    publish(app, true);
}

/// 把检查时间写进 state.json（序列化整份 + fsync，放到阻塞线程池）
async fn record_check_time(app: &tauri::AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let store = state.settings.state_arc();
    let now = fsio::unix_now_ms();
    if let Err(e) = blocking(move || store.update(&mut |s| s.last_update_check_ms = now)).await {
        log::warn!("cannot record update check time: {}", e.message);
    }
}

async fn check_inner(app: &tauri::AppHandle, trigger: Trigger) -> Result<UpdateStatus, IpcError> {
    // 已下载待重启 / 正在下载时不再检查
    let previous = STATE.lock().status.clone();
    if matches!(previous.state, UpdateState::Ready | UpdateState::Downloading) {
        return Ok(previous);
    }
    log::info!("update check trigger={trigger:?}");
    set_status(app, with_known_update(UpdateState::Checking, None), true);
    let checked = match app
        .updater_builder()
        .on_before_exit(cleanup_before_installer(app))
        .timeout(REQUEST_TIMEOUT)
        .build()
    {
        Ok(updater) => updater.check().await.map_err(|e| describe(&e)),
        Err(e) => Err(describe(&e)),
    };
    let status = match checked {
        Ok(Some(update)) => {
            log::info!("update available version={}", update.version);
            let status = UpdateStatus {
                state: UpdateState::Available,
                version: Some(update.version.clone()),
                notes: notes_of(&update),
                progress: None,
                error: None,
            };
            STATE.lock().update = Some(update);
            status
        }
        Ok(None) => {
            log::info!("update check: up to date");
            STATE.lock().update = None;
            if OsKind::current() == OsKind::Windows
                && let Some(dir) = staged_dir(app)
            {
                tauri::async_runtime::spawn_blocking(move || prune_staged(&dir, |_, _| true));
            }
            UpdateStatus {
                state: UpdateState::UpToDate,
                ..UpdateStatus::default()
            }
        }
        Err(message) => {
            // 同一次失败只记这一行。自动检查失败是预期内的（离线、还没有任何发布、端点暂时不可用）且对用户静默：info；
            // 手动检查是用户的显式操作：warn
            match trigger {
                Trigger::Automatic => log::info!("update check failed (automatic): {message}"),
                Trigger::Manual => log::warn!("update check failed (manual): {message}"),
            }
            // 失败不算「已检查」：退避一段时间后，下一次惰性检查再试
            let backoff = i64::try_from(FAILURE_BACKOFF.as_millis()).unwrap_or(i64::MAX);
            RETRY_AFTER_MS.store(fsio::unix_now_ms().saturating_add(backoff), Ordering::Release);
            match trigger {
                // 后台检查失败不打扰用户：恢复检查前的状态（已知的新版本仍然显示）
                Trigger::Automatic => set_status(app, previous, true),
                Trigger::Manual => set_status(app, with_known_update(UpdateState::Error, Some(message.clone())), true),
            }
            return Err(IpcError::new(IpcErrorCode::Io, message));
        }
    };
    // 只有成功的检查才计入 24 h
    RETRY_AFTER_MS.store(0, Ordering::Release);
    record_check_time(app).await;
    set_status(app, status.clone(), true);
    Ok(status)
}

/// 下载并安装（设置页「下载并安装」，`update_install` 命令）
pub async fn install(app: &tauri::AppHandle) -> Result<(), IpcError> {
    apply(app, Apply::Manual).await
}

/// 安装的发起方
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Apply {
    /// 用户点了「下载并安装」
    Manual,
    /// `auto_install` 策略下自动检查发现了新版本
    Automatic,
}

/// 下载并安装。自动安装先按 [`auto_plan`] 判断：需要管理员权限等情况下只提示（状态保持 `available`），不下载
async fn apply(app: &tauri::AppHandle, how: Apply) -> Result<(), IpcError> {
    let Some(update) = STATE.lock().update.clone() else {
        return Err(IpcError::new(
            IpcErrorCode::NotReady,
            "no update is available; check for updates first",
        ));
    };
    let plan = match how {
        Apply::Manual => None,
        Apply::Automatic => {
            let package = current_package();
            match auto_plan(OsKind::current(), package, install_target_writable(package)) {
                AutoPlan::NotifyOnly(reason) => {
                    log::info!("automatic update install skipped ({reason}); waiting for the user");
                    return Ok(());
                }
                plan => Some(plan),
            }
        }
    };
    if ACTIVITY
        .compare_exchange(IDLE, APPLYING, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(IpcError::new(
            IpcErrorCode::Conflict,
            "an update check or download is already running",
        ));
    }
    let result = apply_inner(app, &update, plan).await;
    ACTIVITY.store(IDLE, Ordering::Release);
    if let Err(e) = &result {
        set_status(
            app,
            UpdateStatus {
                state: UpdateState::Error,
                version: Some(update.version.clone()),
                notes: notes_of(&update),
                progress: None,
                error: Some(e.message.clone()),
            },
            true,
        );
    }
    result
}

fn ready_status(update: &Update) -> UpdateStatus {
    UpdateStatus {
        state: UpdateState::Ready,
        version: Some(update.version.clone()),
        notes: notes_of(update),
        progress: Some(1.0),
        error: None,
    }
}

async fn apply_inner(app: &tauri::AppHandle, update: &Update, plan: Option<AutoPlan>) -> Result<(), IpcError> {
    let version = update.version.clone();
    if OsKind::current() == OsKind::Windows {
        // 先落盘（本进程里已暂存的同版本安装包直接用）：安装失败时不必重新下载
        let staged = stage(app, update).await?;
        STATE.lock().staged = Some(staged.clone());
        if plan == Some(AutoPlan::InstallWhenIdle) {
            set_status(app, ready_status(update), true);
            log::info!("update {version} staged; it will be installed when Wisp is idle");
            spawn_idle_install(app);
            return Ok(());
        }
        // 用户点了「下载并安装」：立即以 passive 模式安装，安装器启动后进程退出、装完重新打开
        let (handle, update) = (app.clone(), update.clone());
        return blocking(move || install_staged(&handle, update, &staged, true)).await;
    }
    let bytes = download(app, update).await?;
    let installer = update.clone();
    blocking(move || {
        installer
            .install(&bytes)
            .map_err(|e| IpcError::new(IpcErrorCode::Io, describe(&e)))
    })
    .await?;
    log::info!("update installed version={version}; it takes effect after restart");
    set_status(app, ready_status(update), true);
    if plan.is_some() {
        // 自动安装（用户没有点按钮）：发一条通知说明下次启动生效
        notify(app, "notify.update_ready.title", "notify.update_ready.body", &version);
    }
    Ok(())
}

/// 在阻塞线程池里执行
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, IpcError> + Send + 'static) -> Result<T, IpcError> {
    crate::settings::service::run_blocking(f).await
}

/// 下载安装包（验签由插件完成），进度按 1% 节流 emit
async fn download(app: &tauri::AppHandle, update: &Update) -> Result<Vec<u8>, IpcError> {
    let version = update.version.clone();
    let notes = notes_of(update);
    let progress_status = |progress: Option<f32>| UpdateStatus {
        state: UpdateState::Downloading,
        version: Some(version.clone()),
        notes: notes.clone(),
        progress,
        error: None,
    };
    set_status(app, progress_status(Some(0.0)), true);
    log::info!("update download started version={version}");
    let mut received: u64 = 0;
    let mut last_percent: i64 = -1;
    let bytes = update
        .download(
            |chunk, total| {
                received = received.saturating_add(chunk as u64);
                let Some(total) = total.filter(|t| *t > 0) else { return };
                let fraction = (received as f64 / total as f64).clamp(0.0, 1.0);
                // 进度事件按 1% 节流
                let percent = (fraction * 100.0).floor() as i64;
                if percent != last_percent {
                    last_percent = percent;
                    set_status(app, progress_status(Some(fraction as f32)), false);
                }
            },
            || {},
        )
        .await
        .map_err(|e| IpcError::new(IpcErrorCode::Io, describe(&e)))?;
    log::info!("update downloaded version={version} bytes={}", bytes.len());
    Ok(bytes)
}

/// `data_dir/updates/`
fn staged_dir(app: &tauri::AppHandle) -> Option<PathBuf> {
    app.try_state::<AppState>()
        .map(|s| s.settings.paths().data_dir.join(STAGED_DIR))
}

/// Windows：取得落盘的安装包。本进程里已经下载并暂存过同一版本、且文件内容与当时验签的一致时直接用；否则下载（插件验签）
/// 后落盘（其他暂存文件一并清掉）
async fn stage(app: &tauri::AppHandle, update: &Update) -> Result<Staged, IpcError> {
    let dir = staged_dir(app).ok_or_else(|| IpcError::new(IpcErrorCode::NotReady, "app state is not ready"))?;
    let version = update.version.clone();
    let known = STATE.lock().staged.clone().filter(|s| s.version == version);
    if let Some(staged) = known {
        let check = staged.clone();
        if blocking(move || Ok(staged_intact(&check))).await? {
            log::info!("reusing the installer downloaded earlier for {version}");
            return Ok(staged);
        }
    }
    let bytes = download(app, update).await?;
    blocking(move || write_staged(&dir, &version, &bytes).map_err(IpcError::from)).await
}

/// 暂存文件名：`wisp-<版本>-<blake3 十六进制>.update`（版本里只保留 `[0-9A-Za-z.+-]`）。文件名只用于识别与清理，
/// 完整性以内存里的 [`Staged::hash`] 为准
pub fn staged_file_name(version: &str, hash: &fsio::ContentHash) -> String {
    let version: String = version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    format!("wisp-{version}-{hex}.update")
}

/// 解析暂存文件名 → （版本，哈希）
pub fn parse_staged_file_name(name: &str) -> Option<(String, fsio::ContentHash)> {
    let (version, hex) = name.strip_prefix("wisp-")?.strip_suffix(".update")?.rsplit_once('-')?;
    if version.is_empty() || hex.len() != 64 {
        return None;
    }
    let mut hash = [0u8; 32];
    for (i, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some((version.to_owned(), hash))
}

/// 把（已验签的）安装包写到暂存目录（原子写），并删掉其他暂存文件
pub fn write_staged(dir: &Path, version: &str, bytes: &[u8]) -> std::io::Result<Staged> {
    let hash = fsio::content_hash(bytes);
    let path = dir.join(staged_file_name(version, &hash));
    fsio::write_atomic(&path, bytes)?;
    prune_staged(dir, |other, _| other != path);
    Ok(Staged {
        version: version.to_owned(),
        path,
        hash,
    })
}

/// 读出暂存的安装包，内容必须与下载验签时记下的哈希一致（被替换或损坏时为 None）
pub fn read_staged(staged: &Staged) -> Option<Vec<u8>> {
    std::fs::read(&staged.path)
        .ok()
        .filter(|bytes| fsio::content_hash(bytes) == staged.hash)
}

/// 暂存的安装包是否完好（与下载验签时的内容一致）
fn staged_intact(staged: &Staged) -> bool {
    read_staged(staged).is_some()
}

/// 清理暂存目录：删掉 `remove(路径, 版本)` 为 true 的暂存安装包（只认暂存文件名，目录里的其他文件不动）
fn prune_staged(dir: &Path, remove: impl Fn(&Path, &str) -> bool) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for path in entries.filter_map(Result::ok).map(|e| e.path()) {
        let Some((version, _)) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(parse_staged_file_name)
        else {
            continue;
        };
        if remove(&path, &version)
            && let Err(e) = std::fs::remove_file(&path)
        {
            log::debug!("cannot remove staged installer {}: {e}", path.display());
        }
    }
}

/// 安装落盘的安装包（Windows；**阻塞**）。内容与下载验签时不一致（被替换 / 损坏）时拒绝并删掉它。成功时安装器启动、
/// 插件直接 `process::exit(0)`，不会返回
fn install_staged(app: &tauri::AppHandle, update: Update, staged: &Staged, restart: bool) -> Result<(), IpcError> {
    let Some(bytes) = read_staged(staged) else {
        log::warn!(
            "staged installer {} was modified or damaged; discarding it",
            staged.path.display()
        );
        let _ = std::fs::remove_file(&staged.path);
        STATE.lock().staged = None;
        return Err(IpcError::new(
            IpcErrorCode::Io,
            "the downloaded installer was modified or damaged; download it again",
        ));
    };
    log::info!("installing update version={} restart={restart}", staged.version);
    if let Some(state) = app.try_state::<AppState>() {
        state.settings.flush();
    }
    update
        .restart_after_install(restart)
        .install(&bytes)
        .map_err(|e| IpcError::new(IpcErrorCode::Io, describe(&e)))
}

/// Windows 安装器启动、进程退出之前：在主线程上 `cleanup_before_exit`（移除托盘图标等；tray-icon 的对象只能在主线程上
/// 销毁）。install 可能在主线程（退出时安装）或阻塞线程上执行：在主线程上直接清理，否则派发过去并最多等 2 s
fn cleanup_before_installer(app: &tauri::AppHandle) -> impl Fn() + Send + Sync + 'static {
    let app = app.clone();
    move || {
        // 离线模型的推理进程先停掉：安装器替换文件时它们不能还开着，Wisp 退出后也不能留下孤儿
        if let Some(state) = app.try_state::<crate::state::AppState>() {
            state.models.shutdown();
        }
        if crate::app::runtime::on_main_thread() {
            app.cleanup_before_exit();
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = app.clone();
        let scheduled = app.run_on_main_thread(move || {
            handle.cleanup_before_exit();
            let _ = tx.send(());
        });
        if scheduled.is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(2));
        }
    }
}

/// 有没有 Wisp 窗口可见（查询失败按可见处理；`is_visible` 要与主线程同步往返，调用方放到阻塞线程池）
fn any_window_visible(app: &tauri::AppHandle) -> bool {
    app.webview_windows().values().any(|w| w.is_visible().unwrap_or(true))
}

/// 有动静（窗口获焦 / 失焦 / 销毁、托盘点开、更新策略变化）：等待空闲安装的任务从头计时。没有在等待时只是一次原子读，
/// 可以在高频路径上调用
pub fn note_activity() {
    if IDLE_WAITER.load(Ordering::Acquire) {
        IDLE_NUDGE.notify_one();
    }
}

/// 等下一次动静：[`note_activity`] 或一次新的埋点（触发、面板显示……）
async fn next_activity(triggers: &mut Option<watch::Receiver<u64>>) {
    let closed = match triggers.as_mut() {
        Some(rx) => tokio::select! {
            () = IDLE_NUDGE.notified() => false,
            changed = rx.changed() => changed.is_err(),
        },
        None => {
            IDLE_NUDGE.notified().await;
            false
        }
    };
    if closed {
        // 埋点表不在了（只会在退出时发生）：之后只等窗口事件
        *triggers = None;
        IDLE_NUDGE.notified().await;
    }
}

/// 空闲安装下一步要做的事（纯函数）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleStep {
    /// 安装
    Install,
    /// 有窗口可见，还有复查次数：等下一次动静，最多等 [`IDLE_RECHECK`]
    WaitOrRecheck,
    /// 有窗口可见，复查次数已用完：只等下一次动静（或退出时安装）
    WaitForActivity,
}

/// 安静期满之后的判断（纯函数）
pub fn idle_step(window_visible: bool, rechecks_left: u32) -> IdleStep {
    match (window_visible, rechecks_left) {
        (false, _) => IdleStep::Install,
        (true, 0) => IdleStep::WaitForActivity,
        (true, _) => IdleStep::WaitOrRecheck,
    }
}

/// Windows 自动安装：**事件驱动地**等到空闲（最后一次动静之后安静 [`IDLE_QUIET`]、且没有可见窗口）时安装并重启。
///
/// 动静 = 新的埋点（触发、面板显示……，`PerfMarks::subscribe`）或 [`note_activity`]（窗口获焦 / 失焦 / 销毁、托盘点开）。
/// 安静期是一次性定时，有动静就从头计时；期满时仍有窗口可见（正在看结果、固定窗口常驻）就不再定时，等下一次动静——
/// 隐藏面板不一定产生窗口事件，所以另有 [`IDLE_RECHECKS`] 次、每次 [`IDLE_RECHECK`] 的兜底复查（总数有上限）。
/// 策略不再是自动安装、或暂存被清掉时停止等待（之后由用户重启 / 点按钮，或退出时安装）
fn spawn_idle_install(app: &tauri::AppHandle) {
    if IDLE_WAITER.swap(true, Ordering::AcqRel) {
        note_activity();
        return;
    }
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut triggers = handle.try_state::<AppState>().map(|s| s.perf.subscribe());
        let mut rechecks_left = IDLE_RECHECKS;
        loop {
            // 1. 安静期（一次性定时；期间有动静就从头计时）
            loop {
                tokio::select! {
                    () = tokio::time::sleep(IDLE_QUIET) => break,
                    () = next_activity(&mut triggers) => {}
                }
            }
            // 2. 还要不要装
            let (staged, update) = {
                let inner = STATE.lock();
                (inner.staged.clone(), inner.update.clone())
            };
            let (Some(staged), Some(update)) = (staged, update) else {
                break;
            };
            if policy(&handle) != UpdatePolicy::AutoInstall {
                log::info!("update {}: no longer installing automatically", staged.version);
                break;
            }
            // 3. 有窗口可见：不轮询，等下一次动静（兜底复查次数有限）
            let h = handle.clone();
            let visible = tauri::async_runtime::spawn_blocking(move || any_window_visible(&h))
                .await
                .unwrap_or(true);
            match idle_step(visible, rechecks_left) {
                IdleStep::Install => {}
                IdleStep::WaitOrRecheck => {
                    rechecks_left -= 1;
                    tokio::select! {
                        () = tokio::time::sleep(IDLE_RECHECK) => {}
                        () = next_activity(&mut triggers) => {}
                    }
                    continue;
                }
                IdleStep::WaitForActivity => {
                    next_activity(&mut triggers).await;
                    continue;
                }
            }
            // 4. 安装（有检查 / 下载正在进行时等下一个安静期）
            if ACTIVITY
                .compare_exchange(IDLE, APPLYING, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue;
            }
            notify(
                &handle,
                "notify.update_installing.title",
                "notify.update_installing.body",
                &staged.version,
            );
            let h = handle.clone();
            let result = blocking(move || install_staged(&h, update, &staged, true)).await;
            ACTIVITY.store(IDLE, Ordering::Release);
            // 成功时进程已经退出；走到这里就是失败了
            if let Err(e) = result {
                log::warn!("automatic update install failed: {}", e.message);
                STATE.lock().staged = None;
                set_status(&handle, with_known_update(UpdateState::Error, Some(e.message)), true);
            }
            break;
        }
        IDLE_WAITER.store(false, Ordering::Release);
    });
}

fn notify(app: &tauri::AppHandle, title_key: &'static str, body_key: &'static str, version: &str) {
    use tauri_plugin_notification::NotificationExt;
    let lang = crate::tray::menu::current_lang(app);
    if let Err(e) = app
        .notification()
        .builder()
        .title(crate::app::i18n::tr(lang, title_key))
        .body(crate::app::i18n::tr_args(lang, body_key, &[("version", version)]))
        .show()
    {
        log::debug!("update notification failed: {e}");
    }
}

/// 记下本次退出由 `request_restart` 发起（`RunEvent::ExitRequested` 的 code 为 `RESTART_EXIT_CODE`）
pub fn note_restart_requested() {
    RESTART_REQUESTED.store(true, Ordering::Release);
}

/// 退出时（`RunEvent::Exit`，主线程）：Windows 上有本进程暂存的安装包，且这次是重启、或策略仍是自动安装时，启动安装器
/// （重启 → 装完重新打开；退出 → 只安装）。安装器启动后进程直接退出
pub fn on_exit(app: &tauri::AppHandle) {
    if OsKind::current() != OsKind::Windows {
        return;
    }
    let (staged, update) = {
        let inner = STATE.lock();
        (inner.staged.clone(), inner.update.clone())
    };
    let (Some(staged), Some(update)) = (staged, update) else {
        return;
    };
    let restart = RESTART_REQUESTED.load(Ordering::Acquire);
    if !restart && policy(app) != UpdatePolicy::AutoInstall {
        return;
    }
    if let Err(e) = install_staged(app, update, &staged, restart) {
        log::warn!("staged update install failed: {}", e.message);
    }
}

/// 更新策略变化：改为 off 时清掉「有新版本」的提示；从 off 打开时按 24 h 规则检查。等待空闲安装的任务随之重新判断
pub fn on_policy_changed(app: &tauri::AppHandle, policy: UpdatePolicy) {
    if policy == UpdatePolicy::Off {
        let mut inner = STATE.lock();
        if matches!(
            inner.status.state,
            UpdateState::Available | UpdateState::UpToDate | UpdateState::Error
        ) {
            inner.status = UpdateStatus::default();
            inner.update = None;
        }
    }
    publish(app, true);
    note_activity();
    if policy != UpdatePolicy::Off {
        maybe_check(app);
    }
}

/// 更新器错误 → 英文简述（不含 URL 的 query；endpoint 本身不带凭据）
fn describe(e: &tauri_plugin_updater::Error) -> String {
    let text = e.to_string();
    let text: String = text.chars().take(200).collect();
    crate::app::logging::redact(&text).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 24 * 60 * 60 * 1000;

    /// 引导窗口获焦时的惰性检查刚成功 / 刚失败，启动 30 s 的那次不再重复联网
    #[test]
    fn the_startup_check_skips_when_a_check_just_happened() {
        let now = 1_760_000_000_000;
        let min = 60 * 1000;
        assert!(!checked_recently(now, 0, 0), "从未检查过：照常检查");
        assert!(checked_recently(now, now - 30 * 1000, 0), "30 s 前刚成功检查过");
        assert!(checked_recently(now, now - 9 * min, 0));
        assert!(!checked_recently(now, now - 10 * min, 0), "节流窗口之外：照常检查");
        assert!(!checked_recently(now, now - DAY, 0));
        assert!(checked_recently(now, 0, now + 59 * min), "刚失败、还在退避期内");
        assert!(!checked_recently(now, 0, now - 1), "退避期已过");
        assert!(!checked_recently(now, now + 5 * min, 0), "时钟回拨：不当作刚检查过");
    }

    #[test]
    fn automatic_checks_are_due_every_24_hours() {
        let now = 1_760_000_000_000;
        assert!(due(UpdatePolicy::Notify, now, 0, 0), "从未检查过");
        assert!(!due(UpdatePolicy::Notify, now, now - DAY + 1, 0));
        assert!(due(UpdatePolicy::Notify, now, now - DAY, 0));
        assert!(due(UpdatePolicy::AutoInstall, now, now - 2 * DAY, 0));
        assert!(
            due(UpdatePolicy::Notify, now, now + DAY, 0),
            "系统时间被调回过去时重新检查"
        );
        assert!(!due(UpdatePolicy::Off, now, 0, 0), "关闭后不再检查");
    }

    #[test]
    fn failed_checks_back_off_without_counting_as_checked() {
        let now = 1_760_000_000_000;
        let hour = 60 * 60 * 1000;
        // 开机时没联网：上次成功检查是两天前，失败后 1 h 内不再试，之后再试
        let retry_after = now + hour;
        assert!(!due(UpdatePolicy::Notify, now, now - 2 * DAY, retry_after));
        assert!(!due(UpdatePolicy::Notify, now + hour - 1, now - 2 * DAY, retry_after));
        assert!(due(UpdatePolicy::Notify, now + hour, now - 2 * DAY, retry_after));
    }

    #[test]
    fn only_automatic_checks_install_automatically() {
        assert_eq!(
            follow_up(UpdatePolicy::AutoInstall, Trigger::Automatic),
            FollowUp::AutoInstall
        );
        assert_eq!(follow_up(UpdatePolicy::AutoInstall, Trigger::Manual), FollowUp::Notify);
        assert_eq!(follow_up(UpdatePolicy::Notify, Trigger::Automatic), FollowUp::Notify);
        assert_eq!(follow_up(UpdatePolicy::Off, Trigger::Manual), FollowUp::Notify);
    }

    fn status_of(state: UpdateState) -> Result<UpdateStatus, IpcError> {
        Ok(UpdateStatus {
            state,
            ..UpdateStatus::default()
        })
    }

    #[test]
    fn intermediate_states_are_never_reported_as_up_to_date() {
        assert_eq!(feedback(&status_of(UpdateState::UpToDate)), CheckFeedback::UpToDate);
        for state in [
            UpdateState::Available,
            UpdateState::Downloading,
            UpdateState::Ready,
            UpdateState::Checking,
        ] {
            assert_eq!(feedback(&status_of(state)), CheckFeedback::OpenAbout, "{state:?}");
        }
        assert_eq!(feedback(&status_of(UpdateState::Error)), CheckFeedback::Failed);
        assert_eq!(
            feedback(&Err(IpcError::new(IpcErrorCode::Io, "offline"))),
            CheckFeedback::Failed
        );
    }

    /// 手动检查遇到进行中的检查：订阅在抢占之前，进行中的那次结束时发布的结果一定能收到（不会拿到中间态）
    #[test]
    fn a_manual_check_joins_the_running_check_and_gets_its_outcome() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let done = watch::Sender::<Option<Result<UpdateStatus, IpcError>>>::new(None);
            let mut waiter = done.subscribe();
            let join = tokio::spawn(async move {
                waiter.changed().await.expect("sender alive");
                waiter.borrow_and_update().clone()
            });
            tokio::task::yield_now().await;
            done.send_replace(Some(Err(IpcError::new(IpcErrorCode::Io, "timed out"))));
            let outcome = join.await.expect("join").expect("outcome");
            assert_eq!(
                feedback(&outcome),
                CheckFeedback::Failed,
                "离线时是「检查失败」而不是「已是最新」"
            );
        });
    }

    #[test]
    fn idle_install_does_not_poll_while_windows_are_visible() {
        assert_eq!(idle_step(false, IDLE_RECHECKS), IdleStep::Install);
        assert_eq!(idle_step(false, 0), IdleStep::Install);
        assert_eq!(idle_step(true, 2), IdleStep::WaitOrRecheck);
        assert_eq!(idle_step(true, 0), IdleStep::WaitForActivity, "复查次数用完后只等事件");
        // 兜底复查总数有上限：最多 IDLE_RECHECKS 次定时唤醒
        let mut left = IDLE_RECHECKS;
        let mut timers = 0;
        while idle_step(true, left) == IdleStep::WaitOrRecheck {
            left -= 1;
            timers += 1;
        }
        assert_eq!(timers, IDLE_RECHECKS);
    }

    #[test]
    fn automatic_install_never_needs_a_password() {
        use AutoPlan::{InstallNow, InstallWhenIdle, NotifyOnly};
        assert_eq!(auto_plan(OsKind::Windows, Package::Nsis, false), InstallWhenIdle);
        assert_eq!(auto_plan(OsKind::Macos, Package::MacApp, true), InstallNow);
        assert_eq!(auto_plan(OsKind::Linux, Package::AppImage, true), InstallNow);
        // 需要管理员权限：只提示
        assert!(matches!(
            auto_plan(OsKind::Macos, Package::MacApp, false),
            NotifyOnly(_)
        ));
        assert!(matches!(
            auto_plan(OsKind::Linux, Package::AppImage, false),
            NotifyOnly(_)
        ));
        assert!(matches!(auto_plan(OsKind::Linux, Package::Deb, true), NotifyOnly(_)));
        assert!(matches!(auto_plan(OsKind::Linux, Package::Rpm, true), NotifyOnly(_)));
        assert!(matches!(auto_plan(OsKind::Windows, Package::Msi, true), NotifyOnly(_)));
        // 开发构建 / 便携运行
        for os in [OsKind::Macos, OsKind::Windows, OsKind::Linux] {
            assert!(matches!(auto_plan(os, Package::Unknown, true), NotifyOnly(_)));
        }
    }

    #[test]
    fn staged_file_names_round_trip() {
        let hash = fsio::content_hash(b"installer");
        let name = staged_file_name("0.2.0-beta.1", &hash);
        assert!(name.starts_with("wisp-0.2.0-beta.1-") && name.ends_with(".update"));
        assert_eq!(parse_staged_file_name(&name), Some(("0.2.0-beta.1".to_owned(), hash)));
        // 版本里的路径字符被替换，不会逃出暂存目录
        assert!(!staged_file_name("../../x", &hash).contains('/'));
        for junk in [
            "wisp-0.2.0.update",
            "wisp--abc.update",
            "other-0.2.0-00.update",
            "wisp-0.2.0-zz.update",
        ] {
            assert_eq!(parse_staged_file_name(junk), None, "{junk}");
        }
    }

    #[test]
    fn staged_installers_are_verified_against_the_downloaded_hash_and_pruned() {
        let dir = tempfile::tempdir().expect("tempdir");
        let old = write_staged(dir.path(), "0.1.9", b"old installer").expect("old");
        let staged = write_staged(dir.path(), "0.2.0", b"new installer").expect("stage");
        assert!(!old.path.exists(), "落盘新版本时清掉其他版本");
        assert_eq!(read_staged(&staged).as_deref(), Some(&b"new installer"[..]));
        // 被别的进程替换：文件名照旧、哈希也自洽，但与下载验签时记在内存里的不一致——拒绝
        let forged = b"malicious installer";
        let forged_path = dir.path().join(staged_file_name("0.2.0", &fsio::content_hash(forged)));
        std::fs::write(&forged_path, forged).expect("forge");
        std::fs::rename(&forged_path, &staged.path).expect("replace");
        assert_eq!(read_staged(&staged), None);
        assert!(!staged_intact(&staged));
        // 另一个进程放进来的「新版本」文件：本进程不认识它（不会复用），启动时全部清掉；目录里的其他文件不动
        let planted = dir.path().join(staged_file_name("9.9.9", &fsio::content_hash(forged)));
        std::fs::write(&planted, forged).expect("plant");
        std::fs::write(dir.path().join("notes.txt"), b"keep").expect("other");
        prune_staged(dir.path(), |_, _| true);
        assert!(!planted.exists() && !staged.path.exists());
        assert!(dir.path().join("notes.txt").exists());
    }

    #[test]
    fn write_test_probe_leaves_nothing_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(dir_writable(dir.path()));
        assert_eq!(std::fs::read_dir(dir.path()).expect("read_dir").count(), 0);
        assert!(!dir_writable(&dir.path().join("missing")));
    }
}
