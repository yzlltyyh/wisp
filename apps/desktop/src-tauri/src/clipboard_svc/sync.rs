//! 多设备同步的接入点：本机改动的通知（[`ClipSyncSink`]）与远端改动的应用（[`Engine::apply_remote_batch`]、
//! [`Engine::apply_remote_clear`]、[`Engine::apply_remote_ocr`]）。同步层（`sync_svc`）只经这里读写剪贴板与识别历史。
//!
//! # 本机改动 → 同步
//! 用户的操作在**串行区内、存储调用成功之后**调用 sink（识别历史在它自己的串行锁内），sink 只做 O(1) 的工作（分配时间戳、
//! 把一条小记录推进队列），不阻塞记录与粘贴：
//! - 记录（首次复制 inserted、再次复制 touched）→ [`ClipSyncSink::content`]；
//! - 收藏 / 取消收藏 / 命名 → [`ClipSyncSink::favorite`]（收藏的是验证码时另补一次 [`ClipSyncSink::content`]：
//!   「同步验证码」关闭时它的内容没有发布过）；删除 → [`ClipSyncSink::delete`]（hash 在删除之前取）；
//!   清空 → [`ClipSyncSink::clear`]；
//! - 识别历史：新增 → [`ClipSyncSink::ocr_add`]；删除、以及被重新识别取代（`replaces`）→ [`ClipSyncSink::ocr_delete`]
//!   （键与删除在同一个存储事务里读取）；清空 → [`ClipSyncSink::ocr_clear`]。
//!
//! 粘贴时的 touch、保留策略与识别历史的条数裁剪都**不**通知：它们只属于本机（一台把上限设为 10 的设备不能清掉所有设备
//! 的历史）。验证码条目的到期删除同样不通知：过期时间随条目同步，每台设备按同一时刻各自删除；发布删除记录反而会压过
//! 另一台设备在到期前的收藏（见 `smart.rs`）。文件条目不同步，也不通知。
//!
//! # 远端改动 → 本机
//! 应用路径直接调用存储层的同步接口，从不经过上面那些入口，因此不会调用 sink——「远端写入被当作本机改动再发回去」由
//! 代码路径杜绝，不靠「正在应用远端」之类的时间段标志（那样的标志一旦覆盖了释放串行锁的间隙，期间用户真正的改动就会
//! 被漏掉）。
//!
//! 解码与检查（图片要逐行解码重算像素 hash）在串行区之外完成；每个串行区最多写 [`MAX_REMOTE_BATCH`] 条，记录与粘贴的
//! touch 最多等一个小事务。数据库还没打开时一律返回 [`RemoteOutcome::Deferred`]：同步从不为自己打开数据库。
//!
//! 剪贴板视图的事件（`clipboard:changed`）：
//! - 视图已显示：逐条推送——落进缓存前缀的新条目 `inserted`（更旧的不推，前端按偏移翻页时自然取到）；置顶或最近使用
//!   变化的 `touched`（前端按新的排序键移动它）；只改了创建时间的 `updated`；删除的 `removed`（远端的「清空」只推缓存
//!   前缀里被删的）。显示期间从不发 `reloaded`：前端会把它暂存到下次显示，列表就一直停在旧内容上。每个串行区的条数有
//!   上限，逐条推送的量也就有上限；
//! - 视图准备中或即将打开：发 `reloaded`（排队到视图就绪；这时用户还没开始操作，前端立即重新拉首页）；
//! - 面板没有显示剪贴板视图：不推送（缓存照常更新，下次打开时首屏就是新的）。

use std::sync::Arc;

use wisp_clipboard::{
    ClipStore, IngestOutcome, IngestPolicy, MAX_REMOTE_BATCH, OcrApplied, PreparedOcr, PreparedRemote, Rejected,
    RemoteApplied, RemoteItem, RemoteOcr, RetentionPolicy,
};
use wisp_types::{ClipItemMeta, ClipKind, ClipboardChanged, OcrHistoryChanged};

use super::engine::{Engine, Serial};
use super::outbox::Delivery;

