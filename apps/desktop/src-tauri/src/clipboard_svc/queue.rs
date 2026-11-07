//! 连续粘贴（队列）：多选的条目按选择顺序排成一队，开始时立即粘贴第一条；之后用户在任何应用里每按一次粘贴键
//! （⌘V / Ctrl+V，以及按键映射表里各应用的粘贴键），就把下一条写进剪贴板再让这次按键生效。
//!
//! # 机制
//! - 拦截：队列进行中安装一份只含 `paste_keys` 的输入钩子（[`PasteKeyHooks`]）。平台层拦下匹配的粘贴键（目标应用
//!   收不到）并发出 `PasteKey`；这里在阻塞线程上经统一的粘贴管线写入下一条（`PasteHost::Queue`：写入 + 自写抑制 +
//!   touch，不发键），再让平台层带 Wisp 标记重放同一组键，钩子放行，目标应用粘贴的就是刚写入的那一条；
//! - 第一条由调用方的宿主（面板 / 悬浮剪贴板）照常粘贴；钩子在它之后才安装（X11 的被动抓取分不清 XTEST 与物理按键，
//!   先装会把第一条自己的粘贴键当成用户按的）。第一条只复制成功（例如焦点没交还成功）时它仍在剪贴板里：下一次
//!   粘贴键只重放、不前进，不会跳过它；
//! - 最后一条粘贴出去之后立即结束（之后的粘贴键照常粘贴最后一条）。
//!
//! # 不消耗序号的情形
//! Wisp 没有「粘贴生效了」的信号（事后读输入框的值既要读用户内容、又证明不了失败），只在能确定落空时不前进，其余照旧：
//! - Wisp 自己的窗口持有键盘焦点（悬浮窗的搜索框、设置窗口……）：只重放、不记账、不提示，就像队列不存在；
//! - 重放之后读一次焦点所在之处的类别（`common::paste_target`，只读类别不读内容）：文本条目落在经典按钮、Finder /
//!   资源管理器的文件视图与桌面、菜单上时，这一条留到下一次粘贴键（提示「留到下一次」）。判断不了一律照常前进；
//! - 按下的是平台默认的粘贴键、前台应用却映射了别的粘贴键（Linux 终端的 Ctrl+Shift+V）：改发它的粘贴键；改键不安全时
//!   原样重放、这一条留住；
//! - 这一条还没粘贴出去、期间剪贴板被别的复制改写了：下一次先重新写入它再重放。
//!
//! 不论判定如何，拦下的按键都照常重放：误判的最坏结果是下一次粘贴键再粘贴同一条，绝不会卡住。判断不了的失误（网页里
//! 点到正文、点到不接收焦点的地方）靠「退回一条 / 跳过一条」补救（[`Engine::queue_back`] / [`Engine::queue_skip`]）。
//!
//! # 结束
//! 队列用完、Esc（队列期间注册全局 Esc）、用户取消（视图里的按钮、托盘状态行）、10 分钟没有按过粘贴键、会话锁定 /
//! 切到安全桌面（平台层的 `SessionLocked`）、钩子装不上或重放失败。结束时立刻卸载钩子与全局 Esc——粘贴键绝不会在
//! 队列之外被拦下。无操作的超时是一次性定时器（同一时刻只有一个），到点时按墙钟核对；系统睡眠让定时器迟到时，
//! 醒来后的第一次粘贴键按墙钟判定已经超时，同样结束并照常放行这次按键。
//!
//! # 并发
//! 队列状态只在短临界区里改，粘贴、装卸钩子、重放与宿主回调都在锁外：粘贴进行中再按的粘贴键丢弃（按键已被拦下，
//! 什么都不发生，再按一次即可）。每次开始递增代数，迟到的钩子信号与定时器按代数核对，不会作用到下一个队列。
//!
//! # 隐私
//! 日志只记条数、序号、结束原因，以及焦点判定的类别与耗时；不记条目内容、应用名与窗口标题。

use std::time::{Duration, Instant};

