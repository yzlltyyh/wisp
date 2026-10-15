//! 体积目标：1000 条文本 + 100 张图片时 `clipboard.db` < 20 MB（图片按原图另计，存在 blobs/）。

mod common;

use common::{Fixture, NOW, corpus::clip_text, expect_inserted, image_raw, png};
use wisp_types::RawClipboard;

/// 数据库文件（含 WAL）的总字节数
fn db_bytes(f: &Fixture) -> u64 {
    ["clipboard.db", "clipboard.db-wal"]
        .iter()
        .filter_map(|n| std::fs::metadata(f.dir.path().join(n)).ok())
        .map(|m| m.len())
        .sum()
}

#[test]
fn thousand_texts_and_hundred_images_stay_under_20_mb() {
    let f = Fixture::new();
    let mut text_bytes = 0usize;
    for i in 0..1000u64 {
        let (text, html) = clip_text(i);
        text_bytes += text.len() + html.as_ref().map_or(0, String::len);
        let raw = RawClipboard {
            text: Some(text),
            html,
            ..RawClipboard::default()
        };
        common::expect_meta(f.ingest(&raw, NOW + i as i64));
    }
    for i in 0..100u32 {
        let m = expect_inserted(f.ingest(
            &image_raw(png(24, 16, false, move |x, y| [i as u8, x as u8, y as u8, 255])),
            NOW + 2000 + i64::from(i),
        ));
        f.store.make_thumb(m.id).expect("缩略图");
    }
    // 落盘后（检查点之后）测量
    f.side_conn()
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .expect("检查点");
    let size = db_bytes(&f);
    let mb = size as f64 / 1_048_576.0;
    assert!(
        size < 20 * 1024 * 1024,
        "clipboard.db = {mb:.2} MB（正文 {:.2} MB）",
        text_bytes as f64 / 1_048_576.0
    );
    assert_eq!(f.count("SELECT count(*) FROM items"), 1100);
}
