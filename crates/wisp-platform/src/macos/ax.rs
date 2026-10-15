//! Accessibility（AX）读取：选中文本、选区矩形、选区长度、光标处是否是链接、Electron 的 AXManualAccessibility。
//!
//! 约束：
//! - `kAX*` 常量一律用字符串字面量（部分常量在头文件里是宏，objc2 没有导出）；
//! - 一次取词或一次探测共用**一个**预算（[`Budget`]）：每个 AX 调用之前都把所在元素的消息超时设为剩余预算，
//!   预算用完立即返回 Timeout，不会逐次叠加；
//! - systemWide 元素的消息超时是**进程全局**的（对它设置会改变所有未单独设置超时的元素），因此只设一次、固定为
//!   0.1 s，绝不按某次探测的预算收紧（否则会把并发进行的取词误判为超时）。预算不足 0.1 s 的探测（中键判定）
//!   不经过 systemWide，直接用目标进程的应用元素并按剩余预算设超时；
//! - `APIDisabled` / 未受信任 → 权限缺失；等满超时的 `CannotComplete` → 超时（同一个 pid 不再重试）；
//!   其余错误与空串交给调用方走回退；
//! - 不在主线程调用（由调用方保证：取词在工作线程，中键判定在 tap 线程）。

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Once;
use std::time::{Duration, Instant};

use objc2_application_services::{AXError, AXIsProcessTrusted, AXUIElement, AXValue, AXValueType};
use objc2_core_foundation::{CFBoolean, CFRange, CFRetained, CFString, CFType, CGPoint, CGRect};

use super::focus::frontmost_pid;

/// 默认的 AX 消息超时，也是一次取词中 AX 阶段的总预算
pub(crate) const AX_TIMEOUT: Duration = Duration::from_millis(100);

/// 焦点元素属性
const FOCUSED: &str = "AXFocusedUIElement";

/// systemWide 的全局超时只设置一次
static GLOBAL_TIMEOUT: Once = Once::new();

/// AX 调用失败的归类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AxFail {
    /// 未受信任或 API 被禁用
    PermissionMissing,
    /// 目标应用在超时内没有响应，或预算已经用完
    Timeout,
    /// 没有焦点元素 / 属性不支持 / 没有值
    Unsupported,
    /// 其他错误（附 AXError 数值）
    Other(i32),
}

impl AxFail {
    fn from_error(e: AXError) -> Self {
        match e {
            AXError::APIDisabled => Self::PermissionMissing,
            AXError::CannotComplete => Self::Timeout,
            AXError::AttributeUnsupported
            | AXError::NoValue
            | AXError::NotImplemented
            | AXError::ParameterizedAttributeUnsupported
            | AXError::InvalidUIElement => Self::Unsupported,
            other => Self::Other(other.0),
        }
    }
}

/// 一次取词 / 探测的 AX 总预算
#[derive(Debug, Clone, Copy)]
pub(crate) struct Budget {
    deadline: Instant,
}

impl Budget {
    pub(crate) fn new(total: Duration) -> Self {
        Self {
            deadline: Instant::now() + total,
        }
    }

    /// 剩余预算；已经用完时返回 Timeout
    pub(crate) fn left(&self) -> Result<Duration, AxFail> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() { Err(AxFail::Timeout) } else { Ok(left) }
    }

    pub(crate) fn expired(&self) -> bool {
        self.left().is_err()
    }
}

/// 找焦点元素的路线
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    /// 查找顺序：先 systemWide，拿不到焦点元素（非超时）时改用目标进程的应用元素
    SystemWideFirst,
    /// 只用目标进程的应用元素（超时按剩余预算设置）
    AppOnly,
}

impl Route {
    /// 总预算能容纳一次完整的 systemWide 全局超时才走 systemWide
    pub(crate) fn for_budget(total: Duration) -> Self {
        if total >= AX_TIMEOUT {
            Self::SystemWideFirst
        } else {
            Self::AppOnly
        }
    }
}

/// 进程是否已获辅助功能授权（不弹窗）
pub(crate) fn trusted() -> bool {
    // SAFETY: 无参数的查询函数
    unsafe { AXIsProcessTrusted() }
}

