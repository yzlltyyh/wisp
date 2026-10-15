//! Panel 窗口的显示管线：显示时序（prepare → ready → show）、帧模式、定位与尺寸、失效重建、空闲卸载与回收。
//!
//! [`PanelController`] 是一个只管状态的状态机：所有副作用（原生窗口操作、emit、计时器、平台监听）都经 [`PanelHost`]
//! 发出——真实实现是 [`TauriPanelHost`]，单测用记录调用顺序的 mock（`windows::tests::visibility_order` 等）。
//!
//! ## 一次显示
//! 1. **prepare**：Windows 先同步 `SetIsVisible(true)` + 内存目标 NORMAL → 按视图决定是否可激活 →
//!    隐藏状态下把窗口放到最终宽度与估计高度（Native 帧模式同时把 WebView 固定为卡片上限高，见 native/macos.rs）→
//!    emit `panel:prepare` → 启动 40 ms ready 超时；
//! 2. **ready**：命令内同步显示并立即返回 `PanelShown`（Native 帧模式窗口 alpha 0→1 淡入；输入 / 剪贴板视图同时成为 key）；
//!    关闭监听（outside_click.rs）与透明区点击穿透交给宿主的监听线程异步安装，不推迟前端进入动画的起点。显示带一张
//!    [`ShowTicket`]：决定显示之后才开始的隐藏（截图前隐藏、固定交接……）让它作废，主线程执行到这次显示时直接跳过；
//! 3. **resize**：Native 帧模式交给原生逐帧弹簧（保留速度）；Css 帧模式按阶梯，Above 放置时窗口已是上限高度
//!    不再改；窗口不能透明时窗口就是卡片，每次立即改到位；剪贴板会话内只增不减。Css 帧模式的阶梯富余区
//!    在卡片高度稳定 300 ms 后一次回收，推迟的放大到期后必定执行；
//! 4. **hide**：Native 帧模式窗口 alpha 淡出后 orderOut（粘贴 / 截图 / 固定时立即）；Css 帧模式前端已播完退出动画，
//!    立即隐藏；注销监听，按需把焦点还给原应用，Windows 把 WebView2 设为不可见 + 低内存。Rust 自己发起的隐藏（截图前、
//!    固定交接）随后补发 `hide_request`，让前端照常走退出流程、停掉自己的工作。
//!
//! ## 拖动
//! 所有面板视图都能从头部与空白处拖动（前端 `usePanelDrag` → `panel_drag_start` → 系统的窗口拖动）。拖动期间几何跟随窗口
//! （读窗口位置排在计时器线程上，关闭监听的卡片矩形、点击穿透区域随之更新，「鼠标离开」暂停，失焦关闭推迟到拖动结束），
//! 窗口本身不改：Css 帧模式的阶梯、Native 帧模式的高度弹簧（进行中的停在当前高度）与宽度切换都推迟，不和系统的拖动抢
//! 位置。窗口停下 [`DRAG_SETTLE`] 且鼠标键已松开即拖动结束：窗口确实移动过（只按一下不算）才算拖过，卡片整体夹回所在
//! 屏幕的工作区（跨屏按新屏的工作区与缩放），推迟的变化补上（Native 的高度弹簧从窗口当前高度继续）。之后的会话内高度
//! 变化固定边照常不动，卡片伸出工作区时整体推回（Native 帧模式经 [`VerticalBound`] 在弹簧里逐帧推回）；高度上限仍是
//! 前端排版用的那个，交叉淡入原地替换时新会话继承「拖过」，上限按整个工作区算。拖动只影响本次会话：下次唤出按锚点定位；
//! 剪贴板在「记住位置」模式下隐藏时记住位置。
//!
//! ## 打断与恢复
//! - **淡出中又来新的 prepare**：淡出立即完成（旧内容不会被带到新位置上闪一下）；
//! - **可见时又来新的 prepare**：要求交叉淡入时**保留当前位置**原地替换（屏幕、左边、固定边都不变，只换宽度与高度上限）；
//!   位置没变时同样原地替换；位置变了就先退出（Native 140 ms 窗口淡出；Css 发 `hide_request(retrigger)`
//!   让前端播退出动画）再在新位置进入。原地替换按新视图重新决定可激活性（面板此刻是 key 时保持）；
//!   Native 帧模式宽度要变的交叉淡入里，窗口宽度等旧视图淡出完（`timing.crossfade`，内容完全透明的那一帧）才改，
//!   高度始终走 spring.gentle（[`WidthSwitch`]）；Css 帧模式窗口立即取新旧两张卡片的并集，卡片稳定后回收；
//! - **ready 超时**：以估计高度、窗口 alpha 0 先显示（WebView 可见后恢复渲染；整窗点击穿透，不吞点击），
//!   迟到的 ready 照常淡入；窗口不能透明时不提前显示（否则是一块可见的空卡片）。超时后 1 s 仍没有 ready 就判定 WebView
//!   失效，销毁并重建面板、重放这次 prepare；
//! - **页面被重新加载**（开发者 reload、WebContent 崩溃后重载）：准备中与 ready 超时（alpha 0）的会话按新的轮次重放
//!   prepare（旧轮次的超时 / 看门狗 / 补发作废），已显示的会话直接隐藏（经 [`PanelController::hidden`] 告知编排器）；
//! - **截图前隐藏**：可见的会话立即离屏，淡出中的立即完成，还在准备的取消（连同排队 / 暂存的 prepare），
//!   截图期间不会再有任何显示；
//! - **空闲卸载**（`advanced.unload_panel_after_idle_min`；新配置三端默认关——实测冷唤出内容首帧仍超过 150 ms）：
//!   隐藏超过 N 分钟后销毁面板窗口，下一次 prepare 先重建（暂存的 prepare 随 `Boot.prepare` 送进新页面，
//!   前端第一次渲染就 ready，不等页面加载后的 emit），这一次的额外延迟单独记日志。
//!   任何时刻最多一个卸载计时器。Native 帧模式下重建好窗口、页面还在加载时先显示原生卡片外壳（要焦点的视图立即成为 key，外壳
//!   期间的按键截住、ready 时重放；从第一帧起装关闭监听，页面卡住时外壳只留一次 [`SHELL_LOAD_WAIT`]），ready 时内容淡入（冷唤出的
//!   原生首帧，见 [`PanelController::shell_for_pending`]）。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tauri::webview::PageLoadEvent;
use tauri::{Emitter, Manager, WebviewWindowBuilder, WindowEvent};
use tokio::sync::watch;
use wisp_types::{
    AppRef, Boot, FrameMode, HideReason, PanelFocusChanged, PanelHideRequest, PanelPayload, PanelPrepare, PanelShown,
    PanelTransition, PanelView, Placement, Point, Rect, ResizeSpring, event,
};

use super::native::{
    self, CardSurface, ClickRegion, Fade, ShowAlpha, ShowOpts, ShowTicket, VerticalBound, WidthSwitch,
};
use super::outside_click::{CardHit, GuardSpec, Guards, HideSink};
use super::placement::{self, DesktopUnit, PanelGeometry, Screen};
use super::sizing::{self, CSS_SLACK_RECLAIM, CssHeightStepper, CssResize, CubicBezier, SpringParams};
use crate::app::perf::{PanelPage, PerfStage};
use crate::error::IpcError;
use crate::state::AppState;

/// 面板窗口 label（tauri.conf.json 中 `create: false` 的模板）
pub const PANEL_LABEL: &str = "panel";

pub use super::placement::MIN_CARD_HEIGHT;

/// emit prepare 后这么久还没收到 ready，就以估计高度直接显示（窗口 alpha 0 / 卡片 opacity 0，用户看不到）
pub const READY_TIMEOUT: Duration = Duration::from_millis(40);
/// 超时显示后再等这么久仍没有迟到的 ready，判定 WebView 失效并重建面板
pub const READY_WATCHDOG: Duration = Duration::from_secs(1);
/// Css 帧模式「先退出再进入」时等前端退出动画（fast）播完的兜底
pub const EXIT_FALLBACK: Duration = Duration::from_millis(300);
/// 拖动的窗口这么久没有新的移动事件、且鼠标键已松开，就认为拖动结束并更新几何
pub const DRAG_SETTLE: Duration = Duration::from_millis(200);

/// 拖动过的卡片被推回之后，推回的弹簧最多还要走多久（`Session::push_until`）：从 `from` 到 `to` 的静止时间，另留余量
/// （流式输出中弹簧重定目标时保留速度，实际可能略长）
fn push_settle(spring: ResizeSpring, from: f64, to: f64) -> Duration {
    sizing::settle_duration(SpringParams::of(spring), from, to) + Duration::from_millis(250)
}

/// 重建面板的最多次数（每次 prepare）：重建后仍超时就放弃这次显示，避免循环
const MAX_REBUILDS: u8 = 1;
/// 页面刚（重新）加载时前端的事件监听还没注册好（`listen` 本身是一次异步 IPC），这期间 emit 的 prepare 会丢：
/// 页面加载后这么久之内发出的 prepare，按 [`FRESH_PAGE_RESEND_EVERY`] 补发，直到 ready 到达
const FRESH_PAGE_WINDOW: Duration = Duration::from_secs(3);
/// 新页面上 prepare 的补发间隔：40 ms ready 超时**已经发生**之后每一帧补发一次，直到 ready 或补满
/// [`FRESH_PAGE_RESEND_MAX`] 次。前端正常处理一次 prepare 只要几毫秒到二三十毫秒，过了超时仍没有 ready，第一次 emit
/// 多半丢在了还没注册的监听上；监听一注册好，最多一帧之后就收到补发（按 150 / 400 / 900 ms 分档补发的话，冷唤出会平白多等
/// 约 60–100 ms）。补发与迟到的 ready 撞上时 Rust 侧幂等（见
/// [`PanelController::ready`]），前端按 session 去重；只在新页面（加载后 [`FRESH_PAGE_WINDOW`] 内）发生，
/// 每次是一条很小的 emit，同一时刻只有一个补发计时器
const FRESH_PAGE_RESEND_EVERY: Duration = Duration::from_millis(16);
/// 新页面上最多补发多少次（约 1.5 s；之后交给 ready 看门狗）
const FRESH_PAGE_RESEND_MAX: u8 = 90;
/// 冷唤出的原生外壳（[`PanelController::shell_for_pending`]）上屏之后，页面这么久还没加载完就按 WebView 失效处理
/// （与 ready 看门狗同一条重建路径），不让一张空卡片留在屏幕上
const SHELL_LOAD_WAIT: Duration = Duration::from_secs(2);

/// 减少动效时窗口淡入淡出的时长（线性）
const REDUCED_FADE: f64 = sizing::timing::REDUCED_FADE;

/// 一次 prepare 的输入（几何与视图策略由 WindowManager 按设置与锚点算好）
#[derive(Debug, Clone, PartialEq)]
pub struct PanelOpen {
    pub session_id: u64,
    pub view: PanelView,
    pub payload: PanelPayload,
    pub geometry: PanelGeometry,
    /// 显示后是否持有键盘焦点（输入 / 剪贴板视图）
    pub focused: bool,
    pub transition: PanelTransition,
    /// prepare 阶段窗口的初始卡片高度（估计值；真实高度以 ready 时前端的同步测量为准）
    pub estimated_height: f64,
    /// 触发时的前台应用（Windows / Linux 隐藏后还原焦点用）
    pub prev_app: Option<AppRef>,
    /// 显示期间要装的关闭监听
    pub guards: GuardSpec,
}

/// 运行期可变的选项（来自设置与外观）
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PanelOptions {
    /// 减少动效：窗口淡入淡出改为 100 ms 线性，高度直接到位
    pub reduce_motion: bool,
    /// 隐藏后把 WebView2 内存目标降为 LOW（`advanced.webview_low_memory_when_hidden`）
    pub low_memory_when_hidden: bool,
    /// 隐藏超过这么久后卸载面板 WebView（`advanced.unload_panel_after_idle_min`）
    pub unload_after: Option<Duration>,
    /// 用过的面板隐藏超过这么久后**回收**：销毁 WebView 并立即在后台重建、预热一个干净的页面（空闲卸载的
    /// 变体）。显示过的页面会话后一直占着更多内存（WebContent 约 28–29 MiB，从未显示过的约 21–23 MiB），
    /// macOS 上还会让 WebKit 一直重试把 backing store 标记为易失（会话后唤醒）。回收换掉 WebContent
    /// 进程：唤醒归零、WebContent 回到约 21.5 MiB；但 GPU 进程在亮屏时不随之回落（约 15.8 MiB），解锁会话里实测 WebView
    /// 合计仍有约 43–44 MiB，**回不到**从未显示时的约 39.4 MiB。下一次唤出仍是预热好的页面（但 WebContent 是新进程，JIT
    /// 是冷的）。
    ///
    /// 默认值 `native::PANEL_RECYCLE_AFTER`：macOS 45 s（默认开，常驻空闲零 CPU
    /// 优先），Windows / Linux 默认 None。debug 构建可以用 `WISP_PANEL_RECYCLE_SECS` 覆盖（测量用，见 manager.rs）。
    /// 回收不打断进行中的会话：计时器只在会话离屏之后启动，任何 prepare 都会取消它；推迟规则见 [`PanelHost::busy`]。
    ///
    /// 规则（见 [`idle_plan`]）：没用过的页面（刚预热、刚回收的）不回收；与 `unload_after` 同时设置时先到期的先执行——
    /// `recycle_after < unload_after` 时先回收，回收出来的干净页面在 `unload_after` 到期时照常卸载；否则直接卸载。
    /// 回收要在主线程上建窗口与 WebView：其他 Wisp 窗口在屏幕上、截图进行中时推迟（[`PanelHost::busy`]）
    pub recycle_after: Option<Duration>,
}

/// 面板隐藏之后的空闲动作
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdleAction {
    /// 卸载 WebView（用户开启的 `unload_panel_after_idle_min`；下一次唤出先重建）
    Unload,
    /// 回收：销毁后立即重建、预热干净的页面
    Recycle,
}

/// 回收因为其他 Wisp 窗口有短时活动而推迟（[`PanelHost::busy`]）之后，多久再看一次。只在推迟期间存在这一个计时器，
/// 而且推迟的总时长不超过 [`RECYCLE_DEFER_MAX`]（每轮空闲最多 8 次唤醒）；活动结束之后最多再等这么久，也给收尾动画与
/// 用户紧接着的操作留出余地
const RECYCLE_RETRY: Duration = Duration::from_secs(15);
/// 回收最多推迟这么久：到了就不再看 [`PanelHost::busy`]，直接回收（最坏情况是一次重建时的主线程停顿撞上别的窗口的
/// 动画），保证推迟不会无限期持续、也不会留下一个长期的周期唤醒
const RECYCLE_DEFER_MAX: Duration = Duration::from_secs(120);

/// 两个桌面矩形在半个逻辑像素内相同
fn same_rect(a: Rect, b: Rect) -> bool {
    (a.x - b.x).abs() < 0.5
        && (a.y - b.y).abs() < 0.5
        && (a.width - b.width).abs() < 0.5
        && (a.height - b.height).abs() < 0.5
}

/// 隐藏了 `idle_for` 之后，下一个空闲动作是什么、还要等多久。回收只针对用过的页面，并且要比卸载先到期：
/// `recycle_after < unload_after` 时先回收（回收出来的页面没用过，之后只剩卸载）；卸载先到期、同时到期或已经到期时
/// 直接卸载（它释放得更多，先回收再马上卸载是白做）
fn idle_plan(options: &PanelOptions, used_since_load: bool, idle_for: Duration) -> Option<(Duration, IdleAction)> {
    let unload = options.unload_after;
    let recycle = options
        .recycle_after
        .filter(|&r| used_since_load && unload.is_none_or(|u| r < u && idle_for < u));
    match (recycle, unload) {
        (Some(r), _) => Some((r.saturating_sub(idle_for), IdleAction::Recycle)),
        (None, Some(u)) => Some((u.saturating_sub(idle_for), IdleAction::Unload)),
        (None, None) => None,
    }
}

/// 面板窗口上发生的事（由 [`PanelHost::create`] 注册的回调送回控制器）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelEvent {
    /// 页面加载完成：wry 的 `PageLoadEvent::Finished`。**不保证**前端的模块脚本已执行、事件监听已就绪——macOS 的
    /// WKWebView 在 HTML 文档加载完时（导航开始后约 10–30 ms）就送达，模块图的解析求值与 `listen` 的注册还要再过约 80 ms
    /// （实测）；WebView2 的 NavigationCompleted 与 WebKitGTK 的 LOAD_FINISHED 才接近页面的 load 事件。
    /// 这一刻 emit 的事件可能没人收，靠补发 / `Boot.prepare` 兜住
    PageLoaded,
    /// 成为 / 失去 key
    Focused(bool),
    /// 窗口被移动（拖动）
    Moved,
}

/// 面板事件的接收端
pub type PanelEvents = Arc<dyn Fn(PanelEvent) + Send + Sync>;

/// 可取消的计时器（[`PanelHost::schedule_cancellable`]）。drop 不会取消，要显式调用 [`Self::cancel`]
pub struct TimerHandle(Box<dyn FnOnce() + Send>);

impl TimerHandle {
    /// 由取消动作构造
    pub fn new(cancel: impl FnOnce() + Send + 'static) -> Self {
        Self(Box::new(cancel))
    }

    /// 取消（计时器已触发时什么都不做）
    pub fn cancel(self) {
        (self.0)();
    }
}

/// 冷唤出外壳期间按下 Esc 的回调（[`PanelHost::show_shell`]；在主线程的下一轮事件循环里调用）
pub type ShellEscape = Arc<dyn Fn() + Send + Sync>;

/// 面板的全部副作用。实现必须线程安全；回调（`hide` 的 `done`、`schedule` 的任务、事件）可以在任意线程触发。
///
/// 控制器调用这些方法时**不持有**自己的锁，只有一组例外：关闭监听与点击穿透（`arm_guards`、`update_guard_card`、
/// `set_guard_key`、`pause_guards`、`disarm_guards`、`set_click_through`）与外壳的移动（`move_shell`）在锁内调用，
/// 好让它们的先后与会话状态的变化严格一致（ready 之后才装、隐藏之后不会再装回来）。这几个方法只能**排队**（不阻塞、
/// 不同步回调控制器），由宿主按 FIFO 执行
pub trait PanelHost: Send + Sync + 'static {
    /// 面板窗口是否存在
    fn exists(&self) -> bool;
    /// 隐藏创建面板窗口（阻塞到窗口建好；页面加载完成后经 `events` 送回 [`PanelEvent::PageLoaded`]）
    /// 创建面板窗口（隐藏）。`boot_prepare`：此刻暂存着的 prepare（冷唤出），随 `Boot.prepare` 送进新页面，前端第一次
    /// 渲染就处理它；预热 / 回收创建时为 None
    fn create(&self, events: PanelEvents, boot_prepare: Option<PanelPrepare>) -> Result<(), IpcError>;
    /// 销毁面板窗口（排在已排队的监听 / 穿透操作之后执行）
    fn destroy(&self);
    /// Windows 同步 `SetIsVisible(true)` + 内存目标 NORMAL；必须早于 emit prepare
    fn restore_visibility(&self);
    /// 隐藏后：Windows `SetIsVisible(false)`（+ 内存目标 LOW）
    fn release_visibility(&self, low_memory: bool);
    /// 立即把窗口放到 `rect`（桌面坐标）
    fn set_frame(&self, rect: Rect) -> Result<(), IpcError>;
    /// 进行中的退出淡出立即完成
    fn finish_exit(&self);
    /// Native 帧模式：WebView 固定为 `height` 高、贴住窗口顶边
    fn pin_webview(&self, height: f64) -> Result<(), IpcError>;
    /// 显示前决定是否可激活（Windows NOACTIVATE / Linux focusable）
    fn set_focusable(&self, focusable: bool);
    /// emit `panel:prepare`
    fn emit_prepare(&self, p: &PanelPrepare) -> Result<(), IpcError>;
    /// emit `panel:hide_request`
    fn emit_hide_request(&self, r: PanelHideRequest);
    /// emit `panel:focus_changed`
    fn emit_focus_changed(&self, f: PanelFocusChanged);
    /// 同步显示；主线程执行前 `ticket` 已作废时跳过并返回 [`ShowTicket::superseded`]
    fn show(&self, rect: Rect, opts: ShowOpts, ticket: &ShowTicket) -> Result<(), IpcError>;
    /// 已在屏幕上（alpha 0）的窗口改到 `rect` 并淡入（`ticket` 同上）
    fn reveal(&self, rect: Rect, fade: Option<Fade>, key: bool, ticket: &ShowTicket) -> Result<(), IpcError>;
    /// 冷唤出的原生首帧（只在 Native 帧模式下调用）：页面还在加载时，在**同一次**主线程往返里把窗口放到 `rect`、
    /// WebView 固定为 `max_height` 高，不激活地显示卡片外壳（原生材质 + 着色层 + 阴影，窗口 alpha 按 `fade` 淡入），WebView
    /// 内容保持透明。外壳期间（直到 [`Self::reveal_shell`] / 隐藏）：
    /// - `key`（输入 / 剪贴板视图）时外壳一上屏就成为 key（非激活面板，不激活应用、菜单栏不变），按键不会落到之前的前台
    ///   应用；
    /// - 送到外壳的按键由原生层截住、按原顺序缓存，ready 时重放给页面（第一个字不丢）；Esc 不缓存，下一轮事件循环调用
    ///   `on_escape`（控制器据此隐藏外壳）；
    /// - 外壳接收并**吞掉**鼠标事件：点击既不穿透到下面的应用，也不落到页面里还看不见的控件上。
    ///
    /// 默认实现退回普通显示（Windows / Linux 没有外壳，从不调用）
    fn show_shell(
        &self,
        rect: Rect,
        max_height: f64,
        fade: Option<Fade>,
        key: bool,
        on_escape: ShellEscape,
        ticket: &ShowTicket,
    ) -> Result<(), IpcError> {
        let _ = (max_height, on_escape);
        let opts = ShowOpts {
            alpha: fade.map_or(ShowAlpha::Opaque, ShowAlpha::FadeIn),
            key,
        };
        self.show(rect, opts, ticket)
    }
    /// 外壳在屏幕上、又来了几何不同的 prepare（换了锚点 / 视图 / 宽度）：把外壳移到 `rect`（`animate` 时用 spring.gentle 的
    /// 矩形弹簧，减少动效时直接到位），`max_height` 为 Some 时同一帧重新固定 WebView（内容透明，看不见重排）。
    /// **只排队**（主线程上异步执行，不阻塞），可以在锁内调用——外壳的几次移动与会话的先后严格一致。默认什么都不做
    fn move_shell(&self, rect: Rect, max_height: Option<f64>, animate: bool) {
        let _ = (rect, max_height, animate);
    }
    /// 外壳上的会话 ready：高度从外壳的估计值弹到 `rect`（位置也变了时整个矩形走弹簧；`animate` 为 false 时直接到位），
    /// 内容按 `fade` 淡入，`key` 时成为 key；外壳期间截住的按键随即按原顺序重放给页面，鼠标恢复正常。默认实现退回
    /// [`Self::reveal`]
    fn reveal_shell(
        &self,
        rect: Rect,
        placement: Placement,
        fade: Fade,
        key: bool,
        animate: bool,
        ticket: &ShowTicket,
    ) -> Result<(), IpcError> {
        let _ = (placement, animate);
        self.reveal(rect, Some(fade), key, ticket)
    }
    /// Native 帧模式的高度变化
    /// 原生逐帧弹簧（保留速度重定目标）。`spring`：`panel_resize` 指定的弹簧，其余调用方用 gentle。
    /// `bound`：拖动过的面板的竖直边界（[`VerticalBound`]），其余为 None
    fn animate_height(
        &self,
        height: f64,
        placement: Placement,
        bound: Option<VerticalBound>,
        animate: bool,
        spring: ResizeSpring,
    );
    /// Native 帧模式：进行中的高度弹簧停在当前高度（用户开始拖动面板：拖动期间不再逐帧写窗口，结束时控制器重新
    /// `animate_height`）。只排队、不等主线程；Css 帧模式没有原生弹簧，不调用
    fn freeze_height(&self);
    /// Native 帧模式原地替换的宽度切换（到时刻后改左边与宽度，不打断高度弹簧）
    fn switch_width(&self, switch: WidthSwitch);
    /// 隐藏；`done` 在窗口真正离屏后调用
    fn hide(&self, fade: Option<Fade>, done: Box<dyn FnOnce() + Send>);
    /// 成为 key / 前台
    fn activate(&self);
    /// 开始拖动窗口
    fn start_drag(&self);
    /// 鼠标左键 / 右键此刻是否按着（拖动停下 [`DRAG_SETTLE`] 之后据此区分「按住不动」与「已松开」）
    fn primary_button_down(&self) -> bool;
    /// 窗口外框（桌面坐标）
    fn window_frame(&self) -> Option<Rect>;
    /// 透明区点击穿透（锁内调用，只排队）
    fn set_click_through(&self, region: Option<ClickRegion>);
    /// 装上关闭监听（锁内调用，只排队）
    fn arm_guards(&self, session_id: u64, card: CardHit, spec: GuardSpec, key: bool, sink: HideSink);
    /// 卡片矩形变化（锁内调用，只排队）
    fn update_guard_card(&self, card: CardHit);
    /// key 状态变化：全局 Esc 的注册与注销（锁内调用，只排队）
    fn set_guard_key(&self, key: bool);
    /// 用户拖动面板期间暂停「鼠标离开 N px」的判定（光标随卡片移动，卡片矩形的更新总会晚一点；拖到另一块屏时更是
    /// 整块屏都算离开）。外部点击不受影响：按下在卡片里的拖动本来就不算点击（锁内调用，只排队）
    fn pause_guards(&self, paused: bool);
    /// 注销全部关闭监听（锁内调用，只排队）
    fn disarm_guards(&self);
    /// 隐藏取得过焦点的面板后，平台是否需要主动把焦点还给原应用（Windows / Linux；macOS 的非激活面板天然还原）
    fn restores_focus(&self) -> bool;
    /// 当前前台是否落在桌面 / 任务栏这类外壳上（或没有前台窗口）：失焦关闭后据此决定要不要还原焦点
    fn focus_on_shell(&self) -> bool;
    /// 把焦点还给原应用（Windows / Linux）
    fn restore_focus(&self, prev: &AppRef);
    /// 记住剪贴板面板的位置（卡片左上角，本屏逻辑坐标；按显示器分别记）
    fn remember_position(&self, screen_id: &str, top_left: Point);
    /// 桌面坐标的点落在哪块屏上
    fn screen_at(&self, desktop_point: Point) -> Option<Screen>;
    /// 性能埋点（按面板 session）
    fn perf(&self, session_id: u64, stage: PerfStage);
    /// 本次唤出用的面板页面（perf 行的 `page=`；ready 时报告——那时编排器早已把 session 关联到 trace）
    fn perf_page(&self, session_id: u64, page: PanelPage) {
        let _ = (session_id, page);
    }
    /// `delay` 后执行 `f`
    fn schedule(&self, delay: Duration, f: Box<dyn FnOnce() + Send>);
    /// `delay` 后执行 `f`，可以在到期前取消（长计时器用：取消后立即释放，不留挂起的任务）
    fn schedule_cancellable(&self, delay: Duration, f: Box<dyn FnOnce() + Send>) -> TimerHandle;
    /// 面板以外的 Wisp 窗口此刻有没有**短时**活动（截图进行中、Overlay 显示着、原生逐帧动画、浮标、刚打开的设置 /
    /// 引导窗口、刚建的或正在拖动的 pinned），返回原因（记日志用）。回收据此推迟：重建要在主线程上建窗口与 WebView
    /// （debug 实测 23–38 ms，之后页面加载 63–70 ms），会卡住这些窗口的拖动与动画。长时间留在屏幕上的
    /// pinned、开着的设置窗口本身不算（见 `manager::busy_reason`）。只在计时器线程上调用、不持有锁
    fn busy(&self) -> Option<String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// 已 emit prepare，等 ready
    Preparing,
    /// ready 超时，已以 alpha 0 显示，等迟到的 ready
    TimedOut,
    /// 已显示
    Shown,
    /// 正在隐藏（淡出中）
    Hiding,
}

#[derive(Debug)]
struct Session {
    open: PanelOpen,
    phase: Phase,
    /// 当前窗口高度（Css 帧模式的阶梯）
    window_height: f64,
    /// 窗口覆盖的卡片区宽度（逻辑 px，不含阴影边距）：通常等于卡片宽度；Css 帧模式原地替换期间取新旧两张卡片的并集，
    /// 交叉淡入时两张都不被裁切，卡片稳定后随富余区一起回收
    window_width: f64,
    /// 最近一次的卡片高度
    card_height: f64,
    stepper: CssHeightStepper,
    /// Css 帧模式：已排队的推迟放大
    deferred: bool,
    /// Css 帧模式：富余区回收的到期时刻（卡片每变化一次就往后推）
    slack_due: Option<Instant>,
    /// 已有一个富余区回收计时器在等（同一会话最多一个）
    slack_timer: bool,
    /// 当前是否是 key
    key: bool,
    /// 本次显示中是否成为过 key（失焦关闭只对成为过 key 的面板生效）
    ever_key: bool,
    /// 本次显示中用户把面板拖到了别处：窗口确实移动过（≥ 1 px，只按一下不算）才置位。之后卡片伸出工作区时整体
    /// 推回（[`PanelGeometry::fit_card`]），剪贴板隐藏时记住位置；交叉淡入原地替换时由新会话继承
    dragged: bool,
    /// 用户正在拖动：从 `panel_drag_start` 起，到窗口停下 [`DRAG_SETTLE`] 且鼠标键已松开为止。期间几何跟随窗口、
    /// 不推回、不改窗口（Css 帧模式的阶梯、Native 帧模式的高度弹簧与宽度切换都推迟，不和系统的拖动抢位置），失焦不关闭
    dragging: bool,
    /// 拖动事件的序号（防抖）
    drag_seq: u64,
    /// 已排了一次「读窗口位置、跟随」（同一时刻最多一个；窗口事件回调里不读窗口位置）
    follow_pending: bool,
    /// 拖动期间推迟的窗口尺寸变化（拖动结束时补上：Css 帧模式一次放到位，Native 帧模式的高度弹簧从窗口当前高度继续）
    frame_pending: bool,
    /// Native 帧模式：拖动期间推迟了宽度切换（拖动结束时窗口直接放到会话几何）
    width_pending: bool,
    /// 拖动期间失焦、按「失焦关闭」本该关闭（推迟到拖动结束：那时仍不是 key 再关）
    blur_deferred: bool,
    /// Native 帧模式：拖动过的卡片被推回，推回的弹簧到这一刻之前可能还在走——这期间的高度变化先按窗口当前位置同步固定边
    /// （弹簧从窗口当前位置继续）；其余时候窗口与几何一致，不必读窗口（读要同步等主线程）
    push_until: Option<Instant>,
    /// 复用可见窗口（交叉淡入 / 同位置替换）
    in_place: bool,
    /// 这次 prepare 的 ready 已经超时（看门狗与补发据此判断）
    timed_out: bool,
    prepared_at: Instant,
    /// 本次 prepare 已经重建过几次面板
    rebuilds: u8,
    /// 已补发 prepare 的次数（新页面上监听还没就绪时）
    resends: u8,
    /// 这次 prepare 的轮次（页面重载后重放时递增）：ready 超时、看门狗与补发的计时器带着轮次，过期的轮次作废
    attempt: u32,
    /// 这次唤出用的页面：用过的热页面 / 回收（或启动预热）出来还没用过的新页面 / 要先建窗口、等页面加载
    page: PanelPage,
    /// 窗口上是冷唤出的原生外壳：ready 时内容淡入、高度弹到实测值（[`PanelHost::reveal_shell`]）
    shell: bool,
}

/// 最近一次显示的卡片（toast 定位、截图前隐藏的判断）
#[derive(Debug, Clone, PartialEq)]
pub struct LastCard {
    pub screen: Screen,
    /// 卡片矩形（本屏逻辑坐标）
    pub rect: Rect,
}

#[derive(Default)]
struct Inner {
    session: Option<Session>,
    /// 窗口当前在屏幕上（含淡出中、ready 超时后的 alpha 0），值是「它离屏时会报告完成」的那个会话：显示它的会话，或
    /// 原地替换 / 接管窗口的新会话。旧会话迟到的离屏回调据此判断窗口是否真的已经离屏（见 [`PanelController::finish_hide`]）
    on_screen: Option<u64>,
    /// Native 帧模式：WebView 当前被固定的高度（原地替换时只在需要时重新固定，见 macos.rs 的 `panel_pin_webview`）
    pinned_webview: Option<f64>,
    /// 截图前隐藏在等这个会话的离屏完成（淡出中的会话）
    exit_waiter: Option<(u64, std::sync::mpsc::SyncSender<()>)>,
    /// 窗口不存在 / 页面没加载完时暂存的 prepare（连同已重建的次数）
    pending: Option<(PanelOpen, u8)>,
    /// 当前会话退出后再执行的 prepare（「先退出再进入」）
    queued: Option<PanelOpen>,
    /// 正在创建窗口
    creating: bool,
    /// 窗口创建后页面加载过（之后的 PageLoaded 都是 reload）
    loaded: bool,
    last_card: Option<LastCard>,
    /// 空闲动作（卸载 / 回收）的代号（每次 prepare / 隐藏都递增，已经开始执行的过期计时器据此作废）
    idle_gen: u64,
    /// 当前的空闲计时器（新的 prepare / 隐藏先取消它，任何时刻最多一个）
    idle_timer: Option<TimerHandle>,
    options: PanelOptions,
    /// 创建窗口的起点（卸载后重建的额外延迟单独记日志）
    create_started: Option<Instant>,
    /// 页面最近一次加载完成的时刻
    loaded_at: Option<Instant>,
    /// 页面加载之后收到过 prepare（视图渲染过）：回收只针对用过的页面
    used_since_load: bool,
    /// 正在回收：回收发生在隐藏之后多久（计时器链的逻辑时长）。回收出来的页面加载后，据此给剩下的卸载时长排计时器
    recycled_at: Option<Duration>,
    /// 页面加载后重放暂存的 prepare 时，那次唤出是冷的（等了建窗口 / 页面加载，`page=cold`）
    next_cold: bool,
    /// 暂存的 prepare 来自一张页面没能加载的外壳（`on_ready_watchdog` 正在为它重建）：重建好之后不再显示第二张外壳
    /// （外壳的寿命只有一次 [`SHELL_LOAD_WAIT`]），等新页面加载后走普通的显示。新的触发（`prepare`）清掉它
    pending_no_shell: bool,
    /// 页面加载完成之前就有会话凭 `Boot.prepare` ready 过：页面加载时据此把它记为用过（回收只针对用过的页面）
    boot_used: bool,
    /// 这次创建的窗口已经建好并转换完毕（`PanelHost::create` 已返回）：凭 `Boot.prepare` 提前到达的 ready 只在这之后才把
    /// 暂存的 prepare 变成会话（之前的 ready 回 None，页面等加载后的 emit）
    attached: bool,
    /// 当前页面的 `Boot.prepare` 带的会话：页面会凭它渲染一次视图，即使 Rust 这边那时它已经结束。它的 ready
    /// 到达时取走——没有对应的会话就把这一页记为用过（[`PanelController::note_boot_render`]）
    boot_session: Option<u64>,
}

impl Inner {
    /// 窗口是否在屏幕上
    fn visible(&self) -> bool {
        self.on_screen.is_some()
    }
}

/// 面板的当前状态快照
#[derive(Debug, Clone, PartialEq)]
pub struct PanelSnapshot {
    pub session_id: u64,
    pub view: PanelView,
    pub screen: Screen,
    /// 卡片矩形（本屏逻辑坐标）
    pub rect: Rect,
    pub key: bool,
    /// 窗口在屏幕上（含 ready 超时后的透明显示）
    pub visible: bool,
    /// 已经 ready 并显示
    pub shown: bool,
}

/// ready 时要执行的显示动作
enum ReadyPlan {
    /// 窗口还没显示：同步显示
    Show { rect: Rect, opts: ShowOpts },
    /// 超时后已以 alpha 0 显示：改到最终矩形并淡入
    Reveal { rect: Rect, fade: Option<Fade>, key: bool },
    /// 冷唤出的原生外壳已在屏幕上：内容淡入、高度弹到实测值
    RevealShell {
        rect: Rect,
        placement: Placement,
        fade: Fade,
        key: bool,
        animate: bool,
    },
    /// 原地替换（窗口保持可见）
    InPlace(InPlace),
}

/// 原地替换在 ready 时对窗口做的事：Css 帧模式立即把窗口改到新旧卡片的并集；Native 帧模式高度走弹簧，宽度 / 左边
/// 变了时在 `width_delay` 之后切换（见 [`PanelController::apply_in_place`]）
struct InPlace {
    rect: Rect,
    card: f64,
    placement: Placement,
    /// 新会话的高度上限（Native 帧模式 WebView 的固定高度）
    max_height: f64,
    /// 宽度切换的时刻（相对 ready）
    width_delay: Duration,
    /// 拖动过的面板：Native 帧模式高度弹簧的竖直边界（新视图更高、伸出工作区时整体推回）
    bound: Option<VerticalBound>,
    animate: bool,
    activate: bool,
}

/// 外壳交接之后要在锁外补的事（[`PanelController::note_hand_over`]）
#[derive(Debug, Default, Clone, Copy)]
struct HandOver {
    /// 新视图要焦点、外壳此刻还不是 key：让它成为 key
    activate: bool,
    /// 新视图要焦点、外壳此刻已是 key：补记 `KeyGained`
    key_gained: bool,
}

/// prepare 的分支
enum PrepareRoute {
    Fresh,
    FinishExitFirst,
    InPlace,
    ExitFirst {
        old: u64,
        fade: Option<Fade>,
    },
    /// 排到当前会话退出之后；带被覆盖的那次排队 prepare 的会话
    Queued(Option<u64>),
}

/// 面板显示管线
pub struct PanelController {
    host: Arc<dyn PanelHost>,
    frame_mode: FrameMode,
    /// 阴影边距（Css 帧模式且窗口能透明时为 SHADOW_MARGIN，否则 0；与 Boot.shadow_margin 同源）
    margin: f64,
    /// Css 帧模式且窗口不能透明（Linux Wayland 等）：窗口就是卡片
    opaque: bool,
    unit: DesktopUnit,
    inner: Mutex<Inner>,
    /// 串行化「算出窗口矩形 → 写进窗口」：并发的 resize / 回收 / ready 不会把旧尺寸写在新尺寸之后。
    /// 只在不持有 `inner` 时获取，主线程从不获取它（不会与主线程回调互相等待）
    frame_lock: Mutex<()>,
    /// 显示票的代号（[`ShowTicket`]）：任何隐藏开始时递增，此前发出的票全部作废
    shows: Arc<AtomicU64>,
    me: Weak<Self>,
    /// 页面加载次数
    loaded: watch::Sender<u64>,
    /// 最近一次结束的会话（隐藏**开始**、被取代、取消……，见 [`Self::hidden`]）
    hidden: watch::Sender<u64>,
    /// 最近一次窗口真正离屏的会话（[`Self::offscreen`]）
    offscreen: watch::Sender<u64>,
}

impl PanelController {
    /// 构造（不创建窗口）。`opaque`：Css 帧模式下窗口不能透明（卡片铺满窗口）
    pub fn new(
        host: Arc<dyn PanelHost>,
        frame_mode: FrameMode,
        margin: f64,
        opaque: bool,
        unit: DesktopUnit,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            host,
            frame_mode,
            margin,
            opaque: frame_mode == FrameMode::Css && opaque,
            unit,
            inner: Mutex::new(Inner::default()),
            frame_lock: Mutex::new(()),
            shows: Arc::new(AtomicU64::new(0)),
            me: me.clone(),
            loaded: watch::Sender::new(0),
            hidden: watch::Sender::new(0),
            offscreen: watch::Sender::new(0),
        })
    }

    /// 帧模式
    pub fn frame_mode(&self) -> FrameMode {
        self.frame_mode
    }

    /// 页面加载次数（frame lab 等待首次加载）
    pub fn loaded(&self) -> watch::Receiver<u64> {
        self.loaded.subscribe()
    }

    /// 会话结束的通知（watch 通道，值是最近一次结束的会话 id，**只当作「有会话结束了」的信号**）。每个会话结束时都会
    /// 发一次：**隐藏开始**（进入 `Phase::Hiding` 的那一刻，退出动画还没播：退出动画一开始就取消请求）
    /// 与窗口离屏完毕（再发一次，重复无害）、还没显示就被取消（截图前隐藏）、被新的 prepare 取代
    /// （原地替换 / 交叉淡入、准备中或 ready 超时时重新触发、排队或暂存的 prepare 被更新的覆盖）、窗口重建失败或看门狗
    /// 放弃。watch 只保留最新值，接连结束的几个会话只能看到最后一个，所以收到通知后要用 [`Self::is_live`] 核对自己登记的
    /// 全部会话。**要等窗口真正离开屏幕的调用方用 [`Self::offscreen`]**（这里的通知可能早于离屏）
    pub fn hidden(&self) -> watch::Receiver<u64> {
        self.hidden.subscribe()
    }

    /// 窗口真正离屏的通知（watch 通道，值是窗口刚离屏的那个会话 id）：只在 `finish_hide`（淡出 / orderOut 完成）时发出。
    /// 粘贴要等面板离开屏幕再发粘贴键，否则 ⌘V 可能落进面板自己——它订阅这个通道，而不是 [`Self::hidden`]
    /// （后者在隐藏开始时就会通知）
    pub fn offscreen(&self) -> watch::Receiver<u64> {
        self.offscreen.subscribe()
    }

    /// 某个会话是否仍然有效：是当前会话且没有在隐藏，或者是排队（先退出再进入）/ 暂存（窗口加载中）的 prepare。
    /// 隐藏中、已结束、被取代、从未出现过的会话都返回 false
    pub fn is_live(&self, session_id: u64) -> bool {
        let g = self.inner.lock();
        g.session
            .as_ref()
            .is_some_and(|s| s.open.session_id == session_id && s.phase != Phase::Hiding)
            || g.queued.as_ref().is_some_and(|q| q.session_id == session_id)
            || g.pending.as_ref().is_some_and(|(p, _)| p.session_id == session_id)
    }

    /// 仍然有效的会话（见 [`Self::is_live`]）触发时的前台应用；其他情况为 None
    pub fn prev_app(&self, session_id: u64) -> Option<AppRef> {
        let g = self.inner.lock();
        let open = g
            .session
            .as_ref()
            .filter(|s| s.open.session_id == session_id && s.phase != Phase::Hiding)
            .map(|s| &s.open)
            .or_else(|| g.queued.as_ref().filter(|q| q.session_id == session_id))
            .or_else(|| {
                g.pending
                    .as_ref()
                    .map(|(p, _)| p)
                    .filter(|p| p.session_id == session_id)
            })?;
        open.prev_app.clone()
    }

    /// 当前会话（没有在隐藏）的卡片宽度（逻辑 px，含视图内切换后的宽度）；其他情况为 None
    pub fn card_width(&self, session_id: u64) -> Option<f64> {
        let g = self.inner.lock();
        g.session
            .as_ref()
            .filter(|s| s.open.session_id == session_id && s.phase != Phase::Hiding)
            .map(|s| s.open.geometry.width)
    }

    /// 通知编排器这些会话已经结束（锁外调用）。重复通知无害：编排器按 [`Self::is_live`] 核对。日志每个会话只记一次
    /// （隐藏开始与离屏各通知一次，日志合并成一条）
    fn ended(&self, sessions: impl IntoIterator<Item = u64>) {
        for session_id in sessions {
            if *self.hidden.borrow() != session_id {
                log::debug!("panel session={session_id} ended");
            }
            self.hidden.send_replace(session_id);
        }
    }

    /// 更新运行期选项
    pub fn set_options(&self, options: PanelOptions) {
        self.inner.lock().options = options;
    }

    /// 当前选项
    pub fn options(&self) -> PanelOptions {
        self.inner.lock().options
    }

    fn events(&self) -> PanelEvents {
        let me = self.me.clone();
        Arc::new(move |ev| {
            if let Some(me) = me.upgrade() {
                me.on_event(ev);
            }
        })
    }

    fn sink(&self) -> HideSink {
        let me = self.me.clone();
        Arc::new(move |session_id, reason| {
            if let Some(me) = me.upgrade() {
                me.request_hide_for(session_id, reason);
            }
        })
    }

    /// 预热：隐藏创建面板窗口（已存在时直接返回）。阻塞到窗口建好
    pub fn create(&self) -> Result<(), IpcError> {
        {
            let mut g = self.inner.lock();
            if self.host.exists() || g.creating {
                return Ok(());
            }
            g.creating = true;
            g.attached = false;
            g.loaded = false;
            g.create_started = Some(Instant::now());
        }
        let result = self.host.create(self.events(), self.boot_prepare());
        if let Err(e) = &result {
            let mut g = self.inner.lock();
            g.creating = false;
            if g.pending.take().is_some() {
                log::error!("panel creation failed, pending prepare dropped: {}", e.message);
            }
        } else {
            self.inner.lock().attached = true;
            self.shell_for_pending();
        }
        result
    }

    /// 冷唤出的原生首帧（目标：空闲卸载之后的唤出 ≤ 150 ms、动效不打折）。窗口刚建好、页面还在加载（约 50–80 ms，之后
    /// 前端注册监听、处理 prepare 还要几十毫秒）时，暂存的 prepare 不再干等：立即按它的估计矩形以正常的进入淡入显示卡片
    /// 外壳（原生材质 + 着色层 + 阴影，Native 帧模式本来就由原生层画），WebView 内容保持透明。暂存的 prepare 就此成为
    /// 当前会话，处在「已在屏幕上、等 ready」（`Phase::TimedOut`）：页面加载后照常 emit（[`Self::on_page_loaded`]），ready
    /// 时内容淡入、高度从估计值弹到实测值（[`ReadyPlan::RevealShell`]）；期间的隐藏 / 截图前隐藏 / 新的 prepare 都按
    /// 「ready 超时后已显示」的会话处理。页面 [`SHELL_LOAD_WAIT`] 内没有加载完，按 WebView 失效重建（ready 看门狗）。
    /// 要焦点的视图（输入 / 剪贴板）外壳一上屏就成为 key，外壳期间的按键截住、ready 时重放，Esc 立即收起；外壳吞掉点击
    /// （[`PanelHost::show_shell`]）。
    ///
    /// 定位、固定 WebView 与显示在一次主线程往返里完成，并且整段持有 `frame_lock`：页面可能在这期间加载完、前端的 ready
    /// 紧跟着到达——ready 要等外壳真正上屏之后才能执行 reveal，不会出现「reveal 在前、外壳的显示在后把内容重新藏起来」。
    /// Css 帧模式（Windows / Linux）卡片由 CSS 画，没有可以先显示的外壳，照旧等 ready
    fn shell_for_pending(&self) {
        if self.frame_mode != FrameMode::Native {
            return;
        }
        // 只在阻塞线程上调用（预热 / 重建的 create 之后），从不在主线程：持有 frame_lock 等主线程是安全的（同 ready）
        let _frame = self.frame_lock.lock();
        let (rect, max_height, fade, key, ticket, session_id, attempt) = {
            let mut g = self.inner.lock();
            if g.loaded || g.visible() || g.session.is_some() {
                return;
            }
            // 上一张外壳下的页面没能加载、正在为它重建（`on_ready_watchdog`）：这次不再显示第二张外壳，暂存的 prepare
            // 等新页面加载后走普通的显示（外壳的寿命只有一次 SHELL_LOAD_WAIT）
            if g.pending.is_some() && std::mem::take(&mut g.pending_no_shell) {
                return;
            }
            let Some((open, rebuilds)) = g.pending.take() else {
                return;
            };
            let options = g.options;
            let mut s = self.new_session(open, rebuilds);
            s.phase = Phase::TimedOut;
            s.shell = true;
            s.page = PanelPage::Cold;
            // 看门狗按「已超时」核对：页面迟迟不加载时它照常重建（加载后按新的轮次重新开始，这一轮作废）
            s.timed_out = true;
            let rect = self.desktop_window_rect(&s);
            let max_height = s.open.geometry.max_height;
            let key = s.open.focused;
            let session_id = s.open.session_id;
            let attempt = s.attempt;
            g.session = Some(s);
            g.on_screen = Some(session_id);
            g.pinned_webview = None;
            (
                rect,
                max_height,
                self.fade_for(options, true),
                key,
                self.ticket(),
                session_id,
                attempt,
            )
        };
        self.host.restore_visibility();
        let shown = self
            .host
            .show_shell(rect, max_height, Some(fade), key, self.shell_escape(), &ticket);
        if let Err(e) = shown {
            if ticket.valid() {
                log::warn!("panel shell show failed: {}", e.message);
                self.shell_show_failed(session_id);
            }
            return;
        }
        {
            // 交接要拿 frame_lock（`prepare_attempt`），显示期间不会发生；隐藏不拿它，可能已经开始——那时会话已不是外壳
            let mut g = self.inner.lock();
            g.pinned_webview = Some(max_height);
            // 外壳看得见就要收得掉：装上与热唤出相同的关闭监听——翻译视图（不是 key）的全局 Esc、
            // 点击卡片外部；要焦点的视图另有原生守卫截住 Esc、失焦即收起。监听的回调对外壳会话直接原生收起
            // （`request_hide_for`），ready 时按实测高度重新装
            if let Some(s) = g
                .session
                .as_ref()
                .filter(|s| s.open.session_id == session_id && s.shell && s.phase == Phase::TimedOut)
            {
                self.host
                    .arm_guards(session_id, Self::card_hit(s), s.open.guards, key, self.sink());
            }
        }
        log::debug!("panel cold show: native shell up session={session_id} key={key}");
        self.host.perf(session_id, PerfStage::WindowShown);
        self.arm_shell_watchdog(session_id, attempt);
    }

    /// 外壳没能显示（主线程超时等，票仍然有效）。处理的是**此刻**站在外壳上的会话——不论它是不是发起这次显示的那个
    /// （交接与外壳的显示由 frame_lock 串行，正常不会在显示期间换人；这里不依赖这一点）：
    /// - 页面还没加载：退回暂存，页面加载后照旧准备、显示；
    /// - 页面已经加载（显示期间加载完成，这次会话已按新的轮次 emit 过）：再暂存就没人重放了——改为普通的「准备中」会话，
    ///   ready 超时后照常透明显示、迟到的 ready 淡入，这次触发不会被悄悄丢掉
    fn shell_show_failed(&self, shown_for: u64) {
        let mut g = self.inner.lock();
        g.pinned_webview = None;
        let Some(session_id) = g
            .session
            .as_ref()
            .filter(|s| s.shell && s.phase == Phase::TimedOut)
            .map(|s| s.open.session_id)
        else {
            return;
        };
        if g.on_screen == Some(session_id) {
            g.on_screen = None;
        }
        self.host.disarm_guards();
        if g.loaded {
            if let Some(s) = g.session.as_mut() {
                s.shell = false;
                s.phase = Phase::Preparing;
            }
            log::info!(
                "panel shell show failed after the page loaded; continuing as a normal show session={session_id} \
                 (shown for {shown_for})"
            );
        } else if let Some(s) = g.session.take() {
            g.pending = Some((s.open, s.rebuilds));
        }
    }

    /// 外壳期间按 Esc（[`PanelHost::show_shell`] 的 `on_escape`，主线程的下一轮事件循环）：收起当前的外壳会话。只认会话
    /// 当时的状态、不绑定某个 session——外壳期间新的 prepare 会接过同一张外壳；ready 与 Esc 撞在同一轮里时会话可能刚变成
    /// 已显示，Esc 已被原生层吞掉，同样收起
    fn shell_escape(&self) -> ShellEscape {
        let me = self.me.clone();
        Arc::new(move || {
            let Some(me) = me.upgrade() else { return };
            let session = me
                .inner
                .lock()
                .session
                .as_ref()
                .filter(|s| s.shell && matches!(s.phase, Phase::TimedOut | Phase::Shown))
                .map(|s| s.open.session_id);
            if let Some(session_id) = session {
                log::info!("panel shell: Esc before the page was ready; hiding session={session_id}");
                me.hide_now_and_notify(session_id, HideReason::Escape);
            }
        })
    }

    /// 外壳上屏之后页面 [`SHELL_LOAD_WAIT`] 内没有加载完：按 WebView 失效重建（ready 看门狗的路径；页面加载后轮次递增，
    /// 这个计时器随之作废）
    fn arm_shell_watchdog(&self, session_id: u64, attempt: u32) {
        let me = self.me.clone();
        self.host.schedule(
            SHELL_LOAD_WAIT,
            Box::new(move || {
                if let Some(me) = me.upgrade() {
                    me.on_ready_watchdog(session_id, attempt);
                }
            }),
        );
    }

    /// 出一张显示票（锁内调用）
    fn ticket(&self) -> ShowTicket {
        ShowTicket::issue(&self.shows)
    }

    /// 让已经发出的显示票作废（锁内、在投递任何离屏操作之前调用）
    fn revoke_shows(&self) {
        ShowTicket::revoke_all(&self.shows);
    }

    /// 宽度要变的交叉淡入里旧视图淡出的时长（前端 ViewSwitch 的 `mode="wait"`：`timing.crossfade`，减少动效时
    /// `timing.reducedFade`）。旧内容在这之后完全透明
    fn crossfade_exit(options: PanelOptions) -> Duration {
        Duration::from_secs_f64(if options.reduce_motion {
            sizing::timing::REDUCED_FADE
        } else {
            sizing::timing::CROSSFADE
        })
    }

    fn fade_for(&self, options: PanelOptions, entering: bool) -> Fade {
        if options.reduce_motion {
            Fade {
                duration: REDUCED_FADE,
                curve: CubicBezier::LINEAR,
            }
        } else if entering {
            Fade {
                duration: sizing::duration::BASE,
                curve: CubicBezier::STANDARD,
            }
        } else {
            Fade {
                duration: sizing::duration::FAST,
                curve: CubicBezier::EXIT,
            }
        }
    }

    /// 是否是带透明边距的 Css 帧模式窗口（阶梯富余区、点击穿透只在这时存在）
    fn css_with_margin(&self) -> bool {
        self.frame_mode == FrameMode::Css && !self.opaque && self.margin > 0.0
    }

    /// 显示时的窗口高度：Native / 不透明 = 卡片；Css + Above 取上限（会话内不再改变）；其余 = 卡片 + 2·margin
    fn window_height_for(&self, g: &PanelGeometry, card: f64) -> f64 {
        match self.frame_mode {
            FrameMode::Native => card,
            FrameMode::Css if self.opaque => card,
            FrameMode::Css if g.placement == Placement::Above => g.max_height + 2.0 * self.margin,
            FrameMode::Css => card + 2.0 * self.margin,
        }
    }

    fn desktop_window_rect(&self, s: &Session) -> Rect {
        let g = &s.open.geometry;
        let mut local = g.window_rect(s.window_height, self.margin);
        local.width = s.window_width + 2.0 * self.margin;
        g.screen.to_desktop(local, self.unit)
    }

    /// Css 帧模式且窗口带透明富余区时的点击穿透区域
    fn click_region(&self, s: &Session) -> Option<ClickRegion> {
        if !self.css_with_margin() {
            return None;
        }
        let g = &s.open.geometry;
        Some(ClickRegion {
            window: self.desktop_window_rect(s),
            card: g.screen.to_desktop(g.card_rect(s.card_height), self.unit),
        })
    }

    /// 整个窗口都放行点击（ready 超时后的透明显示：卡片还是 opacity 0）
    fn passthrough_region(&self, window: Rect) -> Option<ClickRegion> {
        self.css_with_margin().then(|| ClickRegion {
            window,
            card: Rect::new(window.x, window.y, 0.0, 0.0),
        })
    }

    fn card_hit(s: &Session) -> CardHit {
        let g = &s.open.geometry;
        (g.screen.id.clone(), g.card_rect(s.card_height))
    }

    /// 窗口是否比卡片多出透明富余区（阶梯放大的余量、原地替换时新旧卡片的并集）。Above 放置的窗口高度是会话上限，
    /// 回收要移动窗口顶边（Windows 上会让卡片跳一帧），所以只回收宽度
    fn has_slack(&self, s: &Session) -> bool {
        if !self.css_with_margin() {
            return false;
        }
        let g = &s.open.geometry;
        s.window_width > g.width + 0.5
            || (g.placement != Placement::Above && s.window_height > s.card_height + 2.0 * self.margin + 0.5)
    }

    /// 卡片刚变化：有富余区就把回收时刻推到 `settle`（至少 [`CSS_SLACK_RECLAIM`]）之后，已排的更晚的时刻保留。
    /// 返回 true 表示要（在锁外）排一个回收计时器
    fn arm_slack(&self, s: &mut Session, settle: Duration) -> bool {
        if !self.has_slack(s) {
            s.slack_due = None;
            return false;
        }
        let due = Instant::now() + settle.max(CSS_SLACK_RECLAIM);
        s.slack_due = Some(s.slack_due.map_or(due, |d| d.max(due)));
        !std::mem::replace(&mut s.slack_timer, true)
    }

    fn schedule_slack(&self, session_id: u64, delay: Duration) {
        let me = self.me.clone();
        self.host.schedule(
            delay,
            Box::new(move || {
                if let Some(me) = me.upgrade() {
                    me.reclaim_slack(session_id);
                }
            }),
        );
    }

    /// 取出空闲卸载计时器（锁内）；调用方在锁外取消
    fn take_idle_timer(g: &mut Inner) -> Option<TimerHandle> {
        g.idle_gen = g.idle_gen.wrapping_add(1);
        g.idle_timer.take()
    }

    /// 建窗口时随 `Boot.prepare` 送进新页面的 prepare：此刻暂存着的那一次（冷唤出：空闲卸载 /
    /// 失效重建之后的唤出）。前端第一次渲染就挂载视图、同步测量并 `panel_ready`，不等页面加载完成、`listen` 的往返与补发；
    /// 页面加载后 Rust 照常 emit 同一会话作兜底（外壳会话按新轮次、普通会话经 `prepare_routed`），前端按会话去重。
    /// 建窗口期间又来的 prepare 取代暂存的那次：这时 Boot 里的已过期，它的 ready 被忽略（返回 None），新会话照旧在页面加载后
    /// emit。只在建窗口之前读一次（Boot 是初始化脚本，之后改不了）
    fn boot_prepare(&self) -> Option<PanelPrepare> {
        let mut g = self.inner.lock();
        let prepare = g.pending.as_ref().map(|(open, _)| Self::prepare_payload(open));
        g.boot_session = prepare.as_ref().map(|p| p.session_id);
        prepare
    }

    /// 页面凭 `Boot.prepare` 渲染了一个 Rust 这边已经不在的会话：冷唤出的外壳上按了 Esc、输入 / 剪贴板视图失焦
    /// 被收起、截图前隐藏取消了暂存——都可能发生在页面 JS 运行之前，那时发出的 `panel:hide_request` 页面还没有监听。
    /// 它的 ready 回 None，前端据此复位为隐藏；这一页已经渲染过视图，记为用过：页面还没加载完时交给页面加载
    /// （`boot_used`，[`Self::on_page_loaded`] 接着排空闲计时器），已经加载且面板空闲时立即按回收 / 卸载的规则重排空闲
    /// 计时器（从现在起算：与隐藏只差页面 JS 运行前的那一小段）；面板不空闲（会话淡出中、新的会话）时由它的隐藏去排。
    /// 只在页面加载完成之后才 emit 的竞态（暂存还没变成会话）同样走到这里：页面随后会凭 emit 再用一次，记为用过不冤枉
    fn note_boot_render(&self, session_id: u64) {
        let rearm = {
            let mut g = self.inner.lock();
            if !g.loaded {
                g.boot_used = true;
                None
            } else if g.used_since_load {
                None
            } else {
                g.used_since_load = true;
                Self::rearm_idle_if_idle(&mut g)
            }
        };
        log::info!(
            "panel ready for a Boot.prepare session that is no longer live; page counted as used session={session_id}"
        );
        if let Some((old, idle_gen, after)) = rearm {
            if let Some(t) = old {
                t.cancel();
            }
            self.arm_idle(idle_gen, after, after);
        }
    }

    /// 面板空闲（没有会话、不在屏幕上、没有创建中 / 暂存 / 排队的 prepare）时按当前的 `used_since_load` 重新开始一轮空闲：
    /// 代号递增（进行中的旧计时器链作废）、取出旧计时器（调用方在锁外取消），返回 (旧计时器, 新代号, 第一段等待)
    fn rearm_idle_if_idle(g: &mut Inner) -> Option<(Option<TimerHandle>, u64, Duration)> {
        let idle = g.session.is_none() && !g.visible() && !g.creating && g.pending.is_none() && g.queued.is_none();
        let (after, _) = idle_plan(&g.options, g.used_since_load, Duration::ZERO).filter(|_| idle)?;
        g.idle_gen = g.idle_gen.wrapping_add(1);
        Some((Self::take_idle_timer(g), g.idle_gen, after))
    }

    /// ready 到达时会话还在暂存里：页面凭 `Boot.prepare` 先于 Rust 处理暂存就 ready 了——Css 帧模式没有外壳、
    /// 暂存要等页面加载完才变成会话；Native 帧模式外壳还没上屏（`shell_for_pending` 还没拿到 frame_lock）或这次不显示外壳
    /// （`pending_no_shell`）。按 prepare 时的做法把它变成当前会话：恢复可见性、决定可激活性、隐藏状态下定位、Native 帧
    /// 模式固定 WebView，随后照常走 ready 的同步显示（整窗淡入，内容已经就绪，不需要外壳）。调用方持有 frame_lock。
    /// 暂存的不是这个会话（已被新的 prepare 取代）或已有当前会话时什么都不做（ready 随后返回 None，页面等新会话的 emit）
    fn promote_pending_for_ready(&self, session_id: u64) {
        let (rect, focused, max_height) = {
            let mut g = self.inner.lock();
            if !g.attached || g.session.is_some() || g.pending.as_ref().is_none_or(|(p, _)| p.session_id != session_id)
            {
                return;
            }
            let Some((open, rebuilds)) = g.pending.take() else {
                return;
            };
            let mut s = self.new_session(open, rebuilds);
            s.page = PanelPage::Cold;
            g.next_cold = false;
            g.pending_no_shell = false;
            g.boot_used = true;
            g.used_since_load = true;
            let rect = self.desktop_window_rect(&s);
            let focused = s.open.focused;
            let max_height = s.open.geometry.max_height;
            g.session = Some(s);
            (rect, focused, max_height)
        };
        log::info!(
            "panel cold show: content ready from Boot.prepare before the page finished loading session={session_id}"
        );
        self.host.restore_visibility();
        self.host.set_focusable(focused);
        if let Err(e) = self.host.set_frame(rect) {
            log::warn!("panel prepare set_frame failed: {}", e.message);
        }
        if self.frame_mode == FrameMode::Native {
            match self.host.pin_webview(max_height) {
                Ok(()) => self.inner.lock().pinned_webview = Some(max_height),
                Err(e) => log::warn!("panel pin webview failed: {}", e.message),
            }
        }
    }

    // -----------------------------------------------------------------------------------------------------------
    // prepare

    /// prepare。见模块注释「一次显示」与「打断与恢复」
    pub fn prepare(&self, open: PanelOpen) {
        // 新的触发：上一张外壳「不再显示第二张」的限制不延续到它
        self.inner.lock().pending_no_shell = false;
        self.prepare_attempt(open, 0);
    }

    /// 冷唤出的外壳此刻在屏幕上（含正在显示的那一刻）：这时的 prepare 要接过外壳（[`Self::hand_over_shell`]），
    /// 必须与外壳的显示和 ready 串行
    fn shell_current(&self) -> bool {
        self.inner
            .lock()
            .session
            .as_ref()
            .is_some_and(|s| s.shell && s.phase == Phase::TimedOut)
    }

    /// 选路之前先看要不要接过外壳：要就先拿 `frame_lock`。外壳的显示（`shell_for_pending`）整段持有
    /// frame_lock，于是交接一定排在显示完成之后——显示期间的交接会让 `move_shell` 先于 `show_shell` 执行（外壳被放回旧
    /// 矩形与旧的 WebView 高度），或让显示失败的回退落在已经换人的会话上。
    ///
    /// 主线程从不获取 frame_lock：在主线程上调用 prepare 的只有页面加载后的重放（[`Self::on_page_loaded`]，那时 `loaded`
    /// 已置位、暂存与外壳会话互斥）与退出完成后的排队 prepare（[`Self::finish_hide`]，会话刚清空），两者都直接走
    /// [`Self::prepare_routed`]，不经过这里
    fn prepare_attempt(&self, open: PanelOpen, rebuilds: u8) {
        let _frame = self.shell_current().then(|| self.frame_lock.lock());
        self.prepare_routed(open, rebuilds);
    }

    fn prepare_routed(&self, open: PanelOpen, rebuilds: u8) {
        // 窗口不存在（未预热 / 已卸载 / 重建中）或页面还没加载完：先暂存，创建并加载后再继续
        let idle_timer = {
            let mut g = self.inner.lock();
            let idle_timer = Self::take_idle_timer(&mut g);
            let exists = self.host.exists();
            // 冷唤出的外壳正在等页面加载：外壳立即交给新的 prepare，旧会话就此结束
            let shell = g
                .session
                .as_ref()
                .filter(|s| {
                    s.shell && s.phase == Phase::TimedOut && !g.loaded && g.on_screen == Some(s.open.session_id)
                })
                .map(|s| (s.open.session_id, s.attempt));
            if exists && let Some((old, attempt)) = shell {
                let mut s = self.new_session(open, rebuilds);
                s.timed_out = true;
                s.attempt = attempt;
                let session_id = s.open.session_id;
                let handed = match g.session.take() {
                    Some(prev) => self.hand_over_shell(&mut g, &mut s, &prev),
                    None => HandOver::default(),
                };
                // 新会话的关闭监听（旧会话的随它结束；外壳一直在屏幕上，要一直收得掉）
                self.host
                    .arm_guards(session_id, Self::card_hit(&s), s.open.guards, s.key, self.sink());
                g.session = Some(s);
                g.on_screen = Some(session_id);
                drop(g);
                if let Some(t) = idle_timer {
                    t.cancel();
                }
                self.note_hand_over(session_id, handed);
                self.ended([old].into_iter().filter(|&id| id != session_id));
                self.arm_shell_watchdog(session_id, attempt);
                return;
            }
            if !exists || g.creating || !g.loaded {
                // 只保留最新的一次：被覆盖的暂存 prepare 从未显示过，它的会话就此结束
                let session_id = open.session_id;
                let replaced = g
                    .pending
                    .replace((open, rebuilds))
                    .map(|(p, _)| p.session_id)
                    .filter(|&id| id != session_id);
                let recreate = !exists && !g.creating;
                if recreate {
                    g.creating = true;
                    g.attached = false;
                    g.loaded = false;
                    g.create_started = Some(Instant::now());
                }
                drop(g);
                if let Some(t) = idle_timer {
                    t.cancel();
                }
                self.ended(replaced);
                if recreate {
                    log::info!("panel window missing, recreating before prepare");
                    self.spawn_create_after_flag();
                }
                return;
            }
            idle_timer
        };
        if let Some(t) = idle_timer {
            t.cancel();
        }
        let route = {
            let mut g = self.inner.lock();
            let options = g.options;
            let visible = g.visible();
            match g.session.as_ref() {
                // 已经在「先退出再进入」（Native 淡出中 / Css 等前端退出）：只保留最新的一次，被覆盖的那次从未显示过，
                // 它的会话就此结束
                Some(_) if g.queued.is_some() => {
                    let replaced = g
                        .queued
                        .replace(open.clone())
                        .map(|q| q.session_id)
                        .filter(|&id| id != open.session_id);
                    PrepareRoute::Queued(replaced)
                }
                Some(s) if s.phase == Phase::Hiding => PrepareRoute::FinishExitFirst,
                // ready 超时后以 alpha 0 显示着的会话：用户什么都没看到，按全新会话准备（之后正常淡入）
                Some(s) if visible && s.phase != Phase::TimedOut => {
                    if open.transition == PanelTransition::Crossfade || s.open.geometry.same_position(&open.geometry) {
                        PrepareRoute::InPlace
                    } else {
                        let old = s.open.session_id;
                        g.queued = Some(open.clone());
                        let fade = (self.frame_mode == FrameMode::Native).then(|| self.fade_for(options, false));
                        PrepareRoute::ExitFirst { old, fade }
                    }
                }
                _ => PrepareRoute::Fresh,
            }
        };
        match route {
            PrepareRoute::Queued(replaced) => self.ended(replaced),
            PrepareRoute::ExitFirst { old, fade } => self.exit_for_retrigger(old, fade),
            PrepareRoute::FinishExitFirst => {
                // 淡出中：立即完成（orderOut + 回调），再按全新会话准备
                self.host.finish_exit();
                self.prepare_fresh(open, rebuilds);
            }
            PrepareRoute::InPlace => self.prepare_in_place(open, rebuilds),
            PrepareRoute::Fresh => self.prepare_fresh(open, rebuilds),
        }
    }

    /// 后台创建窗口（卸载后 / 重建时，调用方已在锁内置好 creating 标志）；创建完成、页面加载后执行暂存的 prepare
    fn spawn_create_after_flag(&self) {
        let me = self.me.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let Some(me) = me.upgrade() else { return };
            let result = me.host.create(me.events(), me.boot_prepare());
            if result.is_ok() {
                me.inner.lock().attached = true;
                me.shell_for_pending();
            }
            if let Err(e) = result {
                let (dropped, recycling) = {
                    let mut g = me.inner.lock();
                    g.creating = false;
                    g.create_started = None;
                    // 回收失败：面板此后与卸载后相同（没有窗口），不能再拿这次回收的时刻给之后的加载排剩余的卸载时长
                    let recycling = g.recycled_at.take().is_some();
                    (g.pending.take().map(|(p, _)| p.session_id), recycling)
                };
                if recycling {
                    log::error!(
                        "recycle panel failed: {}; the panel stays unloaded and the next show recreates it",
                        e.message
                    );
                } else {
                    log::error!("recreate panel failed: {}", e.message);
                }
                me.ended(dropped);
            }
        });
    }

    /// 「先退出再进入」：旧会话退出完毕（`finish_hide`）后执行排队的 prepare
    fn exit_for_retrigger(&self, old: u64, fade: Option<Fade>) {
        if self.frame_mode == FrameMode::Native {
            // 窗口淡出由 Rust 承担；仍要告诉前端旧会话结束了——排队的 prepare 可能被截图作废而不再到来，前端不能停在
            // shown、让视图一直挂在离屏的页面里（见 hide_now_and_notify）
            if self.hide(old, HideReason::Retrigger, fade) {
                self.host.emit_hide_request(PanelHideRequest {
                    session_id: old,
                    reason: HideReason::Retrigger,
                });
            }
            return;
        }
        // Css：让前端播放退出动画后调用 panel_hide；兜底计时器到期仍没隐藏就直接隐藏
        self.host.emit_hide_request(PanelHideRequest {
            session_id: old,
            reason: HideReason::Retrigger,
        });
        let me = self.me.clone();
        self.host.schedule(
            EXIT_FALLBACK,
            Box::new(move || {
                let Some(me) = me.upgrade() else { return };
                let stuck = me
                    .inner
                    .lock()
                    .session
                    .as_ref()
                    .is_some_and(|s| s.open.session_id == old && s.phase != Phase::Hiding);
                if stuck {
                    me.hide(old, HideReason::Retrigger, None);
                }
            }),
        );
    }

    fn new_session(&self, open: PanelOpen, rebuilds: u8) -> Session {
        let card = open.geometry.clamp_height(open.estimated_height);
        let window_height = self.window_height_for(&open.geometry, card);
        let window_width = open.geometry.width;
        Session {
            open,
            phase: Phase::Preparing,
            window_height,
            window_width,
            card_height: card,
            stepper: CssHeightStepper::new(),
            deferred: false,
            slack_due: None,
            slack_timer: false,
            key: false,
            ever_key: false,
            dragged: false,
            dragging: false,
            drag_seq: 0,
            follow_pending: false,
            frame_pending: false,
            width_pending: false,
            blur_deferred: false,
            push_until: None,
            in_place: false,
            timed_out: false,
            prepared_at: Instant::now(),
            rebuilds,
            resends: 0,
            attempt: 0,
            page: PanelPage::Warm,
            shell: false,
        }
    }

    /// 这次唤出用的是哪种页面（锁内、`emit_and_arm_timeout` 把页面标记为用过之前调用）：刚等过页面加载的是冷的；
    /// 页面加载后还没用过（回收或启动预热出来的新页面，JIT 是冷的）记为 recycled；其余是用过的热页面
    fn page_for_next(g: &mut Inner) -> PanelPage {
        if std::mem::take(&mut g.next_cold) {
            PanelPage::Cold
        } else if g.used_since_load {
            PanelPage::Warm
        } else {
            PanelPage::Recycled
        }
    }

    fn prepare_fresh(&self, open: PanelOpen, rebuilds: u8) {
        let mut session = self.new_session(open, rebuilds);
        let rect = self.desktop_window_rect(&session);
        let focused = session.open.focused;
        let max_height = session.open.geometry.max_height;
        let prepare = Self::prepare_payload(&session.open);
        let session_id = session.open.session_id;
        let (superseded, shell, handed) = {
            // 旧会话（如果有）被取代：它的 ready / resize / hide 之后都会被忽略，监听随之注销。新会话立即登记，中间不留
            // 「没有会话」的空档——旧会话迟到的离屏回调据此认出自己已不是当前会话（见 finish_hide）。窗口还以 alpha 0 留在
            // 屏幕上（上一个会话 ready 超时）时由新会话接管，整窗放行点击直到这次 ready
            let mut g = self.inner.lock();
            session.page = Self::page_for_next(&mut g);
            // 冷唤出的外壳还在屏幕上（页面加载期间又来了新的 prepare）：新会话接着用这张外壳，ready 时内容淡入；几何变了
            // 就把外壳用弹簧移过去（不在可见的外壳上硬切）
            // 调用方（prepare_attempt）看到外壳时已经拿了 frame_lock，交接排在外壳的显示之后（见那里）
            let shell = g
                .session
                .as_ref()
                .is_some_and(|o| o.shell && o.phase == Phase::TimedOut && g.on_screen == Some(o.open.session_id));
            let mut handed = HandOver::default();
            let old = if shell {
                let prev = g.session.take();
                if let Some(prev) = &prev {
                    handed = self.hand_over_shell(&mut g, &mut session, prev);
                }
                g.session = Some(session);
                prev
            } else {
                g.session.replace(session)
            };
            if let Some(old) = &old
                && old.phase != Phase::Hiding
                && g.on_screen == Some(old.open.session_id)
            {
                g.on_screen = Some(session_id);
            }
            self.host.disarm_guards();
            if shell && let Some(s) = g.session.as_ref() {
                // 外壳一直在屏幕上：新会话接着装关闭监听（ready 时按实测高度重新装）
                self.host
                    .arm_guards(session_id, Self::card_hit(s), s.open.guards, s.key, self.sink());
            }
            let region = if g.visible() {
                self.passthrough_region(rect)
            } else {
                None
            };
            self.host.set_click_through(region);
            (
                old.map(|o| o.open.session_id).filter(|&id| id != session_id),
                shell,
                handed,
            )
        };
        // 被取代的会话（准备中、ready 超时后 alpha 0、淡出刚被立即完成的）就此结束
        self.ended(superseded);
        // Windows 先恢复 WebView2 的可见性与内存目标，再做任何会让前端开始工作的事
        self.host.restore_visibility();
        self.host.set_focusable(focused);
        if shell {
            // 外壳已经（在锁内排队）移到新矩形、按需重新固定了 WebView
            self.note_hand_over(session_id, handed);
        } else {
            // 隐藏状态下按最终宽度定位：前端按这个宽度同步测量
            if let Err(e) = self.host.set_frame(rect) {
                log::warn!("panel prepare set_frame failed: {}", e.message);
            }
            if self.frame_mode == FrameMode::Native {
                match self.host.pin_webview(max_height) {
                    Ok(()) => self.inner.lock().pinned_webview = Some(max_height),
                    Err(e) => log::warn!("panel pin webview failed: {}", e.message),
                }
            }
        }
        self.emit_and_arm_timeout(&prepare, session_id, 0);
    }

    /// 冷唤出的外壳交给新会话（锁内调用，调用方持有 frame_lock）：新会话处在「外壳上、等 ready」，继承窗口当前的 key
    /// 状态；几何与外壳不同（换了锚点 / 视图 / 宽度）时把外壳用矩形弹簧移到新矩形、上限变了同时重新固定 WebView
    /// （[`PanelHost::move_shell`]，只排队，与会话的先后一致）——ready 时只剩高度弹簧与内容淡入，不会在可见的卡片上硬切。
    /// 返回锁外要补的事（[`Self::note_hand_over`]）
    fn hand_over_shell(&self, g: &mut Inner, s: &mut Session, prev: &Session) -> HandOver {
        s.shell = true;
        s.phase = Phase::TimedOut;
        s.page = PanelPage::Cold;
        s.key = prev.key;
        s.ever_key = prev.ever_key;
        let rect = self.desktop_window_rect(s);
        let max_height = s.open.geometry.max_height;
        let moved = !same_rect(rect, self.desktop_window_rect(prev));
        let repin = g.pinned_webview.is_none_or(|h| (h - max_height).abs() >= 0.5);
        if moved || repin {
            self.host
                .move_shell(rect, repin.then_some(max_height), !g.options.reduce_motion);
            if repin {
                g.pinned_webview = Some(max_height);
            }
        }
        HandOver {
            activate: s.open.focused && !s.key,
            key_gained: s.open.focused && s.key,
        }
    }

    /// 交接之后（锁外）：新会话的 perf 补上「窗口已在屏幕上」——卡片早就在屏幕上，ready 时再记会把 `window_ms` 记成 ready
    /// 的时刻、页面还没加载完就结束的会话则根本没有 `window_ms`（冷唤出的性能门禁按 `window_ms`
    /// 统计）；外壳此刻已是 key 而新视图要焦点时补记 `KeyGained`（窗口的 key 状态没有变化，不会再有焦点事件）；
    /// 新视图要焦点而外壳还不是 key 时让它成为 key（之后的焦点事件照常记 `KeyGained`）
    fn note_hand_over(&self, session_id: u64, handed: HandOver) {
        self.host.perf(session_id, PerfStage::WindowShown);
        if handed.key_gained {
            self.host.perf(session_id, PerfStage::KeyGained);
        }
        if handed.activate {
            self.host.activate();
        }
    }

    /// 原地替换（窗口保持可见）。新会话**保留当前位置**：屏幕、卡片左边、固定边与放置方向沿用旧会话，只换新视图的宽度
    /// 与高度上限——会话几何与窗口始终一致，固定交接、外部点击、自身窗口命中都用得上
    fn prepare_in_place(&self, mut open: PanelOpen, rebuilds: u8) {
        let session_id = open.session_id;
        let (prepare, focusable, replaced) = {
            let mut g = self.inner.lock();
            let visible = g.visible();
            // 选路之后锁放开过：旧会话期间可能开始隐藏或被清掉，这时不能原地替换，重新选路
            let old = g
                .session
                .as_ref()
                .filter(|s| visible && matches!(s.phase, Phase::Preparing | Phase::Shown))
                .map(|s| {
                    (
                        s.open.geometry.clone(),
                        s.window_height,
                        s.window_width,
                        s.card_height,
                        s.key,
                        s.ever_key,
                        s.dragged,
                    )
                });
            let Some((old_geometry, window_height, window_width, card_height, key, ever_key, dragged)) = old else {
                drop(g);
                // 外层的 prepare_attempt 已按需拿过 frame_lock（不可重入）：直接重新选路
                self.prepare_routed(open, rebuilds);
                return;
            };
            open.geometry = old_geometry.keep_position(open.geometry.width, open.geometry.cap);
            if dragged {
                // 拖动过的面板：固定边可能贴近工作区边缘，按固定边以外的空间算上限会把新视图压得很矮、只能滚动——上限按
                // 整个工作区算，新卡片伸出工作区时整体推回（ready 与之后的长高都走 fit_card）
                open.geometry.max_height = open.geometry.movable_max_height();
            }
            let prepare = Self::prepare_payload(&open);
            // 按新视图决定可激活性：新视图要焦点（输入 / 剪贴板），或面板此刻就是 key（例如在剪贴板里选了条目去翻译——
            // 与翻译视图被点击后成为 key 的状态相同），保持可激活；否则恢复不激活（Windows 加回 WS_EX_NOACTIVATE）
            let focusable = open.focused || key;
            let mut s = self.new_session(open, rebuilds);
            s.page = Self::page_for_next(&mut g);
            s.window_height = window_height;
            s.card_height = card_height;
            s.key = key;
            s.ever_key = ever_key;
            s.dragged = dragged;
            if self.css_with_margin() {
                // 新卡片的左边只会不变或左移（keep_position），并集从新卡片左边开始
                let new = &s.open.geometry;
                let right = (old_geometry.x + window_width).max(new.x + new.width);
                s.window_width = right - new.x;
            }
            s.in_place = true;
            let replaced = g
                .session
                .replace(s)
                .map(|old| old.open.session_id)
                .filter(|&id| id != session_id);
            // 窗口留在屏幕上，由新会话接管（旧会话不会再隐藏它）
            g.on_screen = Some(session_id);
            // 旧会话的监听注销；窗口与它的点击穿透区域保持到这次 ready
            self.host.disarm_guards();
            (prepare, focusable, replaced)
        };
        // 被原地替换的旧会话就此结束（它的内容随交叉淡入退场，流不延续到新会话）
        self.ended(replaced);
        self.host.set_focusable(focusable);
        self.emit_and_arm_timeout(&prepare, session_id, 0);
    }

    fn prepare_payload(open: &PanelOpen) -> PanelPrepare {
        let g = &open.geometry;
        PanelPrepare {
            session_id: open.session_id,
            view: open.view,
            payload: open.payload.clone(),
            width: g.width,
            max_height: g.max_height,
            placement: g.placement,
            origin_x: g.origin_x,
            focused: open.focused,
            transition: open.transition,
        }
    }

    /// emit prepare 并启动这一轮（`attempt`）的 ready 超时与新页面补发。会话在定位期间已被取消或取代时什么都不做
    fn emit_and_arm_timeout(&self, prepare: &PanelPrepare, session_id: u64, attempt: u32) {
        let fresh = {
            let g = self.inner.lock();
            let current = g
                .session
                .as_ref()
                .is_some_and(|s| s.open.session_id == session_id && s.attempt == attempt);
            if !current {
                return;
            }
            g.loaded_at.is_some_and(|t| t.elapsed() < FRESH_PAGE_WINDOW)
        };
        self.inner.lock().used_since_load = true;
        if let Err(e) = self.host.emit_prepare(prepare) {
            log::warn!("emit panel:prepare failed: {}", e.message);
        }
        let me = self.me.clone();
        self.host.schedule(
            READY_TIMEOUT,
            Box::new(move || {
                if let Some(me) = me.upgrade() {
                    me.on_ready_timeout(session_id, attempt);
                }
            }),
        );
        if fresh {
            self.schedule_resend(session_id, attempt, READY_TIMEOUT + FRESH_PAGE_RESEND_EVERY);
        }
    }

    fn schedule_resend(&self, session_id: u64, attempt: u32, delay: Duration) {
        let me = self.me.clone();
        self.host.schedule(
            delay,
            Box::new(move || {
                if let Some(me) = me.upgrade() {
                    me.resend_prepare(session_id, attempt);
                }
            }),
        );
    }

    // -----------------------------------------------------------------------------------------------------------
    // ready

    /// ready：在命令内同步显示并立即返回，前端随即播放进入动画；关闭监听与点击穿透在锁内排队、由宿主异步
    /// 安装，不推迟返回。会话已过期（被新会话取代、已隐藏）时返回 None。对当前已显示的会话**幂等**：再次 ready
    /// （补发的 prepare 引起）直接返回 `PanelShown`，不重复显示。`PanelShown.focused` 是面板显示后实际的 key 状态
    /// （原地替换时面板此刻已是 key 也算）
    pub fn ready(&self, session_id: u64, content_height: f64) -> Option<PanelShown> {
        // ready 到达（ready → 窗口上屏 → 成为 key → 首帧的拆分从这里起算，所以在显示之前记）。同一阶段
        // 只记第一次：补发引起的重复 ready、过期会话的 ready 都不影响
        self.host.perf(session_id, PerfStage::Ready);
        let _frame = self.frame_lock.lock();
        self.promote_pending_for_ready(session_id);
        let (plan, focused, waited, settle, page, ticket, cold) = {
            let mut g = self.inner.lock();
            let options = g.options;
            // `Boot.prepare` 带的会话：它的 ready 到了，之后不再需要记住
            let boot = g.boot_session.take_if(|id| *id == session_id).is_some();
            let Some(s) = g
                .session
                .as_mut()
                .filter(|s| s.open.session_id == session_id && s.phase != Phase::Hiding)
            else {
                // 已过期（被新会话取代、已隐藏 / 隐藏中、被取消）。页面凭 Boot.prepare 渲染过它时把这一页记为用过
                drop(g);
                if boot {
                    self.note_boot_render(session_id);
                }
                return None;
            };
            if s.phase == Phase::Shown {
                return Some(PanelShown {
                    session_id,
                    focused: s.open.focused || s.key,
                });
            }
            let card = if s.open.view == PanelView::Clipboard {
                placement::clipboard_open_height(content_height, s.open.geometry.max_height)
            } else {
                s.open.geometry.clamp_height(content_height)
            };
            // 原地替换时前端的卡片用 spring.gentle 从旧高度过渡到新高度：窗口的并集要留到这段过渡停下才回收
            let settle = if s.in_place {
                sizing::settle_duration(SpringParams::GENTLE, s.card_height, card)
            } else {
                Duration::ZERO
            };
            // 原地替换拖动过的面板：新卡片伸出工作区时整体推回（Native 帧模式由带边界的高度弹簧推，见 InPlace.bound）
            let free = s.in_place && s.dragged;
            if free && s.open.geometry.fit_card(card) && self.frame_mode == FrameMode::Native {
                s.push_until = Some(Instant::now() + push_settle(ResizeSpring::Gentle, s.card_height, card));
            }
            let geometry = &s.open.geometry;
            s.card_height = card;
            let need = self.window_height_for(geometry, card);
            if s.in_place && self.css_with_margin() {
                // 交叉淡入期间新旧两张卡片都完整可见：窗口取并集，稳定后回收（见 has_slack）
                s.window_height = s.window_height.max(need);
            } else {
                s.window_height = need;
                s.window_width = geometry.width;
            }
            s.stepper.reset(card);
            let was = s.phase;
            s.phase = Phase::Shown;
            let rect = self.desktop_window_rect(s);
            let focused = s.open.focused || s.key;
            let fade = (self.frame_mode == FrameMode::Native).then(|| self.fade_for(options, true));
            let plan = if s.in_place {
                ReadyPlan::InPlace(InPlace {
                    rect,
                    card,
                    placement: geometry.placement,
                    max_height: geometry.max_height,
                    // 宽度要变的交叉淡入：前端先把旧视图淡出（ViewSwitch 的 mode="wait"），窗口宽度等它完全透明再改。
                    // 不是交叉淡入的原地替换（同位置再次触发）宽度本来就相同，这里只是兜底
                    width_delay: if s.open.transition == PanelTransition::Crossfade {
                        Self::crossfade_exit(options)
                    } else {
                        Duration::ZERO
                    },
                    bound: free.then(|| self.vertical_bound(geometry)),
                    animate: !options.reduce_motion,
                    // 新视图要焦点而面板还不是 key：激活
                    activate: s.open.focused && !s.key,
                })
            } else if was == Phase::TimedOut && s.shell {
                ReadyPlan::RevealShell {
                    rect,
                    placement: geometry.placement,
                    fade: self.fade_for(options, true),
                    key: focused,
                    animate: !options.reduce_motion,
                }
            } else if was == Phase::TimedOut {
                ReadyPlan::Reveal {
                    rect,
                    fade,
                    key: focused,
                }
            } else {
                ReadyPlan::Show {
                    rect,
                    opts: ShowOpts {
                        alpha: fade.map_or(ShowAlpha::Opaque, ShowAlpha::FadeIn),
                        key: focused,
                    },
                }
            };
            let waited = s.prepared_at.elapsed();
            let page = s.page;
            // 冷唤出（外壳上的会话）：`prepared_at` 在页面加载时重置，`waited` 就是「页面加载 → ready」——冷唤出内容首帧
            // 的主要组成（用来评估 Boot 里直接带上 prepare 能省多少）。页面还没加载完就 ready 了
            // （`Boot.prepare`）时 `waited` 是「外壳上屏 → ready」
            let cold = (was == Phase::TimedOut && s.shell).then_some((s.resends, g.loaded));
            g.on_screen = Some(session_id);
            // 页面加载完成之前就显示了（Boot.prepare）：页面加载时把这一页记为用过
            if !g.loaded {
                g.boot_used = true;
            }
            (plan, focused, waited, settle, page, self.ticket(), cold)
        };
        let shown = match plan {
            ReadyPlan::Show { rect, opts } => self.host.show(rect, opts, &ticket),
            ReadyPlan::Reveal { rect, fade, key } => self.host.reveal(rect, fade, key, &ticket),
            ReadyPlan::RevealShell {
                rect,
                placement,
                fade,
                key,
                animate,
            } => self.host.reveal_shell(rect, placement, fade, key, animate, &ticket),
            ReadyPlan::InPlace(p) => {
                self.apply_in_place(&p);
                if p.activate {
                    self.host.activate();
                }
                Ok(())
            }
        };
        if let Err(e) = shown {
            if !ticket.valid() {
                // 显示之前又开始了隐藏（截图前隐藏、固定交接……）：会话交给隐藏流程收尾
                log::debug!("panel session={session_id} show superseded by a hide");
                return None;
            }
            log::warn!("panel show failed: {}", e.message);
            let mut g = self.inner.lock();
            if g.session.as_ref().is_some_and(|s| s.open.session_id == session_id) {
                g.session = None;
                if g.on_screen == Some(session_id) {
                    g.on_screen = None;
                }
                self.host.disarm_guards();
                self.host.set_click_through(None);
            }
            return None;
        }
        let slack = {
            let mut g = self.inner.lock();
            let s = g
                .session
                .as_mut()
                .filter(|s| s.open.session_id == session_id && s.phase == Phase::Shown)?;
            // 显示已完成：监听与穿透排队安装（显示期间被并发隐藏时，上面的过滤让这里什么都不装）
            let key = s.key || focused;
            self.host.set_click_through(self.click_region(s));
            self.host
                .arm_guards(session_id, Self::card_hit(s), s.open.guards, key, self.sink());
            self.arm_slack(s, settle)
        };
        if slack {
            self.schedule_slack(session_id, settle.max(CSS_SLACK_RECLAIM));
        }
        // 原生窗口已在屏幕上（同步显示返回；原地替换时窗口一直在屏幕上，记的是新视图开始过渡的时刻）
        self.host.perf(session_id, PerfStage::WindowShown);
        self.host.perf_page(session_id, page);
        match cold {
            Some((resends, true)) => log::info!(
                "panel cold show: ready {} ms after the page loaded under the native shell (resends {resends}) session={session_id}",
                waited.as_millis()
            ),
            Some((_, false)) => log::info!(
                "panel cold show: ready {} ms after the native shell went up, before the page finished loading (Boot.prepare) session={session_id}",
                waited.as_millis()
            ),
            None => log::debug!(
                "panel session={session_id} shown {} ms after prepare",
                waited.as_millis()
            ),
        }
        Some(PanelShown { session_id, focused })
    }

    /// 原地替换的窗口动作。
    /// - Css：窗口立即改到新旧两张卡片的并集（交叉淡入期间都不被裁切），卡片稳定后由富余区回收收回；
    /// - Native（窗口即卡片）：高度始终走 spring.gentle（固定边不动）。宽度 / 左边要变时**不在这里改**——旧视图此刻还
    ///   完全不透明，立即改宽度会把它裁掉或按新宽度重排；改为 [`WidthSwitch`]：`width_delay`（旧视图淡出完、内容完全
    ///   透明的那一刻）之后由 displayLink 在一帧里改左边与宽度，WebView 的重新固定也放在这一帧。宽度不变时只在上限变大
    ///   时重新固定 WebView（变小时保留较高的 WebView，窗口裁掉多余部分，不必让可见的内容等一次 WebContent 重排）
    fn apply_in_place(&self, p: &InPlace) {
        if self.frame_mode == FrameMode::Css {
            if let Err(e) = self.host.set_frame(p.rect) {
                log::warn!("panel in-place resize failed: {}", e.message);
            }
            return;
        }
        let Some(current) = self.host.window_frame() else {
            // 取不到窗口矩形（窗口正在重建）：直接放到新矩形
            if let Err(e) = self.host.set_frame(p.rect) {
                log::warn!("panel in-place resize failed: {}", e.message);
            }
            return;
        };
        let pinned = self.inner.lock().pinned_webview;
        let horizontal_change = (current.x - p.rect.x).abs() >= 0.5 || (current.width - p.rect.width).abs() >= 0.5;
        if horizontal_change {
            let pin = pinned
                .is_none_or(|h| (h - p.max_height).abs() >= 0.5)
                .then_some(p.max_height);
            self.host.switch_width(WidthSwitch {
                x: p.rect.x,
                width: p.rect.width,
                delay: p.width_delay,
                pin,
            });
            if pin.is_some() {
                self.inner.lock().pinned_webview = Some(p.max_height);
            }
        } else if pinned.is_none_or(|h| p.max_height > h + 0.5) {
            match self.host.pin_webview(p.max_height) {
                Ok(()) => self.inner.lock().pinned_webview = Some(p.max_height),
                Err(e) => log::warn!("panel pin webview failed: {}", e.message),
            }
        }
        self.host
            .animate_height(p.card, p.placement, p.bound, p.animate, ResizeSpring::Gentle);
    }

    /// 前端第一个 opacity > 0 的帧（`panel_painted`）
    pub fn painted(&self, session_id: u64) {
        let current = self
            .inner
            .lock()
            .session
            .as_ref()
            .is_some_and(|s| s.open.session_id == session_id);
        if current {
            self.host.perf(session_id, PerfStage::Shown);
        }
    }

    /// 补发 prepare（新页面上第一次 emit 时监听可能还没注册）：只在这一轮的 ready 已经超时、会话仍在等 ready 时才发，
    /// 每帧一次直到 ready 或补满 [`FRESH_PAGE_RESEND_MAX`] 次（同一时刻只有一个补发计时器）
    fn resend_prepare(&self, session_id: u64, attempt: u32) {
        let prepare = {
            let mut g = self.inner.lock();
            let Some(s) = g.session.as_mut().filter(|s| {
                s.open.session_id == session_id
                    && s.attempt == attempt
                    && matches!(s.phase, Phase::Preparing | Phase::TimedOut)
                    && s.resends < FRESH_PAGE_RESEND_MAX
            }) else {
                return;
            };
            // 超时还没发生（计时器线程上晚到）：下一帧再看
            let prepare = s.timed_out.then(|| Self::prepare_payload(&s.open));
            if prepare.is_some() {
                s.resends += 1;
            }
            prepare
        };
        if let Some(prepare) = prepare {
            log::trace!("panel page just loaded; resending prepare session={session_id}");
            if let Err(e) = self.host.emit_prepare(&prepare) {
                log::warn!("emit panel:prepare failed: {}", e.message);
            }
        }
        self.schedule_resend(session_id, attempt, FRESH_PAGE_RESEND_EVERY);
    }

    /// 超时仍没收到 ready，以估计高度、alpha 0 先显示。Windows / Linux 的 alpha 不适用（卡片本身还是
    /// opacity 0），所以同时整窗放行点击；窗口不能透明时不提前显示（会是一块可见的空卡片），只等 ready 或看门狗。
    /// 页面重载后重放的轮次（窗口已经以 alpha 0 在屏幕上）只重新启动看门狗。
    ///
    /// 冷唤出外壳上的会话不算 ready 超时：外壳早已上屏，新页面上 40 ms 内等不到 ready 是冷页面的常态（监听在
    /// 页面加载后 70–130 ms 才注册），不记 `PerfStage::ReadyTimeout`、不出 `perf warn`（只记 debug 日志）——否则空闲卸载
    /// 之后的每一次唤出都会在日志与门禁统计里报一次假告警。看门狗照常启动
    fn on_ready_timeout(&self, session_id: u64, attempt: u32) {
        let (show, shell) = {
            let mut g = self.inner.lock();
            let Some(s) = g.session.as_mut().filter(|s| {
                s.open.session_id == session_id
                    && s.attempt == attempt
                    && !s.timed_out
                    && matches!(s.phase, Phase::Preparing | Phase::TimedOut)
            }) else {
                return;
            };
            s.timed_out = true;
            let shell = s.shell && s.phase == Phase::TimedOut;
            let show = if s.phase == Phase::TimedOut || s.in_place || self.opaque {
                None
            } else {
                s.phase = Phase::TimedOut;
                let rect = self.desktop_window_rect(s);
                g.on_screen = Some(session_id);
                self.host.set_click_through(self.passthrough_region(rect));
                Some((rect, self.ticket()))
            };
            (show, shell)
        };
        if shell {
            log::debug!(
                "panel session={session_id}: no ready {} ms after the page loaded under the native shell (cold page)",
                READY_TIMEOUT.as_millis()
            );
        } else {
            log::warn!(
                "perf warn ready_timeout session={session_id} after {} ms",
                READY_TIMEOUT.as_millis()
            );
            self.host.perf(session_id, PerfStage::ReadyTimeout);
        }
        if let Some((rect, ticket)) = show {
            let opts = ShowOpts {
                alpha: ShowAlpha::Transparent,
                key: false,
            };
            if let Err(e) = self.host.show(rect, opts, &ticket)
                && ticket.valid()
            {
                log::warn!("panel timeout show failed: {}", e.message);
                // 窗口没有显示出来：退回准备中，迟到的 ready 按正常显示处理（票作废时会话交给隐藏流程收尾）
                let mut g = self.inner.lock();
                if let Some(s) = g
                    .session
                    .as_mut()
                    .filter(|s| s.open.session_id == session_id && s.phase == Phase::TimedOut)
                {
                    s.phase = Phase::Preparing;
                    if g.on_screen == Some(session_id) {
                        g.on_screen = None;
                    }
                    self.host.set_click_through(None);
                }
            }
        }
        let me = self.me.clone();
        self.host.schedule(
            READY_WATCHDOG,
            Box::new(move || {
                if let Some(me) = me.upgrade() {
                    me.on_ready_watchdog(session_id, attempt);
                }
            }),
        );
    }

    /// 超时后 1 s 仍没有迟到的 ready → WebView 失效，销毁并重建面板，重放这次 prepare
    fn on_ready_watchdog(&self, session_id: u64, attempt: u32) {
        let replay = {
            let mut g = self.inner.lock();
            let stuck = g.session.as_ref().is_some_and(|s| {
                s.open.session_id == session_id
                    && s.attempt == attempt
                    && s.timed_out
                    && matches!(s.phase, Phase::Preparing | Phase::TimedOut)
            });
            if !stuck {
                return;
            }
            let Some(s) = g.session.take() else { return };
            self.revoke_shows();
            g.on_screen = None;
            g.pinned_webview = None;
            g.loaded = false;
            g.loaded_at = None;
            self.host.disarm_guards();
            self.host.set_click_through(None);
            if s.rebuilds >= MAX_REBUILDS {
                None
            } else {
                g.creating = true;
                g.attached = false;
                g.create_started = Some(Instant::now());
                // 外壳下的页面 SHELL_LOAD_WAIT 内没加载完：空卡片已经在屏幕上停了 2 s，重建之后不再
                // 显示第二张外壳——新页面加载后这次 prepare 走普通的显示（有内容才出现），外壳的寿命封顶一次等待
                g.pending_no_shell = s.shell;
                g.pending = Some((s.open, s.rebuilds + 1));
                Some(())
            }
        };
        if replay.is_some() {
            log::warn!("panel webview unresponsive (no ready 1 s after timeout), rebuilding session={session_id}");
        } else {
            log::error!("panel webview still unresponsive after rebuild; dropping session={session_id}");
            self.ended([session_id]);
        }
        self.host.destroy();
        if replay.is_some() {
            self.spawn_create_after_flag();
        } else {
            self.inner.lock().creating = false;
        }
    }

    // -----------------------------------------------------------------------------------------------------------
    // resize

    /// 卡片目标高度变化
    ///
    /// `spring`：Native 帧模式窗口高度的弹簧——浮层预留引起的长高与释放后的回收用 snappy（前端
    /// `PanelCard` 与 Css 帧卡片高度同一规则判定），其余 gentle；Css 帧模式的窗口阶梯不受影响
    pub fn resize(&self, session_id: u64, content_height: f64, spring: ResizeSpring) {
        self.resize_to(session_id, Some(content_height), spring);
    }

    /// `content_height` 为 None：推迟的放大到期，按最近一次上报的卡片高度执行（不经过「高度没变」的提前返回）
    fn resize_to(&self, session_id: u64, content_height: Option<f64>, spring: ResizeSpring) {
        enum Act {
            Native(f64, Placement, Option<VerticalBound>, bool),
            Frame(Rect),
            /// 推迟放大；拖动过的卡片被推回时窗口先移到新位置
            Defer(Duration, Option<Rect>),
            Keep,
        }
        let _frame = self.frame_lock.lock();
        // 拖动过的 Native 面板，推回的弹簧可能还在走（`push_until`）：推回进行中又变矮时弹簧从窗口当前的位置继续——先按窗口
        // 当前位置同步固定边，几何才与弹簧的终点一致。其余时候窗口与几何一致，不读窗口（读要同步等主线程；Css 帧模式的窗口
        // 位置由这里直接设置，几何本来就是准的）
        let window = if self.frame_mode == FrameMode::Native {
            let pushing = self.inner.lock().session.as_ref().is_some_and(|s| {
                s.open.session_id == session_id
                    && s.phase == Phase::Shown
                    && !s.dragging
                    && s.push_until.is_some_and(|t| Instant::now() < t)
            });
            if pushing { self.read_window() } else { None }
        } else {
            None
        };
        let (act, slack) = {
            let mut g = self.inner.lock();
            let reduce = g.options.reduce_motion;
            let Some(s) = g
                .session
                .as_mut()
                .filter(|s| s.open.session_id == session_id && s.phase == Phase::Shown)
            else {
                return;
            };
            if let Some((frame, screen)) = window.filter(|_| !s.dragging) {
                self.follow_frame(s, frame, screen);
            }
            let geometry = s.open.geometry.clone();
            let card = match content_height {
                Some(content) => {
                    let card = if s.open.view == PanelView::Clipboard {
                        // 剪贴板会话内锁定高度：搜索 / 过滤导致结果变少时不缩小，只有新条目插入可以让它变高
                        let cap = placement::CLIPBOARD_MAX_HEIGHT.min(geometry.max_height);
                        geometry.clamp_height(content).min(cap).max(s.card_height)
                    } else {
                        geometry.clamp_height(content)
                    };
                    // 高度没变：已排队的推迟放大（如果有）到期后会按最新高度执行
                    if (card - s.card_height).abs() < 0.25 {
                        return;
                    }
                    card
                }
                None => {
                    if !s.deferred {
                        return;
                    }
                    s.deferred = false;
                    s.card_height
                }
            };
            let before = std::mem::replace(&mut s.card_height, card);
            // 拖动过的面板（拖动已结束）：卡片伸出工作区时整体推回（见 PanelGeometry::fit_card）。高度上限是前端排版用的
            // 那个，会话内不变
            let free = s.dragged && !s.dragging;
            let pushed = free && s.open.geometry.fit_card(card);
            // Css 帧模式改窗口：用户正在拖动时推迟到拖动结束（不和系统的拖动抢位置，窗口位置此刻以系统为准）
            let frame = |me: &Self, s: &mut Session| {
                if s.dragging {
                    s.frame_pending = true;
                    Act::Keep
                } else {
                    Act::Frame(me.desktop_window_rect(s))
                }
            };
            let act = match self.frame_mode {
                // 用户正在拖动：高度弹簧推迟到拖动结束（逐帧 setFrame 会和系统的拖动抢位置——macOS 的窗口拖动由 WindowServer
                // 移动窗口，期间读到的窗口位置可能滞后），松开后从窗口当前高度继续
                FrameMode::Native if s.dragging => {
                    s.window_height = card;
                    s.frame_pending = true;
                    Act::Keep
                }
                FrameMode::Native => {
                    s.window_height = card;
                    let bound = free.then(|| self.vertical_bound(&s.open.geometry));
                    if pushed {
                        s.push_until = Some(Instant::now() + push_settle(spring, before, card));
                    }
                    Act::Native(card, geometry.placement, bound, !reduce)
                }
                FrameMode::Css if self.opaque => {
                    s.window_height = card;
                    frame(self, s)
                }
                // Above：窗口已是上限高度，卡片在窗口内由 CSS 跟随（见模块注释）；推回时整个窗口下移
                FrameMode::Css if geometry.placement == Placement::Above => {
                    if pushed {
                        frame(self, s)
                    } else {
                        Act::Keep
                    }
                }
                FrameMode::Css => {
                    let max_window = geometry.max_height + 2.0 * self.margin;
                    match s
                        .stepper
                        .next(card, s.window_height, self.margin, max_window, Instant::now())
                    {
                        CssResize::Keep if pushed => frame(self, s),
                        CssResize::Keep => Act::Keep,
                        CssResize::Set(h) => {
                            s.window_height = h;
                            frame(self, s)
                        }
                        CssResize::Defer(wait) if !s.deferred => {
                            s.deferred = true;
                            Act::Defer(wait, pushed.then(|| self.desktop_window_rect(s)))
                        }
                        CssResize::Defer(_) if pushed => frame(self, s),
                        CssResize::Defer(_) => Act::Keep,
                    }
                }
            };
            self.sync_hit(s);
            (act, self.arm_slack(s, CSS_SLACK_RECLAIM))
        };
        match act {
            Act::Native(h, placement, bound, animate) => self.host.animate_height(h, placement, bound, animate, spring),
            Act::Frame(rect) => {
                if let Err(e) = self.host.set_frame(rect) {
                    log::warn!("panel resize failed: {}", e.message);
                }
            }
            Act::Defer(wait, rect) => {
                if let Some(rect) = rect
                    && let Err(e) = self.host.set_frame(rect)
                {
                    log::warn!("panel resize failed: {}", e.message);
                }
                let me = self.me.clone();
                self.host.schedule(
                    wait,
                    Box::new(move || {
                        if let Some(me) = me.upgrade() {
                            me.resize_to(session_id, None, ResizeSpring::Gentle);
                        }
                    }),
                );
            }
            Act::Keep => {}
        }
        if slack {
            self.schedule_slack(session_id, CSS_SLACK_RECLAIM);
        }
    }

    /// 视图内切换改宽度：输入 → 翻译这类同一会话里的视图切换由前端发起
    /// （`StartAck.width`），Rust 不知道新视图何时就位。前端在 ViewSwitch 交换的那次提交（旧内容已完全透明、新视图
    /// 还是 opacity 0）调用 `panel_set_width`，这里**立即**改窗口：
    /// - 位置按 [`PanelGeometry::keep_position`] 保持（左边不动，右边越出工作区时整体左移夹回），高度上限不变；
    /// - Native：displayLink 下一帧在一帧内改左边与宽度（与进行中的高度弹簧合成同一个 setFrame）；
    /// - Css（带透明边距）：窗口取新旧两张卡片的并集，卡片稳定后随富余区回收；不透明窗口直接改到新宽度。
    ///
    /// 宽度没变时什么都不做——Rust 发起的交叉淡入（`transition = crossfade` 的 prepare）在 ready 时已经按新宽度
    /// 安排好了，前端在交换时照样调用一次也无妨。只对当前已显示的会话生效
    pub fn set_width(&self, session_id: u64, width: f64) {
        enum Act {
            Native(WidthSwitch),
            Frame(Rect),
            Keep,
        }
        let _frame = self.frame_lock.lock();
        let (act, slack) = {
            let mut g = self.inner.lock();
            let Some(s) = g
                .session
                .as_mut()
                .filter(|s| s.open.session_id == session_id && s.phase == Phase::Shown)
            else {
                return;
            };
            let old = s.open.geometry.clone();
            let mut next = old.keep_position(width, old.cap);
            // 视图内切换不换 prepare：前端的高度上限不变（拖动过的面板固定边可能贴近工作区边缘，按位置重算会比前端的小，
            // 卡片会被窗口截掉）
            next.max_height = old.max_height;
            if (next.width - old.width).abs() < 0.5 && (next.x - old.x).abs() < 0.5 {
                return;
            }
            let old_right = old.x + s.window_width;
            s.open.geometry = next;
            let g2 = &s.open.geometry;
            s.window_width = if self.css_with_margin() {
                // 新卡片的左边只会不变或左移（keep_position），并集从新卡片左边开始
                old_right.max(g2.x + g2.width) - g2.x
            } else {
                g2.width
            };
            let rect = self.desktop_window_rect(s);
            let act = if s.dragging {
                // 用户正在拖动：窗口位置以系统为准，改窗口推迟到拖动结束
                match self.frame_mode {
                    FrameMode::Native => s.width_pending = true,
                    FrameMode::Css => s.frame_pending = true,
                }
                Act::Keep
            } else if self.frame_mode == FrameMode::Native {
                Act::Native(WidthSwitch {
                    x: rect.x,
                    width: rect.width,
                    delay: Duration::ZERO,
                    // 高度上限没变（同一固定边、同一上限），WebView 不必重新固定
                    pin: None,
                })
            } else {
                Act::Frame(rect)
            };
            self.host.update_guard_card(Self::card_hit(s));
            self.host.set_click_through(self.click_region(s));
            (act, self.arm_slack(s, CSS_SLACK_RECLAIM))
        };
        match act {
            Act::Native(switch) => self.host.switch_width(switch),
            Act::Frame(rect) => {
                if let Err(e) = self.host.set_frame(rect) {
                    log::warn!("panel set_width failed: {}", e.message);
                }
            }
            Act::Keep => {}
        }
        if slack {
            self.schedule_slack(session_id, CSS_SLACK_RECLAIM);
        }
    }

    /// 卡片高度稳定 [`CSS_SLACK_RECLAIM`]（原地替换后还要等前端卡片的高度弹簧停下）后，把窗口收回到卡片 + 2·m、
    /// 宽度收回到卡片宽度。Below / Center 的卡片贴住窗口顶边、左边贴住窗口左边，一次 SetWindowPos 只动右边与底边，卡片
    /// 不会跳；Above 的窗口高度是会话上限，只收回宽度（见 [`Self::has_slack`]）
    fn reclaim_slack(&self, session_id: u64) {
        enum Step {
            Later(Duration),
            Apply(Rect),
        }
        let _frame = self.frame_lock.lock();
        let step = {
            let mut g = self.inner.lock();
            let Some(s) = g.session.as_mut().filter(|s| s.open.session_id == session_id) else {
                return;
            };
            let due = match s.slack_due {
                Some(due) if s.phase == Phase::Shown => due,
                _ => {
                    s.slack_timer = false;
                    s.slack_due = None;
                    return;
                }
            };
            let now = Instant::now();
            if now < due {
                Step::Later(due - now)
            } else if s.dragging {
                // 用户正在拖动：窗口位置以系统为准，回收等拖动结束之后
                Step::Later(CSS_SLACK_RECLAIM)
            } else {
                s.slack_timer = false;
                s.slack_due = None;
                // 推迟的放大还没执行（窗口比卡片小）：交给它
                if s.deferred || !self.has_slack(s) {
                    return;
                }
                let width = s.open.geometry.width;
                s.window_width = width;
                if s.open.geometry.placement != Placement::Above {
                    s.window_height = s.window_height.min(s.card_height + 2.0 * self.margin);
                }
                self.host.set_click_through(self.click_region(s));
                Step::Apply(self.desktop_window_rect(s))
            }
        };
        match step {
            Step::Later(wait) => self.schedule_slack(session_id, wait),
            Step::Apply(rect) => {
                if let Err(e) = self.host.set_frame(rect) {
                    log::warn!("panel reclaim slack failed: {}", e.message);
                }
            }
        }
    }

    // -----------------------------------------------------------------------------------------------------------
    // hide

    /// emit `panel:hide_request`（前端先播放退出动画，再调用 `panel_hide`）；只对当前可见的会话。冷唤出的外壳还在等 ready
    /// 时前端可能还没加载、走不了退出流程：直接把外壳淡出收起，再补发 `hide_request`（例如外壳期间再按一次剪贴板
    /// 快捷键的 Toggle）
    pub fn request_hide(&self, reason: HideReason) {
        let target = {
            let g = self.inner.lock();
            g.session
                .as_ref()
                .filter(|s| g.visible() && matches!(s.phase, Phase::Shown | Phase::TimedOut))
                .map(|s| (s.open.session_id, s.shell && s.phase == Phase::TimedOut))
        };
        match target {
            Some((session_id, true)) => {
                log::info!("panel hide session={session_id} reason={reason:?} origin=rust (native shell)");
                self.hide_now_and_notify(session_id, reason);
            }
            Some((session_id, false)) => {
                log::debug!("panel hide session={session_id} reason={reason:?} origin=rust");
                self.host.emit_hide_request(PanelHideRequest { session_id, reason });
            }
            None => {}
        }
    }

    /// 只对 `session_id` emit `hide_request`：它仍是当前已显示的会话时才生效（关闭监听的回调、固定交接——期间来了新的
    /// prepare 时，新会话不受影响）
    ///
    /// 冷唤出外壳上的会话（页面还没 ready，前端接不住 hide_request）：关闭监听（外壳期间的全局 Esc、点击外部）的回调直接
    /// 原生收起外壳并补发 hide_request（同 [`Self::request_hide`]）
    pub fn request_hide_for(&self, session_id: u64, reason: HideReason) {
        let current = {
            let g = self.inner.lock();
            g.session
                .as_ref()
                .filter(|s| g.visible() && s.open.session_id == session_id)
                .and_then(|s| match s.phase {
                    Phase::Shown => Some(false),
                    Phase::TimedOut if s.shell => Some(true),
                    _ => None,
                })
        };
        match current {
            Some(false) => {
                log::debug!("panel hide session={session_id} reason={reason:?} origin=rust");
                self.host.emit_hide_request(PanelHideRequest { session_id, reason });
            }
            Some(true) => {
                log::info!("panel hide session={session_id} reason={reason:?} origin=guard (native shell)");
                self.hide_now_and_notify(session_id, reason);
            }
            None => {}
        }
    }

    /// 只隐藏、不通知前端：Native 帧模式淡出窗口 alpha（粘贴 / 截图 / 固定时立即），Css 帧模式立即隐藏。`panel_hide`
    /// 命令与 Rust 的调用方都经 `WindowManager::panel_hide_now` 走 [`Self::hide_now_and_notify`]；这里留给控制器内部与
    /// 单测。返回这次调用是否开始隐藏 `session_id`（它是当前会话、且不在隐藏中）
    pub fn hide_now(&self, session_id: u64, reason: HideReason) -> bool {
        let fade = {
            let g = self.inner.lock();
            let immediate = matches!(reason, HideReason::Paste | HideReason::Capture | HideReason::Pin);
            (self.frame_mode == FrameMode::Native && !immediate && g.visible()).then(|| self.fade_for(g.options, false))
        };
        self.hide(session_id, reason, fade)
    }

    /// 与 [`Self::hide_now`] 相同，隐藏开始后再补发 `hide_request(reason)`：由 Rust 发起的隐藏（粘贴后立即隐藏等，
    /// `WindowManager::panel_hide_now`）前端并不知道，视图会一直挂在离屏的页面里——计时器、Observer、CSS 动画照跑，
    /// 下一次唤出之前 WebContent 一直有唤醒。补发之后前端照常走退出流程（卸载视图、`<html data-hidden>`）；
    /// 前端自己发起的 `panel_hide` 收到的这次回声被忽略（阶段已是 exiting / hidden，或会话已清空），它随后的
    /// `panel_hide` 对已在隐藏的会话无效
    pub fn hide_now_and_notify(&self, session_id: u64, reason: HideReason) {
        if self.hide_now(session_id, reason) {
            self.host.emit_hide_request(PanelHideRequest { session_id, reason });
        }
    }

    fn hide(&self, session_id: u64, reason: HideReason, fade: Option<Fade>) -> bool {
        self.hide_notify(session_id, reason, fade, None)
    }

    /// 截图前：保证截图期间屏幕上没有面板、之后也不会冒出来。
    /// - 可见的会话（含 ready 超时后的 alpha 0、原地替换中的）立即隐藏（不播放动画），等窗口真正离屏（最多 `timeout`）；
    /// - 正在淡出的会话立即完成淡出并等它离屏；
    /// - 还在准备、窗口不在屏幕上的会话**取消**（它的 ready 到达时返回 None），排队（先退出再进入）与暂存（窗口加载中）
    ///   的 prepare 一并作废——OCR 流程会在截图之后重新 prepare。
    ///
    /// 被隐藏或取消的会话随后补发 `hide_request(capture)`：前端照常走退出流程（停掉 rAF / ResizeObserver、清空大列表），
    /// 它随后的 `panel_hide` 对已结束的会话无效。返回窗口是否在屏幕上、已被隐藏（调用方据此等合成器出帧）
    pub fn hide_for_capture(&self, timeout: Duration) -> bool {
        enum Act {
            Nothing,
            Hide(u64),
            Wait(std::sync::mpsc::Receiver<()>),
            Cancel(u64),
        }
        let mut dropped = Vec::new();
        let act = {
            let mut g = self.inner.lock();
            if let Some(q) = g.queued.take() {
                log::debug!("capture drops queued prepare session={}", q.session_id);
                dropped.push(q.session_id);
            }
            if let Some((p, _)) = g.pending.take() {
                log::debug!("capture drops pending prepare session={}", p.session_id);
                dropped.push(p.session_id);
            }
            let visible = g.visible();
            match g.session.as_ref().map(|s| (s.open.session_id, s.phase)) {
                None => Act::Nothing,
                Some((id, Phase::Hiding)) if visible => {
                    let (tx, rx) = std::sync::mpsc::sync_channel(1);
                    g.exit_waiter = Some((id, tx));
                    Act::Wait(rx)
                }
                Some((_, Phase::Hiding)) => Act::Nothing,
                Some((id, _)) if visible => Act::Hide(id),
                Some((id, _)) => {
                    g.session = None;
                    self.revoke_shows();
                    self.host.disarm_guards();
                    self.host.set_click_through(None);
                    Act::Cancel(id)
                }
            }
        };
        // 作废的排队 / 暂存 prepare 从未显示过，它们的会话就此结束
        self.ended(dropped);
        let wait = |rx: &std::sync::mpsc::Receiver<()>| {
            if rx.recv_timeout(timeout).is_err() {
                log::warn!(
                    "panel did not leave the screen within {} ms before capture",
                    timeout.as_millis()
                );
            }
        };
        match act {
            Act::Nothing => false,
            Act::Hide(session_id) => {
                let (tx, rx) = std::sync::mpsc::sync_channel(1);
                self.hide_notify(session_id, HideReason::Capture, None, Some(tx));
                wait(&rx);
                self.host.emit_hide_request(PanelHideRequest {
                    session_id,
                    reason: HideReason::Capture,
                });
                true
            }
            Act::Wait(rx) => {
                // Native 帧模式的淡出立即完成（orderOut + 回调）；Css 帧模式的离屏已经投递，等它执行完
                self.host.finish_exit();
                wait(&rx);
                true
            }
            Act::Cancel(session_id) => {
                log::debug!("capture cancels preparing panel session={session_id}");
                self.ended([session_id]);
                self.host.emit_hide_request(PanelHideRequest {
                    session_id,
                    reason: HideReason::Capture,
                });
                false
            }
        }
    }

    /// 开始隐藏 `session_id`；它不是当前会话或已在隐藏时什么都不做，返回 false
    fn hide_notify(
        &self,
        session_id: u64,
        reason: HideReason,
        fade: Option<Fade>,
        notify: Option<std::sync::mpsc::SyncSender<()>>,
    ) -> bool {
        let (prev, took_focus, remember) = {
            let mut g = self.inner.lock();
            let Some(s) = g
                .session
                .as_mut()
                .filter(|s| s.open.session_id == session_id && s.phase != Phase::Hiding)
            else {
                return false;
            };
            s.phase = Phase::Hiding;
            let took_focus = s.open.focused || s.ever_key;
            let remember = (s.dragged && s.open.view == PanelView::Clipboard).then(|| {
                let g = &s.open.geometry;
                let card = g.card_rect(s.card_height);
                (g.screen.id.clone(), Point { x: card.x, y: card.y })
            });
            let prev = s.open.prev_app.clone();
            // 还没在主线程上执行的显示作废（离屏操作排在它们之后，见 ShowTicket）
            self.revoke_shows();
            self.host.disarm_guards();
            self.host.set_click_through(None);
            (prev, took_focus, remember)
        };
        // 隐藏开始即通知（退出动画一开始就取消请求，不再等 140 ms 的淡出播完才停止计费）；离屏完毕时
        // finish_hide 再通知一次
        self.ended([session_id]);
        let me = self.me.clone();
        self.host.hide(
            fade,
            Box::new(move || {
                if let Some(me) = me.upgrade() {
                    me.finish_hide(session_id);
                }
                if let Some(tx) = notify {
                    let _ = tx.send(());
                }
            }),
        );
        if let Some(prev) = prev.filter(|_| took_focus && self.should_restore_focus(reason)) {
            self.host.restore_focus(&prev);
        }
        if let Some((screen_id, top_left)) = remember {
            self.host.remember_position(&screen_id, top_left);
        }
        true
    }

    /// 取得过焦点的面板隐藏后，Windows / X11 要不要把焦点还给原应用。
    /// - Esc / 关闭按钮 / ⌘W / 再次触发 / 其他：还原（用户取消了这次操作）；
    /// - 失焦（剪贴板 / 输入视图「点击外部关闭」）：焦点落在桌面、任务栏这类外壳上（或没有前台窗口）时还原；用户点中的
    ///   是别的应用（包括 Wisp 自己的 pinned / 设置窗口）时保持不动，那是用户要去的地方；
    /// - 粘贴、截图、固定、toast 自然消失：不还原（这些流程各有负责方）
    fn should_restore_focus(&self, reason: HideReason) -> bool {
        if !self.host.restores_focus() {
            return false;
        }
        match reason {
            HideReason::Escape
            | HideReason::CloseButton
            | HideReason::CommandW
            | HideReason::Toggle
            | HideReason::Other => true,
            HideReason::Blur => self.host.focus_on_shell(),
            _ => false,
        }
    }

    /// 固定交接前：`session_id` 仍是当前可见的会话时，让它进入隐藏状态并注销监听，返回 true（窗口由调用方在同一帧里
    /// 离屏，之后调用 [`Self::finish_external_hide`]）。会话已被取代（交接期间来了新的 prepare）或已在隐藏时返回 false，
    /// 新会话不受影响
    pub fn begin_external_hide_for(&self, session_id: u64) -> bool {
        let mut g = self.inner.lock();
        let visible = g.visible();
        let Some(s) = g
            .session
            .as_mut()
            .filter(|s| visible && s.open.session_id == session_id && s.phase != Phase::Hiding)
        else {
            return false;
        };
        s.phase = Phase::Hiding;
        self.revoke_shows();
        self.host.disarm_guards();
        self.host.set_click_through(None);
        drop(g);
        // 隐藏开始即通知（同 hide_notify）。固定交接时流的所有权早在 pinned 窗口创建时就已转走，
        // 编排器据此收尾的只是仍属于面板会话的请求
        self.ended([session_id]);
        true
    }

    /// 由 Rust 发起的隐藏（[`Self::begin_external_hide_for`]）完成：清掉会话，再补发 `hide_request(reason)`，让前端照常走
    /// 退出流程、停掉自己的工作
    pub fn finish_external_hide(&self, session_id: u64, reason: HideReason) {
        self.finish_hide(session_id);
        self.host.emit_hide_request(PanelHideRequest { session_id, reason });
    }

    /// 窗口已离屏。`session_id` 仍是当前会话时：清掉会话，按需执行排队的 prepare 或安排空闲卸载。已被新会话取代时
    /// （例如 Linux 的离屏确认在新会话 prepare 之后才到）只记下窗口已离屏——排队的 prepare、WebView 可见性与空闲卸载
    /// 都属于新会话，不能被旧会话的回调动到
    pub fn finish_hide(&self, session_id: u64) {
        // 窗口已离开屏幕（粘贴等它再发键）
        self.offscreen.send_replace(session_id);
        let (queued, options, idle_gen, old_timer) = {
            let mut g = self.inner.lock();
            if g.on_screen == Some(session_id) {
                g.on_screen = None;
            }
            if g.exit_waiter.as_ref().is_some_and(|(id, _)| *id == session_id)
                && let Some((_, tx)) = g.exit_waiter.take()
            {
                let _ = tx.try_send(());
            }
            let current = g.session.as_ref().is_some_and(|s| s.open.session_id == session_id);
            if !current {
                drop(g);
                self.ended([session_id]);
                return;
            }
            if let Some(s) = g.session.take() {
                let geometry = &s.open.geometry;
                g.last_card = Some(LastCard {
                    screen: geometry.screen.clone(),
                    rect: geometry.card_rect(s.card_height),
                });
            }
            g.on_screen = None;
            let old_timer = Self::take_idle_timer(&mut g);
            (g.queued.take(), g.options, g.idle_gen, old_timer)
        };
        if let Some(t) = old_timer {
            t.cancel();
        }
        self.ended([session_id]);
        if let Some(open) = queued {
            // 可能在主线程上（Native 淡出完成的回调）：会话刚清空、不会有外壳，直接选路，不碰 frame_lock
            self.prepare_routed(open, 0);
            return;
        }
        self.host.release_visibility(options.low_memory_when_hidden);
        let used = self.inner.lock().used_since_load;
        if let Some((after, _)) = idle_plan(&options, used, Duration::ZERO) {
            self.arm_idle(idle_gen, after, after);
        }
    }

    /// 排空闲计时器：`delay` 之后看一次此时（隐藏了 `at` 之后）该做哪个空闲动作。`at` 是计时器链的**逻辑**时长
    /// （各段 `delay` 之和，不读时钟）：先回收、再卸载与回收被推迟时据此判断哪个动作到期。只保留最新的一个计时器
    fn arm_idle(&self, idle_gen: u64, delay: Duration, at: Duration) {
        let me = self.me.clone();
        let timer = self.host.schedule_cancellable(
            delay,
            Box::new(move || {
                if let Some(me) = me.upgrade() {
                    me.on_idle_timer(idle_gen, at);
                }
            }),
        );
        let stale = {
            let mut g = self.inner.lock();
            if g.idle_gen == idle_gen {
                g.idle_timer.replace(timer)
            } else {
                // 排计时器的间隙里又来了 prepare / 隐藏：这个计时器已经过期
                Some(timer)
            }
        };
        if let Some(t) = stale {
            t.cancel();
        }
    }

    /// 空闲计时器到期时面板是否仍处在同一轮空闲里：没有新的 prepare / 隐藏，隐藏着，没有会话、暂存的 prepare 与
    /// 进行中的创建
    fn still_idle(g: &Inner, idle_gen: u64) -> bool {
        g.idle_gen == idle_gen && g.session.is_none() && !g.visible() && !g.creating && g.pending.is_none()
    }

    /// 隐藏了 `at` 之后、期间没有新的显示——卸载面板 WebView，或回收（销毁后立即在后台重建、预热）。
    /// 选项在这期间变了：按新的选项重新排（或不再需要）。回收遇到其他 Wisp 窗口有短时活动（[`PanelHost::busy`]）时推迟
    /// [`RECYCLE_RETRY`] 再看，推迟的总时长不超过 [`RECYCLE_DEFER_MAX`]；推迟期间卸载到期了就直接卸载（卸载只是销毁，
    /// 不推迟）
    fn on_idle_timer(&self, idle_gen: u64, at: Duration) {
        let (plan, recycle_after) = {
            let g = self.inner.lock();
            if !Self::still_idle(&g, idle_gen) {
                return;
            }
            (idle_plan(&g.options, g.used_since_load, at), g.options.recycle_after)
        };
        let action = match plan {
            None => return,
            Some((wait, _)) if !wait.is_zero() => {
                self.arm_idle(idle_gen, wait, at.saturating_add(wait));
                return;
            }
            Some((_, action)) => action,
        };
        if action == IdleAction::Recycle {
            // 已经推迟了多久（计时器链的逻辑时长减去回收本该到期的时刻）
            let deferred = recycle_after.map_or(Duration::ZERO, |r| at.saturating_sub(r));
            if deferred < RECYCLE_DEFER_MAX
                && let Some(reason) = self.host.busy()
            {
                log::debug!(
                    "panel recycle deferred ({reason}); retry in {} s",
                    RECYCLE_RETRY.as_secs()
                );
                self.arm_idle(idle_gen, RECYCLE_RETRY, at.saturating_add(RECYCLE_RETRY));
                return;
            }
            if !deferred.is_zero() {
                log::info!("panel recycle deferred for {} s; recycling now", deferred.as_secs());
            }
        }
        {
            let mut g = self.inner.lock();
            // busy() 期间没有持锁：再核对一次
            let due = idle_plan(&g.options, g.used_since_load, at).is_some_and(|(w, a)| w.is_zero() && a == action);
            if !Self::still_idle(&g, idle_gen) || !due || !self.host.exists() {
                return;
            }
            g.idle_timer = None;
            g.loaded = false;
            g.loaded_at = None;
            g.pinned_webview = None;
            g.used_since_load = false;
            g.boot_session = None;
            if action == IdleAction::Recycle {
                // 重建期间到达的 prepare 暂存，页面加载后照常执行（与失效重建同一条路径）
                g.creating = true;
                g.attached = false;
                g.create_started = Some(Instant::now());
                g.recycled_at = Some(at);
            }
        }
        self.host.destroy();
        match action {
            IdleAction::Unload => log::info!("panel idle, unloading webview (unload_panel_after_idle_min)"),
            IdleAction::Recycle => {
                log::info!("panel idle {} ms after use, recycling webview", at.as_millis());
                self.spawn_create_after_flag();
            }
        }
    }

    // -----------------------------------------------------------------------------------------------------------
    // 交互

    /// 翻译视图被点击后成为 key（`panel_activate`）
    pub fn activate(&self, session_id: u64) {
        let current = {
            let g = self.inner.lock();
            g.visible()
                && g.session
                    .as_ref()
                    .is_some_and(|s| s.open.session_id == session_id && s.phase == Phase::Shown)
        };
        if current {
            self.host.activate();
        }
    }

    /// 用户在面板的头部 / 空白处按下左键（`panel_drag_start`）：所有面板视图都能拖动。拖动只影响本次会话（下次唤出
    /// 仍按锚点定位）；剪贴板面板在「记住位置」模式下隐藏时记住拖到的位置
    pub fn drag_start(&self, session_id: u64) {
        let Some(seq) = self.begin_drag(session_id) else {
            log::debug!("panel drag start ignored: session={session_id} is not shown");
            return;
        };
        log::debug!("panel drag start session={session_id}");
        self.host.start_drag();
        // 只是按了一下、窗口没动（不会有移动事件）时同样要结束拖动状态
        self.schedule_drag_check(session_id, seq);
    }

    /// window lab：与 `panel_drag_start` 相同地进入拖动状态，但不交给系统拖动（实验台随后自己移动窗口，模拟用户的拖动）
    #[cfg(any(debug_assertions, feature = "fake-llm"))]
    pub fn lab_begin_drag(&self, session_id: u64) -> bool {
        let Some(seq) = self.begin_drag(session_id) else {
            return false;
        };
        self.schedule_drag_check(session_id, seq);
        true
    }

    /// 当前已显示的会话进入拖动状态，返回这次拖动的序号；不是当前会话时 None。`dragged` 不在这里置位：只是按了一下、
    /// 窗口没动不算拖过（跟随 / 结束时发现窗口确实移动了才算）
    fn begin_drag(&self, session_id: u64) -> Option<u64> {
        let seq = {
            let mut g = self.inner.lock();
            let visible = g.visible();
            let s = g
                .session
                .as_mut()
                .filter(|s| visible && s.open.session_id == session_id && s.phase == Phase::Shown)?;
            s.dragging = true;
            s.drag_seq += 1;
            if self.frame_mode == FrameMode::Native {
                // 进行中的高度弹簧停在当前高度（下面的 freeze_height），拖动结束时从窗口当前高度继续到目标
                s.frame_pending = true;
            }
            self.host.pause_guards(true);
            s.drag_seq
        };
        if self.frame_mode == FrameMode::Native {
            // 排在系统拖动之前：拖动期间高度弹簧不再逐帧写窗口（不和系统的拖动抢位置）
            self.host.freeze_height();
        }
        Some(seq)
    }

    fn on_event(&self, ev: PanelEvent) {
        match ev {
            PanelEvent::PageLoaded => self.on_page_loaded(),
            PanelEvent::Focused(focused) => self.on_focus(focused),
            PanelEvent::Moved => self.on_moved(),
        }
    }

    fn on_page_loaded(&self) {
        enum Reboot {
            None,
            Replay(PanelPrepare, u64, u32),
            Hide(u64),
        }
        let (pending, reboot, recreated, idle_next, stale_timer) = {
            let mut g = self.inner.lock();
            let first = !g.loaded;
            g.loaded = true;
            g.loaded_at = Some(Instant::now());
            g.creating = false;
            // 页面加载完成之前就凭 Boot.prepare ready 过的会话：这一页已经用过
            g.used_since_load = std::mem::take(&mut g.boot_used) && first;
            let recreated = g.create_started.take().map(|t| t.elapsed());
            let recycled_at = g.recycled_at.take();
            if first {
                let pending = g.pending.take();
                g.next_cold = pending.is_some();
                // 冷唤出的外壳已在屏幕上：这一轮加载就是它在等的，按新的轮次 emit（外壳期间的看门狗作废）
                let shell = match g.session.as_mut() {
                    Some(s) if pending.is_none() && s.shell && s.phase == Phase::TimedOut => {
                        s.attempt += 1;
                        s.timed_out = false;
                        s.resends = 0;
                        s.prepared_at = Instant::now();
                        Reboot::Replay(Self::prepare_payload(&s.open), s.open.session_id, s.attempt)
                    }
                    _ => Reboot::None,
                };
                // 回收出来的干净页面（期间没有新的 prepare）：同一轮空闲里只剩卸载可能还没到期（用户开启了
                // unload_panel_after_idle_min 且它比回收晚），按剩下的时长排上
                let mut stale_timer = None;
                let idle_next = if let Some(at) = recycled_at {
                    if pending.is_none() && g.session.is_none() && !g.visible() {
                        idle_plan(&g.options, false, at).map(|(wait, _)| (g.idle_gen, wait, at.saturating_add(wait)))
                    } else {
                        None
                    }
                } else if g.used_since_load && pending.is_none() && g.session.is_none() {
                    // 页面加载完成之前就凭 Boot.prepare 用过、那个会话也已经结束：它隐藏时这一页还在创建中，
                    // 空闲计时器没能排上（或已因 creating 作废）——现在按用过的页面重排
                    Self::rearm_idle_if_idle(&mut g).map(|(old, idle_gen, after)| {
                        stale_timer = old;
                        (idle_gen, after, after)
                    })
                } else {
                    None
                };
                (pending, shell, recreated, idle_next, stale_timer)
            } else {
                // 已加载过：页面被重新加载。准备中与 ready 超时（alpha 0，用户什么都没看到）的会话在新页面上
                // 重放这次 prepare——WebContent 崩溃后的重载正是 ready 超时的典型成因；计时器按新的轮次重新开始，旧轮次的
                // 超时 / 看门狗 / 补发作废。已显示的会话内容已经丢失，直接隐藏（经 hidden 通道告知编排器收尾）
                let reboot = match g.session.as_mut() {
                    Some(s) if matches!(s.phase, Phase::Preparing | Phase::TimedOut) => {
                        s.attempt += 1;
                        s.timed_out = false;
                        s.resends = 0;
                        s.prepared_at = Instant::now();
                        Reboot::Replay(Self::prepare_payload(&s.open), s.open.session_id, s.attempt)
                    }
                    Some(s) if s.phase == Phase::Shown => Reboot::Hide(s.open.session_id),
                    _ => Reboot::None,
                };
                (None, reboot, None, None, None)
            }
        };
        if let Some(t) = stale_timer {
            t.cancel();
        }
        self.loaded.send_modify(|n| *n = n.wrapping_add(1));
        if let Some(elapsed) = recreated {
            log::info!("panel window ready (page loaded) in {} ms", elapsed.as_millis());
        }
        match reboot {
            Reboot::None => {}
            Reboot::Replay(prepare, session_id, attempt) => {
                if attempt > 0 && recreated.is_some() {
                    log::debug!("panel page loaded under the native shell; preparing session={session_id}");
                } else {
                    log::info!("panel page reloaded while preparing; replaying prepare session={session_id}");
                }
                self.emit_and_arm_timeout(&prepare, session_id, attempt);
            }
            Reboot::Hide(session_id) => {
                log::info!("panel page reloaded while shown; hiding session={session_id}");
                self.hide(session_id, HideReason::Other, None);
            }
        }
        if let Some((idle_gen, wait, at)) = idle_next {
            self.arm_idle(idle_gen, wait, at);
        }
        if let Some((open, rebuilds)) = pending {
            // 主线程（页面加载回调）：`loaded` 已置位、暂存与外壳会话互斥，直接选路，不碰 frame_lock
            self.prepare_routed(open, rebuilds);
        }
    }

    /// key 状态变化。原地替换中（新会话还在准备、窗口一直可见）的会话同样记下：ready 时的激活、`PanelShown.focused`
    /// 与随后的 Esc 守卫都以它为准。冷唤出外壳上的会话同样记：要焦点的视图外壳一上屏就是 key（`key_ms` 记在
    /// 这一刻，可能早于 ready）；外壳期间失焦（用户点了别的应用）的输入 / 剪贴板视图直接收起——前端此刻可能还没加载，
    /// 走不了它自己的退出流程
    fn on_focus(&self, focused: bool) {
        enum Blur {
            None,
            /// 已显示：让前端播放退出动画
            Request,
            /// 外壳上（还没 ready）：直接隐藏并补发 hide_request
            Shell,
        }
        let (session_id, blur) = {
            let mut g = self.inner.lock();
            let visible = g.visible();
            let Some(s) = g.session.as_mut().filter(|s| {
                visible
                    && (matches!(s.phase, Phase::Shown | Phase::TimedOut)
                        || (s.phase == Phase::Preparing && s.in_place))
            }) else {
                return;
            };
            if s.key == focused {
                return;
            }
            s.key = focused;
            let on_shell = s.shell && s.phase == Phase::TimedOut;
            if focused {
                s.ever_key = true;
                if s.open.focused && (s.phase == Phase::Shown || on_shell) {
                    // 输入 / 剪贴板视图成为 key（perf 的 `key_ms`；同一阶段只记第一次）
                    self.host.perf(s.open.session_id, PerfStage::KeyGained);
                }
            }
            let blur = if focused || !s.ever_key || !s.open.guards.blur_hides {
                Blur::None
            } else if s.phase == Phase::Shown && s.dragging {
                // 用户正在拖动：有的平台拖动期间窗口暂时失焦（X11 的窗口管理器拖动时抓键盘，GDK 报 FocusOut、松开后
                // 再报 FocusIn），不关闭；拖动结束时仍不是 key 再关（on_drag_settled）
                s.blur_deferred = true;
                Blur::None
            } else if s.phase == Phase::Shown {
                Blur::Request
            } else if on_shell {
                Blur::Shell
            } else {
                Blur::None
            };
            let session_id = s.open.session_id;
            log::debug!("panel session={session_id} key={focused}");
            self.host.set_guard_key(focused);
            (session_id, blur)
        };
        self.host.emit_focus_changed(PanelFocusChanged { session_id, focused });
        match blur {
            Blur::None => {}
            Blur::Request => self.request_hide_for(session_id, HideReason::Blur),
            Blur::Shell => {
                log::info!("panel shell lost focus before the page was ready; hiding session={session_id}");
                self.hide_now_and_notify(session_id, HideReason::Blur);
            }
        }
    }

    /// 窗口移动了：只在用户拖动期间跟随（程序自己改的位置以几何为准）。窗口事件的回调里不读窗口位置（Windows / Linux 的
    /// 读取要经事件循环），排到计时器线程上读，同一时刻最多排一次
    fn on_moved(&self) {
        let (session_id, seq, follow) = {
            let mut g = self.inner.lock();
            let Some(s) = g.session.as_mut().filter(|s| s.dragging && s.phase == Phase::Shown) else {
                return;
            };
            s.drag_seq += 1;
            let follow = !std::mem::replace(&mut s.follow_pending, true);
            (s.open.session_id, s.drag_seq, follow)
        };
        if follow {
            let me = self.me.clone();
            self.host.schedule(
                Duration::ZERO,
                Box::new(move || {
                    if let Some(me) = me.upgrade() {
                        me.follow_drag(session_id);
                    }
                }),
            );
        }
        self.schedule_drag_check(session_id, seq);
    }

    fn schedule_drag_check(&self, session_id: u64, seq: u64) {
        let me = self.me.clone();
        self.host.schedule(
            DRAG_SETTLE,
            Box::new(move || {
                if let Some(me) = me.upgrade() {
                    me.on_drag_settled(session_id, seq);
                }
            }),
        );
    }

    /// 窗口当前的外框与它中心所在的屏幕（不持锁调用：会等主线程）
    fn read_window(&self) -> Option<(Rect, Option<Screen>)> {
        let frame = self.host.window_frame()?;
        let center = Point {
            x: frame.x + frame.width / 2.0,
            y: frame.y + frame.height / 2.0,
        };
        Some((frame, self.host.screen_at(center)))
    }

    /// 几何跟随窗口外框 `frame`（桌面坐标），屏幕取不到时留在原来的屏上（锁内）。返回卡片是否确实移动了（换了屏，或左边 /
    /// 固定边变化 ≥ 1 px）；没动时几何保持原样（桌面坐标换算的舍入不写回）
    fn follow_frame(&self, s: &mut Session, frame: Rect, screen: Option<Screen>) -> bool {
        let screen = screen.unwrap_or_else(|| s.open.geometry.screen.clone());
        let local = screen.from_desktop(frame, self.unit);
        let mut next = s.open.geometry.clone();
        next.follow_window(screen, local, self.margin);
        let g = &s.open.geometry;
        let moved =
            next.screen.id != g.screen.id || (next.x - g.x).abs() >= 1.0 || (next.edge_y - g.edge_y).abs() >= 1.0;
        if moved {
            s.open.geometry = next;
        }
        moved
    }

    /// 卡片位置变了：关闭监听的卡片矩形与点击穿透区域随之更新（锁内，只排队）
    fn sync_hit(&self, s: &Session) {
        self.host.update_guard_card(Self::card_hit(s));
        if let Some(region) = self.click_region(s) {
            self.host.set_click_through(Some(region));
        }
    }

    /// 拖动中：几何跟随窗口（之后的高度变化、点击判定都以新位置为准）；窗口确实移动了才算拖过（`dragged`）
    fn follow_drag(&self, session_id: u64) {
        {
            let mut g = self.inner.lock();
            let Some(s) = g.session.as_mut().filter(|s| s.open.session_id == session_id) else {
                return;
            };
            // 先清标记再读位置：读的期间到来的移动会再排一次
            s.follow_pending = false;
            if !s.dragging || s.phase != Phase::Shown {
                return;
            }
        }
        let Some((frame, screen)) = self.read_window() else {
            return;
        };
        let mut g = self.inner.lock();
        if let Some(s) = g
            .session
            .as_mut()
            .filter(|s| s.open.session_id == session_id && s.dragging && s.phase == Phase::Shown)
            && self.follow_frame(s, frame, screen)
        {
            s.dragged = true;
            self.sync_hit(s);
        }
    }

    /// 窗口停下 [`DRAG_SETTLE`]（或按下之后一直没动）：鼠标键还按着就是用户按住不动，再等一轮；已经松开则拖动结束——
    /// - 几何按窗口的最终位置更新；窗口确实移动了（`dragged`）才把卡片整体夹回所在屏幕的工作区（拖出屏幕边缘、拖到另一块
    ///   屏上时按那块屏的工作区与缩放），需要时窗口随之移回；
    /// - 拖动期间推迟的窗口变化补上：Css 帧模式一次放到位；Native 帧模式的高度弹簧从窗口当前高度继续（带竖直边界），推迟了
    ///   宽度切换或要夹回时直接放到会话几何；
    /// - 先更新关闭监听的卡片矩形、再恢复「鼠标离开」判定（两者按顺序排队）；
    /// - 拖动期间推迟的失焦关闭：此刻仍不是 key 就照常关闭
    fn on_drag_settled(&self, session_id: u64, seq: u64) {
        enum Settle {
            Frame(Rect),
            Native(f64, Placement, Option<VerticalBound>, bool),
            None,
        }
        let current = |g: &Inner| {
            g.session.as_ref().is_some_and(|s| {
                s.open.session_id == session_id && s.drag_seq == seq && s.dragging && s.phase == Phase::Shown
            })
        };
        if !current(&self.inner.lock()) {
            return;
        }
        if self.host.primary_button_down() {
            self.schedule_drag_check(session_id, seq);
            return;
        }
        let frame_guard = self.frame_lock.lock();
        let window = self.read_window();
        let (settle, blur) = {
            let mut g = self.inner.lock();
            if !current(&g) {
                return;
            }
            let reduce = g.options.reduce_motion;
            let Some(s) = g.session.as_mut() else { return };
            s.dragging = false;
            if let Some((frame, screen)) = window
                && self.follow_frame(s, frame, screen)
            {
                s.dragged = true;
            }
            let clamped = s.dragged && s.open.geometry.fit_card(s.card_height);
            let frame_pending = std::mem::take(&mut s.frame_pending);
            let width_pending = std::mem::take(&mut s.width_pending);
            self.sync_hit(s);
            self.host.pause_guards(false);
            let settle = if clamped || width_pending {
                Settle::Frame(self.desktop_window_rect(s))
            } else if !frame_pending {
                Settle::None
            } else if self.frame_mode == FrameMode::Native {
                let g2 = &s.open.geometry;
                Settle::Native(
                    s.card_height,
                    g2.placement,
                    s.dragged.then(|| self.vertical_bound(g2)),
                    !reduce,
                )
            } else {
                Settle::Frame(self.desktop_window_rect(s))
            };
            let blur = std::mem::take(&mut s.blur_deferred) && !s.key;
            log::debug!(
                "panel drag settled session={session_id} dragged={} clamped={clamped} card={:?}",
                s.dragged,
                s.open.geometry.card_rect(s.card_height)
            );
            (settle, blur)
        };
        match settle {
            Settle::Frame(rect) => {
                if let Err(e) = self.host.set_frame(rect) {
                    log::warn!("panel settle after drag failed: {}", e.message);
                }
            }
            Settle::Native(h, placement, bound, animate) => {
                self.host
                    .animate_height(h, placement, bound, animate, ResizeSpring::Gentle);
            }
            Settle::None => {}
        }
        drop(frame_guard);
        if blur {
            log::info!("panel lost focus during the drag and is still not key; hiding session={session_id}");
            self.request_hide_for(session_id, HideReason::Blur);
        }
    }

    /// 拖动过的面板的竖直边界（Native 帧模式的高度弹簧用，桌面坐标）
    fn vertical_bound(&self, g: &PanelGeometry) -> VerticalBound {
        let (top, bottom) = g.vertical_limits();
        let r = g
            .screen
            .to_desktop(Rect::new(g.x, top, g.width, bottom - top), self.unit);
        VerticalBound {
            top: r.y,
            bottom: r.bottom(),
        }
    }

    /// 当前会话快照（卡片矩形）；正在隐藏或没有会话时为 None
    pub fn snapshot(&self) -> Option<PanelSnapshot> {
        let g = self.inner.lock();
        let s = g.session.as_ref().filter(|s| s.phase != Phase::Hiding)?;
        let geometry = &s.open.geometry;
        Some(PanelSnapshot {
            session_id: s.open.session_id,
            view: s.open.view,
            screen: geometry.screen.clone(),
            rect: geometry.card_rect(s.card_height),
            key: s.key,
            visible: g.visible(),
            shown: s.phase == Phase::Shown,
        })
    }

    /// 诊断（frame lab 超时时记日志）：当前会话、阶段、窗口是否在屏幕上、排队 / 暂存的 prepare、页面是否加载完
    pub fn describe(&self) -> String {
        let g = self.inner.lock();
        format!(
            "session={:?} on_screen={:?} queued={:?} pending={:?} loaded={} creating={}",
            g.session
                .as_ref()
                .map(|s| (s.open.session_id, s.phase, s.timed_out, s.attempt)),
            g.on_screen,
            g.queued.as_ref().map(|q| q.session_id),
            g.pending.as_ref().map(|(p, _)| p.session_id),
            g.loaded,
            g.creating
        )
    }

    /// 最近一次显示过的卡片（toast 定位）
    pub fn last_card(&self) -> Option<LastCard> {
        let g = self.inner.lock();
        match g.session.as_ref() {
            Some(s) if s.phase != Phase::Preparing => {
                let geometry = &s.open.geometry;
                Some(LastCard {
                    screen: geometry.screen.clone(),
                    rect: geometry.card_rect(s.card_height),
                })
            }
            _ => g.last_card.clone(),
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 真实实现

/// 生成面板 boot 与原生表面的回调（WindowManager 提供，读当前外观）
pub type BootProvider = Arc<dyn Fn() -> (Boot, CardSurface) + Send + Sync>;
/// 性能埋点（按面板 session）：阶段，或本次唤出用的页面
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerfNote {
    Stage(PerfStage),
    Page(PanelPage),
}

/// 性能埋点回调（按面板 session）
pub type PerfHook = Arc<dyn Fn(u64, PerfNote) + Send + Sync>;
/// frame lab 对原生表面的改写
pub type SurfaceOverride = Arc<dyn Fn(CardSurface) -> CardSurface + Send + Sync>;

/// 监听线程上按 FIFO 执行的操作（关闭监听、点击穿透、销毁窗口）
enum GuardOp {
    Arm {
        session_id: u64,
        card: CardHit,
        spec: GuardSpec,
        key: bool,
        sink: HideSink,
    },
    UpdateCard(CardHit),
    SetKey(bool),
    /// 拖动期间暂停「鼠标离开」判定
    Pause(bool),
    Disarm,
    ClickThrough(Option<ClickRegion>),
    /// 先撤掉监听与穿透，再销毁窗口（新窗口不会继承旧窗口的钩子）
    Destroy,
}

/// [`PanelHost`] 的真实实现：Tauri 窗口 + `native::panel_*` + 平台监听
pub struct TauriPanelHost {
    app: tauri::AppHandle,
    frame_mode: FrameMode,
    boot: BootProvider,
    perf: PerfHook,
    /// 监听线程的队列（第一次用到时启动；线程空闲时阻塞在 recv 上，零唤醒）
    guard_ops: OnceLock<std::sync::mpsc::Sender<GuardOp>>,
    /// frame lab 注入的附加初始化脚本（只在 debug 构建）
    extra_script: Mutex<Option<String>>,
    /// frame lab 改写的原生表面（只在 debug 构建）
    surface_override: Mutex<Option<SurfaceOverride>>,
    /// 已请求销毁、窗口还没真正消失（销毁在主线程异步完成；同 label 的新窗口要等旧的消失才能建）
    destroying: Arc<AtomicBool>,
    /// 其他 Wisp 窗口是否在忙（[`PanelHost::busy`]），由 `WindowManager` 构造后装上
    busy_probe: OnceLock<BusyProbe>,
    /// window lab（只在 debug / fake-llm 构建）：这个时刻之前的 `panel:prepare` 不发出——模拟冷页面上监听迟迟没有注册，
    /// 让冷唤出的外壳在屏幕上多留一会儿（`cold_show_keys` 往外壳里打字）；补发照常每帧一次，到点之后的那次送达
    #[cfg(any(debug_assertions, feature = "fake-llm"))]
    lab_hold_prepare: Mutex<Option<Instant>>,
    /// window lab（只在 debug / fake-llm 构建）：true 时新页面的 Boot 不带 prepare——冷唤出 A/B（Boot 带不带 prepare 的对照）
    #[cfg(any(debug_assertions, feature = "fake-llm"))]
    lab_no_boot_prepare: AtomicBool,
}

/// [`PanelHost::busy`] 的实现（`WindowManager::others_busy`）
pub type BusyProbe = Box<dyn Fn() -> Option<String> + Send + Sync>;

/// 重建面板时等旧窗口真正销毁的上限
const DESTROY_WAIT: Duration = Duration::from_millis(1500);

impl TauriPanelHost {
    /// 构造
    pub fn new(app: tauri::AppHandle, frame_mode: FrameMode, boot: BootProvider, perf: PerfHook) -> Self {
        Self {
            app,
            frame_mode,
            boot,
            perf,
            guard_ops: OnceLock::new(),
            extra_script: Mutex::new(None),
            surface_override: Mutex::new(None),
            destroying: Arc::new(AtomicBool::new(false)),
            busy_probe: OnceLock::new(),
            #[cfg(any(debug_assertions, feature = "fake-llm"))]
            lab_hold_prepare: Mutex::new(None),
            #[cfg(any(debug_assertions, feature = "fake-llm"))]
            lab_no_boot_prepare: AtomicBool::new(false),
        }
    }

    /// window lab：之后新建的面板页面 Boot 里带不带暂存的 prepare（冷唤出 A/B）
    #[cfg(any(debug_assertions, feature = "fake-llm"))]
    pub fn lab_set_boot_prepare(&self, enabled: bool) {
        self.lab_no_boot_prepare.store(!enabled, Ordering::Release);
    }

    /// window lab：接下来 `hold` 之内不发 `panel:prepare`（见字段说明）
    #[cfg(any(debug_assertions, feature = "fake-llm"))]
    pub fn lab_hold_prepare(&self, hold: Duration) {
        *self.lab_hold_prepare.lock() = Some(Instant::now() + hold);
    }

    /// 装上「其他 Wisp 窗口是否在忙」的判断（只装一次）
    pub fn set_busy_probe(&self, probe: BusyProbe) {
        if self.busy_probe.set(probe).is_err() {
            log::warn!("panel busy probe already set");
        }
    }

    /// frame lab：附加初始化脚本与原生表面改写
    pub fn set_lab(&self, script: String, surface: SurfaceOverride) {
        *self.extra_script.lock() = Some(script);
        *self.surface_override.lock() = Some(surface);
    }

    fn window(&self) -> Option<tauri::WebviewWindow> {
        self.app.get_webview_window(PANEL_LABEL)
    }

    /// Css 帧模式且窗口不能透明（此时 `Boot.shadow_margin` 为 0，与前端的 `isOpaqueWindow` 是同一个结论）
    fn opaque_window(&self) -> bool {
        self.frame_mode == FrameMode::Css && !crate::app::runtime::transparent_windows()
    }

    /// 不透明的面板窗口跟随明暗切换底色（Native / 透明窗口没有窗口底色）
    pub fn set_theme(&self, theme: wisp_types::ResolvedTheme) {
        if self.opaque_window()
            && let Some(win) = self.window()
            && let Err(e) = win.set_background_color(Some(super::settings_window::window_background(theme)))
        {
            log::warn!("panel background color update failed: {e}");
        }
    }

    /// 外观变化：更新原生卡片表面（只有 Native 帧模式有）
    pub fn set_surface(&self, surface: CardSurface) {
        let surface = match self.surface_override.lock().as_ref() {
            Some(f) => f(surface),
            None => surface,
        };
        if self.frame_mode == FrameMode::Native
            && let Some(win) = self.window()
        {
            native::panel_set_surface(&win, surface);
        }
    }

    /// 排进监听线程。监听的安装可能要等主线程（macOS 的全局点击监听、全局 Esc），放在这条线程上既不阻塞调用方
    /// （ready 命令、控制器的锁），又保证「装 → 卸」的先后与会话状态一致
    fn enqueue(&self, op: GuardOp) {
        let tx = self.guard_ops.get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel::<GuardOp>();
            let app = self.app.clone();
            let destroying = Arc::clone(&self.destroying);
            let spawned = std::thread::Builder::new()
                .name("wisp-panel-guards".to_owned())
                .spawn(move || run_guard_ops(&app, &rx, &destroying));
            if let Err(e) = spawned {
                log::error!("spawn panel guard thread failed: {e}");
            }
            tx
        });
        if let Err(std::sync::mpsc::SendError(op)) = tx.send(op) {
            // 线程没起来：就地执行，至少不丢操作
            let (tx, rx) = std::sync::mpsc::channel();
            let _ = tx.send(op);
            drop(tx);
            run_guard_ops(&self.app, &rx, &self.destroying);
        }
    }
}

/// 监听线程：逐个执行排队的操作，队列关闭时退出
fn run_guard_ops(app: &tauri::AppHandle, rx: &std::sync::mpsc::Receiver<GuardOp>, destroying: &AtomicBool) {
    let guards = Guards::new(app.clone());
    let window = || app.get_webview_window(PANEL_LABEL);
    while let Ok(op) = rx.recv() {
        match op {
            GuardOp::Arm {
                session_id,
                card,
                spec,
                key,
                sink,
            } => guards.arm(session_id, card, spec, key, sink),
            GuardOp::UpdateCard(card) => guards.update_card(card),
            GuardOp::SetKey(key) => guards.set_key(key),
            GuardOp::Pause(paused) => guards.set_paused(paused),
            GuardOp::Disarm => guards.disarm(),
            GuardOp::ClickThrough(region) => {
                if let Some(win) = window() {
                    native::panel_set_click_through(&win, region);
                }
            }
            GuardOp::Destroy => {
                guards.disarm();
                match window() {
                    Some(win) => {
                        native::panel_set_click_through(&win, None);
                        native::panel_destroy(&win);
                    }
                    None => destroying.store(false, Ordering::Release),
                }
            }
        }
    }
}

impl PanelHost for TauriPanelHost {
    fn exists(&self) -> bool {
        !self.destroying.load(Ordering::Acquire) && self.window().is_some()
    }

    fn create(&self, events: PanelEvents, boot_prepare: Option<PanelPrepare>) -> Result<(), IpcError> {
        if self.destroying.load(Ordering::Acquire) {
            // 旧窗口的销毁（监听线程上排队，主线程上完成）还没结束：等它消失（本函数在阻塞线程上调用）
            let started = Instant::now();
            while self.window().is_some() {
                if started.elapsed() > DESTROY_WAIT {
                    return Err(IpcError::internal("old panel window is still being destroyed"));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            self.destroying.store(false, Ordering::Release);
        }
        if self.window().is_some() {
            return Ok(());
        }
        let (mut boot, surface) = (self.boot)();
        // window lab：A/B 关掉时、以及 `lab_hold_prepare` 生效期间（`cold_show_keys` 要外壳在屏幕上多留一会儿：内容只能
        // 等推迟的 emit）新页面不带 prepare
        #[cfg(any(debug_assertions, feature = "fake-llm"))]
        let boot_prepare = boot_prepare.filter(|_| {
            !self.lab_no_boot_prepare.load(Ordering::Acquire)
                && self.lab_hold_prepare.lock().is_none_or(|until| Instant::now() >= until)
        });
        if let Some(p) = &boot_prepare {
            log::debug!("panel boot carries the pending prepare session={}", p.session_id);
        }
        boot.prepare = boot_prepare;
        let surface = match self.surface_override.lock().as_ref() {
            Some(f) => f(surface),
            None => surface,
        };
        let config = self
            .app
            .config()
            .app
            .windows
            .iter()
            .find(|w| w.label == PANEL_LABEL)
            .cloned()
            .ok_or_else(|| IpcError::not_found("window template panel"))?;
        let on_load = Arc::clone(&events);
        let mut builder = WebviewWindowBuilder::from_config(&self.app, &config)?
            .initialization_script(super::boot_script(&boot))
            .visible(false)
            .on_page_load(move |_, payload| {
                if payload.event() == PageLoadEvent::Finished {
                    on_load(PanelEvent::PageLoaded);
                }
            });
        if let Some(script) = self.extra_script.lock().clone() {
            builder = builder.initialization_script(script);
        }
        // 实验开关（只在 debug 构建）：`WISP_PANEL_THROTTLING=throttle|suspend` 覆盖模板的 backgroundThrottling，
        // 用于会话后空闲唤醒的 A/B；发布版始终按模板（disabled）
        #[cfg(any(debug_assertions, feature = "fake-llm"))]
        if let Ok(v) = std::env::var("WISP_PANEL_THROTTLING") {
            use tauri::utils::config::BackgroundThrottlingPolicy as Policy;
            let policy = match v.trim() {
                "throttle" => Some(Policy::Throttle),
                "suspend" => Some(Policy::Suspend),
                "disabled" => Some(Policy::Disabled),
                other => {
                    log::warn!("WISP_PANEL_THROTTLING={other} 无法识别（throttle | suspend | disabled），忽略");
                    None
                }
            };
            if let Some(policy) = policy {
                log::info!("panel background throttling overridden: {policy:?}");
                builder = builder.background_throttling(policy);
            }
        }
        if self.opaque_window() {
            // 窗口不能透明（Linux Wayland、关闭合成）：卡片铺满窗口，页面加载 / WebView 首次绘制之前透出的也必须是卡片的
            // 底色（--surface-solid），否则深色模式下每次唤出都白闪一下
            builder = builder.background_color(super::settings_window::window_background(boot.theme));
        }
        let started = Instant::now();
        let builder = native::template_overrides(builder, native::TemplateKind::Panel, self.frame_mode);
        // 旧面板上面已经等过；其他刚销毁的窗口（Overlay、pinned……）也要等它们的 `Destroyed` 处理完（见
        // native::note_destroying）
        native::await_destroyed(&self.app);
        let win = builder.build()?;
        // 空闲卸载 / 失效重建时旧窗口必须真的释放（macOS 的 tao 引用泄漏，见 native::adopt_window）
        native::adopt_window(&win);
        if let Err(e) = native::panel_attach(&win, self.frame_mode, surface) {
            // 没转换成面板 / 没装上原生表面的窗口不能用：销毁它，下次预热重新创建
            native::note_destroying(win.label());
            if let Err(close) = win.destroy() {
                log::warn!("destroy half-initialized panel failed: {close}");
            }
            return Err(e);
        }
        win.on_window_event(move |ev| match ev {
            WindowEvent::Focused(f) => events(PanelEvent::Focused(*f)),
            WindowEvent::Moved(_) => events(PanelEvent::Moved),
            _ => {}
        });
        log::info!(
            "panel created frame_mode={} in {} ms",
            self.frame_mode.as_str(),
            started.elapsed().as_millis()
        );
        Ok(())
    }

    fn destroy(&self) {
        if self.window().is_some() {
            self.destroying.store(true, Ordering::Release);
            self.enqueue(GuardOp::Destroy);
        }
    }

    fn restore_visibility(&self) {
        if let Some(win) = self.window() {
            native::panel_restore_visibility(&win);
        }
    }

    fn release_visibility(&self, low_memory: bool) {
        if let Some(win) = self.window() {
            native::panel_release_visibility(&win, low_memory);
        }
    }

    fn set_frame(&self, rect: Rect) -> Result<(), IpcError> {
        let win = self.window().ok_or_else(|| IpcError::not_found("panel window"))?;
        native::panel_set_frame(&win, rect)
    }

    fn finish_exit(&self) {
        if let Some(win) = self.window() {
            native::panel_finish_exit(&win);
        }
    }

    fn pin_webview(&self, height: f64) -> Result<(), IpcError> {
        let win = self.window().ok_or_else(|| IpcError::not_found("panel window"))?;
        native::panel_pin_webview(&win, height)
    }

    fn set_focusable(&self, focusable: bool) {
        if let Some(win) = self.window() {
            native::panel_set_focusable(&win, focusable);
        }
    }

    fn show_shell(
        &self,
        rect: Rect,
        max_height: f64,
        fade: Option<Fade>,
        key: bool,
        on_escape: ShellEscape,
        ticket: &ShowTicket,
    ) -> Result<(), IpcError> {
        let win = self.window().ok_or_else(|| IpcError::not_found("panel window"))?;
        native::panel_show_shell(&win, rect, max_height, fade, key, on_escape, ticket)
    }

    fn move_shell(&self, rect: Rect, max_height: Option<f64>, animate: bool) {
        if let Some(win) = self.window() {
            native::panel_move_shell(&win, rect, max_height, animate);
        }
    }

    fn reveal_shell(
        &self,
        rect: Rect,
        placement: Placement,
        fade: Fade,
        key: bool,
        animate: bool,
        ticket: &ShowTicket,
    ) -> Result<(), IpcError> {
        let win = self.window().ok_or_else(|| IpcError::not_found("panel window"))?;
        native::panel_reveal_shell(&win, rect, placement, fade, key, animate, ticket)
    }

    fn emit_prepare(&self, p: &PanelPrepare) -> Result<(), IpcError> {
        #[cfg(any(debug_assertions, feature = "fake-llm"))]
        {
            let mut hold = self.lab_hold_prepare.lock();
            match *hold {
                Some(until) if Instant::now() < until => {
                    log::trace!("window lab: panel:prepare held session={}", p.session_id);
                    return Ok(());
                }
                Some(_) => *hold = None,
                None => {}
            }
        }
        self.app.emit_to(PANEL_LABEL, event::PANEL_PREPARE, p)?;
        Ok(())
    }

    fn emit_hide_request(&self, r: PanelHideRequest) {
        if let Err(e) = self.app.emit_to(PANEL_LABEL, event::PANEL_HIDE_REQUEST, r) {
            log::warn!("emit panel:hide_request failed: {e}");
        }
    }

    fn emit_focus_changed(&self, f: PanelFocusChanged) {
        if let Err(e) = self.app.emit_to(PANEL_LABEL, event::PANEL_FOCUS_CHANGED, f) {
            log::warn!("emit panel:focus_changed failed: {e}");
        }
    }

    fn show(&self, rect: Rect, opts: ShowOpts, ticket: &ShowTicket) -> Result<(), IpcError> {
        let win = self.window().ok_or_else(|| IpcError::not_found("panel window"))?;
        native::panel_show(&win, rect, opts, ticket)?;
        // OCR commit 时面板显示在仍然可见的 Overlay 下方
        if let Some(state) = self.app.try_state::<AppState>() {
            state.windows.raise_overlays();
        }
        Ok(())
    }

    fn reveal(&self, rect: Rect, fade: Option<Fade>, key: bool, ticket: &ShowTicket) -> Result<(), IpcError> {
        let win = self.window().ok_or_else(|| IpcError::not_found("panel window"))?;
        native::panel_reveal(&win, rect, fade, key, ticket)
    }

    fn animate_height(
        &self,
        height: f64,
        placement: Placement,
        bound: Option<VerticalBound>,
        animate: bool,
        spring: ResizeSpring,
    ) {
        if let Some(win) = self.window() {
            native::panel_animate_height(&win, height, placement, bound, animate, spring);
        }
    }

    fn freeze_height(&self) {
        if let Some(win) = self.window() {
            native::panel_freeze_height(&win);
        }
    }

    fn switch_width(&self, switch: WidthSwitch) {
        if let Some(win) = self.window() {
            native::panel_switch_width(&win, switch);
        }
    }

    fn hide(&self, fade: Option<Fade>, done: Box<dyn FnOnce() + Send>) {
        match self.window() {
            Some(win) => native::panel_hide(&win, fade, done),
            None => done(),
        }
    }

    fn activate(&self) {
        if let Some(win) = self.window() {
            native::panel_activate(&win);
        }
    }

    fn start_drag(&self) {
        if let Some(win) = self.window()
            && let Err(e) = win.start_dragging()
        {
            log::warn!("panel start_dragging failed: {e}");
        }
    }

    fn primary_button_down(&self) -> bool {
        native::primary_button_down()
    }

    fn window_frame(&self) -> Option<Rect> {
        self.window().and_then(|w| native::window_frame(&w))
    }

    fn set_click_through(&self, region: Option<ClickRegion>) {
        self.enqueue(GuardOp::ClickThrough(region));
    }

    fn arm_guards(&self, session_id: u64, card: CardHit, spec: GuardSpec, key: bool, sink: HideSink) {
        self.enqueue(GuardOp::Arm {
            session_id,
            card,
            spec,
            key,
            sink,
        });
    }

    fn update_guard_card(&self, card: CardHit) {
        self.enqueue(GuardOp::UpdateCard(card));
    }

    fn set_guard_key(&self, key: bool) {
        self.enqueue(GuardOp::SetKey(key));
    }

    fn pause_guards(&self, paused: bool) {
        self.enqueue(GuardOp::Pause(paused));
    }

    fn disarm_guards(&self) {
        self.enqueue(GuardOp::Disarm);
    }

    fn restores_focus(&self) -> bool {
        native::RESTORE_FOCUS_ON_HIDE
    }

    fn focus_on_shell(&self) -> bool {
        native::foreground_is_shell()
    }

    fn restore_focus(&self, prev: &AppRef) {
        let app = self.app.clone();
        let prev = prev.clone();
        tauri::async_runtime::spawn_blocking(move || {
            if let Some(state) = app.try_state::<AppState>()
                && let Err(e) = state.platform.focus.activate(&prev)
            {
                log::info!("restore focus after panel hide failed: {e}");
            }
        });
    }

    fn remember_position(&self, screen_id: &str, top_left: Point) {
        let app = self.app.clone();
        let screen_id = screen_id.to_owned();
        tauri::async_runtime::spawn_blocking(move || {
            let Some(state) = app.try_state::<AppState>() else {
                return;
            };
            let result = state.settings.state().update(&mut |s| {
                s.clipboard_positions.insert(
                    screen_id.clone(),
                    crate::settings::state_store::Position {
                        x: top_left.x,
                        y: top_left.y,
                    },
                );
            });
            if let Err(e) = result {
                log::warn!("remember clipboard position failed: {}", e.message);
            }
        });
    }

    fn screen_at(&self, desktop_point: Point) -> Option<Screen> {
        let state = self.app.try_state::<AppState>()?;
        let screens: Vec<Screen> = state.platform.screen.monitors().iter().map(Screen::from_info).collect();
        placement::screen_at_desktop(&screens, desktop_point, native::DESKTOP_UNIT).cloned()
    }

    fn perf(&self, session_id: u64, stage: PerfStage) {
        (self.perf)(session_id, PerfNote::Stage(stage));
    }

    fn perf_page(&self, session_id: u64, page: PanelPage) {
        (self.perf)(session_id, PerfNote::Page(page));
    }

    fn schedule(&self, delay: Duration, f: Box<dyn FnOnce() + Send>) {
        drop(spawn_timer(delay, f));
    }

    fn busy(&self) -> Option<String> {
        self.busy_probe.get().and_then(|probe| probe())
    }

    fn schedule_cancellable(&self, delay: Duration, f: Box<dyn FnOnce() + Send>) -> TimerHandle {
        let task = spawn_timer(delay, f);
        TimerHandle::new(move || task.abort())
    }
}

/// 计时器：异步等待 `delay`，到期后在阻塞线程池里执行 `f`（回调里有主线程同步调用，不能占着 async worker）
fn spawn_timer(delay: Duration, f: Box<dyn FnOnce() + Send>) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(delay).await;
        drop(tauri::async_runtime::spawn_blocking(f));
    })
}

// ---------------------------------------------------------------------------------------------------------------
// 测试替身

/// 记录调用顺序的 [`PanelHost`]（`windows::tests` 与本模块的单测共用）
#[cfg(test)]
pub(crate) mod mock {
    use super::*;

    /// 一次调用
    #[derive(Debug, Clone, PartialEq)]
    pub enum Call {
        Create,
        Destroy,
        RestoreVisibility,
        ReleaseVisibility(bool),
        SetFrame(Rect),
        FinishExit,
        PinWebview(f64),
        SetFocusable(bool),
        EmitPrepare(u64),
        EmitHideRequest(u64, HideReason),
        EmitFocusChanged(u64, bool),
        Show(Rect, ShowOpts),
        /// 主线程执行到显示时票已作废，跳过
        ShowSkipped,
        Reveal(Rect),
        AnimateHeight(f64, bool),
        /// `panel_resize` 指定了 snappy 的高度弹簧：(高度, 是否动画)
        AnimateSnappy(f64, bool),
        SwitchWidth(WidthSwitch),
        Hide(bool),
        Activate,
        StartDrag,
        /// Native 帧模式：进行中的高度弹簧停在当前高度（开始拖动）
        FreezeHeight,
        /// 拖动期间暂停 / 恢复「鼠标离开」判定
        PauseGuards(bool),
        /// 紧接着的高度变化带竖直边界（拖动过的 Native 面板）
        HeightBound(VerticalBound),
        ClickThrough(Option<ClickRegion>),
        ArmGuards(u64, GuardSpec, bool),
        UpdateGuardCard(Rect),
        GuardKey(bool),
        DisarmGuards,
        RestoreFocus,
        Remember(String, Point),
        Perf(u64, PerfStage),
        Page(u64, PanelPage),
        /// 外壳：(矩形, 是否淡入, 是否立即成为 key)
        ShowShell(Rect, bool, bool),
        /// 外壳移到新矩形：(矩形, 重新固定的 WebView 高度)
        MoveShell(Rect, Option<f64>),
        /// 外壳上的 ready：(矩形, 是否成为 key)
        RevealShell(Rect, bool),
        Schedule(Duration),
    }

    /// 排队的计时器
    pub type Timer = (Duration, Box<dyn FnOnce() + Send>);

    /// mock 宿主：记录调用；计时器不自动触发，由测试取出后手动执行
    #[derive(Default)]
    pub struct MockHost {
        pub calls: Mutex<Vec<Call>>,
        pub exists: Mutex<bool>,
        pub frame: Mutex<Option<Rect>>,
        pub timers: Mutex<Vec<Timer>>,
        /// 可取消计时器的取消标记（按创建顺序，连同时长）
        pub cancellable: Mutex<Vec<(Duration, Arc<AtomicBool>)>>,
        pub events: Mutex<Option<PanelEvents>>,
        /// `restores_focus` 的返回值（Windows / Linux 为 true）
        pub restores_focus: Mutex<bool>,
        /// `focus_on_shell` 的返回值
        pub focus_on_shell: Mutex<bool>,
        /// 下一次 `show` / `reveal` 在「主线程」执行之前先运行它（模拟决定显示之后、主线程执行之前并发发生的事）
        pub before_show: Mutex<Option<Box<dyn FnOnce() + Send>>>,
        /// true 时 `hide` 的 `done` 不立即调用，而是放进 `deferred_done`（模拟 Linux 的离屏确认迟到）
        pub defer_hide: Mutex<bool>,
        pub deferred_done: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
        /// `busy` 的返回值（其他 Wisp 窗口在忙的原因）
        pub busy: Mutex<Option<String>>,
        /// 设置后，下一次 `create` 先等这个通道放行（模拟窗口还在建：旧窗口已销毁、新窗口还不存在）
        pub create_gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
        /// 最近一次 `show_shell` 交来的 Esc 回调（测试据此模拟外壳期间按 Esc）
        pub shell_escape: Mutex<Option<ShellEscape>>,
        /// 下一次 `show_shell` 在「主线程」执行之前先运行它（模拟显示期间页面加载完、ready 到达）
        pub before_shell: Mutex<Option<Box<dyn FnOnce() + Send>>>,
        /// true 时 `show_shell` 失败（票仍然有效，模拟主线程超时）
        pub fail_shell: Mutex<bool>,
        /// 每次 `create` 收到的 `Boot.prepare`（会话 id）
        pub boot_prepares: Mutex<Vec<Option<u64>>>,
        /// `primary_button_down` 的返回值（拖动中按住不动）
        pub button_down: Mutex<bool>,
        /// `screen_at` 查找的显示器（空时返回 None：控制器留在原来的屏上）
        pub screens: Mutex<Vec<Screen>>,
        /// `window_frame` 被调用的次数（读窗口要同步等主线程：只在需要时读）
        pub frame_reads: Mutex<usize>,
    }

    impl MockHost {
        /// 窗口已存在的宿主
        pub fn existing() -> Arc<Self> {
            let h = Arc::new(Self::default());
            *h.exists.lock() = true;
            h
        }

        fn push(&self, c: Call) {
            self.calls.lock().push(c);
        }

        /// 取出并清空调用记录
        pub fn take(&self) -> Vec<Call> {
            std::mem::take(&mut *self.calls.lock())
        }

        /// 取出全部计时器
        pub fn take_timers(&self) -> Vec<Timer> {
            std::mem::take(&mut *self.timers.lock())
        }

        /// 执行时长等于 `delay` 的计时器（其余的放回去）；执行中新排的计时器留到下次
        pub fn run_timers(&self, delay: Duration) {
            let (due, rest): (Vec<Timer>, Vec<Timer>) = self.take_timers().into_iter().partition(|(d, _)| *d == delay);
            self.timers.lock().extend(rest);
            for (_, f) in due {
                f();
            }
        }

        /// 排着的、时长为 `delay` 的普通计时器个数
        pub fn count_timers(&self, delay: Duration) -> usize {
            self.timers.lock().iter().filter(|(d, _)| *d == delay).count()
        }

        /// 还没被取消的、时长为 `delay` 的可取消计时器个数
        pub fn live_cancellable(&self, delay: Duration) -> usize {
            self.cancellable
                .lock()
                .iter()
                .filter(|(d, cancelled)| *d == delay && !cancelled.load(Ordering::Acquire))
                .count()
        }

        /// 触发窗口事件
        pub fn fire(&self, ev: PanelEvent) {
            let events = self.events.lock().clone();
            if let Some(events) = events {
                events(ev);
            }
        }
    }

    impl PanelHost for MockHost {
        fn exists(&self) -> bool {
            *self.exists.lock()
        }
        fn create(&self, events: PanelEvents, boot_prepare: Option<PanelPrepare>) -> Result<(), IpcError> {
            self.boot_prepares.lock().push(boot_prepare.map(|p| p.session_id));
            let gate = self.create_gate.lock().take();
            if let Some(gate) = gate {
                gate.recv_timeout(Duration::from_secs(5))
                    .map_err(|_| IpcError::internal("create gate was not opened"))?;
            }
            *self.events.lock() = Some(events);
            // 先记调用再置 exists：等 exists 的测试（wait_recreated）随后取调用记录时一定看得到 Create
            self.push(Call::Create);
            *self.exists.lock() = true;
            Ok(())
        }
        fn destroy(&self) {
            *self.exists.lock() = false;
            self.push(Call::Destroy);
        }
        fn restore_visibility(&self) {
            self.push(Call::RestoreVisibility);
        }
        fn release_visibility(&self, low_memory: bool) {
            self.push(Call::ReleaseVisibility(low_memory));
        }
        fn set_frame(&self, rect: Rect) -> Result<(), IpcError> {
            *self.frame.lock() = Some(rect);
            self.push(Call::SetFrame(rect));
            Ok(())
        }
        fn finish_exit(&self) {
            self.push(Call::FinishExit);
        }
        fn pin_webview(&self, height: f64) -> Result<(), IpcError> {
            self.push(Call::PinWebview(height));
            Ok(())
        }
        fn set_focusable(&self, focusable: bool) {
            self.push(Call::SetFocusable(focusable));
        }
        fn emit_prepare(&self, p: &PanelPrepare) -> Result<(), IpcError> {
            self.push(Call::EmitPrepare(p.session_id));
            Ok(())
        }
        fn emit_hide_request(&self, r: PanelHideRequest) {
            self.push(Call::EmitHideRequest(r.session_id, r.reason));
        }
        fn emit_focus_changed(&self, f: PanelFocusChanged) {
            self.push(Call::EmitFocusChanged(f.session_id, f.focused));
        }
        fn show(&self, rect: Rect, opts: ShowOpts, ticket: &ShowTicket) -> Result<(), IpcError> {
            let before = self.before_show.lock().take();
            if let Some(f) = before {
                f();
            }
            // 与原生实现一致：主线程执行前核对票
            if !ticket.valid() {
                self.push(Call::ShowSkipped);
                return Err(ShowTicket::superseded());
            }
            *self.frame.lock() = Some(rect);
            self.push(Call::Show(rect, opts));
            Ok(())
        }
        fn reveal(&self, rect: Rect, fade: Option<Fade>, key: bool, ticket: &ShowTicket) -> Result<(), IpcError> {
            let _ = (fade, key);
            let before = self.before_show.lock().take();
            if let Some(f) = before {
                f();
            }
            if !ticket.valid() {
                self.push(Call::ShowSkipped);
                return Err(ShowTicket::superseded());
            }
            *self.frame.lock() = Some(rect);
            self.push(Call::Reveal(rect));
            Ok(())
        }
        fn animate_height(
            &self,
            height: f64,
            placement: Placement,
            bound: Option<VerticalBound>,
            animate: bool,
            spring: ResizeSpring,
        ) {
            // 与原生实现一致：固定边不动，只改高度；有边界且窗口在边界内时伸出的部分整体推回
            if let Some(r) = self.frame.lock().as_mut() {
                let inside = bound.is_some_and(|b| r.y >= b.top - 1.0 && r.bottom() <= b.bottom + 1.0);
                if placement == Placement::Above {
                    r.y = r.bottom() - height;
                }
                r.height = height;
                if let Some(b) = bound.filter(|_| inside) {
                    r.y = r.y.min(b.bottom - height).max(b.top);
                }
            }
            if let Some(b) = bound {
                self.push(Call::HeightBound(b));
            }
            self.push(match spring {
                ResizeSpring::Gentle => Call::AnimateHeight(height, animate),
                ResizeSpring::Snappy => Call::AnimateSnappy(height, animate),
            });
        }
        fn freeze_height(&self) {
            self.push(Call::FreezeHeight);
        }
        fn switch_width(&self, switch: WidthSwitch) {
            // 计时不模拟：记下请求（含时刻），左边与宽度立即生效
            if let Some(r) = self.frame.lock().as_mut() {
                r.x = switch.x;
                r.width = switch.width;
            }
            self.push(Call::SwitchWidth(switch));
        }
        fn hide(&self, fade: Option<Fade>, done: Box<dyn FnOnce() + Send>) {
            self.push(Call::Hide(fade.is_some()));
            if *self.defer_hide.lock() {
                self.deferred_done.lock().push(done);
            } else {
                done();
            }
        }
        fn activate(&self) {
            self.push(Call::Activate);
        }
        fn start_drag(&self) {
            self.push(Call::StartDrag);
        }
        fn primary_button_down(&self) -> bool {
            *self.button_down.lock()
        }
        fn window_frame(&self) -> Option<Rect> {
            *self.frame_reads.lock() += 1;
            *self.frame.lock()
        }
        fn set_click_through(&self, region: Option<ClickRegion>) {
            self.push(Call::ClickThrough(region));
        }
        fn arm_guards(&self, session_id: u64, card: CardHit, spec: GuardSpec, key: bool, sink: HideSink) {
            let _ = (card, sink);
            self.push(Call::ArmGuards(session_id, spec, key));
        }
        fn update_guard_card(&self, card: CardHit) {
            self.push(Call::UpdateGuardCard(card.1));
        }
        fn set_guard_key(&self, key: bool) {
            self.push(Call::GuardKey(key));
        }
        fn pause_guards(&self, paused: bool) {
            self.push(Call::PauseGuards(paused));
        }
        fn disarm_guards(&self) {
            self.push(Call::DisarmGuards);
        }
        fn restores_focus(&self) -> bool {
            *self.restores_focus.lock()
        }
        fn focus_on_shell(&self) -> bool {
            *self.focus_on_shell.lock()
        }
        fn restore_focus(&self, prev: &AppRef) {
            let _ = prev;
            self.push(Call::RestoreFocus);
        }
        fn remember_position(&self, screen_id: &str, top_left: Point) {
            self.push(Call::Remember(screen_id.to_owned(), top_left));
        }
        fn screen_at(&self, desktop_point: Point) -> Option<Screen> {
            let screens = self.screens.lock();
            placement::screen_at_desktop(&screens, desktop_point, DesktopUnit::Logical).cloned()
        }
        fn perf(&self, session_id: u64, stage: PerfStage) {
            self.push(Call::Perf(session_id, stage));
        }
        fn perf_page(&self, session_id: u64, page: PanelPage) {
            self.push(Call::Page(session_id, page));
        }
        fn show_shell(
            &self,
            rect: Rect,
            _max_height: f64,
            fade: Option<Fade>,
            key: bool,
            on_escape: ShellEscape,
            ticket: &ShowTicket,
        ) -> Result<(), IpcError> {
            let before = self.before_shell.lock().take();
            if let Some(before) = before {
                before();
            }
            if !ticket.valid() {
                return Err(ShowTicket::superseded());
            }
            if *self.fail_shell.lock() {
                return Err(IpcError::internal("main thread timed out"));
            }
            *self.shell_escape.lock() = Some(on_escape);
            *self.frame.lock() = Some(rect);
            self.push(Call::ShowShell(rect, fade.is_some(), key));
            Ok(())
        }
        fn move_shell(&self, rect: Rect, max_height: Option<f64>, _animate: bool) {
            *self.frame.lock() = Some(rect);
            self.push(Call::MoveShell(rect, max_height));
        }
        fn reveal_shell(
            &self,
            rect: Rect,
            _placement: Placement,
            _fade: Fade,
            key: bool,
            _animate: bool,
            ticket: &ShowTicket,
        ) -> Result<(), IpcError> {
            if !ticket.valid() {
                return Err(ShowTicket::superseded());
            }
            self.push(Call::RevealShell(rect, key));
            Ok(())
        }
        fn schedule(&self, delay: Duration, f: Box<dyn FnOnce() + Send>) {
            self.push(Call::Schedule(delay));
            self.timers.lock().push((delay, f));
        }
        fn busy(&self) -> Option<String> {
            self.busy.lock().clone()
        }
        fn schedule_cancellable(&self, delay: Duration, f: Box<dyn FnOnce() + Send>) -> TimerHandle {
            let cancelled = Arc::new(AtomicBool::new(false));
            self.cancellable.lock().push((delay, Arc::clone(&cancelled)));
            let flag = Arc::clone(&cancelled);
            self.schedule(
                delay,
                Box::new(move || {
                    if !flag.load(Ordering::Acquire) {
                        f();
                    }
                }),
            );
            TimerHandle::new(move || cancelled.store(true, Ordering::Release))
        }
    }
}

#[cfg(test)]
mod tests {
    use wisp_types::{Lang, PhysPoint, TaskKind, TranslatePayload};

    use super::mock::{Call, MockHost};
    use super::*;
    use crate::windows::placement::{PlaceAnchor, place_panel};

    fn screen() -> Screen {
        Screen {
            id: "1".to_owned(),
            frame: Rect::new(0.0, 0.0, 1440.0, 900.0),
            work: Rect::new(0.0, 25.0, 1440.0, 805.0),
            scale: 2.0,
            physical_origin: PhysPoint { x: 0, y: 0 },
            primary: true,
        }
    }

    pub(crate) fn open(session_id: u64, view: PanelView, anchor: Rect) -> PanelOpen {
        let geometry = place_panel(&screen(), PlaceAnchor::Rect(anchor), 420.0, 120.0, 483.0);
        PanelOpen {
            session_id,
            view,
            payload: PanelPayload::Translate(TranslatePayload {
                source_text: "hello".to_owned(),
                request_id: None,
                task: TaskKind::Translate,
                style_id: "natural".to_owned(),
                detected_lang: None,
                target_lang: Lang::ZhCn,
                dict_mode: false,
                long_text: None,
                not_ready: None,
                selection_method: None,
            }),
            geometry,
            focused: matches!(view, PanelView::Input | PanelView::Clipboard),
            transition: PanelTransition::Enter,
            estimated_height: 120.0,
            prev_app: None,
            guards: GuardSpec {
                outside_click: view == PanelView::Translate,
                esc: view == PanelView::Translate,
                mouse_leave_px: None,
                blur_hides: matches!(view, PanelView::Input | PanelView::Clipboard),
            },
        }
    }

    /// 已加载的控制器
    pub(crate) fn loaded(host: &Arc<MockHost>, frame_mode: FrameMode, margin: f64) -> Arc<PanelController> {
        loaded_with(host, frame_mode, margin, false)
    }

    /// 已加载的控制器（`opaque`：Css 帧模式下窗口不能透明）
    fn loaded_with(host: &Arc<MockHost>, frame_mode: FrameMode, margin: f64, opaque: bool) -> Arc<PanelController> {
        *host.exists.lock() = false;
        let c = PanelController::new(
            Arc::clone(host) as Arc<dyn PanelHost>,
            frame_mode,
            margin,
            opaque,
            DesktopUnit::Logical,
        );
        c.create().expect("create");
        host.fire(PanelEvent::PageLoaded);
        host.take();
        c
    }

    /// 指定宽度与过渡方式的 prepare
    fn open_with(session_id: u64, anchor: Rect, width: f64, transition: PanelTransition) -> PanelOpen {
        let mut o = open(session_id, PanelView::Translate, anchor);
        o.geometry = place_panel(&screen(), PlaceAnchor::Rect(anchor), width, 120.0, 483.0);
        o.transition = transition;
        o
    }

    fn set_frames(calls: &[Call]) -> Vec<Rect> {
        calls
            .iter()
            .filter_map(|c| match c {
                Call::SetFrame(r) => Some(*r),
                _ => None,
            })
            .collect()
    }

    fn last_region(calls: &[Call]) -> Option<ClickRegion> {
        calls.iter().rev().find_map(|c| match c {
            Call::ClickThrough(r) => Some(*r),
            _ => None,
        })?
    }

    fn anchor_a() -> Rect {
        Rect::new(300.0, 200.0, 100.0, 18.0)
    }

    fn anchor_b() -> Rect {
        Rect::new(700.0, 400.0, 100.0, 18.0)
    }

    #[test]
    fn ready_shows_synchronously_with_fade_in_native() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        let calls = host.take();
        assert!(calls.contains(&Call::Schedule(READY_TIMEOUT)));
        let shown = c.ready(1, 200.0).expect("shown");
        assert_eq!(
            shown,
            PanelShown {
                session_id: 1,
                focused: false
            }
        );
        let calls = host.take();
        let show = calls.iter().find_map(|c| match c {
            Call::Show(r, o) => Some((*r, *o)),
            _ => None,
        });
        let (rect, opts) = show.expect("show called");
        assert_eq!(rect, Rect::new(300.0, 230.0, 420.0, 200.0), "Native：窗口即卡片");
        assert!(matches!(opts.alpha, ShowAlpha::FadeIn(_)));
        assert!(!opts.key);
        assert!(calls.contains(&Call::Perf(1, PerfStage::Ready)));
        // 先显示，监听排在显示之后（宿主异步安装，不推迟 ready 的返回）
        let show = calls.iter().position(|c| matches!(c, Call::Show(..))).expect("show");
        let arm = calls
            .iter()
            .position(|c| matches!(c, Call::ArmGuards(1, ..)))
            .expect("arm");
        assert!(show < arm, "{calls:?}");
        // 同一会话再次 ready：幂等，不重复显示；过期会话返回 None
        assert_eq!(c.ready(1, 200.0), Some(shown));
        assert!(!host.take().iter().any(|c| matches!(c, Call::Show(..))));
        assert!(c.ready(99, 200.0).is_none());
        c.hide_now(1, HideReason::Escape);
        assert!(c.ready(1, 200.0).is_none(), "已隐藏的会话");
    }

    #[test]
    fn css_window_has_margin_and_click_through() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Css, 24.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        let calls = host.take();
        let (rect, opts) = calls
            .iter()
            .find_map(|c| match c {
                Call::Show(r, o) => Some((*r, *o)),
                _ => None,
            })
            .expect("show");
        assert_eq!(rect, Rect::new(276.0, 206.0, 468.0, 168.0));
        assert_eq!(opts.alpha, ShowAlpha::Opaque);
        let region = calls
            .iter()
            .find_map(|c| match c {
                Call::ClickThrough(Some(r)) => Some(*r),
                _ => None,
            })
            .expect("click-through armed");
        assert_eq!(region.card, Rect::new(300.0, 230.0, 420.0, 120.0));
        assert!(region.passes_through(280.0, 210.0));
    }

    #[test]
    fn focused_views_take_key_on_show() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Clipboard, anchor_a()));
        assert!(host.take().contains(&Call::SetFocusable(true)));
        c.ready(1, 100.0).expect("shown");
        let calls = host.take();
        let opts = calls
            .iter()
            .find_map(|c| match c {
                Call::Show(_, o) => Some(*o),
                _ => None,
            })
            .expect("show");
        assert!(opts.key);
        // 剪贴板打开时高度至少 240
        let snap = c.snapshot().expect("snapshot");
        assert_eq!(snap.rect.height, 240.0);
    }

    #[test]
    fn clipboard_height_only_grows_within_session() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Clipboard, anchor_a()));
        c.ready(1, 300.0).expect("shown");
        host.take();
        c.resize(1, 260.0, ResizeSpring::Gentle);
        assert!(
            !host.take().iter().any(|c| matches!(c, Call::AnimateHeight(..))),
            "结果变少时不缩小"
        );
        c.resize(1, 340.0, ResizeSpring::Gentle);
        assert!(host.take().contains(&Call::AnimateHeight(340.0, true)));
    }

    #[test]
    fn ready_timeout_shows_transparent_then_late_ready_reveals() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        let timers = host.take_timers();
        host.take();
        for (_, f) in timers {
            f();
        }
        let calls = host.take();
        assert!(calls.iter().any(|c| matches!(
            c,
            Call::Show(
                _,
                ShowOpts {
                    alpha: ShowAlpha::Transparent,
                    ..
                }
            )
        )));
        assert!(calls.contains(&Call::Perf(1, PerfStage::ReadyTimeout)));
        assert!(calls.contains(&Call::Schedule(READY_WATCHDOG)));
        // 迟到的 ready：改到最终矩形并淡入，不再 show
        c.ready(1, 160.0).expect("late ready");
        let calls = host.take();
        assert!(calls.contains(&Call::Reveal(Rect::new(300.0, 230.0, 420.0, 160.0))));
        // 看门狗到期时已经显示了：什么都不做
        for (_, f) in host.take_timers() {
            f();
        }
        assert!(!host.take().contains(&Call::Destroy));
    }

    /// 新页面上 ready 超时之后每帧补发一次 prepare（同一时刻只有一个补发计时器），ready 一到就停
    #[test]
    fn fresh_page_resends_prepare_until_ready() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        let calls = host.take();
        assert_eq!(calls.iter().filter(|c| **c == Call::EmitPrepare(1)).count(), 1);
        let first = READY_TIMEOUT + FRESH_PAGE_RESEND_EVERY;
        assert_eq!(host.count_timers(first), 1, "第一次补发排在超时之后一帧");
        // 40 ms 超时先到：alpha 0 显示
        host.run_timers(READY_TIMEOUT);
        host.run_timers(first);
        assert!(host.take().contains(&Call::EmitPrepare(1)));
        // 每帧一次，链式排下一次
        for _ in 0..3 {
            assert_eq!(host.count_timers(FRESH_PAGE_RESEND_EVERY), 1);
            host.run_timers(FRESH_PAGE_RESEND_EVERY);
            assert_eq!(host.take().iter().filter(|c| **c == Call::EmitPrepare(1)).count(), 1);
        }
        // ready 到达后不再补发，链也断了
        c.ready(1, 120.0).expect("shown");
        host.take();
        host.run_timers(FRESH_PAGE_RESEND_EVERY);
        assert!(!host.take().contains(&Call::EmitPrepare(1)));
        assert_eq!(host.count_timers(FRESH_PAGE_RESEND_EVERY), 0);
    }

    /// 补发有上限：监听始终没有注册时补满 FRESH_PAGE_RESEND_MAX 次就停（之后交给 ready 看门狗）
    #[test]
    fn fresh_page_resends_are_bounded() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        host.take();
        host.run_timers(READY_TIMEOUT);
        host.run_timers(READY_TIMEOUT + FRESH_PAGE_RESEND_EVERY);
        for _ in 0..200 {
            host.run_timers(FRESH_PAGE_RESEND_EVERY);
        }
        let n = host.take().iter().filter(|c| **c == Call::EmitPrepare(1)).count();
        assert_eq!(n, usize::from(FRESH_PAGE_RESEND_MAX));
        assert_eq!(host.count_timers(FRESH_PAGE_RESEND_EVERY), 0);
    }

    #[test]
    fn watchdog_rebuilds_unresponsive_webview_and_replays() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        // 40 ms 超时
        for (_, f) in host.take_timers() {
            f();
        }
        // 后台重建先拦住：否则快的机器上 spawn_blocking 的重建会在下面的断言之前完成（Windows CI）
        let (open_gate, gate) = std::sync::mpsc::channel();
        *host.create_gate.lock() = Some(gate);
        // 1 s 看门狗
        for (_, f) in host.take_timers() {
            f();
        }
        let calls = host.take();
        assert!(calls.contains(&Call::Destroy), "{calls:?}");
        assert!(!*host.exists.lock());
        open_gate.send(()).expect("open the create gate");
        // 后台重建（spawn_blocking）完成后页面加载：重放同一个 session 的 prepare
        let deadline = Instant::now() + Duration::from_secs(5);
        while !*host.exists.lock() {
            assert!(Instant::now() < deadline, "panel was not recreated");
            std::thread::sleep(Duration::from_millis(5));
        }
        host.fire(PanelEvent::PageLoaded);
        let calls = host.take();
        assert!(calls.contains(&Call::EmitPrepare(1)), "{calls:?}");
        assert!(c.ready(1, 120.0).is_some());
    }

    #[test]
    fn retrigger_elsewhere_exits_first_then_enters() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        host.take();
        // 锚点变了：先淡出旧会话（MockHost 的 hide 立即完成），再在新位置 prepare
        c.prepare(open(2, PanelView::Translate, anchor_b()));
        let calls = host.take();
        let hide = calls.iter().position(|c| *c == Call::Hide(true)).expect("fade out");
        let emit = calls
            .iter()
            .position(|c| *c == Call::EmitPrepare(2))
            .expect("prepare 2");
        assert!(hide < emit);
        // 旧会话的 ready 失效
        assert!(c.ready(1, 120.0).is_none());
        assert!(c.ready(2, 120.0).is_some());
    }

    #[test]
    fn retrigger_after_timeout_prepares_fresh_and_fades_in() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        let mut timers = host.take_timers();
        timers.sort_by_key(|(d, _)| *d);
        let (_, timeout) = timers.remove(0);
        timeout();
        host.take();
        // 同一位置再次触发：上一个会话只以 alpha 0 显示过，新会话必须正常淡入，而不是原地替换
        c.prepare(open(2, PanelView::Translate, anchor_a()));
        c.ready(2, 120.0).expect("shown");
        let calls = host.take();
        assert!(
            calls.iter().any(|c| matches!(
                c,
                Call::Show(
                    _,
                    ShowOpts {
                        alpha: ShowAlpha::FadeIn(_),
                        ..
                    }
                )
            )),
            "{calls:?}"
        );
    }

    #[test]
    fn retrigger_in_place_keeps_window_visible() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        host.take();
        c.prepare(open(2, PanelView::Translate, anchor_a()));
        let calls = host.take();
        assert!(
            !calls.iter().any(|c| matches!(c, Call::Hide(_) | Call::SetFrame(_))),
            "{calls:?}"
        );
        assert!(calls.contains(&Call::EmitPrepare(2)));
        c.ready(2, 200.0).expect("shown in place");
        let calls = host.take();
        assert!(!calls.iter().any(|c| matches!(c, Call::Show(..))));
        assert!(calls.contains(&Call::AnimateHeight(200.0, true)));
    }

    /// 收到一次结束通知并取出它的值（watch 通道只保留最新值）
    fn announced(rx: &mut watch::Receiver<u64>) -> Option<u64> {
        rx.has_changed().expect("sender alive").then(|| *rx.borrow_and_update())
    }

    /// **每个**会话结束都经 hidden 通道通知（原地替换、准备中被取代、排队 / 暂存的 prepare 被覆盖、
    /// 截图作废），通知只是信号，编排器用 is_live 核对
    #[test]
    fn every_ended_session_is_announced_and_no_longer_live() {
        // 原地替换（同位置再触发 / 交叉淡入）：被取代的旧会话立即结束，新会话接管窗口
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        let mut rx = c.hidden();
        let mut first = open(1, PanelView::Translate, anchor_a());
        first.prev_app = Some(AppRef {
            id: "com.example.notes".to_owned(),
            alt_id: None,
            name: "Notes".to_owned(),
            pid: Some(42),
            path: None,
            window: None,
        });
        c.prepare(first);
        c.ready(1, 120.0).expect("shown");
        assert!(c.is_live(1));
        assert_eq!(c.prev_app(1).map(|a| a.id), Some("com.example.notes".to_owned()));
        assert_eq!(c.card_width(1), Some(420.0));
        assert_eq!(announced(&mut rx), None);
        c.prepare(open(2, PanelView::Translate, anchor_a()));
        assert_eq!(announced(&mut rx), Some(1));
        assert!(!c.is_live(1) && c.is_live(2));
        assert_eq!(c.prev_app(1), None, "结束的会话不再给出 prev_app");
        // 隐藏完成：通知；隐藏中的会话已经不是 live
        c.hide_now(2, HideReason::Escape);
        assert_eq!(announced(&mut rx), Some(2));
        assert!(!c.is_live(2));
        assert_eq!(c.card_width(2), None);

        // 准备中（还没显示）又来新的 prepare：旧会话被取代即结束
        c.prepare(open(3, PanelView::Translate, anchor_a()));
        assert!(c.is_live(3));
        c.prepare(open(4, PanelView::Translate, anchor_b()));
        assert_eq!(announced(&mut rx), Some(3));
        assert!(!c.is_live(3) && c.is_live(4));

        // Css「先退出再进入」：排队的 prepare 被更新的覆盖 → 被覆盖的那次结束；截图前隐藏作废排队的 prepare 并隐藏
        // 当前会话，两者都通知（接连两次，只看得到最后一个，所以要靠 is_live 核对）
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Css, 24.0);
        let mut rx = c.hidden();
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        c.prepare(open(2, PanelView::Translate, anchor_b()));
        assert!(c.is_live(1) && c.is_live(2), "旧会话在播退出动画，新会话排队");
        assert_eq!(announced(&mut rx), None);
        c.prepare(open(3, PanelView::Translate, anchor_b()));
        assert_eq!(announced(&mut rx), Some(2));
        assert!(!c.is_live(2) && c.is_live(3));
        assert!(c.hide_for_capture(Duration::from_millis(50)));
        assert!(announced(&mut rx).is_some());
        assert!(!c.is_live(1) && !c.is_live(3), "截图前隐藏结束了当前与排队的会话");

        // 窗口重建中（暂存的 prepare）被覆盖
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        let mut rx = c.hidden();
        *host.exists.lock() = false;
        c.prepare(open(5, PanelView::Translate, anchor_a()));
        c.prepare(open(6, PanelView::Translate, anchor_a()));
        assert_eq!(announced(&mut rx), Some(5));
        assert!(!c.is_live(5) && c.is_live(6));
    }

    /// 隐藏**开始**时就经 hidden 通知（编排器立即取消请求，不等退出动画），
    /// 窗口真正离屏时才经 offscreen 通知（粘贴等它再发键）
    #[test]
    fn hiding_announces_at_start_and_offscreen_only_after_the_exit() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        let mut hidden = c.hidden();
        let mut offscreen = c.offscreen();
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        *host.defer_hide.lock() = true;
        assert!(c.hide_now(1, HideReason::Escape));
        assert_eq!(announced(&mut hidden), Some(1), "退出动画开始就通知");
        assert!(!c.is_live(1));
        assert_eq!(announced(&mut offscreen), None, "窗口还在屏幕上（淡出中）");
        let deferred: Vec<_> = host.deferred_done.lock().drain(..).collect();
        for done in deferred {
            done();
        }
        assert_eq!(announced(&mut offscreen), Some(1));
        assert_eq!(announced(&mut hidden), Some(1), "离屏完毕再通知一次（重复无害）");

        // 固定交接：begin_external_hide_for 同样在开始时通知
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        let mut hidden = c.hidden();
        let mut offscreen = c.offscreen();
        c.prepare(open(2, PanelView::Translate, anchor_a()));
        c.ready(2, 120.0).expect("shown");
        assert!(c.begin_external_hide_for(2));
        assert_eq!(announced(&mut hidden), Some(2));
        assert_eq!(announced(&mut offscreen), None);
        c.finish_external_hide(2, HideReason::Pin);
        assert_eq!(announced(&mut offscreen), Some(2));
    }

    #[test]
    fn css_retrigger_asks_frontend_to_exit() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Css, 24.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        host.take();
        c.prepare(open(2, PanelView::Translate, anchor_b()));
        let calls = host.take();
        assert!(calls.contains(&Call::EmitHideRequest(1, HideReason::Retrigger)));
        assert!(!calls.contains(&Call::EmitPrepare(2)), "等旧会话退出");
        // 前端播完退出动画后 panel_hide(1)
        c.hide_now(1, HideReason::Retrigger);
        let calls = host.take();
        assert!(calls.contains(&Call::EmitPrepare(2)), "{calls:?}");
    }

    #[test]
    fn hide_releases_guards_and_visibility() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Css, 24.0);
        c.set_options(PanelOptions {
            low_memory_when_hidden: true,
            ..PanelOptions::default()
        });
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        host.take();
        c.hide_now(1, HideReason::Escape);
        let calls = host.take();
        assert!(calls.contains(&Call::DisarmGuards));
        assert!(calls.contains(&Call::ClickThrough(None)));
        assert!(calls.contains(&Call::Hide(false)), "Css 帧模式立即隐藏");
        assert!(calls.contains(&Call::ReleaseVisibility(true)));
        assert!(c.snapshot().is_none());
        assert!(c.last_card().is_some());
    }

    #[test]
    fn paste_hides_without_fade_in_native() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Clipboard, anchor_a()));
        c.ready(1, 300.0).expect("shown");
        host.take();
        c.hide_now(1, HideReason::Paste);
        assert!(host.take().contains(&Call::Hide(false)));
    }

    /// 由 Rust 发起的隐藏（WindowManager::panel_hide_now，例如粘贴）补发 hide_request，前端据此卸载视图、
    /// 设置 data-hidden；会话已在隐藏或已结束时不再补发（前端自己发起的 panel_hide 收到的回声由前端忽略）
    #[test]
    fn rust_initiated_hide_tells_the_frontend() {
        for mode in [FrameMode::Native, FrameMode::Css] {
            let host = MockHost::existing();
            let c = loaded(&host, mode, if mode == FrameMode::Css { 24.0 } else { 0.0 });
            c.prepare(open(1, PanelView::Clipboard, anchor_a()));
            c.ready(1, 300.0).expect("shown");
            host.take();
            c.hide_now_and_notify(1, HideReason::Paste);
            let calls = host.take();
            let hide = calls.iter().position(|c| matches!(c, Call::Hide(_)));
            let emit = calls
                .iter()
                .position(|c| *c == Call::EmitHideRequest(1, HideReason::Paste));
            assert!(
                hide.zip(emit).is_some_and(|(h, e)| h < e),
                "{mode:?}：先隐藏，再告诉前端：{calls:?}"
            );
            c.hide_now_and_notify(1, HideReason::Paste);
            c.hide_now_and_notify(7, HideReason::Paste);
            assert!(
                !host.take().iter().any(|c| matches!(c, Call::EmitHideRequest(..))),
                "{mode:?}：已隐藏 / 未知的会话不再补发"
            );
        }
    }

    /// Native 帧模式在别处再次触发：旧会话由 Rust 淡出，同样告诉前端（排队的 prepare 可能被截图作废而不再到来）
    #[test]
    fn native_retrigger_tells_the_frontend_the_old_session_ended() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        host.take();
        c.prepare(open(2, PanelView::Translate, anchor_b()));
        let calls = host.take();
        assert!(
            calls.contains(&Call::EmitHideRequest(1, HideReason::Retrigger)),
            "{calls:?}"
        );
        assert!(calls.iter().any(|c| matches!(c, Call::Hide(_))), "{calls:?}");
    }

    #[test]
    fn blur_hides_focused_views_after_they_were_key() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Input, anchor_a()));
        c.ready(1, 168.0).expect("shown");
        host.take();
        host.fire(PanelEvent::Focused(true));
        host.fire(PanelEvent::Focused(false));
        let calls = host.take();
        assert!(calls.contains(&Call::EmitFocusChanged(1, true)));
        assert!(calls.contains(&Call::EmitHideRequest(1, HideReason::Blur)));
    }

    #[test]
    fn translate_view_toggles_esc_guard_with_key() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        let calls = host.take();
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, Call::ArmGuards(1, spec, false) if spec.esc && spec.outside_click))
        );
        host.fire(PanelEvent::Focused(true));
        let calls = host.take();
        assert!(calls.contains(&Call::GuardKey(true)));
        assert!(
            !calls.iter().any(|c| matches!(c, Call::EmitHideRequest(..))),
            "翻译视图失焦不关闭"
        );
        host.fire(PanelEvent::Focused(false));
        assert!(host.take().contains(&Call::GuardKey(false)));
    }

    /// ready 一到就记 Ready，原生窗口上屏之后记 WindowShown 与这次用的页面；要焦点的视图成为 key 时记
    /// KeyGained（翻译视图被点进去不记）。页面：启动预热 / 回收出来还没用过的是 recycled，用过的是 warm，卸载之后要先建窗口、
    /// 等页面加载的是 cold
    #[test]
    fn ready_window_key_and_page_marks() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.set_options(PanelOptions {
            unload_after: Some(Duration::from_secs(600)),
            ..PanelOptions::default()
        });
        c.prepare(open(1, PanelView::Input, anchor_a()));
        host.take();
        c.ready(1, 120.0).expect("shown");
        let calls = host.take();
        let at = |want: &Call| {
            calls
                .iter()
                .position(|c| c == want)
                .unwrap_or_else(|| panic!("{want:?}: {calls:?}"))
        };
        let show = calls.iter().position(|c| matches!(c, Call::Show(..))).expect("show");
        assert!(at(&Call::Perf(1, PerfStage::Ready)) < show, "ready 到达即记，早于显示");
        assert!(show < at(&Call::Perf(1, PerfStage::WindowShown)));
        assert!(
            calls.contains(&Call::Page(1, PanelPage::Recycled)),
            "预热出来、第一次用"
        );
        host.fire(PanelEvent::Focused(true));
        assert!(host.take().contains(&Call::Perf(1, PerfStage::KeyGained)));
        c.hide_now(1, HideReason::Escape);
        // 用过的页面：warm；翻译视图被点进去不记 KeyGained
        c.prepare(open(2, PanelView::Translate, anchor_a()));
        c.ready(2, 120.0).expect("shown");
        assert!(host.take().contains(&Call::Page(2, PanelPage::Warm)));
        host.fire(PanelEvent::Focused(true));
        assert!(
            !host
                .take()
                .iter()
                .any(|c| matches!(c, Call::Perf(2, PerfStage::KeyGained)))
        );
        c.hide_now(2, HideReason::Escape);
        // 卸载之后：cold
        host.run_timers(Duration::from_secs(600));
        host.take_timers().into_iter().for_each(|(d, f)| {
            if d == Duration::from_secs(600) {
                f();
            }
        });
        assert!(!*host.exists.lock(), "已卸载");
        c.prepare(open(3, PanelView::Translate, anchor_a()));
        wait_recreated(&host);
        host.fire(PanelEvent::PageLoaded);
        c.ready(3, 120.0).expect("shown");
        assert!(host.take().contains(&Call::Page(3, PanelPage::Cold)));
        c.hide_now(3, HideReason::Escape);
        // 冷唤出之后同一个页面再用：warm
        c.prepare(open(4, PanelView::Translate, anchor_a()));
        c.ready(4, 120.0).expect("shown");
        assert!(host.take().contains(&Call::Page(4, PanelPage::Warm)));
    }

    #[test]
    fn prepare_waits_for_page_load_after_unload() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.set_options(PanelOptions {
            unload_after: Some(Duration::from_secs(1800)),
            ..PanelOptions::default()
        });
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        c.hide_now(1, HideReason::Escape);
        host.take_timers().into_iter().for_each(|(d, f)| {
            if d == Duration::from_secs(1800) {
                f();
            }
        });
        assert!(host.take().contains(&Call::Destroy));
        c.prepare(open(2, PanelView::Translate, anchor_a()));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !*host.exists.lock() {
            assert!(Instant::now() < deadline, "panel was not recreated");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!host.take().contains(&Call::EmitPrepare(2)), "页面加载前不 emit");
        host.fire(PanelEvent::PageLoaded);
        assert!(host.take().contains(&Call::EmitPrepare(2)));
    }

    #[test]
    fn reload_while_shown_hides_session() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        host.take();
        host.fire(PanelEvent::PageLoaded);
        assert!(host.take().contains(&Call::Hide(false)));
        assert!(c.snapshot().is_none());
        assert_eq!(*c.hidden().borrow(), 1, "经 hidden 通道告知编排器收尾");
    }

    #[test]
    fn css_resize_steps_and_above_keeps_window() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Css, 24.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        host.take();
        c.resize(1, 130.0, ResizeSpring::Gentle);
        let calls = host.take();
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, Call::SetFrame(r) if r.height == 168.0 + 48.0)),
            "{calls:?}"
        );
        // Above 放置：窗口一开始就是上限高度，之后不改
        let above = Rect::new(500.0, 760.0, 100.0, 18.0);
        c.hide_now(1, HideReason::Escape);
        c.prepare(open(2, PanelView::Translate, above));
        c.ready(2, 120.0).expect("shown");
        let calls = host.take();
        let rect = calls
            .iter()
            .find_map(|c| match c {
                Call::Show(r, _) => Some(*r),
                _ => None,
            })
            .expect("show");
        // 上方可用 715、上限 483：窗口 = 上限 + 2·m，底边 = 卡片底边 + m
        assert_eq!(rect.height, 483.0 + 48.0);
        assert_eq!(rect.bottom(), 748.0 + 24.0);
        c.resize(2, 300.0, ResizeSpring::Gentle);
        assert!(!host.take().iter().any(|c| matches!(c, Call::SetFrame(_))));
    }

    /// 拖动：系统把窗口移到 `to`（桌面坐标的窗口外框），移动事件 → 跟随（读位置排在计时器线程上）
    fn drag_to(host: &MockHost, to: Rect) {
        *host.frame.lock() = Some(to);
        host.fire(PanelEvent::Moved);
        host.run_timers(Duration::ZERO);
    }

    /// 剪贴板（「记住位置」）：隐藏时记住拖到的位置（卡片左上角）
    #[test]
    fn drag_marks_and_remembers_clipboard_position() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Clipboard, anchor_a()));
        c.ready(1, 300.0).expect("shown");
        host.take_timers();
        host.take();
        c.drag_start(1);
        assert!(host.take().contains(&Call::StartDrag));
        drag_to(&host, Rect::new(640.0, 260.0, 420.0, 300.0));
        host.run_timers(DRAG_SETTLE);
        c.hide_now(1, HideReason::Escape);
        assert!(
            host.take()
                .contains(&Call::Remember("1".to_owned(), Point { x: 640.0, y: 260.0 })),
            "记住拖到的位置"
        );
    }

    /// 翻译（以及剪贴板以外的视图）同样能拖动，只影响本次会话——隐藏时不记位置，下次唤出仍按锚点
    #[test]
    fn dragging_other_views_affects_this_session_only() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        for (id, view) in [(1, PanelView::Translate), (2, PanelView::Ocr), (3, PanelView::Input)] {
            c.prepare(open(id, view, anchor_a()));
            c.ready(id, 120.0).expect("shown");
            host.take_timers();
            host.take();
            c.drag_start(id);
            let calls = host.take();
            assert!(calls.contains(&Call::StartDrag), "{view:?} {calls:?}");
            assert!(calls.contains(&Call::PauseGuards(true)), "拖动期间暂停「鼠标离开」");
            drag_to(&host, Rect::new(700.0, 300.0, 420.0, 120.0));
            host.run_timers(DRAG_SETTLE);
            let calls = host.take();
            assert!(calls.contains(&Call::PauseGuards(false)), "{calls:?}");
            assert_eq!(
                c.snapshot().expect("snapshot").rect,
                Rect::new(700.0, 300.0, 420.0, 120.0)
            );
            c.hide_now(id, HideReason::Escape);
            assert!(
                !host.take().iter().any(|c| matches!(c, Call::Remember(..))),
                "{view:?}：不记位置"
            );
        }
        c.prepare(open(4, PanelView::Translate, anchor_a()));
        c.ready(4, 120.0).expect("shown");
        assert_eq!(
            c.snapshot().expect("snapshot").rect,
            Rect::new(300.0, 230.0, 420.0, 120.0),
            "下次唤出按锚点定位"
        );
    }

    /// 只按了一下（窗口没动）也要结束拖动状态，恢复「鼠标离开」，之后的高度变化照常改窗口
    #[test]
    fn drag_start_without_movement_ends_quietly() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Css, 24.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        host.take_timers();
        host.take();
        c.drag_start(1);
        host.take();
        host.run_timers(DRAG_SETTLE);
        let calls = host.take();
        assert!(calls.contains(&Call::PauseGuards(false)), "{calls:?}");
        assert!(set_frames(&calls).is_empty(), "没动：不改窗口");
        c.resize(1, 200.0, ResizeSpring::Gentle);
        let frame = set_frames(&host.take()).last().copied().expect("resized");
        assert_eq!((frame.x, frame.y), (276.0, 206.0));
    }

    /// Css 帧模式拖动中——几何跟随窗口（关闭监听的卡片矩形、点击穿透区域随之更新），流式长高不改窗口（不和系统的拖动
    /// 抢位置）；按住不动不算结束；松开后推迟的尺寸一次放到位，之后继续长高顶边不动、不跳回原锚点
    #[test]
    fn css_drag_follows_window_and_keeps_position_while_growing() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Css, 24.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        host.take_timers();
        host.take();
        let start = host.window_frame().expect("frame");
        assert_eq!(start, Rect::new(276.0, 206.0, 468.0, 168.0));
        c.drag_start(1);
        host.take();
        *host.button_down.lock() = true;
        drag_to(&host, Rect::new(476.0, 306.0, 468.0, 168.0));
        let calls = host.take();
        assert!(
            calls.contains(&Call::UpdateGuardCard(Rect::new(500.0, 330.0, 420.0, 120.0))),
            "{calls:?}"
        );
        let region = last_region(&calls).expect("click-through follows");
        assert_eq!(region.card, Rect::new(500.0, 330.0, 420.0, 120.0));
        // 拖动中流式长高：不改窗口
        c.resize(1, 200.0, ResizeSpring::Gentle);
        assert!(set_frames(&host.take()).is_empty());
        // 停下但还按着：继续等
        host.run_timers(DRAG_SETTLE);
        assert!(!host.take().contains(&Call::PauseGuards(false)));
        // 松开：拖动结束，推迟的窗口尺寸一次放到位，位置是拖到的位置
        *host.button_down.lock() = false;
        host.run_timers(DRAG_SETTLE);
        let calls = host.take();
        assert!(calls.contains(&Call::PauseGuards(false)), "{calls:?}");
        let frame = set_frames(&calls).last().copied().expect("pending frame applied");
        assert_eq!((frame.x, frame.y), (476.0, 306.0));
        assert!(frame.height >= 200.0 + 48.0, "{frame:?}");
        // 之后继续长高（越过阶梯推迟的间隔）：顶边不动
        std::thread::sleep(sizing::CSS_GROW_MIN_INTERVAL);
        c.resize(1, 330.0, ResizeSpring::Gentle);
        let frame = set_frames(&host.take()).last().copied().expect("grown");
        assert_eq!((frame.x, frame.y), (476.0, 306.0));
        assert!(frame.height >= 330.0 + 48.0, "{frame:?}");
        assert_eq!(
            c.snapshot().expect("snapshot").rect,
            Rect::new(500.0, 330.0, 420.0, 330.0)
        );
    }

    /// Native 帧模式——开始拖动时进行中的高度弹簧停在当前高度；拖动中流式长高不动窗口（逐帧 setFrame 会和系统的拖动
    /// 抢位置）；松开后弹簧从窗口当前高度继续（带竖直边界）。拖到工作区底部附近后流式长高伸出工作区时，弹簧带着边界把卡片
    /// 整体推回（底边贴住工作区底），原生窗口与会话几何一致；变矮时顶边不动
    #[test]
    fn native_drag_then_grow_pushes_card_back_into_work_area() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        host.take_timers();
        host.take();
        c.drag_start(1);
        let calls = host.take();
        let freeze = calls.iter().position(|c| *c == Call::FreezeHeight).expect("freeze");
        let start = calls.iter().position(|c| *c == Call::StartDrag).expect("start drag");
        assert!(freeze < start, "先停弹簧再交给系统拖动：{calls:?}");
        drag_to(&host, Rect::new(600.0, 650.0, 420.0, 120.0));
        c.resize(1, 160.0, ResizeSpring::Gentle);
        let calls = host.take();
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, Call::AnimateHeight(..) | Call::SetFrame(_))),
            "拖动中不改窗口：{calls:?}"
        );
        assert_eq!(host.window_frame(), Some(Rect::new(600.0, 650.0, 420.0, 120.0)));
        assert_eq!(
            c.snapshot().expect("snapshot").rect,
            Rect::new(600.0, 650.0, 420.0, 160.0),
            "几何已跟随、高度是目标高度"
        );
        host.run_timers(DRAG_SETTLE);
        let calls = host.take();
        assert!(set_frames(&calls).is_empty(), "卡片在工作区里：松开后不移窗：{calls:?}");
        assert!(calls.contains(&Call::AnimateHeight(160.0, true)), "{calls:?}");
        assert!(
            calls.contains(&Call::HeightBound(VerticalBound {
                top: 33.0,
                bottom: 822.0
            })),
            "{calls:?}"
        );
        assert_eq!(host.window_frame(), Some(Rect::new(600.0, 650.0, 420.0, 160.0)));
        // 流式继续长高到 400：伸出工作区底（830 − 8）→ 整体上移
        c.resize(1, 400.0, ResizeSpring::Gentle);
        let calls = host.take();
        assert!(
            calls.contains(&Call::HeightBound(VerticalBound {
                top: 33.0,
                bottom: 822.0
            })),
            "{calls:?}"
        );
        assert!(calls.contains(&Call::AnimateHeight(400.0, true)), "{calls:?}");
        let snap = c.snapshot().expect("snapshot");
        assert_eq!(snap.rect, Rect::new(600.0, 422.0, 420.0, 400.0));
        assert_eq!(host.window_frame(), Some(snap.rect), "原生弹簧推回的位置与会话几何一致");
        // 变矮：顶边不动
        c.resize(1, 200.0, ResizeSpring::Gentle);
        let snap = c.snapshot().expect("snapshot");
        assert_eq!(snap.rect, Rect::new(600.0, 422.0, 420.0, 200.0));
        assert_eq!(host.window_frame(), Some(snap.rect));
        // 推回的弹簧还在半路（窗口此刻顶边 522、高 300，与几何不一致）时又变矮：弹簧从窗口当前的位置继续，几何先按窗口同步，
        // 终点一致
        c.resize(1, 400.0, ResizeSpring::Gentle);
        *host.frame.lock() = Some(Rect::new(600.0, 522.0, 420.0, 300.0));
        c.resize(1, 250.0, ResizeSpring::Gentle);
        let snap = c.snapshot().expect("snapshot");
        assert_eq!(snap.rect, Rect::new(600.0, 522.0, 420.0, 250.0));
        assert_eq!(host.window_frame(), Some(snap.rect));
    }

    /// 拖过的面板里视图内切换（输入 → 翻译，`panel_set_width`）不按新位置缩小高度上限（前端仍按 prepare 的上限排版），
    /// 长高照样推回；交叉淡入（剪贴板条目 → 翻译）原地替换，留在拖到的位置
    #[test]
    fn dragged_panel_keeps_position_through_view_switches() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Input, anchor_a()));
        c.ready(1, 168.0).expect("shown");
        host.take_timers();
        host.take();
        c.drag_start(1);
        drag_to(&host, Rect::new(500.0, 640.0, 420.0, 168.0));
        host.run_timers(DRAG_SETTLE);
        c.set_width(1, 520.0);
        c.resize(1, 450.0, ResizeSpring::Gentle);
        let calls = host.take();
        assert!(
            calls.contains(&Call::AnimateHeight(450.0, true)),
            "上限仍是 483：{calls:?}"
        );
        let snap = c.snapshot().expect("snapshot");
        assert_eq!(snap.rect, Rect::new(500.0, 372.0, 520.0, 450.0));
        assert_eq!(host.window_frame(), Some(snap.rect));
        // 交叉淡入：保留当前（拖动后、推回后）的位置
        c.prepare(open_with(2, anchor_b(), 520.0, PanelTransition::Crossfade));
        c.ready(2, 300.0).expect("shown in place");
        let snap = c.snapshot().expect("snapshot");
        assert_eq!((snap.rect.x, snap.rect.y), (500.0, 372.0));
        assert_eq!(host.window_frame(), Some(snap.rect));
    }

    /// 松开时卡片伸出屏幕边缘 → 夹回工作区；拖到另一块屏（混合缩放，物理像素的桌面坐标）→ 按那块屏的工作区与缩放
    /// 夹紧，之后的窗口尺寸按新屏的缩放换算
    #[test]
    fn drag_release_clamps_into_the_work_area_of_the_new_screen() {
        let host = MockHost::existing();
        let a = screen();
        let b = Screen {
            id: "2".to_owned(),
            frame: Rect::new(1440.0, 0.0, 1920.0, 1080.0),
            work: Rect::new(0.0, 0.0, 1920.0, 1040.0),
            scale: 1.0,
            physical_origin: PhysPoint { x: 2880, y: 0 },
            primary: false,
        };
        *host.screens.lock() = vec![a, b];
        let c = PanelController::new(
            Arc::clone(&host) as Arc<dyn PanelHost>,
            FrameMode::Css,
            24.0,
            false,
            DesktopUnit::Physical,
        );
        *host.exists.lock() = false;
        c.create().expect("create");
        host.fire(PanelEvent::PageLoaded);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        host.take_timers();
        host.take();
        assert_eq!(
            host.window_frame(),
            Some(Rect::new(552.0, 412.0, 936.0, 336.0)),
            "A 屏 2×"
        );
        c.drag_start(1);
        // 拖到 B 屏（1×，系统按新缩放把窗口改成 468×168 物理像素），右边伸出 B 屏
        drag_to(&host, Rect::new(4580.0, 300.0, 468.0, 168.0));
        host.run_timers(DRAG_SETTLE);
        let calls = host.take();
        let frame = set_frames(&calls).last().copied().expect("clamped");
        // 卡片左边夹到 1920 − 8 − 420 = 1492（本屏逻辑坐标）→ 窗口物理 x = 2880 + 1492 − 24
        assert_eq!(frame, Rect::new(4348.0, 300.0, 468.0, 168.0));
        let snap = c.snapshot().expect("snapshot");
        assert_eq!(snap.screen.id, "2");
        assert_eq!(snap.rect, Rect::new(1492.0, 324.0, 420.0, 120.0));
        // 之后长高：按 B 屏的 1× 换算
        c.resize(1, 200.0, ResizeSpring::Gentle);
        let frame = set_frames(&host.take()).last().copied().expect("grown");
        assert_eq!((frame.x, frame.y), (4348.0, 300.0));
        assert!(frame.height >= 248.0 && frame.height < 400.0, "{frame:?}");
        // 拖到工作区外的下方：顶边夹回，卡片整个留在工作区里
        c.drag_start(1);
        let h = host.window_frame().expect("frame").height;
        drag_to(&host, Rect::new(3000.0, 1000.0, 468.0, h));
        host.run_timers(DRAG_SETTLE);
        let snap = c.snapshot().expect("snapshot");
        assert_eq!(snap.rect.bottom(), 1040.0 - 8.0, "{snap:?}");
    }

    /// Css 帧模式 Above 放置的面板拖到靠近工作区顶部后向上长高：顶边到顶就整窗下移（窗口高度仍是会话上限）
    #[test]
    fn css_above_dragged_panel_grows_down_when_top_is_reached() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Css, 24.0);
        c.prepare(open(1, PanelView::Translate, Rect::new(300.0, 700.0, 100.0, 18.0)));
        c.ready(1, 120.0).expect("shown");
        host.take_timers();
        host.take();
        let start = host.window_frame().expect("frame");
        assert_eq!(
            start,
            Rect::new(276.0, 712.0 - 531.0, 468.0, 531.0),
            "Above：窗口 = 上限 + 2·m"
        );
        c.drag_start(1);
        drag_to(
            &host,
            Rect {
                y: start.y - 300.0,
                ..start
            },
        );
        host.run_timers(DRAG_SETTLE);
        host.take();
        assert_eq!(
            c.snapshot().expect("snapshot").rect,
            Rect::new(300.0, 268.0, 420.0, 120.0)
        );
        // 长到 200：顶边 188，仍在工作区里，窗口不动
        c.resize(1, 200.0, ResizeSpring::Gentle);
        assert!(set_frames(&host.take()).is_empty());
        // 长到 400：顶边会到 −12 → 底边下移到 33 + 400
        c.resize(1, 400.0, ResizeSpring::Gentle);
        let frame = set_frames(&host.take()).last().copied().expect("pushed down");
        assert_eq!(frame, Rect::new(276.0, 433.0 + 24.0 - 531.0, 468.0, 531.0));
        assert_eq!(
            c.snapshot().expect("snapshot").rect,
            Rect::new(300.0, 33.0, 420.0, 400.0)
        );
    }

    /// 只在空白处按了一下（翻译视图获得焦点的常用方式）、窗口没动——不算拖过：之后的高度变化不读窗口（读要同步等
    /// 主线程）、不带边界，剪贴板隐藏时不记位置；真的拖过但没有推回时，高度变化同样不读窗口
    #[test]
    fn click_without_moving_is_not_a_drag() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        host.take_timers();
        host.take();
        c.drag_start(1);
        // 有的平台按下时也报一次移动事件（位置没变）
        drag_to(&host, Rect::new(300.0, 230.0, 420.0, 120.0));
        host.run_timers(DRAG_SETTLE);
        let calls = host.take();
        assert!(calls.contains(&Call::PauseGuards(false)), "{calls:?}");
        assert!(set_frames(&calls).is_empty(), "{calls:?}");
        let reads = *host.frame_reads.lock();
        for h in [160.0, 220.0, 180.0] {
            c.resize(1, h, ResizeSpring::Gentle);
        }
        let calls = host.take();
        assert_eq!(*host.frame_reads.lock(), reads, "高度变化不读窗口");
        assert!(!calls.iter().any(|c| matches!(c, Call::HeightBound(_))), "{calls:?}");
        assert_eq!(
            c.snapshot().expect("snapshot").rect,
            Rect::new(300.0, 230.0, 420.0, 180.0)
        );
        c.hide_now(1, HideReason::Escape);
        host.take();
        // 剪贴板：按一下不记位置
        c.prepare(open(2, PanelView::Clipboard, anchor_a()));
        c.ready(2, 300.0).expect("shown");
        host.take_timers();
        c.drag_start(2);
        host.run_timers(DRAG_SETTLE);
        c.hide_now(2, HideReason::Escape);
        assert!(
            !host.take().iter().any(|c| matches!(c, Call::Remember(..))),
            "没拖动：不记位置"
        );
        // 真的拖过、没有推回：高度变化照样不读窗口
        c.prepare(open(3, PanelView::Translate, anchor_a()));
        c.ready(3, 120.0).expect("shown");
        host.take_timers();
        c.drag_start(3);
        drag_to(&host, Rect::new(500.0, 300.0, 420.0, 120.0));
        host.run_timers(DRAG_SETTLE);
        host.take();
        let reads = *host.frame_reads.lock();
        for h in [160.0, 220.0] {
            c.resize(3, h, ResizeSpring::Gentle);
        }
        assert_eq!(*host.frame_reads.lock(), reads);
        assert!(
            host.take().iter().any(|c| matches!(c, Call::HeightBound(_))),
            "拖过：带边界"
        );
    }

    /// 拖动期间失焦（X11 的窗口管理器拖动时抓键盘，GDK 报 FocusOut）不触发失焦关闭；松开时已重新成为 key 就不关，
    /// 仍不是 key 才照常关闭（按住不动期间也不关）
    #[test]
    fn blur_while_dragging_waits_for_the_release() {
        for frame_mode in [FrameMode::Native, FrameMode::Css] {
            let host = MockHost::existing();
            let margin = if frame_mode == FrameMode::Css { 24.0 } else { 0.0 };
            let c = loaded(&host, frame_mode, margin);
            c.prepare(open(1, PanelView::Input, anchor_a()));
            c.ready(1, 168.0).expect("shown");
            host.fire(PanelEvent::Focused(true));
            host.take_timers();
            host.take();
            // 第一次拖动：拖动中失焦、松开前又成为 key → 不关
            c.drag_start(1);
            *host.button_down.lock() = true;
            host.fire(PanelEvent::Focused(false));
            drag_to(
                &host,
                Rect::new(500.0, 300.0, 420.0 + 2.0 * margin, 168.0 + 2.0 * margin),
            );
            let calls = host.take();
            assert!(calls.contains(&Call::EmitFocusChanged(1, false)), "{calls:?}");
            assert!(
                !calls.iter().any(|c| matches!(c, Call::EmitHideRequest(..))),
                "{frame_mode:?} 拖动中不关：{calls:?}"
            );
            host.fire(PanelEvent::Focused(true));
            *host.button_down.lock() = false;
            host.run_timers(DRAG_SETTLE);
            assert!(
                !host.take().iter().any(|c| matches!(c, Call::EmitHideRequest(..))),
                "{frame_mode:?} 松开时已是 key：不关"
            );
            // 第二次拖动：失焦后一直没回来 → 按住不动时不关，松开后关闭
            c.drag_start(1);
            *host.button_down.lock() = true;
            host.fire(PanelEvent::Focused(false));
            drag_to(
                &host,
                Rect::new(520.0, 320.0, 420.0 + 2.0 * margin, 168.0 + 2.0 * margin),
            );
            host.run_timers(DRAG_SETTLE);
            assert!(
                !host.take().iter().any(|c| matches!(c, Call::EmitHideRequest(..))),
                "{frame_mode:?} 按住不动：不关"
            );
            *host.button_down.lock() = false;
            host.run_timers(DRAG_SETTLE);
            let calls = host.take();
            assert!(
                calls.contains(&Call::EmitHideRequest(1, HideReason::Blur)),
                "{frame_mode:?} 松开时仍不是 key：{calls:?}"
            );
            // 拖动之外的失焦照旧立即关闭
            c.hide_now(1, HideReason::Blur);
            c.prepare(open(2, PanelView::Input, anchor_a()));
            c.ready(2, 168.0).expect("shown");
            host.fire(PanelEvent::Focused(true));
            host.take();
            host.fire(PanelEvent::Focused(false));
            assert!(host.take().contains(&Call::EmitHideRequest(2, HideReason::Blur)));
        }
    }

    /// Native 帧模式（macOS 的逻辑坐标桌面）拖到另一块屏——几何换到那块屏、按它的工作区夹回；之后长高伸出工作区时
    /// 弹簧带着那块屏的竖直边界推回
    #[test]
    fn native_drag_to_another_screen_clamps_and_pushes_within_it() {
        let host = MockHost::existing();
        let b = Screen {
            id: "2".to_owned(),
            frame: Rect::new(1440.0, -100.0, 1920.0, 1080.0),
            work: Rect::new(0.0, 0.0, 1920.0, 1040.0),
            scale: 1.0,
            physical_origin: PhysPoint { x: 2880, y: -200 },
            primary: false,
        };
        *host.screens.lock() = vec![screen(), b];
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        host.take_timers();
        host.take();
        c.drag_start(1);
        // 拖到 B 屏右下角之外：中心 (3410, 960) 不在任何一块屏上，离 B 最近
        drag_to(&host, Rect::new(3200.0, 900.0, 420.0, 120.0));
        host.run_timers(DRAG_SETTLE);
        let calls = host.take();
        // 卡片左边夹到 1920 − 8 − 420 = 1492，底边夹到 1040 − 8 → 顶边 912（本屏）→ 桌面 (2932, 812)
        let frame = set_frames(&calls).last().copied().expect("clamped");
        assert_eq!(frame, Rect::new(2932.0, 812.0, 420.0, 120.0));
        let snap = c.snapshot().expect("snapshot");
        assert_eq!(snap.screen.id, "2");
        assert_eq!(snap.rect, Rect::new(1492.0, 912.0, 420.0, 120.0));
        // 长高到 300：伸出 B 屏工作区底 → 整体上移，边界是 B 屏的（桌面 −100 + 8 … −100 + 1032）
        c.resize(1, 300.0, ResizeSpring::Gentle);
        let calls = host.take();
        assert!(
            calls.contains(&Call::HeightBound(VerticalBound {
                top: -92.0,
                bottom: 932.0
            })),
            "{calls:?}"
        );
        let snap = c.snapshot().expect("snapshot");
        assert_eq!(snap.rect, Rect::new(1492.0, 732.0, 420.0, 300.0));
        assert_eq!(host.window_frame(), Some(Rect::new(2932.0, 632.0, 420.0, 300.0)));
    }

    /// Native 帧模式 Above 放置的面板拖到靠近工作区顶部后向上长高：顶边到顶就改为底边下移（弹簧带边界推）
    #[test]
    fn native_above_dragged_panel_grows_down_when_top_is_reached() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Translate, Rect::new(300.0, 700.0, 100.0, 18.0)));
        c.ready(1, 120.0).expect("shown");
        host.take_timers();
        host.take();
        let start = host.window_frame().expect("frame");
        assert_eq!(start.bottom(), 688.0, "Above：卡片底边在锚点上方 12");
        c.drag_start(1);
        drag_to(&host, Rect::new(300.0, 60.0, 420.0, 120.0));
        host.run_timers(DRAG_SETTLE);
        host.take();
        assert_eq!(
            c.snapshot().expect("snapshot").rect,
            Rect::new(300.0, 60.0, 420.0, 120.0)
        );
        // 长到 140：顶边 40，仍在工作区里，底边不动
        c.resize(1, 140.0, ResizeSpring::Gentle);
        assert_eq!(host.window_frame(), Some(Rect::new(300.0, 40.0, 420.0, 140.0)));
        // 长到 300：顶边会到 −120 → 顶边贴住 33，底边下移到 333
        c.resize(1, 300.0, ResizeSpring::Gentle);
        let calls = host.take();
        assert!(calls.contains(&Call::AnimateHeight(300.0, true)), "{calls:?}");
        let snap = c.snapshot().expect("snapshot");
        assert_eq!(snap.rect, Rect::new(300.0, 33.0, 420.0, 300.0));
        assert_eq!(host.window_frame(), Some(snap.rect));
    }

    /// Css 帧模式拖过的面板里视图内切换改宽度（`panel_set_width`）之后继续长高——左边、固定边留在拖到的位置，
    /// 伸出工作区时整窗上移；拖动中的改宽度推迟到松开（Native 帧模式同样推迟，松开时窗口直接放到会话几何）
    #[test]
    fn width_switch_after_and_during_drag() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Css, 24.0);
        c.prepare(open(1, PanelView::Input, anchor_a()));
        c.ready(1, 168.0).expect("shown");
        host.take_timers();
        host.take();
        c.drag_start(1);
        drag_to(&host, Rect::new(476.0, 600.0, 468.0, 216.0));
        host.run_timers(DRAG_SETTLE);
        host.take();
        c.set_width(1, 520.0);
        let frame = set_frames(&host.take()).last().copied().expect("width");
        assert_eq!((frame.x, frame.y), (476.0, 600.0));
        assert!(frame.width >= 520.0 + 48.0, "{frame:?}");
        // 长高到 300：底边会到 924 > 822 → 整窗上移，卡片底边贴住工作区底
        c.resize(1, 300.0, ResizeSpring::Gentle);
        let frame = set_frames(&host.take()).last().copied().expect("pushed");
        assert_eq!((frame.x, frame.y), (476.0, 522.0 - 24.0));
        assert_eq!(
            c.snapshot().expect("snapshot").rect,
            Rect::new(500.0, 522.0, 520.0, 300.0)
        );
        // 拖动中改宽度（例如拖动时按 Ctrl+Enter 切到翻译）：不改窗口，松开后一次放到位
        c.drag_start(1);
        *host.button_down.lock() = true;
        let at = host.window_frame().expect("frame");
        drag_to(
            &host,
            Rect {
                x: 300.0,
                y: 300.0,
                ..at
            },
        );
        c.set_width(1, 420.0);
        assert!(set_frames(&host.take()).is_empty());
        *host.button_down.lock() = false;
        host.run_timers(DRAG_SETTLE);
        let frame = set_frames(&host.take()).last().copied().expect("deferred width");
        assert_eq!((frame.x, frame.y), (300.0, 300.0));
        assert_eq!(c.snapshot().expect("snapshot").rect.width, 420.0);

        // Native：拖动中的宽度切换推迟，松开时窗口放到会话几何（左边是拖到的位置）
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Input, anchor_a()));
        c.ready(1, 168.0).expect("shown");
        host.take_timers();
        host.take();
        c.drag_start(1);
        drag_to(&host, Rect::new(500.0, 300.0, 420.0, 168.0));
        c.set_width(1, 520.0);
        let calls = host.take();
        assert!(!calls.iter().any(|c| matches!(c, Call::SwitchWidth(_))), "{calls:?}");
        host.run_timers(DRAG_SETTLE);
        let frame = set_frames(&host.take()).last().copied().expect("deferred width");
        assert_eq!(frame, Rect::new(500.0, 300.0, 520.0, 168.0));
    }

    /// 拖过的剪贴板面板交叉淡入到翻译——新会话继承「拖过」，高度上限按整个工作区算（不是固定边以下剩下的那点空间），
    /// 新卡片伸出工作区时整体推回（Native：带边界的高度弹簧）
    #[test]
    fn crossfade_from_a_dragged_panel_keeps_room_to_grow() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Clipboard, anchor_a()));
        c.ready(1, 300.0).expect("shown");
        host.take_timers();
        host.take();
        c.drag_start(1);
        drag_to(&host, Rect::new(500.0, 560.0, 420.0, 300.0));
        host.run_timers(DRAG_SETTLE);
        host.take();
        // 底边 860 > 822：松开时夹回 → 顶边 522
        assert_eq!(
            c.snapshot().expect("snapshot").rect,
            Rect::new(500.0, 522.0, 420.0, 300.0)
        );
        // 固定边以下只剩 300：不继承时上限会是 300，新卡片 420 只能滚动
        c.prepare(open_with(2, anchor_b(), 520.0, PanelTransition::Crossfade));
        c.ready(2, 420.0).expect("shown in place");
        let calls = host.take();
        assert!(
            calls.contains(&Call::HeightBound(VerticalBound {
                top: 33.0,
                bottom: 822.0
            })),
            "{calls:?}"
        );
        let snap = c.snapshot().expect("snapshot");
        assert_eq!(snap.rect, Rect::new(500.0, 402.0, 520.0, 420.0));
        assert_eq!(host.window_frame(), Some(snap.rect));
        // 之后继续长高：照样推回
        c.resize(2, 460.0, ResizeSpring::Gentle);
        let snap = c.snapshot().expect("snapshot");
        assert_eq!(snap.rect, Rect::new(500.0, 362.0, 520.0, 460.0));
        assert_eq!(host.window_frame(), Some(snap.rect));
    }

    #[test]
    fn css_deferred_grow_is_never_lost() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Css, 24.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        host.take_timers();
        host.take();
        // 第一次增长：立即放大一级
        c.resize(1, 130.0, ResizeSpring::Gentle);
        assert_eq!(set_frames(&host.take()).last().map(|r| r.height), Some(168.0 + 48.0));
        // 50 ms 内的第二次增长（流式输出的最后一段落在上一次放大之后）：推迟，窗口暂时不动
        c.resize(1, 200.0, ResizeSpring::Gentle);
        let calls = host.take();
        assert!(set_frames(&calls).is_empty(), "{calls:?}");
        let wait = calls
            .iter()
            .find_map(|c| match c {
                Call::Schedule(d) if *d <= sizing::CSS_GROW_MIN_INTERVAL => Some(*d),
                _ => None,
            })
            .expect("deferred grow timer");
        // 计时器提前跑（还不到 50 ms）：重新推迟，不丢
        host.run_timers(wait);
        let calls = host.take();
        assert!(set_frames(&calls).is_empty(), "{calls:?}");
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, Call::Schedule(d) if *d <= sizing::CSS_GROW_MIN_INTERVAL)),
            "{calls:?}"
        );
        // 到期：之后没有新的上报，也必须放大到能装下卡片
        std::thread::sleep(sizing::CSS_GROW_MIN_INTERVAL);
        let (due, rest): (Vec<_>, Vec<_>) = host
            .take_timers()
            .into_iter()
            .partition(|(d, _)| *d <= sizing::CSS_GROW_MIN_INTERVAL);
        host.timers.lock().extend(rest);
        for (_, f) in due {
            f();
        }
        let frames = set_frames(&host.take());
        let frame = frames.last().expect("deferred grow applied");
        assert!(frame.height >= 200.0 + 2.0 * 24.0, "{frame:?}");
        assert_eq!(frame.y, 230.0 - 24.0, "Below：顶边不动");
    }

    #[test]
    fn css_slack_is_reclaimed_after_card_settles() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Css, 24.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        host.take_timers();
        c.resize(1, 130.0, ResizeSpring::Gentle);
        let calls = host.take();
        assert_eq!(set_frames(&calls).last().map(|r| r.height), Some(216.0));
        assert!(calls.contains(&Call::Schedule(CSS_SLACK_RECLAIM)), "阶梯留下了富余区");
        // 计时器只有一个：卡片再变化只往后推到期时刻
        c.resize(1, 140.0, ResizeSpring::Gentle);
        assert!(!host.take().contains(&Call::Schedule(CSS_SLACK_RECLAIM)));
        // 还没稳定够 300 ms：改期，不动窗口
        host.run_timers(CSS_SLACK_RECLAIM);
        let calls = host.take();
        assert!(set_frames(&calls).is_empty(), "{calls:?}");
        std::thread::sleep(CSS_SLACK_RECLAIM);
        for (_, f) in host.take_timers() {
            f();
        }
        let calls = host.take();
        let frame = *set_frames(&calls).last().expect("slack reclaimed");
        assert_eq!(
            frame,
            Rect::new(276.0, 206.0, 468.0, 140.0 + 48.0),
            "顶边不动，一次收回到卡片 + 2·m"
        );
        let region = last_region(&calls).expect("click-through updated");
        assert_eq!(region.window, frame);
        assert_eq!(c.snapshot().expect("shown").rect, Rect::new(300.0, 230.0, 420.0, 140.0));
        // Above：窗口高度是会话上限，不回收（回收要移动顶边）
        c.hide_now(1, HideReason::Escape);
        c.prepare(open(2, PanelView::Translate, Rect::new(500.0, 760.0, 100.0, 18.0)));
        c.ready(2, 120.0).expect("shown");
        host.take_timers();
        host.take();
        c.resize(2, 200.0, ResizeSpring::Gentle);
        assert!(!host.take().contains(&Call::Schedule(CSS_SLACK_RECLAIM)));
    }

    #[test]
    fn crossfade_keeps_position_native() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Clipboard, anchor_a()));
        c.ready(1, 240.0).expect("shown");
        host.take();
        // 剪贴板条目 → 翻译：锚点不同，但要求交叉淡入，位置保持；宽度相同时只走高度弹簧
        c.prepare(open_with(2, anchor_b(), 420.0, PanelTransition::Crossfade));
        let calls = host.take();
        assert!(
            !calls.iter().any(|c| matches!(c, Call::Hide(_) | Call::SetFrame(_))),
            "{calls:?}"
        );
        c.ready(2, 160.0).expect("shown in place");
        let calls = host.take();
        assert!(calls.contains(&Call::AnimateHeight(160.0, true)), "{calls:?}");
        assert!(
            !calls.iter().any(|c| matches!(c, Call::SwitchWidth(_))),
            "宽度不变：不切换"
        );
        let snap = c.snapshot().expect("snapshot");
        assert_eq!(snap.rect, Rect::new(300.0, 230.0, 420.0, 160.0));
        assert_eq!(host.window_frame(), Some(snap.rect), "窗口即卡片，与会话几何一致");
        // 宽度变化：左边与顶边不变（宽度在旧视图透明时切换，见 native_crossfade_width_waits_for_transparent_frame）
        c.prepare(open_with(
            3,
            Rect::new(900.0, 600.0, 10.0, 16.0),
            520.0,
            PanelTransition::Crossfade,
        ));
        c.ready(3, 180.0).expect("shown in place");
        let snap = c.snapshot().expect("snapshot");
        assert_eq!(snap.rect, Rect::new(300.0, 230.0, 520.0, 180.0));
        assert_eq!(host.window_frame(), Some(snap.rect), "切换生效后窗口与会话几何一致");
    }

    /// Native 帧模式宽度要变的交叉淡入里，ready 时旧视图还完全不透明——窗口宽度不能在这时改，要等它淡出完
    #[test]
    fn native_crossfade_width_waits_for_transparent_frame() {
        let switch_of = |calls: &[Call]| {
            calls.iter().find_map(|c| match c {
                Call::SwitchWidth(w) => Some(*w),
                _ => None,
            })
        };
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Input, anchor_a()));
        c.ready(1, 168.0).expect("shown");
        host.take();
        // 输入（420）→ 翻译（360）
        c.prepare(open_with(2, anchor_a(), 360.0, PanelTransition::Crossfade));
        c.ready(2, 150.0).expect("shown in place");
        let calls = host.take();
        assert!(
            set_frames(&calls).is_empty(),
            "ready 里不出现改宽度的 setFrame：{calls:?}"
        );
        assert!(
            !calls.iter().any(|c| matches!(c, Call::PinWebview(_))),
            "可见状态下不改 WebView 尺寸"
        );
        let switch = switch_of(&calls).expect("width switch scheduled");
        assert_eq!((switch.x, switch.width), (300.0, 360.0));
        assert_eq!(
            switch.delay,
            Duration::from_secs_f64(sizing::timing::CROSSFADE),
            "旧视图淡出完、内容完全透明的那一刻"
        );
        assert_eq!(switch.pin, None, "上限没变：不重新固定 WebView");
        assert!(
            calls.contains(&Call::AnimateHeight(150.0, true)),
            "高度照常走 spring.gentle：{calls:?}"
        );

        // 减少动效：交叉淡入 ≤ 100 ms，高度直接到位
        c.set_options(PanelOptions {
            reduce_motion: true,
            ..PanelOptions::default()
        });
        c.prepare(open_with(3, anchor_a(), 520.0, PanelTransition::Crossfade));
        c.ready(3, 150.0).expect("shown in place");
        let calls = host.take();
        assert!(set_frames(&calls).is_empty(), "{calls:?}");
        let switch = switch_of(&calls).expect("width switch");
        assert_eq!(switch.delay, Duration::from_secs_f64(sizing::timing::REDUCED_FADE));
        assert!(calls.contains(&Call::AnimateHeight(150.0, false)));
        c.set_options(PanelOptions::default());

        // 上限也变了：WebView 的重新固定与宽度切换在同一帧（内容透明时）完成
        let mut lower = open_with(4, anchor_a(), 360.0, PanelTransition::Crossfade);
        lower.geometry = place_panel(&screen(), PlaceAnchor::Rect(anchor_a()), 360.0, 120.0, 400.0);
        c.prepare(lower);
        c.ready(4, 150.0).expect("shown in place");
        let calls = host.take();
        assert_eq!(switch_of(&calls).and_then(|w| w.pin), Some(400.0), "{calls:?}");
        assert!(!calls.iter().any(|c| matches!(c, Call::PinWebview(_))));

        // 宽度不变而上限变大：WebView 必须在窗口长过旧的固定高度之前长到新上限
        c.prepare(open_with(5, anchor_a(), 360.0, PanelTransition::Crossfade));
        c.ready(5, 150.0).expect("shown in place");
        let calls = host.take();
        assert!(switch_of(&calls).is_none(), "{calls:?}");
        assert!(calls.contains(&Call::PinWebview(483.0)), "{calls:?}");
    }

    /// 视图内切换（输入 → 翻译）：前端在 ViewSwitch 交换时调用 `panel_set_width`，窗口立即改宽、位置不变。
    /// 宽度没变、会话不对时什么都不做
    #[test]
    fn in_view_width_switch_follows_the_frontend_swap() {
        let switches = |calls: &[Call]| {
            calls
                .iter()
                .filter_map(|c| match c {
                    Call::SwitchWidth(w) => Some(*w),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        // Native：displayLink 下一帧一次改宽（delay 0），不重新固定 WebView，不另发 setFrame
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Input, anchor_a()));
        c.ready(1, 168.0).expect("shown");
        host.take();
        c.set_width(1, 360.0);
        let calls = host.take();
        assert!(set_frames(&calls).is_empty(), "{calls:?}");
        let sw = switches(&calls);
        assert_eq!(sw.len(), 1, "{calls:?}");
        assert_eq!(
            (sw[0].x, sw[0].width, sw[0].delay, sw[0].pin),
            (300.0, 360.0, Duration::ZERO, None)
        );
        assert_eq!(c.snapshot().expect("snapshot").rect.width, 360.0, "会话几何随之更新");
        c.set_width(1, 360.0);
        c.set_width(9, 520.0);
        assert!(host.take().is_empty(), "宽度没变 / 不是当前会话：空操作");

        // Css（带透明边距）：窗口立即取新旧两张卡片的并集，卡片稳定后回收
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Css, 24.0);
        c.prepare(open(1, PanelView::Input, anchor_a()));
        c.ready(1, 168.0).expect("shown");
        host.take_timers();
        host.take();
        let old = c.snapshot().expect("snapshot").rect;
        c.set_width(1, 520.0);
        let calls = host.take();
        let frame = *set_frames(&calls).last().expect("window widened");
        assert_eq!((frame.x, frame.width), (old.x - 24.0, 520.0 + 48.0));
        c.set_width(1, 360.0);
        let calls = host.take();
        let frame = *set_frames(&calls).last().expect("window kept as the union");
        assert_eq!(
            frame.width,
            520.0 + 48.0,
            "变窄时先保留并集，交叉淡入期间旧卡片不被裁切"
        );
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, Call::Schedule(d) if *d == CSS_SLACK_RECLAIM)),
            "{calls:?}"
        );
        assert_eq!(c.snapshot().expect("snapshot").rect.width, 360.0);
    }

    #[test]
    fn crossfade_keeps_position_css_and_covers_both_cards() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Css, 24.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        host.take_timers();
        host.take();
        // 新视图更窄、锚点在别处：位置不变，窗口暂取新旧两张卡片的并集（交叉淡入期间旧卡片不被裁切）
        c.prepare(open_with(2, anchor_b(), 360.0, PanelTransition::Crossfade));
        c.ready(2, 150.0).expect("shown in place");
        let calls = host.take();
        let frame = *set_frames(&calls).last().expect("window resized at ready");
        assert_eq!(frame, Rect::new(276.0, 206.0, 420.0 + 48.0, 150.0 + 48.0));
        let snap = c.snapshot().expect("snapshot");
        assert_eq!(snap.rect, Rect::new(300.0, 230.0, 360.0, 150.0));
        let region = last_region(&calls).expect("click-through");
        assert_eq!((region.window, region.card), (frame, snap.rect));
        assert!(region.passes_through(690.0, 260.0), "并集里旧卡片多出来的部分放行点击");
        // 前端卡片用 spring.gentle 从旧高度（120）过渡到新高度（150）：停下之后才收回到新卡片
        let settle = sizing::settle_duration(SpringParams::GENTLE, 120.0, 150.0).max(CSS_SLACK_RECLAIM);
        assert!(calls.contains(&Call::Schedule(settle)), "{calls:?}");
        std::thread::sleep(settle);
        for (_, f) in host.take_timers() {
            f();
        }
        let frame = *set_frames(&host.take()).last().expect("reclaimed");
        assert_eq!(frame, Rect::new(276.0, 206.0, 360.0 + 48.0, 150.0 + 48.0));
    }

    #[test]
    fn fresh_page_resend_waits_for_ready_timeout() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        host.take();
        // 超时之前（前端可能已经处理了第一次 prepare、ready 还在路上）：不补发，下一帧再看
        host.run_timers(READY_TIMEOUT + FRESH_PAGE_RESEND_EVERY);
        assert!(!host.take().contains(&Call::EmitPrepare(1)));
        assert_eq!(host.count_timers(FRESH_PAGE_RESEND_EVERY), 1);
        host.run_timers(READY_TIMEOUT);
        host.take();
        host.run_timers(FRESH_PAGE_RESEND_EVERY);
        assert!(host.take().contains(&Call::EmitPrepare(1)), "超时后才补发");
        // 已经显示：补发作废
        c.ready(1, 120.0).expect("shown");
        host.take();
        host.run_timers(FRESH_PAGE_RESEND_EVERY);
        assert!(!host.take().contains(&Call::EmitPrepare(1)));
    }

    /// 卸载过的控制器上来一次冷唤出：窗口重建好（页面还没加载）
    fn cold_with_window(host: &Arc<MockHost>, frame_mode: FrameMode, view: PanelView) -> Arc<PanelController> {
        let c = loaded(host, frame_mode, 0.0);
        c.set_options(PanelOptions {
            unload_after: Some(Duration::from_secs(600)),
            ..PanelOptions::default()
        });
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        c.hide_now(1, HideReason::Escape);
        host.run_timers(Duration::from_secs(600));
        assert!(!*host.exists.lock(), "已卸载");
        host.take();
        c.prepare(open(2, view, anchor_a()));
        wait_recreated(host);
        // 外壳在建好窗口的同一个阻塞任务里显示：等它出现
        let deadline = Instant::now() + Duration::from_secs(5);
        while frame_mode == FrameMode::Native && !host.calls.lock().iter().any(|c| matches!(c, Call::ShowShell(..))) {
            assert!(Instant::now() < deadline, "shell was not shown");
            std::thread::sleep(Duration::from_millis(2));
        }
        c
    }

    /// 卸载之后的冷唤出不等页面加载——窗口一建好就以进入淡入显示原生外壳（内容透明）；页面加载后照常 emit，
    /// ready 时内容淡入、高度从估计值弹到实测值；perf 的 WindowShown 记在外壳上屏的时刻，页面记为 cold
    #[test]
    fn cold_show_puts_up_the_native_shell_before_the_page_loads() {
        let host = MockHost::existing();
        let c = cold_with_window(&host, FrameMode::Native, PanelView::Input);
        let calls = host.take();
        let shell = calls
            .iter()
            .find_map(|c| match c {
                Call::ShowShell(r, fade, key) => Some((*r, *fade, *key)),
                _ => None,
            })
            .expect("shell");
        assert_eq!(shell.0.height, 120.0, "估计高度");
        assert!(shell.1, "正常的进入淡入");
        assert!(shell.2, "输入视图的外壳一上屏就成为 key（按键不落到之前的前台应用）");
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, Call::SetFrame(_) | Call::PinWebview(_))),
            "定位与固定 WebView 和显示在同一次主线程往返里：{calls:?}"
        );
        assert!(calls.contains(&Call::Perf(2, PerfStage::WindowShown)));
        assert!(!calls.contains(&Call::EmitPrepare(2)), "页面加载前不 emit");
        let snap = c.snapshot().expect("session");
        assert!(snap.visible && !snap.shown);
        host.fire(PanelEvent::PageLoaded);
        assert!(host.take().contains(&Call::EmitPrepare(2)));
        // 40 ms 超时：外壳已在屏幕上，不再做透明显示
        host.run_timers(READY_TIMEOUT);
        assert!(!host.take().iter().any(|c| matches!(c, Call::Show(..))));
        let shown = c.ready(2, 200.0).expect("shown");
        assert!(shown.focused, "输入视图");
        let calls = host.take();
        let reveal = calls
            .iter()
            .find_map(|c| match c {
                Call::RevealShell(r, key) => Some((*r, *key)),
                _ => None,
            })
            .expect("reveal shell");
        assert_eq!(reveal.0.height, 200.0, "实测高度");
        assert!(reveal.1, "输入视图成为 key");
        assert!(!calls.iter().any(|c| matches!(c, Call::Show(..) | Call::Reveal(_))));
        assert!(calls.contains(&Call::Page(2, PanelPage::Cold)));
        assert!(calls.iter().any(|c| matches!(c, Call::ArmGuards(2, ..))));
    }

    /// 冷唤出重建窗口时，暂存的 prepare 随 Boot 送进新页面；预热创建（没有暂存）时不带
    #[test]
    fn a_cold_rebuild_hands_the_pending_prepare_to_the_new_page() {
        let host = MockHost::existing();
        let _c = cold_with_window(&host, FrameMode::Native, PanelView::Input);
        let boots = host.boot_prepares.lock().clone();
        assert_eq!(boots, vec![None, Some(2)], "预热创建不带、冷唤出的重建带上会话 2");
    }

    /// Boot.prepare（Native）：页面凭 Boot.prepare 在页面加载完成之前 ready——外壳上的会话直接内容淡入（RevealShell）；页面随后
    /// 加载时不再 emit（会话已显示），这一页记为用过（隐藏后照常排回收）
    #[test]
    fn a_boot_prepare_ready_on_the_shell_before_the_page_loads_reveals_it() {
        let host = MockHost::existing();
        let c = cold_with_window(&host, FrameMode::Native, PanelView::Input);
        host.take();
        let shown = c.ready(2, 210.0).expect("shown before the page loaded");
        assert!(shown.focused);
        let calls = host.take();
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, Call::RevealShell(r, true) if r.height == 210.0)),
            "{calls:?}"
        );
        assert!(calls.contains(&Call::Page(2, PanelPage::Cold)));
        c.set_options(PanelOptions {
            recycle_after: Some(Duration::from_secs(45)),
            ..c.options()
        });
        host.fire(PanelEvent::PageLoaded);
        let calls = host.take();
        assert!(
            !calls.contains(&Call::EmitPrepare(2)),
            "已显示的会话不再 emit：{calls:?}"
        );
        c.hide_now(2, HideReason::Escape);
        assert_eq!(
            host.live_cancellable(Duration::from_secs(45)),
            1,
            "凭 Boot.prepare 用过的页面同样会被回收"
        );
    }

    /// Boot.prepare（Css 帧模式，没有外壳）：页面凭 Boot.prepare 先于页面加载 ready，会话还在暂存里——按 prepare 时的做法把它
    /// 变成当前会话（恢复可见性、定位都在显示之前）再同步显示；页面加载后不再 emit；暂存已被新的 prepare 取代时不认旧会话
    #[test]
    fn a_boot_prepare_ready_before_the_page_loads_promotes_the_pending_prepare() {
        let host = MockHost::existing();
        let c = cold_with_window(&host, FrameMode::Css, PanelView::Translate);
        host.take();
        let shown = c.ready(2, 150.0).expect("shown");
        assert!(!shown.focused);
        let calls = host.take();
        let pos = |want: &dyn Fn(&Call) -> bool| calls.iter().position(want);
        let restore = pos(&|c| *c == Call::RestoreVisibility).expect("restore visibility");
        let frame = pos(&|c| matches!(c, Call::SetFrame(_))).expect("set frame");
        let show = pos(&|c| matches!(c, Call::Show(..))).expect("show");
        assert!(restore < show && frame < show, "{calls:?}");
        assert!(calls.contains(&Call::Page(2, PanelPage::Cold)));
        host.fire(PanelEvent::PageLoaded);
        assert!(!host.take().contains(&Call::EmitPrepare(2)));
        assert!(c.snapshot().is_some_and(|s| s.session_id == 2 && s.shown));

        // 暂存被新的 prepare 取代：Boot 里的旧会话 ready 回 None，新会话照常在页面加载后 emit
        let host = MockHost::existing();
        let c = cold_with_window(&host, FrameMode::Css, PanelView::Translate);
        c.prepare(open(3, PanelView::Translate, anchor_a()));
        host.take();
        assert!(c.ready(2, 150.0).is_none(), "过期的 Boot.prepare");
        host.fire(PanelEvent::PageLoaded);
        assert!(host.take().contains(&Call::EmitPrepare(3)));
        assert!(c.ready(3, 150.0).is_some());
    }

    /// Native：冷唤出的外壳在屏幕上时会话被结束（外壳上按 Esc：`shell_escape` → `hide_now_and_notify`），
    /// 那时的 hide_request 页面还没有监听。页面加载后 Rust 没有会话可 emit；页面 JS 随后凭 Boot.prepare 渲染并 ready——
    /// 回 None（前端据此复位为隐藏），不显示任何东西；这一页渲染过视图，记为用过，空闲计时器改按回收重排（原先只排着卸载）
    #[test]
    fn a_boot_prepare_ready_for_a_session_ended_on_the_shell_counts_the_page_as_used() {
        let host = MockHost::existing();
        let c = cold_with_window(&host, FrameMode::Native, PanelView::Input);
        c.set_options(PanelOptions {
            recycle_after: Some(Duration::from_secs(45)),
            ..c.options()
        });
        // 卸载那一轮已经执行过的计时器（没有取消）也在计数里
        let fired = host.live_cancellable(Duration::from_secs(600));
        c.hide_now_and_notify(2, HideReason::Escape);
        assert!(c.snapshot().is_none(), "会话已结束");
        host.fire(PanelEvent::PageLoaded);
        let calls = host.take();
        assert!(!calls.contains(&Call::EmitPrepare(2)), "{calls:?}");
        assert_eq!(
            host.live_cancellable(Duration::from_secs(45)),
            0,
            "页面还没用过：不排回收"
        );
        assert_eq!(host.live_cancellable(Duration::from_secs(600)), fired + 1, "只排着卸载");
        assert!(c.ready(2, 200.0).is_none(), "已结束的会话");
        let calls = host.take();
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, Call::Show(..) | Call::Reveal(_) | Call::RevealShell(..))),
            "{calls:?}"
        );
        assert_eq!(
            host.live_cancellable(Duration::from_secs(45)),
            1,
            "渲染过视图的页面照常回收"
        );
        assert_eq!(
            host.live_cancellable(Duration::from_secs(600)),
            fired,
            "旧的计时器链作废"
        );
        // 重复的 ready（补发引起）不再重排
        assert!(c.ready(2, 200.0).is_none());
        assert_eq!(host.live_cancellable(Duration::from_secs(45)), 1);
        host.run_timers(Duration::from_secs(45));
        assert!(host.take().contains(&Call::Destroy), "回收");
    }

    /// Css：截图前隐藏在页面 JS 运行之前取消了暂存的 prepare。页面凭 Boot.prepare 在页面加载
    /// 完成之前 ready——回 None；页面加载时 Rust 没有暂存可 emit，按「用过的页面」排回收（那时窗口还在创建中，隐藏 / 取消
    /// 没能排上空闲计时器）
    #[test]
    fn a_boot_prepare_ready_after_capture_cancelled_the_pending_prepare_counts_the_page_as_used() {
        let host = MockHost::existing();
        let c = cold_with_window(&host, FrameMode::Css, PanelView::Translate);
        c.set_options(PanelOptions {
            recycle_after: Some(Duration::from_secs(45)),
            ..c.options()
        });
        assert!(!c.hide_for_capture(Duration::from_millis(10)), "窗口不在屏幕上");
        host.take();
        assert!(c.ready(2, 150.0).is_none(), "暂存已被截图取消");
        assert!(c.snapshot().is_none());
        assert_eq!(
            host.live_cancellable(Duration::from_secs(45)),
            0,
            "页面还在加载：交给页面加载"
        );
        host.fire(PanelEvent::PageLoaded);
        let calls = host.take();
        assert!(
            !calls.iter().any(|c| matches!(c, Call::EmitPrepare(_) | Call::Show(..))),
            "{calls:?}"
        );
        assert_eq!(host.live_cancellable(Duration::from_secs(45)), 1);
        // 之后的唤出照常（回收计时器作废）
        c.prepare(open(3, PanelView::Translate, anchor_a()));
        assert!(host.take().contains(&Call::EmitPrepare(3)));
        assert_eq!(host.live_cancellable(Duration::from_secs(45)), 0);
        assert!(c.ready(3, 150.0).is_some());
    }

    /// `panel_resize` 指定 snappy 时 Native 帧模式的窗口弹簧用它（Css 帧模式的阶梯不受影响）
    #[test]
    fn resize_passes_the_requested_spring_to_the_native_animation() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        host.take();
        c.resize(1, 260.0, ResizeSpring::Snappy);
        c.resize(1, 300.0, ResizeSpring::Gentle);
        let calls = host.take();
        assert!(calls.contains(&Call::AnimateSnappy(260.0, true)), "{calls:?}");
        assert!(calls.contains(&Call::AnimateHeight(300.0, true)), "{calls:?}");
    }

    /// 外壳期间被隐藏（截图前隐藏、Esc）：照「ready 超时后已显示」的会话处理——立即离屏、会话结束；页面随后加载时
    /// 不再 emit
    #[test]
    fn hiding_the_shell_before_the_page_loads_ends_the_session() {
        let host = MockHost::existing();
        let c = cold_with_window(&host, FrameMode::Native, PanelView::Translate);
        host.take();
        assert!(c.hide_for_capture(Duration::from_millis(100)));
        let calls = host.take();
        assert!(calls.contains(&Call::Hide(false)), "{calls:?}");
        assert!(c.snapshot().is_none());
        host.fire(PanelEvent::PageLoaded);
        assert!(!host.take().contains(&Call::EmitPrepare(2)));
    }

    /// 外壳期间又来了新的 prepare：新会话接着用外壳（不透明显示一次再淡入），旧会话结束
    #[test]
    fn a_new_prepare_during_the_shell_reuses_it() {
        let host = MockHost::existing();
        let c = cold_with_window(&host, FrameMode::Native, PanelView::Translate);
        host.take();
        c.prepare(open(3, PanelView::Translate, anchor_a()));
        host.fire(PanelEvent::PageLoaded);
        let calls = host.take();
        assert!(calls.contains(&Call::EmitPrepare(3)));
        assert!(!calls.contains(&Call::EmitPrepare(2)));
        assert_eq!(*c.hidden().borrow(), 2, "被取代的外壳会话结束");
        host.run_timers(READY_TIMEOUT);
        c.ready(3, 160.0).expect("shown");
        let calls = host.take();
        assert!(calls.iter().any(|c| matches!(c, Call::RevealShell(..))), "{calls:?}");
        assert!(!calls.iter().any(|c| matches!(c, Call::Show(..))));
    }

    /// 外壳上屏之后页面迟迟不加载：按 WebView 失效重建，不让空卡片留在屏幕上
    #[test]
    fn a_shell_whose_page_never_loads_is_rebuilt() {
        let host = MockHost::existing();
        let c = cold_with_window(&host, FrameMode::Native, PanelView::Translate);
        host.take();
        let (tx, rx) = std::sync::mpsc::channel();
        *host.create_gate.lock() = Some(rx);
        host.run_timers(SHELL_LOAD_WAIT);
        let calls = host.take();
        assert!(calls.contains(&Call::Destroy), "{calls:?}");
        assert!(c.snapshot().is_none(), "外壳会话交给重建（暂存）");
        drop(tx);
    }

    fn shell_call(calls: &[Call]) -> Option<(Rect, bool, bool)> {
        calls.iter().find_map(|c| match c {
            Call::ShowShell(r, fade, key) => Some((*r, *fade, *key)),
            _ => None,
        })
    }

    /// 不要焦点的翻译视图外壳不抢 key（按键照常留在原应用，与热唤出一致）
    #[test]
    fn translate_shell_does_not_take_the_keyboard() {
        let host = MockHost::existing();
        let _c = cold_with_window(&host, FrameMode::Native, PanelView::Translate);
        let (_, _, key) = shell_call(&host.take()).expect("shell");
        assert!(!key);
    }

    /// 外壳期间按 Esc（原生层截住、下一轮事件循环回调）——外壳立即淡出收起、会话结束、补发 hide_request；
    /// 页面随后加载时不再 emit
    #[test]
    fn escape_on_the_shell_hides_it() {
        let host = MockHost::existing();
        let c = cold_with_window(&host, FrameMode::Native, PanelView::Input);
        host.take();
        let escape = host.shell_escape.lock().clone().expect("escape callback");
        escape();
        let calls = host.take();
        assert!(calls.contains(&Call::Hide(true)), "淡出收起：{calls:?}");
        assert!(calls.contains(&Call::EmitHideRequest(2, HideReason::Escape)));
        assert_eq!(*c.hidden().borrow(), 2);
        host.fire(PanelEvent::PageLoaded);
        assert!(!host.take().contains(&Call::EmitPrepare(2)));
        // 迟到的第二次回调（会话已经没了）什么都不做
        escape();
        assert!(host.take().is_empty());
    }

    /// 要焦点的外壳一上屏就是 key（`key_ms` 记在这一刻，早于 ready）；外壳期间用户点了别的应用（失焦）——
    /// 输入视图直接收起（前端还没加载，走不了它的退出流程）
    #[test]
    fn a_focused_shell_marks_key_and_hides_on_blur() {
        let host = MockHost::existing();
        let c = cold_with_window(&host, FrameMode::Native, PanelView::Input);
        host.take();
        host.fire(PanelEvent::Focused(true));
        assert!(host.take().contains(&Call::Perf(2, PerfStage::KeyGained)));
        host.fire(PanelEvent::Focused(false));
        let calls = host.take();
        assert!(calls.contains(&Call::Hide(true)), "{calls:?}");
        assert!(calls.contains(&Call::EmitHideRequest(2, HideReason::Blur)));
        assert!(c.snapshot().is_none());
    }

    /// 外壳期间来了几何不同的 prepare（换了锚点）——外壳用弹簧移到新矩形（不硬切），ready 时 reveal 到同一个
    /// 矩形；新视图要焦点而外壳还不是 key 时立即成为 key
    #[test]
    fn a_new_prepare_elsewhere_moves_the_shell() {
        let host = MockHost::existing();
        let c = cold_with_window(&host, FrameMode::Native, PanelView::Translate);
        host.take();
        let mut next = open(3, PanelView::Input, anchor_b());
        next.geometry = place_panel(&screen(), PlaceAnchor::Rect(anchor_b()), 360.0, 120.0, 400.0);
        c.prepare(next);
        let calls = host.take();
        let moved = calls
            .iter()
            .find_map(|c| match c {
                Call::MoveShell(r, pin) => Some((*r, *pin)),
                _ => None,
            })
            .expect("shell moved");
        assert_eq!(moved.0.width, 360.0);
        assert!(moved.1.is_some(), "上限变了：重新固定 WebView");
        assert!(
            !calls.iter().any(|c| matches!(c, Call::SetFrame(_))),
            "不在可见的外壳上硬切：{calls:?}"
        );
        assert!(calls.contains(&Call::Activate), "输入视图要焦点");
        host.fire(PanelEvent::PageLoaded);
        host.take();
        c.ready(3, 150.0).expect("shown");
        let reveal = host
            .take()
            .iter()
            .find_map(|c| match c {
                Call::RevealShell(r, _) => Some(*r),
                _ => None,
            })
            .expect("reveal");
        assert_eq!(
            (reveal.x, reveal.y, reveal.width),
            (moved.0.x, moved.0.y, moved.0.width)
        );
        // 同一位置再来一次：不必移动
        let host = MockHost::existing();
        let c = cold_with_window(&host, FrameMode::Native, PanelView::Translate);
        host.take();
        c.prepare(open(3, PanelView::Translate, anchor_a()));
        assert!(!host.take().iter().any(|c| matches!(c, Call::MoveShell(..))));
    }

    /// 页面在外壳显示期间加载完、前端的 ready 紧跟着到达：ready 要等外壳真正上屏之后才 reveal（frame_lock），不会出现
    /// reveal 在前、外壳的显示在后把内容重新藏起来
    #[test]
    fn ready_during_the_shell_show_waits_for_it() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.set_options(PanelOptions {
            unload_after: Some(Duration::from_secs(600)),
            ..PanelOptions::default()
        });
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        c.hide_now(1, HideReason::Escape);
        host.run_timers(Duration::from_secs(600));
        host.take();
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let host2 = Arc::clone(&host);
            let c2 = Arc::clone(&c);
            *host.before_shell.lock() = Some(Box::new(move || {
                host2.fire(PanelEvent::PageLoaded);
                let c3 = Arc::clone(&c2);
                std::thread::spawn(move || {
                    let _ = tx.send(c3.ready(2, 150.0));
                });
                // ready 在另一条线程上，必须等这次显示返回
                std::thread::sleep(Duration::from_millis(50));
            }));
        }
        c.prepare(open(2, PanelView::Input, anchor_a()));
        let shown = rx.recv_timeout(Duration::from_secs(5)).expect("ready returned");
        assert!(shown.is_some());
        let calls = host.take();
        let shell = calls
            .iter()
            .position(|c| matches!(c, Call::ShowShell(..)))
            .expect("shell");
        let reveal = calls
            .iter()
            .position(|c| matches!(c, Call::RevealShell(..)))
            .expect("reveal");
        assert!(shell < reveal, "{calls:?}");
    }

    /// 外壳没能显示（主线程超时）、而页面已在这期间加载完并 emit 过：退回普通的「准备中」，ready 超时后照常透明显示、
    /// ready 时淡入——这次触发不会被悄悄丢掉
    #[test]
    fn a_failed_shell_after_the_page_loaded_falls_back_to_a_normal_show() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.set_options(PanelOptions {
            unload_after: Some(Duration::from_secs(600)),
            ..PanelOptions::default()
        });
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        c.hide_now(1, HideReason::Escape);
        host.run_timers(Duration::from_secs(600));
        host.take();
        *host.fail_shell.lock() = true;
        {
            let host2 = Arc::clone(&host);
            *host.before_shell.lock() = Some(Box::new(move || host2.fire(PanelEvent::PageLoaded)));
        }
        c.prepare(open(2, PanelView::Translate, anchor_a()));
        wait_recreated(&host);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !host.calls.lock().contains(&Call::EmitPrepare(2)) {
            assert!(Instant::now() < deadline, "prepare was never emitted");
            std::thread::sleep(Duration::from_millis(2));
        }
        // 等外壳的失败路径走完
        let deadline = Instant::now() + Duration::from_secs(5);
        while c.snapshot().is_some_and(|s| s.visible) {
            assert!(Instant::now() < deadline, "shell session still on screen");
            std::thread::sleep(Duration::from_millis(2));
        }
        host.take();
        host.run_timers(READY_TIMEOUT);
        let calls = host.take();
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, Call::Show(_, o) if o.alpha == ShowAlpha::Transparent)),
            "普通的 ready 超时显示：{calls:?}"
        );
        assert!(
            calls.contains(&Call::Perf(2, PerfStage::ReadyTimeout)),
            "不再是外壳：照常记超时"
        );
        assert!(c.ready(2, 150.0).is_some(), "触发没有丢");
        assert!(host.take().iter().any(|c| matches!(c, Call::Reveal(_))));
    }

    /// 外壳期间 Rust 要求收起（Toggle：外壳期间再按一次剪贴板快捷键）：前端可能还没加载，直接淡出收起外壳并补发
    /// hide_request，不能只发一个没人接的事件
    #[test]
    fn a_rust_hide_request_on_the_shell_hides_it_natively() {
        let host = MockHost::existing();
        let c = cold_with_window(&host, FrameMode::Native, PanelView::Clipboard);
        host.take();
        c.request_hide(HideReason::Toggle);
        let calls = host.take();
        assert!(calls.contains(&Call::Hide(true)), "{calls:?}");
        assert!(calls.contains(&Call::EmitHideRequest(2, HideReason::Toggle)));
        assert!(c.snapshot().is_none());
    }

    /// 外壳看得见就要收得掉——外壳一上屏就装上关闭监听（翻译视图不是 key：全局 Esc、点击外部），
    /// 监听的回调（`request_hide_for`）对还没 ready 的外壳直接原生收起并补发 hide_request；交接给新会话时监听跟着换
    #[test]
    fn the_shell_has_close_guards_from_the_first_frame() {
        let host = MockHost::existing();
        let c = cold_with_window(&host, FrameMode::Native, PanelView::Translate);
        let calls = host.take();
        assert!(
            calls.iter().any(|c| matches!(c, Call::ArmGuards(2, _, false))),
            "外壳上屏即装关闭监听：{calls:?}"
        );
        // 交接：新会话装自己的监听
        c.prepare(open(3, PanelView::Translate, anchor_a()));
        let calls = host.take();
        assert!(calls.iter().any(|c| matches!(c, Call::ArmGuards(3, ..))), "{calls:?}");
        // 点击外部（监听的回调）：外壳直接淡出收起，补发 hide_request
        c.request_hide_for(3, HideReason::ClickOutside);
        let calls = host.take();
        assert!(calls.contains(&Call::Hide(true)), "{calls:?}");
        assert!(calls.contains(&Call::EmitHideRequest(3, HideReason::ClickOutside)));
        assert!(calls.contains(&Call::DisarmGuards));
        assert!(c.snapshot().is_none());
        // 已经结束的会话再来一次回调：什么都不做
        c.request_hide_for(3, HideReason::Escape);
        assert!(host.take().is_empty());
    }

    /// 页面还没加载时交接（路径 1）——新会话的 perf 立即补上 WindowShown（卡片早就在屏幕上）；外壳
    /// 此刻已是 key 而新视图要焦点时补记 KeyGained（窗口的 key 状态不变，不会再有焦点事件）
    #[test]
    fn a_hand_over_before_the_page_loads_records_window_and_key_for_the_new_session() {
        let host = MockHost::existing();
        let c = cold_with_window(&host, FrameMode::Native, PanelView::Input);
        host.fire(PanelEvent::Focused(true));
        host.take();
        c.prepare(open(3, PanelView::Clipboard, anchor_a()));
        let calls = host.take();
        assert!(calls.contains(&Call::Perf(3, PerfStage::WindowShown)), "{calls:?}");
        assert!(calls.contains(&Call::Perf(3, PerfStage::KeyGained)), "{calls:?}");
        assert!(!calls.contains(&Call::Activate), "外壳已是 key，不必再激活");
        // 翻译视图接过不是 key 的外壳：只补 WindowShown
        let host = MockHost::existing();
        let c = cold_with_window(&host, FrameMode::Native, PanelView::Translate);
        host.take();
        c.prepare(open(3, PanelView::Translate, anchor_a()));
        let calls = host.take();
        assert!(calls.contains(&Call::Perf(3, PerfStage::WindowShown)), "{calls:?}");
        assert!(!calls.contains(&Call::Perf(3, PerfStage::KeyGained)));
        // 页面加载、ready：WindowShown 只记第一次（PerfMarks 去重），这里只核对交接时已经记过
        host.fire(PanelEvent::PageLoaded);
        host.run_timers(READY_TIMEOUT);
        c.ready(3, 150.0).expect("shown");
        assert!(host.take().iter().any(|c| matches!(c, Call::RevealShell(..))));
    }

    /// 页面已加载、还没 ready 时交接（路径 2，经 prepare_fresh）——新会话同样在交接时记 WindowShown，
    /// 而不是等到 ready（否则 window_ms 等于 ready 的时刻，冷唤出门禁会看到假的超标）
    #[test]
    fn a_hand_over_after_the_page_loaded_records_window_shown_at_once() {
        let host = MockHost::existing();
        let c = cold_with_window(&host, FrameMode::Native, PanelView::Input);
        host.fire(PanelEvent::Focused(true));
        host.fire(PanelEvent::PageLoaded);
        host.take();
        c.prepare(open(3, PanelView::Input, anchor_a()));
        let calls = host.take();
        let window = calls
            .iter()
            .position(|c| *c == Call::Perf(3, PerfStage::WindowShown))
            .expect("window shown recorded at the hand-over");
        let emitted = calls.iter().position(|c| *c == Call::EmitPrepare(3)).expect("emit");
        assert!(window < emitted, "交接时就记，不等 ready：{calls:?}");
        assert!(calls.contains(&Call::Perf(3, PerfStage::KeyGained)));
        assert!(calls.iter().any(|c| matches!(c, Call::ArmGuards(3, _, true))));
        host.run_timers(READY_TIMEOUT);
        c.ready(3, 150.0).expect("shown");
        assert!(host.take().iter().any(|c| matches!(c, Call::RevealShell(..))));
    }

    /// 外壳的显示还在进行（主线程上）时来了新的 prepare——交接必须等显示完成（frame_lock），否则
    /// `move_shell` 先于 `show_shell` 执行，外壳被放回旧矩形与旧的 WebView 高度
    #[test]
    fn a_hand_over_waits_for_the_shell_show_in_flight() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.set_options(PanelOptions {
            unload_after: Some(Duration::from_secs(600)),
            ..PanelOptions::default()
        });
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        c.hide_now(1, HideReason::Escape);
        host.run_timers(Duration::from_secs(600));
        host.take();
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let c2 = Arc::clone(&c);
            *host.before_shell.lock() = Some(Box::new(move || {
                // 外壳的显示「在主线程上」进行时，另一条线程来了几何不同的 prepare
                let c3 = Arc::clone(&c2);
                std::thread::spawn(move || {
                    let mut next = open(3, PanelView::Input, anchor_b());
                    next.geometry = place_panel(&screen(), PlaceAnchor::Rect(anchor_b()), 360.0, 120.0, 400.0);
                    c3.prepare(next);
                    let _ = tx.send(());
                });
                std::thread::sleep(Duration::from_millis(50));
            }));
        }
        c.prepare(open(2, PanelView::Translate, anchor_a()));
        rx.recv_timeout(Duration::from_secs(5)).expect("hand-over returned");
        let calls = host.take();
        let shown = calls
            .iter()
            .position(|c| matches!(c, Call::ShowShell(..)))
            .expect("shell shown");
        let moved = calls
            .iter()
            .position(|c| matches!(c, Call::MoveShell(..)))
            .expect("shell moved");
        assert!(shown < moved, "交接排在外壳的显示之后：{calls:?}");
        assert_eq!(c.snapshot().map(|s| s.session_id), Some(3));
    }

    /// 外壳的显示失败时，回退处理的是此刻站在外壳上的会话（不论它是不是发起显示的那个）——页面还没
    /// 加载就退回暂存，加载后重放的是这个会话
    #[test]
    fn a_failed_shell_show_reverts_whichever_session_holds_the_shell() {
        let host = MockHost::existing();
        let c = cold_with_window(&host, FrameMode::Native, PanelView::Translate);
        c.prepare(open(3, PanelView::Translate, anchor_a()));
        host.take();
        // 发起显示的是会话 2，此刻站在外壳上的是 3
        c.shell_show_failed(2);
        assert!(c.snapshot().is_none(), "会话 3 不再被当作在屏幕上");
        assert!(host.take().contains(&Call::DisarmGuards));
        host.fire(PanelEvent::PageLoaded);
        assert!(
            host.take().contains(&Call::EmitPrepare(3)),
            "会话 3 退回暂存、页面加载后重放"
        );
    }

    /// 外壳下的页面 SHELL_LOAD_WAIT 内没加载完——重建之后不再显示第二张外壳（空卡片最多停一次等待），
    /// 新页面加载后这次 prepare 走普通的显示；新的触发不受这条限制
    #[test]
    fn a_stalled_shell_is_rebuilt_without_a_second_shell() {
        let host = MockHost::existing();
        let c = cold_with_window(&host, FrameMode::Native, PanelView::Translate);
        host.take();
        host.run_timers(SHELL_LOAD_WAIT);
        wait_recreated(&host);
        // 重建在阻塞线程上：等它走完
        let deadline = Instant::now() + Duration::from_secs(5);
        while !host.calls.lock().contains(&Call::Create) {
            assert!(Instant::now() < deadline, "not rebuilt");
            std::thread::sleep(Duration::from_millis(2));
        }
        std::thread::sleep(Duration::from_millis(30));
        let calls = host.take();
        assert!(calls.contains(&Call::Destroy), "{calls:?}");
        assert!(
            !calls.iter().any(|c| matches!(c, Call::ShowShell(..))),
            "不显示第二张外壳：{calls:?}"
        );
        host.fire(PanelEvent::PageLoaded);
        assert!(host.take().contains(&Call::EmitPrepare(2)), "重建之后照常准备");
        assert!(c.ready(2, 150.0).is_some());
        let calls = host.take();
        assert!(
            calls.iter().any(|c| matches!(c, Call::Show(..))),
            "普通的显示（有内容才出现）：{calls:?}"
        );
    }

    /// 外壳上的会话在新页面上 40 ms 内等不到 ready 是冷页面的常态：不记 ReadyTimeout（也不出 perf warn）
    #[test]
    fn the_shell_ready_timeout_is_not_a_warning() {
        let host = MockHost::existing();
        let _c = cold_with_window(&host, FrameMode::Native, PanelView::Translate);
        host.fire(PanelEvent::PageLoaded);
        host.take();
        host.run_timers(READY_TIMEOUT);
        let calls = host.take();
        assert!(!calls.contains(&Call::Perf(2, PerfStage::ReadyTimeout)), "{calls:?}");
        assert!(calls.contains(&Call::Schedule(READY_WATCHDOG)), "看门狗照常启动");
    }

    /// Css 帧模式（Windows / Linux）没有原生外壳：照旧等页面加载与 ready
    #[test]
    fn css_frame_mode_has_no_shell() {
        let host = MockHost::existing();
        let _c = cold_with_window(&host, FrameMode::Css, PanelView::Translate);
        std::thread::sleep(Duration::from_millis(20));
        assert!(!host.take().iter().any(|c| matches!(c, Call::ShowShell(..))));
    }

    #[test]
    fn rust_initiated_hides_notify_frontend() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Css, 24.0);
        c.prepare(open(1, PanelView::Clipboard, anchor_a()));
        c.ready(1, 300.0).expect("shown");
        host.take();
        assert!(c.hide_for_capture(Duration::from_millis(100)));
        let calls = host.take();
        let hide = calls.iter().position(|c| *c == Call::Hide(false)).expect("hidden");
        let notify = calls
            .iter()
            .position(|c| *c == Call::EmitHideRequest(1, HideReason::Capture))
            .expect("frontend told to run its exit flow");
        assert!(hide < notify, "先离屏再通知：{calls:?}");
        assert!(calls.contains(&Call::DisarmGuards));
        // 前端走完退出流程后的 panel_hide 对已隐藏的会话无效
        c.hide_now(1, HideReason::Capture);
        assert!(!host.take().iter().any(|c| matches!(c, Call::Hide(_))));
        // 固定交接（Native 路径由 WindowManager 在同一帧里 orderOut）
        c.prepare(open(2, PanelView::Translate, anchor_a()));
        c.ready(2, 120.0).expect("shown");
        host.take();
        assert!(!c.begin_external_hide_for(1), "已结束的会话");
        assert!(c.begin_external_hide_for(2));
        c.finish_external_hide(2, HideReason::Pin);
        let calls = host.take();
        assert!(calls.contains(&Call::EmitHideRequest(2, HideReason::Pin)), "{calls:?}");
        assert!(c.snapshot().is_none());
    }

    #[test]
    fn css_timeout_show_passes_clicks_and_opaque_window_waits() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Css, 24.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        host.take();
        host.run_timers(READY_TIMEOUT);
        let calls = host.take();
        let (rect, _) = calls
            .iter()
            .find_map(|c| match c {
                Call::Show(r, o) if o.alpha == ShowAlpha::Transparent => Some((*r, *o)),
                _ => None,
            })
            .expect("timeout show");
        let region = last_region(&calls).expect("whole-window click-through");
        assert_eq!(region.window, rect);
        assert!(region.passes_through(rect.x + rect.width / 2.0, rect.y + rect.height / 2.0));
        // 迟到的 ready：穿透区域换回卡片外
        c.ready(1, 120.0).expect("late ready");
        let region = last_region(&host.take()).expect("card click-through");
        assert!(!region.passes_through(310.0, 240.0));

        // 窗口不能透明（卡片铺满窗口）：超时不提前显示，看门狗照常
        let host = MockHost::existing();
        let c = loaded_with(&host, FrameMode::Css, 0.0, true);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        host.take();
        host.run_timers(READY_TIMEOUT);
        let calls = host.take();
        assert!(!calls.iter().any(|c| matches!(c, Call::Show(..))), "{calls:?}");
        assert!(calls.contains(&Call::Schedule(READY_WATCHDOG)));
        c.ready(1, 120.0).expect("late ready");
        assert!(
            host.take()
                .iter()
                .any(|c| matches!(c, Call::Show(_, o) if o.alpha == ShowAlpha::Opaque))
        );
    }

    #[test]
    fn blur_restores_focus_only_when_it_landed_on_shell() {
        let prev = AppRef {
            id: "notepad.exe".to_owned(),
            alt_id: None,
            name: "Notepad".to_owned(),
            pid: Some(42),
            path: None,
            window: Some(7),
        };
        for (on_shell, expect) in [(true, true), (false, false)] {
            let host = MockHost::existing();
            *host.restores_focus.lock() = true;
            *host.focus_on_shell.lock() = on_shell;
            let c = loaded(&host, FrameMode::Css, 24.0);
            let mut o = open(1, PanelView::Clipboard, anchor_a());
            o.prev_app = Some(prev.clone());
            c.prepare(o);
            c.ready(1, 300.0).expect("shown");
            host.fire(PanelEvent::Focused(true));
            host.fire(PanelEvent::Focused(false));
            host.take();
            // 前端播完退出动画后 panel_hide(blur)
            c.hide_now(1, HideReason::Blur);
            assert_eq!(host.take().contains(&Call::RestoreFocus), expect, "on_shell={on_shell}");
        }
        // Esc 一律还原；粘贴不还原
        let host = MockHost::existing();
        *host.restores_focus.lock() = true;
        let c = loaded(&host, FrameMode::Css, 24.0);
        for (id, reason, expect) in [(1, HideReason::Escape, true), (2, HideReason::Paste, false)] {
            let mut o = open(id, PanelView::Clipboard, anchor_a());
            o.prev_app = Some(prev.clone());
            c.prepare(o);
            c.ready(id, 300.0).expect("shown");
            host.take();
            c.hide_now(id, reason);
            assert_eq!(host.take().contains(&Call::RestoreFocus), expect, "{reason:?}");
        }
    }

    #[test]
    fn idle_unload_keeps_a_single_timer() {
        let after = Duration::from_secs(24 * 3600);
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.set_options(PanelOptions {
            unload_after: Some(after),
            ..PanelOptions::default()
        });
        for id in 1..=20 {
            c.prepare(open(id, PanelView::Translate, anchor_a()));
            c.ready(id, 120.0).expect("shown");
            c.hide_now(id, HideReason::Escape);
            assert_eq!(host.live_cancellable(after), 1, "频繁唤出时只保留最近一次的卸载计时器");
        }
        // 新的显示取消它
        c.prepare(open(21, PanelView::Translate, anchor_a()));
        assert_eq!(host.live_cancellable(after), 0);
        c.ready(21, 120.0).expect("shown");
        c.hide_now(21, HideReason::Escape);
        host.take();
        host.run_timers(after);
        assert_eq!(host.take().iter().filter(|c| **c == Call::Destroy).count(), 1);
    }

    /// 等后台重建（spawn_blocking）把窗口建好
    fn wait_recreated(host: &MockHost) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !*host.exists.lock() {
            assert!(Instant::now() < deadline, "panel was not recreated");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// 用过一次、隐藏后排好了回收计时器的控制器
    fn used_and_hidden(host: &Arc<MockHost>, options: PanelOptions) -> Arc<PanelController> {
        let c = loaded(host, FrameMode::Native, 0.0);
        c.set_options(options);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        c.hide_now(1, HideReason::Escape);
        host.take();
        c
    }

    /// 用过的面板隐藏 `recycle_after` 之后回收——销毁 WebView 并立即重建一个干净的预热页面；
    /// 连续使用时只保留最近一次的计时器；回收出来的页面照常显示，用过之后再次排回收
    #[test]
    fn used_panel_is_recycled_after_idle() {
        let after = Duration::from_secs(45);
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.set_options(PanelOptions {
            recycle_after: Some(after),
            ..PanelOptions::default()
        });
        for id in 1..=5 {
            c.prepare(open(id, PanelView::Translate, anchor_a()));
            c.ready(id, 120.0).expect("shown");
            c.hide_now(id, HideReason::Escape);
            assert_eq!(host.live_cancellable(after), 1, "连续使用时只保留最近一次的回收计时器");
        }
        host.take();
        host.run_timers(after);
        wait_recreated(&host);
        let calls = host.take();
        let destroy = calls.iter().position(|c| *c == Call::Destroy).expect("destroy");
        let create = calls.iter().position(|c| *c == Call::Create).expect("recreate");
        assert!(destroy < create, "{calls:?}");
        host.take_timers();
        host.fire(PanelEvent::PageLoaded);
        assert!(
            host.take_timers().is_empty(),
            "没开卸载：回收出来的页面加载后不再排任何计时器"
        );
        c.prepare(open(6, PanelView::Translate, anchor_a()));
        assert!(host.take().contains(&Call::EmitPrepare(6)), "预热好的页面立即 emit");
        c.ready(6, 120.0).expect("shown");
        c.hide_now(6, HideReason::Escape);
        assert!(host.take_timers().iter().any(|(d, _)| *d == after));
    }

    /// 显示期间页面被重新加载（→ 隐藏会话）：新页面没用过，隐藏后不排回收
    #[test]
    fn page_reloaded_while_shown_is_not_recycled() {
        let after = Duration::from_secs(45);
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.set_options(PanelOptions {
            recycle_after: Some(after),
            ..PanelOptions::default()
        });
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        host.take();
        host.take_timers();
        host.fire(PanelEvent::PageLoaded);
        assert!(
            host.take().iter().any(|c| matches!(c, Call::Hide(_))),
            "显示中的会话随页面重载隐藏"
        );
        assert!(c.snapshot().is_none(), "会话已结束");
        assert_eq!(host.live_cancellable(after), 0, "重载出来的页面没用过，不回收");
        assert!(host.take_timers().iter().all(|(d, _)| *d != after));
    }

    /// 回收进行中——旧窗口已销毁、新窗口还没建好（`exists() == false`、`creating == true`）——到达的 prepare 暂存，
    /// 不重复创建；页面加载后照常显示
    #[test]
    fn prepare_during_recycle_waits_for_the_new_page() {
        let after = Duration::from_secs(45);
        let host = MockHost::existing();
        let c = used_and_hidden(
            &host,
            PanelOptions {
                recycle_after: Some(after),
                ..PanelOptions::default()
            },
        );
        let (open_gate, gate) = std::sync::mpsc::channel();
        *host.create_gate.lock() = Some(gate);
        host.run_timers(after);
        // 销毁是同步的；重建卡在 create_gate 上
        assert!(!*host.exists.lock());
        assert_eq!(host.take(), vec![Call::Destroy]);
        c.prepare(open(2, PanelView::Translate, anchor_a()));
        let calls = host.take();
        assert!(
            !calls.contains(&Call::EmitPrepare(2)),
            "窗口还没建好，不 emit：{calls:?}"
        );
        assert!(c.is_live(2), "暂存的 prepare 仍然有效");
        open_gate.send(()).expect("open gate");
        wait_recreated(&host);
        // 重建完成之前到达的第二个 prepare 也只是替换暂存
        c.prepare(open(3, PanelView::Translate, anchor_a()));
        assert!(!c.is_live(2), "被更新的 prepare 取代");
        host.fire(PanelEvent::PageLoaded);
        let calls = host.take();
        assert_eq!(
            calls.iter().filter(|c| **c == Call::Create).count(),
            1,
            "整个回收只创建一次：{calls:?}"
        );
        assert!(calls.contains(&Call::EmitPrepare(3)), "{calls:?}");
        assert!(c.ready(3, 120.0).is_some());
    }

    /// 回收时其他 Wisp 窗口在忙（pinned 可见、截图中……）：推迟 RECYCLE_RETRY 再看，不在这时重建；不忙了才回收
    #[test]
    fn recycle_waits_until_other_windows_are_quiet() {
        let after = Duration::from_secs(45);
        let host = MockHost::existing();
        let _c = used_and_hidden(
            &host,
            PanelOptions {
                recycle_after: Some(after),
                ..PanelOptions::default()
            },
        );
        *host.busy.lock() = Some("pinned 1 opened".to_owned());
        host.run_timers(after);
        assert!(!host.take().contains(&Call::Destroy), "忙的时候不回收");
        assert_eq!(host.live_cancellable(RECYCLE_RETRY), 1, "推迟后再看");
        host.run_timers(RECYCLE_RETRY);
        assert!(!host.take().contains(&Call::Destroy));
        assert_eq!(
            host.live_cancellable(RECYCLE_RETRY),
            1,
            "仍在忙：再推迟一次（只留最新的计时器）"
        );
        *host.busy.lock() = None;
        host.run_timers(RECYCLE_RETRY);
        wait_recreated(&host);
        let calls = host.take();
        assert!(
            calls.contains(&Call::Destroy) && calls.contains(&Call::Create),
            "{calls:?}"
        );
    }

    /// 推迟有上限：一直「在忙」（例如漏掉了结束信号）时，推迟满 RECYCLE_DEFER_MAX 之后不再询问、直接回收——不会无限期
    /// 推迟，也不会留下一个长期每 15 s 醒一次的计时器
    #[test]
    fn recycle_deferral_is_capped() {
        let after = Duration::from_secs(45);
        let host = MockHost::existing();
        let _c = used_and_hidden(
            &host,
            PanelOptions {
                recycle_after: Some(after),
                ..PanelOptions::default()
            },
        );
        *host.busy.lock() = Some("capture in progress".to_owned());
        host.run_timers(after);
        let retries = RECYCLE_DEFER_MAX.as_secs() / RECYCLE_RETRY.as_secs();
        assert_eq!(retries, 8);
        for i in 0..retries {
            assert!(!host.take().contains(&Call::Destroy), "第 {i} 次推迟期间不回收");
            assert_eq!(host.live_cancellable(RECYCLE_RETRY), 1, "第 {i} 次推迟后只有一个计时器");
            host.run_timers(RECYCLE_RETRY);
        }
        wait_recreated(&host);
        let calls = host.take();
        assert!(
            calls.contains(&Call::Destroy) && calls.contains(&Call::Create),
            "推迟满上限后直接回收：{calls:?}"
        );
        assert!(
            host.take_timers().iter().all(|(d, _)| *d != RECYCLE_RETRY),
            "不再排推迟的计时器"
        );
    }

    /// 回收时重建失败（主线程忙、旧窗口迟迟不消失……）：面板退化为卸载状态，`recycled_at` 一并清掉——之后预热路径的
    /// 加载不能再拿这次回收的时刻去排剩余的卸载时长；下一次唤出照常重建
    #[test]
    fn failed_recycle_leaves_the_panel_unloaded() {
        let recycle = Duration::from_secs(45);
        let unload = Duration::from_secs(1800);
        let host = MockHost::existing();
        let c = used_and_hidden(
            &host,
            PanelOptions {
                unload_after: Some(unload),
                recycle_after: Some(recycle),
                ..PanelOptions::default()
            },
        );
        // 发送端立即丢弃：重建时 create 返回错误
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        drop(tx);
        *host.create_gate.lock() = Some(rx);
        host.run_timers(recycle);
        let deadline = Instant::now() + Duration::from_secs(5);
        while c.inner.lock().creating {
            assert!(Instant::now() < deadline, "recycle did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(host.take().contains(&Call::Destroy));
        assert!(!*host.exists.lock(), "重建失败：没有窗口");
        {
            let g = c.inner.lock();
            assert_eq!(g.recycled_at, None, "回收失败时清掉 recycled_at");
            assert_eq!(g.create_started, None);
        }
        // 预热路径（没有暂存的 prepare）的加载：不排剩余的卸载计时器
        c.create().expect("prewarm");
        host.fire(PanelEvent::PageLoaded);
        assert_eq!(host.live_cancellable(unload - recycle), 0, "{:?}", host.take());
        // 用过之后照常排回收（已经触发过的那个 45 s 计时器没有被取消，按差值数）
        let armed = host.live_cancellable(recycle);
        c.prepare(open(2, PanelView::Translate, anchor_a()));
        assert!(c.ready(2, 120.0).is_some());
        c.hide_now(2, HideReason::Escape);
        assert_eq!(host.live_cancellable(recycle), armed + 1);
    }

    /// 推迟期间新的 prepare 让这一轮空闲作废：推迟的计时器到期后什么都不做
    #[test]
    fn deferred_recycle_is_dropped_by_a_new_session() {
        let after = Duration::from_secs(45);
        let host = MockHost::existing();
        let c = used_and_hidden(
            &host,
            PanelOptions {
                recycle_after: Some(after),
                ..PanelOptions::default()
            },
        );
        *host.busy.lock() = Some("capture in progress".to_owned());
        host.run_timers(after);
        c.prepare(open(2, PanelView::Translate, anchor_a()));
        c.ready(2, 120.0).expect("shown");
        *host.busy.lock() = None;
        host.take();
        host.run_timers(RECYCLE_RETRY);
        assert!(!host.take().contains(&Call::Destroy), "显示中的面板不回收");
    }

    /// 用户开启了卸载、且它比回收晚：先回收（用过的页面多占的内存与唤醒不必等到卸载），回收出来的干净页面在卸载
    /// 到期时照常卸载（剩下的时长 = unload_after − recycle_after）
    #[test]
    fn unload_longer_than_recycle_recycles_first_then_unloads() {
        let recycle = Duration::from_secs(45);
        let unload = Duration::from_secs(1800);
        let host = MockHost::existing();
        let _c = used_and_hidden(
            &host,
            PanelOptions {
                unload_after: Some(unload),
                recycle_after: Some(recycle),
                ..PanelOptions::default()
            },
        );
        assert_eq!(host.live_cancellable(recycle), 1, "先排回收");
        assert_eq!(host.live_cancellable(unload), 0);
        host.run_timers(recycle);
        wait_recreated(&host);
        host.fire(PanelEvent::PageLoaded);
        let calls = host.take();
        assert!(
            calls.contains(&Call::Destroy) && calls.contains(&Call::Create),
            "{calls:?}"
        );
        let rest = unload - recycle;
        assert_eq!(host.live_cancellable(rest), 1, "回收出来的页面接着排剩下的卸载时长");
        host.run_timers(rest);
        let calls = host.take();
        assert!(calls.contains(&Call::Destroy), "{calls:?}");
        assert!(
            !calls.contains(&Call::Create),
            "卸载之后不重建，等下一次唤出：{calls:?}"
        );
    }

    /// 卸载不晚于回收：直接卸载，不先回收
    #[test]
    fn unload_not_longer_than_recycle_unloads_directly() {
        let recycle = Duration::from_secs(120);
        let unload = Duration::from_secs(60);
        let host = MockHost::existing();
        let _c = used_and_hidden(
            &host,
            PanelOptions {
                unload_after: Some(unload),
                recycle_after: Some(recycle),
                ..PanelOptions::default()
            },
        );
        assert_eq!(host.live_cancellable(recycle), 0);
        assert_eq!(host.live_cancellable(unload), 1);
        host.run_timers(unload);
        let calls = host.take();
        assert!(calls.contains(&Call::Destroy));
        assert!(!calls.contains(&Call::Create), "{calls:?}");
    }

    /// 回收被推迟到卸载也到期了：直接卸载（卸载只是销毁，不因其他窗口在忙而推迟）
    #[test]
    fn deferred_recycle_turns_into_unload_when_unload_is_due() {
        let recycle = Duration::from_secs(45);
        let unload = Duration::from_secs(50);
        let host = MockHost::existing();
        let _c = used_and_hidden(
            &host,
            PanelOptions {
                unload_after: Some(unload),
                recycle_after: Some(recycle),
                ..PanelOptions::default()
            },
        );
        *host.busy.lock() = Some("settings open".to_owned());
        host.run_timers(recycle);
        assert!(!host.take().contains(&Call::Destroy));
        host.run_timers(RECYCLE_RETRY);
        let calls = host.take();
        assert!(calls.contains(&Call::Destroy), "卸载到期：{calls:?}");
        assert!(!calls.contains(&Call::Create), "{calls:?}");
    }

    #[test]
    fn idle_plan_rules() {
        let recycle = Duration::from_secs(45);
        let unload = Duration::from_secs(600);
        let zero = Duration::ZERO;
        let opts = |unload_after, recycle_after| PanelOptions {
            unload_after,
            recycle_after,
            ..PanelOptions::default()
        };
        assert_eq!(idle_plan(&opts(None, None), true, zero), None);
        assert_eq!(
            idle_plan(&opts(None, Some(recycle)), false, zero),
            None,
            "没用过的页面不回收"
        );
        assert_eq!(
            idle_plan(&opts(None, Some(recycle)), true, zero),
            Some((recycle, IdleAction::Recycle))
        );
        assert_eq!(
            idle_plan(&opts(Some(unload), Some(recycle)), true, zero),
            Some((recycle, IdleAction::Recycle)),
            "卸载比回收晚：先回收"
        );
        assert_eq!(
            idle_plan(&opts(Some(unload), Some(recycle)), false, recycle),
            Some((unload - recycle, IdleAction::Unload)),
            "回收出来的页面：剩下的卸载时长"
        );
        assert_eq!(
            idle_plan(&opts(Some(recycle), Some(unload)), true, zero),
            Some((recycle, IdleAction::Unload)),
            "卸载不晚于回收：直接卸载"
        );
        assert_eq!(
            idle_plan(&opts(Some(recycle), Some(recycle)), true, zero),
            Some((recycle, IdleAction::Unload)),
            "同时到期：卸载"
        );
        assert_eq!(
            idle_plan(&opts(Some(unload), Some(recycle)), true, unload),
            Some((zero, IdleAction::Unload)),
            "推迟的回收拖到卸载到期：直接卸载"
        );
        assert_eq!(
            idle_plan(&opts(None, Some(recycle)), true, recycle + RECYCLE_RETRY),
            Some((zero, IdleAction::Recycle)),
            "推迟之后到期"
        );
        assert_eq!(
            idle_plan(&opts(Some(unload), None), false, zero),
            Some((unload, IdleAction::Unload))
        );
    }

    /// 点 [固定] 之后、pinned 就绪之前用户又触发了新的翻译——交接只针对发起固定的会话，新会话不受影响
    #[test]
    fn pin_handoff_only_hides_the_pinning_session() {
        // Native：在别处再次触发（旧会话先淡出，新会话在新位置显示）
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        c.prepare(open(2, PanelView::Translate, anchor_b()));
        c.ready(2, 120.0).expect("new session shown");
        host.take();
        assert!(!c.begin_external_hide_for(1), "发起固定的会话已经结束");
        c.request_hide_for(1, HideReason::Pin);
        let calls = host.take();
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, Call::Hide(_) | Call::EmitHideRequest(..) | Call::DisarmGuards)),
            "{calls:?}"
        );
        assert!(c.snapshot().is_some_and(|s| s.session_id == 2 && s.visible && s.shown));
        // 发起固定的会话还在时照常交接
        assert!(c.begin_external_hide_for(2));
        c.finish_external_hide(2, HideReason::Pin);
        assert!(host.take().contains(&Call::EmitHideRequest(2, HideReason::Pin)));

        // Css：交叉淡入原地替换出的新会话（窗口一直可见）同样不受影响
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Css, 24.0);
        c.prepare(open(1, PanelView::Clipboard, anchor_a()));
        c.ready(1, 300.0).expect("shown");
        c.prepare(open_with(2, anchor_b(), 420.0, PanelTransition::Crossfade));
        host.take();
        assert!(!c.begin_external_hide_for(1));
        c.request_hide_for(1, HideReason::Pin);
        assert!(!host.take().iter().any(|c| matches!(c, Call::EmitHideRequest(..))));
        assert!(c.ready(2, 160.0).is_some(), "新会话照常显示");
    }

    /// 决定显示之后、主线程执行显示之前开始的隐藏让这次显示作废：窗口不会留在屏幕上，会话由隐藏流程收尾
    #[test]
    fn hide_racing_show_never_leaves_window_on_screen() {
        for (mode, margin) in [(FrameMode::Native, 0.0), (FrameMode::Css, 24.0)] {
            // ready 的显示与截图前隐藏交错（隐藏的离屏先在主线程执行）
            let host = MockHost::existing();
            let c = loaded(&host, mode, margin);
            c.prepare(open(1, PanelView::Translate, anchor_a()));
            host.take();
            let racer = Arc::clone(&c);
            *host.before_show.lock() = Some(Box::new(move || {
                assert!(racer.hide_for_capture(Duration::from_millis(50)));
            }));
            assert!(c.ready(1, 120.0).is_none(), "{mode:?}：显示作废");
            let calls = host.take();
            assert!(calls.contains(&Call::ShowSkipped), "{calls:?}");
            assert!(
                !calls.iter().any(|c| matches!(c, Call::Show(..) | Call::ArmGuards(..))),
                "{calls:?}"
            );
            assert!(c.snapshot().is_none());
            assert_eq!(*c.hidden().borrow(), 1);
            // 之后的显示照常
            c.prepare(open(2, PanelView::Translate, anchor_a()));
            assert!(c.ready(2, 120.0).is_some());
            assert!(host.take().iter().any(|c| matches!(c, Call::Show(..))));

            // ready 超时后的 alpha 0 显示同理
            let host = MockHost::existing();
            let c = loaded(&host, mode, margin);
            c.prepare(open(1, PanelView::Translate, anchor_a()));
            host.take();
            let racer = Arc::clone(&c);
            *host.before_show.lock() = Some(Box::new(move || {
                assert!(racer.hide_for_capture(Duration::from_millis(50)));
            }));
            host.run_timers(READY_TIMEOUT);
            let calls = host.take();
            assert!(calls.contains(&Call::ShowSkipped), "{calls:?}");
            assert!(!calls.iter().any(|c| matches!(c, Call::Show(..))), "{calls:?}");
            assert!(c.snapshot().is_none());
            host.run_timers(READY_WATCHDOG);
            assert!(!host.take().contains(&Call::Destroy), "已结束的会话不触发重建");
        }
    }

    /// 截图前隐藏也拦住还没显示的会话与排队的 prepare，截图期间不会冒出面板
    #[test]
    fn capture_cancels_preparing_session_and_queued_prepares() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        host.take();
        assert!(!c.hide_for_capture(Duration::from_millis(50)), "窗口本来就不在屏幕上");
        let calls = host.take();
        assert!(
            calls.contains(&Call::EmitHideRequest(1, HideReason::Capture)),
            "{calls:?}"
        );
        assert!(!calls.iter().any(|c| matches!(c, Call::Hide(_))));
        assert_eq!(*c.hidden().borrow(), 1);
        assert!(c.ready(1, 120.0).is_none(), "取消的会话 ready 时不显示");
        host.run_timers(READY_TIMEOUT);
        assert!(
            !host.take().iter().any(|c| matches!(c, Call::Show(..))),
            "超时计时器也作废"
        );

        // Css「先退出再进入」：可见的会话立即隐藏，排队的 prepare 作废
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Css, 24.0);
        c.prepare(open(2, PanelView::Translate, anchor_a()));
        c.ready(2, 120.0).expect("shown");
        c.prepare(open(3, PanelView::Translate, anchor_b()));
        host.take();
        assert!(c.hide_for_capture(Duration::from_millis(50)));
        let calls = host.take();
        assert!(calls.contains(&Call::Hide(false)), "{calls:?}");
        assert!(!calls.contains(&Call::EmitPrepare(3)), "{calls:?}");
        // 前端随后的 panel_hide 对已结束的会话无效，也不会放出作废的 prepare
        c.hide_now(2, HideReason::Retrigger);
        assert!(!host.take().contains(&Call::EmitPrepare(3)));

        // Native 正在淡出的会话：立即完成淡出并等它离屏
        let host = MockHost::existing();
        *host.defer_hide.lock() = true;
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(4, PanelView::Translate, anchor_a()));
        c.ready(4, 120.0).expect("shown");
        c.hide_now(4, HideReason::Escape);
        host.take();
        let pending = Arc::new(Mutex::new(host.deferred_done.lock().drain(..).collect::<Vec<_>>()));
        assert_eq!(pending.lock().len(), 1);
        // 宿主的 finish_exit 在这里同步跑完淡出的回调（与 macOS 一致）
        let fade_out = Arc::clone(&pending);
        let runner = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            for done in fade_out.lock().drain(..) {
                done();
            }
        });
        assert!(c.hide_for_capture(Duration::from_secs(2)), "淡出中的窗口也在屏幕上");
        runner.join().expect("runner");
        assert!(host.take().contains(&Call::FinishExit));
        assert_eq!(*c.hidden().borrow(), 4);
    }

    /// 旧会话迟到的离屏确认（Linux 等）不能动到新会话：不放出新会话排队的 prepare、不释放 WebView 可见性
    #[test]
    fn late_hide_completion_leaves_new_session_alone() {
        // 新会话已经显示、又在「先退出再进入」时，旧会话的确认才到
        let host = MockHost::existing();
        *host.defer_hide.lock() = true;
        let c = loaded(&host, FrameMode::Css, 24.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        c.hide_now(1, HideReason::Escape);
        c.prepare(open(2, PanelView::Translate, anchor_a()));
        c.ready(2, 120.0).expect("shown");
        c.prepare(open(3, PanelView::Translate, anchor_b()));
        let calls = host.take();
        assert_eq!(
            calls
                .iter()
                .filter(|c| **c == Call::EmitHideRequest(2, HideReason::Retrigger))
                .count(),
            1
        );
        let late: Vec<_> = host.deferred_done.lock().drain(..).collect();
        assert_eq!(late.len(), 1);
        for done in late {
            done();
        }
        let calls = host.take();
        assert!(
            !calls.iter().any(|c| matches!(
                c,
                Call::EmitPrepare(3) | Call::ReleaseVisibility(_) | Call::EmitHideRequest(..) | Call::Schedule(_)
            )),
            "{calls:?}"
        );
        assert!(c.snapshot().is_some_and(|s| s.session_id == 2 && s.visible));
        // 会话 2 真正退出后才执行排队的 prepare
        *host.defer_hide.lock() = false;
        c.hide_now(2, HideReason::Retrigger);
        assert!(host.take().contains(&Call::EmitPrepare(3)));

        // 新会话还在准备（窗口已离屏）时旧确认才到：记下窗口已离屏，不释放新会话刚恢复的 WebView 可见性
        let host = MockHost::existing();
        *host.defer_hide.lock() = true;
        let c = loaded(&host, FrameMode::Css, 24.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        c.ready(1, 120.0).expect("shown");
        c.hide_now(1, HideReason::Escape);
        c.prepare(open(2, PanelView::Translate, anchor_b()));
        host.take();
        for done in host.deferred_done.lock().drain(..).collect::<Vec<_>>() {
            done();
        }
        assert!(!host.take().iter().any(|c| matches!(c, Call::ReleaseVisibility(_))));
        assert!(c.snapshot().is_some_and(|s| s.session_id == 2 && !s.visible));
        c.ready(2, 120.0).expect("shown");
        assert!(
            host.take().iter().any(|c| matches!(c, Call::Show(..))),
            "窗口已离屏：正常显示而不是原地替换"
        );
    }

    /// 原地替换按新视图决定可激活性，准备期间的焦点变化照样记下（ready 的激活、PanelShown.focused、Esc 守卫都以它为准）
    #[test]
    fn in_place_follows_new_view_focus_policy() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Css, 24.0);
        c.prepare(open(1, PanelView::Clipboard, anchor_a()));
        c.ready(1, 300.0).expect("shown");
        host.fire(PanelEvent::Focused(true));
        host.take();
        // 剪贴板（key）→ 翻译：面板此刻是 key，保持可激活，PanelShown 如实报告
        c.prepare(open_with(2, anchor_b(), 420.0, PanelTransition::Crossfade));
        assert!(host.take().contains(&Call::SetFocusable(true)));
        assert!(c.ready(2, 160.0).expect("in place").focused);
        // 准备期间失去 key：ready 以它为准，Esc 守卫按非 key 注册
        c.prepare(open_with(3, anchor_b(), 420.0, PanelTransition::Crossfade));
        host.fire(PanelEvent::Focused(false));
        host.take();
        assert!(!c.ready(3, 160.0).expect("in place").focused);
        let calls = host.take();
        assert!(
            calls.iter().any(|c| matches!(c, Call::ArmGuards(3, _, false))),
            "{calls:?}"
        );
        assert!(!calls.contains(&Call::Activate));
        // 不是 key 的翻译 → 翻译：恢复不激活（Windows 加回 WS_EX_NOACTIVATE）
        c.prepare(open_with(4, anchor_b(), 420.0, PanelTransition::Crossfade));
        assert!(host.take().contains(&Call::SetFocusable(false)));
        // → 输入（要焦点）：可激活，ready 时激活
        let mut input = open(5, PanelView::Input, anchor_a());
        input.transition = PanelTransition::Crossfade;
        c.prepare(input);
        assert!(host.take().contains(&Call::SetFocusable(true)));
        assert!(c.ready(5, 168.0).expect("in place").focused);
        assert!(host.take().contains(&Call::Activate));
    }

    /// ready 超时（alpha 0）后页面重载（WebContent 崩溃的典型后果）——重放 prepare，而不是静默丢掉这次触发
    #[test]
    fn reload_replays_timed_out_session() {
        let host = MockHost::existing();
        let c = loaded(&host, FrameMode::Native, 0.0);
        c.prepare(open(1, PanelView::Translate, anchor_a()));
        host.run_timers(READY_TIMEOUT);
        assert!(host.take().iter().any(|c| matches!(c, Call::Show(..))), "alpha 0 显示");
        host.fire(PanelEvent::PageLoaded);
        let calls = host.take();
        assert!(calls.contains(&Call::EmitPrepare(1)), "{calls:?}");
        assert!(!calls.iter().any(|c| matches!(c, Call::Hide(_))), "{calls:?}");
        // 旧轮次的看门狗作废：页面刚重载，不能马上判定失效
        host.run_timers(READY_WATCHDOG);
        assert!(!host.take().contains(&Call::Destroy));
        // 新轮次超时：窗口已经以 alpha 0 在屏幕上，只重启看门狗
        host.run_timers(READY_TIMEOUT);
        let calls = host.take();
        assert!(!calls.iter().any(|c| matches!(c, Call::Show(..))), "{calls:?}");
        assert!(calls.contains(&Call::Schedule(READY_WATCHDOG)));
        // 迟到的 ready：改到最终矩形并淡入
        c.ready(1, 120.0).expect("revealed");
        assert!(host.take().iter().any(|c| matches!(c, Call::Reveal(_))));
        host.run_timers(READY_WATCHDOG);
        assert!(!host.take().contains(&Call::Destroy));
    }
}
