/**
 * mock 场景：翻译 / 输入 / Pinned 视图。用 `?scenario=<id>` 打开（`panel.html` / `pinned.html`，见 HUD）。
 *
 * 所有场景共用一套「假编排器」（`install`）：
 * - `settings_get`：带两个服务（OpenAI 官方端点、DeepSeek 兼容端点）与正确输出格式的 6 个内置风格 + 1 个自定义风格；
 * - `translate_start`：按原文、风格、方向、任务（翻译 / 润色 / 校对 / 提升译文）、是否词典式给出对应的示例输出，
 *   流式节奏接近真实（首字约 300 ms、每 22 ms 一块）；同一组参数第二次请求命中缓存（立即整段返回、`cached`），
 *   ⌘R（bypass_cache）绕过缓存。场景可以注入错误、截断等脚本。
 * 面板里的第一个请求与 Rust 一样在面板显示之前就开始（`ctx.stream` 预置，视图 `stream_attach` 回放）。
 *
 * 流畅度场景 `translate.stress-stream`：200 token/s 先输出 plain，再切到 technical 输出
 * Markdown-lite（各 4096 token）。截图矩阵（URL 带 `theme=`）只有 20 s 的稳定时限，此时各输出 600 token。
 *
 * i18n-ignore-file：浏览器 mock 的示例数据（原文、译文、模型名）。
 */
import { longTextAck, longTextGate } from '../longText'
import { boot as bootInfo } from '../../boot'
import type {
  Lang,
  LlmError,
  PanelPayload,
  PinState,
  Provider,
  SettingsSnapshot,
  StreamEvent,
  Style,
  TaskKind,
  TranslateReq,
} from '../../types'
import type { MockContext, PrepareOptions, Scenario, StreamScript, StreamStep } from '../types'

// ---------------------------------------------------------------------------
// 示例数据

const MODEL = { provider_id: 'openai-main', model_id: 'gpt-5.4-mini', model_label: 'GPT-5.4 mini' }
const DEEPSEEK = {
  provider_id: 'deepseek',
  model_id: 'deepseek-chat',
  model_label: 'deepseek-chat',
}

interface Sample {
  source: string
  detected: Lang
  /** 各风格的译文（缺省用 natural） */
  out: Record<string, string>
  /** 目标改成另一种语言时的译文 */
  other?: string
  dict?: string
  polish?: string
  proofread?: string
  refine?: string
  lowConfidence?: boolean
}

const EN: Sample = {
  source:
    'Wisp stays out of your way: it idles at near-zero CPU, wakes in milliseconds, and streams the translation right next to your cursor. The panel grows smoothly with its content and switches to internal scrolling once it reaches the limit. You can keep scrolling and selecting text in the original app — the panel never steals focus.',
  detected: 'en',
  out: {
    natural:
      'Wisp 从不打扰你：空闲时几乎不占 CPU，唤出只需几毫秒，译文就在光标旁边逐字流出。面板高度随内容平滑增长，达到上限后改为内部滚动；你在原应用里继续滚动、选择文字，面板都不会抢走焦点。',
    technical:
      'Wisp 常驻后台时几乎不占用 CPU（**空闲 ≈ 0%**），唤出延迟在毫秒级，译文以流式方式紧贴光标输出。\n\n- 面板高度随内容平滑增长，达到上限后切换为**内部滚动**；\n- 原应用中的滚动与选择不受影响，面板以非激活窗口（`NSPanel`）显示，**不抢占焦点**。',
    academic:
      'Wisp 在后台运行时对处理器的占用近乎为零，可于毫秒级时间内被唤起，并在光标附近以流式方式呈现译文。面板高度随内容平滑增加，达到上限后转为内部滚动；用户在原应用中的滚动与文本选择均不受干扰，面板亦不会夺取输入焦点。',
    casual:
      'Wisp 平时安安静静待在后台，几乎不吃 CPU，一叫就到，译文直接在光标旁边一个字一个字冒出来。面板会跟着内容慢慢变高，太长了就在里面滚；你在原来的软件里该滚就滚、该选就选，它一点都不抢焦点。',
    learn:
      '**译文**\nWisp 从不打扰你：空闲时几乎不占 CPU，唤出只需几毫秒，译文就在光标旁边逐字流出。\n\n**词汇**\n- **stay out of one’s way** 不妨碍某人；不打扰\n- **idle** /ˈaɪdl/ *v.* 空转、待机\n- **steal focus** 抢走（输入）焦点\n\n**语法**\n`switches to … once it reaches …` 中 once 引导时间状语从句，意为「一旦……就……」。',
    literal:
      'Wisp 待在你的路之外：它以接近零的 CPU 空闲，在毫秒内唤醒，并把翻译流式输出在你的光标正旁边。面板随其内容平滑地增长，并在达到限制时切换到内部滚动。你可以在原始应用中继续滚动和选择文本——面板从不窃取焦点。',
    'custom-mail':
      '您好！Wisp 会安静地在后台运行，几乎不占用系统资源，并能在毫秒之间响应。译文会在光标旁逐字呈现；面板随内容自动调整高度，过长时可在内部滚动，不会影响您在原应用中的任何操作。',
  },
  refine:
    'Wisp 从不打扰你：空闲时几乎不占 CPU，一唤即出，译文就在光标旁逐字浮现。面板随内容平滑长高，到达上限后转为内部滚动；你在原应用里照常滚动、选字，它始终不抢焦点。',
}

const ZH: Sample = {
  source: '把这段话翻译成英文：周五之前把设计稿和实现计划一起交付，周末前完成联调。',
  detected: 'zh-CN',
  out: {
    natural:
      'Please translate this into English: deliver the design mockups together with the implementation plan before Friday, and finish integration testing before the weekend.',
    technical:
      'Deliver the **design mockups** and the **implementation plan** together by Friday; complete **integration testing** before the weekend.',
  },
  other: '把这段话翻译成英文：周五之前把设计稿和实现计划一起交付，周末前完成联调。',
}

