/**
 * 翻译视图。
 *
 * 结构与视觉稿 `design.translate-*` 一致：
 * - 头部：语言条「EN → 中文 ▾」（自动方向、交换、手动改目标，只对本会话生效）· 风格 chip（菜单含 ⌘1–9）· ⋯（润色 /
 *   校对 / 提升译文 / 复制原文 / 切换模型 / 编辑当前风格）· ✕；润色 / 校对时左侧换成任务标题，chip 换成「返回翻译」；
 * - 原文对照（⇄）；
 * - 正文：骨架 → 流式（plain / mdlite / edit_split，词典式用 mdlite 的词典排版）| 长文本确认 | 未就绪卡片 | 错误卡片 |
 *   思考耗尽的空状态；换新请求时旧正文留在下层与新正文交叉淡入，正文区保持旧正文的高度直到新内容超过它
 *   或流结束（结束时只按最终高度收一次）；卡片 ↔ 骨架 / 正文、骨架 → 第一段正文同样交叉淡入、保持旧高度；
 * - 提示行：截断、参数已调整这类需要读完整的提示（不和指标、按钮挤在一行）；
 * - 底栏：状态行（模型 · 耗时 · tokens，放不下时整段丢弃次要段；思考耗尽等提示）+ 流式中 [停止]、
 *   完成后操作（复制 · 固定 · ⇄ · ⤢）；轻提示（已复制…）临时替换状态行。
 *
 * 请求与流由 `TranslateRun` 管理（run.ts）；流式文字不经 React，直接由 StreamView 写 DOM。高度：视图按内容自然排版，
 * 整体不超过卡片上限（`maxHeight` − 内边距），超出部分在正文内滚动（StreamView 自动跟随到底部）；卡片 / 窗口
 * 追赶新高度期间底栏贴住可见下沿（reveal.ts，减少动效时同样）。Pinned 里视图铺满窗口，底栏贴底。
 *
 * 首次渲染的 JIT 冷启动：面板外壳把窗口标记为隐藏之后把视图在不可见的暂存节点里渲染一次再卸载
 * （`prewarmOnce`，不发 IPC、不登记键盘作用域），第一次唤出时视图代码已经热了。
 */
import {
  AlertTriangle,
  ArrowLeftRight,
  BrainCircuit,
  CircleAlert,
  CircleCheck,
  CircleStop,
  Cpu,
  History,
  Info,
  Languages,
  Maximize2,
  Pin,
  RotateCw,
  SlidersHorizontal,
  WandSparkles,
  X,
  Zap,
  type LucideIcon,
} from 'lucide-react'
import { AnimatePresence, m } from 'motion/react'
import { useAnimate } from 'motion/react-mini'
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
  type CSSProperties,
  type ReactNode,
  type RefObject,
} from 'react'
import { copyText } from '../../shared/hooks/useCopy'
import { usePanelDrag } from '../../shared/hooks/usePanelDrag'
import { useViewActive } from '../../shared/hooks/useViewActive'
import {
  PREWARM_ORDER,
  prewarmRender as prewarmRender_,
  schedulePrewarm,
  usePrewarm,
} from '../../shared/perf/prewarm'
import { formatDuration, formatNumber, formatTokens } from '../../shared/i18n/format'
import { useT, type TFunction } from '../../shared/i18n/useT'
import { api, boot, type SettingsSection } from '../../shared/ipc'
import type {
  Lang,
  ModelRef,
  PermissionKind,
  PinState,
  PinStatus,
  TaskKind,
} from '../../shared/ipc/types'
import type { KeyBinding } from '../../shared/keys/keyRouter'
import { useKeyScope } from '../../shared/keys/useKeyScope'
import { presets, usePreset } from '../../shared/motion/presets'
import { ease, ms, timing } from '../../shared/motion/tokens'
import { useReducedMotionPref } from '../../shared/motion/useReducedMotionPref'
import { offlineRef } from '../../shared/offline/catalog'
import { isOfflineNotReady, OfflineCard } from '../../shared/offline/OfflineCard'
import { StreamView, type StreamFormat, type StreamHandle } from '../../shared/stream/StreamView'
import { stripUrlBreaks } from '../../shared/stream/urlBreaks'
import {
  Button,
  Chip,
  CopyButton,
  cx,
  EmptyState,
  ErrorCard,
  Icon,
  IconButton,
  NotReadyCard,
  ScrollArea,
  Skeleton,
  StatusLine,
  Tooltip,
  type StatusLineProps,
} from '../../shared/ui'
import type { ViewProps } from '../../panel/types'
import { usePinnedChrome, usePinnedHost, usePinnedReadyHold } from '../../pinned/context'
import {
  isCompatibleEndpoint,
  modelChoices,
  providerOf,
  styleList,
  styleOf,
  translateSettings,
  useCatalog,
  type StyleInfo,
} from './catalog'
import { pendingKey, putHandoff, readHandoff, type Handoff } from './handoff'
import { isLocalRequest } from './hub'
import { LangSwitch, type SwapControl } from './LangSwitch'
import { LongTextPrompt } from './LongTextPrompt'
import { MoreMenu, StyleMenu, STYLE_SHORTCUTS } from './menus'
import { revealSource, useRevealEdge } from './reveal'
import { TranslateRun, type RunHost, type RunParams, type RunSnapshot } from './run'
import { measureScrollbarWhenIdle, useScrollbarWidth } from './scrollbar'
import { isDictionaryInput, planSwap, refineEligible, skeletonLines } from './text'
import { holdMarkdownWarmup, scheduleMarkdownWarmup } from './warmup'
import { loadTranslateMessages } from './i18n'
import './translate.css'

loadTranslateMessages()

/** 正文排版：润色 / 校对 → 正文 + 改动；词典式与 Markdown-lite 风格 → mdlite；其余原样 */
export function formatOf(task: TaskKind, style: StyleInfo, dict: boolean): StreamFormat {
  if (task === 'polish' || task === 'proofread') return 'edit_split'
  if (dict) return 'mdlite'
  return style.output === 'markdown_lite' ? 'mdlite' : 'plain'
}

/** 状态行各段之间的分隔（与 StatusLine 的「·」一致） */
const STATUS_SEPARATOR = ' · '

/** 命中缓存：耗时是缓存查找的耗时（Rust 回传 0 ms），不是这次翻译的耗时，状态行不显示，改为注明「来自缓存」 */
function fromCache(snap: RunSnapshot): boolean {
  return snap.cached || snap.notices.includes('cache_hit')
}

/** 底栏状态行的纯文本（状态行的无障碍名称；固定时作为 PinState.status_line 交给旧前端） */
export function statusLineText(snap: RunSnapshot, t: TFunction): string | null {
  if (snap.status === 'cancelled') return t('panel.status.cancelled')
  const cached = fromCache(snap)
  const parts: string[] = []
  if (snap.model) parts.push(snap.model.label)
  if (snap.elapsedMs !== null && !cached) parts.push(formatDuration(snap.elapsedMs))
  if (snap.usage) parts.push(formatTokens(snap.usage.output_tokens))
  if (cached && snap.status === 'done') parts.push(t('panel.status.cached'))
  return parts.length > 0 ? parts.join(STATUS_SEPARATOR) : null
}

/**
 * 固定时交给固定窗口的结构化状态行（PinState.status）：与底栏 StatusLine 的各段一一对应；
 * 还没有任何结果信息（没有模型名、也不是取消）时为 null
 */
export function pinStatusOf(snap: RunSnapshot): PinStatus | null {
  const cancelled = snap.status === 'cancelled'
  if (!snap.model && !cancelled) return null
  const cached = fromCache(snap)
  return {
    model_label: snap.model?.label ?? '',
    elapsed_ms: cached ? null : snap.elapsedMs,
    usage: snap.usage,
    cached,
    cancelled,
    truncated: snap.truncated ? 'max_tokens' : null,
  }
}

/**
 * 预设里的目标值与过渡（WAAPI 播放用）；减少动效时只保留透明度、≤ 100 ms。预设没有给曲线时补上令牌里的
 * 曲线：出场（变暗、淡出）用 ease.exit，入场（恢复、淡入）用 ease.standard（与其他动效同一手感）
 */
