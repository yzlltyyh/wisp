//! 连接（创建 / 加入 / 恢复）、各域的加入规则、「测试连接」与清除服务器数据。
//!
//! 是否创建由服务器状态当场决定：GET vault.json，不存在才创建；PUT 后立即重读，不是自己的就改为解锁赢家
//! （口令还在内存里）。创建方发布完本机数据后再确认一次 vault 没被别的设备覆盖，覆盖了同样改为加入赢家。
//!
//! 加入永不删除本机数据：
//! - 配置标量：副本没有该 key ⇒ 发布本机值（默认值用 `Hlc(0)`，非默认用 `Hlc(1)`）；副本的存活记录是底值、
//!   本机非默认 ⇒ 以 `Hlc(1)` 发布；否则以服务器为准。创建时非默认值以真实 t 发布；
//! - 集合元素、术语、Key：服务器有存活元素 ⇒ 以服务器为准（整体）；没有或只有墓碑 ⇒ 以真实 t 发布本机元素；
//! - 剪贴板 / 识别历史：并集，本机条目以条目自身的时间发布，副本里已有更新 t 的 key 不发布，
//!   早于删除 / 清空的条目因此不会复活。首次加入这份数据时，早于已有清空的本机独有条目留在本机、不发布；
//!   本机以前发布过（以同一 id 重连、重新打开内容开关）时，离开期间的删除与清空照常作用于本机。
//!   其他设备同步过、之后被删除或清空的条目，本机的同一条也随之删除。

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;

use super::apply::clip_hidden_by_clear;
use super::content::{beats, item_hlc};
use super::cx::{Cx, CycleState, Session, blocking};
use super::{
    ConnectOutcome, Connected, CycleOptions, Engine, LocalStore, ProbeReport, ProgressFn, RemoteChange, VaultSummary,
    apply, pull, push,
};
use crate::crypto::{self, Keys, MasterKey};
use crate::error::{LocalError, SyncError};
use crate::hlc::{Hlc, wall_now_ms};
use crate::layout::{self, VAULT_FILE};
use crate::model::{Domain, FORMAT_VERSION, PinValue, VaultHeader, keys, parse_hex32};
use crate::remote::{DirCache, Remote};
use crate::replica::{self, JournalEntry, JournalValue, Row};

/// 派生主密钥并校验口令（CPU 密集，在工作线程里）
async fn unlock_header(passphrase: &str, header: &VaultHeader) -> Result<MasterKey, SyncError> {
    let passphrase = passphrase.to_string();
    let header = header.clone();
    blocking(move || {
        let master = crypto::derive_master(&passphrase, &header.kdf)?;
        let keys = Keys::derive(&master, &header.vault_id)?;
        crypto::verify_check(&keys, &header.vault_id, &header.check)?;
        Ok(master)
    })
    .await?
}

/// 服务器上没有 vault：创建并立即重读。返回最终采用的 vault、主密钥，以及是否由本机创建
async fn create_vault<R: Remote>(
    remote: &R,
    dirs: &mut DirCache,
    passphrase: &str,
    forbidden: &[&str],
) -> Result<(VaultHeader, MasterKey, bool), SyncError> {
    crypto::check_passphrase(passphrase, forbidden).map_err(SyncError::PassphraseRule)?;
    let owned = passphrase.to_string();
    let (header, master) = blocking(move || {
        let vault_id = crypto::random_id()?;
        let kdf = crypto::new_kdf_params()?;
        let master = crypto::derive_master(&owned, &kdf)?;
        let keys = Keys::derive(&master, &vault_id)?;
        let check = crypto::make_check(&keys, &vault_id)?;
        let header = VaultHeader {
            wisp_sync: FORMAT_VERSION,
            vault_id,
            created_ms: i64::try_from(wall_now_ms()).unwrap_or(i64::MAX),
            kdf,
            check,
        };
        Ok::<_, SyncError>((header, master))
    })
    .await??;
    dirs.ensure(remote, "").await?;
    remote.put(VAULT_FILE, Bytes::from(header.to_json()?)).await?;
    // 服务器忽略条件写：同时创建时以重读到的为准
    let reread = remote
        .get(VAULT_FILE)
        .await?
        .ok_or(SyncError::Remote(crate::error::RemoteError::Protocol))?;
    let winner = VaultHeader::parse(&reread)?;
    if winner.vault_id == header.vault_id {
        return Ok((header, master, true));
    }
    let master = unlock_header(passphrase, &winner).await?;
    Ok((winner, master, false))
}

