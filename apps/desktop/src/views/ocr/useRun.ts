/**
 * 一次流式请求的状态机：idle → waiting（还没有非空白文字）→ streaming → done | error | cancelled；
 * waiting → confirm（长文本被 Rust 拦下、请求没有发出，`StartAck.long_text`）。
 * OCR 文本区、译文区、「查看原文」的转录都用它（StreamEvent 语义见 shared/ipc/types.ts）。
 *
 * - **流式文本不进 state**：Delta 累积在 ref 里并直接追加到 StreamView（`sink` 拿到的句柄），只有阶段变化
 *   （第一个非空白 Delta、终止事件）才触发重渲染；StreamView 以 `state.key` 为 key、`initialText={text()}` 挂载，
 *   挂载前到达的 Delta 不会丢；
 * - 每次 `begin` 递增 key，旧请求的迟到事件按 key 丢弃；旧请求还在进行时顺带 `cancel`（换模式 / 模型 / 风格时
 *   不再为被放弃的结果付费）；
 * - 卸载时**不**取消：面板隐藏由 Rust 收尾，固定时所有权已经转移给固定窗口。
 */
import { useCallback, useEffect, useRef, useState } from 'react'
import {
  api,
  WispIpcError,
  type LlmError,
  type LongTextInfo,
  type ModelRef,
  type NoticeKind,
  type PinStatus,
  type StartAck,
  type StreamEvent,
} from '../../shared/ipc'
import type { StreamHandle } from '../../shared/stream/StreamView'

export type RunPhase = 'idle' | 'waiting' | 'streaming' | 'done' | 'error' | 'cancelled' | 'confirm'
export type StartEvent = Extract<StreamEvent, { type: 'start' }>
export type MetaEvent = Extract<StreamEvent, { type: 'meta' }>
export type DoneEvent = Extract<StreamEvent, { type: 'done' }>

export interface RunState {
  /** 每次 begin / seed 递增：StreamView 以它为 key 重新挂载 */
  key: number
  phase: RunPhase
  requestId: string | null
  start: StartEvent | null
  meta: MetaEvent | null
  done: DoneEvent | null
  error: LlmError | null
  notices: readonly NoticeKind[]
  /** 收到过非空白的 Delta（「思考耗尽」由前端判定） */
  hasText: boolean
  /** phase = confirm：长文本确认卡片要显示的估算（请求没有发出） */
  longText: LongTextInfo | null
}

/**
 * 打开请求：把事件回调交给 IPC，返回 StartAck（stream_attach 没有返回值）。`current()` 在这次 begin 被新的 begin
 * 取代（或视图卸载）后为 false——要先等别的结果再发请求的 opener（直接翻译先等原文转录）据此放弃，不再发出注定被
 * 立即取消的计费请求
 */
export type Opener = (
  onEvent: (e: StreamEvent) => void,
  current: () => boolean,
) => Promise<StartAck | void>

export interface Run {
  readonly state: RunState
  /** 已收到的全文 */
  text(): string
  /** 交给 StreamView 的 `ref` */
  sink(handle: StreamHandle | null): void
  /**
   * 开始一次请求（`requestId` 已知时——例如载荷里 Rust 在 t0 发出的请求——先记下，便于取消与固定）。
   * 返回的 Promise 在打开请求的调用返回后 resolve（stream_attach 时即回放已送达），失败也 resolve（错误进状态）
   */
  begin(open: Opener, requestId?: string | null): Promise<void>
  /** 取消进行中的请求（等 Rust 的终止事件；还没拿到 request_id 时，拿到后立即取消） */
  cancel(): void
  /** 用现成的文字直接置为完成（缓存的结果、固定窗口的初始内容；固定前已取消的结果为 cancelled） */
  seed(
    text: string,
    extra?: Partial<Omit<RunState, 'key' | 'phase'>>,
    phase?: 'done' | 'cancelled',
  ): void
  /** 回到 idle（仍在进行的请求先取消：被丢弃的结果不再继续计费） */
  clear(): void
}

