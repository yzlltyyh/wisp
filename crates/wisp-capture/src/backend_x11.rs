//! X11 后端：x11rb 独立连接，优先 MIT-SHM（不可用时退回 GetImage），按 RandR 输出矩形逐屏截取，ZPixmap BGRX → RGBA。
//!
//! 只在调用期间持有连接与共享内存段，返回前全部释放（空闲时零资源）。纯 Rust 的 `RustConnection`，不链接 libxcb，
//! 也不依赖 pipewire / gbm / wayland。
//!
//! 硬超时：x11rb 的 `reply()` 在服务器被别的客户端 XGrabServer 或卡住时会无限阻塞，所以整个捕获在一个
//! 短命的专用线程里执行，调用方最多等 [`CAPTURE_TIMEOUT`]；超时就返回错误并放弃该线程（它持有的连接与共享内存段
//! 在服务器恢复、线程结束时随之释放）。
//!
//! Wayland 会话：`DISPLAY` 通常指向 XWayland，连接能成功，但对根窗口 GetImage 只能得到黑屏或只含 X11 客户端的画面。
//! 因此入口先识别会话类型，Wayland 下直接返回 `Unsupported`（改用 `portal_screenshot`），不交出一张黑色冻结帧。
//!
//! MIT-SHM 走 1.2 的 `CreateSegment`：由服务器创建内存段并把 fd 传过来，再用 `pread` 读出（不需要 libc 的
//! shmget / mmap）；本地连接下比 GetImage 少一次经由套接字的整屏拷贝。远程连接（ssh -X）或服务器不支持时自动退回 GetImage。
//!
//! 窗口列表：`_NET_CLIENT_LIST_STACKING`（自底向上，反转后最上层在前），矩形 = 客户区 + `_NET_FRAME_EXTENTS`
//! （服务端装饰）− `_GTK_FRAME_EXTENTS`（客户端装饰的阴影），排除自己的 `_NET_WM_PID`、桌面 / 停靠栏类型与最小化窗口。
//! 所有请求先全部发出再统一取回复（x11rb 的 cookie 流水线），几十个窗口也只需一次往返的延迟。

use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use wisp_types::MonitorInfo;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, ImageFormat, ImageOrder, MapState, Screen, Window};
use x11rb::rust_connection::RustConnection;

use crate::error::CaptureError;
use crate::frame::{CaptureSet, Frame, WindowRect};
use crate::monitors::{self, RECT_TOLERANCE};
use crate::windows_list::{MonitorSpace, RawWindow, assign_to_monitors};
use crate::zpixmap::{self, PixelFormat};

/// 小于它（物理 px）的窗口不参与窗口检测
const MIN_WINDOW_PX: f64 = 8.0;
/// 整个捕获（连接、截图、窗口列表）的硬超时。本地 MIT-SHM 通常几十毫秒；留出余量给 ssh -X 等远程连接下的 GetImage
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        _NET_CLIENT_LIST_STACKING,
        _NET_CLIENT_LIST,
        _NET_WM_PID,
        _NET_FRAME_EXTENTS,
        _GTK_FRAME_EXTENTS,
        _NET_WM_WINDOW_TYPE,
        _NET_WM_WINDOW_TYPE_DESKTOP,
        _NET_WM_WINDOW_TYPE_DOCK,
        _NET_WM_STATE,
        _NET_WM_STATE_HIDDEN,
    }
}

/// RandR 输出（已启用、有 CRTC 的）在根窗口中的物理矩形
#[derive(Debug, Clone)]
struct Output {
    name: String,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
}

/// 调用期间的服务器端共享内存段：`capture_all` 返回前 detach，fd 随 `file` 关闭，服务器随之释放内存
struct ShmSegment {
    seg: shm::Seg,
    file: File,
    size: usize,
}

fn backend_err(context: &str, e: impl std::fmt::Display) -> CaptureError {
    CaptureError::Backend(format!("X11 {context}: {e}"))
}

