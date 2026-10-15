//! 多设备同步的桌面端服务（[`SyncService`]，放在 `AppState.sync`）。
//!
//! 经用户自己的 WebDAV 存储同步配置、API Key、剪贴板与识别历史；同步引擎在 `wisp_sync`，这里负责接入应用：
//!
//! - **关闭时零成本**：构造不做任何 I/O；没开启同步时不打开 sync.db、不建 HTTP 客户端、不装钩子、不起线程、不读钥匙串。
//!   唯一常驻的是一个跟随设置变化的任务（只在设置变化时被唤醒，比较一下 `settings.sync`）。
//! - **开启后只由事件驱动**：本机改动经钩子与 sink 捕获（[`capture`]），由日志写入线程写进 sync.db（[`writer`]）；
//!   上传由「有待上传的改动」时的一次性去抖发起，取回由用户活动触发（面板、托盘、设置窗口、再次启动），节流与退避都只是
//!   原子读写（[`gate`]），没有任何周期定时器。
//! - **钥匙串**：同步凭据只在用户活动触发的一轮里第一次读取，之后缓存在内存；模型服务的 Key 也只在这类轮次里读写
//!   （本进程成功访问过一次之后，本机改动触发的上传也可以读）。
//! - **隐私**：日志与错误里只有阶段、计数、HTTP 状态码与错误种类；不出现地址、账号、密码、同步密码、密钥、远端路径、
//!   设备名或任何记录内容。
//!
//! # 接入（给装配代码）
//!
//! - 构造：[`SyncService::new`]，放进 `AppState.sync`；托盘就绪后调用 [`SyncService::start_local`]（只做本机工作，
//!   不联网、不读钥匙串）；`RunEvent::Exit` 里调用 [`SyncService::shutdown_flush`]（只写本地，≤ 200 ms）。
//! - 触发：在面板显示（[`CycleReason::ClipboardShown`] / [`CycleReason::PanelShown`]）、托盘打开
//!   （[`CycleReason::TrayOpened`]）、设置窗口获焦（[`CycleReason::SettingsFocused`]）、再次启动
//!   （[`CycleReason::Relaunch`]）处调用 [`SyncService::trigger`]：O(1)，可以放在热路径上，节流与退避在里面判断。
//! - 状态：[`SyncService::status`]（`RuntimeInfo.sync`、`sync_status`）；[`SyncService::subscribe_status`] 的每次变化
//!   对应一次 `sync:status` 事件与托盘刷新（状态转换，以及节流后的进度）。托盘用 [`SyncService::is_enabled`]
//!   决定是否显示「立即同步」，用 [`SyncService::needs_attention`] 决定是否显示「同步需要处理」。
//! - 命令：`sync_now` → [`SyncService::sync_now`]；`sync_devices` → [`SyncService::devices`]；`sync_probe` →
//!   [`SyncService::probe`]；`sync_connect` → [`SyncService::connect`]；`sync_disconnect` → [`SyncService::disconnect`]；
//!   `sync_reset_remote` → [`SyncService::reset_remote`]；`sync_generate_passphrase` →
//!   [`SyncService::generate_passphrase`]。连接、断开、清除之后 `settings.sync` 已经写好（`settings:changed` 照常广播）。

pub mod capture;
pub mod errors;
pub mod gate;
pub mod identity;
pub mod local;
pub mod reconcile;
pub mod writer;

#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Weak};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use parking_lot::{Mutex, RwLock};
use tauri::Manager;
use tokio::sync::{Notify, watch};
use wisp_sync::{
    ConnectOutcome, ContentSwitches, CycleOptions, CycleReport, DeviceInfo, Domain, Engine, Journal, LocalError,
    MasterKey, Progress, ProgressFn, Replica, RetentionPolicy, SyncError, WebDav,
};
use wisp_types::{
    OsKind, SyncAccount, SyncConnectOutcome, SyncConnectResult, SyncContent, SyncDevice, SyncErrorKind, SyncPhase,
    SyncProbe, SyncProgress, SyncSettings, SyncStatus, SyncVaultInfo,
};

pub use wisp_sync::CycleReason;

use self::capture::{Capturer, Captures, Msg};
use self::gate::{Debounce, Gate, Queue, clock_ms};
use self::identity::now_ms;
use self::local::{ClipboardSource, DesktopStore};
use self::writer::{Writer, WriterCtx, describe};
use crate::clipboard_svc::{ClipSyncSink, ClipboardService};
use crate::error::IpcError;
use crate::settings::SettingsService;
use crate::settings::service::{ConfigChange, run_blocking};
use crate::settings::sync_credential::SyncCredential;
use crate::state::{AppState, Deps};

/// 本机副本（与 clipboard.db 同在数据目录）
const SYNC_DB: &str = "sync.db";
/// 同步的临时目录（`spool/`：下载解密后、交给本机存储之前的文件）
const SYNC_DIR: &str = "sync";
/// 退出时等日志写入线程写完的上限（只写本地）
const EXIT_FLUSH: Duration = Duration::from_millis(200);
/// 每轮开始前等日志写入线程写完的上限
const CYCLE_FLUSH: Duration = Duration::from_millis(200);
/// 读写钥匙串的上限（macOS 可能弹出授权框，等用户点）
const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(120);
/// 读写副本等本地阻塞操作的上限
const LOCAL_TIMEOUT: Duration = Duration::from_secs(30);
/// 进度广播的最小间隔
const PROGRESS_EVERY_MS: i64 = 200;
/// 断开时删除本机副本的重试（Windows 上刚关闭的文件可能还被占用一小会儿）
const REMOVE_ATTEMPTS: u32 = 10;
const REMOVE_GAP: Duration = Duration::from_millis(200);

/// 同步开启期间的全部资源。关闭时整体释放
struct Active {
    engine: Arc<Engine>,
    store: Arc<DesktopStore>,
    capturer: Arc<Capturer>,
    captures: Arc<Captures>,
    journal: Journal,
    /// 写入线程的通道（核对置顶时补发记录用）
    tx: Sender<Msg>,
    writer: Writer,
    /// 剪贴板 sink 是否已安装
    sink: AtomicBool,
}

/// 运行循环里保留的 WebDAV 客户端：连续的几轮共用，空闲后释放
struct RemoteSlot {
    tag: [u8; 32],
    remote: WebDav,
}