const POLISH_SRC: Sample = {
  source: 'thanks for quick review, i fix the issue you mention and push new build',
  detected: 'en',
  out: { natural: '感谢你快速审阅，我已修复你提到的问题并推送了新版本。' },
  polish:
    "Thanks for the quick review. I've fixed the issues you mentioned and pushed a new build.\n\n---\n改动：补上冠词 the；“fix” 改为现在完成时；句首大写并拆分长句。",
}

const PROOF_SRC: Sample = {
  source: '我们在周五之前会把设计搞和实现计划一起交付，周末前完成连调。',
  detected: 'zh-CN',
  out: {
    natural:
      'We will deliver the design draft together with the implementation plan before Friday and finish integration testing before the weekend.',
  },
  proofread:
    '我们会在周五之前把设计稿和实现计划一起交付，周末前完成联调。\n\n---\n改动：「设计搞」→「设计稿」；「连调」→「联调」；「在周五之前会」调整为「会在周五之前」。',
}

const DICT_EN: Sample = {
  source: 'serendipity',
  detected: 'en',
  out: { literal: '机缘巧合' },
  dict: '**serendipity** /ˌser.ənˈdɪp.ə.ti/ n.\n1. 机缘巧合 — 意外发现美好事物的运气\n2. 意外之喜 — 偶然得到的好东西\n例：It was pure serendipity that we met. → 我们的相遇纯属机缘巧合。',
}

const DICT_ZH: Sample = {
  source: '缘分',
  detected: 'zh-CN',
  out: { literal: 'fate' },
  dict: '**缘分** yuánfèn\n1. fate; destiny — neutral，最常用\n2. (lucky) chance — casual\n3. predestined affinity — formal / literary\n例：我们能在这里相遇，真是缘分。→ It must be fate that we met here.',
}

const LONG_OUT = Array.from(
  { length: 7 },
  (_, i) =>
    `${i + 1}. ${EN.out.natural}${i % 2 === 0 ? '这一段用来检验长译文在卡片上限处转为内部滚动、边缘渐隐与自动跟随到底部。' : ''}`,
).join('\n\n')

const LONG_SRC: Sample = {
  source: Array.from({ length: 7 }, () => EN.source).join('\n\n'),
  detected: 'en',
  out: { natural: LONG_OUT },
}

const NARROW_SRC: Sample = {
  source:
    'See https://github.com/yzlltyyh/wisp/blob/main/apps/desktop/src/views/translate/TranslateView.tsx#L1 and supercalifragilisticexpialidocious_configuration_value_that_never_wraps',
  detected: 'en',
  out: {
    natural:
      '参见 https://github.com/yzlltyyh/wisp/blob/main/apps/desktop/src/views/translate/TranslateView.tsx#L1 以及一个永远不会换行的 supercalifragilisticexpialidocious_configuration_value_that_never_wraps 配置值。',
  },
}

/** 中英混排、方向不确定（低置信）：交换方向有意义——把原文当作中文、译成英文 */
const MIXED: Sample = {
  source: 'Ship 新版 onboarding before the demo',
  detected: 'en',
  lowConfidence: true,
  out: { natural: '在演示之前上线新版引导流程。' },
  other: 'Ship the new onboarding flow before the demo.',
}

const SAMPLES = [EN, ZH, POLISH_SRC, PROOF_SRC, DICT_EN, DICT_ZH, LONG_SRC, NARROW_SRC, MIXED]

function sampleOf(text: string): Sample {
  return (
    SAMPLES.find((s) => s.source === text) ?? {
      source: text,
      detected: /[一-鿿]/.test(text) ? 'zh-CN' : 'en',
      out: {},
    }
  )
}

/** 风格是否进词典式（literal 不进） */
const DICT_STYLES = new Set(['natural', 'technical', 'academic', 'casual', 'learn'])

/** 按请求给出输出与 Meta（假编排器） */
function respond(req: TranslateReq): {
  text: string
  target: Lang
  dict: boolean
  detected: Lang
  lowConfidence: boolean
} {
  const s = sampleOf(req.text)
  const target: Lang = req.target_lang ?? (s.detected === 'zh-CN' ? 'en' : 'zh-CN')
  const dict = req.task === 'translate' && s.dict !== undefined && DICT_STYLES.has(req.style_id)
  let text: string
  if (req.task === 'polish')
    text = s.polish ?? `${req.text}\n\n---\n改动：调整了语序与用词，使表达更自然。`
  else if (req.task === 'proofread')
    text = s.proofread ?? `${req.text}\n\n---\n改动：未发现明显错误。`
  else if (req.task === 'refine') text = s.refine ?? ''
  else if (dict) text = s.dict ?? ''
  else if (target === s.detected) text = s.other ?? s.source
  else text = s.out[req.style_id] ?? s.out.natural ?? `［${req.text}］`
  return { text, target, dict, detected: s.detected, lowConfidence: s.lowConfidence ?? false }
}

/** 模型（本会话临时换的模型按请求里的 model_ref） */
function modelOf(req: TranslateReq) {
  if (req.model_ref?.provider_id === 'deepseek')
    return { ...DEEPSEEK, model_id: req.model_ref.model_id, model_label: req.model_ref.model_id }
  if (req.model_ref)
    return { ...MODEL, model_id: req.model_ref.model_id, model_label: req.model_ref.model_id }
  return MODEL
}

function cacheKey(req: TranslateReq): string {
  return [req.text, req.task, req.style_id, req.target_lang, req.model_ref?.model_id].join('|')
}

