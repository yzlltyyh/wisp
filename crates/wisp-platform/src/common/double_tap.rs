//! 双击修饰键状态机（纯逻辑，可在任何平台上单测）。
//!
//! 规则：目标修饰键必须是唯一按下的那个；按住 ≤200 ms；两次按下间隔 ≤ 阈值；在第二次抬起时触发；
//! 中间有其他键或鼠标则作废；触发后复位（三连击不会触发两次）。
//!
//! 「唯一按下」由平台层在构造 [`TapInput::TargetDown`] 之前判定（macOS 见 [`mac`]）；状态机只关心时序。

use std::time::{Duration, Instant};

/// 单次按住的上限
pub const MAX_HOLD: Duration = Duration::from_millis(200);
/// 默认的两次按下最大间隔
pub const DEFAULT_INTERVAL: Duration = Duration::from_millis(300);

/// 状态机输入
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapInput {
    /// 目标修饰键按下（且是唯一按下的修饰键）
    TargetDown(Instant),
    /// 目标修饰键抬起
    TargetUp(Instant),
    /// 其他键或其他修饰键
    OtherKey,
    /// 鼠标按下
    MouseDown,
}

/// 状态机内部阶段
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    /// 第一次按下，尚未抬起
    FirstDown {
        t1: Instant,
    },
    /// 第一次已完整点按（按住 ≤ MAX_HOLD），等待第二次按下
    FirstUp {
        t1: Instant,
    },
    /// 第二次按下，尚未抬起
    SecondDown {
        t1: Instant,
        t2: Instant,
    },
}

/// 双击修饰键状态机
#[derive(Debug, Clone)]
pub struct DoubleTapMachine {
    interval: Duration,
    phase: Phase,
}

impl DoubleTapMachine {
    /// `interval`：两次按下的最大间隔（200 / 300 / 400 ms）
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            phase: Phase::Idle,
        }
    }

    /// 当前阈值
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// 喂入一个事件，返回是否触发
    pub fn feed(&mut self, input: TapInput) -> bool {
        let (next, fired) = match (self.phase, input) {
            (_, TapInput::OtherKey | TapInput::MouseDown) => (Phase::Idle, false),
            (Phase::FirstUp { t1 }, TapInput::TargetDown(t2)) => {
                if t2.saturating_duration_since(t1) <= self.interval {
                    (Phase::SecondDown { t1, t2 }, false)
                } else {
                    // 间隔太长：这一次按下作为新序列的第一次
                    (Phase::FirstDown { t1: t2 }, false)
                }
            }
            // 空闲，或按下 / 抬起事件丢失后的异常状态：从这一次重新开始
            (_, TapInput::TargetDown(t)) => (Phase::FirstDown { t1: t }, false),
            (Phase::FirstDown { t1 }, TapInput::TargetUp(t)) => {
                if t.saturating_duration_since(t1) <= MAX_HOLD {
                    (Phase::FirstUp { t1 }, false)
                } else {
                    (Phase::Idle, false)
                }
            }
            (Phase::SecondDown { t2, .. }, TapInput::TargetUp(t)) => {
                // 触发后复位：第三次按下只会开启新序列
                (Phase::Idle, t.saturating_duration_since(t2) <= MAX_HOLD)
            }
            (Phase::Idle | Phase::FirstUp { .. }, TapInput::TargetUp(_)) => (Phase::Idle, false),
        };
        self.phase = next;
        fired
    }

    /// 复位
    pub fn reset(&mut self) {
        self.phase = Phase::Idle;
    }

    /// 进行中的序列的第一次按下时刻（空闲时为 None）。平台层据此判断两次按下之间是否发生过鼠标按下
    /// （macOS 用 `CGEventSourceSecondsSinceLastEventType`）
    pub fn pending_since(&self) -> Option<Instant> {
        match self.phase {
            Phase::Idle => None,
            Phase::FirstDown { t1 } | Phase::FirstUp { t1 } | Phase::SecondDown { t1, .. } => Some(t1),
        }
    }
}

/// macOS `flagsChanged` 事件到 [`TapInput`] 的分类（纯函数，便于在任何平台上测试）。
///
/// 按 flags 判定按下 / 抬起（不依赖 keyCode 的连续性），并先去掉 CapsLock / NumericPad / Help / NonCoalesced 位，
/// 因此 Caps Lock 开启时也能触发。
pub mod mac {
    use wisp_types::ModifierKey;

    /// `kCGEventFlagMaskAlphaShift`（Caps Lock）
    pub const FLAG_CAPS_LOCK: u64 = 0x0001_0000;
    /// `kCGEventFlagMaskShift`
    pub const FLAG_SHIFT: u64 = 0x0002_0000;
    /// `kCGEventFlagMaskControl`
    pub const FLAG_CONTROL: u64 = 0x0004_0000;
    /// `kCGEventFlagMaskAlternate`
    pub const FLAG_ALTERNATE: u64 = 0x0008_0000;
    /// `kCGEventFlagMaskCommand`
    pub const FLAG_COMMAND: u64 = 0x0010_0000;
    /// `kCGEventFlagMaskNumericPad`
    pub const FLAG_NUMERIC_PAD: u64 = 0x0020_0000;
    /// `kCGEventFlagMaskHelp`
    pub const FLAG_HELP: u64 = 0x0040_0000;
    /// `kCGEventFlagMaskSecondaryFn`
    pub const FLAG_FN: u64 = 0x0080_0000;
    /// `kCGEventFlagMaskNonCoalesced`
    pub const FLAG_NON_COALESCED: u64 = 0x0000_0100;

