//! 模型与运行时的目录：编译进二进制，随应用版本发布（不拉远程清单，也就不需要另一套签名体系）。
//!
//! 每个文件的大小与 sha256 都钉死在这里，下载完成后逐个比对，不符即丢弃。模型文件同时登记 ModelScope 与
//! Hugging Face 两个源（钉到具体 commit，两边内容相同、哈希一致）；运行时只从官方 GitHub Release 下载。
//! 运行时发布包里需要的文件按 [`ExtractRule`] 挑出来（只取需要的，拒绝绝对路径与 `..`）。
//!
//! 目录改动（换文件、换版本）时把 [`CATALOG_REVISION`] 加一：已安装的模型记录着安装时的版本号，版本不同时界面提示
//! 「有更新」（`LocalModelInfo.update_available`），不自动重新下载。

use wisp_types::{LocalModelId, LocalRuntimeId};

use crate::ocr::files as ocr_files;

/// 目录版本（见模块说明）
pub const CATALOG_REVISION: u32 = 1;

/// llama.cpp 的版本（官方预编译的 `llama-server`）
pub const LLAMA_CPP_VERSION: &str = "b11669";
/// ONNX Runtime 的版本（官方 CPU 构建的动态库）
pub const ONNX_RUNTIME_VERSION: &str = "1.28.3";

/// 发布目标。按 `std::env::consts` 在运行期判断，不写 `cfg(target_os)`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    MacosArm64,
    WindowsX64,
    LinuxX64,
}

impl Target {
    /// 当前进程的目标；不在发布范围内（例如 Intel Mac、Linux arm64）时为 None——这时离线模型不可用
    pub fn current() -> Option<Self> {
        Self::from_consts(std::env::consts::OS, std::env::consts::ARCH)
    }

    /// 由 `std::env::consts::{OS, ARCH}` 的值判断（单测用）
    pub fn from_consts(os: &str, arch: &str) -> Option<Self> {
        match (os, arch) {
            ("macos", "aarch64") => Some(Self::MacosArm64),
            ("windows", "x86_64") => Some(Self::WindowsX64),
            ("linux", "x86_64") => Some(Self::LinuxX64),
            _ => None,
        }
    }
}

/// 一个模型文件
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileSpec {
    /// 存进模型目录时的相对路径（只含 `[A-Za-z0-9._-]` 与 `/`）
    pub name: &'static str,
    /// 字节数
    pub size: u64,
    /// sha256（小写十六进制）
    pub sha256: &'static str,
    /// ModelScope 的下载地址（钉到 commit 的 `resolve` 地址）
    pub model_scope: &'static str,
    /// Hugging Face 的下载地址（钉到 commit 的 `resolve` 地址）
    pub hugging_face: &'static str,
}

/// 一个模型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelSpec {
    pub id: LocalModelId,
    /// 需要下载的全部文件
    pub files: &'static [FileSpec],
    /// 需要的运行时（下载模型时一并下载缺的）
    pub runtimes: &'static [LocalRuntimeId],
    /// 加载后大约占用的内存（MB，含上下文与工作区），用于加载前的内存检查与界面说明
    pub memory_estimate_mb: u32,
    /// 模型的许可证（SPDX）
    pub license: &'static str,
    /// 官方模型页（设置页的许可证链接）
    pub homepage: &'static str,
}

/// 运行时发布包的格式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArchiveKind {
    TarGz,
    Zip,
}

/// 从发布包里挑出的文件：包内路径（`/` 分隔）以 `prefix` 开头、以 `suffix` 结尾，且两者之间没有 `/`
/// （例如 `build/bin/` + `.dylib` 取出该目录下的全部动态库）；`suffix` 为空时 `prefix` 就是完整路径
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractRule {
    pub prefix: &'static str,
    pub suffix: &'static str,
    /// 解出后的相对路径所在目录（空串 = 运行时目录本身）
    pub dest_dir: &'static str,
    /// 设置可执行位（Unix）
    pub executable: bool,
}

