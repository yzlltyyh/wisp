/**
 * mock 场景：OCR 视图。每个场景 `{ id, label, view, setup(ctx) }`，id 以 `ocr.` 开头（文件名即前缀）；
 * 用 `?scenario=<id>` 打开（Overlay 的场景在 `overlay.ts`）。
 *
 * 共同准备：
 * - 截图缩略图是用 OffscreenCanvas 画出的 PNG（与识别文本一致的「性能预算」便签），经 `ctx.asset('crop/…')` 注册——
 *   不用带 `<text>` 的 SVG（Chromium 第一次排版它要 13–14 ms，会在流畅度采样里制造与真实应用无关的长任务）；
 * - 设置快照里补上三家服务与视觉模型（vision yes / unknown / no 各有），`defaults.ocr` 指向其中一个——模型菜单只列
 *   yes / unknown；
 * - 识别流在 `preparePanel` 之前用 `ctx.stream(request_id, …)` 预置（相当于 Rust 在 commit 时已发出请求）。
 * 需要交互的场景（打开菜单、展开译文、编辑……）在挂载后用 DOM 操作驱动，结束时 `ctx.settled()`。
 */
import { boot as bootInfo } from '../../boot'
import type {
  ModelEntry,
  OcrLayout,
  OcrMode,
  PanelPayload,
  PinState,
  Provider,
  StreamEvent,
} from '../../types'
import type { MockContext, PrepareOptions, Scenario, StreamScript } from '../types'

const OCR_TEXT = `性能预算
面板唤出 p95 ≤ 100 ms
首 token p95 ≤ 1.2 s
空闲 CPU ≈ 0%
常驻内存 ≤ 60 MB`

const OCR_TRANSLATION = `Performance budget
Panel show p95 ≤ 100 ms
First token p95 ≤ 1.2 s
Idle CPU ≈ 0%
Resident memory ≤ 60 MB`

const OCR_MARKDOWN = `## 性能预算

| 指标 | 目标 |
| --- | --- |
| 面板唤出 p95 | ≤ 100 ms |
| 首 token p95 | ≤ 1.2 s |

- 空闲 CPU ≈ 0%
- 常驻内存 ≤ 60 MB

\`\`\`ts
const budget = { idleCpu: 0, memoryMb: 60 }
\`\`\``

const OCR_PARAGRAPHS =
  '性能预算：面板唤出 p95 ≤ 100 ms，首 token p95 ≤ 1.2 s，空闲 CPU ≈ 0%，常驻内存 ≤ 60 MB。'

const LONG_TEXT = Array.from({ length: 48 }, (_, i) => {
  const n = String(i + 1).padStart(2, '0')
  if (i % 3 === 0)
    return `${n}  error[E0502]: cannot borrow \`frames\` as mutable because it is also borrowed as immutable`
  if (i % 3 === 1) return `${n}     --> crates/wisp-capture/src/macos.rs:214:9`
  return `${n}      |  let first = frames.first(); frames.push(next) // 截图帧缓存`
}).join('\n')

const LONG_TRANSLATION = Array.from({ length: 48 }, (_, i) => {
  const n = String(i + 1).padStart(2, '0')
  if (i % 3 === 0) return `${n}  错误[E0502]：无法以可变方式借用 \`frames\`，因为它也被不可变地借用`
  if (i % 3 === 1) return `${n}     --> crates/wisp-capture/src/macos.rs:214:9`
  return `${n}      |  let first = frames.first(); frames.push(next) // 截图帧缓存`
}).join('\n')

const QWEN = { provider_id: 'qwen', model_id: 'qwen3-vl-plus', model_label: 'qwen3-vl-plus' }

function model(id: string, vision: ModelEntry['vision'], label: string | null = null): ModelEntry {
  return {
    id,
    label,
    vision,
    vision_source: vision === 'unknown' ? null : 'capability',
    reasoning: false,
    enabled: true,
    missing_upstream: false,
    deprecated: false,
    context_tokens: null,
    caps: {
      temperature: null,
      max_tokens_field: null,
      thinking_param: null,
      effort_param: null,
      stream_options: null,
      include_obfuscation: null,
      anthropic_thinking_disabled: null,
      anthropic_efforts: null,
    },
    reasoning_effort: null,
  }
}

function provider(id: string, name: string, models: ModelEntry[]): Provider {
  return {
    id,
    name,
    protocol: 'openai_chat',
    base_url: `https://${id}.example.com/v1`,
    headers: {},
    quirks: {
      thinking_off: 'none',
      merge_system_into_user: false,
      strip_think_tags: false,
      max_tokens_field: null,
      dual_auth_header: null,
      gemini_key_in_query: false,
    },
    models,
    timeouts: { connect_ms: 10_000, first_token_ms: 30_000, idle_ms: 30_000 },
    template_id: null,
  }
}

/** 设置快照：三家服务、视觉模型（yes / unknown / no）与默认 OCR 模型 */
function seedSettings(): void {
  const state = window.__WISP_MOCK__?.state
  if (!state) return
  state.settings.providers = [
    provider('qwen', '通义千问', [
      model('qwen3-vl-plus', 'yes'),
      model('qwen3-vl-flash', 'yes'),
      model('qwen-max', 'no'),
    ]),
    provider('openai', 'OpenAI', [
      model('gpt-5.4-mini', 'yes', 'GPT-5.4 mini'),
      model('o4-text', 'no'),
    ]),
    provider('ollama', 'Ollama', [model('llava:13b', 'unknown')]),
  ]
  state.settings.defaults.ocr = { provider_id: 'qwen', model_id: 'qwen3-vl-plus' }
}

