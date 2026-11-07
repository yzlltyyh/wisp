//! 两台模拟设备经真实 WebDAV 服务器的端到端同步，默认忽略。运行方式：
//!
//! ```text
//! WISP_SYNC_TEST_URL=https://… WISP_SYNC_TEST_USER=… WISP_SYNC_TEST_PASS=… \
//!   cargo test -p wisp-sync --locked --test live_engine -- --ignored
//! ```
//!
//! 每台设备有自己的 sync.db、内存里的本机存储（与模拟测试相同的 `FakeStore`）和自己的 [`WebDav`] 客户端；
//! 远端包了一层请求记录，用来检查请求数与「什么都不写」。同一场景也在内存服务器上随普通测试运行
//! （`same_scenario_holds_on_the_in_memory_server`），所以这里的失败多半出在传输或服务器的行为上。
//!
//! 隔离：账号里真正的同步根目录（地址之下的 `wisp/`）一概不碰。测试在地址之下新建独占的 `e2e-<随机 id>/`，
//! 引擎以它为服务器地址，同步根目录因此是 `e2e-<id>/wisp/`；结束时（成功、断言失败或超时都一样）删除整个
//! `e2e-<id>/`。[`WebDav`] 总在地址之后接 `wisp/`，够不到 `e2e-<id>/` 这一层，所以这一层的建立、删除与
//! 存在检查直接发 WebDAV 请求。

mod live;
mod sim;

use std::io::Write as _;
use std::time::Duration;

use live::Account;
use serde_json::json;
use sim::{Device, State, check_converged, noop_progress, settle, tick};
use wisp_sync::layout::VAULT_FILE;
use wisp_sync::remote::memory::Op;
use wisp_sync::{
    ConnectOutcome, CycleReason, Domain, ErrorKind, MemoryRemote, Peek, Remote, Replica, SyncError, WebDav, probe,
    reset_remote,
};

/// 整个场景的时限。服务器每个请求要 0.3–0.6 s、GET 还要经一次重定向，这里留足余量；超时同样会清理
const SCENARIO_LIMIT: Duration = Duration::from_secs(15 * 60);

#[tokio::test]
#[ignore = "需要真实 WebDAV 账号"]
async fn two_devices_sync_through_a_real_webdav_server() {
    let account = Account::from_env();
    let devices = account.clone();
    live::with_scratch(&account, SCENARIO_LIMIT, move |server_url| {
        scenario(live_devices(devices, server_url))
    })
    .await;
}

/// 同一场景在内存服务器上跑一遍（不需要账号，随普通测试运行）：保证场景里的预期与引擎的语义一致
#[tokio::test]
async fn same_scenario_holds_on_the_in_memory_server() {
    let server = MemoryRemote::new();
    scenario(move |label, n| Device::with_remote(server.clone(), label, n, State::with_defaults())).await;
}

/// 两台设备的完整场景；`make` 按标签与编号建一台连到同一服务器的新设备。
/// 每一步是一个函数，断言信息以步骤名开头
async fn scenario<R: Remote>(make: impl Fn(&str, u64) -> Device<R>) {
    let mut a = make("a", 0xa);
    let mut b = make("b", 0xb);
    let seed = create_vault_with_local_data(&mut a).await;
    let from_b = join_and_converge(&mut a, &mut b, &seed).await;
    wrong_passphrase_is_rejected(make("c", 0xc)).await;
    deletes_edits_and_clears_propagate(&mut a, &mut b, &seed, &from_b).await;
    idle_cycle_is_a_single_request(&mut a, &mut b).await;
    realtime_checks_are_read_only(&mut a, &mut b).await;
    reset_stops_the_other_device(&mut a, &mut b).await;
}

/// A 创建同步时已有的本机数据（剪贴板与识别历史的 key）
struct Seed {
    /// 置顶的文本
    alpha: String,
    beta: String,
    gamma: String,
    image: String,
    png: Vec<u8>,
    ocr: String,
}

