//! 同步开启时（托盘就绪之后、不联网）的本机核对。
//!
//! - 配置：与上次记下的「影子」比较，找出应用没在运行时对配置文件的手改，以新的 t 发布。只有本机值既不同于影子、也不同于
//!   副本里的胜出值时才算手改（等于胜出值说明它就是从别处同步来的）。升级或降级之后影子的形状可能变了，这时静默重建影子，
//!   并让这些域在下一轮重新应用；启动时被新建或重置成默认值的文件不发布任何东西，而是让服务器上的值在下一轮覆盖回来。
//!   文件里有写错、被宽松解析丢掉的内容时（[`LoadState::Lossy`]），缺少的 key 不当作删除发布，只发布新增与修改。
//! - API Key：副本里有、本机钥匙串里却没有的（Linux 只在内存里保存 Key 的会话重启之后）重新标为未应用，下一轮写回。
//! - 剪贴板的收藏状态与名称：在第一次成功同步之后单独核对（[`clip_pins`]），这时远端的收藏都已经应用过了。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::Value;
use wisp_sync::model::PinValue;
use wisp_sync::model::keys::{self, ClipKey};
use wisp_sync::projection::{self, ConfigDoc};
use wisp_sync::{Domain, Engine, Hlc, Journal, JournalEntry, JournalValue, ShadowUpdate, SyncError};
use wisp_types::{ClipKind, OsKind};

use super::capture::{CaptureKey, Captures, Msg, file_of};
use crate::settings::service::{ConfigFile, LoadState};

/// 核对需要的输入
pub struct Startup<'a> {
    pub journal: &'a Journal,
    pub engine: &'a Engine,
    /// 安装钩子那一刻的三份规范化文档
    pub baseline: &'a BTreeMap<ConfigFile, Value>,
    pub load_state: &'a dyn Fn(ConfigFile) -> LoadState,
    /// 手改要用的时间戳：在安装钩子**之前**分配，之后钩子捕获的改动 t 都更大，不会被这里的旧值盖掉
    pub t0: Hlc,
    pub fingerprint: &'a str,
    pub os: OsKind,
    /// 本机是否有这个服务、并且钥匙串索引里有它的 Key
    pub key_present: &'a dyn Fn(&str) -> Option<bool>,
}

/// 核对的结果（只有计数，写日志用）
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// 作为手改发布的配置 key 数
    pub published: usize,
    /// 重新标为未应用的域
    pub reapplied: Vec<Domain>,
}

pub fn run(s: &Startup<'_>) -> Result<Report, SyncError> {
    let mut report = Report::default();
    let mut entries = Vec::new();
    let mut shadows = Vec::new();
    for doc in ConfigDoc::ALL {
        let file = file_of(doc);
        let empty = Value::Null;
        let local = projection::project(doc, s.baseline.get(&file).unwrap_or(&empty), s.os);
        let replace = |map: BTreeMap<String, Value>| ShadowUpdate::Replace {
            doc,
            fingerprint: s.fingerprint.to_owned(),
            map,
        };
        let state = (s.load_state)(file);
        if matches!(state, LoadState::Created | LoadState::ResetToDefault) {
            // 内存里是默认值，不是用户的选择：不发布，下一轮用服务器上的值
            shadows.push(replace(local));
            report.reapplied.push(doc.domain());
            continue;
        }
        let Some(shadow) = s.journal.shadow(doc)? else {
            // 第一次（刚连接）：本机的值已由连接时的加入处理，这里只记下基线
            shadows.push(replace(local));
            continue;
        };
        if shadow.fingerprint != s.fingerprint {
            shadows.push(replace(local));
            report.reapplied.push(doc.domain());
            continue;
        }
        let winners: BTreeMap<String, Option<Value>> = s
            .journal
            .winners(&doc.domain())?
            .into_iter()
            .map(|r| (r.k, r.v))
            .collect();
        let lossy = state == LoadState::Lossy;
        let keys: BTreeSet<&String> = local.keys().chain(shadow.map.keys()).collect();
        for key in keys {
            let mine = local.get(key);
            if mine == shadow.map.get(key) {
                continue;
            }
            if mine == winners.get(key).and_then(Option::as_ref) {
                continue;
            }
            // 手改写错的内容被丢掉了，不是用户删除的：不发墓碑（影子随后改为当前值，改正文件后照常作为新增发布）
            if lossy && mine.is_none() {
                continue;
            }
            entries.push(JournalEntry {
                d: doc.domain(),
                k: key.clone(),
                t: s.t0,
                value: mine.cloned().map_or(JournalValue::Delete, JournalValue::Set),
            });
        }
        shadows.push(replace(local));
    }
    // 本机有这个服务、副本里有它的 Key、钥匙串里却没有：下一轮把 Key 写回
    let missing_key = s
        .journal
        .winners(&Domain::Key)?
        .into_iter()
        .any(|r| r.v.is_some() && (s.key_present)(&r.k) == Some(false));
    if missing_key {
        report.reapplied.push(Domain::Key);
    }
    report.published = entries.len();
    s.journal.record(entries, shadows)?;
    for d in &report.reapplied {
        s.engine.reapply(d)?;
    }
    Ok(report)
}

