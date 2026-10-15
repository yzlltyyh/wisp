/**
 * 剪贴板视图：版式与 design.clipboard-* 视觉稿一致——
 * 头部（大号无边框搜索 · 类型过滤「全部 ▾」· ⋯）/ 暂停横幅 / 列表（「置顶 / 最近」两组，检索结果平铺；唯一的滑动
 * 选中指示器）/ 底部快捷键提示条。
 *
 * 键位表见 keys.ts。
 *
 * - prepare 同步路径：首屏只用载荷里 Rust 缓存的首页（不查库），挂载时不读布局、不依赖 rAF；
 *   高度按内容一次算出（显式 px，不用 100% / vh：Native 帧模式下 WebView 比卡片高）：clamp(内容, 下限, min(560, 上限))，会话内只增不减
 *   （搜索 / 过滤让结果变少时不缩小，只有实时插入的新条目能让它变高，与 Rust 的锁定规则一致）；
 *   长高时底部的提示条 / 确认条与卡片当前的底边一起下移（spring.gentle，与卡片高度跟随同一条弹簧），不会被裁掉；
 * - 焦点一直在搜索框里（「唤出即可打字」，focus.ts）：自动聚焦；面板进入 entering / shown、窗口重新获得焦点
 *   时焦点丢了就补；点非输入区域时焦点不离开；点过可选中的正文之后打字照样落进搜索框；每个菜单第一次打开时把焦点
 *   移进菜单。输入即搜索、去抖 60 ms，结果保留上一次的直到新的到达；检索中的转圈与放大镜交叉淡入 / 淡出，头部
 *   任何元素都不移动；
 * - 键盘由统一的路由拦截（视图层 + 预览 / 确认条的 overlay 层）；按住 ⌘ 时行尾显示 ⌘1–9 角标；⌘K 打开操作菜单；
 *   清空确认是模态的：只接受 ↩ 确认、Esc 取消、Space 切换「保留置顶项」，其余按键与指针都不落到列表上；确认条上沿
 *   的渐隐遮罩随确认条一起淡入淡出；
 * - 全文预览的代码（`./preview`）在视图显示后空闲时预取并缓存（previewModule.ts），打开 / 关闭都是真正的交叉淡入；
 * - 隐藏时外壳卸载整个视图：列表、详情缓存、计时器随之释放（隐藏时只保留 Rust 缓存的首屏数据）；原图只在预览里请求，卸载时清空 src。
 */
import './clipboard.css'
import {
  CircleAlert,
  CircleCheck,
  DatabaseZap,
  Ellipsis,
  Inbox,
  SearchX,
  ShieldCheck,
} from 'lucide-react'
import {
  animateSingleValue,
  AnimatePresence,
  cancelFrame,
  frame as motionFrame,
  m,
} from 'motion/react'
import {
  memo,
  useCallback,
  useEffect,
  useId,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type RefObject,
} from 'react'
import { useStore } from 'zustand'
import type { ViewProps } from '../../panel/types'
import { boot } from '../../shared/ipc'
import type { ClipFilter, ClipHit, ClipKind, OsKind } from '../../shared/ipc/types'
import {
  PREWARM_ORDER,
  prewarmRender,
  schedulePrewarm,
  usePrewarm,
} from '../../shared/perf/prewarm'
import { useHidden } from '../../shared/hooks/useHidden'
import { usePanelDrag, type PanelDragHandlers } from '../../shared/hooks/usePanelDrag'
import { usePlatform } from '../../shared/hooks/usePlatform'
import { useViewActive } from '../../shared/hooks/useViewActive'
import { Trans } from '../../shared/i18n/Trans'
import { formatNumber } from '../../shared/i18n/format'
import { useT, useTKey, type TFunction } from '../../shared/i18n/useT'
import { useKeyScope } from '../../shared/keys/useKeyScope'
import { usePreset } from '../../shared/motion/presets'
import { duration, ms, spring, timing } from '../../shared/motion/tokens'
import { useReducedMotionPref } from '../../shared/motion/useReducedMotionPref'
import {
  Banner,
  Button,
  Checkbox,
  Chip,
  EmptyState,
  Icon,
  IconButton,
  InlineConfirm,
  Kbd,
  Menu,
  SearchField,
  Spinner,
} from '../../shared/ui'
import { ClipList } from './ClipList'
import { keepSearchFocus, useRefocusWhenShown, useTypeToSearch } from './focus'
import { warmRowTimeFormats } from './format'
import { loadClipboardMessages } from './i18n'
import { rowDomId } from './ClipRow'
import { clipboardBindings, deleteCombo, once } from './keys'
import { hiddenHints } from './hints'
import { filterMenuItems, moreMenuItems } from './menus'
import {
  BANNER_HEIGHT,
  cycleFilter,
  EMPTY_HEIGHT,
  FILTERS,
  firstItemIndex,
  HEADER_HEIGHT,
  HINT_HEIGHT,
  indexOfId,
  lastItemIndex,
  listContentHeight,
  nthItem,
  OPEN_MAX_HEIGHT,
  openHeight,
  rowOffsets,
  screenRows,
  SHORT_QUERY_CHARS,
  SHORT_QUERY_SCOPE,
  stepItem,
} from './model'
import { preloadPreview, usePreviewModule } from './previewModule'
import {
  createClipStore,
  isSearching,
  rowsOf,
  selectedItem,
  type ClipStore,
  type Status,
} from './store'
import { useClipboard, type ClipActions } from './useClipboard'

loadClipboardMessages()

// 面板窗口常驻：启动后空闲时预热行尾时间的格式器（不在第一次唤出的同步渲染里构造）
if (typeof window !== 'undefined') {
  const warm = () => warmRowTimeFormats()
  if (typeof window.requestIdleCallback === 'function') window.requestIdleCallback(warm)
  else setTimeout(warm, 0)
}

/** 相对时间刷新间隔（显示期间；隐藏时停掉） */
const CLOCK_MS = 60_000

