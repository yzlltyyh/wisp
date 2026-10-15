//! `wisp://` 协议的资源注册表。
//!
//! 路径前缀约定（第一段即前缀，`fetch` 收到的是前缀之后的剩余部分）：
//! - `capture/<session>/<monitor>/{half,full}`：原始 RGBA，带响应头 `x-wisp-width`、`x-wisp-height`；
//!   `capture/<session>/<monitor>/region/<x>/<y>/<w>/<h>[/<max_px>]`：同一帧里的一块物理像素（原生冻结帧的放大镜小块、
//!   放大镜的低分辨率底图与飞入预览；面积超过 `max_px`（缺省 [`REGION_MAX_PX`]）时等比降采样）。
//!   `<session>` 是十进制的 `OverlaySession.session_id`；**`<monitor>` 不是 MonitorId 原文**，而是它 UTF-8 字节的
//!   小写十六进制（Windows 的 `\\.\DISPLAY1` 含反斜杠，原样放进路径会被下面的校验以 400 拒绝）。
//!   拼路径与解析一律用 [`capture_asset_path`] / [`parse_capture_path`]，不要手写
//! - `crop/<capture_id>.png`
//! - `img/<key>`、`thumb/<key>`（`.jpg` 或 `.png`）、`icon/<key>.png`、`ocrimg/<key>`
//!
//! 协议契约（页面源 `tauri://localhost` / `http://tauri.localhost` / dev 的 `http://localhost:1420` 与资源源
//! `wisp://localhost` / `http://wisp.localhost` 不同源）：所有响应统一带 `Cache-Control: no-store`、
//! `Access-Control-Allow-Origin: *`、`Access-Control-Expose-Headers: x-wisp-width, x-wisp-height`；`OPTIONS` 预检返回 204；
//! 前端用 `convertFileSrc(path, 'wisp')` 生成 URL，它会把整个 path 做 `encodeURIComponent`（`/` 变成 `%2F`），
//! 所以这里先对 path 做 percent-decode 再按前缀分发；资源不存在时返回 404。
//!
//! **防目录穿越集中在这里**（各 `AssetSource` 会把剩余路径映射成 blobs 目录下的文件，不能依赖各自校验）：
//! 解码后的路径去掉开头一个 `/` 后按 `/` 分段，任何一段为空（`a//b`、结尾 `/`、前缀后紧跟 `/`）、为 `.` / `..`，
//! 或含反斜杠、`:`（Windows 盘符 / 备用数据流）、NUL 等控制字符时一律 400，不会交给 `AssetSource`。

use std::sync::Arc;

use parking_lot::RwLock;
use tauri::http::{Method, Request, Response, StatusCode, header};

/// 一个资源。
///
/// `bytes` 与 `wisp_capture::Frame::rgba()` 用同一种容器（`Arc<Vec<u8>>`）：截图帧注册为资源时只克隆 Arc，不拷贝像素。
/// 响应时 Tauri 的 `UriSchemeResponder` 需要拥有所有权的 body：本 Asset 是这块内存的唯一持有者时
/// （例如从磁盘读出的 blob）直接移交、零拷贝；仍被别处共享时（截图帧仍由编排器持有）拷贝一次——这是自定义协议
/// 接口下的下限（5K 屏 RGBA 约 59 MB，每次请求最多一次整帧拷贝）。`AssetSource` 应每次 `fetch` 新建 Asset 并只克隆 Arc
pub struct Asset {
    pub bytes: Arc<Vec<u8>>,
    pub mime: &'static str,
    /// 额外响应头，例如 `("x-wisp-width", "3024")`
    pub extra_headers: Vec<(&'static str, String)>,
}

/// 资源来源（由持有数据的服务实现，例如 orchestrator 的截图帧、clipboard_svc 的 blob）
pub trait AssetSource: Send + Sync {
    /// `path` 为前缀之后的剩余部分（已 percent-decode，不含开头的 `/`，且已通过 [`split_path`] 的校验：
    /// 不含空段、`.` / `..`、反斜杠、`:` 与控制字符）；不存在时返回 None
    fn fetch(&self, path: &str) -> Option<Asset>;
}

/// 资源注册表：按路径第一段分发
#[derive(Default)]
pub struct AssetRegistry {
    sources: RwLock<Vec<(&'static str, Arc<dyn AssetSource>)>>,
}

/// 所有响应都带的 CORS 暴露头
const EXPOSE_HEADERS: &str = "x-wisp-width, x-wisp-height";

impl AssetRegistry {
    /// 空注册表
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册（或替换）某个前缀的资源来源；`prefix` 不含斜杠，例如 `"capture"`
    pub fn register(&self, prefix: &'static str, src: Arc<dyn AssetSource>) {
        debug_assert!(!prefix.contains('/'), "前缀不应包含斜杠");
        let mut sources = self.sources.write();
        sources.retain(|(p, _)| *p != prefix);
        sources.push((prefix, src));
    }

