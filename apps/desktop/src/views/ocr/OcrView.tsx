/**
 * OCR 视图。
 *
 * 结构（视觉稿 design.ocr-recognizing / design.ocr-done）：
 * - 头部：缩略图（≤ 72 px 高，卡片上限低时 48 px；点击弹出整张截图）· 截图尺寸（物理像素）· 模式 · 模型 / 识别中… · ✕；
 * - 文本区：骨架（2–3 条宽度递减、1 Hz 脉冲）→ 流式只读（逐段 120 ms 淡入）→ 完成（或取消）后变为可编辑的文本
 *   （逐行 / 段落模式按会话排版显示，见 text/；结构化模式显示 Markdown 源码，不渲染）；`no_text` →「未检测到文字」+
 *   [切换模式 ▾]；
 *   思考耗尽（`truncated = max_tokens` 且没有文字）优先于「未检测到文字」，提示在状态行，空状态给出
 *   下一步 [切换模型 ▾]；
 * - 底栏（第一次完成后淡入，之后重新识别期间保留）：状态行（模型 · 耗时 · tokens；识别中… /
 *   已取消 / 提示 / 已复制）· [排版 | ▾]（逐行 / 段落；结构化在同一位置放「源码 / 预览」）· 复制 · [翻译 | ▾] ·
 *   [重新识别 | ▾] · 固定；没有文字时只留 [重新识别 | ▾] 与固定；
 * - 译文区（[翻译] 展开）：语言条 · 风格 chip（⌘1–9）· 复制；译文流式；状态行从展开起就占位（翻译中… → 结果）；
 *   以（可能编辑过的）OCR 文本为原文，复用翻译管线（translate_start）；切换风格 / 目标语言即重新翻译，旧译文变淡 +
 *   模糊直到新结果到达；
 * - 直接翻译（`payload.mode = translate`）：正文就是译文；语言条与风格 chip 照常；底栏 [原文] 按需对内存中
 *   的截图做一次 paragraphs 转录并缓存（再次切换不发请求；换了视觉模型后重新转录）；切换风格时以转录
 *   为原文重新翻译。
 *
 * 高度：视图以卡片上限为 max-height（固定窗口铺满窗口），两区怎么分由 CSS flex 在同一次布局里完成——内容放不下时
 * 正文区先收缩到 layoutFor().mainFloor（40% / 3 行）、展开区按内容排版，之后才轮到展开区收缩（ocr.css）。所以不会
 * 出现「先缩内容、外框随后才动」的空白；流式中译文每多一行，正文区就在同一次布局里让出一行，底栏只朝一个方向移动。
 * 卡片已到上限时展开区的展开 / 收起用高度弹簧（正文区逐帧让出 / 拿回）；没到上限时卡片 / 窗口的高度跟随负责长高，
 * 内容变矮时下方的块贴着卡片底边一起移动（parts.tsx 的 FlipBox）。
 *
 * 键盘：⌘C 先复制选区，没有选区时译文区存在就复制译文、否则复制 OCR 文本；⌘R 作用于最近一次
 * 交互的区域（OCR 文本 → 重新识别；译文 → 重新翻译）；编辑后再 ⌘R 先确认（会覆盖修改，再按一次 ⌘R 即确认）；
 * ⌘P 固定整个面板；有文字时 ⌘1–9 按该风格翻译（译文区收起时展开它）；⌘L 在「原样」与最近用过的整理方式之间切换排版。
 * Esc 先收起放大的截图与菜单，其余由外壳处理。
 * 面板里 Tab 在视图内循环（最后一个控件之后回到第一个）。
 *
 * 自动复制：由编排器在识别完成时写入剪贴板（转录模式、非直接翻译、设置开启、有文字；commit 与重新识别
 * 都适用，按会话排版处理）；视图在同样的条件下于底栏显示「已复制」约 1.5 s，自己不再写一次。编辑只改视图里的文本，
 * 不会触发复制；文本一被编辑或开始重新识别，「已复制」立即收起（剪贴板里已不是眼前的内容）。切换排版后剪贴板跟随
 * （`ocr_recopy`，写入了才再显示「已复制」）。
 *
 * 固定窗口（PinnedApp，`host = 'pinned'`）：经 `usePinnedHost()` 取 PinState 恢复内容（见 OcrViewProps.pin），
 * 首帧就绪由本视图负责（`usePinnedReadyHold`，接入的流回放完才 `ready()`）；窗口控件用 PinnedApp 的 36 px 标题栏
 * （整条拖拽头部 + 置顶开关 + ✕）。
 */
import './ocr.css'
import {
  ChevronDown,
  BrainCircuit,
  Code,
  Table2,
  Crop,
  Ellipsis,
  History,
  Settings2,
  Languages,
  Pin,
  RotateCw,
  ScanText,
  SlidersHorizontal,
} from 'lucide-react'
import { m } from 'motion/react'
import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from 'react'
import type { ViewProps } from '../../panel/types'
import { usePinnedHost, usePinnedReadyHold } from '../../pinned/context'
import { useAppearance } from '../../shared/hooks/useAppearance'
import { copyText } from '../../shared/hooks/useCopy'
import { usePanelDrag } from '../../shared/hooks/usePanelDrag'
import { useViewActive } from '../../shared/hooks/useViewActive'
import { keepQuoted } from '../../shared/i18n/keepQuoted'
import { useT, useTKey } from '../../shared/i18n/useT'
import {
  api,
  WispIpcError,
  type Entity,
  type Lang,
  type ModelRef,
  type NotReady,
  type OcrMode,
  type OcrReq,
  type PinState,
  type SettingsSection,
  type StreamEvent,
  type Style,
  type TranslateReq,
} from '../../shared/ipc'
import type { SettingsAnchor } from '../../shared/ipc/sections'
import type { KeyBinding } from '../../shared/keys/keyRouter'
import { useKeyScope } from '../../shared/keys/useKeyScope'
import { usePreset } from '../../shared/motion/presets'
import { useReducedMotionPref } from '../../shared/motion/useReducedMotionPref'
import {
  Button,
  Chip,
  CopyButton,
  DropdownMenu,
  EmptyState,
  ErrorCard,
  IconButton,
  InlineConfirm,
  Kbd,
  LangBar,
  NotReadyCard,
  PagedMenu,
  StatusLine,
  StyleChip,
  StyleMenu,
  TextArea,
  estimateMenuHeight,
  useMenuRoom,
  type MenuEntry,
  type MenuPage,
  type StatusLineProps,
  type StyleMenuOption,
  type StyleMenuProps,
  useInlineToast,
} from '../../shared/ui'
import { syncFadeEdges } from '../../shared/ui/ScrollArea'
import {
  OCR_MODELS,
  engineSupports,
  isSlowCloudModel,
  ocrEngineOf,
  offlineIdOf,
  offlineRef,
  type OcrEngine,
} from '../../shared/offline/catalog'
import {
  LAYOUT,
  OCR_MODES,
  copyTarget,
  defaultStyleId,
  hotkeyFor,
  layoutFor,
  lineHeight,
  modelLabel,
  sameModel,
  selectedText,
  settingsSectionFor,
  hasStructure,
  styleList,
  thumbMaxWidth,
  visionModels,
} from './model'
import { useStructuredModule } from './structuredModule'
import { isOfflineNotReady, OfflineCard } from '../../shared/offline/OfflineCard'
import type { ChipMode } from '../../shared/smart/EntityChip'
import { LongTextPrompt } from '../translate/LongTextPrompt'
import { registerOcrMessages } from './i18n'
import {
  isExhausted,
  mainNote,
  NOTE_SPECS,
  translationNote,
  type NoteKind,
  type NoteSpec,
} from './notes'
import { OcrChips } from './OcrChips'
import {
  FlipBox,
  Footer,
  MediaHead,
  RunBody,
  Section,
  SectionFoot,
  SmoothBox,
  Split,
  SplitCaret,
  ZoomImage,
  type FlipHandle,
} from './parts'
import { pinRestoreOf, pinStateOf, type RunSeed, type TranslationBasis } from './pinState'
import { useSettingsSnapshot } from './settings'
import { LayoutSplit, layoutLabel } from './text/LayoutSplit'
import { useOcrText } from './text/useOcrText'
import { useTextSwap } from './text/useTextSwap'
import { isSettled, useRun, warnCode, type Run, type RunState } from './useRun'

export interface OcrViewProps extends ViewProps<'ocr'> {
  /**
   * 固定窗口里的初始内容（kind = ocr 的 PinState）。不传时取 `usePinnedHost()` 的 PinState（PinnedApp 的做法）；面板宿主
   * 两者都没有。OCR 视图产出的 PinState（`panel_pin`）约定：
   * - `ocr_text`：识别完成（可能编辑过）时为当前文本，还在识别时为 null；
   * - `ocr_layers`：OCR 模式下正文的三层（识别文本、会话排版、编辑），固定窗口里可以继续切换排版；还在识别时只带排版；
   * - `show_source`：OCR 模式 = 译文区是否展开**且有可接入的译文请求**；直接翻译 = 「原文」是否展开；
   * - `request_id`：**总是**当前要接上的请求，流结束了也填（编排器据此把截图随固定交接给固定窗口）——
   *   还在识别 → 识别请求；译文区展开且译文请求已发出 → 译文请求；直接翻译 → 译文请求；其余 → 已结束的识别请求
   *   （固定窗口只从它的回放里取模型 / 耗时 / 用量，文字仍用 `ocr_text`，免得回放覆盖编辑过的文本）。译文区展开但
   *   译文请求还没有 id（被拒绝、或还没回 ack）时 `show_source = false`：固定窗口不会把识别请求接到译文区上；
   * - `result_text`：译文（OCR 模式译文区展开时 / 直接翻译），接入失败（日志已释放）时作为退路；
   * - `status` / `model_ref`：正文区（识别；直接翻译时是译文）结构化的状态行与实际使用的模型，
   *   固定窗口不接入已结束的请求就能还原头部的模型名与底栏的状态行；`status_line` 只留给旧前端；
   * - `thumb_url` / `image_size`：截图缩略图与原图尺寸（= 载荷里的同名字段），固定窗口的头部首帧就按实际比例排版。
   */
  pin?: PinState | null
}

