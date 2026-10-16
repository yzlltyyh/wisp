//! 后处理逐字符喂入测试；`split_edit_output` 读取 `fixtures/edit_split.json`
//! （与 TS 共用）。
//!
//! 每个用例都按以下方式喂入，结果必须完全相同（「任意切分方式结果一致」）：整段一次、逐字符、所有二分切法、
//! 若干伪随机切法。

use wisp_prompts::{PostKind, PostProcessor, is_no_text, split_edit_output};
use wisp_text::lines::squeeze_blanks;
use wisp_types::OcrMode;

const TR: PostKind = PostKind::Translate;
const ED: PostKind = PostKind::Edit;

fn ocr(mode: OcrMode) -> PostKind {
    PostKind::Ocr { mode }
}

/// 按给定切分喂入，返回（拼接后的输出, no_text）
fn run(kind: PostKind, source: &str, chunks: &[&str]) -> (String, bool) {
    let mut p = PostProcessor::new(kind, source);
    let mut out = String::new();
    for c in chunks {
        if let Some(s) = p.push(c) {
            assert!(!s.is_empty(), "push 不应返回空串");
            out.push_str(&s);
        }
    }
    let f = p.finish();
    out.push_str(&f.tail);
    // finish 之后再调用都是空操作
    assert_eq!(p.finish(), Default::default());
    assert_eq!(p.push("x"), None);
    (out, f.no_text)
}

fn char_chunks(s: &str) -> Vec<&str> {
    s.char_indices().map(|(i, c)| &s[i..i + c.len_utf8()]).collect()
}

/// 简单的线性同余发生器，生成可复现的伪随机切分
fn random_chunks(s: &str, seed: u64) -> Vec<&str> {
    let bounds: Vec<usize> = s.char_indices().map(|(i, _)| i).skip(1).collect();
    let mut state = seed
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    let mut cuts = Vec::new();
    for b in bounds {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        if (state >> 33).is_multiple_of(4) {
            cuts.push(b);
        }
    }
    let mut out = Vec::new();
    let mut last = 0;
    for c in cuts {
        out.push(&s[last..c]);
        last = c;
    }
    out.push(&s[last..]);
    out
}

/// 所有切分方式下输出都等于 want
fn check(kind: PostKind, source: &str, raw: &str, want: &str, want_no_text: bool) {
    let expect = (want.to_owned(), want_no_text);
    assert_eq!(run(kind, source, &[raw]), expect, "整段：{raw:?}");
    assert_eq!(run(kind, source, &char_chunks(raw)), expect, "逐字符：{raw:?}");
    for (i, _) in raw.char_indices().skip(1) {
        assert_eq!(run(kind, source, &[&raw[..i], &raw[i..]]), expect, "二分 @{i}：{raw:?}");
    }
    for seed in 0..24 {
        assert_eq!(
            run(kind, source, &random_chunks(raw, seed)),
            expect,
            "随机切分 seed={seed}：{raw:?}"
        );
    }
}

fn check_tr(raw: &str, want: &str) {
    check(TR, "Hello", raw, want, false);
}

#[test]
fn translate_prefixes_are_stripped() {
    check_tr("译文：你好", "你好");
    check_tr("译文: 你好", "你好");
    check_tr("翻译：你好", "你好");
    check_tr("翻译:\n\n你好", "你好");
    check_tr("Translation: 你好", "你好");
    check_tr("TRANSLATION：你好", "你好");
    check_tr("translation :你好", "你好");
    check_tr("Here is the translation:\n\n你好", "你好");
    check_tr("Here's the translation: 你好", "你好");
    check_tr("Here’s the translation:\n你好", "你好");
    check_tr(
        "here is the translation into Simplified Chinese:\n\n你好，世界。",
        "你好，世界。",
    );
    check_tr("  \n\n译文：你好\n\n", "你好");
}

#[test]
fn translate_prefix_lookalikes_are_kept() {
    check_tr("Translation is not endorsement.", "Translation is not endorsement.");
    check_tr("Translations: a list", "Translations: a list");
    check_tr(
        "Here is the translation of the contract.\n合同",
        "Here is the translation of the contract.\n合同",
    );
    check_tr("译者：张三", "译者：张三");
    check_tr("Translation", "Translation");
    check_tr("译文", "译文");
    // 原文本身以前缀开头：不剥（例如原文就是「Translation: …」这样的定义）
    check(
        TR,
        "翻译：把文本译成另一种语言",
        "Translation: converting text",
        "Translation: converting text",
        false,
    );
}

