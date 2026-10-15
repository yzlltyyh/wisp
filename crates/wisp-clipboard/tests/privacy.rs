//! 隐私规则：平台标记、排除名单、疑似密钥；命中时库中与 blobs 中都没有任何痕迹。

mod common;

use common::{Fixture, NOW, app, expect_meta, image_raw, png, policy, raw_text};
use wisp_clipboard::{BlobKind, IngestOutcome, OcrHistoryNew, RetentionPolicy, SkipReason};
use wisp_types::{ClipFilter, ExcludedApp, OcrMode, OsKind, RawClipboard};

/// 库里没有任何记录（items、FTS 索引、blobs）
fn assert_no_trace(f: &Fixture) {
    assert_eq!(f.count("SELECT count(*) FROM items"), 0);
    assert_eq!(f.count("SELECT count(*) FROM items_fts_docsize"), 0);
    for k in BlobKind::ALL {
        assert!(f.blob_files(k).is_empty(), "blobs/{} 不应有文件", k.dir());
    }
}

#[test]
fn concealed_content_is_never_recorded() {
    let f = Fixture::new();
    let raw = RawClipboard {
        concealed: true,
        source: Some(app("com.example.unknown", "Unknown")),
        ..RawClipboard::default()
    };
    assert_eq!(f.ingest(&raw, NOW), IngestOutcome::Skipped(SkipReason::Concealed));
    // 即使平台误把正文也带上了，标记仍然优先
    let mut raw = image_raw(png(8, 8, false, |_, _| [1, 2, 3, 255]));
    raw.concealed = true;
    raw.text = Some("hunter2".into());
    assert_eq!(f.ingest(&raw, NOW), IngestOutcome::Skipped(SkipReason::Concealed));
    assert_no_trace(&f);
}

#[test]
fn password_managers_are_excluded_by_default() {
    let f = Fixture::new();
    for (id, name) in [
        ("com.1password.1password", "1Password"),
        ("com.bitwarden.desktop", "Bitwarden"),
        ("org.keepassxc.keepassxc", "KeePassXC"),
        ("keepass.exe", "KeePass"),
        ("1password.exe", "1Password"),
    ] {
        let mut raw = raw_text("correct horse battery staple");
        raw.source = Some(app(id, name));
        assert_eq!(
            f.ingest(&raw, NOW),
            IngestOutcome::Skipped(SkipReason::ExcludedApp),
            "{id}"
        );
    }
    // X11：WM_CLASS 的 instance 命中也算
    let mut raw = raw_text("secret note");
    let mut a = app("KeePassXC", "KeePassXC");
    a.alt_id = Some("keepassxc".into());
    raw.source = Some(a);
    assert_eq!(f.ingest(&raw, NOW), IngestOutcome::Skipped(SkipReason::ExcludedApp));
    assert_no_trace(&f);
}

#[test]
fn user_added_exclusion_takes_effect() {
    // 把 TextEdit 加入排除名单后不再记录
    let f = Fixture::new();
    let mut raw = raw_text("draft paragraph");
    raw.source = Some(app("com.apple.TextEdit", "TextEdit"));
    let mut p = policy();
    p.privacy.excluded_apps.push(ExcludedApp {
        os: OsKind::Macos,
        app_id: "com.apple.TextEdit".into(),
        name: "TextEdit".into(),
    });
    assert_eq!(
        f.store.ingest(&raw, &p, NOW).expect("ingest"),
        IngestOutcome::Skipped(SkipReason::ExcludedApp)
    );
    assert_no_trace(&f);
    // 移出名单后照常记录
    assert!(matches!(f.ingest(&raw, NOW), IngestOutcome::Inserted(_)));
}

#[test]
fn suspected_secrets_are_skipped() {
    let f = Fixture::new();
    // 48 位 sk- 串
    let key = format!("sk-{}", "Ab3dEf6hIj9lMn2pQr5tUv8x".repeat(2));
    assert_eq!(key.len(), 51);
    for secret in [
        key.as_str(),
        "AKIAIOSFODNN7EXAMPLE",
        "-----BEGIN EC PRIVATE KEY-----\nMHcCAQEEIIrYSSNQFaA2Hwf1duRSxKtLYX5CB04fSeQ6tF1aY/PuoAoGCCqGSM49\n-----END EC PRIVATE KEY-----",
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ3aXNwIiwiaWF0IjoxNzAwMDAwMDAwfQ.Zm9vYmFyYmF6cXV4cXV1eA",
        "  ghp_0123456789abcdefghijABCDEFGHIJ0123456789  ",
    ] {
        assert_eq!(
            f.ingest(&raw_text(secret), NOW),
            IngestOutcome::Skipped(SkipReason::Secret),
            "{secret}"
        );
    }
    assert_no_trace(&f);
}

#[test]
fn secret_detection_can_be_turned_off() {
    let f = Fixture::new();
    let mut p = policy();
    p.privacy.secret_detection = false;
    let raw = raw_text("AKIAIOSFODNN7EXAMPLE");
    assert!(matches!(
        f.store.ingest(&raw, &p, NOW).expect("ingest"),
        IngestOutcome::Inserted(_)
    ));
    // 平台标记与排除名单不可关闭
    let mut raw = raw_text("x");
    raw.concealed = true;
    assert_eq!(
        f.store.ingest(&raw, &p, NOW).expect("ingest"),
        IngestOutcome::Skipped(SkipReason::Concealed)
    );
}

