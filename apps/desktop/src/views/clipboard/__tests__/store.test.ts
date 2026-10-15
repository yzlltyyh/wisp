/**
 * 剪贴板视图状态的纯转换：实时变化（新条目插到「最近」顶部、选中项按 id 不跳；updated 原地替换；
 * removed / cleared 后选中项落到相邻条目）、分页偏移随本地变化同步（删除 / 插入 / 取消置顶之后加载下一页不跳过条目）、
 * 失败回滚（检索结果一并恢复）、检索结果、过滤的本地收窄、行缓存。
 */
import { describe, expect, it } from 'vitest'
import {
  applyChange,
  createClipStore,
  initialState,
  placeItem,
  recentHits,
  removeFromLists,
  restoreHit,
  rowsOf,
  shiftOffset,
  visibleHits,
  withSearchResult,
  type ClipState,
} from '../store'
import { appendPage } from '../model'
import { hit, meta, NOW, payload, sample } from './helpers'

const base = (): ClipState => initialState(payload(sample()), NOW)
const ids = (s: ClipState) => visibleHits(s).map((h) => h.item.id)
const apply = (s: ClipState, patch: Partial<ClipState>): ClipState => ({ ...s, ...patch })

describe('初始状态', () => {
  it('首屏来自载荷（Rust 内存缓存），选中第一条；暂停状态来自载荷', () => {
    const s = initialState(payload(sample(), { paused: true }), NOW)
    expect(ids(s)).toEqual([1, 2, 3, 4, 5, 6, 7, 8])
    expect(s.selectedId).toBe(1)
    expect(s.paused).toBe(true)
    expect(initialState(payload([]), NOW).selectedId).toBeNull()
  })
})

describe('实时变化（clipboard:changed）', () => {
  it('inserted：插到置顶区之后的最前面；选中项不变；记为新行并计一次位移', () => {
    const s = apply(base(), { selectedId: 4 })
    const next = apply(
      s,
      applyChange(s, { change: 'inserted', item: meta(20, { last_used_at: NOW }) }),
    )
    expect(ids(next).slice(0, 4)).toEqual([1, 2, 20, 3])
    expect(next.selectedId).toBe(4)
    expect(next.fresh.has(20)).toBe(true)
    expect(next.moves).toBe(s.moves + 1)
  })

  it('touched：已有条目移到顶部（重复复制不新增行），不算新行', () => {
    const s = base()
    const item = { ...sample()[6]!, last_used_at: NOW + 1 }
    const next = apply(s, applyChange(s, { change: 'touched', item }))
    expect(ids(next)).toEqual([1, 2, 7, 3, 4, 5, 6, 8])
    expect(next.fresh.size).toBe(0)
  })

  it('updated：按 id 原地替换，不改变顺序（缩略图生成完成）', () => {
    const s = base()
    const img = sample()[4]!
    const updated = { ...img, image: { ...img.image!, thumb_url: 'thumb/new' } }
    const next = apply(s, applyChange(s, { change: 'updated', item: updated }))
    expect(ids(next)).toEqual(ids(s))
    expect(visibleHits(next)[4]?.item.image?.thumb_url).toBe('thumb/new')
    expect(next.moves).toBe(s.moves)
  })

  it('removed：选中项被删时落到同一位置的下一条，最后一条时落到上一条', () => {
    const s = apply(base(), { selectedId: 4 })
    const next = apply(s, applyChange(s, { change: 'removed', id: 4 }))
    expect(ids(next)).not.toContain(4)
    expect(next.selectedId).toBe(5)
    const last = apply(s, { selectedId: 8 })
    expect(apply(last, applyChange(last, { change: 'removed', id: 8 })).selectedId).toBe(7)
  })

  it('cleared：保留置顶时只剩置顶项；全部清空时为空；分页结束', () => {
    const s = apply(base(), { selectedId: 6 })
    const keep = apply(s, applyChange(s, { change: 'cleared', keep_pinned: true }))
    expect(ids(keep)).toEqual([1, 2])
    expect(keep.selectedId).toBe(1)
    expect(keep.all.nextOffset).toBeNull()
    const none = apply(s, applyChange(s, { change: 'cleared', keep_pinned: false }))
    expect(ids(none)).toEqual([])
    expect(none.selectedId).toBeNull()
  })

  it('paused：横幅开关', () => {
    const s = base()
    expect(apply(s, applyChange(s, { change: 'paused', paused: true })).paused).toBe(true)
  })

  it('检索模式下新插入的条目不直接放进结果（由重新检索决定），已在结果里的条目原地更新并重排', () => {
    const s = apply(base(), {
      query: 'a',
      search: { query: 'a', filter: 'all', hits: [hit(meta(3)), hit(meta(4))], limit: 200 },
    })
    const inserted = apply(
      s,
      applyChange(s, { change: 'inserted', item: meta(30, { last_used_at: NOW }) }),
    )
    expect(ids(inserted)).toEqual([3, 4])
    expect(inserted.fresh.size).toBe(0)
    const touched = apply(s, placeItem(s, meta(4, { last_used_at: NOW + 5 })))
    expect(ids(touched)).toEqual([4, 3])
  })
})

