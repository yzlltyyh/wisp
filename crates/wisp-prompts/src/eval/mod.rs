//! 评测运行器（feature `eval`，仅开发用，CI 不跑）：
//! `cargo run -p wisp-prompts --features eval --bin wisp-eval -- --provider-json <file> --styles all --out target/eval-runs/`（`--out` 省略时也写到这里）。
//! Key 从环境变量 `WISP_EVAL_KEY_<ID>` 读取（ID 为 provider id 的大写，非字母数字替换为 `_`）。
//!
//! 每个模型 × 每个风格 × S1–S16（S14 只跑 polish / proofread，S16 强制目标为 zh），视觉模型另跑 OCR 截图样本；
//! 输出（模型、风格、方向、原始输出、后处理后的输出、耗时、断言）写到 `<out>/<日期>-<provider>-<model>.{md,jsonl}`，
//! 自动断言汇总通过率，其余列为「待人工」勾选项。
//!
//! `--provider-json` 文件可以是：
//! - 一个 `Provider` 对象（与 settings.json 中 `providers[i]` 同形）：评测其中 `enabled` 的模型；
//! - `{"provider": Provider, "models": ["id", …]}`：只评测列出的模型（不在 provider.models 里的按默认能力新建）；
//! - 以上两种的数组，或 `{"providers": [ … ]}`。

pub mod checks;
pub mod samples;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use wisp_llm::{ChatRequest, ClientConfig, LlmClient, MemoryCapsStore, Purpose, Secret, UserContent};
use wisp_types::{DictMode, Lang, ModelEntry, OcrMode, Provider, StreamEvent, Style, Vision};

use self::checks::{Check, CheckKind, EvalTask, evaluate};
use self::samples::{OCR_SAMPLES, OcrSample, SAMPLES, Sample};
use crate::{
    BuiltPrompt, Detection, PostProcessor, PromptCtx, PromptTask, build, builtin_styles, detect_lang, target_for,
    use_dictionary,
};

/// 命令行参数
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalArgs {
    pub provider_json: PathBuf,
    /// "all" 或逗号分隔的风格 id
    pub styles: Vec<String>,
    pub out: PathBuf,
    /// 只跑这些样本（空 = 全部）
    pub samples: Vec<String>,
    /// OCR 截图样本目录（默认 `<out>/ocr-samples`）；目录里没有对应图片时跳过该样本
    pub ocr_dir: Option<PathBuf>,
    /// 并发请求数
    pub concurrency: usize,
    /// 只装配提示词并写出 `.dry-run.jsonl`，不发请求、不需要 Key
    pub dry_run: bool,
}

const USAGE: &str = "usage: wisp-eval --provider-json <file> [--styles all|natural,technical,…] [--out <dir>] \
[--samples all|S1,S4,…] [--ocr-dir <dir>] [--concurrency <n>] [--dry-run]";

/// 单个请求的硬上限（模型自身的超时由 wisp-llm 按 Provider 配置处理）
const CASE_TIMEOUT: Duration = Duration::from_secs(180);

/// 解析命令行参数（不含程序名）
pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<EvalArgs, String> {
    let mut provider_json = None;
    let mut styles = vec!["all".to_owned()];
    let mut out = PathBuf::from("target/eval-runs");
    let mut samples = Vec::new();
    let mut ocr_dir = None;
    let mut concurrency = 4usize;
    let mut dry_run = false;

    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_owned(), Some(v.to_owned())),
            _ => (arg.clone(), None),
        };
        let mut value = |name: &str| -> Result<String, String> {
            inline
                .clone()
                .or_else(|| it.next())
                .ok_or_else(|| format!("{name} requires a value\n{USAGE}"))
        };
        match flag.as_str() {
            "--provider-json" => provider_json = Some(PathBuf::from(value("--provider-json")?)),
            "--styles" => styles = split_list(&value("--styles")?),
            "--out" => out = PathBuf::from(value("--out")?),
            "--samples" => samples = split_list(&value("--samples")?),
            "--ocr-dir" => ocr_dir = Some(PathBuf::from(value("--ocr-dir")?)),
            "--concurrency" => {
                let v = value("--concurrency")?;
                concurrency = v
                    .parse()
                    .ok()
                    .filter(|n| (1..=32).contains(n))
                    .ok_or_else(|| format!("--concurrency must be an integer between 1 and 32, got {v:?}"))?;
            }
            "--dry-run" => dry_run = true,
            "-h" | "--help" => return Err(USAGE.to_owned()),
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    let provider_json = provider_json.ok_or_else(|| format!("--provider-json is required\n{USAGE}"))?;
    if styles.is_empty() {
        return Err(format!("--styles must not be empty\n{USAGE}"));
    }
    if samples.iter().any(|s| s.eq_ignore_ascii_case("all")) {
        samples.clear();
    }
    Ok(EvalArgs {
        provider_json,
        styles,
        out,
        samples,
        ocr_dir,
        concurrency,
        dry_run,
    })
}

fn split_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect()
}

/// 一个待评测的 Provider 与模型
#[derive(Debug, Clone)]
struct ProviderSpec {
    provider: Provider,
    models: Vec<ModelEntry>,
}

