//! 多设备同步模拟的公共工具：内存里的本机存储（[`FakeStore`]）、每台设备一个的远端包装（[`SimRemote`]，
//! 可离线、可注入失败、记录自己发出的请求）、设备（[`Device`]）与服务器检查工具。
//!
//! 所有设备共用一个 [`MemoryRemote`]（同一台「服务器」）。本机改动像桌面端的钩子一样，在存储锁内分配 t 并写入日志；
//! 应用远端变更不写日志（按代码路径防回声）。
#![allow(dead_code)] // 各测试文件只用到其中一部分

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use parking_lot::Mutex;
use serde_json::{Value, json};
use wisp_sync::crypto::{self, FileKind, Keys};
use wisp_sync::model::{HeadPayload, PinValue, SegmentPayload, keys};
use wisp_sync::remote::memory::{Fault, Op};
use wisp_sync::{
    ApplyOutcome, BlobRole, Clock, ConnectOutcome, CycleOptions, CycleReason, CycleReport, DeviceInfo, Domain, Engine,
    Entry, Exported, Journal, JournalEntry, JournalValue, KeyApply, LocalBlob, LocalEntry, LocalError, LocalStore,
    MasterKey, MemoryRemote, PutInfo, Record, Remote, RemoteChange, RemoteError, SyncError, layout,
};
use wisp_types::OsKind;

/// 测试用的同步口令
pub const PASS: &str = "correct horse battery staple";

/// 配置的内置默认值（投影 key → 值）
pub fn cfg_defaults() -> BTreeMap<String, Value> {
    [
        ("/general/theme", json!("system")),
        ("/translate/primary_lang", json!("zh-CN")),
        ("/translate/secondary_lang", json!("en")),
        ("/clipboard/max_items", json!(1000)),
        ("/providers/@order", json!([])),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

fn is_element(d: &Domain, k: &str) -> bool {
    match d {
        Domain::Cfg => k.starts_with("/providers/") && k != "/providers/@order",
        Domain::Sty => k.starts_with("/custom/"),
        Domain::Glo | Domain::Key => true,
        _ => false,
    }
}

pub fn now_ms() -> i64 {
    i64::try_from(wisp_sync::hlc::wall_now_ms()).expect("时间")
}

/// 文本条目的 hash（与剪贴板库的做法同形：带域前缀的 blake3）
pub fn text_hash(text: &str) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"wisp:text\0");
    h.update(text.as_bytes());
    *h.finalize().as_bytes()
}

pub fn text_hash_hex(text: &str) -> String {
    wisp_sync::model::hex(&text_hash(text))
}

pub fn image_hash(bytes: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"wisp:image\0");
    h.update(bytes);
    *h.finalize().as_bytes()
}

pub fn ocr_id(created_at: i64, text: &str) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"wisp:ocr\0");
    h.update(&created_at.to_be_bytes());
    h.update(b"lines");
    h.update(text.as_bytes());
    wisp_sync::model::hex(&h.finalize().as_bytes()[..16])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipItem {
    pub text: String,
    pub created_at: i64,
    pub last_used_at: i64,
    pub pinned: bool,
    /// 收藏的名称（与桌面端相同：只有收藏的条目才有，取消收藏时去掉）
    pub name: Option<String>,
    pub image: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OcrItem {
    pub created_at: i64,
    pub text: String,
    pub thumb: Vec<u8>,
    pub image: Option<Vec<u8>>,
}

/// 一台设备的本机数据
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    pub cfg: BTreeMap<String, Value>,
    pub sty: BTreeMap<String, Value>,
    pub glo: BTreeMap<String, Value>,
    pub keys: BTreeMap<String, Value>,
    /// hash（十六进制）→ 条目
    pub clip: BTreeMap<String, ClipItem>,
    pub ocr: BTreeMap<String, OcrItem>,
}

impl State {
    pub fn with_defaults() -> State {
        State {
            cfg: cfg_defaults(),
            ..State::default()
        }
    }

    fn map(&mut self, d: &Domain) -> &mut BTreeMap<String, Value> {
        match d {
            Domain::Cfg => &mut self.cfg,
            Domain::Sty => &mut self.sty,
            Domain::Glo => &mut self.glo,
            Domain::Key => &mut self.keys,
            _ => panic!("不是寄存器域"),
        }
    }
}

