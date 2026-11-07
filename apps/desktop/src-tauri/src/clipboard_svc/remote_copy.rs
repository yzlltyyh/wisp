//! 实时同步的子选项「把其他设备最新的复制直接放进本机剪贴板」（`sync.realtime_clipboard`，默认关闭）。
//!
//! 同步服务每轮成功之后（实时同步开着时）把本轮从**其他设备**拿到、已经写进本机历史的新条目交过来（只有 hash 与时间，
//! [`RemoteCopy`]），这里在阻塞线程上判断要不要把其中最新的一条写进系统剪贴板。条件（[`decide`]，逐条有单测）：
//! - 子选项开着，并且本机正在记录剪贴板历史（暂停、关闭记录时什么都不写：那是用户不想让 Wisp 碰剪贴板的时候；
//!   监听装不上时同样不写——看不到本机的复制，就没法避开它）；
//! - 它在那台设备上产生不超过 [`FRESH_MS`]：长时间离开后补拉的旧复制不该突然替换剪贴板。对方时钟比本机快时按 0 算
//!   （本轮刚拿到，本身就是新的）；
//! - 本机 [`LOCAL_QUIET`] 之内没有复制过（包括被隐私规则跳过的复制与 Wisp 自己的复制 / 粘贴），剪贴板也没有尚未处理的
//!   变化：刚复制的东西马上被另一台设备的内容盖掉最让人意外；
//! - 没有在连续粘贴、Wisp 没有在粘贴（写入与粘贴共用单飞闸门，见 `paste.rs` 的 `write_quietly`）；
//! - 本机历史里有这一条、而且是最近使用的一条：比本机任何别的条目都新；
//! - 能在本机粘贴：不是文件（文件不同步）、内容没有因为超长被截断（写进去的会是半截）；
//! - 不是验证码：自动替换剪贴板会把它交给本机任何读剪贴板的程序，而验证码正是会自动删除的敏感条目；
//! - 剪贴板里不已经是它（上一次写进去的就是这一条、之后没有别的变化）。
//!
//! 写入走 Wisp 自己写剪贴板的同一条路：带 `from_wisp` 标记并登记自写抑制，监听方看到后跳过——不会被当成新的本机复制
//! 记进历史、再经同步上传回去（防乒乓）。也不 touch：它已经是历史里最新的一条（同步写入时就排到了最上面），历史里
//! 不会多出一条，也不会因为最近使用时间变了而再产生一条同步记录。
//!
//! 日志只记计数与原因，不记内容。

use std::time::{Duration, Instant};

use wisp_types::{ClipKind, ClipPayload};

use super::engine::Engine;
use super::paste::paste_payload;

/// 其他设备上的复制产生之后，多久之内还算「刚复制的」
pub(crate) const FRESH_MS: i64 = 60_000;
/// 本机复制过之后，这么久之内不替换本机剪贴板
pub(crate) const LOCAL_QUIET: Duration = Duration::from_secs(10);

/// 同步拉来的一条其他设备的新复制（只有身份与时间，不含内容）。`Debug` 不输出 hash
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct RemoteCopy {
    /// 剪贴板条目的内容 hash（本机库里条目的身份）
    pub hash: [u8; 32],
    /// 那台设备记下这次复制的时间（Unix 毫秒）
    pub recorded_ms: i64,
    /// 条目自身的最近使用时间（Unix 毫秒）
    pub last_used_ms: i64,
}

impl std::fmt::Debug for RemoteCopy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteCopy")
            .field("recorded_ms", &self.recorded_ms)
            .field("last_used_ms", &self.last_used_ms)
            .finish_non_exhaustive()
    }
}

/// 为什么没有写（只进 debug 日志与单测）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Skip {
    /// 子选项或实时同步关着
    Off,
    /// 暂停记录、关闭了剪贴板历史、历史停用、监听没装上或数据库还没打开
    NotRecording,
    /// 在那台设备上产生超过 [`FRESH_MS`]
    Stale,
    /// 本机刚复制过，或剪贴板有还没处理的变化
    LocalCopy,
    /// 连续粘贴进行中
    Queue,
    /// Wisp 正在粘贴
    Pasting,
    /// 本机历史里找不到（被本机规则拒收、刚被删掉）或不在最近的条目里
    Missing,
    /// 本机有更新的条目
    Older,
    /// 本机粘贴不了的类型
    Kind,
    /// 内容超长被截断过
    Partial,
    /// 验证码
    Sensitive,
    /// 剪贴板里已经是它
    Current,
    /// 平台写入失败
    WriteFailed,
}

/// 本机历史里的那一条（判断用到的部分）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub(crate) kind: ClipKind,
    pub(crate) truncated: bool,
    /// 记录时被识别为验证码（与是否收藏、是否开启自动删除无关）
    pub(crate) otp: bool,
    pub(crate) last_used_ms: i64,
    /// 本机历史里除它之外最近使用的时间（没有别的条目时为 None）
    pub(crate) newest_other_ms: Option<i64>,
    /// 系统剪贴板里已经是它
    pub(crate) current: bool,
}

