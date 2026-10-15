/**
 * StreamView：
 * - 自动跟随到底部；用户以任何方式上滚（不只是滚轮：拖动滚动条、键盘）后不再被拉回底部，滚回底部恢复跟随；
 * - edit_split 的增量切分（EditSplitStream）与全量 `streamingSplit` 逐步一致（共用 fixture × 多种分块方式）。
 */
import { act, fireEvent, render, screen } from '@testing-library/react'
import { createRef } from 'react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import fixture from '../../../../../../crates/wisp-prompts/tests/fixtures/edit_split.json'
import { EditSplitStream, splitEditOutput, streamingSplit, type EditSplit } from '../editSplit'
import { StreamView, type StreamHandle } from '../StreamView'

let frames: FrameRequestCallback[] = []

function frame(ms = 1000 / 60): void {
  vi.advanceTimersByTime(ms)
  const t = performance.now()
  const pending = frames
  frames = []
  for (const cb of pending) cb(t)
}

beforeEach(() => {
  frames = []
  vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout', 'performance', 'Date'] })
  vi.stubGlobal('requestAnimationFrame', (cb: FrameRequestCallback) => frames.push(cb))
  vi.stubGlobal('cancelAnimationFrame', () => {})
})

afterEach(() => {
  vi.useRealTimers()
})

/** jsdom 没有布局：给滚动容器装上可控的 scrollHeight / clientHeight / scrollTop */
function fakeScroller(el: HTMLElement, clientHeight = 100) {
  const state = { scrollHeight: 300, top: 0 }
  Object.defineProperty(el, 'clientHeight', { configurable: true, get: () => clientHeight })
  Object.defineProperty(el, 'scrollHeight', { configurable: true, get: () => state.scrollHeight })
  Object.defineProperty(el, 'scrollTop', {
    configurable: true,
    get: () => state.top,
    set: (v: number) => {
      state.top = Math.max(0, Math.min(v, state.scrollHeight - clientHeight))
    },
  })
  return state
}

describe('StreamView 自动跟随', () => {
  function setup() {
    const ref = createRef<StreamHandle>()
    render(<StreamView ref={ref} format="plain" maxHeight={100} />)
    const el = screen.getByTestId('stream-view')
    const state = fakeScroller(el)
    const append = (text: string, grow = 40) => {
      act(() => ref.current?.append(text))
      state.scrollHeight += grow
      frame()
      // 浏览器会为程序设置的 scrollTop 派发 scroll 事件：模拟它
      fireEvent.scroll(el)
    }
    return { el, state, append }
  }

  it('流式中跟随到底部', () => {
    const { state, append } = setup()
    append('第一段')
    expect(state.top).toBe(state.scrollHeight - 100)
    append('第二段')
    expect(state.top).toBe(state.scrollHeight - 100)
  })

  it('拖动滚动条 / 键盘上滚（没有 wheel 事件）后不再拉回底部，滚回底部恢复跟随', () => {
    const { el, state, append } = setup()
    append('第一段')
    append('第二段')
    // 用户拖动滚动条往上：只有 scroll 事件
    state.top = 120
    fireEvent.scroll(el)
    append('第三段')
    append('第四段')
    expect(state.top).toBe(120)
    // 键盘 End / 拖回底部：恢复跟随
    state.top = state.scrollHeight - 100
    fireEvent.scroll(el)
    append('第五段')
    expect(state.top).toBe(state.scrollHeight - 100)
  })

  it('滚轮上滚立即停止跟随', () => {
    const { el, state, append } = setup()
    append('第一段')
    append('第二段')
    const before = state.top
    fireEvent.wheel(el, { deltaY: -10 })
    append('第三段')
    expect(state.top).toBe(before)
  })

  it('流式中途才给出 maxHeight（内容到达上限后改为内部滚动）：立即贴底，之后继续跟随', () => {
    const ref = createRef<StreamHandle>()
    const view = render(<StreamView ref={ref} format="plain" />)
    const el = screen.getByTestId('stream-view')
    const state = fakeScroller(el)
    act(() => ref.current?.append('第一段'))
    frame()
    // 随内容增高时不滚动
    expect(state.top).toBe(0)
    view.rerender(<StreamView ref={ref} format="plain" maxHeight={100} />)
    expect(state.top).toBe(state.scrollHeight - 100)
    fireEvent.scroll(el)
    act(() => ref.current?.append('第二段'))
    state.scrollHeight += 40
    frame()
    expect(state.top).toBe(state.scrollHeight - 100)
    // 反过来去掉 maxHeight：之后的写入不再设置 scrollTop
    view.rerender(<StreamView ref={ref} format="plain" />)
    const settled = state.top
    act(() => ref.current?.append('第三段'))
    state.scrollHeight += 40
    frame()
    expect(state.top).toBe(settled)
  })
})

/** 确定性的伪随机分块 */
function chunks(text: string, seed: number): string[] {
  let s = seed
  const rand = () => {
    s = (s * 1103515245 + 12345) % 2147483648
    return s / 2147483648
  }
  const out: string[] = []
  const chars = [...text]
  for (let i = 0; i < chars.length;) {
    const n = 1 + Math.floor(rand() * 6)
    out.push(chars.slice(i, i + n).join(''))
    i += n
  }
  return out
}

describe('EditSplitStream：增量检测与全量切分逐步一致', () => {
  const inputs = [
    ...fixture.cases.map((c) => c.input),
    '正文第一段\n\n---\n\n不是改动\n---\n**改动**：\n- a → b\n- c → d\n',
    'Body text\r\n---\r\nChanges:\r\n- x\r\n',
    '正文\n---\n改动：一\n---\n改动：二',
  ]
  for (const [i, input] of inputs.entries()) {
    for (const seed of [1, 7, 42]) {
      it(`#${i} 分块种子 ${seed}`, () => {
        const tr = new EditSplitStream()
        let prefix = ''
        const parts = seed === 1 ? [...input] : chunks(input, seed)
        for (const part of parts) {
          tr.push(part)
          prefix += part
          const expected = streamingSplit(prefix)
          const actual = tr.labeled ? { body: tr.body(), changes: tr.changes() } : null
          expect(actual, JSON.stringify(prefix)).toEqual(expected)
        }
      })
    }
  }
})

describe('StreamView · edit_split', () => {
  it('逐字流式：正文定格、改动区只追加，结束时与 splitEditOutput 一致', () => {
    const ref = createRef<StreamHandle>()
    const onSplit = vi.fn<(split: EditSplit) => void>()
    render(<StreamView ref={ref} format="edit_split" onSplit={onSplit} />)
    const text = '润色后的正文。\n第二行。\n---\n改动：\n- 「的」→「地」\n- 删除重复的「了」\n'
    for (const ch of text) {
      act(() => ref.current?.append(ch))
      frame()
    }
    act(() => ref.current?.end())
    const final = splitEditOutput(text)
    expect(screen.getByTestId('stream-view-body').textContent).toBe(final.body)
    expect(screen.getByTestId('stream-view-changes').textContent).toBe(final.changes)
    expect(onSplit).toHaveBeenCalledTimes(1)
  })

  it('改动区里又出现分隔线：按全文重算', () => {
    const ref = createRef<StreamHandle>()
    render(<StreamView ref={ref} format="edit_split" />)
    for (const part of ['正文\n---\n改动：一', '\n---\n', '改动：二']) {
      act(() => ref.current?.append(part))
      frame()
    }
    expect(screen.getByTestId('stream-view-changes').textContent).toBe('改动：二')
  })
})