/// 读取服务器上的 vault（不存在为 `None`）
///
/// 先列目录再读：有的服务器（如 123 云盘）删除文件后的几秒内，GET 仍可能返回删掉的旧内容，而列目录是立即一致的。
/// 另一台设备刚清除服务器数据时，只靠 GET 会把已删除的 vault 当成现存的去加入
async fn read_vault<R: Remote>(remote: &R) -> Result<Option<VaultHeader>, SyncError> {
    let listed = remote
        .list("")
        .await?
        .is_some_and(|entries| entries.iter().any(|e| !e.is_dir && e.name == VAULT_FILE));
    if !listed {
        return Ok(None);
    }
    match remote.get(VAULT_FILE).await? {
        Some(bytes) => VaultHeader::parse(&bytes).map(Some),
        None => Ok(None),
    }
}

pub(super) async fn connect<R: Remote, L: LocalStore + ?Sized>(
    engine: &Engine,
    remote: &R,
    local: &Arc<L>,
    passphrase: &str,
    forbidden: &[&str],
    options: &CycleOptions,
    progress: &ProgressFn,
) -> Result<Connected, SyncError> {
    let mut state = engine.cycle.lock().await;
    let (mut header, mut master, mut created) = match read_vault(remote).await? {
        None => create_vault(remote, &mut state.dirs, passphrase, forbidden).await?,
        Some(header) => {
            let master = unlock_header(passphrase, &header).await?;
            (header, master, false)
        }
    };

    // 恢复：本机副本属于这个 vault 且连接曾经完成，只更新凭据并解锁
    let vid = header.vault_id.clone();
    let restorable = engine
        .db(move |r| {
            Ok(r.meta(replica::META_VAULT_ID)?.as_deref() == Some(vid.as_str())
                && r.meta(replica::META_JOINED)?.as_deref() == Some("1"))
        })
        .await?;
    if !created && restorable {
        engine.install_session(&header.vault_id, &master)?;
        state.own_verified = false;
        return Ok(Connected {
            outcome: ConnectOutcome::Restored,
            vault_id: header.vault_id,
            master_key: master,
        });
    }

    let mut switched = false;
    loop {
        let (vid, own) = (header.vault_id.clone(), engine.device().id);
        engine.db(move |r| r.reset_for_vault(&vid, &own)).await?;
        engine.install_session(&header.vault_id, &master)?;
        state.own_verified = false;
        let session = engine.session().ok_or(SyncError::Locked)?;
        let mut cx = Cx::new(engine, remote, local, options, progress, session, engine.device().id);
        let result = connect_pipeline(&mut cx, &mut state, created).await;
        let lost_race = match result {
            Err(SyncError::VaultReset) if created => true,
            Err(e) => {
                engine.lock();
                return Err(e);
            }
            Ok(()) if created => {
                // 发布期间 vault.json 被另一台同时创建的设备覆盖了：改为加入赢家
                match read_vault(remote).await? {
                    Some(h) => h.vault_id != header.vault_id,
                    None => true,
                }
            }
            Ok(()) => false,
        };
        if lost_race {
            if switched {
                engine.lock();
                return Err(SyncError::VaultReset);
            }
            switched = true;
            let Some(winner) = read_vault(remote).await? else {
                engine.lock();
                return Err(SyncError::VaultReset);
            };
            master = unlock_header(passphrase, &winner).await?;
            header = winner;
            created = false;
            continue;
        }
        engine.db(|r| r.set_meta(replica::META_JOINED, "1")).await?;
        return Ok(Connected {
            outcome: if created {
                ConnectOutcome::Created
            } else {
                ConnectOutcome::Joined
            },
            vault_id: header.vault_id,
            master_key: master,
        });
    }
}

