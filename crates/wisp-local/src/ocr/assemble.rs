//! 把带框的文本行拼成纯文本（本地 PP-OCR 与云端 PP-OCRv6 共用）。
//!
//! - 按框的竖向重叠把行分到「视觉行」，行内按 x 排序；同一视觉行内相邻片段按间距决定是否插空格（CJK 之间不加）。
//!   间距超过约两个字高的片段视为不同的栏（表格的单元格、并排的面板）：lines 模式用制表符隔开，paragraphs 模式各自成段。
//! - lines：每个视觉行一行，保留相对缩进（左边界与主体左边界之差折算成空格，最多 8 个）；行间有明显空白时空一行。
//! - paragraphs：相邻视觉行在行距 ≤ 1.5 × 行高、左边界接近、上一行接近右边界时合并为一段，跨行处按
//!   [`wisp_text::join::join_break`] 相连（断词还原、东亚文字直接相连、中英之间按全文惯例）；列表项另起一段。
//!   段与段之间空一行；连续的列表项、连续的短行（一行就结束、明显比所在这一栏窄：界面标签、按钮、菜单项、表格的
//!   单元格）之间只换行，一组短行与相邻的正文之间仍空一行（与视觉模型的 paragraphs 模式相同，判定见 `separator`）。
//! - 置信度低于 [`MIN_SCORE`] 的行丢弃。没有文字时输出 [`NO_TEXT`]（编排器的无文字判定照常生效）。
//!
//! 这里只管几何（分行、分栏、缩进、段落边界）；只看文字本身的规则（是否加空格、断词还原、列表项）在 [`wisp_text::join`]。

use wisp_text::join::{MixedSpacing, join_break, needs_space, starts_list_item};
use wisp_types::OcrMode;

use super::TextLine;

/// 低于这个置信度的行丢弃。官方产线的 `text_rec_score_thresh` 是 0（不过滤），但截图里的图标、水印碎片常被识别成
/// 0.3–0.5 的乱码；0.5 与 RapidOCR 的默认值一致，正常文字几乎都在 0.8 以上
pub const MIN_SCORE: f32 = 0.5;
/// 没有文字时的输出（与视觉 LLM 的约定相同）
pub const NO_TEXT: &str = "<NO_TEXT>";
/// 缩进最多折算的空格数
pub const MAX_INDENT: usize = 8;

/// 同一视觉行：两个框的竖向重叠至少占较矮者高度的这个比例
const ROW_OVERLAP: f32 = 0.5;
/// 同一视觉行内，片段间距超过这个倍数的行高即视为另一栏
const COLUMN_GAP: f32 = 2.0;
/// lines 模式：行间空白超过这个倍数的行高时插入空行
const BLANK_LINE_GAP: f32 = 1.0;
/// paragraphs 模式：相邻两行中心的竖向距离不超过这个倍数的行高才可能同段
const PARAGRAPH_PITCH: f32 = 1.5;
/// paragraphs 模式：左边界相差不超过这个倍数的行高才可能同段
const PARAGRAPH_LEFT: f32 = 1.2;
/// paragraphs 模式：上一行的右边界离段落右边界不超过这个倍数的行高，才算「写满了一行」
const PARAGRAPH_RIGHT: f32 = 2.5;
/// paragraphs 模式：首行缩进最多这么多倍的行高（第二行比首行靠左时仍同段）
const FIRST_LINE_INDENT: f32 = 4.0;
/// paragraphs 模式：上一行至少这么多倍的行高宽，才可能是折行（竖排的短标签、菜单项各自成段）
const PARAGRAPH_MIN_WIDTH: f32 = 8.0;
/// paragraphs 模式：估计一栏的右边界时，只看上下这么多倍行高以内、左边界相近的行
const COLUMN_WINDOW: f32 = 15.0;
/// paragraphs 模式：一行就结束、宽度不超过所在这一栏行长这个比例的段算「短行」（界面标签、按钮、菜单项、表格的
/// 单元格、聊天里的短消息）。写到六成以上的单行段多半是恰好一行写完的正文，照常与相邻的段空一行。栏的行长见
/// [`column_measure`]
const SHORT_PARAGRAPH: f32 = 0.6;
/// paragraphs 模式：折了行的段至少这么多倍的行高宽，才拿它的宽度当这一栏的行长（约 40 个字母或 20 个汉字）。界面截图
/// 里上下紧挨着的两行控件也会被拼成「折了行的段」（机票上的两行字段、窄面板里的两行指标），在真实截图的样例里只有
/// 9–15 个行高宽；拿它们当行长，同一组的短行会被当成正文、彼此空行。常见的双栏正文一栏都比这宽
const MIN_COLUMN_MEASURE: f32 = 16.0;