fn load_providers(path: &Path) -> Result<Vec<ProviderSpec>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let value: Value = serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?;
    let items = match value {
        Value::Array(items) => items,
        Value::Object(mut map) if map.contains_key("providers") => match map.remove("providers") {
            Some(Value::Array(items)) => items,
            _ => return Err("\"providers\" must be an array".to_owned()),
        },
        other => vec![other],
    };
    let specs = items.into_iter().map(parse_spec).collect::<Result<Vec<_>, _>>()?;
    if specs.is_empty() {
        return Err("no provider in provider json".to_owned());
    }
    Ok(specs)
}

fn parse_spec(value: Value) -> Result<ProviderSpec, String> {
    let (provider_value, model_ids) = match value {
        Value::Object(mut map) if map.contains_key("provider") => {
            let ids = match map.remove("models") {
                None => None,
                Some(v) => Some(serde_json::from_value::<Vec<String>>(v).map_err(|e| format!("models: {e}"))?),
            };
            (map.remove("provider").unwrap_or(Value::Null), ids)
        }
        other => (other, None),
    };
    let provider: Provider = serde_json::from_value(provider_value).map_err(|e| format!("provider: {e}"))?;
    let models: Vec<ModelEntry> = match model_ids {
        Some(ids) => ids
            .into_iter()
            .map(|id| {
                provider
                    .models
                    .iter()
                    .find(|m| m.id == id)
                    .cloned()
                    .unwrap_or_else(|| ModelEntry::new(id))
            })
            .collect(),
        None => provider.models.iter().filter(|m| m.enabled).cloned().collect(),
    };
    if models.is_empty() {
        return Err(format!("provider {:?} has no model to evaluate", provider.id));
    }
    Ok(ProviderSpec { provider, models })
}

/// `WISP_EVAL_KEY_<ID>`
fn key_env_name(provider_id: &str) -> String {
    let id: String = provider_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("WISP_EVAL_KEY_{id}")
}

fn select_styles(names: &[String]) -> Result<Vec<Style>, String> {
    let all = builtin_styles();
    if names.iter().any(|n| n.eq_ignore_ascii_case("all")) {
        return Ok(all);
    }
    names
        .iter()
        .map(|n| {
            all.iter()
                .find(|s| s.id == *n)
                .cloned()
                .ok_or_else(|| format!("unknown style {n:?}"))
        })
        .collect()
}

fn select_samples(ids: &[String]) -> Result<Vec<Sample>, String> {
    if ids.is_empty() {
        return Ok(SAMPLES.to_vec());
    }
    ids.iter()
        .map(|id| samples::sample_by_id(id).ok_or_else(|| format!("unknown sample {id:?}")))
        .collect()
}

/// 评测用例
#[derive(Debug, Clone)]
struct Case {
    sample_id: &'static str,
    expect: &'static str,
    source: String,
    /// 风格 id、polish、proofread 或 ocr:<mode>
    task: String,
    dict: bool,
    detection: Detection,
    target: Lang,
    prompt: BuiltPrompt,
    image: Option<Arc<Vec<u8>>>,
}

impl Case {
    fn eval_task(&self) -> Option<EvalTask<'_>> {
        match self.task.as_str() {
            "polish" => Some(EvalTask::Polish),
            "proofread" => Some(EvalTask::Proofread),
            t if t.starts_with("ocr:") => None,
            id => Some(EvalTask::Style { id, dict: self.dict }),
        }
    }
}

fn plan_text_cases(styles: &[Style], samples: &[Sample], reasoning: bool) -> Vec<Case> {
    let mut cases = Vec::new();
    for s in samples {
        let detection = detect_lang(s.text);
        let target = s
            .forced_target
            .unwrap_or_else(|| target_for(&detection, Lang::ZhCn, Lang::En));
        let ctx = PromptCtx {
            text: s.text,
            source: detection,
            target,
            glossary: &[],
            reasoning_model: reasoning,
        };
        let mut push = |task: String, dict: bool, prompt: BuiltPrompt| {
            cases.push(Case {
                sample_id: s.id,
                expect: s.expect,
                source: s.text.to_owned(),
                task,
                dict,
                detection,
                target,
                prompt,
                image: None,
            });
        };
        if s.edit_only {
            push("polish".to_owned(), false, build(&PromptTask::Polish, &ctx));
            push("proofread".to_owned(), false, build(&PromptTask::Proofread, &ctx));
            continue;
        }
        for style in styles {
            let dict = use_dictionary(s.text, style, DictMode::Auto);
            push(
                style.id.clone(),
                dict,
                build(&PromptTask::Translate { style, dict }, &ctx),
            );
        }
    }
    cases
}

/// OCR 截图的长边上限：与正式请求一致，超过时等比缩小
const OCR_MAX_LONG_EDGE: u32 = 2048;
/// PNG 文件签名
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n'];

