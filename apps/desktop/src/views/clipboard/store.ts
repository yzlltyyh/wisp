/**
 * 剪贴板视图的状态。每个面板会话一份（视图挂载时创建，隐藏时随视图卸载——
 * 隐藏时只保留首屏数据：下次打开从 Rust 缓存的首屏重新开始，前端不留任何列表）。
 *
 * 只存列表数据、查询、过滤、选中项与 UI 开关；派生数据（行、高度、偏移）在视图里按需计算。
 * 这里的转换都是纯函数（`applyChange` 等），IPC 编排在 `useClipboard.ts`。
 */
import { createStore, type StoreApi } from 'zustand'
import type {
  ClipboardChanged,
  ClipboardPayload,
  ClipFilter,
  ClipHit,
  ClipItemMeta,
} from '../../shared/ipc/types'
import {
  buildRows,
  findHit,
  matchesFilter,
  removeHit,
  replaceItem,
  SEARCH_LIMIT,
  upsertHit,
  type Row,
} from './model'

/**
 * 「最近」列表（某个过滤下）：已加载的条目与下一页的偏移。
 *
 * `nextOffset` 是服务端顺序里「已加载的前缀」的长度，随本地变化同步（`shiftOffset`）：实时插入 / 移到顶部 +1，
 * 从前缀里移除 −1，取消置顶后落到已加载范围之外（排在末尾）−1。宁可少算（下一页开头与已有条目重复，归并时去掉），
 * 不可多算（会跳过条目）
 */
export interface RecentList {
  readonly filter: ClipFilter
  readonly hits: readonly ClipHit[]
  readonly nextOffset: number | null
  /** 服务端结果是否已到（切换过滤时先用已加载的全部条目在本地筛出一份，立即可用） */
  readonly loaded: boolean
}

/** 检索结果（某个查询 + 过滤） */
export interface SearchResult {
  readonly query: string
  readonly filter: ClipFilter
  readonly hits: readonly ClipHit[]
  readonly limit: number
}

/** 行内二次确认：删除置顶项 / 清空历史 */
export type Confirm = { kind: 'delete'; id: number } | { kind: 'clear'; keepPinned: boolean }

/** 轻提示的语气：完成 / 出错 / 说明（例如跳过了疑似密钥） */
export type StatusTone = 'ok' | 'error' | 'info'

/** 卡片底部的轻提示 */
export interface Status {
  readonly seq: number
  readonly tone: StatusTone
  readonly text: string
}

export interface ClipState {
  /** 过滤 = 全部 的最近列表：载荷里的首屏 + 实时事件 + 分页 */
  readonly all: RecentList
  /** 当前过滤（非全部）的最近列表 */
  readonly filtered: RecentList | null
  readonly search: SearchResult | null
  /** 搜索框里的文字（受控，立即更新；检索去抖 60 ms） */
  readonly query: string
  readonly filter: ClipFilter
  /** 有检索 / 过滤请求在进行中 */
  readonly searching: boolean
  readonly loadingMore: boolean
  readonly selectedId: number | null
  readonly paused: boolean
  /** Space 全文预览是否打开 */
  readonly preview: boolean
  readonly confirm: Confirm | null
  readonly status: Status | null
  /** 刚实时插入的条目（rowInsert 动效，播完即清） */
  readonly fresh: ReadonlySet<number>
  /** 已有行位置变化的轮次：每次实时插入 / 置顶 / 删除 +1，列表据此短暂开启行的位移过渡 */
  readonly moves: number
  /** 选中项需要滚进视口的轮次（键盘移动、置顶后跟随条目） */
  readonly reveal: number
  /**
   * 这次选中项变化时指示器直接到位（按键重复、按页 / 到首尾的跳转）：只有单步移动与悬停才用 spring 追随，
   * 否则高亮会拖在实际选中项后面几行
   */
  readonly jump: boolean
  /** 列表换了来源（新的检索词、换过滤、清空搜索）的轮次：滚回顶部 */
  readonly scrollReset: number
  /** ⌘K / ⇧F10 请求在选中行打开操作菜单的轮次（列表据此打开） */
  readonly actionMenu: number
  /** 粘贴进行中（防止连按 Enter 重复粘贴） */
  readonly busy: boolean
  /** 相对时间的基准（打开时取一次，显示期间每分钟刷新；分组只有「置顶 / 最近」，与时间无关） */
  readonly now: number
}

