//! 错误类型。
//!
//! [`LocalError`] 是本 crate 内部与 desktop 之间的错误；给用户看的一律先转成 `LlmError`（[`LocalError::to_llm_error`]）：
//! 推理进程的失败归为 `ErrorCode::LocalRuntime`，原因放在 `hint`（`errors.hint.local.*`，前端 errors.json 里两种语言都有）。
//! 错误信息只写固定的英文简述，不带路径、地址、内容或子进程的输出。

use wisp_types::{DownloadErrorCode, ErrorCode, LlmError};

/// 推理进程（llama-server、ORT 工作进程）失败的原因
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeFailure {
    /// 可用内存不足以加载模型（加载前的内存检查，或进程报告内存分配失败）
    OutOfMemory,
    /// Windows：缺少 VC++ 运行库（进程以 `STATUS_DLL_NOT_FOUND` 退出）
    VcRuntimeMissing,
    /// Linux：缺少系统库（glibc 版本过旧、libgomp 等）
    SystemLibraryMissing,
    /// 显卡后端初始化失败或输出为空：建议关闭 GPU 加速
    GpuFailed,
    /// 进程没能启动（可执行文件缺失、没有执行权限、被安全软件拦截）
    StartFailed,
    /// 进程在运行中意外退出
    Crashed,
    /// 模型或运行时文件缺失、大小不符：需要重新下载
    ModelCorrupt,
    /// 加载超过时限（慢盘上的大模型）
    LoadTimeout,
}

impl RuntimeFailure {
    /// 前端 i18n key（`errors.hint.local.<name>`）
    pub const fn hint_key(self) -> &'static str {
        match self {
            Self::OutOfMemory => "errors.hint.local.out_of_memory",
            Self::VcRuntimeMissing => "errors.hint.local.vc_runtime_missing",
            Self::SystemLibraryMissing => "errors.hint.local.system_library_missing",
            Self::GpuFailed => "errors.hint.local.gpu_failed",
            Self::StartFailed => "errors.hint.local.start_failed",
            Self::Crashed => "errors.hint.local.crashed",
            Self::ModelCorrupt => "errors.hint.local.model_corrupt",
            Self::LoadTimeout => "errors.hint.local.load_timeout",
        }
    }

    /// 英文简述（错误信息与日志用）
    pub const fn describe(self) -> &'static str {
        match self {
            Self::OutOfMemory => "not enough memory to load the model",
            Self::VcRuntimeMissing => "the Visual C++ runtime is missing",
            Self::SystemLibraryMissing => "a required system library is missing",
            Self::GpuFailed => "the GPU backend failed",
            Self::StartFailed => "the inference process could not start",
            Self::Crashed => "the inference process exited unexpectedly",
            Self::ModelCorrupt => "model files are missing or damaged",
            Self::LoadTimeout => "loading the model timed out",
        }
    }

    /// 给用户看的错误（`ErrorCode::LocalRuntime` + hint）。崩溃与超时可以重试
    pub fn to_llm_error(self) -> LlmError {
        let mut e = LlmError::new(ErrorCode::LocalRuntime, self.describe());
        e.hint = Some(self.hint_key().to_owned());
        e.retryable = matches!(self, Self::Crashed | Self::LoadTimeout | Self::GpuFailed);
        e
    }
}

/// 本 crate 的错误
#[derive(Debug, thiserror::Error)]
pub enum LocalError {
    /// 功能尚未实现（返回给调用方的固定错误，不 panic）
    #[error("{0} is not implemented yet")]
    Unimplemented(&'static str),
    /// 调用方取消（暂停 / 取消下载、取消识别）
    #[error("cancelled")]
    Cancelled,
    /// 模型（或它需要的运行时）没有安装
    #[error("the model is not installed")]
    NotInstalled,
    /// 下载失败
    #[error("download failed: {0:?}")]
    Download(DownloadErrorCode),
    /// 推理进程失败
    #[error("{}", .0.describe())]
    Runtime(RuntimeFailure),
    /// 本机文件读写失败（`std::io::Error` 的文字不含路径）
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// 对端（工作进程、llama-server、云端服务）的回应不合预期；内容是固定的英文简述
    #[error("protocol: {0}")]
    Protocol(&'static str),
}

impl LocalError {
    /// 给用户看的错误
    pub fn to_llm_error(&self) -> LlmError {
        match self {
            Self::Cancelled => LlmError::new(ErrorCode::Cancelled, "cancelled"),
            Self::Runtime(f) => f.to_llm_error(),
            Self::NotInstalled => {
                let mut e = LlmError::new(ErrorCode::NotConfigured, "the offline model is not installed");
                e.hint = Some(RuntimeFailure::ModelCorrupt.hint_key().to_owned());
                e
            }
            Self::Download(_) | Self::Io(_) | Self::Protocol(_) | Self::Unimplemented(_) => {
                LlmError::new(ErrorCode::Internal, self.to_string())
            }
        }
    }
}

impl From<RuntimeFailure> for LocalError {
    fn from(f: RuntimeFailure) -> Self {
        Self::Runtime(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_failures_carry_local_hints() {
        let e = RuntimeFailure::GpuFailed.to_llm_error();
        assert_eq!(e.code, ErrorCode::LocalRuntime);
        assert_eq!(e.hint.as_deref(), Some("errors.hint.local.gpu_failed"));
        assert_eq!(
            LocalError::Cancelled.to_llm_error().code,
            ErrorCode::Cancelled,
            "取消不出错误卡片"
        );
    }
}
