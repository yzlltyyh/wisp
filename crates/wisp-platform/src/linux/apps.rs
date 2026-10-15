//! 应用信息（剪贴板来源图标、快捷键冲突的邻居探测）：显示名、图标、正在运行的应用。
//!
//! 图标查找顺序：`.desktop` 的 `Icon=` 在当前图标主题（沿 Inherits 链）与 hicolor 中的 PNG → 窗口的
//! `_NET_WM_ICON`（ARGB 数组）→ None（界面显示通用图标）。比目标尺寸大得多的图标会缩小后重新编码，
//! 缓存文件（blobs/icons）保持小巧。全部是一次性的文件 / X 请求，由调用方放在工作线程上异步执行。
//!
//! 已知限制：没有 SVG 光栅化器（工作区没有 resvg），只提供 scalable SVG 的应用（多数 `org.gnome.*` 核心应用）
//! 只能退回窗口图标；没有窗口的条目（托盘常驻进程、已关闭窗口的历史来源）显示通用图标。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use wisp_types::AppRef;
use x11rb::protocol::xproto::AtomEnum;

use super::desktop_entry::{self, DesktopEntry, DesktopIndex, Recheck};
use super::x11::{X11, focus, props};

/// 两次检查 `.desktop` 目录是否变化的最小间隔
const STALE_CHECK: Duration = Duration::from_secs(10);
/// 解码图标时的内存上限
const DECODE_LIMIT: usize = 16 * 1024 * 1024;

/// 应用信息服务
pub(crate) struct Apps {
    data_dirs: Vec<PathBuf>,
    locales: Vec<String>,
    /// 索引与「上次检查是否过期」的节流
    index: Mutex<Option<(Recheck, Arc<DesktopIndex>)>>,
}

impl Apps {
    pub(crate) fn new() -> Self {
        Self {
            data_dirs: desktop_entry::data_dirs(),
            locales: desktop_entry::process_locales(),
            index: Mutex::new(None),
        }
    }

    fn index(&self) -> Arc<DesktopIndex> {
        let mut guard = self.index.lock();
        // 最多每 10 s 真正检查一次应用目录是否变化；没到时间、或检查后没有变化，都直接用缓存
        if let Some((gate, idx)) = guard.as_mut()
            && (!gate.due(Instant::now(), STALE_CHECK) || !idx.is_stale())
        {
            return Arc::clone(idx);
        }
        let started = Instant::now();
        let idx = Arc::new(DesktopIndex::build(&self.data_dirs, &self.locales));
        log::debug!(
            ".desktop 索引：{} 个条目（{} ms）",
            idx.len(),
            started.elapsed().as_millis()
        );
        *guard = Some((Recheck::new(Instant::now()), Arc::clone(&idx)));
        idx
    }

    fn entry_for(&self, app: &AppRef) -> Option<DesktopEntry> {
        let exe = app.path.as_deref().and_then(|p| p.rsplit('/').next());
        let idx = self.index();
        idx.find(Some(app.id.as_str()), app.alt_id.as_deref(), exe).cloned()
    }

