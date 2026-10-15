//! 写剪贴板与模拟粘贴。
//!
//! 粘贴流程：调用方先 `focus.activate(target)`（或直接调用本函数），再隐藏面板；本函数确认目标已在
//! 前台（每 5 ms 检查一次，最多 100 ms），处理残留修饰键后发送粘贴键。目标已失效、最小化或被隐藏 → `TargetGone`；
//! 已提权 → `Elevated`（上层只复制并提示按 Ctrl+V）；目标无响应（`IsHungAppWindow`）或 100 ms 内没切过去 →
//! `FocusTimeout`，不发键。对目标窗口的操作一律是异步的（不会因目标卡死而卡住调用线程）。

use std::sync::Arc;
use std::time::Duration;

use ::windows::Win32::UI::WindowsAndMessaging::{IsHungAppWindow, IsIconic, IsWindowVisible};
use wisp_types::{AppRef, ClipPayload};

use crate::error::PlatformError;
use crate::traits::{Paste, PasteError, WriteOpts};

use super::Core;
use super::clipboard;
use super::focus;
use super::input;
use super::process;
use super::util::run_with_timeout;

/// 写剪贴板的时限（含等待原所有者处理 `WM_DESTROYCLIPBOARD`）
const WRITE_TIMEOUT: Duration = Duration::from_millis(1500);
/// 等待目标到前台的上限
const FOCUS_WAIT: Duration = Duration::from_millis(100);
/// 刚切换前台时给目标一点时间把键盘焦点还给输入控件
const SETTLE: Duration = Duration::from_millis(10);

/// `Paste` 的实现
pub(crate) struct WinPaste(pub(crate) Arc<Core>);

impl Paste for WinPaste {
    fn write(&self, payload: &ClipPayload, opts: WriteOpts) -> Result<u64, PlatformError> {
        let prepared = clipboard::prepare(payload, opts)?;
        let owner = self.0.clip.owner();
        run_with_timeout("wisp-clip-write", WRITE_TIMEOUT, move || {
            clipboard::write(prepared, owner)
        })
        .unwrap_or(Err(PlatformError::Timeout))
    }

    fn prepare_target(&self, target: &AppRef) -> Result<(), &'static str> {
        target_check(target)
    }

    fn paste_into(&self, target: &AppRef, plain: bool) -> Result<(), PasteError> {
        let core = &self.0;
        // 记录过具体窗口时只认这个窗口：它已关闭就算目标失效（不改粘到同一应用的其他窗口 / 文档里）
        let hwnd = if target.window.is_some() {
            focus::exact_window(target)
        } else {
            focus::target_window(target)
        }
        .ok_or(PasteError::TargetGone)?;
        // 最小化、或被隐藏（最小化到托盘）：SetForegroundWindow 不会让隐藏的窗口显示出来，Ctrl+V 会发给一个看不见的窗口
        // （或等满 100 ms 再报超时）——与 `prepare_target`（`target_check`）同一个判定，只复制并提示
        // SAFETY: 纯查询
        if unsafe { IsIconic(hwnd).as_bool() || !IsWindowVisible(hwnd).as_bool() } {
            return Err(PasteError::TargetGone);
        }
        // 目标无响应：切前台与发键都不会有结果，也不能让调用线程陪它卡住
        // SAFETY: 纯查询（不向目标发送消息）
        if unsafe { IsHungAppWindow(hwnd) }.as_bool() {
            return Err(PasteError::FocusTimeout);
        }
        let (_, pid) = process::window_thread_process(hwnd);
        let pid = target.pid.unwrap_or(pid);
        if process::is_elevated(pid) {
            return Err(PasteError::Elevated);
        }
        let switched = !focus::is_foreground(hwnd);
        if switched {
            focus::request_foreground(hwnd);
            if !focus::wait_foreground(hwnd, FOCUS_WAIT) {
                return Err(PasteError::FocusTimeout);
            }
            std::thread::sleep(SETTLE);
        }
        // plain 只影响写入的类型（调用方写入时已只放纯文本），粘贴键相同
        let _ = plain;
        // 粘贴键按目标窗口查映射表（控制台窗口另按 conhost.exe 匹配）
        let Some(combo) = input::app_keys_for_window(&core.keymaps, Some(target), hwnd, false) else {
            return Err(PasteError::Unsupported("paste key is disabled for this app".into()));
        };
        // 发键前再确认一次：100 ms 内用户可能又切走了，绝不粘到错误的窗口
        if !focus::is_foreground(hwnd) {
            return Err(PasteError::FocusTimeout);
        }
        input::send_combo(combo).map_err(PasteError::Other)
    }
}

/// 预激活（「先 SetForegroundWindow(prev)，再隐藏面板」）与交还焦点之前的检查（`Paste::prepare_target`）。与 [`WinPaste::paste_into`] 认的是**同一个窗口**：`focus::exact_window`——记录的窗口仍有效且属于
/// 同一进程，UWP 的窗口属于宿主 ApplicationFrameHost.exe、pid 是应用进程，同样接受（若直接比较 pid，UWP 应用
/// 会一律被判为「属于别的进程」而不预激活，粘贴时才切前台，面板隐藏后的焦点间隙里键盘输入会落到别处）。
/// 窗口被隐藏（最小化到托盘）、最小化、无响应时返回 Err：预激活会把它恢复出来，而 `paste_into` 对这些情形同样不切前台
/// （最小化 / 隐藏 → `TargetGone`、无响应 → `FocusTimeout`，上层只复制并提示）。
/// 全是纯查询（`IsHungAppWindow` 本身不向目标发消息），目标卡住也不会阻塞调用线程
fn target_check(target: &AppRef) -> Result<(), &'static str> {
    if target.window.is_none_or(|w| w == 0) {
        return Err("no recorded window");
    }
    let hwnd = focus::exact_window(target).ok_or("recorded window is closed or belongs to another process")?;
    // SAFETY: 以下都是纯查询：不解引用句柄、不向目标窗口发送消息
    unsafe {
        if IsIconic(hwnd).as_bool() {
            return Err("target window is minimized");
        }
        if !IsWindowVisible(hwnd).as_bool() {
            return Err("target window is hidden");
        }
        if IsHungAppWindow(hwnd).as_bool() {
            return Err("target window is not responding");
        }
    }
    Ok(())
}
