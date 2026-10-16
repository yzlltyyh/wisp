//! 统一粘贴管线的单测：每种来源（整条、「粘贴为…」变换、片段、多条合并）× 每种宿主（面板、悬浮剪贴板、队列）。
//! 不调用真实平台，用 `tests.rs` 的测试替身；断言写入了什么、调用的先后、touch 了哪些条目、谁来提示。

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use parking_lot::Mutex;
use wisp_platform::{ClipboardAccess, PasteError};
use wisp_types::{ClipKind, CopiedReason, PasteMode, PasteOutcome, RawClipboard, TransformId};

use crate::error::{IpcError, IpcErrorCode};
use crate::windows::panel::PANEL_LABEL;

use super::paste::{FLOAT_LABEL, PasteHost, PasteRequest, PasteSource, PasteStage};
use super::port::ClipView;
use super::tests::{Call, ICON_64, Rig, app, panel_item, rig};

/// 最近一次写入剪贴板的文本
fn last_written(r: &Rig) -> Option<String> {
    r.port.paste.writes.lock().last().and_then(|p| p.text.clone())
}

fn item(id: i64) -> PasteSource {
    PasteSource::Item { id, transform: None }
}

fn queue_req(source: PasteSource) -> PasteRequest {
    PasteRequest {
        source,
        mode: PasteMode::Normal,
        host: PasteHost::Queue,
        selected_at: Instant::now(),
    }
}

// ---------- 来源 ----------

#[test]
fn transformed_items_paste_the_result_without_a_new_history_entry() {
    let r = rig();
    let a = r.copy_text(101, "hello wisp");
    r.copy_text(102, "other");
    r.calls();
    let source = PasteSource::Item {
        id: a.id,
        transform: Some(TransformId::Upper),
    };
    let out = r
        .paste_from(PANEL_LABEL, source, PasteMode::Normal, Some(app("t")))
        .expect("粘贴");
    assert_eq!(out, PasteOutcome::Pasted);
    assert_eq!(last_written(&r).as_deref(), Some("HELLO WISP"));
    let change = r.port.clipboard.change_id();
    assert_eq!(
        r.calls(),
        vec![
            Call::Write,
            Call::Suppress(change),
            Call::Hide,
            Call::PasteInto { plain: false },
            Call::Perf("cg_event".into()),
        ],
        "变换结果同样带自写抑制，交接与整条粘贴相同"
    );
    // 不进历史：条数不变，来源条目被 touch 到顶部
    assert_eq!(r.ids().len(), 2);
    assert_eq!(r.top().id, a.id);
}

#[test]
fn a_transform_that_does_not_apply_fails_before_writing() {
    let r = rig();
    let a = r.copy_text(101, "not json at all");
    r.calls();
    let source = PasteSource::Item {
        id: a.id,
        transform: Some(TransformId::JsonPretty),
    };
    let err = r
        .paste_from(PANEL_LABEL, source, PasteMode::Normal, Some(app("t")))
        .expect_err("不适用");
    assert_eq!(err.code, IpcErrorCode::InvalidArgument);
    assert!(r.calls().is_empty(), "没有写入、没有隐藏");
    assert_eq!(r.engine.paste_gate.stage(), PasteStage::Idle);
    // 面板仍在，同一会话可以换一种方式粘贴
    let out = r.paste(a.id, PasteMode::Normal, Some(app("t"))).expect("粘贴");
    assert_eq!(out, PasteOutcome::Pasted);
}

#[test]
fn fragments_paste_their_text_and_touch_only_their_item() {
    let r = rig();
    let a = r.copy_text(101, "Your verification code is 482915");
    let b = r.copy_text(102, "later");
    let fragment = PasteSource::Fragment {
        from: Some(a.id),
        text: "482915".into(),
    };
    let out = r
        .paste_from(PANEL_LABEL, fragment, PasteMode::Normal, Some(app("t")))
        .expect("粘贴");
    assert_eq!(out, PasteOutcome::Pasted);
    assert_eq!(last_written(&r).as_deref(), Some("482915"));
    assert_eq!(r.ids(), vec![a.id, b.id], "片段不成为新条目，所在条目移到顶部");
    assert_eq!(r.top().use_count, 2);

    // 不知道来自哪条（识别结果里的片段）：只写入，不 touch
    r.reopen();
    let loose = PasteSource::Fragment {
        from: None,
        text: "free text".into(),
    };
    r.paste_from(PANEL_LABEL, loose, PasteMode::Normal, Some(app("t")))
        .expect("粘贴");
    assert_eq!(last_written(&r).as_deref(), Some("free text"));
    assert_eq!(r.ids(), vec![a.id, b.id]);
    assert_eq!(r.top().use_count, 2);
}

