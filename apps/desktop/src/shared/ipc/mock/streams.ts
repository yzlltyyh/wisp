/**
 * mock 的流注册表：语义与 Rust 的 StreamRegistry 一致——
 * 请求一开始就播放并写入事件日志，`stream_attach` 先回放完整日志再推送实时事件；`cancel` 以 `Error{cancelled}` 结束。
 */
import type { StreamListener } from '../commands'
import type { LlmError, StreamEvent } from '../types'
import type { StreamScript, StreamStep, TextStreamOptions } from './types'

interface Entry {
  readonly id: string
  readonly log: StreamEvent[]
  readonly subscribers: Set<StreamListener>
  timer: ReturnType<typeof setTimeout> | null
  done: boolean
}

const DEFAULT_MODEL = {
  provider_id: 'openai-main',
  model_id: 'gpt-5.4-mini',
  model_label: 'GPT-5.4 mini',
}

export class MockStreams {
  private readonly entries = new Map<string, Entry>()
  private readonly queued: StreamScript[] = []
  private seq = 0

  /** 进行中的流数量（自动「稳定」判定用） */
  get active(): number {
    let n = 0
    for (const e of this.entries.values()) if (!e.done) n++
    return n
  }

  newRequestId(): string {
    let id: string
    do id = `r-${++this.seq}`
    while (this.entries.has(id))
    return id
  }

  has(requestId: string): boolean {
    return this.entries.has(requestId)
  }

  /** 场景预置：下一次 translate_start / ocr_start 用这段脚本 */
  enqueue(script: StreamScript): void {
    this.queued.push(script)
  }

  takeQueued(): StreamScript | undefined {
    return this.queued.shift()
  }

  /** 立即开始播放（相当于 Rust 在 t0 发出请求）；同 id 的旧流先取消 */
  start(requestId: string, script: StreamScript): void {
    const old = this.entries.get(requestId)
    if (old) this.stop(old)
    const entry: Entry = {
      id: requestId,
      log: [],
      subscribers: new Set(),
      timer: null,
      done: false,
    }
    this.entries.set(requestId, entry)
    let index = 0
    const step = () => {
      entry.timer = null
      while (index < script.length && !entry.done) {
        const s = script[index++] as StreamStep
        if ('wait' in s) {
          entry.timer = setTimeout(step, s.wait)
          return
        }
        this.push(entry, s.type === 'start' ? { ...s, request_id: requestId } : s)
      }
    }
    step()
  }

  /** 回放 + 订阅；未知 id 抛 not_found（与 Rust 一致） */
  attach(requestId: string, listener: StreamListener): void {
    const entry = this.entries.get(requestId)
    if (!entry) {
      throw { code: 'not_found', message: `mock: unknown request ${requestId}`, llm: null }
    }
    for (const event of entry.log) listener(structuredClone(event))
    if (!entry.done) entry.subscribers.add(listener)
  }

  cancel(requestId: string): void {
    const entry = this.entries.get(requestId)
    if (!entry || entry.done) return
    this.push(entry, errorEvent({ code: 'cancelled', message: 'cancelled', retryable: false }))
  }

  cancelAll(): void {
    for (const id of this.entries.keys()) this.cancel(id)
  }

  private push(entry: Entry, event: StreamEvent): void {
    entry.log.push(event)
    for (const listener of entry.subscribers) listener(structuredClone(event))
    if (event.type === 'done' || event.type === 'error') this.stop(entry)
  }

  private stop(entry: Entry): void {
    entry.done = true
    entry.subscribers.clear()
    if (entry.timer !== null) clearTimeout(entry.timer)
    entry.timer = null
  }
}

// ---------------------------------------------------------------------------
// 脚本构造

const CJK = /[぀-ヿ㐀-鿿가-힯＀-￯]/

function startEvent(model: TextStreamOptions['model']): StreamEvent {
  return { type: 'start', request_id: '', upstream_id: null, ...(model ?? DEFAULT_MODEL) }
}

function metaEvent(meta: TextStreamOptions['meta']): StreamEvent | null {
  if (!meta) return null
  return {
    detected_source_lang: null,
    target_lang: null,
    dict_mode: null,
    low_confidence: null,
    ...meta,
    type: 'meta',
  }
}

