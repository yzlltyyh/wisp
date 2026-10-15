//! 推送：按批次提交本机的待上传记录。
//!
//! 顺序：配置、Key、置顶、清空与删除 → 剪贴板文本（新到旧）→ 图片与识别记录（新到旧，每批 blob 有上限）。
//! 每批是一个日志段 + 一次 head 写入，中断后已完成的批次对其他设备可见。每批的步骤：
//! 1. 先上传本批引用的、本机账本里没有的 blob（上传失败只让引用它的记录留在待上传）；
//! 2. 分配新的写入编号与序号，**先把进行中的写入持久化**；
//! 3. PUT 段 → PUT head（含新段）；
//! 4. 在一个事务里写本机日志账本、按 `(d, k, t)` 精确清除待上传、删除进行中的写入。
//!
//! 编号一经分配就不再复用；head 写入失败（包括响应丢失）时，下一轮重新读一次本机 head 再决定补做还是丢弃。

use std::sync::Arc;

use bytes::Bytes;
use serde_json::Value;

use super::cx::{Cx, CycleState, blocking, remove_quietly, sig_of_put};
use super::pull::seal_key_value;
use super::{BLOBS_PER_BATCH, Exported, LocalBlob, LocalStore, SEGMENT_MAX_PLAINTEXT_BYTES};
use crate::crypto::{self, FileKind};
use crate::error::RemoteError;
use crate::error::{LocalError, SyncError};
use crate::hlc::wall_now_ms;
use crate::layout;
use crate::model::{
    BlobRef, BlobRole, Domain, FORMAT_VERSION, HeadPayload, Record, SegmentEntry, SegmentPayload, segment_hash,
};
use crate::remote::Remote;
use crate::replica::{Inflight, PushedExtra, Row};

/// 批里的一条记录
struct Item {
    row: Row,
    record: Record,
    /// 需要上传的本地文件与 blob 名
    blobs: Vec<(LocalBlob, String)>,
    /// 序列化后的大致字节数
    size: usize,
}

#[derive(Default)]
struct Batch {
    items: Vec<Item>,
    size: usize,
    blobs: usize,
    extra: PushedExtra,
}

impl Batch {
    fn fits(&self, item: &Item, max_blobs: usize) -> bool {
        self.items.is_empty()
            || (self.size + item.size <= SEGMENT_MAX_PLAINTEXT_BYTES && self.blobs + item.blobs.len() <= max_blobs)
    }

    fn push(&mut self, item: Item) {
        self.size += item.size;
        self.blobs += item.blobs.len();
        self.items.push(item);
    }
}

fn is_item(row: &Row) -> bool {
    !row.tomb
        && matches!(
            (&row.d, row.k.get(..2)),
            (Domain::Clip, Some("c/")) | (Domain::Ocr, Some("o/"))
        )
}

fn record_size(record: &Record) -> usize {
    serde_json::to_vec(record).map_or(SEGMENT_MAX_PLAINTEXT_BYTES, |v| v.len())
}

