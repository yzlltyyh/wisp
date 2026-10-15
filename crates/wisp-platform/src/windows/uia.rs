//! UI Automation 取词。
//!
//! 所有 UIA 调用都在一个专用的 MTA 线程上执行：`CUIAutomation8` 惰性创建并复用，Connection / Transaction 超时
//! 设为 100 ms；调用方通过 channel 提交任务并按自己的截止时间等待，等不到就放弃（任务过期后工作线程直接跳过）。
//! 某次调用把工作线程卡住超过 1 s 时，下一次提交会换一个新的工作线程，旧线程在调用返回后自行退出。
//!
//! 取词分两段回复：文本读完立即送出（取词成败只取决于它），选区矩形随后单独送出；调用方只在截止时间之前等矩形，
//! 等不到就不要锚点（退回鼠标位置），已读到的文本绝不会因为取矩形超时而被丢弃、转而走模拟复制。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::{Duration, Instant};

use ::windows::Win32::Foundation::POINT;
use ::windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, SAFEARRAY};
use ::windows::Win32::System::Ole::{
    SafeArrayAccessData, SafeArrayDestroy, SafeArrayGetLBound, SafeArrayGetUBound, SafeArrayUnaccessData,
};
use ::windows::Win32::UI::Accessibility::{
    CUIAutomation, CUIAutomation8, IUIAutomation, IUIAutomation2, IUIAutomationElement, IUIAutomationTextPattern,
    IUIAutomationTextRange, IUIAutomationTreeWalker, UIA_HyperlinkControlTypeId, UIA_TextPatternId,
};
use ::windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
use ::windows::core::{HRESULT, Interface};
use parking_lot::Mutex;

use super::geom::{self, PhysRectF};
use super::util::{ComScope, DpiScope};

/// UIA 的连接 / 事务超时（毫秒）
const UIA_TIMEOUT_MS: u32 = 100;
/// 工作线程被单次调用卡住超过该时长时更换线程
const STUCK_AFTER: Duration = Duration::from_secs(1);
/// 选区最多处理的 range 数
const MAX_RANGES: i32 = 64;
/// 最多处理的矩形数（每个矩形 4 个 double）
const MAX_RECT_VALUES: usize = 4 * 2048;
/// 取选区矩形至少要剩下的预算：`GetBoundingRectangles` 在 Chrome / Word 的大段选区上可能很慢
const RECT_MIN_BUDGET: Duration = Duration::from_millis(30);
/// `UIA_E_TIMEOUT`
const UIA_E_TIMEOUT: HRESULT = HRESULT(0x8013_1505_u32 as i32);

/// 取词结果
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum UiaSelection {
    /// 选区文本（多个 range 用换行连接）与物理像素并集矩形
    Text { text: String, rect: Option<PhysRectF> },
    /// 焦点在密码框：不读取、也不应回退复制
    Password,
    /// 焦点元素不支持 TextPattern
    NoPattern,
}

/// UIA 失败
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UiaError {
    Timeout,
    Failed(String),
}

impl From<::windows::core::Error> for UiaError {
    fn from(e: ::windows::core::Error) -> Self {
        if e.code() == UIA_E_TIMEOUT {
            Self::Timeout
        } else {
            Self::Failed(e.message())
        }
    }
}

/// 取词任务的回复（同一个 channel 上依次送出）
enum SelReply {
    /// 文本阶段：取词的结果（`Text` 的 `rect` 恒为 None）
    Text(Result<UiaSelection, UiaError>),
    /// 矩形阶段：只在文本非空且需要矩形时送出
    Rect(Option<PhysRectF>),
}

#[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
enum Job {
    Selection {
        deadline: Instant,
        want_rect: bool,
        max_chars: i32,
        reply: mpsc::SyncSender<SelReply>,
    },
    HasSelection {
        deadline: Instant,
        reply: mpsc::SyncSender<Option<bool>>,
    },
    LinkAtCursor {
        deadline: Instant,
        reply: mpsc::SyncSender<Option<bool>>,
    },
    /// 中键拦截判定：选区非空且光标不在链接上
    MiddleProbe {
        deadline: Instant,
        reply: mpsc::SyncSender<Option<bool>>,
    },
    /// 预热：创建 UIA 对象，避免首次中键 / 浮标判定超时
    Warm,
}

