//! 拉取：列同步根目录 → 复核 vault → 核对本机 head（进程首轮）→ 只取变化的远端 head → 取新段 → 合并进副本。

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use super::cx::{Cx, CycleState, blocking, sig_of};
use super::{LocalStore, Session};
use crate::crypto::{self, FileKind};
use crate::error::SyncError;
use crate::layout::{self, VAULT_FILE};
use crate::model::{Domain, HeadPayload, Record, SegmentEntry, SegmentPayload, VaultHeader, segment_hash};
use crate::remote::{Entry, Remote};
use crate::replica::{self, MergeStats, Row};

/// 把 key 域的值加密成副本里的存储文本（逻辑名与远端文件的逻辑名不相交）
pub(super) fn seal_key_value(session: &Session, key: &str, value: &Value) -> Result<String, SyncError> {
    use base64::Engine as _;
    let plain = serde_json::to_vec(value).map_err(|_| SyncError::Internal("key value serialization failed"))?;
    let sealed = crypto::seal_small(
        &session.keys,
        FileKind::Segment,
        0,
        &session.vault_id,
        &format!("local/key/{key}"),
        &plain,
    )?;
    Ok(base64::engine::general_purpose::STANDARD.encode(sealed))
}

/// [`seal_key_value`] 的逆
pub(super) fn open_key_value(session: &Session, key: &str, stored: &str) -> Result<Value, SyncError> {
    use base64::Engine as _;
    let sealed = base64::engine::general_purpose::STANDARD
        .decode(stored)
        .map_err(|_| SyncError::Corrupt)?;
    let opened = crypto::open_small(
        &session.keys,
        FileKind::Segment,
        &session.vault_id,
        &format!("local/key/{key}"),
        &sealed,
    )?;
    serde_json::from_slice(&opened.plaintext).map_err(|_| SyncError::Corrupt)
}

fn open_head(session: &Session, dev: &str, bytes: &[u8]) -> Result<HeadPayload, SyncError> {
    let opened = crypto::open_small(
        &session.keys,
        FileKind::Head,
        &session.vault_id,
        &layout::head_aad_name(dev),
        bytes,
    )?;
    let head = HeadPayload::decode(opened.flags, &opened.plaintext)?;
    if head.vault_id != session.vault_id || head.device.id != dev {
        return Err(SyncError::Corrupt);
    }
    Ok(head)
}

fn open_segment(session: &Session, dev: &str, entry: &SegmentEntry, bytes: &[u8]) -> Result<SegmentPayload, SyncError> {
    // head 里记着密文的 hash：对不上说明段被替换或损坏
    if segment_hash(bytes) != entry.hash {
        return Err(SyncError::Corrupt);
    }
    let opened = crypto::open_small(
        &session.keys,
        FileKind::Segment,
        &session.vault_id,
        &layout::segment_aad_name(dev, entry.w),
        bytes,
    )?;
    let seg = SegmentPayload::decode(opened.flags, &opened.plaintext)?;
    if seg.device != dev || seg.w != entry.w {
        return Err(SyncError::Corrupt);
    }
    Ok(seg)
}

/// 读取并解密一台设备的 head；不存在返回 `None`
pub(super) async fn fetch_head<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    dev: &str,
) -> Result<Option<HeadPayload>, SyncError> {
    let path = layout::head_path(cx.vault_id(), dev);
    let Some(bytes) = cx.remote.get(&path).await? else {
        return Ok(None);
    };
    let session = Arc::clone(&cx.session);
    let dev = dev.to_string();
    blocking(move || open_head(&session, &dev, &bytes)).await?.map(Some)
}

/// 读取并解密一个日志段；不存在返回 `None`
pub(super) async fn fetch_segment<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    dev: &str,
    entry: &SegmentEntry,
) -> Result<Option<SegmentPayload>, SyncError> {
    let path = layout::segment_path(cx.vault_id(), dev, entry.w);
    let Some(bytes) = cx.remote.get(&path).await? else {
        return Ok(None);
    };
    let session = Arc::clone(&cx.session);
    let dev = dev.to_string();
    let entry = entry.clone();
    blocking(move || open_segment(&session, &dev, &entry, &bytes))
        .await?
        .map(Some)
}

