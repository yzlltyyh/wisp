//! 模板资产与语言变体接口。
//!
//! `templates/` 下的每个文件以单个换行结尾（编辑器约定，`tests/template_sync.rs` 核对），这里在编译期去掉这一个换行，
//! 运行时零文件 IO、零分配。

/// 去掉文件末尾的单个换行（编译期执行）
const fn strip_final_newline(s: &'static str) -> &'static str {
    match s.as_bytes() {
        [rest @ .., b'\n'] => match core::str::from_utf8(rest) {
            Ok(v) => v,
            Err(_) => panic!("template is not valid UTF-8"),
        },
        _ => s,
    }
}

macro_rules! template_file {
    ($path:literal) => {
        strip_final_newline(include_str!(concat!("../templates/", $path)))
    };
}

/// 共用硬规则块，最后一行是 `{{glossary}}`
pub(crate) const HARD_RULES: &str = template_file!("hard_rules.txt");
/// 术语区块的两行标题与说明，其后逐行追加 `- src => tgt`
pub(crate) const GLOSSARY_BLOCK: &str = template_file!("glossary_block.txt");
/// 翻译 user 消息骨架
pub(crate) const USER_SKELETON: &str = template_file!("user_skeleton.txt");

pub(crate) const NATURAL_SYSTEM: &str = template_file!("styles/natural.system.txt");
pub(crate) const NATURAL_TAIL: &str = template_file!("styles/natural.tail.txt");
pub(crate) const TECHNICAL_SYSTEM: &str = template_file!("styles/technical.system.txt");
pub(crate) const TECHNICAL_TAIL: &str = template_file!("styles/technical.tail.txt");
pub(crate) const ACADEMIC_SYSTEM: &str = template_file!("styles/academic.system.txt");
pub(crate) const ACADEMIC_TAIL: &str = template_file!("styles/academic.tail.txt");
pub(crate) const CASUAL_SYSTEM: &str = template_file!("styles/casual.system.txt");
pub(crate) const CASUAL_TAIL: &str = template_file!("styles/casual.tail.txt");
pub(crate) const LEARN_SYSTEM: &str = template_file!("styles/learn.system.txt");
pub(crate) const LEARN_TAIL: &str = template_file!("styles/learn.tail.txt");
pub(crate) const LITERAL_SYSTEM: &str = template_file!("styles/literal.system.txt");
pub(crate) const LITERAL_TAIL: &str = template_file!("styles/literal.tail.txt");

/// 词典式 system
pub(crate) const DICTIONARY_SYSTEM: &str = template_file!("dictionary.system.txt");
/// technical 变体追加在 Rules 末尾的一行
pub(crate) const DICTIONARY_TECHNICAL_EXTRA: &str = template_file!("dictionary.technical.extra.txt");
/// casual 变体追加在 Rules 末尾的一行
pub(crate) const DICTIONARY_CASUAL_EXTRA: &str = template_file!("dictionary.casual.extra.txt");
/// 词典式 tail
pub(crate) const DICTIONARY_TAIL: &str = template_file!("dictionary.tail.txt");
/// 词典式少样本示例（当前为零样本，不注入，只作渲染夹具）
pub(crate) const DICTIONARY_EXAMPLE: &str = template_file!("dictionary.example.txt");

pub(crate) const POLISH_SYSTEM: &str = template_file!("polish.system.txt");
pub(crate) const PROOFREAD_SYSTEM: &str = template_file!("proofread.system.txt");
/// polish / proofread 共用的 user 模板
pub(crate) const EDIT_USER: &str = template_file!("edit.user.txt");

pub(crate) const OCR_BASE: &str = template_file!("ocr/base.txt");
pub(crate) const OCR_LINES: &str = template_file!("ocr/lines.txt");
pub(crate) const OCR_LINES_USER: &str = template_file!("ocr/lines.user.txt");
pub(crate) const OCR_PARAGRAPHS: &str = template_file!("ocr/paragraphs.txt");
pub(crate) const OCR_PARAGRAPHS_USER: &str = template_file!("ocr/paragraphs.user.txt");
pub(crate) const OCR_MARKDOWN: &str = template_file!("ocr/markdown.txt");
pub(crate) const OCR_MARKDOWN_USER: &str = template_file!("ocr/markdown.user.txt");
pub(crate) const OCR_TRANSLATE: &str = template_file!("ocr/translate.txt");
pub(crate) const OCR_TRANSLATE_USER: &str = template_file!("ocr/translate.user.txt");

pub(crate) const REFINE_REFLECT_SYSTEM: &str = template_file!("refine/reflect.system.txt");
pub(crate) const REFINE_REFLECT_USER: &str = template_file!("refine/reflect.user.txt");
pub(crate) const REFINE_IMPROVE_USER: &str = template_file!("refine/improve.user.txt");

/// 模板的语言变体。
///
/// 系统提示词默认英文；`Zh` 为中文系模型预留（DeepSeek / Qwen / GLM 上做 A/B 之后才决定是否启用）。
/// 目前还没有任何中文模板，`Zh` 一律回退到英文版本。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TemplateVariant {
    #[default]
    En,
    Zh,
}

