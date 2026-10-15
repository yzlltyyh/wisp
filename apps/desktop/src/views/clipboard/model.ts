/**
 * 剪贴板视图的纯逻辑：行模型（「置顶 / 最近」两组，检索结果平铺）、排序与增删、分页合并、
 * 键盘移动、高度、颜色换算、文本工具。不依赖 React / IPC，单测（__tests__/model.test.ts）直接覆盖。
 */
import type { ClipFilter, ClipHit, ClipItemMeta, ClipKind } from '../../shared/ipc/types'

// ---------------------------------------------------------------------------
// 尺寸（逻辑 px）。行高与令牌 --h-row / --h-row-image 一致；头部 / 提示条与 design.* 视觉稿一致

/** 普通行 */
export const ROW_HEIGHT = 44
/** 图片行 */
export const IMAGE_ROW_HEIGHT = 64
/** 分组标题（「置顶 / 最近」，不可选中） */
export const GROUP_HEIGHT = 28
/** VirtualList 首尾的留白（它的 paddingY 默认值） */
export const LIST_PADDING = 4
/** 头部：大号无边框搜索 + 过滤 + ⋯ */
export const HEADER_HEIGHT = 48
/** 底部快捷键提示条（可在设置里隐藏） */
export const HINT_HEIGHT = 36
/**
 * 「剪贴板监听已暂停」横幅所在的条带：上留白 8 + 横幅 40（min-height 32 + 内边距 6 × 2，按钮 28 放得下）+ 下留白 4
 * （列表自己还有 4 的首部留白，横幅到第一行的距离与到头部分隔线的距离都是 8）
 */
export const BANNER_TOP = 8
export const BANNER_BOX = 40
export const BANNER_BOTTOM = 4
export const BANNER_HEIGHT = BANNER_TOP + BANNER_BOX + BANNER_BOTTOM
/** 空状态 / 无结果在列表区里的最小高度 */
export const EMPTY_HEIGHT = 176

/** 剪贴板打开时取 clamp(内容, 240, min(560, 上限))，与 Rust 的 `placement::clipboard_open_height` 同一规则 */
export const OPEN_MAX_HEIGHT = 560
/**
 * 打开高度的下限。面板的通用下限是 240；这里再保证类型过滤菜单（7 项 × 28 + 内边距）在头部下方完整放得下：
 * 菜单在卡片内会「预留高度」让卡片长高，而剪贴板的卡片高度在会话内只增不减（Rust 锁定），长高之后视图就比卡片矮了
 */
export const OPEN_MIN_HEIGHT = HEADER_HEIGHT + 4 + (7 * 28 + 8) + 8

/** 菜单项 / 分隔线 / 菜单内边距的高度（components.css 的 .ui-menu，估算右键菜单是否放得下） */
export const MENU_ITEM_HEIGHT = 28
export const MENU_SEPARATOR_HEIGHT = 9
export const MENU_PADDING = 8

// ---------------------------------------------------------------------------
// 数据量

/** 每页条数（Rust 内存缓存 200 条）；分页同样 200 */
export const PAGE_SIZE = 200
/** 检索结果条数；滚到底还有更多时放大到上限再查一次（`clipboard_search` 的 limit ≤ 500） */
export const SEARCH_LIMIT = 200
export const SEARCH_LIMIT_MAX = 500
/** 输入即搜索的去抖 */
export const SEARCH_DEBOUNCE_MS = 60
/**
 * 少于这么多个字的查询不走全文索引（trigram 至少要 3 个字），只在最近 SHORT_QUERY_SCOPE 条里做子串匹配：
 * 没搜到时提示「多输入几个字可以搜索全部历史」
 */
export const SHORT_QUERY_CHARS = 3
export const SHORT_QUERY_SCOPE = 2000
/** 距离列表末尾不到这么多屏时加载下一页 */
export const PREFETCH_SCREENS = 2
/**
 * 检索覆盖的前缀长度（与 `wisp_clipboard::SEARCH_PREFIX_CHARS` 一致）：
 * 更长的条目在详情里注明「只检索前 N 字」
 */
export const SEARCH_PREFIX_CHARS = 32768
/** 全文预览最多渲染的字数（1 MB 的纯文本一次排版会卡住主线程；粘贴仍是全文） */
export const PREVIEW_RENDER_CHARS = 100_000
/** 全文预览里最多高亮的命中数 */
export const PREVIEW_MAX_MARKS = 500
/**
 * 悬停卡片只显示前 3 行：正文只取开头这么多字再切行（1 MB 的单行压缩 JSON 不会整段进排版），
 * 每行再截到 HOVER_LINE_CHARS 字（3 行 × 340 px 的框最多放下约 200 字，留足余量）
 */