impl Job {
    fn deadline(&self) -> Option<Instant> {
        match self {
            Self::Selection { deadline, .. }
            | Self::HasSelection { deadline, .. }
            | Self::LinkAtCursor { deadline, .. }
            | Self::MiddleProbe { deadline, .. } => Some(*deadline),
            Self::Warm => None,
        }
    }
}

struct Worker {
    tx: mpsc::Sender<Job>,
    /// 当前任务开始的时刻（相对 BASE 的毫秒数 + 1），0 表示空闲
    busy_since: Arc<AtomicU64>,
}

/// UIA 门面
pub(crate) struct Uia {
    worker: Mutex<Option<Worker>>,
}

fn base() -> Instant {
    static BASE: OnceLock<Instant> = OnceLock::new();
    *BASE.get_or_init(Instant::now)
}

fn now_ms() -> u64 {
    base().elapsed().as_millis() as u64 + 1
}

impl Uia {
    pub(crate) fn new() -> Self {
        Self {
            worker: Mutex::new(None),
        }
    }

    fn sender(&self) -> Option<mpsc::Sender<Job>> {
        let mut guard = self.worker.lock();
        if let Some(w) = guard.as_ref() {
            let since = w.busy_since.load(Ordering::Acquire);
            let stuck = since != 0 && now_ms().saturating_sub(since) > STUCK_AFTER.as_millis() as u64;
            if !stuck {
                return Some(w.tx.clone());
            }
            log::warn!("UIA worker is stuck; starting a fresh one");
        }
        let (tx, rx) = mpsc::channel::<Job>();
        let busy = Arc::new(AtomicU64::new(0));
        let busy_t = Arc::clone(&busy);
        let spawned = std::thread::Builder::new()
            .name("wisp-uia".into())
            .spawn(move || worker_main(rx, busy_t));
        if let Err(e) = spawned {
            log::error!("failed to start UIA worker: {e}");
            *guard = None;
            return None;
        }
        *guard = Some(Worker {
            tx: tx.clone(),
            busy_since: busy,
        });
        Some(tx)
    }

    /// 提交任务；返回是否已交给工作线程
    fn submit(&self, job: Job) -> bool {
        let Some(tx) = self.sender() else { return false };
        match tx.send(job) {
            Ok(()) => true,
            Err(mpsc::SendError(job)) => {
                // 工作线程已经不在了（例如 COM 初始化失败后退出）：换一个新的再试一次
                *self.worker.lock() = None;
                self.sender().is_some_and(|tx| tx.send(job).is_ok())
            }
        }
    }

    fn ask<T>(&self, deadline: Instant, make: impl FnOnce(mpsc::SyncSender<T>) -> Job) -> Option<T> {
        let (tx, rx) = mpsc::sync_channel(1);
        if !self.submit(make(tx)) {
            return None;
        }
        recv_until(&rx, deadline)
    }

    /// 预热（异步，不等待）
    #[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
    pub(crate) fn warm(&self) {
        if let Some(tx) = self.sender() {
            let _ = tx.send(Job::Warm);
        }
    }

    /// 读取焦点元素的选区。文本在截止时间之前到达即算成功；矩形只在截止时间之前等，等不到时 `rect` 为 None
    pub(crate) fn selection(
        &self,
        deadline: Instant,
        want_rect: bool,
        max_chars: usize,
    ) -> Result<UiaSelection, UiaError> {
        let max_chars = i32::try_from(max_chars).unwrap_or(i32::MAX);
        let (tx, rx) = mpsc::sync_channel(2);
        if !self.submit(Job::Selection {
            deadline,
            want_rect,
            max_chars,
            reply: tx,
        }) {
            return Err(UiaError::Failed("UIA worker is unavailable".into()));
        }
        match recv_until(&rx, deadline) {
            Some(SelReply::Text(Ok(UiaSelection::Text { text, .. }))) => {
                let rect = if want_rect && !text.trim().is_empty() {
                    match recv_until(&rx, deadline) {
                        Some(SelReply::Rect(r)) => r,
                        _ => None,
                    }
                } else {
                    None
                };
                Ok(UiaSelection::Text { text, rect })
            }
            Some(SelReply::Text(other)) => other,
            Some(SelReply::Rect(_)) | None => Err(UiaError::Timeout),
        }
    }

