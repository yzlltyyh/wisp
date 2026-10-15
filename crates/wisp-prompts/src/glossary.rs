//! 术语匹配（Aho-Corasick；拉丁文本做整词、大小写不敏感匹配）。
//!
//! - 只用 `source` 匹配原文（单向）；`note` 只在界面显示，不参与匹配、不注入、不影响版本号；
//! - 拉丁文：大小写不敏感、整词——术语首 / 尾是字母、数字或下划线时，原文中紧挨的字符不能也是这些
//!   （CJK 字符不算词字符，所以「使用token认证」能命中 token）；中文：子串匹配；
//! - 大小写不敏感覆盖 Unicode：术语含 ASCII 之外的大小写字母（Café、naïve、希腊文、西里尔文）时，
//!   原文与术语都做 Unicode 小写折叠后再匹配（希腊文词尾 ς 视同 σ）；只含 ASCII 的术语表走零开销的 ASCII 路径；
//! - 命中去重，按首次出现位置排序，最多 30 条（超过时取最靠前的 30 条）；
//! - 构建一次、匹配多次：术语表变化时由调用方重建。

use std::collections::HashSet;

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use wisp_types::GlossaryEntry;

use crate::hash::Fnv64;
use crate::script::is_word_char;

/// 每次最多注入的术语条数
pub(crate) const MAX_GLOSSARY_HITS: usize = 30;

/// 一个可匹配的术语
#[derive(Debug, Clone)]
struct Term {
    /// 在 `entries` 中的下标
    entry: usize,
    /// 术语首字符是词字符：匹配位置前面不能紧挨词字符
    check_left: bool,
    /// 术语尾字符是词字符：匹配位置后面不能紧挨词字符
    check_right: bool,
}

/// 术语匹配器
#[derive(Debug, Clone)]
pub struct GlossaryMatcher {
    /// 有效条目（source 非空、按大小写不敏感去重，保持原顺序）
    entries: Vec<GlossaryEntry>,
    /// 第 i 个模式对应的术语
    terms: Vec<Term>,
    /// 没有任何有效条目时为 None
    automaton: Option<AhoCorasick>,
    /// 有术语含 ASCII 之外的大小写字母：原文含非 ASCII 字符时先做 Unicode 小写折叠
    unicode_fold: bool,
    version: u64,
}

impl GlossaryMatcher {
    /// 构建匹配器（Aho-Corasick；拉丁文本做整词、大小写不敏感匹配）
    pub fn new(entries: &[GlossaryEntry]) -> Self {
        let mut kept: Vec<GlossaryEntry> = Vec::with_capacity(entries.len());
        let mut seen: HashSet<String> = HashSet::with_capacity(entries.len());
        for e in entries {
            let source = e.source.trim();
            if source.is_empty() {
                continue;
            }
            // 同一个 source（大小写不敏感）出现多次：保留第一条
            if seen.insert(fold(source)) {
                kept.push(e.clone());
            }
        }

        let terms: Vec<Term> = kept
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let s = e.source.trim();
                Term {
                    entry: i,
                    check_left: s.chars().next().is_some_and(is_word_char),
                    check_right: s.chars().next_back().is_some_and(is_word_char),
                }
            })
            .collect();

        let unicode_fold = kept.iter().any(|e| has_non_ascii_cased(e.source.trim()));
        let automaton = if kept.is_empty() {
            None
        } else {
            // 模式用折叠后的小写形式；ASCII 部分另由 ascii_case_insensitive 兜底，原文可以不折叠
            let patterns = kept.iter().map(|e| fold(e.source.trim()));
            match AhoCorasickBuilder::new()
                .ascii_case_insensitive(true)
                .match_kind(MatchKind::Standard)
                .build(patterns)
            {
                Ok(ac) => Some(ac),
                Err(err) => {
                    // 只有术语表极端庞大时才会失败；退化为不注入术语，不影响翻译本身
                    log::warn!("glossary automaton build failed ({} entries): {err}", kept.len());
                    None
                }
            }
        };

        let version = version_of(&kept);
        Self {
            entries: kept,
            terms,
            automaton,
            unicode_fold,
            version,
        }
    }

    /// 命中的条目：去重，最多 30 条，按首次出现位置排序
    pub fn hits(&self, text: &str) -> Vec<GlossaryEntry> {
        let Some(ac) = &self.automaton else {
            return Vec::new();
        };
        // 每个条目首次（整词）命中的位置
        let mut first: Vec<usize> = vec![usize::MAX; self.entries.len()];
        let mut any = false;
        let mut consider = |pattern: usize, start: usize, end: usize| {
            let term = &self.terms[pattern];
            if first[term.entry] <= start {
                return;
            }
            if term.check_left && text[..start].chars().next_back().is_some_and(is_word_char) {
                return;
            }
            if term.check_right && text[end..].chars().next().is_some_and(is_word_char) {
                return;
            }
            first[term.entry] = start;
            any = true;
        };
        if self.unicode_fold && !text.is_ascii() {
            let folded = Folded::new(text);
            for m in ac.find_overlapping_iter(folded.text.as_str()) {
                consider(m.pattern().as_usize(), folded.orig(m.start()), folded.orig(m.end()));
            }
        } else {
            for m in ac.find_overlapping_iter(text) {
                consider(m.pattern().as_usize(), m.start(), m.end());
            }
        }
        if !any {
            return Vec::new();
        }
        let mut hit: Vec<(usize, usize)> = first
            .iter()
            .enumerate()
            .filter(|(_, p)| **p != usize::MAX)
            .map(|(i, p)| (*p, i))
            .collect();
        // 同一位置命中多个术语时，较长的（更具体的）在前，再按条目顺序
        hit.sort_unstable_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| self.entries[b.1].source.len().cmp(&self.entries[a.1].source.len()))
                .then(a.1.cmp(&b.1))
        });
        hit.into_iter()
            .take(MAX_GLOSSARY_HITS)
            .map(|(_, i)| self.entries[i].clone())
            .collect()
    }

    /// 术语表版本（参与缓存键）：由有效条目的 source / target 内容哈希得到，空表为 0
    pub fn version(&self) -> u64 {
        self.version
    }

    /// 有效条目数
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否没有任何有效条目
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for GlossaryMatcher {
    fn default() -> Self {
        Self::new(&[])
    }
}

