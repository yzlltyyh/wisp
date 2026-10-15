//! 同步引擎读写本机数据的接口（`wisp_sync::LocalStore` 的桌面端实现）。
//!
//! - 配置（settings / styles / glossary）：经投影的反投影写回，走 `SettingsService::apply_synced`（文件正在手改时推迟）；
//! - API Key：经 `secret_apply_synced` 写钥匙串，只在允许读写钥匙串的轮次里进行；Key 只发往设置它时的 origin；
//! - 剪贴板 / 识别历史：经剪贴板服务的远端应用接口；数据库还没打开时一律推迟（同步从不为自己打开数据库）。
//!
//! 全部方法都是阻塞的，引擎在阻塞线程上调用。应用路径不经过钩子与 sink（设置服务以 `Sync` 来源调用钩子，剪贴板的远端
//! 应用接口不调用 sink），所以写进来的远端值不会被当作本机改动再发回去。「本机刚改过」的对象（[`Captures`]）一律跳过，
//! 留到下一轮：那时本机的改动已经进了副本，引擎会按 LWW 正确取舍。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use wisp_clipboard::{ClipStore, OcrSyncItem, RemoteItem, RemoteOcr, SyncItem, SyncSource};
use wisp_sync::model::keys::{self, ClipKey, OcrKey};
use wisp_sync::projection::{self, ConfigDoc};
use wisp_sync::{
    ApplyOutcome, BlobRole, Domain, Exported, Journal, KeyApply, LocalBlob, LocalEntry, LocalError, LocalStore,
    RemoteChange, SpooledBlob,
};
use wisp_types::{ClipKind, OcrMode, OsKind};

use super::capture::{Captures, file_of};
use super::identity::provider_origin;
use crate::clipboard_svc::{
    ClipRemoteChange, ClipRemoteClear, ClipboardService, OcrRemoteChange, OcrSuperseded, RemoteOutcome, Superseded,
};
use crate::error::IpcErrorCode;
use crate::settings::SettingsService;
use crate::settings::service::{LoadState, SyncApply};

/// 取剪贴板服务（应用还没装配好、或在测试里时为 None）
pub type ClipboardSource = Arc<dyn Fn() -> Option<Arc<ClipboardService>> + Send + Sync>;

/// 桌面端的本机存储
pub struct DesktopStore {
    settings: Arc<SettingsService>,
    clipboard: ClipboardSource,
    journal: Journal,
    captures: Arc<Captures>,
    os: OsKind,
    /// 本轮是否允许读写钥匙串（用户活动触发的轮次，或本进程已经成功访问过钥匙串之后）
    keychain: AtomicBool,
    /// 识别记录的同步键 → 本地 id（见 [`DesktopStore::ocr_item`]）
    ocr_ids: Mutex<HashMap<String, i64>>,
}

impl DesktopStore {
    pub fn new(
        settings: Arc<SettingsService>,
        clipboard: ClipboardSource,
        journal: Journal,
        captures: Arc<Captures>,
    ) -> Self {
        Self {
            settings,
            clipboard,
            journal,
            captures,
            os: OsKind::current(),
            keychain: AtomicBool::new(false),
            ocr_ids: Mutex::new(HashMap::new()),
        }
    }

    pub fn set_keychain_allowed(&self, allowed: bool) {
        self.keychain.store(allowed, Ordering::Release);
    }

    fn keychain_allowed(&self) -> bool {
        self.keychain.load(Ordering::Acquire)
    }

    /// 剪贴板服务与已经打开的数据库
    fn clip(&self) -> Result<(Arc<ClipboardService>, Arc<ClipStore>), LocalError> {
        let svc = (self.clipboard)().ok_or(LocalError::Unavailable)?;
        let store = svc.sync_store().ok_or(LocalError::Unavailable)?;
        Ok((svc, store))
    }

    // ---------- 配置 ----------

    fn config_map(&self, doc: ConfigDoc) -> std::collections::BTreeMap<String, Value> {
        projection::project(doc, &self.settings.config_value(file_of(doc)), self.os)
    }

