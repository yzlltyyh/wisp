//! 同步引擎：连接（创建 / 解锁 / 加入 / 恢复）、同步循环（拉取 → 合并 → 应用 → 推送 → 压缩）、清除服务器数据。
//!
//! 引擎不认识具体的配置文件、剪贴板库或钥匙串：这些由桌面端实现 [`LocalStore`]（阻塞方法，
//! 引擎在 `spawn_blocking` 里调用）。引擎本身不起任务、不设计时器，只在调用方发起的
//! [`Engine::sync_cycle`] 里联网；同一时刻只跑一轮（内部串行化，重叠的调用排队）。
//!
//! 一轮的步骤：
//! 1. 列同步根目录（一次请求拿到 vault.json 与全部 head 的 ETag）→ 复核 vault → 只取变化的 head → 取新段 →
//!    逐 key LWW 合并进副本（标记「未应用」），游标与合并在同一个事务里提交；
//! 2. 应用全部「未应用」的胜出记录（不限本轮），顺序 cfg → sty / glo → key → clip → ocr，
//!    [`LocalStore::apply`] 确认后才标记已应用；本机有更新的待上传记录的 key 跳过；
//! 3. 分批推送：配置与 Key → 剪贴板文本（新到旧）→ 图片与识别记录（新到旧，每批 blob 有上限），
//!    每批一个日志段 + 一次 head 写入；单轮 blob 总量有上限，超出时在报告里要求再来一轮；
//! 4. 视情况压缩本机日志（只重写自己的日志），回收不再被引用的 blob。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use serde_json::Value;
use wisp_types::OsKind;

use crate::crypto::MasterKey;
use crate::error::{LocalError, SyncError};
use crate::hlc::{self, Clock};
use crate::model::{BlobRole, DeviceInfo, Domain, SchemaVersions};
use crate::remote::Remote;
use crate::replica::{self, Journal, Replica};

mod apply;
mod compact;
mod content;
mod cx;
mod join;
mod pull;
mod push;

use cx::{Cx, CycleState, Session};

/// 单张图片（剪贴板图片、识别截图原图）同步的大小上限
pub const IMAGE_MAX_BYTES: u64 = 10 * 1024 * 1024;
/// 单轮上传的 blob 总量上限；超出后结束本轮并要求再来一轮，让新改动可以插队
pub const CYCLE_BLOB_BUDGET_BYTES: u64 = 50 * 1024 * 1024;
/// 每批推送最多携带的 blob 数
pub const BLOBS_PER_BATCH: usize = 8;
/// 日志段明文的目标上限（单条记录超过时独占一段）
pub const SEGMENT_MAX_PLAINTEXT_BYTES: usize = 1024 * 1024;
/// 本机日志段数超过这个值时压缩
pub const COMPACT_SEGMENT_LIMIT: usize = 16;
/// 本机日志字节数超过「上次压缩后 × 2 + 这个值」时压缩
pub const COMPACT_SLACK_BYTES: u64 = 256 * 1024;
/// 已发布的本机条目中超出保留策略的达到这个数时压缩。历史到达条数上限后每次复制都会挤出一条，
/// 逐条触发会让每次推送都整体重写一遍日志；攒够一批再裁（其他原因触发的压缩也会顺带裁掉）
pub const COMPACT_RETENTION_BATCH: usize = 64;

// ---------------------------------------------------------------------------------------------
// LocalStore：桌面端实现
// ---------------------------------------------------------------------------------------------

/// 本机记录引用的一个本地文件（推送时由引擎加密上传）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalBlob {
    pub role: BlobRole,
    /// 内容身份（剪贴板图片是条目 hash）；引擎据此得出 blob 名，同一内容只上传一次
    pub identity: Vec<u8>,
    /// 明文文件路径（引擎只读，不移动不删除）
    pub path: PathBuf,
    /// 文件字节数
    pub bytes: u64,
}

/// [`LocalStore::export`] 的结果：记录的当前值与它引用的本地文件
#[derive(Debug, Clone, PartialEq)]
pub struct Exported {
    pub value: Value,
    pub blobs: Vec<LocalBlob>,
}

