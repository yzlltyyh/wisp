//! 浮标：override-redirect 原生小窗，没有 WebView（内存 < 1 MB）。
//!
//! - 有合成器时用 32 位 ARGB 视觉，逐帧软件渲染（`bobber_art`）：出现 scale 0.6→1 + 淡入 140 ms，消失淡出
//!   140 ms，悬停高亮 80 ms，按下缩小；减少动效时只做 ≤100 ms 的淡入淡出；
//! - 没有合成器时退化为 SHAPE 裁成圆形的不透明窗口，出现 / 消失不做动画；
//! - 窗口不接受焦点（override-redirect，从不 SetInputFocus），点击它不会改变源应用的焦点与选区；输入区域用 SHAPE
//!   裁成圆盘，阴影边距可以点穿，「点在浮标外」的命中测试也按圆盘判断；
//! - 点击回调在独立的短命线程上调用（不占用 X11 事件线程，回调里可以直接同步取词）；
//! - 只有可见期间存在一个线程：动画时 60 Hz、静止时 30 Hz 检查「4 s 无交互 / 离开中心 80 pt」，隐藏后线程退出
//!   （只在浮标可见期间轮询光标，最长 4 s，空闲时没有定时器）。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use wisp_types::{MonitorId, Point, Rect};
use x11rb::NONE;
use x11rb::connection::Connection;
use x11rb::protocol::shape;
use x11rb::protocol::xproto::{
    AtomEnum, ColormapAlloc, ConfigureWindowAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask,
    ImageFormat, ImageOrder, PropMode, Rectangle, StackMode, VisualClass, Window, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

use super::{X11, call_guarded, input, screen};
use crate::linux::bobber_art::{self, BobberStyle};
use crate::linux::geometry::{self, PhysMonitor, PxRect};
use crate::types::{DismissReason, InputSignal};

// 时长、尺寸与消失条件统一取自 `common::gesture`（三端共用）
use crate::common::gesture::{
    self, BOBBER_APPEAR, BOBBER_APPEAR_SCALE, BOBBER_DISAPPEAR, BOBBER_DISTANCE_POLL, BOBBER_HOVER,
    BOBBER_IDLE_TIMEOUT, BOBBER_REDUCED_FADE,
};
/// 动画期间的帧间隔（约 60 Hz）
const FRAME: Duration = Duration::from_millis(16);
/// 发出消失信号后，若上层迟迟不隐藏，最多再等这么久就自行淡出（防止残留计时器）
const DISMISS_GRACE: Duration = Duration::from_secs(1);

type ClickCb = Arc<dyn Fn() + Send + Sync>;

/// 没有合成器时窗口的底色（24 位 TrueColor 像素，接近圆盘渐变的中间色）
const DISC_PIXEL: u32 = 0x004A_8CF5;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Appearing(Instant),
    Shown,
    Disappearing(Instant, f64),
    Hidden,
}

struct Win {
    id: Window,
    gc: u32,
    colormap: u32,
    depth: u8,
    argb: bool,
    scale: f64,
    size: u32,
}

struct Bobber {
    win: Win,
    generation: u64,
    phase: Phase,
    dark: bool,
    reduced: bool,
    hovered: bool,
    hover_from: f64,
    hover_start: Instant,
    pressed: bool,
    on_click: Option<ClickCb>,
    idle: Duration,
    dismissed_at: Option<Instant>,
    last_drawn: Option<(u64, u64, u64, u64)>,
    /// 圆形图标的位置（显示器内逻辑坐标），离开判定用
    circle: Rect,
    monitor: PhysMonitor,
}

/// 浮标状态
#[derive(Default)]
pub(crate) struct BobberState {
    inner: Mutex<Option<Bobber>>,
    /// 当前位置（显示器 id, 全局物理矩形）；单独加锁，供 hub 线程的命中测试使用
    rect: Mutex<Option<(MonitorId, PxRect, PhysMonitor)>>,
    visible: AtomicBool,
    generation: AtomicU64,
}

/// 是否可见（不含淡出中）
pub(crate) fn is_visible(x: &X11) -> bool {
    x.bobber.visible.load(Ordering::Acquire)
}

/// 全局物理坐标是否落在浮标的圆盘上（与输入区域一致：点在阴影边距上会落到下面的应用，算作「浮标外」）
pub(crate) fn contains(x: &X11, px: i32, py: i32) -> bool {
    x.bobber
        .rect
        .lock()
        .as_ref()
        .is_some_and(|(_, r, m)| r.contains(px, py) && bobber_art::disc_contains(m.scale, px - r.x, py - r.y))
}