/// 本机剪贴板 / 识别历史改动的接收方（由同步服务实现）。每个方法都在剪贴板服务的串行区内调用，必须是 O(1) 且不阻塞的
/// （分配时间戳、推进队列），不得回调剪贴板服务
pub trait ClipSyncSink: Send + Sync {
    /// 条目被记录（首次复制或再次复制）：之后按 hash 从库里导出内容
    fn content(&self, hash: [u8; 32]);
    /// 收藏状态或名称变了（收藏 / 取消收藏 / 命名）。`name` 只在收藏时有意义
    fn favorite(&self, hash: [u8; 32], pinned: bool, name: Option<&str>);
    /// 用户删除了一条
    fn delete(&self, hash: [u8; 32]);
    /// 用户清空了历史（`keep_pinned`：置顶的保留）
    fn clear(&self, keep_pinned: bool);
    /// 新增了一条识别记录（键见 `wisp_clipboard::ocr_key`）
    fn ocr_add(&self, key: &str);
    /// 删除了一条识别记录（用户删除，或被同一张截图的重新识别取代）
    fn ocr_delete(&self, key: &str);
    /// 清空了识别历史
    fn ocr_clear(&self);
}

/// 一条要应用到本机剪贴板历史的远端改动（同步层按胜出的记录算好）
#[derive(Debug)]
pub enum ClipRemoteChange {
    /// 条目存在：没有就插入，有就合并时间与置顶（`RemoteItem::pinned` 为 None 时保留本机的置顶状态）
    Upsert(RemoteItem),
    /// 只改收藏状态与名称（收藏时名称换成 `name`，取消收藏时去掉）。本机没有这一条时：收藏报告
    /// [`RemoteOutcome::Evicted`]（同步层据此重新取回这一条），取消收藏什么都不做
    Pin {
        hash: [u8; 32],
        pinned: bool,
        name: Option<String>,
    },
    /// 条目不再存在（被删除，或被清空隐藏）
    Delete { hash: [u8; 32] },
}

impl ClipRemoteChange {
    /// 改动针对的条目
    pub fn hash(&self) -> &[u8; 32] {
        match self {
            Self::Upsert(item) => &item.hash,
            Self::Pin { hash, .. } | Self::Delete { hash } => hash,
        }
    }
}

/// 远端的「清空」作用于本机那些同步层没有记录的条目（见 `ClipStore::delete_unsynced_before`）
#[derive(Clone)]
pub struct ClipRemoteClear {
    pub wall_ms: i64,
    pub keep_pinned: bool,
    /// 同步层有记录、由它逐条判断的条目
    pub exclude: Vec<[u8; 32]>,
}

impl std::fmt::Debug for ClipRemoteClear {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipRemoteClear")
            .field("wall_ms", &self.wall_ms)
            .field("keep_pinned", &self.keep_pinned)
            .field("exclude", &self.exclude.len())
            .finish()
    }
}

/// 一条要应用到本机识别历史的远端改动
#[derive(Debug)]
pub enum OcrRemoteChange {
    Upsert(RemoteOcr),
    Delete { key: String },
}

impl OcrRemoteChange {
    /// 改动针对的记录（同步键）
    pub fn key(&self) -> &str {
        match self {
            Self::Upsert(item) => &item.key,
            Self::Delete { key } => key,
        }
    }
}

/// 一条远端改动的应用结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteOutcome {
    /// 已写入本机（或本机已经是这个状态）
    Applied,
    /// 暂时没能写入（数据库还没打开 / 不可用、读写文件失败）：稍后重试
    Deferred,
    /// 本机策略不接收（隐私规则、类型开关、大小与保留上限、内容与身份不符、本机关闭了这类历史）：同一版本的记录不再重试
    Evicted,
    /// 本机在应用之前有了更新的改动（[`Superseded`] 判定）：跳过，本机的改动随后会胜出
    Superseded,
}

/// 「本机是否有比这条远端改动更新、尚未上传的改动」的判定（同步层提供）。在应用远端改动的串行区内调用，与本机改动通知
/// sink 处在同一个串行区里——判定与写入之间插不进本机的新改动。必须是 O(1)、不阻塞的
pub type Superseded = Arc<dyn Fn(&[u8; 32]) -> bool + Send + Sync>;

/// 同上，识别历史（参数是同步键）
pub type OcrSuperseded = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// 远端条目在锁外准备之后的形态
enum Op {
    Upsert(PreparedRemote),
    Pin([u8; 32], bool, Option<String>),
    Delete([u8; 32]),
}

impl Op {
    fn hash(&self) -> &[u8; 32] {
        match self {
            Self::Upsert(p) => p.hash(),
            Self::Pin(h, ..) | Self::Delete(h) => h,
        }
    }
}

