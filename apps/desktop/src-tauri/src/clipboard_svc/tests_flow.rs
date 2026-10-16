//! 状态机、时序与后台任务的接线：
//! - 粘贴的「阶段 × 事件」转移表与单飞：重入、同一会话再次粘贴、写入失败、会话在写入期间结束、粘贴中暂停 /
//!   删除条目、toast 在闸门之外；
//! - 面板离屏之后才发键（macOS orderOut 是异步的）；Windows 预激活只认记录的那个、仍可用的窗口，失败时不再等第二次；
//!   Wayland 会话没有原应用时的 toast 原因；
//! - 文件 exists 检查与事件投递的时序（排队执行的后台任务 + 准备中 / 已显示 / 隐藏的视图）、检查卡住期间到达的页面、
//!   投递的顺序、后台结果只推给仍存在的条目；
//! - 删除时是否置顶只凭缓存判断（缓存判断不了时才查库）；
//! - 监听 reconcile 的交错（快速的暂停 → 恢复）；
//! - 按天数过期（列表里隐藏 + 排清理；面板隐藏时最多每天再排一次）、打开数据库时排第一次清理；
//! - 常驻任务：空闲清理的调度（真实的 watch 通道）、监听工作任务。

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use wisp_clipboard::{ClipStore, IngestOutcome, IngestPolicy, RecentCache};
use wisp_platform::{PasteError, PlatformError, reason};
use wisp_types::{
    Capability, ClipFilter, ClipHit, ClipPage, ClipPayload, ClipboardChanged, CopiedReason, OsKind, PasteMode,
    PasteOutcome, RawClipboard,
};

use crate::error::{IpcError, IpcErrorCode};

use super::engine::pinned_in_cache;
use super::outbox::EXPECT_WINDOW;
use super::paste::{
    PasteEvent as E, PasteStage as S, PasteStep, copied_reason, no_target_reason, paste_payload, paste_step,
};
use super::port::{ClipView, OffscreenWait, PASTE_HIDE_WAIT, clip_view_of, wait_session_ended};
use super::retention::{EXPIRY_RECHECK_MS, drop_expired, run_with};
use super::tests::{Call, ICON_64, Offscreen, app, panel_item, rig, rig_with, text_raw, wait_until};

const DAY_MS: i64 = 86_400_000;

// ---------- 粘贴：状态机 ----------

#[test]
fn paste_transition_table_covers_every_stage_and_event() {
    use PasteStep::{Busy, Illegal as X, Next};
    let events = [
        E::Begin,
        E::Written,
        E::WriteFailed,
        E::Cancelled,
        E::CopyOnly,
        E::Hidden,
        E::Pasted,
        E::PasteFailed,
    ];
    let table: [(S, [PasteStep; 8]); 4] = [
        (S::Idle, [Next(S::Writing), X, X, X, X, X, X, X]),
        (S::Writing, [Busy, Next(S::Handoff), Next(S::Idle), X, X, X, X, X]),
        (
            S::Handoff,
            [Busy, X, X, Next(S::Idle), Next(S::Idle), Next(S::Pasting), X, X],
        ),
        (S::Pasting, [Busy, X, X, X, X, X, Next(S::Idle), Next(S::Idle)]),
    ];
    for (stage, row) in table {
        for (event, expected) in events.into_iter().zip(row) {
            assert_eq!(paste_step(stage, event), expected, "{stage:?} × {event:?}");
        }
    }
    // 每条路径都回到 Idle（单飞闸门因此不会卡住）
    let paths: [&[E]; 5] = [
        &[E::Begin, E::WriteFailed],
        &[E::Begin, E::Written, E::Cancelled],
        &[E::Begin, E::Written, E::CopyOnly],
        &[E::Begin, E::Written, E::Hidden, E::Pasted],
        &[E::Begin, E::Written, E::Hidden, E::PasteFailed],
    ];
    for path in paths {
        let end = path.iter().fold(S::Idle, |s, e| match paste_step(s, *e) {
            Next(n) => n,
            other => panic!("{s:?} × {e:?} → {other:?}"),
        });
        assert_eq!(end, S::Idle, "{path:?}");
    }
}

#[test]
fn a_second_paste_while_the_first_is_writing_is_rejected() {
    let r = rig();
    let a = r.copy_text(101, "first");
    let b = r.copy_text(102, "second");
    r.calls();
    // Windows 上 EmptyClipboard 可能卡 1.5 s：这期间面板仍可见，用户又按了一次 ↩（或单击模式下双击了条目）
    let engine = Arc::clone(&r.engine);
    let second: Arc<Mutex<Option<Result<PasteOutcome, IpcError>>>> = Arc::default();
    let slot = Arc::clone(&second);
    *r.port.paste.during_write.lock() = Some(Box::new(move || {
        *slot.lock() = Some(engine.paste(panel_item(1, b.id)));
    }));
    let out = r.paste(a.id, PasteMode::Normal, Some(app("target"))).expect("粘贴");
    assert_eq!(out, PasteOutcome::Pasted);
    let err = second.lock().take().expect("第二次请求").expect_err("被拒绝");
    assert_eq!(err.code, IpcErrorCode::Conflict);
    let calls = r.calls();
    assert_eq!(calls.iter().filter(|c| **c == Call::Write).count(), 1, "{calls:?}");
    assert_eq!(calls.iter().filter(|c| matches!(c, Call::PasteInto { .. })).count(), 1);
    assert!(r.host.toasts.lock().is_empty(), "被拒绝的请求不弹 toast");
    // 只有第一条被 touch
    let page = r.engine.first_page();
    let use_count = |id: i64| page.items.iter().find(|h| h.item.id == id).map(|h| h.item.use_count);
    assert_eq!((use_count(a.id), use_count(b.id)), (Some(2), Some(1)));
    assert_eq!(r.engine.paste_gate.stage(), S::Idle);
}