/// 内存里的本机存储
pub struct FakeStore {
    pub state: Mutex<State>,
    dir: PathBuf,
    capture: Mutex<Option<(Journal, Arc<Clock>)>>,
    /// 钥匙串拒绝读写
    pub keychain_denied: AtomicBool,
    /// 剪贴板库尚未打开
    pub clip_unavailable: AtomicBool,
    /// 按本机规则（如保留策略）拒收本机还没有的未置顶条目
    pub reject_unpinned: AtomicBool,
    /// 应用路径收到的变更（检查用）
    pub received: Mutex<Vec<RemoteChange>>,
}

impl FakeStore {
    pub fn new(dir: &Path, state: State) -> FakeStore {
        std::fs::create_dir_all(dir).expect("存储目录");
        FakeStore {
            state: Mutex::new(state),
            dir: dir.to_path_buf(),
            capture: Mutex::new(None),
            keychain_denied: AtomicBool::new(false),
            clip_unavailable: AtomicBool::new(false),
            reject_unpinned: AtomicBool::new(false),
            received: Mutex::new(Vec::new()),
        }
    }

    /// 接上（新的）日志写入接口与时钟：钩子从此开始记录本机改动
    pub fn attach(&self, journal: Journal, clock: Arc<Clock>) {
        *self.capture.lock() = Some((journal, clock));
    }

    pub fn detach(&self) {
        *self.capture.lock() = None;
    }

    /// 像桌面端的钩子一样：在存储锁内分配 t 并写进日志（没有接上同步时什么都不记）
    fn capture(&self, d: Domain, k: &str, value: JournalValue) {
        let guard = self.capture.lock();
        if let Some((journal, clock)) = guard.as_ref() {
            let t = clock.now();
            journal
                .record(
                    vec![JournalEntry {
                        d,
                        k: k.to_string(),
                        t,
                        value,
                    }],
                    Vec::new(),
                )
                .expect("写日志");
        }
    }

    pub fn snapshot(&self) -> State {
        self.state.lock().clone()
    }

    // ---- 本机操作（用户行为）----

    pub fn set(&self, d: Domain, k: &str, v: Value) {
        let mut s = self.state.lock();
        s.map(&d).insert(k.to_string(), v.clone());
        let value = if d == Domain::Key {
            JournalValue::Deferred
        } else {
            JournalValue::Set(v)
        };
        self.capture(d, k, value);
    }

    pub fn remove(&self, d: Domain, k: &str) {
        let mut s = self.state.lock();
        s.map(&d).remove(k);
        self.capture(d, k, JournalValue::Delete);
    }

    pub fn set_key(&self, provider: &str, key: &str) {
        self.set(
            Domain::Key,
            provider,
            json!({"key": key, "origin": "https://api.example.com:443"}),
        );
    }

    /// 复制一段文本（新条目或再次复制）
    pub fn copy(&self, text: &str) -> String {
        let h = wisp_sync::model::hex(&text_hash(text));
        let now = now_ms();
        let mut s = self.state.lock();
        s.clip
            .entry(h.clone())
            .and_modify(|i| i.last_used_at = i.last_used_at.max(now))
            .or_insert(ClipItem {
                text: text.to_string(),
                created_at: now,
                last_used_at: now,
                pinned: false,
                name: None,
                image: None,
            });
        self.capture(Domain::Clip, &format!("c/{h}"), JournalValue::Deferred);
        h
    }

    /// 复制一张图片
    pub fn copy_image(&self, bytes: &[u8]) -> String {
        let h = wisp_sync::model::hex(&image_hash(bytes));
        std::fs::write(self.dir.join(format!("img-{h}")), bytes).expect("写图片");
        let now = now_ms();
        let mut s = self.state.lock();
        s.clip.entry(h.clone()).or_insert(ClipItem {
            text: String::new(),
            created_at: now,
            last_used_at: now,
            pinned: false,
            name: None,
            image: Some(bytes.to_vec()),
        });
        self.capture(Domain::Clip, &format!("c/{h}"), JournalValue::Deferred);
        h
    }

    /// 收藏 / 取消收藏（取消收藏时名称一并去掉）
    pub fn pin(&self, h: &str, pinned: bool) {
        let mut s = self.state.lock();
        if let Some(item) = s.clip.get_mut(h) {
            item.pinned = pinned;
            if !pinned {
                item.name = None;
            }
            let value = PinValue {
                pin: pinned,
                name: item.name.clone(),
            };
            self.capture(Domain::Clip, &format!("p/{h}"), JournalValue::Set(value.to_value()));
        }
    }