/** 焦点在视图里的按钮 / 链接上（Tab 进了预览）：↩ / Space 交给它本身 */
function controlFocused(): boolean {
  const el = document.activeElement
  return el instanceof HTMLButtonElement || el instanceof HTMLAnchorElement
}

type ElementRef<T extends HTMLElement> = RefObject<T | null>

/** 预览打开时 Tab / ⇧Tab：在搜索框与预览的按钮之间移动焦点（不再切换过滤） */
function cycleFocusIn(
  root: ElementRef<HTMLElement>,
  search: ElementRef<HTMLInputElement>,
  step: 1 | -1,
): void {
  const area = root.current?.querySelector<HTMLElement>('.clip-preview:not([data-exiting])')
  const input = search.current
  if (!area || !input) return
  const controls = [
    ...area.querySelectorAll<HTMLElement>('button:not([disabled]), a[href]'),
  ].filter((el) => el.getAttribute('aria-hidden') !== 'true')
  const ring: HTMLElement[] = [input, ...controls]
  const at = Math.max(0, ring.indexOf(document.activeElement as HTMLElement))
  ring[(((at + step) % ring.length) + ring.length) % ring.length]?.focus({ preventScroll: true })
}

/** 跟随卡片底边最长多久（卡片高度动画之外的兜底：卡片没有动画时不会一直跟） */
const FOLLOW_MAX_MS = 1000

/**
 * 会话内长高（实时插入、右键菜单需要更多空间）：视图的布局高度一次到位（外壳只量一次、只上报一次），而卡片
 * （Css 帧模式的卡片高度跟随 / Native 帧模式 Rust 的窗口弹簧）还在长。底部的提示条、确认条与轻提示整体上移到卡片
 * 当前的底边处，列表区在它们上沿裁切，卡片长完时回到原位：
 * - follow（Css 帧模式）：每帧在 Motion 写完卡片高度之后读卡片的实际底边（只在长高的这一小段时间里）；
 * - spring（Native 帧模式）：卡片的可见范围由原生窗口裁切，DOM 里量不到，用与 Rust 窗口同一条 spring.gentle；
 * - none：不能透明的窗口（没有高度动画，直接到位）、减少动效。
 * 偏移在布局阶段同步写入：长高的那一帧底部不会先跳到新位置再弹回
 */
function useGrowFollow(
  root: ElementRef<HTMLElement>,
  total: number,
  mode: 'follow' | 'spring' | 'none',
): void {
  const laidOut = useRef(total)
  const gap = useRef(0)
  const stop = useRef<(() => void) | null>(null)
  useLayoutEffect(() => {
    const before = laidOut.current
    laidOut.current = total
    const el = root.current
    if (!el || total <= before || mode === 'none') return
    const footers = [
      ...el.querySelectorAll<HTMLElement>(':scope > .clip-hints, :scope > .clip-footer-layer'),
    ]
    const body = el.querySelector<HTMLElement>(':scope > .clip-body')
    const apply = (v: number) => {
      gap.current = v
      const shift = v > 0.5 ? `translateY(${-v}px)` : ''
      for (const f of footers) f.style.transform = shift
      if (body) body.style.clipPath = v > 0.5 ? `inset(0 0 ${v}px 0)` : ''
    }
    stop.current?.()
    apply(gap.current + (total - before))
    if (mode === 'spring') {
      const a = animateSingleValue(gap.current, 0, { ...spring.gentle, onUpdate: apply })
      stop.current = () => a.stop()
      return
    }
    const card = el.closest<HTMLElement>('.panel-card')
    if (!card) {
      apply(0)
      return
    }
    const started = performance.now()
    const follow = () => {
      const v = Math.max(0, el.getBoundingClientRect().bottom - card.getBoundingClientRect().bottom)
      apply(v)
      if (v <= 0.5 || performance.now() - started > FOLLOW_MAX_MS) {
        cancelFrame(follow)
        apply(0)
      }
    }
    motionFrame.postRender(follow, true)
    stop.current = () => cancelFrame(follow)
  }, [total, mode, root])
  useEffect(() => () => stop.current?.(), [])
}

function scrollBy(el: ElementRef<HTMLDivElement>, step: 1 | -1): void {
  const box = el.current
  if (box) box.scrollTop += step * Math.max(40, box.clientHeight - 40)
}

