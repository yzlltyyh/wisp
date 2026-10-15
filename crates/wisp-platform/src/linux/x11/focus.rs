//! 前台应用：`_NET_ACTIVE_WINDOW` → WM_CLASS / `_NET_WM_PID`；激活（发
//! `_NET_ACTIVE_WINDOW` ClientMessage，来源标记为 2 = pager，窗口管理器不会因「防止抢焦点」而拒绝）。
//!
//! hub 线程监听根窗口的 `_NET_ACTIVE_WINDOW` 变化，持续记录最近一个「不是 Wisp、不是桌面 / 面板」的前台窗口，
//! 所以从托盘、固定窗口触发时仍能得到用户之前所在的应用（与 Windows 的 WinEvent 方案对应，事件驱动）。
//!
//! 没有 EWMH 窗口管理器时（twm 一类、或裸 X 服务器），「前台」退回输入焦点所在的顶层窗口，激活改为
//! SetInputFocus + 置顶。

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use wisp_types::AppRef;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, EventMask, InputFocus, StackMode,
    Window,
};

use super::{X11, props};

/// 前台窗口的跟踪状态
#[derive(Default)]
pub(crate) struct FocusState {
    /// 最近一个外部应用的顶层窗口（0 表示还没有）
    last_external: AtomicU32,
    /// 上一次看到的 `_NET_ACTIVE_WINDOW`（0 = 没有 / 尚未读取）
    active: AtomicU32,
}

/// 根窗口属性变化（hub 线程）。部分窗口管理器（Openbox、xfwm4 等）在焦点不变时也会重写 `_NET_ACTIVE_WINDOW`，
/// 只有值真正变化才算切换应用（否则浮标会刚出现就被关掉）
pub(crate) fn on_root_property(x: &X11, atom: Atom) {
    if atom != x.atoms._NET_ACTIVE_WINDOW {
        return;
    }
    let w = props::active_window(x).unwrap_or(0);
    if x.focus.active.swap(w, Ordering::AcqRel) == w {
        return;
    }
    if w != 0 && is_external(x, w) {
        x.focus.last_external.store(w, Ordering::Release);
    }
    #[cfg(feature = "mouse-hook")]
    super::input::on_app_switched(x);
}

/// 连接建立、订阅根窗口属性之后记录一次当前前台窗口，之后的变化由 PropertyNotify 驱动
pub(crate) fn prime(x: &X11) {
    let w = props::active_window(x).unwrap_or(0);
    x.focus.active.store(w, Ordering::Release);
    if w != 0 && is_external(x, w) {
        x.focus.last_external.store(w, Ordering::Release);
    }
}

/// 窗口是否属于「外部应用」：不是 Wisp 自己，也不是桌面 / 面板 / Dock
fn is_external(x: &X11, w: Window) -> bool {
    if props::pid(x, w) == Some(x.own_pid) {
        return false;
    }
    let types = props::window_types(x, w);
    !types
        .iter()
        .any(|t| *t == x.atoms._NET_WM_WINDOW_TYPE_DESKTOP || *t == x.atoms._NET_WM_WINDOW_TYPE_DOCK)
}

/// 是否有 EWMH 窗口管理器（根窗口上有 `_NET_SUPPORTING_WM_CHECK`）
fn has_ewmh_wm(x: &X11) -> bool {
    props::u32_first(x, x.root, x.atoms._NET_SUPPORTING_WM_CHECK, AtomEnum::WINDOW).is_some_and(|w| w != 0)
}

/// 当前前台的顶层窗口：`_NET_ACTIVE_WINDOW`；没有（没有 EWMH 窗口管理器）时退回输入焦点所在的顶层窗口
pub(crate) fn active_toplevel(x: &X11) -> Option<Window> {
    props::active_window(x).or_else(|| {
        let focus = x.conn.get_input_focus().ok()?.reply().ok()?.focus;
        // 0 = None，1 = PointerRoot
        (focus > 1).then(|| props::find_app_window(x, focus)).flatten()
    })
}

/// 当前前台应用；前台是 Wisp / 桌面时沿用最近一个外部应用
pub(crate) fn frontmost(x: &X11) -> Option<AppRef> {
    let active = active_toplevel(x);
    if let Some(w) = active
        && is_external(x, w)
    {
        x.focus.last_external.store(w, Ordering::Release);
        if let Some(app) = app_ref(x, w) {
            return Some(app);
        }
    }
    let last = x.focus.last_external.load(Ordering::Acquire);
    (last != 0).then(|| app_ref(x, last)).flatten()
}

/// 由顶层窗口构造 AppRef（窗口已销毁时返回 None）
pub(crate) fn app_ref(x: &X11, w: Window) -> Option<AppRef> {
    let class = props::wm_class(x, w);
    let pid = props::pid(x, w);
    if class.is_none() && pid.is_none() {
        return None;
    }
    Some(build_ref(class, pid, Some(w)))
}

/// 由 WM_CLASS / pid 构造 AppRef：id = class 小写，alt_id = instance，name = class（首字母大写）
pub(crate) fn build_ref(class: Option<(String, String)>, pid: Option<u32>, window: Option<Window>) -> AppRef {
    let exe = pid.and_then(exe_path);
    let (instance, class) = class.unwrap_or_default();
    let exe_name = exe
        .as_deref()
        .and_then(|p| p.rsplit('/').next())
        .map(str::to_owned)
        .unwrap_or_default();
    let id_src = if !class.is_empty() {
        class.as_str()
    } else if !instance.is_empty() {
        instance.as_str()
    } else {
        exe_name.as_str()
    };
    AppRef {
        id: id_src.to_ascii_lowercase(),
        alt_id: (!instance.is_empty()).then(|| instance.clone()),
        name: pretty_name(id_src),
        pid,
        path: exe,
        window: window.map(u64::from),
    }
}

