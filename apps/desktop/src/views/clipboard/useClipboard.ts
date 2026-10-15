/**
 * 剪贴板视图的 IPC 编排：检索（去抖 60 ms、过期结果作废）、过滤、分页、实时事件、粘贴 / 置顶 / 删除 /
 * 清空 / 暂停与条目的其他操作。状态都在每个会话一份的 store 里，这里只负责「什么时候调用哪条命令、结果怎么落进 store」。
 *
 * - 粘贴：`clipboard_paste` 由 Rust 立即隐藏面板（不播放退出动画）、还原焦点、写剪贴板并模拟粘贴键；只复制的
 *   各分支由 Rust 以 toast 视图提示。前端不另外处理成功的结果。唯一的例外是平台层要求由 WebView 写剪贴板
 *   （KDE Wayland 不接受后台写入，`platform` + `errors.platform.wayland_clipboard*`，与 `useCopy`
 *   同一约定）：面板此刻仍有焦点，前端用 `navigator.clipboard` 写入（文本类与图片；之后的粘贴记住这一点，在按键的同一个
 *   任务里直接写，不再先绕 Rust，WebKit 只在用户手势里放行写入），再在原位切换到 toast 视图提示「已复制，按 Ctrl+V 粘贴」；
 * - 置顶 / 删除 / 清空 / 暂停都先乐观更新，失败时回滚（检索结果与清空前的列表一并恢复）并在底部提示是哪个操作失败；
 * - 条目已不存在（`not_found`）时从列表移除；文件条目的粘贴 / 「在 Finder 中显示」返回 `not_found` 表示**文件**都已
 *   不在了（记录还在，见 Rust 的 `paste_payload` / `reveal`）：只把这些文件标为不存在（整行置灰），不移除；
 * - 后台路径不读文件内容、不列目录：列表只用元数据，详情（`clipboard_get`）只在用户悬停 / 预览 /
 *   送翻译时请求；悬停只取正文开头（`textHead`），长正文不常驻；
 * - 剪贴板条目 → 翻译：优先 `clipboard_send_to(translate)`（正文留在 Rust 里取），命令未实现时退回交接里的
 *   `trigger_action(translate_selection, text, origin: clipboard)`；两条路径都由 Rust 发起交叉淡入的 prepare。
 */
import { useEffect, useState } from 'react'
import { api, getTransport, onMany, resolveAssetUrl, WispIpcError } from '../../shared/ipc'
import { usePrewarm } from '../../shared/perf/prewarm'
import type {
  ClipboardChanged,
  ClipFilter,
  ClipItemDetail,
  ClipItemMeta,
  OsKind,
  PanelPayload,
  PasteMode,
} from '../../shared/ipc/types'
import { t } from '../../shared/i18n/i18n'
import { formatHotkey } from '../../shared/keys/formatHotkey'
import { duration, ms, timing } from '../../shared/motion/tokens'
import {
  appendPage,
  HOVER_HEAD_CHARS,
  LARGE_TEXT_CHARS,
  PAGE_SIZE,
  SEARCH_DEBOUNCE_MS,
  SEARCH_LIMIT,
  SEARCH_LIMIT_MAX,
  toastWidth,
} from './model'
import {
  applyChange,
  canLoadMore,
  isSearching,
  placeItem,
  recentHits,
  removeFromLists,
  restoreHit,
  selectedItem,
  visibleHits,
  withSearchResult,
  type ClipState,
  type ClipStore,
  type StatusTone,
} from './store'

/** 平台层要求改由 WebView 写剪贴板的原因（与 shared/hooks/useCopy 相同的两个 key） */
const WEBVIEW_COPY_REASONS: ReadonlySet<string> = new Set([
  'errors.platform.wayland_clipboard_bridge',
  'errors.platform.wayland_clipboard',
])

/** 详情缓存的条数上限（悬停 / 预览来回切换时不重复请求；大段正文不常驻：超过 LARGE_TEXT_CHARS 字的不进缓存） */
const DETAIL_CACHE = 8
/** 悬停用的正文开头的缓存条数（每条最多 HOVER_HEAD_CHARS 字，长正文反复悬停也不再取全文） */
const HEAD_CACHE = 32