export function ClipboardView(props: ViewProps<'clipboard'>) {
  const { payload, sessionId, maxHeight, switchView, focused, frame, phase } = props
  const t = useT()
  const tKey = useTKey()
  const prewarm = usePrewarm()
  useEffect(() => {
    if (!prewarm) clipboardMounted = true
    // 预热标记在一个实例里不变
    // oxlint-disable-next-line react/exhaustive-deps
  }, [])
  const os = usePlatform()
  const active = useViewActive()
  const hidden = useHidden()
  const reduced = useReducedMotionPref()
  const [store] = useState(() => createClipStore(payload))
  // 阶段决定实时的 `reloaded` 什么时候处理（显示中暂存，见 useClipboard）
  const actions = useClipboard(store, { os, switchView }, phase)

  // 订阅列表的各个来源（任一变化都重新取行）；行本身由 rowsOf 按 store 缓存
  const all = useStore(store, (s) => s.all)
  useStore(store, (s) => s.filtered)
  useStore(store, (s) => s.search)
  const query = useStore(store, (s) => s.query)
  const filter = useStore(store, (s) => s.filter)
  const now = useStore(store, (s) => s.now)
  const paused = useStore(store, (s) => s.paused)
  const preview = useStore(store, (s) => s.preview)
  const confirm = useStore(store, (s) => s.confirm)
  const status = useStore(store, (s) => s.status)
  const searching = useStore(store, (s) => s.searching)
  const selectedId = useStore(store, (s) => s.selectedId)
  const rows = rowsOf(store)
  const offsets = useMemo(() => rowOffsets(rows), [rows])
  const empty = rows.length === 0
  // 记录状态（优先级：历史库不可用 > 监听装不上 > 设置里关闭了监听 > 已暂停）：横幅告诉用户新的复制不会被记录，免得
  // 以为一切照常。历史库不可用且列表为空时原因放在列表区里（不显示「复制点什么，它会出现在这里」这种相反的空状态），
  // 不再另起横幅。暂停状态以 store 为准（托盘 / ⋯ 菜单切换时实时更新）。改这里的条件时同步 Rust 的
  // `placement::clipboard_has_banner`（按同一规则预估打开高度）
  const unavailable = payload.unavailable ?? null
  const watch = payload.recording
  const recording: 'history' | 'watch' | 'disabled' | 'paused' | null = unavailable
    ? 'history'
    : watch?.state === 'unavailable'
      ? 'watch'
      : watch?.state === 'disabled'
        ? 'disabled'
        : paused
          ? 'paused'
          : null
  const banner = recording === 'history' && empty ? null : recording
  const searchingMode = isSearching({ ...store.getState(), query })
  const flat = searchingMode && store.getState().search !== null

  // ----- 全文预览的代码：显示后空闲时预取；预取之前就按了 Space 时立即加载，到达之前列表不淡出
  const previewMod = usePreviewModule(preview)
  useEffect(() => {
    if (!active) return
    const load = () => void preloadPreview().catch(() => undefined)
    if (typeof window.requestIdleCallback !== 'function') {
      const id = setTimeout(load, 0)
      return () => clearTimeout(id)
    }
    const id = window.requestIdleCallback(load)
    return () => window.cancelIdleCallback(id)
  }, [active])
  const selected = selectedItem(store.getState())
  const FullPreview = previewMod?.FullPreview
  const showPreview = preview && !!selected && !!FullPreview

  // ----- 高度：clamp(内容, 下限, min(560, 上限))，会话内只增不减（记住出现过的最大值：渲染期更新，不多一次提交）
  const hintBar = payload.hint_bar
  const chrome = HEADER_HEIGHT + (banner ? BANNER_HEIGHT : 0) + (hintBar ? HINT_HEIGHT : 0)
  const natural = chrome + (rows.length > 0 ? listContentHeight(rows) : EMPTY_HEIGHT)
  const cap = Math.min(OPEN_MAX_HEIGHT, maxHeight)
  const [peak, setPeak] = useState(0)
  const [extra, setExtra] = useState(0)
  const total = Math.max(peak, openHeight(natural, maxHeight), Math.min(extra, cap))
  if (total !== peak) setPeak(total)
  const listHeight = Math.max(0, total - chrome)
  const ensureHeight = useCallback((bottom: number) => {
    setExtra((e) => Math.max(e, Math.ceil(bottom)))
  }, [])

  // ----- 显示期间每分钟刷新相对时间；隐藏 / 暂存时不跑
  useEffect(() => {
    if (!active || hidden) return
    const id = setInterval(() => store.setState({ now: Date.now() }), CLOCK_MS)
    return () => clearInterval(id)
  }, [store, active, hidden])

  // ----- 唤出即可打字：面板进入 entering / shown（窗口这时才真正可见）、窗口重新获得焦点、页面重新可见时，
  // 焦点丢了就补聚焦搜索框——挂载时窗口还不可见，autoFocus 的 focus() 不生效，而 focused / active / hidden 在显示时
  // 都不会变（见 focus.ts）。点了非输入区域时焦点不离开搜索框；焦点还是丢了时打字照样落进搜索框
  const root = useRef<HTMLElement>(null)
  const searchRef = useRef<HTMLInputElement>(null)
  const shown = phase === 'entering' || phase === 'shown'
  useRefocusWhenShown(root, searchRef, active && !hidden && focused && shown)
  useTypeToSearch(root, searchRef, active && !hidden)
  // 长高时底部的提示条 / 确认条 / 轻提示跟着卡片当前的底边走（见 useGrowFollow）
  useGrowFollow(
    root,
    total,
    frame === 'css' ? (boot.shadow_margin > 0 ? 'follow' : 'none') : reduced ? 'none' : 'spring',
  )

  // ----- 检索中的转圈：检索超过 slow 才出现（大多数检索 < 50 ms，不闪）。先以透明挂载，到时与放大镜在同一位置、同一次
  // 样式计算里交叉淡入；检索结束时同样交叉淡出（转圈播完淡出再卸载），任何一帧两者的不透明度之和都是 1
  const [spin, setSpin] = useState<'off' | 'armed' | 'on' | 'leaving'>('off')
  const [spinFor, setSpinFor] = useState(searching)
  if (spinFor !== searching) {
    // 检索开始 / 结束的那次渲染里就切换（不多一次提交）：开始 → 透明挂载；结束 → 已显示的淡出，没显示的直接卸载
    setSpinFor(searching)
    if (searching) setSpin(spin === 'on' ? 'on' : 'armed')
    else setSpin(spin === 'on' || spin === 'leaving' ? 'leaving' : 'off')
  }
  useEffect(() => {
    if (spin !== 'armed' && spin !== 'leaving') return
    const next = spin === 'armed' ? 'on' : 'off'
    const id = setTimeout(() => setSpin(next), ms(spin === 'armed' ? duration.slow : duration.fast))
    return () => clearTimeout(id)
  }, [spin])

  // ----- 按住 ⌘（Windows / Linux 为 Ctrl）时行尾显示 ⌘n 角标（CSS 延迟淡入，快捷键一按即放不会闪）
  useEffect(() => {
    if (!active) return
    const mod = (e: KeyboardEvent) => (os === 'macos' ? e.metaKey : e.ctrlKey)
    const sync = (held: boolean) => {
      const el = root.current
      if (!el) return
      if (held) el.dataset.modHeld = ''
      else delete el.dataset.modHeld
    }
    const onKey = (e: KeyboardEvent) => sync(mod(e))
    const onBlur = () => sync(false)
    window.addEventListener('keydown', onKey, true)
    window.addEventListener('keyup', onKey, true)
    window.addEventListener('blur', onBlur)
    return () => {
      window.removeEventListener('keydown', onKey, true)
      window.removeEventListener('keyup', onKey, true)
      window.removeEventListener('blur', onBlur)
      sync(false)
    }
  }, [active, os])

  // ----- 菜单
  const filterRef = useRef<HTMLButtonElement>(null)
  const moreRef = useRef<HTMLButtonElement>(null)
  const [filterMenu, setFilterMenu] = useState<'pointer' | 'keyboard' | null>(null)
  const [moreMenu, setMoreMenu] = useState<'pointer' | 'keyboard' | null>(null)
  // 菜单第一次打开时才挂载（之后保留，退出动画照常播放）：唤出面板的同步渲染（prepare）少做两棵子树
  const [filterMenuUsed, setFilterMenuUsed] = useState(false)
  const [moreMenuUsed, setMoreMenuUsed] = useState(false)
  if (filterMenu !== null && !filterMenuUsed) setFilterMenuUsed(true)
  if (moreMenu !== null && !moreMenuUsed) setMoreMenuUsed(true)
  // ⋯ 菜单里有「清空历史…」：打开菜单时先读清空会不会同步到其他设备（打开面板时不读设置）
  const moreOpen = moreMenu !== null
  useEffect(() => {
    if (moreOpen) actions.prepareClear()
  }, [moreOpen, actions])

  // ----- 预览
  const previewScroll = useRef<HTMLDivElement>(null)
  const closePreview = () => actions.setPreview(false)
  // 关闭预览 / 换条目时：焦点在预览里的按钮上（Tab 进去的，按钮正在随预览退出或已经消失）就还给搜索框
  const wasPreview = useRef(preview)
  useEffect(() => {
    const closed = wasPreview.current && !preview
    wasPreview.current = preview
    const at = document.activeElement
    const search = searchRef.current
    if (!search || at === search) return
    const lost = !at || at === document.body
    const inPreview = !!at && !!at.closest('.clip-preview')
    if ((preview && lost) || (closed && (lost || inPreview))) search.focus({ preventScroll: true })
    // selectedId：预览里换条目之后检查一次
    // oxlint-disable-next-line react/exhaustive-effect-dependencies
  }, [preview, selectedId])

  // ----- 键盘
  const move = (step: 1 | -1, count = 1, jump = false) => {
    const r = rowsOf(store)
    const from = indexOfId(r, store.getState().selectedId)
    const next = r[stepItem(r, from, step, count)]
    if (next?.type === 'item') actions.select(next.key, true, jump)
  }
  const bindings = clipboardBindings(os, {
    queryEmpty: () => store.getState().query === '',
    controlFocused,
    // 按住 ↑↓ 的重复事件：指示器直接到位（spring 追不上 30 次 / s 的移动）
    move: (step, repeat) => move(step, 1, repeat),
    page: (step) => move(step, Math.max(1, screenRows(listHeight) - 1), true),
    edge: (which) => {
      const r = rowsOf(store)
      const row = r[which === 'first' ? firstItemIndex(r) : lastItemIndex(r)]
      if (row?.type === 'item') actions.select(row.key, true, true)
    },
    paste: (mode) => actions.paste(mode),
    pasteNth: (n, mode) => {
      const row = nthItem(rowsOf(store), n)
      if (row) actions.paste(mode === 'plain' ? 'plain' : 'normal', row.key)
    },
    togglePin: () => actions.togglePin(),
    remove: (force) => actions.remove(force),
    togglePreview: () => actions.setPreview(!store.getState().preview),
    cycleFilter: (step) => actions.setFilter(cycleFilter(store.getState().filter, step)),
    openFilterMenu: () => setFilterMenu('keyboard'),
    // 选中行的操作菜单由列表打开；没有选中行（空列表）时打开 ⋯ 菜单
    openActions: () => {
      const s = store.getState()
      if (selectedItem(s)) store.setState({ actionMenu: s.actionMenu + 1 })
      else setMoreMenu('keyboard')
    },
    escape: () => {
      const s = store.getState()
      if (s.confirm) {
        actions.cancelConfirm()
        return true
      }
      if (s.query !== '') {
        actions.setQuery('')
        return true
      }
      return false
    },
  })
  useKeyScope('clipboard', bindings)
  // 全文预览：Esc / Space 返回（搜索框有字时 Space 同样返回）；↑↓ 切换条目（视图层处理）；PageUp / PageDown 滚动预览；
  // Tab / ⇧Tab 在预览的按钮之间移动焦点
  const notOnControl = () => !controlFocused()
  const closeOnce = (e: KeyboardEvent) => {
    if (!e.repeat) closePreview()
  }
  useKeyScope(
    'clipboard-preview',
    [
      { combo: 'Escape', allowInInput: true, run: () => closePreview() },
      { combo: 'Space', allowInInput: true, when: notOnControl, run: closeOnce },
      { combo: 'Shift+Space', allowInInput: true, when: notOnControl, run: closeOnce },
      { combo: 'PageDown', allowInInput: true, run: () => scrollBy(previewScroll, 1) },
      { combo: 'PageUp', allowInInput: true, run: () => scrollBy(previewScroll, -1) },
      { combo: 'Tab', allowInInput: true, run: () => cycleFocusIn(root, searchRef, 1) },
      { combo: 'Shift+Tab', allowInInput: true, run: () => cycleFocusIn(root, searchRef, -1) },
    ],
    { layer: 'overlay', enabled: showPreview },
  )
  // 清空确认条（模态）：↩ 确认、Esc 取消、Space 切换「保留置顶项」；其余按键在下面一并吞掉
  const clearOpen = confirm?.kind === 'clear'
  useKeyScope(
    'clipboard-clear',
    [
      { combo: 'Enter', allowInInput: true, run: once(() => actions.clear()) },
      { combo: 'Escape', allowInInput: true, run: () => actions.cancelConfirm() },
      {
        combo: 'Space',
        allowInInput: true,
        run: once(() => {
          const c = store.getState().confirm
          if (c?.kind === 'clear') actions.setKeepPinned(!c.keepPinned)
        }),
      },
    ],
    { layer: 'overlay', enabled: clearOpen, modal: true },
  )
  useEffect(() => {
    if (!clearOpen) return
    // 键盘路由在更早注册的捕获监听里处理了 ↩ / Esc / Space；没处理的键（方向键、⌫、字母…）不落到搜索框与列表上
    const swallow = (e: KeyboardEvent) => {
      if (e.defaultPrevented || e.isComposing) return
      if (['Shift', 'Control', 'Alt', 'Meta'].includes(e.key)) return
      e.preventDefault()
      e.stopPropagation()
    }
    window.addEventListener('keydown', swallow, true)
    return () => window.removeEventListener('keydown', swallow, true)
  }, [clearOpen])
  // 确认条上沿的渐隐遮罩：确认条显示期间与关闭后的交叉淡出期间存在（CSS 里随确认条一起过渡，见 clipboard.css）。
  // 没有提示条时，底部的轻提示同样用它（胶囊停在列表上，不能压着行文字）
  const confirmOpen = confirm !== null
  const statusDocked = status !== null && !hintBar
  const masked = confirmOpen || statusDocked
  const [dock, setDock] = useState<'off' | 'on' | 'leaving'>('off')
  const [dockFor, setDockFor] = useState(masked)
  if (dockFor !== masked) {
    setDockFor(masked)
    setDock(masked ? 'on' : 'leaving')
  }
  useEffect(() => {
    if (dock !== 'leaving') return
    const id = setTimeout(() => setDock('off'), ms(timing.crossfade + timing.exitSlack))
    return () => clearTimeout(id)
  }, [dock])
  // 删除置顶项的行内确认：再按一次 ⌫ 只作用于发起确认的那一条；Esc 取消
  const deleteId = confirm?.kind === 'delete' ? confirm.id : null
  const confirmDelete = () => {
    const c = store.getState().confirm
    if (c?.kind === 'delete') actions.remove(true, c.id)
  }
  useKeyScope(
    'clipboard-delete',
    [
      ...(os === 'macos'
        ? ['Backspace', 'Delete', 'Mod+Backspace', 'Mod+Delete']
        : ['Delete', 'Control+Delete']
      ).map((combo) => ({
        combo,
        allowInInput: true,
        when: () =>
          combo.startsWith('Mod') || combo.startsWith('Control') || store.getState().query === '',
        run: once(confirmDelete),
      })),
      { combo: 'Escape', allowInInput: true, run: () => actions.cancelConfirm() },
    ],
    { layer: 'overlay', enabled: deleteId !== null },
  )

  // ----- 拖动（shared/hooks/usePanelDrag；「记住位置」：Rust 在隐藏时按显示器记下位置）。只从头部 / 提示条的空白处
  // 开始（列表、预览与状态条另有按下的含义）；mousedown 的默认行为由根节点的 keepSearchFocus 阻止（焦点留在搜索框）
  const drag = usePanelDrag(sessionId)

  const listboxId = useId()

  return (
    // 根节点的 mousedown 只用来让焦点留在搜索框里（keepSearchFocus），不是交互控件
    // oxlint-disable-next-line jsx-a11y/no-static-element-interactions
    <section
      ref={root}
      className="clip"
      style={{ height: total, '--clip-crossfade': `${ms(timing.crossfade)}ms` } as CSSProperties}
      data-testid="view-clipboard"
      data-view="clipboard"
      data-session={sessionId}
      data-preview={showPreview || undefined}
      data-hint-bar={hintBar || undefined}
      data-modal={clearOpen || undefined}
      data-confirm={confirm ? confirm.kind : undefined}
      data-dock-mask={dock !== 'off' || masked || undefined}
      data-status-docked={statusDocked || undefined}
      data-status-bar={(status !== null && hintBar) || undefined}
      // 点在非输入区域（列表空白、行、按钮、横幅、确认条、空状态）时焦点不离开搜索框（见 focus.ts）
      onMouseDown={keepSearchFocus}
    >
      <header
        className="clip-head"
        data-spin={spin === 'on' || undefined}
        onPointerDown={drag?.onPointerDown}
      >
        <SearchField
          ref={searchRef}
          variant="bare"
          value={query}
          onChange={actions.setQuery}
          onClear={() => {
            actions.setQuery('')
            searchRef.current?.focus({ preventScroll: true })
          }}
          placeholder={t('clipboard.search.placeholder')}
          aria-label={t('clipboard.a11y.search')}
          // 组合框（ARIA 1.2 的 combobox 模式，元素本身就是 input）：列表一直展开（读屏按 activedescendant 播报
          // 选中项），aria-controls 指向 listbox 本身
          // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role
          role="combobox"
          aria-expanded={!empty}
          aria-controls={empty ? undefined : listboxId}
          aria-activedescendant={selectedId !== null && !empty ? rowDomId(selectedId) : undefined}
          aria-autocomplete="list"
          // 唤出即可打字；视图在切换暂存层里时 useAutoFocus 推迟到显示时
          // oxlint-disable-next-line jsx-a11y/no-autofocus
          autoFocus
          data-testid="clip-search"
          // 转圈与放大镜在同一个位置交叉淡入 / 淡出（绝对定位，不占位：清除按钮不会横跳）
          accessory={
            spin === 'off' ? null : (
              <span
                className="clip-spinner"
                data-on={spin === 'on' || undefined}
                data-testid="clip-spinner"
              >
                <Spinner size={14} />
              </span>
            )
          }
        />
        <Chip
          ref={filterRef}
          variant="subtle"
          dropdown
          selected={filter !== 'all'}
          aria-haspopup="menu"
          aria-expanded={filterMenu !== null}
          aria-label={t('clipboard.a11y.filter', { filter: t(`clipboard.filter.${filter}`) })}
          onClick={() => setFilterMenu((v) => (v ? null : 'pointer'))}
          data-testid="clip-filter"
        >
          <FilterLabel filter={filter} t={t} />
        </Chip>
        <IconButton
          ref={moreRef}
          icon={Ellipsis}
          label={t('common.a11y.more_actions')}
          aria-haspopup="menu"
          aria-expanded={moreMenu !== null}
          onClick={() => setMoreMenu((v) => (v ? null : 'pointer'))}
          data-testid="clip-more"
        />
      </header>
      {filterMenuUsed ? (
        <Menu
          open={filterMenu !== null}
          anchor={filterRef}
          align="end"
          flip={false}
          items={filterMenuItems(filter, t, actions.setFilter)}
          initialActive={filterMenu === 'keyboard' ? 'selected' : 'none'}
          label={t('clipboard.filter.menu')}
          returnFocus={searchRef}
          data-testid="clip-filter-menu"
          onClose={() => setFilterMenu(null)}
        />
      ) : null}
      {moreMenuUsed ? (
        <Menu
          open={moreMenu !== null}
          anchor={moreRef}
          align="end"
          flip={false}
          items={moreMenuItems(paused, all.hits.length > 0, t, actions)}
          initialActive={moreMenu === 'keyboard' ? 'first' : 'none'}
          label={t('clipboard.a11y.more')}
          returnFocus={searchRef}
          data-testid="clip-more-menu"
          onClose={() => setMoreMenu(null)}
        />
      ) : null}
      {banner === 'paused' ? (
        <div className="clip-banner">
          <Banner
            tone="paused"
            action={
              <Button size="sm" variant="secondary" onClick={() => actions.setPaused(false)}>
                {t('clipboard.paused.resume')}
              </Button>
            }
            data-testid="clip-paused"
          >
            {t('clipboard.paused.title')}
          </Banner>
        </div>
      ) : banner === 'disabled' ? (
        <div className="clip-banner">
          <Banner
            tone="paused"
            action={
              <Button size="sm" variant="secondary" onClick={actions.openSettings}>
                {t('clipboard.recording.open_settings')}
              </Button>
            }
            data-testid="clip-disabled"
          >
            {t('clipboard.recording.disabled')}
          </Banner>
        </div>
      ) : banner === 'watch' && watch?.state === 'unavailable' ? (
        <div className="clip-banner">
          {/* 横幅只有一行：后果写在正文里，平台层的具体原因放进提示 */}
          <Banner tone="warning" data-testid="clip-watch-unavailable">
            <span title={tKey(watch.reason_key) ?? undefined}>
              {t('clipboard.watch_unavailable')}
            </span>
          </Banner>
        </div>
      ) : banner === 'history' && unavailable ? (
        <div className="clip-banner">
          {/* 横幅只有一行：标题；完整原因在没有条目时的列表区里 */}
          <Banner tone="warning" data-testid="clip-unavailable">
            <span title={tKey(unavailable, undefined, t('clipboard.history_unavailable'))}>
              {t('clipboard.recording.unavailable_title')}
            </span>
          </Banner>
        </div>
      ) : null}
      <div className="clip-body" style={{ height: listHeight }}>
        {empty && recording === 'history' && unavailable ? (
          <div className="clip-empty">
            <EmptyState
              compact
              icon={DatabaseZap}
              title={t('clipboard.recording.unavailable_title')}
              detail={tKey(unavailable, undefined, t('clipboard.history_unavailable'))}
              data-testid="clip-unavailable"
            />
          </div>
        ) : empty ? (
          <EmptyBody
            t={t}
            os={os}
            query={query}
            searching={searchingMode}
            filter={filter}
            onShowAll={() => actions.setFilter('all')}
          />
        ) : (
          <MemoClipList
            store={store}
            actions={actions}
            rows={rows}
            offsets={offsets}
            height={listHeight}
            os={os}
            pasteOnDoubleClick={payload.paste_on_double_click}
            flat={flat}
            covered={showPreview}
            searchRef={searchRef}
            ensureHeight={ensureHeight}
            listboxId={listboxId}
            showKey={shown ? sessionId : null}
          />
        )}
        <AnimatePresence>
          {showPreview && selected && FullPreview ? (
            <FullPreview
              key="preview"
              hit={{ item: selected, highlights: [] }}
              query={searchingMode ? query : ''}
              height={listHeight}
              now={now}
              os={os}
              actions={actions}
              onClose={closePreview}
              scrollRef={previewScroll}
            />
          ) : null}
        </AnimatePresence>
      </div>
      {hintBar ? <HintBar os={os} preview={showPreview} dim={empty} drag={drag} /> : null}
      <div className="clip-footer-layer">
        <ConfirmDock t={t} os={os} store={store} actions={actions} />
        <StatusPill status={status} onHold={actions.holdStatus} />
      </div>
    </section>
  )
}

