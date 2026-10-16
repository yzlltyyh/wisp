//! 面板与各窗口的几何：全部是纯函数，带四角 / 翻转 / 夹紧 / 多显示器 / 缩放比单测。
//!
//! **坐标约定**：几何一律先在「锚点所在显示器内的逻辑坐标」（左上原点、逻辑 px，与 `wisp_types::Anchor` 一致）里算，
//! 最后再经 [`Screen::to_desktop`] 换成窗口系统要的「桌面坐标」：
//! - macOS（[`DesktopUnit::Logical`]）：全局逻辑坐标（pt，主屏左上为原点、y 向下），即 `MonitorInfo.frame` 的坐标系；
//! - Windows / Linux（[`DesktopUnit::Physical`]）：虚拟桌面的物理像素。混合缩放的多屏下全局逻辑坐标没有一致的定义
//!   （`MonitorInfo.frame` 在这两个平台上只用于排序与命中判断），所以按「显示器物理原点 + 本屏逻辑坐标 × 本屏缩放」换算，
//!   窗口落在哪块屏上就按哪块屏的缩放比摆放，不会在跨 DPI 的屏幕上偏移或变形。

use wisp_types::{MonitorId, MonitorInfo, PhysPoint, Placement, Point, Rect};

/// 面板与锚点的间距（锚点下方 12 px）
pub const ANCHOR_GAP: f64 = 12.0;
/// 面板与工作区边缘的最小距离（水平夹紧、上下翻转时留出的余量）
pub const EDGE_INSET: f64 = 8.0;
/// 以光标为锚点时把光标视作这么高的矩形（面板放在光标箭头下方，不挡住光标）
pub const CURSOR_HEIGHT: f64 = 16.0;
/// 下方空间至少有这么高（或达到本次上限）才放在下方；否则哪边空间大放哪边（下方放不下就翻到上方）
pub const PREFER_BELOW_MIN: f64 = 240.0;
/// 卡片最小高度（逻辑 px）：前端测量异常（0）时不把窗口缩成一条线
pub const MIN_CARD_HEIGHT: f64 = 32.0;

/// OCR 视图宽度
pub const OCR_WIDTH: f64 = 440.0;
/// 输入视图宽度
pub const INPUT_WIDTH: f64 = 420.0;
/// toast 视图宽度下限（内容宽度，夹在 220–360）
pub const TOAST_MIN_WIDTH: f64 = 220.0;
/// toast 视图宽度上限
pub const TOAST_MAX_WIDTH: f64 = 360.0;

/// 各视图 prepare 阶段的初始估计高度（真实高度以 ready 时前端的同步测量为准）
pub mod initial_height {
    /// 翻译
    pub const TRANSLATE: f64 = 120.0;
    /// OCR
    pub const OCR: f64 = 220.0;
    /// 输入
    pub const INPUT: f64 = 168.0;
    /// toast
    pub const TOAST: f64 = 40.0;
    /// 剪贴板（打开时的下限）
    pub const CLIPBOARD: f64 = 240.0;
}

/// 剪贴板面板打开时的高度下限（`clamp(内容, 240, min(560, 工作区 × 比例))`，会话内锁定）
pub const CLIPBOARD_MIN_HEIGHT: f64 = 240.0;
/// 剪贴板面板打开时的高度绝对上限
pub const CLIPBOARD_MAX_HEIGHT: f64 = 560.0;

/// `max_height_ratio` 的合法范围（设置项 0.4–0.8）
pub const MAX_HEIGHT_RATIO_RANGE: (f64, f64) = (0.4, 0.8);

/// Pinned 展开（⤢）的宽度（宽 640，高 min(内容, 屏幕 70%)，在面板所在屏幕居中）
pub const PINNED_EXPAND_WIDTH: f64 = 640.0;
/// Pinned 展开的最大高度比例
pub const PINNED_EXPAND_HEIGHT_RATIO: f64 = 0.7;
/// Pinned 窗口的最小宽度（与 tauri.conf.json 的 pinned 模板一致）
pub const PINNED_MIN_WIDTH: f64 = 320.0;
/// Pinned 窗口的最小高度
pub const PINNED_MIN_HEIGHT: f64 = 120.0;

/// 翻译宽度的 lerp 区间：显示宽度单位 80 起开始变宽，400 时到最大
const TRANSLATE_UNITS_START: f64 = 80.0;
const TRANSLATE_UNITS_SPAN: f64 = 320.0;

/// 桌面坐标的单位（见模块注释）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DesktopUnit {
    /// 全局逻辑坐标（macOS）
    Logical,
    /// 虚拟桌面物理像素（Windows / Linux）
    Physical,
}

/// 一块显示器的几何（取自 `MonitorInfo`）
#[derive(Debug, Clone, PartialEq)]
pub struct Screen {
    pub id: MonitorId,
    /// 全局逻辑坐标中的完整区域
    pub frame: Rect,
    /// 本屏逻辑坐标中的工作区（已扣除菜单栏、Dock、任务栏）
    pub work: Rect,
    pub scale: f64,
    pub physical_origin: PhysPoint,
    pub primary: bool,
}

impl Screen {
    /// 由平台层的显示器信息构造；缩放比非法时按 1 处理，工作区为空时退回整块屏幕
    pub fn from_info(m: &MonitorInfo) -> Self {
        let scale = if m.scale_factor.is_finite() && m.scale_factor > 0.0 {
            m.scale_factor
        } else {
            1.0
        };
        let full = Rect::new(0.0, 0.0, m.frame.width, m.frame.height);
        let work = if m.work_area.width > 0.0 && m.work_area.height > 0.0 {
            m.work_area
        } else {
            full
        };
        Self {
            id: m.id.clone(),
            frame: m.frame,
            work,
            scale,
            physical_origin: m.physical_origin,
            primary: m.is_primary,
        }
    }

    /// 本屏的完整区域（本屏逻辑坐标）
    pub fn local_full(&self) -> Rect {
        Rect::new(0.0, 0.0, self.frame.width, self.frame.height)
    }

    /// 本屏逻辑矩形 → 桌面坐标。物理坐标按像素取整（左上角与右下角分别四舍五入后相减，相邻矩形不留缝）
    pub fn to_desktop(&self, local: Rect, unit: DesktopUnit) -> Rect {
        match unit {
            DesktopUnit::Logical => Rect::new(
                self.frame.x + local.x,
                self.frame.y + local.y,
                local.width,
                local.height,
            ),
            DesktopUnit::Physical => {
                let ox = f64::from(self.physical_origin.x);
                let oy = f64::from(self.physical_origin.y);
                let x0 = (ox + local.x * self.scale).round();
                let y0 = (oy + local.y * self.scale).round();
                let x1 = (ox + local.right() * self.scale).round();
                let y1 = (oy + local.bottom() * self.scale).round();
                Rect::new(x0, y0, x1 - x0, y1 - y0)
            }
        }
    }

    /// 本屏逻辑点 → 桌面坐标
    pub fn point_to_desktop(&self, p: Point, unit: DesktopUnit) -> Point {
        let r = self.to_desktop(Rect::new(p.x, p.y, 0.0, 0.0), unit);
        Point { x: r.x, y: r.y }
    }

    /// 桌面坐标 → 本屏逻辑矩形（[`Self::to_desktop`] 的逆）
    pub fn from_desktop(&self, desktop: Rect, unit: DesktopUnit) -> Rect {
        match unit {
            DesktopUnit::Logical => Rect::new(
                desktop.x - self.frame.x,
                desktop.y - self.frame.y,
                desktop.width,
                desktop.height,
            ),
            DesktopUnit::Physical => Rect::new(
                (desktop.x - f64::from(self.physical_origin.x)) / self.scale,
                (desktop.y - f64::from(self.physical_origin.y)) / self.scale,
                desktop.width / self.scale,
                desktop.height / self.scale,
            ),
        }
    }

    /// 本屏在桌面坐标中的完整区域
    pub fn desktop_frame(&self, unit: DesktopUnit) -> Rect {
        self.to_desktop(self.local_full(), unit)
    }
}

/// 在显示器列表里找 `id`；找不到时依次退回主屏、第一块
pub fn screen_by_id<'a>(screens: &'a [Screen], id: &str) -> Option<&'a Screen> {
    screens
        .iter()
        .find(|s| s.id == id)
        .or_else(|| screens.iter().find(|s| s.primary))
        .or_else(|| screens.first())
}

/// 主屏（没有标记主屏时取第一块）
pub fn primary_screen(screens: &[Screen]) -> Option<&Screen> {
    screens.iter().find(|s| s.primary).or_else(|| screens.first())
}

/// 桌面坐标中的点落在哪块屏上；落在缝隙里时取中心最近的一块
pub fn screen_at_desktop(screens: &[Screen], p: Point, unit: DesktopUnit) -> Option<&Screen> {
    screens.iter().find(|s| s.desktop_frame(unit).contains(p)).or_else(|| {
        screens.iter().min_by(|a, b| {
            let da = center_distance(a.desktop_frame(unit), p);
            let db = center_distance(b.desktop_frame(unit), p);
            da.total_cmp(&db)
        })
    })
}

