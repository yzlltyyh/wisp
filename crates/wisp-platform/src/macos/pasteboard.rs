//! NSPasteboard：读取、写入、全类型快照与恢复、changeCount 监听。
//!
//! - 读取前先看类型列表里有没有隐私标记，命中则只报 `concealed`，不读任何正文；
//! - 快照对每个 item 的每个 type 都深拷贝 `dataForType`（延迟提供的数据在被覆盖后就失效），超过上限或超时就放弃；
//! - 凡是会向数据提供方取数据的调用（读取、快照、回退读文本）都在辅助线程上执行并带硬截止时间，被挂起或很慢的
//!   提供方拖不住调用方；超时后辅助线程在后台自行结束；
//! - 写图片时 PNG 立即写入，TIFF 只**声明**、由数据提供方在有应用真的来要时才转码（粘贴路径上不做编码）；
//! - 恢复 = `clearContents` + `writeObjects`，原剪贴板为空时恢复就是清空；恢复产生的序号区间登记为「Wisp 自己的变更」，
//!   监听方不会看到（见 [`ClipState`]）；
//! - 监听用 4 Hz 的 DispatchSource 定时器（leeway 50 ms，utility QoS），每次只读一个整数；Handle drop 时取消，
//!   暂停期间零唤醒，恢复时以当前序号为起点、不补录；显示器睡眠、锁屏、会话切走时由
//!   [`PollGate`] 放慢到 30 s 一次，回来时立即读一次（`poll_gate`）；屏幕亮着而用户安静 30 s / 5 分钟起
//!   放慢到 1 Hz / 0.2 Hz，有输入、有变化或 `poll_now`（剪贴板面板即将显示）时立即回到 4 Hz。

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use dispatch2::{DispatchObject, DispatchQoS, DispatchQueue, DispatchSource, GlobalQueueIdentifier};
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard, NSPasteboardItem, NSPasteboardItemDataProvider,
    NSPasteboardType, NSPasteboardWriting, NSTIFFCompression,
};
use objc2_foundation::{NSArray, NSData, NSDictionary, NSString, NSURL};
use parking_lot::Mutex;
use wisp_types::{AppRef, ClipPayload, PhysSize, RawClipboard};

use super::Ctx;
use super::poll_gate::{Cadence, PollGate};
use super::util::{guarded, pool, with_deadline};
use crate::common::secrets_markers::{WISP_MARKER, has_wisp_marker, macos_is_concealed};
use crate::common::suppress::{DEFAULT_TTL, Suppressor};
use crate::common::text::truncate_utf8_lossy;
use crate::error::PlatformError;
use crate::traits::{ClipboardAccess, WriteOpts};
use crate::types::{ClipSnapshot, HookHandle, ReadLimits, SuppressRange};

pub(crate) const TYPE_STRING: &str = "public.utf8-plain-text";
const TYPE_HTML: &str = "public.html";
const TYPE_RTF: &str = "public.rtf";
const TYPE_PNG: &str = "public.png";
const TYPE_TIFF: &str = "public.tiff";
const TYPE_FILE_URL: &str = "public.file-url";
/// nspasteboard.org：内容只是暂时放在剪贴板上，剪贴板管理器不应记录
const TYPE_TRANSIENT: &str = "org.nspasteboard.TransientType";

/// 监听周期（250 ms = 4 Hz：在记录的及时性与空闲唤醒之间折中）
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// 定时器 leeway（允许系统合并唤醒）
const POLL_LEEWAY: Duration = Duration::from_millis(50);
/// TIFF 转 PNG 前的粗筛倍数：未压缩 TIFF 通常比 PNG 大数倍
const TIFF_PREFILTER_FACTOR: usize = 4;
/// 写入图片时额外声明 TIFF 的像素上限：TIFF 在有应用来要时才在主线程上转码（LZW，约 11–13 ms / 百万像素），
/// 更大的图片只写 PNG，避免一次请求把主线程卡住太久
const TIFF_MAX_PIXELS: u64 = 24_000_000;
/// `ClipboardAccess::read` 从 pboard 取字节的硬截止时间。延迟提供数据的应用（例如 Excel / Numbers
/// 大范围复制时按需生成 HTML 与图片）卡住时，调用方最多等这么久就拿到 Timeout；正常情况下即使 20 MB 的图片也远低于它
const READ_DEADLINE: Duration = Duration::from_millis(500);
/// 回退读文本时等待数据写完的重试间隔
const SETTLE_STEP: Duration = Duration::from_millis(5);

/// 回退进行中的开放区间最多存活多久（安全网：正常路径总会在恢复之后关闭它；调用方异常退出时也不会永久屏蔽监听）。
/// 必须覆盖「发复制键 → 恢复完成」的最坏耗时（见 selection.rs 的单测）
pub(crate) const PENDING_MAX_AGE: Duration = Duration::from_secs(3);
/// 同时保留的「Wisp 自己产生的序号区间」上限（正常情况下只有一两个；监听方越过之后即清理）
const MAX_OWN_RANGES: usize = 16;

/// Wisp 自身造成的剪贴板变更的识别，三种记录并存，任一命中即忽略：
/// - `suppressor`：按 ttl 过期的区间（`ClipboardAccess::suppress`；回退等待期内没有变化时，覆盖慢应用晚到的复制）；
/// - `own`：确定由 Wisp 产生的序号区间 (lo, hi]（恢复写入；回退期间目标应用的那次复制），**不按时间过期**，
///   监听方看到更大的序号后清理。只记有上界的区间、不记单调的「已见上界」：pboard 守护进程重启后 changeCount
///   从小数字重新计数，单调上界会把之后很长一段时间的变更全部吞掉；
/// - `pending`：回退进行中（复制键已发出、尚未恢复）的开放区间 (lo, ∞)，回退结束时转成 `own` 或 ttl 区间。
///   它不按「等待上限 + 余量」计时，因此无论快照、等待、读文本、恢复各花多久，监听方都不会在恢复之前把目标应用的
///   复制或恢复过程中的中间状态报出去
pub(crate) struct ClipState {
    inner: Mutex<IgnoreState>,
    /// 监听定时器的「屏幕空闲」闸门（显示器睡眠 / 锁屏时放慢到 30 s 一次）
    gate: PollGate,
}