#[test]
fn echoed_tags_are_stripped() {
    check_tr("<translation>你好</translation>", "你好");
    check_tr("<translation>\nHi\n</translation>", "Hi");
    check_tr("<TRANSLATION>Hi</Translation>\n", "Hi");
    check_tr("<source_text>\n你好\n</source_text>", "你好");
    check_tr("你好</translation>", "你好");
    // 闭合标签前后有多个空白 / CRLF：标签仍在滞留范围内，结尾剥掉，不多出换行
    check_tr("你好\n</translation>\n\n\n", "你好");
    check_tr("<translation>\n你好\n</translation>\n\n\n", "你好");
    check_tr("你好</translation>\r\n\r\n", "你好");
    check_tr("你好\n\n</translation>\n", "你好");
    check_tr("<translation>\r\n你好\r\n</translation>\r\n", "你好");
    check_tr("你好\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n</translation>", "你好");
    check_tr("“你好”\n\n\n\n</translation>\n\n\n\n", "你好");
    check_tr("<translation>译文：你好</translation>", "你好");
    // 中段出现的闭合标签不动；不是这两个标签的尖括号也不动
    check_tr("a</translation>b", "a</translation>b");
    check_tr("<div>hi</div>", "<div>hi</div>");
    check_tr("<t", "<t");
    check_tr("1 < 2", "1 < 2");
    // 原文含这些标签时不剥
    check(
        TR,
        "explain <translation> tags",
        "<translation> 标签",
        "<translation> 标签",
        false,
    );
}

#[test]
fn wrapping_quotes_are_stripped_only_when_paired() {
    check_tr("\"你好\"", "你好");
    check_tr("“你好”", "你好");
    check_tr("  \"你好\"  \n", "你好");
    check_tr("\"你好\"</translation>", "你好");
    check_tr("Translation: \"你好\"", "你好");
    let long = "这是一段比较长的译文，用来验证超过二十四个字符的引号包裹也能在结尾正确剥离。";
    check_tr(&format!("\"{long}\""), long);
    check_tr(&format!("“{long}”"), long);
    // 不成对、或闭合引号之后还有正文：不剥
    check_tr("\"最佳\"选项是 X", "\"最佳\"选项是 X");
    check_tr("\"A\" and \"B\"", "\"A\" and \"B\"");
    check_tr("“A”和“B”", "“A”和“B”");
    check_tr("\"unclosed", "\"unclosed");
    check_tr("\"", "\"");
    check_tr("“hi\"", "“hi\"");
    // 原文首尾有引号时保留
    check(TR, "\"hi\"", "\"你好\"", "\"你好\"", false);
    check(TR, "“Hello,” she said.", "“你好，”她说。", "“你好，”她说。", false);
}

#[test]
fn wrapping_fences_are_stripped_when_source_has_none() {
    check_tr("```\n你好\n```", "你好");
    check_tr("```text\n你好\n```\n", "你好");
    check_tr("```markdown\n- 列表\n- 第二项\n```", "- 列表\n- 第二项");
    check_tr("```\n第一行\n第二行\n```", "第一行\n第二行");
    // 只有开头围栏（被截断）：开头已剥离，正文照常输出
    check_tr("```\n你好", "你好");
    // 行内代码不是围栏
    check_tr("`useEffect` 是一个 Hook", "`useEffect` 是一个 Hook");
    check_tr("```js x\n", "```js x");
    // 原文有围栏：保留
    let src = "Run:\n```\nnpm i\n```";
    check(TR, src, "运行：\n```\nnpm i\n```", "运行：\n```\nnpm i\n```", false);
}

#[test]
fn unclosed_wrappers_are_removed_in_pairs() {
    // 开头引号之后 24 个字符内没有闭合引号：先剥开头，闭合引号出现在中段时一并去掉，不留孤立的引号
    check(
        TR,
        "银河系漫游指南是一部喜剧科幻小说。",
        "\"The Hitchhiker's Guide to the Galaxy\" is a comedy science fiction series.",
        "The Hitchhiker's Guide to the Galaxy is a comedy science fiction series.",
        false,
    );
    check_tr(
        "“一个超过二十四个字符的很长很长很长很长很长很长的书名”是一部小说。",
        "一个超过二十四个字符的很长很长很长很长很长很长的书名是一部小说。",
    );
    // 闭合引号落在窗口内且其后还有正文：不是包裹，原样保留
    check_tr(
        "“一个不到二十四个字符的书名”是一部小说。",
        "“一个不到二十四个字符的书名”是一部小说。",
    );
    // 长的包裹引号里有内层引号：内层成对保留，只去掉外层
    check_tr(
        "\"He told everyone in the meeting that \"we ship on Monday\" and then left.\"",
        "He told everyone in the meeting that \"we ship on Monday\" and then left.",
    );
    check_tr(
        "\"会议结束之后大家都离开了，只有他还留在会议室里说\"这个方案我们需要再讨论一下\"然后才走。\"",
        "会议结束之后大家都离开了，只有他还留在会议室里说\"这个方案我们需要再讨论一下\"然后才走。",
    );
    check_tr(
        "“会议结束之后大家都离开了，只有他还留在会议室里说“再讨论一下”然后才走。”",
        "会议结束之后大家都离开了，只有他还留在会议室里说“再讨论一下”然后才走。",
    );
    // 内层直引语长于 15 个字符（≤ 40）：内层开引号不会被误当成外层的闭合引号
    check_tr(
        "\"会议结束之后大家都离开了，只有他还留在会议室里说\"非常非常非常非常非常长的内层引号内容\"然后结束。\"",
        "会议结束之后大家都离开了，只有他还留在会议室里说\"非常非常非常非常非常长的内层引号内容\"然后结束。",
    );
    let inner = "非常".repeat(17) + "长的引号"; // 38 个字符
    check_tr(
        &format!("\"会议结束之后大家都离开了，只有他还留在会议室里说\"{inner}\"然后结束。\""),
        &format!("会议结束之后大家都离开了，只有他还留在会议室里说\"{inner}\"然后结束。"),
    );
    // 原文以书名号开头：引号规则关闭，译文的引号原样保留
    let s = "\"The Hitchhiker's Guide to the Galaxy\" is a comedy science fiction series.";
    check(TR, "《银河系漫游指南》是一部喜剧科幻小说。", s, s, false);
}

