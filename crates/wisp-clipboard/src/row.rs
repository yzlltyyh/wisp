//! 行 ↔ DTO：列表查询的列、`ClipKind` / `ClipFilter` 与库中字符串的对应、文件列表的存储格式。

use std::path::Path;

use serde::{Deserialize, Serialize};
use wisp_types::{ClipFilter, ClipHit, ClipItemMeta, ClipKind, FileRef, ImageMeta, SourceApp};

use crate::blobs::{BlobKind, asset_path};

/// 列表 / 元数据查询的列（顺序与 [`meta_from_row`] 一致）。列表不读正文与完整文件列表：这些列都排在表的前部
/// （见 `schema.rs`「列的顺序」），读一行只用到该行在 b-tree 页内的部分。Link / Color 的正文就是一个短值，例外
pub(crate) const META_COLS: &str = "id, kind, created_at, last_used_at, use_count, pinned, source_app_id, source_app_name, \
     icon_key, text_preview, json_extract(meta_json, '$.char_count'), json_extract(meta_json, '$.line_count'), truncated, \
     image_w, image_h, image_bytes, thumb_blob, files_head_json, CASE WHEN kind IN ('link', 'color') THEN text END";

/// 列表排序（置顶在前，其余按最近使用；同一毫秒按 id 新的在前）
pub(crate) const ORDER_RECENT: &str = "ORDER BY pinned DESC, last_used_at DESC, id DESC";

/// 每条元数据最多带的文件数（`files_head_json` 只存这么多）
pub(crate) const META_FILES: usize = 5;

/// ClipKind → 库中的字符串（与 serde 的 snake_case 一致）
pub(crate) fn kind_to_str(k: ClipKind) -> &'static str {
    match k {
        ClipKind::Text => "text",
        ClipKind::RichText => "rich_text",
        ClipKind::Image => "image",
        ClipKind::Files => "files",
        ClipKind::Link => "link",
        ClipKind::Color => "color",
        ClipKind::Code => "code",
    }
}

/// 库中的字符串 → ClipKind；未知值按 Text 处理（向前兼容）
pub(crate) fn kind_from_str(s: &str) -> ClipKind {
    match s {
        "rich_text" => ClipKind::RichText,
        "image" => ClipKind::Image,
        "files" => ClipKind::Files,
        "link" => ClipKind::Link,
        "color" => ClipKind::Color,
        "code" => ClipKind::Code,
        _ => ClipKind::Text,
    }
}

/// 过滤条件的 SQL 片段（常量，便于 prepare_cached 按过滤器缓存语句）。「文本」包含纯文本与富文本
pub(crate) fn filter_sql(f: ClipFilter) -> &'static str {
    match f {
        ClipFilter::All => "1",
        ClipFilter::Text => "kind IN ('text', 'rich_text')",
        ClipFilter::Image => "kind = 'image'",
        ClipFilter::Files => "kind = 'files'",
        ClipFilter::Link => "kind = 'link'",
        ClipFilter::Code => "kind = 'code'",
        ClipFilter::Color => "kind = 'color'",
    }
}

/// 内存中判断条目是否满足过滤器（与 [`filter_sql`] 一致）
pub(crate) fn filter_matches(f: ClipFilter, k: ClipKind) -> bool {
    match f {
        ClipFilter::All => true,
        ClipFilter::Text => matches!(k, ClipKind::Text | ClipKind::RichText),
        ClipFilter::Image => k == ClipKind::Image,
        ClipFilter::Files => k == ClipKind::Files,
        ClipFilter::Link => k == ClipKind::Link,
        ClipFilter::Code => k == ClipKind::Code,
        ClipFilter::Color => k == ClipKind::Color,
    }
}

/// `files_json` 中的一项（exists 不入库，读取时实时检查）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredFile {
    pub path: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

impl StoredFile {
    /// 入库时取名称与大小（目录没有大小；读不到元数据时为 None）
    pub(crate) fn capture(path: &str) -> Self {
        let size = std::fs::metadata(path).ok().filter(|m| m.is_file()).map(|m| m.len());
        Self {
            path: path.to_owned(),
            name: crate::normalize::file_name_of(path),
            size,
        }
    }

    /// 转成 DTO；exists 先置 true（未检查），由调用方经 [`refresh_file_exists`] 在后台检查
    pub(crate) fn to_ref(&self) -> FileRef {
        FileRef {
            path: self.path.clone(),
            name: self.name.clone(),
            exists: true,
            size: self.size,
        }
    }
}

