//! zip 的区间提取：只用 HTTP Range 取发布包里的一个条目（Windows 的 ONNX Runtime 整包 78.6 MB，dll 压缩后只有 5.7 MB）。
//!
//! 步骤：取文件末尾（结尾记录 + 可能的注释，最多 64 KB）→ 解析结尾记录得到中央目录的位置 → 取中央目录、找到条目 →
//! 取本地头（它的文件名与扩展字段长度可能与中央目录里的不同）→ 取压缩数据 → inflate → 校验解压后的 sha256。
//! 服务器不支持 Range、包是 ZIP64、条目的压缩方式不认识时返回 [`Ranged::Unsupported`]，调用方退回整包下载。

use std::io::Write;
use std::path::Path;

use wisp_types::DownloadErrorCode;

use super::fetch::Fetcher;
use crate::error::LocalError;

/// 结尾记录的签名与固定长度
const EOCD_SIG: u32 = 0x0605_4b50;
const EOCD_LEN: usize = 22;
/// 结尾记录之后最多 65535 字节的注释
const TAIL_MAX: u64 = EOCD_LEN as u64 + 65_535;
/// 中央目录项的签名与固定长度
const CDH_SIG: u32 = 0x0201_4b50;
const CDH_LEN: usize = 46;
/// 本地头的签名与固定长度
const LFH_SIG: u32 = 0x0403_4b50;
const LFH_LEN: usize = 30;
/// 只接受这么大以内的条目（压缩后）：区间提取的数据放在内存里
const MAX_ENTRY: u64 = 64 * 1024 * 1024;

/// 区间提取的结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ranged {
    /// 已解出并校验
    Done,
    /// 这个服务器或这个包不适合区间提取：退回整包
    Unsupported,
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// 结尾记录：(中央目录偏移, 中央目录大小)；ZIP64（字段是 0xFFFF…）为 None
pub(crate) fn parse_eocd(tail: &[u8]) -> Option<(u64, u64)> {
    let pos = (0..=tail.len().checked_sub(EOCD_LEN)?)
        .rev()
        .find(|&i| u32_at(tail, i) == Some(EOCD_SIG))?;
    let size = u32_at(tail, pos + 12)?;
    let offset = u32_at(tail, pos + 16)?;
    if size == u32::MAX || offset == u32::MAX || u16_at(tail, pos + 10)? == u16::MAX {
        return None;
    }
    Some((u64::from(offset), u64::from(size)))
}

/// 中央目录里的一个条目
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) method: u16,
    pub(crate) compressed: u64,
    pub(crate) uncompressed: u64,
    pub(crate) local_header: u64,
}

/// 在中央目录里找名为 `name` 的条目
pub(crate) fn find_entry(cd: &[u8], name: &str) -> Option<Entry> {
    let mut at = 0usize;
    while u32_at(cd, at) == Some(CDH_SIG) {
        let method = u16_at(cd, at + 10)?;
        let compressed = u32_at(cd, at + 20)?;
        let uncompressed = u32_at(cd, at + 24)?;
        let n = usize::from(u16_at(cd, at + 28)?);
        let m = usize::from(u16_at(cd, at + 30)?);
        let k = usize::from(u16_at(cd, at + 32)?);
        let local = u32_at(cd, at + 42)?;
        let file_name = cd.get(at + CDH_LEN..at + CDH_LEN + n)?;
        if file_name == name.as_bytes() {
            if compressed == u32::MAX || uncompressed == u32::MAX || local == u32::MAX {
                return None;
            }
            return Some(Entry {
                method,
                compressed: u64::from(compressed),
                uncompressed: u64::from(uncompressed),
                local_header: u64::from(local),
            });
        }
        at += CDH_LEN + n + m + k;
    }
    None
}

/// 本地头之后数据开始的偏移
pub(crate) fn data_offset(local_header_at: u64, header: &[u8]) -> Option<u64> {
    if u32_at(header, 0)? != LFH_SIG {
        return None;
    }
    let n = u64::from(u16_at(header, 26)?);
    let m = u64::from(u16_at(header, 28)?);
    Some(local_header_at + LFH_LEN as u64 + n + m)
}