use parking_lot::Mutex;
use wisp_platform::common::combo::Combo;
use wisp_platform::common::keymap::{self, KeyChoice};
use wisp_platform::common::paste_target::{self, PasteTarget};
use wisp_platform::{HookError, HookHandle, InputSignal, KeyChord, PasteKeys, reason};
use wisp_types::{ClipKind, CopiedReason, OsKind, PasteMode, PasteOutcome, QueueNote, QueueStatus, ToastPayload};

use crate::error::{IpcError, IpcErrorCode};

use super::engine::Engine;
use super::paste::{PasteHost, PasteRequest, PasteSource};

/// 多久没按粘贴键就结束队列
pub(crate) const QUEUE_IDLE: Duration = Duration::from_secs(10 * 60);

/// 重放之后读焦点的预算（含查前台应用）：与取词的 AX 预算相同；Windows 只做微秒级的纯查询，用不满
pub(crate) const TARGET_PROBE: Duration = Duration::from_millis(100);

/// 队列为什么结束（只进日志）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueueEnd {
    /// 最后一条已经粘贴出去
    Done,
    /// 用户取消（视图、托盘）
    Cancelled,
    /// 全局 Esc
    Escape,
    /// 很久没有按粘贴键
    Idle,
    /// 会话锁定 / 切到安全桌面
    Locked,
    /// 新的队列开始
    Replaced,
    /// 钩子装不上、重放失败、剩下的条目都已不存在
    Failed,
}

/// 进行中的一个队列
struct Run {
    epoch: u64,
    ids: Vec<i64>,
    /// 已写进剪贴板的那一条（下标）
    loaded: usize,
    /// `loaded` 那一条已经粘贴出去（第一条只复制成功、判断落空、退回之后为 false：下一次粘贴键只重放、不前进）
    pasted: bool,
    /// `loaded` 那一条写入之后剪贴板的变更序号；None = 下一次要重新写入（退回到更早的一条、跳过之后）。只重放之前与
    /// 当前序号核对：期间用户复制过别的东西时先重新写入，不把别的内容当成这一条粘贴出去
    loaded_change: Option<u64>,
    /// 拦下的粘贴键不止一种（按键映射表里有别的应用的粘贴键）：重放之前核对前台应用的粘贴键。只有一种时（macOS 默认）
    /// 不必每次查前台应用
    check_front: bool,
    /// 正在粘贴 / 重放：这期间再来的粘贴键丢弃，退回 / 跳过也不动作
    busy: bool,
    /// 最近一次活动（开始、按粘贴键、退回 / 跳过）的墙钟时刻，Unix 毫秒
    active_at: i64,
    /// 拦下粘贴键的钩子；drop 即卸载
    hook: Option<HookHandle>,
}

impl Run {
    fn status(&self) -> QueueStatus {
        QueueStatus {
            active: true,
            index: u32::try_from(self.loaded + usize::from(self.pasted)).unwrap_or(u32::MAX),
            total: u32::try_from(self.ids.len()).unwrap_or(u32::MAX),
        }
    }
}

/// 队列的状态（`Engine::queue`）
#[derive(Default)]
pub(crate) struct PasteQueue {
    inner: Mutex<QueueState>,
}

#[derive(Default)]
struct QueueState {
    /// 每次开始递增
    epoch: u64,
    run: Option<Run>,
    /// 无操作超时的定时器已排上（同一时刻只有一个）
    timer: bool,
}

/// 一次粘贴键要做的事（在锁内决定、锁外执行）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Wisp 自己的窗口持有键盘焦点（悬浮窗的搜索框、设置窗口……）：只重放（粘贴剪贴板里现有的内容），不写入、不记账、
    /// 不提示——这一下不是粘贴到目标应用
    Pass,
    /// 当前这一条还没粘贴出去：只重放（剪贴板已经不是它时先重新写入）
    Replay,
    /// 写入第 n 条再重放
    Advance(usize),
    /// 超时之后的第一次按键：结束队列，照常放行这次按键
    Expire,
}

/// 平台层的钩子错误 → IPC 错误（权限 / 不支持的原因是 i18n key，前端直接本地化）
fn hook_error(e: HookError) -> IpcError {
    match e {
        HookError::PermissionMissing(kind) => IpcError::from(wisp_platform::PlatformError::PermissionMissing(kind)),
        HookError::Unsupported(key) => IpcError::new(IpcErrorCode::Platform, key),
        HookError::Other(m) => IpcError::new(IpcErrorCode::Platform, m),
    }
}

