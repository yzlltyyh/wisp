//! 截图的持有者（截图帧归编排器所有）：
//! - **冻结帧**：当前 Overlay 会话的整屏帧，以 `capture/` 前缀注册到 AssetRegistry（[`FrameSource`]）。路径用
//!   `crate::assets::capture_asset_path` 生成、`parse_capture_path` 解析（MonitorId 以十六进制编码）。
//!   Overlay 结束（commit 后 `overlay_done`、取消、出错）时整屏数据随 [`CaptureStore::clear_frames`] 释放。
//! - **裁剪图**：commit 时裁出的选区，按长边缩放并编码后留在内存里（不落盘），供 OCR 请求、重新识别、「查看原文」与
//!   缩略图 `crop/<capture_id>.png`（[`CropSource`]）使用。归属与流相同（面板会话 / pinned 窗口），会话结束、pinned
//!   关闭或前端 `capture_release` 时释放（截图保留到面板关闭或固定为止）。
//!
//! 编码在阻塞线程上进行、不阻塞 commit 的返回；缩略图请求先于编码完成到达时，在 `wisp://` 的阻塞线程上等它（有上限）。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex, RwLock};
use tokio_util::bytes::Bytes;
use wisp_capture::{Frame, RgbaImage};
use wisp_types::{MonitorId, OcrMode, PhysSize};

use super::registry::Owner;
use crate::assets::{Asset, AssetSource, CaptureVariant, parse_capture_path};
use crate::error::IpcError;

/// 缩略图 / 请求等待编码完成的上限
pub const ENCODE_WAIT: Duration = Duration::from_secs(5);

/// 编码好的裁剪图（缩略图资源与 OCR 请求共用同一份字节，不复制）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encoded {
    pub bytes: Arc<Vec<u8>>,
    pub mime: &'static str,
    /// 编码后的像素尺寸（换模型时按目标协议的上限核对）
    pub width: u32,
    pub height: u32,
}

/// `Arc<Vec<u8>>` → `Bytes` 的零拷贝包装（`Bytes::from_owner` 要求 `AsRef<[u8]>`）
struct SharedBytes(Arc<Vec<u8>>);

impl AsRef<[u8]> for SharedBytes {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

impl Encoded {
    /// 交给 `UserContent::Image` 的字节：与缩略图共享同一块内存（每次 OCR 请求不复制 MB 级的整张图）
    pub fn llm_bytes(&self) -> Bytes {
        Bytes::from_owner(SharedBytes(Arc::clone(&self.bytes)))
    }
}

/// 转录所用的模型（`provider_id`, `model_id`）
pub type ModelKey = (String, String);

/// 裁剪图的状态
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CropImage {
    /// 编码中
    Pending,
    Ready(Arc<Encoded>),
    /// 编码失败或已被放弃
    Failed,
}

/// 一张裁剪图
pub struct Crop {
    pub id: String,
    /// 选区的物理像素尺寸（`OcrPayload.image_size`）
    pub size: PhysSize,
    /// 截图直接翻译的截图（「查看原文」的转录会缓存下来）
    pub direct: bool,
    owner: Mutex<Owner>,
    image: Mutex<CropImage>,
    ready: Condvar,
    /// 缓存的 paragraphs 转录与它用的模型
    transcript: Mutex<Option<(ModelKey, String)>>,
    /// OCR 历史的请求序号（请求结束时同步领取，决定同一模式两次结果的先后；原子计数，领取时不会被进行中的写库挡住）
    history_tickets: AtomicU64,
    /// 这张截图记进 OCR 历史的条目：同一模式重新识别时新结果取代旧条目。「读要取代的条目 → 写库 →
    /// 记下新条目」在这把锁里串行完成（[`Crop::record_history`]，只在阻塞线程上持有）
    history: Mutex<HistorySlots>,
    /// OCR 历史用的缩略图（长边 256 px），编码截图之后从裁剪帧生成（只在 `ocr.history` 开启时）
    thumb: Mutex<Option<Arc<Vec<u8>>>>,
    /// 离线引擎用的像素（缩放后的 RGBA，不经 PNG 编码 / 解码）：只在识别目标是离线引擎时留着，请求结束即释放
    /// （之后的重新识别从编码字节解码）
    pixels: Mutex<Pixels>,
    pixels_ready: Condvar,
}

/// 裁剪图的像素
#[derive(Debug, Clone, Default)]
enum Pixels {
    /// 不保留（视觉 LLM 与云端只用编码字节）
    #[default]
    None,
    /// 缩放中
    Pending,
    Ready(Arc<RgbaImage>),
}

/// 一张截图在 OCR 历史里的条目：每个模式最近写入的那一条
#[derive(Debug, Default)]
struct HistorySlots {
    /// (模式, 写入它的请求序号, 条目 id)
    slots: Vec<(OcrMode, u64, i64)>,
}

impl Crop {
    /// 编码完成
    pub fn set_ready(&self, enc: Encoded) {
        *self.image.lock() = CropImage::Ready(Arc::new(enc));
        self.ready.notify_all();
    }

