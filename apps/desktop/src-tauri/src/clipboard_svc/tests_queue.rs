//! 连续粘贴（`queue.rs`）的单测：开始（第一条经调用窗口的宿主粘贴、之后才装钩子）、每次粘贴键写入下一条再重放、
//! 第一条只复制时不跳过它、各种结束（用完、取消、Esc、锁屏、无操作超时、重放失败、新队列替换）都卸载钩子与全局 Esc、
//! 不可用时不开始。平台与宿主都是 `tests.rs` 的测试替身，按键由 [`MockHooks::press`] 模拟。

use std::sync::atomic::Ordering;
use std::time::Instant;

use wisp_platform::common::combo::{Combo, KeyChord};
use wisp_platform::{HookError, InputSignal, PasteError};
use wisp_types::{AppKeymap, OsKind, QueueStatus};

use crate::error::IpcErrorCode;

use super::paste::PasteHost;
use super::queue::{QUEUE_IDLE, QueueEnd, paste_keys};
use super::tests::{Call, Rig, app, rig};

fn chord(s: &str) -> KeyChord {
    KeyChord::of(&Combo::parse(s).expect("combo")).expect("chord")
}

fn cmd_v() -> KeyChord {
    chord("Super+KeyV")
}

/// 最近一次写入剪贴板的文本
fn last_written(r: &Rig) -> Option<String> {
    r.port.paste.writes.lock().last().and_then(|p| p.text.clone())
}

/// 面板会话里开始一个队列（目标应用记为会话的 prev_app）
fn start(r: &Rig, ids: Vec<i64>) -> Result<QueueStatus, crate::error::IpcError> {
    *r.host.prev_app.lock() = Some(app("target"));
    let session = r.host.view().session().expect("面板正显示剪贴板");
    r.engine.queue_start(ids, PasteHost::Panel { session }, Instant::now())
}

fn status(active: bool, index: u32, total: u32) -> QueueStatus {
    QueueStatus { active, index, total }
}

fn three(r: &Rig) -> [i64; 3] {
    let a = r.copy_text(101, "first");
    let b = r.copy_text(102, "second");
    let c = r.copy_text(103, "third");
    r.calls();
    [a.id, b.id, c.id]
}

#[test]
fn each_paste_key_writes_the_next_item_then_replays_the_key() {
    let r = rig();
    let [a, b, c] = three(&r);
    let s = start(&r, vec![a, b, c]).expect("开始");
    assert_eq!(s, status(true, 1, 3));
    assert_eq!(last_written(&r).as_deref(), Some("first"));
    let calls = r.calls();
    // 第一条经面板照常粘贴（隐藏面板、发键），之后才装钩子与全局 Esc
    let paste = calls
        .iter()
        .position(|c| matches!(c, Call::PasteInto { .. }))
        .expect("第一条发键");
    let install = calls.iter().position(|c| *c == Call::HookInstall).expect("装钩子");
    assert!(
        paste < install,
        "钩子在第一条之后才装（X11 分不清自己的发键）：{calls:?}"
    );
    assert!(calls.contains(&Call::EscGuard(true)));
    assert_eq!(r.engine.queue_status(), Some(status(true, 1, 3)));
    // 默认拦下 ⌘V
    let keys = r.port.hooks.keys.lock().expect("粘贴键");
    assert!(keys.find(cmd_v().modifiers, "KeyV").is_some());

    // 第二次按键：写入第二条（带自写抑制，不发键）→ 重放
    assert!(r.port.hooks.press(cmd_v()));
    assert_eq!(last_written(&r).as_deref(), Some("second"));
    let calls = r.calls();
    assert!(
        matches!(calls.as_slice(), [Call::Write, Call::Suppress(_), Call::Replay("KeyV")]),
        "写入 → 抑制 → 重放，不再隐藏面板、不经 paste_into：{calls:?}"
    );
    assert_eq!(r.engine.queue_status(), Some(status(true, 2, 3)));

    // 第三次：最后一条粘贴出去后立即结束，钩子与全局 Esc 一并卸载
    assert!(r.port.hooks.press(cmd_v()));
    assert_eq!(last_written(&r).as_deref(), Some("third"));
    let calls = r.calls();
    assert!(calls.contains(&Call::Replay("KeyV")));
    assert!(
        calls.contains(&Call::HookRemove) && calls.contains(&Call::EscGuard(false)),
        "{calls:?}"
    );
    assert_eq!(r.engine.queue_status(), None);
    assert!(!r.port.hooks.installed(), "队列之外粘贴键绝不被拦下");
    // 进度：开始、第二条、结束（最后一条也提示一次）
    let events = std::mem::take(&mut *r.host.queue_events.lock());
    assert_eq!(
        events,
        vec![
            (status(true, 1, 3), true),
            (status(true, 2, 3), true),
            (status(false, 3, 3), true),
        ]
    );
    // 粘贴出去的条目都被 touch，片段与合并之外的整条粘贴不新增历史
    assert_eq!(r.ids().len(), 3);
    assert_eq!(r.top().id, c);
}