/// [`LocalStore::enumerate`] 的一项：加入同步时本机该域的一条记录
#[derive(Debug, Clone, PartialEq)]
pub struct LocalEntry {
    pub key: String,
    /// 值；`None` 表示推送时再经 [`LocalStore::export`] 读取（剪贴板 / 识别正文、API Key）
    pub value: Option<Value>,
    /// 条目自身的时间（毫秒）：剪贴板 `c` / `p` 用 `last_used_at`，识别记录用 `created_at`；配置类为 `None`
    pub item_ms: Option<i64>,
    /// 配置标量：值是否等于内置默认值（见 [`crate::projection::is_default`]）
    pub is_default: bool,
    /// 是否是集合元素（provider、自定义风格、风格覆盖、排除应用、按键映射、术语、Key；
    /// 见 [`crate::projection::is_element_key`]）。元素与标量的加入规则不同
    pub element: bool,
}

/// 引擎已下载并解密到 spool 目录的 blob。适配器可以把文件移走（rename）；
/// [`LocalStore::apply`] 返回后仍留在原处的文件由引擎删除
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpooledBlob {
    pub role: BlobRole,
    pub path: PathBuf,
    pub bytes: u64,
}

/// 交给 [`LocalStore::apply`] 的一项远端变更。
///
/// 寄存器式的域（cfg / sty / glo / key）只会收到 [`Set`](Self::Set) / [`Delete`](Self::Delete)；
/// clip / ocr 收到的是引擎根据 `c` / `p` / `o` / `@clear` 寄存器与清空判定式**算好的逐条变更**，
/// 适配器不需要理解寄存器。
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteChange {
    /// cfg / sty / glo / key：寄存器取新值。cfg / sty / glo 的 key 是投影 key（交给 `projection::unproject`）；
    /// glo 的 key 是规范化后的 source，值 `{target, note}`；key 域的 key 是 provider id，值 `{key, origin}`
    Set { key: String, value: Value },
    /// cfg / sty / glo / key：寄存器被删除（集合元素、术语或 Key 被删除）
    Delete { key: String },
    /// clip：条目出现，或内容记录有了更新的版本。`value` 是 `c/<hash>` 的值
    /// （`{kind, created_at, last_used_at, text?, html?, rtf_b64?, image?, src?, truncated, exp?}`）；
    /// 本机已有该 hash 时内容不变，`created_at` 取较小、`last_used_at` 取较大。
    /// `pinned` 为 `None` 时保持本机的收藏状态与名称；为 `Some(true)` 时名称随之换成 `name`（None = 没有名称）。
    /// 图片条目的原图在 `blobs` 里（[`BlobRole::Image`]）
    ClipUpsert {
        hash: [u8; 32],
        value: Value,
        pinned: Option<bool>,
        name: Option<String>,
        blobs: Vec<SpooledBlob>,
    },
    /// clip：只有收藏状态或名称变化（`p/<hash>` 的值，见 [`crate::model::PinValue`]）。收藏时名称换成 `name`，
    /// 取消收藏时名称一并去掉；本机没有该条目时忽略，报 `Applied`
    ClipPin {
        hash: [u8; 32],
        pinned: bool,
        name: Option<String>,
    },
    /// clip：删除这些条目（用户删除的墓碑，以及按清空判定式算出的、已不存活的条目）；本机没有的忽略
    ClipDelete { hashes: Vec<[u8; 32]> },
    /// clip：清空在本机生效到没有同步寄存器的条目（文件条目、超限图片等）：
    /// 删除 `last_used_at < wall_ms` 的条目，`keep_pinned` 时保留置顶条目，`exclude` 中的 hash 一律保留
    /// （它们有寄存器，存活与否已由引擎逐条判定）
    ClipDeleteUnsyncedBefore {
        wall_ms: i64,
        keep_pinned: bool,
        exclude: Vec<[u8; 32]>,
    },
    /// ocr：识别记录出现。`id` 是 32 位十六进制，`value` 是 `{created_at, mode, model, text}`；
    /// `blobs` 含缩略图（[`BlobRole::Thumbnail`]，总是有）与原图（[`BlobRole::Image`]，可能没有）
    OcrUpsert {
        id: String,
        value: Value,
        blobs: Vec<SpooledBlob>,
    },
    /// ocr：删除这些识别记录（用户删除、被替换、按清空判定式算出的）；本机没有的忽略
    OcrDelete { ids: Vec<String> },
    /// ocr：清空在本机生效到没有同步寄存器的记录：删除 `created_at < wall_ms` 的，`exclude` 中的保留
    OcrDeleteUnsyncedBefore { wall_ms: i64, exclude: Vec<String> },
}

