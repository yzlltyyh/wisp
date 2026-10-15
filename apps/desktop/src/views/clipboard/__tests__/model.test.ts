/**
 * 剪贴板视图的纯逻辑：行模型（「置顶 / 最近」两组，检索平铺）、行高与偏移、打开高度、选中项移动、⌘n 计数、过滤、
 * 排序与增删、颜色换算、文本工具、toast 宽度。
 */
import { describe, expect, it } from 'vitest'
import type { ClipHit, ClipItemMeta } from '../../../shared/ipc/types'
import {
  appendPage,
  buildRows,
  dayBounds,
  HOVER_HEAD_CHARS,
  HOVER_LINE_CHARS,
  compareItems,
  cycleFilter,
  displayUnits,
  findMatches,
  firstItemIndex,
  firstLines,
  formatHex,
  formatHsl,
  formatRgb,
  GROUP_HEIGHT,
  hostOf,
  IMAGE_ROW_HEIGHT,
  lastItemIndex,
  LIST_PADDING,
  listContentHeight,
  matchesFilter,
  menuHeight,
  nthItem,
  OPEN_MAX_HEIGHT,
  OPEN_MIN_HEIGHT,
  openHeight,
  parseColor,
  removeHit,
  replaceItem,
  ROW_HEIGHT,
  rowOffsets,
  screenRows,
  stepItem,
  timeGroup,
  toastWidth,
  upsertHit,
} from '../model'
import { HOUR, meta, NOW, TODAY } from './helpers'

const hit = (item: ClipItemMeta): ClipHit => ({ item, highlights: [] })

describe('时间与分组', () => {
  it('今天 / 昨天 / 近 7 天 / 更早按本地日历划分（全文预览的完整时间用），未来时间算今天', () => {
    expect(timeGroup(NOW, NOW)).toBe('today')
    expect(timeGroup(TODAY, NOW)).toBe('today')
    expect(timeGroup(NOW + HOUR, NOW)).toBe('today')
    expect(timeGroup(TODAY - 1, NOW)).toBe('yesterday')
    expect(timeGroup(TODAY - 24 * HOUR, NOW)).toBe('yesterday')
    expect(timeGroup(TODAY - 24 * HOUR - 1, NOW)).toBe('week')
    expect(timeGroup(TODAY - 6 * 24 * HOUR, NOW)).toBe('week')
    expect(timeGroup(TODAY - 6 * 24 * HOUR - 1, NOW)).toBe('earlier')
    // 边界一次算好，之后只比较数字
    const b = dayBounds(NOW)
    expect(timeGroup(TODAY - 1, b)).toBe('yesterday')
  })

  it('最近列表分「置顶 / 最近」两组；分组标题不可选中，序号跳过标题', () => {
    const hits = [
      hit(meta(1, { pinned: true, last_used_at: TODAY - 30 * 24 * HOUR })),
      hit(meta(2, { last_used_at: NOW - 60_000 })),
      hit(meta(3, { last_used_at: TODAY - 2 * HOUR })),
      hit(meta(4, { last_used_at: TODAY - 40 * 24 * HOUR })),
    ]
    const rows = buildRows(hits)
    expect(rows.map((r) => (r.type === 'group' ? r.group : r.key))).toEqual([
      'pinned',
      1,
      'recent',
      2,
      3,
      4,
    ])
    expect(rows.flatMap((r) => (r.type === 'item' ? [r.ordinal] : []))).toEqual([1, 2, 3, 4])
    // 标题 key 稳定（实时插入时标题平移而不是重新挂载）
    expect(rows[2]?.key).toBe('group:recent')
    // 没有置顶项时只有「最近」
    expect(buildRows(hits.slice(1)).filter((r) => r.type === 'group')).toHaveLength(1)
  })

  it('检索结果平铺：没有分组标题，置顶命中仍在前面，序号照常', () => {
    const rows = buildRows([hit(meta(1, { pinned: true })), hit(meta(2)), hit(meta(3))], true)
    expect(rows.every((r) => r.type === 'item')).toBe(true)
    expect(rows.map((r) => (r.type === 'item' ? r.ordinal : 0))).toEqual([1, 2, 3])
  })

  it('某组被拆成两段（顺序不单调）时，标题 key 不重复', () => {
    const rows = buildRows([
      hit(meta(1, { pinned: true })),
      hit(meta(2)),
      hit(meta(3, { pinned: true })),
    ])
    const keys = rows.map((r) => r.key)
    expect(new Set(keys).size).toBe(keys.length)
  })
})