    /// 选区是否非空（只读少量字符，绝不模拟复制）
    pub(crate) fn has_selection(&self, deadline: Instant) -> Option<bool> {
        self.ask(deadline, |reply| Job::HasSelection { deadline, reply })
            .flatten()
    }

    /// 光标处是否是链接
    pub(crate) fn link_at_cursor(&self, deadline: Instant) -> Option<bool> {
        self.ask(deadline, |reply| Job::LinkAtCursor { deadline, reply })
            .flatten()
    }

    /// 中键拦截判定（一次往返完成两项检查）
    #[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
    pub(crate) fn middle_probe(&self, deadline: Instant) -> Option<bool> {
        self.ask(deadline, |reply| Job::MiddleProbe { deadline, reply })
            .flatten()
    }
}

/// 在截止时间之前接收一条消息（已过截止时间时只取已经到达的）
fn recv_until<T>(rx: &mpsc::Receiver<T>, deadline: Instant) -> Option<T> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        rx.try_recv().ok()
    } else {
        rx.recv_timeout(left).ok()
    }
}

struct Ctx {
    automation: IUIAutomation,
    walker: Option<IUIAutomationTreeWalker>,
}

impl Ctx {
    fn new() -> ::windows::core::Result<Self> {
        // SAFETY: COM 已在本线程初始化为 MTA；返回的接口只在本线程使用
        let created: ::windows::core::Result<IUIAutomation> =
            unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) };
        let automation = match created {
            Ok(a) => a,
            // Windows 7 没有 CUIAutomation8；Tauri 2 要求 Windows 10+，这里只是保险
            // SAFETY: 同上
            Err(_) => unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }?,
        };
        if let Ok(a2) = automation.cast::<IUIAutomation2>() {
            // SAFETY: 设置本客户端的超时
            unsafe {
                let _ = a2.SetConnectionTimeout(UIA_TIMEOUT_MS);
                let _ = a2.SetTransactionTimeout(UIA_TIMEOUT_MS);
            }
        }
        // SAFETY: 同上
        let walker = unsafe { automation.ControlViewWalker() }.ok();
        Ok(Self { automation, walker })
    }
}

fn worker_main(rx: mpsc::Receiver<Job>, busy: Arc<AtomicU64>) {
    // UIA 返回的矩形与 ElementFromPoint 的坐标按调用线程的 DPI 感知解释：固定为物理像素
    let _dpi = DpiScope::per_monitor();
    let com = ComScope::multithreaded();
    if !com.ok() {
        log::error!("UIA worker: CoInitializeEx(MTA) failed");
    }
    // 线程只在第一次需要 UIA 时创建：立即建好 UIA 对象，首个任务（哪怕调用方已经放弃）之后的调用都不再付初始化成本
    busy.store(now_ms(), Ordering::Release);
    let ctx = match Ctx::new() {
        Ok(c) => Some(c),
        Err(e) => {
            log::error!("UIA unavailable: {e}");
            None
        }
    };
    busy.store(0, Ordering::Release);
    while let Ok(job) = rx.recv() {
        if job.deadline().is_some_and(|d| Instant::now() >= d) {
            continue; // 调用方已放弃
        }
        busy.store(now_ms(), Ordering::Release);
        run(ctx.as_ref(), job);
        busy.store(0, Ordering::Release);
    }
}