/// 浮标圆形的矩形（显示器内逻辑坐标，22 pt；`BobberWindow::rect` 的三端约定）
pub(crate) fn rect(x: &X11) -> Option<(MonitorId, Rect)> {
    if !is_visible(x) {
        return None;
    }
    let guard = x.bobber.rect.lock();
    let (id, r, m) = guard.as_ref()?;
    let s = m.scale;
    // 三端统一返回 22 pt 的圆形（不含只画阴影的余量）：窗口是以圆形为中心的 34 pt 方块
    let pad = (gesture::BOBBER_WINDOW - gesture::BOBBER_DIAMETER) / 2.0;
    Some((
        id.clone(),
        Rect::new(
            f64::from(r.x - m.rect.x) / s + pad,
            f64::from(r.y - m.rect.y) / s + pad,
            gesture::BOBBER_DIAMETER,
            gesture::BOBBER_DIAMETER,
        ),
    ))
}

fn compositor_running(x: &X11) -> bool {
    x.intern(&format!("_NET_WM_CM_S{}", x.screen))
        .and_then(|a| x.conn.get_selection_owner(a).ok()?.reply().ok())
        .is_some_and(|r| r.owner != NONE)
}

fn argb_visual(x: &X11) -> Option<u32> {
    let scr = x.screen_info()?;
    scr.allowed_depths
        .iter()
        .find(|d| d.depth == 32)?
        .visuals
        .iter()
        .find(|v| v.class == VisualClass::TRUE_COLOR)
        .map(|v| v.visual_id)
}

fn create_window(x: &X11, scale: f64) -> Option<Win> {
    let size = bobber_art::window_px(scale);
    let visual = if compositor_running(x) { argb_visual(x) } else { None };
    let (depth, visual_id, argb) = match visual {
        Some(v) => (32u8, v, true),
        None => (x.root_depth, x.root_visual, false),
    };
    // 软件渲染输出的是每像素 32 位；极少见的 24 位紧凑格式不支持（不显示浮标，而不是画错）
    let bpp = x
        .conn
        .setup()
        .pixmap_formats
        .iter()
        .find(|f| f.depth == depth)
        .map(|f| f.bits_per_pixel);
    if bpp != Some(32) {
        log::warn!("浮标：深度 {depth} 的像素格式不是 32 位（{bpp:?}），不显示浮标");
        return None;
    }
    let id = x.conn.generate_id().ok()?;
    let mut colormap = 0;
    let mut aux = CreateWindowAux::new()
        .override_redirect(1)
        .background_pixel(0)
        .border_pixel(0)
        .event_mask(
            EventMask::EXPOSURE
                | EventMask::BUTTON_PRESS
                | EventMask::BUTTON_RELEASE
                | EventMask::ENTER_WINDOW
                | EventMask::LEAVE_WINDOW,
        );
    if !argb {
        // 不透明窗口在第一帧画上之前就是圆盘的底色，避免映射瞬间闪一下黑色
        aux = aux.background_pixel(DISC_PIXEL);
    }
    if argb {
        colormap = x.conn.generate_id().ok()?;
        x.conn
            .create_colormap(ColormapAlloc::NONE, colormap, x.root, visual_id)
            .ok()?;
        aux = aux.colormap(colormap);
    }
    let side = u16::try_from(size).ok()?;
    x.conn
        .create_window(
            depth,
            id,
            x.root,
            -1000,
            -1000,
            side,
            side,
            0,
            WindowClass::INPUT_OUTPUT,
            visual_id,
            &aux,
        )
        .ok()?
        .check()
        .ok()?;
    let gc = x.conn.generate_id().ok()?;
    x.conn.create_gc(gc, id, &CreateGCAux::new()).ok()?;
    let a = &x.atoms;
    let _ = x.conn.change_property32(
        PropMode::REPLACE,
        id,
        a._NET_WM_WINDOW_TYPE,
        AtomEnum::ATOM,
        &[a._NET_WM_WINDOW_TYPE_DND],
    );
    let _ = x
        .conn
        .change_property32(PropMode::REPLACE, id, a._NET_WM_PID, AtomEnum::CARDINAL, &[x.own_pid]);
    let _ = x.conn.change_property8(
        PropMode::REPLACE,
        id,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        b"wisp-bobber\0Wisp\0",
    );
    let _ = x
        .conn
        .change_property8(PropMode::REPLACE, id, AtomEnum::WM_NAME, AtomEnum::STRING, b"Wisp");
    if x.ext.shape {
        let spans: Vec<Rectangle> = bobber_art::disc_spans(scale)
            .into_iter()
            .map(|(sx, sy, w)| Rectangle {
                x: sx as i16,
                y: sy as i16,
                width: w,
                height: 1,
            })
            .collect();
        // 输入区域只有圆盘：阴影边距可以点穿
        let _ = shape::rectangles(
            &x.conn,
            shape::SO::SET,
            shape::SK::INPUT,
            x11rb::protocol::xproto::ClipOrdering::UNSORTED,
            id,
            0,
            0,
            &spans,
        );
        if !argb {
            // 没有合成器：外形也裁成圆形
            let _ = shape::rectangles(
                &x.conn,
                shape::SO::SET,
                shape::SK::BOUNDING,
                x11rb::protocol::xproto::ClipOrdering::UNSORTED,
                id,
                0,
                0,
                &spans,
            );
        }
    }
    Some(Win {
        id,
        gc,
        colormap,
        depth,
        argb,
        scale,
        size,
    })
}