/// 拦下哪些粘贴键：平台默认键在前，再加上按键映射表里这个平台各应用的粘贴键（去重；设为不发送的跳过）
pub(crate) fn paste_keys(keymaps: &[wisp_types::AppKeymap], os: OsKind) -> PasteKeys {
    let default = keymap::paste_key(&[], os, None);
    let mapped = keymaps.iter().filter(|k| k.os == os).filter_map(|k| {
        let combo = wisp_platform::common::combo::Combo::parse(k.paste.as_deref()?)?;
        Some(combo)
    });
    let chords = match default {
        KeyChoice::Send(c) => Some(c),
        KeyChoice::Skip => None,
    }
    .into_iter()
    .chain(mapped)
    .filter_map(|c| KeyChord::of(&c));
    PasteKeys::new(chords)
}

impl Engine {
    /// 连续粘贴为什么不可用（i18n key）；可用时为 None。不阻塞（只读平台层已有的探测结果）
    pub(crate) fn queue_unavailable(&self) -> Option<String> {
        self.port.paste_hooks().unavailable()
    }

    /// 当前进度（没有进行中的队列时为 None）
    pub(crate) fn queue_status(&self) -> Option<QueueStatus> {
        self.queue.inner.lock().run.as_ref().map(Run::status)
    }

    /// 开始连续粘贴：立即经 `host`（调用窗口的宿主）粘贴第一条，再装上拦下粘贴键的钩子与全局 Esc。已有队列时先结束它。
    /// 第一条粘贴失败时不开始（错误原样返回）；只有一条时粘贴完就结束。**会阻塞**
    pub(crate) fn queue_start(
        &self,
        ids: Vec<i64>,
        host: PasteHost,
        selected_at: Instant,
    ) -> Result<QueueStatus, IpcError> {
        if let Some(why) = self.queue_unavailable() {
            return Err(IpcError::new(IpcErrorCode::Platform, why));
        }
        let Some(&first) = ids.first() else {
            return Err(IpcError::invalid_argument("ids must not be empty"));
        };
        self.queue_end(QueueEnd::Replaced);
        let total = ids.len();
        let epoch = {
            let mut g = self.queue.inner.lock();
            g.epoch += 1;
            // 先占住（busy）：第一条粘贴期间不会有别的开始插进来作用到同一代
            g.run = Some(Run {
                epoch: g.epoch,
                ids,
                loaded: 0,
                pasted: false,
                loaded_change: None,
                check_front: false,
                busy: true,
                active_at: self.host.now_ms(),
                hook: None,
            });
            g.epoch
        };
        let first_outcome = self.paste(PasteRequest {
            source: PasteSource::Item {
                id: first,
                transform: None,
            },
            mode: PasteMode::Normal,
            host,
            selected_at,
        });
        let pasted = match first_outcome {
            Ok(PasteOutcome::Pasted) => true,
            // 只复制：第一条留在剪贴板里，下一次粘贴键先把它粘贴出去
            Ok(PasteOutcome::Copied { reason }) => {
                if matches!(reason, CopiedReason::NoPermission | CopiedReason::Wayland) {
                    // 发不了键的环境：钩子同样装不上 / 重放不了，不开始（toast 已经说明了原因）
                    log::info!("sequential paste not started: first item copied only ({reason:?})");
                    self.drop_run(epoch);
                    return Ok(QueueStatus {
                        active: false,
                        index: 0,
                        total: u32::try_from(total).unwrap_or(u32::MAX),
                    });
                }
                false
            }
            Err(e) => {
                self.drop_run(epoch);
                return Err(e);
            }
        };
        if total == 1 && pasted {
            self.drop_run(epoch);
            return Ok(QueueStatus {
                active: false,
                index: 1,
                total: 1,
            });
        }
        // 第一条之后才装钩子（见模块文档）
        let keys = paste_keys(
            &crate::app::merged_keymaps(&self.host.settings().hotkeys.app_keymaps),
            self.os,
        );
        let check_front = keys.iter().count() > 1;
        let me = self.me.clone();
        let sink = Box::new(move |sig: InputSignal| {
            // 平台回调线程上只转交：粘贴与重放在阻塞线程上进行
            if let Some(engine) = me.upgrade() {
                let job = engine.clone();
                engine.exec.blocking(Box::new(move || job.on_queue_signal(epoch, &sig)));
            }
        });
        let hook = match self.port.paste_hooks().install(keys, sink) {
            Ok(h) => h,
            Err(e) => {
                log::warn!("sequential paste: paste keys not intercepted: {e}");
                self.drop_run(epoch);
                return Err(hook_error(e));
            }
        };
        // 钩子装好之后才判断第一条有没有落空：判断期间（这一代仍是 busy）再按的粘贴键被拦下丢弃，不会漏给应用、把
        // 第一条再粘贴一次
        let kept = pasted && matches!(self.probe_target(first), PasteTarget::Rejects(_));
        let change = self.port.clipboard().change_id();
        let status = {
            let mut g = self.queue.inner.lock();
            match g.run.as_mut().filter(|r| r.epoch == epoch) {
                Some(run) => {
                    run.hook = Some(hook);
                    run.pasted = pasted && !kept;
                    run.loaded_change = Some(change);
                    run.check_front = check_front;
                    run.busy = false;
                    Some(run.status())
                }
                // 第一条粘贴期间被取消（Esc、托盘）：钩子随 drop 卸载
                None => None,
            }
        };
        let Some(status) = status else {
            return Ok(QueueStatus {
                active: false,
                index: u32::from(pasted),
                total: u32::try_from(total).unwrap_or(u32::MAX),
            });
        };
        self.host.queue_escape_guard(true);
        self.arm_idle_timer();
        log::info!("sequential paste started: {total} item(s), first pasted={pasted} kept={kept}");
        // 第一条只复制时 Rust 已经弹了说明原因的 toast：不拿进度把它换掉。判断落空时没有那条说明，提示这一条留着
        let toast = if kept {
            Some(self.queue_toast(status, Some(QueueNote::Kept)))
        } else {
            pasted.then(|| self.queue_toast(status, None))
        };
        self.host.queue_changed(&status, toast);
        Ok(status)
    }