export function presetMotion(
  p: Readonly<Record<string, unknown>> & { transition?: object },
  reduced: boolean,
  direction: 'in' | 'out' = 'in',
): { keyframes: Record<string, unknown>; transition: object } {
  const { transition, ...keyframes } = p
  if (reduced)
    return { keyframes: { opacity: keyframes.opacity }, transition: presets.reduced.fade }
  const base = (transition ?? presets.reduced.fade) as { ease?: unknown }
  return {
    keyframes,
    transition: base.ease
      ? base
      : { ...base, ease: direction === 'out' ? ease.exit : ease.standard },
  }
}

/** 请求状态机的宿主回调：读的是本次渲染的值（视图每次提交后重新设置，见 TranslateView） */
function makeHost(
  sessionId: number,
  styles: readonly StyleInfo[],
  props: ViewProps<'translate'>,
  showSource: boolean,
  forcedSource: Lang | null,
): RunHost {
  return {
    sessionId,
    formatOf: (task, styleId, dict) => formatOf(task, styleOf(styles, styleId), dict),
    guessDict: (p) =>
      p.task === 'translate' && styleOf(styles, p.styleId).dictionary && isDictionaryInput(p.text),
    onWidth: (width, r) => {
      // 新请求要求改宽度（面板宿主）：经 switchView 在透明帧改宽，新实例接上同一个请求
      const s = r.getSnapshot()
      if (props.host !== 'panel' || !s.requestId || isLocalRequest(s.requestId)) return
      r.release()
      putHandoff(s.requestId, {
        target: s.params.target,
        model: s.params.model,
        showSource,
        forcedSource,
      })
      props.switchView(
        {
          kind: 'translate',
          source_text: s.params.text,
          request_id: s.requestId,
          task: s.params.task,
          style_id: s.params.styleId,
          detected_lang: s.detected,
          target_lang: s.target,
          dict_mode: s.dict ?? false,
          long_text: null,
          not_ready: null,
          selection_method: props.payload.selection_method,
        },
        { width },
      )
    },
  }
}

/** 为一个视图实例创建请求状态机：初始参数来自载荷（或 Pinned 的 PinState）与交接来的会话内选择 */
function createRun(
  payload: ViewProps<'translate'>['payload'],
  pin: PinState | null,
  handoff: Handoff | null,
  host: RunHost,
): TranslateRun {
  const params: RunParams = pin
    ? {
        text: pin.source_text,
        task: pin.task,
        styleId: pin.style_id,
        // 固定时保留当时的方向（方向随内容一起脱离）与实际使用的模型（重新生成沿用它）
        target: pin.target_lang,
        model: pin.model_ref ?? null,
      }
    : {
        text: payload.source_text,
        task: payload.task,
        styleId: payload.style_id,
        target: handoff?.target ?? null,
        model: handoff?.model ?? null,
      }
  const r = new TranslateRun(host, {
    params,
    detected: pin ? pin.detected_lang : payload.detected_lang,
    target: pin ? pin.target_lang : payload.target_lang,
    dict: pin ? (pin.dict_mode ?? false) : payload.dict_mode,
  })
  // Pinned 里已经结束的结果：首帧就同步写入（不等 effect），PinnedApp 据此测量
  if (pin && !pin.request_id)
    r.showStatic(pin.result_text, pin.status ?? null, pin.model_ref ?? null)
  return r
}

/** 正文的一层（当前 / 正在淡出的旧正文）：重新渲染时必须沿用挂载时的排版，否则 StreamView 会重建写入器 */
interface TextLayer {
  readonly gen: number
  readonly format: StreamFormat
  readonly dict: boolean
  readonly static: boolean
  readonly hasText: boolean
  readonly visible: boolean
  readonly dimmed: boolean
}

/** 底栏的轻提示（替换状态行显示） */
interface Flash {
  readonly id: number
  readonly text: string
  readonly tone: 'success' | 'warning'
}

/** 正文区的最小高度（卡片换成骨架 / 正文、输入视图交接）：px 与骨架行数 */
interface Floor {
  readonly px: number
  readonly lines: number
}

/** 「在底部」的容差（px，与 StreamView 的跟随判断一致） */
const FOLLOW_SLACK = 8

/** 骨架最多画几行（卡片很高时其余高度由最小高度撑住） */
const MAX_SKELETON_LINES = 8

/** 当前正文层 / 正在淡出的旧正文层（正文区的直接子元素，按语义类名查找） */
const CURRENT_TEXT = ':scope > .tv-stream:not(.tv-stream-out)'
const OUTGOING_TEXT = ':scope > .tv-stream-out'

/** 正文一行的高度（px）：正文字号 × 行高（令牌） */
function linePx(el: HTMLElement): number {
  const cs = getComputedStyle(el)
  const size = Number.parseFloat(cs.getPropertyValue('--text-body')) || 14
  const lh = Number.parseFloat(cs.getPropertyValue('--lh-body')) || 1.6
  return size * lh
}

/**
 * 正文溢出时滚动容器可以用 Tab 聚焦（WebKit / WKWebView 里键盘用户才能滚动长译文）；不溢出时去掉，不多一个空停顿
 */
function useFocusableScroller(
  body: RefObject<HTMLElement | null>,
  gen: number,
  enabled: boolean,
): void {
  useEffect(() => {
    // 每一代正文是新的 StreamView（gen 变化时重新挂上观察）
    if (!enabled || gen === 0) return
    const el = body.current?.querySelector<HTMLElement>(CURRENT_TEXT)
    if (!el) return
    const sync = () => {
      const over = el.scrollHeight > el.clientHeight + 1
      if (over && el.tabIndex !== 0) el.tabIndex = 0
      else if (!over && el.hasAttribute('tabindex')) el.removeAttribute('tabindex')
    }
    const observer = new ResizeObserver(sync)
    observer.observe(el)
    for (const child of el.children) observer.observe(child)
    return () => {
      observer.disconnect()
      el.removeAttribute('tabindex')
    }
  }, [body, gen, enabled])
}

