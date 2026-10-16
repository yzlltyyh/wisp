/**
 * OCR 视图与离线模型 / PaddleOCR 云端：等待阶段（正在加载离线模型、云端排队）、识别模型菜单的分组与一次性指定、
 * 引擎不支持的模式置灰、结构化结果的表格预览与源码切换、部分区域未能识别的提示、未就绪卡片的就地下载与完成后自动识别
 * （每一轮下载最多自动识别一次）。
 */
import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { ViewProps } from '../../../panel/types'
import { getAppearance, setAppearance } from '../../../shared/hooks/useAppearance'
import { setTransport, type Transport } from '../../../shared/ipc'
import { LocalModelsSnapshot as Golden } from '../../../shared/ipc/fixtures'
import type {
  LocalModelsSnapshot,
  ModelEntry,
  OcrPayload,
  Provider,
  SettingsSnapshot,
  StreamEvent,
} from '../../../shared/ipc/types'
import { MotionRoot } from '../../../shared/motion/MotionRoot'
import { resetOfflineModels } from '../../../shared/offline/store'
import { OcrView, type OcrViewProps } from '../OcrView'
import { resetSettingsSnapshot } from '../settings'

type Listener = (e: StreamEvent) => void
let calls: Array<[string, Record<string, unknown>]>
let streams: Map<string, Listener>
let events: Map<string, Set<(p: unknown) => void>>
let seq: number
let offline: LocalModelsSnapshot

