//! 离线模型：在本机下载、运行的 OCR 与翻译模型。
//!
//! 这里只有标识、设置项取值与设置页用的状态快照（`offline_models_get` / `offline_models:changed`）。目录（文件清单、
//! 校验和、下载源）、下载、推理进程的托管都在 `wisp-local`；本模块不含路径、地址等本机细节，快照可以原样发给前端。
//!
//! 字节数一律是 `u64`（TS 为 number；单个模型最大约 6 GB，远小于 2^53）。

use serde::{Deserialize, Serialize};

/// 离线模型的稳定 id：目录名、`installed.json`、设置（`offline.*_model`）与 IPC 都用它，发布后不改
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum LocalModelId {
    /// PP-OCRv6 Tiny：最快，CPU 即可
    #[serde(rename = "pp_ocrv6_tiny")]
    PpOcrV6Tiny,
    /// PP-OCRv6 Small：速度与准确率均衡
    #[serde(rename = "pp_ocrv6_small")]
    PpOcrV6Small,
    /// PaddleOCR-VL-1.6：表格、公式与复杂版面（版面检测 + 视觉语言模型）
    #[serde(rename = "paddleocr_vl_1_6")]
    PaddleOcrVl16,
    /// Index-Translate 2B
    #[serde(rename = "index_translate_2b")]
    IndexTranslate2b,
    /// Index-Translate 9B
    #[serde(rename = "index_translate_9b")]
    IndexTranslate9b,
}

impl LocalModelId {
    /// 全部模型，按设置页的显示顺序（先文字识别、后翻译；同一类里由轻到重）
    pub const ALL: [Self; 5] = [
        Self::PpOcrV6Tiny,
        Self::PpOcrV6Small,
        Self::PaddleOcrVl16,
        Self::IndexTranslate2b,
        Self::IndexTranslate9b,
    ];

    /// 模型的用途
    pub const fn kind(self) -> LocalModelKind {
        match self {
            Self::PpOcrV6Tiny | Self::PpOcrV6Small | Self::PaddleOcrVl16 => LocalModelKind::Ocr,
            Self::IndexTranslate2b | Self::IndexTranslate9b => LocalModelKind::Translate,
        }
    }

    /// 与 serde 一致的 id（目录名、日志）
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PpOcrV6Tiny => "pp_ocrv6_tiny",
            Self::PpOcrV6Small => "pp_ocrv6_small",
            Self::PaddleOcrVl16 => "paddleocr_vl_1_6",
            Self::IndexTranslate2b => "index_translate_2b",
            Self::IndexTranslate9b => "index_translate_9b",
        }
    }

    /// 按 id 查找（不认识的 id 返回 None）
    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == id)
    }
}

/// 离线模型的用途
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalModelKind {
    Ocr,
    Translate,
}

/// 推理运行时：按需下载，与模型分开存放、分开删除
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum LocalRuntimeId {
    /// llama.cpp 的 `llama-server`（PaddleOCR-VL、Index-Translate）
    #[serde(rename = "llama_cpp")]
    LlamaCpp,
    /// ONNX Runtime 动态库（PP-OCRv6、版面检测）
    #[serde(rename = "onnx_runtime")]
    OnnxRuntime,
}

impl LocalRuntimeId {
    /// 全部运行时（设置页的显示顺序）
    pub const ALL: [Self; 2] = [Self::LlamaCpp, Self::OnnxRuntime];

    /// 与 serde 一致的 id（目录名、日志）
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LlamaCpp => "llama_cpp",
            Self::OnnxRuntime => "onnx_runtime",
        }
    }
}

/// 模型文件的下载源（`offline.download_source`）。运行时总是从官方的 GitHub Release 下载，不受它影响
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadSource {
    /// 同时探测各个源，用响应最快的；下载变慢或失败时换源续传
    #[default]
    Auto,
    ModelScope,
    HuggingFace,
}

/// 下载失败的种类（`LocalModelState::Failed.code`），前端按它取文案
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadErrorCode {
    /// 连不上或反复断流（所有下载源都试过）
    Network,
    /// 服务器拒绝或找不到文件（4xx / 5xx）
    Server,
    /// 下载的内容与清单里的校验和不符（换源重试后仍不符）
    ChecksumMismatch,
    /// 磁盘空间不足
    DiskFull,
    /// 写入本机文件失败（权限、只读磁盘等）
    Io,
    /// 运行时安装包解不开或缺少需要的文件
    Extract,
}

/// 一个模型的安装状态（`LocalModelInfo.state`，内部标签 `state`）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LocalModelState {
    NotInstalled,
    /// 排在其他下载后面
    Queued,
    /// 正在下载。`done` / `total` 含需要一并下载的运行时；`bytes_per_sec` 为 3 s 滑动平均（刚开始时为 0）
    Downloading {
        done: u64,
        total: u64,
        bytes_per_sec: u64,
    },
    /// 已暂停（保留已下载的部分，继续时从断点开始）。应用退出时正在下载的任务也会变成暂停
    Paused {
        done: u64,
        total: u64,
    },
    /// 下载完成，正在校验与解包（不确定进度）
    Verifying,
    Installed,
    /// 文件缺失或大小不符：需要重新下载
    Broken,
    /// 下载失败（已下载的部分保留，可以重试）
    Failed {
        code: DownloadErrorCode,
    },
}