#[test]
fn a_panel_session_pastes_only_once() {
    // 第二个请求到达时面板还没反映出隐藏（同一会话仍可见）：按会话去重
    let r = rig();
    let a = r.copy_text(101, "once");
    r.host.hide_ends_session.store(false, Ordering::SeqCst);
    r.calls();
    assert_eq!(
        r.paste(a.id, PasteMode::Normal, Some(app("t"))).expect("粘贴"),
        PasteOutcome::Pasted
    );
    let err = r.paste(a.id, PasteMode::Normal, Some(app("t"))).expect_err("同一会话");
    assert_eq!(err.code, IpcErrorCode::Conflict);
    // 面板已进入隐藏（命令层取到的 target 为 None）：不再多写、多 touch，也不弹多余的 TargetGone toast
    let r = rig();
    let a = r.copy_text(101, "once");
    r.calls();
    assert_eq!(
        r.paste(a.id, PasteMode::Normal, Some(app("t"))).expect("粘贴"),
        PasteOutcome::Pasted
    );
    let err = r.paste(a.id, PasteMode::Normal, None).expect_err("面板已隐藏");
    assert_eq!(err.code, IpcErrorCode::Conflict);
    let calls = r.calls();
    assert_eq!(calls.iter().filter(|c| **c == Call::Write).count(), 1);
    assert!(r.host.toasts.lock().is_empty());
    assert_eq!(r.top().use_count, 2);
    // 新打开的面板照常粘贴
    r.reopen();
    assert_eq!(
        r.paste(a.id, PasteMode::Normal, Some(app("t"))).expect("粘贴"),
        PasteOutcome::Pasted
    );
}

#[test]
fn failures_before_the_handoff_leave_the_session_free_to_retry() {
    let r = rig();
    let a = r.copy_text(101, "retry");
    *r.port.paste.write_error.lock() = Some(PlatformError::Timeout);
    r.paste(a.id, PasteMode::Normal, Some(app("t"))).expect_err("写入失败");
    assert_eq!(r.engine.paste_gate.stage(), S::Idle);
    r.paste(9999, PasteMode::Normal, Some(app("t")))
        .expect_err("条目不存在");
    // 面板仍是同一个会话：用户重试成功
    assert_eq!(
        r.paste(a.id, PasteMode::Normal, Some(app("t"))).expect("重试"),
        PasteOutcome::Pasted
    );
}

#[test]
fn closing_the_panel_while_writing_cancels_the_keystroke() {
    let r = rig();
    let a = r.copy_text(101, "esc");
    r.calls();
    let host = Arc::clone(&r.host);
    *r.port.paste.during_write.lock() = Some(Box::new(move || host.set_view(ClipView::Hidden)));
    let out = r.paste(a.id, PasteMode::Normal, Some(app("t"))).expect("取消");
    assert_eq!(
        out,
        PasteOutcome::Copied {
            reason: CopiedReason::CopyOnly
        }
    );
    let calls = r.calls();
    assert!(
        matches!(calls.as_slice(), [Call::Write, Call::Suppress(_)]),
        "不隐藏、不发键、不 toast：{calls:?}"
    );
    assert_eq!(r.top().use_count, 2, "内容已写进剪贴板，照常 touch");
    assert_eq!(r.engine.paste_gate.stage(), S::Idle);
}

#[test]
fn pausing_or_deleting_during_a_paste_does_not_derail_it() {
    // 粘贴中暂停：照常粘贴
    let r = rig();
    let a = r.copy_text(101, "pause");
    let engine = Arc::clone(&r.engine);
    *r.port.paste.during_write.lock() = Some(Box::new(move || engine.set_paused(true)));
    assert_eq!(
        r.paste(a.id, PasteMode::Normal, Some(app("t"))).expect("粘贴"),
        PasteOutcome::Pasted
    );
    assert!(r.engine.paused());
    assert_eq!(r.top().use_count, 2);
    // 粘贴中删除条目：touch 失败只记日志，照常粘贴；条目不会被 touch 带回缓存
    let r = rig();
    let a = r.copy_text(101, "delete");
    let engine = Arc::clone(&r.engine);
    let id = a.id;
    *r.port.paste.during_write.lock() = Some(Box::new(move || engine.delete(id, true).expect("删除")));
    assert_eq!(
        r.paste(a.id, PasteMode::Normal, Some(app("t"))).expect("粘贴"),
        PasteOutcome::Pasted
    );
    assert!(r.engine.first_page().items.is_empty());
    assert_eq!(r.engine.paste_gate.stage(), S::Idle);
}

#[test]
fn the_toast_is_shown_after_the_gate_is_released() {
    // toast 要与主线程往返（最长约 1 s）：这期间闸门已回到 Idle，用户新打开的剪贴板面板可以照常粘贴
    let r = rig();
    let a = r.copy_text(101, "first");
    let b = r.copy_text(102, "second");
    *r.port.paste.paste_result.lock() = Err(PasteError::FocusTimeout);
    let engine = Arc::clone(&r.engine);
    let host = Arc::clone(&r.host);
    type Seen = (S, Result<PasteOutcome, IpcError>);
    let seen: Arc<Mutex<Option<Seen>>> = Arc::default();
    let slot = Arc::clone(&seen);
    *r.host.on_toast.lock() = Some(Box::new(move || {
        let stage = engine.paste_gate.stage();
        host.set_view(ClipView::Shown(9)); // 新的剪贴板面板会话
        let second = engine.paste(panel_item(9, b.id));
        *slot.lock() = Some((stage, second));
    }));
    let out = r.paste(a.id, PasteMode::Normal, Some(app("t"))).expect("只复制");
    assert_eq!(
        out,
        PasteOutcome::Copied {
            reason: CopiedReason::FocusTimeout
        }
    );
    let (stage, second) = seen.lock().take().expect("toast 期间的请求");
    assert_eq!(stage, S::Idle, "显示 toast 时闸门已放开");
    assert_eq!(
        second.expect("新会话的粘贴不被拒绝"),
        PasteOutcome::Copied {
            reason: CopiedReason::FocusTimeout
        }
    );
    assert_eq!(r.host.toasts.lock().len(), 2);
    // ⌘↩ 仅复制同样在闸门之外显示 toast
    let r = rig();
    let a = r.copy_text(101, "copy");
    let engine = Arc::clone(&r.engine);
    let stage: Arc<Mutex<Option<S>>> = Arc::default();
    let slot = Arc::clone(&stage);
    *r.host.on_toast.lock() = Some(Box::new(move || *slot.lock() = Some(engine.paste_gate.stage())));
    r.paste(a.id, PasteMode::CopyOnly, Some(app("t"))).expect("复制");
    assert_eq!(*stage.lock(), Some(S::Idle));
}

