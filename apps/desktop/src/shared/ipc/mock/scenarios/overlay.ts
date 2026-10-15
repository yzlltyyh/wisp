/**
 * mock 场景：截图 Overlay。id 前缀 `overlay.` 即文件名（`findScenario` 按前缀找文件）；
 * `overlay.drag` 是流畅度场景（`pnpm qa:shots --perf`）。
 *
 * 每个场景用同一张**静态 RGBA 测试图**（合成的桌面：壁纸、菜单栏 / 任务栏、文档窗口、代码窗口、对话框）作冻结帧，
 * 经 `ctx.asset` 以 `capture/<session>/<MonitorId 十六进制>/{half,full}` 注册（与 Rust 同形，带 x-wisp-width / height
 * 头），`overlay_init` 返回与图上窗口一致的 `windows` 矩形。`?os=macos` 走原生冻结帧路径（`frozen_native`）：页面不画
 * 冻结帧，场景在 #root 下面铺一块 canvas 代替原生 CALayer，放大镜按 `region_url` 拉小块；其他平台走 half → full 路径。
 *
 * 交互用合成的 PointerEvent / KeyboardEvent 驱动（与真实输入走同一套处理函数），稳定后 `ctx.settled()`。
 */
/* oxlint-disable no-await-in-loop -- 逐帧驱动指针（每帧一个事件）必须串行 */
import type { OverlayCommitAck, OverlayInit, Rect } from '../../types'
import type { MockContext, Scenario } from '../types'

/** 合成桌面上的窗口（逻辑坐标，按 z 序，最上层在前），随视口按比例摆放 */
function layoutWindows(w: number, h: number, mac: boolean): Rect[] {
  const top = mac ? 24 : 0
  const r = (x: number, y: number, ww: number, hh: number): Rect => ({
    x: Math.round(x * w),
    y: Math.round(top + y * (h - top)),
    width: Math.round(ww * w),
    height: Math.round(hh * (h - top)),
  })
  return [
    r(0.6, 0.6, 0.3, 0.22), // 对话框（最上层）
    r(0.47, 0.12, 0.46, 0.56), // 代码窗口
    r(0.06, 0.08, 0.5, 0.74), // 文档窗口
  ]
}

const DOC_LINES = [
  ['h', '性能预算 · Performance budget'],
  ['p', '面板唤出 p95 ≤ 100 ms，首 token p95 ≤ 1.2 s。'],
  ['p', '空闲 CPU ≈ 0%，常驻内存 ≤ 60 MB。'],
  ['p', 'Wisp stays out of your way: it idles at'],
  ['p', 'near-zero CPU and wakes in milliseconds.'],
  ['h', '截图 OCR'],
  ['p', '拖拽框选 → 松手即识别 → 结果在选区下方出现。'],
  ['p', 'Press ⌥S, drag a region, release to recognize.'],
] as const

const CODE_LINES = [
  ['#c586c0', 'export function'],
  ['#dcdcaa', '  snapRect(r: Rect, scale: number) {'],
  ['#9cdcfe', '  const x0 = Math.round(r.x * s) / s'],
  ['#9cdcfe', '  const y0 = Math.round(r.y * s) / s'],
  ['#6a9955', '  // 对齐物理像素：Retina 下没有半像素'],
  ['#9cdcfe', '  return { x: x0, y: y0, ... }'],
  ['#d4d4d4', '}'],
] as const

type Pixels = Uint8ClampedArray<ArrayBuffer>

