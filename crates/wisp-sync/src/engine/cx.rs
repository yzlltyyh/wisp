//! 一轮同步（或一次连接）的上下文：计数的远端包装、工作线程辅助、会话密钥、进程内状态。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use bytes::Bytes;

use super::{CycleOptions, CycleReport, Engine, LocalStore, Progress, ProgressFn};
use crate::crypto::Keys;
use crate::error::{LocalError, RemoteError, SyncError};
use crate::remote::{DirCache, Entry, PutInfo, Remote};

/// 解锁后的会话密钥
pub(super) struct Session {
    pub vault_id: String,
    pub keys: Keys,
}

/// 进程内状态（随引擎存活，不持久化）；由同步锁保护，一轮同步期间独占
#[derive(Debug, Default)]
pub(super) struct CycleState {
    /// 已确认存在的远端目录（每个进程每个目录最多 MKCOL 一次）
    pub dirs: DirCache,
    /// 本进程已经读过并核对过本机 head（克隆检测、进行中写入的补做）
    pub own_verified: bool,
    /// 换了新的 device_id，尚未通过报告交给调用方
    pub announce_id: Option<String>,
}

/// 计数的远端包装：统计请求数与字节数，写进本轮报告
pub(super) struct Counted<'a, R: Remote> {
    inner: &'a R,
    requests: AtomicU32,
    up: AtomicU64,
    down: AtomicU64,
}

impl<'a, R: Remote> Counted<'a, R> {
    pub fn new(inner: &'a R) -> Self {
        Counted {
            inner,
            requests: AtomicU32::new(0),
            up: AtomicU64::new(0),
            down: AtomicU64::new(0),
        }
    }

    fn tick(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn requests(&self) -> u32 {
        self.requests.load(Ordering::Relaxed)
    }

    pub fn bytes_up(&self) -> u64 {
        self.up.load(Ordering::Relaxed)
    }

    pub fn bytes_down(&self) -> u64 {
        self.down.load(Ordering::Relaxed)
    }
}

impl<R: Remote> Remote for Counted<'_, R> {
    async fn list(&self, dir: &str) -> Result<Option<Vec<Entry>>, RemoteError> {
        self.tick();
        self.inner.list(dir).await
    }

    async fn get(&self, path: &str) -> Result<Option<Bytes>, RemoteError> {
        self.tick();
        let out = self.inner.get(path).await?;
        if let Some(b) = &out {
            self.down.fetch_add(b.len() as u64, Ordering::Relaxed);
        }
        Ok(out)
    }

    async fn get_to_file(&self, path: &str, dest: &Path) -> Result<bool, RemoteError> {
        self.tick();
        let found = self.inner.get_to_file(path, dest).await?;
        if found && let Ok(meta) = tokio::fs::metadata(dest).await {
            self.down.fetch_add(meta.len(), Ordering::Relaxed);
        }
        Ok(found)
    }

    async fn put(&self, path: &str, body: Bytes) -> Result<PutInfo, RemoteError> {
        self.tick();
        let len = body.len() as u64;
        let out = self.inner.put(path, body).await?;
        self.up.fetch_add(len, Ordering::Relaxed);
        Ok(out)
    }

    async fn put_file(&self, path: &str, src: &Path, len: u64) -> Result<PutInfo, RemoteError> {
        self.tick();
        let out = self.inner.put_file(path, src, len).await?;
        self.up.fetch_add(len, Ordering::Relaxed);
        Ok(out)
    }

    async fn delete(&self, path: &str) -> Result<(), RemoteError> {
        self.tick();
        self.inner.delete(path).await
    }

    async fn mkcol(&self, path: &str) -> Result<(), RemoteError> {
        self.tick();
        self.inner.mkcol(path).await
    }
}

/// 在工作线程里执行阻塞工作（SQLite、文件、加解密、本机存储）
pub(super) async fn blocking<T, F>(f: F) -> Result<T, SyncError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| SyncError::Internal("worker task failed"))
}