type Area = 'ocr' | 'translation'
interface Rerun {
  mode?: OcrMode
  model?: ModelRef | null
}
/** 会丢掉编辑的操作先确认：重新识别（再按一次 ⌘R 也能确认），或排版菜单里的「放弃修改」 */
type Confirm = { rerun: Rerun } | { discard: true }
/** 识别菜单：底栏 ▾（模式页 / 视觉模型页，同一个浮层里换页）或错误卡片 / 空状态的入口（模型页） */
interface MenuState {
  open: boolean
  at: 'footer' | 'error'
  page: 'main' | 'models'
  /** 换页方向（1 进入子页，-1 返回） */
  dir: 1 | -1
  /**
   * 模式页在矮面板里的收起程度（打开时按卡片上限决定，打开期间不变）：0 完整；1 去掉「OCR 设置…」（头部 ⋯ 里也有）；
   * 2 再去掉分组标题；3 再去掉各模式的第二行说明。保证整份菜单完整可见、不用滚动（不会出现最后一项
   * 被截掉一半）
   */
  compact: 0 | 1 | 2 | 3
}
/** 直接翻译等原文转录时，转录没有成功的原因（转交给正文区） */
type SourceFailure =
  { kind: 'no_text'; done: Extract<StreamEvent, { type: 'done' }> | null } | { kind: 'cancelled' }

registerOcrMessages()

const live = (r: Run): boolean => r.state.phase === 'waiting' || r.state.phase === 'streaming'
const settled = (r: Run): boolean => isSettled(r.state)
/** 带右侧说明（模式说明、服务名）的菜单宽度 */
const MENU_WIDTH = 264
const modelKey = (ref: ModelRef | null): string => (ref ? `${ref.provider_id}/${ref.model_id}` : '')
/** 状态行的耗时：命中缓存时是缓存查找的耗时、不是这次识别 / 翻译的耗时，不显示（改为注明「来自缓存」） */
const elapsedOf = (done: RunState['done']): number | null =>
  done && !done.cached ? done.elapsed_ms : null
const isSourceFailure = (v: unknown): v is SourceFailure =>
  typeof v === 'object' && v !== null && 'kind' in v && !('code' in v)

