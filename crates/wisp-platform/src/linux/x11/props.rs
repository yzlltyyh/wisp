//! 窗口属性读取的小工具（全部是同步往返，失败一律返回 None：外部窗口随时可能被销毁）。

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt as _, GetPropertyReply, Window};

use super::X11;

/// 单个属性最多读取的字节数（窗口图标等大属性单独处理）
const DEFAULT_MAX: u32 = 64 * 1024;

/// 读取属性（不删除）；`ty` 为 `AtomEnum::ANY` 时接受任何类型
pub(crate) fn get(x: &X11, win: Window, prop: Atom, ty: impl Into<Atom>, max_bytes: u32) -> Option<GetPropertyReply> {
    let reply = x
        .conn
        .get_property(false, win, prop, ty.into(), 0, max_bytes.div_ceil(4))
        .ok()?
        .reply()
        .ok()?;
    (reply.type_ != u32::from(AtomEnum::NONE)).then_some(reply)
}

/// 32 位属性数组
pub(crate) fn u32s(x: &X11, win: Window, prop: Atom, ty: impl Into<Atom>) -> Option<Vec<u32>> {
    let r = get(x, win, prop, ty, DEFAULT_MAX)?;
    Some(r.value32()?.collect())
}

/// 第一个 32 位值
pub(crate) fn u32_first(x: &X11, win: Window, prop: Atom, ty: impl Into<Atom>) -> Option<u32> {
    u32s(x, win, prop, ty)?.into_iter().next()
}

/// WM_CLASS → (instance, class)
pub(crate) fn wm_class(x: &X11, win: Window) -> Option<(String, String)> {
    let r = get(x, win, AtomEnum::WM_CLASS.into(), AtomEnum::STRING, 4096)?;
    let mut parts = r
        .value
        .split(|&b| b == 0)
        .map(|p| String::from_utf8_lossy(p).into_owned());
    let instance = parts.next().unwrap_or_default();
    let class = parts.next().unwrap_or_default();
    (!instance.is_empty() || !class.is_empty()).then_some((instance, class))
}

/// `_NET_WM_PID`
pub(crate) fn pid(x: &X11, win: Window) -> Option<u32> {
    u32_first(x, win, x.atoms._NET_WM_PID, AtomEnum::CARDINAL).filter(|&p| p > 0)
}

/// 根窗口的 `_NET_ACTIVE_WINDOW`（0 表示没有）
pub(crate) fn active_window(x: &X11) -> Option<Window> {
    u32_first(x, x.root, x.atoms._NET_ACTIVE_WINDOW, AtomEnum::WINDOW).filter(|&w| w != 0)
}

/// `_NET_WM_WINDOW_TYPE`
pub(crate) fn window_types(x: &X11, win: Window) -> Vec<Atom> {
    u32s(x, win, x.atoms._NET_WM_WINDOW_TYPE, AtomEnum::ATOM).unwrap_or_default()
}

/// `_NET_CLIENT_LIST`
pub(crate) fn client_list(x: &X11) -> Vec<Window> {
    u32s(x, x.root, x.atoms._NET_CLIENT_LIST, AtomEnum::WINDOW).unwrap_or_default()
}

/// 窗口是否仍然存在
pub(crate) fn exists(x: &X11, win: Window) -> bool {
    x.conn
        .get_window_attributes(win)
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some()
}

/// 沿父链向上找到带 `_NET_WM_PID` 或 `WM_CLASS` 的窗口（选区 owner 常是应用的内部辅助窗口）
pub(crate) fn find_app_window(x: &X11, mut win: Window) -> Option<Window> {
    for _ in 0..8 {
        if win == 0 || win == x.root {
            return None;
        }
        if pid(x, win).is_some() || wm_class(x, win).is_some() {
            return Some(win);
        }
        let tree = x.conn.query_tree(win).ok()?.reply().ok()?;
        win = tree.parent;
    }
    None
}

/// 刷新输出缓冲（请求本身不等回复时用）
pub(crate) fn flush(x: &X11) {
    if let Err(e) = x.conn.flush() {
        log::debug!("X11 flush 失败：{e}");
    }
}