/** 与识别文本一致的截图缩略图（PNG）；`path` 缺省为 `crop/cap-<w>x<h>.png` */
async function thumb(
  ctx: MockContext,
  width: number,
  height: number,
  lines: readonly string[],
  path = `crop/cap-${width}x${height}.png`,
): Promise<string> {
  const c = new OffscreenCanvas(width, height)
  const g = c.getContext('2d')
  if (g) {
    g.fillStyle = '#fbfaf7'
    g.fillRect(0, 0, width, height)
    g.fillStyle = '#1d1d1f'
    const size = Math.max(10, Math.min(28, Math.round(height / (Math.max(1, lines.length) * 1.7))))
    lines.forEach((line, i) => {
      const weight = i === 0 ? 600 : 400
      g.font = `${weight} ${i === 0 ? size + 4 : size}px -apple-system, "PingFang SC", "Segoe UI", sans-serif`
      g.fillText(line, size * 1.2, size * 1.6 + i * size * 1.6)
    })
    g.strokeStyle = 'rgb(0 0 0 / .08)'
    g.lineWidth = 2
    g.strokeRect(0, 0, width, height)
  }
  const blob = await c.convertToBlob({ type: 'image/png' })
  return ctx.asset(path, blob)
}

interface PayloadOptions {
  /** 截图的 id（未就绪时 Rust 还没有截图：空串） */
  captureId?: string
  mode?: OcrMode
  /** 会话排版的初值（Rust 取设置里的默认排版；直接翻译恒为 keep），默认 keep */
  layout?: OcrLayout
  requestId?: string | null
  notReady?: Extract<PanelPayload, { kind: 'ocr' }>['not_ready']
  size?: [number, number]
  thumbLines?: readonly string[]
  noThumb?: boolean
}

async function ocrPayload(ctx: MockContext, o: PayloadOptions = {}): Promise<PanelPayload> {
  const [w, h] = o.size ?? [1280, 480]
  const lines = o.thumbLines ?? OCR_TEXT.split('\n')
  // 缩略图就是送给模型的那张裁剪图（两者共用内存），分辨率接近原图；整行文字的细条按 1600 宽生成，
  // 否则 32 px 高的横条会被放大得发虚，与真机不符
  const tw = Math.min(w, w / h > 4 ? 1600 : 640)
  const th = Math.max(8, Math.round((tw * h) / w))
  return {
    kind: 'ocr',
    capture_id: o.captureId ?? 'cap-1',
    request_id: o.requestId === undefined ? 'r-ocr' : o.requestId,
    mode: o.mode ?? 'lines',
    thumb_url: o.noThumb ? '' : await thumb(ctx, tw, th, lines),
    image_size: o.noThumb ? { width: 0, height: 0 } : { width: w, height: h },
    target_lang: 'zh-CN',
    not_ready: o.notReady ?? null,
    layout: o.layout ?? 'keep',
  }
}

type Done = Extract<StreamEvent, { type: 'done' }>

/** 识别流：start → 首 token 前等待 → 逐段 Delta → done */
function ocrStream(ctx: MockContext, text: string, done: Partial<Done> = {}): StreamScript {
  return ctx.textStream(text, {
    chunkChars: 6,
    intervalMs: 24,
    firstTokenMs: 420,
    model: QWEN,
    done: {
      elapsed_ms: 2140,
      usage: { input_tokens: 1312, output_tokens: 64, reasoning_tokens: null, total_tokens: 1376 },
      ...done,
    },
  })
}

const start: StreamEvent = { type: 'start', request_id: 'r-ocr', ...QWEN, upstream_id: null }

async function until(test: () => boolean, timeoutMs = 8000): Promise<void> {
  const started = performance.now()
  while (!test() && performance.now() - started < timeoutMs) {
    // oxlint-disable-next-line no-await-in-loop -- 轮询 DOM 状态
    await new Promise((r) => setTimeout(r, 30))
  }
}
const $ = <T extends Element = HTMLElement>(sel: string): T | null => document.querySelector<T>(sel)
const pause = (ms: number) => new Promise((r) => setTimeout(r, ms))
const phase = (p: string) => () => $(`[data-testid="view-ocr"][data-phase="${p}"]`) !== null
const click = (sel: string) => $<HTMLElement>(sel)?.click()

/** 编辑 OCR 文本（模拟用户输入，走 React 的受控输入） */
function typeInto(sel: string, value: string): void {
  const el = $<HTMLTextAreaElement>(sel)
  if (!el) return
  Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')?.set?.call(el, value)
  el.dispatchEvent(new Event('input', { bubbles: true }))
}

interface Spec {
  id: string
  label: string
  run: (ctx: MockContext) => Promise<void>
  motion?: boolean
}

function scenario({ id, label, run, motion }: Spec): Scenario {
  return {
    id: `ocr.${id}`,
    label,
    view: 'ocr',
    manualSettle: true,
    ...(motion ? { motion } : null),
    setup(ctx) {
      seedSettings()
      void run(ctx)
        .catch((error: unknown) => console.error('[mock] ocr 场景失败', error))
        .finally(() => ctx.settled())
    },
  }
}

const show = (ctx: MockContext, payload: PanelPayload, options: PrepareOptions = {}) =>
  ctx.preparePanel(payload, { width: 440, ...options })

/** 打开并等识别完成 */
async function recognized(
  ctx: MockContext,
  text = OCR_TEXT,
  options: PayloadOptions = {},
): Promise<void> {
  ctx.stream('r-ocr', ocrStream(ctx, text))
  show(ctx, await ocrPayload(ctx, options))
  await until(phase('done'))
}