describe('行高、偏移与高度', () => {
  const rows = buildRows([
    hit(meta(1, { pinned: true })),
    hit(meta(2, { kind: 'image' })),
    hit(meta(3)),
    hit(meta(4, { kind: 'image' })),
  ])

  it('混合 44 / 64 行高 + 28 的分组标题：偏移与 VirtualList 的 start 相同', () => {
    // [置顶, 1, 最近, 2（图片）, 3, 4（图片）]
    expect(rowOffsets(rows)).toEqual([
      LIST_PADDING,
      LIST_PADDING + GROUP_HEIGHT,
      LIST_PADDING + GROUP_HEIGHT + ROW_HEIGHT,
      LIST_PADDING + 2 * GROUP_HEIGHT + ROW_HEIGHT,
      LIST_PADDING + 2 * GROUP_HEIGHT + ROW_HEIGHT + IMAGE_ROW_HEIGHT,
      LIST_PADDING + 2 * GROUP_HEIGHT + 2 * ROW_HEIGHT + IMAGE_ROW_HEIGHT,
    ])
    expect(listContentHeight(rows)).toBe(
      2 * LIST_PADDING + 2 * GROUP_HEIGHT + 2 * ROW_HEIGHT + 2 * IMAGE_ROW_HEIGHT,
    )
    expect(listContentHeight([])).toBe(0)
  })

  it('打开高度：clamp(内容, 下限, min(560, 上限))；下限保证过滤菜单放得下且不低于 240', () => {
    expect(OPEN_MIN_HEIGHT).toBeGreaterThanOrEqual(240)
    expect(openHeight(100, 800)).toBe(OPEN_MIN_HEIGHT)
    expect(openHeight(400, 800)).toBe(400)
    expect(openHeight(900, 800)).toBe(OPEN_MAX_HEIGHT)
    expect(openHeight(900, 300)).toBe(300)
    expect(openHeight(100, 200)).toBe(200)
  })

  it('视口能放下的行数（overscan 上下各约一屏）', () => {
    expect(screenRows(440)).toBe(10)
    expect(screenRows(10)).toBe(1)
  })

  it('菜单高度：项 28、分隔线 9、内边距 8', () => {
    expect(menuHeight([{}, {}, { type: 'separator' }, {}])).toBe(8 + 3 * 28 + 9)
  })
})

describe('选中项移动', () => {
  const rows = buildRows([
    hit(meta(1, { pinned: true })),
    hit(meta(2)),
    hit(meta(3, { pinned: true })),
    hit(meta(4, { last_used_at: TODAY - 2 * HOUR })),
  ])
  // 顺序不单调时标题拆段：[group, 1, group, 2, group, 3, group, 4]
  //（只为覆盖「跳过多个标题」，真实数据里置顶总在前面）

  it('↑↓ 跳过分组标题，到头停住', () => {
    expect(stepItem(rows, 1, 1)).toBe(3)
    expect(stepItem(rows, 3, 1)).toBe(5)
    expect(stepItem(rows, 5, 1)).toBe(7)
    expect(stepItem(rows, 7, 1)).toBe(7)
    expect(stepItem(rows, 3, -1)).toBe(1)
    expect(stepItem(rows, 1, -1)).toBe(1)
  })

  it('按页移动走 n 个条目；没有选中时从首 / 末开始', () => {
    expect(stepItem(rows, 1, 1, 5)).toBe(7)
    expect(stepItem(rows, -1, 1)).toBe(1)
    expect(stepItem(rows, -1, -1)).toBe(7)
    expect(firstItemIndex(rows)).toBe(1)
    expect(lastItemIndex(rows)).toBe(7)
  })

  it('⌘n 按当前可见顺序计（含置顶区）', () => {
    expect(nthItem(rows, 1)?.key).toBe(1)
    expect(nthItem(rows, 3)?.key).toBe(3)
    expect(nthItem(rows, 5)).toBeUndefined()
  })
})