export const HOVER_HEAD_CHARS = 2000
export const HOVER_LINE_CHARS = 300
/**
 * 悬停卡片最多为多长的正文去取开头：`clipboard_get` 返回的是全文（最长 1 MB），取开头也要整段经 IPC 传过来、在主线程
 * 上解析，更长的多行文本悬停时直接用元数据里的预览（≤ 200 字），不取正文（`clipboard_get` 没有按长度截断的版本）。
 * 同时也是详情缓存收不收一条正文的上限
 */
export const LARGE_TEXT_CHARS = 65_536
/** 列表元数据里最多带的文件数（`wisp_clipboard::META_FILES`）：等于它时实际文件数可能更多 */
export const META_FILES = 5

// ---------------------------------------------------------------------------
// 过滤

export const FILTERS: readonly ClipFilter[] = [
  'all',
  'text',
  'image',
  'files',
  'link',
  'code',
  'color',
]

/** 条目是否属于某个过滤（文本包含富文本，与 wisp-clipboard 的 `ClipFilter::Text` 一致） */
export function matchesFilter(kind: ClipKind, filter: ClipFilter): boolean {
  if (filter === 'all') return true
  if (filter === 'text') return kind === 'text' || kind === 'rich_text'
  return kind === filter
}

/** Tab / ⇧Tab 循环切换过滤 */
export function cycleFilter(filter: ClipFilter, step: 1 | -1): ClipFilter {
  const i = FILTERS.indexOf(filter)
  const n = FILTERS.length
  return FILTERS[(((i + step) % n) + n) % n] ?? 'all'
}

// ---------------------------------------------------------------------------
// 排序与增删（顺序与 Rust 一致：置顶在前，最近使用在前，同一毫秒 id 大的在前）

export function compareItems(a: ClipItemMeta, b: ClipItemMeta): number {
  return Number(b.pinned) - Number(a.pinned) || b.last_used_at - a.last_used_at || b.id - a.id
}

/** 按 id 去掉旧的一条，再按顺序插入新的（保留旧的高亮区间，除非给了新的） */
export function upsertHit(
  hits: readonly ClipHit[],
  item: ClipItemMeta,
  highlights?: ClipHit['highlights'],
): ClipHit[] {
  let kept: ClipHit['highlights'] = highlights ?? []
  const rest: ClipHit[] = []
  for (const h of hits) {
    if (h.item.id === item.id) {
      if (!highlights) kept = h.highlights
    } else rest.push(h)
  }
  let at = rest.findIndex((h) => compareItems(item, h.item) < 0)
  if (at < 0) at = rest.length
  rest.splice(at, 0, { item, highlights: kept })
  return rest
}

/** 原地替换（不改变顺序，对应 `clipboard:changed` 的 `updated`）；不存在时返回原数组 */
export function replaceItem(hits: readonly ClipHit[], item: ClipItemMeta): readonly ClipHit[] {
  const i = hits.findIndex((h) => h.item.id === item.id)
  if (i < 0) return hits
  const next = hits.slice()
  next[i] = { item, highlights: hits[i]?.highlights ?? [] }
  return next
}

export function removeHit(hits: readonly ClipHit[], id: number): readonly ClipHit[] {
  const i = hits.findIndex((h) => h.item.id === id)
  if (i < 0) return hits
  return hits.slice(0, i).concat(hits.slice(i + 1))
}

/**
 * 合并下一页：两边都已按 `compareItems` 排好序，按顺序归并（O(n)），同一 id 只留已加载的那份（它带着本地已应用的
 * 实时变化）。分页期间的实时插入会让下一页的开头与已有条目重复；取消置顶落到已加载范围之外的条目暂时排在末尾，
 * 下一页到达后归并回它真正的位置——不会出现重复，也不会乱序
 */
export function appendPage(hits: readonly ClipHit[], page: readonly ClipHit[]): ClipHit[] {
  const seen = new Set(hits.map((h) => h.item.id))
  const fresh = page.filter((h) => !seen.has(h.item.id))
  if (fresh.length === 0) return hits.slice()
  const out: ClipHit[] = []
  let i = 0
  let j = 0
  while (i < hits.length && j < fresh.length) {
    const a = hits[i] as ClipHit
    const b = fresh[j] as ClipHit
    if (compareItems(a.item, b.item) <= 0) {
      out.push(a)
      i++
    } else {
      out.push(b)
      j++
    }
  }
  while (i < hits.length) out.push(hits[i++] as ClipHit)
  while (j < fresh.length) out.push(fresh[j++] as ClipHit)
  return out
}

