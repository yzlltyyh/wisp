//! OCR 历史的存储：增删查、上限裁剪；关闭并清空后表与 blobs 都被清理。

mod common;

use common::{Fixture, NOW, png};
use wisp_clipboard::{BlobKind, OcrHistoryNew};
use wisp_types::OcrMode;

fn entry<'a>(t: i64, text: &'a str, thumb: Option<&'a [u8]>, image: Option<&'a [u8]>) -> OcrHistoryNew<'a> {
    OcrHistoryNew {
        created_at: t,
        mode: OcrMode::Markdown,
        model: "gpt-4o-mini",
        text,
        thumb,
        image,
    }
}

#[test]
fn add_list_delete_clear() {
    let f = Fixture::new();
    let image = png(32, 32, false, |x, y| [x as u8, y as u8, 0, 255]);
    let thumb = png(8, 8, false, |_, _| [1, 2, 3, 255]);
    let a = f
        .store
        .ocr_history_add(&entry(NOW, "第一条", Some(&thumb), Some(&image)))
        .expect("新增");
    let b = f
        .store
        .ocr_history_add(&entry(NOW + 1, "second", Some(&thumb), Some(&image)))
        .expect("新增");
    let c = f
        .store
        .ocr_history_add(&entry(NOW + 2, "third", None, None))
        .expect("新增");
    // 同样的图片共用文件（缩略图 + 原图各一个）
    assert_eq!(f.blob_files(BlobKind::Ocr).len(), 2);

    let list = f.store.ocr_history_list(0, 10).expect("列表");
    assert_eq!(list.iter().map(|i| i.id).collect::<Vec<_>>(), [c, b, a]);
    assert_eq!(list[2].text, "第一条");
    assert_eq!(list[2].mode, OcrMode::Markdown);
    assert_eq!(list[2].model, "gpt-4o-mini");
    let url = list[2].thumb_url.clone().expect("缩略图");
    assert!(url.starts_with("ocrimg/") && url.ends_with(".png"), "{url}");
    assert!(list[0].thumb_url.is_none());
    // 原图同样给出资源路径（「回看」放大用），没有原图时为 None
    let image_url = list[2].image_url.clone().expect("原图");
    assert!(image_url.starts_with("ocrimg/") && image_url != url, "{image_url}");
    assert!(list[0].image_url.is_none());
    assert_eq!(f.store.ocr_history_list(1, 1).expect("分页").len(), 1);

    // 删一条：仍被另一条引用的文件保留
    f.store.ocr_history_delete(a).expect("删除");
    assert_eq!(f.blob_files(BlobKind::Ocr).len(), 2);
    f.store.ocr_history_delete(b).expect("删除");
    assert!(f.blob_files(BlobKind::Ocr).is_empty());

    f.store
        .ocr_history_add(&entry(NOW + 3, "again", Some(&thumb), Some(&image)))
        .expect("新增");
    f.store.ocr_history_clear().expect("清空");
    assert!(f.store.ocr_history_list(0, 10).expect("列表").is_empty());
    assert!(f.blob_files(BlobKind::Ocr).is_empty());
    assert_eq!(f.count("SELECT count(*) FROM ocr_history"), 0);
}

#[test]
fn retain_keeps_newest() {
    let f = Fixture::new();
    for i in 0..12 {
        let image = png(4, 4, false, move |_, _| [(i * 20) as u8, (240 - i * 20) as u8, 0, 255]);
        f.store
            .ocr_history_add(&entry(NOW + i, &format!("text {i}"), None, Some(&image)))
            .expect("新增");
    }
    assert_eq!(f.store.ocr_history_retain(10).expect("裁剪"), 2);
    let list = f.store.ocr_history_list(0, 100).expect("列表");
    assert_eq!(list.len(), 10);
    assert_eq!(list[0].text, "text 11");
    assert_eq!(list[9].text, "text 2");
    // 每条一张原图 + 一张由原图生成的缩略图
    assert_eq!(f.blob_files(BlobKind::Ocr).len(), 20);
}

#[test]
fn trim_reports_removed_ids_and_get_reads_one() {
    let f = Fixture::new();
    let ids: Vec<i64> = (0..5)
        .map(|i| {
            f.store
                .ocr_history_add(&entry(NOW + i, &format!("t{i}"), None, None))
                .expect("新增")
        })
        .collect();
    // 最旧的两条被裁掉，返回的正是它们的 id（推送 `removed` 用）
    let mut removed = f.store.ocr_history_trim(3).expect("裁剪");
    removed.sort_unstable();
    assert_eq!(removed, ids[..2]);
    assert!(f.store.ocr_history_trim(3).expect("裁剪").is_empty());
    assert!(f.store.ocr_history_get(ids[0]).expect("读取").is_none());
    let item = f.store.ocr_history_get(ids[4]).expect("读取").expect("存在");
    assert_eq!(item.text, "t4");
    assert_eq!(item.created_at, NOW + 4);
    // 删除返回是否真的删掉了（不存在的 id 不推送变化）
    assert!(f.store.ocr_history_delete(ids[4]).expect("删除"));
    assert!(!f.store.ocr_history_delete(ids[4]).expect("删除"));
}