/// 按模式拼装（markdown 与 translate 退回 paragraphs：PP-OCR 不识别版面）
pub fn assemble(lines: &[TextLine], mode: OcrMode) -> String {
    match mode {
        OcrMode::Lines => to_lines(lines),
        OcrMode::Paragraphs | OcrMode::Markdown | OcrMode::Translate => to_paragraphs(lines),
    }
}

/// lines 模式
pub fn to_lines(lines: &[TextLine]) -> String {
    let rows = rows(lines);
    if rows.is_empty() {
        return NO_TEXT.to_owned();
    }
    let h = median(rows.iter().map(Row::height));
    let unit = char_unit(&rows, h);
    let body_left = body_left(&rows, h);
    let mut out = String::new();
    let mut prev_bottom: Option<f32> = None;
    for row in &rows {
        if let Some(bottom) = prev_bottom {
            out.push('\n');
            if row.top() - bottom > BLANK_LINE_GAP * h {
                out.push('\n');
            }
        }
        let indent = ((row.left() - body_left) / unit).round();
        if indent >= 1.0 {
            // 有限的非负数，转换不会溢出
            let n = (indent as usize).min(MAX_INDENT);
            out.extend(std::iter::repeat_n(' ', n));
        }
        for (i, seg) in row.segments.iter().enumerate() {
            if i > 0 {
                out.push('\t');
            }
            out.push_str(&seg.text);
        }
        prev_bottom = Some(row.bottom());
    }
    out
}

/// paragraphs 模式
pub fn to_paragraphs(lines: &[TextLine]) -> String {
    let rows = rows(lines);
    if rows.is_empty() {
        return NO_TEXT.to_owned();
    }
    let h = median(rows.iter().map(Row::height));
    let all: Vec<Segment> = rows.iter().flat_map(|r| r.segments.iter().cloned()).collect();
    // 中英之间的断行补不补空格，看同一截图里其他地方怎么写
    let spacing = MixedSpacing::infer(all.iter().map(|s| s.text.as_str()));
    let text_width = all.iter().map(|s| s.x1).fold(f32::NEG_INFINITY, f32::max)
        - all.iter().map(|s| s.x0).fold(f32::INFINITY, f32::min);
    let mut paragraphs: Vec<Paragraph> = Vec::new();
    // 仍可能接上下一行的段落（每一栏最多一个）
    let mut open: Vec<usize> = Vec::new();
    for row in rows {
        let mut continued = Vec::new();
        for seg in row.segments {
            let target = open
                .iter()
                .copied()
                .filter(|&i| !continued.contains(&i))
                .find(|&i| paragraphs[i].accepts(&seg, &all));
            match target {
                Some(i) => {
                    paragraphs[i].push(seg, spacing);
                    continued.push(i);
                }
                None => {
                    paragraphs.push(Paragraph::new(seg));
                    continued.push(paragraphs.len() - 1);
                }
            }
        }
        // 没被这一行接上的段落就此结束：段落只能由紧接着的下一行延续
        open = continued;
    }
    let compact: Vec<bool> = paragraphs
        .iter()
        .map(|p| p.compact(|| column_measure(p, &paragraphs, h).unwrap_or(text_width)))
        .collect();
    let mut out = String::new();
    for (i, p) in paragraphs.iter().enumerate() {
        if i > 0 {
            out.push_str(separator(&paragraphs[i - 1], p, compact[i - 1] && compact[i], h));
        }
        out.push_str(&p.text);
    }
    out
}