/// GET vault.json 并确认它仍是本会话的 vault（每轮最多一次）：不存在或换了 ⇒ `VaultReset`
pub(super) async fn confirm_vault<R: Remote, L: LocalStore + ?Sized>(cx: &mut Cx<'_, R, L>) -> Result<(), SyncError> {
    if cx.vault_confirmed {
        return Ok(());
    }
    let bytes = cx.remote.get(VAULT_FILE).await?.ok_or(SyncError::VaultReset)?;
    match VaultHeader::parse(&bytes) {
        Ok(header) if header.vault_id == cx.vault_id() => {
            cx.vault_confirmed = true;
            Ok(())
        }
        Err(SyncError::FormatTooNew) => Err(SyncError::FormatTooNew),
        // 换了 vault，或被改成了无法解析的内容：都无法确认身份，不能再写入
        _ => Err(SyncError::VaultReset),
    }
}

/// 合并一台设备的若干段，游标与合并在同一个事务里提交
pub(super) async fn merge_into<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    author: &str,
    segments: Vec<(u64, Vec<Record>)>,
    cursor: Option<(u64, Option<String>)>,
) -> Result<MergeStats, SyncError> {
    let session = Arc::clone(&cx.session);
    let own = cx.own.clone();
    let author = author.to_string();
    let stats = cx
        .db(move |r| {
            let seal = |k: &str, v: &Value| seal_key_value(&session, k, v);
            let cursor = cursor.as_ref().map(|(hi, sig)| (*hi, sig.as_deref()));
            r.merge_device(&own, &author, &segments, cursor, &seal)
        })
        .await?;
    if cx.engine.hlc().observe(stats.max_t) {
        log::warn!("sync: a peer clock runs more than a day ahead of this device");
    }
    cx.report.pulled = cx.report.pulled.saturating_add(stats.won);
    Ok(stats)
}

/// 第一步：列目录、复核 vault、核对本机 head、读取变化的远端 head 与新段
pub(super) async fn pull<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    state: &mut CycleState,
) -> Result<(), SyncError> {
    let entries = cx.remote.list("").await?.ok_or(SyncError::VaultReset)?;
    state.dirs.mark("");
    let vault = entries
        .iter()
        .find(|e| !e.is_dir && e.name == VAULT_FILE)
        .ok_or(SyncError::VaultReset)?;
    check_vault(cx, vault).await?;

    let vault_id = cx.vault_id().to_string();
    let heads: Vec<(String, Entry)> = entries
        .iter()
        .filter(|e| !e.is_dir)
        .filter_map(|e| layout::parse_head_name(&vault_id, &e.name).map(|dev| (dev.to_string(), e.clone())))
        .collect();

    // 本机 head：每个进程只在首轮读一次；之后它的签名变了（别的机器在写我们的 head）才重新核对
    let own_entry = heads.iter().find(|(d, _)| *d == cx.own).map(|(_, e)| e.clone());
    if state.own_verified
        && let Some(entry) = &own_entry
    {
        let listed = sig_of(entry);
        let stored = cx.db(|r| r.meta(replica::META_OWN_SIG)).await?;
        match (stored, listed) {
            (Some(stored), Some(listed)) if stored != listed => state.own_verified = false,
            (None, Some(listed)) => {
                cx.db(move |r| r.set_meta(replica::META_OWN_SIG, &listed)).await?;
            }
            _ => {}
        }
    }
    if !state.own_verified {
        verify_own_head(cx, state, own_entry.as_ref()).await?;
    }

    let mut present = Vec::new();
    for (dev, entry) in &heads {
        if *dev == cx.own {
            continue;
        }
        present.push(dev.clone());
        match read_device(cx, dev, entry).await {
            Ok(()) => {}
            Err(SyncError::Corrupt | SyncError::FormatTooNew) => {
                // 先确认 vault 没有被换掉；没换就只跳过这台设备，记下签名，head 变化前不再重试
                confirm_vault(cx).await?;
                cx.report.corrupt_devices += 1;
                log::warn!("sync: skipped a device whose data could not be read");
                let dev = dev.clone();
                let sig = sig_of(entry);
                cx.db(move |r| r.set_cursor_sig(&dev, sig.as_deref())).await?;
            }
            Err(e) => return Err(e),
        }
    }
    let own_listed = heads.iter().any(|(d, _)| *d == cx.own);
    cx.own_head_listed = own_listed;
    cx.report.device_count = u32::try_from(heads.len() + usize::from(!own_listed)).unwrap_or(u32::MAX);
    cx.db(move |r| r.set_present(&present)).await?;
    Ok(())
}