/// 创建：A 在空服务器上带着非默认配置、模型服务与 Key、术语、三条剪贴板文本（一条置顶）、
/// 一张图片和一条识别记录开启同步
async fn create_vault_with_local_data<R: Remote>(a: &mut Device<R>) -> Seed {
    a.store.set(Domain::Cfg, "/general/theme", json!("dark"));
    a.store.set(Domain::Cfg, "/clipboard/max_items", json!(500));
    a.store.set(
        Domain::Cfg,
        "/providers/openai",
        json!({"base_url": "https://api.example.com/v1"}),
    );
    a.store.set(Domain::Cfg, "/providers/@order", json!(["openai"]));
    // 占位的假 Key：只验证 Key 域能端到端传到另一台设备
    a.store.set_key("openai", "sk-e2e-placeholder");
    a.store.set(Domain::Glo, "Wisp", json!({"target": "Wisp"}));
    let alpha = a.store.copy("e2e alpha");
    tick();
    let beta = a.store.copy("e2e beta");
    tick();
    let gamma = a.store.copy("e2e gamma");
    a.store.pin(&alpha, true);
    let png = sample_png();
    let image = a.store.copy_image(&png);
    let ocr = a.store.ocr_add("e2e 识别结果");

    let outcome = step("创建：连接", a.connect().await);
    assert_eq!(outcome, ConnectOutcome::Created, "创建：空服务器上应当新建同步");
    // 连接只完成配置类域；剪贴板、识别历史与图片由随后的首轮上传，与桌面端一样以 Connect 触发
    step("创建：首轮", a.sync_reason(CycleReason::Connect).await);
    let report = step("创建：后续轮次", a.sync_fully().await);
    assert!(!report.pending, "创建：仍有未推送的改动：{report:?}");
    let status = step("创建：状态", a.engine().status());
    assert!(status.unlocked && !status.joining, "创建：{status:?}");
    Seed {
        alpha,
        beta,
        gamma,
        image,
        png,
        ocr,
    }
}

/// 加入：B 先「测试连接」看到已有的同步数据，再用同一口令加入；A、B、A 各同步一轮后两边一致。
/// 返回 B 独有的剪贴板条目
async fn join_and_converge<R: Remote>(a: &mut Device<R>, b: &mut Device<R>, seed: &Seed) -> String {
    // B 自己的数据：A 保持默认的一项配置，以及一条 A 没有的剪贴板文本
    b.store.set(Domain::Cfg, "/translate/primary_lang", json!("ja"));
    let from_b = b.store.copy("e2e from b");

    let local = step("加入：读本机副本", Replica::peek_vault_id(&b.db_path()));
    let report = step("加入：测试连接", probe(&b.remote, local.as_deref()).await);
    let vault = report.vault.expect("加入：测试连接应当看到服务器上已有的同步数据");
    assert!(vault.supported, "加入：{vault:?}");
    assert_eq!(vault.device_count, 1, "加入：服务器上应当只有 A：{vault:?}");
    assert!(!vault.restorable, "加入：B 从未连过这个同步，不能直接恢复：{vault:?}");

    let outcome = step("加入：连接", b.connect().await);
    assert_eq!(outcome, ConnectOutcome::Joined, "加入：已有同步数据时应当加入");
    step("加入：A 同步", a.sync_fully().await);
    step("加入：B 首轮", b.sync_reason(CycleReason::Connect).await);
    step("加入：B 同步", b.sync_fully().await);
    step("加入：A 再同步", a.sync_fully().await);

    if let Err(e) = check_converged(&[&*a, &*b]) {
        panic!("加入：两台设备没有收敛：{e}");
    }
    let (sa, sb) = (a.state(), b.state());
    // A 的配置、模型服务、Key 与术语到了 B
    assert_eq!(sb.cfg["/general/theme"], json!("dark"), "加入：B 的配置");
    assert_eq!(sb.cfg["/clipboard/max_items"], json!(500), "加入：B 的配置");
    assert_eq!(sb.cfg["/providers/@order"], json!(["openai"]), "加入：B 的模型服务顺序");
    assert!(sb.cfg.contains_key("/providers/openai"), "加入：B 缺少 A 的模型服务");
    assert_eq!(sb.keys["openai"]["key"], json!("sk-e2e-placeholder"), "加入：B 的 Key");
    assert_eq!(sb.glo["Wisp"], json!({"target": "Wisp"}), "加入：B 的术语");
    // 加入规则：服务器上那一项还是 A 的默认值，B 的非默认值胜出并传回 A
    for (label, s) in [("A", &sa), ("B", &sb)] {
        assert_eq!(
            s.cfg["/translate/primary_lang"],
            json!("ja"),
            "加入：{label} 上 B 的非默认值没有保留"
        );
    }
    // 剪贴板：文本、置顶状态、图片内容逐字节一致
    let alpha = sb
        .clip
        .get(&seed.alpha)
        .unwrap_or_else(|| panic!("加入：B 缺少置顶的条目：{}", describe_clip(&sb)));
    assert!(
        alpha.pinned && alpha.text == "e2e alpha",
        "加入：B 上置顶的条目不对：{}",
        describe_clip(&sb)
    );
    for (key, text) in [(&seed.beta, "e2e beta"), (&seed.gamma, "e2e gamma")] {
        assert!(
            sb.clip.get(key).is_some_and(|i| i.text == text && !i.pinned),
            "加入：B 缺少 {text:?}：{}",
            describe_clip(&sb)
        );
    }
    let image = sb.clip.get(&seed.image).and_then(|i| i.image.as_deref());
    assert!(
        image == Some(seed.png.as_slice()),
        "加入：B 上的图片与原图不一致（原图 {} B，B 上 {:?} B）",
        seed.png.len(),
        image.map(<[u8]>::len)
    );
    // 识别记录与缩略图
    let ocr = sb.ocr.get(&seed.ocr).expect("加入：B 缺少识别记录");
    assert_eq!(ocr.text, "e2e 识别结果", "加入：B 上识别记录的正文");
    assert_eq!(ocr.thumb, sa.ocr[&seed.ocr].thumb, "加入：B 上识别记录的缩略图");
    // B 独有的条目到了 A
    assert!(
        sa.clip.get(&from_b).is_some_and(|i| i.text == "e2e from b"),
        "加入：A 缺少 B 的条目：{}",
        describe_clip(&sa)
    );
    for (label, d) in [("A", &*a), ("B", &*b)] {
        let devices = step("加入：设备列表", d.engine().devices());
        assert_eq!(devices.len(), 2, "加入：{label} 看到的设备：{devices:?}");
    }
    from_b
}

