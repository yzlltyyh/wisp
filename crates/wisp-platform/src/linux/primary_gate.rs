//! 浮标判定的「选区是否是这次手势产生的」（X11；纯逻辑，只依赖 std）。
//!
//! X11 的 PRIMARY 在取消高亮后仍然保留，而且几乎总有 owner，「PRIMARY 非空」不能说明用户刚刚选中了文字——拖动
//! 窗口标题栏、滚动条、拖放文件、在画图应用里画一笔都会满足它。GTK、Qt、Firefox、Chromium、VTE、xterm 在每次
//! 产生新选区时都会重新 SetSelectionOwner（GTK 4 在按下时先清空选区，同样会重新占有），XFixes 会把每一次占有连同
//! 服务器时间戳通知给 Wisp。因此判定改为：**PRIMARY 必须在本次手势第一次左键按下之后（双击 / 三击取第一次）
//! 被重新占有过**，否则就是旧选区。
//!
//! 时间都是 X 服务器的毫秒时间戳（32 位，约 49.7 天回绕一次），比较时按回绕处理。

/// `t` 是否不早于 `reference`（按 32 位回绕比较：相差不到半个周期视为在之后）
pub fn at_or_after(t: u32, reference: u32) -> bool {
    t.wrapping_sub(reference) < 0x8000_0000
}

/// 判定结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// 没有在跟踪（没有安装浮标钩子、缺 XFIXES，或还没有任何手势）：只能按「PRIMARY 非空」判断
    Unknown,
    /// 本次手势开始后 PRIMARY 被重新占有过：是新选区
    Fresh,
    /// 本次手势开始后 PRIMARY 没有变过（目前为止）：是旧选区
    Stale,
}

/// 跟踪状态（hub 线程写，取词线程读）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrimaryGate {
    tracking: bool,
    /// 最近一次由其他应用占有 PRIMARY 的服务器时间
    last_claim: Option<u32>,
    /// 当前手势第一次左键按下的服务器时间
    gesture_start: Option<u32>,
}

impl PrimaryGate {
    /// 开始跟踪（订阅 XFixes PRIMARY 时调用）：之前的记录一律作废
    #[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
    pub fn start(&mut self) {
        *self = Self {
            tracking: true,
            ..Self::default()
        };
    }

    /// 停止跟踪
    #[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
    pub fn stop(&mut self) {
        *self = Self::default();
    }

    /// 是否在跟踪
    #[cfg(test)]
    pub fn tracking(&self) -> bool {
        self.tracking
    }

    /// 左键按下。`first_of_gesture`：不是双击 / 三击的后续按下
    #[cfg_attr(not(feature = "mouse-hook"), allow(dead_code))]
    pub fn on_left_press(&mut self, time: u32, first_of_gesture: bool) {
        if self.tracking && (first_of_gesture || self.gesture_start.is_none()) {
            self.gesture_start = Some(time);
        }
    }

    /// 其他应用占有了 PRIMARY（Wisp 自己占有时不调用）
    pub fn on_claim(&mut self, time: u32) {
        if self.tracking {
            self.last_claim = Some(time);
        }
    }

    /// 当前判定
    pub fn verdict(&self) -> Verdict {
        let Some(start) = self.gesture_start.filter(|_| self.tracking) else {
            return Verdict::Unknown;
        };
        match self.last_claim {
            Some(claim) if at_or_after(claim, start) => Verdict::Fresh,
            _ => Verdict::Stale,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracking() -> PrimaryGate {
        let mut g = PrimaryGate::default();
        g.start();
        g
    }

    #[test]
    fn untracked_gate_is_unknown() {
        let mut g = PrimaryGate::default();
        g.on_left_press(100, true);
        g.on_claim(120);
        assert_eq!(g.verdict(), Verdict::Unknown, "没有订阅 XFixes 时不做判定");
        let g = tracking();
        assert_eq!(g.verdict(), Verdict::Unknown, "还没有任何手势");
    }

    #[test]
    fn drag_without_selection_is_stale() {
        // 拖动窗口 / 滚动条 / 画一笔：PRIMARY 仍是很早以前的选区
        let mut g = tracking();
        g.on_claim(500);
        g.on_left_press(10_000, true);
        assert_eq!(g.verdict(), Verdict::Stale);
    }

    #[test]
    fn drag_select_is_fresh_even_if_claimed_after_release() {
        let mut g = tracking();
        g.on_claim(500);
        g.on_left_press(10_000, true);
        assert_eq!(g.verdict(), Verdict::Stale, "抬起时应用可能还没占有 PRIMARY");
        g.on_claim(10_180);
        assert_eq!(g.verdict(), Verdict::Fresh);
        // 同一毫秒内占有也算
        g.on_left_press(20_000, true);
        g.on_claim(20_000);
        assert_eq!(g.verdict(), Verdict::Fresh);
    }

    #[test]
    fn multi_click_uses_first_press() {
        // 双击选词：选区在第二次按下时产生，晚于第一次按下
        let mut g = tracking();
        g.on_left_press(1_000, true);
        g.on_claim(1_150);
        g.on_left_press(1_200, false);
        assert_eq!(g.verdict(), Verdict::Fresh, "第二次按下不应重置手势起点");
        // 新的单击开始新手势：之前的占有不再算数
        g.on_left_press(5_000, true);
        assert_eq!(g.verdict(), Verdict::Stale);
    }

    #[test]
    fn server_time_wraparound() {
        assert!(at_or_after(5, u32::MAX - 5));
        assert!(!at_or_after(u32::MAX - 5, 5));
        assert!(at_or_after(7, 7));
        let mut g = tracking();
        g.on_left_press(u32::MAX - 10, true);
        g.on_claim(3);
        assert_eq!(g.verdict(), Verdict::Fresh);
    }

    #[test]
    fn restart_forgets_history() {
        let mut g = tracking();
        g.on_left_press(100, true);
        g.on_claim(150);
        g.start();
        assert_eq!(g.verdict(), Verdict::Unknown);
        g.stop();
        assert!(!g.tracking());
        g.on_left_press(200, true);
        assert_eq!(g.verdict(), Verdict::Unknown);
    }
}
