/**
 * 翻译 / 输入视图测试的假传输层：记录命令、按 request_id 保存流的监听者，测试里手动推送 StreamEvent。
 */
import { longTextAck, longTextGate } from '../../../shared/ipc/mock/longText'
import { act } from '@testing-library/react'
import { boot, setTransport, type Transport } from '../../../shared/ipc'
import { createFixtures } from '../../../shared/ipc/mock/fixtures'
import { getAppearance, setAppearance } from '../../../shared/hooks/useAppearance'
import type {
  EventMap,
  EventName,
  LlmError,
  SettingsSnapshot,
  StartAck,
  StreamEvent,
  TranslateReq,
  Usage,
} from '../../../shared/ipc/types'
import { resetCatalog } from '../catalog'
import { clearBuffered } from '../hub'

type Handler = (args: Record<string, unknown>) => unknown

export interface FakeIpc {
  readonly calls: Array<[string, Record<string, unknown>]>
  readonly starts: TranslateReq[]
  /** request_id → 监听者（translate_start 与 stream_attach 各自登记） */
  readonly listeners: Map<string, Array<(e: StreamEvent) => void>>
  /** stream_attach 时同步回放的事件 */
  readonly replay: Map<string, StreamEvent[]>
  handlers: Record<string, Handler>
  /** translate_start 返回的宽度 */
  ackWidth: number | null
  emit<K extends EventName>(event: K, payload: EventMap[K]): void
  push(requestId: string, ...events: StreamEvent[]): void
  names(): string[]
}

export const MODEL = {
  provider_id: 'openai-main',
  model_id: 'gpt-5.4-mini',
  model_label: 'GPT-5.4 mini',
} as const

export function start(requestId: string): StreamEvent {
  return { type: 'start', request_id: requestId, upstream_id: null, ...MODEL }
}
export function delta(text: string): StreamEvent {
  return { type: 'delta', text }
}
export function done(patch: Partial<Extract<StreamEvent, { type: 'done' }>> = {}): StreamEvent {
  const usage: Usage = {
    input_tokens: 10,
    output_tokens: 20,
    reasoning_tokens: null,
    total_tokens: null,
  }
  return {
    type: 'done',
    elapsed_ms: 1234,
    usage,
    truncated: null,
    cached: false,
    no_text: false,
    ...patch,
  }
}
export function error(code: LlmError['code'], patch: Partial<LlmError> = {}): StreamEvent {
  return {
    type: 'error',
    code,
    subcode: null,
    message: code,
    retryable: false,
    hint: null,
    status: null,
    url: null,
    retry_after_s: null,
    upstream_request_id: null,
    ...patch,
  }
}

export function snapshot(): SettingsSnapshot {
  const f = createFixtures(boot)
  const settings = f.settings()
  const proto = f.providerTemplates().find((t) => t.id === 'openai')?.provider
  const model = f.models()[0]
  if (proto && model) {
    const provider = (id: string, name: string, models: Array<[string, string | null]>) => ({
      ...proto,
      id,
      name,
      models: models.map(([mid, label]) => ({ ...model, id: mid, label })),
    })
    settings.providers = [
      provider('openai-main', 'OpenAI', [['gpt-5.4-mini', 'GPT-5.4 mini']]),
      provider('deepseek', 'DeepSeek', [['deepseek-chat', null]]),
    ]
    settings.defaults = {
      ...settings.defaults,
      translate: { provider_id: 'openai-main', model_id: 'gpt-5.4-mini' },
    }
  }
  return {
    settings,
    styles: f.styles().map((s) => ({
      ...s,
      output: s.id === 'technical' || s.id === 'learn' ? 'markdown_lite' : 'plain_text',
      dictionary_mode: s.id !== 'literal',
    })),
    runtime: f.runtime(),
  }
}

const savedAppearance = getAppearance()

