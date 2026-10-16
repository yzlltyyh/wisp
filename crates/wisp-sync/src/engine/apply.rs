//! 应用：把副本里全部「未应用」的胜出记录交给本机存储（不限本轮拉到的），本机确认后才标记已应用。
//!
//! - cfg / sty / glo / key：逐 key 交出 `Set` / `Delete`；
//! - clip / ocr：引擎按存在判定式逐条算出要新增、改置顶、删除的条目，以及对没有寄存器的本机条目生效的清空，
//!   适配器不需要理解寄存器。清空寄存器变化时重新评估全部条目；否则只评估有未应用记录的条目。
//!
//! 本机作者的待上传记录在本机就是当前值，不会被当作远端值写回。

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use super::content::{beats, clear_sig, clear_wall_ms, clip_alive, favorite, item_ms, ocr_alive, pin_value, pinned};
use super::cx::{Cx, blocking, remove_quietly};
use super::pull::{open_key_value, refetch};
use super::{ApplyOutcome, KeyApply, LocalStore, RemoteChange, SpooledBlob};
use crate::crypto;
use crate::error::{LocalError, SyncError};
use crate::hlc::Hlc;
use crate::layout;
use crate::model::{BlobRef, BlobRole, Domain, PinValue, Record, parse_hex32};
use crate::remote::Remote;
use crate::replica::{self, ContentView, JournalEntry, JournalValue, Row};

/// 标记用的键：`(域, key, t, 作者)`，只命中这个版本
pub(super) type Mark = (Domain, String, Hlc, String);

pub(super) fn mark_of(row: &Row) -> Mark {
    (row.d.clone(), row.k.clone(), row.t, row.dev.clone())
}

/// 本机存储的调用结果逐项展开；整体失败时全部按推迟处理
fn results_of<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    d: &Domain,
    n: usize,
    outcome: Result<ApplyOutcome, LocalError>,
) -> Vec<KeyApply> {
    match outcome {
        Ok(outcome) => {
            let mut results = outcome.results;
            // 适配器少报的项按推迟处理，下次再试
            results.resize(n, KeyApply::Deferred);
            results
        }
        Err(LocalError::Keychain) => {
            cx.keychain_error = true;
            vec![KeyApply::Deferred; n]
        }
        Err(LocalError::Unavailable) => vec![KeyApply::Deferred; n],
        Err(e) => {
            log::warn!("sync: applying {d} records failed: {e}");
            vec![KeyApply::Deferred; n]
        }
    }
}

/// 没有需要处理的记录、清空寄存器也和上次逐条评估时一样：本轮不必读取整个域。
/// 结论与读完整个域之后的提前返回相同，空闲的一轮只做几次按主键的查询
async fn content_idle<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    d: &Domain,
    eval_key: &'static str,
) -> Result<bool, SyncError> {
    let dd = d.clone();
    cx.db(move |r| {
        let sig = clear_sig(&r.clear_view(&dd)?);
        Ok(r.meta(eval_key)?.as_deref() == Some(sig.as_str()) && !r.has_dirty(&dd)?)
    })
    .await
}

/// 应用若干域（按调用方给的顺序）
pub(super) async fn apply_domains<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    domains: &[Domain],
) -> Result<(), SyncError> {
    for d in domains {
        if !cx.options.content.allows(d) {
            if d.is_content() {
                // 本机不应用这个域：其他设备条目的正文不留在 sync.db 里（重新打开后从作者的日志取回）
                let dd = d.clone();
                cx.db(move |r| r.drop_item_bodies(&dd)).await?;
            }
            continue;
        }
        if *d == Domain::Key && !cx.options.keychain {
            continue;
        }
        match d {
            Domain::Cfg | Domain::Sty | Domain::Glo | Domain::Key => apply_registers(cx, d.clone()).await?,
            Domain::Clip => apply_clip(cx).await?,
            Domain::Ocr => apply_ocr(cx).await?,
            Domain::Unknown(_) => {}
        }
        if cx.keychain_error && *d == Domain::Key {
            // 钥匙串被拒绝：不再继续读写钥匙串，记录留在「未应用」
            continue;
        }
    }
    Ok(())
}

