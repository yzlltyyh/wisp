//! Windows 实现共用的小工具：宽字符串、可跨线程的窗口句柄、带超时的阻塞调用、线程级 DPI / COM 作用域、
//! 注册表读取与内核句柄守卫。

use std::ffi::c_void;
use std::sync::mpsc;
use std::time::Duration;

use ::windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, HWND};
use ::windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize,
};
use ::windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use ::windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use ::windows::core::PCWSTR;

/// Wisp 注入的输入事件在 `dwExtraInfo` 里携带的标记。自己的低级钩子见到它一律放行，
/// 避免把自己模拟的按键 / 鼠标当成用户输入
pub(crate) const WISP_TAG: usize = crate::common::WISP_INPUT_TAG as usize;

/// 把 Rust 字符串转成以 NUL 结尾的 UTF-16
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 从定长、以 NUL 结尾（或填满）的 UTF-16 缓冲区取出字符串
pub(crate) fn from_wide_buf(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

/// 可以跨线程传递、存放的窗口句柄。
///
/// `HWND` 内含裸指针，不是 `Send`；但窗口句柄本身是会话内全局有效的整数，只要不解引用就可以安全地在线程间传递，
/// 真正使用前再用 `IsWindow` 等函数校验
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(crate) struct RawHwnd(pub isize);

impl RawHwnd {
    /// 包装一个窗口句柄
    pub(crate) fn new(hwnd: HWND) -> Self {
        Self(hwnd.0 as isize)
    }

    /// 取回窗口句柄
    pub(crate) fn hwnd(self) -> HWND {
        HWND(self.0 as *mut c_void)
    }

    /// 是否为空句柄
    pub(crate) fn is_null(self) -> bool {
        self.0 == 0
    }

    /// `AppRef.window` 使用的整数形式
    pub(crate) fn as_u64(self) -> u64 {
        self.0 as u64
    }

    /// 从 `AppRef.window` 还原
    pub(crate) fn from_u64(v: u64) -> Self {
        Self(v as isize)
    }
}

/// 在临时线程上执行一个可能被其他进程卡住的调用（剪贴板的延迟渲染、`WM_DESTROYCLIPBOARD` 等都会同步等待
/// 剪贴板所有者）。超时后不再等待：返回 None，临时线程在调用返回后自行结束（阻塞调用一律有硬超时）
pub(crate) fn run_with_timeout<T: Send + 'static>(
    name: &str,
    timeout: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    spawn_call(name, f)?.recv_timeout(timeout).ok()
}

/// 在临时线程上执行 `f`，返回接收结果的 channel（调用方可以分阶段等待，例如先等正常时限、再有界地多等一会儿）。
/// 结果在 `f` 返回之后才送出：收到结果即说明 `f` 持有的资源（例如打开的剪贴板）都已释放
pub(crate) fn spawn_call<T: Send + 'static>(
    name: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> Option<mpsc::Receiver<T>> {
    let (tx, rx) = mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new().name(name.to_owned()).spawn(move || {
        let _ = tx.send(f());
    });
    match spawned {
        Ok(_) => Some(rx),
        Err(e) => {
            log::warn!("failed to spawn {name}: {e}");
            None
        }
    }
}

/// 执行上层传入的回调并兜住 panic：回调运行在窗口过程、WinEvent 回调、钩子分发线程等位置，panic 既不能跨越
/// `extern "system"` 边界（会直接 abort 整个常驻进程），也不能让分发线程静默退出（之后的事件全部丢失）。
/// panic 时记 error 日志并返回 None。
///
/// 注意：工作区的发布版 profile 是 `panic = "abort"`，那里任何 panic 都会直接终止进程，`catch_unwind` 不起作用；
/// 这层兜底在开发 / 测试构建（unwind）中生效。回调本身仍必须保证不 panic
pub(crate) fn call_guarded<R>(what: &str, f: impl FnOnce() -> R) -> Option<R> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => Some(r),
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic payload".to_owned());
            log::error!("{what} panicked: {msg}");
            None
        }
    }
}

/// 毫秒数（饱和到 u32，用于日志与 `elapsed_ms`）
pub(crate) fn millis(d: Duration) -> u32 {
    u32::try_from(d.as_millis()).unwrap_or(u32::MAX)
}

/// 线程级 DPI 感知作用域：作用域内的坐标类 API（`GetCursorPos`、`GetMonitorInfoW`、UIA 矩形等）一律返回物理像素，
/// 不受进程默认 DPI 感知（单测进程是不感知的）影响；离开作用域时恢复原值
pub(crate) struct DpiScope {
    previous: DPI_AWARENESS_CONTEXT,
}

impl DpiScope {
    /// 切换为 Per-Monitor V2
    pub(crate) fn per_monitor() -> Self {
        // SAFETY: 只修改当前线程的 DPI 感知上下文；参数是系统预定义的伪句柄
        let previous = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        Self { previous }
    }
}

impl Drop for DpiScope {
    fn drop(&mut self) {
        if !self.previous.0.is_null() {
            // SAFETY: 恢复进入作用域之前由系统返回的上下文
            unsafe {
                SetThreadDpiAwarenessContext(self.previous);
            }
        }
    }
}

/// 线程级 COM 初始化作用域：初始化成功（含 `S_FALSE`）时在离开作用域时配对调用 `CoUninitialize`；
/// 线程已被初始化成另一种套间（`RPC_E_CHANGED_MODE`）时不做任何事，沿用现有套间
pub(crate) struct ComScope {
    initialized: bool,
}

impl ComScope {
    /// 单线程套间（Shell 图标等 API 要求）
    pub(crate) fn apartment() -> Self {
        // SAFETY: 标准的 COM 初始化；成功时在 Drop 中配对反初始化
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        Self {
            initialized: hr.is_ok(),
        }
    }

    /// 多线程套间（UIA 客户端线程）
    pub(crate) fn multithreaded() -> Self {
        // SAFETY: 同上
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Self {
            initialized: hr.is_ok(),
        }
    }

    /// 本作用域是否完成了初始化（或线程本来就已初始化为同一套间）
    pub(crate) fn ok(&self) -> bool {
        self.initialized
    }
}

impl Drop for ComScope {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: 与构造时成功的 CoInitializeEx 配对
            unsafe { CoUninitialize() };
        }
    }
}

/// 读取 `HKEY_CURRENT_USER\<subkey>` 下的 DWORD 值；不存在或类型不符时返回 None
pub(crate) fn reg_dword(subkey: &str, value: &str) -> Option<u32> {
    let subkey = wide(subkey);
    let value = wide(value);
    let mut data: u32 = 0;
    let mut len = std::mem::size_of::<u32>() as u32;
    // SAFETY: 两个字符串以 NUL 结尾且在调用期间存活；输出缓冲区是一个 u32，长度如实传入
    let err = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut c_void),
            Some(&mut len),
        )
    };
    (err == ERROR_SUCCESS).then_some(data)
}

/// 内核句柄守卫：Drop 时 `CloseHandle`
pub(crate) struct OwnedHandle(pub HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: 句柄由本守卫独占持有，只关闭一次
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// 毫秒级 tick 之差（`GetTickCount` 语义，与低级钩子结构体里的 `time` 字段同源；按回绕处理）。只有扩展触发
/// （双击判定、浮标判定的时间窗）用到
#[cfg(feature = "mouse-hook")]
pub(crate) fn tick_delta(newer: u32, older: u32) -> u32 {
    newer.wrapping_sub(older)
}
