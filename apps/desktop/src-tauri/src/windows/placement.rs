//! 面板与各窗口的几何：全部是纯函数，带四角 / 翻转 / 夹紧 / 多显示器 / 缩放比单测。
//!
//! **坐标约定**：几何一律先在「锚点所在显示器内的逻辑坐标」（左上原点、逻辑 px，与 `wisp_types::Anchor` 一致）里算，
//! 最后再经 [`Screen::to_desktop`] 换成窗口系统要的「桌面坐标」：
//! - macOS（[`DesktopUnit::Logical`]）：全局逻辑坐标（pt，主屏左上为原点、y 向下），即 `MonitorInfo.frame` 的坐标系；
//! - Windows / Linux（[`DesktopUnit::Physical`]）：虚拟桌面的物理像素。混合缩放的多屏下全局逻辑坐标没有一致的定义
//!   （`MonitorInfo.frame` 在这两个平台上只用于排序与命中判断），所以按「显示器物理原点 + 本屏逻辑坐标 × 本屏缩放」换算，
//!   窗口落在哪块屏上就按哪块屏的缩放比摆放，不会在跨 DPI 的屏幕上偏移或变形。

use wisp_types::{Lang, MonitorId, MonitorInfo, PanelSide, PhysPoint, Placement, Point, Rect, TaskKind};

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
/// 长内容换到上方或侧面至少要多出这么高（约 4 行正文）：只多一两行时不值得让卡片离开最常见的「选区下方」
pub const SIDE_GAIN_MIN: f64 = 96.0;

/// 角落提示离工作区边缘的距离
pub const CORNER_INSET: f64 = 16.0;
/// 角落提示（连续粘贴的进度）的卡片高度上限：标题一行 + 说明两行是 73（toast.css：文字上下各留 10、上方另有 1、标题 18、
/// 间距 2、说明每行 16），多出的给字体差异留余量；说明折成三行时是 89，只少 1 px 下边距、文字不被截。「留到下一次」等说明
/// 是这一次唯一要紧的信息，文案按两行写（浏览器级用例 toast.e2e.ts 核对）。Css 帧模式 Above 放置的窗口恒为上限高，按普通
/// 会话的上限（工作区 × 比例）会在屏幕角落生成一个几百 px 高的透明窗口
pub const NOTICE_MAX_HEIGHT: f64 = 88.0;

/// OCR 视图宽度（框选较窄、光标锚点与未就绪时的宽度）
pub const OCR_WIDTH: f64 = 440.0;
/// OCR 视图随框选变宽的上限（内容宽 568 ≈ 80 个拉丁字符 / 40 个汉字一行，再宽就难读了）
pub const OCR_MAX_WIDTH: f64 = 600.0;
/// 默认正文字号（`--text-body`）：框选里的屏幕文字按这个字号近似，正文字号调大时卡片相应变宽
const BODY_FONT: f64 = 14.0;
/// 卡片左右内边距之和（`--pad-panel-x` 16 × 2）
const CONTENT_INSET: f64 = 32.0;
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

/// OCR 视图宽度随框选：`clamp(round(框选宽 × 字号 / 14 + 32), 440, 600)`。逐行模式要与原图逐行对照，框选比 408 px
/// 宽时 440 的卡片会把每一行折成两行；框选里的屏幕文字按默认正文字号近似，正文字号调大时同样的一行需要更宽的卡片。
/// 框选宽度不是有限正数时取 [`OCR_WIDTH`]。超过工作区时由 `place_panel` 照常夹回
pub fn ocr_width(capture_width: f64, font_size: f64) -> f64 {
    if !(capture_width.is_finite() && capture_width > 0.0) {
        return OCR_WIDTH;
    }
    (capture_width * body_font(font_size) / BODY_FONT + CONTENT_INSET)
        .round()
        .clamp(OCR_WIDTH, OCR_MAX_WIDTH)
}

/// 设置里的正文字号（validate 已夹到 12–18）；非法值按默认 14
fn body_font(font_size: f64) -> f64 {
    if font_size.is_finite() && font_size > 0.0 {
        font_size
    } else {
        BODY_FONT
    }
}

/// 预计内容高度（只用来选放在哪一侧，见 [`PlaceOpts::expected`]）的版式常量。OCR 的头部 / 底栏 / 内边距直接用前端
/// `views/ocr/model.ts` 的 `LAYOUT`（测试 `ocr_layout_matches_frontend` 读 model.ts 核对）
pub mod expected_layout {
    /// 正文行高 = 字号 × 1.6（`--lh-body`）
    pub const LINE_HEIGHT: f64 = 1.6;
    /// 卡片上下内边距（`--pad-panel-y` 12 × 2）
    pub const CARD_PAD: f64 = 24.0;
    /// 翻译视图除正文以外的高度：头部 34 + 底栏 38 + 卡片上下内边距 24
    pub const TRANSLATE_CHROME: f64 = 96.0;
    /// 原文对照一行的高度：UI 字号 13 px × 1.6
    pub const SOURCE_LINE: f64 = 20.8;
    /// 原文对照的框架（与正文之间的分隔与留白，translate.css 的 21 px）
    pub const SOURCE_CHROME: f64 = 21.0;
    /// 原文对照最多占视图高度的比例（translate.css：`--tv-max × 0.4 − 21px`）
    pub const SOURCE_SHARE: f64 = 0.4;
    /// 原文对照的字号（每个显示宽度单位约半个字号宽）
    pub const SOURCE_FONT: f64 = 13.0;
    /// 词典式结果的预计高度（释义、例句的卡片，与原文长度无关）
    pub const DICT: f64 = 280.0;
    /// 估计到这么高就停：远超任何会话上限，超长文本不必整篇扫描
    pub const STOP: f64 = 2000.0;
    /// 截图里一行文字的高度：屏幕常见正文 13–14 px × 行高 1.3–1.4
    pub const CAPTURE_LINE: f64 = 18.0;

