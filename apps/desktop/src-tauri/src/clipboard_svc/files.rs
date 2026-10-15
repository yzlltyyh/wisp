//! 文件条目：列表里的 exists 在页面交给前端之前检查；粘贴、在文件管理器中显示时只用仍然
//! 存在的文件。
//!
//! 列表与检索（`ClipStore::recent` / `search`、`RecentCache::page`）都不访问文件系统，文件条目的 exists 先按 true
//! 给出。每当一页结果要交给前端（首页、翻页、检索），就在返回之前对其中的文件条目（每页最多 [`MAX_BATCH`] 条、每条
//! 最多前 5 个路径，元数据里只带这些）做**有时限**的检查：
//! - 检查在阻塞线程池里进行，调用方最多等 [`SYNC_BUDGET`]。本地磁盘上几十条只要几十微秒，结果直接写进这一页
//!   （并原地更新缓存），前端拿到的就是置灰后的列表，不需要事件；
//! - 超时（不可达的网络卷、休眠的外置盘上 stat 可能阻塞数秒）的页面留在后台继续，完成后原地更新缓存，对这一页里
//!   结果与交出时不同的条目推送 `clipboard:changed { updated }`。推送走 [`Engine::emit`] 的规则：视图 ready 之后才发，
//!   准备中的先排队，所以不会先于页面到达而被前端忽略；并在串行区内确认条目仍在（`engine.rs`「事件的顺序」）。
//!
//! **只 `stat`**（对其他 App 数据容器里的路径做 stat 不会触发 TCC 对话框）：不读文件内容、不列目录。
//! 同一时刻只有一个检查任务（卡在网络卷上也只占一个线程）。它卡住期间新到的页面**合并**进下一批（按 id 去重，每个
//! 页面的等待方都保留）：列表是逐页累加的，先交出的页面仍在屏幕上，每一页最终都会拿到结果——在时限内的随页面交出，
//! 超时的经 `updated` 补上。合并的条目数有上限（[`MAX_PENDING`]，约 8 页），超出时丢弃最早的页面（只在检查卡住、
//! 用户又连续翻过很多页文件时才会发生）。

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use wisp_clipboard::RecentCache;
use wisp_types::{ClipHit, ClipItemMeta, ClipPage, ClipboardChanged};

use super::engine::Engine;

/// 一页最多检查的文件条目数（一页 200 条里全是文件条目时也只看前这么多：约等于两三屏）
pub(crate) const MAX_BATCH: usize = 64;
/// 页面交给前端之前等检查结果的上限
pub(crate) const SYNC_BUDGET: Duration = Duration::from_millis(5);
/// 检查卡住期间合并等待的条目数上限（约 8 页）
pub(crate) const MAX_PENDING: usize = 8 * MAX_BATCH;

/// 检查任务的排队状态
#[derive(Default)]
pub(crate) struct FilesRefresh {
    inner: Mutex<Queue>,
}

#[derive(Default)]
struct Queue {
    /// 尚未开始检查的页面（按交出的先后），下一次检查一并处理
    pending: Vec<PageCheck>,
    running: bool,
}

/// 一页的检查请求
struct PageCheck {
    /// 这一页里要检查的文件条目（交出时的快照）
    batch: Vec<ClipHit>,
    /// 等着把结果写进这一页的调用方
    waiter: Arc<Handoff>,
}

/// 检查结果（全部检查过的条目，files 已是检查后的值；多个页面共享同一份）
type Checked = Arc<Vec<ClipHit>>;

/// 页面与后台检查之间的交接：等待方在时限内拿到结果就写进页面；超时则离开，结果改由检查方推送
#[derive(Default)]
struct Handoff {
    state: Mutex<HandoffState>,
    ready: Condvar,
}

#[derive(Default)]
enum HandoffState {
    #[default]
    Waiting,
    /// 检查完成
    Delivered(Checked),
    /// 等待方已超时离开
    Abandoned,
}

impl Handoff {
    /// 等待方：最多等 `budget`。拿到结果返回 Some；超时返回 None，之后的结果由检查方推送
    fn wait(&self, budget: Duration) -> Option<Checked> {
        let deadline = Instant::now() + budget;
        let mut s = self.state.lock();
        loop {
            match std::mem::replace(&mut *s, HandoffState::Abandoned) {
                HandoffState::Delivered(checked) => return Some(checked),
                other => *s = other,
            }
            if self.ready.wait_until(&mut s, deadline).timed_out() {
                // 超时的同一时刻也可能刚好交付了；否则标记为已离开
                return match std::mem::replace(&mut *s, HandoffState::Abandoned) {
                    HandoffState::Delivered(checked) => Some(checked),
                    _ => None,
                };
            }
        }
    }

    /// 检查方：等待方还在等时交给它（true）；已经离开时返回 false，由检查方推送这一页的变化
    fn deliver(&self, checked: &Checked) -> bool {
        let mut s = self.state.lock();
        if matches!(*s, HandoffState::Waiting) {
            *s = HandoffState::Delivered(Arc::clone(checked));
            self.ready.notify_one();
            true
        } else {
            false
        }
    }
}