    /// 编码失败（或裁剪图被释放）
    pub fn set_failed(&self) {
        let mut g = self.image.lock();
        if *g == CropImage::Pending {
            *g = CropImage::Failed;
        }
        self.ready.notify_all();
        drop(g);
        self.drop_pixels();
    }

    /// 这次识别要用像素（离线引擎）：缩放完成时由 [`Self::set_pixels`] 交来
    pub fn expect_pixels(&self) {
        *self.pixels.lock() = Pixels::Pending;
    }

    /// 缩放好的像素
    pub fn set_pixels(&self, img: Arc<RgbaImage>) {
        let mut g = self.pixels.lock();
        if matches!(*g, Pixels::Pending) {
            *g = Pixels::Ready(img);
        }
        self.pixels_ready.notify_all();
    }

    /// 等像素（**阻塞**，最多 `timeout`）；没有保留像素（或已释放、超时）时为 None，调用方改从编码字节解码
    pub fn wait_pixels(&self, timeout: Duration) -> Option<Arc<RgbaImage>> {
        let until = Instant::now() + timeout;
        let mut g = self.pixels.lock();
        loop {
            match &*g {
                Pixels::Ready(img) => return Some(Arc::clone(img)),
                Pixels::None => return None,
                Pixels::Pending => {
                    if self.pixels_ready.wait_until(&mut g, until).timed_out() {
                        return None;
                    }
                }
            }
        }
    }

    /// 释放像素（识别结束；之后的重新识别从编码字节解码）
    pub fn drop_pixels(&self) {
        let mut g = self.pixels.lock();
        *g = Pixels::None;
        self.pixels_ready.notify_all();
    }

    /// 等编码完成（**阻塞**，最多 `timeout`；超时返回 Pending）
    pub fn wait(&self, timeout: Duration) -> CropImage {
        let until = Instant::now() + timeout;
        let mut g = self.image.lock();
        while *g == CropImage::Pending {
            if self.ready.wait_until(&mut g, until).timed_out() {
                break;
            }
        }
        g.clone()
    }

    pub fn owner(&self) -> Owner {
        self.owner.lock().clone()
    }

    pub fn set_owner(&self, owner: Owner) {
        *self.owner.lock() = owner;
    }

    /// 缓存的 paragraphs 转录（直接翻译的「查看原文」，或两次调用模式的第一步）。`model` 为 None（没有指定模型）
    /// 时返回最近一次的转录；指定了模型时只有同一个模型的转录才算命中（换模型就要重新识别）
    pub fn transcript_for(&self, model: Option<&ModelKey>) -> Option<String> {
        self.transcript
            .lock()
            .as_ref()
            .filter(|(m, _)| model.is_none_or(|want| want == m))
            .map(|(_, t)| t.clone())
    }

    /// 记下转录（覆盖之前的，包括换了模型之后的新结果）
    pub fn set_transcript(&self, model: ModelKey, text: String) {
        *self.transcript.lock() = Some((model, text));
    }

