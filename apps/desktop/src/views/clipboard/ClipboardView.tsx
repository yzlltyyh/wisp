/**
 * 剪贴板视图：版式与 design.clipboard-* 视觉稿一致——
 * 头部（大号无边框搜索 · 过滤「全部 ▾」· ⋯）/ 暂停横幅 / 列表（「收藏 / 最近」两组，收藏可折叠；检索结果与收藏视图
 * 平铺；唯一的滑动选中指示器）/ 底部快捷键提示条（列表 / 预览 / 多选 / 芯片条各一组）。底部浮层里依次是芯片条（光标
 * 所在条目识别出的片段）、确认条 / 命名条与轻提示。
 *
 * 用户能触发的操作（快捷键、菜单、提示条、预览里的按钮）都在操作注册表里声明（actions/registry.ts），这里只把注册表
 * 派生出的键位层、菜单与提示条接到界面上；面板与悬浮窗的差异由宿主适配（host.ts）给出。
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
 *   清空确认是模态的：只接受 ↩ 确认、Esc 取消、Space 切换「保留收藏」，其余按键与指针都不落到列表上；确认条上沿
 *   的渐隐遮罩随确认条一起淡入淡出；
 * - 全文预览、芯片条与命名条的代码（`./preview`）在视图显示后空闲时预取并缓存（previewModule.ts），打开 / 关闭都是真正的
 *   交叉淡入；芯片条在检视结果到达后才出现，模块晚到也只是晚一点出现；
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
import type { PanelDragHandlers } from '../../shared/hooks/usePanelDrag'
import { usePlatform } from '../../shared/hooks/usePlatform'
import { useViewActive } from '../../shared/hooks/useViewActive'
import { Trans } from '../../shared/i18n/Trans'
import { formatNumber } from '../../shared/i18n/format'
import { useT, useTKey, type TFunction } from '../../shared/i18n/useT'
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
import {
  useActionKeys,
  useClipRunner,
  type ClipRunner,
  type ClipViewUi,
  type HintMode,
} from './actions'
import { ClipList } from './ClipList'
import { filterMenuItems } from './filters'
import { keepSearchFocus, useRefocusWhenShown, useTypeToSearch } from './focus'
import { nextClockDelay, warmRowTimeFormats } from './format'
import { loadClipboardMessages } from './i18n'
import { rowDomId } from './ClipRow'
import { hiddenHints } from './hints'
import { useClipHost, useHostHeight, type GrowFollow } from './host'
import {
  BANNER_HEIGHT,
  EMPTY_HEIGHT,
  FILTERS,
  HEADER_HEIGHT,
  HINT_HEIGHT,
  listContentHeight,
  rowOffsets,
  screenRows,
  SHORT_QUERY_CHARS,
  SHORT_QUERY_SCOPE,
} from './model'
import { preloadPreview, usePreviewModule } from './previewModule'
import {
  createClipStore,
  isSearching,
  rowsOf,
  selectedItem,
  visibleHits,
  type ClipState,
  type ClipStore,
  type Status,
} from './store'
import { useClipboard } from './useClipboard'

loadClipboardMessages()

// 面板窗口常驻：启动后空闲时预热行尾时间的格式器（不在第一次唤出的同步渲染里构造）
if (typeof window !== 'undefined') {
  const warm = () => warmRowTimeFormats()
  if (typeof window.requestIdleCallback === 'function') window.requestIdleCallback(warm)
  else setTimeout(warm, 0)
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
 * 当前的底边处，列表区在它们上沿裁切，卡片长完时回到原位（方式由宿主给出，见 host.ts）：
 * - follow（Css 帧模式）：每帧在 Motion 写完卡片高度之后读卡片的实际底边（只在长高的这一小段时间里）；
 * - spring（Native 帧模式）：卡片的可见范围由原生窗口裁切，DOM 里量不到，用与 Rust 窗口同一条 spring.gentle；
 * - none：不能透明的窗口（没有高度动画，直接到位）、减少动效、不随内容长高的宿主。
 * 偏移在布局阶段同步写入：长高的那一帧底部不会先跳到新位置再弹回
 */