    /// 本地化的应用名；找不到 `.desktop` 时用 AppRef 自带的名称
    pub(crate) fn display_name(&self, app: &AppRef) -> String {
        self.entry_for(app)
            .map(|e| e.localized_name(&self.locales).to_owned())
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| app.name.clone())
    }

    /// 应用图标（PNG，边长不超过 `size_px`）
    pub(crate) fn icon_png(
        &self,
        app: &AppRef,
        size_px: u32,
        theme: Option<&str>,
        x11: Option<&X11>,
    ) -> Option<Vec<u8>> {
        let size = size_px.clamp(16, 512);
        if let Some(icon) = self.entry_for(app).and_then(|e| e.icon)
            && let Some(path) = desktop_entry::find_icon_png(&icon, size, theme, &self.data_dirs)
            && let Ok(bytes) = std::fs::read(&path)
            && let Some(png) = fit_png(&bytes, size)
        {
            return Some(png);
        }
        let x = x11?;
        let window = app.window.and_then(|w| u32::try_from(w).ok())?;
        window_icon(x, window, size)
    }

    /// 正在运行的应用：图形客户端（X11）+ 有 `.desktop` 条目的本用户进程（托盘常驻、无窗口的应用也能找到）
    pub(crate) fn running_apps(&self, x11: Option<&X11>, own_pid: u32) -> Vec<AppRef> {
        let mut out: Vec<AppRef> = x11.map(focus::running_apps).unwrap_or_default();
        let idx = self.index();
        let Some(uid) = proc_uid("self") else {
            return out;
        };
        let Ok(rd) = std::fs::read_dir("/proc") else {
            return out;
        };
        for ent in rd.flatten() {
            let name = ent.file_name();
            let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            if pid == own_pid || out.iter().any(|a| a.pid == Some(pid)) || proc_uid(&pid.to_string()) != Some(uid) {
                continue;
            }
            let Some(exe) = focus::exe_path(pid) else { continue };
            let base = exe.rsplit('/').next().unwrap_or(&exe).to_owned();
            let Some(entry) = idx.find_by_exec(&base) else { continue };
            let id = entry
                .startup_wm_class
                .clone()
                .unwrap_or_else(|| base.clone())
                .to_ascii_lowercase();
            if out.iter().any(|a| a.id == id) {
                continue;
            }
            out.push(AppRef {
                id,
                alt_id: None,
                name: entry.localized_name(&self.locales).to_owned(),
                pid: Some(pid),
                path: Some(exe),
                window: None,
            });
        }
        out
    }
}

fn proc_uid(pid: &str) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(format!("/proc/{pid}")).ok().map(|m| m.uid())
}

/// 窗口的 `_NET_WM_ICON`：选择不小于目标的最小一张（没有就选最大的一张），转为 PNG
fn window_icon(x: &X11, window: u32, size: u32) -> Option<Vec<u8>> {
    let reply = props::get(x, window, x.atoms._NET_WM_ICON, AtomEnum::CARDINAL, 16 * 1024 * 1024)?;
    let data: Vec<u32> = reply.value32()?.collect();
    let mut best: Option<(u32, u32, usize)> = None;
    let mut i = 0usize;
    while i + 2 <= data.len() {
        let (w, h) = (data[i], data[i + 1]);
        let n = (w as usize).checked_mul(h as usize)?;
        if w == 0 || h == 0 || i + 2 + n > data.len() {
            break;
        }
        let better = match best {
            None => true,
            // 不小于目标的里取最小的；都小于目标时取最大的
            Some((bw, _, _)) => match (w >= size, bw >= size) {
                (true, true) => w < bw,
                (true, false) => true,
                (false, true) => false,
                (false, false) => w > bw,
            },
        };
        if better {
            best = Some((w, h, i + 2));
        }
        i += 2 + n;
    }
    let (w, h, start) = best?;
    let n = (w * h) as usize;
    let mut rgba = Vec::with_capacity(n * 4);
    for &argb in &data[start..start + n] {
        rgba.extend([(argb >> 16) as u8, (argb >> 8) as u8, argb as u8, (argb >> 24) as u8]);
    }
    let (w, h, rgba) = downscale(w, h, rgba, size);
    encode_png(w, h, &rgba)
}

/// 图标已经足够小就原样返回；明显偏大时解码、缩小、重新编码
fn fit_png(bytes: &[u8], size: u32) -> Option<Vec<u8>> {
    let (w, h) = png_dims(bytes)?;
    if w.max(h) <= size + size / 4 {
        return Some(bytes.to_vec());
    }
    let (w, h, rgba) = decode_rgba(bytes)?;
    let (w, h, rgba) = downscale(w, h, rgba, size);
    encode_png(w, h, &rgba)
}

fn png_dims(bytes: &[u8]) -> Option<(u32, u32)> {
    super::x11::clipboard::png_size(bytes).map(|s| (s.width, s.height))
}

