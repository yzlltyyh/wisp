//! 对真实 WebDAV 服务器的测试共用的工具：从环境变量读账号、本次测试独占的测试目录（建立、存在检查、删除），以及
//! 「在隔离的任务里跑场景、无论结果如何都清理」的外壳。
//!
//! 账号只从 `WISP_SYNC_TEST_URL` / `WISP_SYNC_TEST_USER` / `WISP_SYNC_TEST_PASS` 读取；失败信息只含固定词汇与 HTTP 状态码，
//! 不含地址、账号与内容。
#![allow(dead_code)] // 各测试文件只用到其中一部分

use std::any::Any;
use std::future::Future;
use std::time::Duration;

use reqwest::StatusCode;

/// 只取 `resourcetype`：判断目录是否存在用不到别的属性
const PROPFIND_BODY: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/></d:prop></d:propfind>"#,
);

#[derive(Clone)]
pub struct Account {
    pub url: String,
    pub user: String,
    pub pass: String,
}

impl Account {
    pub fn from_env() -> Account {
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("需要环境变量 {name}"));
        Account {
            url: var("WISP_SYNC_TEST_URL"),
            user: var("WISP_SYNC_TEST_USER"),
            pass: var("WISP_SYNC_TEST_PASS"),
        }
    }
}

pub enum Failure {
    Panic(Box<dyn Any + Send>),
    Message(String),
}

impl Failure {
    pub fn message(&self) -> String {
        match self {
            Failure::Panic(payload) => payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
                .unwrap_or_else(|| "场景失败".into()),
            Failure::Message(m) => m.clone(),
        }
    }
}

/// 建立测试目录 → 在单独的任务里跑 `body`（参数是以测试目录为地址的服务器地址）→ 不论结果如何都删除测试目录。
/// 断言失败（panic）与超过 `limit` 都在任务里收住，清理之后再报告；场景的失败优先报告
pub async fn with_scratch<F, Fut>(account: &Account, limit: Duration, body: F)
where
    F: FnOnce(String) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let id = wisp_sync::crypto::random_id().expect("随机 id");
    let scratch = Scratch::new(account, &id);
    let outcome = match scratch.create().await {
        Mkcol::Created => run_isolated(limit, body(scratch.server_url())).await,
        // 同名目录早已存在：不是本测试建的，既不在里面跑，也不删除
        Mkcol::Exists => panic!("测试目录已存在，未做任何改动"),
        // 请求可能已经到达服务器，仍按已建立处理，照常清理
        Mkcol::Failed(e) => Err(Failure::Message(format!("建立测试目录失败：{e}"))),
    };
    let cleanup = scratch.remove().await;
    match (outcome, cleanup) {
        (Ok(()), Ok(())) => {}
        (Ok(()), Err(e)) => panic!("清理测试目录失败：{e}"),
        (Err(Failure::Panic(payload)), Ok(())) => std::panic::resume_unwind(payload),
        (Err(failure), Ok(())) => panic!("{}", failure.message()),
        (Err(failure), Err(e)) => panic!("{}（清理测试目录也失败了：{e}）", failure.message()),
    }
}

/// 在单独的任务里跑场景：断言失败（panic）与超时都在这里收住，调用方随后照常清理
pub async fn run_isolated(limit: Duration, scenario: impl Future<Output = ()> + Send + 'static) -> Result<(), Failure> {
    let task = tokio::spawn(tokio::time::timeout(limit, scenario));
    match task.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(Failure::Message(format!(
            "场景超过 {} 分钟仍未结束",
            limit.as_secs() / 60
        ))),
        Err(e) if e.is_panic() => Err(Failure::Panic(e.into_panic())),
        Err(_) => Err(Failure::Message("场景任务被取消".into())),
    }
}

pub enum Mkcol {
    Created,
    Exists,
    Failed(String),
}

