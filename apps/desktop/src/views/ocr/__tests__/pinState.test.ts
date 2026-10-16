/**
 * 固定窗口交接的组装与还原（pinState.ts）：OCR 模式按识别 / 译文区的状态决定接哪个请求、带哪段文字；直接翻译带上
 * 原文转录；还原时识别已结束就不再接入识别请求，译文区展开时接上译文请求；组装出的状态还原回来与面板一致。
 */
import { describe, expect, it } from 'vitest'
import type { OcrMode, PinState } from '../../../shared/ipc/types'
import { pinRestoreOf, pinStateOf, statusLineOf, type PinInput, type RunView } from '../pinState'
import { seedOf, type DoneEvent, type MetaEvent, type RunState, type StartEvent } from '../useRun'

const BASE: RunState = {
  key: 1,
  phase: 'idle',
  requestId: null,
  start: null,
  meta: null,
  done: null,
  error: null,
  notices: [],
  hasText: false,
  longText: null,
  stage: null,
}

const state = (patch: Partial<RunState> = {}): RunState => ({ ...BASE, ...patch })
const run = (patch: Partial<RunState> = {}, text = ''): RunView => ({
  state: state({ hasText: text.trim() !== '', ...patch }),
  text: () => text,
})
const start = (id: string, label = 'Vision'): StartEvent => ({
  type: 'start',
  request_id: id,
  provider_id: 'p1',
  model_id: 'm1',
  model_label: label,
  upstream_id: null,
})
const done = (patch: Partial<DoneEvent> = {}): DoneEvent => ({
  type: 'done',
  elapsed_ms: 1240,
  usage: null,
  truncated: null,
  cached: false,
  no_text: false,
  ...patch,
})
const meta = (target: MetaEvent['target_lang']): MetaEvent => ({
  type: 'meta',
  detected_source_lang: 'en',
  target_lang: target,
  dict_mode: null,
  low_confidence: null,
})

const PAYLOAD: PinInput['payload'] = {
  capture_id: 'cap-1',
  target_lang: 'zh-CN',
  thumb_url: 'wisp://thumb/cap-1',
  image_size: { width: 800, height: 400 },
}

function input(patch: Partial<PinInput> = {}): PinInput {
  return {
    direct: false,
    payload: PAYLOAD,
    main: run(),
    tr: run(),
    src: run(),
    mode: 'lines' as OcrMode,
    ocrText: '',
    style: 'natural',
    target: null,
    trOpen: false,
    srcOpen: false,
    ...patch,
  }
}

const recognized = (text: string) =>
  run({ phase: 'done', requestId: 'r-ocr', start: start('r-ocr'), done: done() }, text)

describe('状态行', () => {
  it('模型 · 耗时；命中缓存时不写耗时；什么都没有时为 null', () => {
    expect(statusLineOf(state({ start: start('r'), done: done() }))).toBe('Vision · 1.2 s')
    expect(statusLineOf(state({ start: start('r'), done: done({ cached: true }) }))).toBe('Vision')
    expect(statusLineOf(state())).toBeNull()
  })
})

describe('组装（OCR 模式）', () => {
  it('还在识别：不带文字，request_id 是识别请求', () => {
    const p = pinStateOf(
      input({ main: run({ phase: 'streaming', requestId: 'r-ocr', start: start('r-ocr') }, '半') }),
    )
    expect(p).toMatchObject({
      kind: 'ocr',
      source_text: '',
      result_text: '',
      ocr_text: null,
      request_id: 'r-ocr',
      show_source: false,
      ocr_mode: 'lines',
      capture_id: 'cap-1',
      status_line: 'Vision',
      model_ref: { provider_id: 'p1', model_id: 'm1' },
    })
    expect(p.status).toMatchObject({ model_label: 'Vision', elapsed_ms: null, cancelled: false })
  })

  it('识别完成、译文区展开且译文请求有 id：接译文请求，文字用编辑后的', () => {
    const p = pinStateOf(
      input({
        main: recognized('原文'),
        ocrText: '改过的原文',
        mode: 'paragraphs',
        trOpen: true,
        tr: run({ phase: 'done', requestId: 'r-tr', meta: meta('ja'), done: done() }, '訳文'),
        style: 'formal',
      }),
    )
    expect(p).toMatchObject({
      source_text: '改过的原文',
      ocr_text: '改过的原文',
      result_text: '訳文',
      request_id: 'r-tr',
      show_source: true,
      target_lang: 'ja',
      detected_lang: 'en',
      style_id: 'formal',
      ocr_mode: 'paragraphs',
      status_line: 'Vision · 1.2 s',
    })
  })

  it('译文区展开但译文请求还没有 id：不把识别请求接到译文区上', () => {
    const p = pinStateOf(
      input({
        main: recognized('原文'),
        ocrText: '原文',
        trOpen: true,
        tr: run({ phase: 'waiting' }),
      }),
    )
    expect(p).toMatchObject({ request_id: 'r-ocr', show_source: false, result_text: '' })
  })

  it('目标语言：译文的实际目标 > 用户换过的 > 载荷里的', () => {
    const base = { main: recognized('原文'), ocrText: '原文' }
    expect(pinStateOf(input(base)).target_lang).toBe('zh-CN')
    expect(pinStateOf(input({ ...base, target: 'ko' })).target_lang).toBe('ko')
    expect(
      pinStateOf(input({ ...base, target: 'ko', tr: run({ meta: meta('ja') }) })).target_lang,
    ).toBe('ja')
  })

  it('缩略图与尺寸：没有缩略图、尺寸未知时为 null', () => {
    const p = pinStateOf(
      input({ payload: { ...PAYLOAD, thumb_url: '', image_size: { width: 0, height: 0 } } }),
    )
    expect(p.thumb_url).toBeNull()
    expect(p.image_size).toBeNull()
    expect(pinStateOf(input()).image_size).toEqual({ width: 800, height: 400 })
  })
})