    /// 结束进行中的队列（没有时什么都不做）：卸载钩子与全局 Esc，推送结束。**会阻塞**（卸载钩子要等钩子线程）
    pub(crate) fn queue_end(&self, why: QueueEnd) {
        let run = self.queue.inner.lock().run.take();
        self.finish_run(run, why, None);
    }

    /// 全局 Esc：有进行中的队列就结束它，返回是否处理了
    pub(crate) fn queue_escape(&self) -> bool {
        let run = self.queue.inner.lock().run.take();
        let handled = run.is_some();
        self.finish_run(run, QueueEnd::Escape, None);
        handled
    }

    /// 退回一条（悬浮窗工具栏、⌘K、⋯ 菜单）：刚粘贴出去的那一条重新排到下一次粘贴键；那一条本来就还没粘贴出去（留着、
    /// 第一条只复制、刚退回过）时再往前退一条。纯状态变更、不碰剪贴板：剪贴板里还是那一条时下一次只重放，否则（更早的
    /// 一条、期间复制过别的）下一次先重新写入。没有队列、粘贴进行中、已经退到第一条之前时什么都不做，返回 None；否则
    /// 返回新的进度。**会阻塞**（进度提示要与主线程往返）
    pub(crate) fn queue_back(&self) -> Option<QueueStatus> {
        let now = self.host.now_ms();
        let status = {
            let mut g = self.queue.inner.lock();
            let run = g.run.as_mut().filter(|r| !r.busy)?;
            if run.pasted {
                run.pasted = false;
            } else if run.loaded > 0 {
                run.loaded -= 1;
                run.loaded_change = None;
            } else {
                return None;
            }
            run.active_at = now;
            run.status()
        };
        log::info!("sequential paste: back to {}/{}", status.index, status.total);
        self.host
            .queue_changed(&status, Some(self.queue_toast(status, Some(QueueNote::Back))));
        Some(status)
    }

