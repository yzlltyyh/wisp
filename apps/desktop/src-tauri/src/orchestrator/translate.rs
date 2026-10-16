//! 划词翻译流程：取词（工作线程）→ 未就绪检查 → 语言和方向 → 词典式判定 → 长文本确认 → 缓存 → 请求（与面板显示并行）→
//! 后处理 → 合并 → 通道；连续切换风格时只有最后一个请求在流式输出（同一宿主同一通道新请求取消旧请求）。
//! 前端发起的翻译 / 润色 / 校对 / 提升（`translate_start`）与 refine 两步法也在这里。
//!
//! **钥匙串不在关键路径上**：划词触发时（未就绪检查通过之后），只要读取不会弹系统对话框（Windows、或本进程已经成功
//! 读过这个 Provider 的 Key），立即在阻塞线程上预读，与取词并行；请求发出时多半已经读好（`engine::KeyFetch`）。可能
//! 弹框时（macOS 未签名构建更新后第一次访问、Linux keyring 被锁）不在触发的瞬间读，推迟到缓存未命中之后——长文本、
//! 空选区、缓存命中这些不发请求的路径不会因此弹框。送来的文本没有取词可重叠，同样在缓存未命中之后才读。「已发出」的
//! 日志在 `llm_stream` 返回之后打印（`request … dispatched trace=… at +N ms`，与 `perf trace=… shown_ms=…` 同一起点，
//! 可直接比较先后）。
//!
//! **长原文**：超过 [`HEAVY_TEXT_BYTES`] 时，语言检测、术语表匹配、缓存键与提示词装配都放进阻塞线程池（运行时只有
//! 2 个 worker）；`StartAck.width` 只用原文前缀计算，不复制整段原文。

use std::sync::Arc;
use std::time::{Duration, Instant};

use wisp_llm::{CancellationToken, Purpose, Secret};
use wisp_platform::SelectionOpts;
use wisp_prompts::GlossaryMatcher;
use wisp_types::{
    AppRef, DictMode, ErrorCode, LlmError, NotReady, NoticeKind, PanelPayload, PanelTransition, PanelView, RequestId,
    StartAck, StreamEvent, StreamPhase, TaskKind, TranslateAnchor, TranslatePayload, TranslateReq,
};

use super::RequestHost;
use super::engine::{
    Core, HEAVY_TEXT_BYTES, KeyFetch, KeySource, Source, Trigger, blocking, cpu, log_dispatched, not_ready_error,
    owner_of,
};
use super::fsm::{Event, Flow, Stage};
use super::not_ready;
use super::plan::{
    self, Resolved, TextInput, TextPlan, build_improve_prompt, build_reflect_prompt, build_text_prompt, chat_request,
    plan_text, resolve_style, text_cache_key,
};
use super::registry::{Lane, OpenSpec, Owner, PumpEnd, Sink, pump};
use crate::app::perf::PerfStage;
use crate::bus::TriggerOrigin;
use crate::error::IpcError;
use crate::windows::{AnchorSource, PanelShowRequest};

/// `view_width` 只看原文的前几百个显示单位（80–400 单位之间线性插值，之后封顶）；算 `StartAck.width` 时给它一段
/// 足够长的前缀（每个字符至少 1 个单位），不复制整段原文（最大 1 MB）
const WIDTH_PROBE_CHARS: usize = 2048;

/// 文本请求的参数
pub(super) struct TextJob {
    pub(super) id: RequestId,
    pub(super) token: CancellationToken,
    pub(super) plan: TextPlan,
    pub(super) resolved: Resolved,
    /// 原文（`Arc<String>`：交给阻塞线程池做 CPU 工作时不复制）
    pub(super) text: Arc<String>,
    pub(super) bypass_cache: bool,
    pub(super) draft: Option<String>,
    pub(super) run: Option<u64>,
    pub(super) trace: Option<u64>,
    /// 触发时刻（Rust 发起的请求；「已发出」日志的 +ms 起点）
    pub(super) started: Option<Instant>,
    /// 触发时开始预读的 Key
    pub(super) key: Option<KeyFetch>,
}