    /// 注销某个前缀
    pub fn unregister(&self, prefix: &str) {
        self.sources.write().retain(|(p, _)| *p != prefix);
    }

    /// 按已解码的完整路径（`<prefix>/<rest>`）取资源；路径未通过 [`split_path`] 的校验时同样返回 None
    pub fn fetch(&self, decoded_path: &str) -> Option<Asset> {
        let (prefix, rest) = split_path(decoded_path)?;
        self.fetch_split(prefix, rest)
    }

    fn fetch_split(&self, prefix: &str, rest: &str) -> Option<Asset> {
        let src = self
            .sources
            .read()
            .iter()
            .find(|(p, _)| *p == prefix)
            .map(|(_, s)| Arc::clone(s))?;
        src.fetch(rest)
    }

    /// 处理一个 `wisp://` 请求：OPTIONS 预检 → 204；GET → percent-decode 后按前缀分发；
    /// 非法转义或可疑路径（见 [`split_path`]）→ 400；不存在 → 404
    pub fn respond(&self, req: &Request<Vec<u8>>) -> Response<Vec<u8>> {
        let method = req.method();
        if method == Method::OPTIONS {
            return preflight_response();
        }
        if method != Method::GET && method != Method::HEAD {
            return empty_response(StatusCode::METHOD_NOT_ALLOWED);
        }
        let Some(path) = percent_decode(req.uri().path()) else {
            return empty_response(StatusCode::BAD_REQUEST);
        };
        let Some((prefix, rest)) = split_path(&path) else {
            log::warn!("wisp:// rejected suspicious path ({} bytes)", path.len());
            return empty_response(StatusCode::BAD_REQUEST);
        };
        match self.fetch_split(prefix, rest) {
            Some(asset) => asset_response(asset, method == Method::HEAD),
            None => empty_response(StatusCode::NOT_FOUND),
        }
    }
}

/// 校验并拆分已解码的资源路径：去掉开头一个 `/` 后得到 `(<prefix>, <rest>)`。
/// 任何一段为空（`a//b`、结尾 `/`、前缀后紧跟 `/`）、为 `.` / `..`，或含反斜杠、`:`、控制字符（含 NUL）时返回 None。
/// 只有前缀没有剩余部分（`"capture"`）时 rest 为空串
pub fn split_path(decoded_path: &str) -> Option<(&str, &str)> {
    let path = decoded_path.strip_prefix('/').unwrap_or(decoded_path);
    if path.is_empty() {
        return None;
    }
    let suspicious = |seg: &str| {
        seg.is_empty() || seg == "." || seg == ".." || seg.chars().any(|c| c == '\\' || c == ':' || c.is_control())
    };
    if path.split('/').any(suspicious) {
        return None;
    }
    Some(path.split_once('/').unwrap_or((path, "")))
}

/// `capture/` 资源的帧规格
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaptureVariant {
    /// 1/2 降采样帧（Windows / X11 先画它）
    Half,
    /// 全分辨率帧（Windows / X11 换上清晰帧与放大镜）
    Full,
    /// 帧里的一块物理像素：macOS 原生冻结帧时放大镜按需拉取的小块、低分辨率底图与飞入用的选区预览。
    /// 第二项是输出面积的上限（物理像素，可缺省的第 5 段；None = [`REGION_MAX_PX`]），更大的区域等比降采样到这以内。
    /// 路径前缀由 [`capture_region_base`] 生成，页面接上 `/<x>/<y>/<w>/<h>[/<max_px>]`
    Region(wisp_types::PhysRect, Option<u32>),
}

/// `region` 变体的面积上限（物理像素）：更大的区域等比降采样到这以内（约 16 MB RGBA；放大镜的小块远小于它，飞入的那块
/// 像素起飞后立即缩小，看不出差别——与原先页面里 `PIECE_MAX_PX` 的取值相同）
pub const REGION_MAX_PX: u64 = 4_000_000;

impl CaptureVariant {
    /// 路径里的写法（`region` 之后还有 4 段坐标与可缺省的面积上限）
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Half => "half",
            Self::Full => "full",
            Self::Region(..) => "region",
        }
    }
}

