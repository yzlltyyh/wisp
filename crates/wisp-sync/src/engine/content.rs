//! 剪贴板 / 识别历史的存在判定与保留策略（应用与压缩共用）。
//!
//! 剪贴板条目 `h` 存在，当且仅当：`c/h` 存在且不是墓碑；`c/h` 胜过 `@clear/all`；并且条目已置顶，
//! 或 `c/h` 与 `p/h` 中较新的一个胜过 `@clear/unpinned`（清空之后的置顶 / 取消置顶算作活动）。
//! 识别记录 `o/id` 存在，当且仅当它不是墓碑且胜过 `@clear`。「胜过」一律按 `(t, device_id)` 比较，
//! 与寄存器的 LWW 一致，所以各设备对同一组寄存器得出同样的结论。

use std::collections::HashSet;

use super::RetentionPolicy;
use crate::hlc::Hlc;
use crate::model::{ClearValue, Domain, outranks};
use crate::replica::{ContentView, Row};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// `a` 胜过清空寄存器 `clear`（没有清空、或清空是墓碑时总是成立）
pub(super) fn beats(a: &Row, clear: Option<&Row>) -> bool {
    match clear {
        Some(c) if !c.tomb => outranks(a.t, &a.dev, c.t, &c.dev),
        _ => true,
    }
}

/// 置顶寄存器的有效值：早于「清空全部」的置顶已随条目一起被清掉，不再算数（之后再次复制的条目是未置顶的）
pub(super) fn pinned(p: Option<&Row>, view: &ContentView) -> Option<bool> {
    let p = p.filter(|p| !p.tomb)?;
    if !beats(p, view.clear_all.as_ref()) {
        return Some(false);
    }
    p.json().and_then(|v| v.as_bool())
}

/// 剪贴板条目是否存在（见模块说明）
pub(super) fn clip_alive(c: &Row, p: Option<&Row>, view: &ContentView) -> bool {
    if c.tomb || !beats(c, view.clear_all.as_ref()) {
        return false;
    }
    if pinned(p, view) == Some(true) {
        return true;
    }
    let latest = match p {
        Some(p) if outranks(p.t, &p.dev, c.t, &c.dev) => p,
        _ => c,
    };
    beats(latest, view.clear_unpinned.as_ref())
}

/// 识别记录是否存在
pub(super) fn ocr_alive(o: &Row, view: &ContentView) -> bool {
    !o.tomb && beats(o, view.clear_all.as_ref())
}

/// 清空寄存器的签名：变化说明需要重新评估全部条目
pub(super) fn clear_sig(view: &ContentView) -> String {
    let part = |r: Option<&Row>| r.map_or_else(String::new, |r| format!("{}:{}:{}", r.t.0, r.dev, u8::from(r.tomb)));
    format!(
        "{}|{}",
        part(view.clear_all.as_ref()),
        part(view.clear_unpinned.as_ref())
    )
}

/// 清空寄存器值里的墙钟毫秒（只用于本机没有寄存器的条目）；值无法解析时取它的 t 的物理时间
pub(super) fn clear_wall_ms(row: &Row) -> i64 {
    row.json()
        .and_then(|v| serde_json::from_value::<ClearValue>(v).ok())
        .map_or_else(|| i64::try_from(row.t.wall_ms()).unwrap_or(i64::MAX), |c| c.wall_ms)
}

/// 条目自身的时间：值里有就用值里的，否则用 t 的物理时间
pub(super) fn item_ms(row: &Row) -> i64 {
    row.item_ms
        .unwrap_or_else(|| i64::try_from(row.t.wall_ms()).unwrap_or(i64::MAX))
}