    /// 跳过一条：下一次粘贴键要粘贴的那一条不再粘贴，直接到再下一条。留着的那一条（判断落空、第一条只复制）其实已经
    /// 进去了时也用它（误判的补救）。跳过的是最后一条时队列用完、结束。纯状态变更、不碰剪贴板；没有队列、粘贴进行中时
    /// 什么都不做，返回 None；否则返回新的进度。**会阻塞**（进度提示要与主线程往返，结束时还要卸载钩子）
    pub(crate) fn queue_skip(&self) -> Option<QueueStatus> {
        let now = self.host.now_ms();
        let (mut status, done, epoch) = {
            let mut g = self.queue.inner.lock();
            let run = g.run.as_mut().filter(|r| !r.busy)?;
            if run.pasted {
                // 队列没有结束说明 `loaded` 之后还有条目
                run.loaded += 1;
                run.loaded_change = None;
            } else {
                run.pasted = true;
            }
            run.active_at = now;
            (run.status(), run.loaded + 1 >= run.ids.len(), run.epoch)
        };
        log::info!("sequential paste: skipped to {}/{}", status.index, status.total);
        if done {
            self.end_epoch_noted(epoch, QueueEnd::Done, Some(QueueNote::Skipped));
            status.active = false;
        } else {
            self.host
                .queue_changed(&status, Some(self.queue_toast(status, Some(QueueNote::Skipped))));
        }
        Some(status)
    }

    /// 开始失败时撤销占位（这一代还没有钩子与全局 Esc，不推送结束）
    fn drop_run(&self, epoch: u64) {
        let mut g = self.queue.inner.lock();
        if g.run.as_ref().is_some_and(|r| r.epoch == epoch) {
            g.run = None;
        }
    }

    /// 结束一代队列。`note` 是结束提示的说明（跳过最后一条时）
    fn finish_run(&self, run: Option<Run>, why: QueueEnd, note: Option<QueueNote>) {
        let Some(mut run) = run else { return };
        let mut status = run.status();
        status.active = false;
        let had_hook = run.hook.is_some();
        // 先卸载钩子：之后的粘贴键一律照常交给应用
        drop(run.hook.take());
        if had_hook {
            self.host.queue_escape_guard(false);
        }
        log::info!("sequential paste ended ({why:?}) at {}/{}", status.index, status.total);
        // 粘贴完最后一条时同样提示一次（「连续粘贴 · 5 / 5」）；其余结束方式只推送状态
        let toast = (why == QueueEnd::Done).then(|| self.queue_toast(status, note));
        self.host.queue_changed(&status, toast);
    }

    /// 结束第 `epoch` 代队列（已经结束或换了一代时什么都不做）
    fn end_epoch(&self, epoch: u64, why: QueueEnd) {
        self.end_epoch_noted(epoch, why, None);
    }

    fn end_epoch_noted(&self, epoch: u64, why: QueueEnd, note: Option<QueueNote>) {
        let run = {
            let mut g = self.queue.inner.lock();
            if g.run.as_ref().is_some_and(|r| r.epoch == epoch) {
                g.run.take()
            } else {
                None
            }
        };
        self.finish_run(run, why, note);
    }

    /// 钩子信号（在阻塞线程上执行）
    fn on_queue_signal(&self, epoch: u64, sig: &InputSignal) {
        match sig {
            InputSignal::PasteKey { chord } => self.queue_step(epoch, *chord),
            InputSignal::SessionLocked => self.end_epoch(epoch, QueueEnd::Locked),
            _ => {}
        }
    }