function doneEvent(
  elapsed: number,
  outputChars: number,
  done: TextStreamOptions['done'],
): StreamEvent {
  return {
    elapsed_ms: Math.round(elapsed),
    usage: {
      input_tokens: 180,
      output_tokens: Math.max(1, Math.round(outputChars / 2.5)),
      reasoning_tokens: null,
      total_tokens: null,
    },
    truncated: null,
    cached: false,
    no_text: outputChars === 0,
    ...done,
    type: 'done',
  }
}

export function errorEvent(error: Partial<LlmError> & { code: LlmError['code'] }): StreamEvent {
  return {
    subcode: null,
    message: error.code,
    retryable: false,
    hint: null,
    status: null,
    url: null,
    retry_after_s: null,
    upstream_request_id: null,
    ...error,
    type: 'error',
  }
}

/** 把文本切成 chunk：CJK 每块 2 字，其他每块约 4 字符（尽量在空白处断开） */
export function chunkText(text: string, chunkChars?: number): string[] {
  const chars = Array.from(text)
  const out: string[] = []
  let i = 0
  while (i < chars.length) {
    const size = chunkChars ?? (CJK.test(chars[i] ?? '') ? 2 : 4)
    let end = Math.min(chars.length, i + size)
    if (!chunkChars) {
      // 拉丁文本延伸到下一个空白，避免把单词切得太碎
      while (end < chars.length && end - i < size + 6 && !/\s/.test(chars[end] ?? ' ')) end++
    }
    out.push(chars.slice(i, end).join(''))
    i = end
  }
  return out
}

export function textStream(text: string, options: TextStreamOptions = {}): StreamScript {
  const interval = options.intervalMs ?? 20
  const first = options.firstTokenMs ?? 280
  const steps: StreamStep[] = [startEvent(options.model)]
  const meta = metaEvent(options.meta)
  if (meta) steps.push(meta)
  steps.push(...(options.before ?? []))
  steps.push({ wait: first })
  const chunks = chunkText(text, options.chunkChars)
  chunks.forEach((chunk, i) => {
    if (i > 0) steps.push({ wait: interval })
    steps.push({ type: 'delta', text: chunk })
  })
  steps.push(doneEvent(first + chunks.length * interval, text.length, options.done))
  return steps
}

const STRESS_TOKENS = [
  '流式',
  '渲染',
  '只追加',
  'span',
  '，',
  'the ',
  'panel ',
  'never ',
  'blocks ',
  'input',
  '。',
  '\n\n',
  '性能',
  '预算',
  ' ≤ ',
  '17 ms',
  '；',
  'Motion ',
  'stays ',
  'smooth',
  '.',
]

/** 压力流：每 16 ms 一个 tick，按速率累积 token（流畅度场景 `translate.stress-stream`） */
export function streamRate(
  tokensPerSec: number,
  total: number,
  options: TextStreamOptions = {},
): StreamScript {
  const tick = 16
  const perTick = (tokensPerSec * tick) / 1000
  const steps: StreamStep[] = [startEvent(options.model)]
  const meta = metaEvent(options.meta)
  if (meta) steps.push(meta)
  steps.push({ wait: options.firstTokenMs ?? 200 })
  let emitted = 0
  let carry = 0
  let chars = 0
  let k = 0
  while (emitted < total) {
    carry += perTick
    const n = Math.min(total - emitted, Math.floor(carry))
    carry -= n
    if (n > 0) {
      let text = ''
      for (let i = 0; i < n; i++) text += STRESS_TOKENS[k++ % STRESS_TOKENS.length]
      chars += text.length
      emitted += n
      steps.push({ type: 'delta', text })
    }
    if (emitted < total) steps.push({ wait: tick })
  }
  steps.push(doneEvent((total / tokensPerSec) * 1000, chars, options.done))
  return steps
}

export function errorStream(
  error: Partial<LlmError> & { code: LlmError['code'] },
  partialText = '',
): StreamScript {
  const steps: StreamStep[] = [startEvent(undefined), { wait: 320 }]
  for (const chunk of chunkText(partialText))
    steps.push({ type: 'delta', text: chunk }, { wait: 20 })
  steps.push(errorEvent(error))
  return steps
}