describe('过滤与排序', () => {
  it('文本过滤包含富文本；Tab / ⇧Tab 循环', () => {
    expect(matchesFilter('rich_text', 'text')).toBe(true)
    expect(matchesFilter('code', 'text')).toBe(false)
    expect(matchesFilter('image', 'all')).toBe(true)
    expect(cycleFilter('all', 1)).toBe('text')
    expect(cycleFilter('all', -1)).toBe('color')
    expect(cycleFilter('color', 1)).toBe('all')
  })

  it('顺序与 Rust 一致：置顶在前、最近使用在前、同一毫秒 id 大的在前', () => {
    const a = meta(1, { last_used_at: 10 })
    const b = meta(2, { last_used_at: 10 })
    const c = meta(3, { last_used_at: 5, pinned: true })
    expect([a, b, c].sort(compareItems).map((x) => x.id)).toEqual([3, 2, 1])
  })

  it('upsert：按 id 去掉旧的再按顺序插入，保留旧的高亮', () => {
    const hits: ClipHit[] = [
      { item: meta(1, { pinned: true }), highlights: [] },
      { item: meta(2, { last_used_at: 100 }), highlights: [{ start: 0, end: 2 }] },
      { item: meta(3, { last_used_at: 50 }), highlights: [] },
    ]
    const touched = upsertHit(hits, meta(3, { last_used_at: 200 }))
    expect(touched.map((h) => h.item.id)).toEqual([1, 3, 2])
    const moved = upsertHit(touched, meta(2, { last_used_at: 100, pinned: true }))
    // 置顶区内同样按最近使用排序：1 比 2 新
    expect(moved.map((h) => h.item.id)).toEqual([1, 2, 3])
    expect(moved[1]?.highlights).toEqual([{ start: 0, end: 2 }])
  })

  it('原地替换不改顺序；删除；分页按 id 去重', () => {
    const hits = [hit(meta(1)), hit(meta(2))]
    expect(replaceItem(hits, meta(2, { preview: 'x' }))[1]?.item.preview).toBe('x')
    expect(replaceItem(hits, meta(9))).toBe(hits)
    expect(removeHit(hits, 1).map((h) => h.item.id)).toEqual([2])
    expect(removeHit(hits, 9)).toBe(hits)
    expect(appendPage(hits, [hit(meta(2)), hit(meta(3))]).map((h) => h.item.id)).toEqual([1, 2, 3])
  })

  it('分页按顺序归并：排在末尾、实际属于已加载范围之外的条目（取消置顶）回到它真正的位置', () => {
    // 已加载：3、4，以及刚取消置顶、比它们都旧的 9（暂时排在末尾）
    const loaded = [hit(meta(3)), hit(meta(4)), hit(meta(9))]
    const page = [hit(meta(5)), hit(meta(9)), hit(meta(10))]
    expect(appendPage(loaded, page).map((h) => h.item.id)).toEqual([3, 4, 5, 9, 10])
    // 已加载的那份优先（带着本地已应用的实时变化）
    const local = { ...meta(9), preview: 'local' }
    const merged = appendPage([hit(meta(3)), hit(local)], page)
    expect(merged.find((h) => h.item.id === 9)?.item.preview).toBe('local')
  })
})

describe('颜色', () => {
  it('解析 #RGB / #RRGGBBAA / rgb() / rgba()，输出 HEX / RGB / HSL', () => {
    expect(parseColor('#3B82F6')).toEqual({ r: 59, g: 130, b: 246, a: 1 })
    expect(parseColor('#fff')).toEqual({ r: 255, g: 255, b: 255, a: 1 })
    expect(parseColor('rgb(52 120 246 / 50%)')).toEqual({ r: 52, g: 120, b: 246, a: 0.5 })
    expect(parseColor('rgba(52,120,246,0.5)')).toEqual({ r: 52, g: 120, b: 246, a: 0.5 })
    expect(parseColor('oklch(0.7 0.15 250)')).toBeNull()
    const c = { r: 59, g: 130, b: 246, a: 1 }
    expect(formatHex(c)).toBe('#3B82F6')
    expect(formatRgb(c)).toBe('rgb(59, 130, 246)')
    expect(formatHsl(c)).toBe('hsl(217, 91%, 60%)')
    expect(formatHex({ ...c, a: 0.5 })).toBe('#3B82F680')
    expect(formatRgb({ ...c, a: 0.5 })).toBe('rgba(59, 130, 246, 0.5)')
  })
})

describe('文本工具', () => {
  it('全文高亮大小写不敏感、有上限', () => {
    expect(findMatches('Wisp wisp WISP', 'wisp')).toEqual([
      { start: 0, end: 4 },
      { start: 5, end: 9 },
      { start: 10, end: 14 },
    ])
    expect(findMatches('aaaa', 'a', 2)).toHaveLength(2)
    expect(findMatches('abc', '  ')).toEqual([])
  })

  it('前 n 行（跳过开头的空行，统一换行符）', () => {
    expect(firstLines('\n\na\r\nb\rc\nd', 3)).toEqual({ text: 'a\nb\nc', more: true })
    expect(firstLines('one', 3)).toEqual({ text: 'one', more: false })
  })

  it('前 n 行的工作量有上限：只看开头 2000 字，每行截到 300 字（1 MB 的单行 JSON 不整段进排版）', () => {
    const huge = '{"k":1,'.repeat(150_000)
    const { text, more } = firstLines(huge, 3)
    expect(text.length).toBe(HOVER_LINE_CHARS + 1)
    expect(text.endsWith('…')).toBe(true)
    expect(more).toBe(true)
    const lines = Array.from({ length: 10 }, (_, i) => `line ${i}`).join('\n')
    expect(firstLines(lines, 3).text).toBe('line 0\nline 1\nline 2')
    expect(firstLines('x'.repeat(HOVER_HEAD_CHARS + 50), 3).more).toBe(true)
  })

  it('链接主机名', () => {
    expect(hostOf('https://github.com/yzlltyyh/wisp')).toBe('github.com')
    expect(hostOf('not a url')).toBeNull()
  })

  it('toast 宽度与 Rust 的 toast_width 同一估算，夹在 220–360', () => {
    expect(displayUnits('⌘V')).toBe(2)
    expect(displayUnits('中')).toBe(2)
    expect(toastWidth('⌘V')).toBe(64 + 28 * 7)
    expect(toastWidth('Ctrl+Shift+Alt+V'.repeat(4))).toBe(360)
    expect(toastWidth('')).toBeGreaterThanOrEqual(220)
  })
})