/// 生成冻结帧的资源路径 `capture/<session>/<monitor-hex>/{half,full}`（不带开头的 `/`，直接用作 `OverlayInit.image_url`
/// / `half_url`，前端经 `resolveAssetUrl` 转成 URL）。
///
/// `<monitor-hex>` 是 `monitor_id` 的 UTF-8 字节的小写十六进制：Windows 的 MonitorId `\\.\DISPLAY1` 含反斜杠，
/// macOS 是十进制、X11 是 `DP-1` 这类输出名，统一编码后只含 `[0-9a-f]`，三端形状一致且一定能通过 [`split_path`]
pub fn capture_asset_path(session_id: u64, monitor_id: &str, variant: CaptureVariant) -> String {
    use std::fmt::Write as _;
    let mut path = String::with_capacity(48 + monitor_id.len() * 2);
    let _ = write!(path, "capture/{session_id}/");
    for b in monitor_id.bytes() {
        let _ = write!(path, "{b:02x}");
    }
    path.push('/');
    path.push_str(variant.as_str());
    if let CaptureVariant::Region(r, max_px) = variant {
        let _ = write!(path, "/{}/{}/{}/{}", r.x, r.y, r.width, r.height);
        if let Some(max_px) = max_px {
            let _ = write!(path, "/{max_px}");
        }
    }
    path
}

/// `OverlayInit.region_url`：`capture/<session>/<monitor-hex>/region`。页面在后面接 `/<x>/<y>/<w>/<h>`
/// （十进制、本帧物理像素），要更小的输出时再接 `/<max_px>`，与 [`capture_asset_path`] 的 `Region` 变体逐字相同
pub fn capture_region_base(session_id: u64, monitor_id: &str) -> String {
    let mut path = capture_asset_path(session_id, monitor_id, CaptureVariant::Full);
    path.truncate(path.len() - "full".len());
    path.push_str("region");
    path
}

