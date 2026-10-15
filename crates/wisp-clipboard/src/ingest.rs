//! 记录管线的纯逻辑部分：隐私检查 → 选择主类型 → 规范化 → 计算 hash。
//! 落库（去重或插入）在 `store.rs`；图片的像素 hash 需要解码，在 `imaging.rs`。
//!
//! 各阶段顺序（命中即丢弃，库中不留任何痕迹）：
//! 1. 平台隐私标记（`RawClipboard.concealed`，不可关闭）→ [`SkipReason::Concealed`]
//! 2. 应用排除名单 → [`SkipReason::ExcludedApp`]
//! 3. 疑似密钥（可关）→ [`SkipReason::Secret`]
//! 4. 选择主类型：文件 > 文本 > 图片（图片同时带着「整段是一个 URL」或空白文本时——浏览器「拷贝图像」——以图片为准）；
//!    该类型被设置关闭 → [`SkipReason::KindDisabled`]；什么都没有 → [`SkipReason::Empty`]
//! 5. 规范化：文本只去掉末尾空白、超过上限截断并标 truncated；HTML 剥离后 > 256 KB 丢弃；RTF > 256 KB 丢弃；
//!    图片超过上限 → [`SkipReason::TooLarge`]
//! 6. hash：文本取规范化后的 UTF-8；文件取排序后的路径列表；图片取解码后的 RGBA（含宽高，见 `imaging.rs`）

use wisp_types::{ClipItemMeta, ClipKind, ClipboardSettings, RawClipboard};

use crate::html::{html_to_text, sanitize_html};
use crate::infer::{infer_kind, prefix_at_char_boundary};
use crate::normalize::{
    MAX_RICH_BYTES, PREVIEW_CHARS, char_count, file_name_of, line_count, make_preview, normalize_text,
};
use crate::privacy::{PrivacyRules, looks_like_secret};

/// 疑似密钥检测在 `max_text_bytes` 之外多看的字节数
const SECRET_SCAN_MARGIN: usize = 4096;

/// 记录策略（由 ClipboardSettings 生成）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestPolicy {
    pub record_text: bool,
    pub record_rich_text: bool,
    pub record_images: bool,
    pub record_files: bool,
    pub max_text_bytes: usize,
    pub max_image_bytes: usize,
    pub privacy: PrivacyRules,
}

impl From<&ClipboardSettings> for IngestPolicy {
    fn from(s: &ClipboardSettings) -> Self {
        Self {
            record_text: s.record_text,
            record_rich_text: s.record_rich_text,
            record_images: s.record_images,
            record_files: s.record_files,
            max_text_bytes: usize::try_from(s.max_text_bytes).unwrap_or(usize::MAX),
            max_image_bytes: usize::try_from(s.max_image_bytes).unwrap_or(usize::MAX),
            privacy: PrivacyRules {
                excluded_apps: s.excluded_apps.clone(),
                secret_detection: s.secret_detection,
            },
        }
    }
}

/// 记录结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestOutcome {
    Inserted(ClipItemMeta),
    /// 与已有条目重复：只更新时间和次数
    Touched(ClipItemMeta),
    Skipped(SkipReason),
}

/// 跳过原因（只记原因，不记内容）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SkipReason {
    Concealed,
    ExcludedApp,
    Secret,
    KindDisabled,
    TooLarge,
    Empty,
}

/// 规范化后的待写入内容
#[derive(Debug)]
pub(crate) enum Prepared<'a> {
    Text(TextRecord),
    /// 原始 PNG 字节（像素 hash 与尺寸在落库前由 `imaging::inspect_png` 计算）
    Image(&'a [u8]),
    Files(FilesRecord),
}

/// 文本类条目（Text / RichText / Link / Color / Code）
#[derive(Debug)]
pub(crate) struct TextRecord {
    pub kind: ClipKind,
    pub text: String,
    pub html: Option<String>,
    pub rtf: Option<Vec<u8>>,
    pub truncated: bool,
    pub preview: String,
    pub char_count: u32,
    pub line_count: u32,
    pub hash: [u8; 32],
}

