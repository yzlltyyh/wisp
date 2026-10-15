//! 按键规划（纯逻辑，不调用 Win32）：扩展键判定、发送组合键前处理残留修饰键的按键序列，
//! 以及「前台窗口 → 映射表查找对象」的修正。
//! W3C `KeyboardEvent.code` → 虚拟键码的映射在 `common::combo`（`Combo::win_vk`）。

use wisp_types::AppRef;

/// 修饰键类别
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModKind {
    Ctrl,
    Alt,
    Shift,
    Win,
}

/// 虚拟键码
pub(crate) mod vk {
    pub(crate) const SHIFT_L: u16 = 0xA0;
    pub(crate) const SHIFT_R: u16 = 0xA1;
    pub(crate) const CTRL_L: u16 = 0xA2;
    pub(crate) const CTRL_R: u16 = 0xA3;
    pub(crate) const ALT_L: u16 = 0xA4;
    pub(crate) const ALT_R: u16 = 0xA5;
    pub(crate) const WIN_L: u16 = 0x5B;
    pub(crate) const WIN_R: u16 = 0x5C;
    /// 未分配的虚拟键：在 Alt / Win 仍按着时插入一次按下 + 抬起，阻止系统在随后的抬起时激活菜单栏 / 开始菜单
    pub(crate) const MASK: u16 = 0xE8;
    pub(crate) const C: u16 = 0x43;
    pub(crate) const V: u16 = 0x56;
}

/// 8 个左右区分的修饰键
pub(crate) const MODIFIER_VKS: [(u16, ModKind); 8] = [
    (vk::SHIFT_L, ModKind::Shift),
    (vk::SHIFT_R, ModKind::Shift),
    (vk::CTRL_L, ModKind::Ctrl),
    (vk::CTRL_R, ModKind::Ctrl),
    (vk::ALT_L, ModKind::Alt),
    (vk::ALT_R, ModKind::Alt),
    (vk::WIN_L, ModKind::Win),
    (vk::WIN_R, ModKind::Win),
];

/// 某虚拟键是否为（左右区分的）修饰键
pub(crate) fn modifier_kind(v: u16) -> Option<ModKind> {
    MODIFIER_VKS.iter().find(|(k, _)| *k == v).map(|(_, m)| *m)
}

/// 修饰键 + 主键
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeyCombo {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    pub vk: u16,
}

impl KeyCombo {
    /// Ctrl+C
    pub(crate) const CTRL_C: Self = Self {
        ctrl: true,
        alt: false,
        shift: false,
        win: false,
        vk: vk::C,
    };
    /// Ctrl+V
    pub(crate) const CTRL_V: Self = Self {
        ctrl: true,
        alt: false,
        shift: false,
        win: false,
        vk: vk::V,
    };

    fn wants(&self, m: ModKind) -> bool {
        match m {
            ModKind::Ctrl => self.ctrl,
            ModKind::Alt => self.alt,
            ModKind::Shift => self.shift,
            ModKind::Win => self.win,
        }
    }
}

/// 一次按下或抬起
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeyStroke {
    pub vk: u16,
    pub up: bool,
}

impl KeyStroke {
    const fn down(vk: u16) -> Self {
        Self { vk, up: false }
    }
    const fn up(vk: u16) -> Self {
        Self { vk, up: true }
    }
}

/// 规划发送组合键的完整按键序列：
/// 1. 用户仍按着、但组合键不需要的修饰键注入抬起；其中有 Alt / Win 时先发一次掩码键（0xE8），避免抬起时激活
///    菜单栏 / 开始菜单；不回放按下状态；
/// 2. 组合键需要、但用户没有按着的修饰键补发按下（用左侧键），主键按下 + 抬起，再按相反顺序抬起补发的修饰键；
///    用户本来就按着的、组合键也需要的修饰键保持不动
pub(crate) fn plan_combo(combo: KeyCombo, held: &[u16]) -> Vec<KeyStroke> {
    let mut out = Vec::with_capacity(16);
    let held_mods: Vec<(u16, ModKind)> = held.iter().filter_map(|&v| modifier_kind(v).map(|m| (v, m))).collect();
    let release: Vec<(u16, ModKind)> = held_mods.iter().copied().filter(|(_, m)| !combo.wants(*m)).collect();
    if release.iter().any(|(_, m)| matches!(m, ModKind::Alt | ModKind::Win)) {
        out.push(KeyStroke::down(vk::MASK));
        out.push(KeyStroke::up(vk::MASK));
    }
    out.extend(release.iter().map(|(v, _)| KeyStroke::up(*v)));

    let satisfied = |m: ModKind| held_mods.iter().any(|(_, h)| *h == m);
    let mut pressed: Vec<u16> = Vec::with_capacity(4);
    for (m, v) in [
        (ModKind::Ctrl, vk::CTRL_L),
        (ModKind::Alt, vk::ALT_L),
        (ModKind::Shift, vk::SHIFT_L),
        (ModKind::Win, vk::WIN_L),
    ] {
        if combo.wants(m) && !satisfied(m) {
            out.push(KeyStroke::down(v));
            pressed.push(v);
        }
    }
    out.push(KeyStroke::down(combo.vk));
    out.push(KeyStroke::up(combo.vk));
    out.extend(pressed.iter().rev().map(|v| KeyStroke::up(*v)));
    out
}