    fn apply_config(&self, doc: ConfigDoc, changes: Vec<RemoteChange>) -> Result<ApplyOutcome, LocalError> {
        let file = file_of(doc);
        let mut results = vec![KeyApply::Deferred; changes.len()];
        // 文件正在手改、或这份文档有本机刚做的改动：整批留到下一轮
        if self.captures.doc_pending(doc) || self.settings.config_load_state(file) == LoadState::InvalidJson {
            return Ok(ApplyOutcome { results });
        }
        let mut list = Vec::with_capacity(changes.len());
        for (i, change) in changes.into_iter().enumerate() {
            match change {
                RemoteChange::Set { key, value } => list.push((i, key, Some(value))),
                RemoteChange::Delete { key } => list.push((i, key, None)),
                // 不属于配置域的变更不会出现；出现了也与本机无关
                _ => results[i] = KeyApply::Applied,
            }
        }
        if doc == ConfigDoc::Settings {
            list = self.guard_moved_providers(list)?;
        }
        if list.is_empty() {
            return Ok(ApplyOutcome { results });
        }
        let pairs: Vec<(String, Option<Value>)> = list.iter().map(|(_, k, v)| (k.clone(), v.clone())).collect();
        let mut per_key = None;
        let captures = &self.captures;
        let os = self.os;
        let outcome = self.settings.apply_synced(file, &mut |current| {
            // 在该文件的写锁内再查一次：钩子也在这把锁内捕获改动，查过之后不会再插进本机改动
            if captures.doc_pending(doc) {
                return None;
            }
            let merged = projection::unproject_detailed(doc, current, &pairs, os);
            per_key = Some(merged.results);
            (merged.value != *current).then_some(merged.value)
        });
        match outcome {
            Ok(SyncApply::Deferred) => {}
            Ok(SyncApply::Applied | SyncApply::Unchanged) => {
                if let Some(per_key) = per_key {
                    for ((i, _, _), result) in list.iter().zip(per_key) {
                        results[*i] = result;
                    }
                }
            }
            Err(e) => log::warn!("sync: writing {} failed ({:?})", file.file_name(), e.code),
        }
        Ok(ApplyOutcome { results })
    }

    /// 远端把某个模型服务的地址改到了另一个 origin：本机为旧地址保存的 Key 不能跟着发过去。没有与新 origin 匹配的胜出
    /// Key 记录时，先删掉本机的 Key 再应用新地址；不允许读写钥匙串的轮次里这类元素留到下一轮
    fn guard_moved_providers(
        &self,
        list: Vec<(usize, String, Option<Value>)>,
    ) -> Result<Vec<(usize, String, Option<Value>)>, LocalError> {
        let current = self.settings.get();
        let mut keep = Vec::with_capacity(list.len());
        let mut remove_keys = Vec::new();
        for item in list {
            let moved = provider_id_of(&item.1).and_then(|id| {
                let local = current.providers.iter().find(|p| p.id == id)?;
                let new_origin = item.2.as_ref()?.get("base_url")?.as_str().and_then(provider_origin);
                let moved = new_origin.is_some() && provider_origin(&local.base_url) != new_origin;
                (moved && self.settings.keychain().has(&id)).then_some((id, new_origin))
            });
            if let Some((id, new_origin)) = moved {
                let follows = self
                    .journal
                    .key_origin(&id)
                    .map_err(|_| LocalError::Failed("key origin lookup"))?;
                if follows != new_origin {
                    if !self.keychain_allowed() {
                        continue;
                    }
                    remove_keys.push(id);
                }
            }
            keep.push(item);
        }
        for id in remove_keys {
            let captures = &self.captures;
            // 与写入同步来的 Key 一样在 Key 的写锁内判定：用户刚保存的 Key 不能被这里删掉
            match self
                .settings
                .secret_apply_synced(&id, None, &|| !captures.key_pending(&id))
            {
                Ok(true) => log::info!("sync: a provider moved to another host; its local key was removed"),
                Err(e) if e.code == IpcErrorCode::Keychain => return Err(LocalError::Keychain),
                // 本机刚改过这个 Key，或删除失败：这个服务的改动留到下一轮
                Ok(false) | Err(_) => {
                    keep.retain(|(_, key, _)| provider_id_of(key).as_deref() != Some(id.as_str()));
                }
            }
        }
        Ok(keep)
    }

