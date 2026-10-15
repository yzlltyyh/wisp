/**
 * Overlay：框选、放大镜、空格平移、窗口高亮、Esc / Enter / ⌘A 的 DOM 级测试。
 *
 * 用假传输层驱动真实的 mountOverlay：命令调用按顺序记录，`fetch` 返回带尺寸头的 RGBA 帧；交互用合成的
 * PointerEvent / KeyboardEvent（与真实输入走同一套处理函数）。WAAPI（Element.animate）在 jsdom 里没有，这里按需 stub。
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'
import { setTransport } from '../../shared/ipc/runtime'
import type { Transport } from '../../shared/ipc/transport'
import type { EventMap, OverlayInit, Rect } from '../../shared/ipc/types'
import { mountOverlay } from '../index'
import {
  CLICK_SLOP,
  MAG_LABEL,
  MAG_OFFSET,
  MAG_SIZE,
  UNIT,
  bandTransforms,
  clampRect,
  edgeTransforms,
  farCorner,
  hairline,
  hitWindow,
  magnifierAt,
  rectOf,
  shiftRect,
  snapRect,
} from '../geometry'

type Listener = (payload: unknown) => void
let listeners: Map<string, Set<Listener>>
let calls: Array<[string, Record<string, unknown>]>
let responses: Record<string, (args: Record<string, unknown>) => unknown>
let fetched: string[]
let animations: Array<{ el: Element; keyframes: Keyframe[]; options: KeyframeAnimationOptions }>

const W = 1000
const H = 600
const WINDOWS: Rect[] = [
  { x: 600, y: 300, width: 300, height: 200 }, // 最上层
  { x: 100, y: 100, width: 600, height: 400 },
]

function init(extra: Partial<OverlayInit> = {}): OverlayInit {
  return {
    session_id: 7,
    monitor: {
      id: '\\\\.\\DISPLAY1',
      name: 'DISPLAY1',
      physical_origin: { x: 0, y: 0 },
      physical_size: { width: W * 2, height: H * 2 },
      scale_factor: 2,
      frame: { x: 0, y: 0, width: W, height: H },
      work_area: { x: 0, y: 0, width: W, height: H - 40 },
      is_primary: true,
    },
    image_url: 'capture/7/5c5c2e5c444953504c415931/full',
    image_size: { width: W * 2, height: H * 2 },
    windows: WINDOWS,
    mode: 'ocr',
    window_detect: true,
    fly_animation: true,
    reduced_motion: false,
    frozen_native: false,
    half_url: 'capture/7/5c5c2e5c444953504c415931/half',
    ...extra,
  }
}

function fakeTransport(): Transport {
  return {
    kind: 'mock',
    invoke: <T>(cmd: string, args?: Record<string, unknown>) => {
      calls.push([cmd, args ?? {}])
      const r = responses[cmd]
      try {
        return Promise.resolve((r ? r(args ?? {}) : undefined) as T)
      } catch (error) {
        return Promise.reject(error as Error)
      }
    },
    openStream: () => Promise.reject(new Error('unused')),
    listen: (event, cb) => {
      calls.push(['listen', { event }])
      const set = listeners.get(event) ?? new Set()
      set.add(cb as Listener)
      listeners.set(event, set)
      return Promise.resolve(() => set.delete(cb as Listener))
    },
    window: {} as Transport['window'],
    protocolUrl: (p) => `http://wisp.localhost/${p}`,
  }
}

function emit<K extends keyof EventMap>(event: K, payload: EventMap[K]): void {
  for (const cb of listeners.get(event) ?? []) cb(payload)
}

/** RGBA 帧（尺寸由路径决定：half = 一半；`region/<x>/<y>/<w>/<h>` = 那一块，带第 5 段时按面积上限等比缩小） */
function stubFetch(fail: (url: string) => boolean = () => false) {
  vi.stubGlobal('fetch', async (url: string) => {
    fetched.push(url)
    if (fail(url)) return new Response(null, { status: 404 })
    const region = /\/region\/(\d+)\/(\d+)\/(\d+)\/(\d+)(?:\/(\d+))?$/.exec(url)
    if (region) {
      const k = region[5]
        ? Math.min(1, Math.sqrt(Number(region[5]) / (Number(region[3]) * Number(region[4]))))
        : 1
      const rw = Math.max(1, Math.floor(Number(region[3]) * k))
      const rh = Math.max(1, Math.floor(Number(region[4]) * k))
      return new Response(new Uint8Array(rw * rh * 4), {
        headers: { 'x-wisp-width': String(rw), 'x-wisp-height': String(rh) },
      })
    }
    const half = url.endsWith('/half')
    const w = half ? W : W * 2
    const h = half ? H : H * 2
    return new Response(new Uint8Array(w * h * 4), {
      headers: { 'x-wisp-width': String(w), 'x-wisp-height': String(h) },
    })
  })
}

const flush = async (n = 6) => {
  for (let i = 0; i < n; i++) {
    // oxlint-disable-next-line no-await-in-loop -- 逐轮清空微任务与定时器
    await new Promise((r) => setTimeout(r, 0))
  }
}
const frame = () => new Promise((r) => requestAnimationFrame(() => r(null)))

let root: HTMLElement
let dispose: (() => void) | null = null

/**
 * 指针事件（与真实引擎同形：`button` 是这次变化的键，`buttons` 是此刻按着的全部键）。不给 buttons 时按惯例推：
 * pointerdown / pointermove 的左键拖拽为 1，pointerup 为 0
 */
function pointer(type: string, x: number, y: number, button = 0, buttons?: number) {
  const held = buttons ?? (type === 'pointerup' ? 0 : type === 'pointerdown' ? 1 << button : 0)
  root.dispatchEvent(
    new MouseEvent(type, {
      clientX: x,
      clientY: y,
      button,
      buttons: held,
      bubbles: true,
      cancelable: true,
    }),
  )
}
function key(code: string, extra: KeyboardEventInit = {}, type: 'keydown' | 'keyup' = 'keydown') {
  const k = code === 'Space' ? ' ' : code.startsWith('Key') ? code.slice(3).toLowerCase() : code
  window.dispatchEvent(new KeyboardEvent(type, { code, key: k, bubbles: true, ...extra }))
}
const sel = () => root.querySelector<HTMLElement>('.ov-sel')
/** 选区 / 放大镜是否隐藏（visibility：data-off） */
const off = (node: HTMLElement | null | undefined) => node?.hasAttribute('data-off')
/** 选区边框强调色的四段（上、下、左、右）、尺寸标签与暗化层挖洞的四块 */
const edge = (i: number) => root.querySelectorAll<HTMLElement>('.ov-e')[i]
const label = () => root.querySelector<HTMLElement>('.ov-size')
const band = (i: number) => root.querySelectorAll<HTMLElement>('.ov-band')[i]
const mag = () => root.querySelector<HTMLElement>('.ov-mag')
const dim = () => root.querySelector<HTMLElement>('.ov-dim')
const callsOf = (cmd: string) => calls.filter(([c]) => c === cmd)
/** 第 i 次 overlay_commit 的参数 */
function commitOf(i = 0): { rect: Rect; kind: string; session_id: number } {
  const call = callsOf('overlay_commit')[i]
  if (!call) throw new Error('overlay_commit 没有被调用')
  return call[1].commit as { rect: Rect; kind: string; session_id: number }
}

async function mount(i: OverlayInit | null = init()): Promise<void> {
  responses.overlay_init = () => i
  dispose = mountOverlay(root)
  await flush()
  // 交互模块动态加载
  await vi.waitFor(() => expect(sel()).not.toBeNull())
  await flush()
}