fn center_distance(r: Rect, p: Point) -> f64 {
    let cx = r.x + r.width / 2.0;
    let cy = r.y + r.height / 2.0;
    (cx - p.x).hypot(cy - p.y)
}

/// 点到矩形的距离（在矩形内为 0）
pub fn distance_to_rect(r: Rect, p: Point) -> f64 {
    let dx = (r.x - p.x).max(0.0).max(p.x - r.right());
    let dy = (r.y - p.y).max(0.0).max(p.y - r.bottom());
    dx.hypot(dy)
}

/// 文本的显示宽度单位（CJK 等全角字符每字计 2，其他计 1）。宽度公式在 400 单位时已饱和，
/// 所以数到 `cap` 就停（超长文本不必整篇扫描）
pub fn display_units(text: &str, cap: usize) -> usize {
    let mut units = 0usize;
    for c in text.chars() {
        units += if is_wide(c) { 2 } else { 1 };
        if units >= cap {
            return cap;
        }
    }
    units
}

/// 东亚宽字符（CJK 统一表意文字、假名、谚文、全角形式、CJK 标点与符号等）
fn is_wide(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x115F       // 谚文字母（初声）
        | 0x2E80..=0x303E     // CJK 部首、康熙部首、CJK 符号与标点
        | 0x3041..=0x33FF     // 平假名、片假名、注音、谚文兼容字母、CJK 兼容
        | 0x3400..=0x4DBF     // CJK 扩展 A
        | 0x4E00..=0x9FFF     // CJK 统一表意文字
        | 0xA000..=0xA4CF     // 彝文
        | 0xAC00..=0xD7A3     // 谚文音节
        | 0xF900..=0xFAFF     // CJK 兼容表意文字
        | 0xFE30..=0xFE4F     // CJK 兼容形式
        | 0xFF00..=0xFF60     // 全角 ASCII 与标点
        | 0xFFE0..=0xFFE6     // 全角符号
        | 0x1F300..=0x1F64F   // 常用 emoji（按 2 列显示）
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x2FFFD   // CJK 扩展 B–F
        | 0x30000..=0x3FFFD)
}

/// 翻译视图宽度：`lerp(min, max, clamp((u − 80) / 320, 0, 1))`；词典式固定为 `min`。
/// `min > max`（设置被手改）时以 min 为准
pub fn translate_width(source_text: &str, min: f64, max: f64, dict_mode: bool) -> f64 {
    let max = max.max(min);
    if dict_mode {
        return min;
    }
    let cap = (TRANSLATE_UNITS_START + TRANSLATE_UNITS_SPAN) as usize;
    let u = display_units(source_text, cap) as f64;
    let t = ((u - TRANSLATE_UNITS_START) / TRANSLATE_UNITS_SPAN).clamp(0.0, 1.0);
    (min + (max - min) * t).round()
}

/// toast 视图宽度（内容宽度，夹在 220–360）。toast 文案由前端按 `CopiedReason` 本地化，Rust 只知道粘贴键，
/// 这里按「一行提示 + 粘贴键徽标」估算；卡片在这个宽度内按内容排版
pub fn toast_width(paste_key: &str) -> f64 {
    // 提示文本约 26 个显示单位（中文 13 字 / 英文 26 字符），按 14 px 字号约 7 px 一单位；左右内边距与图标约 64 px
    const BASE_UNITS: f64 = 26.0;
    const UNIT_PX: f64 = 7.0;
    const CHROME_PX: f64 = 64.0;
    let key_units = display_units(paste_key, 32) as f64;
    (CHROME_PX + (BASE_UNITS + key_units) * UNIT_PX).clamp(TOAST_MIN_WIDTH, TOAST_MAX_WIDTH)
}

/// 本次会话的卡片高度上限：工作区高度 × `max_height_ratio`（比例夹在设置允许的范围内）
pub fn height_cap(screen: &Screen, max_height_ratio: f64) -> f64 {
    let ratio = if max_height_ratio.is_finite() {
        max_height_ratio.clamp(MAX_HEIGHT_RATIO_RANGE.0, MAX_HEIGHT_RATIO_RANGE.1)
    } else {
        0.6
    };
    (screen.work.height * ratio).floor().max(MIN_CARD_HEIGHT)
}

/// 剪贴板视图的尺寸（逻辑 px），与前端 `views/clipboard/model.ts` 同名常量一致（Rust 据此预估打开高度；
/// 测试 `clipboard_rows_match_frontend` 直接读 model.ts 核对，前端改了数值这里会失败）
pub mod clipboard_rows {
    pub const ROW: f64 = 44.0;
    pub const IMAGE_ROW: f64 = 64.0;
    /// 分组标题（「收藏 / 最近」）
    pub const GROUP: f64 = 28.0;
    /// 收藏分组末尾的「查看全部 N 个收藏」行（前端 `MORE_HEIGHT`）
    pub const MORE: f64 = 28.0;
    /// 「全部」列表里收藏分组最多列出几条，多出的由「查看全部」行代表（前端 `FAVORITES_PREVIEW`）
    pub const FAVORITES_PREVIEW: usize = 5;
    pub const LIST_PADDING: f64 = 4.0;
    pub const HEADER: f64 = 48.0;
    pub const HINT: f64 = 36.0;
    /// 「已暂停」横幅的条带（上 8 + 横幅 40 + 下 4）
    pub const BANNER: f64 = 52.0;
    pub const EMPTY: f64 = 176.0;
    /// 类型过滤菜单的项数（全部、收藏、文本、图片、文件、链接、代码、颜色；前端 `FILTERS`）
    pub const FILTER_ITEMS: f64 = 8.0;
    /// 打开高度的下限：头部下方放得下整个类型过滤菜单（每项 28 + 菜单内边距 8），前端 `OPEN_MIN_HEIGHT`。
    /// 剪贴板卡片在会话内只增不减，菜单若放不下会把卡片撑高、之后视图就比卡片矮了，所以下限按菜单算
    pub const OPEN_MIN: f64 = HEADER + 4.0 + (FILTER_ITEMS * 28.0 + 8.0) + 8.0;
}

/// 剪贴板视图顶部是否有记录状态横幅，与前端 `ClipboardView` 的 `banner` 同一规则：
/// 历史库不可用（列表为空时原因放在列表区里，不另起横幅）、监听装不上、设置里关了监听、已暂停，任一成立即有横幅
pub fn clipboard_has_banner(payload: &wisp_types::ClipboardPayload) -> bool {
    if payload.unavailable.is_some() {
        return !payload.page.items.is_empty();
    }
    payload.paused || !payload.recording.is_recording()
}

/// 剪贴板面板打开时卡片的预估高度：居中放置按「卡片中心在工作区高度 1/3 处」算位置，原先一律按初始
/// 高度 240 算，卡片随后向下长到 560，有历史时（常见情况）整张卡片偏下（中心约在 52% 处）。按首屏载荷估算前端的
/// `clamp(内容, 下限, min(560, 上限))`：头部 + 横幅（[`clipboard_has_banner`]）+ 提示条 + 列表
/// （[`clipboard_list_height`]），没有条目时是空状态
pub fn clipboard_estimated_height(payload: &wisp_types::ClipboardPayload, cap: f64) -> f64 {
    use clipboard_rows as r;
    let banner = clipboard_has_banner(payload);
    let chrome = r::HEADER + if banner { r::BANNER } else { 0.0 } + if payload.hint_bar { r::HINT } else { 0.0 };
    let list = if payload.page.items.is_empty() {
        r::EMPTY
    } else {
        // 超过最大高度之后的行不影响结果（总会被封顶），不必再数
        clipboard_list_height(
            &payload.page,
            payload.favorites_collapsed,
            CLIPBOARD_MAX_HEIGHT - chrome,
        )
    };
    let hi = CLIPBOARD_MAX_HEIGHT.min(cap).max(MIN_CARD_HEIGHT);
    (chrome + list).clamp(r::OPEN_MIN.min(hi), hi)
}

/// 打开时（「全部」、没有检索）列表内容的高度，逐行镜像前端 `buildRows(hits, { collapsed, cap: FAVORITES_PREVIEW })`
/// 与 `listContentHeight`：首尾留白各 4；每进入一组加一个分组标题；收藏分组折叠时只留标题，展开时只列前 5 条，
/// 收藏多于 5 条时在分组末尾加一行「查看全部」。收藏条数按这一页里的条目数（与前端一致，不用 `pinned_count`）。
/// 内容达到 `stop_at` 就停下（调用方只关心封顶之前的部分）
fn clipboard_list_height(page: &wisp_types::ClipPage, collapsed: bool, stop_at: f64) -> f64 {
    use clipboard_rows as r;
    let favorites = page.items.iter().filter(|h| h.item.pinned).count();
    // 离开收藏分组时，没列全的收藏由「查看全部」行代表（折叠时连它也不显示）
    let more = |listed: usize| {
        if !collapsed && listed < favorites { r::MORE } else { 0.0 }
    };
    let mut h = 2.0 * r::LIST_PADDING;
    let mut group: Option<bool> = None;
    let mut listed = 0usize;
    for hit in &page.items {
        let pinned = hit.item.pinned;
        if group != Some(pinned) {
            if group == Some(true) {
                h += more(listed);
            }
            h += r::GROUP;
            group = Some(pinned);
        }
        if pinned {
            if collapsed || listed >= r::FAVORITES_PREVIEW {
                continue;
            }
            listed += 1;
        }
        h += if hit.item.kind == wisp_types::ClipKind::Image {
            r::IMAGE_ROW
        } else {
            r::ROW
        };
        if h >= stop_at {
            return h;
        }
    }
    if group == Some(true) {
        h += more(listed);
    }
    h
}