/// paragraphs 模式：段所在这一栏的行长，用来判断单行段是不是短行。取同一栏里折了行的段：左边界与本段首行相差不超过
/// [`PARAGRAPH_LEFT`] 个行高、竖向相距不超过 [`COLUMN_WINDOW`] 个行高、至少 [`MIN_COLUMN_MEASURE`] 个行高宽的多行段里，
/// 最靠右的右边界到本段左边界的距离。
///
/// 只看折了行的段：它们写满了一栏，宽度才是这一栏的行长。双栏文档、带侧栏的页面里一栏不到全图的六成，按全部文字的
/// 宽度算，恰好一行写完的正文也成了短行；反过来，若把附近所有左边界相近的行都算进来，侧栏里一列菜单项的「行长」就是
/// 最长的那一项，菜单项彼此之间反而要空行。附近没有够宽的折行段（侧栏的菜单、表格、只有短消息的聊天、窄面板）时为
/// None，由调用方退回全部文字的宽度，与这些版面原来的分组一致：窄面板里的一组指标、标签仍算短行
fn column_measure(p: &Paragraph, paragraphs: &[Paragraph], h: f32) -> Option<f32> {
    let left = p.first.x0;
    paragraphs
        .iter()
        .filter(|q| {
            let q_left = q.first.x0.min(q.last.x0);
            q.lines > 1
                && q.right - q_left >= MIN_COLUMN_MEASURE * h
                && (q_left - left).abs() <= PARAGRAPH_LEFT * h
                && (q.first.y0 - p.first.y1).max(p.first.y0 - q.last.y1) <= COLUMN_WINDOW * h
        })
        .map(|q| q.right)
        .reduce(f32::max)
        .map(|right| right - left)
}

/// paragraphs 模式两段之间的分隔：前后两段都是列表项或短行（`both_compact`，见 [`Paragraph::compact`]），且两段之间的
/// 竖向空白不超过 lines 模式插空行的阈值（[`BLANK_LINE_GAP`] 个行高，行高取全图的中位数）时只换行，否则照旧空一行。
///
/// 宁可多空一行：竖向空白大说明版面上隔开了，多半是另一组（剪贴板历史里相邻的两条、设置页里的两节、标题与下面的
/// 列表），即使都是短行也照常空行；lines 模式在同样的地方也会空一行，两种模式的分组因此一致。不按框的高度区分标题：
/// 检测框的高度受并进框里的图标、单独的符号影响很大，设置页截图里同一列相邻的两个框就能差到两倍以上，按高度判断
/// 只会让同类的截图时而空行、时而不空。
///
/// 段落按首行的位置排序，多栏的版面里前后两段可能分属两栏；只要满足上面的条件，同一视觉行上的两个单元格、
/// 相邻两行的标签之间同样只换行
fn separator(prev: &Paragraph, next: &Paragraph, both_compact: bool, h: f32) -> &'static str {
    let close = next.first.y0 - prev.last.y1 <= BLANK_LINE_GAP * h;
    if close && both_compact { "\n" } else { "\n\n" }
}

/// 一个片段：一个或几个间距较小的文本框拼成的文字，带外接矩形
#[derive(Debug, Clone)]
struct Segment {
    text: String,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

impl Segment {
    fn of(line: &TextLine) -> Option<Self> {
        let text = line.text.trim();
        if text.is_empty() || line.score < MIN_SCORE || !line.score.is_finite() {
            return None;
        }
        let xs = line.poly.map(|p| p[0]);
        let ys = line.poly.map(|p| p[1]);
        let x0 = xs.iter().copied().fold(f32::INFINITY, f32::min);
        let x1 = xs.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let y0 = ys.iter().copied().fold(f32::INFINITY, f32::min);
        let y1 = ys.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        if ![x0, x1, y0, y1].iter().all(|v| v.is_finite()) {
            return None;
        }
        Some(Self {
            text: text.to_owned(),
            x0,
            y0,
            x1: x1.max(x0 + 1.0),
            y1: y1.max(y0 + 1.0),
        })
    }

    fn height(&self) -> f32 {
        self.y1 - self.y0
    }

    fn center_y(&self) -> f32 {
        (self.y0 + self.y1) / 2.0
    }

