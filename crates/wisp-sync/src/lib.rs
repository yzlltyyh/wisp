//! Wisp 的多设备同步。
//!
//! 经用户自己的 WebDAV 存储，在多台设备之间双向同步配置（设置、模型服务、风格、术语表、API Key）
//! 与使用数据（剪贴板历史、识别历史）。数据在本机加密后才上传，服务器只见密文；没有中转服务器。
//!
//! ## 模型
//!
//! - 每个可同步的值是一个**寄存器**：`(域, key) → 值`，带混合逻辑时间戳 [`Hlc`]；合并是逐 key 的
//!   last-writer-wins，胜负按 `(t, device_id)` 字典序，因此满足交换律、结合律与幂等，任意顺序、重复接收都收敛。
//! - 每台设备只写自己的文件（很多 WebDAV 服务器忽略条件请求，无法安全地并发改同一个文件）：
//!   一份不可变**日志段**序列加一个 **head**（设备信息与段清单）。读者按每台设备的游标增量读取。
//! - 本机副本 `sync.db` 只保留每个 key 的胜出记录、读取游标与本机日志账本（[`replica`]）。
//! - 配置文件经 [`projection`] 拆成寄存器；剪贴板与识别历史按条目（内容 hash / 记录 id）成为寄存器，
//!   清空用带时间戳的 `@clear` 寄存器表达；图片等大对象作为加密 blob 单独存放，只有上传者写和删。
//!
//! ## 模块
//!
//! | 模块 | 内容 |
//! | --- | --- |
//! | [`crypto`] | PBKDF2 主密钥、HKDF 子密钥、ChaCha20-Poly1305 小文件封装与分块流式封装、blob 命名、口令规则 |
//! | [`hlc`] | 混合逻辑时钟（原子，O(1)） |
//! | [`model`] | 记录、日志段、head、vault.json、设备信息与 JSON + deflate 编解码 |
//! | [`layout`] | 远端目录布局与加密用的逻辑名 |
//! | [`remote`] | [`Remote`] trait、[`WebDav`] 实现、测试用的 [`MemoryRemote`] |
//! | [`replica`] | 本机副本 sync.db 与改动日志写入接口 [`Journal`] |
//! | [`projection`] | settings / styles / glossary 的投影与反投影 |
//! | [`engine`] | 连接、同步循环、清除服务器数据；桌面端实现的 [`LocalStore`] |
//!
//! ## 资源与隐私
//!
//! 引擎不起常驻任务、不设计时器，只在调用方发起的操作里联网；可能阻塞的工作（SQLite、文件读写、
//! 密钥派生、本机存储读写）在工作线程里完成。错误与日志只含固定词汇（错误种类、HTTP 状态码、计数），
//! 不含服务器地址、账号、口令、密钥、远端路径、设备名与任何记录内容。
//!
//! 本 crate 不依赖 Tauri，只横向依赖 `wisp-types`。
//!
//! ## 桌面端接入
//!
//! 1. 同步开启时（托盘就绪后，不联网）：[`Engine::open`]（sync.db、spool 目录、本机设备信息）。
//!    [`Engine::hlc`] 给钩子与 sink 分配 t（在各自的锁内调用 `now()`，O(1)）；[`Engine::journal`] 给日志写入线程，
//!    它把 `(d, k, t, 值)` 经 [`Journal::record`] 写进 sync.db（同时更新配置影子）。
//! 2. 记录本机改动的约定：
//!    - 配置：投影 key → `Set(值)` / `Delete`；API Key：`Deferred`（明文不进 sync.db，推送时经 `export` 读取）；
//!    - 剪贴板：新复制与再次复制 → `c/<hash>` `Deferred`；收藏、取消收藏与命名 →
//!      `p/<hash>` `Set({"pin": bool, "name"?: string})`（[`model::PinValue`]）；
//!      用户删除 → `c/<hash>` `Delete`；清空 → `@clear/all` 或 `@clear/unpinned` `Set({"wall_ms": 当前毫秒})`；
//!    - 识别历史：新增 → `o/<id>` `Deferred`；删除与被替换 → `o/<id>` `Delete`；清空 → `@clear` `Set({"wall_ms"})`；
//!    - 粘贴、保留策略的删除都不记录。
//! 3. 连接：[`Engine::connect`] 返回后把 `vault_id` 与 `master_key` 连同 WebDAV 凭据存进钥匙串，随后在后台跑一轮
//!    [`CycleReason::Connect`]；之后的启动用 [`Engine::unlock`] 解锁。
//! 4. 每次触发调用 [`Engine::sync_cycle`]：`report.again` 为真时尽快再跑一轮；`report.new_device_id` 有值时写回本机运行态；
//!    出错时按 [`SyncError::kind`] / [`SyncError::is_transient`] 退避或暂停。钥匙串只在 `CycleOptions::keychain` 为真时读写。
//! 5. [`Engine::reapply`]：配置文件在启动时被创建或重置、指纹变化（升级 / 降级）、Linux 会话钥匙串重启后、
//!    剪贴板或识别历史的内容开关重新打开时调用。
//! 6. 应用远端值时，跳过「已经在本机改了、但还在日志写入线程队列里」的 key（或先把队列写完再应用）：
//!    副本只认识已写入的改动。
//! 7. 断开：[`Engine::close`] 后删除 sync.db（含 `-wal` / `-shm`）与 spool 目录。

pub mod crypto;
pub mod engine;
pub mod error;
pub mod hlc;
pub mod layout;
pub mod model;
pub mod projection;
pub mod remote;
pub mod replica;

pub use crypto::{Keys, MasterKey};
pub use engine::{
    ApplyOutcome, ConnectOutcome, Connected, ContentSwitches, CycleOptions, CycleReason, CycleReport, DeviceSummary,
    Engine, EngineStatus, Exported, KeyApply, LocalBlob, LocalEntry, LocalStore, ProbeReport, Progress, ProgressFn,
    RemoteChange, RetentionPolicy, SpooledBlob, VaultSummary, probe, reset_remote,
};
pub use error::{ErrorKind, LocalError, PassphraseIssue, RemoteError, ServerUrlError, SyncError};
pub use hlc::{Clock, Hlc};
pub use model::{
    BlobRef, BlobRole, DeviceInfo, Domain, HeadPayload, KdfParams, Record, SchemaVersions, SegmentEntry,
    SegmentPayload, VaultHeader,
};
pub use projection::ConfigDoc;
pub use remote::{DirCache, Entry, MemoryRemote, PutInfo, Remote, WebDav, validate_server_url};
pub use replica::{Journal, JournalEntry, JournalValue, Replica, Shadow, ShadowUpdate};
