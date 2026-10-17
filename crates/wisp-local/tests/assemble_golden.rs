//! 文本拼装的黄金输出：把官方 PP-OCRv6 产线对真实截图的识别结果（`tests/fixtures/ppocr/*.json`，含低分行）与一组
//! 构造的版面（本文件的 [`synthetic`]：跨行断词、中英混排、韩文、列表、多栏）分别按 lines 与 paragraphs 模式拼装，
//! 与 `tests/fixtures/assemble/<样例>.<模式>.txt` 逐字比对。
//!
//! 拼接规则改动时，这些文件的差异就是用户能看到的行为变化，逐条审阅、确认符合预期后运行
//! `UPDATE_GOLDEN=1 cargo test -p wisp-local --test assemble_golden` 重写（同时删除不再对应样例的旧文件）。
//! 文件内容是拼装结果再加一个结尾换行，方便编辑器与 diff 显示；结果里的制表符是分栏，原样保留。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use wisp_local::ocr::TextLine;
use wisp_local::ocr::assemble::assemble;
use wisp_types::OcrMode;

/// 参与比对的模式（markdown 与 translate 退回 paragraphs，不单独比对）
const MODES: [(&str, OcrMode); 2] = [("lines", OcrMode::Lines), ("paragraphs", OcrMode::Paragraphs)];

fn ppocr_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ppocr")
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/assemble")
}

fn update_mode() -> bool {
    std::env::var("UPDATE_GOLDEN").is_ok_and(|v| !v.is_empty() && v != "0")
}

#[derive(Deserialize)]
struct Fixture {
    lines: Vec<FixtureLine>,
}

#[derive(Deserialize)]
struct FixtureLine {
    #[serde(rename = "box")]
    poly: [[f32; 2]; 4],
    text: String,
    score: f32,
}

/// 真实截图的识别结果：`<图片名>.<档位>` → 文本行
fn ppocr_inputs() -> Vec<(String, Vec<TextLine>)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(ppocr_dir()).expect("读取 fixtures/ppocr") {
        let path = entry.expect("目录项").path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).expect("文件名").to_owned();
        let bytes = std::fs::read(&path).expect("读取样例");
        let fixture: Fixture =
            serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("解析 {}：{e}", path.display()));
        let lines = fixture
            .lines
            .into_iter()
            .map(|l| TextLine {
                text: l.text,
                score: l.score,
                poly: l.poly,
            })
            .collect();
        out.push((stem, lines));
    }
    assert!(out.len() >= 12, "fixtures/ppocr 下的样例少于预期：{}", out.len());
    out
}

/// 一个横平竖直的文本框
fn line(text: &str, x0: f32, y0: f32, x1: f32, y1: f32) -> TextLine {
    TextLine {
        text: text.to_owned(),
        score: 0.95,
        poly: [[x0, y0], [x1, y0], [x1, y1], [x0, y1]],
    }
}

/// 一栏正文：每行一个文本框，行高 20、行距 24，左边界对齐；`full` 为 true 的行写满栏宽（480），否则按字数估计宽度。
/// 真实截图里很少出现行尾断词、韩文折行这类情况，这里构造出来，让跨行拼接规则的变化在黄金输出里看得到
fn column(x0: f32, y0: f32, rows: &[(&str, bool)]) -> Vec<TextLine> {
    rows.iter()
        .enumerate()
        .map(|(i, &(text, full))| {
            let top = y0 + 24.0 * i as f32;
            let width = if full {
                480.0
            } else {
                // 粗略的字宽：全角字 18、其余 9
                text.chars().map(|c| if c.is_ascii() { 9.0 } else { 18.0 }).sum::<f32>()
            };
            line(text, x0, top, x0 + width, top + 20.0)
        })
        .collect()
}

