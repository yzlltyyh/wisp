//! `common/` 纯逻辑测试：组合键解析、文本规范化、双击修饰键（时序与按键组合的单测矩阵）、浮标手势与长按、抑制窗口、
//! 复制 / 粘贴键映射表、隐私标记。三端 CI 都跑。

use std::time::{Duration, Instant};

use wisp_platform::common::combo::{
    self, CARBON_CMD, CARBON_CONTROL, CARBON_FN, CARBON_OPTION, CARBON_SHIFT, Combo, Modifiers, SYMBOLIC_FN_BIT,
    canonical, same_combo,
};
use wisp_platform::common::double_tap::mac::{
    DEV_LALT, DEV_LCMD, DEV_LSHIFT, DEV_RALT, DEV_RCTL, FLAG_ALTERNATE, FLAG_CAPS_LOCK, FLAG_COMMAND, FLAG_CONTROL,
    FLAG_NON_COALESCED, FLAG_SHIFT, FlagsEvent, KC_LEFT_SHIFT, KC_RIGHT_ALT, KC_RIGHT_CTRL, classify_flags_changed,
};
use wisp_platform::common::double_tap::{DoubleTapMachine, MAX_HOLD, TapInput};
use wisp_platform::common::gesture::{
    self, BOBBER_DIAMETER, BOBBER_HIT_RADIUS, BOBBER_MOVE_AWAY_PX, BOBBER_WINDOW, DRAG_THRESHOLD_PX, LONG_PRESS,
    LongPress, LongPressUp, MouseUpFacts, bobber_hit, bobber_rect, bobber_window_rect, is_bobber_candidate, moved_away,
};
use wisp_platform::common::keymap::{self, KEY_NONE, KeyChoice, builtin, copy_key, paste_key, resolve_app};
use wisp_platform::common::secrets_markers::{
    LINUX_PASSWORD_HINT_MIME, MACOS_CONCEALED_TYPES, WINDOWS_EXCLUDE_FORMATS, WINDOWS_ZERO_MEANS_PRIVATE_FORMATS,
    WISP_MARKER, has_wisp_marker, linux_is_password_hint, macos_is_concealed, windows_format_is_private,
};
use wisp_platform::common::suppress::Suppressor;
use wisp_platform::common::text::{
    MAX_SELECTION_BYTES, is_blank, normalize_selection, normalize_with_limit, truncate_utf8_lossy,
};
use wisp_platform::{MouseSpec, SuppressRange};
use wisp_types::{AppKeymap, AppRef, HotkeyAction, ModifierKey, MouseButtonKind, OsKind, Point, Rect};

// ---------------------------------------------------------------- combo

#[test]
fn combo_canonical_order_and_aliases() {
    let cases = [
        ("Alt+KeyD", "Alt+KeyD"),
        ("alt+d", "Alt+KeyD"),
        ("Option+D", "Alt+KeyD"),
        ("shift+control+alt+super+KeyD", "Control+Alt+Shift+Super+KeyD"),
        ("Super+Shift+Alt+Control+KeyD", "Control+Alt+Shift+Super+KeyD"),
        ("  ctrl + shift + v ", "Control+Shift+KeyV"),
        ("Cmd+Space", "Super+Space"),
        ("Command+Space", "Super+Space"),
        ("Meta+KeyK", "Super+KeyK"),
        ("Ctrl+Esc", "Control+Escape"),
        ("Shift+Insert", "Shift+Insert"),
        ("Control+Up", "Control+ArrowUp"),
        ("Alt+1", "Alt+Digit1"),
        ("Alt+`", "Alt+Backquote"),
        ("Ctrl+Num5", "Control+Numpad5"),
        ("F13", "F13"),
        ("f5", "F5"),
        ("Ctrl+Return", "Control+Enter"),
    ];
    for (input, expected) in cases {
        assert_eq!(canonical(input).as_deref(), Some(expected), "input {input:?}");
    }
}

#[test]
fn combo_rejects_malformed_strings() {
    for bad in [
        "",
        "Ctrl",
        "Ctrl+Shift",
        "Ctrl+C+A",
        "C+Shift",
        "Ctrl++",
        "+D",
        "Ctrl+ +D",
        "Hyper+D",
        "Ctrl+NoSuchKey",
        "Fn+D",
    ] {
        assert_eq!(Combo::parse(bad), None, "input {bad:?}");
    }
}

#[test]
fn combo_cmd_or_ctrl_follows_platform() {
    let c = Combo::parse("CmdOrCtrl+Shift+KeyS").expect("parse");
    if cfg!(target_os = "macos") {
        assert_eq!(c.to_canonical(), "Shift+Super+KeyS");
    } else {
        assert_eq!(c.to_canonical(), "Control+Shift+KeyS");
    }
    assert_eq!(canonical("CommandOrControl+KeyA"), canonical("CmdOrCtrl+A"));
}