// mountOverlay 按需 import('./select')：第一次加载要现场转换 select.ts 与内联的 CSS / 文案，Windows runner 上超过了
// mount() 里 vi.waitFor 的默认 1 s（总是第一个调用 mount 的用例超时）。先在这里加载一次（模块缓存），
// 用例里的 waitFor 只等 attach 本身；「加载之前」的两个用例用 vi.doMock + resetModules，不受影响
beforeAll(async () => {
  await import('../select')
}, 30_000)

beforeEach(() => {
  listeners = new Map()
  calls = []
  fetched = []
  animations = []
  responses = {
    overlay_commit: () => ({ fly_to: { x: 120, y: 400, width: 440, height: 220 } }),
  }
  stubFetch()
  setTransport(fakeTransport())
  document.documentElement.dataset.os = 'windows'
  // jsdom 没有 WAAPI：记录调用，返回立即结束的动画
  Object.defineProperty(Element.prototype, 'animate', {
    configurable: true,
    writable: true,
    value(this: Element, keyframes: Keyframe[], options: KeyframeAnimationOptions) {
      animations.push({ el: this, keyframes, options })
      return { finished: Promise.resolve(), cancel() {} } as unknown as Animation
    },
  })
  Object.defineProperty(Element.prototype, 'getAnimations', {
    configurable: true,
    writable: true,
    value: () => [],
  })
  root = document.createElement('div')
  root.id = 'root'
  document.body.append(root)
})

afterEach(() => {
  dispose?.()
  dispose = null
  root.remove()
  document.head.querySelectorAll('style').forEach((s) => s.remove())
  delete (Element.prototype as Partial<Element>).animate
  delete (Element.prototype as Partial<Element>).getAnimations
  setTransport(null)
  delete document.documentElement.dataset.os
})

// ---------------------------------------------------------------------------

describe('几何', () => {
  it('任意方向拖拽得到规范矩形', () => {
    expect(rectOf({ x: 50, y: 80 }, { x: 10, y: 20 })).toEqual({
      x: 10,
      y: 20,
      width: 40,
      height: 60,
    })
  })
  it('四条边对齐到物理像素（150% 缩放下没有半像素）', () => {
    const r = snapRect({ x: 10.2, y: 5.1, width: 20.3, height: 7.7 }, 1.5)
    for (const v of [r.x, r.y, r.x + r.width, r.y + r.height]) {
      expect(Math.abs(v * 1.5 - Math.round(v * 1.5))).toBeLessThan(1e-9)
    }
  })
  it('夹在显示器内；平移保持尺寸并且不越界', () => {
    expect(clampRect({ x: -10, y: 590, width: 50, height: 50 }, W, H)).toEqual({
      x: 0,
      y: 590,
      width: 40,
      height: 10,
    })
    expect(shiftRect({ x: 900, y: 10, width: 80, height: 40 }, 500, -100, W, H)).toEqual({
      x: 920,
      y: 0,
      width: 80,
      height: 40,
    })
  })
  it('松开空格后以离指针最远的角为锚点', () => {
    expect(farCorner({ x: 0, y: 0, width: 100, height: 50 }, { x: 98, y: 49 })).toEqual({
      x: 0,
      y: 0,
    })
    expect(farCorner({ x: 0, y: 0, width: 100, height: 50 }, { x: 1, y: 2 })).toEqual({
      x: 100,
      y: 50,
    })
  })
  it('窗口检测按 z 序取最上层', () => {
    expect(hitWindow(WINDOWS, { x: 650, y: 350 })).toBe(WINDOWS[0])
    expect(hitWindow(WINDOWS, { x: 150, y: 150 })).toBe(WINDOWS[1])
    expect(hitWindow(WINDOWS, { x: 5, y: 5 })).toBeNull()
  })
  it('放大镜在光标右下 16 px，靠边翻转', () => {
    expect(magnifierAt({ x: 100, y: 100 }, W, H)).toEqual({
      x: 100 + MAG_OFFSET,
      y: 100 + MAG_OFFSET,
    })
    const corner = magnifierAt({ x: W - 10, y: H - 10 }, W, H)
    expect(corner.x).toBe(W - 10 - MAG_OFFSET - MAG_SIZE)
    expect(corner.y).toBeLessThan(H - 10)
  })
  it('放大镜翻转时优先不压住选区（贴着右边框选：翻到左下，而不是进入选区）', () => {
    // 从 (700, 100) 拖到贴右边的 (995, 300)：右边放不下，左上 / 右上都会压住选区，左下在选区下方
    const avoid = { x: 700, y: 100, width: 295, height: 200 }
    const at = magnifierAt({ x: 995, y: 300 }, W, H, avoid)
    expect(at).toEqual({ x: 995 - MAG_OFFSET - MAG_SIZE, y: 300 + MAG_OFFSET })
    // 选区贴着底边：下方放不下，只好进入选区
    const low = { x: 700, y: 300, width: 295, height: 300 }
    const inside = magnifierAt({ x: 995, y: 595 }, W, H, low)
    expect(inside.y).toBe(595 - MAG_OFFSET - MAG_SIZE - MAG_LABEL)
  })
  it('挖洞四块与边框八段：只用 transform（100 px 的基准方块按矩形缩放），边框粗细不随选区缩放', () => {
    expect(bandTransforms({ x: 10, y: 20, width: 30, height: 40 }, 100, 80)).toEqual([
      'translate(0px,0px) scale(1,0.2)',
      'translate(0px,60px) scale(1,0.2)',
      'translate(0px,20px) scale(0.1,0.4)',
      'translate(40px,20px) scale(0.6,0.4)',
    ])
    // 没有洞：上面一块铺满整屏，其余三块缩成 0
    expect(bandTransforms(null, 100, 80)).toEqual([
      'translate(0px,0px) scale(1,0.8)',
      'translate(0px,0px) scale(0,0)',
      'translate(0px,0px) scale(0,0)',
      'translate(0px,0px) scale(0,0)',
    ])
    // 外侧 1 px 描边（上下两段含四个角）+ 内侧 1 px 强调色
    expect(edgeTransforms({ x: 10, y: 20, width: 30, height: 40 }, 1, 1)).toEqual([
      'translate(9px,19px) scale(0.32,0.01)',
      'translate(9px,60px) scale(0.32,0.01)',
      'translate(9px,20px) scale(0.01,0.4)',
      'translate(40px,20px) scale(0.01,0.4)',
      'translate(10px,20px) scale(0.3,0.01)',
      'translate(10px,59px) scale(0.3,0.01)',
      'translate(10px,20px) scale(0.01,0.4)',
      'translate(39px,20px) scale(0.01,0.4)',
    ])
    // 窗口高亮：强调色 2 px
    expect(edgeTransforms({ x: 0, y: 0, width: 10, height: 10 }, 2, 1)[7]).toBe(
      'translate(8px,0px) scale(0.02,0.1)',
    )
  })
})

/**
 * 分数缩放（用户真机 Windows 150%：选框的四条边向右 / 向下长了 1/3，挖空也不对）。引擎按整设备像素画基准方块：方块是
 * `UNIT × dpr` 个设备像素，非整数时画成 `round(UNIT × dpr)` 个（真实设备缩放的 Chromium 与 WebKit 实测，150% 下 1 px 的
 * 方块画成 2 个设备像素），transform 的 scale 再乘上它。这里按这个模型把每一段的 transform 换算成屏幕上的设备像素矩形，
 * 要求：边线的端点、挖空的矩形都正好是选框；描边在选框外侧、四边一样粗；所有边都落在整设备像素上（清晰，没有半像素）
 */