/// 准备 OCR 截图：校验 PNG 签名并解码，长边超过 2048 时等比缩小（Lanczos3）后重新编码为 PNG，
/// 保证评测请求与正式请求的图片尺寸一致
fn prepare_ocr_png(bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    if !bytes.starts_with(&PNG_SIGNATURE) {
        return Err("not a PNG file".to_owned());
    }
    let img =
        image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).map_err(|e| format!("decode PNG: {e}"))?;
    let (w, h) = (img.width(), img.height());
    let long = w.max(h);
    if long == 0 {
        return Err("empty image".to_owned());
    }
    if long <= OCR_MAX_LONG_EDGE {
        return Ok(bytes);
    }
    // 四舍五入的整数缩放：4000×1000 → 2048×512
    let fit = |d: u32| {
        let scaled = (u64::from(d) * u64::from(OCR_MAX_LONG_EDGE) + u64::from(long) / 2) / u64::from(long);
        u32::try_from(scaled.max(1)).unwrap_or(OCR_MAX_LONG_EDGE)
    };
    let resized = img.resize_exact(fit(w), fit(h), image::imageops::FilterType::Lanczos3);
    let mut out = Vec::new();
    resized
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .map_err(|e| format!("encode PNG: {e}"))?;
    Ok(out)
}

/// OCR 用例：缺少的截图记入 missing（报告里注明跳过）；不是 PNG 或无法解码时报错（退出码非零）
fn plan_ocr_cases(dir: &Path) -> Result<(Vec<Case>, Vec<String>), String> {
    let mut cases = Vec::new();
    let mut missing = Vec::new();
    let detection = Detection {
        lang: Lang::Other,
        low_confidence: true,
    };
    for o in OCR_SAMPLES {
        let path = dir.join(o.file);
        let Ok(bytes) = std::fs::read(&path) else {
            missing.push(format!("{} ({})", o.id, path.display()));
            continue;
        };
        let bytes = prepare_ocr_png(bytes).map_err(|e| format!("OCR sample {}: {e}", path.display()))?;
        let target = Lang::ZhCn;
        let ctx = PromptCtx {
            text: "",
            source: detection,
            target,
            glossary: &[],
            reasoning_model: false,
        };
        cases.push(Case {
            sample_id: o.id,
            expect: o.expect,
            source: String::new(),
            task: format!("ocr:{}", ocr_mode_name(o.mode)),
            dict: false,
            detection,
            target,
            prompt: build(&PromptTask::Ocr { mode: o.mode }, &ctx),
            image: Some(Arc::new(bytes)),
        });
    }
    Ok((cases, missing))
}

fn ocr_mode_name(mode: OcrMode) -> &'static str {
    match mode {
        OcrMode::Lines => "lines",
        OcrMode::Paragraphs => "paragraphs",
        OcrMode::Markdown => "markdown",
        OcrMode::Translate => "translate",
    }
}

/// OCR 样本的断言（`raw` 为模型原始输出，段落模式用来检查模型自己的排版）
fn evaluate_ocr(sample: &OcrSample, output: &str, raw: &str, no_text: bool) -> Vec<Check> {
    let check = |name: &str, kind: CheckKind, passed: Option<bool>| Check {
        name: name.to_owned(),
        kind,
        passed,
    };
    let out = output.trim();
    let mut v = Vec::new();
    if sample.file == "icon.png" {
        v.push(check("判为「未检测到文字」", CheckKind::Auto, Some(no_text)));
        return v;
    }
    v.push(check("有文字", CheckKind::Auto, Some(!no_text && !out.is_empty())));
    match sample.id {
        "O1-terminal-lines" => v.push(check("多行输出", CheckKind::Aux, Some(out.lines().count() >= 2))),
        "O2-table-markdown" => v.push(check(
            "含 Markdown 表格分隔行",
            CheckKind::Auto,
            Some(out.lines().any(|l| l.contains("|") && l.contains("---"))),
        )),
        "O3-formula-markdown" => v.push(check("含 $ 公式", CheckKind::Auto, Some(out.contains('$')))),
        "O3-formula-translate" => v.push(check(
            "输出含中文",
            CheckKind::Auto,
            Some(out.chars().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c))),
        )),
        _ => {}
    }
    if sample.mode == OcrMode::Paragraphs {
        v.extend(checks::evaluate_ocr_paragraphs(sample.id, out, raw));
    }
    v.push(check(sample.expect, CheckKind::Manual, None));
    v
}

/// 一条用例的执行结果
#[derive(Debug, Clone)]
struct Outcome {
    case: Case,
    raw: String,
    output: String,
    no_text: bool,
    elapsed_ms: u128,
    truncated: bool,
    error: Option<String>,
    checks: Vec<Check>,
}