/// 模型的加载状态（是否占着内存）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadState {
    /// 没有加载（不占内存）
    #[default]
    Unloaded,
    /// 正在启动推理进程、加载权重
    Loading,
    /// 已加载，闲置一段时间或系统内存紧张时自动释放
    Loaded,
}

/// 本机运行这个模型的适配程度
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FitnessLevel {
    /// 适合本机
    Good,
    /// 能用但较慢（例如只有 CPU）
    Slow,
    /// 内存不足：仍允许下载，但需要二次确认
    Insufficient,
    /// 还不知道（Windows / Linux 在推理组件下载之前检测不到显卡）
    Unknown,
}

/// 设备适配结论
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Fitness {
    pub level: FitnessLevel,
    /// 说明原因的 i18n key（`settings.offline.fitness.<reason>`）；不需要说明时为 null
    pub reason_key: Option<String>,
}

impl Fitness {
    /// 不带说明的结论
    pub const fn plain(level: FitnessLevel) -> Self {
        Self {
            level,
            reason_key: None,
        }
    }
}

/// 设置页里一个模型的完整状态
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalModelInfo {
    pub id: LocalModelId,
    pub kind: LocalModelKind,
    /// 模型文件的总下载量（字节，不含运行时）
    pub download_bytes: u64,
    /// 还需要一并下载的运行时（字节；需要的运行时都已安装时为 0）
    pub pending_runtime_bytes: u64,
    pub state: LocalModelState,
    pub load: LoadState,
    /// 加载后大约占用的内存（MB），用于说明与加载前的内存检查
    pub memory_estimate_mb: u32,
    pub fitness: Fitness,
    /// 已安装的文件与本版本的目录不一致（模型有更新）：界面提示重新下载
    pub update_available: bool,
}

/// 一个运行时的状态
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalRuntimeInfo {
    pub id: LocalRuntimeId,
    /// 版本号，例如 `b11669`、`1.28.3`
    pub version: String,
    pub installed: bool,
    /// 已安装时为占用的磁盘空间，未安装时为下载量（字节）
    pub size: u64,
    /// 有已安装的模型依赖它（这时不能单独删除）
    pub in_use: bool,
}

/// 一块显卡（来自 llama.cpp 的设备检测）。只有名称与显存，不含序列号、驱动路径等
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GpuInfo {
    pub name: String,
    pub vram_total_mb: u32,
    /// 集成显卡（核显）：它的「显存」是划给它的系统内存，界面不当作显存显示，设备适配按 CPU 档评估
    #[serde(default)]
    pub integrated: bool,
}

/// 本机的硬件概况（设备适配与设置页的说明用）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// 物理内存总量（字节）；取不到时为 null
    pub total_memory: Option<u64>,
    /// Apple 芯片（统一内存，Metal 加速）
    pub apple_silicon: bool,
    /// 检测到的显卡（macOS 恒为空：统一内存不单独列出）
    pub gpus: Vec<GpuInfo>,
    /// 是否已经检测过显卡（Windows / Linux 要先下载 llama.cpp 运行时才能检测；macOS 恒为 true）
    pub gpu_probed: bool,
}

/// 离线模型页的完整快照（`offline_models_get` / `offline_devices_refresh` 的返回值，`offline_models:changed` 的载荷）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalModelsSnapshot {
    /// 按 [`LocalModelId::ALL`] 的顺序
    pub models: Vec<LocalModelInfo>,
    /// 按 [`LocalRuntimeId::ALL`] 的顺序
    pub runtimes: Vec<LocalRuntimeInfo>,
    pub device: DeviceInfo,
    /// 模型与运行时一共占用的磁盘空间（字节，含未完成的下载）
    pub disk_used: u64,
    /// 模型目录所在磁盘的可用空间（字节）；取不到时为 null
    pub disk_free: Option<u64>,
}

/// 设置快照里的离线模型摘要（`RuntimeInfo.offline`）：只在安装 / 删除时变化，面板据此决定模型菜单。下载进度不在这里
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OfflineSummary {
    /// 已安装（可以直接使用）的模型，按 [`LocalModelId::ALL`] 的顺序
    pub installed: Vec<LocalModelId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_and_match_serde() {
        for id in LocalModelId::ALL {
            let json = serde_json::to_string(&id).expect("serialize");
            assert_eq!(json, format!("\"{}\"", id.as_str()));
            assert_eq!(LocalModelId::parse(id.as_str()), Some(id));
        }
        for id in LocalRuntimeId::ALL {
            let json = serde_json::to_string(&id).expect("serialize");
            assert_eq!(json, format!("\"{}\"", id.as_str()));
        }
        assert_eq!(LocalModelId::parse("pp_ocrv5"), None);
    }

    #[test]
    fn kinds() {
        assert_eq!(LocalModelId::PaddleOcrVl16.kind(), LocalModelKind::Ocr);
        assert_eq!(LocalModelId::IndexTranslate9b.kind(), LocalModelKind::Translate);
        // 设置页按「先识别、后翻译」排列
        let kinds: Vec<_> = LocalModelId::ALL.iter().map(|m| m.kind()).collect();
        assert!(kinds.windows(2).all(|w| w[0] <= w[1]));
    }
}