const IDLE: RunState = {
  key: 0,
  phase: 'idle',
  requestId: null,
  start: null,
  meta: null,
  done: null,
  error: null,
  notices: [],
  hasText: false,
  longText: null,
}

const live = (p: RunPhase): boolean => p === 'waiting' || p === 'streaming'

/** 已经是 LlmError（例如另一个流的 Error 事件原样传下来）：有 code、message 与 retryable，不是 Error 实例 */
function isLlmError(value: unknown): value is LlmError {
  return (
    typeof value === 'object' &&
    value !== null &&
    !(value instanceof Error) &&
    typeof (value as Partial<LlmError>).code === 'string' &&
    typeof (value as Partial<LlmError>).message === 'string' &&
    typeof (value as Partial<LlmError>).retryable === 'boolean'
  )
}

/**
 * 失败原因 → LlmError：LlmError 原样返回（no_vision / auth / network 等卡片与按钮才对得上）；IPC 错误带 llm 时取 llm；
 * 命令本身被拒绝（没有 llm 字段）按 internal 显示通用卡片
 */
export function toLlmError(error: unknown): LlmError {
  if (isLlmError(error)) return error
  const ipc = WispIpcError.from('stream', error)
  if (ipc.llm) return ipc.llm
  return {
    code: 'internal',
    subcode: null,
    message: ipc.message,
    retryable: true,
    hint: null,
    status: null,
    url: null,
    retry_after_s: null,
    upstream_request_id: null,
  }
}