function useGrowFollow(root: ElementRef<HTMLElement>, total: number, mode: GrowFollow): void {
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
  const { payload, sessionId, maxHeight, focused, phase } = props
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
  // 面板 / 悬浮窗的差异（高度、焦点、拖动、Esc、代写剪贴板后的提示）
  const host = useClipHost(props, reduced)
  // 阶段决定实时的 `reloaded` 什么时候处理（显示中暂存，见 useClipboard）
  const cmd = useClipboard(
    store,
    { os, announceCopied: host.announceCopied, copiedNotice: host.copiedNotice },
    phase,
  )
  // 悬浮剪贴板：接着显示面板交接时的查询、筛选与光标（布局阶段写入：交接上屏的第一帧就是它们）
  const seed = host.float?.seed
  useLayoutEffect(() => {
    if (!seed) return
    cmd.setFilter(seed.filter)
    cmd.setQuery(seed.query)
    if (seed.selectedId !== null) cmd.select(seed.selectedId, true, true)
    // 只在挂载时接一次
    // oxlint-disable-next-line react/exhaustive-deps
  }, [])

  // 订阅列表的各个来源（任一变化都重新取行）；行本身由 rowsOf 按 store 缓存
  useStore(store, (s) => s.all)
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
  const marked = useStore(store, (s) => s.marks.length)
  const marking = marked > 0
  const chipFocus = useStore(store, (s) => s.chip)
  const renaming = useStore(store, (s) => s.rename !== null)
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

  // ----- 全文预览 / 芯片条 / 命名条的代码：显示后空闲时预取；预取之前就按了 Space、打开了命名条时立即加载，到达之前
  // 列表不淡出
  const previewMod = usePreviewModule(preview || renaming)
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

  // ----- 高度：面板 clamp(内容, 下限, min(560, 上限))、会话内只增不减；悬浮窗占满窗口（见 host.ts）
  const hintBar = payload.hint_bar
  const chrome = HEADER_HEIGHT + (banner ? BANNER_HEIGHT : 0) + (hintBar ? HINT_HEIGHT : 0)
  const natural = chrome + (rows.length > 0 ? listContentHeight(rows) : EMPTY_HEIGHT)
  const { total, ensureHeight } = useHostHeight(host, natural, maxHeight)
  const listHeight = Math.max(0, total - chrome)

  // ----- 显示期间在下一次标签变化时刷新相对时间（一次性定时器，醒来后再按当时的行排下一次）；隐藏 / 暂存 / 页面
  // 不可见（悬浮窗被最小化、遮住）时不跑。列表的内容变了（新条目到达、更新、翻页、检索结果）时立刻按当前时间刷新
  // 并重排：行都较旧时定时器可能排在一小时之后，新验证码的「N 分钟后删除」会按陈旧的时间基准多算出最多 60 分钟，
  // 相对时间也停在旧值（悬浮窗常开时尤其明显）
  useEffect(() => {
    if (!active || hidden) return
    let id: ReturnType<typeof setTimeout> | undefined
    const arm = () => {
      clearTimeout(id)
      if (typeof document !== 'undefined' && document.hidden) return
      const items = visibleHits(store.getState()).map((h) => h.item)
      id = setTimeout(
        () => {
          store.setState({ now: Date.now() })
          arm()
        },
        nextClockDelay(items, Date.now()),
      )
    }
    const onVisible = () => {
      if (!document.hidden) store.setState({ now: Date.now() })
      arm()
    }
    // 比较列表的来源而不是 visibleHits 的结果（本地筛选每次都返回新数组）；刷新 now 不改这些来源，不会循环
    const sources = (s: ClipState) => [s.all, s.filtered, s.search] as const
    let seen = sources(store.getState())
    const unsubscribe = store.subscribe((s) => {
      const next = sources(s)
      if (next.every((v, i) => v === seen[i])) return
      seen = next
      store.setState({ now: Date.now() })
      arm()
    })
    arm()
    document.addEventListener('visibilitychange', onVisible)
    return () => {
      clearTimeout(id)
      unsubscribe()
      document.removeEventListener('visibilitychange', onVisible)
    }
  }, [store, active, hidden])

  // ----- 唤出即可打字：面板进入 entering / shown（窗口这时才真正可见）、窗口重新获得焦点、页面重新可见时，
  // 焦点丢了就补聚焦搜索框——挂载时窗口还不可见，autoFocus 的 focus() 不生效，而 focused / active / hidden 在显示时
  // 都不会变（见 focus.ts）。点了非输入区域时焦点不离开搜索框；焦点还是丢了时打字照样落进搜索框
  const root = useRef<HTMLElement>(null)
  const searchRef = useRef<HTMLInputElement>(null)
  const shown = phase === 'entering' || phase === 'shown'
  useRefocusWhenShown(root, searchRef, host.focusOnShow && active && !hidden && focused && shown)
  useTypeToSearch(root, searchRef, active && !hidden)
  // 长高时底部的提示条 / 确认条 / 轻提示跟着卡片当前的底边走（见 useGrowFollow）
  useGrowFollow(root, total, host.growFollow)

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

  // ----- 预览
  const previewScroll = useRef<HTMLDivElement>(null)
  const closePreview = () => cmd.setPreview(false)
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

  // 命名条关闭（保存 / 取消）之后焦点回到搜索框（输入框随命名条卸载，焦点会落到 body）
  const wasRenaming = useRef(renaming)
  useEffect(() => {
    const closed = wasRenaming.current && !renaming
    wasRenaming.current = renaming
    if (closed) searchRef.current?.focus({ preventScroll: true })
  }, [renaming])

  // ----- 操作注册表：视图提供界面能力（菜单、预览滚动与焦点、当前的行），runner 据此执行操作、派生菜单与键位层
  const listHeightRef = useRef(listHeight)
  useLayoutEffect(() => {
    listHeightRef.current = listHeight
  })
  const ui = useMemo<ClipViewUi>(
    () => ({
      rows: () => rowsOf(store),
      pageRows: () => screenRows(listHeightRef.current),
      openFilterMenu: () => setFilterMenu('keyboard'),
      openMoreMenu: () => setMoreMenu('keyboard'),
      openItemMenu: (page) =>
        store.setState((s) => ({ actionMenu: s.actionMenu + 1, actionPage: page ?? null })),
      scrollPreview: (step) => scrollBy(previewScroll, step),
      cyclePreviewFocus: (step) => cycleFocusIn(root, searchRef, step),
    }),
    [store],
  )
  const runner = useClipRunner(store, os, { cmd, ui, host })
  // 头部的图标按钮（注册表的 toolbar 界面）：面板是交接到悬浮窗的钉子，悬浮窗是置顶、关闭；连续粘贴进行中多一个「结束」
  useStore(store, (s) => s.queue?.active === true)
  const toolbar = runner.entries('toolbar', runner.ctx('toolbar', null), t)
  // ⋯ 菜单里有「清空历史…」：打开菜单时让菜单里的操作做准备（先读清空会不会同步到其他设备；打开面板时不读设置）
  const moreOpen = moreMenu !== null
  useEffect(() => {
    if (moreOpen) runner.prepare('more', runner.ctx('more'))
  }, [moreOpen, runner])

  // ----- 键盘：注册表的四个键位层。视图层一直在；全文预览、清空确认（模态）、删除收藏的行内确认各是一层覆盖层，
  // 同时打开时后打开的优先（再按一次 ⌫ 只作用于发起确认的那一条、Esc 先取消确认）
  const clearOpen = confirm?.kind === 'clear'
  const deleteId = confirm?.kind === 'delete' ? confirm.id : null
  useActionKeys(runner, 'list', true)
  useActionKeys(runner, 'preview', showPreview)
  useActionKeys(runner, 'clear', clearOpen)
  useActionKeys(runner, 'confirm', deleteId !== null)
  useActionKeys(runner, 'chips', chipFocus !== null)
  useActionKeys(runner, 'rename', renaming)
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
  const confirmOpen = confirm !== null || renaming
  const statusDocked = status !== null && !hintBar
  // 芯片条浮在列表底部：与确认条同一个渐隐遮罩，被盖住的那一行不会被拦腰截断
  const chipsShown =
    useStore(
      store,
      (s) =>
        s.inspect !== null && s.inspect.id === s.selectedId && s.inspect.result.entities.length > 0,
    ) &&
    !confirmOpen &&
    !showPreview &&
    !empty
  const masked = confirmOpen || statusDocked || chipsShown
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

  // ----- 拖动（宿主给出：面板经 shared/hooks/usePanelDrag，「记住位置」由 Rust 在隐藏时按显示器记下位置）。只从头部 /
  // 提示条的空白处开始（列表、预览与状态条另有按下的含义）；mousedown 的默认行为由根节点的 keepSearchFocus 阻止
  const drag = host.drag

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
      // 清空确认与命名条是模态的：列表不接收指针（命名时点到一行不会把它粘贴出去）
      data-modal={clearOpen || renaming || undefined}
      data-confirm={confirm ? confirm.kind : renaming ? 'rename' : undefined}
      data-dock-mask={dock !== 'off' || masked || undefined}
      data-status-docked={statusDocked || undefined}
      data-chips={(chipsShown && previewMod !== null) || undefined}
      data-status-bar={(status !== null && hintBar) || undefined}
      // 点在非输入区域（列表空白、行、按钮、横幅、确认条、空状态）时焦点不离开搜索框（见 focus.ts）
      onMouseDown={keepSearchFocus}
    >
      <header
        className="clip-head"
        data-spin={spin === 'on' || undefined}
        data-tauri-drag-region={host.dragRegion || undefined}
        onPointerDown={drag?.onPointerDown}
      >
        <SearchField
          ref={searchRef}
          variant="bare"
          value={query}
          onChange={cmd.setQuery}
          onClear={() => {
            cmd.setQuery('')
            searchRef.current?.focus({ preventScroll: true })
          }}
          placeholder={t('clipboard.search.placeholder')}
          aria-label={t('clipboard.a11y.search')}
          // 悬浮窗平时不抢焦点：点进搜索框（或它拿到焦点）时才临时可聚焦，面板里什么都不做。焦点离开窗口（切到别的
          // 应用、Esc 交还）时恢复不可聚焦——否则之后每次点条目都会抢走目标应用的前台；焦点只是移到窗口里别的输入框
          // （改名条、⌘K）时保持可聚焦，用户还在打字
          onPointerDown={() => host.searchFocus(true)}
          onFocus={() => host.searchFocus(true)}
          onBlur={(e) => {
            if (e.relatedTarget === null) host.searchFocus(false)
          }}
          // 组合框（ARIA 1.2 的 combobox 模式，元素本身就是 input）：列表一直展开（读屏按 activedescendant 播报
          // 选中项），aria-controls 指向 listbox 本身
          // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role
          role="combobox"
          aria-expanded={!empty}
          aria-controls={empty ? undefined : listboxId}
          aria-activedescendant={
            chipFocus !== null && previewMod
              ? previewMod.chipDomId(chipFocus)
              : selectedId !== null && !empty
                ? rowDomId(selectedId)
                : undefined
          }
          aria-autocomplete="list"
          // 唤出即可打字（宿主不抢焦点时除外）；视图在切换暂存层里时 useAutoFocus 推迟到显示时
          // oxlint-disable-next-line jsx-a11y/no-autofocus
          autoFocus={host.focusOnShow}
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
        {toolbar.map((e) => (
          <IconButton
            key={e.id}
            icon={e.icon ?? Ellipsis}
            label={e.label}
            shortcut={e.shortcut}
            active={e.checked}
            disabled={e.disabled}
            onClick={e.run}
            data-testid={`clip-tool-${e.id}`}
          />
        ))}
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
          items={filterMenuItems(filter, t, cmd.setFilter)}
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
          items={runner.menu('more', runner.ctx('more'), t)}
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
              <Button
                size="sm"
                variant="secondary"
                onClick={() => runner.run('pause', runner.ctx('button'))}
              >
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
              <Button
                size="sm"
                variant="secondary"
                onClick={() => runner.run('settings', runner.ctx('button'))}
              >
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
            cycleKey={runner.shortcut('cycle_filter')}
            favoriteKey={runner.shortcut('favorite')}
            onShowAll={() => cmd.setFilter('all')}
          />
        ) : (
          <MemoClipList
            store={store}
            cmd={cmd}
            runner={runner}
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
            refocusSearch={host.focusOnShow}
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
              cmd={cmd}
              runner={runner}
              store={store}
              onClose={closePreview}
              scrollRef={previewScroll}
            />
          ) : null}
        </AnimatePresence>
      </div>
      {hintBar ? (
        <HintBar
          runner={runner}
          mode={showPreview ? 'preview' : chipFocus !== null ? 'chips' : marking ? 'marks' : 'list'}
          marked={marked}
          dim={empty}
          drag={drag}
        />
      ) : null}
      <div className="clip-footer-layer">
        {previewMod ? (
          <previewMod.ChipStrip store={store} runner={runner} hidden={!chipsShown} />
        ) : null}
        <div className="clip-dock">
          <ConfirmDock t={t} store={store} runner={runner} />
          {previewMod ? <previewMod.RenameDock store={store} cmd={cmd} runner={runner} /> : null}
        </div>
        <StatusPill status={status} onHold={cmd.holdStatus} />
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
  cycleKey,
  favoriteKey,
  onShowAll,
}: {
  t: TFunction
  os: OsKind
  query: string
  searching: boolean
  filter: ClipFilter
  /** 循环切换类型过滤的快捷键（⇥） */
  cycleKey: string | null
  /** 收藏的快捷键（⌘D）：收藏视图为空时告诉用户怎么收藏 */
  favoriteKey: string | null
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
          filter === 'favorite' ? (
            <Trans
              k="clipboard.empty.favorites_detail"
              nodes={{ key: favoriteKey ? <Kbd combo={favoriteKey} variant="inline" /> : null }}
            />
          ) : (
            <Trans
              k="clipboard.empty.filtered_detail"
              nodes={{ key: cycleKey ? <Kbd combo={cycleKey} variant="inline" /> : null }}
            />
          )
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
 * 行内二次确认（删除收藏 / 删除含收藏的多选 / 清空历史）：浮在卡片底部（离三边 8 px），盖住提示条，从下往上展开。按钮
 * 与说明里的按键都来自注册表（确认 = 删除 / 清空，取消 = 取消确认，复选框 = 保留收藏）
 */
function ConfirmDock({ t, store, runner }: { t: TFunction; store: ClipStore; runner: ClipRunner }) {
  const confirm = useStore(store, (s) => s.confirm)
  const del = confirm?.kind === 'delete' ? confirm : null
  const clear = confirm?.kind === 'clear' ? confirm : null
  const cancel = () => runner.run('cancel_confirm', runner.ctx('button'))
  // 「再按一次 ⌫ 删除」：说明的是删除键本身（不按条目区分的那个）
  const deleteKey = runner.shortcut('delete')
  const keepKey = runner.shortcut('keep_pinned', 'clear')
  const keyNode = { key: deleteKey ? <Kbd combo={deleteKey} variant="cap" /> : null }
  return (
    <>
      <InlineConfirm
        open={del !== null}
        message={
          del?.ids ? (
            <Trans
              k="clipboard.confirm.delete_many"
              params={{ n: del.ids.length }}
              nodes={keyNode}
            />
          ) : (
            <Trans k="clipboard.confirm.delete_pinned" nodes={keyNode} />
          )
        }
        confirmLabel={t('clipboard.confirm.delete')}
        onConfirm={() => {
          // 多选的确认作用于整个选择（按钮的上下文带着多选），否则作用于发起确认的那一条
          if (del) runner.run('delete', runner.ctx('button', del.id), true)
        }}
        onCancel={cancel}
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
                onChange={(keep) => runner.run('keep_pinned', runner.ctx('button'), keep)}
                label={t('clipboard.confirm.keep_pinned')}
                data-testid="clip-keep-pinned"
              />
              {keepKey ? <Kbd combo={keepKey} variant="cap" /> : null}
            </span>
          </span>
        }
        confirmLabel={t('clipboard.confirm.clear_action')}
        onConfirm={() => runner.run('clear_confirm', runner.ctx('button'))}
        onCancel={cancel}
        data-testid="clip-confirm-clear"
      />
    </>
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
 * 底部快捷键提示条：各项、显示顺序与优先级都来自注册表（操作的 `hints`）——列表里是 粘贴 · 纯文本 · 仅复制 · 收藏 ·
 * 删除 · Space 预览 · ⌘K 操作 · ⌘F 筛选 · Esc 关闭；放不下时按优先级整项隐藏（`hints.ts`：必留 粘贴 / 纯文本 / 筛选 /
 * 关闭，其余按 仅复制 → 删除 → 收藏 → 预览 → 操作 让位），不加宽面板、不出现半截提示。预览打开时换成预览里的一组；
 * 没有条目时整条变淡。只随模式 / 空列表变化（memo）。
 *
 * 取舍在 ResizeObserver 回调里直接改 DOM（`data-off`）：回调在布局之后、绘制之前执行，宽度变化与换项在同一帧里
 * 生效，没有「先按末尾裁掉、下一帧再换」的闪动；挂载时也不在布局阶段读布局（视图可能还在暂存层里）
 */
const HintBar = memo(function HintBar({
  runner,
  mode,
  dim,
  drag,
}: {
  runner: ClipRunner
  mode: HintMode
  /** 选了几条：提示随它变（选了两条以上时 ⌘↩ 是连续粘贴），只用来让提示条重新取一次 */
  marked: number
  dim: boolean
  /** 从提示条的空白处拖动面板 */
  drag: PanelDragHandlers | undefined
}) {
  const t = useT()
  const ref = useRef<HTMLElement>(null)
  const items = runner.hints(mode, t)
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