/// 判断用到的事实（调用方此刻读到的）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Facts {
    pub(crate) now_ms: i64,
    pub(crate) copy: RemoteCopy,
    /// 距本机最近一次复制多久（本进程里还没有过时为 None）
    pub(crate) since_local_copy: Option<Duration>,
    /// 剪贴板的序号与最近处理过的不同：有一次本机复制还没来得及处理
    pub(crate) unseen_local_change: bool,
    pub(crate) queue_active: bool,
    pub(crate) pasting: bool,
    /// 本机历史里的这一条；找不到时为 None
    pub(crate) item: Option<Candidate>,
}

/// 要不要把这一条写进系统剪贴板（条件见模块文档）
pub(crate) fn decide(f: &Facts) -> Result<(), Skip> {
    if f.now_ms.saturating_sub(f.copy.recorded_ms) > FRESH_MS {
        return Err(Skip::Stale);
    }
    if f.unseen_local_change || f.since_local_copy.is_some_and(|d| d < LOCAL_QUIET) {
        return Err(Skip::LocalCopy);
    }
    if f.queue_active {
        return Err(Skip::Queue);
    }
    if f.pasting {
        return Err(Skip::Pasting);
    }
    let Some(item) = f.item else {
        return Err(Skip::Missing);
    };
    if item.kind == ClipKind::Files {
        return Err(Skip::Kind);
    }
    if item.truncated {
        return Err(Skip::Partial);
    }
    if item.otp {
        return Err(Skip::Sensitive);
    }
    // 同一毫秒算不上「更新」：说不清谁先谁后时保留本机的
    if item.newest_other_ms.is_some_and(|other| other >= item.last_used_ms) {
        return Err(Skip::Older);
    }
    if item.current {
        return Err(Skip::Current);
    }
    Ok(())
}

impl Engine {
    /// 同步拉来了其他设备的新复制（最新的在前）：子选项开着时交给阻塞线程判断并写入。在同步任务上调用，不阻塞
    pub(crate) fn offer_remote_copies(&self, copies: Vec<RemoteCopy>) {
        let s = self.host.settings();
        if copies.is_empty() || !(s.sync.realtime && s.sync.realtime_clipboard) {
            return;
        }
        self.spawn(move |e| match e.place_remote_copy(&copies) {
            Ok(_) => log::info!("clipboard: placed the newest copy from another device on the clipboard"),
            Err(skip) => log::debug!("clipboard: newest copy from another device not placed reason={skip:?}"),
        });
    }

    /// 判断并把最新的一条写进系统剪贴板，返回写入后的剪贴板序号。**会阻塞**（数据库、平台写入）
    pub(crate) fn place_remote_copy(&self, copies: &[RemoteCopy]) -> Result<u64, Skip> {
        let settings = self.host.settings();
        if !(settings.sync.realtime && settings.sync.realtime_clipboard) {
            return Err(Skip::Off);
        }
        let Some(copy) = copies.iter().copied().max_by_key(|c| c.last_used_ms) else {
            return Err(Skip::Missing);
        };
        // 监听没装上（例如 Wayland 没有 data-control）时看不到本机的复制，「本机刚复制过」无从判断：不写
        if !self.recording() || !self.watching() {
            return Err(Skip::NotRecording);
        }
        // 同步从不为自己打开数据库：还没打开时拉来的条目也还没写进来
        let Some(store) = self.ready_store() else {
            return Err(Skip::NotRecording);
        };
        let clipboard_now = self.port.clipboard().change_id();
        let written = self.remote_written();
        let ours = written.is_some_and(|(change, _)| change == clipboard_now);
        let id = match store.id_by_hash(&copy.hash) {
            Ok(id) => id,
            Err(e) => {
                log::debug!("clipboard: remote copy lookup failed: {e}");
                None
            }
        };
        let item = id.and_then(|id| {
            let (meta, newest_other_ms) = self.newest_besides(id)?;
            Some(Candidate {
                kind: meta.kind,
                truncated: meta.truncated,
                otp: meta.expires_at.is_some() || store.is_otp(id).unwrap_or(true),
                last_used_ms: meta.last_used_at,
                newest_other_ms,
                current: ours && written.is_some_and(|(_, hash)| hash == copy.hash),
            })
        });
        let facts = Facts {
            now_ms: self.host.now_ms(),
            copy,
            since_local_copy: self.since_local_copy(),
            unseen_local_change: !ours && self.last_seen_change().is_some_and(|seen| seen != clipboard_now),
            queue_active: self.queue_status().is_some(),
            pasting: self.paste_busy(),
            item,
        };
        decide(&facts)?;
        let id = id.ok_or(Skip::Missing)?;
        let payload = match store.get_payload(id, false) {
            Ok(Some(p)) => p,
            // 图片原图丢失之类：粘贴不出来就不写
            Ok(None) => return Err(Skip::Missing),
            Err(e) => {
                log::debug!("clipboard: remote copy payload failed: {e}");
                return Err(Skip::Missing);
            }
        };
        let payload: ClipPayload = paste_payload(payload, false).map_err(|_| Skip::Kind)?;
        match self.write_quietly(&payload) {
            Ok(Some(change)) => {
                self.set_remote_written(change, copy.hash);
                Ok(change)
            }
            Ok(None) => Err(Skip::Pasting),
            Err(e) => {
                log::info!("clipboard: writing the copy from another device failed: {e}");
                Err(Skip::WriteFailed)
            }
        }
    }