/// 单项变更的应用结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyApply {
    /// 已写入本机（或本机已是这个状态）
    Applied,
    /// 暂时无法应用（配置文件当前是无效 JSON、剪贴板库未打开、钥匙串被拒绝等），保持「未应用」，以后重试
    Deferred,
    /// 被本机规则拒绝（隐私规则、类型开关、大小上限、保留策略、识别历史关闭、Key 的 origin 不匹配）：
    /// 记下这个版本，同一 key 出现更新的版本才重新尝试
    Evicted,
}

/// [`LocalStore::apply`] 的结果：`results[i]` 对应传入的第 i 项变更
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyOutcome {
    pub results: Vec<KeyApply>,
}

impl ApplyOutcome {
    /// `n` 项变更都是同一个结果（例如一个配置文件整体写入成功或被推迟）
    pub fn uniform(n: usize, result: KeyApply) -> ApplyOutcome {
        ApplyOutcome {
            results: vec![result; n],
        }
    }
}

/// 本机数据的读写接口，由桌面端实现。方法都是阻塞的，引擎在 `spawn_blocking` 里调用；
/// 只会以本版本认识的域调用。应用路径写入本机时不得再触发改动捕获（按代码路径防回声）
pub trait LocalStore: Send + Sync + 'static {
    /// 推送时读取本机记录的当前值（日志里记为 [`JournalValue::Deferred`](crate::replica::JournalValue::Deferred) 的记录）；
    /// `None` = 本机已没有，不发布
    fn export(&self, d: Domain, key: &str) -> Result<Option<Exported>, LocalError>;

    /// 应用同一域的一批变更（blob 已由引擎下载解密到 spool 文件）。返回值与 `changes` 一一对应
    fn apply(&self, d: Domain, changes: Vec<RemoteChange>) -> Result<ApplyOutcome, LocalError>;

    /// 加入同步时枚举本机该域的全部记录（含条目自身时间、是否等于默认值、是否是集合元素）
    fn enumerate(&self, d: Domain) -> Result<Vec<LocalEntry>, LocalError>;

    /// 该域的本机存储眼下能否写入（例如剪贴板库还没打开或打不开时为 `false`）。不可用时引擎不为它下载附件，
    /// 记录留在「未应用」，也不要求立即再来一轮。默认总是可用
    fn available(&self, d: &Domain) -> bool {
        let _ = d;
        true
    }
}

// ---------------------------------------------------------------------------------------------
// 选项与报告
// ---------------------------------------------------------------------------------------------

/// 发起一轮同步的原因。除 [`LocalChange`](Self::LocalChange) 外都算用户活动
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CycleReason {
    /// 本机改动（去抖之后）
    LocalChange,
    /// 剪贴板视图显示：本轮先应用 clip 域
    ClipboardShown,
    /// 其他面板视图显示
    PanelShown,
    /// 托盘菜单打开
    TrayOpened,
    /// 设置窗口获得焦点
    SettingsFocused,
    /// 再次启动 / Reopen
    Relaunch,
    /// 「立即同步」
    Manual,
    /// 连接之后在后台继续的首轮（剪贴板与识别历史的加入、上传与下载）
    Connect,
    /// 用户修改了同步设置（内容开关等）
    SettingsChanged,
}

impl CycleReason {
    /// 是否由用户活动触发（钥匙串只在这类循环里首次读取，远端 Key 也只在这类循环里写入）
    pub fn is_user_activity(self) -> bool {
        !matches!(self, Self::LocalChange)
    }

    /// 日志用的短名
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LocalChange => "local_change",
            Self::ClipboardShown => "clipboard_shown",
            Self::PanelShown => "panel_shown",
            Self::TrayOpened => "tray_opened",
            Self::SettingsFocused => "settings_focused",
            Self::Relaunch => "relaunch",
            Self::Manual => "manual",
            Self::Connect => "connect",
            Self::SettingsChanged => "settings_changed",
        }
    }
}

/// 「同步内容」开关（与设置里的 `sync.content` 对应）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentSwitches {
    /// 设置、模型服务、风格、术语表
    pub settings: bool,
    /// API Key（`settings` 关闭时无效）
    pub api_keys: bool,
    pub clipboard: bool,
    /// 剪贴板图片与识别截图
    pub images: bool,
    /// 识别历史；调用方应传「同步开关 && 本机开启了识别历史」
    pub ocr_history: bool,
}

