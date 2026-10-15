//! 性能埋点：从触发到面板显示、首 token 的各阶段耗时；开发者模式下 emit `panel:latency`。
//!
//! **用法**：触发时 `let t = perf.begin(origin)`；取词完成 `perf.set_method(t, "ax")` +
//! `perf.mark(t, Selection)`；`panel_prepare` 拿到 session 后 `perf.bind_session(t, session)` + `mark(t, Prepare)`；
//! 之后 WindowManager 只知道 session，用 [`PerfMarks::mark_session`] 记 Ready / Shown / ReadyTimeout；首个 Delta 记
//! FirstToken；流程结束（Done / Error / 面板关闭）时 `finish(t, app)`。同一阶段只记第一次。
//!
//! `finish` 写一行 `perf …` 日志（`scripts/perf/latency-report.mjs` 解析它算 p50 / p95），格式固定为
//! `perf trace=<id> origin=<origin> session=<id|-> method=<m|-> selection_ms=<n|-> prepare_ms=<n> ready_ms=<n>
//! shown_ms=<n> first_token_ms=<n|-> ready_timeout=<bool> overlay_ms=<n|-> selection=<result|-> window_ms=<n|-> key_ms=<n|->
//! page=<warm|recycled|cold|->`（毫秒均从 `begin` 起算；`overlay_ms` / `selection`：截图流程第一块屏 Overlay
//! 上屏的耗时、取词的结果分类；`window_ms` / `key_ms` / `page`：ready 之后原生窗口
//! 真正上屏（orderFront / ShowWindow 返回）、面板成为 key 窗口的时刻，以及本次唤出用的面板页面是热页面、回收出来的新页面
//! 还是卸载后重建的（冷唤出））；开发者模式（或 debug 构建）下再 emit_to panel `panel:latency`
//! （`LatencyMarks`）。字段只追加不改名，`scripts/perf/latency-report.mjs` 按 `key=value` 解析，旧解析器忽略新字段。
//!
//! **取词统计**：每次取词由编排器调用 [`PerfMarks::record_selection`]，写一行不含正文的 INFO 摘要
//! `selection app=<bundle id | exe> method=<m|-> result=<分类> elapsed_ms=<n>`，并在内存里按应用累计 method 分布、结果分布
//! 与平均耗时（最多 [`MAX_SELECTION_APPS`] 个应用）；诊断导出写成 `selection-stats.json`。
//!
//! 内存有界：只保留最近 [`MAX_TRACES`] 条（未 finish 的旧追踪被挤掉，不会泄漏）；不启动任何定时器。
//! 冒烟模式（`app::smoke`）通过 [`PerfMarks::subscribe`] / [`PerfMarks::first_after`] 等待并读取结果。
//!
//! **粘贴埋点**（记录从选中条目到发出粘贴键的时间，剪贴板服务调用）：剪贴板面板里选中条目时记下
//! `Instant`，模拟粘贴的按键事件发出（`SendInput` / `CGEventPost` / XTest）后调用 [`PerfMarks::paste`]，写一行
//! `perf paste method=<m> select_to_post_ms=<n>`（同样由 `latency-report.mjs` 解析）。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tauri::{Emitter, Manager};
use tokio::sync::watch;
use wisp_types::{LatencyMarks, event};

use crate::bus::TriggerOrigin;
use crate::state::AppState;

/// 保留的追踪条数上限
pub const MAX_TRACES: usize = 256;
/// 取词统计最多记多少个应用（满了之后新应用归入 `other`）
pub const MAX_SELECTION_APPS: usize = 128;

/// 某个应用的取词统计
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct AppSelectionStats {
    pub count: u32,
    /// method → 次数（成功取到时的取词路径；失败记 `-`）
    pub methods: std::collections::BTreeMap<String, u32>,
    /// 结果分类 → 次数
    pub results: std::collections::BTreeMap<String, u32>,
    pub total_ms: u64,
    pub avg_ms: u32,
}

/// 取词摘要的日志行（不含正文）
pub fn selection_log_line(app: &str, method: Option<&str>, result: &str, elapsed_ms: u32) -> String {
    format!(
        "selection app={} method={} result={result} elapsed_ms={elapsed_ms}",
        sanitize_token(app),
        method.map_or_else(|| "-".to_owned(), sanitize_token)
    )
}