async fn apply_registers<R: Remote, L: LocalStore + ?Sized>(cx: &mut Cx<'_, R, L>, d: Domain) -> Result<(), SyncError> {
    let dd = d.clone();
    let rows = cx.db(move |r| r.dirty_rows(&dd)).await?;
    if rows.is_empty() {
        return Ok(());
    }
    let own = cx.own.clone();
    let mut changes = Vec::new();
    let mut targets = Vec::new();
    let mut local_source = Vec::new();
    let mut missing = Vec::new();
    for row in rows {
        if row.pending {
            continue;
        }
        if row.tomb {
            changes.push(RemoteChange::Delete { key: row.k.clone() });
            targets.push(row);
            continue;
        }
        let value = if d == Domain::Key {
            row.v
                .as_deref()
                .and_then(|stored| open_key_value(&cx.session, &row.k, stored).ok())
        } else {
            row.json()
        };
        match value {
            Some(value) => {
                changes.push(RemoteChange::Set {
                    key: row.k.clone(),
                    value,
                });
                targets.push(row);
            }
            // 本机作者的记录（值只在本机）：本机就是来源
            None if row.dev == own => local_source.push(mark_of(&row)),
            None => missing.push(mark_of(&row)),
        }
    }
    if !local_source.is_empty() || !missing.is_empty() {
        cx.db(move |r| {
            r.mark_applied(&local_source, false, false)?;
            r.mark_missing(&missing)
        })
        .await?;
    }
    if changes.is_empty() {
        return Ok(());
    }
    let n = changes.len();
    let dd = d.clone();
    let outcome = cx.local_call(move |l| l.apply(dd, changes)).await?;
    let results = results_of(cx, &d, n, outcome);
    let mut applied = Vec::new();
    let mut evicted = Vec::new();
    for (row, result) in targets.iter().zip(results) {
        match result {
            KeyApply::Applied => applied.push(mark_of(row)),
            KeyApply::Evicted => evicted.push(mark_of(row)),
            KeyApply::Deferred => cx.report.deferred += 1,
        }
    }
    cx.report.applied = cx
        .report
        .applied
        .saturating_add(u32::try_from(applied.len()).unwrap_or(u32::MAX));
    cx.db(move |r| {
        r.mark_applied(&applied, false, false)?;
        r.mark_evicted(&evicted)
    })
    .await
}

/// 下载并解密一个 blob 到 spool 文件；不存在或解不开时返回 `None`（引用它的记录标为缺内容）
async fn download_blob<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    bref: &BlobRef,
) -> Result<Option<SpooledBlob>, SyncError> {
    // 远端给出的名字要先校验，才能拼进路径
    if !layout::is_valid_id(&bref.dev) || !layout::is_valid_blob_name(&bref.name) {
        return Ok(None);
    }
    let enc = cx.spool_file("dl");
    let plain = cx.spool_file("blob");
    let path = layout::blob_path(cx.vault_id(), &bref.dev, &bref.name);
    let found = match cx.remote.get_to_file(&path, &enc).await {
        Ok(found) => found,
        Err(e) => {
            remove_quietly(&enc);
            return Err(e.into());
        }
    };
    if !found {
        return Ok(None);
    }
    cx.down_budget = cx.down_budget.saturating_sub(bref.bytes);
    let session = Arc::clone(&cx.session);
    let name = layout::blob_aad_name(&bref.dev, &bref.name);
    let dest = plain.clone();
    let opened = blocking(move || {
        let out = crypto::open_blob_file(&session.keys, &session.vault_id, &name, &enc, &dest);
        remove_quietly(&enc);
        out
    })
    .await?;
    match opened {
        Ok(bytes) => Ok(Some(SpooledBlob {
            role: bref.role.clone(),
            path: plain,
            bytes,
        })),
        Err(_) => {
            remove_quietly(&plain);
            log::warn!("sync: a downloaded attachment could not be decrypted");
            Ok(None)
        }
    }
}

/// 本机条目需要重新出现、但副本里没有正文：从作者日志里取回
async fn body_of<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    cache: &mut HashMap<(String, u64), Option<Vec<Record>>>,
    row: &Row,
) -> Result<Option<(serde_json::Value, Vec<BlobRef>)>, SyncError> {
    if let Some(v) = row.json() {
        return Ok(Some((v, row.b.clone())));
    }
    Ok(refetch(cx, cache, row).await?.and_then(|rec| rec.v.map(|v| (v, rec.b))))
}