/** 画一张合成的桌面（RGBA 字节，与 Rust 送来的冻结帧同格式） */
function paintDesktop(pw: number, ph: number, scale: number, mac: boolean): Pixels {
  const canvas = new OffscreenCanvas(pw, ph)
  const g = canvas.getContext('2d')
  if (!g) return new Uint8ClampedArray(pw * ph * 4)
  g.scale(scale, scale)
  const w = pw / scale
  const h = ph / scale
  const bg = g.createLinearGradient(0, 0, w, h)
  bg.addColorStop(0, '#7f9cf5')
  bg.addColorStop(0.55, '#b794f4')
  bg.addColorStop(1, '#f6ad55')
  g.fillStyle = bg
  g.fillRect(0, 0, w, h)
  const sans = '-apple-system, "PingFang SC", "Segoe UI", "Microsoft YaHei", sans-serif'
  if (mac) {
    g.fillStyle = 'rgb(255 255 255 / .55)'
    g.fillRect(0, 0, w, 24)
    g.fillStyle = '#1d1d1f'
    g.font = `600 13px ${sans}`
    g.fillText('', 18, 17)
    g.fillText('Notes', 44, 17)
    g.font = `13px ${sans}`
    ;['File', 'Edit', 'View', 'Window', 'Help'].forEach((m, i) => g.fillText(m, 96 + i * 52, 17))
  } else {
    g.fillStyle = 'rgb(240 240 244 / .86)'
    g.fillRect(0, h - 48, w, 48)
    for (let i = 0; i < 6; i++) {
      g.fillStyle = i === 2 ? '#3b82f6' : 'rgb(0 0 0 / .18)'
      g.fillRect(w / 2 - 150 + i * 52, h - 36, 24, 24)
    }
  }
  const [dialog, code, doc] = layoutWindows(w, h, mac)
  const shadow = (r: Rect) => {
    g.fillStyle = 'rgb(0 0 0 / .18)'
    g.fillRect(r.x + 3, r.y + 10, r.width, r.height)
  }
  const chrome = (r: Rect, dark: boolean, title: string) => {
    shadow(r)
    g.fillStyle = dark ? '#1e1e1e' : '#ffffff'
    g.fillRect(r.x, r.y, r.width, r.height)
    g.fillStyle = dark ? '#2d2d2d' : '#f3f3f5'
    g.fillRect(r.x, r.y, r.width, 30)
    ;['#ff5f57', '#febc2e', '#28c840'].forEach((c, i) => {
      g.fillStyle = c
      g.beginPath()
      g.arc(r.x + 16 + i * 18, r.y + 15, 5.5, 0, Math.PI * 2)
      g.fill()
    })
    g.fillStyle = dark ? '#cccccc' : '#3a3a3c'
    g.font = `12px ${sans}`
    g.fillText(title, r.x + 76, r.y + 19)
  }
  if (doc) {
    chrome(doc, false, 'design.md — Notes')
    let y = doc.y + 64
    for (const [kind, text] of DOC_LINES) {
      g.fillStyle = kind === 'h' ? '#111114' : '#3a3a3c'
      g.font = kind === 'h' ? `600 20px ${sans}` : `15px ${sans}`
      g.fillText(text, doc.x + 28, y)
      y += kind === 'h' ? 36 : 28
    }
  }
  if (code) {
    chrome(code, true, 'geometry.ts — Wisp')
    g.font = '13px "SF Mono", Menlo, Consolas, monospace'
    CODE_LINES.forEach(([color, text], i) => {
      g.fillStyle = '#858585'
      g.fillText(String(i + 1).padStart(2, ' '), code.x + 14, code.y + 58 + i * 22)
      g.fillStyle = color
      g.fillText(text, code.x + 44, code.y + 58 + i * 22)
    })
  }
  if (dialog) {
    shadow(dialog)
    g.fillStyle = '#ffffff'
    g.fillRect(dialog.x, dialog.y, dialog.width, dialog.height)
    g.fillStyle = '#111114'
    g.font = `600 15px ${sans}`
    g.fillText('保存更改？', dialog.x + 20, dialog.y + 34)
    g.fillStyle = '#6e6e73'
    g.font = `13px ${sans}`
    g.fillText('Do you want to save changes?', dialog.x + 20, dialog.y + 58)
    g.fillStyle = '#3b82f6'
    g.fillRect(dialog.x + dialog.width - 100, dialog.y + dialog.height - 44, 80, 28)
    g.fillStyle = '#ffffff'
    g.fillText('保存 Save', dialog.x + dialog.width - 88, dialog.y + dialog.height - 25)
  }
  return g.getImageData(0, 0, pw, ph).data
}

