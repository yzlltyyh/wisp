/**
 * mock 的模拟窗口。
 *
 * 浏览器里整页就是「屏幕」，`#root` 充当窗口：mock 用 transform 把它放到锚点处、按 PanelPrepare 设宽度、
 * 按 panel_ready / panel_resize 上报的高度设高度，并在隐藏时 `visibility: hidden`。
 * `#root` 带 transform，因此其中 `position: fixed` 的元素也以「窗口」为包含块，和真实窗口一致。
 * 应用代码布局时应以 `#root`（= 窗口）为准，不要用 vw / vh。
 *
 * 同时模拟：40 ms ready 超时、`hidden_raf=0`（prepare 到 ready 之间屏蔽 rAF / ResizeObserver）、
 * 非 key 面板收不到键盘（Esc 由「全局 Esc guard」转成 hide_request）、外部点击关闭、开发者模式的延迟埋点。
 */
import type { Boot } from '../boot'
import { windowKindOf } from '../boot'
import type { HideReason, PanelPrepare, PanelShown, PanelView, Rect, ResizeSpring } from '../types'
import type { MockBus } from './bus'
import type { MockDesktop } from './desktop'

/** 各视图的初始估计高度（40 ms 超时时用它 show） */
const ESTIMATED_HEIGHT: Record<PanelView, number> = {
  translate: 120,
  ocr: 220,
  clipboard: 240,
  input: 168,
  toast: 40,
}
const READY_TIMEOUT_MS = 40
const ANCHOR_GAP = 12
/** Css 帧模式下窗口为阴影留的透明边距（--shadow-edge） */
const SHADOW_EDGE = 24
/** Css 帧模式的阶梯放大步长与最小间隔 */
const STEP = 48
/** Native 帧模式模拟窗口的高度过渡：近似 Rust 的 spring.gentle（约 300 ms）与 spring.snappy（约 230 ms） */
const NATIVE_TRANSITION_GENTLE =
  'height 300ms cubic-bezier(.22,1,.36,1), transform 300ms cubic-bezier(.22,1,.36,1)'
const NATIVE_TRANSITION_SNAPPY =
  'height 230ms cubic-bezier(.22,1,.36,1), transform 230ms cubic-bezier(.22,1,.36,1)'

interface Session {
  prepare: PanelPrepare
  anchor: Rect
  /**
   * Center 放置时窗口的顶边（按会话开始时的高度算一次）：与 Rust 一致——卡片中心在工作区 1/3 处（按初始高度）、之后
   * 顶边不动向下生长（windows/placement.rs `place_panel` / panel.rs `desktop_window_rect`）。每次阶梯放大都重新
   * 居中的话，Css 帧模式里窗口会上移半个台阶、贴顶的卡片随之跳动一帧
   */
  centerTop: number | null
  preparedAt: number
  readyAt: number | null
  readyTimeout: boolean
  focused: boolean
}

export class MockPanelWindow {
  private readonly root: HTMLElement
  private session: Session | null = null
  private visible = false
  private contentHeight = 0
  private windowHeight = 0
  private pendingAnchor: Rect | null = null
  private timeout: ReturnType<typeof setTimeout> | null = null
  private readonly frames: FrameGate

  constructor(
    private readonly boot: Boot,
    private readonly bus: MockBus,
    private readonly desktop: MockDesktop | null,
    hiddenRafBlocked: boolean,
  ) {
    this.root = ensureRoot()
    this.frames = new FrameGate(hiddenRafBlocked)
    Object.assign(this.root.style, {
      position: 'absolute',
      left: '0',
      top: '0',
      zIndex: '1',
      overflow: 'hidden',
      visibility: 'hidden',
      contain: 'layout',
    } satisfies Partial<CSSStyleDeclaration>)
    if (boot.frame_mode === 'native') {
      // 原生帧模式：窗口即卡片，模拟 NSVisualEffectView（popover 材质）+ 原生 hasShadow + 12 pt 圆角。
      // WebKit 内核下（Playwright 的无头 WebKit 声明支持 backdrop-filter、截图里却不画模糊）改用不透明底色，
      // 否则半透明着色层背后的应用文字原样透出；材质的通透感以 Chromium 为准
      const blur = boot.material === 'native' ? 'blur(40px) saturate(1.8)' : ''
      if (blur && isWebKitEngine()) document.documentElement.dataset.mockMaterial = 'solid'
      Object.assign(this.root.style, {
        borderRadius: '12px',
        backdropFilter: blur,
        boxShadow: '0 0 0 .5px rgb(0 0 0 / .18), 0 10px 32px rgb(0 0 0 / .22)',
        transition: NATIVE_TRANSITION_GENTLE,
      } satisfies Partial<CSSStyleDeclaration>)
      // Safari 18 之前只认带前缀的写法
      this.root.style.setProperty('-webkit-backdrop-filter', blur)
    }
    bus.addHook((event, payload) => {
      if (event === 'panel:prepare') this.onPrepare(payload as PanelPrepare)
    })
    this.installInputRouting()
  }

