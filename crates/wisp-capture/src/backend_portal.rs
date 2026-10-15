//! Wayland 后端：xdg-desktop-portal Screenshot。
//!
//! 交互式截图由系统负责框选（跳过 Wisp 自己的 Overlay），门户把结果写成图片文件并返回 `file://` URI；
//! 读出后立即删除该文件（截图不落盘），解码为 RGBA。

use ashpd::desktop::ResponseError;
use ashpd::desktop::screenshot::Screenshot;
use image::RgbaImage;

use crate::error::CaptureError;

/// 交互式截图（阻塞在用户完成框选或取消之前）
pub(crate) async fn screenshot() -> Result<RgbaImage, CaptureError> {
    let request = Screenshot::request()
        .interactive(true)
        .send()
        .await
        .map_err(map_error)?;
    let response = request.response().map_err(map_error)?;
    let uri = response.uri().as_str();
    let path =
        file_uri_to_path(uri).ok_or_else(|| CaptureError::Backend(format!("portal returned a non-file URI: {uri}")))?;

    // 文件很小（单张截图），同步读写的耗时在毫秒级；读完立即删除，避免截图留在磁盘上
    let bytes = std::fs::read(&path).map_err(|e| CaptureError::Backend(format!("read portal screenshot: {e}")));
    if let Err(e) = std::fs::remove_file(&path) {
        log::warn!("删除门户截图临时文件失败：{e}");
    }
    let img = image::load_from_memory(&bytes?)
        .map_err(|e| CaptureError::Backend(format!("decode portal screenshot: {e}")))?;
    Ok(img.to_rgba8())
}

fn map_error(e: ashpd::Error) -> CaptureError {
    match e {
        ashpd::Error::Response(ResponseError::Cancelled) => CaptureError::Cancelled,
        ashpd::Error::PortalNotFound(_) => CaptureError::Unsupported("the desktop has no screenshot portal".to_owned()),
        ashpd::Error::Zbus(e) => CaptureError::Unsupported(format!("D-Bus unavailable: {e}")),
        other => CaptureError::Backend(format!("portal: {other}")),
    }
}

/// `file:///path/%E5%9B%BE.png` → `/path/图.png`；只接受本机 file URI（主机名为空或 localhost）
fn file_uri_to_path(uri: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    let rest = uri.strip_prefix("file://")?;
    let path = if rest.starts_with('/') {
        rest
    } else {
        rest.strip_prefix("localhost")?
    };
    if !path.starts_with('/') {
        return None;
    }
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(std::path::PathBuf::from(std::ffi::OsString::from_vec(out)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_file_uris() {
        assert_eq!(
            file_uri_to_path("file:///home/u/Pictures/Screenshot%20from%202026.png"),
            Some("/home/u/Pictures/Screenshot from 2026.png".into())
        );
        assert_eq!(
            file_uri_to_path("file:///home/u/%E5%9B%BE%E7%89%87/a.png"),
            Some("/home/u/图片/a.png".into())
        );
        assert_eq!(
            file_uri_to_path("file://localhost/tmp/a.png"),
            Some("/tmp/a.png".into())
        );
        assert_eq!(file_uri_to_path("file://otherhost/tmp/a.png"), None);
        assert_eq!(file_uri_to_path("https://example.com/a.png"), None);
        assert_eq!(file_uri_to_path("file:///bad%zz"), None);
    }
}
