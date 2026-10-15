//! 日志写入线程：把钩子与 sink 捕获的本机改动写进 sync.db。
//!
//! 钩子在调用方的锁里只把一条小消息推进通道；这里每次取出通道里已有的全部消息（不额外等待），配置改动做投影比对，
//! 在一个短事务里写入（通常在捕获后 10 ms 内），然后把这些项从「尚未写入」里移走。线程只在同步开启期间存在。

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex as StdMutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::Value;
use wisp_sync::projection::{self, ConfigDoc};
use wisp_sync::{Domain, Hlc, Journal, JournalEntry, JournalValue, ShadowUpdate, SyncError};
use wisp_types::OsKind;

use super::capture::{CaptureKey, Captures, Msg, doc_of};
use super::identity::provider_origin;
use crate::settings::service::ChangeOrigin;

/// 一次最多合并这么多条消息
const MAX_BATCH: usize = 512;
/// 写入失败时的重试（SQLite 偶尔忙）
const WRITE_ATTEMPTS: u32 = 3;
const RETRY_GAP: Duration = Duration::from_millis(20);

/// 写入线程需要的外部能力
pub struct WriterCtx {
    pub journal: Journal,
    pub captures: Arc<Captures>,
    pub os: OsKind,
    /// 本机是否保存了某个模型服务的 Key（只看索引，不读钥匙串）
    pub has_key: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    /// 写入了本机改动之后调用（安排去抖上传）
    pub on_local: Arc<dyn Fn() + Send + Sync>,
}

/// 写入线程的句柄
pub struct Writer {
    tx: Sender<Msg>,
    handle: StdMutex<Option<JoinHandle<()>>>,
}

impl Writer {
    /// 启动线程。`rx` 里可能已经排着安装钩子之后捕获的改动
    pub fn spawn(tx: Sender<Msg>, rx: Receiver<Msg>, ctx: WriterCtx) -> std::io::Result<Self> {
        let handle = std::thread::Builder::new()
            .name("wisp-sync-journal".into())
            .spawn(move || run(&rx, &ctx))?;
        Ok(Self {
            tx,
            handle: StdMutex::new(Some(handle)),
        })
    }

    /// 等此前捕获的改动全部写完（**阻塞**，最多 `timeout`）。返回是否在期限内写完；线程已经退出时视为写完
    pub fn flush(&self, timeout: Duration) -> bool {
        let (ack, done) = mpsc::sync_channel(1);
        if self.tx.send(Msg::Flush(ack)).is_err() {
            return true;
        }
        done.recv_timeout(timeout).is_ok()
    }

    /// 写完队列后结束线程并等它退出（**阻塞**）。调用前先卸下钩子
    pub fn stop(&self) {
        let _ = self.tx.send(Msg::Stop);
        let handle = self.handle.lock().ok().and_then(|mut h| h.take());
        if let Some(handle) = handle
            && handle.join().is_err()
        {
            log::warn!("sync: the journal thread panicked");
        }
    }

    /// 线程是否还在运行
    pub fn is_running(&self) -> bool {
        self.handle
            .lock()
            .ok()
            .is_some_and(|h| h.as_ref().is_some_and(|h| !h.is_finished()))
    }
}

fn run(rx: &Receiver<Msg>, ctx: &WriterCtx) {
    let mut batch = Vec::new();
    // 所有发送端都没了（同步已关闭）时 recv 返回 Err，线程结束
    while let Ok(first) = rx.recv() {
        batch.push(first);
        while batch.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok(msg) => batch.push(msg),
                Err(_) => break,
            }
        }
        if process(ctx, std::mem::take(&mut batch)) {
            break;
        }
    }
}