fn destroy_window(x: &X11, w: &Win) {
    let _ = x.conn.free_gc(w.gc);
    let _ = x.conn.destroy_window(w.id);
    if w.colormap != 0 {
        let _ = x.conn.free_colormap(w.colormap);
    }
    let _ = x.conn.flush();
}

fn ease_out_cubic(t: f64) -> f64 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

/// 某一时刻的 (不透明度, 缩放, 悬停, 是否仍在动画)
fn frame_params(b: &Bobber, now: Instant) -> (f64, f64, f64, bool) {
    let hover_target = if b.hovered { 1.0 } else { 0.0 };
    let ht = (now.duration_since(b.hover_start).as_secs_f64() / BOBBER_HOVER.as_secs_f64()).clamp(0.0, 1.0);
    let hover = b.hover_from + (hover_target - b.hover_from) * ht;
    let hovering = ht < 1.0;
    let (opacity, zoom, animating) = match b.phase {
        Phase::Appearing(t0) => {
            let dur = if b.reduced { BOBBER_REDUCED_FADE } else { BOBBER_APPEAR };
            let t = now.duration_since(t0).as_secs_f64() / dur.as_secs_f64();
            let e = ease_out_cubic(t);
            let zoom = if b.reduced {
                1.0
            } else {
                BOBBER_APPEAR_SCALE + (1.0 - BOBBER_APPEAR_SCALE) * e
            };
            (e, zoom, t < 1.0)
        }
        Phase::Shown => (1.0, 1.0, false),
        Phase::Disappearing(t0, from) => {
            let dur = if b.reduced {
                BOBBER_REDUCED_FADE
            } else {
                BOBBER_DISAPPEAR
            };
            let t = now.duration_since(t0).as_secs_f64() / dur.as_secs_f64();
            (from * (1.0 - t.clamp(0.0, 1.0)), 1.0, t < 1.0)
        }
        Phase::Hidden => (0.0, 1.0, false),
    };
    (opacity, zoom, hover, animating || hovering)
}

fn draw(x: &X11, b: &mut Bobber, opacity: f64, zoom: f64, hover: f64) {
    let key = (
        opacity.to_bits(),
        zoom.to_bits(),
        hover.to_bits(),
        u64::from(b.pressed) | (u64::from(b.dark) << 1),
    );
    if b.last_drawn == Some(key) {
        return;
    }
    let style = BobberStyle {
        scale: b.win.scale,
        dark: b.dark,
        hover,
        pressed: if b.pressed { 1.0 } else { 0.0 },
        zoom,
        opacity,
    };
    let mut data = if b.win.argb {
        bobber_art::render(&style)
    } else {
        bobber_art::render_opaque(&style)
    };
    if x.conn.setup().image_byte_order == ImageOrder::MSB_FIRST {
        for px in data.chunks_exact_mut(4) {
            px.reverse();
        }
    }
    let side = b.win.size as u16;
    let _ = x.conn.put_image(
        ImageFormat::Z_PIXMAP,
        b.win.id,
        b.win.gc,
        side,
        side,
        0,
        0,
        0,
        b.win.depth,
        &data,
    );
    let _ = x.conn.flush();
    b.last_drawn = Some(key);
}

