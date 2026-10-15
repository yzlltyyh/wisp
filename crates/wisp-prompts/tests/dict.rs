//! 词典式触发判定表。

use std::time::Instant;

use wisp_prompts::{builtin_styles, is_dictionary_input, use_dictionary};
use wisp_types::{DictMode, Style};

/// (输入, 是否触发, 说明)
const TABLE: &[(&str, bool, &str)] = &[
    // —— 触发 ——
    ("token", true, "S1 单词"),
    ("rule of thumb", true, "S2：3 个词"),
    ("内卷", true, "S3：2 个汉字"),
    ("useEffect", true, "标识符"),
    ("serendipity", true, "单个英文词"),
    ("缘分", true, "2 个汉字"),
    ("苹果", true, "常见词也触发"),
    ("经验法则", true, "4 个汉字"),
    ("  token  ", true, "首尾空白先 trim"),
    ("\n\ntoken\n", true, "首尾空行先 trim"),
    ("state-of-the-art", true, "连字符不拆词"),
    ("don't", true, "撇号不拆词"),
    ("rock 'n' roll", true, "撇号不拆词：3 个词"),
    ("Node.js", true, "夹在字母间的点不是句末标点"),
    ("v1.2", true, "版本号里的点"),
    ("and/or", true, "只有 1 个斜杠"),
    ("TCP/IP", true, "只有 1 个斜杠"),
    ("H2O", true, "字母 + 数字"),
    ("naïve", true, "带变音符号的拉丁字母"),
    ("「内卷」", true, "全角引号也算非空白字符：共 4 个"),
    ("内 卷", true, "CJK 非空白字符 2 个"),
    ("ngl 💀", true, "emoji 不算字母"),
    ("Привет", true, "其他字母文字的单词"),
    ("abcdefghijklmnopqrstuvwxyzabcd", true, "正好 30 字符"),
    ("token, coin", true, "逗号不是句末标点"),
    // —— 不触发 ——
    ("Hello, world.", false, "带句号"),
    ("Hello.", false, "带句号"),
    ("Really?", false, "问号"),
    ("Wow!", false, "感叹号"),
    ("真的吗？", false, "全角问号"),
    ("好。", false, "全角句号"),
    ("e.g.", false, "以点结尾"),
    ("one two three four", false, "4 个词"),
    ("abcdefghijklmnopqrstuvwxyzabcde", false, "31 字符"),
    ("ab cd efghijklmnopqrstuvwxyzabcd", false, "3 个词但 32 字符"),
    ("https://example.com", false, "URL"),
    ("http://localhost:5173", false, "URL"),
    ("www.example.com", false, "www. 开头"),
    ("/usr/bin", false, "绝对路径"),
    ("~/Documents", false, "家目录路径"),
    ("./src", false, "相对路径"),
    ("../lib", false, "上级路径"),
    ("C:\\Windows", false, "盘符"),
    ("D:/data", false, "盘符（正斜杠）"),
    ("src/lib/mod", false, "2 个斜杠"),
    ("a\\b\\c", false, "2 个反斜杠"),
    ("划词翻译功能", false, "6 个汉字"),
    ("五个汉字啊", false, "5 个汉字"),
    ("PR 合并", false, "中英混排"),
    ("用token", false, "中英混排"),
    ("12345", false, "纯数字"),
    ("3.14", false, "纯数字"),
    ("+-*/", false, "纯符号"),
    ("💀", false, "只有 emoji"),
    ("token\ncoin", false, "两行"),
    ("token\r\ncoin", false, "两行（CRLF）"),
    ("", false, "空"),
    ("   ", false, "只有空白"),
];

#[test]
fn dictionary_trigger_table() {
    let mut failures = Vec::new();
    for &(text, want, note) in TABLE {
        if is_dictionary_input(text) != want {
            failures.push(format!("{note}: {text:?} 期望 {want}"));
        }
    }
    assert!(failures.is_empty(), "词典式判定表失败：\n{}", failures.join("\n"));
}

fn style(id: &str) -> Style {
    builtin_styles().into_iter().find(|s| s.id == id).expect(id)
}

#[test]
fn style_switch_and_force_modes() {
    // literal 下不触发；其余 5 个内置风格触发
    for s in builtin_styles() {
        assert_eq!(
            use_dictionary("token", &s, DictMode::Auto),
            s.id != "literal",
            "{}",
            s.id
        );
    }
    let literal = style("literal");
    let natural = style("natural");
    // 强制模式优先于风格开关与输入形态
    assert!(use_dictionary("token", &literal, DictMode::Dictionary));
    assert!(use_dictionary("a whole sentence here.", &natural, DictMode::Dictionary));
    assert!(!use_dictionary("token", &natural, DictMode::Prose));
    assert!(!use_dictionary("one two three four", &natural, DictMode::Auto));
    // 自定义风格默认开启词典式
    let custom = Style::custom("custom-1", "C");
    assert!(use_dictionary("内卷", &custom, DictMode::Auto));
}

#[test]
fn decision_is_fast() {
    // < 10 µs；长文本走快速拒绝路径
    let long = "word ".repeat(2000);
    let inputs = [
        "token",
        "rule of thumb",
        "内卷",
        "https://example.com/a/b",
        long.as_str(),
    ];
    let rounds = 2000u32;
    let start = Instant::now();
    for _ in 0..rounds {
        for t in inputs {
            std::hint::black_box(is_dictionary_input(std::hint::black_box(t)));
        }
    }
    let per_call = start.elapsed() / (rounds * inputs.len() as u32);
    // 发布构建的实测值见 benches/assemble.rs；调试构建 + 并行编译的抖动下留出 5 倍余量
    assert!(per_call.as_micros() < 50, "is_dictionary_input 平均 {per_call:?}");
}
