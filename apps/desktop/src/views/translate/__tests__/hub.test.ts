/**
 * 输入视图 → 翻译视图的流交接缓冲（hub.ts）：同步回放并继续转发；被接走并结束后释放；接走它的视图在流结束前就退订
 * （流式中被固定、视图在 Error{cancelled} 之前卸载）时成为孤儿，终止事件到达时立即释放；StrictMode 的重新订阅不释放。
 */
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import type { StreamEvent, TranslateReq } from '../../../shared/ipc/types'
import { bufferedCount, failLocally, startBuffered, subscribeBuffered } from '../hub'
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
afterEach(() => idleTransport())

const req: TranslateReq = {
  session_id: 1,
  text: 'hello',
  task: 'translate',
  style_id: 'natural',
  target_lang: null,
  model_ref: null,
  dict_mode: 'auto',
  bypass_cache: false,
  draft: null,
}

describe('流交接缓冲', () => {
  it('被接走并结束后释放（推迟到微任务）', async () => {
    const ack = await startBuffered(req)
    const seen: StreamEvent['type'][] = []
    const unsubscribe = subscribeBuffered(ack.request_id, (e) => seen.push(e.type))
    ipc.push(ack.request_id, start(ack.request_id), delta('a'), done())
    expect(seen).toEqual(['start', 'delta', 'done'])
    unsubscribe?.()
    expect(bufferedCount()).toBe(1)
    await flush()
    expect(bufferedCount()).toBe(0)
  })

  it('还没结束就退订（孤儿）：之后到达的终止事件随即释放整段缓冲，不一直留在 WebView 里', async () => {
    const ack = await startBuffered(req)
    const unsubscribe = subscribeBuffered(ack.request_id, () => undefined)
    ipc.push(ack.request_id, start(ack.request_id), delta('半'))
    unsubscribe?.()
    expect(bufferedCount()).toBe(1)
    ipc.push(ack.request_id, ...Array.from({ length: 50 }, () => delta('更多')))
    ipc.push(ack.request_id, done())
    expect(bufferedCount()).toBe(0)
  })

  it('孤儿在 Error{cancelled} 到达时同样释放', async () => {
    const ack = await startBuffered(req)
    subscribeBuffered(ack.request_id, () => undefined)?.()
    ipc.push(ack.request_id, error('cancelled'))
    expect(bufferedCount()).toBe(0)
  })

  it('StrictMode 的「退订 → 立即重新订阅」：终止事件到达时仍有订阅者，不释放；回放完整', async () => {
    const ack = await startBuffered(req)
    ipc.push(ack.request_id, start(ack.request_id), delta('a'))
    subscribeBuffered(ack.request_id, () => undefined)?.()
    const seen: StreamEvent['type'][] = []
    const unsubscribe = subscribeBuffered(ack.request_id, (e) => seen.push(e.type))
    ipc.push(ack.request_id, done())
    expect(seen).toEqual(['start', 'delta', 'done'])
    expect(bufferedCount()).toBe(1)
    unsubscribe?.()
    await flush()
    expect(bufferedCount()).toBe(0)
  })

  it('没有人接过的缓冲（还在等翻译视图挂载）不因为结束就被释放', async () => {
    const ack = await startBuffered(req)
    ipc.push(ack.request_id, start(ack.request_id), delta('缓存命中'), done())
    expect(bufferedCount()).toBe(1)
    const seen: StreamEvent['type'][] = []
    subscribeBuffered(ack.request_id, (e) => seen.push(e.type))
    expect(seen).toEqual(['start', 'delta', 'done'])
  })

  it('命令失败时的本地流：只有 Error，可被接上', () => {
    const id = failLocally({
      code: 'not_configured',
      subcode: null,
      message: 'x',
      retryable: false,
      hint: null,
      status: null,
      url: null,
      retry_after_s: null,
      upstream_request_id: null,
    })
    const seen: StreamEvent['type'][] = []
    subscribeBuffered(id, (e) => seen.push(e.type))
    expect(seen).toEqual(['error'])
  })
})