    /// 给条目命名（命名即收藏；None 只清除名称）
    pub fn name(&self, h: &str, name: Option<&str>) {
        let mut s = self.state.lock();
        if let Some(item) = s.clip.get_mut(h) {
            if name.is_some() {
                item.pinned = true;
            }
            item.name = name.map(str::to_owned);
            let value = PinValue {
                pin: item.pinned,
                name: item.name.clone(),
            };
            self.capture(Domain::Clip, &format!("p/{h}"), JournalValue::Set(value.to_value()));
        }
    }

    pub fn delete(&self, h: &str) {
        let mut s = self.state.lock();
        if s.clip.remove(h).is_some() {
            self.capture(Domain::Clip, &format!("c/{h}"), JournalValue::Delete);
        }
    }

    pub fn clear(&self, keep_pinned: bool) {
        let mut s = self.state.lock();
        s.clip.retain(|_, i| keep_pinned && i.pinned);
        let k = if keep_pinned {
            keys::CLIP_CLEAR_UNPINNED
        } else {
            keys::CLIP_CLEAR_ALL
        };
        self.capture(Domain::Clip, k, JournalValue::Set(json!({ "wall_ms": now_ms() })));
    }

    pub fn ocr_add(&self, text: &str) -> String {
        let created_at = now_ms();
        let id = ocr_id(created_at, text);
        let thumb = format!("thumb:{text}").into_bytes();
        std::fs::write(self.dir.join(format!("thumb-{id}")), &thumb).expect("写缩略图");
        let mut s = self.state.lock();
        s.ocr.insert(
            id.clone(),
            OcrItem {
                created_at,
                text: text.to_string(),
                thumb,
                image: None,
            },
        );
        self.capture(Domain::Ocr, &format!("o/{id}"), JournalValue::Deferred);
        id
    }

    pub fn ocr_delete(&self, id: &str) {
        let mut s = self.state.lock();
        if s.ocr.remove(id).is_some() {
            self.capture(Domain::Ocr, &format!("o/{id}"), JournalValue::Delete);
        }
    }

    pub fn ocr_clear(&self) {
        let mut s = self.state.lock();
        s.ocr.clear();
        self.capture(
            Domain::Ocr,
            keys::OCR_CLEAR,
            JournalValue::Set(json!({ "wall_ms": now_ms() })),
        );
    }

    fn read_spooled(path: &Path) -> Vec<u8> {
        std::fs::read(path).expect("读取 spool 文件")
    }

