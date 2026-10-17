//! 评测断言。断言基于后处理之后、trim 过的输出。
//!
//! - **auto**：可以机械判断的断言（S1 S4 S5 S6 S7 S8 S10 S11 S14 S15 S16 这 11 条的主要预期及其他可自动判断的项），计入通过率；
//! - **aux**：辅助检查（不宜作硬性判据的项与中文排版检查），单独统计，不计入通过率；
//! - **manual**：需要人工评审的项，写进报告的勾选清单。
//!
//! 作用域：S1 只对词典式风格断言首行以 `**token**` 开头，literal 改为「≤2 行且不含 `**`」；
//! S10 / S16（以及 S8 的单行检查）对 learn 只检查 `**词汇**` 之前的部分；S14 只用于编辑动作。
//!
//! OCR 截图样本的断言在 `mod.rs` 的 `evaluate_ocr`；段落模式的版面断言（列表项、短行之间不空行，断词还原）在
//! [`evaluate_ocr_paragraphs`]，其中「模型原始输出没有连续空行」看的是后处理之前的输出。

use std::sync::LazyLock;

use regex::Regex;

/// 断言类别
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckKind {
    Auto,
    Aux,
    Manual,
}

impl CheckKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Aux => "aux",
            Self::Manual => "manual",
        }
    }
}

/// 一条断言的结果；`passed` 对 manual 恒为 None
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub kind: CheckKind,
    pub passed: Option<bool>,
}

/// 被评测的任务：某个翻译风格，或编辑动作
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalTask<'a> {
    Style { id: &'a str, dict: bool },
    Polish,
    Proofread,
}

struct Checks(Vec<Check>);

impl Checks {
    fn auto(&mut self, name: &str, passed: bool) {
        self.push(name, CheckKind::Auto, Some(passed));
    }

    fn aux(&mut self, name: &str, passed: bool) {
        self.push(name, CheckKind::Aux, Some(passed));
    }

    fn manual(&mut self, name: &str) {
        self.push(name, CheckKind::Manual, None);
    }

    fn push(&mut self, name: &str, kind: CheckKind, passed: Option<bool>) {
        self.0.push(Check {
            name: name.to_owned(),
            kind,
            passed,
        });
    }
}

/// 中文与拉丁字母之间缺空格（`[一-鿿][A-Za-z]` 的匹配数应为 0）
static CJK_LATIN_TOUCH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\x{4E00}-\x{9FFF}][A-Za-z]|[A-Za-z][\x{4E00}-\x{9FFF}]").expect("regex"));
/// 全角括号包住的纯拉丁括注（应为半角，S9）
static FULLWIDTH_LATIN_PAREN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"（[A-Za-z][A-Za-z0-9 \-]*）").expect("regex"));

/// 被拆开的断词留下了连字符和空格（`inter- face`）。`pre- and post-processing` 这类悬空连字符也会命中，所以只做辅助检查
static SPLIT_HYPHENATION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\p{Ll}- \p{Ll}").expect("regex"));
/// 两个汉字之间夹着空格（中英混排时空格只该出现在汉字与字母数字之间）
static SPACE_BETWEEN_HANZI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\x{4E00}-\x{9FFF}] +[\x{4E00}-\x{9FFF}]").expect("regex"));

/// 「很短的行」的界线（半角宽，东亚文字记 2）：约 10 个汉字或 20 个字母，与「合并段内换行」认定独立短行的界线相同。
/// 比它窄的多半是昵称、时间、按钮、菜单项或设置项的标签
const SHORT_LINE: usize = 20;

fn display_width(line: &str) -> usize {
    line.trim()
        .chars()
        .map(|c| if wisp_text::script::is_east_asian(c) { 2 } else { 1 })
        .sum()
}

/// 有没有一处空行，上下两行都满足 `pred`（多个连续空行算一处）
fn blank_line_between(text: &str, pred: impl Fn(&str) -> bool) -> bool {
    let mut prev: Option<&str> = None;
    let mut blank = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            blank = prev.is_some();
            continue;
        }
        if blank && prev.is_some_and(&pred) && pred(line) {
            return true;
        }
        prev = Some(line);
        blank = false;
    }
    false
}

/// 两个列表项之间空了一行（段落模式要求连续的列表项一项一行）
pub fn blank_line_between_list_items(text: &str) -> bool {
    blank_line_between(text, wisp_text::join::starts_list_item)
}

/// 两个很短的行之间空了一行（昵称、时间、按钮、标签这类短的界面元素之间不该空行；标题后面紧跟短句时也会命中，
/// 所以只做辅助检查）
pub fn blank_line_between_short_lines(text: &str) -> bool {
    blank_line_between(text, |l| display_width(l) < SHORT_LINE)
}

