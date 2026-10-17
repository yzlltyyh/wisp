//! 评测运行器（feature `eval`，仅开发用，CI 不跑）：
//! `cargo run -p wisp-prompts --features eval --bin wisp-eval -- --provider-json <file> --styles all --out target/eval-runs/`（`--out` 省略时也写到这里）。
//! Key 从环境变量 `WISP_EVAL_KEY_<ID>` 读取（ID 为 provider id 的大写，非字母数字替换为 `_`）。
//!
//! 每个模型 × 每个风格 × S1–S16（S14 只跑 polish / proofread，S16 强制目标为 zh），视觉模型另跑 OCR 截图样本；
//! 输出（模型、风格、方向、原始输出、后处理后的输出、耗时、断言）写到 `<out>/<日期>-<provider>-<model>.{md,jsonl}`，
//! 自动断言汇总通过率，其余列为「待人工」勾选项。跑完在标准输出打印一份按样本汇总的结果（模型、耗时、自动断言的
//! 通过与失败项、报告路径），进度与警告写到标准错误，便于脚本只转发汇总。
//!
//! OCR 截图样本（`samples.rs` 的 `OCR_SAMPLES`）只发给视觉能力不是 `no` 的模型，图片从 `--ocr-dir`
//! （默认 `<out>/ocr-samples/`）读取，缺少的样本跳过并在报告里注明：
//! - `--ocr-samples O5,O6-pdf-paragraphs`：只跑列出的 OCR 样本（大小写不敏感；`O5` 这种写法匹配 id 中第一个 `-`
//!   之前的部分，所以 `O3` 同时选中 `O3-formula-markdown` 与 `O3-formula-translate`）。不给时跑全部；
//!   传了 `--samples` 时默认不跑 OCR 样本，除非同时给出 `--ocr-samples`；
//! - `--ocr-only`：只跑 OCR 样本，不跑翻译样本（忽略 `--styles`，不能与 `--samples` 同时使用）。此时选中的样本
//!   一张截图都没有、或者没有一个模型能收图片，直接以非零退出，避免什么都没跑却显示成功。
//!   仓库根目录的 `pnpm eval:ocr` 会先生成段落模式的截图再以这个模式调用本工具（见 docs/development.md）。
//!
//! `--dry-run` 只装配提示词：在标准输出列出每个模型的计划（同样按上面两个参数过滤），并写出 `.dry-run.jsonl`，
//! 不发请求、不读 Key。Key 与请求头不进汇总、报告与日志。
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
    /// 只跑 OCR 截图样本，不跑翻译样本
    pub ocr_only: bool,
    /// 只跑这些 OCR 截图样本（解析时已换成 `OCR_SAMPLES` 里的完整 id，按样本表的顺序；空 = 全部）
    pub ocr_samples: Vec<&'static str>,
}

impl EvalArgs {
    /// 视觉模型是否跑 OCR 样本：只给 `--samples` 时专注翻译样本（沿用原来的行为），
    /// 明确要了 OCR（`--ocr-only` 或 `--ocr-samples`）时总是跑
    fn runs_ocr(&self) -> bool {
        self.ocr_only || !self.ocr_samples.is_empty() || self.samples.is_empty()
    }
}

const USAGE: &str = "usage: wisp-eval --provider-json <file> [--styles all|natural,technical,…] [--out <dir>] \
[--samples all|S1,S4,…] [--ocr-only] [--ocr-samples all|O5,O6-pdf-paragraphs,…] [--ocr-dir <dir>] \
[--concurrency <n>] [--dry-run]";

