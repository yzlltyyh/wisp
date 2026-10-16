//! 剪贴板服务核心 `Engine`：数据库的惰性打开、内存缓存、记录管线、置顶 / 删除 / 清空、监听的安装与注销。
//!
//! 不依赖 Tauri 运行时：平台经 [`Port`]、窗口 / 事件 / 托盘经 [`Host`]、后台工作经 [`Exec`]。方法分两类：
//! - **不阻塞**：`paused` / `set_paused`、`apply_settings`、`emit`；`first_page` 在缓存就绪后只读缓存，页面里有文件条目时
//!   最多再等 5 ms 的 exists 检查（[`super::files`]）；
//! - **会阻塞**（数据库、平台读写）：其余全部，调用方（[`super::service`]）放进阻塞线程池。
//!
//! # 数据库的打开
//! 启动时不打开。`preload` 在「托盘就绪且 Panel 预热完成后的空闲时刻」由 bootstrap 调用，一次性打开并加载
//! `RecentCache`；第一次剪贴板事件、第一次命令也会顺带打开（日志 `clipboard store opened reason=…` 记下原因，
//! 「启动后第一次 ⌥V 不触发打开」以 `reason=first_page` 不出现为证）。
//! 打开失败：`SchemaTooNew`（用户从新版降级）→ 停用历史（注销监听、不再记录、列表为空）并通知一次，库文件原样保留；
//! 其他错误 → 记日志，按「历史不可用」处理，[`REOPEN_AFTER`] 之后再试。打开成功时排一次空闲清理（启动后第一次
//! 空闲时清孤儿 blob：清理从不为自己打开数据库，所以由打开这一刻来排）。
//!
//! # 事件的顺序
//! 改动缓存的 `clipboard:changed`（inserted / touched / removed / cleared，以及后台结果的 updated）都在**串行区内**
//! 交给 [`Engine::emit`]，投递本身在投递锁内按调用顺序发出（`outbox.rs`）。因此同一条目的事件顺序与库里的改动顺序一致：
//! 后台结果（缩略图、图标、迟到的文件检查）推送 updated 之前先在串行区内确认这一条仍在（在缓存里，或库里还有这一行），
//! 删除与它们交错时，前端要么先收到 updated 再收到 removed，要么根本收不到这个 updated——不会出现「removed 之后又来
//! updated」。`emit` 不阻塞（只读面板状态并投递），持串行锁调用它不会拖慢记录与粘贴。
//!
//! # 不持串行锁的库改动
//! wisp-clipboard 的约定是「改库的 `ClipStore` 调用 + 对应的缓存更新」同在串行区。有两处例外，都只改内容字段、不改条目的排序键，
//! 缓存「数据库排序后的前缀」这一不变式不受影响（`RecentCache` 文档：唯一允许滞后到达的是后台缩略图 / 图标的结果）：
//! - `make_thumb`（[`Engine::make_thumb`]）：读原图、解码、缩放、编码要几十到上百毫秒，持串行锁会挡住记录与粘贴的
//!   touch。存储层在写回之前于自己的锁内重新确认「这一行还在、还没有缩略图」，否则丢弃已生成的文件；删除在同一把锁内
//!   收集并删除这一行引用的缩略图，所以不会留下孤儿文件。随后持串行锁原地合并进缓存，并按上一节确认这一条仍在才推送；
//! - `put_icon`：见 `icons.rs` 模块文档。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use wisp_clipboard::{ClipStore, IngestOutcome, IngestPolicy, RecentCache, SkipReason, StoreError};
use wisp_platform::{HookHandle, PlatformError, ReadLimits};
use wisp_types::{
    ClipFilter, ClipItemDetail, ClipItemMeta, ClipKind, ClipPage, ClipboardChanged, ClipboardRecording,
    ClipboardSettings, OsKind, RawClipboard,
};

use crate::error::{IpcError, IpcErrorCode};

use super::files::{FilesRefresh, keep_checked_files};
use super::icons::IconState;
use super::outbox::Outbox;
use super::paste::PasteGate;
use super::port::{Exec, Host, Port};
use super::queue::PasteQueue;
use super::retention::Idle;
use super::smart::{ExpirySchedule, InspectCache};
use super::sync::{ClipSyncSink, sync_ident};
use super::watch::{Capture, WatchSignal};

/// 首屏条数：缓存的全部（先显示 Rust 内存缓存里最近 200 条的元数据）
pub(crate) const FIRST_PAGE_LIMIT: u32 = 200;
/// 打开失败（`SchemaTooNew` 除外）后多久再试
pub(crate) const REOPEN_AFTER: Duration = Duration::from_secs(30);
/// HTML / RTF 的读取上限（各超过 256 KB 就丢弃该格式）
const MAX_RICH_BYTES: usize = 256 * 1024;

/// 数据库的打开状态
enum Slot {
    Unopened,
    Ready(Arc<ClipStore>),
    /// 库来自更新版本：本进程内停用历史
    TooNew,
    /// 其他打开错误：记下时刻，过一段时间再试
    Failed(Instant),
}

/// 串行区（[`Engine::serial`]）
pub(crate) struct Serial<'a> {
    _guard: MutexGuard<'a, ()>,
    cache: &'a RwLock<RecentCache>,
}