impl Default for ClipState {
    fn default() -> Self {
        Self {
            inner: Mutex::default(),
            gate: PollGate::new(Cadence {
                interval: POLL_INTERVAL,
                leeway: POLL_LEEWAY,
            }),
        }
    }
}

/// [`ClipState`] 的纯逻辑部分（时间由调用方传入，便于单测）
#[derive(Default)]
struct IgnoreState {
    suppressor: Suppressor,
    /// (lo, hi]：lo 不含、hi 含
    own: Vec<(u64, u64)>,
    /// (标识, lo, 开始时刻)
    pending: Vec<(u64, u64, Instant)>,
    next_pending: u64,
    /// 监听方最近一次看到的序号（用来发现计数器重置）
    last_seen: Option<u64>,
}

impl IgnoreState {
    fn record_own(&mut self, lo: u64, hi: u64) {
        if hi <= lo {
            return;
        }
        if self.own.len() >= MAX_OWN_RANGES {
            self.own.remove(0);
        }
        self.own.push((lo, hi));
    }

    fn begin_pending(&mut self, lo: u64, now: Instant) -> u64 {
        self.pending
            .retain(|(_, _, at)| now.saturating_duration_since(*at) <= PENDING_MAX_AGE);
        self.next_pending = self.next_pending.wrapping_add(1);
        self.pending.push((self.next_pending, lo, now));
        self.next_pending
    }

    fn take_pending(&mut self, id: u64) -> Option<u64> {
        let i = self.pending.iter().position(|(p, _, _)| *p == id)?;
        Some(self.pending.remove(i).1)
    }

    /// 监听方看到序号 `change` 时调用：先维护记录（计数器重置时清空、清掉已经越过的区间），再判断是否忽略
    fn observe(&mut self, change: u64, now: Instant) -> bool {
        if self.last_seen.is_some_and(|last| change < last) {
            // changeCount 变小：pboard 守护进程重启后重新计数，旧记录里的序号已经失去意义
            log::info!(
                "clipboard change counter went backwards (pasteboard daemon restarted?); resetting suppression state"
            );
            self.own.clear();
            self.suppressor.disarm();
        }
        self.last_seen = Some(change);
        let ignore = self.should_ignore(change, now);
        // 已经越过的区间不会再出现（多个监听方时，看到 hi 本身的那一方之后才清理）
        self.own.retain(|(_, hi)| *hi >= change);
        ignore
    }

    fn should_ignore(&self, change: u64, now: Instant) -> bool {
        self.own.iter().any(|(lo, hi)| change > *lo && change <= *hi)
            || self
                .pending
                .iter()
                .any(|(_, lo, at)| change > *lo && now.saturating_duration_since(*at) <= PENDING_MAX_AGE)
            || self.suppressor.should_ignore(change, now)
    }
}

impl ClipState {
    /// 监听定时器的闸门
    pub(crate) fn gate(&self) -> &PollGate {
        &self.gate
    }

    /// 布防一个按 ttl 过期的区间（`ClipboardAccess::suppress`）。轮询先回到 4 Hz：慢档的周期比
    /// ttl 长时，区间会在监听看到这次写入之前过期
    pub(crate) fn arm(&self, range: SuppressRange) {
        self.gate.nudge();
        self.inner.lock().suppressor.arm(range, Instant::now());
    }

    /// Wisp 自己写入（恢复）产生的序号区间 (lo, hi]：监听方不会看到它们
    pub(crate) fn record_own(&self, lo: u64, hi: u64) {
        self.inner.lock().record_own(lo, hi);
    }

    /// 开始一次模拟复制回退：此后序号大于 `s0` 的变更都不上报，直到返回的守卫结束
    pub(crate) fn begin_fallback(&self, s0: u64) -> FallbackGuard<'_> {
        // 回退结束时可能改成按 ttl 过期的区间（慢应用晚到的复制）：在 4 Hz 下观察（同 `arm`）
        self.gate.nudge();
        let id = self.inner.lock().begin_pending(s0, Instant::now());
        FallbackGuard {
            state: self,
            id,
            s0,
            done: false,
        }
    }

    /// 监听方看到新序号时调用；返回 true 表示忽略这次变更
    fn observe(&self, change: u64) -> bool {
        self.inner.lock().observe(change, Instant::now())
    }
}

/// 一次模拟复制回退的抑制守卫（[`ClipState::begin_fallback`]）。三种结束方式：
/// - [`FallbackGuard::close`]：剪贴板变了且已恢复——(s0, 恢复后] 整段永久忽略（与 Windows 的区间语义一致）；
/// - [`FallbackGuard::expire_after`]：等待上限内没有变化——改成从 s0+1 起、`ttl` 后过期的区间，覆盖慢应用晚到的复制；
/// - [`FallbackGuard::cancel`]：复制键没有发出去，不会有变更——直接撤销。
///
/// 未显式结束就被 drop（例如 panic 展开）时按 `expire_after(DEFAULT_TTL)` 处理
pub(crate) struct FallbackGuard<'a> {
    state: &'a ClipState,
    id: u64,
    s0: u64,
    done: bool,
}