const MemoClipList = memo(ClipList)

/**
 * 过滤 Chip 的文字：所有类型名叠在同一格里（只显示当前的），宽度恒为最长的那个——Tab 循环切换时头部布局不动
 */
function FilterLabel({ filter, t }: { filter: ClipFilter; t: TFunction }) {
  return (
    <span className="clip-filter-label">
      {FILTERS.map((f) => (
        <span key={f} data-on={f === filter || undefined} aria-hidden={f !== filter || undefined}>
          {t(`clipboard.filter.${f}`)}
        </span>
      ))}
    </span>
  )
}

function EmptyBody({
  t,
  os,
  query,
  searching,
  filter,
  onShowAll,
}: {
  t: TFunction
  os: OsKind
  query: string
  searching: boolean
  filter: ClipFilter
  onShowAll(): void
}) {
  // 句中的类型名用小写的名词（英文「No images yet」，不是 Chip 上的「Images」）
  const kind = t(`clipboard.noun.${filter}`)
  const q = query.trim()
  // 两种「这个类型下什么都没有」的空状态共用的出口：回到全部类型
  const showAll = (
    <Button size="sm" variant="secondary" onClick={onShowAll} data-testid="clip-show-all">
      {t('clipboard.empty.show_all')}
    </Button>
  )
  let body
  if (searching) {
    // 少于 3 个字的查询只在最近 2000 条里做子串匹配：没搜到多半是更早的记录没有被搜，提示多输入几个字
    const short = [...q].length < SHORT_QUERY_CHARS
    body = (
      <EmptyState
        compact
        icon={SearchX}
        title={t('clipboard.no_result.title', { query: q })}
        detail={
          filter !== 'all'
            ? t('clipboard.no_result.filtered', { kind })
            : short
              ? t('clipboard.no_result.detail_short', {
                  min: SHORT_QUERY_CHARS,
                  limit: formatNumber(SHORT_QUERY_SCOPE),
                })
              : t('clipboard.no_result.detail')
        }
        action={filter === 'all' ? null : showAll}
        data-testid="clip-no-result"
      />
    )
  } else if (filter !== 'all') {
    // 与「过滤后无结果」同一个出口（鼠标用户也有路可走）；键盘路径（⇥ 切到下一个类型）留在说明里——⇥ 不是「全部类型」，
    // 不放进按钮
    body = (
      <EmptyState
        compact
        icon={Inbox}
        title={t('clipboard.empty.filtered', { kind })}
        detail={
          <Trans
            k="clipboard.empty.filtered_detail"
            nodes={{ key: <Kbd combo="Tab" variant="inline" /> }}
          />
        }
        action={showAll}
        data-testid="clip-empty-filtered"
      />
    )
  } else {
    body = (
      <EmptyState
        compact
        icon={Inbox}
        title={t('clipboard.empty.title')}
        detail={
          <Trans
            k="clipboard.empty.detail"
            nodes={{
              key: <Kbd combo={os === 'macos' ? 'Mod+KeyC' : 'Control+KeyC'} variant="inline" />,
            }}
          />
        }
        data-testid="clip-empty"
      />
    )
  }
  // 在列表区里垂直居中（design.clipboard-empty）
  return <div className="clip-empty">{body}</div>
}