/// 阻塞地捕获所有显示器（最多等 [`CAPTURE_TIMEOUT`]）。Wayland 会话返回 `Unsupported`
pub(crate) fn capture_all(monitors: &[MonitorInfo], exclude_pid: u32) -> Result<CaptureSet, CaptureError> {
    if monitors.is_empty() {
        return Err(CaptureError::NoDisplays);
    }
    let session_type = std::env::var_os("XDG_SESSION_TYPE");
    let wayland_display = std::env::var_os("WAYLAND_DISPLAY");
    if is_wayland_session(session_type.as_deref(), wayland_display.as_deref()) {
        return Err(CaptureError::Unsupported(
            "wayland session: use portal_screenshot".to_owned(),
        ));
    }

    let monitors = monitors.to_vec();
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("wisp-capture-x11".to_owned())
        .spawn(move || {
            let _ = tx.send(capture_blocking(&monitors, exclude_pid));
        })
        .map_err(|e| backend_err("spawn capture thread", e))?;
    match rx.recv_timeout(CAPTURE_TIMEOUT) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => {
            log::error!(
                "X11 截图 {} s 内没有完成（X 服务器无响应或被其他客户端独占），放弃",
                CAPTURE_TIMEOUT.as_secs()
            );
            Err(CaptureError::Backend("X11 capture timed out".to_owned()))
        }
        Err(RecvTimeoutError::Disconnected) => Err(CaptureError::Backend("X11 capture thread panicked".to_owned())),
    }
}

/// 是否 Wayland 会话：`XDG_SESSION_TYPE` 明确时以它为准；没有（或为 tty 等）时看 `WAYLAND_DISPLAY` 是否非空
fn is_wayland_session(session_type: Option<&OsStr>, wayland_display: Option<&OsStr>) -> bool {
    match session_type
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("wayland") => true,
        Some("x11") => false,
        _ => wayland_display.is_some_and(|d| !d.is_empty()),
    }
}

/// 捕获本体（在 [`capture_all`] 的专用线程里执行，可能无限阻塞在 X 服务器上）
fn capture_blocking(monitors: &[MonitorInfo], exclude_pid: u32) -> Result<CaptureSet, CaptureError> {
    let started = Instant::now();
    let (conn, screen_num) = x11rb::connect(None).map_err(|e| {
        CaptureError::Unsupported(format!("cannot connect to the X server (is this an X11 session?): {e}"))
    })?;
    let screen = conn
        .setup()
        .roots
        .get(screen_num)
        .cloned()
        .ok_or_else(|| CaptureError::Backend("X11 screen not found".to_owned()))?;
    let root = screen.root;
    let format = pixel_format(&conn, &screen)?;
    // 原子与输出信息的请求先发出，与下面的配对、截图交错
    let atoms_cookie = Atoms::new(&conn).map_err(|e| backend_err("intern atoms", e))?;

    let outputs = list_outputs(&conn, &screen);
    let paired = monitors::pair(
        monitors,
        outputs,
        |m, o| m.id == o.name,
        |m, o| {
            monitors::rect_close(
                monitors::physical_rect(m),
                (f64::from(o.x), f64::from(o.y), f64::from(o.width), f64::from(o.height)),
                RECT_TOLERANCE,
            )
        },
    );
    if paired.is_empty() {
        return Err(CaptureError::NoDisplays);
    }

    let max_bytes = paired
        .iter()
        .map(|(_, o)| zpixmap::stride(u32::from(o.width), format.0.bits_per_pixel, format.1) * usize::from(o.height))
        .max()
        .unwrap_or(0);
    let shm = create_shm(&conn, max_bytes);

    // 逐屏取像素（同一连接上服务器本来就串行处理），每取完一屏就在独立线程里转换，与下一屏的读取重叠
    let mut frames = Vec::with_capacity(paired.len());
    let mut first_error = None;
    std::thread::scope(|scope| {
        let mut converting = Vec::with_capacity(paired.len());
        for (monitor, output) in &paired {
            match grab(&conn, root, output, format, shm.as_ref()) {
                Ok((raw, stride)) => {
                    let (w, h) = (u32::from(output.width), u32::from(output.height));
                    let fmt = format.0;
                    converting.push((
                        monitor,
                        w,
                        h,
                        scope.spawn(move || zpixmap::to_rgba(&raw, w, h, stride, fmt)),
                    ));
                }
                Err(e) => {
                    log::warn!("显示器 {} 截图失败：{e}", monitor.id);
                    first_error.get_or_insert(e);
                }
            }
        }
        for (monitor, w, h, handle) in converting {
            match handle.join() {
                Ok(rgba) => frames.push(Frame::from_rgba(monitor.clone(), w, h, rgba)),
                Err(_) => {
                    first_error.get_or_insert(CaptureError::Backend("pixel conversion panicked".to_owned()));
                }
            }
        }
    });
    if let Some(shm) = &shm {
        let _ = conn.shm_detach(shm.seg);
    }

    let windows = match atoms_cookie.reply() {
        Ok(atoms) => {
            let spaces: Vec<MonitorSpace> = paired
                .iter()
                .map(|(m, o)| MonitorSpace {
                    id: m.id.clone(),
                    x: f64::from(o.x),
                    y: f64::from(o.y),
                    width: f64::from(o.width),
                    height: f64::from(o.height),
                    unit_to_logical: 1.0 / valid_scale(m.scale_factor),
                })
                .collect();
            list_windows(&conn, root, &atoms, &spaces, exclude_pid)
        }
        Err(e) => {
            log::warn!("X11 原子查询失败，窗口检测不可用：{e}");
            Vec::new()
        }
    };
    let _ = conn.flush();

    if frames.is_empty() {
        return Err(first_error.unwrap_or(CaptureError::NoDisplays));
    }
    log::debug!(
        "X11 捕获 {} 个显示器（{}）、{} 个窗口矩形，用时 {} ms",
        frames.len(),
        if shm.is_some() { "MIT-SHM" } else { "GetImage" },
        windows.len(),
        started.elapsed().as_millis()
    );
    Ok(CaptureSet { frames, windows })
}