/// 文件条目
#[derive(Debug)]
pub(crate) struct FilesRecord {
    /// 保持原顺序（粘贴与展示用）；hash 用排序后的列表
    pub paths: Vec<String>,
    /// 检索与纯文本粘贴用：路径逐行拼接
    pub text: String,
    pub truncated: bool,
    pub preview: String,
    pub hash: [u8; 32],
}

/// 执行管线的 1–6 步（图片的 hash 除外）
pub(crate) fn prepare<'a>(raw: &'a RawClipboard, policy: &IngestPolicy) -> Result<Prepared<'a>, SkipReason> {
    // 1–2：平台标记与排除名单
    if raw.concealed {
        return Err(SkipReason::Concealed);
    }
    if policy.privacy.excludes(raw.source.as_ref()) {
        return Err(SkipReason::ExcludedApp);
    }

    // 纯文本：平台给的 text；没有（或全是空白）时从 HTML 提取
    let derived;
    let plain: Option<&str> = match raw.text.as_deref() {
        Some(t) if !t.trim().is_empty() => Some(t),
        _ => match raw.html.as_deref() {
            Some(h) => {
                derived = html_to_text(h);
                (!derived.is_empty()).then_some(derived.as_str())
            }
            None => None,
        },
    };

    // 3：疑似密钥。只检查会入库的部分（超出 max_text_bytes 的部分会被截掉），留一点余量覆盖跨越截断处的令牌
    let scan_limit = policy.max_text_bytes.saturating_add(SECRET_SCAN_MARGIN);
    if policy.privacy.secret_detection
        && plain.is_some_and(|t| looks_like_secret(prefix_at_char_boundary(t, scan_limit)))
    {
        return Err(SkipReason::Secret);
    }

    // 4–5：主类型与规范化
    let files: Vec<&str> = raw
        .files
        .iter()
        .flatten()
        .map(|p| p.trim_end_matches(['\r', '\n']))
        .filter(|p| !p.trim().is_empty())
        .collect();
    if !files.is_empty() {
        if !policy.record_files {
            return Err(SkipReason::KindDisabled);
        }
        return Ok(Prepared::Files(files_record(&files, policy.max_text_bytes)));
    }

    let image = raw.image_png.as_deref().filter(|b| !b.is_empty());
    let image_preferred = image.is_some() && plain.is_none_or(|t| infer_kind(t) == ClipKind::Link);
    if let Some(text) = plain.filter(|_| !image_preferred) {
        let (norm, truncated) = normalize_text(text, policy.max_text_bytes);
        if !norm.is_empty() {
            return text_record(raw, policy, norm, truncated || raw.text_truncated).map(Prepared::Text);
        }
    }

    if let Some(png) = image {
        if !policy.record_images {
            return Err(SkipReason::KindDisabled);
        }
        if png.len() > policy.max_image_bytes {
            return Err(SkipReason::TooLarge);
        }
        return Ok(Prepared::Image(png));
    }
    Err(SkipReason::Empty)
}

fn text_record(
    raw: &RawClipboard,
    policy: &IngestPolicy,
    text: &str,
    truncated: bool,
) -> Result<TextRecord, SkipReason> {
    let (html, rtf) = if policy.record_rich_text {
        let html = raw
            .html
            .as_deref()
            .map(sanitize_html)
            .filter(|h| !h.trim().is_empty() && h.len() <= MAX_RICH_BYTES);
        let rtf = raw
            .rtf
            .as_ref()
            .filter(|r| !r.is_empty() && r.len() <= MAX_RICH_BYTES)
            .cloned();
        (html, rtf)
    } else {
        (None, None)
    };
    let rich = html.is_some() || rtf.is_some();
    if !rich && !policy.record_text {
        return Err(SkipReason::KindDisabled);
    }
    let inferred = infer_kind(text);
    let kind = if inferred == ClipKind::Text && rich {
        ClipKind::RichText
    } else {
        inferred
    };
    Ok(TextRecord {
        kind,
        preview: make_preview(text, PREVIEW_CHARS),
        char_count: char_count(text),
        line_count: line_count(text),
        hash: text_hash(text),
        text: text.to_owned(),
        html,
        rtf,
        truncated,
    })
}