/// 一个 AX 元素
pub(crate) struct Element {
    el: CFRetained<AXUIElement>,
    /// systemWide 元素：超时是进程全局的，不随预算调整
    system: bool,
}

impl Element {
    /// systemWide 元素；第一次使用时把进程全局的默认超时设为 [`AX_TIMEOUT`]，此后不再改动
    pub(crate) fn system_wide() -> Self {
        // SAFETY: 无前置条件
        let el = unsafe { AXUIElement::new_system_wide() };
        GLOBAL_TIMEOUT.call_once(|| set_timeout(&el, AX_TIMEOUT));
        Self { el, system: true }
    }

    /// 某个进程的应用元素（超时由 [`Element::within`] 按预算设置）
    pub(crate) fn application(pid: i32) -> Self {
        // SAFETY: 任意 pid 都安全（不存在时后续调用返回错误）
        let el = unsafe { AXUIElement::new_application(pid) };
        Self { el, system: false }
    }

    /// 把本元素的消息超时设为剩余预算后返回自己；预算已用完时返回 Timeout。systemWide 元素保持全局超时不变
    pub(crate) fn within(&self, budget: &Budget) -> Result<&Self, AxFail> {
        let left = budget.left()?;
        if !self.system {
            set_timeout(&self.el, left);
        }
        Ok(self)
    }

    /// 读取一个属性
    pub(crate) fn attr(&self, name: &str) -> Result<CFRetained<CFType>, AxFail> {
        let key = CFString::from_str(name);
        let mut out: *const CFType = std::ptr::null();
        // SAFETY: out 指向有效的指针槽；成功时得到 +1 引用，由 CFRetained 接管
        let err = unsafe { self.el.copy_attribute_value(&key, NonNull::from(&mut out)) };
        if err != AXError::Success {
            return Err(AxFail::from_error(err));
        }
        NonNull::new(out.cast_mut())
            // SAFETY: Copy 规则返回的 +1 引用
            .map(|p| unsafe { CFRetained::from_raw(p) })
            .ok_or(AxFail::Unsupported)
    }

    /// 读取一个带参数的属性
    pub(crate) fn param_attr(&self, name: &str, param: &CFType) -> Result<CFRetained<CFType>, AxFail> {
        let key = CFString::from_str(name);
        let mut out: *const CFType = std::ptr::null();
        // SAFETY: 同 attr
        let err = unsafe {
            self.el
                .copy_parameterized_attribute_value(&key, param, NonNull::from(&mut out))
        };
        if err != AXError::Success {
            return Err(AxFail::from_error(err));
        }
        NonNull::new(out.cast_mut())
            // SAFETY: Copy 规则返回的 +1 引用
            .map(|p| unsafe { CFRetained::from_raw(p) })
            .ok_or(AxFail::Unsupported)
    }

    /// 属性值是另一个元素时读取它（新元素的超时在下一次 `within` 时设置）
    pub(crate) fn child(&self, name: &str) -> Result<Element, AxFail> {
        let v = self.attr(name)?;
        let el = v.downcast::<AXUIElement>().map_err(|_| AxFail::Unsupported)?;
        Ok(Element { el, system: false })
    }

    /// 字符串属性
    pub(crate) fn string(&self, name: &str) -> Result<String, AxFail> {
        let v = self.attr(name)?;
        let s = v.downcast_ref::<CFString>().ok_or(AxFail::Unsupported)?;
        Ok(s.to_string())
    }

    /// 写入布尔属性
    pub(crate) fn set_bool(&self, name: &str, value: bool) -> Result<(), AxFail> {
        let key = CFString::from_str(name);
        let v: &CFType = CFBoolean::new(value);
        // SAFETY: key / value 都是有效的 CF 对象
        let err = unsafe { self.el.set_attribute_value(&key, v) };
        if err == AXError::Success {
            Ok(())
        } else {
            Err(AxFail::from_error(err))
        }
    }

