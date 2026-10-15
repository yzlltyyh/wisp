/**
 * Overlay 的框选交互（状态：idle → hover → drag → done），首帧之后动态加载（见 host.ts）。
 *
 * - 拖拽框选：任意方向拖拽，限制在本显示器（本窗口）内；松手即确认；位移 < 4 px 算单击（这期间继续显示
 *   按下前的高亮，触控板上的小位移不会让高亮闪掉）；拖出来的选区宽或高不到 8 逻辑 px（纯竖直 / 水平拖动、拖出去又
 *   拖回来、误触）同样按单击处理，不提交、也不会停在拖拽状态；
 * - 单击：开启窗口检测时选中光标下的窗口；悬停时先高亮候选窗口（挖洞 + 2 px 边框）；
 * - 按住空格：整个选区跟随指针平移，松开后以最远的角为锚点继续调整大小；
 * - Esc 随时取消；确认之后（请求已在 commit 时发出）Esc 只提前收起 Overlay：等 commit 回应时 `overlay_cancel`、
 *   飞入 / 淡出中直接 `overlay_done`，面板照常出现（编排器在 commit 之后收到 cancel 不中止请求）；
 *   Enter 确认当前选区（拖拽中的选区、⌘A 的全屏、悬停中的窗口）；⌘A / Ctrl+A 选中整个显示器；右键：有选区时清掉选区，没有时取消。
 *   **拖拽中按右键是组合按键**：浏览器只发 pointermove（button = 2），不发 pointerdown（Pointer Events 规范），
 *   所以在 pointermove 里识别；结束以「所有按键都松开」（buttons = 0）为准；
 * - 尺寸标签实时显示 W × H（逻辑单位；macOS 写 pt、Windows / Linux 写 px，文案在 overlay.json；放大镜坐标是
 *   物理像素），默认在选区左上角外侧，
 *   上方放不下时放进内部；放大镜 112 px：14×14 物理像素 ×8、网格 + 十字、下方显示物理坐标，位于光标右下 16 px，
 *   靠边翻转、优先不压住选区；
 * - 渲染：所有 DOM 写入合并到一个 rAF；选区边框的八段、四个把手、尺寸标签与暗化层挖洞的四块**只改 transform**（交给
 *   合成器，不布局、不重绘整屏暗化层），值没变就不写；边框与挖洞是 100 px 的纯色方块按选区缩放（分数缩放下长度精确、
 *   线宽取整到设备像素，见 geometry.ts 的 UNIT / hairline）；放大镜只改 transform 并重画一块 112 px canvas（像素尺寸按
 *   当前 devicePixelRatio，混合缩放的多显示器之间换屏后自动跟上）；
 * - 确认：选区定格 → 立即 `overlay_commit`（Rust 裁剪并发出请求、面板在 Overlay 下方显示；3 s 内没有回应即收起
 *   Overlay）→ **选中的那块像素本身**（冻结帧里裁出的一块，独立合成层）连同边框在 `--dur-base` 内飞到面板头部缩略图
 *   的位置（translate + scale，等比；`presets.flyTo` 同值），暗化层同时整体淡出、洞不动；设置里可关，减少动效时不飞
 *   → 整个 Overlay 淡出 `--dur-fast`（减少动效时 ≤ 100 ms）→ `overlay_done`。
 * - **macOS 原生冻结帧按需拉小块**：有 `region_url` 时放大镜不再拉取整屏 RGBA（4K 屏约 23 MiB，
 *   主进程里会留下两块整屏缓冲区、WebContent 再解码一份），改为按光标所在的 256 物理像素见方的小块拉取（采样窗口外
 *   多预取 48 px，最多缓存 16 块、约 4 MiB，按最近使用淘汰）；飞入用的那块像素在确认时与 `overlay_commit` 并行按选区
 *   拉取（Rust 把大块等比降采样到约 4 MP）。同时在 Overlay 显示时拉一张整屏的低分辨率底图（约 16 万像素、640 KB）：
 *   小块还没到的部分先画底图放大的样子，不画黑块（快速划过屏幕时放大镜里不会出现纯黑的方块）；底图与第一块
 *   都没到之前放大镜隐藏。在路上的小块最多 [`TILE_INFLIGHT`] 个（先拉采样窗口本身覆盖的，再拉预取余量里的），快速扫屏
 *   时不会一下子发出几十个请求。没有 `region_url`（旧的 Rust）时仍按整屏帧处理。
 * - 飞入那块像素在确认的每条路径上都会释放：commit 失败、commit 回来之后会话已换、没有飞入目标或飞入关闭时，等它到达后
 *   立即 close（不留给 GC）。
 */
import en from '../locales/en-US/overlay.json'
import zh from '../locales/zh-CN/overlay.json'
import type { OverlayInit, OverlaySelectionKind, Point, Rect } from '../shared/ipc/overlay'
import tokens from '../shared/styles/tokens.css?inline'
import css from './select.css?inline'
import {
  CLICK_SLOP,
  MAG_SAMPLES,
  MAG_SIZE,
  bandTransforms,
  clamp,
  clampRect,
  edgeTransforms,
  farCorner,
  hitWindow,
  landingRect,
  magnifierAt,
  MIN_SELECTION,
  rectOf,
  shiftRect,
  snapRect,
  tf,
} from './geometry'
import type { Interaction, OverlayHost } from './host'
import { token, tokenMs } from './motion'