impl Serial<'_> {
    /// 缓存的写锁（只在串行区内取，用完即放）
    pub(crate) fn cache(&self) -> RwLockWriteGuard<'_, RecentCache> {
        self.cache.write()
    }

    /// 缓存的读锁（串行区内只读、不挡首屏的读者）
    pub(crate) fn cache_read(&self) -> RwLockReadGuard<'_, RecentCache> {
        self.cache.read()
    }
}

/// 剪贴板服务核心
pub(crate) struct Engine {
    pub(crate) port: Arc<dyn Port>,
    pub(crate) host: Arc<dyn Host>,
    pub(crate) exec: Arc<dyn Exec>,
    /// 当前操作系统（粘贴的焦点顺序、粘贴键显示与埋点的方式名按它选择；单测可以模拟其他平台）
    pub(crate) os: OsKind,
    pub(crate) me: Weak<Engine>,
    data_dir: PathBuf,
    paused: AtomicBool,
    slot: Mutex<Slot>,
    /// 串行锁：包住「改动数据库的 `ClipStore` 调用 + 对应的 `RecentCache` 更新」（wisp-clipboard 的约定），两者之间不放开
    serial: Mutex<()>,
    /// 缓存的读写锁：只在持有串行锁时写；首屏只拿读锁
    cache: RwLock<RecentCache>,
    watch: Mutex<Option<HookHandle>>,
    /// 最近一次安装监听失败的原因（i18n key；装上之后清除）
    watch_error: Mutex<Option<String>>,
    signal: Arc<WatchSignal>,
    /// 最近处理过的剪贴板序号（只按相等去重：pboard 重启后序号会从小数字重新计数）
    last_change: Mutex<Option<u64>>,
    /// 最近一次**确实记进历史**（inserted / touched）的剪贴板序号（[`Engine::current_content_recorded`]）
    recorded_change: Mutex<Option<u64>>,
    /// 最近一次应用的剪贴板设置（`apply_settings` 据此判断哪些变了）
    applied: Mutex<Option<ClipboardSettings>>,
    too_new_notified: AtomicBool,
    /// 数据库已打开（无锁的只读标志：异步上下文里判断用，不去碰可能正被打开过程占着的 slot 锁）
    opened: AtomicBool,
    pub(crate) icons: IconState,
    pub(crate) files: FilesRefresh,
    pub(crate) idle: Idle,
    pub(crate) outbox: Outbox,
    pub(crate) paste_gate: PasteGate,
    /// 本机改动的同步通知（同步关闭时为 None，记录路径上没有任何额外开销）。见 `sync.rs`
    pub(crate) sync_sink: RwLock<Option<Arc<dyn ClipSyncSink>>>,
    /// 识别历史的改动（本机的新增 / 删除 / 清空与远端的应用）串行执行：同步层判断「本机有没有更新的改动」与应用远端
    /// 记录之间不会插进一次本机改动。识别历史不进剪贴板的串行区（写图片文件要几十毫秒，不该挡住记录与粘贴）
    pub(crate) ocr_serial: Mutex<()>,
    /// 最近几条的检视结果（`smart.rs`）
    pub(crate) inspect_cache: InspectCache,
    /// 验证码条目的到期排程（`smart.rs`）
    pub(crate) expiry: ExpirySchedule,
    /// 连续粘贴（`queue.rs`）
    pub(crate) queue: PasteQueue,
}

/// 存储层错误 → IPC 错误（message 只写英文简述，不含正文）
pub(crate) fn store_error(e: StoreError) -> IpcError {
    match e {
        StoreError::NotFound(_) => IpcError::not_found("clipboard item not found"),
        StoreError::SchemaTooNew { .. } => history_too_new(),
        StoreError::Io(e) => IpcError::new(IpcErrorCode::Io, format!("clipboard storage: {e}")),
        StoreError::InvalidInput(m) => IpcError::invalid_argument(m),
        other => IpcError::internal(format!("clipboard storage: {other}")),
    }
}

fn history_too_new() -> IpcError {
    IpcError::new(
        IpcErrorCode::NotReady,
        "clipboard history is disabled: the database was created by a newer Wisp",
    )
}

fn history_unavailable() -> IpcError {
    IpcError::new(IpcErrorCode::NotReady, "clipboard history is unavailable")
}

/// 按设置算出读取上限：不记录图片 / 富文本时干脆不读（省去向数据提供方取大块数据）
pub(crate) fn read_limits(s: &ClipboardSettings) -> ReadLimits {
    ReadLimits {
        max_text_bytes: usize::try_from(s.max_text_bytes).unwrap_or(usize::MAX),
        max_rich_bytes: if s.record_rich_text { MAX_RICH_BYTES } else { 0 },
        max_image_bytes: if s.record_images {
            usize::try_from(s.max_image_bytes).unwrap_or(usize::MAX)
        } else {
            0
        },
    }
}