    /// 把右侧紧挨着的片段接上来（同一栏内）
    fn extend(&mut self, next: Segment, h: f32) {
        let gap = next.x0 - self.x1;
        if needs_space(&self.text, &next.text, Some(gap / h.max(1.0))) {
            self.text.push(' ');
        }
        self.text.push_str(&next.text);
        self.x0 = self.x0.min(next.x0);
        self.x1 = self.x1.max(next.x1);
        self.y0 = self.y0.min(next.y0);
        self.y1 = self.y1.max(next.y1);
    }
}

/// 一个视觉行：按 x 排序的若干栏
#[derive(Debug, Clone)]
struct Row {
    segments: Vec<Segment>,
}

impl Row {
    fn left(&self) -> f32 {
        self.segments.iter().map(|s| s.x0).fold(f32::INFINITY, f32::min)
    }

    fn top(&self) -> f32 {
        self.segments.iter().map(|s| s.y0).fold(f32::INFINITY, f32::min)
    }

    fn bottom(&self) -> f32 {
        self.segments.iter().map(|s| s.y1).fold(f32::NEG_INFINITY, f32::max)
    }

    fn height(&self) -> f32 {
        median(self.segments.iter().map(Segment::height))
    }
}

/// 过滤、分视觉行、行内排序与分栏
fn rows(lines: &[TextLine]) -> Vec<Row> {
    let mut frags: Vec<Segment> = lines.iter().filter_map(Segment::of).collect();
    frags.sort_by(|a, b| a.center_y().total_cmp(&b.center_y()));

    // 每个视觉行记录成员与代表的竖向范围（成员范围的平均，避免一路被拉长）
    struct Band {
        members: Vec<Segment>,
        y0: f32,
        y1: f32,
    }
    let mut bands: Vec<Band> = Vec::new();
    for f in frags {
        let best = bands
            .iter_mut()
            .rev()
            // 按中心排序后，能重叠的视觉行只会在最近的几行里
            .take(4)
            .filter(|b| {
                let overlap = b.y1.min(f.y1) - b.y0.max(f.y0);
                let min_h = (b.y1 - b.y0).min(f.height());
                overlap >= ROW_OVERLAP * min_h
                    && !b
                        .members
                        .iter()
                        .any(|m| m.x1.min(f.x1) - m.x0.max(f.x0) > 0.5 * (m.x1 - m.x0).min(f.x1 - f.x0))
            })
            .max_by(|a, b| {
                let oa = a.y1.min(f.y1) - a.y0.max(f.y0);
                let ob = b.y1.min(f.y1) - b.y0.max(f.y0);
                oa.total_cmp(&ob)
            });
        match best {
            Some(b) => {
                b.members.push(f);
                let n = b.members.len() as f32;
                b.y0 = b.members.iter().map(|m| m.y0).sum::<f32>() / n;
                b.y1 = b.members.iter().map(|m| m.y1).sum::<f32>() / n;
            }
            None => bands.push(Band {
                y0: f.y0,
                y1: f.y1,
                members: vec![f],
            }),
        }
    }
    bands.sort_by(|a, b| (a.y0 + a.y1).total_cmp(&(b.y0 + b.y1)));

    bands
        .into_iter()
        .map(|mut b| {
            b.members.sort_by(|a, c| a.x0.total_cmp(&c.x0));
            let h = median(b.members.iter().map(Segment::height));
            let mut segments: Vec<Segment> = Vec::new();
            for m in b.members {
                match segments.last_mut() {
                    Some(last) if m.x0 - last.x1 <= COLUMN_GAP * h => last.extend(m, h),
                    _ => segments.push(m),
                }
            }
            Row { segments }
        })
        .collect()
}

/// 一段正在拼的文字
#[derive(Debug, Clone)]
struct Paragraph {
    text: String,
    lines: usize,
    /// 以列表项开头（列表项总是另起一段，所以只看首行）
    item: bool,
    first: Segment,
    last: Segment,
    /// 段内各行的最大右边界
    right: f32,
}

impl Paragraph {
    fn new(seg: Segment) -> Self {
        Self {
            text: seg.text.clone(),
            lines: 1,
            item: starts_list_item(&seg.text),
            right: seg.x1,
            first: seg.clone(),
            last: seg,
        }
    }