/// 本次测试独占的目录 `e2e-<id>/`，在账号地址之下、与真正的同步根目录 `wisp/` 并列
pub struct Scratch {
    client: reqwest::Client,
    url: url::Url,
    user: String,
    pass: String,
}

impl Scratch {
    pub fn new(account: &Account, id: &str) -> Scratch {
        let base = wisp_sync::validate_server_url(&account.url).expect("WISP_SYNC_TEST_URL 不是可用的服务器地址");
        let url = base.join(&format!("e2e-{id}/")).expect("测试目录地址");
        // reqwest 用的是不带默认加密实现的 rustls：WebDav 建客户端前会装上 ring，这里可能先于它，所以自己兜底
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::ring::default_provider().install_default();
        }
        let client = reqwest::Client::builder()
            // 不跟随重定向：凭据只发给用户填写的服务器
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(60))
            .build()
            .expect("HTTP 客户端");
        Scratch {
            client,
            url,
            user: account.user.clone(),
            pass: account.pass.clone(),
        }
    }

    /// 引擎用的服务器地址：同步根目录因此落在 `e2e-<id>/wisp/`
    pub fn server_url(&self) -> String {
        self.url.to_string()
    }

    async fn request(&self, method: &str, propfind: bool) -> Result<(StatusCode, String), String> {
        let m = reqwest::Method::from_bytes(method.as_bytes()).expect("请求方法");
        let mut req = self
            .client
            .request(m, self.url.clone())
            .basic_auth(&self.user, Some(&self.pass));
        if propfind {
            req = req
                .header("Depth", "0")
                .header(reqwest::header::CONTENT_TYPE, "application/xml; charset=utf-8")
                .body(PROPFIND_BODY);
        }
        // 只报告错误种类：reqwest 的错误文本里带地址
        let resp = req.send().await.map_err(|e| {
            let kind = if e.is_timeout() {
                "timeout"
            } else if e.is_connect() {
                "connect"
            } else {
                "request"
            };
            format!("{method}: {kind}")
        })?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Ok((status, body))
    }

    pub async fn create(&self) -> Mkcol {
        match self.request("MKCOL", false).await {
            Ok((s, _)) if s.is_success() => Mkcol::Created,
            Ok((StatusCode::METHOD_NOT_ALLOWED, _)) => Mkcol::Exists,
            Ok((s, _)) => Mkcol::Failed(format!("MKCOL: http {}", s.as_u16())),
            Err(e) => Mkcol::Failed(e),
        }
    }

    /// `Depth: 0` 的 PROPFIND。有的服务器对不存在的资源回 207、在多状态里报 404，
    /// 所以 207 时还要看有没有成功（200）的属性
    pub async fn exists(&self) -> Result<bool, String> {
        let (status, body) = self.request("PROPFIND", true).await?;
        match status.as_u16() {
            404 | 410 => Ok(false),
            207 => Ok(body.split("status>").any(|s| {
                let mut words = s.split_whitespace();
                words.next().is_some_and(|w| w.starts_with("HTTP/")) && words.next() == Some("200")
            })),
            s => Err(format!("PROPFIND: http {s}")),
        }
    }

    /// 删除整个测试目录并等它消失。有的服务器 DELETE 先返回成功、约一秒后才真正删除，期间仍能列出，
    /// 所以最多等约 10 秒
    pub async fn remove(&self) -> Result<(), String> {
        let (status, _) = self.request("DELETE", false).await?;
        let accepted = (status.is_success() && status != StatusCode::MULTI_STATUS)
            || status == StatusCode::NOT_FOUND
            || status == StatusCode::GONE;
        if !accepted {
            return Err(format!("DELETE: http {}", status.as_u16()));
        }
        let mut last = Ok(true);
        for _ in 0..20 {
            last = self.exists().await;
            if last == Ok(false) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        match last {
            Err(e) => Err(format!("删除后检查测试目录失败：{e}")),
            _ => Err("删除后约 10 秒仍能列出测试目录".into()),
        }
    }
}
