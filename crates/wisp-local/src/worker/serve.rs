//! 工作进程一侧：读请求、执行、写响应。

use std::io::{self, BufReader, BufWriter, Write};

use super::frame::{self, Request, Response, WorkerError};

/// 在 stdin / stdout 上服务，返回退出码（0 = 正常结束；1 = 管道或协议错误）
pub(super) fn serve_stdio() -> i32 {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = BufReader::new(stdin.lock());
    let mut output = BufWriter::new(stdout.lock());
    let mut engines = Engines::default();
    loop {
        let frame = match frame::read_frame(&mut input) {
            Ok(Some(f)) => f,
            Ok(None) => return 0,
            Err(_) => return 1,
        };
        let response = match serde_json::from_slice::<Request>(&frame.header) {
            Ok(Request::Exit) => return 0,
            Ok(Request::Ping) => Response::ok(),
            Ok(req) => engines.handle(req, frame.payload),
            Err(_) => Response::err(WorkerError::BadRequest),
        };
        let Ok(header) = serde_json::to_vec(&response) else {
            return 1;
        };
        if frame::write_frame(&mut output, &header, &[])
            .and_then(|()| output.flush())
            .is_err()
        {
            return 1;
        }
    }
}

/// ONNX Runtime 动态库加载失败的原因。`op` 是加载器报的操作（`LoadLibraryExW failed` / `dlopen failed`），`detail` 是系统给的
/// 说明；只看其中固定的特征，原文不保留（里面有本机路径）。按报错本身判断而不是按操作系统猜：同一个系统上也可能是别的原因
/// （例如库校验拒绝、被安全软件拦截），那些归为 [`WorkerError::RuntimeLoad`]
#[cfg_attr(
    not(feature = "ort"),
    allow(dead_code, reason = "只在编入 ONNX Runtime 的构建里加载动态库")
)]
fn classify_runtime_load(op: &str, detail: &str) -> WorkerError {
    let detail = detail.to_ascii_lowercase();
    if op.starts_with("LoadLibraryExW") {
        // ERROR_MOD_NOT_FOUND（126）：主文件在（加载前已检查），找不到的是它依赖的 DLL。官方构建除系统 DLL 外只依赖
        // Visual C++ 运行库（MSVCP140 / VCRUNTIME140_1 等）；Wisp 自己不用 C++ 标准库，Wisp 能运行不代表它们在
        return if detail.contains("(os error 126)") {
            WorkerError::VcRuntimeMissing
        } else {
            WorkerError::RuntimeLoad
        };
    }
    // 依赖的共享库不存在，或系统库太旧（glibc / libstdc++ 的符号版本不满足、缺符号）
    let missing = [
        "cannot open shared object file",
        "library not loaded",
        "undefined symbol",
        "symbol not found",
    ]
    .iter()
    .any(|k| detail.contains(k))
        || (detail.contains("version `") && detail.contains("not found"));
    if op.starts_with("dlopen") && missing {
        WorkerError::SystemLibraryMissing
    } else {
        WorkerError::RuntimeLoad
    }
}

/// 一边的像素上限：与主进程侧的截图上限（4096）留足余量，防止坏请求让工作进程分配巨量内存
const MAX_SIDE: u32 = 16_384;

/// 负载是不是 `width × height` 的 RGB8 像素
fn rgb_image(width: u32, height: u32, payload: Vec<u8>) -> Result<image::RgbImage, WorkerError> {
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return Err(WorkerError::BadRequest);
    }
    image::RgbImage::from_raw(width, height, payload).ok_or(WorkerError::BadRequest)
}

/// 按需创建、进程内复用的推理会话
#[derive(Default)]
struct Engines {
    #[cfg(feature = "ort")]
    ort: ort_engines::OrtEngines,
}

impl Engines {
    fn handle(&mut self, req: Request, payload: Vec<u8>) -> Response {
        let result = match req {
            Request::Ppocr {
                tier,
                width,
                height,
                max_side,
            } => rgb_image(width, height, payload).and_then(|img| self.ppocr(tier, img, max_side)),
            Request::Layout { width, height } => rgb_image(width, height, payload).and_then(|img| self.layout(img)),
            // 主循环已处理
            Request::Ping | Request::Exit => Ok(Response::ok()),
        };
        result.unwrap_or_else(Response::err)
    }

    #[cfg(feature = "ort")]
    fn ppocr(
        &mut self,
        tier: crate::ocr::PpOcrTier,
        image: image::RgbImage,
        max_side: u32,
    ) -> Result<Response, WorkerError> {
        let lines = self.ort.ppocr(tier, image, max_side)?;
        Ok(Response {
            ok: true,
            lines: Some(lines),
            ..Response::default()
        })
    }

