/**
 * OCR 视图。
 *
 * 用假传输层驱动真实的 OcrView：流式命令（stream_attach / ocr_start / translate_start）的事件回调按 request_id 记下，
 * 用例里逐个推送 StreamEvent；普通命令按顺序记录。
 */
import { longTextAck, longTextGate } from '../../../shared/ipc/mock/longText'
import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { ViewProps } from '../../../panel/types'
import { boot, setTransport, type Transport } from '../../../shared/ipc'
import type {
  ClipInspect,
  LlmError,
  ModelEntry,
  OcrPayload,
  PinState,
  Provider,
  SettingsSnapshot,
  StreamEvent,
  TranslateReq,
} from '../../../shared/ipc/types'
import { getAppearance, setAppearance } from '../../../shared/hooks/useAppearance'
import { MotionRoot } from '../../../shared/motion/MotionRoot'
import { PinnedContext, type PinnedHost } from '../../../pinned/context'
import { OCR_MESSAGES } from '../i18n'
import {
  copyTarget,
  hotkeyDisplay,
  layoutFor,
  lineHeight,
  settingsSectionFor,
  thumbMaxWidth,
  visionModels,
} from '../model'
import { OcrView, type OcrViewProps } from '../OcrView'
import { resetSettingsSnapshot } from '../settings'
import { toLlmError } from '../useRun'

type Listener = (e: StreamEvent) => void
let calls: Array<[string, Record<string, unknown>]>
let streams: Map<string, Listener>
let seq: number
/** stream_attach 返回 not_found 的请求（请求已被取代 / 日志已释放） */
let gone: Set<string>
/** stream_attach 以其他错误失败的请求（不是 not_found：不应改为自己发起） */
let broken: Set<string>
/** translate_start 在 Start 之前就被拒绝 */
let rejectTranslate: boolean
/** stream_attach 时先同步回放这些事件再 resolve（与真实的接入一致：resolve 时回放已送达） */
let replays: Map<string, StreamEvent[]>

