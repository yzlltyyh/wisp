//! 翻译缓存键：
//! (provider_id, model_id, style_id + 模板和参数的哈希, dict, source_lang, target_lang, 术语表版本, text_hash)。

use wisp_types::{Lang, Style, TaskKind};

use crate::assemble::normalize_source;
use crate::hash::Fnv128;

/// 缓存键的全部维度
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CacheKeyInput<'a> {
    pub provider_id: &'a str,
    pub model_id: &'a str,
    /// polish / proofread 以动作 id 作为风格参与缓存
    pub task: TaskKind,
    /// 翻译时的风格（id + 模板 + 参数参与哈希）；polish / proofread 为 None
    pub style: Option<&'a Style>,
    pub dict: bool,
    pub source_lang: Lang,
    pub target_lang: Lang,
    pub glossary_version: u64,
    pub text: &'a str,
}

/// 键格式版本：键的组成方式改变时递增，避免新旧键混用
const KEY_FORMAT: &[u8] = b"wisp-prompts/cache-key/v1";

/// 计算缓存键（u128 不跨 IPC）
///
/// - 风格只取影响请求的部分：id、system 模板、tail、temperature；名称、快捷键、绑定模型（模型已是单独维度）、
///   输出格式（只影响渲染）不参与，改这些不会让缓存失效；
/// - 原文先做与装配相同的规范化（去首尾空行和行尾空白），只差空白的两次选择共用缓存。
///
/// **refine 不可缓存**：反思与改进的结果取决于译稿（⌘R 重新生成后译稿就变了），而 [`CacheKeyInput`] 没有译稿维度，
/// 用它缓存 refine 会让新译稿命中旧的改进结果。`task` 为 [`TaskKind::Refine`] 时调试构建直接断言失败；
/// 发布构建仍返回一个键（与 Translate 等任务的键互不冲突），但调用方不应该用它缓存。
pub fn cache_key(k: &CacheKeyInput<'_>) -> u128 {
    debug_assert!(
        k.task != TaskKind::Refine,
        "refine 的结果取决于译稿，CacheKeyInput 没有译稿维度，不能缓存"
    );
    let mut h = Fnv128::new();
    h.field(KEY_FORMAT);
    h.field(k.provider_id.as_bytes());
    h.field(k.model_id.as_bytes());
    h.u8(match k.task {
        TaskKind::Translate => 1,
        TaskKind::Polish => 2,
        TaskKind::Proofread => 3,
        TaskKind::Refine => 4,
    });
    match k.style {
        Some(style) => {
            h.u8(1);
            h.field(style.id.as_bytes());
            h.field(style.system_template.as_bytes());
            h.field(style.tail_instruction.as_bytes());
            match style.temperature {
                Some(t) => {
                    h.u8(1);
                    h.bytes(&t.to_bits().to_le_bytes());
                }
                None => h.u8(0),
            }
        }
        None => h.u8(0),
    }
    h.u8(u8::from(k.dict));
    h.field(k.source_lang.as_str().as_bytes());
    h.field(k.target_lang.as_str().as_bytes());
    h.u64(k.glossary_version);
    h.field(normalize_source(k.text).as_bytes());
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin_styles;

    fn base<'a>(style: &'a Style) -> CacheKeyInput<'a> {
        CacheKeyInput {
            provider_id: "openai-main",
            model_id: "gpt-4.1",
            task: TaskKind::Translate,
            style: Some(style),
            dict: false,
            source_lang: Lang::En,
            target_lang: Lang::ZhCn,
            glossary_version: 7,
            text: "Hello world",
        }
    }

    #[test]
    fn every_dimension_changes_the_key() {
        let styles = builtin_styles();
        let natural = &styles[0];
        let literal = &styles[5];
        let k = cache_key(&base(natural));
        let variants = [
            CacheKeyInput {
                provider_id: "other",
                ..base(natural)
            },
            CacheKeyInput {
                model_id: "gpt-4.1-mini",
                ..base(natural)
            },
            CacheKeyInput {
                task: TaskKind::Polish,
                style: None,
                ..base(natural)
            },
            CacheKeyInput {
                style: Some(literal),
                ..base(natural)
            },
            CacheKeyInput {
                dict: true,
                ..base(natural)
            },
            CacheKeyInput {
                source_lang: Lang::Other,
                ..base(natural)
            },
            CacheKeyInput {
                target_lang: Lang::En,
                ..base(natural)
            },
            CacheKeyInput {
                glossary_version: 8,
                ..base(natural)
            },
            CacheKeyInput {
                text: "Hello, world",
                ..base(natural)
            },
        ];
        let mut keys = vec![k];
        for v in &variants {
            keys.push(cache_key(v));
        }
        let mut unique = keys.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), keys.len(), "每个维度都应改变缓存键");
        // polish 与 proofread 以动作 id 区分
        let polish = cache_key(&CacheKeyInput {
            task: TaskKind::Polish,
            style: None,
            ..base(natural)
        });
        let proofread = cache_key(&CacheKeyInput {
            task: TaskKind::Proofread,
            style: None,
            ..base(natural)
        });
        assert_ne!(polish, proofread);
    }

    #[test]
    fn style_template_and_params_participate() {
        let natural = builtin_styles().remove(0);
        let k = cache_key(&base(&natural));
        let mut s = natural.clone();
        s.temperature = Some(0.5);
        assert_ne!(cache_key(&base(&s)), k);
        let mut s = natural.clone();
        s.temperature = None;
        assert_ne!(cache_key(&base(&s)), k);
        let mut s = natural.clone();
        s.system_template.push('!');
        assert_ne!(cache_key(&base(&s)), k);
        let mut s = natural.clone();
        s.tail_instruction.push('!');
        assert_ne!(cache_key(&base(&s)), k);
        // 名称、快捷键、输出格式、modified 不影响请求，不参与
        let mut s = natural.clone();
        s.name = "renamed".to_owned();
        s.hotkey = Some("Alt+Shift+N".to_owned());
        s.output = wisp_types::OutputFormat::MarkdownLite;
        s.modified = true;
        assert_eq!(cache_key(&base(&s)), k);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "不能缓存")]
    fn refine_is_not_cacheable() {
        let natural = builtin_styles().remove(0);
        cache_key(&CacheKeyInput {
            task: TaskKind::Refine,
            ..base(&natural)
        });
    }

    #[test]
    fn whitespace_only_differences_share_a_key() {
        let natural = builtin_styles().remove(0);
        let k = cache_key(&base(&natural));
        let padded = CacheKeyInput {
            text: "\n\nHello world   \n",
            ..base(&natural)
        };
        assert_eq!(cache_key(&padded), k);
        let crlf = CacheKeyInput {
            text: "a\r\nb",
            ..base(&natural)
        };
        let lf = CacheKeyInput {
            text: "a\nb",
            ..base(&natural)
        };
        assert_eq!(cache_key(&crlf), cache_key(&lf));
        // 首行缩进是内容的一部分
        let indented = CacheKeyInput {
            text: "  Hello world",
            ..base(&natural)
        };
        assert_ne!(cache_key(&indented), k);
        // 稳定：同一输入两次计算相同
        assert_eq!(cache_key(&base(&natural)), k);
    }
}