#[test]
fn a_first_item_that_was_only_copied_is_pasted_by_the_next_key() {
    let r = rig();
    let [a, b, _] = three(&r);
    // 第一条发键失败（焦点没交还成功）：只复制，留在剪贴板里
    *r.port.paste.paste_result.lock() = Err(PasteError::FocusTimeout);
    let s = start(&r, vec![a, b]).expect("开始");
    assert_eq!(s, status(true, 0, 2), "第一条还没粘贴出去");
    assert_eq!(
        r.host.queue_events.lock().as_slice(),
        &[(status(true, 0, 2), false)],
        "说明原因的 toast 不被进度换掉"
    );
    *r.port.paste.paste_result.lock() = Ok(());
    r.calls();
    // 下一次按键只重放：粘贴的正是第一条，不跳过它
    assert!(r.port.hooks.press(cmd_v()));
    assert_eq!(r.calls(), vec![Call::Replay("KeyV")]);
    assert_eq!(last_written(&r).as_deref(), Some("first"));
    assert_eq!(r.engine.queue_status(), Some(status(true, 1, 2)));
    // 再按：第二条，粘贴完结束
    assert!(r.port.hooks.press(cmd_v()));
    assert_eq!(last_written(&r).as_deref(), Some("second"));
    assert_eq!(r.engine.queue_status(), None);
}

#[test]
fn every_way_of_ending_releases_the_hook_and_escape() {
    type End = fn(&Rig);
    let ends: [(&str, End); 4] = [
        ("取消", |r| r.engine.queue_end(QueueEnd::Cancelled)),
        ("Esc", |r| assert!(r.engine.queue_escape())),
        ("锁屏", |r| r.port.hooks.signal(InputSignal::SessionLocked)),
        ("新队列替换", |r| {
            // 用户又打开面板，从头开始另一个队列
            r.reopen();
            let ids = r.ids();
            start(r, ids).expect("新队列");
            assert_eq!(r.engine.queue_status().map(|s| s.total), Some(3));
        }),
    ];
    for (why, end) in ends {
        let r = rig();
        let [a, b, c] = three(&r);
        start(&r, vec![a, b, c]).expect("开始");
        r.calls();
        r.host.queue_events.lock().clear();
        end(&r);
        let calls = r.calls();
        assert!(calls.contains(&Call::HookRemove), "{why}：钩子卸载 {calls:?}");
        assert!(calls.contains(&Call::EscGuard(false)), "{why}：全局 Esc 注销 {calls:?}");
        let ended = r.host.queue_events.lock().first().copied().expect("推送结束");
        assert_eq!(ended, (status(false, 1, 3), false), "{why}：推送结束，不弹 toast");
        if why != "新队列替换" {
            assert_eq!(r.engine.queue_status(), None, "{why}");
            assert!(!r.port.hooks.installed(), "{why}");
            assert!(!r.port.hooks.press(cmd_v()), "{why}：之后的按键照常交给应用");
        }
    }
    // 没有队列时 Esc 交给面板
    let r = rig();
    assert!(!r.engine.queue_escape());
}

#[test]
fn idle_queues_end_on_the_timer_and_on_a_late_key() {
    let r = rig();
    let [a, b, c] = three(&r);
    start(&r, vec![a, b, c]).expect("开始");
    // 定时器到点但还没超时（墙钟没走够）：按剩余时间再排一次，同一时刻只有一个定时器
    assert_eq!(r.exec.fire_timers(), 1);
    assert_eq!(r.engine.queue_status().map(|s| s.active), Some(true));
    // 墙钟走过 10 分钟
    r.host
        .now
        .fetch_add(i64::try_from(QUEUE_IDLE.as_millis()).expect("ms"), Ordering::SeqCst);
    assert_eq!(r.exec.fire_timers(), 1);
    assert_eq!(r.engine.queue_status(), None);
    assert!(r.calls().contains(&Call::HookRemove));

    // 睡眠让定时器迟到：醒来后的第一次按键按墙钟判定超时，结束队列并照常重放这次按键（粘贴剪贴板里现有的内容）
    let r = rig();
    let [a, b, c] = three(&r);
    start(&r, vec![a, b, c]).expect("开始");
    let before = last_written(&r);
    r.host
        .now
        .fetch_add(i64::try_from(QUEUE_IDLE.as_millis()).expect("ms"), Ordering::SeqCst);
    r.calls();
    assert!(r.port.hooks.press(cmd_v()));
    let calls = r.calls();
    assert_replayed_then_removed(&calls);
    assert!(!calls.contains(&Call::Write), "不再写入下一条：{calls:?}");
    assert_eq!(last_written(&r), before);
    assert_eq!(r.engine.queue_status(), None);
}

