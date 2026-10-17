//! 整段文字按行处理的操作。

use crate::join::{MixedSpacing, join_break, starts_list_item};
use crate::script::is_east_asian;

/// 识别结果的默认整理（整段版本），只动空白、不丢信息：
///
/// - `\r\n` 与单独的 `\r` 统一成 `\n`；
/// - 去掉每行行尾的空白（含 `\t`、全角空格 U+3000、不换行空格 U+00A0 等一切 Unicode 空白）；
/// - 连续两个以上的空行压成一个，开头与结尾的空行去掉（结果不以换行结尾）；
/// - 行首缩进（包括第一行的缩进）与行内空格原样保留，不拼接行。
///
/// 与流式版本 [`BlankSqueezer`](crate::stream::BlankSqueezer) 的结果逐字相同。幂等；去掉所有空白后与输入相同。
pub fn squeeze_blanks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    // 上一个非空行之后是否出现过空行（开头的空行不算：那时 out 还是空的）
    let mut gap = false;
    for line in line_contents(text).map(str::trim_end) {
        if line.is_empty() {
            gap |= !out.is_empty();
            continue;
        }
        if !out.is_empty() {
            out.push_str(if gap { "\n\n" } else { "\n" });
        }
        gap = false;
        out.push_str(line);
    }
    out
}

/// 按 `\n`、`\r\n`、单独的 `\r` 断开的物理行：`(内容, 连同换行符的整行)`，最后一行可能没有换行符。
/// 末尾的换行符不算再起一个空行（与 [`str::lines`] 相同），所以 `"a\n"` 只有一行；整行依次拼起来就是原文。
///
/// 本模块按行处理的操作都经这里断行，三种换行符处处含义相同（流式版本的规则也一样）；别处按行判断或处理文字时
/// （例如剪贴板变换「是否有多行」「去掉首尾空白」）也用它，「行」的含义才处处一致。不用 [`str::lines`]：
/// 它不把单独的 `\r` 当换行，同一段老式 Mac 换行的文字在「合成一行」里会留下 `\r`，在其他操作里却被断开
pub fn split_lines(text: &str) -> impl Iterator<Item = (&str, &str)> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let (content_end, line_end) = match rest.find(['\n', '\r']) {
            Some(i) if rest[i..].starts_with("\r\n") => (i, i + 2),
            Some(i) => (i, i + 1),
            None => (rest.len(), rest.len()),
        };
        let (line, tail) = rest.split_at(line_end);
        rest = tail;
        Some((&line[..content_end], line))
    })
}

/// 各行的内容（不含换行符），断行同 [`split_lines`]
fn line_contents(text: &str) -> impl Iterator<Item = &str> {
    split_lines(text).map(|(content, _)| content)
}

/// 只含空白（含全角空格、不换行空格）或什么都没有的行
fn is_blank(content: &str) -> bool {
    content.trim().is_empty()
}

/// 文中是否有空行（只含空白的行，开头与结尾的也算）。与 `remove_blank_lines(text) != text` 等价，但不分配、
/// 遇到第一个空行就停：「粘贴为…」检视时每移动一次光标都要判断一次，长文本不应每次都复制一份
pub fn has_blank_lines(text: &str) -> bool {
    line_contents(text).any(is_blank)
}

/// 去掉全部空行（只含空白的行，包括开头与结尾的空行），其余每一行连同它自己的换行符逐字保留：
/// 行首缩进、行尾空白与 `\r\n` 都不动，也不拼接行。用于段落之间不要空行的场合（段落模式的识别结果每段一行，
/// 去掉空行后段与段紧挨着）。
///
/// 幂等；只删整行，不改动留下的任何一行。
pub fn remove_blank_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (_, line) in split_lines(text).filter(|(content, _)| !is_blank(content)) {
        out.push_str(line);
    }
    out
}

/// 把所有非空行合成一行（`\n`、`\r\n` 与单独的 `\r` 都是换行）：每行去掉首尾空白、跳过空行，相邻两行按
/// [`join_break`] 相连；东亚文字与字母数字之间断行处补不补空格，按全文的惯例推断（[`MixedSpacing::infer`]）。
///
/// 幂等；除被还原的断词连字符与软连字符外，不增删任何非空白字符。
pub fn join_all(text: &str) -> String {
    let spacing = MixedSpacing::infer(line_contents(text));
    let mut out = String::with_capacity(text.len());
    for line in line_contents(text).map(str::trim).filter(|l| !l.is_empty()) {
        join_break(&mut out, line, spacing);
    }
    out
}

