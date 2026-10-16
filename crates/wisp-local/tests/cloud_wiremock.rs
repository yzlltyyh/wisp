//! PaddleOCR 云端客户端：任务式与 serving 式接口的成功、排队、失败、鉴权、额度、字段类型宽松、多行 JSONL。
//! 响应体按实测结构手写的精简版（地址、jobId、内容都是虚构的）。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::json;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};
use wisp_local::ocr::cloud::{CloudClient, CloudOutput, CloudRequest, Token};
use wisp_types::{ErrorCode, OcrMode};

const JOBS: &str = "/api/v2/ocr/jobs";

fn request(server: &MockServer, path: &str, model: &str, token: Option<&str>) -> CloudRequest {
    CloudRequest {
        endpoint: format!("{}{path}", server.uri()),
        token: token.and_then(|t| Token::new(t.to_owned())),
        model: model.to_owned(),
        image: Arc::new(b"\x89PNG fake image bytes".to_vec()),
        mime: "image/png",
    }
}

async fn recognize(req: CloudRequest) -> (Result<CloudOutput, wisp_types::LlmError>, usize) {
    let queued = Arc::new(AtomicUsize::new(0));
    let q = Arc::clone(&queued);
    let on_queued = move || {
        q.fetch_add(1, Ordering::SeqCst);
    };
    let client = CloudClient::new().expect("client");
    let out = client.recognize(req, &on_queued, CancellationToken::new()).await;
    (out, queued.load(Ordering::SeqCst))
}

/// 请求体（原始字节：multipart 里有图片的二进制，不是合法的 UTF-8）里含有这段文字
fn body_has(needle: &'static str) -> impl Fn(&Request) -> bool {
    move |req: &Request| req.body.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// 下载结果的请求不能带鉴权头
fn no_auth(req: &Request) -> bool {
    !req.headers.contains_key("authorization")
}

fn ocr_line(texts: &[&str]) -> String {
    let polys: Vec<_> = (0..texts.len())
        .map(|i| {
            let y = 10 + 30 * i as i64;
            json!([[10, y], [300, y], [300, y + 20], [10, y + 20]])
        })
        .collect();
    json!({
        "logId": "log-1",
        "result": {
            "ocrResults": [{"prunedResult": {
                "rec_texts": texts,
                "rec_scores": texts.iter().map(|_| "0.98").collect::<Vec<_>>(),
                "rec_polys": polys,
                "text_det_params": {"limit_side_len": 960, "limit_type": "max"},
            }}],
            "dataInfo": {"width": 400, "height": 200, "type": "image"},
        },
        "errorCode": 0,
        "errorMsg": "Success",
    })
    .to_string()
}

/// 挂上一个任务的查询与结果下载；`submit` 时连同一个不挑请求内容的提交接口
async fn mount_job(server: &MockServer, submit: bool, job_id: serde_json::Value, pending: u64, jsonl: String) {
    if submit {
        Mock::given(method("POST"))
            .and(path(JOBS))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"traceId": "t", "code": 0, "msg": "Success", "data": {"jobId": job_id}})),
            )
            .expect(1)
            .mount(server)
            .await;
    }
    let id = match &job_id {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let poll = format!("{JOBS}/{id}");
    if pending > 0 {
        Mock::given(method("GET"))
            .and(path(poll.clone()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0, "data": {"jobId": id, "state": "running", "extractProgress": {"totalPages": "1", "extractedPages": 0}}})))
            .up_to_n_times(pending)
            .mount(server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path(poll))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": "0",
            "msg": "Success",
            "data": {"jobId": id, "state": "done", "resultUrl": {"jsonUrl": format!("{}/store/result.jsonl", server.uri())}},
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/store/result.jsonl"))
        .and(no_auth)
        .respond_with(ResponseTemplate::new(200).set_body_string(jsonl))
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn jobs_success_after_queueing() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(JOBS))
        .and(header("authorization", "bearer secret-token"))
        .and(body_has("name=\"model\"\r\n\r\nPP-OCRv6\r\n"))
        .and(body_has("\"useTextlineOrientation\":false"))
        .and(body_has("\"visualize\":false"))
        .and(body_has("filename=\"capture.png\"\r\nContent-Type: image/png"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0, "data": {"jobId": "job-1"}})))
        .expect(1)
        .mount(&server)
        .await;
    mount_job(&server, false, json!("job-1"), 2, ocr_line(&["Hello Wisp", "离线识别"])).await;
    let (out, queued) = recognize(request(&server, JOBS, "PP-OCRv6", Some("secret-token"))).await;
    let out = out.expect("ok");
    assert_eq!(queued, 1, "提交成功后提示一次「排队中」");
    assert_eq!(out.text(OcrMode::Lines), "Hello Wisp\n离线识别");
}