/** 2× 降采样（half_url 的帧） */
function downsample(src: Pixels, pw: number, ph: number): Pixels {
  const w = Math.max(1, Math.round(pw / 2))
  const h = Math.max(1, Math.round(ph / 2))
  const out = new Uint8ClampedArray(w * h * 4)
  for (let y = 0; y < h; y++) {
    for (let x = 0; x < w; x++) {
      const s = (Math.min(ph - 1, y * 2) * pw + Math.min(pw - 1, x * 2)) * 4
      const d = (y * w + x) * 4
      out[d] = src[s] ?? 0
      out[d + 1] = src[s + 1] ?? 0
      out[d + 2] = src[s + 2] ?? 0
      out[d + 3] = 255
    }
  }
  return out
}

const hex = (id: string): string =>
  Array.from(new TextEncoder().encode(id), (b) => b.toString(16).padStart(2, '0')).join('')

interface OverlayRig {
  init: OverlayInit
  root(): HTMLElement
  /** 交互模块挂好、（按需）全分辨率帧就绪 */
  ready(): Promise<void>
  move(x: number, y: number, buttons?: number): void
  down(x: number, y: number, button?: number): void
  up(x: number, y: number): void
  key(code: string, init?: KeyboardEventInit & { type?: 'keydown' | 'keyup' }): void
  frames(n?: number): Promise<void>
}

interface RigOptions {
  windowDetect?: boolean
  fly?: boolean
  /** 全分辨率帧延迟（放大镜「就绪前隐藏」） */
  fullDelayMs?: number
}