/// 合并段内换行：以空行为段界，段内被硬换行断开的行按 [`join_break`] 连成一行（断词还原、东亚文字直接相连、中英之间
/// 按全文惯例，与 [`join_all`] 相同）。用于从 PDF、网页或截图里得到的、每行末尾都有换行的段落。
///
/// 只动被合并的换行：空行（段界）与没有合并的行连同各自的换行符逐字保留；被合并的行去掉接缝处的空白（上一行的行尾
/// 空白），一串合并的行里第一行的缩进保留。宁可少合并，也不误合并代码与表格，下面这些行保持原样：
///
/// - **不与上下行相连**：以列表项开头（[`starts_list_item`]）；含制表符，或行内有连续两个以上的空白（按空格对齐的列；
///   句末 `.` `!` `?` 后正好两个半角空格除外，那是英文打字的句间空格）；以 `;` `{` `}` `\` `|` 结尾（代码、续行的
///   命令、表格）；以 `>`（引用、提示符）、`$ ` 或 `❯`（命令提示符）、`#` 加空格（标题、注释）、`//`（注释）、`|` 或
///   制表线字符（表格、目录树）开头；整行只有一个不含东亚文字的词、且含 `/`、`\` 或 `@`（链接、路径、邮箱的清单）；
///   没有任何字母、数字或文字（分隔线）；
/// - **不接到上一行后面**：有缩进（行首有空白：首行缩进的新段落、代码块）；以「标签：」开头（`Name: …`、`error: …`、
///   `电话：…`）；
/// - **不接下一行**：很短的独立行（不足 20 个半角宽、自己也没有接到上一行后面：标题、称呼、菜单项、命令）；有缩进的
///   行，除非它是段落开头（段内第一行，或上一行以句末标点结束、是很短的独立行）且下一行没有缩进——首行缩进的段落
///   照常合并，缩进的代码不会接上后面的行。
///
/// 已知的局限（要保留原来的换行时不用这个变换）：
///
/// - 段落之间既没有空行、也没有首行缩进时，段尾的短行与段中的行分不开，下一段会接到上一段末尾；
/// - 诗歌、歌词以空行分节时，节内长短相近的诗行会合成一行（很短的诗行除外）；
/// - 列表项折行后的续行、整段都有缩进的文字不合并；
/// - 没有缩进、分号与花括号这些特征的代码（例如不写分号的脚本）可能被合并。
///
/// 幂等；除被还原的断词连字符与软连字符外，不增删任何非空白字符；三种换行符（`\n`、`\r\n`、单独的 `\r`）含义相同。
pub fn reflow(text: &str) -> String {
    let spacing = MixedSpacing::infer(line_contents(text));
    let mut out = String::with_capacity(text.len());
    let mut plan = Reflow::default();
    // 上一行的换行符与行尾空白的长度：下一行接上来时丢掉，否则原样写出
    let mut pending = "";
    let mut trailing = 0;
    for (content, line) in split_lines(text) {
        if plan.joins(content) {
            // 接上来的行没有缩进（有缩进的行不接到上一行后面），接缝两侧都不是空白
            out.truncate(out.len() - trailing);
            join_break(&mut out, content, spacing);
        } else {
            out.push_str(pending);
            out.push_str(content);
        }
        pending = &line[content.len()..];
        trailing = content.len() - content.trim_end().len();
    }
    out.push_str(pending);
    out
}

/// 是否有可以合并的段内换行。与 `reflow(text) != text` 等价（每合并一处就少一个换行），但不分配、遇到第一处就停：
/// 「粘贴为…」检视时每移动一次光标都要判断一次
pub fn can_reflow(text: &str) -> bool {
    let mut plan = Reflow::default();
    line_contents(text).any(|content| plan.joins(content))
}

/// 「很短」的界线（半角宽，东亚文字记 2）：约 10 个汉字或 20 个字母。比它窄的独立行多半是标题、称呼、菜单项或命令，
/// 不是被版面宽度折断的长句
const SHORT_LINE: usize = 20;

/// [`reflow`] 的逐行决定：每一行是否接到上一行后面。
///
/// 只看每一行自己的开头、结尾与内容，以及上一行的这些特征，所以对合并的结果再做一次，每处断开的理由都还在（幂等）：
/// 合并只改动行与行之间的接缝；一串合并的行的开头是第一行的开头，结尾是最后一行的结尾；保持独立的行从不参与合并；
/// 第一行至少 [`SHORT_LINE`] 宽才会接下一行，合并出来的行因此不会变成「很短」
#[derive(Default)]
struct Reflow {
    /// 段内的上一行；None 表示段落开头（文首或刚过空行）
    prev: Option<Prev>,
}

struct Prev {
    shape: Shape,
    /// 自己没有接到上一行后面（一串合并的行里的第一行）
    head: bool,
    /// 是段落开头：段内第一行，或上一行以句末标点结束、是很短的独立行。只对有缩进的行有用（首行缩进的段落）
    opens: bool,
}

impl Reflow {
    /// 下一行（不含换行符）是否接到上一行后面
    fn joins(&mut self, content: &str) -> bool {
        if is_blank(content) {
            self.prev = None;
            return false;
        }
        let shape = Shape::of(content);
        let (joins, opens) = match &self.prev {
            None => (false, true),
            Some(prev) => {
                let standalone_short = prev.head && prev.shape.short;
                let joins = !(prev.shape.fixed
                    || shape.fixed
                    || shape.starts_new
                    || standalone_short
                    || (prev.shape.indented && !prev.opens));
                (joins, prev.shape.ends_sentence || standalone_short)
            }
        };
        self.prev = Some(Prev {
            shape,
            head: !joins,
            opens,
        });
        joins
    }
}

/// 一行的特征（见 [`reflow`] 的说明）
#[derive(Debug, Clone, Copy)]
struct Shape {
    /// 行首有空白
    indented: bool,
    /// 不接到上一行后面：有缩进，或以「标签：」开头
    starts_new: bool,
    /// 不与上下行相连
    fixed: bool,
    /// 窄于 [`SHORT_LINE`]
    short: bool,
    /// 以句末标点结束（后面可以跟着引号、括号）
    ends_sentence: bool,
}