/// `--help` 的完整说明：参数出错时只附 USAGE，免得错误信息被淹没
pub const HELP: &str = "usage: wisp-eval --provider-json <file> [options]

  --provider-json <file>   provider to evaluate (a settings.json provider object, {\"provider\", \"models\"}, or a list)
  --styles <list>          all (default) or comma-separated style ids; ignored with --ocr-only
  --samples <list>         all (default) or translation samples such as S1,S4; skips OCR samples unless
                           --ocr-samples is also given
  --ocr-only               run only the OCR screenshot samples (cannot be combined with --samples)
  --ocr-samples <list>     all (default) or OCR sample ids, case-insensitive; a prefix such as O5 selects
                           every id whose part before the first '-' matches
  --ocr-dir <dir>          screenshot directory (default <out>/ocr-samples)
  --out <dir>              report directory (default target/eval-runs)
  --concurrency <n>        parallel requests, 1-32 (default 4)
  --dry-run                list the plan and write the assembled prompts; no requests, no key

The API key is read from WISP_EVAL_KEY_<ID> (provider id upper-cased, other characters replaced with _).
A per-sample summary is printed to stdout; progress goes to stderr.";

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
    // 记下是否显式给过 --samples：`--samples all` 解析后也是空列表，但同样不能与 --ocr-only 并用
    let mut samples_given = false;
    let mut ocr_only = false;
    let mut ocr_samples = Vec::new();

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
            "--samples" => {
                samples = split_list(&value("--samples")?);
                samples_given = true;
            }
            "--ocr-only" => ocr_only = true,
            "--ocr-samples" => ocr_samples = resolve_ocr_samples(&split_list(&value("--ocr-samples")?))?,
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
            "-h" | "--help" => return Err(HELP.to_owned()),
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    let provider_json = provider_json.ok_or_else(|| format!("--provider-json is required\n{USAGE}"))?;
    if styles.is_empty() {
        return Err(format!("--styles must not be empty\n{USAGE}"));
    }
    if ocr_only && samples_given {
        return Err(format!(
            "--ocr-only runs no translation samples, so it cannot be combined with --samples; \
             use --ocr-samples to pick screenshots\n{USAGE}"
        ));
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
        ocr_only,
        ocr_samples,
    })
}

/// 把 `--ocr-samples` 的列表换成样本表里的完整 id（按样本表顺序、去重）。在解析阶段就核对，
/// 拼错的 id 立刻报错并列出可用的 id，而不是等读完配置、跑完翻译样本才发现
fn resolve_ocr_samples(tokens: &[String]) -> Result<Vec<&'static str>, String> {
    if tokens.is_empty() {
        // 空列表若当成「全部」，脚本里变量为空时会悄悄跑满所有截图
        return Err(format!("--ocr-samples must not be empty\n{USAGE}"));
    }
    if tokens.iter().any(|t| t.eq_ignore_ascii_case("all")) {
        return Ok(Vec::new());
    }
    let unknown: Vec<String> = tokens
        .iter()
        .filter(|t| !OCR_SAMPLES.iter().any(|o| samples::ocr_sample_matches(o, t)))
        .map(|t| format!("{t:?}"))
        .collect();
    if !unknown.is_empty() {
        let available: Vec<&str> = OCR_SAMPLES.iter().map(|o| o.id).collect();
        return Err(format!(
            "unknown OCR sample {}; available: {} (a prefix such as O5 also works)",
            unknown.join(", "),
            available.join(", ")
        ));
    }
    Ok(OCR_SAMPLES
        .iter()
        .filter(|o| tokens.iter().any(|t| samples::ocr_sample_matches(o, t)))
        .map(|o| o.id)
        .collect())
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

/// 按 `EvalArgs::ocr_samples`（完整 id，空 = 全部）取样本，保持样本表的顺序
fn select_ocr_samples(ids: &[&str]) -> Vec<OcrSample> {
    OCR_SAMPLES
        .iter()
        .filter(|o| ids.is_empty() || ids.contains(&o.id))
        .copied()
        .collect()
}

/// 视觉能力不是 `no` 的模型才收截图：未知的模型照样试，由服务端的回应说明它能不能看图
fn accepts_images(model: &ModelEntry) -> bool {
    model.vision != Vision::No
}

/// OCR 用例：缺少的截图记入 missing（报告里注明跳过）；不是 PNG 或无法解码时报错（退出码非零）
fn plan_ocr_cases(dir: &Path, selected: &[OcrSample]) -> Result<(Vec<Case>, Vec<String>), String> {
    let mut cases = Vec::new();
    let mut missing = Vec::new();
    for o in selected {
        let path = dir.join(o.file);
        let Ok(bytes) = std::fs::read(&path) else {
            missing.push(format!("{} ({})", o.id, path.display()));
            continue;
        };
        let bytes = prepare_ocr_png(bytes).map_err(|e| format!("OCR sample {}: {e}", path.display()))?;
        cases.push(ocr_case(o, Arc::new(bytes)));
    }
    Ok((cases, missing))
}

