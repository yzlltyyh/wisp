//! 内存里的远端：测试与多设备模拟用。
//!
//! 行为对齐「最弱的服务器」：PUT 不自动建目录（父目录不存在 ⇒ 409）、MKCOL 已存在视为成功、
//! 覆盖写后 ETag 改变。可以注入失败（操作前失败，或操作已生效但响应丢失）、延迟，以及去掉 ETag / 元数据。
//! 克隆出的句柄共享同一份状态，多台模拟设备各持一个句柄即可访问同一台「服务器」。

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use parking_lot::Mutex;

use super::{Entry, PutInfo, Remote};
use crate::error::RemoteError;

/// 远端操作种类（失败注入钩子据此决定是否失败）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Op {
    List,
    Get,
    Put,
    Delete,
    Mkcol,
}

impl Op {
    pub const ALL: [Op; 5] = [Op::List, Op::Get, Op::Put, Op::Delete, Op::Mkcol];

    const fn index(self) -> usize {
        self as usize
    }
}

/// 注入的失败
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// 操作不生效，直接返回错误
    Before(RemoteError),
    /// 操作已在服务器上生效，但客户端收到错误（模拟响应丢失）
    After(RemoteError),
}

/// 失败注入钩子：对每次操作调用，返回 `Some` 即注入
pub type FaultHook = Arc<dyn Fn(Op, &str) -> Option<Fault> + Send + Sync>;

/// 列目录时报告哪些元数据
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Metadata {
    /// ETag、大小、修改时间都有
    #[default]
    Full,
    /// 没有 ETag（PUT 响应也不带），只有大小与修改时间
    NoEtag,
    /// 什么都没有
    None,
}

#[derive(Debug, Clone)]
struct MemFile {
    bytes: Bytes,
    etag: String,
    modified_ms: i64,
}

#[derive(Default)]
struct State {
    files: BTreeMap<String, MemFile>,
    dirs: BTreeSet<String>,
    next_etag: u64,
    clock_ms: i64,
    metadata: Metadata,
    lenient_parents: bool,
    fault: Option<FaultHook>,
    latency: Option<Duration>,
    requests: u64,
    /// 按 [`Op::index`] 分类的请求数
    per_op: [u64; 5],
    /// 请求日志（`Some` 时记录每个请求的种类与路径；默认不记，长时间的模拟不必无限增长）
    log: Option<Vec<(Op, String)>>,
}

/// 内存远端
#[derive(Clone, Default)]
pub struct MemoryRemote {
    state: Arc<Mutex<State>>,
}

impl std::fmt::Debug for MemoryRemote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.state.lock();
        f.debug_struct("MemoryRemote")
            .field("files", &s.files.len())
            .field("dirs", &s.dirs.len())
            .finish()
    }
}

fn parent(path: &str) -> Option<&str> {
    if path.is_empty() {
        return None;
    }
    Some(path.rsplit_once('/').map_or("", |(p, _)| p))
}

fn name_of(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, n)| n)
}

fn is_under(path: &str, dir: &str) -> bool {
    dir.is_empty() || path.strip_prefix(dir).is_some_and(|rest| rest.starts_with('/'))
}

impl State {
    fn dir_exists(&self, dir: &str) -> bool {
        self.dirs.contains(dir)
    }

    fn parent_ok(&self, path: &str) -> bool {
        self.lenient_parents || parent(path).is_none_or(|p| self.dir_exists(p))
    }

    fn write(&mut self, path: &str, bytes: Bytes) -> PutInfo {
        if self.lenient_parents {
            let mut p = parent(path);
            while let Some(dir) = p {
                self.dirs.insert(dir.to_string());
                p = parent(dir);
            }
        }
        self.next_etag += 1;
        self.clock_ms += 1000;
        let etag = format!("\"m{}\"", self.next_etag);
        self.files.insert(
            path.to_string(),
            MemFile {
                bytes,
                etag: etag.clone(),
                modified_ms: self.clock_ms,
            },
        );
        PutInfo {
            etag: (self.metadata == Metadata::Full).then_some(etag),
        }
    }
}

