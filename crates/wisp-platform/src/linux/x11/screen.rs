//! 显示器（RandR ≥ 1.3 的已启用输出，MonitorId = 输出名）、光标位置与 XSETTINGS。
//!
//! 显示器的枚举方式与 wisp-capture 的 X11 后端**完全相同**（GetScreenResourcesCurrent → 已连接且有 CRTC 的输出 →
//! GetCrtcInfo；没有 RandR 时整个屏幕是一台 id 为 `default` 的显示器），因此 Overlay、锚点、浮标按 MonitorId
//! 查找显示器时两边一致（X11 的 MonitorId 是 RandR 输出名）。
//!
//! 显示器列表与 XSETTINGS 都缓存起来，分别在 RandR 通知、根窗口工作区属性变化、XSETTINGS 属性变化时失效——
//! 读取是 O(1)，变化由事件驱动。

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use x11rb::NONE;
use x11rb::protocol::randr;
use x11rb::protocol::xfixes;
use x11rb::protocol::xproto::{Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, EventMask, Window};

use super::{X11, props};
use crate::linux::edid;
use crate::linux::geometry::{self, PhysMonitor, PxRect};
use crate::linux::xsettings::{self, XSettings};

/// 外观监听回调
pub(crate) type SettingsListener = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct XSettingsState {
    selection: Atom,
    window: Window,
    values: Option<XSettings>,
}

/// 屏幕相关缓存
#[derive(Default)]
pub(crate) struct ScreenState {
    monitors: Mutex<Option<Arc<Vec<PhysMonitor>>>>,
    edid_names: Mutex<HashMap<u32, Option<String>>>,
    xsettings: Mutex<XSettingsState>,
    /// 当前桌面的 `_GTK_WORKAREAS_D<n>` 原子（0 = 尚未查询或不存在）
    gtk_workarea_atom: std::sync::atomic::AtomicU32,
}

impl ScreenState {
    /// 显示器配置或工作区变化
    pub(crate) fn invalidate_monitors(&self) {
        self.monitors.lock().take();
    }

    /// 是否是当前 XSETTINGS 管理器的窗口（本连接常驻订阅了它的属性变化与销毁）
    pub(crate) fn is_xsettings_window(&self, w: Window) -> bool {
        let st = self.xsettings.lock();
        st.window != NONE && st.window == w
    }

    /// 当前 XSETTINGS（没有管理器时为 None）
    pub(crate) fn xsettings(&self) -> Option<XSettings> {
        self.xsettings.lock().values.clone()
    }
}

/// 显示器列表（缓存）
pub(crate) fn monitors(x: &X11) -> Arc<Vec<PhysMonitor>> {
    if let Some(m) = x.screen_state.monitors.lock().as_ref() {
        return Arc::clone(m);
    }
    let list = Arc::new(query_monitors(x));
    *x.screen_state.monitors.lock() = Some(Arc::clone(&list));
    list
}

fn query_monitors(x: &X11) -> Vec<PhysMonitor> {
    let scale = geometry::gdk_scale(
        std::env::var("GDK_SCALE").ok().as_deref(),
        x.screen_state.xsettings().and_then(|s| s.window_scaling_factor()),
    );
    let (gtk_areas, net_area) = work_areas(x);
    let mut out = Vec::new();
    if x.ext.randr {
        let primary = randr::get_output_primary(&x.conn, x.root)
            .ok()
            .and_then(|c| c.reply().ok())
            .map_or(NONE, |r| r.output);
        for o in randr_outputs(x) {
            let name = edid_name(x, o.output).unwrap_or_else(|| o.name.clone());
            out.push(PhysMonitor {
                id: o.name,
                name,
                rect: o.rect,
                work_area: geometry::pick_work_area(o.rect, &gtk_areas, net_area),
                primary: o.output == primary,
                scale,
            });
        }
    }
    if out.is_empty()
        && let Some(s) = x.screen_info()
    {
        // 没有 RandR（或没有启用的输出）：整个屏幕当作一台显示器，id 与 wisp-capture 的退回值相同
        let rect = PxRect::new(0, 0, i32::from(s.width_in_pixels), i32::from(s.height_in_pixels));
        out.push(PhysMonitor {
            id: FALLBACK_MONITOR_ID.to_owned(),
            name: format!("Screen {}", x.screen),
            rect,
            work_area: geometry::pick_work_area(rect, &gtk_areas, net_area),
            primary: true,
            scale,
        });
    }
    if !out.iter().any(|m| m.primary)
        && let Some(first) = out.first_mut()
    {
        first.primary = true;
    }
    out
}