/**
 * 固定窗口（PinnedApp + 本视图）：macOS / Windows 的窗口外框与阴影由系统画（shadow_margin = 0），Linux 是透明窗口。
 * 缩略图与原图尺寸随 PinState 交接（thumb_url / image_size）
 */
async function pinnedOcr(ctx: MockContext, state: Partial<PinState>, height = 360): Promise<void> {
  const margin = ctx.boot.os === 'linux' ? 24 : 0
  bootInfo.shadow_margin = margin
  document.documentElement.style.setProperty('--shadow-margin', `${margin}px`)
  const root = document.getElementById('root')
  if (root) Object.assign(root.style, { width: '440px', height: `${height}px` })
  // pinned_init 必须在场景 setup 的同步部分就注册（PinnedApp 一启动就调用它）；缩略图异步生成，pinned_init 等它好了
  // 再返回——否则固定窗口可能先拿到默认的翻译 PinState，或者缩略图还没注册就去加载
  const ready = thumb(ctx, 640, 240, OCR_TEXT.split('\n'), 'crop/cap-1.png')
  const pin: PinState = {
    kind: 'ocr',
    source_text: OCR_TEXT,
    result_text: '',
    task: 'translate',
    style_id: 'natural',
    target_lang: 'en',
    detected_lang: 'zh-CN',
    request_id: 'r-ocr',
    capture_id: 'cap-1',
    ocr_text: OCR_TEXT,
    ocr_mode: 'lines',
    show_source: false,
    status_line: 'qwen3-vl-plus · 2.1 s',
    status: {
      model_label: 'qwen3-vl-plus',
      elapsed_ms: 2100,
      usage: null,
      cached: false,
      cancelled: false,
      truncated: null,
    },
    model_ref: { provider_id: 'dashscope', model_id: 'qwen3-vl-plus' },
    thumb_url: 'crop/cap-1.png',
    image_size: { width: 640, height: 240 },
    on_top: true,
    ...state,
  }
  ctx.handle('pinned_init', async () => {
    await ready
    return pin
  })
  await ready
}

const pinnedScenario = (spec: Spec): Scenario => ({ ...scenario(spec), view: 'pinned' })

// ---------------------------------------------------------------------------
// 排版（逐行 / 段落的 [整理 | ▾]）

/**
 * 排版场景的文字：逐行与段落取 Rust 变换样例表里的输入（mock 的 text_transform 只认样例表，切换后看到的就是真实结果），
 * 结构化用带表格的 Markdown（同一位置是「源码 / 预览」）
 */
const LAYOUT_TEXT = {
  lines: '第一行\n第二行\n第三行',
  paragraphs: '第一段，被\n硬换行的中文。\n\n第二段。',
  markdown: OCR_MARKDOWN,
} as const
type LayoutMode = keyof typeof LAYOUT_TEXT

/** 窄面板（360 px）上识别完成 */
async function layoutPanel(ctx: MockContext, mode: LayoutMode): Promise<void> {
  const text = LAYOUT_TEXT[mode]
  ctx.stream('r-ocr', ocrStream(ctx, text))
  show(ctx, await ocrPayload(ctx, { mode, thumbLines: text.split('\n').slice(0, 6) }), {
    width: 360,
  })
  await until(phase('done'))
}

/** URL 的 `mode`（lines / paragraphs / markdown），默认段落 */
function layoutMode(): LayoutMode {
  const mode = new URLSearchParams(location.search).get('mode')
  return mode === 'lines' || mode === 'markdown' ? mode : 'paragraphs'
}

// ---------------------------------------------------------------------------
// 离线模型与 PaddleOCR 云端

const OFFLINE_SMALL = {
  provider_id: '@offline',
  model_id: 'pp_ocrv6_small',
  model_label: 'PP-OCRv6 Small',
}
const OFFLINE_VL = {
  provider_id: '@offline',
  model_id: 'paddleocr_vl_1_6',
  model_label: 'PaddleOCR-VL',
}
const PADDLE_STRUCTURE = {
  provider_id: 'paddleocr',
  model_id: 'PP-StructureV3',
  model_label: 'PP-StructureV3',
}

/** 结构化结果：管道表格、带合并单元格的 HTML 表格（PaddleX 的输出形式）、公式 */
const OCR_TABLES = `## 季度指标

| 指标 | Q1 | Q2 | 变化 |
| :--- | ---: | ---: | :---: |
| 唤出 p95 | 118 ms | 96 ms | **−19%** |
| 首 token p95 | 1.4 s | 1.1 s | −21% |
| 常驻内存 | 64 MB | 58 MB | −9% |

<div style="text-align: center;"><html><body><table border=1><tr><td rowspan=2>平台</td><td colspan=2>安装包</td></tr><tr><td>大小</td><td>预算</td></tr><tr><td>macOS</td><td>9.8 MB</td><td>12 MB</td></tr><tr><td>Windows</td><td>7.6 MB</td><td>10 MB</td></tr></table></body></html></div>

内存估算按 $M = n \\times s^2$ 计算：

$$
T_{p95} = \\mu + 1.645\\,\\sigma
$$`

