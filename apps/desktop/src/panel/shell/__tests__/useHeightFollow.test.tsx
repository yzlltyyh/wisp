/**
 * 高度跟随：三种方式下 `panel_resize` 的上报时机。
 * - native：每次内容高度变化都上报（Rust 逐帧弹簧跟随），卡片高度不由前端控制；
 * - css：变高先上报再让卡片长高；变矮先让卡片收缩，动画结束（onHeightSettled）才上报，窗口一次缩到位；
 * - opaque（Css 帧模式但窗口不能透明）：卡片铺满窗口、没有高度动画，变高变矮都立即上报。
 */
import { act, renderHook } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { setTransport, type Transport } from '../../../shared/ipc'
import { triggerResizeObservers } from '../../../test/stubs'
import { useHeightFollow, type HeightFollowMode } from '../useHeightFollow'

/** 各会话 ready 时报给 Rust 的高度（替身；真实值由 usePanelLifecycle 记下） */
const readyHeights = vi.hoisted(() => new Map<number, number>())
vi.mock('../usePanelLifecycle', () => ({
  readyHeightOf: (session: number) => readyHeights.get(session) ?? null,
}))

let calls: Array<[string, unknown]>
let frames: FrameRequestCallback[]

/** 执行排队中的 rAF 回调（模拟下一帧） */
function nextFrame() {
  const pending = frames
  frames = []
  for (const cb of pending) cb(performance.now())
}

function recordingTransport(): Transport {
  return {
    kind: 'mock',
    invoke: <T,>(cmd: string, args?: unknown) => {
      calls.push([cmd, args])
      return Promise.resolve(null as T)
    },
    openStream: () => Promise.reject(new Error('unused')),
    listen: () => Promise.resolve(() => {}),
    window: {} as Transport['window'],
    protocolUrl: (p) => p,
  }
}

/** 高度可控的内容元素 */
function content(initial: number) {
  const el = document.createElement('div')
  let height = initial
  Object.defineProperty(el, 'offsetHeight', { configurable: true, get: () => height })
  return {
    ref: { current: el },
    set(h: number) {
      height = h
      act(() => {
        triggerResizeObservers()
        nextFrame()
      })
    },
  }
}

function resizes(): number[] {
  return calls
    .filter(([cmd]) => cmd === 'panel_resize')
    .map(([, args]) => (args as { content_height: number }).content_height)
}

function follow(mode: HeightFollowMode, initial = 100) {
  const c = content(initial)
  const hook = renderHook(({ active }) => useHeightFollow(c.ref, 7, active, mode), {
    initialProps: { active: true },
  })
  return { c, hook }
}

beforeEach(() => {
  calls = []
  frames = []
  readyHeights.clear()
  setTransport(recordingTransport())
  // rAF 排队，由 nextFrame() 推进
  vi.stubGlobal('requestAnimationFrame', (cb: FrameRequestCallback) => frames.push(cb))
  vi.stubGlobal('cancelAnimationFrame', () => {})
})

afterEach(() => {
  // 不清成 null：迟到的调用为 null 时会触发浏览器 mock 的异步加载，覆盖下一个用例注入的传输层
  setTransport(recordingTransport())
})

describe('useHeightFollow · native', () => {
  it('每次内容高度变化都上报，卡片高度不由前端控制', () => {
    const { c, hook } = follow('native')
    c.set(140)
    c.set(140)
    c.set(90)
    expect(resizes()).toEqual([140, 90])
    expect(hook.result.current.cardHeight).toBeNull()
  })
})

describe('useHeightFollow · native 的浮层预留', () => {
  function springs(): Array<string | undefined> {
    return calls
      .filter(([cmd]) => cmd === 'panel_resize')
      .map(([, args]) => (args as { spring?: string }).spring)
  }

  it('预留引起的长高与释放之后的回收指定 snappy，其余缺省（gentle）；与 Css 帧卡片同一规则', () => {
    const c = content(100)
    const ref = { current: { reserved: 0, releaseUntil: 0 } }
    const hint = ref.current
    renderHook(() => useHeightFollow(c.ref, 7, true, 'native', Infinity, false, ref))
    c.set(140)
    // 菜单打开：卡片预留到 320
    hint.reserved = 320
    c.set(320)
    // 菜单关闭：回收窗口内的那次收缩
    hint.reserved = 0
    hint.releaseUntil = performance.now() + 1000
    c.set(140)
    // 回收窗口过了：内容变化照常 gentle
    hint.releaseUntil = 0
    c.set(180)
    expect(resizes()).toEqual([140, 320, 140, 180])
    expect(springs()).toEqual([undefined, 'snappy', 'snappy', undefined])
  })
})