export type ClipStore = StoreApi<ClipState>

const EMPTY: ReadonlySet<number> = new Set()

export function initialState(payload: ClipboardPayload, now: number): ClipState {
  const page = payload.page
  const hits = page.items
  return {
    all: { filter: 'all', hits, nextOffset: page.next_offset, loaded: true },
    filtered: null,
    search: null,
    query: '',
    filter: 'all',
    searching: false,
    loadingMore: false,
    selectedId: hits[0]?.item.id ?? null,
    paused: payload.paused,
    preview: false,
    confirm: null,
    status: null,
    fresh: EMPTY,
    moves: 0,
    reveal: 0,
    jump: false,
    scrollReset: 0,
    actionMenu: 0,
    busy: false,
    now,
  }
}

export function createClipStore(payload: ClipboardPayload, now = Date.now()): ClipStore {
  return createStore<ClipState>()(() => initialState(payload, now))
}

/** 当前过滤下的「最近」列表（还没有服务端结果时从全部条目里本地筛出） */
export function recentHits(s: ClipState): readonly ClipHit[] {
  if (s.filter === 'all') return s.all.hits
  if (s.filtered?.filter === s.filter) return s.filtered.hits
  return s.all.hits.filter((h) => matchesFilter(h.item.kind, s.filter))
}

/** 是否处于检索模式（搜索框有内容） */
export function isSearching(s: ClipState): boolean {
  return s.query.trim() !== ''
}

/**
 * 当前显示的条目：搜索框有内容且已有检索结果时显示结果（新的结果到达之前保留上一次的，不闪烁；过滤变了先在本地
 * 收窄），否则显示最近列表
 */
export function visibleHits(s: ClipState): readonly ClipHit[] {
  if (isSearching(s) && s.search) {
    if (s.search.filter === s.filter) return s.search.hits
    return s.search.hits.filter((h) => matchesFilter(h.item.kind, s.filter))
  }
  return recentHits(s)
}

/** 当前列表还能不能往下加载 */
export function canLoadMore(s: ClipState): boolean {
  if (isSearching(s)) {
    const r = s.search
    return !!r && r.hits.length >= r.limit && r.limit < 500
  }
  if (s.filter === 'all') return s.all.nextOffset !== null
  return s.filtered?.filter === s.filter && s.filtered.loaded && s.filtered.nextOffset !== null
}

export function selectedItem(s: ClipState): ClipItemMeta | undefined {
  return findHit(visibleHits(s), s.selectedId)?.item
}

/** 删除 / 移走某条之后的选中项：同一位置的下一条，没有则上一条 */
export function neighborAfterRemoval(hits: readonly ClipHit[], id: number): number | null {
  const i = hits.findIndex((h) => h.item.id === id)
  if (i < 0) return null
  return hits[i + 1]?.item.id ?? hits[i - 1]?.item.id ?? null
}

type ListKind = 'recent' | 'search'

/**
 * 一条条目在某个最近列表里的位置变化之后，下一页的偏移（见 `RecentList.nextOffset`）。只在还有下一页时调整；
 * 落在末尾的条目视为「已加载范围之外」（它在服务端的真实位置可能更靠后）
 */