impl Core {
    // ------------------------------------------------------------------------------------------------
    // 翻译（划词 / 指定风格 / 送来的文本）

    pub(super) fn start_translate(self: &Arc<Self>, t: Trigger, source: Source, style: Option<String>) {
        let run = self.start_run(Flow::Translate, t.trace);
        let core = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            core.translate_flow(run, t.trace, t.started, t.origin, source, style)
                .await;
        });
    }

    pub(super) async fn translate_flow(
        self: Arc<Self>,
        run: u64,
        trace: u64,
        started: Instant,
        origin: TriggerOrigin,
        source: Source,
        style_id: Option<String>,
    ) {
        let settings = self.env.settings();
        let styles = self.env.styles();
        let style_id = style_id.unwrap_or_else(|| settings.translate.default_style.clone());
        let style = resolve_style(&styles, &style_id, &settings.translate.default_style);
        // 剪贴板条目 → 翻译：Rust 发起的交叉淡入，目标应用沿用剪贴板会话记下的那个
        let transition = if origin == TriggerOrigin::Clipboard {
            PanelTransition::Crossfade
        } else {
            PanelTransition::Enter
        };
        let offline = self.offline_index(&settings, None).await;
        let readiness = not_ready::check_text(&settings, &style, None, TaskKind::Translate, &self.keys(), &offline);
        // 划词且能发请求时预读 Key：与取词（10–110 ms）并行，钥匙串访问不落在「面板可见 → 首 token」之间。只在读取
        // 不会弹系统对话框时预读：可能弹框时推迟到缓存未命中之后，对话框不在按下快捷键的瞬间出现，长文本 / 空选区 /
        // 缓存命中也不会白弹一次。送来的文本（剪贴板条目、引导页、冒烟）没有取词可重叠（只差一次前台应用查询），
        // 不预读——缓存命中时就完全不访问钥匙串
        let mut key = match (&source, &readiness) {
            (Source::Selection, Ok(r)) => self.prefetch_key_if_quiet(&r.provider),
            _ => None,
        };
        // 被取代的 Overlay 拆完（焦点已还给原应用）、进行中的截图段做完，再读前台应用 / 取词
        self.screen.settled().await;

        let (text, anchor, prev_app, method) = match source {
            Source::Text(text) => {
                let prev = self.prev_app_for(origin).await;
                if text.trim().is_empty() {
                    if self.advance(run, Event::SelectionMissed).is_some() {
                        self.open_input(run, trace, prev, None).await;
                    }
                    return;
                }
                if self.advance(run, Event::Selected).is_none() {
                    return;
                }
                (text, AnchorSource::Cursor, prev, None)
            }
            Source::Selection => {
                if let Err(nr) = &readiness {
                    // 没配置好模型：不取词（避免无谓的模拟复制），直接在鼠标处显示未就绪卡片
                    if self.advance(run, Event::NotReady).is_some() {
                        let prev = self.prev_app_for(origin).await;
                        let payload =
                            translate_payload(String::new(), None, &style.id, &settings, None, Some(nr.clone()), None);
                        self.show(
                            run,
                            trace,
                            translate_request(payload, AnchorSource::Cursor, prev, transition),
                            None,
                            None,
                        )
                        .await;
                    }
                    return;
                }
                if self.advance(run, Event::Begin).is_none() {
                    return;
                }
                let opts = SelectionOpts {
                    allow_copy_fallback: true,
                    copy_wait: Duration::from_millis(u64::from(settings.hotkeys.copy_wait_ms.clamp(50, 500))),
                    target: None,
                    history_enabled: false,
                };
                let env = Arc::clone(&self.env);
                let Some((prev, result)) = blocking(move || {
                    let prev = env.frontmost();
                    let mut opts = opts;
                    opts.target.clone_from(&prev);
                    // 「原内容已在历史里」在取词之前现查：暂停记录、隐私规则 / 排除名单跳过、还没来得及记录时都是
                    // false，模拟复制备份失败就不覆盖用户的剪贴板
                    opts.history_enabled = env.clipboard_has_current();
                    let result = env.read_selection(&opts);
                    (prev, result)
                })
                .await
                else {
                    self.advance(run, Event::Failed);
                    self.drop_run(run);
                    return;
                };
                let perf = self.env.perf();
                // 一行不含正文的取词摘要（应用、路径、结果分类、耗时），按应用累计进诊断统计
                {
                    let app_id = prev.as_ref().map_or("-", |a| a.id.as_str());
                    let (method, class, elapsed) = match &result {
                        Ok(sel) if !sel.text.trim().is_empty() => (Some(sel.method.as_str()), "ok", sel.elapsed_ms),
                        Ok(sel) => (Some(sel.method.as_str()), "empty", sel.elapsed_ms),
                        Err(e) => (
                            None,
                            selection_class(e),
                            perf.snapshot(trace).map_or(0, |t| t.elapsed_ms()),
                        ),
                    };
                    perf.record_selection(trace, app_id, method, class, elapsed);
                }
                match result {
                    Ok(sel) if !sel.text.trim().is_empty() => {
                        perf.set_method(trace, sel.method.as_str());
                        perf.mark(trace, PerfStage::Selection);
                        if self.advance(run, Event::Selected).is_none() {
                            log::debug!("run {run}: selection arrived after a newer trigger; dropped");
                            return;
                        }
                        let anchor = match (settings.translate.anchor, sel.anchor) {
                            (TranslateAnchor::Selection, Some(a)) => AnchorSource::Selection(a),
                            _ => AnchorSource::Cursor,
                        };
                        let prev = prev.or(sel.source);
                        (sel.text, anchor, prev, Some(sel.method.as_str().to_owned()))
                    }
                    other => {
                        perf.mark(trace, PerfStage::Selection);
                        let hint = match &other {
                            Err(e) => not_ready::selection_hint(e),
                            Ok(_) => None,
                        };
                        if self.advance(run, Event::SelectionMissed).is_some() {
                            self.open_input(run, trace, prev, hint).await;
                        }
                        return;
                    }
                }
            }
        };

        // 规划：风格、检测、方向、词典式、长文本（超长原文放进阻塞线程池）
        let text = Arc::new(text);
        let threshold = settings.translate.long_text_threshold;
        let speed = *self.speed.lock();
        let planned = {
            let (settings, styles, text, style_id) = (
                Arc::clone(&settings),
                Arc::clone(&styles),
                Arc::clone(&text),
                style.id.clone(),
            );
            cpu(text.len() > HEAVY_TEXT_BYTES, move || {
                let plan = plan_text(
                    &settings,
                    &styles,
                    &TextInput {
                        text: &text,
                        task: TaskKind::Translate,
                        style_id: &style_id,
                        target: None,
                        dict_mode: DictMode::Auto,
                    },
                );
                (plan, plan::long_text(&text, threshold, speed))
            })
            .await
        };
        let Some((mut plan, long)) = planned else {
            self.advance(run, Event::Failed);
            self.drop_run(run);
            return;
        };
        if let Ok(r) = &readiness {
            plan::adapt_for(&mut plan, r);
        }
        let resolved = match readiness {
            Ok(r) => r,
            Err(nr) => {
                if self.advance(run, Event::NotReady).is_some() {
                    let payload = translate_payload(
                        text.to_string(),
                        Some(&plan),
                        &plan.style.id,
                        &settings,
                        None,
                        Some(nr),
                        method,
                    );
                    self.show(
                        run,
                        trace,
                        translate_request(payload, anchor, prev_app, transition),
                        None,
                        None,
                    )
                    .await;
                }
                return;
            }
        };
        // 长文本：先不发请求，等用户确认（确认后由前端 translate_start 发起，run 随之 Confirming → Requesting）
        if let Some(info) = long {
            if self.advance(run, Event::LongText).is_some() {
                let mut payload = translate_payload(
                    text.to_string(),
                    Some(&plan),
                    &plan.style.id,
                    &settings,
                    None,
                    None,
                    method,
                );
                payload.long_text = Some(info);
                self.show(
                    run,
                    trace,
                    translate_request(payload, anchor, prev_app, transition),
                    None,
                    None,
                )
                .await;
            }
            return;
        }
        // 请求：先登记、在后台发出，再显示面板（两者并行）
        let id = self.registry.new_id();
        let token = self.registry.open(
            &id,
            OpenSpec {
                owner: Owner::Unbound,
                lane: Lane::Text,
                model: resolved.info(),
                meta: Some(plan.meta()),
                sink: None,
                trace: Some(trace),
                run: Some(run),
                capture_id: None,
            },
        );
        self.set_run_request(run, &id);
        if self.advance(run, Event::Requested).is_none() {
            self.registry.remove(&id);
            return;
        }
        log::debug!(
            "run {run}: request {id} registered style={} dict={} target={}",
            plan.style.id,
            plan.dict,
            plan.target.as_str()
        );
        let payload = translate_payload(
            text.to_string(),
            Some(&plan),
            &plan.style.id,
            &settings,
            Some(id.clone()),
            None,
            method,
        );
        let job = TextJob {
            id: id.clone(),
            token,
            plan,
            resolved,
            text,
            bypass_cache: false,
            draft: None,
            run: Some(run),
            trace: Some(trace),
            started: Some(started),
            key: key.take(),
        };
        let core = Arc::clone(&self);
        tauri::async_runtime::spawn(async move { core.execute_text(job).await });
        self.show(
            run,
            trace,
            translate_request(payload, anchor, prev_app, transition),
            Some(id),
            None,
        )
        .await;
    }

    // ------------------------------------------------------------------------------------------------
    // 文本请求的执行（Rust 发起与前端发起共用）

    pub(super) async fn execute_text(self: Arc<Self>, mut job: TextJob) {
        let key = KeySource::from(job.key.take());
        let end = self.text_pipeline(&job, key).await;
        self.request_ended(job.run, job.trace, &end);
    }

    /// 缓存 → 取 Key → 请求（或 refine 两步）→ 写缓存
    pub(super) async fn text_pipeline(self: &Arc<Self>, job: &TextJob, key: KeySource) -> PumpEnd {
        let settings = self.env.settings();
        let glossary = self.env.glossary();
        let heavy = job.text.len() > HEAVY_TEXT_BYTES;
        let version = glossary.version();
        let cache_key = if !job.plan.cacheable(&settings) {
            None
        } else if heavy {
            let (plan, resolved, text) = (job.plan.clone(), job.resolved.clone(), Arc::clone(&job.text));
            blocking(move || text_cache_key(&plan, &resolved, &text, version)).await
        } else {
            Some(text_cache_key(&job.plan, &job.resolved, &job.text, version))
        };
        if let Some(k) = cache_key
            && !job.bypass_cache
            && let Some(hit) = self.cache.get(k, Instant::now())
        {
            log::debug!("request {}: cache hit", job.id);
            let reg = &self.registry;
            let delivered = reg.emit(
                &job.id,
                StreamEvent::Notice {
                    kind: NoticeKind::CacheHit,
                },
            ) && reg.emit(&job.id, StreamEvent::Delta { text: hit.to_string() })
                && reg.emit(
                    &job.id,
                    StreamEvent::Done {
                        elapsed_ms: 0,
                        usage: None,
                        truncated: None,
                        cached: true,
                        no_text: false,
                    },
                );
            return if delivered {
                PumpEnd::Done {
                    usage: None,
                    truncated: None,
                    no_text: false,
                    elapsed_ms: 0,
                }
            } else {
                PumpEnd::Cancelled
            };
        }
        let secret = match self.read_key(&job.id, &job.resolved.provider, key, &job.token).await {
            Ok(s) => s,
            Err(end) => return end,
        };
        if job.token.is_cancelled() {
            return PumpEnd::Cancelled;
        }
        if job.plan.task == TaskKind::Refine {
            return self.refine(job, secret, &glossary).await;
        }
        let reasoning = job.resolved.model.reasoning;
        // 离线翻译模型用 Index-Translate 的官方格式（单条 user 消息、中文指令、temperature 0）
        let profile = job.resolved.prompt_profile();
        let built = if heavy {
            let (plan, text, glossary) = (job.plan.clone(), Arc::clone(&job.text), Arc::clone(&glossary));
            match blocking(move || build_text_prompt(&plan, &text, &glossary, reasoning, profile)).await {
                Some(b) => b,
                None => {
                    let err = LlmError::new(ErrorCode::Internal, "building the prompt failed");
                    self.registry.emit(&job.id, StreamEvent::Error(err.clone()));
                    return PumpEnd::Failed(err);
                }
            }
        } else {
            build_text_prompt(&job.plan, &job.text, &glossary, reasoning, profile)
        };
        let post = built.post;
        let req = chat_request(&job.id, &job.resolved, secret, built, Purpose::Translate);
        let rx = self.env.llm_stream(req, job.token.clone());
        log_dispatched(&job.id, job.trace, job.started, &job.resolved.info());
        let (end, timing) = pump(&self.registry, &job.id, rx, post, &job.text).await;
        if let PumpEnd::Done { usage, truncated, .. } = &end {
            if let Some(u) = usage {
                self.env
                    .record_usage(&job.resolved.provider.id, &job.resolved.model.id, u);
                if let (Some(first), Some(streaming)) = (timing.first_delta, timing.streaming) {
                    self.speed.lock().observe(first, streaming, u.output_tokens);
                }
            }
            if let Some(k) = cache_key
                && truncated.is_none()
                && let Some(text) = self.registry.text(&job.id)
                && !text.trim().is_empty()
            {
                // ⌘R（bypass）同样写入：覆盖旧结果
                self.cache.put(k, text, Instant::now());
            }
        }
        end
    }

    /// 提升译文：`Phase{reviewing}` → 非流式反思（只 trim）→ OK 时直接 Done；
    /// 否则 `Phase{improving}` → 流式改进。不进缓存
    pub(super) async fn refine(
        self: &Arc<Self>,
        job: &TextJob,
        secret: Option<Secret>,
        glossary: &GlossaryMatcher,
    ) -> PumpEnd {
        let reg = &self.registry;
        let started = Instant::now();
        if !reg.emit(
            &job.id,
            StreamEvent::Phase {
                phase: StreamPhase::Reviewing,
            },
        ) {
            return PumpEnd::Cancelled;
        }
        let draft = job.draft.clone().unwrap_or_default();
        let reasoning = job.resolved.model.reasoning;
        let reflect = build_reflect_prompt(&job.plan, &job.text, &draft, reasoning);
        let copy = secret.as_ref().map(|s| Secret::new(s.expose().to_owned()));
        let req = chat_request(&job.id, &job.resolved, copy, reflect, Purpose::Reflect);
        let mut rx = self.env.llm_stream(req, job.token.clone());
        log_dispatched(&job.id, job.trace, job.started, &job.resolved.info());
        let mut review = String::new();
        let outcome = loop {
            match rx.recv().await {
                Some(StreamEvent::Delta { text }) => review.push_str(&text),
                Some(StreamEvent::Done { usage, .. }) => break Ok(usage),
                Some(StreamEvent::Error(e)) => break Err(e),
                Some(_) => {}
                None => {
                    break Err(LlmError::new(
                        ErrorCode::Internal,
                        "the stream ended without a terminal event",
                    ));
                }
            }
        };
        drop(rx);
        match outcome {
            Err(e) if e.code == ErrorCode::Cancelled || job.token.is_cancelled() => PumpEnd::Cancelled,
            Err(e) => {
                reg.emit(&job.id, StreamEvent::Error(e.clone()));
                PumpEnd::Failed(e)
            }
            Ok(usage) => {
                if let Some(u) = &usage {
                    self.env
                        .record_usage(&job.resolved.provider.id, &job.resolved.model.id, u);
                }
                if job.token.is_cancelled() {
                    return PumpEnd::Cancelled;
                }
                if review_is_ok(&review) {
                    // 译文已无需改进：Phase{reviewing} → Done，中间没有 Delta（前端保留原译文）
                    let elapsed_ms = crate::app::perf::duration_ms(started.elapsed());
                    let done = StreamEvent::Done {
                        elapsed_ms,
                        usage,
                        truncated: None,
                        cached: false,
                        no_text: false,
                    };
                    return if reg.emit(&job.id, done) {
                        PumpEnd::Done {
                            usage,
                            truncated: None,
                            no_text: false,
                            elapsed_ms,
                        }
                    } else {
                        PumpEnd::Cancelled
                    };
                }
                if !reg.emit(
                    &job.id,
                    StreamEvent::Phase {
                        phase: StreamPhase::Improving,
                    },
                ) {
                    return PumpEnd::Cancelled;
                }
                let review = review.trim();
                let improve = build_improve_prompt(&job.plan, &job.text, &draft, review, glossary, reasoning);
                let post = improve.post;
                let req = chat_request(&job.id, &job.resolved, secret, improve, Purpose::Translate);
                let rx = self.env.llm_stream(req, job.token.clone());
                let (end, _) = pump(reg, &job.id, rx, post, &job.text).await;
                if let PumpEnd::Done { usage: Some(u), .. } = &end {
                    self.env
                        .record_usage(&job.resolved.provider.id, &job.resolved.model.id, u);
                }
                end
            }
        }
    }

    /// 前端发起的翻译 / 润色 / 校对 / 提升（面板或 pinned）。
    ///
    /// 面板会话的 run 还停在 Editing（输入视图还没提交过）或 Confirming（长文本等确认）时，这个请求就是该流程的提交：
    /// run 推进到 Requesting，之后随这个请求 Streaming / Done（[`super::fsm`]）
    pub async fn translate_start(
        self: &Arc<Self>,
        req: TranslateReq,
        host: RequestHost,
        sink: Sink,
    ) -> Result<StartAck, IpcError> {
        if req.text.trim().is_empty() {
            return Err(IpcError::invalid_argument("req.text is empty"));
        }
        if req.task == TaskKind::Refine && req.draft.as_deref().is_none_or(|d| d.trim().is_empty()) {
            return Err(IpcError::invalid_argument(
                "refine needs the current translation in req.draft",
            ));
        }
        let settings = self.env.settings();
        let styles = self.env.styles();
        let owner = owner_of(&host);
        let TranslateReq {
            text,
            task,
            style_id,
            target_lang,
            model_ref,
            dict_mode,
            bypass_cache,
            draft,
            confirm_long,
            ..
        } = req;
        let text = Arc::new(text);
        // 规划（超长原文在阻塞线程池里做语言检测与词典式判定）
        let mut plan = {
            let (s, st, t, sid) = (Arc::clone(&settings), Arc::clone(&styles), Arc::clone(&text), style_id);
            cpu(t.len() > HEAVY_TEXT_BYTES, move || {
                plan_text(
                    &s,
                    &st,
                    &TextInput {
                        text: &t,
                        task,
                        style_id: &sid,
                        target: target_lang,
                        dict_mode,
                    },
                )
            })
            .await
            .ok_or_else(|| IpcError::internal("planning the request failed"))?
        };
        let offline = self.offline_index(&settings, model_ref.as_ref()).await;
        let readiness = not_ready::check_text(&settings, &plan.style, model_ref.as_ref(), task, &self.keys(), &offline);
        // 离线翻译模型：词典式退化为普通翻译、提升译文退化为一次翻译（登记时的 Meta 与之后的请求都按调整后的规划）
        if let Ok(r) = &readiness {
            plan::adapt_for(&mut plan, r);
        }
        // 长文本：输入视图的提交、OCR 面板里的翻译、润色 / 校对
        // 也要先确认。没确认过（confirm_long = false）且超过阈值时不登记请求，只把估算交回去；未就绪时照常走下面的
        // Error（先告诉用户配置问题，确认长文本没有意义）。refine（提升译文）针对已经确认过的译文，不判
        if readiness.is_ok()
            && !confirm_long
            && matches!(task, TaskKind::Translate | TaskKind::Polish | TaskKind::Proofread)
        {
            let threshold = settings.translate.long_text_threshold;
            let speed = *self.speed.lock();
            let t = Arc::clone(&text);
            let long = cpu(text.len() > HEAVY_TEXT_BYTES, move || {
                plan::long_text(&t, threshold, speed)
            })
            .await
            .flatten();
            if let Some(info) = long {
                let width = match &host {
                    RequestHost::Panel { session_id } => {
                        // 输入视图还没提交过：Editing → Confirming（再按输入快捷键是打开新的输入框，不是关闭）
                        if let Some((run, _, Stage::Editing)) = self.run_of_session(*session_id) {
                            self.advance(run, Event::LongText);
                        }
                        self.ack_width(*session_id, &text, &plan, &settings)
                    }
                    RequestHost::Pinned { .. } => None,
                };
                log::info!(
                    "translate_start ({task:?}) from {owner:?}: long text ({} chars) waits for confirmation",
                    info.chars
                );
                return Ok(StartAck {
                    request_id: RequestId::new(),
                    width,
                    long_text: Some(info),
                });
            }
        }
        let (width, run) = match &host {
            RequestHost::Panel { session_id } => (
                self.ack_width(*session_id, &text, &plan, &settings),
                readiness.as_ref().ok().and_then(|_| self.claim_submit(*session_id)),
            ),
            RequestHost::Pinned { .. } => (None, None),
        };
        let id = self.registry.new_id();
        let model = readiness.as_ref().map(Resolved::info).unwrap_or_default();
        let token = self.registry.open(
            &id,
            OpenSpec {
                owner: owner.clone(),
                lane: Lane::Text,
                model,
                meta: Some(plan.meta()),
                sink: Some(sink),
                trace: None,
                run,
                capture_id: None,
            },
        );
        match readiness {
            Err(nr) => {
                self.registry.emit(&id, StreamEvent::Error(not_ready_error(&nr)));
            }
            Ok(resolved) => {
                log::info!(
                    "request {id} ({task:?}) from {owner:?} model={}/{} style={} dict={} bypass_cache={bypass_cache}{}",
                    resolved.provider.id,
                    resolved.model.id,
                    plan.style.id,
                    plan.dict,
                    run.map(|r| format!(" submits run {r}")).unwrap_or_default()
                );
                let job = TextJob {
                    id: id.clone(),
                    token,
                    plan,
                    resolved,
                    text,
                    bypass_cache,
                    draft,
                    run,
                    trace: None,
                    started: None,
                    key: None,
                };
                let core = Arc::clone(self);
                tauri::async_runtime::spawn(async move { core.execute_text(job).await });
            }
        }
        if matches!(owner, Owner::Panel(_)) {
            // 登记之后立即核对一次（会话可能在登记之前就结束了）
            self.reconcile();
        }
        Ok(StartAck {
            request_id: id,
            width,
            long_text: None,
        })
    }

    /// 面板会话的 run 还在等前端提交（Editing：输入视图；Confirming：长文本确认）时，把它推进到 Requesting 并返回 run
    pub(super) fn claim_submit(&self, session: u64) -> Option<u64> {
        let (run, _, stage) = self.run_of_session(session)?;
        if !matches!(stage, Stage::Editing | Stage::Confirming) {
            return None;
        }
        self.advance(run, Event::Requested).map(|_| run)
    }

    /// `StartAck.width`：`view_width(Translate, …)` 与当前卡片宽度相差不到 0.5 px 时为 null；
    /// OCR 视图里的翻译区不改面板宽度。只把原文前缀交给 `view_width`（[`WIDTH_PROBE_CHARS`]）
    pub(super) fn ack_width(
        &self,
        session: u64,
        text: &str,
        plan: &TextPlan,
        settings: &wisp_types::Settings,
    ) -> Option<f64> {
        if self
            .env
            .panel_state()
            .is_some_and(|p| p.session_id == session && p.view == PanelView::Ocr)
        {
            return None;
        }
        let card = self.env.panel_card_width(session)?;
        let payload = PanelPayload::Translate(translate_payload(
            width_probe(text),
            Some(plan),
            &plan.style.id,
            settings,
            None,
            None,
            None,
        ));
        let width = self.env.view_width(PanelView::Translate, &payload);
        ((width - card).abs() >= 0.5).then_some(width)
    }
}