/// 某个目标上的运行时发布包
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeAsset {
    pub target: Target,
    /// GitHub Release 的下载地址
    pub url: &'static str,
    pub size: u64,
    /// 发布包的 sha256（GitHub 资产的 digest）。解出的文件以整包哈希为准，不再逐个钉
    pub sha256: &'static str,
    pub archive: ArchiveKind,
    /// 需要解出的文件
    pub extract: &'static [ExtractRule],
    /// 运行时的主文件（相对运行时目录）：`llama-server[.exe]` 或 ONNX Runtime 的动态库
    pub main: &'static str,
    /// 只用 HTTP Range 取 zip 里的 [`main`](Self::main) 这一个条目（Windows 的 ONNX Runtime 整包 78 MB，只需要其中的
    /// dll）；服务器不支持 Range 时退回整包下载。此时整包哈希无法校验，改为校验 [`entry_sha256`](Self::entry_sha256)
    pub ranged_entry: bool,
    /// `ranged_entry` 时这个条目解压后的 sha256
    pub entry_sha256: Option<&'static str>,
    /// `ranged_entry` 时区间提取实际下载的字节数：包尾（结尾记录 + 中央目录，[`crate::download::Ranged`] 一次取
    /// `min(包大小, 65 557)` 字节）+ 本地头的固定部分（30 字节）+ 条目的压缩数据。整包的哈希已经钉死，这个数随之确定
    pub ranged_bytes: Option<u64>,
}

impl RuntimeAsset {
    /// 安装这个运行时实际要下载的字节数（空间检查、进度总量与界面上的大小都用它）：能区间提取时是 [`ranged_bytes`]，
    /// 否则是整包大小。服务器不支持 Range 而退回整包时，下载器把总量补上差额（进度只增不减）
    ///
    /// [`ranged_bytes`]: Self::ranged_bytes
    pub const fn download_bytes(&self) -> u64 {
        match (self.ranged_entry, self.ranged_bytes) {
            (true, Some(n)) => n,
            _ => self.size,
        }
    }
}

/// 一个运行时
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeSpec {
    pub id: LocalRuntimeId,
    pub version: &'static str,
    /// 各目标的发布包
    pub assets: &'static [RuntimeAsset],
    /// 许可证（SPDX）
    pub license: &'static str,
    /// 官方项目页
    pub homepage: &'static str,
}

// ---------------------------------------------------------------------------
// 文件清单
//
// 数据来源（2026-10-16 核对）：ModelScope 文件列表 API（`/api/v1/models/<org>/<repo>/repo/files?Revision=master&Recursive=true`
// 的 `Size` / `Sha256`）、Hugging Face 模型 API（`?blobs=true` 的 `lfs.sha256`；非 LFS 小文件按 git blob 核对）与 GitHub
// Release API（资产的 `digest`）。两边的模型文件哈希完全一致。
//
// 地址钉到具体 commit：ModelScope 的 `resolve/<commit>/<文件>` 接受 commit（取该仓库最后一次改动文件的 commit），Hugging Face
// 用仓库当时的 commit。即使上游以后改了仓库，下载到的仍是同一份内容；无论哪个源，最终都以这里的 sha256 为准。

/// ModelScope 的钉住 commit 的下载地址
macro_rules! ms {
    ($repo:literal, $rev:literal, $file:literal) => {
        concat!("https://modelscope.cn/models/", $repo, "/resolve/", $rev, "/", $file)
    };
}

/// Hugging Face 的钉住 commit 的下载地址
macro_rules! hf {
    ($repo:literal, $rev:literal, $file:literal) => {
        concat!("https://huggingface.co/", $repo, "/resolve/", $rev, "/", $file)
    };
}

/// 同一个文件的两个源（仓库在两边同名）
macro_rules! file {
    ($name:expr, $size:literal, $sha:literal, $repo:literal, $ms_rev:literal, $hf_rev:literal, $file:literal) => {
        FileSpec {
            name: $name,
            size: $size,
            sha256: $sha,
            model_scope: ms!($repo, $ms_rev, $file),
            hugging_face: hf!($repo, $hf_rev, $file),
        }
    };
}

/// PaddleOCR-VL-1.6 的语言模型（llama-server 的 `-m`）
pub const VL_MODEL_FILE: &str = "PaddleOCR-VL-1.6-GGUF.gguf";
/// PaddleOCR-VL-1.6 的视觉投影器（`--mmproj`）
pub const VL_MMPROJ_FILE: &str = "PaddleOCR-VL-1.6-GGUF-mmproj.gguf";
/// Index-Translate-2B 的权重（Q4_K_M）
pub const INDEX_2B_FILE: &str = "Index-Translate-2B.Q4_K_M.gguf";
/// Index-Translate-9B 的权重（Q4_K_M）
pub const INDEX_9B_FILE: &str = "Index-Translate-9B.Q4_K_M.gguf";

/// llama-server 要加载的文件（相对模型目录）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GgufFiles {
    pub model: &'static str,
    pub mmproj: Option<&'static str>,
}