    /// 领取一个 OCR 历史的请求序号。**在请求结束时同步调用**（不要放进后台任务）：同一模式的两次结果按序号决定谁取代谁，
    /// 与后台写库任务的执行顺序无关。不拿历史锁（那把锁在写库期间一直被持有），异步的收尾路径不会被慢写入挡住
    pub fn history_ticket(&self) -> u64 {
        self.history_tickets.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// 把一次识别结果记进 OCR 历史（**阻塞**，在阻塞线程上调用）：在这张截图的历史锁里串行完成「找出同一模式要取代的
    /// 条目 → `write(要取代的 id)` 写库、返回新条目 id → 记下新条目」。两次重新识别的写库任务即使交错、乱序执行，也
    /// 只留下序号较新的那一条：比 `ticket` 更新的结果已经写入时不再写（它本来就会被取代）。`write` 返回 None（关闭了
    /// 历史、写入失败）时保持原样
    pub fn record_history(&self, mode: OcrMode, ticket: u64, write: impl FnOnce(Option<i64>) -> Option<i64>) {
        let mut h = self.history.lock();
        let current = h.slots.iter().find(|(m, _, _)| *m == mode).copied();
        if current.is_some_and(|(_, newer, _)| newer > ticket) {
            return;
        }
        let Some(id) = write(current.map(|(_, _, id)| id)) else {
            return;
        };
        h.slots.retain(|(m, _, _)| *m != mode);
        h.slots.push((mode, ticket, id));
    }

    /// 这张截图用这个模式记进 OCR 历史的条目（还没有时为 None）
    #[cfg(test)]
    pub fn history_id(&self, mode: OcrMode) -> Option<i64> {
        self.history
            .lock()
            .slots
            .iter()
            .find(|(m, _, _)| *m == mode)
            .map(|(_, _, id)| *id)
    }

    /// 在阻塞线程上从裁剪帧（RGBA8，宽 × 高）生成 OCR 历史用的缩略图（长边 256 px，逐行面积平均：4K 帧约 16 ms，不复制整帧）。编码完成
    /// 之后调用（只在 `ocr.history` 开启时），与已经发出的识别请求并行，不占关键路径。请求在缩略图生成之前就结束时
    /// （实际不会：识别至少要一次网络往返），写入历史时由存储层从原图生成
    pub fn spawn_history_thumb(self: &Arc<Self>, img: Arc<RgbaImage>) {
        let crop = Arc::clone(self);
        tauri::async_runtime::spawn_blocking(move || {
            if let Some(t) = wisp_clipboard::ocr_thumbnail(img.width(), img.height(), img.as_raw()) {
                *crop.thumb.lock() = Some(Arc::new(t));
            }
        });
    }

    /// OCR 历史用的缩略图（没有生成过时为 None：写入时由存储层从原图生成）
    pub fn thumb(&self) -> Option<Arc<Vec<u8>>> {
        self.thumb.lock().clone()
    }
}

/// 当前 Overlay 会话的冻结帧
struct FrameSet {
    session_id: u64,
    frames: Vec<Arc<Frame>>,
}

/// 截图存储
pub struct CaptureStore {
    frames: RwLock<Option<FrameSet>>,
    crops: Mutex<HashMap<String, Arc<Crop>>>,
    next: AtomicU64,
}

impl CaptureStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            frames: RwLock::new(None),
            crops: Mutex::new(HashMap::new()),
            next: AtomicU64::new(0),
        })
    }

    /// 登记 Overlay 会话的冻结帧（替换上一组）
    pub fn set_frames(&self, session_id: u64, frames: Vec<Arc<Frame>>) {
        *self.frames.write() = Some(FrameSet { session_id, frames });
    }

    /// 释放某个 Overlay 会话的冻结帧（会话不符时不动）
    pub fn clear_frames(&self, session_id: u64) {
        let mut g = self.frames.write();
        if g.as_ref().is_some_and(|f| f.session_id == session_id) {
            *g = None;
        }
    }

    /// 某个会话某块显示器的帧
    pub fn frame(&self, session_id: u64, monitor_id: &str) -> Option<Arc<Frame>> {
        self.frames
            .read()
            .as_ref()
            .filter(|f| f.session_id == session_id)
            .and_then(|f| f.frames.iter().find(|fr| fr.monitor.id == monitor_id).cloned())
    }

    /// 是否还持有冻结帧（测试用）
    #[cfg(test)]
    pub(crate) fn has_frames(&self) -> bool {
        self.frames.read().is_some()
    }

    /// 新建一张（编码中的）裁剪图
    pub fn new_crop(&self, owner: Owner, size: PhysSize, direct: bool) -> Arc<Crop> {
        let id = format!("c-{}", self.next.fetch_add(1, Ordering::Relaxed) + 1);
        let crop = Arc::new(Crop {
            id: id.clone(),
            size,
            direct,
            owner: Mutex::new(owner),
            image: Mutex::new(CropImage::Pending),
            ready: Condvar::new(),
            transcript: Mutex::new(None),
            history_tickets: AtomicU64::new(0),
            history: Mutex::new(HistorySlots::default()),
            thumb: Mutex::new(None),
            pixels: Mutex::new(Pixels::None),
            pixels_ready: Condvar::new(),
        });
        self.crops.lock().insert(id, Arc::clone(&crop));
        crop
    }

    pub fn crop(&self, id: &str) -> Option<Arc<Crop>> {
        self.crops.lock().get(id).cloned()
    }

    /// 释放一张裁剪图（还在等它的请求会拿到 Failed）
    pub fn remove(&self, id: &str) -> bool {
        self.remove_if(id, |_| true).unwrap_or(false)
    }

    /// 前端的 `capture_release`：`allow(宿主)` 为 false（调用方不是截图的宿主窗口）时 Err（编排器记日志后忽略）；
    /// 不存在时 Ok(false)
    pub fn remove_if(&self, id: &str, allow: impl Fn(&Owner) -> bool) -> Result<bool, IpcError> {
        let removed = {
            let mut g = self.crops.lock();
            match g.get(id) {
                Some(c) if !allow(&c.owner()) => return Err(IpcError::not_found("capture")),
                Some(_) => g.remove(id),
                None => None,
            }
        };
        if let Some(c) = &removed {
            c.set_failed();
        }
        Ok(removed.is_some())
    }

    /// 释放宿主满足条件的全部裁剪图，返回释放的张数
    pub fn end_where(&self, pred: impl Fn(&Owner) -> bool) -> usize {
        let removed: Vec<Arc<Crop>> = {
            let mut g = self.crops.lock();
            let ids: Vec<String> = g
                .iter()
                .filter(|(_, c)| pred(&c.owner()))
                .map(|(id, _)| id.clone())
                .collect();
            ids.iter().filter_map(|id| g.remove(id)).collect()
        };
        for c in &removed {
            c.set_failed();
        }
        removed.len()
    }

    /// 把宿主为 `from` 的裁剪图全部改归 `to`（固定交接）
    pub fn reassign(&self, from: &Owner, to: &Owner) {
        for c in self.crops.lock().values() {
            if c.owner() == *from {
                c.set_owner(to.clone());
            }
        }
    }

    /// 裁剪图所属的全部面板会话（去重）
    pub fn panel_sessions(&self) -> Vec<u64> {
        let mut out: Vec<u64> = self
            .crops
            .lock()
            .values()
            .filter_map(|c| match c.owner() {
                Owner::Panel(s) => Some(s),
                _ => None,
            })
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// 裁剪图张数（测试用）
    #[cfg(test)]
    pub(crate) fn crop_count(&self) -> usize {
        self.crops.lock().len()
    }
}