#[test]
fn combo_comparison_goes_through_parsing() {
    assert!(same_combo("shift+control+KeyV", "Control+Shift+KeyV"));
    assert!(same_combo("alt+d", "Option+KeyD"));
    assert!(!same_combo("Alt+KeyD", "Alt+KeyF"));
    assert!(!same_combo("Alt+KeyD", "garbage"));
    // 规范串经过一次往返保持不变
    for s in ["Control+Alt+Shift+Super+KeyD", "Alt+Space", "F19"] {
        assert_eq!(canonical(s).as_deref(), Some(s));
    }
}

#[test]
fn combo_keycodes_for_three_platforms() {
    let c = Combo::parse("Super+KeyC").expect("parse");
    assert_eq!(c.mac_keycode(), Some(0x08));
    assert_eq!(c.win_vk(), Some(0x43));
    assert_eq!(c.x11_keysym(), Some(0x63));
    assert_eq!(c.letter(), Some('c'));
    let v = Combo::parse("Control+KeyV").expect("parse");
    assert_eq!(v.mac_keycode(), Some(0x09));
    assert_eq!(v.win_vk(), Some(0x56));
    let ins = Combo::parse("Shift+Insert").expect("parse");
    assert_eq!(ins.win_vk(), Some(0x2D));
    assert_eq!(ins.x11_keysym(), Some(0xFF63));
    assert_eq!(ins.letter(), None);
    assert_eq!(combo::code_from_mac_keycode(0x31), Some("Space"));
    assert_eq!(combo::letter_of("Digit1"), None);
    assert_eq!(combo::letter_of("KeyZ"), Some('z'));
}

#[test]
fn combo_carbon_modifiers_round_trip() {
    let all = Modifiers {
        control: true,
        alt: true,
        shift: true,
        super_key: true,
    };
    assert_eq!(
        all.to_carbon(),
        CARBON_CMD | CARBON_SHIFT | CARBON_OPTION | CARBON_CONTROL
    );
    assert_eq!(Modifiers::from_carbon(all.to_carbon()), all);
    assert!(Modifiers::NONE.is_empty());
    assert_eq!(Modifiers::from_carbon(0), Modifiers::NONE);
}

#[test]
fn combo_from_symbolic_hotkey_strips_fn_bit() {
    // Spotlight：⌘Space
    let spot = Combo::from_mac(0x31, CARBON_CMD).expect("known keycode");
    assert_eq!(spot.to_canonical(), "Super+Space");
    // Mission Control 左移一个空间：⌃←（系统在方向键上合成 Fn 位；macOS 26.4 实测条目为 0x21000）
    for fn_bit in [CARBON_FN, SYMBOLIC_FN_BIT] {
        let left = Combo::from_mac(0x7B, CARBON_CONTROL | fn_bit).expect("known keycode");
        assert_eq!(left.to_canonical(), "Control+ArrowLeft");
        // F 键同理：⌃F3
        let f3 = Combo::from_mac(0x63, CARBON_CONTROL | fn_bit).expect("known keycode");
        assert_eq!(f3.to_canonical(), "Control+F3");
        // 🌐C（控制中心）：字母键上的 Fn 是真实修饰键，global-hotkey 注册不了，不参与冲突比较
        assert_eq!(Combo::from_mac(0x08, fn_bit), None);
    }
    // 右侧修饰位与左侧同义
    let right = Combo::from_mac(0x31, combo::CARBON_RIGHT_OPTION | CARBON_CMD).expect("known keycode");
    assert_eq!(right.to_canonical(), "Alt+Super+Space");
    // 专用键（亮度、调度中心）与未绑定（0xFFFF）
    assert_eq!(Combo::from_mac(0x90, 0), None);
    assert_eq!(Combo::from_mac(0xFFFF, 0), None);
    assert!(combo::fn_is_synthesized("F12") && combo::fn_is_synthesized("PageDown"));
    assert!(!combo::fn_is_synthesized("KeyF") && !combo::fn_is_synthesized("Space"));
}

// ---------------------------------------------------------------- text

#[test]
fn text_normalizes_line_endings_and_nul() {
    assert_eq!(normalize_selection("a\r\nb\rc\nd"), ("a\nb\nc\nd".to_owned(), false));
    assert_eq!(normalize_selection("a\r\n\r\nb"), ("a\n\nb".to_owned(), false));
    assert_eq!(normalize_selection("\r"), ("\n".to_owned(), false));
    assert_eq!(normalize_selection("x\0y\0"), ("xy".to_owned(), false));
    // 内部空白原样保留，不 trim
    assert_eq!(normalize_selection("  a \t b  "), ("  a \t b  ".to_owned(), false));
}

