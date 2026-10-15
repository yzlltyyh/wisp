//! 空闲清理：「面板隐藏且 30 s 无变更」后一次性调度保留策略、孤儿 blob 清理与 VACUUM 判定。
//!
//! - 有活干（[`Idle::mark`]）的时机：数据库打开（打开后的第一次空闲清孤儿 blob）、记录了新条目（可能超出
//!   条数上限）、保留设置变了（例如上限改为 10 后，空闲时清掉多余的）、列表里出现了按天数已过期的条目、
//!   面板隐藏时距上次执行保留策略已满 [`EXPIRY_RECHECK_MS`]（按天数保留时）；
//! - 调度任务（[`run`]）平时停在 `Notify` 与面板的会话结束通知上，零唤醒；有活时等到最后一次变更后满 [`QUIET`]，
//!   面板可见就等它隐藏（订阅 `panel_hidden`，事件驱动），然后在阻塞线程池里执行一次 [`Engine::run_cleanup`]；
//! - 清理：持串行锁 `retain`，有删除时重载缓存 → `sweep_orphan_blobs` → `vacuum_if_due`（距上次 ≥ 30 天，时间记在
//!   state.json 的 `last_vacuum_ms`）→ OCR 历史按条数上限修剪。VACUUM 不改变条目与顺序，不持串行锁（它可能要几百毫秒，
//!   期间粘贴的 touch 不必等它）。
//!
//! 按天数过期（7 / 30 / 90 天）不靠周期定时器：列表交给前端之前先去掉已过期的条目（[`Engine::hide_expired`]，显示
//! 永远正确），同时排一次清理把它们真正删掉；用户长时间不复制、只用翻译时，面板每次隐藏顺带看一眼，最多每
//! [`EXPIRY_RECHECK_MS`] 排一次清理（覆盖缓存之外、更旧的条目）。

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::Notify;
use wisp_clipboard::{RecentCache, RetentionPolicy};
use wisp_types::{ClipPage, ClipboardChanged};

use super::engine::Engine;

/// 最后一次变更之后的静默期
pub const QUIET: Duration = Duration::from_secs(30);
/// 一天的毫秒数
const DAY_MS: i64 = 86_400_000;
/// 按天数保留时，面板隐藏后距上次执行保留策略超过这么久就再排一次清理
pub(crate) const EXPIRY_RECHECK_MS: i64 = DAY_MS;

/// 「有活干」的状态
pub(crate) struct Idle {
    state: Mutex<IdleState>,
    notify: Notify,
    /// 上次成功执行保留策略的时间（Unix 毫秒；0 = 本进程还没执行过）
    last_retain_ms: AtomicI64,
    /// 调度任务醒来的次数与执行清理的次数（单测断言「空闲零唤醒」）
    #[cfg(test)]
    pub(crate) wakeups: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    pub(crate) runs: std::sync::atomic::AtomicUsize,
}

#[derive(Debug, Clone, Copy)]
struct IdleState {
    dirty: bool,
    last_change: Instant,
}