impl Engine {
    /// 构造（不打开数据库、不安装监听）
    pub(crate) fn new(
        port: Arc<dyn Port>,
        host: Arc<dyn Host>,
        exec: Arc<dyn Exec>,
        os: OsKind,
        data_dir: PathBuf,
        paused: bool,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            port,
            host,
            exec,
            os,
            me: me.clone(),
            data_dir,
            paused: AtomicBool::new(paused),
            slot: Mutex::new(Slot::Unopened),
            serial: Mutex::new(()),
            cache: RwLock::new(RecentCache::default()),
            watch: Mutex::new(None),
            watch_error: Mutex::new(None),
            signal: Arc::new(WatchSignal::default()),
            last_change: Mutex::new(None),
            recorded_change: Mutex::new(None),
            applied: Mutex::new(None),
            too_new_notified: AtomicBool::new(false),
            opened: AtomicBool::new(false),
            icons: IconState::default(),
            files: FilesRefresh::default(),
            idle: Idle::default(),
            outbox: Outbox::default(),
            paste_gate: PasteGate::default(),
            sync_sink: RwLock::new(None),
            ocr_serial: Mutex::new(()),
            inspect_cache: InspectCache::default(),
            expiry: ExpirySchedule::default(),
            queue: PasteQueue::default(),
        })
    }

    /// 监听回调与工作任务的转交点
    pub(crate) fn signal(&self) -> &Arc<WatchSignal> {
        &self.signal
    }

    /// 在阻塞线程池里执行一个需要 `Arc<Self>` 的后台任务
    pub(crate) fn spawn(&self, job: impl FnOnce(Arc<Engine>) + Send + 'static) {
        if let Some(me) = self.me.upgrade() {
            self.exec.blocking(Box::new(move || job(me)));
        }
    }

    // ---------- 暂停与监听 ----------

    /// 是否已暂停
    pub(crate) fn paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    /// 暂停 / 恢复。不阻塞：状态立即生效，监听的注销 / 重新安装交给后台
    pub(crate) fn set_paused(&self, paused: bool) {
        if self.paused.swap(paused, Ordering::AcqRel) == paused {
            return;
        }
        log::info!("clipboard history paused={paused}");
        self.host.paused_changed(paused);
        self.emit(ClipboardChanged::Paused { paused });
        self.spawn(|e| e.reconcile_watch_logged());
    }

    /// 历史已因库来自更新版本而停用
    pub(crate) fn too_new(&self) -> bool {
        matches!(*self.slot.lock(), Slot::TooNew)
    }

    /// 历史不可用的原因（i18n key，`ClipboardPayload.unavailable`）：库来自更新版本 / 打不开。不触发打开
    pub(crate) fn history_issue(&self) -> Option<&'static str> {
        match *self.slot.lock() {
            Slot::TooNew => Some(HISTORY_TOO_NEW),
            Slot::Failed(_) => Some(HISTORY_UNAVAILABLE),
            Slot::Unopened | Slot::Ready(_) => None,
        }
    }

    /// 监听状态（`ClipboardPayload.recording`）
    pub(crate) fn recording_state(&self) -> ClipboardRecording {
        if self.paused() {
            return ClipboardRecording::Paused;
        }
        if !self.host.settings().clipboard.enabled {
            return ClipboardRecording::Disabled;
        }
        match self.watch_error.lock().clone() {
            Some(reason_key) if self.watch.lock().is_none() => ClipboardRecording::Unavailable { reason_key },
            _ => ClipboardRecording::Recording,
        }
    }

    /// 现在是否应该记录：没有暂停、监听开关打开、历史没有停用
    pub(crate) fn recording(&self) -> bool {
        let paused = self.paused();
        let enabled = self.host.settings().clipboard.enabled;
        !paused && enabled && !self.too_new()
    }

    /// 当前剪贴板里的内容已经在历史里（剪贴板过大时仍执行模拟复制的前提）：正在记录（没有暂停、监听开着、
    /// 历史没停用），且最近一次记进历史的序号就是现在的剪贴板序号。暂停期间、被隐私规则 / 排除名单跳过、或还没来得及
    /// 记录（4 Hz 轮询之间）都是 false——取词的模拟复制此时备份失败就不能放弃恢复。**会阻塞**（读一次平台序号）
    pub(crate) fn current_content_recorded(&self) -> bool {
        if !self.recording() {
            return false;
        }
        let Some(recorded) = *self.recorded_change.lock() else {
            return false;
        };
        self.port.clipboard().change_id() == recorded
    }

    /// 让监听的安装状态与「是否应该记录」一致。**会阻塞**（Windows 首次安装要建剪贴板线程与窗口）。
    ///
    /// 「是否应该记录」在持有监听锁之后才读：快速的「暂停 → 恢复」会派发两次 reconcile，它们在这把锁上串行，后拿到锁的
    /// 那次读到的一定是最终状态。若先读状态再拿锁，暂停那次可能读到 paused = true 后被挡住，恢复那次先拿锁（监听还在，
    /// 什么都不做），暂停那次随后注销监听——状态是未暂停、监听却没了，历史悄悄停止记录
    pub(crate) fn reconcile_watch(&self) -> Result<(), IpcError> {
        let mut watch = self.watch.lock();
        let want = self.recording();
        match (want, watch.is_some()) {
            (true, false) => {
                let signal = Arc::clone(&self.signal);
                let handle = match self
                    .port
                    .clipboard()
                    .watch(Box::new(move |change_id| signal.post(change_id)))
                {
                    Ok(h) => h,
                    Err(e) => {
                        *self.watch_error.lock() = Some(watch_reason_key(&e));
                        return Err(e.into());
                    }
                };
                *watch = Some(handle);
                *self.watch_error.lock() = None;
                log::info!("clipboard watch installed");
            }
            (false, true) => {
                // drop 即注销（macOS 取消定时器、Windows RemoveClipboardFormatListener、X11 取消订阅）
                *watch = None;
                log::info!("clipboard watch removed");
            }
            _ => {}
        }
        Ok(())
    }

    fn reconcile_watch_logged(&self) {
        if let Err(e) = self.reconcile_watch() {
            log::info!("clipboard watch unavailable: {}", e.message);
        }
    }

    /// 监听是否已安装
    pub(crate) fn watching(&self) -> bool {
        self.watch.lock().is_some()
    }

    /// 记下启动时的剪贴板设置（之后的变化与它比较）；监听由调用方随后同步安装
    pub(crate) fn apply_settings_now(&self, s: &ClipboardSettings) {
        *self.applied.lock() = Some(s.clone());
    }

    /// 剪贴板设置变化（监听开关、保留策略）。记录策略在每次记录时现取，不需要在这里处理。不阻塞
    pub(crate) fn apply_settings(&self, next: &ClipboardSettings) {
        let prev = self.applied.lock().replace(next.clone());
        let Some(prev) = prev else {
            self.spawn(|e| e.reconcile_watch_logged());
            return;
        };
        if prev.enabled != next.enabled {
            log::info!("clipboard watch enabled={}", next.enabled);
            self.spawn(|e| e.reconcile_watch_logged());
        }
        if prev.max_items != next.max_items || prev.max_age_days != next.max_age_days {
            // 保留上限改小后，空闲时清掉多余的
            self.idle.mark();
        }
    }

    // ---------- 数据库 ----------

    /// 数据库是否已打开（不取锁、不触发打开）
    pub(crate) fn store_opened(&self) -> bool {
        self.opened.load(Ordering::Acquire)
    }

    /// 已打开的数据库（不触发打开）
    pub(crate) fn ready_store(&self) -> Option<Arc<ClipStore>> {
        match &*self.slot.lock() {
            Slot::Ready(s) => Some(Arc::clone(s)),
            _ => None,
        }
    }

    /// 取得数据库，必要时打开并加载缓存。**会阻塞**（首次约十几毫秒）
    pub(crate) fn open_store(&self, reason: &'static str) -> Result<Arc<ClipStore>, IpcError> {
        let mut slot = self.slot.lock();
        match &*slot {
            Slot::Ready(s) => return Ok(Arc::clone(s)),
            Slot::TooNew => return Err(history_too_new()),
            Slot::Failed(at) if at.elapsed() < REOPEN_AFTER => return Err(history_unavailable()),
            Slot::Unopened | Slot::Failed(_) => {}
        }
        let started = Instant::now();
        match ClipStore::open(&self.data_dir) {
            Ok(store) => {
                let store = Arc::new(store);
                let cache = RecentCache::load(&store).unwrap_or_else(|e| {
                    // 缓存加载失败不影响历史：空缓存的首页带 next_offset，前端照常向数据库翻页
                    log::warn!("clipboard cache load failed: {e}");
                    RecentCache::default()
                });
                let cached = cache.len();
                *self.cache.write() = cache;
                *slot = Slot::Ready(Arc::clone(&store));
                drop(slot);
                self.opened.store(true, Ordering::Release);
                log::info!(
                    "clipboard store opened reason={reason} cached={cached} elapsed_ms={}",
                    started.elapsed().as_millis()
                );
                // 打开后的第一次空闲：保留策略与孤儿 blob
                self.idle.mark();
                // 上次退出 / 崩溃前没做完的缩略图在后台补上（否则这些图片条目一直只显示通用图标）
                self.spawn(|e| e.repair_thumbs());
                // 关闭期间到期的验证码现在删掉，并为还没到期的排上一次性任务
                self.spawn(|e| e.expire_codes());
                Ok(store)
            }
            Err(StoreError::SchemaTooNew { found, supported }) => {
                *slot = Slot::TooNew;
                drop(slot);
                log::warn!(
                    "clipboard history disabled: database schema v{found} is newer than supported v{supported} \
                     (file kept untouched)"
                );
                self.on_history_too_new();
                Err(history_too_new())
            }
            Err(e) => {
                *slot = Slot::Failed(Instant::now());
                log::warn!("clipboard history unavailable reason={reason}: {e}");
                Err(store_error(e))
            }
        }
    }

    fn on_history_too_new(&self) {
        if !self.too_new_notified.swap(true, Ordering::AcqRel) {
            self.host.notify_history_too_new();
        }
        // 停止监听：既然不能记录，就不该每 250 ms 醒一次
        self.spawn(|e| e.reconcile_watch_logged());
    }

    /// 启动后的空闲时刻一次性打开数据库并加载缓存。**会阻塞**
    pub(crate) fn preload(&self) {
        if self.ready_store().is_some() {
            return;
        }
        // 失败已在 open_store 里记日志
        let _ = self.open_store("preload");
    }

    // ---------- 列表与检索 ----------

    /// 首页（来自内存缓存，不查库），供编排器放进 `ClipboardPayload`。数据库还没打开（预加载之前就唤出了面板）时先打开
    /// 它——日志里会出现 `reason=first_page`。交出之前：去掉按天数已过期的条目、对文件条目做有时限的 exists 检查
    /// （最多 5 ms），并告诉事件投递「剪贴板视图即将打开」（此后产生的事件排队到视图 ready 再推送）
    pub(crate) fn first_page(&self) -> ClipPage {
        if self.ready_store().is_none() && self.open_store("first_page").is_err() {
            return ClipPage::empty(ClipFilter::All);
        }
        // 先登记再取页：取页之后、面板 prepare 之前产生的事件也不会丢（与首页重叠的由前端按 id 幂等处理）
        self.expect_clipboard_view();
        // macOS 的轮询在用户安静时放慢到 1 s / 5 s 一次：让它马上读一次，刚复制的内容随即经监听插进打开的列表
        // （事件驱动的平台什么都不做）
        if self.watching() {
            self.port.clipboard().poll_now();
        }
        let mut page = self.cache.read().page(ClipFilter::All, FIRST_PAGE_LIMIT);
        self.finish_page(&mut page);
        page
    }

    /// 分页。偏移 0 且缓存能完整回答时不查库。**会阻塞**
    ///
    /// 「收藏」筛选同样如此：缓存是数据库排序（收藏在前）的前缀，缓存里的收藏就是全部收藏的前缀——条数够一页时直接
    /// 回答，否则（收藏比缓存多，或缓存不完整）查库，翻页偏移与数据库一致
    pub(crate) fn recent(&self, offset: u32, limit: u32, filter: ClipFilter) -> Result<ClipPage, IpcError> {
        let store = match self.open_store("recent") {
            Ok(s) => s,
            Err(_) => return Ok(ClipPage::empty(filter)),
        };
        // 定时器因系统睡眠晚到时先把到期的验证码删掉：之后的缓存、查询与偏移都不再含它们
        self.expire_if_overdue();
        if offset == 0 {
            let mut page = self.cache.read().page(filter, limit);
            if page.items.len() >= limit as usize || page.next_offset.is_none() {
                self.finish_page(&mut page);
                return Ok(page);
            }
        }
        let mut page = store.recent(offset, limit, filter).map_err(store_error)?;
        self.finish_page(&mut page);
        Ok(page)
    }

    /// 一页结果交给前端之前的共同处理：去掉按天数已过期的条目（[`Engine::hide_expired`]）与已到期的验证码
    /// （[`Engine::hide_expired_codes`]），检查文件条目的 exists
    fn finish_page(&self, page: &mut ClipPage) {
        self.hide_expired(page);
        self.hide_expired_codes(page);
        self.check_page_files(page);
    }

    /// 检索。**会阻塞**
    pub(crate) fn search(&self, query: &str, filter: ClipFilter, limit: u32) -> Result<ClipPage, IpcError> {
        let store = match self.open_store("search") {
            Ok(s) => s,
            Err(_) => {
                let mut page = ClipPage::empty(filter);
                page.query = Some(query.to_owned());
                return Ok(page);
            }
        };
        self.expire_if_overdue();
        let mut page = store.search(query, filter, limit).map_err(store_error)?;
        self.finish_page(&mut page);
        Ok(page)
    }

    /// 详情（Space 预览）。**会阻塞**
    pub(crate) fn detail(&self, id: i64) -> Result<Option<ClipItemDetail>, IpcError> {
        let store = self.open_store("detail")?;
        store.get_detail(id).map_err(store_error)
    }

    /// 缓存中的一条（单测与后台任务用）
    pub(crate) fn cached(&self, id: i64) -> Option<ClipItemMeta> {
        self.cache.read().get(id).cloned()
    }

    /// 进入串行区：之后的「改库 + 改缓存」在离开前不会与别的改动交错。缓存的写锁只在 [`Serial::cache`] 期间持有，
    /// 改库（例如解码一张大图）期间首屏照常可读
    pub(crate) fn serial(&self) -> Serial<'_> {
        Serial {
            _guard: self.serial.lock(),
            cache: &self.cache,
        }
    }

    // ---------- 记录管线 ----------

    /// 回调序号去重：与最近处理过的相等时返回 false
    pub(crate) fn begin_change(&self, change: u64) -> bool {
        let mut last = self.last_change.lock();
        if *last == Some(change) {
            return false;
        }
        *last = Some(change);
        true
    }

    /// 读取一次剪贴板并记录。**会阻塞**（读取在平台层有 0.5 s 的硬截止，记录一张 4K 截图约 150 ms）
    pub(crate) fn capture(&self, change: u64) -> Capture {
        if !self.recording() {
            return Capture::Done;
        }
        let settings = self.host.settings();
        let raw = match self.port.clipboard().read(&read_limits(&settings.clipboard)) {
            Ok(raw) => raw,
            Err(e @ (PlatformError::Unsupported(_) | PlatformError::PermissionMissing(_))) => {
                log::info!("clipboard read unavailable: {e}");
                return Capture::Done;
            }
            Err(e) => {
                log::debug!("clipboard read failed change={change}: {e}");
                return Capture::Retry;
            }
        };
        // 读到的是打开剪贴板那一刻的序号：之后若回调报告的正是它，内容已经处理过了
        if raw.change_id != change {
            *self.last_change.lock() = Some(raw.change_id);
        }
        if raw.from_wisp {
            // Wisp 自己的写入（粘贴、copy_text）由写入方自己处理：touch 或直接记录（以 from_wisp 为准）
            log::debug!("clipboard change from Wisp skipped change={change}");
            return Capture::Done;
        }
        if raw.concealed {
            // 隐私标记：不打开数据库、不取图标、不发事件，库中不留任何痕迹
            log::debug!("clipboard change skipped reason=concealed");
            return Capture::Done;
        }
        if is_empty_read(&raw) && raw.change_id == change {
            // macOS 上 clearContents 递增序号、随后的 writeObjects 不递增：轮询正好落在两者之间时读到的是空剪贴板。
            // 当作 Skipped(Empty) 记下这个序号的话，紧接着写入的内容就永远丢了。按读取失败
            // 退避重试（120 / 360 / 1000 ms），期间有了新的序号就交给下一轮
            log::debug!("clipboard change {change} read empty; retrying (writer may still be writing)");
            return Capture::Retry;
        }
        // 读取期间可能刚好暂停
        if !self.recording() {
            return Capture::Done;
        }
        match self.ingest(&raw) {
            Ok(IngestOutcome::Inserted(_) | IngestOutcome::Touched(_)) => {
                *self.recorded_change.lock() = Some(raw.change_id);
            }
            Ok(IngestOutcome::Skipped(_)) => {}
            Err(e) => log::warn!("clipboard record failed: {}", e.message),
        }
        Capture::Done
    }

    /// 记录一份原始内容：持锁 `ingest` → `RecentCache::apply` → 推送 → 后台补图标 / 缩略图。**会阻塞**
    pub(crate) fn ingest(&self, raw: &RawClipboard) -> Result<IngestOutcome, IpcError> {
        self.ingest_with(raw, false).map(|(outcome, _)| outcome)
    }

    /// 与 [`Self::ingest`] 相同，另外返回新建条目的内容 hash（读不到时为 None）。hash 在插入的同一个串行区里读：出了
    /// 串行区，这一行可能已被删掉、行号分给了别的条目。**会阻塞**
    pub(crate) fn ingest_identified(&self, raw: &RawClipboard) -> Result<(IngestOutcome, Option<[u8; 32]>), IpcError> {
        self.ingest_with(raw, true)
    }

    fn ingest_with(&self, raw: &RawClipboard, identify: bool) -> Result<(IngestOutcome, Option<[u8; 32]>), IpcError> {
        let store = self.open_store("clipboard_event")?;
        let settings = self.host.settings();
        let policy = IngestPolicy::from(&settings.clipboard);
        let now = self.host.now_ms();
        let (outcome, hash) = {
            let serial = self.serial();
            let outcome = store.ingest(raw, &policy, now).map_err(store_error)?;
            let hash = match &outcome {
                IngestOutcome::Inserted(meta) if identify => sync_ident(&store, meta.id).map(|(hash, _)| hash),
                _ => None,
            };
            serial.cache().apply(&outcome);
            // 在串行区内投递（模块文档「事件的顺序」）
            match &outcome {
                IngestOutcome::Inserted(meta) => self.emit(ClipboardChanged::Inserted { item: meta.clone() }),
                IngestOutcome::Touched(meta) => self.emit(ClipboardChanged::Touched { item: meta.clone() }),
                IngestOutcome::Skipped(_) => {}
            }
            // 首次复制与再次复制都通知同步（在串行区内，与远端改动的应用不会交错）
            if let IngestOutcome::Inserted(meta) | IngestOutcome::Touched(meta) = &outcome {
                self.notify_sync(&store, meta.id, meta.kind, |sink, hash| sink.content(hash));
            }
            (outcome, hash)
        };
        // 后台任务在串行区外派发（单测的同步执行器会就地执行它们，它们自己也要进串行区）
        match &outcome {
            IngestOutcome::Inserted(meta) => {
                log::debug!("clipboard item inserted id={} kind={:?}", meta.id, meta.kind);
                self.after_record(meta, raw);
                self.idle.mark();
                // 验证码条目：到点自动删除（再次复制只 touch，不延长）
                if let Some(at) = meta.expires_at {
                    self.schedule_expiry(at);
                }
            }
            IngestOutcome::Touched(meta) => {
                log::debug!("clipboard item touched id={}", meta.id);
                self.after_record(meta, raw);
            }
            IngestOutcome::Skipped(SkipReason::Secret) => {
                log::info!("clipboard change skipped reason=secret");
                self.host.flash_tray();
                self.emit(ClipboardChanged::SecretSkipped);
            }
            IngestOutcome::Skipped(reason) => {
                log::debug!("clipboard change skipped reason={reason:?}");
            }
        }
        Ok((outcome, hash))
    }

    /// 记录之后的后台工作：缺图标的取图标，缺缩略图的生成缩略图（都不在关键路径上）
    fn after_record(&self, meta: &ClipItemMeta, raw: &RawClipboard) {
        if meta.image.as_ref().is_some_and(|i| i.thumb_url.is_none()) {
            let id = meta.id;
            self.spawn(move |e| e.make_thumb(id));
        }
        // 只为条目显示的那个来源取图标：重复内容被 touch 时来源保持最初的应用，这次复制的应用可能是另一个
        if let (Some(source), Some(app)) = (meta.source.as_ref(), raw.source.as_ref())
            && source.icon_url.is_none()
            && source.id == app.id
        {
            let app = app.clone();
            self.spawn(move |e| e.fetch_icon(&app));
        }
    }

    /// 补做缺失的缩略图（打开数据库后、每次空闲清理后各一次；最多 [`THUMB_REPAIR_LIMIT`] 条，最近使用的在前）。**会阻塞**
    pub(crate) fn repair_thumbs(&self) {
        let Some(store) = self.ready_store() else { return };
        let ids = match store.missing_thumbs(THUMB_REPAIR_LIMIT) {
            Ok(ids) => ids,
            Err(e) => {
                log::debug!("clipboard thumbnail repair query failed: {e}");
                return;
            }
        };
        if !ids.is_empty() {
            log::info!("clipboard: repairing {} missing thumbnail(s)", ids.len());
        }
        for id in ids {
            self.make_thumb(id);
        }
    }

    /// 后台生成缩略图，完成后原地更新缓存并推送 `updated`。库的改动不持串行锁，理由见模块
    /// 文档「不持串行锁的库改动」
    pub(super) fn make_thumb(&self, id: i64) {
        let Some(store) = self.ready_store() else { return };
        let meta = match store.make_thumb(id) {
            Ok(Some(meta)) => meta,
            Ok(None) => return,
            Err(e) => {
                log::warn!("clipboard thumbnail failed id={id}: {e}");
                return;
            }
        };
        let serial = self.serial();
        let cached = {
            let mut cache = serial.cache();
            cache.update(&meta);
            cache.get(id).cloned()
        };
        let item = match cached {
            Some(merged) => merged,
            // 不在缓存里：已被删除，或排在缓存之外（≥ 200 条置顶占满了缓存）。确认库里还有这一行才推送——图片条目的
            // 详情只是一次按主键的查询，不访问文件系统
            None => match store.get_detail(id) {
                Ok(Some(_)) => meta,
                Ok(None) => {
                    log::debug!("clipboard thumbnail ready for a deleted item id={id}; not pushed");
                    return;
                }
                Err(e) => {
                    log::debug!("clipboard thumbnail existence check failed id={id}: {e}");
                    return;
                }
            },
        };
        // 在串行区内投递（模块文档「事件的顺序」）
        self.emit(ClipboardChanged::Updated { item });
    }

    // ---------- 条目操作 ----------

    /// 粘贴时的 touch：只更新时间和次数，条目移到顶部。**会阻塞**
    pub(crate) fn touch(&self, store: &ClipStore, id: i64) -> Result<ClipItemMeta, IpcError> {
        let now = self.host.now_ms();
        let serial = self.serial();
        let mut meta = store.touch(id, now).map_err(store_error)?;
        let mut cache = serial.cache();
        keep_checked_files(&cache, &mut meta);
        cache.upsert(&meta);
        Ok(meta)
    }

    /// 收藏（置顶）/ 取消收藏。收藏去掉验证码的过期时间，取消收藏去掉名称（存储层的规则）。**会阻塞**
    pub(crate) fn pin(&self, id: i64, pinned: bool) -> Result<ClipItemMeta, IpcError> {
        let store = self.open_store("pin")?;
        let serial = self.serial();
        let meta = store.set_pinned(id, pinned).map_err(store_error)?;
        Ok(self.favorite_changed(&serial, &store, meta))
    }

    /// 给收藏命名（`name` 已在 IPC 第一步清理；None 清除名称）。命名即收藏，排序位置可能改变。**会阻塞**
    pub(crate) fn set_name(&self, id: i64, name: Option<&str>) -> Result<ClipItemMeta, IpcError> {
        let store = self.open_store("name")?;
        let serial = self.serial();
        let meta = store.set_name(id, name).map_err(store_error)?;
        Ok(self.favorite_changed(&serial, &store, meta))
    }

    /// 收藏状态或名称变了（调用方持串行锁）：通知同步（`p/` 的值带名称），更新缓存
    fn favorite_changed(&self, serial: &Serial<'_>, store: &ClipStore, mut meta: ClipItemMeta) -> ClipItemMeta {
        let (pinned, name) = (meta.pinned, meta.name.clone());
        // 「同步验证码」关闭时验证码条目的内容在导出时被跳过；收藏之后它按普通条目同步，要补发一次内容，
        // 否则其他设备只收到一个取不到内容的收藏
        let republish = pinned
            && self.sync_sink().is_some()
            && store.is_otp(meta.id).unwrap_or_else(|e| {
                log::warn!("clipboard verification code flag lookup failed: {e}");
                false
            });
        self.notify_sync(store, meta.id, meta.kind, |sink, hash| {
            sink.favorite(hash, pinned, name.as_deref());
            if republish {
                sink.content(hash);
            }
        });
        let mut cache = serial.cache();
        keep_checked_files(&cache, &mut meta);
        cache.upsert(&meta);
        meta
    }

    /// 删除；置顶项要求 force（前端负责二次确认）。条目不存在时视为已删除。**会阻塞**
    pub(crate) fn delete(&self, id: i64, force: bool) -> Result<(), IpcError> {
        self.delete_checked(id, force, None).map(drop)
    }

    /// 与 [`Self::delete`] 相同；`expect` 给出时只在这一行的内容 hash 仍等于它时才删（行号没有 AUTOINCREMENT，原条目
    /// 被删掉后行号可能分给了别的条目），核对与删除在同一个串行区里。返回是否删掉了一条。**会阻塞**
    pub(crate) fn delete_checked(&self, id: i64, force: bool, expect: Option<&[u8; 32]>) -> Result<bool, IpcError> {
        let store = self.open_store("delete")?;
        if !force && self.is_pinned(&store, id)? {
            return Err(IpcError::new(
                IpcErrorCode::Conflict,
                "pinned item requires force to delete",
            ));
        }
        let serial = self.serial();
        // 删除之后行就没了：同步要用的 hash 在删除之前、同一个串行区里取
        let ident = (expect.is_some() || self.sync_sink().is_some())
            .then(|| sync_ident(&store, id))
            .flatten();
        if let Some(want) = expect
            && ident.is_none_or(|(hash, _)| hash != *want)
        {
            return Ok(false);
        }
        let removed = store.delete(id).map_err(store_error)?;
        serial.cache().remove(id);
        if removed {
            if let (Some(sink), Some((hash, kind))) = (self.sync_sink(), ident)
                && kind != ClipKind::Files
            {
                sink.delete(hash);
            }
            log::debug!("clipboard item deleted id={id}");
            // 在串行区内投递（模块文档「事件的顺序」）
            self.emit(ClipboardChanged::Removed { id });
        }
        Ok(removed)
    }

    /// 条目是否置顶（不存在视为否）。先看缓存（[`pinned_in_cache`]，几乎总能回答）；只有缓存被 ≥ 200 条置顶占满、
    /// 又不在缓存里时才查库——`get_detail` 对文件条目会实时 stat 路径，所以不作为常规路径
    fn is_pinned(&self, store: &ClipStore, id: i64) -> Result<bool, IpcError> {
        if let Some(pinned) = pinned_in_cache(&self.cache.read(), id) {
            return Ok(pinned);
        }
        Ok(store
            .get_detail(id)
            .map_err(store_error)?
            .is_some_and(|d| d.meta.pinned))
    }

    /// 清空（同时删除 blob），返回删除条数。**会阻塞**（1000 条约 40 ms，1 万条约 0.5 s）
    pub(crate) fn clear(&self, keep_pinned: bool) -> Result<u32, IpcError> {
        let store = self.open_store("clear")?;
        let serial = self.serial();
        let n = store.clear(keep_pinned).map_err(store_error)?;
        if let Some(sink) = self.sync_sink() {
            sink.clear(keep_pinned);
        }
        serial.cache().clear(keep_pinned);
        // 在串行区内投递（模块文档「事件的顺序」）
        self.emit(ClipboardChanged::Cleared { keep_pinned });
        drop(serial);
        log::info!("clipboard history cleared count={n} keep_pinned={keep_pinned}");
        Ok(n)
    }
}