    /// 一次粘贴键：决定这一步（锁内）→ 写入 / 重放 / 判断有没有落空（锁外）→ 记下进度
    fn queue_step(&self, epoch: u64, chord: KeyChord) {
        let now = self.host.now_ms();
        // 原子读（窗口获焦事件维护），在取锁之前问
        let in_wisp = self.host.keyboard_in_wisp();
        let (step, ids, loaded, loaded_change, check_front) = {
            let mut g = self.queue.inner.lock();
            let Some(run) = g.run.as_mut().filter(|r| r.epoch == epoch) else {
                drop(g);
                // 队列在这次按键的信号送达之前已经结束（Esc、托盘、最后一条刚粘贴完）：按键已被钩子拦下，
                // 照常重放，粘贴剪贴板里现有的内容，不让这一下凭空消失
                self.replay(chord);
                return;
            };
            if run.busy {
                log::debug!("sequential paste: paste key ignored while the previous one is in progress");
                return;
            }
            let idle = Duration::from_millis(u64::try_from(now.saturating_sub(run.active_at)).unwrap_or(0));
            let step = if idle >= QUEUE_IDLE {
                Step::Expire
            } else if in_wisp {
                Step::Pass
            } else if !run.pasted {
                Step::Replay
            } else {
                Step::Advance(run.loaded + 1)
            };
            run.busy = true;
            run.active_at = now;
            (step, run.ids.clone(), run.loaded, run.loaded_change, run.check_front)
        };
        // 之后任何一步 panic 都要结束这一代：否则 busy 一直为真，每次粘贴键都被吞掉直到无操作超时
        let _guard = StepGuard { engine: self, epoch };
        // 结束且照常放行这次按键时，先重放再结束：重放要在钩子还在时进行（X11 的抓取由重放临时撤销、再抓回；
        // 先结束再重放会在路由已清除后把粘贴键抓回去）
        let next = match step {
            Step::Expire => {
                // 睡眠让定时器迟到：醒来后的第一次粘贴键结束队列，并照常粘贴剪贴板里现有的内容
                self.replay(chord);
                self.end_epoch(epoch, QueueEnd::Idle);
                return;
            }
            Step::Pass => {
                if self.replay(chord) {
                    self.release(epoch);
                } else {
                    self.end_epoch(epoch, QueueEnd::Failed);
                }
                return;
            }
            Step::Replay if loaded_change == Some(self.port.clipboard().change_id()) => Some(loaded),
            // 这一条已经不在剪贴板里（期间复制过别的、退回到了更早的一条）：先重新写入，从它开始
            Step::Replay => self.load_next(&ids, loaded),
            Step::Advance(next) => self.load_next(&ids, next),
        };
        let Some(next) = next else {
            // 剩下的都已不存在：结束，这次按键照常粘贴剪贴板里现有的内容
            self.replay(chord);
            self.end_epoch(epoch, QueueEnd::Failed);
            return;
        };
        let change = self.port.clipboard().change_id();
        // 按下的是不是前台应用的粘贴键（拦下的粘贴键只有一种时不必查）
        let key = if check_front {
            self.front_replay_key(chord)
        } else {
            ReplayKey::Pressed
        };
        let sent = match key {
            ReplayKey::Mapped(mapped) => mapped,
            ReplayKey::Pressed | ReplayKey::Mismatch => chord,
        };
        if !self.replay(sent) {
            self.end_epoch(epoch, QueueEnd::Failed);
            return;
        }
        // 按键原样交给了不认它的应用：已经确定没粘贴进去，不必再读焦点
        let kept = key == ReplayKey::Mismatch || matches!(self.probe_target(ids[next]), PasteTarget::Rejects(_));
        let (status, done) = {
            let mut g = self.queue.inner.lock();
            let Some(run) = g.run.as_mut().filter(|r| r.epoch == epoch) else {
                return;
            };
            run.loaded = next;
            run.loaded_change = Some(change);
            run.pasted = !kept;
            run.busy = false;
            // 最后一条留着时不结束：钩子还在，下一次粘贴键再粘贴它
            (run.status(), run.pasted && run.loaded + 1 >= run.ids.len())
        };
        if done {
            self.end_epoch(epoch, QueueEnd::Done);
        } else {
            let note = kept.then_some(QueueNote::Kept);
            self.host.queue_changed(&status, Some(self.queue_toast(status, note)));
        }
    }

    /// 只重放之后放开这一代（不改进度）
    fn release(&self, epoch: u64) {
        let mut g = self.queue.inner.lock();
        if let Some(run) = g.run.as_mut().filter(|r| r.epoch == epoch) {
            run.busy = false;
        }
    }