/// 一个 OCR 用例：截图与提示词都和模型无关，所有视觉模型共用
fn ocr_case(o: &OcrSample, image: Arc<Vec<u8>>) -> Case {
    let detection = Detection {
        lang: Lang::Other,
        low_confidence: true,
    };
    let target = Lang::ZhCn;
    let ctx = PromptCtx {
        text: "",
        source: detection,
        target,
        glossary: &[],
        reasoning_model: false,
    };
    Case {
        sample_id: o.id,
        expect: o.expect,
        source: String::new(),
        task: format!("ocr:{}", ocr_mode_name(o.mode)),
        dict: false,
        detection,
        target,
        prompt: build(&PromptTask::Ocr { mode: o.mode }, &ctx),
        image: Some(image),
    }
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
    // --ocr-only 不跑翻译样本，风格与翻译样本都用不上，也就不核对
    let (styles, samples) = if args.ocr_only {
        (Vec::new(), Vec::new())
    } else {
        (select_styles(&args.styles)?, select_samples(&args.samples)?)
    };
    let runs_ocr = args.runs_ocr();
    let any_vision = specs.iter().flat_map(|s| &s.models).any(accepts_images);
    if args.ocr_only && !any_vision {
        return Err(
            "--ocr-only: none of the selected models accepts images (all have \"vision\": \"no\"); \
             nothing to run"
                .to_owned(),
        );
    }
    std::fs::create_dir_all(&args.out).map_err(|e| format!("create {}: {e}", args.out.display()))?;
    let ocr_dir = args.ocr_dir.clone().unwrap_or_else(|| args.out.join("ocr-samples"));
    // 截图与 OCR 提示词和模型无关：只读一次，所有视觉模型共用；没有视觉模型时不读，坏图也不会让纯文本评测失败
    let (ocr_cases, missing_ocr) = if runs_ocr && any_vision {
        plan_ocr_cases(&ocr_dir, &select_ocr_samples(&args.ocr_samples))?
    } else {
        (Vec::new(), Vec::new())
    };
    if args.ocr_only && ocr_cases.is_empty() {
        return Err(no_screenshots_error(&ocr_dir, &select_ocr_samples(&args.ocr_samples)));
    }
    if !missing_ocr.is_empty() {
        log_line(&format!(
            "skipping OCR samples without screenshots: {}",
            missing_ocr.join(", ")
        ));
    }
    let date = today();
    let client = LlmClient::new(ClientConfig {
        user_agent: format!("wisp-eval/{}", env!("CARGO_PKG_VERSION")),
        caps: Arc::new(MemoryCapsStore::new()),
    });

    let (mut failed_auto, mut errors) = (0usize, 0usize);
    let mut report = String::new();
    for spec in &specs {
        let runnable = spec.models.iter().any(|m| !args.ocr_only || accepts_images(m));
        let key = if args.dry_run || !runnable {
            None
        } else {
            std::env::var(key_env_name(&spec.provider.id)).ok()
        };
        if !args.dry_run && runnable && key.is_none() {
            log_line(&format!(
                "warning: {} is not set; sending requests without a key (only works for keyless local endpoints)",
                key_env_name(&spec.provider.id)
            ));
        }
        let key = key.map(|k| Arc::new(Secret::new(k)));
        for model in &spec.models {
            let label = format!("{} / {}", spec.provider.id, model.id);
            if args.ocr_only && !accepts_images(model) {
                log_line(&format!("{label}: skipped, the model does not accept images"));
                let _ = writeln!(report, "{label}  →  skipped (\"vision\": \"no\")");
                continue;
            }
            let ocr_for_model = runs_ocr && accepts_images(model);
            let mut cases = if args.ocr_only {
                Vec::new()
            } else {
                plan_text_cases(&styles, &samples, model.reasoning)
            };
            if ocr_for_model {
                cases.extend(ocr_cases.iter().cloned());
            }
            let missing_for_model: &[String] = if ocr_for_model { &missing_ocr } else { &[] };
            let stem = format!("{date}-{}-{}", slug(&spec.provider.id), slug(&model.id));
            if args.dry_run {
                let path = args.out.join(format!("{stem}.dry-run.jsonl"));
                write_dry_run(&path, &spec.provider, model, &cases)?;
                report.push_str(&format_plan(&label, &path, &cases, missing_for_model));
                continue;
            }
            log_line(&format!("{label}: {} cases", cases.len()));
            let outcomes = execute(&client, &spec.provider, model, key.clone(), cases, args.concurrency).await;
            let summary = Summary::of(&outcomes);
            failed_auto += summary.auto_total - summary.auto_passed;
            errors += summary.errors;
            let md_path = args.out.join(format!("{stem}.md"));
            write_jsonl(
                &args.out.join(format!("{stem}.jsonl")),
                &spec.provider,
                model,
                &outcomes,
            )?;
            write_markdown(&md_path, &date, &spec.provider, model, &outcomes, missing_for_model)?;
            log_line(&format!(
                "{label}: auto {}/{} passed, aux {}/{}, {} errors, {} manual items → {}",
                summary.auto_passed,
                summary.auto_total,
                summary.aux_passed,
                summary.aux_total,
                summary.errors,
                summary.manual,
                md_path.display()
            ));
            report.push_str(&format_model_summary(&label, &md_path, &outcomes, missing_for_model));
        }
    }
    // 汇总走标准输出：断言失败、退出码非零时也照样打印，一眼看到哪个样本、哪条断言没过
    print_report(if args.dry_run { "plan" } else { "summary" }, &report);
    match (failed_auto, errors) {
        (0, 0) => Ok(()),
        (f, e) => Err(format!(
            "{f} automatic assertion(s) failed, {e} request(s) errored; see the reports"
        )),
    }
}

