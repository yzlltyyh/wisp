//! PaddleOCR-VL-1.6 的文档流水线：版面检测 → 阅读顺序 → 逐区域识别 → 拼装（照 PaddleX 的 `pipeline.py` 移植）。
//!
//! - **小图快路径**：短边 < 400 px、或面积 < 0.4 MP、或版面只检出不超过 1 个文字类区域（且没有表格、公式）时，整图一次
//!   `OCR:`，输出边收边发。
//! - **否则**：按类别选提示词——文字类 [`PROMPT_OCR`]、表格 [`PROMPT_TABLE`]（OTSL → Markdown 表格）、公式类
//!   [`PROMPT_FORMULA`]（先裁到墨迹，输出 `$$…$$`）、图表 [`PROMPT_CHART`]（默认关闭，与官方一致）、印章
//!   [`PROMPT_SEAL`]（默认关）；页眉、页脚、页码等按官方的 `markdown_ignore_labels` 跳过；图片区域在 markdown 模式输出
//!   占位（不嵌入图片）。lines / paragraphs 模式不要标题标记与占位，表格按行展开为制表符分隔的文本。
//! - **请求**：区域裁成 PNG，先图后文，`temperature 0`，区域 `max_tokens 4096`；输出做重复截断（PaddleX 的
//!   `truncate_repetitive_content`）；单个区域失败（例如个别字符引起服务端 500）跳过并继续，不让整页失败，但在结果之后发
//!   `Notice{partial}`，界面提示「部分区域未识别」；所有区域都失败时报最后一个错误。所有区域都「成功」但全是空结果时：
//!   用着 GPU 加速就报 `gpu_failed`（某些显卡后端加载成功却不出任何 token），提示可以关闭它；没用 GPU 时就是没有文字。
//! - **流式**：每完成一个区域就发一段 Delta（区域之间空一行）；区域之间检查取消。
//!
//! 识别经 [`VlmBackend`]（desktop 用 wisp-llm 访问 vl 槽位的 llama-server 实现），版面经 [`LayoutDetector`]。

use std::sync::Arc;

use image::RgbaImage;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wisp_types::{ErrorCode, LlmError, NoticeKind, OcrMode, StreamEvent};

use super::layout::{LayoutBox, LayoutDetector};
use crate::error::{LocalError, RuntimeFailure};
use crate::stream::EngineSink;

/// 文字类区域的提示词
pub const PROMPT_OCR: &str = "OCR:";
/// 表格区域的提示词（输出 OTSL）
pub const PROMPT_TABLE: &str = "Table Recognition:";
/// 公式区域的提示词
pub const PROMPT_FORMULA: &str = "Formula Recognition:";
/// 图表区域的提示词（默认不识别图表）
pub const PROMPT_CHART: &str = "Chart Recognition:";
/// 印章区域的提示词（默认不识别印章）
pub const PROMPT_SEAL: &str = "Seal Recognition:";
/// 单个区域的最大输出
pub const REGION_MAX_TOKENS: u32 = 4096;
/// 小图快路径：短边低于这个值
pub const SMALL_SHORT_EDGE: u32 = 400;
/// 小图快路径：面积低于这个值（像素）
pub const SMALL_AREA: u64 = 400_000;

/// 不送识别、markdown 里只放占位的图片类区域（PaddleX 的 `IMAGE_LABELS`；不识别图表时图表也算）
const IMAGE_LABELS: [&str; 3] = ["image", "header_image", "footer_image"];
/// 不进结果的区域（官方 PaddleOCR-VL-1.6 配置的 `markdown_ignore_labels`）
const IGNORE_LABELS: [&str; 7] = [
    "number",
    "footnote",
    "header",
    "header_image",
    "footer",
    "footer_image",
    "aside_text",
];
/// 边长不足这个值的区域不送识别（裁出来什么也看不清，还白占一次请求）
const MIN_REGION_SIDE: u32 = 8;

/// 一次识别请求（一张整图或一个区域）
#[derive(Debug, Clone)]
pub struct VlmRequest {
    /// PNG 编码的图片
    pub png: Arc<Vec<u8>>,
    /// 任务提示词（上面的 `PROMPT_*` 之一）
    pub prompt: &'static str,
    pub max_tokens: u32,
}

/// 视觉语言模型的调用方式（desktop 用 wisp-llm 访问 vl 槽位实现；冷启动由实现负责）
pub trait VlmBackend: Send + Sync {
    /// 发起一次识别，返回与 `LlmClient::stream` 同语义的事件流（Start → Delta* → Done | Error）
    fn recognize(&self, req: VlmRequest, cancel: CancellationToken) -> mpsc::Receiver<StreamEvent>;
}