export function OcrView(props: OcrViewProps) {
  const { payload, sessionId, host, maxHeight, requestHide, activate } = props
  // 面板宿主：从头部（缩略图旁）、状态行与卡片空白处拖动（shared/hooks/usePanelDrag）；识别结果的文本框、按钮、菜单
  // 不触发。固定窗口由 PinnedApp 的头部拖动
  const panelDrag = usePanelDrag(sessionId, host === 'panel')
  const t = useT()
  const tKey = useTKey()
  const snap = useSettingsSnapshot()
  const { fontSize, os } = useAppearance()
  const footerIn = usePreset('footerIn')
  const reducedMotion = useReducedMotionPref()
  const active = useViewActive()
  const direct = payload.mode === 'translate'
  const pinnedHost = usePinnedHost()
  // 固定窗口：首帧就绪由本视图负责（接入的流要等回放完）
  usePinnedReadyHold()
  const pinned: PinState | null =
    host === 'pinned'
      ? (props.pin ?? (pinnedHost?.state.kind === 'ocr' ? pinnedHost.state : null))
      : null
  // 固定窗口的载荷由 PinState 还原（PinnedApp 的 payloadOf：缩略图与尺寸随面板交接）
  const shown = payload

  /** OCR 文本（直接翻译时是译文） */
  const main = useRun()
  /** 译文区（OCR 模式） */
  const tr = useRun()
  /** 原文转录（直接翻译的「原文」，以及直接翻译时切换风格所需的原文） */
  const src = useRun()

  const [mode, setMode] = useState<OcrMode>(pinned?.ocr_mode ?? payload.mode)
  const [model, setModel] = useState<ModelRef | null>(null)
  /**
   * 正文的三层（识别文本 → 排版 → 编辑，见 text/model.ts）：排版只在逐行 / 段落模式下作用，结构化与直接翻译不出现
   */
  const layoutOn = !direct && (mode === 'lines' || mode === 'paragraphs')
  const content = useOcrText(main, {
    initial: payload.layout,
    enabled: layoutOn,
    follow: followClipboard,
  })
  const [confirm, setConfirm] = useState<Confirm | null>(null)
  const [trOpen, setTrOpen] = useState(false)
  const [srcOpen, setSrcOpen] = useState(false)
  const [styleId, setStyleId] = useState<string | null>(pinned?.style_id ?? null)
  const [target, setTarget] = useState<Lang | null>(null)
  /** 重新识别 / 直接翻译换风格时，正文区在新结果到达前继续显示的旧文字 */
  const [mainGhost, setMainGhost] = useState<string | null>(null)
  /** 译文区切换风格 / 目标语言时，新结果到达前继续显示的旧译文 */
  const [trGhost, setTrGhost] = useState<string | null>(null)
  const basis = useRef<TranslationBasis | null>(null)
  const [zoom, setZoom] = useState(false)
  const [menu, setMenu] = useState<MenuState>({
    open: false,
    at: 'footer',
    page: 'main',
    dir: 1,
    compact: 0,
  })
  /**
   * 载荷里的未就绪已经解决（离线模型在卡片上下载完成、这张截图还在）：视图转为正常识别。只在面板里发生
   */
  const [readyNow, setReadyNow] = useState(false)
  /** 结构化结果有表格 / 公式时默认显示预览；切到源码后才能编辑（本次会话内记住选择） */
  const [showSource, setShowSource] = useState(false)
  const blocked = payload.not_ready !== null && !readyNow
  /** 第一次有结果之后底栏一直保留（重新识别期间也在，按钮按状态置灰） */
  const [footerSeen, setFooterSeen] = useState(false)
  const errorRef = useRef<HTMLDivElement>(null)
  const recognizeRef = useRef<HTMLSpanElement>(null)
  const recognizeCaret = useRef<HTMLButtonElement>(null)
  const emptyAction = useRef<HTMLButtonElement>(null)
  const mediaRef = useRef<HTMLDivElement>(null)
  const area = useRef<Area>('ocr')

  // 底栏的「已复制」：内容一变（编辑、重新识别）就立即收起，剪贴板里已不是眼前的内容
  const toast = useInlineToast()
  /** 底栏提示复制的结果（已复制 / 复制失败） */
  const flashCopied = (ok: boolean) =>
    toast.show(t(ok ? 'common.state.copied' : 'common.state.copy_failed'))

  const styles = styleList(snap)
  const effStyle = styleId ?? defaultStyleId(snap)
  const models = visionModels(snap)
  /** 本机选用的离线识别模型（优先于默认 OCR 模型） */
  const offlineOcr = snap?.settings.offline?.ocr_model ?? null
  const currentModel: ModelRef | null =
    model ??
    (main.state.start
      ? { provider_id: main.state.start.provider_id, model_id: main.state.start.model_id }
      : offlineOcr
        ? offlineRef(offlineOcr)
        : (snap?.settings.defaults.ocr ?? null))
  /** 下一次识别会用到的引擎（决定哪些模式可用：PP-OCRv6 没有「结构化」） */
  const engineFor = (ref: ModelRef | null): OcrEngine =>
    ocrEngineOf(
      ref,
      offlineOcr,
      snap?.settings.defaults.ocr ?? null,
      snap?.settings.providers ?? [],
    )
  const engine = engineFor(model)
  /** 显示的正文：编辑过的以编辑后的为准，否则是排版后的；复制、芯片粘贴、翻译、固定都取它 */
  const ocrText = content.display
  const edited = content.edited
  /** 译文依据里的编辑：编辑后又改回与排版结果一样的不算 */
  const editKey = edited ? content.edit : null

  // ---- 请求 ----

  const ocrReq = (ocrMode: OcrMode, mdl: ModelRef | null, tl: Lang | null): OcrReq => ({
    session_id: sessionId,
    capture_id: payload.capture_id,
    mode: ocrMode,
    model_ref: mdl,
    target_lang: tl,
    // 识别完成时 Rust 按它自动复制、写入识别历史，与面板里显示的排版一致（直接翻译不排版）
    ...(direct ? null : { layout: content.session }),
  })
  const trReq = (
    text: string,
    style: string,
    tl: Lang | null,
    bypass: boolean,
    confirmLong: boolean,
  ): TranslateReq => ({
    session_id: sessionId,
    text,
    task: 'translate',
    style_id: style,
    target_lang: tl,
    model_ref: null,
    dict_mode: 'auto',
    bypass_cache: bypass,
    draft: null,
    confirm_long: confirmLong,
  })
  /**
   * 用户已经确认过的长原文：同一段文字之后换风格 / 改目标 /
   * ⌘R 不再确认；文字改了（编辑、重新识别）就要重新确认
   */
  const confirmedLong = useRef<string | null>(null)

  // 进入后只启动一次（不以 phase 为依赖；StrictMode 的重复挂载由 started 挡住）
  const started = useRef(false)
  /** 固定窗口：内容就绪后的那次提交里通知 PinnedApp 测量并 pinned_ready */
  const readyPending = useRef(false)
  const [, bump] = useState(0)
  useEffect(() => {
    if (started.current) return
    started.current = true
    if (pinned) {
      void seedFromPin(pinned).then(() => {
        readyPending.current = true
        // 没有任何状态变化（空回放）时也要有一次提交来触发下面的布局 effect
        bump((n) => n + 1)
      })
      return
    }
    if (payload.not_ready) return
    const req = ocrReq(mode, null, direct ? payload.target_lang : null)
    const id = payload.request_id
    // 放进紧随其后的微任务：仍早于任何计时器 / 帧，但不落在 prepare 的同步路径（flushSync → 测量 → panel_ready）里，
    // 免得整个视图在 ready 之前多渲染一遍（与翻译视图同一做法）
    queueMicrotask(() => {
      if (!id) return void main.begin((cb) => api.ocrStart(req, cb))
      // 只有 not_found（请求被取代 / 会话已结束）才用同一张截图自己发起；其他失败（参数、内部错误、
      // 传输层的瞬时失败）原请求可能仍在进行，进错误态显示卡片，不重复计费
      void main.begin(
        (cb) =>
          api.streamAttach(id, cb).catch((e: unknown) => {
            if (WispIpcError.from('stream_attach', e).code === 'not_found')
              return api.ocrStart(req, cb)
            throw e
          }),
        id,
      )
    })
    // oxlint-disable-next-line react/exhaustive-deps -- 只在挂载时启动一次
  }, [])
  useLayoutEffect(() => {
    if (!readyPending.current || !pinnedHost) return
    readyPending.current = false
    pinnedHost.ready()
  })

  /** 从 PinState 恢复（怎么还原见 pinState.ts）；resolve 时内容已经写入（接入的流已回放） */
  async function seedFromPin(p: PinState): Promise<void> {
    const plan = pinRestoreOf(p, direct)
    if (plan.source) {
      if (plan.source.text !== null) {
        src.seed(plan.source.text)
        // 记作默认模型（model = null）的转录：展开「原文」时不再重新请求
        srcFor.current = ''
      }
      setSrcOpen(plan.source.open)
    }
    // 识别已结束：识别文本写入正文区，排版与编辑绑到这次结果上
    const restored = plan.text
      ? content.restore(main.seed(plan.main.text, plan.main.extra, plan.main.phase), plan.text)
      : seedRun(main, plan.main)
    if (!plan.translation) return restored
    setTrOpen(true)
    basis.current = plan.translation.basis
    return seedRun(tr, plan.translation)
  }

  /** 写入一个区：有请求就接入，日志已释放（not_found）时退回 PinState 里的文字 */
  function seedRun(run: Run, seed: RunSeed): Promise<void> | void {
    const id = seed.attach
    if (id === null) {
      run.seed(seed.text, seed.extra, seed.phase)
      return
    }
    return run.begin(
      (cb) =>
        api.streamAttach(id, cb).catch(() => {
          run.seed(seed.text, seed.extra, seed.phase)
        }),
      id,
    )
  }

  /** 识别（重新识别、换模式 / 模型）；编辑过的文本会被覆盖时先确认 */
  function recognize(next: Rerun = {}, force = false): void {
    if (blocked && !pinned) return
    if (!force && edited && !direct) {
      setConfirm({ rerun: next })
      return
    }
    setConfirm(null)
    toast.hide()
    const nextMode = next.mode ?? mode
    const mdl = next.model !== undefined ? next.model : model
    // 旧文字变淡留在原处（叠加骨架同节奏的脉冲），新结果到达时替换，文本区与底栏不塌陷
    const previous = direct ? main.text() : ocrText
    setMainGhost(settled(main) && previous.trim() !== '' ? previous : null)
    setMode(nextMode)
    setModel(mdl)
    area.current = 'ocr'
    if (direct) {
      if (styleId !== null) return directTranslate(styleId, target, true, mdl)
      // 换了视觉模型：旧模型的原文转录作废（展开着就用新模型重新转录，否则作废——clear 会取消仍在进行的转录，
      // 下次打开「原文」时再请求）
      if (modelKey(mdl) !== srcFor.current && srcFor.current !== null) {
        if (srcOpen) transcribe(mdl)
        else {
          src.clear()
          srcFor.current = null
        }
      }
      main.begin((cb) => api.ocrStart(ocrReq('translate', mdl, target ?? payload.target_lang), cb))
      return
    }
    main.begin((cb) => api.ocrStart(ocrReq(nextMode, mdl, null), cb))
  }

  /** 翻译当前（可能编辑过的）OCR 文本 */
  function translate(
    opts: { style?: string; target?: Lang | null; bypass?: boolean; confirm?: boolean } = {},
  ): void {
    const text = ocrText
    if (text.trim() === '') return
    const style = opts.style ?? effStyle
    const tl = opts.target !== undefined ? opts.target : target
    if (opts.confirm) confirmedLong.current = text
    setTrGhost(tr.state.phase === 'idle' || tr.state.phase === 'confirm' ? null : tr.text() || null)
    basis.current = { raw: main.text(), edit: editKey, style, target: tl }
    area.current = 'translation'
    const confirmLong = confirmedLong.current === text
    tr.begin((cb) =>
      api.translateStart(trReq(text, style, tl, opts.bypass ?? false, confirmLong), cb),
    )
  }

  function toggleTranslate(): void {
    if (trOpen) {
      tr.cancel()
      setTrOpen(false)
      return
    }
    setTrOpen(true)
    const b = basis.current
    const fresh = sameSource(b) && b.style === effStyle && b.target === target
    if (fresh && tr.state.phase === 'done') return
    // 原文 / 风格变过：重新翻译，上一次的译文变淡留在原处直到新结果到达（与换风格同一表现，区的高度不先塌成骨架）
    translate()
  }

  // ---- 直接翻译：原文转录（按需、缓存；换了视觉模型后作废）----

  /** 当前转录（进行中或已完成）用的视觉模型；null = 还没有转录 */
  const srcFor = useRef<string | null>(null)
  const waiters = useRef<Array<{ ok: (text: string) => void; fail: (e: unknown) => void }>>([])
  function transcribe(mdl: ModelRef | null): void {
    srcFor.current = modelKey(mdl)
    void src.begin((cb) => api.ocrStart(ocrReq('paragraphs', mdl, null), cb))
  }
  function ensureSource(mdl: ModelRef | null): Promise<string> {
    const fresh = srcFor.current === modelKey(mdl)
    if (fresh && src.state.phase === 'done' && src.state.hasText) return Promise.resolve(src.text())
    if (!fresh || !live(src)) transcribe(mdl)
    return new Promise((ok, fail) => waiters.current.push({ ok, fail }))
  }
  // 转录结束时结算等待者：失败原样交出 LlmError（no_vision 卡片的 [选择模型] 才会出现）；没有文字转交「未检测到文字」
  useEffect(() => {
    const { phase, error, done } = src.state
    if (phase !== 'done' && phase !== 'error' && phase !== 'cancelled') return
    const text = src.text()
    for (const w of waiters.current.splice(0)) {
      if (phase === 'done' && text.trim() !== '') w.ok(text)
      else if (phase === 'done') w.fail({ kind: 'no_text', done } satisfies SourceFailure)
      else if (phase === 'error' && error) w.fail(error)
      else w.fail({ kind: 'cancelled' } satisfies SourceFailure)
    }
    // oxlint-disable-next-line react/exhaustive-deps -- 每次转录结束结算一次
  }, [src.state.key, src.state.phase])

  function directTranslate(
    style: string,
    tl: Lang | null,
    bypass: boolean,
    mdl: ModelRef | null,
  ): void {
    main.begin(async (cb, current) => {
      let text: string
      try {
        text = await ensureSource(mdl)
      } catch (why: unknown) {
        if (!current()) return
        if (!isSourceFailure(why)) throw why
        // 转录没有文字：正文区进入「未检测到文字」（不合成 internal 错误）；转录被取消：正文区同样是已取消
        cb(
          why.kind === 'no_text'
            ? {
                type: 'done',
                elapsed_ms: why.done?.elapsed_ms ?? 0,
                usage: why.done?.usage ?? null,
                truncated: why.done?.truncated ?? null,
                cached: false,
                no_text: true,
              }
            : {
                type: 'error',
                code: 'cancelled',
                subcode: null,
                message: 'source transcription cancelled',
                retryable: true,
                hint: null,
                status: null,
                url: null,
                retry_after_s: null,
                upstream_request_id: null,
              },
        )
        return
      }
      // 等转录期间被新的请求取代：不再发出注定被立即取消的 translate_start
      if (!current()) return
      // 直接翻译换风格：用户要的是这张截图的译文（默认的一次调用本来就不经过长文本确认），不再拦一次
      return api.translateStart(trReq(text, style, tl, bypass, true), cb)
    })
  }

  const canTranslate = !direct && settled(main) && ocrText.trim() !== ''

  function pickStyle(id: string): void {
    if (direct) {
      setStyleId(id)
      setMainGhost(main.text() || null)
      directTranslate(id, target, false, model)
      return
    }
    if (!canTranslate) return
    setStyleId(id)
    setTrOpen(true)
    translate({ style: id })
  }

  function pickTarget(lang: Lang): void {
    setTarget(lang)
    if (!direct) {
      // 重新识别期间（正文还在流式）只记下目标语言：识别完成后由下面的 effect 以新文本、新语言重新翻译——不能拿半截
      // 识别结果发一次计费的翻译
      if (canTranslate) translate({ target: lang })
      return
    }
    setMainGhost(main.text() || null)
    if (styleId === null) main.begin((cb) => api.ocrStart(ocrReq('translate', model, lang), cb))
    else directTranslate(styleId, lang, false, model)
  }

  function toggleSource(): void {
    const open = !srcOpen
    setSrcOpen(open)
    if (!open) return
    const fresh = srcFor.current === modelKey(model)
    if (!fresh || !(live(src) || (src.state.phase === 'done' && src.state.hasText))) {
      transcribe(model)
    }
  }

  /** 译文依据的原文与眼前的一致：识别文本与编辑都没变（切换排版不算原文改变） */
  function sameSource(b: TranslationBasis | null): b is TranslationBasis {
    return b !== null && b.raw === main.text() && b.edit === editKey
  }

  // OCR 重新识别完成后，展开着的译文区跟着重新翻译（文本或目标语言变了）；要排版时等排版结果到了再翻译排版后的文字
  useEffect(() => {
    if (direct || !trOpen || main.state.phase !== 'done' || !main.state.hasText) return
    if (content.pending) return
    if (sameSource(basis.current) && basis.current?.target === target) return
    translate()
    // oxlint-disable-next-line react/exhaustive-deps -- 每次识别结束（排版结果到达）检查一次
  }, [main.state.key, main.state.phase, content.pending])

  // 第一次有结果之后保留底栏
  useEffect(() => {
    if (settled(main) && !footerSeen) setFooterSeen(true)
    // oxlint-disable-next-line react/exhaustive-deps -- 只看阶段
  }, [main.state.phase])

  // ---- 自动复制的提示：编排器已在识别完成时写入剪贴板，这里按同样的条件提示「已复制」----
  const autoCopied = useRef(0)
  useEffect(() => {
    const s = main.state
    // 只对本视图里真正跑完的识别（有 Done）提示；从 PinState 恢复的内容没有 Done
    if (direct || pinned || s.phase !== 'done' || !s.done || autoCopied.current === s.key || !snap)
      return
    autoCopied.current = s.key
    if (snap.settings.ocr.auto_copy && !s.done.no_text && s.hasText) flashCopied(true)
    // oxlint-disable-next-line react/exhaustive-deps -- 每次识别结束（设置就绪后）判断一次
  }, [main.state.key, main.state.phase, snap])

  function onEdit(value: string): void {
    // 剪贴板里是识别结果、不是编辑后的文本：「已复制」立即收起
    toast.hide()
    content.setEdit(value)
  }

  /**
   * 切换排版、新的文字就绪之后让剪贴板跟上：自动复制开着、在面板里（固定窗口不再带动剪贴板）、不是直接翻译（编辑过的
   * 文字不能切换排版）、识别是完整跑完的（被取消的半截文字不写进剪贴板，也不能替换掉之前那次完整结果的历史条目）；
   * Rust 再核对这张截图自动复制之后剪贴板没有被别处改过，并替换掉那次复制新建的历史条目。写入了就再显示「已复制」，
   * 否则收起（剪贴板里已不是眼前的内容）；等写入期间这次切换已不作数（`still`）时什么也不提示
   */
  function followClipboard(next: string, still: () => boolean): Promise<boolean> {
    const follow =
      host === 'panel' &&
      !direct &&
      main.state.phase === 'done' &&
      snap?.settings.ocr.auto_copy === true
    const written = follow
      ? api.ocrRecopy(payload.capture_id, next).catch((error: unknown) => {
          warnCode('ocr_recopy')(error)
          return false
        })
      : Promise.resolve(false)
    return written.then((ok) => {
      if (!still()) return false
      if (ok) flashCopied(true)
      else toast.hide()
      return ok
    })
  }

  // ---- 复制 / 固定 / 设置 ----

  /**
   * 芯片：单击把片段粘贴到原来的应用（与剪贴板的芯片同一条粘贴管线，片段不进历史）；⌥ 单击只复制、在底栏提示。
   * 粘贴不成时（例如没有可以粘贴的目标）退回复制
   */
  function pasteFragment(entity: Entity, how: ChipMode): void {
    const copy = () => void copyText(entity.value).then(flashCopied)
    if (how === 'copy_only') {
      copy()
      return
    }
    api.clipboardPasteText(entity.value, null, 'normal').catch((error: unknown) => {
      if (WispIpcError.from('clipboard_paste_text', error).code !== 'conflict') copy()
    })
  }

  function copyByKey(): boolean {
    const translation = direct ? main.text() : trOpen && tr.state.hasText ? tr.text() : null
    const text = copyTarget(selectedText(), translation, direct ? main.text() : ocrText)
    if (text.trim() === '') return false
    void copyText(text).then(flashCopied)
    return true
  }

  function pin(): void {
    if (host !== 'panel' || blocked) return
    const state = pinStateOf({
      direct,
      payload,
      main,
      tr,
      src,
      mode,
      ocrText: content.pending ? main.text() : ocrText,
      layers: direct ? null : content.layers(),
      style: effStyle,
      target,
      trOpen,
      srcOpen,
    })
    void api.panelPin(sessionId, state, false).catch(warnCode('panel_pin'))
  }

  function openSettings(section: SettingsSection, anchor: SettingsAnchor | null = null): void {
    void api
      .windowOpen(anchor ? { kind: 'settings', section, anchor } : { kind: 'settings', section })
      .catch(warnCode('window_open'))
    if (host === 'panel') requestHide('other')
  }

  /** 确认条上的确认（或再按一次 ⌘R 确认重新识别） */
  function confirmNow(): void {
    if (!confirm) return
    if ('rerun' in confirm) return recognize(confirm.rerun, true)
    setConfirm(null)
    content.discard()
  }

  function regenerate(): void {
    if (confirm && 'rerun' in confirm) return confirmNow()
    if (!direct && area.current === 'translation' && trOpen) return translate({ bypass: true })
    recognize()
  }

  // ---- 菜单（识别模式 / 视觉模型；同一个浮层里换页）----

  /** 识别菜单的锚点上下能放下的最大高度（卡片上限以内） */
  const footerRoom = useMenuRoom(recognizeRef)
  /** 模式页放得下的收起程度（见 MenuState.compact）；从错误卡片打开的只有模型页，不收起 */
  function fitCompact(at: MenuState['at']): MenuState['compact'] {
    if (at !== 'footer') return 0
    const room = footerRoom()
    for (const level of [0, 1, 2, 3] as const) {
      if (estimateMenuHeight(mainItems(level)) <= room) return level
    }
    return 3
  }

  const openMenu = (at: MenuState['at'], page: MenuState['page'], dir: 1 | -1 = 1) =>
    setMenu((cur) => ({
      open: true,
      at,
      page,
      dir,
      // 换页（菜单已开着）沿用打开时定下的收起程度；新打开时按卡片上限重新决定
      compact: cur.open && cur.at === at ? cur.compact : fitCompact(at),
    }))
  const closeMenu = () => setMenu((cur) => ({ ...cur, open: false }))

  // ---- 键盘 ----
  const bindings: KeyBinding[] = [
    { combo: 'Mod+KeyC', allowInInput: true, run: () => copyByKey() },
    { combo: 'Mod+KeyR', allowInInput: true, run: () => regenerate() },
    { combo: 'Mod+KeyP', allowInInput: true, when: () => host === 'panel', run: pin },
    // 排版控件在底栏时：⌘L 等同点左半按钮（不能切换时什么也不做）
    { combo: 'Mod+KeyL', allowInInput: true, when: () => layoutShown, run: () => content.toggle() },
  ]
  styles.slice(0, 9).forEach((st, i) => {
    bindings.push({
      combo: `Mod+Digit${i + 1}`,
      allowInInput: true,
      // 有文字就能用（译文区收起时展开它），与风格菜单里标的快捷键一致
      when: () => direct || canTranslate,
      run: () => pickStyle(st.id),
    })
  })
  useKeyScope('ocr', bindings, { enabled: !blocked || pinned !== null })

  // ---- 布局（flex，见 ocr.css；这里只给出上限与下限，并在内容变矮时让下方的块贴着卡片底边）----

  const lh = lineHeight(fontSize)
  const layout = layoutFor({ maxHeight, fontSize, direct })
  const sectionOpen = direct ? srcOpen : trOpen
  const viewRef = useRef<HTMLElement>(null)
  const topRef = useRef<HTMLDivElement>(null)
  const mainBox = useRef<HTMLDivElement>(null)
  const sectionBox = useRef<HTMLDivElement>(null)
  const tailRef = useRef<FlipHandle>(null)
  const footRef = useRef<FlipHandle>(null)
  /** 头部的内容宽度（缩略图的宽度上限按它算） */
  const [topWidth, setTopWidth] = useState(0)
  /** 正文区内容的自然高度（正文区在两区都放不下时最少保留 min(它, mainFloor)） */
  const [mainContent, setMainContent] = useState(0)
  /** 视图已到高度上限（卡片不能再长高）：展开区的展开 / 收起改用高度过渡，骨架只占一行 */
  const [atMax, setAtMax] = useState(host === 'pinned')
  const sizes = useRef<{ view: number; main: number; section: number } | null>(null)
  const viewMaxRef = useRef(layout.viewMax)
  viewMaxRef.current = layout.viewMax

  /**
   * 是否已到高度上限。回落要低于上限一行以上才算（滞后）：骨架在到上限时只占一行，否则两行——没有滞后的话，
   * 恰好卡在边界上时会来回切换
   */
  const reachesMax = (view: number, was: boolean): boolean =>
    host === 'pinned' ||
    view >= viewMaxRef.current - 1 ||
    (was && view >= viewMaxRef.current - lineHeight(fontSize) - 2)
  const measure = () => ({
    view: viewRef.current?.offsetHeight ?? 0,
    main: mainBox.current?.offsetHeight ?? 0,
    section: sectionBox.current?.offsetHeight ?? 0,
  })
  const mainNatural = (): number => {
    const el = mainBox.current?.querySelector<HTMLElement>('textarea, .ui-stream, .ocr-preview')
    return el ? el.scrollHeight : 0
  }
  // 每次提交之后（布局刚算过）：内容变矮、卡片随后收缩时，让下方的块贴着卡片底边；记下是否已到上限、正文的自然高度。
  // 暂存层里（视图切换还没显示）不读布局，显示的那次提交再量。state 只在值变化时更新（不会形成更新链）
  // oxlint-disable-next-line react-hooks/exhaustive-deps -- 每次提交都要核对布局
  useLayoutEffect(() => {
    if (!active) return
    const next = measure()
    const prev = sizes.current
    sizes.current = next
    const max = reachesMax(next.view, atMax)
    if (max !== atMax) setAtMax(max)
    const natural = mainNatural()
    if (Math.abs(natural - mainContent) >= 2) setMainContent(natural)
    if (topRef.current && topRef.current.offsetWidth !== topWidth) {
      setTopWidth(topRef.current.offsetWidth)
    }
    if (!prev || host !== 'panel') return
    const shrink = prev.view - next.view
    if (shrink < 1) return
    if (prev.main - next.main >= 1) tailRef.current?.nudge(Math.min(shrink, prev.main - next.main))
    else if (next.section > 0 && prev.section - next.section >= 1) {
      footRef.current?.nudge(Math.min(shrink, prev.section - next.section))
    }
  })
  // 布局在提交之外变化（流式文字增长、窗口 / 卡片宽度变化）：回调里只读尺寸；需要重渲染的放到下一帧，
  // 不在回调里改被观察元素的尺寸（免得触发「ResizeObserver loop」）
  useEffect(() => {
    const view = viewRef.current
    if (!view || typeof ResizeObserver === 'undefined') return
    let frame = 0
    const ro = new ResizeObserver(() => {
      sizes.current = measure()
      frame ||= requestAnimationFrame(() => {
        frame = 0
        const s = sizes.current
        if (!s) return
        setAtMax((cur) => reachesMax(s.view, cur))
        const natural = mainNatural()
        setMainContent((cur) => (Math.abs(cur - natural) >= 2 ? natural : cur))
        const w = topRef.current?.offsetWidth ?? 0
        setTopWidth((cur) => (cur === w ? cur : w))
      })
    })
    ro.observe(view)
    if (mainBox.current) ro.observe(mainBox.current)
    const stream = mainBox.current?.querySelector('.ui-stream-body')
    if (stream) ro.observe(stream)
    if (sectionBox.current) ro.observe(sectionBox.current)
    return () => {
      ro.disconnect()
      if (frame) cancelAnimationFrame(frame)
    }
    // oxlint-disable-next-line react/exhaustive-deps -- 观察的元素随阶段 / 展开区变化
  }, [main.state.key, main.state.phase, sectionOpen, host])

  // OCR 文本框：可滚动方向的边缘渐隐（与 StreamView 一致）
  const textRef = useRef<HTMLTextAreaElement>(null)
  /** 结构化结果的预览（与文本框同一个位置、同样的边缘渐隐） */
  const previewRef = useRef<HTMLDivElement>(null)
  useLayoutEffect(() => {
    if (textRef.current && active) syncFadeEdges(textRef.current)
    if (previewRef.current && active) syncFadeEdges(previewRef.current)
  })
  const trackScroll = useTextSwap(
    textRef,
    content.base,
    content.edit !== null,
    content.turned,
    reducedMotion,
  )
  const editableNow = !direct && settled(main)
  useEffect(() => {
    const el = textRef.current
    if (!el || typeof ResizeObserver === 'undefined') return
    const ro = new ResizeObserver(() => syncFadeEdges(el))
    ro.observe(el)
    return () => ro.disconnect()
  }, [editableNow, main.state.key])
  // 流式结束、只读视图换成可编辑文本时：放不下的长文本先停在末尾（与流式中的自动跟随衔接，不跳），识别完成（done）后
  // 再回到开头——与翻译视图、固定窗口、OCR 的译文区同一种行为（停在末尾的话，开头被顶部渐隐盖住）；
  // 取消时停在末尾（看得到停在了哪里）；放得下时不动
  const wasStreaming = useRef(false)
  useLayoutEffect(() => {
    const el = textRef.current
    const phase = main.state.phase
    if (el && wasStreaming.current && el.scrollHeight > el.clientHeight + 1) {
      el.scrollTop = el.scrollHeight
      syncFadeEdges(el)
      if (phase === 'done') {
        const behavior = reducedMotion ? 'auto' : 'smooth'
        requestAnimationFrame(() => el.scrollTo({ top: 0, behavior }))
      }
    }
    wasStreaming.current = phase === 'streaming'
    // oxlint-disable-next-line react/exhaustive-deps -- 只在阶段变化时执行
  }, [main.state.phase])

  // ---- 菜单项 ----

  const styleName = (st: Style | undefined, id: string): string =>
    st && !st.builtin ? st.name || t('styles.custom') : tKey(`styles.${id}`, undefined, id)
  const currentStyleName = styleName(
    styles.find((x) => x.id === effStyle),
    effStyle,
  )
  // 风格菜单（组件库的 StyleMenu，与翻译视图同一份：分组标题、⌘1–9、数字直选、「编辑当前风格」）
  const styleOptions: StyleMenuOption[] = styles.length
    ? styles.map((st) => ({ id: st.id, name: styleName(st, st.id) }))
    : [{ id: effStyle, name: styleName(undefined, effStyle) }]
  const styleMenu = (
    testId: string,
    renderTrigger: StyleMenuProps['renderTrigger'],
    side: StyleMenuProps['side'] = 'bottom',
  ) => (
    <StyleMenu
      styles={styleOptions}
      current={effStyle}
      onPick={pickStyle}
      onEdit={() => openSettings('translate')}
      label={styleMenuLabel}
      side={side}
      align="end"
      renderTrigger={renderTrigger}
      data-testid={testId}
    />
  )
  // 识别模型菜单：已下载的离线模型、文字识别服务（PaddleOCR）、各服务的视觉模型。只有一类时不加分组标题（与原来一样
  // 是一列模型，服务名在右侧）
  const installedOffline = OCR_MODELS.filter((id) => snap?.runtime.offline?.installed.includes(id))
  const services = models.filter((v) => v.service)
  const llms = models.filter((v) => !v.service)
  const grouped =
    [installedOffline.length > 0, services.length > 0, llms.length > 0].filter(Boolean).length > 1
  /** 选一个模型重新识别：新模型不支持当前的「结构化」时改用段落（与 Rust 的兜底一致，界面不留一个置灰的选中项） */
  const pickModel = (ref: ModelRef) =>
    recognize(
      mode === 'markdown' && !direct && !engineSupports(engineFor(ref), 'markdown')
        ? { model: ref, mode: 'paragraphs' }
        : { model: ref },
    )
  const llmItem = (v: (typeof models)[number]): MenuEntry => ({
    id: `model-${v.ref.provider_id}/${v.ref.model_id}`,
    label: modelLabel(v.model),
    hint: v.unconfirmed
      ? t('ocr.menu.unknown_vision')
      : grouped && v.service
        ? undefined
        : v.provider.name,
    checked: sameModel(v.ref, currentModel),
    onSelect: () => pickModel(v.ref),
  })
  const modelItems: MenuEntry[] =
    models.length || installedOffline.length
      ? [
          ...(installedOffline.length > 0 && grouped
            ? [{ type: 'label' as const, id: 'group-offline', label: t('ocr.menu.offline') }]
            : []),
          ...installedOffline.map((id): MenuEntry => ({
            id: `model-offline/${id}`,
            label: t(`common.offline_model.${id}`),
            checked: sameModel(offlineRef(id), currentModel),
            onSelect: () => pickModel(offlineRef(id)),
          })),
          ...services.flatMap((v, i): MenuEntry[] => [
            ...(grouped && (i === 0 || services[i - 1]?.provider.id !== v.provider.id)
              ? [
                  ...(i > 0 || installedOffline.length > 0
                    ? [{ type: 'separator' as const, id: `sep-${v.provider.id}` }]
                    : []),
                  { type: 'label' as const, id: `group-${v.provider.id}`, label: v.provider.name },
                ]
              : []),
            llmItem(v),
          ]),
          ...(grouped && llms.length > 0
            ? [
                { type: 'separator' as const, id: 'sep-llm' },
                { type: 'label' as const, id: 'group-llm', label: t('ocr.menu.cloud') },
              ]
            : []),
          ...llms.map(llmItem),
        ]
      : [{ id: 'no-models', label: t('ocr.menu.no_models'), disabled: true }]
  /** 当前模型的显示名（离线模型用它的本地化名称） */
  const currentLocal = offlineIdOf(currentModel)
  const currentModelName: string | null = currentLocal
    ? t(`common.offline_model.${currentLocal}`)
    : (() => {
        const hit = models.find((v) => sameModel(v.ref, currentModel))?.model
        return hit ? modelLabel(hit) : null
      })()
  const styleMenuLabel = t('ocr.a11y.style_menu')
  // 模式页：三种模式 +「视觉模型 ›」（同一个浮层里换成模型页）+「OCR 设置…」；矮面板里按 compact 收起低频项
  const mainItems = (compact: MenuState['compact']): MenuEntry[] => [
    ...(direct
      ? []
      : [
          ...(compact < 2
            ? [{ type: 'label' as const, id: 'modes', label: t('ocr.menu.modes') }]
            : []),
          ...OCR_MODES.map((each) => {
            // 当前引擎不支持的模式（PP-OCRv6 与云端 PP-OCRv6 没有「结构化」）：置灰，第二行说明原因
            const supported = engineSupports(engine, each)
            return {
              id: `mode-${each}`,
              label: t(`common.ocr_mode.${each}`),
              // 说明放第二行（可以折行）：右侧的提示列放不下整句时会被截成省略号（Chromium / WebView2）
              description: !supported
                ? t('ocr.menu.mode_unsupported', { model: currentModelName ?? '' })
                : compact < 3
                  ? t(`ocr.mode_hint.${each}`)
                  : undefined,
              disabled: !supported,
              checked: each === mode,
              onSelect: () => recognize({ mode: each }),
            }
          }),
          { type: 'separator' as const, id: 'sep-modes' },
        ]),
    {
      id: 'models',
      label: t('ocr.menu.models'),
      hint: currentModelName ?? undefined,
      submenu: true,
      onSelect: () => openMenu('footer', 'models', 1),
    },
    ...(compact < 1
      ? [{ id: 'settings', label: t('ocr.menu.ocr_settings'), onSelect: () => openSettings('ocr') }]
      : []),
  ]
  const mainPage: MenuPage = {
    key: 'main',
    label: t('ocr.a11y.recognize_menu'),
    'data-testid': 'ocr-recognize-list',
    items: mainItems(menu.compact),
  }
  const modelsPage: MenuPage = {
    key: 'models',
    label: t('ocr.menu.models'),
    'data-testid': 'ocr-model-menu',
    items:
      menu.at === 'footer'
        ? modelItems
        : [{ type: 'label', id: 'models-title', label: t('ocr.menu.models') }, ...modelItems],
    back:
      menu.at === 'footer'
        ? {
            label: t('ocr.menu.models'),
            ariaLabel: t('ocr.a11y.back_to_menu'),
            onSelect: () => openMenu('footer', 'main', -1),
          }
        : undefined,
  }

  // ---- 渲染 ----

  // 头部 ⋯（面板宿主）：重新截图（origin = panel）· 识别历史…（设置 › OCR 的历史列表）· OCR 设置…
  const shotAction = direct ? 'ocr_translate' : 'ocr'
  const shotKey = hotkeyFor(snap, shotAction)
  const moreMenu = (
    <DropdownMenu
      label={t('common.a11y.more_actions')}
      align="end"
      items={[
        {
          id: 'retake',
          label: t('ocr.menu.retake'),
          icon: Crop,
          shortcut: shotKey?.combo ?? null,
          onSelect: () =>
            void api.triggerAction(shotAction, 'panel').catch(warnCode('trigger_action')),
        },
        { type: 'separator', id: 'sep' },
        {
          id: 'history',
          label: t('ocr.menu.history'),
          icon: History,
          onSelect: () => openSettings('ocr', 'ocr_history'),
        },
        {
          id: 'settings',
          label: t('ocr.menu.ocr_settings'),
          icon: Settings2,
          onSelect: () => openSettings('ocr'),
        },
      ]}
      data-testid="ocr-more-menu"
      renderTrigger={(trigger, open) => (
        <IconButton
          {...trigger}
          icon={Ellipsis}
          label={t('common.a11y.more_actions')}
          noTooltip={open}
          data-testid="ocr-more"
        />
      )}
    />
  )

  const s = main.state
  const done = s.phase === 'done'
  const cancelled = s.phase === 'cancelled'
  const exhausted = isExhausted(s)
  const noText = done && (exhausted || s.done?.no_text === true || !s.hasText)
  const editable = !direct && ((done && !noText) || (cancelled && s.hasText))
  const notReady: NotReady | null = pinned || !blocked ? null : payload.not_ready
  /** 离线模型在卡片上下载完成：截图还在（有 capture_id）就直接识别这张截图，否则卡片提示重新截图 */
  const offlineReady = (): void => {
    if (!payload.capture_id) return
    setReadyNow(true)
    main.begin((cb) => api.ocrStart(ocrReq(mode, null, direct ? payload.target_lang : null), cb))
  }
  const modeName = t(`common.ocr_mode.${direct ? 'translate' : mode}`)
  // 头部第二行：识别中… / 「模式 · 模型」（出错时同样保留，失败只由错误卡片表达）
  let metaStatus: ReactNode = null
  if (!notReady) {
    if (live(main) || s.phase === 'idle') {
      // 等第一段文字时：离线模型冷启动、云端排队各有一句（同一句也出现在骨架下方）
      metaStatus =
        s.stage === 'loading_model'
          ? t('common.state.loading_model')
          : s.stage === 'queued'
            ? t('common.state.queued')
            : t(direct ? 'common.state.translating' : 'common.state.recognizing')
    } else {
      const modelName = s.start?.model_label || (s.phase === 'error' ? currentModelName : null)
      // 「模式 · 模型」：放不下时在两段之间换行（不在模型名中间断开），换到行首的分隔点被裁掉（ocr.css）；
      // 读屏读到的仍是「模式 · 模型」
      metaStatus = (
        <span className="ocr-meta-segs">
          <span className="ocr-meta-seg">{modeName}</span>
          {modelName ? (
            <span className="ocr-meta-seg">
              <span className="ui-sr-only"> · </span>
              {modelName}
            </span>
          ) : null}
        </span>
      )
    }
  }
  // 读屏播报（polite）：识别中 / 识别完成 / 未检测到文字 / 识别失败 / 已取消
  let announce = ''
  if (!notReady) {
    if (live(main)) {
      announce = t(direct ? 'common.state.translating' : 'common.state.recognizing')
    } else if (s.phase === 'error') announce = t('ocr.meta.failed')
    // 切换排版的结果排在「已取消」前面：取消后的半截文字同样可以切换，也要播报
    else if (content.notice) {
      const { layout: switched, copied } = content.notice
      announce = t(copied ? 'ocr.layout.switched_copied' : 'ocr.layout.switched', {
        name: t(layoutLabel(switched)),
      })
    } else if (cancelled) announce = t('panel.status.cancelled')
    else if (exhausted) announce = t('ocr.empty.no_result')
    else if (noText) announce = t('ocr.empty.no_text')
    else if (done) announce = t(direct ? 'ocr.a11y.translated' : 'ocr.a11y.recognized')
  }

  /** 状态行的提示（优先级见 notes.ts）：短句放不下时省略号截断，全文在 title 里 */
  const noteOf = (kind: NoteKind | null): StatusLineProps['note'] => {
    if (kind === null) return null
    const spec: NoteSpec = NOTE_SPECS[kind]
    const text = t(spec.label)
    return {
      icon: spec.icon,
      text: (
        <span className="ocr-note-text" title={text}>
          {text}
        </span>
      ),
      tone: spec.tone,
    }
  }
  const note = noteOf(mainNote(s, edited))
  const ts = tr.state
  const stale = basis.current !== null && !sameSource(basis.current)
  const trNote = noteOf(translationNote(ts, stale))

  const errorCard = (run: Run, onRetry: () => void, chooseModel: boolean): ReactNode =>
    run.state.error ? (
      <div ref={chooseModel ? errorRef : undefined} className="ocr-card">
        <ErrorCard
          error={run.state.error}
          providerName={
            snap?.settings.providers.find((p) => p.id === run.state.start?.provider_id)?.name ??
            null
          }
          onRetry={onRetry}
          onOpenSettings={() => openSettings(settingsSectionFor(run.state.error?.code ?? ''))}
          onChooseModel={chooseModel ? () => openMenu('error', 'models') : undefined}
          onSwitchModel={chooseModel ? () => openMenu('error', 'models') : undefined}
          onOpenUrl={(url) => void api.openUrl(url).catch(warnCode('open_url'))}
        />
      </div>
    ) : null

  const shrinkable = editable || live(main) || (settled(main) && !noText && direct)
  // 结构化模式开始识别时就预取预览代码；结果里有表格 / 公式时默认显示预览（源码可以切换回来编辑）
  const structuredMod = useStructuredModule(!direct && mode === 'markdown')
  const structured = editable && mode === 'markdown' && hasStructure(ocrText)
  const preview = structured && !showSource
  let body: ReactNode
  if (notReady) {
    body = (
      <NotReadyBody
        notReady={notReady}
        direct={direct}
        snap={snap}
        open={openSettings}
        onOfflineReady={offlineReady}
        canResume={payload.capture_id !== ''}
      />
    )
  } else if (s.phase === 'error') {
    body = errorCard(main, () => recognize({}, true), true)
  } else if (noText) {
    // 底栏已有 [重新识别 | ▾]：空状态给的是下一步（换模式 / 换模型，都是打开选择，▾ 后置），不再重复「重新识别」
    const switchModel = exhausted || direct
    const emptyOpen = menu.open && menu.at === (switchModel ? 'error' : 'footer')
    body = (
      <div ref={errorRef} className="ocr-card">
        <EmptyState
          compact
          icon={exhausted ? BrainCircuit : ScanText}
          title={t(exhausted ? 'ocr.empty.no_result' : 'ocr.empty.no_text')}
          detail={t(exhausted ? 'ocr.empty.no_result_detail' : 'ocr.empty.no_text_detail')}
          action={
            <>
              <Button
                ref={emptyAction}
                size="sm"
                variant="secondary"
                trailingIcon={ChevronDown}
                aria-haspopup="menu"
                aria-expanded={emptyOpen}
                onClick={() =>
                  emptyOpen
                    ? closeMenu()
                    : switchModel
                      ? openMenu('error', 'models')
                      : openMenu('footer', 'main')
                }
                data-testid="ocr-empty-action"
              >
                {t(switchModel ? 'ocr.action.switch_model' : 'ocr.action.switch_mode')}
              </Button>
              {/* 思考耗尽：说明里的另一条出路「调高输出上限」（与翻译视图的思考耗尽卡片同一种带图标的 ghost 按钮，
                  不用内联链接样式） */}
              {exhausted ? (
                <Button
                  size="sm"
                  variant="ghost"
                  icon={SlidersHorizontal}
                  onClick={() => openSettings('providers')}
                  data-testid="ocr-empty-raise"
                >
                  {t('ocr.action.raise_limit')}
                </Button>
              ) : null}
            </>
          }
          data-testid="ocr-empty"
        />
      </div>
    )
  } else if (preview && structuredMod) {
    const Preview = structuredMod.StructuredPreview
    body = (
      <Preview
        text={ocrText}
        label={t('ocr.a11y.text')}
        className="ocr-text"
        scrollRef={previewRef}
        onScroll={(e) => syncFadeEdges(e.currentTarget)}
        data-testid="ocr-preview"
      />
    )
  } else if (editable) {
    body = (
      <TextArea
        ref={textRef}
        variant="bare"
        value={ocrText}
        onChange={onEdit}
        minRows={1}
        // 可见高度由 flex 决定（ocr.css）：这里只让它按全文自然增高，放不下时在框内滚动
        maxRows={100_000}
        spellCheck={false}
        aria-label={t('ocr.a11y.text')}
        className="ocr-text"
        onScroll={(e) => {
          syncFadeEdges(e.currentTarget)
          trackScroll(e.currentTarget)
        }}
        onFocus={() => {
          area.current = 'ocr'
        }}
        data-testid="ocr-text"
      />
    )
  } else {
    body = (
      <RunBody
        run={main}
        format="plain"
        maxHeight={layout.viewMax}
        skeleton={3}
        ghost={mainGhost}
        waitingNote={
          // 头部已经写着「正在加载离线模型…」/「云端处理中…」：骨架下方只补一句为什么要等。加载通常一两秒就完成，
          // 说明几秒后才淡入（CSS 的 animation-delay，见 components.css 的 .ui-late），快的时候不会一闪而过
          s.stage === 'loading_model' ? (
            <span className="ui-late">{t('common.state.loading_slow')}</span>
          ) : s.stage === 'queued' ? (
            engine.kind === 'cloud' && isSlowCloudModel(engine.model) ? (
              t('ocr.wait.queued_slow')
            ) : null
          ) : direct && styleId !== null && live(src) && !mainGhost ? (
            t('ocr.translation.transcribing')
          ) : null
        }
        label={t(direct ? 'ocr.a11y.translation' : 'ocr.a11y.text')}
        data-testid="ocr-stream"
      />
    )
  }

  const footerOn = !notReady && s.phase !== 'error' && (settled(main) || footerSeen)
  const busy = live(main)
  const hasText = !busy && s.hasText && !noText
  /** 排版控件：逐行 / 段落模式、有文字时（重新识别期间置灰保留，底栏不跳） */
  const layoutShown = footerOn && layoutOn && !noText && !notReady
  const markOcr = () => {
    area.current = 'ocr'
  }
  const markTranslation = () => {
    area.current = 'translation'
  }
  const recognizeOpen = menu.open && menu.at === 'footer'
  const discarding = confirm !== null && 'discard' in confirm
  // 展开区：卡片已到上限（或马上会到）时展开 / 收起用高度过渡，正文区逐帧让出 / 拿回
  const sectionRoom = LAYOUT.section + LAYOUT.status + lh
  const willMax = atMax || (sizes.current?.view ?? 0) + sectionRoom > layout.viewMax
  const mainMin = shrinkable
    ? Math.min(mainContent || layout.mainFloor, layout.mainFloor)
    : undefined
  const settledStatus = settled(main)

  return (
    // oxlint-disable-next-line jsx-a11y/no-noninteractive-element-interactions -- 按下让面板成为 key、从空白处开始拖动（usePanelDrag）
    <section
      ref={viewRef}
      className="ocr-view"
      data-testid="view-ocr"
      data-view="ocr"
      data-host={host}
      data-session={sessionId}
      data-phase={notReady ? 'not_ready' : s.phase}
      data-mode={direct ? 'translate' : mode}
      aria-label={t('ocr.a11y.view')}
      style={host === 'panel' ? { maxHeight: layout.viewMax } : undefined}
      onPointerDown={
        host === 'panel'
          ? (e) => {
              activate()
              panelDrag?.onPointerDown(e)
            }
          : undefined
      }
      onMouseDown={panelDrag?.onMouseDown}
    >
      <output className="ui-sr-only" aria-live="polite" data-testid="ocr-announce">
        {announce}
      </output>
      <div ref={topRef} className="ocr-top">
        <MediaHead
          payload={shown}
          status={metaStatus}
          expanded={zoom}
          onToggleImage={() => setZoom((z) => !z)}
          thumbMaxWidth={thumbMaxWidth(topWidth)}
          thumbHeight={layout.thumb}
          onClose={host === 'panel' ? () => requestHide('close_button') : null}
          more={host === 'panel' ? moreMenu : null}
          mediaRef={mediaRef}
        />
        {direct && !notReady ? (
          <div className="ocr-subhead" data-testid="ocr-direct-head">
            <LangBar
              source={s.meta?.detected_source_lang ?? null}
              detecting={live(main) && !s.meta}
              lowConfidence={s.meta?.low_confidence === true}
              target={target ?? s.meta?.target_lang ?? payload.target_lang}
              onPickTarget={pickTarget}
            />
            <span className="ocr-spacer" />
            {styleMenu('ocr-direct-style-list', (p) => (
              <StyleChip {...p} name={currentStyleName} data-testid="ocr-style-chip" />
            ))}
          </div>
        ) : null}
      </div>
      {shown.thumb_url !== '' ? (
        <ZoomImage payload={shown} open={zoom} anchor={mediaRef} onClose={() => setZoom(false)} />
      ) : null}
      {/* oxlint-disable-next-line jsx-a11y/no-static-element-interactions -- 只记录最近一次交互的区域 */}
      <div
        ref={mainBox}
        className="ocr-body"
        data-shrink={shrinkable || undefined}
        style={mainMin !== undefined ? { minHeight: mainMin } : undefined}
        onPointerDown={markOcr}
      >
        {body}
      </div>
      <FlipBox handle={tailRef} className="ocr-tail">
        <InlineConfirm
          open={confirm !== null}
          // 按钮由这里放进条内：警示语调；重新识别的确认按钮带上 ⌘R 提示（再按一次 ⌘R 也能确认）
          message={
            <span className="ocr-confirm-row">
              <span className="ocr-confirm-text">
                {t(discarding ? 'ocr.layout.discard_confirm' : 'ocr.confirm.overwrite')}
              </span>
              <ConfirmActions
                onCancel={() => setConfirm(null)}
                onConfirm={confirmNow}
                label={t(discarding ? 'ocr.layout.discard_action' : 'ocr.confirm.overwrite_action')}
                combo={discarding ? null : 'Mod+KeyR'}
              />
            </span>
          }
          buttons={false}
          tone="warning"
          className="ocr-confirm"
          onConfirm={confirmNow}
          onCancel={() => setConfirm(null)}
          data-testid="ocr-confirm"
        />
        {footerOn && settledStatus && hasText && !direct ? (
          <OcrChips
            text={ocrText}
            raw={main.text()}
            editing={content.edit !== null}
            os={os}
            onPick={pasteFragment}
            onInspect={content.inspected}
          />
        ) : null}
        {footerOn ? (
          <Footer data-testid="ocr-footer">
            <StatusLine
              model={settledStatus ? s.start?.model_label || null : null}
              elapsedMs={settledStatus ? elapsedOf(s.done) : null}
              usage={settledStatus ? (s.done?.usage ?? null) : null}
              state={cancelled ? 'cancelled' : null}
              // 「已复制」只停留一两秒：显示期间由它占住提示的位置，之后提示（部分区域未能识别、来自缓存……）再回来——
              // 两者并排时底栏放不下，「已复制」会被裁掉一半
              note={toast.visible ? null : note}
              trailing={toast.node}
              className="ocr-status"
            />
            <span className="ocr-actions">
              {structured && structuredMod ? (
                <IconButton
                  icon={showSource ? Table2 : Code}
                  label={t(showSource ? 'ocr.preview.show_preview' : 'ocr.preview.show_source')}
                  active={showSource}
                  onClick={() => setShowSource((v) => !v)}
                  data-testid="ocr-source-toggle"
                />
              ) : layoutShown ? (
                <LayoutSplit
                  text={content}
                  disabled={!hasText}
                  onDiscard={() => setConfirm({ discard: true })}
                />
              ) : null}
              {noText ? null : (
                <CopyButton
                  text={() => (direct ? main.text() : ocrText)}
                  label={t(direct ? 'ocr.a11y.copy_translation' : 'ocr.a11y.copy_text')}
                  shortcut="Mod+KeyC"
                  disabled={!hasText}
                  data-testid="ocr-copy"
                />
              )}
              {direct ? (
                noText ? null : (
                  <Split selected={srcOpen} data-testid="ocr-show-source-split">
                    <Chip
                      variant="subtle"
                      icon={ScanText}
                      selected={srcOpen}
                      // 底栏太挤时只留图标（ocr.css 的 data-compact），名称仍要读得出来
                      aria-label={t('ocr.action.source')}
                      aria-pressed={srcOpen}
                      aria-expanded={srcOpen}
                      onClick={toggleSource}
                      data-testid="ocr-show-source"
                    >
                      {t('ocr.action.source')}
                    </Chip>
                  </Split>
                )
              ) : noText ? null : (
                <Split selected={trOpen} data-testid="ocr-translate">
                  <Chip
                    variant="subtle"
                    icon={Languages}
                    selected={trOpen}
                    aria-label={t('common.action.translate')}
                    disabled={!hasText}
                    onClick={toggleTranslate}
                    aria-expanded={trOpen}
                    data-testid="ocr-translate-toggle"
                  >
                    {t('common.action.translate')}
                  </Chip>
                  {styleMenu(
                    'ocr-style-list',
                    (p) => (
                      <SplitCaret
                        {...p}
                        label={styleMenuLabel}
                        disabled={!hasText}
                        data-testid="ocr-translate-menu"
                      />
                    ),
                    'top',
                  )}
                </Split>
              )}
              <Split ref={recognizeRef} open={recognizeOpen} data-testid="ocr-recognize">
                <IconButton
                  icon={RotateCw}
                  label={t(direct ? 'common.action.regenerate' : 'ocr.action.recognize_again')}
                  shortcut="Mod+KeyR"
                  onClick={() => recognize()}
                  data-testid="ocr-recognize-again"
                />
                <SplitCaret
                  ref={recognizeCaret}
                  label={t('ocr.a11y.recognize_menu')}
                  aria-haspopup="menu"
                  aria-expanded={recognizeOpen}
                  data-open={recognizeOpen || undefined}
                  onClick={() => (recognizeOpen ? closeMenu() : openMenu('footer', 'main'))}
                  onKeyDown={(e) => {
                    if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return
                    e.preventDefault()
                    openMenu('footer', 'main')
                  }}
                  data-testid="ocr-recognize-menu"
                />
              </Split>
              {host === 'panel' ? (
                <IconButton
                  icon={Pin}
                  label={t('common.action.pin')}
                  shortcut="Mod+KeyP"
                  onClick={pin}
                  data-testid="ocr-pin"
                />
              ) : null}
            </span>
          </Footer>
        ) : null}

        {direct ? (
          <Section
            open={srcOpen}
            expand={willMax}
            collapse={atMax}
            label={t('ocr.a11y.source')}
            data-testid="ocr-source"
          >
            <div className="ocr-section-head">
              <span className="ocr-section-title">{t('ocr.translation.source_title')}</span>
              <span className="ocr-spacer" />
              <CopyButton
                text={() => src.text()}
                label={t('ocr.a11y.copy_source')}
                disabled={!src.state.hasText}
                data-testid="ocr-source-copy"
              />
            </div>
            <SmoothBox
              ref={sectionBox}
              animate={atMax}
              stream={live(src)}
              streamKey={src.state.key}
            >
              {src.state.phase === 'error' ? (
                errorCard(src, () => transcribe(model), false)
              ) : (
                <RunBody
                  run={src}
                  format="plain"
                  skeleton={atMax ? 1 : 2}
                  label={t('ocr.a11y.source')}
                  data-testid="ocr-source-body"
                />
              )}
            </SmoothBox>
          </Section>
        ) : (
          <Section
            open={trOpen}
            expand={willMax}
            collapse={atMax}
            label={t('ocr.a11y.translation')}
            data-testid="ocr-translation"
          >
            {/* oxlint-disable-next-line jsx-a11y/no-static-element-interactions -- 只记录最近一次交互的区域 */}
            <div className="ocr-section-head" onPointerDown={markTranslation}>
              <LangBar
                source={ts.meta?.detected_source_lang ?? null}
                detecting={live(tr) && !ts.meta}
                lowConfidence={ts.meta?.low_confidence === true}
                target={target ?? ts.meta?.target_lang ?? payload.target_lang}
                onPickTarget={pickTarget}
              />
              <span className="ocr-spacer" />
              {styleMenu('ocr-section-style-list', (p) => (
                <StyleChip {...p} name={currentStyleName} data-testid="ocr-style-chip" />
              ))}
              <CopyButton
                text={() => tr.text()}
                label={t('ocr.a11y.copy_translation')}
                disabled={!ts.hasText}
                data-testid="ocr-translation-copy"
              />
            </div>
            <SmoothBox
              ref={sectionBox}
              animate={atMax}
              stream={live(tr)}
              streamKey={tr.state.key}
              onPointerDown={markTranslation}
            >
              {ts.phase === 'error' ? (
                errorCard(tr, () => translate({ bypass: true }), false)
              ) : ts.phase === 'confirm' && ts.longText ? (
                // 长文本：确认之后才翻译（与翻译视图同一张卡片；Esc 照常关闭面板）
                <LongTextPrompt
                  info={ts.longText}
                  onContinue={() => translate({ confirm: true })}
                  onCancel={() => {
                    tr.clear()
                    setTrOpen(false)
                  }}
                />
              ) : (
                <RunBody
                  run={tr}
                  format={
                    styles.find((x) => x.id === effStyle)?.output === 'markdown_lite' ||
                    ts.meta?.dict_mode
                      ? 'mdlite'
                      : 'plain'
                  }
                  // 卡片已到上限时骨架只占一行：结果至少一行，展开区在这一次操作里只会长高、不会先高后矮
                  skeleton={atMax ? 1 : 2}
                  ghost={trGhost}
                  label={t('ocr.a11y.translation')}
                  data-testid="ocr-translation-body"
                />
              )}
            </SmoothBox>
            {ts.phase !== 'error' ? (
              // 状态行从展开起就占位（翻译中…），结束时结果淡入：不会在完成时再长出一行
              <SectionFoot handle={footRef} data-testid="ocr-translation-foot">
                {settled(tr) ? (
                  <m.div key={ts.key} className="ocr-foot-row" {...footerIn}>
                    <StatusLine
                      model={ts.start?.model_label ?? null}
                      elapsedMs={elapsedOf(ts.done)}
                      usage={ts.done?.usage ?? null}
                      state={ts.phase === 'cancelled' ? 'cancelled' : null}
                      note={trNote}
                      className="ocr-status"
                    />
                    {stale ? (
                      <Button
                        size="sm"
                        variant="ghost"
                        icon={Languages}
                        onClick={() => translate()}
                        data-testid="ocr-retranslate"
                      >
                        {t('ocr.action.retranslate')}
                      </Button>
                    ) : null}
                  </m.div>
                ) : live(tr) ? (
                  <span className="ocr-foot-wait">{t('common.state.translating')}</span>
                ) : null}
              </SectionFoot>
            ) : null}
          </Section>
        )}
      </FlipBox>

      <PagedMenu
        open={menu.open}
        onClose={closeMenu}
        anchor={menu.at === 'footer' ? recognizeRef : errorRef}
        returnFocus={menu.at === 'footer' ? recognizeCaret : emptyAction}
        page={menu.page === 'models' ? modelsPage : mainPage}
        direction={menu.dir}
        side={menu.at === 'footer' ? 'top' : 'bottom'}
        align={menu.at === 'footer' ? 'end' : 'start'}
        minWidth={MENU_WIDTH}
      />
    </section>
  )
}