/// 构造的版面：`<名字>` → 文本行
fn synthetic() -> Vec<(String, Vec<TextLine>)> {
    let cases: Vec<(&str, Vec<TextLine>)> = vec![
        (
            "hyphenation",
            [
                column(
                    10.0,
                    10.0,
                    &[
                        ("The protocol negotiates a shared inter-", true),
                        ("face before any data is sent. Since COVID-", true),
                        ("19 the team works remotely using Wi-", true),
                        ("Fi hotspots and a well-", true),
                        ("known VPN; see https://example.com/long-", true),
                        ("path for details.", false),
                    ],
                ),
                column(
                    10.0,
                    200.0,
                    &[
                        ("Version 2020-", true),
                        ("2021 changed the API. The value is -", true),
                        ("5 degrees and the range is 3 -", true),
                        ("7 units.", false),
                    ],
                ),
            ]
            .concat(),
        ),
        (
            "cjk-latin",
            [
                column(
                    10.0,
                    10.0,
                    &[
                        ("常驻后台的工具首先要安静：空闲时几乎不占用", true),
                        ("CPU，也不抢走焦点。使用 Wisp 时，划词翻译", true),
                        ("会在光标旁弹出面板，支持 OpenAI 和", true),
                        ("Anthropic 等服务。", false),
                    ],
                ),
                column(
                    10.0,
                    200.0,
                    &[
                        ("他说——", true),
                        ("我们明天再谈。「引号」与（括号）在行尾", true),
                        ("（同样）不加空格。", false),
                    ],
                ),
                column(
                    10.0,
                    300.0,
                    &[
                        ("スクリーンショットから文字を認識して、", true),
                        ("クリップボードにコピーします。", false),
                    ],
                ),
            ]
            .concat(),
        ),
        (
            "hangul",
            [
                column(
                    10.0,
                    10.0,
                    &[
                        ("화면의 글자를 인식해서 클립보드에", true),
                        ("복사합니다. 번역 결과는 패널에", true),
                        ("표시됩니다.", false),
                    ],
                ),
                column(
                    10.0,
                    200.0,
                    &[("Wisp는 백그라운드에서 조용히", true), ("실행됩니다.", false)],
                ),
            ]
            .concat(),
        ),
        (
            // 每一项的上一行都写满了，是不是列表项只取决于这一行的开头
            "lists",
            column(
                10.0,
                10.0,
                &[
                    ("Steps to follow before the release goes out, in order:", true),
                    ("1. Update the version number in every manifest", true),
                    ("and commit the change.", false),
                    ("2) Run the checks and wait for every job to pass.", true),
                    ("(3) Tag the release and push the tag to the remote.", true),
                    ("• Bullet item that is long enough to fill the line.", true),
                    ("- Dash item that is long enough to fill the line.", true),
                    ("① 第一项写满了整整一行的宽度，后面紧跟着下一项", true),
                    ("⑴ 带括号的编号也写满了一整行的宽度", true),
                    ("❶ 黑底圆圈的编号同样写满了一整行", true),
                    ("一、第一节的内容写满了整整一行的宽度", true),
                    ("a. lettered item that also fills the whole line", true),
                    ("1.2.3 Multi-level heading numbers fill the line", true),
                    ("1.5 GB is not a list item but it does start the", true),
                    ("line with a number.", false),
                ],
            ),
        ),
        (
            // 短行成组：设置页的标签与按钮、聊天里的昵称、短消息与时间。紧挨着的短行之间只换行；隔开超过一个行高、
            // 或者有一边是多行的正文时照常空一行
            "groups",
            [
                vec![
                    line("Settings", 10.0, 10.0, 90.0, 30.0),
                    line("General", 10.0, 60.0, 80.0, 80.0),
                    line("Shortcuts", 10.0, 88.0, 95.0, 108.0),
                    line("Models", 10.0, 116.0, 70.0, 136.0),
                    line("Save", 300.0, 116.0, 340.0, 136.0),
                    line("Cancel", 360.0, 116.0, 420.0, 136.0),
                    line("Alice", 10.0, 180.0, 60.0, 200.0),
                ],
                column(
                    10.0,
                    204.0,
                    &[
                        ("Are we still meeting tomorrow at three? I booked the", true),
                        ("small room on the second floor.", false),
                    ],
                ),
                vec![
                    line("10:24", 10.0, 252.0, 50.0, 272.0),
                    line("Sounds good!", 380.0, 280.0, 490.0, 300.0),
                    line("10:25", 450.0, 304.0, 490.0, 324.0),
                ],
            ]
            .concat(),
        ),
        (
            "columns",
            vec![
                line("Name", 10.0, 10.0, 60.0, 30.0),
                line("Age", 200.0, 10.0, 240.0, 30.0),
                line("City", 320.0, 10.0, 365.0, 30.0),
                line("Alice", 10.0, 40.0, 60.0, 60.0),
                line("30", 200.0, 40.0, 225.0, 60.0),
                line("Paris", 320.0, 40.0, 370.0, 60.0),
                // 同一视觉行内贴得很近的片段：中英之间按间距决定空格
                line("版本", 10.0, 90.0, 46.0, 110.0),
                line("v0.1", 48.0, 90.0, 84.0, 110.0),
                line("发布于", 100.0, 90.0, 154.0, 110.0),
                line("2026", 166.0, 90.0, 202.0, 110.0),
                line("年", 204.0, 90.0, 222.0, 110.0),
                line("fn main() {", 10.0, 150.0, 120.0, 170.0),
                line("let x = 1;", 50.0, 172.0, 150.0, 192.0),
                line("}", 10.0, 194.0, 20.0, 214.0),
            ],
        ),
    ];
    cases
        .into_iter()
        .map(|(name, lines)| (format!("synthetic-{name}"), lines))
        .collect()
}