/// 连接时的工作：读入全部远端日志 → 配置类域的加入 → 首次应用 → 推送配置类域。
/// 剪贴板与识别历史的加入、上传与下载由之后的同步循环在后台完成
async fn connect_pipeline<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    state: &mut CycleState,
    created: bool,
) -> Result<(), SyncError> {
    pull::pull(cx, state).await?;
    let config = [Domain::Cfg, Domain::Sty, Domain::Glo, Domain::Key];
    for d in &config {
        if cx.options.content.allows(d) {
            join_config(cx, d.clone(), created).await?;
        }
    }
    apply::apply_domains(cx, &config).await?;
    push::push(cx, state, &config).await?;
    push::ensure_head(cx, state).await?;
    cx.engine.persist_clock().await?;
    if cx.keychain_error {
        return Err(SyncError::Local(LocalError::Keychain));
    }
    Ok(())
}

/// 配置类域的加入（规则见模块说明）
pub(super) async fn join_config<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    d: Domain,
    created: bool,
) -> Result<(), SyncError> {
    let dd = d.clone();
    let entries = match cx.local_call(move |l| l.enumerate(dd)).await? {
        Ok(entries) => entries,
        Err(LocalError::Unavailable) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let dd = d.clone();
    let rows: HashMap<String, Row> = cx
        .db(move |r| Ok(r.rows(&dd)?.into_iter().map(|row| (row.k.clone(), row)).collect()))
        .await?;
    let clock = cx.engine.hlc();
    let mut journal = Vec::new();
    for e in entries {
        let existing = rows.get(&e.key);
        let element = e.element || matches!(d, Domain::Glo | Domain::Key);
        let t = if element {
            if existing.is_some_and(|r| !r.tomb) {
                // 服务器有存活元素：以服务器为准（整体）
                continue;
            }
            clock.now()
        } else if created {
            if e.is_default { Hlc::FLOOR_DEFAULT } else { clock.now() }
        } else {
            match existing {
                None => {
                    if e.is_default {
                        Hlc::FLOOR_DEFAULT
                    } else {
                        Hlc::FLOOR_VALUE
                    }
                }
                // 本机 id 下的旧底值（断开后以同一 id 重连、读回了自己的旧日志）：同一个 (t, 作者) 不会胜出，
                // 本机值不同时用紧随其后的底值发布（仍是底值，任何真实改动都胜过它）；值相同就不必重发
                Some(r) if !r.tomb && r.t.is_floor() && !e.is_default && r.dev == cx.own => {
                    if e.value.is_some() && r.json() == e.value {
                        continue;
                    }
                    Hlc(r.t.0 + 1).max(Hlc::FLOOR_VALUE)
                }
                Some(r) if !r.tomb && r.t.is_floor() && !e.is_default => Hlc::FLOOR_VALUE,
                Some(_) => continue,
            }
        };
        let value = match e.value {
            Some(v) => JournalValue::Set(v),
            None => JournalValue::Deferred,
        };
        journal.push(JournalEntry {
            d: d.clone(),
            k: e.key,
            t,
            value,
        });
    }
    let own = cx.own.clone();
    cx.db(move |r| {
        r.record_own(&own, &journal)?;
        r.set_meta(&replica::meta_join_key(&d), "done")
    })
    .await
}

/// 剪贴板 / 识别历史的加入：本机条目以条目自身的时间发布
pub(super) async fn join_content<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    d: Domain,
) -> Result<(), SyncError> {
    let dd = d.clone();
    let entries = match cx.local_call(move |l| l.enumerate(dd)).await? {
        Ok(entries) => entries,
        // 剪贴板库尚未打开：下一轮再加入（状态里的 joining 一直为真，不催促立即重跑）
        Err(LocalError::Unavailable) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let dd = d.clone();
    let (view, first) = cx
        .db(move |r| {
            let first = r.meta(&replica::meta_first_join_key(&dd))?.is_some();
            Ok((r.content_view(&dd)?, first))
        })
        .await?;
    let own = cx.own.clone();
    // 本机收藏状态（加入时也发布 `p`）。值是 `{"pin": bool, "name"?: string}` 对象，按 `PinValue` 解析：
    // 只认裸 bool 会把全部收藏当成未收藏，重新加入时被「清空（保留收藏）」删掉
    let local_pins: HashMap<String, bool> = entries
        .iter()
        .filter_map(|e| {
            let h = e.key.strip_prefix("p/")?;
            Some((h.to_string(), PinValue::parse(e.value.as_ref()?)?.pin))
        })
        .collect();
    let mut journal = Vec::new();
    let mut hidden_items: Vec<String> = Vec::new();
    for e in entries {
        let t = item_hlc(e.item_ms);
        let hidden = match (&d, keys::parse_clip(&e.key), keys::parse_ocr(&e.key)) {
            (Domain::Clip, Some(keys::ClipKey::Content(_)), _) => {
                let h = &e.key[2..];
                let pinned_now = local_pins.get(h).copied().unwrap_or(false)
                    || super::content::pinned(view.pins.get(h), &view) == Some(true);
                clip_hidden_by_clear(t, &own, pinned_now, &view)
            }
            (Domain::Clip, Some(keys::ClipKey::Pin(_)), _) => false,
            (Domain::Ocr, _, Some(keys::OcrKey::Item(_))) => {
                let probe = Row {
                    d: Domain::Ocr,
                    k: String::new(),
                    dev: own.clone(),
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
                !beats(&probe, view.clear_all.as_ref())
            }
            // 加入不发布清空，也不认识的 key 不发布
            _ => true,
        };
        if hidden {
            // 早于已有的删除 / 清空：按判定式它不存在，不发布（否则会在其他设备上复活）。
            // 首次加入这份数据时它是本机独有的历史，加入不删除本机数据，只是不发布；
            // 本机以前发布过（以同一 id 重连、重新打开内容开关）时，离开期间的删除与清空照常作用于本机：
            // 直接删掉本机这一条，而不是留给清空的墙钟比较（同一毫秒内的先后只有 t 分得清）
            if !first && (e.key.starts_with("c/") || e.key.starts_with("o/")) {
                hidden_items.push(e.key[2..].to_string());
            }
            continue;
        }
        let value = match (e.key.starts_with("p/"), e.value) {
            (true, Some(v)) => JournalValue::Set(v),
            (true, None) => continue,
            (false, _) => JournalValue::Deferred,
        };
        journal.push(JournalEntry {
            d: d.clone(),
            k: e.key,
            t,
            value,
        });
    }
    if !hidden_items.is_empty() {
        let change = if d == Domain::Clip {
            RemoteChange::ClipDelete {
                hashes: hidden_items.iter().filter_map(|h| parse_hex32(h)).collect(),
            }
        } else {
            RemoteChange::OcrDelete { ids: hidden_items }
        };
        let dd = d.clone();
        match cx.local_call(move |l| l.apply(dd, vec![change])).await? {
            Ok(_) => {}
            // 删不掉也不要紧：清空寄存器应用时仍会按墙钟清理
            Err(e) => log::debug!("sync: removing items hidden by an earlier clear failed: {e}"),
        }
    }
    // 首次加入：加入时已有的清空记为已在本机生效。它们早于本机加入，不应再按墙钟删除本机独有、没有寄存器的条目；
    // 之后的清空有新的 t，照常生效
    let clears: Vec<apply::Mark> = if first {
        [view.clear_all.as_ref(), view.clear_unpinned.as_ref()]
            .into_iter()
            .flatten()
            .map(apply::mark_of)
            .collect()
    } else {
        Vec::new()
    };
    // 本机还有的条目：内容开关关闭期间压缩移除了它们的本机记录时，重新发布
    let present: Vec<String> = journal.iter().map(|e| e.k.clone()).collect();
    let own = cx.own.clone();
    cx.db(move |r| r.finish_content_join(&own, &d, &journal, &present, &clears))
        .await
}

impl Engine {
    pub(super) fn install_session(&self, vault_id: &str, master: &MasterKey) -> Result<(), SyncError> {
        let keys = Keys::derive(master, vault_id)?;
        *self.session.lock() = Some(Arc::new(Session {
            vault_id: vault_id.to_string(),
            keys,
        }));
        Ok(())
    }
}

/// 「测试连接」：认证、确保同步根目录存在、写入并删除一个探测文件、读取服务器上已有的同步数据概况
pub(super) async fn probe<R: Remote>(remote: &R, local_vault_id: Option<&str>) -> Result<ProbeReport, SyncError> {
    let mut dirs = DirCache::new();
    dirs.ensure(remote, "").await?;
    let name = format!("probe-{}.tmp", blocking(crypto::random_id).await??);
    remote.put(&name, Bytes::from_static(b"wisp")).await?;
    remote.delete(&name).await?;
    // 以列目录判断 vault 是否存在（见 read_vault：删除后的短时间内 GET 可能仍返回旧内容）
    let entries = remote.list("").await?.unwrap_or_default();
    let Some(vault_entry) = entries.iter().find(|e| !e.is_dir && e.name == VAULT_FILE) else {
        return Ok(ProbeReport { vault: None });
    };
    let vault_modified = vault_entry.modified_ms;
    let Some(bytes) = remote.get(VAULT_FILE).await? else {
        return Ok(ProbeReport { vault: None });
    };
    let summary = match VaultHeader::parse(&bytes) {
        Ok(header) => {
            let heads: Vec<_> = entries
                .iter()
                .filter(|e| !e.is_dir && layout::parse_head_name(&header.vault_id, &e.name).is_some())
                .collect();
            VaultSummary {
                supported: true,
                device_count: u32::try_from(heads.len()).unwrap_or(u32::MAX),
                last_modified_ms: heads.iter().filter_map(|e| e.modified_ms).max().or(vault_modified),
                restorable: local_vault_id == Some(header.vault_id.as_str()),
            }
        }
        // 更新的格式或无法解析：报告为「不受支持」，界面引导升级或清除服务器数据
        Err(_) => VaultSummary {
            supported: false,
            device_count: 0,
            last_modified_ms: vault_modified,
            restorable: false,
        },
    };
    Ok(ProbeReport { vault: Some(summary) })
}

/// 等到列目录里看不到 vault.json 为止（最多约 10 秒）。
///
/// 有的服务器（如 123 云盘）DELETE 先返回成功、稍后才真正删除。清除之后用户常常紧接着重新连接（例如忘记同步密码时
/// 「清除并重新开始」）；如果那时 vault.json 还在，连接会把它当成「服务器已有数据」去要旧的同步密码。这只发生在用户
/// 主动清除的操作里，等待有上限，超时也不报错——之后的同步循环同样以列目录为准
async fn wait_vault_gone<R: Remote>(remote: &R) {
    const DELAYS_MS: [u64; 6] = [250, 500, 1_000, 2_000, 3_000, 3_000];
    for delay in DELAYS_MS {
        match remote.list("").await {
            Ok(Some(entries)) if entries.iter().any(|e| !e.is_dir && e.name == VAULT_FILE) => {}
            _ => return,
        }
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }
    log::debug!("sync: vault file still listed after reset");
}

/// 清除服务器上的同步数据：先删除 vault.json（所有设备下一轮立刻得知），再尽力删除 head 与数据目录
pub(super) async fn reset_remote<R: Remote>(remote: &R) -> Result<(), SyncError> {
    remote.delete(VAULT_FILE).await?;
    wait_vault_gone(remote).await;
    // 之后的删除都是尽力而为：新 vault 不会读到这些残留
    let entries = match remote.list("").await {
        Ok(Some(entries)) => entries,
        Ok(None) => return Ok(()),
        Err(e) => {
            log::debug!("sync: listing leftovers after reset failed: {}", e.detail());
            return Ok(());
        }
    };
    for e in entries {
        let target = if e.is_dir && layout::is_valid_id(&e.name) {
            Some(format!("{}/", e.name))
        } else if !e.is_dir && is_head_name(&e.name) {
            Some(e.name)
        } else {
            None
        };
        if let Some(path) = target
            && let Err(err) = remote.delete(&path).await
        {
            log::debug!("sync: deleting leftovers after reset failed: {}", err.detail());
        }
    }
    Ok(())
}

/// `<vault_id>.<device_id>.head`（任意 vault 的）
fn is_head_name(name: &str) -> bool {
    name.split_once('.')
        .is_some_and(|(vault, _)| layout::is_valid_id(vault) && layout::parse_head_name(vault, name).is_some())
}