/**
 * 本 WebView 已确认剪贴板要由前端写（KDE Wayland）。面板 WebView 常驻，记在模块里：之后的粘贴在按键的
 * 同一个任务里直接写（WebKit 只在用户手势里放行 `navigator.clipboard.write`），不再先绕一次 Rust
 */
let webviewClipboard = false

/** 失败提示里的动作名（`clipboard.status.failed.*`） */
export type FailedAction =
  | 'paste'
  | 'copy'
  | 'pin'
  | 'unpin'
  | 'delete'
  | 'clear'
  | 'pause'
  | 'resume'
  | 'save_image'
  | 'reveal'
  | 'open_link'
  | 'translate'
  | 'settings'
  | 'search'
  | 'load'

export interface ClipActions {
  setQuery(query: string): void
  setFilter(filter: ClipFilter): void
  /** `jump`：指示器直接到位（按键重复、按页 / 到首尾），否则单步 spring 追随 */
  select(id: number | null, reveal?: boolean, jump?: boolean): void
  loadMore(): void
  paste(mode: PasteMode, id?: number): void
  togglePin(id?: number): void
  remove(force: boolean, id?: number): void
  cancelConfirm(): void
  askClear(): void
  setKeepPinned(keep: boolean): void
  clear(): void
  setPaused(paused: boolean): void
  setPreview(open: boolean): void
  saveImage(id: number): void
  reveal(id: number): void
  openLink(id: number): void
  translate(id: number): void
  openSettings(): void
  detail(id: number): Promise<ClipItemDetail | null>
  /** 详情缓存里已有的结果（同步，没有时为 undefined） */
  cachedDetail(id: number): ClipItemDetail | null | undefined
  /** 悬停卡片用：正文开头（最多 HOVER_HEAD_CHARS 字）；只缓存开头，不缓存全文 */
  textHead(id: number): Promise<string | null>
  cachedHead(id: number): string | null | undefined
  showStatus(text: string, tone?: StatusTone): void
  /** 指针停在轻提示上：暂停 / 恢复它的计时（错误要读得完） */
  holdStatus(hold: boolean): void
}

export interface ClipboardHost {
  readonly os: OsKind
  switchView(payload: PanelPayload, options?: { width?: number | null }): void
}

/**
 * 面板轻提示的停留（毫秒）：
 * - `ok`：一眼就懂的确认（「已保存图片」）toastHold（1.5 s）；
 * - `info`：带说明的信息（「已跳过一条疑似密码或密钥的内容」）按字数计时——toastHold + 每字 toastViewPerChar，夹在
 *   [noticeHold 3 s, toastViewDetailMax 5 s]（与设置窗口的轻提示、toast 视图的说明同一套规则：1.5 s
 *   读不完，而且这条有隐私含义）；
 * - `error`：statusErrorHold（4 s）——与设置 / 引导窗口一致，错误要读得完（「置顶失败，请再试一次」）
 */
export function statusHoldMs(text: string, tone: StatusTone): number {
  if (tone === 'error') return ms(timing.statusErrorHold)
  if (tone === 'ok') return ms(timing.toastHold)
  const chars = [...text.replace(/\s+/g, '')].length
  const hold = timing.toastHold + chars * timing.toastViewPerChar
  return ms(Math.min(timing.toastViewDetailMax, Math.max(timing.noticeHold, hold)))
}

const isTextual = (item: ClipItemMeta) =>
  item.kind === 'text' ||
  item.kind === 'rich_text' ||
  item.kind === 'code' ||
  item.kind === 'link' ||
  item.kind === 'color'

/** 视图里用：每个会话（store）一份操作对象；挂载时订阅实时变化，卸载时释放 */
export function useClipboard(store: ClipStore, host: ClipboardHost): ClipActions {
  const [actions] = useState(() => createClipActions(store, host))
  // JIT 预热渲染（ClipboardView 末尾）不订阅推送
  const prewarm = usePrewarm()
  // 宿主回调（switchView 等）每次渲染可能是新的函数：执行操作时用最新的
  useEffect(() => {
    actions.setHost(host)
  })
  useEffect(() => (prewarm ? undefined : actions.attach()), [actions, prewarm])
  return actions
}

