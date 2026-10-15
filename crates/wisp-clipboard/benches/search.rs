//! 剪贴板检索与写入基准。
//!
//! 运行：`cargo bench -p wisp-clipboard --bench search -- --quick`
//!
//! 先执行「预算检查」：按目标逐项实测（取中位数）并打印结果表，任何一项超标都让基准以非零状态退出——
//! 1 万条搜索 < 30 ms、文本 ingest < 5 ms、4K 截图 ingest < 150 ms（不含后台缩略图）、
//! 1000 条文本 + 100 张图时 clipboard.db < 20 MB；另外记录（不设门槛）缩略图生成、打开数据库、缓存加载与首屏。
//!
//! 接近上限的长文本单独设预算（日常文本的 5 ms 预算不适用；语料最长 1.2 万字符）：1 MB 多样文本的 ingest
//! < 15 ms（疑似密钥扫描 + 前 32768 字符的 FTS 索引 + 写入 1 MB），以及最近 2000 条里有 30 条 1 MB 长文本时
//! 1–2 字短查询（LIKE）< 30 ms；500 条长文本、命中都在预览之外（每条结果都要生成上下文片段）时的搜索 < 30 ms。
//!
//! 批量删除（期间持有存储锁，搜索 / 翻页 / 记录都要等）：默认上限 1000 条时，用户触发的清空与
//! 「上限改为 10」< 100 ms，空闲清理里删掉一部分（2%、10%）< 250 ms；1 万条时分别 < 1 s、< 2.5 s（要读完并清零、
//! 改写约 70 MB 的页）。单条删除只记录不设门槛。
//! 然后由 criterion 输出各项的详细统计。`cargo test --benches`（测试模式，没有 `--bench` 参数）只跑冒烟。
#![allow(clippy::print_stdout)] // 基准的结果表直接打印到标准输出

#[path = "../tests/common/mod.rs"]
mod common;

use std::hint::black_box;
use std::path::Path;
use std::time::{Duration, Instant};

use common::corpus::clip_text;
use common::{Fixture, NOW, image_raw, png, policy, screenshot_like};
use criterion::{BatchSize, Criterion, criterion_group};
use wisp_clipboard::{ClipStore, IngestOutcome, RecentCache, RetentionPolicy};
use wisp_types::{ClipFilter, RawClipboard};

/// 搜索基准的数据量
const SEARCH_ROWS: u64 = 10_000;
/// 长文本的长度（= 默认 max_text_bytes）
const LONG_TEXT_BYTES: usize = 1_048_576;

/// 接近上限的多样文本：把语料（中英文、代码混合）拼接到 `bytes` 字节
fn long_text(seed: u64, bytes: usize) -> String {
    let mut s = String::with_capacity(bytes + 16 * 1024);
    let mut i = 1_000_000 + seed * 10_000;
    while s.len() < bytes {
        s.push_str(&clip_text(i).0);
        s.push('\n');
        i += 1;
    }
    let mut cut = bytes;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s
}

fn raw_from_corpus(i: u64) -> RawClipboard {
    let (text, html) = clip_text(i);
    RawClipboard {
        text: Some(text),
        html,
        ..RawClipboard::default()
    }
}

/// 1 万条语料（5 条置顶）
fn search_fixture() -> Fixture {
    corpus_fixture(SEARCH_ROWS)
}

/// n 条语料（每 2000 条有 1 条置顶）
fn corpus_fixture(n: u64) -> Fixture {
    let f = Fixture::new();
    let p = policy();
    for i in 0..n {
        let o = f.store.ingest(&raw_from_corpus(i), &p, NOW + i as i64).expect("ingest");
        if i % 2000 == 7
            && let IngestOutcome::Inserted(m) = o
        {
            f.store.set_pinned(m.id, true).expect("置顶");
        }
    }
    f
}