/** 确认丢掉修改的按钮（InlineConfirm 的条内，警示语调；重新识别的确认按钮带 ⌘R 提示） */
function ConfirmActions({
  onCancel,
  onConfirm,
  label,
  combo,
}: {
  onCancel: () => void
  onConfirm: () => void
  label: string
  /** 再按一次就能确认的快捷键（重新识别的 ⌘R） */
  combo: string | null
}) {
  const t = useT()
  return (
    <span className="ocr-confirm-actions" data-testid="ocr-confirm-actions">
      <Button size="sm" variant="ghost" onClick={onCancel}>
        {t('common.action.cancel')}
      </Button>
      <Button size="sm" variant="secondary" onClick={onConfirm} data-testid="ocr-confirm-ok">
        {label}
        {combo ? <Kbd combo={combo} variant="inline" /> : null}
      </Button>
    </span>
  )
}

/**
 * 未就绪：NotReadyCard 按 key 渲染。
 * - 缺屏幕录制权限：说明换成合并后的一句「在系统设置里打开开关后，点「重新启动 Wisp」，之后按 ⌥S 截图。」（键帽）；
 * - 截图失败：卡片带 [重新截图 ⌥S]（`trigger_action(ocr | ocr_translate, origin = panel)`，Rust 先隐藏面板再截图）；
 *   截图后端被占用 / 显示器刚变化时说明补上原因。
 */