/// 剪贴板面板打开时的锁定高度：`clamp(内容, 240, min(560, 上限))`
pub fn clipboard_open_height(content: f64, cap: f64) -> f64 {
    let hi = CLIPBOARD_MAX_HEIGHT.min(cap).max(MIN_CARD_HEIGHT);
    content.clamp(CLIPBOARD_MIN_HEIGHT.min(hi), hi)
}

/// 面板定位的锚点（本屏逻辑坐标）
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlaceAnchor {
    /// 选区矩形 / OCR 框选矩形 / 光标（光标用 [`cursor_rect`] 构造）
    Rect(Rect),
    /// 水平居中、卡片中心位于工作区高度 1/3 处（剪贴板的默认位置）
    Center,
    /// 记住的卡片左上角（「记住上次位置」），超出工作区时夹回
    At(Point),
}

/// 粘贴反馈 toast 的锚点（左上角）：与上一次面板**水平居中**、顶边对齐（若用面板左上角，420 宽的剪贴板面板
/// 之后约 260 宽的 toast 会贴在面板左侧，视觉上偏离面板中心）。超出工作区由 `place_panel` 夹回
pub fn toast_anchor(last: Rect, toast_width: f64) -> Point {
    Point {
        x: (last.x + (last.width - toast_width) / 2.0).round(),
        y: last.y,
    }
}

/// 光标锚点：光标处一个 [`CURSOR_HEIGHT`] 高、0 宽的矩形
pub fn cursor_rect(p: Point) -> Rect {
    Rect::new(p.x, p.y, 0.0, CURSOR_HEIGHT)
}

/// 一次显示的面板几何（本屏逻辑坐标）。窗口变高时 `edge_y` 这条边不动
#[derive(Debug, Clone, PartialEq)]
pub struct PanelGeometry {
    pub screen: Screen,
    /// 卡片左边 x
    pub x: f64,
    /// 固定边的 y：Below / Center 为卡片顶边，Above 为卡片底边
    pub edge_y: f64,
    pub placement: Placement,
    /// 卡片宽度（显示前确定，会话内锁定）
    pub width: f64,
    /// 卡片最大高度：min([`Self::cap`], 固定边到工作区边缘的可用空间)
    pub max_height: f64,
    /// 本次会话的高度上限：工作区 × `max_height_ratio`（剪贴板再夹到 560）。位置变化（拖动、交叉淡入保留位置）后
    /// 据此重算 `max_height`
    pub cap: f64,
    /// 锚点 x 相对卡片左边的偏移（Css 帧模式 transform-origin 用）
    pub origin_x: f64,
}

impl PanelGeometry {
    /// 高为 `height` 时的卡片矩形
    pub fn card_rect(&self, height: f64) -> Rect {
        let y = match self.placement {
            Placement::Above => self.edge_y - height,
            Placement::Below | Placement::Center => self.edge_y,
        };
        Rect::new(self.x, y, self.width, height)
    }

    /// 窗口高为 `window_height`、四周留 `margin` 时的窗口矩形（Native 帧模式 margin = 0，即卡片矩形）
    pub fn window_rect(&self, window_height: f64, margin: f64) -> Rect {
        let y = match self.placement {
            Placement::Above => self.edge_y + margin - window_height,
            Placement::Below | Placement::Center => self.edge_y - margin,
        };
        Rect::new(self.x - margin, y, self.width + 2.0 * margin, window_height)
    }

    /// 卡片高度夹到 [MIN_CARD_HEIGHT, max_height]；非有限值按最小高度
    pub fn clamp_height(&self, height: f64) -> f64 {
        if !height.is_finite() {
            return MIN_CARD_HEIGHT;
        }
        height.clamp(MIN_CARD_HEIGHT, self.max_height.max(MIN_CARD_HEIGHT))
    }

    /// 位置与宽度是否与另一次几何相同（判断「锚点变了就重新定位」）
    pub fn same_position(&self, other: &Self) -> bool {
        const EPS: f64 = 0.5;
        self.screen.id == other.screen.id
            && self.placement == other.placement
            && (self.x - other.x).abs() < EPS
            && (self.edge_y - other.edge_y).abs() < EPS
            && (self.width - other.width).abs() < EPS
    }

    /// 固定边到工作区边缘（留 [`EDGE_INSET`]）的可用高度
    pub fn available_height(&self) -> f64 {
        let work = self.screen.work;
        let avail = match self.placement {
            Placement::Above => self.edge_y - (work.y + EDGE_INSET),
            Placement::Below | Placement::Center => work.bottom() - EDGE_INSET - self.edge_y,
        };
        avail.max(0.0)
    }

    /// 原地替换视图时保留当前位置（输入 → 翻译、剪贴板条目 → 翻译时位置不变）：屏幕、卡片左边、固定边与
    /// 放置方向都不变，只换成新视图的宽度（右边越出工作区时整体左移夹回）与高度上限，再按固定边重算 `max_height`。
    /// 新卡片的左边只会不变或左移，所以新旧两张卡片的并集从新卡片的左边开始
    pub fn keep_position(&self, width: f64, cap: f64) -> Self {
        let work = self.screen.work;
        let width = width.min(work.width - 2.0 * EDGE_INSET).max(MIN_CARD_HEIGHT).round();
        let min_x = work.x + EDGE_INSET;
        let max_x = (work.right() - EDGE_INSET - width).max(min_x);
        let x = self.x.min(max_x).max(min_x).round();
        let anchor_x = self.x + self.origin_x;
        let mut kept = Self {
            screen: self.screen.clone(),
            x,
            edge_y: self.edge_y,
            placement: self.placement,
            width,
            max_height: 0.0,
            cap,
            origin_x: (anchor_x - x).clamp(0.0, width),
        };
        kept.max_height = cap.min(kept.available_height()).max(MIN_CARD_HEIGHT).floor();
        kept
    }

    /// 工作区里卡片可以占的竖直范围（上下各留 [`EDGE_INSET`]）：(顶, 底)，本屏逻辑坐标
    pub fn vertical_limits(&self) -> (f64, f64) {
        let work = self.screen.work;
        let top = work.y + EDGE_INSET;
        (top, (work.bottom() - EDGE_INSET).max(top))
    }

    /// 拖动过的面板的高度上限：不受固定边位置的限制（卡片伸出工作区时由 [`Self::fit_card`] 整体推回），只受会话上限
    /// `cap` 与整个工作区高度（上下各留 [`EDGE_INSET`]）限制
    pub fn movable_max_height(&self) -> f64 {
        let (top, bottom) = self.vertical_limits();
        self.cap.min(bottom - top).max(MIN_CARD_HEIGHT).floor()
    }

    /// 用户拖动面板时跟随窗口：`window` 是窗口外框在 `screen` 上的本屏逻辑坐标（含四周 `margin`）。卡片左边、固定边与
    /// 所在屏幕随窗口走——Below / Center 的卡片贴窗口顶边，Above 贴底边（Css 帧模式的 Above 窗口比卡片高）。宽度与
    /// `max_height` 不变：前端按 prepare 给的上限排版，会话内不改，拖动后卡片长到工作区外由 [`Self::fit_card`] 推回来。
    /// 换到另一块屏时 `cap` 按两块屏工作区高度的比例换算（会话上限是工作区 × 比例），之后的原地替换据此重算上限
    pub fn follow_window(&mut self, screen: Screen, window: Rect, margin: f64) {
        if screen.id != self.screen.id && self.screen.work.height > 0.0 {
            self.cap = (self.cap * screen.work.height / self.screen.work.height)
                .floor()
                .max(MIN_CARD_HEIGHT);
        }
        self.screen = screen;
        self.x = window.x + margin;
        self.edge_y = match self.placement {
            Placement::Above => window.bottom() - margin,
            Placement::Below | Placement::Center => window.y + margin,
        };
    }

