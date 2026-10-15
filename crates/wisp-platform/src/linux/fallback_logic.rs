//! 模拟复制回退的决策，X11；纯逻辑。
//!
//! `selection.rs` 与 `x11/clipboard.rs` 负责 X 往返，这里只回答「下一步做什么」：快照读哪些目标、按什么顺序、
//! 读到什么结果时放弃；无法备份时是否继续；抑制区间的 ttl 与何时撤销。本文件只依赖 std 与 `common/` 的公开常量，
//! `tests/smoke_linux.rs` 用 `#[path]` 引入，在任何平台都能单测。

use std::time::Duration;

use crate::common::secrets_markers::{LINUX_PASSWORD_HINT_MIME, WISP_MARKER};

/// X11 原子（u32）
pub type Atom = u32;

// ---------------------------------------------------------------- 快照

/// 快照里一个目标的类别：决定读取顺序，以及读不到时是否放弃整个快照
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TargetClass {
    /// 隐私标记、Wisp 标记：很小，决定恢复后是否仍按隐私内容处理
    Marker,
    /// 纯文本的各种编码
    Text,
    /// HTML / RTF
    Rich,
    /// 文件列表
    Files,
    /// 一种图片格式（有 image/png 时只取它）
    Image,
    /// 应用私有格式（LibreOffice 的嵌入对象、浏览器的内部格式……）：可能很贵，尽力而为
    Extra,
}

impl TargetClass {
    /// 必须拿到的类别：读取超时 / 过大时整个快照放弃。私有格式读不到只是少恢复一项
    pub fn is_core(self) -> bool {
        self != Self::Extra
    }
}

/// 快照最多读取的目标数（去重、去掉派生图片格式之后；正常的应用只有 5–15 个）
pub const MAX_SNAPSHOT_TARGETS: usize = 32;
/// 同时向 owner 发出的转换请求数。owner 通常按顺序处理请求，并行只是省掉往返；限制在途数量是为了预算用完时
/// 不在 owner 那里留下一长串排队的编码（例如每种图片格式都要把整张图重新编码一遍，源应用会因此卡顿）
pub const SNAPSHOT_IN_FLIGHT: usize = 4;

/// 不读取的目标：协议元目标，以及指向服务器资源、恢复时没有意义的旧式目标
const SKIPPED: &[&str] = &[
    "TARGETS",
    "MULTIPLE",
    "TIMESTAMP",
    "SAVE_TARGETS",
    "DELETE",
    "INSERT_SELECTION",
    "INSERT_PROPERTY",
    "LENGTH",
    "PIXMAP",
    "BITMAP",
    "DRAWABLE",
    "COLORMAP",
    "WINDOW",
];

/// 图片格式的别名（不以 image/ 开头，但同样是把整张图重新编码一遍）
const IMAGE_ALIASES: &[&str] = &["application/ico", "text/ico", "application/x-qt-image"];

fn is_image(name: &str) -> bool {
    name.starts_with("image/") || IMAGE_ALIASES.contains(&name)
}

/// 目标的类别；None 表示不读取
pub fn classify(name: &str) -> Option<TargetClass> {
    if name.is_empty() || SKIPPED.contains(&name) {
        return None;
    }
    if name == LINUX_PASSWORD_HINT_MIME || name == WISP_MARKER {
        return Some(TargetClass::Marker);
    }
    if matches!(name, "UTF8_STRING" | "STRING" | "TEXT" | "COMPOUND_TEXT") || name.starts_with("text/plain") {
        return Some(TargetClass::Text);
    }
    if matches!(name, "text/html" | "text/rtf" | "application/rtf" | "text/richtext") {
        return Some(TargetClass::Rich);
    }
    if matches!(
        name,
        "text/uri-list"
            | "x-special/gnome-copied-files"
            | "x-special/nautilus-clipboard"
            | "application/x-kde4-urilist"
            | "application/x-kde-cutselection"
    ) {
        return Some(TargetClass::Files);
    }
    if is_image(name) {
        return Some(TargetClass::Image);
    }
    Some(TargetClass::Extra)
}

