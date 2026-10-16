//! 模拟输入（`SendInput`）与「应用 → 复制键 / 粘贴键」映射的解析。
//!
//! 所有注入事件的 `dwExtraInfo` 都写入 [`WISP_TAG`]，本 crate 的低级钩子见到它就放行。UIPI 拦截 `SendInput` 时
//! 系统不会报告原因，因此调用方必须事先检查目标是否已提权。

use ::windows::Win32::Foundation::HWND;
use ::windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, MapVirtualKeyW, SendInput, VIRTUAL_KEY,
};
use ::windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, XBUTTON1, XBUTTON2,
};
use parking_lot::RwLock;
use wisp_types::{AppKeymap, AppRef, OsKind};

use crate::common::combo::Combo;
use crate::common::keymap::{self, KeyChoice};

use super::geom;
use super::keys::{self, KeyCombo, KeyStroke, MODIFIER_VKS};
use super::process;
use super::util::WISP_TAG;

/// 当前仍按着的（左右区分的）修饰键
pub(crate) fn held_modifiers() -> Vec<u16> {
    MODIFIER_VKS
        .iter()
        .map(|(v, _)| *v)
        // SAFETY: 纯查询；最高位为 1 表示该键当前按下
        .filter(|v| unsafe { GetAsyncKeyState(i32::from(*v)) } < 0)
        .collect()
}

/// 当前是否按着任意鼠标键（双击修饰键在没有鼠标钩子时用它判断「中间有没有鼠标按下」）
#[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
pub(crate) fn any_mouse_button_held() -> bool {
    // VK_LBUTTON / VK_RBUTTON / VK_MBUTTON / VK_XBUTTON1 / VK_XBUTTON2
    // SAFETY: 纯查询
    [0x01, 0x02, 0x04, 0x05, 0x06]
        .iter()
        .any(|v| unsafe { GetAsyncKeyState(*v) } < 0)
}

/// 当前是否按着 Ctrl（浮标判定：按着 Ctrl 时不出现）
#[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
pub(crate) fn ctrl_held() -> bool {
    // VK_CONTROL 同时反映左右两个 Ctrl
    // SAFETY: 纯查询
    unsafe { GetAsyncKeyState(0x11) < 0 }
}