    /// 本机有了一次复制（记录管线读到的变化，或 Wisp 自己的复制 / 粘贴）：之后 [`LOCAL_QUIET`] 之内不替换剪贴板
    pub(crate) fn note_local_copy(&self) {
        *self.last_local_copy.lock() = Some(Instant::now());
    }

    fn since_local_copy(&self) -> Option<Duration> {
        self.last_local_copy.lock().map(|at| at.elapsed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_760_000_000_000;

    fn candidate() -> Candidate {
        Candidate {
            kind: ClipKind::Text,
            truncated: false,
            otp: false,
            last_used_ms: NOW - 2_000,
            newest_other_ms: Some(NOW - 30_000),
            current: false,
        }
    }

    fn facts() -> Facts {
        Facts {
            now_ms: NOW,
            copy: RemoteCopy {
                hash: [7; 32],
                recorded_ms: NOW - 2_000,
                last_used_ms: NOW - 2_000,
            },
            since_local_copy: Some(Duration::from_secs(60)),
            unseen_local_change: false,
            queue_active: false,
            pasting: false,
            item: Some(candidate()),
        }
    }

    fn with(f: impl FnOnce(&mut Facts)) -> Result<(), Skip> {
        let mut x = facts();
        f(&mut x);
        decide(&x)
    }

    fn with_item(f: impl FnOnce(&mut Candidate)) -> Result<(), Skip> {
        with(|x| {
            let mut item = candidate();
            f(&mut item);
            x.item = Some(item);
        })
    }

    #[test]
    fn a_fresh_newest_copy_from_another_device_is_placed() {
        assert_eq!(decide(&facts()), Ok(()));
        assert_eq!(with(|x| x.since_local_copy = None), Ok(()), "本进程里还没复制过");
        assert_eq!(with_item(|i| i.newest_other_ms = None), Ok(()), "历史里只有它");
    }

    #[test]
    fn only_copies_made_within_a_minute_count() {
        assert_eq!(with(|x| x.copy.recorded_ms = NOW - FRESH_MS), Ok(()));
        assert_eq!(with(|x| x.copy.recorded_ms = NOW - FRESH_MS - 1), Err(Skip::Stale));
        assert_eq!(
            with(|x| x.copy.recorded_ms = NOW + 5 * 60_000),
            Ok(()),
            "对方时钟快：本轮刚拿到的仍算新的"
        );
    }

    #[test]
    fn a_recent_or_unprocessed_local_copy_wins() {
        assert_eq!(
            with(|x| x.since_local_copy = Some(LOCAL_QUIET - Duration::from_millis(1))),
            Err(Skip::LocalCopy)
        );
        assert_eq!(with(|x| x.since_local_copy = Some(LOCAL_QUIET)), Ok(()));
        assert_eq!(with(|x| x.unseen_local_change = true), Err(Skip::LocalCopy));
    }

    #[test]
    fn never_while_pasting() {
        assert_eq!(with(|x| x.queue_active = true), Err(Skip::Queue));
        assert_eq!(with(|x| x.pasting = true), Err(Skip::Pasting));
    }

    #[test]
    fn the_item_must_be_in_history_pasteable_and_not_sensitive() {
        assert_eq!(with(|x| x.item = None), Err(Skip::Missing));
        assert_eq!(with_item(|i| i.kind = ClipKind::Files), Err(Skip::Kind));
        for kind in [
            ClipKind::Text,
            ClipKind::RichText,
            ClipKind::Image,
            ClipKind::Link,
            ClipKind::Color,
            ClipKind::Code,
        ] {
            assert_eq!(with_item(|i| i.kind = kind), Ok(()), "{kind:?}");
        }
        assert_eq!(with_item(|i| i.truncated = true), Err(Skip::Partial));
        assert_eq!(with_item(|i| i.otp = true), Err(Skip::Sensitive));
    }

    #[test]
    fn it_must_be_newer_than_everything_else_here() {
        assert_eq!(
            with_item(|i| i.newest_other_ms = Some(i.last_used_ms + 1)),
            Err(Skip::Older)
        );
        assert_eq!(
            with_item(|i| i.newest_other_ms = Some(i.last_used_ms)),
            Err(Skip::Older),
            "同一毫秒保留本机的"
        );
        assert_eq!(with_item(|i| i.newest_other_ms = Some(i.last_used_ms - 1)), Ok(()));
    }

    #[test]
    fn nothing_to_do_when_it_is_already_on_the_clipboard() {
        assert_eq!(with_item(|i| i.current = true), Err(Skip::Current));
    }

    #[test]
    fn debug_hides_the_hash() {
        let s = format!("{:?}", facts().copy);
        assert!(!s.contains("hash") && !s.contains("7, 7"), "{s}");
    }
}