/// 原文前 [`WIDTH_PROBE_CHARS`] 个字符
fn width_probe(text: &str) -> String {
    match text.char_indices().nth(WIDTH_PROBE_CHARS) {
        Some((end, _)) => text[..end].to_owned(),
        None => text.to_owned(),
    }
}

/// 反思的输出 trim 后等于 OK（大小写、句点不计）时跳过改进
pub(super) fn review_is_ok(review: &str) -> bool {
    let t = review.trim().trim_end_matches(['.', '。']);
    t.eq_ignore_ascii_case("ok")
}

/// 翻译视图的载荷
pub(super) fn translate_payload(
    text: String,
    plan: Option<&TextPlan>,
    style_id: &str,
    settings: &wisp_types::Settings,
    request_id: Option<RequestId>,
    not_ready: Option<NotReady>,
    selection_method: Option<String>,
) -> TranslatePayload {
    TranslatePayload {
        source_text: text,
        request_id,
        task: TaskKind::Translate,
        style_id: style_id.to_owned(),
        detected_lang: plan.map(|p| p.detection.lang),
        target_lang: plan.map_or(settings.translate.primary_lang, |p| p.target),
        dict_mode: plan.is_some_and(|p| p.dict),
        long_text: None,
        not_ready,
        selection_method,
    }
}