/** 命中缓存：Meta 先到，整段文字一次给出（Rust 的缓存命中同样不等网络） */
function cachedScript(req: TranslateReq): StreamScript {
  const r = respond(req)
  return [
    { type: 'start', request_id: '', upstream_id: null, ...modelOf(req) },
    {
      type: 'meta',
      detected_source_lang: r.detected,
      target_lang: r.target,
      dict_mode: r.dict,
      low_confidence: r.lowConfidence,
    },
    { type: 'notice', kind: 'cache_hit' },
    { wait: 4 },
    { type: 'delta', text: r.text },
    {
      type: 'done',
      elapsed_ms: 6,
      usage: null,
      truncated: null,
      cached: true,
      no_text: false,
    },
  ]
}

function scriptFor(ctx: MockContext, req: TranslateReq, pace: Pace = {}): StreamScript {
  const r = respond(req)
  const meta = {
    detected_source_lang: r.detected,
    target_lang: r.target,
    dict_mode: r.dict,
    low_confidence: r.lowConfidence,
  }
  if (req.task === 'refine') {
    // 提升译文：Phase{reviewing}（无 Delta）→ Phase{improving} → Delta… → Done
    const steps = ctx.textStream(r.text, {
      model: modelOf(req),
      meta,
      firstTokenMs: 260,
      intervalMs: pace.intervalMs ?? 24,
      before: [
        { type: 'phase', phase: 'reviewing' },
        { wait: 1100 },
        { type: 'phase', phase: 'improving' },
      ],
    })
    return steps
  }
  return ctx.textStream(r.text, {
    model: modelOf(req),
    meta,
    firstTokenMs: pace.firstTokenMs ?? 320,
    intervalMs: pace.intervalMs ?? 22,
    chunkChars: pace.chunkChars,
    done: pace.done,
  })
}

interface Pace {
  firstTokenMs?: number
  intervalMs?: number
  chunkChars?: number
  done?: Partial<Extract<StreamEvent, { type: 'done' }>>
}

// ---------------------------------------------------------------------------
// 假编排器

const STYLE_OUTPUT: Record<string, Style['output']> = {
  technical: 'markdown_lite',
  learn: 'markdown_lite',
}

function provider(
  ctx: MockContext,
  id: string,
  name: string,
  base: string,
  models: string[],
): Provider {
  const template = ctx.fixtures.providerTemplates().find((t) => t.id === 'openai')
  const all = ctx.fixtures.models()
  const proto = template?.provider as Provider
  return {
    ...proto,
    id,
    name,
    base_url: base,
    template_id: id === 'deepseek' ? 'deepseek' : 'openai',
    models: models.map((m) => {
      const known = all.find((x) => x.id === m) ?? all[0]
      // 显示名与 Start 事件的 model_label 一致（Rust：label 为空时用 id）
      return { ...(known as Provider['models'][number]), id: m, label: MODEL_LABELS[m] ?? null }
    }),
  }
}

/** 示例模型的显示名（没有的用 id） */
const MODEL_LABELS: Record<string, string> = {
  'gpt-5.4-mini': 'GPT-5.4 mini',
  'gpt-5.4': 'GPT-5.4',
}

function snapshot(
  ctx: MockContext,
  patch: Partial<SettingsSnapshot['settings']['translate']> = {},
): SettingsSnapshot {
  const settings = ctx.fixtures.settings()
  settings.providers = [
    provider(ctx, 'openai-main', 'OpenAI', 'https://api.openai.com/v1', [
      'gpt-5.4-mini',
      'gpt-5.4',
    ]),
    provider(ctx, 'deepseek', 'DeepSeek', 'https://api.deepseek.com', [
      'deepseek-chat',
      'deepseek-reasoner',
    ]),
  ]
  settings.defaults = {
    ...settings.defaults,
    translate: { provider_id: 'openai-main', model_id: 'gpt-5.4-mini' },
  }
  settings.translate = { ...settings.translate, ...patch }
  const styles: Style[] = ctx.fixtures.styles().map((s) => ({
    ...s,
    output: STYLE_OUTPUT[s.id] ?? 'plain_text',
    dictionary_mode: s.id !== 'literal',
  }))
  styles.push({
    id: 'custom-mail',
    name: '邮件回复',
    builtin: false,
    system_template: '<custom>',
    tail_instruction: '',
    temperature: 0.4,
    output: 'plain_text',
    dictionary_mode: false,
    model_ref: null,
    hotkey: null,
    modified: false,
  })
  return { settings, styles, runtime: ctx.fixtures.runtime() }
}

interface InstallOptions {
  /** 翻译设置的覆写（例如默认显示原文对照） */
  translate?: Partial<SettingsSnapshot['settings']['translate']>
  /** 场景接管第 n 个请求（从 1 开始）的脚本；返回 null 时用默认 */
  script?: (req: TranslateReq, n: number) => StreamScript | null
  /** StartAck.width：输入 → 翻译时返回翻译视图的宽度 */
  ackWidth?: (req: TranslateReq) => number | null
  pace?: Pace
}

/** 假编排器的翻译缓存（同一组参数第二次请求命中）；面板打开时预置的请求同样计入 */
const cache = new Set<string>()

function install(ctx: MockContext, options: InstallOptions = {}): void {
  ctx.handle('settings_get', () => snapshot(ctx, options.translate))
  cache.clear()
  let n = 0
  ctx.handle('translate_start', ({ req }, call) => {
    const mock = window.__WISP_MOCK__
    if (!mock) throw new Error('mock not ready')
    const threshold = snapshot(ctx, options.translate).settings.translate.long_text_threshold
    const long = longTextGate(req, threshold)
    if (long) return longTextAck(long, options.ackWidth?.(req) ?? null)
    n++
    const key = cacheKey(req)
    const custom = options.script?.(req, n) ?? null
    const script =
      custom ??
      (cache.has(key) && !req.bypass_cache && req.task !== 'refine'
        ? cachedScript(req)
        : scriptFor(ctx, req, options.pace))
    if (!custom) cache.add(key)
    const id = mock.streams.newRequestId()
    mock.streams.start(id, script)
    if (call.onEvent) mock.streams.attach(id, call.onEvent)
    return { request_id: id, width: options.ackWidth?.(req) ?? null }
  })
}

