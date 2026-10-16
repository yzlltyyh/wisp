/**
 * useOcrText：向 Rust 要排版结果（假传输层手动决定 text_transform 什么时候返回）、迟到的结果按 key 丢弃、连续切换时
 * 剪贴板只跟随最近一次、同一次结果同一种排版只请求一次、有编辑时不能切换、适用性来自芯片的检视。
 */
import { act, renderHook } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { setTransport, type Transport } from '../../../../shared/ipc'
import type { OcrLayout, TransformId } from '../../../../shared/ipc/types'
import type { RunView } from '../../pinState'
import type { RunPhase, RunState } from '../../useRun'
import { useOcrText } from '../useOcrText'

const RAW = '第一段\n\n第二段'
const TIGHT = '第一段\n第二段'
const ONE = '第一段第二段'

interface Pending {
  text: string
  transform: TransformId
  resolve(out: string): void
}
let pending: Pending[]

function transport(): Transport {
  return {
    kind: 'mock',
    invoke: <T,>(cmd: string, args?: Record<string, unknown>) => {
      if (cmd !== 'text_transform') return Promise.resolve(undefined as T)
      return new Promise<T>((resolve) => {
        pending.push({
          text: args?.text as string,
          transform: args?.transform as TransformId,
          resolve: (out) => resolve(out as T),
        })
      })
    },
    openStream: () => Promise.resolve(undefined as never),
    listen: () => Promise.resolve(() => {}),
    window: {} as Transport['window'],
    protocolUrl: (p) => p,
  }
}

const runOf = (key: number, raw: string, phase: RunPhase = 'done'): RunView => ({
  state: { key, phase, hasText: raw.trim() !== '' } as RunState,
  text: () => raw,
})

function mount(
  initial: OcrLayout,
  run = runOf(1, RAW),
  follow = vi.fn<(text: string) => Promise<boolean>>(async () => true),
) {
  const hook = renderHook(
    (p: { run: RunView }) => useOcrText(p.run, { initial, enabled: true, follow }),
    {
      initialProps: { run },
    },
  )
  return { ...hook, follow }
}

async function answer(i: number, out: string) {
  await act(async () => {
    pending[i]?.resolve(out)
    await Promise.resolve()
  })
}

beforeEach(() => {
  pending = []
  setTransport(transport())
})
afterEach(() => setTransport(transport()))

describe('useOcrText', () => {
  it('识别完成时按会话排版求一次结果；等待期间显示识别文本；迟到的上一次结果丢弃', async () => {
    const { result, rerender } = mount('remove_blank_lines')
    expect(pending.map((p) => [p.text, p.transform])).toEqual([[RAW, 'remove_blank_lines']])
    expect(result.current.display).toBe(RAW)
    // 还在识别时不求
    rerender({ run: runOf(2, 'X\n\nY', 'streaming') })
    expect(pending).toHaveLength(1)
    rerender({ run: runOf(2, 'X\n\nY') })
    expect(pending).toHaveLength(2)
    await answer(1, 'X\nY')
    expect(result.current.display).toBe('X\nY')
    await answer(0, TIGHT)
    expect(result.current.display).toBe('X\nY')
  })

  it('连续切换：剪贴板按先后执行、只跟随最近一次，播报最后那次', async () => {
    const { result, follow } = mount('keep')
    act(() => result.current.choose('remove_blank_lines'))
    act(() => result.current.choose('join_lines'))
    expect(pending.map((p) => p.transform)).toEqual(['remove_blank_lines', 'join_lines'])
    await answer(1, ONE)
    // 前一次切换的结果还没到：排在后面的也不先跟随
    expect(follow).not.toHaveBeenCalled()
    expect(result.current.display).toBe(ONE)
    await answer(0, TIGHT)
    await act(async () => {})
    expect(follow).toHaveBeenCalledTimes(1)
    expect(follow).toHaveBeenCalledWith(ONE)
    expect(result.current.notice).toEqual({ layout: 'join_lines', copied: true })
    expect(result.current.recent).toBe('join_lines')
  })

  it('同一次结果同一种排版只请求一次；切回原样不请求；有编辑时不能切换', async () => {
    const { result, follow } = mount('keep')
    act(() => result.current.toggle())
    await answer(0, TIGHT)
    expect(result.current.display).toBe(TIGHT)
    act(() => result.current.toggle())
    expect(result.current.display).toBe(RAW)
    await act(async () => {})
    act(() => result.current.toggle())
    await act(async () => {})
    expect(pending).toHaveLength(1)
    expect(follow.mock.calls.map(([t]) => t)).toEqual([TIGHT, RAW, TIGHT])

    act(() => result.current.setEdit('改过'))
    expect(result.current).toMatchObject({ edited: true, switchable: false })
    act(() => result.current.toggle())
    expect(result.current).toMatchObject({ session: 'remove_blank_lines', display: '改过' })
    expect(result.current.layers()).toEqual({
      raw: RAW,
      layout: 'remove_blank_lines',
      edit: '改过',
    })
    act(() => result.current.discard())
    expect(result.current).toMatchObject({ display: TIGHT, edited: false })
    expect(result.current.layers().edit).toBeNull()
  })

  it('适用性来自识别文本的检视（编辑后的检视不算）；不适用的整理方式切不过去', async () => {
    const { result } = mount('keep')
    act(() => result.current.inspected('别的文字', []))
    expect(result.current.can('join_lines')).toBe(true)
    act(() => result.current.inspected(RAW, ['remove_blank_lines']))
    expect(result.current.can('join_lines')).toBe(false)
    expect(result.current.can('remove_blank_lines')).toBe(true)
    act(() => result.current.choose('join_lines'))
    expect(pending).toHaveLength(0)
    expect(result.current.session).toBe('keep')
  })

  it('从固定交接还原：没有编辑时交接的文字就是排版结果，不再请求', () => {
    const { result } = mount('keep', runOf(5, RAW))
    act(() => result.current.restore(5, { layout: 'remove_blank_lines', edit: null, shown: TIGHT }))
    expect(result.current).toMatchObject({
      display: TIGHT,
      pending: false,
      session: 'remove_blank_lines',
    })
    expect(pending).toHaveLength(0)
  })
})