  /** 下一次 prepare 使用的锚点（场景 / HUD 指定） */
  setNextAnchor(anchor: Rect | null): void {
    this.pendingAnchor = anchor
  }

  get currentSession(): number | null {
    return this.session?.prepare.session_id ?? null
  }

  workArea(): Rect {
    return this.desktop?.workArea() ?? { x: 0, y: 0, width: innerWidth, height: innerHeight }
  }

  // ----- 命令

  ready(sessionId: number, contentHeight: number): PanelShown | null {
    const s = this.session
    if (!s || s.prepare.session_id !== sessionId) return null
    this.clearTimeout()
    if (s.readyAt === null) s.readyAt = performance.now()
    this.contentHeight = contentHeight
    this.applyHeight(true)
    this.show()
    return { session_id: sessionId, focused: s.focused }
  }

  painted(sessionId: number): void {
    const s = this.session
    if (!s || s.prepare.session_id !== sessionId || !this.boot.dev) return
    const now = performance.now()
    this.bus.emit('panel:latency', {
      session_id: sessionId,
      origin: 'mock',
      method: null,
      selection_ms: null,
      prepare_ms: 0,
      ready_ms: Math.round((s.readyAt ?? now) - s.preparedAt),
      shown_ms: Math.round(now - s.preparedAt),
      first_token_ms: null,
      ready_timeout: s.readyTimeout,
    })
  }

  /** `spring = snappy`（浮层预留引起的长高 / 回收）：Native 帧模式这一次的高度过渡改用更短的时长 */
  resize(sessionId: number, contentHeight: number, spring: ResizeSpring = 'gentle'): void {
    if (this.session?.prepare.session_id !== sessionId) return
    this.contentHeight = contentHeight
    if (this.boot.frame_mode === 'native') {
      this.root.style.transition =
        spring === 'snappy' ? NATIVE_TRANSITION_SNAPPY : NATIVE_TRANSITION_GENTLE
    }
    this.applyHeight(false)
  }

  /** 视图内切换的新卡片宽度（`panel_set_width`）：窗口立即改宽，位置按锚点重算（真实 Rust 保持左边不动） */
  setWidth(sessionId: number, width: number): void {
    const s = this.session
    if (!s || s.prepare.session_id !== sessionId || s.prepare.width === width) return
    s.prepare = { ...s.prepare, width }
    const edge = this.boot.frame_mode === 'css' ? SHADOW_EDGE : 0
    this.root.style.width = `${width + edge * 2}px`
    this.position(this.windowHeight, edge)
  }

  hide(sessionId: number, _reason: HideReason): void {
    if (this.session?.prepare.session_id !== sessionId) return
    this.clearTimeout()
    this.visible = false
    this.root.style.visibility = 'hidden'
    this.root.dataset.mockWindow = 'hidden'
  }

  activate(sessionId: number): void {
    const s = this.session
    if (!s || s.prepare.session_id !== sessionId || s.focused) return
    s.focused = true
    this.bus.emit('panel:focus_changed', { session_id: sessionId, focused: true })
  }

  /** 模拟 Rust 发出 hide_request（Esc guard、外部点击、再次触发 toggle） */
  requestHide(reason: HideReason): void {
    if (!this.session || !this.visible) return
    this.bus.emit('panel:hide_request', { session_id: this.session.prepare.session_id, reason })
  }

  // ----- 内部