/// --ocr-only 时一张截图都没找到：给出目录与期望的文件名，方便放图或改 --ocr-dir
fn no_screenshots_error(dir: &Path, selected: &[OcrSample]) -> String {
    let mut files: Vec<&str> = Vec::new();
    for o in selected {
        if !files.contains(&o.file) {
            files.push(o.file);
        }
    }
    format!(
        "--ocr-only: no screenshot found for the selected OCR samples in {}; expected {} \
         (generate them with `pnpm eval:ocr`, or point --ocr-dir at your own PNGs)",
        dir.display(),
        files.join(", ")
    )
}

#[allow(clippy::print_stderr)] // 命令行工具：进度输出到 stderr
fn log_line(s: &str) {
    eprintln!("wisp-eval: {s}");
}

#[allow(clippy::print_stdout)] // 命令行工具：计划与汇总是结果本身，写到 stdout 便于脚本转发或重定向
fn print_report(title: &str, body: &str) {
    if body.is_empty() {
        return;
    }
    println!("\nwisp-eval {title}\n\n{}", body.trim_end());
}

/// 按样本 id 分组，保持首次出现的顺序：同一样本的多个风格合成一组
fn group_by_sample<T>(items: &[T], id: impl Fn(&T) -> &'static str) -> Vec<(&'static str, Vec<&T>)> {
    let mut groups: Vec<(&'static str, Vec<&T>)> = Vec::new();
    for item in items {
        let key = id(item);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, list)) => list.push(item),
            None => groups.push((key, vec![item])),
        }
    }
    groups
}

/// 缺图跳过的 OCR 样本也写进计划与汇总：脚本只转发标准输出时同样看得到少跑了哪些
fn push_skipped(out: &mut String, missing: &[String]) {
    if !missing.is_empty() {
        let _ = writeln!(out, "  skipped (no screenshot): {}", missing.join(", "));
    }
}

/// dry run 时列出一个模型的计划：同一样本的多个风格并成一行，OCR 样本注明截图文件
fn format_plan(label: &str, path: &Path, cases: &[Case], missing: &[String]) -> String {
    let groups = group_by_sample(cases, |c| c.sample_id);
    let width = groups.iter().map(|(id, _)| id.len()).max().unwrap_or(0);
    let mut out = format!("{label}: {} case(s)  →  {}\n", cases.len(), path.display());
    for (id, list) in &groups {
        let tasks: Vec<String> = list
            .iter()
            .map(|c| match OCR_SAMPLES.iter().find(|o| o.id == c.sample_id) {
                Some(o) if c.image.is_some() => format!("{} ({})", c.task, o.file),
                _ => c.task.clone(),
            })
            .collect();
        let _ = writeln!(out, "  {id:<width$}  {}", tasks.join(", "));
    }
    push_skipped(&mut out, missing);
    out.push('\n');
    out
}