impl FallbackGuard<'_> {
    pub(crate) fn close(mut self, hi: u64) {
        self.done = true;
        let mut st = self.state.inner.lock();
        if let Some(lo) = st.take_pending(self.id) {
            st.record_own(lo, hi);
        }
    }

    pub(crate) fn expire_after(mut self, ttl: Duration) {
        self.done = true;
        self.finish_with_ttl(ttl);
    }

    pub(crate) fn cancel(mut self) {
        self.done = true;
        self.state.inner.lock().take_pending(self.id);
    }

    fn finish_with_ttl(&self, ttl: Duration) {
        let mut st = self.state.inner.lock();
        if st.take_pending(self.id).is_some() {
            let range = SuppressRange {
                from_change_id: self.s0 + 1,
                ttl,
            };
            st.suppressor.arm(range, Instant::now());
        }
    }
}

impl Drop for FallbackGuard<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.finish_with_ttl(DEFAULT_TTL);
        }
    }
}

fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

/// 当前的 changeCount
pub(crate) fn change_count() -> u64 {
    pool(|| change_count_of(&NSPasteboard::generalPasteboard()))
}

/// 读取纯文本（模拟复制回退用；不检查隐私标记——这是用户自己选中并要求翻译的内容）。
///
/// 序号先于数据变化时（个别应用先 clearContents 再写入）在 `settle` 内每 5 ms 重试一次。整个读取在辅助线程上执行，
/// 最多等 `deadline`：延迟提供数据的应用卡住时返回 None，拖不住调用方
pub(crate) fn read_plain_text(settle: Duration, deadline: Duration) -> Option<String> {
    with_deadline("wisp-clip-text", deadline, move || {
        let until = Instant::now() + settle;
        loop {
            let text = pool(|| {
                NSPasteboard::generalPasteboard()
                    .stringForType(&ns(TYPE_STRING))
                    .map(|s| s.to_string())
            });
            if text.is_some() || Instant::now() >= until {
                return text;
            }
            std::thread::sleep(SETTLE_STEP);
        }
    })
    .flatten()
}

/// 剪贴板上是否有富内容类型（HTML / RTF / 图片 / 文件）：纯文本粘贴前检查调用方是否遵守约定
pub(crate) fn has_rich_types() -> bool {
    pool(|| {
        NSPasteboard::generalPasteboard().types().is_some_and(|types| {
            types.iter().any(|t| {
                let t = t.to_string();
                [TYPE_HTML, TYPE_RTF, TYPE_PNG, TYPE_TIFF, TYPE_FILE_URL].contains(&t.as_str())
            })
        })
    })
}