/// 快照计划：去掉元目标与重复项；图片只保留一种（有 image/png 时取它，否则取 owner 列出的第一种——恢复时只
/// 提供这一种，读取方会自行转换）；按类别排序（同类保持 TARGETS 的顺序），最多 [`MAX_SNAPSHOT_TARGETS`] 个
pub fn plan_snapshot(targets: &[(Atom, &str)]) -> Vec<(Atom, TargetClass)> {
    let has_png = targets.iter().any(|(_, n)| *n == "image/png");
    let mut image_taken = false;
    let mut plan: Vec<(Atom, TargetClass)> = Vec::new();
    for &(atom, name) in targets {
        let Some(class) = classify(name) else { continue };
        if plan.iter().any(|(a, _)| *a == atom) {
            continue;
        }
        if class == TargetClass::Image {
            let wanted = if has_png { name == "image/png" } else { true };
            if !wanted || image_taken {
                continue;
            }
            image_taken = true;
        }
        plan.push((atom, class));
    }
    plan.sort_by_key(|&(_, class)| class);
    plan.truncate(MAX_SNAPSHOT_TARGETS);
    plan
}

/// 一个目标的读取结果（不含数据）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemOutcome {
    /// 读到了，且合计没有超过上限
    Data,
    /// 单项或合计超过上限
    TooLarge,
    Timeout,
    /// owner 声明了却拒绝转换
    Refused,
}

/// 读到一个结果之后的处理
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapStep {
    /// 收下
    Keep,
    /// 跳过这一项（恢复时不提供），继续
    Skip,
    /// 跳过这一项，并且不再发出新的请求（预算已经用完，剩下的只有私有格式）
    Stop,
    /// 整个快照放弃：过大（「剪贴板内容过大」）
    AbortTooLarge,
    /// 整个快照放弃：owner 没有及时应答
    AbortTimeout,
}

/// 读取结果 → 下一步（必须的类别过大 / 超时就放弃整个快照；私有格式读不到只是少恢复一项）
pub fn snapshot_step(class: TargetClass, outcome: ItemOutcome) -> SnapStep {
    match (outcome, class.is_core()) {
        (ItemOutcome::Data, _) => SnapStep::Keep,
        (ItemOutcome::Refused, _) => SnapStep::Skip,
        (ItemOutcome::TooLarge, true) => SnapStep::AbortTooLarge,
        (ItemOutcome::TooLarge, false) => SnapStep::Skip,
        (ItemOutcome::Timeout, true) => SnapStep::AbortTimeout,
        (ItemOutcome::Timeout, false) => SnapStep::Stop,
    }
}

/// 预算在发出下一批请求之前就用完了：剩下的还有必须的类别 → 放弃；只剩私有格式 → 停止，保留已读到的
pub fn budget_exhausted(remaining: &[(Atom, TargetClass)]) -> SnapStep {
    if remaining.iter().any(|(_, c)| c.is_core()) {
        SnapStep::AbortTimeout
    } else {
        SnapStep::Stop
    }
}

// ---------------------------------------------------------------- 回退流程

/// 抑制区间在等待上限之外的余量：目标应用稍晚才完成的复制也算 Wisp 自身的变更
pub const SUPPRESS_SLACK: Duration = Duration::from_millis(150);
/// 抑制区间的最短 ttl
pub const MIN_SUPPRESS_TTL: Duration = Duration::from_millis(300);

/// 复制等待上限（默认 150 ms，可配 50–500）
pub fn clamp_copy_wait(wait: Duration) -> Duration {
    wait.clamp(Duration::from_millis(50), Duration::from_millis(500))
}

/// 抑制区间的 ttl = max(300 ms, 等待上限 + 150 ms)
pub fn suppress_ttl(copy_wait: Duration) -> Duration {
    (copy_wait + SUPPRESS_SLACK).max(MIN_SUPPRESS_TTL)
}

/// 快照的结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backup {
    Taken,
    TooLarge,
    /// owner 没有在预算内应答，或不支持 TARGETS
    Timeout,
}