/// 用 llama-server 运行的模型的文件；PP-OCR 等 ORT 模型为 None
pub const fn gguf(id: LocalModelId) -> Option<GgufFiles> {
    match id {
        LocalModelId::PaddleOcrVl16 => Some(GgufFiles {
            model: VL_MODEL_FILE,
            mmproj: Some(VL_MMPROJ_FILE),
        }),
        LocalModelId::IndexTranslate2b => Some(GgufFiles {
            model: INDEX_2B_FILE,
            mmproj: None,
        }),
        LocalModelId::IndexTranslate9b => Some(GgufFiles {
            model: INDEX_9B_FILE,
            mmproj: None,
        }),
        LocalModelId::PpOcrV6Tiny | LocalModelId::PpOcrV6Small => None,
    }
}

// PP-OCRv6：检测、识别与识别的 yml（内嵌字典）；检测模型的 yml 只有预处理参数，Wisp 显式设置，不下载
static PPOCR_TINY_FILES: [FileSpec; 3] = [
    file!(
        ocr_files::PPOCR_DET,
        1_780_590,
        "193bab7a04fca699a6c82e6abb5b81bdb28177f0abd4062552b04908dafb19f8",
        "PaddlePaddle/PP-OCRv6_tiny_det_onnx",
        "7d7f5d128d9309ebf6de4f21f404dd583afdbae3",
        "2ba1506c0380b8f0b03dd142459aac66d4421f6c",
        "inference.onnx"
    ),
    file!(
        ocr_files::PPOCR_REC,
        4_462_639,
        "9ef676d6ed3c88256a2d92c640c44f25b0c40947e111b14b8be8f594091563e6",
        "PaddlePaddle/PP-OCRv6_tiny_rec_onnx",
        "afba04b618200c5f4824531c6e42c957c6439d9a",
        "2612ab37152ae0a677521bae4e1e3d4fb4cf7c30",
        "inference.onnx"
    ),
    file!(
        ocr_files::PPOCR_REC_CONFIG,
        55_571,
        "66170210bad538e83fff3c4a3867e547d6bf20b50d64b20347c4b913f3034ea1",
        "PaddlePaddle/PP-OCRv6_tiny_rec_onnx",
        "afba04b618200c5f4824531c6e42c957c6439d9a",
        "2612ab37152ae0a677521bae4e1e3d4fb4cf7c30",
        "inference.yml"
    ),
];

static PPOCR_SMALL_FILES: [FileSpec; 3] = [
    file!(
        ocr_files::PPOCR_DET,
        9_880_512,
        "d73e0058b7a8086bbd57f3d10b8bcd4ff95363f67e06e2762b5e814fe9c9410e",
        "PaddlePaddle/PP-OCRv6_small_det_onnx",
        "956a0b620a4017cc04056c692be1703b0025d028",
        "28fe5895c24fd108c19eb3e8479f4ab385fbfc62",
        "inference.onnx"
    ),
    file!(
        ocr_files::PPOCR_REC,
        21_159_378,
        "5435fd747c9e0efe15a96d0b378d5bd157e9492ed8fd80edf08f30d02fa24634",
        "PaddlePaddle/PP-OCRv6_small_rec_onnx",
        "296d43bc0ebced0fd9c605174aa5962e49810ab6",
        "b8f84f0b80c529de40b4fbb3544b84fa7233a513",
        "inference.onnx"
    ),
    file!(
        ocr_files::PPOCR_REC_CONFIG,
        150_579,
        "ab078671bb49f06228eadccd34f1bb501e157f7a047095ffb943ba81512c77d1",
        "PaddlePaddle/PP-OCRv6_small_rec_onnx",
        "296d43bc0ebced0fd9c605174aa5962e49810ab6",
        "b8f84f0b80c529de40b4fbb3544b84fa7233a513",
        "inference.yml"
    ),
];

