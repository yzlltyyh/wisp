//! 只读探测（实时同步）：列一次同步根目录，与上一轮成功同步时看到的各文件签名比较，判断别的设备有没有新改动。
//!
//! 不读写 sync.db、不改时钟与游标、不下载任何文件——探测间隔只有几秒，任何写入都会在本机日志里不停累积。
//! 比较对象是进程内的 [`Snapshot`]：每轮成功同步结束时由那一轮用的列表刷新，失败时清空（下一次探测按「有变化」处理，
//! 重跑完整的一轮）。

use std::collections::BTreeMap;

use super::Peek;
use super::cx::sig_of;
use crate::layout::{self, VAULT_FILE};
use crate::remote::Entry;

/// 列同步根目录时看到的签名
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RootSigs {
    /// 列表是按哪个本机设备 id 区分「本机 / 别的设备」的
    pub own_id: String,
    /// vault.json：`None` = 不在服务器上；`Some(None)` = 服务器没给签名
    pub vault: Option<Option<String>>,
    /// 别的设备的 head（本 vault）
    pub peers: BTreeMap<String, Option<String>>,
    /// 本机的 head：`None` = 不在服务器上
    pub own: Option<Option<String>>,
}

impl RootSigs {
    pub fn of(entries: &[Entry], vault_id: &str, own: &str) -> RootSigs {
        let mut sigs = RootSigs {
            own_id: own.to_string(),
            vault: None,
            peers: BTreeMap::new(),
            own: None,
        };
        for e in entries.iter().filter(|e| !e.is_dir) {
            if e.name == VAULT_FILE {
                sigs.vault = Some(sig_of(e));
            } else if let Some(dev) = layout::parse_head_name(vault_id, &e.name) {
                if dev == own {
                    sigs.own = Some(sig_of(e));
                } else {
                    sigs.peers.insert(dev.to_string(), sig_of(e));
                }
            }
        }
        sigs
    }
}

/// 上一轮成功同步结束时服务器的样子（进程内，随引擎存活）
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Snapshot {
    pub vault_id: String,
    pub own_id: String,
    pub vault: Option<String>,
    pub peers: BTreeMap<String, Option<String>>,
    /// 本机 head 的签名；`None` = 不知道（写入时服务器没回 ETag），下一次探测看到的值就记下来
    pub own: Option<String>,
}

/// 探测的结论（不含列表本身，由调用方附上）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Verdict {
    Unchanged,
    Changed,
    Unsupported,
}

/// 比较这次的列表与快照。看到本机 head 的签名而快照里还不知道时记进快照（只改内存）
pub(super) fn compare(snapshot: Option<&mut Snapshot>, now: &RootSigs, vault_id: &str) -> Verdict {
    // vault.json 不见了：交给完整的一轮报告「数据已被清除」
    let Some(vault) = &now.vault else {
        return Verdict::Changed;
    };
    // 没有签名就无法判断有没有变化：每次都得把 head 全读一遍，不适合高频探测
    if vault.is_none() || now.peers.values().any(Option::is_none) {
        return Verdict::Unsupported;
    }
    let Some(snap) = snapshot else {
        return Verdict::Changed;
    };
    if snap.vault_id != vault_id || snap.own_id != now.own_id || snap.vault != *vault || snap.peers != now.peers {
        return Verdict::Changed;
    }
    match &now.own {
        // 本机 head 消失了（被删除、服务器回档）：完整的一轮会重写它
        None => Verdict::Changed,
        // 别的机器在写我们的 head（数据目录被复制）：完整的一轮做克隆检测
        Some(Some(listed)) => match &snap.own {
            Some(known) if known != listed => Verdict::Changed,
            Some(_) => Verdict::Unchanged,
            None => {
                snap.own = Some(listed.clone());
                Verdict::Unchanged
            }
        },
        // 服务器不给本机 head 签名：只能不管它（别的设备的改动照样看得到）
        Some(None) => Verdict::Unchanged,
    }
}

