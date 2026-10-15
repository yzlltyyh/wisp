//! 把捕获后端枚举出的显示器与 platform 给出的 `MonitorInfo` 配对。
//!
//! 分两遍：第一遍对所有 `MonitorInfo` 按稳定 id 配对（macOS = CGDirectDisplayID、Windows = `\\.\DISPLAYn`、
//! X11 = RandR 输出名）；第二遍只在剩下的后端显示器里，为 id 没对上的 `MonitorInfo` 按矩形配对（两边对同一块屏幕的
//! 命名偶有分歧，例如 platform 侧退回了别的命名方式）。先占满 id 配对，按矩形配对的一方就不会抢走别人按 id 本该
//! 配上的显示器（镜像显示器矩形相同时会发生）。每个后端显示器至多配对一次；配不上的 `MonitorInfo` 记日志后跳过——
//! 它没有冻结帧，也就不会为它创建 Overlay。

use wisp_types::MonitorInfo;

/// 物理矩形比较的容差（px）：两边分别取整时可能差 1
pub(crate) const RECT_TOLERANCE: f64 = 2.0;

/// 两个矩形（x, y, w, h）在容差内相等
pub(crate) fn rect_close(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64), tolerance: f64) -> bool {
    (a.0 - b.0).abs() <= tolerance
        && (a.1 - b.1).abs() <= tolerance
        && (a.2 - b.2).abs() <= tolerance
        && (a.3 - b.3).abs() <= tolerance
}

/// `MonitorInfo` 的物理矩形（Windows / X11 按物理矩形配对；macOS 按逻辑 frame 配对，不用它）
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) fn physical_rect(m: &MonitorInfo) -> (f64, f64, f64, f64) {
    (
        f64::from(m.physical_origin.x),
        f64::from(m.physical_origin.y),
        f64::from(m.physical_size.width),
        f64::from(m.physical_size.height),
    )
}

/// 配对结果按 `monitors` 的顺序给出；`same_id` / `same_rect` 判断一个后端显示器是否就是该 `MonitorInfo`
pub(crate) fn pair<D>(
    monitors: &[MonitorInfo],
    displays: Vec<D>,
    same_id: impl Fn(&MonitorInfo, &D) -> bool,
    same_rect: impl Fn(&MonitorInfo, &D) -> bool,
) -> Vec<(MonitorInfo, D)> {
    let mut taken = vec![false; displays.len()];
    let mut chosen: Vec<Option<usize>> = vec![None; monitors.len()];
    let mut claim = |chosen: &mut Option<usize>, matches: &dyn Fn(&D) -> bool| {
        let found = (0..displays.len()).find(|&i| !taken[i] && matches(&displays[i]));
        if let Some(i) = found {
            taken[i] = true;
            *chosen = Some(i);
        }
    };
    // 第一遍：稳定 id
    for (m, slot) in monitors.iter().zip(chosen.iter_mut()) {
        claim(slot, &|d| same_id(m, d));
    }
    // 第二遍：id 没对上的，只在剩下的显示器里按矩形找
    for (m, slot) in monitors.iter().zip(chosen.iter_mut()) {
        if slot.is_none() {
            claim(slot, &|d| same_rect(m, d));
        }
    }

    let mut pool: Vec<Option<D>> = displays.into_iter().map(Some).collect();
    let mut out = Vec::with_capacity(monitors.len());
    for (m, slot) in monitors.iter().zip(chosen) {
        match slot.and_then(|i| pool[i].take()) {
            Some(d) => out.push((m.clone(), d)),
            None => log::warn!("显示器 {}（{}）在捕获后端中不存在，跳过", m.id, m.name),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use wisp_types::{PhysPoint, PhysSize, Rect};

    use super::*;

    fn mon(id: &str, x: i32, w: u32) -> MonitorInfo {
        MonitorInfo {
            id: id.to_owned(),
            name: id.to_owned(),
            physical_origin: PhysPoint { x, y: 0 },
            physical_size: PhysSize { width: w, height: 1080 },
            scale_factor: 1.0,
            frame: Rect::new(f64::from(x), 0.0, f64::from(w), 1080.0),
            work_area: Rect::new(0.0, 0.0, f64::from(w), 1040.0),
            is_primary: x == 0,
        }
    }

    #[test]
    fn pairs_by_id_then_by_rect_each_display_once() {
        let monitors = [mon("A", 0, 1920), mon("B", 1920, 1920), mon("C", -1280, 1280)];
        // 后端：B 的 id 一致；A 的 id 不同但矩形一致；C 不存在
        let displays = vec![("B", 1920.0, 1920.0), ("other", 1.0, 1919.0)];
        let paired = pair(
            &monitors,
            displays,
            |m, d| m.id == d.0,
            |m, d| rect_close(physical_rect(m), (d.1, 0.0, d.2, 1080.0), RECT_TOLERANCE),
        );
        let ids: Vec<_> = paired.iter().map(|(m, d)| (m.id.as_str(), d.0)).collect();
        assert_eq!(ids, vec![("A", "other"), ("B", "B")]);
    }

    /// 镜像显示器矩形相同：排在前面、id 对不上的一方不能按矩形抢走后面按 id 本该配上的显示器
    #[test]
    fn id_matches_win_over_rect_matches_for_mirrored_displays() {
        let by_id = |m: &MonitorInfo, d: &(&str, f64, f64)| m.id == d.0;
        let by_rect = |m: &MonitorInfo, d: &(&str, f64, f64)| {
            rect_close(physical_rect(m), (d.1, 0.0, d.2, 1080.0), RECT_TOLERANCE)
        };
        // platform 退回了别的命名（"mirror"），矩形与 "B" 相同
        let monitors = [mon("mirror", 0, 1920), mon("B", 0, 1920)];

        // 后端只有一块：归按 id 配上的 B，镜像那一项跳过
        let paired = pair(&monitors, vec![("B", 0.0, 1920.0)], by_id, by_rect);
        let ids: Vec<_> = paired.iter().map(|(m, d)| (m.id.as_str(), d.0)).collect();
        assert_eq!(ids, vec![("B", "B")]);

        // 后端两块同矩形：B 按 id 拿到自己的，镜像项按矩形拿剩下那块；输出仍按 monitors 的顺序
        let paired = pair(&monitors, vec![("B", 0.0, 1920.0), ("C", 0.0, 1920.0)], by_id, by_rect);
        let ids: Vec<_> = paired.iter().map(|(m, d)| (m.id.as_str(), d.0)).collect();
        assert_eq!(ids, vec![("mirror", "C"), ("B", "B")]);
    }
}