export function TranslateView(props: ViewProps<'translate'>) {
  // JIT 预热渲染：不发 IPC、不接上 / 发起请求、不拉取设置（见模块注释）
  const prewarm = usePrewarm()
  const { payload, host, sessionId, phase, focused, maxHeight, frame } = props
  const t = useT()
  const pinned = usePinnedHost()
  const pinnedChrome = usePinnedChrome()
  // Pinned：首帧就绪由本视图负责（仍在进行的请求要等 stream_attach 回放完）
  usePinnedReadyHold()
  const isPinned = host === 'pinned' && pinned !== null
  // 面板宿主：从头部与空白处拖动（shared/hooks/usePanelDrag）；固定窗口的头部是 data-tauri-drag-region
  const panelDrag = usePanelDrag(sessionId, host === 'panel')
  const catalog = useCatalog(!prewarm)
  const styles = useMemo(() => styleList(catalog), [catalog])
  const settings = translateSettings(catalog)
  const reduced = useReducedMotionPref()
  const viewActive = useViewActive()
  const scrollbar = useScrollbarWidth()

  // 交接来的会话内选择（输入视图、改宽度时重建的实例）：只在创建时读一次
  const [handoff] = useState<Handoff | null>(() =>
    isPinned ? null : readHandoff(payload.request_id ?? pendingKey(sessionId)),
  )

  // 原文对照：用户在本会话里切换过就以它为准；否则 Pinned 取 PinState、面板取交接来的选择或设置里的默认值
  // （设置快照可能在挂载之后才到，所以默认值是推导出来的，不在初始化时定格）
  const [sourceChoice, setSourceChoice] = useState<boolean | null>(() =>
    isPinned ? pinned.state.show_source : (handoff?.showSource ?? null),
  )
  const showSource = sourceChoice ?? catalog?.settings.translate.show_source_by_default ?? false
  const toggleSource = () => setSourceChoice(!showSource)
  // 交换方向时强制显示的源语言（检测不确定时「把原文当作另一种语言」，text.ts planSwap）
  const [forcedSource, setForcedSource] = useState<Lang | null>(() => handoff?.forcedSource ?? null)

  const runSession = isPinned ? 0 : sessionId
  const [run] = useState(() =>
    createRun(
      payload,
      isPinned ? pinned.state : null,
      handoff,
      makeHost(runSession, styles, props, showSource, forcedSource),
    ),
  )
  // 每次提交后换上读到本次渲染值的回调（换风格之后、设置变化之后状态机都读得到最新的风格表）
  useLayoutEffect(() => {
    run.setHost(makeHost(runSession, styles, props, showSource, forcedSource))
  })
  const snap = useSyncExternalStore(run.subscribe, run.getSnapshot, run.getSnapshot)
  const params = snap.params

  /** 离线模型在未就绪卡片上下载完成、已经重新发起这次翻译：不再显示卡片 */
  const [readyNow, setReadyNow] = useState(false)
  const notReady = isPinned || readyNow ? null : payload.not_ready
  // 长文本确认中：风格 / 任务 / 模型 / 方向只改待发请求的参数，确认后才按新参数发起
  const [longTextPending, setLongTextPending] = useState(
    () =>
      !isPinned && notReady === null && payload.request_id === null && payload.long_text !== null,
  )
  const configuring = longTextPending
  const blocked = notReady !== null || longTextPending

  // ---------------------------------------------------------------------------
  // 进入后只启动一次（不以 phase 为依赖）：接上 Rust 已发出的请求、交接来的请求，或自己发起

  const readyPending = useRef(false)
  // 挂载时的意图（载荷在一个视图实例里不变：换载荷由 ViewSwitch 换新实例）
  const [mount] = useState(() => ({
    pinned: isPinned,
    requestId: payload.request_id,
    autoStart: payload.not_ready === null && payload.long_text === null,
  }))
  useEffect(() => {
    if (prewarm) return
    viewMounted = true
    let alive = true
    if (pinned && mount.pinned) {
      const pin = pinned.state
      if (pin.request_id) {
        void run.attach(pin.request_id).then((ok) => {
          if (!alive) return
          // 请求已经结束、日志也已释放（not_found）：退回 PinState 里的结果
          if (!ok) run.showStatic(pin.result_text, pin.status ?? null, pin.model_ref ?? null)
          readyPending.current = true
          // 没有任何事件（空日志）时也要有一次提交来触发下面的布局 effect
          run.touch()
        })
      } else {
        pinned.ready()
      }
    } else if (mount.requestId || mount.autoStart) {
      // 面板：放进紧随其后的微任务——仍在任何计时器 / 帧之前发出，但不落在 prepare 的同步路径（flushSync → 测量 →
      // panel_ready）里：接上 / 发起会通知状态机的订阅者，同步执行会让整个视图在 ready 之前多渲染一遍
      const requestId = mount.requestId
      queueMicrotask(() => {
        if (!alive) return
        if (requestId) {
          void run.attach(requestId).then((ok) => {
            if (alive && !ok) run.start()
          })
        } else run.start()
      })
    }
    return () => {
      alive = false
      run.suspend()
    }
  }, [run, pinned, mount, prewarm])

  // Pinned：回放完的那次提交之后（StreamView 已绑定、文字已同步写入）通知 PinnedApp 测量并 pinned_ready
  useLayoutEffect(() => {
    if (!readyPending.current || !pinned) return
    readyPending.current = false
    pinned.ready()
  })

  // 面板开始关闭（Esc / ✕ / ⌘W / 点击外部……）：一并取消请求并冻结界面；已转给 Pinned 的不取消
  useEffect(() => {
    if (phase === 'exiting') run.cancelForClose()
  }, [phase, run])

  const busy = snap.status === 'waiting' || snap.status === 'streaming'
  // 挂载后紧随其后的微任务里就会接上 / 发起请求：这一刻（面板 prepare 的同步首帧）底栏与状态行已按「等待中」渲染，
  // 不先渲染一次完成态的操作（含 Motion 容器）再立即换掉
  const starting =
    snap.status === 'idle' &&
    !mount.pinned &&
    (mount.requestId !== null || mount.autoStart) &&
    !longTextPending
  const footerBusy = busy || starting
  const shown = phase === 'shown' && viewActive

  // 真正空闲（显示中、没有流式输出）时：补 Markdown-lite 的字体预热、复核滚动条宽度（都不在进入动画与流式期间）；
  // 流式期间同时暂停模块加载后开始的后台预热（warmup.ts）
  useEffect(() => {
    if (prewarm) return
    if (busy) return holdMarkdownWarmup()
    if (!shown) return
    const cancelWarm = scheduleMarkdownWarmup()
    const cancelMeasure = measureScrollbarWhenIdle()
    return () => {
      cancelWarm()
      cancelMeasure()
    }
  }, [shown, busy, prewarm])

  // ---------------------------------------------------------------------------
  // 正文：StreamView 以 textGen 为 key，挂载后把缓冲的文字交给它

  const streamRef = useRef<StreamHandle | null>(null)
  const textGen = snap.textGen
  const bindStream = useMemo(
    () => (handle: StreamHandle | null) => {
      if (handle) streamRef.current = handle
      else if (run.getSnapshot().textGen === textGen) streamRef.current = null
      run.bindSink(textGen, handle)
    },
    [run, textGen],
  )

  const editTask = params.task === 'polish' || params.task === 'proofread'
  const hasResult = snap.textGen > 0 && snap.hasText
  // 思考耗尽且这一代没有正文：正文区显示空状态；提升译文的思考耗尽保留原译文（run.ts）
  const exhaustedEmpty = snap.thinkingExhausted && !snap.hasText
  // 上一次请求结束时没有任何正文（思考耗尽之后重新生成）：新请求等首字时显示骨架，不留一个空的正文层
  const staleEmpty = !snap.hasText && snap.textGen !== snap.gen
  const showText =
    snap.textGen > 0 &&
    !(snap.status === 'error' && snap.textGen !== snap.gen) &&
    !notReady &&
    !exhaustedEmpty &&
    !staleEmpty

  // 换新正文时旧正文留在下层淡出（与新正文交叉淡入，不出现空帧）。上一次提交时的正文层记在 lastText
  const lastText = useRef<TextLayer | null>(null)
  const [outgoing, setOutgoing] = useState<TextLayer | null>(null)
  const [seenGen, setSeenGen] = useState(textGen)
  if (seenGen !== textGen) {
    setSeenGen(textGen)
    const last = lastText.current
    setOutgoing(
      last && last.gen === seenGen && last.gen > 0 && last.visible && last.hasText ? last : null,
    )
  }
  useLayoutEffect(() => {
    lastText.current = {
      gen: snap.textGen,
      format: snap.textFormat,
      dict: snap.textDict,
      static: snap.textStatic,
      hasText: snap.hasText,
      visible: showText,
      dimmed: snap.dimmed,
    }
  })

  // 正文区的最小高度：
  // - 第一次流式期间不低于骨架屏（文字从第一行写起时面板不先缩再长，data-hold）；
  // - 之后每一代新正文都不低于被替换的旧正文（hold），直到新内容超过它或流结束——结束时只按最终高度收一次；命中缓存
  //   （新正文一次性到齐、流已结束）时不保持：旧正文叠在上面淡出，卡片同时向新高度收缩；
  // - 卡片（长文本确认 / 错误 / 思考耗尽）换成骨架或正文、输入视图交接过来时，不低于原来的高度（floor），同样到流结束
  const [skeleton] = useState(() =>
    skeletonLines(payload.source_text, payload.target_lang, payload.dict_mode),
  )
  const [firstGen, setFirstGen] = useState(0)
  if (firstGen === 0 && snap.textGen > 0) setFirstGen(snap.textGen)
  const [hold, setHold] = useState<{ gen: number; px: number } | null>(null)
  const holdSkeleton = busy && !isPinned && (snap.textGen === 0 || snap.textGen === firstGen)
  const holdPx = !isPinned && busy && hold && hold.gen === snap.textGen ? hold.px : null
  const [floor, setFloor] = useState<Floor | null>(() => handoff?.body ?? null)
  if (floor !== null && !footerBusy && snap.status !== 'idle') setFloor(null)
  const floorPx = !isPinned && footerBusy && floor ? floor.px : null
  const skeletonCount = Math.min(
    MAX_SKELETON_LINES,
    Math.max(skeleton, floorPx !== null && floor ? floor.lines : 0),
  )
  const minBody = Math.max(holdPx ?? 0, floorPx ?? 0)

  // 风格切换的动效：宽限期过后旧正文 opacity 0.3 + 2 px 模糊（Linux 与减少动效只降透明度：WebKitGTK 上 filter 动画容易掉帧）；
  // 新内容到达时旧正文淡出、新正文从变暗的状态恢复清晰；宽限期内就到达（命中缓存）时直接交叉淡入
  // （presets.pageSwitch）。只在切换时播放，挂载时不创建任何动画
  const [bodyScope, animateBody] = useAnimate<HTMLElement>()
  const prevDim = useRef({ dimmed: snap.dimmed, textGen })
  useLayoutEffect(() => {
    const body = bodyScope.current
    const prev = prevDim.current
    prevDim.current = { dimmed: snap.dimmed, textGen }
    if (!body) return
    const plain = reduced || boot.os === 'linux'
    const outPreset = plain ? presets.styleSwitchOutPlain : presets.styleSwitchOut
    const inPreset = plain ? presets.styleSwitchInPlain : presets.styleSwitchIn
    const play = (
      el: HTMLElement,
      motion: { keyframes: Record<string, unknown>; transition: object },
    ) => {
      if (typeof el.animate !== 'function') {
        // 没有 WAAPI 的环境（jsdom）：直接落到终值
        for (const [k, v] of Object.entries(motion.keyframes))
          el.style.setProperty(k, String(Array.isArray(v) ? v[v.length - 1] : v))
        return Promise.resolve()
      }
      return animateBody(el, motion.keyframes as never, motion.transition as never).then(() => {
        // 恢复到终值后去掉内联的 opacity: 1 / blur(0px)：不留合成层，文字照常按子像素抗锯齿绘制
        const last = (v: unknown) => (Array.isArray(v) ? v[v.length - 1] : v)
        if (last(motion.keyframes.opacity) === 1) el.style.removeProperty('opacity')
        if (last(motion.keyframes.filter) === 'blur(0px)') el.style.removeProperty('filter')
      })
    }
    const current = body.querySelector<HTMLElement>(CURRENT_TEXT)
    if (textGen !== prev.textGen) {
      const old = body.querySelector<HTMLElement>(OUTGOING_TEXT)
      if (!old || !outgoing) return
      // 新正文代次：流式中正文区先保持旧正文的高度（命中缓存时流已经结束，holdPx 不生效）
      setHold({ gen: textGen, px: old.offsetHeight })
      const gen = outgoing.gen
      const drop = () => setOutgoing((o) => (o?.gen === gen ? null : o))
      const fade = presets.pageSwitch
      void play(
        old,
        presetMotion(
          { opacity: fade.exit.opacity, transition: fade.exit.transition },
          reduced,
          'out',
        ),
      ).finally(drop)
      if (!current) return
      if (outgoing.dimmed) {
        // 从变暗的状态恢复清晰（styleSwitchOut → styleSwitchIn）
        const from = presetMotion(outPreset, reduced)
        const to = presetMotion(inPreset, reduced)
        const keyframes: Record<string, unknown> = {}
        for (const [k, v] of Object.entries(to.keyframes)) keyframes[k] = [from.keyframes[k], v]
        void play(current, { keyframes, transition: to.transition })
      } else {
        void play(current, {
          keyframes: { opacity: [fade.initial.opacity, fade.animate.opacity] },
          transition: reduced ? presets.reduced.fade : fade.animate.transition,
        })
      }
      return
    }
    if (!current) return
    if (snap.dimmed && !prev.dimmed) void play(current, presetMotion(outPreset, reduced, 'out'))
    else if (!snap.dimmed && prev.dimmed) void play(current, presetMotion(inPreset, reduced, 'in'))
    // 只在代次 / 变暗状态变化时执行
    // oxlint-disable-next-line react/exhaustive-deps
  }, [snap.dimmed, textGen, outgoing])

  // 流式结束：一路跟随到底部的长结果回到开头（学习风格最重要的「译文」小节在最前面）——面板与固定窗口同一种行为
  // （停在末尾的话，开头被顶部渐隐盖住）；用户自己往上滚过（不在底部）时不动
  const wasBusy = useRef(busy)
  useEffect(() => {
    const ended = wasBusy.current && !busy
    wasBusy.current = busy
    if (!ended || snap.status !== 'done') return
    const el = bodyScope.current?.querySelector<HTMLElement>(CURRENT_TEXT)
    if (!el || el.scrollHeight <= el.clientHeight + 1) return
    if (el.scrollTop + el.clientHeight < el.scrollHeight - FOLLOW_SLACK) return
    el.scrollTo({ top: 0, behavior: reduced ? 'auto' : 'smooth' })
    // 只在流式结束时执行一次
    // oxlint-disable-next-line react/exhaustive-deps
  }, [busy])

  useFocusableScroller(bodyScope, textGen, showText && !prewarm)

  // ---------------------------------------------------------------------------
  // 长高 / 变矮时底栏贴住卡片可见的下沿（reveal.ts）

  const rootRef = useRef<HTMLElement>(null)
  // 窗口不能透明（Css 帧模式且 shadow_margin = 0，与外壳 usePanelLifecycle 的 isOpaqueWindow 同一判定）
  const opaque = boot.frame_mode === 'css' && boot.shadow_margin === 0
  const reveal = revealSource({ frame, opaque, pinned: isPinned })
  // 底栏贴住卡片可见的下沿（菜单的下拉帘改由组件库的浮层边界提供，PanelCard 的 edge）
  useRevealEdge(
    rootRef,
    reveal,
    viewActive && (phase === 'entering' || phase === 'shown'),
    maxHeight,
    reduced,
  )

  // ---------------------------------------------------------------------------
  // 卡片（长文本确认 / 错误 / 思考耗尽）换成骨架或正文：正文区先保持卡片原来的高度（直到这次流结束），骨架画满它

  const cardEls = useRef(new Map<string, HTMLElement>())
  const cardRefs = useRef(new Map<string, (el: HTMLElement | null) => void>())
  const cardRef = (key: string) => {
    let r = cardRefs.current.get(key)
    if (!r) {
      r = (el: HTMLElement | null) => {
        if (el) cardEls.current.set(key, el)
        else cardEls.current.delete(key)
      }
      cardRefs.current.set(key, r)
    }
    return r
  }

  // ---------------------------------------------------------------------------
  // 动作

  const [flash, setFlash] = useState<Flash | null>(null)
  // 轻提示的动画容器第一次用到时才挂载（面板 prepare 的同步首帧里只渲染看得见的东西）
  const [flashUsed, setFlashUsed] = useState(false)
  const flashTimer = useRef<ReturnType<typeof setTimeout> | null>(null)
  useEffect(
    () => () => {
      if (flashTimer.current !== null) clearTimeout(flashTimer.current)
    },
    [],
  )
  const showFlash = useCallback((text: string, tone: Flash['tone'] = 'success') => {
    if (flashTimer.current !== null) clearTimeout(flashTimer.current)
    setFlashUsed(true)
    setFlash((f) => ({ id: (f?.id ?? 0) + 1, text, tone }))
    flashTimer.current = setTimeout(() => setFlash(null), ms(timing.toastHold))
  }, [])
  const copied = (ok: boolean, success: string) =>
    showFlash(ok ? success : t('common.state.copy_failed'), ok ? 'success' : 'warning')

  // 原文对照的动画容器第一次用到时才挂载：面板 prepare 的同步首帧里只渲染看得见的东西
  const [sourceMounted, setSourceMounted] = useState(showSource)
  // 挂载时就显示的原文不播放展开动画；之后第一次切换出来的照常播放
  const [sourceAnimateIn] = useState(!showSource)
  if (showSource && !sourceMounted) setSourceMounted(true)
  const footerRef = useRef<HTMLDivElement>(null)
  const [modelMenuRequest, setModelMenuRequest] = useState(0)
  const openModelMenu = () => setModelMenuRequest((n) => n + 1)

  const requestHide = props.requestHide
  const close = useCallback(() => {
    run.cancelForClose()
    requestHide('close_button')
  }, [run, requestHide])

  /** 改请求参数：长文本确认中只记下（确认后按新参数发起），否则立即发起新请求 */
  const apply = (patch: Partial<RunParams>) => {
    if (configuring) run.configure(patch)
    else run.start(patch)
  }
  const pickStyle = (id: string) => {
    if (id === params.styleId && params.task === 'translate' && snap.status !== 'error') return
    apply({ styleId: id, task: 'translate' })
  }
  const cycleStyle = (delta: 1 | -1) => {
    if (styles.length === 0 || notReady) return false
    const i = styles.findIndex((s) => s.id === params.styleId)
    const next = styles[(i + delta + styles.length) % styles.length]
    if (next) pickStyle(next.id)
  }
  const pickTarget = (lang: Lang) => {
    // 目标没变（含自动方向下重选当前显示的目标）：不重新请求
    if (lang === snap.target) return
    setForcedSource(null)
    apply({ target: lang })
  }
  // 交换方向：只在有意义时提供（text.ts planSwap）；不可用时语言条说明原因
  const swapPlan = planSwap({
    detected: snap.detected,
    forced: forcedSource,
    target: snap.target,
    lowConfidence: snap.lowConfidence,
    primary: settings.primary_lang,
    secondary: settings.secondary_lang,
  })
  const swap: SwapControl = notReady
    ? null
    : swapPlan.ok
      ? {
          run: () => {
            setForcedSource(swapPlan.forceSource)
            apply({ target: swapPlan.target })
          },
        }
      : {
          reason: t('translate.a11y.swap_unavailable', {
            lang: t(`common.lang.name.${snap.detected ?? 'other'}`),
          }),
        }
  const regenerate = () => {
    if (blocked) return false
    // 提升译文之后的「重新生成」按普通翻译重来（反思用的译稿已经被替换：同样的文本和风格、绕过缓存）
    run.start(params.task === 'refine' ? { task: 'translate' } : {}, { bypassCache: true })
    return true
  }
  const lastDraft = useRef<string | null>(null)
  const retry = () => run.start({}, { draft: params.task === 'refine' ? lastDraft.current : null })
  // 原文按字符扫描一次（长文本上千字，不在每次渲染里重复）
  const longEnough = useMemo(() => refineEligible(params.text), [params.text])
  // 提升译文只针对完整的已有译文：被停止的半截译文、思考耗尽的空结果都不行
  const canRefine =
    !configuring &&
    !editTask &&
    params.task !== 'refine' &&
    snap.status === 'done' &&
    !snap.thinkingExhausted &&
    hasResult &&
    longEnough
  /** 当前正文（edit_split 只取正文，不含改动；词典式取模型原文，不含显示用的整理标记） */
  const resultText = () => (snap.textDict ? run.rawText() : (streamRef.current?.body() ?? ''))
  const refine = () => {
    if (!canRefine) return false
    const draft = resultText()
    lastDraft.current = draft
    run.start({ task: 'refine' }, { draft })
    return true
  }
  const pickModel = (model: ModelRef | null) => {
    const same =
      model === params.model ||
      (model !== null &&
        params.model !== null &&
        model.provider_id === params.model.provider_id &&
        model.model_id === params.model.model_id)
    if (same && snap.status !== 'error') return
    apply({ model })
  }
  const setTask = (task: 'polish' | 'proofread' | 'translate') => {
    if (task === params.task && snap.status !== 'error') return
    apply({ task })
  }
  const openSettings = (section: SettingsSection) =>
    void api.windowOpen({ kind: 'settings', section })
  const openPermission = (kind: PermissionKind) => void api.permissionsOpen(kind)

  // ⌘C 复制全文：经底栏的复制按钮执行（它的图标换成对勾），同时显示轻提示「已复制」——键盘复制时视线在正文上，
  // 很难注意到右下角的小图标。点击复制按钮只换图标
  const keyboardCopy = useRef(false)
  const copy = () => {
    // 视图内有选区时复制选区，否则复制全部译文（不含原文和状态行）
    const sel = window.getSelection()
    const root = rootRef.current
    const selected =
      sel && !sel.isCollapsed && root && sel.anchorNode && root.contains(sel.anchorNode)
        ? stripUrlBreaks(sel.toString())
        : ''
    if (selected.trim()) {
      void copyText(selected).then((ok) => copied(ok, t('translate.status.copied_selection')))
      return
    }
    const text = hasResult ? resultText() : ''
    if (!text) return false
    const button = footerRef.current?.querySelector('.ui-iconswap')?.closest('button')
    if (button) {
      keyboardCopy.current = true
      button.click()
      return
    }
    void copyText(text).then((ok) => copied(ok, t('common.state.copied')))
  }
  const onCopied = (ok: boolean) => {
    if (!keyboardCopy.current) return
    keyboardCopy.current = false
    copied(ok, t('common.state.copied'))
  }
  const copySource = () =>
    void copyText(params.text).then((ok) => copied(ok, t('translate.status.copied_source')))

  const pinning = useRef(false)
  const pin = (expand: boolean) => {
    if (host !== 'panel' || pinning.current || blocked) return false
    const s = run.getSnapshot()
    // 出错时没有「固定」（与底栏一致：只保留复制与原文对照）
    if (s.status === 'error') return false
    const transfer = run.active && s.requestId !== null && !isLocalRequest(s.requestId)
    // 没有正文、也没有进行中的请求可以转移：没有东西可固定
    if (!transfer && !(s.textGen > 0 && s.hasText)) return false
    const state: PinState = {
      kind: 'translate',
      source_text: s.params.text,
      // 词典式交接模型原文（Pinned 重新整理排版）；其余是 StreamView 里的全文（edit_split 含改动）
      result_text: s.textDict ? run.rawText() : (streamRef.current?.text() ?? ''),
      task: s.textGen > 0 ? s.textTask : s.params.task,
      style_id: s.params.styleId,
      target_lang: s.target,
      detected_lang: s.detected,
      request_id: transfer ? s.requestId : null,
      capture_id: null,
      ocr_text: null,
      ocr_mode: null,
      // 展开 ⤢ 默认显示原文对照
      show_source: expand || showSource,
      // 纯文本形式（兼容只认 status_line 的读取方）；固定窗口读结构化的 status
      status_line: statusLineText(s, t),
      dict_mode: s.textGen > 0 ? s.textDict : (s.dict ?? false),
      model_ref: s.model ? { provider_id: s.model.providerId, model_id: s.model.modelId } : null,
      status: pinStatusOf(s),
    }
    pinning.current = true
    // 进行中的流随窗口转移：之后面板关闭不再取消它
    if (transfer) run.release()
    api.panelPin(sessionId, state, expand).then(
      () => {
        pinning.current = false
      },
      () => {
        pinning.current = false
        if (transfer) run.release(false)
      },
    )
    return true
  }

  // ---------------------------------------------------------------------------
  // 键盘：Esc / ⌘W / ⌘, 由外壳（Pinned 为 PinnedApp）处理。
  // ⌘R / ⌘⇧R / ⌘P 在动作当前不可用时也吞掉（keys.ts：否则落到 WebView2 的重载 / 打印预览）

  const bindings: KeyBinding[] = [
    { combo: 'Mod+KeyC', run: () => copy() },
    {
      combo: 'Mod+KeyR',
      run: () => {
        regenerate()
      },
    },
    {
      combo: 'Mod+Shift+KeyR',
      run: () => {
        refine()
      },
    },
    { combo: 'Mod+Period', run: () => (busy ? run.stop() : false) },
    { combo: 'Mod+ArrowDown', run: () => cycleStyle(1) },
    { combo: 'Mod+ArrowUp', run: () => cycleStyle(-1) },
    // Pinned 里没有「固定」：同样吞掉
    {
      combo: 'Mod+KeyP',
      run: () => {
        if (host === 'panel') pin(false)
      },
    },
    ...Array.from({ length: STYLE_SHORTCUTS }, (_, i) => ({
      combo: `Mod+Digit${i + 1}`,
      run: () => {
        const s = styles[i]
        if (!s || notReady) return false
        pickStyle(s.id)
      },
    })),
  ]
  useKeyScope('translate', bindings)

  // ---------------------------------------------------------------------------
  // 渲染

  const provider = providerOf(catalog, snap.model?.providerId ?? null)
  const choices = useMemo(() => modelChoices(catalog), [catalog])
  // 本机选用的离线翻译模型优先于同步的默认模型（与 Rust 的选择顺序一致）
  const offlineTranslate = catalog?.settings.offline?.translate_model ?? null
  const defaultRef = offlineTranslate
    ? offlineRef(offlineTranslate)
    : (catalog?.settings.defaults.translate ?? null)
  const defaultModel =
    (defaultRef
      ? choices.find(
          (c) =>
            c.ref.provider_id === defaultRef.provider_id && c.ref.model_id === defaultRef.model_id,
        )?.label
      : choices[0]?.label) ?? null
  // 离线翻译模型不支持润色 / 校对：有默认的云端翻译模型时给一个显式的「用 <模型> 处理这一次」（用户自己点的，不是悄悄
  // 改用云端），这一次的请求带上它的模型引用；默认模型已经不在列表里（服务被删）时只留「打开设置」
  const cloudDefault = catalog?.settings.defaults.translate ?? null
  const cloudChoice = cloudDefault
    ? choices.find(
        (c) =>
          c.ref.provider_id === cloudDefault.provider_id &&
          c.ref.model_id === cloudDefault.model_id,
      )
    : undefined
  const refineHint = longEnough ? null : t('translate.more.refine_hint')
  const detecting = busy && snap.detected === null && !editTask && forcedSource === null
  const showSkeleton =
    !showText && !blocked && snap.status !== 'error' && (busy || snap.status === 'idle')
  const exhausted = snap.status === 'done' && exhaustedEmpty && !notReady
  // 流式中途出错仍保留已收到的部分：底栏照常提供复制、原文对照
  const footerVisible = !blocked && (snap.status !== 'error' || (showText && hasResult))
  const canPin = host === 'panel' && hasResult && snap.status !== 'error'

  // 正文区里的卡片：正文之前（骨架 / 长文本确认 / 未就绪 / 思考耗尽）与之后（错误：保留已收到的部分时排在它下面）
  const leadKey: string | null = notReady
    ? 'notready'
    : longTextPending && payload.long_text
      ? 'longtext'
      : exhausted
        ? 'exhausted'
        : showSkeleton
          ? 'skeleton'
          : null
  const errorKey = snap.status === 'error' && snap.error ? `error-${snap.gen}` : null
  const prevCards = useRef({ lead: leadKey, error: errorKey })
  useLayoutEffect(() => {
    const prev = prevCards.current
    prevCards.current = { lead: leadKey, error: errorKey }
    if (!footerBusy || isPinned) return
    const gone = [
      prev.lead !== leadKey && prev.lead !== 'skeleton' ? prev.lead : null,
      prev.error !== errorKey ? prev.error : null,
    ]
    let px = 0
    for (const key of gone) {
      // 正在退场的卡片已经脱离排版（popLayout），仍停在原处：它的下沿就是原来正文区的高度
      const el = key ? cardEls.current.get(key) : undefined
      if (el) px = Math.max(px, el.offsetTop + el.offsetHeight)
    }
    const body = bodyScope.current
    if (px <= 0 || !body) return
    setFloor({ px, lines: Math.floor(px / linePx(body)) })
    // 只在卡片换掉时执行
    // oxlint-disable-next-line react/exhaustive-deps
  }, [leadKey, errorKey])

  const statusProps = ((): StatusLineProps => {
    if (snap.status === 'cancelled') return { state: 'cancelled' }
    if (footerBusy) {
      const note =
        snap.phase === 'loading_model'
          ? t('common.state.loading_model')
          : snap.phase === 'reviewing'
            ? t('translate.status.reviewing')
            : snap.phase === 'improving'
              ? t('translate.status.improving')
              : snap.status !== 'streaming'
                ? t('translate.status.waiting')
                : params.task === 'polish'
                  ? t('translate.status.polishing')
                  : params.task === 'proofread'
                    ? t('translate.status.proofreading')
                    : t('translate.status.translating')
      return { model: snap.model?.label ?? null, note: { text: note } }
    }
    let note: StatusLineProps['note'] = null
    if (snap.status === 'error') return { model: snap.model?.label ?? null }
    // 思考耗尽显示在状态行；没有正文时正文区的空状态已经说明原因、给出下一步——状态行只留模型 · 耗时，
    // 不再用琥珀色重复一遍（重复的那句会把模型名挤成「GPT-5…」）
    if (snap.thinkingExhausted && !exhausted)
      note = { icon: BrainCircuit, text: t('panel.status.thinking_exhausted'), tone: 'warning' }
    else if (snap.thinkingExhausted) note = null
    else if (snap.noImprovement)
      note = { icon: CircleCheck, text: t('translate.status.no_improvement') }
    else if (snap.refined === 'improved')
      note = { icon: WandSparkles, text: t('translate.status.refined') }
    else if (snap.refined === 'original')
      note = { icon: History, text: t('translate.status.original') }
    else if (snap.notices.includes('retried'))
      note = { icon: RotateCw, text: t('panel.status.retried') }
    else if (fromCache(snap)) note = { icon: Zap, text: t('panel.status.cached') }
    return {
      model: snap.model?.label ?? null,
      // 命中缓存时的「0 ms」不是真实耗时：不显示（「来自缓存」由 note 说明）
      elapsedMs: fromCache(snap) ? null : snap.elapsedMs,
      usage: snap.usage,
      note,
    }
  })()

  // 需要读完整的提示：单独一行，不和指标、按钮挤在一起（截断：重新生成或调高输出上限；参数已调整）
  const notice: {
    icon: LucideIcon
    text: string
    tone: 'warning' | 'default'
    action?: ReactNode
  } | null =
    snap.status === 'done' && hasResult && snap.truncated && !snap.thinkingExhausted
      ? {
          icon: AlertTriangle,
          tone: 'warning',
          text: t('panel.status.truncated'),
          action: (
            <Button
              size="sm"
              variant="ghost"
              className="ui-link-action"
              onClick={() => openSettings('providers')}
              data-testid="tv-notice-action"
            >
              {t('translate.notice.raise_limit')}
            </Button>
          ),
        }
      : snap.status === 'done' && snap.notices.includes('param_degraded')
        ? { icon: Info, tone: 'default', text: t('panel.status.param_degraded') }
        : null

  // 状态行悬停 / 键盘聚焦时显示输入 / 输出 token
  const tokens =
    snap.usage && snap.status === 'done'
      ? t('translate.status.tokens', {
          input: formatNumber(snap.usage.input_tokens),
          output: formatNumber(snap.usage.output_tokens),
        })
      : null

  // 状态行：有 usage 时包一层 Tooltip（悬停 / 键盘聚焦显示输入 / 输出 token）；之前（含 prepare 的同步首帧）
  // 只渲染状态行本身
  const statusBody = <StatusLine {...statusProps} data-testid="tv-status" />
  const statusContent = tokens ? (
    <Tooltip content={tokens} className="tv-status-anchor">
      <span
        className="tv-status-focus"
        // oxlint-disable-next-line jsx-a11y/no-noninteractive-tabindex -- tooltip 的触发点要能获得焦点（键盘用户也能看到 token 明细）
        tabIndex={0}
        aria-label={[statusLineText(snap, t), tokens].filter(Boolean).join(STATUS_SEPARATOR)}
      >
        {statusBody}
      </span>
    </Tooltip>
  ) : (
    statusBody
  )

  // 视图整体的高度上限 = 卡片上限 − 上下内边距（PanelCard 的 .panel-content；Pinned 的卡片同样的内边距）。
  // Pinned 测量首帧时不设上限（展开窗口按内容定高），之后铺满窗口
  const measuring = isPinned && phase === 'preparing'
  const rootStyle = {
    '--tv-max': measuring
      ? 'none'
      : `calc(${Math.max(0, Math.round(maxHeight))}px - 2 * var(--pad-panel-y))`,
    '--tv-sbw': `${scrollbar}px`,
  } as CSSProperties
  // 正文的内部滚动上限：测量首帧不限（拿到真实的自然高度）
  const streamMax = measuring ? undefined : Math.max(0, maxHeight)
  const footerIn = usePreset('footerIn')
  const cardIn = usePreset('cardIn')
  const sectionExpand = usePreset('sectionExpand')
  const toast = usePreset('toast')
  // 交叉淡入（presets.pageSwitch；减少动效时 ≤ 100 ms 的淡入淡出）：原文对照（有揭开时）、正文区里的卡片 / 骨架
  const fadeIn = reduced
    ? { opacity: 1, transition: presets.reduced.fade }
    : presets.pageSwitch.animate
  const fadeOut = reduced
    ? { opacity: 0, transition: presets.reduced.fade }
    : presets.pageSwitch.exit
  // 原文对照：有揭开时（面板）原文立即排版、淡入，由揭开随卡片的弹簧逐渐露出；收起时脱离排版（popLayout）、淡出，
  // 由揭开从下往上裁、正文与底栏随卡片下沿收上来——展开与收起对称，都只有卡片一个弹簧。Pinned / opaque 时高度动画
  const sourceMotion = reveal
    ? { initial: presets.pageSwitch.initial, animate: fadeIn, exit: fadeOut }
    : sectionExpand
  const crossfade = reduced ? presets.reduced.fade : presets.crossfade
  const dragRegion = isPinned || undefined

  const textLayers: Array<Pick<TextLayer, 'gen' | 'format' | 'dict' | 'static'>> = []
  if (outgoing && outgoing.gen !== snap.textGen) textLayers.push(outgoing)
  if (showText)
    textLayers.push({
      gen: snap.textGen,
      format: snap.textFormat,
      dict: snap.textDict,
      static: snap.textStatic,
    })
  const textClass = (layer: { dict: boolean; static: boolean }) =>
    cx('tv-stream', layer.dict && 'tv-dict', layer.static && 'tv-static')

  const leadCard: ReactNode =
    leadKey === 'notready' && notReady ? (
      isOfflineNotReady(notReady) ? (
        // 离线翻译模型还没下载：就地下载，完成后直接翻译这段文字；「切换模型」打开默认模型的设置
        <OfflineCard
          notReady={notReady}
          onReady={() => {
            setReadyNow(true)
            run.start()
          }}
          onSwitchModel={() => openSettings('providers')}
          data-testid="tv-not-ready"
        />
      ) : (
        <NotReadyCard
          notReady={notReady}
          // 系统凭据存储不可用时同样「去设置」（Key 只能在设置里重新填写，组件库提供）；离线模型不支持的任务打开离线模型分组
          onOpenSettings={() =>
            openSettings(notReady.reason === 'offline_task_unsupported' ? 'offline' : 'providers')
          }
          onOpenSystemSettings={openPermission}
          actions={
            notReady.reason === 'offline_task_unsupported' && cloudChoice ? (
              <>
                <Button
                  size="sm"
                  variant="primary"
                  onClick={() => {
                    setReadyNow(true)
                    run.start({ model: cloudChoice.ref })
                  }}
                  data-testid="tv-not-ready-use-cloud"
                >
                  {t('panel.not_ready.offline_task_unsupported.use_cloud', {
                    model: cloudChoice.label,
                  })}
                </Button>
                <Button
                  size="sm"
                  variant="secondary"
                  onClick={() => openSettings('offline')}
                  data-testid="tv-not-ready-settings"
                >
                  {t('common.action.go_settings')}
                </Button>
              </>
            ) : undefined
          }
          data-testid="tv-not-ready"
        />
      )
    ) : leadKey === 'longtext' && payload.long_text ? (
      <LongTextPrompt
        info={payload.long_text}
        onContinue={() => {
          if (!longTextPending) return
          setLongTextPending(false)
          run.start()
        }}
        onCancel={close}
      />
    ) : leadKey === 'exhausted' ? (
      <EmptyState
        compact
        icon={BrainCircuit}
        title={t('translate.exhausted.title')}
        detail={t('translate.exhausted.detail')}
        className="tv-exhausted"
        data-testid="tv-exhausted"
        action={
          // 一主两次：「重新生成」是主操作；「切换模型」「调高上限」是同一种带图标的 ghost 按钮，高度都是 28，
          // 不在按钮组里混进内联链接样式
          <>
            <Button
              size="sm"
              variant="primary"
              icon={RotateCw}
              onClick={() => void regenerate()}
              data-testid="tv-exhausted-regenerate"
            >
              {t('common.action.regenerate')}
            </Button>
            {isPinned ? null : (
              <Button
                size="sm"
                variant="ghost"
                icon={Cpu}
                onClick={openModelMenu}
                data-testid="tv-exhausted-model"
              >
                {t('common.action.switch_model')}
              </Button>
            )}
            {/* 说明里的第三条出路：去设置 › 模型服务改输出上限 */}
            <Button
              size="sm"
              variant="ghost"
              icon={SlidersHorizontal}
              onClick={() => openSettings('providers')}
              data-testid="tv-exhausted-raise"
            >
              {t('translate.notice.raise_limit')}
            </Button>
          </>
        }
      />
    ) : leadKey === 'skeleton' ? (
      <>
        <Skeleton lines={skeletonCount} />
        {/* 离线模型冷启动：状态行已写着「正在加载离线模型…」，几秒后再淡入一句为什么要等（CSS 的 animation-delay，
            快的时候不会一闪而过；见 components.css 的 .ui-late） */}
        {snap.phase === 'loading_model' ? (
          <p className="tv-wait-note ui-late" data-testid="tv-loading-note">
            {t('common.state.loading_slow')}
          </p>
        ) : null}
      </>
    ) : null

  return (
    // 按下：没有焦点时让面板成为 key（翻译视图不抢焦点地出现），并从头部 / 空白处开始拖动（usePanelDrag），不是交互控件
    // oxlint-disable-next-line jsx-a11y/no-noninteractive-element-interactions
    <section
      ref={rootRef}
      className="tv"
      data-testid="view-translate"
      data-view="translate"
      data-session={sessionId}
      data-host={host}
      data-status={snap.status}
      data-fill={(isPinned && !measuring) || undefined}
      aria-label={t('translate.a11y.view')}
      aria-busy={busy || undefined}
      style={rootStyle}
      onPointerDown={(e) => {
        if (host === 'panel' && !focused) props.activate()
        panelDrag?.onPointerDown(e)
      }}
      onMouseDown={panelDrag?.onMouseDown}
    >
      <div className="tv-header" data-tauri-drag-region={dragRegion}>
        {editTask ? (
          <>
            <span className="tv-title" data-tauri-drag-region={dragRegion}>
              {t(`translate.task_title.${params.task}` as 'translate.task_title.polish')}
            </span>
            <span className="tv-spacer" data-tauri-drag-region={dragRegion} />
            <Chip
              variant="subtle"
              icon={Languages}
              onClick={() => setTask('translate')}
              data-testid="tv-back"
            >
              {t('translate.more.back_to_translate')}
            </Chip>
          </>
        ) : (
          <>
            <LangSwitch
              source={forcedSource ?? snap.detected}
              target={snap.target}
              detecting={detecting}
              lowConfidence={snap.lowConfidence && forcedSource === null}
              targets={[settings.primary_lang, settings.secondary_lang]}
              onPickTarget={notReady ? undefined : pickTarget}
              swap={swap}
              data-testid="tv-langbar"
            />
            {snap.lowConfidence && !busy ? (
              <output className="ui-sr-only">{t('panel.status.low_confidence')}</output>
            ) : null}
            <span className="tv-spacer" data-tauri-drag-region={dragRegion} />
            {notReady ? null : (
              <StyleMenu
                styles={styles}
                current={params.styleId}
                onPick={pickStyle}
                onEdit={() => openSettings('translate')}
              />
            )}
          </>
        )}
        {notReady ? null : (
          <MoreMenu
            task={params.task}
            refine={{ enabled: canRefine, hint: refineHint }}
            refined={snap.refined}
            models={choices}
            model={params.model}
            defaultModel={defaultModel}
            onTask={setTask}
            onRefine={refine}
            onToggleRefined={() => run.toggleRefined()}
            onCopySource={copySource}
            onModel={pickModel}
            onEditStyle={() => openSettings('translate')}
            modelMenuRequest={modelMenuRequest}
          />
        )}
        {isPinned ? (
          pinnedChrome
        ) : (
          <IconButton
            icon={X}
            label={t('common.action.close')}
            shortcut="Escape"
            onClick={close}
            data-testid="tv-close"
          />
        )}
      </div>

      {sourceMounted ? (
        <AnimatePresence initial={sourceAnimateIn} mode={reveal ? 'popLayout' : 'sync'}>
          {showSource && !notReady ? (
            <m.section
              key="source"
              className="tv-source-clip"
              aria-label={t('translate.a11y.source')}
              {...sourceMotion}
            >
              <ScrollArea className="tv-source ui-selectable" data-testid="tv-source">
                {params.text}
              </ScrollArea>
            </m.section>
          ) : null}
        </AnimatePresence>
      ) : null}

      <section
        ref={bodyScope}
        className="tv-body"
        aria-label={t('translate.a11y.result')}
        data-dimmed={snap.dimmed || undefined}
        data-hold={(holdSkeleton && minBody === 0) || undefined}
        style={
          {
            '--tv-hold': skeleton,
            minHeight: minBody > 0 ? minBody : undefined,
          } as CSSProperties
        }
      >
        {/* 正文之前的卡片 / 骨架：换掉时脱离排版、淡出，叠在新内容下层（与旧正文同一种叠层方式） */}
        <AnimatePresence initial={false} mode="popLayout">
          {leadKey ? (
            <m.div
              key={leadKey}
              ref={cardRef(leadKey)}
              className="tv-card"
              initial={leadKey === 'skeleton' ? presets.pageSwitch.initial : false}
              animate={fadeIn}
              exit={fadeOut}
            >
              {leadCard}
            </m.div>
          ) : null}
        </AnimatePresence>
        {/* 旧正文与当前正文放在同一个数组里：key 相同的旧正文换到「淡出层」时 React 保留它的 DOM（文字、滚动位置） */}
        {textLayers.map((layer) =>
          layer.gen === snap.textGen ? (
            <StreamView
              key={`text-${layer.gen}`}
              ref={bindStream}
              format={snap.textFormat}
              maxHeight={streamMax}
              className={textClass(layer)}
              data-testid="tv-stream"
            />
          ) : (
            <StreamView
              key={`text-${layer.gen}`}
              format={layer.format}
              maxHeight={streamMax}
              className={cx(textClass(layer), 'tv-stream-out')}
              data-testid="tv-stream-out"
            />
          ),
        )}
        <AnimatePresence initial={false} mode="popLayout">
          {errorKey && snap.error ? (
            <m.div
              key={errorKey}
              ref={cardRef(errorKey)}
              className="tv-card tv-error"
              initial={false}
              animate={fadeIn}
              exit={fadeOut}
            >
              <ErrorCard
                error={snap.error}
                providerName={provider?.name ?? null}
                preferServerMessage={isCompatibleEndpoint(provider)}
                onRetry={retry}
                onOpenSettings={() =>
                  openSettings(snap.error?.code === 'local_runtime' ? 'offline' : 'providers')
                }
                onSwitchModel={openModelMenu}
                onOpenUrl={(url) =>
                  void api
                    .openUrl(url)
                    .catch((e: unknown) => console.warn('[translate] open_url', e))
                }
                data-testid="tv-error"
              />
            </m.div>
          ) : null}
        </AnimatePresence>
      </section>

      {notice ? (
        <m.output className="tv-notice" data-tone={notice.tone} data-testid="tv-notice" {...cardIn}>
          <Icon icon={notice.icon} size={14} className="tv-notice-icon" />
          <span className="tv-notice-text">{notice.text}</span>
          {notice.action ?? null}
        </m.output>
      ) : null}

      {footerVisible ? (
        <div ref={footerRef} className="tv-footer" data-testid="tv-footer">
          <output className="tv-status" aria-live="polite">
            {flashUsed ? (
              <m.div
                className="tv-status-line"
                initial={false}
                animate={{ opacity: flash ? 0 : 1 }}
                transition={crossfade}
                aria-hidden={flash ? true : undefined}
              >
                {statusContent}
              </m.div>
            ) : (
              <div className="tv-status-line">{statusContent}</div>
            )}
            {flashUsed ? (
              <AnimatePresence initial={false}>
                {flash ? (
                  <m.span
                    key={flash.id}
                    className="ui-toast tv-flash"
                    data-tone={flash.tone}
                    data-testid="tv-flash"
                    {...toast}
                  >
                    <Icon icon={flash.tone === 'success' ? CircleCheck : CircleAlert} size={14} />
                    {flash.text}
                  </m.span>
                ) : null}
              </AnimatePresence>
            ) : null}
          </output>
          {footerBusy ? (
            <Button
              size="sm"
              variant="ghost"
              icon={CircleStop}
              onClick={() => run.stop()}
              aria-keyshortcuts={boot.os === 'macos' ? 'Meta+Period' : 'Control+Period'}
              data-testid="tv-stop"
            >
              {t('common.action.stop')}
            </Button>
          ) : (
            <m.div key="actions" className="tv-actions" {...footerIn}>
              {hasResult && showText ? (
                <CopyButton
                  text={resultText}
                  shortcut="Mod+KeyC"
                  onCopied={onCopied}
                  data-testid="tv-copy"
                />
              ) : null}
              {(snap.status === 'cancelled' || isPinned) && !exhausted ? (
                <IconButton
                  icon={RotateCw}
                  label={t('common.action.regenerate')}
                  shortcut="Mod+KeyR"
                  onClick={() => void regenerate()}
                  data-testid="tv-regenerate"
                />
              ) : null}
              {canPin ? (
                <IconButton
                  icon={Pin}
                  label={t('common.action.pin')}
                  shortcut="Mod+KeyP"
                  onClick={() => void pin(false)}
                  data-testid="tv-pin"
                />
              ) : null}
              <IconButton
                icon={ArrowLeftRight}
                label={showSource ? t('common.action.hide_source') : t('common.action.show_source')}
                active={showSource}
                onClick={toggleSource}
                data-testid="tv-source-toggle"
              />
              {canPin ? (
                <IconButton
                  icon={Maximize2}
                  label={t('common.action.expand')}
                  onClick={() => void pin(true)}
                  data-testid="tv-expand"
                />
              ) : null}
            </m.div>
          )}
        </div>
      ) : null}
    </section>
  )
}