/// 从 PNG 头读尺寸（IHDR）
fn png_size(png: &[u8]) -> Option<PhysSize> {
    const SIG: &[u8] = b"\x89PNG\r\n\x1a\n";
    if png.len() < 24 || &png[..8] != SIG || &png[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
    let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
    Some(PhysSize { width: w, height: h })
}

/// HTML 的字节解码：UTF-16（带 BOM）或 UTF-8（去掉 BOM，非法序列替换）
fn decode_html(bytes: &[u8]) -> String {
    let utf16 = |be: bool| -> String {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| {
                if be {
                    u16::from_be_bytes([c[0], c[1]])
                } else {
                    u16::from_le_bytes([c[0], c[1]])
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    match bytes {
        [0xFF, 0xFE, ..] => utf16(false),
        [0xFE, 0xFF, ..] => utf16(true),
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// 可以跨线程移交的 NSData（不可变的 NSData 是线程安全的；这里只在两个线程之间移交所有权，不并发访问）
struct SendData(Retained<NSData>);
// SAFETY: 见上；内容在移交之后才被读取，没有任何一方修改它
unsafe impl Send for SendData {}

/// 从 pboard 取出的原始内容。取字节可能被延迟提供数据的应用拖住，因此在辅助线程上、硬截止时间内完成；
/// 文本解码、TIFF 转 PNG 等本进程内的工作在调用方线程上进行，不计入截止时间
#[derive(Default)]
struct PulledBytes {
    change_id: u64,
    from_wisp: bool,
    concealed: bool,
    /// 最多 `max_text_bytes + 1` 字节（多出的 1 字节用来判断截断）
    text: Option<Vec<u8>>,
    html: Option<Vec<u8>>,
    rtf: Option<Vec<u8>>,
    png: Option<Vec<u8>>,
    tiff: Option<SendData>,
    files: Option<Vec<String>>,
    /// 剪贴板上一个类型都没有（刚被 clearContents）
    no_types: bool,
}

/// 读一遍当前剪贴板的原始内容：先看隐私标记再读正文。
///
/// 延迟提供的数据只有整份取到之后才知道大小：超限的格式在拿到长度后立即丢弃、不再拷贝，但无法阻止提供方先把它生成出来
fn pull_once(pb: &NSPasteboard, limits: &ReadLimits) -> PulledBytes {
    let change_id = change_count_of(pb);
    let types: Vec<String> = pb
        .types()
        .map(|a| a.iter().map(|t| t.to_string()).collect())
        .unwrap_or_default();
    let has = |t: &str| types.iter().any(|x| x == t);
    let mut raw = PulledBytes {
        change_id,
        from_wisp: has_wisp_marker(types.iter().map(String::as_str)),
        no_types: types.is_empty(),
        ..PulledBytes::default()
    };
    if macos_is_concealed(types.iter().map(String::as_str)) {
        raw.concealed = true;
        return raw;
    }
    // 上限为 0（设置里关闭了图片 / 富文本）时根本不向数据提供方要这一格式：延迟提供的大图不会因此被生成出来
    let data_within = |t: &str, max: usize| {
        (max > 0)
            .then(|| pb.dataForType(&ns(t)))
            .flatten()
            .filter(|d| !d.is_empty() && d.len() <= max)
    };
    if let Some(data) = pb.dataForType(&ns(TYPE_STRING)) {
        raw.text = Some(data.to_vec_bounded(limits.max_text_bytes));
    }
    if has(TYPE_HTML) {
        raw.html = data_within(TYPE_HTML, limits.max_rich_bytes).map(|d| d.to_vec());
    }
    if has(TYPE_RTF) {
        raw.rtf = data_within(TYPE_RTF, limits.max_rich_bytes).map(|d| d.to_vec());
    }
    if has(TYPE_PNG) {
        raw.png = data_within(TYPE_PNG, limits.max_image_bytes).map(|d| d.to_vec());
    } else if has(TYPE_TIFF) {
        let max_tiff = limits.max_image_bytes.saturating_mul(TIFF_PREFILTER_FACTOR);
        raw.tiff = data_within(TYPE_TIFF, max_tiff).map(SendData);
    }
    if has(TYPE_FILE_URL) {
        let files: Vec<String> = pb
            .pasteboardItems()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.stringForType(&ns(TYPE_FILE_URL)))
                    .filter_map(|s| NSURL::URLWithString(&s))
                    .filter(|u| u.isFileURL())
                    .filter_map(|u| u.filePathURL().and_then(|u| u.path()))
                    .map(|p| p.to_string())
                    .collect()
            })
            .unwrap_or_default();
        if !files.is_empty() {
            raw.files = Some(files);
        }
    }
    raw
}

/// 刚被清空的剪贴板最多等多久写入方写完（clearContents 递增序号，随后的 writeObjects **不**递增，只能看类型列表）
const EMPTY_WAIT: Duration = Duration::from_millis(300);
/// 等待期间查看类型列表的间隔
const EMPTY_POLL: Duration = Duration::from_millis(15);

/// 取原始内容；读取期间剪贴板又变了就重读一次，保证 change_id 与内容一致。读到「一个类型都没有」时，多半是应用先
/// clearContents、再构造数据写入（Office / Java / Electron、延迟写入的应用）：序号不会再变，最多等 [`EMPTY_WAIT`]
/// 让类型出现后重读（在读取的辅助线程上，受 [`READ_DEADLINE`] 约束）；真的被清空时照样返回空内容
fn pull(limits: ReadLimits) -> PulledBytes {
    pool(|| {
        let pb = NSPasteboard::generalPasteboard();
        let mut raw = pull_once(&pb, &limits);
        if raw.no_types && !raw.concealed {
            let started = Instant::now();
            while started.elapsed() < EMPTY_WAIT && change_count_of(&pb) == raw.change_id {
                std::thread::sleep(EMPTY_POLL);
                if pb.types().is_some_and(|t| t.count() > 0) {
                    raw = pull_once(&pb, &limits);
                    break;
                }
            }
        }
        if change_count_of(&pb) == raw.change_id {
            raw
        } else {
            pull_once(&pb, &limits)
        }
    })
}

/// 原始内容 → RawClipboard（解码文本与 HTML，TIFF 转 PNG）
fn decode(raw: PulledBytes, limits: &ReadLimits, source: Option<AppRef>) -> RawClipboard {
    let mut out = RawClipboard {
        change_id: raw.change_id,
        source,
        from_wisp: raw.from_wisp,
        concealed: raw.concealed,
        ..RawClipboard::default()
    };
    if raw.concealed {
        return out;
    }
    if let Some(bytes) = raw.text {
        let (text, truncated) = truncate_utf8_lossy(&bytes, limits.max_text_bytes);
        if !text.is_empty() {
            out.text = Some(text);
            out.text_truncated = truncated;
        }
    }
    if let Some(bytes) = raw.html {
        let html = decode_html(&bytes);
        if !html.trim().is_empty() {
            out.html = Some(html);
        }
    }
    out.rtf = raw.rtf;
    if let Some(png) = raw.png {
        out.image_size = png_size(&png);
        out.image_png = Some(png);
    } else if let Some(SendData(tiff)) = raw.tiff
        && let Some((png, size)) = pool(|| tiff_to_png(&tiff))
        && png.len() <= limits.max_image_bytes
    {
        out.image_size = Some(size);
        out.image_png = Some(png);
    }
    out.files = raw.files;
    out
}

/// 读取剪贴板。`source` 由调用方给出（变更时刻的前台应用）。
/// 从 pboard 取字节有硬截止时间：超时返回 `PlatformError::Timeout`，由调用方退避重试
pub(crate) fn read(limits: &ReadLimits, source: Option<AppRef>) -> Result<RawClipboard, PlatformError> {
    let limits = *limits;
    let raw = with_deadline("wisp-clip-read", READ_DEADLINE, move || pull(limits)).ok_or_else(|| {
        log::info!("clipboard read: no answer within {READ_DEADLINE:?} (data provider busy)");
        PlatformError::Timeout
    })?;
    Ok(decode(raw, &limits, source))
}

fn tiff_to_png(tiff: &NSData) -> Option<(Vec<u8>, PhysSize)> {
    let rep = NSBitmapImageRep::imageRepWithData(tiff)?;
    let size = PhysSize {
        width: u32::try_from(rep.pixelsWide()).ok()?,
        height: u32::try_from(rep.pixelsHigh()).ok()?,
    };
    let props = NSDictionary::new();
    // SAFETY: 空属性字典
    let png = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &props) }?;
    Some((png.to_vec(), size))
}

fn png_to_tiff(png: &NSData) -> Option<Retained<NSData>> {
    let rep = NSBitmapImageRep::imageRepWithData(png)?;
    rep.TIFFRepresentationUsingCompression_factor(NSTIFFCompression::LZW, 0.0)
}

/// 惰性 TIFF 提供方的实例变量
struct TiffIvars {
    /// 待转码的 PNG（与写入剪贴板的是同一份 NSData，不额外拷贝）；提供过一次或剪贴板易主后释放
    png: Mutex<Option<SendData>>,
}

define_class!(
    // SAFETY: NSObject 没有特殊的子类化要求；本类不实现 Drop。实例变量只经 Mutex 访问，回调可以来自任意线程
    // （跨进程请求在主线程的 run loop 上，进程内的后台读取在读取方线程上，均已实测）
    #[unsafe(super(NSObject))]
    #[ivars = TiffIvars]
    struct TiffProvider;

    unsafe impl NSObjectProtocol for TiffProvider {}

    unsafe impl NSPasteboardItemDataProvider for TiffProvider {
        #[unsafe(method(pasteboard:item:provideDataForType:))]
        fn provide_data_for_type(
            &self,
            _pasteboard: Option<&NSPasteboard>,
            item: &NSPasteboardItem,
            data_type: &NSPasteboardType,
        ) {
            guarded("lazy TIFF provider", || self.provide(item, data_type));
        }

        #[unsafe(method(pasteboardFinishedWithDataProvider:))]
        fn pasteboard_finished(&self, _pasteboard: &NSPasteboard) {
            // 剪贴板易主或承诺已全部兑现：释放 PNG（提供方对象本身由剪贴板持有并释放）
            guarded("lazy TIFF provider", || drop(self.ivars().png.lock().take()));
        }
    }
);

impl TiffProvider {
    fn new(png: Retained<NSData>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(TiffIvars {
            png: Mutex::new(Some(SendData(png))),
        });
        // SAFETY: NSObject 的指定初始化方法
        unsafe { msg_send![super(this), init] }
    }

    fn provide(&self, item: &NSPasteboardItem, data_type: &NSPasteboardType) {
        if data_type.to_string() != TYPE_TIFF {
            return;
        }
        let Some(SendData(png)) = self.ivars().png.lock().take() else {
            return;
        };
        let started = Instant::now();
        match pool(|| png_to_tiff(&png)) {
            Some(tiff) => {
                item.setData_forType(&tiff, data_type);
                log::debug!(
                    "clipboard: provided TIFF on demand ({} bytes) in {} ms",
                    tiff.len(),
                    started.elapsed().as_millis()
                );
            }
            None => log::warn!("clipboard: PNG could not be decoded; TIFF not provided"),
        }
    }
}

/// 给 item 声明惰性的 TIFF（图片写 PNG + TIFF，兼容只认 TIFF 的应用）。只有像素数不超过上限时才声明；
/// 返回是否声明成功
fn offer_lazy_tiff(item: &NSPasteboardItem, png: Retained<NSData>) -> bool {
    // SAFETY: dataForType / with_bytes 得到的是不可变 NSData，借用期间不会被修改
    let size = png_size(unsafe { png.as_bytes_unchecked() });
    let Some(size) = size.filter(|s| u64::from(s.width) * u64::from(s.height) <= TIFF_MAX_PIXELS) else {
        return false;
    };
    let provider = TiffProvider::new(png);
    let types = NSArray::from_slice(&[&*ns(TYPE_TIFF)]);
    let offered = item.setDataProvider_forTypes(ProtocolObject::from_ref(&*provider), &types);
    if !offered {
        log::debug!(
            "clipboard: could not offer TIFF for {}x{} image",
            size.width,
            size.height
        );
    }
    offered
}

/// NSData 的辅助：只拷贝前 `max` 字节
trait DataExt {
    fn to_vec_bounded(&self, max: usize) -> Vec<u8>;
}

impl DataExt for NSData {
    fn to_vec_bounded(&self, max: usize) -> Vec<u8> {
        if self.len() <= max {
            return self.to_vec();
        }
        // 只拷贝前 max + 1 字节：多出的 1 字节让 truncate_utf8_lossy 知道发生了截断，并能在原始边界上回退到字符边界
        // SAFETY: dataForType 返回的是不可变的 NSData，借用期间不会被修改
        let bytes = unsafe { self.as_bytes_unchecked() };
        bytes[..=max].to_vec()
    }
}

/// 写入剪贴板：返回写入后的 changeCount
pub(crate) fn write(payload: &ClipPayload, opts: WriteOpts) -> Result<u64, PlatformError> {
    pool(|| {
        let pb = NSPasteboard::generalPasteboard();
        let mut items: Vec<Retained<NSPasteboardItem>> = Vec::new();
        let first = NSPasteboardItem::new();
        let mut first_has_content = false;
        if let Some(text) = payload.text.as_deref() {
            first_has_content |= first.setString_forType(&ns(text), &ns(TYPE_STRING));
        }
        if let Some(html) = payload.html.as_deref() {
            first_has_content |= first.setString_forType(&ns(html), &ns(TYPE_HTML));
        }
        if let Some(rtf) = payload.rtf.as_deref() {
            first_has_content |= first.setData_forType(&NSData::with_bytes(rtf), &ns(TYPE_RTF));
        }
        if let Some(png) = payload.image_png.as_deref() {
            let png = NSData::with_bytes(png);
            if first.setData_forType(&png, &ns(TYPE_PNG)) {
                first_has_content = true;
                // TIFF 只声明，有应用来要时才转码：粘贴路径上不做任何图像编码（粘贴要在 120 ms 内完成）
                offer_lazy_tiff(&first, png);
            }
        }
        // 文件：每个文件一个 file-url item；没有其他内容时第一个文件放进第一个 item
        let mut file_items: Vec<Retained<NSPasteboardItem>> = Vec::new();
        for path in payload.files.iter().flatten() {
            let url = NSURL::fileURLWithPath(&ns(path));
            let Some(s) = url.absoluteString() else { continue };
            if !first_has_content && file_items.is_empty() && items.is_empty() {
                first_has_content = first.setString_forType(&s, &ns(TYPE_FILE_URL));
                continue;
            }
            let item = NSPasteboardItem::new();
            item.setString_forType(&s, &ns(TYPE_FILE_URL));
            file_items.push(item);
        }
        pb.clearContents();
        if !first_has_content {
            // 空载荷：清空即可（不留下只有私有标记的 item）
            return Ok(change_count_of(&pb));
        }
        // 私有标记与「不记录」标记只加在第一个 item 上
        first.setData_forType(&NSData::new(), &ns(WISP_MARKER));
        if opts.transient || opts.exclude_from_history {
            first.setData_forType(&NSData::new(), &ns(TYPE_TRANSIENT));
        }
        items.push(first);
        items.extend(file_items);
        if write_items(&pb, items) {
            Ok(change_count_of(&pb))
        } else {
            Err(PlatformError::Other("NSPasteboard writeObjects failed".into()))
        }
    })
}

fn change_count_of(pb: &NSPasteboard) -> u64 {
    u64::try_from(pb.changeCount()).unwrap_or(0)
}

fn write_items(pb: &NSPasteboard, items: Vec<Retained<NSPasteboardItem>>) -> bool {
    let objs: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> =
        items.into_iter().map(ProtocolObject::from_retained).collect();
    let array = NSArray::from_retained_slice(&objs);
    pb.writeObjects(&array)
}

/// 全类型快照的私有结构
struct MacSnapshot {
    items: Vec<SnapItem>,
}

/// 快照里的一个 item
struct SnapItem {
    /// (type, data)
    entries: Vec<(String, Vec<u8>)>,
    /// Wisp 写入的图片 item：TIFF 是由 PNG 惰性生成的，快照时不去要（否则会在快照线程上同步转码），
    /// 恢复时重新声明
    lazy_tiff: bool,
}

/// 读取全类型快照；超过 `max_bytes` 或超过 `deadline` 返回 None
fn snapshot_inner(max_bytes: usize, deadline: Duration) -> Option<(MacSnapshot, usize)> {
    let started = Instant::now();
    let pb = NSPasteboard::generalPasteboard();
    let mut out = MacSnapshot { items: Vec::new() };
    let mut total = 0usize;
    let Some(items) = pb.pasteboardItems() else {
        return Some((out, 0));
    };
    for item in items.iter() {
        let types: Vec<Retained<NSString>> = item.types().iter().collect();
        let names: Vec<String> = types.iter().map(|t| t.to_string()).collect();
        let has = |t: &str| names.iter().any(|n| n == t);
        let lazy_tiff = has(WISP_MARKER) && has(TYPE_PNG) && has(TYPE_TIFF);
        let mut entries = Vec::new();
        for (t, name) in types.iter().zip(&names) {
            if lazy_tiff && name == TYPE_TIFF {
                continue;
            }
            if started.elapsed() > deadline {
                log::info!("clipboard snapshot: exceeded {deadline:?}, giving up");
                return None;
            }
            let Some(data) = item.dataForType(t) else { continue };
            total = total.saturating_add(data.len());
            if total > max_bytes {
                log::info!("clipboard snapshot: larger than {max_bytes} bytes, giving up");
                return None;
            }
            entries.push((name.clone(), data.to_vec()));
        }
        out.items.push(SnapItem { entries, lazy_tiff });
    }
    Some((out, total))
}

/// 一次恢复的结果
struct Restored {
    /// 恢复产生的序号区间 (lo, hi]
    own: (u64, u64),
    result: Result<u64, PlatformError>,
}

/// 恢复快照。`clearContents` 返回清空后的序号，因此 (返回值 − 1, 写入后的序号] 正好是这次恢复产生的变更
fn restore_inner(snap: MacSnapshot) -> Restored {
    pool(|| {
        let pb = NSPasteboard::generalPasteboard();
        let cleared = u64::try_from(pb.clearContents()).unwrap_or(0);
        let lo = cleared.saturating_sub(1);
        let items: Vec<Retained<NSPasteboardItem>> = snap
            .items
            .into_iter()
            .filter(|it| !it.entries.is_empty())
            .map(|it| {
                let item = NSPasteboardItem::new();
                let mut png = None;
                for (t, data) in it.entries {
                    // 纯文本按字符串写回（恢复后要逐类型一致）：按原始字节 setData 时，系统会从同一项的 RTF 另外
                    // 合成一个原来没有的 public.utf16-external-plain-text，内容取自 RTF、与原纯文本不同；setString 让系统
                    // 从这段字符串推导其他文本类型。不是合法 UTF-8 时退回按字节写
                    if t == TYPE_STRING
                        && let Ok(text) = std::str::from_utf8(&data)
                        && item.setString_forType(&NSString::from_str(text), &ns(&t))
                    {
                        continue;
                    }
                    let data = NSData::with_bytes(&data);
                    item.setData_forType(&data, &ns(&t));
                    if t == TYPE_PNG {
                        png = Some(data);
                    }
                }
                if it.lazy_tiff
                    && let Some(png) = png
                {
                    offer_lazy_tiff(&item, png);
                }
                item
            })
            .collect();
        if items.is_empty() || write_items(&pb, items) {
            let hi = change_count_of(&pb);
            Restored {
                own: (lo, hi),
                result: Ok(hi),
            }
        } else {
            Restored {
                own: (lo, cleared),
                result: Err(PlatformError::Other(
                    "NSPasteboard writeObjects failed during restore".into(),
                )),
            }
        }
    })
}

/// ClipboardAccess 实现
pub(crate) struct MacClipboard {
    ctx: Arc<Ctx>,
}

impl MacClipboard {
    pub(crate) fn new(ctx: Arc<Ctx>) -> Self {
        Self { ctx }
    }
}

/// 快照（供取词回退直接调用）：None = 过大或超时
pub(crate) fn snapshot(max_bytes: usize, deadline: Duration) -> Option<ClipSnapshot> {
    let (snap, bytes) = with_deadline("wisp-clip-snapshot", deadline, move || {
        snapshot_inner(max_bytes, deadline)
    })??;
    Some(ClipSnapshot::new(snap, bytes))
}

/// 恢复快照并让监听方忽略这次写入（写入失败时，已经发生的清空同样不上报）
pub(crate) fn restore(ctx: &Ctx, snap: ClipSnapshot) -> Result<u64, PlatformError> {
    let snap = snap
        .into_inner::<MacSnapshot>()
        .ok_or_else(|| PlatformError::Other("snapshot was not taken on macOS".into()))?;
    let restored = restore_inner(snap);
    let (lo, hi) = restored.own;
    ctx.clip.record_own(lo, hi);
    restored.result
}

/// 监听上下文（DispatchSource 的 context；在取消回调里释放）
struct WatchCtx {
    cb: Box<dyn Fn(u64) + Send + Sync>,
    last: AtomicU64,
    ctx: Weak<Ctx>,
}

extern "C" fn on_tick(p: *mut c_void) {
    // 回调来自 libdispatch：panic 一律在这里截住（extern "C" 里展开会直接 abort）
    guarded("clipboard watch tick", || {
        // SAFETY: p 是 watch() 里泄漏的 Box<WatchCtx>，在取消回调里才释放；同一个定时源的事件回调不会并发，
        // 也不会与取消回调并发
        let w = unsafe { &*p.cast::<WatchCtx>() };
        // 屏幕空闲时的慢速 tick（30 s 一次）：顺便核对屏幕是否其实已经不空闲（漏掉了解锁 / 唤醒通知时自愈）
        let ctx = w.ctx.upgrade();
        if let Some(ctx) = &ctx {
            ctx.clip.gate().recheck_if_paused();
        }
        let cc = change_count();
        let changed = w.last.swap(cc, Ordering::AcqRel) != cc;
        // 用户空闲分档：按输入与变化换 4 Hz / 1 Hz / 0.2 Hz
        if let Some(ctx) = &ctx {
            ctx.clip.gate().after_tick(changed);
        }
        if !changed {
            return;
        }
        if ctx.is_some_and(|ctx| ctx.clip.observe(cc)) {
            return;
        }
        (w.cb)(cc);
    });
}

extern "C" fn on_cancel(p: *mut c_void) {
    guarded("clipboard watch cancel", || {
        // SAFETY: 取消回调在最后一次事件回调之后执行一次，此后不再使用 p
        drop(unsafe { Box::from_raw(p.cast::<WatchCtx>()) });
    });
}

impl ClipboardAccess for MacClipboard {
    fn change_id(&self) -> u64 {
        change_count()
    }

    fn read(&self, limits: &ReadLimits) -> Result<RawClipboard, PlatformError> {
        let source = super::focus::frontmost_any(&self.ctx);
        read(limits, source)
    }

    fn snapshot(&self, max_bytes: usize, deadline: Duration) -> Result<Option<ClipSnapshot>, PlatformError> {
        Ok(snapshot(max_bytes, deadline))
    }

    fn restore(&self, snap: ClipSnapshot) -> Result<u64, PlatformError> {
        restore(&self.ctx, snap)
    }

    fn watch(&self, cb: Box<dyn Fn(u64) + Send + Sync>) -> Result<HookHandle, PlatformError> {
        let ctx = Box::new(WatchCtx {
            cb,
            last: AtomicU64::new(change_count()),
            ctx: Arc::downgrade(&self.ctx),
        });
        let queue = DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(DispatchQoS::Utility));
        // SAFETY: 定时器类型常量由 libdispatch 导出；handle / mask 对定时器无意义，传 0
        let source = unsafe {
            DispatchSource::new(
                (&raw const dispatch2::_dispatch_source_type_timer).cast_mut(),
                0,
                0,
                Some(&queue),
            )
        };
        // SAFETY: context 在取消回调里释放；事件与取消回调都通过 context 访问它
        unsafe { source.set_context(Box::into_raw(ctx).cast::<c_void>()) };
        source.set_event_handler_f(on_tick);
        source.set_cancel_handler_f(on_cancel);
        // 周期由闸门设定：平时 4 Hz（leeway 50 ms），用户安静 30 s / 5 分钟起 1 Hz / 0.2 Hz，显示器睡眠 / 锁屏时 30 s 一次
        let gate_id = self.ctx.clip.gate().add(&self.ctx, source.clone());
        source.activate();
        let weak = Arc::downgrade(&self.ctx);
        Ok(HookHandle::new(move || {
            if let Some(ctx) = weak.upgrade() {
                ctx.clip.gate().remove(gate_id);
            }
            source.cancel();
        }))
    }

    fn suppress(&self, range: SuppressRange) {
        self.ctx.clip.arm(range);
    }

    fn poll_now(&self) {
        self.ctx.clip.gate().nudge();
    }
}