export function findHit(hits: readonly ClipHit[], id: number | null): ClipHit | undefined {
  if (id === null) return undefined
  return hits.find((h) => h.item.id === id)
}

// ---------------------------------------------------------------------------
// 时间（全文预览里的完整时间用；列表只分「置顶 / 最近」两组）

export type TimeGroup = 'today' | 'yesterday' | 'week' | 'earlier'
/** 列表分组：置顶 / 最近；检索结果平铺、没有分组标题 */
export type GroupId = 'pinned' | 'recent'

/** 本地时区当天 0 点 */
export function startOfDay(ts: number): number {
  const d = new Date(ts)
  d.setHours(0, 0, 0, 0)
  return d.getTime()
}

/** 某一天往前数 `days` 天的 0 点（按日历加减，夏令时切换的那天不会差一小时） */
function daysBefore(dayStart: number, days: number): number {
  const d = new Date(dayStart)
  d.setDate(d.getDate() - days)
  return d.getTime()
}

/** 「今天 / 昨天 / 近 7 天」的起点（一次算好，按时间归组时只比较数字） */
export interface DayBounds {
  readonly today: number
  readonly yesterday: number
  readonly week: number
}

export function dayBounds(now: number): DayBounds {
  const today = startOfDay(now)
  return { today, yesterday: daysBefore(today, 1), week: daysBefore(today, 6) }
}

/** 按时间落到哪一天的范围（今天 / 昨天 / 近 7 天 / 更早；未来时间算今天） */
export function timeGroup(ts: number, now: number | DayBounds): TimeGroup {
  const b = typeof now === 'number' ? dayBounds(now) : now
  if (ts >= b.today) return 'today'
  if (ts >= b.yesterday) return 'yesterday'
  if (ts >= b.week) return 'week'
  return 'earlier'
}

// ---------------------------------------------------------------------------
// 行模型

export interface GroupRow {
  readonly type: 'group'
  readonly key: string
  readonly group: GroupId
}
export interface ItemRow {
  readonly type: 'item'
  readonly key: number
  readonly hit: ClipHit
  /** 在当前可见顺序里的序号（从 1 开始，含置顶区，受过滤与搜索影响） */
  readonly ordinal: number
  readonly group: GroupId
}
export type Row = GroupRow | ItemRow

/**
 * 条目 → 行：最近列表分「置顶 / 最近」两组（design.clipboard-list），分组标题不可选中；检索结果平铺
 * （`flat`：置顶命中排在前面，其余按时间，没有分组标题——置顶项在行尾带图钉标记）。
 * 输入应已按 `compareItems` 排好序（Rust 的列表 / 检索结果本来就是这个顺序）；万一某组被拆成两段，标题 key 加序号避免重复
 */
export function buildRows(hits: readonly ClipHit[], flat = false): Row[] {
  const rows: Row[] = []
  let current: GroupId | null = null
  let splits = 0
  for (let i = 0; i < hits.length; i++) {
    const hit = hits[i] as ClipHit
    const group: GroupId = hit.item.pinned ? 'pinned' : 'recent'
    if (!flat && group !== current) {
      const seen = current !== null && rows.some((r) => r.type === 'group' && r.group === group)
      rows.push({
        type: 'group',
        key: seen ? `group:${group}:${++splits}` : `group:${group}`,
        group,
      })
      current = group
    }
    rows.push({ type: 'item', key: hit.item.id, hit, ordinal: i + 1, group })
  }
  return rows
}

export function rowHeight(row: Row | undefined): number {
  if (!row) return ROW_HEIGHT
  if (row.type === 'group') return GROUP_HEIGHT
  return row.hit.item.kind === 'image' ? IMAGE_ROW_HEIGHT : ROW_HEIGHT
}

/** 每一行在滚动内容里的起点（含首部留白；与 VirtualList / TanStack 的 `start` 相同） */
export function rowOffsets(rows: readonly Row[]): number[] {
  const out: number[] = []
  let y = LIST_PADDING
  for (const r of rows) {
    out.push(y)
    y += rowHeight(r)
  }
  return out
}

/** 列表内容的总高度（含首尾留白；没有行时为 0） */
export function listContentHeight(rows: readonly Row[]): number {
  if (rows.length === 0) return 0
  let h = 2 * LIST_PADDING
  for (const r of rows) h += rowHeight(r)
  return h
}