impl MemoryRemote {
    /// 空服务器（连同步根目录都还不存在）
    pub fn new() -> MemoryRemote {
        let remote = MemoryRemote::default();
        remote.state.lock().clock_ms = 1_700_000_000_000;
        remote
    }

    /// 设置失败注入钩子（`None` 取消）
    pub fn set_fault(&self, hook: Option<FaultHook>) {
        self.state.lock().fault = hook;
    }

    /// 每个请求的延迟（配合 `tokio::time::pause` 使用）
    pub fn set_latency(&self, latency: Option<Duration>) {
        self.state.lock().latency = latency;
    }

    pub fn set_metadata(&self, metadata: Metadata) {
        self.state.lock().metadata = metadata;
    }

    /// `true`：PUT 自动创建缺失的上级目录（有些服务器如此）；默认 `false`
    pub fn set_lenient_parents(&self, lenient: bool) {
        self.state.lock().lenient_parents = lenient;
    }

    /// 至今收到的请求数（含被注入失败的）
    pub fn requests(&self) -> u64 {
        self.state.lock().requests
    }

    /// 至今收到的某一种请求的数量（含被注入失败的）
    pub fn requests_of(&self, op: Op) -> u64 {
        self.state.lock().per_op[op.index()]
    }

    /// 计数清零（总数与各类），同时清空请求日志（如果开着）
    pub fn reset_requests(&self) {
        let mut s = self.state.lock();
        s.requests = 0;
        s.per_op = [0; 5];
        if let Some(log) = s.log.as_mut() {
            log.clear();
        }
    }

    /// 开 / 关请求日志；关闭时丢弃已记录的内容
    pub fn set_request_log(&self, enabled: bool) {
        let mut s = self.state.lock();
        s.log = enabled.then(Vec::new);
    }

    /// 取出并清空请求日志（`(种类, 路径)`，按发生顺序）；日志未开启时为空
    pub fn take_request_log(&self) -> Vec<(Op, String)> {
        self.state.lock().log.as_mut().map(std::mem::take).unwrap_or_default()
    }

    /// 全部文件路径（有序）
    pub fn paths(&self) -> Vec<String> {
        self.state.lock().files.keys().cloned().collect()
    }

    /// 直接读取文件（不计请求、不触发注入）
    pub fn raw_get(&self, path: &str) -> Option<Bytes> {
        self.state.lock().files.get(path).map(|f| f.bytes.clone())
    }

    /// 直接写入文件（篡改测试用；自动补齐上级目录，不计请求、不触发注入）
    pub fn raw_put(&self, path: &str, bytes: Bytes) {
        let mut s = self.state.lock();
        let mut p = parent(path);
        while let Some(dir) = p {
            s.dirs.insert(dir.to_string());
            p = parent(dir);
        }
        s.write(path, bytes);
    }

    /// 直接删除文件（不计请求、不触发注入）
    pub fn raw_remove(&self, path: &str) -> bool {
        self.state.lock().files.remove(path).is_some()
    }

    /// 计数、查询注入与延迟；返回需要的延迟与注入的失败
    async fn begin(&self, op: Op, path: &str) -> Option<Fault> {
        let (latency, fault) = {
            let mut s = self.state.lock();
            s.requests += 1;
            s.per_op[op.index()] += 1;
            if let Some(log) = s.log.as_mut() {
                log.push((op, path.to_string()));
            }
            (s.latency, s.fault.clone())
        };
        if let Some(latency) = latency {
            tokio::time::sleep(latency).await;
        }
        fault.and_then(|hook| hook(op, path))
    }

    /// 执行一次操作，按注入的失败决定是否生效、是否报错
    async fn run<T>(
        &self,
        op: Op,
        path: &str,
        f: impl FnOnce(&mut State) -> Result<T, RemoteError>,
    ) -> Result<T, RemoteError> {
        match self.begin(op, path).await {
            Some(Fault::Before(e)) => Err(e),
            Some(Fault::After(e)) => {
                f(&mut self.state.lock())?;
                Err(e)
            }
            None => f(&mut self.state.lock()),
        }
    }
}

