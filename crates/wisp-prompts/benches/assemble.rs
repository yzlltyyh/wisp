//! 提示词装配基准：检测 + 词典判定 + 术语匹配 + 渲染 + 装配 ≤ 1 ms（8000 字符、30 条命中、1000 条术语），
//! 后处理 100 KB 总共 ≤ 2 ms（普通输出，以及整段被引号包住、配对跟踪器全程工作的最坏情况）。
//!
//! 流水线分两种原文各测一次：中英混排（含汉字，检测只做文字统计）与纯英文（最常见的长段英文选区，检测会调用
//! whatlang，是更慢的一条路径）。
//!
//! 运行：`cargo bench -p wisp-prompts --bench assemble -- --quick`。除 criterion 的统计外，开始时先跑一道**门禁**：
//! 两种原文各逐次计时 3000 次完整流水线取 p99、逐次计时 50 次 100 KB 后处理取中位数，超出预算时以非零状态退出。

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::Criterion;
use wisp_prompts::{
    CacheKeyInput, GlossaryMatcher, PostKind, PostProcessor, PromptCtx, PromptTask, build, builtin_styles, cache_key,
    detect_lang, is_dictionary_input, render, target_for, use_dictionary,
};
use wisp_types::{DictMode, GlossaryEntry, Lang, Style, TaskKind};

/// 流水线预算
const PIPELINE_P99_BUDGET: Duration = Duration::from_millis(1);
/// 后处理 100 KB 的预算
const POST_100KB_BUDGET: Duration = Duration::from_millis(2);

/// 8000 字符的中英混排原文，含 40 处术语
fn source_text() -> String {
    let mut text = String::new();
    let mut i = 0;
    while text.chars().count() < 8000 {
        text.push_str(&format!(
            "The term{i} appears in this paragraph about latency and the token cache. 我们在前端用 React 渲染第 {i} 段。\n"
        ));
        i += 1;
    }
    text.chars().take(8000).collect()
}

/// 8000 字符的纯英文原文，含 40 多处术语。不含汉字，检测走 whatlang 兜底（较慢的路径）
fn english_source_text() -> String {
    let mut text = String::new();
    let mut i = 0;
    while text.len() < 8000 {
        text.push_str(&format!(
            "The term{i} appears in this paragraph about latency and the token cache, which the front end renders with React.\n"
        ));
        i += 1;
    }
    text.truncate(8000);
    text
}

/// 1000 条术语
fn glossary_entries() -> Vec<GlossaryEntry> {
    (0..1000)
        .map(|i| GlossaryEntry {
            source: format!("term{i}"),
            target: format!("术语{i}"),
            note: None,
        })
        .collect()
}

/// 一次完整的「面板可见 → 发出请求」本地处理：检测、方向、词典判定、术语匹配、装配、缓存键
fn pipeline(text: &str, style: &Style, matcher: &GlossaryMatcher) -> u128 {
    let source = detect_lang(text);
    let target = target_for(&source, Lang::ZhCn, Lang::En);
    let dict = use_dictionary(text, style, DictMode::Auto);
    let hits = matcher.hits(text);
    let ctx = PromptCtx {
        text,
        source,
        target,
        glossary: &hits,
        reasoning_model: false,
    };
    let built = build(&PromptTask::Translate { style, dict }, &ctx);
    black_box(&built);
    cache_key(&CacheKeyInput {
        provider_id: "openai-main",
        model_id: "gpt-4.1",
        task: TaskKind::Translate,
        style: Some(style),
        dict,
        source_lang: source.lang,
        target_lang: target,
        glossary_version: matcher.version(),
        text,
    })
}

/// 100 KB 模型输出，按 ~6 字节一个 delta 切分（接近真实 SSE 的 token 粒度）
fn output_chunks() -> (String, Vec<(usize, usize)>) {
    let unit = "这是第 N 段译文，包含 `code`、<tag> 和 \"引号\"。\nThe quick brown fox jumps over the lazy dog.\n";
    chunked(unit.repeat(100 * 1024 / unit.len() + 1))
}

/// 最坏情况：整段 100 KB 被一对引号包住（内含成对的内层引号），配对跟踪器从头到尾都在工作
fn wrapped_output_chunks() -> (String, Vec<(usize, usize)>) {
    let (text, _) = output_chunks();
    chunked(format!("\"{text}\""))
}

fn chunked(text: String) -> (String, Vec<(usize, usize)>) {
    let mut ranges = Vec::new();
    let mut start = 0;
    for (i, _) in text.char_indices().skip(1) {
        if i - start >= 6 {
            ranges.push((start, i));
            start = i;
        }
    }
    ranges.push((start, text.len()));
    (text, ranges)
}

fn post_all(text: &str, ranges: &[(usize, usize)]) -> usize {
    let mut p = PostProcessor::new(PostKind::Translate, "source");
    let mut n = 0;
    for &(a, b) in ranges {
        if let Some(s) = p.push(&text[a..b]) {
            n += s.len();
        }
    }
    n + p.finish().tail.len()
}

fn percentile(samples: &mut [Duration], p: f64) -> Duration {
    samples.sort_unstable();
    let idx = ((samples.len() as f64 - 1.0) * p).round() as usize;
    samples[idx]
}

/// 逐次计时 3000 次完整流水线，返回（p50, p99）
fn pipeline_percentiles(text: &str, style: &Style, matcher: &GlossaryMatcher) -> (Duration, Duration) {
    for _ in 0..200 {
        black_box(pipeline(text, style, matcher));
    }
    let mut samples: Vec<Duration> = (0..3000)
        .map(|_| {
            let t = Instant::now();
            black_box(pipeline(black_box(text), style, matcher));
            t.elapsed()
        })
        .collect();
    (percentile(&mut samples, 0.5), percentile(&mut samples, 0.99))
}