/// 一次应用的累计结果
#[derive(Default)]
struct Totals {
    /// 需要后台生成缩略图的新图片
    thumbs: Vec<i64>,
    inserted: bool,
    removed: bool,
    /// 新插入的验证码条目里最早的过期时间（随后排一次到期删除）
    expiry: Option<i64>,
}

impl Engine {
    /// 安装 / 卸载同步通知（同步开启 / 关闭时由同步服务调用）。不阻塞
    pub(crate) fn set_sync_sink(&self, sink: Option<Arc<dyn ClipSyncSink>>) {
        *self.sync_sink.write() = sink;
    }

    /// 当前的同步通知（克隆一个 Arc）
    pub(crate) fn sync_sink(&self) -> Option<Arc<dyn ClipSyncSink>> {
        self.sync_sink.read().clone()
    }

    /// 在串行区内通知同步：取这一条的 hash（只在装了 sink 时多一次按主键的查询），文件条目不通知
    pub(crate) fn notify_sync(
        &self,
        store: &ClipStore,
        id: i64,
        kind: ClipKind,
        f: impl FnOnce(&dyn ClipSyncSink, [u8; 32]),
    ) {
        if kind == ClipKind::Files {
            return;
        }
        let Some(sink) = self.sync_sink() else { return };
        match store.hash_of(id) {
            Ok(Some(hash)) => f(&*sink, hash),
            Ok(None) => {}
            Err(e) => log::warn!("clipboard sync notify failed: {e}"),
        }
    }

    // ---------- 剪贴板 ----------

    /// 应用一批远端改动，结果与输入一一对应。**会阻塞**（图片解码、数据库），只在阻塞线程上调用。
    /// 本机关闭了剪贴板历史（`clipboard.enabled`，只属于本机）时不接收新条目，置顶与删除照常作用于已有条目
    pub(crate) fn apply_remote_batch(
        &self,
        changes: Vec<ClipRemoteChange>,
        superseded: Option<&Superseded>,
    ) -> Vec<RemoteOutcome> {
        let mut outcomes = vec![RemoteOutcome::Deferred; changes.len()];
        // 同步从不为自己打开数据库（启动后的空闲时刻会预加载；停用 / 打不开时之后再试）
        let Some(store) = self.ready_store() else {
            return outcomes;
        };
        let settings = self.host.settings();
        let accept = settings.clipboard.enabled;
        // 「同步验证码」关闭（只属于本机）：其他设备的验证码条目不导入
        let accept_codes = settings.sync.content.verification_codes;
        let policy = IngestPolicy::from(&settings.clipboard);
        let retention = RetentionPolicy::from(&settings.clipboard);
        let now = self.host.now_ms();

        // 锁外：本机策略、解码与 hash 核对、原图复制
        let mut ops = Vec::with_capacity(changes.len());
        for (i, change) in changes.into_iter().enumerate() {
            match change {
                ClipRemoteChange::Upsert(_) if !accept => outcomes[i] = RemoteOutcome::Evicted,
                ClipRemoteChange::Upsert(item) if !accept_codes && is_code_item(&item) => {
                    outcomes[i] = RemoteOutcome::Evicted;
                }
                ClipRemoteChange::Upsert(item) => match store.prepare_remote(item, &policy, &retention, now) {
                    Ok(p) => ops.push((i, Op::Upsert(p))),
                    Err(Rejected::Policy(reason)) => {
                        log::debug!("clipboard remote item not accepted reason={reason:?}");
                        outcomes[i] = RemoteOutcome::Evicted;
                    }
                    Err(Rejected::Failed(e)) => log::warn!("clipboard remote item deferred: {e}"),
                },
                ClipRemoteChange::Pin { hash, pinned, name } => ops.push((i, Op::Pin(hash, pinned, name))),
                ClipRemoteChange::Delete { hash } => ops.push((i, Op::Delete(hash))),
            }
        }

        let mut totals = Totals::default();
        let mut ops = ops.into_iter().peekable();
        while ops.peek().is_some() {
            let chunk: Vec<_> = ops.by_ref().take(MAX_REMOTE_BATCH).collect();
            self.apply_chunk(&store, chunk, superseded, &mut outcomes, &mut totals);
        }
        self.after_remote(&store, totals);
        outcomes
    }