#[test]
fn thumbnail_is_derived_from_the_original() {
    let f = Fixture::new();
    // PNG 原图：缩略图长边 256，不透明 → JPEG
    let image = png(1024, 512, false, |x, y| [(x % 251) as u8, (y % 241) as u8, 9, 255]);
    let id = f
        .store
        .ocr_history_add(&entry(NOW, "png", None, Some(&image)))
        .expect("新增");
    let item = f.store.ocr_history_get(id).expect("读取").expect("存在");
    let thumb = item.thumb_url.expect("生成了缩略图");
    assert!(thumb.ends_with(".jpg"), "{thumb}");
    assert!(item.image_url.expect("原图").ends_with(".png"));
    let key = thumb.trim_start_matches("ocrimg/");
    let bytes = std::fs::read(f.store.blob_path(BlobKind::Ocr, key)).expect("缩略图文件");
    let decoded = image::load_from_memory(&bytes).expect("可解码");
    assert_eq!((decoded.width(), decoded.height()), (256, 128));

    // JPEG 原图（截图过大时送模型用的格式）同样可以保存，缩略图照样生成
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90)
        .encode(&[200u8; 64 * 32 * 3], 64, 32, image::ExtendedColorType::Rgb8)
        .expect("编码");
    let id = f
        .store
        .ocr_history_add(&entry(NOW + 1, "jpeg", None, Some(&jpeg)))
        .expect("新增");
    let item = f.store.ocr_history_get(id).expect("读取").expect("存在");
    assert!(item.image_url.expect("原图").ends_with(".jpg"));
    assert!(item.thumb_url.is_some());
}

/// 编排器在编码截图时从裁剪帧生成缩略图（`ocr_thumbnail`），写入时原样保存、不再从原图解码
#[test]
fn provided_thumbnail_from_the_crop_frame_is_stored_as_is() {
    let f = Fixture::new();
    let (w, h) = (1024u32, 512u32);
    let rgba: Vec<u8> = (0..h)
        .flat_map(|y| (0..w).flat_map(move |x| [(x % 251) as u8, (y % 241) as u8, 9, 255]))
        .collect();
    let thumb = wisp_clipboard::ocr_thumbnail(w, h, &rgba).expect("缩略图");
    let decoded = image::load_from_memory(&thumb).expect("可解码");
    assert_eq!((decoded.width(), decoded.height()), (256, 128));
    assert!(
        wisp_clipboard::ocr_thumbnail(w, h, &rgba[..100]).is_none(),
        "像素数据不足"
    );
    let image = png(w, h, false, |x, y| [(x % 251) as u8, (y % 241) as u8, 9, 255]);
    let id = f
        .store
        .ocr_history_add(&entry(NOW, "with thumb", Some(&thumb), Some(&image)))
        .expect("新增");
    let item = f.store.ocr_history_get(id).expect("读取").expect("存在");
    let key = item.thumb_url.expect("缩略图").trim_start_matches("ocrimg/").to_owned();
    let stored = std::fs::read(f.store.blob_path(BlobKind::Ocr, &key)).expect("缩略图文件");
    assert_eq!(stored, thumb, "原样保存，没有从原图重新生成");
}

#[test]
fn rejects_unknown_image_formats() {
    let f = Fixture::new();
    assert!(
        f.store
            .ocr_history_add(&entry(NOW, "x", Some(b"GIF89a"), None))
            .is_err()
    );
    assert!(
        f.store
            .ocr_history_add(&entry(NOW, "x", None, Some(b"GIF89a")))
            .is_err(),
        "原图必须是 PNG 或 JPEG"
    );
    // 认得出格式、但解码不了的原图：照常记录，只是没有缩略图
    let jpeg_header = [0xff_u8, 0xd8, 0xff, 0xe0];
    let id = f
        .store
        .ocr_history_add(&entry(NOW, "x", None, Some(&jpeg_header)))
        .expect("新增");
    assert!(
        f.store
            .ocr_history_get(id)
            .expect("读取")
            .expect("存在")
            .thumb_url
            .is_none()
    );
    assert!(
        f.store
            .ocr_history_add(&entry(NOW, "x", Some(&jpeg_header), None))
            .is_ok()
    );
}

#[test]
fn clear_keeps_in_flight_temporary_files() {
    // 清空 OCR 历史时，并发的 ocr_history_add 可能正在锁外写临时文件：保留期内的临时文件不删（与孤儿清理一致），
    // 崩溃遗留的过期临时文件照常删除
    let f = Fixture::new();
    let image = png(8, 8, false, |_, _| [1, 1, 1, 255]);
    f.store
        .ocr_history_add(&entry(NOW, "x", None, Some(&image)))
        .expect("新增");
    let dir = f.dir.path().join("blobs").join("ocr");
    std::fs::write(dir.join(".tmp-1-0-in-flight.png"), b"partial").expect("写");
    let stale = std::fs::File::create(dir.join(".tmp-1-1-crashed.png")).expect("写");
    stale
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(11 * 60))
        .expect("改时间");
    drop(stale);

    f.store.ocr_history_clear().expect("清空");
    assert_eq!(f.blob_files(BlobKind::Ocr), [".tmp-1-0-in-flight.png"]);
    // 清空之后照常可以新增（同内容的文件重新写入：原图 + 生成的缩略图）
    f.store
        .ocr_history_add(&entry(NOW + 1, "y", None, Some(&image)))
        .expect("新增");
    assert_eq!(
        f.blob_files(BlobKind::Ocr)
            .iter()
            .filter(|n| !n.starts_with(".tmp-"))
            .count(),
        2
    );
}