/// 错误口令：第三台设备用错误的同步口令连接，报口令错误，且不往服务器写任何东西
async fn wrong_passphrase_is_rejected<R: Remote>(c: Device<R>) {
    let result = c
        .engine()
        .connect(
            &c.remote,
            &c.store,
            "definitely not the passphrase",
            &[],
            &c.options,
            noop_progress(),
        )
        .await;
    let err = match result {
        Ok(_) => panic!("错误口令：不应当连接成功"),
        Err(e) => e,
    };
    assert_eq!(err.kind(), ErrorKind::Passphrase, "错误口令：{err}");
    assert!(
        c.remote.writes().is_empty(),
        "错误口令：不应写入服务器：{:?}",
        c.remote.writes()
    );
    assert!(!c.engine().is_unlocked(), "错误口令：不应解锁");
}

/// 删除与清空：A 删除一条剪贴板、改一项配置，B 同步后两者都反映出来；随后 B 清空剪贴板（保留置顶），
/// 双方同步后 A 上只剩置顶的条目
async fn deletes_edits_and_clears_propagate<R: Remote>(
    a: &mut Device<R>,
    b: &mut Device<R>,
    seed: &Seed,
    from_b: &str,
) {
    tick();
    a.store.delete(&seed.beta);
    a.store.set(Domain::Cfg, "/general/theme", json!("light"));
    step("删除：A 同步", a.sync_fully().await);
    step("删除：B 同步", b.sync_fully().await);
    let sb = b.state();
    assert!(
        !sb.clip.contains_key(&seed.beta),
        "删除：B 上被删的条目还在：{}",
        describe_clip(&sb)
    );
    assert_eq!(sb.cfg["/general/theme"], json!("light"), "删除：B 上的配置没有更新");

    // 清空的时间要晚于所有条目的最近使用时间，它们才都算作「清空之前」的条目
    tick();
    b.store.clear(true);
    step("清空：B 同步", b.sync_fully().await);
    step("清空：A 同步", a.sync_fully().await);
    for (label, s) in [("A", a.state()), ("B", b.state())] {
        let left: Vec<&String> = s.clip.keys().collect();
        assert_eq!(
            left,
            vec![&seed.alpha],
            "清空：{label} 上应当只剩置顶的条目：{}",
            describe_clip(&s)
        );
        assert!(s.clip[&seed.alpha].pinned, "清空：{label} 上的置顶状态丢了");
        assert!(!s.clip.contains_key(from_b) && !s.clip.contains_key(&seed.image));
        // 清空剪贴板不影响识别历史
        assert!(s.ocr.contains_key(&seed.ocr), "清空：{label} 上的识别记录不该被清掉");
    }
}

