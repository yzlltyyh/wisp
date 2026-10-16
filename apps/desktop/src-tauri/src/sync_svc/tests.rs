//! 同步服务的测试：关闭时不创建任何资源、日志写入线程与退出时的写入、按来源记录配置改动、启动核对、远端配置的应用、
//! 凭据与账号不匹配。都不联网，钥匙串用进程内存后端。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use serde_json::{Value, json};
use wisp_sync::projection::{self, ConfigDoc};
use wisp_sync::{
    DeviceInfo, Domain, Engine, Hlc, Journal, JournalEntry, JournalValue, KeyApply, LocalStore, RemoteChange,
    ShadowUpdate,
};
use wisp_types::{OsKind, Settings, SyncErrorKind, SyncPhase, SyncStatus};

use super::capture::{CaptureKey, Capturer, Captures, Msg};
use super::local::DesktopStore;
use super::reconcile::{self, Startup};
use super::writer::{self, Writer, WriterCtx};
use super::{CycleReason, EXIT_FLUSH, SyncService, errors};
use crate::settings::SettingsService;
use crate::settings::service::tests::{provider, test_service};
use crate::settings::service::{ChangeOrigin, ConfigChange, ConfigFile, LoadState, ReloadOutcome};
use crate::settings::sync_credential::SyncCredential;

const THRESHOLD: &str = "/translate/long_text_threshold";
const MOUSE_LEAVE: &str = "/translate/close_on_mouse_leave";

fn open_engine(dir: &std::path::Path) -> Engine {
    Engine::open(
        &dir.join("sync.db"),
        &dir.join("spool"),
        DeviceInfo {
            id: "0123456789abcdef".into(),
            name: "test".into(),
            os: OsKind::current(),
            app_version: "0.0.0".into(),
        },
    )
    .expect("open replica")
}

fn settings_doc(f: impl FnOnce(&mut Settings)) -> Value {
    let mut s = Settings::default();
    f(&mut s);
    projection::canonical(&s).expect("canonical")
}

fn ctx(journal: &Journal, captures: &Arc<Captures>, has_key: bool, local: &Arc<AtomicUsize>) -> WriterCtx {
    let local = Arc::clone(local);
    WriterCtx {
        journal: journal.clone(),
        captures: Arc::clone(captures),
        os: OsKind::current(),
        has_key: Arc::new(move |_| has_key),
        on_local: Arc::new(move || {
            local.fetch_add(1, Ordering::SeqCst);
        }),
    }
}

/// 同步没开启：启动之后、设置有别的变化时，都不创建 sync.db、不起写入线程、不装钩子
#[tokio::test]
async fn disabled_sync_creates_nothing() {
    let (_dir, settings) = test_service();
    let svc = SyncService::with_parts(Arc::clone(&settings), None, "0.0.0".into());
    svc.start_local();
    settings
        .update(&mut |s| s.translate.close_on_mouse_leave = !s.translate.close_on_mouse_leave)
        .expect("update");
    svc.trigger(CycleReason::Manual);
    svc.trigger(CycleReason::TrayOpened);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(svc.active().is_none(), "没有开启时不持有任何资源");
    assert!(!svc.db_path().exists(), "不创建 sync.db");
    assert!(!svc.sync_dir().exists(), "不创建临时目录");
    assert!(!svc.is_enabled());
    assert!(!svc.running.load(Ordering::SeqCst), "没有运行循环");
    assert_eq!(svc.status(), SyncStatus::off());
    assert_eq!(svc.sync_now().expect_err("未连接").message, "errors.sync.not_connected");
    assert!(svc.devices().await.expect("devices").is_empty());
}

