//! 压缩：只重写自己的日志，按**复制**而不是重新导出——GET 本机全部段并解密，逐条保留仍需要的记录，
//! 原样写成覆盖 `[1, max_hi]` 的新段 → PUT head → 删除被替换的段与孤儿段 → 回收不再被引用的本机 blob。
//!
//! 保留一条本机记录，当且仅当：副本里该 key 的胜出记录正是它，或它只被本机更新的（尚未上传的）记录取代；
//! 它没有被清空判定式隐藏；剪贴板 / 识别记录还在（已同步的）保留策略范围内；对应的内容开关仍然打开。
//! 被别的设备取代的记录可以丢：取代者的作者会保留它（同一条不变式），所以服务器上的历史有界、
//! 删除与清空的内容最终从服务器上消失。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use bytes::Bytes;

use super::content::{beats, clip_alive, ocr_alive, retention_drops, retention_sig};
use super::cx::{Cx, CycleState, blocking, sig_of_put};
use super::pull::fetch_segment;
use super::push::put_head;
use super::{
    COMPACT_RETENTION_BATCH, COMPACT_SEGMENT_LIMIT, COMPACT_SLACK_BYTES, CycleOptions, LocalStore,
    SEGMENT_MAX_PLAINTEXT_BYTES,
};
use crate::crypto::{self, FileKind};
use crate::error::SyncError;
use crate::hlc::wall_now_ms;
use crate::layout;
use crate::model::{Domain, FORMAT_VERSION, Record, SegmentEntry, SegmentPayload, segment_hash};
use crate::remote::Remote;
use crate::replica::{self, Inflight, Replica};

fn now_ms() -> i64 {
    i64::try_from(wall_now_ms()).unwrap_or(i64::MAX)
}

/// 推送之后判断是否需要压缩（不靠计时器）：段数过多、日志相对上次压缩膨胀、「需要清除」标志、
/// 内容开关关闭后本机日志里还有该域的记录，或超出保留策略的已发布本机条目攒够了一批
fn should_compact(r: &Replica, own: &str, options: &CycleOptions) -> Result<Option<Vec<SegmentEntry>>, SyncError> {
    let ledger = r.ledger()?;
    if ledger.is_empty() {
        return Ok(None);
    }
    let purge = r.meta(replica::META_PURGE)?.as_deref() == Some("1");
    let base = r.meta_u64(replica::META_COMPACT_BYTES)?.unwrap_or(0);
    let total: u64 = ledger.iter().map(|e| e.bytes).sum();
    let mut trigger = ledger.len() > COMPACT_SEGMENT_LIMIT
        || total > base.saturating_mul(2).saturating_add(COMPACT_SLACK_BYTES)
        || purge;
    // 关闭了剪贴板 / 识别历史的同步：本机上传过的记录要从服务器上移除（开关说明承诺了这一点），
    // 不等别的原因触发压缩。压缩后这些记录不再在本机日志里，条件随之消失
    for d in [Domain::Clip, Domain::Ocr] {
        if !trigger && !options.content.allows(&d) {
            trigger = r.has_own_published(own, &d)?;
        }
    }
    if !trigger {
        trigger = retention_due(r, own, options)?;
    }
    Ok(trigger.then_some(ledger))
}

/// 超出保留策略、仍在本机日志里的本机条目是否攒够了一批。
/// 剪贴板 / 识别记录与策略都没变时结论不变：记下签名，下次不必再读取整个域
fn retention_due(r: &Replica, own: &str, options: &CycleOptions) -> Result<bool, SyncError> {
    let now = now_ms();
    let sig = retention_sig(&options.retention, now);
    if r.meta(replica::META_RETENTION_EVAL)?.as_deref() == Some(sig.as_str()) {
        return Ok(false);
    }
    let clip = r.content_view(&Domain::Clip)?;
    let ocr = r.content_view(&Domain::Ocr)?;
    let drops = retention_drops(&clip, &ocr, &options.retention, now);
    let published = drops
        .iter()
        .filter(|(d, k)| {
            let view = if *d == Domain::Clip { &clip } else { &ocr };
            matches!(k.get(..2), Some("c/" | "o/"))
                && view
                    .items
                    .get(&k[2..])
                    .is_some_and(|row| row.dev == own && row.src_hi.is_some())
        })
        .count();
    if published >= COMPACT_RETENTION_BATCH {
        return Ok(true);
    }
    r.set_meta(replica::META_RETENTION_EVAL, &sig)?;
    Ok(false)
}