    /// 这一片段能否接在本段之后（`all` 是全部片段，用来估计这一栏的右边界）
    fn accepts(&self, seg: &Segment, all: &[Segment]) -> bool {
        let last = &self.last;
        let h = last.height().max(seg.height()).max(1.0);
        let pitch = seg.center_y() - last.center_y();
        if pitch <= 0.0 || pitch > PARAGRAPH_PITCH * h {
            return false;
        }
        // 同一栏：横向要有重叠
        if seg.x1.min(self.right) - seg.x0.max(self.first.x0.min(last.x0)) <= 0.0 {
            return false;
        }
        // 左边界接近；或者本段只有一行且首行缩进了
        let dx = seg.x0 - last.x0;
        let left_ok = dx.abs() <= PARAGRAPH_LEFT * h || (self.lines == 1 && dx < 0.0 && -dx <= FIRST_LINE_INDENT * h);
        if !left_ok {
            return false;
        }
        // 上一行写到了接近这一栏右边界的位置（短行意味着段落在这里结束）
        if last.x1 - last.x0 < PARAGRAPH_MIN_WIDTH * h {
            return false;
        }
        let column_right = all
            .iter()
            .filter(|s| {
                (s.x0 - last.x0).abs() <= PARAGRAPH_LEFT * h
                    && (s.center_y() - last.center_y()).abs() <= COLUMN_WINDOW * h
            })
            .map(|s| s.x1)
            .fold(self.right.max(seg.x1), f32::max);
        if last.x1 < column_right - PARAGRAPH_RIGHT * h {
            return false;
        }
        // 列表项另起一段
        !starts_list_item(&seg.text)
    }

    /// 可以与相邻的同类段落只隔一个换行：列表项（不论折成几行），或一行就结束、宽度不超过所在这一栏行长
    /// [`SHORT_PARAGRAPH`] 的短行。`measure` 给出这一栏的行长，只在单行、不是列表项的段上才算
    fn compact(&self, measure: impl FnOnce() -> f32) -> bool {
        self.item || (self.lines == 1 && self.first.x1 - self.first.x0 <= SHORT_PARAGRAPH * measure())
    }

    fn push(&mut self, seg: Segment, spacing: MixedSpacing) {
        join_break(&mut self.text, &seg.text, spacing);
        self.lines += 1;
        self.right = self.right.max(seg.x1);
        self.last = seg;
    }
}

/// 一个「字宽」：按片段宽度除以显示宽度估计，取中位数。只用来折算 lines 模式的缩进
fn char_unit(rows: &[Row], h: f32) -> f32 {
    let unit = median(rows.iter().flat_map(|r| r.segments.iter()).filter_map(|s| {
        let units: usize = s.text.chars().map(|c| if is_wide(c) { 2 } else { 1 }).sum();
        (units >= 4).then(|| (s.x1 - s.x0) / units as f32)
    }));
    if unit.is_finite() && unit > 0.5 {
        unit
    } else {
        (h * 0.5).max(1.0)
    }
}

/// 估字宽时记作两个半角宽的字符：CJK 部首、CJK 符号与标点、假名、注音、汉字（基本区、扩展 A、扩展 B 及以后、
/// 兼容汉字）、竖排标点，以及整个全角与半角形式区。
///
/// 只服务于 [`char_unit`]，刻意与拼接规则用的文字分类分开：拼接规则调整时，缩进的折算不跟着变。半角片假名记 2、
/// 谚文记 1，与实际字宽有出入，但缩进只取整数个空格、字宽取各片段的中位数，影响可以接受
fn is_wide(c: char) -> bool {
    matches!(
        c as u32,
        0x2E80..=0x2FDF      // CJK 部首
            | 0x3000..=0x303F // CJK 符号与标点
            | 0x3040..=0x30FF // 平假名、片假名
            | 0x3100..=0x312F // 注音
            | 0x31F0..=0x31FF // 片假名扩展
            | 0x3400..=0x4DBF // 扩展 A
            | 0x4E00..=0x9FFF // 基本汉字
            | 0xF900..=0xFAFF // 兼容汉字
            | 0xFE30..=0xFE4F // 竖排标点
            | 0xFF00..=0xFFEF // 全角与半角形式
            | 0x20000..=0x3134F // 扩展 B 及以后
    )
}

/// 主体左边界：至少四分之一的行靠着的最左位置（避免个别靠左的行号、图标把整体都算成缩进）
fn body_left(rows: &[Row], h: f32) -> f32 {
    let mut lefts: Vec<f32> = rows.iter().map(Row::left).collect();
    lefts.sort_by(f32::total_cmp);
    let need = (rows.len() / 4).max(1);
    lefts
        .iter()
        .copied()
        .find(|&c| lefts.iter().filter(|&&x| (x - c).abs() <= h).count() >= need)
        .unwrap_or_else(|| lefts.first().copied().unwrap_or(0.0))
}

/// 中位数（空时为 NaN，由调用方兜底）
fn median(values: impl Iterator<Item = f32>) -> f32 {
    let mut v: Vec<f32> = values.filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return f32::NAN;
    }
    v.sort_by(f32::total_cmp);
    v[v.len() / 2]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个横平竖直的文本框
    fn line(text: &str, x0: f32, y0: f32, x1: f32, y1: f32) -> TextLine {
        TextLine {
            text: text.to_owned(),
            score: 0.95,
            poly: [[x0, y0], [x1, y0], [x1, y1], [x0, y1]],
        }
    }