impl Default for ContentSwitches {
    fn default() -> Self {
        ContentSwitches {
            settings: true,
            api_keys: true,
            clipboard: true,
            images: true,
            ocr_history: true,
        }
    }
}

impl ContentSwitches {
    /// 该域当前是否参与推送与应用
    pub fn allows(&self, d: &Domain) -> bool {
        match d {
            Domain::Cfg | Domain::Sty | Domain::Glo => self.settings,
            Domain::Key => self.settings && self.api_keys,
            Domain::Clip => self.clipboard,
            Domain::Ocr => self.ocr_history,
            Domain::Unknown(_) => false,
        }
    }
}

/// 已同步的保留策略（全局设置，所有设备最终一致）；压缩时据此裁剪服务器上的历史
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// `clipboard.max_items`
    pub clip_max_items: u32,
    /// `clipboard.max_age_days`（`None` = 不按时间清理）
    pub clip_max_age_days: Option<u32>,
    /// `ocr.history_limit`
    pub ocr_history_limit: u32,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        RetentionPolicy {
            clip_max_items: 1000,
            clip_max_age_days: None,
            ocr_history_limit: 50,
        }
    }
}

/// 一轮同步（以及连接）的选项，由调用方按当前设置填写
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleOptions {
    pub content: ContentSwitches,
    pub retention: RetentionPolicy,
    /// 单张图片的上限（默认 [`IMAGE_MAX_BYTES`]）
    pub images_max_bytes: u64,
    /// 本轮是否允许读写钥匙串（用户活动触发，或凭据已缓存在内存）；为 `false` 时 key 域不推送也不应用
    pub keychain: bool,
    /// 当前各配置文件的 schema_version（写进日志段）
    pub schema: SchemaVersions,
}

impl Default for CycleOptions {
    fn default() -> Self {
        CycleOptions {
            content: ContentSwitches::default(),
            retention: RetentionPolicy::default(),
            images_max_bytes: IMAGE_MAX_BYTES,
            keychain: false,
            schema: SchemaVersions::default(),
        }
    }
}

/// 进度（例如加入时上传 / 下载剪贴板历史）
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Progress {
    pub done: u32,
    pub total: u32,
}

/// 进度回调；在引擎的任务里同步调用，不能阻塞
pub type ProgressFn = dyn Fn(Progress) + Send + Sync;

/// 一轮同步的结果（只含计数，不含任何内容，可以写进日志）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CycleReport {
    /// 合并进副本的远端记录数
    pub pulled: u32,
    /// 本轮成功应用到本机的记录数
    pub applied: u32,
    /// 仍在等待应用的记录数（被推迟）
    pub deferred: u32,
    /// 本轮上传的记录数
    pub pushed: u32,
    /// 本轮发出的请求数
    pub requests: u32,
    pub bytes_up: u64,
    pub bytes_down: u64,
    /// 本轮的 blob 上传 / 下载额度用完、还有剩余工作，调用方应尽快再跑一轮。
    /// 失败不会让它为真：附件上传失败时本轮返回暂时性错误，由调用方退避
    pub again: bool,
    /// 结束时仍有待上传的本机改动
    pub pending: bool,
    pub compacted: bool,
    /// 本轮因解密或解析失败而跳过的远端设备数
    pub corrupt_devices: u32,
    /// vault 中的设备数（含本机）
    pub device_count: u32,
    /// 检测到数据目录被复制到了另一台机器，本机换了新的 device_id：调用方写回本机运行态并在之后使用
    pub new_device_id: Option<String>,
}

/// 连接的方式（由服务器与本机状态决定，调用方不指定）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectOutcome {
    /// 服务器上没有同步数据：创建并发布本机数据
    Created,
    /// 服务器已有数据、本机没有对应副本：合并，服务器优先，本机独有的数据保留
    Joined,
    /// 本机副本属于服务器上的这份数据：只更新凭据并解锁
    Restored,
}

/// 连接成功的结果。调用方把 `vault_id` 与 `master_key` 连同 WebDAV 凭据存进钥匙串；
/// 之后启动时用 [`Engine::unlock`] 解锁，不再需要口令
#[derive(Debug)]
pub struct Connected {
    pub outcome: ConnectOutcome,
    pub vault_id: String,
    pub master_key: MasterKey,
}

