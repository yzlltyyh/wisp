/**
 * 请求状态机（run.ts）：代次丢弃迟到事件、ack 之前就过期的请求补发取消（不留孤儿请求）、
 * 回放一次性写入（含 invoke 返回之后才到的事件）、宽限期变暗（提升译文同样）、思考耗尽判定、提升译文
 * 「无需改进」与前后切换、长文本确认期间只改参数、StreamView 重新挂载后整段补写。
 */
import { act } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { StreamEvent } from '../../../shared/ipc/types'
import type { StreamHandle } from '../../../shared/stream/StreamView'
import { startBuffered } from '../hub'
import { TranslateRun, toLlmError, type RunHost } from '../run'
import { WispIpcError } from '../../../shared/ipc'
import {
  delta,
  done,
  error,
  flush,
  idleTransport,
  installFakeIpc,
  start,
  type FakeIpc,
} from './harness'

let ipc: FakeIpc

beforeEach(() => {
  ipc = installFakeIpc()
})
afterEach(() => {
  vi.useRealTimers()
  idleTransport()
})

const host: RunHost = {
  sessionId: 7,
  formatOf: (task, styleId, dict) =>
    task === 'polish' || task === 'proofread'
      ? 'edit_split'
      : dict || styleId === 'learn'
        ? 'mdlite'
        : 'plain',
  guessDict: () => false,
}

function make(): TranslateRun {
  return new TranslateRun(host, {
    params: {
      text: 'hello world',
      task: 'translate',
      styleId: 'natural',
      target: null,
      model: null,
    },
    detected: 'en',
    target: 'zh-CN',
    dict: false,
  })
}

/** 记录写入的假 StreamView 句柄 */
function sink() {
  const log: string[] = []
  let text = ''
  const handle: StreamHandle = {
    append: (t) => {
      log.push(`append:${t}`)
      text += t
    },
    end: () => log.push('end'),
    reset: (t = '') => {
      log.push(`reset:${t}`)
      text = t
    },
    text: () => text,
    body: () => text,
  }
  return { handle, log, text: () => text }
}