/// 运行评测：逐样本 × 风格请求模型，执行自动断言，结果写入 out 目录
pub async fn run(args: EvalArgs) -> Result<(), String> {
    let specs = load_providers(&args.provider_json)?;
    let styles = select_styles(&args.styles)?;
    let samples = select_samples(&args.samples)?;
    std::fs::create_dir_all(&args.out).map_err(|e| format!("create {}: {e}", args.out.display()))?;
    let ocr_dir = args.ocr_dir.clone().unwrap_or_else(|| args.out.join("ocr-samples"));
    let date = today();
    let client = LlmClient::new(ClientConfig {
        user_agent: format!("wisp-eval/{}", env!("CARGO_PKG_VERSION")),
        caps: Arc::new(MemoryCapsStore::new()),
    });

    let (mut failed_auto, mut errors) = (0usize, 0usize);
    for spec in &specs {
        let key = if args.dry_run {
            None
        } else {
            std::env::var(key_env_name(&spec.provider.id)).ok()
        };
        if !args.dry_run && key.is_none() {
            log_line(&format!(
                "warning: {} is not set; sending requests without a key (only works for keyless local endpoints)",
                key_env_name(&spec.provider.id)
            ));
        }
        let key = key.map(|k| Arc::new(Secret::new(k)));
        for model in &spec.models {
            let mut cases = plan_text_cases(&styles, &samples, model.reasoning);
            let mut missing_ocr = Vec::new();
            if model.vision != Vision::No && args.samples.is_empty() {
                let (ocr, missing) = plan_ocr_cases(&ocr_dir)?;
                cases.extend(ocr);
                missing_ocr = missing;
            }
            let stem = format!("{date}-{}-{}", slug(&spec.provider.id), slug(&model.id));
            if !missing_ocr.is_empty() {
                log_line(&format!(
                    "skipping OCR samples without screenshots: {}",
                    missing_ocr.join(", ")
                ));
            }
            if args.dry_run {
                let path = args.out.join(format!("{stem}.dry-run.jsonl"));
                write_dry_run(&path, &spec.provider, model, &cases)?;
                log_line(&format!("dry run: {} cases → {}", cases.len(), path.display()));
                continue;
            }
            log_line(&format!("{} / {}: {} cases", spec.provider.id, model.id, cases.len()));
            let outcomes = execute(&client, &spec.provider, model, key.clone(), cases, args.concurrency).await;
            let summary = Summary::of(&outcomes);
            failed_auto += summary.auto_total - summary.auto_passed;
            errors += summary.errors;
            write_jsonl(
                &args.out.join(format!("{stem}.jsonl")),
                &spec.provider,
                model,
                &outcomes,
            )?;
            write_markdown(
                &args.out.join(format!("{stem}.md")),
                &date,
                &spec.provider,
                model,
                &outcomes,
                &missing_ocr,
            )?;
            log_line(&format!(
                "{} / {}: auto {}/{} passed, aux {}/{}, {} errors, {} manual items → {}",
                spec.provider.id,
                model.id,
                summary.auto_passed,
                summary.auto_total,
                summary.aux_passed,
                summary.aux_total,
                summary.errors,
                summary.manual,
                args.out.join(format!("{stem}.md")).display()
            ));
        }
    }
    match (failed_auto, errors) {
        (0, 0) => Ok(()),
        (f, e) => Err(format!(
            "{f} automatic assertion(s) failed, {e} request(s) errored; see the reports"
        )),
    }
}

#[allow(clippy::print_stderr)] // 命令行工具：进度输出到 stderr
fn log_line(s: &str) {
    eprintln!("wisp-eval: {s}");
}

async fn execute(
    client: &LlmClient,
    provider: &Provider,
    model: &ModelEntry,
    key: Option<Arc<Secret>>,
    cases: Vec<Case>,
    concurrency: usize,
) -> Vec<Outcome> {
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let mut set = JoinSet::new();
    // 任务 id → (序号, 用例)：任务 panic 时仍能在报告里记下是哪个用例出错
    let mut pending: HashMap<tokio::task::Id, (usize, Case)> = HashMap::with_capacity(cases.len());
    for (index, case) in cases.into_iter().enumerate() {
        let (client, provider, model, key, semaphore) = (
            client.clone(),
            provider.clone(),
            model.clone(),
            key.clone(),
            semaphore.clone(),
        );
        let task_case = case.clone();
        let handle = set.spawn(async move {
            let _permit = semaphore.acquire_owned().await;
            run_case(&client, provider, model, key, task_case).await
        });
        pending.insert(handle.id(), (index, case));
    }
    let mut outcomes: Vec<(usize, Outcome)> = Vec::with_capacity(pending.len());
    while let Some(joined) = set.join_next_with_id().await {
        match joined {
            Ok((id, outcome)) => {
                if let Some((index, _)) = pending.remove(&id) {
                    outcomes.push((index, outcome));
                }
            }
            Err(err) => {
                if let Some((index, case)) = pending.remove(&err.id()) {
                    outcomes.push((index, failed_outcome(case, format!("case task failed: {err}"))));
                }
            }
        }
    }
    outcomes.sort_by_key(|(i, _)| *i);
    outcomes.into_iter().map(|(_, o)| o).collect()
}

/// 没有拿到模型输出的用例（任务 panic 等）：记为一条错误，计入 errors，退出码非零
fn failed_outcome(case: Case, error: String) -> Outcome {
    Outcome {
        case,
        raw: String::new(),
        output: String::new(),
        no_text: false,
        elapsed_ms: 0,
        truncated: false,
        error: Some(error),
        checks: Vec::new(),
    }
}

/// 一个请求的流式结果
#[derive(Debug, Default, PartialEq)]
struct Collected {
    /// 模型原始输出（后处理之前）
    raw: String,
    /// 后处理放行的文本（不含 finish 的结尾部分）
    output: String,
    truncated: bool,
    error: Option<String>,
}

/// 通道在 Done / Error 之前关闭时记录的错误
const STREAM_ENDED_EARLY: &str = "stream ended without Done";

