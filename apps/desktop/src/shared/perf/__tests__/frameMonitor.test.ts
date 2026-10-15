/**
 * 帧监视的掉帧判定：rAF 自身有 ±1 ms 的抖动，掉帧按「> 1.5 × 实测帧周期」判定，空闲面板不误报。
 */
import { afterEach, describe, expect, it, vi } from 'vitest'
import { framesWarn, resetFrameStats, snapshot, startFrameMonitor } from '../frameMonitor'

describe('frameMonitor', () => {
  afterEach(() => {
    vi.unstubAllGlobals()
    resetFrameStats()
  })

  function drive() {
    const callbacks: FrameRequestCallback[] = []
    vi.stubGlobal('requestAnimationFrame', (cb: FrameRequestCallback) => callbacks.push(cb))
    vi.stubGlobal('cancelAnimationFrame', () => undefined)
    let now = 1000
    return (dt: number) => {
      now += dt
      callbacks.shift()?.(now)
    }
  }

  it('60 Hz 下 16.6–17.2 ms 的抖动不算掉帧；> 1.5 帧才算；显示实测刷新率', () => {
    const frame = drive()
    const stop = startFrameMonitor()
    try {
      frame(0)
      for (let i = 0; i < 120; i++) frame(i % 2 ? 17.2 : 16.6)
      expect(snapshot().dropped).toBe(0)
      expect(snapshot().hz).toBe(60)
      frame(34)
      expect(snapshot().dropped).toBe(1)
      expect(framesWarn(snapshot())).toBe(false)
    } finally {
      stop()
    }
  })

  it('120 Hz：帧周期按实测取，> 12.5 ms 记为掉帧', () => {
    const frame = drive()
    const stop = startFrameMonitor()
    try {
      frame(0)
      for (let i = 0; i < 60; i++) frame(8.3)
      snapshot()
      frame(16.7)
      const s = snapshot()
      expect(s.hz).toBe(120)
      expect(s.dropped).toBe(1)
    } finally {
      stop()
    }
  })
})