describe('分数缩放下的边线与挖空（dpr 1 / 1.25 / 1.5 / 1.75 / 2 / 2.5）', () => {
  interface Px {
    x0: number
    y0: number
    x1: number
    y1: number
  }
  /** 一段 transform 在屏幕上画出的矩形（设备像素） */
  function painted(transform: string, dpr: number): Px {
    const m = /^translate\(([-\d.e]+)px,([-\d.e]+)px\) scale\(([-\d.e]+),([-\d.e]+)\)$/.exec(
      transform,
    )
    if (!m) throw new Error(`unexpected transform ${transform}`)
    const [tx, ty, sx, sy] = m.slice(1).map(Number) as [number, number, number, number]
    const base = Math.round(UNIT * dpr)
    return { x0: tx * dpr, y0: ty * dpr, x1: tx * dpr + sx * base, y1: ty * dpr + sy * base }
  }
  const dev = (r: Rect, dpr: number): Px => ({
    x0: r.x * dpr,
    y0: r.y * dpr,
    x1: (r.x + r.width) * dpr,
    y1: (r.y + r.height) * dpr,
  })
  const bbox = (list: Px[]): Px => ({
    x0: Math.min(...list.map((b) => b.x0)),
    y0: Math.min(...list.map((b) => b.y0)),
    x1: Math.max(...list.map((b) => b.x1)),
    y1: Math.max(...list.map((b) => b.y1)),
  })
  const grow = (b: Px, k: number): Px => ({
    x0: b.x0 - k,
    y0: b.y0 - k,
    x1: b.x1 + k,
    y1: b.y1 + k,
  })
  /** 两个设备像素矩形相等（浮点误差内） */
  function same(a: Px, b: Px): void {
    for (const k of ['x0', 'y0', 'x1', 'y1'] as const) expect(a[k]).toBeCloseTo(b[k], 6)
  }
  /** 每条边都落在整设备像素上 */
  function whole(b: Px): void {
    for (const v of [b.x0, b.y0, b.x1, b.y1]) expect(Math.abs(v - Math.round(v))).toBeLessThan(1e-6)
  }
  const inside = (b: Px, outer: Px): boolean =>
    b.x0 >= outer.x0 - 1e-6 &&
    b.y0 >= outer.y0 - 1e-6 &&
    b.x1 <= outer.x1 + 1e-6 &&
    b.y1 <= outer.y1 + 1e-6
  const overlaps = (a: Px, b: Px): boolean =>
    a.x0 < b.x1 - 1e-6 && b.x0 < a.x1 - 1e-6 && a.y0 < b.y1 - 1e-6 && b.y0 < a.y1 - 1e-6

  /** 显示器（逻辑 px）与一次拖拽：吸附后的选框是用户真机那种不对齐整逻辑像素的尺寸 */
  const SCREEN = { w: 1280, h: 720 }
  const RAW = { x: 100.4, y: 57.3, width: 621.5, height: 260.4 }

  for (const dpr of [1, 1.25, 1.5, 1.75, 2, 2.5]) {
    it(`dpr ${dpr}：基准方块是整数个设备像素`, () => {
      expect(Math.abs(UNIT * dpr - Math.round(UNIT * dpr))).toBeLessThan(1e-9)
    })

    it(`dpr ${dpr}：挖空正好是选框，四块暗化带铺满其余部分`, () => {
      const r = snapRect(RAW, dpr)
      const s = dev(r, dpr)
      const sw = SCREEN.w * dpr
      const sh = SCREEN.h * dpr
      const bands = bandTransforms(r, SCREEN.w, SCREEN.h).map((t) => painted(t, dpr))
      same(bands[0] as Px, { x0: 0, y0: 0, x1: sw, y1: s.y0 })
      same(bands[1] as Px, { x0: 0, y0: s.y1, x1: sw, y1: sh })
      same(bands[2] as Px, { x0: 0, y0: s.y0, x1: s.x0, y1: s.y1 })
      same(bands[3] as Px, { x0: s.x1, y0: s.y0, x1: sw, y1: s.y1 })
      for (const b of bands) {
        whole(b)
        expect(overlaps(b, s)).toBe(false)
      }
      // 没有洞时一块铺满整屏
      same(painted(bandTransforms(null, SCREEN.w, SCREEN.h)[0] as string, dpr), {
        x0: 0,
        y0: 0,
        x1: sw,
        y1: sh,
      })
    })

    for (const t of [1, 2]) {
      it(`dpr ${dpr}：${t === 1 ? '框选' : '窗口高亮'}的边线端点正好是选框，描边在外侧、整设备像素`, () => {
        const r = snapRect(RAW, dpr)
        const s = dev(r, dpr)
        const pieces = edgeTransforms(r, t, dpr).map((v) => painted(v, dpr))
        const rings = pieces.slice(0, 4)
        const accents = pieces.slice(4)
        const ring = Math.max(1, Math.round(dpr))
        const accent = Math.max(1, Math.round(t * dpr))
        expect(hairline(1, dpr) * dpr).toBeCloseTo(ring, 9)
        expect(hairline(t, dpr) * dpr).toBeCloseTo(accent, 9)
        for (const b of pieces) whole(b)
        // 强调色：都在选框内，合起来正好是选框（上下两段的端点 = 选框的左右边，左右两段的端点 = 选框的上下边）
        for (const b of accents) expect(inside(b, s)).toBe(true)
        same(bbox(accents), s)
        same(accents[0] as Px, { x0: s.x0, y0: s.y0, x1: s.x1, y1: s.y0 + accent })
        same(accents[1] as Px, { x0: s.x0, y0: s.y1 - accent, x1: s.x1, y1: s.y1 })
        same(accents[2] as Px, { x0: s.x0, y0: s.y0, x1: s.x0 + accent, y1: s.y1 })
        same(accents[3] as Px, { x0: s.x1 - accent, y0: s.y0, x1: s.x1, y1: s.y1 })
        // 描边：在选框外、外扩 ring 个设备像素之内，四段互不重叠（半透明，重叠处会更深），合起来正好一圈
        for (const b of rings) {
          expect(overlaps(b, s)).toBe(false)
          expect(inside(b, grow(s, ring))).toBe(true)
        }
        rings.forEach((a, i) =>
          rings.slice(i + 1).forEach((b) => expect(overlaps(a, b)).toBe(false)),
        )
        same(bbox(rings), grow(s, ring))
        const area = rings.reduce((n, b) => n + (b.x1 - b.x0) * (b.y1 - b.y0), 0)
        const outer = grow(s, ring)
        expect(area).toBeCloseTo(
          (outer.x1 - outer.x0) * (outer.y1 - outer.y0) - (s.x1 - s.x0) * (s.y1 - s.y0),
          6,
        )
      })
    }
  }
})