/// 没有 RandR 时整屏显示器的 id（与 wisp-capture `backend_x11::list_outputs` 的退回值一致）
const FALLBACK_MONITOR_ID: &str = "default";

/// 一个已启用的 RandR 输出
struct RandrOutput {
    output: u32,
    /// 输出名（`DP-1`、`HDMI-A-0`、`eDP-1`……）= MonitorId
    name: String,
    rect: PxRect,
}

/// 已连接、有 CRTC 的 RandR 输出（服务器给出的顺序），查询方式与 wisp-capture 相同；请求都流水线发出
fn randr_outputs(x: &X11) -> Vec<RandrOutput> {
    let Some(res) = randr::get_screen_resources_current(&x.conn, x.root)
        .ok()
        .and_then(|c| c.reply().ok())
    else {
        return Vec::new();
    };
    let ts = res.config_timestamp;
    let cookies: Vec<_> = res
        .outputs
        .iter()
        .map(|&o| (o, randr::get_output_info(&x.conn, o, ts).ok()))
        .collect();
    let infos: Vec<(u32, randr::GetOutputInfoReply)> = cookies
        .into_iter()
        .filter_map(|(o, c)| Some((o, c?.reply().ok()?)))
        .filter(|(_, i)| i.crtc != 0 && i.connection == randr::Connection::CONNECTED)
        .collect();
    let crtcs: Vec<_> = infos
        .iter()
        .map(|(_, i)| randr::get_crtc_info(&x.conn, i.crtc, ts).ok())
        .collect();
    infos
        .into_iter()
        .zip(crtcs)
        .filter_map(|((output, info), cookie)| {
            let crtc = cookie?.reply().ok()?;
            (crtc.width > 0 && crtc.height > 0).then(|| RandrOutput {
                output,
                name: String::from_utf8_lossy(&info.name).into_owned(),
                rect: PxRect::new(
                    i32::from(crtc.x),
                    i32::from(crtc.y),
                    i32::from(crtc.width),
                    i32::from(crtc.height),
                ),
            })
        })
        .collect()
}

fn edid_name(x: &X11, output: u32) -> Option<String> {
    if let Some(cached) = x.screen_state.edid_names.lock().get(&output) {
        return cached.clone();
    }
    let name = randr::get_output_property(&x.conn, output, x.atoms.EDID, AtomEnum::ANY, 0, 64, false, false)
        .ok()
        .and_then(|c| c.reply().ok())
        .and_then(|r| edid::monitor_name(&r.data));
    x.screen_state.edid_names.lock().insert(output, name.clone());
    name
}

/// (`_GTK_WORKAREAS_D<当前桌面>` 的全部矩形, `_NET_WORKAREA` 中当前桌面的矩形)
fn work_areas(x: &X11) -> (Vec<PxRect>, Option<PxRect>) {
    let desktop = props::u32_first(x, x.root, x.atoms._NET_CURRENT_DESKTOP, AtomEnum::CARDINAL).unwrap_or(0);
    let to_rects = |v: &[u32]| -> Vec<PxRect> {
        v.chunks_exact(4)
            .map(|c| PxRect::new(c[0] as i32, c[1] as i32, c[2] as i32, c[3] as i32))
            .filter(|r| r.w > 0 && r.h > 0)
            .collect()
    };
    let gtk_atom = x.existing_atom(&format!("_GTK_WORKAREAS_D{desktop}"));
    x.screen_state
        .gtk_workarea_atom
        .store(gtk_atom.unwrap_or(0), std::sync::atomic::Ordering::Relaxed);
    let gtk = gtk_atom
        .and_then(|a| props::u32s(x, x.root, a, AtomEnum::CARDINAL))
        .map(|v| to_rects(&v))
        .unwrap_or_default();
    let net = props::u32s(x, x.root, x.atoms._NET_WORKAREA, AtomEnum::CARDINAL).and_then(|v| {
        let rects = to_rects(&v);
        rects.get(desktop as usize).or_else(|| rects.first()).copied()
    });
    (gtk, net)
}