/**
 * `clipboard:changed` 的常驻订阅：后端只在剪贴板视图 ready 之后推送、准备期产生的事件
 * ready 后立即补发——视图挂载时才异步 `listen` 的话，补发可能先于订阅生效而丢失（偶尔第一次打开时缩略图 / 置灰要到下次
 * 才出现）。面板 WebView 常驻，所以在面板启动时订阅一次、之后不再退订（一个监听的开销可以忽略），按当前挂载的视图分发；
 * 视图卸载只解绑处理函数
 */
let changedSink: ((change: ClipboardChanged) => void) | null = null
/** 常驻订阅装在哪个传输层上（生产里传输层不变；单测每个用例换一个，换了就重新订阅） */
let residentFor: { transport: unknown; off: () => void } | null = null

export function installClipboardEvents(): void {
  const transport = getTransport()
  if (residentFor?.transport === transport) return
  residentFor?.off()
  residentFor = {
    transport,
    off: onMany({ 'clipboard:changed': (change) => changedSink?.(change) }),
  }
}

/**
 * 操作对象（与 React 无关，单测直接构造）。`attach()` 订阅 `clipboard:changed`，返回的函数在卸载（隐藏）时
 * 退订并释放计时器与详情缓存；StrictMode 下的重复挂载会重新 attach
 */
