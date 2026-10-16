//! PaddleOCR-VL 表格识别的输出（OTSL：`<fcel>` / `<ecel>` / `<lcel>` / `<ucel>` / `<xcel>` / `<nl>`）转成 Markdown 表格。
//!
//! Markdown 表格没有合并单元格：横向合并（`lcel`）与纵向合并（`ucel`）、交叉合并（`xcel`）都留空，文字只写在合并区域
//! 左上角的格子里；第一行作为表头。行的格子数不一致时（模型漏写或多写了格子）照 PaddleX 的 `otsl_pad_to_sqr_v2`
//! 选一个使各行增删格子总数最少的列数，再补空格子或截掉多余的。输出不是合法的 OTSL（例如模型直接给出了 Markdown 或
//! HTML）时返回 None，由调用方原样使用。

/// OTSL 的格子种类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cell {
    /// 有文字的格子
    Filled,
    /// 空格子
    Empty,
    /// 与左边合并
    Left,
    /// 与上边合并
    Up,
    /// 与左上合并
    Cross,
}

const TAGS: [(&str, Option<Cell>); 6] = [
    ("<fcel>", Some(Cell::Filled)),
    ("<ecel>", Some(Cell::Empty)),
    ("<lcel>", Some(Cell::Left)),
    ("<ucel>", Some(Cell::Up)),
    ("<xcel>", Some(Cell::Cross)),
    ("<nl>", None),
];

/// OTSL → Markdown 表格；不是 OTSL 时为 None
pub fn to_markdown(otsl: &str) -> Option<String> {
    let grid = parse(otsl)?;
    let mut out = String::new();
    for (i, row) in grid.iter().enumerate() {
        out.push('|');
        for text in row {
            out.push(' ');
            out.push_str(&escape(text));
            out.push_str(" |");
        }
        out.push('\n');
        if i == 0 {
            out.push('|');
            out.extend(std::iter::repeat_n(" --- |", row.len()));
            out.push('\n');
        }
    }
    out.pop();
    Some(out)
}

/// OTSL → 按行展开、格子之间用制表符分隔的纯文本（lines / paragraphs 模式用）；不是 OTSL 时为 None
pub fn to_tsv(otsl: &str) -> Option<String> {
    let grid = parse(otsl)?;
    Some(
        grid.iter()
            .map(|row| {
                row.iter()
                    .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
                    .collect::<Vec<_>>()
                    .join("\t")
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// 解析成「每行每格的文字」的矩形网格（合并区域只有左上角有文字）
fn parse(otsl: &str) -> Option<Vec<Vec<String>>> {
    let tokens = tokenize(otsl)?;
    // 按 <nl> 分行；每行是若干 (种类, 文字)
    let mut rows: Vec<Vec<(Cell, String)>> = Vec::new();
    let mut current: Vec<(Cell, String)> = Vec::new();
    for (tag, text) in tokens {
        match tag {
            Some(cell) => current.push((cell, text)),
            None => {
                if !current.is_empty() {
                    rows.push(std::mem::take(&mut current));
                }
            }
        }
    }
    if !current.is_empty() {
        rows.push(current);
    }
    if rows.is_empty() {
        return None;
    }

    // 列数：不少于任何一行最后一个有字格子的位置，在此之上让各行增删格子的总数最少
    let min_width = rows
        .iter()
        .map(|r| r.iter().rposition(|(c, _)| *c == Cell::Filled).map_or(0, |i| i + 1))
        .max()
        .unwrap_or(0);
    let max_width = rows.iter().map(Vec::len).max().unwrap_or(0).max(min_width);
    let width = (min_width..=max_width)
        .min_by_key(|&w| rows.iter().map(|r| r.len().abs_diff(w)).sum::<usize>())
        .unwrap_or(max_width)
        .max(1);
    for row in &mut rows {
        row.truncate(width);
        row.resize(width, (Cell::Empty, String::new()));
    }

    Some(
        rows.into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|(cell, text)| match cell {
                        Cell::Filled => text.trim().to_owned(),
                        Cell::Empty | Cell::Left | Cell::Up | Cell::Cross => String::new(),
                    })
                    .collect()
            })
            .collect(),
    )
}

/// 拆成 (标签, 标签后面直到下一个标签的文字)。没有任何格子标签时为 None；第一个标签之前的文字丢弃（与 PaddleX 相同）
fn tokenize(s: &str) -> Option<Vec<(Option<Cell>, String)>> {
    let mut out: Vec<(Option<Cell>, String)> = Vec::new();
    let mut rest = s;
    let mut saw_cell = false;
    while let Some(pos) = rest.find('<') {
        let (before, at) = rest.split_at(pos);
        if let Some(last) = out.last_mut() {
            last.1.push_str(before);
        }
        match TAGS.iter().find(|(t, _)| at.starts_with(t)) {
            Some((t, kind)) => {
                saw_cell |= kind.is_some();
                out.push((*kind, String::new()));
                rest = &at[t.len()..];
            }
            None => {
                // 普通的「<」字符
                if let Some(last) = out.last_mut() {
                    last.1.push('<');
                }
                rest = &at[1..];
            }
        }
    }
    if let Some(last) = out.last_mut() {
        last.1.push_str(rest);
    }
    saw_cell.then_some(out)
}

/// Markdown 表格单元格里的文字：竖线转义、换行并成空格
fn escape(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_table() {
        let otsl = "<fcel>Name<fcel>Age<nl><fcel>Alice<fcel>30<nl><fcel>Bob<ecel><nl>";
        assert_eq!(
            to_markdown(otsl).as_deref(),
            Some("| Name | Age |\n| --- | --- |\n| Alice | 30 |\n| Bob |  |")
        );
        assert_eq!(to_tsv(otsl).as_deref(), Some("Name\tAge\nAlice\t30\nBob"));
    }

    #[test]
    fn merged_cells_degrade_to_empty_cells() {
        // 第一行横向合并两格；第一列纵向合并两行；右下是交叉合并
        let otsl = "<fcel>Region<lcel><fcel>Total<nl><fcel>East<fcel>Q1<fcel>10<nl><ucel><fcel>Q2<xcel><nl>";
        assert_eq!(
            to_markdown(otsl).as_deref(),
            Some("| Region |  | Total |\n| --- | --- | --- |\n| East | Q1 | 10 |\n|  | Q2 |  |")
        );
    }

    #[test]
    fn ragged_rows_are_padded_or_truncated() {
        // 第二行少一格、第三行多一格（多出来的是空格子）：列数取 2
        let otsl = "<fcel>a<fcel>b<nl><fcel>c<nl><fcel>d<fcel>e<ecel><nl>";
        assert_eq!(
            to_markdown(otsl).as_deref(),
            Some("| a | b |\n| --- | --- |\n| c |  |\n| d | e |")
        );
        // 没有结尾的 <nl> 也能解析
        assert_eq!(to_tsv("<fcel>x<fcel>y").as_deref(), Some("x\ty"));
    }

    #[test]
    fn cell_text_is_escaped() {
        let otsl = "<fcel>a | b<fcel>x < y\nz<nl>";
        assert_eq!(
            to_markdown(otsl).as_deref(),
            Some("| a \\| b | x < y z |\n| --- | --- |")
        );
    }

    #[test]
    fn non_otsl_is_none() {
        assert_eq!(to_markdown("| a | b |\n| --- | --- |"), None);
        assert_eq!(to_markdown("<table><tr><td>a</td></tr></table>"), None);
        assert_eq!(to_markdown(""), None);
        assert_eq!(to_markdown("<nl><nl>"), None);
    }
}
