//! 剪贴板服务与粘贴：监听 → 工作线程读取 → ingest → 缓存 → 发出 `clipboard:changed`；暂停；空闲清理；
//! 粘贴流程（写入 + 抑制 + touch + 立即隐藏面板 + paste_into + 结果处理，只复制的各分支触发 toast）；
//! 图标缓存与 `wisp://` 资源；OCR 历史服务。
//!
//! # 结构
//! - [`service`]：对外的 [`ClipboardService`]，只做异步外壳——把阻塞调用
//!   放进阻塞线程池、启动后台任务；
//! - `engine`：服务核心 `Engine`（数据库的惰性打开、内存缓存、记录管线、置顶 / 删除 / 清空），不依赖 Tauri 运行时；
//! - `port`：核心对外部世界的两个接口——平台（`Port`，生产实现就是 `wisp_platform::Platform`）与宿主（`Host`：
//!   窗口、事件、托盘、设置、埋点），单测用 mock 驱动；
//! - `watch`：监听回调 → 工作任务的转交与「读取失败退避重试」；
//! - `drag`：拖出条目时的拖拽数据（与粘贴同一个口径）；
//! - [`paste`]：统一的粘贴管线（来源 × 宿主，阶段 × 事件的状态机、单飞、按面板会话去重）与 `copy_text`；
//! - `queue`：连续粘贴——队列进行中拦下粘贴键，每按一次写入下一条再重放这次按键；
//! - `smart`：智能识别的接入——条目检视的缓存、验证码条目的到期删除；
//! - `outbox`：`clipboard:changed` 的投递（视图 ready 之后才推送，准备中的排队）；
//! - [`icons`]：来源应用图标；
//! - [`files`]：文件条目的 exists 检查（页面交给前端之前，有时限）与「仍存在的文件」；
//! - [`retention`]：空闲清理与按天数过期条目的隐藏；
//! - [`assets`]：`wisp://` 资源 `img/`、`thumb/`、`icon/`、`ocrimg/`；
//! - [`sync`]：多设备同步的接入点（本机改动的通知 `ClipSyncSink`、远端改动的应用）；
//! - `texts`：本模块用到的 Rust 侧文案（原生对话框与通知）。
//!
//! # 线程与锁
//! - 平台回调只转交序号（`WatchSignal::post`），读取、记录都在阻塞线程池里进行；
//! - 「改动数据库的 `ClipStore` 调用 + 对应的 `RecentCache` 更新」由同一把串行锁包住（wisp-clipboard 的约定），缓存另有一把读写锁：
//!   首屏（`first_page`）只拿缓存的读锁，不会被正在进行的记录（图片解码可达上百毫秒）挡住；
//! - 取图标（含 `put_icon` 的 PNG 解码与缩放）、生成缩略图都在串行锁外（理由见 `engine.rs`「不持串行锁的库改动」）；
//! - 改动缓存的 `clipboard:changed` 在串行区内交给 `outbox`，后者在投递锁内按调用顺序发出（不阻塞）：同一条目的
//!   事件顺序与库里的改动顺序一致（`engine.rs`「事件的顺序」）。锁的顺序固定为 串行锁 → 投递锁 → 窗口层的面板状态锁；
//! - 粘贴有自己的单飞闸门（`paste::PasteGate`），与上面的锁无嵌套；只复制的 toast 在闸门放开之后才显示；
//! - 同步：本机改动在串行区内通知 sink，远端改动的应用每个串行区最多写 50 条（`sync.rs`）；识别历史的改动另有一把
//!   串行锁（`Engine::ocr_serial`），与剪贴板的串行锁不嵌套。
//!
//! # 隐私
//! 后台路径（记录、列表、预取）只对文件路径做 `stat`（是否存在、大小），**不读取文件内容、不列目录**；读取文件只发生在
//! 用户的明确操作里（另存为图片读的是 Wisp 自己的 blob）。被隐私规则拦下的复制不留任何痕迹（不取图标、不发事件）。

pub mod assets;
pub(crate) mod drag;
pub(crate) mod engine;
pub mod files;
pub mod icons;
pub mod ocr_history;
pub(crate) mod outbox;
pub mod paste;
pub(crate) mod port;
pub(crate) mod queue;
pub mod retention;
pub(crate) mod running;
pub mod service;
pub(crate) mod smart;
pub mod sync;
pub(crate) mod texts;
pub(crate) mod watch;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_float;
#[cfg(test)]
mod tests_flow;
#[cfg(test)]
mod tests_paste;
#[cfg(test)]
mod tests_queue;
#[cfg(test)]
mod tests_smart;
#[cfg(test)]
mod tests_sync;

pub use paste::{CopyReceipt, FLOAT_LABEL, PasteHost, PasteKind, PasteRequest, PasteSource};
pub use service::ClipboardService;
pub use sync::{
    ClipRemoteChange, ClipRemoteClear, ClipSyncSink, OcrRemoteChange, OcrSuperseded, RemoteOutcome, Superseded,
};