export function createClipActions(
  store: ClipStore,
  initialHost: ClipboardHost,
): ClipActions & { attach(): () => void; setHost(host: ClipboardHost): void } {
  const timers = new Set<ReturnType<typeof setTimeout>>()
  let currentHost = initialHost
  const host = () => currentHost
  const get = store.getState
  const set = (patch: Partial<ClipState> | ((s: ClipState) => Partial<ClipState>)) =>
    store.setState(patch)
  let searchSeq = 0
  let filterSeq = 0
  let searchTimer: ReturnType<typeof setTimeout> | null = null
  let statusSeq = 0
  let statusTimer: ReturnType<typeof setTimeout> | null = null
  const details = new Map<number, Promise<ClipItemDetail | null>>()
  const settled = new Map<number, ClipItemDetail | null>()
  const heads = new Map<number, string | null>()
  let disposed = false

  const later = (delay: number, fn: () => void) => {
    const id = setTimeout(() => {
      timers.delete(id)
      if (!disposed) fn()
    }, delay)
    timers.add(id)
    return id
  }

  /** 轻提示还剩多久（ms，暂停时用）与本次计时开始的时刻 */
  let statusLeft = 0
  let statusFrom = 0
  const runStatusTimer = (left: number) => {
    if (statusTimer !== null) clearTimeout(statusTimer)
    const seq = statusSeq
    statusLeft = left
    statusFrom = Date.now()
    statusTimer = later(left, () => {
      statusTimer = null
      if (get().status?.seq === seq) set({ status: null })
    })
  }
  /**
   * 轻提示（停留规则见 `statusHoldMs`）；指针停在上面时暂停计时，下一次提示会立即取代它
   */
  const showStatus = (text: string, tone: StatusTone = 'ok') => {
    statusSeq++
    set({ status: { seq: statusSeq, tone, text } })
    runStatusTimer(statusHoldMs(text, tone))
  }
  const holdStatus = (hold: boolean) => {
    if (!get().status) return
    if (hold) {
      if (statusTimer === null) return
      clearTimeout(statusTimer)
      timers.delete(statusTimer)
      statusTimer = null
      statusLeft = Math.max(0, statusLeft - (Date.now() - statusFrom))
    } else if (statusTimer === null) {
      // 移开之后至少再留一会儿，不在指针离开的那一刻消失
      runStatusTimer(Math.max(statusLeft, ms(timing.toastHold)))
    }
  }

  /** 文件条目的文件都已不在（记录还在）：本地标为不存在，整行置灰 */
  const markFilesMissing = (item: ClipItemMeta) => {
    const files = (item.files ?? []).map((f) => ({ ...f, exists: false }))
    const cur = visibleHits(get()).find((h) => h.item.id === item.id)?.item ?? item
    set((s) => applyChange(s, { change: 'updated', item: { ...cur, files } }))
    invalidate(item.id)
  }

  /**
   * 命令失败的统一处理：
   * - `not_found`：文件条目的粘贴 / 显示 → 文件都已不存在（记录还在），置灰并提示；图片另存 → 图片文件不在了；
   *   其余 → 条目已不存在，从列表移除；
   * - 其他：提示是哪个操作没有完成
   */
  const failed = (
    command: string,
    error: unknown,
    opts: { id?: number; item?: ClipItemMeta; action: FailedAction },
  ) => {
    const e = WispIpcError.from(command, error)
    const id = opts.id ?? opts.item?.id
    if (e.code === 'not_found' && id !== undefined) {
      const item = opts.item ?? visibleHits(get()).find((h) => h.item.id === id)?.item
      if (
        item?.kind === 'files' &&
        (command === 'clipboard_paste' || command === 'clipboard_reveal')
      ) {
        markFilesMissing(item)
        showStatus(t('clipboard.status.files_gone'), 'error')
        return
      }
      if (command === 'clipboard_save_image') {
        showStatus(t('clipboard.status.image_gone'), 'error')
        return
      }
      invalidate(id)
      set((s) => removeFromLists(s, id))
      showStatus(t('clipboard.status.gone'), 'error')
      return
    }
    console.warn(`[clipboard] ${command} failed: ${e.code}`)
    showStatus(t(`clipboard.status.failed.${opts.action}`), 'error')
  }

  const invalidate = (id: number) => {
    details.delete(id)
    settled.delete(id)
    heads.delete(id)
  }

  const detail = (id: number): Promise<ClipItemDetail | null> => {
    const hit = details.get(id)
    if (hit) {
      // 最近使用的放到末尾（Map 按插入顺序淘汰）
      details.delete(id)
      details.set(id, hit)
      return hit
    }
    const p = api.clipboardGet(id).then(
      (d) => {
        if (d && (d.text?.length ?? 0) > LARGE_TEXT_CHARS) details.delete(id)
        else settled.set(id, d)
        return d
      },
      (error: unknown) => {
        details.delete(id)
        const e = WispIpcError.from('clipboard_get', error)
        if (e.code === 'not_found') set((s) => removeFromLists(s, id))
        return null
      },
    )
    details.set(id, p)
    while (details.size > DETAIL_CACHE) {
      const oldest = details.keys().next().value
      if (oldest === undefined) break
      details.delete(oldest)
      settled.delete(oldest)
    }
    return p
  }

  const textHead = (id: number): Promise<string | null> => {
    const full = settled.get(id)
    if (full !== undefined) return Promise.resolve(full?.text?.slice(0, HOVER_HEAD_CHARS) ?? null)
    if (heads.has(id)) {
      const head = heads.get(id) ?? null
      heads.delete(id)
      heads.set(id, head)
      return Promise.resolve(head)
    }
    return detail(id).then((d) => {
      const head = d?.text?.slice(0, HOVER_HEAD_CHARS) ?? null
      if (d !== null && !disposed) {
        heads.set(id, head)
        while (heads.size > HEAD_CACHE) {
          const oldest = heads.keys().next().value
          if (oldest === undefined) break
          heads.delete(oldest)
        }
      }
      return head
    })
  }

  const cachedHead = (id: number): string | null | undefined => {
    const full = settled.get(id)
    if (full !== undefined) return full?.text?.slice(0, HOVER_HEAD_CHARS) ?? null
    return heads.get(id)
  }

  // ----- 检索

  const runSearch = (query: string, filter: ClipFilter, limit = SEARCH_LIMIT) => {
    const seq = ++searchSeq
    set({ searching: true })
    api.clipboardSearch(query, filter, limit).then(
      (page) => {
        if (seq !== searchSeq || disposed) return
        const s = get()
        if (s.query.trim() !== query || s.filter !== filter) return
        set((cur) => withSearchResult(cur, query, filter, page.items, limit))
      },
      (error: unknown) => {
        if (seq !== searchSeq || disposed) return
        set({ searching: false })
        failed('clipboard_search', error, { action: 'search' })
      },
    )
  }

  const scheduleSearch = (delay = SEARCH_DEBOUNCE_MS) => {
    if (searchTimer !== null) clearTimeout(searchTimer)
    searchTimer = later(delay, () => {
      searchTimer = null
      const s = get()
      const q = s.query.trim()
      if (q) runSearch(q, s.filter, s.search?.query === q ? s.search.limit : SEARCH_LIMIT)
    })
  }

  const setQuery = (query: string) => {
    const before = get()
    if (query === before.query) return
    const wasSearching = isSearching(before)
    if (query.trim() === '') {
      if (searchTimer !== null) clearTimeout(searchTimer)
      searchTimer = null
      searchSeq++
      const recent = recentHits({ ...before, query })
      set((s) => ({
        query,
        search: null,
        searching: false,
        selectedId: recent[0]?.item.id ?? null,
        confirm: s.confirm?.kind === 'delete' ? null : s.confirm,
        scrollReset: wasSearching ? s.scrollReset + 1 : s.scrollReset,
      }))
      return
    }
    set((s) => ({ query, confirm: s.confirm?.kind === 'delete' ? null : s.confirm }))
    scheduleSearch()
  }

  // ----- 过滤

  const loadFiltered = (filter: ClipFilter) => {
    const seq = ++filterSeq
    api.clipboardRecent(0, PAGE_SIZE, filter).then(
      (page) => {
        if (seq !== filterSeq || disposed || get().filter !== filter) return
        set((s) => {
          const keep = page.items.some((h) => h.item.id === s.selectedId)
          return {
            filtered: { filter, hits: page.items, nextOffset: page.next_offset, loaded: true },
            selectedId: keep || isSearching(s) ? s.selectedId : (page.items[0]?.item.id ?? null),
          }
        })
      },
      (error: unknown) => {
        if (seq === filterSeq) failed('clipboard_recent', error, { action: 'load' })
      },
    )
  }

  const setFilter = (filter: ClipFilter) => {
    const before = get()
    if (filter === before.filter) return
    set((s) => {
      const next = { ...s, filter }
      const hits = visibleHits(next)
      return {
        filter,
        selectedId: hits[0]?.item.id ?? null,
        scrollReset: s.scrollReset + 1,
        confirm: s.confirm?.kind === 'delete' ? null : s.confirm,
      }
    })
    const s = get()
    if (isSearching(s)) {
      if (searchTimer !== null) clearTimeout(searchTimer)
      searchTimer = null
      runSearch(s.query.trim(), filter)
    }
    if (filter !== 'all') loadFiltered(filter)
    else filterSeq++
  }

  // ----- 分页

  let loading = false
  const loadMore = () => {
    const s = get()
    if (loading || !canLoadMore(s)) return
    if (isSearching(s)) {
      // 检索结果满了一页：放大 limit 再查一次（进行中的检索没回来之前不重复发）
      const r = s.search
      if (r && !s.searching && r.limit < SEARCH_LIMIT_MAX) {
        runSearch(r.query, r.filter, SEARCH_LIMIT_MAX)
      }
      return
    }
    const filter = s.filter
    const list = filter === 'all' ? s.all : s.filtered
    if (!list || list.nextOffset === null) return
    loading = true
    set({ loadingMore: true })
    api.clipboardRecent(list.nextOffset, PAGE_SIZE, filter).then(
      (page) => {
        loading = false
        if (disposed) return
        set((cur) => {
          const target = filter === 'all' ? cur.all : cur.filtered
          if (!target || target.filter !== filter) return { loadingMore: false }
          const next = {
            ...target,
            hits: appendPage(target.hits, page.items),
            nextOffset: page.next_offset,
          }
          return filter === 'all'
            ? { all: next, loadingMore: false }
            : { filtered: next, loadingMore: false }
        })
      },
      (error: unknown) => {
        loading = false
        set({ loadingMore: false })
        failed('clipboard_recent', error, { action: 'load' })
      },
    )
  }

  // ----- 选中

  const select = (id: number | null, reveal = false, jump = false) => {
    const s = get()
    if (s.selectedId === id && !reveal) return
    set({
      selectedId: id,
      reveal: reveal ? s.reveal + 1 : s.reveal,
      jump,
      confirm: s.confirm?.kind === 'delete' && s.confirm.id !== id ? null : s.confirm,
    })
  }

  // ----- 条目操作

  const target = (id?: number): ClipItemMeta | undefined => {
    const s = get()
    if (id === undefined) return selectedItem(s)
    return visibleHits(s).find((h) => h.item.id === id)?.item
  }

  /**
   * 由 WebView 写入后在原位切到 toast 视图（面板此刻有焦点，toast 在第一次按键时让出焦点）。⌘↩ 仅复制
   * 只提示「已复制」，其余提示「已复制，关闭提示后按 Ctrl+V 粘贴」+ Wayland 的原因
   */
  const pasteViaWebView = (item: ClipItemMeta, mode: PasteMode) => {
    // 在调用方的同一个任务里发起写入（数据可以稍后到），WebKit 才认这是用户手势
    void writeViaWebView(item, () => detail(item.id)).then((ok) => {
      if (disposed) return
      if (!ok) {
        showStatus(t('clipboard.status.failed.copy'), 'error')
        return
      }
      // 粘贴键按平台默认（Ctrl+V）；终端等目标应用的 Ctrl+Shift+V 需要 Rust 在错误里带上
      const key = formatHotkey('Mod+KeyV', host().os)
      host().switchView(
        { kind: 'toast', reason: mode === 'copy_only' ? 'copy_only' : 'wayland', paste_key: key },
        { width: toastWidth(key) },
      )
    })
  }

  const paste = (mode: PasteMode, id?: number) => {
    const item = target(id)
    if (!item || get().busy) return
    if (webviewClipboard && webviewWritable(item)) {
      set({ confirm: null })
      pasteViaWebView(item, mode)
      return
    }
    set({ busy: true, confirm: null })
    api.clipboardPaste(item.id, mode).then(
      () => set({ busy: false }),
      (error: unknown) => {
        set({ busy: false })
        const e = WispIpcError.from('clipboard_paste', error)
        // 后端约定：conflict = 单飞拒绝 / 本次会话已经粘贴过——静默忽略，面板还可见时不能提示「粘贴失败」
        if (e.code === 'conflict') return
        if (e.code === 'platform' && WEBVIEW_COPY_REASONS.has(e.message)) {
          webviewClipboard = true
          if (webviewWritable(item)) pasteViaWebView(item, mode)
          else showStatus(t('clipboard.status.webview_unsupported'), 'error')
          return
        }
        failed('clipboard_paste', error, { item, action: mode === 'copy_only' ? 'copy' : 'paste' })
      },
    )
  }

  const togglePin = (id?: number) => {
    const item = target(id)
    if (!item) return
    const before = get()
    const searchHit = before.search?.hits.find((h) => h.item.id === item.id)
    const next: ClipItemMeta = { ...item, pinned: !item.pinned }
    set((s) => ({
      ...placeItem(s, next),
      moves: s.moves + 1,
      reveal: s.reveal + 1,
      jump: false,
      confirm: null,
    }))
    api.clipboardPin(item.id, next.pinned).then(
      (meta) => set((s) => placeItem(s, meta)),
      (error: unknown) => {
        set((s) => ({
          ...restoreHit(s, { item, highlights: searchHit?.highlights ?? [] }, searchHit),
          moves: s.moves + 1,
        }))
        failed('clipboard_pin', error, { item, action: next.pinned ? 'pin' : 'unpin' })
      },
    )
  }

  const remove = (force: boolean, id?: number) => {
    const item = target(id)
    if (!item) return
    const s = get()
    if (item.pinned && !force) {
      // 置顶项：第一次 ⌫ 只弹出行内确认，2 s 内再按一次才删除；⌘⌫ 直接删
      if (!(s.confirm?.kind === 'delete' && s.confirm.id === item.id)) {
        set({ confirm: { kind: 'delete', id: item.id } })
        return
      }
    }
    // 回滚用：检索结果里的那一条（带高亮）
    const searchHit = s.search?.hits.find((h) => h.item.id === item.id)
    const recentHit = visibleHits(s).find((h) => h.item.id === item.id) ?? {
      item,
      highlights: [],
    }
    invalidate(item.id)
    set((cur) => removeFromLists(cur, item.id))
    api.clipboardDelete(item.id, item.pinned).then(
      () => {},
      (error: unknown) => {
        const e = WispIpcError.from('clipboard_delete', error)
        if (e.code === 'not_found') return
        set((cur) => ({ ...restoreHit(cur, recentHit, searchHit), moves: cur.moves + 1 }))
        failed('clipboard_delete', error, { action: 'delete' })
      },
    )
  }

  const cancelConfirm = () => set({ confirm: null })
  const askClear = () => set({ confirm: { kind: 'clear', keepPinned: true }, preview: false })
  const setKeepPinned = (keep: boolean) =>
    set((s) =>
      s.confirm?.kind === 'clear' ? { confirm: { kind: 'clear', keepPinned: keep } } : {},
    )

  const clear = () => {
    const c = get().confirm
    if (c?.kind !== 'clear') return
    const keep = c.keepPinned
    // 回滚用的快照：清空失败时各列表、选中项原样恢复（数据仍在库里）
    const snap = get()
    details.clear()
    settled.clear()
    heads.clear()
    set((s) => applyChange(s, { change: 'cleared', keep_pinned: keep }))
    set({ confirm: null })
    api.clipboardClear(keep).then(
      () => {},
      (error: unknown) => {
        if (!disposed) {
          set((s) => ({
            all: snap.all,
            filtered: snap.filtered,
            search: snap.search,
            selectedId: snap.selectedId,
            moves: s.moves + 1,
          }))
        }
        failed('clipboard_clear', error, { action: 'clear' })
      },
    )
  }

  const setPaused = (paused: boolean) => {
    const before = get().paused
    set({ paused })
    api.clipboardSetPaused(paused).then(
      () => {},
      (error: unknown) => {
        set({ paused: before })
        failed('clipboard_set_paused', error, { action: paused ? 'pause' : 'resume' })
      },
    )
  }

  const setPreview = (open: boolean) => {
    const s = get()
    if (open && !selectedItem(s)) return
    if (s.preview !== open) set({ preview: open })
  }

  const saveImage = (id: number) => {
    api.clipboardSaveImage(id).then(
      (path) => {
        if (path) showStatus(t('clipboard.status.saved'))
      },
      (error: unknown) => failed('clipboard_save_image', error, { id, action: 'save_image' }),
    )
  }

  const reveal = (id: number) => {
    const item = target(id)
    api
      .clipboardReveal(id)
      .catch((error: unknown) => failed('clipboard_reveal', error, { id, item, action: 'reveal' }))
  }

  const openLink = (id: number) => {
    api
      .clipboardOpenLink(id)
      .catch((error: unknown) => failed('clipboard_open_link', error, { id, action: 'open_link' }))
  }

  /**
   * 送翻译：优先 `clipboard_send_to(translate)`——正文留在 Rust 里取（最长 1 MB，不经 WebView 往返），编排器以
   * `TriggerOrigin::Clipboard` 发起交叉淡入的 prepare；命令尚未实现时退回交接里的写法
   * `trigger_action(translate_selection, text, origin: clipboard)`（两者在 Rust 里是同一条 `TranslateText` 命令）
   */
  const translate = (id: number) => {
    const item = target(id)
    if (!item) return
    void (async () => {
      try {
        await api.clipboardSendTo(id, 'translate')
        return
      } catch (error) {
        if (WispIpcError.from('clipboard_send_to', error).code !== 'not_implemented') {
          failed('clipboard_send_to', error, { id, action: 'translate' })
          return
        }
      }
      const text = (await detail(id))?.text ?? item.preview
      if (!text.trim()) return
      try {
        await api.triggerAction('translate_selection', 'clipboard', text)
      } catch (error) {
        failed('trigger_action', error, { action: 'translate' })
      }
    })()
  }

  const openSettings = () => {
    api
      .windowOpen({ kind: 'settings', section: 'clipboard' })
      .catch((error: unknown) => failed('window_open', error, { action: 'settings' }))
  }

  // ----- 实时事件

  const onChanged = (change: ClipboardChanged) => {
    if (change.change === 'updated' || change.change === 'touched') invalidate(change.item.id)
    if (change.change === 'removed') invalidate(change.id)
    if (change.change === 'secret_skipped') {
      showStatus(t('clipboard.status.secret_skipped'), 'info')
      return
    }
    const before = get().fresh
    set((s) => applyChange(s, change))
    const added = [...get().fresh].filter((id) => !before.has(id))
    if (added.length > 0) {
      // rowInsert 播完后清掉标记：行被滚出再滚回来时不会重播
      later(ms(duration.slow), () =>
        set((s) => {
          const fresh = new Set(s.fresh)
          for (const id of added) fresh.delete(id)
          return { fresh }
        }),
      )
    }
    if ((change.change === 'inserted' || change.change === 'touched') && isSearching(get())) {
      scheduleSearch()
    }
  }

  return {
    setQuery,
    setFilter,
    select,
    loadMore,
    paste,
    togglePin,
    remove,
    cancelConfirm,
    askClear,
    setKeepPinned,
    clear,
    setPaused,
    setPreview,
    saveImage,
    reveal,
    openLink,
    translate,
    openSettings,
    detail,
    cachedDetail: (id) => settled.get(id),
    textHead,
    cachedHead,
    showStatus,
    holdStatus,
    setHost: (next) => {
      currentHost = next
    },
    attach: () => {
      disposed = false
      // 常驻订阅（面板启动时已装好；单测与 mock 里第一次 attach 时补装）按当前挂载的视图分发。每次 attach 用新的
      // 闭包：StrictMode 的「挂载 → 卸载 → 再挂载」里，上一次迟到的解绑不会把这一次的也解掉
      installClipboardEvents()
      const sink = (change: ClipboardChanged) => onChanged(change)
      changedSink = sink
      return () => {
        if (changedSink === sink) changedSink = null
        disposed = true
        for (const id of timers) clearTimeout(id)
        timers.clear()
        searchTimer = null
        statusTimer = null
        details.clear()
        settled.clear()
        heads.clear()
      }
    },
  }
}