    #[test]
    fn empty_and_low_confidence_input_is_no_text() {
        assert_eq!(to_lines(&[]), NO_TEXT);
        assert_eq!(to_paragraphs(&[]), NO_TEXT);
        let mut junk = line("~~", 0.0, 0.0, 30.0, 20.0);
        junk.score = 0.3;
        assert_eq!(to_lines(&[junk.clone()]), NO_TEXT);
        assert_eq!(
            assemble(&[junk, line("   ", 0.0, 30.0, 10.0, 50.0)], OcrMode::Paragraphs),
            NO_TEXT
        );
    }

    #[test]
    fn fragments_on_one_visual_line_are_joined_in_x_order() {
        let input = [
            line("world", 120.0, 12.0, 200.0, 32.0),
            line("Hello", 10.0, 10.0, 100.0, 30.0),
            line("你好", 10.0, 50.0, 50.0, 70.0),
            line("世界", 52.0, 51.0, 92.0, 71.0),
            line("Wisp", 96.0, 50.0, 150.0, 70.0),
        ];
        // 拉丁文之间加空格；汉字之间不加；汉字与拉丁文贴得很近时不加
        assert_eq!(to_lines(&input), "Hello world\n你好世界Wisp");
    }

    #[test]
    fn distant_fragments_become_columns() {
        let input = [
            line("Name", 10.0, 10.0, 60.0, 30.0),
            line("Age", 200.0, 10.0, 240.0, 30.0),
            line("Alice", 10.0, 40.0, 60.0, 60.0),
            line("30", 200.0, 40.0, 225.0, 60.0),
        ];
        assert_eq!(to_lines(&input), "Name\tAge\nAlice\t30");
        // paragraphs：两栏各自成段；单元格都是短行，彼此之间只换行
        assert_eq!(to_paragraphs(&input), "Name\nAge\nAlice\n30");
    }

    #[test]
    fn lines_keep_relative_indent_and_blank_gaps() {
        let input = [
            line("fn main() {", 10.0, 10.0, 120.0, 30.0),
            line("let x = 1;", 50.0, 32.0, 150.0, 52.0),
            line("}", 10.0, 54.0, 20.0, 74.0),
            line("// end", 10.0, 120.0, 70.0, 140.0),
        ];
        let out = to_lines(&input);
        let lines: Vec<&str> = out.split('\n').collect();
        assert_eq!(lines[0], "fn main() {");
        assert!(
            lines[1].starts_with(' ') && lines[1].trim_start() == "let x = 1;",
            "{out:?}"
        );
        assert!(lines[1].len() - lines[1].trim_start().len() <= MAX_INDENT);
        assert_eq!(lines[2], "}");
        assert_eq!(lines[3], "", "明显的竖向空白插一个空行");
        assert_eq!(lines[4], "// end");
    }