/// 多设备同步服务
pub struct SyncService {
    settings: Arc<SettingsService>,
    app: Option<tauri::AppHandle>,
    app_version: String,
    gate: Gate,
    queue: Queue,
    debounce: Debounce,
    /// 有一个运行循环在处理排队的触发
    running: AtomicBool,
    active: RwLock<Option<Arc<Active>>>,
    /// 串行化开启 / 关闭、连接、断开、清除
    lifecycle: tokio::sync::Mutex<()>,
    /// 一轮同步期间持有：关闭时等它退出
    cycle_lock: tokio::sync::Mutex<()>,
    /// 关闭时打断进行中的一轮
    cancel: Notify,
    generation: AtomicU64,
    /// 同步凭据（第一次在用户活动里读出后缓存；drop 时清零）
    creds: Mutex<Option<SyncCredential>>,
    /// 本进程已经在一轮用户活动里成功读写过钥匙串
    keychain_ok: AtomicBool,
    /// 剪贴板置顶状态已核对过
    pins_checked: AtomicBool,
    status: watch::Sender<SyncStatus>,
    progress_ms: AtomicI64,
    started: AtomicBool,
    /// 上一次处理过的 `settings.sync`（持有 `lifecycle` 时读写）：连接、断开写入的设置在这里直接记下，跟随任务不再重复处理
    seen: Mutex<Option<SyncSettings>>,
}

impl std::fmt::Debug for SyncService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncService")
            .field("enabled", &self.gate.enabled())
            .finish_non_exhaustive()
    }
}

impl SyncService {
    /// 构造（不做任何 I/O）
    pub fn new(d: &Deps) -> Arc<Self> {
        Self::with_parts(
            Arc::clone(&d.settings),
            Some(d.app.clone()),
            d.app.package_info().version.to_string(),
        )
    }

    pub(crate) fn with_parts(
        settings: Arc<SettingsService>,
        app: Option<tauri::AppHandle>,
        app_version: String,
    ) -> Arc<Self> {
        let initial = if settings.get().sync.enabled {
            SyncStatus {
                phase: SyncPhase::Idle,
                ..SyncStatus::off()
            }
        } else {
            SyncStatus::off()
        };
        Arc::new(Self {
            settings,
            app,
            app_version,
            gate: Gate::default(),
            queue: Queue::default(),
            debounce: Debounce::default(),
            running: AtomicBool::new(false),
            active: RwLock::new(None),
            lifecycle: tokio::sync::Mutex::new(()),
            cycle_lock: tokio::sync::Mutex::new(()),
            cancel: Notify::new(),
            generation: AtomicU64::new(0),
            creds: Mutex::new(None),
            keychain_ok: AtomicBool::new(false),
            pins_checked: AtomicBool::new(false),
            status: watch::Sender::new(initial),
            progress_ms: AtomicI64::new(0),
            started: AtomicBool::new(false),
            seen: Mutex::new(None),
        })
    }

    // ================================================================================================
    // 对外接口
    // ================================================================================================