/** WebView 能代写的条目：文本类（纯文本）与图片（PNG）；文件条目没有可移植的写法 */
function webviewWritable(item: ClipItemMeta): boolean {
  return isTextual(item) || item.kind === 'image'
}

/**
 * 平台层要求由 WebView 写剪贴板时（面板仍有焦点）。`write` 在调用方的任务里同步发起，数据用 Promise 稍后
 * 给出（WebKit 要求写入发生在用户手势里，取详情是一次 IPC 往返）；没有 `ClipboardItem` 时退回 `writeText`
 */
async function writeViaWebView(
  item: ClipItemMeta,
  load: () => Promise<ClipItemDetail | null>,
): Promise<boolean> {
  const clip = typeof navigator === 'undefined' ? undefined : navigator.clipboard
  if (!clip) return false
  const canItem = typeof ClipboardItem !== 'undefined' && typeof clip.write === 'function'
  try {
    if (item.kind === 'image') {
      if (!canItem) return false
      const png = load().then(async (d) => {
        if (!d?.image_url) throw new Error('image missing')
        const blob = await (await fetch(resolveAssetUrl(d.image_url))).blob()
        return blob.type === 'image/png' ? blob : new Blob([blob], { type: 'image/png' })
      })
      await clip.write([new ClipboardItem({ 'image/png': png })])
      return true
    }
    const text = load().then((d) => d?.text ?? item.link ?? item.color ?? item.preview)
    if (canItem) {
      const blob = text.then((s) => new Blob([s], { type: 'text/plain' }))
      await clip.write([new ClipboardItem({ 'text/plain': blob })])
    } else {
      await clip.writeText(await text)
    }
    return true
  } catch {
    return false
  }
}

/** 测试用：重置「本 WebView 要由前端写剪贴板」的记忆 */
export function resetWebviewClipboardForTest(): void {
  webviewClipboard = false
}
