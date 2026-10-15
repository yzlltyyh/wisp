//! 触发去抖（只响应 Pressed，同一动作 150 ms 内去抖；按住不放不重复触发），纯状态机，单测覆盖各平台的
//! 事件形状：
//! - macOS（Carbon）与 Windows（`MOD_NOREPEAT`）按住时只有一次 Pressed，抬起时一次 Released（Windows 由轮询线程
//!   在约 50 ms 内补发）；
//! - X11 的自动重复是成对的 Release + Press（间隔 ≈ 0），需要按「抬起后极短时间内又按下」识别为重复；
//! - Released 可能丢失（例如按住期间注销了快捷键）：按住标记在最后一次 Pressed 之后 [`STALE_HOLD`] 失效，不会让
//!   快捷键永久失灵；确知某个键已被注销（重新注册、Esc guard 注销 / 注册 Esc）时直接 [`PressFilter::forget`]。

use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

/// 同一目标两次触发的最小间隔
pub const DEBOUNCE: Duration = Duration::from_millis(150);
/// 抬起后这么短时间内又按下：视为 X11 的自动重复
pub const REPEAT_GAP: Duration = Duration::from_millis(40);
/// 按住标记的有效期（自最后一次 Pressed 算起）
pub const STALE_HOLD: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, Copy, Default)]
struct KeyState {
    held: bool,
    last_press: Option<Instant>,
    last_release: Option<Instant>,
}

/// 物理按键层面的过滤：识别按住与自动重复（按快捷键 id）
#[derive(Debug)]
pub struct PressFilter<K> {
    keys: HashMap<K, KeyState>,
}

impl<K> Default for PressFilter<K> {
    fn default() -> Self {
        Self { keys: HashMap::new() }
    }
}

impl<K: Eq + Hash + Copy> PressFilter<K> {
    /// 收到 Pressed：返回 true 表示这是一次新的按下（应当触发）
    pub fn press(&mut self, key: K, now: Instant) -> bool {
        let st = self.keys.entry(key).or_default();
        let since_press = st.last_press.map(|t| now.saturating_duration_since(t));
        let since_release = st.last_release.map(|t| now.saturating_duration_since(t));
        st.last_press = Some(now);
        if st.held && since_press.is_some_and(|d| d < STALE_HOLD) {
            // 按住期间的重复 Pressed
            return false;
        }
        st.held = true;
        // X11：Release + Press 紧挨着到达
        since_release.is_none_or(|d| d >= REPEAT_GAP)
    }

    /// 收到 Released
    pub fn release(&mut self, key: K, now: Instant) {
        let st = self.keys.entry(key).or_default();
        st.held = false;
        st.last_release = Some(now);
    }

    /// 忘掉一个键的按住 / 抬起记录（它刚被注销或重新注册：注销之后插件不会再送来它的 Released）
    pub fn forget(&mut self, key: K) {
        self.keys.remove(&key);
    }

    /// 清空（快捷键全部重新注册之后，旧的按住状态不再有意义）
    pub fn clear(&mut self) {
        self.keys.clear();
    }
}

/// 目标层面的去抖：同一目标（动作 / 风格）在 [`DEBOUNCE`] 内只触发一次，不论来自快捷键还是扩展触发
#[derive(Debug)]
pub struct Debouncer<T> {
    last: HashMap<T, Instant>,
}

impl<T> Default for Debouncer<T> {
    fn default() -> Self {
        Self { last: HashMap::new() }
    }
}

impl<T: Eq + Hash + Clone> Debouncer<T> {
    /// 是否放行
    pub fn allow(&mut self, target: &T, now: Instant) -> bool {
        if let Some(t) = self.last.get(target)
            && now.saturating_duration_since(*t) < DEBOUNCE
        {
            return false;
        }
        // 表很小（≤ 动作数 + 风格数），不需要清理
        self.last.insert(target.clone(), now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn press_release_cycles_fire_each_time() {
        let t0 = Instant::now();
        let mut f = PressFilter::default();
        assert!(f.press(1, t0));
        f.release(1, t0 + ms(80));
        assert!(f.press(1, t0 + ms(400)));
        f.release(1, t0 + ms(480));
        // 另一个键互不影响
        assert!(f.press(2, t0 + ms(481)));
    }

    /// macOS / Windows：按住时没有重复事件；即使有（例如平台补发），按住期间也不再触发
    #[test]
    fn holding_does_not_repeat() {
        let t0 = Instant::now();
        let mut f = PressFilter::default();
        assert!(f.press(1, t0));
        for i in 1..30 {
            assert!(!f.press(1, t0 + ms(500 + i * 33)), "第 {i} 次重复");
        }
        f.release(1, t0 + ms(1600));
        assert!(f.press(1, t0 + ms(1900)));
    }

    /// X11：自动重复是成对的 Release + Press
    #[test]
    fn x11_autorepeat_pairs_are_ignored() {
        let t0 = Instant::now();
        let mut f = PressFilter::default();
        assert!(f.press(7, t0));
        let mut t = t0 + ms(500);
        for _ in 0..20 {
            f.release(7, t);
            assert!(!f.press(7, t + ms(1)));
            t += ms(33);
        }
        f.release(7, t);
        assert!(f.press(7, t + ms(200)), "真正抬起后再按要触发");
    }

    /// Released 丢失时按住标记会过期
    #[test]
    fn lost_release_expires() {
        let t0 = Instant::now();
        let mut f = PressFilter::default();
        assert!(f.press(1, t0));
        assert!(!f.press(1, t0 + ms(300)));
        assert!(f.press(1, t0 + ms(300) + STALE_HOLD + ms(1)));
        f.clear();
        assert!(f.press(1, t0 + ms(5000)));
    }

    /// 键被注销时忘掉它的按住状态：没等到 Released 也不会把下一次按下当成重复
    #[test]
    fn forget_drops_the_hold() {
        let t0 = Instant::now();
        let mut f = PressFilter::default();
        assert!(f.press(1, t0));
        assert!(f.press(2, t0));
        f.forget(1);
        assert!(f.press(1, t0 + ms(200)), "Released 丢失，但键已被注销后重新注册");
        assert!(!f.press(2, t0 + ms(200)), "其他键的按住状态不受影响");
    }

    /// 150 ms 去抖（双击修饰键的三连击不会触发两次由平台层的状态机保证，这里只管同一目标的连发）
    #[test]
    fn debounce_per_target() {
        let t0 = Instant::now();
        let mut d = Debouncer::default();
        assert!(d.allow(&"a", t0));
        assert!(!d.allow(&"a", t0 + ms(149)));
        assert!(d.allow(&"b", t0 + ms(10)));
        assert!(d.allow(&"a", t0 + ms(150)));
    }
}