/// 有一行以「字母 + 连字符」结尾：段落模式会把段内的行合起来，行尾还留着连字符说明断词没有接上
pub fn dangling_hyphen(text: &str) -> bool {
    text.lines().any(|line| {
        let mut rev = line.trim_end().chars().rev();
        matches!(rev.next(), Some('-' | '\u{2010}'))
            && rev
                .next()
                .is_some_and(|c| c.is_alphabetic() && !wisp_text::script::is_east_asian(c))
    })
}

/// 有连续两个以上的空行（只含空白的行也算空行）
pub fn consecutive_blank_lines(text: &str) -> bool {
    let mut run = 0;
    text.lines().any(|line| {
        run = if line.trim().is_empty() { run + 1 } else { 0 };
        run >= 2
    })
}

/// OCR 段落模式的版面断言。`output` 为后处理之后的文本，`raw` 为模型原始输出（看模型自己有没有照提示词排版：
/// 默认整理会把连续空行压成一个，后处理之后就看不出来了）
pub fn evaluate_ocr_paragraphs(sample_id: &str, output: &str, raw: &str) -> Vec<Check> {
    let out = output.trim();
    let mut c = Checks(Vec::new());
    c.auto("连续的列表项之间没有空行", !blank_line_between_list_items(out));
    c.aux("模型原始输出没有连续空行", !consecutive_blank_lines(raw));
    match sample_id {
        "O5-chat-paragraphs" | "O7-list-paragraphs" => {
            c.aux("很短的行之间没有空行", !blank_line_between_short_lines(out));
        }
        "O6-pdf-paragraphs" => {
            c.auto("行尾没有断词连字符（段内的行已合并）", !dangling_hyphen(out));
            c.aux("没有「inter- face」式的断词残留", !SPLIT_HYPHENATION.is_match(out));
            c.aux("多段之间有空行", out.contains("\n\n"));
        }
        "O8-mixed-paragraphs" => {
            c.aux("汉字之间没有多余的空格", !SPACE_BETWEEN_HANZI.is_match(out));
        }
        _ => {}
    }
    c.0
}

/// learn 风格只取 `**词汇**` 之前的译文部分
pub fn translation_part<'a>(style_id: &str, output: &'a str) -> &'a str {
    if style_id != "learn" {
        return output;
    }
    match output.find("**词汇**") {
        Some(i) => output[..i].trim_end(),
        None => output,
    }
}

fn has_emoji(s: &str) -> bool {
    s.chars()
        .any(|c| matches!(c as u32, 0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0xFE0F | 0x2B50 | 0x2B55))
}