/// 推送 `domains` 中（且当前开关允许的）本机待上传记录
pub(super) async fn push<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    state: &mut CycleState,
    domains: &[Domain],
) -> Result<(), SyncError> {
    let own = cx.own.clone();
    let rows = cx.db(move |r| r.pending_rows(&own)).await?;
    let keychain = cx.options.keychain;
    let rows: Vec<Row> = rows
        .into_iter()
        .filter(|r| domains.contains(&r.d) && cx.options.content.allows(&r.d) && (r.d != Domain::Key || keychain))
        .collect();
    if rows.is_empty() {
        return Ok(());
    }
    cx.add_work(u32::try_from(rows.len()).unwrap_or(u32::MAX));
    let (mut items, small): (Vec<Row>, Vec<Row>) = rows.into_iter().partition(is_item);
    let mut unpublished: Vec<(String, String, i64)> = Vec::new();

    // 第一层：配置、Key、置顶、清空与删除（小记录）
    let mut batch = Batch::default();
    for row in small {
        if row.d == Domain::Key && cx.keychain_error {
            continue;
        }
        let Some((record, key_value)) = small_record(cx, &row).await? else {
            if !cx.keychain_error || row.d != Domain::Key {
                unpublished.push((row.d.as_str().to_string(), row.k.clone(), row.t.to_i64()));
            }
            continue;
        };
        let item = Item {
            size: record_size(&record),
            record,
            row,
            blobs: Vec::new(),
        };
        if !batch.fits(&item, usize::MAX) {
            flush(cx, state, std::mem::take(&mut batch)).await?;
        }
        if let Some(kv) = key_value {
            batch.extra.key_values.push(kv);
        }
        batch.push(item);
    }
    flush(cx, state, std::mem::take(&mut batch)).await?;

    // 第二、三层：条目新到旧；没有附件的先走，带附件的随后每批限量
    items.sort_by_key(|r| std::cmp::Reverse(r.t));
    let mut with_blobs: Vec<(Item, i64)> = Vec::new();
    let mut text = Batch::default();
    for chunk in items.chunks(64) {
        let wanted: Vec<(Domain, String)> = chunk.iter().map(|r| (r.d.clone(), r.k.clone())).collect();
        let exported = cx
            .local_call(move |l| Ok(wanted.into_iter().map(|(d, k)| l.export(d, &k)).collect::<Vec<_>>()))
            .await??;
        for (row, exported) in chunk.iter().zip(exported) {
            match exported {
                Ok(Some(exp)) => match prepare_item(cx, row, exp)? {
                    Some((item, ms)) if item.blobs.is_empty() => {
                        if !text.fits(&item, usize::MAX) {
                            flush(cx, state, std::mem::take(&mut text)).await?;
                        }
                        if let Some(ms) = ms {
                            text.extra
                                .item_times
                                .push((row.d.clone(), row.k.clone(), row.t.to_i64(), ms));
                        }
                        text.push(item);
                    }
                    Some((item, ms)) => with_blobs.push((item, ms.unwrap_or(0))),
                    None => unpublished.push((row.d.as_str().to_string(), row.k.clone(), row.t.to_i64())),
                },
                // 本机已没有：不发布
                Ok(None) => unpublished.push((row.d.as_str().to_string(), row.k.clone(), row.t.to_i64())),
                // 本机存储暂不可用：留在待上传，下次再试
                Err(LocalError::Unavailable) => {}
                Err(e) => log::warn!("sync: reading a local {} item failed: {e}", row.d),
            }
        }
    }
    flush(cx, state, text).await?;

    let mut blob_batch = Batch::default();
    let mut planned = 0u64;
    for (item, ms) in with_blobs {
        // 单轮上传额度：超出就结束本轮，让新改动可以插队
        let bytes: u64 = item.blobs.iter().map(|(b, _)| b.bytes).sum();
        if planned > 0 && planned.saturating_add(bytes) > cx.up_budget {
            cx.report.again = true;
            break;
        }
        planned = planned.saturating_add(bytes);
        if !blob_batch.fits(&item, BLOBS_PER_BATCH) {
            flush(cx, state, std::mem::take(&mut blob_batch)).await?;
        }
        blob_batch
            .extra
            .item_times
            .push((item.row.d.clone(), item.row.k.clone(), item.row.t.to_i64(), ms));
        blob_batch.push(item);
    }
    flush(cx, state, blob_batch).await?;

    if !unpublished.is_empty() {
        let own = cx.own.clone();
        cx.db(move |r| r.clear_pending_exact(&unpublished, &own)).await?;
    }
    Ok(())
}

/// 小记录的值：墓碑、副本里保存的值，或推送时从本机读取（Key 一律从钥匙串读取）
async fn small_record<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    row: &Row,
) -> Result<Option<(Record, Option<(String, i64, String, Option<String>)>)>, SyncError> {
    let mut record = Record {
        d: row.d.clone(),
        k: row.k.clone(),
        t: row.t,
        v: None,
        b: Vec::new(),
    };
    if row.tomb {
        return Ok(Some((record, None)));
    }
    let stored = if row.d == Domain::Key || row.deferred {
        None
    } else {
        row.json()
    };
    let value = match stored {
        Some(v) => v,
        None => {
            let (d, k) = (row.d.clone(), row.k.clone());
            match cx.local_call(move |l| l.export(d, &k)).await? {
                Ok(Some(Exported { value, .. })) => value,
                Ok(None) => return Ok(None),
                Err(LocalError::Keychain) => {
                    cx.keychain_error = true;
                    return Ok(None);
                }
                Err(e) => {
                    log::warn!("sync: reading a local {} value failed: {e}", row.d);
                    return Ok(None);
                }
            }
        }
    };
    let key_value = if row.d == Domain::Key {
        let sealed = seal_key_value(&cx.session, &row.k, &value)?;
        let origin = value.get("origin").and_then(Value::as_str).map(str::to_string);
        Some((row.k.clone(), row.t.to_i64(), sealed, origin))
    } else {
        None
    };
    record.v = Some(value);
    Ok(Some((record, key_value)))
}