describe('组装（直接翻译）', () => {
  it('正文是译文；转录完成才带 ocr_text，「原文」是否展开照实交接', () => {
    const main = run(
      { phase: 'done', requestId: 'r-dt', start: start('r-dt'), meta: meta('en'), done: done() },
      'Translated',
    )
    const p = pinStateOf(
      input({
        direct: true,
        main,
        src: run({ phase: 'done', requestId: 'r-src' }, '原文'),
        srcOpen: true,
      }),
    )
    expect(p).toMatchObject({
      source_text: '原文',
      result_text: 'Translated',
      ocr_text: '原文',
      ocr_mode: 'translate',
      request_id: 'r-dt',
      show_source: true,
      target_lang: 'en',
    })
    const transcribing = pinStateOf(
      input({ direct: true, main, src: run({ phase: 'streaming', requestId: 'r-src' }, '原') }),
    )
    expect(transcribing).toMatchObject({ source_text: '原', ocr_text: null, show_source: false })
  })
})

function pin(patch: Partial<PinState> = {}): PinState {
  return {
    kind: 'ocr',
    source_text: '',
    result_text: '',
    task: 'translate',
    style_id: 'natural',
    target_lang: 'zh-CN',
    detected_lang: null,
    request_id: null,
    capture_id: 'cap-1',
    ocr_text: null,
    ocr_mode: 'lines',
    show_source: false,
    status_line: null,
    ...patch,
  }
}

describe('还原', () => {
  const status = {
    model_label: 'Vision',
    elapsed_ms: 1240,
    usage: null,
    cached: false,
    cancelled: false,
    truncated: null,
  }
  const model = { provider_id: 'p1', model_id: 'm1' }

  it('还在识别：接上识别请求，日志已释放时退回 source_text', () => {
    const plan = pinRestoreOf(pin({ request_id: 'r-ocr', source_text: '半' }), false)
    expect(plan.main).toEqual({ attach: 'r-ocr', text: '半', extra: {}, phase: 'done' })
    expect(plan.source).toBeNull()
    expect(plan.translation).toBeNull()
  })

  it('识别已结束：正文用 ocr_text、不接识别请求；模型与耗时取结构化状态行', () => {
    const p = pin({ request_id: 'r-ocr', ocr_text: '改过的', status, model_ref: model })
    const plan = pinRestoreOf(p, false)
    expect(plan.main).toEqual({
      attach: null,
      text: '改过的',
      extra: seedOf(status, model),
      phase: 'done',
    })
    expect(plan.translation).toBeNull()
  })

  it('译文区展开：接上译文请求，译文以 ocr_text 与交接的风格为依据', () => {
    const p = pin({
      request_id: 'r-tr',
      ocr_text: '原文',
      result_text: '译文',
      show_source: true,
      style_id: 'formal',
    })
    expect(pinRestoreOf(p, false).translation).toEqual({
      attach: 'r-tr',
      text: '译文',
      extra: {},
      phase: 'done',
      basis: { text: '原文', style: 'formal', target: null },
    })
  })

  it('固定前已取消：正文区仍是已取消，译文区照常结束', () => {
    const p = pin({
      ocr_text: '一半',
      show_source: true,
      result_text: '译文',
      status: { ...status, cancelled: true },
    })
    const plan = pinRestoreOf(p, false)
    expect(plan.main.phase).toBe('cancelled')
    expect(plan.translation?.phase).toBe('done')
  })

  it('直接翻译：正文接译文请求；有转录才展开「原文」', () => {
    const p = pin({
      ocr_mode: 'translate',
      request_id: 'r-dt',
      result_text: 'Hi',
      show_source: true,
    })
    expect(pinRestoreOf(p, true)).toEqual({
      main: { attach: 'r-dt', text: 'Hi', extra: {}, phase: 'done' },
      source: { text: null, open: false },
      translation: null,
    })
    expect(pinRestoreOf({ ...p, ocr_text: '你好' }, true).source).toEqual({
      text: '你好',
      open: true,
    })
  })

  it('组装出的状态还原回来：文字、请求与译文依据与面板一致', () => {
    const p = pinStateOf(
      input({
        main: recognized('原文'),
        ocrText: '改过的原文',
        trOpen: true,
        tr: run({ phase: 'done', requestId: 'r-tr', done: done() }, '译文'),
        style: 'formal',
      }),
    )
    const plan = pinRestoreOf(p, false)
    expect(plan.main).toMatchObject({ attach: null, text: '改过的原文', phase: 'done' })
    expect(plan.main.extra.start?.model_label).toBe('Vision')
    expect(plan.translation).toMatchObject({
      attach: 'r-tr',
      text: '译文',
      basis: { text: '改过的原文', style: 'formal' },
    })
  })
})