describe('会话', () => {
  it('先订阅 overlay:session 再 overlay_init；null 时等事件，按 session_id 去重', async () => {
    await mount(null)
    expect(calls[0]).toEqual(['listen', { event: 'overlay:session' }])
    // overlay:end 同样在 init 之前订阅
    expect(calls[1]).toEqual(['listen', { event: 'overlay:end' }])
    expect(calls[2]?.[0]).toBe('overlay_init')
    expect(callsOf('overlay_ready')).toHaveLength(0)

    responses.overlay_init = () => init()
    emit('overlay:session', { session_id: 7, monitor_id: '\\\\.\\DISPLAY1' })
    await flush()
    expect(callsOf('overlay_init')).toHaveLength(2)
    expect(callsOf('overlay_ready')).toEqual([
      ['overlay_ready', { session_id: 7, monitor_id: '\\\\.\\DISPLAY1' }],
    ])
    // 同一会话的事件再来一次：不重新 init
    emit('overlay:session', { session_id: 7, monitor_id: '\\\\.\\DISPLAY1' })
    await flush()
    expect(callsOf('overlay_init')).toHaveLength(2)
  })

  it('overlay:end（Rust 结束的会话）：按 session_id 核对后立即收尾、释放冻结帧，不再回调 cancel / done', async () => {
    await mount()
    expect(root).toHaveAttribute('data-on')
    // 别的会话的结束：不理
    emit('overlay:end', { session_id: 99, reason: 'cancelled' })
    expect(root).toHaveAttribute('data-on')
    emit('overlay:end', { session_id: 7, reason: 'retrigger' })
    expect(root).not.toHaveAttribute('data-on')
    expect(root.querySelector<HTMLCanvasElement>('.ov-frame')?.width).toBe(0)
    // 重复收尾无害；之后的 Esc 不再发 overlay_cancel（会话已经不在：Rust 的 overlay_init 回 null）
    emit('overlay:end', { session_id: 7, reason: 'retrigger' })
    responses.overlay_init = () => null
    key('Escape')
    await flush()
    expect(callsOf('overlay_cancel')).toHaveLength(0)
    expect(callsOf('overlay_done')).toHaveLength(0)
  })

  it('多屏：指针进入还不是 key 的 Overlay 时请求一次 overlay_focus；失去 key 后再进入时再请求（A → B → A）', async () => {
    const hasFocus = vi.spyOn(document, 'hasFocus').mockReturnValue(false)
    try {
      await mount()
      pointer('pointermove', 10, 10)
      pointer('pointermove', 20, 20)
      expect(callsOf('overlay_focus')).toEqual([['overlay_focus', { session_id: 7 }]])
      // 成为 key 之后指针移到另一块屏（B 成为 key，本窗口失去焦点），再回到本屏：要再请求一次
      hasFocus.mockReturnValue(true)
      window.dispatchEvent(new Event('focus'))
      pointer('pointermove', 25, 25)
      expect(callsOf('overlay_focus')).toHaveLength(1)
      hasFocus.mockReturnValue(false)
      window.dispatchEvent(new Event('blur'))
      pointer('pointermove', 30, 30)
      pointer('pointermove', 31, 31)
      expect(callsOf('overlay_focus')).toEqual([
        ['overlay_focus', { session_id: 7 }],
        ['overlay_focus', { session_id: 7 }],
      ])
      // 已经是 key：不请求
      hasFocus.mockReturnValue(true)
      responses.overlay_init = () => init({ session_id: 8 })
      emit('overlay:session', { session_id: 8, monitor_id: '\\\\.\\DISPLAY1' })
      await flush()
      pointer('pointermove', 40, 40)
      expect(callsOf('overlay_focus')).toHaveLength(2)
      // 新会话、不是 key：照常请求（带新会话号）
      hasFocus.mockReturnValue(false)
      window.dispatchEvent(new Event('blur'))
      pointer('pointermove', 50, 50)
      expect(callsOf('overlay_focus').at(-1)).toEqual(['overlay_focus', { session_id: 8 }])
    } finally {
      hasFocus.mockRestore()
    }
  })

  it('多屏：请求了但窗口没成为 key（没有 blur）时，指针离开再进入会再请求；请求失败复位并冷却 500 ms，不是每次移动都发', async () => {
    const hasFocus = vi.spyOn(document, 'hasFocus').mockReturnValue(false)
    const now = vi.spyOn(performance, 'now')
    try {
      await mount()
      pointer('pointermove', 10, 10)
      expect(callsOf('overlay_focus')).toHaveLength(1)
      // 没成为 key、也没有 blur：指针一直在里面不重复请求；离开再进入时再请求一次
      pointer('pointermove', 12, 12)
      expect(callsOf('overlay_focus')).toHaveLength(1)
      root.dispatchEvent(new Event('pointerleave'))
      pointer('pointermove', 14, 14)
      expect(callsOf('overlay_focus')).toHaveLength(2)
      // 请求失败：复位，但 500 ms 内的移动不再请求；冷却之后再请求
      responses.overlay_focus = () => {
        throw { code: 'internal', message: 'not key', llm: null }
      }
      root.dispatchEvent(new Event('pointerleave'))
      const t = performance.now()
      now.mockReturnValue(t)
      pointer('pointermove', 16, 16)
      expect(callsOf('overlay_focus')).toHaveLength(3)
      await flush()
      now.mockReturnValue(t + 100)
      pointer('pointermove', 18, 18)
      expect(callsOf('overlay_focus')).toHaveLength(3)
      now.mockReturnValue(t + 600)
      pointer('pointermove', 20, 20)
      expect(callsOf('overlay_focus')).toHaveLength(4)
    } finally {
      now.mockRestore()
      hasFocus.mockRestore()
    }
  })

  it('Windows / X11：先画 half 再 ready，之后换全分辨率；路径只交给 resolveAssetUrl', async () => {
    await mount()
    expect(fetched[0]).toBe('http://wisp.localhost/capture/7/5c5c2e5c444953504c415931/half')
    expect(fetched[1]).toBe('http://wisp.localhost/capture/7/5c5c2e5c444953504c415931/full')
    const canvas = root.querySelector<HTMLCanvasElement>('.ov-frame')
    expect(canvas?.hidden).toBe(false)
    expect(canvas?.width).toBe(W * 2)
    // 暗化层 0 → 1 渐入（presets.overlayDim，CSS 动画由 data-on 触发）
    expect(root.hasAttribute('data-on')).toBe(true)
  })

  it('macOS frozen_native：不画冻结帧，只拉全分辨率帧给放大镜；位图就绪后放大镜可见', async () => {
    await mount(init({ frozen_native: true, half_url: null }))
    expect(root.querySelector<HTMLCanvasElement>('.ov-frame')?.hidden).toBe(true)
    expect(fetched).toEqual(['http://wisp.localhost/capture/7/5c5c2e5c444953504c415931/full'])
    expect(callsOf('overlay_ready')).toHaveLength(1)
    pointer('pointermove', 300, 200)
    await frame()
    expect(off(mag())).toBe(false)
    expect(root.querySelector('.ov-coord')?.textContent).toBe('600, 400')
  })

  it('macOS 原生冻结帧带 region_url：不拉整屏帧，放大镜按光标所在的 256 px 小块拉取，到达后可见', async () => {
    const base = 'capture/7/5c5c2e5c444953504c415931/region'
    await mount(init({ frozen_native: true, half_url: null, region_url: base }))
    // 只拉整屏的低分辨率底图（第 5 段是面积上限），不拉整屏帧
    expect(fetched).toEqual([`http://wisp.localhost/${base}/0/0/2000/1200/160000`])
    expect(callsOf('overlay_ready')).toHaveLength(1)
    // 光标在逻辑 (300, 200) = 物理 (600, 400)：采样窗口加预取余量落在第 (2, 1) 块
    pointer('pointermove', 300, 200)
    await frame()
    expect(fetched.slice(1)).toEqual([`http://wisp.localhost/${base}/512/256/256/256`])
    await flush()
    await frame()
    expect(off(mag())).toBe(false)
    expect(root.querySelector('.ov-coord')?.textContent).toBe('600, 400')
    // 同一块里移动不再拉取
    pointer('pointermove', 310, 210)
    await frame()
    expect(fetched).toHaveLength(2)
    // 靠近块的角：预取相邻的块（已有的 (2, 1) 不再拉）
    pointer('pointermove', 256, 128)
    await frame()
    expect(fetched.slice(2).sort()).toEqual(
      [
        `http://wisp.localhost/${base}/256/0/256/256`,
        `http://wisp.localhost/${base}/512/0/256/256`,
        `http://wisp.localhost/${base}/256/256/256/256`,
      ].sort(),
    )
    // 右下角的小块按帧尺寸截短（2000 × 1200）
    pointer('pointermove', 995, 595)
    await frame()
    expect(fetched.at(-1)).toBe(`http://wisp.localhost/${base}/1792/1024/208/176`)
  })

  it('小块在路上的最多 6 个（快速扫屏不一下子发出几十个请求）；底图先到时放大镜照常出现，缺的部分画底图而不是黑块', async () => {
    const base = 'capture/7/5c5c2e5c444953504c415931/region'
    const gates: Array<() => void> = []
    vi.stubGlobal('fetch', async (url: string) => {
      fetched.push(url)
      const overview = url.endsWith('/160000')
      if (!overview) await new Promise<void>((r) => gates.push(r))
      const m = /\/(\d+)\/(\d+)(?:\/160000)?$/.exec(url)
      const rw = overview ? 516 : Number(m?.[1] ?? 1)
      const rh = overview ? 309 : Number(m?.[2] ?? 1)
      return new Response(new Uint8Array(rw * rh * 4), {
        headers: { 'x-wisp-width': String(rw), 'x-wisp-height': String(rh) },
      })
    })
    const stats = {
      tile_ms: [] as number[],
      lens: 0,
      partial: 0,
      black: 0,
      overview_ms: null,
      peak_inflight: 0,
    }
    ;(globalThis as { __WISP_LAB_OVERLAY__?: unknown }).__WISP_LAB_OVERLAY__ = stats
    try {
      await mount(init({ frozen_native: true, half_url: null, region_url: base }))
      await flush()
      const tilesFetched = () => fetched.filter((u) => !u.endsWith('/160000'))
      // 底图已到、小块都还在路上：放大镜可见（画的是底图）
      pointer('pointermove', 300, 200)
      await frame()
      expect(off(mag())).toBe(false)
      expect(stats.partial).toBeGreaterThan(0)
      expect(stats.black).toBe(0)
      // 快速扫过整屏：每帧都换到新的块，但在路上的不超过 6 个
      for (let i = 0; i < 12; i++) {
        pointer('pointermove', 40 + i * 80, 40 + (i % 5) * 110)
        // oxlint-disable-next-line no-await-in-loop -- 逐帧推进
        await frame()
      }
      expect(tilesFetched()).toHaveLength(6)
      expect(stats.peak_inflight).toBe(6)
      // 放行在路上的：之后按光标此刻的位置补拉
      for (const g of gates.splice(0)) g()
      await flush()
      pointer('pointermove', 600, 300)
      await frame()
      expect(tilesFetched().length).toBeGreaterThan(6)
      expect(stats.tile_ms.length).toBe(6)
    } finally {
      for (const g of gates.splice(0)) g()
      delete (globalThis as { __WISP_LAB_OVERLAY__?: unknown }).__WISP_LAB_OVERLAY__
    }
  })

  it('飞入那块在确认的提前返回路径上也会释放：commit 失败 / 没有飞入目标', async () => {
    const base = 'capture/7/5c5c2e5c444953504c415931/region'
    const closed: number[] = []
    const real = globalThis.createImageBitmap
    vi.stubGlobal('createImageBitmap', async (src: ImageData) => {
      const b = await real(src)
      const close = b.close.bind(b)
      return Object.assign(b, {
        close: () => {
          closed.push(src.width * src.height)
          close()
        },
      })
    })
    for (const commit of [
      () => {
        throw new Error('rust is stuck')
      },
      () => ({ fly_to: null }),
    ]) {
      closed.length = 0
      responses.overlay_commit = commit
      // oxlint-disable-next-line no-await-in-loop -- 两种路径依次跑
      await mount(init({ frozen_native: true, half_url: null, region_url: base }))
      pointer('pointerdown', 100, 100)
      pointer('pointermove', 300, 250)
      pointer('pointerup', 300, 250)
      // oxlint-disable-next-line no-await-in-loop -- 等 commit 与那块的拉取落定
      await flush(10)
      expect(fetched).toContain(`http://wisp.localhost/${base}/200/200/400/300`)
      // 选区那块（400 × 300 物理像素）已释放
      expect(closed).toContain(400 * 300)
      dispose?.()
      dispose = null
      fetched = []
    }
  })

  it('macOS 原生冻结帧带 region_url：确认时按选区拉取那块像素（与 commit 并行），照常飞入', async () => {
    const base = 'capture/7/5c5c2e5c444953504c415931/region'
    await mount(init({ frozen_native: true, half_url: null, region_url: base }))
    pointer('pointerdown', 100, 100)
    pointer('pointermove', 300, 250)
    pointer('pointerup', 300, 250)
    await flush()
    expect(fetched).toContain(`http://wisp.localhost/${base}/200/200/400/300`)
    const fly = animations.filter((a) => a.keyframes.some((k) => 'transform' in k))
    // 边框八段 + 那块像素
    expect(fly).toHaveLength(9)
    expect(fly.some((a) => a.el.classList.contains('ov-piece'))).toBe(true)
    await flush()
    expect(callsOf('overlay_done')).toEqual([['overlay_done', { session_id: 7 }]])
    expect(root.querySelector('.ov-piece')).toBeNull()
  })

  it('全分辨率位图还没到（macOS）：放大镜隐藏，框选照常', async () => {
    let release = (): void => {}
    const late = new Promise<void>((r) => {
      release = r
    })
    vi.stubGlobal('fetch', async (url: string) => {
      fetched.push(url)
      await late
      return new Response(new Uint8Array(W * 2 * H * 2 * 4), {
        headers: { 'x-wisp-width': String(W * 2), 'x-wisp-height': String(H * 2) },
      })
    })
    await mount(init({ frozen_native: true, half_url: null }))
    pointer('pointermove', 300, 200)
    await frame()
    expect(off(mag())).toBe(true)
    release()
    await flush()
    pointer('pointermove', 310, 210)
    await frame()
    expect(off(mag())).toBe(false)
  })

  it('没有 half_url（页面自己画全分辨率冻结帧）：全分辨率帧只拉取、解码一次，放大镜照常可用', async () => {
    await mount(init({ half_url: null }))
    expect(fetched).toEqual(['http://wisp.localhost/capture/7/5c5c2e5c444953504c415931/full'])
    pointer('pointermove', 200, 200)
    await frame()
    expect(off(mag())).toBe(false)
  })

  it('首帧解码期间会话被取代：旧位图不再画进冻结帧', async () => {
    let release = (): void => {}
    const gate = new Promise<void>((r) => {
      release = r
    })
    vi.stubGlobal('fetch', async (url: string) => {
      fetched.push(url)
      // 第一次会话的 half 帧被挡住
      if (url.endsWith('7/5c5c2e5c444953504c415931/half')) await gate
      const half = url.endsWith('/half')
      const big = url.includes('/7/')
      const w = (big ? W * 2 : W) / (half ? 2 : 1)
      const h = (big ? H * 2 : H) / (half ? 2 : 1)
      return new Response(new Uint8Array(w * h * 4), {
        headers: { 'x-wisp-width': String(w), 'x-wisp-height': String(h) },
      })
    })
    responses.overlay_init = () => init()
    dispose = mountOverlay(root)
    await flush()
    responses.overlay_init = () =>
      init({
        session_id: 8,
        image_size: { width: W, height: H },
        image_url: 'capture/8/5c5c2e5c444953504c415931/full',
        half_url: null,
      })
    emit('overlay:session', { session_id: 8, monitor_id: '\\\\.\\DISPLAY1' })
    await flush()
    const canvas = root.querySelector<HTMLCanvasElement>('.ov-frame')
    expect(canvas?.width).toBe(W)
    release()
    await flush()
    expect(canvas?.width).toBe(W)
  })

  it('冻结帧拿不到：取消本次截图（不留透明的 Overlay）', async () => {
    stubFetch(() => true)
    await mount()
    expect(callsOf('overlay_cancel')).toEqual([['overlay_cancel', { session_id: 7 }]])
    expect(callsOf('overlay_ready')).toHaveLength(0)
  })

  it('新会话重置全部状态（预建 Overlay 复用）', async () => {
    await mount()
    pointer('pointerdown', 100, 100)
    pointer('pointermove', 300, 200)
    await frame()
    expect(off(sel())).toBe(false)
    responses.overlay_init = () => init({ session_id: 8 })
    emit('overlay:session', { session_id: 8, monitor_id: '\\\\.\\DISPLAY1' })
    await flush()
    await frame()
    expect(off(sel())).toBe(true)
    expect(callsOf('overlay_ready').at(-1)?.[1]).toEqual({
      session_id: 8,
      monitor_id: '\\\\.\\DISPLAY1',
    })
  })
})