    #[test]
    fn paragraphs_merge_full_lines_and_restore_hyphenation() {
        let input = [
            line("Wisp stays out of your way: it idles at near", 10.0, 10.0, 500.0, 30.0),
            line("zero CPU and wakes in milli-", 10.0, 34.0, 495.0, 54.0),
            line("seconds.", 10.0, 58.0, 90.0, 78.0),
            line("A second paragraph.", 10.0, 110.0, 200.0, 130.0),
        ];
        assert_eq!(
            to_paragraphs(&input),
            "Wisp stays out of your way: it idles at near zero CPU and wakes in milliseconds.\n\nA second paragraph."
        );
    }

    #[test]
    fn paragraphs_join_cjk_without_spaces_and_split_list_items() {
        let input = [
            line("常驻后台的工具首先要安静：空闲时几乎不占", 10.0, 10.0, 400.0, 30.0),
            line("CPU，也不抢走焦点。", 10.0, 34.0, 200.0, 54.0),
            line("1. 第一项", 10.0, 80.0, 100.0, 100.0),
            line("2. 第二项", 10.0, 104.0, 100.0, 124.0),
        ];
        assert_eq!(
            to_paragraphs(&input),
            "常驻后台的工具首先要安静：空闲时几乎不占CPU，也不抢走焦点。\n\n1. 第一项\n2. 第二项"
        );
    }

    #[test]
    fn list_items_stay_together_even_when_wrapped() {
        let input = [
            line("Before you release:", 10.0, 10.0, 180.0, 30.0),
            line(
                "1. Update the version number in every manifest",
                10.0,
                60.0,
                490.0,
                80.0,
            ),
            line("and commit the change.", 10.0, 84.0, 210.0, 104.0),
            line("2. Run the checks.", 10.0, 108.0, 170.0, 128.0),
            line(
                "That is all there is to it, and the rest of this paragraph",
                10.0,
                160.0,
                490.0,
                180.0,
            ),
            line("explains why.", 10.0, 184.0, 120.0, 204.0),
        ];
        // 折成两行的列表项与下一项之间也只换行；列表与前后的文字之间隔开了超过一个行高，照常空行
        assert_eq!(
            to_paragraphs(&input),
            "Before you release:\n\n1. Update the version number in every manifest and commit the change.\n2. Run the checks.\n\nThat is all there is to it, and the rest of this paragraph explains why."
        );
    }

    #[test]
    fn short_lines_group_without_blank_lines() {
        let input = [
            line(
                "Wisp keeps every tool one shortcut away and stays out",
                10.0,
                10.0,
                490.0,
                30.0,
            ),
            line("of the way.", 10.0, 34.0, 110.0, 54.0),
            // 紧挨着的一组标签
            line("General", 10.0, 70.0, 80.0, 90.0),
            line("Shortcuts", 10.0, 98.0, 95.0, 118.0),
            line("Models", 10.0, 126.0, 70.0, 146.0),
            // 隔开一大段的另一组
            line("Sync", 10.0, 200.0, 50.0, 220.0),
            line("About", 10.0, 228.0, 60.0, 248.0),
            // 一行写完、但写到七成以上宽度的两段正文
            line(
                "A single sentence that runs most of the way across.",
                10.0,
                256.0,
                400.0,
                276.0,
            ),
            line(
                "Another one that also runs most of the way across.",
                10.0,
                284.0,
                390.0,
                304.0,
            ),
        ];
        assert_eq!(
            to_paragraphs(&input),
            "Wisp keeps every tool one shortcut away and stays out of the way.\n\nGeneral\nShortcuts\nModels\n\nSync\nAbout\n\nA single sentence that runs most of the way across.\n\nAnother one that also runs most of the way across."
        );
        // lines 模式不受影响
        assert_eq!(to_lines(&input[2..5]), "General\nShortcuts\nModels");
    }

