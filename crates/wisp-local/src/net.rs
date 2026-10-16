//! 模型下载与 PaddleOCR 云端共用的 HTTP 客户端构造。客户端按需创建、用完即丢（平时不存在），读系统代理（与 wisp-llm
//! 一致；回环地址直连），不设 User-Agent 之外的任何默认请求头。

use std::time::Duration;

/// 连接超时
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// 基础的客户端构造器：rustls（ring）、连接超时、Wisp 的 User-Agent。调用方再按用途设重定向策略与读超时
pub fn client_builder() -> reqwest::ClientBuilder {
    // reqwest 用的是 rustls-no-provider：进程里还没有默认 CryptoProvider 时（测试、ORT 工作进程以外的子命令）兜底安装 ring；
    // desktop 启动时已经装过，这里只是一次原子读
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .user_agent(concat!("Wisp/", env!("CARGO_PKG_VERSION")))
}