/// 一个待应用的条目
struct Upsert {
    key: String,
    hash: Option<[u8; 32]>,
    item: Row,
    pin: Option<Row>,
    /// 收藏寄存器的有效值（`None` = 没有寄存器，保持本机的收藏状态与名称）
    favorite: Option<PinValue>,
}

/// 逐条计算剪贴板变更
async fn apply_clip<R: Remote, L: LocalStore + ?Sized>(cx: &mut Cx<'_, R, L>) -> Result<(), SyncError> {
    if content_idle(cx, &Domain::Clip, replica::META_CLIP_EVAL).await? {
        return Ok(());
    }
    let view = cx.db(|r| r.content_view(&Domain::Clip)).await?;
    let sig = clear_sig(&view);
    let stored = cx.db(|r| r.meta(replica::META_CLIP_EVAL)).await?;
    let eval_all = stored.as_deref() != Some(sig.as_str());
    let hashes: BTreeSet<String> = if eval_all {
        view.items.keys().chain(view.pins.keys()).cloned().collect()
    } else {
        view.items
            .iter()
            .chain(view.pins.iter())
            .filter(|(_, r)| r.is_dirty())
            .map(|(h, _)| h.clone())
            .collect()
    };
    let own = cx.own.clone();
    let clears: Vec<Row> = [view.clear_all.clone(), view.clear_unpinned.clone()]
        .into_iter()
        .flatten()
        .filter(|r| r.is_dirty())
        .collect();
    if hashes.is_empty() && clears.is_empty() {
        if eval_all {
            cx.db(move |r| r.set_meta(replica::META_CLIP_EVAL, &sig)).await?;
        }
        return Ok(());
    }

    let mut marks: Vec<Mark> = Vec::new();
    let mut republish: Vec<String> = Vec::new();
    let mut delete_rows: Vec<(Row, Option<Row>)> = Vec::new();
    // 改置顶：(hash, 置顶寄存器, 内容寄存器)
    let mut pin_changes: Vec<([u8; 32], Row, Option<Row>)> = Vec::new();
    let mut upserts: Vec<Upsert> = Vec::new();
    for h in &hashes {
        let Some(hash) = parse_hex32(h) else { continue };
        let c = view.items.get(h);
        let p = view.pins.get(h);
        let dirty_pin = p.filter(|p| p.is_dirty());
        match c {
            None => {
                // 只有置顶寄存器（本机可能有该条目但没有同步内容）：照常改置顶
                if let Some(p) = dirty_pin {
                    if p.dev != own && !p.tomb {
                        pin_changes.push((hash, p.clone(), None));
                    } else {
                        marks.push(mark_of(p));
                    }
                }
            }
            // 不存在的条目总是交给本机删除（幂等）：「本机已不存在」的标记可能是保守估计，不能据此省掉删除
            Some(c) if !clip_alive(c, p, &view) => delete_rows.push((c.clone(), p.cloned())),
            Some(c) => {
                // 条目只因为本机的置顶寄存器（置顶，或清空之后的取消置顶）才躲过「清空（保留置顶）」：清空的那台设备可能已删掉它，
                // 原作者也可能已在压缩时丢掉了正文。由置顶的这台设备重新发布一次内容，正文就总能拿到
                if p.is_some_and(|p| p.dev == own && !p.tomb)
                    && c.dev != own
                    && !beats(c, view.clear_unpinned.as_ref())
                    && clip_alive(c, p, &view)
                {
                    republish.push(format!("c/{h}"));
                }
                let materialized = c.is_applied() && !c.gone;
                // 「清空全部」让更早的收藏失效：寄存器没变，但本机条目的收藏状态（与名称）要跟着变
                let neutralized = eval_all && p.is_some_and(|p| !p.tomb && pin_value(p) != favorite(Some(p), &view));
                if materialized || c.evicted_t == Some(c.t) || c.missing_t == Some(c.t) {
                    if let Some(p) = dirty_pin.or(p.filter(|_| neutralized)) {
                        if materialized && !p.tomb {
                            pin_changes.push((hash, p.clone(), Some(c.clone())));
                        } else if c.evicted_t == Some(c.t) && pinned(Some(p), &view) == Some(true) {
                            // 本机按自己的规则（保留策略等）拒收过这一版本；置顶的条目不受这些规则限制，带着置顶状态重新导入
                            upserts.push(Upsert {
                                key: h.clone(),
                                hash: Some(hash),
                                item: c.clone(),
                                pin: Some(p.clone()),
                                favorite: favorite(Some(p), &view),
                            });
                        } else {
                            marks.push(mark_of(p));
                        }
                    }
                } else {
                    // 未应用的条目（包括断开重连后读回的本机旧日志里的：本机现在不一定还有它）
                    upserts.push(Upsert {
                        key: h.clone(),
                        hash: Some(hash),
                        item: c.clone(),
                        pin: p.cloned(),
                        favorite: favorite(p, &view),
                    });
                }
            }
        }
    }

    let mut deferred_any = false;
    if !marks.is_empty() {
        cx.db(move |r| r.mark_applied(&marks, false, false)).await?;
    }
    if !republish.is_empty() {
        let clock = cx.engine.hlc();
        let own = own.clone();
        cx.db(move |r| {
            let entries: Vec<JournalEntry> = republish
                .into_iter()
                .map(|k| JournalEntry {
                    d: Domain::Clip,
                    k,
                    t: clock.now(),
                    value: JournalValue::Deferred,
                })
                .collect();
            r.record_own(&own, &entries).map(|_| ())
        })
        .await?;
    }

    // 删除、改置顶与对无寄存器条目的清空放在一次调用里
    let mut changes = Vec::new();
    let delete_hashes: Vec<[u8; 32]> = delete_rows
        .iter()
        .filter_map(|(c, _)| c.k.strip_prefix("c/").and_then(parse_hex32))
        .collect();
    if !delete_hashes.is_empty() {
        changes.push(RemoteChange::ClipDelete { hashes: delete_hashes });
    }
    for (hash, p, _) in &pin_changes {
        let fav = favorite(Some(p), &view).unwrap_or_default();
        changes.push(RemoteChange::ClipPin {
            hash: *hash,
            pinned: fav.pin,
            name: fav.name,
        });
    }
    let remote_clears: Vec<Row> = clears.iter().filter(|r| r.dev != own && !r.tomb).cloned().collect();
    let own_clears: Vec<Mark> = clears.iter().filter(|r| r.dev == own || r.tomb).map(mark_of).collect();
    if !remote_clears.is_empty() {
        let exclude: Vec<[u8; 32]> = view.items.keys().filter_map(|h| parse_hex32(h)).collect();
        for clear in &remote_clears {
            changes.push(RemoteChange::ClipDeleteUnsyncedBefore {
                wall_ms: clear_wall_ms(clear),
                keep_pinned: clear.k == crate::model::keys::CLIP_CLEAR_UNPINNED,
                exclude: exclude.clone(),
            });
        }
    }
    if !changes.is_empty() {
        let n = changes.len();
        let outcome = cx.local_call(move |l| l.apply(Domain::Clip, changes)).await?;
        let results = results_of(cx, &Domain::Clip, n, outcome);
        let mut it = results.into_iter();
        let mut gone = Vec::new();
        let mut done = Vec::new();
        if !delete_rows.is_empty() {
            match it.next() {
                Some(KeyApply::Applied | KeyApply::Evicted) => {
                    for (c, p) in &delete_rows {
                        gone.push(mark_of(c));
                        done.extend(p.as_ref().filter(|p| p.is_dirty()).map(mark_of));
                    }
                }
                _ => deferred_any = true,
            }
        }
        for (hash, p, c) in &pin_changes {
            match (it.next(), c) {
                // 置顶一条本机已没有的条目（被本机的保留策略清理过）：置顶后它应当存在，带着置顶状态重新导入
                (Some(KeyApply::Evicted), Some(c)) if pinned(Some(p), &view) == Some(true) => {
                    upserts.push(Upsert {
                        key: c.k[2..].to_string(),
                        hash: Some(*hash),
                        item: c.clone(),
                        pin: Some(p.clone()),
                        favorite: favorite(Some(p), &view),
                    });
                }
                (Some(KeyApply::Applied | KeyApply::Evicted), _) => done.push(mark_of(p)),
                _ => deferred_any = true,
            }
        }
        for clear in &remote_clears {
            match it.next() {
                Some(KeyApply::Applied | KeyApply::Evicted) => done.push(mark_of(clear)),
                _ => deferred_any = true,
            }
        }
        cx.report.applied = cx
            .report
            .applied
            .saturating_add(u32::try_from(gone.len() + done.len()).unwrap_or(u32::MAX));
        cx.db(move |r| {
            r.mark_applied(&gone, true, true)?;
            r.mark_applied(&done, false, false)
        })
        .await?;
    }
    if !own_clears.is_empty() {
        cx.db(move |r| r.mark_applied(&own_clears, false, false)).await?;
    }

    if !run_upserts(cx, Domain::Clip, upserts).await? {
        deferred_any = true;
    }
    if !deferred_any {
        cx.db(move |r| r.set_meta(replica::META_CLIP_EVAL, &sig)).await?;
    }
    Ok(())
}