/// 需要 `KEYEVENTF_EXTENDEDKEY` 的虚拟键
pub(crate) fn is_extended(v: u16) -> bool {
    matches!(
        v,
        0x21..=0x28 // PageUp PageDown End Home ← ↑ → ↓
            | 0x2C // PrintScreen
            | 0x2D // Insert
            | 0x2E // Delete
            | 0x5B | 0x5C | 0x5D // Win / Apps
            | 0x6F // Numpad /
            | 0x90 // NumLock
            | vk::CTRL_R
            | vk::ALT_R
    )
}

/// 传统控制台宿主的 exe 名（`common::keymap` 内置表里的条目 id）
pub(crate) const CONSOLE_HOST_EXE: &str = "conhost.exe";

/// 由控制台宿主承载的窗口类：`ConsoleWindowClass`（conhost.exe 的经典控制台窗口）与 `PseudoConsoleWindow`（ConPTY
/// 的隐藏伪窗口）。conhost 会用 `ConsoleSetWindowOwner` 把窗口的所属进程改成客户端（cmd.exe / powershell.exe /
/// python.exe …），`GetWindowThreadProcessId` 拿到的是客户端而不是 conhost.exe，只按进程查表会错过内置的
/// conhost 条目、退回 Ctrl+C，在控制台里中断正在运行的命令
pub(crate) fn is_console_host_class(class: &str) -> bool {
    class == "ConsoleWindowClass" || class == "PseudoConsoleWindow"
}

