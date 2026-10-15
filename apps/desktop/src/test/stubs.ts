/**
 * jsdom 缺失的浏览器 API stub。在 vitest 的 `setupFiles` 中**排在 setup.ts 之前**单独执行
 * （`sequence.setupFiles: 'list'`），因此：
 * - 各模块的 `__tests__/setup.local.ts`（由 setup.ts eager glob 加载）在模块顶层就能用到这里的全部 stub；
 * - local 文件对同名 API 的覆写（例如自定义 `HTMLCanvasElement.prototype.getContext`）**一定生效**，不会被这里覆盖。
 *
 * 优先级：本文件（基础 stub）< setup.local.ts（各模块的补充 / 覆写）< 单个测试里的 `vi.stubGlobal` / `vi.spyOn`。
 * 全局对象上的 stub 只在缺失时定义（`define`）；原型上的 canvas / 滚动 / 指针捕获方法无条件或按需定义。
 */
import { vi } from 'vitest'

// ---------------------------------------------------------------------------
// ResizeObserver / IntersectionObserver：可手动触发的最小实现

type ROCallback = (entries: ResizeObserverEntry[], observer: ResizeObserver) => void

class ResizeObserverStub implements ResizeObserver {
  static readonly instances = new Set<ResizeObserverStub>()
  readonly targets = new Set<Element>()
  constructor(private readonly callback: ROCallback) {
    ResizeObserverStub.instances.add(this)
  }
  observe(target: Element): void {
    this.targets.add(target)
  }
  unobserve(target: Element): void {
    this.targets.delete(target)
  }
  disconnect(): void {
    this.targets.clear()
    ResizeObserverStub.instances.delete(this)
  }
  /** 测试里手动触发：对每个目标按 getBoundingClientRect 构造 entry */
  trigger(): void {
    const entries = [...this.targets].map((target) => {
      const rect = target.getBoundingClientRect()
      const size = [{ inlineSize: rect.width, blockSize: rect.height }]
      return {
        target,
        contentRect: rect,
        borderBoxSize: size,
        contentBoxSize: size,
        devicePixelContentBoxSize: size,
      } as unknown as ResizeObserverEntry
    })
    this.callback(entries, this)
  }
}

class IntersectionObserverStub implements IntersectionObserver {
  readonly root = null
  readonly rootMargin = '0px'
  readonly scrollMargin = '0px'
  readonly thresholds = [0]
  observe(): void {}
  unobserve(): void {}
  disconnect(): void {}
  takeRecords(): IntersectionObserverEntry[] {
    return []
  }
}

define('ResizeObserver', ResizeObserverStub)
define('IntersectionObserver', IntersectionObserverStub)

/** 触发所有活动 ResizeObserver（测试辅助） */
export function triggerResizeObservers(): void {
  for (const ro of ResizeObserverStub.instances) ro.trigger()
}

// ---------------------------------------------------------------------------
// matchMedia：默认全部不匹配；测试可用 vi.spyOn(window, 'matchMedia') 覆写

define('matchMedia', (query: string): MediaQueryList => {
  const mql = {
    matches: false,
    media: query,
    onchange: null,
    addListener: () => {},
    removeListener: () => {},
    addEventListener: () => {},
    removeEventListener: () => {},
    dispatchEvent: () => false,
  }
  return mql as MediaQueryList
})

// ---------------------------------------------------------------------------
// 图像：ImageData / createImageBitmap / OffscreenCanvas / 最小 2D context

class ImageDataStub {
  readonly colorSpace = 'srgb'
  readonly data: Uint8ClampedArray
  readonly width: number
  readonly height: number
  constructor(dataOrWidth: Uint8ClampedArray | number, widthOrHeight: number, height?: number) {
    if (typeof dataOrWidth === 'number') {
      this.width = dataOrWidth
      this.height = widthOrHeight
      this.data = new Uint8ClampedArray(dataOrWidth * widthOrHeight * 4)
    } else {
      this.data = dataOrWidth
      this.width = widthOrHeight
      this.height = height ?? dataOrWidth.length / 4 / widthOrHeight
    }
  }
}
define('ImageData', ImageDataStub)

define('createImageBitmap', async (source: unknown): Promise<ImageBitmap> => {
  const s = source as { width?: number; height?: number }
  return {
    width: s?.width ?? 0,
    height: s?.height ?? 0,
    close: vi.fn<() => void>(),
  } as unknown as ImageBitmap
})