/// `capture/<session>/<monitor-hex>/{half,full}` 与 `…/region/<x>/<y>/<w>/<h>[/<max_px>]`：原始 RGBA，带 `x-wisp-width` /
/// `x-wisp-height`。
///
/// `region`每次新建一块只属于这次响应的缓冲（[`wisp_capture::Frame::region_preview`]：macOS 原生冻结帧只转换
/// 这一块），响应时直接移交、不再拷贝；不在帧内的区域 404
pub struct FrameSource(pub Arc<CaptureStore>);

/// window lab（只在 debug / fake-llm 构建）：冻结帧资源的请求次数（整屏 full / 小块 region），内存步骤据此核对页面实际走的
/// 数据流
#[cfg(any(debug_assertions, feature = "fake-llm"))]
pub static LAB_FRAME_FETCHES: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

impl AssetSource for FrameSource {
    fn fetch(&self, path: &str) -> Option<Asset> {
        let (session_id, monitor_id, variant): (u64, MonitorId, CaptureVariant) = parse_capture_path(path)?;
        let frame = self.0.frame(session_id, &monitor_id)?;
        #[cfg(any(debug_assertions, feature = "fake-llm"))]
        {
            let i = usize::from(matches!(variant, CaptureVariant::Region(..)));
            LAB_FRAME_FETCHES[i].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let (w, h, bytes) = match variant {
            CaptureVariant::Full => (frame.width, frame.height, frame.rgba()),
            CaptureVariant::Half => frame.rgba_half(),
            CaptureVariant::Region(r, max_px) => {
                let max_px = max_px.map_or(crate::assets::REGION_MAX_PX, u64::from);
                let (w, h, px) = frame.region_preview(r, max_px)?;
                (w, h, Arc::new(px))
            }
        };
        Some(Asset {
            bytes,
            mime: "application/octet-stream",
            extra_headers: vec![("x-wisp-width", w.to_string()), ("x-wisp-height", h.to_string())],
        })
    }
}

/// `crop/<capture_id>.png`：编码好的裁剪图（OCR 面板的缩略图；编码未完成时在这里等，最多 [`ENCODE_WAIT`]）
pub struct CropSource(pub Arc<CaptureStore>);

impl AssetSource for CropSource {
    fn fetch(&self, path: &str) -> Option<Asset> {
        let id = path.strip_suffix(".png")?;
        let crop = self.0.crop(id)?;
        match crop.wait(ENCODE_WAIT) {
            CropImage::Ready(enc) => Some(Asset {
                bytes: Arc::clone(&enc.bytes),
                mime: enc.mime,
                extra_headers: Vec::new(),
            }),
            CropImage::Pending | CropImage::Failed => None,
        }
    }
}

/// 缩略图的资源路径（`OcrPayload.thumb_url`；前端经 `resolveAssetUrl` 转成 URL）
pub fn thumb_path(capture_id: &str) -> String {
    format!("crop/{capture_id}.png")
}

#[cfg(test)]
mod tests {
    use wisp_types::{MonitorInfo, PhysPoint, Rect};