#[tokio::test]
async fn jobs_document_with_multi_line_jsonl_and_numeric_job_id() {
    let server = MockServer::start().await;
    let page = |title: &str, body: &str| {
        json!({
            "result": {"layoutParsingResults": [{
                "markdown": {"text": format!("## {title}\n\n{body}\n\n<div style=\"text-align: center;\"><img src=\"imgs/img_in_image_box_1_1_9_9.jpg\" alt=\"Image\" width=\"14%\" /></div>"), "images": {"imgs/img_in_image_box_1_1_9_9.jpg": "https://store.invalid/x"}},
                "prunedResult": {"parsing_res_list": [
                    {"block_label": "header_image", "block_content": "", "block_order": null},
                    {"block_label": "paragraph_title", "block_content": title, "block_order": 1},
                    {"block_label": "text", "block_content": body, "block_order": "2"},
                    {"block_label": "image", "block_content": "", "block_order": null},
                ]},
            }]},
            "errorCode": 0,
        })
        .to_string()
    };
    let jsonl = format!("{}\n{}\n", page("第一页", "正文一"), page("Page two", "Body two"));
    mount_job(&server, true, json!(104_436_058_182_316_032_u64), 0, jsonl).await;
    let (out, _) = recognize(request(&server, JOBS, "PaddleOCR-VL-1.6", Some("t"))).await;
    let out = out.expect("ok");
    assert_eq!(
        out.text(OcrMode::Markdown),
        "## 第一页\n\n正文一\n\n## Page two\n\nBody two"
    );
    assert_eq!(
        out.text(OcrMode::Paragraphs),
        "第一页\n\n正文一\n\nPage two\n\nBody two"
    );
}

#[tokio::test]
async fn jobs_document_models_turn_off_charts_and_seals() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(JOBS))
        .and(body_has("\"useChartRecognition\":false"))
        .and(body_has("\"useSealRecognition\":false"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0, "data": {"jobId": "j"}})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{JOBS}/j")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"code": 0, "data": {"state": "failed", "errorMsg": "文件格式不支持"}})),
        )
        .mount(&server)
        .await;
    let (out, queued) = recognize(request(&server, JOBS, "PP-StructureV3", Some("t"))).await;
    let err = out.expect_err("failed job");
    assert_eq!(queued, 1);
    assert_eq!(err.code, ErrorCode::Upstream);
    assert!(!err.message.contains("文件格式"), "不回显服务端原文");
}

#[tokio::test]
async fn jobs_auth_and_quota_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(JOBS))
        .and(header("authorization", "bearer bad"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"code": 401, "msg": "Token 无效"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(JOBS))
        .and(header("authorization", "bearer spent"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"code": 12001, "msg": "已达每日页数上限"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(JOBS))
        .and(header("authorization", "bearer busy"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({"code": 12002, "msg": "请求频率过高"})))
        .mount(&server)
        .await;
    let (out, queued) = recognize(request(&server, JOBS, "PP-OCRv6", Some("bad"))).await;
    let err = out.expect_err("auth");
    assert_eq!((err.code, err.status, queued), (ErrorCode::Auth, Some(401), 0));
    assert_eq!(err.hint.as_deref(), Some("errors.hint.check_key"));
    let (out, _) = recognize(request(&server, JOBS, "PP-OCRv6", Some("spent"))).await;
    assert_eq!(out.expect_err("quota").code, ErrorCode::Quota);
    let (out, _) = recognize(request(&server, JOBS, "PP-OCRv6", Some("busy"))).await;
    let err = out.expect_err("rate");
    assert_eq!(err.code, ErrorCode::RateLimited);
    assert!(err.retryable);
}

#[tokio::test]
async fn jobs_bad_jsonl_lines_fail_the_whole_result() {
    let server = MockServer::start().await;
    mount_job(
        &server,
        true,
        json!("j2"),
        0,
        format!("{}\nnot json\n", ocr_line(&["a"])),
    )
    .await;
    let (out, _) = recognize(request(&server, JOBS, "PP-OCRv6", Some("t"))).await;
    assert_eq!(out.expect_err("bad line").code, ErrorCode::Protocol);
}

#[tokio::test]
async fn jobs_cancellation_stops_polling() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(JOBS))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0, "data": {"jobId": "slow"}})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{JOBS}/slow")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0, "data": {"state": "pending"}})))
        .mount(&server)
        .await;
    let client = CloudClient::new().expect("client");
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        c.cancel();
    });
    let t0 = std::time::Instant::now();
    let err = client
        .recognize(request(&server, JOBS, "PP-OCRv6", Some("t")), &|| {}, cancel)
        .await
        .expect_err("cancelled");
    assert_eq!(err.code, ErrorCode::Cancelled);
    assert!(t0.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn serving_ocr_and_layout_parsing() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/ocr"))
        .and(header("authorization", "token app-token"))
        .and(body_has("\"fileType\":1"))
        .and(body_has("\"file\":\"iVBORyBmYWtlIGltYWdlIGJ5dGVz\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "logId": "x",
            "errorCode": "0",
            "errorMsg": "Success",
            "result": serde_json::from_str::<serde_json::Value>(&ocr_line(&["serving"])).expect("json")["result"].clone(),
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/layout-parsing"))
        .and(no_auth)
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "errorCode": 0,
            "result": {"layoutParsingResults": [{
                "markdown": {"text": "# 文档\n\n<table><tr><td>a</td><td>b</td></tr></table>"},
                "prunedResult": {"parsing_res_list": [
                    {"block_label": "doc_title", "block_content": "文档", "block_order": 1},
                    {"block_label": "table", "block_content": "<table><tr><td>a</td><td>b</td></tr></table>", "block_order": null},
                ]},
            }]},
        })))
        .mount(&server)
        .await;
    let (out, queued) = recognize(request(&server, "/ocr", "ignored", Some("app-token"))).await;
    assert_eq!(out.expect("ocr").text(OcrMode::Paragraphs), "serving");
    assert_eq!(queued, 0, "serving 式没有排队阶段");
    let (out, _) = recognize(request(&server, "/layout-parsing", "", None)).await;
    let out = out.expect("layout");
    assert_eq!(out.text(OcrMode::Markdown), "# 文档\n\n| a | b |\n| --- | --- |");
    assert_eq!(out.text(OcrMode::Lines), "文档\n\na\tb");
}