#[test]
fn text_blank_detection_trims_only_for_the_check() {
    assert!(is_blank(""));
    assert!(is_blank(" \n\t\u{3000}"));
    assert!(!is_blank(" x "));
    let (t, _) = normalize_selection(" \r\n ");
    assert!(is_blank(&t));
}

#[test]
fn text_truncates_on_char_boundary() {
    // 3 字节字符：上限 7 时只能放下 2 个（6 字节）
    let (t, cut) = normalize_with_limit("你好世界", 7);
    assert_eq!((t.as_str(), cut), ("你好", true));
    let (t, cut) = normalize_with_limit("你好", 6);
    assert_eq!((t.as_str(), cut), ("你好", false));
    // 恰好 1 MB 不截断，多 1 字节截断
    let exact = "a".repeat(MAX_SELECTION_BYTES);
    assert!(!normalize_selection(&exact).1);
    let over = "a".repeat(MAX_SELECTION_BYTES + 1);
    let (t, cut) = normalize_selection(&over);
    assert!(cut);
    assert_eq!(t.len(), MAX_SELECTION_BYTES);
    // 超长且含多字节字符：结果不超过上限且是合法 UTF-8
    let wide = "字".repeat(MAX_SELECTION_BYTES / 3 + 10);
    let (t, cut) = normalize_selection(&wide);
    assert!(cut && t.len() <= MAX_SELECTION_BYTES && t.len() % 3 == 0);
}

#[test]
fn text_truncate_utf8_lossy_respects_boundaries() {
    let bytes = "ab字".as_bytes(); // 5 字节
    assert_eq!(truncate_utf8_lossy(bytes, 5), ("ab字".to_owned(), false));
    assert_eq!(truncate_utf8_lossy(bytes, 4), ("ab".to_owned(), true));
    assert_eq!(truncate_utf8_lossy(bytes, 2), ("ab".to_owned(), true));
    assert_eq!(truncate_utf8_lossy(b"\xFFok", 10), ("\u{FFFD}ok".to_owned(), false));
}

// ---------------------------------------------------------------- double tap

/// 按「按下时刻, 按住时长」描述一串点按，返回触发次数
fn taps(interval_ms: u64, seq: &[(u64, u64)]) -> usize {
    let base = Instant::now();
    let at = |ms: u64| base + Duration::from_millis(ms);
    let mut m = DoubleTapMachine::new(Duration::from_millis(interval_ms));
    let mut fired = 0;
    for &(down, hold) in seq {
        fired += usize::from(m.feed(TapInput::TargetDown(at(down))));
        fired += usize::from(m.feed(TapInput::TargetUp(at(down + hold))));
    }
    fired
}

#[test]
fn double_tap_interval_boundaries() {
    assert_eq!(taps(300, &[(0, 50), (299, 50)]), 1, "299 ms 触发");
    assert_eq!(taps(300, &[(0, 50), (300, 50)]), 1, "恰好 300 ms 触发");
    assert_eq!(taps(300, &[(0, 50), (301, 50)]), 0, "301 ms 不触发");
    assert_eq!(taps(200, &[(0, 40), (201, 40)]), 0);
    assert_eq!(taps(400, &[(0, 40), (399, 40)]), 1);
}

#[test]
fn double_tap_rejects_long_holds() {
    assert_eq!(taps(300, &[(0, 250), (280, 50)]), 0, "第一次按住 250 ms");
    assert_eq!(taps(300, &[(0, 50), (200, 250)]), 0, "第二次按住 250 ms");
    let hold = MAX_HOLD.as_millis() as u64;
    assert_eq!(taps(300, &[(0, hold), (250, hold)]), 1, "按住恰好 200 ms 仍算点按");
}

#[test]
fn double_tap_fires_once_per_pair() {
    assert_eq!(taps(300, &[(0, 50), (150, 50), (300, 50)]), 1, "三连击只触发一次");
    assert_eq!(
        taps(300, &[(0, 50), (150, 50), (300, 50), (450, 50)]),
        2,
        "四连击触发两次"
    );
    // 第一对间隔过长，第二、三次构成一对
    assert_eq!(taps(300, &[(0, 50), (500, 50), (650, 50)]), 1);
}