#[allow(clippy::print_stderr)] // 基准门禁：结果输出到 stderr
fn gates() {
    let matcher = GlossaryMatcher::new(&glossary_entries());
    let style = builtin_styles()
        .into_iter()
        .find(|s| s.id == "technical")
        .expect("technical");

    let mut p99 = Duration::ZERO;
    let english = english_source_text();
    assert_eq!(detect_lang(&english).lang, Lang::En);
    for (label, text) in [("mixed zh/en", source_text()), ("pure English", english)] {
        assert_eq!(text.chars().count(), 8000);
        assert_eq!(matcher.hits(&text).len(), 30, "基准输入应命中 30 条术语");
        let (p50, q99) = pipeline_percentiles(&text, &style, &matcher);
        eprintln!(
            "[gate] pipeline {label} (8000 chars, 1000 terms, 30 hits): p50 {p50:?}, p99 {q99:?} (budget {PIPELINE_P99_BUDGET:?})"
        );
        p99 = p99.max(q99);
    }

    let post_p50 = |(out, ranges): (String, Vec<(usize, usize)>), label: &str| {
        let mut post: Vec<Duration> = (0..50)
            .map(|_| {
                let t = Instant::now();
                black_box(post_all(black_box(&out), &ranges));
                t.elapsed()
            })
            .collect();
        let p50 = percentile(&mut post, 0.5);
        eprintln!(
            "[gate] post-process 100 KB {label} in {} deltas: p50 {p50:?} (budget {POST_100KB_BUDGET:?})",
            ranges.len()
        );
        p50
    };
    let post_p50 = post_p50(output_chunks(), "plain").max(post_p50(wrapped_output_chunks(), "wrapped in quotes"));

    let mut failed = false;
    if p99 > PIPELINE_P99_BUDGET {
        eprintln!("[gate] FAIL: pipeline p99 {p99:?} > {PIPELINE_P99_BUDGET:?}");
        failed = true;
    }
    if post_p50 > POST_100KB_BUDGET {
        eprintln!("[gate] FAIL: post-process 100 KB {post_p50:?} > {POST_100KB_BUDGET:?}");
        failed = true;
    }
    if failed {
        std::process::exit(1);
    }
}

fn benches(c: &mut Criterion) {
    let text = source_text();
    let english = english_source_text();
    let entries = glossary_entries();
    let matcher = GlossaryMatcher::new(&entries);
    let styles = builtin_styles();
    let natural = &styles[0];
    let technical = &styles[1];
    let hits = matcher.hits(&text);

    c.bench_function("pipeline/8000chars_1000terms", |b| {
        b.iter(|| pipeline(black_box(&text), technical, &matcher))
    });
    c.bench_function("pipeline/8000chars_english_1000terms", |b| {
        b.iter(|| pipeline(black_box(&english), technical, &matcher))
    });
    c.bench_function("detect_lang/8000chars", |b| b.iter(|| detect_lang(black_box(&text))));
    c.bench_function("detect_lang/8000chars_english", |b| {
        b.iter(|| detect_lang(black_box(&english)))
    });
    c.bench_function("detect_lang/short", |b| b.iter(|| detect_lang(black_box("token"))));
    c.bench_function("is_dictionary_input/word", |b| {
        b.iter(|| is_dictionary_input(black_box("rule of thumb")))
    });
    c.bench_function("is_dictionary_input/8000chars", |b| {
        b.iter(|| is_dictionary_input(black_box(&text)))
    });
    c.bench_function("glossary/hits_8000chars_1000terms", |b| {
        b.iter(|| matcher.hits(black_box(&text)))
    });
    c.bench_function("glossary/build_1000terms", |b| {
        b.iter(|| GlossaryMatcher::new(black_box(&entries)))
    });
    c.bench_function("render/hard_rules_8000chars", |b| {
        let vars = wisp_prompts::RenderVars {
            source_lang: "English",
            target_lang: "Simplified Chinese",
            text: &text,
            glossary: "",
            tail_instruction: "Reply with the translation only.",
            draft: None,
            review: None,
        };
        let template = "<source_text>\n{{text}}\n</source_text>\n\n{{tail_instruction}}";
        b.iter(|| render(black_box(template), &vars))
    });
    c.bench_function("build/translate_8000chars_30hits", |b| {
        let source = detect_lang(&text);
        let ctx = PromptCtx {
            text: &text,
            source,
            target: Lang::ZhCn,
            glossary: &hits,
            reasoning_model: false,
        };
        b.iter(|| {
            build(
                &PromptTask::Translate {
                    style: natural,
                    dict: false,
                },
                black_box(&ctx),
            )
        })
    });
    c.bench_function("cache_key/8000chars", |b| {
        b.iter(|| {
            cache_key(&CacheKeyInput {
                provider_id: "p",
                model_id: "m",
                task: TaskKind::Translate,
                style: Some(natural),
                dict: false,
                source_lang: Lang::En,
                target_lang: Lang::ZhCn,
                glossary_version: 1,
                text: black_box(&text),
            })
        })
    });
    let (out, ranges) = output_chunks();
    c.bench_function("post/translate_100KB", |b| {
        b.iter(|| post_all(black_box(&out), &ranges))
    });
    let (out, ranges) = wrapped_output_chunks();
    c.bench_function("post/translate_100KB_wrapped", |b| {
        b.iter(|| post_all(black_box(&out), &ranges))
    });
}

fn main() {
    gates();
    let mut c = Criterion::default().configure_from_args();
    benches(&mut c);
    c.final_summary();
}
