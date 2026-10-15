//! 来源应用图标：`SHDefExtractIconW` 按请求尺寸取 exe 图标（失败时退回 `SHGetFileInfoW` 的大图标），
//! 读出 32 位颜色位图（没有 alpha 的旧式图标用掩码位图补 alpha），编码为 PNG；按 (路径, 尺寸) 缓存。
//! 调用方在后台线程调用（不在关键路径上）。

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Arc;

use ::windows::Win32::Foundation::S_OK;
use ::windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits, GetObjectW, HBITMAP,
    HGDIOBJ, ReleaseDC,
};
use ::windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
use ::windows::Win32::UI::Shell::{SHDefExtractIconW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGetFileInfoW};
use ::windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};
use ::windows::core::PCWSTR;
use parking_lot::Mutex;

use super::clip_format::{Rgba, encode_png};
use super::util::{ComScope, wide};

/// 缓存条目上限（每个约 2–10 KB）
const CACHE_LIMIT: usize = 64;

/// (小写路径, 尺寸) → PNG（None 表示取不到，同样缓存，避免反复尝试）
type CacheMap = HashMap<(String, u32), Option<Arc<Vec<u8>>>>;

/// 图标缓存
pub(crate) struct IconCache {
    map: Mutex<CacheMap>,
}

impl IconCache {
    pub(crate) fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    /// exe 图标的 PNG（`size_px` 为期望边长，实际尺寸以系统给出的为准）
    pub(crate) fn png(&self, path: &str, size_px: u32) -> Option<Vec<u8>> {
        let size = size_px.clamp(16, 256);
        let key = (path.to_lowercase(), size);
        if let Some(hit) = self.map.lock().get(&key) {
            return hit.as_ref().map(|v| v.as_ref().clone());
        }
        let png = extract(path, size).map(Arc::new);
        let mut map = self.map.lock();
        if map.len() >= CACHE_LIMIT {
            map.clear();
        }
        map.insert(key, png.clone());
        png.map(|v| v.as_ref().clone())
    }
}

/// 图标句柄守卫
struct IconGuard(HICON);

impl Drop for IconGuard {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            // SAFETY: 由 SHDefExtractIconW / SHGetFileInfoW 创建、由本进程持有
            unsafe {
                let _ = DestroyIcon(self.0);
            }
        }
    }
}

fn extract(path: &str, size: u32) -> Option<Vec<u8>> {
    let _com = ComScope::apartment();
    let wpath = wide(path);
    let mut hicon = HICON::default();
    // SAFETY: 路径以 NUL 结尾；只请求大图标，尺寸放在低 16 位
    let hr = unsafe { SHDefExtractIconW(PCWSTR(wpath.as_ptr()), 0, 0, Some(&mut hicon), None, size & 0xFFFF) };
    if hr != S_OK || hicon.0.is_null() {
        let mut info = SHFILEINFOW::default();
        // SAFETY: 输出结构体大小如实传入；成功时 hIcon 归本进程所有
        let r = unsafe {
            SHGetFileInfoW(
                PCWSTR(wpath.as_ptr()),
                FILE_FLAGS_AND_ATTRIBUTES(0),
                Some(&mut info),
                std::mem::size_of::<SHFILEINFOW>() as u32,
                SHGFI_ICON | SHGFI_LARGEICON,
            )
        };
        if r == 0 || info.hIcon.0.is_null() {
            return None;
        }
        hicon = info.hIcon;
    }
    let icon = IconGuard(hicon);
    let img = icon_to_rgba(icon.0)?;
    encode_png(&img)
}

/// 位图句柄守卫
struct BitmapGuard(HBITMAP);

impl Drop for BitmapGuard {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            // SAFETY: GetIconInfo 返回的位图由调用方负责删除
            unsafe {
                let _ = DeleteObject(HGDIOBJ(self.0.0));
            }
        }
    }
}

fn icon_to_rgba(hicon: HICON) -> Option<Rgba> {
    let mut ii = ICONINFO::default();
    // SAFETY: 输出结构体指向局部变量
    unsafe { GetIconInfo(hicon, &mut ii) }.ok()?;
    let color = BitmapGuard(ii.hbmColor);
    let mask = BitmapGuard(ii.hbmMask);
    if color.0.0.is_null() {
        // 单色图标：极少见，放弃
        return None;
    }
    let mut bm = BITMAP::default();
    // SAFETY: 输出缓冲区是 BITMAP
    let n = unsafe {
        GetObjectW(
            HGDIOBJ(color.0.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut BITMAP as *mut c_void),
        )
    };
    if n == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
        return None;
    }
    let (w, h) = (bm.bmWidth, bm.bmHeight);
    let mut bgra = read_bits(color.0, w, h)?;
    let has_alpha = bgra.chunks_exact(4).any(|p| p[3] != 0);
    if !has_alpha {
        // 旧式图标：掩码位图中黑色（0）为不透明
        let m = read_bits(mask.0, w, h);
        for (i, px) in bgra.chunks_exact_mut(4).enumerate() {
            let transparent = m.as_ref().is_some_and(|m| m[i * 4] != 0);
            px[3] = if transparent { 0 } else { 255 };
        }
    }
    let pixels = bgra.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
    Some(Rgba {
        width: w as u32,
        height: h as u32,
        pixels,
    })
}

/// 以 32 位自上而下格式读取位图像素（BGRA）
fn read_bits(bitmap: HBITMAP, w: i32, h: i32) -> Option<Vec<u8>> {
    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
    // SAFETY: 屏幕 DC 用完释放；缓冲区大小与 bmi 描述一致
    let lines = unsafe {
        let hdc = GetDC(None);
        let lines = GetDIBits(
            hdc,
            bitmap,
            0,
            h as u32,
            Some(buf.as_mut_ptr() as *mut c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, hdc);
        lines
    };
    (lines == h).then_some(buf)
}