// -----------------------------------------------------------------------------
// JIT 预热：面板窗口常驻、启动时隐藏创建。第一次执行翻译视图的代码路径（解析 / 编译 1000 多行的组件与它
// 用到的子组件、第一次匹配 .tv 的样式）实测约 16 ms，之后每次 1.4–2.5 ms——第一次唤出时这段冷启动会落在 prepare 的
// 40 ms 预算里。外壳把窗口标记为隐藏之后，把视图（等待首字的状态：头部、骨架、底栏）在不可见的暂存节点里同步渲染一次、
// 排一次版、立即卸载：不发 IPC（不接上 / 发起请求、不拉取设置）、不登记键盘作用域（视图不活动）、不留任何节点。
// 只在面板窗口里做、整个窗口只做一次；真实的视图已经挂载过时跳过。

/** 真实的翻译视图已经挂载过（不再需要预热） */
let viewMounted = false
let prewarmed = false

const PREWARM_PROPS: ViewProps<'translate'> = {
  sessionId: 0,
  payload: {
    kind: 'translate',
    source_text: 'Wisp',
    request_id: 'prewarm',
    task: 'translate',
    style_id: 'natural',
    detected_lang: 'en',
    target_lang: 'zh-CN',
    dict_mode: false,
    long_text: null,
    not_ready: null,
    selection_method: null,
  },
  phase: 'preparing',
  focused: false,
  host: 'panel',
  frame: boot.frame_mode,
  placement: 'below',
  maxHeight: 480,
  requestHide: () => undefined,
  switchView: () => undefined,
  activate: () => undefined,
}

/** 预热一次（见上） */
export function prewarmOnce(): void {
  if (prewarmed || viewMounted || typeof document === 'undefined' || !document.body) return
  prewarmed = true
  prewarmRender()
}

/** 预热渲染本身：同步渲染、排版、卸载，不留节点（shared/perf/prewarm.tsx） */
export function prewarmRender(): void {
  prewarmRender_(<TranslateView {...PREWARM_PROPS} />, { width: 520, height: 480 })
}

// 外壳把窗口标记为隐藏之后紧接着做（shared/perf/prewarm 的队列：输入 → 翻译 → 剪贴板）
schedulePrewarm(prewarmOnce, PREWARM_ORDER.translate)