impl Shape {
    fn of(content: &str) -> Self {
        let body = content.trim();
        let indented = content.starts_with(char::is_whitespace);
        Self {
            indented,
            starts_new: indented || starts_with_label(body),
            fixed: is_fixed(body),
            short: is_short(body),
            ends_sentence: ends_sentence(body),
        }
    }
}

/// 保持独立、不与上下行相连的行（`body` 已去掉首尾空白）
fn is_fixed(body: &str) -> bool {
    starts_list_item(body)
        || has_columns(body)
        || body.ends_with([';', '{', '}', '\\', '|'])
        || starts_with_marker(body)
        || is_address(body)
        || !body.chars().any(char::is_alphanumeric)
}

/// 含制表符，或行内有连续两个以上的空白（含全角空格）：按制表符或空格对齐的列。句末 `.` `!` `?` 后正好两个半角空格
/// 不算：英文打字的句间空格，按这种习惯折行的纯文本（邮件、提交说明、编辑器自动折行的文字）正是要合并的对象
fn has_columns(body: &str) -> bool {
    if body.contains('\t') {
        return true;
    }
    let (mut run, mut spaces_only, mut before) = (0usize, true, None);
    for c in body.chars() {
        if c.is_whitespace() {
            run += 1;
            spaces_only &= c == ' ';
            continue;
        }
        if run >= 2 && !(run == 2 && spaces_only && matches!(before, Some('.' | '!' | '?'))) {
            return true;
        }
        (run, spaces_only, before) = (0, true, Some(c));
    }
    false
}

/// 以引用或提示符（`>`）、命令提示符（`$ `、`❯`）、Markdown 标题或注释（`#` 加空格）、注释（`//`）、表格与目录树的
/// 标记（`|`、制表线字符）开头
fn starts_with_marker(body: &str) -> bool {
    body.starts_with(['>', '|', '❯'])
        || body.starts_with(|c: char| matches!(c, '\u{2500}'..='\u{257F}'))
        || body.starts_with("$ ")
        || body.starts_with("//")
        || (body.starts_with('#') && body.trim_start_matches('#').starts_with(' '))
}

/// 整行只有一个不含东亚文字的词、且含 `/`、`\` 或 `@`：链接、路径、邮箱的清单逐行列出，接起来也只是用空格隔开。
/// 中文句子同样不含空格，含斜杠的（`单价五元/斤`）不算
fn is_address(body: &str) -> bool {
    !body.contains(char::is_whitespace) && body.contains(['/', '\\', '@']) && !body.chars().any(is_east_asian)
}

/// 以「标签：」开头：一个含字母、不含空白的词（最多 24 个字符）后跟半角冒号与空白，或者最多 8 个字母、数字或汉字后跟
/// 全角冒号；冒号后面都还有内容。单独以冒号结尾的词多半是句子的末尾（`as follows:`、`如下：`），不算
fn starts_with_label(body: &str) -> bool {
    let half = body.split_once(char::is_whitespace).is_some_and(|(token, _)| {
        token.strip_suffix(':').is_some_and(|name| {
            name.chars().count() <= 24 && name.chars().any(char::is_alphabetic) && !name.contains(':')
        })
    });
    let full = body.split_once('：').is_some_and(|(name, rest)| {
        !rest.is_empty() && (1..=8).contains(&name.chars().count()) && name.chars().all(char::is_alphanumeric)
    });
    half || full
}

/// 窄于 [`SHORT_LINE`]（东亚文字与全角空格记 2，其余记 1；数到界线就停，长行不必数完）
fn is_short(body: &str) -> bool {
    let mut width = 0;
    for c in body.chars() {
        width += if is_east_asian(c) || c == '\u{3000}' { 2 } else { 1 };
        if width >= SHORT_LINE {
            return false;
        }
    }
    true
}

