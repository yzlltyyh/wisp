//! 去重与 touch 语义，以及每种 kind 的往返。

mod common;

use common::{Fixture, NOW, app, expect_inserted, expect_meta, image_raw, png, raw_text};
use wisp_clipboard::IngestOutcome;
use wisp_types::{ClipFilter, ClipKind, RawClipboard};

#[test]
fn same_text_twice_is_one_item_with_use_count_two() {
    let f = Fixture::new();
    let first = expect_inserted(f.ingest(&raw_text("hello world"), NOW));
    assert_eq!(first.use_count, 1);
    // 末尾空白不同也算同一条
    let IngestOutcome::Touched(second) = f.ingest(&raw_text("hello world  \n"), NOW + 1000) else {
        panic!("应为 Touched")
    };
    assert_eq!(second.id, first.id);
    assert_eq!(second.use_count, 2);
    assert_eq!(second.created_at, NOW);
    assert_eq!(second.last_used_at, NOW + 1000);
    assert_eq!(f.count("SELECT count(*) FROM items"), 1);
}

#[test]
fn touched_item_moves_to_top() {
    let f = Fixture::new();
    let a = f.text("alpha", NOW);
    f.text("beta", NOW + 1);
    f.text("gamma", NOW + 2);
    f.text("alpha", NOW + 3);
    let page = f.store.recent(0, 10, ClipFilter::All).expect("列表");
    let order: Vec<i64> = page.items.iter().map(|h| h.item.id).collect();
    assert_eq!(order[0], a.id);
    assert_eq!(page.items.len(), 3);
}

#[test]
fn same_image_from_different_sources_and_encodings_is_one_item() {
    let f = Fixture::new();
    let pixels = |x: u32, y: u32| [(x * 3) as u8, (y * 5) as u8, 77, 255];
    let mut a = image_raw(png(64, 48, true, pixels));
    a.source = Some(app("com.apple.Preview", "Preview"));
    let first = expect_inserted(f.ingest(&a, NOW));
    // 同样的像素，RGB 编码（字节完全不同）、来源不同
    let mut b = image_raw(png(64, 48, false, pixels));
    b.source = Some(app("com.google.Chrome", "Chrome"));
    let IngestOutcome::Touched(second) = f.ingest(&b, NOW + 5) else {
        panic!("应为 Touched")
    };
    assert_eq!(second.id, first.id);
    assert_eq!(second.use_count, 2);
    // 来源保持第一次的
    assert_eq!(second.source.as_ref().map(|s| s.id.as_str()), Some("com.apple.Preview"));
    assert_eq!(f.count("SELECT count(*) FROM items"), 1);
    assert_eq!(f.blob_files(wisp_clipboard::BlobKind::Img).len(), 1);
}

#[test]
fn same_files_in_different_order_is_one_item() {
    let f = Fixture::new();
    let raw = |paths: &[&str]| RawClipboard {
        files: Some(paths.iter().map(|p| (*p).to_owned()).collect()),
        ..RawClipboard::default()
    };
    let first = expect_inserted(f.ingest(&raw(&["/tmp/wisp-a", "/tmp/wisp-b"]), NOW));
    let second = expect_meta(f.ingest(&raw(&["/tmp/wisp-b", "/tmp/wisp-a"]), NOW + 1));
    assert_eq!(first.id, second.id);
    assert_eq!(second.use_count, 2);
    let third = expect_inserted(f.ingest(&raw(&["/tmp/wisp-a"]), NOW + 2));
    assert_ne!(third.id, first.id);
}