#[cfg(test)]
mod tests {
    use objc2_app_kit::NSBitmapFormat;

    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn pending_fallback_ignores_changes_however_long_it_takes() {
        // 旧做法的 ttl 从布防时刻计 330 ms；快照 + 键码 + 等待 + 读文本最坏约 360 ms，tick 落在其间就会把目标应用的
        // 复制或恢复的中间状态报出去。开放区间不按时间计，直到恢复后关闭
        let t0 = Instant::now();
        let mut st = IgnoreState::default();
        let id = st.begin_pending(10, t0);
        assert!(st.observe(11, t0 + 340 * MS), "target app's copy");
        assert!(st.observe(12, t0 + 900 * MS), "restore's clearContents");
        let lo = st.take_pending(id).expect("pending");
        st.record_own(lo, 13);
        assert!(st.observe(13, t0 + 950 * MS), "restore's final change");
        assert!(!st.observe(14, t0 + 960 * MS), "the user's next copy is reported");
    }

    #[test]
    fn pending_interval_has_a_safety_age_limit() {
        let t0 = Instant::now();
        let mut st = IgnoreState::default();
        st.begin_pending(10, t0);
        assert!(st.should_ignore(11, t0 + PENDING_MAX_AGE));
        assert!(!st.should_ignore(11, t0 + PENDING_MAX_AGE + MS));
    }