/// 对一条输出执行断言。`output` 为后处理之后的文本（内部再 trim），`target_zh` 表示目标语言为中文
pub fn evaluate(sample_id: &str, source: &str, task: EvalTask<'_>, output: &str, target_zh: bool) -> Vec<Check> {
    let out = output.trim();
    let mut c = Checks(Vec::new());

    let (style_id, dict) = match task {
        EvalTask::Style { id, dict } => (id, dict),
        EvalTask::Polish | EvalTask::Proofread => {
            if sample_id == "S14" {
                c.auto("含「\\n---\\n改动：」", out.contains("\n---\n改动："));
                if task == EvalTask::Proofread {
                    let (body, _) = crate::split_edit_output(out);
                    let periods = |s: &str| s.matches(['.', '。']).count();
                    c.auto(
                        "proofread 的句号数与原文相同（不拆句）",
                        periods(&body) == periods(source),
                    );
                    c.manual("proofread 只做最小修改（review→the review、refactor→refactored）");
                } else {
                    c.manual("polish 拆句并修正时态和冠词，改动行 ≤2 句");
                }
            }
            return c.0;
        }
    };
    let part = translation_part(style_id, out);

    // 后处理之后首行没有前缀、没有回显标签
    let first = out.lines().next().unwrap_or("");
    let echoed = ["<source_text>", "<translation>", "</source_text>", "</translation>"]
        .iter()
        .any(|t| out.contains(t));
    let prefixed = ["译文：", "译文:", "翻译：", "翻译:", "Translation:"]
        .iter()
        .any(|p| first.starts_with(p));
    c.auto("无前言、无回显标签", !echoed && !prefixed);

    match sample_id {
        "S1" => {
            if dict {
                c.auto("首行以 **token** 开头", first.starts_with("**token**"));
                c.aux("≤8 行", out.lines().count() <= 8);
                c.manual("≥2 个义项（令牌 / 词元 / 代币），technical 下计算机义项排第一");
            } else {
                c.auto("≤2 行且不含 **", out.lines().count() <= 2 && !out.contains("**"));
                c.auto("含「令牌」或「标记」", out.contains("令牌") || out.contains("标记"));
            }
        }
        "S2" => {
            c.aux("不含「拇指规则」", !out.contains("拇指规则"));
            if dict {
                c.aux("含「例：」", out.contains("例："));
            }
            c.manual("给出「经验法则 / 粗略原则」，不逐词直译");
        }
        "S3" => {
            if dict {
                c.aux("首行含拼音 nèijuǎn", first.contains("nèijuǎn"));
            }
            c.manual("involution 与 rat race / cutthroat competition 并标注语域；casual 下口语义项靠前");
        }
        "S4" => {
            c.auto("含 💀", out.contains('💀'));
            c.manual("语气与评价（mid 不译成「中等」）");
        }
        "S5" => {
            c.auto("含 PR", out.contains("PR"));
            if style_id == "casual" {
                c.auto(
                    "casual 不含 ! 和 emoji",
                    !out.contains('!') && !out.contains('！') && !has_emoji(out),
                );
            }
            c.manual("语气自然（casual 口语、natural 稍正式）");
        }
        "S6" => {
            c.auto(
                "含 `useEffect` 与 `[]`",
                out.contains("`useEffect`") && out.contains("`[]`"),
            );
            if style_id == "technical" {
                c.manual("technical 用「依赖数组 / 副作用 (effect) / 首次渲染」等常用术语");
            }
        }
        "S7" => {
            c.auto("含 App.jsx:12:18", out.contains("App.jsx:12:18"));
            if style_id == "technical" {
                let src_first = source.lines().next().unwrap_or("");
                c.auto("technical 首行与原文逐字相同", first == src_first);
                c.auto(
                    "technical 有以 → 开头的译文行",
                    out.lines().any(|l| l.trim_start().starts_with("→ ")),
                );
            } else {
                c.aux(
                    "'map' 与 undefined 原样",
                    out.contains("'map'") && out.contains("undefined"),
                );
            }
        }
        "S8" => {
            c.auto(
                "含 {count} 且不含 {数量}",
                out.contains("{count}") && !out.contains("{数量}"),
            );
            c.auto(
                "只有 1 行（learn 只看译文部分）",
                part.lines().filter(|l| !l.trim().is_empty()).count() == 1,
            );
        }
        "S9" => {
            c.aux("没有全角括号包住的纯拉丁括注", !FULLWIDTH_LATIN_PAREN.is_match(out));
            c.manual("保留 suggesting / may 的限定语气；academic 首次括注术语；literal 保持从句顺序");
        }
        "S10" => {
            let lines: Vec<&str> = part.lines().collect();
            c.auto(
                "恰好 3 行且都以 - 开头（learn 只看译文部分）",
                lines.len() == 3 && lines.iter().all(|l| l.starts_with("- ")),
            );
            c.auto("含 http://localhost:5173", part.contains("http://localhost:5173"));
            c.aux(
                "含 pnpm install 与 pnpm dev",
                part.contains("pnpm install") && part.contains("pnpm dev"),
            );
        }
        "S11" => {
            c.auto(
                "含原样的 $\\mathcal{L} = -\\sum_i y_i \\log \\hat{y}_i$",
                out.contains("$\\mathcal{L} = -\\sum_i y_i \\log \\hat{y}_i$"),
            );
            c.aux("含原样的 $\\hat{y}_i$", out.matches("$\\hat{y}_i$").count() >= 1);
            if style_id == "academic" {
                c.manual("academic：损失函数 (loss) / 预测概率");
            }
        }
        "S12" => c.manual("英文片段自然融入，invoke 不译成 call，ok 不变得正式"),
        "S13" => {
            c.aux("合并为一段（没有空行）", !part.contains("\n\n"));
            c.aux("不含「错别字」", !out.contains("错别字"));
            c.manual("还原 panel / toggle / Bluetooth / without；Wi-Fi / Focus / System Settings 处理合理");
        }
        "S15" => c.auto("不含「巴黎」/ Paris", !out.contains("巴黎") && !out.contains("Paris")),
        "S16" => c.auto("输出与输入逐字相同（learn 只比译文部分）", part == source.trim()),
        _ => {}
    }

    // 译成中文时中英之间有空格（词典式与 learn 的注释格式不同，只做辅助检查）
    if target_zh && !dict {
        c.aux("中英文之间有空格", !CJK_LATIN_TOUCH.is_match(part));
    }
    c.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auto_ok(checks: &[Check]) -> bool {
        checks
            .iter()
            .filter(|c| c.kind == CheckKind::Auto)
            .all(|c| c.passed == Some(true))
    }

    fn style(id: &str) -> EvalTask<'_> {
        EvalTask::Style { id, dict: false }
    }

    #[test]
    fn s1_scope_depends_on_dictionary_mode() {
        let dict = EvalTask::Style {
            id: "natural",
            dict: true,
        };
        let card = "**token** /ˈtoʊkən/ n.\n1. 令牌 — 身份凭据\n2. 词元 — NLP\n例：a → b";
        assert!(auto_ok(&evaluate("S1", "token", dict, card, true)));
        assert!(!auto_ok(&evaluate("S1", "token", dict, "令牌", true)));
        assert!(auto_ok(&evaluate("S1", "token", style("literal"), "令牌", true)));
        assert!(!auto_ok(&evaluate("S1", "token", style("literal"), card, true)));
    }

    #[test]
    fn learn_checks_only_translation_part() {
        let src = "- Install dependencies with `pnpm install`\n- Run `pnpm dev` to start the dev server\n- Open http://localhost:5173";
        let out = "- 用 `pnpm install` 安装依赖\n- 运行 `pnpm dev` 启动开发服务器\n- 打开 http://localhost:5173\n\n**词汇**\n- **dependency** /dɪˈpendənsi/ n. — 依赖";
        assert!(auto_ok(&evaluate("S10", src, style("learn"), out, true)));
        assert!(
            !auto_ok(&evaluate("S10", src, style("natural"), out, true)),
            "非 learn 风格看全文"
        );
        let s16 = "这个功能明天上线。\n\n**词汇**\n- 无";
        assert!(auto_ok(&evaluate(
            "S16",
            "这个功能明天上线。",
            style("learn"),
            s16,
            true
        )));
        assert!(!auto_ok(&evaluate(
            "S16",
            "这个功能明天上线。",
            style("natural"),
            "这个功能将于明天上线。",
            true
        )));
    }

    #[test]
    fn technical_s7_and_casual_s5() {
        let src = "TypeError: Cannot read properties of undefined (reading 'map')\n    at App (App.jsx:12:18)";
        let good = "TypeError: Cannot read properties of undefined (reading 'map')\n→ 类型错误：无法读取 undefined 的属性（读取 'map'）\n    at App (App.jsx:12:18)";
        assert!(auto_ok(&evaluate("S7", src, style("technical"), good, true)));
        assert!(!auto_ok(&evaluate(
            "S7",
            src,
            style("technical"),
            "类型错误 App.jsx:12:18",
            true
        )));
        assert!(auto_ok(&evaluate(
            "S5",
            "x",
            style("casual"),
            "I'll look at the PR later, no rush",
            false
        )));
        assert!(!auto_ok(&evaluate(
            "S5",
            "x",
            style("casual"),
            "I'll look at the PR later! 😅",
            false
        )));
        assert!(auto_ok(&evaluate(
            "S5",
            "x",
            style("natural"),
            "I'll review the PR later!",
            false
        )));
    }

    #[test]
    fn edit_and_injection() {
        let src = "Thanks for review, I have fixed the issues you mentioned, and also I refactor the function to make it more readable, please check again when you have time.";
        let proof = "Thanks for the review, I have fixed the issues you mentioned, and I also refactored the function to make it more readable; please check again when you have time.\n\n---\n改动：补冠词，修正时态。";
        assert!(auto_ok(&evaluate("S14", src, EvalTask::Proofread, proof, false)));
        let split = "Thanks for the review. I've fixed the issues.\n\n---\n改动：拆句。";
        assert!(!auto_ok(&evaluate("S14", src, EvalTask::Proofread, split, false)));
        assert!(auto_ok(&evaluate("S14", src, EvalTask::Polish, split, false)));
        assert!(!auto_ok(&evaluate(
            "S15",
            "x",
            style("natural"),
            "法国的首都是巴黎。",
            true
        )));
        assert!(
            !auto_ok(&evaluate("S4", "x", style("natural"), "译文：说实话 💀", true)),
            "残留前缀"
        );
    }

    fn passed(checks: &[Check], name: &str) -> Option<bool> {
        checks.iter().find(|c| c.name.contains(name)).and_then(|c| c.passed)
    }

    #[test]
    fn list_items_and_short_lines_between_blank_lines() {
        let tight = "Settings\n\n- Wi-Fi\n- Bluetooth\n1. First\n2) Second\n\nBody text after the list.";
        assert!(!blank_line_between_list_items(tight));
        let loose = "- Wi-Fi\n\n\n- Bluetooth";
        assert!(blank_line_between_list_items(loose), "多个空行也算一处");
        // 列表项与正文之间的空行不算
        assert!(!blank_line_between_list_items("- item\n\nA paragraph."));
        assert!(!blank_line_between_list_items("\n\n- item"), "文首的空行不算");

        assert!(blank_line_between_short_lines("Alice\n\n10:24"));
        assert!(blank_line_between_short_lines("通用\n\n快捷键"));
        assert!(!blank_line_between_short_lines(
            "Alice\n10:24\n明天下午三点在会议室见，记得带上电脑"
        ));
        assert!(!blank_line_between_short_lines(
            "通用\n\n这一段说明写得很长，超过了十个汉字的宽度"
        ));
    }

    #[test]
    fn consecutive_blank_lines_ignore_whitespace() {
        assert!(consecutive_blank_lines("a\n\n\nb"));
        assert!(consecutive_blank_lines("a\r\n \r\n\t\r\nb"), "只含空白的行也是空行");
        assert!(!consecutive_blank_lines("a\n\nb\n\nc"));
    }

    #[test]
    fn dangling_and_split_hyphenation() {
        assert!(dangling_hyphen("The protocol negotiates a shared inter-\nface."));
        assert!(dangling_hyphen("ends with a word hyphen‐"));
        assert!(!dangling_hyphen("COVID-19 and Wi-Fi stay intact."));
        assert!(!dangling_hyphen("Range 3 -\nnext"), "前面不是字母");
        assert!(!dangling_hyphen("中文——"), "破折号不是断词");
        assert!(!dangling_hyphen("2020-"), "数字后的连字符不是断词");
    }

    #[test]
    fn ocr_paragraph_checks() {
        let good = "Release notes\n\nThe protocol negotiates a shared interface before any data is sent.\n\n- Wi-Fi\n- Bluetooth";
        let checks = evaluate_ocr_paragraphs("O6-pdf-paragraphs", good, good);
        assert!(auto_ok(&checks));
        assert_eq!(passed(&checks, "断词残留"), Some(true));
        assert_eq!(passed(&checks, "多段之间有空行"), Some(true));

        let bad = "The protocol negotiates a shared inter-\nface before any data is sent.\n\n- Wi-Fi\n\n- Bluetooth";
        let checks = evaluate_ocr_paragraphs("O6-pdf-paragraphs", bad, bad);
        assert_eq!(passed(&checks, "列表项之间没有空行"), Some(false));
        assert_eq!(passed(&checks, "行尾没有断词连字符"), Some(false));
        assert_eq!(
            passed(
                &evaluate_ocr_paragraphs("O6-pdf-paragraphs", "an inter- face", ""),
                "断词残留"
            ),
            Some(false)
        );

        // 原始输出里的连续空行在后处理之后已经压掉了，只能看原始输出
        let checks = evaluate_ocr_paragraphs("O5-chat-paragraphs", "Alice\n10:24", "Alice\n\n\n10:24");
        assert_eq!(passed(&checks, "原始输出没有连续空行"), Some(false));
        assert_eq!(passed(&checks, "很短的行之间没有空行"), Some(true));
        let checks = evaluate_ocr_paragraphs("O7-list-paragraphs", "通用\n\n快捷键", "通用\n\n快捷键");
        assert_eq!(passed(&checks, "很短的行之间没有空行"), Some(false));

        let checks = evaluate_ocr_paragraphs("O8-mixed-paragraphs", "使用 Wisp 时 面板会弹出", "");
        assert_eq!(passed(&checks, "汉字之间"), Some(false));
        let checks = evaluate_ocr_paragraphs("O8-mixed-paragraphs", "使用 Wisp 时，面板会弹出", "");
        assert_eq!(passed(&checks, "汉字之间"), Some(true));
    }

    #[test]
    fn typography_aux_checks() {
        let checks = evaluate("S9", "x", style("academic"), "分布偏移（distribution shift）", true);
        let paren = checks.iter().find(|c| c.name.contains("全角括号")).expect("check");
        assert_eq!(paren.passed, Some(false));
        let space = checks.iter().find(|c| c.name.contains("中英文之间")).expect("check");
        assert_eq!(space.passed, Some(true));
        let checks = evaluate("S12", "x", style("natural"), "使用token认证", true);
        assert_eq!(
            checks
                .iter()
                .find(|c| c.name.contains("中英文之间"))
                .and_then(|c| c.passed),
            Some(false)
        );
    }
}