/// 读完一个请求的事件流：Delta 经过后处理累积，Done / Error 结束。通道在二者之前关闭（客户端任务异常退出等）
/// 时记为错误——输出可能不完整，不能当成功用例去跑自动断言
async fn collect_stream(
    rx: &mut tokio::sync::mpsc::Receiver<StreamEvent>,
    post: &mut PostProcessor,
    got: &mut Collected,
) {
    while let Some(event) = rx.recv().await {
        match event {
            StreamEvent::Delta { text } => {
                got.raw.push_str(&text);
                if let Some(s) = post.push(&text) {
                    got.output.push_str(&s);
                }
            }
            StreamEvent::Done { truncated, .. } => {
                got.truncated = truncated.is_some();
                return;
            }
            StreamEvent::Error(e) => {
                got.error = Some(format!("{:?}: {}", e.code, e.message));
                return;
            }
            _ => {}
        }
    }
    got.error = Some(STREAM_ENDED_EARLY.to_owned());
}

async fn run_case(
    client: &LlmClient,
    provider: Provider,
    model: ModelEntry,
    key: Option<Arc<Secret>>,
    case: Case,
) -> Outcome {
    let user = match &case.image {
        Some(bytes) => UserContent::Image {
            bytes: bytes.as_ref().clone().into(),
            mime: "image/png",
            text: case.prompt.user_text.clone(),
            detail: wisp_types::ImageDetail::High,
        },
        None => UserContent::Text(case.prompt.user_text.clone()),
    };
    let request = ChatRequest {
        request_id: format!("r-eval-{}-{}", case.sample_id, case.task),
        provider,
        key: key.map(|k| Secret::new(k.expose().to_owned())),
        model,
        system: case.prompt.system.clone(),
        user,
        temperature: case.prompt.temperature,
        max_output_tokens: case.prompt.max_output_tokens,
        purpose: if case.image.is_some() {
            Purpose::Ocr
        } else {
            Purpose::Translate
        },
    };
    // `stream` 需要 tokio-util 的 CancellationToken；这里靠类型推断构造，避免为开发工具新增依赖（不改 Cargo.lock）
    let cancel = Default::default();
    let mut rx = client.stream(request, Clone::clone(&cancel));
    let mut post = PostProcessor::new(case.prompt.post, &case.source);
    let mut got = Collected::default();
    let started = Instant::now();
    // 超时时保留已经收到的部分（写进报告便于排查）
    if tokio::time::timeout(CASE_TIMEOUT, collect_stream(&mut rx, &mut post, &mut got))
        .await
        .is_err()
    {
        cancel.cancel();
        got.error = Some(format!("timed out after {}s", CASE_TIMEOUT.as_secs()));
    }
    let Collected {
        raw,
        mut output,
        truncated,
        error,
    } = got;
    let finish = post.finish();
    output.push_str(&finish.tail);
    let output = output.trim().to_owned();
    let checks = if error.is_some() {
        Vec::new()
    } else if let Some(task) = case.eval_task() {
        evaluate(case.sample_id, &case.source, task, &output, case.target == Lang::ZhCn)
    } else {
        OCR_SAMPLES
            .iter()
            .find(|o| o.id == case.sample_id)
            .map_or_else(Vec::new, |o| evaluate_ocr(o, &output, &raw, finish.no_text))
    };
    Outcome {
        elapsed_ms: started.elapsed().as_millis(),
        raw,
        output,
        no_text: finish.no_text,
        truncated,
        error,
        checks,
        case,
    }
}

/// 一组结果的统计
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Summary {
    auto_total: usize,
    auto_passed: usize,
    aux_total: usize,
    aux_passed: usize,
    manual: usize,
    errors: usize,
}

impl Summary {
    fn of(outcomes: &[Outcome]) -> Self {
        let mut s = Self::default();
        for o in outcomes {
            if o.error.is_some() {
                s.errors += 1;
            }
            for c in &o.checks {
                match c.kind {
                    CheckKind::Auto => {
                        s.auto_total += 1;
                        s.auto_passed += usize::from(c.passed == Some(true));
                    }
                    CheckKind::Aux => {
                        s.aux_total += 1;
                        s.aux_passed += usize::from(c.passed == Some(true));
                    }
                    CheckKind::Manual => s.manual += 1,
                }
            }
        }
        s
    }
}

fn provider_json(provider: &Provider, model: &ModelEntry) -> Value {
    // 只记录不含凭据的字段：headers 的值可能含 Key，不写入报告
    json!({
        "provider_id": provider.id,
        "protocol": provider.protocol,
        "base_url": provider.base_url,
        "model_id": model.id,
        "reasoning": model.reasoning,
    })
}

fn checks_json(checks: &[Check]) -> Value {
    Value::Array(
        checks
            .iter()
            .map(|c| json!({ "name": c.name, "kind": c.kind.as_str(), "passed": c.passed }))
            .collect(),
    )
}

fn case_json(case: &Case) -> Value {
    json!({
        "sample": case.sample_id,
        "task": case.task,
        "dict": case.dict,
        "source_lang": case.detection.lang,
        "low_confidence": case.detection.low_confidence,
        "target_lang": case.target,
        "temperature": case.prompt.temperature,
        "max_output_tokens": case.prompt.max_output_tokens,
    })
}

fn write_dry_run(path: &Path, provider: &Provider, model: &ModelEntry, cases: &[Case]) -> Result<(), String> {
    let mut out = String::new();
    for case in cases {
        let mut v = case_json(case);
        v["model"] = provider_json(provider, model);
        v["system"] = json!(case.prompt.system);
        v["user"] = json!(case.prompt.user_text);
        out.push_str(&v.to_string());
        out.push('\n');
    }
    std::fs::write(path, out).map_err(|e| format!("write {}: {e}", path.display()))
}