fn valid_scale(s: f64) -> f64 {
    if s.is_finite() && s > 0.0 { s } else { 1.0 }
}

/// 根窗口的 ZPixmap 像素格式与 scanline_pad
fn pixel_format(conn: &RustConnection, screen: &Screen) -> Result<(PixelFormat, u8), CaptureError> {
    let setup = conn.setup();
    let depth = screen.root_depth;
    let pixmap = setup
        .pixmap_formats
        .iter()
        .find(|f| f.depth == depth)
        .ok_or_else(|| CaptureError::Unsupported(format!("no pixmap format for depth {depth}")))?;
    let visual = screen
        .allowed_depths
        .iter()
        .flat_map(|d| d.visuals.iter())
        .find(|v| v.visual_id == screen.root_visual)
        .ok_or_else(|| CaptureError::Unsupported("root visual not found".to_owned()))?;
    let fmt = PixelFormat {
        bits_per_pixel: pixmap.bits_per_pixel,
        msb_first: setup.image_byte_order == ImageOrder::MSB_FIRST,
        red_mask: visual.red_mask,
        green_mask: visual.green_mask,
        blue_mask: visual.blue_mask,
    };
    if !fmt.supported() {
        return Err(CaptureError::Unsupported(format!(
            "unsupported X11 pixel format: depth {depth}, {} bpp",
            pixmap.bits_per_pixel
        )));
    }
    Ok((fmt, pixmap.scanline_pad))
}

/// 枚举已连接、已启用的 RandR 输出；没有 RandR（或查询失败）时把整个根窗口当作一个输出
fn list_outputs(conn: &RustConnection, screen: &Screen) -> Vec<Output> {
    let whole = || {
        vec![Output {
            name: "default".to_owned(),
            x: 0,
            y: 0,
            width: screen.width_in_pixels,
            height: screen.height_in_pixels,
        }]
    };
    let has_randr = conn
        .extension_information(randr::X11_EXTENSION_NAME)
        .ok()
        .flatten()
        .is_some();
    if !has_randr {
        return whole();
    }
    let resources = conn
        .randr_query_version(1, 3)
        .ok()
        .and_then(|c| c.reply().ok())
        .and_then(|_| conn.randr_get_screen_resources_current(screen.root).ok())
        .and_then(|c| c.reply().ok());
    let Some(res) = resources else {
        return whole();
    };
    let ts = res.config_timestamp;
    let infos: Vec<_> = res
        .outputs
        .iter()
        .filter_map(|&o| conn.randr_get_output_info(o, ts).ok())
        .collect();
    let infos: Vec<_> = infos
        .into_iter()
        .filter_map(|c| c.reply().ok())
        .filter(|i| i.crtc != 0 && i.connection == randr::Connection::CONNECTED)
        .collect();
    let crtcs: Vec<_> = infos
        .iter()
        .map(|i| conn.randr_get_crtc_info(i.crtc, ts).ok())
        .collect();
    let outputs: Vec<Output> = infos
        .iter()
        .zip(crtcs)
        .filter_map(|(info, cookie)| {
            let crtc = cookie?.reply().ok()?;
            (crtc.width > 0 && crtc.height > 0).then(|| Output {
                name: String::from_utf8_lossy(&info.name).into_owned(),
                x: crtc.x,
                y: crtc.y,
                width: crtc.width,
                height: crtc.height,
            })
        })
        .collect();
    if outputs.is_empty() { whole() } else { outputs }
}

