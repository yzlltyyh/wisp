//! 智能识别在剪贴板服务里的接入：条目检视（芯片条、「粘贴为…」、⌘K 子页共用一次结果）与验证码条目的到期删除。
//! 识别与变换本身在 `wisp_clipboard::smart`（识别器 / 变换注册表），这里只负责缓存、排程与事件。
//!
//! # 检视
//! 按需计算、不落库。[`Engine::inspect`] 按条目内容的 hash 缓存最近 [`INSPECT_CACHE`] 条：内容不可变，同一个 hash
//! 的结果只取决于识别设置，设置一变整个缓存作废（[`InspectCache`]）。纯文本（识别结果等）用 [`inspect_text`]，不缓存。
//!
//! # 验证码到期
//! - 排程：一次性的 [`Exec::after`](super::port::Exec::after)，不是周期定时器。打开数据库时、记录或接收了带过期时间的
//!   条目时，按最早的过期时间排一次；到点删除已到期的，再按下一个过期时间排下一次。只记着「已排的最早时刻」，更晚的
//!   请求不重复排，所以同一时刻在途的定时器只有寥寥几个；
//! - 系统睡眠期间（tokio 的单调时钟不走）定时器会晚到：列表与检索结果交给前端之前过滤掉已到期的
//!   （[`Engine::hide_expired_codes`]），查库之前发现排好的时刻已经过了就当场删除（[`Engine::expire_if_overdue`]）；
//! - 删除：串行区内删库（不留明文）+ 删缓存 + 推送 `removed`。与保留策略的清理一样**不通知同步**：过期时间随条目
//!   同步，每台设备按同一个绝对时刻各自删除，收到时已经过期的不导入。发布删除记录反而会出错——删除记录不看收藏，
//!   另一台设备在到期前收藏了这一条、而本机睡眠或没在运行时，本机醒来先到期删除再拉取，发出去的删除会把那边的收藏删掉；
//!   不发删除时，那边的收藏照常让这一条在本机重新出现；
//! - 删库持续失败（磁盘满、只读）时至少隔 [`EXPIRY_RETRY_MS`] 再试，不按已经过去的时刻立即重排而空转；
//! - 内容与识别结果都不写日志，只记条数。

use std::num::NonZeroUsize;
use std::time::Duration;

use lru::LruCache;
use parking_lot::Mutex;
use wisp_clipboard::otp_expired;
use wisp_clipboard::smart::{self, RecognizeOpts};
use wisp_types::{ClipInspect, ClipKind, ClipPage, ClipboardChanged, SmartSettings};

use crate::error::IpcError;

use super::engine::{Engine, store_error};

/// 检视结果缓存的条数（光标在列表里来回移动时，最近看过的几条不必重算）
pub(crate) const INSPECT_CACHE: usize = 8;

/// 检视结果的缓存：内容 hash → 结果，连同算出这些结果时的识别设置
pub(crate) struct InspectCache {
    inner: Mutex<Cached>,
}

struct Cached {
    /// 下面这些结果按哪套识别设置算出（None = 还没算过）
    opts: Option<RecognizeOpts>,
    results: LruCache<[u8; 32], ClipInspect>,
}

impl Default for InspectCache {
    fn default() -> Self {
        let cap = NonZeroUsize::new(INSPECT_CACHE).unwrap_or(NonZeroUsize::MIN);
        Self {
            inner: Mutex::new(Cached {
                opts: None,
                results: LruCache::new(cap),
            }),
        }
    }
}

impl InspectCache {
    fn get(&self, hash: &[u8; 32], opts: &RecognizeOpts) -> Option<ClipInspect> {
        let mut g = self.inner.lock();
        if g.opts.as_ref() != Some(opts) {
            // 识别设置变了：旧结果按旧设置算的，全部作废
            g.opts = Some(*opts);
            g.results.clear();
            return None;
        }
        g.results.get(hash).cloned()
    }

    fn put(&self, hash: [u8; 32], opts: &RecognizeOpts, result: ClipInspect) {
        let mut g = self.inner.lock();
        if g.opts.as_ref() == Some(opts) {
            g.results.put(hash, result);
        }
    }

    /// 缓存的条数（单测用）
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.lock().results.len()
    }
}

/// 纯文本（识别结果、片段）的检视：按文本条目对待
pub(crate) fn inspect_text(text: &str, smart: &SmartSettings) -> ClipInspect {
    smart::inspect(ClipKind::Text, text, &RecognizeOpts::from(smart))
}

/// 到期删除失败后至少隔这么久再试（毫秒）
pub(crate) const EXPIRY_RETRY_MS: i64 = 60_000;

/// 已排的验证码到期任务：最早的那一个时刻（Unix 毫秒）
#[derive(Default)]
pub(crate) struct ExpirySchedule {
    due: Mutex<Option<i64>>,
}