/// 显示浮标（`at` 为抬起点，显示器内逻辑坐标）
pub(crate) fn show(x: &Arc<X11>, at: (MonitorId, Point), dark: bool, reduced_motion: bool, on_click: ClickCb) {
    let monitors = screen::monitors(x);
    let monitor = monitors
        .iter()
        .find(|m| m.id == at.0)
        .or_else(|| {
            let (px, py, _) = screen::pointer(x)?;
            geometry::monitor_at(&monitors, px, py)
        })
        .cloned();
    let Some(monitor) = monitor else {
        log::debug!("浮标：找不到显示器 {}", at.0);
        return;
    };
    // 位置规则（右下 (8,12)、越界按方向翻转、夹回工作区）与其他平台共用 common::gesture
    let work_area = monitor.to_info().work_area;
    let circle = gesture::bobber_rect(at.1, work_area);
    let window = gesture::bobber_window_rect(circle);
    let (wx, wy) = monitor.to_global(Point {
        x: window.x,
        y: window.y,
    });
    let side = bobber_art::window_px(monitor.scale) as i32;
    let r = PxRect::new(wx, wy, side, side);
    let generation = x.bobber.generation.fetch_add(1, Ordering::AcqRel) + 1;
    {
        let mut guard = x.bobber.inner.lock();
        let compositor = compositor_running(x);
        let reuse = guard.as_ref().is_some_and(|b| {
            (b.win.scale - monitor.scale).abs() < f64::EPSILON && b.win.argb == (compositor && argb_visual(x).is_some())
        });
        if !reuse {
            if let Some(old) = guard.take() {
                destroy_window(x, &old.win);
            }
            let Some(win) = create_window(x, monitor.scale) else {
                log::warn!("浮标窗口创建失败");
                return;
            };
            *guard = Some(Bobber {
                win,
                generation,
                phase: Phase::Hidden,
                dark,
                reduced: reduced_motion,
                hovered: false,
                hover_from: 0.0,
                hover_start: Instant::now(),
                pressed: false,
                on_click: None,
                idle: Duration::ZERO,
                dismissed_at: None,
                last_drawn: None,
                circle,
                monitor: monitor.clone(),
            });
        }
        let Some(b) = guard.as_mut() else { return };
        let now = Instant::now();
        b.generation = generation;
        b.dark = dark;
        b.reduced = reduced_motion;
        b.hovered = false;
        b.hover_from = 0.0;
        b.hover_start = now;
        b.pressed = false;
        b.on_click = Some(on_click);
        b.idle = Duration::ZERO;
        b.dismissed_at = None;
        b.last_drawn = None;
        b.circle = circle;
        b.monitor = monitor.clone();
        b.phase = if b.win.argb {
            Phase::Appearing(now)
        } else {
            Phase::Shown
        };
        let (o, z, h, _) = frame_params(b, now);
        let _ = x.conn.configure_window(
            b.win.id,
            &ConfigureWindowAux::new().x(r.x).y(r.y).stack_mode(StackMode::ABOVE),
        );
        let _ = x.conn.map_window(b.win.id);
        draw(x, b, o, z, h);
    }
    *x.bobber.rect.lock() = Some((monitor.id.clone(), r, monitor));
    x.bobber.visible.store(true, Ordering::Release);
    input::refresh_mask(x);
    let x2 = Arc::clone(x);
    let _ = std::thread::Builder::new()
        .name("wisp-bobber".into())
        .spawn(move || run(&x2, generation));
}

/// 隐藏浮标
pub(crate) fn hide(x: &X11, animated: bool) {
    let was_visible = x.bobber.visible.swap(false, Ordering::AcqRel);
    {
        let mut guard = x.bobber.inner.lock();
        if let Some(b) = guard.as_mut() {
            b.on_click = None;
            let now = Instant::now();
            match b.phase {
                Phase::Hidden => {}
                _ if animated && b.win.argb => {
                    let (o, _, _, _) = frame_params(b, now);
                    if !matches!(b.phase, Phase::Disappearing(..)) {
                        b.phase = Phase::Disappearing(now, o);
                    }
                }
                _ => {
                    b.phase = Phase::Hidden;
                    let _ = x.conn.unmap_window(b.win.id);
                    let _ = x.conn.flush();
                }
            }
        }
    }
    x.bobber.rect.lock().take();
    if was_visible {
        input::refresh_mask(x);
    }
}

/// 关闭浮标功能时销毁窗口
pub(crate) fn destroy(x: &X11) {
    x.bobber.visible.store(false, Ordering::Release);
    x.bobber.generation.fetch_add(1, Ordering::AcqRel);
    x.bobber.rect.lock().take();
    if let Some(b) = x.bobber.inner.lock().take() {
        destroy_window(x, &b.win);
    }
}

