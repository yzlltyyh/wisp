//! 搜索：中英文 1、2、≥3 字查询都能命中；MATCH / LIKE 转义；过滤器组合；置顶在前；
//! UTF-16 高亮；FTS 与 items 的一致性（insert / update 改 text / delete）。

mod common;

use common::{Fixture, NOW, expect_inserted, image_raw, png};
use wisp_types::{ClipFilter, ClipKind, ClipPage, HighlightRange};

/// 按 UTF-16 区间切片（模拟 JS 的 `String.prototype.slice`）
fn js_slice(s: &str, r: HighlightRange) -> String {
    let units: Vec<u16> = s.encode_utf16().collect();
    String::from_utf16(&units[r.start as usize..r.end as usize]).expect("合法 UTF-16")
}

fn previews(p: &ClipPage) -> Vec<String> {
    p.items.iter().map(|h| h.item.preview.clone()).collect()
}

fn search(f: &Fixture, q: &str) -> ClipPage {
    f.store.search(q, ClipFilter::All, 50).expect("搜索")
}

fn corpus() -> Fixture {
    let f = Fixture::new();
    for (i, t) in [
        "今天天气很好，我们去公园散步",
        "剪贴板历史记录",
        "The quick brown fox jumps over the lazy dog",
        "Rust 与 TypeScript 混合项目",
        "价格上涨了 50% 左右",
        "snake_case 与 c_d 命名",
        "他说：\"a\"b\" 是一个引号测试",
        "AND OR NEAR 不是运算符",
        "星号 * 也按字面匹配",
        "emoji 😀 与中文混排：表情😀符号",
    ]
    .iter()
    .enumerate()
    {
        f.text(t, NOW + i as i64);
    }
    f
}

#[test]
fn chinese_queries_of_one_two_three_chars() {
    let f = corpus();
    // 1 字：LIKE
    let p = search(&f, "园");
    assert_eq!(previews(&p), ["今天天气很好，我们去公园散步"]);
    // 2 字：LIKE
    let p = search(&f, "历史");
    assert_eq!(previews(&p), ["剪贴板历史记录"]);
    // 3 字：FTS trigram
    let p = search(&f, "剪贴板");
    assert_eq!(previews(&p), ["剪贴板历史记录"]);
    // 更长
    let p = search(&f, "去公园散步");
    assert_eq!(p.items.len(), 1);
    // 跨中英文
    let p = search(&f, "Rust 与");
    assert_eq!(p.items.len(), 1);
}

#[test]
fn english_queries_are_case_insensitive_substrings() {
    let f = corpus();
    assert_eq!(search(&f, "q").items.len(), 1, "1 字母");
    assert_eq!(search(&f, "OX").items.len(), 1, "2 字母子串（fox）");
    assert_eq!(search(&f, "QUICK").items.len(), 1);
    assert_eq!(search(&f, "uick bro").items.len(), 1, "跨词子串");
    assert!(search(&f, "zebra").items.is_empty());
}

#[test]
fn special_characters_match_literally() {
    let f = corpus();
    for (q, expect) in [
        ("a\"b", "他说：\"a\"b\" 是一个引号测试"),
        ("\"a\"b\"", "他说：\"a\"b\" 是一个引号测试"),
        ("AND", "AND OR NEAR 不是运算符"),
        ("OR NEAR", "AND OR NEAR 不是运算符"),
        ("50%", "价格上涨了 50% 左右"),
        ("%", "价格上涨了 50% 左右"),
        ("c_d", "snake_case 与 c_d 命名"),
        ("_", "snake_case 与 c_d 命名"),
        ("*", "星号 * 也按字面匹配"),
        ("号 *", "星号 * 也按字面匹配"),
    ] {
        let p = search(&f, q);
        assert_eq!(previews(&p), [expect], "查询 {q:?}");
        assert!(!p.items[0].highlights.is_empty(), "查询 {q:?} 应有高亮");
    }
    // 这些都不应报语法错误
    for q in [
        "(",
        ")",
        "-x",
        "NOT",
        "a OR b",
        "\"",
        "\"\"",
        "\\",
        "NEAR(a b)",
        "col:val",
        "^start",
        "a*",
    ] {
        f.store.search(q, ClipFilter::All, 10).expect("不应报错");
    }
    // `_` 与 `%` 不是通配符：「c_d」不会匹配「cxd」
    f.text("cxd 50x", NOW + 100);
    assert_eq!(search(&f, "c_d").items.len(), 1);
    assert_eq!(search(&f, "0%").items.len(), 1);
}