    /// 托盘就绪后调用一次：开始跟随 `settings.sync` 的变化；同步已开启时在后台打开 sync.db、安装钩子、做启动核对。
    /// 不联网、不读钥匙串——第一次联网发生在第一次用户活动触发时
    pub fn start_local(self: &Arc<Self>) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let svc = Arc::clone(self);
        let mut rx = self.settings.subscribe();
        rx.borrow_and_update();
        tauri::async_runtime::spawn(async move {
            svc.follow_settings().await;
            while rx.changed().await.is_ok() {
                rx.borrow_and_update();
                svc.follow_settings().await;
            }
        });
    }

    /// 退出时（`RunEvent::Exit`，主线程）：把日志写入线程队列里的本机改动写进 sync.db，最多等 200 ms，不联网
    pub fn shutdown_flush(&self) {
        if let Some(active) = self.active()
            && !active.writer.flush(EXIT_FLUSH)
        {
            log::warn!("sync: local changes were still being written at exit");
        }
    }

    /// 发起一轮同步（O(1)，可以在热路径上调用）。没有开启同步、关闭了自动同步、在节流间隔或退避期内、或因需要处理的
    /// 错误暂停时什么也不做；「立即同步」与连接后的首轮不受这些限制。正在同步时合并成「结束后再来一轮」
    pub fn trigger(self: &Arc<Self>, reason: CycleReason) {
        if !self.gate.allows(reason, clock_ms()) {
            return;
        }
        self.queue.push(reason);
        self.kick();
    }

    /// 「立即同步」：解除暂停与退避，发起（或并入正在进行的）一轮，立即返回当前状态；进度经状态订阅
    pub fn sync_now(self: &Arc<Self>) -> Result<SyncStatus, IpcError> {
        if !self.settings.get().sync.enabled {
            return Err(errors::not_connected());
        }
        if !self.gate.enabled() {
            if self.active().is_some() {
                return Err(errors::not_connected());
            }
            // 设置里开着同步、本机部分却没有开启（启动时 sync.db 打不开、磁盘满等）：界面这时请用户重试，「立即同步」
            // 就是重试——先在后台再开启一次，成功了再同步
            let svc = Arc::clone(self);
            tauri::async_runtime::spawn(async move {
                {
                    let _life = svc.lifecycle.lock().await;
                    if svc.active().is_none() && svc.settings.get().sync.enabled {
                        svc.enable_locked().await;
                    }
                }
                if svc.gate.enabled() {
                    svc.gate.resume();
                    svc.trigger(CycleReason::Manual);
                }
            });
            return Ok(self.status());
        }
        self.gate.resume();
        self.trigger(CycleReason::Manual);
        Ok(self.status())
    }

    /// 当前状态（不含地址、账号与设备名）
    pub fn status(&self) -> SyncStatus {
        self.status.borrow().clone()
    }

    /// 订阅状态变化（状态转换与节流后的进度）
    pub fn subscribe_status(&self) -> watch::Receiver<SyncStatus> {
        self.status.subscribe()
    }

    /// 同步是否开启并已就绪（托盘菜单据此显示「立即同步」）
    pub fn is_enabled(&self) -> bool {
        self.gate.enabled()
    }

    /// 是否有需要用户处理的错误（托盘状态行「同步需要处理」）
    pub fn needs_attention(&self) -> bool {
        let status = self.status.borrow();
        status.phase == SyncPhase::Error && !status.transient && status.error.is_some_and(SyncErrorKind::is_persistent)
    }

    /// 已知的设备（来自最近读到的各设备信息），不联网。同步未开启时为空
    pub async fn devices(&self) -> Result<Vec<SyncDevice>, IpcError> {
        let Some(active) = self.active() else {
            return Ok(Vec::new());
        };
        let list = run_blocking(move || {
            active
                .engine
                .devices()
                .map_err(|_| IpcError::internal("sync device list unavailable"))
        })
        .await?;
        Ok(list
            .into_iter()
            .map(|d| SyncDevice {
                id: d.id,
                name: d.name,
                os: d.os,
                app_version: d.app_version,
                last_seen_ms: d.last_seen_ms,
                this_device: d.this_device,
            })
            .collect())
    }

    /// 「测试连接」：认证、建目录、写入并删除探测文件、读取服务器上已有数据的概况。`password = None` 时用已保存的、与这个
    /// 地址和账号匹配的密码。传输与服务器的错误放进报告，不抛出；地址不合法时抛 `invalid_argument`
    pub async fn probe(&self, account: SyncAccount, password: Option<String>) -> Result<SyncProbe, IpcError> {
        let (server_url, username, _) = check_account(&account)?;
        let password = match password {
            Some(p) => p,
            None => match self.stored_password(&server_url, &username).await? {
                Some(p) => p,
                None => return Ok(probe_failed(SyncErrorKind::CredentialsMissing, None)),
            },
        };
        let remote = match WebDav::new(&server_url, &username, &password) {
            Ok(r) => r,
            Err(e) => return Ok(probe_failed(errors::kind_of(e.kind()), e.detail())),
        };
        let db = self.db_path();
        let local_vault = blocking(LOCAL_TIMEOUT, move || Replica::peek_vault_id(&db).ok().flatten())
            .await
            .flatten();
        match wisp_sync::probe(&remote, local_vault.as_deref()).await {
            Ok(report) => Ok(SyncProbe {
                ok: true,
                error: None,
                error_detail: None,
                vault: report.vault.map(|v| SyncVaultInfo {
                    supported: v.supported,
                    device_count: v.device_count,
                    last_modified_ms: v.last_modified_ms,
                    restorable: v.restorable,
                }),
            }),
            Err(e) => {
                log::info!("sync: connection test failed ({})", describe(&e));
                Ok(probe_failed(errors::kind_of(e.kind()), e.detail()))
            }
        }
    }

    /// 连接：按服务器与本机的情况创建、加入或恢复。完成解锁 / 创建、保存凭据、配置类内容的加入与首次应用后返回；剪贴板与
    /// 识别历史随后在后台继续（状态带进度）。`password = None`（更新密码时留空）用已保存的密码。
    ///
    /// 同步密码不对抛 `conflict`（`errors.sync.passphrase`），不满足规则抛 `invalid_argument`
    /// （`errors.sync.passphrase_weak` / `passphrase_reused`）；其余失败的消息是设置页状态行同一套原因文案的 key
    pub async fn connect(
        self: &Arc<Self>,
        account: SyncAccount,
        password: Option<String>,
        passphrase: String,
        content: SyncContent,
    ) -> Result<SyncConnectResult, IpcError> {
        let _life = self.lifecycle.try_lock().map_err(|_| errors::busy())?;
        let (server_url, username, host) = check_account(&account)?;
        let password = match password {
            Some(p) => p,
            None => self
                .stored_password(&server_url, &username)
                .await?
                .ok_or_else(errors::credentials_missing)?,
        };
        // 不联网，在开启任何东西之前构造：失败时没有要撤回的
        let remote = WebDav::new(&server_url, &username, &password).map_err(|e| errors::ipc_error(&e))?;
        let prior = self.settings.get().sync.clone();
        let prior_status = self.status();
        // 同步内容只属于本机：先写进设置（连接前可以调整），开启时按它安装剪贴板 sink
        self.write_sync(move |s| s.content = content).await?;
        self.mark_seen();
        let fresh_db = !self.db_path().exists();
        let active = self.ensure_active().await.map_err(|e| errors::ipc_error(&e))?;
        if prior.enabled && prior.content != content {
            self.apply_content(&active, &prior.content, &content).await;
        }
        // 已开启时（更新密码）可能正有一轮在用旧凭据：等它结束，连接期间也不开始新的一轮
        let cycle = self.cycle_lock.lock().await;
        let options = self.options(true, content);
        active.store.set_keychain_allowed(true);
        self.set_status(|s| {
            s.phase = SyncPhase::Syncing;
            s.progress = None;
        });
        let progress = self.progress_fn();
        // 与每一轮同步一样：先把排队的本机改动写进副本，连接期间才写进来的改动在应用远端值时跳过
        let flushing = Arc::clone(&active);
        let _ = blocking(CYCLE_FLUSH * 2, move || flushing.writer.flush(CYCLE_FLUSH)).await;
        active.captures.begin_cycle();
        let result = {
            let forbidden = [password.as_str(), username.as_str(), host.as_str()];
            active
                .engine
                .connect(&remote, &active.store, &passphrase, &forbidden, &options, &*progress)
                .await
        };
        active.captures.end_cycle();
        active.store.set_keychain_allowed(false);
        drop(remote);
        let connected = match result {
            Ok(c) => c,
            Err(e) => {
                drop(cycle);
                log::info!("sync: connect failed ({})", describe(&e));
                if prior.enabled {
                    self.status.send_replace(prior_status);
                } else {
                    // 没有连上：不留下任何东西（本次新建的副本删掉；之前就有的副本留着，之后还能恢复）
                    self.abandon_first_connect(active, fresh_db).await;
                }
                return Err(errors::ipc_error(&e));
            }
        };
        let outcome = match connected.outcome {
            ConnectOutcome::Created => SyncConnectOutcome::Created,
            ConnectOutcome::Joined => SyncConnectOutcome::Joined,
            ConnectOutcome::Restored => SyncConnectOutcome::Restored,
        };
        let credential = SyncCredential {
            server_url: server_url.clone(),
            username: username.clone(),
            password,
            vault_id: connected.vault_id.clone(),
            key: BASE64.encode(connected.master_key.as_bytes()),
        };
        // 先放进内存：随后写设置时，跟随设置变化的任务看到的地址与账号已经和凭据一致
        *self.creds.lock() = Some(credential.clone());
        self.keychain_ok.store(true, Ordering::Release);
        let settings = Arc::clone(&self.settings);
        match blocking(KEYCHAIN_TIMEOUT, move || settings.sync_credentials().set(&credential)).await {
            Some(Ok(())) => {}
            Some(Err(e)) => log::warn!(
                "sync: credentials kept for this session only; the keychain refused them ({:?})",
                e.code
            ),
            None => log::warn!("sync: credentials kept for this session only; the keychain did not answer"),
        }
        drop(cycle);
        let (url, user) = (server_url.clone(), username.clone());
        let saved = self
            .write_sync(move |s| {
                s.enabled = true;
                s.server_url.clone_from(&url);
                s.username.clone_from(&user);
                s.content = content;
            })
            .await;
        if let Err(e) = saved {
            log::warn!("sync: the sync settings could not be saved ({:?})", e.code);
            if prior.enabled {
                self.status.send_replace(prior_status);
            } else {
                // 设置没写进去，本机仍是「未连接」：撤回刚保存的凭据与开启的一切，钩子与写入线程不能在同步关闭时继续运行
                *self.creds.lock() = None;
                self.keychain_ok.store(false, Ordering::Release);
                let settings = Arc::clone(&self.settings);
                if !matches!(
                    blocking(KEYCHAIN_TIMEOUT, move || settings.sync_credentials().delete()).await,
                    Some(Ok(()))
                ) {
                    log::warn!("sync: the saved credentials could not be removed");
                }
                self.abandon_first_connect(active, fresh_db).await;
            }
            return Err(e);
        }
        self.mark_seen();
        self.gate.set_auto(self.settings.get().sync.auto);
        self.gate.set_enabled(true);
        self.gate.resume();
        self.pins_checked.store(false, Ordering::Release);
        self.set_status(|s| {
            s.phase = SyncPhase::Idle;
            s.error = None;
            s.transient = false;
            s.error_detail = None;
            s.progress = None;
        });
        self.refresh_from_engine(&active).await;
        log::info!("sync: connected outcome={outcome:?}");
        // 剪贴板与识别历史的加入、上传与下载在后台继续
        self.trigger(CycleReason::Connect);
        Ok(SyncConnectResult {
            outcome,
            status: self.status(),
        })
    }

    /// 断开：本机停止同步，删除本机副本与保存的凭据，`enabled = false`（地址与账号保留，方便重连）。服务器上的数据不动
    pub async fn disconnect(self: &Arc<Self>) -> Result<(), IpcError> {
        let _life = self.lifecycle.try_lock().map_err(|_| errors::busy())?;
        self.disconnect_locked(None).await
    }

    /// 清除服务器上的同步数据（所有设备下一轮都会停止同步），随后本机断开。`account = None` 用设置里的地址与账号，
    /// `password = None` 用已保存的密码。清除的是本机自己的服务器时保存的应用密码留下（只去掉旧数据的 vault 与密钥）：
    /// 接着「重新开始」的测试连接与连接可以留空沿用
    pub async fn reset_remote(
        self: &Arc<Self>,
        account: Option<SyncAccount>,
        password: Option<String>,
    ) -> Result<(), IpcError> {
        let _life = self.lifecycle.try_lock().map_err(|_| errors::busy())?;
        let current = self.settings.get().sync.clone();
        let account = match account {
            Some(a) => a,
            None if current.server_url.is_empty() || current.username.is_empty() => {
                return Err(errors::not_connected());
            }
            None => SyncAccount {
                server_url: current.server_url.clone(),
                username: current.username.clone(),
            },
        };
        let (server_url, username, _) = check_account(&account)?;
        let (password, from_store) = match password {
            Some(p) => (p, false),
            None => (
                self.stored_password(&server_url, &username)
                    .await?
                    .ok_or_else(errors::credentials_missing)?,
                true,
            ),
        };
        let remote = WebDav::new(&server_url, &username, &password).map_err(|e| errors::ipc_error(&e))?;
        if let Err(e) = wisp_sync::reset_remote(&remote).await {
            log::info!("sync: clearing the server failed ({})", describe(&e));
            return Err(errors::ipc_error(&e));
        }
        log::info!("sync: server data cleared");
        // 清除的是本机正在同步的那个服务器（或本机还没连接）时本机随之断开；清除的是别的服务器时本机不受影响
        let same = current.server_url == server_url && current.username == username;
        if !current.enabled || same {
            // 应用密码刚用过、仍然有效：本机连着这个服务器（或本来就保存着它的密码）时留下，只是不再绑定已清除的数据；
            // 本机没保存过时不因为这次清除新存凭据
            let keep = (same && (current.enabled || from_store)).then(|| SyncCredential {
                server_url: server_url.clone(),
                username: username.clone(),
                password: password.clone(),
                vault_id: String::new(),
                key: String::new(),
            });
            self.disconnect_locked(keep).await?;
        }
        Ok(())
    }

    /// 生成一个随机的同步密码（24 个 base32 字符，每 4 个一组）
    pub fn generate_passphrase() -> Result<String, IpcError> {
        wisp_sync::crypto::generate_passphrase().map_err(|_| IpcError::internal("cannot generate a sync passphrase"))
    }

    // ================================================================================================
    // 开启 / 关闭
    // ================================================================================================

    fn active(&self) -> Option<Arc<Active>> {
        self.active.read().clone()
    }

    /// 本机副本的路径
    fn db_path(&self) -> PathBuf {
        self.settings.paths().data_dir.join(SYNC_DB)
    }

    fn sync_dir(&self) -> PathBuf {
        self.settings.paths().data_dir.join(SYNC_DIR)
    }

    fn clipboard(&self) -> Option<Arc<ClipboardService>> {
        let app = self.app.as_ref()?;
        Some(Arc::clone(&app.try_state::<AppState>()?.clipboard))
    }

    fn clipboard_source(&self) -> ClipboardSource {
        let app = self.app.clone();
        Arc::new(move || {
            let app = app.as_ref()?;
            Some(Arc::clone(&app.try_state::<AppState>()?.clipboard))
        })
    }

    fn set_status(&self, f: impl FnOnce(&mut SyncStatus)) {
        self.status.send_if_modified(|s| {
            let before = s.clone();
            f(s);
            *s != before
        });
    }

    /// 写 `settings.sync`（只属于本机的设置，来源记为本机写入；投影本来就不包含它）
    async fn write_sync(&self, f: impl Fn(&mut SyncSettings) + Send + 'static) -> Result<(), IpcError> {
        let settings = Arc::clone(&self.settings);
        run_blocking(move || settings.update(&mut |s| f(&mut s.sync)).map(|_| ())).await
    }

    /// 已开启就直接返回；否则在阻塞线程里开启（调用方持有 `lifecycle`）
    async fn ensure_active(self: &Arc<Self>) -> Result<Arc<Active>, SyncError> {
        if let Some(active) = self.active() {
            return Ok(active);
        }
        let svc = Arc::clone(self);
        // 打开 sync.db 受 SQLite 的忙等待上限约束；这里不再另设超时：开启到一半被丢下会留下装好的钩子
        let active = tauri::async_runtime::spawn_blocking(move || svc.activate_blocking())
            .await
            .map_err(|_| SyncError::Internal("sync start task failed"))??;
        *self.active.write() = Some(Arc::clone(&active));
        self.refresh_from_engine(&active).await;
        Ok(active)
    }

    /// 开启（**阻塞**）：打开 sync.db → 装钩子（拿到基线）→ 启动核对 → 启动写入线程 → 装剪贴板 sink
    fn activate_blocking(self: &Arc<Self>) -> Result<Arc<Active>, SyncError> {
        let settings = &self.settings;
        let content = settings.get().sync.content;
        let id = identity::ensure_device_id(settings.state())
            .map_err(|_| SyncError::Local(LocalError::Failed("device id")))?;
        let device = DeviceInfo {
            id,
            name: identity::device_name(),
            os: OsKind::current(),
            app_version: self.app_version.clone(),
        };
        let engine = Arc::new(Engine::open(&self.db_path(), &self.sync_dir().join("spool"), device)?);
        let journal = engine.journal()?;
        let captures = Arc::new(Captures::default());
        let (tx, rx) = mpsc::channel();
        let capturer = Arc::new(Capturer::new(engine.hlc(), tx.clone(), Arc::clone(&captures)));
        capturer.set_content(content.clipboard, content.ocr_history);
        let store = Arc::new(DesktopStore::new(
            Arc::clone(settings),
            self.clipboard_source(),
            journal.clone(),
            Arc::clone(&captures),
        ));

        // 手改的时间戳在装钩子之前分配：之后钩子捕获的改动都比它新
        let t0 = engine.hlc().now();
        let hook = Arc::clone(&capturer);
        let baseline = settings.set_change_hook(Some(Arc::new(move |c: &ConfigChange<'_>| hook.config(c))));
        let hook = Arc::clone(&capturer);
        settings.set_secret_hook(Some(Arc::new(move |id: &str, key: Option<&str>| hook.secret(id, key))));

        // 核对在写入线程启动之前做：期间钩子捕获的改动排在通道里，核对写完之后才处理
        let fingerprint = identity::fingerprint(&self.app_version);
        let load_state = |file| settings.config_load_state(file);
        let key_present = |id: &str| {
            let current = settings.get();
            current
                .providers
                .iter()
                .any(|p| p.id == id)
                .then(|| settings.keychain().has(id))
        };
        let startup = reconcile::Startup {
            journal: &journal,
            engine: &engine,
            baseline: &baseline,
            load_state: &load_state,
            t0,
            fingerprint: &fingerprint,
            os: OsKind::current(),
            key_present: &key_present,
        };
        match reconcile::run(&startup) {
            Ok(r) if r.published > 0 || !r.reapplied.is_empty() => log::info!(
                "sync: startup check published={} reapplied={}",
                r.published,
                r.reapplied.len()
            ),
            Ok(_) => {}
            Err(e) => log::warn!("sync: startup check failed ({})", describe(&e)),
        }

        let keys = Arc::clone(settings);
        let weak = Arc::downgrade(self);
        let ctx = WriterCtx {
            journal: journal.clone(),
            captures: Arc::clone(&captures),
            os: OsKind::current(),
            has_key: Arc::new(move |id: &str| keys.keychain().has(id)),
            on_local: Arc::new(move || {
                if let Some(svc) = weak.upgrade() {
                    svc.local_changed();
                }
            }),
        };
        let writer = match Writer::spawn(tx.clone(), rx, ctx) {
            Ok(w) => w,
            Err(e) => {
                settings.set_change_hook(None);
                settings.set_secret_hook(None);
                return Err(SyncError::Io(e.kind()));
            }
        };
        let active = Arc::new(Active {
            engine,
            store,
            capturer,
            captures,
            journal,
            tx,
            writer,
            sink: AtomicBool::new(false),
        });
        self.install_sink(&active, content.clipboard || content.ocr_history);
        log::info!("sync: started");
        Ok(active)
    }

    /// 安装 / 卸下剪贴板 sink（剪贴板与识别历史都不同步时卸下：剪贴板服务每次记录就少一次按主键的查询）
    fn install_sink(&self, active: &Arc<Active>, on: bool) {
        let Some(clipboard) = self.clipboard() else { return };
        if active.sink.swap(on, Ordering::AcqRel) != on {
            clipboard.set_sync_sink(on.then(|| Arc::clone(&active.capturer) as Arc<dyn ClipSyncSink>));
        }
    }

    /// 开启后用副本里的状态刷新显示（不联网）
    async fn refresh_from_engine(&self, active: &Arc<Active>) {
        let engine = Arc::clone(&active.engine);
        let Some(Ok(st)) = blocking(LOCAL_TIMEOUT, move || engine.status()).await else {
            return;
        };
        self.set_status(|s| {
            if s.phase == SyncPhase::Off {
                s.phase = SyncPhase::Idle;
            }
            s.last_ok_ms = st.last_ok_ms;
            s.pending = st.pending;
            s.device_count = st.device_count;
        });
    }

    /// 同步已开启（启动时，或设置被手改为开启）：本机工作，不联网
    async fn enable_locked(self: &Arc<Self>) {
        match self.ensure_active().await {
            Ok(_) => {
                self.gate.set_auto(self.settings.get().sync.auto);
                self.gate.set_enabled(true);
            }
            Err(e) => {
                log::warn!("sync: cannot start ({})", describe(&e));
                self.fail(&e);
            }
        }
    }

    /// 关闭：卸钩子 → 打断进行中的一轮并等它退出 → 写完队列、结束写入线程 → 关闭 sync.db。不删除文件
    async fn deactivate(&self) {
        self.gate.set_enabled(false);
        let Some(active) = self.active.write().take() else {
            return;
        };
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.cancel.notify_waiters();
        let settings = Arc::clone(&self.settings);
        let clipboard = self.clipboard();
        // 卸配置钩子要拿三把写锁（可能正有一次写盘），放到阻塞线程
        let _ = tauri::async_runtime::spawn_blocking(move || {
            settings.set_change_hook(None);
            settings.set_secret_hook(None);
            if let Some(c) = clipboard {
                c.set_sync_sink(None);
            }
        })
        .await;
        let _cycle = self.cycle_lock.lock().await;
        let _ = tauri::async_runtime::spawn_blocking(move || shut(active)).await;
        log::info!("sync: stopped");
    }

    /// 删除本机副本与临时目录（断开、清除服务器数据之后）
    async fn remove_local_files(&self) {
        let db = self.db_path();
        let dir = self.sync_dir();
        let _ = blocking(LOCAL_TIMEOUT, move || {
            let wal = PathBuf::from(format!("{}-wal", db.display()));
            let shm = PathBuf::from(format!("{}-shm", db.display()));
            for attempt in 1..=REMOVE_ATTEMPTS {
                let ok = [&db, &wal, &shm].into_iter().all(|p| remove_file(p)) && remove_dir(&dir);
                if ok {
                    return;
                }
                if attempt < REMOVE_ATTEMPTS {
                    std::thread::sleep(REMOVE_GAP);
                }
            }
            log::warn!("sync: the local sync data could not be removed completely");
        })
        .await;
    }

    /// 第一次连接没有完成：撤回本次开启的一切。先放下调用方持有的引用——关闭要拿到最后一个引用才能关掉 sync.db，
    /// 否则副本仍然开着（Windows 上开着的文件删不掉，失败的提示还要等删除重试完才出来）
    async fn abandon_first_connect(&self, active: Arc<Active>, fresh_db: bool) {
        drop(active);
        self.deactivate().await;
        if fresh_db {
            self.remove_local_files().await;
        }
        self.status.send_replace(SyncStatus::off());
    }

    /// 本机停止同步、删除本机副本，`enabled = false`。保存的凭据整项删除；`keep` 为 Some 时改存它（只有应用密码，
    /// 不含 vault 与密钥）
    async fn disconnect_locked(self: &Arc<Self>, keep: Option<SyncCredential>) -> Result<(), IpcError> {
        self.deactivate().await;
        self.remove_local_files().await;
        self.creds.lock().clone_from(&keep);
        self.keychain_ok.store(false, Ordering::Release);
        self.pins_checked.store(false, Ordering::Release);
        let settings = Arc::clone(&self.settings);
        let saved = blocking(KEYCHAIN_TIMEOUT, move || {
            let store = settings.sync_credentials();
            match &keep {
                // 写不进去就整项删除：留着与已清除数据的绑定没有意义
                Some(c) => store.set(c).or_else(|_| store.delete()),
                None => store.delete(),
            }
        })
        .await;
        match saved {
            Some(Ok(())) => {}
            Some(Err(e)) => log::warn!("sync: the saved credentials could not be updated ({:?})", e.code),
            None => log::warn!("sync: the keychain did not answer while updating the saved credentials"),
        }
        self.write_sync(|s| s.enabled = false).await?;
        self.mark_seen();
        self.status.send_replace(SyncStatus::off());
        log::info!("sync: disconnected");
        Ok(())
    }

    /// 设置变了：在 `lifecycle` 锁内读**当前**的 `settings.sync`（等锁期间可能又变过），与上次处理过的比较。
    /// 第一次（启动时）同步已开启就开启本机部分
    async fn follow_settings(self: &Arc<Self>) {
        let _life = self.lifecycle.lock().await;
        let current = self.settings.get().sync.clone();
        let previous = self.seen.lock().replace(current.clone());
        match previous {
            None if current.enabled => self.enable_locked().await,
            Some(previous) if previous != current => self.on_sync_settings(&previous, &current).await,
            _ => {}
        }
    }

    /// 记下连接 / 断开自己写入的 `settings.sync`（调用方持有 `lifecycle`）
    fn mark_seen(&self) {
        *self.seen.lock() = Some(self.settings.get().sync.clone());
    }

    /// `settings.sync` 变了（设置页、手改配置文件；调用方持有 `lifecycle`）
    async fn on_sync_settings(self: &Arc<Self>, prev: &SyncSettings, cur: &SyncSettings) {
        if prev.enabled != cur.enabled {
            if cur.enabled {
                self.enable_locked().await;
            } else {
                self.deactivate().await;
                self.status.send_replace(SyncStatus::off());
            }
            return;
        }
        let Some(active) = self.active() else {
            // 同步开着、本机部分却没有开启：之前开启失败过（sync.db 打不开、磁盘满等），设置又有变化时重试
            if cur.enabled {
                self.enable_locked().await;
            }
            return;
        };
        self.gate.set_auto(cur.auto);
        if prev.server_url != cur.server_url || prev.username != cur.username {
            // 地址或账号改了：保存的密码不能拿去访问另一个服务器
            let stale = self
                .creds
                .lock()
                .as_ref()
                .is_some_and(|c| !c.matches(&cur.server_url, &cur.username));
            if stale {
                *self.creds.lock() = None;
                active.engine.lock();
                self.fail(&SyncError::Locked);
            }
        }
        if prev.content != cur.content {
            self.apply_content(&active, &prev.content, &cur.content).await;
        }
        if prev.auto != cur.auto || prev.content != cur.content {
            // 改同步设置是用户操作：解除暂停，按新设置同步一轮
            self.gate.resume();
            self.trigger(CycleReason::SettingsChanged);
        }
    }

    /// 内容开关变化：剪贴板 / 识别历史关闭期间不记录，重新打开时对该域重新加入（条目自身的时间）
    async fn apply_content(&self, active: &Arc<Active>, prev: &SyncContent, cur: &SyncContent) {
        active.capturer.set_content(cur.clipboard, cur.ocr_history);
        self.install_sink(active, cur.clipboard || cur.ocr_history);
        let mut rejoin = Vec::new();
        if cur.clipboard && !prev.clipboard {
            rejoin.push(Domain::Clip);
        }
        if cur.ocr_history && !prev.ocr_history {
            rejoin.push(Domain::Ocr);
        }
        if rejoin.is_empty() {
            return;
        }
        if cur.clipboard && !prev.clipboard {
            self.pins_checked.store(false, Ordering::Release);
        }
        let engine = Arc::clone(&active.engine);
        let result = blocking(LOCAL_TIMEOUT, move || rejoin.iter().try_for_each(|d| engine.reapply(d))).await;
        if let Some(Err(e)) = result {
            log::warn!("sync: re-joining a content type failed ({})", describe(&e));
        }
    }

    // ================================================================================================
    // 本机改动与去抖
    // ================================================================================================

    /// 写入线程写进了本机改动：标记「有改动等待同步」，安排一次性的去抖上传（退避期内、暂停时、关闭自动同步时不安排）
    fn local_changed(self: &Arc<Self>) {
        self.set_status(|s| {
            if s.phase != SyncPhase::Off {
                s.pending = true;
            }
        });
        let now = clock_ms();
        if !self.gate.debounce_allowed(now) || !self.debounce.note(now) {
            return;
        }
        let svc = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            loop {
                let wait = svc.debounce.deadline().saturating_sub(clock_ms());
                if wait <= 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(u64::try_from(wait).unwrap_or(0))).await;
            }
            svc.debounce.fire();
            svc.trigger(CycleReason::LocalChange);
        });
    }

    // ================================================================================================
    // 同步循环
    // ================================================================================================

    fn kick(self: &Arc<Self>) {
        if self
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let svc = Arc::clone(self);
            tauri::async_runtime::spawn(async move { svc.run_loop().await });
        }
    }

    /// 依次处理排队的触发，直到队列空；WebDAV 客户端在连续的几轮之间共用，循环结束即释放
    async fn run_loop(self: Arc<Self>) {
        let mut slot: Option<RemoteSlot> = None;
        loop {
            if let Some(reason) = self.queue.take() {
                self.run_one(reason, &mut slot).await;
                continue;
            }
            self.running.store(false, Ordering::Release);
            // 放下标记的同时又有触发排进来、而它的 kick 因为标记还在没有启动新循环：接着处理
            if self.queue.is_empty()
                || self
                    .running
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
            {
                break;
            }
        }
    }

    async fn run_one(self: &Arc<Self>, reason: CycleReason, slot: &mut Option<RemoteSlot>) {
        let _cycle = self.cycle_lock.lock().await;
        let Some(active) = self.active() else { return };
        if !self.gate.enabled() {
            return;
        }
        let generation = self.generation.load(Ordering::Acquire);
        let credential = match self.credentials(reason, &active).await {
            Ok(Some(c)) => c,
            // 本机改动触发、还没有用户活动读过凭据：等用户活动时再同步（改动已在副本里，不会丢）
            Ok(None) => return,
            Err(e) => {
                self.cycle_failed(reason, &e);
                return;
            }
        };
        let tag = remote_tag(&credential);
        let remote = match slot.take() {
            Some(s) if s.tag == tag => s.remote,
            _ => match WebDav::new(&credential.server_url, &credential.username, &credential.password) {
                Ok(remote) => remote,
                Err(e) => {
                    self.cycle_failed(reason, &e);
                    return;
                }
            },
        };
        drop(credential);
        let keychain = reason.is_user_activity() || self.keychain_ok.load(Ordering::Acquire);
        let content = self.settings.get().sync.content;
        let options = self.options(keychain, content);
        active.store.set_keychain_allowed(keychain);
        // 写入线程里排着的本机改动先落盘：引擎这一轮能看到它们；之后才写进来的算「本轮期间的本机改动」，应用远端值时跳过
        let flushing = Arc::clone(&active);
        let _ = blocking(CYCLE_FLUSH * 2, move || flushing.writer.flush(CYCLE_FLUSH)).await;
        active.captures.begin_cycle();
        self.gate.started(clock_ms());
        self.set_status(|s| {
            s.phase = SyncPhase::Syncing;
            s.progress = None;
        });
        let progress = self.progress_fn();
        let notified = self.cancel.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let result = if self.generation.load(Ordering::Acquire) == generation {
            tokio::select! {
                r = active.engine.sync_cycle(&remote, &active.store, reason, &options, &*progress) => Some(r),
                () = &mut notified => None,
            }
        } else {
            None
        };
        active.captures.end_cycle();
        active.store.set_keychain_allowed(false);
        match result {
            // 同步正在关闭
            None => return,
            Some(Ok(report)) => self.cycle_succeeded(&active, reason, keychain, &options, report).await,
            Some(Err(e)) => self.cycle_failed(reason, &e),
        }
        *slot = Some(RemoteSlot { tag, remote });
    }

    /// 本轮用的凭据。缓存里没有时只在用户活动触发的轮次里读钥匙串（`Ok(None)` = 这一轮不同步）；与设置里的地址、账号
    /// 不一致按缺少凭据处理。副本还没解锁时用凭据里的密钥解锁
    async fn credentials(
        &self,
        reason: CycleReason,
        active: &Arc<Active>,
    ) -> Result<Option<SyncCredential>, SyncError> {
        let sync = self.settings.get().sync.clone();
        let cached = self.creds.lock().clone();
        let credential = match cached {
            Some(c) if c.matches(&sync.server_url, &sync.username) => c,
            Some(_) => {
                *self.creds.lock() = None;
                active.engine.lock();
                return Err(SyncError::Locked);
            }
            None if !reason.is_user_activity() => return Ok(None),
            None => {
                let settings = Arc::clone(&self.settings);
                match blocking(KEYCHAIN_TIMEOUT, move || settings.sync_credentials().get()).await {
                    Some(Ok(Some(c))) if c.matches(&sync.server_url, &sync.username) => {
                        *self.creds.lock() = Some(c.clone());
                        c
                    }
                    Some(Ok(_)) => return Err(SyncError::Locked),
                    Some(Err(_)) | None => return Err(SyncError::Local(LocalError::Keychain)),
                }
            }
        };
        if !active.engine.is_unlocked() {
            let master = decode_master(&credential.key).ok_or(SyncError::Locked)?;
            let engine = Arc::clone(&active.engine);
            let vault = credential.vault_id.clone();
            blocking(LOCAL_TIMEOUT, move || engine.unlock(&vault, &master))
                .await
                .unwrap_or(Err(SyncError::Internal("unlock did not finish")))?;
        }
        Ok(Some(credential))
    }

    fn options(&self, keychain: bool, content: SyncContent) -> CycleOptions {
        let s = self.settings.get();
        CycleOptions {
            content: ContentSwitches {
                settings: content.settings,
                // 「API Key」是「设置与模型服务」的子开关：Key 按服务 id 保存，脱离服务同步没有意义
                api_keys: content.settings && content.api_keys,
                clipboard: content.clipboard,
                images: content.images,
                // 本机没开识别历史时不导入、也没有可发布的识别记录
                ocr_history: content.ocr_history && s.ocr.history,
            },
            retention: RetentionPolicy {
                clip_max_items: s.clipboard.max_items,
                clip_max_age_days: s.clipboard.max_age_days,
                ocr_history_limit: s.ocr.history_limit,
            },
            images_max_bytes: wisp_types::defaults::SYNC_MAX_IMAGE_BYTES,
            keychain,
            schema: identity::schema_versions(),
        }
    }

    /// 进度回调：每 200 ms 至多广播一次（完成时总是广播）
    fn progress_fn(self: &Arc<Self>) -> Arc<ProgressFn> {
        let weak: Weak<Self> = Arc::downgrade(self);
        Arc::new(move |p: Progress| {
            let Some(svc) = weak.upgrade() else { return };
            let now = clock_ms();
            if p.done < p.total && now.saturating_sub(svc.progress_ms.load(Ordering::Relaxed)) < PROGRESS_EVERY_MS {
                return;
            }
            svc.progress_ms.store(now, Ordering::Relaxed);
            svc.set_status(|s| {
                s.phase = SyncPhase::Syncing;
                s.progress = (p.total > 0).then_some(SyncProgress {
                    done: p.done,
                    total: p.total,
                });
            });
        })
    }

    async fn cycle_succeeded(
        self: &Arc<Self>,
        active: &Arc<Active>,
        reason: CycleReason,
        keychain: bool,
        options: &CycleOptions,
        report: CycleReport,
    ) {
        self.gate.succeeded();
        if keychain {
            self.keychain_ok.store(true, Ordering::Release);
        }
        if let Some(id) = report.new_device_id.clone() {
            // 发现本机的数据目录是从另一台设备复制来的，引擎换了新的设备 id
            let settings = Arc::clone(&self.settings);
            if !matches!(
                blocking(LOCAL_TIMEOUT, move || settings.state().set_sync_device_id(Some(&id))).await,
                Some(Ok(()))
            ) {
                log::warn!("sync: the new device id could not be saved");
            }
        }
        log::debug!(
            "sync: {} cycle ok requests={} pulled={} applied={} deferred={} pushed={} up={} down={}",
            reason.as_str(),
            report.requests,
            report.pulled,
            report.applied,
            report.deferred,
            report.pushed,
            report.bytes_up,
            report.bytes_down
        );
        let now = now_ms();
        self.set_status(|s| {
            *s = SyncStatus {
                phase: SyncPhase::Idle,
                last_ok_ms: Some(now),
                pending: report.pending,
                error: None,
                transient: false,
                error_detail: None,
                progress: None,
                device_count: report.device_count,
            };
            if report.corrupt_devices > 0 {
                // 有设备的数据读不出来：只跳过那台设备，其他照常；作为暂时性问题显示，不暂停自动同步
                s.phase = SyncPhase::Error;
                s.error = Some(SyncErrorKind::Corrupt);
                s.transient = true;
            }
        });
        // 本轮的流量预算用完、还有剩余：接着再跑一轮。照样经过开关判定（关闭了同步或自动同步、暂停、退避期内不续跑），
        // 只是不受节流限制——这是同一次触发的延续
        if report.again && self.gate.allows_continuation(reason, clock_ms()) {
            self.queue.push(reason);
        }
        if options.content.clipboard && !self.pins_checked.swap(true, Ordering::AcqRel) {
            self.check_pins(active).await;
        }
    }

    /// 剪贴板置顶的一次性核对（第一次成功同步之后；剪贴板库还没打开时留到下一次）
    async fn check_pins(&self, active: &Arc<Active>) {
        let Some(store) = self.clipboard().and_then(|c| c.sync_store()) else {
            self.pins_checked.store(false, Ordering::Release);
            return;
        };
        let active = Arc::clone(active);
        let result = blocking(LOCAL_TIMEOUT, move || {
            reconcile::clip_pins(
                &store,
                &active.journal,
                &active.captures,
                &active.engine.hlc(),
                &active.tx,
            )
        })
        .await;
        match result {
            Some(Ok(n)) if n > 0 => log::info!("sync: re-published {n} clipboard pin states"),
            Some(Err(e)) => log::warn!("sync: clipboard pin check failed ({})", describe(&e)),
            _ => {}
        }
    }

    fn cycle_failed(&self, reason: CycleReason, e: &SyncError) {
        log::info!("sync: {} cycle failed ({})", reason.as_str(), describe(e));
        self.fail(e);
    }

    /// 记下失败：暂时性的退避（不设计时器，下一次活动时判断），需要处理的暂停自动同步直到用户操作
    fn fail(&self, e: &SyncError) {
        let kind = errors::kind_of(e.kind());
        let transient = e.is_transient();
        if transient {
            self.gate.failed_transient(clock_ms());
        } else {
            self.gate.failed_persistent();
        }
        if matches!(
            kind,
            SyncErrorKind::Auth
                | SyncErrorKind::CredentialsMissing
                | SyncErrorKind::Passphrase
                | SyncErrorKind::VaultReset
        ) {
            // 凭据已经不能用了：下次用户操作时重新读取（「更新密码…」会写入新的）
            *self.creds.lock() = None;
            if let Some(active) = self.active() {
                active.engine.lock();
            }
        }
        self.set_status(|s| {
            s.phase = SyncPhase::Error;
            s.error = Some(kind);
            s.transient = transient;
            s.error_detail = e.detail();
            s.progress = None;
        });
    }

    /// 与这个地址、账号匹配的已保存密码（缓存，或读钥匙串——只在用户操作的命令里调用）
    async fn stored_password(&self, server_url: &str, username: &str) -> Result<Option<String>, IpcError> {
        if let Some(password) = self
            .creds
            .lock()
            .as_ref()
            .filter(|c| c.matches(server_url, username))
            .map(|c| c.password.clone())
        {
            return Ok(Some(password));
        }
        let settings = Arc::clone(&self.settings);
        match blocking(KEYCHAIN_TIMEOUT, move || settings.sync_credentials().get()).await {
            Some(Ok(Some(c))) if c.matches(server_url, username) => Ok(Some(c.password.clone())),
            Some(Ok(_)) => Ok(None),
            Some(Err(e)) => Err(e),
            None => Err(IpcError::keychain("the keychain did not answer")),
        }
    }
}