    /// 前台应用的粘贴键核对（见 [`replay_key`]）。**会阻塞**（X11 要与服务器往返）
    fn front_replay_key(&self, pressed: KeyChord) -> ReplayKey {
        let Some(default) = Combo::parse(keymap::default_paste(self.os)).and_then(|c| KeyChord::of(&c)) else {
            return ReplayKey::Pressed;
        };
        let key = replay_key(pressed, default, self.port.paste().foreground_paste_key());
        if key != ReplayKey::Pressed {
            log::debug!("sequential paste: pressed key is not the front app's paste key ({key:?})");
        }
        key
    }

    /// 重放之后读一次焦点所在之处的类别，判断这一下有没有落空（规则见 `paste_target`）。放在重放之后：用户感到的粘贴
    /// 延迟一点不增加，判定只影响记账（这期间这一代仍是 busy，再按的粘贴键被拦下丢弃，与写入期间相同）。
    ///
    /// 风险：粘贴本身可能移动焦点。按键是异步投递的，读到的通常是目标处理这次粘贴之前的焦点；目标处理得快时，读到的
    /// 可能已经是粘贴移动过的焦点（粘贴后自动跳到下一格、粘贴弹出了对话框而焦点落在它的按钮上）。误判为落空的代价是
    /// 下一次粘贴键再粘贴同一条，提示里写着「留到下一次」，用户可以「跳过一条」。
    ///
    /// 规则只对文本条目生效：文件 / 图片条目不读焦点（条目刚 touch 过、在缓存里；查不到时按非文本处理）
    fn probe_target(&self, id: i64) -> PasteTarget {
        let text_item = self
            .cached(id)
            .is_some_and(|m| !matches!(m.kind, ClipKind::Image | ClipKind::Files));
        if !text_item {
            return PasteTarget::Unknown;
        }
        let started = Instant::now();
        let facts = self.port.focus().focus_facts(TARGET_PROBE);
        let verdict = paste_target::classify(facts.as_ref(), true);
        log::debug!(
            "sequential paste: target surface={:?} verdict={verdict:?} in {}ms",
            facts.map(|f| f.surface),
            started.elapsed().as_millis()
        );
        verdict
    }

    /// 把第 `from` 条起第一条还能粘贴的写进剪贴板（不发键），返回它的下标；都不能粘贴时为 None
    fn load_next(&self, ids: &[i64], from: usize) -> Option<usize> {
        for (k, id) in ids.iter().enumerate().skip(from) {
            let req = PasteRequest {
                source: PasteSource::Item {
                    id: *id,
                    transform: None,
                },
                mode: PasteMode::Normal,
                host: PasteHost::Queue,
                selected_at: Instant::now(),
            };
            match self.paste(req) {
                Ok(_) => return Some(k),
                // 条目已删除、图片 / 文件已不在：跳到下一条；与别的粘贴撞上（单飞）同样跳过这一条
                Err(e) => log::info!(
                    "sequential paste: item {} of {} skipped: {}",
                    k + 1,
                    ids.len(),
                    e.message
                ),
            }
        }
        None
    }