// PaddleOCR-VL-1.6：官方 BF16 的 GGUF 与 mmproj，加上整页文档用的 PP-DocLayoutV3 版面模型
static VL_FILES: [FileSpec; 3] = [
    file!(
        VL_MODEL_FILE,
        935_769_056,
        "f3ae46ec885050acf4b3d31944431e1fd90d50664fb09126af4a3c050ba14ee8",
        "PaddlePaddle/PaddleOCR-VL-1.6-GGUF",
        "dea493a839bfee633c4014ddad016ab7f0336b6f",
        "511b09642bb324401f15f97cc23bc67e8f0a291d",
        "PaddleOCR-VL-1.6-GGUF.gguf"
    ),
    file!(
        VL_MMPROJ_FILE,
        881_770_560,
        "204d757d7610d9b3faab10d506d69e5b244e32bf765e2bab2d0167e65e0a058a",
        "PaddlePaddle/PaddleOCR-VL-1.6-GGUF",
        "dea493a839bfee633c4014ddad016ab7f0336b6f",
        "511b09642bb324401f15f97cc23bc67e8f0a291d",
        "PaddleOCR-VL-1.6-GGUF-mmproj.gguf"
    ),
    file!(
        ocr_files::LAYOUT,
        130_502_049,
        "45bf71750b00739a41fc209f132eb104a4d6b5bb29483c9078164d8b87cf28ba",
        "PaddlePaddle/PP-DocLayoutV3_onnx",
        "70e508e348e3aaa4246a5bf3e2a509d0b19dc7e2",
        "46bbdf188bb0a772c08aed74882ce7e51a8f1ea6",
        "inference.onnx"
    ),
];

// Index-Translate：官方推荐的 Q4_K_M（官方说明与 F16 输出几乎逐字一致）；纯文本翻译不需要 mmproj
static INDEX_2B_FILES: [FileSpec; 1] = [file!(
    INDEX_2B_FILE,
    1_312_164_352,
    "044b313d29342bd3b2c77cbb64023ca0d209bd9b3247763f9d162767ef2d746a",
    "IndexTeam/Index-Translate-2B-GGUF",
    "b92a7c7bf499a6f05da1a66fab8549da3de5b603",
    "3a0eca6ce931d88d00614d427ab3ed8ff0743902",
    "Index-Translate-2B.Q4_K_M.gguf"
)];

static INDEX_9B_FILES: [FileSpec; 1] = [file!(
    INDEX_9B_FILE,
    5_780_090_304,
    "9cc6758b0007f4cbf42ea3400768cbff91ae74ef8d640e467158348426d33713",
    "IndexTeam/Index-Translate-9B-GGUF",
    "e97205e0e68986a643eab5ebfd75cebeb67234d0",
    "645939fbab839a656f1bc39975a872bc8ad749f8",
    "Index-Translate-9B.Q4_K_M.gguf"
)];

// 内存估计（MB，加载并跑过一次后的常驻量，含上下文与工作区；M5 实测后取整并留余量）：
// - PP-OCRv6：ORT 工作进程推理 2 MP 截图后的常驻与峰值（tiny ≈ 125 / 280 MB，small ≈ 150 / 380 MB），取峰值；
// - VL：llama-server 常驻约 2.1 GB，另加 ORT 工作进程里的版面模型会话（约 0.5 GB 峰值）；
// - Index-Translate：llama-server 常驻 2B ≈ 1.5 GB、9B ≈ 5.8 GB（`-c 8192`，权重 mmap 计入）。
static MODELS: [ModelSpec; 5] = [
    ModelSpec {
        id: LocalModelId::PpOcrV6Tiny,
        files: &PPOCR_TINY_FILES,
        runtimes: &[LocalRuntimeId::OnnxRuntime],
        memory_estimate_mb: 300,
        license: "Apache-2.0",
        homepage: "https://huggingface.co/PaddlePaddle/PP-OCRv6_tiny_det_onnx",
    },
    ModelSpec {
        id: LocalModelId::PpOcrV6Small,
        files: &PPOCR_SMALL_FILES,
        runtimes: &[LocalRuntimeId::OnnxRuntime],
        memory_estimate_mb: 400,
        license: "Apache-2.0",
        homepage: "https://huggingface.co/PaddlePaddle/PP-OCRv6_small_det_onnx",
    },
    ModelSpec {
        id: LocalModelId::PaddleOcrVl16,
        files: &VL_FILES,
        runtimes: &[LocalRuntimeId::LlamaCpp, LocalRuntimeId::OnnxRuntime],
        memory_estimate_mb: 2_700,
        license: "Apache-2.0",
        homepage: "https://huggingface.co/PaddlePaddle/PaddleOCR-VL-1.6-GGUF",
    },
    ModelSpec {
        id: LocalModelId::IndexTranslate2b,
        files: &INDEX_2B_FILES,
        runtimes: &[LocalRuntimeId::LlamaCpp],
        memory_estimate_mb: 1_700,
        license: "Apache-2.0",
        homepage: "https://huggingface.co/IndexTeam/Index-Translate-2B-GGUF",
    },
    ModelSpec {
        id: LocalModelId::IndexTranslate9b,
        files: &INDEX_9B_FILES,
        runtimes: &[LocalRuntimeId::LlamaCpp],
        memory_estimate_mb: 6_200,
        license: "Apache-2.0",
        homepage: "https://huggingface.co/IndexTeam/Index-Translate-9B-GGUF",
    },
];

