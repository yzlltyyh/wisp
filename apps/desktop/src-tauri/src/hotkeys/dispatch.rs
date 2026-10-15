//! 快捷键回调与扩展触发的派发判定（纯状态机，单测覆盖）：按住 / 自动重复过滤、150 ms 去抖、录制与测试期间的抑制、
//! 测试按钮的回显、全局 Esc，以及录制 / 测试看门狗的判定。`HotkeyService` 只负责把判定结果接到插件、总线与事件上
//! （回调在主线程上、插件持有自己的锁时调用：这里只做内存里的判定，不阻塞）。

use std::time::{Duration, Instant};

use wisp_types::HotkeyTarget;

use super::press::{Debouncer, PressFilter};

/// 测试按钮的上限（前端 10 s 超时；Rust 侧兜底，设置窗口崩溃时不会一直吞掉快捷键）
pub const TEST_LIMIT: Duration = Duration::from_secs(15);
/// 录制的上限（录制器失焦即取消；这里只防设置窗口异常退出后快捷键一直处于暂停）
pub const RECORD_LIMIT: Duration = Duration::from_secs(600);

/// 一次快捷键回调的处理结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyStep {
    /// 执行动作（投递到编排器）
    Fire(HotkeyTarget),
    /// 测试按钮：按下的正是被测的键，回显 `hotkey:test_received`（不执行动作）
    Echo(HotkeyTarget),
    /// 全局 Esc（Esc guard）：请求隐藏面板
    Esc,
    /// 不处理：抬起、按住 / 自动重复、去抖、录制中、测试期间的其他键、不认识的 id
    Ignore,
}

/// 看门狗一轮的判定
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Watchdog {
    /// 既不在录制也不在测试：看门狗可以退出
    pub idle: bool,
    /// 结束测试（设置与引导窗口都不在了，或超过 [`TEST_LIMIT`]）
    pub end_test: bool,
    /// 结束录制、恢复全部快捷键（窗口都不在了，或超过 [`RECORD_LIMIT`]）
    pub end_recording: bool,
}

/// 派发状态
#[derive(Debug, Default)]
pub struct Dispatch {
    filter: PressFilter<u32>,
    debounce: Debouncer<HotkeyTarget>,
    /// 录制开始的时刻（录制中不执行任何动作）
    pub recording_since: Option<Instant>,
    /// 测试中的目标与开始时刻
    pub test: Option<(HotkeyTarget, Instant)>,
}

impl Dispatch {
    /// 全局快捷键插件的一次回调。`target` 是该 id 的路由（没有时为 None），`is_esc` 表示它是 Esc guard 注册的 Esc
    pub fn on_key(
        &mut self,
        id: u32,
        target: Option<HotkeyTarget>,
        is_esc: bool,
        pressed: bool,
        now: Instant,
    ) -> KeyStep {
        if !pressed {
            self.filter.release(id, now);
            return KeyStep::Ignore;
        }
        if !self.filter.press(id, now) {
            return KeyStep::Ignore;
        }
        match target {
            Some(target) => {
                if let Some((testing, _)) = &self.test {
                    // 测试期间不执行动作；按下的正是被测的键时回显
                    return if *testing == target {
                        KeyStep::Echo(target)
                    } else {
                        KeyStep::Ignore
                    };
                }
                if self.recording_since.is_some() || !self.debounce.allow(&target, now) {
                    return KeyStep::Ignore;
                }
                KeyStep::Fire(target)
            }
            None if is_esc => KeyStep::Esc,
            None => KeyStep::Ignore,
        }
    }

    /// 扩展触发（浮标点击、双击修饰键、鼠标键）是否放行：录制 / 测试期间不执行，与快捷键共用 150 ms 去抖
    pub fn on_trigger(&mut self, target: &HotkeyTarget, now: Instant) -> bool {
        if self.recording_since.is_some() || self.test.is_some() {
            return false;
        }
        self.debounce.allow(target, now)
    }

    /// 快捷键全部重新注册之后：旧的按住状态不再有意义
    pub fn reset_keys(&mut self) {
        self.filter.clear();
    }