/// 「测试连接」看到的服务器上的同步数据
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultSummary {
    /// 格式与 KDF 参数是否受本版本支持
    pub supported: bool,
    pub device_count: u32,
    /// 最近一次有设备写入的时间
    pub last_modified_ms: Option<i64>,
    /// 本机副本属于这份数据（可以直接「重新连接」）
    pub restorable: bool,
}

/// [`probe`] 的结果：认证与读写都已通过
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    /// 服务器上没有同步数据时为 `None`
    pub vault: Option<VaultSummary>,
}

/// 设备列表的一项
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSummary {
    pub id: String,
    pub name: String,
    pub os: OsKind,
    pub app_version: String,
    /// 该设备 head 最近一次更新的时间
    pub last_seen_ms: i64,
    pub this_device: bool,
}

/// 引擎的本地状态（不联网）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineStatus {
    pub vault_id: Option<String>,
    pub unlocked: bool,
    /// 有待上传的本机改动
    pub pending: bool,
    pub device_count: u32,
    pub last_ok_ms: Option<i64>,
    /// 剪贴板 / 识别历史的加入尚未完成
    pub joining: bool,
}

// ---------------------------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------------------------

/// 同步引擎。`Send + Sync`，可放进 `Arc` 在多个任务间共享
pub struct Engine {
    db_path: PathBuf,
    spool_dir: PathBuf,
    /// 引擎自己的副本连接；只在工作线程里使用，锁不会跨 `.await` 持有
    replica: Arc<Mutex<Replica>>,
    clock: Arc<Clock>,
    device: Mutex<DeviceInfo>,
    session: Mutex<Option<Arc<Session>>>,
    /// 同一时刻只跑一轮同步 / 连接；锁里是进程内状态（目录缓存、本机 head 是否已核对）
    cycle: tokio::sync::Mutex<CycleState>,
    /// spool 临时文件的序号
    spool_seq: AtomicU64,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Engine(..)")
    }
}