function model(id: string, vision: ModelEntry['vision'] = 'yes'): ModelEntry {
  return {
    id,
    label: null,
    vision,
    vision_source: null,
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

function provider(id: string, name: string, models: ModelEntry[], paddle = false): Provider {
  return {
    id,
    name,
    protocol: paddle ? 'paddle_ocr' : 'openai_chat',
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

function snapshot(local: string | null = null): SettingsSnapshot {
  return {
    settings: {
      providers: [
        provider(
          'paddle',
          'PaddleOCR',
          [model('PP-OCRv6', 'unknown'), model('PP-StructureV3', 'unknown')],
          true,
        ),
        provider('qwen', '通义千问', [model('vl-plus')]),
      ],
      defaults: { translate: null, ocr: { provider_id: 'qwen', model_id: 'vl-plus' } },
      translate: { default_style: 'natural', primary_lang: 'zh-CN' },
      ocr: { auto_copy: false },
      offline: { ocr_model: local, translate_model: null },
    },
    styles: [],
    runtime: { hotkeys: [], offline: { installed: ['pp_ocrv6_small', 'paddleocr_vl_1_6'] } },
  } as unknown as SettingsSnapshot
}

function transport(): Transport {
  return {
    kind: 'mock',
    invoke: <T,>(cmd: string, args?: Record<string, unknown>) => {
      calls.push([cmd, args ?? {}])
      if (cmd === 'offline_models_get') return Promise.resolve(structuredClone(offline) as T)
      if (cmd === 'text_inspect') return Promise.resolve({ entities: [], transforms: [] } as T)
      return Promise.resolve(undefined as T)
    },
    openStream: <T,>(cmd: string, args: Record<string, unknown>, onEvent: Listener) => {
      calls.push([cmd, args])
      if (cmd === 'stream_attach') {
        streams.set(args.request_id as string, onEvent)
        return Promise.resolve(undefined as T)
      }
      const id = `r-${++seq}`
      streams.set(id, onEvent)
      return Promise.resolve({ request_id: id, width: null } as T)
    },
    listen: (event, cb) => {
      const set = events.get(event) ?? new Set()
      set.add(cb as (p: unknown) => void)
      events.set(event, set)
      return Promise.resolve(() => set.delete(cb as (p: unknown) => void))
    },
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
  ...extra,
})

function view(p = payload()) {
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
  }
  return render(
    <MotionRoot>
      <OcrView {...props} />
    </MotionRoot>,
  )
}

async function push(id: string, ...list: StreamEvent[]) {
  await act(async () => {})
  const cb = streams.get(id)
  if (!cb) throw new Error(`没有请求 ${id}`)
  await act(async () => {
    for (const e of list) cb(e)
    await Promise.resolve()
  })
}

const start = (id: string, provider_id = 'qwen', model_id = 'vl-plus'): StreamEvent => ({
  type: 'start',
  request_id: id,
  provider_id,
  model_id,
  model_label: model_id,
  upstream_id: null,
})
const done: StreamEvent = {
  type: 'done',
  elapsed_ms: 300,
  usage: null,
  truncated: null,
  cached: false,
  no_text: false,
}
const callsOf = (cmd: string) => calls.filter(([c]) => c === cmd)
const lastReq = (cmd: string) =>
  (callsOf(cmd).at(-1)?.[1] as { req?: Record<string, unknown> } | undefined)?.req

const saved = getAppearance()
beforeEach(() => {
  calls = []
  streams = new Map()
  events = new Map()
  seq = 0
  offline = structuredClone(Golden) as LocalModelsSnapshot
  setTransport(transport())
  resetOfflineModels()
  resetSettingsSnapshot(snapshot())
  setAppearance({ ...saved, uiLanguage: 'zh-CN', os: 'macos' })
})
afterEach(() => {
  resetSettingsSnapshot()
  resetOfflineModels()
  setAppearance(saved)
})

describe('等待阶段', () => {
  it('离线模型冷启动：头部与骨架下方说明「正在加载离线模型」，第一段文字到达后恢复', async () => {
    view()
    await push('r-ocr', start('r-ocr', '@offline', 'pp_ocrv6_small'), {
      type: 'phase',
      phase: 'loading_model',
    })
    expect(screen.getByTestId('view-ocr')).toHaveTextContent('正在加载离线模型…')
    // 补充说明一开始就在（布局不跳），由 CSS 几秒后才淡入
    const late = within(screen.getByTestId('ocr-stream')).getByText(
      '第一次加载需要十几秒，之后会快很多',
    )
    expect(late).toHaveClass('ui-late')
    await push('r-ocr', { type: 'delta', text: '你好' })
    expect(screen.getByTestId('view-ocr')).not.toHaveTextContent('正在加载离线模型…')
  })

  it('云端排队：头部「云端处理中…」；PP-StructureV3 在骨架下方注明可能要等几十秒', async () => {
    resetSettingsSnapshot({
      ...snapshot(),
      settings: {
        ...snapshot().settings,
        defaults: { translate: null, ocr: { provider_id: 'paddle', model_id: 'PP-StructureV3' } },
      },
    } as SettingsSnapshot)
    view(payload({ mode: 'markdown' }))
    await push('r-ocr', start('r-ocr', 'paddle', 'PP-StructureV3'), {
      type: 'phase',
      phase: 'queued',
    })
    expect(screen.getByTestId('view-ocr')).toHaveTextContent('云端处理中…')
    expect(screen.getByTestId('ocr-stream')).toHaveTextContent(
      'PP-StructureV3 通常较慢，服务繁忙时要等几十秒',
    )
  })
})

describe('识别模型菜单与模式', () => {
  it('分组列出已下载的离线模型、PaddleOCR 与视觉模型；选离线模型带 @offline 引用重新识别', async () => {
    view()
    await push('r-ocr', start('r-ocr'), { type: 'delta', text: '第一行' }, done)
    fireEvent.click(screen.getByTestId('ocr-recognize-menu'))
    fireEvent.click(await screen.findByRole('menuitem', { name: /识别模型/ }))
    const page = (await screen.findByTestId('ocr-model-menu')).querySelector<HTMLElement>(
      '[data-page="models"]',
    ) as HTMLElement
    expect(within(page).getByText('离线模型')).toBeInTheDocument()
    expect(within(page).getByText('PaddleOCR')).toBeInTheDocument()
    expect(within(page).getByText('视觉模型')).toBeInTheDocument()
    // 文字识别服务不提示「未确认是否支持图片」
    expect(within(page).queryByText('未确认是否支持图片')).toBeNull()
    fireEvent.click(within(page).getByText('PP-OCRv6 Small'))
    expect(lastReq('ocr_start')).toMatchObject({
      model_ref: { provider_id: '@offline', model_id: 'pp_ocrv6_small' },
    })
  })

  it('本机选用 PP-OCRv6：「结构化」置灰并说明；结构化模式下换成 PP-OCRv6 时改按段落识别', async () => {
    resetSettingsSnapshot(snapshot('pp_ocrv6_small'))
    view()
    await push(
      'r-ocr',
      start('r-ocr', '@offline', 'pp_ocrv6_small'),
      { type: 'delta', text: 'a' },
      done,
    )
    fireEvent.click(screen.getByTestId('ocr-recognize-menu'))
    const list = await screen.findByTestId('ocr-recognize-list')
    const markdown = within(list).getByTestId('ocr-recognize-list-item-mode-markdown')
    expect(markdown).toHaveAttribute('aria-disabled', 'true')
    expect(markdown).toHaveTextContent('PP-OCRv6 Small 只识别文字，不支持结构化')
  })

  it('结构化模式下改用云端 PP-OCRv6：改按段落识别（不留下一个不支持的选中项）', async () => {
    view(payload({ mode: 'markdown' }))
    await push('r-ocr', start('r-ocr'), { type: 'delta', text: '# 标题' }, done)
    fireEvent.click(screen.getByTestId('ocr-recognize-menu'))
    fireEvent.click(await screen.findByRole('menuitem', { name: /识别模型/ }))
    const page = (await screen.findByTestId('ocr-model-menu')).querySelector<HTMLElement>(
      '[data-page="models"]',
    ) as HTMLElement
    fireEvent.click(within(page).getByText('PP-OCRv6'))
    expect(lastReq('ocr_start')).toMatchObject({
      mode: 'paragraphs',
      model_ref: { provider_id: 'paddle', model_id: 'PP-OCRv6' },
    })
  })
})

describe('结构化结果的预览', () => {
  it('有表格时默认显示预览（真正的表格），切到源码后是可编辑的文本框', async () => {
    view(payload({ mode: 'markdown' }))
    await push(
      'r-ocr',
      start('r-ocr'),
      { type: 'delta', text: '| a | b |\n| --- | --- |\n| 1 | 2 |' },
      done,
    )
    const preview = await screen.findByTestId('ocr-preview')
    expect(preview.querySelector('table')).not.toBeNull()
    const toggle = screen.getByTestId('ocr-source-toggle')
    expect(toggle).toHaveAccessibleName('显示 Markdown 源码')
    fireEvent.click(toggle)
    expect(await screen.findByTestId('ocr-text')).toHaveValue('| a | b |\n| --- | --- |\n| 1 | 2 |')
    expect(screen.queryByTestId('ocr-preview')).toBeNull()
  })

  it('文档有区域没能识别：状态行一句安静的提示（不用警示色），识别出的部分照常显示', async () => {
    view(payload({ mode: 'markdown' }))
    await push(
      'r-ocr',
      start('r-ocr', '@offline', 'paddleocr_vl_1_6'),
      { type: 'notice', kind: 'partial' },
      { type: 'delta', text: '| a | b |\n| --- | --- |\n| 1 | 2 |' },
      done,
    )
    expect(await screen.findByTestId('ocr-preview')).toBeInTheDocument()
    const line = within(screen.getByTestId('ocr-footer')).getByTestId('status-line')
    expect(line).toHaveTextContent('部分区域未能识别')
    expect(line.querySelector('[data-tone="warning"]')).toBeNull()
  })

  it('没有表格 / 公式的结构化结果照常是可编辑的文本，不显示切换按钮', async () => {
    view(payload({ mode: 'markdown' }))
    await push('r-ocr', start('r-ocr'), { type: 'delta', text: '## 标题\n\n- 一\n- 二' }, done)
    expect(await screen.findByTestId('ocr-text')).toBeInTheDocument()
    expect(screen.queryByTestId('ocr-source-toggle')).toBeNull()
  })
})

describe('未就绪：离线模型还没下载', () => {
  const missing = { reason: 'offline_model_missing' as const, model: 'pp_ocrv6_small' as const }
  const installed = (s: LocalModelsSnapshot): LocalModelsSnapshot => ({
    ...s,
    models: s.models.map((m) =>
      m.id === 'pp_ocrv6_small' ? { ...m, state: { state: 'installed' } } : m,
    ),
  })
  const emit = (event: string, p: unknown) =>
    act(() => {
      for (const cb of events.get(event) ?? []) cb(p)
    })

  it('就地下载；截图还在时下载完成即识别这张截图', async () => {
    offline = {
      ...offline,
      models: offline.models.map((m) =>
        m.id === 'pp_ocrv6_small' ? { ...m, state: { state: 'not_installed' } } : m,
      ),
    }
    view(payload({ request_id: null, not_ready: missing }))
    const button = await screen.findByTestId('ocr-offline-card-download')
    expect(screen.getByTestId('ocr-offline-card')).toHaveTextContent('PP-OCRv6 Small 尚未下载')
    fireEvent.click(button)
    await waitFor(() => expect(callsOf('offline_model_download')).toHaveLength(1))
    expect(callsOf('ocr_start')).toHaveLength(0)
    emit('offline_models:changed', installed(offline))
    await waitFor(() => expect(callsOf('ocr_start')).toHaveLength(1))
    expect(lastReq('ocr_start')).toMatchObject({ capture_id: 'cap-1', model_ref: null })
    expect(screen.queryByTestId('ocr-offline-card')).toBeNull()
  })

  it('每一轮下载最多自动识别一次：再次落到卡片时只给手动「重试」，不会循环', async () => {
    resetOfflineModels(installed(offline))
    // 模型装好了，Rust 仍判为未就绪（第一次：这一轮还没有自动识别过）
    const first = view(payload({ request_id: null, not_ready: missing }))
    await waitFor(() => expect(callsOf('ocr_start')).toHaveLength(1))
    first.unmount()
    // 再次未就绪：新挂载的卡片不再自动识别
    view(payload({ request_id: null, not_ready: missing }))
    const retry = await screen.findByTestId('ocr-offline-card-retry')
    expect(callsOf('ocr_start')).toHaveLength(1)
    fireEvent.click(retry)
    await waitFor(() => expect(callsOf('ocr_start')).toHaveLength(2))
  })

  it('还没有截图（Rust 在截图前就发现模型没下载）：下载完成后提示重新截图', async () => {
    view(payload({ request_id: null, capture_id: '', not_ready: missing }))
    await screen.findByTestId('ocr-offline-card')
    emit('offline_models:changed', installed(offline))
    expect(await screen.findByTestId('ocr-offline-retake')).toBeInTheDocument()
    fireEvent.click(screen.getByTestId('ocr-offline-retake'))
    expect(callsOf('trigger_action').at(-1)?.[1]).toEqual({
      action: 'ocr',
      text: null,
      origin: 'panel',
    })
    expect(callsOf('ocr_start')).toHaveLength(0)
  })
})
