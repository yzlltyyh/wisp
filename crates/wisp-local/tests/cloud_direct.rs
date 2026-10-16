//! PaddleOCR 客户端访问本机 / 局域网的自托管服务时直连、不走系统代理。
//!
//! 进程的代理环境变量指向一个不存在的代理：走代理的请求会失败，直连的请求能到达本机的模拟服务器。环境变量是进程级的，
//! 所以这个测试单独成一个测试二进制，并且只有一个用例。

use std::sync::Arc;

use serde_json::json;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use wisp_local::ocr::cloud::{CloudClient, CloudOutput, CloudRequest};
use wisp_types::OcrMode;

#[test]
fn loopback_services_bypass_the_system_proxy() {
    // SAFETY: 这个测试二进制只有这一个用例，此时还没有启动运行时或任何别的线程，没有并发读写环境变量的代码
    unsafe {
        for k in ["NO_PROXY", "no_proxy"] {
            std::env::remove_var(k);
        }
        // 端口 9（discard）在本机上通常没有监听：经代理的请求一定失败
        for k in [
            "ALL_PROXY",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "all_proxy",
            "http_proxy",
            "https_proxy",
        ] {
            std::env::set_var(k, "http://127.0.0.1:9");
        }
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/ocr"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errorCode": 0,
                "errorMsg": "Success",
                "result": {"ocrResults": [{"prunedResult": {
                    "rec_texts": ["直连"],
                    "rec_scores": [0.99],
                    "rec_polys": [[[0, 0], [40, 0], [40, 20], [0, 20]]]
                }}]}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let req = CloudRequest {
            endpoint: format!("{}/ocr", server.uri()),
            token: None,
            model: "PP-OCRv6".to_owned(),
            image: Arc::new(b"\x89PNG fake image bytes".to_vec()),
            mime: "image/png",
        };
        let client = CloudClient::new().expect("client");
        let out = client
            .recognize(req, &|| {}, CancellationToken::new())
            .await
            .expect("loopback goes direct");
        assert!(matches!(&out, CloudOutput::Lines(l) if l.len() == 1));
        assert_eq!(out.text(OcrMode::Lines), "直连");
    });
}