    /// 屏幕上某一点处的元素（Quartz 全局坐标）
    pub(crate) fn element_at(&self, p: CGPoint) -> Result<Element, AxFail> {
        let mut out: *const AXUIElement = std::ptr::null();
        // SAFETY: out 指向有效的指针槽；成功时得到 +1 引用
        let err = unsafe {
            self.el
                .copy_element_at_position(p.x as f32, p.y as f32, NonNull::from(&mut out))
        };
        if err != AXError::Success {
            return Err(AxFail::from_error(err));
        }
        let el = NonNull::new(out.cast_mut())
            // SAFETY: Copy 规则返回的 +1 引用
            .map(|p| unsafe { CFRetained::from_raw(p) })
            .ok_or(AxFail::Unsupported)?;
        Ok(Element { el, system: false })
    }
}

fn set_timeout(el: &AXUIElement, timeout: Duration) {
    // 0 表示「用全局默认值」，因此最小取 1 ms
    let secs = timeout.as_secs_f32().max(0.001);
    // SAFETY: el 是有效的 AXUIElement
    let _ = unsafe { el.set_messaging_timeout(secs) };
}

/// 取 AXValue 中的 CFRange
fn range_of(v: &CFType) -> Option<CFRange> {
    let v = v.downcast_ref::<AXValue>()?;
    let mut range = CFRange { location: 0, length: 0 };
    // SAFETY: 输出指针指向正确类型的结构体
    let ok = unsafe { v.value(AXValueType::CFRange, NonNull::from(&mut range).cast::<c_void>()) };
    ok.then_some(range)
}

/// 取 AXValue 中的 CGRect
fn rect_of(v: &CFType) -> Option<CGRect> {
    let v = v.downcast_ref::<AXValue>()?;
    let mut rect = CGRect::default();
    // SAFETY: 输出指针指向正确类型的结构体
    let ok = unsafe { v.value(AXValueType::CGRect, NonNull::from(&mut rect).cast::<c_void>()) };
    ok.then_some(rect)
}

/// 目标进程应用元素的焦点元素（超时取剩余预算）
fn app_focused(pid: i32, budget: &Budget) -> Result<Element, AxFail> {
    Element::application(pid).within(budget)?.child(FOCUSED)
}

/// 当前的焦点元素。
///
/// `SystemWideFirst`：systemWide → AXFocusedUIElement；拿不到焦点元素时改用目标进程的应用元素。systemWide 等满了
/// 超时说明目标应用没有响应，对同一个 pid 再试只会再等一次超时，直接返回 Timeout。
/// `AppOnly`：只用目标进程的应用元素，没有 pid 时视为不支持
pub(crate) fn focused_element(target_pid: Option<i32>, budget: &Budget, route: Route) -> Result<Element, AxFail> {
    if route == Route::AppOnly {
        return app_focused(target_pid.ok_or(AxFail::Unsupported)?, budget);
    }
    let system = Element::system_wide();
    let started = Instant::now();
    match system.within(budget)?.child(FOCUSED) {
        Ok(el) => Ok(el),
        Err(AxFail::PermissionMissing) => Err(AxFail::PermissionMissing),
        // 很快返回的 CannotComplete 不是「没有响应」（例如目标刚退出），仍然值得用 pid 再试
        Err(AxFail::Timeout) if started.elapsed() >= AX_TIMEOUT / 2 => Err(AxFail::Timeout),
        Err(first) => match target_pid {
            Some(pid) => app_focused(pid, budget),
            None => Err(first),
        },
    }
}

/// AX 取词的结果
pub(crate) struct AxSelection {
    pub(crate) text: String,
    /// Quartz 全局坐标的选区矩形（拿不到为 None）
    pub(crate) bounds: Option<CGRect>,
}

