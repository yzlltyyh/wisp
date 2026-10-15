//! 本机改动的捕获：配置改动钩子、Key 钩子与剪贴板 sink。
//!
//! 它们都在调用方的锁里被调用（配置文件的写锁、Key 的写锁、剪贴板的串行区），所以只做 O(1) 的事：在捕获的那一刻
//! 分配时间戳（这样 t 的先后就是改动的先后）、记下「有一条还没写进 sync.db 的本机改动」、把一条小消息推进日志写入
//! 线程的通道。投影与比对、写库都在写入线程里做。
//!
//! 「还没写进 sync.db 的本机改动」（[`Captures`]）给应用远端改动的那一侧用：副本只认识已经写入的改动，应用远端值时
//! 必须跳过这些 key，否则用户刚做的修改会被一条更旧的远端值盖掉，而日志随后又把用户的值当作胜者发出去——两边就不一致了。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use parking_lot::Mutex;
use serde_json::{Value, json};
use wisp_sync::model::keys;
use wisp_sync::projection::ConfigDoc;
use wisp_sync::{Clock, Domain, Hlc, JournalEntry, JournalValue};

use crate::clipboard_svc::ClipSyncSink;
use crate::settings::service::{ChangeOrigin, ConfigChange, ConfigFile};

/// 配置文件 → 投影文档
pub fn doc_of(file: ConfigFile) -> ConfigDoc {
    match file {
        ConfigFile::Settings => ConfigDoc::Settings,
        ConfigFile::Styles => ConfigDoc::Styles,
        ConfigFile::Glossary => ConfigDoc::Glossary,
    }
}

/// 投影文档 → 配置文件
pub fn file_of(doc: ConfigDoc) -> ConfigFile {
    match doc {
        ConfigDoc::Settings => ConfigFile::Settings,
        ConfigDoc::Styles => ConfigFile::Styles,
        ConfigDoc::Glossary => ConfigFile::Glossary,
    }
}

/// 一条尚未写进 sync.db 的本机改动作用的对象
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CaptureKey {
    /// 某个配置文档（具体改了哪些 key 要等写入线程投影之后才知道，所以按整份文档记）
    Doc(ConfigDoc),
    /// 某个模型服务的 Key
    Key(String),
    Clip([u8; 32]),
    ClipClear,
    Ocr(String),
    OcrClear,
}

/// 「本机刚改过、远端值不能覆盖」的对象：还没写进 sync.db 的本机改动，以及当前这一轮同步开始之后才写进去的。
///
/// 后一种也要算：引擎在一轮里先读出要应用的胜出记录、稍后才交给本机，这中间写进来的本机改动引擎已经看不到了。所以一轮
/// 同步进行期间，写入完成的项只标记为「已写入」、保留到这一轮结束；没有同步在进行时写入完成就移除（集合不会无限增长）
#[derive(Debug, Default)]
pub struct Captures {
    inner: Mutex<CapturesInner>,
}

#[derive(Debug, Default)]
struct CapturesInner {
    /// 键 → (最新一次捕获的 t, 是否已写入 sync.db)
    map: HashMap<CaptureKey, (Hlc, bool)>,
    /// 一轮同步正在进行
    in_cycle: bool,
}

impl Captures {
    pub fn note(&self, key: CaptureKey, t: Hlc) {
        self.inner.lock().map.insert(key, (t, false));
    }

    /// 写入线程提交之后：只处理 t 与提交的一致的项（期间又被改动的保持未写入，等下一次提交）
    pub fn settle(&self, done: &[(CaptureKey, Hlc)]) {
        let mut inner = self.inner.lock();
        let keep = inner.in_cycle;
        for (key, t) in done {
            if inner.map.get(key).is_some_and(|(cur, _)| cur == t) {
                if keep {
                    inner.map.insert(key.clone(), (*t, true));
                } else {
                    inner.map.remove(key);
                }
            }
        }
    }

    /// 一轮同步开始（调用方已经让写入线程把队列写完）：之前写入完成的项引擎都能看到了，移除它们
    pub fn begin_cycle(&self) {
        let mut inner = self.inner.lock();
        inner.in_cycle = true;
        inner.map.retain(|_, (_, written)| !*written);
    }

    /// 一轮同步结束
    pub fn end_cycle(&self) {
        let mut inner = self.inner.lock();
        inner.in_cycle = false;
        inner.map.retain(|_, (_, written)| !*written);
    }

    fn contains(&self, key: &CaptureKey) -> bool {
        self.inner.lock().map.contains_key(key)
    }

    pub fn doc_pending(&self, doc: ConfigDoc) -> bool {
        self.contains(&CaptureKey::Doc(doc))
    }

    pub fn key_pending(&self, provider_id: &str) -> bool {
        self.contains(&CaptureKey::Key(provider_id.to_owned()))
    }

    /// 剪贴板条目有本机改动（或刚清空过历史）
    pub fn clip_pending(&self, hash: &[u8; 32]) -> bool {
        let inner = self.inner.lock();
        inner.map.contains_key(&CaptureKey::ClipClear) || inner.map.contains_key(&CaptureKey::Clip(*hash))
    }

    pub fn ocr_pending(&self, key: &str) -> bool {
        let inner = self.inner.lock();
        inner.map.contains_key(&CaptureKey::OcrClear) || inner.map.contains_key(&CaptureKey::Ocr(key.to_owned()))
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().map.is_empty()
    }
}