describe('框选与放大镜', () => {
  it('拖拽显示选区、W × H 标签与放大镜；松手即确认并立即 commit', async () => {
    await mount()
    pointer('pointermove', 100, 100)
    pointer('pointerdown', 100, 100)
    pointer('pointermove', 340.3, 260.7)
    await frame()
    const s = sel()
    expect(off(s)).toBe(false)
    expect(s?.dataset.k).toBe('drag')
    // 选区对齐物理像素（2 倍缩放：240.5 × 160.5 逻辑单位）；尺寸标签是逻辑单位，放大镜坐标才是物理像素。
    // 单位按平台：这里是 Windows（px），macOS 写 pt（见下面的「尺寸标签的单位」）
    expect(label()?.style.transform).toBe('translate(100px,100px)')
    expect(label()?.textContent).toBe('241 × 161 px')
    // 边框与挖洞都是 100 px 的基准方块按矩形缩放（jsdom 的 devicePixelRatio 是 1：强调色 1 px）
    expect(edge(0)?.style.transform).toBe('translate(100px,100px) scale(2.405,0.01)')
    // 暗化层挖洞：自身透明，四块围出洞
    expect(dim()?.hasAttribute('data-bands')).toBe(true)
    expect(band(0)?.style.transform).toBe('translate(0px,0px) scale(10,1)')
    // 全分辨率帧就绪后放大镜可见，显示物理坐标
    expect(off(mag())).toBe(false)
    expect(root.querySelector('.ov-coord')?.textContent).toBe('680, 521')
    pointer('pointerup', 340.3, 260.7)
    await flush()
    const commit = commitOf()
    expect(commit.kind).toBe('drag')
    expect(commit.session_id).toBe(7)
    expect(commit.rect).toEqual({ x: 100, y: 100, width: 240.5, height: 160.5 })
    // 飞入：选中的像素本身（裁出的一块）连同边框八段飞到面板头部缩略图的位置（等比，只动 transform）；暗化层整体
    // 淡出，挖洞的四块不动 → 淡出 → overlay_done
    const fly = animations.filter((a) => a.keyframes.some((k) => 'transform' in k))
    // 边框八段 + 那块像素
    expect(fly).toHaveLength(9)
    expect(fly.every((a) => Object.keys(a.keyframes[0] ?? {}).join() === 'transform')).toBe(true)
    const piece = fly.find((a) => a.el.classList.contains('ov-piece'))?.el as HTMLElement
    expect(piece.style.width).toBe('240.5px')
    // fly_to = (120, 400, 440 × 220)：落点 = 卡片内边距（16, 12）处、高 72（等比缩放 72 / 160.5）
    const k = 72 / 160.5
    expect(fly.find((a) => a.el === piece)?.keyframes).toEqual([
      { transform: 'translate(100px,100px) scale(1,1)' },
      { transform: `translate(136px,412px) scale(${k},${k})` },
    ])
    expect(animations.some((a) => a.el.classList.contains('ov-band'))).toBe(false)
    expect(animations.find((a) => a.el === dim())?.keyframes).toEqual([
      { opacity: 1 },
      { opacity: 0 },
    ])
    await flush()
    expect(callsOf('overlay_done')).toEqual([['overlay_done', { session_id: 7 }]])
    // 会话结束：那块像素移除并释放
    expect(root.querySelector('.ov-piece')).toBeNull()
  })

  it('拖不出起始显示器：选区夹在窗口边界内', async () => {
    await mount()
    pointer('pointerdown', 900, 500)
    pointer('pointermove', 1500, 900)
    pointer('pointerup', 1500, 900)
    await flush()
    const rect = commitOf().rect
    expect(rect).toEqual({ x: 900, y: 500, width: 100, height: 100 })
  })

  it('纯竖直拖拽（选区宽 < 1 px）：松手后离开拖拽、选区隐藏，不再跟着指针走；Enter 不提交', async () => {
    await mount(init({ window_detect: false }))
    pointer('pointerdown', 50, 50)
    pointer('pointermove', 50, 80)
    await frame()
    pointer('pointerup', 50, 80)
    // 没有按键按着地移动指针
    pointer('pointermove', 300, 300, 0, 0)
    await frame()
    expect(off(sel())).toBe(true)
    key('Enter')
    await flush()
    expect(callsOf('overlay_commit')).toHaveLength(0)
    // 之后照常框选
    pointer('pointerdown', 100, 100)
    pointer('pointermove', 300, 260)
    pointer('pointerup', 300, 260)
    await flush()
    expect(commitOf().rect).toEqual({ x: 100, y: 100, width: 200, height: 160 })
  })

  it('拖出去又拖回起点那一行：同样按单击处理，不卡在拖拽', async () => {
    await mount(init({ window_detect: false }))
    pointer('pointerdown', 50, 50)
    pointer('pointermove', 200, 120)
    pointer('pointermove', 200, 50.2)
    pointer('pointerup', 200, 50.2)
    pointer('pointermove', 400, 400, 0, 0)
    await frame()
    expect(off(sel())).toBe(true)
    expect(callsOf('overlay_commit')).toHaveLength(0)
  })

  it('极小的选区（< 8 × 8 逻辑 px，几乎一定是误触）不提交；落在窗口上时按单击选中该窗口', async () => {
    await mount(init({ window_detect: false }))
    pointer('pointerdown', 20, 20)
    pointer('pointermove', 26, 25)
    pointer('pointerup', 26, 25)
    await flush()
    expect(callsOf('overlay_commit')).toHaveLength(0)
    await frame()
    expect(off(sel())).toBe(true)
    dispose?.()
    dispose = null
    calls = []
    await mount()
    pointer('pointermove', 650, 350)
    pointer('pointerdown', 650, 350)
    pointer('pointermove', 656, 355)
    pointer('pointerup', 656, 355)
    await flush()
    expect(commitOf()).toMatchObject({ kind: 'window', rect: WINDOWS[0] })
  })

  it('全分辨率帧就绪前隐藏放大镜', async () => {
    stubFetch((url) => url.endsWith('/full'))
    await mount()
    pointer('pointermove', 200, 200)
    await frame()
    expect(off(mag())).toBe(true)
    // 冻结帧（half）照常，可以框选
    pointer('pointerdown', 200, 200)
    pointer('pointermove', 260, 260)
    await frame()
    expect(off(sel())).toBe(false)
  })

  it('按住空格平移整个选区，松开后从最远的角继续调整大小', async () => {
    await mount()
    pointer('pointerdown', 100, 100)
    pointer('pointermove', 200, 150)
    key('Space')
    expect(root.hasAttribute('data-pan')).toBe(true)
    pointer('pointermove', 260, 230)
    await frame()
    expect(label()?.style.transform).toBe('translate(160px,180px)')
    expect(label()?.textContent).toBe('100 × 50 px')
    expect(edge(0)?.style.transform).toBe('translate(160px,180px) scale(1,0.01)')
    key('Space', {}, 'keyup')
    expect(root.hasAttribute('data-pan')).toBe(false)
    pointer('pointermove', 300, 260)
    pointer('pointerup', 300, 260)
    await flush()
    const rect = commitOf().rect
    expect(rect).toEqual({ x: 160, y: 180, width: 140, height: 80 })
  })

  // 与真实引擎一致的事件序列（Chromium / WebKit 实测）：左键按住时再按右键是组合按键，只有 pointermove（button = 2）
  it('拖拽中按右键（组合按键）：清掉选区，松开左键不提交', async () => {
    await mount(init({ window_detect: false }))
    pointer('pointerdown', 100, 100)
    pointer('pointermove', 200, 200, -1, 1)
    await frame()
    expect(off(sel())).toBe(false)
    pointer('pointermove', 200, 200, 2, 3) // 右键按下
    await frame()
    expect(off(sel())).toBe(true)
    expect(callsOf('overlay_cancel')).toHaveLength(0)
    pointer('pointermove', 210, 210, 2, 1) // 右键松开，左键仍按着
    pointer('pointermove', 260, 260, -1, 1)
    pointer('pointerup', 260, 260, 0, 0)
    await flush()
    expect(callsOf('overlay_commit')).toHaveLength(0)
    expect(off(sel())).toBe(true)
    // 没有选区时再按右键：取消
    pointer('pointerdown', 260, 260, 2)
    await flush()
    expect(callsOf('overlay_cancel')).toHaveLength(1)
  })

  it('拖拽中按右键、先松左键再松右键：不卡在拖拽状态，之后照常框选', async () => {
    await mount(init({ window_detect: false }))
    pointer('pointerdown', 100, 100)
    pointer('pointermove', 200, 200, -1, 1)
    pointer('pointermove', 200, 200, 2, 3) // 右键按下
    pointer('pointermove', 200, 200, 0, 2) // 左键先松开
    pointer('pointermove', 400, 300, -1, 2) // 只按着右键移动：选区不跟着走
    await frame()
    expect(off(sel())).toBe(true)
    pointer('pointerup', 400, 300, 2, 0) // 最后松开的是右键
    await flush()
    expect(callsOf('overlay_commit')).toHaveLength(0)
    pointer('pointerdown', 100, 100)
    pointer('pointermove', 300, 300, -1, 1)
    pointer('pointerup', 300, 300)
    await flush()
    expect(commitOf().rect).toEqual({ x: 100, y: 100, width: 200, height: 200 })
  })

  it('⌘A 的全屏选区上单独按右键：清掉选区', async () => {
    await mount()
    key('KeyA', { ctrlKey: true })
    await frame()
    expect(sel()?.dataset.k).toBe('fullscreen')
    pointer('pointerdown', 200, 200, 2)
    await frame()
    expect(off(sel())).toBe(true)
    expect(callsOf('overlay_cancel')).toHaveLength(0)
  })
})