/// 包内的一个文件（完整路径），解到运行时目录本身
const fn take(path: &'static str) -> ExtractRule {
    ExtractRule {
        prefix: path,
        suffix: "",
        dest_dir: "",
        executable: false,
    }
}

/// 同上，并设可执行位
const fn take_exe(path: &'static str) -> ExtractRule {
    ExtractRule {
        prefix: path,
        suffix: "",
        dest_dir: "",
        executable: true,
    }
}

// llama.cpp：只取 llama-server 与它加载的动态库（含版本号软链接，`@rpath` / `$ORIGIN` 按 `lib*.0.dylib` / `lib*.so.0` 查找），
// 不取其余几十个 llama-* 工具。macOS 的后端（Metal / CPU / BLAS / RPC）是直接链接的；Windows / Linux 是 GGML_BACKEND_DL 构建，
// 后端按目录扫描加载：取 Vulkan 与全部 CPU 指令集变体（运行时按 CPU 打分选一个），不取 RPC 后端。
static LLAMA_MACOS: [ExtractRule; 21] = [
    take_exe("llama-b11669/llama-server"),
    take("llama-b11669/libllama-server-impl.dylib"),
    take("llama-b11669/libllama-common.0.7.0.dylib"),
    take("llama-b11669/libllama-common.0.dylib"),
    take("llama-b11669/libmtmd.0.7.0.dylib"),
    take("llama-b11669/libmtmd.0.dylib"),
    take("llama-b11669/libllama.0.7.0.dylib"),
    take("llama-b11669/libllama.0.dylib"),
    take("llama-b11669/libggml.0.27.0.dylib"),
    take("llama-b11669/libggml.0.dylib"),
    take("llama-b11669/libggml-base.0.27.0.dylib"),
    take("llama-b11669/libggml-base.0.dylib"),
    take("llama-b11669/libggml-cpu.0.27.0.dylib"),
    take("llama-b11669/libggml-cpu.0.dylib"),
    take("llama-b11669/libggml-blas.0.27.0.dylib"),
    take("llama-b11669/libggml-blas.0.dylib"),
    take("llama-b11669/libggml-metal.0.27.0.dylib"),
    take("llama-b11669/libggml-metal.0.dylib"),
    // macOS 构建直接链接了 RPC 后端（缺了它 llama-server 起不来）
    take("llama-b11669/libggml-rpc.0.27.0.dylib"),
    take("llama-b11669/libggml-rpc.0.dylib"),
    take("llama-b11669/LICENSE"),
];

static LLAMA_LINUX: [ExtractRule; 28] = [
    take_exe("llama-b11669/llama-server"),
    take("llama-b11669/libllama-server-impl.so"),
    take("llama-b11669/libllama-common.so.0.7.0"),
    take("llama-b11669/libllama-common.so.0"),
    take("llama-b11669/libmtmd.so.0.7.0"),
    take("llama-b11669/libmtmd.so.0"),
    take("llama-b11669/libllama.so.0.7.0"),
    take("llama-b11669/libllama.so.0"),
    take("llama-b11669/libggml.so.0.27.0"),
    take("llama-b11669/libggml.so.0"),
    take("llama-b11669/libggml-base.so.0.27.0"),
    take("llama-b11669/libggml-base.so.0"),
    take("llama-b11669/libggml-vulkan.so"),
    take("llama-b11669/libggml-cpu-x64.so"),
    take("llama-b11669/libggml-cpu-sse42.so"),
    take("llama-b11669/libggml-cpu-sandybridge.so"),
    take("llama-b11669/libggml-cpu-ivybridge.so"),
    take("llama-b11669/libggml-cpu-piledriver.so"),
    take("llama-b11669/libggml-cpu-haswell.so"),
    take("llama-b11669/libggml-cpu-skylakex.so"),
    take("llama-b11669/libggml-cpu-cannonlake.so"),
    take("llama-b11669/libggml-cpu-cascadelake.so"),
    take("llama-b11669/libggml-cpu-cooperlake.so"),
    take("llama-b11669/libggml-cpu-icelake.so"),
    take("llama-b11669/libggml-cpu-alderlake.so"),
    take("llama-b11669/libggml-cpu-sapphirerapids.so"),
    take("llama-b11669/libggml-cpu-zen4.so"),
    take("llama-b11669/LICENSE"),
];