export function shiftOffset(
  nextOffset: number | null,
  before: readonly ClipHit[],
  after: readonly ClipHit[],
  id: number,
): number | null {
  if (nextOffset === null || before === after) return nextOffset
  const was = before.findIndex((h) => h.item.id === id)
  const now = after.findIndex((h) => h.item.id === id)
  const atEnd = now >= 0 && now === after.length - 1
  if (was >= 0 && now < 0) return Math.max(0, nextOffset - 1)
  if (was < 0 && now >= 0) return atEnd ? nextOffset : nextOffset + 1
  if (was >= 0 && atEnd && was !== before.length - 1) return Math.max(0, nextOffset - 1)
  return nextOffset
}

function mapLists(
  s: ClipState,
  fn: (hits: readonly ClipHit[], filter: ClipFilter, kind: ListKind) => readonly ClipHit[],
  /** 变化涉及的条目：给出时同步最近列表的下一页偏移 */
  id?: number,
): Pick<ClipState, 'all' | 'filtered' | 'search'> {
  const recent = (list: RecentList): RecentList => {
    const hits = fn(list.hits, list.filter, 'recent')
    const nextOffset =
      id === undefined ? list.nextOffset : shiftOffset(list.nextOffset, list.hits, hits, id)
    return { ...list, hits, nextOffset }
  }
  const all = recent(s.all)
  const filtered = s.filtered ? recent(s.filtered) : null
  const search = s.search
    ? { ...s.search, hits: fn(s.search.hits, s.search.filter, 'search') }
    : null
  return { all, filtered, search }
}

/**
 * 一条条目的新状态写进各个列表并按顺序重排（置顶 / 取消置顶、touched 都会改变位置）。检索结果里只更新已有的
 * 条目（它仍然匹配查询词）；不在结果里的新条目是否匹配由重新检索决定（调用方负责）
 */
export function placeItem(
  s: ClipState,
  item: ClipItemMeta,
): Pick<ClipState, 'all' | 'filtered' | 'search'> {
  return mapLists(
    s,
    (hits, filter, kind) => {
      const present = hits.some((h) => h.item.id === item.id)
      if (!matchesFilter(item.kind, filter)) return present ? removeHit(hits, item.id) : hits
      if (kind === 'search' && !present) return hits
      return upsertHit(hits, item)
    },
    item.id,
  )
}

/**
 * 失败回滚：把一条（连同它在检索结果里的高亮）放回各列表。检索结果里原来有它、现在没有时按原来的高亮放回
 * （`placeItem` 不会往检索结果里加条目）
 */
export function restoreHit(
  s: ClipState,
  hit: ClipHit,
  searchHit?: ClipHit,
): Pick<ClipState, 'all' | 'filtered' | 'search'> {
  const lists = placeItem(s, hit.item)
  const search = lists.search
  if (!search || !searchHit || !matchesFilter(hit.item.kind, search.filter)) return lists
  if (search.hits.some((h) => h.item.id === hit.item.id)) return lists
  return {
    ...lists,
    search: { ...search, hits: upsertHit(search.hits, hit.item, searchHit.highlights) },
  }
}

/**
 * `clipboard:changed` → 新状态（纯函数）。
 * - inserted / touched：最近列表里放到「最近」组的顶部（置顶项在「置顶」组顶部），选中项按 id 保持不变
 *   （高亮不跳动）；检索模式下由调用方重新检索；
 * - updated：按 id 原地替换，不改顺序（缩略图生成完成、文件存在性刷新）；
 * - removed / cleared：从各列表移除，选中项落到相邻条目；
 * - paused：横幅；secret_skipped：底部轻提示（由调用方给出文案）。
 */
