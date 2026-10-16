//! X11 全局快捷键：根窗口上的被动抓取（`XGrabKey`），按键事件由平台层唯一的 X11 事件
//! 线程（`wisp-x11`，阻塞在 `wait_for_event` 上）分发——空闲时零唤醒，取代 global-hotkey 0.8 每 50 ms 轮询一次的
//! X11 后端。纯逻辑（解析、匹配、按住状态、重新抓取的判定）在 `linux::hotkey_logic`。
//!
//! - 注册表与回调保存在跨连接的 [`super::Registry`] 里：断线重连后由新连接重新抓取全部组合（[`sync`]，`Resync::Reconnect`）；
//!   键盘映射变化（`MappingNotify`，例如 setxkbmap 换布局）后在回调线程上按新映射重新算 keycode，**只移动变了的**
//!   （[`on_mapping`]，`Resync::Mapping`）——Wisp 自己发送按键时的临时映射每次都触发两次 `MappingNotify`，已注册组合的
//!   keycode 不变，什么都不做（没有撤销—重抓的空窗，按住状态保留）；
//! - register / unregister / sync 由同一把注册锁（`ops`）串行执行，锁内与 X 服务器同步往返（它们都不在事件线程上）：
//!   注销不会被并发的重新抓取「按旧快照抓回去」，注册的抓取与登记之间也不会插进一次重连的接管；
//! - 注册在调用方线程上同步往返（`check()`，亚毫秒级），`BadAccess` = 已被别的程序占用；4 种锁定位组合任一失败都
//!   整体撤销，不留半个抓取；
//! - 回调不在事件线程上执行：投递到 `wisp-x11-cb` 线程（与剪贴板监听等外部回调同一个线程、按顺序）；
//! - 没有启用 XKB 的可检测自动重复（启用后服务器不再给本连接发核心 `MappingNotify`，会破坏键盘映射缓存的失效），
//!   按住时的 Release + Press 对由事件线程按「同一 keycode、同一时间戳」识别并丢弃（`hotkey_logic::is_autorepeat`）；
//!   抓取换了 keycode（或连接换了）时正按着的组合补发一次抬起（旧 keycode 的抬起不会再送到 Wisp）。
//! - 连续粘贴的粘贴键（`HookSpec::paste_keys`）与快捷键共用这张抓取表：队列开始时抓取、结束时撤销，按键路由给
//!   [`PasteRoute`] 而不是快捷键回调。被动抓取在按下之后独占键盘直到抬起，所以在**抬起**时才通知上层；重放
//!   （[`replay_paste`]）与 Wisp 自己发的粘贴键（[`send_outside_paste_grab`]）先撤销这一组的抓取、XTEST 发送、
//!   再抓回来——XTEST 的按键同样会触发被动抓取。整个过程与队列结束（[`end_paste`]）持同一把注册锁，路由清除之后
//!   不会再抓回来。

use std::sync::Arc;

use parking_lot::Mutex;
use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::ErrorKind;
use x11rb::protocol::xproto::{ConnectionExt as _, GrabMode, ModMask};

use super::{X11, keyboard};
use crate::linux::hotkey_logic::{self, GrabTable, LOCK_VARIANTS, Regrab, Spec};
use crate::traits::KeyGrabError;

/// 按键回调：(规范串, 是否按下)
pub(crate) type Handler = Arc<dyn Fn(&str, bool) + Send + Sync>;

/// 连续粘贴期间抓取的粘贴键：规范串与对应的组合，以及抬起时的回调
#[derive(Clone)]
pub(crate) struct PasteRoute {
    pub combos: Vec<(String, crate::types::KeyChord)>,
    pub sink: Arc<dyn Fn(crate::types::KeyChord) + Send + Sync>,
}

/// 跨连接保存的快捷键注册
#[derive(Default)]
pub(crate) struct Registered {
    /// 注册锁：串行化 register / unregister / sync（锁内与 X 服务器同步往返）。只在调用方线程、回调线程、连接线程上
    /// 持有，**事件线程从不拿它**（事件线程只碰 `table`，短临界区）
    ops: Mutex<()>,
    table: Mutex<GrabTable>,
    handler: Mutex<Option<Handler>>,
    /// 连续粘贴的粘贴键（队列进行中才有）
    paste: Mutex<Option<PasteRoute>>,
}