/// 一次最多补做多少张缺失的缩略图
const THUMB_REPAIR_LIMIT: u32 = 50;

/// 横幅文案：历史库来自更新版本的 Wisp（i18n key）
pub(crate) const HISTORY_TOO_NEW: &str = "clipboard.history_too_new";
/// 横幅文案：历史库打不开（i18n key）
pub(crate) const HISTORY_UNAVAILABLE: &str = "clipboard.history_unavailable";
/// 监听装不上、平台层没给出具体原因时（i18n key）
pub(crate) const WATCH_UNAVAILABLE: &str = "errors.platform.clipboard_watch_unavailable";

/// 监听安装失败 → 横幅的 i18n key：平台层的 `Unsupported` 已经是 `errors.platform.*`
fn watch_reason_key(e: &PlatformError) -> String {
    match e {
        PlatformError::Unsupported(key) if key.starts_with("errors.platform.") => key.clone(),
        _ => WATCH_UNAVAILABLE.to_owned(),
    }
}

/// 读到的剪贴板里什么可记录的内容都没有（文本、HTML、RTF、图片、文件都为空）
fn is_empty_read(raw: &RawClipboard) -> bool {
    raw.text.is_none() && raw.html.is_none() && raw.rtf.is_none() && raw.image_png.is_none() && raw.files.is_none()
}

/// 只凭缓存判断条目是否置顶。缓存是数据库按「置顶在前」排序后的前缀（`RecentCache` 的不变式），所以只要缓存里有
/// 非置顶条目、或缓存就是全部，所有置顶条目都在缓存里——不在缓存里的条目一定没有置顶。缓存全是置顶条目且不完整
/// （≥ 200 条置顶，或缓存加载失败而为空）时无法判断，返回 None
pub(crate) fn pinned_in_cache(cache: &RecentCache, id: i64) -> Option<bool> {
    if let Some(meta) = cache.get(id) {
        return Some(meta.pinned);
    }
    // limit = 0 只计数、不克隆条目：pinned_count 是缓存里置顶的条数
    if (cache.page(ClipFilter::All, 0).pinned_count as usize) < cache.len() {
        return Some(false);
    }
    // 缓存全是置顶条目（或为空）：缓存就是全部时同样可以断定
    cache
        .page(ClipFilter::All, u32::MAX)
        .next_offset
        .is_none()
        .then_some(false)
}