fn decode_rgba(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut dec = png::Decoder::new_with_limits(std::io::Cursor::new(bytes), png::Limits { bytes: DECODE_LIMIT });
    dec.set_transformations(
        png::Transformations::EXPAND | png::Transformations::STRIP_16 | png::Transformations::ALPHA,
    );
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    let px = (w as usize) * (h as usize);
    let rgba = match info.color_type {
        png::ColorType::Rgba => {
            buf.truncate(px * 4);
            buf
        }
        png::ColorType::Rgb => buf[..px * 3]
            .chunks_exact(3)
            .flat_map(|c| [c[0], c[1], c[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf[..px * 2]
            .chunks_exact(2)
            .flat_map(|c| [c[0], c[0], c[0], c[1]])
            .collect(),
        png::ColorType::Grayscale => buf[..px].iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    (rgba.len() == px * 4).then_some((w, h, rgba))
}

/// 按面积平均缩小到边长不超过 `size`（在预乘空间里平均，避免半透明边缘发黑）
fn downscale(w: u32, h: u32, rgba: Vec<u8>, size: u32) -> (u32, u32, Vec<u8>) {
    let longest = w.max(h);
    if longest <= size || w == 0 || h == 0 {
        return (w, h, rgba);
    }
    let tw = ((u64::from(w) * u64::from(size)) / u64::from(longest)).max(1) as u32;
    let th = ((u64::from(h) * u64::from(size)) / u64::from(longest)).max(1) as u32;
    let mut out = Vec::with_capacity((tw * th * 4) as usize);
    for ty in 0..th {
        let y0 = (u64::from(ty) * u64::from(h) / u64::from(th)) as u32;
        let y1 = ((u64::from(ty + 1) * u64::from(h)).div_ceil(u64::from(th)) as u32).min(h);
        for tx in 0..tw {
            let x0 = (u64::from(tx) * u64::from(w) / u64::from(tw)) as u32;
            let x1 = ((u64::from(tx + 1) * u64::from(w)).div_ceil(u64::from(tw)) as u32).min(w);
            let (mut r, mut g, mut b, mut a, mut n) = (0u64, 0u64, 0u64, 0u64, 0u64);
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = ((y * w + x) * 4) as usize;
                    let al = u64::from(rgba[i + 3]);
                    r += u64::from(rgba[i]) * al;
                    g += u64::from(rgba[i + 1]) * al;
                    b += u64::from(rgba[i + 2]) * al;
                    a += al;
                    n += 1;
                }
            }
            if a == 0 || n == 0 {
                out.extend([0, 0, 0, 0]);
            } else {
                out.extend([(r / a) as u8, (g / a) as u8, (b / a) as u8, (a / n) as u8]);
            }
        }
    }
    (tw, th, out)
}

fn encode_png(w: u32, h: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Balanced);
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(rgba).ok()?;
        writer.finish().ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_roundtrip_and_downscale() {
        let (w, h) = (64u32, 32u32);
        let mut rgba = Vec::new();
        for y in 0..h {
            for x in 0..w {
                rgba.extend([x as u8 * 4, y as u8 * 8, 128, if x < 32 { 255 } else { 0 }]);
            }
        }
        let png = encode_png(w, h, &rgba).unwrap();
        assert_eq!(png_dims(&png), Some((64, 32)));
        let (dw, dh, back) = decode_rgba(&png).unwrap();
        assert_eq!((dw, dh), (64, 32));
        assert_eq!(back, rgba);
        let small = fit_png(&png, 16).unwrap();
        assert_eq!(png_dims(&small), Some((16, 8)));
        // 足够小的图标原样返回
        assert_eq!(fit_png(&png, 64).unwrap(), png);
    }

    #[test]
    fn downscale_ignores_transparent_color() {
        // 一半是不透明白色，一半是完全透明的黑色：缩成 1 像素后颜色仍是白色，alpha 约为一半
        let rgba = vec![255, 255, 255, 255, 0, 0, 0, 0];
        let (w, h, out) = downscale(2, 1, rgba, 1);
        assert_eq!((w, h), (1, 1));
        assert_eq!(&out[..3], &[255, 255, 255]);
        assert_eq!(out[3], 127);
    }
}