#[test]
fn double_tap_cancelled_by_other_input() {
    let base = Instant::now();
    let at = |ms: u64| base + Duration::from_millis(ms);
    for interrupt in [TapInput::OtherKey, TapInput::MouseDown] {
        let mut m = DoubleTapMachine::new(Duration::from_millis(300));
        assert!(!m.feed(TapInput::TargetDown(at(0))));
        assert!(!m.feed(TapInput::TargetUp(at(50))));
        assert!(m.pending_since().is_some());
        assert!(!m.feed(interrupt));
        assert!(m.pending_since().is_none());
        assert!(!m.feed(TapInput::TargetDown(at(120))));
        assert!(!m.feed(TapInput::TargetUp(at(170))), "{interrupt:?} 之后不应触发");
    }
    // 第二次按住期间夹带按键
    let mut m = DoubleTapMachine::new(Duration::from_millis(300));
    m.feed(TapInput::TargetDown(at(0)));
    m.feed(TapInput::TargetUp(at(40)));
    m.feed(TapInput::TargetDown(at(100)));
    m.feed(TapInput::OtherKey);
    assert!(!m.feed(TapInput::TargetUp(at(140))));
}

#[test]
fn double_tap_ignores_stray_up_and_resets() {
    let base = Instant::now();
    let at = |ms: u64| base + Duration::from_millis(ms);
    let mut m = DoubleTapMachine::new(Duration::from_millis(300));
    assert!(!m.feed(TapInput::TargetUp(at(0))));
    m.feed(TapInput::TargetDown(at(10)));
    m.reset();
    assert!(m.pending_since().is_none());
    assert_eq!(m.interval(), Duration::from_millis(300));
}

#[test]
fn mac_flags_classification_matrix() {
    let ralt = ModifierKey::RightAlt;
    let down = DEV_RALT | FLAG_ALTERNATE | FLAG_NON_COALESCED;
    assert_eq!(classify_flags_changed(KC_RIGHT_ALT, down, ralt), FlagsEvent::TargetDown);
    // Caps Lock 开启时仍是唯一按下
    assert_eq!(
        classify_flags_changed(KC_RIGHT_ALT, down | FLAG_CAPS_LOCK, ralt),
        FlagsEvent::TargetDown
    );
    // 抬起：设备位消失
    assert_eq!(
        classify_flags_changed(KC_RIGHT_ALT, FLAG_NON_COALESCED, ralt),
        FlagsEvent::TargetUp
    );
    assert_eq!(
        classify_flags_changed(KC_RIGHT_ALT, FLAG_CAPS_LOCK, ralt),
        FlagsEvent::TargetUp
    );
    // 按着左 ⌥ 再按右 ⌥：不是唯一按下
    assert_eq!(
        classify_flags_changed(KC_RIGHT_ALT, down | DEV_LALT, ralt),
        FlagsEvent::Other
    );
    // 按着 ⇧ / ⌘ 时按右 ⌥
    assert_eq!(
        classify_flags_changed(KC_RIGHT_ALT, down | DEV_LSHIFT | FLAG_SHIFT, ralt),
        FlagsEvent::Other
    );
    assert_eq!(
        classify_flags_changed(KC_RIGHT_ALT, down | DEV_LCMD | FLAG_COMMAND, ralt),
        FlagsEvent::Other
    );
    // 其他修饰键的变化作废序列
    assert_eq!(
        classify_flags_changed(KC_LEFT_SHIFT, DEV_LSHIFT | FLAG_SHIFT, ralt),
        FlagsEvent::Other
    );
    // 不报设备位的键盘：退回通用位
    assert_eq!(
        classify_flags_changed(KC_RIGHT_ALT, FLAG_ALTERNATE, ralt),
        FlagsEvent::TargetDown
    );
    assert_eq!(classify_flags_changed(KC_RIGHT_ALT, 0, ralt), FlagsEvent::TargetUp);
    // 右 Ctrl
    let rctl = ModifierKey::RightCtrl;
    assert_eq!(
        classify_flags_changed(KC_RIGHT_CTRL, DEV_RCTL | FLAG_CONTROL, rctl),
        FlagsEvent::TargetDown
    );
}

#[test]
fn mac_double_tap_with_caps_lock_end_to_end() {
    let base = Instant::now();
    let at = |ms: u64| base + Duration::from_millis(ms);
    let ralt = ModifierKey::RightAlt;
    let mut m = DoubleTapMachine::new(Duration::from_millis(300));
    let mut fired = false;
    for (ms, flags) in [
        (0, DEV_RALT | FLAG_ALTERNATE | FLAG_CAPS_LOCK),
        (60, FLAG_CAPS_LOCK),
        (180, DEV_RALT | FLAG_ALTERNATE | FLAG_CAPS_LOCK),
        (230, FLAG_CAPS_LOCK),
    ] {
        let input = match classify_flags_changed(KC_RIGHT_ALT, flags, ralt) {
            FlagsEvent::TargetDown => TapInput::TargetDown(at(ms)),
            FlagsEvent::TargetUp => TapInput::TargetUp(at(ms)),
            FlagsEvent::Other => TapInput::OtherKey,
        };
        fired |= m.feed(input);
    }
    assert!(fired);
}