/// 捕获的改动在写入线程里落进副本；Key 只记「需要时再读」，明文不进 sync.db；退出时的 flush 把队列写完
#[test]
fn journal_writer_persists_and_exit_flush_drains() {
    let dir = tempfile::tempdir().expect("tempdir");
    let engine = open_engine(dir.path());
    let journal = engine.journal().expect("journal");
    let captures = Arc::new(Captures::default());
    let local = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel();
    let writer = Writer::spawn(tx.clone(), rx, ctx(&journal, &captures, false, &local)).expect("spawn");
    let capturer = Capturer::new(engine.hlc(), tx, Arc::clone(&captures));
    const SECRET: &str = "sk-plaintext-must-not-be-stored-4242";

    capturer.secret("alpha", Some(SECRET));
    capturer.secret("beta", None);
    assert!(writer.flush(Duration::from_secs(5)));
    assert!(captures.is_empty(), "写入之后不再算「尚未写入」");
    assert!(local.load(Ordering::SeqCst) >= 1, "写入了本机改动就通知去抖");
    let alpha = journal.winner(&Domain::Key, "alpha").expect("read").expect("有记录");
    assert!(alpha.v.is_some());
    let beta = journal.winner(&Domain::Key, "beta").expect("read").expect("有记录");
    assert!(beta.v.is_none(), "删除记为墓碑");

    for i in 0..300 {
        capturer.secret(&format!("p{i}"), Some(SECRET));
    }
    assert!(writer.flush(EXIT_FLUSH), "退出时在期限内写完");
    assert!(journal.winner(&Domain::Key, "p299").expect("read").is_some());
    assert!(captures.is_empty());

    writer.stop();
    assert!(!writer.is_running());
    assert!(writer.flush(Duration::from_millis(10)), "线程退出后 flush 立即返回");
    drop(journal);
    engine.close().expect("close");
    for name in ["sync.db", "sync.db-wal"] {
        if let Ok(bytes) = std::fs::read(dir.path().join(name)) {
            assert!(
                !bytes.windows(SECRET.len()).any(|w| w == SECRET.as_bytes()),
                "{name} 里不能有 Key 的明文"
            );
        }
    }
}

/// 配置改动按来源记录：本机写入与热加载以捕获时的 t 记为待上传；自己应用的远端值只更新影子
#[test]
fn config_changes_are_journaled_by_origin() {
    let dir = tempfile::tempdir().expect("tempdir");
    let engine = open_engine(dir.path());
    let journal = engine.journal().expect("journal");
    let captures = Arc::new(Captures::default());
    let local = Arc::new(AtomicUsize::new(0));
    let ctx = ctx(&journal, &captures, true, &local);
    let clock = engine.hlc();
    let base = settings_doc(|_| {});

    let t1 = clock.now();
    captures.note(CaptureKey::Doc(ConfigDoc::Settings), t1);
    let after = settings_doc(|s| s.translate.long_text_threshold = 4000);
    writer::process(
        &ctx,
        vec![Msg::Config {
            file: ConfigFile::Settings,
            before: base.clone(),
            after: after.clone(),
            origin: ChangeOrigin::Local,
            lossy: false,
            t: t1,
        }],
    );
    let w = journal
        .winner(&Domain::Cfg, THRESHOLD)
        .expect("read")
        .expect("本机写入被记录");
    assert_eq!((w.t, w.v), (t1, Some(json!(4000))));
    assert!(!captures.doc_pending(ConfigDoc::Settings));

    let t2 = clock.now();
    let edited = settings_doc(|s| {
        s.translate.long_text_threshold = 4000;
        s.translate.close_on_mouse_leave = true;
    });
    writer::process(
        &ctx,
        vec![Msg::Config {
            file: ConfigFile::Settings,
            before: after.clone(),
            after: edited.clone(),
            origin: ChangeOrigin::HotReload,
            lossy: false,
            t: t2,
        }],
    );
    let w = journal
        .winner(&Domain::Cfg, MOUSE_LEAVE)
        .expect("read")
        .expect("热加载被记录");
    assert_eq!((w.t, w.v), (t2, Some(json!(true))));

    let synced = settings_doc(|s| {
        s.translate.long_text_threshold = 4000;
        s.translate.close_on_mouse_leave = true;
        s.translate.default_style = "formal".into();
    });
    let before_count = local.load(Ordering::SeqCst);
    writer::process(
        &ctx,
        vec![Msg::Config {
            file: ConfigFile::Settings,
            before: edited,
            after: synced,
            origin: ChangeOrigin::Sync,
            lossy: false,
            t: Hlc::default(),
        }],
    );
    assert!(
        journal
            .winner(&Domain::Cfg, "/translate/default_style")
            .expect("read")
            .is_none(),
        "远端值不当作本机改动"
    );
    assert_eq!(local.load(Ordering::SeqCst), before_count, "不安排上传");
    let shadow = journal.shadow(ConfigDoc::Settings).expect("read").expect("影子");
    assert_eq!(shadow.map.get("/translate/default_style"), Some(&json!("formal")));
    assert_eq!(shadow.map.get(THRESHOLD), Some(&json!(4000)));

    // 服务地址改到另一台主机、没重新输入 Key：Key 以同一个 t 重新记一条（推送时按新 origin 导出）
    let mut p = provider("svc", &["m"]);
    let before = settings_doc(|s| s.providers = vec![p.clone()]);
    p.base_url = "https://elsewhere.example.org/v1".into();
    let moved = settings_doc(|s| s.providers = vec![p.clone()]);
    let t3 = clock.now();
    writer::process(
        &ctx,
        vec![Msg::Config {
            file: ConfigFile::Settings,
            before,
            after: moved,
            origin: ChangeOrigin::Local,
            lossy: false,
            t: t3,
        }],
    );
    let key = journal
        .winner(&Domain::Key, "svc")
        .expect("read")
        .expect("Key 重新记录");
    assert_eq!(key.t, t3);
}

