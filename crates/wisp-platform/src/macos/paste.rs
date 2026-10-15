//! 写剪贴板与模拟粘贴。
//!
//! `paste_into`：目标已退出 → TargetGone；没有辅助功能权限 → PermissionMissing（上层只复制并提示）；目标不在前台
//! （Wisp 因固定窗口等原因处于激活状态）→ 激活目标并每 5 ms 轮询，100 ms 内没到前台 → FocusTimeout；
//! 等一帧（约 16 ms，让刚 orderOut 的面板交还 key 窗口）后经 HID 层发送粘贴键（按映射表与当前布局）。
//!
//! `plain`（只粘贴 utf8-plain-text）：剪贴板内容由调用方先经 `write` 写好，纯文本粘贴就是调用方
//! 用只含 `text` 的载荷调用 `write`；`paste_into` 不改写剪贴板（改写会产生新的变更序号，打乱调用方的去重与抑制），
//! 只在 `plain` 时检查剪贴板上是否还留着富内容类型，有就记录警告（调用方违反了约定），照常粘贴。

use std::sync::Arc;
use std::time::{Duration, Instant};

use wisp_types::{AppRef, ClipPayload, OsKind};

use super::Ctx;
use super::ax;
use super::focus::{activate_app, frontmost_pid, running_app};
use super::keyboard;
use super::pasteboard;
use super::util::pool;
use crate::common::keymap::{self, KeyChoice};
use crate::error::PlatformError;
use crate::traits::{Paste, PasteError, WriteOpts};

/// 等待目标到前台的上限
const FOCUS_WAIT: Duration = Duration::from_millis(100);
/// 轮询间隔
const FOCUS_POLL: Duration = Duration::from_millis(5);
/// 发键前等一帧
const FRAME: Duration = Duration::from_millis(16);

/// Paste 实现
pub(crate) struct MacPaste {
    ctx: Arc<Ctx>,
}

impl MacPaste {
    pub(crate) fn new(ctx: Arc<Ctx>) -> Self {
        Self { ctx }
    }
}

impl Paste for MacPaste {
    fn write(&self, payload: &ClipPayload, opts: WriteOpts) -> Result<u64, PlatformError> {
        pasteboard::write(payload, opts)
    }

    fn paste_into(&self, target: &AppRef, plain: bool) -> Result<(), PasteError> {
        let started = Instant::now();
        let running = pool(|| running_app(target).map(|a| a.processIdentifier()));
        let Some(pid) = running else {
            return Err(PasteError::TargetGone);
        };
        if !ax::trusted() {
            return Err(PasteError::PermissionMissing);
        }
        if frontmost_pid() != Some(pid) {
            activate_app(&self.ctx, target).map_err(|e| match e {
                PlatformError::Timeout => PasteError::FocusTimeout,
                other => PasteError::Other(other.to_string()),
            })?;
            let deadline = Instant::now() + FOCUS_WAIT;
            while frontmost_pid() != Some(pid) {
                if Instant::now() >= deadline {
                    return Err(PasteError::FocusTimeout);
                }
                std::thread::sleep(FOCUS_POLL);
            }
        }
        if plain && pasteboard::has_rich_types() {
            log::warn!(
                "paste: plain paste requested but the clipboard still holds rich types; write a text-only payload first"
            );
        }
        std::thread::sleep(FRAME);
        let combo = match keymap::paste_key(&self.ctx.keymaps.read(), OsKind::Macos, Some(target)) {
            KeyChoice::Send(c) => c,
            // 用户把粘贴键设为 none：只写入、不发键
            KeyChoice::Skip => return Ok(()),
        };
        keyboard::post_combo(&self.ctx, &combo).map_err(PasteError::Other)?;
        log::debug!(
            "paste: app={} plain={plain} elapsed={}ms",
            target.id,
            started.elapsed().as_millis()
        );
        Ok(())
    }
}