    use super::*;
    use crate::assets::capture_asset_path;

    pub fn monitor(id: &str) -> MonitorInfo {
        MonitorInfo {
            id: id.to_owned(),
            name: id.to_owned(),
            physical_origin: PhysPoint { x: 0, y: 0 },
            physical_size: PhysSize { width: 8, height: 4 },
            scale_factor: 2.0,
            frame: Rect::new(0.0, 0.0, 4.0, 2.0),
            work_area: Rect::new(0.0, 0.0, 4.0, 2.0),
            is_primary: true,
        }
    }

    #[test]
    fn frames_are_served_by_session_and_released() {
        let store = CaptureStore::new();
        let id = r"\\.\DISPLAY1";
        let frame = Frame::from_rgba(monitor(id), 8, 4, vec![7; 8 * 4 * 4]);
        store.set_frames(3, vec![Arc::new(frame)]);
        let src = FrameSource(Arc::clone(&store));
        let full = capture_asset_path(3, id, CaptureVariant::Full);
        let rest = full.strip_prefix("capture/").expect("prefix");
        let asset = src.fetch(rest).expect("full frame");
        assert_eq!(asset.bytes.len(), 8 * 4 * 4);
        assert!(asset.extra_headers.contains(&("x-wisp-width", "8".to_owned())));
        let half = capture_asset_path(3, id, CaptureVariant::Half);
        let asset = src.fetch(half.strip_prefix("capture/").expect("prefix")).expect("half");
        assert!(asset.extra_headers.contains(&("x-wisp-height", "2".to_owned())));
        // region：帧内的一块逐像素、只属于这次响应（移交不拷贝）；越界 404
        let region = capture_asset_path(
            3,
            id,
            CaptureVariant::Region(
                wisp_types::PhysRect {
                    x: 2,
                    y: 1,
                    width: 3,
                    height: 2,
                },
                None,
            ),
        );
        let asset = src
            .fetch(region.strip_prefix("capture/").expect("prefix"))
            .expect("region");
        assert_eq!(asset.bytes.len(), 3 * 2 * 4);
        assert_eq!(Arc::strong_count(&asset.bytes), 1, "region 的缓冲只属于这次响应");
        assert!(asset.extra_headers.contains(&("x-wisp-width", "3".to_owned())));
        assert!(asset.extra_headers.contains(&("x-wisp-height", "2".to_owned())));
        // 第 5 段的面积上限：整帧（8 × 4）按 8 px 取低分辨率底图 → 4 × 2
        let low = format!("{}/0/0/8/4/8", crate::assets::capture_region_base(3, id));
        let asset = src.fetch(low.strip_prefix("capture/").expect("prefix")).expect("low");
        assert_eq!(asset.bytes.len(), 4 * 2 * 4);
        assert!(asset.extra_headers.contains(&("x-wisp-width", "4".to_owned())));
        assert!(asset.extra_headers.contains(&("x-wisp-height", "2".to_owned())));
        let outside = format!("{}/6/0/3/1", crate::assets::capture_region_base(3, id));
        assert!(src.fetch(outside.strip_prefix("capture/").expect("prefix")).is_none());
        // 别的会话、释放之后都是 404
        let other = capture_asset_path(4, id, CaptureVariant::Full);
        assert!(src.fetch(other.strip_prefix("capture/").expect("prefix")).is_none());
        store.clear_frames(3);
        assert!(!store.has_frames());
        assert!(src.fetch(rest).is_none());
    }

