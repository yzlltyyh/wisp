/**
 * 剪贴板视图的 IPC 编排：检索（去抖 60 ms、过期结果作废）、过滤、分页、实时事件、粘贴（整条 / 变换后 / 片段 / 多条
 * 合并）/ 收藏与命名 / 删除 / 清空 / 暂停、光标条目的检视（芯片条）与条目的其他操作。状态都在每个会话一份的 store
 * 里，这里只负责「什么时候调用哪条命令、结果怎么落进 store」。
 *
 * - 粘贴：`clipboard_paste` 由 Rust 立即隐藏面板（不播放退出动画）、还原焦点、写剪贴板并模拟粘贴键；只复制的
 *   各分支由 Rust 以 toast 视图提示。前端不另外处理成功的结果。唯一的例外是平台层要求由 WebView 写剪贴板
 *   （KDE Wayland 不接受后台写入，`platform` + `errors.platform.wayland_clipboard*`，与 `useCopy`
 *   同一约定）：面板此刻仍有焦点，前端用 `navigator.clipboard` 写入（文本类与图片；之后的粘贴记住这一点，在按键的同一个
 *   任务里直接写，不再先绕 Rust，WebKit 只在用户手势里放行写入），再在原位切换到 toast 视图提示「已复制，按 Ctrl+V 粘贴」；
 * - 收藏 / 命名 / 删除 / 清空 / 暂停都先乐观更新，失败时回滚（检索结果与清空前的列表一并恢复）并在底部提示是哪个操作失败；
 * - 粘贴的各种来源都由 Rust 取正文、变换、拼接后写入（1 MB 的正文不经 WebView 往返）；片段、变换结果与合并结果不进历史；
 * - 检视（`clipboard_inspect`）只对光标所在的文本类条目、在光标停下来之后发起（INSPECT_DEBOUNCE_MS），结果按条目缓存；
 *   识别出的值不写日志；
 * - 条目已不存在（`not_found`）时从列表移除；文件条目的粘贴 / 「在 Finder 中显示」返回 `not_found` 表示**文件**都已
 *   不在了（记录还在，见 Rust 的 `paste_payload` / `reveal`）：只把这些文件标为不存在（整行置灰），不移除；
 * - 后台路径不读文件内容、不列目录：列表只用元数据，详情（`clipboard_get`）只在用户悬停 / 预览 /
 *   送翻译时请求；悬停只取正文开头（`textHead`），长正文不常驻；
 * - 剪贴板条目 → 翻译：优先 `clipboard_send_to(translate)`（正文留在 Rust 里取），命令未实现时退回交接里的
 *   `trigger_action(translate_selection, text, origin: clipboard)`；两条路径都由 Rust 发起交叉淡入的 prepare；
 * - `reloaded`（一大批改动，例如其他设备同步来的条目）：Rust 在视图显示期间改发逐条事件，`reloaded` 只在视图还没
 *   显示时发出；它到达时（通常在打开的过程中，偶尔与显示交错）立即重新拉首页——列表不能停在过时的内容上，点到已经
 *   不存在的条目。位置刚变过的行由列表的防误点保护；只有退出动画期间收到的暂存到隐藏之后、或下一次打开时再拉。
 */
