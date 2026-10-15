//! 图片条目：ingest 不等缩略图就返回；后台生成的缩略图长边 256，不透明 JPEG / 带透明 PNG；
//! 上限与异常输入。

mod common;

use common::{Fixture, NOW, expect_inserted, image_raw, png, screenshot_like};
use wisp_clipboard::{BlobKind, IngestOutcome, SkipReason};
use wisp_types::ClipKind;

fn decode_size(bytes: &[u8]) -> (u32, u32) {
    if bytes.starts_with(&[0xff, 0xd8]) {
        // JPEG：找 SOF0 / SOF2
        let mut i = 2;
        while i + 9 < bytes.len() {
            if bytes[i] == 0xff && matches!(bytes[i + 1], 0xc0 | 0xc2) {
                let h = u32::from(u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]));
                let w = u32::from(u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]));
                return (w, h);
            }
            i += 1;
        }
        panic!("没有 SOF");
    }
    let r = png::Decoder::new(std::io::Cursor::new(bytes)).read_info().expect("PNG");
    (r.info().width, r.info().height)
}

#[test]
fn screenshot_ingest_returns_before_thumbnail() {
    // 4K 截图入库时 ingest 返回不等缩略图；缩略图长边 256
    let f = Fixture::new();
    let bytes = screenshot_like(3840, 2160, 1);
    let m = expect_inserted(f.ingest(&image_raw(bytes.clone()), NOW));
    let img = m.image.as_ref().expect("图片");
    assert_eq!((img.width, img.height), (3840, 2160));
    assert!(img.thumb_url.is_none(), "ingest 返回时还没有缩略图");
    assert!(f.blob_files(BlobKind::Thumb).is_empty());
    assert_eq!(f.blob_files(BlobKind::Img).len(), 1);

    // 原图按原 PNG 字节保存
    let stored = std::fs::read(f.dir.path().join("blobs/img").join(&f.blob_files(BlobKind::Img)[0])).expect("原图");
    assert_eq!(stored, bytes);

    let updated = f.store.make_thumb(m.id).expect("生成").expect("有更新");
    assert_eq!(updated.id, m.id);
    assert_eq!(updated.last_used_at, m.last_used_at, "缩略图更新不改变排序");
    let url = updated
        .image
        .as_ref()
        .and_then(|i| i.thumb_url.clone())
        .expect("thumb_url");
    assert!(url.starts_with("thumb/") && url.ends_with(".jpg"), "{url}");
    let key = url.trim_start_matches("thumb/");
    let thumb = std::fs::read(f.store.blob_path(BlobKind::Thumb, key)).expect("缩略图文件");
    assert_eq!(decode_size(&thumb), (256, 144));
    assert!(thumb.len() < 64 * 1024, "缩略图应很小：{} 字节", thumb.len());

    // 幂等
    assert!(f.store.make_thumb(m.id).expect("再次").is_none());
    // 列表里带上缩略图
    let page = f.store.recent(0, 1, wisp_types::ClipFilter::Image).expect("列表");
    assert_eq!(
        page.items[0].item.image.as_ref().and_then(|i| i.thumb_url.as_deref()),
        Some(url.as_str())
    );
}

#[test]
fn transparent_images_get_png_thumbnails() {
    let f = Fixture::new();
    let bytes = png(600, 300, true, |x, _| {
        if x < 300 { [255, 0, 0, 0] } else { [0, 128, 255, 255] }
    });
    let m = expect_inserted(f.ingest(&image_raw(bytes), NOW));
    let updated = f.store.make_thumb(m.id).expect("生成").expect("更新");
    let url = updated.image.and_then(|i| i.thumb_url).expect("url");
    assert!(url.ends_with(".png"), "{url}");
    let thumb = std::fs::read(f.store.blob_path(BlobKind::Thumb, url.trim_start_matches("thumb/"))).expect("文件");
    assert_eq!(decode_size(&thumb), (256, 128));
}

#[test]
fn make_thumb_is_noop_for_missing_or_non_image_items() {
    let f = Fixture::new();
    let t = f.text("not an image", NOW);
    assert!(f.store.make_thumb(t.id).expect("文本").is_none());
    assert!(f.store.make_thumb(12345).expect("不存在").is_none());
    // 删除后再生成：不留文件
    let m = expect_inserted(f.ingest(&image_raw(png(10, 10, false, |_, _| [5, 5, 5, 255])), NOW));
    f.store.delete(m.id).expect("删除");
    assert!(f.store.make_thumb(m.id).expect("已删除").is_none());
    assert!(f.blob_files(BlobKind::Thumb).is_empty());
}