#[test]
fn code_blocks_followed_by_text_are_kept() {
    // 原文没有反引号（例如从网页复制的命令 + 说明），technical 输出代码块 + 说明：
    // 窗口内看到闭合围栏且其后还有正文 → 不剥，代码块完整保留
    let short = "```bash\nnpm i\n```\n然后运行 `npm run dev` 启动开发服务器。";
    check(TR, "npm i then run npm run dev", short, short, false);
    // 代码块超出窗口：开头围栏已剥，闭合围栏成对去掉（不留孤立的 ```，否则后文全部被当成代码）
    check(
        TR,
        "npm install then run the dev server",
        "```bash\nnpm install --save-dev typescript @types/node\n```\n然后运行 `npm run dev`。",
        "npm install --save-dev typescript @types/node\n然后运行 `npm run dev`。",
        false,
    );
    // 内层代码块（```js … ```）不会被当成外层的闭合围栏
    check_tr(
        "```markdown\n# 标题\n一段足够长、超过二十四个字符的说明文字。\n```js\nlet x = 1;\n```\n结尾\n```",
        "# 标题\n一段足够长、超过二十四个字符的说明文字。\n```js\nlet x = 1;\n```\n结尾",
    );
    // 四个反引号的外层围栏：内部三个反引号的代码块原样保留
    check_tr(
        "````markdown\n很长的一段说明文字，超过二十四个字符。\n```\ncode\n```\n````",
        "很长的一段说明文字，超过二十四个字符。\n```\ncode\n```",
    );
    // 围栏里套着引号，结尾两者都去掉
    check_tr("```\n\"你好\"\n```", "你好");
    check_tr("\"```\n你好\n```\"", "你好");
}

#[test]
fn edit_outputs_wrapped_before_the_changes_line() {
    // 编辑动作：包住正文的闭合符号位于 `\n---\n改动` 之前，也要成对去掉
    let long = "Thanks for the review. I've fixed the issues you mentioned.";
    let want = format!("{long}\n\n---\n改动：补上冠词。");
    check(
        ED,
        "Thanks for review",
        &format!("\"{long}\"\n\n---\n改动：补上冠词。"),
        &want,
        false,
    );
    check(
        ED,
        "Thanks for review",
        &format!("“{long}”\n\n---\n改动：补上冠词。"),
        &want,
        false,
    );
    check(
        ED,
        "Thanks for review",
        &format!("```\n{long}\n```\n\n---\n改动：补上冠词。"),
        &want,
        false,
    );
    // 正文很短（闭合符号落在窗口内）
    check(
        ED,
        "thx",
        "\"Thanks!\"\n\n---\n改动：补上感叹号。",
        "Thanks!\n\n---\n改动：补上感叹号。",
        false,
    );
    check(
        ED,
        "thx",
        "```\nThanks!\n```\n---\n**Changes**: none",
        "Thanks!\n---\n**Changes**: none",
        false,
    );
    check(ED, "thx", "\"Thanks!\"\n---", "Thanks!\n---", false);
    // 引号之后是别的正文：不是包裹，原样保留
    let kept = "\"Ship it\" is what he said.\n\n---\n改动：无";
    check(ED, "Ship it is what he said", kept, kept, false);
    // 切分结果不带孤立的闭合符号
    let (out, _) = run(ED, "thx", &[&format!("\"{long}\"\n\n---\n改动：补上冠词。")]);
    assert_eq!(
        split_edit_output(&out),
        (long.to_owned(), Some("改动：补上冠词。".to_owned()))
    );
}

#[test]
fn prefixes_in_the_source_close_the_whole_group() {
    // 跨语言：原文以 Translate / Translated text 开头时，「翻译：」「译文：」是正确的译文
    check(TR, "Translate: Ctrl+T", "翻译：Ctrl+T", "翻译：Ctrl+T", false);
    check(TR, "Translated text: none", "译文：无", "译文：无", false);
    check(TR, "译文：无", "Translation: none", "Translation: none", false);
    // 原文没有这些形式时照常剥离
    check(TR, "Ctrl+T", "翻译：Ctrl+T", "Ctrl+T", false);
    // 编辑动作：原文以 Here's the … 开头时，同形的前言不剥
    let src = "Here's the plan: ship on Monday.";
    let out = "Here's the revised plan: ship on Monday.\n\n---\n改动：补上 revised。";
    check(ED, src, out, out, false);
}