    fn apply_one(&self, s: &mut State, d: &Domain, change: RemoteChange) -> KeyApply {
        match change {
            RemoteChange::Set { key, value } => {
                s.map(d).insert(key, value);
            }
            RemoteChange::Delete { key } => {
                s.map(d).remove(&key);
            }
            RemoteChange::ClipUpsert {
                hash,
                value,
                pinned,
                name,
                blobs,
            } => {
                let h = wisp_sync::model::hex(&hash);
                let created = value["created_at"].as_i64().unwrap_or(0);
                let last = value["last_used_at"].as_i64().unwrap_or(0);
                let image = blobs
                    .iter()
                    .find(|b| b.role == BlobRole::Image)
                    .map(|b| Self::read_spooled(&b.path));
                if value["kind"] == "image" && image.is_none() && !s.clip.contains_key(&h) {
                    return KeyApply::Evicted;
                }
                if self.reject_unpinned.load(Ordering::SeqCst) && pinned != Some(true) && !s.clip.contains_key(&h) {
                    return KeyApply::Evicted;
                }
                if let Some(bytes) = &image {
                    std::fs::write(self.dir.join(format!("img-{h}")), bytes).expect("落图片");
                }
                match s.clip.get_mut(&h) {
                    Some(item) => {
                        item.created_at = item.created_at.min(created);
                        item.last_used_at = item.last_used_at.max(last);
                        if let Some(p) = pinned {
                            item.pinned = p;
                            item.name = if p { name } else { None };
                        }
                        if item.image.is_none() {
                            item.image = image;
                        }
                    }
                    None => {
                        s.clip.insert(
                            h,
                            ClipItem {
                                text: value["text"].as_str().unwrap_or_default().to_string(),
                                created_at: created,
                                last_used_at: last,
                                pinned: pinned.unwrap_or(false),
                                name: name.filter(|_| pinned == Some(true)),
                                image,
                            },
                        );
                    }
                }
            }
            RemoteChange::ClipPin { hash, pinned, name } => match s.clip.get_mut(&wisp_sync::model::hex(&hash)) {
                Some(item) => {
                    item.pinned = pinned;
                    item.name = if pinned { name } else { None };
                }
                // 与桌面端的约定：置顶一条本机没有的条目报 Evicted，引擎随后带着置顶状态重新导入它
                None if pinned => return KeyApply::Evicted,
                None => {}
            },
            RemoteChange::ClipDelete { hashes } => {
                for h in hashes {
                    s.clip.remove(&wisp_sync::model::hex(&h));
                }
            }
            RemoteChange::ClipDeleteUnsyncedBefore {
                wall_ms,
                keep_pinned,
                exclude,
            } => {
                let exclude: Vec<String> = exclude.iter().map(|h| wisp_sync::model::hex(h)).collect();
                s.clip
                    .retain(|h, i| exclude.contains(h) || (keep_pinned && i.pinned) || i.last_used_at >= wall_ms);
            }
            RemoteChange::OcrUpsert { id, value, blobs } => {
                let Some(thumb) = blobs.iter().find(|b| b.role == BlobRole::Thumbnail) else {
                    return KeyApply::Evicted;
                };
                let thumb = Self::read_spooled(&thumb.path);
                std::fs::write(self.dir.join(format!("thumb-{id}")), &thumb).expect("落缩略图");
                let image = blobs
                    .iter()
                    .find(|b| b.role == BlobRole::Image)
                    .map(|b| Self::read_spooled(&b.path));
                s.ocr.entry(id).or_insert(OcrItem {
                    created_at: value["created_at"].as_i64().unwrap_or(0),
                    text: value["text"].as_str().unwrap_or_default().to_string(),
                    thumb,
                    image,
                });
            }
            RemoteChange::OcrDelete { ids } => {
                for id in ids {
                    s.ocr.remove(&id);
                }
            }
            RemoteChange::OcrDeleteUnsyncedBefore { wall_ms, exclude } => {
                s.ocr.retain(|id, o| exclude.contains(id) || o.created_at >= wall_ms);
            }
        }
        KeyApply::Applied
    }
}

impl LocalStore for FakeStore {
    fn export(&self, d: Domain, key: &str) -> Result<Option<Exported>, LocalError> {
        let s = self.state.lock();
        let plain = |v: Option<&Value>| {
            v.map(|v| Exported {
                value: v.clone(),
                blobs: Vec::new(),
            })
        };
        Ok(match d {
            Domain::Cfg => plain(s.cfg.get(key)),
            Domain::Sty => plain(s.sty.get(key)),
            Domain::Glo => plain(s.glo.get(key)),
            Domain::Key => {
                if self.keychain_denied.load(Ordering::SeqCst) {
                    return Err(LocalError::Keychain);
                }
                plain(s.keys.get(key))
            }
            Domain::Clip => {
                if self.clip_unavailable.load(Ordering::SeqCst) {
                    return Err(LocalError::Unavailable);
                }
                let Some(h) = key.strip_prefix("c/") else {
                    return Ok(None);
                };
                s.clip.get(h).map(|i| {
                    let mut value = json!({
                        "kind": if i.image.is_some() { "image" } else { "text" },
                        "created_at": i.created_at,
                        "last_used_at": i.last_used_at,
                        "truncated": false,
                    });
                    let mut blobs = Vec::new();
                    if let Some(img) = &i.image {
                        value["image"] = json!({"w": 1, "h": 1, "bytes": img.len()});
                        blobs.push(LocalBlob {
                            role: BlobRole::Image,
                            identity: wisp_sync::model::parse_hex32(h).expect("hash").to_vec(),
                            path: self.dir.join(format!("img-{h}")),
                            bytes: img.len() as u64,
                        });
                    } else {
                        value["text"] = json!(i.text);
                    }
                    Exported { value, blobs }
                })
            }
            Domain::Ocr => {
                let Some(id) = key.strip_prefix("o/") else {
                    return Ok(None);
                };
                s.ocr.get(id).map(|o| Exported {
                    value: json!({"created_at": o.created_at, "mode": "lines", "model": "m", "text": o.text}),
                    blobs: vec![LocalBlob {
                        role: BlobRole::Thumbnail,
                        identity: format!("thumb:{id}").into_bytes(),
                        path: self.dir.join(format!("thumb-{id}")),
                        bytes: o.thumb.len() as u64,
                    }],
                })
            }
            Domain::Unknown(_) => None,
        })
    }