/// 退出 / 崩溃前没做完的缩略图可以在之后补上（最近使用的在前）
#[test]
fn missing_thumbnails_are_listed_for_repair() {
    let f = Fixture::new();
    let a = expect_inserted(f.ingest(&image_raw(png(20, 10, false, |_, _| [1, 2, 3, 255])), NOW));
    let b = expect_inserted(f.ingest(&image_raw(png(30, 10, false, |_, _| [4, 5, 6, 255])), NOW + 1000));
    f.text("text items never need thumbnails", NOW + 2000);
    assert_eq!(f.store.missing_thumbs(50).expect("query"), vec![b.id, a.id]);
    assert_eq!(f.store.missing_thumbs(1).expect("limit"), vec![b.id]);
    f.store.make_thumb(b.id).expect("thumb").expect("updated");
    assert_eq!(f.store.missing_thumbs(50).expect("query"), vec![a.id]);
}

#[test]
fn oversized_and_invalid_images() {
    let f = Fixture::new();
    let mut p = common::policy();
    p.max_image_bytes = 100;
    let big = png(64, 64, false, |x, y| [(x * y) as u8, x as u8, y as u8, 255]);
    assert!(big.len() > 100);
    assert_eq!(
        f.store.ingest(&image_raw(big), &p, NOW).expect("ingest"),
        IngestOutcome::Skipped(SkipReason::TooLarge)
    );
    // 像素数超限（解压炸弹）：12000 × 12000 的 1 位纯色图编码后只有几十 KB
    let bomb = {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, 12_000, 12_000);
        enc.set_color(png::ColorType::Grayscale);
        enc.set_depth(png::BitDepth::One);
        enc.set_compression(png::Compression::Fast);
        let mut w = enc.write_header().expect("头");
        w.write_image_data(&vec![0u8; 12_000 / 8 * 12_000]).expect("数据");
        drop(w);
        out
    };
    assert!(bomb.len() < 20 * 1024 * 1024);
    assert_eq!(
        f.ingest(&image_raw(bomb), NOW),
        IngestOutcome::Skipped(SkipReason::TooLarge)
    );
    // 不是 PNG
    assert!(
        f.store
            .ingest(&image_raw(b"\x89PNG garbage".to_vec()), &common::policy(), NOW)
            .is_err()
    );
    assert_eq!(f.count("SELECT count(*) FROM items"), 0);
    assert!(f.blob_files(BlobKind::Img).is_empty());
}

#[test]
fn image_with_url_text_is_recorded_as_image() {
    // 浏览器「拷贝图像」：图片 + 指向图片的 URL
    let f = Fixture::new();
    let mut raw = image_raw(png(20, 20, false, |_, _| [9, 8, 7, 255]));
    raw.text = Some("https://example.com/cat.png".into());
    let m = expect_inserted(f.ingest(&raw, NOW));
    assert_eq!(m.kind, ClipKind::Image);
}

/// 只有文件头的 PNG（尺寸检查发生在读图像数据之前）
fn header_only_png(w: u32, h: u32, interlaced: bool) -> Vec<u8> {
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = u32::MAX;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let crc = crc32(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    }
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 0, 0, 0, u8::from(interlaced)]);
    let mut out = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(
        &mut out,
        b"IDAT",
        &[0x78, 0x01, 0x01, 0x00, 0x00, 0xff, 0xff, 0x00, 0x00, 0x00, 0x01],
    );
    chunk(&mut out, b"IEND", &[]);
    out
}

#[test]
fn large_interlaced_images_are_skipped_without_full_frame_decode() {
    // 隔行图必须整帧解码：上限 8 MiP（32 MB）。约 11585² 的隔行纯色图压缩后不到 1 MB，旧上限下要分配约 512 MB
    let f = Fixture::new();
    for (w, h) in [(11_585, 11_585), (4_000, 4_000)] {
        let bomb = header_only_png(w, h, true);
        assert!(bomb.len() < 1024);
        assert_eq!(
            f.ingest(&image_raw(bomb), NOW),
            IngestOutcome::Skipped(SkipReason::TooLarge),
            "{w}x{h}"
        );
    }
    // 超宽的逐行图：行缓冲与宽度成正比
    assert_eq!(
        f.ingest(&image_raw(header_only_png(1_000_000, 2, false)), NOW),
        IngestOutcome::Skipped(SkipReason::TooLarge)
    );
    assert_eq!(f.count("SELECT count(*) FROM items"), 0);
    assert!(f.blob_files(BlobKind::Img).is_empty());
}