// Windows 包没有顶层目录；libomp.dll 是 CPU 后端的 OpenMP（随包附带它的许可证）
static LLAMA_WINDOWS: [ExtractRule; 24] = [
    take_exe("llama-server.exe"),
    take("llama-server-impl.dll"),
    take("llama-common.dll"),
    take("mtmd.dll"),
    take("llama.dll"),
    take("ggml.dll"),
    take("ggml-base.dll"),
    take("libomp.dll"),
    take("LICENSE-LLVM-OpenMP"),
    take("ggml-vulkan.dll"),
    take("ggml-cpu-x64.dll"),
    take("ggml-cpu-sse42.dll"),
    take("ggml-cpu-sandybridge.dll"),
    take("ggml-cpu-ivybridge.dll"),
    take("ggml-cpu-piledriver.dll"),
    take("ggml-cpu-haswell.dll"),
    take("ggml-cpu-skylakex.dll"),
    take("ggml-cpu-cannonlake.dll"),
    take("ggml-cpu-cascadelake.dll"),
    take("ggml-cpu-cooperlake.dll"),
    take("ggml-cpu-icelake.dll"),
    take("ggml-cpu-alderlake.dll"),
    take("ggml-cpu-sapphirerapids.dll"),
    take("ggml-cpu-zen4.dll"),
];

static LLAMA_ASSETS: [RuntimeAsset; 3] = [
    RuntimeAsset {
        target: Target::MacosArm64,
        url: "https://github.com/ggml-org/llama.cpp/releases/download/b11669/llama-b11669-bin-macos-arm64.tar.gz",
        size: 12_191_582,
        sha256: "dbed55493d72bc1ba2babc8770d669c2289b5bc4ee9fb2a4dfc158d45b7d13fd",
        archive: ArchiveKind::TarGz,
        extract: &LLAMA_MACOS,
        main: "llama-server",
        ranged_entry: false,
        entry_sha256: None,
        ranged_bytes: None,
    },
    // Vulkan 包含 CPU 后端：没有 Vulkan 驱动时 ggml-vulkan 加载失败只是被跳过，退回 CPU
    RuntimeAsset {
        target: Target::WindowsX64,
        url: "https://github.com/ggml-org/llama.cpp/releases/download/b11669/llama-b11669-bin-win-vulkan-x64.zip",
        size: 32_182_350,
        sha256: "2c07574e7bde71321cb785282b20cc821e951132149eeceeea32ddebdbf0598e",
        archive: ArchiveKind::Zip,
        extract: &LLAMA_WINDOWS,
        main: "llama-server.exe",
        ranged_entry: false,
        entry_sha256: None,
        ranged_bytes: None,
    },
    RuntimeAsset {
        target: Target::LinuxX64,
        url: "https://github.com/ggml-org/llama.cpp/releases/download/b11669/llama-b11669-bin-ubuntu-vulkan-x64.tar.gz",
        size: 30_497_889,
        sha256: "e8709e5ba6c1b093dea7f3de57f4aba842f9a188fba6b465782d65d6fcfa4d9f",
        archive: ArchiveKind::TarGz,
        extract: &LLAMA_LINUX,
        main: "llama-server",
        ranged_entry: false,
        entry_sha256: None,
        ranged_bytes: None,
    },
];

// ONNX Runtime：只取 CPU 构建的动态库本身（包里的头文件、调试符号、测试库都不需要）
static ORT_MACOS: [ExtractRule; 2] = [
    take("onnxruntime-osx-arm64-1.28.3/lib/libonnxruntime.1.28.3.dylib"),
    take("onnxruntime-osx-arm64-1.28.3/LICENSE"),
];

static ORT_LINUX: [ExtractRule; 2] = [
    take("onnxruntime-linux-x64-1.28.3/lib/libonnxruntime.so.1.28.3"),
    take("onnxruntime-linux-x64-1.28.3/LICENSE"),
];

static ORT_WINDOWS: [ExtractRule; 1] = [take("onnxruntime-win-x64-1.28.3/lib/onnxruntime.dll")];