    fn enumerate_config(&self, doc: ConfigDoc) -> Vec<LocalEntry> {
        self.config_map(doc)
            .into_iter()
            .map(|(key, value)| LocalEntry {
                is_default: projection::is_default(doc, &key, &value, self.os),
                element: projection::is_element_key(doc, &key),
                key,
                value: Some(value),
                item_ms: None,
            })
            .collect()
    }

    // ---------- API Key ----------

    fn export_key(&self, provider_id: &str) -> Result<Option<Exported>, LocalError> {
        let settings = self.settings.get();
        let Some(provider) = settings.providers.iter().find(|p| p.id == provider_id) else {
            return Ok(None);
        };
        // 读不出来一律按钥匙串错误报告：引擎据此保留这条待上传记录，而不是当作「本机已没有」丢掉
        let secret = self
            .settings
            .keychain()
            .get(provider_id)
            .map_err(|_| LocalError::Keychain)?;
        Ok(secret.map(|secret| Exported {
            value: json!({ "key": secret.expose(), "origin": provider_origin(&provider.base_url) }),
            blobs: Vec::new(),
        }))
    }

    fn apply_keys(&self, changes: Vec<RemoteChange>) -> Result<ApplyOutcome, LocalError> {
        if !self.keychain_allowed() || self.captures.doc_pending(ConfigDoc::Settings) {
            return Ok(ApplyOutcome::uniform(changes.len(), KeyApply::Deferred));
        }
        let settings = self.settings.get();
        let mut results = Vec::with_capacity(changes.len());
        for change in changes {
            let result = match change {
                RemoteChange::Set { key: id, value } if !self.captures.key_pending(&id) => {
                    let secret = value.get("key").and_then(Value::as_str);
                    let origin = value.get("origin").and_then(Value::as_str);
                    match (settings.providers.iter().find(|p| p.id == id), secret) {
                        // 服务还没到（或本机刚删）：下一轮再看
                        (None, _) => KeyApply::Deferred,
                        (Some(_), None) => KeyApply::Evicted,
                        (Some(p), Some(secret)) => {
                            if provider_origin(&p.base_url).as_deref() == origin {
                                self.write_key(&id, Some(secret))?
                            } else {
                                KeyApply::Evicted
                            }
                        }
                    }
                }
                RemoteChange::Delete { key: id } if !self.captures.key_pending(&id) => self.write_key(&id, None)?,
                RemoteChange::Set { .. } | RemoteChange::Delete { .. } => KeyApply::Deferred,
                _ => KeyApply::Applied,
            };
            results.push(result);
        }
        Ok(ApplyOutcome { results })
    }

    fn write_key(&self, provider_id: &str, key: Option<&str>) -> Result<KeyApply, LocalError> {
        let captures = &self.captures;
        // 调用方锁外的判定只是提前跳过；用户保存 Key 时在 Key 的写锁内报告改动，所以要在同一把锁内再判定一次，
        // 否则用户正在写钥匙串的那一刻，更旧的远端值会排在后面把它盖掉
        match self
            .settings
            .secret_apply_synced(provider_id, key, &|| !captures.key_pending(provider_id))
        {
            Ok(true) => Ok(KeyApply::Applied),
            Ok(false) => Ok(KeyApply::Deferred),
            Err(e) if e.code == IpcErrorCode::Keychain => Err(LocalError::Keychain),
            Err(_) => Ok(KeyApply::Deferred),
        }
    }

    fn enumerate_keys(&self) -> Vec<LocalEntry> {
        let keychain = self.settings.keychain();
        self.settings
            .get()
            .providers
            .iter()
            .filter(|p| keychain.has(&p.id))
            .map(|p| LocalEntry {
                key: p.id.clone(),
                value: None,
                item_ms: None,
                is_default: false,
                element: true,
            })
            .collect()
    }

    // ---------- 剪贴板 ----------

    fn export_clip(&self, key: &str) -> Result<Option<Exported>, LocalError> {
        let Some(ClipKey::Content(hash)) = keys::parse_clip(key) else {
            return Ok(None);
        };
        let (_, store) = self.clip()?;
        let item = store
            .sync_export(&hash)
            .map_err(|_| LocalError::Failed("clipboard export"))?;
        Ok(item.map(|item| clip_exported(item, self.os)))
    }

