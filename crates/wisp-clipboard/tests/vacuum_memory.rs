//! VACUUM 期间的内存（常驻进程要轻量）：临时库放在文件里，整理一个几十 MB 的库时，
//! 进程的峰值内存不随库的大小上涨（临时库放在内存里时约等于库的大小）。
//!
//! 单独一个测试二进制、只有一个测试（独占进程）：用 `getrusage` 的 `ru_maxrss`（进程的峰值 RSS）前后对比。
//! 只在 64 位 Unix 上编译（`struct rusage` 的布局按 64 位 macOS / Linux 声明）。
#![cfg(all(unix, target_pointer_width = "64"))]

mod common;

use common::{Fixture, NOW};

/// `struct rusage`：两个 `timeval`（各 16 字节）之后是 14 个 `long`，第一个是 `ru_maxrss`
/// （macOS 的单位是字节，Linux 是 KB）
#[repr(C)]
struct RUsage {
    times: [i64; 4],
    maxrss: i64,
    rest: [i64; 13],
}

unsafe extern "C" {
    fn getrusage(who: i32, usage: *mut RUsage) -> i32;
}

/// 进程至今的峰值 RSS（字节）
fn peak_rss_bytes() -> u64 {
    let mut u = RUsage {
        times: [0; 4],
        maxrss: 0,
        rest: [0; 13],
    };
    // SAFETY: RUsage 的大小与布局和 64 位 macOS / Linux 的 struct rusage 一致（2 个 timeval + 14 个 long，共 144 字节）；
    // who = 0 即 RUSAGE_SELF；指针指向一个有效、可写的局部变量
    let rc = unsafe { getrusage(0, &raw mut u) };
    assert_eq!(rc, 0, "getrusage 失败");
    let v = u64::try_from(u.maxrss).unwrap_or(0);
    if cfg!(target_os = "macos") { v } else { v * 1024 }
}

#[test]
fn vacuum_does_not_hold_the_database_in_memory() {
    const MB: u64 = 1 << 20;
    let f = Fixture::new();
    {
        // 48 条约 1.5 MB、几乎不可压缩的文本（库约 72 MB），再删掉一半，留出足够的空闲页让 vacuum_if_due 执行
        let c = f.side_conn();
        c.execute_batch("PRAGMA cache_size = -2048;").expect("配置");
        c.execute(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 48)
             INSERT INTO items(kind, created_at, last_used_at, hash, text_preview, text)
             SELECT 'text', i, i, randomblob(32), 'x', hex(randomblob(768 * 1024)) FROM n",
            [],
        )
        .expect("写入");
        c.execute("DELETE FROM items WHERE id % 2 = 0", []).expect("删除");
        c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .expect("检查点");
    }
    let before_bytes = std::fs::metadata(f.db_path()).expect("库").len();
    assert!(before_bytes > 64 * MB, "库应足够大：{} MB", before_bytes / MB);

    let before = peak_rss_bytes();
    assert!(
        f.store.vacuum_if_due(NOW, NOW - 31 * 86_400_000).expect("VACUUM"),
        "应满足 VACUUM 条件"
    );
    let after = peak_rss_bytes();
    let remaining = std::fs::metadata(f.db_path()).expect("库").len();
    assert!(remaining > 32 * MB && remaining < before_bytes, "{remaining}");
    assert!(
        after.saturating_sub(before) < 16 * MB,
        "VACUUM 期间峰值内存上涨了 {} MB（整理后的库 {} MB）",
        after.saturating_sub(before) / MB,
        remaining / MB
    );
}