/// 全部模板：(名称, 英文版本)。名称即 `templates/` 下去掉 `.txt` 的相对路径
const EN_TEMPLATES: &[(&str, &str)] = &[
    ("hard_rules", HARD_RULES),
    ("glossary_block", GLOSSARY_BLOCK),
    ("user_skeleton", USER_SKELETON),
    ("styles/natural.system", NATURAL_SYSTEM),
    ("styles/natural.tail", NATURAL_TAIL),
    ("styles/technical.system", TECHNICAL_SYSTEM),
    ("styles/technical.tail", TECHNICAL_TAIL),
    ("styles/academic.system", ACADEMIC_SYSTEM),
    ("styles/academic.tail", ACADEMIC_TAIL),
    ("styles/casual.system", CASUAL_SYSTEM),
    ("styles/casual.tail", CASUAL_TAIL),
    ("styles/learn.system", LEARN_SYSTEM),
    ("styles/learn.tail", LEARN_TAIL),
    ("styles/literal.system", LITERAL_SYSTEM),
    ("styles/literal.tail", LITERAL_TAIL),
    ("dictionary.system", DICTIONARY_SYSTEM),
    ("dictionary.technical.extra", DICTIONARY_TECHNICAL_EXTRA),
    ("dictionary.casual.extra", DICTIONARY_CASUAL_EXTRA),
    ("dictionary.tail", DICTIONARY_TAIL),
    ("dictionary.example", DICTIONARY_EXAMPLE),
    ("polish.system", POLISH_SYSTEM),
    ("proofread.system", PROOFREAD_SYSTEM),
    ("edit.user", EDIT_USER),
    ("ocr/base", OCR_BASE),
    ("ocr/lines", OCR_LINES),
    ("ocr/lines.user", OCR_LINES_USER),
    ("ocr/paragraphs", OCR_PARAGRAPHS),
    ("ocr/paragraphs.user", OCR_PARAGRAPHS_USER),
    ("ocr/markdown", OCR_MARKDOWN),
    ("ocr/markdown.user", OCR_MARKDOWN_USER),
    ("ocr/translate", OCR_TRANSLATE),
    ("ocr/translate.user", OCR_TRANSLATE_USER),
    ("refine/reflect.system", REFINE_REFLECT_SYSTEM),
    ("refine/reflect.user", REFINE_REFLECT_USER),
    ("refine/improve.user", REFINE_IMPROVE_USER),
];

/// 中文变体（预留，目前为空；加入时与英文版本同名）
const ZH_TEMPLATES: &[(&str, &str)] = &[];

/// 按 (名称, 变体) 取模板原文（未渲染）。名称不存在时返回 `None`；变体缺失时回退到英文版本
pub fn template(name: &str, variant: TemplateVariant) -> Option<&'static str> {
    let lookup = |table: &[(&'static str, &'static str)]| table.iter().find(|(n, _)| *n == name).map(|(_, t)| *t);
    match variant {
        TemplateVariant::Zh => lookup(ZH_TEMPLATES).or_else(|| lookup(EN_TEMPLATES)),
        TemplateVariant::En => lookup(EN_TEMPLATES),
    }
}

/// 全部模板名称（评测与测试用）
pub fn template_names() -> impl Iterator<Item = &'static str> {
    EN_TEMPLATES.iter().map(|(n, _)| *n)
}

/// 按变体取模板常量：变体表里没有就用英文
pub(crate) fn pick(name: &'static str, en: &'static str, variant: TemplateVariant) -> &'static str {
    match variant {
        TemplateVariant::En => en,
        TemplateVariant::Zh => ZH_TEMPLATES.iter().find(|(n, _)| *n == name).map_or(en, |(_, t)| *t),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn final_newline_is_stripped_once() {
        assert!(!HARD_RULES.ends_with('\n'));
        assert!(HARD_RULES.ends_with("{{glossary}}"));
        assert!(USER_SKELETON.ends_with("{{tail_instruction}}"));
        assert_eq!(strip_final_newline("a\n\n"), "a\n");
        assert_eq!(strip_final_newline("a"), "a");
    }

    #[test]
    fn variant_lookup_falls_back_to_english() {
        assert_eq!(template("hard_rules", TemplateVariant::En), Some(HARD_RULES));
        assert_eq!(template("hard_rules", TemplateVariant::Zh), Some(HARD_RULES));
        assert_eq!(template("nope", TemplateVariant::En), None);
        assert_eq!(pick("hard_rules", HARD_RULES, TemplateVariant::Zh), HARD_RULES);
        assert_eq!(template_names().count(), EN_TEMPLATES.len());
    }

    #[test]
    fn every_template_is_non_empty_without_trailing_whitespace() {
        for (name, t) in EN_TEMPLATES {
            assert!(!t.trim().is_empty(), "{name}");
            assert_eq!(t.trim_end(), *t, "{name} 末尾不应有空白");
        }
    }
}