impl Engine {
    /// 打开（不存在则创建）sync.db，从中恢复时钟。`spool_dir` 存放下载解密后、尚未交给本机存储的 blob
    /// 与上传前的密文临时文件（打开时清理残留）。`device` 的 id 来自本机运行态；若副本里记着另一个 id
    /// （上次换了新 id 但没来得及写回运行态），以副本为准，并在下一轮的报告里再次给出。阻塞，调用方放进工作线程
    pub fn open(db_path: &Path, spool_dir: &Path, device: DeviceInfo) -> Result<Engine, SyncError> {
        std::fs::create_dir_all(spool_dir)?;
        if let Ok(entries) = std::fs::read_dir(spool_dir) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_file()) {
                    cx::remove_quietly(&entry.path());
                }
            }
        }
        let replica = Replica::open(db_path)?;
        let clock = Clock::new(replica.max_hlc()?);
        let mut device = device;
        let mut state = CycleState::default();
        match replica.meta(replica::META_DEVICE_ID)? {
            Some(id) if id != device.id && replica.meta(replica::META_VAULT_ID)?.is_some() => {
                device.id = id.clone();
                state.announce_id = Some(id);
            }
            Some(id) if id == device.id => {}
            _ => replica.set_meta(replica::META_DEVICE_ID, &device.id)?,
        }
        Ok(Engine {
            db_path: db_path.to_path_buf(),
            spool_dir: spool_dir.to_path_buf(),
            replica: Arc::new(Mutex::new(replica)),
            clock: Arc::new(clock),
            device: Mutex::new(device),
            session: Mutex::new(None),
            cycle: tokio::sync::Mutex::new(state),
            spool_seq: AtomicU64::new(0),
        })
    }

    /// 捕获改动用的原子时钟句柄（钩子与 sink 在锁内调用 `now()`）
    pub fn hlc(&self) -> Arc<Clock> {
        Arc::clone(&self.clock)
    }

    /// 日志写入线程使用的写入接口（sync.db 上的第二个连接）
    pub fn journal(&self) -> Result<Journal, SyncError> {
        Journal::open(&self.db_path)
    }

    /// 本机当前的设备信息（克隆检测后 id 可能已更换）
    pub fn device(&self) -> DeviceInfo {
        self.device.lock().clone()
    }

    /// 更新本机设备名（下次写 head 时生效）
    pub fn set_device_name(&self, name: &str) {
        self.device.lock().name = name.to_string();
    }

    fn set_device_id(&self, id: &str) {
        self.device.lock().id = id.to_string();
    }

    /// 副本所属的 vault（未连接过为 `None`）。阻塞（读 sync.db）
    pub fn vault_id(&self) -> Result<Option<String>, SyncError> {
        self.replica.lock().meta(replica::META_VAULT_ID)
    }

    pub fn is_unlocked(&self) -> bool {
        self.session.lock().is_some()
    }

    /// 用钥匙串里保存的主密钥解锁。`vault_id` 与副本记录的不一致、或副本的连接没有完成 ⇒ `VaultReset`
    pub fn unlock(&self, vault_id: &str, master: &MasterKey) -> Result<(), SyncError> {
        {
            let replica = self.replica.lock();
            let matches = replica.meta(replica::META_VAULT_ID)?.as_deref() == Some(vault_id)
                && replica.meta(replica::META_JOINED)?.as_deref() == Some("1");
            if !matches {
                return Err(SyncError::VaultReset);
            }
        }
        self.install_session(vault_id, master)
    }

    /// 丢弃内存里的密钥
    pub fn lock(&self) {
        *self.session.lock() = None;
    }

    fn session(&self) -> Option<Arc<Session>> {
        self.session.lock().clone()
    }

    /// 在工作线程里读写副本
    async fn db<T, F>(&self, f: F) -> Result<T, SyncError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Replica) -> Result<T, SyncError> + Send + 'static,
    {
        let replica = Arc::clone(&self.replica);
        cx::blocking(move || f(&mut replica.lock())).await?
    }

    /// 持久化时钟（收到远端记录后时钟可能前进了）
    async fn persist_clock(&self) -> Result<(), SyncError> {
        let last = self.clock.last();
        self.db(move |r| r.persist_hlc(last)).await
    }

    /// spool 目录下一个新的临时文件路径
    fn spool_file(&self, tag: &str) -> PathBuf {
        let n = self.spool_seq.fetch_add(1, Ordering::Relaxed);
        self.spool_dir.join(format!("{tag}-{}-{n}.part", std::process::id()))
    }

    /// 连接：按服务器与本机状态创建 / 加入 / 恢复。`forbidden` 是口令不得等于的值（WebDAV 密码、账号、
    /// 服务器主机名），只在创建时检查。完成解锁或创建、配置类域的加入与首次应用后返回；
    /// 剪贴板与识别历史的加入由之后的 [`Engine::sync_cycle`]（[`CycleReason::Connect`]）继续
    pub async fn connect<R: Remote, L: LocalStore + ?Sized>(
        &self,
        remote: &R,
        local: &Arc<L>,
        passphrase: &str,
        forbidden: &[&str],
        options: &CycleOptions,
        progress: &ProgressFn,
    ) -> Result<Connected, SyncError> {
        join::connect(self, remote, local, passphrase, forbidden, options, progress).await
    }

    /// 跑一轮同步。必须已解锁（否则 `Locked`）；vault 被清除或替换 ⇒ `VaultReset`，本轮不写任何文件
    pub async fn sync_cycle<R: Remote, L: LocalStore + ?Sized>(
        &self,
        remote: &R,
        local: &Arc<L>,
        reason: CycleReason,
        options: &CycleOptions,
        progress: &ProgressFn,
    ) -> Result<CycleReport, SyncError> {
        let mut state = self.cycle.lock().await;
        let session = self.session().ok_or(SyncError::Locked)?;
        let vid = session.vault_id.clone();
        let ready = self
            .db(move |r| {
                Ok(r.meta(replica::META_VAULT_ID)?.as_deref() == Some(vid.as_str())
                    && r.meta(replica::META_JOINED)?.as_deref() == Some("1"))
            })
            .await?;
        if !ready {
            return Err(SyncError::VaultReset);
        }
        let own = self.device().id;
        let mut cx = Cx::new(self, remote, local, options, progress, session, own);
        let result = run_cycle(&mut cx, &mut state, reason).await;
        cx.finish_counts();
        log::debug!(
            "sync: cycle {} finished: {} requests, {} pulled, {} applied, {} pushed",
            reason.as_str(),
            cx.report.requests,
            cx.report.pulled,
            cx.report.applied,
            cx.report.pushed
        );
        result?;
        let mut report = cx.report;
        report.new_device_id = state.announce_id.take();
        Ok(report)
    }

    /// 把某个域重新标为需要处理，下一轮重新应用：
    /// - 配置类域与 Key（配置文件在启动时被创建或重置、升级后可能认识了以前不认识的值、Linux 会话钥匙串重启后
    ///   丢失了 Key）：全部胜出记录重新标为「未应用」；
    /// - 剪贴板 / 识别历史（打开了对应的内容开关）：重新执行一次加入（条目自身的时间），并重新评估全部条目。
    ///
    /// 阻塞（写 sync.db）
    pub fn reapply(&self, d: &Domain) -> Result<(), SyncError> {
        let replica = self.replica.lock();
        match d {
            Domain::Clip | Domain::Ocr => {
                replica.set_meta(&replica::meta_join_key(d), "pending")?;
                let eval = if *d == Domain::Clip {
                    replica::META_CLIP_EVAL
                } else {
                    replica::META_OCR_EVAL
                };
                replica.del_meta(eval)
            }
            _ => replica.mark_domain_unapplied(d),
        }
    }

    /// 已知的设备（来自最近读到的各设备 head 与本机信息），不联网。阻塞（读 sync.db）
    pub fn devices(&self) -> Result<Vec<DeviceSummary>, SyncError> {
        let me = self.device();
        let replica = self.replica.lock();
        let last_ok = replica
            .meta(replica::META_LAST_OK)?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut out = vec![DeviceSummary {
            id: me.id.clone(),
            name: me.name.clone(),
            os: me.os,
            app_version: me.app_version.clone(),
            last_seen_ms: last_ok,
            this_device: true,
        }];
        for c in replica.cursors()? {
            if !c.present || c.dev == me.id {
                continue;
            }
            if let Some(info) = c.info {
                out.push(DeviceSummary {
                    id: c.dev,
                    name: info.name,
                    os: info.os,
                    app_version: info.app_version,
                    last_seen_ms: c.updated_ms.unwrap_or(0),
                    this_device: false,
                });
            }
        }
        Ok(out)
    }

    /// 本地状态，不联网。阻塞（读 sync.db）
    pub fn status(&self) -> Result<EngineStatus, SyncError> {
        let unlocked = self.is_unlocked();
        let replica = self.replica.lock();
        let joining = [Domain::Clip, Domain::Ocr]
            .iter()
            .map(|d| replica.meta(&replica::meta_join_key(d)))
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|v| v.as_deref() == Some("pending"));
        Ok(EngineStatus {
            vault_id: replica.meta(replica::META_VAULT_ID)?,
            unlocked,
            pending: replica.has_any_pending()?,
            device_count: replica
                .meta_u64(replica::META_DEVICE_COUNT)?
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0),
            last_ok_ms: replica.meta(replica::META_LAST_OK)?.and_then(|v| v.parse().ok()),
            joining,
        })
    }

    /// 关闭副本（断开同步时调用方随后删除 sync.db 与 spool 目录）
    pub fn close(self) -> Result<(), SyncError> {
        let replica = Arc::try_unwrap(self.replica).map_err(|_| SyncError::Internal("replica still in use"))?;
        replica.into_inner().close()
    }
}