// ---------------------------------------------------------------- gesture

fn facts(drag_px: f64, click_count: u32) -> MouseUpFacts {
    MouseUpFacts {
        drag_px,
        click_count,
        command_or_control: false,
        in_own_window: false,
    }
}

#[test]
fn bobber_candidate_matrix() {
    assert!(is_bobber_candidate(&facts(30.0, 1)), "拖选");
    assert!(is_bobber_candidate(&facts(DRAG_THRESHOLD_PX, 1)), "恰好 4 px");
    assert!(!is_bobber_candidate(&facts(3.9, 1)), "单击");
    assert!(!is_bobber_candidate(&facts(0.0, 1)));
    assert!(is_bobber_candidate(&facts(0.0, 2)), "双击选词");
    assert!(is_bobber_candidate(&facts(0.0, 3)), "三击选段");
    let cmd = MouseUpFacts {
        command_or_control: true,
        ..facts(30.0, 1)
    };
    assert!(!is_bobber_candidate(&cmd), "按住 ⌘ / Ctrl");
    let own = MouseUpFacts {
        in_own_window: true,
        ..facts(30.0, 2)
    };
    assert!(!is_bobber_candidate(&own), "Wisp 自己的窗口");
}

#[test]
fn bobber_geometry_flips_at_edges() {
    let area = Rect::new(0.0, 0.0, 1000.0, 800.0);
    let d = BOBBER_DIAMETER;
    // 默认在右下 (8, 12)
    assert_eq!(
        bobber_rect(Point { x: 100.0, y: 100.0 }, area),
        Rect::new(108.0, 112.0, d, d)
    );
    // 右边越界：水平翻到左侧
    let r = bobber_rect(Point { x: 990.0, y: 100.0 }, area);
    assert_eq!((r.x, r.y), (990.0 - 8.0 - d, 112.0));
    // 下边越界：垂直翻到上方
    let r = bobber_rect(Point { x: 100.0, y: 790.0 }, area);
    assert_eq!((r.x, r.y), (108.0, 790.0 - 12.0 - d));
    // 右下角：两个方向都翻
    let r = bobber_rect(Point { x: 995.0, y: 795.0 }, area);
    assert_eq!((r.x, r.y), (995.0 - 8.0 - d, 795.0 - 12.0 - d));
    // 四个角都完整可见
    for p in [(0.0, 0.0), (1000.0, 0.0), (0.0, 800.0), (1000.0, 800.0), (-5.0, -5.0)] {
        let r = bobber_rect(Point { x: p.0, y: p.1 }, area);
        assert!(
            r.x >= area.x && r.y >= area.y && r.right() <= area.right() && r.bottom() <= area.bottom(),
            "{p:?}"
        );
    }
    // 工作区不从 0 开始（菜单栏在上方）
    let work = Rect::new(0.0, 25.0, 1000.0, 775.0);
    let r = bobber_rect(Point { x: 10.0, y: 0.0 }, work);
    assert!(r.y >= 25.0);
    let w = bobber_window_rect(Rect::new(108.0, 112.0, d, d));
    assert_eq!((w.width, w.height), (BOBBER_WINDOW, BOBBER_WINDOW));
    assert_eq!(w.x + w.width / 2.0, 108.0 + d / 2.0);
}

#[test]
fn bobber_hit_covers_disc_and_shadow_but_not_window_corners() {
    let d = BOBBER_DIAMETER;
    let circle = Rect::new(100.0, 100.0, d, d);
    let c = 100.0 + d / 2.0;
    // 圆盘本身
    assert!(bobber_hit(circle, Point { x: c, y: c }));
    assert!(bobber_hit(circle, Point { x: c + d / 2.0, y: c }));
    // 阴影外圈（圆盘外 4 pt 以内）也算浮标，不能被吞掉
    assert!(bobber_hit(
        circle,
        Point {
            x: c,
            y: c + d / 2.0 + 4.0
        }
    ));
    // 点击范围不超出窗口：窗口四角（完全透明、点击穿透）不算
    const { assert!(BOBBER_HIT_RADIUS <= BOBBER_WINDOW / 2.0) };
    let w = bobber_window_rect(circle);
    assert!(!bobber_hit(
        circle,
        Point {
            x: w.x + 1.0,
            y: w.y + 1.0
        }
    ));
    assert!(!bobber_hit(
        circle,
        Point {
            x: c + BOBBER_HIT_RADIUS + 0.5,
            y: c
        }
    ));
}