/// 日志里的标识符：去掉空白与等号（应用 id 不会有，防御性处理），空串记 `-`
fn sanitize_token(s: &str) -> String {
    let t: String = s.chars().filter(|c| !c.is_whitespace() && *c != '=').take(96).collect();
    if t.is_empty() { "-".to_owned() } else { t }
}

/// 埋点阶段
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PerfStage {
    Selection,
    Prepare,
    Ready,
    /// 前端第一个 opacity > 0 的帧（panel_painted）
    Shown,
    FirstToken,
    ReadyTimeout,
    /// 截图：第一块屏的 Overlay 上屏（macOS 原生冻结帧的 `overlay_show` 返回 / Windows、X11 的 `overlay_ready` 显示完成；
    /// 目标 ≤ 150 ms）
    OverlayShown,
    /// `panel_ready` 之后原生窗口真正上屏（macOS orderFront / Windows ShowWindow 返回）。
    /// ready→shown 的 Rust 侧拆分（输入视图目标 ≤ 100 ms），由窗口层记
    WindowShown,
    /// 面板成为 key 窗口（输入 / 剪贴板视图；macOS 推迟一轮事件循环之后、Windows 去掉 NOACTIVATE 之后）
    KeyGained,
}

impl PerfStage {
    const COUNT: usize = 9;

    const fn index(self) -> usize {
        match self {
            Self::Selection => 0,
            Self::Prepare => 1,
            Self::Ready => 2,
            Self::Shown => 3,
            Self::FirstToken => 4,
            Self::ReadyTimeout => 5,
            Self::OverlayShown => 6,
            Self::WindowShown => 7,
            Self::KeyGained => 8,
        }
    }
}

/// 本次唤出用的面板页面（perf 行的 `page=`）。由窗口层在 prepare 时按面板 WebView 的状态记
/// （[`PerfMarks::set_page_for_session`]）；没有记时为 `-`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PanelPage {
    /// 用过的热页面
    Warm,
    /// 回收出来的新页面（JIT 冷）
    Recycled,
    /// 空闲卸载之后重建的页面（冷唤出，目标含原生首帧掩盖 ≤ 150 ms）
    Cold,
}

impl PanelPage {
    /// perf 行里的取值
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Warm => "warm",
            Self::Recycled => "recycled",
            Self::Cold => "cold",
        }
    }
}

/// 一次追踪的快照（毫秒均从 `begin` 起算）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceSnapshot {
    pub id: u64,
    pub origin: TriggerOrigin,
    pub session_id: Option<u64>,
    pub method: Option<String>,
    pub selection_ms: Option<u32>,
    pub prepare_ms: Option<u32>,
    pub ready_ms: Option<u32>,
    pub shown_ms: Option<u32>,
    pub first_token_ms: Option<u32>,
    pub ready_timeout: bool,
    /// 截图流程：触发 → 第一块屏的 Overlay 上屏
    pub overlay_ms: Option<u32>,
    /// 取词的结果分类：ok / empty / timeout / permission / elevated / unsupported / error
    pub selection_result: Option<&'static str>,
    /// ready 之后原生窗口上屏 / 面板成为 key 的时刻，与本次用的面板页面
    pub window_ms: Option<u32>,
    pub key_ms: Option<u32>,
    pub page: Option<PanelPage>,
    pub finished: bool,
    /// 拍快照时距 `begin` 的毫秒数
    pub elapsed_now_ms: u32,
}

impl TraceSnapshot {
    /// 到现在为止的耗时（取词失败时的耗时用它，平台层的错误不带耗时）
    pub fn elapsed_ms(&self) -> u32 {
        self.elapsed_now_ms
    }

    /// prepare → shown 的耗时（两者都有时）
    pub fn prepare_to_shown_ms(&self) -> Option<u32> {
        Some(self.shown_ms?.saturating_sub(self.prepare_ms?))
    }

    /// 转成 `panel:latency` 载荷
    pub fn to_marks(&self) -> LatencyMarks {
        LatencyMarks {
            session_id: self.session_id.unwrap_or(0),
            origin: self.origin.as_str().to_owned(),
            method: self.method.clone(),
            selection_ms: self.selection_ms,
            prepare_ms: self.prepare_ms.unwrap_or(0),
            ready_ms: self.ready_ms.unwrap_or(0),
            shown_ms: self.shown_ms.unwrap_or(0),
            first_token_ms: self.first_token_ms,
            ready_timeout: self.ready_timeout,
        }
    }