    /// 前端 `views/ocr/model.ts` 的 `LAYOUT`（只取预计高度用到的几项）
    pub mod ocr {
        /// 头部缩略图的高度上限
        pub const THUMB: f64 = 72.0;
        /// 头部除缩略图外的高度
        pub const HEAD_EXTRA: f64 = 12.0;
        /// 直接翻译的语言条
        pub const SUBHEAD: f64 = 40.0;
        pub const FOOTER: f64 = 40.0;
        /// 卡片上下内边距
        pub const PAD: f64 = 24.0;
    }
}

/// 译文显示宽度与原文之比。一个汉字译成英文约 3.5 个字母：中日韩原文译成拉丁文字时显示宽度单位（汉字计 2）约 ×1.8，
/// 反过来约 ×0.6；同一种文字体系、目标语言不明（`other`）、润色 / 校对这类不换语言的任务为 1。原文的文字体系按开头
/// 至多 400 个字符里宽字符与其他可见字符的显示宽度谁多来判断
pub fn translation_ratio(source: &str, target: Lang, task: TaskKind) -> f64 {
    const CJK_TO_LATIN: f64 = 1.8;
    const LATIN_TO_CJK: f64 = 0.6;
    if task != TaskKind::Translate {
        return 1.0;
    }
    let target_wide = match target {
        Lang::ZhCn | Lang::Ja | Lang::Ko => true,
        Lang::En => false,
        Lang::Other => return 1.0,
    };
    let (mut wide, mut narrow) = (0usize, 0usize);
    for c in source.chars().filter(|c| !c.is_whitespace()).take(400) {
        if is_wide(c) {
            wide += 2;
        } else {
            narrow += 1;
        }
    }
    if wide + narrow == 0 {
        return 1.0;
    }
    match (wide >= narrow, target_wide) {
        (true, false) => CJK_TO_LATIN,
        (false, true) => LATIN_TO_CJK,
        _ => 1.0,
    }
}

/// [`translate_expected_height`] 的输入
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TranslateEstimate<'a> {
    pub source: &'a str,
    /// 卡片宽度
    pub width: f64,
    /// 正文字号（`appearance.font_size`）
    pub font_size: f64,
    /// 译文显示宽度与原文之比（[`translation_ratio`]）
    pub ratio: f64,
    /// 默认显示原文对照（`translate.show_source_by_default`）
    pub show_source: bool,
    pub dict_mode: bool,
    /// 本次会话的高度上限（原文对照最多占视图的 40%）
    pub cap: f64,
}

/// 翻译视图的预计卡片高度：`96 + Σ 段落 ceil(段落显示单位 × 比例 / 每行单位) × 行高`，每行单位 = (宽 − 32) / (字号 / 2)；
/// 默认显示原文时加上原文对照（13 px，最多占视图的 40%）；词典式固定 280。估计到 [`expected_layout::STOP`] 就停。
/// 只用来选侧：译文的真实长度要到流式输出时才知道，估计偏差只影响卡片放在哪一侧，高度仍跟随真实内容
pub fn translate_expected_height(e: &TranslateEstimate<'_>) -> f64 {
    use expected_layout as l;
    if e.dict_mode {
        return l::DICT;
    }
    let font = body_font(e.font_size);
    let lh = font * l::LINE_HEIGHT;
    let inner = (e.width - CONTENT_INSET).max(1.0);
    let per_line = (inner / (font / 2.0)).max(1.0);
    let per_source_line = (inner / (l::SOURCE_FONT / 2.0)).max(1.0);
    let ratio = if e.ratio.is_finite() && e.ratio > 0.0 {
        e.ratio
    } else {
        1.0
    };
    let mut body = 0.0;
    let mut source_lines = 0.0;
    for para in e.source.split('\n') {
        // 这一段最多数到让正文到达 STOP 所需的单位（超长的一段不整段扫描）
        let budget = ((l::STOP - body) / lh).ceil().max(1.0) * per_line / ratio;
        let units = display_units(para, budget.ceil() as usize + 1) as f64;
        // 空行（段落之间的空白）按一行计
        body += (units * ratio / per_line).ceil().max(1.0) * lh;
        source_lines += (units / per_source_line).ceil().max(1.0);
        if body >= l::STOP {
            break;
        }
    }
    let source = if e.show_source {
        let view = (e.cap - l::CARD_PAD).max(0.0);
        (source_lines * l::SOURCE_LINE).min((view * l::SOURCE_SHARE - l::SOURCE_CHROME).max(0.0)) + l::SOURCE_CHROME
    } else {
        0.0
    };
    (l::TRANSLATE_CHROME + body + source).min(l::STOP)
}

/// OCR 视图的预计卡片高度：头部 + 底栏 + 内边距（前端 `LAYOUT`）+ 源行数 × 折行数 × 行高。源行数 = 框选高 / 18；
/// 折行数按与 [`ocr_width`] 同一口径（框选里的一行按设置字号重排后需要的宽度 / 卡片内容宽）。图片多、字少的截图会被
/// 高估——只影响选侧，可以接受
pub fn ocr_expected_height(capture: Rect, width: f64, font_size: f64, direct: bool) -> f64 {
    use expected_layout as l;
    let font = body_font(font_size);
    let chrome =
        l::ocr::PAD + l::ocr::THUMB + l::ocr::HEAD_EXTRA + l::ocr::FOOTER + if direct { l::ocr::SUBHEAD } else { 0.0 };
    if !(capture.width.is_finite() && capture.height.is_finite()) {
        return chrome;
    }
    let lines = (capture.height / l::CAPTURE_LINE).round().max(1.0);
    let needed = capture.width.max(0.0) * font / BODY_FONT;
    let inner = (width - CONTENT_INSET).max(1.0);
    // 留一点余量：框选刚好等于卡片内容宽时不算折行（浮点误差）
    let wraps = (needed / inner - 1e-6).ceil().max(1.0);
    (chrome + lines * wraps * font * l::LINE_HEIGHT).min(l::STOP)
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
    /// 工作区的一个角（离边 [`CORNER_INSET`]）：连续粘贴的进度提示，远离用户正在填写的输入框
    Corner(ScreenCorner),
}