// ---------- 粘贴：离屏与焦点 ----------

// 离屏的等待经 `OffscreenWait`：mock 在被调用的那一刻记下「等到了通知」或「等到上限」，下面三条按调用序列判定，
// 不按墙钟（按墙钟判定的话，负载高的机器上会偶发先发键）

#[test]
fn the_keystroke_waits_for_the_panel_to_leave_the_screen() {
    let r = rig();
    let a = r.copy_text(101, "offscreen");
    *r.host.offscreen.lock() = Offscreen::Arrives;
    let mut ended = r.host.hidden_tx.subscribe();
    ended.borrow_and_update();
    r.calls();
    assert_eq!(
        r.paste(a.id, PasteMode::Normal, Some(app("t"))).expect("粘贴"),
        PasteOutcome::Pasted
    );
    let calls = r.calls();
    // 不等就发键：没有 Offscreen；先发键后才等：Offscreen 落在 PasteInto 之后——两种回归都过不了这条序列
    assert_eq!(
        &calls[2..],
        &[
            Call::Hide,
            Call::Offscreen,
            Call::PasteInto { plain: false },
            Call::Perf("cg_event".into()),
        ],
        "发键在窗口离屏之后"
    );
    // 等的是这次粘贴的会话
    assert!(ended.has_changed().expect("通道仍在"));
    assert_eq!(*ended.borrow(), 1);
}

#[test]
fn a_missing_offscreen_notice_only_delays_the_keystroke_up_to_the_cap() {
    let r = rig();
    let a = r.copy_text(101, "slow");
    *r.host.offscreen.lock() = Offscreen::Never;
    r.calls();
    assert_eq!(
        r.paste(a.id, PasteMode::Normal, Some(app("t"))).expect("粘贴"),
        PasteOutcome::Pasted
    );
    // 等待带着上限（PASTE_HIDE_WAIT），到点之后照常发键
    assert_eq!(
        &r.calls()[2..],
        &[
            Call::Hide,
            Call::OffscreenTimeout(PASTE_HIDE_WAIT),
            Call::PasteInto { plain: false },
            Call::Perf("cg_event".into()),
        ]
    );
}

#[test]
fn copy_only_does_not_wait_for_the_panel() {
    let r = rig();
    let a = r.copy_text(101, "copy");
    *r.host.offscreen.lock() = Offscreen::Never;
    r.calls();
    r.paste(a.id, PasteMode::CopyOnly, Some(app("t"))).expect("复制");
    let calls = r.calls();
    assert!(calls.contains(&Call::Hide), "{calls:?}");
    assert!(
        !calls
            .iter()
            .any(|c| matches!(c, Call::Offscreen | Call::OffscreenTimeout(_))),
        "只复制不等面板离屏：{calls:?}"
    );
}

#[test]
fn the_production_offscreen_wait_honours_the_notice_the_cap_and_a_closed_channel() {
    // 生产的等待端就是 mpsc 接收端（AppHost::hide_for_paste 的异步任务在离屏时发一个值）。只断言与机器快慢无关的部分：
    // 已到的通知立即返回 true；没有通知时至少等满上限（recv_timeout 的保证）；发送端已关闭时立即返回 false
    let (tx, rx) = mpsc::sync_channel::<()>(1);
    tx.send(()).expect("发送");
    assert!(Box::new(rx).wait(Duration::from_secs(60)));
    let (_tx, rx) = mpsc::sync_channel::<()>(1);
    let cap = Duration::from_millis(20);
    let started = Instant::now();
    assert!(!Box::new(rx).wait(cap));
    assert!(started.elapsed() >= cap);
    let (tx, rx) = mpsc::sync_channel::<()>(1);
    drop(tx);
    assert!(!Box::new(rx).wait(Duration::from_secs(60)));
}

#[test]
fn windows_does_not_pre_activate_minimized_or_closed_targets() {
    for why in ["target window is minimized", "recorded window is closed"] {
        let r = rig_with(OsKind::Windows, tempfile::tempdir().expect("临时目录"));
        let a = r.copy_text(101, "win");
        *r.port.target_check.lock() = Err(why);
        // paste_into 对这两种情形的判定是 TargetGone
        *r.port.paste.paste_result.lock() = Err(PasteError::TargetGone);
        r.calls();
        let out = r
            .paste(a.id, PasteMode::Normal, Some(app("notepad.exe")))
            .expect("只复制");
        assert_eq!(
            out,
            PasteOutcome::Copied {
                reason: CopiedReason::TargetGone
            }
        );
        let calls = r.calls();
        assert!(
            !calls.iter().any(|c| matches!(c, Call::Activate(_))),
            "{why}：不恢复 / 不拉起其他窗口"
        );
        assert_eq!(
            &calls[2..],
            &[
                Call::Hide,
                Call::PasteInto { plain: false },
                Call::Toast(CopiedReason::TargetGone)
            ],
            "{why}"
        );
        // ⌘↩ 仅复制同样不去激活
        r.reopen();
        r.calls();
        r.paste(a.id, PasteMode::CopyOnly, Some(app("notepad.exe")))
            .expect("复制");
        assert!(!r.calls().iter().any(|c| matches!(c, Call::Activate(_))), "{why}");
    }
}

