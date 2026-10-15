//! 术语表匹配测试。

use wisp_prompts::GlossaryMatcher;
use wisp_types::GlossaryEntry;

fn e(source: &str, target: &str) -> GlossaryEntry {
    GlossaryEntry {
        source: source.to_owned(),
        target: target.to_owned(),
        note: None,
    }
}

fn sources(hits: &[GlossaryEntry]) -> Vec<&str> {
    hits.iter().map(|h| h.source.as_str()).collect()
}

#[test]
fn latin_terms_are_whole_word_and_case_insensitive() {
    let m = GlossaryMatcher::new(&[e("token", "令牌")]);
    assert!(m.hits("Tokens are cheap").is_empty(), "Tokens 不命中 token");
    assert_eq!(sources(&m.hits("Token expired")), ["token"], "Token 命中 token");
    assert_eq!(sources(&m.hits("the TOKEN.")), ["token"]);
    assert!(m.hits("mytoken").is_empty());
    assert!(m.hits("token_id").is_empty(), "下划线算词字符");
    assert!(m.hits("token2").is_empty(), "数字算词字符");
    assert_eq!(sources(&m.hits("(token)")), ["token"]);
    assert_eq!(sources(&m.hits("access-token")), ["token"], "连字符是词边界");
    // CJK 字符不算词字符：中文里紧挨的英文照样命中
    assert_eq!(sources(&m.hits("使用token进行认证")), ["token"]);
    // 第一次出现不是整词、后面有整词出现时仍命中
    assert_eq!(sources(&m.hits("tokens and a token")), ["token"]);
}

#[test]
fn latin_case_folding_covers_unicode_letters() {
    // 「拉丁文：大小写不敏感」不只是 ASCII
    let m = GlossaryMatcher::new(&[
        e("Café", "咖啡馆"),
        e("naïve", "朴素的"),
        e("λόγος", "逻各斯"),
        e("Привет", "你好"),
        e("token", "令牌"),
    ]);
    assert_eq!(sources(&m.hits("CAFÉ AU LAIT")), ["Café"]);
    assert_eq!(sources(&m.hits("a NAÏVE approach")), ["naïve"]);
    assert_eq!(sources(&m.hits("Ο ΛΌΓΟΣ")), ["λόγος"], "大写希腊文（词尾 Σ 对应 ς）");
    assert_eq!(sources(&m.hits("ПРИВЕТ, мир")), ["Привет"]);
    // 整词规则照常生效
    assert!(m.hits("cafés").is_empty());
    assert!(m.hits("ПРИВЕТСТВИЕ").is_empty());
    // 折叠后变长的字符（İ）出现在命中之前：位置仍按原文排序
    assert_eq!(sources(&m.hits("İstanbul TOKEN, then CAFÉ")), ["token", "Café"]);
    // 去重同样大小写不敏感
    let dedup = GlossaryMatcher::new(&[e("Café", "a"), e("CAFÉ", "b")]);
    assert_eq!(dedup.len(), 1);
}

#[test]
fn chinese_terms_are_substrings() {
    let m = GlossaryMatcher::new(&[e("划词翻译", "selection translation")]);
    assert_eq!(
        sources(&m.hits("划词翻译功能")),
        ["划词翻译"],
        "划词翻译功能 命中 划词翻译"
    );
    assert_eq!(sources(&m.hits("支持划词翻译。")), ["划词翻译"]);
    assert!(m.hits("划词").is_empty());
}

#[test]
fn mixed_terms_check_only_latin_edges() {
    let m = GlossaryMatcher::new(&[e("AI 智能体", "AI agent"), e("Wi-Fi", "Wi-Fi")]);
    assert_eq!(sources(&m.hits("一个AI 智能体框架")), ["AI 智能体"]);
    assert!(m.hits("OpenAI 智能体").is_empty(), "左边紧挨字母");
    assert_eq!(sources(&m.hits("打开 wi-fi 开关")), ["Wi-Fi"]);
}

#[test]
fn hits_are_deduplicated_and_ordered_by_first_occurrence() {
    let m = GlossaryMatcher::new(&[
        e("cache", "缓存"),
        e("token", "令牌"),
        e("Wisp", "Wisp"),
        e("TOKEN", "重复"),
    ]);
    let hits = m.hits("Wisp stores the token in a cache; the token is reused, Wisp says.");
    assert_eq!(sources(&hits), ["Wisp", "token", "cache"]);
    // 同一个 source 的重复条目保留第一条
    assert_eq!(hits[1].target, "令牌");
    assert_eq!(m.len(), 3);
}

#[test]
fn overlapping_terms_both_hit() {
    let m = GlossaryMatcher::new(&[e("token", "令牌"), e("access token", "访问令牌")]);
    let hits = m.hits("Send the access token.");
    // 同一位置的较长术语在前
    assert_eq!(sources(&hits), ["access token", "token"]);
}

#[test]
fn at_most_30_hits_keep_the_earliest() {
    let entries: Vec<GlossaryEntry> = (0..35)
        .map(|i| e(&format!("term{i:02}"), &format!("术语{i}")))
        .collect();
    let m = GlossaryMatcher::new(&entries);
    // 倒序出现：term34 最先出现
    let text: Vec<String> = (0..35).rev().map(|i| format!("term{i:02}")).collect();
    let hits = m.hits(&text.join(" "));
    assert_eq!(hits.len(), 30, "35 条命中时取最靠前的 30 条");
    assert_eq!(hits[0].source, "term34");
    assert_eq!(hits[29].source, "term05");
}

#[test]
fn empty_and_blank_sources_are_ignored() {
    let m = GlossaryMatcher::new(&[e("", "x"), e("   ", "y"), e(" token ", "令牌")]);
    assert_eq!(m.len(), 1);
    assert_eq!(sources(&m.hits("a token")), [" token "]);
    let empty = GlossaryMatcher::new(&[]);
    assert!(empty.is_empty());
    assert!(empty.hits("anything").is_empty());
    assert_eq!(empty.version(), 0);
}

#[test]
fn version_changes_with_content() {
    let a = GlossaryMatcher::new(&[e("token", "令牌")]);
    let b = GlossaryMatcher::new(&[e("token", "令牌"), e("cache", "缓存")]);
    let c = GlossaryMatcher::new(&[e("token", "令牌")]);
    assert_ne!(a.version(), b.version());
    assert_eq!(a.version(), c.version(), "相同内容的版本稳定");
}

#[test]
fn large_glossary_scales() {
    // 1000 条术语、8000 字符原文、30 条命中
    let entries: Vec<GlossaryEntry> = (0..1000).map(|i| e(&format!("term{i}"), &format!("术语{i}"))).collect();
    let m = GlossaryMatcher::new(&entries);
    let mut text = String::new();
    for i in 0..40 {
        text.push_str(&format!("The term{i} appears here. "));
    }
    while text.len() < 8000 {
        text.push_str("filler words without glossary hits. ");
    }
    let hits = m.hits(&text);
    assert_eq!(hits.len(), 30);
    assert_eq!(hits[0].source, "term0");
}