function NotReadyBody({
  notReady,
  direct,
  snap,
  open,
  onOfflineReady,
  canResume,
}: {
  notReady: NotReady
  direct: boolean
  snap: ReturnType<typeof useSettingsSnapshot>
  open: (section: SettingsSection) => void
  /** 离线模型下载完成 */
  onOfflineReady: () => void
  /** 这张截图还在：下载完成后直接识别（否则提示重新截图） */
  canResume: boolean
}) {
  const t = useT()
  const action = direct ? 'ocr_translate' : 'ocr'
  const key = hotkeyFor(snap, action)
  if (isOfflineNotReady(notReady)) {
    const retakeNow = () => {
      void api.triggerAction(action, 'panel').catch(warnCode('trigger_action'))
    }
    return (
      <div className="ocr-notready">
        <OfflineCard
          notReady={notReady}
          onReady={onOfflineReady}
          readyAction={
            canResume ? undefined : (
              <Button
                size="sm"
                variant="primary"
                onClick={retakeNow}
                data-testid="ocr-offline-retake"
              >
                {t('panel.not_ready.retake')}
                {key?.combo ? <Kbd combo={key.combo} variant="inline" /> : null}
              </Button>
            )
          }
          // 「切换模型」：设置 › OCR 的「OCR 模型」（本机的离线选择在那里改）
          onSwitchModel={() => open('ocr')}
          resumes={canResume}
          data-testid="ocr-offline-card"
        />
      </div>
    )
  }
  let detail: ReactNode = null
  if (notReady.reason === 'permission_missing' && notReady.permission === 'screen_recording') {
    detail = key ? (
      <HintWithKey text={t('ocr.hint.retrigger', { key: '\u0000' })} hotkey={key} />
    ) : (
      t('ocr.hint.retrigger_plain')
    )
  } else if (
    notReady.reason === 'capture_failed' &&
    notReady.message === 'errors.capture.backend'
  ) {
    detail = t('ocr.hint.capture_backend_short')
  }
  const retake = () => {
    void api.triggerAction(action, 'panel').catch(warnCode('trigger_action'))
  }
  return (
    <div className="ocr-notready">
      <NotReadyCard
        notReady={notReady}
        detail={detail}
        onOpenSettings={() => open(settingsSectionFor(notReady.reason))}
        onOpenSystemSettings={(kind) => {
          void api.permissionsOpen(kind).catch(warnCode('permissions_open'))
        }}
        // macOS 的屏幕录制要重启 Wisp 才生效（与引导的权限步骤一致）：授权后直接在这里重启，不会再按 ⌥S 落回同一张卡片
        onRelaunch={() => void api.appRelaunch().catch(warnCode('app_relaunch'))}
        onRetry={retryable(notReady) ? retake : undefined}
        retryShortcut={key?.combo ?? null}
      />
    </div>
  )
}