/** 安装 Overlay 的 mock：测试图、`overlay_init` / `overlay_commit` 与驱动器 */
function rig(ctx: MockContext, opts: RigOptions = {}): OverlayRig {
  const mac = ctx.boot.os === 'macos'
  const monitorId = ctx.boot.os === 'windows' ? '\\\\.\\DISPLAY1' : mac ? '1' : 'DP-1'
  const w = innerWidth
  const h = innerHeight
  const scale = devicePixelRatio || 1
  const pw = Math.round(w * scale)
  const ph = Math.round(h * scale)
  const pixels = paintDesktop(pw, ph, scale, mac)
  const base = `capture/1/${hex(monitorId)}`
  const rgba = (path: string, data: Pixels, dw: number, dh: number) =>
    ctx.asset(path, new Blob([data.slice().buffer]), {
      'x-wisp-width': String(dw),
      'x-wisp-height': String(dh),
    })
  const fullUrl = rgba(`${base}/full`, pixels, pw, ph)
  if (!mac) rgba(`${base}/half`, downsample(pixels, pw, ph), Math.round(pw / 2), Math.round(ph / 2))
  // 图层（自下而上）：「实时屏幕」（测试图）→ 面板替身（commit 后出现在 Overlay 下方）→
  // macOS 的原生冻结帧图层（CALayer，属于 Overlay 窗口，overlay_done 后随窗口销毁）→ Overlay 页面（#root）
  document.getElementById('wisp-mock-desktop')?.remove()
  const layer = (z: number) => {
    const c = document.createElement('canvas')
    c.width = pw
    c.height = ph
    Object.assign(c.style, {
      position: 'fixed',
      inset: '0',
      width: '100%',
      height: '100%',
      zIndex: String(z),
    })
    c.getContext('2d')?.putImageData(new ImageData(pixels, pw, ph), 0, 0)
    document.body.append(c)
    return c
  }
  layer(0)
  const native = mac ? layer(2) : null
  const panel = document.createElement('div')
  panel.setAttribute('aria-hidden', 'true')
  Object.assign(panel.style, {
    position: 'fixed',
    zIndex: '1',
    display: 'none',
    boxSizing: 'border-box',
    padding: '14px 16px',
    borderRadius: 'var(--r-panel)',
    background: 'var(--surface-solid)',
    boxShadow: 'var(--shadow-popover)',
  } satisfies Partial<CSSStyleDeclaration>)
  for (const width of ['38%', '100%', '92%', '64%']) {
    const line = document.createElement('div')
    Object.assign(line.style, {
      width,
      height: '10px',
      margin: '10px 0 14px',
      borderRadius: '5px',
      background: 'var(--fill-pressed)',
    })
    panel.append(line)
  }
  document.body.append(panel)
  requestAnimationFrame(() => {
    const r = document.getElementById('root')
    if (r) r.style.zIndex = '3'
  })
  const init: OverlayInit = {
    session_id: 1,
    monitor: {
      id: monitorId,
      name: mac ? 'Built-in Retina Display' : 'DISPLAY1',
      physical_origin: { x: 0, y: 0 },
      physical_size: { width: pw, height: ph },
      scale_factor: scale,
      frame: { x: 0, y: 0, width: w, height: h },
      work_area: { x: 0, y: mac ? 24 : 0, width: w, height: h - (mac ? 24 : 48) },
      is_primary: true,
    },
    image_url: `${base}/full`,
    image_size: { width: pw, height: ph },
    windows: layoutWindows(w, h, mac),
    mode: 'ocr',
    window_detect: opts.windowDetect ?? true,
    fly_animation: (opts.fly ?? true) && !ctx.boot.reduce_motion,
    reduced_motion: ctx.boot.reduce_motion,
    frozen_native: mac,
    half_url: mac ? null : `${base}/half`,
    // 原生冻结帧：放大镜与飞入按需拉小块
    region_url: mac ? ctx.regionAsset(`${base}/region`, `${base}/full`) : null,
  }
  let readyCalled: () => void = () => {}
  const readyCall = new Promise<void>((resolve) => (readyCalled = resolve))
  ctx.handle('overlay_init', () => init)
  ctx.handle('overlay_ready', () => readyCalled())
  ctx.handle('overlay_commit', ({ commit }): OverlayCommitAck => {
    ctx.log('overlay_commit', commit.kind, commit.rect)
    const r = commit.rect
    const pwid = 440
    const below = r.y + r.height + 8
    const to = {
      x: Math.min(Math.max(8, r.x), w - pwid - 8),
      y: below + 220 > h ? Math.max(8, r.y - 228) : below,
      width: pwid,
      height: 220,
    }
    // Rust 在 commit 时立即让面板以 OCR 视图出现在 Overlay 下方（被冻结帧挡住，Overlay 淡出后露出）
    Object.assign(panel.style, {
      display: 'block',
      left: `${to.x}px`,
      top: `${to.y}px`,
      width: `${to.width}px`,
      height: `${to.height}px`,
    })
    return { fly_to: to }
  })
  ctx.handle('overlay_done', () => native?.remove())
  ctx.handle('overlay_cancel', () => {
    native?.remove()
    panel.remove()
  })
  if (opts.fullDelayMs) {
    // 全分辨率帧晚到：先只有冻结帧，放大镜隐藏
    const realFetch = window.fetch
    window.fetch = async (input, initArg) => {
      const url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url
      if (url === fullUrl) {
        await new Promise((r) => setTimeout(r, opts.fullDelayMs))
      }
      return realFetch(input, initArg)
    }
  }
  const root = () => document.getElementById('root') as HTMLElement
  const pointer = (type: string, x: number, y: number, button: number, buttons: number) =>
    root().dispatchEvent(
      new PointerEvent(type, {
        clientX: x,
        clientY: y,
        button,
        buttons,
        pointerId: 1,
        pointerType: 'mouse',
        isPrimary: true,
        bubbles: true,
        cancelable: true,
      }),
    )
  const frames = (n = 2) =>
    new Promise<void>((resolve) => {
      const step = (left: number) =>
        left <= 0 ? resolve() : requestAnimationFrame(() => step(left - 1))
      step(n)
    })
  return {
    init,
    root,
    async ready() {
      await readyCall
      while (!root().querySelector('.ov-mag')) await frames(1)
      await frames(2)
    },
    move: (x, y, buttons = 0) => pointer('pointermove', x, y, 0, buttons),
    down: (x, y, button = 0) => pointer('pointerdown', x, y, button, button === 0 ? 1 : 2),
    up: (x, y) => pointer('pointerup', x, y, 0, 0),
    key(code, { type = 'keydown', ...rest } = {}) {
      const key =
        code === 'Space' ? ' ' : code.startsWith('Key') ? code.slice(3).toLowerCase() : code
      dispatchEvent(
        new KeyboardEvent(type, { code, key, bubbles: true, cancelable: true, ...rest }),
      )
    },
    frames,
  }
}