fn run(x: &Arc<X11>, generation: u64) {
    let mut last = Instant::now();
    loop {
        let mut dismiss: Option<DismissReason> = None;
        let mut auto_hide = false;
        let sleep = {
            let mut guard = x.bobber.inner.lock();
            let Some(b) = guard.as_mut() else { return };
            if b.generation != generation {
                return;
            }
            let now = Instant::now();
            let dt = now.duration_since(last);
            last = now;
            let (o, z, h, animating) = frame_params(b, now);
            match b.phase {
                Phase::Appearing(_) if !animating => b.phase = Phase::Shown,
                Phase::Disappearing(..) if !animating => {
                    b.phase = Phase::Hidden;
                    let _ = x.conn.unmap_window(b.win.id);
                    let _ = x.conn.flush();
                    return;
                }
                Phase::Hidden => return,
                _ => {}
            }
            draw(x, b, o, z, h);
            let visible = x.bobber.visible.load(Ordering::Acquire);
            if visible {
                match b.dismissed_at {
                    None => {
                        if !b.hovered {
                            b.idle += dt;
                        }
                        if b.idle >= BOBBER_IDLE_TIMEOUT {
                            dismiss = Some(DismissReason::Timeout);
                        } else if let Some((px, py, _)) = screen::pointer(x)
                            && gesture::moved_away(b.circle, b.monitor.to_local(px, py))
                        {
                            dismiss = Some(DismissReason::MovedAway);
                        }
                        if dismiss.is_some() {
                            b.dismissed_at = Some(now);
                        }
                    }
                    Some(at) if now.duration_since(at) > DISMISS_GRACE => auto_hide = true,
                    Some(_) => {}
                }
            }
            if animating { FRAME } else { BOBBER_DISTANCE_POLL }
        };
        if let Some(reason) = dismiss {
            input::emit_bobber(x, InputSignal::BobberDismiss(reason));
        }
        if auto_hide {
            hide(x, true);
        }
        std::thread::sleep(sleep);
    }
}

/// 进入 / 离开浮标（hub 线程）
pub(crate) fn on_crossing(x: &X11, window: Window, entered: bool) {
    let mut guard = x.bobber.inner.lock();
    let Some(b) = guard.as_mut() else { return };
    if b.win.id != window || b.hovered == entered {
        return;
    }
    let now = Instant::now();
    let (_, _, h, _) = frame_params(b, now);
    b.hover_from = h;
    b.hover_start = now;
    b.hovered = entered;
    if !entered {
        b.pressed = false;
    }
    if !b.win.argb {
        let (o, z, h, _) = frame_params(b, now);
        draw(x, b, o, z, h);
    }
}

/// 重绘（hub 线程）
pub(crate) fn on_expose(x: &X11, window: Window) {
    let mut guard = x.bobber.inner.lock();
    let Some(b) = guard.as_mut() else { return };
    if b.win.id != window {
        return;
    }
    b.last_drawn = None;
    let (o, z, h, _) = frame_params(b, Instant::now());
    draw(x, b, o, z, h);
}

/// 浮标上的鼠标按键（hub 线程）；返回 true 表示事件属于浮标
pub(crate) fn on_button(x: &X11, window: Window, button: u8, press: bool) -> bool {
    let click = {
        let mut guard = x.bobber.inner.lock();
        let Some(b) = guard.as_mut() else { return false };
        if b.win.id != window {
            return false;
        }
        if button != 1 {
            return true;
        }
        let now = Instant::now();
        if press {
            b.pressed = true;
            let (o, z, h, _) = frame_params(b, now);
            draw(x, b, o, z, h);
            return true;
        }
        let was_pressed = std::mem::replace(&mut b.pressed, false);
        if was_pressed && b.hovered {
            b.on_click.take()
        } else {
            None
        }
    };
    if let Some(cb) = click {
        // 点击后浮标立即消失，再触发翻译。回调放到短命线程上，事件线程立刻回来分发 SelectionNotify 等事件
        hide(x, false);
        let spawned = std::thread::Builder::new()
            .name("wisp-bobber-click".into())
            .spawn(move || call_guarded("浮标点击", || cb()));
        if let Err(e) = spawned {
            log::warn!("浮标点击：创建线程失败（{e}）");
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn art_sizes_match_shared_constants() {
        assert!((bobber_art::WINDOW_PT - gesture::BOBBER_WINDOW).abs() < f64::EPSILON);
        assert!((bobber_art::ICON_PT - gesture::BOBBER_DIAMETER).abs() < f64::EPSILON);
    }

    #[test]
    fn appear_animation_curve() {
        assert!(ease_out_cubic(0.0).abs() < 1e-9);
        assert!((ease_out_cubic(1.0) - 1.0).abs() < 1e-9);
        assert!(ease_out_cubic(0.5) > 0.5, "ease-out 前半段更快");
    }
}