impl From<Verdict> for Option<Peek> {
    fn from(v: Verdict) -> Option<Peek> {
        match v {
            Verdict::Unchanged => Some(Peek::Unchanged),
            Verdict::Unsupported => Some(Peek::Unsupported),
            Verdict::Changed => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VAULT: &str = "0123456789abcdef0123456789abcdef";

    fn entry(name: &str, etag: Option<&str>) -> Entry {
        Entry {
            name: name.into(),
            is_dir: false,
            etag: etag.map(Into::into),
            size: None,
            modified_ms: None,
        }
    }

    fn head(dev: &str) -> String {
        format!("{VAULT}.{dev}.head")
    }

    fn listing(own: Option<&str>, peers: &[(&str, &str)]) -> Vec<Entry> {
        let mut out = vec![
            entry(VAULT_FILE, Some("v1")),
            Entry {
                name: VAULT.into(),
                is_dir: true,
                etag: None,
                size: None,
                modified_ms: None,
            },
            // 旧 vault 的残留与无关文件不影响判断
            entry("ffffffffffffffffffffffffffffffff.aaaaaaaaaaaaaaaa.head", Some("x")),
            entry("notes.txt", Some("y")),
        ];
        if let Some(sig) = own {
            out.push(entry(&head("1111111111111111"), Some(sig)));
        }
        for (dev, sig) in peers {
            out.push(entry(&head(dev), Some(sig)));
        }
        out
    }

    fn snapshot_of(entries: &[Entry]) -> Snapshot {
        let sigs = RootSigs::of(entries, VAULT, "1111111111111111");
        Snapshot {
            vault_id: VAULT.into(),
            own_id: sigs.own_id.clone(),
            vault: sigs.vault.clone().flatten(),
            peers: sigs.peers.clone(),
            own: sigs.own.clone().flatten(),
        }
    }

    fn verdict(snap: Option<&mut Snapshot>, entries: &[Entry]) -> Verdict {
        compare(snap, &RootSigs::of(entries, VAULT, "1111111111111111"), VAULT)
    }

    #[test]
    fn same_listing_is_unchanged() {
        let base = listing(Some("o1"), &[("2222222222222222", "p1")]);
        let mut snap = snapshot_of(&base);
        assert_eq!(verdict(Some(&mut snap), &base), Verdict::Unchanged);
    }

    #[test]
    fn no_snapshot_means_changed() {
        let base = listing(Some("o1"), &[("2222222222222222", "p1")]);
        assert_eq!(verdict(None, &base), Verdict::Changed);
    }

    #[test]
    fn peer_changes_are_detected() {
        let base = listing(Some("o1"), &[("2222222222222222", "p1")]);
        let mut snap = snapshot_of(&base);
        for changed in [
            listing(Some("o1"), &[("2222222222222222", "p2")]),
            listing(Some("o1"), &[("2222222222222222", "p1"), ("3333333333333333", "q1")]),
            listing(Some("o1"), &[]),
        ] {
            assert_eq!(verdict(Some(&mut snap.clone()), &changed), Verdict::Changed);
        }
        // vault.json 被替换或消失
        let mut replaced = base.clone();
        replaced[0].etag = Some("v2".into());
        assert_eq!(verdict(Some(&mut snap), &replaced), Verdict::Changed);
        let gone: Vec<Entry> = base.iter().filter(|e| e.name != VAULT_FILE).cloned().collect();
        assert_eq!(verdict(Some(&mut snap), &gone), Verdict::Changed);
    }

    #[test]
    fn own_head_is_learned_once_then_guarded() {
        let base = listing(Some("o1"), &[("2222222222222222", "p1")]);
        let mut snap = snapshot_of(&base);
        snap.own = None;
        // 刚推送过、服务器没回 ETag：下一次列表里本机 head 的签名记下来，不算变化
        let pushed = listing(Some("o2"), &[("2222222222222222", "p1")]);
        assert_eq!(verdict(Some(&mut snap), &pushed), Verdict::Unchanged);
        assert_eq!(snap.own.as_deref(), Some("e:o2"));
        assert_eq!(verdict(Some(&mut snap), &pushed), Verdict::Unchanged);
        // 之后又变了：不是我们写的
        let foreign = listing(Some("o3"), &[("2222222222222222", "p1")]);
        assert_eq!(verdict(Some(&mut snap), &foreign), Verdict::Changed);
        // 本机 head 不见了
        let missing = listing(None, &[("2222222222222222", "p1")]);
        assert_eq!(verdict(Some(&mut snap), &missing), Verdict::Changed);
    }

    #[test]
    fn missing_signatures_are_unsupported() {
        let mut no_sig = listing(Some("o1"), &[("2222222222222222", "p1")]);
        let base = snapshot_of(&no_sig);
        for e in &mut no_sig {
            if e.name.contains("2222222222222222") {
                e.etag = None;
            }
        }
        assert_eq!(verdict(Some(&mut base.clone()), &no_sig), Verdict::Unsupported);
        let mut no_vault_sig = listing(Some("o1"), &[]);
        no_vault_sig[0].etag = None;
        assert_eq!(verdict(None, &no_vault_sig), Verdict::Unsupported);
        // 只有大小与修改时间也能比较
        let mut sized = listing(Some("o1"), &[("2222222222222222", "p1")]);
        for e in &mut sized {
            if !e.is_dir {
                e.etag = None;
                e.size = Some(10);
                e.modified_ms = Some(1);
            }
        }
        let mut snap = snapshot_of(&sized);
        assert_eq!(verdict(Some(&mut snap), &sized), Verdict::Unchanged);
    }

    #[test]
    fn another_vault_or_device_id_invalidates_the_snapshot() {
        let base = listing(Some("o1"), &[("2222222222222222", "p1")]);
        let mut snap = snapshot_of(&base);
        snap.vault_id = "another".into();
        assert_eq!(verdict(Some(&mut snap), &base), Verdict::Changed);
        let mut snap = snapshot_of(&base);
        snap.own_id = "9999999999999999".into();
        assert_eq!(verdict(Some(&mut snap), &base), Verdict::Changed);
    }
}