#[test]
fn short_and_edge_outputs() {
    // 「ok」在 Done 时放行（流不足 24 字符就结束）
    check_tr("ok", "ok");
    check_tr("a", "a");
    check_tr("", "");
    check_tr("   \n\n ", "");
    check_tr("你好\n\n", "你好");
    check_tr("\n\n你好", "你好");
    check_tr("<", "<");
    check_tr("`", "`");
    check_tr("Here", "Here");
}

#[test]
fn structure_is_preserved() {
    // learn 风格的 Markdown、technical 的 → 行、S10 的列表、S7 的缩进都原样通过
    let learn = "你好，世界。\n\n**词汇**\n- **hello** /həˈloʊ/ int. — 你好\n\n**要点**\n- 打招呼的常用语。";
    check_tr(learn, learn);
    let technical = "TypeError: Cannot read properties of undefined (reading 'map')\n→ 类型错误：无法读取 undefined 的属性（读取 'map'）\n    at App (App.jsx:12:18)";
    check_tr(technical, technical);
    let list = "- 用 `pnpm install` 安装依赖\n- 运行 `pnpm dev` 启动开发服务器\n- 打开 http://localhost:5173";
    check_tr(list, list);
    let dict = "**token** /ˈtoʊkən/ n.\n1. 令牌；凭证 — 计算机：身份验证凭据\n例：The API rejected the expired token. → API 拒绝了过期的令牌。";
    check_tr(dict, dict);
}

#[test]
fn indentation_follows_source() {
    // 原文首行没有缩进：去掉输出首行的缩进
    check(TR, "Hello", "  你好", "你好", false);
    // 原文首行有缩进：保留
    check(TR, "    let x = 1;", "    let x = 1;", "    let x = 1;", false);
    check(TR, "\n\n    code", "\n    代码\n", "    代码", false);
}

#[test]
fn edit_outputs() {
    let body = "Thanks for the review. I've fixed the issues you mentioned.\n\n---\n改动：补上冠词，修正时态。";
    check(ED, "Thanks for review", body, body, false);
    check(
        ED,
        "Thanks for review",
        &format!("Here's the improved version:\n\n{body}"),
        body,
        false,
    );
    check(
        ED,
        "Thanks for review",
        &format!("Here is the corrected text:\n{body}"),
        body,
        false,
    );
    check(
        ED,
        "原文",
        "润色后：这个功能明天上线。\n\n---\n改动：无",
        "这个功能明天上线。\n\n---\n改动：无",
        false,
    );
    // 原文本身以前言开头时不剥
    let src = "Here is my plan: ship it on Monday.";
    check(
        ED,
        src,
        "Here is my plan: ship it on Monday.\n\n---\n改动：无",
        "Here is my plan: ship it on Monday.\n\n---\n改动：无",
        false,
    );
}

#[test]
fn ocr_no_text_forms() {
    // 5 种输入判为无文字，流式中不放行
    for raw in [
        "<NO_TEXT>",
        "```\n<NO_TEXT>\n```",
        "□",
        "null",
        "No text found.",
        "  <NO_TEXT>\n",
        "NULL",
        // 一对反引号之后跟着空白
        "`<NO_TEXT>`\n",
        "`<NO_TEXT>` ",
        "`<NO_TEXT>`\r\n",
        "`null`\n",
        // OCR 前缀之后的哨兵：前缀会被剥掉，剩下的仍是哨兵
        "Transcription: <NO_TEXT>",
        "Here is the transcription:\n<NO_TEXT>",
        "译文：<NO_TEXT>",
        "```text\nTranscription: <NO_TEXT>\n```",
        "Transcription:\n```\n<NO_TEXT>\n```\n",
        "Translation: `<NO_TEXT>`",
    ] {
        for mode in [
            OcrMode::Lines,
            OcrMode::Paragraphs,
            OcrMode::Markdown,
            OcrMode::Translate,
        ] {
            check(ocr(mode), "", raw, "", true);
        }
    }
    for (raw, want) in [
        ("OK", "OK"),
        ("A1", "A1"),
        ("nullable", "nullable"),
        ("□□□", "□□□"),
        ("No texts here", "No texts here"),
        ("Transcription: <NO_TEXT> x", "<NO_TEXT> x"),
        ("`<NO_TEXT>` x", "`<NO_TEXT>` x"),
        ("Here is the menu:\nTea", "Here is the menu:\nTea"),
    ] {
        check(ocr(OcrMode::Lines), "", raw, want, false);
    }
    // 流式中出现 `<NO_` 时不放行
    let mut p = PostProcessor::new(ocr(OcrMode::Lines), "");
    assert_eq!(p.push("<NO_"), None);
    assert_eq!(p.push("TEXT>"), None);
    let f = p.finish();
    assert!(f.no_text && f.tail.is_empty());
}