/// 典型查询：FTS（罕见 / 常见 / 中文 3 字 / 跨词）与 LIKE（1–2 字）
fn queries() -> Vec<(&'static str, String)> {
    // 从语料里取一段真实存在的中文 3 字
    let cjk3: String = (0..)
        .map(|i| clip_text(i).0)
        .find_map(|t| {
            let chars: Vec<char> = t.chars().collect();
            chars
                .windows(3)
                .find(|w| w.iter().all(|c| ('\u{4e00}'..='\u{9fff}').contains(c)))
                .map(|w| w.iter().collect())
        })
        .expect("语料中有中文");
    vec![
        ("fts_rare", "#4242".to_owned()),
        // 命中都在正文末尾（预览之外）：每条结果都要读正文生成上下文片段，是搜索的最坏路径
        ("fts_tail_snippet", "#42".to_owned()),
        ("fts_common_en", "the".to_owned()),
        ("fts_phrase", "quick brown".to_owned()),
        ("fts_cjk_3", cjk3),
        ("like_cjk_1", "的".to_owned()),
        ("like_cjk_2", "中国".to_owned()),
        ("like_en_2", "qu".to_owned()),
    ]
}

/// 把数据目录里的库（须已关闭、没有 WAL）复制到新的临时目录并打开：批量删除的各个场景从同一份数据开始
fn clone_db(src: &Path) -> Fixture {
    let dir = tempfile::tempdir().expect("临时目录");
    std::fs::copy(src.join("clipboard.db"), dir.path().join("clipboard.db")).expect("复制库");
    let store = ClipStore::open(dir.path()).expect("打开");
    Fixture { dir, store }
}

/// 批量删除的预算行：清空（含 / 不含置顶）、保留策略（上限改为 10；删掉最旧的 2%、10%）、单条删除。
/// `budget_ms`：清空与「上限改为 10」；`partial_ms`：删掉一部分
fn bulk_delete_rows(rows: &mut Vec<Row>, src: &Path, budget_ms: f64, partial_ms: f64) {
    let retain = |max_items: u32| RetentionPolicy {
        max_items,
        max_age_days: None,
    };
    let f = clone_db(src);
    let (d, removed) = time(|| f.store.clear(false).expect("清空"));
    let n = u64::from(removed);
    rows.push(row(format!("clear(false)（{n} 条）"), d, Some(budget_ms)));
    let f = clone_db(src);
    rows.push(row(
        format!("clear(true)（{n} 条，保留置顶）"),
        time(|| f.store.clear(true).expect("清空")).0,
        Some(budget_ms),
    ));
    let f = clone_db(src);
    let (d, report) = time(|| f.store.retain(&retain(10), NOW + 100_000).expect("保留"));
    assert!(u64::from(report.removed_items) + 20 > n);
    rows.push(row(format!("retain（{n} → 10 条）"), d, Some(budget_ms)));
    for (pct, label) in [(98, "删掉最旧的 2%：日常空闲清理"), (90, "删掉最旧的 10%")] {
        let f = clone_db(src);
        let keep = u32::try_from(n * pct / 100).expect("条数");
        rows.push(row(
            format!("retain（{n} → {keep} 条，{label}）"),
            time(|| f.store.retain(&retain(keep), NOW + 100_000).expect("保留")).0,
            Some(partial_ms),
        ));
    }
    let f = clone_db(src);
    let ids: Vec<i64> = f
        .store
        .recent(0, 5, ClipFilter::All)
        .expect("列表")
        .items
        .iter()
        .map(|h| h.item.id)
        .collect();
    let samples = ids
        .iter()
        .map(|id| time(|| f.store.delete(*id).expect("删除")).0)
        .collect();
    rows.push(row(format!("delete（{n} 条中删一条）中位数"), median(samples), None));
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort_unstable();
    v[v.len() / 2]
}

fn time<T>(f: impl FnOnce() -> T) -> (Duration, T) {
    let t = Instant::now();
    let r = f();
    (t.elapsed(), r)
}

/// 预算检查的一行
struct Row {
    name: String,
    value: String,
    budget: String,
    ok: bool,
}

fn row(name: impl Into<String>, d: Duration, budget_ms: Option<f64>) -> Row {
    let ms = d.as_secs_f64() * 1000.0;
    Row {
        name: name.into(),
        value: format!("{ms:.3} ms"),
        budget: budget_ms.map_or_else(|| "—".to_owned(), |b| format!("< {b} ms")),
        ok: budget_ms.is_none_or(|b| ms < b),
    }
}

