//! 集成测试的公共工具。
#![allow(dead_code)] // 各测试文件只用到其中一部分

pub mod corpus;

use std::path::{Path, PathBuf};

use wisp_clipboard::{BlobKind, ClipStore, IngestOutcome, IngestPolicy, PrivacyRules};
use wisp_types::{AppRef, ClipItemMeta, RawClipboard, defaults::default_excluded_apps};

/// 固定的「现在」
pub const NOW: i64 = 1_760_000_000_000;

/// 临时数据目录 + 打开的 store
pub struct Fixture {
    pub dir: tempfile::TempDir,
    pub store: ClipStore,
}

impl Fixture {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("临时目录");
        let store = ClipStore::open(dir.path()).expect("打开");
        Self { dir, store }
    }

    pub fn db_path(&self) -> PathBuf {
        self.dir.path().join("clipboard.db")
    }

    /// 另开一个连接（WAL 下与 store 的连接并存），用于检查表结构或模拟外部修改
    pub fn side_conn(&self) -> rusqlite::Connection {
        let c = rusqlite::Connection::open(self.db_path()).expect("旁路连接");
        c.busy_timeout(std::time::Duration::from_secs(5)).expect("busy timeout");
        c
    }

    pub fn count(&self, sql: &str) -> i64 {
        self.side_conn().query_row(sql, [], |r| r.get(0)).expect("查询")
    }

    /// 某个 blob 目录下的文件名
    pub fn blob_files(&self, kind: BlobKind) -> Vec<String> {
        list_files(&self.dir.path().join("blobs").join(kind.dir()))
    }

    pub fn ingest(&self, raw: &RawClipboard, now: i64) -> IngestOutcome {
        self.store.ingest(raw, &policy(), now).expect("ingest")
    }

    /// 记录一段文本，返回元数据（插入或 touch）
    pub fn text(&self, text: &str, now: i64) -> ClipItemMeta {
        expect_meta(self.ingest(&raw_text(text), now))
    }
}

pub fn list_files(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|e| {
            e.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// 默认策略（与设置默认值一致）
pub fn policy() -> IngestPolicy {
    IngestPolicy {
        record_text: true,
        record_rich_text: true,
        record_images: true,
        record_files: true,
        max_text_bytes: 1_048_576,
        max_image_bytes: 20_971_520,
        privacy: PrivacyRules {
            excluded_apps: default_excluded_apps(),
            secret_detection: true,
        },
    }
}

pub fn raw_text(text: &str) -> RawClipboard {
    RawClipboard {
        text: Some(text.to_owned()),
        ..RawClipboard::default()
    }
}

pub fn app(id: &str, name: &str) -> AppRef {
    AppRef {
        id: id.to_owned(),
        alt_id: None,
        name: name.to_owned(),
        pid: Some(42),
        path: None,
        window: None,
    }
}

pub fn expect_meta(o: IngestOutcome) -> ClipItemMeta {
    match o {
        IngestOutcome::Inserted(m) | IngestOutcome::Touched(m) => m,
        IngestOutcome::Skipped(r) => panic!("意外跳过：{r:?}"),
    }
}

pub fn expect_inserted(o: IngestOutcome) -> ClipItemMeta {
    match o {
        IngestOutcome::Inserted(m) => m,
        other => panic!("应为 Inserted：{other:?}"),
    }
}

/// 生成 PNG：`f(x, y)` 给出 RGBA 像素；`alpha = false` 时按 RGB 编码
pub fn png(w: u32, h: u32, alpha: bool, f: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
    let channels = if alpha { 4 } else { 3 };
    let mut data = Vec::with_capacity((w * h) as usize * channels);
    for y in 0..h {
        for x in 0..w {
            let p = f(x, y);
            data.extend_from_slice(&p[..channels]);
        }
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(if alpha {
            png::ColorType::Rgba
        } else {
            png::ColorType::Rgb
        });
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut wr = enc.write_header().expect("头");
        wr.write_image_data(&data).expect("数据");
    }
    out
}

/// 一张像截图的 RGB 图：大片纯色 / 渐变的界面区域 + 文字般的细节 + 一块噪声（照片）
pub fn screenshot_like(w: u32, h: u32, seed: u32) -> Vec<u8> {
    png(w, h, false, |x, y| {
        let band = (y / 40) % 6;
        if x > w * 2 / 3 && y > h / 2 {
            // 照片区域：伪随机噪声
            let v = (x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503) ^ seed).wrapping_mul(2_246_822_519);
            [(v >> 8) as u8, (v >> 16) as u8, (v >> 24) as u8, 255]
        } else if (x / 7 + y / 11 + seed).is_multiple_of(13) && band % 2 == 1 {
            // 文字笔画
            [20, 20, 24, 255]
        } else {
            let g = 230 - band as u8 * 6;
            [g, g, (g as u32 + (x * 10 / w.max(1))) as u8, 255]
        }
    })
}

pub fn image_raw(png_bytes: Vec<u8>) -> RawClipboard {
    RawClipboard {
        image_png: Some(png_bytes),
        ..RawClipboard::default()
    }
}