fn key_input(stroke: KeyStroke) -> INPUT {
    // SAFETY: 纯查询，把虚拟键映射为扫描码（部分应用按扫描码判断按键）
    let scan = unsafe { MapVirtualKeyW(u32::from(stroke.vk), MAPVK_VK_TO_VSC) } as u16;
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if stroke.up {
        flags |= KEYEVENTF_KEYUP;
    }
    if keys::is_extended(stroke.vk) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(stroke.vk),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: WISP_TAG,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> Result<(), String> {
    if inputs.is_empty() {
        return Ok(());
    }
    // SAFETY: 切片中的 INPUT 已完整初始化，cbsize 为结构体大小
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize == inputs.len() {
        Ok(())
    } else {
        Err(format!(
            "SendInput inserted {sent}/{} events: {}",
            inputs.len(),
            ::windows::core::Error::from_thread()
        ))
    }
}

/// 处理残留修饰键后发送组合键（一次 `SendInput` 调用，事件不会与用户输入交错）
pub(crate) fn send_combo(combo: KeyCombo) -> Result<(), String> {
    let plan = keys::plan_combo(combo, &held_modifiers());
    let inputs: Vec<INPUT> = plan.into_iter().map(key_input).collect();
    send(&inputs)
}

/// 发送一次掩码键（按下 + 抬起）：既能阻止 Alt / Win 抬起时激活菜单，也让本进程成为「最后一次输入」的来源，
/// 从而获得 `SetForegroundWindow` 的权限
pub(crate) fn send_mask_key() -> bool {
    let inputs = [
        key_input(KeyStroke {
            vk: keys::vk::MASK,
            up: false,
        }),
        key_input(KeyStroke {
            vk: keys::vk::MASK,
            up: true,
        }),
    ];
    send(&inputs).is_ok()
}

/// 被拦截后需要补发的鼠标键
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
pub(crate) enum ExtraButton {
    Middle,
    Back,
    Forward,
}

/// 补发一次完整的鼠标单击（长按模式下 400 ms 内抬起时）：单击落在**按下时**的位置。用户按住期间移动了
/// 鼠标时，先把光标移回按下点、单击，再移回抬起点，整个序列一次 `SendInput` 完成（不与用户输入交错），光标的最终位置
/// 不变。坐标是物理像素（调用线程必须是 Per-Monitor V2，与低级钩子给出的坐标同一坐标系）
#[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
pub(crate) fn reinject_click(button: ExtraButton, down_at: (i32, i32), up_at: (i32, i32)) -> Result<(), String> {
    let (down, up, data) = match button {
        ExtraButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0u32),
        ExtraButton::Back => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, u32::from(XBUTTON1)),
        ExtraButton::Forward => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, u32::from(XBUTTON2)),
    };
    let make = |flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: u32| INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: WISP_TAG,
            },
        },
    };
    // SAFETY: 纯查询（虚拟桌面的物理像素范围，调用线程是 Per-Monitor V2）
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    let move_to = |(x, y): (i32, i32)| {
        make(
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            geom::abs_mouse_coord(x, vx, vw),
            geom::abs_mouse_coord(y, vy, vh),
            0,
        )
    };
    let moved = down_at != up_at && vw > 0 && vh > 0;
    let mut inputs = Vec::with_capacity(4);
    if moved {
        inputs.push(move_to(down_at));
    }
    inputs.push(make(down, 0, 0, data));
    inputs.push(make(up, 0, 0, data));
    if moved {
        inputs.push(move_to(up_at));
    }
    send(&inputs)
}

/// 复制键（`copy = true`）或粘贴键：合并表 = 用户条目在前 + 内置默认表在后，按 `common::keymap` 的规则
/// 逐字段查找（exe 名带不带 `.exe` 都能匹配），没有条目时用平台默认 Ctrl+C / Ctrl+V。返回 None 表示映射表要求不发键
fn app_keys(user: &RwLock<Vec<AppKeymap>>, app: Option<&AppRef>, copy: bool) -> Option<KeyCombo> {
    let mut table = user.read().clone();
    table.extend(keymap::builtin(OsKind::Windows));
    let choice = if copy {
        keymap::copy_key(&table, OsKind::Windows, app)
    } else {
        keymap::paste_key(&table, OsKind::Windows, app)
    };
    let fallback = if copy { KeyCombo::CTRL_C } else { KeyCombo::CTRL_V };
    match choice {
        KeyChoice::Skip => None,
        KeyChoice::Send(combo) => Some(to_key_combo(&combo).unwrap_or_else(|| {
            log::warn!(
                "keymap key {:?} has no Windows virtual key; using the platform default",
                combo.code
            );
            fallback
        })),
    }
}

/// 某个前台窗口应发送的复制键 / 粘贴键：在 [`app_keys`] 之前按窗口类修正查找对象（控制台窗口额外按 conhost.exe
/// 匹配，见 `keys::keymap_subject`）。`app` 是该窗口的应用（可能是触发时记录的来源应用）
pub(crate) fn app_keys_for_window(
    user: &RwLock<Vec<AppKeymap>>,
    app: Option<&AppRef>,
    hwnd: HWND,
    copy: bool,
) -> Option<KeyCombo> {
    let subject = keys::keymap_subject(app.cloned(), &process::class_name(hwnd));
    app_keys(user, subject.as_ref(), copy)
}

/// 规范组合键 → 虚拟键码组合
pub(crate) fn to_key_combo(c: &Combo) -> Option<KeyCombo> {
    Some(KeyCombo {
        ctrl: c.modifiers.control,
        alt: c.modifiers.alt,
        shift: c.modifiers.shift,
        win: c.modifiers.super_key,
        vk: c.win_vk()?,
    })
}