/// 屏幕角落（各平台用哪个角见 `native::NOTICE_CORNER`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenCorner {
    /// 右上：贴顶边向下长（macOS 菜单栏下、Linux 桌面的通知位置）
    TopRight,
    /// 右下：贴底边向上长（Windows 任务栏上方的通知位置）
    BottomRight,
}

/// [`place_panel_with`] 的输入
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlaceOpts {
    /// 卡片宽度
    pub width: f64,
    /// 初始估计高度：ready 之前窗口按它显示，也只有它决定要不要压住选区
    pub initial: f64,
    /// 预计内容高度（翻译按原文长度、OCR 按框选尺寸估计）：**只用来选放在哪一侧**，不作为窗口高度。不超过
    /// `max(min(上限, 240), initial)` 时与 `initial` 完全等价
    pub expected: f64,
    /// 本次会话的高度上限
    pub cap: f64,
    /// 允许放到锚点左右两侧：锚点是真实的选区 / 框选矩形时才允许（光标锚点、锚点所在显示器断开后退回的光标锚点
    /// 不允许——那时卡片放在侧面并不挨着用户在看的东西）
    pub side_ok: bool,
    /// 侧放时按 `width` 两侧都放不下，可以退回的较窄宽度（OCR 随框选变宽时退回默认的 440）
    pub narrow_width: Option<f64>,
}