/// 以句末标点结束，后面可以跟着引号或括号（`他说：“走吧。”`、`(see above.)`）
fn ends_sentence(body: &str) -> bool {
    body.trim_end_matches(['"', '\'', '”', '’', '」', '』', '）', ')', ']', '】', '》'])
        .ends_with(['.', '!', '?', '。', '．', '｡', '！', '？', '…'])
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn squeeze_blanks_cases() {
        for (input, want) in [
            ("", ""),
            (" \t\u{3000}\n\r\n\u{A0}", ""),
            ("single", "single"),
            // 行尾空白（空格、制表符、全角空格、不换行空格）去掉，行内空格不动
            ("a  b \t\nc\u{3000}\nd\u{A0}", "a  b\nc\nd"),
            // 空行压成一个；只有空白的行也是空行
            ("段一\n\n\n\n段二\n  \t \n\u{3000}\n段三", "段一\n\n段二\n\n段三"),
            // 开头与结尾的空行去掉，第一行的缩进保留
            ("\n \n\n    indented\n  next\n\n\n", "    indented\n  next"),
            ("\u{3000}\u{3000}首行缩进两格", "\u{3000}\u{3000}首行缩进两格"),
            // \r\n 与单独的 \r 都是一个换行
            ("a\r\nb\r\rc\r\n\r\n\r\nd\r", "a\nb\n\nc\n\nd"),
            // 代码：缩进与单个空行原样，多余的空行压掉
            (
                "fn main() {  \n    let x = 1;\n\n\n    x\n}\n",
                "fn main() {\n    let x = 1;\n\n    x\n}",
            ),
            // 制表符分栏：行内的制表符保留，行尾的去掉
            ("名称\t数量\t\n苹果\t3\t\t\n梨\t\t5", "名称\t数量\n苹果\t3\n梨\t\t5"),
            // 不拼接行：断行的英文、列表、Markdown 硬换行（行尾两个空格）都只去掉行尾空白
            (
                "- item one  \n- item two\nwrapped line  \nnext",
                "- item one\n- item two\nwrapped line\nnext",
            ),
        ] {
            assert_eq!(squeeze_blanks(input), want, "{input:?}");
        }
    }

    #[test]
    fn remove_blank_lines_cases() {
        for (input, want) in [
            // 段落模式的识别结果：每段一行，段与段之间一个空行
            ("第一段。\n\n第二段。\n\n第三段。", "第一段。\n第二段。\n第三段。"),
            // 只含空白（空格、制表符、全角空格、不换行空格）的行也是空行
            ("a\n  \n\t\nb\n\u{3000}\n\u{A0}\nc", "a\nb\nc"),
            // 开头与结尾的空行一并去掉；留下的行连同自己的换行符原样保留
            ("\n\n  indented  \nnext\n\n", "  indented  \nnext\n"),
            ("a\n \n", "a\n"),
            ("a\n  ", "a\n"),
            // 换行符原样保留：\r\n 不改成 \n，单独的 \r 也是换行
            ("a\r\n\r\nb\r\n", "a\r\nb\r\n"),
            ("a\r\rb", "a\rb"),
            // 代码：只删空行，缩进与行尾空白不动
            (
                "fn main() {\n    let x = 1;\n\n    x \n}",
                "fn main() {\n    let x = 1;\n    x \n}",
            ),
            // 全是空白时什么都不剩
            ("   ", ""),
            ("\n\n", ""),
        ] {
            assert_eq!(remove_blank_lines(input), want, "{input:?}");
            assert!(has_blank_lines(input), "{input:?} 有空行");
        }
        // 没有空行：末尾的一个换行符不算空行
        for input in ["", "single", "a\nb", "a\nb\n", "a\r\nb\r\n", "  a\n\tb  "] {
            assert!(!has_blank_lines(input), "{input:?} 没有空行");
            assert_eq!(remove_blank_lines(input), input);
        }
        // 默认整理之后的段落：去掉空行后不再有连续换行
        assert_eq!(
            remove_blank_lines(&squeeze_blanks("段一  \n\n\n段二\r\n\r\n段三\n")),
            "段一\n段二\n段三"
        );
    }

    #[test]
    fn join_all_cases() {
        for (input, want) in [
            ("first line\n  second line  \n\nthird", "first line second line third"),
            ("a\r\nb\r\n", "a b"),
            ("   \n\n", ""),
            ("single", "single"),
            // 中文断行直接相连；全文没有中英间隔空格时中英之间也不加
            (
                "这是一段被\n硬换行的中文\nwith English",
                "这是一段被硬换行的中文with English",
            ),
            // 全文习惯在中英之间加空格时补上
            ("支持 OpenAI 和\nAnthropic 等服务", "支持 OpenAI 和 Anthropic 等服务"),
            ("화면의 글자를\n인식합니다", "화면의 글자를 인식합니다"),
            ("他说——\n我们走吧……\n然后", "他说——我们走吧……然后"),
            (
                "COVID-\n19 and Wi-\nFi in 2020-\n2021",
                "COVID-19 and Wi-Fi in 2020-2021",
            ),
            ("an inter-\nface", "an interface"),
            // 以斜杠结尾的行多半是完整的条目：目录列表、网址列表合并后各自分开
            ("src/\ntests/\ndocs/", "src/ tests/ docs/"),
            ("https://a.com/\nhttps://b.com/", "https://a.com/ https://b.com/"),
            ("Visit:\nhttps://a.com/\nNext", "Visit: https://a.com/ Next"),
            // 单独的 \r 与 \n、\r\n 一样是换行
            ("a\rb\nc", "a b c"),
            ("old\rmac\r\r text\r", "old mac text"),
        ] {
            assert_eq!(join_all(input), want, "{input:?}");
        }
    }

    #[test]
    fn long_runs_without_spaces_stay_linear() {
        // 每次拼接只回看上一行末尾的有限几个字符：整段是标点或不含空白时也不会越拼越慢
        for unit in ["....................\n", "abc-\n", "中文……\n"] {
            let text = unit.repeat(20_000);
            let out = join_all(&text);
            assert!(!out.contains('\n') && out.len() < text.len());
        }
    }

    /// 合并段内换行的样例：`(说明, 输入, 结果)`。结果与输入相同的是「保持原样」的情形
    const REFLOW_CASES: &[(&str, &str, &str)] = &[
        (
            "PDF 里的英文：段内连成一行、行尾断词还原，空行（段界）保留",
            "Wisp is a lightweight desktop tool that stays in the back-\nground. It translates selected text, recognizes text in screen-\nshots and keeps a clipboard history.\n\nAll AI features run through the model service you configure.",
            "Wisp is a lightweight desktop tool that stays in the background. It translates selected text, recognizes text in screenshots and keeps a clipboard history.\n\nAll AI features run through the model service you configure.",
        ),
        (
            "编号与复合词里的连字符保留",
            "The study covers the period 2020-\n2021 and the COVID-\n19 pandemic in detail.",
            "The study covers the period 2020-2021 and the COVID-19 pandemic in detail.",
        ),
        (
            "中文硬换行直接相连",
            "这是一段从网页上截图识别出来的中文，每一行的末尾\n都有换行，但句子其实还没有结束，需要连成一段。\n\n第二段只有一行。",
            "这是一段从网页上截图识别出来的中文，每一行的末尾都有换行，但句子其实还没有结束，需要连成一段。\n\n第二段只有一行。",
        ),
        (
            "中英混排：全文习惯在中英之间加空格时断行处补上",
            "Wisp 是常驻后台的轻量桌面工具：划词翻译、截图\nOCR、剪贴板历史，都可以经你自己的 WebDAV 在多台\n设备之间端到端加密同步。",
            "Wisp 是常驻后台的轻量桌面工具：划词翻译、截图 OCR、剪贴板历史，都可以经你自己的 WebDAV 在多台设备之间端到端加密同步。",
        ),
        (
            "中英混排：全文中英之间不加空格时直接相连",
            "使用Wisp时，可以把识别出来的文字直接粘贴回\nWord或者其他编辑器里继续修改。",
            "使用Wisp时，可以把识别出来的文字直接粘贴回Word或者其他编辑器里继续修改。",
        ),
        (
            "列表项各自一行，列表前被折断的句子照常合并",
            "Before you start, make sure that the following tools are\ninstalled on your machine:\n- Rust 1.80 or later\n- Node.js 22 with pnpm\n1. Clone the repository\n2. Run pnpm install",
            "Before you start, make sure that the following tools are installed on your machine:\n- Rust 1.80 or later\n- Node.js 22 with pnpm\n1. Clone the repository\n2. Run pnpm install",
        ),
        (
            "中文列表：句末单独的「人员：」不是标签，接回上一行",
            "需要准备下面这些材料，到窗口办理时一并交给工作\n人员：\n1、身份证原件\n2、户口本\n（三）两张一寸照片",
            "需要准备下面这些材料，到窗口办理时一并交给工作人员：\n1、身份证原件\n2、户口本\n（三）两张一寸照片",
        ),
        (
            "代码：缩进、分号与花括号的行保持原样",
            "fn main() {\n    let name = \"Wisp\";\n    println!(\"hello {name}\");\n}",
            "fn main() {\n    let name = \"Wisp\";\n    println!(\"hello {name}\");\n}",
        ),
        (
            "代码：缩进的块不接上后面没有缩进的行",
            "for item in items_to_process:\n    handle(item)\nprint(\"all items were processed\")\n\ndef greet(name):\n    return f\"Hello, {name}\"",
            "for item in items_to_process:\n    handle(item)\nprint(\"all items were processed\")\n\ndef greet(name):\n    return f\"Hello, {name}\"",
        ),
        (
            "说明文字里的缩进命令：前面的句子合并，命令与后面的行保持原样",
            "Run the following command to install the\ndependencies:\n    pnpm install --frozen-lockfile\nThen start the development server.",
            "Run the following command to install the dependencies:\n    pnpm install --frozen-lockfile\nThen start the development server.",
        ),
        (
            "终端输出：提示符、缩进与对齐的列",
            "$ cargo build --release\n   Compiling wisp v0.1.0\n    Finished `release` profile [optimized] target(s) in 42.0s\n$ ls target/release\nwisp  wisp.d",
            "$ cargo build --release\n   Compiling wisp v0.1.0\n    Finished `release` profile [optimized] target(s) in 42.0s\n$ ls target/release\nwisp  wisp.d",
        ),
        (
            "编译器报错：标签开头、缩进与对齐",
            "error[E0425]: cannot find value `naem` in this scope\n --> src/main.rs:3:20\n  |\n3 |     println!(\"{naem}\");\n  |                ^^^^ help: a local variable with a similar name exists",
            "error[E0425]: cannot find value `naem` in this scope\n --> src/main.rs:3:20\n  |\n3 |     println!(\"{naem}\");\n  |                ^^^^ help: a local variable with a similar name exists",
        ),
        (
            "日志：以标签开头的行各自一行",
            "INFO: server listening on port 8080 and ready\nWARN: certificate expires in 3 days, renew soon\nERROR: failed to connect to the database",
            "INFO: server listening on port 8080 and ready\nWARN: certificate expires in 3 days, renew soon\nERROR: failed to connect to the database",
        ),
        (
            "续行的命令：行尾的反斜杠不能接上下一行",
            "docker run --rm -p 8080:80 nginx:latest \\\n--name web-server-for-testing",
            "docker run --rm -p 8080:80 nginx:latest \\\n--name web-server-for-testing",
        ),
        (
            "TSV：制表符分隔的列",
            "名称\t数量\t单价\n苹果\t3\t5.00\n梨\t10\t2.50",
            "名称\t数量\t单价\n苹果\t3\t5.00\n梨\t10\t2.50",
        ),
        (
            "空格对齐的表格与 Markdown 表格",
            "Name        Age   City\nAlice       30    Paris\nBob         25    Berlin\n\n| Name | Age |\n| ---- | --- |\n| Alice | 30 |",
            "Name        Age   City\nAlice       30    Paris\nBob         25    Berlin\n\n| Name | Age |\n| ---- | --- |\n| Alice | 30 |",
        ),
        (
            "两个空格隔开的金额是对齐的列；句末两个空格是句间空格，照常合并",
            "Total amount due  1,234.00\nTax included  123.40\n\nThis sentence ends here.  The next one begins and\nwraps onto the following line.",
            "Total amount due  1,234.00\nTax included  123.40\n\nThis sentence ends here.  The next one begins and wraps onto the following line.",
        ),
        (
            "诗歌：以空行分节时，节内长短相近的诗行合成一行（预期如此）",
            "白日依山尽，黄河入海流。\n欲穷千里目，更上一层楼。\n\nThe river runs beneath the bridge tonight\nAnd every lantern carries someone's light\n\nWe walk until the morning finds the shore",
            "白日依山尽，黄河入海流。欲穷千里目，更上一层楼。\n\nThe river runs beneath the bridge tonight And every lantern carries someone's light\n\nWe walk until the morning finds the shore",
        ),
        (
            "诗歌：很短的诗行保持原样",
            "床前明月光，\n疑是地上霜。\n举头望明月，\n低头思故乡。",
            "床前明月光，\n疑是地上霜。\n举头望明月，\n低头思故乡。",
        ),
        (
            "首行缩进的段落：段内合并，下一段的缩进行另起",
            "\u{3000}\u{3000}第一段的第一行，前面空了两格，后面的文字被硬换\n行了，这一段到这里结束。\n\u{3000}\u{3000}第二段同样首行缩进，也被硬换行断开，需要\n连成一行。",
            "\u{3000}\u{3000}第一段的第一行，前面空了两格，后面的文字被硬换行了，这一段到这里结束。\n\u{3000}\u{3000}第二段同样首行缩进，也被硬换行断开，需要连成一行。",
        ),
        (
            "首行缩进（半角空格，识别结果里的相对缩进）",
            "    The first paragraph is indented and its lines are\nwrapped by the layout of the page.\n    The second paragraph starts here and also wraps\nacross two lines.",
            "    The first paragraph is indented and its lines are wrapped by the layout of the page.\n    The second paragraph starts here and also wraps across two lines.",
        ),
        (
            "标题之后的首行缩进段落",
            "第一章\u{3000}总则\n\u{3000}\u{3000}第一条\u{3000}为了规范剪贴板历史的同步，根据有关\n规定，制定本办法。",
            "第一章\u{3000}总则\n\u{3000}\u{3000}第一条\u{3000}为了规范剪贴板历史的同步，根据有关规定，制定本办法。",
        ),
        (
            "表单：以「标签：」开头的行各自一行",
            "姓名：张三，男，一九九零年出生，现居北京市海淀区\n电话：13800138000\n地址：北京市海淀区中关村大街一号院",
            "姓名：张三，男，一九九零年出生，现居北京市海淀区\n电话：13800138000\n地址：北京市海淀区中关村大街一号院",
        ),
        (
            "标签开头的段落可以接下一行",
            "Note: the configuration file is read once at startup, so\nchanges only take effect after a restart.",
            "Note: the configuration file is read once at startup, so changes only take effect after a restart.",
        ),
        (
            "Markdown：标题与引用各自一行",
            "# Getting started\nWisp stays in the background and wakes up only when\nyou press a hotkey.\n> Tip: hold Shift to keep the panel open\n> after the result appears.",
            "# Getting started\nWisp stays in the background and wakes up only when you press a hotkey.\n> Tip: hold Shift to keep the panel open\n> after the result appears.",
        ),
        (
            "分隔线、路径与链接的清单",
            "Chapter One: The Beginning of It All\n==============\nIt was a quiet morning when the first message arrived.\n\napps/desktop/src-tauri/src/main.rs\nhttps://example.com/docs/getting-started\nsupport@example.com",
            "Chapter One: The Beginning of It All\n==============\nIt was a quiet morning when the first message arrived.\n\napps/desktop/src-tauri/src/main.rs\nhttps://example.com/docs/getting-started\nsupport@example.com",
        ),
        (
            "很短的独立行：菜单项、称呼",
            "Open Recent File\nSave As Template\nExport to PDF\n\nDear Alice,\nThank you for the detailed report on the new release.",
            "Open Recent File\nSave As Template\nExport to PDF\n\nDear Alice,\nThank you for the detailed report on the new release.",
        ),
        (
            "换行符原样保留；接缝处的行尾空白去掉，最后一行的行尾空白不动",
            "first line that is long enough to join   \r\nthe second line  \r\n\r\nnext paragraph\r\n",
            "first line that is long enough to join the second line  \r\n\r\nnext paragraph\r\n",
        ),
        (
            "单独的 \\r 也是换行",
            "a first line that is long enough\rand its continuation\r",
            "a first line that is long enough and its continuation\r",
        ),
        ("空文本", "", ""),
        ("只有一行", "single line", "single line"),
        ("只有空行", "\n \n\u{3000}\n", "\n \n\u{3000}\n"),
    ];

    #[test]
    fn reflow_cases() {
        for &(name, input, want) in REFLOW_CASES {
            let out = reflow(input);
            assert_eq!(out, want, "{name}");
            assert_eq!(can_reflow(input), want != input, "{name}：是否适用与结果是否不同一致");
            assert_eq!(reflow(&out), out, "{name}：幂等");
        }
    }

    #[test]
    fn reflow_stays_linear_on_long_lines() {
        // 每行的特征只扫一遍；接缝只回看上一行末尾的有限几个字符
        let line = "word ".repeat(40);
        let text = format!("{line}\n").repeat(5_000);
        let out = reflow(&text);
        assert_eq!(out.matches('\n').count(), 1);
        let cjk = "中文".repeat(20_000);
        assert_eq!(reflow(&format!("{cjk}\n{cjk}")), format!("{cjk}{cjk}"));
    }

    /// 生成文字用的字符：覆盖各条拼接规则涉及的字符类别
    const ALPHABET: &[char] = &[
        'a', 'b', 'Z', 'é', 'ж', '1', '2', '中', '文', 'あ', 'カ', '한', '글', '-', '\u{2010}', '\u{AD}', '—', '…',
        '“', '”', '·', '，', '。', '「', '/', '_', '@', '.', ',', '(', ')', ' ', ' ', '\u{3000}', '\t', '\n', '\n',
        '\r',
    ];

    fn text() -> impl Strategy<Value = String> {
        proptest::collection::vec(proptest::sample::select(ALPHABET), 0..80).prop_map(|v| v.into_iter().collect())
    }

    /// `out` 去掉空白后，是否是 `input` 去掉空白后只删掉若干连字符与软连字符的结果
    fn same_except_dropped_hyphens(input: &str, out: &str) -> bool {
        let mut want = input.chars().filter(|c| !c.is_whitespace());
        for c in out.chars().filter(|c| !c.is_whitespace()) {
            loop {
                match want.next() {
                    Some(w) if w == c => break,
                    Some('-' | '\u{2010}' | '\u{AD}') => {}
                    _ => return false,
                }
            }
        }
        want.all(|w| matches!(w, '-' | '\u{2010}' | '\u{AD}'))
    }

    /// 空白与换行占一半的文字：便于凑出行尾空白、空行、缩进与 \r\n
    fn spacey_text() -> impl Strategy<Value = String> {
        const CHARS: &[char] = &[
            'a', 'Z', '中', '1', '-', '|', ' ', ' ', '\t', '\u{3000}', '\u{A0}', '\u{2028}', '\n', '\n', '\n', '\r',
        ];
        proptest::collection::vec(proptest::sample::select(CHARS), 0..60).prop_map(|v| v.into_iter().collect())
    }

    /// 由词语拼成的多行文字：多数是普通的词，行足够长、常有可以合并的换行；夹着的特殊片段让「缩进」「标签」「列表项」、
    /// 对齐的列、代码与提示符这些规则都有机会生效
    fn paragraphs() -> impl Strategy<Value = String> {
        // 每个词都写得很短：数组里有长元素时 rustfmt 会把整个数组拆成一行一个
        const WORDS: &[&str] = &[
            "the ", "Wisp ", "inter-", "face ", "COVID-", "19 ", "中文", "被硬", "换行", "，", "。", ". ", "! ", "“",
            "。”", "x", "Ab ", "한글 ", "支持", "OpenAI ", "and ", "text",
        ];
        const SPECIAL: &[&str] = &[
            "- ", "1. ", "• ", "  ", "\t", " ", "\u{3000}", "# ", "> ", "$ ", "//", "|", ";", "{", "}", "\\", "Note: ",
            "电：", "下：", "a.co/x", "a@b.c", "src/", "——", "……", "\u{AD}", "===",
        ];
        const BREAKS: &[&str] = &["\n", "\n", "\n", "\r\n", "\r", "\n\n", "\n \n"];
        let token = prop_oneof![
            4 => proptest::sample::select(WORDS),
            1 => proptest::sample::select(SPECIAL),
        ];
        let line = move || proptest::collection::vec(token.clone(), 0..14).prop_map(|v| v.concat());
        (
            proptest::collection::vec((line(), proptest::sample::select(BREAKS)), 0..8),
            line(),
        )
            .prop_map(|(lines, last)| {
                let mut out: String = lines.into_iter().flat_map(|(l, b)| [l, b.to_owned()]).collect();
                out.push_str(&last);
                out
            })
    }

    /// 按 \r\n、\r、\n 断开的物理行
    fn physical_lines(s: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let mut rest = s;
        while let Some(i) = rest.find(['\r', '\n']) {
            out.push(&rest[..i]);
            let skip = if rest[i..].starts_with("\r\n") { 2 } else { 1 };
            rest = &rest[i + skip..];
        }
        out.push(rest);
        out
    }

    proptest! {
        #[test]
        fn squeeze_blanks_is_idempotent(s in spacey_text()) {
            let once = squeeze_blanks(&s);
            prop_assert_eq!(squeeze_blanks(&once), once);
        }

        #[test]
        fn squeeze_blanks_only_touches_whitespace(s in spacey_text()) {
            let out = squeeze_blanks(&s);
            let visible = |t: &str| t.chars().filter(|c| !c.is_whitespace()).collect::<String>();
            prop_assert_eq!(visible(&out), visible(&s));
        }

        /// 非空行逐行对应（只去掉行尾空白，缩进不变、不拼接），空行至多一个、不在首尾
        #[test]
        fn squeeze_blanks_keeps_lines(s in spacey_text()) {
            let out = squeeze_blanks(&s);
            let kept: Vec<&str> = physical_lines(&s).into_iter().map(str::trim_end).filter(|l| !l.is_empty()).collect();
            let got: Vec<&str> = out.split('\n').filter(|l| !l.is_empty()).collect();
            prop_assert_eq!(got, kept);
            prop_assert!(!out.contains('\r') && !out.contains("\n\n\n"));
            prop_assert!(!out.starts_with('\n') && !out.ends_with('\n'));
            prop_assert!(out.split('\n').all(|l| l.trim_end() == l));
        }

        #[test]
        fn remove_blank_lines_is_idempotent(s in spacey_text()) {
            let once = remove_blank_lines(&s);
            prop_assert_eq!(remove_blank_lines(&once), once.clone());
            prop_assert!(!has_blank_lines(&once));
        }

        /// 「是否适用」与「结果是否不同」一致
        #[test]
        fn has_blank_lines_agrees_with_removal(s in spacey_text()) {
            prop_assert_eq!(has_blank_lines(&s), remove_blank_lines(&s) != s);
        }

        /// 留下的行与输入里的非空行逐行对应、连同换行符逐字相同（只删整行，不拼接、不改动行内容）
        #[test]
        fn remove_blank_lines_keeps_other_lines_verbatim(s in spacey_text()) {
            let out = remove_blank_lines(&s);
            let kept: Vec<&str> = split_lines(&s).filter(|(c, _)| !is_blank(c)).map(|(_, l)| l).collect();
            let got: Vec<&str> = split_lines(&out).map(|(_, l)| l).collect();
            prop_assert_eq!(got, kept);
        }

        #[test]
        fn join_all_is_idempotent(s in text()) {
            let once = join_all(&s);
            prop_assert_eq!(join_all(&once), once.clone());
            prop_assert!(!once.contains(['\n', '\r']));
        }

        /// 内部的断行与参照实现（按 \r\n、\r、\n 断开）一致，整行拼回去与原文相同
        #[test]
        fn split_lines_matches_reference(s in spacey_text()) {
            let contents: Vec<&str> = split_lines(&s).map(|(c, _)| c).collect();
            let mut want = physical_lines(&s);
            // 末尾的换行符不再起一个空行（与 str::lines 相同）
            if want.last() == Some(&"") {
                want.pop();
            }
            prop_assert_eq!(contents, want);
            prop_assert_eq!(split_lines(&s).map(|(_, l)| l).collect::<String>(), s);
        }

        /// 三种换行符（\n、\r\n、单独的 \r）在各个操作里含义相同：换成统一的 \n 不改变结果
        #[test]
        fn line_break_styles_are_equivalent(s in spacey_text(), t in text()) {
            let unify = |x: &str| x.replace("\r\n", "\n").replace('\r', "\n");
            prop_assert_eq!(join_all(&t), join_all(&unify(&t)));
            prop_assert_eq!(squeeze_blanks(&s), squeeze_blanks(&unify(&s)));
            prop_assert_eq!(unify(&remove_blank_lines(&s)), remove_blank_lines(&unify(&s)));
            prop_assert_eq!(has_blank_lines(&s), has_blank_lines(&unify(&s)));
        }

        #[test]
        fn join_all_keeps_visible_characters(s in text()) {
            let out = join_all(&s);
            prop_assert!(same_except_dropped_hyphens(&s, &out), "{:?} -> {:?}", s, out);
        }

        /// 一次拼接只在两行之间加一个空格、什么都不加，或删掉上一行末尾的一个连字符
        #[test]
        fn join_break_only_touches_the_seam(a in text(), b in text(), spaced in any::<bool>()) {
            let spacing = if spaced { MixedSpacing::Spaced } else { MixedSpacing::Tight };
            let mut out = a.clone();
            join_break(&mut out, &b, spacing);
            let direct = format!("{a}{b}");
            let space = format!("{a} {b}");
            let dropped = a
                .strip_suffix(['-', '\u{2010}', '\u{AD}'])
                .map(|head| format!("{head}{b}"));
            prop_assert!(
                out == direct || out == space || dropped.as_ref() == Some(&out),
                "{:?} + {:?} -> {:?}", a, b, out
            );
        }

        #[test]
        fn reflow_is_idempotent(s in paragraphs(), t in text()) {
            for x in [s, t] {
                let once = reflow(&x);
                prop_assert_eq!(reflow(&once), once.clone(), "{:?}", x);
                prop_assert!(!can_reflow(&once));
            }
        }

        #[test]
        fn reflow_keeps_visible_characters(s in paragraphs(), t in text()) {
            for x in [s, t] {
                let out = reflow(&x);
                prop_assert!(same_except_dropped_hyphens(&x, &out), "{:?} -> {:?}", x, out);
            }
        }

        /// 「是否适用」与「结果是否不同」一致
        #[test]
        fn can_reflow_agrees_with_reflow(s in paragraphs()) {
            prop_assert_eq!(can_reflow(&s), reflow(&s) != s);
        }

        /// 只合并段内的行：空行（段界）连同换行符逐字保留，行数只少不多
        #[test]
        fn reflow_keeps_blank_lines(s in paragraphs()) {
            let out = reflow(&s);
            let blanks = |x: &str| -> Vec<String> {
                split_lines(x).filter(|(c, _)| is_blank(c)).map(|(_, l)| l.to_owned()).collect()
            };
            prop_assert_eq!(blanks(&out), blanks(&s));
            prop_assert!(split_lines(&out).count() <= split_lines(&s).count());
        }

        /// 三种换行符含义相同：换成统一的 \n 不改变合并的结果
        #[test]
        fn reflow_treats_line_break_styles_alike(s in paragraphs()) {
            let unify = |x: &str| x.replace("\r\n", "\n").replace('\r', "\n");
            prop_assert_eq!(unify(&reflow(&s)), reflow(&unify(&s)));
            prop_assert_eq!(can_reflow(&s), can_reflow(&unify(&s)));
        }
    }
}