    /// 重放拦下的粘贴键，返回是否成功
    fn replay(&self, chord: KeyChord) -> bool {
        match self.port.paste_hooks().replay(chord) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("sequential paste: replaying the paste key failed: {e}");
                false
            }
        }
    }

    /// 无操作超时：排一个一次性定时器（已有就不再排）。到点时按墙钟核对，还没到就按剩余时间再排一次
    fn arm_idle_timer(&self) {
        {
            let mut g = self.queue.inner.lock();
            if g.timer || g.run.is_none() {
                return;
            }
            g.timer = true;
        }
        self.schedule_idle_check(QUEUE_IDLE);
    }

    fn schedule_idle_check(&self, after: Duration) {
        let me = self.me.clone();
        self.exec.after(
            after,
            Box::new(move || {
                let Some(engine) = me.upgrade() else { return };
                // 定时器任务本身不得阻塞：结束（卸载钩子）放进阻塞线程池
                let job = engine.clone();
                engine.exec.blocking(Box::new(move || job.idle_check()));
            }),
        );
    }

    fn idle_check(&self) {
        let now = self.host.now_ms();
        let remaining = {
            let mut g = self.queue.inner.lock();
            let Some(run) = g.run.as_ref() else {
                g.timer = false;
                return;
            };
            let idle = Duration::from_millis(u64::try_from(now.saturating_sub(run.active_at)).unwrap_or(0));
            if idle < QUEUE_IDLE {
                Some(QUEUE_IDLE - idle)
            } else {
                g.timer = false;
                None
            }
        };
        match remaining {
            Some(rest) => self.schedule_idle_check(rest),
            None => self.queue_end(QueueEnd::Idle),
        }
    }

    /// 连续粘贴进度的 toast 载荷（toast 视图显示「连续粘贴 · 2 / 5」与下一次要按的粘贴键；`note` 给出时改为这一次的
    /// 说明：留到下一次 / 已退回 / 已跳过）
    pub(crate) fn queue_toast(&self, status: QueueStatus, note: Option<QueueNote>) -> ToastPayload {
        ToastPayload {
            reason: CopiedReason::CopyOnly,
            paste_key: self.paste_key_label(None),
            queue: Some(status),
            queue_note: note,
        }
    }
}

/// 拦下的粘贴键怎样重放（[`replay_key`]）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplayKey {
    /// 原样重放
    Pressed,
    /// 改发前台应用映射的粘贴键
    Mapped(KeyChord),
    /// 前台应用不认这个键、又不能安全地换键：原样重放，但这一条算没有粘贴进去
    Mismatch,
}

/// 拦下的粘贴键 `pressed` 在前台应用里是不是它的粘贴键（`front` 是 `Paste::foreground_paste_key` 的结果，`default` 是
/// 平台默认的粘贴键）。纯函数：
/// - 取不到前台应用、映射表要求不发键、前台应用正用这个键：原样重放；
/// - 按下的不是平台默认键（是映射表里别的应用的粘贴键，例如浏览器里的 Ctrl+Shift+V 是纯文本粘贴、Shift+Insert 在多数
///   应用里也是粘贴）：信任用户，原样重放；
/// - 按下的是平台默认键、前台应用映射了别的键（Linux 终端里按了 Ctrl+V，它在终端里是「逐字输入」）：新组合包含按下的
///   全部修饰键时改发它——用户可能还按着那些修饰键，X11 的重放不模拟、也不松开用户按着的修饰键，按着的正是新组合需要的；
///   否则（xterm 的 Shift+Insert 不含 Ctrl，按着的 Ctrl 会把它变成别的组合）不换键，这一条留住
pub(crate) fn replay_key(pressed: KeyChord, default: KeyChord, front: Option<KeyChoice>) -> ReplayKey {
    let Some(KeyChoice::Send(combo)) = front else {
        return ReplayKey::Pressed;
    };
    let Some(mapped) = KeyChord::of(&combo) else {
        return ReplayKey::Pressed;
    };
    if mapped == pressed || pressed != default {
        return ReplayKey::Pressed;
    }
    let (want, held) = (mapped.modifiers, pressed.modifiers);
    let covers = (want.control || !held.control)
        && (want.alt || !held.alt)
        && (want.shift || !held.shift)
        && (want.super_key || !held.super_key);
    if covers {
        ReplayKey::Mapped(mapped)
    } else {
        ReplayKey::Mismatch
    }
}

/// `queue_step` 中途 panic（展开）时结束这一代队列。正常返回时什么都不做
struct StepGuard<'a> {
    engine: &'a Engine,
    epoch: u64,
}

impl Drop for StepGuard<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.engine.end_epoch(self.epoch, QueueEnd::Failed);
        }
    }
}

/// 平台层报告的连续粘贴不可用原因：先看能否注入按键（重放要用），再看输入钩子（拦下粘贴键要用）
pub(crate) fn unavailable_of(
    paste_inject: &wisp_types::Capability,
    input_hooks: &wisp_types::Capability,
) -> Option<String> {
    [paste_inject, input_hooks].into_iter().find(|c| !c.available).map(|c| {
        c.reason_key
            .clone()
            .unwrap_or_else(|| reason::HOOKS_NOT_COMPILED.to_owned())
    })
}