impl Remote for MemoryRemote {
    async fn list(&self, dir: &str) -> Result<Option<Vec<Entry>>, RemoteError> {
        self.run(Op::List, dir, |s| {
            if !s.dir_exists(dir) {
                return Ok(None);
            }
            let metadata = s.metadata;
            let mut out: Vec<Entry> = s
                .dirs
                .iter()
                .filter(|d| parent(d) == Some(dir))
                .map(|d| Entry {
                    name: name_of(d).to_string(),
                    is_dir: true,
                    etag: None,
                    size: None,
                    modified_ms: None,
                })
                .collect();
            out.extend(
                s.files
                    .iter()
                    .filter(|(p, _)| parent(p) == Some(dir))
                    .map(|(p, f)| Entry {
                        name: name_of(p).to_string(),
                        is_dir: false,
                        etag: (metadata == Metadata::Full).then(|| f.etag.clone()),
                        size: (metadata != Metadata::None).then(|| f.bytes.len() as u64),
                        modified_ms: (metadata != Metadata::None).then_some(f.modified_ms),
                    }),
            );
            Ok(Some(out))
        })
        .await
    }

    async fn get(&self, path: &str) -> Result<Option<Bytes>, RemoteError> {
        self.run(Op::Get, path, |s| Ok(s.files.get(path).map(|f| f.bytes.clone())))
            .await
    }

