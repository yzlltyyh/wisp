/**
 * mock 引擎测试：流的回放语义、默认命令、以及面板外壳与模拟窗口的端到端 prepare → ready 时序。
 */
import { act, render, screen, waitFor } from '@testing-library/react'
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest'
import { PanelApp } from '../../../panel/shell/PanelApp'
import { usePanelStore } from '../../../panel/shell/panelStore'
import * as api from '../commands'
import { getTransport, whenReady } from '../runtime'
import type { StreamEvent } from '../types'
import { MockStreams, streamRate, textStream } from '../mock/streams'
import { mergePatch } from '../mock/defaults'

beforeAll(async () => {
  await new Promise<void>((resolve) => whenReady(resolve))
})

afterEach(() => {
  vi.useRealTimers()
})

const handle = () => {
  const h = window.__WISP_MOCK__
  if (!h) throw new Error('mock 未安装')
  return h
}

describe('MockStreams', () => {
  it('attach 先回放已有日志，再推送实时事件，恰好一个终止事件', async () => {
    vi.useFakeTimers()
    const streams = new MockStreams()
    streams.start(
      'r-a',
      textStream('你好，世界。Hello world!', { firstTokenMs: 10, intervalMs: 5 }),
    )
    await vi.advanceTimersByTimeAsync(12)
    const events: StreamEvent[] = []
    streams.attach('r-a', (e) => events.push(e))
    expect(events[0]).toMatchObject({ type: 'start', request_id: 'r-a' })
    await vi.advanceTimersByTimeAsync(500)
    const terminal = events.filter((e) => e.type === 'done' || e.type === 'error')
    expect(terminal).toHaveLength(1)
    expect(events.at(-1)?.type).toBe('done')
    const text = events.flatMap((e) => (e.type === 'delta' ? [e.text] : [])).join('')
    expect(text).toBe('你好，世界。Hello world!')
    expect(streams.active).toBe(0)
  })

  it('cancel 以 Error{cancelled} 结束', () => {
    vi.useFakeTimers()
    const streams = new MockStreams()
    const events: StreamEvent[] = []
    streams.start('r-b', textStream('long text', { firstTokenMs: 1000 }))
    streams.attach('r-b', (e) => events.push(e))
    streams.cancel('r-b')
    expect(events.at(-1)).toMatchObject({ type: 'error', code: 'cancelled' })
  })

  it('streamRate 输出指定数量的 token', () => {
    const script = streamRate(200, 4096)
    const deltas = script.filter((s) => 'type' in s && s.type === 'delta')
    expect(deltas.length).toBeGreaterThan(200)
    expect(script.at(-1)).toMatchObject({ type: 'done' })
  })

  it('未知 request_id 的 attach 报 not_found', () => {
    let error: unknown = null
    try {
      new MockStreams().attach('r-x', () => {})
    } catch (e) {
      error = e
    }
    expect(error).toMatchObject({ code: 'not_found' })
  })
})

describe('mock 默认命令', () => {
  it('覆盖全部命令', () => {
    const registered = new Set(handle().router.registered())
    expect(api.COMMAND_NAMES.filter((c) => !registered.has(c))).toEqual([])
  })

  it('settings_patch 按 JSON Merge Patch 合并并广播 settings:changed', async () => {
    const seen = vi.fn<(s: unknown) => void>()
    const un = await getTransport().listen('settings:changed', seen)
    const snap = await api.settingsPatch({ appearance: { font_size: 16 }, defaults: { ocr: null } })
    expect(snap.settings.appearance.font_size).toBe(16)
    await waitFor(() => expect(seen).toHaveBeenCalledTimes(1))
    un()
  })

  it('JSON Merge Patch：null 删除、数组整体替换', () => {
    expect(mergePatch({ a: 1, b: { c: 2, d: 3 }, e: [1, 2] }, { b: { c: null }, e: [3] })).toEqual({
      a: 1,
      b: { d: 3 },
      e: [3],
    })
  })

  it('ocr_recopy 跟随自动复制的开关；默认排版为原样', async () => {
    const snap = await api.settingsGet()
    expect(snap.settings.ocr.layout).toBe('keep')
    await expect(api.ocrRecopy('cap-1', 'r-1', '第一段\n第二段')).resolves.toBe(true)
    await api.settingsPatch({ ocr: { auto_copy: false } })
    await expect(api.ocrRecopy('cap-1', 'r-1', '第一段\n第二段')).resolves.toBe(false)
    await api.settingsPatch({ ocr: { auto_copy: true } })
  })

  it('translate_start 立即开始流并经通道推送', async () => {
    const events: StreamEvent[] = []
    const ack = await api.translateStart(
      {
        session_id: 1,
        text: 'hi',
        task: 'translate',
        style_id: 'natural',
        target_lang: null,
        model_ref: null,
        dict_mode: 'auto',
        bypass_cache: false,
        draft: null,
      },
      (e) => events.push(e),
    )
    expect(ack.request_id).toMatch(/^r-/)
    await waitFor(() => expect(events.at(-1)?.type).toBe('done'), { timeout: 5000 })
  })

  it('命令失败抛 WispIpcError', async () => {
    await expect(api.clipboardGet(99_999)).resolves.toBeNull()
    await expect(api.clipboardPin(99_999, true)).rejects.toMatchObject({
      name: 'WispIpcError',
      code: 'not_found',
    })
  })
})

describe('面板外壳 × 模拟窗口', () => {
  it('prepare → 同步渲染并测量 → panel_ready → 进入；hide_request → 退出后 panel_hide', async () => {
    const h = handle()
    const readySpy = vi.fn<(args: unknown) => void>()
    const hideSpy = vi.fn<(args: unknown) => void>()
    const restoreReady = h.router.handle('panel_ready', (args) => {
      readySpy(args)
      return h.panel ? h.panel.ready(args.session_id, args.content_height) : null
    })
    const restoreHide = h.router.handle('panel_hide', (args) => {
      hideSpy(args)
      h.panel?.hide(args.session_id, args.reason)
    })

    render(<PanelApp />)
    let sessionId = 0
    act(() => {
      sessionId = h.ctx.preparePanel(h.fixtures.payload.translate())
    })
    await screen.findByTestId('view-translate')
    await waitFor(() => expect(readySpy).toHaveBeenCalledTimes(1))
    expect(readySpy.mock.calls[0]?.[0]).toMatchObject({
      session_id: sessionId,
      content_height: expect.any(Number),
    })
    // 进入动画在负载下可能已经播完（entering → shown），两者都表示 ready 之后已显示
    await waitFor(() => expect(['entering', 'shown']).toContain(usePanelStore.getState().phase))
    expect(['entering', 'shown']).toContain(screen.getByTestId('panel-card').dataset.phase)
    expect(document.getElementById('root')?.dataset.mockWindow).toBe('shown')

    act(() => h.ctx.requestHide('escape'))
    await waitFor(() => expect(hideSpy).toHaveBeenCalledTimes(1))
    expect(hideSpy.mock.calls[0]?.[0]).toEqual({ session_id: sessionId, reason: 'escape' })
    expect(document.getElementById('root')?.dataset.mockWindow).toBe('hidden')
    restoreReady()
    restoreHide()
  })
})