/// 结束并照常放行这次按键时，先重放、再卸载钩子：X11 的重放临时撤销抓取再抓回，先卸载会让重放把粘贴键抓回去，
/// 之后粘贴键在整个桌面失效
fn assert_replayed_then_removed(calls: &[Call]) {
    let replay = calls.iter().position(|c| *c == Call::Replay("KeyV"));
    let remove = calls.iter().position(|c| *c == Call::HookRemove);
    assert!(matches!((replay, remove), (Some(a), Some(b)) if a < b), "{calls:?}");
}

#[test]
fn keys_whose_signal_arrives_after_the_end_are_still_pasted() {
    let r = rig();
    let [a, b, c] = three(&r);
    start(&r, vec![a, b, c]).expect("开始");
    let before = last_written(&r);
    // 按键已被钩子拦下、信号还在路上时队列结束（Esc、托盘）
    r.exec.queue(true);
    assert!(r.port.hooks.press(cmd_v()));
    r.engine.queue_end(QueueEnd::Cancelled);
    r.calls();
    r.exec.run_jobs();
    // 这一下不凭空消失：照常重放，粘贴剪贴板里现有的内容，不写入下一条
    assert_eq!(r.calls(), vec![Call::Replay("KeyV")]);
    assert_eq!(last_written(&r), before);
}

#[test]
fn keys_pressed_while_a_step_is_in_progress_are_dropped() {
    let r = rig();
    let [a, b, c] = three(&r);
    start(&r, vec![a, b, c]).expect("开始");
    // 钩子信号转交到阻塞线程：排队模式下两次按键都排着，第一个执行时第二个被丢弃
    r.exec.queue(true);
    assert!(r.port.hooks.press(cmd_v()));
    assert!(r.port.hooks.press(cmd_v()));
    // 第一个任务执行到一半（写入期间）时第二个任务开始：用写入钩子模拟
    let engine = r.engine.clone();
    let exec = r.exec.clone();
    *r.port.paste.during_write.lock() = Some(Box::new(move || {
        exec.run_jobs();
        let _ = engine;
    }));
    r.calls();
    r.exec.run_jobs();
    let replays = r.calls().iter().filter(|c| matches!(c, Call::Replay(_))).count();
    assert_eq!(replays, 1, "进行中再按的那一下被丢弃（已被拦下，什么都不发生）");
    assert_eq!(r.engine.queue_status(), Some(status(true, 2, 3)));
}

#[test]
fn deleted_items_are_skipped_and_a_failed_replay_ends_the_queue() {
    let r = rig();
    let [a, b, c] = three(&r);
    start(&r, vec![a, b, c]).expect("开始");
    // 第二条在队列期间被删除：跳到第三条
    r.engine.delete(b, true).expect("删除");
    assert!(r.port.hooks.press(cmd_v()));
    assert_eq!(last_written(&r).as_deref(), Some("third"));
    assert_eq!(r.engine.queue_status(), None, "第三条是最后一条");

    // 剩下的都已删除：结束，这次按键照常粘贴现有的内容（先重放、再卸载钩子）
    let r = rig();
    let [a, b, c] = three(&r);
    start(&r, vec![a, b, c]).expect("开始");
    r.engine.delete(b, true).expect("删除");
    r.engine.delete(c, true).expect("删除");
    r.calls();
    assert!(r.port.hooks.press(cmd_v()));
    assert_replayed_then_removed(&r.calls());
    assert_eq!(r.engine.queue_status(), None);

    // 重放失败（例如 XTEST 不可用）：结束队列，不让粘贴键一直被拦着
    let r = rig();
    let [a, b, c] = three(&r);
    start(&r, vec![a, b, c]).expect("开始");
    *r.port.hooks.replay_error.lock() = Some(HookError::Other("boom".into()));
    assert!(r.port.hooks.press(cmd_v()));
    assert_eq!(r.engine.queue_status(), None);
    assert!(r.calls().contains(&Call::HookRemove));
}