#[test]
fn plain_paste_touch_keeps_html_and_source() {
    // 对富文本条目做纯文本粘贴：条数不变，条目在顶部，HTML 还在，来源不变
    let f = Fixture::new();
    let rich = RawClipboard {
        text: Some("Hello rich".into()),
        html: Some("<b>Hello</b> rich".into()),
        source: Some(app("com.apple.Safari", "Safari")),
        ..RawClipboard::default()
    };
    let item = expect_inserted(f.ingest(&rich, NOW));
    assert_eq!(item.kind, ClipKind::RichText);
    f.text("something newer", NOW + 1);

    let payload = f.store.get_payload(item.id, true).expect("取载荷").expect("存在");
    assert_eq!(payload.text.as_deref(), Some("Hello rich"));
    assert!(payload.html.is_none(), "纯文本载荷不带 html");
    let touched = f.store.touch(item.id, NOW + 2).expect("touch");
    assert_eq!(touched.use_count, 2);

    // 平台随后读到Wisp 自己写入的纯文本：命中去重，不新建条目
    let mut echo = raw_text("Hello rich");
    echo.from_wisp = true;
    echo.source = Some(app("com.apple.TextEdit", "TextEdit"));
    let again = expect_meta(f.ingest(&echo, NOW + 3));
    assert_eq!(again.id, item.id);

    assert_eq!(f.count("SELECT count(*) FROM items"), 2);
    let page = f.store.recent(0, 10, ClipFilter::All).expect("列表");
    assert_eq!(page.items[0].item.id, item.id);
    let detail = f.store.get_detail(item.id).expect("详情").expect("存在");
    assert!(detail.has_html);
    assert_eq!(
        detail.meta.source.as_ref().map(|s| s.id.as_str()),
        Some("com.apple.Safari")
    );
    assert_eq!(detail.meta.kind, ClipKind::RichText);
    let full = f.store.get_payload(item.id, false).expect("取载荷").expect("存在");
    assert_eq!(full.html.as_deref(), Some("<b>Hello</b> rich"));
}

#[test]
fn touch_and_pin_on_missing_item_report_not_found() {
    let f = Fixture::new();
    assert!(matches!(
        f.store.touch(999, NOW),
        Err(wisp_clipboard::StoreError::NotFound(999))
    ));
    assert!(matches!(
        f.store.set_pinned(999, true),
        Err(wisp_clipboard::StoreError::NotFound(999))
    ));
    assert!(!f.store.delete(999).expect("删除不存在的条目"));
    assert!(f.store.get_detail(999).expect("详情").is_none());
    assert!(f.store.get_payload(999, false).expect("载荷").is_none());
}