    /// `NX_DEVICELCTLKEYMASK` 等区分左右的设备位（IOLLEvent.h）
    pub const DEV_LCTL: u64 = 0x0000_0001;
    pub const DEV_LSHIFT: u64 = 0x0000_0002;
    pub const DEV_RSHIFT: u64 = 0x0000_0004;
    pub const DEV_LCMD: u64 = 0x0000_0008;
    pub const DEV_RCMD: u64 = 0x0000_0010;
    pub const DEV_LALT: u64 = 0x0000_0020;
    pub const DEV_RALT: u64 = 0x0000_0040;
    pub const DEV_RCTL: u64 = 0x0000_2000;

    /// 所有修饰键相关的位（判定「唯一按下」时考察的范围；CapsLock 等已排除在外）
    const MODIFIER_BITS: u64 = FLAG_SHIFT
        | FLAG_CONTROL
        | FLAG_ALTERNATE
        | FLAG_COMMAND
        | FLAG_FN
        | DEV_LCTL
        | DEV_LSHIFT
        | DEV_RSHIFT
        | DEV_LCMD
        | DEV_RCMD
        | DEV_LALT
        | DEV_RALT
        | DEV_RCTL;

    /// macOS 修饰键的虚拟键码
    pub const KC_RIGHT_CMD: u16 = 0x36;
    pub const KC_LEFT_CMD: u16 = 0x37;
    pub const KC_LEFT_SHIFT: u16 = 0x38;
    pub const KC_CAPS_LOCK: u16 = 0x39;
    pub const KC_LEFT_ALT: u16 = 0x3A;
    pub const KC_LEFT_CTRL: u16 = 0x3B;
    pub const KC_RIGHT_SHIFT: u16 = 0x3C;
    pub const KC_RIGHT_ALT: u16 = 0x3D;
    pub const KC_RIGHT_CTRL: u16 = 0x3E;
    pub const KC_FN: u16 = 0x3F;

    /// 一次 flagsChanged 的分类结果
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum FlagsEvent {
        /// 目标修饰键按下，且没有其他修饰键按着
        TargetDown,
        /// 目标修饰键抬起
        TargetUp,
        /// 其他修饰键变化，或按下目标键时还按着别的修饰键：作废当前序列
        Other,
    }

    /// 目标修饰键的（虚拟键码, 设备位, 通用位）
    pub const fn key_bits(key: ModifierKey) -> (u16, u64, u64) {
        match key {
            ModifierKey::LeftAlt => (KC_LEFT_ALT, DEV_LALT, FLAG_ALTERNATE),
            ModifierKey::RightAlt => (KC_RIGHT_ALT, DEV_RALT, FLAG_ALTERNATE),
            ModifierKey::LeftCtrl => (KC_LEFT_CTRL, DEV_LCTL, FLAG_CONTROL),
            ModifierKey::RightCtrl => (KC_RIGHT_CTRL, DEV_RCTL, FLAG_CONTROL),
            ModifierKey::LeftCmd => (KC_LEFT_CMD, DEV_LCMD, FLAG_COMMAND),
            ModifierKey::RightCmd => (KC_RIGHT_CMD, DEV_RCMD, FLAG_COMMAND),
        }
    }

    /// 分类一次 flagsChanged：`keycode` 是事件的 `kCGKeyboardEventKeycode`，`flags` 是事件 flags
    pub fn classify_flags_changed(keycode: u16, flags: u64, target: ModifierKey) -> FlagsEvent {
        let (target_kc, dev_bit, class_bit) = key_bits(target);
        if keycode != target_kc {
            return FlagsEvent::Other;
        }
        let relevant = flags & MODIFIER_BITS;
        // 有的键盘 / 远程桌面不报设备位：此时只能退回通用位判断按下与否
        let has_device_bits = relevant & (MODIFIER_BITS & 0xFFFF) != 0;
        let down = if has_device_bits {
            relevant & dev_bit != 0
        } else {
            relevant & class_bit != 0
        };
        if !down {
            return FlagsEvent::TargetUp;
        }
        // 「唯一按下」：去掉目标键自己的设备位与通用位后不能再有任何修饰位
        if relevant & !(dev_bit | class_bit) != 0 {
            return FlagsEvent::Other;
        }
        FlagsEvent::TargetDown
    }

    /// 某个虚拟键码是否是修饰键（含 CapsLock、Fn）
    pub const fn is_modifier_keycode(keycode: u16) -> bool {
        matches!(keycode, KC_RIGHT_CMD..=KC_FN)
    }
}