/// 空闲：两边都没有改动、也都读过对方最新的数据后，A 的一轮检查只发 1 个请求（列同步根目录）
async fn idle_cycle_is_a_single_request<R: Remote>(a: &mut Device<R>, b: &mut Device<R>) {
    settle(&mut [&mut *a, &mut *b]).await;
    if let Err(e) = check_converged(&[&*a, &*b]) {
        panic!("空闲：稳定后两台设备不一致：{e}");
    }
    a.remote.clear_ops();
    let report = step("空闲：A 同步", a.sync().await);
    let ops = a.remote.ops();
    assert_eq!(
        report.requests, 1,
        "空闲：引擎报告的请求数：{report:?}；实际请求：{ops:?}"
    );
    assert!(
        ops.len() == 1 && ops[0].0 == Op::List,
        "空闲：实际请求应当只有一次列目录：{ops:?}"
    );
    assert!(!report.pending && !report.again, "空闲：{report:?}");
}

/// 实时同步的只读探测：没有变化时只列一次目录、不写 sync.db；B 推送后 A 看得出变化，接着的一轮复用这次的列表；
/// A 自己的推送（服务器回不回 ETag、PUT 与列目录的写法是否一致都一样）不被当成别人的改动
async fn realtime_checks_are_read_only<R: Remote>(a: &mut Device<R>, b: &mut Device<R>) {
    // 跨 await 持有的连接要能在线程间共享（场景在多线程运行时里跑），用互斥锁包一层
    let db = std::sync::Mutex::new(rusqlite::Connection::open(a.db_path()).expect("实时：第二个连接"));
    let version = || -> i64 {
        db.lock()
            .expect("实时：连接")
            .query_row("PRAGMA data_version", [], |r| r.get(0))
            .expect("实时：data_version")
    };
    let before = version();
    a.remote.clear_ops();
    let p = peek_of(a).await;
    assert!(matches!(p, Peek::Unchanged), "实时：没有改动时应当没有变化：{p:?}");
    let ops = a.remote.ops();
    assert!(ops.len() == 1 && ops[0].0 == Op::List, "实时：只列一次目录：{ops:?}");
    assert_eq!(version(), before, "实时：探测不写 sync.db");

    let text = b.store.copy("e2e realtime from b");
    step("实时：B 推送", b.sync().await);
    let Peek::Changed(listing) = peek_of(a).await else {
        panic!("实时：B 推送之后 A 应当看到变化");
    };
    a.remote.clear_ops();
    let report = step(
        "实时：A 复用列表同步",
        a.engine()
            .sync_cycle_with(
                &a.remote,
                &a.store,
                CycleReason::Poll,
                &a.options,
                noop_progress(),
                Some(listing),
            )
            .await,
    );
    let ops = a.remote.ops();
    assert!(ops.iter().all(|(op, _)| *op != Op::List), "实时：不应再列目录：{ops:?}");
    assert!(
        report
            .remote_clips
            .iter()
            .any(|c| sim::text_hash("e2e realtime from b") == c.hash),
        "实时：报告里应当有 B 的新复制：{report:?}"
    );
    assert!(a.state().clip.contains_key(&text), "实时：A 缺少 B 的新复制");

    a.store.copy("e2e realtime from a");
    step("实时：A 推送", a.sync().await);
    for i in 0..2 {
        let p = peek_of(a).await;
        assert!(
            matches!(p, Peek::Unchanged),
            "实时：A 自己的推送不算变化（第 {i} 次）：{p:?}"
        );
    }
    let Peek::Changed(listing) = peek_of(b).await else {
        panic!("实时：A 推送之后 B 应当看到变化");
    };
    step(
        "实时：B 复用列表同步",
        b.engine()
            .sync_cycle_with(
                &b.remote,
                &b.store,
                CycleReason::Poll,
                &b.options,
                noop_progress(),
                Some(listing),
            )
            .await,
    );
    let p = peek_of(b).await;
    assert!(matches!(p, Peek::Unchanged), "实时：同步之后 B 回到没有变化：{p:?}");
}

async fn peek_of<R: Remote>(d: &Device<R>) -> Peek {
    step("实时：探测", d.engine().peek(&d.remote).await)
}