import { useEffect, useState } from 'react'
import { api, getTransport, onMany, resolveAssetUrl, WispIpcError } from '../../shared/ipc'
import { usePrewarm } from '../../shared/perf/prewarm'
import type {
  ClipboardChanged,
  ClipDragData,
  ClipFilter,
  ClipHit,
  ClipInspect,
  ClipItemDetail,
  ClipItemMeta,
  OsKind,
  PasteMode,
  PasteOutcome,
  QueueStatus,
  TransformId,
} from '../../shared/ipc/types'
import type { PanelPhase } from '../../panel/types'
import { useHidden } from '../../shared/hooks/useHidden'
import { t, tKey } from '../../shared/i18n/i18n'
import { formatHotkey } from '../../shared/keys/formatHotkey'
import { duration, ms, timing } from '../../shared/motion/tokens'
import { copiedNotice, queueNotice } from './copied'
import type { ClipHost } from './host'
import { isTextual } from './kinds'
import { inspectOrEmpty, NO_INSPECT, orderEntities, orderTransforms } from './specs'
import {
  appendPage,
  HOVER_HEAD_CHARS,
  indexOfId,
  INSPECT_DEBOUNCE_MS,
  LARGE_TEXT_CHARS,
  PAGE_SIZE,
  SEARCH_DEBOUNCE_MS,
  SEARCH_LIMIT,
  SEARCH_LIMIT_MAX,
  stepItem,
  type Row,
} from './model'
import {
  applyChange,
  canLoadMore,
  isSearching,
  keepSelection,
  placeItem,
  removeFromLists,
  restoreHit,
  selectedItem,
  syncMarks,
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
/** 检视结果的缓存条数（来回移动光标时不重复请求；与 Rust 引擎里的缓存条数一致） */
const INSPECT_CACHE = 8

/**
 * 清空剪贴板历史会不会同步到其他设备（同步已开启且同步剪贴板历史），决定清空确认的文案。打开面板时不读设置：在打开含
 * 「清空历史…」的菜单与发起清空时才读；面板 WebView 常驻，读到的结果留给之后的确认先用。null = 还没读过
 */
let clearSyncs: boolean | null = null

/**
 * 本 WebView 已确认剪贴板要由前端写（KDE Wayland）。面板 WebView 常驻，记在模块里：之后的粘贴在按键的
 * 同一个任务里直接写（WebKit 只在用户手势里放行 `navigator.clipboard.write`），不再先绕一次 Rust
 */
let webviewClipboard = false

/** 失败提示里的动作名（`clipboard.status.failed.*`） */
export type FailedAction =
  | 'paste'
  | 'copy'
  | 'favorite'
  | 'unfavorite'
  | 'rename'
  | 'delete'
  | 'ocr'
  | 'float'
  | 'queue'
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

/**
 * 视图的命令层：只负责 IPC 编排与 store 的乐观更新。用户能触发的操作（快捷键、菜单、提示条、按钮）在
 * `actions/` 的注册表里声明，由注册表调用这里的命令
 */
export interface ClipCommands {
  setQuery(query: string): void
  setFilter(filter: ClipFilter): void
  /** `jump`：指示器直接到位（按键重复、按页 / 到首尾），否则单步 spring 追随 */
  select(id: number | null, reveal?: boolean, jump?: boolean): void
  loadMore(): void
  /** 粘贴一条；`transform`（「粘贴为…」）由 Rust 取全文变换后写入，结果不进历史 */
  paste(mode: PasteMode, id?: number, transform?: TransformId | null): void
  /** 多选的条目按选择顺序以换行连接后粘贴（合并结果不进历史） */
  pasteMany(items: readonly ClipItemMeta[], mode: PasteMode): void
  /** 粘贴一个片段（芯片）：不进历史，只更新来源条目的最近使用时间 */
  pasteText(text: string, from: number | null, mode: PasteMode): void
  /** 收藏 / 取消收藏：一条时切换；多条时只要有一条还没收藏就全部收藏，否则全部取消 */
  toggleFavorite(items: readonly ClipItemMeta[]): void
  /** 底部的命名条：打开（草稿是现有名称）、编辑、提交（空 = 清除名称）、取消 */
  startRename(id: number): void
  setRenameDraft(draft: string): void
  commitRename(): void
  cancelRename(): void
  /** 折叠 / 展开列表顶部的「收藏」分组（本机保存） */
  toggleFavoritesCollapsed(): void
  remove(force: boolean, id?: number): void
  /** 删除多选的条目；其中有收藏且不是 force 时先弹出行内确认 */
  removeMany(items: readonly ClipItemMeta[], force: boolean): void
  /** 多选：⌘ 点击切换一条、⇧ 点击 / ⇧↑↓ 从起点扩展到某一条（按当前的行顺序）、清除 */
  toggleMark(id: number): void
  markRange(rows: readonly Row[], to: number): void
  extendMarks(rows: readonly Row[], step: 1 | -1): void
  clearMarks(): void
  /** 检视一条（有缓存）；图片 / 文件直接给出空结果 */
  inspect(id: number): Promise<ClipInspect>
  /** 立即检视光标所在的条目（菜单打开前）；不需要等待时返回 null */
  inspectNow(): Promise<void> | null
  /** 芯片条的键盘焦点：进入（⌘E）、←/→ 移动、退出 */
  enterChips(): void
  moveChip(step: 1 | -1): void
  exitChips(): void
  /** 图片送去识别文字（面板切到 OCR 视图） */
  ocr(id: number): void
  /** 交接到悬浮剪贴板（继承查询、筛选与光标） */
  openFloat(): void
  /** 连续粘贴多选的条目（按选择顺序）：先粘贴第一条，之后在任何应用里每按一次粘贴键粘贴下一条 */
  startQueue(items: readonly ClipItemMeta[]): void
  /** 结束进行中的连续粘贴 */
  cancelQueue(): void
  /** 拖出：按下时先取好拖拽数据（拖拽开始的回调里只能同步写入） */
  prepareDrag(ids: readonly number[]): void
  /** 已经取好的拖拽数据（还没取到时为 null：这次拖拽不开始） */
  dragData(ids: readonly number[]): ClipDragData | null
  cancelConfirm(): void
  /** 打开含「清空历史…」的菜单时：先读一次清空会不会同步到其他设备，确认条出现时文案就是对的 */
  prepareClear(): void
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

/** 命令层需要的宿主信息：平台（粘贴键的写法）与只复制之后的提示方式（见 host.ts） */
export interface ClipboardHost extends Pick<ClipHost, 'announceCopied'> {
  readonly os: OsKind
  /** 只复制之后谁来提示（默认 toast：面板，Rust 弹 toast 视图） */
  readonly copiedNotice?: ClipHost['copiedNotice']
}

/**
 * 视图此刻对用户的可见程度（决定 `reloaded` 什么时候处理）：
 * - `opening`：准备中 / 进入动画——还没开始操作，整页换掉无妨；
 * - `shown`：显示中——用户可能正看着、指着某一行（位置刚变的行单击不粘贴）；
 * - `leaving`：退出动画——马上就看不见了，不必再拉；
 * - `hidden`：窗口已隐藏（视图通常随之卸载）
 */
export type ClipVisibility = 'opening' | 'shown' | 'leaving' | 'hidden'

/** 面板阶段 + 窗口是否隐藏 → 可见程度 */
export function clipVisibility(phase: PanelPhase, windowHidden: boolean): ClipVisibility {
  if (windowHidden || phase === 'hidden') return 'hidden'
  if (phase === 'shown') return 'shown'
  if (phase === 'exiting') return 'leaving'
  return 'opening'
}

/**
 * 面板轻提示的停留（毫秒）：
 * - `ok`：一眼就懂的确认（「已保存图片」）toastHold（1.5 s）；
 * - `info`：带说明的信息（「已跳过一条疑似密码或密钥的内容」）按字数计时——toastHold + 每字 toastViewPerChar，夹在
 *   [noticeHold 3 s, toastViewDetailMax 5 s]（与设置窗口的轻提示、toast 视图的说明同一套规则：1.5 s
 *   读不完，而且这条有隐私含义）；
 * - `error`：statusErrorHold（4 s）——与设置 / 引导窗口一致，错误要读得完（「收藏失败，请再试一次」）
 */
export function statusHoldMs(text: string, tone: StatusTone): number {
  if (tone === 'error') return ms(timing.statusErrorHold)
  if (tone === 'ok') return ms(timing.toastHold)
  const chars = [...text.replace(/\s+/g, '')].length
  const hold = timing.toastHold + chars * timing.toastViewPerChar
  return ms(Math.min(timing.toastViewDetailMax, Math.max(timing.noticeHold, hold)))
}

/** 视图里用：每个会话（store）一份操作对象；挂载时订阅实时变化，卸载时释放 */
export function useClipboard(
  store: ClipStore,
  host: ClipboardHost,
  phase: PanelPhase = 'shown',
): ClipCommands {
  const [actions] = useState(() => createClipCommands(store, host))
  // JIT 预热渲染（ClipboardView 末尾）不订阅推送
  const prewarm = usePrewarm()
  const windowHidden = useHidden()
  const visibility = clipVisibility(phase, windowHidden)
  // 宿主回调（切换到 toast 视图等）每次渲染可能是新的函数：执行操作时用最新的
  useEffect(() => {
    actions.setHost(host)
  })
  useEffect(() => (prewarm ? undefined : actions.attach()), [actions, prewarm])
  useEffect(() => actions.setVisibility(visibility), [actions, visibility])
  return actions
}

/**
 * `clipboard:changed` 的常驻订阅：后端只在剪贴板视图 ready 之后推送、准备期产生的事件
 * ready 后立即补发——视图挂载时才异步 `listen` 的话，补发可能先于订阅生效而丢失（偶尔第一次打开时缩略图 / 置灰要到下次
 * 才出现）。面板 WebView 常驻，所以在面板启动时订阅一次、之后不再退订（一个监听的开销可以忽略），按当前挂载的视图分发；
 * 视图卸载只解绑处理函数
 */
let changedSink: ((change: ClipboardChanged) => void) | null = null
/**
 * 悬浮窗在 `clipboard_float_ready`（Rust 从此推送改动、并在这之后取首页）与视图 attach 之间到达的改动没有视图接收，
 * 而首页可能在它们之前就取好了：这段时间里记下是否漏掉了改动，attach 时据此重新拉取首页，不让新条目缺席到下一次
 * 改动。面板不需要：它隐藏时 Rust 不推送，下次打开的载荷本身就是最新的
 */
let awaitingView = false
let missedChanges = false

/** 悬浮窗调用 `clipboard_float_ready` 之前：开始记下视图 attach 之前漏掉的改动 */
export function awaitClipboardView(): void {
  awaitingView = true
  missedChanges = false
}
/** `clipboard:queue`（连续粘贴的进度）同样按当前挂载的视图分发 */
let queueSink: ((status: QueueStatus) => void) | null = null
/** `settings:changed` 里的识别设置变了：当前挂载的视图丢掉按旧设置算出的检视结果 */
let smartSink: (() => void) | null = null
/** 上一次看到的识别设置（序列化后比较；null = 还没看到过） */
let lastSmart: string | null = null
/** 常驻订阅装在哪个传输层上（生产里传输层不变；单测每个用例换一个，换了就重新订阅） */
let residentFor: { transport: unknown; off: () => void } | null = null

export function installClipboardEvents(): void {
  const transport = getTransport()
  if (residentFor?.transport === transport) return
  residentFor?.off()
  awaitingView = false
  missedChanges = false
  lastSmart = null
  residentFor = {
    transport,
    off: onMany({
      'clipboard:changed': (change) => {
        if (changedSink) changedSink(change)
        else if (awaitingView && change.change !== 'secret_skipped') missedChanges = true
      },
      'clipboard:queue': (status) => queueSink?.(status),
      'settings:changed': (snap) => {
        const smart = JSON.stringify(snap.settings.clipboard.smart)
        if (smart === lastSmart) return
        lastSmart = smart
        smartSink?.()
      },
    }),
  }
}

/**
 * 操作对象（与 React 无关，单测直接构造）。`attach()` 订阅 `clipboard:changed`，返回的函数在卸载（隐藏）时
 * 退订并释放计时器与详情缓存；StrictMode 下的重复挂载会重新 attach
 */
export function createClipCommands(
  store: ClipStore,
  initialHost: ClipboardHost,
): ClipCommands & {
  attach(): () => void
  setHost(host: ClipboardHost): void
  setVisibility(visibility: ClipVisibility): void
} {
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
  const inspected = new Map<number, Promise<ClipInspect>>()
  let inspectTimer: ReturnType<typeof setTimeout> | null = null
  /** 等待中的检视是哪一条（其他状态变化不推迟它） */
  let inspectFor: number | null = null
  let disposed = false
  /** 视图的可见程度（见 `ClipVisibility`；单独构造时按显示中处理） */
  let visibility: ClipVisibility = 'shown'
  /** 退出动画期间收到、暂存着的 `reloaded` */
  let reloadHeld = false

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
    inspected.delete(id)
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
      set((s) => ({
        query,
        search: null,
        searching: false,
        // 回到最近列表的第一条列出来的条目（收藏折叠时不落在藏起来的收藏上）
        selectedId: keepSelection({ ...s, query, search: null }, null),
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
          const filtered = { filter, hits: page.items, nextOffset: page.next_offset, loaded: true }
          return {
            filtered,
            selectedId: isSearching(s) ? s.selectedId : keepSelection({ ...s, filtered }),
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
      return {
        filter,
        selectedId: keepSelection({ ...s, filter }, null),
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

  // ----- 检视（芯片条、「粘贴为…」与「提取」子页）

  const inspect = (id: number): Promise<ClipInspect> => {
    const hit = inspected.get(id)
    if (hit) {
      inspected.delete(id)
      inspected.set(id, hit)
      return hit
    }
    const item = visibleHits(get()).find((h) => h.item.id === id)?.item
    // 图片 / 文件没有文字：不发请求
    if (item && !isTextual(item.kind)) return Promise.resolve(NO_INSPECT)
    // 结果先过一遍兜底（缺失的当作没有片段）：缓存里的 Promise 总是给出完整的结果，落 store 的回调不会再抛错
    const p = api
      .clipboardInspect(id)
      .then(inspectOrEmpty)
      .catch((error: unknown) => {
        inspected.delete(id)
        const e = WispIpcError.from('clipboard_inspect', error)
        // 识别失败不打扰用户（芯片条只是不出现）；只记错误码，不记内容
        if (e.code !== 'not_found') console.warn(`[clipboard] clipboard_inspect failed: ${e.code}`)
        return NO_INSPECT
      })
    inspected.set(id, p)
    while (inspected.size > INSPECT_CACHE) {
      const oldest = inspected.keys().next().value
      if (oldest === undefined) break
      inspected.delete(oldest)
    }
    return p
  }

  /** 检视结果落进 store（只在光标仍在那一条时）：排好显示顺序，芯片条、子页与芯片的键盘下标都按这一份 */
  const landInspect = (item: ClipItemMeta, result: ClipInspect) => {
    if (disposed || get().selectedId !== item.id) return
    const ordered = {
      entities: orderEntities(result.entities, item.kind),
      transforms: orderTransforms(result.transforms),
    }
    set({ inspect: { id: item.id, result: ordered } })
  }

  /**
   * 立即检视光标所在的条目（菜单打开前：「粘贴为」「提取」取决于它，菜单项一次到位、打开后不再增减）。不需要等待时
   * （已经检视过、图片 / 文件）返回 null，调用方同步继续
   */
  const inspectNow = (): Promise<void> | null => {
    const s = get()
    const item = selectedItem(s)
    if (!item || !isTextual(item.kind) || s.inspect?.id === item.id) return null
    if (inspectTimer !== null) clearTimeout(inspectTimer)
    inspectTimer = null
    inspectFor = null
    return inspect(item.id).then((r) => landInspect(item, r))
  }

  /** 光标停在某条上之后检视它，结果落进 store（只在光标仍在那一条时） */
  const inspectCursor = () => {
    const s = get()
    const item = selectedItem(s)
    if (item && inspectTimer !== null && inspectFor === item.id) return
    if (inspectTimer !== null) clearTimeout(inspectTimer)
    inspectTimer = null
    inspectFor = null
    if (!item || !isTextual(item.kind)) {
      if (s.inspect !== null) set({ inspect: null })
      return
    }
    if (s.inspect?.id === item.id) return
    const id = item.id
    const land = (result: ClipInspect) => landInspect(item, result)
    // 检视过的条目立即显示（同一次提交）；没有的等光标停下来再请求
    const cached = inspected.get(id)
    if (cached) {
      void cached.then(land)
      return
    }
    inspectFor = id
    inspectTimer = later(INSPECT_DEBOUNCE_MS, () => {
      inspectTimer = null
      inspectFor = null
      if (get().selectedId === id) void inspect(id).then(land)
    })
    if (s.inspect !== null) set({ inspect: null })
  }

  const enterChips = () => {
    const s = get()
    const n = s.inspect?.id === s.selectedId ? (s.inspect?.result.entities.length ?? 0) : 0
    if (n > 0) set({ chip: 0, preview: false })
  }
  const moveChip = (step: 1 | -1) => {
    const s = get()
    const n = s.inspect?.result.entities.length ?? 0
    if (s.chip === null || n === 0) return
    set({ chip: (((s.chip + step) % n) + n) % n })
  }
  const exitChips = () => {
    if (get().chip !== null) set({ chip: null })
  }

  // ----- 多选

  /** 按当前的行顺序取 `from` 到 `to` 之间（含两端）的条目，从 `from` 朝 `to` 的方向排列 */
  const rangeOf = (rows: readonly Row[], from: number, to: number): ClipItemMeta[] => {
    const a = indexOfId(rows, from)
    const b = indexOfId(rows, to)
    if (a < 0 || b < 0) return []
    const step = a <= b ? 1 : -1
    const out: ClipItemMeta[] = []
    for (let i = a; ; i += step) {
      const r = rows[i]
      if (r?.type === 'item') out.push(r.hit.item)
      if (i === b) break
    }
    return out
  }

  const toggleMark = (id: number) => {
    const s = get()
    const item = visibleHits(s).find((h) => h.item.id === id)?.item
    if (!item) return
    const marked = s.marks.some((m) => m.id === id)
    set({
      marks: marked ? s.marks.filter((m) => m.id !== id) : [...s.marks, item],
      markAnchor: id,
      selectedId: id,
      confirm: null,
      chip: null,
    })
  }

  /** ⇧ 点击：从起点（没有时是光标所在的那一条）到 `to` 的区间成为新的选择 */
  const markRange = (rows: readonly Row[], to: number) => {
    const s = get()
    const anchor = s.markAnchor ?? s.selectedId ?? to
    const marks = rangeOf(rows, anchor, to)
    if (marks.length === 0) return
    set({ marks, markAnchor: anchor, selectedId: to, confirm: null, chip: null })
  }

  /** ⇧↑ / ⇧↓：光标移一步，选择扩展（或收回）到光标 */
  const extendMarks = (rows: readonly Row[], step: 1 | -1) => {
    const s = get()
    const from = indexOfId(rows, s.selectedId)
    const next = rows[stepItem(rows, from, step)]
    if (next?.type !== 'item') return
    const anchor = s.markAnchor ?? s.selectedId ?? next.key
    const marks = rangeOf(rows, anchor, next.key)
    set({
      marks,
      markAnchor: anchor,
      selectedId: next.key,
      reveal: s.reveal + 1,
      jump: false,
      confirm: null,
      chip: null,
    })
  }

  const clearMarks = () => {
    const s = get()
    if (s.marks.length > 0 || s.markAnchor !== null) set({ marks: [], markAnchor: null })
  }

  // ----- 条目操作

  const target = (id?: number): ClipItemMeta | undefined => {
    const s = get()
    if (id === undefined) return selectedItem(s)
    return visibleHits(s).find((h) => h.item.id === id)?.item
  }

  /** 提示里的粘贴键按平台默认（Ctrl+V）；终端等目标应用的 Ctrl+Shift+V 需要 Rust 在错误里带上 */
  const pasteKey = () => formatHotkey('Mod+KeyV', host().os)

  /** 前端代写剪贴板之后的提示：面板在原位切到 toast 视图，悬浮窗在窗口里轻提示 */
  const announceCopied = (reason: 'copy_only' | 'wayland') => {
    if (host().copiedNotice === 'status')
      showStatus(copiedNotice(reason, host().os, pasteKey(), t), 'info')
    else host().announceCopied(reason, pasteKey())
  }

  /**
   * 由 WebView 写入后由宿主提示（面板在原位切到 toast 视图：面板此刻有焦点，toast 在第一次按键时让出焦点）。⌘↩ 仅复制
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
      announceCopied(mode === 'copy_only' ? 'copy_only' : 'wayland')
    })
  }

  /**
   * 粘贴管线的前端一侧（整条 / 片段 / 多条合并共用）：单飞（busy）、失败的统一处理。`conflict` = 单飞拒绝 / 本次会话
   * 已经粘贴过——静默忽略，面板还可见时不能提示「粘贴失败」；平台要求由 WebView 写剪贴板（KDE Wayland）时交给
   * `viaWebView`（没有时提示这种来源在当前会话里不可用）
   */
  const runPaste = (
    command: string,
    call: () => Promise<PasteOutcome>,
    opts: { item?: ClipItemMeta; mode: PasteMode; viaWebView?: () => void },
  ) => {
    if (get().busy) return
    set({ busy: true, confirm: null, chip: null })
    call().then(
      (outcome: PasteOutcome | undefined) => {
        set({ busy: false })
        // 面板由 Rust 在原位弹 toast；悬浮窗不隐藏，只复制的结果在窗口里轻提示
        if (outcome?.type === 'copied' && host().copiedNotice === 'status' && !disposed) {
          showStatus(copiedNotice(outcome.reason, host().os, pasteKey(), t), 'info')
        }
      },
      (error: unknown) => {
        set({ busy: false })
        const e = WispIpcError.from(command, error)
        if (e.code === 'conflict') return
        if (e.code === 'platform' && WEBVIEW_COPY_REASONS.has(e.message)) {
          webviewClipboard = true
          if (opts.viaWebView) opts.viaWebView()
          else showStatus(t('clipboard.status.webview_unsupported'), 'error')
          return
        }
        failed(command, error, {
          item: opts.item,
          action: opts.mode === 'copy_only' ? 'copy' : 'paste',
        })
      },
    )
  }

  const paste = (mode: PasteMode, id?: number, transform: TransformId | null = null) => {
    const item = target(id)
    if (!item || get().busy) return
    // 前端代写只认整条的原样内容；变换结果只有 Rust 能算
    const viaWebView =
      transform === null && webviewWritable(item) ? () => pasteViaWebView(item, mode) : undefined
    if (webviewClipboard && viaWebView) {
      set({ confirm: null })
      viaWebView()
      return
    }
    runPaste('clipboard_paste', () => api.clipboardPaste(item.id, mode, transform), {
      item,
      mode,
      viaWebView,
    })
  }

  const pasteMany = (items: readonly ClipItemMeta[], mode: PasteMode) => {
    if (items.length === 0) return
    if (items.length === 1) {
      paste(mode, items[0]?.id)
      return
    }
    runPaste(
      'clipboard_paste_many',
      () =>
        api.clipboardPasteMany(
          items.map((it) => it.id),
          mode,
        ),
      {
        mode,
      },
    )
  }

  const pasteText = (text: string, from: number | null, mode: PasteMode) => {
    if (!text) return
    runPaste('clipboard_paste_text', () => api.clipboardPasteText(text, from, mode), {
      mode,
      viaWebView: () => {
        void writeTextViaWebView(text).then((ok) => {
          if (disposed) return
          if (!ok) showStatus(t('clipboard.status.failed.copy'), 'error')
          else announceCopied(mode === 'copy_only' ? 'copy_only' : 'wayland')
        })
      },
    })
  }

  /** 乐观地把一条的新状态放进各个列表与多选标记 */
  const applyItem = (item: ClipItemMeta) =>
    set((s) => ({ ...placeItem(s, item), marks: syncMarks(s.marks, { item }) }))

  /** 失败回滚：放回原来的状态（检索结果里的高亮一并恢复） */
  const restoreItem = (item: ClipItemMeta, searchHit: ClipHit | undefined) =>
    set((s) => ({
      ...restoreHit(s, { item, highlights: searchHit?.highlights ?? [] }, searchHit),
      marks: syncMarks(s.marks, { item }),
      moves: s.moves + 1,
    }))

  const findSearchHit = (id: number) => get().search?.hits.find((h) => h.item.id === id)

  const toggleFavorite = (items: readonly ClipItemMeta[]) => {
    if (items.length === 0) return
    const on = items.some((it) => !it.pinned)
    const changed = items.filter((it) => it.pinned !== on)
    const backups = changed.map((it) => ({ item: it, searchHit: findSearchHit(it.id) }))
    set((s) => {
      // 逐条放进各个列表（同一份副本上累积，每条都看到前面几条的结果）
      const next: ClipState = { ...s }
      for (const it of changed) {
        // 收藏去掉验证码的到期时间、取消收藏去掉名称（与 Rust 一致，界面立即反映）
        const meta: ClipItemMeta = on
          ? { ...it, pinned: true, expires_at: null }
          : { ...it, pinned: false, name: null }
        Object.assign(next, placeItem(next, meta), { marks: syncMarks(next.marks, { item: meta }) })
      }
      return {
        all: next.all,
        filtered: next.filtered,
        search: next.search,
        marks: next.marks,
        selectedId: keepSelection(next),
        moves: s.moves + 1,
        reveal: s.reveal + 1,
        jump: false,
        confirm: null,
      }
    })
    for (const { item, searchHit } of backups) {
      api.clipboardPin(item.id, on).then(applyItem, (error: unknown) => {
        restoreItem(item, searchHit)
        failed('clipboard_pin', error, { item, action: on ? 'favorite' : 'unfavorite' })
      })
    }
  }

  const startRename = (id: number) => {
    const item = target(id)
    if (!item) return
    set({ rename: { id, draft: item.name ?? '' }, preview: false, confirm: null, chip: null })
  }
  const setRenameDraft = (draft: string) =>
    set((s) => (s.rename ? { rename: { ...s.rename, draft } } : {}))
  const cancelRename = () => {
    if (get().rename) set({ rename: null })
  }
  const commitRename = () => {
    const r = get().rename
    if (!r) return
    set({ rename: null })
    const item = target(r.id) ?? findSearchHit(r.id)?.item
    if (!item) return
    const name = r.draft.trim() || null
    if (name === (item.name ?? null)) return
    const searchHit = findSearchHit(item.id)
    // 命名即收藏（Rust 同时设为收藏、去掉验证码的到期时间）；清除名称不改变收藏
    const next: ClipItemMeta = name
      ? { ...item, name, pinned: true, expires_at: null }
      : { ...item, name: null }
    set((s) => ({
      ...placeItem(s, next),
      marks: syncMarks(s.marks, { item: next }),
      moves: next.pinned !== item.pinned ? s.moves + 1 : s.moves,
      reveal: s.reveal + 1,
    }))
    api.clipboardSetName(item.id, name).then(applyItem, (error: unknown) => {
      restoreItem(item, searchHit)
      failed('clipboard_set_name', error, { item, action: 'rename' })
    })
  }

  const toggleFavoritesCollapsed = () => {
    const collapsed = !get().favoritesCollapsed
    set((s) => ({
      favoritesCollapsed: collapsed,
      selectedId: keepSelection({ ...s, favoritesCollapsed: collapsed }),
      moves: s.moves + 1,
    }))
    // 只是本机的显示偏好：写不进去时下次打开回到原来的状态，不打扰用户
    api.clipboardSetFavoritesCollapsed(collapsed).catch((error: unknown) => {
      console.warn(
        `[clipboard] clipboard_set_favorites_collapsed failed: ${WispIpcError.from('clipboard_set_favorites_collapsed', error).code}`,
      )
    })
  }

  /** 删除一条（已确认）：乐观移除，失败时放回原处 */
  const deleteNow = (item: ClipItemMeta) => {
    const s = get()
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

  const remove = (force: boolean, id?: number) => {
    const item = target(id)
    if (!item) return
    const s = get()
    if (item.pinned && !force) {
      // 收藏：第一次 ⌫ 只弹出行内确认，2 s 内再按一次才删除；⌘⌫ 直接删
      if (!(s.confirm?.kind === 'delete' && s.confirm.id === item.id)) {
        set({ confirm: { kind: 'delete', id: item.id } })
        return
      }
    }
    deleteNow(item)
  }

  const removeMany = (items: readonly ClipItemMeta[], force: boolean) => {
    if (items.length === 0) return
    const s = get()
    const favorite = items.find((it) => it.pinned)
    const ids = items.map((it) => it.id)
    if (favorite && !force) {
      // 选择里有收藏：与删除一条收藏同样先确认，再按一次删除全部
      const c = s.confirm
      const same =
        c?.kind === 'delete' && c.ids?.length === ids.length && c.ids.every((x, i) => x === ids[i])
      if (!same) {
        set({ confirm: { kind: 'delete', id: favorite.id, ids } })
        return
      }
    }
    set({ marks: [], markAnchor: null })
    for (const it of items) deleteNow(it)
  }

  const cancelConfirm = () => set({ confirm: null })
  /** 读设置，更新「清空会不会同步到其他设备」；确认条已经打开时随之更新文案 */
  const prepareClear = () => {
    void api
      .settingsGet()
      .then((snap) => snap.settings.sync.enabled && snap.settings.sync.content.clipboard)
      .then(
        (synced) => {
          clearSyncs = synced
          if (disposed) return
          set((s) =>
            s.confirm?.kind === 'clear' && s.confirm.synced !== synced
              ? { confirm: { ...s.confirm, synced } }
              : {},
          )
        },
        // 读不到时沿用上一次的结果
        () => {},
      )
  }
  // 确认条立即出现（之后的按键立即是模态的），文案先用已知的结果，读到最新的设置后更新
  const askClear = () => {
    set({
      confirm: { kind: 'clear', keepPinned: true, synced: clearSyncs ?? false },
      preview: false,
    })
    prepareClear()
  }
  const setKeepPinned = (keep: boolean) =>
    set((s) => (s.confirm?.kind === 'clear' ? { confirm: { ...s.confirm, keepPinned: keep } } : {}))

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

  /** 图片送去识别文字：Rust 取原图、面板交叉淡入到 OCR 视图（之后的流程与截图识别相同） */
  const ocr = (id: number) => {
    api
      .clipboardSendTo(id, 'ocr')
      .catch((error: unknown) => failed('clipboard_send_to', error, { id, action: 'ocr' }))
  }

  const openFloat = () => {
    const s = get()
    api
      .clipboardFloatOpen(s.query, s.filter, s.selectedId)
      .catch((error: unknown) => failed('clipboard_float_open', error, { action: 'float' }))
  }

  // ----- 连续粘贴：第一条由 Rust 照常粘贴（面板随之隐藏），之后的进度经 `clipboard:queue` 推送
  const startQueue = (items: readonly ClipItemMeta[]) => {
    if (items.length < 2 || get().busy) return
    set({ busy: true, confirm: null, chip: null })
    api.clipboardQueueStart(items.map((it) => it.id)).then(
      (status) => {
        set({ busy: false, marks: [], markAnchor: null, queue: status.active ? status : null })
      },
      (error: unknown) => {
        set({ busy: false })
        const e = WispIpcError.from('clipboard_queue_start', error)
        if (e.code === 'conflict') return
        // 不可用 / 缺少权限：原因是 errors.platform.* 的 key，直接说明
        if (e.code === 'platform' || e.code === 'permission') {
          showStatus(tKey(e.message, undefined, t('clipboard.status.failed.queue')), 'error')
          return
        }
        failed('clipboard_queue_start', error, { action: 'queue' })
      },
    )
  }

  const cancelQueue = () => {
    set({ queue: null })
    api.clipboardQueueCancel().catch(() => undefined)
  }

  /** 进度：进行中的记在 store 里（工具栏的「结束连续粘贴」、菜单据此出现）；悬浮窗另在窗口里轻提示 */
  const onQueue = (status: QueueStatus) => {
    set({ queue: status.active ? status : null })
    if (host().copiedNotice === 'status') showStatus(queueNotice(status, t), 'info')
  }

  // ----- 拖出：按下时取好拖拽数据（每次按下只留一份；拖拽开始的回调里同步读取）
  let drag: { key: string; data: ClipDragData | null } | null = null
  const dragKey = (ids: readonly number[]) => ids.join(',')
  const prepareDrag = (ids: readonly number[]) => {
    if (ids.length === 0) return
    const key = dragKey(ids)
    if (drag?.key === key) return
    const entry: { key: string; data: ClipDragData | null } = { key, data: null }
    drag = entry
    api.clipboardDragData(ids).then(
      (data) => {
        entry.data = data
      },
      // 取不到（条目已删除、图片）：这次不拖，什么都不提示
      () => undefined,
    )
  }
  const dragData = (ids: readonly number[]) => (drag?.key === dragKey(ids) ? drag.data : null)

  const openSettings = () => {
    api
      .windowOpen({ kind: 'settings', section: 'clipboard' })
      .catch((error: unknown) => failed('window_open', error, { action: 'settings' }))
  }

  // ----- 实时事件

  /** 一批改动（同步带来的大量新增 / 删除）之后：重新拉取首页；当前的过滤列表与检索结果一并刷新 */
  const reloadFirstPage = () => {
    api.clipboardRecent(0, PAGE_SIZE, 'all').then(
      (page) => {
        if (disposed) return
        set((s) => {
          const all = {
            filter: 'all' as const,
            hits: page.items,
            nextOffset: page.next_offset,
            loaded: true,
          }
          return { all, selectedId: keepSelection({ ...s, all }) }
        })
      },
      (error: unknown) => failed('clipboard_recent', error, { action: 'load' }),
    )
    const s = get()
    if (s.filter !== 'all') loadFiltered(s.filter)
    if (isSearching(s)) scheduleSearch(0)
  }

  const onChanged = (change: ClipboardChanged) => {
    if (change.change === 'updated' || change.change === 'touched') invalidate(change.item.id)
    if (change.change === 'removed') invalidate(change.id)
    if (change.change === 'secret_skipped') {
      showStatus(t('clipboard.status.secret_skipped'), 'info')
      return
    }
    if (change.change === 'reloaded') {
      // 显示期间 Rust 改发逐条事件；这里收到的多半是打开前积下的一批，与显示交错时也不能把过时的行留到下次打开。
      // 只有退出动画期间不必再拉：暂存到隐藏 / 下一次打开
      if (visibility === 'leaving') reloadHeld = true
      else reloadFirstPage()
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
    pasteMany,
    pasteText,
    toggleFavorite,
    startRename,
    setRenameDraft,
    commitRename,
    cancelRename,
    toggleFavoritesCollapsed,
    remove,
    removeMany,
    toggleMark,
    markRange,
    extendMarks,
    clearMarks,
    inspect,
    inspectNow,
    enterChips,
    moveChip,
    exitChips,
    ocr,
    openFloat,
    startQueue,
    cancelQueue,
    prepareDrag,
    dragData,
    cancelConfirm,
    prepareClear,
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
    setVisibility: (next) => {
      if (next === visibility) return
      visibility = next
      // 隐藏之后（视图还在时）、或下一次打开时处理暂存的 reloaded：此刻没有人看着列表
      if (reloadHeld && (next === 'hidden' || next === 'opening')) {
        reloadHeld = false
        reloadFirstPage()
      }
    },
    attach: () => {
      disposed = false
      // 常驻订阅（面板启动时已装好；单测与 mock 里第一次 attach 时补装）按当前挂载的视图分发。每次 attach 用新的
      // 闭包：StrictMode 的「挂载 → 卸载 → 再挂载」里，上一次迟到的解绑不会把这一次的也解掉
      installClipboardEvents()
      const sink = (change: ClipboardChanged) => onChanged(change)
      changedSink = sink
      const onQueueEvent = (status: QueueStatus) => onQueue(status)
      queueSink = onQueueEvent
      // 识别设置变了（例如关掉「证件与银行卡号」）：Rust 的检视缓存已按新设置作废，这里按 id 缓存的结果同样作废，
      // 当前条目重新检视——常驻的悬浮窗里不能一直显示已关闭种类的芯片与「提取」项
      const onSmart = () => {
        inspected.clear()
        if (get().inspect !== null || get().chip !== null) set({ inspect: null, chip: null })
        inspectCursor()
      }
      smartSink = onSmart
      awaitingView = false
      if (missedChanges) {
        missedChanges = false
        reloadFirstPage()
      }
      // 光标换条目：退出芯片条的键盘焦点，停下来之后检视新的那一条（同一条目的更新使缓存失效后重新检视）
      const unsubscribe = store.subscribe((s, prev) => {
        if (s.selectedId !== prev.selectedId) {
          if (s.chip !== null) set({ chip: null })
        } else if (s.inspect !== null || visibleHits(s) === visibleHits(prev)) return
        inspectCursor()
      })
      inspectCursor()
      return () => {
        unsubscribe()
        if (changedSink === sink) changedSink = null
        if (queueSink === onQueueEvent) queueSink = null
        if (smartSink === onSmart) smartSink = null
        disposed = true
        for (const id of timers) clearTimeout(id)
        timers.clear()
        searchTimer = null
        statusTimer = null
        inspectTimer = null
        details.clear()
        settled.clear()
        heads.clear()
        inspected.clear()
      }
    },
  }
}

/** 由 WebView 写一段文字（片段粘贴的 KDE Wayland 退路） */
async function writeTextViaWebView(text: string): Promise<boolean> {
  const clip = typeof navigator === 'undefined' ? undefined : navigator.clipboard
  if (!clip) return false
  try {
    await clip.writeText(text)
    return true
  } catch {
    return false
  }
}

/** WebView 能代写的条目：文本类（纯文本）与图片（PNG）；文件条目没有可移植的写法 */
function webviewWritable(item: ClipItemMeta): boolean {
  return isTextual(item.kind) || item.kind === 'image'
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

/** 测试用：重置「本 WebView 要由前端写剪贴板」与「清空会不会同步」的记忆 */
export function resetWebviewClipboardForTest(): void {
  webviewClipboard = false
  clearSyncs = null
}