/**
 * 行内二次确认（删除置顶项 / 清空历史）：浮在卡片底部（离三边 8 px），盖住提示条，从下往上展开
 */
function ConfirmDock({
  t,
  os,
  store,
  actions,
}: {
  t: TFunction
  os: OsKind
  store: ClipStore
  actions: ClipActions
}) {
  const confirm = useStore(store, (s) => s.confirm)
  const del = confirm?.kind === 'delete' ? confirm : null
  const clear = confirm?.kind === 'clear' ? confirm : null
  return (
    <div className="clip-dock">
      <InlineConfirm
        open={del !== null}
        message={
          <Trans
            k="clipboard.confirm.delete_pinned"
            nodes={{ key: <Kbd combo={deleteCombo(os)} variant="cap" /> }}
          />
        }
        confirmLabel={t('clipboard.confirm.delete')}
        onConfirm={() => {
          if (del) actions.remove(true, del.id)
        }}
        onCancel={actions.cancelConfirm}
        data-testid="clip-confirm-delete"
      />
      <InlineConfirm
        open={clear !== null}
        timeoutMs={null}
        message={
          <span className="clip-clear-message">
            <span>
              {clear?.synced ? t('clipboard.confirm.clear_synced') : t('clipboard.confirm.clear')}
            </span>
            <span className="clip-keep">
              <Checkbox
                checked={clear?.keepPinned ?? true}
                onChange={actions.setKeepPinned}
                label={t('clipboard.confirm.keep_pinned')}
                data-testid="clip-keep-pinned"
              />
              <Kbd combo="Space" variant="cap" />
            </span>
          </span>
        }
        confirmLabel={t('clipboard.confirm.clear_action')}
        onConfirm={actions.clear}
        onCancel={actions.cancelConfirm}
        data-testid="clip-confirm-clear"
      />
    </div>
  )
}