export function applyChange(s: ClipState, change: ClipboardChanged): Partial<ClipState> {
  switch (change.change) {
    case 'inserted':
    case 'touched': {
      const item = change.item
      const before = visibleHits(s)
      const existed = before.some((h) => h.item.id === item.id)
      const lists = placeItem(s, item)
      const next = { ...s, ...lists }
      const after = visibleHits(next)
      const shown = after.some((h) => h.item.id === item.id)
      const fresh = shown && !existed ? new Set([...s.fresh, item.id]) : s.fresh
      return {
        ...lists,
        fresh,
        moves: shown ? s.moves + 1 : s.moves,
        selectedId: s.selectedId ?? after[0]?.item.id ?? null,
      }
    }
    case 'updated':
      return mapLists(s, (hits) => replaceItem(hits, change.item))
    case 'removed':
      return removeFromLists(s, change.id)
    case 'cleared': {
      const keep = change.keep_pinned
      const lists = mapLists(s, (hits) => (keep ? hits.filter((h) => h.item.pinned) : []))
      const next = { ...s, ...lists }
      const after = visibleHits(next)
      const still = after.some((h) => h.item.id === s.selectedId)
      return {
        ...lists,
        all: { ...lists.all, nextOffset: null },
        filtered: lists.filtered ? { ...lists.filtered, nextOffset: null } : null,
        selectedId: still ? s.selectedId : (after[0]?.item.id ?? null),
        confirm: null,
        preview: after.length > 0 && s.preview,
        moves: s.moves + 1,
      }
    }
    case 'paused':
      return { paused: change.paused }
    case 'secret_skipped':
      return {}
  }
}

/** 从全部列表里移除一条，选中项落到相邻条目 */
export function removeFromLists(s: ClipState, id: number): Partial<ClipState> {
  const before = visibleHits(s)
  const neighbor = s.selectedId === id ? neighborAfterRemoval(before, id) : s.selectedId
  const lists = mapLists(s, (hits) => removeHit(hits, id), id)
  const after = visibleHits({ ...s, ...lists })
  const changed = after.length !== before.length
  return {
    ...lists,
    selectedId: after.some((h) => h.item.id === neighbor) ? neighbor : (after[0]?.item.id ?? null),
    confirm: s.confirm?.kind === 'delete' && s.confirm.id === id ? null : s.confirm,
    preview: s.preview && after.length > 0,
    moves: changed ? s.moves + 1 : s.moves,
  }
}

/** 检索结果到达 */
export function withSearchResult(
  s: ClipState,
  query: string,
  filter: ClipFilter,
  hits: readonly ClipHit[],
  limit = SEARCH_LIMIT,
): Partial<ClipState> {
  // 搜索框已经清空：结果作废
  if (!isSearching(s)) return { searching: false }
  const sameQuery = s.search?.query === query && s.search.filter === filter
  // 新的检索词：选中第一条并滚回顶部；同一个词放大 limit（加载更多）或实时变化后重新检索时保持选中与滚动位置
  if (sameQuery) {
    return {
      search: { query, filter, hits, limit },
      searching: false,
      selectedId: hits.some((h) => h.item.id === s.selectedId)
        ? s.selectedId
        : (hits[0]?.item.id ?? null),
    }
  }
  return {
    search: { query, filter, hits, limit },
    searching: false,
    selectedId: hits[0]?.item.id ?? null,
    scrollReset: s.scrollReset + 1,
  }
}

/** 每个 store 最近一次算出的行（视图渲染与键盘处理共用，列表没变时不重算） */
const rowCache = new WeakMap<ClipStore, { inputs: readonly unknown[]; rows: readonly Row[] }>()

/**
 * 当前显示的行（最近：置顶 / 最近两组；检索：平铺）；输入（各列表、是否检索、过滤）都没变时返回同一个数组。
 * 行不依赖时间基准：每分钟刷新相对时间只重渲染行尾，不重建行
 */
export function rowsOf(store: ClipStore): readonly Row[] {
  const s = store.getState()
  const searching = isSearching(s)
  const inputs = [s.all, s.filtered, s.search, searching, s.filter]
  const hit = rowCache.get(store)
  if (hit && hit.inputs.every((v, i) => v === inputs[i])) return hit.rows
  const rows = buildRows(visibleHits(s), searching && s.search !== null)
  rowCache.set(store, { inputs, rows })
  return rows
}
