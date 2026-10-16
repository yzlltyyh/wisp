//! 统一粘贴管线的单测：每种来源（整条、「粘贴为…」变换、片段、多条合并）× 每种宿主（面板的剪贴板 / OCR 视图、悬浮
//! 剪贴板、队列）。
//! 不调用真实平台，用 `tests.rs` 的测试替身；断言写入了什么、调用的先后、touch 了哪些条目、谁来提示。

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use parking_lot::Mutex;
use wisp_platform::{ClipboardAccess, PasteError};
use wisp_types::{ClipKind, CopiedReason, OsKind, PanelView, PasteMode, PasteOutcome, RawClipboard, TransformId};

use crate::error::{IpcError, IpcErrorCode};
use crate::windows::panel::PANEL_LABEL;

use super::paste::{FLOAT_LABEL, PasteHost, PasteKind, PasteRequest, PasteSource, PasteStage};
use super::port::ClipView;
use super::tests::{Call, ICON_64, Rig, app, panel_item, rig, rig_with};

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

const KINDS: [PasteKind; 2] = [PasteKind::Items, PasteKind::Fragment];

#[test]
fn hosts_come_from_the_calling_window_and_the_view_it_shows() {
    let panel = |view| Some((3, view));
    // 剪贴板视图：条目与片段都能粘贴
    for kind in KINDS {
        assert_eq!(
            PasteHost::of_window(PANEL_LABEL, panel(PanelView::Clipboard), kind).expect("剪贴板视图"),
            PasteHost::Panel { session: 3 },
            "{kind:?}"
        );
    }
    // OCR 视图：识别出的片段粘贴回原应用；条目（整条、变换、合并、连续粘贴、交接悬浮窗）只属于剪贴板视图
    assert_eq!(
        PasteHost::of_window(PANEL_LABEL, panel(PanelView::Ocr), PasteKind::Fragment).expect("OCR 的片段"),
        PasteHost::Panel { session: 3 }
    );
    let err = PasteHost::of_window(PANEL_LABEL, panel(PanelView::Ocr), PasteKind::Items).expect_err("OCR 的条目");
    assert_eq!(err.code, IpcErrorCode::Conflict);
    // 其他视图、面板没在显示：conflict（前端静默）
    for kind in KINDS {
        for view in [PanelView::Translate, PanelView::Input, PanelView::Toast] {
            let err = PasteHost::of_window(PANEL_LABEL, panel(view), kind).expect_err("其他视图");
            assert_eq!(err.code, IpcErrorCode::Conflict, "{view:?} {kind:?}");
        }
        let err = PasteHost::of_window(PANEL_LABEL, None, kind).expect_err("面板没在显示");
        assert_eq!(err.code, IpcErrorCode::Conflict, "{kind:?}");
    }
    // 悬浮剪贴板不依赖面板的会话与视图
    for kind in KINDS {
        for shown in [None, panel(PanelView::Clipboard), panel(PanelView::Ocr)] {
            assert_eq!(
                PasteHost::of_window(FLOAT_LABEL, shown, kind).expect("悬浮"),
                PasteHost::Floating,
                "{shown:?} {kind:?}"
            );
        }
    }
    // 其余窗口（包括显示 OCR 结果的固定窗口）不能粘贴，与面板显示着什么无关
    for other in ["settings", "pinned-1", "overlay-0", "onboarding"] {
        for kind in KINDS {
            for shown in [None, panel(PanelView::Clipboard), panel(PanelView::Ocr)] {
                let err = PasteHost::of_window(other, shown, kind).expect_err(other);
                assert_eq!(err.code, IpcErrorCode::InvalidArgument, "{other} {shown:?} {kind:?}");
            }
        }
    }
}

#[test]
fn only_fragments_count_as_fragments() {
    assert_eq!(item(1).kind(), PasteKind::Items);
    let transformed = PasteSource::Item {
        id: 1,
        transform: Some(TransformId::Upper),
    };
    assert_eq!(transformed.kind(), PasteKind::Items);
    assert_eq!(PasteSource::Many { ids: vec![1, 2] }.kind(), PasteKind::Items);
    let fragment = PasteSource::Fragment {
        from: Some(1),
        text: "x".into(),
    };
    assert_eq!(fragment.kind(), PasteKind::Fragment);
}

fn loose_fragment(text: &str) -> PasteSource {
    PasteSource::Fragment {
        from: None,
        text: text.into(),
    }
}

#[test]
fn ocr_results_paste_fragments_back_into_the_original_app() {
    let r = rig();
    // 识别完成时自动复制进历史的全文
    let full = r.copy_text(101, "Order 482915 ships today");
    r.show_ocr();
    r.calls();
    let out = r
        .paste_from(
            PANEL_LABEL,
            loose_fragment("482915"),
            PasteMode::Normal,
            Some(app("editor")),
        )
        .expect("粘贴");
    assert_eq!(out, PasteOutcome::Pasted);
    assert_eq!(last_written(&r).as_deref(), Some("482915"));
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
        "与剪贴板面板的芯片相同：写入、收起面板、等它离屏，再向原应用发粘贴键"
    );
    assert_eq!(r.ids(), vec![full.id], "片段不进历史");
    assert_eq!(r.top().use_count, 1, "识别结果里的片段不属于任何条目，不 touch");
    assert!(r.host.ocr_session.lock().is_none(), "面板已收起");
    assert_eq!(r.engine.paste_gate.stage(), PasteStage::Idle);
}

