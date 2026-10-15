//! 模拟按键。
//!
//! - 事件源用 `CGEventSource(Private)`，不继承用户此刻按着的修饰键；flags 只带组合键要求的修饰位（⌘C 只带 Command），
//!   因此按住 ⌥D 触发时目标收到的仍是 ⌘C；`kCGEventSourceUserData` 写入 Wisp 标记，自己的钩子据此放行；
//! - 字母键的键码按当前键盘布局用 `UCKeyTranslate` 反查（先查 ⌘ 修饰下的字符表，兼容「Dvorak – QWERTY ⌘」与
//!   俄文等非拉丁布局；当前布局没有该字母时改用 ASCII-capable 布局）。TIS 只能在主线程调用（macOS 14 起在其他线程
//!   调用会触发断言），所以字母表在主线程上**预先**建好：`create` 时异步建一次；输入源变化
//!   （`kTISNotifySelectedKeyboardInputSourceChanged`，主线程上回调）时只把缓存标脏，[`REBUILD_DEBOUNCE`] 之后没有再
//!   切换才在主线程上重建（一次重建要占主线程约 29 ms，若在通知回调里同步重建，正好撞上输入视图弹出时的进入 /
//!   长高动画——中文用户常在这时切换输入法）。发键时只读缓存，不等主线程；缓存标脏期间要发键时才同步重建一次；
//! - 缓存为空（启动后主线程还没来得及建表）时才同步等主线程一次（≤ 60 ms）；仍拿不到就报错，**绝不**按 ANSI 位置
//!   盲发字母键——Dvorak 下 ANSI 的 C / V 位置是 J / .，⌘. 在终端等应用里是中断，属于有副作用的误发键。
//!   非字母键（F 键、Insert 等）的键码与布局无关，直接查表。

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use objc2::MainThreadMarker;
use objc2_core_foundation::{CFData, CFRetained, CFString, CFType};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
};
use parking_lot::Mutex;

use super::Ctx;
use super::ffi;
use super::notify::DistributedObserver;
use crate::common::WISP_INPUT_TAG;
use crate::common::combo::{CARBON_CMD, Combo, Modifiers};

/// 缓存为空时等待主线程建表的上限
const LAYOUT_WAIT: Duration = Duration::from_millis(60);
/// 输入源变化后等多久再重建字母表（连续切换只建一次，也避开切换那一刻的界面动画）
const REBUILD_DEBOUNCE: Duration = Duration::from_millis(800);

/// `kCGEventFlagMaskCommand` | `NX_DEVICELCMDKEYMASK`（同时带设备位，兼容按左右区分修饰键的应用）
const FLAGS_COMMAND: u64 = 0x0010_0000 | 0x08;
/// `kCGEventFlagMaskShift` | `NX_DEVICELSHIFTKEYMASK`
const FLAGS_SHIFT: u64 = 0x0002_0000 | 0x02;
/// `kCGEventFlagMaskControl` | `NX_DEVICELCTLKEYMASK`
const FLAGS_CONTROL: u64 = 0x0004_0000 | 0x01;
/// `kCGEventFlagMaskAlternate` | `NX_DEVICELALTKEYMASK`
const FLAGS_ALTERNATE: u64 = 0x0008_0000 | 0x20;
/// 合成事件都带上 NonCoalesced（与真实键盘事件一致）
const FLAGS_NON_COALESCED: u64 = 0x100;

/// 字母 → 键码的缓存（主线程上预建，输入源变化后防抖重建）
#[derive(Default)]
pub(crate) struct LayoutCache {
    letters: Mutex<Option<Arc<HashMap<char, u16>>>>,
    /// 输入源变了、表还没重建
    stale: AtomicBool,
    /// 输入源变化的代数（防抖：只有最后一次变化负责重建）
    generation: AtomicU64,
    /// 输入源变化的观察者（`create` 时登记）
    observer: Mutex<Option<DistributedObserver>>,
}

impl LayoutCache {
    fn get(&self) -> Option<Arc<HashMap<char, u16>>> {
        self.letters.lock().clone()
    }

    fn set(&self, map: Option<HashMap<char, u16>>) {
        *self.letters.lock() = map.map(Arc::new);
        self.stale.store(false, Ordering::Release);
    }
}