fn write_jsonl(path: &Path, provider: &Provider, model: &ModelEntry, outcomes: &[Outcome]) -> Result<(), String> {
    let mut out = String::new();
    for o in outcomes {
        let mut v = case_json(&o.case);
        v["model"] = provider_json(provider, model);
        v["raw"] = json!(o.raw);
        v["output"] = json!(o.output);
        v["no_text"] = json!(o.no_text);
        v["truncated"] = json!(o.truncated);
        v["elapsed_ms"] = json!(o.elapsed_ms);
        v["error"] = json!(o.error);
        v["checks"] = checks_json(&o.checks);
        out.push_str(&v.to_string());
        out.push('\n');
    }
    std::fs::write(path, out).map_err(|e| format!("write {}: {e}", path.display()))
}

fn write_markdown(
    path: &Path,
    date: &str,
    provider: &Provider,
    model: &ModelEntry,
    outcomes: &[Outcome],
    missing_ocr: &[String],
) -> Result<(), String> {
    let s = Summary::of(outcomes);
    let mut md = String::new();
    let _ = writeln!(md, "# 提示词评测 · {} / {}\n", provider.id, model.id);
    let _ = writeln!(md, "- 日期：{date}");
    let _ = writeln!(
        md,
        "- 协议：{:?}；base_url：`{}`；推理模型：{}",
        provider.protocol, provider.base_url, model.reasoning
    );
    let _ = writeln!(md, "- 用例：{}；请求错误：{}", outcomes.len(), s.errors);
    let _ = writeln!(
        md,
        "- 自动断言：{}/{} 通过；辅助检查：{}/{}；待人工：{} 项",
        s.auto_passed, s.auto_total, s.aux_passed, s.aux_total, s.manual
    );
    let _ = writeln!(
        md,
        "- 生成：`cargo run -p wisp-prompts --features eval --bin wisp-eval`（断言作用域见 `src/eval/checks.rs`）\n"
    );
    if !missing_ocr.is_empty() {
        let _ = writeln!(md, "> 缺少 OCR 截图样本，已跳过：{}\n", missing_ocr.join("、"));
    }

    // 按任务汇总
    let _ = writeln!(
        md,
        "## 汇总\n\n| 任务 | 自动断言 | 辅助检查 | 错误 |\n| --- | --- | --- | --- |"
    );
    let mut tasks: Vec<&str> = Vec::new();
    for o in outcomes {
        if !tasks.contains(&o.case.task.as_str()) {
            tasks.push(&o.case.task);
        }
    }
    for task in tasks {
        let subset: Vec<Outcome> = outcomes.iter().filter(|o| o.case.task == task).cloned().collect();
        let t = Summary::of(&subset);
        let _ = writeln!(
            md,
            "| {task} | {}/{} | {}/{} | {} |",
            t.auto_passed, t.auto_total, t.aux_passed, t.aux_total, t.errors
        );
    }

    let failures: Vec<&Outcome> = outcomes
        .iter()
        .filter(|o| {
            o.error.is_some()
                || o.checks
                    .iter()
                    .any(|c| c.kind == CheckKind::Auto && c.passed == Some(false))
        })
        .collect();
    let _ = writeln!(md, "\n## 自动断言失败与请求错误（{}）\n", failures.len());
    for o in &failures {
        let _ = writeln!(
            md,
            "- **{} · {}**：{}",
            o.case.sample_id,
            o.case.task,
            failure_reason(o)
        );
    }

    let _ = writeln!(md, "\n## 逐条输出\n");
    for o in outcomes {
        let _ = writeln!(
            md,
            "### {} · {}{}\n\n- 方向：{} → {}{}；temperature {:?}；max_output_tokens {}；耗时 {} ms{}{}",
            o.case.sample_id,
            o.case.task,
            if o.case.dict { "（词典式）" } else { "" },
            o.case.detection.lang.as_str(),
            o.case.target.as_str(),
            if o.case.detection.low_confidence {
                "（低置信度）"
            } else {
                ""
            },
            o.case.prompt.temperature,
            o.case.prompt.max_output_tokens,
            o.elapsed_ms,
            if o.truncated { "；**输出被截断**" } else { "" },
            if o.no_text { "；判为无文字" } else { "" },
        );
        let _ = writeln!(md, "- 期望：{}", o.case.expect);
        if let Some(e) = &o.error {
            let _ = writeln!(md, "- 错误：{e}");
        }
        for c in &o.checks {
            let mark = match (c.kind, c.passed) {
                (CheckKind::Manual, _) => "- [ ] 待人工：",
                (_, Some(true)) => "- [x] ",
                _ => "- [ ] **失败**：",
            };
            let _ = writeln!(md, "{mark}{}（{}）", c.name, c.kind.as_str());
        }
        let _ = writeln!(md, "\n输出（后处理后）：\n\n{}\n", fenced(&o.output));
        if o.raw.trim() != o.output {
            let _ = writeln!(md, "原始输出：\n\n{}\n", fenced(&o.raw));
        }
    }
    std::fs::write(path, md).map_err(|e| format!("write {}: {e}", path.display()))
}