    /// 一个串行区：最多 [`MAX_REMOTE_BATCH`] 条改动落库、同步更新缓存、按视图状态推送
    fn apply_chunk(
        &self,
        store: &ClipStore,
        chunk: Vec<(usize, Op)>,
        superseded: Option<&Superseded>,
        outcomes: &mut [RemoteOutcome],
        totals: &mut Totals,
    ) {
        let serial = self.serial();
        let mut upserts = Vec::new();
        let mut upsert_at = Vec::new();
        let mut pins = Vec::new();
        let mut deletes = Vec::new();
        let mut delete_at = Vec::new();
        for (i, op) in chunk {
            // 在串行区内判定：本机的改动也在串行区内通知 sink，两者不会交错
            if superseded.is_some_and(|f| f(op.hash())) {
                outcomes[i] = RemoteOutcome::Superseded;
                continue;
            }
            match op {
                Op::Upsert(p) => {
                    upsert_at.push(i);
                    upserts.push(p);
                }
                Op::Pin(hash, pinned, name) => pins.push((i, hash, pinned, name)),
                Op::Delete(hash) => {
                    delete_at.push(i);
                    deletes.push(hash);
                }
            }
        }
        let mut events = Vec::new();
        // 同一条的置顶在内容之后应用（新插入的条目随后置顶）；同一条不会既存在又被删除
        if !upserts.is_empty() {
            match store.apply_remote_many(upserts) {
                Ok(results) => {
                    for (i, r) in upsert_at.into_iter().zip(results) {
                        outcomes[i] = self.cache_remote(&serial, r, &mut events, totals);
                    }
                }
                Err(e) => log::warn!("clipboard remote items deferred: {e}"),
            }
        }
        for (i, hash, pinned, name) in pins {
            outcomes[i] = match store.set_favorite_by_hash(&hash, pinned, name.as_deref()) {
                Ok(Some(meta)) => {
                    let was = serial
                        .cache_read()
                        .get(meta.id)
                        .map(|m| (m.pinned, m.name.clone(), m.expires_at));
                    if was != Some((meta.pinned, meta.name.clone(), meta.expires_at)) {
                        reposition(&serial, &meta, &mut events);
                    }
                    RemoteOutcome::Applied
                }
                // 本机没有这一条（保留策略清掉了、或之前被拒收）：置顶让它重新属于应当保留的条目，报告「未接收」，同步层
                // 据此重新取回内容（置顶的条目不受保留上限约束）；取消置顶对不存在的条目没有影响
                Ok(None) if pinned => RemoteOutcome::Evicted,
                Ok(None) => RemoteOutcome::Applied,
                Err(e) => {
                    log::warn!("clipboard remote pin deferred: {e}");
                    RemoteOutcome::Deferred
                }
            };
        }
        if !deletes.is_empty() {
            match store.delete_by_hashes(&deletes) {
                Ok(ids) => {
                    let mut cache = serial.cache();
                    for id in ids {
                        cache.remove(id);
                        events.push(ClipboardChanged::Removed { id });
                        totals.removed = true;
                    }
                    drop(cache);
                    for i in delete_at {
                        outcomes[i] = RemoteOutcome::Applied;
                    }
                }
                Err(e) => log::warn!("clipboard remote deletions deferred: {e}"),
            }
        }
        self.deliver(events);
        drop(serial);
    }

    /// 一条落库结果 → 缓存与事件
    fn cache_remote(
        &self,
        serial: &Serial<'_>,
        r: RemoteApplied,
        events: &mut Vec<ClipboardChanged>,
        totals: &mut Totals,
    ) -> RemoteOutcome {
        match r {
            RemoteApplied::Inserted(meta) => {
                totals.inserted = true;
                if let Some(at) = meta.expires_at {
                    totals.expiry = Some(totals.expiry.map_or(at, |t| t.min(at)));
                }
                if meta.image.as_ref().is_some_and(|i| i.thumb_url.is_none()) {
                    totals.thumbs.push(meta.id);
                }
                let mut cache = serial.cache();
                cache.apply(&IngestOutcome::Inserted(meta.clone()));
                // 只推落进缓存前缀的：更旧的条目在已加载的列表之后，前端按偏移翻页时自然取到（提前塞进列表末尾会让
                // 之后的翻页错位）
                if cache.get(meta.id).is_some() {
                    events.push(ClipboardChanged::Inserted { item: meta });
                }
                RemoteOutcome::Applied
            }
            RemoteApplied::Merged { meta, moved: true } => {
                reposition(serial, &meta, events);
                RemoteOutcome::Applied
            }
            RemoteApplied::Merged { meta, moved: false } => {
                let mut cache = serial.cache();
                cache.update(&meta);
                if let Some(merged) = cache.get(meta.id) {
                    events.push(ClipboardChanged::Updated { item: merged.clone() });
                }
                RemoteOutcome::Applied
            }
            RemoteApplied::Unchanged(_) => RemoteOutcome::Applied,
            RemoteApplied::Rejected(reason) => {
                log::debug!("clipboard remote item not accepted reason={reason:?}");
                RemoteOutcome::Evicted
            }
        }
    }