impl Default for Idle {
    fn default() -> Self {
        Self {
            state: Mutex::new(IdleState {
                dirty: false,
                last_change: Instant::now(),
            }),
            notify: Notify::new(),
            last_retain_ms: AtomicI64::new(0),
            #[cfg(test)]
            wakeups: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            runs: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

/// 下一步
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Plan {
    /// 没有活：等下一次 mark（或面板隐藏，顺带检查过期）
    Sleep,
    /// 静默期还没满：再等这么久（期间有新的 mark 就重新计算）
    Wait(Duration),
    /// 面板可见：等它隐藏
    WaitPanel,
    /// 执行清理
    Run,
}

/// 按状态决定下一步（纯函数，单测覆盖）
pub(crate) fn plan(dirty: bool, since_change: Duration, panel_visible: bool, quiet: Duration) -> Plan {
    if !dirty {
        Plan::Sleep
    } else if since_change < quiet {
        Plan::Wait(quiet - since_change)
    } else if panel_visible {
        Plan::WaitPanel
    } else {
        Plan::Run
    }
}

impl Idle {
    /// 记一笔「有活干」，静默期从现在重新算
    pub(crate) fn mark(&self) {
        {
            let mut s = self.state.lock();
            s.dirty = true;
            s.last_change = Instant::now();
        }
        self.notify.notify_one();
    }

    /// 是否有待执行的清理
    #[cfg(test)]
    pub(crate) fn dirty(&self) -> bool {
        self.state.lock().dirty
    }

    fn snapshot(&self) -> IdleState {
        *self.state.lock()
    }

    /// 开始执行前清掉标记（执行期间新的 mark 会再排一次）
    pub(crate) fn take(&self) {
        self.state.lock().dirty = false;
    }

    fn woke(&self) {
        #[cfg(test)]
        self.wakeups.fetch_add(1, Ordering::SeqCst);
    }
}

/// 调度任务（常驻；没活时停在 Notify 与面板的会话结束通知上）
pub(crate) async fn run(engine: Arc<Engine>) {
    run_with(engine, QUIET).await;
}

/// [`run`] 的实现（单测用较短的静默期）
pub(crate) async fn run_with(engine: Arc<Engine>, quiet: Duration) {
    let mut hidden = engine.host.panel_hidden();
    loop {
        let s = engine.idle.snapshot();
        // 先把已有的隐藏通知标为已读，再看面板状态：之后的隐藏一定会唤醒下面的 changed()
        if let Some(rx) = hidden.as_mut() {
            rx.borrow_and_update();
        }
        match plan(s.dirty, s.last_change.elapsed(), engine.host.panel_visible(), quiet) {
            Plan::Sleep => match hidden.as_mut() {
                Some(rx) => {
                    tokio::select! {
                        changed = rx.changed() => {
                            if changed.is_err() {
                                hidden = None;
                            } else {
                                // 面板（任一视图）刚隐藏：按天数保留时顺带看看是否该清理
                                engine.check_expiry_on_hide();
                            }
                        }
                        () = engine.idle.notify.notified() => {}
                    }
                }
                None => engine.idle.notify.notified().await,
            },
            Plan::Wait(d) => {
                tokio::select! {
                    () = tokio::time::sleep(d) => {}
                    () = engine.idle.notify.notified() => {}
                }
            }
            Plan::WaitPanel => match hidden.as_mut() {
                Some(rx) => {
                    tokio::select! {
                        changed = rx.changed() => {
                            if changed.is_err() {
                                hidden = None;
                            }
                        }
                        () = engine.idle.notify.notified() => {}
                    }
                }
                None => tokio::time::sleep(quiet).await,
            },
            Plan::Run => {
                engine.idle.take();
                let e = Arc::clone(&engine);
                if let Err(err) = tauri::async_runtime::spawn_blocking(move || e.run_cleanup()).await {
                    log::warn!("clipboard cleanup task failed: {err}");
                }
            }
        }
        engine.idle.woke();
    }
}

/// 一次清理的结果（日志与单测用）
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CleanupReport {
    pub removed_items: u32,
    pub removed_blobs: u32,
    pub orphans: u32,
    pub vacuumed: bool,
    pub ocr_trimmed: u32,
}

/// 页面里按天数已过期的条目去掉（纯函数，单测覆盖）：非置顶且 `last_used_at` 早于 `cutoff`。列表与检索结果都是
/// 「置顶在前、其余按最近使用排序」，过期的是非置顶部分的一段后缀，之后的条目只会更旧——去掉了就不再翻页。
/// 返回去掉的条数
pub(crate) fn drop_expired(page: &mut ClipPage, cutoff: i64) -> usize {
    let before = page.items.len();
    page.items.retain(|h| h.item.pinned || h.item.last_used_at >= cutoff);
    let dropped = before - page.items.len();
    if dropped > 0 {
        page.next_offset = None;
    }
    dropped
}

impl Engine {
    /// 按天数保留时的截止时间（`last_used_at` 早于它的非置顶条目已过期，与 `ClipStore::retain` 同一口径）
    fn expiry_cutoff(&self) -> Option<i64> {
        let days = self.host.settings().clipboard.max_age_days?;
        Some(
            self.host
                .now_ms()
                .saturating_sub(i64::from(days).saturating_mul(DAY_MS)),
        )
    }

    /// 页面交给前端之前：去掉按天数已过期的条目，并排一次空闲清理把它们真正删掉
    pub(crate) fn hide_expired(&self, page: &mut ClipPage) {
        let Some(cutoff) = self.expiry_cutoff() else { return };
        let dropped = drop_expired(page, cutoff);
        if dropped > 0 {
            log::debug!("clipboard page hides {dropped} expired item(s); cleanup scheduled");
            self.idle.mark();
        }
    }

    /// 面板（任一视图）隐藏后调用：按天数保留、数据库已打开，且距上次执行保留策略已满 [`EXPIRY_RECHECK_MS`] 时排一次
    /// 清理。不阻塞
    pub(crate) fn check_expiry_on_hide(&self) {
        // 在调度任务（异步上下文）里调用：只读无锁的标志，不碰数据库
        if self.host.settings().clipboard.max_age_days.is_none() || !self.store_opened() {
            return;
        }
        let last = self.idle.last_retain_ms.load(Ordering::Acquire);
        if self.host.now_ms().saturating_sub(last) >= EXPIRY_RECHECK_MS {
            self.idle.mark();
        }
    }

    /// 执行一次空闲清理。数据库还没打开时什么都不做（不为清理而打开；打开时会另排一次）。**会阻塞**
    pub(crate) fn run_cleanup(&self) -> CleanupReport {
        let mut report = CleanupReport::default();
        let Some(store) = self.ready_store() else {
            return report;
        };
        #[cfg(test)]
        self.idle.runs.fetch_add(1, Ordering::SeqCst);
        let started = Instant::now();
        let settings = self.host.settings();
        let now = self.host.now_ms();
        let policy = RetentionPolicy::from(&settings.clipboard);
        {
            let serial = self.serial();
            match store.retain(&policy, now) {
                Ok(r) => {
                    self.idle.last_retain_ms.store(now, Ordering::Release);
                    report.removed_items = r.removed_items;
                    report.removed_blobs = r.removed_blobs;
                    if r.removed_items > 0 {
                        let before = cached_ids(&serial.cache_read());
                        match RecentCache::load(&store) {
                            Ok(cache) => *serial.cache() = cache,
                            Err(e) => {
                                log::warn!("clipboard cache reload failed: {e}");
                                *serial.cache() = RecentCache::default();
                            }
                        }
                        // 告诉（可能正要打开的）视图哪些行已经没了：保留策略只删除、不改排序，原来在缓存里、重新加载
                        // 后不在的就是被删的。在串行区内投递（engine.rs「事件的顺序」）；视图准备中时由 outbox 排队
                        let after = cached_ids(&serial.cache_read());
                        for id in before.difference(&after) {
                            self.emit(ClipboardChanged::Removed { id: *id });
                        }
                    }
                }
                Err(e) => log::warn!("clipboard retention failed: {e}"),
            }
        }
        match store.sweep_orphan_blobs() {
            Ok(n) => report.orphans = n,
            Err(e) => log::warn!("clipboard orphan sweep failed: {e}"),
        }
        match store.vacuum_if_due(now, self.host.last_vacuum_ms()) {
            Ok(true) => {
                report.vacuumed = true;
                self.host.set_last_vacuum_ms(now);
            }
            Ok(false) => {}
            Err(e) => log::warn!("clipboard vacuum failed: {e}"),
        }
        // 调小 `ocr.history_limit` 之后补裁；裁掉的推送给设置窗口的历史列表
        report.ocr_trimmed = self.ocr_history_trim(&store, settings.ocr.history_limit);
        log::info!(
            "clipboard cleanup removed={} blobs={} orphans={} vacuumed={} ocr_trimmed={} elapsed_ms={}",
            report.removed_items,
            report.removed_blobs,
            report.orphans,
            report.vacuumed,
            report.ocr_trimmed,
            started.elapsed().as_millis()
        );
        // 生成时遇到暂时性错误的缩略图在空闲时再补一次（没有缺失时只是一次索引查询）
        self.repair_thumbs();
        report
    }
}
/// 缓存里全部条目的 id
fn cached_ids(cache: &RecentCache) -> std::collections::BTreeSet<i64> {
    cache
        .page(wisp_types::ClipFilter::All, u32::MAX)
        .items
        .iter()
        .map(|h| h.item.id)
        .collect()
}