/// vault.json 的签名变了（或服务器不提供签名）才 GET 并确认
async fn check_vault<R: Remote, L: LocalStore + ?Sized>(cx: &mut Cx<'_, R, L>, entry: &Entry) -> Result<(), SyncError> {
    let sig = sig_of(entry);
    let stored = cx.db(|r| r.meta(replica::META_VAULT_SIG)).await?;
    if sig.is_some() && stored == sig {
        return Ok(());
    }
    confirm_vault(cx).await?;
    if let Some(sig) = sig {
        cx.db(move |r| r.set_meta(replica::META_VAULT_SIG, &sig)).await?;
    }
    Ok(())
}

/// 读取一台远端设备变化的 head 与新段
async fn read_device<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    dev: &str,
    entry: &Entry,
) -> Result<(), SyncError> {
    let sig = sig_of(entry);
    let owned = dev.to_string();
    let cursor = cx.db(move |r| r.cursor(&owned)).await?;
    if sig.is_some() && cursor.as_ref().and_then(|c| c.sig.clone()) == sig {
        return Ok(());
    }
    let mut read_hi = cursor.as_ref().map_or(0, |c| c.read_hi);
    // 上次读到的段清单：用来发现「已读过的序号区间里出现了被替换或新冒出来的段」
    let mut seen: Vec<SegmentEntry> = cursor.map(|c| c.segments).unwrap_or_default();
    let mut retried = false;
    loop {
        let Some(head) = fetch_head(cx, dev).await? else {
            // 列目录之后被删除：下一轮的列表里就不会再有它
            return Ok(());
        };
        let (owned, info, updated, segments) = (
            dev.to_string(),
            head.device.clone(),
            head.updated_ms,
            head.segments.clone(),
        );
        cx.db(move |r| r.note_head(&owned, &info, updated, &segments)).await?;
        if head.max_hi() < read_hi {
            // 序号倒退：先确认 vault 没有被换掉，再把这台设备整个重读一遍（合并是幂等的）
            confirm_vault(cx).await?;
            read_hi = 0;
            let owned = dev.to_string();
            cx.db(move |r| r.reset_cursor(&owned)).await?;
        }
        // 新序号的段；以及序号已读过、但同一编号换了内容（hash 变了），或冒出一个普通推送形态（lo = hi）的新编号——
        // 两台机器共用一份被复制的数据目录时会写出同样的编号与序号，只按序号读会漏掉后写的那份
        let mut todo: Vec<SegmentEntry> = head
            .segments
            .iter()
            .filter(|s| {
                s.hi > read_hi
                    || (!seen.is_empty()
                        && match seen.iter().find(|p| p.w == s.w) {
                            Some(prev) => prev.hash != s.hash,
                            None => s.lo == s.hi,
                        })
            })
            .cloned()
            .collect();
        todo.sort_by_key(|s| (s.hi, s.w));
        seen.clone_from(&head.segments);
        if todo.is_empty() {
            let owned = dev.to_string();
            let sig = sig.clone();
            cx.db(move |r| r.set_cursor_sig(&owned, sig.as_deref())).await?;
            return Ok(());
        }
        // 同一个 hi 的段（压缩产物的各部分）一起提交，游标才不会越过没读完的部分
        let mut missing = false;
        let mut i = 0;
        while i < todo.len() {
            let hi = todo[i].hi;
            let group: Vec<SegmentEntry> = todo[i..].iter().take_while(|s| s.hi == hi).cloned().collect();
            i += group.len();
            let mut batch = Vec::with_capacity(group.len());
            for seg in &group {
                match fetch_segment(cx, dev, seg).await? {
                    Some(payload) => batch.push((hi, payload.records)),
                    None => {
                        missing = true;
                        break;
                    }
                }
            }
            if missing {
                break;
            }
            // 最后一组提交时记下 head 签名：之后 head 不变就不再读取
            let cursor_sig = if i >= todo.len() { sig.clone() } else { None };
            // 重读的旧段不让游标后退
            read_hi = read_hi.max(hi);
            merge_into(cx, dev, batch, Some((read_hi, cursor_sig))).await?;
        }
        if !missing {
            return Ok(());
        }
        // 段 404：作者可能刚压缩过，重新取 head 再试一次
        if retried {
            return Err(SyncError::Corrupt);
        }
        retried = true;
    }
}