    fn enumerate_clip(&self) -> Result<Vec<LocalEntry>, LocalError> {
        let (_, store) = self.clip()?;
        let rows = store.sync_rows().map_err(|_| LocalError::Failed("clipboard rows"))?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows.into_iter().filter(|r| r.kind != ClipKind::Files) {
            out.push(LocalEntry {
                key: keys::clip_content(&row.hash),
                value: None,
                item_ms: Some(row.last_used_at),
                is_default: false,
                element: true,
            });
            if row.pinned {
                out.push(LocalEntry {
                    key: keys::clip_pin(&row.hash),
                    value: Some(Value::Bool(true)),
                    item_ms: Some(row.last_used_at),
                    is_default: false,
                    element: true,
                });
            }
        }
        Ok(out)
    }

    fn apply_clip(&self, changes: Vec<RemoteChange>) -> Result<ApplyOutcome, LocalError> {
        let (svc, _) = self.clip()?;
        let captures = Arc::clone(&self.captures);
        let superseded: Superseded = Arc::new(move |hash: &[u8; 32]| captures.clip_pending(hash));
        let mut results = vec![KeyApply::Deferred; changes.len()];
        // 连续的条目改动合成一批交给剪贴板服务；遇到「清空」先把前面的写完，保持先后顺序
        let mut run: Vec<(usize, Vec<ClipRemoteChange>)> = Vec::new();
        let flush = |run: &mut Vec<(usize, Vec<ClipRemoteChange>)>, results: &mut Vec<KeyApply>| {
            if run.is_empty() {
                return;
            }
            let spans: Vec<(usize, usize)> = run.iter().map(|(i, c)| (*i, c.len())).collect();
            let flat: Vec<ClipRemoteChange> = run.drain(..).flat_map(|(_, c)| c).collect();
            let outcomes = svc.apply_remote_batch_blocking(flat, Some(&superseded));
            combine(&spans, &outcomes, results);
        };
        for (i, change) in changes.into_iter().enumerate() {
            match change {
                RemoteChange::ClipUpsert {
                    hash,
                    value,
                    pinned,
                    blobs,
                } => match remote_item(hash, &value, pinned, &blobs) {
                    Ok(item) => run.push((i, vec![ClipRemoteChange::Upsert(item)])),
                    Err(result) => results[i] = result,
                },
                RemoteChange::ClipPin { hash, pinned } => run.push((i, vec![ClipRemoteChange::Pin { hash, pinned }])),
                RemoteChange::ClipDelete { hashes } => run.push((
                    i,
                    hashes
                        .into_iter()
                        .map(|hash| ClipRemoteChange::Delete { hash })
                        .collect(),
                )),
                RemoteChange::ClipDeleteUnsyncedBefore {
                    wall_ms,
                    keep_pinned,
                    exclude,
                } => {
                    flush(&mut run, &mut results);
                    let clear = ClipRemoteClear {
                        wall_ms,
                        keep_pinned,
                        exclude,
                    };
                    results[i] = key_apply(svc.apply_remote_clear_blocking(&clear));
                }
                _ => results[i] = KeyApply::Applied,
            }
        }
        flush(&mut run, &mut results);
        Ok(ApplyOutcome { results })
    }

    // ---------- 识别历史 ----------

    fn export_ocr(&self, key: &str) -> Result<Option<Exported>, LocalError> {
        let Some(OcrKey::Item(id)) = keys::parse_ocr(key) else {
            return Ok(None);
        };
        let (_, store) = self.clip()?;
        let item = self.ocr_item(&store, id)?;
        Ok(item.map(|item| {
            let mut blobs = Vec::new();
            for (role, path, tag) in [
                (BlobRole::Thumbnail, item.thumb_path.as_ref(), "thumb"),
                (BlobRole::Image, item.image_path.as_ref(), "image"),
            ] {
                let Some(path) = path else { continue };
                // 文件丢了就不带这张图（缩略图之后可以由对端从原图生成）
                let Ok(meta) = std::fs::metadata(path) else { continue };
                blobs.push(LocalBlob {
                    role,
                    identity: format!("ocr:{tag}:{id}").into_bytes(),
                    path: path.clone(),
                    bytes: meta.len(),
                });
            }
            Exported {
                value: json!({
                    "created_at": item.created_at,
                    "mode": item.mode,
                    "model": item.model,
                    "text": item.text,
                }),
                blobs,
            }
        }))
    }