    #[test]
    fn own_ranges_are_bounded_and_pruned() {
        let now = Instant::now();
        let mut st = IgnoreState::default();
        st.record_own(5, 7);
        assert!(!st.observe(5, now), "lower bound is exclusive");
        assert!(st.observe(6, now));
        assert!(st.observe(7, now));
        assert!(!st.observe(8, now));
        assert!(st.own.is_empty(), "passed ranges are dropped");
        // 空区间不登记
        st.record_own(9, 9);
        assert!(st.own.is_empty());
        // 上限：最早的被丢弃
        for i in 0..(MAX_OWN_RANGES as u64 + 4) {
            st.record_own(100 + i * 10, 101 + i * 10);
        }
        assert_eq!(st.own.len(), MAX_OWN_RANGES);
    }

    #[test]
    fn counter_reset_does_not_swallow_later_changes() {
        // pboard 守护进程重启后 changeCount 从小数字重新计数：旧的区间与 ttl 区间都作废，新变更照常上报
        let now = Instant::now();
        let mut st = IgnoreState::default();
        st.suppressor.arm(
            SuppressRange {
                from_change_id: 1000,
                ttl: Duration::from_secs(60),
            },
            now,
        );
        st.record_own(998, 1001);
        assert!(st.observe(1000, now));
        assert!(!st.observe(3, now), "first change after the restart");
        for cc in 4..=1001 {
            assert!(!st.observe(cc, now), "change {cc} after the restart must be reported");
        }
    }

