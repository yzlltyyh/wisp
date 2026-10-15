//! Win32 剪贴板读写（不用 clipboard-rs，直接用 windows crate 实现：避免依赖第二套 windows-core，并覆盖监听、快照与隐私格式）。
//!
//! - 读取：先查隐私格式再读正文；`CF_UNICODETEXT`、`HTML Format`、`Rich Text Format`、
//!   `PNG` 优先于 `CF_DIBV5` / `CF_DIB`、`CF_HDROP`；按 `ReadLimits` 截断或丢弃；
//! - 全类型快照与恢复：枚举全部格式，跳过系统合成的格式与 GDI / 私有句柄格式，`CF_ENHMETAFILE`
//!   按字节深拷贝，其余 HGLOBAL 复制字节；
//! - 写入：纯文本 / HTML + RTF + 纯文本 / PNG + DIBV5 / `CF_HDROP` + `Preferred DropEffect`，
//!   并附 Wisp 私有标记与（按需）排除历史 / 排除监视的格式。标记与排除格式先写，任一格式写失败就清空剪贴板，
//!   不留下没有标记、没有排除格式的残缺内容。
//!
//! 所有函数都假定在**临时线程**上执行（见 `util::run_with_timeout`）：`GetClipboardData` 触发延迟渲染、
//! `EmptyClipboard` 通知原所有者时都会同步等待其他进程，卡住时由调用方的超时兜底。`OpenClipboard` 失败时每 10 ms
//! 重试一次，直到该操作的打开预算用完。剪贴板打开期间只做拷贝与轻量解析，图片解码 / PNG 编码等
//! 耗时工作一律放在关闭剪贴板之后（写入时放在打开之前，各格式事先写进各自的全局内存），其他程序的
//! `OpenClipboard` 不会因 Wisp 而失败。
//!
//! 读取（含快照）时 `GetClipboardData` 会让原所有者渲染延迟格式，序号可能因此增加：读取方把这段序号报告给剪贴板
//! 线程登记为同内容别名（`ClipHub::read_began` / `read_ended`），监听方不会把 Wisp 自己的读取当成一次新的复制。

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ::windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use ::windows::Win32::Graphics::Gdi::{DeleteEnhMetaFile, GetEnhMetaFileBits, HENHMETAFILE, SetEnhMetaFileBits};
use ::windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardFormatNameW, GetClipboardOwner,
    GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use ::windows::Win32::System::Memory::{GHND, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};
use ::windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use ::windows::core::PCWSTR;
use wisp_types::{ClipPayload, PhysSize, RawClipboard};

use crate::common::secrets_markers::{
    WINDOWS_EXCLUDE_FORMATS, WINDOWS_ZERO_MEANS_PRIVATE_FORMATS, WISP_MARKER, windows_format_is_private,
};
use crate::error::PlatformError;
use crate::traits::WriteOpts;
use crate::types::ReadLimits;

use super::clip_format::{self as fmt, DibImage, Marker, MarkerKind};
use super::clip_watch::ClipHub;
use super::util::{RawHwnd, wide};

// 标准格式（winuser.h），用整数避免与 `CLIPBOARD_FORMAT` 类型来回转换
const CF_TEXT: u32 = 1;
const CF_BITMAP: u32 = 2;
const CF_METAFILEPICT: u32 = 3;
const CF_OEMTEXT: u32 = 7;
const CF_DIB: u32 = 8;
const CF_PALETTE: u32 = 9;
const CF_UNICODETEXT: u32 = 13;
const CF_ENHMETAFILE: u32 = 14;
const CF_HDROP: u32 = 15;
const CF_DIBV5: u32 = 17;
const CF_OWNERDISPLAY: u32 = 0x80;
const CF_DSPBITMAP: u32 = 0x82;
const CF_DSPMETAFILEPICT: u32 = 0x83;
const CF_DSPENHMETAFILE: u32 = 0x8E;
const CF_PRIVATE_RANGE: std::ops::RangeInclusive<u32> = 0x200..=0x2FF;
const CF_GDIOBJ_RANGE: std::ops::RangeInclusive<u32> = 0x300..=0x3FF;

/// DROPEFFECT_COPY
const DROPEFFECT_COPY: u32 = 1;

/// 快照与恢复时不复制的注册格式：OLE 剪贴板的内部管道（恢复后会指向已失效的数据对象）
const SNAPSHOT_SKIP_NAMES: &[&str] = &["DataObject", "Ole Private Data", "OwnerLink", "ObjectLink", "Link"];

/// `OpenClipboard` 被占用时的重试间隔
const OPEN_RETRY_EVERY: Duration = Duration::from_millis(10);
/// 读 / 写 / 恢复默认的打开预算：复制大范围表格时 Excel / Word 会占用剪贴板几十到上百毫秒
pub(crate) const OPEN_BUDGET: Duration = Duration::from_millis(250);

/// 注册格式的 id（进程内只注册一次）
struct Formats {
    html: u32,
    rtf: u32,
    png: u32,
    marker: u32,
    exclude_monitor: u32,
    /// 隐私格式（id, 名称）：判定规则见 `common::secrets_markers::windows_format_is_private`
    privacy: Vec<(u32, &'static str)>,
    can_history: u32,
    can_cloud: u32,
    drop_effect: u32,
}

fn register(name: &str) -> u32 {
    let w = wide(name);
    // SAFETY: 字符串以 NUL 结尾；失败时返回 0
    unsafe { RegisterClipboardFormatW(PCWSTR(w.as_ptr())) }
}

fn formats() -> &'static Formats {
    static F: OnceLock<Formats> = OnceLock::new();
    F.get_or_init(|| Formats {
        html: register("HTML Format"),
        rtf: register("Rich Text Format"),
        png: register("PNG"),
        marker: register(WISP_MARKER),
        exclude_monitor: register("ExcludeClipboardContentFromMonitorProcessing"),
        privacy: WINDOWS_EXCLUDE_FORMATS
            .iter()
            .chain(WINDOWS_ZERO_MEANS_PRIVATE_FORMATS)
            .map(|&n| (register(n), n))
            .filter(|(id, _)| *id != 0)
            .collect(),
        can_history: register("CanIncludeInClipboardHistory"),
        can_cloud: register("CanUploadToCloudClipboard"),
        drop_effect: register("Preferred DropEffect"),
    })
}

/// 当前剪贴板序号（`GetClipboardSequenceNumber`）
pub(crate) fn seq() -> u64 {
    // SAFETY: 纯查询
    u64::from(unsafe { GetClipboardSequenceNumber() })
}

/// 打开剪贴板的守卫：Drop 时 `CloseClipboard`（必须在同一线程）
struct ClipGuard;