    /// 按同步键读取一条识别记录。键由内容算出、库里没有它的索引：扫一次建好「键 → id」的表，之后逐条按 id 读——批量
    /// 导出（加入同步、重新打开识别历史的同步）只扫一次，而不是每条都把整张表读出来重算一遍。id 可能被复用，读到的记录
    /// 要核对键；对不上、或表里没有时重扫一次
    fn ocr_item(&self, store: &ClipStore, key: &str) -> Result<Option<OcrSyncItem>, LocalError> {
        let by_id = |id: i64| {
            store
                .ocr_sync_export_by_id(id)
                .map(|item| item.filter(|it| it.key == key))
                .map_err(|_| LocalError::Failed("ocr export"))
        };
        let cached = self.ocr_ids.lock().get(key).copied();
        if let Some(id) = cached
            && let Some(item) = by_id(id)?
        {
            return Ok(Some(item));
        }
        let rows = store.ocr_sync_rows().map_err(|_| LocalError::Failed("ocr rows"))?;
        let ids: HashMap<String, i64> = rows.into_iter().map(|r| (r.key, r.id)).collect();
        let id = ids.get(key).copied();
        *self.ocr_ids.lock() = ids;
        id.map_or(Ok(None), by_id)
    }

    fn enumerate_ocr(&self) -> Result<Vec<LocalEntry>, LocalError> {
        let (_, store) = self.clip()?;
        let rows = store.ocr_sync_rows().map_err(|_| LocalError::Failed("ocr rows"))?;
        Ok(rows
            .into_iter()
            .map(|row| LocalEntry {
                key: keys::ocr_item(&row.key),
                value: None,
                item_ms: Some(row.created_at),
                is_default: false,
                element: true,
            })
            .collect())
    }

    fn apply_ocr(&self, changes: Vec<RemoteChange>) -> Result<ApplyOutcome, LocalError> {
        let (svc, store) = self.clip()?;
        let captures = Arc::clone(&self.captures);
        let superseded: OcrSuperseded = Arc::new(move |key: &str| captures.ocr_pending(key));
        let mut results = vec![KeyApply::Deferred; changes.len()];
        let mut spans = Vec::new();
        let mut flat = Vec::new();
        for (i, change) in changes.into_iter().enumerate() {
            let batch: Vec<OcrRemoteChange> = match change {
                RemoteChange::OcrUpsert { id, value, blobs } => match remote_ocr(id, &value, &blobs) {
                    Ok(item) => vec![OcrRemoteChange::Upsert(item)],
                    Err(result) => {
                        results[i] = result;
                        continue;
                    }
                },
                RemoteChange::OcrDelete { ids } => ids.into_iter().map(|key| OcrRemoteChange::Delete { key }).collect(),
                // 清空对没有同步记录的本机条目生效：按时间算出要删的键
                RemoteChange::OcrDeleteUnsyncedBefore { wall_ms, exclude } => {
                    let exclude: HashSet<String> = exclude.into_iter().collect();
                    let rows = store.ocr_sync_rows().map_err(|_| LocalError::Failed("ocr rows"))?;
                    rows.into_iter()
                        .filter(|r| r.created_at < wall_ms && !exclude.contains(&r.key))
                        .map(|r| OcrRemoteChange::Delete { key: r.key })
                        .collect()
                }
                _ => {
                    results[i] = KeyApply::Applied;
                    continue;
                }
            };
            spans.push((i, batch.len()));
            flat.extend(batch);
        }
        if !flat.is_empty() {
            let outcomes = svc.apply_remote_ocr_blocking(flat, Some(&superseded));
            combine(&spans, &outcomes, &mut results);
        } else {
            for (i, _) in spans {
                results[i] = KeyApply::Applied;
            }
        }
        Ok(ApplyOutcome { results })
    }
}