    /// 单个键刚被注销或重新注册（Esc guard 注册 / 注销 Esc）：忘掉它的按住状态。注销时如果键还没松开，插件不会再送来
    /// Released，不清掉的话 [`STALE_HOLD`](super::press::STALE_HOLD) 内重新注册后的第一次按下会被当成按住重复
    pub fn forget_key(&mut self, id: u32) {
        self.filter.forget(id);
    }

    /// 看门狗的一轮：`window_open` = 设置或引导窗口还在
    pub fn watchdog(&self, window_open: bool, now: Instant) -> Watchdog {
        let over = |since: Instant, limit: Duration| now.saturating_duration_since(since) > limit;
        Watchdog {
            idle: self.recording_since.is_none() && self.test.is_none(),
            end_test: self
                .test
                .as_ref()
                .is_some_and(|(_, t)| !window_open || over(*t, TEST_LIMIT)),
            end_recording: self
                .recording_since
                .is_some_and(|t| !window_open || over(t, RECORD_LIMIT)),
        }
    }
}

#[cfg(test)]
mod tests {
    use wisp_types::HotkeyAction;

    use super::super::press::DEBOUNCE;
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn act(a: HotkeyAction) -> HotkeyTarget {
        HotkeyTarget::Action { action: a }
    }

    /// 按下触发、抬起不触发；按住与 X11 的 Release + Press 自动重复只触发一次
    #[test]
    fn press_fires_once_per_physical_press() {
        let t0 = Instant::now();
        let mut d = Dispatch::default();
        let ocr = act(HotkeyAction::Ocr);
        assert_eq!(
            d.on_key(1, Some(ocr.clone()), false, true, t0),
            KeyStep::Fire(ocr.clone())
        );
        assert_eq!(
            d.on_key(1, Some(ocr.clone()), false, true, t0 + ms(300)),
            KeyStep::Ignore
        );
        assert_eq!(
            d.on_key(1, Some(ocr.clone()), false, false, t0 + ms(330)),
            KeyStep::Ignore
        );
        assert_eq!(
            d.on_key(1, Some(ocr.clone()), false, true, t0 + ms(331)),
            KeyStep::Ignore
        );
        assert_eq!(
            d.on_key(1, Some(ocr.clone()), false, false, t0 + ms(400)),
            KeyStep::Ignore
        );
        assert_eq!(
            d.on_key(1, Some(ocr.clone()), false, true, t0 + ms(800)),
            KeyStep::Fire(ocr)
        );
        // 没有路由的 id（例如刚注销的键的迟到事件）
        assert_eq!(d.on_key(9, None, false, true, t0 + ms(900)), KeyStep::Ignore);
    }

    /// 同一目标 150 ms 内去抖：不同的键绑定到同一目标、或快捷键与扩展触发交替，都只触发一次
    #[test]
    fn debounce_is_per_target_across_sources() {
        let t0 = Instant::now();
        let mut d = Dispatch::default();
        let tr = act(HotkeyAction::TranslateSelection);
        assert_eq!(
            d.on_key(1, Some(tr.clone()), false, true, t0),
            KeyStep::Fire(tr.clone())
        );
        assert!(!d.on_trigger(&tr, t0 + ms(40)), "双击修饰键紧跟着快捷键");
        assert!(d.on_trigger(&act(HotkeyAction::Ocr), t0 + ms(41)), "别的目标不受影响");
        assert!(d.on_trigger(&tr, t0 + DEBOUNCE + ms(1)));
    }