/// 建立 MIT-SHM 1.2 的服务器端内存段；不可用时返回 None（退回 GetImage）
fn create_shm(conn: &RustConnection, size: usize) -> Option<ShmSegment> {
    let size32 = u32::try_from(size).ok().filter(|&s| s > 0)?;
    conn.extension_information(shm::X11_EXTENSION_NAME).ok().flatten()?;
    let version = conn.shm_query_version().ok()?.reply().ok()?;
    if (version.major_version, version.minor_version) < (1, 2) {
        log::debug!(
            "MIT-SHM {}.{} 不支持 CreateSegment，使用 GetImage",
            version.major_version,
            version.minor_version
        );
        return None;
    }
    let seg = conn.generate_id().ok()?;
    match conn.shm_create_segment(seg, size32, false).map(|c| c.reply()) {
        Ok(Ok(reply)) => Some(ShmSegment {
            seg,
            file: File::from(reply.shm_fd),
            size,
        }),
        Ok(Err(e)) => {
            log::debug!("MIT-SHM CreateSegment 失败（可能是远程连接），使用 GetImage：{e}");
            None
        }
        Err(e) => {
            log::debug!("MIT-SHM CreateSegment 请求失败，使用 GetImage：{e}");
            None
        }
    }
}

/// 取一个输出的原始 ZPixmap 数据与行跨度：优先共享内存，失败时退回 GetImage
fn grab(
    conn: &RustConnection,
    root: Window,
    out: &Output,
    format: (PixelFormat, u8),
    shm: Option<&ShmSegment>,
) -> Result<(Vec<u8>, usize), CaptureError> {
    let stride = zpixmap::stride(u32::from(out.width), format.0.bits_per_pixel, format.1);
    let expected = stride * usize::from(out.height);
    if let Some(shm) = shm {
        match grab_shm(conn, root, out, shm, expected) {
            Ok(raw) => return Ok((raw, stride)),
            Err(e) => log::debug!("MIT-SHM 截图失败，改用 GetImage：{e}"),
        }
    }
    let reply = conn
        .get_image(ImageFormat::Z_PIXMAP, root, out.x, out.y, out.width, out.height, !0)
        .map_err(|e| backend_err("GetImage", e))?
        .reply()
        .map_err(|e| backend_err("GetImage", e))?;
    if reply.data.len() < expected {
        return Err(CaptureError::Backend(format!(
            "X11 GetImage returned {} bytes, expected {expected}",
            reply.data.len()
        )));
    }
    Ok((reply.data, stride))
}

fn grab_shm(
    conn: &RustConnection,
    root: Window,
    out: &Output,
    shm: &ShmSegment,
    expected: usize,
) -> Result<Vec<u8>, CaptureError> {
    if expected > shm.size {
        return Err(CaptureError::Backend("shm segment too small".to_owned()));
    }
    let reply = conn
        .shm_get_image(
            root,
            out.x,
            out.y,
            out.width,
            out.height,
            !0,
            u8::from(ImageFormat::Z_PIXMAP),
            shm.seg,
            0,
        )
        .map_err(|e| backend_err("ShmGetImage", e))?
        .reply()
        .map_err(|e| backend_err("ShmGetImage", e))?;
    let size = (reply.size as usize).min(shm.size);
    if size < expected {
        return Err(CaptureError::Backend(format!(
            "ShmGetImage wrote {size} bytes, expected {expected}"
        )));
    }
    let mut raw = vec![0u8; expected];
    shm.file
        .read_exact_at(&mut raw, 0)
        .map_err(|e| backend_err("read shm segment", e))?;
    Ok(raw)
}

/// 一个客户窗口的全部待取回复（流水线：先全部发出，再依次取）
struct WindowCookies<'c> {
    attrs: Option<x11rb::cookie::Cookie<'c, RustConnection, x11rb::protocol::xproto::GetWindowAttributesReply>>,
    geometry: Option<x11rb::cookie::Cookie<'c, RustConnection, x11rb::protocol::xproto::GetGeometryReply>>,
    origin: Option<x11rb::cookie::Cookie<'c, RustConnection, x11rb::protocol::xproto::TranslateCoordinatesReply>>,
    pid: Option<PropertyCookie<'c>>,
    frame: Option<PropertyCookie<'c>>,
    gtk_frame: Option<PropertyCookie<'c>>,
    kind: Option<PropertyCookie<'c>>,
    state: Option<PropertyCookie<'c>>,
    class: Option<PropertyCookie<'c>>,
}