fn run(ctx: Option<&Ctx>, job: Job) {
    match job {
        Job::Warm => {}
        Job::Selection {
            deadline,
            want_rect,
            max_chars,
            reply,
        } => {
            let Some(c) = ctx else {
                let _ = reply.try_send(SelReply::Text(Err(UiaError::Failed("UIA is unavailable".into()))));
                return;
            };
            match read_selection(c, deadline, max_chars) {
                Ok((sel, ranges)) => {
                    let has_text = matches!(&sel, UiaSelection::Text { text, .. } if !text.trim().is_empty());
                    // 先把文本送出去：调用方拿到文本就不会再走模拟复制
                    let _ = reply.try_send(SelReply::Text(Ok(sel)));
                    if want_rect && has_text {
                        let _ = reply.try_send(SelReply::Rect(selection_rect(&ranges, deadline)));
                    }
                }
                Err(e) => {
                    let _ = reply.try_send(SelReply::Text(Err(e)));
                }
            }
        }
        Job::HasSelection { deadline, reply } => {
            let _ = reply.try_send(ctx.and_then(|c| has_selection(c, deadline)));
        }
        Job::LinkAtCursor { deadline, reply } => {
            let _ = reply.try_send(ctx.and_then(|c| link_at_cursor(c, deadline)));
        }
        Job::MiddleProbe { deadline, reply } => {
            let r = ctx.and_then(|c| match has_selection(c, deadline)? {
                false => Some(false),
                true => link_at_cursor(c, deadline).map(|on_link| !on_link),
            });
            let _ = reply.try_send(r);
        }
    }
}

fn check(deadline: Instant) -> Result<(), UiaError> {
    if Instant::now() >= deadline {
        Err(UiaError::Timeout)
    } else {
        Ok(())
    }
}

fn focused_text_pattern(ctx: &Ctx) -> Result<Result<IUIAutomationTextPattern, UiaSelection>, UiaError> {
    // SAFETY: 以下 COM 调用都在创建这些接口的 MTA 线程上进行
    let el: IUIAutomationElement = unsafe { ctx.automation.GetFocusedElement() }?;
    // SAFETY: 同上
    if unsafe { el.CurrentIsPassword() }.is_ok_and(|b| b.as_bool()) {
        return Ok(Err(UiaSelection::Password));
    }
    // SAFETY: 同上；不支持该模式时返回错误
    match unsafe { el.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId) } {
        Ok(p) => Ok(Ok(p)),
        Err(e) if e.code() == UIA_E_TIMEOUT => Err(UiaError::Timeout),
        Err(_) => Ok(Err(UiaSelection::NoPattern)),
    }
}

/// 读选区文本（`rect` 留空），同时返回各个 range 供随后取矩形
fn read_selection(
    ctx: &Ctx,
    deadline: Instant,
    max_chars: i32,
) -> Result<(UiaSelection, Vec<IUIAutomationTextRange>), UiaError> {
    let pattern = match focused_text_pattern(ctx)? {
        Ok(p) => p,
        Err(other) => return Ok((other, Vec::new())),
    };
    check(deadline)?;
    // SAFETY: 同 MTA 线程
    let ranges = unsafe { pattern.GetSelection() }?;
    // SAFETY: 同上
    let n = unsafe { ranges.Length() }?.clamp(0, MAX_RANGES);
    let mut parts: Vec<String> = Vec::with_capacity(n as usize);
    let mut kept: Vec<IUIAutomationTextRange> = Vec::with_capacity(n as usize);
    let mut budget = max_chars;
    for i in 0..n {
        check(deadline)?;
        // SAFETY: 同上
        let range: IUIAutomationTextRange = unsafe { ranges.GetElement(i) }?;
        if budget > 0 {
            // SAFETY: 同上
            let text = unsafe { range.GetText(budget) }?;
            let s = String::from_utf16_lossy(&text);
            budget = budget.saturating_sub(i32::try_from(text.len()).unwrap_or(i32::MAX));
            if !s.is_empty() {
                parts.push(s);
            }
        }
        kept.push(range);
    }
    let sel = UiaSelection::Text {
        text: parts.join("\n"),
        rect: None,
    };
    Ok((sel, kept))
}