/// 下一次到期任务的时刻：删除失败时最早的过期时间往往已经过去，按它重排会以 0 ms 反复触发、空转并刷日志
pub(crate) fn next_expiry_run(next: i64, now: i64, failed: bool) -> i64 {
    if failed {
        next.max(now.saturating_add(EXPIRY_RETRY_MS))
    } else {
        next
    }
}

impl Engine {
    /// 条目的检视（实体与可用变换）。条目不存在时返回 `not_found`；图片与文件没有正文，结果为空。**会阻塞**
    pub(crate) fn inspect(&self, id: i64) -> Result<ClipInspect, IpcError> {
        let store = self.open_store("inspect")?;
        let not_found = || IpcError::not_found("clipboard item not found");
        let hash = store.hash_of(id).map_err(store_error)?.ok_or_else(not_found)?;
        let opts = RecognizeOpts::from(&self.host.settings().clipboard.smart);
        if let Some(hit) = self.inspect_cache.get(&hash, &opts) {
            return Ok(hit);
        }
        let result = store.inspect(id, &opts).map_err(store_error)?.ok_or_else(not_found)?;
        log::debug!(
            "clipboard inspect id={id} entities={} transforms={}",
            result.entities.len(),
            result.transforms.len()
        );
        self.inspect_cache.put(hash, &opts, result.clone());
        Ok(result)
    }

    // ---------- 验证码到期 ----------

    /// 在 `at`（Unix 毫秒）排一次到期删除；已经排了不晚于它的就什么都不做。不阻塞
    pub(crate) fn schedule_expiry(&self, at: i64) {
        {
            let mut due = self.expiry.due.lock();
            if due.is_some_and(|d| d <= at) {
                return;
            }
            *due = Some(at);
        }
        let wait = u64::try_from(at.saturating_sub(self.host.now_ms())).unwrap_or(0);
        let me = self.me.clone();
        self.exec.after(
            Duration::from_millis(wait),
            Box::new(move || {
                // 定时任务本身不得阻塞：删库放进阻塞线程池
                if let Some(engine) = me.upgrade() {
                    engine.spawn(move |e| e.expiry_fired(at));
                }
            }),
        );
    }

    fn expiry_fired(&self, at: i64) {
        {
            let mut due = self.expiry.due.lock();
            if *due == Some(at) {
                *due = None;
            }
        }
        self.expire_codes();
    }

    /// 排好的到期时刻已经过了（定时器因为系统睡眠还没触发）就当场删除。查库之前调用：之后的结果与偏移都是删除之后的。
    /// **会阻塞**
    pub(crate) fn expire_if_overdue(&self) {
        let overdue = self.expiry.due.lock().is_some_and(|d| d <= self.host.now_ms());
        if overdue {
            self.expire_codes();
        }
    }

    /// 删除已到期的验证码条目，再按下一个过期时间排程。数据库没打开时什么都不做（打开时会调用一次）。**会阻塞**
    pub(crate) fn expire_codes(&self) {
        let Some(store) = self.ready_store() else { return };
        let now = self.host.now_ms();
        let failed = {
            let serial = self.serial();
            match store.expire_due(now) {
                Ok(due) if !due.is_empty() => {
                    {
                        let mut cache = serial.cache();
                        for (id, _) in &due {
                            cache.remove(*id);
                        }
                    }
                    // 不通知同步（见模块说明「删除」）
                    for (id, _) in &due {
                        // 在串行区内投递（engine.rs「事件的顺序」）
                        self.emit(ClipboardChanged::Removed { id: *id });
                    }
                    log::info!("clipboard: {} verification code item(s) expired", due.len());
                    false
                }
                Ok(_) => false,
                Err(e) => {
                    log::warn!("clipboard verification code expiry failed: {e}");
                    true
                }
            }
        };
        match store.next_expiry() {
            Ok(Some(next)) => self.schedule_expiry(next_expiry_run(next, now, failed)),
            Ok(None) => {}
            Err(e) => log::debug!("clipboard next expiry lookup failed: {e}"),
        }
    }

    /// 页面交给前端之前去掉已到期的验证码条目（定时器晚到时，显示也永远正确），并在后台把它们删掉。被去掉的条目都在这一页
    /// 里：删除之后数据库里的偏移随之前移，`next_offset` 同样前移（下一页查库前会先完成删除，见
    /// [`Engine::expire_if_overdue`]）
    pub(crate) fn hide_expired_codes(&self, page: &mut ClipPage) {
        let now = self.host.now_ms();
        let before = page.items.len();
        page.items.retain(|h| !otp_expired(&h.item, now));
        let dropped = before - page.items.len();
        if dropped > 0 {
            log::debug!("clipboard page hides {dropped} expired verification code item(s)");
            let shift = u32::try_from(dropped).unwrap_or(u32::MAX);
            page.next_offset = page.next_offset.map(|o| o.saturating_sub(shift));
            self.spawn(|e| e.expire_codes());
        }
    }
}