export function useRun(): Run {
  const [state, setState] = useState<RunState>(IDLE)
  const key = useRef(0)
  const text = useRef('')
  const hasText = useRef(false)
  const handle = useRef<StreamHandle | null>(null)
  /** 最新状态（事件回调与 cancel 里同步读取；随每次状态更新一起改，不在渲染期间写） */
  const current = useRef<RunState>(IDLE)
  const alive = useRef(true)
  /** 等 request_id 到了就取消的 key */
  const cancelOnAck = useRef(0)

  useEffect(() => {
    alive.current = true
    return () => {
      alive.current = false
    }
  }, [])

  const update = useCallback((k: number, patch: (s: RunState) => RunState) => {
    if (!alive.current || k !== key.current) return
    const next = patch(current.current)
    current.current = next
    setState(next)
  }, [])

  const onEvent = useCallback(
    (k: number, e: StreamEvent) => {
      if (!alive.current || k !== key.current) return
      switch (e.type) {
        case 'start':
          update(k, (s) => ({ ...s, start: e, requestId: e.request_id }))
          break
        case 'meta':
          update(k, (s) => ({ ...s, meta: e }))
          break
        case 'notice':
          update(k, (s) => ({ ...s, notices: [...s.notices, e.kind] }))
          break
        case 'phase':
          break
        case 'delta':
          text.current += e.text
          handle.current?.append(e.text)
          if (!hasText.current && e.text.trim() !== '') {
            hasText.current = true
            update(k, (s) => ({ ...s, phase: 'streaming', hasText: true }))
          }
          break
        case 'done':
          handle.current?.end()
          update(k, (s) => ({ ...s, phase: 'done', done: e }))
          break
        case 'error': {
          handle.current?.end()
          const { type: _type, ...error } = e
          update(k, (s) =>
            e.code === 'cancelled' ? { ...s, phase: 'cancelled' } : { ...s, phase: 'error', error },
          )
          break
        }
      }
    },
    [update],
  )

  const begin = useCallback(
    (open: Opener, requestId: string | null = null) => {
      const prev = current.current
      if (live(prev.phase)) {
        if (prev.requestId) void api.cancel(prev.requestId).catch(() => {})
        else cancelOnAck.current = prev.key
      }
      const k = ++key.current
      text.current = ''
      hasText.current = false
      handle.current = null
      const next: RunState = { ...IDLE, key: k, phase: 'waiting', requestId }
      current.current = next
      setState(next)
      return open(
        (e) => onEvent(k, e),
        () => alive.current && k === key.current,
      ).then(
        (ack) => {
          if (!ack) return
          // 长文本被 Rust 拦下：没有请求，等用户确认（「继续」时以 confirm_long 重新 begin）
          if (ack.long_text) {
            const info = ack.long_text
            update(k, (s) => ({ ...s, phase: 'confirm', requestId: null, longText: info }))
            return
          }
          // 被放弃的请求：拿到 id 立即取消
          if (cancelOnAck.current === k || k !== key.current) {
            void api.cancel(ack.request_id).catch(() => {})
            return
          }
          // 以实际发出的请求为准（接入失败后改为新发起时，id 会变）
          update(k, (s) => ({ ...s, requestId: ack.request_id }))
        },
        (error: unknown) => {
          update(k, (s) => (live(s.phase) ? { ...s, phase: 'error', error: toLlmError(error) } : s))
        },
      )
    },
    [onEvent, update],
  )

  const cancel = useCallback(() => {
    const s = current.current
    if (!live(s.phase)) return
    if (s.requestId) void api.cancel(s.requestId).catch(() => {})
    else cancelOnAck.current = s.key
  }, [])

  const seed = useCallback(
    (value: string, extra: Partial<RunState> = {}, phase: 'done' | 'cancelled' = 'done') => {
      const k = ++key.current
      text.current = value
      hasText.current = value.trim() !== ''
      handle.current = null
      const next: RunState = { ...IDLE, ...extra, key: k, phase, hasText: hasText.current }
      current.current = next
      setState(next)
    },
    [],
  )

  const clear = useCallback(() => {
    const prev = current.current
    if (live(prev.phase)) {
      if (prev.requestId) void api.cancel(prev.requestId).catch(() => {})
      else cancelOnAck.current = prev.key
    }
    const k = ++key.current
    text.current = ''
    hasText.current = false
    handle.current = null
    const next = { ...IDLE, key: k }
    current.current = next
    setState(next)
  }, [])

  const sink = useCallback((h: StreamHandle | null) => {
    handle.current = h
  }, [])

  return {
    state,
    text: () => text.current,
    sink,
    begin,
    cancel,
    seed,
    clear,
  }
}

/**
 * 固定交接的结构化状态行（PinState.status）：与 StatusLine 的各段一一对应；还没有模型名、也不是
 * 取消时为 null
 */
export function pinStatusOf(state: RunState): PinStatus | null {
  const cancelled = state.phase === 'cancelled'
  if (!state.start && !cancelled) return null
  const cached = state.done?.cached ?? false
  return {
    model_label: state.start?.model_label ?? '',
    elapsed_ms: state.done && !cached ? state.done.elapsed_ms : null,
    usage: state.done?.usage ?? null,
    cached,
    cancelled,
    truncated: state.done?.truncated ?? null,
  }
}

/** 由结构化状态行还原 seed 用的 Start / Done（固定窗口不再接入已结束的请求去回放） */
export function seedOf(status: PinStatus | null, model: ModelRef | null): Partial<RunState> {
  if (!status) return {}
  const start: StartEvent | null = status.model_label
    ? {
        type: 'start',
        request_id: '',
        provider_id: model?.provider_id ?? '',
        model_id: model?.model_id ?? '',
        model_label: status.model_label,
        upstream_id: null,
      }
    : null
  const done: DoneEvent | null = status.cancelled
    ? null
    : {
        type: 'done',
        // 命中缓存时没有耗时（状态行改为注明「来自缓存」）
        elapsed_ms: status.elapsed_ms ?? 0,
        usage: status.usage,
        truncated: status.truncated,
        cached: status.cached,
        no_text: false,
      }
  return { start, done }
}