/** 等放大镜出现（全分辨率帧就绪） */
async function lensShown(r: OverlayRig): Promise<void> {
  for (let i = 0; i < 120 && r.root().querySelector('.ov-mag')?.hasAttribute('data-off'); i++) {
    await r.frames(1)
  }
  await r.frames(2)
}

/** 平滑拖拽：每帧一个 pointermove（ease-in-out 插值） */
async function glide(r: OverlayRig, from: [number, number], to: [number, number], steps: number) {
  for (let i = 1; i <= steps; i++) {
    const t = i / steps
    const e = t < 0.5 ? 2 * t * t : 1 - (-2 * t + 2) ** 2 / 2
    r.move(from[0] + (to[0] - from[0]) * e, from[1] + (to[1] - from[1]) * e, 1)
    await r.frames(1)
  }
}

const scenario = (
  id: string,
  label: string,
  run: (ctx: MockContext) => Promise<void>,
  extra: Partial<Scenario> = {},
): Scenario => ({
  id: `overlay.${id}`,
  label,
  view: 'overlay',
  manualSettle: true,
  setup(ctx) {
    void run(ctx)
      .catch((error: unknown) => console.error('[mock] overlay 场景失败', error))
      .finally(() => ctx.settled())
  },
  ...extra,
})

const mac = (ctx: MockContext) => ctx.boot.os === 'macos'