/// 逐条计算识别历史变更
async fn apply_ocr<R: Remote, L: LocalStore + ?Sized>(cx: &mut Cx<'_, R, L>) -> Result<(), SyncError> {
    if content_idle(cx, &Domain::Ocr, replica::META_OCR_EVAL).await? {
        return Ok(());
    }
    let view = cx.db(|r| r.content_view(&Domain::Ocr)).await?;
    let sig = clear_sig(&view);
    let stored = cx.db(|r| r.meta(replica::META_OCR_EVAL)).await?;
    let eval_all = stored.as_deref() != Some(sig.as_str());
    let own = cx.own.clone();
    let ids: Vec<String> = view
        .items
        .iter()
        .filter(|(_, r)| eval_all || r.is_dirty())
        .map(|(id, _)| id.clone())
        .collect();
    let clear = view.clear_all.clone().filter(|r| r.is_dirty());
    if ids.is_empty() && clear.is_none() {
        if eval_all {
            cx.db(move |r| r.set_meta(replica::META_OCR_EVAL, &sig)).await?;
        }
        return Ok(());
    }
    let mut deletes: Vec<Row> = Vec::new();
    let mut upserts = Vec::new();
    for id in ids {
        let Some(o) = view.items.get(&id) else { continue };
        if !ocr_alive(o, &view) {
            deletes.push(o.clone());
        } else if (o.is_applied() && !o.gone) || o.evicted_t == Some(o.t) || o.missing_t == Some(o.t) {
            // 已在本机，或这一版本被拒绝 / 缺内容
        } else {
            upserts.push(Upsert {
                key: id,
                hash: None,
                item: o.clone(),
                pin: None,
                favorite: None,
            });
        }
    }
    let mut deferred_any = false;
    let mut changes = Vec::new();
    if !deletes.is_empty() {
        changes.push(RemoteChange::OcrDelete {
            ids: deletes
                .iter()
                .filter_map(|o| o.k.strip_prefix("o/").map(str::to_string))
                .collect(),
        });
    }
    let remote_clear = clear.as_ref().filter(|c| c.dev != own && !c.tomb).cloned();
    if let Some(c) = &remote_clear {
        changes.push(RemoteChange::OcrDeleteUnsyncedBefore {
            wall_ms: clear_wall_ms(c),
            exclude: view.items.keys().cloned().collect(),
        });
    }
    let mut done = Vec::new();
    if let Some(c) = clear.as_ref().filter(|c| c.dev == own || c.tomb) {
        done.push(mark_of(c));
    }
    if !changes.is_empty() {
        let n = changes.len();
        let outcome = cx.local_call(move |l| l.apply(Domain::Ocr, changes)).await?;
        let results = results_of(cx, &Domain::Ocr, n, outcome);
        let mut it = results.into_iter();
        let mut gone = Vec::new();
        if !deletes.is_empty() {
            match it.next() {
                Some(KeyApply::Applied | KeyApply::Evicted) => gone.extend(deletes.iter().map(mark_of)),
                _ => deferred_any = true,
            }
        }
        if let Some(c) = &remote_clear {
            match it.next() {
                Some(KeyApply::Applied | KeyApply::Evicted) => done.push(mark_of(c)),
                _ => deferred_any = true,
            }
        }
        cx.db(move |r| r.mark_applied(&gone, true, true)).await?;
    }
    if !done.is_empty() {
        cx.db(move |r| r.mark_applied(&done, false, false)).await?;
    }
    if !run_upserts(cx, Domain::Ocr, upserts).await? {
        deferred_any = true;
    }
    if !deferred_any {
        cx.db(move |r| r.set_meta(replica::META_OCR_EVAL, &sig)).await?;
    }
    Ok(())
}