#[test]
fn windows_activation_failure_does_not_wait_a_second_time() {
    for (err, reason) in [
        (PlatformError::Timeout, CopiedReason::FocusTimeout),
        (
            PlatformError::Other("target window is gone".into()),
            CopiedReason::TargetGone,
        ),
    ] {
        let r = rig_with(OsKind::Windows, tempfile::tempdir().expect("临时目录"));
        let a = r.copy_text(101, "win");
        *r.port.focus.activate_result.lock() = Err(err.clone());
        r.calls();
        let out = r
            .paste(a.id, PasteMode::Normal, Some(app("notepad.exe")))
            .expect("只复制");
        assert_eq!(out, PasteOutcome::Copied { reason }, "{err:?}");
        let calls = r.calls();
        assert_eq!(
            &calls[2..],
            &[Call::Activate("notepad.exe".into()), Call::Hide, Call::Toast(reason)],
            "{err:?}：不再调用 paste_into"
        );
    }
}

#[test]
fn wayland_reasons_are_matched_by_constant_not_by_substring() {
    for (key, expected) in [
        (reason::WAYLAND_PASTE, CopiedReason::Wayland),
        (reason::WAYLAND_FOCUS, CopiedReason::Wayland),
        // 名字里带 wayland、但不是粘贴 / 焦点的原因，以及其他没有对应原因的失败：只提示按键、不写原因（Manual）
        (reason::WAYLAND_CLIPBOARD, CopiedReason::Manual),
        (reason::X11_MISSING_XTEST, CopiedReason::Manual),
    ] {
        assert_eq!(copied_reason(&PasteError::Unsupported(key.into())), expected, "{key}");
    }
    // 以 Wayland 原因开头、但不是它本身的 key：同样不算（按常量全等匹配）。运行时拼出来，源码里不出现形如 i18n key 的
    // 字面量（前端的 i18n 单测会扫描 Rust 源码里 errors.platform.* 形状的字符串字面量，并要求它有文案）
    let lookalike = format!("{}_v2", reason::WAYLAND_PASTE);
    assert_eq!(copied_reason(&PasteError::Unsupported(lookalike)), CopiedReason::Manual);
}

#[test]
fn wayland_sessions_without_a_target_say_wayland() {
    // Wayland 会话取不到前台应用（prev_app 恒为 None）：提示「已复制，按 Ctrl+V 粘贴」，
    // 而不是「原来的应用已经关闭」
    let r = rig_with(OsKind::Linux, tempfile::tempdir().expect("临时目录"));
    *r.port.paste_inject.lock() = Capability::no(reason::WAYLAND_PASTE);
    let a = r.copy_text(101, "wayland");
    r.calls();
    let out = r.paste(a.id, PasteMode::Normal, None).expect("只复制");
    assert_eq!(
        out,
        PasteOutcome::Copied {
            reason: CopiedReason::Wayland
        }
    );
    assert_eq!(&r.calls()[2..], &[Call::Hide, Call::Toast(CopiedReason::Wayland)]);
    assert!(!r.host.toasts.lock()[0].paste_key.is_empty());
    // X11 / 能模拟粘贴的平台：不知道原应用（例如从托盘打开）只提示按键（Manual——不是「已经关闭」）
    let r = rig_with(OsKind::Linux, tempfile::tempdir().expect("临时目录"));
    let a = r.copy_text(101, "x11");
    assert_eq!(
        r.paste(a.id, PasteMode::Normal, None).expect("只复制"),
        PasteOutcome::Copied {
            reason: CopiedReason::Manual
        }
    );
    for (cap, expected) in [
        (Capability::yes(), CopiedReason::Manual),
        (Capability::no(reason::WAYLAND_PASTE), CopiedReason::Wayland),
        (Capability::no(reason::WAYLAND_FOCUS), CopiedReason::Wayland),
        (Capability::no(reason::X11_MISSING_XTEST), CopiedReason::Manual),
        (Capability::no(reason::NO_DISPLAY), CopiedReason::Manual),
        (Capability::default(), CopiedReason::Manual),
    ] {
        assert_eq!(no_target_reason(&cap), expected, "{cap:?}");
    }
}

// ---------- 粘贴：载荷 ----------

#[test]
fn plain_paste_of_files_lists_only_existing_paths() {
    let r = rig();
    let (item, [a, b]) = r.copy_files(101, ["a.txt", "b.txt"]);
    std::fs::remove_file(&b).expect("删文件");
    r.paste(item.id, PasteMode::Plain, Some(app("t"))).expect("纯文本粘贴");
    let written = r.port.paste.writes.lock().last().cloned().expect("写入");
    assert_eq!(written.text.as_deref(), Some(a.to_string_lossy().as_ref()));
    assert!(written.files.is_none(), "纯文本只写路径文本");
    std::fs::remove_file(&a).expect("删文件");
    r.reopen();
    r.calls();
    let err = r
        .paste(item.id, PasteMode::Plain, Some(app("t")))
        .expect_err("都不在了");
    assert_eq!(err.code, IpcErrorCode::NotFound);
    assert!(r.calls().is_empty(), "与普通粘贴一致：不写入、面板保持打开");
}