impl LocalStore for DesktopStore {
    fn export(&self, d: Domain, key: &str) -> Result<Option<Exported>, LocalError> {
        match d {
            Domain::Cfg | Domain::Sty | Domain::Glo => {
                let Some(doc) = ConfigDoc::from_domain(&d) else {
                    return Ok(None);
                };
                Ok(self.config_map(doc).remove(key).map(|value| Exported {
                    value,
                    blobs: Vec::new(),
                }))
            }
            Domain::Key => self.export_key(key),
            Domain::Clip => self.export_clip(key),
            Domain::Ocr => self.export_ocr(key),
            Domain::Unknown(_) => Ok(None),
        }
    }

    fn apply(&self, d: Domain, changes: Vec<RemoteChange>) -> Result<ApplyOutcome, LocalError> {
        match d {
            Domain::Cfg | Domain::Sty | Domain::Glo => match ConfigDoc::from_domain(&d) {
                Some(doc) => self.apply_config(doc, changes),
                None => Ok(ApplyOutcome::uniform(changes.len(), KeyApply::Deferred)),
            },
            Domain::Key => self.apply_keys(changes),
            Domain::Clip => self.apply_clip(changes),
            Domain::Ocr => self.apply_ocr(changes),
            Domain::Unknown(_) => Ok(ApplyOutcome::uniform(changes.len(), KeyApply::Deferred)),
        }
    }

    fn enumerate(&self, d: Domain) -> Result<Vec<LocalEntry>, LocalError> {
        match d {
            Domain::Cfg | Domain::Sty | Domain::Glo => {
                Ok(ConfigDoc::from_domain(&d).map_or_else(Vec::new, |doc| self.enumerate_config(doc)))
            }
            Domain::Key => Ok(self.enumerate_keys()),
            Domain::Clip => self.enumerate_clip(),
            Domain::Ocr => self.enumerate_ocr(),
            Domain::Unknown(_) => Ok(Vec::new()),
        }
    }

    fn available(&self, d: &Domain) -> bool {
        match d {
            // 只看剪贴板库是否已经打开（同步从不为自己打开它）：没打开时引擎不为这两个域下载附件、也不催促立即重跑
            Domain::Clip | Domain::Ocr => (self.clipboard)().is_some_and(|svc| svc.sync_store().is_some()),
            _ => true,
        }
    }
}

/// `/providers/<id>` → id（`@order` 等保留名不是）
fn provider_id_of(key: &str) -> Option<String> {
    let segment = key.strip_prefix("/providers/")?;
    if segment.starts_with('@') || segment.contains('/') {
        return None;
    }
    projection::unescape_segment(segment)
}

/// 剪贴板 / 识别历史的应用结果 → 引擎的结果。被本机更新的改动取代的按推迟处理：下一轮引擎会看到本机的改动并按 LWW 取舍
fn key_apply(outcome: RemoteOutcome) -> KeyApply {
    match outcome {
        RemoteOutcome::Applied => KeyApply::Applied,
        RemoteOutcome::Evicted => KeyApply::Evicted,
        RemoteOutcome::Deferred | RemoteOutcome::Superseded => KeyApply::Deferred,
    }
}

/// 一项变更展开成了若干条：全部写入才算写入；有一条要重试就整项重试（重复应用是幂等的）
fn combine(spans: &[(usize, usize)], outcomes: &[RemoteOutcome], results: &mut [KeyApply]) {
    let mut at = 0;
    for &(i, len) in spans {
        let part = outcomes.get(at..at + len).unwrap_or(&[]);
        at += len;
        results[i] = if part.len() < len {
            KeyApply::Deferred
        } else if part.is_empty() {
            KeyApply::Applied
        } else {
            let mapped: Vec<KeyApply> = part.iter().map(|o| key_apply(*o)).collect();
            if mapped.contains(&KeyApply::Deferred) {
                KeyApply::Deferred
            } else if mapped.iter().all(|r| *r == KeyApply::Evicted) {
                KeyApply::Evicted
            } else {
                KeyApply::Applied
            }
        };
    }
}

fn spooled(blobs: &[SpooledBlob], role: &BlobRole) -> Option<std::path::PathBuf> {
    blobs.iter().find(|b| &b.role == role).map(|b| b.path.clone())
}