#[test]
fn assembled_text_matches_golden_files() {
    let mut expected: BTreeMap<String, String> = BTreeMap::new();
    for (name, lines) in ppocr_inputs().into_iter().chain(synthetic()) {
        for (mode_name, mode) in MODES {
            let text = assemble(&lines, mode);
            expected.insert(format!("{name}.{mode_name}.txt"), format!("{text}\n"));
        }
    }

    let dir = golden_dir();
    let existing: Vec<String> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok()?.file_name().into_string().ok())
                .filter(|n| n.ends_with(".txt"))
                .collect()
        })
        .unwrap_or_default();

    if update_mode() {
        std::fs::create_dir_all(&dir).expect("创建 fixtures/assemble");
        for (name, text) in &expected {
            std::fs::write(dir.join(name), text).expect("写入黄金输出");
        }
        for stale in existing.iter().filter(|n| !expected.contains_key(*n)) {
            std::fs::remove_file(dir.join(stale)).expect("删除旧的黄金输出");
        }
        return;
    }

    let mut problems = Vec::new();
    for (name, text) in &expected {
        let actual = std::fs::read_to_string(dir.join(name)).unwrap_or_default();
        if &actual != text {
            // 只报第一处不同的行，完整差异用 UPDATE_GOLDEN=1 重写后看 git diff
            let at = actual
                .split('\n')
                .zip(text.split('\n'))
                .position(|(a, b)| a != b)
                .unwrap_or_else(|| actual.split('\n').count().min(text.split('\n').count()));
            let golden_line = actual.split('\n').nth(at).unwrap_or("<缺>");
            let new_line = text.split('\n').nth(at).unwrap_or("<缺>");
            problems.push(format!(
                "{name} 第 {} 行：\n  黄金：{golden_line:?}\n  现在：{new_line:?}",
                at + 1
            ));
        }
    }
    for stale in existing.iter().filter(|n| !expected.contains_key(*n)) {
        problems.push(format!("{stale}：没有对应的样例"));
    }
    assert!(
        problems.is_empty(),
        "拼装结果与黄金输出不一致（审阅后运行 UPDATE_GOLDEN=1 cargo test -p wisp-local --test assemble_golden 重写）：\n{}",
        problems.join("\n")
    );
}