/// 生成 n 张互不相同的 4K 截图
fn screenshots(n: u32) -> Vec<RawClipboard> {
    (0..n).map(|s| image_raw(screenshot_like(3840, 2160, s + 1))).collect()
}

fn budget_checks(smoke: bool) {
    let mut rows = Vec::new();

    // ---- 搜索：1 万条 < 30 ms ----
    let (build, f) = time(search_fixture);
    rows.push(row("（准备）写入 1 万条语料", build, None));
    let runs = if smoke { 1 } else { 15 };
    for (name, q) in queries() {
        let samples = (0..runs)
            .map(|_| time(|| f.store.search(&q, ClipFilter::All, 50).expect("搜索")).0)
            .collect();
        rows.push(row(
            format!("search/{name} 「{q}」（1 万条，limit 50）"),
            median(samples),
            Some(30.0),
        ));
    }
    let samples = (0..runs)
        .map(|_| time(|| f.store.search("the", ClipFilter::Code, 200).expect("搜索")).0)
        .collect();
    rows.push(row(
        "search/fts_common_en + 过滤器 Code（limit 200）",
        median(samples),
        Some(30.0),
    ));

    // ---- 首屏：缓存与查库 ----
    let (load, cache) = time(|| RecentCache::load(&f.store).expect("加载缓存"));
    rows.push(row("RecentCache::load（1 万条中的前 200）", load, None));
    let samples = (0..runs.max(5))
        .map(|_| time(|| black_box(cache.page(ClipFilter::All, 200))).0)
        .collect();
    rows.push(row(
        "RecentCache::page（首屏 200 条，不查库）",
        median(samples),
        Some(5.0),
    ));
    let samples = (0..runs)
        .map(|_| time(|| f.store.recent(0, 200, ClipFilter::All).expect("列表")).0)
        .collect();
    rows.push(row("ClipStore::recent(0, 200)", median(samples), Some(30.0)));
    let samples = (0..runs)
        .map(|_| time(|| f.store.recent(9000, 100, ClipFilter::All).expect("列表")).0)
        .collect();
    rows.push(row(
        "ClipStore::recent(9000, 100)（深分页）",
        median(samples),
        Some(30.0),
    ));

    // ---- 文本 ingest < 5 ms（含去重与 FTS 写入），在 1 万条的库上 ----
    let p = policy();
    let mut samples = Vec::new();
    for i in 0..(if smoke { 3 } else { 200 }) {
        let raw = raw_from_corpus(SEARCH_ROWS + 100 + i);
        samples.push(time(|| f.store.ingest(&raw, &p, NOW + 50_000 + i as i64).expect("ingest")).0);
    }
    let mut sorted = samples.clone();
    sorted.sort_unstable();
    let p95 = sorted[sorted.len() * 95 / 100];
    let worst = *sorted.last().expect("非空");
    rows.push(row(
        "ingest/text（语料分布，1 万条库）中位数",
        median(samples),
        Some(5.0),
    ));
    rows.push(row("ingest/text p95", p95, Some(5.0)));
    rows.push(row("ingest/text 最慢一次（长文本 / FTS 段合并）", worst, None));
    let raw = raw_from_corpus(3);
    let samples = (0..runs)
        .map(|_| time(|| f.store.ingest(&raw, &p, NOW).expect("ingest")).0)
        .collect();
    rows.push(row("ingest/text 去重命中（touch）", median(samples), Some(5.0)));

    // ---- 接近上限的长文本：1 MB ingest；最近 2000 条中有 30 条 1 MB 长文本时的短查询 ----
    {
        let f = Fixture::new();
        let n_long = if smoke { 2 } else { 30 };
        let mut samples = Vec::new();
        for i in 0..n_long {
            let raw = RawClipboard {
                text: Some(long_text(i, LONG_TEXT_BYTES)),
                ..RawClipboard::default()
            };
            samples.push(time(|| f.store.ingest(&raw, &p, NOW + i as i64).expect("ingest")).0);
        }
        rows.push(row(
            "ingest/text_1mb（多样语料，1 MB 上限）中位数",
            median(samples),
            Some(15.0),
        ));
        for i in 0..(2000 - n_long) {
            f.store
                .ingest(&raw_from_corpus(i), &p, NOW + 1000 + i as i64)
                .expect("ingest");
        }
        for (name, q) in [("miss", "zq"), ("cjk_1", "的"), ("en_2", "qu")] {
            let samples = (0..runs)
                .map(|_| time(|| f.store.search(q, ClipFilter::All, 50).expect("搜索")).0)
                .collect();
            rows.push(row(
                format!("search/like_{name} 「{q}」（2000 条，其中 {n_long} 条 1 MB）"),
                median(samples),
                Some(30.0),
            ));
        }
    }

    // ---- 打开数据库（预加载时机）----
    let dir = f.dir.path().to_path_buf();
    drop(f.store);
    let (open, reopened) = time(|| ClipStore::open(&dir).expect("打开"));
    rows.push(row(
        "ClipStore::open（1 万条，常规：迁移 + 轻量 FTS 自检）",
        open,
        Some(50.0),
    ));
    drop(reopened);
    std::fs::write(dir.join("clipboard.db.recover"), b"").expect("恢复标记");
    let (open, reopened) = time(|| ClipStore::open(&dir).expect("打开"));
    rows.push(row(
        "ClipStore::open（1 万条，恢复标记：quick_check + integrity-check）",
        open,
        None,
    ));
    drop(reopened);

    // ---- 批量删除（期间持有存储锁）：1 万条 ----
    if !smoke {
        bulk_delete_rows(&mut rows, &dir, 1000.0, 2500.0);
    }
    drop(f.dir);

    // ---- 批量删除：1000 条（默认上限）----
    if !smoke {
        let f = corpus_fixture(1000);
        let dir = f.dir.path().to_path_buf();
        drop(f.store);
        bulk_delete_rows(&mut rows, &dir, 100.0, 250.0);
        drop(f.dir);
    }

    // ---- 500 条长文本、命中都在预览之外：每条结果都要读正文生成上下文片段（锁内只读正文前缀）----
    {
        let f = Fixture::new();
        let n_long = if smoke { 20 } else { 500 };
        for i in 0..n_long {
            let body = long_text(2000 + i, 20_000);
            let raw = RawClipboard {
                text: Some(format!("{body} zqtailmark {i}")),
                ..RawClipboard::default()
            };
            f.store.ingest(&raw, &p, NOW + i as i64).expect("ingest");
        }
        for limit in [50u32, 200, 500] {
            let samples = (0..runs)
                .map(|_| time(|| f.store.search("zqtailmark", ClipFilter::All, limit).expect("搜索")).0)
                .collect();
            rows.push(row(
                format!("search/snippet_long 「zqtailmark」（{n_long} 条 2 万字节长文本，limit {limit}）"),
                median(samples),
                Some(30.0),
            ));
        }
    }

    // ---- 4K 截图 ingest < 150 ms（缩略图在后台）----
    let f = Fixture::new();
    let imgs = screenshots(if smoke { 1 } else { 3 });
    let png_mb = imgs[0].image_png.as_ref().map_or(0, Vec::len) as f64 / 1_048_576.0;
    let mut samples = Vec::new();
    let mut thumb_samples = Vec::new();
    for i in 0..(if smoke { 1 } else { 9 }) {
        let raw = &imgs[i % imgs.len()];
        let (d, o) = time(|| f.store.ingest(raw, &p, NOW + i as i64).expect("ingest"));
        samples.push(d);
        if let IngestOutcome::Inserted(m) = o {
            thumb_samples.push(time(|| f.store.make_thumb(m.id).expect("缩略图")).0);
            f.store.delete(m.id).expect("删除");
        }
    }
    rows.push(row(
        format!("ingest/image_4k（3840×2160，PNG {png_mb:.1} MB）"),
        median(samples),
        Some(150.0),
    ));
    rows.push(row("make_thumb/4k（后台线程，长边 256）", median(thumb_samples), None));
    drop(f);

    // ---- 体积：1000 条文本 + 100 张图 < 20 MB ----
    let f = Fixture::new();
    for i in 0..1000u64 {
        f.store.ingest(&raw_from_corpus(i), &p, NOW + i as i64).expect("ingest");
    }
    for i in 0..100u32 {
        let raw = image_raw(png(64, 48, false, move |x, y| [i as u8, x as u8, y as u8, 255]));
        if let IngestOutcome::Inserted(m) = f.store.ingest(&raw, &p, NOW + 5000 + i64::from(i)).expect("ingest") {
            f.store.make_thumb(m.id).expect("缩略图");
        }
    }
    f.side_conn()
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .expect("检查点");
    let bytes: u64 = ["clipboard.db", "clipboard.db-wal"]
        .iter()
        .filter_map(|n| std::fs::metadata(f.dir.path().join(n)).ok())
        .map(|m| m.len())
        .sum();
    let mb = bytes as f64 / 1_048_576.0;
    rows.push(Row {
        name: "clipboard.db 体积（1000 条文本 + 100 张图，图片另存 blobs/）".into(),
        value: format!("{mb:.2} MB"),
        budget: "< 20 MB".into(),
        ok: mb < 20.0,
    });
    drop(f);

    println!(
        "\n== wisp-clipboard 预算检查{} ==",
        if smoke {
            "（冒烟：测试模式 / debug 构建，只确认能跑通，不判定预算）"
        } else {
            ""
        }
    );
    let width = rows.iter().map(|r| r.name.chars().count()).max().unwrap_or(0);
    for r in &rows {
        let pad = " ".repeat(width.saturating_sub(r.name.chars().count()));
        println!(
            "{} {}{pad}  {:>12}  {:>10}",
            if smoke {
                "·"
            } else if r.ok {
                "✓"
            } else {
                "✗"
            },
            r.name,
            r.value,
            r.budget
        );
    }
    println!();
    let failed: Vec<&str> = rows.iter().filter(|r| !r.ok).map(|r| r.name.as_str()).collect();
    assert!(smoke || failed.is_empty(), "超出预算：{failed:?}");
}