#[test]
fn ocr_fences_and_prefixes() {
    let lines = ocr(OcrMode::Lines);
    let md = ocr(OcrMode::Markdown);
    check(lines, "", "```\nabc\n```", "abc", false);
    check(md, "", "```python\nprint(1)\n```", "```python\nprint(1)\n```", false);
    check(md, "", "```markdown\n# T\n```", "# T", false);
    check(
        md,
        "",
        "```md\n# T\n\n| a | b |\n| - | - |\n```",
        "# T\n\n| a | b |\n| - | - |",
        false,
    );
    check(md, "", "```\nplain\n```", "```\nplain\n```", false);
    check(lines, "", "```text\nline 1\nline 2\n```", "line 1\nline 2", false);
    check(lines, "", "```markdown\n# T\n```", "# T", false);
    check(lines, "", "```python\nprint(1)\n```", "```python\nprint(1)\n```", false);
    check(ocr(OcrMode::Translate), "", "```\n设置\n```", "设置", false);
    // 前缀
    check(lines, "", "Here is the transcription:\nabc", "abc", false);
    check(lines, "", "Transcription: abc", "abc", false);
    check(lines, "", "识别结果：abc", "abc", false);
    check(lines, "", "译文：abc", "abc", false);
    check(ocr(OcrMode::Translate), "", "Translation: 设置", "设置", false);
    // 转录模式不剥「Translation:」，也不套用引号与标签规则
    check(lines, "", "Translation: abc", "Translation: abc", false);
    check(lines, "", "\"quoted\"", "\"quoted\"", false);
    check(
        lines,
        "",
        "<translation>x</translation>",
        "<translation>x</translation>",
        false,
    );
    // lines 模式保留首行缩进
    check(
        lines,
        "",
        "    at App (App.jsx:12:18)\n    at main",
        "    at App (App.jsx:12:18)\n    at main",
        false,
    );
    check(lines, "", "\n\n  indented\n", "  indented", false);
}

#[test]
fn ocr_default_tidy() {
    // 逐行与段落模式：统一换行符、去掉行尾空白、连续空行压成一个、去掉首尾空行；缩进与行内空格不动
    let cases = [
        // 行尾空白（空格、制表符、全角空格、不换行空格）
        (
            "第一行  \n第二行\t\n第三行\u{3000}\nlast\u{A0}",
            "第一行\n第二行\n第三行\nlast",
        ),
        // 连续空行，含只有空白的行
        ("a\n\n\n\nb\n  \n\t\n\nc", "a\n\nb\n\nc"),
        // 首尾空行；第一行的缩进保留
        ("\n \n\n  a\n\n\n", "  a"),
        ("\u{3000}\u{3000}首行缩进\n\n\n正文", "\u{3000}\u{3000}首行缩进\n\n正文"),
        // 缩进与行内空格
        (
            "    at App (App.jsx:12:18)  \n\n\n\n    at main   (x)",
            "    at App (App.jsx:12:18)\n\n    at main   (x)",
        ),
        // 制表符分栏的行：行内制表符保留，行尾的去掉
        ("名称\t数量\t\n苹果\t3\t\t\n梨\t\t5\n", "名称\t数量\n苹果\t3\n梨\t\t5"),
        // \r\n 与单独的 \r
        ("a\r\n\r\n\r\nb\rc  \r\n", "a\n\nb\nc"),
        // 与围栏剥离组合：围栏里的多余空白同样整理
        ("```text\nline 1  \n\n\n\nline 2\t\n```", "line 1\n\nline 2"),
        ("```\n\n\n  code\n\n\n```\n\n", "  code"),
        // 与前缀剥离组合：前缀之后的空行去掉，下一行的缩进保留
        ("Here is the transcription:\n\n\n  abc  \n\n\n\ndef", "  abc\n\ndef"),
        ("Transcription:   abc  \r\n\r\n\r\ndef", "abc\n\ndef"),
        ("识别结果：\n\n第一段\u{3000}\n\n\n第二段", "第一段\n\n第二段"),
        // 看着像无文字、其实有文字：整理照常
        ("Transcription: <NO_TEXT> x  \n\n\ny", "<NO_TEXT> x\n\ny"),
    ];
    for mode in [OcrMode::Lines, OcrMode::Paragraphs] {
        for (raw, want) in cases {
            check(ocr(mode), "", raw, want, false);
        }
        // 无文字判定不受影响：空行与空白包着的哨兵仍判为无文字，什么都不放行
        for raw in [
            "\n\n<NO_TEXT>\n\n\n",
            "```\n\n\n<NO_TEXT>\n\n```",
            "Transcription:\n\n\n`null`  \r\n",
            " \n\t\n",
        ] {
            check(ocr(mode), "", raw, "", true);
        }
    }
    // 结构化与直接翻译不整理：Markdown 的硬换行（行尾两个空格）、代码块里的空行、译文的空行都原样保留（只去掉首尾空白）
    let md = "# 标题\n\n第一行  \n第二行\n\n\n```\ncode\n\n\n```";
    check(ocr(OcrMode::Markdown), "", md, md, false);
    check(
        ocr(OcrMode::Translate),
        "",
        "设置  \n\n\n\n关于\t",
        "设置  \n\n\n\n关于",
        false,
    );
}

#[test]
fn ocr_tidy_streams_without_rewriting() {
    // 文字一到就放行，只有它之后的空白滞留到下一个非空白字符：行尾空白丢掉，空行压成一个，缩进随下一行放行
    let mut p = PostProcessor::new(ocr(OcrMode::Lines), "");
    assert_eq!(p.push("Hello  ").as_deref(), Some("Hello"));
    assert_eq!(p.push("\n\n\n"), None);
    assert_eq!(p.push("  \t"), None);
    assert_eq!(p.push("world").as_deref(), Some("\n\n  \tworld"));
    assert_eq!(p.push("\u{3000}\r\n"), None);
    assert_eq!(p.finish(), wisp_prompts::PostFinish::default());
}