/** 本机已下载 PP-OCRv6 Small 与 VL、加上一个 PaddleOCR 服务；`select` 时选用 Small 作为离线识别模型 */
function seedOffline(select: boolean): void {
  const state = window.__WISP_MOCK__?.state
  if (!state) return
  const snap = state.offline.snapshot
  state.offline.snapshot = {
    ...snap,
    models: snap.models.map((m) =>
      m.id === 'pp_ocrv6_small' || m.id === 'paddleocr_vl_1_6'
        ? { ...m, state: { state: 'installed' as const } }
        : m,
    ),
  }
  const paddle = provider('paddleocr', 'PaddleOCR', [
    model('PP-OCRv6', 'unknown'),
    model('PP-StructureV3', 'unknown'),
    model('PaddleOCR-VL-1.6', 'unknown'),
  ])
  paddle.protocol = 'paddle_ocr'
  paddle.base_url = 'https://paddleocr.aistudio-app.com/api/v2/ocr/jobs'
  paddle.template_id = 'paddleocr'
  state.settings.providers = [...state.settings.providers, paddle]
  if (select) state.settings.offline = { ...state.settings.offline, ocr_model: 'pp_ocrv6_small' }
}

const offlineMissing = (id: 'pp_ocrv6_small' | 'paddleocr_vl_1_6') => ({
  reason: 'offline_model_missing' as const,
  model: id,
})