describe('发起与接收', () => {
  it('第一个 Delta 换上新正文代次；文字先缓冲，绑定 StreamView 后写入；结束后补 end()', async () => {
    const run = make()
    run.start()
    await flush()
    expect(ipc.starts[0]).toMatchObject({
      session_id: 7,
      text: 'hello world',
      style_id: 'natural',
      target_lang: null,
      dict_mode: 'auto',
      bypass_cache: false,
    })
    expect(run.getSnapshot()).toMatchObject({ status: 'waiting', requestId: 'r1', textGen: 0 })
    ipc.push('r1', start('r1'), delta('你好'))
    const s = run.getSnapshot()
    expect(s.status).toBe('streaming')
    expect(s.textGen).toBe(s.gen)
    expect(s.hasText).toBe(true)
    expect(s.model?.label).toBe('GPT-5.4 mini')
    const v = sink()
    run.bindSink(s.textGen, v.handle)
    ipc.push('r1', delta('，世界'), done())
    expect(v.log).toEqual(['append:你好', 'append:，世界', 'end'])
    expect(run.getSnapshot()).toMatchObject({ status: 'done', elapsedMs: 1234, truncated: false })
  })

  it('换风格：取消旧请求，旧请求迟到的事件全部丢弃，只有最后一个在流式', async () => {
    const run = make()
    run.start()
    await flush()
    ipc.push('r1', start('r1'), delta('旧'))
    run.start({ styleId: 'technical' })
    await flush()
    run.start({ styleId: 'academic' })
    await flush()
    expect(ipc.names().filter((n) => n === 'cancel')).toHaveLength(2)
    expect(ipc.calls.filter(([c]) => c === 'cancel').map(([, a]) => a.request_id)).toEqual([
      'r1',
      'r2',
    ])
    ipc.push('r1', delta('迟到'), error('cancelled'))
    ipc.push('r2', error('cancelled'))
    expect(run.getSnapshot().status).toBe('waiting')
    ipc.push('r3', start('r3'), delta('新'), done())
    const s = run.getSnapshot()
    expect(s).toMatchObject({ status: 'done', params: { styleId: 'academic' } })
    const v = sink()
    run.bindSink(s.textGen, v.handle)
    expect(v.text()).toBe('新')
  })

  it('停止：保留已收到的部分、显示已取消，之后的事件（含 usage）都不再进来', async () => {
    const run = make()
    run.start()
    await flush()
    ipc.push('r1', start('r1'), delta('一半'))
    const v = sink()
    run.bindSink(run.getSnapshot().textGen, v.handle)
    run.stop()
    expect(ipc.calls.at(-1)).toEqual(['cancel', { request_id: 'r1' }])
    expect(run.getSnapshot().status).toBe('cancelled')
    ipc.push('r1', delta('不该出现'), done())
    expect(v.text()).toBe('一半')
    expect(run.getSnapshot()).toMatchObject({ status: 'cancelled', usage: null, elapsedMs: null })
    expect(v.log.at(-1)).toBe('end')
  })

  it('ack 之前连切两次（不等命令返回）：第一个请求在 ack 到达时被取消，只有最后一个在流式', async () => {
    const run = make()
    run.start()
    run.start({ styleId: 'technical' })
    run.start({ styleId: 'academic' })
    await flush()
    const cancelled = ipc.calls.filter(([c]) => c === 'cancel').map(([, a]) => a.request_id)
    expect(cancelled).toEqual(['r1', 'r2'])
    expect(run.getSnapshot()).toMatchObject({ requestId: 'r3', params: { styleId: 'academic' } })
    // 旧请求迟到的 Start（含 Start 先于 ack 的情形）不会再补发第二次取消
    ipc.push('r1', start('r1'), delta('迟到'))
    expect(ipc.calls.filter(([c]) => c === 'cancel')).toHaveLength(2)
    ipc.push('r3', start('r3'), delta('新'))
    expect(run.getSnapshot()).toMatchObject({ status: 'streaming', textGen: run.getSnapshot().gen })
  })

  it('Start 事件先于 StartAck 到达、之后过期：按 Start 里的 request_id 取消', async () => {
    let resolveAck: (v: unknown) => void = () => undefined
    let listener: ((e: StreamEvent) => void) | null = null
    ipc.handlers.translate_start = (args) => {
      listener = args.onEvent as (e: StreamEvent) => void
      return new Promise((r) => {
        resolveAck = r
      })
    }
    const run = make()
    run.start()
    run.stop()
    act(() => listener?.(start('slow')))
    expect(ipc.calls.filter(([c]) => c === 'cancel').map(([, a]) => a.request_id)).toEqual(['slow'])
    resolveAck({ request_id: 'slow', width: null })
    await flush()
    // ack 随后到达：不重复取消
    expect(ipc.calls.filter(([c]) => c === 'cancel')).toHaveLength(1)
    expect(run.getSnapshot().status).toBe('cancelled')
  })

  it('等待首字时（request_id 还不知道）停止：ack 到达后补发取消', async () => {
    const run = make()
    run.start()
    expect(run.getSnapshot().requestId).toBeNull()
    run.stop()
    expect(run.getSnapshot().status).toBe('cancelled')
    expect(ipc.names()).not.toContain('cancel')
    await flush()
    expect(ipc.calls.find(([c]) => c === 'cancel')?.[1]).toEqual({ request_id: 'r1' })
  })

  it('面板开始关闭时 ack 还没到：之后到达的 ack 立即取消', async () => {
    const run = make()
    run.start()
    run.cancelForClose()
    await flush()
    expect(ipc.calls.find(([c]) => c === 'cancel')?.[1]).toEqual({ request_id: 'r1' })
  })

  it('接上的请求过期时不取消（StrictMode 的重新挂载、新的视图实例接着显示它）', async () => {
    ipc.replay.set('rx', [start('rx'), delta('甲')])
    const run = make()
    void run.attach('rx')
    run.suspend()
    await run.attach('rx')
    ipc.push('rx', start('rx'))
    expect(ipc.names()).not.toContain('cancel')
  })

  it('长文本确认期间 configure 只改参数不发请求；确认后按新参数发起', async () => {
    const run = make()
    run.configure({ styleId: 'technical', task: 'polish', target: 'en' })
    expect(ipc.starts).toHaveLength(0)
    expect(run.getSnapshot()).toMatchObject({
      status: 'idle',
      target: 'en',
      params: { styleId: 'technical', task: 'polish' },
    })
    run.start()
    await flush()
    expect(ipc.starts[0]).toMatchObject({
      style_id: 'technical',
      task: 'polish',
      target_lang: 'en',
    })
  })

  it('面板关闭后冻结界面：之后到达的 Error{cancelled}、usage 一律丢弃，快照不变', async () => {
    const run = make()
    run.start()
    await flush()
    ipc.push('r1', start('r1'), delta('半'))
    const before = run.getSnapshot()
    const seen = vi.fn<() => void>()
    run.subscribe(seen)
    run.cancelForClose()
    expect(ipc.calls.filter(([c]) => c === 'cancel')).toEqual([['cancel', { request_id: 'r1' }]])
    ipc.push('r1', error('cancelled'), done())
    expect(seen).not.toHaveBeenCalled()
    expect(run.getSnapshot()).toMatchObject({ status: 'streaming', textGen: before.textGen })
    // 重复调用（✕ 之后外壳又进入 exiting）无害，不重复取消
    run.cancelForClose()
    expect(ipc.calls.filter(([c]) => c === 'cancel')).toHaveLength(1)
  })

  it('固定失败时面板已经在关闭：收回所有权并补发取消', async () => {
    const run = make()
    run.start()
    await flush()
    ipc.push('r1', start('r1'), delta('x'))
    run.release()
    run.cancelForClose()
    expect(ipc.names()).not.toContain('cancel')
    run.release(false)
    expect(ipc.names()).toContain('cancel')
  })

  it('已转给 Pinned 的请求，面板关闭时不再取消', async () => {
    const run = make()
    run.start()
    await flush()
    ipc.push('r1', start('r1'), delta('x'))
    run.release()
    run.cancelForClose()
    expect(ipc.names()).not.toContain('cancel')
    run.release(false)
    run.cancelForClose()
    expect(ipc.names()).toContain('cancel')
  })

  it('命令失败：以错误卡片结束；IpcError 带的 LlmError 优先', async () => {
    ipc.handlers.translate_start = () => {
      throw new WispIpcError('translate_start', {
        code: 'not_ready',
        message: 'no provider',
        llm: null,
      })
    }
    const run = make()
    run.start()
    await flush()
    expect(run.getSnapshot()).toMatchObject({ status: 'error', error: { code: 'not_configured' } })
    const llm = { ...toLlmError(new Error('x')), code: 'auth' as const }
    expect(toLlmError(new WispIpcError('x', { code: 'llm', message: '', llm })).code).toBe('auth')
    expect(toLlmError(new Error('boom'))).toMatchObject({ code: 'internal', message: 'boom' })
  })
})