impl ClipGuard {
    /// 打开剪贴板：被其他进程占用时每 10 ms 重试一次，直到 `budget` 用完（至少尝试一次）。重试间的等待是一次性的
    /// 有界等待，只在剪贴板确实被占用时发生
    fn open(owner: Option<RawHwnd>, budget: Duration) -> Result<Self, PlatformError> {
        let owner = owner.filter(|h| !h.is_null()).map(RawHwnd::hwnd);
        let deadline = Instant::now() + budget;
        loop {
            // SAFETY: owner 是本进程的消息窗口或 None
            if unsafe { OpenClipboard(owner) }.is_ok() {
                return Ok(Self);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(PlatformError::Busy("clipboard is held by another process".into()));
            }
            std::thread::sleep(OPEN_RETRY_EVERY.min(deadline - now));
        }
    }
}

impl Drop for ClipGuard {
    fn drop(&mut self) {
        // SAFETY: 与成功的 OpenClipboard 配对，在同一线程
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

/// 只读打开剪贴板的守卫。Wisp 开着剪贴板期间其他程序无法清空或写入，序号只可能因原所有者响应 `WM_RENDERFORMAT`
/// （`GetClipboardData` 触发的延迟渲染，Excel / Office / 浏览器都常用）而增加，内容并没有变：打开后立即向剪贴板
/// 线程登记开放的同内容别名，关闭之前收窄到当前序号，监听方不会把 Wisp 自己的读取当成一次新的复制。
/// 无论系统在渲染当时还是关闭剪贴板时发出变更通知，别名都已在场
struct ReadGuard<'a> {
    _clip: ClipGuard,
    /// 打开时的序号
    opened_at: u64,
    /// 剪贴板线程与别名 token
    alias: Option<(&'a ClipHub, u64)>,
}

impl<'a> ReadGuard<'a> {
    fn open(budget: Duration, hub: Option<&'a ClipHub>) -> Result<Self, PlatformError> {
        let clip = ClipGuard::open(None, budget)?;
        let opened_at = seq();
        Ok(Self {
            _clip: clip,
            opened_at,
            alias: hub.map(|h| (h, h.read_began(opened_at))),
        })
    }
}

impl Drop for ReadGuard<'_> {
    fn drop(&mut self) {
        // 先于字段（`_clip`，即 CloseClipboard）执行：别名在关闭剪贴板之前收窄
        if let Some((hub, token)) = self.alias {
            hub.read_ended(token, self.opened_at, seq());
        }
    }
}

/// 借用某格式的 HGLOBAL 内容（剪贴板必须已由当前线程打开）
fn with_global<R>(format: u32, f: impl FnOnce(&[u8]) -> R) -> Option<R> {
    if format == 0 {
        return None;
    }
    // SAFETY: 剪贴板已打开；返回的句柄归系统所有，只读访问
    let h = unsafe { GetClipboardData(format) }.ok()?;
    let hg = HGLOBAL(h.0);
    // SAFETY: 句柄来自剪贴板；不是 HGLOBAL 时 GlobalSize 返回 0
    let size = unsafe { GlobalSize(hg) };
    if size == 0 {
        return None;
    }
    // SAFETY: 同上；锁定期间内存有效
    let p = unsafe { GlobalLock(hg) };
    if p.is_null() {
        return None;
    }
    // SAFETY: GlobalLock 返回的指针至少有 GlobalSize 字节可读
    let slice = unsafe { std::slice::from_raw_parts(p as *const u8, size) };
    let r = f(slice);
    // SAFETY: 与上面的 GlobalLock 配对
    unsafe {
        let _ = GlobalUnlock(hg);
    }
    Some(r)
}

fn available(format: u32) -> bool {
    // SAFETY: 纯查询
    format != 0 && unsafe { IsClipboardFormatAvailable(format) }.is_ok()
}

/// 当前内容是否带隐私标记：ExcludeClipboardContentFromMonitorProcessing / Clipboard Viewer Ignore
/// 存在，或 CanIncludeInClipboardHistory / CanUploadToCloudClipboard 的 DWORD 值为 0（读不到值时按隐私处理）
fn is_private(f: &Formats) -> bool {
    f.privacy.iter().any(|&(id, name)| {
        available(id) && {
            let dword = with_global(id, |b| {
                (b.len() >= 4).then(|| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            })
            .flatten();
            windows_format_is_private(name, dword)
        }
    })
}

fn read_marker(f: &Formats) -> Option<Marker> {
    if !available(f.marker) {
        return None;
    }
    Some(with_global(f.marker, fmt::decode_marker).unwrap_or(Marker {
        kind: MarkerKind::Write,
        private: false,
    }))
}

/// `read` 的结果：剪贴板内容 + 所有者窗口（来源应用在关闭剪贴板之后再解析，尽量缩短占用时间）
pub(crate) struct ReadOutcome {
    pub raw: RawClipboard,
    pub owner: Option<RawHwnd>,
}

/// 隐私判定。Wisp 写入的内容：其中的排除历史 / 排除监视格式是 Wisp 自己加的，不代表隐私；Wisp 恢复的
/// 原内容：只认标记里保存的、快照时的判定（恢复时 Wisp 同样会加排除历史 / 排除监视格式，不能再按格式重新判断）
fn content_is_private(f: &Formats, marker: Option<Marker>) -> bool {
    match marker {
        Some(Marker {
            kind: MarkerKind::Write,
            ..
        }) => false,
        Some(Marker {
            kind: MarkerKind::Restore,
            private,
        }) => private,
        None => is_private(f),
    }
}

/// 读取剪贴板。剪贴板打开期间只拷贝数据；图片的解码与 PNG 编码在关闭之后进行。
/// `change_id` 是打开时的序号（与监听回调携带的序号一致；读取引起的延迟渲染登记到 `hub`，见 [`ReadGuard`]）
pub(crate) fn read(limits: ReadLimits, hub: Option<&ClipHub>) -> Result<ReadOutcome, PlatformError> {
    let f = formats();
    let mut raw = RawClipboard::default();
    let (owner, image) = {
        let g = ReadGuard::open(OPEN_BUDGET, hub)?;
        raw.change_id = g.opened_at;
        // SAFETY: 纯查询；没有所有者时返回错误
        let owner = unsafe { GetClipboardOwner() }.ok().map(RawHwnd::new);

        // 1. 标记与隐私格式：命中则不读正文
        let marker = read_marker(f);
        raw.from_wisp = marker.is_some();
        if content_is_private(f, marker) {
            raw.concealed = true;
            return Ok(ReadOutcome { raw, owner });
        }

        // 2. 正文：文本 / HTML / RTF / 文件列表都是一次线性扫描，在锁内完成；图片只拷贝原始字节
        if available(CF_UNICODETEXT) {
            let max_units = limits.max_text_bytes.saturating_add(1);
            if let Some((text, capped)) = with_global(CF_UNICODETEXT, |b| fmt::utf16le_to_string(b, max_units)) {
                let mut text = fmt::to_lf(&text).into_owned();
                let cut = fmt::truncate_utf8(&mut text, limits.max_text_bytes);
                raw.text_truncated = cut || capped;
                raw.text = Some(text);
            }
        }
        if available(f.html) {
            raw.html = with_global(f.html, |b| fmt::parse_cf_html(b, limits.max_rich_bytes)).flatten();
        }
        if available(f.rtf) {
            raw.rtf = with_global(f.rtf, |b| {
                let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
                (n > 0 && n <= limits.max_rich_bytes).then(|| b[..n].to_vec())
            })
            .flatten();
        }
        if available(CF_HDROP) {
            raw.files = read_files();
        }
        (owner, copy_image(f, limits.max_image_bytes))
    };

    // 3. 剪贴板已关闭：解码 DIB、编码 PNG
    if let Some((png, (w, h))) = image.and_then(|i| i.into_png(limits.max_image_bytes)) {
        raw.image_png = Some(png);
        raw.image_size = Some(PhysSize { width: w, height: h });
    }
    Ok(ReadOutcome { raw, owner })
}

/// 在剪贴板打开期间拷出的图片原始字节
enum ImageBytes {
    /// `PNG` 格式（已确认不超过上限）与尺寸
    Png(Vec<u8>, (u32, u32)),
    /// `CF_DIBV5` / `CF_DIB`（已按头部尺寸粗筛）
    Dib(Vec<u8>),
}

/// 图片（剪贴板必须已打开）：`PNG` 格式直接拷贝；否则取枚举顺序中先出现的 `CF_DIBV5` / `CF_DIB`（原始格式先于
/// 系统合成的格式），按头部尺寸粗筛后拷贝原始字节。超过上限的图片不拷贝
fn copy_image(f: &Formats, max_bytes: usize) -> Option<ImageBytes> {
    if available(f.png) {
        let mut too_large = false;
        let png = with_global(f.png, |b| {
            let dims = fmt::png_dimensions(b)?;
            // GlobalSize 可能大于实际数据；PNG 自带 IEND，多余的尾部字节无害但没必要保留
            let end = png_end(b).unwrap_or(b.len());
            too_large = end > max_bytes;
            (!too_large).then(|| (b[..end].to_vec(), dims))
        })
        .flatten();
        if let Some((png, dims)) = png {
            return Some(ImageBytes::Png(png, dims));
        }
        if too_large {
            // 原图的 PNG 已经超过上限：同一张图换成 DIB 再编码只会更大，直接放弃
            return None;
        }
    }
    let dib_format = first_present(&[CF_DIBV5, CF_DIB])?;
    with_global(dib_format, |b| {
        fmt::dib_worth_decoding(b, max_bytes).then(|| b.to_vec())
    })
    .flatten()
    .map(ImageBytes::Dib)
}

impl ImageBytes {
    /// 剪贴板关闭之后：DIB 解码并编码为 PNG；PNG 超过 `max_bytes` 时不记录
    fn into_png(self, max_bytes: usize) -> Option<(Vec<u8>, (u32, u32))> {
        let dib = match self {
            Self::Png(png, dims) => return Some((png, dims)),
            Self::Dib(dib) => dib,
        };
        let decoded = fmt::decode_dib(&dib)?;
        // 编码前先释放原始字节，压低峰值内存
        drop(dib);
        let png = match decoded {
            DibImage::Png(png) => png,
            DibImage::Pixels(img) => fmt::encode_png(&img)?,
        };
        let dims = fmt::png_dimensions(&png)?;
        (png.len() <= max_bytes).then_some((png, dims))
    }
}

/// PNG 数据的实际结尾（IEND 块之后）
fn png_end(b: &[u8]) -> Option<usize> {
    let pos = b.windows(4).rposition(|w| w == b"IEND")?;
    // IEND 块：长度(4) + 类型(4) + CRC(4)；pos 指向类型
    let end = pos + 8;
    (end <= b.len()).then_some(end)
}

/// 按枚举顺序返回候选格式中最先出现的一个
fn first_present(candidates: &[u32]) -> Option<u32> {
    let mut format = 0u32;
    loop {
        // SAFETY: 剪贴板已打开；返回 0 表示枚举结束
        format = unsafe { EnumClipboardFormats(format) };
        if format == 0 {
            return None;
        }
        if candidates.contains(&format) {
            return Some(format);
        }
    }
}

fn read_files() -> Option<Vec<String>> {
    if let Some(Some(files)) = with_global(CF_HDROP, fmt::parse_dropfiles_wide) {
        return (!files.is_empty()).then_some(files);
    }
    // ANSI 版本：交给系统转换
    // SAFETY: 剪贴板已打开
    let h = unsafe { GetClipboardData(CF_HDROP) }.ok()?;
    let hdrop = HDROP(h.0);
    // SAFETY: 0xFFFFFFFF 查询文件数
    let count = unsafe { DragQueryFileW(hdrop, u32::MAX, None) };
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count.min(10_000) {
        // SAFETY: 先查长度（不含 NUL），再按长度 + 1 取路径
        let len = unsafe { DragQueryFileW(hdrop, i, None) } as usize;
        let mut buf = vec![0u16; len + 1];
        // SAFETY: 缓冲区长度由切片给出
        let n = unsafe { DragQueryFileW(hdrop, i, Some(&mut buf)) } as usize;
        out.push(String::from_utf16_lossy(&buf[..n.min(len)]));
    }
    (!out.is_empty()).then_some(out)
}

/// 只读纯文本（模拟复制回退的第 ⑥ 步）。目标应用刚复制完时往往还开着剪贴板：在 `open_budget` 内反复重试打开。
/// 返回 None 表示没有文本格式
pub(crate) fn read_text(
    max_bytes: usize,
    open_budget: Duration,
    hub: Option<&ClipHub>,
) -> Result<Option<String>, PlatformError> {
    let _g = ReadGuard::open(open_budget, hub)?;
    if !available(CF_UNICODETEXT) {
        return Ok(None);
    }
    Ok(with_global(CF_UNICODETEXT, |b| {
        fmt::utf16le_to_string(b, max_bytes.saturating_add(1)).0
    }))
}

// ---------------------------------------------------------------- 快照 / 恢复

enum SnapData {
    Global(Vec<u8>),
    EnhMetafile(Vec<u8>),
}

struct SnapItem {
    format: u32,
    data: SnapData,
}

/// 平台私有的快照结构（装进 `ClipSnapshot`）
pub(crate) struct WinSnapshot {
    items: Vec<SnapItem>,
    /// 原内容是否带隐私标记（恢复后写进 Wisp 标记，读取时仍按隐私处理）
    private: bool,
}

/// 快照结果
pub(crate) enum SnapshotOutcome {
    Taken(WinSnapshot, usize),
    /// 超过 `max_bytes` 或读取超过 `deadline`：放弃（只记日志）
    Abandoned(&'static str),
}

fn format_name(format: u32) -> Option<String> {
    if format < 0xC000 {
        return None;
    }
    let mut buf = [0u16; 128];
    // SAFETY: 缓冲区长度由切片给出
    let n = unsafe { GetClipboardFormatNameW(format, &mut buf) };
    (n > 0).then(|| String::from_utf16_lossy(&buf[..n as usize]))
}

/// 深拷贝剪贴板的全部类型。打开剪贴板也计入 `deadline`。逐格式 `GetClipboardData` 会强制原所有者
/// 渲染延迟格式，由此造成的序号变化登记到 `hub`（见 [`ReadGuard`]）
pub(crate) fn snapshot(
    max_bytes: usize,
    deadline: Instant,
    hub: Option<&ClipHub>,
) -> Result<SnapshotOutcome, PlatformError> {
    let f = formats();
    let _g = ReadGuard::open(deadline.saturating_duration_since(Instant::now()), hub)?;
    let private = content_is_private(f, read_marker(f));
    let mut items = Vec::new();
    let mut total = 0usize;
    // 系统会在文本三兄弟、DIB 两兄弟之间互相合成：只保存信息最全的一个，其余恢复后由系统重新合成
    let has_unicode = available(CF_UNICODETEXT);
    let has_dibv5 = available(CF_DIBV5);
    let mut seen_ansi_text = false;
    let mut format = 0u32;
    loop {
        // SAFETY: 剪贴板已打开
        format = unsafe { EnumClipboardFormats(format) };
        if format == 0 {
            break;
        }
        if Instant::now() > deadline {
            return Ok(SnapshotOutcome::Abandoned("deadline"));
        }
        match format {
            // GDI 句柄类与无法按字节复制的格式。CF_LOCALE 照常保存（4 字节 LCID）：恢复后系统按它把 Unicode 文本
            // 换算成 CF_TEXT，不保存的话会按恢复线程当前的输入语言重新生成，ANSI 程序读到的文本可能是乱码
            CF_BITMAP | CF_PALETTE | CF_METAFILEPICT | CF_OWNERDISPLAY | CF_DSPBITMAP | CF_DSPMETAFILEPICT
            | CF_DSPENHMETAFILE => continue,
            f if CF_PRIVATE_RANGE.contains(&f) || CF_GDIOBJ_RANGE.contains(&f) => continue,
            CF_TEXT | CF_OEMTEXT => {
                if has_unicode || seen_ansi_text {
                    continue;
                }
                seen_ansi_text = true;
            }
            CF_DIB if has_dibv5 => continue,
            _ => {}
        }
        if format == f.marker {
            continue;
        }
        if let Some(name) = format_name(format)
            && SNAPSHOT_SKIP_NAMES.contains(&name.as_str())
        {
            continue;
        }
        let data = if format == CF_ENHMETAFILE {
            // SAFETY: 剪贴板已打开；CF_ENHMETAFILE 的句柄是 HENHMETAFILE
            let Ok(h) = (unsafe { GetClipboardData(format) }) else {
                continue;
            };
            let hemf = HENHMETAFILE(h.0);
            // SAFETY: 先查大小，再按大小取字节
            let size = unsafe { GetEnhMetaFileBits(hemf, None) } as usize;
            if size == 0 {
                continue;
            }
            if total + size > max_bytes {
                return Ok(SnapshotOutcome::Abandoned("too large"));
            }
            let mut buf = vec![0u8; size];
            // SAFETY: 缓冲区大小与查询结果一致
            let n = unsafe { GetEnhMetaFileBits(hemf, Some(&mut buf)) } as usize;
            buf.truncate(n);
            SnapData::EnhMetafile(buf)
        } else {
            let mut too_large = false;
            let bytes = with_global(format, |b| {
                if total + b.len() > max_bytes {
                    too_large = true;
                    None
                } else {
                    Some(b.to_vec())
                }
            })
            .flatten();
            if too_large {
                return Ok(SnapshotOutcome::Abandoned("too large"));
            }
            let Some(bytes) = bytes else { continue };
            SnapData::Global(bytes)
        };
        total += match &data {
            SnapData::Global(b) | SnapData::EnhMetafile(b) => b.len(),
        };
        items.push(SnapItem { format, data });
    }
    Ok(SnapshotOutcome::Taken(WinSnapshot { items, private }, total))
}

fn unregistered() -> PlatformError {
    PlatformError::Other("clipboard format is not registered".into())
}

/// 预先分配并写好内容的可移动全局内存（`HGLOBAL`）。写入时在打开剪贴板**之前**准备好，打开期间只做
/// `SetClipboardData`，不在占用剪贴板时拷贝大块数据，也不会出现「Vec + HGLOBAL」两份副本。交给剪贴板后归系统所有；
/// 没交出去（写入失败、超时后被丢弃）时由 Drop 释放
struct OwnedGlobal(HGLOBAL);

// SAFETY: 可移动全局内存的句柄不绑定线程：分配、锁定、交给剪贴板可以在不同线程上进行；本类型独占该句柄
unsafe impl Send for OwnedGlobal {}

impl OwnedGlobal {
    /// 分配 `len` 字节（已清零）并交给 `write` 填写
    fn fill(len: usize, write: impl FnOnce(&mut [u8]) -> Result<(), PlatformError>) -> Result<Self, PlatformError> {
        // SAFETY: GHND = GMEM_MOVEABLE | GMEM_ZEROINIT：内容已清零，下面可以安全地借成 &mut [u8]
        let h = unsafe { GlobalAlloc(GHND, len.max(1)) }
            .map_err(|e| PlatformError::Other(format!("GlobalAlloc({len}): {e}")))?;
        let me = Self(h);
        // SAFETY: 句柄刚分配，由本守卫独占
        let p = unsafe { GlobalLock(h) };
        if p.is_null() {
            return Err(PlatformError::Other("GlobalLock failed".into()));
        }
        // SAFETY: 锁定期间指针至少有 len 字节可写，且没有其他引用
        let r = write(unsafe { std::slice::from_raw_parts_mut(p.cast::<u8>(), len) });
        // SAFETY: 与上面的 GlobalLock 配对
        unsafe {
            let _ = GlobalUnlock(h);
        }
        r.map(|()| me)
    }

    fn from_bytes(bytes: &[u8]) -> Result<Self, PlatformError> {
        Self::fill(bytes.len(), |b| {
            b.copy_from_slice(bytes);
            Ok(())
        })
    }

    /// 交给剪贴板（剪贴板必须已由当前线程打开）：成功后所有权归系统，失败时由 Drop 释放
    fn into_clipboard(self, format: u32) -> Result<(), PlatformError> {
        if format == 0 {
            return Err(unregistered());
        }
        // SAFETY: 剪贴板已打开；句柄是可移动全局内存
        unsafe { SetClipboardData(format, Some(HANDLE(self.0.0))) }
            .map_err(|e| PlatformError::Other(format!("SetClipboardData({format}): {e}")))?;
        std::mem::forget(self);
        Ok(())
    }
}

impl Drop for OwnedGlobal {
    fn drop(&mut self) {
        // SAFETY: 句柄由本守卫独占，且没有交给剪贴板
        unsafe {
            let _ = GlobalFree(Some(self.0));
        }
    }
}

/// 放一份数据到剪贴板（剪贴板必须已打开且已清空）
fn set_bytes(format: u32, bytes: &[u8]) -> Result<(), PlatformError> {
    if format == 0 {
        return Err(unregistered());
    }
    OwnedGlobal::from_bytes(bytes)?.into_clipboard(format)
}

fn set_dword(format: u32, v: u32) -> Result<(), PlatformError> {
    set_bytes(format, &v.to_le_bytes())
}

fn empty() -> Result<(), PlatformError> {
    // SAFETY: 剪贴板已由当前线程打开
    unsafe { EmptyClipboard() }.map_err(|e| PlatformError::Other(format!("EmptyClipboard: {e}")))
}

/// 恢复快照（原剪贴板为空时就是清空），返回恢复后的序号。
///
/// 恢复的内容另附 Restore 标记（保存快照时的隐私判定）与「不进系统剪贴板历史 / 不上传云剪贴板 / 剪贴板监视器忽略」
/// 三个格式：原内容在用户当初复制时已经被记录过，恢复只是把它放回去，不应在 Win + V 历史、云剪贴板或第三方剪贴板
/// 管理器里再多出一条重复项。这些格式由 Wisp 自己添加，读取时按 Restore 标记里保存的判定处理，不会被当成隐私标记
pub(crate) fn restore(snap: WinSnapshot, owner: Option<RawHwnd>, open_budget: Duration) -> Result<u64, PlatformError> {
    let f = formats();
    {
        let _g = ClipGuard::open(owner, open_budget)?;
        empty()?;
        if !snap.items.is_empty() {
            // 先写 Restore 标记与排除格式、再写原内容（理由同 `write`）。标记写失败时不加排除格式：否则读取方会把
            // 这些格式当成隐私标记、隐藏正文
            let marker = fmt::encode_marker(Marker {
                kind: MarkerKind::Restore,
                private: snap.private,
            });
            if set_bytes(f.marker, &marker).is_ok() {
                for format in [f.can_history, f.can_cloud, f.exclude_monitor] {
                    if let Err(e) = set_dword(format, 0) {
                        log::debug!("restore: failed to add exclusion format {format}: {e}");
                    }
                }
            }
            for item in &snap.items {
                let r = match &item.data {
                    SnapData::Global(b) => set_bytes(item.format, b),
                    SnapData::EnhMetafile(b) => {
                        // SAFETY: 由快照字节重建 EMF；交给剪贴板后归系统所有
                        let hemf = unsafe { SetEnhMetaFileBits(b) };
                        if hemf.0.is_null() {
                            Err(PlatformError::Other("SetEnhMetaFileBits failed".into()))
                        } else {
                            // SAFETY: 剪贴板已打开；交给剪贴板失败时句柄仍归本进程，删除以免泄漏
                            unsafe { SetClipboardData(CF_ENHMETAFILE, Some(HANDLE(hemf.0))) }
                                .map(|_| ())
                                .map_err(|e| {
                                    // SAFETY: 见上
                                    let _ = unsafe { DeleteEnhMetaFile(Some(hemf)) };
                                    PlatformError::Other(format!("SetClipboardData(EMF): {e}"))
                                })
                        }
                    }
                };
                if let Err(e) = r {
                    log::debug!("restore: skipped format {}: {e}", item.format);
                }
            }
        }
    }
    Ok(seq())
}

// ---------------------------------------------------------------- 写入

/// 写入前在剪贴板外准备好的内容：每个正文格式都已写进各自的全局内存（解码 PNG 等耗时工作不占用剪贴板）
pub(crate) struct Prepared {
    /// 正文格式，按写入顺序（富格式在前：部分应用按枚举顺序挑选格式）
    body: Vec<(u32, OwnedGlobal)>,
    /// 有文件列表：另附 `Preferred DropEffect = DROPEFFECT_COPY`
    drop_effect: bool,
    opts: WriteOpts,
}

/// 编码载荷，直接写进各格式的全局内存。图片：PNG 原样拷贝一次（不再另存一份）；DIBV5 由 PNG 解码后原地转换，直接
/// 写在它自己的全局内存里（不保留中间的 RGBA），峰值内存约为一份 DIB + 一份 PNG。图片超过与读取一侧对称的上限
/// （[`fmt::dib_within_budget`]，按默认 `ReadLimits` 约 20 MP，5K 屏截图以内）时只写 PNG：Office、浏览器、画图等
/// 主流应用都直接读 PNG，常驻进程不为一份未压缩副本承受上百 MB 的内存尖峰
pub(crate) fn prepare(payload: &ClipPayload, opts: WriteOpts) -> Result<Prepared, PlatformError> {
    let f = formats();
    let mut body = Vec::with_capacity(6);
    if let Some(rtf) = &payload.rtf {
        // RTF 以 NUL 结尾：分配时已清零，多出的最后一个字节就是 NUL
        let g = OwnedGlobal::fill(rtf.len() + 1, |b| {
            b[..rtf.len()].copy_from_slice(rtf);
            Ok(())
        })?;
        body.push((f.rtf, g));
    }
    if let Some(html) = payload.html.as_deref() {
        body.push((f.html, OwnedGlobal::from_bytes(&fmt::build_cf_html(html))?));
    }
    if let Some(text) = payload.text.as_deref() {
        body.push((
            CF_UNICODETEXT,
            OwnedGlobal::from_bytes(&fmt::utf16le_z(&fmt::to_crlf(text)))?,
        ));
    }
    if let Some(png) = payload.image_png.as_deref() {
        let not_png = || PlatformError::Other("image_png is not a decodable PNG".into());
        let (w, h) = fmt::png_dimensions(png).ok_or_else(not_png)?;
        body.push((f.png, OwnedGlobal::from_bytes(png)?));
        if fmt::dib_within_budget(w, h, ReadLimits::default().max_image_bytes) {
            let len = fmt::dibv5_len(w, h).ok_or_else(not_png)?;
            let dib = OwnedGlobal::fill(len, |b| fmt::png_into_dibv5(png, b).ok_or_else(not_png))?;
            body.push((CF_DIBV5, dib));
        } else {
            log::info!("clipboard write: {w}x{h} image is over the DIB budget, writing PNG only");
        }
    }
    let files = payload.files.as_ref().filter(|f| !f.is_empty());
    if let Some(files) = files {
        body.push((CF_HDROP, OwnedGlobal::from_bytes(&fmt::build_dropfiles(files))?));
    }
    Ok(Prepared {
        body,
        drop_effect: files.is_some(),
        opts,
    })
}

/// 写入剪贴板，返回写入后的序号。任一格式写失败时清空剪贴板再关闭：EmptyClipboard 已经清掉了原内容，宁可留空，
/// 也不留下缺格式的残缺内容
pub(crate) fn write(p: Prepared, owner: Option<RawHwnd>) -> Result<u64, PlatformError> {
    let f = formats();
    {
        let _g = ClipGuard::open(owner, OPEN_BUDGET)?;
        empty()?;
        if let Err(e) = put_prepared(f, p) {
            if let Err(e2) = empty() {
                log::warn!("clipboard write failed ({e}) and could not be rolled back: {e2}");
            }
            return Err(e);
        }
    }
    Ok(seq())
}

/// 依次放入：Wisp 标记 →（按需）排除格式 → 正文。标记与排除格式在最前：即使正文写到一半失败、回滚的清空也失败，
/// 剪贴板里的内容也带着标记与排除格式——监听方不会把它当成外部复制（`from_wisp = true`），Win + V 历史与云剪贴板
/// 也不会收录（`WriteOpts`）
fn put_prepared(f: &Formats, p: Prepared) -> Result<(), PlatformError> {
    set_bytes(
        f.marker,
        &fmt::encode_marker(Marker {
            kind: MarkerKind::Write,
            private: false,
        }),
    )?;
    if p.opts.transient {
        set_dword(f.exclude_monitor, 0)?;
    }
    if p.opts.exclude_from_history {
        set_dword(f.can_history, 0)?;
        set_dword(f.can_cloud, 0)?;
    }
    // 中途失败时，剩下没交出去的全局内存随迭代器一起释放
    for (format, g) in p.body {
        g.into_clipboard(format)?;
    }
    if p.drop_effect {
        set_dword(f.drop_effect, DROPEFFECT_COPY)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! 需要真实桌面会话的剪贴板冒烟测试（会改动当前剪贴板内容），默认忽略：
    //! `cargo test -p wisp-platform -- --ignored --test-threads=1`

    use std::cell::RefCell;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};

    use ::windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use ::windows::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, DestroyWindow, HWND_MESSAGE, PostMessageW, PostQuitMessage, WINDOW_EX_STYLE, WINDOW_STYLE,
        WM_APP, WM_DESTROY, WM_RENDERFORMAT,
    };

    use super::super::clip_format::Rgba;
    use super::super::clip_watch::ClipHub;
    use super::super::selection;
    use super::super::wnd;
    use super::*;

    /// 与生产路径一致：写入时用剪贴板线程的消息窗口做所有者
    fn owner() -> Option<RawHwnd> {
        static HUB: OnceLock<std::sync::Arc<ClipHub>> = OnceLock::new();
        HUB.get_or_init(ClipHub::new).owner()
    }

    /// 把剪贴板设为文本 + 指定的隐私格式
    fn write_with_format(text: &str, format_name: &str, dword: Option<u32>) -> Result<(), PlatformError> {
        let _g = ClipGuard::open(owner(), OPEN_BUDGET)?;
        empty()?;
        set_bytes(CF_UNICODETEXT, &fmt::utf16le_z(text))?;
        let id = register(format_name);
        match dword {
            Some(v) => set_dword(id, v),
            None => set_bytes(id, &[0]),
        }
    }

    fn read_default() -> RawClipboard {
        read(ReadLimits::default(), None).expect("read clipboard").raw
    }

    fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        f()
    }

    // ------------------------------------------------ 延迟渲染的所有者

    const LAZY_CLASS: &str = "WispTestLazyOwner";
    const WM_LAZY_PUBLISH: u32 = WM_APP + 0x40;
    const WM_LAZY_QUIT: u32 = WM_APP + 0x41;
    /// 除文本外再宣告一个只有快照会去读的私有格式（快照逐格式读取，会再触发一次渲染）
    const LAZY_EXTRA_FORMAT: &str = "WispTestLazyExtra";

    /// 所有者线程上的状态
    struct LazyState {
        text: String,
        renders: Arc<AtomicUsize>,
        requests: mpsc::Receiver<(String, mpsc::SyncSender<u64>)>,
    }

    thread_local! {
        static LAZY: RefCell<Option<LazyState>> = const { RefCell::new(None) };
    }

    /// 延迟渲染的剪贴板所有者（模拟 Excel / Office / 浏览器）：发布时只用 `SetClipboardData(fmt, NULL)` 宣告格式，
    /// 有人 `GetClipboardData` 时才在 `WM_RENDERFORMAT` 里真正放入数据
    struct LazyOwner {
        hwnd: RawHwnd,
        renders: Arc<AtomicUsize>,
        requests: mpsc::Sender<(String, mpsc::SyncSender<u64>)>,
    }

    impl LazyOwner {
        fn start() -> Self {
            let renders = Arc::new(AtomicUsize::new(0));
            let (req_tx, req_rx) = mpsc::channel();
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let counter = Arc::clone(&renders);
            std::thread::Builder::new()
                .name("wisp-test-lazy-owner".into())
                .spawn(move || {
                    assert!(wnd::ensure_class(LAZY_CLASS, Some(lazy_proc), None));
                    let hwnd = wnd::create_window(LAZY_CLASS, WINDOW_EX_STYLE(0), WINDOW_STYLE(0), Some(HWND_MESSAGE))
                        .expect("lazy owner window");
                    LAZY.with(|l| {
                        *l.borrow_mut() = Some(LazyState {
                            text: String::new(),
                            renders: counter,
                            requests: req_rx,
                        })
                    });
                    let _ = ready_tx.send(RawHwnd::new(hwnd));
                    wnd::run_message_loop(|_| false);
                })
                .expect("spawn lazy owner");
            let hwnd = ready_rx.recv_timeout(Duration::from_secs(2)).expect("lazy owner ready");
            Self {
                hwnd,
                renders,
                requests: req_tx,
            }
        }

        /// 以延迟渲染的方式发布 `text`，返回发布后的序号
        fn publish(&self, text: &str) -> u64 {
            let (ack_tx, ack_rx) = mpsc::sync_channel(1);
            self.requests
                .send((text.to_owned(), ack_tx))
                .expect("send publish request");
            // SAFETY: 向测试线程的窗口投递消息
            unsafe { PostMessageW(Some(self.hwnd.hwnd()), WM_LAZY_PUBLISH, WPARAM(0), LPARAM(0)) }.expect("post");
            let s = ack_rx.recv_timeout(Duration::from_secs(2)).expect("publish ack");
            assert_ne!(s, 0, "publishing delayed formats failed");
            s
        }

        fn renders(&self) -> usize {
            self.renders.load(Ordering::SeqCst)
        }
    }

    impl Drop for LazyOwner {
        fn drop(&mut self) {
            // 先换成普通内容：所有者销毁时不必再渲染全部格式
            if let Ok(p) = prepare(&ClipPayload::text("done"), WriteOpts::default()) {
                let _ = write(p, owner());
            }
            // SAFETY: 向测试线程的窗口投递消息；窗口销毁后线程的消息循环结束
            unsafe {
                let _ = PostMessageW(Some(self.hwnd.hwnd()), WM_LAZY_QUIT, WPARAM(0), LPARAM(0));
            }
        }
    }

    /// 以 `hwnd` 为所有者清空剪贴板并宣告延迟渲染的格式
    fn publish_lazy(hwnd: HWND) -> bool {
        let Ok(_g) = ClipGuard::open(Some(RawHwnd::new(hwnd)), OPEN_BUDGET) else {
            return false;
        };
        if empty().is_err() {
            return false;
        }
        // SAFETY: 剪贴板由本线程以 hwnd 为所有者打开；NULL 句柄表示延迟渲染（此时返回值本来就是 NULL，不看它）
        unsafe {
            let _ = SetClipboardData(CF_UNICODETEXT, None);
            let _ = SetClipboardData(register(LAZY_EXTRA_FORMAT), None);
        }
        available(CF_UNICODETEXT)
    }

    unsafe extern "system" fn lazy_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        match msg {
            WM_LAZY_PUBLISH => {
                LAZY.with(|l| {
                    if let Some(st) = l.borrow_mut().as_mut() {
                        while let Ok((text, ack)) = st.requests.try_recv() {
                            st.text = text;
                            let s = if publish_lazy(hwnd) { seq() } else { 0 };
                            let _ = ack.send(s);
                        }
                    }
                });
                LRESULT(0)
            }
            WM_RENDERFORMAT => {
                // 读取方开着剪贴板、调用了 GetClipboardData：所有者此时直接 SetClipboardData（不打开剪贴板）
                let format = wparam.0 as u32;
                LAZY.with(|l| {
                    if let Some(st) = l.borrow().as_ref() {
                        st.renders.fetch_add(1, Ordering::SeqCst);
                        let data = if format == CF_UNICODETEXT {
                            fmt::utf16le_z(&st.text)
                        } else {
                            b"lazy".to_vec()
                        };
                        let _ = set_bytes(format, &data);
                    }
                });
                LRESULT(0)
            }
            WM_LAZY_QUIT => {
                // SAFETY: 本线程创建的窗口
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                // SAFETY: 结束本线程的消息循环
                unsafe { PostQuitMessage(0) };
                LRESULT(0)
            }
            // SAFETY: 其余消息交给默认处理
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    /// 复制回退：快照会强制原所有者渲染延迟格式。无论这会不会推进序号，第 ⑤ 步都以备份**之后**的序号
    /// 为基线，不会在目标应用响应复制键之前就认为「已复制」、把原剪贴板里的文本当成选区
    #[test]
    #[ignore = "需要真实桌面会话（会改动剪贴板）"]
    #[allow(clippy::print_stderr)] // 真机手工用例：打印观察到的系统行为（延迟渲染是否推进序号），供记录
    fn copy_fallback_baseline_survives_delayed_rendering() {
        let lazy = LazyOwner::start();
        let hub = ClipHub::new();
        let s0 = lazy.publish("original clipboard text");
        let (backup, s_base) = selection::take_backup(&hub).expect("snapshot thread closed the clipboard");
        assert!(
            matches!(backup, Some(Ok(SnapshotOutcome::Taken(..)))),
            "backup should succeed"
        );
        assert!(lazy.renders() >= 1, "the snapshot should force delayed rendering");
        eprintln!("delayed rendering during the backup moved the sequence number {s0} -> {s_base}");
        // 没有发复制键：剪贴板此后没有任何变化
        assert!(!selection::wait_for_copy(s_base, Duration::from_millis(120)));
        // 渲染若推进了序号，旧的基线（备份之前的 s0）会立即误判为「已复制」——这正是要避免的
        if s_base != s0 {
            assert!(selection::wait_for_copy(s0, Duration::from_millis(10)));
        }
    }

    /// 上层收到回调后读取（或快照）剪贴板时，原所有者的延迟渲染不会让监听回调多触发一次；读取恰好落在
    /// 某次外部变更的去抖期间时，那次变更照常投递一次，不会丢
    #[test]
    #[ignore = "需要真实桌面会话（会改动剪贴板）"]
    #[allow(clippy::print_stderr)] // 真机手工用例：打印观察到的系统行为（延迟渲染是否推进序号），供记录
    fn own_reads_of_delayed_formats_do_not_trigger_the_watcher() {
        let lazy = LazyOwner::start();
        let hub = ClipHub::new();
        let seen = Arc::new(parking_lot::Mutex::new(Vec::<u64>::new()));
        let _watch = {
            let seen = Arc::clone(&seen);
            hub.watch(Box::new(move |id| seen.lock().push(id))).expect("watch")
        };
        // 开始监听是异步投递的
        std::thread::sleep(Duration::from_millis(100));
        let count = || seen.lock().len();

        // 1. 外部「复制」：回调一次
        let s1 = lazy.publish("lazy one");
        assert!(wait_until(Duration::from_secs(1), || count() == 1));
        // 2. 上层读取：原所有者渲染延迟格式
        let renders = lazy.renders();
        let raw = read(ReadLimits::default(), Some(&hub)).expect("read").raw;
        assert_eq!(raw.text.as_deref(), Some("lazy one"));
        assert_eq!(raw.change_id, s1, "change_id 是打开时的序号，与回调一致");
        assert!(lazy.renders() > renders, "reading should force delayed rendering");
        eprintln!(
            "delayed rendering during read moved the sequence number {s1} -> {}",
            seq()
        );
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(count(), 1, "Wisp 自己的读取不应再触发一次回调");
        // 3. 快照会读到读取没碰过的私有格式，再渲染一次：同样不触发
        let snap = snapshot(10 * 1024 * 1024, Instant::now() + Duration::from_secs(1), Some(&hub)).expect("snapshot");
        assert!(matches!(snap, SnapshotOutcome::Taken(..)));
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(count(), 1, "Wisp 自己的快照不应再触发一次回调");
        // 4. 去抖期间就读取：这次外部变更照常投递，且只投递一次
        lazy.publish("lazy two");
        let raw = read(ReadLimits::default(), Some(&hub)).expect("read").raw;
        assert_eq!(raw.text.as_deref(), Some("lazy two"));
        assert!(wait_until(Duration::from_secs(1), || count() == 2), "外部变更不能丢");
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(count(), 2);
    }

    #[test]
    #[ignore = "需要真实桌面会话（会改动剪贴板）"]
    fn privacy_formats_conceal_content() {
        for (name, dword) in [
            ("ExcludeClipboardContentFromMonitorProcessing", None),
            ("Clipboard Viewer Ignore", None),
            ("CanIncludeInClipboardHistory", Some(0)),
            ("CanUploadToCloudClipboard", Some(0)),
        ] {
            write_with_format("secret", name, dword).expect("write");
            let raw = read_default();
            assert!(raw.concealed, "{name} should conceal");
            assert_eq!(raw.text, None, "{name}: concealed content must not be read");
        }
        // 值为 1 时不算隐私
        write_with_format("public", "CanIncludeInClipboardHistory", Some(1)).expect("write");
        let raw = read_default();
        assert!(!raw.concealed);
        assert_eq!(raw.text.as_deref(), Some("public"));
    }

    #[test]
    #[ignore = "需要真实桌面会话（会改动剪贴板）"]
    fn write_read_roundtrip_all_types() {
        let png = fmt::encode_png(&Rgba {
            width: 2,
            height: 1,
            pixels: vec![255, 0, 0, 255, 0, 0, 255, 128],
        })
        .expect("png");
        let payload = ClipPayload {
            text: Some("line1\nline2".into()),
            html: Some("<b>bold</b>".into()),
            rtf: Some(b"{\\rtf1 bold}".to_vec()),
            image_png: Some(png),
            files: None,
        };
        let opts = WriteOpts {
            transient: true,
            exclude_from_history: true,
        };
        let before = seq();
        let after = write(prepare(&payload, opts).expect("prepare"), owner()).expect("write");
        assert!(after > before);
        let raw = read_default();
        assert!(raw.from_wisp);
        assert!(!raw.concealed, "Wisp 自己加的排除格式不应被当成隐私内容");
        assert_eq!(raw.text.as_deref(), Some("line1\nline2"));
        assert_eq!(raw.html.as_deref(), Some("<b>bold</b>"));
        assert_eq!(raw.rtf.as_deref(), Some(&b"{\\rtf1 bold}"[..]));
        assert_eq!(raw.image_size, Some(PhysSize { width: 2, height: 1 }));
        {
            // DIBV5 由 PNG 直接写成：只读 DIB 的应用拿到的像素与 alpha 一致
            let _g = ClipGuard::open(None, OPEN_BUDGET).expect("open");
            let dib = with_global(CF_DIBV5, fmt::decode_dib).flatten();
            assert_eq!(
                dib,
                Some(DibImage::Pixels(Rgba {
                    width: 2,
                    height: 1,
                    pixels: vec![255, 0, 0, 255, 0, 0, 255, 128],
                }))
            );
        }

        let files = ClipPayload {
            files: Some(vec![r"C:\Windows\notepad.exe".into()]),
            ..ClipPayload::default()
        };
        write(prepare(&files, WriteOpts::default()).expect("prepare"), owner()).expect("write");
        assert_eq!(read_default().files, Some(vec![r"C:\Windows\notepad.exe".to_owned()]));
    }

    /// 正文写到一半失败：清空剪贴板再关闭，不留下没有标记、没有排除格式的残缺内容
    #[test]
    #[ignore = "需要真实桌面会话（会改动剪贴板）"]
    fn failed_write_leaves_no_partial_content() {
        let opts = WriteOpts {
            transient: false,
            exclude_from_history: true,
        };
        let mut p = prepare(&ClipPayload::text("partial"), opts).expect("prepare");
        // 未注册的格式：SetClipboardData 必然失败
        p.body.push((0, OwnedGlobal::from_bytes(b"x").expect("alloc")));
        assert!(write(p, owner()).is_err());
        let _g = ClipGuard::open(None, OPEN_BUDGET).expect("open");
        // SAFETY: 剪贴板已打开
        assert_eq!(unsafe { EnumClipboardFormats(0) }, 0, "剪贴板应为空");
    }

    #[test]
    #[ignore = "需要真实桌面会话（会改动剪贴板）"]
    fn snapshot_restore_is_faithful() {
        let payload = ClipPayload {
            text: Some("keep me".into()),
            html: Some("<i>keep</i>".into()),
            ..ClipPayload::default()
        };
        write(prepare(&payload, WriteOpts::default()).expect("prepare"), owner()).expect("write");
        let SnapshotOutcome::Taken(snap, bytes) =
            snapshot(10 * 1024 * 1024, Instant::now() + Duration::from_secs(1), None).expect("snapshot")
        else {
            panic!("snapshot abandoned");
        };
        assert!(bytes > 0);
        write(
            prepare(&ClipPayload::text("overwritten"), WriteOpts::default()).expect("prepare"),
            owner(),
        )
        .expect("write");
        restore(snap, owner(), OPEN_BUDGET).expect("restore");
        let raw = read_default();
        assert_eq!(raw.text.as_deref(), Some("keep me"));
        assert_eq!(raw.html.as_deref(), Some("<i>keep</i>"));
        // 恢复时 Wisp 自己加的排除历史 / 排除监视格式不会让内容被当成隐私
        assert!(raw.from_wisp);
        assert!(!raw.concealed);
        // 超过上限时放弃
        assert!(matches!(
            snapshot(4, Instant::now() + Duration::from_secs(1), None).expect("snapshot"),
            SnapshotOutcome::Abandoned(_)
        ));
    }
}
