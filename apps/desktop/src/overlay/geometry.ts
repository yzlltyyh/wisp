/**
 * Overlay 的纯几何计算。不碰 DOM，便于单测。
 *
 * 坐标系：Overlay 窗口铺满一个显示器，指针的 clientX / clientY 就是**本显示器内的逻辑坐标**（与 `OverlayCommit.rect`、
 * `OverlayInit.windows`、`OverlayCommitAck.fly_to` 同一坐标系）；冻结帧是物理像素（`image_size`）。
 */
import type { Point, Rect } from '../shared/ipc/overlay'

/** 位移小于它算「单击」（< 4 px 选中光标下的窗口） */
export const CLICK_SLOP = 4
/**
 * 拖拽选区的最小边长（逻辑 px）：宽或高不到它时按单击处理——这么小的选区几乎一定是误触（纯竖直 / 水平拖动、拖出去又
 * 拖回来），不能发起一次计费的识别请求
 */
export const MIN_SELECTION = 8
/** 放大镜：采样 14×14 物理像素、放大 8 倍 = 112 px */
export const MAG_SAMPLES = 14
export const MAG_ZOOM = 8
export const MAG_SIZE = MAG_SAMPLES * MAG_ZOOM
/** 放大镜在光标右下方的距离（16 px，靠边翻转） */
export const MAG_OFFSET = 16
/** 放大镜下方坐标标签占的高度（含间距） */
export const MAG_LABEL = 26

export const clamp = (v: number, lo: number, hi: number): number => Math.min(hi, Math.max(lo, v))

/** 两点张成的矩形（任意方向拖拽） */
export function rectOf(a: Point, b: Point): Rect {
  return {
    x: Math.min(a.x, b.x),
    y: Math.min(a.y, b.y),
    width: Math.abs(a.x - b.x),
    height: Math.abs(a.y - b.y),
  }
}

/**
 * 把矩形的四条边对齐到物理像素（scale = 本显示器的缩放）：Rust 按 scale_factor 换算成物理坐标裁剪，
 * 对齐之后裁出来的就是屏幕上框住的那些像素（Retina / 150% 等混合缩放下没有半像素偏移）
 */
export function snapRect(r: Rect, scale: number): Rect {
  const s = scale > 0 ? scale : 1
  const x0 = Math.round(r.x * s) / s
  const y0 = Math.round(r.y * s) / s
  const x1 = Math.round((r.x + r.width) * s) / s
  const y1 = Math.round((r.y + r.height) * s) / s
  return { x: x0, y: y0, width: x1 - x0, height: y1 - y0 }
}

/** 与显示器（0, 0, w, h）求交；完全在外时宽高为 0 */
export function clampRect(r: Rect, w: number, h: number): Rect {
  const x0 = clamp(r.x, 0, w)
  const y0 = clamp(r.y, 0, h)
  const x1 = clamp(r.x + r.width, 0, w)
  const y1 = clamp(r.y + r.height, 0, h)
  return { x: x0, y: y0, width: x1 - x0, height: y1 - y0 }
}

/** 平移（空格按住时整个选区跟随），尺寸不变、不越出显示器 */
export function shiftRect(r: Rect, dx: number, dy: number, w: number, h: number): Rect {
  return {
    ...r,
    x: clamp(r.x + dx, 0, Math.max(0, w - r.width)),
    y: clamp(r.y + dy, 0, Math.max(0, h - r.height)),
  }
}

/** 松开空格后继续调整大小：以离指针最远的那个角为新的锚点 */
export function farCorner(r: Rect, p: Point): Point {
  return {
    x: Math.abs(p.x - r.x) > Math.abs(p.x - (r.x + r.width)) ? r.x : r.x + r.width,
    y: Math.abs(p.y - r.y) > Math.abs(p.y - (r.y + r.height)) ? r.y : r.y + r.height,
  }
}