    #[test]
    fn crop_waits_for_encoding_and_follows_owner() {
        let store = CaptureStore::new();
        let crop = store.new_crop(Owner::Panel(1), PhysSize { width: 10, height: 5 }, false);
        let src = CropSource(Arc::clone(&store));
        let path = thumb_path(&crop.id);
        let rest = path.strip_prefix("crop/").expect("prefix").to_owned();
        let waiter = std::thread::spawn({
            let store = Arc::clone(&store);
            move || CropSource(store).fetch(&rest)
        });
        std::thread::sleep(Duration::from_millis(30));
        crop.set_ready(Encoded {
            bytes: Arc::new(b"\x89PNG".to_vec()),
            mime: "image/png",
            width: 1,
            height: 1,
        });
        let asset = waiter.join().expect("join").expect("thumb after encoding");
        assert_eq!(asset.mime, "image/png");
        assert!(src.fetch("c-404.png").is_none());
        // 只有宿主窗口能释放
        assert!(store.remove_if(&crop.id, |o| *o == Owner::Pinned("p".into())).is_err());
        assert_eq!(store.remove_if("c-404", |_| false).ok(), Some(false));
        // 固定交接后归 pinned；会话结束不再释放它
        store.reassign(&Owner::Panel(1), &Owner::Pinned("p".into()));
        assert_eq!(store.end_where(|o| *o == Owner::Panel(1)), 0);
        assert_eq!(store.end_where(|o| *o == Owner::Pinned("p".into())), 1);
        assert_eq!(store.crop_count(), 0);
    }

    #[test]
    fn llm_bytes_share_the_thumbnail_memory() {
        let enc = Encoded {
            bytes: Arc::new(vec![1, 2, 3, 4]),
            mime: "image/png",
            width: 1,
            height: 1,
        };
        let b = enc.llm_bytes();
        assert_eq!(&b[..], &[1, 2, 3, 4]);
        assert_eq!(b.as_ptr(), enc.bytes.as_ptr(), "零拷贝");
    }