describe('删除（乐观）', () => {
  it('从各列表移除，取消该条目的删除确认', () => {
    const s = apply(base(), { selectedId: 1, confirm: { kind: 'delete', id: 1 } })
    const next = apply(s, removeFromLists(s, 1))
    expect(ids(next)).not.toContain(1)
    expect(next.confirm).toBeNull()
    expect(next.selectedId).toBe(2)
  })
})

describe('分页偏移（nextOffset 随本地变化同步）', () => {
  /** 服务端有 300 条（id 1..300，按 id 升序即服务端顺序），已加载前 200 条 */
  const server = Array.from({ length: 300 }, (_, i) =>
    meta(i + 1, { last_used_at: NOW - (i + 1) * 1000 }),
  )
  const loaded = (): ClipState => {
    const s = initialState(payload(server.slice(0, 200)), NOW)
    return { ...s, all: { ...s.all, nextOffset: 200 } }
  }
  /** 模拟服务端在本地变化之后的顺序，按 nextOffset 取下一页并归并 */
  const loadMore = (
    s: ClipState,
    serverAfter: readonly ClipState['all']['hits'][number]['item'][],
  ) => {
    const offset = s.all.nextOffset ?? 0
    const page = serverAfter.slice(offset, offset + 200).map((m) => hit(m))
    return appendPage(s.all.hits, page).map((h) => h.item.id)
  }

  it('删除已加载的 3 条后加载下一页：原来第 201–203 条都在，没有被跳过', () => {
    let s = loaded()
    for (const id of [5, 10, 20]) s = apply(s, removeFromLists(s, id))
    expect(s.all.nextOffset).toBe(197)
    const rest = server.filter((m) => ![5, 10, 20].includes(m.id))
    expect(loadMore(s, rest)).toEqual(rest.map((m) => m.id))
  })

  it('retention 的 removed 事件同样同步；删除未加载的条目不改偏移', () => {
    const s = loaded()
    const a = apply(s, applyChange(s, { change: 'removed', id: 7 }))
    expect(a.all.nextOffset).toBe(199)
    const b = apply(s, applyChange(s, { change: 'removed', id: 250 }))
    expect(b.all.nextOffset).toBe(200)
  })

  it('实时插入：偏移 +1（下一页不会与已加载的重复，也不会跳过）', () => {
    const s = loaded()
    const item = meta(1000, { last_used_at: NOW })
    const next = apply(s, applyChange(s, { change: 'inserted', item }))
    expect(next.all.nextOffset).toBe(201)
    expect(loadMore(next, [item, ...server])).toEqual([1000, ...server.map((m) => m.id)])
  })

  it('取消置顶后落到已加载范围之外：先排在末尾，偏移 −1；下一页到达后归并回真正的位置', () => {
    const pinnedOld = meta(999, { pinned: true, last_used_at: NOW - 250_500 })
    const base0 = initialState(payload([pinnedOld, ...server.slice(0, 199)]), NOW)
    let s: ClipState = { ...base0, all: { ...base0.all, nextOffset: 200 } }
    s = apply(s, placeItem(s, { ...pinnedOld, pinned: false }))
    expect(s.all.hits.at(-1)?.item.id).toBe(999)
    expect(s.all.nextOffset).toBe(199)
    // 服务端：999 落在 250 与 251 之间
    const after = [...server.slice(0, 250), { ...pinnedOld, pinned: false }, ...server.slice(250)]
    const got = loadMore(s, after)
    expect(got).toEqual(after.slice(0, got.length).map((m) => m.id))
    expect(got).toContain(200)
  })

  it('shiftOffset：只有还有下一页时才调整；宁少勿多', () => {
    const hits = [hit(meta(1)), hit(meta(2)), hit(meta(3))]
    expect(shiftOffset(null, hits, hits.slice(1), 1)).toBeNull()
    expect(shiftOffset(10, hits, hits.slice(1), 1)).toBe(9)
    expect(shiftOffset(10, hits.slice(1), hits, 1)).toBe(11)
    // 新条目落在末尾：视为已加载范围之外，不加
    expect(shiftOffset(10, hits.slice(0, 2), hits, 3)).toBe(10)
    expect(shiftOffset(0, hits, hits.slice(1), 1)).toBe(0)
  })
})