#[test]
fn merged_pastes_follow_the_pick_order_and_skip_items_without_text() {
    let r = rig();
    let a = r.copy_text(101, "one");
    r.copy(
        102,
        RawClipboard {
            change_id: 102,
            image_png: Some(ICON_64.to_vec()),
            ..RawClipboard::default()
        },
    );
    let img = r.top();
    assert_eq!(img.kind, ClipKind::Image);
    let b = r.copy_text(103, "two");
    let c = r.copy_text(104, "untouched");
    let many = PasteSource::Many {
        ids: vec![b.id, img.id, a.id],
    };
    let out = r
        .paste_from(PANEL_LABEL, many, PasteMode::Normal, Some(app("t")))
        .expect("粘贴");
    assert_eq!(out, PasteOutcome::Pasted);
    assert_eq!(last_written(&r).as_deref(), Some("two\none"), "图片没有文本，跳过");
    // 选中的条目都被 touch，最先选的排在最上面；合并结果不进历史
    assert_eq!(r.ids(), vec![b.id, img.id, a.id, c.id]);

    // 一条可粘贴的都没有：not_found，不写入
    r.reopen();
    r.calls();
    let gone = PasteSource::Many {
        ids: vec![img.id, 9999],
    };
    let err = r
        .paste_from(PANEL_LABEL, gone, PasteMode::Normal, Some(app("t")))
        .expect_err("没有文本");
    assert_eq!(err.code, IpcErrorCode::NotFound);
    assert!(r.calls().is_empty());
}

// ---------- 宿主 ----------

#[test]
fn hosts_come_from_the_calling_window() {
    assert_eq!(
        PasteHost::of_window(PANEL_LABEL, Some(3)).expect("面板"),
        PasteHost::Panel { session: 3 }
    );
    let err = PasteHost::of_window(PANEL_LABEL, None).expect_err("面板没显示剪贴板");
    assert_eq!(err.code, IpcErrorCode::Conflict);
    // 悬浮剪贴板不依赖面板会话
    assert_eq!(
        PasteHost::of_window(FLOAT_LABEL, None).expect("悬浮"),
        PasteHost::Floating
    );
    for other in ["settings", "pinned-1", "overlay-0", "onboarding"] {
        let err = PasteHost::of_window(other, Some(1)).expect_err(other);
        assert_eq!(err.code, IpcErrorCode::InvalidArgument, "{other}");
    }
}

#[test]
fn a_panel_request_for_a_session_no_longer_showing_is_rejected() {
    let r = rig();
    let a = r.copy_text(101, "x");
    r.calls();
    // 命令层取到的是会话 7，进入管线时面板显示的已经是会话 1
    let err = r.engine.paste(panel_item(7, a.id)).expect_err("会话已变");
    assert_eq!(err.code, IpcErrorCode::Conflict);
    assert!(r.calls().is_empty());
}

#[test]
fn floating_pastes_without_hiding_after_yielding_the_focus() {
    let r = rig();
    let a = r.copy_text(101, "first");
    let b = r.copy_text(102, "second");
    // 面板没有显示：悬浮窗照样能粘贴
    r.host.set_view(ClipView::Hidden);
    r.calls();
    let out = r
        .paste_from(FLOAT_LABEL, item(a.id), PasteMode::Normal, Some(app("editor")))
        .expect("粘贴");
    assert_eq!(out, PasteOutcome::Pasted);
    let change = r.port.clipboard.change_id();
    assert_eq!(
        r.calls(),
        vec![
            Call::Write,
            Call::Suppress(change),
            Call::FloatYield("editor".into()),
            Call::PasteInto { plain: false },
            Call::Perf("cg_event".into()),
        ],
        "不隐藏任何窗口；先交还键盘焦点再发键"
    );
    // 没有会话：可以连续粘贴，不受「一个会话只粘贴一次」的限制
    let out = r
        .paste_from(FLOAT_LABEL, item(b.id), PasteMode::Plain, Some(app("editor")))
        .expect("再次粘贴");
    assert_eq!(out, PasteOutcome::Pasted);
    assert!(r.calls().contains(&Call::PasteInto { plain: true }));
    assert_eq!(r.top().id, b.id);
}