impl Registered {
    pub(crate) fn set_handler(&self, h: Handler) {
        *self.handler.lock() = Some(h);
    }

    /// 设置连续粘贴的路由（抓取本身由调用方注册；清除见 [`end_paste`]）
    #[cfg(feature = "mouse-hook")]
    pub(crate) fn set_paste(&self, route: Option<PasteRoute>) {
        *self.paste.lock() = route;
    }

    /// 没有连接时的注销：只从注册表里去掉（重连后不会再抓取）
    pub(crate) fn forget(&self, combo: &str) {
        if let Some((canon, _)) = hotkey_logic::spec(combo) {
            let _ops = self.ops.lock();
            self.table.lock().remove(&canon);
        }
    }
}

/// 重新抓取的原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resync {
    /// 新连接（首次连接、断线重连）：旧连接上的抓取随连接消失，全部重新抓取
    Reconnect,
    /// 键盘映射变化：只移动 keycode 变了的组合
    Mapping,
}

/// 抓取 4 种锁定位组合；任一失败时撤销已抓的并返回错误
fn grab(x: &X11, keycode: u8, mods: u16) -> Result<(), KeyGrabError> {
    for (i, lock) in LOCK_VARIANTS.iter().enumerate() {
        let result = x
            .conn
            .grab_key(
                false,
                x.root,
                ModMask::from(mods | lock),
                keycode,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )
            .map_err(|e| KeyGrabError::Rejected(e.to_string()))
            .and_then(|cookie| {
                cookie.check().map_err(|e| match e {
                    ReplyError::X11Error(err) if err.error_kind == ErrorKind::Access => KeyGrabError::Taken,
                    other => KeyGrabError::Rejected(other.to_string()),
                })
            });
        if let Err(e) = result {
            for lock in &LOCK_VARIANTS[..i] {
                let _ = x.conn.ungrab_key(keycode, x.root, ModMask::from(mods | lock));
            }
            let _ = x.conn.flush();
            return Err(e);
        }
    }
    Ok(())
}

/// 撤销抓取（不等回复：撤销不存在的抓取也不报错）
fn ungrab(x: &X11, keycode: u8, mods: u16) {
    for lock in LOCK_VARIANTS {
        let _ = x.conn.ungrab_key(keycode, x.root, ModMask::from(mods | lock));
    }
    let _ = x.conn.flush();
}

/// 当前键盘映射下产生这个 keysym 的 keycode
fn keycode_for(x: &X11, spec: Spec) -> Option<u8> {
    keyboard::keymap(x)?.find(spec.keysym).map(|(code, _)| code)
}

/// 注册一个组合（**阻塞**：与 X 服务器同步往返）
pub(crate) fn register(x: &X11, combo: &str) -> Result<(), KeyGrabError> {
    let (canon, spec) =
        hotkey_logic::spec(combo).ok_or_else(|| KeyGrabError::Rejected(format!("cannot parse hotkey {combo}")))?;
    let _ops = x.reg.hotkeys.ops.lock();
    register_locked(x, canon, spec)
}

/// 注册（调用方持注册锁）
fn register_locked(x: &X11, canon: String, spec: Spec) -> Result<(), KeyGrabError> {
    let reg = &x.reg.hotkeys;
    if reg.table.lock().contains(&canon) {
        return Ok(());
    }
    let keycode = keycode_for(x, spec)
        .ok_or_else(|| KeyGrabError::Rejected(format!("no key for {canon} in the current keyboard layout")))?;
    // 同一物理键 + 修饰位已经被 Wisp 自己的另一个组合占着（布局里两个 keysym 落在同一个键上）：X 不会报错，但两者
    // 无法区分——按被占用处理
    if reg.table.lock().owner_of(keycode, spec.mods).is_some() {
        return Err(KeyGrabError::Taken);
    }
    // 连接已经断开（还没重连）时抓取会失败并报告 Rejected；HotkeyService 稍后重试时用的是新连接
    grab(x, keycode, spec.mods)?;
    reg.table.lock().insert(canon, spec, Some(keycode));
    Ok(())
}