/// 跑完后一个模型的汇总：每个样本一行（同一样本的多个风格合并），列出任务、请求耗时之和、自动断言的通过数与
/// 失败的断言名、请求错误；只用断言名与模型 id，不涉及 Key、请求头与输出内容
fn format_model_summary(label: &str, report: &Path, outcomes: &[Outcome], missing: &[String]) -> String {
    let groups = group_by_sample(outcomes, |o| o.case.sample_id);
    let rows: Vec<[String; 4]> = groups.iter().map(|(id, list)| summary_row(id, list)).collect();
    let w0 = rows.iter().map(|r| r[0].len()).max().unwrap_or(0);
    let w1 = rows.iter().map(|r| r[1].len()).max().unwrap_or(0);
    let w2 = rows.iter().map(|r| r[2].len()).max().unwrap_or(0);
    let mut out = format!("{label}  →  {}\n", report.display());
    for [id, task, time, result] in &rows {
        let _ = writeln!(out, "  {id:<w0$}  {task:<w1$}  {time:>w2$}  {result}");
    }
    push_skipped(&mut out, missing);
    let s = Summary::of(outcomes);
    let _ = writeln!(
        out,
        "  total: auto {}/{} passed, aux {}/{}, {} error(s), {} manual item(s)\n",
        s.auto_passed, s.auto_total, s.aux_passed, s.aux_total, s.errors, s.manual
    );
    out
}

fn summary_row(id: &str, list: &[&Outcome]) -> [String; 4] {
    // 同一样本跑了多个风格时，失败项前面带上风格，才知道是哪一次没过
    let multi = list.len() > 1;
    let (mut passed, mut total) = (0usize, 0usize);
    let (mut failed, mut errors) = (Vec::new(), Vec::new());
    let mut elapsed_ms = 0u128;
    for o in list {
        let tag = |s: &str| {
            if multi {
                format!("{} · {s}", o.case.task)
            } else {
                s.to_owned()
            }
        };
        elapsed_ms += o.elapsed_ms;
        if let Some(e) = &o.error {
            errors.push(tag(&e.replace(['\r', '\n'], " ")));
        }
        for c in o.checks.iter().filter(|c| c.kind == CheckKind::Auto) {
            total += 1;
            if c.passed == Some(true) {
                passed += 1;
            } else {
                failed.push(tag(&c.name));
            }
        }
    }
    let status = if !errors.is_empty() {
        "ERROR"
    } else if !failed.is_empty() {
        "FAIL"
    } else if total == 0 {
        "n/a"
    } else {
        "pass"
    };
    let mut result = format!("{status:<5} auto {passed}/{total}");
    if !failed.is_empty() {
        let _ = write!(result, "  failed: {}", failed.join("; "));
    }
    if !errors.is_empty() {
        let _ = write!(result, "  error: {}", errors.join("; "));
    }
    let task = match list {
        [one] => one.case.task.clone(),
        _ => format!("{} tasks", list.len()),
    };
    [id.to_owned(), task, format_secs(elapsed_ms), result]
}

