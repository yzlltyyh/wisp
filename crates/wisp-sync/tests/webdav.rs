//! WebDAV 客户端对 wiremock：多种服务器风格的 PROPFIND、重定向规则（跨源不带 Authorization 与 Referer、
//! http → https、非 GET 遇到 3xx）、按流上传与下载、状态码到错误种类的映射。

use std::time::Duration;

use bytes::Bytes;
use wiremock::matchers::{any, header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};
use wisp_sync::error::ErrorKind;
use wisp_sync::{Entry, Remote, RemoteError, WebDav};

/// base64("u:p")
const AUTH: &str = "Basic dTpw";
const T0: i64 = 1_791_964_800_000; // Wed, 14 Oct 2026 08:00:00 GMT
const T1: i64 = 1_791_964_801_000; // Wed, 14 Oct 2026 08:00:01 GMT

fn dav(server: &MockServer, base_path: &str) -> WebDav {
    WebDav::new(&format!("{}{base_path}", server.uri()), "u", "p").expect("valid test url")
}

fn multistatus(body: impl Into<String>) -> ResponseTemplate {
    ResponseTemplate::new(207).set_body_raw(body.into().into_bytes(), "application/xml; charset=utf-8")
}

async fn requests(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap_or_default()
}

fn header_of<'r>(req: &'r Request, name: &str) -> Option<&'r str> {
    req.headers.get(name).and_then(|v| v.to_str().ok())
}

fn file(name: &str, etag: Option<&str>, size: Option<u64>, modified_ms: Option<i64>) -> Entry {
    Entry {
        name: name.into(),
        is_dir: false,
        etag: etag.map(Into::into),
        size,
        modified_ms,
    }
}

fn dir(name: &str, etag: Option<&str>, modified_ms: Option<i64>) -> Entry {
    Entry {
        name: name.into(),
        is_dir: true,
        etag: etag.map(Into::into),
        size: None,
        modified_ms,
    }
}

// ---------------------------------------------------------------------------------------------
// PROPFIND 样本

/// 123 云盘风格：`D:` 前缀，带引号与不带引号的 ETag 混用
const PAN123: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/webdav/wisp/</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype><D:collection/></D:resourcetype>
        <D:getlastmodified>Wed, 14 Oct 2026 08:00:00 GMT</D:getlastmodified>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/webdav/wisp/vault.json</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype/>
        <D:getetag>"9a0364b9e99bb480dd25e1f0284c8555"</D:getetag>
        <D:getcontentlength>312</D:getcontentlength>
        <D:getlastmodified>Wed, 14 Oct 2026 08:00:01 GMT</D:getlastmodified>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/webdav/wisp/0123456789abcdef.fedcba9876543210.head</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype></D:resourcetype>
        <D:getetag>5d41402abc4b2a76b9719d911017c592</D:getetag>
        <D:getcontentlength>1024</D:getcontentlength>
        <D:getlastmodified>Wed, 14 Oct 2026 08:00:00 GMT</D:getlastmodified>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/webdav/wisp/0123456789abcdef/</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype><D:collection/></D:resourcetype>
        <D:getlastmodified>Wed, 14 Oct 2026 08:00:00 GMT</D:getlastmodified>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#;

#[tokio::test]
async fn propfind_pan123_style() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .and(path("/webdav/wisp/"))
        .and(header("depth", "1"))
        .and(header("authorization", AUTH))
        .respond_with(multistatus(PAN123))
        .expect(1)
        .mount(&server)
        .await;
    let entries = dav(&server, "/webdav").list("").await.unwrap().unwrap();
    assert_eq!(
        entries,
        vec![
            file(
                "vault.json",
                Some("9a0364b9e99bb480dd25e1f0284c8555"),
                Some(312),
                Some(T1)
            ),
            file(
                "0123456789abcdef.fedcba9876543210.head",
                Some("5d41402abc4b2a76b9719d911017c592"),
                Some(1024),
                Some(T0)
            ),
            dir("0123456789abcdef", None, Some(T0)),
        ]
    );
    // 只请求用得到的属性
    let req = &requests(&server).await[0];
    let body = String::from_utf8_lossy(&req.body);
    assert!(body.contains("getetag") && body.contains("resourcetype") && !body.contains("allprop"));
    assert!(header_of(req, "user-agent").is_some_and(|ua| ua.starts_with("Wisp/")));
}