    /// `perf …` 日志行（不含任何正文）
    pub fn log_line(&self) -> String {
        fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
            v.map_or_else(|| "-".to_owned(), |v| v.to_string())
        }
        format!(
            "perf trace={} origin={} session={} method={} selection_ms={} prepare_ms={} ready_ms={} shown_ms={} first_token_ms={} ready_timeout={} overlay_ms={} selection={} window_ms={} key_ms={} page={}",
            self.id,
            self.origin.as_str(),
            opt(self.session_id),
            self.method.as_deref().unwrap_or("-"),
            opt(self.selection_ms),
            opt(self.prepare_ms),
            opt(self.ready_ms),
            opt(self.shown_ms),
            opt(self.first_token_ms),
            self.ready_timeout,
            opt(self.overlay_ms),
            self.selection_result.unwrap_or("-"),
            opt(self.window_ms),
            opt(self.key_ms),
            self.page.map_or("-", PanelPage::as_str),
        )
    }
}

#[derive(Debug)]
struct Trace {
    id: u64,
    origin: TriggerOrigin,
    t0: Instant,
    session_id: Option<u64>,
    method: Option<String>,
    selection_result: Option<&'static str>,
    page: Option<PanelPage>,
    stages: [Option<Duration>; PerfStage::COUNT],
    finished: bool,
}

impl Trace {
    fn snapshot(&self) -> TraceSnapshot {
        let ms = |s: PerfStage| self.stages[s.index()].map(duration_ms);
        TraceSnapshot {
            id: self.id,
            origin: self.origin,
            session_id: self.session_id,
            method: self.method.clone(),
            selection_ms: ms(PerfStage::Selection),
            prepare_ms: ms(PerfStage::Prepare),
            ready_ms: ms(PerfStage::Ready),
            shown_ms: ms(PerfStage::Shown),
            first_token_ms: ms(PerfStage::FirstToken),
            ready_timeout: self.stages[PerfStage::ReadyTimeout.index()].is_some(),
            overlay_ms: ms(PerfStage::OverlayShown),
            selection_result: self.selection_result,
            window_ms: ms(PerfStage::WindowShown),
            key_ms: ms(PerfStage::KeyGained),
            page: self.page,
            finished: self.finished,
            elapsed_now_ms: duration_ms(self.t0.elapsed()),
        }
    }
}

/// 毫秒（u32 饱和）
pub fn duration_ms(d: Duration) -> u32 {
    u32::try_from(d.as_millis()).unwrap_or(u32::MAX)
}

/// 粘贴埋点的日志行（固定格式）：`perf paste method=<m> select_to_post_ms=<n>`。`method` 只保留小写字母、数字、`_`、`-`，
/// 最长 24 字符；清洗后为空时记 `-`
pub fn paste_log_line(method: &str, select_to_post_ms: u32) -> String {
    let method: String = method
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        .take(24)
        .collect();
    let method = if method.is_empty() { "-" } else { method.as_str() };
    format!("perf paste method={method} select_to_post_ms={select_to_post_ms}")
}