/// 开始跟踪键盘布局（`create` 调用；不阻塞）：登记输入源变化的观察者，并在主线程上异步预建字母表
pub(crate) fn start_tracking(ctx: &Arc<Ctx>) {
    // SAFETY: 常量由 HIToolbox 导出，指向静态的 CFString
    let name = unsafe { cf_string_ref(ffi::kTISNotifySelectedKeyboardInputSourceChanged) };
    let weak: Weak<Ctx> = Arc::downgrade(ctx);
    let observer = name.and_then(|name| {
        DistributedObserver::new(
            ctx.main.clone(),
            &[name.to_string().as_str()],
            Arc::new(move || {
                if let Some(ctx) = weak.upgrade() {
                    schedule_rebuild(&ctx);
                }
            }),
        )
    });
    *ctx.layout.observer.lock() = observer;
    rebuild(ctx);
}

/// 输入源变了：标脏，[`REBUILD_DEBOUNCE`] 内没有再变化时重建（一次性的线程，只在切换输入源时出现，不是常驻计时器）
fn schedule_rebuild(ctx: &Arc<Ctx>) {
    ctx.layout.stale.store(true, Ordering::Release);
    let generation = ctx.layout.generation.fetch_add(1, Ordering::AcqRel) + 1;
    let weak: Weak<Ctx> = Arc::downgrade(ctx);
    let spawned = std::thread::Builder::new()
        .name("wisp-layout-debounce".to_owned())
        .spawn(move || {
            std::thread::sleep(REBUILD_DEBOUNCE);
            if let Some(ctx) = weak.upgrade()
                && ctx.layout.generation.load(Ordering::Acquire) == generation
                && ctx.layout.stale.load(Ordering::Acquire)
            {
                rebuild(&ctx);
            }
        });
    if spawned.is_err() {
        rebuild(ctx);
    }
}

/// 在主线程上重建字母表（已在主线程时就地完成，否则异步派发；重建完成之前沿用旧表）
fn rebuild(ctx: &Arc<Ctx>) {
    let weak: Weak<Ctx> = Arc::downgrade(ctx);
    ctx.main.run_async(move |mtm| {
        if let Some(ctx) = weak.upgrade() {
            let map = build_letter_map(mtm);
            if map.is_none() {
                log::warn!("keyboard: no layout data for the current input source");
            }
            ctx.layout.set(map);
        }
    });
}

/// 解析好、可以直接发送的一次组合键
pub(crate) struct KeyStroke {
    keycode: u16,
    flags: CGEventFlags,
}

/// 组合键主键在当前布局下的键码（字母只读预建的缓存）
fn keycode_for(ctx: &Ctx, combo: &Combo) -> Result<u16, String> {
    match combo.letter() {
        Some(letter) => letter_keycode(ctx, letter),
        None => combo
            .mac_keycode()
            .ok_or_else(|| format!("no keycode for {}", combo.code)),
    }
}

fn letter_keycode(ctx: &Ctx, letter: char) -> Result<u16, String> {
    // 输入源刚变、防抖重建还没做：现在就要发键，同步重建一次（拿不到时沿用旧表）
    if ctx.layout.stale.load(Ordering::Acquire)
        && let Some(built) = ctx.main.run_sync(LAYOUT_WAIT, build_letter_map).flatten()
    {
        ctx.layout.set(Some(built));
    }
    let map = match ctx.layout.get() {
        Some(map) => map,
        None => {
            // 主线程还没来得及预建：同步等它一次
            let built = ctx
                .main
                .run_sync(LAYOUT_WAIT, build_letter_map)
                .flatten()
                .ok_or_else(|| "keyboard layout table unavailable (main thread busy or no layout data)".to_owned())?;
            let map = Arc::new(built);
            *ctx.layout.letters.lock() = Some(map.clone());
            map
        }
    };
    map.get(&letter)
        .copied()
        .ok_or_else(|| format!("letter {letter:?} not found in the current or ASCII-capable layout"))
}

/// 解析组合键（键码按当前布局、flags 只带组合键要求的修饰位）。发键之前先解析，回退流程据此在布防之后立即发键
pub(crate) fn prepare(ctx: &Ctx, combo: &Combo) -> Result<KeyStroke, String> {
    Ok(KeyStroke {
        keycode: keycode_for(ctx, combo)?,
        flags: flags_of(&combo.modifiers),
    })
}

/// 把 HIToolbox 导出的 CFStringRef 常量转成引用
///
/// # Safety
/// `p` 必须是有效的 CFStringRef 或空指针
unsafe fn cf_string_ref(p: ffi::CFTypeRef) -> Option<&'static CFString> {
    // SAFETY: 由调用方保证；HIToolbox 的常量是进程生命周期内有效的静态对象
    unsafe { p.cast::<CFString>().as_ref() }
}