    /// 拖动过的卡片留在工作区里：把高为 `card_height` 的卡片整体夹回当前屏幕的工作区（四周留 [`EDGE_INSET`]）。
    /// 水平方向越界时平移回来；竖直方向固定边照常不动，只有卡片伸出工作区时才把它推回来——向下长到底之后改为整体上移
    /// （顶边跟着往上走），Above 向上长到顶之后整体下移；卡片比工作区还高时顶边贴工作区顶。返回位置是否变了
    pub fn fit_card(&mut self, card_height: f64) -> bool {
        let work = self.screen.work;
        let min_x = work.x + EDGE_INSET;
        let max_x = (work.right() - EDGE_INSET - self.width).max(min_x);
        let x = self.x.clamp(min_x, max_x);
        let (top_limit, bottom_limit) = self.vertical_limits();
        let card = self.card_rect(card_height);
        let top = card.y.min(bottom_limit - card.height).max(top_limit);
        let edge_y = match self.placement {
            Placement::Above => top + card.height,
            Placement::Below | Placement::Center => top,
        };
        let moved = (x - self.x).abs() >= 0.5 || (edge_y - self.edge_y).abs() >= 0.5;
        self.x = x;
        self.edge_y = edge_y;
        moved
    }
}

/// 按锚点放置面板：
/// - `Rect`：放在锚点下方 [`ANCHOR_GAP`]、左对齐。记 `fit = min(初始高度, 上限)`、`want = max(min(上限, PREFER_BELOW_MIN), fit)`：
///   下方可用空间达到 `want`，或下方放得下 `fit` 且不比上方小时放下方；否则上方放得下 `fit` 就翻到上方（底边固定）；
///   两侧都放不下 `fit`（选区几乎占满工作区：⌘A 全选、整屏段落）时只能压住选区——取空间较大的一侧，把固定边夹回
///   工作区内、至少留出 `want` 的高度（宁可盖住一部分原文，也不出屏、不缩成一条）。水平夹紧到工作区内（留 [`EDGE_INSET`]）。
///   锚点在工作区之外（例如被菜单栏遮住的选区）时先把它夹回工作区；
/// - `Center`：水平居中，卡片中心在工作区高度 1/3 处（按初始高度），向下生长；
/// - `At`：记住的左上角，夹回工作区，向下生长。
///
/// 返回的 `max_height` 已扣除所选一侧的可用空间，卡片怎么长都留在工作区内
pub fn place_panel(screen: &Screen, anchor: PlaceAnchor, width: f64, initial_height: f64, cap: f64) -> PanelGeometry {
    let work = screen.work;
    let width = width.min(work.width - 2.0 * EDGE_INSET).max(MIN_CARD_HEIGHT).round();
    let initial = initial_height.max(MIN_CARD_HEIGHT);
    let min_x = work.x + EDGE_INSET;
    let max_x = (work.right() - EDGE_INSET - width).max(min_x);
    let clamp_x = |x: f64| x.clamp(min_x, max_x).round();
    let top_limit = work.y + EDGE_INSET;
    let bottom_limit = work.bottom() - EDGE_INSET;

    let (x, edge_y, placement, avail, origin) = match anchor {
        PlaceAnchor::Rect(a) => {
            let a = clamp_anchor(a, work);
            let below_top = a.bottom() + ANCHOR_GAP;
            let above_bottom = a.y - ANCHOR_GAP;
            let space_below = (bottom_limit - below_top).max(0.0);
            let space_above = (above_bottom - top_limit).max(0.0);
            let fit = initial.min(cap);
            let want = cap.min(PREFER_BELOW_MIN).max(fit);
            let x = clamp_x(a.x);
            if space_below >= want || (space_below >= fit && space_below >= space_above) {
                (x, below_top.round(), Placement::Below, space_below, a.x)
            } else if space_above >= fit {
                (x, above_bottom.round(), Placement::Above, space_above, a.x)
            } else if space_below >= space_above {
                // 压住选区（下侧）：顶边上移到「工作区底 − want」，仍不高于工作区顶
                let top = below_top.min(bottom_limit - want).max(top_limit).round();
                (x, top, Placement::Below, (bottom_limit - top).max(0.0), a.x)
            } else {
                // 压住选区（上侧）：底边下移到「工作区顶 + want」，仍不低于工作区底
                let bottom = above_bottom.max(top_limit + want).min(bottom_limit).round();
                (x, bottom, Placement::Above, (bottom - top_limit).max(0.0), a.x)
            }
        }
        PlaceAnchor::Center => {
            let x = clamp_x(work.x + (work.width - width) / 2.0);
            let center_y = work.y + work.height / 3.0;
            let top = (center_y - initial.min(cap) / 2.0).clamp(top_limit, (bottom_limit - initial).max(top_limit));
            (
                x,
                top.round(),
                Placement::Center,
                (bottom_limit - top).max(0.0),
                x + width / 2.0,
            )
        }
        PlaceAnchor::At(p) => {
            let x = clamp_x(p.x);
            let top = p.y.clamp(top_limit, (bottom_limit - initial).max(top_limit));
            (x, top.round(), Placement::Below, (bottom_limit - top).max(0.0), x)
        }
    };
    let max_height = cap.min(avail).max(MIN_CARD_HEIGHT).floor();
    PanelGeometry {
        screen: screen.clone(),
        x,
        edge_y,
        placement,
        width,
        max_height,
        cap,
        origin_x: (origin - x).clamp(0.0, width),
    }
}

/// 把锚点矩形夹回工作区（选区可能部分在菜单栏 / 任务栏下面或屏幕外）
fn clamp_anchor(a: Rect, work: Rect) -> Rect {
    let x = a.x.clamp(work.x, work.right());
    let y = a.y.clamp(work.y, work.bottom());
    let right = a.right().clamp(x, work.right());
    let bottom = a.bottom().clamp(y, work.bottom());
    Rect::new(x, y, right - x, bottom - y)
}

/// Pinned「展开」的卡片宽度（逻辑 px）：640，不超过工作区（两侧各留 `EDGE_INSET`），不小于固定窗口的最小宽度。
/// `PinState.expand_width`与 [`pinned_expand_rect`] 同一来源
pub fn pinned_expand_width(screen: &Screen) -> f64 {
    PINNED_EXPAND_WIDTH
        .min(screen.work.width - 2.0 * EDGE_INSET)
        .max(PINNED_MIN_WIDTH)
}

/// Pinned「展开」的目标矩形（本屏逻辑坐标）：宽 640（不超过工作区），高 `min(内容, 工作区 × 70%)`，在工作区居中
pub fn pinned_expand_rect(screen: &Screen, content_height: f64) -> Rect {
    let work = screen.work;
    let width = pinned_expand_width(screen);
    let max_h = (work.height * PINNED_EXPAND_HEIGHT_RATIO).floor();
    let height = if content_height.is_finite() {
        content_height.clamp(PINNED_MIN_HEIGHT, max_h.max(PINNED_MIN_HEIGHT))
    } else {
        max_h
    };
    Rect::new(
        (work.x + (work.width - width) / 2.0).round(),
        (work.y + (work.height - height) / 2.0).round(),
        width.round(),
        height.round(),
    )
}

/// 把一个窗口矩形（本屏逻辑坐标）夹回工作区：尺寸不超过工作区，位置整体落在工作区内
pub fn clamp_into_work(screen: &Screen, r: Rect) -> Rect {
    let work = screen.work;
    let width = r.width.min(work.width);
    let height = r.height.min(work.height);
    let x = r.x.clamp(work.x, work.right() - width);
    let y = r.y.clamp(work.y, work.bottom() - height);
    Rect::new(x, y, width, height)
}