/// 首字母大写（`firefox` → `Firefox`；`org.gnome.Nautilus` 取最后一段 → `Nautilus`）
fn pretty_name(raw: &str) -> String {
    let base = if raw.contains('.') && !raw.contains(' ') {
        raw.rsplit('.').next().unwrap_or(raw)
    } else {
        raw
    };
    let mut chars = base.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// `/proc/<pid>/exe`
pub(crate) fn exe_path(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|p| p.to_string_lossy().trim_end_matches(" (deleted)").to_owned())
}

/// 剪贴板 owner → 来源应用：owner 窗口（或其祖先）的 `_NET_WM_PID`；找到 pid 但没有 WM_CLASS 时，
/// 在客户端列表里找同 pid 的顶层窗口；都不行时用当前前台窗口
pub(crate) fn app_for_owner(x: &X11, owner: Window) -> Option<AppRef> {
    if let Some(w) = props::find_app_window(x, owner) {
        let pid = props::pid(x, w);
        if let Some(class) = props::wm_class(x, w) {
            return Some(build_ref(Some(class), pid, Some(w)));
        }
        if let Some(pid) = pid {
            if pid == x.own_pid {
                return None;
            }
            if let Some(active) = props::active_window(x)
                && props::pid(x, active) == Some(pid)
            {
                return app_ref(x, active);
            }
            for c in props::client_list(x) {
                if props::pid(x, c) == Some(pid) {
                    return app_ref(x, c);
                }
            }
            return Some(build_ref(None, Some(pid), None));
        }
    }
    props::active_window(x)
        .filter(|&w| is_external(x, w))
        .and_then(|w| app_ref(x, w))
}

/// 找到应用对应的顶层窗口：优先记录的 XID（仍存在时），否则按 pid、再按 WM_CLASS 在客户端列表中查找
pub(crate) fn resolve_window(x: &X11, app: &AppRef) -> Option<Window> {
    if let Some(w) = app.window.and_then(|w| u32::try_from(w).ok())
        && props::exists(x, w)
    {
        return Some(w);
    }
    let clients = props::client_list(x);
    if let Some(pid) = app.pid
        && let Some(&w) = clients.iter().find(|&&c| props::pid(x, c) == Some(pid))
    {
        return Some(w);
    }
    clients
        .into_iter()
        .find(|&c| props::wm_class(x, c).is_some_and(|(_, class)| class.eq_ignore_ascii_case(&app.id)))
}

/// 请求激活窗口：有 EWMH 窗口管理器时发 `_NET_ACTIVE_WINDOW`（来源 = pager），否则自己置顶并设输入焦点
pub(crate) fn request_activate(x: &X11, w: Window) -> Result<(), String> {
    let time = x.server_time(Duration::from_millis(20)).unwrap_or(0);
    if !has_ewmh_wm(x) {
        x.conn
            .configure_window(w, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))
            .map_err(|e| e.to_string())?;
        x.conn
            .set_input_focus(InputFocus::PARENT, w, time)
            .map_err(|e| e.to_string())?;
        return x.conn.flush().map_err(|e| e.to_string());
    }
    let current = props::active_window(x).unwrap_or(0);
    let ev = ClientMessageEvent::new(32, w, x.atoms._NET_ACTIVE_WINDOW, [2, time, current, 0, 0]);
    x.conn
        .send_event(
            false,
            x.root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            ev,
        )
        .map_err(|e| e.to_string())?;
    x.conn.flush().map_err(|e| e.to_string())
}

/// 等待窗口成为前台（每 5 ms 查询一次，最多 `timeout`）；只在粘贴流程中短暂使用
pub(crate) fn wait_active(x: &X11, w: Window, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if active_toplevel(x) == Some(w) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// 正在运行的图形应用（客户端列表，按 id 去重）。dock / 面板（`_NET_WM_WINDOW_TYPE_DOCK`：xfce4-panel、plank、
/// tint2…）与桌面（`_NET_WM_WINDOW_TYPE_DESKTOP`：xfdesktop、nautilus-desktop…）不是用户的应用，一些窗口管理器
/// 把它们也放进客户端列表——跳过（排除名单选择器只列真正的应用；邻居探测的目标都不是这两类窗口）
pub(crate) fn running_apps(x: &X11) -> Vec<AppRef> {
    let mut out: Vec<AppRef> = Vec::new();
    let shell = [x.atoms._NET_WM_WINDOW_TYPE_DOCK, x.atoms._NET_WM_WINDOW_TYPE_DESKTOP];
    for w in props::client_list(x) {
        if props::window_types(x, w).iter().any(|t| shell.contains(t)) {
            continue;
        }
        if let Some(app) = app_ref(x, w)
            && app.pid != Some(x.own_pid)
            && !app.id.is_empty()
            && !out.iter().any(|a| a.id == app.id)
        {
            out.push(app);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretty_names() {
        assert_eq!(pretty_name("firefox"), "Firefox");
        assert_eq!(pretty_name("org.gnome.Nautilus"), "Nautilus");
        assert_eq!(pretty_name("Gnome-terminal"), "Gnome-terminal");
        assert_eq!(pretty_name(""), "");
    }

    #[test]
    fn app_ref_from_class() {
        let r = build_ref(Some(("Navigator".into(), "firefox".into())), None, Some(42));
        assert_eq!(r.id, "firefox");
        assert_eq!(r.alt_id.as_deref(), Some("Navigator"));
        assert_eq!(r.name, "Firefox");
        assert_eq!(r.window, Some(42));
    }
}