export const scenarios: readonly Scenario[] = [
  scenario(
    'enter',
    'Overlay · 进入（冻结帧 + 暗化层渐入）',
    async (ctx) => {
      const r = rig(ctx)
      await r.ready()
      ctx.motionMid()
      await new Promise((resolve) => setTimeout(resolve, 420))
    },
    { motion: true },
  ),
  scenario('hover-window', 'Overlay · 悬停高亮窗口 + 放大镜', async (ctx) => {
    const r = rig(ctx)
    await r.ready()
    const [, code] = r.init.windows
    if (code) r.move(code.x + code.width * 0.4, code.y + code.height * 0.45)
    await lensShown(r)
  }),
  scenario('select', 'Overlay · 拖拽框选（尺寸标签 + 放大镜）', async (ctx) => {
    const r = rig(ctx)
    await r.ready()
    const doc = r.init.windows[2] ?? r.init.monitor.frame
    const a: [number, number] = [doc.x + 18, doc.y + 40]
    r.down(...a)
    await glide(r, a, [doc.x + doc.width * 0.82, doc.y + 150 + (mac(ctx) ? 4 : 0)], 12)
    await lensShown(r)
  }),
  scenario('select-edge', 'Overlay · 贴边选区（标签放进内部、放大镜翻转）', async (ctx) => {
    const r = rig(ctx)
    await r.ready()
    const { width: w, height: h } = r.init.monitor.frame
    r.down(w - 12, 6)
    await glide(r, [w - 12, 6], [w - 380, h - 10], 12)
    await lensShown(r)
  }),
  scenario('fullscreen', 'Overlay · ⌘A 全屏（等待 Enter）', async (ctx) => {
    const r = rig(ctx)
    await r.ready()
    r.move(640, 420)
    r.key('KeyA', mac(ctx) ? { metaKey: true } : { ctrlKey: true })
    await lensShown(r)
  }),
  scenario('lens-loading', 'Overlay · 全分辨率帧未到：放大镜隐藏', async (ctx) => {
    const r = rig(ctx, { fullDelayMs: 60_000 })
    await r.ready()
    const doc = r.init.windows[2] ?? r.init.monitor.frame
    r.down(doc.x + 30, doc.y + 60)
    await glide(r, [doc.x + 30, doc.y + 60], [doc.x + 320, doc.y + 180], 8)
    await r.frames(3)
  }),
  scenario(
    'fly',
    'Overlay · 松手确认 → 飞向面板 → 淡出',
    async (ctx) => {
      const r = rig(ctx)
      await r.ready()
      const doc = r.init.windows[2] ?? r.init.monitor.frame
      const a: [number, number] = [doc.x + 18, doc.y + 40]
      const b: [number, number] = [doc.x + doc.width * 0.8, doc.y + 180]
      r.down(...a)
      await glide(r, a, b, 10)
      await lensShown(r)
      // 飞入一开始（commit 返回、根节点带上 data-fly 的同一个任务里动画已创建）就把飞入的动画（选中的那块像素、
      // 边框、暗化层的整体淡出）定在一半处，
      // 留出截中间帧的时间，再继续播完（淡出 → overlay_done → 露出面板）。用 MutationObserver 而不是逐帧轮询：
      // 录屏时无头浏览器的 rAF 会被拖慢，轮询可能错过整个 220 ms 的飞入
      // 减少动效 / 关闭飞入时没有飞入：直接看淡出（兜底超时，场景总能稳定）
      const flying = new Promise<boolean>((resolve) => {
        if (!r.init.fly_animation) return resolve(false)
        const timer = setTimeout(() => resolve(false), 2000)
        const mo = new MutationObserver(() => {
          if (!r.root().hasAttribute('data-fly')) return
          mo.disconnect()
          clearTimeout(timer)
          resolve(true)
        })
        mo.observe(r.root(), { attributes: true, attributeFilter: ['data-fly'] })
      })
      r.up(...b)
      if (!(await flying)) {
        await new Promise((resolve) => setTimeout(resolve, 600))
        return
      }
      // 只定格飞入（WAAPI：那块像素与边框的 transform、暗化层的淡出）；暗化层渐入是 CSS 动画，可能还没播完，不动它
      const running = document
        .getAnimations()
        .filter(
          (anim) =>
            anim.playState === 'running' &&
            !(anim instanceof CSSAnimation) &&
            anim.effect instanceof KeyframeEffect &&
            anim.effect.target !== r.root(),
        )
      for (const anim of running) {
        anim.pause()
        const total = anim.effect?.getTiming().duration
        if (typeof total === 'number') anim.currentTime = total / 2
      }
      ctx.motionMid()
      // 录屏时截一张 2× 全屏图可能要一两秒：定格久一点
      await new Promise((resolve) => setTimeout(resolve, 2500))
      for (const anim of running) anim.play()
      await new Promise((resolve) => setTimeout(resolve, 600))
    },
    { motion: true },
  ),
  scenario(
    'drag',
    'Overlay · 拖拽流畅度（放大镜 + 尺寸标签 + 空格平移 + 确认飞入）',
    async (ctx) => {
      const r = rig(ctx)
      await r.ready()
      await lensShown(r)
      const { width: w, height: h } = r.init.monitor.frame
      // 悬停扫过几个窗口（窗口高亮 + 放大镜）
      await glide(r, [w * 0.1, h * 0.2], [w * 0.8, h * 0.5], 40)
      // 拖拽框选
      const a: [number, number] = [w * 0.12, h * 0.15]
      r.down(...a)
      await glide(r, a, [w * 0.62, h * 0.7], 90)
      // 按住空格平移整个选区，松开后继续调整大小
      r.key('Space')
      await glide(r, [w * 0.62, h * 0.7], [w * 0.8, h * 0.82], 40)
      r.key('Space', { type: 'keyup' })
      await glide(r, [w * 0.8, h * 0.82], [w * 0.9, h * 0.9], 30)
      r.up(w * 0.9, h * 0.9)
      await new Promise((resolve) => setTimeout(resolve, 600))
    },
    { motion: true },
  ),
]
