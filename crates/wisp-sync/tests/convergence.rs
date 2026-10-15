//! 收敛：3 台设备随机交错地改动、同步、离线、重启、遭遇失败的写入（含响应丢失）、中途加入与断开重连，
//! 最后都在线同步到稳定时，本机数据必须完全一致。种子固定，失败可复现。
//!
//! 唯一的例外是设计如此：中途加入的设备保留加入前本机独有、早于当时已有清空的条目（加入不删除本机数据，
//! 也不发布它们）。比较前把这类条目从它的数据里去掉。

mod sim;

use rusqlite::OptionalExtension;
use serde_json::json;
use sim::{Device, Rng, fault_once, settle, tick};
use wisp_sync::model::keys;
use wisp_sync::remote::memory::{Fault, Op};
use wisp_sync::{Domain, Hlc, MemoryRemote, RemoteError, SyncError};

const SCALARS: [&str; 2] = ["/general/theme", "/translate/primary_lang"];
const TEXTS: [&str; 8] = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta"];

fn random_local_op(rng: &mut Rng, d: &Device, trace: &mut Vec<String>) {
    let store = &d.store;
    let roll = rng.below(100);
    let before = store.snapshot();
    match roll {
        0..=11 => store.set(Domain::Cfg, rng.pick(&SCALARS), json!(format!("v{}", rng.below(4)))),
        12..=19 => store.set(
            Domain::Cfg,
            &format!("/providers/p{}", rng.below(3)),
            json!({"base_url": format!("https://h{}", rng.below(5))}),
        ),
        20..=23 => store.remove(Domain::Cfg, &format!("/providers/p{}", rng.below(3))),
        24..=30 => store.set(
            Domain::Glo,
            &format!("term{}", rng.below(4)),
            json!({"target": format!("t{}", rng.below(5))}),
        ),
        31..=34 => store.remove(Domain::Glo, &format!("term{}", rng.below(4))),
        35..=39 => store.set_key(&format!("p{}", rng.below(3)), &format!("sk-{}", rng.below(9))),
        40..=41 => store.remove(Domain::Key, &format!("p{}", rng.below(3))),
        42..=59 => {
            store.copy(rng.pick(&TEXTS));
        }
        60..=69 => {
            let items: Vec<String> = store.state.lock().clip.keys().cloned().collect();
            if !items.is_empty() {
                store.pin(rng.pick(&items), rng.chance(60));
            }
        }
        70..=77 => {
            let items: Vec<String> = store.state.lock().clip.keys().cloned().collect();
            if !items.is_empty() {
                store.delete(rng.pick(&items));
            }
        }
        78..=80 => store.clear(rng.chance(50)),
        81..=88 => {
            store.ocr_add(rng.pick(&TEXTS));
        }
        89..=92 => {
            let items: Vec<String> = store.state.lock().ocr.keys().cloned().collect();
            if !items.is_empty() {
                store.ocr_delete(rng.pick(&items));
            }
        }
        93 => store.ocr_clear(),
        _ => store.set(Domain::Sty, "/order", json!([format!("s{}", rng.below(3))])),
    }
    let after = store.snapshot();
    let pins = |s: &sim::State| {
        s.clip
            .iter()
            .map(|(h, i)| format!("{}{}", &h[..4], if i.pinned { "*" } else { "" }))
            .collect::<Vec<_>>()
            .join(",")
    };
    trace.push(format!(
        "{} local#{roll}: [{}] -> [{}]",
        d.label,
        pins(&before),
        pins(&after)
    ));
}

/// 一轮同步；离线或注入的失败允许报错，其他错误说明引擎有问题
async fn try_sync(d: &mut Device, flaky: bool) {
    match d.sync().await {
        Ok(_) => {}
        Err(SyncError::Remote(_)) if flaky || d.remote.ctl.offline.load(std::sync::atomic::Ordering::SeqCst) => {}
        Err(e) => panic!("{} 同步失败：{e:?}", d.label),
    }
}