/// 关闭时释放资源（**阻塞**）：结束写入线程，关闭 sync.db
fn shut(active: Arc<Active>) {
    active.writer.stop();
    let Ok(active) = Arc::try_unwrap(active) else {
        log::debug!("sync: resources still referenced at stop; released later");
        return;
    };
    let Active { engine, .. } = active;
    match Arc::try_unwrap(engine) {
        Ok(engine) => {
            if let Err(e) = engine.close() {
                log::debug!("sync: closing the replica: {}", describe(&e));
            }
        }
        Err(_) => log::debug!("sync: the replica is still in use; it closes when the last task finishes"),
    }
}

/// 校验地址与账号：返回规范化的地址、账号与主机名
fn check_account(account: &SyncAccount) -> Result<(String, String, String), IpcError> {
    let url = wisp_sync::validate_server_url(&account.server_url).map_err(errors::url_error)?;
    let username = account.username.clone();
    // Basic 认证的账号里不能有冒号
    if username.is_empty() || username.contains(':') || username.chars().any(char::is_control) {
        return Err(IpcError::invalid_argument(
            "sync username must be non-empty without ':' or control characters",
        ));
    }
    let host = url.host_str().unwrap_or_default().to_owned();
    Ok((url.as_str().to_owned(), username, host))
}