#[test]
fn bobber_move_away_threshold() {
    let circle = Rect::new(100.0, 100.0, BOBBER_DIAMETER, BOBBER_DIAMETER);
    let center = 100.0 + BOBBER_DIAMETER / 2.0;
    assert!(!moved_away(
        circle,
        Point {
            x: center + BOBBER_MOVE_AWAY_PX,
            y: center
        }
    ));
    assert!(moved_away(
        circle,
        Point {
            x: center + BOBBER_MOVE_AWAY_PX + 0.5,
            y: center
        }
    ));
    assert!(!moved_away(circle, Point { x: center, y: center }));
    assert_eq!(
        gesture::distance(Point { x: 0.0, y: 0.0 }, Point { x: 3.0, y: 4.0 }),
        5.0
    );
}

#[test]
fn long_press_state_machine() {
    let base = Instant::now();
    let at = |ms: u64| base + Duration::from_millis(ms);
    let threshold = LONG_PRESS.as_millis() as u64;

    let mut lp = LongPress::new();
    assert_eq!(lp.up(at(0)), LongPressUp::PassThrough, "没有被拦截的按下");

    lp.down(at(0));
    assert!(lp.is_active());
    assert_eq!(lp.remaining(at(100)), Some(Duration::from_millis(threshold - 100)));
    assert!(!lp.tick(at(100)));
    assert_eq!(lp.up(at(150)), LongPressUp::Replay, "400 ms 内抬起：重新注入");
    assert!(!lp.is_active());

    lp.down(at(1000));
    assert!(lp.tick(at(1000 + threshold)), "到点触发");
    assert!(!lp.tick(at(1000 + threshold + 50)), "只触发一次");
    assert_eq!(lp.up(at(1600)), LongPressUp::Swallow);

    lp.down(at(2000));
    assert_eq!(
        lp.up(at(2000 + threshold + 10)),
        LongPressUp::FireAndSwallow,
        "定时器晚到"
    );

    lp.down(at(3000));
    lp.reset();
    assert_eq!(lp.up(at(3100)), LongPressUp::PassThrough);
}

#[test]
fn mouse_actions_follow_spec() {
    let spec = MouseSpec {
        long_press: false,
        middle: Some(HotkeyAction::TranslateSelection),
        back: None,
        forward: Some(HotkeyAction::Ocr),
    };
    assert_eq!(
        gesture::mouse_action(&spec, MouseButtonKind::Middle),
        Some(HotkeyAction::TranslateSelection)
    );
    assert_eq!(
        gesture::mouse_action(&spec, MouseButtonKind::Back),
        None,
        "未启用的侧键放行"
    );
    assert_eq!(
        gesture::mouse_action(&spec, MouseButtonKind::Forward),
        Some(HotkeyAction::Ocr)
    );
}

// ---------------------------------------------------------------- suppress

#[test]
fn suppressor_window() {
    let now = Instant::now();
    let mut s = Suppressor::new();
    assert!(!s.should_ignore(5, now));
    s.arm(
        SuppressRange {
            from_change_id: 11,
            ttl: Duration::from_millis(300),
        },
        now,
    );
    assert!(s.is_armed(now));
    assert!(!s.should_ignore(10, now), "区间之前的变更照常上报");
    assert!(s.should_ignore(11, now + Duration::from_millis(50)), "目标应用复制");
    assert!(s.should_ignore(12, now + Duration::from_millis(250)), "Wisp 恢复");
    assert!(!s.should_ignore(13, now + Duration::from_millis(301)), "过期后照常上报");
    assert!(!s.is_armed(now + Duration::from_millis(301)));
    s.disarm();
    assert!(!s.should_ignore(11, now));
}

#[test]
fn suppressor_keeps_overlapping_ranges() {
    let now = Instant::now();
    let ms = Duration::from_millis;
    let fallback = SuppressRange {
        from_change_id: 20,
        ttl: ms(330),
    };
    let own_write = SuppressRange {
        from_change_id: 25,
        ttl: ms(100),
    };
    let mut s = Suppressor::new();
    s.arm(fallback, now);
    // 回退进行中，剪贴板服务又为自己的写入布防：不覆盖回退的区间
    s.arm(own_write, now + ms(20));
    assert!(s.should_ignore(21, now + ms(200)), "回退区间仍然有效");
    assert!(s.should_ignore(26, now + ms(60)));
    assert!(
        s.should_ignore(26, now + ms(200)),
        "后一个区间过期后，前一个区间仍覆盖它"
    );
    assert!(!s.should_ignore(19, now + ms(60)), "两个区间之前的变更照常上报");
    // 撤销只影响指定的区间
    assert!(s.retract(fallback));
    assert!(!s.retract(fallback), "已撤销");
    assert!(!s.should_ignore(21, now + ms(60)));
    assert!(s.should_ignore(25, now + ms(60)));
    assert!(!s.is_armed(now + ms(121)));
    // 过期区间在下一次布防时清理；区间数有上限，超出时丢弃最早的
    for i in 0..40 {
        s.arm(
            SuppressRange {
                from_change_id: 100 + i,
                ttl: ms(1000),
            },
            now + ms(200),
        );
    }
    assert!(s.should_ignore(139, now + ms(300)));
    assert!(s.is_armed(now + ms(300)));
    assert!(!s.is_armed(now + ms(1201)));
}

