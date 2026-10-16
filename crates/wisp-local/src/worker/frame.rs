//! ORT 工作进程的帧协议（stdin / stdout）。
//!
//! 一帧 = `u32 LE 头长度` + `u32 LE 负载长度` + 头（UTF-8 JSON）+ 负载（原始字节，例如 RGB8 像素）。请求与响应一问一答、
//! 严格串行。头与负载各有上限，超出即视为协议错误（防止坏数据让对方分配巨量内存）。

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

use crate::ocr::layout::LayoutBox;
use crate::ocr::{PpOcrTier, TextLine};

/// 头的上限（1 MB）
pub const MAX_HEADER: usize = 1024 * 1024;
/// 负载的上限（256 MB；4096 × 4096 的 RGB8 约 48 MB）
pub const MAX_PAYLOAD: usize = 256 * 1024 * 1024;

/// 一帧
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Frame {
    pub header: Vec<u8>,
    pub payload: Vec<u8>,
}

/// 写一帧（不 flush；调用方写完后 flush）
pub fn write_frame<W: Write>(w: &mut W, header: &[u8], payload: &[u8]) -> io::Result<()> {
    if header.len() > MAX_HEADER || payload.len() > MAX_PAYLOAD {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "frame too large"));
    }
    let len = |n: usize| u32::try_from(n).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"));
    w.write_all(&len(header.len())?.to_le_bytes())?;
    w.write_all(&len(payload.len())?.to_le_bytes())?;
    w.write_all(header)?;
    w.write_all(payload)
}

/// 读一帧；对端在帧边界上关闭时为 `Ok(None)`，帧中途断开或超出上限为错误
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Option<Frame>> {
    let mut lens = [0u8; 8];
    // 第一个字节就读到 EOF 才算正常结束
    let mut got = 0;
    while got < lens.len() {
        match r.read(&mut lens[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let header_len = u32::from_le_bytes([lens[0], lens[1], lens[2], lens[3]]) as usize;
    let payload_len = u32::from_le_bytes([lens[4], lens[5], lens[6], lens[7]]) as usize;
    if header_len > MAX_HEADER || payload_len > MAX_PAYLOAD {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut header = vec![0u8; header_len];
    r.read_exact(&mut header)?;
    let mut payload = vec![0u8; payload_len];
    r.read_exact(&mut payload)?;
    Ok(Some(Frame { header, payload }))
}

/// 请求头（负载为 RGB8 像素，`width × height × 3` 字节）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// 存活检查
    Ping,
    /// PP-OCRv6 检测 + 识别。`max_side` 为检测输入的长边上限（`ocr.max_edge_px`）
    Ppocr {
        tier: PpOcrTier,
        width: u32,
        height: u32,
        max_side: u32,
    },
    /// PP-DocLayoutV3 版面检测（已做后处理与排序）
    Layout { width: u32, height: u32 },
    /// 退出（与 stdin EOF 等价）
    Exit,
}

/// 工作进程报告的错误种类（固定词汇，不含路径与内容）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerError {
    /// 请求不合法（像素长度不符等）
    BadRequest,
    /// ONNX Runtime 动态库加载失败（原因不属于下面两种）
    RuntimeLoad,
    /// 动态库依赖的 Microsoft Visual C++ 运行库不在系统里（Windows 上 `LoadLibraryExW` 报「找不到指定的模块」）
    VcRuntimeMissing,
    /// 动态库依赖的系统库缺失或版本太旧（`dlopen` 报找不到共享库、glibc / libstdc++ 的符号版本不满足）
    SystemLibraryMissing,
    /// 模型文件缺失或加载失败
    ModelLoad,
    /// 推理失败
    Inference,
    /// 内存不足
    OutOfMemory,
    /// 这个构建不支持该操作（没有编入 ONNX Runtime）
    Unsupported,
}

/// 响应头（没有负载）
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    /// `ppocr` 的结果
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<Vec<TextLine>>,
    /// `layout` 的结果
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boxes: Option<Vec<LayoutBox>>,
    /// 失败时的种类
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<WorkerError>,
}

impl Response {
    /// 成功、没有数据（`ping`）
    pub fn ok() -> Self {
        Self {
            ok: true,
            ..Self::default()
        }
    }

    /// 失败
    pub fn err(kind: WorkerError) -> Self {
        Self {
            ok: false,
            error: Some(kind),
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, br#"{"op":"ping"}"#, &[]).expect("write");
        write_frame(&mut buf, b"{}", &[1, 2, 3]).expect("write");
        let mut r = buf.as_slice();
        let a = read_frame(&mut r).expect("read").expect("frame");
        assert_eq!(
            serde_json::from_slice::<Request>(&a.header).expect("json"),
            Request::Ping
        );
        let b = read_frame(&mut r).expect("read").expect("frame");
        assert_eq!(b.payload, [1, 2, 3]);
        assert_eq!(read_frame(&mut r).expect("eof"), None, "帧边界上的 EOF 是正常结束");
    }

    #[test]
    fn truncated_and_oversized_frames_are_errors() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"{}", &[9; 16]).expect("write");
        buf.truncate(buf.len() - 1);
        assert!(read_frame(&mut buf.as_slice()).is_err());

        let mut huge = Vec::new();
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        huge.extend_from_slice(&0u32.to_le_bytes());
        assert!(read_frame(&mut huge.as_slice()).is_err());
    }

    #[test]
    fn json_shapes() {
        let req = Request::Ppocr {
            tier: PpOcrTier::Small,
            width: 4,
            height: 2,
            max_side: 2048,
        };
        assert_eq!(
            serde_json::to_string(&req).expect("json"),
            r#"{"op":"ppocr","tier":"small","width":4,"height":2,"max_side":2048}"#
        );
        assert_eq!(
            serde_json::to_string(&Response::err(WorkerError::ModelLoad)).expect("json"),
            r#"{"ok":false,"error":"model_load"}"#
        );
    }
}