/// Nextcloud 风格：`d:` 前缀、实体转义的引号、404 的 propstat、路径里的百分号编码
const NEXTCLOUD: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns">
 <d:response>
  <d:href>/remote.php/dav/files/some%20one/wisp/</d:href>
  <d:propstat>
   <d:prop>
    <d:getlastmodified>Wed, 14 Oct 2026 08:00:00 GMT</d:getlastmodified>
    <d:resourcetype><d:collection/></d:resourcetype>
    <d:getetag>&quot;6527f0a1b2c3d&quot;</d:getetag>
   </d:prop>
   <d:status>HTTP/1.1 200 OK</d:status>
  </d:propstat>
  <d:propstat>
   <d:prop><d:getcontentlength/></d:prop>
   <d:status>HTTP/1.1 404 Not Found</d:status>
  </d:propstat>
 </d:response>
 <d:response>
  <d:href>/remote.php/dav/files/some%20one/wisp/vault.json</d:href>
  <d:propstat>
   <d:prop>
    <d:getlastmodified>Wed, 14 Oct 2026 08:00:01 GMT</d:getlastmodified>
    <d:getcontentlength>298</d:getcontentlength>
    <d:resourcetype/>
    <d:getetag>&quot;0b1e2f3a4b5c6d7e&quot;</d:getetag>
   </d:prop>
   <d:status>HTTP/1.1 200 OK</d:status>
  </d:propstat>
  <d:propstat>
   <d:prop><oc:fileid/></d:prop>
   <d:status>HTTP/1.1 404 Not Found</d:status>
  </d:propstat>
 </d:response>
 <d:response>
  <d:href>/remote.php/dav/files/some%20one/wisp/0123456789abcdef/</d:href>
  <d:propstat>
   <d:prop>
    <d:resourcetype><d:collection/></d:resourcetype>
    <d:getetag>&quot;6527f0a1b2c3e&quot;</d:getetag>
   </d:prop>
   <d:status>HTTP/1.1 200 OK</d:status>
  </d:propstat>
 </d:response>
</d:multistatus>"#;

#[tokio::test]
async fn propfind_nextcloud_style() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .and(path("/remote.php/dav/files/some%20one/wisp/"))
        .respond_with(multistatus(NEXTCLOUD))
        .mount(&server)
        .await;
    let entries = dav(&server, "/remote.php/dav/files/some one")
        .list("")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        entries,
        vec![
            file("vault.json", Some("0b1e2f3a4b5c6d7e"), Some(298), Some(T1)),
            dir("0123456789abcdef", Some("6527f0a1b2c3e"), None),
        ]
    );
}

/// Apache mod_dav 风格：属性用 `lp1:` 等前缀（绑定到 DAV:），弱 ETag，404 的 propstat 里有同名属性
const APACHE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:ns0="DAV:">
<D:response xmlns:lp1="DAV:" xmlns:lp2="http://apache.org/dav/props/">
<D:href>/dav/wisp/0123456789abcdef/log/fedcba9876543210/</D:href>
<D:propstat>
<D:prop>
<lp1:resourcetype><D:collection/></lp1:resourcetype>
<lp1:getlastmodified>Wed, 14 Oct 2026 08:00:00 GMT</lp1:getlastmodified>
<lp1:getetag>"1000-5b1c2d3e4f5a6"</lp1:getetag>
</D:prop>
<D:status>HTTP/1.1 200 OK</D:status>
</D:propstat>
<D:propstat>
<D:prop>
<g0:getcontentlength xmlns:g0="DAV:"/>
</D:prop>
<D:status>HTTP/1.1 404 Not Found</D:status>
</D:propstat>
</D:response>
<D:response xmlns:lp1="DAV:" xmlns:lp2="http://apache.org/dav/props/">
<D:href>/dav/wisp/0123456789abcdef/log/fedcba9876543210/0000000001.seg</D:href>
<D:propstat>
<D:prop>
<lp1:resourcetype/>
<lp1:getcontentlength>2048</lp1:getcontentlength>
<lp1:getlastmodified>Wed, 14 Oct 2026 08:00:01 GMT</lp1:getlastmodified>
<lp1:getetag>W/"800-5b1c2d3e4f5a7"</lp1:getetag>
</D:prop>
<D:status>HTTP/1.1 200 OK</D:status>
</D:propstat>
<D:propstat>
<D:prop>
<lp1:getetag>"should-be-ignored"</lp1:getetag>
</D:prop>
<D:status>HTTP/1.1 404 Not Found</D:status>
</D:propstat>
</D:response>
</D:multistatus>"#;