#[test]
fn an_ocr_result_hands_off_only_once() {
    // 第二次单击到达时面板还没反映出隐藏（同一会话仍可见）：按会话去重，不写第二次
    let r = rig();
    let session = r.show_ocr();
    r.host.hide_ends_session.store(false, Ordering::SeqCst);
    r.calls();
    r.paste_from(PANEL_LABEL, loose_fragment("a"), PasteMode::Normal, Some(app("editor")))
        .expect("粘贴");
    let err = r
        .paste_from(PANEL_LABEL, loose_fragment("b"), PasteMode::Normal, Some(app("editor")))
        .expect_err("同一会话");
    assert_eq!(err.code, IpcErrorCode::Conflict);
    assert_eq!(r.port.paste.writes.lock().len(), 1);
    assert_eq!(last_written(&r).as_deref(), Some("a"));
    // 命令层推出宿主之后面板换成了别的会话：同样拒绝
    let next = r.show_ocr();
    assert_ne!(next, session);
    let err = r
        .engine
        .paste(PasteRequest {
            source: loose_fragment("c"),
            mode: PasteMode::Normal,
            host: PasteHost::Panel { session },
            selected_at: Instant::now(),
        })
        .expect_err("会话已变");
    assert_eq!(err.code, IpcErrorCode::Conflict);
    assert_eq!(r.port.paste.writes.lock().len(), 1);
}

#[test]
fn ocr_results_do_not_host_item_pastes() {
    let r = rig();
    let a = r.copy_text(101, "first");
    let b = r.copy_text(102, "second");
    let session = r.show_ocr();
    r.calls();
    // 命令层就会拒绝；绕过它直接带着 OCR 会话进入管线的条目粘贴也在开始前被拒绝
    let err = r
        .paste_from(PANEL_LABEL, item(a.id), PasteMode::Normal, Some(app("editor")))
        .expect_err("命令层");
    assert_eq!(err.code, IpcErrorCode::Conflict);
    for source in [
        item(a.id),
        PasteSource::Item {
            id: a.id,
            transform: Some(TransformId::Upper),
        },
        PasteSource::Many { ids: vec![a.id, b.id] },
    ] {
        let err = r
            .engine
            .paste(PasteRequest {
                source: source.clone(),
                mode: PasteMode::Normal,
                host: PasteHost::Panel { session },
                selected_at: Instant::now(),
            })
            .expect_err("管线");
        assert_eq!(err.code, IpcErrorCode::Conflict, "{source:?}");
    }
    assert!(r.calls().is_empty(), "没有写入、没有隐藏");
    assert_eq!(r.ids(), vec![b.id, a.id], "没有 touch");
    // 面板仍显示着 OCR 结果，片段照常粘贴
    r.paste_from(PANEL_LABEL, loose_fragment("x"), PasteMode::Normal, Some(app("editor")))
        .expect("片段");
}

#[test]
fn ocr_results_without_a_known_app_copy_and_say_so() {
    let r = rig();
    r.show_ocr();
    r.calls();
    let out = r
        .paste_from(PANEL_LABEL, loose_fragment("x"), PasteMode::Normal, None)
        .expect("只复制");
    assert_eq!(
        out,
        PasteOutcome::Copied {
            reason: CopiedReason::Manual
        }
    );
    let change = r.port.clipboard.change_id();
    assert_eq!(
        r.calls(),
        vec![
            Call::Write,
            Call::Suppress(change),
            Call::Hide,
            Call::Toast(CopiedReason::Manual)
        ],
        "收起面板，在原位置提示按粘贴键"
    );
}

#[test]
fn windows_hands_the_foreground_back_before_hiding_an_ocr_result() {
    // OCR 面板默认不抢焦点，用户按下卡片后才成为前台：无论哪种情形，都先把前台交还原应用，再收起面板、发键
    let r = rig_with(OsKind::Windows, tempfile::tempdir().expect("临时目录"));
    r.show_ocr();
    r.calls();
    let out = r
        .paste_from(
            PANEL_LABEL,
            loose_fragment("482915"),
            PasteMode::Normal,
            Some(app("notepad.exe")),
        )
        .expect("粘贴");
    assert_eq!(out, PasteOutcome::Pasted);
    let change = r.port.clipboard.change_id();
    assert_eq!(
        r.calls(),
        vec![
            Call::Write,
            Call::Suppress(change),
            Call::Activate("notepad.exe".into()),
            Call::Hide,
            Call::PasteInto { plain: false },
            Call::Perf("send_input".into()),
        ]
    );
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
