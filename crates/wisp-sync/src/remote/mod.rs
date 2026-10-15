//! 远端存储抽象。
//!
//! 按最弱的 WebDAV 服务器设计：只用 `Depth: 1` 的 PROPFIND；条件请求（`If-Match`）会被忽略，
//! 所以每台设备只写自己拥有的文件；PUT 不保证自动建目录，写前用 [`DirCache`] 确保目录存在。
//!
//! 路径约定：相对于同步根目录（用户填写的地址之下的 [`ROOT_DIR`]），用 `/` 分隔，不带开头与结尾的 `/`，
//! 空字符串表示根目录本身。实现负责逐段百分号编码。

use std::collections::HashSet;
use std::future::Future;
use std::path::Path;

use bytes::Bytes;

use crate::error::RemoteError;

pub mod memory;
pub mod webdav;

pub use memory::MemoryRemote;
pub use webdav::{WebDav, validate_server_url};

/// 用户填写的地址之下的固定子目录
pub const ROOT_DIR: &str = "wisp";

/// 目录列表中的一项
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// 名称（最后一段，已解码），不含路径与结尾的 `/`
    pub name: String,
    pub is_dir: bool,
    /// 服务器报告的 ETag，只用于相等比较的不透明值（WebDAV 实现去掉了 `W/` 前缀与引号，
    /// 使列目录与 PUT 响应头里的写法一致）；有的服务器不提供
    pub etag: Option<String>,
    /// 字节数（`getcontentlength`）
    pub size: Option<u64>,
    /// 最后修改时间（`getlastmodified`，毫秒）
    pub modified_ms: Option<i64>,
}

/// 写入的结果
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PutInfo {
    /// PUT 响应里的 ETag（与 [`Entry::etag`] 同样规范化）；没有时调用方采用下一次列目录时报告的值
    pub etag: Option<String>,
}

/// 远端存储。方法返回的 future 都是 `Send`，可以在多线程运行时里跨 `.await` 持有；
/// trait 不是对象安全的，按泛型使用（`R: Remote`）
pub trait Remote: Send + Sync {
    /// 列出目录的直接子项（`Depth: 1`，不含目录自身）；目录不存在返回 `None`
    fn list(&self, dir: &str) -> impl Future<Output = Result<Option<Vec<Entry>>, RemoteError>> + Send;

    /// 读取整个文件到内存（vault.json、head、日志段）；不存在返回 `None`
    fn get(&self, path: &str) -> impl Future<Output = Result<Option<Bytes>, RemoteError>> + Send;

    /// 把文件按流写到本地 `dest`（覆盖），不整块读入内存；不存在返回 `false`（不创建 `dest`）
    fn get_to_file(&self, path: &str, dest: &Path) -> impl Future<Output = Result<bool, RemoteError>> + Send;

    /// 写入（覆盖）文件；父目录必须已存在
    fn put(&self, path: &str, body: Bytes) -> impl Future<Output = Result<PutInfo, RemoteError>> + Send;

    /// 从本地文件 `src` 按流上传（覆盖），`len` 是文件长度（作为 Content-Length）；父目录必须已存在
    fn put_file(&self, path: &str, src: &Path, len: u64) -> impl Future<Output = Result<PutInfo, RemoteError>> + Send;

    /// 删除文件；路径以 `/` 结尾时删除目录及其全部内容。不存在视为成功
    fn delete(&self, path: &str) -> impl Future<Output = Result<(), RemoteError>> + Send;

    /// 创建目录；已存在视为成功（201 / 405）；父目录不存在 ⇒ `Status(409)`
    fn mkcol(&self, path: &str) -> impl Future<Output = Result<(), RemoteError>> + Send;
}

/// 已确认存在的远端目录（只在内存里，进程内有效）。写前调用 [`DirCache::ensure`]，
/// 每个目录在一个进程里最多 MKCOL 一次
#[derive(Debug, Default, Clone)]
pub struct DirCache {
    known: HashSet<String>,
}

impl DirCache {
    pub fn new() -> DirCache {
        DirCache::default()
    }

    /// 确保 `dir` 与它的所有上级（包括根目录 `""`）都存在，从上往下逐级 MKCOL，已确认的跳过
    pub async fn ensure<R: Remote>(&mut self, remote: &R, dir: &str) -> Result<(), RemoteError> {
        if self.known.contains(dir) {
            return Ok(());
        }
        let mut prefixes = vec![String::new()];
        let mut current = String::new();
        for part in dir.split('/').filter(|p| !p.is_empty()) {
            if !current.is_empty() {
                current.push('/');
            }
            current.push_str(part);
            prefixes.push(current.clone());
        }
        for prefix in prefixes {
            if !self.known.contains(&prefix) {
                remote.mkcol(&prefix).await?;
                self.known.insert(prefix);
            }
        }
        Ok(())
    }

    /// 记下已知存在的目录（例如列目录时看到的）
    pub fn mark(&mut self, dir: &str) {
        self.known.insert(dir.to_string());
    }

    /// 忘掉全部记录（服务器数据被清除、写入遇到 409 之后）
    pub fn clear(&mut self) {
        self.known.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dir_cache_creates_each_level_once() {
        let remote = MemoryRemote::new();
        let mut cache = DirCache::new();
        cache.ensure(&remote, "v/log/d").await.unwrap();
        assert_eq!(remote.requests(), 4); // "", "v", "v/log", "v/log/d"
        cache.ensure(&remote, "v/log/d").await.unwrap();
        cache.ensure(&remote, "v/log").await.unwrap();
        assert_eq!(remote.requests(), 4);
        cache.ensure(&remote, "v/blobs/d").await.unwrap();
        assert_eq!(remote.requests(), 6);
        remote.put("v/blobs/d/x.blob", Bytes::from_static(b"x")).await.unwrap();
    }
}