/// 添加服务时界面先保存 Key、再添加服务：服务出现时补记一条 Key（推送时再从钥匙串读取），中间若有一轮推送把先前那条
/// Key 记录当作「本机没有这个服务」放弃了，Key 仍会跟着服务发出去。没有 Key 的服务不记
#[test]
fn a_new_provider_carries_its_saved_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let engine = open_engine(dir.path());
    let journal = engine.journal().expect("journal");
    let captures = Arc::new(Captures::default());
    let local = Arc::new(AtomicUsize::new(0));
    let clock = engine.hlc();
    let before = settings_doc(|_| {});
    let added = settings_doc(|s| s.providers = vec![provider("fresh", &["m"])]);
    let add = |ctx: &WriterCtx| {
        let t = clock.now();
        writer::process(
            ctx,
            vec![Msg::Config {
                file: ConfigFile::Settings,
                before: before.clone(),
                after: added.clone(),
                origin: ChangeOrigin::Local,
                lossy: false,
                t,
            }],
        );
        t
    };

    add(&ctx(&journal, &captures, false, &local));
    assert!(
        journal.winner(&Domain::Key, "fresh").expect("read").is_none(),
        "没有 Key 的服务不记"
    );
    let t = add(&ctx(&journal, &captures, true, &local));
    let key = journal.winner(&Domain::Key, "fresh").expect("read").expect("补记 Key");
    assert_eq!(key.t, t);
    assert!(key.v.is_some(), "记的是「需要时再读」，不是删除");
}

fn write_json(path: &std::path::Path, f: impl FnOnce(&mut Value)) {
    let mut v: Value = serde_json::from_slice(&std::fs::read(path).expect("read")).expect("json");
    f(&mut v);
    std::fs::write(path, serde_json::to_vec_pretty(&v).expect("json")).expect("write");
}

fn gentry(source: &str, target: &str) -> wisp_types::GlossaryEntry {
    wisp_types::GlossaryEntry {
        source: source.into(),
        target: target.into(),
        note: None,
    }
}