#[test]
fn every_kind_round_trips() {
    let f = Fixture::new();
    let src = app("com.microsoft.VSCode", "Code");

    // Text
    let mut raw = raw_text("  plain text\nsecond line  ");
    raw.source = Some(src.clone());
    let m = expect_inserted(f.ingest(&raw, NOW));
    assert_eq!(m.kind, ClipKind::Text);
    assert_eq!(m.preview, "plain text second line");
    assert_eq!((m.char_count, m.line_count, m.truncated), (Some(24), Some(2), false));
    let s = m.source.as_ref().expect("来源");
    assert_eq!(
        (s.id.as_str(), s.name.as_str(), s.icon_url.as_deref()),
        ("com.microsoft.VSCode", "Code", None)
    );
    let d = f.store.get_detail(m.id).expect("详情").expect("存在");
    assert_eq!(d.text.as_deref(), Some("  plain text\nsecond line"));
    assert!(!d.has_html && !d.has_rtf && d.image_url.is_none() && d.files.is_none());
    assert_eq!(d.meta, m);

    // RichText（HTML + RTF）
    let raw = RawClipboard {
        text: Some("Rich".into()),
        html: Some("<i>Rich</i><script>x()</script>".into()),
        rtf: Some(b"{\\rtf1 Rich}".to_vec()),
        ..RawClipboard::default()
    };
    let m = expect_inserted(f.ingest(&raw, NOW + 1));
    assert_eq!(m.kind, ClipKind::RichText);
    let p = f.store.get_payload(m.id, false).expect("载荷").expect("存在");
    assert_eq!(p.html.as_deref(), Some("<i>Rich</i>"));
    assert_eq!(p.rtf.as_deref(), Some(&b"{\\rtf1 Rich}"[..]));
    let d = f.store.get_detail(m.id).expect("详情").expect("存在");
    assert!(d.has_html && d.has_rtf);

    // Link
    let m = expect_inserted(f.ingest(&raw_text("www.example.com/path"), NOW + 2));
    assert_eq!(m.kind, ClipKind::Link);
    assert_eq!(m.link.as_deref(), Some("https://www.example.com/path"));
    assert_eq!(m.preview, "www.example.com/path");

    // Color
    let m = expect_inserted(f.ingest(&raw_text(" #3478F6 "), NOW + 3));
    assert_eq!(m.kind, ClipKind::Color);
    assert_eq!(m.color.as_deref(), Some("#3478F6"));
    assert!(m.link.is_none());

    // Code
    let m = expect_inserted(f.ingest(&raw_text("fn main() {\n    run();\n}"), NOW + 4));
    assert_eq!(m.kind, ClipKind::Code);
    assert_eq!(m.line_count, Some(3));

    // Files
    let dir = tempfile::tempdir().expect("临时目录");
    let a = dir.path().join("report.pdf");
    std::fs::write(&a, vec![0u8; 1234]).expect("写文件");
    let missing = dir.path().join("gone.txt");
    let raw = RawClipboard {
        files: Some(vec![
            a.to_string_lossy().into_owned(),
            missing.to_string_lossy().into_owned(),
        ]),
        ..RawClipboard::default()
    };
    let m = expect_inserted(f.ingest(&raw, NOW + 5));
    assert_eq!(m.kind, ClipKind::Files);
    assert_eq!(m.preview, "report.pdf, gone.txt");
    let files = m.files.as_ref().expect("文件");
    assert_eq!(files[0].name, "report.pdf");
    assert_eq!(files[0].size, Some(1234));
    let d = f.store.get_detail(m.id).expect("详情").expect("存在");
    let files = d.files.as_ref().expect("文件");
    assert_eq!(files.iter().map(|f| f.exists).collect::<Vec<_>>(), [true, false]);
    let p = f.store.get_payload(m.id, false).expect("载荷").expect("存在");
    assert_eq!(p.files.as_ref().map(Vec::len), Some(2));
    let p = f.store.get_payload(m.id, true).expect("载荷").expect("存在");
    assert!(p.files.is_none() && p.text.as_deref().is_some_and(|t| t.contains("report.pdf")));

    // Image
    let bytes = png(30, 20, true, |x, y| [x as u8, y as u8, 0, 200]);
    let m = expect_inserted(f.ingest(&image_raw(bytes.clone()), NOW + 6));
    assert_eq!(m.kind, ClipKind::Image);
    let img = m.image.as_ref().expect("图片元数据");
    assert_eq!((img.width, img.height, img.bytes), (30, 20, bytes.len() as u64));
    assert!(img.thumb_url.is_none());
    assert_eq!(m.preview, "");
    let d = f.store.get_detail(m.id).expect("详情").expect("存在");
    let url = d.image_url.expect("原图路径");
    assert!(url.starts_with("img/") && url.ends_with(".png"), "{url}");
    let p = f.store.get_payload(m.id, false).expect("载荷").expect("存在");
    assert_eq!(p.image_png.as_deref(), Some(&bytes[..]));

    assert_eq!(f.count("SELECT count(*) FROM items"), 7);
}

#[test]
fn deleting_source_file_greys_out_on_next_read() {
    let f = Fixture::new();
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("a.txt");
    std::fs::write(&path, b"x").expect("写文件");
    let raw = RawClipboard {
        files: Some(vec![path.to_string_lossy().into_owned()]),
        ..RawClipboard::default()
    };
    let m = expect_inserted(f.ingest(&raw, NOW));
    std::fs::remove_file(&path).expect("删除");

    // 列表查询不访问文件系统（网络卷上 stat 可能阻塞数秒）：exists 按存在处理
    let mut page = f.store.recent(0, 10, ClipFilter::Files).expect("列表");
    assert!(page.items[0].item.files.as_ref().expect("文件")[0].exists);
    let hits = f.store.search("a.txt", ClipFilter::All, 10).expect("搜索");
    assert!(hits.items[0].item.files.as_ref().expect("文件")[0].exists);

    // 调用方对可视批次在后台刷新，拿到需要推送 updated 的 id
    assert_eq!(wisp_clipboard::refresh_file_exists(&mut page.items), [m.id]);
    assert!(!page.items[0].item.files.as_ref().expect("文件")[0].exists);
    assert!(wisp_clipboard::refresh_file_exists(&mut page.items).is_empty());

    // 预览（单条）实时检查
    let d = f.store.get_detail(m.id).expect("详情").expect("存在");
    assert!(!d.files.as_ref().expect("文件")[0].exists);
    assert!(!d.meta.files.as_ref().expect("文件")[0].exists);
}