/// 注销一个组合（**阻塞**：与重新抓取串行）
pub(crate) fn unregister(x: &X11, combo: &str) {
    let Some((canon, spec)) = hotkey_logic::spec(combo) else {
        return;
    };
    let _ops = x.reg.hotkeys.ops.lock();
    unregister_locked(x, &canon, spec);
}

/// 注销（调用方持注册锁）
fn unregister_locked(x: &X11, canon: &str, spec: Spec) {
    let removed = x.reg.hotkeys.table.lock().remove(canon);
    if let Some(keycode) = removed.and_then(|e| e.keycode) {
        ungrab(x, keycode, spec.mods);
    }
}

/// 结束连续粘贴：在注册锁内清除路由并撤销这些组合的抓取（`x` 为 None = 没有连接，只从注册表里去掉）。
/// 与 [`send_outside_paste_grab`] 串行——发送期间临时撤销的抓取不会在路由清除之后被抓回来；否则这组键留在抓取表里
/// 却没有路由，交给快捷键回调后被丢弃，粘贴键在整个桌面失效
#[cfg(feature = "mouse-hook")]
pub(crate) fn end_paste(reg: &Registered, x: Option<&X11>, combos: &[String]) {
    let _ops = reg.ops.lock();
    *reg.paste.lock() = None;
    for combo in combos {
        let Some((canon, spec)) = hotkey_logic::spec(combo) else {
            continue;
        };
        match x {
            Some(x) => unregister_locked(x, &canon, spec),
            None => {
                reg.table.lock().remove(&canon);
            }
        }
    }
}

/// 按当前连接与键盘映射重新抓取已注册的组合（`why` 见 [`Resync`]）。**阻塞**，不在事件线程上调用。
/// 连接已经断开时什么都不做：新连接建立时会以 `Reconnect` 接管（它同样要拿注册锁，因此总在这次之后或之前完整执行）
pub(crate) fn sync(x: &X11, why: Resync) {
    let reg = &x.reg.hotkeys;
    let _ops = reg.ops.lock();
    if !x.is_alive() {
        return;
    }
    let entries: Vec<_> = {
        let mut table = reg.table.lock();
        if why == Resync::Reconnect {
            for combo in table.forget_keycodes() {
                post(x, combo, false);
            }
        }
        table.entries().to_vec()
    };
    for e in entries {
        let wanted = keycode_for(x, e.spec);
        let Regrab::Move { old, new } = hotkey_logic::regrab(e.keycode, wanted) else {
            continue;
        };
        if let Some(old) = old {
            ungrab(x, old, e.spec.mods);
        }
        let grabbed = new.and_then(|code| {
            if let Some(other) = reg.table.lock().owner_of(code, e.spec.mods) {
                log::warn!("快捷键 {} 在新的键盘布局里与 {other} 落在同一个键上，暂不抓取", e.combo);
                return None;
            }
            match grab(x, code, e.spec.mods) {
                Ok(()) => Some(code),
                Err(err) => {
                    log::warn!("快捷键 {} 重新抓取失败：{err}", e.combo);
                    None
                }
            }
        });
        if new.is_none() {
            log::info!("快捷键 {} 在当前键盘布局里没有对应的键", e.combo);
        }
        match reg.table.lock().set_keycode(&e.combo, grabbed) {
            Some(true) => post(x, e.combo.clone(), false),
            Some(false) => {}
            // 注册锁之下不会发生（注销与这里串行）；兜底：已经不在表里的组合不能留着抓取，否则这个键在整个桌面被吞掉
            None => {
                if let Some(code) = grabbed {
                    ungrab(x, code, e.spec.mods);
                }
            }
        }
    }
}

/// 键盘映射变化（事件线程调用）：映射缓存已失效，重新抓取放到回调线程上做（要同步往返）
pub(crate) fn on_mapping(x: &Arc<X11>) {
    if x.reg.hotkeys.table.lock().entries().is_empty() {
        return;
    }
    let me = Arc::clone(x);
    x.reg.post("hotkey regrab", move || sync(&me, Resync::Mapping));
}