/// 手改文件时写错的内容被宽松解析丢掉（协议拼错、`"providers": null`、术语表里一条写坏）：热加载只发布新增与修改，
/// 不给丢掉的内容发墓碑；用户改正文件之后，它作为新增重新发布
#[test]
fn lossy_hand_edits_are_never_published_as_deletions() {
    let (_dir, first) = test_service();
    first
        .update(&mut |s| s.providers = vec![provider("keep", &["m"]), provider("typo", &["m"])])
        .expect("seed");
    first
        .glossary_set(vec![gentry("hello", "你好"), gentry("world", "世界")])
        .expect("seed");
    // 重新启动：这次是正常读到的文件
    let settings = SettingsService::load_for_tests(first.paths());
    let sync_dir = tempfile::tempdir().expect("tempdir");
    let engine = open_engine(sync_dir.path());
    let journal = engine.journal().expect("journal");
    let captures = Arc::new(Captures::default());
    let local = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel();
    let capturer = Arc::new(Capturer::new(engine.hlc(), tx, Arc::clone(&captures)));
    let hook = Arc::clone(&capturer);
    settings.set_change_hook(Some(Arc::new(move |c: &ConfigChange<'_>| hook.config(c))));
    let ctx = ctx(&journal, &captures, false, &local);
    let drain = || {
        writer::process(&ctx, rx.try_iter().collect());
    };
    let record = |d: Domain, k: &str| journal.winner(&d, k).expect("read");
    let settings_path = settings.paths().settings_file();
    let glossary_path = settings.paths().glossary_file();

    // 协议拼错，同时改了一个合法的值：合法的照常发布，丢掉的服务不发墓碑
    write_json(&settings_path, |v| {
        v["providers"][1]["protocol"] = json!("openai_chatt");
        v["translate"]["long_text_threshold"] = json!(4000);
    });
    assert_eq!(settings.reload(ConfigFile::Settings), ReloadOutcome::Applied);
    drain();
    assert_eq!(record(Domain::Cfg, THRESHOLD).expect("发布").v, Some(json!(4000)));
    assert!(record(Domain::Cfg, "/providers/typo").is_none(), "不发墓碑");
    let shadow = journal.shadow(ConfigDoc::Settings).expect("read").expect("影子");
    assert!(
        !shadow.map.contains_key("/providers/typo"),
        "影子跟随本机的值：之后的启动核对也不会把它当作关闭期间的删除"
    );

    // `"providers": null`：整组退回默认值，同样不发墓碑
    write_json(&settings_path, |v| v["providers"] = Value::Null);
    assert_eq!(settings.reload(ConfigFile::Settings), ReloadOutcome::Applied);
    drain();
    assert!(settings.get().providers.is_empty());
    assert!(record(Domain::Cfg, "/providers/keep").is_none(), "不发墓碑");

    // 改正之后：作为新增重新发布
    write_json(&settings_path, |v| {
        v["providers"] = json!([provider("keep", &["m"]), provider("typo", &["m"])]);
    });
    assert_eq!(settings.reload(ConfigFile::Settings), ReloadOutcome::Applied);
    drain();
    for id in ["keep", "typo"] {
        let r = record(Domain::Cfg, &format!("/providers/{id}")).expect("重新发布");
        assert!(r.v.is_some(), "{id}");
    }

    // 术语表里一条写坏：其余照常，坏掉的这条不发墓碑
    write_json(&glossary_path, |v| v["entries"][1]["target"] = json!(42));
    assert_eq!(settings.reload(ConfigFile::Glossary), ReloadOutcome::Applied);
    drain();
    assert_eq!(*settings.glossary_entries(), vec![gentry("hello", "你好")]);
    assert!(record(Domain::Glo, "world").is_none(), "不发墓碑");

    // 真正的删除（干净的手改）照常发墓碑
    write_json(&glossary_path, |v| {
        v["entries"] = json!([{"source": "world", "target": "世界"}]);
    });
    assert_eq!(settings.reload(ConfigFile::Glossary), ReloadOutcome::Applied);
    drain();
    assert!(record(Domain::Glo, "hello").expect("墓碑").v.is_none());
    assert!(record(Domain::Glo, "world").expect("新增").v.is_some());
    settings.set_change_hook(None);
}

/// 启动核对读到有损的文件（关闭期间手改写错了某个服务）：缺少的 key 不当作关闭期间的删除，只发布新增与修改
#[test]
fn startup_check_does_not_publish_lossy_drops_as_deletions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let engine = open_engine(dir.path());
    let journal = engine.journal().expect("journal");
    let os = OsKind::current();
    let both = settings_doc(|s| s.providers = vec![provider("a", &["m"]), provider("b", &["m"])]);
    let published = projection::project(ConfigDoc::Settings, &both, os);
    // 两个服务早先都已发布（副本里是存活的胜出值）
    let t_earlier = engine.hlc().now();
    journal
        .record(
            ["/providers/a", "/providers/b"]
                .into_iter()
                .map(|k| JournalEntry {
                    d: Domain::Cfg,
                    k: k.into(),
                    t: t_earlier,
                    value: JournalValue::Set(published[k].clone()),
                })
                .collect(),
            Vec::new(),
        )
        .expect("seed winners");
    let seed = || {
        journal
            .record(
                Vec::new(),
                vec![ShadowUpdate::Replace {
                    doc: ConfigDoc::Settings,
                    fingerprint: "v1".into(),
                    map: projection::project(ConfigDoc::Settings, &both, os),
                }],
            )
            .expect("seed");
    };
    // 本机：b 写坏被丢掉，另外改了阈值
    let edited = settings_doc(|s| {
        s.providers = vec![provider("a", &["m"])];
        s.translate.long_text_threshold = 4000;
    });
    let baseline = BTreeMap::from([(ConfigFile::Settings, edited)]);
    let no_keys = |_: &str| None;
    let t0 = engine.hlc().now();
    let run = |state: LoadState| {
        let load_state = move |_: ConfigFile| state;
        reconcile::run(&Startup {
            journal: &journal,
            engine: &engine,
            baseline: &baseline,
            load_state: &load_state,
            t0,
            fingerprint: "v1",
            os,
            key_present: &no_keys,
        })
        .expect("reconcile")
    };

    seed();
    let report = run(LoadState::Lossy);
    assert_eq!(
        report.published, 2,
        "阈值与服务的顺序（顺序里少了 b 不会删掉它：不在顺序里的服务排在后面）"
    );
    let b = journal
        .winner(&Domain::Cfg, "/providers/b")
        .expect("read")
        .expect("早先的记录");
    assert_eq!(b.t, t_earlier, "没有发墓碑");
    assert!(b.v.is_some());
    assert!(journal.winner(&Domain::Cfg, THRESHOLD).expect("read").is_some());

    // 同样的差异出现在正常读到的文件里：那是关闭期间真正的删除，照常发墓碑
    seed();
    run(LoadState::Normal);
    let b = journal
        .winner(&Domain::Cfg, "/providers/b")
        .expect("read")
        .expect("墓碑");
    assert_eq!((b.t, b.v), (t0, None));
}

/// 启动核对：只有既不同于影子、也不同于副本胜出值的本机值才作为手改发布；升级后静默重建；新建的文件不发布
#[test]
fn startup_check_publishes_only_offline_edits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let engine = open_engine(dir.path());
    let journal = engine.journal().expect("journal");
    let os = OsKind::current();
    let base = settings_doc(|_| {});
    // 上次的影子是默认值；另一个 key 的胜出值来自别处，本机值等于它
    journal
        .record(
            vec![JournalEntry {
                d: Domain::Cfg,
                k: MOUSE_LEAVE.into(),
                t: engine.hlc().now(),
                value: JournalValue::Set(json!(true)),
            }],
            vec![ShadowUpdate::Replace {
                doc: ConfigDoc::Settings,
                fingerprint: "v1".into(),
                map: projection::project(ConfigDoc::Settings, &base, os),
            }],
        )
        .expect("seed");
    let edited = settings_doc(|s| {
        s.translate.long_text_threshold = 4000;
        s.translate.close_on_mouse_leave = true;
    });
    let baseline = BTreeMap::from([(ConfigFile::Settings, edited)]);
    let normal = |_: ConfigFile| LoadState::Normal;
    let no_keys = |_: &str| None;
    let t0 = engine.hlc().now();
    let run = |fingerprint: &str, load_state: &dyn Fn(ConfigFile) -> LoadState| {
        reconcile::run(&Startup {
            journal: &journal,
            engine: &engine,
            baseline: &baseline,
            load_state,
            t0,
            fingerprint,
            os,
            key_present: &no_keys,
        })
        .expect("reconcile")
    };

    let report = run("v1", &normal);
    assert_eq!(report.published, 1, "只有阈值是手改");
    let w = journal.winner(&Domain::Cfg, THRESHOLD).expect("read").expect("发布");
    assert_eq!((w.t, w.v), (t0, Some(json!(4000))));
    let shadow = journal.shadow(ConfigDoc::Settings).expect("read").expect("影子");
    assert_eq!(shadow.map.get(THRESHOLD), Some(&json!(4000)), "影子更新为当前值");

    assert_eq!(run("v1", &normal).published, 0, "再启动一次没有新的手改");

    let upgraded = run("v2", &normal);
    assert_eq!(upgraded.published, 0, "升级后不发布");
    assert!(upgraded.reapplied.contains(&Domain::Cfg));

    let created = |f: ConfigFile| {
        if f == ConfigFile::Glossary {
            LoadState::Created
        } else {
            LoadState::Normal
        }
    };
    let report = run("v2", &created);
    assert!(report.reapplied.contains(&Domain::Glo), "新建的文件等服务器的值");
    assert_eq!(report.published, 0);
}

/// 远端配置经反投影写回；这一轮里本机改过这份文档时整批推迟，下一轮再应用
#[test]
fn remote_config_is_applied_unless_changed_locally_during_the_cycle() {
    let (_settings_dir, settings) = test_service();
    let dir = tempfile::tempdir().expect("tempdir");
    let engine = open_engine(dir.path());
    let captures = Arc::new(Captures::default());
    let store = DesktopStore::new(
        Arc::clone(&settings),
        Arc::new(|| None),
        engine.journal().expect("journal"),
        Arc::clone(&captures),
    );
    let set = |value: u32| {
        vec![RemoteChange::Set {
            key: THRESHOLD.into(),
            value: json!(value),
        }]
    };

    let out = store.apply(Domain::Cfg, set(4000)).expect("apply");
    assert_eq!(out.results, vec![KeyApply::Applied]);
    assert_eq!(settings.get().translate.long_text_threshold, 4000);

    // 本轮开始之后本机改过 settings（已经写进副本，但引擎这一轮看不到）
    captures.begin_cycle();
    let t = Hlc::from_wall_ms(1);
    captures.note(CaptureKey::Doc(ConfigDoc::Settings), t);
    captures.settle(&[(CaptureKey::Doc(ConfigDoc::Settings), t)]);
    let out = store.apply(Domain::Cfg, set(5000)).expect("apply");
    assert_eq!(out.results, vec![KeyApply::Deferred]);
    assert_eq!(settings.get().translate.long_text_threshold, 4000, "本机的改动不被覆盖");
    captures.end_cycle();

    let out = store.apply(Domain::Cfg, set(5000)).expect("apply");
    assert_eq!(out.results, vec![KeyApply::Applied]);
    assert_eq!(settings.get().translate.long_text_threshold, 5000);

    // 术语
    let out = store
        .apply(
            Domain::Glo,
            vec![RemoteChange::Set {
                key: "hello".into(),
                value: json!({ "target": "你好", "note": "" }),
            }],
        )
        .expect("apply");
    assert_eq!(out.results, vec![KeyApply::Applied]);
    assert!(settings.glossary_entries().iter().any(|e| e.source == "hello"));

    // 剪贴板库还没打开：推迟
    assert!(matches!(
        store.apply(
            Domain::Clip,
            vec![RemoteChange::ClipPin {
                hash: [1; 32],
                pinned: true,
                name: None,
            }]
        ),
        Err(wisp_sync::LocalError::Unavailable)
    ));
    // Key 只在允许读写钥匙串的轮次里写
    let key = vec![RemoteChange::Delete { key: "svc".into() }];
    assert_eq!(
        store.apply(Domain::Key, key).expect("apply").results,
        vec![KeyApply::Deferred]
    );
}

/// 钥匙串里的凭据属于另一个账号：按缺少凭据处理，暂停自动同步；本机改动触发的轮次不读钥匙串
#[tokio::test]
async fn credentials_for_another_account_count_as_missing() {
    let (_dir, settings) = test_service();
    settings
        .update(&mut |s| {
            s.sync.enabled = true;
            s.sync.server_url = "https://dav.example.com/dav/".into();
            s.sync.username = "me".into();
        })
        .expect("update");
    settings
        .sync_credentials()
        .set(&SyncCredential {
            server_url: "https://other.example.com/dav/".into(),
            username: "me".into(),
            password: "pw".into(),
            vault_id: "0123456789abcdef".into(),
            key: "AAAA".into(),
        })
        .expect("save");
    let svc = SyncService::with_parts(Arc::clone(&settings), None, "0.0.0".into());
    {
        let _life = svc.lifecycle.lock().await;
        svc.enable_locked().await;
    }
    assert!(svc.is_enabled());
    assert!(svc.db_path().exists(), "开启后才有本机副本");
    let active = svc.active().expect("active");
    assert!(active.writer.is_running());

    assert!(
        matches!(svc.credentials(CycleReason::LocalChange, &active).await, Ok(None)),
        "没有用户活动不读钥匙串"
    );
    let err = svc
        .credentials(CycleReason::TrayOpened, &active)
        .await
        .expect_err("账号不匹配");
    assert_eq!(errors::kind_of(err.kind()), SyncErrorKind::CredentialsMissing);

    svc.run_one(CycleReason::Manual, &mut None).await;
    let status = svc.status();
    assert_eq!(status.phase, SyncPhase::Error);
    assert_eq!(status.error, Some(SyncErrorKind::CredentialsMissing));
    assert!(!status.transient);
    assert!(svc.needs_attention());
    assert!(
        !svc.gate.allows(CycleReason::TrayOpened, i64::MAX / 2),
        "暂停自动同步直到用户操作"
    );

    svc.deactivate().await;
    assert!(svc.active().is_none());
    assert!(!active.writer.is_running(), "关闭后写入线程退出");
}

/// 启动时开启本机部分失败（sync.db 打不开）：「立即同步」重试开启，而不是回答「尚未连接」
#[tokio::test]
async fn sync_now_retries_a_failed_start() {
    let (_dir, settings) = test_service();
    settings
        .update(&mut |s| {
            s.sync.enabled = true;
            s.sync.server_url = "https://dav.example.com/dav/".into();
            s.sync.username = "me".into();
        })
        .expect("update");
    let svc = SyncService::with_parts(Arc::clone(&settings), None, "0.0.0".into());
    // 用一个同名目录挡住 sync.db
    std::fs::create_dir_all(svc.db_path()).expect("obstruct");
    {
        let _life = svc.lifecycle.lock().await;
        svc.enable_locked().await;
    }
    assert!(!svc.is_enabled());
    assert_eq!(svc.status().phase, SyncPhase::Error);

    std::fs::remove_dir(svc.db_path()).expect("unblock");
    svc.sync_now().expect("重试开启");
    for _ in 0..200 {
        if svc.is_enabled() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(svc.is_enabled(), "重试之后开启");
    svc.deactivate().await;
}

/// 第一次连接失败：本次开启的一切都撤回（副本关闭并删除），设置仍是未连接
#[tokio::test]
async fn a_failed_first_connect_leaves_nothing_behind() {
    let (_dir, settings) = test_service();
    let svc = SyncService::with_parts(Arc::clone(&settings), None, "0.0.0".into());
    // 本机回环上一个刚释放的端口：连接立即被拒绝，不出本机
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("port")
        .port();
    let account = wisp_types::SyncAccount {
        server_url: format!("http://127.0.0.1:{port}/dav/"),
        username: "me".into(),
    };
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        svc.connect(
            account,
            Some("pw".into()),
            "correct horse battery staple".into(),
            wisp_types::SyncContent::default(),
        ),
    )
    .await
    .expect("不应挂起");
    assert!(result.is_err());
    assert!(svc.active().is_none());
    assert!(!svc.db_path().exists(), "本次新建的副本删掉");
    assert!(!svc.is_enabled());
    assert!(!settings.get().sync.enabled);
    assert_eq!(svc.status(), SyncStatus::off());
}

/// 清除自己的服务器之后断开：保存的凭据只留应用密码（不再绑定已清除的数据），随后的测试连接与连接可以留空沿用；
/// 普通的断开整项删除
#[tokio::test]
async fn clearing_keeps_only_the_app_password() {
    let (_dir, settings) = test_service();
    let (url, user) = ("https://dav.example.com/dav/", "me");
    settings
        .update(&mut |s| {
            s.sync.enabled = true;
            s.sync.server_url = url.into();
            s.sync.username = user.into();
        })
        .expect("update");
    let full = SyncCredential {
        server_url: url.into(),
        username: user.into(),
        password: "pw".into(),
        vault_id: "0123456789abcdef".into(),
        key: "AAAA".into(),
    };
    settings.sync_credentials().set(&full).expect("save");
    let svc = SyncService::with_parts(Arc::clone(&settings), None, "0.0.0".into());
    let keep = SyncCredential {
        server_url: url.into(),
        username: user.into(),
        password: "pw".into(),
        vault_id: String::new(),
        key: String::new(),
    };
    {
        let _life = svc.lifecycle.lock().await;
        svc.disconnect_locked(Some(keep.clone())).await.expect("disconnect");
    }
    assert!(!settings.get().sync.enabled);
    assert_eq!(settings.sync_credentials().get().expect("read"), Some(keep));
    assert_eq!(
        svc.stored_password(url, user).await.expect("read").as_deref(),
        Some("pw")
    );
    assert_eq!(svc.stored_password(url, "other").await.expect("read"), None);

    {
        let _life = svc.lifecycle.lock().await;
        svc.disconnect_locked(None).await.expect("disconnect");
    }
    assert_eq!(settings.sync_credentials().get().expect("read"), None);
    assert_eq!(svc.stored_password(url, user).await.expect("read"), None);
}

/// 对外的异步接口要能放进 Tauri 的异步命令（future 必须是 Send）
#[allow(dead_code)]
fn public_futures_are_send(svc: &Arc<SyncService>) {
    fn is_send<T: Send>(_: T) {}
    let account = || wisp_types::SyncAccount {
        server_url: String::new(),
        username: String::new(),
    };
    is_send(svc.connect(account(), None, String::new(), wisp_types::SyncContent::default()));
    is_send(svc.probe(account(), None));
    is_send(svc.disconnect());
    is_send(svc.reset_remote(None, None));
    is_send(svc.devices());
}