fn files_record(paths: &[&str], max_text_bytes: usize) -> FilesRecord {
    let joined = paths.join("\n");
    let (text, truncated) = normalize_text(&joined, max_text_bytes);
    let names: Vec<String> = paths.iter().map(|p| file_name_of(p)).collect();
    FilesRecord {
        preview: make_preview(&names.join(", "), PREVIEW_CHARS),
        text: text.to_owned(),
        truncated,
        hash: files_hash(paths),
        paths: paths.iter().map(|p| (*p).to_owned()).collect(),
    }
}

/// 文本去重键：规范化后 UTF-8 的 blake3（加域前缀，与文件 / 图片的 hash 不会相撞）
pub(crate) fn text_hash(text: &str) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"wisp:text\0");
    h.update(text.as_bytes());
    *h.finalize().as_bytes()
}

/// 文件去重键：排序后的路径列表的 blake3（顺序不同的同一组文件只有一条）
pub(crate) fn files_hash(paths: &[&str]) -> [u8; 32] {
    let mut sorted = paths.to_vec();
    sorted.sort_unstable();
    let mut h = blake3::Hasher::new();
    h.update(b"wisp:files\0");
    for p in sorted {
        h.update(p.as_bytes());
        h.update(b"\0");
    }
    *h.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wisp_types::{AppRef, ExcludedApp, OsKind};

    fn policy() -> IngestPolicy {
        IngestPolicy {
            record_text: true,
            record_rich_text: true,
            record_images: true,
            record_files: true,
            max_text_bytes: 1 << 20,
            max_image_bytes: 20 << 20,
            privacy: PrivacyRules {
                excluded_apps: vec![ExcludedApp {
                    os: OsKind::Macos,
                    app_id: "com.bitwarden.desktop".into(),
                    name: "Bitwarden".into(),
                }],
                secret_detection: true,
            },
        }
    }

    fn raw_text(t: &str) -> RawClipboard {
        RawClipboard {
            text: Some(t.into()),
            ..RawClipboard::default()
        }
    }

    fn app(id: &str) -> Option<AppRef> {
        Some(AppRef {
            id: id.into(),
            alt_id: None,
            name: id.into(),
            pid: None,
            path: None,
            window: None,
        })
    }

    const SECRET: &str = "sk-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789AbCdEfGhIjKl";

    fn skip(raw: &RawClipboard, p: &IngestPolicy) -> SkipReason {
        prepare(raw, p).expect_err("应被跳过")
    }

    #[test]
    fn privacy_stages_run_in_order() {
        let p = policy();
        // 平台标记优先于排除名单与密钥
        let mut raw = raw_text(SECRET);
        raw.concealed = true;
        raw.source = app("com.bitwarden.desktop");
        assert_eq!(skip(&raw, &p), SkipReason::Concealed);
        // 排除名单优先于密钥
        raw.concealed = false;
        assert_eq!(skip(&raw, &p), SkipReason::ExcludedApp);
        // 密钥优先于类型开关
        raw.source = app("com.apple.TextEdit");
        let mut off = policy();
        off.record_text = false;
        assert_eq!(skip(&raw, &off), SkipReason::Secret);
        // 关闭密钥检测后才轮到类型开关
        off.privacy.secret_detection = false;
        assert_eq!(skip(&raw, &off), SkipReason::KindDisabled);
        // 全部放行
        off.record_text = true;
        assert!(matches!(prepare(&raw, &off), Ok(Prepared::Text(_))));
    }

    #[test]
    fn secret_in_html_only_is_detected() {
        let raw = RawClipboard {
            html: Some(format!("<p>{SECRET}</p>")),
            ..RawClipboard::default()
        };
        assert_eq!(skip(&raw, &policy()), SkipReason::Secret);
    }

    #[test]
    fn empty_inputs() {
        let p = policy();
        assert_eq!(skip(&RawClipboard::default(), &p), SkipReason::Empty);
        assert_eq!(skip(&raw_text("  \n\t "), &p), SkipReason::Empty);
        let raw = RawClipboard {
            files: Some(vec![String::new(), "  ".into()]),
            image_png: Some(Vec::new()),
            ..RawClipboard::default()
        };
        assert_eq!(skip(&raw, &p), SkipReason::Empty);
    }

    #[test]
    fn primary_kind_selection() {
        let p = policy();
        // 文件优先
        let raw = RawClipboard {
            text: Some("a.txt".into()),
            files: Some(vec!["/tmp/a.txt".into()]),
            image_png: Some(vec![1, 2, 3]),
            ..RawClipboard::default()
        };
        assert!(matches!(prepare(&raw, &p), Ok(Prepared::Files(_))));
        // 文本优先于图片（Office 复制会附带渲染图）
        let raw = RawClipboard {
            text: Some("cell A1\tcell B1".into()),
            html: Some("<table><tr><td>cell A1</td></tr></table>".into()),
            image_png: Some(vec![1, 2, 3]),
            ..RawClipboard::default()
        };
        let Ok(Prepared::Text(t)) = prepare(&raw, &p) else {
            panic!("应为文本")
        };
        assert_eq!(t.kind, ClipKind::RichText);
        // 浏览器「拷贝图像」：图片 + URL 文本 → 图片
        let raw = RawClipboard {
            text: Some("https://example.com/cat.png".into()),
            html: Some("<img src=\"https://example.com/cat.png\">".into()),
            image_png: Some(vec![1, 2, 3]),
            ..RawClipboard::default()
        };
        assert!(matches!(prepare(&raw, &p), Ok(Prepared::Image(_))));
    }

    #[test]
    fn kind_switches() {
        let mut p = policy();
        p.record_files = false;
        let raw = RawClipboard {
            files: Some(vec!["/tmp/a".into()]),
            ..RawClipboard::default()
        };
        assert_eq!(skip(&raw, &p), SkipReason::KindDisabled);

        // 关闭富文本：降级为纯文本
        let mut p = policy();
        p.record_rich_text = false;
        let raw = RawClipboard {
            text: Some("bold".into()),
            html: Some("<b>bold</b>".into()),
            rtf: Some(b"{\\rtf1 bold}".to_vec()),
            ..RawClipboard::default()
        };
        let Ok(Prepared::Text(t)) = prepare(&raw, &p) else {
            panic!()
        };
        assert_eq!(
            (t.kind, t.html.is_none(), t.rtf.is_none()),
            (ClipKind::Text, true, true)
        );

        // 关闭纯文本但开着富文本：富文本照记，纯文本跳过
        let mut p = policy();
        p.record_text = false;
        let Ok(Prepared::Text(t)) = prepare(&raw, &p) else {
            panic!()
        };
        assert_eq!(t.kind, ClipKind::RichText);
        assert_eq!(skip(&raw_text("plain"), &p), SkipReason::KindDisabled);

        let mut p = policy();
        p.record_images = false;
        let raw = RawClipboard {
            image_png: Some(vec![1]),
            ..RawClipboard::default()
        };
        assert_eq!(skip(&raw, &p), SkipReason::KindDisabled);
    }

    #[test]
    fn size_limits() {
        let mut p = policy();
        p.max_image_bytes = 10;
        let raw = RawClipboard {
            image_png: Some(vec![0; 11]),
            ..RawClipboard::default()
        };
        assert_eq!(skip(&raw, &p), SkipReason::TooLarge);

        // 1.5 MB 文本 → 1 MB 且 truncated
        let p = policy();
        let big = "a".repeat(1_572_864);
        let Ok(Prepared::Text(t)) = prepare(&raw_text(&big), &p) else {
            panic!()
        };
        assert_eq!((t.text.len(), t.truncated), (1 << 20, true));

        // 平台已经截断
        let mut raw = raw_text("short");
        raw.text_truncated = true;
        let Ok(Prepared::Text(t)) = prepare(&raw, &p) else {
            panic!()
        };
        assert!(t.truncated);

        // HTML / RTF 超过 256 KB 丢弃该格式，kind 降为推断结果
        let raw = RawClipboard {
            text: Some("x".into()),
            html: Some(format!("<p>{}</p>", "y".repeat(MAX_RICH_BYTES))),
            rtf: Some(vec![b'r'; MAX_RICH_BYTES + 1]),
            ..RawClipboard::default()
        };
        let Ok(Prepared::Text(t)) = prepare(&raw, &p) else {
            panic!()
        };
        assert_eq!(
            (t.kind, t.html.is_none(), t.rtf.is_none()),
            (ClipKind::Text, true, true)
        );
    }

    #[test]
    fn text_normalization_and_meta() {
        let p = policy();
        let Ok(Prepared::Text(t)) = prepare(&raw_text("  line one\n\tline two  \n\n"), &p) else {
            panic!()
        };
        assert_eq!(t.text, "  line one\n\tline two");
        assert_eq!(t.preview, "line one line two");
        assert_eq!((t.char_count, t.line_count), (20, 2));
        // 末尾空白不同的同一文本 hash 相同
        let Ok(Prepared::Text(t2)) = prepare(&raw_text("  line one\n\tline two"), &p) else {
            panic!()
        };
        assert_eq!(t.hash, t2.hash);
        // 首部空白不同则不同
        let Ok(Prepared::Text(t3)) = prepare(&raw_text("line one\n\tline two"), &p) else {
            panic!()
        };
        assert_ne!(t.hash, t3.hash);
    }

    #[test]
    fn html_only_derives_text_and_keeps_sanitized_html() {
        let raw = RawClipboard {
            html: Some("<meta charset=utf-8><p onclick=x()>Hello <b>world</b></p><script>bad()</script>".into()),
            ..RawClipboard::default()
        };
        let Ok(Prepared::Text(t)) = prepare(&raw, &policy()) else {
            panic!()
        };
        assert_eq!(t.text, "Hello world");
        assert_eq!(t.html.as_deref(), Some("<p>Hello <b>world</b></p>"));
        assert_eq!(t.kind, ClipKind::RichText);
    }

    #[test]
    fn html_only_with_indented_source_collapses_whitespace() {
        // 只有 HTML 的复制：纯文本按渲染结果提取，缩进与源码换行不进入正文、行数与 hash
        let raw = RawClipboard {
            html: Some(
                "<div>\n    <p>Hello\n      world,   this is\n      wrapped source\n    </p>\n\n    <ul>\n      \
                 <li>one</li>\n      <li>two</li>\n    </ul>\n  </div>"
                    .into(),
            ),
            ..RawClipboard::default()
        };
        let Ok(Prepared::Text(t)) = prepare(&raw, &policy()) else {
            panic!()
        };
        assert_eq!(t.text, "Hello world, this is wrapped source\none\ntwo");
        assert_eq!(t.line_count, 3);
        assert_eq!(t.hash, text_hash("Hello world, this is wrapped source\none\ntwo"));
    }

    #[test]
    fn inferred_kinds_keep_rich_parts() {
        // VS Code 复制代码会附带 HTML：kind 仍按文本推断为 Code，HTML 保留供粘贴
        let raw = RawClipboard {
            text: Some("fn main() {\n    run();\n}".into()),
            html: Some("<div><span>fn</span> main() {}</div>".into()),
            ..RawClipboard::default()
        };
        let Ok(Prepared::Text(t)) = prepare(&raw, &policy()) else {
            panic!()
        };
        assert_eq!(t.kind, ClipKind::Code);
        assert!(t.html.is_some());
    }

    #[test]
    fn files_hash_ignores_order() {
        assert_eq!(files_hash(&["/a", "/b"]), files_hash(&["/b", "/a"]));
        assert_ne!(files_hash(&["/a", "/b"]), files_hash(&["/a"]));
        assert_ne!(files_hash(&["/a"]), text_hash("/a"));
        let raw = RawClipboard {
            files: Some(vec!["/Users/me/b.pdf".into(), "/Users/me/a.txt".into()]),
            ..RawClipboard::default()
        };
        let Ok(Prepared::Files(f)) = prepare(&raw, &policy()) else {
            panic!()
        };
        assert_eq!(f.paths, ["/Users/me/b.pdf", "/Users/me/a.txt"]);
        assert_eq!(f.preview, "b.pdf, a.txt");
        assert_eq!(f.text, "/Users/me/b.pdf\n/Users/me/a.txt");
    }
}