/// 埋点表
#[derive(Debug)]
pub struct PerfMarks {
    next: AtomicU64,
    traces: Mutex<VecDeque<Trace>>,
    /// 启动各阶段的耗时（从进程启动起算）
    startup: Mutex<Vec<(&'static str, u32)>>,
    /// 每次有新的标记都递增（冒烟模式等待用；没有接收者时 `send_modify` 照常写入）
    changed: watch::Sender<u64>,
    /// 按应用累计的取词统计
    selection: Mutex<std::collections::BTreeMap<String, AppSelectionStats>>,
}

impl Default for PerfMarks {
    fn default() -> Self {
        Self::new()
    }
}

impl PerfMarks {
    /// 空表
    pub fn new() -> Self {
        Self {
            next: AtomicU64::new(0),
            traces: Mutex::new(VecDeque::with_capacity(16)),
            startup: Mutex::new(Vec::new()),
            changed: watch::channel(0).0,
            selection: Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    fn bump(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }

    fn with_trace<R>(&self, trace: u64, f: impl FnOnce(&mut Trace) -> R) -> Option<R> {
        let mut traces = self.traces.lock();
        traces.iter_mut().rev().find(|t| t.id == trace).map(f)
    }

    /// 开始一次追踪，返回 trace id
    pub fn begin(&self, origin: TriggerOrigin) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        {
            let mut traces = self.traces.lock();
            if traces.len() >= MAX_TRACES {
                traces.pop_front();
            }
            traces.push_back(Trace {
                id,
                origin,
                t0: Instant::now(),
                session_id: None,
                method: None,
                selection_result: None,
                page: None,
                stages: [None; PerfStage::COUNT],
                finished: false,
            });
        }
        log::trace!("perf begin trace={id} origin={}", origin.as_str());
        self.bump();
        id
    }

    /// 记录一个阶段（同一阶段只记第一次；trace 不存在时忽略）
    pub fn mark(&self, trace: u64, stage: PerfStage) {
        let recorded = self
            .with_trace(trace, |t| {
                let slot = &mut t.stages[stage.index()];
                if slot.is_none() {
                    *slot = Some(t.t0.elapsed());
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false);
        if recorded {
            self.bump();
        }
    }

    /// 把面板 session 关联到追踪（之后 WindowManager 可以按 session 记阶段）
    pub fn bind_session(&self, trace: u64, session_id: u64) {
        self.with_trace(trace, |t| t.session_id = Some(session_id));
    }

    /// 按面板 session 记录阶段（关联到该 session 最近的一次追踪）
    pub fn mark_session(&self, session_id: u64, stage: PerfStage) {
        let trace = {
            let traces = self.traces.lock();
            traces
                .iter()
                .rev()
                .find(|t| t.session_id == Some(session_id))
                .map(|t| t.id)
        };
        if let Some(trace) = trace {
            self.mark(trace, stage);
        }
    }

    /// 记下本次唤出用的面板页面（关联到该 session 最近的一次追踪，只记第一次）
    pub fn set_page_for_session(&self, session_id: u64, page: PanelPage) {
        let mut traces = self.traces.lock();
        if let Some(t) = traces.iter_mut().rev().find(|t| t.session_id == Some(session_id))
            && t.page.is_none()
        {
            t.page = Some(page);
        }
    }

    /// 记录取词路径（ax / uia / atspi / copy / primary …；只写方法名，不写内容）
    pub fn set_method(&self, trace: u64, method: &str) {
        self.with_trace(trace, |t| t.method = Some(method.to_owned()));
    }

    /// 记录一次取词的结果：写一行 INFO 摘要（[`selection_log_line`]，不含正文），记进追踪
    /// （perf 行的 `selection=`），并按应用累计统计
    pub fn record_selection(&self, trace: u64, app: &str, method: Option<&str>, result: &'static str, elapsed_ms: u32) {
        log::info!("{}", selection_log_line(app, method, result, elapsed_ms));
        self.with_trace(trace, |t| t.selection_result = Some(result));
        let mut stats = self.selection.lock();
        let key = if stats.contains_key(app) || stats.len() < MAX_SELECTION_APPS {
            sanitize_token(app)
        } else {
            "other".to_owned()
        };
        let e = stats.entry(key).or_default();
        e.count = e.count.saturating_add(1);
        *e.methods.entry(method.unwrap_or("-").to_owned()).or_default() += 1;
        *e.results.entry(result.to_owned()).or_default() += 1;
        e.total_ms = e.total_ms.saturating_add(u64::from(elapsed_ms));
        e.avg_ms = u32::try_from(e.total_ms / u64::from(e.count.max(1))).unwrap_or(u32::MAX);
    }

    /// 按应用的取词统计（诊断导出的 `selection-stats.json`）
    pub fn selection_stats(&self) -> serde_json::Value {
        serde_json::to_value(&*self.selection.lock()).unwrap_or(serde_json::Value::Null)
    }

    /// 某次追踪的快照
    pub fn snapshot(&self, trace: u64) -> Option<TraceSnapshot> {
        self.traces
            .lock()
            .iter()
            .rev()
            .find(|t| t.id == trace)
            .map(Trace::snapshot)
    }

    /// id 大于 `after` 的追踪中最早的一条（冒烟模式：找到自己发出的那次触发）
    pub fn first_after(&self, after: u64) -> Option<TraceSnapshot> {
        self.traces.lock().iter().find(|t| t.id > after).map(Trace::snapshot)
    }

    /// 最近一次 begin 的 id（0 表示还没有）
    pub fn last_id(&self) -> u64 {
        self.next.load(Ordering::Relaxed)
    }

    /// 订阅「有新标记」通知
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// 结束追踪：写 `perf` 日志；开发者模式（或 debug 构建）下 emit_to panel `panel:latency`
    pub fn finish(&self, trace: u64, app: &tauri::AppHandle) {
        let snap = self
            .with_trace(trace, |t| {
                (!t.finished).then(|| {
                    t.finished = true;
                    t.snapshot()
                })
            })
            .flatten();
        let Some(snap) = snap else { return };
        log::info!("{}", snap.log_line());
        self.bump();
        let dev = cfg!(debug_assertions)
            || app
                .try_state::<AppState>()
                .is_some_and(|s| s.settings.get().general.developer_mode);
        if dev && let Err(e) = app.emit_to("panel", event::PANEL_LATENCY, snap.to_marks()) {
            log::debug!("emit panel:latency failed: {e}");
        }
    }

    /// 粘贴埋点：从选中条目（`selected_at`）到模拟粘贴按键发出（`posted_at`）的耗时。`method` 是发键方式
    /// （`send_input` / `cg_event` / `xtest` …，只保留 `[a-z0-9_-]`，最长 24 字符），写一行 [`paste_log_line`] 格式的日志，
    /// 返回耗时（毫秒）
    pub fn paste(&self, method: &str, selected_at: Instant, posted_at: Instant) -> u32 {
        let ms = duration_ms(posted_at.saturating_duration_since(selected_at));
        log::info!("{}", paste_log_line(method, ms));
        self.bump();
        ms
    }

    /// 记录一个启动阶段（从进程启动起算）
    pub fn record_startup(&self, stage: &'static str, since_process_start: Duration) {
        self.startup.lock().push((stage, duration_ms(since_process_start)));
        self.bump();
    }

    /// 启动各阶段的耗时（毫秒，从进程启动起算）
    pub fn startup(&self) -> Vec<(&'static str, u32)> {
        self.startup.lock().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_are_recorded_once_and_relative_to_begin() {
        let perf = PerfMarks::new();
        let t = perf.begin(TriggerOrigin::Hotkey);
        perf.set_method(t, "ax");
        perf.mark(t, PerfStage::Selection);
        perf.bind_session(t, 7);
        perf.mark(t, PerfStage::Prepare);
        std::thread::sleep(Duration::from_millis(5));
        perf.mark_session(7, PerfStage::Ready);
        perf.mark_session(7, PerfStage::Shown);
        let first = perf.snapshot(t).expect("trace");
        perf.mark_session(7, PerfStage::Shown);
        let again = perf.snapshot(t).expect("trace");
        assert_eq!(first.shown_ms, again.shown_ms, "同一阶段只记第一次");
        assert_eq!(again.method.as_deref(), Some("ax"));
        assert!(again.prepare_to_shown_ms().is_some_and(|d| d >= 5));
        assert!(!again.ready_timeout);
        let marks = again.to_marks();
        assert_eq!(marks.session_id, 7);
        assert_eq!(marks.origin, "hotkey");
    }

    #[test]
    fn log_line_has_fixed_fields_and_no_content() {
        let perf = PerfMarks::new();
        let t = perf.begin(TriggerOrigin::Cli);
        perf.mark(t, PerfStage::ReadyTimeout);
        let line = perf.snapshot(t).expect("trace").log_line();
        assert!(line.starts_with(&format!("perf trace={t} origin=cli session=- method=- selection_ms=-")));
        assert!(line.contains(" ready_timeout=true "), "{line}");
        // 后加的项在行尾（latency-report 按 key=value 解析，旧的解析不受影响）
        assert!(
            line.ends_with("overlay_ms=- selection=- window_ms=- key_ms=- page=-"),
            "{line}"
        );
        perf.mark(t, PerfStage::OverlayShown);
        perf.record_selection(t, "com.apple.TextEdit", Some("ax"), "ok", 12);
        let line = perf.snapshot(t).expect("trace").log_line();
        assert!(
            line.contains(" overlay_ms=") && line.contains(" selection=ok "),
            "{line}"
        );
        assert!(!line.contains("overlay_ms=-"), "{line}");
    }

    /// ready→shown 的 Rust 侧拆分与本次用的面板页面
    #[test]
    fn window_key_and_page_marks_are_logged() {
        let perf = PerfMarks::new();
        let t = perf.begin(TriggerOrigin::Hotkey);
        perf.bind_session(t, 9);
        perf.mark(t, PerfStage::Prepare);
        perf.set_page_for_session(9, PanelPage::Cold);
        perf.set_page_for_session(9, PanelPage::Warm);
        perf.mark_session(9, PerfStage::Ready);
        perf.mark_session(9, PerfStage::WindowShown);
        perf.mark_session(9, PerfStage::KeyGained);
        perf.mark_session(9, PerfStage::Shown);
        let snap = perf.snapshot(t).expect("trace");
        assert_eq!(snap.page, Some(PanelPage::Cold), "只记第一次");
        assert!(snap.window_ms.is_some() && snap.key_ms.is_some());
        let line = snap.log_line();
        assert!(line.contains(" window_ms=") && !line.contains("window_ms=-"), "{line}");
        assert!(line.contains(" key_ms=") && !line.contains("key_ms=-"), "{line}");
        assert!(line.ends_with(" page=cold"), "{line}");
        // 没有关联 session 的追踪不受影响
        perf.set_page_for_session(404, PanelPage::Recycled);
        assert_eq!(perf.snapshot(t).expect("trace").page, Some(PanelPage::Cold));
    }

    #[test]
    fn selection_summaries_carry_no_text_and_are_aggregated_per_app() {
        let perf = PerfMarks::new();
        let t = perf.begin(TriggerOrigin::Hotkey);
        perf.record_selection(t, "com.apple.Safari", Some("ax"), "ok", 10);
        perf.record_selection(t, "com.apple.Safari", Some("copy"), "ok", 30);
        perf.record_selection(t, "com.apple.Terminal", None, "empty", 300);
        let stats = perf.selection_stats();
        assert_eq!(stats["com.apple.Safari"]["count"], 2);
        assert_eq!(stats["com.apple.Safari"]["methods"]["ax"], 1);
        assert_eq!(stats["com.apple.Safari"]["avg_ms"], 20);
        assert_eq!(stats["com.apple.Terminal"]["results"]["empty"], 1);
        assert_eq!(stats["com.apple.Terminal"]["methods"]["-"], 1);
        assert_eq!(
            selection_log_line("com.apple.Safari", Some("ax"), "ok", 10),
            "selection app=com.apple.Safari method=ax result=ok elapsed_ms=10"
        );
        assert_eq!(
            selection_log_line("", None, "timeout", 0),
            "selection app=- method=- result=timeout elapsed_ms=0"
        );
    }

    #[test]
    fn traces_are_bounded() {
        let perf = PerfMarks::new();
        let first = perf.begin(TriggerOrigin::Tray);
        for _ in 0..MAX_TRACES {
            perf.begin(TriggerOrigin::Tray);
        }
        assert!(perf.snapshot(first).is_none(), "最旧的追踪被挤掉");
        assert_eq!(perf.traces.lock().len(), MAX_TRACES);
        assert_eq!(perf.first_after(first).map(|s| s.id), Some(first + 1));
    }

    #[test]
    fn unknown_traces_and_sessions_are_ignored() {
        let perf = PerfMarks::new();
        perf.mark(42, PerfStage::Shown);
        perf.mark_session(42, PerfStage::Shown);
        assert!(perf.snapshot(42).is_none());
        assert_eq!(perf.last_id(), 0);
    }

    #[test]
    fn subscribers_are_notified() {
        let perf = PerfMarks::new();
        let mut rx = perf.subscribe();
        rx.borrow_and_update();
        let t = perf.begin(TriggerOrigin::Hotkey);
        assert!(rx.has_changed().expect("open"));
        rx.borrow_and_update();
        perf.mark(t, PerfStage::Prepare);
        assert!(rx.has_changed().expect("open"));
    }

    #[test]
    fn paste_marks_have_a_fixed_format() {
        let perf = PerfMarks::new();
        let mut rx = perf.subscribe();
        rx.borrow_and_update();
        let selected = Instant::now();
        let ms = perf.paste("send_input", selected, selected + Duration::from_millis(37));
        assert_eq!(ms, 37);
        assert!(rx.has_changed().expect("open"));
        // 发键早于选中（调用方传反）时记 0，不 panic
        assert_eq!(perf.paste("cg_event", selected + Duration::from_millis(5), selected), 0);
        assert_eq!(
            paste_log_line("CG_Event", 12),
            "perf paste method=cg_event select_to_post_ms=12"
        );
        assert_eq!(
            paste_log_line("x test\n=1", 3),
            "perf paste method=xtest1 select_to_post_ms=3"
        );
        assert_eq!(paste_log_line("", 0), "perf paste method=- select_to_post_ms=0");
    }

    #[test]
    fn startup_stages_are_kept_in_order() {
        let perf = PerfMarks::new();
        perf.record_startup("settings", Duration::from_millis(12));
        perf.record_startup("tray", Duration::from_millis(80));
        assert_eq!(perf.startup(), vec![("settings", 12), ("tray", 80)]);
    }
}