#[test]
fn plain_payloads_drop_rich_formats() {
    let rich = ClipPayload {
        text: Some("bold".into()),
        html: Some("<b>bold</b>".into()),
        rtf: Some("{\\rtf1 bold}".into()),
        ..ClipPayload::default()
    };
    let plain = paste_payload(rich.clone(), true).expect("纯文本");
    assert_eq!(
        (plain.text.as_deref(), plain.html, plain.rtf),
        (Some("bold"), None, None)
    );
    assert_eq!(paste_payload(rich.clone(), false).expect("原样"), rich);
    let image = ClipPayload {
        image_png: Some(vec![1, 2, 3]),
        ..ClipPayload::default()
    };
    assert_eq!(paste_payload(image.clone(), true).expect("图片照常"), image);
}

// ---------- 文件 exists 与事件投递的时序 ----------

#[test]
fn late_file_checks_wait_until_the_view_is_ready() {
    let r = rig();
    let (_, [_, b]) = r.copy_files(101, ["a.txt", "b.txt"]);
    std::fs::remove_file(&b).expect("删文件");
    // 编排器：先取首页（此时面板还没 prepare，显示的是别的视图），再 prepare；检查任务排队、没赶上时限
    r.host.set_view(ClipView::Hidden);
    r.events();
    r.exec.queue(true);
    let page = r.engine.first_page();
    assert!(
        page.items[0]
            .item
            .files
            .as_ref()
            .expect("files")
            .iter()
            .all(|f| f.exists),
        "超时的批次先按 true 交出"
    );
    assert!(r.exec.pending_jobs() >= 1);
    // 面板 prepare 了剪贴板视图，但还没 ready：检查完成，结果排队、不推送
    r.host.set_view(ClipView::Preparing(2));
    r.exec.run_jobs();
    assert!(r.events().is_empty(), "ready 之前不推送（视图可能还没订阅）");
    assert_eq!(r.engine.queued_events(), 1);
    assert_eq!(r.exec.fire_timers(), 1);
    assert!(r.events().is_empty(), "仍在准备：下一帧再看");
    // ready 之后补发
    r.host.set_view(ClipView::Shown(2));
    r.exec.fire_timers();
    let events = r.events();
    let [ClipboardChanged::Updated { item }] = events.as_slice() else {
        panic!("{events:?}")
    };
    assert_eq!(
        item.files
            .as_ref()
            .expect("files")
            .iter()
            .map(|f| f.exists)
            .collect::<Vec<_>>(),
        vec![true, false]
    );
    assert_eq!(r.engine.queued_events(), 0);
    // 缓存也是新的：下一次打开直接是置灰的
    r.exec.queue(false);
    assert_eq!(r.top().files, item.files);
}

#[test]
fn late_checks_for_later_pages_are_pushed_once_the_page_is_out() {
    let r = rig();
    let (_, [a, _]) = r.copy_files(101, ["a.txt", "b.txt"]);
    std::fs::remove_file(&a).expect("删文件");
    r.events();
    r.exec.queue(true);
    let page = r.engine.search("txt", ClipFilter::All, 10).expect("检索");
    assert!(
        page.items[0]
            .item
            .files
            .as_ref()
            .expect("files")
            .iter()
            .all(|f| f.exists)
    );
    assert!(r.events().is_empty(), "命令返回之前不推送");
    r.exec.run_jobs();
    let events = r.events();
    assert!(
        matches!(events.as_slice(), [ClipboardChanged::Updated { item }] if item.files.as_ref().is_some_and(|f| !f[0].exists)),
        "{events:?}"
    );
}

#[test]
fn queued_events_are_dropped_when_the_view_never_appears() {
    let r = rig();
    r.host.set_view(ClipView::Preparing(5));
    r.engine.emit(ClipboardChanged::Paused { paused: true });
    assert_eq!(r.engine.queued_events(), 1);
    assert!(r.events().is_empty());
    // 会话被取代 / 放弃：面板已不显示剪贴板视图
    r.host.set_view(ClipView::Hidden);
    r.exec.fire_timers();
    assert_eq!(r.engine.queued_events(), 0);
    assert!(r.events().is_empty());
    assert_eq!(r.exec.fire_timers(), 0, "丢弃后不再检查：零唤醒");
    // 隐藏且不是「即将打开」：直接丢弃，不排队
    r.engine.emit(ClipboardChanged::Paused { paused: false });
    assert_eq!(r.engine.queued_events(), 0);
}

#[test]
fn events_right_after_the_first_page_wait_for_the_new_view() {
    let r = rig();
    r.host.set_view(ClipView::Hidden);
    let _ = r.engine.first_page();
    // 首页交出之后、面板 prepare 之前的一次复制
    r.copy_text(101, "between");
    assert!(r.events().is_empty());
    assert!(r.engine.queued_events() >= 1);
    r.host.set_view(ClipView::Shown(3));
    r.exec.fire_timers();
    assert!(matches!(r.events().first(), Some(ClipboardChanged::Inserted { .. })));
    // 窗口过了：隐藏时直接丢弃
    std::thread::sleep(EXPECT_WINDOW + Duration::from_millis(20));
    r.host.set_view(ClipView::Hidden);
    r.copy_text(102, "later");
    let _ = r.events();
    assert_eq!(r.engine.queued_events(), 0);
}

#[test]
fn a_new_event_never_overtakes_queued_ones() {
    let r = rig();
    r.host.set_view(ClipView::Preparing(2));
    r.engine.emit(ClipboardChanged::Removed { id: 1 });
    r.engine.emit(ClipboardChanged::Removed { id: 2 });
    assert_eq!(r.engine.queued_events(), 2);
    r.host.set_view(ClipView::Shown(2));
    // 补发第一条的同时，另一个线程投递一条新事件
    let engine = Arc::clone(&r.engine);
    let racer: Arc<Mutex<Option<std::thread::JoinHandle<()>>>> = Arc::default();
    let slot = Arc::clone(&racer);
    *r.host.on_emit.lock() = Some(Box::new(move || {
        *slot.lock() = Some(std::thread::spawn(move || {
            engine.emit(ClipboardChanged::Removed { id: 3 });
        }));
        // 给它足够的时间：若在投递锁外补发，它会拿到已清空的队列、立即发出，插到排队的两条之前
        std::thread::sleep(Duration::from_millis(60));
    }));
    assert_eq!(r.exec.fire_timers(), 1);
    if let Some(t) = racer.lock().take() {
        t.join().expect("投递线程");
    }
    assert_eq!(
        r.events(),
        vec![
            ClipboardChanged::Removed { id: 1 },
            ClipboardChanged::Removed { id: 2 },
            ClipboardChanged::Removed { id: 3 },
        ]
    );
}