    /// 测试按钮：只回显被测的键，任何键都不执行动作；扩展触发也不执行
    #[test]
    fn test_mode_echoes_and_suppresses() {
        let t0 = Instant::now();
        let mut d = Dispatch::default();
        let tr = act(HotkeyAction::TranslateSelection);
        let ocr = act(HotkeyAction::Ocr);
        d.test = Some((tr.clone(), t0));
        assert_eq!(
            d.on_key(1, Some(tr.clone()), false, true, t0),
            KeyStep::Echo(tr.clone())
        );
        assert_eq!(d.on_key(2, Some(ocr.clone()), false, true, t0), KeyStep::Ignore);
        assert!(!d.on_trigger(&tr, t0 + ms(500)));
        // 测试期间回显不占用去抖：结束测试后立刻按，照常执行
        d.on_key(1, Some(tr.clone()), false, false, t0 + ms(50));
        d.test = None;
        assert_eq!(
            d.on_key(1, Some(tr.clone()), false, true, t0 + ms(100)),
            KeyStep::Fire(tr)
        );
    }

    /// 录制期间：快捷键与扩展触发都不执行（插件里残留的事件也被吞掉）
    #[test]
    fn recording_suppresses_everything() {
        let t0 = Instant::now();
        let mut d = Dispatch {
            recording_since: Some(t0),
            ..Dispatch::default()
        };
        let tr = act(HotkeyAction::TranslateSelection);
        assert_eq!(d.on_key(1, Some(tr.clone()), false, true, t0), KeyStep::Ignore);
        assert!(!d.on_trigger(&tr, t0));
        d.recording_since = None;
        d.reset_keys();
        assert_eq!(
            d.on_key(1, Some(tr.clone()), false, true, t0 + ms(10)),
            KeyStep::Fire(tr)
        );
    }

    /// Esc guard 注册的 Esc：没有动作路由，按下即请求隐藏面板；同样过滤按住
    #[test]
    fn escape_branch() {
        let t0 = Instant::now();
        let mut d = Dispatch::default();
        assert_eq!(d.on_key(7, None, true, true, t0), KeyStep::Esc);
        assert_eq!(d.on_key(7, None, true, true, t0 + ms(200)), KeyStep::Ignore, "按住 Esc");
        d.on_key(7, None, true, false, t0 + ms(300));
        assert_eq!(d.on_key(7, None, true, true, t0 + ms(600)), KeyStep::Esc);
    }

    /// 按 Esc 关闭面板时 Esc 还没松开就被注销（插件不再送 Released），面板随即再次以非 key 状态出现、Esc 重新注册：
    /// 下一次按下必须照常关闭面板，不能被当成按住重复
    #[test]
    fn escape_rearmed_after_unregister_without_release() {
        let t0 = Instant::now();
        let mut d = Dispatch::default();
        assert_eq!(d.on_key(7, None, true, true, t0), KeyStep::Esc);
        // 面板隐藏 → esc_guard(false) 注销 Esc；之后 esc_guard(true) 重新注册。两次都会 forget_key
        d.forget_key(7);
        d.forget_key(7);
        assert_eq!(d.on_key(7, None, true, true, t0 + ms(400)), KeyStep::Esc);
        // 不 forget 时的对照：Released 丢失后 STALE_HOLD 内的按下被忽略
        let mut stale = Dispatch::default();
        assert_eq!(stale.on_key(7, None, true, true, t0), KeyStep::Esc);
        assert_eq!(stale.on_key(7, None, true, true, t0 + ms(400)), KeyStep::Ignore);
    }

    /// 看门狗：窗口都关了或超时时结束录制 / 测试；什么都没有时退出
    #[test]
    fn watchdog_decisions() {
        let t0 = Instant::now();
        let mut d = Dispatch::default();
        assert_eq!(
            d.watchdog(true, t0),
            Watchdog {
                idle: true,
                ..Watchdog::default()
            }
        );
        d.recording_since = Some(t0);
        d.test = Some((act(HotkeyAction::Ocr), t0));
        assert_eq!(d.watchdog(true, t0 + ms(2000)), Watchdog::default(), "窗口还在、没超时");
        let closed = d.watchdog(false, t0 + ms(2000));
        assert!(closed.end_test && closed.end_recording && !closed.idle);
        let late = d.watchdog(true, t0 + TEST_LIMIT + ms(1));
        assert!(late.end_test && !late.end_recording);
        assert!(d.watchdog(true, t0 + RECORD_LIMIT + ms(1)).end_recording);
    }
}