/// 处理一批消息；返回是否应当结束线程
pub(super) fn process(ctx: &WriterCtx, msgs: Vec<Msg>) -> bool {
    let mut entries = Vec::new();
    let mut shadows = Vec::new();
    let mut done = Vec::new();
    let mut acks = Vec::new();
    let mut stop = false;
    for msg in msgs {
        match msg {
            Msg::Config {
                file,
                before,
                after,
                origin,
                lossy,
                t,
            } => {
                let doc = doc_of(file);
                let changes = projection::diff(
                    &projection::project(doc, &before, ctx.os),
                    &projection::project(doc, &after, ctx.os),
                );
                // 自己应用的远端值只更新影子：它已经是副本里的胜出值，再记一遍就成了回声
                if origin != ChangeOrigin::Sync {
                    for (key, value) in &changes {
                        // 手改文件里写错、被宽松解析丢掉的内容不是用户删除的：发出墓碑会让每台设备都删掉它（连同那边的
                        // 编辑与学到的能力）。只记新增与修改；用户改正文件后，下一次热加载把它作为新增重新发布。影子照常
                        // 跟随本机的值，所以之后的启动核对也不会把这次缺少当作关闭期间的删除
                        if lossy && value.is_none() {
                            continue;
                        }
                        entries.push(JournalEntry {
                            d: doc.domain(),
                            k: key.clone(),
                            t,
                            value: value.clone().map_or(JournalValue::Delete, JournalValue::Set),
                        });
                    }
                    if doc == ConfigDoc::Settings {
                        entries.extend(carried_keys(ctx, &before, &after, t));
                    }
                    done.push((CaptureKey::Doc(doc), t));
                }
                if !changes.is_empty() {
                    shadows.push(ShadowUpdate::Patch { doc, changes });
                }
            }
            Msg::Record { entry, capture } => {
                done.push((capture, entry.t));
                entries.push(entry);
            }
            Msg::Flush(ack) => acks.push(ack),
            Msg::Stop => stop = true,
        }
    }
    let local = !entries.is_empty();
    if local || !shadows.is_empty() {
        write(&ctx.journal, entries, shadows);
    }
    // 写入失败也移走：留着只会让远端值一直被跳过；这条改动下次再改同一处时会重新记录
    ctx.captures.settle(&done);
    if local {
        (ctx.on_local)();
    }
    for ack in acks {
        let _ = ack.send(());
    }
    stop
}

fn write(journal: &Journal, entries: Vec<JournalEntry>, shadows: Vec<ShadowUpdate>) {
    let mut attempt = 1;
    loop {
        match journal.record(entries.clone(), shadows.clone()) {
            Ok(()) => return,
            Err(e) if attempt < WRITE_ATTEMPTS => {
                log::debug!("sync: journal write retry ({})", describe(&e));
                attempt += 1;
                std::thread::sleep(RETRY_GAP);
            }
            Err(e) => {
                log::warn!(
                    "sync: dropped {} local changes after a journal write failure ({})",
                    entries.len(),
                    describe(&e)
                );
                return;
            }
        }
    }
}

/// 错误的固定词汇描述（种类 + 细节），不含任何内容
pub fn describe(e: &SyncError) -> String {
    match e.detail() {
        Some(detail) => format!("{} {detail}", e.kind().as_str()),
        None => e.kind().as_str().to_owned(),
    }
}

/// 本机改动之后需要重新记一条（以新的 t、推送时再从钥匙串读取）的 Key：
/// - 某个模型服务的地址改到了另一个 origin、却没有重新输入 Key：Key 视为沿用，推送时按新的 origin 导出，这样其他设备也会
///   把它用于新地址，而不是因为 origin 对不上丢掉它；
/// - 新出现的服务已经有 Key：添加服务时界面先保存 Key、再添加服务，中间若刚好有一轮推送，那条 Key 记录会因为「本机没有
///   这个服务」被放弃，之后不会再有别的改动把它发出去。服务出现时补记一条，Key 就跟着服务一起到达其他设备
fn carried_keys(ctx: &WriterCtx, before: &Value, after: &Value, t: Hlc) -> Vec<JournalEntry> {
    let origins = |doc: &Value| -> Vec<(String, Option<String>)> {
        doc.get("providers")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|p| {
                        let id = p.get("id")?.as_str()?.to_owned();
                        let origin = p.get("base_url").and_then(Value::as_str).and_then(provider_origin);
                        Some((id, origin))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let old = origins(before);
    origins(after)
        .into_iter()
        .filter(|(id, origin)| {
            let moved_or_new = match old.iter().find(|(old_id, _)| old_id == id) {
                Some((_, old_origin)) => old_origin != origin,
                None => true,
            };
            moved_or_new && (ctx.has_key)(id)
        })
        .map(|(id, _)| JournalEntry {
            d: Domain::Key,
            k: id,
            t,
            value: JournalValue::Deferred,
        })
        .collect()
}