#[test]
fn highlights_are_utf16_offsets_that_slice_correctly() {
    let f = corpus();
    for q in ["😀", "表情😀符", "中文", "emoji"] {
        let p = search(&f, q);
        assert_eq!(p.items.len(), 1, "{q}");
        let hit = &p.items[0];
        assert!(!hit.highlights.is_empty(), "{q}");
        for r in &hit.highlights {
            assert_eq!(js_slice(&hit.item.preview, *r).to_lowercase(), q.to_lowercase(), "{q}");
        }
    }
    // 两处 😀 都高亮
    let p = search(&f, "😀");
    assert_eq!(p.items[0].highlights.len(), 2);
}

#[test]
fn match_beyond_preview_gets_context_snippet() {
    let f = Fixture::new();
    let long = format!(
        "{}\n\nthe secret ingredient is cinnamon",
        "lorem ipsum dolor sit amet ".repeat(20)
    );
    f.text(&long, NOW);
    let p = search(&f, "cinnamon");
    let hit = &p.items[0];
    assert!(hit.item.preview.starts_with('…'), "{}", hit.item.preview);
    assert!(hit.item.preview.chars().count() <= 200);
    assert_eq!(js_slice(&hit.item.preview, hit.highlights[0]), "cinnamon");
    // 列表（非搜索）里的 preview 仍是开头
    let r = f.store.recent(0, 1, ClipFilter::All).expect("列表");
    assert!(r.items[0].item.preview.starts_with("lorem"));
}

#[test]
fn pinned_hits_come_first_then_recent() {
    let f = Fixture::new();
    let a = f.text("match one", NOW);
    let b = f.text("match two", NOW + 1);
    let c = f.text("match three", NOW + 2);
    f.store.set_pinned(a.id, true).expect("置顶");
    let p = search(&f, "match");
    let ids: Vec<i64> = p.items.iter().map(|h| h.item.id).collect();
    assert_eq!(ids, [a.id, c.id, b.id]);
    assert_eq!(p.pinned_count, 1);
    assert_eq!(p.query.as_deref(), Some("match"));
    assert_eq!(p.next_offset, None);
    // 短查询同样排序
    let p = search(&f, "ma");
    assert_eq!(
        p.items.iter().map(|h| h.item.id).collect::<Vec<_>>(),
        [a.id, c.id, b.id]
    );
}

#[test]
fn filters_combine_with_search() {
    let f = Fixture::new();
    f.text("https://example.com/wisp", NOW);
    f.text("#336699", NOW + 1);
    f.text("fn wisp() {\n    run();\n}", NOW + 2);
    f.text("wisp plain text", NOW + 3);
    let rich = wisp_types::RawClipboard {
        text: Some("wisp rich".into()),
        html: Some("<b>wisp</b> rich".into()),
        ..wisp_types::RawClipboard::default()
    };
    f.ingest(&rich, NOW + 4);
    expect_inserted(f.ingest(&image_raw(png(4, 4, false, |_, _| [1, 1, 1, 255])), NOW + 5));

    let kinds = |filter: ClipFilter, q: &str| -> Vec<ClipKind> {
        f.store
            .search(q, filter, 50)
            .expect("搜索")
            .items
            .iter()
            .map(|h| h.item.kind)
            .collect()
    };
    assert_eq!(kinds(ClipFilter::All, "wisp").len(), 4);
    assert_eq!(kinds(ClipFilter::Link, "wisp"), [ClipKind::Link]);
    assert_eq!(kinds(ClipFilter::Code, "wisp"), [ClipKind::Code]);
    assert_eq!(kinds(ClipFilter::Text, "wisp"), [ClipKind::RichText, ClipKind::Text]);
    assert!(kinds(ClipFilter::Image, "wisp").is_empty());
    assert_eq!(kinds(ClipFilter::Color, "3366"), [ClipKind::Color]);
    assert_eq!(kinds(ClipFilter::Color, "36"), [ClipKind::Color]);

    // 列表过滤
    let p = f.store.recent(0, 50, ClipFilter::Image).expect("列表");
    assert_eq!(p.items.len(), 1);
    // 空查询等同于列表
    let p = f.store.search("   ", ClipFilter::All, 50).expect("搜索");
    assert_eq!(p.items.len(), 6);
}