type Mode = 'idle' | 'hover' | 'drag' | 'done'
interface Shown {
  rect: Rect
  kind: OverlaySelectionKind
}

/** overlay_commit 的回应上限：Rust 卡住时不让置顶的全屏 Overlay 无法退出（「Esc 随时取消」的兜底） */
const COMMIT_TIMEOUT_MS = 3000
/** 还没定位的边框段与挖洞块：缩成 0（挂上 DOM 之后、第一次画之前不露出 100 px 的方块） */
const NONE = tf(0, 0, 0, 0)
/** 飞入用的那块像素的上限（约 4 MP、16 MB）：更大的选区按比例降采样，飞起后立即缩小，看不出差别 */
const PIECE_MAX_PX = 4_000_000
/** 放大镜小块（macOS 原生冻结帧，见文件头）：边长（物理像素）、缓存上限、采样窗口外的预取余量 */
const TILE = 256
const TILE_CACHE = 16
const TILE_MARGIN = 48
/** 在路上的小块上限（见文件头） */
const TILE_INFLIGHT = 6
/** 低分辨率底图的面积上限（物理像素；区域路径的第 5 段） */
const OVERVIEW_PX = 160_000
/** 飞入那块像素在 commit 回应之后最多再等多久（通常已与 commit 并行到达；没到就只飞边框） */
const PIECE_WAIT_MS = 60
/** 小块的状态：位图 / 在路上 / 拿不到（不再重试，画底图） */
type Tile = ImageBitmap | 'loading' | 'failed'

/**
 * window lab 的统计（`WISP_WINDOW_LAB_OCR_DRIVE`）：实验台在驱动页面之前放一个空对象到
 * `window.__WISP_LAB_OVERLAY__`，这里只在它存在时记录——产品里每次只多一次属性读取。`tile_ms`：每块小块的往返（请求 →
 * 位图可用）；`lens`：画过的放大镜帧数；`partial`：采样窗口里有一部分还没有清晰小块（画了底图）的帧；`black`：
 * 底图也还没到、那一部分只能画黑的帧；`overview_ms`：底图的往返；`peak_inflight`：在路上的小块最多时有几个
 */
interface LabOverlayStats {
  tile_ms: number[]
  lens: number
  partial: number
  black: number
  overview_ms: number | null
  peak_inflight: number
}
function labStats(): LabOverlayStats | undefined {
  return (globalThis as { __WISP_LAB_OVERLAY__?: LabOverlayStats }).__WISP_LAB_OVERLAY__
}

/**
 * WAAPI 动画结束；回调没有到达时（窗口被遮挡、动画被取消）最多再等一个时长。兜底只在动画没有被暂停时生效：被 pause 的
 * 动画（QA 场景把飞入定格在一半截中间帧）到点后重新计时，恢复播放之后照常由 finished 或下一次兜底结束——否则定格约
 * 0.47 s 后整个 Overlay 就开始淡出，中间帧截不到。产品里没有谁会暂停这些动画，兜底的语义不变
 */
function settle(a: Animation, ms: number): Promise<void> {
  let timer: ReturnType<typeof setTimeout> | undefined
  const fallback = new Promise<void>((resolve) => {
    const arm = () => {
      timer = setTimeout(() => (a.playState === 'paused' ? arm() : resolve()), ms * 2)
    }
    arm()
  })
  return Promise.race([a.finished.then(noop, noop), fallback]).finally(() => clearTimeout(timer))
}
const noop = (): void => {}

/** `promise` 在 `ms` 之内落定就取它的值，否则 null（迟到的位图到达后立即释放） */
function settleWithin(
  promise: Promise<ImageBitmap | null>,
  ms: number,
): Promise<ImageBitmap | null> {
  let late = false
  return Promise.race([
    promise.then((b) => {
      if (late) b?.close()
      return b
    }),
    new Promise<null>((resolve) =>
      setTimeout(() => {
        late = true
        resolve(null)
      }, ms),
    ),
  ])
}

/**
 * 文字没变就不写；变了只改已有文本节点的 data（写 textContent 会把文本节点整个换掉：移出 / 插入布局树，每帧都要多做
 * 一次样式与布局）
 */
function text(node: HTMLElement, value: string): void {
  const first = node.firstChild
  if (first instanceof Text) {
    if (first.data !== value) first.data = value
  } else node.textContent = value
}
/** transform 没变就不写 */
function move(node: HTMLElement, value: string): void {
  if (node.style.transform !== value) node.style.transform = value
}
/**
 * 显示 / 隐藏用 visibility（data-off），不用 hidden / display：合成层一直在，开始拖拽、离开窗口时不重建图层、不重新布局
 * （实测 display 切换会在开始拖拽那一帧带来约 3 ms 布局 + 3 ms 图层提交）
 */
function show(node: HTMLElement, on: boolean): void {
  if (node.hasAttribute('data-off') === on) node.toggleAttribute('data-off', !on)
}

function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls = ''): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag)
  node.className = cls
  return node
}