const STATUS_ICONS = { ok: CircleCheck, error: CircleAlert, info: ShieldCheck } as const

/** 底部的轻提示（已保存 / 操作失败 / 跳过了疑似密钥）：presets.toast 进出，停留 1.5 s */
function StatusPill({
  status,
  onHold,
}: {
  status: Status | null
  onHold: (hold: boolean) => void
}) {
  const motion = usePreset('toast')
  return (
    <div className="clip-status-dock" aria-live="polite">
      <AnimatePresence>
        {status ? (
          <m.span
            key={status.seq}
            className="clip-status"
            data-tone={status.tone}
            data-testid="clip-status"
            onPointerEnter={() => onHold(true)}
            onPointerLeave={() => onHold(false)}
            {...motion}
          >
            <Icon icon={STATUS_ICONS[status.tone]} size={14} className="clip-status-icon" />
            {status.text}
          </m.span>
        ) : null}
      </AnimatePresence>
    </div>
  )
}

/**
 * 底部快捷键提示条：显示顺序是 粘贴 · 纯文本 · 仅复制 · 置顶 · 删除 ·
 * Space 预览 · ⌘K 操作 · ⌘F 筛选 · Esc 关闭；放不下时按优先级整项隐藏（`hints.ts`：必留 粘贴 / 纯文本 / 筛选 / 关闭，
 * 其余按 仅复制 → 删除 → 置顶 → 预览 → 操作 让位），不加宽面板、不出现半截提示。预览打开时换成预览里的键；没有条目
 * 时整条变淡。只随预览开关 / 空列表变化（memo）。
 *
 * 取舍在 ResizeObserver 回调里直接改 DOM（`data-off`）：回调在布局之后、绘制之前执行，宽度变化与换项在同一帧里
 * 生效，没有「先按末尾裁掉、下一帧再换」的闪动；挂载时也不在布局阶段读布局（视图可能还在暂存层里）
 */