describe('结束状态', () => {
  it('截断且没有任何非空白 Delta → 思考耗尽；有文字的截断只是截断', async () => {
    const run = make()
    run.start()
    await flush()
    ipc.push('r1', start('r1'), delta('  \n'), done({ truncated: 'max_tokens' }))
    expect(run.getSnapshot()).toMatchObject({ thinkingExhausted: true, truncated: true })
    run.start()
    await flush()
    ipc.push('r2', start('r2'), delta('有字'), done({ truncated: 'max_tokens' }))
    expect(run.getSnapshot()).toMatchObject({ thinkingExhausted: false, truncated: true })
  })

  it('提升译文：Phase{reviewing} → Done 没有 Delta 时保留原译文，提示无需改进', async () => {
    const run = make()
    run.start()
    await flush()
    ipc.push('r1', start('r1'), delta('原译文'), done())
    const before = run.getSnapshot().textGen
    run.start({ task: 'refine' }, { draft: '原译文' })
    await flush()
    expect(ipc.starts[1]).toMatchObject({ task: 'refine', draft: '原译文' })
    ipc.push('r2', start('r2'), { type: 'phase', phase: 'reviewing' })
    expect(run.getSnapshot().phase).toBe('reviewing')
    expect(run.getSnapshot().dimmed).toBe(false)
    ipc.push('r2', done())
    expect(run.getSnapshot()).toMatchObject({
      status: 'done',
      noImprovement: true,
      textGen: before,
    })
  })

  it('提升译文以截断结束且没有任何 Delta：思考耗尽，保留原译文，不同时报「无需改进」', async () => {
    const run = make()
    run.start()
    await flush()
    ipc.push('r1', start('r1'), delta('原译文'), done())
    const before = run.getSnapshot().textGen
    run.start({ task: 'refine' }, { draft: '原译文' })
    await flush()
    ipc.push('r2', start('r2'), done({ truncated: 'max_tokens' }))
    expect(run.getSnapshot()).toMatchObject({
      status: 'done',
      thinkingExhausted: true,
      noImprovement: false,
      refined: null,
      textGen: before,
      hasText: true,
    })
    expect(run.rawText()).toBe('原译文')
  })

  it('提升译文：反思阶段宽限期后旧译文变暗；改进完成后标记「已提升」，可以在原译文与提升后的译文之间切换', async () => {
    vi.useFakeTimers()
    const run = make()
    run.start()
    await vi.advanceTimersByTimeAsync(0)
    ipc.push('r1', start('r1'), delta('原译文'), done({ elapsed_ms: 900 }))
    run.start({ task: 'refine' }, { draft: '原译文' })
    await vi.advanceTimersByTimeAsync(0)
    ipc.push('r2', start('r2'), { type: 'phase', phase: 'reviewing' })
    await vi.advanceTimersByTimeAsync(100)
    expect(run.getSnapshot().dimmed).toBe(true)
    ipc.push(
      'r2',
      { type: 'phase', phase: 'improving' },
      delta('改进后'),
      done({ elapsed_ms: 2000 }),
    )
    expect(run.getSnapshot()).toMatchObject({ refined: 'improved', dimmed: false, elapsedMs: 2000 })
    const v = sink()
    run.toggleRefined()
    expect(run.getSnapshot()).toMatchObject({
      refined: 'original',
      elapsedMs: 900,
      textStatic: true,
    })
    run.bindSink(run.getSnapshot().textGen, v.handle)
    expect(v.log).toEqual(['reset:原译文', 'end'])
    run.toggleRefined()
    const w = sink()
    run.bindSink(run.getSnapshot().textGen, w.handle)
    expect(w.text()).toBe('改进后')
    expect(run.getSnapshot()).toMatchObject({ refined: 'improved', elapsedMs: 2000 })
  })

  it('没有任何 Delta 的结束（非 refine）：清空旧正文，不让上一个风格的译文冒充这次的结果', async () => {
    const run = make()
    run.start()
    await flush()
    ipc.push('r1', start('r1'), delta('旧'), done())
    run.start({ styleId: 'casual' })
    await flush()
    ipc.push('r2', start('r2'), done())
    const s = run.getSnapshot()
    expect(s.textGen).toBe(s.gen)
    expect(s.hasText).toBe(false)
  })

  it('Error 在流开始前（Start → Error）与流中途都以错误结束；cancelled 只显示已取消', async () => {
    const run = make()
    run.start()
    await flush()
    ipc.push('r1', start('r1'), error('rate_limited', { retry_after_s: 5 }))
    expect(run.getSnapshot()).toMatchObject({
      status: 'error',
      error: { code: 'rate_limited', retry_after_s: 5 },
    })
    run.start()
    await flush()
    ipc.push('r2', start('r2'), error('cancelled'))
    expect(run.getSnapshot()).toMatchObject({ status: 'cancelled', error: null })
  })
})