fn str_of(value: &Value, field: &str) -> Option<String> {
    value.get(field).and_then(Value::as_str).map(str::to_owned)
}

/// 记录的值 → 剪贴板服务的远端条目。类型是本版本不认识的（更新的版本写的）推迟，升级后再试；缺字段的拒收
fn remote_item(
    hash: [u8; 32],
    value: &Value,
    pinned: Option<bool>,
    blobs: &[SpooledBlob],
) -> Result<RemoteItem, KeyApply> {
    let kind = value
        .get("kind")
        .cloned()
        .and_then(|k| serde_json::from_value::<ClipKind>(k).ok())
        .ok_or(KeyApply::Deferred)?;
    let created_at = value
        .get("created_at")
        .and_then(Value::as_i64)
        .ok_or(KeyApply::Evicted)?;
    let last_used_at = value
        .get("last_used_at")
        .and_then(Value::as_i64)
        .ok_or(KeyApply::Evicted)?;
    let source = value.get("src").and_then(|src| {
        Some(SyncSource {
            app_id: str_of(src, "app_id")?,
            app_name: str_of(src, "app_name").unwrap_or_default(),
        })
    });
    Ok(RemoteItem {
        hash,
        kind,
        created_at,
        last_used_at,
        pinned,
        text: str_of(value, "text"),
        html: str_of(value, "html"),
        rtf: value
            .get("rtf_b64")
            .and_then(Value::as_str)
            .and_then(|s| BASE64.decode(s).ok()),
        image_path: spooled(blobs, &BlobRole::Image),
        source,
        truncated: value.get("truncated").and_then(Value::as_bool).unwrap_or(false),
    })
}

fn remote_ocr(id: String, value: &Value, blobs: &[SpooledBlob]) -> Result<RemoteOcr, KeyApply> {
    let mode = value
        .get("mode")
        .cloned()
        .and_then(|m| serde_json::from_value::<OcrMode>(m).ok())
        .ok_or(KeyApply::Deferred)?;
    Ok(RemoteOcr {
        key: id,
        created_at: value
            .get("created_at")
            .and_then(Value::as_i64)
            .ok_or(KeyApply::Evicted)?,
        mode,
        model: str_of(value, "model").unwrap_or_default(),
        text: str_of(value, "text").ok_or(KeyApply::Evicted)?,
        thumb_path: spooled(blobs, &BlobRole::Thumbnail),
        image_path: spooled(blobs, &BlobRole::Image),
    })
}