/// 每次交给本机存储的新增条目数上限（适配器在一个事务里写入）
const UPSERTS_PER_CALL: usize = 50;

/// 新增 / 恢复条目：新到旧，分批下载附件并交给本机存储。返回 `false` 表示有条目被推迟或本轮额度用完
async fn run_upserts<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    d: Domain,
    mut upserts: Vec<Upsert>,
) -> Result<bool, SyncError> {
    if upserts.is_empty() {
        return Ok(true);
    }
    // 本机存储眼下写不进去（剪贴板库还没打开或打不开）：不下载附件——下载了也只能丢掉，
    // 记录留在「未应用」，也不要求立即再来一轮
    let dd = d.clone();
    if !cx.local_call(move |l| Ok(l.available(&dd))).await?? {
        return Ok(false);
    }
    upserts.sort_by(|a, b| item_ms(&b.item).cmp(&item_ms(&a.item)).then(b.item.t.cmp(&a.item.t)));
    let total = upserts.len();
    cx.add_work(u32::try_from(total).unwrap_or(u32::MAX));
    let mut cache = HashMap::new();
    let mut complete = true;
    // 本轮的下载额度用完（只有这种情况要求再来一轮）
    let mut more = false;
    // 本机存储在本轮中途变得不可用：停下，不再下载
    let mut unavailable = false;
    let mut iter = upserts.into_iter().peekable();
    while iter.peek().is_some() {
        if cx.down_budget == 0 {
            more = true;
            complete = false;
            break;
        }
        let mut changes = Vec::new();
        let mut targets: Vec<(Row, Option<Row>)> = Vec::new();
        let mut spooled = Vec::new();
        let mut evicted = Vec::new();
        let mut missing = Vec::new();
        // 条目本身没能导入（缺内容或被拒绝）时，随它一起的置顶寄存器也算处理过，免得每轮重试
        let mut pins_done = Vec::new();
        let mut taken = 0u32;
        while changes.len() + evicted.len() + missing.len() < UPSERTS_PER_CALL {
            let Some(up) = iter.next() else { break };
            taken += 1;
            let dirty_pin = up.pin.as_ref().filter(|p| p.is_dirty()).map(mark_of);
            let Some((value, blob_refs)) = body_of(cx, &mut cache, &up.item).await? else {
                missing.push(mark_of(&up.item));
                pins_done.extend(dirty_pin);
                continue;
            };
            match collect_blobs(cx, &d, &blob_refs).await? {
                BlobFetch::Ready(blobs) => {
                    spooled.extend(blobs.iter().map(|b| b.path.clone()));
                    let change = match up.hash {
                        Some(hash) => RemoteChange::ClipUpsert {
                            hash,
                            value,
                            pinned: up.favorite.as_ref().map(|f| f.pin),
                            name: up.favorite.and_then(|f| f.name),
                            blobs,
                        },
                        None => RemoteChange::OcrUpsert {
                            id: up.key.clone(),
                            value,
                            blobs,
                        },
                    };
                    changes.push(change);
                    targets.push((up.item, up.pin));
                }
                BlobFetch::Rejected => {
                    evicted.push(mark_of(&up.item));
                    pins_done.extend(dirty_pin);
                }
                BlobFetch::Missing => {
                    missing.push(mark_of(&up.item));
                    pins_done.extend(dirty_pin);
                }
                BlobFetch::OverBudget => {
                    more = true;
                    complete = false;
                    break;
                }
            }
        }
        if !changes.is_empty() {
            let n = changes.len();
            let dd = d.clone();
            let outcome = cx.local_call(move |l| l.apply(dd, changes)).await?;
            // 适配器没有移走的 spool 文件由引擎删除
            blocking(move || spooled.iter().for_each(|p| remove_quietly(p))).await?;
            unavailable = matches!(outcome, Err(LocalError::Unavailable));
            let results = results_of(cx, &d, n, outcome);
            let mut applied = Vec::new();
            let mut pins = Vec::new();
            for ((item, pin), result) in targets.iter().zip(results) {
                match result {
                    KeyApply::Applied => {
                        applied.push(mark_of(item));
                        pins.extend(pin.as_ref().filter(|p| p.is_dirty()).map(mark_of));
                    }
                    KeyApply::Evicted => {
                        evicted.push(mark_of(item));
                        pins.extend(pin.as_ref().filter(|p| p.is_dirty()).map(mark_of));
                    }
                    KeyApply::Deferred => {
                        cx.report.deferred += 1;
                        complete = false;
                    }
                }
            }
            cx.report.applied = cx
                .report
                .applied
                .saturating_add(u32::try_from(applied.len()).unwrap_or(u32::MAX));
            cx.db(move |r| {
                r.mark_applied(&applied, true, false)?;
                r.mark_applied(&pins, false, false)
            })
            .await?;
        }
        if !evicted.is_empty() || !missing.is_empty() {
            cx.db(move |r| {
                r.mark_evicted(&evicted)?;
                r.mark_missing(&missing)?;
                r.mark_applied(&pins_done, false, false)
            })
            .await?;
        }
        cx.work_done(taken);
        if more || unavailable {
            break;
        }
    }
    if more && !unavailable {
        cx.report.again = true;
    }
    Ok(complete)
}