export function attach(host: OverlayHost): Interaction {
  const { root, dim, signal } = host
  const bound = { signal }
  const style = el('style')
  // 完整的设计令牌随交互模块注入（Overlay 入口只加载首帧子集 tokens-overlay.css）：选区的强调色、标签字体……
  style.textContent = tokens + css
  document.head.append(style)
  root.setAttribute('role', 'application')
  // 界面语言与平台：boot 已写在 <html lang> / data-os（交互模块不引用 IPC 模块，见 host.ts）
  const msgs = document.documentElement.lang === 'en-US' ? en : zh
  root.setAttribute('aria-label', msgs.a11y.region)
  // 尺寸标签的单位按平台：macOS 叫 pt，Windows / Linux 叫 px（数值都是逻辑单位，两种叫法只是写法不同）
  const sizeTemplate = document.documentElement.dataset.os === 'macos' ? msgs.size_pt : msgs.size_px
  const sizeText = (width: number, height: number) =>
    sizeTemplate.replace('{width}', String(width)).replace('{height}', String(height))

  // 选区：零尺寸的容器，边框的八段（外侧描边四段 + 强调色四段，顺序同 edgeRects）、四个把手与尺寸标签各自用 transform 定位
  const sel = el('div', 'ov-sel')
  const edges = [
    ...Array.from({ length: 4 }, () => el('b', 'ov-ring')),
    ...Array.from({ length: 4 }, () => el('b', 'ov-e')),
  ]
  const handles = [el('i'), el('i'), el('i'), el('i')]
  const size = el('span', 'ov-size')
  sel.append(...edges, ...handles, size)
  // 暗化层挖洞：洞四周的四块。交互模块加载后暗化层自身透明、一直由这四块铺出（没有洞时上面一块铺满整屏），
  // 之后只改 transform
  const bands = [el('b', 'ov-band'), el('b', 'ov-band'), el('b', 'ov-band'), el('b', 'ov-band')]
  for (const node of [...edges, ...bands]) move(node, NONE)
  dim.append(...bands)
  const mag = el('div', 'ov-mag')
  const lensEl = el('canvas', 'ov-lens')
  const coord = el('span', 'ov-coord')
  mag.append(lensEl, coord)
  show(sel, false)
  show(mag, false)
  // 尺寸标签先排一次版（隐藏中照常排版）：第一次出现「×」时的字形 / 回退字体查找（Chromium 实测约 3 ms 的布局）
  // 发生在交互模块加载时，而不是用户第一次悬停到窗口上的那一帧
  text(size, sizeText(0, 0))
  root.append(sel, mag)
  const lens = lensEl.getContext('2d')
  signal.addEventListener('abort', () => {
    style.remove()
    for (const b of bands) b.remove()
    dim.removeAttribute('data-bands')
  })

  let init: OverlayInit | null = null
  let mode: Mode = 'idle'
  let w = 0
  let h = 0
  let kx = 1
  let ky = 1
  /** 当前指针（已夹在显示器内）；指针离开窗口后为 null */
  let p: Point | null = null
  let anchor: Point = { x: 0, y: 0 }
  let moved = false
  let drag: Rect | null = null
  let pan: { from: Point; rect: Rect } | null = null
  let pending: Shown | null = null
  let hover: Rect | null = null
  /** 按下时高亮着的窗口（位移还没超过单击容差时继续显示它） */
  let press: Rect | null = null
  /** 已 commit、还在等 Rust 回应（这期间 Esc 收起 Overlay） */
  let committing = false
  /** 飞入中的那块像素（会话结束时移除并释放） */
  let piece: HTMLCanvasElement | null = null
  let source: CanvasImageSource | null = null
  /** macOS：放大镜用的全分辨率位图（会话结束时 close；只在没有 region_url 时拉取） */
  let full: ImageBitmap | null = null
  /** macOS 原生冻结帧的放大镜小块（键 `tx,ty`，Map 的顺序即最近使用；会话结束时全部 close）。null = 不用小块 */
  let tiles: Map<string, Tile> | null = null
  /** 第一块小块已经到达（之前放大镜隐藏，除非底图已到） */
  let tileReady = false
  /** 在路上的小块数（上限 TILE_INFLIGHT） */
  let inflight = 0
  /** 整屏的低分辨率底图（小块没到的部分先画它；会话结束时 close） */
  let overview: ImageBitmap | null = null
  let accent = ''
  let frame = 0
  /** 本模块启动的动画（飞入、淡出；fill: forwards）：新会话开始时取消，预建的 Overlay 复用时回到初始状态 */
  let anims: Animation[] = []
  /** 上一帧写进 DOM 的选区（含粗细）：没变就不写 */
  let lastKey = ''

  const schedule = (): void => {
    frame ||= requestAnimationFrame(render)
  }
  const point = (e: MouseEvent): Point => ({
    x: clamp(e.clientX, 0, w),
    y: clamp(e.clientY, 0, h),
  })
  const flag = (name: string, on: boolean): void => {
    root.toggleAttribute(name, on)
  }
  const snap = (r: Rect): Rect => snapRect(clampRect(r, w, h), init?.monitor.scale_factor ?? 1)
  /** 吸附后的选区够不够大：拖拽的选区至少 8 × 8 逻辑 px，窗口 / 全屏至少 1 px */
  const big = (r: Rect, kind: OverlaySelectionKind): boolean => {
    const min = kind === 'drag' ? MIN_SELECTION : 1
    return r.width >= min && r.height >= min
  }

  /** 现在要画的选区：拖拽中 > ⌘A / 已定格 > 悬停的窗口 */
  function shown(): Shown | null {
    if (mode === 'drag') {
      if (moved && drag) return { rect: drag, kind: 'drag' }
      // 按下后还没超过单击容差：继续显示按下前的选区 / 窗口高亮（不闪）
      return pending ?? (press ? { rect: press, kind: 'window' } : null)
    }
    if (pending) return pending
    return mode === 'hover' && hover ? { rect: hover, kind: 'window' } : null
  }

  /** 选区（含挖洞）写到 DOM：只写 transform */
  function place(r: Rect, t: number, dpr: number): void {
    edgeTransforms(r, t, dpr).forEach((v, i) => move(edges[i] as HTMLElement, v))
    bandTransforms(r, w, h).forEach((v, i) => move(bands[i] as HTMLElement, v))
    const x1 = r.x + r.width
    const y1 = r.y + r.height
    const corners: Array<[number, number]> = [
      [r.x - 4, r.y - 4],
      [x1 - 3, r.y - 4],
      [r.x - 4, y1 - 3],
      [x1 - 3, y1 - 3],
    ]
    corners.forEach(([x, y], i) => move(handles[i] as HTMLElement, `translate(${x}px,${y}px)`))
    // 尺寸标签：默认在选区左上角外侧；上方放不下时放进内部，靠右边时右对齐
    const right = r.x + 120 > w
    sel.toggleAttribute('data-in', r.y < 28)
    sel.toggleAttribute('data-r', right)
    move(size, `translate(${right ? x1 : r.x}px,${r.y}px)`)
    // 逻辑单位；放大镜的坐标才是物理像素。OCR 面板头部显示的同样是逻辑尺寸（物理像素在它的 tooltip 里）
    text(size, sizeText(Math.round(r.width), Math.round(r.height)))
  }

  function render(): void {
    frame = 0
    const cur = shown()
    const r = cur ? snap(cur.rect) : null
    const t = cur?.kind === 'window' ? 2 : 1
    // 线宽按引擎实际的设备像素比取整（长度与它无关，见 geometry.ts）；它变了（换到另一块缩放不同的显示器）也要重画
    const dpr = devicePixelRatio || 1
    const key = r ? `${r.x},${r.y},${r.width},${r.height},${t},${dpr}` : `none,${w},${h}`
    if (key !== lastKey) {
      lastKey = key
      // 第一次画（尺寸已知）起暗化层改由四块铺出
      dim.toggleAttribute('data-bands', true)
      if (r) place(r, t, dpr)
      // 没有洞：上面一块铺满整屏
      else bandTransforms(null, w, h).forEach((v, i) => move(bands[i] as HTMLElement, v))
    }
    if (cur && sel.dataset.k !== cur.kind) sel.dataset.k = cur.kind
    show(sel, cur !== null)
    const at = p
    if (tiles && at && (mode === 'hover' || mode === 'drag')) ensureTiles(at)
    const lensOn = !!(
      (source || tileReady || overview) &&
      at &&
      (mode === 'hover' || mode === 'drag')
    )
    show(mag, lensOn)
    if (lensOn && at) {
      const avoid = mode === 'drag' && moved && r ? r : null
      const m = magnifierAt(at, w, h, avoid)
      move(mag, `translate(${m.x}px,${m.y}px)`)
      drawLens(at)
    }
  }

  /** 小块 `(tx, ty)` 在帧里的矩形（物理像素；右 / 下边缘的小块按帧尺寸截短） */
  function tileRect(tx: number, ty: number): Rect {
    const iw = init?.image_size.width ?? 0
    const ih = init?.image_size.height ?? 0
    const x = tx * TILE
    const y = ty * TILE
    return { x, y, width: Math.min(TILE, iw - x), height: Math.min(TILE, ih - y) }
  }

  /**
   * 采样窗口（加预取余量）覆盖到的小块，没有的就去拉（见文件头）：先拉采样窗口本身覆盖的，再拉预取余量里的；在路上的
   * 达到上限时这一帧不再发新的请求（小块到达时会重画一帧、再补）
   */
  function ensureTiles(at: Point): void {
    const cur = init
    const map = tiles
    const base = cur?.region_url
    if (!cur || !map || !base) return
    const half = MAG_SAMPLES / 2
    const px = Math.floor(at.x * kx)
    const py = Math.floor(at.y * ky)
    const maxX = Math.floor((cur.image_size.width - 1) / TILE)
    const maxY = Math.floor((cur.image_size.height - 1) / TILE)
    const span = (c: number, pad: number, max: number): [number, number] => [
      clamp(Math.floor((c - half - pad) / TILE), 0, max),
      clamp(Math.floor((c + half + pad) / TILE), 0, max),
    ]
    for (const pad of [0, TILE_MARGIN]) {
      const [tx0, tx1] = span(px, pad, maxX)
      const [ty0, ty1] = span(py, pad, maxY)
      for (let ty = ty0; ty <= ty1; ty++) {
        for (let tx = tx0; tx <= tx1; tx++) {
          const k = `${tx},${ty}`
          const have = map.get(k)
          if (have !== undefined) {
            // 最近使用：移到末尾
            map.delete(k)
            map.set(k, have)
            continue
          }
          if (inflight >= TILE_INFLIGHT) continue
          const r = tileRect(tx, ty)
          if (r.width <= 0 || r.height <= 0) continue
          requestTile(map, k, `${base}/${r.x}/${r.y}/${r.width}/${r.height}`)
        }
      }
    }
    pruneTiles(map)
  }

  function requestTile(map: Map<string, Tile>, k: string, url: string): void {
    map.set(k, 'loading')
    inflight++
    const lab = labStats()
    if (lab) lab.peak_inflight = Math.max(lab.peak_inflight, inflight)
    const t0 = performance.now()
    host.load(url).then(
      (b) => {
        if (tiles !== map) return b.close()
        inflight--
        labStats()?.tile_ms.push(Math.round((performance.now() - t0) * 10) / 10)
        map.set(k, b)
        tileReady = true
        pruneTiles(map)
        schedule()
      },
      () => {
        // 会话结束时的中止、或这块拿不到（只缺放大镜的这一块）：不重试，画底图
        if (tiles !== map) return
        inflight--
        map.set(k, 'failed')
        schedule()
      },
    )
  }

  /** 整屏的低分辨率底图（见文件头） */
  function requestOverview(cur: OverlayInit, map: Map<string, Tile>): void {
    const { width, height } = cur.image_size
    if (!cur.region_url || width <= 0 || height <= 0) return
    const t0 = performance.now()
    host.load(`${cur.region_url}/0/0/${width}/${height}/${OVERVIEW_PX}`).then(
      (b) => {
        if (tiles !== map) return b.close()
        const lab = labStats()
        if (lab) lab.overview_ms = Math.round(performance.now() - t0)
        overview = b
        schedule()
      },
      // 只缺底图：小块没到的部分画黑
      () => {},
    )
  }

  /** 超过缓存上限时从最久没用的开始淘汰（在路上的保留） */
  function pruneTiles(map: Map<string, Tile>): void {
    for (const [k, t] of map) {
      if (map.size <= TILE_CACHE) break
      if (t === 'loading') continue
      if (t !== 'failed') t.close()
      map.delete(k)
    }
  }

  /** 释放全部小块与底图 */
  function dropTiles(): void {
    if (tiles) for (const t of tiles.values()) if (t !== 'loading' && t !== 'failed') t.close()
    tiles = null
    tileReady = false
    inflight = 0
    overview?.close()
    overview = null
  }

  /**
   * 把采样窗口（左上角 `(sx, sy)`、边长 MAG_SAMPLES 物理像素）画进放大镜（每格 `cell` 个设备像素）：先画底图放大的样子，
   * 再把已到达的小块画在上面。返回采样窗口在帧内的部分是否都有清晰的小块（`partial` = 有的地方只有底图，`black` = 底图
   * 也还没到）
   */
  function drawTiles(
    ctx: CanvasRenderingContext2D,
    sx: number,
    sy: number,
    cell: number,
  ): { partial: boolean; black: boolean } {
    let partial = false
    if (!tiles) return { partial, black: false }
    const iw = init?.image_size.width ?? 0
    const ih = init?.image_size.height ?? 0
    const ex = sx + MAG_SAMPLES
    const ey = sy + MAG_SAMPLES
    if (overview && iw > 0 && ih > 0) {
      // 帧内的那部分（帧外与屏幕边缘之外一致，留黑）
      const fx0 = Math.max(0, sx)
      const fy0 = Math.max(0, sy)
      const fx1 = Math.min(iw, ex)
      const fy1 = Math.min(ih, ey)
      if (fx1 > fx0 && fy1 > fy0) {
        const ox = overview.width / iw
        const oy = overview.height / ih
        const dx = Math.round((fx0 - sx) * cell)
        const dy = Math.round((fy0 - sy) * cell)
        // 底图放大时平滑（读起来是「还在加载的模糊」，不是马赛克方块）；小块照旧逐像素
        ctx.imageSmoothingEnabled = true
        ctx.drawImage(
          overview,
          fx0 * ox,
          fy0 * oy,
          Math.max(1e-3, (fx1 - fx0) * ox),
          Math.max(1e-3, (fy1 - fy0) * oy),
          dx,
          dy,
          Math.round((fx1 - sx) * cell) - dx,
          Math.round((fy1 - sy) * cell) - dy,
        )
        ctx.imageSmoothingEnabled = false
      }
    }
    for (let ty = Math.floor(Math.max(0, sy) / TILE); ty * TILE < ey && ty * TILE < ih; ty++) {
      for (let tx = Math.floor(Math.max(0, sx) / TILE); tx * TILE < ex && tx * TILE < iw; tx++) {
        const t = tiles.get(`${tx},${ty}`)
        if (t === undefined || t === 'loading' || t === 'failed') {
          partial = true
          continue
        }
        const r = tileRect(tx, ty)
        const ix0 = Math.max(sx, r.x)
        const iy0 = Math.max(sy, r.y)
        const ix1 = Math.min(ex, r.x + r.width)
        const iy1 = Math.min(ey, r.y + r.height)
        if (ix1 <= ix0 || iy1 <= iy0) continue
        const dx = Math.round((ix0 - sx) * cell)
        const dy = Math.round((iy0 - sy) * cell)
        ctx.drawImage(
          t,
          ix0 - r.x,
          iy0 - r.y,
          ix1 - ix0,
          iy1 - iy0,
          dx,
          dy,
          Math.round((ix1 - sx) * cell) - dx,
          Math.round((iy1 - sy) * cell) - dy,
        )
      }
    }
    return { partial, black: partial && !overview }
  }

  /** 放大镜：14×14 物理像素 ×8，网格 + 十字（光标所在像素描边），下方显示物理坐标 */
  function drawLens(at: Point): void {
    if (!lens || !(source || tiles)) return
    // 像素尺寸 = 112 px × 当前设备像素比（125% / 150% / 175% 下是 140 / 168 / 196，每格整数个设备像素）。按画的这一刻取：
    // 预建的 Overlay 在缩放不同的显示器之间复用时，begin 那一刻的 devicePixelRatio 可能还是上一块屏的
    const want = Math.round(MAG_SIZE * (devicePixelRatio || 1))
    if (lensEl.width !== want) lensEl.width = lensEl.height = want
    const n = lensEl.width
    const cell = n / MAG_SAMPLES
    const half = MAG_SAMPLES / 2
    const px = Math.floor(at.x * kx)
    const py = Math.floor(at.y * ky)
    lens.imageSmoothingEnabled = false
    lens.globalAlpha = 1
    lens.fillStyle = '#000'
    lens.fillRect(0, 0, n, n)
    if (source) lens.drawImage(source, px - half, py - half, MAG_SAMPLES, MAG_SAMPLES, 0, 0, n, n)
    else {
      const cover = drawTiles(lens, px - half, py - half, cell)
      const lab = labStats()
      if (lab) {
        lab.lens++
        if (cover.partial) lab.partial++
        if (cover.black) lab.black++
      }
    }
    lens.fillStyle = 'rgb(128 128 128 / .3)'
    for (let i = 1; i < MAG_SAMPLES; i++) {
      const o = Math.round(i * cell)
      lens.fillRect(o, 0, 1, n)
      lens.fillRect(0, o, n, 1)
    }
    const c = Math.round(half * cell)
    const s = Math.round(cell)
    lens.globalAlpha = 0.25
    lens.fillStyle = accent
    lens.fillRect(c, 0, s, n)
    lens.fillRect(0, c, n, s)
    lens.globalAlpha = 1
    lens.strokeStyle = accent
    lens.lineWidth = Math.max(2, Math.round(n / MAG_SIZE))
    lens.strokeRect(c, c, s, s)
    text(coord, `${px}, ${py}`)
  }

  /**
   * 选区定格并确认：commit → 飞入 → 淡出 → done。选区太小时什么也不做、返回 false（状态不变，由调用方决定怎么收场）
   */
  function finish(rect: Rect, kind: OverlaySelectionKind): boolean {
    const r = snap(rect)
    const cur = init
    if (!cur || !big(r, kind)) return false
    void confirm(cur, r, kind)
    return true
  }

  /** 选区在冻结帧里的物理像素矩形（夹在帧内） */
  function physRect(r: Rect): Rect {
    const iw = init?.image_size.width ?? 1
    const ih = init?.image_size.height ?? 1
    const x = clamp(Math.round(r.x * kx), 0, iw - 1)
    const y = clamp(Math.round(r.y * ky), 0, ih - 1)
    return {
      x,
      y,
      width: clamp(Math.round(r.width * kx), 1, iw - x),
      height: clamp(Math.round(r.height * ky), 1, ih - y),
    }
  }

  /**
   * macOS 原生冻结帧：飞入用的那块像素按选区拉取（与 overlay_commit 并行；Rust 把大块降采样到约 4 MP）。拿不到时为 null
   * （只飞边框）
   */
  function loadPiece(cur: OverlayInit, r: Rect): Promise<ImageBitmap | null> {
    const ph = physRect(r)
    return host.load(`${cur.region_url}/${ph.x}/${ph.y}/${ph.width}/${ph.height}`).then(
      (b) => {
        if (init === cur) return b
        b.close()
        return null
      },
      () => null,
    )
  }

  /**
   * 从冻结帧（Windows / X11 的 canvas，或 macOS 的全分辨率位图）裁出选区那一块，放进独立的合成层（飞入用）。`whole`：
   * `img` 本身就是选区那一块（macOS 按需拉取的预览，可能已降采样）及其像素尺寸，整张画进去
   */
  function cutout(
    r: Rect,
    img: CanvasImageSource,
    whole: { width: number; height: number } | null = null,
  ): HTMLCanvasElement {
    const ph = physRect(r)
    const sx = whole ? 0 : ph.x
    const sy = whole ? 0 : ph.y
    const sw = whole ? whole.width : ph.width
    const sh = whole ? whole.height : ph.height
    const k = Math.min(1, Math.sqrt(PIECE_MAX_PX / (sw * sh)))
    const c = el('canvas', 'ov-piece')
    c.width = Math.max(1, Math.round(sw * k))
    c.height = Math.max(1, Math.round(sh * k))
    c.style.width = `${r.width}px`
    c.style.height = `${r.height}px`
    c.style.transform = tf(r.x, r.y, 1, 1)
    c.getContext('2d')?.drawImage(img, sx, sy, sw, sh, 0, 0, c.width, c.height)
    // 暗化层之上、边框之下
    root.insertBefore(c, sel)
    return c
  }
  function dropPiece(): void {
    if (!piece) return
    piece.width = piece.height = 0
    piece.remove()
    piece = null
  }

  async function confirm(cur: OverlayInit, r: Rect, kind: OverlaySelectionKind): Promise<void> {
    mode = 'done'
    pending = { rect: r, kind }
    pan = null
    flag('data-done', true)
    flag('data-pan', false)
    cancelAnimationFrame(frame)
    render()
    show(mag, false)
    let to: Rect | null
    committing = true
    // macOS 原生冻结帧、没有整屏位图：飞入的那块像素与 commit 并行拉取（见文件头）
    const flies = cur.fly_animation && !cur.reduced_motion
    const pieceP = flies && cur.frozen_native && !full && cur.region_url ? loadPiece(cur, r) : null
    // 飞入那块交给 settleWithin 之后由它负责（迟到的自己释放，按时到的画完即释放）；之前的每条提前返回（commit 失败、
    // 会话已换、没有飞入目标）都在 finally 里等它到达后立即 close（见文件头）
    let pieceTaken = false
    try {
      let timer: ReturnType<typeof setTimeout> | undefined
      try {
        const ack = await Promise.race([
          host.commit({ session_id: cur.session_id, monitor_id: cur.monitor.id, rect: r, kind }),
          new Promise<never>((_, reject) => {
            timer = setTimeout(
              () => reject(new Error('overlay_commit timed out')),
              COMMIT_TIMEOUT_MS,
            )
          }),
        ])
        to = ack.fly_to
      } catch (error) {
        console.error('[overlay] overlay_commit failed', error)
        if (init === cur) host.cancel()
        return
      } finally {
        clearTimeout(timer)
        committing = false
      }
      if (init !== cur) return
      if (to && cur.fly_animation && !cur.reduced_motion) {
        const opts: KeyframeAnimationOptions = {
          duration: tokenMs('--dur-base'),
          easing: token('--ease-standard') || 'linear',
          fill: 'forwards',
        }
        flag('data-fly', true)
        // 飞的是选中的像素本身（shared element）：裁出的那块与四条边同一时长、同一曲线、同一种函数列表（translate +
        // scale，等比），每一帧边框都贴着那块像素；终点是面板头部缩略图的位置。暗化层整体淡出，洞留在原处不动
        const land = landingRect(r, to)
        const t = kind === 'window' ? 2 : 1
        const dpr = devicePixelRatio || 1
        const e0 = edgeTransforms(r, t, dpr)
        const e1 = edgeTransforms(land, t, dpr)
        edges.forEach((node, i) =>
          anims.push(node.animate([{ transform: e0[i] }, { transform: e1[i] }], opts)),
        )
        // Windows / X11：冻结帧 canvas（全分辨率帧还没到时就是放大的 1/2 帧，与屏幕上一致）；macOS：按选区拉取的那块
        // （commit 回应之后最多再等 PIECE_WAIT_MS），或旧路径的全分辨率位图（都没有时只飞边框）
        pieceTaken = true
        const regionPiece = pieceP ? await settleWithin(pieceP, PIECE_WAIT_MS) : null
        if (init !== cur) {
          regionPiece?.close()
          return
        }
        const img = cur.frozen_native ? (regionPiece ?? full) : host.frame
        if (img) {
          piece = cutout(r, img, img === regionPiece ? regionPiece : null)
          regionPiece?.close()
          const s = land.width / Math.max(1e-6, r.width)
          anims.push(
            piece.animate(
              [{ transform: tf(r.x, r.y, 1, 1) }, { transform: tf(land.x, land.y, s, s) }],
              opts,
            ),
          )
        }
        anims.push(dim.animate([{ opacity: 1 }, { opacity: 0 }], opts))
        await settle(anims[anims.length - 1] as Animation, opts.duration as number)
        if (init !== cur) return
      }
      // 减少动效时 --dur-base 为 100 ms（≤ 100 ms 的淡出）
      const out = tokenMs(cur.reduced_motion ? '--dur-base' : '--dur-fast')
      const fade = root.animate([{ opacity: 1 }, { opacity: 0 }], {
        duration: out,
        easing: token('--ease-exit') || 'linear',
        fill: 'forwards',
      })
      anims.push(fade)
      await settle(fade, out)
      if (init === cur) host.done()
    } finally {
      if (pieceP && !pieceTaken) void pieceP.then((b) => b?.close())
    }
  }

  /** 拖拽 / 选中的状态清空，回到悬停 */
  function clearSelection(): void {
    mode = 'hover'
    drag = pending = pan = press = null
    moved = false
    flag('data-pan', false)
    schedule()
  }

  /** 右键（单独按下，或拖拽中的组合按键） */
  function secondary(): void {
    if (mode === 'drag' || pending) clearSelection()
    else host.cancel()
  }

  const live = (): boolean => mode === 'hover' || mode === 'drag'

  // ---- 指针（Overlay 窗口 = 一个显示器；拖出窗口时按窗口边界夹住，即限制在起始显示器内）----
  const onRoot = <K extends keyof HTMLElementEventMap>(
    type: K,
    fn: (e: HTMLElementEventMap[K]) => void,
  ) => root.addEventListener(type, fn, bound)
  onRoot('pointerdown', (e) => {
    if (!live()) return
    e.preventDefault()
    if (e.button === 2) return secondary()
    if (e.button) return
    anchor = p = point(e)
    drag = null
    moved = false
    press = mode === 'hover' ? hover : null
    mode = 'drag'
    try {
      root.setPointerCapture(e.pointerId)
    } catch {
      // 合成事件（测试 / mock 场景）没有真实指针
    }
  })
  onRoot('pointermove', (e) => {
    if (!live()) return
    // 左键按住时再按右键：组合按键只有 pointermove（button = 2，buttons 带上右键位）
    if (e.button === 2 && e.buttons & 2) {
      secondary()
      return
    }
    const at = (p = point(e))
    if (mode === 'drag') {
      if (!moved && Math.hypot(at.x - anchor.x, at.y - anchor.y) >= CLICK_SLOP) {
        moved = true
        pending = press = null
      }
      if (moved) {
        drag = pan
          ? shiftRect(pan.rect, at.x - pan.from.x, at.y - pan.from.y, w, h)
          : rectOf(anchor, at)
      }
    } else hover = init?.window_detect ? hitWindow(init.windows, at) : null
    schedule()
  })
  onRoot('pointerup', (e) => {
    // 所有按键都松开才算结束（组合按键松开其中一个时只有 pointermove）
    if (mode !== 'drag' || e.buttons) return
    const at = (p = point(e))
    if (moved && drag && finish(drag, 'drag')) return
    // 单击，或拖出来的选区太小（按单击处理）：选中光标下的窗口；否则回到悬停——单击桌面（或没开窗口检测）时保留 ⌘A
    // 的选区，太小的拖拽连同它一起清掉。无论哪种都离开拖拽状态（没有按键按着时选区不能再跟着指针走）
    if (moved) clearSelection()
    else {
      mode = 'hover'
      press = null
    }
    const win = init?.window_detect ? hitWindow(init.windows, at) : null
    if (win && finish(win, 'window')) return
    hover = win
    schedule()
  })
  onRoot('pointercancel', () => {
    if (mode === 'drag') clearSelection()
  })
  onRoot('pointerleave', () => {
    if (mode !== 'hover') return
    p = hover = null
    schedule()
  })
  onRoot('contextmenu', (e) => e.preventDefault())

  // ---- 键盘（Overlay 是非激活的 key 面板，按键直接到这里）----
  addEventListener(
    'keydown',
    (e) => {
      if (e.isComposing) return
      if (e.key === 'Escape' && mode === 'done') {
        // 请求已在 commit 时发出、面板就在下面：Esc 只提前收起 Overlay（等回应时 cancel，飞入 / 淡出中直接 done）
        e.preventDefault()
        if (committing) host.cancel()
        else host.done()
        return
      }
      if (!live()) return
      const mod = document.documentElement.dataset.os === 'macos' ? e.metaKey : e.ctrlKey
      if (e.key === 'Escape') host.cancel()
      else if (e.key === 'Enter') {
        // 选区太小时不提交（拖拽中继续拖）
        const cur = shown()
        if (cur) finish(cur.rect, cur.kind)
      } else if (e.code === 'KeyA' && mod) {
        mode = 'hover'
        drag = press = null
        moved = false
        pending = { rect: { x: 0, y: 0, width: w, height: h }, kind: 'fullscreen' }
        schedule()
      } else if (e.code === 'Space') {
        if (!e.repeat && mode === 'drag' && moved && drag && p) {
          pan = { from: p, rect: drag }
          flag('data-pan', true)
        }
      } else return
      e.preventDefault()
    },
    bound,
  )
  addEventListener(
    'keyup',
    (e) => {
      if (e.code !== 'Space' || !pan) return
      if (drag && p) anchor = farCorner(drag, p)
      pan = null
      flag('data-pan', false)
    },
    bound,
  )

  return {
    begin(next) {
      cancelAnimationFrame(frame)
      frame = 0
      init = next
      w = next.monitor.frame.width
      h = next.monitor.frame.height
      kx = w > 0 ? next.image_size.width / w : next.monitor.scale_factor
      ky = h > 0 ? next.image_size.height / h : next.monitor.scale_factor
      full?.close()
      p = drag = pan = pending = hover = press = source = full = null
      dropTiles()
      moved = committing = false
      mode = 'hover'
      for (const a of anims) a.cancel()
      anims = []
      dropPiece()
      lastKey = ''
      for (const name of ['data-done', 'data-pan', 'data-fly']) flag(name, false)
      accent = token('--accent') || '#3b82f6'
      lensEl.width = lensEl.height = Math.round(MAG_SIZE * (devicePixelRatio || 1))
      render()
    },
    shown(fullFrame) {
      const cur = init
      if (!cur) return
      // 冻结帧本身就是全分辨率帧（没有 half_url）：直接用它，不再拉取、解码第二次（5K 屏上约 59 MB）
      if (fullFrame && !cur.frozen_native) {
        source = host.frame
        return schedule()
      }
      // macOS 原生冻结帧：放大镜按需拉小块，不拉整屏帧（见文件头）
      if (cur.frozen_native && cur.region_url) {
        const map = new Map<string, Tile>()
        tiles = map
        requestOverview(cur, map)
        return schedule()
      }
      host.load(cur.image_url).then(
        (b) => {
          if (init !== cur) return b.close()
          if (cur.frozen_native) source = full = b
          else {
            host.paint(b)
            b.close()
            source = host.frame
          }
          schedule()
        },
        // 只缺放大镜：照常框选
        (e: unknown) => console.warn('[overlay] full frame', e),
      )
    },
    end() {
      mode = 'idle'
      dropPiece()
      full?.close()
      dropTiles()
      init = source = full = p = null
      committing = false
      cancelAnimationFrame(frame)
      frame = 0
      show(mag, false)
      // 空闲时（预建的 Overlay 等下一次会话）不留放大镜的缓冲区；begin 时按显示器缩放重新分配
      lensEl.width = lensEl.height = 0
    },
  }
}