/// 本机条目 → 记录的值（`{kind, created_at, last_used_at, text?, html?, rtf_b64?, image?, src?, truncated}`）与原图
fn clip_exported(item: SyncItem, os: OsKind) -> Exported {
    let mut value = Map::new();
    value.insert("kind".into(), json!(item.kind));
    value.insert("created_at".into(), json!(item.created_at));
    value.insert("last_used_at".into(), json!(item.last_used_at));
    if let Some(text) = item.text {
        value.insert("text".into(), Value::String(text));
    }
    if let Some(html) = item.html {
        value.insert("html".into(), Value::String(html));
    }
    if let Some(rtf) = item.rtf {
        value.insert("rtf_b64".into(), Value::String(BASE64.encode(rtf)));
    }
    let mut blobs = Vec::new();
    if let Some(image) = item.image {
        value.insert(
            "image".into(),
            json!({ "w": image.width, "h": image.height, "bytes": image.bytes }),
        );
        blobs.push(LocalBlob {
            role: BlobRole::Image,
            identity: item.hash.to_vec(),
            path: image.path,
            bytes: image.bytes,
        });
    }
    if let Some(src) = item.source {
        value.insert(
            "src".into(),
            json!({ "app_id": src.app_id, "app_name": src.app_name, "os": wisp_sync::projection::os_tag(os) }),
        );
    }
    value.insert("truncated".into(), Value::Bool(item.truncated));
    Exported {
        value: Value::Object(value),
        blobs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_keys_skip_reserved_names() {
        assert_eq!(provider_id_of("/providers/openai").as_deref(), Some("openai"));
        assert_eq!(provider_id_of("/providers/@order"), None);
        assert_eq!(provider_id_of("/translate/primary_lang"), None);
    }

    /// 识别记录按键导出：一次扫描建好「键 → id」的表；id 被复用时核对键，不会导出别的记录
    #[test]
    fn ocr_lookup_scans_once_and_checks_reused_ids() {
        let (_settings_dir, settings) = crate::settings::service::tests::test_service();
        let dir = tempfile::tempdir().expect("tempdir");
        let engine = wisp_sync::Engine::open(
            &dir.path().join("sync.db"),
            &dir.path().join("spool"),
            wisp_sync::DeviceInfo {
                id: "0123456789abcdef".into(),
                name: "test".into(),
                os: OsKind::current(),
                app_version: "0.0.0".into(),
            },
        )
        .expect("open replica");
        let store = DesktopStore::new(
            settings,
            Arc::new(|| None),
            engine.journal().expect("journal"),
            Arc::new(Captures::default()),
        );
        let clip = ClipStore::open(&dir.path().join("clip")).expect("open clipboard");
        let add = |t: i64, text: &str| {
            clip.ocr_history_add(&wisp_clipboard::OcrHistoryNew {
                created_at: t,
                mode: OcrMode::Lines,
                model: "m",
                text,
                thumb: None,
                image: None,
            })
            .expect("add")
        };
        let key_of = |id: i64| clip.ocr_key_of(id).expect("key").expect("exists");
        let text_of = |key: &str| store.ocr_item(&clip, key).expect("export").map(|i| i.text);
        let a = add(1, "alpha");
        let b = add(2, "beta");
        let (key_a, key_b) = (key_of(a), key_of(b));
        assert_eq!(text_of(&key_a).as_deref(), Some("alpha"));
        assert_eq!(store.ocr_ids.lock().len(), 2, "一次扫描建好整张表");
        assert_eq!(text_of(&key_b).as_deref(), Some("beta"));

        // 删掉最新的一条再新增：SQLite 复用它的 id，旧键不能读到新记录
        assert!(clip.ocr_history_delete(b).expect("delete"));
        let c = add(3, "gamma");
        assert_eq!(text_of(&key_b), None);
        assert_eq!(text_of(&key_of(c)).as_deref(), Some("gamma"));
    }

    #[test]
    fn expanded_changes_need_every_part() {
        use RemoteOutcome as O;
        let mut results = vec![KeyApply::Deferred; 4];
        combine(
            &[(0, 2), (1, 1), (2, 2), (3, 0)],
            &[O::Applied, O::Applied, O::Evicted, O::Applied, O::Superseded],
            &mut results,
        );
        assert_eq!(
            results,
            vec![
                KeyApply::Applied,
                KeyApply::Evicted,
                KeyApply::Deferred,
                KeyApply::Applied
            ]
        );
    }

    #[test]
    fn clip_values_round_trip() {
        let hash = [7u8; 32];
        let exported = clip_exported(
            SyncItem {
                hash,
                kind: ClipKind::RichText,
                created_at: 1,
                last_used_at: 2,
                pinned: false,
                text: Some("t".into()),
                html: Some("<b>t</b>".into()),
                rtf: Some(vec![1, 2, 3]),
                image: None,
                source: Some(SyncSource {
                    app_id: "app".into(),
                    app_name: "App".into(),
                }),
                truncated: true,
            },
            OsKind::Macos,
        );
        assert_eq!(exported.value["src"]["os"], "macos");
        let item = remote_item(hash, &exported.value, Some(true), &[]).expect("item");
        assert_eq!(item.kind, ClipKind::RichText);
        assert_eq!(item.rtf.as_deref(), Some(&[1u8, 2, 3][..]));
        assert_eq!(item.source.map(|s| s.app_name), Some("App".to_owned()));
        assert!(item.truncated);
        assert_eq!(item.pinned, Some(true));
        assert_eq!(
            remote_item(
                hash,
                &json!({"kind": "hologram", "created_at": 1, "last_used_at": 1}),
                None,
                &[]
            )
            .err(),
            Some(KeyApply::Deferred),
            "新版本的类型留到升级后"
        );
        assert_eq!(
            remote_item(hash, &json!({"kind": "text"}), None, &[]).err(),
            Some(KeyApply::Evicted)
        );
    }
}