    #[cfg(feature = "ort")]
    fn layout(&mut self, image: image::RgbImage) -> Result<Response, WorkerError> {
        let boxes = self.ort.layout(&image)?;
        Ok(Response {
            ok: true,
            boxes: Some(boxes),
            ..Response::default()
        })
    }

    // 这个构建没有编入 ONNX Runtime
    #[cfg(not(feature = "ort"))]
    fn ppocr(
        &mut self,
        _tier: crate::ocr::PpOcrTier,
        _image: image::RgbImage,
        _max_side: u32,
    ) -> Result<Response, WorkerError> {
        Err(WorkerError::Unsupported)
    }

    #[cfg(not(feature = "ort"))]
    fn layout(&mut self, _image: image::RgbImage) -> Result<Response, WorkerError> {
        Err(WorkerError::Unsupported)
    }
}

#[cfg(feature = "ort")]
mod ort_engines {
    use std::path::PathBuf;
    use std::sync::Arc;

    use wisp_types::LocalModelId;

    use crate::error::{LocalError, RuntimeFailure};
    use crate::ocr::layout::{LayoutBox, LayoutModel};
    use crate::ocr::ppocr::PpOcrEngine;
    use crate::ocr::{PpOcrTier, TextLine, files};
    use crate::store::Store;
    use crate::worker::frame::WorkerError;
    use crate::worker::{ENV_DYLIB, ENV_MODELS, ENV_THREADS};

    /// 没有给线程数时的默认值
    const DEFAULT_THREADS: u32 = 2;

    /// ONNX Runtime 的环境与会话。环境每个进程只能建一次（动态库加载后也不能卸载），所以第一次推理时才建；
    /// 会话按档位懒建，之后一直复用到进程结束
    #[derive(Default)]
    pub(super) struct OrtEngines {
        runtime: Option<Result<Runtime, WorkerError>>,
        tiny: Option<PpOcrEngine>,
        small: Option<PpOcrEngine>,
        layout: Option<LayoutModel>,
    }

    struct Runtime {
        store: Store,
        threads: u32,
    }

    impl OrtEngines {
        pub(super) fn ppocr(
            &mut self,
            tier: PpOcrTier,
            image: image::RgbImage,
            max_side: u32,
        ) -> Result<Vec<TextLine>, WorkerError> {
            let (store, threads) = self.runtime()?;
            let slot = match tier {
                PpOcrTier::Tiny => &mut self.tiny,
                PpOcrTier::Small => &mut self.small,
            };
            if slot.is_none() {
                let dir = store.model_dir(tier.model());
                *slot = Some(PpOcrEngine::load(&dir, tier, threads).map_err(load_error)?);
            }
            let engine = slot.as_mut().ok_or(WorkerError::ModelLoad)?;
            engine.recognize(image, max_side).map_err(inference_error)
        }

        pub(super) fn layout(&mut self, image: &image::RgbImage) -> Result<Vec<LayoutBox>, WorkerError> {
            let (store, threads) = self.runtime()?;
            if self.layout.is_none() {
                let path = store.model_dir(LocalModelId::PaddleOcrVl16).join(files::LAYOUT);
                if !path.is_file() {
                    return Err(WorkerError::ModelLoad);
                }
                self.layout = Some(LayoutModel::load(&path, threads).map_err(load_error)?);
            }
            let model = self.layout.as_mut().ok_or(WorkerError::ModelLoad)?;
            model.detect(image).map_err(inference_error)
        }

        /// 第一次调用时加载动态库、建环境；失败后不再重试（同一进程里重试也不会成功）
        fn runtime(&mut self) -> Result<(Store, u32), WorkerError> {
            let rt = self.runtime.get_or_insert_with(init_runtime);
            match rt {
                Ok(rt) => Ok((rt.store.clone(), rt.threads)),
                Err(e) => Err(*e),
            }
        }
    }