/// 条目的发布形态：过滤附件（剪贴板图片超限或图片开关关闭 ⇒ 整条不发布；识别记录只去掉原图），得出 blob 名
fn prepare_item<R: Remote, L: LocalStore + ?Sized>(
    cx: &Cx<'_, R, L>,
    row: &Row,
    exported: Exported,
) -> Result<Option<(Item, Option<i64>)>, SyncError> {
    let images_ok = |b: &LocalBlob| cx.options.content.images && b.bytes <= cx.options.images_max_bytes;
    let mut blobs = Vec::new();
    for b in exported.blobs {
        let keep = match (&row.d, &b.role) {
            (Domain::Clip, BlobRole::Image) => {
                if !images_ok(&b) {
                    return Ok(None);
                }
                true
            }
            (Domain::Ocr, BlobRole::Image) => images_ok(&b),
            (Domain::Ocr, BlobRole::Thumbnail) => true,
            _ => false,
        };
        if keep {
            let name = crypto::blob_name(&cx.session.keys, &b.identity);
            if !layout::is_valid_blob_name(&name) {
                return Err(SyncError::Internal("blob name"));
            }
            blobs.push((b, name));
        }
    }
    let ms = match row.d {
        Domain::Clip => exported.value.get("last_used_at").and_then(Value::as_i64),
        Domain::Ocr => exported.value.get("created_at").and_then(Value::as_i64),
        _ => None,
    };
    let record = Record {
        d: row.d.clone(),
        k: row.k.clone(),
        t: row.t,
        v: Some(exported.value),
        b: blobs
            .iter()
            .map(|(b, name)| BlobRef {
                dev: cx.own.clone(),
                name: name.clone(),
                bytes: b.bytes,
                role: b.role.clone(),
            })
            .collect(),
    };
    Ok(Some((
        Item {
            size: record_size(&record),
            row: row.clone(),
            record,
            blobs,
        },
        ms,
    )))
}

/// 上传一个 blob（流式加密到 spool 临时文件后上传）
async fn upload_blob<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    state: &mut CycleState,
    blob: &LocalBlob,
    name: &str,
) -> Result<(), SyncError> {
    let n = name.to_string();
    cx.db(move |r| r.blob_intent(&n)).await?;
    let dir = layout::blob_dir(cx.vault_id(), &cx.own);
    state.dirs.ensure(&cx.remote, &dir).await?;
    let tmp = cx.spool_file("up");
    let session = Arc::clone(&cx.session);
    let aad = layout::blob_aad_name(&cx.own, name);
    let (src, dest) = (blob.path.clone(), tmp.clone());
    let sealed = blocking(move || crypto::seal_blob_file(&session.keys, &session.vault_id, &aad, &src, &dest)).await?;
    let len = match sealed {
        Ok(len) => len,
        Err(e) => {
            remove_quietly(&tmp);
            return Err(e);
        }
    };
    let path = layout::blob_path(cx.vault_id(), &cx.own, name);
    let put = cx.remote.put_file(&path, &tmp, len).await;
    remove_quietly(&tmp);
    if let Err(RemoteError::Status(409)) = &put {
        // 目录被删了：下次重新建
        state.dirs.clear();
    }
    put?;
    cx.up_budget = cx.up_budget.saturating_sub(len);
    let n = name.to_string();
    let now = i64::try_from(wall_now_ms()).unwrap_or(i64::MAX);
    cx.db(move |r| r.blob_done(&n, now)).await?;
    Ok(())
}