    async fn get_to_file(&self, path: &str, dest: &Path) -> Result<bool, RemoteError> {
        let bytes = self
            .run(Op::Get, path, |s| Ok(s.files.get(path).map(|f| f.bytes.clone())))
            .await?;
        match bytes {
            Some(bytes) => {
                tokio::fs::write(dest, &bytes).await?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn put(&self, path: &str, body: Bytes) -> Result<PutInfo, RemoteError> {
        self.run(Op::Put, path, |s| {
            if !s.parent_ok(path) {
                return Err(RemoteError::Status(409));
            }
            Ok(s.write(path, body))
        })
        .await
    }

    async fn put_file(&self, path: &str, src: &Path, len: u64) -> Result<PutInfo, RemoteError> {
        let bytes = tokio::fs::read(src).await?;
        if bytes.len() as u64 != len {
            return Err(RemoteError::Io(std::io::ErrorKind::InvalidData));
        }
        self.put(path, Bytes::from(bytes)).await
    }

    async fn delete(&self, path: &str) -> Result<(), RemoteError> {
        self.run(Op::Delete, path, |s| {
            let trimmed = path.trim_end_matches('/');
            if path.ends_with('/') || s.dirs.contains(trimmed) {
                s.files.retain(|p, _| !is_under(p, trimmed));
                s.dirs.retain(|d| d != trimmed && !is_under(d, trimmed));
            } else {
                s.files.remove(trimmed);
            }
            Ok(())
        })
        .await
    }

    async fn mkcol(&self, path: &str) -> Result<(), RemoteError> {
        self.run(Op::Mkcol, path, |s| {
            if s.dir_exists(path) {
                return Ok(());
            }
            if !s.parent_ok(path) {
                return Err(RemoteError::Status(409));
            }
            s.dirs.insert(path.to_string());
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn weakest_server_semantics() {
        let r = MemoryRemote::new();
        assert_eq!(r.list("").await.unwrap(), None);
        assert_eq!(
            r.put("a/x", Bytes::from_static(b"1")).await,
            Err(RemoteError::Status(409))
        );
        assert_eq!(r.mkcol("a").await, Err(RemoteError::Status(409)));
        r.mkcol("").await.unwrap();
        r.mkcol("a").await.unwrap();
        r.mkcol("a").await.unwrap();
        let first = r.put("a/x", Bytes::from_static(b"1")).await.unwrap();
        let second = r.put("a/x", Bytes::from_static(b"22")).await.unwrap();
        assert!(first.etag.is_some() && first.etag != second.etag);
        assert_eq!(r.get("a/x").await.unwrap().as_deref(), Some(&b"22"[..]));
        assert_eq!(r.get("a/y").await.unwrap(), None);

        let root = r.list("").await.unwrap().unwrap();
        assert_eq!(root.len(), 1);
        assert!(root[0].is_dir && root[0].name == "a");
        let a = r.list("a").await.unwrap().unwrap();
        assert_eq!(a[0].name, "x");
        assert_eq!(a[0].etag, second.etag);
        assert_eq!(a[0].size, Some(2));

        r.delete("a/missing").await.unwrap();
        r.delete("a/").await.unwrap();
        assert!(r.paths().is_empty());
        assert_eq!(r.list("a").await.unwrap(), None);
    }

    #[tokio::test]
    async fn fault_injection_before_and_after() {
        let r = MemoryRemote::new();
        r.mkcol("").await.unwrap();
        let hook: FaultHook = Arc::new(|op, path| match (op, path) {
            (Op::Put, "lost") => Some(Fault::After(RemoteError::Timeout)),
            (Op::Put, "refused") => Some(Fault::Before(RemoteError::Status(503))),
            _ => None,
        });
        r.set_fault(Some(hook));
        assert_eq!(r.put("lost", Bytes::from_static(b"x")).await, Err(RemoteError::Timeout));
        assert_eq!(r.raw_get("lost").as_deref(), Some(&b"x"[..]));
        assert_eq!(
            r.put("refused", Bytes::from_static(b"x")).await,
            Err(RemoteError::Status(503))
        );
        assert_eq!(r.raw_get("refused"), None);
        assert_eq!(r.requests(), 3);
    }

    #[tokio::test]
    async fn per_op_counters_and_log() {
        let r = MemoryRemote::new();
        r.set_request_log(true);
        r.mkcol("").await.unwrap();
        r.put("a", Bytes::from_static(b"1")).await.unwrap();
        r.get("a").await.unwrap();
        r.get("b").await.unwrap();
        r.list("").await.unwrap();
        assert_eq!(r.requests(), 5);
        assert_eq!(r.requests_of(Op::Get), 2);
        assert_eq!(r.requests_of(Op::Put), 1);
        assert_eq!(r.requests_of(Op::Delete), 0);
        let log = r.take_request_log();
        assert_eq!(log.len(), 5);
        assert_eq!(log[0], (Op::Mkcol, String::new()));
        assert_eq!(log[3], (Op::Get, "b".to_string()));
        assert!(r.take_request_log().is_empty());
        r.reset_requests();
        assert_eq!((r.requests(), r.requests_of(Op::Get)), (0, 0));
        r.set_request_log(false);
        r.get("a").await.unwrap();
        assert!(r.take_request_log().is_empty());
    }

    #[tokio::test]
    async fn metadata_modes() {
        let r = MemoryRemote::new();
        r.set_lenient_parents(true);
        r.set_metadata(Metadata::NoEtag);
        let info = r.put("d/f", Bytes::from_static(b"abc")).await.unwrap();
        assert_eq!(info.etag, None);
        let e = &r.list("d").await.unwrap().unwrap()[0];
        assert_eq!((e.etag.as_deref(), e.size), (None, Some(3)));
        assert!(e.modified_ms.is_some());
        r.set_metadata(Metadata::None);
        let e = &r.list("d").await.unwrap().unwrap()[0];
        assert_eq!((e.size, e.modified_ms), (None, None));
    }

    #[tokio::test]
    async fn file_transfer() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dest = dir.path().join("dest");
        std::fs::write(&src, b"payload").unwrap();
        let r = MemoryRemote::new();
        r.mkcol("").await.unwrap();
        r.put_file("f", &src, 7).await.unwrap();
        assert!(r.get_to_file("f", &dest).await.unwrap());
        assert_eq!(std::fs::read(&dest).unwrap(), b"payload");
        assert!(!r.get_to_file("missing", &dir.path().join("none")).await.unwrap());
        assert!(!dir.path().join("none").exists());
        assert!(r.put_file("g", &src, 8).await.is_err());
    }
}