function createContext2D(canvas: { width: number; height: number }): CanvasRenderingContext2D {
  const noop = () => {}
  const ctx: Record<string, unknown> = {
    canvas,
    fillStyle: '#000',
    strokeStyle: '#000',
    lineWidth: 1,
    font: '10px sans-serif',
    globalAlpha: 1,
    globalCompositeOperation: 'source-over',
    imageSmoothingEnabled: true,
    textAlign: 'start',
    textBaseline: 'alphabetic',
    measureText: (text: string) => ({ width: text.length * 7 }) as TextMetrics,
    getImageData: (_x: number, _y: number, w: number, h: number) => new ImageDataStub(w, h),
    createImageData: (w: number, h: number) => new ImageDataStub(w, h),
    createLinearGradient: () => ({ addColorStop: noop }),
    createRadialGradient: () => ({ addColorStop: noop }),
    createPattern: () => null,
    getTransform: () => ({ a: 1, b: 0, c: 0, d: 1, e: 0, f: 0 }),
    isPointInPath: () => false,
  }
  for (const name of [
    'arc',
    'arcTo',
    'beginPath',
    'bezierCurveTo',
    'clearRect',
    'clip',
    'closePath',
    'drawImage',
    'ellipse',
    'fill',
    'fillRect',
    'fillText',
    'lineTo',
    'moveTo',
    'putImageData',
    'quadraticCurveTo',
    'rect',
    'resetTransform',
    'restore',
    'rotate',
    'roundRect',
    'save',
    'scale',
    'setLineDash',
    'setTransform',
    'stroke',
    'strokeRect',
    'strokeText',
    'transform',
    'translate',
  ]) {
    ctx[name] = vi.fn<(...args: unknown[]) => void>()
  }
  return ctx as unknown as CanvasRenderingContext2D
}

Object.defineProperty(HTMLCanvasElement.prototype, 'getContext', {
  configurable: true,
  writable: true,
  value: function getContext(this: HTMLCanvasElement, kind: string) {
    if (kind !== '2d') return null
    const self = this as HTMLCanvasElement & { __ctx2d?: CanvasRenderingContext2D }
    return (self.__ctx2d ??= createContext2D(this))
  },
})
Object.defineProperty(HTMLCanvasElement.prototype, 'toDataURL', {
  configurable: true,
  writable: true,
  value: () => 'data:image/png;base64,',
})

class OffscreenCanvasStub {
  private ctx: CanvasRenderingContext2D | null = null
  constructor(
    public width: number,
    public height: number,
  ) {}
  getContext(kind: string) {
    if (kind !== '2d') return null
    return (this.ctx ??= createContext2D(this))
  }
}
define('OffscreenCanvas', OffscreenCanvasStub)

// ---------------------------------------------------------------------------
// 滚动、指针捕获、object URL

for (const name of ['scrollTo', 'scrollBy', 'scrollIntoView'] as const) {
  if (!(name in Element.prototype)) {
    Object.defineProperty(Element.prototype, name, {
      configurable: true,
      writable: true,
      value() {},
    })
  }
}
// window 上的滚动方法 jsdom 只有「Not implemented」占位（调用时往 virtual console 打一条错误）。Motion 的
// KeyframesResolver 测量完需要读取 DOM 的关键帧（例如 height: auto）后会调用 window.scrollTo 恢复滚动位置，
// 测试输出因此混进与被测代码无关的噪声。替换成空实现
for (const name of ['scrollTo', 'scrollBy', 'scroll'] as const) {
  Object.defineProperty(window, name, {
    configurable: true,
    writable: true,
    value() {},
  })
}
for (const name of ['setPointerCapture', 'releasePointerCapture'] as const) {
  if (!(name in Element.prototype)) {
    Object.defineProperty(Element.prototype, name, {
      configurable: true,
      writable: true,
      value() {},
    })
  }
}
if (!('hasPointerCapture' in Element.prototype)) {
  Object.defineProperty(Element.prototype, 'hasPointerCapture', {
    configurable: true,
    writable: true,
    value: () => false,
  })
}

let objectUrlSeq = 0
if (typeof URL.createObjectURL !== 'function') {
  URL.createObjectURL = () => `blob:wisp-test/${++objectUrlSeq}`
  URL.revokeObjectURL = () => {}
}

function define(name: string, value: unknown): void {
  if (name in globalThis) return
  Object.defineProperty(globalThis, name, { configurable: true, writable: true, value })
}