    #[test]
    fn short_lines_are_measured_against_their_own_column() {
        // 双栏文档：左栏 10–350、右栏 400–740。左栏里两段各一行写完、写到这一栏八成的正文，按全部文字的宽度（730）
        // 算不到六成，按这一栏的行长（由左栏折了行的段估计）才看得出是正文，照常空一行
        let input = [
            line(
                "Wisp keeps every tool one shortcut away, so it can stay",
                10.0,
                10.0,
                350.0,
                30.0,
            ),
            line(
                "The right column has its own paragraph that also runs on",
                400.0,
                10.0,
                740.0,
                30.0,
            ),
            line("out of the way.", 10.0, 34.0, 150.0, 54.0),
            line("to a second line.", 400.0, 34.0, 600.0, 54.0),
            line(
                "A single sentence that fills most of the column.",
                10.0,
                70.0,
                290.0,
                90.0,
            ),
            line("Another one that also fills most of it.", 10.0, 98.0, 280.0, 118.0),
        ];
        assert_eq!(
            to_paragraphs(&input),
            "Wisp keeps every tool one shortcut away, so it can stay out of the way.\n\nThe right column has its own paragraph that also runs on to a second line.\n\nA single sentence that fills most of the column.\n\nAnother one that also fills most of it."
        );
        // 侧栏的菜单项：这一栏没有折了行的段，退回按全部文字的宽度算，仍是短行，彼此之间只换行
        let sidebar = [
            line("General", 10.0, 10.0, 80.0, 30.0),
            line(
                "Wisp keeps every tool one shortcut away and stays",
                200.0,
                10.0,
                600.0,
                30.0,
            ),
            line("Shortcuts", 10.0, 38.0, 95.0, 58.0),
            line("out of the way.", 200.0, 34.0, 320.0, 54.0),
            line("Models", 10.0, 66.0, 70.0, 86.0),
        ];
        assert_eq!(
            to_paragraphs(&sidebar),
            "General\n\nWisp keeps every tool one shortcut away and stays out of the way.\n\nShortcuts\nModels"
        );
        // 窄面板：两行拼成的段只有 10 个行高宽，不拿它当行长；面板里的一组指标仍是短行，彼此之间只换行
        let panel = [
            line("Performance budget for the", 10.0, 10.0, 210.0, 30.0),
            line("GPT-5.4 mini", 500.0, 10.0, 600.0, 30.0),
            line("panel", 10.0, 34.0, 60.0, 54.0),
            line("Panel show p95 ≤ 100 ms", 10.0, 70.0, 160.0, 90.0),
            line("First token p95 ≤ 1.2 s", 10.0, 98.0, 160.0, 118.0),
            line("Idle CPU ≈ 0%", 10.0, 126.0, 120.0, 146.0),
        ];
        assert_eq!(
            to_paragraphs(&panel),
            "Performance budget for the panel\n\nGPT-5.4 mini\n\nPanel show p95 ≤ 100 ms\nFirst token p95 ≤ 1.2 s\nIdle CPU ≈ 0%"
        );
    }

    #[test]
    fn paragraphs_follow_the_screenshots_cjk_latin_spacing() {
        let spaced = [
            line("使用 Wisp 时，面板在光标旁弹出，支持", 10.0, 10.0, 480.0, 30.0),
            line("OpenAI 等服务。", 10.0, 34.0, 150.0, 54.0),
        ];
        assert_eq!(
            to_paragraphs(&spaced),
            "使用 Wisp 时，面板在光标旁弹出，支持 OpenAI 等服务。"
        );
        let tight = [
            line("使用Wisp时，面板在光标旁弹出，支持", 10.0, 10.0, 480.0, 30.0),
            line("OpenAI等服务。", 10.0, 34.0, 150.0, 54.0),
        ];
        assert_eq!(
            to_paragraphs(&tight),
            "使用Wisp时，面板在光标旁弹出，支持OpenAI等服务。"
        );
    }

    #[test]
    fn short_lines_end_paragraphs() {
        let input = [
            line("Title", 10.0, 10.0, 60.0, 30.0),
            line(
                "Body text that fills the whole line width here.",
                10.0,
                34.0,
                480.0,
                54.0,
            ),
            line("And continues on the next line.", 10.0, 58.0, 300.0, 78.0),
        ];
        assert_eq!(
            to_paragraphs(&input),
            "Title\n\nBody text that fills the whole line width here. And continues on the next line."
        );
    }

    #[test]
    fn markdown_and_translate_fall_back_to_paragraphs() {
        let input = [line("Hello", 10.0, 10.0, 60.0, 30.0)];
        assert_eq!(assemble(&input, OcrMode::Markdown), "Hello");
        assert_eq!(assemble(&input, OcrMode::Translate), "Hello");
        assert_eq!(assemble(&input, OcrMode::Lines), "Hello");
    }
}