    fn apply(&self, d: Domain, changes: Vec<RemoteChange>) -> Result<ApplyOutcome, LocalError> {
        if d == Domain::Key && self.keychain_denied.load(Ordering::SeqCst) {
            return Err(LocalError::Keychain);
        }
        if d == Domain::Clip && self.clip_unavailable.load(Ordering::SeqCst) {
            return Err(LocalError::Unavailable);
        }
        self.received.lock().extend(changes.iter().cloned());
        let mut s = self.state.lock();
        let results = changes.into_iter().map(|c| self.apply_one(&mut s, &d, c)).collect();
        Ok(ApplyOutcome { results })
    }

    fn enumerate(&self, d: Domain) -> Result<Vec<LocalEntry>, LocalError> {
        let s = self.state.lock();
        let defaults = cfg_defaults();
        let registers = |map: &BTreeMap<String, Value>, deferred: bool| {
            map.iter()
                .map(|(k, v)| LocalEntry {
                    key: k.clone(),
                    value: (!deferred).then(|| v.clone()),
                    item_ms: None,
                    is_default: d == Domain::Cfg && defaults.get(k) == Some(v),
                    element: is_element(&d, k),
                })
                .collect::<Vec<_>>()
        };
        Ok(match d {
            Domain::Cfg => registers(&s.cfg, false),
            Domain::Sty => registers(&s.sty, false),
            Domain::Glo => registers(&s.glo, false),
            Domain::Key => registers(&s.keys, true),
            Domain::Clip => {
                if self.clip_unavailable.load(Ordering::SeqCst) {
                    return Err(LocalError::Unavailable);
                }
                let mut out = Vec::new();
                for (h, i) in &s.clip {
                    out.push(LocalEntry {
                        key: format!("c/{h}"),
                        value: None,
                        item_ms: Some(i.last_used_at),
                        is_default: false,
                        element: true,
                    });
                    if i.pinned {
                        let value = PinValue {
                            pin: true,
                            name: i.name.clone(),
                        };
                        out.push(LocalEntry {
                            key: format!("p/{h}"),
                            value: Some(value.to_value()),
                            item_ms: Some(i.last_used_at),
                            is_default: false,
                            element: true,
                        });
                    }
                }
                out
            }
            Domain::Ocr => s
                .ocr
                .iter()
                .map(|(id, o)| LocalEntry {
                    key: format!("o/{id}"),
                    value: None,
                    item_ms: Some(o.created_at),
                    is_default: false,
                    element: true,
                })
                .collect(),
            Domain::Unknown(_) => Vec::new(),
        })
    }

    fn available(&self, d: &Domain) -> bool {
        !(*d == Domain::Clip && self.clip_unavailable.load(Ordering::SeqCst))
    }
}

// ---------------------------------------------------------------------------------------------
// 每台设备的远端包装
// ---------------------------------------------------------------------------------------------

/// 失败注入：对本设备的每次操作调用，返回 `Some` 即注入
pub type Hook = Arc<dyn Fn(Op, &str) -> Option<Fault> + Send + Sync>;

#[derive(Default)]
pub struct RemoteCtl {
    pub offline: AtomicBool,
    pub log: Mutex<Vec<(Op, String)>>,
    pub hook: Mutex<Option<Hook>>,
}

/// 一台设备看到的「服务器」：共享同一个 [`MemoryRemote`]，但离线、失败注入与请求记录各自独立。
/// 也可以包装真实的远端（[`SimRemote::wrap`]），用同样的请求记录检查对真实服务器发出的请求
#[derive(Clone)]
pub struct SimRemote<R = MemoryRemote> {
    pub inner: R,
    pub ctl: Arc<RemoteCtl>,
}

impl SimRemote<MemoryRemote> {
    pub fn new(inner: &MemoryRemote) -> SimRemote {
        SimRemote::wrap(inner.clone())
    }
}

impl<R> SimRemote<R> {
    pub fn wrap(inner: R) -> SimRemote<R> {
        SimRemote {
            inner,
            ctl: Arc::new(RemoteCtl::default()),
        }
    }

    pub fn set_offline(&self, offline: bool) {
        self.ctl.offline.store(offline, Ordering::SeqCst);
    }

    pub fn set_hook(&self, hook: Option<Hook>) {
        *self.ctl.hook.lock() = hook;
    }