describe('窗口检测', () => {
  it('悬停高亮候选窗口；单击（< 4 px）选中该窗口', async () => {
    await mount()
    pointer('pointermove', 650, 350)
    await frame()
    expect(sel()?.dataset.k).toBe('window')
    expect(label()?.style.transform).toBe('translate(600px,300px)')
    pointer('pointerdown', 650, 350)
    pointer('pointermove', 650 + CLICK_SLOP - 1, 350)
    pointer('pointerup', 650 + CLICK_SLOP - 1, 350)
    await flush()
    const commit = commitOf()
    expect(commit.kind).toBe('window')
    expect(commit.rect).toEqual(WINDOWS[0])
  })

  it('按下后的小位移（< 4 px）不让窗口高亮闪掉', async () => {
    await mount()
    pointer('pointermove', 650, 350)
    await frame()
    pointer('pointerdown', 650, 350)
    pointer('pointermove', 652, 350, -1, 1)
    await frame()
    expect(off(sel())).toBe(false)
    expect(sel()?.dataset.k).toBe('window')
    expect(label()?.style.transform).toBe('translate(600px,300px)')
    // 超过单击容差：换成拖拽选区
    pointer('pointermove', 700, 420, -1, 1)
    await frame()
    expect(sel()?.dataset.k).toBe('drag')
  })

  it('关闭窗口检测时不高亮，单击什么也不做', async () => {
    await mount(init({ window_detect: false }))
    pointer('pointermove', 650, 350)
    await frame()
    expect(off(sel())).toBe(true)
    pointer('pointerdown', 650, 350)
    pointer('pointerup', 650, 350)
    await flush()
    expect(callsOf('overlay_commit')).toHaveLength(0)
  })
})