static ORT_ASSETS: [RuntimeAsset; 3] = [
    RuntimeAsset {
        target: Target::MacosArm64,
        url: "https://github.com/microsoft/onnxruntime/releases/download/v1.28.3/onnxruntime-osx-arm64-1.28.3.tgz",
        size: 33_057_743,
        sha256: "c436bd9f47dbce6f6311ccc21de97f829b3c862c09d24aa321958cb72f0a1d32",
        archive: ArchiveKind::TarGz,
        extract: &ORT_MACOS,
        main: "libonnxruntime.1.28.3.dylib",
        ranged_entry: false,
        entry_sha256: None,
        ranged_bytes: None,
    },
    // 整包 78.6 MB，其中 dll 压缩后只有 5.7 MB（另有 70 MB 的调试符号）：按区间只取 dll 这一个条目。
    // 实际下载 = 包尾 65 557 B（中央目录 4 646 B 在其中）+ 本地头 30 B + dll 压缩数据 5 665 242 B
    RuntimeAsset {
        target: Target::WindowsX64,
        url: "https://github.com/microsoft/onnxruntime/releases/download/v1.28.3/onnxruntime-win-x64-1.28.3.zip",
        size: 78_606_946,
        sha256: "1d6fab48e85f948436af7c8c971d2c145cf224e2c444755dec894f8b0de11a83",
        archive: ArchiveKind::Zip,
        extract: &ORT_WINDOWS,
        main: "onnxruntime.dll",
        ranged_entry: true,
        entry_sha256: Some("4d2774a5f64e4a230b16b74b167171e92f65b0683c19680c1d04fb284b92a6e2"),
        ranged_bytes: Some(65_557 + 30 + 5_665_242),
    },
    RuntimeAsset {
        target: Target::LinuxX64,
        url: "https://github.com/microsoft/onnxruntime/releases/download/v1.28.3/onnxruntime-linux-x64-1.28.3.tgz",
        size: 9_130_098,
        sha256: "db14e4863bd37893fc59729d986ab2a0d043d10b7d44da1913c4982b7e3d009c",
        archive: ArchiveKind::TarGz,
        extract: &ORT_LINUX,
        main: "libonnxruntime.so.1.28.3",
        ranged_entry: false,
        entry_sha256: None,
        ranged_bytes: None,
    },
];

static RUNTIMES: [RuntimeSpec; 2] = [
    RuntimeSpec {
        id: LocalRuntimeId::LlamaCpp,
        version: LLAMA_CPP_VERSION,
        assets: &LLAMA_ASSETS,
        license: "MIT",
        homepage: "https://github.com/ggml-org/llama.cpp",
    },
    RuntimeSpec {
        id: LocalRuntimeId::OnnxRuntime,
        version: ONNX_RUNTIME_VERSION,
        assets: &ORT_ASSETS,
        license: "MIT",
        homepage: "https://github.com/microsoft/onnxruntime",
    },
];

/// 一个模型的目录项
pub fn model(id: LocalModelId) -> &'static ModelSpec {
    // MODELS 与 LocalModelId::ALL 一一对应（单测保证），找不到时退回第一项只是防御
    MODELS.iter().find(|m| m.id == id).unwrap_or(&MODELS[0])
}

/// 全部模型（按 [`LocalModelId::ALL`] 的顺序）
pub fn models() -> &'static [ModelSpec] {
    &MODELS
}

/// 一个运行时的目录项
pub fn runtime(id: LocalRuntimeId) -> &'static RuntimeSpec {
    RUNTIMES.iter().find(|r| r.id == id).unwrap_or(&RUNTIMES[0])
}

/// 某个运行时在某个目标上的发布包；没有（目录未填或目标不在发布范围）时为 None
pub fn runtime_asset(id: LocalRuntimeId, target: Target) -> Option<&'static RuntimeAsset> {
    runtime(id).assets.iter().find(|a| a.target == target)
}