    pub fn ops(&self) -> Vec<(Op, String)> {
        self.ctl.log.lock().clone()
    }

    pub fn clear_ops(&self) {
        self.ctl.log.lock().clear();
    }

    /// 本设备发出的写操作（PUT / DELETE / MKCOL）
    pub fn writes(&self) -> Vec<(Op, String)> {
        self.ops()
            .into_iter()
            .filter(|(op, _)| matches!(op, Op::Put | Op::Delete | Op::Mkcol))
            .collect()
    }

    async fn run<T>(
        &self,
        op: Op,
        path: &str,
        fut: impl Future<Output = Result<T, RemoteError>>,
    ) -> Result<T, RemoteError> {
        if self.ctl.offline.load(Ordering::SeqCst) {
            return Err(RemoteError::Connect);
        }
        self.ctl.log.lock().push((op, path.to_string()));
        let hook = self.ctl.hook.lock().clone();
        match hook.and_then(|h| h(op, path)) {
            Some(Fault::Before(e)) => Err(e),
            Some(Fault::After(e)) => {
                fut.await?;
                Err(e)
            }
            None => fut.await,
        }
    }
}

impl<R: Remote> Remote for SimRemote<R> {
    async fn list(&self, dir: &str) -> Result<Option<Vec<Entry>>, RemoteError> {
        self.run(Op::List, dir, self.inner.list(dir)).await
    }

    async fn get(&self, path: &str) -> Result<Option<Bytes>, RemoteError> {
        self.run(Op::Get, path, self.inner.get(path)).await
    }

    async fn get_to_file(&self, path: &str, dest: &Path) -> Result<bool, RemoteError> {
        self.run(Op::Get, path, self.inner.get_to_file(path, dest)).await
    }

    async fn put(&self, path: &str, body: Bytes) -> Result<PutInfo, RemoteError> {
        self.run(Op::Put, path, self.inner.put(path, body)).await
    }

    async fn put_file(&self, path: &str, src: &Path, len: u64) -> Result<PutInfo, RemoteError> {
        self.run(Op::Put, path, self.inner.put_file(path, src, len)).await
    }

    async fn delete(&self, path: &str) -> Result<(), RemoteError> {
        self.run(Op::Delete, path, self.inner.delete(path)).await
    }

    async fn mkcol(&self, path: &str) -> Result<(), RemoteError> {
        self.run(Op::Mkcol, path, self.inner.mkcol(path)).await
    }
}

/// 只在第一次匹配时注入一次失败
pub fn fault_once(pred: impl Fn(Op, &str) -> bool + Send + Sync + 'static, fault: Fault) -> Hook {
    let fired = AtomicBool::new(false);
    Arc::new(move |op, path| {
        if pred(op, path) && !fired.swap(true, Ordering::SeqCst) {
            Some(fault.clone())
        } else {
            None
        }
    })
}

// ---------------------------------------------------------------------------------------------
// 设备
// ---------------------------------------------------------------------------------------------

pub fn noop_progress() -> &'static wisp_sync::ProgressFn {
    &|_| {}
}

pub fn options() -> CycleOptions {
    CycleOptions {
        keychain: true,
        ..CycleOptions::default()
    }
}

/// 一台模拟设备：自己的数据目录（sync.db、spool、本机存储）、引擎与远端视图
pub struct Device<R = MemoryRemote> {
    pub label: String,
    pub dir: tempfile::TempDir,
    pub engine: Option<Engine>,
    pub store: Arc<FakeStore>,
    pub remote: SimRemote<R>,
    pub options: CycleOptions,
    pub device_id: String,
    /// 连接成功后保存的 vault 与主密钥（相当于钥匙串）
    pub creds: Option<(String, MasterKey)>,
}

impl Device<MemoryRemote> {
    /// `n` 决定 device_id（同 t 时按 device_id 决胜，用数字让测试可预期）
    pub fn new(server: &MemoryRemote, label: &str, n: u64) -> Device {
        Self::with_state(server, label, n, State::with_defaults())
    }

    pub fn with_state(server: &MemoryRemote, label: &str, n: u64, state: State) -> Device {
        Device::with_remote(server.clone(), label, n, state)
    }
}

