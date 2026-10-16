//! 悬浮剪贴板在剪贴板服务一侧的接线：它就绪（页面已订阅）之后 `clipboard:changed` 同时推给它、投递视为「已显示」；
//! 就绪之前与关闭之后不推。拖出条目的拖拽数据（`drag.rs`）。粘贴管线的悬浮窗宿主见 `tests_paste.rs`。

use std::sync::atomic::Ordering;

use wisp_types::{ClipKind, ClipboardChanged, RawClipboard};

use super::outbox::Delivery;
use super::port::ClipView;
use super::tests::rig;
use super::watch::Capture;

#[test]
fn a_ready_floating_clipboard_receives_every_change() {
    let r = rig();
    r.engine.preload();
    // 面板隐藏、悬浮窗还没就绪：谁都不推
    r.host.set_view(ClipView::Hidden);
    assert_eq!(r.engine.delivery(), Delivery::None);
    r.copy_text(101, "before ready");
    assert!(r.host.float_events.lock().is_empty());

    // 就绪之后：每条改动都推给它，投递视为已显示（远端改动因此逐条推送，不只发 reloaded）
    r.host.float_ready.store(true, Ordering::SeqCst);
    assert_eq!(r.engine.delivery(), Delivery::Shown);
    let item = r.copy_text(102, "while floating");
    let got = std::mem::take(&mut *r.host.float_events.lock());
    assert!(
        matches!(got.as_slice(), [ClipboardChanged::Inserted { item: i }] if i.id == item.id),
        "{got:?}"
    );

    // 面板同时显示：两边各推一份，互不影响
    r.host.set_view(ClipView::Shown(3));
    r.events();
    let both = r.copy_text(103, "both");
    let to_float = std::mem::take(&mut *r.host.float_events.lock());
    assert!(
        matches!(to_float.as_slice(), [ClipboardChanged::Inserted { item: i }] if i.id == both.id),
        "{to_float:?}"
    );
    assert_eq!(r.events().last(), to_float.last(), "面板照常收到同一条");

    // 关闭之后不再推
    r.host.float_ready.store(false, Ordering::SeqCst);
    r.host.set_view(ClipView::Hidden);
    r.host.float_events.lock().clear();
    r.copy_text(104, "after close");
    assert!(r.host.float_events.lock().is_empty());
}

#[test]
fn drag_data_matches_what_a_paste_would_write() {
    let r = rig();
    let plain = r.copy_text(101, "plain words");
    let link = r.copy_text(102, "https://example.com/a?b=1");
    assert_eq!(link.kind, ClipKind::Link);
    let raw = RawClipboard {
        change_id: 103,
        text: Some("Hello Wisp".into()),
        html: Some("<p>Hello <b>Wisp</b></p>".into()),
        ..RawClipboard::default()
    };
    assert_eq!(r.copy(103, raw), Capture::Done);
    let rich = r.top();
    assert_eq!(rich.kind, ClipKind::RichText);

    let d = r.engine.drag_data(&[plain.id]).expect("纯文本");
    assert_eq!((d.text.as_str(), d.html, d.uri), ("plain words", None, None));
    let d = r.engine.drag_data(&[link.id]).expect("链接");
    assert_eq!(
        d.uri.as_deref(),
        Some("https://example.com/a?b=1"),
        "链接另带 text/uri-list"
    );
    let d = r.engine.drag_data(&[rich.id]).expect("富文本");
    assert_eq!(
        d.html.as_deref(),
        Some("<p>Hello <b>Wisp</b></p>"),
        "富文本另带 text/html"
    );
    assert_eq!(d.text, "Hello Wisp");
    // 多条：按选择顺序以换行合并（与合并粘贴相同），不带 HTML 与地址
    let d = r.engine.drag_data(&[rich.id, plain.id]).expect("多条");
    assert_eq!(
        (d.text.as_str(), d.html, d.uri),
        ("Hello Wisp\nplain words", None, None)
    );
    // 拖拽不改历史：不写剪贴板、不 touch
    assert!(r.port.paste.writes.lock().is_empty());
    assert_eq!(r.top().id, rich.id);
}