/** 窗口检测：按 z 序（最上层在前）取第一个包含该点的窗口 */
export function hitWindow(windows: readonly Rect[], p: Point): Rect | null {
  for (const r of windows) {
    if (p.x >= r.x && p.y >= r.y && p.x < r.x + r.width && p.y < r.y + r.height) return r
  }
  return null
}

const intersects = (a: Rect, b: Rect): boolean =>
  a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height

/**
 * 放大镜左上角：默认在光标右下 16 px；右边 / 下边放不下时翻到左边 / 上边，并始终留在显示器内。
 * 给出 `avoid`（拖拽中的选区）时，优先选不压住选区的一侧（例如贴着右边框选时翻到左下，而不是翻进选区里），
 * 实在放不下才进入选区
 */
export function magnifierAt(p: Point, w: number, h: number, avoid: Rect | null = null): Point {
  const boxH = MAG_SIZE + MAG_LABEL
  const right = p.x + MAG_OFFSET
  const left = p.x - MAG_OFFSET - MAG_SIZE
  const below = p.y + MAG_OFFSET
  const above = p.y - MAG_OFFSET - boxH
  const fits = (c: Point) => c.x >= 0 && c.x + MAG_SIZE <= w && c.y >= 0 && c.y + boxH <= h
  const candidates = [
    { x: right, y: below },
    { x: left, y: below },
    { x: right, y: above },
    { x: left, y: above },
  ].filter(fits)
  const clear = avoid
    ? candidates.find((c) => !intersects({ x: c.x, y: c.y, width: MAG_SIZE, height: boxH }, avoid))
    : undefined
  const pick = clear ?? candidates[0]
  if (pick) return pick
  // 显示器比放大镜还小：夹在显示器内
  let x = right
  let y = below
  if (x + MAG_SIZE > w) x = left
  if (y + boxH > h) y = above
  return { x: clamp(x, 0, Math.max(0, w - MAG_SIZE)), y: clamp(y, 0, Math.max(0, h - boxH)) }
}

/** `translate(x, y) scale(sx, sy)`：选区边框与暗化层的每一块都用同一种函数列表，飞入时逐项线性插值 */
export const tf = (x: number, y: number, sx: number, sy: number): string =>
  `translate(${x}px,${y}px) scale(${sx},${sy})`

/**
 * 暗化层挖洞的四块与边框的每一段都是 UNIT × UNIT CSS px 的纯色方块（select.css），用 transform 缩放到位（只改 transform，
 * 交给合成器）。
 *
 * 基准方块不能是 1 px：分数缩放下 1 CSS px 是 1.25 / 1.5 / 1.75 个设备像素，引擎按整设备像素画这个方块（真实设备缩放的
 * Chromium——WebView2 走的就是它——与 WebKit 实测：150% 下画成 2 个设备像素），scale 再把这点误差放大到整条边：150% 下
 * 边框与暗化带都长了 1/3（用户真机：932 物理像素宽的选框，上下两条边画了约 1244），125% 下反而变短、屏幕右侧与下方露出
 * 一条不暗的。UNIT = 100 在任何以 1% 为步长的缩放（Windows 的 100%…500%）下都是整数个设备像素，缩放之后的长度就是精确的，
 * 而且不依赖页面知道自己的 devicePixelRatio（多显示器混合缩放、DPR 还没更新时长度照样对）
 */
export const UNIT = 100

/** 空矩形（用不到的挖洞块：缩成 0） */
const EMPTY: Rect = { x: 0, y: 0, width: 0, height: 0 }

/** 矩形（CSS px）→ 基准方块的 transform */
export const boxTransform = (r: Rect): string => tf(r.x, r.y, r.width / UNIT, r.height / UNIT)

/**
 * 线宽（逻辑 px）取整到设备像素，至少 1 个：分数缩放下 1 px 的描边是 1.5 个设备像素，会糊成两条半透明的线。只取整粗细，
 * 不碰长度与位置（长度、位置由已吸附到物理像素的选区决定）
 */