impl<R: Remote> Device<R> {
    /// 连到任意远端的设备（例如每台设备各自持有一个真实的 WebDAV 客户端）
    pub fn with_remote(remote: R, label: &str, n: u64, state: State) -> Device<R> {
        let dir = tempfile::tempdir().expect("临时目录");
        let store = Arc::new(FakeStore::new(&dir.path().join("store"), state));
        let mut dev = Device {
            label: label.to_string(),
            dir,
            engine: None,
            store,
            remote: SimRemote::wrap(remote),
            options: options(),
            device_id: format!("{n:016x}"),
            creds: None,
        };
        dev.open_engine();
        dev
    }

    pub fn db_path(&self) -> PathBuf {
        self.dir.path().join("sync.db")
    }

    pub fn open_engine(&mut self) {
        let engine = Engine::open(
            &self.db_path(),
            &self.dir.path().join("spool"),
            DeviceInfo {
                id: self.device_id.clone(),
                name: self.label.clone(),
                os: OsKind::Linux,
                app_version: "0.2.0".into(),
            },
        )
        .expect("打开引擎");
        self.device_id = engine.device().id;
        self.store.attach(engine.journal().expect("日志"), engine.hlc());
        self.engine = Some(engine);
    }

    pub fn engine(&self) -> &Engine {
        self.engine.as_ref().expect("引擎已打开")
    }

    pub async fn connect(&mut self) -> Result<ConnectOutcome, SyncError> {
        let connected = self
            .engine()
            .connect(&self.remote, &self.store, PASS, &[], &self.options, noop_progress())
            .await?;
        self.creds = Some((connected.vault_id.clone(), connected.master_key.clone()));
        Ok(connected.outcome)
    }

    /// 连接后在后台继续的首轮，以及之后的每一轮
    pub async fn sync(&mut self) -> Result<CycleReport, SyncError> {
        let report = self
            .engine()
            .sync_cycle(
                &self.remote,
                &self.store,
                CycleReason::Manual,
                &self.options,
                noop_progress(),
            )
            .await?;
        if let Some(id) = &report.new_device_id {
            self.device_id = id.clone();
        }
        Ok(report)
    }

    pub async fn sync_reason(&mut self, reason: CycleReason) -> Result<CycleReport, SyncError> {
        let report = self
            .engine()
            .sync_cycle(&self.remote, &self.store, reason, &self.options, noop_progress())
            .await?;
        if let Some(id) = &report.new_device_id {
            self.device_id = id.clone();
        }
        Ok(report)
    }

    /// 同步直到没有剩余工作（最多若干轮）
    pub async fn sync_fully(&mut self) -> Result<CycleReport, SyncError> {
        let mut last = self.sync().await?;
        for _ in 0..8 {
            if !last.again && !last.pending {
                break;
            }
            last = self.sync().await?;
        }
        Ok(last)
    }

    /// 模拟应用重启：重新打开引擎，用「钥匙串」里的主密钥解锁
    pub fn restart(&mut self) {
        self.store.detach();
        if let Some(engine) = self.engine.take() {
            engine.close().expect("关闭引擎");
        }
        self.open_engine();
        if let Some((vault, master)) = &self.creds {
            self.engine().unlock(vault, master).expect("解锁");
        }
    }

    /// 断开同步：删除 sync.db（保留 device_id 与本机数据）
    pub fn disconnect(&mut self) {
        self.store.detach();
        if let Some(engine) = self.engine.take() {
            engine.close().expect("关闭引擎");
        }
        for suffix in ["", "-wal", "-shm"] {
            let p = self.dir.path().join(format!("sync.db{suffix}"));
            if p.exists() {
                std::fs::remove_file(p).expect("删除 sync.db");
            }
        }
        self.creds = None;
        self.open_engine();
    }

    pub fn state(&self) -> State {
        self.store.snapshot()
    }
}

/// 同步到稳定：所有设备轮流同步若干遍
pub async fn settle<R: Remote>(devices: &mut [&mut Device<R>]) {
    for _ in 0..4 {
        for d in devices.iter_mut() {
            d.sync_fully()
                .await
                .unwrap_or_else(|e| panic!("{} 同步失败：{e}", d.label));
        }
    }
}

/// 所有设备的本机数据是否一致；不一致时给出第一处差异
pub fn check_converged<R: Remote>(devices: &[&Device<R>]) -> Result<(), String> {
    let states: Vec<(String, State)> = devices.iter().map(|d| (d.label.clone(), d.state())).collect();
    check_states_converged(&states)
}