/// 日志写入线程的消息
pub enum Msg {
    /// 配置文件的一次替换（规范化文档）。`origin = Sync` 的只更新影子，不记为本机改动；`lossy` 的（手改文件里有写错的
    /// 内容被丢掉）只记新增与修改，不记删除
    Config {
        file: ConfigFile,
        before: Value,
        after: Value,
        origin: ChangeOrigin,
        lossy: bool,
        t: Hlc,
    },
    /// 一条已经确定的本机改动（Key、剪贴板、识别历史）
    Record { entry: JournalEntry, capture: CaptureKey },
    /// 把此前的消息全部写入后回复
    Flush(mpsc::SyncSender<()>),
    /// 写完此前的消息后退出
    Stop,
}

/// 钩子与 sink 的共同实现
pub struct Capturer {
    clock: Arc<Clock>,
    tx: mpsc::Sender<Msg>,
    captures: Arc<Captures>,
    /// 剪贴板历史 / 识别历史的同步开关：关闭期间不记录（重新打开时整域重新加入）
    clip_on: AtomicBool,
    ocr_on: AtomicBool,
}

impl Capturer {
    pub fn new(clock: Arc<Clock>, tx: mpsc::Sender<Msg>, captures: Arc<Captures>) -> Self {
        Self {
            clock,
            tx,
            captures,
            clip_on: AtomicBool::new(false),
            ocr_on: AtomicBool::new(false),
        }
    }

    pub fn set_content(&self, clipboard: bool, ocr: bool) {
        self.clip_on.store(clipboard, Ordering::Release);
        self.ocr_on.store(ocr, Ordering::Release);
    }

    fn send(&self, msg: Msg) {
        // 写入线程已经退出（同步正在关闭）：这条改动不再需要
        let _ = self.tx.send(msg);
    }

    fn record(&self, d: Domain, k: String, value: JournalValue, capture: CaptureKey) {
        let t = self.clock.now();
        self.captures.note(capture.clone(), t);
        self.send(Msg::Record {
            entry: JournalEntry { d, k, t, value },
            capture,
        });
    }

    /// 配置改动钩子（在该文件的写锁内调用）
    pub fn config(&self, change: &ConfigChange<'_>) {
        let t = if change.origin == ChangeOrigin::Sync {
            // 自己应用的远端值只更新影子，不分配时间戳、也不算本机改动
            Hlc::default()
        } else {
            let t = self.clock.now();
            self.captures.note(CaptureKey::Doc(doc_of(change.file)), t);
            t
        };
        self.send(Msg::Config {
            file: change.file,
            before: change.before.clone(),
            after: change.after.clone(),
            origin: change.origin,
            lossy: change.lossy,
            t,
        });
    }

    /// Key 钩子（在 Key 的写锁内调用）。明文不进通道：推送时再从钥匙串读取
    pub fn secret(&self, provider_id: &str, key: Option<&str>) {
        let value = if key.is_some() {
            JournalValue::Deferred
        } else {
            JournalValue::Delete
        };
        self.record(
            Domain::Key,
            provider_id.to_owned(),
            value,
            CaptureKey::Key(provider_id.to_owned()),
        );
    }

    fn clip_on(&self) -> bool {
        self.clip_on.load(Ordering::Acquire)
    }

    fn ocr_on(&self) -> bool {
        self.ocr_on.load(Ordering::Acquire)
    }
}

/// 清空的值：本机的墙钟毫秒（只用于对端那些没有同步寄存器的条目）
fn clear_value() -> JournalValue {
    let wall_ms = i64::try_from(wisp_sync::hlc::wall_now_ms()).unwrap_or(i64::MAX);
    JournalValue::Set(json!({ "wall_ms": wall_ms }))
}

impl ClipSyncSink for Capturer {
    fn content(&self, hash: [u8; 32]) {
        if self.clip_on() {
            self.record(
                Domain::Clip,
                keys::clip_content(&hash),
                JournalValue::Deferred,
                CaptureKey::Clip(hash),
            );
        }
    }

    fn pin(&self, hash: [u8; 32], pinned: bool) {
        if self.clip_on() {
            self.record(
                Domain::Clip,
                keys::clip_pin(&hash),
                JournalValue::Set(Value::Bool(pinned)),
                CaptureKey::Clip(hash),
            );
        }
    }

    fn delete(&self, hash: [u8; 32]) {
        if self.clip_on() {
            self.record(
                Domain::Clip,
                keys::clip_content(&hash),
                JournalValue::Delete,
                CaptureKey::Clip(hash),
            );
        }
    }

    fn clear(&self, keep_pinned: bool) {
        if self.clip_on() {
            let key = if keep_pinned {
                keys::CLIP_CLEAR_UNPINNED
            } else {
                keys::CLIP_CLEAR_ALL
            };
            self.record(Domain::Clip, key.to_owned(), clear_value(), CaptureKey::ClipClear);
        }
    }

    fn ocr_add(&self, key: &str) {
        if self.ocr_on() {
            self.record(
                Domain::Ocr,
                keys::ocr_item(key),
                JournalValue::Deferred,
                CaptureKey::Ocr(key.to_owned()),
            );
        }
    }

    fn ocr_delete(&self, key: &str) {
        if self.ocr_on() {
            self.record(
                Domain::Ocr,
                keys::ocr_item(key),
                JournalValue::Delete,
                CaptureKey::Ocr(key.to_owned()),
            );
        }
    }

    fn ocr_clear(&self) {
        if self.ocr_on() {
            self.record(
                Domain::Ocr,
                keys::OCR_CLEAR.to_owned(),
                clear_value(),
                CaptureKey::OcrClear,
            );
        }
    }
}