/// 提交一批：上传 blob → 持久化进行中的写入 → PUT 段 → PUT head → 写账本并精确清除待上传
async fn flush<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    state: &mut CycleState,
    batch: Batch,
) -> Result<(), SyncError> {
    if batch.items.is_empty() {
        return Ok(());
    }
    let mut extra = batch.extra;
    let mut records = Vec::with_capacity(batch.items.len());
    let mut keys = Vec::with_capacity(batch.items.len());
    let mut blob_failures = 0;
    for item in batch.items {
        let mut ok = true;
        for (blob, name) in &item.blobs {
            let n = name.clone();
            if cx.db(move |r| r.blob_uploaded(&n)).await? {
                continue;
            }
            match upload_blob(cx, state, blob, name).await {
                Ok(()) => {}
                // 持续性错误（认证、空间不足等）：整轮停下，交给调用方
                Err(SyncError::Remote(e)) if !e.is_transient() => return Err(SyncError::Remote(e)),
                Err(e) => {
                    // 一张图失败只让引用它的记录留在待上传，本轮其余工作照常，结束时报告这次失败（调用方退避后重试）；
                    // 连续失败说明网络不通，停下
                    blob_failures += 1;
                    log::warn!("sync: uploading an attachment failed: {}", e.kind().as_str());
                    if blob_failures > 1 {
                        return Err(e);
                    }
                    if cx.blob_error.is_none() {
                        cx.blob_error = Some(e);
                    }
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            keys.push((item.row.d.as_str().to_string(), item.row.k.clone(), item.row.t.to_i64()));
            records.push(item.record);
        }
    }
    if records.is_empty() {
        return Ok(());
    }
    let count = u32::try_from(records.len()).unwrap_or(u32::MAX);
    extra
        .key_values
        .retain(|(k, t, _, _)| keys.iter().any(|(d, kk, tt)| d == "key" && kk == k && tt == t));
    extra
        .item_times
        .retain(|(d, k, t, _)| keys.iter().any(|(dd, kk, tt)| dd == d.as_str() && kk == k && tt == t));

    let (w, seq) = cx.db(|r| r.allocate(1, 1)).await?;
    let own = cx.own.clone();
    let payload = SegmentPayload {
        v: FORMAT_VERSION,
        device: own.clone(),
        w,
        lo: seq,
        hi: seq,
        schema: cx.options.schema,
        records,
    };
    let session = Arc::clone(&cx.session);
    let owned = own.clone();
    let sealed = blocking(move || {
        let enc = payload.encode()?;
        crypto::seal_small(
            &session.keys,
            FileKind::Segment,
            enc.flags,
            &session.vault_id,
            &layout::segment_aad_name(&owned, w),
            &enc.bytes,
        )
    })
    .await??;
    let entry = SegmentEntry {
        w,
        lo: seq,
        hi: seq,
        bytes: sealed.len() as u64,
        hash: segment_hash(&sealed),
    };
    let flight = Inflight {
        entry: entry.clone(),
        records: keys,
        compaction: false,
    };
    let persisted = flight.clone();
    cx.db(move |r| r.begin_inflight(std::slice::from_ref(&persisted)))
        .await?;

    let dir = layout::log_dir(cx.vault_id(), &own);
    state.dirs.ensure(&cx.remote, &dir).await?;
    let seg_path = layout::segment_path(cx.vault_id(), &own, w);
    if let Err(e) = cx.remote.put(&seg_path, Bytes::from(sealed)).await {
        if e == RemoteError::Status(409) {
            state.dirs.clear();
        }
        // 段没写成：进行中的写入留到下一轮核对 head 时丢弃，记录保持待上传，下次用新的编号
        state.own_verified = false;
        return Err(e.into());
    }
    let ledger = cx.db(|r| r.ledger()).await?;
    let mut segments = ledger;
    segments.push(entry);
    let info = put_head(cx, state, segments).await?;
    let sig = sig_of_put(&info);
    let own2 = own.clone();
    cx.db(move |r| r.commit_push(&own2, &flight, &extra, sig.as_deref()))
        .await?;
    cx.report.pushed = cx.report.pushed.saturating_add(count);
    cx.work_done(count);
    Ok(())
}

/// 写本机 head。失败（包括响应丢失）时让下一轮重新核对本机 head
pub(super) async fn put_head<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    state: &mut CycleState,
    segments: Vec<SegmentEntry>,
) -> Result<crate::remote::PutInfo, SyncError> {
    let mut device = cx.engine.device();
    device.id = cx.own.clone();
    let head = HeadPayload {
        v: FORMAT_VERSION,
        vault_id: cx.vault_id().to_string(),
        device,
        updated_ms: i64::try_from(wall_now_ms()).unwrap_or(i64::MAX),
        segments,
    };
    let session = Arc::clone(&cx.session);
    let own = cx.own.clone();
    let sealed = blocking(move || {
        let enc = head.encode()?;
        crypto::seal_small(
            &session.keys,
            FileKind::Head,
            enc.flags,
            &session.vault_id,
            &layout::head_aad_name(&own),
            &enc.bytes,
        )
    })
    .await??;
    let path = layout::head_path(cx.vault_id(), &cx.own);
    match cx.remote.put(&path, Bytes::from(sealed)).await {
        Ok(info) => {
            cx.head_written = true;
            Ok(info)
        }
        Err(e) => {
            state.own_verified = false;
            Err(e.into())
        }
    }
}

/// 本机 head 不在服务器上（刚加入、换了新 id、或被意外删除）且本轮没有写过：写一份，让其他设备能看到本机
pub(super) async fn ensure_head<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    state: &mut CycleState,
) -> Result<(), SyncError> {
    if cx.own_head_listed || cx.head_written {
        return Ok(());
    }
    let segments = cx.db(|r| r.ledger()).await?;
    let info = put_head(cx, state, segments).await?;
    let sig = sig_of_put(&info);
    cx.db(move |r| match sig {
        Some(sig) => r.set_meta(crate::replica::META_OWN_SIG, &sig),
        None => r.del_meta(crate::replica::META_OWN_SIG),
    })
    .await
}