  private onPrepare(p: PanelPrepare): void {
    this.clearTimeout()
    const anchor = this.pendingAnchor ?? this.desktop?.selectionRect() ?? centerRect()
    this.pendingAnchor = null
    this.session = {
      prepare: p,
      anchor,
      centerTop: null,
      preparedAt: performance.now(),
      readyAt: null,
      readyTimeout: false,
      focused: p.focused,
    }
    if (!this.visible) {
      this.frames.block()
      this.contentHeight = ESTIMATED_HEIGHT[p.view]
      this.windowHeight = 0
    }
    const edge = this.boot.frame_mode === 'css' ? SHADOW_EDGE : 0
    this.root.style.width = `${p.width + edge * 2}px`
    this.root.dataset.mockSession = String(p.session_id)
    this.applyHeight(true)
    this.timeout = setTimeout(() => {
      // 超时未 ready 就以估算高度 show（卡片此时仍是 opacity 0）
      const s = this.session
      if (!s || s.prepare.session_id !== p.session_id || this.visible) return
      s.readyTimeout = true
      console.warn(
        `[mock] panel_ready 超时（${READY_TIMEOUT_MS} ms），以估算高度显示`,
        p.session_id,
      )
      this.show()
    }, READY_TIMEOUT_MS)
  }

  private show(): void {
    this.visible = true
    this.root.style.visibility = 'visible'
    this.root.dataset.mockWindow = 'shown'
    this.frames.release()
  }

  private applyHeight(immediate: boolean): void {
    const s = this.session
    if (!s) return
    const p = s.prepare
    const css = this.boot.frame_mode === 'css'
    const edge = css ? SHADOW_EDGE : 0
    const target = Math.min(this.contentHeight, p.max_height) + edge * 2
    let next = target
    if (css && !immediate && target > this.windowHeight && this.windowHeight > 0) {
      // 阶梯放大：new = min(max(h + 2m, cur + 48), max_h)
      next = Math.min(Math.max(target, this.windowHeight + STEP), p.max_height + edge * 2)
    }
    this.windowHeight = next
    const transition = this.root.style.transition
    if (immediate) this.root.style.transition = 'none'
    this.root.style.height = `${next}px`
    this.position(next, edge)
    if (immediate) {
      void this.root.offsetHeight
      this.root.style.transition = transition
    }
  }

  private position(height: number, edge: number): void {
    const s = this.session
    if (!s) return
    const p = s.prepare
    const work = this.workArea()
    const width = p.width + edge * 2
    let x: number
    let y: number
    if (p.placement === 'center') {
      x = work.x + (work.width - width) / 2
      s.centerTop ??= work.y + work.height / 3 - height / 2
      y = s.centerTop
    } else {
      x = s.anchor.x - edge
      y =
        p.placement === 'above'
          ? s.anchor.y - ANCHOR_GAP - height + edge
          : s.anchor.y + s.anchor.height + ANCHOR_GAP - edge
    }
    x = clamp(x, work.x - edge, work.x + work.width - width + edge)
    y = clamp(y, work.y - edge, work.y + work.height - height + edge)
    this.root.style.transform = `translate(${Math.round(x)}px, ${Math.round(y)}px)`
  }

  private clearTimeout(): void {
    if (this.timeout !== null) clearTimeout(this.timeout)
    this.timeout = null
  }

  /** 非 key 的翻译 / OCR / toast 面板收不到键盘：Esc 由 Rust 的全局 Esc guard 转成 hide_request */
  private installInputRouting(): void {
    addEventListener(
      'keydown',
      (e) => {
        const s = this.session
        if (!s || !this.visible || s.focused) return
        if (isHudEvent(e)) return
        e.stopImmediatePropagation()
        e.preventDefault()
        // toast 视图不注册 Esc guard
        if (e.key === 'Escape' && s.prepare.view !== 'toast') this.requestHide('escape')
      },
      { capture: true },
    )
    addEventListener(
      'pointerdown',
      (e) => {
        const s = this.session
        if (!s || !this.visible || isHudEvent(e)) return
        if (this.root.contains(e.target as Node)) {
          // macOS：点击非激活面板后它自动成为 key
          if (this.boot.os === 'macos') this.activate(s.prepare.session_id)
          return
        }
        const view = s.prepare.view
        if (view === 'toast') return // toast 不监听外部点击
        if (view === 'translate' || view === 'ocr') {
          if (!s.focused) this.requestHide('click_outside')
          else this.requestHide('blur')
        } else this.requestHide('blur')
      },
      { capture: true },
    )
  }
}