// ---------------------------------------------------------------- keymap

fn app(id: &str, alt: Option<&str>) -> AppRef {
    AppRef {
        id: id.to_owned(),
        alt_id: alt.map(str::to_owned),
        name: id.to_owned(),
        pid: None,
        path: None,
        window: None,
    }
}

fn send(choice: KeyChoice) -> String {
    match choice {
        KeyChoice::Send(c) => c.to_canonical(),
        KeyChoice::Skip => KEY_NONE.to_owned(),
    }
}

#[test]
fn keymap_builtin_tables_parse() {
    for os in [OsKind::Macos, OsKind::Windows, OsKind::Linux] {
        for entry in builtin(os) {
            assert_eq!(entry.os, os);
            for key in [&entry.copy, &entry.paste].into_iter().flatten() {
                assert!(
                    key == KEY_NONE || canonical(key).as_deref() == Some(key.as_str()),
                    "{key}"
                );
            }
        }
        assert!(Combo::parse(keymap::default_copy(os)).is_some());
        assert!(Combo::parse(keymap::default_paste(os)).is_some());
    }
    assert!(builtin(OsKind::Macos).is_empty(), "macOS 一律 ⌘C / ⌘V");
}

#[test]
fn keymap_linux_terminals() {
    let table = builtin(OsKind::Linux);
    let gnome = app("gnome-terminal", Some("gnome-terminal-server"));
    assert_eq!(
        send(copy_key(&table, OsKind::Linux, Some(&gnome))),
        "Control+Shift+KeyC"
    );
    assert_eq!(
        send(paste_key(&table, OsKind::Linux, Some(&gnome))),
        "Control+Shift+KeyV"
    );
    let konsole = app("konsole", Some("konsole"));
    assert_eq!(
        send(paste_key(&table, OsKind::Linux, Some(&konsole))),
        "Control+Shift+KeyV"
    );
    // xterm：不发复制键，粘贴用 Shift+Insert
    let xterm = app("xterm", Some("xterm"));
    assert_eq!(copy_key(&table, OsKind::Linux, Some(&xterm)), KeyChoice::Skip);
    assert_eq!(send(paste_key(&table, OsKind::Linux, Some(&xterm))), "Shift+Insert");
    let uxterm = app("UXTerm", None);
    assert_eq!(send(paste_key(&table, OsKind::Linux, Some(&uxterm))), "Shift+Insert");
    // 普通应用：平台默认
    let gedit = app("gedit", Some("gedit"));
    assert_eq!(send(copy_key(&table, OsKind::Linux, Some(&gedit))), "Control+KeyC");
    assert_eq!(send(paste_key(&table, OsKind::Linux, None)), "Control+KeyV");
}

#[test]
fn keymap_windows_matching_ignores_case_and_extension() {
    let table = builtin(OsKind::Windows);
    let wt = app("windowsterminal.exe", None);
    assert_eq!(send(copy_key(&table, OsKind::Windows, Some(&wt))), "Control+Shift+KeyC");
    assert_eq!(send(paste_key(&table, OsKind::Windows, Some(&wt))), "Control+KeyV");
    let mintty = app("MINTTY.EXE", None);
    assert_eq!(send(copy_key(&table, OsKind::Windows, Some(&mintty))), "Control+Insert");
    assert_eq!(send(paste_key(&table, OsKind::Windows, Some(&mintty))), "Shift+Insert");
    assert!(
        resolve_app(&table, OsKind::Windows, &app("conhost", None)).is_some(),
        ".exe 可省略"
    );
    assert!(
        resolve_app(&table, OsKind::Linux, &app("conhost.exe", None)).is_none(),
        "按平台过滤"
    );
}