    /// 按视图状态推送一个串行区的事件（调用方持串行锁，见模块文档）。调用方保证条数有上限（一个串行区最多
    /// [`MAX_REMOTE_BATCH`] 条改动；清空只推缓存前缀里的）
    fn deliver(&self, events: Vec<ClipboardChanged>) {
        if events.is_empty() {
            return;
        }
        match self.delivery() {
            Delivery::Shown => {
                for e in events {
                    self.emit(e);
                }
            }
            Delivery::Soon => self.emit(ClipboardChanged::Reloaded),
            Delivery::None => {}
        }
    }

    /// 一次应用之后：删过条目时重新加载缓存（删除让缓存变短，补满首屏）；新图片在后台生成缩略图；有新条目时排一次空闲
    /// 清理（条数上限）
    fn after_remote(&self, store: &ClipStore, totals: Totals) {
        if totals.removed {
            let serial = self.serial();
            reload_cache(&serial, store);
        }
        for id in totals.thumbs {
            self.spawn(move |e| e.make_thumb(id));
        }
        if totals.inserted {
            self.idle.mark();
        }
        if let Some(at) = totals.expiry {
            self.schedule_expiry(at);
        }
    }

    /// 应用远端的「清空」（本机那些同步层没有记录的条目，见 [`ClipRemoteClear`]）。**会阻塞**
    pub(crate) fn apply_remote_clear(&self, clear: &ClipRemoteClear) -> RemoteOutcome {
        let Some(store) = self.ready_store() else {
            return RemoteOutcome::Deferred;
        };
        let serial = self.serial();
        let ids = match store.delete_unsynced_before(clear.wall_ms, clear.keep_pinned, &clear.exclude) {
            Ok(ids) => ids,
            Err(e) => {
                log::warn!("clipboard remote clear deferred: {e}");
                return RemoteOutcome::Deferred;
            }
        };
        if !ids.is_empty() {
            log::info!("clipboard remote clear removed={}", ids.len());
            // 只推缓存前缀里被删的（最多一页，视图首屏显示的就是这些）：一次清空可能删掉上千条，全部逐条推送太重；
            // 更靠后、已经翻页加载出来的行，用户点到时前端按「已不存在」移除
            let shown: Vec<ClipboardChanged> = {
                let cache = serial.cache_read();
                ids.into_iter()
                    .filter(|id| cache.get(*id).is_some())
                    .map(|id| ClipboardChanged::Removed { id })
                    .collect()
            };
            // 可能删掉了缓存里的一大段：直接按库重新加载（补满首屏）
            reload_cache(&serial, &store);
            self.deliver(shown);
        }
        RemoteOutcome::Applied
    }

    // ---------- 识别历史 ----------