/// 从本机旧日志里挑出要保留的记录（见模块说明）
fn select_survivors(
    r: &Replica,
    own: &str,
    options: &CycleOptions,
    records: Vec<Record>,
) -> Result<Vec<Record>, SyncError> {
    let clip = r.content_view(&Domain::Clip)?;
    let ocr = r.content_view(&Domain::Ocr)?;
    let drops = retention_drops(&clip, &ocr, &options.retention, now_ms());
    let mut out = Vec::with_capacity(records.len());
    for rec in records {
        if rec.d.is_content() && !options.content.allows(&rec.d) {
            // 内容开关关闭后从本机日志里移除（不写墓碑）
            continue;
        }
        let Some(row) = r.row(&rec.d, &rec.k)? else {
            // 副本不认识这个 key（不应出现）：保守地保留
            out.push(rec);
            continue;
        };
        if row.dev != own {
            continue;
        }
        if row.t == rec.t {
            if drops.contains(&(rec.d.clone(), rec.k.clone())) {
                continue;
            }
            let keep = match (&rec.d, rec.k.as_str()) {
                (Domain::Clip, k) if k.starts_with("c/") => {
                    let pin = clip.pins.get(&k[2..]);
                    if row.tomb {
                        // 被全部清空覆盖的删除不再需要
                        beats(&row, clip.clear_all.as_ref())
                    } else {
                        clip_alive(&row, pin, &clip)
                    }
                }
                // 置顶寄存器即使条目眼下不存在也要留着（条目可能被再次复制而复活）；只有早于「清空全部」的不再起作用
                (Domain::Clip, k) if k.starts_with("p/") => beats(&row, clip.clear_all.as_ref()),
                (Domain::Ocr, k) if k.starts_with("o/") => {
                    if row.tomb {
                        beats(&row, ocr.clear_all.as_ref())
                    } else {
                        ocr_alive(&row, &ocr)
                    }
                }
                _ => true,
            };
            if keep {
                out.push(rec);
            }
        } else if row.t > rec.t && (row.pending || row.src_hi.is_none()) {
            // 只被本机更新、尚未发布的记录取代：保留旧的，直到新的上传
            out.push(rec);
        }
    }
    Ok(out)
}

/// 把记录切成不超过上限的若干部分（至少一部分，保住 `max_hi`）
fn split_parts(records: Vec<Record>) -> Vec<Vec<Record>> {
    let mut parts: Vec<Vec<Record>> = vec![Vec::new()];
    let mut size = 0usize;
    for rec in records {
        let n = serde_json::to_vec(&rec).map_or(SEGMENT_MAX_PLAINTEXT_BYTES, |v| v.len());
        if size + n > SEGMENT_MAX_PLAINTEXT_BYTES && parts.last().is_some_and(|p| !p.is_empty()) {
            parts.push(Vec::new());
            size = 0;
        }
        size += n;
        if let Some(last) = parts.last_mut() {
            last.push(rec);
        }
    }
    parts
}

/// 视情况压缩本机日志并回收 blob
pub(super) async fn maybe_compact<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    state: &mut CycleState,
) -> Result<(), SyncError> {
    let own = cx.own.clone();
    let options = cx.options.clone();
    let Some(ledger) = cx.db(move |r| should_compact(r, &own, &options)).await? else {
        return Ok(());
    };
    compact(cx, state, ledger).await
}