/// 毫秒 → `12.3s`（整数运算，免得浮点转换）
fn format_secs(ms: u128) -> String {
    format!("{}.{}s", ms / 1000, ms % 1000 / 100)
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
        assert!(!a.ocr_only && a.ocr_samples.is_empty() && a.runs_ocr());
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
        // bin 靠「错误文本等于 HELP」区分主动要帮助（退出码 0）与参数出错（退出码 2）
        assert_eq!(args(&["--help"]).expect_err("help"), HELP);
        assert_eq!(args(&["--provider-json", "p", "-h"]).expect_err("help"), HELP);
        assert_ne!(args(&[]).expect_err("missing"), HELP);
    }

    #[test]
    fn parse_ocr_arguments() {
        let ids = |list: &[&str]| args(list).map(|a| a.ocr_samples);

        let a = args(&["--provider-json", "p", "--ocr-only"]).expect("ocr-only");
        assert!(a.ocr_only && a.ocr_samples.is_empty() && a.runs_ocr());
        assert_eq!(a.styles, vec!["all"], "--styles 可以省略");
        assert!(args(&["--provider-json", "p", "--ocr-only", "--styles", "natural"]).is_ok());

        // 完整 id 与前缀写法都大小写不敏感；结果按样本表的顺序去重
        assert_eq!(
            ids(&["--provider-json", "p", "--ocr-samples", "O5,o6-PDF-paragraphs"]).expect("ids"),
            ["O5-chat-paragraphs", "O6-pdf-paragraphs"]
        );
        assert_eq!(
            ids(&["--provider-json", "p", "--ocr-samples=o8, O5,O5-chat-paragraphs"]).expect("ids"),
            ["O5-chat-paragraphs", "O8-mixed-paragraphs"]
        );
        assert_eq!(
            ids(&["--provider-json", "p", "--ocr-samples", "o3"]).expect("ids"),
            ["O3-formula-markdown", "O3-formula-translate"],
            "前缀匹配同一张截图的两种模式"
        );
        assert!(
            ids(&["--provider-json", "p", "--ocr-samples", "all"])
                .expect("all")
                .is_empty()
        );

        // 未知 id：报错并列出可用的 id；前缀只认第一个 '-' 之前的部分
        let err = args(&["--provider-json", "p", "--ocr-samples", "O5,O9"]).expect_err("unknown");
        assert!(err.contains("\"O9\"") && !err.contains("\"O5\""), "{err}");
        assert!(OCR_SAMPLES.iter().all(|o| err.contains(o.id)), "{err}");
        assert!(args(&["--provider-json", "p", "--ocr-samples", "O5-chat"]).is_err());
        assert!(args(&["--provider-json", "p", "--ocr-samples", "O"]).is_err());
        assert!(args(&["--provider-json", "p", "--ocr-samples", " , "]).is_err());
        assert!(args(&["--provider-json", "p", "--ocr-samples"]).is_err());

        // --ocr-only 与 --samples 冲突（包括 --samples all），顺序无关
        for list in [
            ["--ocr-only", "--samples", "S1"],
            ["--samples", "S1", "--ocr-only"],
            ["--samples", "all", "--ocr-only"],
        ] {
            let mut full = vec!["--provider-json", "p"];
            full.extend(list);
            let err = args(&full).expect_err("conflict");
            assert!(err.contains("--ocr-only") && err.contains("--samples"), "{err}");
        }

        // 只给 --samples 时不跑 OCR（与原来一致），再给 --ocr-samples 时跑
        assert!(
            !args(&["--provider-json", "p", "--samples", "S1"])
                .expect("a")
                .runs_ocr()
        );
        assert!(
            args(&["--provider-json", "p", "--samples", "S1", "--ocr-samples", "O5"])
                .expect("a")
                .runs_ocr()
        );

        let help = args(&["--help"]).expect_err("help");
        assert!(help.contains("--ocr-only") && help.contains("--ocr-samples"), "{help}");
    }

    #[test]
    fn ocr_plan_follows_selection() {
        assert_eq!(select_ocr_samples(&[]).len(), OCR_SAMPLES.len());
        let dir = std::env::temp_dir().join(format!("wisp-eval-ocr-plan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("chat.png"), png(40, 20)).expect("write");
        std::fs::write(dir.join("pdf.png"), png(40, 20)).expect("write");
        let selected = select_ocr_samples(&["O5-chat-paragraphs", "O6-pdf-paragraphs", "O7-list-paragraphs"]);
        let (cases, missing) = plan_ocr_cases(&dir, &selected).expect("plan");
        assert_eq!(
            cases.iter().map(|c| c.sample_id).collect::<Vec<_>>(),
            ["O5-chat-paragraphs", "O6-pdf-paragraphs"]
        );
        assert!(cases.iter().all(|c| c.task == "ocr:paragraphs" && c.image.is_some()));
        assert_eq!(missing.len(), 1);
        assert!(missing[0].starts_with("O7-list-paragraphs"), "{missing:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// dry run 的临时目录：写好 provider 文件与截图，返回目录
    fn dry_run_dir(name: &str, models: Value, screenshots: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wisp-eval-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("shots")).expect("dir");
        let provider = json!({
            "id": "vl",
            "protocol": "openai_chat",
            "base_url": "http://127.0.0.1:9/v1",
            "headers": {"X-Secret": "header-value-must-not-leak"},
            "models": models
        });
        std::fs::write(dir.join("p.json"), provider.to_string()).expect("write");
        for file in screenshots {
            std::fs::write(dir.join("shots").join(file), png(40, 20)).expect("write");
        }
        dir
    }

    fn dry_run_args(dir: &Path, extra: &[&str]) -> EvalArgs {
        let mut list = vec![
            "--provider-json".to_owned(),
            dir.join("p.json").display().to_string(),
            "--out".to_owned(),
            dir.join("out").display().to_string(),
            "--ocr-dir".to_owned(),
            dir.join("shots").display().to_string(),
            "--dry-run".to_owned(),
        ];
        list.extend(extra.iter().map(|s| (*s).to_owned()));
        parse_args(list).expect("args")
    }

    /// 每个 `.dry-run.jsonl` 的（文件名，各行的 sample · task）
    fn dry_run_plans(dir: &Path) -> Vec<(String, Vec<String>)> {
        let mut plans: Vec<(String, Vec<String>)> = std::fs::read_dir(dir.join("out"))
            .expect("out")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().ends_with(".dry-run.jsonl"))
            .map(|p| {
                let text = std::fs::read_to_string(&p).expect("read");
                assert!(
                    !text.contains("header-value-must-not-leak"),
                    "请求头的值不进 dry run 文件"
                );
                let rows = text
                    .lines()
                    .map(|l| {
                        let v: Value = serde_json::from_str(l).expect("json");
                        format!(
                            "{} · {}",
                            v["sample"].as_str().unwrap_or(""),
                            v["task"].as_str().unwrap_or("")
                        )
                    })
                    .collect();
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (name, rows)
            })
            .collect();
        plans.sort();
        plans
    }

    #[tokio::test]
    async fn ocr_only_dry_run_plans_selected_screenshots() {
        let models = json!([{"id": "text-only", "vision": "no"}, {"id": "vl-max", "vision": "yes"}]);
        let dir = dry_run_dir("ocr-only", models, &["chat.png", "list.png"]);
        run(dry_run_args(&dir, &["--ocr-only", "--ocr-samples", "O5,O6,o7"]))
            .await
            .expect("dry run");
        let plans = dry_run_plans(&dir);
        // 不收图片的模型整个跳过；缺图的 O6 跳过；没有翻译样本
        assert_eq!(plans.len(), 1, "{plans:?}");
        assert!(plans[0].0.ends_with("-vl-vl-max.dry-run.jsonl"), "{plans:?}");
        assert_eq!(
            plans[0].1,
            [
                "O5-chat-paragraphs · ocr:paragraphs",
                "O7-list-paragraphs · ocr:paragraphs"
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ocr_only_without_screenshots_or_vision_fails() {
        // 选中的样本一张截图都没有：非零退出并给出图片目录与期望的文件名
        let dir = dry_run_dir("ocr-none", json!([{"id": "vl-max"}]), &["table.png"]);
        let err = run(dry_run_args(&dir, &["--ocr-only", "--ocr-samples", "O5,O6"]))
            .await
            .expect_err("no screenshots");
        assert!(err.contains(&dir.join("shots").display().to_string()), "{err}");
        assert!(err.contains("chat.png, pdf.png"), "{err}");
        assert!(dry_run_plans(&dir).is_empty(), "什么都不写");
        let _ = std::fs::remove_dir_all(&dir);

        // 没有一个模型能收图片
        let dir = dry_run_dir(
            "ocr-novision",
            json!([{"id": "text-only", "vision": "no"}]),
            &["chat.png"],
        );
        let err = run(dry_run_args(&dir, &["--ocr-only"])).await.expect_err("no vision");
        assert!(err.contains("accepts images"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn default_plan_is_unchanged_without_new_flags() {
        let dir = dry_run_dir("ocr-default", json!([{"id": "vl-max"}]), &["chat.png", "pdf.png"]);
        let styles = builtin_styles().len();

        // 只给 --samples：照旧不跑 OCR 样本
        run(dry_run_args(&dir, &["--samples", "S1"])).await.expect("dry run");
        assert_eq!(dry_run_plans(&dir)[0].1.len(), styles);

        // 再给 --ocr-samples：翻译样本之后加上选中的、有截图的 OCR 样本
        run(dry_run_args(&dir, &["--samples", "S1", "--ocr-samples", "O5,O7"]))
            .await
            .expect("dry run");
        let rows = &dry_run_plans(&dir)[0].1;
        assert_eq!(rows.len(), styles + 1);
        assert_eq!(
            rows.last().map(String::as_str),
            Some("O5-chat-paragraphs · ocr:paragraphs")
        );

        // 什么都不给：全部翻译样本 + 有截图的全部 OCR 样本，缺图的跳过而不报错
        run(dry_run_args(&dir, &[])).await.expect("dry run");
        let rows = &dry_run_plans(&dir)[0].1;
        assert_eq!(rows.len(), 15 * styles + 2 + 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn check(name: &str, kind: CheckKind, passed: Option<bool>) -> Check {
        Check {
            name: name.to_owned(),
            kind,
            passed,
        }
    }

    fn outcome(case: Case, elapsed_ms: u128, error: Option<&str>, checks: Vec<Check>) -> Outcome {
        Outcome {
            case,
            raw: "model output".to_owned(),
            output: "model output".to_owned(),
            no_text: false,
            elapsed_ms,
            truncated: false,
            error: error.map(str::to_owned),
            checks,
        }
    }

    #[test]
    fn summary_lists_each_sample() {
        let ocr = |id: &str| {
            let sample = OCR_SAMPLES.iter().find(|o| o.id == id).expect("sample");
            ocr_case(sample, Arc::new(Vec::new()))
        };
        let styles: Vec<Style> = builtin_styles()
            .into_iter()
            .filter(|s| s.id == "natural" || s.id == "technical")
            .collect();
        let s1 = plan_text_cases(&styles, &SAMPLES[..1], false);
        let outcomes = vec![
            outcome(
                ocr("O5-chat-paragraphs"),
                8_234,
                None,
                vec![
                    check("有文字", CheckKind::Auto, Some(true)),
                    check("消息不空行", CheckKind::Auto, Some(true)),
                    check("期望", CheckKind::Manual, None),
                ],
            ),
            outcome(
                ocr("O6-pdf-paragraphs"),
                12_050,
                None,
                vec![
                    check("有文字", CheckKind::Auto, Some(true)),
                    check("断词还原", CheckKind::Auto, Some(false)),
                    check("辅助", CheckKind::Aux, Some(false)),
                ],
            ),
            outcome(ocr("O7-list-paragraphs"), 0, Some("Timeout: timed\nout"), Vec::new()),
            outcome(
                s1[0].clone(),
                1_000,
                None,
                vec![check("A", CheckKind::Auto, Some(true))],
            ),
            outcome(
                s1[1].clone(),
                1_500,
                None,
                vec![check("B", CheckKind::Auto, Some(false))],
            ),
        ];
        let missing = vec!["O8-mixed-paragraphs (shots/mixed.png)".to_owned()];
        let text = format_model_summary("vl / vl-max", Path::new("out/r.md"), &outcomes, &missing);
        let want = [
            "vl / vl-max  →  out/r.md",
            "  O5-chat-paragraphs  ocr:paragraphs   8.2s  pass  auto 2/2",
            "  O6-pdf-paragraphs   ocr:paragraphs  12.0s  FAIL  auto 1/2  failed: 断词还原",
            "  O7-list-paragraphs  ocr:paragraphs   0.0s  ERROR auto 0/0  error: Timeout: timed out",
            "  S1                  2 tasks          2.5s  FAIL  auto 1/2  failed: technical · B",
            "  skipped (no screenshot): O8-mixed-paragraphs (shots/mixed.png)",
            "  total: auto 4/6 passed, aux 0/1, 1 error(s), 1 manual item(s)",
            "",
        ];
        assert_eq!(text.lines().collect::<Vec<_>>(), want, "{text}");
        assert!(!text.contains("model output"), "汇总不贴模型输出");

        // 没有自动断言的样本标 n/a
        let only_manual = outcome(
            ocr("O8-mixed-paragraphs"),
            99,
            None,
            vec![check("期望", CheckKind::Manual, None)],
        );
        assert!(format_model_summary("m", Path::new("r.md"), &[only_manual], &[]).contains("0.0s  n/a   auto 0/0"));
    }

    #[test]
    fn plan_lists_tasks_per_sample() {
        let styles: Vec<Style> = builtin_styles().into_iter().filter(|s| s.id == "natural").collect();
        let mut cases = plan_text_cases(&styles, &SAMPLES[13..14], false);
        let o5 = OCR_SAMPLES
            .iter()
            .find(|o| o.id == "O5-chat-paragraphs")
            .expect("sample");
        cases.push(ocr_case(o5, Arc::new(Vec::new())));
        let missing = ["O6-pdf-paragraphs (shots/pdf.png)".to_owned()];
        let text = format_plan("vl / vl-max", Path::new("out/p.dry-run.jsonl"), &cases, &missing);
        assert_eq!(
            text.lines().collect::<Vec<_>>(),
            [
                "vl / vl-max: 3 case(s)  →  out/p.dry-run.jsonl",
                "  S14                 polish, proofread",
                "  O5-chat-paragraphs  ocr:paragraphs (chat.png)",
                "  skipped (no screenshot): O6-pdf-paragraphs (shots/pdf.png)",
                "",
            ]
        );
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
