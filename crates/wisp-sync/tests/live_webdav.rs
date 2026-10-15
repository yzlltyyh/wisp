//! 对真实 WebDAV 服务器的冒烟测试，默认忽略。运行方式：
//!
//! ```text
//! WISP_SYNC_TEST_URL=https://… WISP_SYNC_TEST_USER=… WISP_SYNC_TEST_PASS=… \
//!   cargo test -p wisp-sync --locked --test live_webdav -- --ignored
//! ```
//!
//! 只在同步根目录（地址之下的 `wisp/`）里新建一个随机子目录，结束时（成功或失败）删除它；
//! 根目录是本测试创建的、且结束时为空，才一并删除。不读、不改其他任何文件。

use bytes::Bytes;
use wisp_sync::crypto::{self, MasterKey};
use wisp_sync::{DirCache, Keys, Remote, RemoteError, WebDav};

fn account() -> (String, String, String) {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("需要环境变量 {name}"));
    (
        var("WISP_SYNC_TEST_URL"),
        var("WISP_SYNC_TEST_USER"),
        var("WISP_SYNC_TEST_PASS"),
    )
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

#[tokio::test]
#[ignore = "需要真实 WebDAV 账号"]
async fn live_server_round_trip() {
    let (url, user, pass) = account();
    let dav = WebDav::new(&url, &user, &pass).expect("server url");
    let root_existed = dav.list("").await.expect("list sync root").is_some();
    let sub = format!("test-{}", crypto::random_id().unwrap());

    let outcome = exercise(&dav, &sub).await;

    let cleanup = dav.delete(&format!("{sub}/")).await;
    // 删除可能是异步的（见 exercise 末尾）：等测试目录消失后再判断根目录是否已空
    let mut sub_gone = false;
    for _ in 0..20 {
        if matches!(dav.list(&sub).await, Ok(None)) {
            sub_gone = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    if !root_existed && matches!(dav.list("").await, Ok(Some(entries)) if entries.is_empty()) {
        let _ = dav.delete("").await;
    }
    outcome.unwrap();
    cleanup.expect("delete test directory");
    assert!(sub_gone, "test directory still listed after 10 s");
}

async fn exercise(dav: &WebDav, sub: &str) -> Result<(), String> {
    let step = |what: &'static str| move |e: RemoteError| format!("{what}: {e} ({})", e.detail());
    let nested = format!("{sub}/nested");

    // 目录：逐级 MKCOL；已存在视为成功
    let mut dirs = DirCache::new();
    dirs.ensure(dav, &nested).await.map_err(step("mkcol"))?;
    dav.mkcol(sub).await.map_err(step("mkcol existing"))?;

    // 小文件：写、读（可能 302 到 CDN）、覆盖后 ETag 改变
    let small = format!("{sub}/small.bin");
    let first = dav
        .put(&small, Bytes::from(pattern(1000, 1)))
        .await
        .map_err(step("put"))?;
    let got = dav.get(&small).await.map_err(step("get"))?;
    assert_eq!(got.as_deref(), Some(pattern(1000, 1).as_slice()));
    let listed = dav.list(sub).await.map_err(step("list"))?.expect("test dir exists");
    let entry = listed.iter().find(|e| e.name == "small.bin").expect("small.bin listed");
    assert!(!entry.is_dir);
    assert_eq!(entry.size, Some(1000));
    assert!(listed.iter().any(|e| e.name == "nested" && e.is_dir));
    let before = entry.etag.clone();

    let second = dav
        .put(&small, Bytes::from(pattern(1200, 2)))
        .await
        .map_err(step("overwrite"))?;
    if first.etag.is_some() && second.etag.is_some() {
        assert_ne!(first.etag, second.etag);
    }
    let after = dav.stat(&small).await.map_err(step("stat"))?.expect("small.bin exists");
    assert_eq!(after.size, Some(1200));
    if before.is_some() && after.etag.is_some() {
        assert_ne!(before, after.etag);
    }

    // blob：加密后按流上传，按流下载（跟随重定向）再解密
    let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let plain_path = tmp.path().join("plain");
    let sealed_path = tmp.path().join("sealed");
    let fetched_path = tmp.path().join("fetched");
    let opened_path = tmp.path().join("opened");
    let plain = pattern(300_000, 3);
    std::fs::write(&plain_path, &plain).map_err(|e| e.to_string())?;
    let vault = crypto::random_id().expect("random id");
    let keys = Keys::derive(&MasterKey::from_bytes([1; 32]), &vault).expect("derive keys");
    let name = crypto::blob_name(&keys, b"live-test");
    let aad = format!("blobs/{sub}/{name}");
    let sealed_len = crypto::seal_blob_file(&keys, &vault, &aad, &plain_path, &sealed_path).expect("seal blob");
    let blob = format!("{nested}/{name}.blob");
    dav.put_file(&blob, &sealed_path, sealed_len)
        .await
        .map_err(step("put_file"))?;
    assert!(
        dav.get_to_file(&blob, &fetched_path)
            .await
            .map_err(step("get_to_file"))?
    );
    crypto::open_blob_file(&keys, &vault, &aad, &fetched_path, &opened_path).expect("open blob");
    assert_eq!(std::fs::read(&opened_path).expect("read opened blob"), plain);

    // 父目录不存在的 PUT：最弱的服务器报 409，有的服务器自动建目录
    let orphan = format!("{sub}/missing/x.bin");
    match dav.put(&orphan, Bytes::from_static(b"x")).await {
        Ok(_) | Err(RemoteError::Status(409)) => {}
        Err(e) => return Err(step("put without parent")(e)),
    }

    // 不存在的文件与目录
    assert_eq!(dav.get(&format!("{sub}/nope")).await, Ok(None));
    assert_eq!(
        dav.get_to_file(&format!("{sub}/nope"), &tmp.path().join("nope")).await,
        Ok(false)
    );
    assert!(!tmp.path().join("nope").exists());
    assert_eq!(dav.list(&format!("{sub}/nope")).await, Ok(None));

    // 删除：有的服务器（如 123 云盘）DELETE 先返回成功、约一秒后才真正删除，期间列目录与 GET 都还能看到旧文件，
    // 所以这里等它消失（最多约 10 秒）；重复删除也算成功
    dav.delete(&small).await.map_err(step("delete"))?;
    let mut gone = false;
    for _ in 0..20 {
        if dav.stat(&small).await.map_err(step("stat after delete"))?.is_none() {
            gone = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    if !gone {
        return Err("deleted file still listed after 10 s".into());
    }
    dav.delete(&small).await.map_err(step("delete again"))?;
    Ok(())
}