#[test]
fn source_icons_are_attached_and_backfilled() {
    let f = Fixture::new();
    let safari = app("com.apple.Safari", "Safari");
    let mut raw = raw_text("before icon");
    raw.source = Some(safari.clone());
    let before = expect_inserted(f.ingest(&raw, NOW));
    assert!(before.source.as_ref().expect("来源").icon_url.is_none());
    assert!(f.store.icon_key("com.apple.Safari").is_none());

    // 1024 px 的图标被缩到 64 px
    let icon = png(1024, 1024, true, |x, y| [(x / 4) as u8, (y / 4) as u8, 128, 255]);
    let key = f.store.put_icon("com.apple.Safari", &icon).expect("保存图标");
    assert_eq!(f.store.icon_key("com.apple.Safari").as_deref(), Some(key.as_str()));
    let path = f.store.blob_path(wisp_clipboard::BlobKind::Icon, &format!("{key}.png"));
    let stored = std::fs::read(&path).expect("图标文件");
    let dec = png::Decoder::new(std::io::Cursor::new(&stored[..]));
    let info = dec.read_info().expect("PNG");
    assert_eq!((info.info().width, info.info().height), (64, 64));

    // 已有条目被补上图标；之后的条目直接带上
    let detail = f.store.get_detail(before.id).expect("详情").expect("存在");
    let url = detail
        .meta
        .source
        .as_ref()
        .expect("来源")
        .icon_url
        .clone()
        .expect("补图标");
    assert_eq!(url, format!("icon/{key}.png"));
    let mut raw = raw_text("after icon");
    raw.source = Some(safari);
    let after = expect_inserted(f.ingest(&raw, NOW + 1));
    assert_eq!(after.source.expect("来源").icon_url.as_deref(), Some(url.as_str()));

    // 非法图标
    assert!(f.store.put_icon("com.example", b"not png").is_err());
    assert!(f.store.put_icon("", &icon).is_err());
}

#[test]
fn huge_file_lists_stay_cheap_in_lists() {
    // 一次复制上万个文件：列表元数据只带前 5 个（不解析完整列表）；详情给完整列表，只实时检查前 200 个
    let f = Fixture::new();
    let dir = tempfile::tempdir().expect("临时目录");
    let existing = dir.path().join("kept.txt");
    std::fs::write(&existing, b"x").expect("写文件");
    let mut paths: Vec<String> = (0..3000)
        .map(|i| {
            dir.path()
                .join(format!("missing-{i:05}.bin"))
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    paths[0] = existing.to_string_lossy().into_owned();
    let raw = RawClipboard {
        files: Some(paths.clone()),
        ..RawClipboard::default()
    };
    let m = expect_inserted(f.ingest(&raw, NOW));
    assert_eq!(m.files.as_ref().map(Vec::len), Some(5));
    // 列表读的是只含前 5 项的列，完整列表只在详情 / 粘贴时读
    let head_len: i64 = f.count("SELECT length(files_head_json) FROM items");
    let full_len: i64 = f.count("SELECT length(files_json) FROM items");
    assert!(head_len < 1024 && full_len > 100_000, "{head_len} / {full_len}");
    let page = f.store.recent(0, 10, wisp_types::ClipFilter::All).expect("列表");
    assert_eq!(page.items[0].item.files.as_ref().map(Vec::len), Some(5));
    assert_eq!(page.items[0].item.files.as_ref().expect("文件")[0].name, "kept.txt");

    let d = f.store.get_detail(m.id).expect("详情").expect("存在");
    let files = d.files.as_ref().expect("文件");
    assert_eq!(files.len(), 3000);
    assert!(files[0].exists);
    assert!(files[1..200].iter().all(|f| !f.exists), "前 200 个实时检查");
    assert!(files[200..].iter().all(|f| f.exists), "之后的未检查，按 true 给出");
    let head = d.meta.files.as_ref().expect("文件");
    assert_eq!(
        head.iter().map(|f| f.exists).collect::<Vec<_>>(),
        [true, false, false, false, false]
    );
    let p = f.store.get_payload(m.id, false).expect("载荷").expect("存在");
    assert_eq!(p.files.as_deref(), Some(paths.as_slice()));
}