impl Engine {
    /// 页面交给前端之前：对其中的文件条目做有时限的 exists 检查，结果写进页面（缓存同步更新）；超时的部分在后台完成后
    /// 经 `updated` 推送。**会阻塞**最多 [`SYNC_BUDGET`]
    pub(crate) fn check_page_files(&self, page: &mut ClipPage) {
        let batch: Vec<ClipHit> = page
            .items
            .iter()
            .filter(|h| h.item.files.as_ref().is_some_and(|f| !f.is_empty()))
            .take(MAX_BATCH)
            .cloned()
            .collect();
        if batch.is_empty() {
            return;
        }
        let waiter = Arc::new(Handoff::default());
        self.enqueue_files(PageCheck {
            batch,
            waiter: Arc::clone(&waiter),
        });
        let Some(checked) = waiter.wait(SYNC_BUDGET) else {
            log::debug!("clipboard file check exceeded {SYNC_BUDGET:?}; results will follow as updated");
            return;
        };
        let by_id: HashMap<i64, &ClipHit> = checked.iter().map(|c| (c.item.id, c)).collect();
        for hit in &mut page.items {
            if let Some(c) = by_id.get(&hit.item.id) {
                hit.item.files.clone_from(&c.item.files);
            }
        }
    }

    fn enqueue_files(&self, check: PageCheck) {
        {
            let mut q = self.files.inner.lock();
            q.pending.push(check);
            // 上限：丢弃最早的页面（它的等待方早已超时离开）
            let mut total: usize = q.pending.iter().map(|c| c.batch.len()).sum();
            while total > MAX_PENDING && q.pending.len() > 1 {
                let dropped = q.pending.remove(0);
                total -= dropped.batch.len();
                log::debug!(
                    "clipboard file check backlog full; oldest page dropped items={}",
                    dropped.batch.len()
                );
            }
            if q.running {
                return;
            }
            q.running = true;
        }
        self.spawn(|e| e.run_files_refresh());
    }

    fn run_files_refresh(&self) {
        loop {
            let pages = {
                let mut q = self.files.inner.lock();
                if q.pending.is_empty() {
                    q.running = false;
                    return;
                }
                std::mem::take(&mut q.pending)
            };
            self.refresh_pages(pages);
        }
    }

    /// 检查一批页面（合并、去重后统一 stat），把结果交给仍在等的页面，更新缓存，推送超时页面里的变化
    fn refresh_pages(&self, pages: Vec<PageCheck>) {
        let mut batch: Vec<ClipHit> = Vec::new();
        let mut seen = HashSet::new();
        // 同一条目的路径列表不会变，任取一份快照即可；从最新的页面取起
        for page in pages.iter().rev() {
            for hit in &page.batch {
                if seen.insert(hit.item.id) {
                    batch.push(hit.clone());
                }
            }
        }
        wisp_clipboard::refresh_file_exists(&mut batch);
        let checked: Checked = Arc::new(batch);
        let by_id: HashMap<i64, &ClipHit> = checked.iter().map(|c| (c.item.id, c)).collect();

        let serial = self.serial();
        // 缓存：files 与检查结果不同的条目原地更新（只换 files；以缓存里的当前元数据为底，其他字段——例如刚补上的
        // 图标——不能被几秒前的快照覆盖）
        {
            let mut cache = serial.cache();
            for c in checked.iter() {
                if let Some(current) = cache.get(c.item.id)
                    && current.files != c.item.files
                {
                    let mut m = current.clone();
                    m.files.clone_from(&c.item.files);
                    cache.update(&m);
                }
            }
        }
        // 仍在等的页面随结果交出；已离开的页面里，结果与它交出时不同的条目稍后推送
        let mut late: Vec<i64> = Vec::new();
        let mut queued = HashSet::new();
        for page in &pages {
            if page.waiter.deliver(&checked) {
                continue;
            }
            for hit in &page.batch {
                let id = hit.item.id;
                if by_id.get(&id).is_some_and(|c| c.item.files != hit.item.files) && queued.insert(id) {
                    late.push(id);
                }
            }
        }
        if late.is_empty() {
            return;
        }
        log::debug!("clipboard file items refreshed late changed={}", late.len());
        let store = self.ready_store();
        for id in late {
            let cached = serial.cache_read().get(id).cloned();
            let item = match cached {
                Some(meta) => meta,
                // 不在缓存里（后面的页面、检索结果）：先确认库里还有这一行。文件条目的载荷只是路径列表，
                // `get_payload` 不访问文件系统
                None => {
                    let alive = store
                        .as_ref()
                        .is_some_and(|s| matches!(s.get_payload(id, false), Ok(Some(_))));
                    let Some(c) = by_id.get(&id).filter(|_| alive) else {
                        continue;
                    };
                    c.item.clone()
                }
            };
            // 在串行区内投递（`engine.rs`「事件的顺序」）
            self.emit(ClipboardChanged::Updated { item });
        }
    }
}

/// 数据库返回的元数据里文件的 exists 一律为 true（未检查）。置顶、粘贴的 touch 之后用它替换缓存时，沿用缓存里已经
/// 检查过的结果，避免已置灰的文件闪回正常
pub(crate) fn keep_checked_files(cache: &RecentCache, meta: &mut ClipItemMeta) {
    let (Some(files), Some(checked)) = (meta.files.as_mut(), cache.get(meta.id).and_then(|m| m.files.as_ref())) else {
        return;
    };
    for f in files.iter_mut() {
        if let Some(c) = checked.iter().find(|c| c.path == f.path) {
            f.exists = c.exists;
        }
    }
}

/// 保留仍然存在的文件（用户的明确操作里调用：粘贴、在文件管理器中显示；只 stat）
pub(crate) fn existing(paths: Vec<String>) -> Vec<String> {
    paths.into_iter().filter(|p| Path::new(p).exists()).collect()
}