/** 与 Rust 一样，面板显示之前请求就已发出：预置流并以带 request_id 的载荷打开面板 */
function openTranslate(
  ctx: MockContext,
  sample: Sample,
  options: PrepareOptions & {
    style?: string
    task?: TaskKind
    script?: StreamScript
    pace?: Pace
    dict?: boolean
  } = {},
): number {
  const style = options.style ?? 'natural'
  const task = options.task ?? 'translate'
  const req: TranslateReq = {
    session_id: 0,
    text: sample.source,
    task,
    style_id: style,
    target_lang: null,
    model_ref: null,
    dict_mode: 'auto',
    bypass_cache: false,
    draft: null,
  }
  const r = respond(req)
  const id = `r-open-${Math.random().toString(16).slice(2, 8)}`
  ctx.stream(id, options.script ?? scriptFor(ctx, req, options.pace))
  if (!options.script) cache.add(cacheKey(req))
  return ctx.preparePanel(
    {
      kind: 'translate',
      source_text: sample.source,
      request_id: id,
      task,
      style_id: style,
      detected_lang: r.detected,
      target_lang: r.target,
      dict_mode: options.dict ?? r.dict,
      long_text: null,
      not_ready: null,
      selection_method: ctx.boot.os === 'windows' ? 'uia' : 'ax',
    },
    { width: r.dict ? 360 : 420, ...options },
  )
}

// ---------------------------------------------------------------------------
// 交互：按键与点击（场景里模拟用户操作）

const KEY_OF: Record<string, string> = { Period: '.', Enter: 'Enter', Escape: 'Escape' }

function press(
  ctx: MockContext,
  code: string,
  mods: { mod?: boolean; shift?: boolean } = {},
): void {
  const mac = ctx.boot.os === 'macos'
  const key =
    KEY_OF[code] ??
    (code.startsWith('Key')
      ? code.slice(3).toLowerCase()
      : code.startsWith('Digit')
        ? code.slice(5)
        : code)
  const target = document.activeElement ?? document.body
  target.dispatchEvent(
    new KeyboardEvent('keydown', {
      code,
      key,
      metaKey: mac && mods.mod === true,
      ctrlKey: !mac && mods.mod === true,
      shiftKey: mods.shift === true,
      bubbles: true,
      cancelable: true,
    }),
  )
}

function click(testId: string): void {
  document.querySelector<HTMLElement>(`[data-testid="${testId}"]`)?.click()
}

/** 等视图到达某个状态（轮询，计入「未稳定」） */
function whenStatus(
  ctx: MockContext,
  status: string,
  fn: () => void,
  testId = 'view-translate',
): void {
  const check = () => {
    const el = document.querySelector<HTMLElement>(`[data-testid="${testId}"]`)
    if (el?.dataset.status === status) fn()
    else ctx.after(50, check)
  }
  ctx.after(50, check)
}

// ---------------------------------------------------------------------------
// 错误与未就绪

function llmError(code: LlmError['code'], patch: Partial<LlmError> = {}): LlmError {
  return {
    code,
    subcode: null,
    message: '',
    retryable: false,
    hint: null,
    status: null,
    url: null,
    retry_after_s: null,
    upstream_request_id: null,
    ...patch,
  }
}

const ERRORS: ReadonlyArray<[LlmError['code'], Partial<LlmError>]> = [
  [
    'auth',
    {
      status: 401,
      message: 'Incorrect API key provided: sk-proj-***3f2a.',
      url: 'api.openai.com/v1/chat/completions',
      upstream_request_id: 'req_8f2c41e0',
    },
  ],
  [
    'not_found',
    { status: 404, url: 'api.example.com/chat/completions', hint: 'errors.hint.try_v1' },
  ],
  ['too_long', { status: 400, message: 'context_length_exceeded' }],
  ['quota', { status: 429, message: 'You exceeded your current quota.' }],
  ['rate_limited', { status: 429, retry_after_s: 18, retryable: true }],
  ['upstream', { status: 503, retryable: true }],
  ['content_filter', { status: 400 }],
  ['network', { retryable: true, subcode: 'connection_lost' }],
  ['timeout', { retryable: true }],
  ['protocol', { subcode: 'html_body', url: 'example.com/' }],
  ['not_configured', {}],
  ['keychain', {}],
  ['internal', { message: 'unexpected end of stream' }],
]

function errorScript(error: LlmError, partial = '', delay = 360): StreamScript {
  const steps: StreamStep[] = [
    { type: 'start', request_id: '', upstream_id: null, ...MODEL },
    { wait: delay },
  ]
  for (const chunk of partial.match(/[\s\S]{1,3}/g) ?? [])
    steps.push({ type: 'delta', text: chunk }, { wait: 18 })
  steps.push({ ...error, type: 'error' })
  return steps
}

function translatePayload(
  sample: Sample,
  patch: Partial<Extract<PanelPayload, { kind: 'translate' }>> = {},
) {
  const r = respond({
    session_id: 0,
    text: sample.source,
    task: 'translate',
    style_id: 'natural',
    target_lang: null,
    model_ref: null,
    dict_mode: 'auto',
    bypass_cache: false,
    draft: null,
  })
  return {
    kind: 'translate' as const,
    source_text: sample.source,
    request_id: null,
    task: 'translate' as const,
    style_id: 'natural',
    detected_lang: r.detected,
    target_lang: r.target,
    dict_mode: false,
    long_text: null,
    not_ready: null,
    selection_method: null,
    ...patch,
  }
}