async fn compact<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    state: &mut CycleState,
    ledger: Vec<SegmentEntry>,
) -> Result<(), SyncError> {
    let own = cx.own.clone();
    let max_hi = ledger.iter().map(|e| e.hi).max().unwrap_or(0);
    // 每个 key 只留本机日志里最新的一版
    let mut latest: HashMap<(Domain, String), Record> = HashMap::new();
    // 服务器上已经没有的本机段的序号区间
    let mut lost: Vec<(u64, u64)> = Vec::new();
    for entry in &ledger {
        match fetch_segment(cx, &own, entry).await {
            Ok(Some(seg)) => {
                for rec in seg.records {
                    let key = (rec.d.clone(), rec.k.clone());
                    if latest.get(&key).is_none_or(|old| old.t < rec.t) {
                        latest.insert(key, rec);
                    }
                }
            }
            // 段已丢失（被删除、服务器回滚，或刚写入还读不到）：其中的记录别的设备拿不到了，
            // 但本机副本里仍是胜出者，压缩后重新发布
            Ok(None) => {
                log::warn!("sync: a segment of this device is missing on the server");
                lost.push((entry.lo, entry.hi));
            }
            Err(SyncError::Corrupt | SyncError::FormatTooNew) => {
                // 读不懂自己的段：宁可不压缩，也不丢数据
                log::warn!("sync: skipped compaction because an own segment could not be read");
                return Ok(());
            }
            Err(e) => return Err(e),
        }
    }
    let mut records: Vec<Record> = latest.into_values().collect();
    records.sort_by(|a, b| a.t.cmp(&b.t).then_with(|| a.k.cmp(&b.k)));
    let own2 = own.clone();
    let options = cx.options.clone();
    let survivors = cx.db(move |r| select_survivors(r, &own2, &options, records)).await?;
    let kept: Vec<(String, String, i64)> = survivors
        .iter()
        .map(|r| (r.d.as_str().to_string(), r.k.clone(), r.t.to_i64()))
        .collect();
    let referenced: HashSet<String> = survivors
        .iter()
        .flat_map(|r| r.b.iter())
        .filter(|b| b.dev == own)
        .map(|b| b.name.clone())
        .collect();
    // 只存在于丢失的段里、副本中仍由本机胜出的记录（不在保留下来的记录里）；内容开关关闭的域不重新发布
    let republish: Vec<(String, String, i64)> = if lost.is_empty() {
        Vec::new()
    } else {
        let own_r = own.clone();
        let rows = cx.db(move |r| r.own_published(&own_r)).await?;
        let kept_set: HashSet<&(String, String, i64)> = kept.iter().collect();
        rows.into_iter()
            .filter(|(d, _, _, hi)| {
                cx.options.content.allows(&Domain::parse(d)) && lost.iter().any(|(lo, top)| lo <= hi && hi <= top)
            })
            .map(|(d, k, t, _)| (d, k, t))
            .filter(|key| !kept_set.contains(key))
            .collect()
    };

    let parts = split_parts(survivors);
    let n = parts.len() as u64;
    let (w0, _) = cx.db(move |r| r.allocate(n, 0)).await?;
    let session = Arc::clone(&cx.session);
    let schema = cx.options.schema;
    let own3 = own.clone();
    let sealed: Vec<(SegmentEntry, Vec<u8>)> = blocking(move || {
        let mut out = Vec::new();
        for (i, records) in parts.into_iter().enumerate() {
            let w = w0 + i as u64;
            let payload = SegmentPayload {
                v: FORMAT_VERSION,
                device: own3.clone(),
                w,
                lo: 1,
                hi: max_hi,
                schema,
                records,
            };
            let enc = payload.encode()?;
            let bytes = crypto::seal_small(
                &session.keys,
                FileKind::Segment,
                enc.flags,
                &session.vault_id,
                &layout::segment_aad_name(&own3, w),
                &enc.bytes,
            )?;
            let entry = SegmentEntry {
                w,
                lo: 1,
                hi: max_hi,
                bytes: bytes.len() as u64,
                hash: segment_hash(&bytes),
            };
            out.push((entry, bytes));
        }
        Ok::<_, SyncError>(out)
    })
    .await??;
    let entries: Vec<SegmentEntry> = sealed.iter().map(|(e, _)| e.clone()).collect();
    let flights: Vec<Inflight> = entries
        .iter()
        .map(|e| Inflight {
            entry: e.clone(),
            records: Vec::new(),
            compaction: true,
        })
        .collect();
    cx.db(move |r| r.begin_inflight(&flights)).await?;
    let dir = layout::log_dir(cx.vault_id(), &own);
    state.dirs.ensure(&cx.remote, &dir).await?;
    for (entry, bytes) in sealed {
        let path = layout::segment_path(cx.vault_id(), &own, entry.w);
        if let Err(e) = cx.remote.put(&path, Bytes::from(bytes)).await {
            state.own_verified = false;
            return Err(e.into());
        }
    }
    let info = put_head(cx, state, entries.clone()).await?;
    let sig = sig_of_put(&info);
    let (own4, parts_done) = (own.clone(), entries.clone());
    cx.db(move |r| r.commit_compaction(&own4, &parts_done, &kept, &republish, max_hi, sig.as_deref()))
        .await?;
    cx.report.compacted = true;

    // 删除被替换的段与孤儿段（之前失败的写入留下的）：列一次自己的日志目录
    let keep: HashSet<String> = entries.iter().map(|e| format!("{:010}.seg", e.w)).collect();
    match cx.remote.list(&dir).await {
        Ok(Some(files)) => {
            for f in files
                .into_iter()
                .filter(|f| !f.is_dir && f.name.ends_with(".seg") && !keep.contains(&f.name))
            {
                let path = format!("{dir}/{}", f.name);
                if let Err(e) = cx.remote.delete(&path).await {
                    log::debug!("sync: deleting a replaced segment failed: {}", e.detail());
                }
            }
        }
        _ => {
            for old in ledger.iter().filter(|e| !entries.iter().any(|n| n.w == e.w)) {
                let path = layout::segment_path(cx.vault_id(), &own, old.w);
                if let Err(e) = cx.remote.delete(&path).await {
                    log::debug!("sync: deleting a replaced segment failed: {}", e.detail());
                }
            }
        }
    }

    // 回收本机 blob：只有上传者回收，只看本机账本，不列目录。本机还有待上传的条目时引用尚未确定，本轮不回收
    let own5 = own.clone();
    let (pending, names) = cx.db(move |r| Ok((r.content_pending(&own5)?, r.blob_names()?))).await?;
    if !pending {
        for name in names.into_iter().filter(|n| !referenced.contains(n)) {
            if !layout::is_valid_blob_name(&name) {
                let n = name.clone();
                cx.db(move |r| r.blob_forget(&n)).await?;
                continue;
            }
            let path = layout::blob_path(cx.vault_id(), &own, &name);
            let n = name.clone();
            cx.db(move |r| r.blob_deleting(&n)).await?;
            match cx.remote.delete(&path).await {
                Ok(()) => {
                    cx.db(move |r| r.blob_forget(&name)).await?;
                }
                Err(e) => log::debug!("sync: deleting an attachment failed: {}", e.detail()),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::RetentionPolicy;
    use crate::hlc::Hlc;
    use serde_json::json;

    const OWN: &str = "aaaaaaaaaaaaaaaa";
    const PEER: &str = "bbbbbbbbbbbbbbbb";

    fn seal(_: &str, _: &serde_json::Value) -> Result<String, SyncError> {
        Ok(String::new())
    }

    fn item(n: usize, ms: i64) -> Record {
        Record {
            d: Domain::Clip,
            k: format!("c/{n:064x}"),
            t: Hlc::from_wall_ms_i64(ms),
            v: Some(json!({ "last_used_at": ms })),
            b: Vec::new(),
        }
    }

    fn options(max_items: u32) -> CycleOptions {
        CycleOptions {
            retention: RetentionPolicy {
                clip_max_items: max_items,
                ..RetentionPolicy::default()
            },
            ..CycleOptions::default()
        }
    }

    #[test]
    fn retention_compacts_in_batches_and_skips_unchanged_checks() {
        let mut r = Replica::open_in_memory().unwrap();
        r.reset_for_vault("0123456789abcdef", OWN).unwrap();
        let entry = SegmentEntry {
            w: 1,
            lo: 1,
            hi: 1,
            bytes: 10,
            hash: "h".into(),
        };
        r.reconcile_own_head(OWN, &[entry], None).unwrap();
        // 本机已发布的条目，超出上限的差一条才够一批
        let n = 5 + COMPACT_RETENTION_BATCH - 1;
        let records: Vec<Record> = (0..n).map(|i| item(i, 1_000 + i64::try_from(i).unwrap())).collect();
        r.merge_device(OWN, OWN, &[(1, records)], None, &seal).unwrap();
        assert!(should_compact(&r, OWN, &options(5)).unwrap().is_none());
        // 结论记下了：内容与策略都没变时不再读取整个域
        assert!(r.meta(replica::META_RETENTION_EVAL).unwrap().is_some());
        assert!(should_compact(&r, OWN, &options(5)).unwrap().is_none());
        // 策略变了：重新检查
        assert!(should_compact(&r, OWN, &options(4)).unwrap().is_some());
        // 其他设备更新的条目把一条本机条目挤出上限：内容变化让检查重新进行，攒够一批就压缩
        r.merge_device(OWN, PEER, &[(1, vec![item(n, 9_000_000)])], None, &seal)
            .unwrap();
        assert!(r.meta(replica::META_RETENTION_EVAL).unwrap().is_none());
        assert!(should_compact(&r, OWN, &options(5)).unwrap().is_some());
    }

    #[test]
    fn switched_off_content_with_own_records_in_the_log_triggers_compaction() {
        let mut r = Replica::open_in_memory().unwrap();
        r.reset_for_vault("0123456789abcdef", OWN).unwrap();
        let entry = SegmentEntry {
            w: 1,
            lo: 1,
            hi: 1,
            bytes: 10,
            hash: "h".into(),
        };
        r.reconcile_own_head(OWN, &[entry], None).unwrap();
        r.merge_device(OWN, OWN, &[(1, vec![item(1, 1_000)])], None, &seal)
            .unwrap();
        let mut off = options(1000);
        assert!(should_compact(&r, OWN, &off).unwrap().is_none());
        off.content.clipboard = false;
        assert!(should_compact(&r, OWN, &off).unwrap().is_some());
        // 只看本机作者的记录
        let mut r2 = Replica::open_in_memory().unwrap();
        r2.reset_for_vault("0123456789abcdef", OWN).unwrap();
        r2.reconcile_own_head(OWN, &r.ledger().unwrap(), None).unwrap();
        r2.merge_device(OWN, PEER, &[(1, vec![item(1, 1_000)])], None, &seal)
            .unwrap();
        assert!(should_compact(&r2, OWN, &off).unwrap().is_none());
    }
}