#[test]
fn like_scan_is_limited_to_recent_2000() {
    let f = Fixture::new();
    f.text("zq needle-old", NOW);
    for i in 0..2000 {
        f.text(&format!("filler {i}"), NOW + 1 + i);
    }
    // 2 字查询只看最近 2000 条：最旧的那条落在范围外
    assert!(search(&f, "zq").items.is_empty());
    // 3 字查询走 FTS，不受限制
    assert_eq!(search(&f, "zq ").items.len(), 0, "首尾空白被去掉，仍是 2 字");
    assert_eq!(search(&f, "zq n").items.len(), 1);
}

#[test]
fn fts_stays_consistent_through_insert_update_delete() {
    let f = Fixture::new();
    let a = f.text("original wording here", NOW);
    let b = f.text("another entry", NOW + 1);
    assert_eq!(search(&f, "wording").items.len(), 1);

    // update（改 text）：au 触发器同步索引
    f.side_conn()
        .execute(
            "UPDATE items SET text = 'replaced phrasing', text_preview = 'replaced phrasing' WHERE id = ?1",
            [a.id],
        )
        .expect("更新");
    assert!(search(&f, "wording").items.is_empty());
    assert_eq!(search(&f, "phrasing").items.len(), 1);

    // delete：ad 触发器
    assert!(f.store.delete(b.id).expect("删除"));
    assert!(search(&f, "another").items.is_empty());

    // 与 items 逐行比对
    f.side_conn()
        .execute(
            "INSERT INTO items_fts(items_fts, rank) VALUES ('integrity-check', 1)",
            [],
        )
        .expect("integrity-check");
    assert_eq!(
        f.count("SELECT count(*) FROM items"),
        f.count("SELECT count(*) FROM items_fts_docsize")
    );
}

#[test]
fn files_are_searchable_by_name() {
    let f = Fixture::new();
    let raw = wisp_types::RawClipboard {
        files: Some(vec!["/Users/me/Documents/季度报告.pdf".into(), "/tmp/notes.md".into()]),
        ..wisp_types::RawClipboard::default()
    };
    f.ingest(&raw, NOW);
    let p = search(&f, "季度报告");
    assert_eq!(p.items.len(), 1);
    assert_eq!(p.items[0].item.kind, ClipKind::Files);
    assert_eq!(js_slice(&p.items[0].item.preview, p.items[0].highlights[0]), "季度报告");
    assert_eq!(search(&f, "notes").items.len(), 1);
}

#[test]
fn limit_is_respected() {
    let f = Fixture::new();
    for i in 0..30 {
        f.text(&format!("repeated term {i}"), NOW + i);
    }
    let p = f.store.search("repeated", ClipFilter::All, 10).expect("搜索");
    assert_eq!(p.items.len(), 10);
    assert_eq!(p.items[0].item.preview, "repeated term 29");
    let p = f.store.search("re", ClipFilter::All, 5).expect("搜索");
    assert_eq!(p.items.len(), 5);
}