/// 大小写折叠：Unicode 小写；希腊文词尾 ς 归为 σ（逐字符小写不区分词尾形式）
fn fold_char(c: char, out: &mut String) {
    if c.is_ascii() {
        out.push(c.to_ascii_lowercase());
    } else if c == 'ς' {
        out.push('σ');
    } else {
        out.extend(c.to_lowercase());
    }
}

fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        fold_char(c, &mut out);
    }
    out
}

/// 含 ASCII 之外的大小写字母（含 ǅ 这类标题大小写字母）
fn has_non_ascii_cased(s: &str) -> bool {
    s.chars()
        .any(|c| !c.is_ascii() && (c.is_lowercase() || c.is_uppercase() || c.to_lowercase().ne([c])))
}

/// 折叠后的原文与字节位置映射
struct Folded {
    text: String,
    /// 折叠后第 k 个字节对应的原文字节位置（末尾多一个 = 原文长度）；
    /// 所有字符折叠前后 UTF-8 长度相同时（拉丁、希腊、西里尔的常见情况）位置不变，为 None
    map: Option<Vec<usize>>,
}

impl Folded {
    fn new(src: &str) -> Self {
        let mut text = String::with_capacity(src.len());
        let mut map: Option<Vec<usize>> = None;
        for (i, c) in src.char_indices() {
            let before = text.len();
            fold_char(c, &mut text);
            let added = text.len() - before;
            if map.is_none() && added != c.len_utf8() {
                // 第一次出现长度变化：之前的位置一一对应
                let mut m = Vec::with_capacity(src.len() + 16);
                m.extend(0..before);
                map = Some(m);
            }
            if let Some(m) = map.as_mut() {
                m.extend(std::iter::repeat_n(i, added));
            }
        }
        if let Some(m) = map.as_mut() {
            m.push(src.len());
        }
        Self { text, map }
    }

    /// 折叠后的字节位置 → 原文字节位置（落在原文字符边界上）
    fn orig(&self, k: usize) -> usize {
        self.map.as_ref().map_or(k, |m| m[k])
    }
}

fn version_of(entries: &[GlossaryEntry]) -> u64 {
    if entries.is_empty() {
        return 0;
    }
    let mut h = Fnv64::new();
    for e in entries {
        h.field(e.source.trim().as_bytes());
        h.field(e.target.trim().as_bytes());
    }
    // 保证非空表的版本不为 0
    h.finish().max(1)
}

/// 渲染术语区块：模板的两行标题与说明 + 每条一行 `- src => tgt`。没有条目时为空串。
///
/// 换行会破坏区块结构，条目里的换行替换为空格；target 为空时视为「不译」（等于 source）。
pub(crate) fn render_glossary_block(header: &str, entries: &[GlossaryEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(header.len() + entries.len() * 32);
    out.push_str(header);
    for e in entries.iter().take(MAX_GLOSSARY_HITS) {
        let source = single_line(&e.source);
        let target = single_line(&e.target);
        let target = if target.is_empty() { source.clone() } else { target };
        out.push_str("\n- ");
        out.push_str(&source);
        out.push_str(" => ");
        out.push_str(&target);
    }
    out
}

fn single_line(s: &str) -> String {
    s.trim()
        .split(['\n', '\r'])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(s: &str, t: &str) -> GlossaryEntry {
        GlossaryEntry {
            source: s.to_owned(),
            target: t.to_owned(),
            note: None,
        }
    }

    #[test]
    fn version_tracks_content_not_notes() {
        let a = GlossaryMatcher::new(&[e("token", "令牌")]);
        let mut with_note = e("token", "令牌");
        with_note.note = Some("备注".to_owned());
        let b = GlossaryMatcher::new(&[with_note]);
        let c = GlossaryMatcher::new(&[e("token", "词元")]);
        assert_eq!(a.version(), b.version());
        assert_ne!(a.version(), c.version());
        assert_ne!(a.version(), 0);
        assert_eq!(GlossaryMatcher::new(&[]).version(), 0);
        assert_eq!(GlossaryMatcher::default().len(), 0);
    }

    #[test]
    fn folding_keeps_offsets_on_char_boundaries() {
        // İ 折叠后变长（i + 组合点），之后的位置要映射回原文
        let f = Folded::new("İİ CAFÉ");
        assert!(f.map.is_some());
        let at = f.text.find("café").expect("folded");
        assert_eq!(&"İİ CAFÉ"[f.orig(at)..f.orig(at + "café".len())], "CAFÉ");
        // 长度不变时不建映射
        assert!(Folded::new("ΛΌΓΟΣ Привет Ünïcode").map.is_none());
        assert_eq!(fold("ΛΌΓΟΣ λόγος"), "λόγοσ λόγοσ");
    }

    #[test]
    fn render_block_sanitizes_entries() {
        let out = render_glossary_block("H", &[e(" a\nb ", ""), e("x", " y ")]);
        assert_eq!(out, "H\n- a b => a b\n- x => y");
        assert_eq!(render_glossary_block("H", &[]), "");
    }
}