impl PlaceOpts {
    /// 只按初始高度放置（预计高度 = 初始高度、不侧放）
    pub fn new(width: f64, initial: f64, cap: f64) -> Self {
        Self {
            width,
            initial,
            expected: initial,
            cap,
            side_ok: false,
            narrow_width: None,
        }
    }
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
    /// 侧放时卡片在锚点的哪一侧（前端据此让进入动效水平移向锚点）；竖直放置为 None
    pub side: Option<PanelSide>,
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
            side: self.side,
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
/// - `At`：记住的左上角，夹回工作区，向下生长；
/// - `Corner`：工作区的右上角（向下长）或右下角（底边固定、向上长），离边 [`CORNER_INSET`]，变换原点在右缘。
///
/// 返回的 `max_height` 已扣除所选一侧的可用空间，卡片怎么长都留在工作区内。等价于预计高度 = 初始高度、不侧放的
/// [`place_panel_with`]
pub fn place_panel(screen: &Screen, anchor: PlaceAnchor, width: f64, initial_height: f64, cap: f64) -> PanelGeometry {
    place_panel_with(screen, anchor, PlaceOpts::new(width, initial_height, cap))
}

/// 锚点旁边的一个落点（本屏逻辑坐标）
#[derive(Debug, Clone, Copy, PartialEq)]
struct Spot {
    x: f64,
    /// 固定边的 y（Below 为顶边，Above 为底边）
    edge_y: f64,
    placement: Placement,
    /// 固定边到工作区边缘（留 [`EDGE_INSET`]）的可用高度
    avail: f64,
    /// 锚点的 x（变换原点）
    origin: f64,
    width: f64,
    /// 侧放：在锚点的哪一侧
    side: Option<PanelSide>,
}

/// 按锚点放置面板，`Rect` 锚点额外考虑预计内容高度（[`PlaceOpts::expected`]）。先按 [`place_panel`] 的规则（只看初始
/// 高度）得到竖直落点；记 `wish = clamp(预计高度, want, 上限)`，只有 `wish > want`（长译文 / 长识别结果）时才可能换落点，
/// 每一侧的「能用的高度」按 `min(该侧空间, wish)` 计：
/// 1. 竖直落点是选区下方、而上方比下方至少多出 [`SIDE_GAIN_MIN`]：翻到上方（底边固定）；
/// 2. `side_ok` 时，锚点右侧（放不下再试左侧，仍放不下再按 `narrow_width` 试一次）比上一步选定的落点至少多出
///    [`SIDE_GAIN_MIN`]：放到侧面——顶边与锚点顶边对齐（锚点太靠下时上移到放得下 `wish`），向下长，不盖住锚点。
///
/// 预计高度不会让卡片压住选区（是否压住只由初始高度决定）；`wish ≤ want` 时结果与 [`place_panel`] 逐字段相同
pub fn place_panel_with(screen: &Screen, anchor: PlaceAnchor, opts: PlaceOpts) -> PanelGeometry {
    let work = screen.work;
    let cap = opts.cap;
    let width = fit_width(opts.width, work);
    let initial = opts.initial.max(MIN_CARD_HEIGHT);
    let min_x = work.x + EDGE_INSET;
    let max_x = (work.right() - EDGE_INSET - width).max(min_x);
    let clamp_x = |x: f64| x.clamp(min_x, max_x).round();
    let top_limit = work.y + EDGE_INSET;
    let bottom_limit = work.bottom() - EDGE_INSET;
    let spot = |x: f64, edge_y: f64, placement: Placement, avail: f64, origin: f64| Spot {
        x,
        edge_y,
        placement,
        avail,
        origin,
        width,
        side: None,
    };

    let chosen = match anchor {
        PlaceAnchor::Rect(a) => {
            let a = clamp_anchor(a, work);
            let below_top = a.bottom() + ANCHOR_GAP;
            let above_bottom = a.y - ANCHOR_GAP;
            let space_below = (bottom_limit - below_top).max(0.0);
            let space_above = (above_bottom - top_limit).max(0.0);
            let fit = initial.min(cap);
            let want = cap.min(PREFER_BELOW_MIN).max(fit);
            let x = clamp_x(a.x);
            let below = spot(x, below_top.round(), Placement::Below, space_below, a.x);
            let above = spot(x, above_bottom.round(), Placement::Above, space_above, a.x);
            let prefer_below = space_below >= want || (space_below >= fit && space_below >= space_above);
            let vertical = if prefer_below {
                below
            } else if space_above >= fit {
                above
            } else if space_below >= space_above {
                // 压住选区（下侧）：顶边上移到「工作区底 − want」，仍不高于工作区顶
                let top = below_top.min(bottom_limit - want).max(top_limit).round();
                spot(x, top, Placement::Below, (bottom_limit - top).max(0.0), a.x)
            } else {
                // 压住选区（上侧）：底边下移到「工作区顶 + want」，仍不低于工作区底
                let bottom = above_bottom.max(top_limit + want).min(bottom_limit).round();
                spot(x, bottom, Placement::Above, (bottom - top_limit).max(0.0), a.x)
            };
            // NaN 的预计高度按 want（max 会丢掉 NaN）
            let wish = opts.expected.max(want).min(cap);
            if wish <= want {
                vertical
            } else {
                let room = |s: &Spot| s.avail.min(wish);
                let mut best = if prefer_below && room(&above) - room(&below) >= SIDE_GAIN_MIN {
                    above
                } else {
                    vertical
                };
                if opts.side_ok {
                    let widths = std::iter::once(width).chain(opts.narrow_width.map(|w| fit_width(w, work)));
                    if let Some(side) = widths.filter_map(|w| side_spot(work, a, w, wish)).next()
                        && room(&side) - room(&best) >= SIDE_GAIN_MIN
                    {
                        best = side;
                    }
                }
                best
            }
        }
        PlaceAnchor::Center => {
            let x = clamp_x(work.x + (work.width - width) / 2.0);
            let center_y = work.y + work.height / 3.0;
            let top = (center_y - initial.min(cap) / 2.0).clamp(top_limit, (bottom_limit - initial).max(top_limit));
            spot(
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
            spot(x, top.round(), Placement::Below, (bottom_limit - top).max(0.0), x)
        }
        PlaceAnchor::Corner(corner) => {
            let x = clamp_x(work.right() - CORNER_INSET - width);
            match corner {
                ScreenCorner::TopRight => {
                    let top = (work.y + CORNER_INSET).min(bottom_limit).max(top_limit).round();
                    spot(x, top, Placement::Below, (bottom_limit - top).max(0.0), x + width)
                }
                ScreenCorner::BottomRight => {
                    let bottom = (work.bottom() - CORNER_INSET).max(top_limit).min(bottom_limit).round();
                    spot(x, bottom, Placement::Above, (bottom - top_limit).max(0.0), x + width)
                }
            }
        }
    };
    let max_height = cap.min(chosen.avail).max(MIN_CARD_HEIGHT).floor();
    PanelGeometry {
        screen: screen.clone(),
        x: chosen.x,
        edge_y: chosen.edge_y,
        placement: chosen.placement,
        width: chosen.width,
        max_height,
        cap,
        origin_x: (chosen.origin - chosen.x).clamp(0.0, chosen.width),
        side: chosen.side,
    }
}

/// 卡片宽度夹到工作区内（两侧各留 [`EDGE_INSET`]），取整
fn fit_width(width: f64, work: Rect) -> f64 {
    width.min(work.width - 2.0 * EDGE_INSET).max(MIN_CARD_HEIGHT).round()
}

/// 锚点（已夹回工作区）右侧、放不下时左侧的侧放落点：与锚点隔 [`ANCHOR_GAP`]、整张卡片在工作区内（留 [`EDGE_INSET`]）。
/// 顶边与锚点顶边对齐，锚点太靠下时上移到放得下 `wish`（侧放不和锚点重叠，可以上移）。变换原点贴着锚点一侧的边。
/// 两侧都放不下这个宽度时为 None
fn side_spot(work: Rect, a: Rect, width: f64, wish: f64) -> Option<Spot> {
    let top_limit = work.y + EDGE_INSET;
    let bottom_limit = work.bottom() - EDGE_INSET;
    // 向远离锚点的方向取整：取整之后与锚点的间距仍不小于 ANCHOR_GAP
    let right = (a.right() + ANCHOR_GAP).ceil();
    let left = (a.x - ANCHOR_GAP - width).floor();
    let (x, origin, side) = if right + width <= work.right() - EDGE_INSET {
        (right, right, PanelSide::Right)
    } else if left >= work.x + EDGE_INSET {
        (left, left + width, PanelSide::Left)
    } else {
        return None;
    };
    let top = a.y.min(bottom_limit - wish).max(top_limit).round();
    Some(Spot {
        x,
        edge_y: top,
        placement: Placement::Below,
        avail: (bottom_limit - top).max(0.0),
        origin,
        width,
        side: Some(side),
    })
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

    // ---- 角落提示（连续粘贴的进度）

    #[test]
    fn corner_top_right_hugs_the_work_area() {
        let s = mac_screen();
        let g = place_panel(
            &s,
            PlaceAnchor::Corner(ScreenCorner::TopRight),
            260.0,
            40.0,
            NOTICE_MAX_HEIGHT,
        );
        assert_eq!(g.placement, Placement::Below);
        // 菜单栏下 16 px、右缘 16 px，向下长
        assert_eq!(g.edge_y, s.work.y + CORNER_INSET);
        assert_eq!(g.x + g.width, s.work.right() - CORNER_INSET);
        assert_eq!(g.card_rect(40.0).y, g.card_rect(72.0).y, "顶边固定");
        assert_eq!(g.max_height, NOTICE_MAX_HEIGHT);
        // 变换原点在右缘（进入动画从角落长出来）
        assert_eq!(g.origin_x, g.width);
        assert_inside_work(&s, &g, "右上角");
    }

    #[test]
    fn corner_bottom_right_grows_upward() {
        let s = mac_screen();
        let g = place_panel(
            &s,
            PlaceAnchor::Corner(ScreenCorner::BottomRight),
            288.0,
            40.0,
            NOTICE_MAX_HEIGHT,
        );
        assert_eq!(g.placement, Placement::Above);
        assert_eq!(g.edge_y, s.work.bottom() - CORNER_INSET);
        assert_eq!(g.x + g.width, s.work.right() - CORNER_INSET);
        // 底边固定：第一条两行、之后一行，都贴着同一条底边
        assert_eq!(g.card_rect(56.0).bottom(), g.card_rect(38.0).bottom());
        assert_eq!(g.max_height, NOTICE_MAX_HEIGHT);
        assert_eq!(g.origin_x, g.width);
        assert_inside_work(&s, &g, "右下角");
    }

    #[test]
    fn corner_on_a_secondary_screen_and_a_narrow_work_area() {
        // 副屏（1.5×）：本屏逻辑坐标里算，换成物理像素后仍在这块屏里、尺寸按缩放换算
        let s = win_secondary();
        let g = place_panel(
            &s,
            PlaceAnchor::Corner(ScreenCorner::BottomRight),
            288.0,
            40.0,
            NOTICE_MAX_HEIGHT,
        );
        let card = g.card_rect(56.0);
        assert_eq!(card.right(), s.work.right() - CORNER_INSET);
        assert_eq!(card.bottom(), s.work.bottom() - CORNER_INSET);
        let desk = s.to_desktop(card, DesktopUnit::Physical);
        assert_eq!((desk.width, desk.height), (432.0, 84.0));
        let frame = s.desktop_frame(DesktopUnit::Physical);
        assert!(
            desk.x >= frame.x && desk.right() <= frame.right(),
            "{desk:?} 在 {frame:?} 里"
        );
        assert!(
            desk.y >= frame.y && desk.bottom() <= frame.bottom(),
            "{desk:?} 在 {frame:?} 里"
        );
        // 极窄的工作区：卡片夹到工作区内，不越过左边
        let mut narrow = mac_screen();
        narrow.work = Rect::new(0.0, 25.0, 200.0, 805.0);
        let g = place_panel(
            &narrow,
            PlaceAnchor::Corner(ScreenCorner::TopRight),
            260.0,
            40.0,
            NOTICE_MAX_HEIGHT,
        );
        assert_inside_work(&narrow, &g, "窄工作区");
    }

    // ---- 按预计内容高度选侧、侧放、OCR 宽度

    /// 改动之前的 `place_panel`（`Rect` 锚点）的逐行副本：短内容的等价性测试拿它作对照
    fn legacy_rect(screen: &Screen, a: Rect, width: f64, initial: f64, cap: f64) -> PanelGeometry {
        let work = screen.work;
        let width = width.min(work.width - 2.0 * EDGE_INSET).max(MIN_CARD_HEIGHT).round();
        let initial = initial.max(MIN_CARD_HEIGHT);
        let min_x = work.x + EDGE_INSET;
        let max_x = (work.right() - EDGE_INSET - width).max(min_x);
        let clamp_x = |x: f64| x.clamp(min_x, max_x).round();
        let top_limit = work.y + EDGE_INSET;
        let bottom_limit = work.bottom() - EDGE_INSET;
        let a = clamp_anchor(a, work);
        let below_top = a.bottom() + ANCHOR_GAP;
        let above_bottom = a.y - ANCHOR_GAP;
        let space_below = (bottom_limit - below_top).max(0.0);
        let space_above = (above_bottom - top_limit).max(0.0);
        let fit = initial.min(cap);
        let want = cap.min(PREFER_BELOW_MIN).max(fit);
        let x = clamp_x(a.x);
        let (edge_y, placement, avail) = if space_below >= want || (space_below >= fit && space_below >= space_above) {
            (below_top.round(), Placement::Below, space_below)
        } else if space_above >= fit {
            (above_bottom.round(), Placement::Above, space_above)
        } else if space_below >= space_above {
            let top = below_top.min(bottom_limit - want).max(top_limit).round();
            (top, Placement::Below, (bottom_limit - top).max(0.0))
        } else {
            let bottom = above_bottom.max(top_limit + want).min(bottom_limit).round();
            (bottom, Placement::Above, (bottom - top_limit).max(0.0))
        };
        PanelGeometry {
            screen: screen.clone(),
            x,
            edge_y,
            placement,
            width,
            max_height: cap.min(avail).max(MIN_CARD_HEIGHT).floor(),
            cap,
            origin_x: (a.x - x).clamp(0.0, width),
            side: None,
        }
    }

    /// 几种常见屏幕的工作区（本屏逻辑坐标）：Mac 1440×900 Dock 在底部、Mac 1512×982 Dock 隐藏、Windows 1080p@150%
    /// （逻辑 1280×720、任务栏 48）、Windows 1440p@150% 副屏
    fn screens() -> Vec<Screen> {
        let mut mac_hidden_dock = mac_screen();
        mac_hidden_dock.frame = Rect::new(0.0, 0.0, 1512.0, 982.0);
        mac_hidden_dock.work = Rect::new(0.0, 41.0, 1512.0, 941.0);
        vec![mac_screen(), mac_hidden_dock, win_small(), win_secondary()]
    }

    /// Windows 1080p@150%：逻辑 1280×720，任务栏 48
    fn win_small() -> Screen {
        let mut s = win_secondary();
        s.frame = Rect::new(0.0, 0.0, 1280.0, 720.0);
        s.work = Rect::new(0.0, 0.0, 1280.0, 672.0);
        s.physical_origin = PhysPoint { x: 0, y: 0 };
        s
    }

    fn intersects(a: Rect, b: Rect) -> bool {
        a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
    }

    #[test]
    fn short_content_is_unchanged() {
        // 预计高度不超过 want（短内容）时，无论是否允许侧放，结果与改动前逐字段相同
        for s in screens() {
            let cap = height_cap(&s, 0.6);
            for pct in (0..=100).step_by(5) {
                let top = s.work.y + s.work.height * f64::from(pct) / 100.0;
                for height in [0.0, 18.0, 200.0, 500.0, 900.0] {
                    for left in [-40.0, 0.0, 300.0, s.work.right() - 60.0] {
                        let a = Rect::new(left, top, 120.0, height);
                        for initial in [initial_height::TOAST, initial_height::TRANSLATE, initial_height::OCR] {
                            let want = cap.min(PREFER_BELOW_MIN).max(initial.min(cap));
                            for expected in [initial, want, f64::NAN] {
                                for side_ok in [false, true] {
                                    let opts = PlaceOpts {
                                        width: 420.0,
                                        initial,
                                        expected,
                                        cap,
                                        side_ok,
                                        narrow_width: Some(OCR_WIDTH),
                                    };
                                    assert_eq!(
                                        place_panel_with(&s, PlaceAnchor::Rect(a), opts),
                                        legacy_rect(&s, a, 420.0, initial, cap),
                                        "{} {a:?} initial={initial} expected={expected} side_ok={side_ok}",
                                        s.id
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn long(width: f64, cap: f64) -> PlaceOpts {
        PlaceOpts {
            expected: 9999.0,
            ..PlaceOpts::new(width, initial_height::TRANSLATE, cap)
        }
    }

    #[test]
    fn long_content_prefers_the_roomier_side() {
        let s = mac_screen();
        let cap = height_cap(&s, 0.6);
        // 一行选区在 y = 470：下方 322（够放下短内容）、上方 425
        let a = Rect::new(300.0, 470.0, 200.0, 18.0);
        assert_eq!(
            place_panel(&s, PlaceAnchor::Rect(a), 420.0, 120.0, cap).placement,
            Placement::Below
        );
        let g = place_panel_with(&s, PlaceAnchor::Rect(a), long(420.0, cap));
        assert_eq!(g.placement, Placement::Above);
        assert_eq!(g.edge_y, a.y - ANCHOR_GAP);
        assert_eq!(g.max_height, 425.0);
        assert_eq!(g.x, a.x, "仍与选区左对齐");
        // 上方只多 78 px（< 96）：保持下方
        let near = PlaceOpts {
            expected: 400.0,
            ..long(420.0, cap)
        };
        let g = place_panel_with(&s, PlaceAnchor::Rect(a), near);
        assert_eq!(g, place_panel(&s, PlaceAnchor::Rect(a), 420.0, 120.0, cap));
        // 下方本来就放得下预计高度：不动
        let high = Rect::new(300.0, 120.0, 200.0, 18.0);
        assert_eq!(
            place_panel_with(&s, PlaceAnchor::Rect(high), long(420.0, cap)),
            place_panel(&s, PlaceAnchor::Rect(high), 420.0, 120.0, cap)
        );
    }

    #[test]
    fn side_placement_when_vertical_room_is_short() {
        let s = mac_screen();
        let cap = height_cap(&s, 0.6);
        // 520×260 的框选在屏幕中部：上下都只有约 250
        let a = Rect::new(200.0, 300.0, 520.0, 260.0);
        let opts = PlaceOpts {
            side_ok: true,
            narrow_width: Some(OCR_WIDTH),
            ..long(552.0, cap)
        };
        let g = place_panel_with(&s, PlaceAnchor::Rect(a), opts);
        assert_eq!(g.placement, Placement::Below);
        assert_eq!(g.x, a.right() + ANCHOR_GAP, "右侧，与框选隔 12");
        assert_eq!(g.edge_y, a.y, "顶边与框选对齐");
        assert_eq!(g.max_height, cap);
        assert_eq!(g.width, 552.0);
        assert_eq!(g.origin_x, 0.0, "变换原点贴着框选一侧");
        assert_eq!(g.side, Some(PanelSide::Right), "进入动效从右边移向框选");
        assert!(!intersects(g.card_rect(g.max_height), a));
        assert_inside_work(&s, &g, "右侧");
        // 原地替换保留位置，也保留「在哪一侧」
        assert_eq!(g.keep_position(440.0, cap).side, Some(PanelSide::Right));
        // 不允许侧放（光标锚点）：仍在下方
        let below = place_panel_with(&s, PlaceAnchor::Rect(a), PlaceOpts { side_ok: false, ..opts });
        assert_eq!(below.placement, Placement::Below);
        assert_eq!(below.edge_y, a.bottom() + ANCHOR_GAP);
        assert_eq!(below.side, None);
        // 右侧放不下：左侧，变换原点在卡片右缘
        let right_side = Rect::new(800.0, 300.0, 520.0, 260.0);
        let g = place_panel_with(&s, PlaceAnchor::Rect(right_side), opts);
        assert_eq!(g.x + g.width, right_side.x - ANCHOR_GAP);
        assert_eq!(g.origin_x, g.width);
        assert_eq!(g.side, Some(PanelSide::Left));
        assert!(!intersects(g.card_rect(g.max_height), right_side));
        assert_inside_work(&s, &g, "左侧");
        // 框选又高又靠下（下方没有空间、上方 355）：侧放，顶边上移到放得下上限，仍不与框选重叠
        let low = Rect::new(200.0, 400.0, 520.0, 410.0);
        let g = place_panel_with(&s, PlaceAnchor::Rect(low), opts);
        assert_eq!(g.x, low.right() + ANCHOR_GAP);
        assert_eq!(g.max_height, cap);
        assert!(g.edge_y < low.y);
        assert!(!intersects(g.card_rect(g.max_height), low));
        assert_inside_work(&s, &g, "上移");
        // 两侧都放不下（宽框选）：竖直逻辑
        let wide = Rect::new(400.0, 300.0, 640.0, 260.0);
        let g = place_panel_with(&s, PlaceAnchor::Rect(wide), PlaceOpts { width: 600.0, ..opts });
        assert_eq!(g.x, wide.x);
        assert_eq!(g.edge_y, wide.bottom() + ANCHOR_GAP);
        assert_eq!(g.side, None);
    }

    #[test]
    fn side_placement_needs_a_real_gain() {
        let s = mac_screen();
        let cap = height_cap(&s, 0.6);
        // 下方 400、侧面 483：只多 83 px，保持下方
        let a = Rect::new(200.0, 150.0, 300.0, 260.0);
        let opts = PlaceOpts {
            side_ok: true,
            ..long(420.0, cap)
        };
        let g = place_panel_with(&s, PlaceAnchor::Rect(a), opts);
        assert_eq!(g.placement, Placement::Below);
        assert_eq!(g.edge_y, a.bottom() + ANCHOR_GAP);
        assert_eq!(g.max_height, 400.0);
    }

    #[test]
    fn side_placement_falls_back_to_narrow_width() {
        // Windows 1080p@150%（逻辑 1280×672 的工作区）：600 宽两侧都放不下，退回 440 放在右侧
        let s = win_small();
        let cap = height_cap(&s, 0.6);
        let a = Rect::new(300.0, 150.0, 520.0, 260.0);
        let opts = PlaceOpts {
            side_ok: true,
            narrow_width: Some(OCR_WIDTH),
            ..long(600.0, cap)
        };
        let g = place_panel_with(&s, PlaceAnchor::Rect(a), opts);
        assert_eq!((g.width, g.x, g.edge_y), (OCR_WIDTH, a.right() + ANCHOR_GAP, a.y));
        assert_eq!(g.max_height, cap);
        assert_inside_work(&s, &g, "退回 440");
        // 物理像素：宽 = 440 × 1.5，仍在这块屏里
        let desk = s.to_desktop(g.card_rect(g.max_height), DesktopUnit::Physical);
        assert_eq!(desk.width, 660.0);
        assert!(desk.right() <= s.desktop_frame(DesktopUnit::Physical).right());
        // 没有可退回的宽度：竖直逻辑（下方 242）
        let g = place_panel_with(
            &s,
            PlaceAnchor::Rect(a),
            PlaceOpts {
                narrow_width: None,
                ..opts
            },
        );
        assert_eq!(
            (g.width, g.placement, g.edge_y),
            (600.0, Placement::Below, a.bottom() + ANCHOR_GAP)
        );
    }

    #[test]
    fn expected_never_overlaps_the_anchor_and_stays_inside() {
        // 网格扫一遍：长内容的落点始终在工作区内；改动前不压住锚点的，现在也不压住；上限不会比改动前小
        for s in screens() {
            let cap = height_cap(&s, 0.6);
            for pct in (0..=100).step_by(4) {
                let top = s.work.y + s.work.height * f64::from(pct) / 100.0;
                for height in [0.0, 18.0, 120.0, 260.0, 500.0, 900.0] {
                    for (left, w) in [
                        (-30.0, 120.0),
                        (40.0, 300.0),
                        (300.0, 520.0),
                        (s.work.right() - 500.0, 480.0),
                    ] {
                        let a = Rect::new(left, top, w, height);
                        for expected in [300.0, 400.0, 9999.0] {
                            for width in [420.0, 600.0] {
                                let opts = PlaceOpts {
                                    expected,
                                    side_ok: true,
                                    narrow_width: Some(OCR_WIDTH),
                                    ..PlaceOpts::new(width, initial_height::OCR, cap)
                                };
                                let what = format!("{} {a:?} expected={expected} width={width}", s.id);
                                let g = place_panel_with(&s, PlaceAnchor::Rect(a), opts);
                                let old = legacy_rect(&s, a, width, initial_height::OCR, cap);
                                assert_inside_work(&s, &g, &what);
                                assert!(g.max_height >= old.max_height, "{what}: {g:?} vs {old:?}");
                                let clamped = clamp_anchor(a, s.work);
                                if !intersects(old.card_rect(old.max_height), clamped) {
                                    assert!(!intersects(g.card_rect(g.max_height), clamped), "{what}: {g:?}");
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn ocr_width_follows_capture() {
        assert_eq!(ocr_width(300.0, 14.0), OCR_WIDTH);
        assert_eq!(ocr_width(408.0, 14.0), OCR_WIDTH);
        assert_eq!(ocr_width(520.0, 14.0), 552.0);
        assert_eq!(ocr_width(900.0, 14.0), OCR_MAX_WIDTH);
        // 字号调大：同样的框选需要更宽的卡片
        assert_eq!(ocr_width(420.0, 18.0), 572.0);
        assert_eq!(ocr_width(f64::NAN, 14.0), OCR_WIDTH);
        assert_eq!(ocr_width(-5.0, 14.0), OCR_WIDTH);
        assert_eq!(ocr_width(520.0, f64::NAN), 552.0);
        // 窄工作区：place_panel 夹到工作区宽 − 16
        let mut s = mac_screen();
        s.work = Rect::new(0.0, 25.0, 590.0, 805.0);
        let g = place_panel(
            &s,
            PlaceAnchor::Rect(Rect::new(10.0, 100.0, 560.0, 40.0)),
            600.0,
            220.0,
            483.0,
        );
        assert_eq!(g.width, 590.0 - 2.0 * EDGE_INSET);
    }

    #[test]
    fn translation_ratio_follows_scripts_and_task() {
        let zh = "这是一段需要翻译的中文原文，用来估计译文的长度。";
        let en = "This is an English paragraph that needs translating.";
        assert_eq!(translation_ratio(zh, Lang::En, TaskKind::Translate), 1.8);
        assert_eq!(translation_ratio(en, Lang::ZhCn, TaskKind::Translate), 0.6);
        assert_eq!(translation_ratio(en, Lang::Ja, TaskKind::Translate), 0.6);
        assert_eq!(translation_ratio(zh, Lang::Ja, TaskKind::Translate), 1.0);
        assert_eq!(translation_ratio(en, Lang::En, TaskKind::Translate), 1.0);
        assert_eq!(translation_ratio(zh, Lang::Other, TaskKind::Translate), 1.0);
        // 润色 / 校对不换语言
        assert_eq!(translation_ratio(zh, Lang::En, TaskKind::Polish), 1.0);
        assert_eq!(translation_ratio("   ", Lang::En, TaskKind::Translate), 1.0);
        // 夹杂少量英文的中文仍算中文
        assert_eq!(
            translation_ratio("用 Rust 写的 Tauri 应用", Lang::En, TaskKind::Translate),
            1.8
        );
    }

    fn estimate(source: &str) -> TranslateEstimate<'_> {
        TranslateEstimate {
            source,
            width: 520.0,
            font_size: 14.0,
            ratio: 1.0,
            show_source: false,
            dict_mode: false,
            cap: 483.0,
        }
    }

    #[test]
    fn translate_expected_height_grows_with_content() {
        use expected_layout as l;
        let lh = 14.0 * l::LINE_HEIGHT;
        // 520 宽：每行 (520 − 32) / 7 ≈ 69.7 个单位
        assert_eq!(translate_expected_height(&estimate("hello")), l::TRANSLATE_CHROME + lh);
        let one = "a".repeat(140);
        assert_eq!(
            translate_expected_height(&estimate(&one)),
            l::TRANSLATE_CHROME + 3.0 * lh
        );
        // 多段：每段各自折行，空行算一行
        let paras = format!("{one}\n\n{one}");
        assert_eq!(
            translate_expected_height(&estimate(&paras)),
            l::TRANSLATE_CHROME + 7.0 * lh
        );
        // 比例：拉丁 → 中文约 0.6
        let shorter = TranslateEstimate {
            ratio: 0.6,
            ..estimate(&one)
        };
        assert_eq!(translate_expected_height(&shorter), l::TRANSLATE_CHROME + 2.0 * lh);
        // 词典式固定
        let dict = TranslateEstimate {
            dict_mode: true,
            ..estimate(&one)
        };
        assert_eq!(translate_expected_height(&dict), l::DICT);
        // 默认显示原文：加上原文对照（13 px）
        let with_source = TranslateEstimate {
            show_source: true,
            ..estimate(&one)
        };
        let src_lines = (140.0f64 / ((520.0 - 32.0) / 6.5)).ceil();
        assert_eq!(
            translate_expected_height(&with_source),
            l::TRANSLATE_CHROME + 3.0 * lh + src_lines * l::SOURCE_LINE + l::SOURCE_CHROME
        );
        // 原文对照最多占视图的 40%
        let long_source = "a".repeat(1400);
        let huge_source = TranslateEstimate {
            show_source: true,
            ..estimate(&long_source)
        };
        let source_max = (483.0 - l::CARD_PAD) * l::SOURCE_SHARE;
        assert_eq!(
            translate_expected_height(&huge_source),
            l::TRANSLATE_CHROME + 21.0 * lh + source_max
        );
        // 超长文本：到 STOP 就停，不整篇扫描
        let big = "字".repeat(100_000);
        let started = std::time::Instant::now();
        assert_eq!(translate_expected_height(&estimate(&big)), l::STOP);
        let lines = "line\n".repeat(100_000);
        assert_eq!(translate_expected_height(&estimate(&lines)), l::STOP);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(50),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn ocr_expected_height_follows_the_capture() {
        use expected_layout as l;
        let chrome = l::ocr::PAD + l::ocr::THUMB + l::ocr::HEAD_EXTRA + l::ocr::FOOTER;
        let lh = 14.0 * l::LINE_HEIGHT;
        // 一行字的框选：短内容（不超过 240，放下方）
        let line = ocr_expected_height(Rect::new(0.0, 0.0, 300.0, 18.0), OCR_WIDTH, 14.0, false);
        assert_eq!(line, chrome + lh);
        assert!(line < PREFER_BELOW_MIN);
        // 卡片随框选变宽时不折行：520×260 ≈ 14 行
        let w = ocr_width(520.0, 14.0);
        assert_eq!(
            ocr_expected_height(Rect::new(0.0, 0.0, 520.0, 260.0), w, 14.0, false),
            chrome + 14.0 * lh
        );
        // 宽于 600 的框选：每行折成两行
        let wide = ocr_expected_height(Rect::new(0.0, 0.0, 900.0, 180.0), ocr_width(900.0, 14.0), 14.0, false);
        assert_eq!(wide, chrome + 10.0 * 2.0 * lh);
        // 直接翻译多一条语言条；大框选封顶
        assert_eq!(
            ocr_expected_height(Rect::new(0.0, 0.0, 300.0, 18.0), OCR_WIDTH, 14.0, true),
            chrome + l::ocr::SUBHEAD + lh
        );
        assert_eq!(
            ocr_expected_height(Rect::new(0.0, 0.0, 600.0, 4000.0), 600.0, 14.0, false),
            l::STOP
        );
        assert_eq!(
            ocr_expected_height(Rect::new(0.0, 0.0, f64::NAN, 18.0), 600.0, 14.0, false),
            chrome
        );
    }

    /// OCR 的版式常量直接对照前端 model.ts 的 LAYOUT（唯一来源）：前端改了缩略图、底栏或内边距而这里没跟上时失败
    #[test]
    fn ocr_layout_matches_frontend() {
        use expected_layout as l;
        let model = include_str!("../../../src/views/ocr/model.ts");
        let compact: String = model.chars().filter(|c| !c.is_whitespace()).collect();
        let start = compact.find("exportconstLAYOUT={").expect("model.ts 里找不到 LAYOUT");
        let block = &compact[start..start + compact[start..].find("}asconst").expect("LAYOUT 的结尾")];
        for decl in [
            format!("thumb:{},", l::ocr::THUMB),
            format!("headExtra:{},", l::ocr::HEAD_EXTRA),
            format!("subhead:{},", l::ocr::SUBHEAD),
            format!("footer:{},", l::ocr::FOOTER),
            format!("pad:{},", l::ocr::PAD),
        ] {
            assert!(block.contains(&decl), "LAYOUT 里找不到 `{decl}`");
        }
        let line = format!(
            "exportconstlineHeight=(fontSize:number):number=>fontSize*{}",
            l::LINE_HEIGHT
        );
        assert!(compact.contains(&line), "model.ts 里找不到 `{line}`");
        assert_eq!(l::ocr::PAD, l::CARD_PAD);
    }
}