describe('风格切换的变暗', () => {
  it('宽限期过后还没有新内容才变暗；新内容到达后恢复', async () => {
    vi.useFakeTimers()
    const run = make()
    run.start()
    await vi.advanceTimersByTimeAsync(0)
    ipc.push('r1', start('r1'), delta('旧'), done())
    run.start({ styleId: 'technical' })
    expect(run.getSnapshot().dimmed).toBe(false)
    await vi.advanceTimersByTimeAsync(100)
    expect(run.getSnapshot().dimmed).toBe(true)
    ipc.push('r2', start('r2'), delta('新'))
    expect(run.getSnapshot().dimmed).toBe(false)
  })

  it('宽限期内到达（命中缓存）：从不变暗', async () => {
    vi.useFakeTimers()
    const run = make()
    run.start()
    await vi.advanceTimersByTimeAsync(0)
    ipc.push('r1', start('r1'), delta('旧'), done())
    run.start({ styleId: 'technical' })
    await vi.advanceTimersByTimeAsync(0)
    ipc.push(
      'r2',
      start('r2'),
      { type: 'notice', kind: 'cache_hit' },
      delta('缓存'),
      done({ cached: true }),
    )
    await vi.advanceTimersByTimeAsync(200)
    expect(run.getSnapshot()).toMatchObject({ dimmed: false, cached: true, status: 'done' })
  })
})