/// 模型文件的总下载量（字节，不含运行时）
pub fn model_download_bytes(id: LocalModelId) -> u64 {
    model(id).files.iter().map(|f| f.size).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_model_and_runtime_is_listed_once() {
        let ids: Vec<LocalModelId> = models().iter().map(|m| m.id).collect();
        assert_eq!(ids, LocalModelId::ALL);
        let rts: Vec<LocalRuntimeId> = RUNTIMES.iter().map(|r| r.id).collect();
        assert_eq!(rts, LocalRuntimeId::ALL);
        for id in LocalModelId::ALL {
            assert_eq!(model(id).id, id);
            assert!(!model(id).runtimes.is_empty());
        }
    }

    #[test]
    fn file_specs_are_well_formed() {
        for m in models() {
            for f in m.files {
                assert!(f.size > 0, "{}", f.name);
                assert_eq!(f.sha256.len(), 64, "{}", f.name);
                assert!(
                    f.sha256
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                );
                assert!(
                    f.name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
                        && !f.name.starts_with('/')
                        && !f.name.split('/').any(|seg| seg == ".." || seg.is_empty()),
                    "{}",
                    f.name
                );
                assert!(f.model_scope.starts_with("https://") && f.hugging_face.starts_with("https://"));
            }
        }
    }

    #[test]
    fn every_model_has_files_and_every_target_has_runtimes() {
        let is_hex = |h: &str| h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        for m in models() {
            assert!(!m.files.is_empty(), "{:?}", m.id);
            let mut names: Vec<&str> = m.files.iter().map(|f| f.name).collect();
            names.sort_unstable();
            names.dedup();
            assert_eq!(names.len(), m.files.len(), "{:?} 的文件名重复", m.id);
            assert!(m.memory_estimate_mb > 0);
            for f in m.files {
                assert!(
                    f.model_scope.starts_with("https://modelscope.cn/models/"),
                    "{}",
                    f.model_scope
                );
                assert!(
                    f.hugging_face.starts_with("https://huggingface.co/"),
                    "{}",
                    f.hugging_face
                );
                assert!(
                    !f.model_scope.contains("/master/") && !f.hugging_face.contains("/main/"),
                    "必须钉 commit"
                );
            }
            if let Some(g) = gguf(m.id) {
                assert!(m.files.iter().any(|f| f.name == g.model));
                assert!(g.mmproj.is_none_or(|p| m.files.iter().any(|f| f.name == p)));
                assert!(m.runtimes.contains(&LocalRuntimeId::LlamaCpp));
            }
        }
        for r in &RUNTIMES {
            for t in [Target::MacosArm64, Target::WindowsX64, Target::LinuxX64] {
                let a = runtime_asset(r.id, t).unwrap_or_else(|| panic!("{:?} 缺 {t:?}", r.id));
                assert!(a.url.starts_with("https://github.com/") && a.url.contains(r.version));
                assert!(a.size > 0 && is_hex(a.sha256));
                assert!(!a.extract.is_empty());
                // 主文件一定在解包规则里
                let picked: Vec<_> = a
                    .extract
                    .iter()
                    .filter_map(|rule| crate::download::extract::select(std::slice::from_ref(rule), rule.prefix))
                    .collect();
                assert_eq!(picked.len(), a.extract.len(), "{:?} {t:?}：每条规则都能选中自己", r.id);
                let main = picked.iter().find(|(p, _)| p == std::path::Path::new(a.main));
                assert!(main.is_some(), "{:?} {t:?} 的主文件不在规则里", r.id);
                if r.id == LocalRuntimeId::LlamaCpp {
                    assert!(main.is_some_and(|(_, exec)| *exec), "llama-server 要有可执行位");
                }
                assert_eq!(a.ranged_entry, a.entry_sha256.is_some());
                assert!(a.entry_sha256.is_none_or(is_hex));
                assert_eq!(a.ranged_entry, a.ranged_bytes.is_some(), "{:?} {t:?}", r.id);
                assert!(a.ranged_bytes.is_none_or(|n| n > 0 && n < a.size), "{:?} {t:?}", r.id);
                assert_eq!(a.download_bytes(), a.ranged_bytes.unwrap_or(a.size));
            }
        }
    }

    #[test]
    fn windows_onnx_runtime_downloads_only_the_dll() {
        let a = runtime_asset(LocalRuntimeId::OnnxRuntime, Target::WindowsX64).expect("asset");
        assert_eq!(a.download_bytes(), 5_730_829);
        // 整包下载的运行时按整包计
        let mac = runtime_asset(LocalRuntimeId::OnnxRuntime, Target::MacosArm64).expect("asset");
        assert_eq!(mac.download_bytes(), mac.size);
    }

    #[test]
    fn ocr_models_use_the_worker_file_names() {
        for id in [LocalModelId::PpOcrV6Tiny, LocalModelId::PpOcrV6Small] {
            let names: Vec<&str> = model(id).files.iter().map(|f| f.name).collect();
            assert_eq!(
                names,
                [ocr_files::PPOCR_DET, ocr_files::PPOCR_REC, ocr_files::PPOCR_REC_CONFIG]
            );
        }
        assert!(
            model(LocalModelId::PaddleOcrVl16)
                .files
                .iter()
                .any(|f| f.name == ocr_files::LAYOUT)
        );
    }

    #[test]
    fn targets() {
        assert_eq!(Target::from_consts("macos", "aarch64"), Some(Target::MacosArm64));
        assert_eq!(Target::from_consts("macos", "x86_64"), None);
        assert_eq!(Target::from_consts("windows", "x86_64"), Some(Target::WindowsX64));
        assert_eq!(Target::from_consts("linux", "aarch64"), None);
    }
}