/// 剪贴板收藏状态与名称的核对：本机与副本不一致的（例如上次退出时还没来得及写进副本的收藏或改名），以新的 t 补发 `p`。
/// 只看副本里有存活内容记录的条目；本机刚改过的跳过（它们自己会被记录）。返回补发的条数
pub fn clip_pins(
    store: &wisp_clipboard::ClipStore,
    journal: &Journal,
    captures: &Captures,
    clock: &Arc<wisp_sync::Clock>,
    tx: &std::sync::mpsc::Sender<Msg>,
) -> Result<usize, SyncError> {
    let rows = store
        .sync_rows()
        .map_err(|_| SyncError::Local(wisp_sync::LocalError::Failed("clipboard rows")))?;
    let mut alive = BTreeSet::new();
    let mut pins = BTreeMap::new();
    for record in journal.winners(&Domain::Clip)? {
        match keys::parse_clip(&record.k) {
            Some(ClipKey::Content(hash)) if record.v.is_some() => {
                alive.insert(hash);
            }
            Some(ClipKey::Pin(hash)) => {
                let value = record.v.as_ref().and_then(PinValue::parse).unwrap_or_default();
                pins.insert(hash, value);
            }
            _ => {}
        }
    }
    let mut sent = 0;
    for row in rows {
        if row.kind == ClipKind::Files || !alive.contains(&row.hash) || captures.clip_pending(&row.hash) {
            continue;
        }
        // 收藏状态与名称一起比较（名称只属于收藏）
        let local = PinValue {
            pin: row.pinned,
            name: row.name.clone().filter(|_| row.pinned),
        };
        if pins.get(&row.hash).cloned().unwrap_or_default() == local {
            continue;
        }
        let t = clock.now();
        captures.note(CaptureKey::Clip(row.hash), t);
        let msg = Msg::Record {
            entry: JournalEntry {
                d: Domain::Clip,
                k: keys::clip_pin(&row.hash),
                t,
                value: JournalValue::Set(local.to_value()),
            },
            capture: CaptureKey::Clip(row.hash),
        };
        if tx.send(msg).is_err() {
            break;
        }
        sent += 1;
    }
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wisp_clipboard::{ClipStore, IngestOutcome, IngestPolicy};
    use wisp_types::{ClipboardSettings, RawClipboard};

    use super::*;

    /// 本机的收藏状态与名称与副本里的 `p/` 不一致时补发对象形态的值；一致（含旧的 bool 值）时不发
    #[test]
    fn favorite_names_are_reconciled_with_the_pin_register() {
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
        let journal = engine.journal().expect("journal");
        let clock = engine.hlc();
        let captures = Captures::default();
        let store = ClipStore::open(&dir.path().join("clip")).expect("open clipboard");
        let raw = RawClipboard {
            change_id: 1,
            text: Some("Guest network: wisp-guest".into()),
            ..RawClipboard::default()
        };
        let policy = IngestPolicy::from(&ClipboardSettings::default());
        let IngestOutcome::Inserted(meta) = store.ingest(&raw, &policy, 1_000).expect("ingest") else {
            panic!("inserted")
        };
        let hash = store.hash_of(meta.id).expect("hash").expect("exists");
        store.set_name(meta.id, Some("Wi-Fi")).expect("name");
        let publish = |k: String, v: Value| {
            journal
                .record(
                    vec![JournalEntry {
                        d: Domain::Clip,
                        k,
                        t: clock.now(),
                        value: JournalValue::Set(v),
                    }],
                    Vec::new(),
                )
                .expect("record");
        };
        publish(keys::clip_content(&hash), json!({"kind": "text"}));
        // 副本里只有旧形态的 `true`：名称不一致，补发
        publish(keys::clip_pin(&hash), json!(true));
        let (tx, rx) = std::sync::mpsc::channel();
        assert_eq!(clip_pins(&store, &journal, &captures, &clock, &tx).expect("pins"), 1);
        let Ok(Msg::Record { entry, .. }) = rx.try_recv() else {
            panic!("record")
        };
        assert_eq!(entry.k, keys::clip_pin(&hash));
        assert!(matches!(entry.value, JournalValue::Set(ref v) if *v == json!({"pin": true, "name": "Wi-Fi"})));
        // 写进副本之后一致：不再补发
        publish(keys::clip_pin(&hash), json!({"pin": true, "name": "Wi-Fi"}));
        let captures = Captures::default();
        assert_eq!(clip_pins(&store, &journal, &captures, &clock, &tx).expect("pins"), 0);
    }
}