/// 按（已同步的）保留策略，存活条目中应从服务器历史里裁掉的 key：
/// 剪贴板未置顶且超出条数上限（按 last_used_at 排名）或早于天数上限的 `c/<h>`（连同 `p/<h>`）；
/// 识别记录超出条数上限的 `o/<id>`。置顶条目不裁
pub(super) fn retention_drops(
    clip: &ContentView,
    ocr: &ContentView,
    policy: &RetentionPolicy,
    now_ms: i64,
) -> HashSet<(Domain, String)> {
    let mut out = HashSet::new();

    let mut live: Vec<(&String, &Row)> = clip
        .items
        .iter()
        .filter(|(h, c)| {
            let p = clip.pins.get(*h);
            clip_alive(c, p, clip) && pinned(p, clip) != Some(true)
        })
        .collect();
    live.sort_by(|a, b| item_ms(b.1).cmp(&item_ms(a.1)).then_with(|| a.0.cmp(b.0)));
    let max_items = usize::try_from(policy.clip_max_items).unwrap_or(usize::MAX);
    let cutoff = policy
        .clip_max_age_days
        .map(|days| now_ms.saturating_sub(i64::from(days).saturating_mul(DAY_MS)));
    for (i, (h, c)) in live.iter().enumerate() {
        if i >= max_items || cutoff.is_some_and(|cut| item_ms(c) < cut) {
            out.insert((Domain::Clip, format!("c/{h}")));
            out.insert((Domain::Clip, format!("p/{h}")));
        }
    }

    let mut live: Vec<(&String, &Row)> = ocr.items.iter().filter(|(_, o)| ocr_alive(o, ocr)).collect();
    live.sort_by(|a, b| item_ms(b.1).cmp(&item_ms(a.1)).then_with(|| a.0.cmp(b.0)));
    let limit = usize::try_from(policy.ocr_history_limit).unwrap_or(usize::MAX);
    for (id, _) in live.iter().skip(limit) {
        out.insert((Domain::Ocr, format!("o/{id}")));
    }
    out
}

/// 保留策略的签名：策略变了要重新检查；按天数清理时每天换一次，让随时间过期的条目也会被发现
pub(super) fn retention_sig(policy: &RetentionPolicy, now_ms: i64) -> String {
    let day = policy.clip_max_age_days.map_or(0, |_| now_ms.div_euclid(DAY_MS));
    format!(
        "{}:{:?}:{}:{day}",
        policy.clip_max_items, policy.clip_max_age_days, policy.ocr_history_limit
    )
}

/// 以条目自身时间发布的 t
pub(super) fn item_hlc(ms: Option<i64>) -> Hlc {
    Hlc::from_wall_ms_i64(ms.unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Domain;
    use serde_json::Value;

    fn row(k: &str, t: u64, dev: &str, v: Option<Value>) -> Row {
        Row {
            d: Domain::Clip,
            k: k.into(),
            dev: dev.into(),
            t: Hlc(t),
            tomb: v.is_none(),
            v: v.map(|v| v.to_string()),
            b: Vec::new(),
            aux: None,
            item_ms: None,
            src_hi: None,
            applied_t: None,
            evicted_t: None,
            missing_t: None,
            gone: false,
            pending: false,
            deferred: false,
        }
    }

    #[test]
    fn clear_and_pin_predicate() {
        let c = row("c/x", 10, "a", Some(Value::Null));
        let mut view = ContentView::default();
        assert!(clip_alive(&c, None, &view));
        view.clear_unpinned = Some(row("@clear/unpinned", 20, "b", Some(serde_json::json!({"wall_ms": 0}))));
        assert!(!clip_alive(&c, None, &view));
        // 清空之前置顶的条目保留
        let pin_before = row("p/x", 15, "a", Some(Value::Bool(true)));
        assert!(clip_alive(&c, Some(&pin_before), &view));
        // 清空之后取消置顶仍算活动
        let unpin_after = row("p/x", 30, "a", Some(Value::Bool(false)));
        assert!(clip_alive(&c, Some(&unpin_after), &view));
        // 清空之前取消置顶：被清空
        let unpin_before = row("p/x", 15, "a", Some(Value::Bool(false)));
        assert!(!clip_alive(&c, Some(&unpin_before), &view));
        // 清空全部连置顶的也清
        view.clear_all = Some(row("@clear/all", 25, "b", Some(serde_json::json!({"wall_ms": 0}))));
        assert!(!clip_alive(&c, Some(&pin_before), &view));
        assert_ne!(clear_sig(&view), clear_sig(&ContentView::default()));
    }

    #[test]
    fn retention_ranks_live_unpinned_items() {
        let mut clip = ContentView::default();
        for i in 0..5u64 {
            let mut c = row(&format!("c/{i}"), 10 + i, "a", Some(Value::Null));
            c.item_ms = Some(i64::try_from(i).unwrap() * 1000);
            clip.items.insert(i.to_string(), c);
        }
        clip.pins
            .insert("0".into(), row("p/0", 50, "a", Some(Value::Bool(true))));
        let policy = RetentionPolicy {
            clip_max_items: 2,
            clip_max_age_days: None,
            ocr_history_limit: 10,
        };
        let drops = retention_drops(&clip, &ContentView::default(), &policy, 10_000);
        // 4、3 保留；2、1 超出；0 置顶不裁
        assert!(drops.contains(&(Domain::Clip, "c/2".into())));
        assert!(drops.contains(&(Domain::Clip, "c/1".into())));
        assert!(!drops.contains(&(Domain::Clip, "c/0".into())));
        assert!(!drops.contains(&(Domain::Clip, "c/4".into())));
    }
}