#[test]
fn floating_copy_only_outcomes_are_reported_without_a_toast() {
    let r = rig();
    let a = r.copy_text(101, "x");
    r.calls();
    let paste = |mode, target| r.paste_from(FLOAT_LABEL, item(a.id), mode, target).expect("粘贴");
    // 交还不了焦点：只复制（发键会落进悬浮窗自己）
    r.host.float_yields.store(false, Ordering::SeqCst);
    assert_eq!(
        paste(PasteMode::Normal, Some(app("editor"))),
        PasteOutcome::Copied {
            reason: CopiedReason::FocusTimeout
        }
    );
    assert!(!r.calls().iter().any(|c| matches!(c, Call::PasteInto { .. })));
    r.host.float_yields.store(true, Ordering::SeqCst);
    // ⌘↩ 仅复制、不知道前台应用、发键失败
    assert_eq!(
        paste(PasteMode::CopyOnly, Some(app("editor"))),
        PasteOutcome::Copied {
            reason: CopiedReason::CopyOnly
        }
    );
    assert_eq!(
        paste(PasteMode::Normal, None),
        PasteOutcome::Copied {
            reason: CopiedReason::Manual
        }
    );
    *r.port.paste.paste_result.lock() = Err(PasteError::TargetGone);
    assert_eq!(
        paste(PasteMode::Normal, Some(app("editor"))),
        PasteOutcome::Copied {
            reason: CopiedReason::TargetGone
        }
    );
    // 悬浮窗在自己的窗口里提示：Rust 不弹 toast、不隐藏面板
    let calls = r.calls();
    assert!(r.host.toasts.lock().is_empty(), "{calls:?}");
    assert!(!calls.contains(&Call::Hide));
    assert_eq!(r.engine.paste_gate.stage(), PasteStage::Idle);
}

#[test]
fn queue_steps_write_and_touch_and_leave_the_keys_to_the_queue() {
    let r = rig();
    let a = r.copy_text(101, "first");
    r.copy_text(102, "second");
    r.calls();
    let out = r.engine.paste(queue_req(item(a.id))).expect("队列");
    assert_eq!(out, PasteOutcome::Pasted);
    let change = r.port.clipboard.change_id();
    assert_eq!(
        r.calls(),
        vec![Call::Write, Call::Suppress(change)],
        "用户按下的粘贴键由队列重放：不隐藏、不发键、不提示"
    );
    assert_eq!(r.top().id, a.id);
    assert_eq!(r.engine.paste_gate.stage(), PasteStage::Idle);
}

#[test]
fn every_host_shares_the_single_flight_gate() {
    let r = rig();
    let a = r.copy_text(101, "first");
    let b = r.copy_text(102, "second");
    // 悬浮窗写入期间（Windows 上可能卡 1.5 s）面板发来一次粘贴：拒绝，不写入第二次
    let engine = Arc::clone(&r.engine);
    let second: Arc<Mutex<Option<Result<PasteOutcome, IpcError>>>> = Arc::default();
    let slot = Arc::clone(&second);
    *r.port.paste.during_write.lock() = Some(Box::new(move || {
        *slot.lock() = Some(engine.paste(panel_item(1, b.id)));
    }));
    let out = r
        .paste_from(FLOAT_LABEL, item(a.id), PasteMode::Normal, Some(app("editor")))
        .expect("粘贴");
    assert_eq!(out, PasteOutcome::Pasted);
    let err = second.lock().take().expect("第二次请求").expect_err("被拒绝");
    assert_eq!(err.code, IpcErrorCode::Conflict);
    assert_eq!(r.port.paste.writes.lock().len(), 1);
    // 悬浮窗的粘贴不占用面板会话：面板随后照常粘贴
    let out = r.paste(b.id, PasteMode::Normal, Some(app("t"))).expect("面板粘贴");
    assert_eq!(out, PasteOutcome::Pasted);
}