/// 解压并校验（阻塞）：写到 `dest`，sha256 必须等于 `sha256`
pub(crate) fn inflate_to(
    data: &[u8],
    method: u16,
    uncompressed: u64,
    sha256: &str,
    dest: &Path,
) -> Result<(), LocalError> {
    let tmp = dest.with_extension("tmp");
    let mut out = std::fs::File::create(&tmp).map_err(super::fetch::local_io)?;
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut written = 0u64;
    let mut sink = |buf: &[u8]| -> Result<(), LocalError> {
        // 解出的量超过中央目录声明的大小就立即停止：内容一定不对，而压缩数据理论上能展开成几十 GB
        if written.saturating_add(buf.len() as u64) > uncompressed {
            return Err(LocalError::Download(DownloadErrorCode::ChecksumMismatch));
        }
        out.write_all(buf).map_err(super::fetch::local_io)?;
        ctx.update(buf);
        written += buf.len() as u64;
        Ok(())
    };
    let inflated = match method {
        0 => sink(data),
        8 => {
            use std::io::Read;
            let mut dec = flate2::read::DeflateDecoder::new(data);
            let mut buf = vec![0u8; 1 << 16];
            loop {
                let n = match dec.read(&mut buf) {
                    Ok(n) => n,
                    Err(_) => break Err(LocalError::Download(DownloadErrorCode::Extract)),
                };
                if n == 0 {
                    break Ok(());
                }
                if let Err(e) = sink(&buf[..n]) {
                    break Err(e);
                }
            }
        }
        _ => Err(LocalError::Download(DownloadErrorCode::Extract)),
    };
    if let Err(e) = inflated {
        drop(out);
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    out.sync_all().map_err(super::fetch::local_io)?;
    drop(out);
    let got: String = ctx.finish().as_ref().iter().map(|b| format!("{b:02x}")).collect();
    if written != uncompressed || got != sha256 {
        let _ = std::fs::remove_file(&tmp);
        return Err(LocalError::Download(DownloadErrorCode::ChecksumMismatch));
    }
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

impl Fetcher<'_> {
    /// 从远端 zip（`url`，整包 `archive_size` 字节）只取条目 `entry`，解压到 `dest` 并校验 sha256
    pub async fn zip_entry(
        &mut self,
        url: &str,
        archive_size: u64,
        entry: &str,
        sha256: &str,
        dest: &Path,
    ) -> Result<Ranged, LocalError> {
        let tail_len = archive_size.min(TAIL_MAX);
        let tail_start = archive_size - tail_len;
        let Some(tail) = self.range(url, tail_start, archive_size - 1, archive_size).await? else {
            return Ok(Ranged::Unsupported);
        };
        let Some((cd_off, cd_len)) = parse_eocd(&tail) else {
            return Ok(Ranged::Unsupported);
        };
        if cd_off + cd_len > archive_size {
            return Ok(Ranged::Unsupported);
        }
        let cd = if cd_off >= tail_start {
            let from = usize::try_from(cd_off - tail_start).map_err(|_| LocalError::Protocol("zip"))?;
            let to = usize::try_from(cd_off + cd_len - tail_start).map_err(|_| LocalError::Protocol("zip"))?;
            tail.get(from..to).map(<[u8]>::to_vec)
        } else {
            self.range(url, cd_off, cd_off + cd_len - 1, archive_size).await?
        };
        let Some(e) = cd.as_deref().and_then(|cd| find_entry(cd, entry)) else {
            return Ok(Ranged::Unsupported);
        };
        if !matches!(e.method, 0 | 8) || e.compressed > MAX_ENTRY || e.compressed == 0 {
            return Ok(Ranged::Unsupported);
        }
        let Some(header) = self
            .range(url, e.local_header, e.local_header + LFH_LEN as u64 - 1, archive_size)
            .await?
        else {
            return Ok(Ranged::Unsupported);
        };
        let Some(start) = data_offset(e.local_header, &header) else {
            return Ok(Ranged::Unsupported);
        };
        if start + e.compressed > archive_size {
            return Ok(Ranged::Unsupported);
        }
        let Some(data) = self.range(url, start, start + e.compressed - 1, archive_size).await? else {
            return Ok(Ranged::Unsupported);
        };
        // 解压一个不超过 64 MB 的条目不到一秒，这里不报「校验中」（由调用方按整个任务决定什么时候报，见 `install_runtime`）
        let (dest, sha) = (dest.to_path_buf(), sha256.to_owned());
        super::blocking(move || inflate_to(&data, e.method, e.uncompressed, &sha, &dest)).await?;
        Ok(Ranged::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用 zip crate 写一个两项的包，再用这里的解析器找回第二项
    #[test]
    fn central_directory_round_trip() {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            z.start_file("pkg/readme.txt", opts).expect("start");
            z.write_all(b"hello").expect("write");
            z.start_file("pkg/lib/onnxruntime.dll", opts).expect("start");
            z.write_all(&vec![7u8; 10_000]).expect("write");
            z.finish().expect("finish");
        }
        let bytes = buf.into_inner();
        let (off, len) = parse_eocd(&bytes).expect("eocd");
        let cd = &bytes[usize::try_from(off).expect("off")..usize::try_from(off + len).expect("len")];
        let e = find_entry(cd, "pkg/lib/onnxruntime.dll").expect("entry");
        assert_eq!((e.method, e.uncompressed), (8, 10_000));
        assert!(find_entry(cd, "pkg/lib/missing.dll").is_none());
        let at = usize::try_from(e.local_header).expect("at");
        let start =
            usize::try_from(data_offset(e.local_header, &bytes[at..at + LFH_LEN]).expect("data")).expect("start");
        let data = &bytes[start..start + usize::try_from(e.compressed).expect("c")];
        let dir = tempfile::tempdir().expect("dir");
        let dest = dir.path().join("onnxruntime.dll");
        let sha: String = ring::digest::digest(&ring::digest::SHA256, &vec![7u8; 10_000])
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        inflate_to(data, e.method, e.uncompressed, &sha, &dest).expect("inflate");
        assert_eq!(std::fs::read(&dest).expect("read"), vec![7u8; 10_000]);
        assert!(matches!(
            inflate_to(data, e.method, e.uncompressed, &"0".repeat(64), &dest),
            Err(LocalError::Download(DownloadErrorCode::ChecksumMismatch))
        ));
        // 声明的解压大小比实际小：写到上限就停，不留临时文件
        let small = dir.path().join("small.dll");
        assert!(matches!(
            inflate_to(data, e.method, 4_096, &sha, &small),
            Err(LocalError::Download(DownloadErrorCode::ChecksumMismatch))
        ));
        assert!(!small.with_extension("tmp").exists());
        assert!(!small.exists());
    }

    #[test]
    fn zip64_markers_are_unsupported() {
        let mut eocd = vec![0u8; EOCD_LEN];
        eocd[..4].copy_from_slice(&EOCD_SIG.to_le_bytes());
        eocd[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse_eocd(&eocd), None);
        assert_eq!(parse_eocd(b"short"), None);
    }
}