/// 进程首轮核对本机 head：补做进行中的写入、发现丢失本机状态后的旧日志、发现被复制的数据目录
async fn verify_own_head<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    state: &mut CycleState,
    entry: Option<&Entry>,
) -> Result<(), SyncError> {
    let own = cx.own.clone();
    let head = match entry {
        Some(_) => match fetch_head(cx, &own).await {
            Ok(head) => head.map(Ok),
            Err(e @ (SyncError::Corrupt | SyncError::FormatTooNew)) => Some(Err(e)),
            Err(e) => return Err(e),
        },
        None => None,
    };
    let owned = own.clone();
    let (ledger, inflights, blank) = cx
        .db(move |r| Ok((r.ledger()?, r.inflights()?, r.is_blank(&owned)?)))
        .await?;
    let sig = entry.and_then(sig_of);
    match head {
        None => {
            if !ledger.is_empty() {
                // 本机 head 意外消失：先确认 vault 没有被清除；没有就保留账本，下次推送时重写 head
                confirm_vault(cx).await?;
            }
            let own = own.clone();
            cx.db(move |r| r.reconcile_own_head(&own, &ledger, None)).await?;
        }
        Some(Ok(head)) => {
            let known = |e: &SegmentEntry| {
                ledger.iter().any(|l| l.w == e.w && l.hash == e.hash)
                    || inflights.iter().any(|f| f.entry.w == e.w && f.entry.hash == e.hash)
            };
            let unknown = head.segments.iter().any(|e| !known(e));
            let max_w = head
                .segments
                .iter()
                .map(|e| e.w)
                .chain(inflights.iter().map(|f| f.entry.w))
                .max()
                .unwrap_or(0);
            let max_hi = head
                .segments
                .iter()
                .map(|e| e.hi)
                .chain(inflights.iter().map(|f| f.entry.hi))
                .max()
                .unwrap_or(0);
            if !unknown {
                let own = own.clone();
                let listed = head.segments.clone();
                cx.db(move |r| r.reconcile_own_head(&own, &listed, sig.as_deref()))
                    .await?;
            } else if blank {
                adopt_own_log(cx, &head, sig).await?;
            } else {
                confirm_vault(cx).await?;
                become_new_device(cx, state, &ledger, &head.segments).await?;
            }
            // 编号与序号越过 head 上已有的，永不复用
            // 每个进程第一次核对后，写入编号再随机向前跳一段：被复制的数据目录在两台机器上同时写入时，
            // 几乎不可能写出同名的段文件（同名会互相覆盖，读者也分不清）
            let jitter = blocking(crypto::random_id)
                .await??
                .get(..4)
                .and_then(|h| u64::from_str_radix(h, 16).ok())
                .unwrap_or(0);
            cx.db(move |r| r.advance_allocation(max_w + 1 + jitter, max_hi + 1))
                .await?;
        }
        Some(Err(_)) => {
            // 读不懂自己的 head（被别的机器以其他方式写过）：不再往这个 id 下写
            confirm_vault(cx).await?;
            become_new_device(cx, state, &ledger, &[]).await?;
        }
    }
    state.own_verified = true;
    Ok(())
}

/// 本机副本为空、而服务器上有本机 id 的旧日志（断开后重连、数据目录丢失）：把它当作本机作者的旧日志读入，
/// 之后继续追加。记录归属本机，但标为未应用（本机现在的数据可能已不同）
async fn adopt_own_log<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    head: &HeadPayload,
    sig: Option<String>,
) -> Result<(), SyncError> {
    let own = cx.own.clone();
    let mut kept = Vec::with_capacity(head.segments.len());
    for seg in &head.segments {
        match fetch_segment(cx, &own, seg).await {
            Ok(Some(payload)) => {
                merge_into(cx, &own, vec![(seg.hi, payload.records)], None).await?;
                kept.push(seg.clone());
            }
            // 丢失或损坏的段不再列进 head，免得读者反复重试
            Ok(None) | Err(SyncError::Corrupt | SyncError::FormatTooNew) => {
                log::warn!("sync: an old segment of this device could not be read");
            }
            Err(e) => return Err(e),
        }
    }
    cx.db(move |r| {
        r.reconcile_own_head(&own, &kept, sig.as_deref())?;
        // 本机向这份数据发布过：剪贴板与识别历史之后的加入不算首次，离开期间的删除与清空照常作用于本机
        for d in [Domain::Clip, Domain::Ocr] {
            r.del_meta(&replica::meta_first_join_key(&d))?;
        }
        Ok(())
    })
    .await?;
    Ok(())
}