/** 重试也不会成功的截图失败：当前桌面不支持截图；Wayland 会话（这一版没有接入截图门户） */
const NOT_RETRYABLE = new Set(['errors.capture.unsupported', 'errors.platform.wayland_capture'])

/**
 * 重新截图有意义的截图失败：后端被占用 / 显示器刚变化、没找到显示器、以及没有归类的失败；NOT_RETRYABLE 里的
 * 不给按钮（按了只会落回同一张卡片）
 */
function retryable(notReady: NotReady): boolean {
  return notReady.reason === 'capture_failed' && !NOT_RETRYABLE.has(notReady.message)
}

/** 把文案里的快捷键占位换成键帽（Kbd，与其他地方的快捷键显示一致；没有组合串时退回显示串） */
function HintWithKey({
  text,
  hotkey,
}: {
  text: string
  hotkey: { combo: string | null; display: string }
}) {
  const [before, after] = text.split('\u0000')
  // 引用的按钮名（「重新启动 Wisp」）整体不拆行（keepQuoted.tsx）
  return (
    <>
      {keepQuoted(before)}
      {hotkey.combo ? (
        <Kbd combo={hotkey.combo} variant="cap" className="ocr-hint-kbd" />
      ) : (
        hotkey.display
      )}
      {keepQuoted(after)}
    </>
  )
}