/// 解析 `capture` 来源收到的剩余路径 `<session>/<monitor-hex>/{half,full}`（[`capture_asset_path`] 的逆运算）；
/// 形状不对、十六进制非法或解码后不是 UTF-8 时返回 None
pub fn parse_capture_path(rest: &str) -> Option<(u64, wisp_types::MonitorId, CaptureVariant)> {
    let mut parts = rest.split('/');
    let (session, monitor_hex, variant) = (parts.next()?, parts.next()?, parts.next()?);
    if monitor_hex.is_empty() || monitor_hex.len() % 2 != 0 {
        return None;
    }
    // 只认 `capture_asset_path` 生成的规范写法（十进制无符号、无前导 + / 0 开头的多位数）
    let decimal = |s: &str| s.bytes().all(|b| b.is_ascii_digit()) && !s.is_empty() && (s == "0" || !s.starts_with('0'));
    if !session.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let session_id = session.parse().ok()?;
    let variant = match variant {
        "half" => CaptureVariant::Half,
        "full" => CaptureVariant::Full,
        "region" => {
            let mut num = || parts.next().filter(|s| decimal(s)).and_then(|s| s.parse::<u32>().ok());
            let (x, y, width, height) = (num()?, num()?, num()?, num()?);
            if width == 0 || height == 0 {
                return None;
            }
            // 可缺省的第 5 段：输出面积上限，1..=REGION_MAX_PX（写成上限本身也接受；0、更大与非规范写法拒绝）
            let max_px = match parts.next() {
                None => None,
                Some(s) => Some(
                    Some(s)
                        .filter(|s| decimal(s))
                        .and_then(|s| s.parse::<u32>().ok())
                        .filter(|&m| m > 0 && u64::from(m) <= REGION_MAX_PX)?,
                ),
            };
            CaptureVariant::Region(wisp_types::PhysRect { x, y, width, height }, max_px)
        }
        _ => return None,
    };
    if parts.next().is_some() {
        return None;
    }
    let lower_hex = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    let bytes = monitor_hex
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| Some((lower_hex(pair[0])? << 4) | lower_hex(pair[1])?))
        .collect::<Option<Vec<u8>>>()?;
    Some((session_id, String::from_utf8(bytes).ok()?, variant))
}

/// 注册表尚不可用（setup 完成之前）时的响应
pub fn unavailable_response() -> Response<Vec<u8>> {
    empty_response(StatusCode::SERVICE_UNAVAILABLE)
}

fn base_builder(status: StatusCode) -> tauri::http::response::Builder {
    Response::builder()
        .status(status)
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::ACCESS_CONTROL_EXPOSE_HEADERS, EXPOSE_HEADERS)
}

fn finish(builder: tauri::http::response::Builder, body: Vec<u8>) -> Response<Vec<u8>> {
    builder.body(body).unwrap_or_else(|e| {
        log::error!("wisp:// response build failed: {e}");
        let mut resp = Response::new(Vec::new());
        *resp.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
        resp
    })
}

fn empty_response(status: StatusCode) -> Response<Vec<u8>> {
    finish(base_builder(status), Vec::new())
}

fn preflight_response() -> Response<Vec<u8>> {
    let builder = base_builder(StatusCode::NO_CONTENT)
        .header(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, HEAD, OPTIONS")
        .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "*")
        .header(header::ACCESS_CONTROL_MAX_AGE, "600");
    finish(builder, Vec::new())
}

fn asset_response(asset: Asset, head_only: bool) -> Response<Vec<u8>> {
    let mut builder = base_builder(StatusCode::OK)
        .header(header::CONTENT_TYPE, asset.mime)
        .header(header::CONTENT_LENGTH, asset.bytes.len());
    for (name, value) in &asset.extra_headers {
        builder = builder.header(*name, value.as_str());
    }
    let body = if head_only {
        Vec::new()
    } else {
        // 唯一持有者：直接取出 Vec（零拷贝）；否则拷贝一次
        Arc::try_unwrap(asset.bytes).unwrap_or_else(|shared| shared.as_ref().clone())
    };
    finish(builder, body)
}

