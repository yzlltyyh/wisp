//! Wisp 的离线模型。
//!
//! 在本机下载、存放、运行 OCR 与翻译模型，以及访问 PaddleOCR 云端。只依赖 `wisp-types` 与 `wisp-text`，不依赖 Tauri；
//! 平台相关的部分（受托管子进程、内存与磁盘信息）由 desktop 经 [`launch::Launcher`] 等接口注入，本 crate 不写
//! `cfg(target_os)`。
//!
//! ## 模块
//!
//! - [`catalog`]：编译进二进制的模型与运行时清单（文件、大小、sha256、下载源）；
//! - [`store`]：`models/` 目录布局、安装记录（`installed.json`）、删除与占用统计；
//! - [`download`]：断点续传、多源、校验、解包，进度回调；
//! - [`fitness`]：按设备信息评估模型是否适合本机；
//! - [`launch`]：启动受托管子进程的接口（desktop 用 `wisp_platform::child` 实现）；
//! - [`llama`]：`llama-server` 的参数、启动、就绪判定、输出处理、设备检测与停止；
//! - [`worker`]：ORT 工作进程——子进程里执行的 [`worker::run`] 与主进程侧的客户端 [`worker::OrtWorker`]、帧协议；
//! - [`ocr`]：PP-OCR（只在工作进程里执行）、版面检测、文本拼装、PaddleOCR-VL 文档流水线、OTSL 表格、云端客户端；
//! - [`net`]：下载与云端共用的 HTTP 客户端构造；
//! - [`stream`]：把引擎的结果包装成与 wisp-llm 同语义的 `StreamEvent` 流；
//! - `fake`（feature `fake`）：假引擎。
//!
//! ## 约定
//!
//! - **默认零成本**：构造任何类型都不做 I/O、不起线程、不建 HTTP 客户端；只有用户点了下载或用到离线引擎时才有动作。
//! - **隐私**：识别与翻译的内容、截图、Token、服务地址的完整形式、结果下载链接、推理进程的输出都不进日志与错误信息。
//!   日志里的地址只写 host 与固定的路径类别；错误信息是固定的英文简述。
//! - 可能阻塞的调用（读写文件、计算哈希、解包、等子进程）标明「阻塞」，由调用方放进阻塞线程或专用线程。

pub mod catalog;
pub mod download;
pub mod error;
#[cfg(feature = "fake")]
pub mod fake;
pub mod fitness;
pub mod launch;
pub mod llama;
pub mod net;
pub mod ocr;
pub mod store;
pub mod stream;
pub mod worker;

pub use error::{LocalError, RuntimeFailure};