#[test]
fn streaming_releases_text_early() {
    // 普通文本：第一个字符立即放行
    let mut p = PostProcessor::new(TR, "Hello");
    assert_eq!(p.push("你").as_deref(), Some("你"));
    assert_eq!(p.push("好").as_deref(), Some("好"));

    // 可能是前缀时缓冲，确定不是后立即放行
    let mut p = PostProcessor::new(TR, "Hello");
    assert_eq!(p.push("Tr"), None);
    assert_eq!(p.push("u").as_deref(), Some("Tru"));

    // 引号开头：最多看 24 个字符就做出判断
    let mut p = PostProcessor::new(TR, "Hello");
    let text = format!("\"{}", "很长的译文".repeat(10));
    let mut first_release = None;
    for (n, c) in char_chunks(&text).into_iter().enumerate() {
        if p.push(c).is_some() {
            first_release = Some(n + 1);
            break;
        }
    }
    assert!(
        first_release.is_some_and(|n| n <= 26),
        "引号开头应在 ~25 个字符内放行：{first_release:?}"
    );

    // 围栏开头：开头围栏行之后最多看 24 个字符
    let mut p = PostProcessor::new(TR, "Hello");
    let text = format!("```\n{}", "很长的译文".repeat(10));
    let mut first_release = None;
    for (n, c) in char_chunks(&text).into_iter().enumerate() {
        if p.push(c).is_some() {
            first_release = Some(n + 1);
            break;
        }
    }
    assert!(
        first_release.is_some_and(|n| n <= 4 + 25),
        "围栏开头应在开头行 + ~25 个字符内放行：{first_release:?}"
    );

    // 剥掉开头引号后，等待配对的闭合引号时滞留不超过 41 个字符（引号本身 + 向后看的 40 个字符）
    let text = format!(
        "\"{}\"是一部小说，{}",
        "很长的书名".repeat(6),
        "后面还有很多正文。".repeat(4)
    );
    let mut p = PostProcessor::new(TR, "Hello");
    let (mut fed, mut emitted) = (0usize, 0usize);
    let mut head_done = false;
    for c in char_chunks(&text) {
        fed += 1;
        if let Some(s) = p.push(c) {
            emitted += s.chars().count();
            head_done = true;
        }
        if head_done {
            // 已剥掉的开头引号算作放行
            assert!(fed - emitted - 1 <= 41, "滞留了 {} 个字符", fed - emitted - 1);
        }
    }
    let f = p.finish();
    assert!(head_done && f.tail.chars().count() <= 41);
}

#[test]
fn tail_holdback_is_bounded() {
    // 中段滞留的非空白字符不超过 16 个（空白不计数，可见文本的延迟有界）
    let text = "The quick brown fox jumps over the lazy dog, again and again, with spaces   and\ttabs.  \n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n</transl\n</source_text>\n</translation> trailing words";
    let mut p = PostProcessor::new(TR, "Hello");
    let mut emitted = String::new();
    for (n, c) in char_chunks(text).into_iter().enumerate() {
        if let Some(s) = p.push(c) {
            emitted.push_str(&s);
        }
        let fed: String = text.chars().take(n + 1).collect();
        assert!(fed.starts_with(&emitted), "放行的文本必须是输入的前缀");
        let held_visible = fed[emitted.len()..].chars().filter(|c| !c.is_whitespace()).count();
        assert!(
            held_visible <= 16,
            "第 {} 个字符后滞留了 {held_visible} 个非空白字符",
            n + 1
        );
    }
    let f = p.finish();
    emitted.push_str(&f.tail);
    assert_eq!(emitted, text);
    // 大段空白逐字符喂入：全部滞留、不放行，结尾剥掉
    let mut p = PostProcessor::new(TR, "Hello");
    assert_eq!(p.push("你好").as_deref(), Some("你好"));
    for _ in 0..10_000 {
        assert_eq!(p.push("\n"), None);
    }
    assert_eq!(p.finish().tail, "");
}

/// 按约 6 字节一段切开（模拟逐 token 到达）
fn token_chunks(text: &str) -> Vec<&str> {
    let mut v = Vec::new();
    let mut start = 0;
    for (i, _) in text.char_indices().skip(1) {
        if i - start >= 6 {
            v.push(&text[start..i]);
            start = i;
        }
    }
    v.push(&text[start..]);
    v
}

#[test]
fn large_stream_matches_whole() {
    // 100 KB 逐 token 喂入：结果与整段一致（线性时间由 benches/assemble.rs 的门禁保证：100 KB ≤ 2 ms）
    let unit = "这是第 N 段译文，包含 `code`、<tag> 和 \"引号\"。\nThe quick brown fox jumps over the lazy dog.\n";
    let text = unit.repeat(100 * 1024 / unit.len() + 1);
    let chunks = token_chunks(&text);
    let (out, _) = run(TR, "Hello", &chunks);
    assert_eq!(out, text.trim_end());
}