describe('接上已有请求（回放）', () => {
  it('stream_attach 回放的文字缓冲到命令返回，再一次性无动画写入（Pinned 首帧可以同步测量）', async () => {
    ipc.replay.set('rx', [
      start('rx'),
      {
        type: 'meta',
        detected_source_lang: 'en',
        target_lang: 'zh-CN',
        dict_mode: true,
        low_confidence: true,
      },
      delta('甲'),
      delta('乙'),
    ])
    const run = make()
    const v = sink()
    const attached = run.attach('rx')
    // 回放期间绑定的句柄不会被写入
    run.bindSink(run.getSnapshot().textGen, v.handle)
    expect(v.log).toEqual([])
    await attached
    run.bindSink(run.getSnapshot().textGen, v.handle)
    expect(v.log).toEqual(['reset:甲乙'])
    expect(run.getSnapshot()).toMatchObject({
      dict: true,
      lowConfidence: true,
      textDict: true,
      textFormat: 'mdlite',
    })
    ipc.push('rx', delta('丙'))
    expect(v.log.at(-1)).toBe('append:丙')
  })

  it('invoke 返回之后才到的回放事件（大消息异步投递）：等到第一个事件再结束回放，同样一次性写入', async () => {
    ipc.handlers.stream_attach = (args) => {
      const on = args.onEvent as (e: StreamEvent) => void
      setTimeout(() => {
        on(start('big'))
        on(delta('很长的缓存译文'))
      }, 20)
      return undefined
    }
    const run = make()
    const attached = run.attach('big')
    await new Promise((r) => setTimeout(r, 5))
    const v = sink()
    run.bindSink(run.getSnapshot().textGen, v.handle)
    expect(v.log).toEqual([])
    await attached
    run.bindSink(run.getSnapshot().textGen, v.handle)
    expect(v.log).toEqual(['reset:很长的缓存译文'])
  })

  it('空日志（还在等首字）：最多等一小段时间就结束回放，之后的事件照常实时写入', async () => {
    ipc.handlers.stream_attach = (args) => {
      ipc.listeners.set('quiet', [args.onEvent as (e: StreamEvent) => void])
      return undefined
    }
    const run = make()
    const t0 = performance.now()
    await run.attach('quiet')
    expect(performance.now() - t0).toBeLessThan(1000)
    ipc.push('quiet', start('quiet'), delta('首字'))
    const v = sink()
    run.bindSink(run.getSnapshot().textGen, v.handle)
    expect(v.log).toEqual(['append:首字'])
  })

  it('词典式：显示的文字经过整理，复制 / 固定用的原文不含整理标记', async () => {
    const run = new TranslateRun(host, {
      params: {
        text: 'serendipity',
        task: 'translate',
        styleId: 'natural',
        target: null,
        model: null,
      },
      detected: 'en',
      target: 'zh-CN',
      dict: true,
    })
    run.start()
    await flush()
    const raw = '**serendipity** /x/ n.\n1. 机缘\n例：pure serendipity. → 纯属机缘。'
    ipc.push(
      'r1',
      start('r1'),
      {
        type: 'meta',
        detected_source_lang: 'en',
        target_lang: 'zh-CN',
        dict_mode: true,
        low_confidence: false,
      },
      delta(raw),
      done(),
    )
    const v = sink()
    run.bindSink(run.getSnapshot().textGen, v.handle)
    expect(v.text()).toContain('`n.`')
    expect(v.text()).toContain('**serendipity**.')
    expect(run.rawText()).toBe(raw)
  })

  it('输入视图交接的请求：从缓冲同步回放，不再经 IPC stream_attach', async () => {
    const ack = await startBuffered({
      session_id: 1,
      text: 't',
      task: 'translate',
      style_id: 'natural',
      target_lang: null,
      model_ref: null,
      dict_mode: 'auto',
      bypass_cache: false,
      draft: null,
    })
    ipc.push(ack.request_id, start(ack.request_id), delta('早到'))
    const run = make()
    await run.attach(ack.request_id)
    expect(ipc.names()).not.toContain('stream_attach')
    const v = sink()
    run.bindSink(run.getSnapshot().textGen, v.handle)
    expect(v.log).toEqual(['reset:早到'])
    ipc.push(ack.request_id, delta('晚到'))
    expect(v.log.at(-1)).toBe('append:晚到')
  })

  it('StreamView 重新挂载（StrictMode / 重建）后整段补写，不丢已写的文字', async () => {
    const run = make()
    run.start()
    await flush()
    ipc.push('r1', start('r1'), delta('Wisp'))
    const a = sink()
    run.bindSink(run.getSnapshot().textGen, a.handle)
    run.bindSink(run.getSnapshot().textGen, null)
    const b = sink()
    run.bindSink(run.getSnapshot().textGen, b.handle)
    expect(b.log).toEqual(['reset:Wisp'])
    ipc.push('r1', delta(' 从不打扰你'))
    expect(b.text()).toBe('Wisp 从不打扰你')
  })

  it('静态结果（PinState.result_text）：同步写入、已结束，结构化状态行逐项还原', () => {
    const run = make()
    run.showStatic(
      '结果',
      {
        model_label: 'GPT',
        elapsed_ms: 1000,
        usage: { input_tokens: 12, output_tokens: 34, reasoning_tokens: null, total_tokens: 46 },
        cached: false,
        cancelled: false,
        truncated: null,
      },
      { provider_id: 'p', model_id: 'gpt' },
    )
    const v = sink()
    run.bindSink(run.getSnapshot().textGen, v.handle)
    expect(v.log).toEqual(['reset:结果', 'end'])
    expect(run.getSnapshot()).toMatchObject({
      status: 'done',
      model: { providerId: 'p', modelId: 'gpt', label: 'GPT' },
      elapsedMs: 1000,
      usage: { input_tokens: 12, output_tokens: 34, reasoning_tokens: null, total_tokens: 46 },
      cached: false,
      truncated: false,
      hasText: true,
    })
  })

  it('静态结果：已取消 → cancelled；截断且没有文字 → 思考耗尽；没有状态行时只写正文', () => {
    const cancelled = make()
    cancelled.showStatic('半截', {
      model_label: 'GPT',
      elapsed_ms: null,
      usage: null,
      cached: false,
      cancelled: true,
      truncated: null,
    })
    expect(cancelled.getSnapshot().status).toBe('cancelled')
    const exhausted = make()
    exhausted.showStatic('', {
      model_label: 'GPT',
      elapsed_ms: 900,
      usage: null,
      cached: false,
      cancelled: false,
      truncated: 'max_tokens',
    })
    expect(exhausted.getSnapshot()).toMatchObject({ truncated: true, thinkingExhausted: true })
    const plain = make()
    plain.showStatic('结果')
    expect(plain.getSnapshot()).toMatchObject({ status: 'done', model: null, elapsedMs: null })
  })
})