    #[test]
    fn fallback_guard_outcomes() {
        let clip = ClipState::default();
        // 变了且已恢复：(s0, hi] 永久忽略
        let g = clip.begin_fallback(20);
        assert!(clip.observe(21));
        g.close(23);
        assert!(clip.observe(22) && clip.observe(23));
        assert!(!clip.observe(24));
        // 没有变化：从 s0+1 起再保留 ttl
        let g = clip.begin_fallback(30);
        g.expire_after(40 * MS);
        assert!(clip.observe(31), "late copy within the slack");
        std::thread::sleep(60 * MS);
        assert!(!clip.observe(32), "after the slack");
        // 键没发出去：立即撤销
        let g = clip.begin_fallback(40);
        g.cancel();
        assert!(!clip.observe(41));
        // 未结束就 drop（例如 panic 展开）：按默认 ttl 处理，不会永久屏蔽
        drop(clip.begin_fallback(50));
        assert!(clip.observe(51));
        assert!(clip.inner.lock().pending.is_empty());
    }

    /// 生成一张 w × h 的 PNG
    fn png_of(w: isize, h: isize) -> Retained<NSData> {
        // SAFETY: 让 AppKit 自己分配像素缓冲区；参数描述的是 8 位 RGBA
        let rep = unsafe {
            NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bitmapFormat_bytesPerRow_bitsPerPixel(
                NSBitmapImageRep::alloc(),
                std::ptr::null_mut(),
                w,
                h,
                8,
                4,
                true,
                false,
                objc2_app_kit::NSDeviceRGBColorSpace,
                NSBitmapFormat::empty(),
                0,
                0,
            )
        }
        .expect("bitmap");
        let props = NSDictionary::new();
        // SAFETY: 空属性字典
        unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &props) }.expect("png")
    }

    #[test]
    fn tiff_is_offered_lazily_and_converted_on_demand() {
        // 用进程私有的唯一剪贴板，不碰用户的系统剪贴板
        pool(|| {
            let pb = NSPasteboard::pasteboardWithUniqueName();
            let png = png_of(3, 2);
            let item = NSPasteboardItem::new();
            assert!(item.setData_forType(&png, &ns(TYPE_PNG)));
            assert!(offer_lazy_tiff(&item, png), "TIFF declared");
            pb.clearContents();
            assert!(write_items(&pb, vec![item]), "write to a private pasteboard");
            let types: Vec<String> = pb
                .types()
                .map(|t| t.iter().map(|x| x.to_string()).collect())
                .unwrap_or_default();
            assert!(types.iter().any(|t| t == TYPE_TIFF), "{types:?}");
            // 同进程读取：提供方被同步调用，当场转码
            let tiff = pb.dataForType(&ns(TYPE_TIFF)).expect("TIFF provided on demand");
            let rep = NSBitmapImageRep::imageRepWithData(&tiff).expect("valid TIFF");
            assert_eq!((rep.pixelsWide(), rep.pixelsHigh()), (3, 2));
            release_globally(&pb);
        });
    }

    /// 释放测试用的唯一剪贴板（`-releaseGlobally` 没有绑定，直接发消息）
    fn release_globally(pb: &NSPasteboard) {
        // SAFETY: 无参数、无返回值的实例方法
        unsafe {
            let () = msg_send![pb, releaseGlobally];
        }
    }

    #[test]
    fn oversized_images_get_png_only() {
        // 只看 IHDR 里的尺寸，不解码：超过上限不声明 TIFF
        let mut fake = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        fake.extend_from_slice(&10_000u32.to_be_bytes());
        fake.extend_from_slice(&10_000u32.to_be_bytes());
        let item = NSPasteboardItem::new();
        assert!(!offer_lazy_tiff(&item, NSData::with_bytes(&fake)));
    }
}