/// 读取选中文本与（可选的）选区矩形，全部计入 `budget`。文本为空时返回 `Ok(None)`
pub(crate) fn read_selection(
    target_pid: Option<i32>,
    want_bounds: bool,
    budget: &Budget,
    route: Route,
) -> Result<Option<AxSelection>, AxFail> {
    let el = focused_element(target_pid, budget, route)?;
    let text = match el.within(budget)?.string("AXSelectedText") {
        Ok(t) => t,
        Err(AxFail::Unsupported) => return Ok(None),
        Err(e) => return Err(e),
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    let bounds = if want_bounds {
        selection_bounds(&el, budget)
    } else {
        None
    };
    Ok(Some(AxSelection { text, bounds }))
}

/// AXSelectedTextRange → AXBoundsForRange（计入同一个预算；失败或预算用完为 None）
fn selection_bounds(el: &Element, budget: &Budget) -> Option<CGRect> {
    let range = el.within(budget).ok()?.attr("AXSelectedTextRange").ok()?;
    if range_of(&range)?.length <= 0 {
        return None;
    }
    let rect = el.within(budget).ok()?.param_attr("AXBoundsForRange", &range).ok()?;
    rect_of(&rect)
}

/// 选区是否非空（浮标与中键判定用，只读长度，绝不读全文）。`deadline` 内拿不到结果返回 None。
///
/// 目标是前台应用（与取词一致），见 [`has_selection_of`]
pub(crate) fn has_selection(deadline: Duration) -> Option<bool> {
    // 查前台进程（NSWorkspace，负载高时可能要几十毫秒）也算在预算里（不计入的话探测偶尔会超出预算约 80 ms）
    let started = Instant::now();
    let pid = frontmost_pid();
    let left = deadline.checked_sub(started.elapsed()).filter(|d| !d.is_zero())?;
    has_selection_of(pid, left)
}

/// 某个进程的选区是否非空：预算够一次全局超时时先问 systemWide，再用该 pid 的应用元素退路；预算更短时（中键判定）
/// 直接用该 pid 的应用元素并按剩余预算设超时
pub(crate) fn has_selection_of(pid: Option<i32>, deadline: Duration) -> Option<bool> {
    if !trusted() {
        return None;
    }
    let budget = Budget::new(deadline);
    let el = match focused_element(pid, &budget, Route::for_budget(deadline)) {
        Ok(el) => el,
        Err(AxFail::Unsupported) => return Some(false),
        Err(_) => return None,
    };
    let result = match el.within(&budget).ok()?.attr("AXSelectedTextRange") {
        Ok(v) => Some(range_of(&v).is_some_and(|r| r.length > 0)),
        // 不支持文本范围的控件：退回 AXSelectedText（仍只判断空与否）
        Err(AxFail::Unsupported) => match el.within(&budget).ok()?.string("AXSelectedText") {
            Ok(t) => Some(!t.trim().is_empty()),
            Err(AxFail::Unsupported) => Some(false),
            Err(_) => None,
        },
        Err(_) => None,
    };
    result.filter(|_| !budget.expired())
}

/// 光标处是否是链接：AXUIElementCopyElementAtPosition 后向上最多 3 层找 AXLink。
///
/// `owner_pid` 是光标下窗口的属主进程（中键判定从事件里取得）：已知时直接在它的应用元素上命中测试，结果与
/// systemWide 一致且能按预算设超时；未知时，预算够一次全局超时才用 systemWide，否则用前台应用
pub(crate) fn link_at(p: CGPoint, deadline: Duration, owner_pid: Option<i32>) -> Option<bool> {
    if !trusted() {
        return None;
    }
    let budget = Budget::new(deadline);
    let root = match owner_pid {
        Some(pid) => Element::application(pid),
        None if deadline >= AX_TIMEOUT => Element::system_wide(),
        None => Element::application(frontmost_pid()?),
    };
    let mut el = match root.within(&budget).ok()?.element_at(p) {
        Ok(el) => el,
        Err(AxFail::Unsupported) => return Some(false),
        Err(_) => return None,
    };
    for depth in 0..=3 {
        match el.within(&budget).ok()?.string("AXRole") {
            Ok(role) if role == "AXLink" => return Some(true),
            Ok(_) | Err(AxFail::Unsupported) => {}
            Err(_) => return None,
        }
        if depth == 3 {
            break;
        }
        match el.within(&budget).ok()?.child("AXParent") {
            Ok(parent) => el = parent,
            Err(AxFail::Unsupported) => return Some(false),
            Err(_) => return None,
        }
    }
    (!budget.expired()).then_some(false)
}

/// 对某个 Electron 应用打开完整的无障碍树（**不**设置 AXEnhancedUserInterface，它会让窗口动画失常）
pub(crate) fn enable_manual_accessibility(pid: i32, budget: &Budget) -> Result<(), AxFail> {
    Element::application(pid)
        .within(budget)?
        .set_bool("AXManualAccessibility", true)
}