#[test]
fn ordinary_text_with_technical_tokens_is_recorded() {
    let f = Fixture::new();
    for t in [
        "git checkout da39a3ee5e6b4b0d3255bfef95601890afd80709",
        "550e8400-e29b-41d4-a716-446655440000",
        "pip install scikit-learn",
        "The sk-learn library is great",
        "https://example.com/?token=",
    ] {
        assert!(matches!(f.ingest(&raw_text(t), NOW), IngestOutcome::Inserted(_)), "{t}");
    }
    assert_eq!(f.store.recent(0, 10, ClipFilter::All).expect("列表").items.len(), 5);
}

#[test]
fn code_that_mentions_keys_is_recorded() {
    // 赋值右侧是代码里的名字，不是密钥：整段复制的代码不能被当成密钥丢弃
    let f = Fixture::new();
    for t in [
        "const client = new S3Client({ accessKeyId: config.s3AccessKeyId });",
        "let token = session.oauth2Token;",
        "export const API_KEY_HEADER = 'x-api-key-v2';",
        "fetch(url, { headers: { Authorization: `Bearer ${token}` }, apiKey: env.API_KEY_V2 })",
    ] {
        assert!(matches!(f.ingest(&raw_text(t), NOW), IngestOutcome::Inserted(_)), "{t}");
    }
}

/// 库文件与 WAL 中是否还有这段明文
fn on_disk(f: &Fixture, needle: &str) -> bool {
    ["clipboard.db", "clipboard.db-wal"].iter().any(|n| {
        std::fs::read(f.dir.path().join(n)).is_ok_and(|b| b.windows(needle.len()).any(|w| w == needle.as_bytes()))
    })
}

fn checkpoint(f: &Fixture) {
    f.side_conn()
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .expect("检查点");
}

/// 短文本、富文本（HTML）、超过一页的长文本（溢出页）各写一批，返回（标记，id）
fn write_marked(f: &Fixture, tag: &str, n: i64, t0: i64) -> Vec<(String, i64)> {
    let pad = "lorem ipsum dolor sit amet ".repeat(300);
    (0..n)
        .map(|i| {
            let mark = format!("WISPMARK-{tag}-{i:04}");
            let raw = match i % 3 {
                0 => raw_text(&format!("{mark} short entry")),
                1 => RawClipboard {
                    text: Some(format!("rich {mark}")),
                    html: Some(format!("<p><b>rich</b> {mark}</p>")),
                    ..RawClipboard::default()
                },
                _ => raw_text(&format!("{pad}{mark}{pad}")),
            };
            (mark, expect_meta(f.ingest(&raw, t0 + i)).id)
        })
        .collect()
}

#[test]
fn deleted_content_leaves_no_plaintext_on_disk() {
    let f = Fixture::new();
    let items = write_marked(&f, "del", 90, NOW);
    checkpoint(&f);
    assert!(
        items.iter().all(|(m, _)| on_disk(&f, m)),
        "检查点之后标记应在库文件里（检测手段有效）"
    );

    // 单条删除（含长文本）
    for (mark, id) in &items[..6] {
        assert!(f.store.delete(*id).expect("删除"));
        assert!(!on_disk(&f, mark), "删除后仍能在磁盘上找到 {mark}");
    }
    // 保留策略的批量删除：只留最新 30 条
    let report = f
        .store
        .retain(
            &RetentionPolicy {
                max_items: 30,
                max_age_days: None,
            },
            NOW + 1000,
        )
        .expect("清理");
    assert_eq!(report.removed_items, 54);
    checkpoint(&f);
    for (mark, _) in &items[6..60] {
        assert!(!on_disk(&f, mark), "保留策略删除后仍能找到 {mark}");
    }
    for (mark, _) in &items[60..] {
        assert!(on_disk(&f, mark), "保留的 {mark} 应仍在");
    }
}

#[test]
fn clear_leaves_no_plaintext_on_disk() {
    let f = Fixture::new();
    let items = write_marked(&f, "clr", 60, NOW);
    let (kept_mark, kept_id) = items[59].clone();
    f.store.set_pinned(kept_id, true).expect("置顶");
    checkpoint(&f);
    assert!(items.iter().all(|(m, _)| on_disk(&f, m)));

    assert_eq!(f.store.clear(true).expect("清空"), 59);
    for (mark, _) in &items[..59] {
        assert!(!on_disk(&f, mark), "清空后仍能在磁盘上找到 {mark}");
    }
    checkpoint(&f);
    assert!(on_disk(&f, &kept_mark), "保留的置顶条目仍在");
    f.store.clear(false).expect("全部清空");
    assert!(!on_disk(&f, &kept_mark));
}

#[test]
fn ocr_history_clear_leaves_no_plaintext_on_disk() {
    let f = Fixture::new();
    let pad = "recognized text line ".repeat(400);
    for i in 0..20 {
        f.store
            .ocr_history_add(&OcrHistoryNew {
                created_at: NOW + i,
                mode: OcrMode::Markdown,
                model: "test-model",
                text: &format!("{pad}WISPMARK-ocr-{i:04}"),
                thumb: None,
                image: None,
            })
            .expect("写入");
    }
    checkpoint(&f);
    assert!(on_disk(&f, "WISPMARK-ocr-0007"));
    assert_eq!(f.store.ocr_history_retain(10).expect("裁剪"), 10);
    checkpoint(&f);
    assert!(!on_disk(&f, "WISPMARK-ocr-0007"));
    assert!(on_disk(&f, "WISPMARK-ocr-0015"));
    f.store.ocr_history_clear().expect("清空");
    assert!(!on_disk(&f, "WISPMARK-ocr-0015"));
}