/// 按下（事件线程调用）：只查表、投递回调
pub(crate) fn on_press(x: &X11, keycode: u8, state: u16) {
    let Some(combo) = x.reg.hotkeys.table.lock().press(keycode, state) else {
        return;
    };
    post(x, combo, true);
}

/// 这个 keycode 上是否有 Wisp 的抓取（事件线程调用：不是 Wisp 抓的键的抬起不做自动重复判定）
pub(crate) fn is_grabbed(x: &X11, keycode: u8) -> bool {
    x.reg
        .hotkeys
        .table
        .lock()
        .entries()
        .iter()
        .any(|e| e.keycode == Some(keycode))
}

/// 抬起（事件线程调用；自动重复的抬起已由调用方过滤）
pub(crate) fn on_release(x: &X11, keycode: u8) {
    let released = x.reg.hotkeys.table.lock().release(keycode);
    for combo in released {
        post(x, combo, false);
    }
}

fn post(x: &X11, combo: String, pressed: bool) {
    // 连续粘贴的粘贴键：按下与抬起都已被抓取吞掉，抬起时（键盘不再被独占）才通知上层
    let paste = x.reg.hotkeys.paste.lock().as_ref().and_then(|route| {
        route
            .combos
            .iter()
            .find(|(c, _)| *c == combo)
            .map(|(_, chord)| (*chord, Arc::clone(&route.sink)))
    });
    if let Some((chord, sink)) = paste {
        if !pressed {
            x.reg.post("paste key", move || sink(chord));
        }
        return;
    }
    let Some(handler) = x.reg.hotkeys.handler.lock().clone() else {
        return;
    };
    x.reg.post("hotkey", move || handler(&combo, pressed));
}

/// 在连续粘贴的抓取之外发送一组按键（**阻塞**：与 X 服务器同步往返）。`combo` 正被连续粘贴抓取时：撤销这一组的
/// 抓取 → `send` → 抓回来——XTEST 的按键同样会触发被动抓取，否则重放会被再次拦下，Wisp 自己（面板、悬浮窗）发的
/// 粘贴键也会被当成用户按的、变成一次队列前进。全程持注册锁，与 [`end_paste`] 串行：只在路由仍在时抓回来。
/// 抓回失败时只记日志：之后这组键不再被拦下，队列由上层的超时结束
pub(crate) fn send_outside_paste_grab<T>(x: &X11, combo: &crate::common::combo::Combo, send: impl FnOnce() -> T) -> T {
    let reg = &x.reg.hotkeys;
    let _ops = reg.ops.lock();
    // 两把小锁分开拿（不嵌套）：事件线程只碰 table，post 只碰 paste
    let routed = |canon: &str| {
        reg.paste
            .lock()
            .as_ref()
            .is_some_and(|route| route.combos.iter().any(|(c, _)| c == canon))
    };
    let grabbed = hotkey_logic::spec(&combo.to_canonical())
        .filter(|(canon, _)| routed(canon))
        .filter(|(canon, _)| reg.table.lock().contains(canon));
    if let Some((canon, spec)) = &grabbed {
        unregister_locked(x, canon, *spec);
    }
    let out = send();
    if let Some((canon, spec)) = grabbed
        && let Err(e) = register_locked(x, canon.clone(), spec)
    {
        log::warn!("paste key {canon} could not be grabbed again: {e}");
    }
    out
}

/// 重放一次被连续粘贴拦下的粘贴键（**阻塞**）。用户仍按着的修饰键不模拟（见 `keyboard::replay_combo`）；队列已经
/// 结束（路由已清除）时直接发送、不再抓取
#[cfg(feature = "mouse-hook")]
pub(crate) fn replay_paste(x: &Arc<X11>, chord: crate::types::KeyChord) -> Result<(), String> {
    let combo = chord.to_combo();
    send_outside_paste_grab(x, &combo, || keyboard::replay_combo(x, &combo))
}