#[test]
fn queues_that_cannot_work_do_not_start() {
    // 平台层报告不可用（Wayland 等）：不粘贴、不装钩子，原因 key 原样返回
    let r = rig();
    let [a, b, _] = three(&r);
    *r.port.hooks.unavailable.lock() = Some("errors.platform.wayland_input_hooks".into());
    let e = start(&r, vec![a, b]).expect_err("不可用");
    assert_eq!(
        (e.code, e.message.as_str()),
        (IpcErrorCode::Platform, "errors.platform.wayland_input_hooks")
    );
    assert!(r.calls().is_empty());

    // 钩子装不上（缺少权限）：第一条已经粘贴出去，队列不开始，错误交给调用方
    let r = rig();
    let [a, b, _] = three(&r);
    *r.port.hooks.install_error.lock() = Some(HookError::PermissionMissing(wisp_types::PermissionKind::Accessibility));
    let e = start(&r, vec![a, b]).expect_err("装不上");
    assert_eq!(e.code, IpcErrorCode::Permission);
    assert_eq!(r.engine.queue_status(), None);
    assert!(!r.calls().contains(&Call::EscGuard(true)));

    // 只有一条：粘贴完就结束，根本不装钩子
    let r = rig();
    let [a, _, _] = three(&r);
    assert_eq!(start(&r, vec![a]).expect("一条"), status(false, 1, 1));
    assert!(!r.calls().contains(&Call::HookInstall));

    // 第一条因缺少辅助功能权限只复制：发不了键的环境不开始（toast 已经说明原因）
    let r = rig();
    let [a, b, _] = three(&r);
    *r.port.paste.paste_result.lock() = Err(PasteError::PermissionMissing);
    let s = start(&r, vec![a, b]).expect("只复制");
    assert!(!s.active);
    assert!(!r.calls().contains(&Call::HookInstall));
}

#[test]
fn paste_keys_include_mapped_paste_keys_for_this_platform() {
    let keymaps = vec![
        AppKeymap {
            os: OsKind::Linux,
            app_id: "org.gnome.Terminal".into(),
            copy: Some("Ctrl+Shift+C".into()),
            paste: Some("Ctrl+Shift+V".into()),
        },
        AppKeymap {
            os: OsKind::Linux,
            app_id: "kitty".into(),
            copy: None,
            paste: Some("Ctrl+Shift+V".into()),
        },
        AppKeymap {
            os: OsKind::Linux,
            app_id: "skip".into(),
            copy: None,
            paste: Some("none".into()),
        },
        AppKeymap {
            os: OsKind::Macos,
            app_id: "elsewhere".into(),
            copy: None,
            paste: Some("Super+Shift+V".into()),
        },
    ];
    let keys = paste_keys(&keymaps, OsKind::Linux);
    let got: Vec<KeyChord> = keys.iter().collect();
    assert_eq!(
        got,
        vec![chord("Ctrl+KeyV"), chord("Ctrl+Shift+KeyV")],
        "默认键在前、去重、只取本平台、跳过不发送"
    );
    let mac = paste_keys(&[], OsKind::Macos);
    assert_eq!(mac.iter().collect::<Vec<_>>(), vec![cmd_v()]);
}

#[test]
fn the_floating_clipboard_can_start_a_queue_too() {
    let r = rig();
    let [a, b, _] = three(&r);
    *r.port.focus.frontmost.lock() = Some(app("front"));
    let s = r
        .engine
        .queue_start(vec![a, b], PasteHost::Floating, Instant::now())
        .expect("开始");
    assert_eq!(s, status(true, 1, 2));
    let calls = r.calls();
    assert!(
        calls.contains(&Call::FloatYield("front".into())),
        "第一条照悬浮窗的规矩交还焦点：{calls:?}"
    );
    assert!(!calls.contains(&Call::Hide), "悬浮窗不隐藏");
}

#[test]
fn progress_replaces_the_previous_progress_toast() {
    use super::port::queue_toast_shown;
    use wisp_types::PanelView;
    assert!(queue_toast_shown(None, false));
    // 上一次的进度 toast 还在：换成新的进度，不停在「1 / 5」
    assert!(queue_toast_shown(Some(PanelView::Toast), false));
    // 用户打开的面板不被 toast 换掉；悬浮剪贴板开着时进度在它里面
    assert!(!queue_toast_shown(Some(PanelView::Clipboard), false));
    assert!(!queue_toast_shown(None, true));
}