/// 流水线的选项
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipelineOptions {
    pub mode: OcrMode,
    /// 识别图表（默认关）
    pub charts: bool,
    /// 识别印章（默认关）
    pub seals: bool,
    /// 识别进程用着 GPU 加速（默认是）。全部区域都是空结果时，用着 GPU 才归为显卡后端出错、提示关闭 GPU 加速
    pub gpu: bool,
}

impl PipelineOptions {
    /// 某种模式的默认选项
    pub const fn for_mode(mode: OcrMode) -> Self {
        Self {
            mode,
            charts: false,
            seals: false,
            gpu: true,
        }
    }

    /// 识别进程是否用着 GPU 加速（设置里的「使用 GPU 加速」且本机有可用的显卡）
    #[must_use]
    pub const fn with_gpu(mut self, gpu: bool) -> Self {
        self.gpu = gpu;
        self
    }

    fn markdown(&self) -> bool {
        matches!(self.mode, OcrMode::Markdown)
    }
}

/// 一个区域怎么处理
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// 送识别
    Recognize(&'static str),
    /// markdown 模式放占位
    Placeholder(&'static str),
    /// 跳过
    Skip,
}

fn action(label: &str, opts: &PipelineOptions) -> Action {
    if IGNORE_LABELS.contains(&label) {
        return Action::Skip;
    }
    if IMAGE_LABELS.contains(&label) {
        return Action::Placeholder("[image]");
    }
    match label {
        "table" => Action::Recognize(PROMPT_TABLE),
        "chart" if opts.charts => Action::Recognize(PROMPT_CHART),
        "chart" => Action::Placeholder("[chart]"),
        "seal" if opts.seals => Action::Recognize(PROMPT_SEAL),
        "seal" => Action::Skip,
        l if l.contains("formula") && l != "formula_number" => Action::Recognize(PROMPT_FORMULA),
        _ => Action::Recognize(PROMPT_OCR),
    }
}

/// 运行流水线：把结果以 Delta 写进 `sink`（Done / Error 由 [`crate::stream::spawn`] 负责）
pub async fn run<L: LayoutDetector, V: VlmBackend>(
    image: Arc<RgbaImage>,
    opts: PipelineOptions,
    layout: &L,
    vlm: &V,
    sink: &EngineSink,
    cancel: &CancellationToken,
) -> Result<(), LlmError> {
    let (w, h) = image.dimensions();
    if w == 0 || h == 0 {
        return Ok(());
    }
    if w.min(h) < SMALL_SHORT_EDGE || u64::from(w) * u64::from(h) < SMALL_AREA {
        return whole_image(image, vlm, sink, cancel).await;
    }

    let boxes = layout
        .detect(Arc::clone(&image), cancel.clone())
        .await
        .map_err(|e| e.to_llm_error())?;
    let regions: Vec<(LayoutBox, &'static str)> = boxes
        .into_iter()
        .filter_map(|b| match action(&b.label, &opts) {
            Action::Recognize(p) => Some((b, p)),
            Action::Placeholder(text) if opts.markdown() => Some((b, text)),
            Action::Placeholder(_) | Action::Skip => None,
        })
        .collect();
    let ocr_regions = regions.iter().filter(|(_, p)| *p == PROMPT_OCR).count();
    let structured = regions
        .iter()
        .any(|(_, p)| matches!(*p, PROMPT_TABLE | PROMPT_FORMULA | PROMPT_CHART | PROMPT_SEAL));
    if ocr_regions <= 1 && !structured {
        return whole_image(image, vlm, sink, cancel).await;
    }

    let mut first = true;
    let mut recognized = 0usize;
    let mut non_empty = 0usize;
    let mut failed = 0usize;
    let mut last_error: Option<LlmError> = None;
    for (b, prompt) in regions {
        if cancel.is_cancelled() {
            return Err(LocalError::Cancelled.to_llm_error());
        }
        let text = if prompt.starts_with('[') {
            prompt.to_owned()
        } else {
            let Some(png) = crop_png(&image, &b, prompt).await else {
                continue;
            };
            let req = VlmRequest {
                png: Arc::new(png),
                prompt,
                max_tokens: REGION_MAX_TOKENS,
            };
            match collect(vlm.recognize(req, cancel.clone()), cancel).await {
                Ok(raw) => {
                    recognized += 1;
                    let text = format_region(&b.label, prompt, &raw, &opts);
                    if !text.trim().is_empty() {
                        non_empty += 1;
                    }
                    text
                }
                Err(e) if e.code == ErrorCode::Cancelled => return Err(e),
                Err(e) => {
                    // 单个区域失败不让整页失败（错误内容不记日志：可能带服务端回显的文字）
                    log::debug!("local: VL region skipped: {:?}", e.code);
                    failed += 1;
                    last_error = Some(e);
                    continue;
                }
            }
        };
        let text = text.trim_matches('\n');
        if text.trim().is_empty() {
            continue;
        }
        let chunk = if first { text.to_owned() } else { format!("\n\n{text}") };
        first = false;
        if !sink.delta(chunk).await {
            return Ok(());
        }
    }
    finish(recognized, non_empty, failed, last_error, opts.gpu, sink).await
}

/// 逐区域识别之后的结论（见模块说明）
async fn finish(
    recognized: usize,
    non_empty: usize,
    failed: usize,
    last_error: Option<LlmError>,
    gpu: bool,
    sink: &EngineSink,
) -> Result<(), LlmError> {
    if non_empty == 0 {
        // 没有一个区域给出文字。识别「成功」却全空：用着 GPU 时是显卡后端的问题（提示关闭 GPU 加速）；没用 GPU 时
        // 有失败就报失败，否则就是没有文字
        if recognized > 0 && gpu {
            return Err(RuntimeFailure::GpuFailed.to_llm_error());
        }
        if let Some(e) = last_error {
            return Err(e);
        }
        return Ok(());
    }
    if failed > 0 {
        sink.notice(NoticeKind::Partial).await;
    }
    Ok(())
}

/// 整图一次 `OCR:`，边收边发
async fn whole_image<V: VlmBackend>(
    image: Arc<RgbaImage>,
    vlm: &V,
    sink: &EngineSink,
    cancel: &CancellationToken,
) -> Result<(), LlmError> {
    let png = tokio::task::spawn_blocking(move || encode_png(&image))
        .await
        .ok()
        .flatten()
        .ok_or_else(|| LocalError::Protocol("cannot encode the image").to_llm_error())?;
    let req = VlmRequest {
        png: Arc::new(png),
        prompt: PROMPT_OCR,
        max_tokens: REGION_MAX_TOKENS,
    };
    let mut rx = vlm.recognize(req, cancel.clone());
    loop {
        let ev = tokio::select! {
            () = cancel.cancelled() => return Err(LocalError::Cancelled.to_llm_error()),
            ev = rx.recv() => ev,
        };
        match ev {
            Some(StreamEvent::Delta { text }) => {
                if !sink.delta(text).await {
                    return Ok(());
                }
            }
            Some(StreamEvent::Done { .. }) => return Ok(()),
            Some(StreamEvent::Error(e)) => return Err(e),
            Some(_) => {}
            None => return Err(LocalError::Protocol("the recognition stream ended early").to_llm_error()),
        }
    }
}

/// 收完一个区域的输出
async fn collect(mut rx: mpsc::Receiver<StreamEvent>, cancel: &CancellationToken) -> Result<String, LlmError> {
    let mut out = String::new();
    loop {
        let ev = tokio::select! {
            () = cancel.cancelled() => return Err(LocalError::Cancelled.to_llm_error()),
            ev = rx.recv() => ev,
        };
        match ev {
            Some(StreamEvent::Delta { text }) => out.push_str(&text),
            Some(StreamEvent::Done { .. }) => return Ok(out),
            Some(StreamEvent::Error(e)) => return Err(e),
            Some(_) => {}
            None => return Err(LocalError::Protocol("the recognition stream ended early").to_llm_error()),
        }
    }
}

/// 裁出区域并编成 PNG（阻塞部分放进阻塞线程）。公式先裁到墨迹。区域太小或编码失败时为 None
async fn crop_png(image: &Arc<RgbaImage>, b: &LayoutBox, prompt: &'static str) -> Option<Vec<u8>> {
    let (w, h) = image.dimensions();
    let [x1, y1, x2, y2] = b.bbox;
    let x1 = (x1.max(0.0) as u32).min(w);
    let y1 = (y1.max(0.0) as u32).min(h);
    let x2 = (x2.max(0.0).ceil() as u32).min(w);
    let y2 = (y2.max(0.0).ceil() as u32).min(h);
    if x2.saturating_sub(x1) < MIN_REGION_SIDE || y2.saturating_sub(y1) < MIN_REGION_SIDE {
        return None;
    }
    let image = Arc::clone(image);
    tokio::task::spawn_blocking(move || {
        let mut region = image::imageops::crop_imm(&*image, x1, y1, x2 - x1, y2 - y1).to_image();
        if prompt == PROMPT_FORMULA {
            region = crop_margin(region);
        }
        encode_png(&region)
    })
    .await
    .ok()
    .flatten()
}

fn encode_png(image: &RgbaImage) -> Option<Vec<u8>> {
    use image::ImageEncoder;
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgba8,
        )
        .ok()?;
    Some(out)
}

/// 裁掉公式四周的空白（PaddleX 的 `crop_margin`）：灰度按最小 / 最大值拉伸到 0–255，不超过 200 的算墨迹，取墨迹的
/// 外接矩形。整块同色、没有墨迹或裁完不足 3 像素时原样返回
fn crop_margin(img: RgbaImage) -> RgbaImage {
    let gray: Vec<u8> = img
        .pixels()
        .map(|p| {
            let [r, g, b, _] = p.0;
            // ITU-R BT.601（与 OpenCV 的 COLOR_BGR2GRAY 相同）
            ((u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114) / 1000) as u8
        })
        .collect();
    let (Some(&min), Some(&max)) = (gray.iter().min(), gray.iter().max()) else {
        return img;
    };
    if min == max {
        return img;
    }
    let (w, h) = img.dimensions();
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
    for (i, &v) in gray.iter().enumerate() {
        let norm = u32::from(v - min) * 255 / u32::from(max - min);
        if norm <= 200 {
            let i = i as u32;
            let (x, y) = (i % w, i / w);
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    if x0 > x1 || y0 > y1 {
        return img;
    }
    let (cw, ch) = (x1 - x0 + 1, y1 - y0 + 1);
    if cw <= 2 || ch <= 2 || (cw == w && ch == h) {
        return img;
    }
    image::imageops::crop_imm(&img, x0, y0, cw, ch).to_image()
}

/// 一个区域的识别结果 → 输出文字（重复截断、LaTeX 定界符、表格、标题标记）
fn format_region(label: &str, prompt: &'static str, raw: &str, opts: &PipelineOptions) -> String {
    let min_count = if prompt == PROMPT_TABLE { 5000 } else { 50 };
    let mut text = truncate_repetitive(raw, min_count);
    text = normalize_latex_delimiters(&text, label);
    if prompt == PROMPT_TABLE {
        let table = if opts.markdown() {
            super::otsl::to_markdown(&text)
        } else {
            super::otsl::to_tsv(&text)
        };
        return table.unwrap_or(text);
    }
    if prompt == PROMPT_FORMULA {
        let t = text.trim();
        if opts.markdown() && !t.contains('$') && !t.is_empty() {
            return format!("$$\n{t}\n$$");
        }
        return t.to_owned();
    }
    if !opts.markdown() {
        return text;
    }
    match label {
        "doc_title" => format!("# {}", collapse_newlines(&text)),
        "paragraph_title" => {
            let title = collapse_newlines(&text);
            format!("{} {title}", "#".repeat(title_level(&title) + 1))
        }
        // 每一行在 Markdown 里各成一段（PaddleX 的 `_format_normalize_newlines`）
        "text" | "vertical_text" | "reference_content" | "vision_footnote" => {
            text.replace("\n\n", "\n").replace('\n', "\n\n")
        }
        _ => text,
    }
}

/// 标题级别：「1.2.3 标题」这类编号按点数加深，其余为一级（PaddleX 的 `format_title`）
fn title_level(title: &str) -> usize {
    let numbering: String = title.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let numbering = numbering.trim_end_matches('.');
    if numbering.is_empty() || !numbering.starts_with(|c: char| c.is_ascii_digit()) {
        return 1;
    }
    (numbering.matches('.').count() + 1).min(5)
}

/// 折成几行的标题合成一行，规则与「合并为一行」相同（[`wisp_text::lines::join_all`]）：断词还原，`2020-2021`、
/// `COVID-19` 这类本来就有的连字符保留；东亚文字之间直接相连，中英之间按标题里已有的写法
fn collapse_newlines(s: &str) -> String {
    wisp_text::lines::join_all(s)
}

/// 模型用 `\( \)` / `\[ \]` 写公式时换成 `$ … $` / `$$ … $$`（PaddleX 的同名处理）；公式编号去掉 `$`
fn normalize_latex_delimiters(s: &str, label: &str) -> String {
    let has_inline = s.contains("\\(") && s.contains("\\)");
    let has_display = s.contains("\\[") && s.contains("\\]");
    if !has_inline && !has_display {
        return s.to_owned();
    }
    let mut out = s
        .replace('$', "")
        .replace("\\(", " $ ")
        .replace("\\)", " $")
        .replace("\\[\\[", "\\[")
        .replace("\\]\\]", "\\]")
        .replace("\\[", " $$ ")
        .replace("\\]", " $$ ");
    if label == "formula_number" {
        out = out.replace('$', "");
    }
    out
}

/// 截断模型的重复输出（PaddleX 的 `truncate_repetitive_content`）：短于 `min_count` 个字符的不处理；单行长文本以
/// 同一短语重复结尾且重复部分占一半以上时去掉重复；整行是同一子串的重复时只留一份；同一行重复出现占八成以上时只留一行
pub fn truncate_repetitive(content: &str, min_count: usize) -> String {
    let len = content.chars().count();
    if len < min_count {
        return content.to_owned();
    }
    let stripped = content.trim();
    if stripped.is_empty() {
        return content.to_owned();
    }
    let chars: Vec<char> = stripped.chars().collect();
    let single_line = !stripped.contains('\n');
    if single_line
        && chars.len() > 100
        && let Some((prefix_len, unit_len, count)) = repeating_suffix(&chars, 8, 5)
        && unit_len * count * 2 > chars.len()
    {
        return chars[..prefix_len].iter().collect();
    }
    if single_line
        && chars.len() > 10
        && let Some(unit) = shortest_repeating_unit(&chars)
        && chars.len() / unit >= 10
    {
        return chars[..unit].iter().collect();
    }
    let lines: Vec<&str> = content.split('\n').map(str::trim).filter(|l| !l.is_empty()).collect();
    if lines.len() >= 10 {
        let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for l in &lines {
            *counts.entry(l).or_default() += 1;
        }
        // 计数相同时取先出现的一行（与 Python Counter 的 most_common 一致）
        let mut best: Option<(&str, usize)> = None;
        for l in &lines {
            let c = counts[l];
            if best.is_none_or(|(_, bc)| c > bc) {
                best = Some((l, c));
            }
        }
        if let Some((line, count)) = best
            && count >= 10
            && count * 5 >= lines.len() * 4
        {
            return line.to_owned();
        }
    }
    content.to_owned()
}

/// 以至少 `min_repeats` 次重复的单元结尾（单元至少 `min_len` 个字符，从长到短找）：返回（前缀长度，单元长度，重复次数）
fn repeating_suffix(s: &[char], min_len: usize, min_repeats: usize) -> Option<(usize, usize, usize)> {
    let n = s.len();
    let max_unit = n / min_repeats;
    for unit in (min_len..=max_unit).rev() {
        let tail = &s[n - unit..];
        let ends_with_repeats = (0..min_repeats).all(|k| &s[n - unit * (k + 1)..n - unit * k] == tail);
        if ends_with_repeats {
            let mut count = 0;
            while unit * (count + 1) <= n && &s[n - unit * (count + 1)..n - unit * count] == tail {
                count += 1;
            }
            return Some((n - unit * count, unit, count));
        }
    }
    None
}

/// 整串由某个子串重复而成时，最短子串的长度
fn shortest_repeating_unit(s: &[char]) -> Option<usize> {
    let n = s.len();
    (1..=n / 2).find(|&i| n.is_multiple_of(i) && s.chunks(i).all(|c| c == &s[..i]))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use parking_lot::Mutex;

    use super::*;

    struct FakeLayout(Vec<LayoutBox>);

    impl LayoutDetector for FakeLayout {
        async fn detect(&self, _: Arc<RgbaImage>, _: CancellationToken) -> Result<Vec<LayoutBox>, LocalError> {
            Ok(self.0.clone())
        }
    }

    /// 假的 VLM：按提示词与请求次序给出脚本化的回答；`Err` 表示这次请求失败
    struct FakeVlm {
        answers: Mutex<Vec<Result<String, ErrorCode>>>,
        prompts: Mutex<Vec<&'static str>>,
        calls: AtomicUsize,
    }

    impl FakeVlm {
        fn new(answers: Vec<Result<&str, ErrorCode>>) -> Self {
            Self {
                answers: Mutex::new(answers.into_iter().rev().map(|a| a.map(str::to_owned)).collect()),
                prompts: Mutex::new(Vec::new()),
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl VlmBackend for FakeVlm {
        fn recognize(&self, req: VlmRequest, _: CancellationToken) -> mpsc::Receiver<StreamEvent> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.prompts.lock().push(req.prompt);
            assert!(req.png.starts_with(b"\x89PNG"), "区域编码成 PNG");
            let answer = self.answers.lock().pop().unwrap_or(Ok(String::new()));
            let (tx, rx) = mpsc::channel(8);
            tokio::spawn(async move {
                match answer {
                    Ok(text) => {
                        // 分两段发，验证收齐
                        let mid = text.char_indices().nth(text.chars().count() / 2).map_or(0, |(i, _)| i);
                        for part in [&text[..mid], &text[mid..]] {
                            let _ = tx.send(StreamEvent::Delta { text: part.to_owned() }).await;
                        }
                        let _ = tx
                            .send(StreamEvent::Done {
                                elapsed_ms: 1,
                                usage: None,
                                truncated: None,
                                cached: false,
                                no_text: false,
                            })
                            .await;
                    }
                    Err(code) => {
                        let _ = tx.send(StreamEvent::Error(LlmError::new(code, "fake"))).await;
                    }
                }
            });
            rx
        }
    }

    fn region(label: &str, bbox: [f32; 4], order: Option<u32>) -> LayoutBox {
        LayoutBox {
            label: label.to_owned(),
            score: 0.9,
            bbox,
            order,
        }
    }

    async fn run_collect(
        image: RgbaImage,
        opts: PipelineOptions,
        layout: &Arc<FakeLayout>,
        vlm: &Arc<FakeVlm>,
    ) -> (Vec<StreamEvent>, Option<LlmError>) {
        let image = Arc::new(image);
        let (layout, vlm) = (Arc::clone(layout), Arc::clone(vlm));
        let mut rx = crate::stream::spawn(CancellationToken::new(), move |sink, cancel| async move {
            run(image, opts, &*layout, &*vlm, &sink, &cancel).await
        });
        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev);
        }
        let err = events.iter().find_map(|e| match e {
            StreamEvent::Error(e) => Some(e.clone()),
            _ => None,
        });
        (events, err)
    }

    fn text_of(events: &[StreamEvent]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn page() -> RgbaImage {
        RgbaImage::from_pixel(1000, 800, image::Rgba([255, 255, 255, 255]))
    }

    #[tokio::test]
    async fn small_images_take_the_whole_image_path() {
        let layout = Arc::new(FakeLayout(vec![region("text", [0.0, 0.0, 10.0, 10.0], Some(1))]));
        let vlm = Arc::new(FakeVlm::new(vec![Ok("Hello Wisp")]));
        let (events, err) = run_collect(
            RgbaImage::new(300, 200),
            PipelineOptions::for_mode(OcrMode::Lines),
            &layout,
            &vlm,
        )
        .await;
        assert!(err.is_none());
        assert_eq!(text_of(&events), "Hello Wisp");
    }

    #[tokio::test]
    async fn one_text_region_takes_the_whole_image_path() {
        let layout = Arc::new(FakeLayout(vec![
            region("header", [0.0, 0.0, 900.0, 30.0], None),
            region("text", [10.0, 50.0, 900.0, 700.0], Some(1)),
        ]));
        let vlm = Arc::new(FakeVlm::new(vec![Ok("整页文字")]));
        let (events, _) = run_collect(page(), PipelineOptions::for_mode(OcrMode::Paragraphs), &layout, &vlm).await;
        assert_eq!(text_of(&events), "整页文字");
    }

    #[tokio::test]
    async fn markdown_document_with_titles_tables_formulas_and_images() {
        let layout = Arc::new(FakeLayout(vec![
            region("doc_title", [10.0, 10.0, 900.0, 60.0], Some(1)),
            region("header", [10.0, 0.0, 300.0, 9.0], None),
            region("paragraph_title", [10.0, 80.0, 600.0, 110.0], Some(2)),
            region("text", [10.0, 120.0, 900.0, 300.0], Some(3)),
            region("table", [10.0, 320.0, 900.0, 500.0], None),
            region("display_formula", [100.0, 520.0, 800.0, 600.0], Some(4)),
            region("image", [10.0, 620.0, 400.0, 790.0], None),
        ]));
        let vlm = Arc::new(FakeVlm::new(vec![
            Ok("Annual Report"),
            Ok("1.2 Results"),
            Ok("Line one\nLine two"),
            Ok("<fcel>A<fcel>B<nl><fcel>1<fcel>2<nl>"),
            Ok("E = mc^2"),
        ]));
        let (events, err) = run_collect(page(), PipelineOptions::for_mode(OcrMode::Markdown), &layout, &vlm).await;
        assert!(err.is_none(), "{err:?}");
        assert_eq!(
            text_of(&events),
            "# Annual Report\n\n### 1.2 Results\n\nLine one\n\nLine two\n\n| A | B |\n| --- | --- |\n| 1 | 2 |\n\n$$\nE = mc^2\n$$\n\n[image]"
        );
        // 每个区域一段 Delta：标题、小标题、正文、表格、公式、图片占位
        assert_eq!(
            events.iter().filter(|e| matches!(e, StreamEvent::Delta { .. })).count(),
            6
        );
    }

    #[tokio::test]
    async fn plain_modes_drop_markup_and_expand_tables() {
        let layout = Arc::new(FakeLayout(vec![
            region("paragraph_title", [10.0, 80.0, 600.0, 110.0], Some(1)),
            region("text", [10.0, 120.0, 900.0, 300.0], Some(2)),
            region("table", [10.0, 320.0, 900.0, 500.0], None),
            region("image", [10.0, 620.0, 400.0, 790.0], None),
        ]));
        let vlm = Arc::new(FakeVlm::new(vec![
            Ok("Results"),
            Ok("Body"),
            Ok("<fcel>A<fcel>B<nl><fcel>1<fcel>2<nl>"),
        ]));
        let (events, _) = run_collect(page(), PipelineOptions::for_mode(OcrMode::Lines), &layout, &vlm).await;
        assert_eq!(text_of(&events), "Results\n\nBody\n\nA\tB\n1\t2");
    }

    #[tokio::test]
    async fn failed_regions_are_skipped_but_all_failures_surface() {
        let layout = Arc::new(FakeLayout(vec![
            region("text", [10.0, 10.0, 900.0, 300.0], Some(1)),
            region("text", [10.0, 320.0, 900.0, 600.0], Some(2)),
        ]));
        let vlm = Arc::new(FakeVlm::new(vec![Err(ErrorCode::Upstream), Ok("second")]));
        let (events, err) = run_collect(page(), PipelineOptions::for_mode(OcrMode::Lines), &layout, &vlm).await;
        assert!(err.is_none());
        assert_eq!(text_of(&events), "second");
        // 结果不完整：在 Done 之前提示一次
        let notices: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Notice { kind } => Some(*kind),
                _ => None,
            })
            .collect();
        assert_eq!(notices, [NoticeKind::Partial]);
        assert!(matches!(
            &events[events.len() - 2..],
            [StreamEvent::Notice { .. }, StreamEvent::Done { .. }]
        ));

        // 全部成功时没有提示
        let vlm = Arc::new(FakeVlm::new(vec![Ok("first"), Ok("second")]));
        let (events, _) = run_collect(page(), PipelineOptions::for_mode(OcrMode::Lines), &layout, &vlm).await;
        assert!(!events.iter().any(|e| matches!(e, StreamEvent::Notice { .. })));

        let vlm = Arc::new(FakeVlm::new(vec![Err(ErrorCode::Upstream), Err(ErrorCode::Upstream)]));
        let (_, err) = run_collect(page(), PipelineOptions::for_mode(OcrMode::Lines), &layout, &vlm).await;
        assert_eq!(err.map(|e| e.code), Some(ErrorCode::Upstream));
    }

    #[tokio::test]
    async fn all_empty_results_mean_the_gpu_backend_failed() {
        let layout = Arc::new(FakeLayout(vec![
            region("text", [10.0, 10.0, 900.0, 300.0], Some(1)),
            region("text", [10.0, 320.0, 900.0, 600.0], Some(2)),
        ]));
        let vlm = Arc::new(FakeVlm::new(vec![Ok(""), Ok("  ")]));
        let (_, err) = run_collect(page(), PipelineOptions::for_mode(OcrMode::Lines), &layout, &vlm).await;
        let err = err.expect("error");
        assert_eq!(err.code, ErrorCode::LocalRuntime);
        assert_eq!(err.hint.as_deref(), Some("errors.hint.local.gpu_failed"));

        // 没用 GPU 时不提示关闭 GPU 加速：全空就是没有文字
        let cpu = PipelineOptions::for_mode(OcrMode::Lines).with_gpu(false);
        let vlm = Arc::new(FakeVlm::new(vec![Ok(""), Ok("  ")]));
        let (events, err) = run_collect(page(), cpu, &layout, &vlm).await;
        assert!(err.is_none(), "{err:?}");
        assert!(text_of(&events).is_empty());
        assert!(matches!(events.last(), Some(StreamEvent::Done { .. })));
        // 没用 GPU、一个区域失败一个全空：报那次失败
        let vlm = Arc::new(FakeVlm::new(vec![Err(ErrorCode::Upstream), Ok("")]));
        let (_, err) = run_collect(page(), cpu, &layout, &vlm).await;
        assert_eq!(err.map(|e| e.code), Some(ErrorCode::Upstream));
    }

    #[tokio::test]
    async fn prompts_follow_labels_and_options() {
        let opts = PipelineOptions {
            mode: OcrMode::Markdown,
            charts: true,
            seals: false,
            gpu: true,
        };
        assert_eq!(action("chart", &opts), Action::Recognize(PROMPT_CHART));
        assert_eq!(
            action("chart", &PipelineOptions::for_mode(OcrMode::Markdown)),
            Action::Placeholder("[chart]")
        );
        assert_eq!(action("seal", &opts), Action::Skip);
        assert_eq!(action("inline_formula", &opts), Action::Recognize(PROMPT_FORMULA));
        assert_eq!(action("formula_number", &opts), Action::Recognize(PROMPT_OCR));
        assert_eq!(action("footer", &opts), Action::Skip);
        assert_eq!(action("header_image", &opts), Action::Skip);
        assert_eq!(action("figure_title", &opts), Action::Recognize(PROMPT_OCR));
    }

    #[tokio::test]
    async fn cancellation_stops_between_regions() {
        let layout = Arc::new(FakeLayout(vec![
            region("text", [10.0, 10.0, 900.0, 300.0], Some(1)),
            region("text", [10.0, 320.0, 900.0, 600.0], Some(2)),
        ]));
        let vlm = Arc::new(FakeVlm::new(vec![Ok("first"), Ok("second")]));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let (tx, mut rx) = mpsc::channel(4);
        let sink = crate::stream::EngineSink::for_test(tx);
        let r = run(
            Arc::new(page()),
            PipelineOptions::for_mode(OcrMode::Lines),
            &*layout,
            &*vlm,
            &sink,
            &cancel,
        )
        .await;
        assert_eq!(r.map_err(|e| e.code), Err(ErrorCode::Cancelled));
        assert_eq!(vlm.calls.load(Ordering::SeqCst), 0);
        drop(sink);
        assert!(rx.recv().await.is_none());
    }

    #[test]
    fn repetition_is_truncated() {
        let unit = "重复的短语abc";
        let looped = format!("开头的内容{}", unit.repeat(40));
        assert_eq!(truncate_repetitive(&looped, 50), "开头的内容");
        assert_eq!(truncate_repetitive(&"ab".repeat(40), 50), "ab");
        let lines = "same line\n".repeat(12);
        assert_eq!(truncate_repetitive(&lines, 50), "same line");
        assert_eq!(truncate_repetitive("short", 50), "short");
        let normal = "a normal paragraph of text that does not repeat itself at all and is long enough";
        assert_eq!(truncate_repetitive(normal, 50), normal);
    }

    #[test]
    fn latex_delimiters_and_titles() {
        assert_eq!(normalize_latex_delimiters("x \\(a\\) y", "text"), "x  $ a $ y");
        assert_eq!(
            normalize_latex_delimiters("\\[E=mc^2\\]", "display_formula").trim(),
            "$$ E=mc^2 $$"
        );
        assert_eq!(normalize_latex_delimiters("(\\(1\\))", "formula_number"), "(  1 )");
        assert_eq!(title_level("Introduction"), 1);
        assert_eq!(title_level("2 Methods"), 1);
        assert_eq!(title_level("2.1 Data"), 2);
        assert_eq!(title_level("2.1.3. Details"), 3);
    }

    #[test]
    fn wrapped_titles_are_joined_like_join_lines() {
        let md = PipelineOptions::for_mode(OcrMode::Markdown);
        let title = |label: &str, raw: &str| format_region(label, PROMPT_OCR, raw, &md);
        // 本来就有的连字符保留，断词还原
        assert_eq!(
            title("doc_title", "Annual Report 2020-\n2021"),
            "# Annual Report 2020-2021"
        );
        assert_eq!(title("doc_title", "Life after COVID-\n19"), "# Life after COVID-19");
        assert_eq!(title("doc_title", "Inter-\nnational Trade\n"), "# International Trade");
        // 东亚文字之间不加空格，西文之间补一个空格
        assert_eq!(title("paragraph_title", "第一章\n概述"), "## 第一章概述");
        assert_eq!(
            title("paragraph_title", "2.1 Results and\nDiscussion"),
            "### 2.1 Results and Discussion"
        );
        // 中英之间按这一行里已有的写法
        assert_eq!(
            title("doc_title", "使用 Wisp 识别\nPDF 文档"),
            "# 使用 Wisp 识别 PDF 文档"
        );
        // 只有 markdown 模式加标题标记、合成一行
        let lines = PipelineOptions::for_mode(OcrMode::Lines);
        assert_eq!(
            format_region("doc_title", PROMPT_OCR, "2020-\n2021", &lines),
            "2020-\n2021"
        );
    }

    #[test]
    fn formulas_are_cropped_to_ink() {
        let mut img = RgbaImage::from_pixel(100, 60, image::Rgba([255, 255, 255, 255]));
        for x in 30..70 {
            for y in 20..35 {
                img.put_pixel(x, y, image::Rgba([0, 0, 0, 255]));
            }
        }
        let cropped = crop_margin(img);
        assert_eq!(cropped.dimensions(), (40, 15));
        let blank = RgbaImage::from_pixel(10, 10, image::Rgba([255, 255, 255, 255]));
        assert_eq!(crop_margin(blank).dimensions(), (10, 10));
    }
}