describe('失败回滚', () => {
  it('检索模式下删除失败：条目连同高亮放回检索结果与最近列表', () => {
    const target = sample()[2]!
    const searchHit = hit(target, [{ start: 0, end: 2 }])
    const s = apply(base(), {
      query: 'al',
      search: { query: 'al', filter: 'all', hits: [searchHit, hit(sample()[3]!)], limit: 200 },
    })
    const removed = apply(s, removeFromLists(s, target.id))
    expect(ids(removed)).toEqual([4])
    const restored = apply(removed, restoreHit(removed, hit(target), searchHit))
    expect(ids(restored)).toEqual([3, 4])
    expect(restored.search?.hits[0]?.highlights).toEqual([{ start: 0, end: 2 }])
    expect(restored.all.hits.some((h) => h.item.id === target.id)).toBe(true)
  })
})

describe('检索与过滤', () => {
  it('新的检索词：选中第一条并滚回顶部；同一个词（放大 limit / 重新检索）保持选中', () => {
    const s = apply(base(), { query: 'x', selectedId: 6 })
    const first = apply(s, withSearchResult(s, 'x', 'all', [hit(meta(6)), hit(meta(7))]))
    expect(ids(first)).toEqual([6, 7])
    expect(first.selectedId).toBe(6)
    expect(first.scrollReset).toBe(s.scrollReset + 1)
    const moved = apply(first, { selectedId: 7 })
    const again = apply(
      moved,
      withSearchResult(moved, 'x', 'all', [hit(meta(6)), hit(meta(7))], 500),
    )
    expect(again.selectedId).toBe(7)
    expect(again.scrollReset).toBe(first.scrollReset)
  })

  it('搜索框已经清空时，迟到的结果作废', () => {
    const s = base()
    expect(withSearchResult(s, 'x', 'all', [hit(meta(9))])).toEqual({ searching: false })
  })

  it('换过滤后服务端结果到达之前，先用已加载的条目在本地筛出', () => {
    const s = apply(base(), { filter: 'image' })
    expect(recentHits(s).map((h) => h.item.id)).toEqual([5])
    const t = apply(base(), { filter: 'text' })
    expect(recentHits(t).map((h) => h.item.id)).toEqual([1, 3, 4, 6, 7, 8])
  })

  it('检索模式下换过滤：新结果到达之前先在本地收窄', () => {
    const s = apply(base(), {
      query: 'a',
      filter: 'image',
      search: { query: 'a', filter: 'all', hits: [hit(meta(3)), hit(sample()[4]!)], limit: 200 },
    })
    expect(ids(s)).toEqual([5])
  })
})

describe('行缓存', () => {
  it('输入没变时返回同一个数组；列表变化后重算；每分钟刷新时间基准不重建行', () => {
    const store = createClipStore(payload(sample()), NOW)
    const a = rowsOf(store)
    expect(rowsOf(store)).toBe(a)
    store.setState({ now: NOW + 60_000 })
    expect(rowsOf(store)).toBe(a)
    store.setState({ query: 'x' })
    // 只是输入框有字、还没有检索结果：仍显示最近列表，但「是否检索」变了 → 重算
    expect(rowsOf(store)).not.toBe(a)
    store.setState((s) => applyChange(s, { change: 'removed', id: 3 }))
    expect(rowsOf(store).some((r) => r.key === 3)).toBe(false)
  })

  it('检索结果到达后平铺（没有分组标题）', () => {
    const store = createClipStore(payload(sample()), NOW)
    store.setState({ query: 'a' })
    store.setState((s) => withSearchResult(s, 'a', 'all', [hit(sample()[0]!), hit(sample()[2]!)]))
    expect(rowsOf(store).every((r) => r.type === 'item')).toBe(true)
  })
})