async function statesPage(
  ctx: MockContext,
  title: string,
  desc: string,
  cases: Array<{
    caption: string
    payload: Extract<PanelPayload, { kind: 'translate' }>
    script?: StreamScript
    maxHeight?: number
  }>,
): Promise<void> {
  install(ctx)
  for (const c of cases)
    if (c.payload.request_id && c.script) ctx.stream(c.payload.request_id, c.script)
  const [{ StatesPage }, { setPageOverride }, { createElement }] = await Promise.all([
    import('../../../../views/translate/mock/StatesPage'),
    import('../../../../panel/shell/devHost'),
    import('react'),
  ])
  document.documentElement.dataset.window = 'wp14-states'
  setPageOverride(createElement(StatesPage, { title, desc, cases }))
}

// ---------------------------------------------------------------------------
// 流畅度场景：200 token/s，plain 4096 token → 换 technical（Markdown-lite）4096 token

const MD_TOKENS = [
  '## ',
  '流式',
  '渲染',
  '\n\n',
  '- ',
  '**只追加**',
  ' span',
  '，',
  'the ',
  'panel ',
  'never ',
  'blocks ',
  '`input`',
  '\n',
  '- ',
  '性能',
  '预算',
  ' ≤ ',
  '17 ms',
  '；',
  '*Motion* ',
  'stays ',
  'smooth',
  '。',
  '\n\n',
  '1. ',
  '首字',
  ' p95 ',
  '≤ ',
  '1.2 s',
  '\n',
  '2. ',
  '空闲 ',
  'CPU ',
  '≈ ',
  '0%',
  '\n\n',
  '```',
  '\n',
  'const ',
  'x ',
  '= ',
  '1',
  '\n',
  '```',
  '\n\n',
  'Wisp ',
  'streams ',
  'the ',
  'translation ',
  'right ',
  'next ',
  'to ',
  'your ',
  'cursor',
  '.',
  '\n\n',
]

/** 与 `ctx.streamRate` 同一节奏（每 16 ms 一个 tick 按速率累积 token），只是 token 带 Markdown 结构 */
function mdStress(tokensPerSec: number, total: number): StreamScript {
  const tick = 16
  const perTick = (tokensPerSec * tick) / 1000
  const steps: StreamStep[] = [
    { type: 'start', request_id: '', upstream_id: null, ...MODEL },
    { wait: 200 },
  ]
  let emitted = 0
  let carry = 0
  let k = 0
  let chars = 0
  while (emitted < total) {
    carry += perTick
    const n = Math.min(total - emitted, Math.floor(carry))
    carry -= n
    if (n > 0) {
      let text = ''
      for (let i = 0; i < n; i++) text += MD_TOKENS[k++ % MD_TOKENS.length]
      chars += text.length
      emitted += n
      steps.push({ type: 'delta', text })
    }
    if (emitted < total) steps.push({ wait: tick })
  }
  steps.push({
    type: 'done',
    elapsed_ms: Math.round((total / tokensPerSec) * 1000),
    usage: { input_tokens: 180, output_tokens: total, reasoning_tokens: null, total_tokens: null },
    truncated: null,
    cached: false,
    no_text: chars === 0,
  })
  return steps
}

// ---------------------------------------------------------------------------
// Pinned

function pinState(patch: Partial<PinState> = {}): PinState {
  return {
    kind: 'translate',
    source_text: EN.source,
    result_text: EN.out.natural as string,
    task: 'translate',
    style_id: 'natural',
    target_lang: 'zh-CN',
    detected_lang: 'en',
    request_id: null,
    capture_id: null,
    ocr_text: null,
    ocr_mode: null,
    show_source: false,
    status_line: 'GPT-5.4 mini · 1.6 s · 96 tokens',
    status: {
      model_label: 'GPT-5.4 mini',
      elapsed_ms: 1600,
      usage: { input_tokens: 42, output_tokens: 96, reasoning_tokens: null, total_tokens: 138 },
      cached: false,
      cancelled: false,
      truncated: null,
    },
    model_ref: { provider_id: MODEL.provider_id, model_id: MODEL.model_id },
    on_top: true,
    ...patch,
  }
}

/** Pinned 窗口：macOS / Windows 的窗口外框与阴影由系统画（shadow_margin = 0），Linux 是透明窗口 */
function pinnedWindow(ctx: MockContext, width = 440, height = 320): void {
  const margin = ctx.boot.os === 'linux' ? 24 : 0
  bootInfo.shadow_margin = margin
  document.documentElement.style.setProperty('--shadow-margin', `${margin}px`)
  const root = document.getElementById('root')
  if (!root) return
  Object.assign(root.style, { width: `${width}px`, height: `${height}px` })
  // 宽窗口（展开 ⤢）在模拟屏幕上居中，不越出视口
  if (width > 480)
    root.style.transform = `translate(${Math.round((innerWidth - width) / 2)}px, 96px)`
}

const panel = (s: Omit<Scenario, 'view'> & { view?: Scenario['view'] }): Scenario => ({
  view: 'translate',
  ...s,
})

// ---------------------------------------------------------------------------