    #[test]
    fn history_records_are_serialized_and_ordered_by_ticket() {
        let store = CaptureStore::new();
        let crop = store.new_crop(Owner::Panel(1), PhysSize { width: 4, height: 4 }, false);
        // 第一次识别：没有要取代的
        let t1 = crop.history_ticket();
        crop.record_history(OcrMode::Lines, t1, |replaces| {
            assert_eq!(replaces, None);
            Some(10)
        });
        // 两次重新识别，写库任务乱序执行：较新的（t3）先写，取代 10；较旧的（t2）随后到达，不再写
        let (t2, t3) = (crop.history_ticket(), crop.history_ticket());
        crop.record_history(OcrMode::Lines, t3, |replaces| {
            assert_eq!(replaces, Some(10));
            Some(12)
        });
        crop.record_history(OcrMode::Lines, t2, |_| panic!("比已写入的结果旧，不应再写"));
        assert_eq!(crop.history_id(OcrMode::Lines), Some(12));
        // 换模式各留一条；写入失败（None）保持原样
        let t4 = crop.history_ticket();
        crop.record_history(OcrMode::Markdown, t4, |replaces| {
            assert_eq!(replaces, None);
            None
        });
        assert_eq!(crop.history_id(OcrMode::Markdown), None);
        let t5 = crop.history_ticket();
        crop.record_history(OcrMode::Markdown, t5, |_| Some(13));
        assert_eq!(crop.history_id(OcrMode::Lines), Some(12));
        assert_eq!(crop.history_id(OcrMode::Markdown), Some(13));
        // 并发：同一张截图的多个写库任务在锁里串行，每次都看到上一次的结果
        let crop = Arc::new(crop);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (crop, seen) = (Arc::clone(&crop), Arc::clone(&seen));
                let ticket = crop.history_ticket();
                std::thread::spawn(move || {
                    crop.record_history(OcrMode::Translate, ticket, |replaces| {
                        std::thread::sleep(Duration::from_millis(2));
                        seen.lock().push((ticket, replaces));
                        Some(100 + i64::try_from(ticket).expect("序号"))
                    });
                })
            })
            .collect();
        for h in handles {
            h.join().expect("join");
        }
        let seen = seen.lock();
        // 每次写入取代的都是此前最后写入的那一条（链式），最终留下的是某个已写入的序号最大的结果
        for pair in seen.windows(2) {
            assert_eq!(pair[1].1, Some(100 + i64::try_from(pair[0].0).expect("序号")));
            assert!(pair[1].0 > pair[0].0, "乱序到达的旧结果不再写");
        }
        let last = seen.last().expect("至少写入一次").0;
        assert_eq!(
            crop.history_id(OcrMode::Translate),
            Some(100 + i64::try_from(last).expect("序号"))
        );
    }

    #[test]
    fn transcript_hits_only_for_the_same_model() {
        let store = CaptureStore::new();
        let crop = store.new_crop(Owner::Panel(1), PhysSize { width: 1, height: 1 }, true);
        let a: ModelKey = ("p".into(), "a".into());
        let b: ModelKey = ("p".into(), "b".into());
        assert_eq!(crop.transcript_for(None), None);
        crop.set_transcript(a.clone(), "from a".into());
        assert_eq!(crop.transcript_for(None).as_deref(), Some("from a"));
        assert_eq!(crop.transcript_for(Some(&a)).as_deref(), Some("from a"));
        assert_eq!(crop.transcript_for(Some(&b)), None, "换了模型要重新识别");
        crop.set_transcript(b.clone(), "from b".into());
        assert_eq!(crop.transcript_for(None).as_deref(), Some("from b"));
    }

    #[test]
    fn removed_crop_wakes_waiters() {
        let store = CaptureStore::new();
        let crop = store.new_crop(Owner::Unbound, PhysSize { width: 1, height: 1 }, true);
        let id = crop.id.clone();
        let waiter = std::thread::spawn(move || crop.wait(Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(20));
        assert!(store.remove(&id));
        assert_eq!(waiter.join().expect("join"), CropImage::Failed);
    }
}