/// 克隆检测命中：数据目录被复制到了另一台机器，两边都在用同一个 device_id。本机换新 id：
/// 待上传的记录（以及已发布但所在的段已被另一台机器覆盖的记录）以新 id、新 t 重新盖章；旧 id 作为远端设备从头读
async fn become_new_device<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    state: &mut CycleState,
    ledger: &[SegmentEntry],
    listed: &[SegmentEntry],
) -> Result<(), SyncError> {
    let new_id = blocking(crypto::random_id).await??;
    let old = cx.own.clone();
    // 账本里有、但 head 上已不是我们写的那份的段：其中的记录在服务器上丢了，需要重新发布
    let lost: Vec<(u64, u64)> = ledger
        .iter()
        .filter(|l| !listed.iter().any(|e| e.w == l.w && e.hash == l.hash))
        .map(|l| (l.lo, l.hi))
        .collect();
    let clock = cx.engine.hlc();
    let new = new_id.clone();
    cx.db(move |r| r.rebase_identity(&old, &new, &lost, &mut || clock.now()))
        .await?;
    cx.engine.set_device_id(&new_id);
    cx.own = new_id.clone();
    state.announce_id = Some(new_id);
    log::warn!("sync: this data directory appears to be copied from another device; continuing under a new device id");
    Ok(())
}

/// 重新取回一条记录的正文（已应用后正文被清空、之后又需要在本机重新出现时）：
/// 在作者当前 head 里找覆盖它所在序号的段，GET 并解密后取出同 t 的那条
pub(super) async fn refetch<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    cache: &mut HashMap<(String, u64), Option<Vec<Record>>>,
    row: &Row,
) -> Result<Option<Record>, SyncError> {
    let Some(src_hi) = row.src_hi else {
        return Ok(None);
    };
    let dev = row.dev.clone();
    let is_own = dev == cx.own;
    let mut refreshed = false;
    loop {
        let owned = dev.clone();
        let segments = cx
            .db(move |r| {
                if is_own {
                    r.ledger()
                } else {
                    Ok(r.cursor(&owned)?.map(|c| c.segments).unwrap_or_default())
                }
            })
            .await?;
        let mut any_missing = false;
        for seg in segments.iter().filter(|s| s.lo <= src_hi && src_hi <= s.hi) {
            let key = (dev.clone(), seg.w);
            if !cache.contains_key(&key) {
                let records = match fetch_segment(cx, &dev, seg).await {
                    Ok(Some(payload)) => Some(payload.records),
                    Ok(None) => {
                        any_missing = true;
                        None
                    }
                    Err(SyncError::Corrupt | SyncError::FormatTooNew) => None,
                    Err(e) => return Err(e),
                };
                cache.insert(key.clone(), records);
            }
            if let Some(Some(records)) = cache.get(&key)
                && let Some(found) = records.iter().find(|r| r.d == row.d && r.k == row.k && r.t == row.t)
            {
                return Ok(Some(found.clone()));
            }
        }
        if !any_missing || is_own || refreshed {
            return Ok(None);
        }
        // 作者压缩过：取一次最新的 head 再找
        refreshed = true;
        match fetch_head(cx, &dev).await {
            Ok(Some(head)) => {
                let (owned, info, updated, segments) =
                    (dev.clone(), head.device.clone(), head.updated_ms, head.segments);
                cx.db(move |r| r.note_head(&owned, &info, updated, &segments)).await?;
            }
            Ok(None) | Err(SyncError::Corrupt | SyncError::FormatTooNew) => return Ok(None),
            Err(e) => return Err(e),
        }
    }
}