/** pinned / overlay / settings：固定的「窗口」框 */
export function frameStaticWindow(boot: Boot, desktop: MockDesktop | null): void {
  const root = ensureRoot()
  const kind = windowKindOf(boot.label)
  root.dataset.mockWindow = 'shown'
  if (kind === 'pinned') {
    const work = desktop?.workArea() ?? { x: 0, y: 0, width: innerWidth, height: innerHeight }
    const radius = boot.os === 'macos' ? '12px' : boot.os === 'windows' ? '8px' : '0'
    Object.assign(root.style, {
      position: 'absolute',
      left: '0',
      top: '0',
      zIndex: '1',
      width: '440px',
      height: '320px',
      transform: `translate(${Math.round(work.x + work.width * 0.52)}px, ${Math.round(work.y + work.height * 0.18)}px)`,
      overflow: 'hidden',
      contain: 'layout',
      borderRadius: radius,
      boxShadow:
        boot.os === 'linux' ? '' : '0 0 0 .5px rgb(0 0 0 / .2), 0 16px 48px rgb(0 0 0 / .28)',
    } satisfies Partial<CSSStyleDeclaration>)
  } else if (kind === 'overlay') {
    Object.assign(root.style, {
      position: 'fixed',
      inset: '0',
      zIndex: '1',
    } satisfies Partial<CSSStyleDeclaration>)
  }
}

// ---------------------------------------------------------------------------

/** prepare 到 ready 之间屏蔽 rAF 与 ResizeObserver 回调（模拟隐藏窗口，`?hidden_raf=0`） */
class FrameGate {
  private blocked = false
  private readonly rafQueue = new Map<number, FrameRequestCallback>()
  private readonly roQueue = new Map<ResizeObserver, () => void>()
  private nextId = -1

  constructor(private readonly enabled: boolean) {
    if (!enabled) return
    const realRaf = window.requestAnimationFrame.bind(window)
    const realCancel = window.cancelAnimationFrame.bind(window)
    window.requestAnimationFrame = (cb) => {
      if (!this.blocked) return realRaf(cb)
      const id = this.nextId--
      this.rafQueue.set(id, cb)
      return id
    }
    window.cancelAnimationFrame = (id) => {
      if (!this.rafQueue.delete(id)) realCancel(id)
    }
    const RealRO = window.ResizeObserver
    const isBlocked = () => this.blocked
    const queue = this.roQueue
    window.ResizeObserver = class extends RealRO {
      constructor(cb: ResizeObserverCallback) {
        super((entries, observer) => {
          if (isBlocked()) queue.set(observer, () => cb(entries, observer))
          else cb(entries, observer)
        })
      }
    }
  }

  block(): void {
    if (this.enabled) this.blocked = true
  }

  release(): void {
    if (!this.enabled || !this.blocked) return
    this.blocked = false
    const ro = [...this.roQueue.values()]
    this.roQueue.clear()
    for (const run of ro) run()
    const raf = [...this.rafQueue.values()]
    this.rafQueue.clear()
    for (const cb of raf) requestAnimationFrame(cb)
  }
}

function ensureRoot(): HTMLElement {
  let root = document.getElementById('root')
  if (!root) {
    root = document.createElement('div')
    root.id = 'root'
    document.body.append(root)
  }
  return root
}

function centerRect(): Rect {
  return { x: innerWidth * 0.3, y: innerHeight * 0.3, width: 1, height: 20 }
}

function clamp(v: number, lo: number, hi: number): number {
  return Math.max(lo, Math.min(hi, v))
}

function isHudEvent(e: Event): boolean {
  return e.composedPath().some((n) => n instanceof HTMLElement && n.id === 'wisp-mock-hud')
}

/**
 * 浏览器 mock 是否跑在 WebKit 内核上（Safari、Playwright 的 WebKit）。只用于 mock 的材质模拟：无头 WebKit 不画
 * backdrop-filter 的模糊，原生材质用不透明的 `--surface-solid` 代替（真实应用的材质由 NSVisualEffectView 绘制，不受影响）
 */
function isWebKitEngine(): boolean {
  const ua = navigator.userAgent
  return /AppleWebKit/.test(ua) && !/Chrome|Chromium|Edg\//.test(ua)
}