/// 解析 `files_json` / `files_head_json`；损坏时返回空列表
pub(crate) fn parse_files(json: Option<&str>) -> Vec<StoredFile> {
    json.and_then(|j| serde_json::from_str(j).ok()).unwrap_or_default()
}

/// 文件列表入库：（完整列表 `files_json`，前 [`META_FILES`] 项 `files_head_json`）
pub(crate) fn files_columns(files: &[StoredFile]) -> serde_json::Result<(String, String)> {
    Ok((
        serde_json::to_string(files)?,
        serde_json::to_string(&files[..files.len().min(META_FILES)])?,
    ))
}

/// 读一行 [`META_COLS`]
pub(crate) fn meta_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ClipItemMeta> {
    let kind = kind_from_str(&r.get::<_, String>(1)?);
    let source_id: Option<String> = r.get(6)?;
    let source_name: Option<String> = r.get(7)?;
    let icon_key: Option<String> = r.get(8)?;
    let source = source_id.filter(|id| !id.is_empty()).map(|id| SourceApp {
        name: source_name.unwrap_or_default(),
        icon_url: icon_key
            .as_deref()
            .map(|k| asset_path(BlobKind::Icon, &format!("{k}.png"))),
        id,
    });
    let image = if kind == ClipKind::Image {
        Some(ImageMeta {
            width: to_u32(r.get::<_, Option<i64>>(13)?),
            height: to_u32(r.get::<_, Option<i64>>(14)?),
            bytes: r
                .get::<_, Option<i64>>(15)?
                .map_or(0, |b| u64::try_from(b).unwrap_or(0)),
            thumb_url: r.get::<_, Option<String>>(16)?.map(|k| asset_path(BlobKind::Thumb, &k)),
        })
    } else {
        None
    };
    let files = if kind == ClipKind::Files {
        let head = parse_files(r.get::<_, Option<String>>(17)?.as_deref());
        Some(head.iter().take(META_FILES).map(StoredFile::to_ref).collect())
    } else {
        None
    };
    let short_text: Option<String> = r.get(18)?;
    let (link, color) = match kind {
        ClipKind::Link => (short_text.as_deref().map(crate::infer::link_target), None),
        ClipKind::Color => (None, short_text.map(|t| t.trim().to_owned())),
        _ => (None, None),
    };
    Ok(ClipItemMeta {
        id: r.get(0)?,
        kind,
        created_at: r.get(2)?,
        last_used_at: r.get(3)?,
        use_count: to_u32(r.get::<_, Option<i64>>(4)?),
        pinned: r.get::<_, i64>(5)? != 0,
        source,
        preview: r.get::<_, Option<String>>(9)?.unwrap_or_default(),
        char_count: r.get::<_, Option<i64>>(10)?.map(|v| to_u32(Some(v))),
        line_count: r.get::<_, Option<i64>>(11)?.map(|v| to_u32(Some(v))),
        truncated: r.get::<_, i64>(12)? != 0,
        image,
        files,
        link,
        color,
    })
}

fn to_u32(v: Option<i64>) -> u32 {
    v.map_or(0, |v| u32::try_from(v.max(0)).unwrap_or(u32::MAX))
}

/// 实时检查文件条目的 `exists`（不存在的置灰）。返回 exists 有变化的条目 id。
///
/// `ClipStore::recent` / `search` 与 `RecentCache::page` 都不访问文件系统（不可达的网络卷、已拔出的磁盘上 stat 可能
/// 阻塞数秒），文件条目的 exists 先按 true 给出；调用方在后台线程对可视批次调用本函数，
/// 再对返回的 id 推送 `clipboard:changed` 的 `updated`。每条最多检查前 5 个路径（元数据里只带这么多）
pub fn refresh_file_exists(items: &mut [ClipHit]) -> Vec<i64> {
    let mut changed = Vec::new();
    for hit in items {
        let Some(files) = hit.item.files.as_mut() else { continue };
        let mut any = false;
        for f in files.iter_mut() {
            let exists = Path::new(&f.path).exists();
            if exists != f.exists {
                f.exists = exists;
                any = true;
            }
        }
        if any {
            changed.push(hit.item.id);
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip_and_match_serde() {
        for k in [
            ClipKind::Text,
            ClipKind::RichText,
            ClipKind::Image,
            ClipKind::Files,
            ClipKind::Link,
            ClipKind::Color,
            ClipKind::Code,
        ] {
            assert_eq!(kind_from_str(kind_to_str(k)), k);
            assert_eq!(serde_json::to_value(k).expect("序列化"), kind_to_str(k));
        }
        assert_eq!(kind_from_str("future_kind"), ClipKind::Text);
    }
}
