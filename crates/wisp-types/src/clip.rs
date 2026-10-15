//! 剪贴板历史相关类型。

use serde::{Deserialize, Serialize};

use crate::app::AppRef;
use crate::geom::PhysSize;
use crate::style::OcrMode;

/// 条目类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipKind {
    Text,
    RichText,
    Image,
    Files,
    Link,
    Color,
    Code,
}

/// 列表过滤
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipFilter {
    #[default]
    All,
    Text,
    Image,
    Files,
    Link,
    Code,
    Color,
}

/// 来源应用（展示用）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceApp {
    pub id: String,
    pub name: String,
    pub icon_url: Option<String>,
}

/// 图片元数据
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageMeta {
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    /// 缩略图在后台生成，生成前为 null（生成后通过 clipboard:changed 推送）
    pub thumb_url: Option<String>,
}

/// 文件引用
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRef {
    pub path: String,
    pub name: String,
    pub exists: bool,
    pub size: Option<u64>,
}

/// 条目元数据（列表展示用）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipItemMeta {
    pub id: i64,
    pub kind: ClipKind,
    pub created_at: i64,
    pub last_used_at: i64,
    pub use_count: u32,
    pub pinned: bool,
    pub source: Option<SourceApp>,
    /// ≤200 字符，空白已压缩
    pub preview: String,
    pub char_count: Option<u32>,
    pub line_count: Option<u32>,
    pub truncated: bool,
    pub image: Option<ImageMeta>,
    /// 最多带前 5 个
    pub files: Option<Vec<FileRef>>,
    pub link: Option<String>,
    pub color: Option<String>,
}

/// 高亮区间：preview 中的 UTF-16 偏移
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HighlightRange {
    pub start: u32,
    pub end: u32,
}

/// 搜索 / 列表命中
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipHit {
    pub item: ClipItemMeta,
    pub highlights: Vec<HighlightRange>,
}

/// 一页结果
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipPage {
    pub items: Vec<ClipHit>,
    pub pinned_count: u32,
    pub next_offset: Option<u32>,
    pub query: Option<String>,
    pub filter: ClipFilter,
}

impl ClipPage {
    /// 空页
    pub fn empty(filter: ClipFilter) -> Self {
        Self {
            items: Vec::new(),
            pinned_count: 0,
            next_offset: None,
            query: None,
            filter,
        }
    }
}

/// 条目详情（Space 预览）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipItemDetail {
    pub meta: ClipItemMeta,
    pub text: Option<String>,
    pub has_html: bool,
    pub has_rtf: bool,
    pub image_url: Option<String>,
    pub files: Option<Vec<FileRef>>,
}

/// 粘贴方式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PasteMode {
    Normal,
    Plain,
    CopyOnly,
}

/// 只复制（未能粘贴）的原因
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CopiedReason {
    CopyOnly,
    Wayland,
    Elevated,
    TargetGone,
    FocusTimeout,
    NoPermission,
    /// 已复制、请手动按粘贴键，**不写原因**：没有对应原因的粘贴失败（X11 缺
    /// XTEST、该应用的粘贴键设为不发送、发键失败），以及不知道原应用（从托盘打开剪贴板、平台取不到前台应用）
    Manual,
}

/// 粘贴结果
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PasteOutcome {
    Pasted,
    Copied { reason: CopiedReason },
}

/// `clipboard:changed` 事件载荷
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "change", rename_all = "snake_case")]
pub enum ClipboardChanged {
    Inserted {
        item: ClipItemMeta,
    },
    Touched {
        item: ClipItemMeta,
    },
    /// 已有条目的元数据更新（目前用于后台缩略图生成完成：`image.thumb_url` 由 null 变为 URL）。
    /// 前端按 id 原地替换，不改变列表顺序
    Updated {
        item: ClipItemMeta,
    },
    Removed {
        id: i64,
    },
    Cleared {
        keep_pinned: bool,
    },
    Paused {
        paused: bool,
    },
    SecretSkipped,
    /// 一批改动（例如同步带来的大量新增、删除）不适合逐条推送：前端重新拉取列表首页
    Reloaded,
}

/// 平台读到的原始剪贴板内容（platform 产出，clipboard 消费）
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RawClipboard {
    pub change_id: u64,
    pub source: Option<AppRef>,
    /// 命中隐私标记时为 true，此时下面各字段全部为空
    pub concealed: bool,
    /// 带有 Wisp 私有标记
    pub from_wisp: bool,
    pub text: Option<String>,
    pub text_truncated: bool,
    pub html: Option<String>,
    pub rtf: Option<Vec<u8>>,
    pub image_png: Option<Vec<u8>>,
    pub image_size: Option<PhysSize>,
    pub files: Option<Vec<String>>,
}

/// 写回剪贴板的内容（粘贴时由 clipboard 产出，platform 消费）
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ClipPayload {
    pub text: Option<String>,
    pub html: Option<String>,
    pub rtf: Option<Vec<u8>>,
    pub image_png: Option<Vec<u8>>,
    pub files: Option<Vec<String>>,
}

impl ClipPayload {
    /// 纯文本载荷
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            ..Self::default()
        }
    }
}

/// OCR 历史条目
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OcrHistoryItem {
    pub id: i64,
    /// Unix 毫秒
    pub created_at: i64,
    pub mode: OcrMode,
    /// 模型显示名（记录时的 `StreamEvent::Start.model_label`）
    pub model: String,
    /// 识别结果全文（translate 模式是译文）
    pub text: String,
    /// 缩略图的资源路径（`ocrimg/<key>`，前端经 `resolveAssetUrl`）
    pub thumb_url: Option<String>,
    /// 原图的资源路径（`ocrimg/<key>`，「回看」时放大显示）；没有保存原图时为 null
    pub image_url: Option<String>,
}

/// `ocr_history:changed` 事件载荷→ settings：识别历史列表据此原地增删（有进入 / 退出动画），不整页重拉
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "change", rename_all = "snake_case")]
pub enum OcrHistoryChanged {
    /// 新记录（插在最前面）
    Inserted { item: OcrHistoryItem },
    /// 删除单条（`ocr_history_delete`）或按 `ocr.history_limit` 裁剪掉的旧记录
    Removed { ids: Vec<i64> },
    /// 全部清空（`ocr_history_clear`、关闭历史并清空）
    Cleared,
}