#[tokio::test]
async fn propfind_apache_style() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .and(path("/dav/wisp/0123456789abcdef/log/fedcba9876543210/"))
        .respond_with(multistatus(APACHE))
        .mount(&server)
        .await;
    let entries = dav(&server, "/dav")
        .list("0123456789abcdef/log/fedcba9876543210")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        entries,
        vec![file("0000000001.seg", Some("800-5b1c2d3e4f5a7"), Some(2048), Some(T1))]
    );
}

/// 坚果云风格：中文路径（百分号编码）、没有 ETag、目录自身的 href 不带结尾 `/`、附带大量无关属性
const NUTSTORE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="no"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://ns.jianguoyun.com">
<d:response><d:href>/dav/%E6%88%91%E7%9A%84%E5%9D%9A%E6%9E%9C%E4%BA%91/wisp</d:href><d:propstat><d:prop><d:getlastmodified>Wed, 14 Oct 2026 08:00:00 GMT</d:getlastmodified><d:getcontentlength>0</d:getcontentlength><d:owner>someone</d:owner><d:current-user-privilege-set><d:privilege><d:read/></d:privilege><d:privilege><d:write/></d:privilege></d:current-user-privilege-set><d:getcontenttype>httpd/unix-directory</d:getcontenttype><d:displayname>wisp</d:displayname><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
<d:response><d:href>/dav/%E6%88%91%E7%9A%84%E5%9D%9A%E6%9E%9C%E4%BA%91/wisp/vault.json</d:href><d:propstat><d:prop><d:getlastmodified>Wed, 14 Oct 2026 08:00:01 GMT</d:getlastmodified><d:getcontentlength>301</d:getcontentlength><d:owner>someone</d:owner><d:getcontenttype>application/json</d:getcontenttype><d:displayname>vault.json</d:displayname><d:resourcetype/></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
<d:response><d:href>/dav/%E6%88%91%E7%9A%84%E5%9D%9A%E6%9E%9C%E4%BA%91/wisp/%E6%97%A7%E6%96%87%E4%BB%B6.txt</d:href><d:propstat><d:prop><d:getcontentlength>5</d:getcontentlength><d:resourcetype/></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
</d:multistatus>"#;

#[tokio::test]
async fn propfind_nutstore_style() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .and(path("/dav/%E6%88%91%E7%9A%84%E5%9D%9A%E6%9E%9C%E4%BA%91/wisp/"))
        .respond_with(multistatus(NUTSTORE))
        .mount(&server)
        .await;
    let entries = dav(&server, "/dav/我的坚果云").list("").await.unwrap().unwrap();
    assert_eq!(
        entries,
        vec![
            file("vault.json", None, Some(301), Some(T1)),
            file("旧文件.txt", None, Some(5), None),
        ]
    );
}

#[tokio::test]
async fn propfind_default_namespace_absolute_urls_cdata_and_entities() {
    let server = MockServer::start().await;
    let origin = server.uri();
    let body = format!(
        r#"<multistatus xmlns="DAV:">
 <response><href>{origin}/dav/wisp</href><propstat><prop><resourcetype><collection/></resourcetype></prop><status>HTTP/1.1 200 OK</status></propstat></response>
 <response><href>
   {origin}/dav/wisp/a%20b.txt
 </href><propstat><prop><getcontentlength> 7 </getcontentlength><getetag>"x&amp;y"</getetag></prop><status>HTTP/1.1 200 OK</status></propstat></response>
 <response><href><![CDATA[/dav/wisp/c&d.txt]]></href><propstat><prop><getetag>&#34;cd&#x22;</getetag><getlastmodified>not a date</getlastmodified></prop><status>HTTP/1.1 200 OK</status></propstat></response>
 <response><href>/dav/wisp/gone.txt</href><status>HTTP/1.1 404 Not Found</status></response>
 <response><href>/dav/wisp/sub/</href><propstat><prop><resourcetype><collection></collection></resourcetype></prop></propstat></response>
 <!-- 注释 -->
</multistatus>"#
    );
    Mock::given(method("PROPFIND"))
        .and(path("/dav/wisp/"))
        .respond_with(multistatus(body))
        .mount(&server)
        .await;
    let entries = dav(&server, "/dav").list("").await.unwrap().unwrap();
    assert_eq!(
        entries,
        vec![
            file("a b.txt", Some("x&y"), Some(7), None),
            file("c&d.txt", Some("cd"), None, None),
            dir("sub", None, None),
        ]
    );
}