const HintBar = memo(function HintBar({
  os,
  preview,
  dim,
  drag,
}: {
  os: OsKind
  preview: boolean
  dim: boolean
  /** 从提示条的空白处拖动面板 */
  drag: PanelDragHandlers | undefined
}) {
  const t = useT()
  const ref = useRef<HTMLElement>(null)
  const items: { id: string; combos: string[]; label: string; priority: number }[] = preview
    ? [
        { id: 'back', combos: ['Space'], label: t('clipboard.hint.back'), priority: 0 },
        { id: 'paste', combos: ['Enter'], label: t('clipboard.hint.paste'), priority: 1 },
        {
          id: 'browse',
          combos: ['ArrowUp', 'ArrowDown'],
          label: t('clipboard.hint.browse'),
          priority: 2,
        },
      ]
    : [
        { id: 'paste', combos: ['Enter'], label: t('clipboard.hint.paste'), priority: 0 },
        { id: 'plain', combos: ['Shift+Enter'], label: t('clipboard.hint.plain'), priority: 1 },
        { id: 'copy', combos: ['Mod+Enter'], label: t('clipboard.hint.copy'), priority: 4 },
        // 文案固定为「置顶」（⌘P 是开关）：不随选中项变化，移动选中项时提示条不跳动
        { id: 'pin', combos: ['Mod+KeyP'], label: t('clipboard.hint.pin'), priority: 6 },
        {
          id: 'delete',
          combos: [deleteCombo(os)],
          label: t('clipboard.hint.delete'),
          priority: 5,
        },
        { id: 'preview', combos: ['Space'], label: t('clipboard.hint.preview'), priority: 7 },
        { id: 'actions', combos: ['Mod+KeyK'], label: t('clipboard.hint.actions'), priority: 8 },
        { id: 'filter', combos: ['Mod+KeyF'], label: t('clipboard.hint.filter'), priority: 2 },
        { id: 'close', combos: ['Escape'], label: t('clipboard.hint.close'), priority: 3 },
      ]
  const signature = items.map((it) => `${it.id}:${it.label}`).join('|')

  useLayoutEffect(() => {
    const el = ref.current
    if (!el || typeof ResizeObserver === 'undefined') return
    const fit = () => {
      const children = [...el.querySelectorAll<HTMLElement>(':scope > .clip-hint')]
      // 先全部放回来量自然宽度（宽度只取决于文案与字体；只有这个小提示条重新排版）
      for (const c of children) delete c.dataset.off
      const style = getComputedStyle(el)
      const available =
        el.clientWidth -
        Number.parseFloat(style.paddingLeft) -
        Number.parseFloat(style.paddingRight)
      const gap = Number.parseFloat(style.columnGap) || 0
      const sizes = children.map((c) => ({
        id: c.dataset.hint ?? '',
        priority: Number(c.dataset.priority),
        width: c.getBoundingClientRect().width,
      }))
      const hidden = hiddenHints(sizes, available, gap)
      for (const c of children) {
        if (hidden.has(c.dataset.hint ?? '')) c.dataset.off = ''
      }
    }
    // 首次回调在挂载后的第一次布局之后到达（暂存层里同样排版，不强制同步布局）；之后宽度变化时再取舍
    const ro = new ResizeObserver(fit)
    ro.observe(el)
    return () => ro.disconnect()
    // 换项（预览开关、界面语言）时重新观察，回调随之再取舍一次（signature 只用来触发重新观察）
    // oxlint-disable-next-line react/exhaustive-deps
  }, [signature])

  return (
    <footer
      ref={ref}
      className="clip-hints"
      data-testid="clip-hints"
      data-dim={dim || undefined}
      onPointerDown={drag?.onPointerDown}
    >
      {items.map((it) => (
        <span key={it.id} className="clip-hint" data-hint={it.id} data-priority={it.priority}>
          {it.combos.map((c) => (
            <Kbd key={c} combo={c} variant="cap" />
          ))}
          {it.label}
        </span>
      ))}
    </footer>
  )
})

