//! 存储层错误。

/// 存储层错误
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("migration: {0}")]
    Migration(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("image: {0}")]
    Image(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// 指定 id 的条目不存在（`touch` / `set_pinned` 这类要求条目存在的操作）
    #[error("clip item {0} not found")]
    NotFound(i64),
    /// 库的 schema 版本比本程序新（用户从新版降级回来）。库文件原样保留、不重建；
    /// 调用方应停用剪贴板历史并提示用户升级（再次升级后历史完好如初）
    #[error("clipboard database schema v{found} is newer than supported v{supported}")]
    SchemaTooNew { found: i64, supported: u32 },
}

impl From<rusqlite_migration::Error> for StoreError {
    fn from(e: rusqlite_migration::Error) -> Self {
        match e {
            rusqlite_migration::Error::RusqliteError { err, .. } => Self::Sqlite(err),
            other => Self::Migration(other.to_string()),
        }
    }
}

impl StoreError {
    /// FTS 索引结构损坏（SQLITE_CORRUPT_VTAB）：重建索引即可恢复，不影响 items
    pub(crate) fn is_fts_corruption(&self) -> bool {
        matches!(self, Self::Sqlite(rusqlite::Error::SqliteFailure(e, _)) if e.extended_code == rusqlite::ffi::SQLITE_CORRUPT_VTAB)
    }

    /// 数据库文件损坏（SQLITE_CORRUPT / SQLITE_NOTADB，FTS 索引损坏除外）
    pub(crate) fn is_db_corruption(&self) -> bool {
        !self.is_fts_corruption()
            && matches!(self, Self::Sqlite(e) if matches!(
                e.sqlite_error_code(),
                Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
            ))
    }

    /// 打开阶段的错误能否靠「备份旧库并新建」解决：文件损坏、不是数据库、结构 / 迁移 SQL 失败可以；
    /// 版本比本程序新不是损坏（[`Self::SchemaTooNew`]，原样保留）；打不开文件、权限、只读、磁盘满、I/O 错误、
    /// 被其他进程锁住也不行（新建同样会失败，或会误删正在使用的库）
    pub(crate) fn is_recoverable_by_recreate(&self) -> bool {
        use rusqlite::ErrorCode as C;
        match self {
            // 迁移框架自身的错误：user_version 为负等无法解释的版本号（迁移定义错误由单测保证不会出现）
            Self::Migration(_) => true,
            // 只认 SQLite 引擎报告的错误；rusqlite 自身的类型转换错误属于程序缺陷，不能拿用户数据兜底
            Self::Sqlite(e) => e.sqlite_error_code().is_some_and(|code| {
                !matches!(
                    code,
                    C::CannotOpen
                        | C::PermissionDenied
                        | C::ReadOnly
                        | C::DiskFull
                        | C::SystemIoFailure
                        | C::DatabaseBusy
                        | C::DatabaseLocked
                        | C::OutOfMemory
                        | C::OperationInterrupted
                )
            }),
            _ => false,
        }
    }
}
