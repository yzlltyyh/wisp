/**
 * Writer：每个 rAF 合并一次；span 合并；DOM 节点上限；隐藏时 setTimeout 兜底；
 * 同时在动画中的 span 不超过 8 个。
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { MAX_ANIMATING } from '../chunks'
import { StreamWriter } from '../writer'

let frames: FrameRequestCallback[] = []

/** 推进一帧：假时钟前进 ms，再以同一时钟执行排队的 rAF 回调（与浏览器里 rAF 时间戳和 performance.now 同源一致） */
function frame(ms = 1000 / 60): void {
  vi.advanceTimersByTime(ms)
  const t = performance.now()
  const pending = frames
  frames = []
  for (const cb of pending) cb(t)
}

/** 容器子树的全部节点数（元素 + 文本） */
function nodeCount(el: Node): number {
  let n = 0
  const walk = (node: Node) => {
    for (const c of node.childNodes) {
      n++
      walk(c)
    }
  }
  walk(el)
  return n
}

/** 创建时间在 120 ms 之内的 span 数（span 按时间顺序排列，用 data 属性无从得知，这里按创建时刻记录） */
const born = new WeakMap<Element, number>()
function animatingSpans(el: Element): number {
  const t = performance.now()
  let n = 0
  for (const s of el.querySelectorAll('span.chunk')) {
    if (!born.has(s)) born.set(s, t)
    if (t - (born.get(s) ?? t) < 120) n++
  }
  return n
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

describe('StreamWriter · plain', () => {
  it('同一帧内的多次 append 合并为一个 span', () => {
    const el = document.createElement('div')
    const w = new StreamWriter(el)
    w.append('你')
    w.append('好')
    w.append('，世界')
    expect(el.textContent).toBe('')
    frame()
    expect(el.querySelectorAll('span.chunk')).toHaveLength(1)
    expect(el.textContent).toBe('你好，世界')
    w.destroy()
  })

  it('200 token/s 流式 30 s：StreamView 子树的 DOM 节点数始终 ≤ 64，动画中的 span ≤ 8', () => {
    const el = document.createElement('div')
    const w = new StreamWriter(el)
    const tokensPerSec = 200
    const fps = 60
    let carry = 0
    let maxNodes = 0
    let maxAnimating = 0
    for (let f = 0; f < 30 * fps; f++) {
      carry += tokensPerSec / fps
      while (carry >= 1) {
        w.append('词元 ')
        carry--
      }
      frame(1000 / fps)
      maxNodes = Math.max(maxNodes, nodeCount(el))
      maxAnimating = Math.max(maxAnimating, animatingSpans(el))
    }
    expect(maxNodes).toBeLessThanOrEqual(64)
    expect(maxAnimating).toBeLessThanOrEqual(MAX_ANIMATING)
    w.end()
    expect(nodeCount(el)).toBe(1)
    expect(el.textContent).toBe('词元 '.repeat(6000))
  })

  it('120 Hz 下也只让 ≤ 8 个 span 同时动画（两个 span 至少间隔 15 ms）', () => {
    const el = document.createElement('div')
    const w = new StreamWriter(el)
    let max = 0
    for (let f = 0; f < 240; f++) {
      w.append('x')
      frame(1000 / 120)
      max = Math.max(max, animatingSpans(el))
    }
    expect(max).toBeLessThanOrEqual(MAX_ANIMATING)
    expect(nodeCount(el)).toBeLessThanOrEqual(64)
    w.end()
    expect(el.textContent).toBe('x'.repeat(240))
  })

  it('空闲 500 ms 合并已播完的 span', () => {
    const el = document.createElement('div')
    const w = new StreamWriter(el)
    for (let i = 0; i < 5; i++) {
      w.append(`段${i}`)
      frame(20)
    }
    expect(el.querySelectorAll('span.chunk').length).toBe(5)
    vi.advanceTimersByTime(600)
    expect(el.querySelectorAll('span.chunk').length).toBe(0)
    expect(el.childNodes).toHaveLength(1)
    w.destroy()
  })

  it('document 隐藏时用 setTimeout(50) 兜底', () => {
    const el = document.createElement('div')
    const hidden = vi.spyOn(document, 'hidden', 'get').mockReturnValue(true)
    const w = new StreamWriter(el)
    w.append('隐藏中也要写')
    expect(frames).toHaveLength(0)
    vi.advanceTimersByTime(50)
    expect(el.textContent).toBe('隐藏中也要写')
    hidden.mockRestore()
    w.destroy()
  })

  it('末尾光标：流式中 data-caret=true，end 后淡出再移除', () => {
    const el = document.createElement('div')
    const w = new StreamWriter(el)
    expect(el.dataset.caret).toBe('true')
    w.append('a')
    frame()
    w.end()
    expect(el.dataset.caret).toBe('ending')
    vi.advanceTimersByTime(200)
    expect(el.dataset.caret).toBeUndefined()
  })

  it('reset(text) 立即写入（不经 rAF，prepare 路径可同步测量）', () => {
    const el = document.createElement('div')
    const w = new StreamWriter(el)
    w.reset('初始内容')
    expect(el.textContent).toBe('初始内容')
    expect(el.querySelector('span')).toBeNull()
    w.destroy()
  })
})

describe('StreamWriter · mdlite', () => {
  it('每帧只更新尾块，结束后结构正确', () => {
    const el = document.createElement('div')
    const w = new StreamWriter(el, { format: 'mdlite' })
    const text = '**词汇**\n- a\n- b\n\n结束。'
    for (const ch of text) {
      w.append(ch)
      frame()
    }
    w.end()
    expect(el.querySelector('strong')?.textContent).toBe('词汇')
    expect(el.querySelectorAll('li')).toHaveLength(2)
    expect(el.querySelectorAll('p')).toHaveLength(2)
    expect(el.querySelectorAll('span.chunk')).toHaveLength(0)
  })
})