/// 光标的全局物理坐标与按键 / 修饰键掩码
pub(crate) fn pointer(x: &X11) -> Option<(i32, i32, u16)> {
    let r = x.conn.query_pointer(x.root).ok()?.reply().ok()?;
    Some((i32::from(r.root_x), i32::from(r.root_y), u16::from(r.mask)))
}

/// 根窗口属性变化（hub 线程）：工作区 / 当前桌面变化时刷新显示器缓存
pub(crate) fn on_root_property(x: &X11, atom: Atom) {
    let a = &x.atoms;
    if atom == a._NET_WORKAREA || atom == a._NET_CURRENT_DESKTOP {
        x.screen_state.invalidate_monitors();
        return;
    }
    // _GTK_WORKAREAS_D<n> 的原子是动态的：与上次查询到的当前桌面原子比较
    let gtk = x
        .screen_state
        .gtk_workarea_atom
        .load(std::sync::atomic::Ordering::Relaxed);
    if gtk != 0 && atom == gtk {
        x.screen_state.invalidate_monitors();
    }
}

/// 找到 XSETTINGS 管理器并订阅它的属性变化；管理器重启（owner 变化）时由 XFixes 通知重新调用
pub(crate) fn attach_xsettings(x: &X11) {
    let Some(sel) = x.intern(&format!("_XSETTINGS_S{}", x.screen)) else {
        return;
    };
    if x.ext.xfixes {
        let _ = xfixes::select_selection_input(&x.conn, x.win, sel, xfixes::SelectionEventMask::SET_SELECTION_OWNER);
    }
    let owner = x
        .conn
        .get_selection_owner(sel)
        .ok()
        .and_then(|c| c.reply().ok())
        .map_or(NONE, |r| r.owner);
    if owner != NONE {
        let _ = x.conn.change_window_attributes(
            owner,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
        );
    }
    let values = (owner != NONE)
        .then(|| {
            props::get(
                x,
                owner,
                x.atoms._XSETTINGS_SETTINGS,
                x.atoms._XSETTINGS_SETTINGS,
                256 * 1024,
            )
        })
        .flatten()
        .and_then(|r| xsettings::parse(&r.value));
    let mut st = x.screen_state.xsettings.lock();
    st.selection = sel;
    st.window = owner;
    st.values = values;
}

fn notify_settings(x: &X11) {
    // 缩放可能变化
    x.screen_state.invalidate_monitors();
    let listeners: Vec<SettingsListener> = x
        .reg
        .settings_listeners
        .lock()
        .iter()
        .map(|(_, l)| Arc::clone(l))
        .collect();
    for l in listeners {
        x.reg.post("XSETTINGS 监听", move || l());
    }
}

/// 外部窗口属性变化（hub 线程）
pub(crate) fn on_property(x: &X11, window: Window, atom: Atom) {
    let is_manager = {
        let st = x.screen_state.xsettings.lock();
        st.window == window && st.window != NONE
    };
    if is_manager && atom == x.atoms._XSETTINGS_SETTINGS {
        let values = props::get(x, window, atom, atom, 256 * 1024).and_then(|r| xsettings::parse(&r.value));
        x.screen_state.xsettings.lock().values = values;
        notify_settings(x);
    }
}

/// 其他选区的 owner 变化（hub 线程）：XSETTINGS 管理器重启
pub(crate) fn on_selection_owner(x: &X11, selection: Atom) {
    let ours = x.screen_state.xsettings.lock().selection == selection;
    if ours {
        attach_xsettings(x);
        notify_settings(x);
    }
}

/// 窗口销毁（hub 线程）：XSETTINGS 管理器退出
pub(crate) fn on_destroy(x: &X11, window: Window) {
    let mut st = x.screen_state.xsettings.lock();
    if st.window == window && window != NONE {
        st.window = NONE;
        st.values = None;
        drop(st);
        notify_settings(x);
    }
}