/** 打开时的卡片高度：clamp(内容, 下限, min(560, 上限))（会话内只增不减由调用方保证） */
export function openHeight(natural: number, maxHeight: number): number {
  const cap = Math.max(0, Math.min(OPEN_MAX_HEIGHT, maxHeight))
  return Math.min(Math.max(natural, Math.min(OPEN_MIN_HEIGHT, cap)), cap)
}

/** 视口能放下多少个普通行（overscan 上下各约一屏） */
export function screenRows(listHeight: number): number {
  return Math.max(1, Math.floor(listHeight / ROW_HEIGHT))
}

/** 菜单的自然高度（项 / 分隔线 / 标题） */
export function menuHeight(items: readonly { type?: string }[]): number {
  let h = MENU_PADDING
  for (const e of items) h += e.type === 'separator' ? MENU_SEPARATOR_HEIGHT : MENU_ITEM_HEIGHT
  return h
}

// ---------------------------------------------------------------------------
// 选中项的移动（跳过分组标题）

export function isItem(row: Row | undefined): row is ItemRow {
  return row?.type === 'item'
}

export function indexOfId(rows: readonly Row[], id: number | null): number {
  if (id === null) return -1
  return rows.findIndex((r) => r.type === 'item' && r.key === id)
}

export function firstItemIndex(rows: readonly Row[]): number {
  return rows.findIndex(isItem)
}

export function lastItemIndex(rows: readonly Row[]): number {
  for (let i = rows.length - 1; i >= 0; i--) if (isItem(rows[i])) return i
  return -1
}

/** 从 `from` 起按 `step` 方向走 `count` 个条目（跳过标题）；走不动时停在最后一个能到的条目 */
export function stepItem(rows: readonly Row[], from: number, step: 1 | -1, count = 1): number {
  if (from < 0) return step > 0 ? firstItemIndex(rows) : lastItemIndex(rows)
  let at = from
  let left = count
  for (let i = from + step; i >= 0 && i < rows.length && left > 0; i += step) {
    if (isItem(rows[i])) {
      at = i
      left--
    }
  }
  return at
}

/** 当前可见顺序里的第 n 个条目（⌘1–9） */
export function nthItem(rows: readonly Row[], n: number): ItemRow | undefined {
  for (const r of rows) if (r.type === 'item' && r.ordinal === n) return r
  return undefined
}

// ---------------------------------------------------------------------------
// 显示单位（toast 宽度，与 Rust `placement::toast_width` 同一估算）

const WIDE = /[ᄀ-ᅟ⺀-꓏가-힣豈-﫿︰-﹏＀-｠￠-￦]/

export function displayUnits(text: string): number {
  let n = 0
  for (const ch of text) n += WIDE.test(ch) ? 2 : 1
  return n
}

/** toast 视图宽度：内容宽度，夹在 220–360 */
export function toastWidth(pasteKey: string): number {
  const units = 26 + displayUnits(pasteKey)
  return Math.min(360, Math.max(220, 64 + units * 7))
}

// ---------------------------------------------------------------------------
// 颜色（全文预览里的 HEX / RGB / HSL；只解析常见写法，其余格式由调用方用 canvas 换算）

export interface Rgba {
  r: number
  g: number
  b: number
  /** 0–1 */
  a: number
}

const HEX = /^#([0-9a-f]{3,4}|[0-9a-f]{6}|[0-9a-f]{8})$/i
const RGB = /^rgba?\(\s*([^)]*)\)$/i

function channel(token: string): number | null {
  const v = Number.parseFloat(token)
  if (!Number.isFinite(v)) return null
  return token.trim().endsWith('%') ? (v / 100) * 255 : v
}

function alphaOf(token: string | undefined): number | null {
  if (token === undefined) return 1
  const v = Number.parseFloat(token)
  if (!Number.isFinite(v)) return null
  return token.trim().endsWith('%') ? v / 100 : v
}