#[test]
fn damaged_image_blob_is_treated_as_missing() {
    // 旧版本断电时可能留下「文件名在、内容为空」的原图：粘贴与生成缩略图都按缺失处理，不把坏数据写进剪贴板
    let f = Fixture::new();
    let m = expect_inserted(f.ingest(&image_raw(png(12, 12, false, |_, _| [4, 5, 6, 255])), NOW));
    let key = &f.blob_files(BlobKind::Img)[0];
    std::fs::write(f.store.blob_path(BlobKind::Img, key), b"").expect("写坏");
    assert!(f.store.get_payload(m.id, false).expect("载荷").is_none());
    assert!(f.store.make_thumb(m.id).expect("缩略图").is_none());
    // 写入没有留下临时文件
    assert!(f.blob_files(BlobKind::Thumb).is_empty());
    assert!(!f.blob_files(BlobKind::Img).iter().any(|n| n.starts_with(".tmp-")));
}

#[test]
fn copying_the_same_image_again_restores_a_lost_original() {
    // 原图文件丢失（断电时 rename 没落盘、被外部误删）或残缺后，再次复制同一张图时自愈：粘贴与缩略图恢复可用
    let f = Fixture::new();
    let bytes = png(20, 10, false, |x, y| [x as u8 * 9, y as u8 * 20, 7, 255]);
    let m = expect_inserted(f.ingest(&image_raw(bytes.clone()), NOW));
    f.store.make_thumb(m.id).expect("缩略图").expect("生成");
    let key = f.blob_files(BlobKind::Img)[0].clone();
    let thumb = f.blob_files(BlobKind::Thumb)[0].clone();

    // 原图与缩略图都丢了
    std::fs::remove_file(f.store.blob_path(BlobKind::Img, &key)).expect("删除原图");
    std::fs::remove_file(f.store.blob_path(BlobKind::Thumb, &thumb)).expect("删除缩略图");
    assert!(f.store.get_payload(m.id, false).expect("载荷").is_none());
    let IngestOutcome::Touched(t) = f.ingest(&image_raw(bytes.clone()), NOW + 1) else {
        panic!("应为 Touched")
    };
    assert_eq!(t.id, m.id);
    assert_eq!(t.use_count, 2);
    let payload = f.store.get_payload(m.id, false).expect("载荷").expect("可粘贴");
    assert_eq!(payload.image_png.as_deref(), Some(bytes.as_slice()));
    // 缩略图引用已清掉：调用方照常在后台重新生成
    assert!(t.image.as_ref().expect("图片").thumb_url.is_none());
    let regenerated = f.store.make_thumb(m.id).expect("缩略图").expect("重新生成");
    assert!(regenerated.image.expect("图片").thumb_url.is_some());

    // 原图被写坏（空文件）：同样用新的复制补回；同一像素的不同 PNG 编码也可以
    std::fs::write(f.store.blob_path(BlobKind::Img, &key), b"").expect("写坏");
    let reencoded = {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, 20, 10);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::High);
        let data: Vec<u8> = (0..10u32)
            .flat_map(|y| (0..20u32).flat_map(move |x| [x as u8 * 9, y as u8 * 20, 7, 255]))
            .collect();
        enc.write_header().expect("头").write_image_data(&data).expect("数据");
        out
    };
    assert_ne!(reencoded, bytes);
    assert!(matches!(
        f.ingest(&image_raw(reencoded.clone()), NOW + 2),
        IngestOutcome::Touched(_)
    ));
    let payload = f.store.get_payload(m.id, false).expect("载荷").expect("可粘贴");
    assert_eq!(payload.image_png.as_deref(), Some(reencoded.as_slice()));
    let detail = f.store.get_detail(m.id).expect("详情").expect("存在");
    assert_eq!(detail.meta.image.expect("图片").bytes, reencoded.len() as u64);

    // 文件完好时重复复制不重写原图
    let before = std::fs::metadata(f.store.blob_path(BlobKind::Img, &key))
        .expect("原图")
        .modified()
        .expect("时间");
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert!(matches!(
        f.ingest(&image_raw(bytes), NOW + 3),
        IngestOutcome::Touched(_)
    ));
    let after = std::fs::metadata(f.store.blob_path(BlobKind::Img, &key))
        .expect("原图")
        .modified()
        .expect("时间");
    assert_eq!(before, after);
    assert!(!f.blob_files(BlobKind::Img).iter().any(|n| n.starts_with(".tmp-")));
}