/// 记住的窗口矩形是否仍可用：矩形与该屏工作区的重叠面积至少占窗口的一半
/// （显示器断开、分辨率变小后不把窗口放到看不见的地方）
pub fn restorable(screen: &Screen, r: Rect) -> bool {
    if !(r.width > 0.0 && r.height > 0.0) {
        return false;
    }
    let work = screen.work;
    let ix = (r.right().min(work.right()) - r.x.max(work.x)).max(0.0);
    let iy = (r.bottom().min(work.bottom()) - r.y.max(work.y)).max(0.0);
    ix * iy >= 0.5 * r.width * r.height
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1440×900 的主屏（2×），菜单栏 25，Dock 在底部 70
    fn mac_screen() -> Screen {
        Screen {
            id: "1".to_owned(),
            frame: Rect::new(0.0, 0.0, 1440.0, 900.0),
            work: Rect::new(0.0, 25.0, 1440.0, 805.0),
            scale: 2.0,
            physical_origin: PhysPoint { x: 0, y: 0 },
            primary: true,
        }
    }

    /// 右侧副屏：物理 3840×2160 @1.5（逻辑 2560×1440），物理原点 (2880, -300)
    fn win_secondary() -> Screen {
        Screen {
            id: r"\\.\DISPLAY2".to_owned(),
            frame: Rect::new(1920.0, -200.0, 2560.0, 1440.0),
            work: Rect::new(0.0, 0.0, 2560.0, 1392.0),
            scale: 1.5,
            physical_origin: PhysPoint { x: 2880, y: -300 },
            primary: false,
        }
    }

    #[test]
    fn display_units_counts_cjk_double() {
        assert_eq!(display_units("hello", 400), 5);
        assert_eq!(display_units("你好", 400), 4);
        assert_eq!(display_units("こんにちは", 400), 10);
        assert_eq!(display_units("한국어", 400), 6);
        assert_eq!(display_units("ＡＢ", 400), 4);
        assert_eq!(display_units("a你", 400), 3);
        // 达到上限即停
        assert_eq!(display_units(&"字".repeat(10_000), 400), 400);
    }

    #[test]
    fn translate_width_lerps_and_locks_dictionary() {
        assert_eq!(translate_width("short", 360.0, 520.0, false), 360.0);
        assert_eq!(translate_width(&"a".repeat(80), 360.0, 520.0, false), 360.0);
        assert_eq!(translate_width(&"a".repeat(240), 360.0, 520.0, false), 440.0);
        assert_eq!(translate_width(&"a".repeat(400), 360.0, 520.0, false), 520.0);
        assert_eq!(translate_width(&"a".repeat(5000), 360.0, 520.0, false), 520.0);
        // 120 个汉字 = 240 单位
        assert_eq!(translate_width(&"字".repeat(120), 360.0, 520.0, false), 440.0);
        // 词典式固定为 min
        assert_eq!(translate_width(&"a".repeat(400), 360.0, 520.0, true), 360.0);
        // min > max 时以 min 为准
        assert_eq!(translate_width(&"a".repeat(400), 480.0, 440.0, false), 480.0);
    }

    #[test]
    fn toast_width_is_clamped() {
        let w = toast_width("⌘V");
        assert!((TOAST_MIN_WIDTH..=TOAST_MAX_WIDTH).contains(&w), "{w}");
        assert_eq!(toast_width(&"Ctrl+Shift+Alt+V".repeat(4)), TOAST_MAX_WIDTH);
        assert!(toast_width("") >= TOAST_MIN_WIDTH);
    }

    #[test]
    fn height_cap_uses_ratio_within_range() {
        let s = mac_screen();
        assert_eq!(height_cap(&s, 0.6), (805.0f64 * 0.6).floor());
        assert_eq!(height_cap(&s, 0.1), (805.0f64 * 0.4).floor());
        assert_eq!(height_cap(&s, 2.0), (805.0f64 * 0.8).floor());
        assert_eq!(height_cap(&s, f64::NAN), (805.0f64 * 0.6).floor());
    }

    #[test]
    fn clipboard_height_is_locked_in_range() {
        assert_eq!(clipboard_open_height(100.0, 483.0), 240.0);
        assert_eq!(clipboard_open_height(300.0, 483.0), 300.0);
        assert_eq!(clipboard_open_height(900.0, 483.0), 483.0);
        assert_eq!(clipboard_open_height(900.0, 900.0), 560.0);
        // 上限比下限还小（极矮的屏幕）：以上限为准
        assert_eq!(clipboard_open_height(500.0, 200.0), 200.0);
    }

    #[test]
    fn places_below_anchor_left_aligned() {
        let s = mac_screen();
        let a = Rect::new(300.0, 200.0, 180.0, 18.0);
        let g = place_panel(&s, PlaceAnchor::Rect(a), 420.0, 120.0, 483.0);
        assert_eq!(g.placement, Placement::Below);
        assert_eq!(g.x, 300.0);
        assert_eq!(g.edge_y, 200.0 + 18.0 + ANCHOR_GAP);
        assert_eq!(g.card_rect(120.0), Rect::new(300.0, 230.0, 420.0, 120.0));
        assert_eq!(g.max_height, 483.0);
        assert_eq!(g.origin_x, 0.0);
    }

    #[test]
    fn four_corners_stay_inside_work_area() {
        let s = mac_screen();
        let w = 420.0;
        let cap = height_cap(&s, 0.6);
        let corners = [
            Rect::new(2.0, 30.0, 40.0, 16.0),     // 左上
            Rect::new(1420.0, 30.0, 18.0, 16.0),  // 右上
            Rect::new(2.0, 810.0, 40.0, 16.0),    // 左下
            Rect::new(1400.0, 812.0, 38.0, 16.0), // 右下
        ];
        for a in corners {
            let g = place_panel(&s, PlaceAnchor::Rect(a), w, 120.0, cap);
            let card = g.card_rect(g.max_height);
            assert!(card.x >= s.work.x + EDGE_INSET - 1e-9, "{a:?} → {card:?}");
            assert!(card.right() <= s.work.right() - EDGE_INSET + 1e-9, "{a:?} → {card:?}");
            assert!(card.y >= s.work.y + EDGE_INSET - 1e-9, "{a:?} → {card:?}");
            assert!(card.bottom() <= s.work.bottom() - EDGE_INSET + 1e-9, "{a:?} → {card:?}");
            // 上方的锚点放下方，下方的锚点翻到上方；与锚点保持 12 px
            if a.y < 400.0 {
                assert_eq!(g.placement, Placement::Below);
                assert_eq!(card.y, (a.bottom() + ANCHOR_GAP).round());
            } else {
                assert_eq!(g.placement, Placement::Above);
                assert_eq!(card.bottom(), (a.y - ANCHOR_GAP).round());
            }
        }
    }

    #[test]
    fn flips_above_and_keeps_bottom_edge() {
        let s = mac_screen();
        let a = Rect::new(500.0, 700.0, 100.0, 20.0);
        let g = place_panel(&s, PlaceAnchor::Rect(a), 420.0, 120.0, 483.0);
        assert_eq!(g.placement, Placement::Above);
        assert_eq!(g.edge_y, 688.0);
        // 底边固定：无论多高，卡片底边都在锚点上方 12 px
        for h in [40.0, 120.0, 300.0] {
            assert_eq!(g.card_rect(h).bottom(), 688.0);
        }
        assert_eq!(g.max_height, 483.0);
    }

    #[test]
    fn tight_space_picks_larger_side_and_limits_height() {
        // 矮屏（工作区 400 高），锚点在中间偏下：两侧都放不下 PREFER_BELOW_MIN，取大的一侧并限制上限
        let mut s = mac_screen();
        s.work = Rect::new(0.0, 0.0, 1440.0, 400.0);
        let a = Rect::new(100.0, 230.0, 50.0, 20.0);
        let g = place_panel(&s, PlaceAnchor::Rect(a), 420.0, 120.0, 300.0);
        assert_eq!(g.placement, Placement::Above);
        assert_eq!(g.max_height, (230.0f64 - ANCHOR_GAP - EDGE_INSET).floor());
        let card = g.card_rect(g.max_height);
        assert!(card.y >= EDGE_INSET - 1e-9);
    }

    /// 卡片长到 `max_height` 时整张都在工作区内（留 EDGE_INSET）
    fn assert_inside_work(s: &Screen, g: &PanelGeometry, what: &str) {
        let card = g.card_rect(g.max_height);
        let w = s.work;
        assert!(card.x >= w.x + EDGE_INSET - 1e-9, "{what}: {card:?}");
        assert!(card.right() <= w.right() - EDGE_INSET + 1e-9, "{what}: {card:?}");
        assert!(card.y >= w.y + EDGE_INSET - 1e-9, "{what}: {card:?}");
        assert!(card.bottom() <= w.bottom() - EDGE_INSET + 1e-9, "{what}: {card:?}");
    }

    #[test]
    fn selection_filling_work_area_overlaps_anchor_inside_work() {
        // AXBoundsForRange 给的是整段多行选区的外接矩形：⌘A 全选时它就是整个工作区，上下都没有空间
        let s = mac_screen();
        let cap = height_cap(&s, 0.6);
        let want = cap.min(PREFER_BELOW_MIN);
        let g = place_panel(&s, PlaceAnchor::Rect(s.work), 420.0, 120.0, cap);
        assert_inside_work(&s, &g, "整个工作区");
        assert_eq!(g.placement, Placement::Below);
        assert!(g.max_height >= want, "压住选区时至少留出 {want}：{g:?}");
        // 上下各只剩 10 px
        let a = Rect::new(100.0, s.work.y + 10.0, 600.0, s.work.height - 20.0);
        let g = place_panel(&s, PlaceAnchor::Rect(a), 420.0, 120.0, cap);
        assert_inside_work(&s, &g, "上下各剩 10 px");
        assert!(g.max_height >= want);
        // 上方略宽：压在上侧，底边固定
        let a = Rect::new(100.0, s.work.y + 90.0, 600.0, s.work.height - 100.0);
        let g = place_panel(&s, PlaceAnchor::Rect(a), 420.0, 120.0, cap);
        assert_eq!(g.placement, Placement::Above);
        assert_inside_work(&s, &g, "压在上侧");
        assert!(g.max_height >= want);
        // 一侧放得下初始高度时照常不压住选区
        let a = Rect::new(100.0, s.work.y + 200.0, 600.0, s.work.height - 220.0);
        let g = place_panel(&s, PlaceAnchor::Rect(a), 420.0, 120.0, cap);
        assert_eq!(g.placement, Placement::Above);
        assert_eq!(g.card_rect(40.0).bottom(), (a.y - ANCHOR_GAP).round());
    }

    #[test]
    fn any_anchor_keeps_card_inside_work_area() {
        // 锚点矩形在工作区里按网格扫一遍（含高度接近整屏的），卡片长到上限也不出工作区
        let s = mac_screen();
        let cap = height_cap(&s, 0.6);
        for initial in [initial_height::TRANSLATE, initial_height::CLIPBOARD] {
            for top in (0..=830).step_by(46) {
                for height in [0.0, 18.0, 200.0, 500.0, 760.0, 900.0] {
                    for left in [-50.0, 0.0, 700.0, 1430.0] {
                        let a = Rect::new(left, f64::from(top), 120.0, height);
                        let g = place_panel(&s, PlaceAnchor::Rect(a), 420.0, initial, cap);
                        assert_inside_work(&s, &g, &format!("{a:?} initial={initial}"));
                        assert!(g.max_height >= initial.min(cap), "{a:?} → {g:?}");
                    }
                }
            }
        }
    }

    /// 拖动时跟随窗口——卡片左边、固定边随窗口走（Css 帧模式扣掉阴影边距；Above 贴窗口底边），宽度与上限不变；
    /// 换到另一块屏时会话上限按两块屏工作区高度的比例换算
    #[test]
    fn follow_window_tracks_the_dragged_window() {
        let s = mac_screen();
        let mut g = place_panel(
            &s,
            PlaceAnchor::Rect(Rect::new(300.0, 200.0, 20.0, 16.0)),
            420.0,
            120.0,
            483.0,
        );
        let max = g.max_height;
        g.follow_window(s.clone(), Rect::new(476.0, 306.0, 468.0, 168.0), 24.0);
        assert_eq!((g.x, g.edge_y, g.width, g.max_height), (500.0, 330.0, 420.0, max));
        assert_eq!(g.cap, 483.0, "同一块屏：上限不变");
        // Above：卡片贴窗口底边
        let mut above = place_panel(
            &s,
            PlaceAnchor::Rect(Rect::new(300.0, 780.0, 20.0, 16.0)),
            420.0,
            120.0,
            483.0,
        );
        assert_eq!(above.placement, Placement::Above);
        above.follow_window(s.clone(), Rect::new(100.0, 50.0, 468.0, 531.0), 24.0);
        assert_eq!((above.x, above.edge_y), (124.0, 557.0));
        // 换到另一块屏（工作区高 1392）：本屏逻辑坐标、上限按比例
        let other = win_secondary();
        g.follow_window(other.clone(), Rect::new(40.0, 60.0, 468.0, 168.0), 24.0);
        assert_eq!(g.screen.id, other.id);
        assert_eq!((g.x, g.edge_y), (64.0, 84.0));
        assert_eq!(g.cap, (483.0f64 * 1392.0 / 805.0).floor());
    }

    /// 拖动过的卡片夹回工作区——水平越界平移回来；竖直方向固定边不动，只有伸出工作区时才整体推回（Below 到底改为顶边
    /// 上移，Above 到顶改为底边下移），卡片比工作区还高时顶边贴工作区顶
    #[test]
    fn fit_card_pushes_the_card_back_only_when_it_sticks_out() {
        let s = mac_screen();
        let (top, bottom) = (s.work.y + EDGE_INSET, s.work.bottom() - EDGE_INSET);
        let mut g = place_panel(
            &s,
            PlaceAnchor::Rect(Rect::new(300.0, 200.0, 20.0, 16.0)),
            420.0,
            120.0,
            483.0,
        );
        assert_eq!(g.vertical_limits(), (top, bottom));
        // 在工作区里：不动
        g.edge_y = 600.0;
        assert!(!g.fit_card(200.0));
        assert_eq!(g.edge_y, 600.0);
        // 长到伸出底部：顶边上移，底边贴工作区底
        assert!(g.fit_card(300.0));
        assert_eq!(g.card_rect(300.0).bottom(), bottom);
        // 变矮：顶边不动
        let edge = g.edge_y;
        assert!(!g.fit_card(100.0));
        assert_eq!(g.edge_y, edge);
        // 水平越界（拖出右边、左边）
        g.x = 1300.0;
        assert!(g.fit_card(100.0));
        assert_eq!(g.x + g.width, s.work.right() - EDGE_INSET);
        g.x = -50.0;
        assert!(g.fit_card(100.0));
        assert_eq!(g.x, EDGE_INSET);
        // 拖出顶部：顶边夹回
        g.edge_y = 0.0;
        assert!(g.fit_card(100.0));
        assert_eq!(g.edge_y, top);
        // 比工作区还高：顶边贴工作区顶
        g.edge_y = 400.0;
        assert!(g.fit_card(2000.0));
        assert_eq!(g.edge_y, top);
        // Above：向上长到顶之后底边下移
        let mut above = place_panel(
            &s,
            PlaceAnchor::Rect(Rect::new(300.0, 780.0, 20.0, 16.0)),
            420.0,
            120.0,
            483.0,
        );
        above.edge_y = 300.0;
        assert!(!above.fit_card(200.0));
        assert!(above.fit_card(400.0));
        assert_eq!((above.card_rect(400.0).y, above.edge_y), (top, top + 400.0));
    }

    #[test]
    fn keep_position_preserves_edge_and_rebounds_height() {
        let s = mac_screen();
        // 剪贴板（居中，420 宽）→ 翻译（520 宽）：左边与顶边不变，上限换成新视图的
        let clip = place_panel(&s, PlaceAnchor::Center, 420.0, 240.0, 483.0);
        let kept = clip.keep_position(520.0, 400.0);
        assert_eq!(
            (kept.x, kept.edge_y, kept.placement),
            (clip.x, clip.edge_y, clip.placement)
        );
        assert_eq!(kept.width, 520.0);
        assert_eq!(kept.cap, 400.0);
        assert_eq!(kept.max_height, 400.0f64.min(kept.available_height()).floor());
        assert!(kept.card_rect(kept.max_height).bottom() <= s.work.bottom() - EDGE_INSET);
        // 右边越出工作区时整体左移，新卡片仍覆盖旧卡片的水平范围
        let near_right = place_panel(
            &s,
            PlaceAnchor::Rect(Rect::new(1000.0, 100.0, 20.0, 16.0)),
            420.0,
            120.0,
            483.0,
        );
        let wide = near_right.keep_position(520.0, 483.0);
        assert_eq!(wide.x + wide.width, s.work.right() - EDGE_INSET);
        assert!(wide.x < near_right.x && wide.x + wide.width >= near_right.x + near_right.width);
        assert_eq!(wide.edge_y, near_right.edge_y);
        // Above：底边不变，可用高度按底边到工作区顶算
        let above = place_panel(
            &s,
            PlaceAnchor::Rect(Rect::new(300.0, 780.0, 20.0, 16.0)),
            420.0,
            120.0,
            483.0,
        );
        assert_eq!(above.placement, Placement::Above);
        let kept = above.keep_position(360.0, 300.0);
        assert_eq!((kept.edge_y, kept.placement), (above.edge_y, Placement::Above));
        assert_eq!(kept.max_height, 300.0);
        assert!(kept.card_rect(kept.max_height).y >= s.work.y + EDGE_INSET);
    }

    #[test]
    fn clamps_horizontally_and_records_origin() {
        let s = mac_screen();
        let a = Rect::new(1300.0, 100.0, 60.0, 18.0);
        let g = place_panel(&s, PlaceAnchor::Rect(a), 420.0, 120.0, 483.0);
        assert_eq!(g.x, 1440.0 - EDGE_INSET - 420.0);
        // transform-origin 指向锚点
        assert_eq!(g.origin_x, 1300.0 - g.x);
        // 超宽的卡片被限制在工作区内
        let g = place_panel(&s, PlaceAnchor::Rect(a), 2000.0, 120.0, 483.0);
        assert_eq!(g.width, 1440.0 - 2.0 * EDGE_INSET);
        assert_eq!(g.x, EDGE_INSET);
    }

    #[test]
    fn anchor_outside_work_area_is_clamped() {
        let s = mac_screen();
        // 选区在菜单栏里
        let g = place_panel(
            &s,
            PlaceAnchor::Rect(Rect::new(200.0, 2.0, 80.0, 14.0)),
            420.0,
            120.0,
            483.0,
        );
        assert_eq!(g.placement, Placement::Below);
        assert!(g.edge_y >= s.work.y + ANCHOR_GAP);
        // 选区在屏幕左侧之外
        let g = place_panel(
            &s,
            PlaceAnchor::Rect(Rect::new(-300.0, 300.0, 80.0, 14.0)),
            420.0,
            120.0,
            483.0,
        );
        assert_eq!(g.x, EDGE_INSET);
    }

    #[test]
    fn center_puts_card_center_at_one_third() {
        let s = mac_screen();
        let g = place_panel(&s, PlaceAnchor::Center, 420.0, 240.0, 483.0);
        assert_eq!(g.placement, Placement::Center);
        assert_eq!(g.x, 510.0);
        let card = g.card_rect(240.0);
        let center = card.y + card.height / 2.0;
        assert!((center - (25.0 + 805.0 / 3.0)).abs() <= 1.0, "{center}");
        assert!(g.card_rect(g.max_height).bottom() <= s.work.bottom());
    }

    fn clip_page(items: usize, pinned: usize) -> wisp_types::ClipPage {
        let mut page = wisp_types::ClipPage::empty(wisp_types::ClipFilter::All);
        for i in 0..items {
            let mut item: wisp_types::ClipItemMeta =
                serde_json::from_str(include_str!("../../../src/shared/ipc/fixtures/ClipItemMeta.json")).expect("meta");
            item.id = i64::try_from(i).expect("id");
            item.pinned = i < pinned;
            item.kind = wisp_types::ClipKind::Text;
            page.items.push(wisp_types::ClipHit {
                item,
                highlights: Vec::new(),
            });
        }
        page
    }

    fn clip_payload(items: usize, pinned: usize, paused: bool, hint_bar: bool) -> wisp_types::ClipboardPayload {
        wisp_types::ClipboardPayload {
            page: clip_page(items, pinned),
            paused,
            hint_bar,
            paste_on_double_click: false,
            unavailable: None,
            recording: if paused {
                wisp_types::ClipboardRecording::Paused
            } else {
                wisp_types::ClipboardRecording::Recording
            },
            favorites_collapsed: false,
            queue_unavailable: None,
        }
    }

    #[test]
    fn clipboard_open_height_follows_the_first_page() {
        let s = mac_screen();
        let cap = height_cap(&s, 0.6).min(CLIPBOARD_MAX_HEIGHT);
        // 空历史：头部 + 空状态 + 提示条，不低于下限
        let empty = clipboard_estimated_height(&clip_payload(0, 0, false, true), cap);
        assert_eq!(empty, clipboard_rows::OPEN_MIN.max(48.0 + 176.0 + 36.0));
        // 3 条：头部 48 + 分组 28 + 3 × 44 + 首尾留白 8 + 提示条 36 = 252，低于菜单下限时取下限
        let few = clipboard_estimated_height(&clip_payload(3, 0, false, true), cap);
        assert_eq!(few, clipboard_rows::OPEN_MIN.max(252.0));
        // 收藏 + 最近两组、暂停横幅
        let two_groups = clipboard_estimated_height(&clip_payload(4, 1, true, false), cap);
        assert_eq!(
            two_groups,
            clipboard_rows::OPEN_MIN.max(48.0 + 52.0 + 8.0 + 2.0 * 28.0 + 4.0 * 44.0)
        );
        // 1000 条：封顶 min(560, 上限)；居中放置时整张卡片的中心在工作区 1/3 处
        let full = clipboard_estimated_height(&clip_payload(1000, 0, false, true), cap);
        assert_eq!(full, cap.min(CLIPBOARD_MAX_HEIGHT));
        let g = place_panel(&s, PlaceAnchor::Center, 420.0, full, cap);
        let card = g.card_rect(full);
        let center = card.y + card.height / 2.0;
        let third = s.work.y + s.work.height / 3.0;
        assert!(
            (center - third).abs() <= 1.0 || card.y <= s.work.y + EDGE_INSET + 1.0,
            "{center} vs {third}"
        );
    }

    /// 尺寸常量直接对照前端 model.ts（唯一来源）：前端改了行高、收藏上限或过滤项数而这里没跟上时失败
    #[test]
    fn clipboard_rows_match_frontend() {
        use clipboard_rows as r;
        let model = include_str!("../../../src/views/clipboard/model.ts");
        let compact: String = model.chars().filter(|c| !c.is_whitespace()).collect();
        for decl in [
            format!("exportconstROW_HEIGHT={}", r::ROW),
            format!("exportconstIMAGE_ROW_HEIGHT={}", r::IMAGE_ROW),
            format!("exportconstGROUP_HEIGHT={}", r::GROUP),
            format!("exportconstMORE_HEIGHT={}", r::MORE),
            format!("exportconstFAVORITES_PREVIEW={}", r::FAVORITES_PREVIEW),
            format!("exportconstLIST_PADDING={}", r::LIST_PADDING),
            format!("exportconstHEADER_HEIGHT={}", r::HEADER),
            format!("exportconstHINT_HEIGHT={}", r::HINT),
            format!("exportconstEMPTY_HEIGHT={}", r::EMPTY),
            format!("exportconstOPEN_MAX_HEIGHT={CLIPBOARD_MAX_HEIGHT}"),
            format!(
                "exportconstOPEN_MIN_HEIGHT=HEADER_HEIGHT+4+({}*28+8)+8",
                r::FILTER_ITEMS
            ),
        ] {
            assert!(compact.contains(&decl), "model.ts 里找不到 `{decl}`");
        }
        // 横幅条带 = 上留白 + 横幅 + 下留白
        let banner = ["BANNER_TOP", "BANNER_BOX", "BANNER_BOTTOM"]
            .iter()
            .map(|name| {
                let key = format!("exportconst{name}=");
                let at = compact
                    .find(&key)
                    .unwrap_or_else(|| panic!("model.ts 里找不到 `{key}`"))
                    + key.len();
                let digits: String = compact[at..].chars().take_while(char::is_ascii_digit).collect();
                digits.parse::<f64>().expect("数值")
            })
            .sum::<f64>();
        assert_eq!(banner, r::BANNER);
        // 过滤菜单的项数
        let key = "exportconstFILTERS:readonlyClipFilter[]=[";
        let at = compact.find(key).expect("model.ts 里找不到 FILTERS") + key.len();
        let list = &compact[at..at + compact[at..].find(']').expect("FILTERS 的结尾")];
        let items = list.split(',').filter(|s| !s.is_empty()).count();
        assert_eq!(items as f64, r::FILTER_ITEMS, "FILTERS = [{list}]");
        assert_eq!(r::OPEN_MIN, 292.0);
    }

    /// 「全部」列表的收藏分组：与前端 model.test.ts 的「Rust 高度镜像」用例是同一组样例与数值（两边各自断言，
    /// 任何一边的算式变了都会失败）
    #[test]
    fn clipboard_list_mirrors_favorites_layout() {
        let list = |favorites: usize, recent: usize, collapsed: bool| {
            clipboard_list_height(&clip_page(favorites + recent, favorites), collapsed, f64::INFINITY)
        };
        // 没有收藏：「最近」标题 + 行
        assert_eq!(list(0, 3, false), 8.0 + 28.0 + 3.0 * 44.0);
        assert_eq!(list(0, 3, false), 168.0);
        // 只有收藏：展开列出全部（不超过 5 条）；折叠只留标题
        assert_eq!(list(3, 0, false), 168.0);
        assert_eq!(list(3, 0, true), 36.0);
        // 刚好 5 条：不加「查看全部」
        assert_eq!(list(5, 2, false), 8.0 + 28.0 + 5.0 * 44.0 + 28.0 + 2.0 * 44.0);
        assert_eq!(list(5, 2, false), 372.0);
        // 7 条：只列前 5 条 +「查看全部」行，然后是「最近」
        assert_eq!(list(7, 2, false), 8.0 + 28.0 + 5.0 * 44.0 + 28.0 + 28.0 + 2.0 * 44.0);
        assert_eq!(list(7, 2, false), 400.0);
        // 只有收藏且多于 5 条：「查看全部」在最后
        assert_eq!(list(7, 0, false), 284.0);
        // 折叠：收藏标题 +「最近」标题 + 行；没有最近时只剩收藏标题
        assert_eq!(list(7, 2, true), 8.0 + 28.0 + 28.0 + 2.0 * 44.0);
        assert_eq!(list(7, 2, true), 152.0);
        assert_eq!(list(7, 0, true), 36.0);
        assert_eq!(list(50, 2, false), 400.0);

        // 打开高度按载荷里的折叠状态算：头部 48 + 提示条 36 + 列表（上限取最大高度，不让屏幕高度先封顶）
        let cap = CLIPBOARD_MAX_HEIGHT;
        let mut p = clip_payload(9, 7, false, true);
        assert_eq!(clipboard_estimated_height(&p, cap), 84.0 + 400.0);
        p.favorites_collapsed = true;
        // 84 + 152 = 236，低于过滤菜单的下限
        assert_eq!(clipboard_estimated_height(&p, cap), clipboard_rows::OPEN_MIN);
        // 大量收藏：展开时只列前 5 条，不会因为收藏多就顶到最大高度
        let many = clip_payload(52, 50, false, true);
        assert_eq!(
            clipboard_estimated_height(&many, cap),
            84.0 + 8.0 + 28.0 + 5.0 * 44.0 + 28.0 + 28.0 + 2.0 * 44.0
        );
    }

    /// 横幅规则与前端 `ClipboardView` 一致：关了监听 / 监听装不上也有横幅；历史库不可用时只有列表非空才有横幅
    #[test]
    fn clipboard_banner_follows_the_view_rule() {
        let mut p = clip_payload(3, 0, false, true);
        assert!(!clipboard_has_banner(&p));
        p.recording = wisp_types::ClipboardRecording::Disabled;
        assert!(clipboard_has_banner(&p));
        p.recording = wisp_types::ClipboardRecording::Unavailable {
            reason_key: "errors.platform.clipboard_watch_unavailable".into(),
        };
        assert!(clipboard_has_banner(&p));
        p.recording = wisp_types::ClipboardRecording::Recording;
        p.unavailable = Some("clipboard.history_too_new".into());
        assert!(clipboard_has_banner(&p));
        let mut empty = clip_payload(0, 0, true, true);
        empty.unavailable = Some("clipboard.history_too_new".into());
        assert!(!clipboard_has_banner(&empty), "空列表时原因在列表区里，不另起横幅");
        // 关了监听时的预估高度带上横幅条带（原先只看 paused，卡片随后长高 52 px、位置偏下）
        let s = mac_screen();
        let cap = height_cap(&s, 0.6).min(CLIPBOARD_MAX_HEIGHT);
        let mut many = clip_payload(6, 0, false, true);
        let plain = clipboard_estimated_height(&many, cap);
        many.recording = wisp_types::ClipboardRecording::Disabled;
        assert_eq!(clipboard_estimated_height(&many, cap), plain + clipboard_rows::BANNER);
    }

    #[test]
    fn toast_is_centred_on_the_last_panel() {
        let last = Rect::new(546.0, 300.0, 420.0, 560.0);
        let p = toast_anchor(last, 260.0);
        assert_eq!((p.x, p.y), (626.0, 300.0));
        assert_eq!(p.x + 130.0, last.x + last.width / 2.0, "中心对齐");
    }

    #[test]
    fn remembered_position_is_clamped_into_work_area() {
        let s = mac_screen();
        let g = place_panel(&s, PlaceAnchor::At(Point { x: 5000.0, y: -50.0 }), 420.0, 240.0, 483.0);
        assert_eq!(g.x, 1440.0 - EDGE_INSET - 420.0);
        assert_eq!(g.edge_y, 25.0 + EDGE_INSET);
        let g = place_panel(&s, PlaceAnchor::At(Point { x: 300.0, y: 400.0 }), 420.0, 240.0, 483.0);
        assert_eq!((g.x, g.edge_y), (300.0, 400.0));
        assert_eq!(g.max_height, (830.0f64 - EDGE_INSET - 400.0).min(483.0).floor());
    }

    #[test]
    fn cursor_anchor_places_below_cursor() {
        let s = mac_screen();
        let g = place_panel(
            &s,
            PlaceAnchor::Rect(cursor_rect(Point { x: 400.0, y: 300.0 })),
            420.0,
            168.0,
            483.0,
        );
        assert_eq!(g.edge_y, 300.0 + CURSOR_HEIGHT + ANCHOR_GAP);
    }

    #[test]
    fn desktop_conversion_logical_and_physical() {
        let s = win_secondary();
        let local = Rect::new(100.0, 50.0, 420.0, 120.0);
        // macOS 风格：全局逻辑 = frame 原点 + 本屏逻辑
        assert_eq!(
            s.to_desktop(local, DesktopUnit::Logical),
            Rect::new(2020.0, -150.0, 420.0, 120.0)
        );
        // Windows / Linux：物理原点 + 本屏逻辑 × 缩放
        assert_eq!(
            s.to_desktop(local, DesktopUnit::Physical),
            Rect::new(3030.0, -225.0, 630.0, 180.0)
        );
        // 往返
        for unit in [DesktopUnit::Logical, DesktopUnit::Physical] {
            let back = s.from_desktop(s.to_desktop(local, unit), unit);
            assert!(
                (back.x - local.x).abs() < 1e-9 && (back.width - local.width).abs() < 1e-9,
                "{unit:?}"
            );
        }
        // 非整数缩放下按像素取整，相邻矩形不留缝
        let mut s125 = s.clone();
        s125.scale = 1.25;
        let a = s125.to_desktop(Rect::new(0.0, 0.0, 33.3, 10.0), DesktopUnit::Physical);
        let b = s125.to_desktop(Rect::new(33.3, 0.0, 10.0, 10.0), DesktopUnit::Physical);
        assert_eq!(a.right(), b.x);
    }

    #[test]
    fn multi_monitor_lookup() {
        let screens = vec![
            Screen {
                id: r"\\.\DISPLAY1".to_owned(),
                frame: Rect::new(0.0, 0.0, 1920.0, 1080.0),
                work: Rect::new(0.0, 0.0, 1920.0, 1040.0),
                scale: 1.0,
                physical_origin: PhysPoint { x: 0, y: 0 },
                primary: true,
            },
            win_secondary(),
        ];
        let p = Point { x: 3000.0, y: 100.0 };
        assert_eq!(
            screen_at_desktop(&screens, p, DesktopUnit::Physical).map(|s| s.id.as_str()),
            Some(r"\\.\DISPLAY2")
        );
        assert_eq!(
            screen_at_desktop(&screens, Point { x: 10.0, y: 10.0 }, DesktopUnit::Physical).map(|s| s.id.as_str()),
            Some(r"\\.\DISPLAY1")
        );
        // 缝隙中的点取最近的屏幕
        assert!(screen_at_desktop(&screens, Point { x: -50.0, y: 500.0 }, DesktopUnit::Physical).is_some());
        // 未知 id 退回主屏
        assert_eq!(
            screen_by_id(&screens, "gone").map(|s| s.id.as_str()),
            Some(r"\\.\DISPLAY1")
        );
        assert_eq!(primary_screen(&screens).map(|s| s.id.as_str()), Some(r"\\.\DISPLAY1"));
        // 副屏上放置：几何在副屏本地坐标里算，窗口按副屏的缩放比摆放
        let s = &screens[1];
        let g = place_panel(
            s,
            PlaceAnchor::Rect(Rect::new(2500.0, 1300.0, 40.0, 16.0)),
            420.0,
            120.0,
            835.0,
        );
        assert_eq!(g.placement, Placement::Above);
        assert!(g.card_rect(120.0).right() <= s.work.right() - EDGE_INSET);
        let desk = s.to_desktop(g.card_rect(120.0), DesktopUnit::Physical);
        assert_eq!(desk.width, 630.0);
        assert!(
            s.desktop_frame(DesktopUnit::Physical)
                .contains(Point { x: desk.x, y: desk.y })
        );
    }

    #[test]
    fn mixed_scale_keeps_logical_card_size_per_screen() {
        // 同一张 420×120 的卡片：1× 屏上物理 420×120，1.5× 屏上物理 630×180（逻辑尺寸一致）
        let one = Screen {
            id: "a".to_owned(),
            frame: Rect::new(0.0, 0.0, 1920.0, 1080.0),
            work: Rect::new(0.0, 0.0, 1920.0, 1040.0),
            scale: 1.0,
            physical_origin: PhysPoint { x: 0, y: 0 },
            primary: true,
        };
        let card = Rect::new(10.0, 10.0, 420.0, 120.0);
        assert_eq!(one.to_desktop(card, DesktopUnit::Physical).width, 420.0);
        assert_eq!(win_secondary().to_desktop(card, DesktopUnit::Physical).width, 630.0);
    }

    #[test]
    fn distance_to_rect_is_zero_inside() {
        let r = Rect::new(10.0, 10.0, 100.0, 50.0);
        assert_eq!(distance_to_rect(r, Point { x: 20.0, y: 20.0 }), 0.0);
        assert_eq!(distance_to_rect(r, Point { x: 0.0, y: 20.0 }), 10.0);
        assert_eq!(distance_to_rect(r, Point { x: 113.0, y: 64.0 }), 5.0);
    }

    #[test]
    fn pinned_expand_rect_is_centered_and_bounded() {
        let s = mac_screen();
        let r = pinned_expand_rect(&s, 300.0);
        assert_eq!(r.width, 640.0);
        assert_eq!(r.height, 300.0);
        assert_eq!(r.x, 400.0);
        assert!((r.y + r.height / 2.0 - (25.0 + 805.0 / 2.0)).abs() <= 1.0);
        let r = pinned_expand_rect(&s, 5000.0);
        assert_eq!(r.height, (805.0f64 * 0.7).floor());
        let r = pinned_expand_rect(&s, 10.0);
        assert_eq!(r.height, PINNED_MIN_HEIGHT);
    }

    #[test]
    fn restorable_requires_half_visible() {
        let s = mac_screen();
        assert!(restorable(&s, Rect::new(100.0, 100.0, 760.0, 560.0)));
        assert!(!restorable(&s, Rect::new(1200.0, 100.0, 760.0, 560.0)));
        assert!(!restorable(&s, Rect::new(0.0, 0.0, 0.0, 560.0)));
        let c = clamp_into_work(&s, Rect::new(1200.0, 700.0, 760.0, 560.0));
        assert_eq!(c, Rect::new(680.0, 270.0, 760.0, 560.0));
    }

    #[test]
    fn screen_from_info_falls_back_on_bad_values() {
        let info = MonitorInfo {
            id: "9".to_owned(),
            name: "x".to_owned(),
            physical_origin: PhysPoint { x: 0, y: 0 },
            physical_size: wisp_types::PhysSize {
                width: 100,
                height: 100,
            },
            scale_factor: 0.0,
            frame: Rect::new(0.0, 0.0, 100.0, 100.0),
            work_area: Rect::default(),
            is_primary: false,
        };
        let s = Screen::from_info(&info);
        assert_eq!(s.scale, 1.0);
        assert_eq!(s.work, Rect::new(0.0, 0.0, 100.0, 100.0));
    }
}
