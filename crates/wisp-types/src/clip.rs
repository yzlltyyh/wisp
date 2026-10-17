//! 剪贴板历史相关类型。

use serde::{Deserialize, Serialize};

use crate::app::AppRef;
use crate::geom::PhysSize;
use crate::macros::str_enum;
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
    /// 收藏（`ClipItemMeta.pinned`）：按条目状态而不是类型过滤，判断时要看整条元数据
    Favorite,
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
    /// 收藏的名称（只有收藏的条目才有；≤ 64 字符）。没有时省略（TS：`name?: string | null`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 验证码条目在本机自动删除的时间（Unix 毫秒）；收藏后去掉。没有时省略（TS：`expires_at?: number | null`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
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

/// 拖出条目时放进拖拽数据的内容（`clipboard_drag_data`）。拖拽开始的回调必须同步写数据，前端在按下时先取好：
/// 一条时是它的纯文本（文件条目是仍存在的路径，逐行）、富文本的 HTML、链接；多条时按选择顺序以换行合并的纯文本
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipDragData {
    pub text: String,
    /// 富文本条目的 HTML（`text/html`）；其余为 null
    pub html: Option<String>,
    /// 链接条目的地址（`text/uri-list`）；其余为 null
    pub uri: Option<String>,
}

// 手写 Debug：内容是用户的剪贴板，只输出长度
impl std::fmt::Debug for ClipDragData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipDragData")
            .field("text_bytes", &self.text.len())
            .field("html", &self.html.as_ref().map(String::len))
            .field("uri", &self.uri.is_some())
            .finish()
    }
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

str_enum! {
    /// 智能识别出的片段种类。识别逻辑在 wisp-clipboard（每种一个识别器），前端只按种类决定图标与文案
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
    pub enum EntityKind {
        Email = "email",
        /// 中国大陆手机号、带区号的固话、E.164 国际号码
        Phone = "phone",
        Url = "url",
        /// IPv4（每段 0–255）
        Ip = "ip",
        /// 验证码：关键词附近的 4–8 位数字或字母数字
        Otp = "otp",
        /// 快递单号
        Tracking = "tracking",
        /// 「订单号」语境下的长编号
        Order = "order",
        /// 带货币符号或「元」的金额
        Amount = "amount",
        /// 日期 / 日期时间
        Datetime = "datetime",
        /// 18 位居民身份证号（校验位正确）
        IdCard = "id_card",
        /// 银行卡号（Luhn 校验通过）
        BankCard = "bank_card",
    }
}

impl EntityKind {
    /// 敏感种类：芯片里中间遮罩显示，识别默认关闭
    pub const fn is_sensitive(self) -> bool {
        matches!(self, Self::IdCard | Self::BankCard)
    }
}

/// 识别出的一个片段
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Entity {
    pub kind: EntityKind,
    /// 粘贴用的值（已规范化：电话、卡号去掉空格与连字符，金额去掉货币符号与千位分隔符）
    pub value: String,
    /// 在原文中的位置：UTF-16 偏移（与 `HighlightRange` 一致，前端直接用于高亮）
    pub start: u32,
    pub end: u32,
    /// 敏感内容（证件号、银行卡号）：显示时遮罩中间部分
    pub sensitive: bool,
}

str_enum! {
    /// 「粘贴为…」的文本变换。实现在 wisp-clipboard（每种一个变换），前端只负责展示。声明顺序（`ALL`）也是菜单里的顺序
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
    pub enum TransformId {
        /// 合并为一行
        JoinLines = "join_lines",
        /// 合并段内换行：以空行为段界，段内被硬换行断开的行连成一行，列表、代码、对齐的列保持原样
        Reflow = "reflow",
        /// 去掉空行（只含空白的行），其余各行原样保留
        RemoveBlankLines = "remove_blank_lines",
        /// 去掉首尾空白
        Trim = "trim",
        Upper = "upper",
        Lower = "lower",
        /// 每个单词首字母大写
        Title = "title",
        /// 全角 → 半角
        HalfWidth = "half_width",
        /// 半角 → 全角
        FullWidth = "full_width",
        /// JSON 格式化（保留键的顺序）
        JsonPretty = "json_pretty",
        /// JSON 压缩为一行
        JsonMinify = "json_minify",
        /// 去掉链接里的跟踪参数（utm_*、fbclid 等）
        StripTracking = "strip_tracking",
    }
}

/// 一条内容的检视结果（`clipboard_inspect` / `text_inspect`）：芯片条、「粘贴为…」与 ⌘K 子页共用一次结果
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ClipInspect {
    /// 按在原文中的位置排序，互不重叠
    pub entities: Vec<Entity>,
    /// 适用于这条内容的变换（按 [`TransformId::ALL`] 的顺序）
    pub transforms: Vec<TransformId>,
}

/// 连续粘贴的进度（`clipboard_queue_start` 的返回值与 `clipboard:queue` 事件载荷）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct QueueStatus {
    /// 队列进行中（为 false 时已结束，粘贴键不再被拦截）
    pub active: bool,
    /// 已经粘贴的条数（0..=total），界面显示为「index / total」
    pub index: u32,
    pub total: u32,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::SmartSettings;

    #[test]
    fn enum_strings_match_serde() {
        for k in EntityKind::ALL {
            assert_eq!(serde_json::to_value(k).expect("json"), k.as_str());
        }
        for t in TransformId::ALL {
            assert_eq!(serde_json::to_value(t).expect("json"), t.as_str());
        }
    }

    #[test]
    fn smart_settings_gate_every_kind_by_group() {
        let all_off = SmartSettings {
            contacts: false,
            codes: false,
            numbers: false,
            identity: false,
            otp_auto_delete: false,
        };
        for k in EntityKind::ALL {
            assert!(!all_off.allows(k), "{k:?}");
        }
        let d = SmartSettings::default();
        // 默认只关敏感的两种，且敏感与否与分组一致
        for k in EntityKind::ALL {
            assert_eq!(d.allows(k), !k.is_sensitive(), "{k:?}");
        }
        let id_only = SmartSettings {
            identity: true,
            ..all_off
        };
        assert!(id_only.allows(EntityKind::IdCard) && id_only.allows(EntityKind::BankCard));
        assert!(!id_only.allows(EntityKind::Phone));
    }
}