pub(super) fn translate_request(
    payload: TranslatePayload,
    anchor: AnchorSource,
    prev_app: Option<AppRef>,
    transition: PanelTransition,
) -> PanelShowRequest {
    PanelShowRequest {
        view: PanelView::Translate,
        payload: PanelPayload::Translate(payload),
        anchor,
        take_focus: false,
        prev_app,
        transition,
    }
}

/// 取词失败的分类（取词统计用；与 `not_ready::selection_hint` 的映射一致）
fn selection_class(e: &wisp_platform::SelectionError) -> &'static str {
    use wisp_platform::SelectionError as E;
    match e {
        E::Empty => "empty",
        E::Timeout => "timeout",
        E::PermissionMissing(_) => "permission",
        E::Elevated => "elevated",
        E::Unsupported(_) => "unsupported",
        E::Other(_) => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_probe_keeps_a_char_boundary_prefix() {
        assert_eq!(width_probe("short"), "short");
        let long = "字".repeat(WIDTH_PROBE_CHARS + 10);
        let probe = width_probe(&long);
        assert_eq!(probe.chars().count(), WIDTH_PROBE_CHARS);
        // 前缀已经远超 view_width 的封顶（400 个显示单位），宽度与整段原文相同
        assert_eq!(
            crate::windows::placement::translate_width(&probe, 360.0, 520.0, false),
            crate::windows::placement::translate_width(&long, 360.0, 520.0, false)
        );
    }
}