#[test]
fn large_ocr_stream_matches_whole() {
    // 100 KB 带行尾空白、空行与缩进的识别结果逐 token 喂入：与整段默认整理的结果一致
    let unit = "  第 N 行 识别结果  \r\n\tcol1\tcol2\t\n\n\n\u{3000}\nThe quick brown fox.\u{A0}\n";
    let text = unit.repeat(100 * 1024 / unit.len() + 1);
    let chunks = token_chunks(&text);
    for mode in [OcrMode::Lines, OcrMode::Paragraphs] {
        let (out, no_text) = run(ocr(mode), "", &chunks);
        assert!(!no_text);
        assert_eq!(out, squeeze_blanks(&text), "{mode:?}");
    }
}

#[derive(serde::Deserialize)]
struct Fixture {
    cases: Vec<Case>,
}

#[derive(serde::Deserialize)]
struct Case {
    name: String,
    input: String,
    body: String,
    changes: Option<String>,
}

#[test]
fn edit_split_fixture() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/edit_split.json");
    let text = std::fs::read_to_string(path).expect("read edit_split.json");
    let fixture: Fixture = serde_json::from_str(&text).expect("parse edit_split.json");
    assert!(fixture.cases.len() >= 10);
    for c in fixture.cases {
        let (body, changes) = split_edit_output(&c.input);
        assert_eq!(body, c.body, "{}: body", c.name);
        assert_eq!(changes, c.changes, "{}: changes", c.name);
    }
}

#[test]
fn is_no_text_acceptance() {
    for s in ["<NO_TEXT>", "```\n<NO_TEXT>\n```", "□", "null", "No text found."] {
        assert!(is_no_text(s), "{s:?}");
    }
    for s in ["OK", "A1"] {
        assert!(!is_no_text(s), "{s:?}");
    }
}