type PropertyCookie<'c> = x11rb::cookie::Cookie<'c, RustConnection, x11rb::protocol::xproto::GetPropertyReply>;

fn property<'c>(
    conn: &'c RustConnection,
    window: Window,
    atom: u32,
    kind: impl Into<u32>,
    len: u32,
) -> Option<PropertyCookie<'c>> {
    conn.get_property(false, window, atom, kind.into(), 0, len).ok()
}

fn cardinals(cookie: Option<PropertyCookie<'_>>) -> Vec<u32> {
    cookie
        .and_then(|c| c.reply().ok())
        .and_then(|r| r.value32().map(Iterator::collect))
        .unwrap_or_default()
}

/// 可见的顶层窗口，按 z 序最上层在前，切分到各显示器
fn list_windows(
    conn: &RustConnection,
    root: Window,
    atoms: &Atoms,
    spaces: &[MonitorSpace],
    exclude_pid: u32,
) -> Vec<WindowRect> {
    let list = |atom| cardinals(property(conn, root, atom, AtomEnum::WINDOW, 4096));
    let mut clients = list(atoms._NET_CLIENT_LIST_STACKING);
    if clients.is_empty() {
        // 没有 EWMH 的堆叠序时退回映射序（不一定是 z 序，但比没有窗口检测强）
        clients = list(atoms._NET_CLIENT_LIST);
    }
    // 自底向上 → 最上层在前
    clients.reverse();

    let pending: Vec<WindowCookies<'_>> = clients
        .iter()
        .map(|&w| WindowCookies {
            attrs: conn.get_window_attributes(w).ok(),
            geometry: conn.get_geometry(w).ok(),
            origin: conn.translate_coordinates(w, root, 0, 0).ok(),
            pid: property(conn, w, atoms._NET_WM_PID, AtomEnum::CARDINAL, 1),
            frame: property(conn, w, atoms._NET_FRAME_EXTENTS, AtomEnum::CARDINAL, 4),
            gtk_frame: property(conn, w, atoms._GTK_FRAME_EXTENTS, AtomEnum::CARDINAL, 4),
            kind: property(conn, w, atoms._NET_WM_WINDOW_TYPE, AtomEnum::ATOM, 16),
            state: property(conn, w, atoms._NET_WM_STATE, AtomEnum::ATOM, 32),
            class: property(conn, w, AtomEnum::WM_CLASS.into(), AtomEnum::STRING, 256),
        })
        .collect();

    let mut raws = Vec::with_capacity(pending.len());
    for c in pending {
        let viewable = c
            .attrs
            .and_then(|c| c.reply().ok())
            .is_some_and(|a| a.map_state == MapState::VIEWABLE);
        let geometry = c.geometry.and_then(|c| c.reply().ok());
        let origin = c.origin.and_then(|c| c.reply().ok());
        let pid = cardinals(c.pid);
        let frame = cardinals(c.frame);
        let gtk = cardinals(c.gtk_frame);
        let kinds = cardinals(c.kind);
        let states = cardinals(c.state);
        let class = c
            .class
            .and_then(|c| c.reply().ok())
            .map(|r| r.value)
            .unwrap_or_default();

        let (Some(geometry), Some(origin)) = (geometry, origin) else {
            continue;
        };
        if !viewable
            || pid.first() == Some(&exclude_pid)
            || kinds
                .iter()
                .any(|&k| k == atoms._NET_WM_WINDOW_TYPE_DESKTOP || k == atoms._NET_WM_WINDOW_TYPE_DOCK)
            || states.contains(&atoms._NET_WM_STATE_HIDDEN)
        {
            continue;
        }
        let rect = visible_frame(
            (
                f64::from(origin.dst_x),
                f64::from(origin.dst_y),
                f64::from(geometry.width),
                f64::from(geometry.height),
            ),
            &frame,
            &gtk,
        );
        if rect.2 < MIN_WINDOW_PX || rect.3 < MIN_WINDOW_PX {
            continue;
        }
        raws.push(RawWindow {
            x: rect.0,
            y: rect.1,
            width: rect.2,
            height: rect.3,
            app_name: class_name(&class),
        });
    }
    assign_to_monitors(&raws, spaces)
}