fn probe_failed(kind: SyncErrorKind, detail: Option<String>) -> SyncProbe {
    SyncProbe {
        ok: false,
        error: Some(kind),
        error_detail: detail,
        vault: None,
    }
}

/// 凭据的指纹（运行循环据此判断能否沿用上一轮的客户端）
fn remote_tag(c: &SyncCredential) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    for part in [&c.server_url, &c.username, &c.password] {
        h.update(part.as_bytes());
        h.update(&[0]);
    }
    *h.finalize().as_bytes()
}

/// 凭据里保存的主密钥
fn decode_master(encoded: &str) -> Option<MasterKey> {
    let mut bytes = BASE64.decode(encoded).ok()?;
    let master = MasterKey::from_slice(&bytes).ok();
    bytes.fill(0);
    std::hint::black_box(&bytes);
    master
}

/// 在阻塞线程里执行，最多等 `limit`；超时或任务失败返回 None（超时的任务仍会在后台跑完）
async fn blocking<T, F>(limit: Duration, f: F) -> Option<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    match tokio::time::timeout(limit, tauri::async_runtime::spawn_blocking(f)).await {
        Ok(Ok(v)) => Some(v),
        Ok(Err(_)) => {
            log::warn!("sync: a background task failed");
            None
        }
        Err(_) => {
            log::warn!("sync: a background task timed out");
            None
        }
    }
}

fn remove_file(path: &std::path::Path) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

fn remove_dir(path: &std::path::Path) -> bool {
    match std::fs::remove_dir_all(path) {
        Ok(()) => true,
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}