/// 随机拼接「容易出错」的片段（前缀、标签、引号、围栏、空白、中英文），验证任意切分的结果都与整段喂入相同
#[test]
fn split_invariance_fuzz() {
    const PIECES: &[&str] = &[
        "译文：",
        "翻译:",
        "Translation: ",
        "Here is the translation:",
        "Here's the translation into Chinese:\n",
        "Here is the ",
        "<translation>",
        "</translation>",
        "<source_text>\n",
        "</source_text>",
        "<",
        "</",
        "\"",
        "“",
        "”",
        "```",
        "```text\n",
        "```python\n",
        "\n```",
        "`",
        "\n",
        "\n\n",
        "  ",
        "\t",
        "你好",
        "世界。",
        "hello",
        " world",
        "<NO_TEXT>",
        "null",
        "No text found.",
        "□",
        "---\n改动：无",
        "Transcription:",
        "识别结果：",
        "ok",
        ".",
        "-",
        "NO_",
        "TEXT>",
        "Here",
        "’s the translation:",
        "**",
        "Galaxy\" is",
        "说\"",
        "\"你",
        "”是",
        "“内层",
        "```bash\n",
        "\n```\n",
        "````",
        "```js\n",
        "\n---\n改动：无",
        "\n---\nChanges: x",
        "\n\n---\n",
        "改动",
        "<source_text>",
        "很长很长很长很长很长的一段译文",
        "The quick brown fox jumps over",
        "\r\n",
        "   ```",
        "`<NO_TEXT>`",
        "Here is the transcription:\n",
        "Transcription: ",
        "``",
        // OCR 默认整理：行尾空白、空行、缩进、制表符分栏
        "  \n",
        "\n\n\n",
        "\u{3000}",
        "\u{A0}",
        "\r",
        "    indented",
        "名称\t数量\t",
    ];
    let kinds = [
        (TR, "Hello"),
        (TR, "\"quoted source\""),
        (TR, "    indented"),
        (ED, "Some text"),
        (ED, "Here's the plan: x"),
        (TR, "Translate: x"),
        (ocr(OcrMode::Lines), ""),
        (ocr(OcrMode::Paragraphs), ""),
        (ocr(OcrMode::Markdown), ""),
        (ocr(OcrMode::Translate), ""),
    ];
    let mut state: u64 = std::env::var("WISP_POST_FUZZ_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    // 默认 4000 轮；开发时可用 WISP_POST_FUZZ_ROUNDS 放大（例如 500000）
    let rounds = std::env::var("WISP_POST_FUZZ_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4000usize);
    for round in 0..rounds {
        let len = (next() % 9 + 1) as usize;
        let raw: String = (0..len)
            .map(|_| PIECES[(next() % PIECES.len() as u64) as usize])
            .collect();
        let (kind, source) = kinds[round % kinds.len()];
        let whole = run(kind, source, &[raw.as_str()]);
        assert_eq!(run(kind, source, &char_chunks(&raw)), whole, "逐字符 {kind:?} {raw:?}");
        for seed in 0..4 {
            assert_eq!(
                run(kind, source, &random_chunks(&raw, next() ^ seed)),
                whole,
                "随机 {kind:?} {raw:?}"
            );
        }
        // 输出不会凭空多出字符：长度不超过输入
        assert!(whole.0.len() <= raw.len(), "{kind:?} {raw:?} → {:?}", whole.0);
        // 逐行与段落模式的输出已经过默认整理：再整理一遍不变
        if matches!(
            kind,
            PostKind::Ocr {
                mode: OcrMode::Lines | OcrMode::Paragraphs
            }
        ) {
            assert_eq!(squeeze_blanks(&whole.0), whole.0, "{kind:?} {raw:?}");
        }
        // OCR：no_text 与 is_no_text 一致，判为无文字时不放行任何文本；其余种类不判无文字
        if matches!(kind, PostKind::Ocr { .. }) {
            assert_eq!(whole.1, is_no_text(&raw), "no_text {kind:?} {raw:?}");
            assert!(!whole.1 || whole.0.is_empty(), "{kind:?} {raw:?} → {:?}", whole.0);
        } else {
            assert!(!whole.1, "{kind:?} {raw:?}");
        }
    }
}

/// 按「无文字」语法随机生成的字符串：[空白] [前缀] [围栏] [前缀] [`] 核心 [.] [`] [闭合围栏] [空白]
fn random_no_text_form(next: &mut impl FnMut() -> u64) -> String {
    const WS: &[&str] = &["", "", " ", "\n", "\r\n", "\n\n", "\t", "  \n"];
    const PREFIXES: &[&str] = &[
        "Transcription: ",
        "Transcription:\n",
        "TRANSCRIPTION :",
        "Here is the transcription:\n",
        "Here's the transcription of the image:\n\n",
        "Here’s the transcription: ",
        "识别结果：",
        "译文：",
        "翻译:",
        "Translation: ",
        "Here is the translation:\n",
    ];
    const FENCES: &[&str] = &["```\n", "```text\n", "```md\n", "```plaintext \r\n", "```python\n"];
    const CLOSERS: &[&str] = &["", "```", "\n```", "\n```\n", "\n``` "];
    const CORES: &[&str] = &[
        "<NO_TEXT>",
        "<no_text>",
        "NO_TEXT",
        "null",
        "NULL",
        "No text found",
        "no text found",
        "□",
        "□□",
        "--",
        "",
        "<NO_TEXT> ",
    ];
    fn pick(next: &mut impl FnMut() -> u64, items: &[&'static str]) -> &'static str {
        items[(next() % items.len() as u64) as usize]
    }
    let mut out = String::new();
    out.push_str(pick(next, WS));
    // 前缀在围栏外还是围栏内（或者没有）
    let prefix_slot = next() % 3;
    let fenced = next().is_multiple_of(2);
    if prefix_slot == 1 {
        out.push_str(pick(next, PREFIXES));
        out.push_str(pick(next, WS));
    }
    if fenced {
        out.push_str(pick(next, FENCES));
        out.push_str(pick(next, WS));
    }
    if prefix_slot == 2 {
        out.push_str(pick(next, PREFIXES));
        out.push_str(pick(next, WS));
    }
    let inline = next().is_multiple_of(3);
    if inline {
        out.push('`');
    }
    out.push_str(pick(next, CORES));
    if next().is_multiple_of(3) {
        out.push('.');
    }
    if inline {
        out.push('`');
    }
    out.push_str(pick(next, WS));
    if fenced {
        out.push_str(pick(next, CLOSERS));
    }
    out.push_str(pick(next, WS));
    out
}

/// 「无文字」形式的前缀闭包：任何 is_no_text 为真的输出，逐字符喂入时都不放行任何文本，
/// 结束时判为无文字——即流式开头的缓冲覆盖了每一种无文字形式的每一个前缀
#[test]
fn no_text_forms_are_never_released() {
    let mut state: u64 = std::env::var("WISP_POST_FUZZ_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0xD1B5_4A32_D192_ED03);
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let rounds = std::env::var("WISP_POST_FUZZ_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000usize);
    let modes = [
        OcrMode::Lines,
        OcrMode::Paragraphs,
        OcrMode::Markdown,
        OcrMode::Translate,
    ];
    let mut accepted = 0usize;
    for round in 0..rounds {
        let raw = random_no_text_form(&mut next);
        if !is_no_text(&raw) {
            continue;
        }
        accepted += 1;
        let kind = ocr(modes[round % modes.len()]);
        let mut p = PostProcessor::new(kind, "");
        for (i, c) in char_chunks(&raw).into_iter().enumerate() {
            assert_eq!(p.push(c), None, "{kind:?} {raw:?}：第 {} 个字符处提前放行", i + 1);
        }
        assert_eq!(
            p.finish(),
            wisp_prompts::PostFinish {
                tail: String::new(),
                no_text: true
            },
            "{kind:?} {raw:?}"
        );
        assert_eq!(
            run(kind, "", &[raw.as_str()]),
            (String::new(), true),
            "整段 {kind:?} {raw:?}"
        );
    }
    // 生成器的大部分输出都应是无文字形式（否则测试形同虚设）
    assert!(accepted * 2 > rounds, "只有 {accepted}/{rounds} 个随机形式被认作无文字");
}