#[test]
fn pages_queued_behind_a_stuck_check_all_get_their_results() {
    let r = rig();
    let (x, [_, x2]) = r.copy_files(101, ["x1.txt", "x2.txt"]);
    let (y, [y1, y2]) = r.copy_files(102, ["y1.txt", "y2.txt"]);
    std::fs::remove_file(&x2).expect("删文件");
    std::fs::remove_file(&y2).expect("删文件");
    r.events();
    // 检查任务卡住（排着、不执行，相当于卡在网络卷上）：首页与翻页都超时，先按 true 交出
    r.exec.queue(true);
    let all_exist = |page: &ClipPage| {
        page.items
            .iter()
            .all(|h| h.item.files.as_ref().is_some_and(|f| f.iter().all(|f| f.exists)))
    };
    let first = r.engine.recent(0, 1, ClipFilter::Files).expect("首页");
    assert_eq!(first.items[0].item.id, y.id);
    let second = r.engine.recent(1, 1, ClipFilter::Files).expect("翻页");
    assert_eq!(second.items[0].item.id, x.id);
    assert!(all_exist(&first) && all_exist(&second));
    assert_eq!(r.exec.pending_jobs(), 1, "同一时刻只有一个检查任务");
    // 检查恢复：两页都拿到结果（若后到的页面覆盖先到的，首页的结果永远不会来）
    r.exec.run_jobs();
    let mut greyed: Vec<i64> = r
        .events()
        .iter()
        .filter_map(|e| match e {
            ClipboardChanged::Updated { item } if item.files.as_ref().is_some_and(|f| !f[1].exists) => Some(item.id),
            _ => None,
        })
        .collect();
    greyed.sort_unstable();
    let mut expected = vec![x.id, y.id];
    expected.sort_unstable();
    assert_eq!(greyed, expected);
    // 迟到的结果不推给已删除的条目
    std::fs::remove_file(&y1).expect("删文件");
    let page = r.engine.recent(0, 1, ClipFilter::Files).expect("首页");
    assert_eq!(page.items[0].item.id, y.id);
    r.engine.delete(y.id, true).expect("删除");
    r.exec.run_jobs();
    assert_eq!(
        r.events(),
        vec![ClipboardChanged::Removed { id: y.id }],
        "removed 之后不再有 updated"
    );
}

#[test]
fn late_thumbnails_are_pushed_only_while_the_item_exists() {
    let image = |change: u64| RawClipboard {
        change_id: change,
        image_png: Some(ICON_64.to_vec()),
        ..RawClipboard::default()
    };
    let inserted = |events: Vec<ClipboardChanged>| match events.as_slice() {
        [ClipboardChanged::Inserted { item }] => item.id,
        other => panic!("{other:?}"),
    };
    // ① 不在缓存里（例如 ≥ 200 条置顶占满了缓存）、库里还在：照常推送
    let r = rig();
    r.exec.queue(true);
    r.copy(101, image(101));
    let id = inserted(r.events());
    r.engine.serial().cache().remove(id);
    r.exec.run_jobs();
    let events = r.events();
    assert!(
        matches!(events.as_slice(), [ClipboardChanged::Updated { item }]
            if item.id == id && item.image.as_ref().is_some_and(|i| i.thumb_url.is_some())),
        "{events:?}"
    );
    // ② 缩略图已写回库、合并进缓存之前条目被删除：不推送
    let r = rig();
    r.exec.queue(true);
    r.copy(101, image(101));
    let id = inserted(r.events());
    let store = r.engine.ready_store().expect("数据库");
    let serial = r.engine.serial();
    let exec = Arc::clone(&r.exec);
    let worker = std::thread::spawn(move || exec.run_jobs());
    let written = Instant::now();
    let thumb_written = || {
        store
            .get_detail(id)
            .expect("详情")
            .and_then(|d| d.meta.image)
            .is_some_and(|i| i.thumb_url.is_some())
    };
    while !thumb_written() {
        assert!(written.elapsed() < Duration::from_secs(5), "缩略图没有写回");
        std::thread::sleep(Duration::from_millis(2));
    }
    // 后台任务此时停在串行锁上：删除这一条（与 Engine::delete 相同的两步）
    assert!(store.delete(id).expect("删除"));
    serial.cache().remove(id);
    drop(serial);
    worker.join().expect("后台任务");
    assert!(r.events().is_empty(), "已删除的条目不推送 updated");
}