/// 映射表的查找对象：控制台窗口在客户端 exe 之外再按 conhost.exe 匹配（放进 `alt_id`；
/// `common::keymap` 对 id / alt_id 一视同仁，合并表仍是用户条目优先：用户给 python.exe 或 conhost.exe 写的条目
/// 都先于内置的 conhost 条目命中）。客户端进程解析不出来时直接按 conhost.exe 查
pub(crate) fn keymap_subject(app: Option<AppRef>, window_class: &str) -> Option<AppRef> {
    if !is_console_host_class(window_class) {
        return app;
    }
    let mut app = app.unwrap_or_else(|| AppRef {
        id: CONSOLE_HOST_EXE.to_owned(),
        alt_id: None,
        name: "conhost".to_owned(),
        pid: None,
        path: None,
        window: None,
    });
    if !app.id.eq_ignore_ascii_case(CONSOLE_HOST_EXE) {
        app.alt_id = Some(CONSOLE_HOST_EXE.to_owned());
    }
    Some(app)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strokes(s: &[KeyStroke]) -> Vec<(u16, bool)> {
        s.iter().map(|k| (k.vk, k.up)).collect()
    }

    #[test]
    fn plain_ctrl_c_without_held_keys() {
        let plan = plan_combo(KeyCombo::CTRL_C, &[]);
        assert_eq!(
            strokes(&plan),
            vec![(vk::CTRL_L, false), (vk::C, false), (vk::C, true), (vk::CTRL_L, true)]
        );
    }

    #[test]
    fn residual_ctrl_alt_from_hotkey() {
        // Ctrl+Alt+D 仍按着：Ctrl 保留（组合键需要），Alt 抬起前先发掩码键，不补发 Ctrl
        let plan = plan_combo(KeyCombo::CTRL_C, &[vk::CTRL_L, vk::ALT_L]);
        assert_eq!(
            strokes(&plan),
            vec![
                (vk::MASK, false),
                (vk::MASK, true),
                (vk::ALT_L, true),
                (vk::C, false),
                (vk::C, true)
            ]
        );
    }

    #[test]
    fn residual_shift_is_released_without_mask() {
        // ⇧↩ 粘贴：Shift 抬起（无需掩码键），目标收到的是 Ctrl+V
        let plan = plan_combo(KeyCombo::CTRL_V, &[vk::SHIFT_R]);
        assert_eq!(
            strokes(&plan),
            vec![
                (vk::SHIFT_R, true),
                (vk::CTRL_L, false),
                (vk::V, false),
                (vk::V, true),
                (vk::CTRL_L, true)
            ]
        );
    }

    #[test]
    fn residual_win_gets_mask_key() {
        let plan = plan_combo(KeyCombo::CTRL_V, &[vk::WIN_L]);
        assert_eq!(plan[0], KeyStroke::down(vk::MASK));
        assert_eq!(plan[1], KeyStroke::up(vk::MASK));
        assert_eq!(plan[2], KeyStroke::up(vk::WIN_L));
    }

    const INSERT: u16 = 0x2D;

    #[test]
    fn shift_insert_for_terminals() {
        let combo = KeyCombo {
            ctrl: false,
            alt: false,
            shift: true,
            win: false,
            vk: INSERT,
        };
        let plan = plan_combo(combo, &[vk::CTRL_R]);
        assert_eq!(
            strokes(&plan),
            vec![
                (vk::CTRL_R, true),
                (vk::SHIFT_L, false),
                (INSERT, false),
                (INSERT, true),
                (vk::SHIFT_L, true)
            ]
        );
        assert!(is_extended(INSERT));
        assert!(!is_extended(vk::C));
    }

    #[test]
    fn modifier_table() {
        assert_eq!(modifier_kind(vk::ALT_R), Some(ModKind::Alt));
        assert_eq!(modifier_kind(vk::C), None);
    }

    fn app(id: &str) -> AppRef {
        AppRef {
            id: id.to_owned(),
            alt_id: None,
            name: id.to_owned(),
            pid: Some(42),
            path: None,
            window: None,
        }
    }

    #[test]
    fn console_windows_are_looked_up_as_conhost() {
        assert!(is_console_host_class("ConsoleWindowClass"));
        assert!(is_console_host_class("PseudoConsoleWindow"));
        assert!(!is_console_host_class("CASCADIA_HOSTING_WINDOW_CLASS"));
        assert!(!is_console_host_class("consolewindowclass"), "窗口类名区分大小写");

        // 经典控制台：所属进程是客户端（python.exe），查表时再按 conhost.exe 匹配
        let s = keymap_subject(Some(app("python.exe")), "ConsoleWindowClass").expect("subject");
        assert_eq!(s.id, "python.exe");
        assert_eq!(s.alt_id.as_deref(), Some(CONSOLE_HOST_EXE));
        assert_eq!(s.pid, Some(42), "其余字段保持不变");
        // 客户端解析不出来时直接按 conhost.exe 查
        let s = keymap_subject(None, "PseudoConsoleWindow").expect("subject");
        assert_eq!(s.id, CONSOLE_HOST_EXE);
        assert_eq!(s.alt_id, None);
        // 本来就是 conhost.exe：不重复登记
        let s = keymap_subject(Some(app("CONHOST.EXE")), "ConsoleWindowClass").expect("subject");
        assert_eq!(s.alt_id, None);
        // 普通窗口原样返回
        assert_eq!(
            keymap_subject(Some(app("notepad.exe")), "Notepad"),
            Some(app("notepad.exe"))
        );
        assert_eq!(keymap_subject(None, "Notepad"), None);
    }

    #[test]
    fn console_copy_key_never_falls_back_to_ctrl_c() {
        use crate::common::keymap::{self, KeyChoice};
        use wisp_types::{AppKeymap, OsKind};

        let builtin = keymap::builtin(OsKind::Windows);
        let copy_of = |table: &[AppKeymap], subject: Option<AppRef>| match keymap::copy_key(
            table,
            OsKind::Windows,
            subject.as_ref(),
        ) {
            KeyChoice::Send(c) => Some((c.modifiers.control, c.modifiers.shift, c.code)),
            KeyChoice::Skip => None,
        };
        let ctrl_shift_c = Some((true, true, "KeyC".to_owned()));
        // conhost 里跑着 python.exe：按 conhost 的内置条目发 Ctrl+Shift+C，而不是会中断程序的 Ctrl+C
        let subject = keymap_subject(Some(app("python.exe")), "ConsoleWindowClass");
        assert_eq!(copy_of(&builtin, subject.clone()), ctrl_shift_c);
        // 修正之前（只按客户端 exe 查）会退回平台默认 Ctrl+C
        assert_eq!(
            copy_of(&builtin, Some(app("python.exe"))),
            Some((true, false, "KeyC".to_owned()))
        );
        // 用户条目优先：给客户端 exe 或 conhost.exe 写的条目都先于内置条目
        for user_id in ["python.exe", "conhost"] {
            let mut table = vec![AppKeymap {
                os: OsKind::Windows,
                app_id: user_id.to_owned(),
                copy: Some("Control+Insert".to_owned()),
                paste: None,
            }];
            table.extend(builtin.iter().cloned());
            assert_eq!(
                copy_of(&table, subject.clone()),
                Some((true, false, "Insert".to_owned())),
                "user entry {user_id}"
            );
        }
    }
}