async fn run(seed: u64, steps: usize) {
    let mut rng = Rng::new(seed);
    let mut trace: Vec<String> = Vec::new();
    let server = MemoryRemote::new();
    let mut devices = vec![
        Device::new(&server, "a", 0xa),
        Device::new(&server, "b", 0xb),
        Device::new(&server, "c", 0xc),
    ];
    // c 先在本机攒一些数据，中途才加入
    for _ in 0..4 {
        random_local_op(&mut rng, &devices[2], &mut trace);
        tick();
    }
    devices[0].connect().await.expect("测试步骤");
    devices[1].connect().await.expect("测试步骤");
    let mut c_joined = false;
    let mut flaky = [false; 3];

    for step in 0..steps {
        let i = usize::try_from(rng.below(3)).expect("测试步骤");
        if i == 2 && !c_joined {
            if rng.chance(15) || step > steps / 2 {
                devices[2].connect().await.expect("测试步骤");
                trace.push("c connect".into());
                c_joined = true;
            } else {
                random_local_op(&mut rng, &devices[2], &mut trace);
            }
            continue;
        }
        let roll = rng.below(100);
        let d = &mut devices[i];
        match roll {
            0..=54 => random_local_op(&mut rng, d, &mut trace),
            55..=84 => {
                try_sync(d, flaky[i]).await;
                trace.push(format!("{} sync", d.label));
            }
            85..=89 => {
                let offline = !d.remote.ctl.offline.load(std::sync::atomic::Ordering::SeqCst);
                d.remote.set_offline(offline);
                trace.push(format!("{} offline={offline}", d.label));
            }
            90..=94 => {
                // 下一次写入失败：一半是「已生效但响应丢失」
                let fault = if rng.chance(50) {
                    Fault::After(RemoteError::Timeout)
                } else {
                    Fault::Before(RemoteError::Status(503))
                };
                let target = if rng.chance(50) { ".head" } else { ".seg" };
                d.remote.set_hook(Some(fault_once(
                    move |op, p| op == Op::Put && p.ends_with(target),
                    fault,
                )));
                flaky[i] = true;
                try_sync(d, true).await;
                d.remote.set_hook(None);
                trace.push(format!("{} flaky sync ({target})", d.label));
            }
            95..=97 => {
                d.restart();
                trace.push(format!("{} restart", d.label));
            }
            _ => {
                if d.remote.ctl.offline.load(std::sync::atomic::Ordering::SeqCst) {
                    continue;
                }
                // 断开后重连（丢失本机同步状态，保留 device_id 与本机数据）
                d.disconnect();
                d.connect().await.expect("测试步骤");
                trace.push(format!("{} reconnect", d.label));
            }
        }
        if rng.chance(20) {
            tick();
        }
    }

    if !c_joined {
        devices[2].connect().await.expect("测试步骤");
    }
    for d in &mut devices {
        d.remote.set_offline(false);
        d.remote.set_hook(None);
    }
    let [a, b, c] = &mut devices[..] else { unreachable!() };
    settle(&mut [a, b, c]).await;
    let states = vec![
        (devices[0].label.clone(), devices[0].state()),
        (devices[1].label.clone(), devices[1].state()),
        (devices[2].label.clone(), without_join_leftovers(&devices[2])),
    ];
    if let Err(e) = sim::check_states_converged(&states) {
        let dump: Vec<String> = devices.iter().map(dump_clip).collect();
        let full = std::env::var("WISP_SIM_TRACE").is_ok();
        let tail = if full { 0 } else { trace.len().saturating_sub(120) };
        panic!("seed {seed}: {e}\n{}\n{}", dump.join("\n"), trace[tail..].join("\n"));
    }
    // 稳定之后，再一轮空闲检查都只需要 1 个请求
    for d in &mut devices {
        d.remote.clear_ops();
        let report = d.sync().await.expect("测试步骤");
        assert_eq!(
            report.requests,
            1,
            "seed {seed}: {} 空闲检查不是 1 个请求：{:?}",
            d.label,
            d.remote.ops()
        );
    }
}

/// 设备的本机数据，去掉加入时留在本机、没有发布的条目：副本里没有它的内容寄存器，
/// 时间也不晚于副本里的清空寄存器（更晚的清空本会按墙钟删除它）
fn without_join_leftovers(d: &Device) -> sim::State {
    let conn = rusqlite::Connection::open(d.db_path()).expect("打开副本");
    let clear_t = |dom: &str, k: &str| -> Option<i64> {
        conn.query_row(
            "SELECT t FROM records WHERE d = ?1 AND k = ?2 AND tomb = 0",
            [dom, k],
            |r| r.get(0),
        )
        .optional()
        .expect("查询")
    };
    let registered = |dom: &str, k: &str| -> bool {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM records WHERE d = ?1 AND k = ?2)",
            [dom, k],
            |r| r.get::<_, i64>(0),
        )
        .expect("查询")
            != 0
    };
    let clip_clear = clear_t("clip", keys::CLIP_CLEAR_ALL).max(clear_t("clip", keys::CLIP_CLEAR_UNPINNED));
    let ocr_clear = clear_t("ocr", keys::OCR_CLEAR);
    let hidden = |ms: i64, clear: Option<i64>| clear.is_some_and(|t| Hlc::from_wall_ms_i64(ms).to_i64() <= t);
    let mut s = d.state();
    s.clip
        .retain(|h, i| registered("clip", &format!("c/{h}")) || !hidden(i.last_used_at, clip_clear));
    s.ocr
        .retain(|id, o| registered("ocr", &format!("o/{id}")) || !hidden(o.created_at, ocr_clear));
    s
}

/// 失败时的诊断：各设备副本里剪贴板域的行
fn dump_clip(d: &Device) -> String {
    let conn = rusqlite::Connection::open(d.db_path()).expect("打开副本");
    let mut stmt = conn
        .prepare("SELECT k, dev, t, tomb, v, applied_t, gone, pending FROM records WHERE d = 'clip' ORDER BY k")
        .expect("查询");
    let rows = stmt
        .query_map([], |r| {
            Ok(format!(
                "  {} dev={} t={} tomb={} v={:?} applied={:?} gone={} pending={}",
                r.get::<_, String>(0)?.chars().take(8).collect::<String>(),
                &r.get::<_, String>(1)?[12..],
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<String>>(4)?
                    .map(|v| v.chars().take(40).collect::<String>()),
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, i64>(7)?,
            ))
        })
        .expect("查询")
        .collect::<Result<Vec<_>, _>>()
        .expect("读取");
    format!("[{} id={}]\n{}", d.label, &d.device_id[12..], rows.join("\n"))
}

#[tokio::test]
async fn random_interleavings_converge() {
    // 默认跑 12 个种子；WISP_SIM_SEEDS=N 可以在本机加大到 N 个，WISP_SIM_SEED 只跑指定的一个
    let steps = std::env::var("WISP_SIM_STEPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(160usize);
    if let Some(seed) = std::env::var("WISP_SIM_SEED").ok().and_then(|s| s.parse().ok()) {
        run(seed, steps).await;
        return;
    }
    let seeds = std::env::var("WISP_SIM_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(12u64);
    for seed in 1..=seeds {
        run(seed, steps).await;
    }
}

#[tokio::test]
async fn long_random_run_converges() {
    run(0xC0FFEE, 600).await;
}
