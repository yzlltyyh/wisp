//! 模板渲染：单遍、白名单式替换；替换进去的值不再展开；白名单之外的 {{x}} 原样保留。

/// 渲染变量（白名单：source_lang / target_lang / text / glossary / tail_instruction；refine 另有 draft / review）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RenderVars<'a> {
    /// 语言名用英文；低置信度时为 "the source language"
    pub source_lang: &'a str,
    pub target_lang: &'a str,
    pub text: &'a str,
    /// 没有命中术语时为空串（且不留空标题）
    pub glossary: &'a str,
    pub tail_instruction: &'a str,
    /// refine 改进步骤用
    pub draft: Option<&'a str>,
    pub review: Option<&'a str>,
}

impl<'a> RenderVars<'a> {
    /// 变量名 → 值；不在白名单或未提供（draft / review 为 None）时返回 None
    pub(crate) fn get(&self, name: &str) -> Option<&'a str> {
        match name {
            "source_lang" => Some(self.source_lang),
            "target_lang" => Some(self.target_lang),
            "text" => Some(self.text),
            "glossary" => Some(self.glossary),
            "tail_instruction" => Some(self.tail_instruction),
            "draft" => self.draft,
            "review" => self.review,
            _ => None,
        }
    }
}

/// 风格模板（内置与自定义风格的 system、tail）里可以使用的变量。
///
/// 只有语言名：`{{text}}` 若在 system 里展开，会把不可信的划词内容提升到 system 角色，绕过 `<source_text>`
/// 包裹与硬规则①的防注入设计；术语只放在硬规则末尾、tail 本身就在 user 消息里。其余变量在风格模板里原样保留。
pub(crate) const STYLE_VARIABLES: [&str; 2] = ["source_lang", "target_lang"];

/// 只做单遍替换；白名单之外的 {{x}} 原样保留
///
/// 从左到右扫描一次：遇到 `{{name}}` 且 `name` 在白名单里就写入对应的值，写入的值不会再被扫描，
/// 因此原文里出现的 `{{target_lang}}` 原样保留；硬规则里字面的 `{{var}}` 不在白名单里，同样原样保留。
pub fn render(template: &str, vars: &RenderVars<'_>) -> String {
    let extra = vars.text.len() + vars.glossary.len() + vars.tail_instruction.len();
    render_with(template, extra, |name, _| vars.get(name))
}