#[tokio::test]
async fn serving_errors_accept_string_codes() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/ocr"))
        .respond_with(
            ResponseTemplate::new(503).set_body_json(json!({"errorCode": "503", "errorMsg": "Too many requests"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/quota/ocr"))
        .respond_with(
            ResponseTemplate::new(429).set_body_json(json!({"errorCode": 429, "errorMsg": "超出单日解析最大页数"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/limited/ocr"))
        .respond_with(
            ResponseTemplate::new(429).set_body_json(json!({"errorCode": 429, "errorMsg": "Too many requests"})),
        )
        .mount(&server)
        .await;
    let (out, _) = recognize(request(&server, "/ocr", "", None)).await;
    let err = out.expect_err("503");
    assert_eq!((err.code, err.retryable), (ErrorCode::Upstream, true));
    let (out, _) = recognize(request(&server, "/quota/ocr", "", None)).await;
    assert_eq!(out.expect_err("quota").code, ErrorCode::Quota);
    let (out, _) = recognize(request(&server, "/limited/ocr", "", None)).await;
    assert_eq!(out.expect_err("rate").code, ErrorCode::RateLimited);
}

#[tokio::test]
async fn connection_tests() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{JOBS}/wisp-connection-test")))
        .and(header("authorization", "bearer good"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"code": 11001, "msg": "jobId 不存在"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{JOBS}/wisp-connection-test")))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"code": 401})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/ocr"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({"errorCode": 0, "result": {"ocrResults": [{"prunedResult": {"rec_texts": []}}]}}),
            ),
        )
        .mount(&server)
        .await;
    let client = CloudClient::new().expect("client");
    let jobs = format!("{}{JOBS}", server.uri());
    client
        .test(&jobs, Token::new("good".into()))
        .await
        .expect("token accepted");
    let err = client
        .test(&jobs, Token::new("bad".into()))
        .await
        .expect_err("token rejected");
    assert_eq!(err.code, ErrorCode::Auth);
    client
        .test(&format!("{}/ocr", server.uri()), None)
        .await
        .expect("serving ok");
    let err = client
        .test("https://example.com/v1/chat", None)
        .await
        .expect_err("unknown address");
    assert_eq!(err.code, ErrorCode::NotConfigured);
    let err = client
        .test("http://example.com/ocr", None)
        .await
        .expect_err("plain http on a public host");
    assert_eq!(err.code, ErrorCode::NotConfigured);
}

#[tokio::test]
async fn oversized_uploads_are_refused_locally() {
    let server = MockServer::start().await;
    let mut req = request(&server, JOBS, "PP-OCRv6", Some("t"));
    req.image = Arc::new(vec![0; wisp_local::ocr::cloud::MAX_UPLOAD_BYTES + 1]);
    let (out, queued) = recognize(req).await;
    assert_eq!(
        (out.expect_err("too large").code, queued),
        (ErrorCode::ImageTooLarge, 0)
    );
    assert!(server.received_requests().await.unwrap_or_default().is_empty());
}
