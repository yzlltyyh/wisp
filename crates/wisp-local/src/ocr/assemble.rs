//! 把带框的文本行拼成纯文本（本地 PP-OCR 与云端 PP-OCRv6 共用）。
//!
//! - 按框的竖向重叠把行分到「视觉行」，行内按 x 排序；同一视觉行内相邻片段按间距决定是否插空格（CJK 之间不加）。
//!   间距超过约两个字高的片段视为不同的栏（表格的单元格、并排的面板）：lines 模式用制表符隔开，paragraphs 模式各自成段。
//! - lines：每个视觉行一行，保留相对缩进（左边界与主体左边界之差折算成空格，最多 8 个）；行间有明显空白时空一行。
//! - paragraphs：相邻视觉行在行距 ≤ 1.5 × 行高、左边界接近、上一行接近右边界时合并为一段；英文行尾的连字符断词还原；
//!   列表项另起一段。段与段之间空一行（与视觉模型的 paragraphs 模式相同）。
//! - 置信度低于 [`MIN_SCORE`] 的行丢弃。没有文字时输出 [`NO_TEXT`]（编排器的无文字判定照常生效）。
//!
//! 这里只管几何（分行、分栏、缩进、段落边界）；只看文字本身的规则（是否加空格、断词还原、列表项）在 [`wisp_text::join`]。

use wisp_text::join::{join_line, needs_space, starts_list_item};
use wisp_text::script::is_unspaced;
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
    let all: Vec<Segment> = rows.iter().flat_map(|r| r.segments.iter().cloned()).collect();
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
                    paragraphs[i].push(seg);
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
    paragraphs
        .iter()
        .map(|p| p.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
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

    fn push(&mut self, seg: Segment) {
        join_line(&mut self.text, &seg.text);
        self.lines += 1;
        self.right = self.right.max(seg.x1);
        self.last = seg;
    }
}

/// 一个「字宽」：按片段宽度除以显示宽度估计，取中位数。显示宽度粗略地把不用空格分词的东亚文字（汉字、假名、
/// 全角形式等）记 2、其余记 1，只用来折算缩进，谚文按 1 计带来的偏差可以接受
fn char_unit(rows: &[Row], h: f32) -> f32 {
    let unit = median(rows.iter().flat_map(|r| r.segments.iter()).filter_map(|s| {
        let units: usize = s.text.chars().map(|c| if is_unspaced(c) { 2 } else { 1 }).sum();
        (units >= 4).then(|| (s.x1 - s.x0) / units as f32)
    }));
    if unit.is_finite() && unit > 0.5 {
        unit
    } else {
        (h * 0.5).max(1.0)
    }
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
        // paragraphs：两栏各自成段
        assert_eq!(to_paragraphs(&input), "Name\n\nAge\n\nAlice\n\n30");
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
            "常驻后台的工具首先要安静：空闲时几乎不占CPU，也不抢走焦点。\n\n1. 第一项\n\n2. 第二项"
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