describe('键盘', () => {
  it('Esc 随时取消', async () => {
    await mount()
    pointer('pointerdown', 10, 10)
    pointer('pointermove', 100, 100)
    key('Escape')
    await flush()
    expect(callsOf('overlay_cancel')).toEqual([['overlay_cancel', { session_id: 7 }]])
    expect(callsOf('overlay_commit')).toHaveLength(0)
  })

  it('Ctrl+A（Windows）选中整个显示器，Enter 确认', async () => {
    await mount()
    key('KeyA', { ctrlKey: true })
    await frame()
    expect(sel()?.dataset.k).toBe('fullscreen')
    key('Enter')
    await flush()
    const commit = commitOf()
    expect(commit).toMatchObject({ kind: 'fullscreen', rect: { x: 0, y: 0, width: W, height: H } })
  })

  it('尺寸标签的单位按平台：macOS 写 pt，Windows / Linux 写 px（数值同为逻辑单位，文案在 overlay.json）', async () => {
    document.documentElement.dataset.os = 'macos'
    await mount()
    pointer('pointerdown', 100, 100)
    pointer('pointermove', 200, 150)
    await frame()
    expect(label()?.textContent).toBe('100 × 50 pt')
  })

  it('macOS 用 ⌘A；Ctrl+A 不触发', async () => {
    document.documentElement.dataset.os = 'macos'
    await mount()
    key('KeyA', { ctrlKey: true })
    await frame()
    expect(off(sel())).toBe(true)
    key('KeyA', { metaKey: true })
    await frame()
    expect(sel()?.dataset.k).toBe('fullscreen')
  })

  it('Enter 确认悬停中的窗口', async () => {
    await mount()
    pointer('pointermove', 150, 150)
    key('Enter')
    await flush()
    const commit = commitOf()
    expect(commit).toMatchObject({ kind: 'window', rect: WINDOWS[1] })
  })

  it('交互模块加载之前 Esc 也能取消（会话已开始、select 还没加载完）', async () => {
    let release = (): void => {}
    const gate = new Promise<void>((r) => {
      release = r
    })
    // 新的模块实例，交互模块的动态导入被挡住；传输层也要装到新实例上
    vi.resetModules()
    vi.doMock('../select', async (importOriginal) => {
      await gate
      return importOriginal()
    })
    const runtime = await import('../../shared/ipc/runtime')
    runtime.setTransport(fakeTransport())
    const { mountOverlay: mountFresh } = await import('../index')
    responses.overlay_init = () => init()
    dispose = mountFresh(root)
    await flush()
    // 会话已经开始（冻结帧已上屏、ready 已发出），交互模块还被挡着
    expect(callsOf('overlay_ready')).toHaveLength(1)
    expect(sel()).toBeNull()
    key('Escape')
    await flush()
    expect(callsOf('overlay_cancel')).toEqual([['overlay_cancel', { session_id: 7 }]])
    release()
    await flush()
    vi.doUnmock('../select')
    runtime.setTransport(null)
  })

  it('交互模块加载之前右键也能取消（负载高时第一次右键不能被吞掉）', async () => {
    let release = (): void => {}
    const gate = new Promise<void>((r) => {
      release = r
    })
    vi.resetModules()
    vi.doMock('../select', async (importOriginal) => {
      await gate
      return importOriginal()
    })
    const runtime = await import('../../shared/ipc/runtime')
    runtime.setTransport(fakeTransport())
    const { mountOverlay: mountFresh } = await import('../index')
    responses.overlay_init = () => init()
    dispose = mountFresh(root)
    await flush()
    expect(callsOf('overlay_ready')).toHaveLength(1)
    expect(sel()).toBeNull()
    const menu = new MouseEvent('contextmenu', { bubbles: true, cancelable: true, button: 2 })
    root.dispatchEvent(menu)
    expect(menu.defaultPrevented).toBe(true)
    await flush()
    expect(callsOf('overlay_cancel')).toEqual([['overlay_cancel', { session_id: 7 }]])
    release()
    await flush()
    vi.doUnmock('../select')
    runtime.setTransport(null)
  })

  it('还没有会话时按 Esc：重新 init，拿到会话立即取消（macOS 原生冻结帧不会卡在屏幕上）', async () => {
    await mount(null)
    expect(callsOf('overlay_ready')).toHaveLength(0)
    responses.overlay_init = () => init()
    key('Escape')
    await flush()
    expect(callsOf('overlay_init')).toHaveLength(2)
    expect(callsOf('overlay_cancel')).toEqual([['overlay_cancel', { session_id: 7 }]])
    expect(callsOf('overlay_ready')).toHaveLength(0)
  })

  it('init 先回 null、Esc 之后会话才由事件送到：仍然取消（Esc 不会被 null 清掉）', async () => {
    await mount(null)
    key('Escape')
    await flush()
    // 重新 init 仍然是 null（macOS：overlay_show 还没返回）
    expect(callsOf('overlay_init')).toHaveLength(2)
    expect(callsOf('overlay_cancel')).toHaveLength(0)
    responses.overlay_init = () => init()
    emit('overlay:session', { session_id: 7, monitor_id: '\\\\.\\DISPLAY1' })
    await flush()
    expect(callsOf('overlay_cancel')).toEqual([['overlay_cancel', { session_id: 7 }]])
    expect(callsOf('overlay_ready')).toHaveLength(0)
  })

  it('很久以前的 Esc 不会取消之后的会话', async () => {
    await mount(null)
    const now = performance.now()
    const spy = vi.spyOn(performance, 'now').mockReturnValue(now)
    key('Escape')
    await flush()
    spy.mockReturnValue(now + 5000)
    responses.overlay_init = () => init()
    emit('overlay:session', { session_id: 7, monitor_id: '\\\\.\\DISPLAY1' })
    await flush()
    spy.mockRestore()
    expect(callsOf('overlay_cancel')).toHaveLength(0)
    expect(callsOf('overlay_ready')).toHaveLength(1)
  })

  it('overlay_init 失败：有限次重试', async () => {
    let n = 0
    responses.overlay_init = () => {
      n += 1
      if (n < 3) throw { code: 'internal', message: 'not yet', llm: null }
      return init()
    }
    dispose = mountOverlay(root)
    await vi.waitFor(() => expect(callsOf('overlay_ready')).toHaveLength(1), { timeout: 2000 })
    expect(callsOf('overlay_init')).toHaveLength(3)
  })
})