function model(id: string, vision: ModelEntry['vision'], enabled = true): ModelEntry {
  return {
    id,
    label: null,
    vision,
    vision_source: null,
    reasoning: false,
    enabled,
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

function provider(id: string, models: ModelEntry[]): Provider {
  return {
    id,
    name: id.toUpperCase(),
    protocol: 'openai_chat',
    base_url: 'https://x',
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
    timeouts: { connect_ms: 1, first_token_ms: 1, idle_ms: 1 },
    template_id: null,
  }
}

const STYLES = ['natural', 'technical', 'academic'].map((id) => ({
  id,
  name: '',
  builtin: true,
  system_template: '',
  tail_instruction: '',
  temperature: null,
  output: 'plain_text' as const,
  dictionary_mode: false,
  model_ref: null,
  hotkey: null,
  modified: false,
}))

function snapshot(autoCopy = true): SettingsSnapshot {
  return {
    settings: {
      providers: [
        provider('qwen', [model('vl-plus', 'yes'), model('max', 'no'), model('off', 'yes', false)]),
        provider('ollama', [model('llava', 'unknown')]),
      ],
      defaults: { translate: null, ocr: { provider_id: 'qwen', model_id: 'vl-plus' } },
      translate: { default_style: 'natural', primary_lang: 'zh-CN' },
      ocr: { auto_copy: autoCopy },
    },
    styles: STYLES,
    runtime: {
      hotkeys: [
        {
          target: { kind: 'action', action: 'ocr' },
          binding: 'default',
          effective: 'Alt+KeyS',
          display: '⌥S',
          state: 'registered',
          reason_key: null,
        },
      ],
    },
  } as unknown as SettingsSnapshot
}

const NO_ENTITIES: ClipInspect = { entities: [], transforms: [] }
/**
 * text_inspect 的结果：undefined 模拟没实现这条命令的替身（未知命令一律返回 undefined），'fail' 模拟命令失败；函数按
 * 检视的文字给出结果
 */
let inspectResult: ClipInspect | undefined | 'fail' | ((text: string) => ClipInspect) = NO_ENTITIES
/** ocr_recopy 的结果：Rust 是否写入了剪贴板 */
let recopyResult: boolean
/** 不为 null 时 text_transform 等它 resolve 之后才返回（模拟排版结果还没到） */
let transformHold: Promise<void> | null
/** 不为 null 时 clipboard_paste_text 以这个错误码失败 */
let pasteError: string | null
/** 不为 null 时 clipboard_copy_text 以 platform + 这个原因失败（例如要求改由 WebView 写入） */
let copyRefusal: string | null

/** text_transform 的替身（只为用例里的几段文字给出结果；真实规则在 Rust） */
function fakeTransform(text: string, transform: string): string {
  if (transform === 'remove_blank_lines') return text.replace(/\n{2,}/g, '\n')
  if (transform === 'join_lines') return text.replace(/\n+/g, '')
  // 合并段内换行：段内的单个换行连起来，空行（段界）保留
  if (transform === 'reflow') return text.replace(/([^\n])\n(?=[^\n])/g, '$1')
  return text
}

function fakeTransport(): Transport {
  return {
    kind: 'mock',
    invoke: <T,>(cmd: string, args?: Record<string, unknown>) => {
      calls.push([cmd, args ?? {}])
      if (cmd === 'panel_pin') return Promise.resolve('p-1' as T)
      if (cmd === 'text_transform') {
        const out = fakeTransform(args?.text as string, args?.transform as string) as T
        return transformHold ? transformHold.then(() => out) : Promise.resolve(out)
      }
      if (cmd === 'ocr_recopy') return Promise.resolve(recopyResult as T)
      if (cmd === 'clipboard_paste_text' && pasteError) {
        return Promise.reject({ code: pasteError, message: 'paste failed', llm: null })
      }
      if (cmd === 'clipboard_copy_text' && copyRefusal) {
        return Promise.reject({ code: 'platform', message: copyRefusal, llm: null })
      }
      // 识别完成后视图会检视文字（芯片）：默认没有片段，用例需要芯片时改 inspectResult
      if (cmd === 'text_inspect') {
        if (inspectResult === 'fail') {
          return Promise.reject({ code: 'internal', message: 'inspect failed', llm: null })
        }
        if (typeof inspectResult === 'function') {
          return Promise.resolve(inspectResult(args?.text as string) as T)
        }
        // 默认没有片段；可用的排版按替身的结果列出（与 Rust 一样：列出的恰好是会改变文字的那些）
        if (inspectResult === NO_ENTITIES) {
          const text = args?.text as string
          const transforms = (['join_lines', 'reflow', 'remove_blank_lines'] as const).filter(
            (id) => fakeTransform(text, id) !== text,
          )
          return Promise.resolve({ entities: [], transforms } as T)
        }
        return Promise.resolve(inspectResult as T)
      }
      return Promise.resolve(undefined as T)
    },
    openStream: <T,>(cmd: string, args: Record<string, unknown>, onEvent: Listener) => {
      calls.push([cmd, args])
      if (cmd === 'stream_attach') {
        const id = args.request_id as string
        if (gone.has(id)) {
          return Promise.reject({ code: 'not_found', message: `unknown request ${id}`, llm: null })
        }
        if (broken.has(id)) {
          return Promise.reject({ code: 'internal', message: 'ipc channel closed', llm: null })
        }
        streams.set(id, onEvent)
        for (const e of replays.get(id) ?? []) onEvent(e)
        return Promise.resolve(undefined as T)
      }
      if (cmd === 'translate_start' && rejectTranslate) {
        return Promise.reject({ code: 'invalid_argument', message: 'text too long', llm: null })
      }
      // 与 Rust 同一长文本闸门（阈值取默认 3000）
      if (cmd === 'translate_start') {
        const long = longTextGate(args.req as TranslateReq, 3000)
        if (long) return Promise.resolve(longTextAck(long, null) as T)
      }
      const id = `r-${++seq}`
      streams.set(id, onEvent)
      return Promise.resolve({ request_id: id, width: null } as T)
    },
    listen: () => Promise.resolve(() => {}),
    window: {} as Transport['window'],
    protocolUrl: (p) => p,
  }
}

const payload = (extra: Partial<OcrPayload> = {}): ViewProps<'ocr'>['payload'] => ({
  kind: 'ocr',
  capture_id: 'cap-1',
  request_id: 'r-ocr',
  mode: 'lines',
  thumb_url: 'crop/cap-1.png',
  image_size: { width: 1280, height: 480 },
  target_lang: 'zh-CN',
  not_ready: null,
  layout: 'keep',
  ...extra,
})

function view(extra: Partial<OcrViewProps> = {}, p = payload()) {
  const props: OcrViewProps = {
    sessionId: 3,
    payload: p,
    phase: 'shown',
    focused: true,
    host: 'panel',
    frame: 'native',
    placement: 'below',
    maxHeight: 520,
    requestHide: vi.fn<(reason: string) => void>(),
    switchView: vi.fn<OcrViewProps['switchView']>(),
    activate: vi.fn<() => void>(),
    ...extra,
  }
  const r = render(
    <MotionRoot>
      <OcrView {...props} />
    </MotionRoot>,
  )
  return { ...r, props }
}

/** 挂载后识别请求在紧随其后的微任务里接上 / 发起 */
const started = () => act(async () => {})

/** 往某个请求推事件（act 包裹，等状态落定） */
async function push(id: string, ...events: StreamEvent[]) {
  await started()
  const cb = streams.get(id)
  if (!cb) throw new Error(`没有请求 ${id}`)
  await act(async () => {
    for (const e of events) cb(e)
    await Promise.resolve()
  })
}

const start = (id: string): StreamEvent => ({
  type: 'start',
  request_id: id,
  provider_id: 'qwen',
  model_id: 'vl-plus',
  model_label: 'vl-plus',
  upstream_id: null,
})
const delta = (text: string): StreamEvent => ({ type: 'delta', text })
const done = (extra: Partial<Extract<StreamEvent, { type: 'done' }>> = {}): StreamEvent => ({
  type: 'done',
  elapsed_ms: 1200,
  usage: { input_tokens: 10, output_tokens: 20, reasoning_tokens: null, total_tokens: 30 },
  truncated: null,
  cached: false,
  no_text: false,
  ...extra,
})
const error = (code: LlmError['code']): StreamEvent => ({
  type: 'error',
  code,
  subcode: null,
  message: code,
  retryable: true,
  hint: null,
  status: null,
  url: null,
  retry_after_s: null,
  upstream_request_id: null,
})

async function recognize(id = 'r-ocr', text = '第一行\n第二行') {
  await push(id, start(id), delta(text.slice(0, 2)), delta(text.slice(2)), done())
}

const callsOf = (cmd: string) => calls.filter(([c]) => c === cmd)
const settle = () => act(async () => new Promise((r) => setTimeout(r, 0)))
function key(code: string, init: KeyboardEventInit = {}) {
  window.dispatchEvent(
    new KeyboardEvent('keydown', {
      code,
      key: code.replace(/^Key|^Digit/, '').toLowerCase(),
      bubbles: true,
      cancelable: true,
      ...init,
    }),
  )
}
/** `Mod` 按 boot.os 展开（macOS = ⌘，其他平台 = Ctrl） */
const mod = (code: string) => key(code, boot.os === 'macos' ? { metaKey: true } : { ctrlKey: true })

const savedAppearance = getAppearance()

beforeEach(() => {
  calls = []
  streams = new Map()
  seq = 0
  gone = new Set()
  broken = new Set()
  rejectTranslate = false
  replays = new Map()
  inspectResult = NO_ENTITIES
  recopyResult = true
  transformHold = null
  pasteError = null
  copyRefusal = null
  setTransport(fakeTransport())
  resetSettingsSnapshot(snapshot())
  // 用例里的断言用中文文案
  setAppearance({ ...savedAppearance, uiLanguage: 'zh-CN', os: 'macos' })
})
afterEach(() => {
  setTransport(fakeTransport())
  resetSettingsSnapshot()
  setAppearance(savedAppearance)
})

// ---------------------------------------------------------------------------

describe('纯函数', () => {
  it('视觉模型：只列已启用、vision 为 yes / unknown', () => {
    const list = visionModels(snapshot())
    expect(list.map((v) => `${v.ref.provider_id}/${v.ref.model_id}`)).toEqual([
      'qwen/vl-plus',
      'ollama/llava',
    ])
    expect(list.map((v) => v.unconfirmed)).toEqual([false, true])
  })
  it('⌘C 的复制对象：选区 > 译文 > OCR 文本', () => {
    expect(copyTarget('选中', '译文', 'OCR')).toBe('选中')
    expect(copyTarget('', '译文', 'OCR')).toBe('译文')
    expect(copyTarget(' ', null, 'OCR')).toBe('OCR')
  })
  it('版式：卡片上限低时缩略图更细；两区都放不下时正文区至少保留 40% / 3 行', () => {
    const lh = lineHeight(14)
    const normal = layoutFor({ maxHeight: 466, fontSize: 14, direct: false })
    expect(normal.thumb).toBe(72)
    expect(normal.viewMax).toBe(466 - 24)
    const shared = 466 - 24 - 72 - 12 - 40 - 56 - 30
    expect(normal.mainFloor).toBeCloseTo(Math.max(3 * lh, shared * 0.4))
    // 上限 380：缩略图收成 48 px，正文至少 3 行
    const low = layoutFor({ maxHeight: 380, fontSize: 14, direct: false })
    expect(low.thumb).toBe(48)
    expect(low.mainFloor).toBeGreaterThanOrEqual(3 * lh)
    // 窄面板：缩略图不超过内容宽的 38%（给「模式 · 模型」留位置）
    expect(thumbMaxWidth(336)).toBeLessThanOrEqual(Math.round(336 * 0.38))
    expect(thumbMaxWidth(408)).toBe(224)
  })
  it('失败原因原样转成 LlmError（no_vision 等卡片的按钮才对得上）', () => {
    const llm: LlmError = {
      code: 'no_vision',
      subcode: null,
      message: 'model does not support image input',
      retryable: false,
      hint: null,
      status: 400,
      url: null,
      retry_after_s: null,
      upstream_request_id: null,
    }
    expect(toLlmError(llm)).toBe(llm)
    expect(toLlmError({ code: 'internal', message: 'boom', llm }).code).toBe('no_vision')
    expect(toLlmError({ code: 'invalid_argument', message: 'bad', llm: null })).toMatchObject({
      code: 'internal',
      message: 'bad',
    })
  })
  it('设置分组与快捷键显示', () => {
    expect(settingsSectionFor('no_vision')).toBe('ocr')
    expect(settingsSectionFor('auth')).toBe('providers')
    expect(hotkeyDisplay(snapshot(), 'ocr')).toBe('⌥S')
    expect(hotkeyDisplay(snapshot(), 'ocr_translate')).toBeNull()
  })
  it('ocr / overlay 文案的中英键集合一致', async () => {
    const keys = (o: object, prefix = ''): string[] =>
      Object.entries(o).flatMap(([k, v]) =>
        typeof v === 'object' && v !== null ? keys(v, `${prefix}${k}.`) : [`${prefix}${k}`],
      )
    expect(keys(OCR_MESSAGES['en-US']).sort()).toEqual(keys(OCR_MESSAGES['zh-CN']).sort())
    const [zh, en] = await Promise.all([
      import('../../../locales/zh-CN/overlay.json'),
      import('../../../locales/en-US/overlay.json'),
    ])
    expect(keys(en.default).sort()).toEqual(keys(zh.default).sort())
  })
})

describe('识别流程', () => {
  it('骨架 → 流式只读 → 完成后可编辑；底栏完成后才出现', async () => {
    view()
    // 挂载的同步路径里不发请求（prepare 只渲染一次），紧随其后的微任务里接上
    expect(callsOf('stream_attach')).toHaveLength(0)
    await started()
    expect(callsOf('stream_attach')).toEqual([['stream_attach', { request_id: 'r-ocr' }]])
    expect(screen.getByTestId('skeleton')).toBeInTheDocument()
    expect(screen.queryByTestId('ocr-footer')).toBeNull()
    await push('r-ocr', start('r-ocr'), delta('第一'))
    expect(screen.queryByTestId('skeleton')).toBeNull()
    expect(screen.getByTestId('stream-view')).toHaveTextContent('第一')
    expect(screen.queryByTestId('ocr-text')).toBeNull()
    await push('r-ocr', delta('行'), done())
    const text = screen.getByTestId('ocr-text') as HTMLTextAreaElement
    expect(text.value).toBe('第一行')
    expect(screen.getByTestId('ocr-footer')).toBeInTheDocument()
    expect(screen.getByTestId('ocr-meta-status')).toHaveTextContent('vl-plus')
  })

  it('底栏状态行是「模型 · 耗时 · tokens」；两个复制按钮的名称不同；读屏播报阶段', async () => {
    view()
    await started()
    expect(screen.getByTestId('ocr-announce')).toHaveTextContent('识别中…')
    await recognize()
    const footer = screen.getByTestId('ocr-footer')
    expect(within(footer).getByTestId('status-line')).toHaveTextContent(/vl-plus.*1\.2 s/)
    expect(screen.getByTestId('ocr-announce')).toHaveTextContent('识别完成')
    expect(screen.getByTestId('ocr-copy')).toHaveAccessibleName(/复制识别文字/)
    fireEvent.click(screen.getByTestId('ocr-translate-toggle'))
    expect(screen.getByTestId('ocr-translation-copy')).toHaveAccessibleName(/复制译文/)
    // 缩略图按钮会打开浮层：aria-haspopup / aria-expanded
    const thumb = screen.getByTestId('ocr-thumbnail')
    expect(thumb).toHaveAttribute('aria-haspopup', 'dialog')
    expect(thumb).toHaveAttribute('aria-expanded', 'false')
  })

  it('出错时头部仍是「模式 · 模型」，失败只由错误卡片表达', async () => {
    view()
    await push('r-ocr', start('r-ocr'), error('network'))
    expect(screen.getByTestId('ocr-meta-status')).toHaveTextContent('纯文本 · vl-plus')
    expect(screen.getByTestId('ocr-announce')).toHaveTextContent('识别失败')
  })

  it('载荷没有 request_id 时自己发起 ocr_start', async () => {
    view({}, payload({ request_id: null, mode: 'paragraphs', layout: 'remove_blank_lines' }))
    await started()
    expect(callsOf('ocr_start')[0]?.[1]).toEqual({
      req: {
        session_id: 3,
        capture_id: 'cap-1',
        mode: 'paragraphs',
        model_ref: null,
        target_lang: null,
        // 会话排版（初值是载荷里的设置默认值）：Rust 按它自动复制
        layout: 'remove_blank_lines',
      },
    })
  })

  it('编辑后复制的是编辑后的文本', async () => {
    view()
    await recognize()
    fireEvent.change(screen.getByTestId('ocr-text'), { target: { value: '改过的' } })
    fireEvent.click(screen.getByTestId('ocr-copy'))
    await settle()
    expect(callsOf('clipboard_copy_text').at(-1)?.[1]).toEqual({ text: '改过的' })
    expect(screen.getByTestId('status-line')).toHaveTextContent('已编辑')
  })

  it('未检测到文字：空状态 + [切换模式]（打开模式菜单），底栏只留重新识别与固定，不自动复制', async () => {
    view()
    await push('r-ocr', start('r-ocr'), done({ no_text: true }))
    expect(screen.getByTestId('ocr-empty')).toHaveTextContent('未检测到文字')
    await settle()
    expect(callsOf('clipboard_copy_text')).toHaveLength(0)
    expect(screen.queryByTestId('ocr-copy')).toBeNull()
    expect(screen.queryByTestId('ocr-translate')).toBeNull()
    expect(screen.getByTestId('ocr-recognize-again')).toBeInTheDocument()
    // 打开的是选择（菜单），不是立即重新识别
    const action = screen.getByTestId('ocr-empty-action')
    expect(action).toHaveAttribute('aria-haspopup', 'menu')
    expect(action).toHaveAttribute('aria-expanded', 'false')
    fireEvent.click(action)
    expect(await screen.findByTestId('ocr-recognize-list')).toBeInTheDocument()
    expect(action).toHaveAttribute('aria-expanded', 'true')
    fireEvent.click(await screen.findByRole('menuitemcheckbox', { name: /段落/ }))
    expect(callsOf('ocr_start').at(-1)?.[1]).toMatchObject({ req: { mode: 'paragraphs' } })
  })

  it('思考耗尽（max_tokens 且没有文字）优先于「未检测到文字」：空状态说明原因并给出 [切换模型] [调高上限]，状态行不重复原因', async () => {
    view()
    await push(
      'r-ocr',
      start('r-ocr'),
      delta('  '),
      done({ truncated: 'max_tokens', no_text: true }),
    )
    const empty = screen.getByTestId('ocr-empty')
    expect(empty).toHaveTextContent('没有产出识别结果')
    expect(empty).toHaveTextContent('可以切换模型，或调高输出上限。')
    // 空状态已经说明原因：状态行只留模型 · 耗时（琥珀色的重复原因把模型名挤掉）
    const status = screen.getByTestId('status-line')
    expect(status).not.toHaveTextContent('模型思考耗尽了输出额度')
    expect(status.querySelector('.lucide-brain-circuit')).toBeNull()
    // 说明里的两条出路都有按钮：调高上限去设置 › 模型服务
    fireEvent.click(screen.getByTestId('ocr-empty-raise'))
    expect(callsOf('window_open').at(-1)?.[1]).toEqual({
      target: { kind: 'settings', section: 'providers' },
    })
    fireEvent.click(screen.getByTestId('ocr-empty-action'))
    const menu = await screen.findByTestId('ocr-model-menu')
    expect(within(menu).getByText('vl-plus')).toBeInTheDocument()
  })

  it('取消后保留已收到的文字（可编辑），「已取消」只在状态行出现一次', async () => {
    view()
    await push('r-ocr', start('r-ocr'), delta('半截'), error('cancelled'))
    expect((screen.getByTestId('ocr-text') as HTMLTextAreaElement).value).toBe('半截')
    expect(screen.getByTestId('status-line')).toHaveTextContent('已取消')
    expect(screen.getByTestId('ocr-meta-status')).not.toHaveTextContent('已取消')
  })

  it('stream_attach 以 not_found 之外的原因失败：显示错误卡片，不重复发起', async () => {
    broken.add('r-ocr')
    view()
    await started()
    await settle()
    expect(callsOf('ocr_start')).toHaveLength(0)
    expect(screen.getByTestId('view-ocr')).toHaveAttribute('data-phase', 'error')
  })
})

describe('自动复制', () => {
  it('编排器已写入剪贴板：识别完成显示「已复制」，视图不再写一次', async () => {
    view()
    await recognize()
    await settle()
    expect(screen.getByTestId('inline-toast')).toHaveTextContent('已复制')
    expect(callsOf('clipboard_copy_text')).toHaveLength(0)
  })

  it('完成后编辑文本：「已复制」立即收起（剪贴板里已不是眼前的内容）', async () => {
    view()
    await recognize()
    expect(screen.getByTestId('inline-toast')).toHaveTextContent('已复制')
    fireEvent.change(screen.getByTestId('ocr-text'), { target: { value: '改了' } })
    // 收起（toast 的退出动画结束后移除；不等 1.5 s 的停留）
    await waitFor(() => expect(screen.queryByTestId('inline-toast')).toBeNull(), { timeout: 1000 })
  })

  it('设置关闭、未检测到文字时不提示', async () => {
    resetSettingsSnapshot(snapshot(false))
    const first = view()
    await recognize()
    await settle()
    expect(screen.queryByTestId('inline-toast')).toBeNull()
    first.unmount()
    resetSettingsSnapshot(snapshot(true))
    view()
    await push('r-ocr', start('r-ocr'), done({ no_text: true }))
    await settle()
    expect(screen.queryByTestId('inline-toast')).toBeNull()
  })

  it('请求被取代（attach 返回 not_found）时改为用同一张截图重新发起', async () => {
    gone.add('r-ocr')
    view()
    await started()
    await settle()
    expect(callsOf('ocr_start')[0]?.[1]).toMatchObject({
      req: { capture_id: 'cap-1', mode: 'lines', session_id: 3 },
    })
    await recognize('r-1', '重新发起')
    expect((screen.getByTestId('ocr-text') as HTMLTextAreaElement).value).toBe('重新发起')
  })
})

describe('识别出的片段（芯片）', () => {
  it('识别完成后检视文字：片段排成芯片，单击粘贴片段（不进历史）、⌥ 单击只复制并提示', async () => {
    inspectResult = {
      entities: [
        { kind: 'phone', value: '13800138000', start: 4, end: 17, sensitive: false },
        { kind: 'otp', value: '482913', start: 22, end: 28, sensitive: false },
      ],
      transforms: [],
    }
    view()
    await recognize('r-ocr', '联系电话 138 0013 8000，验证码 482913')
    await settle()
    expect(callsOf('text_inspect')).toEqual([
      ['text_inspect', { text: '联系电话 138 0013 8000，验证码 482913' }],
    ])
    const chips = await screen.findAllByTestId('clip-chip')
    // 验证码排在最前（展示注册表的顺序）
    expect(chips.map((c) => c.textContent)).toEqual(['482913', '13800138000'])
    fireEvent.click(chips[0] as HTMLElement)
    expect(callsOf('clipboard_paste_text')).toEqual([
      ['clipboard_paste_text', { text: '482913', from: null, mode: 'normal' }],
    ])
    fireEvent.click(chips[1] as HTMLElement, { altKey: true })
    await waitFor(() =>
      expect(callsOf('clipboard_copy_text')).toEqual([
        ['clipboard_copy_text', { text: '13800138000' }],
      ]),
    )
    expect(callsOf('clipboard_paste_text')).toHaveLength(1)
  })

  it('没有片段时不显示；编辑之后旧的芯片立即收起，停下来再重新检视', async () => {
    view()
    await recognize()
    await settle()
    expect(screen.queryByTestId('ocr-chips')).toBeNull()
    inspectResult = {
      entities: [{ kind: 'email', value: 'a@b.co', start: 0, end: 6, sensitive: false }],
      transforms: [],
    }
    fireEvent.change(screen.getByTestId('ocr-text'), { target: { value: 'a@b.co' } })
    await waitFor(() => expect(screen.getByTestId('ocr-chips')).toHaveTextContent('a@b.co'))
    expect(callsOf('text_inspect')).toHaveLength(2)
  })

  it('检视结果缺失（替身没实现 text_inspect）或检视失败：不显示芯片，也不留下未处理的 Promise 拒绝', async () => {
    // 应用代码的类型里没有 Node；这里只借用进程的 unhandledRejection 事件确认拒绝都有人处理
    const node = (globalThis as unknown as { process: NodeEvents }).process
    const unhandled: unknown[] = []
    const onUnhandled = (reason: unknown) => unhandled.push(reason)
    node.on('unhandledRejection', onUnhandled)
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
    try {
      inspectResult = undefined
      view()
      await recognize()
      await settle()
      expect(callsOf('text_inspect')).toHaveLength(1)
      expect(screen.queryByTestId('ocr-chips')).toBeNull()
      // 结果缺失只是没有片段，不算失败
      expect(warn).not.toHaveBeenCalled()

      inspectResult = 'fail'
      fireEvent.change(screen.getByTestId('ocr-text'), { target: { value: 'a@b.co' } })
      await waitFor(() => expect(callsOf('text_inspect')).toHaveLength(2))
      await settle()
      expect(screen.queryByTestId('ocr-chips')).toBeNull()
      // 失败只记错误码，不记文字
      expect(warn).toHaveBeenCalledWith('[ocr] text_inspect failed: internal')
      // Node 在微任务清空之后才报告未处理的拒绝：再等一轮宏任务
      await settle()
      expect(unhandled).toEqual([])
    } finally {
      node.off('unhandledRejection', onUnhandled)
    }
  })
})

interface NodeEvents {
  on(event: 'unhandledRejection', listener: (reason: unknown) => void): void
  off(event: 'unhandledRejection', listener: (reason: unknown) => void): void
}

describe('重新识别', () => {
  it('模式菜单：切到结构化 → 用同一张截图重新识别（不重新截图）', async () => {
    view()
    await recognize()
    fireEvent.click(screen.getByTestId('ocr-recognize-menu'))
    fireEvent.click(await screen.findByRole('menuitemcheckbox', { name: /结构化/ }))
    expect(callsOf('ocr_start').at(-1)?.[1]).toMatchObject({
      req: { capture_id: 'cap-1', mode: 'markdown', model_ref: null },
    })
    // 文本区「回到骨架」：旧文字变淡留在原处（骨架同节奏的脉冲），新结果到达时替换
    expect(screen.getByTestId('ocr-stream')).toHaveAttribute('data-ghost', 'true')
    expect(screen.getByTestId('ocr-stream')).toHaveTextContent('第一行')
  })

  it('视觉模型菜单不列 vision = no；unknown 带提示；选择后带 model_ref 重新识别', async () => {
    view()
    await recognize()
    fireEvent.click(screen.getByTestId('ocr-recognize-menu'))
    fireEvent.click(await screen.findByRole('menuitem', { name: /识别模型/ }))
    // 同一个浮层里换成模型页，顶部有「‹ 识别模型」返回行
    // 换页时旧页还在淡出：只看当前页
    const menu = (await screen.findByTestId('ocr-model-menu')).querySelector<HTMLElement>(
      '[data-page="models"]',
    ) as HTMLElement
    expect(within(menu).getByTestId('ocr-model-menu-item-back')).toHaveAccessibleName(
      '返回识别菜单',
    )
    expect(within(menu).queryByText('max')).toBeNull()
    expect(within(menu).getByText('vl-plus')).toBeInTheDocument()
    expect(within(menu).getByText('未确认是否支持图片')).toBeInTheDocument()
    fireEvent.click(within(menu).getByText('llava'))
    expect(callsOf('ocr_start').at(-1)?.[1]).toMatchObject({
      req: { model_ref: { provider_id: 'ollama', model_id: 'llava' } },
    })
  })

  it('no_vision 错误卡片：[选择模型] 打开模型列表，[打开设置] 打开 OCR 设置', async () => {
    const { props } = view()
    await push('r-ocr', start('r-ocr'), error('no_vision'))
    fireEvent.click(screen.getByRole('button', { name: '选择模型' }))
    expect(await screen.findByTestId('ocr-model-menu')).toBeInTheDocument()
    fireEvent.click(screen.getByRole('button', { name: '打开设置' }))
    expect(callsOf('window_open').at(-1)?.[1]).toEqual({
      target: { kind: 'settings', section: 'ocr' },
    })
    expect(props.requestHide).toHaveBeenCalledWith('other')
  })

  it('视觉模型页：← 返回模式页，→ 在「视觉模型 ›」上进入', async () => {
    view()
    await recognize()
    fireEvent.click(screen.getByTestId('ocr-recognize-menu'))
    const list = await screen.findByTestId('ocr-recognize-list')
    fireEvent.click(within(list).getByTestId('ocr-recognize-list-item-models'))
    const models = await screen.findByTestId('ocr-model-menu')
    fireEvent.keyDown(models, { key: 'ArrowLeft' })
    const back = await screen.findByTestId('ocr-recognize-list')
    // 同一个浮层（没有卸载重建），旧页在淡出
    expect(back).toBe(models)
    // 高亮停在「视觉模型 ›」上时按 →
    const page = back.querySelector<HTMLElement>(
      '[data-page="main"]:not([aria-hidden])',
    ) as HTMLElement
    const item = within(page).getByTestId('ocr-recognize-list-item-models')
    // 悬停只认真实的指针移动：第一次只记基线，坐标变了才高亮（usePointerIntent）
    fireEvent.pointerMove(item, { clientX: 20, clientY: 40 })
    fireEvent.pointerMove(item, { clientX: 24, clientY: 41 })
    fireEvent.keyDown(back, { key: 'ArrowRight' })
    expect(await screen.findByTestId('ocr-model-menu')).toBeInTheDocument()
  })

  it('编辑后 ⌘R：先出确认条（警示语调、按钮带 ⌘R 提示），再按 ⌘R 才重新识别', async () => {
    view()
    await recognize()
    fireEvent.change(screen.getByTestId('ocr-text'), { target: { value: '改过的' } })
    act(() => mod('KeyR'))
    expect(screen.getByTestId('ocr-confirm')).toHaveTextContent('重新识别会覆盖你的修改')
    expect(screen.getByTestId('ocr-confirm-ok').querySelector('kbd, .ui-kbd-inline')).not.toBeNull()
    expect(callsOf('ocr_start')).toHaveLength(0)
    act(() => mod('KeyR'))
    expect(callsOf('ocr_start')).toHaveLength(1)
    // 旧文字变淡留在原处，底栏保留
    expect(screen.getByTestId('ocr-stream')).toHaveAttribute('data-ghost', 'true')
    expect(screen.getByTestId('ocr-footer')).toBeInTheDocument()
  })

  it('换模式时取消仍在进行的旧请求', async () => {
    view()
    await push('r-ocr', start('r-ocr'), delta('半'))
    act(() => mod('KeyR'))
    expect(callsOf('cancel')).toEqual([['cancel', { request_id: 'r-ocr' }]])
  })
})

describe('排版', () => {
  const PARAS = '第一段\n\n第二段'
  const TIGHT = '第一段\n第二段'
  const box = () => screen.getByTestId('ocr-text') as HTMLTextAreaElement

  it('逐行模式有文字时底栏出现排版按钮；切换后正文更新、aria-pressed；剪贴板跟随时「已复制」重新出现、否则收起，并播报', async () => {
    view()
    await recognize('r-ocr', PARAS)
    await settle()
    const toggle = screen.getByTestId('ocr-layout-toggle')
    expect(toggle).toHaveAttribute('aria-pressed', 'false')
    expect(toggle).toHaveAccessibleName('去掉空行')
    // 剪贴板没能跟随（期间被别处复制过等）：「已复制」收起
    recopyResult = false
    fireEvent.click(toggle)
    await settle()
    expect(box().value).toBe(TIGHT)
    expect(toggle).toHaveAttribute('aria-pressed', 'true')
    expect(callsOf('text_transform')).toEqual([
      ['text_transform', { text: PARAS, transform: 'remove_blank_lines' }],
    ])
    expect(callsOf('ocr_recopy')).toEqual([
      ['ocr_recopy', { capture_id: 'cap-1', request_id: 'r-ocr', text: TIGHT }],
    ])
    await waitFor(() => expect(screen.queryByTestId('inline-toast')).toBeNull(), { timeout: 1000 })
    expect(screen.getByTestId('ocr-announce')).toHaveTextContent(/^已切换为「去掉空行」$/)
    // 跟随成功：「已复制」重新出现；切回原样不用再求排版
    recopyResult = true
    fireEvent.click(toggle)
    await settle()
    expect(box().value).toBe(PARAS)
    expect(toggle).toHaveAttribute('aria-pressed', 'false')
    expect(callsOf('text_transform')).toHaveLength(1)
    expect(callsOf('ocr_recopy').at(-1)?.[1]).toEqual({
      capture_id: 'cap-1',
      request_id: 'r-ocr',
      text: PARAS,
    })
    expect(screen.getByTestId('inline-toast')).toHaveTextContent('已复制')
    expect(screen.getByTestId('ocr-announce')).toHaveTextContent('已切换为「原样」，已复制')
  })

  it('读屏不重复：切换排版的播报已经带上「已复制」，底栏的「已复制」只显示不播报；⌘C 的「已复制」照常播报', async () => {
    view()
    await recognize('r-ocr', PARAS)
    await settle()
    fireEvent.click(screen.getByTestId('ocr-layout-toggle'))
    await settle()
    expect(screen.getByTestId('ocr-announce')).toHaveTextContent('已切换为「去掉空行」，已复制')
    const toast = screen.getByTestId('inline-toast')
    expect(toast).toHaveTextContent('已复制')
    // 播报只有一处：底栏提示从无障碍树里隐去（它所在的 aria-live 区域不再念一遍）
    expect(toast).toHaveAttribute('aria-hidden', 'true')
    expect(screen.queryByRole('status', { name: '已复制' })).toBeNull()
    act(() => mod('KeyC'))
    await settle()
    expect(screen.getByTestId('inline-toast')).not.toHaveAttribute('aria-hidden')
  })

  it('「合并段内换行」只在逐行模式的菜单里出现；选中后段内换行连起来，左半随之换成它', async () => {
    const WRAPPED = '被硬换行\n断开的句子\n\n第二段'
    const first = view()
    await recognize('r-ocr', WRAPPED)
    await settle()
    fireEvent.click(screen.getByTestId('ocr-layout-caret'))
    const menu = await screen.findByTestId('ocr-layout-menu')
    expect(within(menu).getAllByRole('menuitemradio')).toHaveLength(4)
    const reflow = screen.getByTestId('ocr-layout-menu-item-reflow')
    expect(reflow).toHaveTextContent('合并段内换行')
    expect(reflow).toHaveTextContent('把 PDF 里的硬换行连成段落')
    fireEvent.click(reflow)
    await settle()
    expect(box().value).toBe('被硬换行断开的句子\n\n第二段')
    expect(callsOf('text_transform').at(-1)?.[1]).toEqual({ text: WRAPPED, transform: 'reflow' })
    expect(screen.getByTestId('ocr-layout-toggle')).toHaveAccessibleName('合并段内换行')
    expect(screen.getByTestId('ocr-layout-toggle')).toHaveAttribute('aria-pressed', 'true')
    first.unmount()

    view({}, payload({ mode: 'paragraphs' }))
    await recognize('r-ocr', WRAPPED)
    await settle()
    fireEvent.click(screen.getByTestId('ocr-layout-caret'))
    await screen.findByTestId('ocr-layout-menu')
    expect(screen.queryByTestId('ocr-layout-menu-item-reflow')).toBeNull()
    expect(screen.getAllByRole('menuitemradio')).toHaveLength(3)
  })

  it('没有可合并的段内换行时置灰并写明原因', async () => {
    inspectResult = { entities: [], transforms: ['join_lines', 'remove_blank_lines'] }
    view()
    await recognize('r-ocr', PARAS)
    await settle()
    fireEvent.click(screen.getByTestId('ocr-layout-caret'))
    const reflow = await screen.findByTestId('ocr-layout-menu-item-reflow')
    expect(reflow).toHaveAttribute('aria-disabled', 'true')
    expect(reflow).toHaveTextContent('没有可以合并的段内换行')
  })

  it('会话排版在当前模式下不可用：逐行选了合并段内换行、换到段落模式重新识别后显示识别文本，按钮与勾选回到原样（与交付一致）', async () => {
    const WRAPPED = '被硬换行\n断开的句子'
    view()
    await recognize('r-ocr', WRAPPED)
    await settle()
    fireEvent.click(screen.getByTestId('ocr-layout-caret'))
    fireEvent.click(await screen.findByTestId('ocr-layout-menu-item-reflow'))
    await settle()
    expect(box().value).toBe('被硬换行断开的句子')
    fireEvent.click(screen.getByTestId('ocr-recognize-menu'))
    fireEvent.click(await screen.findByRole('menuitemcheckbox', { name: /段落/ }))
    // 请求照样带上会话排版：Rust 按同一条规则判断，在段落模式下原样交付
    expect(callsOf('ocr_start').at(-1)?.[1]).toMatchObject({
      req: { mode: 'paragraphs', layout: 'reflow' },
    })
    const before = callsOf('text_transform').length
    await recognize('r-1', WRAPPED)
    await settle()
    expect(box().value).toBe(WRAPPED)
    expect(callsOf('text_transform')).toHaveLength(before)
    const toggle = screen.getByTestId('ocr-layout-toggle')
    expect(toggle).toHaveAttribute('aria-pressed', 'false')
    // 左半换成这个模式下可用的整理方式
    expect(toggle).toHaveAccessibleName('去掉空行')
    fireEvent.click(screen.getByTestId('ocr-layout-caret'))
    const menu = await screen.findByTestId('ocr-layout-menu')
    expect(
      within(menu)
        .getAllByRole('menuitemradio')
        .map((el) => el.getAttribute('aria-checked')),
    ).toEqual(['true', 'false', 'false'])
  })

  it('设置默认去掉空行：一开始就是按下状态，识别完成后显示排版结果（Rust 已按它复制，不再复制）；菜单是单选', async () => {
    view({}, payload({ mode: 'paragraphs', layout: 'remove_blank_lines' }))
    await recognize('r-ocr', PARAS)
    await settle()
    expect(box().value).toBe(TIGHT)
    expect(screen.getByTestId('ocr-layout-toggle')).toHaveAttribute('aria-pressed', 'true')
    expect(callsOf('ocr_recopy')).toHaveLength(0)
    fireEvent.click(screen.getByTestId('ocr-layout-caret'))
    const menu = await screen.findByTestId('ocr-layout-menu')
    const items = within(menu).getAllByRole('menuitemradio')
    expect(items.map((el) => el.getAttribute('aria-checked'))).toEqual(['false', 'true', 'false'])
    expect(items[0]).toHaveTextContent('原样保留识别出的换行与空行')
    fireEvent.click(screen.getByTestId('ocr-layout-menu-item-join_lines'))
    await settle()
    expect(box().value).toBe('第一段第二段')
    // 左半按钮改为在原样与「合并为一行」之间切换
    expect(screen.getByTestId('ocr-layout-toggle')).toHaveAccessibleName('合并为一行')
  })

  it('不适用的整理方式（按芯片检视的结果）：菜单里置灰并写明原因；左半在原样时也按不动', async () => {
    inspectResult = { entities: [], transforms: ['join_lines'] }
    view()
    await recognize('r-ocr', '第一行\n第二行')
    await settle()
    const toggle = screen.getByTestId('ocr-layout-toggle')
    expect(toggle).toHaveAttribute('aria-disabled', 'true')
    fireEvent.click(toggle)
    act(() => mod('KeyL'))
    await settle()
    expect(callsOf('text_transform')).toHaveLength(0)
    // 检视只请求了一次（排版不另外检视）
    expect(callsOf('text_inspect')).toHaveLength(1)
    fireEvent.click(screen.getByTestId('ocr-layout-caret'))
    const remove = await screen.findByTestId('ocr-layout-menu-item-remove_blank_lines')
    expect(remove).toHaveAttribute('aria-disabled', 'true')
    expect(remove).toHaveTextContent('文字里没有空行')
    fireEvent.click(screen.getByTestId('ocr-layout-menu-item-join_lines'))
    await settle()
    expect(box().value).toBe('第一行第二行')
  })

  it('有编辑时：排版按钮置灰、⌘L 不切换；菜单里「放弃修改」先确认，确认后回到识别结果', async () => {
    view()
    await recognize('r-ocr', PARAS)
    await settle()
    fireEvent.change(box(), { target: { value: '改了' } })
    expect(screen.getByTestId('ocr-layout-toggle')).toHaveAttribute('aria-disabled', 'true')
    act(() => mod('KeyL'))
    await settle()
    expect(callsOf('text_transform')).toHaveLength(0)
    fireEvent.click(screen.getByTestId('ocr-layout-caret'))
    const menu = await screen.findByTestId('ocr-layout-menu')
    for (const item of within(menu).getAllByRole('menuitemradio')) {
      expect(item).toHaveAttribute('aria-disabled', 'true')
    }
    fireEvent.click(screen.getByTestId('ocr-layout-menu-item-discard'))
    expect(screen.getByTestId('ocr-confirm')).toHaveTextContent('放弃修改会恢复为识别结果')
    expect(box().value).toBe('改了')
    fireEvent.click(screen.getByTestId('ocr-confirm-ok'))
    expect(box().value).toBe(PARAS)
    expect(screen.getByTestId('ocr-layout-toggle')).not.toHaveAttribute('aria-disabled')
    expect(screen.getByTestId('ocr-footer')).not.toHaveTextContent('已编辑')
  })

  it('结构化与直接翻译没有排版按钮', async () => {
    const md = view({}, payload({ mode: 'markdown' }))
    await recognize('r-ocr', PARAS)
    await settle()
    expect(screen.getByTestId('ocr-footer')).toBeInTheDocument()
    expect(screen.queryByTestId('ocr-layout')).toBeNull()
    md.unmount()
    view({}, payload({ mode: 'translate' }))
    await recognize('r-ocr', 'Hello\n\nWorld')
    await settle()
    expect(screen.getByTestId('ocr-footer')).toBeInTheDocument()
    expect(screen.queryByTestId('ocr-layout')).toBeNull()
  })

  it('切换排版不算「原文已修改」（收起再展开复用译文），编辑才算；重新识别时请求带上会话排版', async () => {
    view()
    await recognize('r-ocr', PARAS)
    fireEvent.click(screen.getByTestId('ocr-translate-toggle'))
    await push('r-1', start('r-1'), delta('Hi'), done())
    fireEvent.click(screen.getByTestId('ocr-layout-toggle'))
    await settle()
    expect(box().value).toBe(TIGHT)
    expect(screen.queryByTestId('ocr-retranslate')).toBeNull()
    expect(screen.getByTestId('ocr-translation-foot')).not.toHaveTextContent('原文已修改')
    fireEvent.click(screen.getByTestId('ocr-translate-toggle'))
    fireEvent.click(screen.getByTestId('ocr-translate-toggle'))
    expect(callsOf('translate_start')).toHaveLength(1)
    fireEvent.change(box(), { target: { value: '改了' } })
    expect(screen.getByTestId('ocr-retranslate')).toBeInTheDocument()
    // 重新识别（先确认会覆盖修改）：请求带上会话排版，Rust 按它自动复制
    fireEvent.pointerDown(box())
    act(() => mod('KeyR'))
    act(() => mod('KeyR'))
    expect(callsOf('ocr_start').at(-1)?.[1]).toMatchObject({
      req: { mode: 'lines', layout: 'remove_blank_lines' },
    })
  })

  it('识别被取消后排版仍可切换显示，但剪贴板不跟随（半截文字不写进剪贴板）；切换照样播报', async () => {
    view()
    await recognize('r-ocr', PARAS)
    await settle()
    act(() => mod('KeyR'))
    await push('r-1', start('r-1'), delta('第三段\n\n第四段'), error('cancelled'))
    await settle()
    fireEvent.click(screen.getByTestId('ocr-layout-toggle'))
    await settle()
    expect(box().value).toBe('第三段\n第四段')
    expect(callsOf('ocr_recopy')).toHaveLength(0)
    expect(screen.getByTestId('ocr-announce')).toHaveTextContent(/^已切换为「去掉空行」$/)
  })

  it('重新识别期间排版按钮置灰，按下态仍是会话排版（不闪成原样）', async () => {
    view({}, payload({ layout: 'remove_blank_lines' }))
    await recognize('r-ocr', PARAS)
    await settle()
    act(() => mod('KeyR'))
    await push('r-1', start('r-1'), delta('半'))
    const toggle = screen.getByTestId('ocr-layout-toggle')
    expect(toggle).toBeDisabled()
    expect(toggle).toHaveAttribute('aria-pressed', 'true')
  })

  it('固定时新排版的结果还没到：交接识别文本，固定窗口里重新求一次（不把切换前的文字当成排版结果）', async () => {
    const first = view()
    await recognize('r-ocr', PARAS)
    await settle()
    fireEvent.click(screen.getByTestId('ocr-layout-toggle'))
    await settle()
    transformHold = new Promise(() => {})
    fireEvent.click(screen.getByTestId('ocr-layout-caret'))
    fireEvent.click(await screen.findByTestId('ocr-layout-menu-item-join_lines'))
    await settle()
    // 等结果期间继续显示切换前的文字
    expect(box().value).toBe(TIGHT)
    act(() => mod('KeyP'))
    const state = (callsOf('panel_pin')[0]?.[1] as { state: PinState } | undefined)?.state
    if (!state) throw new Error('没有固定')
    expect(state).toMatchObject({
      ocr_text: PARAS,
      ocr_layers: { raw: PARAS, layout: 'join_lines', edit: null },
    })
    first.unmount()
    calls = []
    transformHold = null
    view({ host: 'pinned', sessionId: 0, pin: state }, payload({ request_id: null }))
    await settle()
    expect(callsOf('text_transform')).toEqual([
      ['text_transform', { text: PARAS, transform: 'join_lines' }],
    ])
    expect(box().value).toBe('第一段第二段')
    expect(screen.getByTestId('ocr-layout-toggle')).toHaveAttribute('aria-pressed', 'true')
  })

  it('芯片按显示的文字检视（合并为一行后连起来的片段）；可用变换仍按识别文本，正在用的方式不置灰', async () => {
    const RAW = '邮箱 a@b.\nco'
    inspectResult = (text) => ({
      entities: text.includes('a@b.co')
        ? [{ kind: 'email', value: 'a@b.co', start: 3, end: 9, sensitive: false }]
        : [],
      transforms: text.includes('\n') ? ['join_lines'] : [],
    })
    view()
    await recognize('r-ocr', RAW)
    await settle()
    expect(screen.queryByTestId('ocr-chips')).toBeNull()
    fireEvent.click(screen.getByTestId('ocr-layout-caret'))
    fireEvent.click(await screen.findByTestId('ocr-layout-menu-item-join_lines'))
    await settle()
    expect(box().value).toBe('邮箱 a@b.co')
    await waitFor(() => expect(screen.getByTestId('ocr-chips')).toHaveTextContent('a@b.co'))
    // 切换排版立即检视（不等编辑的防抖），识别文本只检视了一次
    expect(callsOf('text_inspect').map(([, a]) => a.text)).toEqual([RAW, '邮箱 a@b.co'])
    fireEvent.click(screen.getByTestId('ocr-layout-caret'))
    expect(await screen.findByTestId('ocr-layout-menu-item-join_lines')).not.toHaveAttribute(
      'aria-disabled',
    )
  })

  it('⌘L 在原样与最近用过的整理方式之间切换', async () => {
    view()
    await recognize('r-ocr', PARAS)
    await settle()
    act(() => mod('KeyL'))
    await settle()
    expect(box().value).toBe(TIGHT)
    act(() => mod('KeyL'))
    await settle()
    expect(box().value).toBe(PARAS)
  })

  it('固定：PinState 带上三层；固定窗口里还原为同样的显示，可以继续切换但不再带动剪贴板', async () => {
    const first = view()
    await recognize('r-ocr', PARAS)
    await settle()
    fireEvent.click(screen.getByTestId('ocr-layout-toggle'))
    await settle()
    act(() => mod('KeyP'))
    const state = (callsOf('panel_pin')[0]?.[1] as { state: PinState } | undefined)?.state
    if (!state) throw new Error('没有固定')
    expect(state).toMatchObject({
      ocr_text: TIGHT,
      ocr_layers: { raw: PARAS, layout: 'remove_blank_lines', edit: null },
    })
    first.unmount()
    calls = []
    view(
      { host: 'pinned', sessionId: 0, pin: state },
      payload({ request_id: null, layout: 'remove_blank_lines' }),
    )
    await settle()
    expect(box().value).toBe(TIGHT)
    expect(callsOf('text_transform')).toHaveLength(0)
    expect(screen.getByTestId('ocr-layout-toggle')).toHaveAttribute('aria-pressed', 'true')
    fireEvent.click(screen.getByTestId('ocr-layout-toggle'))
    await settle()
    expect(box().value).toBe(PARAS)
    expect(callsOf('ocr_recopy')).toHaveLength(0)
  })
})

describe('翻译联动', () => {
  it('[翻译] 展开译文区，以编辑后的文本为原文；⌘2 切换风格即重新翻译', async () => {
    view()
    await recognize()
    fireEvent.change(screen.getByTestId('ocr-text'), { target: { value: '编辑后' } })
    fireEvent.click(screen.getByTestId('ocr-translate-toggle'))
    expect(callsOf('translate_start').at(-1)?.[1]).toMatchObject({
      req: {
        session_id: 3,
        text: '编辑后',
        task: 'translate',
        style_id: 'natural',
        dict_mode: 'auto',
      },
    })
    expect(screen.getByTestId('ocr-translation')).toBeInTheDocument()
    await push('r-1', start('r-1'), delta('Edited'), done())
    expect(screen.getByTestId('ocr-translation-body')).toHaveTextContent('Edited')
    act(() => mod('Digit2'))
    expect(callsOf('translate_start').at(-1)?.[1]).toMatchObject({ req: { style_id: 'technical' } })
    expect(callsOf('translate_start')).toHaveLength(2)
  })

  it('译文区收起时 ⌘2：展开译文区并按该风格翻译（与菜单里标的快捷键一致）', async () => {
    view()
    await recognize()
    expect(screen.queryByTestId('ocr-translation')).toBeNull()
    act(() => mod('Digit2'))
    expect(screen.getByTestId('ocr-translation')).toBeInTheDocument()
    expect(callsOf('translate_start').at(-1)?.[1]).toMatchObject({ req: { style_id: 'technical' } })
  })

  it('改目标语言即重新翻译；状态行从展开起就占位（翻译中… → 结果）', async () => {
    view()
    await recognize()
    fireEvent.click(screen.getByTestId('ocr-translate-toggle'))
    expect(screen.getByTestId('ocr-translation-foot')).toHaveTextContent('翻译中…')
    await push('r-1', start('r-1'), delta('Hi'), done())
    expect(
      within(screen.getByTestId('ocr-translation-foot')).getByTestId('status-line'),
    ).toBeInTheDocument()
    fireEvent.click(within(screen.getByTestId('ocr-translation')).getByTestId('lang-bar-target'))
    fireEvent.click(await screen.findByTestId('lang-bar-menu-item-en'))
    expect(callsOf('translate_start')).toHaveLength(2)
    expect(callsOf('translate_start').at(-1)?.[1]).toMatchObject({ req: { target_lang: 'en' } })
    // 新结果流式期间区底仍占位
    expect(screen.getByTestId('ocr-translation-foot')).toHaveTextContent('翻译中…')
  })

  it('重新识别期间改目标语言：不拿半截识别结果发翻译；识别完成后按新语言重新翻译', async () => {
    view()
    await recognize()
    fireEvent.click(screen.getByTestId('ocr-translate-toggle'))
    await push('r-1', start('r-1'), delta('Hi'), done())
    expect(callsOf('translate_start')).toHaveLength(1)
    fireEvent.click(screen.getByTestId('ocr-recognize-again'))
    // 重新识别的流还在进行（只到了半截）
    await push('r-2', start('r-2'), delta('第一'))
    fireEvent.click(within(screen.getByTestId('ocr-translation')).getByTestId('lang-bar-target'))
    fireEvent.click(await screen.findByTestId('lang-bar-menu-item-en'))
    expect(callsOf('translate_start')).toHaveLength(1)
    // 语言条立即显示新目标
    expect(
      within(screen.getByTestId('ocr-translation')).getByTestId('lang-bar-target'),
    ).toHaveTextContent(/英语/)
    await push('r-2', delta('行'), done())
    await settle()
    expect(callsOf('translate_start')).toHaveLength(2)
    expect(callsOf('translate_start').at(-1)?.[1]).toMatchObject({
      req: { text: '第一行', target_lang: 'en' },
    })
  })

  it('长 OCR 文本：翻译先确认（StartAck.long_text），「继续」后才请求；同一段文字换风格不再确认', async () => {
    view()
    const long = '字'.repeat(3200)
    await recognize('r-ocr', long)
    fireEvent.click(screen.getByTestId('ocr-translate-toggle'))
    await settle()
    expect(callsOf('translate_start')).toHaveLength(1)
    expect(callsOf('translate_start')[0]?.[1]).toMatchObject({ req: { confirm_long: false } })
    const card = await screen.findByTestId('tv-long-text')
    expect(card).toHaveTextContent('3,200')
    fireEvent.click(within(card).getByRole('button', { name: /继续/ }))
    await settle()
    expect(callsOf('translate_start')).toHaveLength(2)
    expect(callsOf('translate_start')[1]?.[1]).toMatchObject({ req: { confirm_long: true } })
    expect(screen.queryByTestId('tv-long-text')).toBeNull()
    await push('r-1', start('r-1'), delta('译文'), done())
    act(() => mod('Digit3'))
    await settle()
    expect(callsOf('translate_start')).toHaveLength(3)
    expect(callsOf('translate_start')[2]?.[1]).toMatchObject({ req: { confirm_long: true } })
  })

  it('长 OCR 文本确认过之后切换排版：换风格仍不再确认（排版只改换行）；编辑之后要重新确认', async () => {
    view()
    const half = '字'.repeat(1600)
    await recognize('r-ocr', `${half}\n\n${half}`)
    await settle()
    fireEvent.click(screen.getByTestId('ocr-translate-toggle'))
    await settle()
    const card = await screen.findByTestId('tv-long-text')
    fireEvent.click(within(card).getByRole('button', { name: /继续/ }))
    await settle()
    await push('r-1', start('r-1'), delta('译文'), done())
    fireEvent.click(screen.getByTestId('ocr-layout-toggle'))
    await settle()
    const text = screen.getByTestId('ocr-text') as HTMLTextAreaElement
    expect(text.value).toBe(`${half}\n${half}`)
    act(() => mod('Digit2'))
    await settle()
    expect(callsOf('translate_start').at(-1)?.[1]).toMatchObject({
      req: { text: `${half}\n${half}`, confirm_long: true },
    })
    fireEvent.change(text, { target: { value: `${half}改` } })
    act(() => mod('Digit3'))
    await settle()
    expect(callsOf('translate_start').at(-1)?.[1]).toMatchObject({ req: { confirm_long: false } })
  })

  it('长文本确认卡片等确认时，⌘↩ 是卡片上的「继续」，不粘贴', async () => {
    view()
    await recognize('r-ocr', '字'.repeat(3200))
    fireEvent.click(screen.getByTestId('ocr-translate-toggle'))
    await screen.findByTestId('tv-long-text')
    act(() => mod('Enter'))
    await settle()
    expect(callsOf('translate_start').at(-1)?.[1]).toMatchObject({ req: { confirm_long: true } })
    expect(callsOf('clipboard_paste_text')).toHaveLength(0)
  })

  it('译文 Done{truncated: max_tokens} 且没有文字：区底状态行提示思考耗尽', async () => {
    view()
    await recognize()
    fireEvent.click(screen.getByTestId('ocr-translate-toggle'))
    await push('r-1', start('r-1'), delta(' '), done({ truncated: 'max_tokens' }))
    const foot = screen.getByTestId('ocr-translation-foot')
    expect(foot).toHaveTextContent('思考耗尽了输出额度')
    expect(within(foot).getByTitle('模型思考耗尽了输出额度')).toBeInTheDocument()
  })

  it('译文有文字但被截断：区底提示「已截断」；参数降级时提示', async () => {
    view()
    await recognize()
    fireEvent.click(screen.getByTestId('ocr-translate-toggle'))
    await push('r-1', start('r-1'), delta('Partial'), done({ truncated: 'max_tokens' }))
    expect(screen.getByTestId('ocr-translation-foot')).toHaveTextContent('已截断')
    act(() => mod('Digit3'))
    await push('r-2', start('r-2'), { type: 'notice', kind: 'param_degraded' }, delta('A'), done())
    expect(screen.getByTestId('ocr-translation-foot')).not.toHaveTextContent('已截断')
    expect(screen.getByTestId('ocr-translation-foot').textContent).not.toBe('')
  })

  it('收起后再展开：原文与风格没变时复用结果，不重新请求', async () => {
    view()
    await recognize()
    fireEvent.click(screen.getByTestId('ocr-translate-toggle'))
    await push('r-1', start('r-1'), delta('Hi'), done())
    fireEvent.click(screen.getByTestId('ocr-translate-toggle'))
    fireEvent.click(screen.getByTestId('ocr-translate-toggle'))
    expect(callsOf('translate_start')).toHaveLength(1)
  })

  it('原文改过之后提示「原文已修改」，可以重新翻译', async () => {
    view()
    await recognize()
    fireEvent.click(screen.getByTestId('ocr-translate-toggle'))
    await push('r-1', start('r-1'), delta('Hi'), done())
    fireEvent.change(screen.getByTestId('ocr-text'), { target: { value: '新的原文' } })
    fireEvent.click(screen.getByTestId('ocr-retranslate'))
    expect(callsOf('translate_start').at(-1)?.[1]).toMatchObject({ req: { text: '新的原文' } })
  })
})

describe('键盘', () => {
  it('⌘C：没有选区时复制 OCR 文本；译文区展开后复制译文', async () => {
    view()
    await recognize()
    await settle()
    calls = []
    act(() => mod('KeyC'))
    await settle()
    expect(callsOf('clipboard_copy_text')).toEqual([
      ['clipboard_copy_text', { text: '第一行\n第二行' }],
    ])
    fireEvent.click(screen.getByTestId('ocr-translate-toggle'))
    await push('r-1', start('r-1'), delta('Line'), done())
    act(() => mod('KeyC'))
    await settle()
    expect(callsOf('clipboard_copy_text').at(-1)?.[1]).toEqual({ text: 'Line' })
  })

  it('⌘C：OCR 文本里有选区时复制选区', async () => {
    view()
    await recognize()
    const el = screen.getByTestId('ocr-text') as HTMLTextAreaElement
    el.focus()
    el.setSelectionRange(0, 2)
    calls = []
    act(() => mod('KeyC'))
    await settle()
    expect(callsOf('clipboard_copy_text')).toEqual([['clipboard_copy_text', { text: '第一' }]])
  })

  describe('⌘↩ 把当前的文字粘贴回原应用', () => {
    const pasted = () => callsOf('clipboard_paste_text').map(([, a]) => a)

    it('与 ⌘C 同一优先级：没有选区时是正文显示的文字，译文区展开且有内容时是译文，有选区时是选区；片段粘贴、不进历史', async () => {
      view()
      await recognize()
      await settle()
      act(() => mod('Enter'))
      expect(pasted()).toEqual([{ text: '第一行\n第二行', from: null, mode: 'normal' }])
      fireEvent.click(screen.getByTestId('ocr-translate-toggle'))
      await push('r-1', start('r-1'), delta('Line'), done())
      act(() => mod('Enter'))
      expect(pasted().at(-1)).toEqual({ text: 'Line', from: null, mode: 'normal' })
      const el = screen.getByTestId('ocr-text') as HTMLTextAreaElement
      el.focus()
      el.setSelectionRange(0, 2)
      act(() => mod('Enter'))
      expect(pasted().at(-1)).toEqual({ text: '第一', from: null, mode: 'normal' })
      // 粘贴成功：不复制、不提示
      await settle()
      expect(callsOf('clipboard_copy_text')).toHaveLength(0)
    })

    it('在文本框里按不插入换行；菜单打开时不触发；还在识别时不粘贴半截文字', async () => {
      view()
      await push('r-ocr', start('r-ocr'), delta('半截'))
      act(() => mod('Enter'))
      expect(pasted()).toHaveLength(0)
      await push('r-ocr', delta('文字'), done())
      await settle()
      const el = screen.getByTestId('ocr-text') as HTMLTextAreaElement
      el.focus()
      let typed = true
      act(() => {
        typed = fireEvent.keyDown(el, {
          code: 'Enter',
          key: 'Enter',
          ...(boot.os === 'macos' ? { metaKey: true } : { ctrlKey: true }),
        })
      })
      // 键盘路由处理并 preventDefault：文本框不会插入换行
      expect(typed).toBe(false)
      expect(el.value).toBe('半截文字')
      expect(pasted()).toEqual([{ text: '半截文字', from: null, mode: 'normal' }])
      fireEvent.click(screen.getByTestId('ocr-recognize-menu'))
      const list = await screen.findByTestId('ocr-recognize-list')
      act(() => mod('Enter'))
      expect(pasted()).toHaveLength(1)
      // 焦点在菜单里（高亮着当前模式）：菜单也不把 ⌘↩ 当作 Enter 选中高亮项，不会重新识别
      const starts = callsOf('ocr_start').length
      fireEvent.keyDown(list, {
        code: 'Enter',
        key: 'Enter',
        ...(boot.os === 'macos' ? { metaKey: true } : { ctrlKey: true }),
      })
      await settle()
      expect(callsOf('ocr_start')).toHaveLength(starts)
      expect(pasted()).toHaveLength(1)
      expect(screen.getByTestId('ocr-recognize-list')).toBeInTheDocument()
    })

    it('粘贴 conflict（面板已不在显示这个视图）静默；其他失败退回复制并提示', async () => {
      view()
      await recognize()
      await settle()
      pasteError = 'conflict'
      act(() => mod('Enter'))
      await settle()
      expect(callsOf('clipboard_copy_text')).toHaveLength(0)
      pasteError = 'invalid_argument'
      act(() => mod('Enter'))
      await settle()
      expect(callsOf('clipboard_copy_text')).toEqual([
        ['clipboard_copy_text', { text: '第一行\n第二行' }],
      ])
      expect(screen.getByTestId('inline-toast')).toHaveTextContent('已复制')
    })

    it('固定窗口里不能粘贴：退回复制并提示', async () => {
      const pin: PinState = {
        kind: 'ocr',
        source_text: '第一行',
        result_text: '',
        task: 'translate',
        style_id: 'natural',
        target_lang: 'zh-CN',
        detected_lang: null,
        request_id: 'r-done',
        capture_id: 'cap-1',
        ocr_text: '第一行',
        ocr_mode: 'lines',
        show_source: false,
        status_line: 'vl-plus · 1.2 s',
        status: {
          model_label: 'vl-plus',
          elapsed_ms: 1200,
          usage: null,
          cached: false,
          cancelled: false,
          truncated: null,
        },
        model_ref: { provider_id: 'dashscope', model_id: 'qwen-vl-plus' },
      }
      view({ host: 'pinned', sessionId: 0, pin }, payload({ request_id: null }))
      await settle()
      act(() => mod('Enter'))
      await settle()
      expect(pasted()).toHaveLength(0)
      expect(callsOf('clipboard_copy_text')).toEqual([['clipboard_copy_text', { text: '第一行' }]])
      expect(screen.getByTestId('inline-toast')).toHaveTextContent('已复制')
    })

    it('复制按钮的 tooltip 写着 ⌘↩ 粘贴到原应用（面板里）', async () => {
      view()
      await recognize()
      await settle()
      vi.useFakeTimers()
      try {
        fireEvent.pointerEnter(screen.getByTestId('ocr-copy').parentElement as HTMLElement, {
          pointerType: 'mouse',
        })
        act(() => vi.advanceTimersByTime(1000))
      } finally {
        vi.useRealTimers()
      }
      const tip = await screen.findByRole('tooltip')
      expect(tip).toHaveTextContent('复制识别文字')
      expect(tip).toHaveTextContent('粘贴到原应用')
      expect(tip).toHaveTextContent('⌘↩')
    })
  })

  it('⌘R 作用于最近一次交互的区域：译文区 → 重新翻译（绕过缓存）', async () => {
    view()
    await recognize()
    fireEvent.click(screen.getByTestId('ocr-translate-toggle'))
    await push('r-1', start('r-1'), delta('A'), done())
    fireEvent.pointerDown(screen.getByTestId('ocr-translation-body'))
    act(() => mod('KeyR'))
    expect(callsOf('translate_start').at(-1)?.[1]).toMatchObject({ req: { bypass_cache: true } })
    fireEvent.pointerDown(screen.getByTestId('ocr-text'))
    act(() => mod('KeyR'))
    expect(callsOf('ocr_start')).toHaveLength(1)
  })

  it('⌘P：译文区展开但译文请求还没有 id（被拒绝）时，固定窗口不会把识别请求接到译文区', async () => {
    rejectTranslate = true
    view()
    await recognize()
    fireEvent.click(screen.getByTestId('ocr-translate-toggle'))
    await settle()
    act(() => mod('KeyP'))
    const call = callsOf('panel_pin')[0]
    expect((call?.[1] as { state: PinState } | undefined)?.state).toMatchObject({
      request_id: 'r-ocr',
      show_source: false,
      result_text: '',
    })
  })

  it('⌘P 固定整个面板：PinState 带编辑后的 OCR 文本与进行中的译文流', async () => {
    view()
    await recognize()
    fireEvent.change(screen.getByTestId('ocr-text'), { target: { value: '改' } })
    fireEvent.click(screen.getByTestId('ocr-translate-toggle'))
    await push('r-1', start('r-1'), delta('Ch'))
    act(() => mod('KeyP'))
    const state = callsOf('panel_pin')[0]?.[1] as {
      session_id: number
      state: PinState
      expand: boolean
    }
    expect(state.session_id).toBe(3)
    expect(state.expand).toBe(false)
    expect(state.state).toMatchObject({
      kind: 'ocr',
      ocr_text: '改',
      source_text: '改',
      request_id: 'r-1',
      capture_id: 'cap-1',
      ocr_mode: 'lines',
      show_source: true,
    })
  })
})

describe('直接翻译', () => {
  it('正文是译文；「查看原文」按需转录一次并缓存', async () => {
    view({}, payload({ mode: 'translate' }))
    await push('r-ocr', start('r-ocr'), delta('Budget'), done())
    expect(screen.getByTestId('ocr-direct-head')).toBeInTheDocument()
    expect(screen.queryByTestId('ocr-text')).toBeNull()
    fireEvent.click(screen.getByTestId('ocr-show-source'))
    expect(callsOf('ocr_start')).toEqual([
      [
        'ocr_start',
        {
          req: {
            session_id: 3,
            capture_id: 'cap-1',
            mode: 'paragraphs',
            model_ref: null,
            target_lang: null,
          },
        },
      ],
    ])
    await push('r-1', start('r-1'), delta('预算'), done())
    expect(screen.getByTestId('ocr-source-body')).toHaveTextContent('预算')
    fireEvent.click(screen.getByTestId('ocr-show-source'))
    fireEvent.click(screen.getByTestId('ocr-show-source'))
    expect(callsOf('ocr_start')).toHaveLength(1)
  })

  it('直接翻译 + 选过风格：转录 no_vision 时显示 no_vision 卡片（带 [选择模型]），不是通用的内部错误', async () => {
    view({}, payload({ mode: 'translate' }))
    await push('r-ocr', start('r-ocr'), delta('Budget'), done())
    act(() => mod('Digit2'))
    // 先按需转录原文（paragraphs），转录失败
    expect(callsOf('ocr_start').at(-1)?.[1]).toMatchObject({ req: { mode: 'paragraphs' } })
    await push('r-1', start('r-1'), error('no_vision'))
    await settle()
    expect(callsOf('translate_start')).toHaveLength(0)
    expect(screen.getByRole('button', { name: '选择模型' })).toBeInTheDocument()
    expect(screen.queryByText(/\[object Object\]/)).toBeNull()
  })

  it('直接翻译 + 选过风格：转录没有文字时显示「未检测到文字」', async () => {
    view({}, payload({ mode: 'translate' }))
    await push('r-ocr', start('r-ocr'), delta('Budget'), done())
    act(() => mod('Digit2'))
    await push('r-1', start('r-1'), done({ no_text: true }))
    await settle()
    expect(screen.getByTestId('ocr-empty')).toHaveTextContent('未检测到文字')
    expect(callsOf('translate_start')).toHaveLength(0)
  })

  it('直接翻译 + 选过风格：换视觉模型后用新模型重新转录（不复用旧转录），再翻译', async () => {
    view({}, payload({ mode: 'translate' }))
    await push('r-ocr', start('r-ocr'), delta('Budget'), done())
    act(() => mod('Digit2'))
    await push('r-1', start('r-1'), delta('预算'), done())
    await settle()
    expect(callsOf('translate_start')).toHaveLength(1)
    // 「重新生成 ▾」→ 视觉模型 → llava
    fireEvent.click(screen.getByTestId('ocr-recognize-menu'))
    fireEvent.click(await screen.findByRole('menuitem', { name: /识别模型/ }))
    fireEvent.click(within(await screen.findByTestId('ocr-model-menu')).getByText('llava'))
    expect(callsOf('ocr_start').at(-1)?.[1]).toMatchObject({
      req: { mode: 'paragraphs', model_ref: { provider_id: 'ollama', model_id: 'llava' } },
    })
    const id = `r-${seq}`
    await push(id, start(id), delta('新转录'), done())
    await settle()
    expect(callsOf('translate_start').at(-1)?.[1]).toMatchObject({ req: { text: '新转录' } })
  })

  it('直接翻译：「原文」收起后换视觉模型，仍在进行的旧转录被取消（不为没人用的结果付费）', async () => {
    view({}, payload({ mode: 'translate' }))
    await push('r-ocr', start('r-ocr'), delta('Budget'), done())
    fireEvent.click(screen.getByTestId('ocr-show-source'))
    const id = `r-${seq}`
    await push(id, start(id))
    // 转录还在进行时收起「原文」
    fireEvent.click(screen.getByTestId('ocr-show-source'))
    fireEvent.click(screen.getByTestId('ocr-recognize-menu'))
    fireEvent.click(await screen.findByRole('menuitem', { name: /识别模型/ }))
    fireEvent.click(within(await screen.findByTestId('ocr-model-menu')).getByText('llava'))
    await settle()
    expect(callsOf('cancel').map(([, a]) => a.request_id)).toContain(id)
  })

  it('直接翻译：等转录期间被新的请求取代，旧的不再发出 translate_start', async () => {
    view({}, payload({ mode: 'translate' }))
    await push('r-ocr', start('r-ocr'), delta('Budget'), done())
    act(() => mod('Digit2'))
    act(() => mod('Digit3'))
    await push('r-1', start('r-1'), delta('预算'), done())
    await settle()
    expect(callsOf('translate_start')).toHaveLength(1)
    expect(callsOf('translate_start')[0]?.[1]).toMatchObject({ req: { style_id: 'academic' } })
  })

  it('直接翻译不自动复制', async () => {
    view({}, payload({ mode: 'translate' }))
    await push('r-ocr', start('r-ocr'), delta('Budget'), done())
    await settle()
    expect(callsOf('clipboard_copy_text')).toHaveLength(0)
  })
})

/** 固定窗口宿主（PinnedApp 的 PinnedContext）：识别还在进行时固定（ocr_text = null，接入 r-pin） */
function pinnedHost(ready: () => void): PinnedHost {
  return {
    pinnedId: 'p-1',
    state: {
      kind: 'ocr',
      source_text: '',
      result_text: '',
      task: 'translate',
      style_id: 'natural',
      target_lang: 'zh-CN',
      detected_lang: null,
      request_id: 'r-pin',
      capture_id: 'cap-9',
      ocr_text: null,
      ocr_mode: 'lines',
      show_source: false,
      status_line: null,
    },
    ready,
    holdReady: vi.fn<() => void>(),
    claimChrome: vi.fn<() => void>(),
  }
}

describe('未就绪与固定窗口', () => {
  it('缺少屏幕录制权限：卡片 + [前往系统设置]，说明合并为「授权后重启、再按 ⌥S」（没有重新截图：要先重启）', async () => {
    view(
      {},
      payload({
        request_id: null,
        thumb_url: '',
        image_size: { width: 0, height: 0 },
        not_ready: { reason: 'permission_missing', permission: 'screen_recording' },
      }),
    )
    expect(callsOf('stream_attach')).toHaveLength(0)
    expect(callsOf('ocr_start')).toHaveLength(0)
    fireEvent.click(screen.getByRole('button', { name: '前往系统设置' }))
    expect(callsOf('permissions_open')).toEqual([
      ['permissions_open', { kind: 'screen_recording' }],
    ])
    const detail = screen.getByTestId('not-ready-card').querySelector('.ui-notready-detail')
    expect(detail).toHaveTextContent('⌥S')
    expect(detail?.querySelector('.ocr-hint-kbd')).not.toBeNull()
    expect(screen.queryByTestId('not-ready-card-retry')).toBeNull()
  })

  it('截图失败：说明补上原因，卡片带 [重新截图 ⌥S]，点按以 origin = panel 触发截图', async () => {
    view(
      {},
      payload({
        request_id: null,
        thumb_url: '',
        image_size: { width: 0, height: 0 },
        not_ready: { reason: 'capture_failed', message: 'errors.capture.backend' },
      }),
    )
    expect(screen.getByText('截图失败')).toBeInTheDocument()
    expect(screen.getByText('可能被其他截图工具占用，或显示器刚刚变化。')).toBeInTheDocument()
    const retry = screen.getByTestId('not-ready-card-retry')
    expect(retry).toHaveTextContent('重新截图')
    expect(retry).toHaveTextContent('⌥S')
    fireEvent.click(retry)
    await settle()
    expect(callsOf('trigger_action')).toEqual([
      ['trigger_action', { action: 'ocr', text: null, origin: 'panel' }],
    ])
  })

  it('截图失败（当前桌面不支持）：保留卡片的说明，不提示重新截图', async () => {
    view(
      {},
      payload({
        request_id: null,
        thumb_url: '',
        image_size: { width: 0, height: 0 },
        not_ready: { reason: 'capture_failed', message: 'errors.capture.unsupported' },
      }),
    )
    expect(screen.getByText('当前桌面环境不支持截图。')).toBeInTheDocument()
    expect(screen.queryByTestId('not-ready-card-retry')).toBeNull()
  })

  it('截图失败（Wayland 会话）：说明请切换到 X11 会话，不提示重新截图（重试只会落回同一张卡片）', async () => {
    view(
      {},
      payload({
        request_id: null,
        thumb_url: '',
        image_size: { width: 0, height: 0 },
        not_ready: { reason: 'capture_failed', message: 'errors.platform.wayland_capture' },
      }),
    )
    expect(
      screen.getByText('这一版在 Wayland 会话下无法截图。需要截图时，请切换到 X11 会话'),
    ).toBeInTheDocument()
    expect(screen.queryByTestId('not-ready-card-retry')).toBeNull()
  })

  it('头部 ⋯：重新截图（origin = panel）· 识别历史…（设置深链锚点 ocr_history）· OCR 设置…；固定窗口没有 ⋯', async () => {
    const { props } = view()
    await recognize()
    fireEvent.click(screen.getByTestId('ocr-more'))
    expect(await screen.findByTestId('ocr-more-menu')).toBeInTheDocument()
    expect(screen.getByTestId('ocr-more-menu-item-retake')).toHaveTextContent('⌥S')
    fireEvent.click(screen.getByTestId('ocr-more-menu-item-history'))
    await settle()
    expect(callsOf('window_open').at(-1)).toEqual([
      'window_open',
      { target: { kind: 'settings', section: 'ocr', anchor: 'ocr_history' } },
    ])
    expect(props.requestHide).toHaveBeenCalledWith('other')
    fireEvent.click(screen.getByTestId('ocr-more'))
    fireEvent.click(await screen.findByTestId('ocr-more-menu-item-retake'))
    await settle()
    expect(callsOf('trigger_action').at(-1)).toEqual([
      'trigger_action',
      { action: 'ocr', text: null, origin: 'panel' },
    ])
  })

  it('放大的截图是浮层；Esc 先收起它', async () => {
    view()
    await recognize()
    fireEvent.click(screen.getByTestId('ocr-thumbnail'))
    expect(await screen.findByTestId('ocr-zoom')).toBeInTheDocument()
    act(() => key('Escape'))
    await settle()
    await vi.waitFor(() => expect(screen.queryByTestId('ocr-zoom')).toBeNull())
  })

  it('固定窗口：从 PinState 恢复（编辑后的文本与译文），没有关闭 / 固定按钮，不提示已复制', async () => {
    const pin: PinState = {
      kind: 'ocr',
      source_text: '改过',
      result_text: 'Edited',
      task: 'translate',
      style_id: 'technical',
      target_lang: 'en',
      detected_lang: 'zh-CN',
      request_id: null,
      capture_id: 'cap-1',
      ocr_text: '改过',
      ocr_mode: 'paragraphs',
      show_source: true,
      status_line: null,
    }
    view({ host: 'pinned', sessionId: 0, pin }, payload({ request_id: null }))
    await settle()
    expect((screen.getByTestId('ocr-text') as HTMLTextAreaElement).value).toBe('改过')
    expect(screen.getByTestId('ocr-translation-body')).toHaveTextContent('Edited')
    expect(screen.queryByTestId('ocr-close')).toBeNull()
    expect(screen.queryByTestId('ocr-pin')).toBeNull()
    expect(callsOf('stream_attach')).toHaveLength(0)
    expect(screen.queryByTestId('inline-toast')).toBeNull()
  })

  it('PinnedApp 宿主：读 usePinnedHost，回放完才 ready；缩略图来自交接的 thumb_url', async () => {
    const ready = vi.fn<() => void>()
    const pinned = pinnedHost(ready)
    const props: OcrViewProps = {
      sessionId: 0,
      payload: payload({
        request_id: 'r-pin',
        capture_id: 'cap-9',
        thumb_url: 'crop/cap-9.png',
        image_size: { width: 800, height: 240 },
      }),
      phase: 'shown',
      focused: true,
      host: 'pinned',
      frame: 'native',
      placement: 'below',
      maxHeight: 520,
      requestHide: vi.fn<(reason: string) => void>(),
      switchView: vi.fn<OcrViewProps['switchView']>(),
      activate: vi.fn<() => void>(),
    }
    render(
      <MotionRoot>
        <PinnedContext.Provider value={pinned}>
          <OcrView {...props} />
        </PinnedContext.Provider>
      </MotionRoot>,
    )
    expect(pinned.holdReady).toHaveBeenCalled()
    expect(screen.queryByTestId('ocr-close')).toBeNull()
    expect(screen.getByTestId('ocr-thumbnail').querySelector('img')?.getAttribute('src')).toBe(
      'crop/cap-9.png',
    )
    await settle()
    expect(callsOf('stream_attach')).toEqual([['stream_attach', { request_id: 'r-pin' }]])
    expect(ready).toHaveBeenCalledTimes(1)
    await push('r-pin', start('r-pin'), delta('固定后'), done())
    expect((screen.getByTestId('ocr-text') as HTMLTextAreaElement).value).toBe('固定后')
  })

  it('固定窗口（识别已结束）：文字用 ocr_text，模型 / 耗时 / 用量取自结构化状态行（不再接入回放），与面板一致', async () => {
    const pin: PinState = {
      kind: 'ocr',
      source_text: '改过的',
      result_text: '',
      task: 'translate',
      style_id: 'natural',
      target_lang: 'zh-CN',
      detected_lang: null,
      request_id: 'r-done',
      capture_id: 'cap-1',
      ocr_text: '改过的',
      ocr_mode: 'lines',
      show_source: false,
      status_line: 'vl-plus · 1.2 s',
      status: {
        model_label: 'vl-plus',
        elapsed_ms: 1200,
        usage: null,
        cached: false,
        cancelled: false,
        truncated: null,
      },
      model_ref: { provider_id: 'dashscope', model_id: 'qwen-vl-plus' },
    }
    view({ host: 'pinned', sessionId: 0, pin }, payload({ request_id: null }))
    await settle()
    expect(callsOf('stream_attach')).toEqual([])
    expect((screen.getByTestId('ocr-text') as HTMLTextAreaElement).value).toBe('改过的')
    expect(screen.getByTestId('ocr-meta-status')).toHaveTextContent('纯文本 · vl-plus')
    expect(within(screen.getByTestId('ocr-footer')).getByTestId('status-line')).toHaveTextContent(
      /vl-plus.*1\.2 s/,
    )
  })

  it('固定窗口的结构化状态行：命中缓存不显示耗时、注明来自缓存；已取消只显示「已取消」', async () => {
    const pin: PinState = {
      kind: 'ocr',
      source_text: '文字',
      result_text: '',
      task: 'translate',
      style_id: 'natural',
      target_lang: 'zh-CN',
      detected_lang: null,
      request_id: 'r-done',
      capture_id: 'cap-1',
      ocr_text: '文字',
      ocr_mode: 'lines',
      show_source: false,
      status_line: 'qwen3-vl-plus',
      status: {
        model_label: 'qwen3-vl-plus',
        elapsed_ms: null,
        usage: null,
        cached: true,
        cancelled: false,
        truncated: null,
      },
    }
    const first = view({ host: 'pinned', sessionId: 0, pin }, payload({ request_id: null }))
    await settle()
    const line = within(screen.getByTestId('ocr-footer')).getByTestId('status-line')
    expect(line).toHaveTextContent('qwen3-vl-plus')
    expect(line).toHaveTextContent('来自缓存')
    expect(line).not.toHaveTextContent(/\d+ ms|\d\.\d s/)
    first.unmount()
    view(
      {
        host: 'pinned',
        sessionId: 0,
        pin: { ...pin, status: { ...pin.status!, cached: false, cancelled: true } },
      },
      payload({ request_id: null }),
    )
    await settle()
    expect(within(screen.getByTestId('ocr-footer')).getByTestId('status-line')).toHaveTextContent(
      '已取消',
    )
  })

  it('固定窗口接入失败（日志已释放）时退回 PinState 里的文字', async () => {
    gone.add('r-old')
    const pin: PinState = {
      kind: 'ocr',
      source_text: '',
      result_text: 'Cached translation',
      task: 'translate',
      style_id: 'natural',
      target_lang: 'en',
      detected_lang: 'zh-CN',
      request_id: 'r-old',
      capture_id: 'cap-1',
      ocr_text: '原文',
      ocr_mode: 'lines',
      show_source: true,
      status_line: null,
    }
    view({ host: 'pinned', sessionId: 0, pin }, payload({ request_id: null }))
    await settle()
    expect(screen.getByTestId('ocr-translation-body')).toHaveTextContent('Cached translation')
  })
})

describe('展开区（Section）在 StrictMode 下（命令式 controls.start 会停在 opacity 0）', () => {
  it('展开（淡入 / 按高度展开）、收起再展开之后都走到 opacity 1', async () => {
    const { Section } = await import('../parts')
    const { StrictMode } = await import('react')
    const tree = (open: boolean, expand: boolean) => (
      <StrictMode>
        <MotionRoot>
          <Section open={open} expand={expand} collapse={expand} label="译文" data-testid="sec">
            <p>内容</p>
          </Section>
        </MotionRoot>
      </StrictMode>
    )
    const opacity = () => Number(screen.getByTestId('sec').style.opacity || '1')
    const cycle = async (expand: boolean) => {
      const r = render(tree(false, expand))
      r.rerender(tree(true, expand))
      await waitFor(() => expect(opacity()).toBe(1), { timeout: 3000 })
      r.rerender(tree(false, expand))
      await waitFor(() => expect(screen.queryByTestId('sec')).toBeNull(), { timeout: 3000 })
      r.rerender(tree(true, expand))
      await waitFor(() => expect(opacity()).toBe(1), { timeout: 3000 })
      r.unmount()
    }
    // 淡入（卡片还没到上限）与按高度展开（已到上限）两种进入
    await cycle(false)
    await cycle(true)
  })
})

// ---------------------------------------------------------------------------
describe('拖动', () => {
  const down = (el: Element) => fireEvent.pointerDown(el, { button: 0, pointerType: 'mouse' })

  it('面板宿主：从头部、底栏与空白处拖动（同时成为 key）；识别结果文本框、按钮、缩略图不触发', async () => {
    const { props } = view()
    await recognize()
    const head = screen.getByTestId('ocr-head')
    down(head)
    expect(callsOf('panel_drag_start')).toEqual([['panel_drag_start', { session_id: 3 }]])
    expect(props.activate).toHaveBeenCalled()
    expect(fireEvent.mouseDown(head, { button: 0 })).toBe(false)
    down(screen.getByTestId('ocr-footer'))
    down(screen.getByTestId('view-ocr'))
    expect(callsOf('panel_drag_start')).toHaveLength(3)
    const text = screen.getByTestId('ocr-text')
    down(text)
    expect(fireEvent.mouseDown(text, { button: 0 })).toBe(true)
    down(screen.getByTestId('ocr-copy'))
    down(screen.getByTestId('ocr-recognize-again'))
    const thumb = screen.getByTestId('ocr-thumbnail').closest('button')
    if (thumb) down(thumb)
    expect(callsOf('panel_drag_start')).toHaveLength(3)
  })

  it('点一下就完成的控件（复制、芯片、排版按钮左半）按下时不让面板成为 key；文本框、▾ 照旧激活', async () => {
    inspectResult = (text) => ({
      entities: [{ kind: 'email', value: 'a@b.co', start: 0, end: 6, sensitive: false }],
      transforms: fakeTransform(text, 'remove_blank_lines') === text ? [] : ['remove_blank_lines'],
    })
    const { props } = view()
    await recognize('r-ocr', 'a@b.co\n\n第二段')
    await settle()
    fireEvent.click(screen.getByTestId('ocr-translate-toggle'))
    await push('r-1', start('r-1'), delta('Hi'), done())
    const activate = props.activate as ReturnType<typeof vi.fn>
    activate.mockClear()
    for (const el of [
      screen.getByTestId('ocr-copy'),
      screen.getByTestId('ocr-translation-copy'),
      screen.getByTestId('clip-chip'),
      screen.getByTestId('ocr-layout-toggle'),
    ]) {
      down(el)
      // 按在按钮里的图标上也一样
      const icon = el.querySelector('svg')
      if (icon) down(icon)
    }
    expect(activate).not.toHaveBeenCalled()
    down(screen.getByTestId('ocr-text'))
    expect(activate).toHaveBeenCalledTimes(1)
    down(screen.getByTestId('ocr-layout-caret'))
    down(screen.getByTestId('ocr-translate-menu'))
    down(screen.getByTestId('ocr-style-chip'))
    expect(activate).toHaveBeenCalledTimes(4)
  })

  it('复制按钮没有激活面板，而 Rust 要求改由 WebView 写剪贴板（Wayland）时：先让面板成为 key、等拿到焦点再写', async () => {
    const write = vi.fn<(text: string) => Promise<void>>(() => Promise.resolve())
    vi.stubGlobal('navigator', { ...navigator, clipboard: { writeText: write } })
    const focused = vi.spyOn(document, 'hasFocus').mockReturnValue(false)
    try {
      // 激活经 Rust 异步完成：窗口随后收到 focus 事件
      const activate = vi.fn<() => void>(() => {
        setTimeout(() => window.dispatchEvent(new Event('focus')), 0)
      })
      view({ activate })
      await recognize()
      await settle()
      copyRefusal = 'errors.platform.wayland_clipboard'
      down(screen.getByTestId('ocr-copy'))
      expect(activate).not.toHaveBeenCalled()
      fireEvent.click(screen.getByTestId('ocr-copy'))
      await waitFor(() => expect(write).toHaveBeenCalledWith('第一行\n第二行'))
      expect(activate).toHaveBeenCalledTimes(1)
      expect(activate.mock.invocationCallOrder[0]).toBeLessThan(
        write.mock.invocationCallOrder[0] ?? 0,
      )
    } finally {
      focused.mockRestore()
      vi.unstubAllGlobals()
    }
  })

  it('固定窗口宿主：不发 panel_drag_start（由 PinnedApp 的头部拖动）', async () => {
    view({ host: 'pinned' })
    await settle()
    down(screen.getByTestId('ocr-head'))
    down(screen.getByTestId('view-ocr'))
    expect(callsOf('panel_drag_start')).toHaveLength(0)
  })
})