/// 主线程：用当前（或 ASCII-capable）布局反查 a–z 的键码
fn build_letter_map(_mtm: MainThreadMarker) -> Option<HashMap<char, u16>> {
    // SAFETY: 在主线程上调用 TIS；返回的输入源按 Copy 规则持有，下面用 CFRetained 接管释放
    let current = unsafe { owned(ffi::TISCopyCurrentKeyboardLayoutInputSource()) };
    let mut map = current.as_deref().map(letters_of_source).unwrap_or_default();
    if !(map.contains_key(&'c') && map.contains_key(&'v')) {
        // SAFETY: 同上
        let ascii = unsafe { owned(ffi::TISCopyCurrentASCIICapableKeyboardLayoutInputSource()) };
        if let Some(ascii) = ascii.as_deref() {
            for (c, kc) in letters_of_source(ascii) {
                map.entry(c).or_insert(kc);
            }
        }
    }
    (!map.is_empty()).then_some(map)
}

/// 接管一个 Copy 规则返回的 CF 对象
///
/// # Safety
/// `p` 必须是 +1 引用的 CF 对象或空指针
unsafe fn owned(p: ffi::CFTypeRef) -> Option<CFRetained<CFType>> {
    std::ptr::NonNull::new(p.cast_mut().cast::<CFType>())
        // SAFETY: 由调用方保证是 +1 引用
        .map(|p| unsafe { CFRetained::from_raw(p) })
}

/// 某个输入源的字母表：先查 ⌘ 修饰下的字符（快捷键匹配用的就是它），再补无修饰的
fn letters_of_source(source: &CFType) -> HashMap<char, u16> {
    let mut map = HashMap::new();
    // SAFETY: source 是有效的 TISInputSourceRef；属性按 Get 规则返回，不需要释放
    let data = unsafe {
        ffi::TISGetInputSourceProperty((source as *const CFType).cast(), ffi::kTISPropertyUnicodeKeyLayoutData)
    };
    // SAFETY: 该属性的值是 CFData（或空）
    let Some(data) = (unsafe { data.cast::<CFData>().as_ref() }) else {
        return map;
    };
    let layout = data.byte_ptr().cast::<c_void>();
    // SAFETY: 无参数的查询函数
    let kbd_type = u32::from(unsafe { ffi::LMGetKbdType() });
    for modifier_state in [(CARBON_CMD >> 8) & 0xFF, 0] {
        for keycode in 0u16..0x80 {
            let mut dead = 0u32;
            let mut len = 0usize;
            let mut buf = [0u16; 4];
            // SAFETY: layout 指向 UCKeyboardLayout；输出缓冲区长度与 max_string_length 一致
            let status = unsafe {
                ffi::UCKeyTranslate(
                    layout,
                    keycode,
                    ffi::UC_KEY_ACTION_DOWN,
                    modifier_state,
                    kbd_type,
                    ffi::UC_NO_DEAD_KEYS,
                    &mut dead,
                    buf.len(),
                    &mut len,
                    buf.as_mut_ptr(),
                )
            };
            if status != 0 || len != 1 {
                continue;
            }
            if let Some(c) = char::from_u32(u32::from(buf[0])).map(|c| c.to_ascii_lowercase())
                && c.is_ascii_lowercase()
            {
                map.entry(c).or_insert(keycode);
            }
        }
    }
    map
}

/// 组合键的修饰位 → CGEventFlags
fn flags_of(m: &Modifiers) -> CGEventFlags {
    let mut bits = FLAGS_NON_COALESCED;
    if m.super_key {
        bits |= FLAGS_COMMAND;
    }
    if m.shift {
        bits |= FLAGS_SHIFT;
    }
    if m.control {
        bits |= FLAGS_CONTROL;
    }
    if m.alt {
        bits |= FLAGS_ALTERNATE;
    }
    CGEventFlags(bits)
}

/// 发送一次已解析的组合键（按下 + 抬起），投递到 HID 层。需要辅助功能授权，否则系统会静默丢弃
pub(crate) fn post(keys: &KeyStroke) -> Result<(), String> {
    let source = CGEventSource::new(CGEventSourceStateID::Private);
    let tag = i64::from(WISP_INPUT_TAG);
    CGEventSource::set_user_data(source.as_deref(), tag);
    for down in [true, false] {
        let ev = CGEvent::new_keyboard_event(source.as_deref(), keys.keycode, down)
            .ok_or_else(|| "CGEventCreateKeyboardEvent failed".to_owned())?;
        CGEvent::set_flags(Some(&ev), keys.flags);
        CGEvent::set_integer_value_field(Some(&ev), CGEventField::EventSourceUserData, tag);
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
    }
    Ok(())
}

/// 解析并发送一次组合键
pub(crate) fn post_combo(ctx: &Ctx, combo: &Combo) -> Result<(), String> {
    post(&prepare(ctx, combo)?)
}
