//! 云端结果的解析：字段类型宽松（数字可能是字符串、字段可能缺失或为 null）、未知字段忽略。
//!
//! - OCR 类模型：`ocrResults[].prunedResult.{rec_texts, rec_scores, rec_polys}`（没有 `rec_polys` 时用 `rec_boxes`）→ 文本行；
//! - 文档解析类：`layoutParsingResults[].markdown.text`（去掉区块图片的引用，HTML 表格换成 Markdown 表格）与
//!   `prunedResult.parsing_res_list`（按 `block_order` 排的文字块，lines / paragraphs 模式用）。

use serde_json::Value;

use super::CloudOutput;
use crate::ocr::TextLine;

/// 不进纯文本结果的区块（页眉页脚、页码、脚注、边栏、图片类；与官方的 `markdown_ignore_labels` 一致，另加图片、图表、印章）
const IGNORE_BLOCKS: [&str; 10] = [
    "number",
    "footnote",
    "header",
    "header_image",
    "footer",
    "footer_image",
    "aside_text",
    "image",
    "chart",
    "seal",
];

/// 数字或数字字符串
pub(super) fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// 整数或整数字符串（错误码）
pub(super) fn int(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// 字符串或数字（jobId 可能是数字）
pub(super) fn string(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// 一个或多个 `result` 对象 → 识别结果。`document` 表示按文档解析处理（服务给的结构里两种都有时以它为准）。
/// 结构完全不认识时为 None
pub(super) fn output(results: &[&Value], document: bool) -> Option<CloudOutput> {
    let layout: Vec<&Value> = results
        .iter()
        .filter_map(|r| r.get("layoutParsingResults")?.as_array())
        .flatten()
        .collect();
    let ocr: Vec<&Value> = results
        .iter()
        .filter_map(|r| r.get("ocrResults")?.as_array())
        .flatten()
        .collect();
    let has_layout = results
        .iter()
        .any(|r| r.get("layoutParsingResults").is_some_and(Value::is_array));
    let has_ocr = results.iter().any(|r| r.get("ocrResults").is_some_and(Value::is_array));
    if has_layout && (document || !has_ocr) {
        let mut markdown = Vec::new();
        let mut blocks = Vec::new();
        for page in layout {
            let md = page
                .pointer("/markdown/text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let md = clean_markdown(md);
            if !md.is_empty() {
                markdown.push(md);
            }
            if let Some(list) = page.pointer("/prunedResult/parsing_res_list").and_then(Value::as_array) {
                blocks.extend(text_blocks(list));
            }
        }
        return Some(CloudOutput::Document {
            markdown: markdown.join("\n\n"),
            blocks,
        });
    }
    if has_ocr {
        let mut lines = Vec::new();
        for page in ocr {
            if let Some(pruned) = page.get("prunedResult") {
                lines.extend(text_lines(pruned));
            }
        }
        return Some(CloudOutput::Lines(lines));
    }
    None
}

/// `prunedResult` 里的文本行
fn text_lines(pruned: &Value) -> Vec<TextLine> {
    let empty = Vec::new();
    let texts = pruned.get("rec_texts").and_then(Value::as_array).unwrap_or(&empty);
    let scores = pruned.get("rec_scores").and_then(Value::as_array).unwrap_or(&empty);
    let polys = pruned.get("rec_polys").and_then(Value::as_array);
    let boxes = pruned.get("rec_boxes").and_then(Value::as_array);
    texts
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            let text = t.as_str()?.to_owned();
            let score = scores.get(i).and_then(num).unwrap_or(1.0) as f32;
            let poly = polys
                .and_then(|p| p.get(i))
                .and_then(poly)
                .or_else(|| boxes.and_then(|b| b.get(i)).and_then(rect))?;
            Some(TextLine { text, score, poly })
        })
        .collect()
}

/// `[[x, y] × 4]`
fn poly(v: &Value) -> Option<[[f32; 2]; 4]> {
    let pts = v.as_array()?;
    if pts.len() < 4 {
        return None;
    }
    let mut out = [[0f32; 2]; 4];
    for (o, p) in out.iter_mut().zip(pts) {
        let p = p.as_array()?;
        *o = [num(p.first()?)? as f32, num(p.get(1)?)? as f32];
    }
    Some(out)
}

/// `[x1, y1, x2, y2]` → 四个顶点
fn rect(v: &Value) -> Option<[[f32; 2]; 4]> {
    let b = v.as_array()?;
    let [x1, y1, x2, y2] = [0, 1, 2, 3].map(|i| b.get(i).and_then(num).map(|f| f as f32));
    let (x1, y1, x2, y2) = (x1?, y1?, x2?, y2?);
    Some([[x1, y1], [x2, y1], [x2, y2], [x1, y2]])
}

/// `parsing_res_list` → 按阅读顺序的文字块。`block_order` 为 null 的块（表格等）跟在它前面那个有编号的块之后
fn text_blocks(list: &[Value]) -> Vec<String> {
    let mut picked: Vec<(f64, usize, String)> = Vec::new();
    let mut last_order = 0.0;
    for (i, b) in list.iter().enumerate() {
        let label = b.get("block_label").and_then(Value::as_str).unwrap_or("text");
        let order = b.get("block_order").and_then(num);
        if let Some(o) = order {
            last_order = o;
        }
        if IGNORE_BLOCKS.contains(&label) {
            continue;
        }
        let content = b.get("block_content").and_then(Value::as_str).unwrap_or_default();
        let text = if label == "table" || content.trim_start().starts_with("<table") {
            html_tables_to(content, Table::Tsv)
        } else {
            content.trim().to_owned()
        };
        let text = text.trim();
        if !text.is_empty() {
            picked.push((order.unwrap_or(last_order), i, text.to_owned()));
        }
    }
    picked.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    picked.into_iter().map(|(_, _, t)| t).collect()
}

/// 服务给的 Markdown：去掉区块图片（`<img>` 与包着它的 `<div>`、`![](imgs/…)`），HTML 表格换成 Markdown 表格，
/// 其余 HTML 包装（居中的 `<div>`）去掉，多余的空行合并
pub(super) fn clean_markdown(md: &str) -> String {
    let mut s = remove_image_divs(md);
    s = remove_markdown_images(&s);
    s = remove_tags(&s, "img");
    s = html_tables_to(&s, Table::Markdown);
    s = remove_tags(&s, "div");
    s = remove_tags(&s, "/div");
    collapse_blank_lines(&s)
}

/// 去掉内容里有 `<img` 的 `<div …>…</div>`
fn remove_image_divs(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = find_ci(rest, "<div") {
        let Some(end_rel) = find_ci(&rest[start..], "</div>") else {
            break;
        };
        let end = start + end_rel + "</div>".len();
        let block = &rest[start..end];
        out.push_str(&rest[..start]);
        if find_ci(block, "<img").is_none() {
            out.push_str(block);
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// 去掉 `![…](imgs/…)` 这样指向区块图片的 Markdown 图片
fn remove_markdown_images(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("![") {
        let after = &rest[start..];
        let parsed = after.find("](").and_then(|mid| {
            let close = after[mid..].find(')')? + mid;
            let target = after[mid + 2..close].trim();
            (!target.contains("://") && !target.starts_with("data:")).then_some(close + 1)
        });
        match parsed {
            Some(len) => {
                out.push_str(&rest[..start]);
                rest = &rest[start + len..];
            }
            None => {
                out.push_str(&rest[..start + 2]);
                rest = &rest[start + 2..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// 去掉某种标签（只去标签本身，保留内容）；`name` 形如 `div`、`/div`、`img`
fn remove_tags(s: &str, name: &str) -> String {
    let open = format!("<{name}");
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = find_ci(rest, &open) {
        let after = &rest[start + open.len()..];
        // 必须是完整的标签名（`<div` 不能匹配 `<divider`）
        if !after.starts_with(|c: char| c.is_whitespace() || c == '>' || c == '/') {
            out.push_str(&rest[..start + open.len()]);
            rest = after;
            continue;
        }
        let Some(end) = after.find('>') else {
            break;
        };
        out.push_str(&rest[..start]);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// 不区分大小写地找 ASCII 子串
fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    (0..=h.len() - n.len()).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

fn collapse_blank_lines(s: &str) -> String {
    let mut out = String::new();
    let mut blank = 0;
    for line in s.trim().split('\n') {
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out.trim_end().to_owned()
}

/// HTML 表格换成什么
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Table {
    Markdown,
    Tsv,
}

/// 把文字里的每个 `<table>…</table>` 换掉（没有合并单元格的概念：`colspan` 补空格子，`rowspan` 忽略）
fn html_tables_to(s: &str, kind: Table) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = find_ci(rest, "<table") {
        let Some(end_rel) = find_ci(&rest[start..], "</table>") else {
            break;
        };
        let end = start + end_rel + "</table>".len();
        out.push_str(&rest[..start]);
        let rows = table_rows(&rest[start..end]);
        out.push_str(&match kind {
            Table::Markdown => markdown_table(&rows),
            Table::Tsv => rows.iter().map(|r| r.join("\t")).collect::<Vec<_>>().join("\n"),
        });
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn table_rows(table: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut rest = table;
    while let Some(tr) = find_ci(rest, "<tr") {
        let row_start = tr + 3;
        let row_end = find_ci(&rest[row_start..], "</tr>").map_or(rest.len(), |e| row_start + e);
        let row = &rest[row_start..row_end];
        let mut cells = Vec::new();
        let mut r = row;
        loop {
            let td = find_ci(r, "<td");
            let th = find_ci(r, "<th");
            let Some(pos) = [td, th].into_iter().flatten().min() else {
                break;
            };
            let tag_end = r[pos..].find('>').map_or(r.len(), |e| pos + e);
            let attrs = &r[pos..tag_end];
            let content_start = (tag_end + 1).min(r.len());
            let close = ["</td>", "</th>"]
                .iter()
                .filter_map(|c| find_ci(&r[content_start..], c))
                .min()
                .map_or(r.len(), |e| content_start + e);
            cells.push(cell_text(&r[content_start..close]));
            for _ in 1..colspan(attrs) {
                cells.push(String::new());
            }
            r = &r[(close + 5).min(r.len())..];
        }
        if !cells.is_empty() {
            rows.push(cells);
        }
        rest = &rest[(row_end + 5).min(rest.len())..];
    }
    rows
}

fn colspan(attrs: &str) -> usize {
    let Some(i) = find_ci(attrs, "colspan") else {
        return 1;
    };
    attrs[i + 7..]
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse::<usize>()
        .map_or(1, |n| n.clamp(1, 64))
}

/// 单元格的文字：去掉标签、还原常见实体、空白并成一个空格
fn cell_text(html: &str) -> String {
    let mut text = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                text.push(' ');
            }
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }
    let text = text
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn markdown_table(rows: &[Vec<String>]) -> String {
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    for (i, row) in rows.iter().enumerate() {
        out.push('|');
        for j in 0..width {
            out.push(' ');
            out.push_str(&row.get(j).map_or(String::new(), |c| c.replace('|', "\\|")));
            out.push_str(" |");
        }
        out.push('\n');
        if i == 0 {
            out.push('|');
            out.push_str(&" --- |".repeat(width));
            out.push('\n');
        }
    }
    format!("\n{}\n", out.trim_end())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn markdown_images_and_wrappers_are_removed() {
        let md = "## 标题\n\n正文 $ \\rightarrow $\n\n\n\n<div style=\"text-align: center;\"><img src=\"imgs/img_in_image_box_1_2_3_4.jpg\" alt=\"Image\" width=\"14%\" /></div>\n\n![](imgs/a.jpg)\n\n<div style=\"text-align: center;\">居中的字</div>\n\n尾";
        assert_eq!(clean_markdown(md), "## 标题\n\n正文 $ \\rightarrow $\n\n居中的字\n\n尾");
        // 外链图片保留
        assert_eq!(
            clean_markdown("![logo](https://example.com/a.png)"),
            "![logo](https://example.com/a.png)"
        );
    }

    #[test]
    fn html_tables_become_markdown_or_tsv() {
        let html = "<div style=\"text-align: center;\"><table border=1><tr><th>名称</th><th colspan=\"2\">数量</th></tr><tr><td>A &amp; B</td><td>1</td><td>2</td></tr></table></div>";
        assert_eq!(
            clean_markdown(html),
            "| 名称 | 数量 |  |\n| --- | --- | --- |\n| A & B | 1 | 2 |"
        );
        assert_eq!(
            html_tables_to(html, Table::Tsv)
                .replace("<div style=\"text-align: center;\">", "")
                .replace("</div>", ""),
            "名称\t数量\t\nA & B\t1\t2"
        );
    }

    #[test]
    fn lenient_numbers() {
        assert_eq!(num(&json!("12.5")), Some(12.5));
        assert_eq!(num(&json!(3)), Some(3.0));
        assert_eq!(num(&json!(null)), None);
        assert_eq!(int(Some(&json!("503"))), Some(503));
        assert_eq!(
            string(Some(&json!(104436058182316032_u64))).as_deref(),
            Some("104436058182316032")
        );
    }

    #[test]
    fn ocr_results_become_lines() {
        let r = json!({"ocrResults": [{"prunedResult": {
            "rec_texts": ["Hello", "世界", 7],
            "rec_scores": [0.99, "0.8", 1],
            "rec_polys": [[[1, 2], [30, 2], [30, 12], [1, 12]], [["1", "20"], [30, 20], [30, 32], [1, 32]], []],
        }}]});
        let Some(CloudOutput::Lines(lines)) = output(&[&r], false) else {
            panic!("lines");
        };
        assert_eq!(lines.len(), 2, "非字符串的文字跳过");
        assert_eq!(lines[1].text, "世界");
        assert!((lines[1].score - 0.8).abs() < 1e-6);
        assert_eq!(lines[1].poly[0], [1.0, 20.0]);

        // 没有 rec_polys 时用 rec_boxes
        let r = json!({"ocrResults": [{"prunedResult": {"rec_texts": ["x"], "rec_boxes": [[1, 2, 3, 4]]}}]});
        let Some(CloudOutput::Lines(lines)) = output(&[&r], false) else {
            panic!("lines");
        };
        assert_eq!(lines[0].poly, [[1.0, 2.0], [3.0, 2.0], [3.0, 4.0], [1.0, 4.0]]);
    }

    #[test]
    fn layout_results_become_documents() {
        let r = json!({"layoutParsingResults": [{
            "markdown": {"text": "## Title\n\nBody", "images": {}},
            "prunedResult": {"parsing_res_list": [
                {"block_label": "header", "block_content": "Notes", "block_order": null},
                {"block_label": "text", "block_content": "Body", "block_order": "2"},
                {"block_label": "paragraph_title", "block_content": "Title", "block_order": 1},
                {"block_label": "table", "block_content": "<table><tr><td>a</td><td>b</td></tr></table>", "block_order": null},
                {"block_label": "image", "block_content": "", "block_order": null},
                {"block_label": "text", "block_content": "  ", "block_order": 3},
            ]},
        }], "ocrResults": []});
        let out = output(&[&r], true).expect("doc");
        assert_eq!(
            out,
            CloudOutput::Document {
                markdown: "## Title\n\nBody".into(),
                // 表格没有编号，排在列表里它前面那个有编号的块（标题，1）之后
                blocks: vec!["Title".into(), "a\tb".into(), "Body".into()],
            }
        );
        // OCR 类模型的结果里同时有两种时按文本行处理
        assert!(matches!(output(&[&r], false), Some(CloudOutput::Lines(_))));
        assert_eq!(output(&[&json!({"other": 1})], true), None);
    }
}