enum BlobFetch {
    Ready(Vec<SpooledBlob>),
    /// 本机规则不接收（图片开关关闭、超过单张上限）
    Rejected,
    /// 必需的附件不存在或解不开
    Missing,
    /// 本轮下载额度用完
    OverBudget,
}

/// 下载条目引用的附件：剪贴板原图必需；识别记录缩略图必需、原图可选
async fn collect_blobs<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    d: &Domain,
    refs: &[BlobRef],
) -> Result<BlobFetch, SyncError> {
    let mut out = Vec::new();
    let images_ok = |b: &BlobRef| cx.options.content.images && b.bytes <= cx.options.images_max_bytes;
    let mut plan: Vec<(BlobRef, bool)> = Vec::new();
    for b in refs {
        match (d, &b.role) {
            (Domain::Clip, BlobRole::Image) => {
                if !images_ok(b) {
                    return Ok(BlobFetch::Rejected);
                }
                plan.push((b.clone(), true));
            }
            (Domain::Ocr, BlobRole::Thumbnail) => plan.push((b.clone(), true)),
            (Domain::Ocr, BlobRole::Image) if images_ok(b) => plan.push((b.clone(), false)),
            // 本版本不认识的用途、或不接收的可选原图：跳过
            _ => {}
        }
    }
    let need: u64 = plan.iter().map(|(b, _)| b.bytes).sum();
    if need > cx.down_budget && need > 0 {
        return Ok(BlobFetch::OverBudget);
    }
    for (b, required) in plan {
        match download_blob(cx, &b).await? {
            Some(blob) => out.push(blob),
            None if required => {
                let paths: Vec<_> = out.iter().map(|b: &SpooledBlob| b.path.clone()).collect();
                blocking(move || paths.iter().for_each(|p| remove_quietly(p))).await?;
                return Ok(BlobFetch::Missing);
            }
            None => {}
        }
    }
    Ok(BlobFetch::Ready(out))
}

/// 加入时判断一条以条目自身时间发布的剪贴板记录是否会被已有的清空隐藏
pub(super) fn clip_hidden_by_clear(t: Hlc, own: &str, pinned_now: bool, view: &ContentView) -> bool {
    let probe = Row {
        d: Domain::Clip,
        k: String::new(),
        dev: own.to_string(),
        t,
        tomb: false,
        v: None,
        b: Vec::new(),
        aux: None,
        item_ms: None,
        src_hi: None,
        applied_t: None,
        evicted_t: None,
        missing_t: None,
        gone: false,
        pending: false,
        deferred: false,
    };
    if !beats(&probe, view.clear_all.as_ref()) {
        return true;
    }
    !pinned_now && !beats(&probe, view.clear_unpinned.as_ref())
}