/// 一轮同步：拉取 → 待完成的加入 → 应用 → 推送 → 压缩
async fn run_cycle<R: Remote, L: LocalStore + ?Sized>(
    cx: &mut Cx<'_, R, L>,
    state: &mut CycleState,
    reason: CycleReason,
) -> Result<(), SyncError> {
    pull::pull(cx, state).await?;

    // 连接时跳过的域（开关关闭、或剪贴板与识别历史留给后台）在这里加入
    for d in [
        Domain::Cfg,
        Domain::Sty,
        Domain::Glo,
        Domain::Key,
        Domain::Clip,
        Domain::Ocr,
    ] {
        if !cx.options.content.allows(&d) {
            continue;
        }
        let key = replica::meta_join_key(&d);
        let pending = cx.db(move |r| r.meta(&key)).await?.as_deref() == Some("pending");
        if !pending {
            continue;
        }
        if d.is_content() {
            join::join_content(cx, d).await?;
        } else {
            join::join_config(cx, d, false).await?;
        }
    }

    let mut order = vec![
        Domain::Cfg,
        Domain::Sty,
        Domain::Glo,
        Domain::Key,
        Domain::Clip,
        Domain::Ocr,
    ];
    if reason == CycleReason::ClipboardShown {
        // 剪贴板视图正在显示：先让剪贴板的变化出现
        order.retain(|d| *d != Domain::Clip);
        order.insert(0, Domain::Clip);
    }
    apply::apply_domains(cx, &order).await?;

    push::push(cx, state, &Domain::KNOWN).await?;
    compact::maybe_compact(cx, state).await?;
    if cx.report.compacted {
        // 压缩发现本机的段在服务器上丢了时，会把只在那些段里的胜出记录重新标为待上传：本轮就补发出去
        push::push(cx, state, &Domain::KNOWN).await?;
    }
    push::ensure_head(cx, state).await?;

    let own = cx.own.clone();
    let allowed: Vec<Domain> = Domain::KNOWN
        .iter()
        .filter(|d| cx.options.content.allows(d))
        .cloned()
        .collect();
    let device_count = cx.report.device_count;
    let now = i64::try_from(hlc::wall_now_ms()).unwrap_or(i64::MAX);
    let last = cx.engine.hlc().last();
    cx.report.pending = cx
        .db(move |r| {
            r.persist_hlc(last)?;
            r.set_meta(replica::META_LAST_OK, &now.to_string())?;
            r.set_meta(replica::META_DEVICE_COUNT, &device_count.to_string())?;
            r.has_pending(&own, &allowed)
        })
        .await?;
    if cx.keychain_error {
        return Err(SyncError::Local(LocalError::Keychain));
    }
    // 附件上传失败：已完成的工作都已提交，引用它的记录留在待上传。报告为暂时性失败让调用方退避，
    // 而不是要求立即重跑——同样的失败多半会马上重现，立即重跑只会反复上传同一张图
    if let Some(e) = cx.blob_error.take() {
        return Err(e);
    }
    Ok(())
}