#[test]
fn whether_an_item_is_pinned_comes_from_the_cache_when_it_can_tell() {
    assert_eq!(pinned_in_cache(&RecentCache::default(), 1), None, "加载失败的空缓存");
    let dir = tempfile::tempdir().expect("临时目录");
    let (a, b, beyond) = {
        let store = ClipStore::open(dir.path()).expect("建库");
        let policy = IngestPolicy::from(&wisp_types::Settings::default().clipboard);
        let ingest = |i: u64| match store
            .ingest(&text_raw(i, &format!("item {i}")), &policy, 1_000 + i as i64)
            .expect("写入")
        {
            IngestOutcome::Inserted(m) => m.id,
            other => panic!("{other:?}"),
        };
        let a = ingest(1);
        let b = ingest(2);
        store.set_pinned(a, true).expect("置顶");
        let cache = RecentCache::load(&store).expect("缓存");
        assert_eq!(pinned_in_cache(&cache, a), Some(true));
        assert_eq!(pinned_in_cache(&cache, b), Some(false));
        assert_eq!(pinned_in_cache(&cache, 9_999), Some(false), "缓存就是全部");
        // 再置顶 200 条：缓存（200 条）全是置顶、库里还有更多，不在缓存里的无法只凭缓存判断
        for i in 3..=202 {
            let id = ingest(i);
            store.set_pinned(id, true).expect("置顶");
        }
        let cache = RecentCache::load(&store).expect("缓存");
        assert_eq!(cache.len(), 200);
        assert_eq!(pinned_in_cache(&cache, b), None);
        // 最早的置顶 a 排在缓存之外
        assert_eq!(pinned_in_cache(&cache, a), None);
        (a, b, 9_999)
    };
    // 这种情形下删除才查库：未置顶的照常删除，置顶的要求 force
    let r = rig_with(OsKind::Macos, dir);
    r.engine.preload();
    r.engine.delete(b, false).expect("未置顶");
    assert!(r.engine.detail(b).expect("详情").is_none());
    let err = r.engine.delete(a, false).expect_err("置顶项要求 force");
    assert_eq!(err.code, IpcErrorCode::Conflict);
    r.engine.delete(beyond, false).expect("不存在的视为已删除");
}

#[test]
fn clip_view_follows_the_panel_state() {
    assert_eq!(clip_view_of(None, |_| true), ClipView::Hidden);
    assert_eq!(
        clip_view_of(Some((4, wisp_types::PanelView::Translate)), |_| true),
        ClipView::Hidden
    );
    assert_eq!(
        clip_view_of(Some((4, wisp_types::PanelView::Clipboard)), |_| false),
        ClipView::Preparing(4)
    );
    assert_eq!(
        clip_view_of(Some((4, wisp_types::PanelView::Clipboard)), |s| s == 4),
        ClipView::Shown(4)
    );
    assert_eq!(ClipView::Shown(4).session(), Some(4));
    assert_eq!(ClipView::Hidden.session(), None);
}

#[tokio::test]
async fn waiting_for_the_panel_ignores_other_sessions_and_has_a_cap() {
    let (tx, rx) = tokio::sync::watch::channel(0u64);
    let mut sub = rx.clone();
    sub.borrow_and_update();
    let sender = tx.clone();
    let notifier = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        sender.send_replace(6); // 别的会话结束
        tokio::time::sleep(Duration::from_millis(10)).await;
        sender.send_replace(7);
    });
    assert!(wait_session_ended(&mut sub, 7, Duration::from_secs(2)).await);
    notifier.await.expect("通知");
    // 超时
    let mut sub = rx.clone();
    sub.borrow_and_update();
    assert!(!wait_session_ended(&mut sub, 8, Duration::from_millis(20)).await);
    // 通道关闭
    let mut sub = rx;
    sub.borrow_and_update();
    drop(tx);
    assert!(!wait_session_ended(&mut sub, 9, Duration::from_secs(2)).await);
}

// ---------- 监听的 reconcile ----------

#[test]
fn a_quick_pause_and_resume_leaves_the_watch_installed() {
    let r = rig();
    r.engine.apply_settings_now(&wisp_types::Settings::default().clipboard);
    r.engine.reconcile_watch().expect("安装监听");
    assert!(r.engine.watching());
    r.exec.queue(true);
    r.engine.set_paused(true); // 暂停：排一次 reconcile（J1），先不执行
    // J1 读到 paused = true 之后、注销之前：用户又恢复了（J2），J2 立刻在另一个线程上执行
    let engine = Arc::clone(&r.engine);
    let exec = Arc::clone(&r.exec);
    let other: Arc<Mutex<Option<std::thread::JoinHandle<usize>>>> = Arc::default();
    let handle = Arc::clone(&other);
    *r.host.on_settings.lock() = Some(Box::new(move || {
        engine.set_paused(false);
        *handle.lock() = Some(std::thread::spawn(move || exec.run_jobs()));
        // 给 J2 足够的时间：修复前它会抢在 J1 之前跑完（监听还在，什么都不做），J1 随后把监听注销
        std::thread::sleep(Duration::from_millis(80));
    }));
    r.exec.run_jobs();
    if let Some(t) = other.lock().take() {
        t.join().expect("J2");
    }
    assert!(!r.engine.paused());
    assert!(r.engine.watching(), "未暂停时监听必须在（历史不能悄悄停止记录）");
}

// ---------- 按天数过期、打开时排清理 ----------

#[test]
fn expired_items_are_cut_from_pages() {
    let hit = |id: i64, pinned: bool, last_used_at: i64| ClipHit {
        item: wisp_types::ClipItemMeta {
            id,
            pinned,
            last_used_at,
            ..super::tests::text_meta()
        },
        highlights: Vec::new(),
    };
    let mut page = ClipPage {
        items: vec![
            hit(1, true, 10),
            hit(2, false, 300),
            hit(3, false, 200),
            hit(4, false, 50),
        ],
        pinned_count: 1,
        next_offset: Some(4),
        query: None,
        filter: ClipFilter::All,
    };
    assert_eq!(drop_expired(&mut page, 100), 1);
    assert_eq!(page.items.iter().map(|h| h.item.id).collect::<Vec<_>>(), vec![1, 2, 3]);
    assert_eq!(page.next_offset, None, "之后的只会更旧");
    assert_eq!(page.pinned_count, 1, "置顶不过期");
    let mut fresh = page.clone();
    fresh.next_offset = Some(3);
    assert_eq!(drop_expired(&mut fresh, 100), 0);
    assert_eq!(fresh.next_offset, Some(3));
}