describe('确认后的衔接', () => {
  it('关闭飞入：不飞，直接淡出', async () => {
    await mount(init({ fly_animation: false }))
    pointer('pointerdown', 100, 100)
    pointer('pointermove', 300, 300)
    pointer('pointerup', 300, 300)
    await flush()
    expect(animations.some((a) => a.el === sel())).toBe(false)
    expect(animations.some((a) => a.el === root)).toBe(true)
    expect(callsOf('overlay_done')).toHaveLength(1)
  })

  it('减少动效：不飞入', async () => {
    await mount(init({ reduced_motion: true }))
    pointer('pointerdown', 100, 100)
    pointer('pointermove', 300, 300)
    pointer('pointerup', 300, 300)
    await flush()
    expect(animations.some((a) => a.el === sel())).toBe(false)
    expect(document.documentElement.dataset.reducedMotion).toBe('true')
  })

  it('commit 迟迟不回应：Esc 取消；3 s 内没有回应也自动取消', async () => {
    responses.overlay_commit = () => new Promise(() => {})
    await mount()
    pointer('pointerdown', 100, 100)
    pointer('pointermove', 300, 300, -1, 1)
    pointer('pointerup', 300, 300)
    await flush()
    expect(callsOf('overlay_commit')).toHaveLength(1)
    key('Escape')
    await flush()
    expect(callsOf('overlay_cancel')).toEqual([['overlay_cancel', { session_id: 7 }]])

    // 新会话里不按 Esc：等超时
    responses.overlay_init = () => init({ session_id: 8 })
    emit('overlay:session', { session_id: 8, monitor_id: '\\\\.\\DISPLAY1' })
    await flush()
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    try {
      pointer('pointerdown', 100, 100)
      pointer('pointermove', 300, 300, -1, 1)
      pointer('pointerup', 300, 300)
      await vi.advanceTimersByTimeAsync(3100)
    } finally {
      vi.useRealTimers()
    }
    await flush()
    expect(callsOf('overlay_cancel').at(-1)).toEqual(['overlay_cancel', { session_id: 8 }])
  })

  it('飞入 / 淡出中按 Esc：直接结束', async () => {
    // 令牌给出真实时长（兜底计时器才不会立即结束），动画不结束：停在飞入里
    document.documentElement.style.setProperty('--dur-base', '220ms')
    document.documentElement.style.setProperty('--dur-fast', '140ms')
    Object.defineProperty(Element.prototype, 'animate', {
      configurable: true,
      writable: true,
      value(this: Element, keyframes: Keyframe[], options: KeyframeAnimationOptions) {
        animations.push({ el: this, keyframes, options })
        return { finished: new Promise(() => {}), cancel() {} } as unknown as Animation
      },
    })
    await mount()
    pointer('pointerdown', 100, 100)
    pointer('pointermove', 300, 300, -1, 1)
    pointer('pointerup', 300, 300)
    await flush()
    expect(animations.length).toBeGreaterThan(0)
    expect(callsOf('overlay_done')).toHaveLength(0)
    key('Escape')
    await flush()
    expect(callsOf('overlay_done')).toEqual([['overlay_done', { session_id: 7 }]])
    expect(callsOf('overlay_cancel')).toHaveLength(0)
    document.documentElement.style.removeProperty('--dur-base')
    document.documentElement.style.removeProperty('--dur-fast')
  })

  it('commit 失败：取消（交还焦点），不进入飞入', async () => {
    responses.overlay_commit = () => {
      throw { code: 'internal', message: 'crop failed', llm: null }
    }
    await mount()
    pointer('pointerdown', 100, 100)
    pointer('pointermove', 300, 300)
    pointer('pointerup', 300, 300)
    await flush()
    expect(callsOf('overlay_cancel')).toHaveLength(1)
    expect(callsOf('overlay_done')).toHaveLength(0)
  })

  it('会话结束后释放冻结帧缓冲区，暗化层复位', async () => {
    await mount()
    key('Escape')
    await flush()
    expect(root.querySelector<HTMLCanvasElement>('.ov-frame')?.width).toBe(0)
    expect(root.hasAttribute('data-on')).toBe(false)
  })
})

describe('动效令牌', () => {
  it('CSS 时长按单位换算（生产构建会把 220ms 压缩成 .22s）', async () => {
    const { toMs } = await import('../motion')
    expect(toMs('220ms')).toBe(220)
    expect(toMs('.22s')).toBe(220)
    expect(toMs('0.1s')).toBe(100)
    expect(toMs(' 320ms ')).toBe(320)
    expect(toMs('')).toBe(0)
  })
})