fn failure_reason(o: &Outcome) -> String {
    if let Some(e) = &o.error {
        return format!("请求错误 {e}");
    }
    o.checks
        .iter()
        .filter(|c| c.kind == CheckKind::Auto && c.passed == Some(false))
        .map(|c| c.name.as_str())
        .collect::<Vec<_>>()
        .join("；")
}

/// 用比内容中最长反引号串更长的围栏包住文本
fn fenced(text: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}text\n{text}\n{fence}")
}

/// 文件名用的 slug：字母数字保留，其余替换为 `-`
fn slug(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-').to_owned()
}

/// 今天的 UTC 日期 YYYY-MM-DD（不引入日期库：days-from-civil 的逆运算）
fn today() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    civil_date(secs / 86_400)
}

fn civil_date(days_since_epoch: u64) -> String {
    let z = days_since_epoch as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<EvalArgs, String> {
        parse_args(list.iter().map(|s| (*s).to_owned()))
    }

    #[test]
    fn parse_arguments() {
        let a = args(&["--provider-json", "p.json"]).expect("args");
        assert_eq!(a.provider_json, PathBuf::from("p.json"));
        assert_eq!(a.styles, vec!["all"]);
        assert_eq!(a.out, PathBuf::from("target/eval-runs"));
        assert!(a.samples.is_empty() && !a.dry_run && a.ocr_dir.is_none());
        assert_eq!(a.concurrency, 4);

        let a = args(&[
            "--provider-json=p.json",
            "--styles",
            "natural, technical",
            "--out",
            "/tmp/x",
            "--samples=S1,S16",
            "--ocr-dir",
            "shots",
            "--concurrency",
            "8",
            "--dry-run",
        ])
        .expect("args");
        assert_eq!(a.styles, vec!["natural", "technical"]);
        assert_eq!(a.samples, vec!["S1", "S16"]);
        assert_eq!(a.ocr_dir, Some(PathBuf::from("shots")));
        assert_eq!(a.concurrency, 8);
        assert!(a.dry_run);
        assert!(
            args(&["--provider-json", "p", "--samples", "all"])
                .expect("args")
                .samples
                .is_empty()
        );

        assert!(args(&[]).is_err());
        assert!(args(&["--provider-json"]).is_err());
        assert!(args(&["--provider-json", "p", "--concurrency", "0"]).is_err());
        assert!(args(&["--provider-json", "p", "--bogus"]).is_err());
        assert!(args(&["--help"]).is_err());
    }

    #[test]
    fn styles_and_samples_selection() {
        assert_eq!(select_styles(&["all".to_owned()]).expect("all").len(), 6);
        assert_eq!(select_styles(&["learn".to_owned()]).expect("learn")[0].id, "learn");
        assert!(select_styles(&["nope".to_owned()]).is_err());
        assert_eq!(select_samples(&[]).expect("all").len(), 16);
        assert!(select_samples(&["S99".to_owned()]).is_err());
    }

    #[test]
    fn plan_covers_matrix() {
        let styles = builtin_styles();
        let cases = plan_text_cases(&styles, &SAMPLES, false);
        // 15 个翻译样本 × 6 风格 + S14 × 2 个编辑动作
        assert_eq!(cases.len(), 15 * 6 + 2);
        let s16: Vec<&Case> = cases.iter().filter(|c| c.sample_id == "S16").collect();
        assert!(s16.iter().all(|c| c.target == Lang::ZhCn), "S16 强制目标为 zh");
        let s1_literal = cases
            .iter()
            .find(|c| c.sample_id == "S1" && c.task == "literal")
            .expect("S1 literal");
        assert!(!s1_literal.dict, "literal 不进词典式");
        let s1_natural = cases
            .iter()
            .find(|c| c.sample_id == "S1" && c.task == "natural")
            .expect("S1 natural");
        assert!(s1_natural.dict);
        let s3 = cases.iter().find(|c| c.sample_id == "S3").expect("S3");
        assert_eq!(s3.detection.lang, Lang::ZhCn, "S3 检测为 zh");
        assert_eq!(s3.target, Lang::En);
        let s12 = cases.iter().find(|c| c.sample_id == "S12").expect("S12");
        assert_eq!(s12.detection.lang, Lang::ZhCn, "S12 检测为 zh");
        assert!(
            cases
                .iter()
                .filter(|c| c.sample_id == "S14")
                .all(|c| c.task == "polish" || c.task == "proofread")
        );
    }

    #[test]
    fn provider_file_shapes() {
        let dir = std::env::temp_dir().join(format!("wisp-eval-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let provider = json!({
            "id": "openai-main",
            "protocol": "openai_chat",
            "base_url": "https://api.openai.com/v1",
            "models": [{"id": "gpt-4.1"}, {"id": "o4-mini", "reasoning": true, "enabled": false}]
        });
        let write = |name: &str, v: &Value| {
            let p = dir.join(name);
            std::fs::write(&p, v.to_string()).expect("write");
            p
        };
        let one = load_providers(&write("one.json", &provider)).expect("one");
        assert_eq!(
            one[0].models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["gpt-4.1"]
        );
        let pick = load_providers(&write(
            "pick.json",
            &json!({"provider": provider, "models": ["o4-mini", "new-model"]}),
        ))
        .expect("pick");
        assert!(pick[0].models[0].reasoning);
        assert_eq!(pick[0].models[1].id, "new-model");
        let many = load_providers(&write("many.json", &json!({"providers": [provider, provider]}))).expect("many");
        assert_eq!(many.len(), 2);
        assert!(load_providers(&write("bad.json", &json!({"id": "x"}))).is_err());
        assert!(load_providers(&dir.join("missing.json")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb([255, 255, 255]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .expect("encode");
        out
    }

    #[test]
    fn ocr_screenshots_match_production_size() {
        // 4000×1000 → 2048×512
        let big = prepare_ocr_png(png(4000, 1000)).expect("big");
        let img = image::load_from_memory(&big).expect("decode");
        assert_eq!((img.width(), img.height()), (2048, 512));
        let tall = image::load_from_memory(&prepare_ocr_png(png(900, 3000)).expect("tall")).expect("decode");
        assert_eq!((tall.width(), tall.height()), (614, 2048));
        // 不超过上限：原样使用
        let small = png(800, 600);
        assert_eq!(prepare_ocr_png(small.clone()).expect("small"), small);
        // 不是 PNG / 截断的 PNG：明确报错
        assert!(prepare_ocr_png(b"GIF89a....".to_vec()).is_err());
        let mut cut = png(10, 10);
        cut.truncate(20);
        assert!(prepare_ocr_png(cut).is_err());
    }

    #[test]
    fn paragraph_ocr_samples_get_layout_checks() {
        let sample = OCR_SAMPLES
            .iter()
            .find(|o| o.id == "O7-list-paragraphs")
            .expect("sample");
        let raw = "Wi-Fi\n\n\nBluetooth";
        let checks = evaluate_ocr(sample, "General\n\n- Wi-Fi\n\n- Bluetooth", raw, false);
        let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"连续的列表项之间没有空行"), "{names:?}");
        assert!(names.contains(&sample.expect), "期望要点留给人工评审");
        let list = checks
            .iter()
            .find(|c| c.name == "连续的列表项之间没有空行")
            .expect("check");
        assert_eq!(list.passed, Some(false));
        // 其他模式不做段落版面检查
        let lines = OCR_SAMPLES
            .iter()
            .find(|o| o.id == "O1-terminal-lines")
            .expect("sample");
        assert!(
            !evaluate_ocr(lines, "a\n\n- b\n\n- c", "", false)
                .iter()
                .any(|c| c.name.contains("列表项"))
        );
    }

    #[test]
    fn failed_tasks_count_as_errors() {
        let styles = builtin_styles();
        let case = plan_text_cases(&styles, &SAMPLES[..1], false).remove(0);
        let outcome = failed_outcome(case, "case task failed: panicked".to_owned());
        assert_eq!(Summary::of(&[outcome]).errors, 1);
    }

    #[tokio::test]
    async fn stream_closed_before_done_is_an_error() {
        let delta = |t: &str| StreamEvent::Delta { text: t.to_owned() };
        // Done 之前通道关闭：记为错误，已收到的部分保留
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        tx.send(delta("你好")).await.expect("send");
        drop(tx);
        let mut post = PostProcessor::new(crate::PostKind::Translate, "Hello");
        let mut got = Collected::default();
        collect_stream(&mut rx, &mut post, &mut got).await;
        assert_eq!(got.error.as_deref(), Some(STREAM_ENDED_EARLY));
        assert_eq!((got.raw.as_str(), got.output.as_str()), ("你好", "你好"));

        // 正常结束：没有错误；Done 之后的事件不再读取
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        tx.send(delta("ok")).await.expect("send");
        tx.send(StreamEvent::Done {
            elapsed_ms: 1,
            usage: None,
            truncated: None,
            cached: false,
            no_text: false,
        })
        .await
        .expect("send");
        tx.send(delta("ignored")).await.expect("send");
        let mut post = PostProcessor::new(crate::PostKind::Translate, "Hello");
        let mut got = Collected::default();
        collect_stream(&mut rx, &mut post, &mut got).await;
        assert_eq!((got.error, got.raw.as_str(), got.truncated), (None, "ok", false));
    }

    #[tokio::test]
    async fn panicking_case_tasks_are_reported() {
        // JoinSet 的任务 panic 时，join_next_with_id 返回的 JoinError 带着任务 id，能找回对应的用例
        let mut set = JoinSet::new();
        let handle = set.spawn(async {
            if std::hint::black_box(true) {
                panic!("boom");
            }
        });
        let id = handle.id();
        let joined = set.join_next_with_id().await.expect("joined");
        let err = joined.expect_err("panicked");
        assert!(err.is_panic());
        assert_eq!(err.id(), id);
    }

    #[test]
    fn helpers() {
        assert_eq!(key_env_name("openai-main"), "WISP_EVAL_KEY_OPENAI_MAIN");
        assert_eq!(slug("qwen/Qwen3-VL 235B"), "qwen-qwen3-vl-235b");
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(20_371), "2025-10-10");
        assert_eq!(civil_date(19_782), "2024-02-29");
        assert_eq!(fenced("a ``` b"), "````text\na ``` b\n````");
        assert_eq!(fenced("plain"), "```text\nplain\n```");
    }
}