#[tokio::test]
async fn propfind_behind_a_proxy_that_strips_the_prefix() {
    let server = MockServer::start().await;
    let body = r#"<D:multistatus xmlns:D="DAV:">
<D:response><D:href>/wisp/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
<D:response><D:href>/wisp/vault.json</D:href><D:propstat><D:prop><D:getcontentlength>3</D:getcontentlength></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
</D:multistatus>"#;
    Mock::given(method("PROPFIND"))
        .and(path("/proxied/dav/wisp/"))
        .respond_with(multistatus(body))
        .mount(&server)
        .await;
    let entries = dav(&server, "/proxied/dav").list("").await.unwrap().unwrap();
    assert_eq!(entries, vec![file("vault.json", None, Some(3), None)]);
}

#[tokio::test]
async fn propfind_missing_and_malformed() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .and(path("/dav/wisp/missing/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("PROPFIND"))
        .and(path("/dav/wisp/html/"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html><body>portal</body></html>"))
        .mount(&server)
        .await;
    Mock::given(method("PROPFIND"))
        .and(path("/dav/wisp/cut/"))
        .respond_with(multistatus(
            r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/dav/wisp/cut/</D:href>"#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("PROPFIND"))
        .and(path("/dav/wisp/garbage/"))
        .respond_with(multistatus("<D:multistatus><a></b></D:multistatus>"))
        .mount(&server)
        .await;
    Mock::given(method("PROPFIND"))
        .and(path("/dav/wisp/empty/"))
        .respond_with(multistatus(r#"<D:multistatus xmlns:D="DAV:"/>"#))
        .mount(&server)
        .await;
    let dav = dav(&server, "/dav");
    assert_eq!(dav.list("missing").await, Ok(None));
    assert_eq!(dav.list("html").await, Err(RemoteError::Protocol));
    assert_eq!(dav.list("cut").await, Err(RemoteError::Protocol));
    assert_eq!(dav.list("garbage").await, Err(RemoteError::Protocol));
    assert_eq!(dav.list("empty").await, Ok(Some(Vec::new())));
}

#[tokio::test]
async fn stat_uses_depth_zero() {
    let server = MockServer::start().await;
    let body = r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/dav/wisp/vault.json</D:href><D:propstat><D:prop><D:getetag>"e1"</D:getetag><D:getcontentlength>9</D:getcontentlength><D:resourcetype/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;
    Mock::given(method("PROPFIND"))
        .and(path("/dav/wisp/vault.json"))
        .and(header("depth", "0"))
        .respond_with(multistatus(body))
        .mount(&server)
        .await;
    let dav = dav(&server, "/dav");
    assert_eq!(
        dav.stat("vault.json").await.unwrap(),
        Some(file("vault.json", Some("e1"), Some(9), None))
    );
    assert_eq!(dav.stat("nope").await.unwrap(), None);
}

// ---------------------------------------------------------------------------------------------
// 读写

#[tokio::test]
async fn put_get_delete_mkcol_paths_and_results() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/dav/wisp/v.d.head"))
        .and(header("authorization", AUTH))
        .respond_with(ResponseTemplate::new(201).insert_header("ETag", "W/\"abc\""))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/dav/wisp/noetag"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/dav/wisp/vault.json"))
        .and(header("authorization", AUTH))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"{\"wisp_sync\":1}".to_vec()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("MKCOL"))
        .and(path("/dav/wisp/"))
        .respond_with(ResponseTemplate::new(405))
        .mount(&server)
        .await;
    Mock::given(method("MKCOL"))
        .and(path("/dav/wisp/v/log/"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;
    Mock::given(method("MKCOL"))
        .and(path("/dav/wisp/x/y/"))
        .respond_with(ResponseTemplate::new(409))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/dav/wisp/v/"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/dav/wisp/partial/"))
        .respond_with(ResponseTemplate::new(207))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let dav = dav(&server, "/dav");
    let info = dav.put("v.d.head", Bytes::from_static(b"sealed")).await.unwrap();
    assert_eq!(info.etag.as_deref(), Some("abc"));
    assert_eq!(dav.put("noetag", Bytes::from_static(b"")).await.unwrap().etag, None);
    assert_eq!(
        dav.get("vault.json").await.unwrap().as_deref(),
        Some(&b"{\"wisp_sync\":1}"[..])
    );
    assert_eq!(dav.get("missing").await, Ok(None));
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("none");
    assert_eq!(dav.get_to_file("missing", &dest).await, Ok(false));
    assert!(!dest.exists());

    dav.mkcol("").await.unwrap();
    dav.mkcol("v/log").await.unwrap();
    assert_eq!(dav.mkcol("x/y").await, Err(RemoteError::Status(409)));
    dav.delete("v/").await.unwrap();
    dav.delete("already/gone").await.unwrap();
    assert_eq!(dav.delete("partial/").await, Err(RemoteError::Status(207)));

    let put = requests(&server)
        .await
        .into_iter()
        .find(|r| r.url.path() == "/dav/wisp/v.d.head")
        .unwrap();
    assert_eq!(put.body, b"sealed");
    assert_eq!(header_of(&put, "content-length"), Some("6"));
}

#[tokio::test]
async fn put_file_streams_with_content_length() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/dav/wisp/v/blobs/d/n.blob"))
        .and(header("authorization", AUTH))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&server)
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("sealed");
    let data: Vec<u8> = (0..300_000u32).map(|i| (i % 253) as u8).collect();
    std::fs::write(&src, &data).unwrap();

    let dav = dav(&server, "/dav");
    dav.put_file("v/blobs/d/n.blob", &src, data.len() as u64).await.unwrap();
    let req = &requests(&server).await[0];
    assert_eq!(req.body, data);
    assert_eq!(header_of(req, "content-length"), Some("300000"));
    assert_eq!(header_of(req, "transfer-encoding"), None);

    // 长度与文件不符：不发请求
    assert_eq!(
        dav.put_file("v/blobs/d/n.blob", &src, 5).await,
        Err(RemoteError::Io(std::io::ErrorKind::InvalidData))
    );
    assert_eq!(
        dav.put_file("v/blobs/d/n.blob", &tmp.path().join("missing"), 5).await,
        Err(RemoteError::Io(std::io::ErrorKind::NotFound))
    );
}

// ---------------------------------------------------------------------------------------------
// 重定向

/// 123 云盘的 GET 302 到另一台主机（CDN）：跟随，但不把 Authorization 与 Referer 带过去
#[tokio::test]
async fn get_follows_redirect_to_another_origin_without_credentials() {
    let origin = MockServer::start().await;
    let cdn = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/webdav/wisp/v/blobs/d/n.blob"))
        .and(header("authorization", AUTH))
        .respond_with(
            ResponseTemplate::new(302).insert_header("Location", format!("{}/cdn/n.blob?sig=s3cr3t", cdn.uri())),
        )
        .mount(&origin)
        .await;
    Mock::given(method("GET"))
        .and(path("/webdav/wisp/v.d.head"))
        .respond_with(ResponseTemplate::new(302).insert_header("Location", format!("{}/cdn/head", cdn.uri())))
        .mount(&origin)
        .await;
    Mock::given(method("GET"))
        .and(path("/cdn/n.blob"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![7u8; 100_000]))
        .mount(&cdn)
        .await;
    Mock::given(method("GET"))
        .and(path("/cdn/head"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"head".to_vec()))
        .mount(&cdn)
        .await;

    let dav = dav(&origin, "/webdav");
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("blob");
    assert_eq!(dav.get_to_file("v/blobs/d/n.blob", &dest).await, Ok(true));
    assert_eq!(std::fs::read(&dest).unwrap(), vec![7u8; 100_000]);
    assert_eq!(dav.get("v.d.head").await.unwrap().as_deref(), Some(&b"head"[..]));

    let cdn_requests = requests(&cdn).await;
    assert_eq!(cdn_requests.len(), 2);
    for req in &cdn_requests {
        assert_eq!(header_of(req, "authorization"), None);
        assert_eq!(header_of(req, "referer"), None);
    }
    assert_eq!(cdn_requests[0].url.query(), Some("sig=s3cr3t"));
    for req in requests(&origin).await {
        assert_eq!(header_of(&req, "authorization"), Some(AUTH));
    }
}

#[tokio::test]
async fn get_keeps_credentials_on_same_origin_redirects_and_caps_hops() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/dav/wisp/a"))
        .respond_with(ResponseTemplate::new(301).insert_header("Location", "/dav/wisp/b"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/dav/wisp/b"))
        .respond_with(ResponseTemplate::new(307).insert_header("Location", "c"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/dav/wisp/c"))
        .and(header("authorization", AUTH))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"ok".to_vec()))
        .mount(&server)
        .await;
    let dav = dav(&server, "/dav");
    assert_eq!(dav.get("a").await.unwrap().as_deref(), Some(&b"ok"[..]));
    assert_eq!(requests(&server).await.len(), 3);

    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/dav/wisp/loop"))
        .respond_with(ResponseTemplate::new(302).insert_header("Location", "/dav/wisp/loop"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/dav/wisp/nowhere"))
        .respond_with(ResponseTemplate::new(302))
        .mount(&server)
        .await;
    assert_eq!(dav.get("loop").await, Err(RemoteError::Redirect));
    // 最初的请求加上最多 3 次跟随
    assert_eq!(requests(&server).await.len(), 4);
    assert_eq!(dav.get("nowhere").await, Err(RemoteError::Redirect));
}

#[tokio::test]
async fn http_redirects_to_other_hosts_or_https_are_refused() {
    let origin = MockServer::start().await;
    let other = MockServer::start().await;
    let other_port = other.address().port();
    let origin_port = origin.address().port();
    Mock::given(method("GET"))
        .and(path("/dav/wisp/cross"))
        .respond_with(ResponseTemplate::new(302).insert_header("Location", format!("http://localhost:{other_port}/x")))
        .mount(&origin)
        .await;
    Mock::given(method("GET"))
        .and(path("/dav/wisp/upgrade"))
        .respond_with(
            ResponseTemplate::new(301)
                .insert_header("Location", format!("https://127.0.0.1:{origin_port}/dav/wisp/upgrade")),
        )
        .mount(&origin)
        .await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&other)
        .await;

    let dav = dav(&origin, "/dav");
    assert_eq!(dav.get("cross").await, Err(RemoteError::Redirect));
    let upgrade = dav.get("upgrade").await.unwrap_err();
    assert_eq!(upgrade, RemoteError::HttpsRequired);
    assert_eq!(upgrade.kind(), ErrorKind::HttpsRequired);
    // 没有带着凭据去 https 重试
    assert_eq!(requests(&origin).await.len(), 2);
}

/// 非 GET 遇到 3xx 不跟随（请求体不会被重发到别处，PUT 也不会被改成 GET）
#[tokio::test]
async fn non_get_methods_do_not_follow_redirects() {
    let origin = MockServer::start().await;
    let elsewhere = MockServer::start().await;
    let to_elsewhere = format!("{}/x", elsewhere.uri());
    let port = origin.address().port();
    let upgrade = format!("https://127.0.0.1:{port}/dav/wisp/f");
    Mock::given(method("PUT"))
        .and(path("/dav/wisp/f302"))
        .respond_with(ResponseTemplate::new(302).insert_header("Location", to_elsewhere.as_str()))
        .mount(&origin)
        .await;
    Mock::given(method("PUT"))
        .and(path("/dav/wisp/f307"))
        .respond_with(ResponseTemplate::new(307).insert_header("Location", "/dav/wisp/other"))
        .mount(&origin)
        .await;
    Mock::given(method("PUT"))
        .and(path("/dav/wisp/f303"))
        .respond_with(ResponseTemplate::new(303).insert_header("Location", "/dav/wisp/other"))
        .mount(&origin)
        .await;
    Mock::given(method("PUT"))
        .and(path("/dav/wisp/https"))
        .respond_with(ResponseTemplate::new(301).insert_header("Location", upgrade.as_str()))
        .mount(&origin)
        .await;
    Mock::given(method("PROPFIND"))
        .respond_with(ResponseTemplate::new(301).insert_header("Location", "/dav/wisp/elsewhere/"))
        .mount(&origin)
        .await;
    Mock::given(method("MKCOL"))
        .respond_with(ResponseTemplate::new(301).insert_header("Location", upgrade.as_str()))
        .mount(&origin)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(302).insert_header("Location", to_elsewhere.as_str()))
        .mount(&origin)
        .await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&elsewhere)
        .await;

    let dav = dav(&origin, "/dav");
    let body = || Bytes::from_static(b"payload");
    assert_eq!(dav.put("f302", body()).await, Err(RemoteError::Redirect));
    assert_eq!(dav.put("f307", body()).await, Err(RemoteError::Redirect));
    assert_eq!(dav.put("f303", body()).await, Err(RemoteError::Redirect));
    assert_eq!(dav.put("https", body()).await, Err(RemoteError::HttpsRequired));
    assert_eq!(dav.list("").await, Err(RemoteError::Redirect));
    assert_eq!(dav.mkcol("a").await, Err(RemoteError::HttpsRequired));
    assert_eq!(dav.delete("f").await, Err(RemoteError::Redirect));

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("f");
    std::fs::write(&src, b"payload").unwrap();
    assert_eq!(dav.put_file("f302", &src, 7).await, Err(RemoteError::Redirect));

    // 每个调用只发了一个请求，没有任何请求跟到别处（同源的 /dav/wisp/other 也没有）
    let seen = requests(&origin).await;
    assert_eq!(seen.len(), 8);
    assert!(seen.iter().all(|r| r.url.path() != "/dav/wisp/other"));
    assert!(RemoteError::Redirect.kind() == ErrorKind::Server && RemoteError::Redirect.detail() == "redirect");
}

// ---------------------------------------------------------------------------------------------
// 状态码与传输错误

#[tokio::test]
async fn status_codes_map_to_error_kinds() {
    let cases: [(u16, ErrorKind, bool); 11] = [
        (401, ErrorKind::Auth, false),
        (403, ErrorKind::Forbidden, false),
        (409, ErrorKind::NotFound, false),
        (413, ErrorKind::Quota, false),
        (507, ErrorKind::Quota, false),
        (423, ErrorKind::Server, true),
        (429, ErrorKind::Server, true),
        (500, ErrorKind::Server, true),
        (502, ErrorKind::Server, true),
        (503, ErrorKind::Server, true),
        (400, ErrorKind::Server, true),
    ];
    let server = MockServer::start().await;
    let dav = dav(&server, "/dav");
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    std::fs::write(&src, b"abc").unwrap();
    for (code, kind, transient) in cases {
        server.reset().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(code).set_body_string("<error>with details</error>"))
            .mount(&server)
            .await;
        let err = RemoteError::Status(code);
        assert_eq!(dav.list("d").await.unwrap_err(), err, "list {code}");
        assert_eq!(dav.get("f").await.unwrap_err(), err, "get {code}");
        assert_eq!(dav.get_to_file("f", &tmp.path().join("dest")).await.unwrap_err(), err);
        assert!(!tmp.path().join("dest").exists());
        assert_eq!(
            dav.put("f", Bytes::from_static(b"x")).await.unwrap_err(),
            err,
            "put {code}"
        );
        assert_eq!(dav.put_file("f", &src, 3).await.unwrap_err(), err, "put_file {code}");
        assert_eq!(dav.delete("f").await.unwrap_err(), err, "delete {code}");
        assert_eq!(dav.mkcol("d").await.unwrap_err(), err, "mkcol {code}");
        assert_eq!(err.kind(), kind, "{code}");
        assert_eq!(err.is_transient(), transient, "{code}");
        assert_eq!(err.detail(), format!("http {code}"));
    }
}

#[tokio::test]
async fn transport_errors_use_fixed_vocabulary() {
    // 绑定后立即释放，得到一个大概率没有人监听的端口
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let dav = WebDav::new(&format!("http://127.0.0.1:{port}/secret-path"), "someone", "hunter2").unwrap();
    let err = tokio::time::timeout(Duration::from_secs(20), dav.list(""))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(err, RemoteError::Connect);
    assert_eq!(err.kind(), ErrorKind::Network);
    assert!(err.is_transient());
    for text in [err.to_string(), err.detail(), format!("{err:?}"), format!("{dav:?}")] {
        for secret in ["127.0.0.1", "secret-path", "someone", "hunter2", &port.to_string()] {
            assert!(!text.contains(secret), "{text:?} leaks {secret}");
        }
    }
}