export function hairline(width: number, dpr: number): number {
  const d = dpr > 0 ? dpr : 1
  return Math.max(1, Math.round(width * d)) / d
}

/**
 * 暗化层挖洞：洞四周的四块（上、下、左、右），洞正好是选区。`r` 为 null 时没有洞：上面一块铺满整屏，其余三块为空。
 * 拖拽只改 transform，不重新光栅化整屏暗化层（clip-path 在 WebKit 上每帧重绘整屏遮罩，5K 屏上尤甚）。选区已按物理像素
 * 对齐，四块的边落在整设备像素上，相邻两块不会在同一行各盖一部分（那样抗锯齿叠加会露出一条亮线）；飞入时洞不动
 */
export function bandRects(r: Rect | null, w: number, h: number): Rect[] {
  if (!r) return [{ x: 0, y: 0, width: w, height: h }, EMPTY, EMPTY, EMPTY]
  const x1 = r.x + r.width
  const y1 = r.y + r.height
  return [
    { x: 0, y: 0, width: w, height: r.y },
    { x: 0, y: y1, width: w, height: Math.max(0, h - y1) },
    { x: 0, y: r.y, width: r.x, height: r.height },
    { x: x1, y: r.y, width: Math.max(0, w - x1), height: r.height },
  ]
}

export const bandTransforms = (r: Rect | null, w: number, h: number): string[] =>
  bandRects(r, w, h).map(boxTransform)

/**
 * 选区边框的八段：前四段是外侧一圈深色描边（上、下、左、右；上下两段含四个角，互不重叠——描边半透明，重叠处会更深），
 * 后四段是强调色，正好盖住选区最外一圈（上、下、左、右）。描边 1 px、强调色 t px（框选 1 / 窗口高亮 2），粗细都取整到
 * 设备像素（`hairline`）、不随选区缩放；长度与位置就是选区本身
 */
export function edgeRects(r: Rect, t: number, dpr: number): Rect[] {
  const { x, y, width: w, height: h } = r
  const x1 = x + w
  const y1 = y + h
  const k = hairline(1, dpr)
  const a = hairline(t, dpr)
  return [
    { x: x - k, y: y - k, width: w + 2 * k, height: k },
    { x: x - k, y: y1, width: w + 2 * k, height: k },
    { x: x - k, y, width: k, height: h },
    { x: x1, y, width: k, height: h },
    { x, y, width: w, height: a },
    { x, y: y1 - a, width: w, height: a },
    { x, y, width: a, height: h },
    { x: x1 - a, y, width: a, height: h },
  ]
}

export const edgeTransforms = (r: Rect, t: number, dpr: number): string[] =>
  edgeRects(r, t, dpr).map(boxTransform)

/** 面板卡片的内边距（--pad-panel-x 16 / --pad-panel-y 12），OCR 视图头部的缩略图就从这里开始 */
const LAND_X = 16
const LAND_Y = 12

/**
 * 面板头部缩略图的落点（飞入的终点）：选区按原比例缩小，落在面板卡片内边距处、高 ≤ 72、宽 ≤ min(240, 内容宽 × 0.55)
 * （窄面板 × 0.38）——与 OCR 视图头部的缩略图（views/ocr/model.ts 的 LAYOUT.thumb 与 thumbMaxWidth）同一规则，
 * 选中的那块截图飞进面板后正好落在同一张图上。`panel` 是 `OverlayCommitAck.fly_to`（面板卡片矩形）
 */
export function landingRect(r: Rect, panel: Rect): Rect {
  const inner = Math.max(1, panel.width - 2 * LAND_X)
  const maxW = Math.min(240, inner * (inner < 400 ? 0.38 : 0.55))
  const s = Math.min(1, 72 / Math.max(1, r.height), maxW / Math.max(1, r.width))
  return { x: panel.x + LAND_X, y: panel.y + LAND_Y, width: r.width * s, height: r.height * s }
}
