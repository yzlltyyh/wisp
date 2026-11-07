//! Wisp 自己的某个窗口此刻是否持有键盘焦点。
//!
//! 连续粘贴拦下的粘贴键是会话级的：用户在悬浮剪贴板的搜索框、设置窗口里按 ⌘V 时也会被拦下。这时的粘贴不是给目标
//! 应用的，队列只重放、不前进。粘贴键路径上只做一次原子读：标志由所有窗口（面板、悬浮剪贴板、设置、引导、固定窗口、
//! 截图覆盖层）的获焦 / 失焦 / 销毁事件维护（`app::bootstrap` 统一转交，各窗口不必各写一份），不向系统查询、不用
//! AX / UIA。连续粘贴的全局 Esc 同样看这个标志：持有状态变化时由 `app::bootstrap` 通知快捷键服务，键盘在 Wisp 窗口里时
//! 不注册，Esc 照常交给那个窗口。
//!
//! 记下持有焦点的窗口 label，而不是计数：焦点在两个 Wisp 窗口之间转移时，新窗口的获焦与旧窗口的失焦谁先到不确定；
//! 只有当前持有者的失焦（或销毁）才清除标志，失焦事件漏掉一次也不会让计数永远偏高。

use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;

/// 键盘焦点的持有者
pub struct KeyFocus {
    held: AtomicBool,
    owner: Mutex<Option<String>>,
}

impl KeyFocus {
    pub const fn new() -> Self {
        Self {
            held: AtomicBool::new(false),
            owner: Mutex::new(None),
        }
    }

    /// 某个窗口获焦 / 失焦。返回「Wisp 的某个窗口持有键盘焦点」是否因此变化（焦点在两个 Wisp 窗口之间转移不算）
    pub fn on_focus(&self, label: &str, focused: bool) -> bool {
        let mut owner = self.owner.lock();
        if focused {
            *owner = Some(label.to_owned());
        } else if owner.as_deref() == Some(label) {
            *owner = None;
        }
        let held = owner.is_some();
        self.held.swap(held, Ordering::AcqRel) != held
    }

    /// 某个窗口销毁（销毁时不一定先收到失焦）。返回值同 [`Self::on_focus`]
    pub fn on_destroyed(&self, label: &str) -> bool {
        self.on_focus(label, false)
    }

    /// Wisp 的某个窗口持有键盘焦点（原子读）
    pub fn held(&self) -> bool {
        self.held.load(Ordering::Acquire)
    }
}

impl Default for KeyFocus {
    fn default() -> Self {
        Self::new()
    }
}

/// 进程内唯一的一份（窗口事件在 `app::bootstrap` 里转交，剪贴板服务在粘贴键路径上读取）
pub static KEY_FOCUS: KeyFocus = KeyFocus::new();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_owner_losing_focus_clears_the_flag() {
        let f = KeyFocus::new();
        assert!(!f.held());
        assert!(f.on_focus("panel", true), "进入 Wisp 窗口：变化");
        assert!(f.held());
        // 焦点从面板转到悬浮窗，新窗口的获焦先到：持有状态不变
        assert!(!f.on_focus("clipfloat", true));
        assert!(!f.on_focus("panel", false));
        assert!(f.held(), "旧窗口迟到的失焦不清除新窗口的焦点");
        assert!(f.on_focus("clipfloat", false), "离开 Wisp 窗口：变化");
        assert!(!f.held());
        // 获焦的窗口直接销毁、没有失焦事件
        f.on_focus("pinned-1", true);
        assert!(f.on_destroyed("pinned-1"));
        assert!(!f.held());
        // 别的窗口销毁不影响当前持有者
        f.on_focus("settings", true);
        assert!(!f.on_destroyed("pinned-2"));
        assert!(f.held());
    }
}