fn criterion_benches(c: &mut Criterion) {
    let f = search_fixture();
    let p = policy();

    let mut g = c.benchmark_group("search_10k");
    for (name, q) in queries() {
        g.bench_function(name, |b| {
            b.iter(|| f.store.search(black_box(&q), ClipFilter::All, 50).expect("搜索"))
        });
    }
    g.finish();

    let cache = RecentCache::load(&f.store).expect("加载缓存");
    c.bench_function("cache/page_200", |b| {
        b.iter(|| black_box(cache.page(ClipFilter::All, 200)))
    });
    c.bench_function("store/recent_200", |b| {
        b.iter(|| f.store.recent(0, 200, ClipFilter::All).expect("列表"))
    });

    let mut next = SEARCH_ROWS * 10;
    c.bench_function("ingest/text_10k_db", |b| {
        b.iter_batched(
            || {
                next += 1;
                (raw_from_corpus(next), next)
            },
            |(raw, i)| f.store.ingest(&raw, &p, NOW + i as i64).expect("ingest"),
            BatchSize::SmallInput,
        )
    });
    drop(f);

    let f = Fixture::new();
    let imgs = screenshots(2);
    let mut g = c.benchmark_group("image_4k");
    g.sample_size(10);
    g.bench_function("ingest", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for i in 0..iters {
                let raw = &imgs[i as usize % imgs.len()];
                let (d, o) = time(|| f.store.ingest(raw, &p, NOW + i as i64).expect("ingest"));
                total += d;
                if let IngestOutcome::Inserted(m) = o {
                    f.store.delete(m.id).expect("删除");
                }
            }
            total
        })
    });
    g.bench_function("make_thumb", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for i in 0..iters {
                let raw = &imgs[i as usize % imgs.len()];
                if let IngestOutcome::Inserted(m) = f.store.ingest(raw, &p, NOW).expect("ingest") {
                    total += time(|| f.store.make_thumb(m.id).expect("缩略图")).0;
                    f.store.delete(m.id).expect("删除");
                }
            }
            total
        })
    });
    g.finish();
}

criterion_group!(benches, criterion_benches);

fn main() {
    // cargo bench 会传 `--bench`；没有时是 `cargo test --benches` 的测试模式，只做冒烟
    let bench_mode = std::env::args().any(|a| a == "--bench");
    budget_checks(!bench_mode);
    if bench_mode {
        benches();
        Criterion::default().configure_from_args().final_summary();
    }
}