/// 可见边框：客户区加上服务端装饰（`_NET_FRAME_EXTENTS`：左、右、上、下），再减去客户端装饰的阴影
/// （`_GTK_FRAME_EXTENTS`：左、右、上、下）
fn visible_frame(client: (f64, f64, f64, f64), frame: &[u32], gtk_shadow: &[u32]) -> (f64, f64, f64, f64) {
    let (mut x, mut y, mut w, mut h) = client;
    if let [l, r, t, b] = frame {
        let (l, r, t, b) = (f64::from(*l), f64::from(*r), f64::from(*t), f64::from(*b));
        x -= l;
        y -= t;
        w += l + r;
        h += t + b;
    }
    if let [l, r, t, b] = gtk_shadow {
        let (l, r, t, b) = (f64::from(*l), f64::from(*r), f64::from(*t), f64::from(*b));
        x += l;
        y += t;
        w = (w - l - r).max(0.0);
        h = (h - t - b).max(0.0);
    }
    (x, y, w, h)
}

/// WM_CLASS 是「实例名\0类名\0」，取类名（通常是应用名，如 `Firefox`）；只有实例名时用实例名
fn class_name(raw: &[u8]) -> String {
    let mut parts = raw.split(|&b| b == 0).filter(|p| !p.is_empty());
    let instance = parts.next();
    let class = parts.next();
    class
        .or(instance)
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .unwrap_or_default()
}

/// 冒烟测试用：按 RandR 输出构造 `MonitorInfo`（X11 下缩放比按 1 处理，与没有设置 Xft.dpi 的桌面一致）
#[cfg(test)]
pub(crate) fn system_monitors() -> Vec<MonitorInfo> {
    use wisp_types::{PhysPoint, PhysSize, Rect};

    let Ok((conn, screen_num)) = x11rb::connect(None) else {
        return Vec::new();
    };
    let Some(screen) = conn.setup().roots.get(screen_num).cloned() else {
        return Vec::new();
    };
    list_outputs(&conn, &screen)
        .into_iter()
        .enumerate()
        .map(|(i, o)| {
            let (w, h) = (f64::from(o.width), f64::from(o.height));
            MonitorInfo {
                id: o.name.clone(),
                name: o.name,
                physical_origin: PhysPoint {
                    x: i32::from(o.x),
                    y: i32::from(o.y),
                },
                physical_size: PhysSize {
                    width: u32::from(o.width),
                    height: u32::from(o.height),
                },
                scale_factor: 1.0,
                frame: Rect::new(f64::from(o.x), f64::from(o.y), w, h),
                work_area: Rect::new(0.0, 0.0, w, h),
                is_primary: i == 0,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_extents_and_csd_shadow() {
        // 服务端装饰：左右 1、上 30、下 1
        assert_eq!(
            visible_frame((100.0, 100.0, 800.0, 600.0), &[1, 1, 30, 1], &[]),
            (99.0, 70.0, 802.0, 631.0)
        );
        // 客户端装饰：四周 20 px 阴影
        assert_eq!(
            visible_frame((100.0, 100.0, 840.0, 640.0), &[], &[20, 20, 20, 20]),
            (120.0, 120.0, 800.0, 600.0)
        );
        // 属性缺失或长度不对时不调整
        assert_eq!(
            visible_frame((0.0, 0.0, 10.0, 10.0), &[1, 2], &[]),
            (0.0, 0.0, 10.0, 10.0)
        );
    }

    #[test]
    fn wayland_sessions_are_detected() {
        let os = |s: &'static str| Some(OsStr::new(s));
        assert!(is_wayland_session(os("wayland"), os("wayland-0")));
        assert!(is_wayland_session(os("wayland"), None), "会话类型明确为 wayland");
        assert!(is_wayland_session(os("Wayland"), None), "大小写不敏感");
        assert!(
            is_wayland_session(None, os("wayland-0")),
            "没有会话类型时看 WAYLAND_DISPLAY"
        );
        assert!(is_wayland_session(os("tty"), os("wayland-1")), "从 tty 启动的合成器");
        assert!(
            !is_wayland_session(os("x11"), os("wayland-0")),
            "明确的 X11 会话里残留的 WAYLAND_DISPLAY 不算"
        );
        assert!(!is_wayland_session(os("x11"), None));
        assert!(!is_wayland_session(None, os("")), "空的 WAYLAND_DISPLAY");
        assert!(!is_wayland_session(None, None));
    }

    #[test]
    fn wm_class_prefers_class_part() {
        assert_eq!(class_name(b"navigator\0Firefox\0"), "Firefox");
        assert_eq!(class_name(b"xterm\0"), "xterm");
        assert_eq!(class_name(b""), "");
    }
}