#[test]
fn keymap_resolve_app_matches_like_key_lookup() {
    // Linux：条目按 WM_CLASS 的 instance 填写时，resolve_app 与 copy_key / paste_key 一样能命中
    let table = vec![AppKeymap {
        os: OsKind::Linux,
        app_id: "my-term".to_owned(),
        copy: None,
        paste: Some("Shift+Insert".to_owned()),
    }];
    let term = app("MyTerm", Some("my-term"));
    let hit = resolve_app(&table, OsKind::Linux, &term).expect("matched by instance");
    assert_eq!(hit.paste.as_deref(), Some("Shift+Insert"));
    assert_eq!(send(paste_key(&table, OsKind::Linux, Some(&term))), "Shift+Insert");
    // resolve_app 只找条目、不做逐字段合并：取按键必须用 copy_key（这里回落到平台默认）
    assert_eq!(hit.copy, None);
    assert_eq!(send(copy_key(&table, OsKind::Linux, Some(&term))), "Control+KeyC");
    // 旧 API 只比较 id：同一条目按 instance 填写时找不到（已标记 deprecated，引导到新 API）
    #[allow(deprecated)]
    let legacy = keymap::resolve(&table, OsKind::Linux, "MyTerm");
    assert!(legacy.is_none());
}

#[test]
fn keymap_user_entries_override_field_by_field() {
    let mut table = vec![AppKeymap {
        os: OsKind::Linux,
        app_id: "Konsole".to_owned(),
        copy: None,
        paste: Some("Shift+Insert".to_owned()),
    }];
    table.extend(builtin(OsKind::Linux));
    let konsole = app("konsole", None);
    // 用户只改了粘贴键：复制键仍按内置表
    assert_eq!(send(paste_key(&table, OsKind::Linux, Some(&konsole))), "Shift+Insert");
    assert_eq!(
        send(copy_key(&table, OsKind::Linux, Some(&konsole))),
        "Control+Shift+KeyC"
    );
    // macOS 上的用户条目
    let mac = vec![AppKeymap {
        os: OsKind::Macos,
        app_id: "com.googlecode.iterm2".to_owned(),
        copy: Some(KEY_NONE.to_owned()),
        paste: None,
    }];
    let iterm = app("com.googlecode.iTerm2", None);
    assert_eq!(copy_key(&mac, OsKind::Macos, Some(&iterm)), KeyChoice::Skip);
    assert_eq!(send(paste_key(&mac, OsKind::Macos, Some(&iterm))), "Super+KeyV");
    // 无法解析的用户串退回平台默认
    let broken = vec![AppKeymap {
        os: OsKind::Macos,
        app_id: "x".to_owned(),
        copy: Some("Ctrl+Nope".to_owned()),
        paste: None,
    }];
    assert_eq!(
        send(copy_key(&broken, OsKind::Macos, Some(&app("x", None)))),
        "Super+KeyC"
    );
}

// ---------------------------------------------------------------- 隐私标记

#[test]
fn macos_concealed_markers() {
    for t in MACOS_CONCEALED_TYPES {
        assert!(macos_is_concealed(["public.utf8-plain-text", t]), "{t}");
    }
    assert!(!macos_is_concealed(["public.utf8-plain-text", "public.html"]));
    assert!(!macos_is_concealed(std::iter::empty::<&str>()));
    assert!(has_wisp_marker(["public.utf8-plain-text", WISP_MARKER]));
    assert!(!has_wisp_marker(["public.utf8-plain-text"]));
}

#[test]
fn windows_private_formats() {
    for f in WINDOWS_EXCLUDE_FORMATS {
        assert!(windows_format_is_private(f, None), "{f}");
        assert!(
            windows_format_is_private(&f.to_ascii_uppercase(), Some(1)),
            "{f} 不区分大小写"
        );
    }
    for f in WINDOWS_ZERO_MEANS_PRIVATE_FORMATS {
        assert!(windows_format_is_private(f, Some(0)), "{f}=0");
        assert!(!windows_format_is_private(f, Some(1)), "{f}=1");
        assert!(windows_format_is_private(f, None), "{f} 读不到值时按隐私处理");
    }
    assert!(!windows_format_is_private("HTML Format", None));
}

#[test]
fn linux_password_hint() {
    assert!(linux_is_password_hint(LINUX_PASSWORD_HINT_MIME, b"secret"));
    assert!(linux_is_password_hint(LINUX_PASSWORD_HINT_MIME, b"secret\n"));
    assert!(linux_is_password_hint(LINUX_PASSWORD_HINT_MIME, b"secret\0"));
    assert!(!linux_is_password_hint(LINUX_PASSWORD_HINT_MIME, b"public"));
    assert!(!linux_is_password_hint("text/plain", b"secret"));
}

#[test]
fn wisp_input_tag_spells_wisp() {
    assert_eq!(&wisp_platform::common::WISP_INPUT_TAG.to_be_bytes(), b"WISP");
}