/// 清除：A 清除服务器上的同步数据；B 的下一轮报「同步数据已被清除」，不写服务器，也不动本机数据
async fn reset_stops_the_other_device<R: Remote>(a: &mut Device<R>, b: &mut Device<R>) {
    step("清除：A 清除服务器数据", reset_remote(&a.remote).await);
    // reset_remote 自己会等 vault.json 从列目录里消失（有上限、超时不报错）；这里再确认一次，
    // 免得把服务器的删除延迟误当成引擎的问题
    let mut vault_gone = false;
    for _ in 0..20 {
        let listed = step("清除：列目录", a.remote.inner.list("").await.map_err(SyncError::from));
        if !listed.is_some_and(|entries| entries.iter().any(|e| !e.is_dir && e.name == VAULT_FILE)) {
            vault_gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(vault_gone, "清除：约 10 秒后服务器上仍列出 vault.json");

    b.store.set(Domain::Glo, "after-reset", json!({"target": "b"}));
    let before = b.state();
    // 连续两轮：清除的结论是持续的，第二轮同样不写
    for round in 1..=2 {
        b.remote.clear_ops();
        let err = match b.sync().await {
            Ok(report) => panic!("清除：B 的第 {round} 轮不应成功：{report:?}"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), ErrorKind::VaultReset, "清除：B 的第 {round} 轮：{err}");
        assert!(
            b.remote.writes().is_empty(),
            "清除：B 的第 {round} 轮不应写入服务器：{:?}",
            b.remote.writes()
        );
    }
    assert!(b.state() == before, "清除：B 的本机数据不应改变");
}

// ---------------------------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------------------------

/// 连到测试目录的设备工厂：每台设备有自己的临时数据目录与自己的 WebDAV 客户端
fn live_devices(account: Account, server_url: String) -> impl Fn(&str, u64) -> Device<WebDav> {
    move |label, n| {
        let dav = WebDav::new(&server_url, &account.user, &account.pass).expect("服务器地址");
        Device::with_remote(dav, label, n, State::with_defaults())
    }
}

/// 引擎错误 → 带步骤名的失败。错误只含固定词汇（种类、HTTP 状态码），不含地址、账号与内容
fn step<T>(what: &str, result: Result<T, SyncError>) -> T {
    result.unwrap_or_else(|e| panic!("{what}：{e}（{}，{:?}）", e.kind().as_str(), e.detail()))
}

/// 剪贴板的简短描述，用于失败信息（图片只给字节数）
fn describe_clip(s: &State) -> String {
    let items: Vec<String> = s
        .clip
        .values()
        .map(|i| {
            let pin = if i.pinned { "（置顶）" } else { "" };
            match &i.image {
                Some(img) => format!("[图片 {} B]{pin}", img.len()),
                None => format!("{:?}{pin}", i.text),
            }
        })
        .collect();
    format!("[{}]", items.join(", "))
}

/// 一张 64×64 的 RGBA PNG（渐变）。按 PNG 格式手工拼出，免得为测试引入图像库；
/// 本机存储不解码图片，只要求内容在两台设备之间逐字节一致
fn sample_png() -> Vec<u8> {
    const SIDE: u32 = 64;
    let mut raw = Vec::with_capacity(((SIDE * 4 + 1) * SIDE) as usize);
    for y in 0..SIDE {
        raw.push(0); // 每行的过滤方式：无
        for x in 0..SIDE {
            raw.extend_from_slice(&[(x * 4) as u8, (y * 4) as u8, ((x ^ y) * 4) as u8, 0xff]);
        }
    }
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(&raw).expect("压缩像素");
    let idat = z.finish().expect("压缩像素");
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&SIDE.to_be_bytes());
    ihdr.extend_from_slice(&SIDE.to_be_bytes());
    // 8 位、RGBA、deflate、标准过滤、不隔行
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    let iend: &[u8] = &[];
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    for (kind, data) in [(b"IHDR", ihdr.as_slice()), (b"IDAT", idat.as_slice()), (b"IEND", iend)] {
        png.extend_from_slice(&u32::try_from(data.len()).expect("块长度").to_be_bytes());
        png.extend_from_slice(kind);
        png.extend_from_slice(data);
        let mut crc = flate2::Crc::new();
        crc.update(kind);
        crc.update(data);
        png.extend_from_slice(&crc.sum().to_be_bytes());
    }
    png
}