export function installFakeIpc(): FakeIpc {
  // 文案断言按简体中文写；动效照常（减少动效另测）
  setAppearance({ ...savedAppearance, uiLanguage: 'zh-CN', reduceMotion: false })
  const eventListeners = new Map<string, Set<(p: unknown) => void>>()
  let seq = 0
  const ipc: FakeIpc = {
    calls: [],
    starts: [],
    listeners: new Map(),
    replay: new Map(),
    handlers: {},
    ackWidth: null,
    emit(event, payload) {
      for (const cb of eventListeners.get(event) ?? []) cb(payload)
    },
    push(requestId, ...events) {
      act(() => {
        for (const e of events) for (const l of ipc.listeners.get(requestId) ?? []) l(e)
      })
    },
    names: () => ipc.calls.map(([c]) => c),
  }
  const listen = (id: string, l: (e: StreamEvent) => void) => {
    ipc.listeners.set(id, [...(ipc.listeners.get(id) ?? []), l])
  }
  const transport: Transport = {
    kind: 'mock',
    invoke: <T>(cmd: string, args?: Record<string, unknown>) => {
      ipc.calls.push([cmd, args ?? {}])
      const h = ipc.handlers[cmd]
      if (h) {
        try {
          return Promise.resolve(h(args ?? {}) as T)
        } catch (e) {
          return Promise.reject(e)
        }
      }
      if (cmd === 'settings_get') return Promise.resolve(snapshot() as T)
      if (cmd === 'panel_pin') return Promise.resolve('p1' as T)
      return Promise.resolve(undefined as T)
    },
    openStream: <T>(
      cmd: string,
      args: Record<string, unknown>,
      onEvent: (e: StreamEvent) => void,
    ) => {
      ipc.calls.push([cmd, args])
      const h = ipc.handlers[cmd]
      if (h) {
        try {
          const r = h({ ...args, onEvent })
          return Promise.resolve(r as T)
        } catch (e) {
          return Promise.reject(e)
        }
      }
      if (cmd === 'translate_start') {
        const req = args.req as TranslateReq
        // 与 Rust 同一长文本闸门：没确认过的长文本不发请求
        const long = longTextGate(req, snapshot().settings.translate.long_text_threshold)
        if (long) return Promise.resolve(longTextAck(long, ipc.ackWidth) as T)
        ipc.starts.push(req)
        const id = `r${++seq}`
        listen(id, onEvent)
        const ack: StartAck = { request_id: id, width: ipc.ackWidth }
        return Promise.resolve(ack as T)
      }
      if (cmd === 'stream_attach') {
        const id = args.request_id as string
        listen(id, onEvent)
        for (const e of ipc.replay.get(id) ?? []) onEvent(e)
        return Promise.resolve(undefined as T)
      }
      return Promise.reject(new Error(`unexpected stream ${cmd}`))
    },
    listen: (event, cb) => {
      const set = eventListeners.get(event) ?? new Set()
      set.add(cb as (p: unknown) => void)
      eventListeners.set(event, set)
      return Promise.resolve(() => set.delete(cb as (p: unknown) => void))
    },
    window: {
      label: 'panel',
      startDragging: () => Promise.resolve(),
      close: () => {
        ipc.calls.push(['window.close', {}])
        return Promise.resolve()
      },
    },
    protocolUrl: (p) => p,
  }
  setTransport(transport)
  resetCatalog(snapshot())
  clearBuffered()
  return ipc
}

/** 用例之间：吞掉迟到调用的占位传输层（不要清成 null，见 shell 测试的说明） */
export function idleTransport(): void {
  setAppearance(savedAppearance)
  setTransport({
    kind: 'mock',
    invoke: <T>() => Promise.resolve(undefined as T),
    openStream: <T>() => new Promise<T>(() => undefined),
    listen: () => Promise.resolve(() => undefined),
    window: {
      label: 'panel',
      startDragging: () => Promise.resolve(),
      close: () => Promise.resolve(),
    },
    protocolUrl: (p) => p,
  })
}

/** 模拟按键（⌘ 在 macOS，其他平台 Ctrl；与 keys/combo 的 Mod 一致） */
export function press(code: string, mods: { mod?: boolean; shift?: boolean } = {}): void {
  const mac = boot.os === 'macos'
  const key = code.startsWith('Key')
    ? code.slice(3).toLowerCase()
    : code.startsWith('Digit')
      ? code.slice(5)
      : code === 'Period'
        ? '.'
        : code
  act(() => {
    ;(document.activeElement ?? document.body).dispatchEvent(
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
  })
}

/** 等待微任务（命令的 Promise）落定 */
export async function flush(): Promise<void> {
  await act(async () => {
    await Promise.resolve()
    await Promise.resolve()
  })
}