export const scenarios: readonly Scenario[] = [
  panel({
    id: 'translate.streaming',
    label: '翻译 · 流式中（plain，状态行 + 停止）',
    manualSettle: true,
    setup(ctx) {
      install(ctx)
      openTranslate(ctx, EN, { pace: { intervalMs: 60, chunkChars: 2 } })
      ctx.after(1500, () => ctx.settled())
    },
  }),
  panel({
    id: 'translate.done',
    label: '翻译 · 完成（底栏淡入：复制 · 固定 · 原文 · 展开）',
    setup(ctx) {
      install(ctx)
      openTranslate(ctx, EN)
    },
  }),
  panel({
    id: 'translate.learn',
    label: '翻译 · 学习风格（Markdown-lite）',
    setup(ctx) {
      install(ctx)
      openTranslate(ctx, EN, { style: 'learn', width: 440 })
    },
  }),
  panel({
    id: 'translate.technical',
    label: '翻译 · 技术风格（Markdown-lite：粗体、列表、行内代码）',
    setup(ctx) {
      install(ctx)
      openTranslate(ctx, EN, { style: 'technical' })
    },
  }),
  panel({
    id: 'translate.dict',
    label: '翻译 · 词典式（serendipity，宽 360）',
    setup(ctx) {
      install(ctx)
      openTranslate(ctx, DICT_EN)
    },
  }),
  panel({
    id: 'translate.dict-zh',
    label: '翻译 · 词典式（缘分 → 英文）',
    setup(ctx) {
      install(ctx)
      openTranslate(ctx, DICT_ZH)
    },
  }),
  panel({
    id: 'translate.zh-en',
    label: '翻译 · 中文 → 英文（自动方向）',
    setup(ctx) {
      install(ctx)
      openTranslate(ctx, ZH)
    },
  }),
  panel({
    id: 'translate.polish',
    label: '润色 · 正文 + 改动（edit_split）',
    setup(ctx) {
      install(ctx)
      openTranslate(ctx, POLISH_SRC, { task: 'polish' })
    },
  }),
  panel({
    id: 'translate.proofread',
    label: '校对 · 正文 + 改动（中文）',
    setup(ctx) {
      install(ctx)
      openTranslate(ctx, PROOF_SRC, { task: 'proofread' })
    },
  }),
  panel({
    id: 'translate.style-switch',
    label: '风格切换（⌘2：旧正文 0.3 + 模糊 → 新内容到达后恢复）',
    motion: true,
    setup(ctx) {
      install(ctx, { pace: { firstTokenMs: 700 } })
      openTranslate(ctx, EN, { focused: true, pace: { intervalMs: 14 } })
      whenStatus(ctx, 'done', () => {
        press(ctx, 'Digit2', { mod: true })
        ctx.after(420, () => ctx.motionMid())
      })
    },
  }),
  panel({
    id: 'translate.cache-hit',
    label: '风格切换 · 命中缓存（切回「自然」：不模糊，直接交叉淡入）',
    motion: true,
    setup(ctx) {
      install(ctx)
      openTranslate(ctx, EN, { focused: true, pace: { intervalMs: 10 } })
      whenStatus(ctx, 'done', () => {
        press(ctx, 'Digit2', { mod: true })
        whenStatus(ctx, 'done', () => {
          press(ctx, 'Digit1', { mod: true })
          ctx.after(40, () => ctx.motionMid())
        })
      })
    },
  }),
  panel({
    id: 'translate.cancelled',
    label: '停止（⌘.）：保留已收到的部分，显示「已取消」',
    setup(ctx) {
      install(ctx)
      openTranslate(ctx, EN, { focused: true, pace: { intervalMs: 60, chunkChars: 2 } })
      ctx.after(1500, () => press(ctx, 'Period', { mod: true }))
    },
  }),
  panel({
    id: 'translate.regenerate',
    label: '重新生成（⌘R，绕过缓存）',
    motion: true,
    setup(ctx) {
      install(ctx)
      openTranslate(ctx, EN, { focused: true, pace: { intervalMs: 10 } })
      whenStatus(ctx, 'done', () => {
        press(ctx, 'KeyR', { mod: true })
        ctx.after(300, () => ctx.motionMid())
      })
    },
  }),
  panel({
    id: 'translate.swap',
    label: '语言条交换方向（中英混排、低置信：「EN → 中文」交换为「中文 → 英语」）',
    setup(ctx) {
      install(ctx)
      openTranslate(ctx, MIXED, { focused: true, pace: { intervalMs: 10 } })
      whenStatus(ctx, 'done', () => click('tv-langbar-source'))
    },
  }),
  panel({
    id: 'translate.show-source',
    label: '原文对照 ⇄（次要色、可选中，高度 spring.gentle）',
    setup(ctx) {
      install(ctx, { translate: { show_source_by_default: true } })
      openTranslate(ctx, EN)
    },
  }),
  panel({
    id: 'translate.long-output',
    label: '长译文：达到卡片上限后正文内部滚动（边缘渐隐、跟随到底部）',
    setup(ctx) {
      install(ctx)
      openTranslate(ctx, LONG_SRC, { width: 480, maxHeight: 380, pace: { intervalMs: 8 } })
    },
  }),
  panel({
    id: 'translate.narrow',
    label: '极端尺寸：宽 360、上限 200，长 URL 与超长单词',
    setup(ctx) {
      install(ctx)
      openTranslate(ctx, NARROW_SRC, { width: 360, maxHeight: 200 })
    },
  }),
  panel({
    id: 'translate.long-text',
    label: '长文本确认（5200 字）',
    setup(ctx) {
      install(ctx)
      ctx.preparePanel(
        translatePayload(LONG_SRC, {
          long_text: { chars: 5200, est_tokens: 1300, est_seconds: 12.5 },
        }),
        { width: 480, focused: true },
      )
    },
  }),
  panel({
    id: 'translate.long-text-huge',
    label: '长文本确认（12000 字：额外提示可能被截断）',
    setup(ctx) {
      install(ctx)
      ctx.preparePanel(
        translatePayload(LONG_SRC, {
          long_text: { chars: 12000, est_tokens: 3000, est_seconds: null },
        }),
        { width: 480 },
      )
    },
  }),
  panel({
    id: 'translate.not-ready',
    label: '未就绪：还没有接入模型',
    setup(ctx) {
      install(ctx)
      ctx.preparePanel(translatePayload(EN, { not_ready: { reason: 'no_provider' } }), {
        width: 420,
      })
    },
  }),
  panel({
    id: 'translate.error',
    label: '错误卡片：auth（服务名、详情可折叠）',
    setup(ctx) {
      install(ctx)
      openTranslate(ctx, EN, {
        script: errorScript(llmError('auth', ERRORS[0]?.[1])),
      })
    },
  }),
  panel({
    id: 'translate.error-partial',
    label: '错误卡片：流式中断网（保留已收到的部分）',
    setup(ctx) {
      install(ctx)
      openTranslate(ctx, EN, {
        script: errorScript(
          llmError('network', { subcode: 'connection_lost', retryable: true }),
          (EN.out.natural as string).slice(0, 52),
        ),
      })
    },
  }),
  panel({
    id: 'translate.truncated',
    label: '截断（max_tokens，有文字）',
    setup(ctx) {
      install(ctx)
      openTranslate(ctx, EN, { pace: { done: { truncated: 'max_tokens' } } })
    },
  }),
  panel({
    id: 'translate.thinking-exhausted',
    label: '思考耗尽了输出额度（截断且没有文字）',
    setup(ctx) {
      install(ctx)
      openTranslate(ctx, EN, {
        script: [
          { type: 'start', request_id: '', upstream_id: null, ...MODEL },
          { wait: 900 },
          {
            type: 'done',
            elapsed_ms: 8400,
            usage: {
              input_tokens: 182,
              output_tokens: 4096,
              reasoning_tokens: 4096,
              total_tokens: 4278,
            },
            truncated: 'max_tokens',
            cached: false,
            no_text: false,
          },
        ],
      })
    },
  }),
  panel({
    id: 'translate.refine',
    label: '提升译文：正在检查译文… → 流式改进稿',
    motion: true,
    setup(ctx) {
      install(ctx)
      openTranslate(ctx, EN, { focused: true, pace: { intervalMs: 8 } })
      whenStatus(ctx, 'done', () => {
        press(ctx, 'KeyR', { mod: true, shift: true })
        ctx.after(500, () => ctx.motionMid())
      })
    },
  }),
  panel({
    id: 'translate.menu-style',
    label: '风格菜单（6 个内置 + 自定义，⌘1–9）',
    setup(ctx) {
      install(ctx)
      openTranslate(ctx, EN, { pace: { intervalMs: 8 }, maxHeight: 460 })
      whenStatus(ctx, 'done', () => click('tv-style-chip'))
    },
  }),
  panel({
    id: 'translate.menu-more',
    label: '⋯ 菜单（润色 / 校对 / 提升译文 / 复制原文 / 切换模型 / 编辑风格）',
    setup(ctx) {
      install(ctx)
      openTranslate(ctx, EN, { pace: { intervalMs: 8 }, maxHeight: 460 })
      whenStatus(ctx, 'done', () => click('tv-more'))
    },
  }),
  {
    id: 'translate.errors',
    label: '全部错误码（状态总览页）',
    view: 'page',
    async setup(ctx) {
      await statesPage(
        ctx,
        '翻译视图 · 错误卡片',
        '每个 ErrorCode 一张：Start → Error（流开始前失败）；cancelled 与 param_unsupported 不出卡片。',
        ERRORS.map(([code, patch]) => ({
          caption: code,
          payload: translatePayload(EN, { request_id: `r-err-${code}` }),
          script: errorScript(llmError(code, patch), '', 30),
        })),
      )
    },
  },
  {
    id: 'translate.states',
    label: '未就绪 / 长文本 / 截断（状态总览页）',
    view: 'page',
    async setup(ctx) {
      await statesPage(ctx, '翻译视图 · 未就绪与提示', '未就绪卡片、长文本确认与截断提示。', [
        {
          caption: 'not_ready: no_provider',
          payload: translatePayload(EN, { not_ready: { reason: 'no_provider' } }),
        },
        {
          caption: 'not_ready: no_default_model',
          payload: translatePayload(EN, {
            not_ready: { reason: 'no_default_model', task: 'translate' },
          }),
        },
        {
          caption: 'not_ready: keychain_unavailable',
          payload: translatePayload(EN, { not_ready: { reason: 'keychain_unavailable' } }),
        },
        {
          caption: 'long_text: 5200',
          payload: translatePayload(LONG_SRC, {
            long_text: { chars: 5200, est_tokens: 1300, est_seconds: 12.5 },
          }),
        },
        {
          caption: 'long_text: 12000（可能被截断）',
          payload: translatePayload(LONG_SRC, {
            long_text: { chars: 12000, est_tokens: 3000, est_seconds: null },
          }),
        },
        {
          caption: 'done · truncated',
          payload: translatePayload(EN, { request_id: 'r-st-trunc' }),
          script: scriptFor(
            ctx,
            {
              session_id: 0,
              text: EN.source,
              task: 'translate',
              style_id: 'natural',
              target_lang: null,
              model_ref: null,
              dict_mode: 'auto',
              bypass_cache: false,
              draft: null,
            },
            { intervalMs: 0, firstTokenMs: 0, done: { truncated: 'max_tokens' } },
          ),
        },
      ])
    },
  },

  // ----- 输入视图
  {
    id: 'translate.input',
    label: '输入翻译 · 空（自动聚焦，占位文案）',
    view: 'input',
    setup(ctx) {
      install(ctx)
      ctx.preparePanel(ctx.fixtures.payload.input(), { width: 420, focused: true })
    },
  },
  {
    id: 'translate.input-draft',
    label: '输入翻译 · 已输入（自动检测 → 目标）',
    view: 'input',
    setup(ctx) {
      install(ctx)
      ctx.preparePanel(
        { kind: 'input', initial_text: ZH.source, target_lang: null, hint: null },
        { width: 420, focused: true },
      )
    },
  },
  {
    id: 'translate.input-hint',
    label: '输入翻译 · 缺辅助功能权限（NotReadyBanner）',
    view: 'input',
    setup(ctx) {
      install(ctx)
      ctx.preparePanel(
        {
          kind: 'input',
          initial_text: '',
          target_lang: null,
          hint: { reason: 'permission_missing', permission: 'accessibility' },
        },
        { width: 420, focused: true },
      )
    },
  },
  {
    id: 'translate.input-wayland',
    label: '输入翻译 · Wayland 请先复制',
    view: 'input',
    setup(ctx) {
      install(ctx)
      ctx.preparePanel(
        {
          kind: 'input',
          initial_text: '',
          target_lang: null,
          hint: { reason: 'wayland_copy_first' },
        },
        { width: 420, focused: true },
      )
    },
  },
  {
    id: 'translate.input-submit',
    label: '输入 → 翻译（⌘↩：交叉淡入，透明帧改宽）',
    view: 'input',
    motion: true,
    setup(ctx) {
      install(ctx, { ackWidth: () => 380 })
      ctx.preparePanel(
        { kind: 'input', initial_text: ZH.source, target_lang: null, hint: null },
        { width: 420, focused: true },
      )
      ctx.after(900, () => {
        press(ctx, 'Enter', { mod: true })
        ctx.after(80, () => ctx.motionMid())
      })
    },
  },

  // ----- Pinned
  {
    id: 'translate.pinned',
    label: 'Pinned · 已完成的译文（置顶开关、✕、浮起）',
    view: 'pinned',
    motion: true,
    setup(ctx) {
      install(ctx)
      pinnedWindow(ctx)
      ctx.handle('pinned_init', () => pinState())
      ctx.after(200, () => ctx.motionMid())
    },
  },
  {
    id: 'translate.pinned-streaming',
    label: 'Pinned · 流式中固定（stream_attach 回放后继续接收）',
    view: 'pinned',
    setup(ctx) {
      install(ctx)
      pinnedWindow(ctx)
      const req: TranslateReq = {
        session_id: 0,
        text: EN.source,
        task: 'translate',
        style_id: 'learn',
        target_lang: null,
        model_ref: null,
        dict_mode: 'auto',
        bypass_cache: false,
        draft: null,
      }
      ctx.stream('r-pinned', scriptFor(ctx, req, { firstTokenMs: 60, intervalMs: 30 }))
      ctx.handle('pinned_init', () =>
        pinState({
          request_id: 'r-pinned',
          style_id: 'learn',
          result_text: '',
          status_line: null,
          status: null,
        }),
      )
    },
  },
  {
    id: 'translate.pinned-expand',
    label: 'Pinned · 展开 ⤢ 长译文（按 640 宽测量首帧，高 min(内容, 屏幕 70%)，默认显示原文）',
    view: 'pinned',
    motion: true,
    setup(ctx) {
      install(ctx)
      // 面板矩形（宽 420）出发；Rust 在 pinned_init 里给出展开后的卡片宽度
      pinnedWindow(ctx, 420, 260)
      ctx.handle('pinned_init', () =>
        pinState({
          source_text: LONG_SRC.source,
          result_text: LONG_OUT,
          show_source: true,
          status_line: 'GPT-5.4 mini · 9.8 s · 1,024 tokens',
          status: {
            model_label: 'GPT-5.4 mini',
            elapsed_ms: 9800,
            usage: {
              input_tokens: 512,
              output_tokens: 1024,
              reasoning_tokens: null,
              total_tokens: 1536,
            },
            cached: false,
            cancelled: false,
            truncated: null,
          },
          expand_width: Math.min(640, screen.availWidth - 16),
        }),
      )
      // mock 里由这里模拟 Rust 的展开：矩形弹簧到 640 × min(内容, 屏幕 70%)、在屏幕上居中
      ctx.handle('pinned_ready', ({ content_height }) => {
        const root = document.getElementById('root')
        if (!root) return
        const margin = 2 * bootInfo.shadow_margin
        const height = Math.min(content_height + margin, Math.round(innerHeight * 0.7))
        root.dataset.contentHeight = String(content_height)
        root.style.transition =
          'width 420ms cubic-bezier(.22,1,.36,1), height 420ms cubic-bezier(.22,1,.36,1), transform 420ms cubic-bezier(.22,1,.36,1)'
        Object.assign(root.style, {
          width: `${640 + margin}px`,
          height: `${height}px`,
          transform: `translate(${Math.round((innerWidth - 640 - margin) / 2)}px, 64px)`,
        })
        ctx.after(200, () => ctx.motionMid())
      })
    },
  },
  {
    id: 'translate.pinned-polish',
    label: 'Pinned · 润色结果（正文 + 改动）',
    view: 'pinned',
    setup(ctx) {
      install(ctx)
      pinnedWindow(ctx, 440, 300)
      ctx.handle('pinned_init', () =>
        pinState({
          source_text: POLISH_SRC.source,
          result_text: POLISH_SRC.polish,
          task: 'polish',
          detected_lang: 'en',
          target_lang: 'en',
        }),
      )
    },
  },

  // ----- 流畅度
  panel({
    id: 'translate.stress-stream',
    label: '流畅度：200 token/s 输出 plain 与 Markdown-lite 各 4096 token',
    manualSettle: true,
    async setup(ctx) {
      // 常驻面板的稳态：真实应用里外壳与翻译视图的字体预热在启动后、第一次挂载后的空闲时间里完成；mock 页面一加载就
      // 开始流式，所以在挂载之前同步做完同样的预热（与 shell.view-switch 的做法一致）
      const [{ warmUp }, { warmMarkdownFonts }] = await Promise.all([
        import('../../../perf/warmup'),
        import('../../../../views/translate/warmup'),
      ])
      warmUp()
      warmMarkdownFonts()
      // 截图矩阵（URL 带 theme）只有 20 s 稳定时限：各输出 600 token；流畅度 trace（不带 theme）跑满 4096
      const shot = new URLSearchParams(location.search).has('theme')
      const total = shot ? 600 : 4096
      install(ctx, {
        script: (req) => (req.style_id === 'technical' ? mdStress(200, total) : null),
      })
      openTranslate(ctx, EN, {
        focused: true,
        width: 480,
        script: ctx.streamRate(200, total, { model: MODEL }),
      })
      whenStatus(ctx, 'done', () => {
        press(ctx, 'Digit2', { mod: true })
        ctx.after(300, () => whenStatus(ctx, 'done', () => ctx.settled()))
      })
    },
  }),
]