/// 一轮同步的上下文
pub(super) struct Cx<'a, R: Remote, L: LocalStore + ?Sized> {
    pub engine: &'a Engine,
    pub remote: Counted<'a, R>,
    pub local: &'a Arc<L>,
    pub options: &'a CycleOptions,
    pub progress: &'a ProgressFn,
    pub session: Arc<Session>,
    /// 本机 device_id（克隆检测后会更换）
    pub own: String,
    pub report: CycleReport,
    /// 本轮已经 GET 过 vault.json 并确认没有变化
    pub vault_confirmed: bool,
    /// 本轮剩余的 blob 下载 / 上传额度
    pub down_budget: u64,
    pub up_budget: u64,
    /// 钥匙串被拒绝：本轮其余工作照常，结束时报告
    pub keychain_error: bool,
    /// 附件上传失败（暂时性）：本轮其余工作照常，结束时报告
    pub blob_error: Option<SyncError>,
    /// 本轮列目录时本机 head 在服务器上
    pub own_head_listed: bool,
    /// 本轮写过本机 head
    pub head_written: bool,
    /// 进度：本轮需要处理的总数与已完成数
    pub progress_total: u32,
    pub progress_done: u32,
}

impl<'a, R: Remote, L: LocalStore + ?Sized> Cx<'a, R, L> {
    pub fn new(
        engine: &'a Engine,
        remote: &'a R,
        local: &'a Arc<L>,
        options: &'a CycleOptions,
        progress: &'a ProgressFn,
        session: Arc<Session>,
        own: String,
    ) -> Self {
        Cx {
            engine,
            remote: Counted::new(remote),
            local,
            options,
            progress,
            session,
            own,
            report: CycleReport::default(),
            vault_confirmed: false,
            down_budget: super::CYCLE_BLOB_BUDGET_BYTES,
            up_budget: super::CYCLE_BLOB_BUDGET_BYTES,
            keychain_error: false,
            blob_error: None,
            own_head_listed: false,
            head_written: false,
            progress_total: 0,
            progress_done: 0,
        }
    }

    pub fn vault_id(&self) -> &str {
        &self.session.vault_id
    }

    /// 在工作线程里读写副本
    pub async fn db<T, F>(&self, f: F) -> Result<T, SyncError>
    where
        T: Send + 'static,
        F: FnOnce(&mut crate::replica::Replica) -> Result<T, SyncError> + Send + 'static,
    {
        self.engine.db(f).await
    }

    /// 在工作线程里调用本机存储
    pub async fn local_call<T, F>(&self, f: F) -> Result<Result<T, LocalError>, SyncError>
    where
        T: Send + 'static,
        F: FnOnce(&L) -> Result<T, LocalError> + Send + 'static,
    {
        let local = Arc::clone(self.local);
        blocking(move || f(&local)).await
    }

    /// 增加本轮的工作总量（进度条的分母）
    pub fn add_work(&mut self, n: u32) {
        self.progress_total = self.progress_total.saturating_add(n);
        self.emit_progress();
    }

    pub fn work_done(&mut self, n: u32) {
        self.progress_done = self.progress_done.saturating_add(n).min(self.progress_total);
        self.emit_progress();
    }

    fn emit_progress(&self) {
        if self.progress_total > 0 {
            (self.progress)(Progress {
                done: self.progress_done,
                total: self.progress_total,
            });
        }
    }

    /// spool 目录下一个新的临时文件路径（不创建文件）
    pub fn spool_file(&self, tag: &str) -> PathBuf {
        self.engine.spool_file(tag)
    }

    /// 把计数写进报告
    pub fn finish_counts(&mut self) {
        self.report.requests = self.remote.requests();
        self.report.bytes_up = self.remote.bytes_up();
        self.report.bytes_down = self.remote.bytes_down();
    }
}

/// 远端条目的签名：有 ETag 用 ETag，否则用大小 + 修改时间；都没有时为 `None`（每次都要重新读取）
pub(super) fn sig_of(e: &Entry) -> Option<String> {
    if let Some(etag) = &e.etag {
        return Some(format!("e:{etag}"));
    }
    match (e.size, e.modified_ms) {
        (Some(size), Some(ms)) => Some(format!("s:{size}:{ms}")),
        _ => None,
    }
}

/// PUT 响应里的 ETag 转成签名（与 [`sig_of`] 的格式一致）
pub(super) fn sig_of_put(info: &PutInfo) -> Option<String> {
    info.etag.as_ref().map(|e| format!("e:{e}"))
}

/// 删除临时文件；不存在也算成功
pub(super) fn remove_quietly(path: &Path) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        log::debug!("sync: spool cleanup failed: {:?}", e.kind());
    }
}