#[test]
fn expired_items_are_hidden_and_then_deleted() {
    let r = rig();
    let old = r.copy_text(101, "old").id;
    let pinned_old = r.copy_text(102, "pinned old").id;
    r.engine.pin(pinned_old, true).expect("置顶");
    // 8 天之后又复制了一条；保留 7 天
    r.host.now.fetch_add(8 * DAY_MS, Ordering::SeqCst);
    r.copy_text(103, "fresh");
    let fresh = r.engine.first_page().items[1].item.id;
    r.set_settings(|s| s.clipboard.max_age_days = Some(7));
    r.engine.idle.take();
    let ids: Vec<i64> = r.engine.first_page().items.iter().map(|h| h.item.id).collect();
    assert_eq!(ids, vec![pinned_old, fresh], "过期的不再显示");
    assert!(r.engine.idle.dirty(), "同时排一次清理");
    let from_db = r.engine.recent(1, 10, ClipFilter::All).expect("翻页");
    assert!(from_db.items.iter().all(|h| h.item.id != old));
    let report = r.engine.run_cleanup();
    assert_eq!(report.removed_items, 1);
    assert_eq!(r.ids(), vec![pinned_old, fresh]);
}

#[test]
fn hiding_the_panel_rechecks_expiry_at_most_daily() {
    let r = rig();
    r.copy_text(101, "x");
    r.engine.run_cleanup();
    r.engine.idle.take();
    // 不限天数：从不因此排清理
    r.engine.check_expiry_on_hide();
    assert!(!r.engine.idle.dirty());
    r.set_settings(|s| s.clipboard.max_age_days = Some(30));
    r.engine.check_expiry_on_hide();
    assert!(!r.engine.idle.dirty(), "刚执行过保留策略");
    r.host.now.fetch_add(EXPIRY_RECHECK_MS, Ordering::SeqCst);
    r.engine.check_expiry_on_hide();
    assert!(r.engine.idle.dirty(), "满一天再排一次");
}

#[test]
fn opening_the_store_schedules_the_first_cleanup() {
    let r = rig();
    // 数据库还没打开：清理什么都不做，也不会把「启动后第一次清理」吃掉
    assert_eq!(r.engine.run_cleanup(), super::retention::CleanupReport::default());
    assert!(!r.engine.idle.dirty());
    r.engine.preload();
    assert!(r.engine.idle.dirty(), "打开之后的第一次空闲清孤儿 blob");
}

// ---------- 图标 ----------

#[test]
fn icon_urls_come_from_the_store_and_follow_the_displayed_source() {
    let r = rig();
    let a = r.copy_text(101, "icon");
    let detail = r.engine.detail(a.id).expect("详情").expect("存在");
    assert_eq!(
        r.top().source.and_then(|s| s.icon_url),
        detail.meta.source.and_then(|s| s.icon_url),
        "缓存里的图标路径与存储层给出的一致"
    );
    // 取不到图标的应用复制了一段文字，之后另一个应用复制了同样的文字（touch：来源仍是前者）
    let r = rig();
    r.port.apps.icon.store(false, std::sync::atomic::Ordering::SeqCst);
    let mut raw = text_raw(101, "same");
    raw.source = Some(app("com.example.noicon"));
    r.copy(101, raw);
    r.port.apps.icon.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut raw = text_raw(102, "same");
    raw.source = Some(app("com.example.other"));
    r.copy(102, raw);
    let top = r.top();
    assert_eq!(top.source.as_ref().map(|s| s.id.as_str()), Some("com.example.noicon"));
    assert!(
        top.source.and_then(|s| s.icon_url).is_none(),
        "不会把另一个应用的图标挂到这一条上"
    );
}

// ---------- 常驻任务 ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_cleanup_task_waits_for_the_panel_then_runs_once_and_sleeps() {
    let r = rig();
    r.engine.preload(); // 打开即排第一次清理
    r.host.set_view(ClipView::Shown(1));
    let task = tokio::spawn(run_with(Arc::clone(&r.engine), Duration::from_millis(30)));
    // 静默期已过、面板仍可见：不执行
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(r.engine.idle.runs.load(Ordering::SeqCst), 0);
    // 面板隐藏（会话结束通知）→ 执行一次
    r.host.set_view(ClipView::Hidden);
    r.host.hidden_tx.send_replace(1);
    assert!(
        wait_until(
            || r.engine.idle.runs.load(Ordering::SeqCst) == 1,
            Duration::from_secs(3)
        )
        .await,
        "隐藏后执行"
    );
    // 之后零唤醒
    tokio::time::sleep(Duration::from_millis(50)).await;
    let wakeups = r.engine.idle.wakeups.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(r.engine.idle.wakeups.load(Ordering::SeqCst), wakeups, "空闲零唤醒");
    assert_eq!(r.engine.idle.runs.load(Ordering::SeqCst), 1);
    // 按天数保留、一天之后面板又隐藏了一次：顺带排一次清理并执行
    r.set_settings(|s| s.clipboard.max_age_days = Some(7));
    r.host.now.fetch_add(EXPIRY_RECHECK_MS, Ordering::SeqCst);
    r.host.hidden_tx.send_replace(2);
    assert!(
        wait_until(
            || r.engine.idle.runs.load(Ordering::SeqCst) == 2,
            Duration::from_secs(3)
        )
        .await,
        "面板隐藏时检查过期"
    );
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_watch_worker_records_what_the_callback_announces() {
    let r = rig();
    r.queue_read(Ok(text_raw(101, "from the worker")));
    let worker = tokio::spawn(super::watch::run(Arc::clone(&r.engine)));
    r.engine.signal().post(101);
    assert!(
        wait_until(|| r.engine.first_page().items.len() == 1, Duration::from_secs(3)).await,
        "回调转交 → 工作任务读取并记录"
    );
    assert_eq!(r.top().preview, "from the worker");
    // 同一序号再报一次：不再读取
    r.engine.signal().post(101);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(r.port.clipboard.read_calls.load(Ordering::SeqCst), 1);
    worker.abort();
}
