//! 来源应用图标缓存：`platform.apps.icon_png` → `ClipStore::put_icon` → `wisp://icon/<key>.png`。
//!
//! - 只在记录之后、条目没有图标时在后台取（不在关键路径上）；被隐私规则跳过的复制不取（不留痕迹）；
//! - 图标按应用落盘一次（`blobs/icons/<key>.png`，64 px），之后同一应用的新条目在 `ingest` 时直接带上；
//! - 平台取不到图标的应用在内存里记一笔，本进程内不再重取（前端显示通用图标）；
//! - Wisp 自己（`copy_text` 写入的内容）用内置的应用图标，不依赖是否以 .app / 安装包形式运行；
//! - 同一应用的图标同一时刻只取一次（并发的几次记录只有第一个去取）。
//!
//! 取到图标后：`put_icon` → 图标资源路径 → 持串行锁把缓存里同一应用、还没有图标的条目原地
//! 补上 → 在串行区内推送 `updated`（`engine.rs`「事件的顺序」）。资源路径直接由 `put_icon` / `icon_key` 返回的 key
//! 按存储层文档的约定（`icon/<icon_key>.png`）得出——不用 `get_detail` 取路径，因为它对文件条目会实时 stat
//! 最多 200 个路径（网络卷、休眠的外置盘上可能阻塞数秒），只为读一个与 key 一一对应的字符串不值得；单测
//! `icon_urls_come_from_the_store_and_follow_the_displayed_source` 核对两者一致。
//!
//! 锁：`put_icon` 内部有 PNG 解码、缩放、编码（几毫秒），**不持串行锁**——它对库的改动（登记图标 + 回填该应用没有图标
//! 的行）在存储层自己的锁里一次完成，与 `ingest` 读图标登记、插入新行互斥：新行要么插在登记之前（随后被回填），要么
//! 插在之后（插入时就带上图标）。缓存的回填只改内容字段、不改顺序，持串行锁只是为了与 `ingest`「改库 + 放进缓存」整体
//! 互斥（否则可能漏掉刚插入、还没放进缓存的那一行）；扫描用读锁，写锁只在逐条补图标时短暂持有。

use std::collections::HashSet;

use parking_lot::Mutex;
use wisp_clipboard::{BlobKind, ClipStore};
use wisp_types::{AppRef, ClipFilter, ClipItemMeta, ClipboardChanged};

use super::engine::Engine;

/// 向平台要的图标边长（32 pt @2x；`put_icon` 再统一缩到 64 px）
pub const ICON_PX: u32 = 64;
/// Wisp 自己的图标（与安装包同源）
pub(crate) const SELF_ICON_PNG: &[u8] = include_bytes!("../../icons/64x64.png");
/// 「取不到图标」名单的上限（应用数通常只有几十个；超出时不再记，最坏只是多取几次）
const MAX_MISSING: usize = 512;

/// 图标的获取状态
#[derive(Default)]
pub(crate) struct IconState {
    /// 平台取不到图标的应用 id
    missing: Mutex<HashSet<String>>,
    /// 正在取的应用 id
    inflight: Mutex<HashSet<String>>,
}

/// 图标资源路径（`ClipStore::put_icon` 的文档约定：`icon/<icon_key>.png`；与存储层元数据里的 `icon_url` 同一规则）
pub(crate) fn icon_asset_path(key: &str) -> String {
    format!("{}/{key}.png", BlobKind::Icon.asset_prefix())
}

impl Engine {
    /// 后台取某个应用的图标并更新缓存里该应用的条目。**会阻塞**（平台读图标、写 blob）
    pub(crate) fn fetch_icon(&self, app: &AppRef) {
        let app_id = app.id.trim();
        if app_id.is_empty() || self.icons.missing.lock().contains(app_id) {
            return;
        }
        if !self.icons.inflight.lock().insert(app_id.to_owned()) {
            return;
        }
        self.fetch_icon_inner(app_id, app);
        self.icons.inflight.lock().remove(app_id);
    }

    fn fetch_icon_inner(&self, app_id: &str, app: &AppRef) {
        let Some(store) = self.ready_store() else { return };
        // 并发的另一次记录可能已经取到并落盘了
        let key = match store.icon_key(app_id) {
            Some(key) => key,
            None => {
                let png = if app_id == self.host.self_app().id {
                    Some(SELF_ICON_PNG.to_vec())
                } else {
                    self.port.apps().icon_png(app, ICON_PX)
                };
                let Some(png) = png else {
                    let mut missing = self.icons.missing.lock();
                    if missing.len() < MAX_MISSING {
                        missing.insert(app_id.to_owned());
                    }
                    log::debug!("no icon for clipboard source app");
                    return;
                };
                match store.put_icon(app_id, &png) {
                    Ok(key) => key,
                    Err(e) => {
                        log::warn!("store clipboard source icon failed: {e}");
                        return;
                    }
                }
            }
        };
        // `put_icon` 已回填库里该应用没有图标的行；缓存与前端用同一个路径（不必再查库，见模块文档）
        self.apply_icon(app_id, &icon_asset_path(&key));
    }

    /// 排除名单选择器（`apps_running`）用：已落盘的图标直接给资源路径；没有时向平台取一次并落盘（顺带补上历史里
    /// 该应用还没有图标的条目），取不到的记进「取不到」名单、本进程内不再重取。**会阻塞**（平台读图标、写 blob）
    pub(crate) fn ensure_icon(&self, store: &ClipStore, app: &AppRef) -> Option<String> {
        let app_id = app.id.trim();
        if app_id.is_empty() {
            return None;
        }
        if let Some(key) = store.icon_key(app_id) {
            return Some(icon_asset_path(&key));
        }
        if self.icons.missing.lock().contains(app_id) {
            return None;
        }
        let Some(png) = self.port.apps().icon_png(app, ICON_PX) else {
            let mut missing = self.icons.missing.lock();
            if missing.len() < MAX_MISSING {
                missing.insert(app_id.to_owned());
            }
            return None;
        };
        match store.put_icon(app_id, &png) {
            Ok(key) => {
                let url = icon_asset_path(&key);
                self.apply_icon(app_id, &url);
                Some(url)
            }
            Err(e) => {
                log::warn!("store running app icon failed: {e}");
                None
            }
        }
    }

    /// 缓存里该应用、还没有图标的条目原地补上图标，并在串行区内推送 `updated`（只推送仍在缓存里的条目：它们一定还在库里）
    fn apply_icon(&self, app_id: &str, url: &str) {
        let serial = self.serial();
        // 缓存没有迭代接口，只能取一整页；只收集 id，读锁下进行，不挡首屏
        let ids: Vec<i64> = serial
            .cache_read()
            .page(ClipFilter::All, u32::MAX)
            .items
            .iter()
            .filter(|h| {
                h.item
                    .source
                    .as_ref()
                    .is_some_and(|s| s.id == app_id && s.icon_url.is_none())
            })
            .map(|h| h.item.id)
            .collect();
        if ids.is_empty() {
            return;
        }
        let updated: Vec<ClipItemMeta> = {
            let mut cache = serial.cache();
            ids.into_iter()
                .filter_map(|id| {
                    let mut m = cache.get(id)?.clone();
                    let source = m.source.as_mut()?;
                    source.icon_url = Some(url.to_owned());
                    cache.update(&m);
                    Some(m)
                })
                .collect()
        };
        // 缓存写锁已放开（首屏可读），串行锁仍持有：与删除互斥
        for item in updated {
            self.emit(ClipboardChanged::Updated { item });
        }
    }
}