#[test]
fn unicode_case_folding_is_consistent_across_query_lengths() {
    // 1–2 字走 LIKE、≥ 3 字走 FTS：大小写折叠必须一致，结果不能随查询长度跳变
    let f = Fixture::new();
    for (i, t) in ["Джон Смит", "ÉCOLE normale", "Σίσυφος", "plain ascii"]
        .iter()
        .enumerate()
    {
        f.text(t, NOW + i as i64);
    }
    for (q, expect) in [
        ("дж", "Джон Смит"),
        ("ДЖ", "Джон Смит"),
        ("джо", "Джон Смит"),
        ("ДЖОН", "Джон Смит"),
        ("é", "ÉCOLE normale"),
        ("éc", "ÉCOLE normale"),
        ("éco", "ÉCOLE normale"),
        ("σί", "Σίσυφος"),
        ("ΣΊΣ", "Σίσυφος"),
    ] {
        let p = search(&f, q);
        assert_eq!(previews(&p), [expect], "查询「{q}」");
        let hl = &p.items[0].highlights;
        assert!(!hl.is_empty(), "查询「{q}」应有高亮");
        // 逐字符折叠（str::to_lowercase 会按词尾规则把 Σ 变成 ς）
        let fold = |s: &str| s.chars().flat_map(char::to_lowercase).collect::<String>();
        assert_eq!(
            fold(&js_slice(&p.items[0].item.preview, hl[0])),
            fold(q),
            "查询「{q}」的高亮"
        );
    }
}

#[test]
fn search_covers_the_first_32k_chars_of_long_texts() {
    // FTS 索引、短查询的 LIKE 与上下文片段都只看正文的前 32768 个字符（长文本不拖慢记录与短查询）
    let f = Fixture::new();
    let pad = |n: usize| "lorem ipsum ".repeat(n / 12);
    let text = format!("{}needleinside Ю {}needleoutside Ж tail", pad(30_000), pad(10_000));
    assert!(text.chars().count() > 40_000);
    let m = f.text(&text, NOW);
    for q in ["needleinside", "Ю", "ю"] {
        let p = search(&f, q);
        assert_eq!(p.items.len(), 1, "前缀内的「{q}」应能搜到");
        assert_eq!(p.items[0].item.id, m.id);
        let preview = &p.items[0].item.preview;
        assert!(preview.starts_with('…'), "命中在预览之外，应换成上下文片段：{preview}");
        assert_eq!(
            js_slice(preview, p.items[0].highlights[0]).to_lowercase(),
            q.to_lowercase()
        );
    }
    for q in ["needleoutside", "Ж", "ж"] {
        assert!(search(&f, q).items.is_empty(), "前缀之外的「{q}」不在检索范围内");
    }
    // 详情与粘贴仍是全文
    let d = f.store.get_detail(m.id).expect("详情").expect("存在");
    assert!(d.text.expect("正文").ends_with("needleoutside Ж tail"));
}

#[test]
fn context_snippets_are_limited_to_the_first_64_results() {
    // 命中都在预览之外时，只为前 64 条结果生成上下文片段（约 5 屏），其余保留原 preview；结果条数不受影响
    let f = Fixture::new();
    let pad = "lorem ipsum dolor sit amet ".repeat(12);
    for i in 0..70 {
        f.text(&format!("{i:02} {pad} needle at the end"), NOW + i);
    }
    let p = f.store.search("needle", ClipFilter::All, 100).expect("搜索");
    assert_eq!(p.items.len(), 70);
    for (i, hit) in p.items.iter().enumerate() {
        if i < 64 {
            assert!(hit.item.preview.starts_with('…'), "{i}: {}", hit.item.preview);
            assert_eq!(js_slice(&hit.item.preview, hit.highlights[0]), "needle");
        } else {
            assert!(
                hit.item.preview.starts_with(char::is_numeric),
                "{i}: {}",
                hit.item.preview
            );
            assert!(hit.highlights.is_empty());
        }
    }
}