    fn init_runtime() -> Result<Runtime, WorkerError> {
        let dylib = std::env::var_os(ENV_DYLIB)
            .map(PathBuf::from)
            .ok_or(WorkerError::RuntimeLoad)?;
        let models = std::env::var_os(ENV_MODELS)
            .map(PathBuf::from)
            .ok_or(WorkerError::ModelLoad)?;
        let threads = std::env::var(ENV_THREADS)
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_THREADS);
        // 动态库文件不在：下载不完整或被删了，需要重新下载；在但加载不了：缺系统运行库、库校验拒绝等
        if !dylib.is_file() {
            return Err(WorkerError::ModelLoad);
        }
        let builder = ort::init_from(&dylib).map_err(|e| load_dynamic_error(&e))?;
        // 遥测：环境变量 ORT_DISABLE_TELEMETRY=1 已由主进程设好（必须在建环境之前），这里再关一次 API 级的开关。
        // ONNX Runtime 自己的日志全部丢弃：工作进程的 stderr 不应出现任何内容
        let _ = builder
            .with_name("wisp")
            .with_telemetry(false)
            .with_logger(Arc::new(|_, _, _, _, _| {}))
            .commit();
        Ok(Runtime {
            store: Store::new(models),
            threads,
        })
    }

    /// 动态库加载失败 → 错误种类。导出表或版本不对说明文件不是这个版本的 ONNX Runtime（下载不完整或被替换），按模型损坏处理
    fn load_dynamic_error(e: &ort::LoadDynamicError) -> WorkerError {
        use std::error::Error as _;
        match e {
            ort::LoadDynamicError::Dlopen { error, .. } => super::classify_runtime_load(
                &error.to_string(),
                &error.source().map(ToString::to_string).unwrap_or_default(),
            ),
            ort::LoadDynamicError::MissingApi { .. } | ort::LoadDynamicError::BadVersion { .. } => {
                WorkerError::ModelLoad
            }
        }
    }

    fn load_error(e: LocalError) -> WorkerError {
        match e {
            LocalError::Runtime(RuntimeFailure::OutOfMemory) => WorkerError::OutOfMemory,
            _ => WorkerError::ModelLoad,
        }
    }

    fn inference_error(e: LocalError) -> WorkerError {
        match e {
            LocalError::Runtime(RuntimeFailure::OutOfMemory) => WorkerError::OutOfMemory,
            LocalError::NotInstalled => WorkerError::ModelLoad,
            _ => WorkerError::Inference,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr::PpOcrTier;

    /// 加载器报错取自 libloading 的格式：`LoadLibraryExW failed` + 系统错误（`… (os error N)`，说明文字随系统语言），
    /// `dlopen failed` + dlerror 的原文
    #[test]
    fn runtime_load_failures_are_classified_by_the_loader_message() {
        use WorkerError::*;
        let win = |d: &str| classify_runtime_load("LoadLibraryExW failed", d);
        assert_eq!(win("找不到指定的模块。 (os error 126)"), VcRuntimeMissing);
        assert_eq!(
            win("The specified module could not be found. (os error 126)"),
            VcRuntimeMissing
        );
        assert_eq!(win("%1 is not a valid Win32 application. (os error 193)"), RuntimeLoad);
        assert_eq!(win("Access is denied. (os error 5)"), RuntimeLoad);
        let unix = |d: &str| classify_runtime_load("dlopen failed", d);
        assert_eq!(
            unix(
                "/lib/x86_64-linux-gnu/libm.so.6: version `GLIBC_2.29' not found (required by /x/libonnxruntime.so.1.28.3)"
            ),
            SystemLibraryMissing
        );
        assert_eq!(
            unix("libstdc++.so.6: cannot open shared object file: No such file or directory"),
            SystemLibraryMissing
        );
        assert_eq!(
            unix("dlopen(/x/libonnxruntime.1.28.3.dylib, 0x0005): Symbol not found: _xyz"),
            SystemLibraryMissing
        );
        assert_eq!(
            unix(
                "dlopen(/x/libonnxruntime.1.28.3.dylib, 0x0005): tried: '/x/libonnxruntime.1.28.3.dylib' (code signature \
                 in <ABCD> '/x/libonnxruntime.1.28.3.dylib' not valid for use in process)"
            ),
            RuntimeLoad
        );
        assert_eq!(
            classify_runtime_load("dlopen failed, but system did not report the error", ""),
            RuntimeLoad
        );
    }

    #[test]
    fn malformed_pixels_are_bad_requests() {
        let mut engines = Engines::default();
        let req = Request::Ppocr {
            tier: PpOcrTier::Tiny,
            width: 4,
            height: 2,
            max_side: 2048,
        };
        // 4 × 2 × 3 = 24 字节，少一个字节即拒绝
        let r = engines.handle(req, vec![0; 23]);
        assert_eq!(r, Response::err(WorkerError::BadRequest));
        let r = engines.handle(Request::Layout { width: 0, height: 5 }, Vec::new());
        assert_eq!(r, Response::err(WorkerError::BadRequest));
    }
}