/// 「测试连接」：认证、确保同步根目录存在、写入并删除一个探测文件、读取服务器上已有的同步数据概况。
/// `local_vault_id` 是本机副本所属的 vault（[`Replica::peek_vault_id`]），用于判断能否直接恢复
pub async fn probe<R: Remote>(remote: &R, local_vault_id: Option<&str>) -> Result<ProbeReport, SyncError> {
    join::probe(remote, local_vault_id).await
}

/// 清除服务器上的同步数据：先删除 vault.json（所有设备下一轮立刻得知），再尽力删除本 vault 的 head 与目录。
/// 不需要口令；之后调用方断开本机同步
pub async fn reset_remote<R: Remote>(remote: &R) -> Result<(), SyncError> {
    join::reset_remote(remote).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::MemoryRemote;

    // 只做类型检查：引擎对外的 future 必须是 Send（桌面端在多线程运行时里 spawn）
    #[allow(dead_code)]
    fn futures_are_send(
        engine: &Engine,
        remote: &MemoryRemote,
        local: &Arc<dyn LocalStore>,
        options: &CycleOptions,
        progress: &ProgressFn,
    ) {
        fn is_send<T: Send>(_: T) {}
        is_send(engine.sync_cycle(remote, local, CycleReason::Manual, options, progress));
        is_send(engine.connect(remote, local, "passphrase", &[], options, progress));
        is_send(probe(remote, None));
        is_send(reset_remote(remote));
    }

    #[test]
    fn engine_is_shareable() {
        fn is_send_sync<T: Send + Sync>() {}
        is_send_sync::<Engine>();
        is_send_sync::<Journal>();
    }

    #[test]
    fn content_switches_gate_domains() {
        let mut c = ContentSwitches::default();
        assert!(Domain::KNOWN.iter().all(|d| c.allows(d)));
        assert!(!c.allows(&Domain::Unknown("x".into())));
        c.settings = false;
        assert!(!c.allows(&Domain::Key) && !c.allows(&Domain::Glo));
        c.settings = true;
        c.api_keys = false;
        assert!(!c.allows(&Domain::Key) && c.allows(&Domain::Cfg));
    }

    #[test]
    fn only_local_changes_are_not_user_activity() {
        assert!(!CycleReason::LocalChange.is_user_activity());
        assert!(CycleReason::TrayOpened.is_user_activity());
        assert!(CycleReason::Manual.is_user_activity());
    }
}