describe('useHeightFollow · opaque（窗口不能透明，shadow_margin = 0）', () => {
  it('变矮立即上报（没有卡片高度动画可等，窗口不会停在最高的高度）', () => {
    const { c, hook } = follow('opaque', 200)
    c.set(260)
    c.set(120)
    c.set(90)
    expect(resizes()).toEqual([260, 120, 90])
    expect(hook.result.current.cardHeight).toBeNull()
    // 没有待上报的收缩：结束回调是空操作
    act(() => hook.result.current.onHeightSettled())
    expect(resizes()).toEqual([260, 120, 90])
  })
})

describe('useHeightFollow · css', () => {
  it('激活时卡片从 ready 时的测量值起跳，不上报', () => {
    readyHeights.set(7, 120)
    const { hook } = follow('css', 120)
    expect(hook.result.current.cardHeight).toBe(120)
    expect(resizes()).toEqual([])
  })

  it('ready 报的高度与激活时的内容高度不同（ready 报了估计值 / 之后内容变了）：立即补报，窗口不停在 ready 的高度', () => {
    readyHeights.set(7, 240)
    const { c, hook } = follow('css', 451)
    expect(hook.result.current.cardHeight).toBe(451)
    expect(resizes()).toEqual([451])
    // 之后的变化照常：变矮先收缩、结束后上报
    c.set(300)
    expect(resizes()).toEqual([451])
    act(() => hook.result.current.onHeightSettled())
    expect(resizes()).toEqual([451, 300])
  })

  it('ready 报的高度比内容高：同样立即补报（卡片已经是内容高度，没有收缩动画可等）', () => {
    readyHeights.set(7, 300)
    follow('css', 200)
    expect(resizes()).toEqual([200])
  })

  it('变高：立即上报并让卡片长高', () => {
    const { c, hook } = follow('css', 100)
    c.set(180)
    expect(resizes()).toEqual([180])
    expect(hook.result.current.cardHeight).toBe(180)
  })

  it('变矮：卡片先收缩，动画结束后才上报一次', () => {
    const { c, hook } = follow('css', 200)
    c.set(120)
    expect(hook.result.current.cardHeight).toBe(120)
    expect(resizes()).toEqual([])
    act(() => hook.result.current.onHeightSettled())
    expect(resizes()).toEqual([120])
    // 重复的结束回调不会再次上报
    act(() => hook.result.current.onHeightSettled())
    expect(resizes()).toEqual([120])
  })

  it('收缩途中改变目标：只在最终目标的动画结束后上报一次', () => {
    const { c, hook } = follow('css', 200)
    c.set(120)
    c.set(160)
    expect(resizes()).toEqual([])
    act(() => hook.result.current.onHeightSettled())
    expect(resizes()).toEqual([160])
    // 之后再变高：超过已上报的 160，立即上报
    c.set(260)
    expect(resizes()).toEqual([160, 260])
  })

  it('收缩途中又长回原高度以内：不上报收缩，窗口保持', () => {
    const { c, hook } = follow('css', 200)
    c.set(120)
    c.set(200)
    act(() => hook.result.current.onHeightSettled())
    expect(resizes()).toEqual([])
  })

  it('新会话不沿用上一次的卡片高度', () => {
    const c = content(150)
    const hook = renderHook(
      ({ session, active }) => useHeightFollow(c.ref, session, active, 'css'),
      { initialProps: { session: 1, active: true } },
    )
    expect(hook.result.current.cardHeight).toBe(150)
    hook.rerender({ session: 2, active: false })
    expect(hook.result.current.cardHeight).toBeNull()
  })
})
