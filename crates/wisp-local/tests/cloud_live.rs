//! PaddleOCR 云端的真机测试（真实网络、真实 Token），手动运行：
//!
//! ```text
//! WISP_PADDLE_TEST_TOKEN=<token> cargo test -p wisp-local --test cloud_live -- --ignored --nocapture
//! ```
//!
//! 可选 `WISP_PADDLE_TEST_URL`（默认 AI Studio 的任务式地址）。只打印模型名、耗时、行数与字数，不打印识别出的内容。

// 真机测量的数据直接打印在终端
#![allow(clippy::print_stderr)]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use tokio_util::sync::CancellationToken;
use wisp_local::ocr::cloud::{CloudClient, CloudOutput, CloudRequest, DEFAULT_JOBS_URL, Token};
use wisp_types::{ErrorCode, OcrMode};

fn token() -> Option<Token> {
    Token::new(std::env::var("WISP_PADDLE_TEST_TOKEN").expect("WISP_PADDLE_TEST_TOKEN"))
}

fn endpoint() -> String {
    std::env::var("WISP_PADDLE_TEST_URL").unwrap_or_else(|_| DEFAULT_JOBS_URL.to_owned())
}

#[tokio::test]
#[ignore = "访问真实的 PaddleOCR 服务，需要 WISP_PADDLE_TEST_TOKEN"]
async fn three_models_on_a_screenshot() {
    let png = std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/images/translate-light.png"))
        .expect("image");
    let image = Arc::new(png);
    let client = CloudClient::new().expect("client");
    for model in ["PP-OCRv6", "PaddleOCR-VL-1.6", "PP-StructureV3"] {
        let queued = AtomicBool::new(false);
        let t0 = Instant::now();
        let req = CloudRequest {
            endpoint: endpoint(),
            token: token(),
            model: model.to_owned(),
            image: Arc::clone(&image),
            mime: "image/png",
        };
        let out = client
            .recognize(req, &|| queued.store(true, Ordering::SeqCst), CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{model}: {:?} {}", e.code, e.message));
        let shape = match &out {
            CloudOutput::Lines(l) => format!("{} 行", l.len()),
            CloudOutput::Document { markdown, blocks } => {
                assert!(!markdown.contains("<img"), "区块图片已去掉");
                format!("Markdown {} 字、{} 个文字块", markdown.chars().count(), blocks.len())
            }
        };
        let paragraphs = out.text(OcrMode::Paragraphs);
        eprintln!(
            "{model}: {} ms，排队提示 {}，{shape}，paragraphs {} 字",
            t0.elapsed().as_millis(),
            queued.load(Ordering::SeqCst),
            paragraphs.chars().count()
        );
        assert!(!paragraphs.trim().is_empty());
    }
}

#[tokio::test]
#[ignore = "访问真实的 PaddleOCR 服务，需要 WISP_PADDLE_TEST_TOKEN"]
async fn connection_test_accepts_the_token_and_rejects_a_wrong_one() {
    let client = CloudClient::new().expect("client");
    client.test(&endpoint(), token()).await.expect("token accepted");
    let err = client
        .test(
            &endpoint(),
            Token::new("0000000000000000000000000000000000000000".into()),
        )
        .await
        .expect_err("wrong token");
    assert_eq!(err.code, ErrorCode::Auth);
}