// -----------------------------------------------------------------------------
// JIT 预热（shared/perf/prewarm.tsx）：启动后 / 回收重建后第一次打开剪贴板（1000 条历史）首帧实测 66–101 ms，贴着
// 唤出的 100 ms 预算。空闲时在不可见的暂存节点里渲染一屏各类条目（文本、链接、代码、颜色、富文本）再卸载：
// 不订阅推送、不取详情 / 缩略图（没有图片 / 文件条目与来源图标）、没有下一页；真实的视图挂载过就不再需要

let clipboardMounted = false
let clipboardPrewarmed = false

function prewarmItem(id: number, kind: ClipKind, preview: string): ClipHit {
  return {
    item: {
      id,
      kind,
      created_at: 0,
      last_used_at: 0,
      use_count: 1,
      pinned: id === 1,
      source: null,
      preview,
      char_count: preview.length,
      line_count: 1,
      truncated: false,
      image: null,
      files: null,
      link: kind === 'link' ? preview : null,
      color: kind === 'color' ? preview : null,
    },
    highlights: [],
  }
}

/** 预热一次（见上）；真实视图已挂载过、或已经预热过时什么都不做 */
export function prewarmClipboardOnce(): void {
  if (clipboardPrewarmed || clipboardMounted) return
  clipboardPrewarmed = true
  prewarmClipboardRender()
}

/** 预热渲染本身（同步渲染、排版、卸载，不留节点） */
export function prewarmClipboardRender(): void {
  const items: ClipHit[] = [
    prewarmItem(1, 'text', 'Wisp 剪贴板 · Clipboard'),
    prewarmItem(2, 'link', 'https://example.com/a?b=1'),
    prewarmItem(3, 'code', 'const x = 1'),
    prewarmItem(4, 'color', '#3B82F6'),
    prewarmItem(5, 'rich_text', '富文本 Rich text'),
    prewarmItem(6, 'text', '0123456789「中文」，标点。'),
  ]
  const props: ViewProps<'clipboard'> = {
    sessionId: 0,
    payload: {
      kind: 'clipboard',
      page: { items, pinned_count: 1, next_offset: null, query: null, filter: 'all' },
      paused: false,
      hint_bar: true,
      paste_on_double_click: false,
    },
    phase: 'preparing',
    focused: false,
    host: 'panel',
    frame: boot.frame_mode,
    placement: 'center',
    maxHeight: 560,
    requestHide: () => undefined,
    switchView: () => undefined,
    activate: () => undefined,
  }
  prewarmRender(<ClipboardView {...props} />, { width: 520, height: 560 })
}

schedulePrewarm(prewarmClipboardOnce, PREWARM_ORDER.clipboard)