    /// 应用一批远端识别记录的改动，结果与输入一一对应。**会阻塞**。本机没有开启识别历史（`ocr.history`，只属于本机）
    /// 时不接收新记录，删除照常；插入之后按 `ocr.history_limit` 裁剪（裁剪只属于本机，不通知同步）
    pub(crate) fn apply_remote_ocr(
        &self,
        changes: Vec<OcrRemoteChange>,
        superseded: Option<&OcrSuperseded>,
    ) -> Vec<RemoteOutcome> {
        let mut outcomes = vec![RemoteOutcome::Deferred; changes.len()];
        let Some(store) = self.ready_store() else {
            return outcomes;
        };
        let settings = self.host.settings();
        let accept = settings.ocr.history;
        let limit = settings.ocr.history_limit;

        let mut prepared: Vec<(usize, PreparedOcr)> = Vec::new();
        let mut deletes: Vec<(usize, String)> = Vec::new();
        for (i, change) in changes.into_iter().enumerate() {
            match change {
                OcrRemoteChange::Upsert(_) if !accept => outcomes[i] = RemoteOutcome::Evicted,
                OcrRemoteChange::Upsert(item) => match store.ocr_prepare_remote(item) {
                    Ok(p) => prepared.push((i, p)),
                    Err(Rejected::Policy(reason)) => {
                        log::debug!("ocr remote record not accepted reason={reason:?}");
                        outcomes[i] = RemoteOutcome::Evicted;
                    }
                    Err(Rejected::Failed(e)) => log::warn!("ocr remote record deferred: {e}"),
                },
                OcrRemoteChange::Delete { key } => deletes.push((i, key)),
            }
        }

        let _ocr = self.ocr_serial.lock();
        let mut inserted = false;
        let mut prepared = prepared.into_iter().peekable();
        while prepared.peek().is_some() {
            let mut at = Vec::new();
            let mut batch = Vec::new();
            for (i, p) in prepared.by_ref().take(MAX_REMOTE_BATCH) {
                if superseded.is_some_and(|f| f(p.key())) {
                    outcomes[i] = RemoteOutcome::Superseded;
                } else {
                    at.push(i);
                    batch.push(p);
                }
            }
            if batch.is_empty() {
                continue;
            }
            match store.ocr_apply_remote(batch, limit) {
                Ok(results) => {
                    for (i, r) in at.into_iter().zip(results) {
                        outcomes[i] = match r {
                            OcrApplied::Inserted(item) => {
                                inserted = true;
                                self.host.emit_ocr_history(&OcrHistoryChanged::Inserted { item });
                                RemoteOutcome::Applied
                            }
                            OcrApplied::Existing(_) => RemoteOutcome::Applied,
                            OcrApplied::Rejected(reason) => {
                                log::debug!("ocr remote record not accepted reason={reason:?}");
                                RemoteOutcome::Evicted
                            }
                        };
                    }
                }
                Err(e) => log::warn!("ocr remote records deferred: {e}"),
            }
        }
        let (at, keys): (Vec<usize>, Vec<String>) = deletes
            .into_iter()
            .filter(|(i, key)| {
                let skip = superseded.is_some_and(|f| f(key));
                if skip {
                    outcomes[*i] = RemoteOutcome::Superseded;
                }
                !skip
            })
            .unzip();
        if !keys.is_empty() {
            match store.ocr_delete_by_keys(&keys) {
                Ok(ids) => {
                    for i in at {
                        outcomes[i] = RemoteOutcome::Applied;
                    }
                    if !ids.is_empty() {
                        self.host.emit_ocr_history(&OcrHistoryChanged::Removed { ids });
                    }
                }
                Err(e) => log::warn!("ocr remote deletions deferred: {e}"),
            }
        }
        if inserted {
            // 与本机记录之后一样按上限裁剪（只属于本机，不通知同步）
            self.ocr_history_trim(&store, limit);
        }
        outcomes
    }
}

/// 排序键（置顶、最近使用）变了的条目：在缓存里挪到新位置；原来在缓存里、或现在落进缓存前缀的推 `touched`
/// （前端按新的排序键移动它，原来不在列表里的放进去）
fn reposition(serial: &Serial<'_>, meta: &ClipItemMeta, events: &mut Vec<ClipboardChanged>) {
    let mut cache = serial.cache();
    let was = cache.get(meta.id).is_some();
    cache.upsert(meta);
    if was || cache.get(meta.id).is_some() {
        events.push(ClipboardChanged::Touched { item: meta.clone() });
    }
}

/// 从库里重新加载缓存（调用方持串行锁）；失败时清空缓存（首屏带 next_offset，前端照常向数据库翻页）
fn reload_cache(serial: &Serial<'_>, store: &ClipStore) {
    match wisp_clipboard::RecentCache::load(store) {
        Ok(cache) => *serial.cache() = cache,
        Err(e) => {
            log::warn!("clipboard cache reload failed: {e}");
            *serial.cache() = wisp_clipboard::RecentCache::default();
        }
    }
}

/// 远端条目是不是会到期的验证码：带着过期时间，或正文按本机的检测器是验证码消息。收藏的不算（用户明确要保留，
/// 它已经没有过期时间，按普通收藏同步）
fn is_code_item(item: &RemoteItem) -> bool {
    item.pinned != Some(true)
        && (item.expires_at.is_some() || item.text.as_deref().is_some_and(wisp_clipboard::smart::otp::detect))
}

/// 条目的 hash 与类型（删除之前取，供同步通知）；查询失败只记日志
pub(crate) fn sync_ident(store: &ClipStore, id: i64) -> Option<([u8; 32], ClipKind)> {
    match store.ident_of(id) {
        Ok(ident) => ident,
        Err(e) => {
            log::warn!("clipboard sync identity lookup failed: {e}");
            None
        }
    }
}
