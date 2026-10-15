//! 剪贴板的 `wisp://` 资源来源：`img/<key>`、`thumb/<key>`、`icon/<key>.png`、`ocrimg/<key>`（实现 `assets::AssetSource`）。
//!
//! 路径的防穿越校验已由 `AssetRegistry` 集中完成；这里另外**只接受自己生成的 key 形状**：
//! 32 位小写十六进制 + 该种类允许的扩展名（`img` / `icon` 只有 `.png`，`thumb` / `ocrimg` 为 `.jpg` 或 `.png`）。
//! 读的都是 Wisp 自己数据目录下的 blob；数据库还没打开时返回 404（列表渲染之前数据库必然已经打开）。

use std::sync::{Arc, Weak};

use wisp_clipboard::BlobKind;

use crate::assets::{Asset, AssetRegistry, AssetSource};

use super::engine::Engine;

/// 一种 blob 的资源来源
pub(crate) struct BlobAssets {
    engine: Weak<Engine>,
    kind: BlobKind,
}

/// 注册四个前缀
pub(crate) fn register(registry: &AssetRegistry, engine: &Arc<Engine>) {
    for kind in BlobKind::ALL {
        registry.register(
            kind.asset_prefix(),
            Arc::new(BlobAssets {
                engine: Arc::downgrade(engine),
                kind,
            }),
        );
    }
}

/// key 是否是该种类会生成的形状；是则返回 MIME
pub(crate) fn key_mime(kind: BlobKind, key: &str) -> Option<&'static str> {
    let (stem, ext) = key.split_once('.')?;
    if stem.len() != 32 || !stem.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let mime = match ext {
        "png" => "image/png",
        "jpg" => "image/jpeg",
        _ => return None,
    };
    let allowed = match kind {
        BlobKind::Img | BlobKind::Icon => ext == "png",
        BlobKind::Thumb | BlobKind::Ocr => true,
    };
    allowed.then_some(mime)
}

impl AssetSource for BlobAssets {
    fn fetch(&self, path: &str) -> Option<Asset> {
        let mime = key_mime(self.kind, path)?;
        let store = self.engine.upgrade()?.ready_store()?;
        match std::fs::read(store.blob_path(self.kind, path)) {
            Ok(bytes) => Some(Asset {
                bytes: Arc::new(bytes),
                mime,
                extra_headers: Vec::new(),
            }),
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    log::debug!("clipboard asset read failed kind={}: {e}", self.kind.dir());
                }
                None
            }
        }
    }
}