export const scenarios: readonly Scenario[] = [
  scenario({
    id: 'recognizing',
    label: 'OCR · 识别中（骨架）',
    async run(ctx) {
      ctx.stream('r-ocr', [start, { wait: 60_000 }])
      show(ctx, await ocrPayload(ctx))
      await pause(500)
    },
  }),
  scenario({
    id: 'streaming',
    label: 'OCR · 流式中（只读）',
    async run(ctx) {
      ctx.stream('r-ocr', [...ocrStream(ctx, OCR_TEXT.slice(0, 30)).slice(0, -1), { wait: 60_000 }])
      show(ctx, await ocrPayload(ctx))
      await until(phase('streaming'))
      await pause(900)
    },
  }),
  scenario({
    id: 'done',
    label: 'OCR · 完成（可编辑 + 自动复制「已复制」）',
    async run(ctx) {
      await recognized(ctx)
      await pause(350)
    },
  }),
  scenario({
    id: 'edited',
    label: 'OCR · 编辑后（状态行「已编辑」）',
    async run(ctx) {
      await recognized(ctx)
      await pause(1700)
      typeInto('[data-testid="ocr-text"]', `${OCR_TEXT}\n备注：以上为初稿，周五前定稿`)
      await pause(300)
    },
  }),
  scenario({
    id: 'confirm',
    label: 'OCR · 编辑后重新识别（确认覆盖）',
    async run(ctx) {
      await recognized(ctx)
      typeInto('[data-testid="ocr-text"]', `${OCR_TEXT}（已校对）`)
      await pause(100)
      click('[data-testid="ocr-recognize-again"]')
      await pause(500)
    },
  }),
  scenario({
    id: 'translate',
    label: 'OCR · 展开译文区（流式完成）',
    async run(ctx) {
      await recognized(ctx)
      ctx.nextStream(
        ctx.textStream(OCR_TRANSLATION, {
          model: { provider_id: 'openai', model_id: 'gpt-5.4-mini', model_label: 'GPT-5.4 mini' },
          meta: { detected_source_lang: 'zh-CN', target_lang: 'en' },
          done: { elapsed_ms: 980 },
        }),
      )
      click('[data-testid="ocr-translate-toggle"]')
      await until(() => $('[data-testid="ocr-translation-body"][data-phase="done"]') !== null)
      await pause(600)
    },
  }),
  scenario({
    id: 'translate-streaming',
    label: 'OCR · 译文流式中',
    async run(ctx) {
      await recognized(ctx)
      const steps = ctx.textStream(OCR_TRANSLATION, {
        meta: { detected_source_lang: 'zh-CN', target_lang: 'en' },
      })
      ctx.nextStream([...steps.slice(0, Math.round(steps.length * 0.55)), { wait: 60_000 }])
      click('[data-testid="ocr-translate-toggle"]')
      await until(() => $('[data-testid="ocr-translation-body"][data-phase="streaming"]') !== null)
      await pause(700)
    },
  }),
  scenario({
    id: 'style-menu',
    label: 'OCR · 翻译风格菜单',
    async run(ctx) {
      await recognized(ctx)
      click('[data-testid="ocr-translate-menu"]')
      await pause(400)
    },
  }),
  scenario({
    id: 'recognize-menu',
    label: 'OCR · 重新识别菜单（模式 / 视觉模型）',
    async run(ctx) {
      await recognized(ctx)
      click('[data-testid="ocr-recognize-menu"]')
      await pause(400)
    },
  }),
  scenario({
    id: 'model-menu',
    label: 'OCR · 视觉模型菜单（只列 vision = yes / unknown）',
    async run(ctx) {
      await recognized(ctx)
      click('[data-testid="ocr-recognize-menu"]')
      await until(() => $('[role="menu"]') !== null)
      const item = [...document.querySelectorAll<HTMLElement>('[role="menuitem"]')].find((el) =>
        /视觉模型|Vision model/.test(el.textContent ?? ''),
      )
      item?.click()
      await until(() => $('[data-testid="ocr-model-menu"]') !== null)
      // 脚本里的 click() 让焦点回到 ▾ 时带上 :focus-visible（真实的鼠标点击不会），会冒出它的 tooltip：移走焦点
      $<HTMLElement>('[data-testid="ocr-recognize-menu"]')?.blur()
      await pause(400)
    },
  }),
  scenario({
    id: 'markdown',
    label: 'OCR · 结构化模式（Markdown 源码，可编辑）',
    async run(ctx) {
      ctx.stream('r-ocr', ocrStream(ctx, OCR_MARKDOWN))
      show(ctx, await ocrPayload(ctx, { mode: 'markdown' }))
      await until(phase('done'))
      await pause(1800)
    },
  }),
  scenario({
    id: 'long-text',
    label: 'OCR · 长文本（区内滚动）',
    async run(ctx) {
      ctx.stream('r-ocr', ctx.textStream(LONG_TEXT, { chunkChars: 40, intervalMs: 8, model: QWEN }))
      show(
        ctx,
        await ocrPayload(ctx, {
          size: [1720, 1180],
          thumbLines: LONG_TEXT.split('\n').slice(0, 14),
        }),
      )
      await until(phase('done'))
      await pause(1800)
    },
  }),
  scenario({
    id: 'no-text',
    label: 'OCR · 未检测到文字',
    async run(ctx) {
      ctx.stream('r-ocr', [
        start,
        { wait: 600 },
        {
          type: 'done',
          elapsed_ms: 1630,
          usage: null,
          truncated: null,
          cached: false,
          no_text: true,
        },
      ])
      show(ctx, await ocrPayload(ctx, { thumbLines: [] }))
      await until(phase('done'))
      await pause(400)
    },
  }),
  scenario({
    id: 'thinking-exhausted',
    label: 'OCR · 思考耗尽输出额度',
    async run(ctx) {
      ctx.stream('r-ocr', [
        start,
        { wait: 600 },
        {
          type: 'done',
          elapsed_ms: 8120,
          usage: {
            input_tokens: 1312,
            output_tokens: 4096,
            reasoning_tokens: 4096,
            total_tokens: 5408,
          },
          truncated: 'max_tokens',
          cached: false,
          no_text: false,
        },
      ])
      show(ctx, await ocrPayload(ctx))
      await until(phase('done'))
      await pause(400)
    },
  }),
  scenario({
    id: 'cancelled',
    label: 'OCR · 已取消（保留已收到的文字）',
    async run(ctx) {
      ctx.stream('r-ocr', ctx.errorStream({ code: 'cancelled' }, OCR_TEXT.slice(0, 24)))
      show(ctx, await ocrPayload(ctx))
      await until(phase('cancelled'))
      await pause(400)
    },
  }),
  scenario({
    id: 'error-no-vision',
    label: 'OCR · 当前模型不支持图片（选择模型 / 打开设置）',
    async run(ctx) {
      ctx.stream(
        'r-ocr',
        ctx.errorStream({
          code: 'no_vision',
          message: 'model does not support image input',
          status: 400,
          url: 'dashscope.aliyuncs.com/compatible-mode/v1/chat/completions',
        }),
      )
      show(ctx, await ocrPayload(ctx))
      await until(phase('error'))
      await pause(500)
    },
  }),
  scenario({
    id: 'error-network',
    label: 'OCR · 网络错误（重试）',
    async run(ctx) {
      ctx.stream('r-ocr', ctx.errorStream({ code: 'network', retryable: true }))
      show(ctx, await ocrPayload(ctx))
      await until(phase('error'))
      await pause(500)
    },
  }),
  scenario({
    id: 'permission',
    label: 'OCR · 缺少屏幕录制权限（引导）',
    async run(ctx) {
      show(
        ctx,
        await ocrPayload(ctx, {
          requestId: null,
          captureId: '',
          noThumb: true,
          notReady: { reason: 'permission_missing', permission: 'screen_recording' },
        }),
        { width: 400 },
      )
      await pause(500)
    },
  }),
  scenario({
    id: 'no-vision-model',
    label: 'OCR · 还没有能识别图片的模型',
    async run(ctx) {
      show(ctx, await ocrPayload(ctx, { requestId: null, notReady: { reason: 'no_vision_model' } }))
      await pause(500)
    },
  }),
  scenario({
    id: 'capture-failed',
    label: 'OCR · 截图失败',
    async run(ctx) {
      show(
        ctx,
        await ocrPayload(ctx, {
          requestId: null,
          captureId: '',
          noThumb: true,
          notReady: { reason: 'capture_failed', message: 'errors.capture.backend' },
        }),
        { width: 400 },
      )
      await pause(500)
    },
  }),
  scenario({
    id: 'zoom',
    label: 'OCR · 放大查看截图',
    async run(ctx) {
      await recognized(ctx)
      click('[data-testid="ocr-thumbnail"]')
      await pause(700)
    },
  }),
  scenario({
    id: 'wide-image',
    label: 'OCR · 极宽的截图（3200 × 96）',
    async run(ctx) {
      const line = '面板唤出 p95 ≤ 100 ms · 首 token p95 ≤ 1.2 s · 空闲 CPU ≈ 0% · 常驻内存 ≤ 60 MB'
      await recognized(ctx, line, { size: [3200, 96], thumbLines: [line] })
      await pause(1800)
    },
  }),
  scenario({
    id: 'tall-image',
    label: 'OCR · 极高的截图（220 × 2400）+ 小卡片上限',
    async run(ctx) {
      ctx.stream('r-ocr', ctx.textStream(LONG_TEXT, { chunkChars: 40, intervalMs: 8, model: QWEN }))
      show(
        ctx,
        await ocrPayload(ctx, {
          size: [220, 2400],
          thumbLines: LONG_TEXT.split('\n').slice(0, 40),
        }),
        { maxHeight: 320 },
      )
      await until(phase('done'))
      await pause(1800)
    },
  }),
  scenario({
    id: 'narrow',
    label: 'OCR · 窄面板（360 px）+ 译文区 + 小卡片上限',
    async run(ctx) {
      ctx.stream('r-ocr', ocrStream(ctx, OCR_TEXT))
      show(ctx, await ocrPayload(ctx), { width: 360, maxHeight: 380 })
      await until(phase('done'))
      ctx.nextStream(
        ctx.textStream(OCR_TRANSLATION, {
          model: { provider_id: 'openai', model_id: 'gpt-5.4-mini', model_label: 'GPT-5.4 mini' },
          meta: { detected_source_lang: 'zh-CN', target_lang: 'en' },
        }),
      )
      click('[data-testid="ocr-translate-toggle"]')
      await until(() => $('[data-testid="ocr-translation-body"][data-phase="done"]') !== null)
      await pause(1800)
    },
  }),
  scenario({
    id: 'layout',
    label: 'OCR · 窄面板（360 px）的排版按钮（?mode=lines / paragraphs / markdown）',
    async run(ctx) {
      await layoutPanel(ctx, layoutMode())
      await pause(300)
    },
  }),
  scenario({
    id: 'layout-menu',
    label: 'OCR · 排版菜单（原样 / 去掉空行 / 合并为一行）',
    async run(ctx) {
      await layoutPanel(ctx, 'paragraphs')
      click('[data-testid="ocr-layout-caret"]')
      await until(() => $('[data-testid="ocr-layout-menu"]') !== null)
      await pause(400)
    },
  }),
  scenario({
    id: 'direct',
    label: '截图直接翻译 · 完成',
    async run(ctx) {
      ctx.stream(
        'r-ocr',
        ctx.textStream(OCR_TRANSLATION, {
          chunkChars: 8,
          model: QWEN,
          meta: { detected_source_lang: 'zh-CN', target_lang: 'en' },
          done: { elapsed_ms: 2380 },
        }),
      )
      show(ctx, await ocrPayload(ctx, { mode: 'translate' }))
      await until(phase('done'))
      await pause(500)
    },
  }),
  scenario({
    id: 'direct-source',
    label: '截图直接翻译 · 查看原文（按需转录）',
    async run(ctx) {
      ctx.stream(
        'r-ocr',
        ctx.textStream(OCR_TRANSLATION, {
          chunkChars: 8,
          model: QWEN,
          meta: { detected_source_lang: 'zh-CN', target_lang: 'en' },
        }),
      )
      show(ctx, await ocrPayload(ctx, { mode: 'translate' }))
      await until(phase('done'))
      ctx.nextStream(ocrStream(ctx, OCR_PARAGRAPHS))
      click('[data-testid="ocr-show-source"]')
      await until(() => $('[data-testid="ocr-source-body"][data-phase="done"]') !== null)
      await pause(600)
    },
  }),
  scenario({
    id: 'enter',
    label: 'OCR · 面板进入 → 骨架 → 流式 → 完成（动效）',
    motion: true,
    async run(ctx) {
      ctx.stream('r-ocr', ocrStream(ctx, OCR_TEXT))
      show(ctx, await ocrPayload(ctx))
      await until(phase('streaming'))
      ctx.motionMid()
      await until(phase('done'))
      await pause(400)
    },
  }),
  scenario({
    id: 'long-text-translate',
    label: 'OCR · 长文本 + 长译文（卡片已到上限，两区都在框内滚动）',
    async run(ctx) {
      // 原文取 30 行：不超过长文本阈值（3000 字），点翻译直接开始
      const text = LONG_TEXT.split('\n').slice(0, 30).join('\n')
      ctx.stream('r-ocr', ctx.textStream(text, { chunkChars: 40, intervalMs: 8, model: QWEN }))
      show(
        ctx,
        await ocrPayload(ctx, {
          size: [1720, 1180],
          thumbLines: LONG_TEXT.split('\n').slice(0, 14),
        }),
      )
      await until(phase('done'))
      ctx.nextStream(
        ctx.textStream(LONG_TRANSLATION, {
          chunkChars: 40,
          intervalMs: 8,
          meta: { detected_source_lang: 'en', target_lang: 'zh-CN' },
        }),
      )
      click('[data-testid="ocr-translate-toggle"]')
      await until(() => $('[data-testid="ocr-translation-body"][data-phase="done"]') !== null)
      await pause(800)
    },
  }),
  scenario({
    id: 'long-text-zoom',
    label: 'OCR · 长文本上放大截图（浮层，不挤掉底栏）',
    async run(ctx) {
      ctx.stream('r-ocr', ctx.textStream(LONG_TEXT, { chunkChars: 40, intervalMs: 8, model: QWEN }))
      show(
        ctx,
        await ocrPayload(ctx, {
          size: [1720, 1180],
          thumbLines: LONG_TEXT.split('\n').slice(0, 14),
        }),
      )
      await until(phase('done'))
      await pause(300)
      click('[data-testid="ocr-thumbnail"]')
      await pause(700)
    },
  }),
  scenario({
    id: 'rerecognize',
    label: 'OCR · 重新识别中（旧文字变淡 + 脉冲，底栏保留）',
    async run(ctx) {
      await recognized(ctx)
      ctx.nextStream([{ ...start, request_id: 'r-1' }, { wait: 60_000 }])
      click('[data-testid="ocr-recognize-again"]')
      await pause(600)
    },
  }),
  scenario({
    id: 'translate-restyle',
    label: 'OCR · 译文区换风格（旧译文变淡，区底「翻译中…」占位）',
    motion: true,
    async run(ctx) {
      await recognized(ctx)
      ctx.nextStream(
        ctx.textStream(OCR_TRANSLATION, {
          meta: { detected_source_lang: 'zh-CN', target_lang: 'en' },
          done: { elapsed_ms: 980 },
        }),
      )
      click('[data-testid="ocr-translate-toggle"]')
      await until(() => $('[data-testid="ocr-translation-body"][data-phase="done"]') !== null)
      await pause(700)
      const steps = ctx.textStream(OCR_TRANSLATION.toUpperCase(), {
        firstTokenMs: 500,
        meta: { detected_source_lang: 'zh-CN', target_lang: 'en' },
      })
      ctx.nextStream([...steps.slice(0, Math.round(steps.length * 0.4)), { wait: 60_000 }])
      window.dispatchEvent(
        new KeyboardEvent('keydown', {
          code: 'Digit2',
          key: '2',
          metaKey: ctx.boot.os === 'macos',
          ctrlKey: ctx.boot.os !== 'macos',
          bubbles: true,
        }),
      )
      ctx.motionMid()
      await pause(1200)
    },
  }),
  pinnedScenario({
    id: 'pinned-done',
    label:
      'OCR · 固定窗口：识别结果（36 px 标题栏、缩略图回退到 crop/<id>.png、模型 / 耗时取自回放）',
    motion: true,
    async run(ctx) {
      // 已结束的识别请求：固定窗口只从它的回放里取模型 / 耗时 / 用量（文字用 PinState.ocr_text）
      ctx.stream(
        'r-ocr',
        ctx.textStream(OCR_TEXT, {
          firstTokenMs: 0,
          intervalMs: 0,
          chunkChars: 400,
          model: QWEN,
          done: { elapsed_ms: 2140 },
        }),
      )
      await pinnedOcr(ctx, {})
      ctx.after(200, () => ctx.motionMid())
      await until(() => $('[data-testid="ocr-text"]') !== null)
      await pause(500)
    },
  }),
  pinnedScenario({
    id: 'pinned-translate',
    label: 'OCR · 固定窗口：识别结果 + 译文区（接入译文流的回放）',
    async run(ctx) {
      ctx.stream(
        'r-pin-tr',
        ctx.textStream(OCR_TRANSLATION, {
          firstTokenMs: 40,
          model: { provider_id: 'openai', model_id: 'gpt-5.4-mini', model_label: 'GPT-5.4 mini' },
          meta: { detected_source_lang: 'zh-CN', target_lang: 'en' },
          done: { elapsed_ms: 980 },
        }),
      )
      await pinnedOcr(
        ctx,
        { request_id: 'r-pin-tr', show_source: true, result_text: OCR_TRANSLATION },
        440,
      )
      await until(() => $('[data-testid="ocr-translation-body"][data-phase="done"]') !== null)
      await pause(600)
    },
  }),
  pinnedScenario({
    id: 'pinned-direct',
    label: '截图直接翻译 · 固定窗口（译文 + 已转录的原文）',
    async run(ctx) {
      ctx.stream(
        'r-pin-direct',
        ctx.textStream(OCR_TRANSLATION, {
          firstTokenMs: 40,
          model: QWEN,
          meta: { detected_source_lang: 'zh-CN', target_lang: 'en' },
          done: { elapsed_ms: 2380 },
        }),
      )
      await pinnedOcr(
        ctx,
        {
          request_id: 'r-pin-direct',
          ocr_mode: 'translate',
          ocr_text: OCR_PARAGRAPHS,
          source_text: OCR_PARAGRAPHS,
          result_text: OCR_TRANSLATION,
          show_source: true,
        },
        440,
      )
      await until(() => $('[data-testid="view-ocr"][data-phase="done"]') !== null)
      await pause(600)
    },
  }),
  // ---- 离线模型与 PaddleOCR 云端
  scenario({
    id: 'offline-loading',
    label: 'OCR · 正在加载离线模型（PP-OCRv6 冷启动）',
    async run(ctx) {
      seedOffline(true)
      ctx.stream('r-ocr', [
        { type: 'start', request_id: 'r-ocr', ...OFFLINE_SMALL, upstream_id: null },
        { type: 'phase', phase: 'loading_model' },
        { wait: 60_000 },
      ])
      show(ctx, await ocrPayload(ctx))
      // 「第一次加载需要十几秒」在约 3 秒后才淡入：等它出现再截图
      await pause(3_800)
    },
  }),
  scenario({
    id: 'offline-partial',
    label: 'OCR · PaddleOCR-VL 结构化结果：部分区域未能识别（状态行的提示）',
    async run(ctx) {
      seedOffline(false)
      ctx.stream(
        'r-ocr',
        ctx.textStream(OCR_TABLES, {
          chunkChars: 40,
          intervalMs: 10,
          firstTokenMs: 100,
          model: OFFLINE_VL,
          before: [{ type: 'notice', kind: 'partial' }],
          done: { elapsed_ms: 4_200, usage: null },
        }),
      )
      show(ctx, await ocrPayload(ctx, { mode: 'markdown', size: [1280, 900] }), {
        maxHeight: 640,
      })
      await until(phase('done'))
      await until(() => $('[data-testid="ocr-preview"]') !== null)
      // 自动复制的「已复制」先占住状态行一两秒，收起后才是这条提示
      await pause(2_200)
    },
  }),
  scenario({
    id: 'cloud-queued',
    label: 'OCR · 云端处理中（PaddleOCR PP-StructureV3 排队）',
    async run(ctx) {
      const state = window.__WISP_MOCK__?.state
      seedOffline(false)
      if (state)
        state.settings.defaults.ocr = { provider_id: 'paddleocr', model_id: 'PP-StructureV3' }
      ctx.stream('r-ocr', [
        { type: 'start', request_id: 'r-ocr', ...PADDLE_STRUCTURE, upstream_id: null },
        { type: 'phase', phase: 'queued' },
        { wait: 60_000 },
      ])
      show(ctx, await ocrPayload(ctx, { mode: 'markdown' }))
      await pause(500)
    },
  }),
  scenario({
    id: 'table',
    label: 'OCR · 结构化结果：表格与公式的预览',
    async run(ctx) {
      seedOffline(false)
      ctx.stream(
        'r-ocr',
        ctx.textStream(OCR_TABLES, {
          chunkChars: 24,
          intervalMs: 16,
          firstTokenMs: 200,
          model: PADDLE_STRUCTURE,
          done: { elapsed_ms: 28_400, usage: null },
        }),
      )
      show(ctx, await ocrPayload(ctx, { mode: 'markdown', size: [1280, 900] }), {
        maxHeight: 640,
      })
      await until(phase('done'))
      await until(() => $('[data-testid="ocr-preview"]') !== null)
      await pause(400)
    },
  }),
  scenario({
    id: 'table-source',
    label: 'OCR · 结构化结果：切到 Markdown 源码（可编辑）',
    async run(ctx) {
      seedOffline(false)
      ctx.stream(
        'r-ocr',
        ctx.textStream(OCR_TABLES, {
          chunkChars: 40,
          intervalMs: 10,
          firstTokenMs: 100,
          model: PADDLE_STRUCTURE,
        }),
      )
      show(ctx, await ocrPayload(ctx, { mode: 'markdown', size: [1280, 900] }), {
        maxHeight: 640,
      })
      await until(() => $('[data-testid="ocr-source-toggle"]') !== null)
      click('[data-testid="ocr-source-toggle"]')
      await pause(400)
    },
  }),
  scenario({
    id: 'offline-menu',
    label: 'OCR · 识别模型菜单（离线 / PaddleOCR / 视觉模型分组）',
    async run(ctx) {
      seedOffline(true)
      await recognized(ctx)
      click('[data-testid="ocr-recognize-menu"]')
      await until(() => $('[data-testid="ocr-recognize-list-item-models"]') !== null)
      click('[data-testid="ocr-recognize-list-item-models"]')
      await pause(500)
    },
  }),
  scenario({
    id: 'offline-modes',
    label: 'OCR · 选用 PP-OCRv6 时「结构化」置灰并说明',
    async run(ctx) {
      seedOffline(true)
      ctx.stream(
        'r-ocr',
        ctx.textStream(OCR_TEXT, {
          chunkChars: 8,
          intervalMs: 16,
          firstTokenMs: 120,
          model: OFFLINE_SMALL,
          done: { elapsed_ms: 310, usage: null },
        }),
      )
      show(ctx, await ocrPayload(ctx))
      await until(phase('done'))
      click('[data-testid="ocr-recognize-menu"]')
      await pause(500)
    },
  }),
  scenario({
    id: 'offline-missing',
    label: 'OCR · 未就绪：离线识别模型还没下载（就地下载）',
    async run(ctx) {
      show(
        ctx,
        await ocrPayload(ctx, {
          requestId: null,
          captureId: '',
          noThumb: true,
          notReady: offlineMissing('pp_ocrv6_small'),
        }),
      )
      await pause(500)
    },
  }),
  scenario({
    id: 'offline-downloading',
    label: 'OCR · 未就绪：离线识别模型下载中（42%）',
    async run(ctx) {
      const state = window.__WISP_MOCK__?.state
      if (state) {
        const snap = state.offline.snapshot
        state.offline.snapshot = {
          ...snap,
          models: snap.models.map((m) =>
            m.id === 'paddleocr_vl_1_6'
              ? {
                  ...m,
                  state: {
                    state: 'downloading' as const,
                    done: 0.42 * 1986e6,
                    total: 1986e6,
                    bytes_per_sec: 12.4e6,
                  },
                }
              : m,
          ),
        }
      }
      show(
        ctx,
        await ocrPayload(ctx, {
          requestId: null,
          captureId: '',
          noThumb: true,
          notReady: { reason: 'offline_model_downloading', model: 'paddleocr_vl_1_6' },
        }),
      )
      await pause(500)
    },
  }),
  scenario({
    id: 'offline-failed',
    label: 'OCR · 未就绪：离线识别模型下载失败（文件校验没有通过）',
    async run(ctx) {
      const state = window.__WISP_MOCK__?.state
      if (state) {
        const snap = state.offline.snapshot
        state.offline.snapshot = {
          ...snap,
          models: snap.models.map((m) =>
            m.id === 'pp_ocrv6_small'
              ? { ...m, state: { state: 'failed' as const, code: 'checksum_mismatch' as const } }
              : m,
          ),
        }
      }
      show(
        ctx,
        await ocrPayload(ctx, {
          requestId: null,
          captureId: '',
          noThumb: true,
          notReady: offlineMissing('pp_ocrv6_small'),
        }),
      )
      await pause(500)
    },
  }),
  scenario({
    id: 'offline-ready',
    label: 'OCR · 未就绪卡片：下载完成（提示重新截图）',
    async run(ctx) {
      show(
        ctx,
        await ocrPayload(ctx, {
          requestId: null,
          captureId: '',
          noThumb: true,
          notReady: offlineMissing('pp_ocrv6_small'),
        }),
      )
      await until(() => $('[data-testid="ocr-offline-card-download"]') !== null)
      click('[data-testid="ocr-offline-card-download"]')
      await until(() => $('[data-testid="ocr-offline-retake"]') !== null)
      await pause(400)
    },
  }),
]