/** `#RGB[A]` / `#RRGGBB[AA]` / `rgb[a](…)`（逗号或空格分隔、`/ alpha`） */
export function parseColor(value: string): Rgba | null {
  const v = value.trim()
  const hex = HEX.exec(v)?.[1]
  if (hex) {
    const full =
      hex.length <= 4
        ? Array.from(hex)
            .map((c) => c + c)
            .join('')
        : hex
    const n = (i: number) => Number.parseInt(full.slice(i * 2, i * 2 + 2), 16)
    return { r: n(0), g: n(1), b: n(2), a: full.length === 8 ? n(3) / 255 : 1 }
  }
  const body = RGB.exec(v)?.[1]
  if (body === undefined) return null
  const [main, slashAlpha] = body.split('/')
  const parts = (main ?? '').split(/[\s,]+/).filter(Boolean)
  if (parts.length < 3 || parts.length > 4) return null
  const [r, g, b] = parts.slice(0, 3).map(channel)
  const a = alphaOf(slashAlpha ?? parts[3])
  if (r == null || g == null || b == null || a == null) return null
  const clamp = (x: number, max: number) => Math.min(max, Math.max(0, x))
  return { r: clamp(r, 255), g: clamp(g, 255), b: clamp(b, 255), a: clamp(a, 1) }
}

const hex2 = (n: number) => Math.round(n).toString(16).padStart(2, '0').toUpperCase()

export function formatHex(c: Rgba): string {
  return `#${hex2(c.r)}${hex2(c.g)}${hex2(c.b)}${c.a < 1 ? hex2(c.a * 255) : ''}`
}

const round = (n: number, digits = 0) => {
  const f = 10 ** digits
  return Math.round(n * f) / f
}

/**
 * RGB / HSL 用逗号分隔的写法（`rgb(59, 130, 246)`、带透明度时 `rgba(…, 0.5)`）：设计工具、旧版 CSS 与各语言的颜色库
 * 都认，CSS Color 4 的空格写法很多地方粘贴进去不识别
 */
export function formatRgb(c: Rgba): string {
  const rgb = `${Math.round(c.r)}, ${Math.round(c.g)}, ${Math.round(c.b)}`
  return c.a < 1 ? `rgba(${rgb}, ${round(c.a, 2)})` : `rgb(${rgb})`
}

export function formatHsl(c: Rgba): string {
  const r = c.r / 255
  const g = c.g / 255
  const b = c.b / 255
  const max = Math.max(r, g, b)
  const min = Math.min(r, g, b)
  const l = (max + min) / 2
  const d = max - min
  let h = 0
  let s = 0
  if (d > 0) {
    s = d / (1 - Math.abs(2 * l - 1))
    if (max === r) h = ((g - b) / d) % 6
    else if (max === g) h = (b - r) / d + 2
    else h = (r - g) / d + 4
    h *= 60
    if (h < 0) h += 360
  }
  const hsl = `${round(h)}, ${round(s * 100)}%, ${round(l * 100)}%`
  return c.a < 1 ? `hsla(${hsl}, ${round(c.a, 2)})` : `hsl(${hsl})`
}

/** 文本里（大小写不敏感）找出查询词的命中区间，最多 `limit` 处 */
export function findMatches(
  text: string,
  query: string,
  limit = PREVIEW_MAX_MARKS,
): { start: number; end: number }[] {
  const q = query.trim().toLowerCase()
  if (!q) return []
  const hay = text.toLowerCase()
  // toLowerCase 改变长度（极少数字符）时下标不再对应，放弃高亮
  if (hay.length !== text.length) return []
  const out: { start: number; end: number }[] = []
  let at = hay.indexOf(q)
  while (at >= 0 && out.length < limit) {
    out.push({ start: at, end: at + q.length })
    at = hay.indexOf(q, at + q.length)
  }
  return out
}

/**
 * 文本的前 n 行（悬停预览）。只看开头 `headChars` 字、每行截到 `lineChars` 字（多出的部分以 … 表示）：
 * 无论正文多长，切行与排版的工作量都有上限
 */
export function firstLines(
  text: string,
  n: number,
  headChars = HOVER_HEAD_CHARS,
  lineChars = HOVER_LINE_CHARS,
): { text: string; more: boolean } {
  const head = text.length > headChars ? text.slice(0, headChars) : text
  const lines = head.replace(/\r\n?/g, '\n').split('\n')
  // 去掉开头的空行，预览从第一行有内容的地方开始
  while (lines.length > 1 && (lines[0] ?? '').trim() === '') lines.shift()
  const shown = lines
    .slice(0, n)
    .map((l) => (l.length > lineChars ? `${l.slice(0, lineChars)}…` : l))
  return { text: shown.join('\n'), more: lines.length > n || head.length < text.length }
}

/** 正文是否只有空白（行内预览为空时显示占位，不让整行看起来像渲染失败） */
export function isBlank(preview: string): boolean {
  return preview.trim() === ''
}

/** 链接的主机名（解析失败时为 null） */
export function hostOf(link: string): string | null {
  try {
    return new URL(link).host || null
  } catch {
    return null
  }
}