/// 选区的屏幕矩形（物理像素）。range 多于两个时只取首尾两个（选区的上下边界都在其中，锚点只需要外接矩形）；
/// 每次调用前剩余预算不足 [`RECT_MIN_BUDGET`] 就整体放弃（不完整的外接矩形会把面板放到选区中间），返回 None
fn selection_rect(ranges: &[IUIAutomationTextRange], deadline: Instant) -> Option<PhysRectF> {
    let picks: Vec<&IUIAutomationTextRange> = match ranges {
        [] => return None,
        [only] => vec![only],
        [first, .., last] => vec![first, last],
    };
    let mut rect: Option<PhysRectF> = None;
    for range in picks {
        if deadline.saturating_duration_since(Instant::now()) < RECT_MIN_BUDGET {
            return None;
        }
        if let Some(r) = bounding_rect(range) {
            rect = Some(match rect {
                Some(acc) => acc.union(&r),
                None => r,
            });
        }
    }
    rect
}

fn has_selection(ctx: &Ctx, deadline: Instant) -> Option<bool> {
    let pattern = match focused_text_pattern(ctx).ok()? {
        Ok(p) => p,
        Err(_) => return Some(false),
    };
    // SAFETY: 同 MTA 线程
    let ranges = unsafe { pattern.GetSelection() }.ok()?;
    // SAFETY: 同上
    let n = unsafe { ranges.Length() }.ok()?.clamp(0, 8);
    for i in 0..n {
        if Instant::now() >= deadline {
            return None;
        }
        // SAFETY: 同上
        let range = unsafe { ranges.GetElement(i) }.ok()?;
        // SAFETY: 同上；只取前 64 个字符判断是否全是空白
        let text = unsafe { range.GetText(64) }.ok()?;
        if !String::from_utf16_lossy(&text).trim().is_empty() {
            return Some(true);
        }
    }
    Some(false)
}

fn link_at_cursor(ctx: &Ctx, deadline: Instant) -> Option<bool> {
    let mut pt = POINT::default();
    // SAFETY: 输出参数指向局部变量；本线程是 Per-Monitor V2，得到物理像素
    unsafe { GetCursorPos(&mut pt) }.ok()?;
    // SAFETY: 同 MTA 线程
    let mut el = unsafe { ctx.automation.ElementFromPoint(pt) }.ok()?;
    // 自身及向上 3 层
    for depth in 0..4 {
        if Instant::now() >= deadline {
            return None;
        }
        // SAFETY: 同上
        if unsafe { el.CurrentControlType() }.ok()? == UIA_HyperlinkControlTypeId {
            return Some(true);
        }
        if depth == 3 {
            break;
        }
        let Some(walker) = ctx.walker.as_ref() else { break };
        // SAFETY: 同上；到达根时返回错误
        match unsafe { walker.GetParentElement(&el) } {
            Ok(parent) => el = parent,
            Err(_) => break,
        }
    }
    Some(false)
}

/// 某 range 的屏幕矩形并集（物理像素）
fn bounding_rect(range: &IUIAutomationTextRange) -> Option<PhysRectF> {
    // SAFETY: 同 MTA 线程；返回的 SAFEARRAY 归本进程所有，用完销毁
    let sa: *mut SAFEARRAY = unsafe { range.GetBoundingRectangles() }.ok()?;
    if sa.is_null() {
        return None;
    }
    let values = read_f64_array(sa);
    // SAFETY: 由 GetBoundingRectangles 分配，只销毁一次
    unsafe {
        let _ = SafeArrayDestroy(sa);
    }
    geom::union_of_flat_rects(&values?)
}

fn read_f64_array(sa: *mut SAFEARRAY) -> Option<Vec<f64>> {
    // SAFETY: sa 是有效的一维 VT_R8 SAFEARRAY（GetBoundingRectangles 的约定）
    unsafe {
        let lb = SafeArrayGetLBound(sa, 1).ok()?;
        let ub = SafeArrayGetUBound(sa, 1).ok()?;
        if ub < lb {
            return Some(Vec::new());
        }
        let n = ((ub - lb + 1) as usize).min(MAX_RECT_VALUES);
        let mut data: *mut std::ffi::c_void = std::ptr::null_mut();
        SafeArrayAccessData(sa, &mut data).ok()?;
        let out = if data.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(data as *const f64, n).to_vec()
        };
        let _ = SafeArrayUnaccessData(sa);
        Some(out)
    }
}