/// 若干台设备（标签与本机数据）是否一致；不一致时给出第一处差异
pub fn check_states_converged(states: &[(String, State)]) -> Result<(), String> {
    let (first_label, first) = &states[0];
    // last_used_at 不比较：粘贴等使用只影响本机，按较大值合并，设计上就不要求各设备一致
    let clip = |s: &State| {
        s.clip
            .iter()
            .map(|(h, i)| (h.clone(), (i.text.clone(), i.pinned, i.name.clone(), i.image.clone())))
            .collect::<BTreeMap<_, _>>()
    };
    let ocr = |s: &State| s.ocr.keys().cloned().collect::<Vec<_>>();
    for (label, s) in &states[1..] {
        let pair = format!("{first_label} vs {label}");
        if first.cfg != s.cfg {
            return Err(format!("cfg 不一致（{pair}）：{:?} / {:?}", first.cfg, s.cfg));
        }
        if first.sty != s.sty {
            return Err(format!("sty 不一致（{pair}）：{:?} / {:?}", first.sty, s.sty));
        }
        if first.glo != s.glo {
            return Err(format!("glo 不一致（{pair}）：{:?} / {:?}", first.glo, s.glo));
        }
        if first.keys != s.keys {
            return Err(format!("keys 不一致（{pair}）：{:?} / {:?}", first.keys, s.keys));
        }
        if clip(first) != clip(s) {
            return Err(format!("剪贴板不一致（{pair}）：{:?} / {:?}", clip(first), clip(s)));
        }
        if ocr(first) != ocr(s) {
            return Err(format!("识别历史不一致（{pair}）：{:?} / {:?}", ocr(first), ocr(s)));
        }
    }
    Ok(())
}

/// 断言所有设备的本机数据一致
pub fn assert_converged<R: Remote>(devices: &[&Device<R>]) {
    if let Err(e) = check_converged(devices) {
        panic!("{e}");
    }
}

// ---------------------------------------------------------------------------------------------
// 服务器检查
// ---------------------------------------------------------------------------------------------

/// 用某台设备的凭据解开服务器上 `dev` 的 head 与它列出的全部段
pub fn read_log(server: &MemoryRemote, creds: &(String, MasterKey), dev: &str) -> Option<(HeadPayload, Vec<Record>)> {
    let (vault, master) = creds;
    let keys = Keys::derive(master, vault).expect("派生子密钥");
    let sealed = server.raw_get(&layout::head_path(vault, dev))?;
    let opened =
        crypto::open_small(&keys, FileKind::Head, vault, &layout::head_aad_name(dev), &sealed).expect("解开 head");
    let head = HeadPayload::decode(opened.flags, &opened.plaintext).expect("解析 head");
    let mut records = Vec::new();
    for seg in &head.segments {
        let Some(bytes) = server.raw_get(&layout::segment_path(vault, dev, seg.w)) else {
            continue;
        };
        let opened = crypto::open_small(
            &keys,
            FileKind::Segment,
            vault,
            &layout::segment_aad_name(dev, seg.w),
            &bytes,
        )
        .expect("解开段");
        records.extend(
            SegmentPayload::decode(opened.flags, &opened.plaintext)
                .expect("解析段")
                .records,
        );
    }
    Some((head, records))
}

/// 服务器上属于 vault 的全部 head 的 device_id
pub fn head_devices(server: &MemoryRemote, vault: &str) -> Vec<String> {
    server
        .paths()
        .iter()
        .filter_map(|p| layout::parse_head_name(vault, p).map(str::to_string))
        .collect()
}

/// 服务器上某台设备上传的 blob 路径
pub fn blob_paths(server: &MemoryRemote, vault: &str, dev: &str) -> Vec<String> {
    let prefix = format!("{}/", layout::blob_dir(vault, dev));
    server.paths().into_iter().filter(|p| p.starts_with(&prefix)).collect()
}

/// 服务器上某台设备日志目录里的段文件
pub fn segment_paths(server: &MemoryRemote, vault: &str, dev: &str) -> Vec<String> {
    let prefix = format!("{}/", layout::log_dir(vault, dev));
    server.paths().into_iter().filter(|p| p.starts_with(&prefix)).collect()
}

/// 简单的确定性伪随机数（xorshift64*），让随机交错可复现
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    pub fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[usize::try_from(self.below(items.len() as u64)).expect("下标")]
    }
}

/// 让物理时钟至少走过 1 ms（需要不同墙钟时间的场景用）
pub fn tick() {
    std::thread::sleep(std::time::Duration::from_millis(3));
}