/// 无法备份时的决定
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proceed {
    /// 继续发复制键（有备份；或历史开启——历史里留着用户的剪贴板）
    Go,
    /// 历史关闭且剪贴板确实过大：返回 `clipboard_too_large`
    AbortTooLarge,
    /// 历史关闭且 owner 没有及时应答：按超时返回（重试多半就能成功）
    AbortTimeout,
}

/// 历史关闭时，无法备份就不能覆盖用户的剪贴板
pub fn proceed(backup: Backup, history_enabled: bool) -> Proceed {
    match (backup, history_enabled) {
        (Backup::Taken, _) | (_, true) => Proceed::Go,
        (Backup::TooLarge, false) => Proceed::AbortTooLarge,
        (Backup::Timeout, false) => Proceed::AbortTimeout,
    }
}

/// 复制键发出之后的结局
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyOutcome {
    /// 按键根本没有发出去
    SendFailed,
    /// 等待上限内序号没变（剪贴板没被动过，不需要恢复）
    NoChange,
    /// 序号变了：读文本、恢复
    Changed,
}

/// 是否撤销本次布防的抑制区间。只有确定不会再有变更时（按键没有发出去）才撤销；等待超时**不撤销**——慢应用
/// （LibreOffice、Java 程序）可能在等待结束之后才完成复制，那次变更仍是 Wisp 造成的，区间按 ttl 自然过期
/// （连续回退取词 20 次历史条数也不变；与 macOS 的实现一致）
pub fn retract_suppression(outcome: CopyOutcome) -> bool {
    outcome == CopyOutcome::SendFailed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(names: &[&str]) -> Vec<(Atom, TargetClass)> {
        let targets: Vec<(Atom, &str)> = names.iter().enumerate().map(|(i, n)| (i as Atom + 1, *n)).collect();
        plan_snapshot(&targets)
    }

    fn names<'a>(all: &[&'a str], plan: &[(Atom, TargetClass)]) -> Vec<&'a str> {
        plan.iter().map(|(a, _)| all[*a as usize - 1]).collect()
    }

    #[test]
    fn classification() {
        assert_eq!(classify("TARGETS"), None);
        assert_eq!(classify("SAVE_TARGETS"), None);
        assert_eq!(classify("PIXMAP"), None);
        assert_eq!(classify(""), None);
        assert_eq!(classify(LINUX_PASSWORD_HINT_MIME), Some(TargetClass::Marker));
        assert_eq!(classify(WISP_MARKER), Some(TargetClass::Marker));
        assert_eq!(classify("UTF8_STRING"), Some(TargetClass::Text));
        assert_eq!(classify("text/plain;charset=utf-8"), Some(TargetClass::Text));
        assert_eq!(classify("text/html"), Some(TargetClass::Rich));
        assert_eq!(classify("x-special/gnome-copied-files"), Some(TargetClass::Files));
        assert_eq!(classify("image/bmp"), Some(TargetClass::Image));
        assert_eq!(classify("application/x-qt-image"), Some(TargetClass::Image));
        assert_eq!(
            classify("application/x-openoffice-embed-source-xml;windows_formatname=\"Star Embed Source (XML)\""),
            Some(TargetClass::Extra)
        );
    }

    #[test]
    fn gtk_image_copy_keeps_only_png() {
        // GTK 复制图片时会声明十几种格式，每一种都要 owner 重新编码整张图
        let all = [
            "TIMESTAMP",
            "TARGETS",
            "MULTIPLE",
            "SAVE_TARGETS",
            "image/png",
            "image/bmp",
            "image/x-bmp",
            "image/x-MS-bmp",
            "image/x-icon",
            "image/x-ico",
            "image/x-win-bitmap",
            "image/vnd.microsoft.icon",
            "application/ico",
            "image/ico",
            "image/icon",
            "text/ico",
            "image/tiff",
            "image/jpeg",
        ];
        let p = plan(&all);
        assert_eq!(names(&all, &p), ["image/png"]);
    }

    #[test]
    fn without_png_the_first_image_format_is_kept() {
        let all = ["image/jpeg", "image/bmp", "UTF8_STRING"];
        assert_eq!(names(&all, &plan(&all)), ["UTF8_STRING", "image/jpeg"]);
    }

    #[test]
    fn plan_orders_by_class_and_dedups() {
        let all = [
            "chromium/x-web-custom-data",
            "text/html",
            "TARGETS",
            "UTF8_STRING",
            "x-kde-passwordManagerHint",
            "text/uri-list",
            "image/png",
            "STRING",
        ];
        let mut targets: Vec<(Atom, &str)> = all.iter().enumerate().map(|(i, n)| (i as Atom + 1, *n)).collect();
        // owner 重复列出同一个目标
        targets.push((4, "UTF8_STRING"));
        let p = plan_snapshot(&targets);
        assert_eq!(
            names(&all, &p),
            [
                "x-kde-passwordManagerHint",
                "UTF8_STRING",
                "STRING",
                "text/html",
                "text/uri-list",
                "image/png",
                "chromium/x-web-custom-data"
            ]
        );
    }

    #[test]
    fn plan_is_capped() {
        let owned: Vec<String> = (0..100).map(|i| format!("application/x-private-{i}")).collect();
        let targets: Vec<(Atom, &str)> = owned
            .iter()
            .enumerate()
            .map(|(i, n)| (i as Atom + 1, n.as_str()))
            .collect();
        assert_eq!(plan_snapshot(&targets).len(), MAX_SNAPSHOT_TARGETS);
    }

    #[test]
    fn snapshot_steps() {
        use ItemOutcome::*;
        use TargetClass::*;
        assert_eq!(snapshot_step(Text, Data), SnapStep::Keep);
        assert_eq!(snapshot_step(Extra, Data), SnapStep::Keep);
        assert_eq!(snapshot_step(Rich, Refused), SnapStep::Skip);
        // 必须的类别：过大 / 超时都放弃整个快照（12 MB 的图片 → 过大）
        assert_eq!(snapshot_step(Image, TooLarge), SnapStep::AbortTooLarge);
        assert_eq!(snapshot_step(Text, Timeout), SnapStep::AbortTimeout);
        assert_eq!(snapshot_step(Marker, Timeout), SnapStep::AbortTimeout);
        // 私有格式：过大跳过；超时不再发新请求，保留已读到的
        assert_eq!(snapshot_step(Extra, TooLarge), SnapStep::Skip);
        assert_eq!(snapshot_step(Extra, Timeout), SnapStep::Stop);
        assert_eq!(budget_exhausted(&[(1, Extra), (2, Extra)]), SnapStep::Stop);
        assert_eq!(budget_exhausted(&[(1, Image), (2, Extra)]), SnapStep::AbortTimeout);
        assert_eq!(budget_exhausted(&[]), SnapStep::Stop);
    }

    #[test]
    fn copy_wait_and_suppression_ttl() {
        assert_eq!(clamp_copy_wait(Duration::from_millis(10)), Duration::from_millis(50));
        assert_eq!(clamp_copy_wait(Duration::from_millis(900)), Duration::from_millis(500));
        assert_eq!(suppress_ttl(Duration::from_millis(50)), Duration::from_millis(300));
        assert_eq!(suppress_ttl(Duration::from_millis(150)), Duration::from_millis(300));
        assert_eq!(suppress_ttl(Duration::from_millis(500)), Duration::from_millis(650));
    }

    #[test]
    fn oversized_clipboard_fallback_branches() {
        assert_eq!(proceed(Backup::Taken, false), Proceed::Go);
        assert_eq!(proceed(Backup::TooLarge, true), Proceed::Go);
        assert_eq!(proceed(Backup::Timeout, true), Proceed::Go);
        assert_eq!(proceed(Backup::TooLarge, false), Proceed::AbortTooLarge);
        assert_eq!(proceed(Backup::Timeout, false), Proceed::AbortTimeout);
    }

    #[test]
    fn suppression_is_only_retracted_when_no_key_was_sent() {
        assert!(retract_suppression(CopyOutcome::SendFailed));
        // 等待超时：慢应用稍后完成的复制仍要被抑制
        assert!(!retract_suppression(CopyOutcome::NoChange));
        // 变更已经发生：区间由 ttl 结束（恢复那次由 owner = Wisp 识别）
        assert!(!retract_suppression(CopyOutcome::Changed));
    }
}