/// 对 URL path 做 percent-decode（`%2F` → `/`）；出现非法转义或结果不是 UTF-8 时返回 None。
/// `+` 不视为空格（这是 path，不是 query）
pub fn percent_decode(s: &str) -> Option<String> {
    fn hex(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = hex(*bytes.get(i + 1)?)?;
            let lo = hex(*bytes.get(i + 2)?)?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed;

    impl AssetSource for Fixed {
        fn fetch(&self, path: &str) -> Option<Asset> {
            (path == "3/1/full").then(|| Asset {
                bytes: Arc::new(b"\x01\x02\x03\x04".to_vec()),
                mime: "application/octet-stream",
                extra_headers: vec![("x-wisp-width", "1".to_owned()), ("x-wisp-height", "1".to_owned())],
            })
        }
    }

    fn get(uri: &str) -> Request<Vec<u8>> {
        Request::builder()
            .method(Method::GET)
            .uri(uri)
            .body(Vec::new())
            .expect("request")
    }

    #[test]
    fn decodes_encoded_slashes() {
        assert_eq!(
            percent_decode("/capture%2F3%2F1%2Ffull").as_deref(),
            Some("/capture/3/1/full")
        );
        assert_eq!(percent_decode("a%E4%BD%A0b").as_deref(), Some("a你b"));
        assert_eq!(percent_decode("bad%2"), None);
        assert_eq!(percent_decode("bad%zz"), None);
    }

    #[test]
    fn dispatches_by_prefix_with_cors() {
        let reg = AssetRegistry::new();
        reg.register("capture", Arc::new(Fixed));
        for uri in [
            "wisp://localhost/capture%2F3%2F1%2Ffull",
            "http://wisp.localhost/capture%2F3%2F1%2Ffull",
        ] {
            let resp = reg.respond(&get(uri));
            assert_eq!(resp.status(), StatusCode::OK, "{uri}");
            assert_eq!(resp.body().len(), 4);
            let h = resp.headers();
            assert_eq!(h[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
            assert_eq!(h[header::CACHE_CONTROL], "no-store");
            assert_eq!(h[header::ACCESS_CONTROL_EXPOSE_HEADERS], EXPOSE_HEADERS);
            assert_eq!(h["x-wisp-width"], "1");
        }
    }

    #[test]
    fn unique_asset_body_is_moved_not_copied() {
        let pixels = vec![7u8; 4096];
        let ptr = pixels.as_ptr();
        let unique = Asset {
            bytes: Arc::new(pixels),
            mime: "application/octet-stream",
            extra_headers: Vec::new(),
        };
        let resp = asset_response(unique, false);
        assert_eq!(resp.body().as_ptr(), ptr, "唯一持有时直接移交 Vec");

        let shared = Arc::new(vec![9u8; 16]);
        let asset = Asset {
            bytes: Arc::clone(&shared),
            mime: "application/octet-stream",
            extra_headers: Vec::new(),
        };
        let resp = asset_response(asset, false);
        assert_eq!(
            resp.body().as_slice(),
            shared.as_slice(),
            "共享时拷贝一次，原数据不受影响"
        );
        assert_eq!(asset_response_head_len(&shared), 16);
    }

    fn asset_response_head_len(bytes: &Arc<Vec<u8>>) -> usize {
        let asset = Asset {
            bytes: Arc::clone(bytes),
            mime: "application/octet-stream",
            extra_headers: Vec::new(),
        };
        let resp = asset_response(asset, true);
        assert!(resp.body().is_empty(), "HEAD 不带 body");
        resp.headers()[header::CONTENT_LENGTH]
            .to_str()
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

    #[test]
    fn rejects_traversal_and_suspicious_segments() {
        let reg = AssetRegistry::new();
        reg.register("capture", Arc::new(Fixed));
        reg.register("img", Arc::new(Fixed));
        for uri in [
            // 目录穿越（编码后的 / 与 .）
            "wisp://localhost/img%2F..%2F..%2Fetc%2Fpasswd",
            "wisp://localhost/img%2F%2E%2E%2Fsecret",
            "wisp://localhost/img%2F.%2Fa",
            "wisp://localhost/capture%2F3%2F..%2F1%2Ffull",
            // 前缀后紧跟 /（剩余部分会变成绝对路径）、空段、结尾 /
            "wisp://localhost/img%2F%2Fetc%2Fpasswd",
            "wisp://localhost/capture%2F3%2F%2F1%2Ffull",
            "wisp://localhost/capture%2F3%2F1%2Ffull%2F",
            // 反斜杠、Windows 盘符、NUL 与其他控制字符
            "wisp://localhost/img%2F..%5C..%5Cwin.ini",
            "wisp://localhost/img%2Fa%5Cb",
            "wisp://localhost/img%2FC%3A%2FWindows%2Fwin.ini",
            "wisp://localhost/img%2Fa%00b",
            "wisp://localhost/img%2Fa%0Ab",
            // 空路径
            "wisp://localhost/",
        ] {
            assert_eq!(reg.respond(&get(uri)).status(), StatusCode::BAD_REQUEST, "{uri}");
        }
        // fetch 同样拒绝（直接调用方不绕过校验）
        assert!(reg.fetch("capture/3/../1/full").is_none());
        assert!(reg.fetch("/capture/3/1/full").is_some());
        // 合法的点号文件名不受影响
        assert_eq!(split_path("/crop/r-1.png"), Some(("crop", "r-1.png")));
        assert_eq!(split_path("thumb/ab..cd.jpg"), Some(("thumb", "ab..cd.jpg")));
        assert_eq!(split_path("capture"), Some(("capture", "")));
        assert_eq!(split_path("//capture/x"), None, "只去掉一个开头的 /");
    }

    /// 与 JS `encodeURIComponent` 相同的转义（`convertFileSrc` 对整个 path 做这一步）
    fn encode_uri_component(s: &str) -> String {
        let mut out = String::new();
        for b in s.bytes() {
            if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
                out.push(char::from(b));
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
        out
    }

    /// 只认 Windows 主显示器的全分辨率帧
    struct WindowsFrames;

    impl AssetSource for WindowsFrames {
        fn fetch(&self, path: &str) -> Option<Asset> {
            let (session, monitor, variant) = parse_capture_path(path)?;
            (session == 7 && monitor == r"\\.\DISPLAY1" && variant == CaptureVariant::Full).then(|| Asset {
                bytes: Arc::new(vec![0u8; 4]),
                mime: "application/octet-stream",
                extra_headers: Vec::new(),
            })
        }
    }

    #[test]
    fn capture_path_encodes_windows_monitor_id() {
        let windows_id = r"\\.\DISPLAY1";
        let path = capture_asset_path(7, windows_id, CaptureVariant::Full);
        assert_eq!(path, "capture/7/5c5c2e5c444953504c415931/full");
        // 三端的 id 都能往返；编码结果一定通过 split_path（只含 [0-9a-f]）
        for id in [windows_id, "69733568", "DP-1", "HDMI-A-1", "显示器:1"] {
            for variant in [CaptureVariant::Half, CaptureVariant::Full] {
                let p = capture_asset_path(u64::MAX, id, variant);
                let (prefix, rest) = split_path(&p).unwrap_or_else(|| panic!("{p} 未通过 split_path"));
                assert_eq!(prefix, "capture");
                assert_eq!(
                    parse_capture_path(rest),
                    Some((u64::MAX, id.to_owned(), variant)),
                    "{id}"
                );
            }
        }

        // 端到端：前端 convertFileSrc(path, 'wisp') = encodeURIComponent 后拼到协议源上，两种源都能分发到帧
        let reg = AssetRegistry::new();
        reg.register("capture", Arc::new(WindowsFrames));
        for origin in ["wisp://localhost/", "http://wisp.localhost/"] {
            let uri = format!("{origin}{}", encode_uri_component(&path));
            assert_eq!(reg.respond(&get(&uri)).status(), StatusCode::OK, "{uri}");
        }
        let half = capture_asset_path(7, windows_id, CaptureVariant::Half);
        let uri = format!("wisp://localhost/{}", encode_uri_component(&half));
        assert_eq!(
            reg.respond(&get(&uri)).status(),
            StatusCode::NOT_FOUND,
            "来源没有 half 帧"
        );
        // 反例：把 MonitorId 原样放进路径会被集中校验拒绝（这正是要编码的原因）
        let raw = format!("capture/7/{windows_id}/full");
        let uri = format!("wisp://localhost/{}", encode_uri_component(&raw));
        assert_eq!(reg.respond(&get(&uri)).status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn parse_capture_path_rejects_malformed() {
        for bad in [
            "",
            "7",
            "7/31",
            "7/31/full/x",
            "7//full",
            "7/3/full",                     // 奇数长度
            "7/3G/full",                    // 非十六进制
            "7/3A/full",                    // 大写不是规范写法
            "7/31/medium",                  // 未知规格
            "+7/31/full",                   // 非规范十进制
            "-1/31/full",                   // 负数
            "x/31/full",                    // 非数字
            "7/ff/full",                    // 解码后不是 UTF-8
            "18446744073709551616/31/full", // 超出 u64
        ] {
            assert_eq!(parse_capture_path(bad), None, "{bad}");
        }
        assert_eq!(
            parse_capture_path("0/31/half"),
            Some((0, "1".to_owned(), CaptureVariant::Half))
        );
    }

    /// `region` 变体：基础路径 + 4 段规范十进制 + 可缺省的面积上限；宽高为 0、缺段、多段、非规范写法、
    /// 上限为 0 或超过 REGION_MAX_PX 一律拒绝
    #[test]
    fn region_paths_round_trip_and_reject_malformed() {
        let base = capture_region_base(3, "1");
        assert_eq!(base, "capture/3/31/region");
        let r = wisp_types::PhysRect {
            x: 0,
            y: 1964,
            width: 256,
            height: 1,
        };
        let path = format!("{base}/0/1964/256/1");
        assert_eq!(path, capture_asset_path(3, "1", CaptureVariant::Region(r, None)));
        let (prefix, rest) = split_path(&path).expect("split");
        assert_eq!(prefix, "capture");
        assert_eq!(
            parse_capture_path(rest),
            Some((3, "1".to_owned(), CaptureVariant::Region(r, None)))
        );
        // 低分辨率底图：第 5 段是输出面积上限
        let low = format!("{base}/0/1964/256/1/160000");
        assert_eq!(
            low,
            capture_asset_path(3, "1", CaptureVariant::Region(r, Some(160_000)))
        );
        assert_eq!(
            parse_capture_path(split_path(&low).expect("split").1),
            Some((3, "1".to_owned(), CaptureVariant::Region(r, Some(160_000))))
        );
        assert_eq!(
            parse_capture_path("3/31/region/0/0/16/16/4000000"),
            Some((
                3,
                "1".to_owned(),
                CaptureVariant::Region(
                    wisp_types::PhysRect {
                        x: 0,
                        y: 0,
                        width: 16,
                        height: 16
                    },
                    Some(4_000_000)
                )
            ))
        );
        for bad in [
            "3/31/region",
            "3/31/region/0/0/16",
            "3/31/region/0/0/16/16/0",
            "3/31/region/0/0/16/16/4000001",
            "3/31/region/0/0/16/16/01",
            "3/31/region/0/0/16/16/1/1",
            "3/31/region/0/0/0/16",
            "3/31/region/0/0/16/0",
            "3/31/region/-1/0/16/16",
            "3/31/region/+1/0/16/16",
            "3/31/region/01/0/16/16",
            "3/31/region/1.5/0/16/16",
            "3/31/region/4294967296/0/16/16",
            "3/31/full/0/0/16/16",
        ] {
            assert_eq!(parse_capture_path(bad), None, "{bad}");
        }
    }

    #[test]
    fn missing_is_404_and_options_is_204() {
        let reg = AssetRegistry::new();
        reg.register("capture", Arc::new(Fixed));
        assert_eq!(
            reg.respond(&get("wisp://localhost/capture%2F9%2F9%2Ffull")).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            reg.respond(&get("wisp://localhost/nope%2Fx")).status(),
            StatusCode::NOT_FOUND
        );
        let pre = Request::builder()
            .method(Method::OPTIONS)
            .uri("wisp://localhost/x")
            .body(Vec::new())
            .expect("request");
        let resp = reg.respond(&pre);
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(resp.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
        reg.unregister("capture");
        assert_eq!(
            reg.respond(&get("wisp://localhost/capture%2F3%2F1%2Ffull")).status(),
            StatusCode::NOT_FOUND
        );
    }
}