/// 单遍替换的实现：`lookup(变量名, 模板中占位符之前的全部文本)` 返回值，None 表示原样保留
pub(crate) fn render_with<'v>(
    template: &str,
    extra: usize,
    mut lookup: impl FnMut(&str, &str) -> Option<&'v str>,
) -> String {
    let mut out = String::with_capacity(template.len() + extra + 64);
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        let after_open = &rest[open + 2..];
        // 变量名只由 [a-z_] 组成且紧贴花括号（`{{ text }}` 不算变量），避免跨行或跨段误配
        let name_len = after_open
            .bytes()
            .take_while(|b| b.is_ascii_lowercase() || *b == b'_')
            .count();
        let closed = after_open[name_len..].starts_with("}}");
        let before = &template[..template.len() - rest.len() + open];
        match closed.then(|| lookup(&after_open[..name_len], before)).flatten() {
            Some(value) => {
                out.push_str(&rest[..open]);
                out.push_str(value);
                rest = &after_open[name_len + 2..];
            }
            None => {
                // 不是白名单变量：原样输出到第一个 '{' 为止，从下一个字符继续扫描（`{{{{var}}}}` 等形式逐字保留）
                out.push_str(&rest[..=open]);
                rest = &rest[open + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// 风格模板（system / tail）中不会被替换的变量（「未知变量」黄色提示用）。
///
/// 风格模板只替换 `{{source_lang}}` / `{{target_lang}}`；其余 `{{x}}`——包括其他模板里才有的 `text`、`glossary`、
/// `tail_instruction`、`draft`、`review`——在风格模板里都不支持，原样保留并在这里报告。
/// 返回去重后的变量名，保持首次出现顺序。只有 `{{` 与 `}}` 在同一行、中间不含花括号时才算一个变量。
pub fn unknown_variables(template: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        let after_open = &rest[open + 2..];
        let candidate = after_open.find("}}").map(|close| (close, &after_open[..close]));
        match candidate {
            Some((close, name)) if !name.is_empty() && !name.contains(['{', '}', '\n', '\r']) => {
                if !STYLE_VARIABLES.contains(&name) && !out.iter().any(|n| n == name) {
                    out.push(name.to_owned());
                }
                rest = &after_open[close + 2..];
            }
            _ => rest = &rest[open + 1..],
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> RenderVars<'static> {
        RenderVars {
            source_lang: "English",
            target_lang: "Simplified Chinese",
            text: "Hello {{target_lang}} {{text}}",
            glossary: "",
            tail_instruction: "Tail.",
            draft: None,
            review: None,
        }
    }

    #[test]
    fn replaces_whitelisted_variables_once() {
        let out = render(
            "From {{source_lang}} into {{target_lang}}:\n{{text}}\n{{tail_instruction}}",
            &vars(),
        );
        assert_eq!(
            out,
            "From English into Simplified Chinese:\nHello {{target_lang}} {{text}}\nTail."
        );
    }

    #[test]
    fn keeps_unknown_and_malformed_placeholders() {
        let v = vars();
        let keep = "{{var}} {{foo}} {{ text }} {{TEXT}} {name} %s $1";
        assert_eq!(render(keep, &v), keep);
        assert_eq!(render("{{{{var}}}}", &v), "{{{{var}}}}");
        assert_eq!(render("{{{target_lang}}}", &v), "{Simplified Chinese}");
        assert_eq!(render("unterminated {{target_lang", &v), "unterminated {{target_lang");
        assert_eq!(render("{{", &v), "{{");
        assert_eq!(render("}}{{}}", &v), "}}{{}}");
        assert_eq!(render("", &v), "");
        // draft / review 没有提供时原样保留
        assert_eq!(render("{{draft}}|{{review}}", &v), "{{draft}}|{{review}}");
        let v = RenderVars {
            draft: Some("D"),
            review: Some("R"),
            ..v
        };
        assert_eq!(render("{{draft}}|{{review}}", &v), "D|R");
    }

    #[test]
    fn multibyte_text_around_placeholders() {
        let v = RenderVars {
            text: "内卷", ..vars()
        };
        assert_eq!(
            render("「{{text}}」→{{target_lang}}。", &v),
            "「内卷」→Simplified Chinese。"
        );
    }

    #[test]
    fn lookup_sees_the_template_before_the_placeholder() {
        let mut seen = Vec::new();
        let out = render_with("a {{x}} b {{y}}", 0, |name, before| {
            seen.push((name.to_owned(), before.to_owned()));
            (name == "x").then_some("X")
        });
        assert_eq!(out, "a X b {{y}}");
        assert_eq!(
            seen,
            [
                ("x".to_owned(), "a ".to_owned()),
                ("y".to_owned(), "a {{x}} b ".to_owned())
            ]
        );
    }

    #[test]
    fn glossary_empty_string_is_inserted() {
        assert_eq!(render("a\n{{glossary}}", &vars()), "a\n");
    }

    #[test]
    fn unknown_variables_are_reported_once_in_order() {
        let t = "{{source_lang}} {{tone}} {{target_lang}} {{tone}} {{draft}} {{ text }} {{}}";
        assert_eq!(unknown_variables(t), vec!["tone", "draft", " text "]);
        assert_eq!(unknown_variables("{{{{x}}}}"), vec!["x"]);
        // 风格模板不支持 text / glossary / tail_instruction（不会展开）
        assert_eq!(
            unknown_variables("plain {{text}} {{glossary}} {{tail_instruction}} {{source_lang}}"),
            vec!["text", "glossary", "tail_instruction"]
        );
        assert!(unknown_variables("From {{source_lang}} into {{target_lang}}.").is_empty());
        assert!(unknown_variables("{{a\nb}}").is_empty());
    }
}
