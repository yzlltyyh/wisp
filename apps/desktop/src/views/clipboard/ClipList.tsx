/**
 * 剪贴板列表。
 *
 * - VirtualList 只负责虚拟化与滚动容器：只渲染视口 + 上下各约一屏（overscan = 视口能放下的行数）；混合 44 / 64
 *   行高 + 28 的分组标题，行高由 `estimateSize` 精确给出（行不需要实测就能对齐）；
 * - **唯一的选中指示器**由本组件画在滚动内容层里（挂在 VirtualList 内容层的第一个子节点，随内容滚动、画在行的下面，
 *   样式沿用 `.ui-vlist-indicator`），按变化的原因选择驱动（目标是「跟手」）：
 *   · 同一列表里单步移动 / 悬停：presets.indicatorSlide（spring.snappy），从当前位置出发、不做任何位置跳变；正在朝同一方向移动时沿用当前速度
 *     （快速点按时逐帧位移连续），从静止出发或反向时给一个朝目标的初速度；提前半帧起跑——按键后的第一帧就有位移，
 *     重定向时也没有停顿的一帧；跨过分组标题当作一步（完整走 72 px）；
 *   · 按键重复（按住 ↑↓）、按页 / 到首尾、换了列表来源（过滤、检索结果、清空检索、分页）：直接到位；
 *   · 需要滚动才能露出选中项（键盘移动越过视口边缘）：直接到位——滚动本身就是运动，屏幕上的高亮停在边上，不会先朝
 *     按键的反方向跳一下；
 *   · 实时插入 / 收藏 / 删除：从选中行此刻画出来的位置出发，与行的位移同一条曲线、同一个起点，任何一帧都贴着选中行；
 * - 实时变化的位移（FLIP）：所有受影响的行（含分组标题）在同一次提交里从变化之前的**屏幕位置**平移到新位置；列表已经
 *   滚下去时先补偿滚动位置（视口顶部的内容不动），再对视口里位置变了的行做位移（例如视口中间删掉一条，下面的行合拢）。
 *   跨过其他行的那一行（收藏 / 取消收藏 / 重复复制移到顶部）抬起（不透明底、压在上面），文字不会互相叠印；新插入的行
 *   等位移走过一半、让出空位之后再淡入（rowInsert）；
 * - 被删除 / 被清空的行不会在一帧内消失：变化之前把它们的 DOM 复制成「残影」留在原处，timing.menuOut 淡出后移除，
 *   下面的行同时合拢（减少动效时直接移除）；
 * - **每次会话显示时**（面板进入、或从视图切换的暂存层换成显示中——不是挂载时：视图在 prepare / 暂存层里挂载时
 *   卡片还看不见）首屏的行依次 `rowIn`（listStagger：分组标题与下一条共用名额，第 8 个之后的可见行与第 8 个一起；
 *   WAAPI 直接作用在行内容上，不重新挂载、不经 React），选中指示器不参与，键盘不被阻塞，减少动效时不播；
 * - 悬停 300 ms 在面板内部显示预览卡片，原图只在这时请求。卡片只在指针明显想进入它时保持（指针在卡片上
 *   慢下来）：正常速度往下扫时卡片收起、卡片下面的那一行照常被选中，不会挡住指针的去路；点卡片等于点它对应的行；
 *   卡片的出现 / 消失都按 presets.popover；单击粘贴（可改为双击），右键菜单 / ⌘K 操作菜单含全部操作（菜单项、单击 /
 *   双击的粘贴都经操作注册表，见 actions/）；菜单是分页的（「粘贴为…」「提取」在同一个浮层里换页）；
 * - 多选：⌘ / Ctrl 点击切换一条、⇧ 点击从起点选到这一条；已有选择时单击也是切换（不会误粘贴一条、丢掉整个选择）。
 *   标记的序号画在行首（只是行的一个属性，不进行模型的缓存输入）；
 * - 收藏分组的标题是折叠按钮，「查看全部 N 个收藏」行切到收藏视图（都经注册表的操作）；
 * - 防误点：行的屏幕位置因为数据变化（其他设备同步来的条目插在上面、收藏、删除、重新检索）刚改变过——不到一个
 *   `--dur-base`，或在按下指针之后才变——的那一行，单击只选中、不粘贴：用户瞄准的是变化之前在那里的条目；
 * - 滚动条是覆盖式的（三端一致）：不占内容宽度，行与高亮离卡片两边恒为 6 px；滚动时 / 指针靠近右边缘时出现，可拖动；
 * - 行内确认进行中时，悬停不移动选中项（再按一次 ⌫ 只作用于发起确认的那一条）；
 * - 距离末尾不到两屏时加载下一页。
 */
import { animateSingleValue, AnimatePresence, frameData, m, useMotionValue } from 'motion/react'
import {
  startTransition,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type DragEvent as ReactDragEvent,
  type MouseEvent as ReactMouseEvent,
  type PointerEvent as ReactPointerEvent,
  type ReactNode,
  type RefObject,
} from 'react'
import { createPortal } from 'react-dom'
import { useStore } from 'zustand'
import type { ClipItemMeta, OsKind } from '../../shared/ipc/types'
import { useViewActive } from '../../shared/hooks/useViewActive'
import { useT } from '../../shared/i18n/useT'
import { adaptPreset, presets, reducedPolicy } from '../../shared/motion/presets'
import { cssEase, duration, ease, ms, timing } from '../../shared/motion/tokens'
import { useReducedMotionPref } from '../../shared/motion/useReducedMotionPref'
import { ChevronDown } from 'lucide-react'
import {
  Icon,
  PagedMenu,
  VirtualList,
  type MenuEntry,
  type MenuPage,
  type VirtualListHandle,
} from '../../shared/ui'
import type { ClipActionId, ClipRunner, MenuSurface } from './actions'
import { ClipRow } from './ClipRow'
import {
  CHIP_STRIP_INSET,
  firstItemIndex,
  HEADER_HEIGHT,
  indexOfId,
  listContentHeight,
  menuHeight,
  PREFETCH_SCREENS,
  ROW_HEIGHT,
  rowHeight,
  screenRows,
  type Row,
} from './model'
import { preloadPreview, usePreviewModule } from './previewModule'
import { isSearching, rowsOf, visibleHits, type ClipStore } from './store'
import type { ClipCommands } from './useClipboard'

export interface ClipListProps {
  store: ClipStore
  /** 数据命令：选中、分页、详情 */
  cmd: ClipCommands
  /** 操作注册表：菜单项与单击 / 双击的粘贴 */
  runner: ClipRunner
  rows: readonly Row[]
  offsets: readonly number[]
  height: number
  os: OsKind
  pasteOnDoubleClick: boolean
  /** 检索结果平铺（没有分组标题，收藏带星标） */
  flat: boolean
  /** 全文预览打开时列表淡出（保留滚动位置） */
  covered: boolean
  /** 右键菜单关闭后把焦点还给搜索框 */
  searchRef: RefObject<HTMLInputElement | null>
  /** 右键菜单需要的高度超出当前卡片时，请视图先长高（会话内只增不减） */
  ensureHeight(bottom: number): void
  /** listbox 的 id（搜索框的 aria-controls 指向它） */
  listboxId: string
  /** 本次显示的标识（会话号）：视图显示中（进入 / 已显示）时给出，其余为 null——变成新值时重放打开的 stagger */
  showKey: number | null
  /**
   * 按下可以拖出的行之后把焦点还给搜索框（它的 mousedown 没被拦下，焦点会离开搜索框）。悬浮窗不还：搜索框拿到焦点
   * 会让它临时可聚焦、抢走目标应用的键盘
   */
  refocusSearch: boolean
}

interface Hover {
  id: number
  top: number
  bottom: number
}

/** 指针打开的是右键菜单；键盘（⌘K / ⇧F10 / ContextMenu）打开的操作菜单另含视图级操作 */
function surfaceOf(via: 'pointer' | 'keyboard'): MenuSurface {
  return via === 'keyboard' ? 'command' : 'context'
}

/** 一帧（秒） */
const FRAME_S = 1 / 60
/**
 * 从静止出发时 spring 提前这么久起跑（秒，半帧）：按键平均落在两帧中间，加上这半帧，按键后的第一帧平均正好走出一帧的
 * 位移——第一帧就有响应，又不会比后面几帧大出一截
 */
const LEAD_S = FRAME_S / 2

/**
 * 新 spring 的起始时间：动画进行中（帧循环在跑）时对齐到上一帧——起点的位置与速度就是上一帧画出来的值，按键之后的
 * 第一帧正好走一帧的时长，不随按键落在两帧之间的哪个位置忽长忽短（逐帧位移连续）；帧循环空闲时提前半帧起跑
 */
function springLead(): number {
  const since = (performance.now() - frameData.timestamp) / 1000
  return since >= 0 && since < FRAME_S * 1.5 ? since : LEAD_S
}
/** 从静止出发时的初速度：每秒这么多个「位移量」（第一帧约走 1/4，overshoot 仍 < 2%） */
const LEAD_VELOCITY = 15
/** 正在同向移动时速度的下限：上面那个初速度的这么多倍（快速点按时当前速度通常已经比它大，逐帧位移连续） */
const LEAD_CARRY = 0.35
/**
 * 指针在悬停卡片上的速度低于它（px / ms）才算「想进入卡片」、保持卡片；更快的移动是在往下扫，卡片让路。
 * 正常移动鼠标约 1 px / ms，停下来看卡片时 < 0.1
 */
const CARD_INTENT_SPEED = 0.3
/** 覆盖式滚动条：滑块离列表区上下边的距离、最短长度、指针离右边缘多近时显示 */
const BAR_INSET = 4
const BAR_MIN_THUMB = 28
const BAR_NEAR = 12

/** 位移的过渡（与行的 FLIP 同一条曲线） */
const MOVE = { duration: duration.base, ease: ease.standard } as const
/**
 * 行的屏幕位置变化之后多久之内单击不粘贴（毫秒，`--dur-base`：位移的过渡就是这么长，变化还没走完时指针下的已经不是
 * 用户瞄准的那一条）
 */
const SHIFT_GUARD_MS = ms(duration.base)
/**
 * 拖出的数据要提前取：拖拽开始的回调里只能同步写。正文超过这么多字的条目（以及富文本：HTML 往往比正文大得多）
 * 等按下之后指针真的开始移动才取——普通单击粘贴不该为一次可能的拖拽经 IPC 取几 MB 的载荷；短条目取得快，按下时就取好
 */
const DRAG_EAGER_CHARS = 4096
/** 残影（被删除 / 清空的行）淡出 */
const GHOST_OUT = { duration: timing.menuOut, ease: ease.standard } as const

interface Snapshot {
  rows: readonly Row[]
  offsets: readonly number[]
  moves: number
  reveal: number
  index: number
  /** 选中项（id）与它的位置：服务端确认、缩略图更新等只换了数组、选中项没动时，进行中的动画照常走完 */
  key: number | null
  y: number
}

/** 行内容上正在进行的 FLIP（打断时从当前偏移接着走） */
interface Flip {
  offset: number
  stop(): void
}

/** 变化之前复制下来的行（残影）与它在内容层里的位置 */
interface Ghost {
  node: HTMLElement
  y: number
}

export function ClipList({
  store,
  cmd,
  runner,
  rows,
  offsets,
  height,
  os,
  pasteOnDoubleClick,
  flat,
  covered,
  searchRef,
  ensureHeight,
  listboxId,
  showKey,
  refocusSearch,
}: ClipListProps) {
  const t = useT()
  const reduced = useReducedMotionPref()
  const selectedId = useStore(store, (s) => s.selectedId)
  const fresh = useStore(store, (s) => s.fresh)
  const moves = useStore(store, (s) => s.moves)
  const reveal = useStore(store, (s) => s.reveal)
  const scrollReset = useStore(store, (s) => s.scrollReset)
  const now = useStore(store, (s) => s.now)
  const confirm = useStore(store, (s) => s.confirm)
  const paused = useStore(store, (s) => s.paused)
  const actionMenu = useStore(store, (s) => s.actionMenu)
  const marks = useStore(store, (s) => s.marks)
  const inspect = useStore(store, (s) => s.inspect)
  // 检索时行里的收藏名称按查询词高亮（预览的高亮来自 Rust）
  const nameQuery = useStore(store, (s) => (isSearching(s) ? s.query.trim() : ''))
  /** 条目 → 选择序号（从 1 开始）：每行 O(1) 取用 */
  const markOf = useMemo(() => new Map(marks.map((it, i) => [it.id, i + 1])), [marks])
  const listRef = useRef<VirtualListHandle>(null)
  const wrapRef = useRef<HTMLDivElement>(null)
  const selectedIndex = indexOfId(rows, selectedId)
  const confirmingId = confirm?.kind === 'delete' ? confirm.id : null
  const preview = usePreviewModule()

  // ----- 选中指示器：挂在 VirtualList 内容层的第一个子节点（画在行下面、随内容滚动）
  const [host] = useState(() =>
    typeof document === 'undefined' ? null : document.createElement('div'),
  )
  useLayoutEffect(() => {
    const inner = listRef.current?.element()?.firstElementChild
    if (!host || !inner) return
    host.className = 'clip-indicator-host'
    inner.prepend(host)
    return () => host.remove()
  }, [host])
  const startOf = (i: number) => (i >= 0 ? (offsets[i] ?? 0) : 0)
  const indicatorY = useMotionValue(startOf(selectedIndex))
  const indicatorH = useMotionValue(
    selectedIndex >= 0 ? rowHeight(rows[selectedIndex]) : ROW_HEIGHT,
  )
  const indicatorAnim = useRef<{ stop(): void }[]>([])
  const flips = useRef(new Map<HTMLElement, Flip>())
  /** 条目 → 它的屏幕位置最近一次因为数据变化而改变的时刻（performance.now；只留防误点窗口内的） */
  const shiftedAt = useRef(new Map<string | number, number>())

  /**
   * 记下这次提交里屏幕位置变了的条目（防误点）：新出现在列表里的、或位置与变化之前不同的。`delta` 是为了让视口内容
   * 不动而补偿的滚动距离——补偿之后屏幕上没动的行不算。用户自己的滚动不经过这里（滚动时看得见内容在动）
   */
  const markShifted = (before: Snapshot, delta: number) => {
    const at = performance.now()
    const shifted = shiftedAt.current
    for (const [key, when] of shifted) if (at - when >= SHIFT_GUARD_MS) shifted.delete(key)
    const old = new Map<string | number, number>()
    before.rows.forEach((r, i) => old.set(r.key, before.offsets[i] ?? 0))
    rows.forEach((r, i) => {
      if (r.type !== 'item') return
      const from = old.get(r.key)
      if (from === undefined || Math.abs(from + delta - (offsets[i] ?? 0)) >= 0.5)
        shifted.set(r.key, at)
    })
  }

  const stopIndicator = () => {
    for (const a of indicatorAnim.current) a.stop()
    indicatorAnim.current = []
  }
  const jumpIndicator = (y: number, h: number) => {
    stopIndicator()
    indicatorY.jump(y)
    indicatorH.jump(h)
  }
  const stopFlips = () => {
    for (const [content, f] of flips.current) {
      f.stop()
      content.style.transform = ''
    }
    flips.current.clear()
  }

  // ----- 残影：store 变化时（React 提交之前，DOM 还是旧的）把将要消失的行复制下来，提交后在原处淡出
  const pendingGhosts = useRef(new Map<string | number, Ghost>())
  const liveGhosts = useRef(new Set<HTMLElement>())
  useEffect(
    () =>
      store.subscribe((s, prev) => {
        if (s.moves === prev.moves || reduced) return
        // 换了列表来源（过滤 / 检索）不算删除：直接换
        if (s.filter !== prev.filter || isSearching(s) !== isSearching(prev)) return
        const el = listRef.current?.element()
        if (!el) return
        const before = snap.current
        const keep = new Set(rowsOf(store).map((r) => r.key))
        for (const node of el.querySelectorAll<HTMLElement>('.ui-vlist-row[data-index]')) {
          const index = Number(node.dataset.index)
          const row = before.rows[index]
          if (!row || keep.has(row.key) || pendingGhosts.current.has(row.key)) continue
          const y = before.offsets[index]
          if (y === undefined) continue
          pendingGhosts.current.set(row.key, { node: node.cloneNode(true) as HTMLElement, y })
        }
      }),
    // snap 是 ref
    // oxlint-disable-next-line react/exhaustive-deps
    [store, reduced],
  )
  const playGhosts = (scrollDelta: number) => {
    const pending = pendingGhosts.current
    if (pending.size === 0) return
    pendingGhosts.current = new Map()
    const inner = listRef.current?.element()?.firstElementChild
    if (!inner || reduced) return
    const present = new Set(rows.map((r) => r.key))
    for (const [key, g] of pending) {
      if (present.has(key)) continue
      const node = g.node
      node.classList.add('clip-ghost')
      node.removeAttribute('data-index')
      node.setAttribute('aria-hidden', 'true')
      node.inert = true
      for (const el of node.querySelectorAll('[id]')) el.removeAttribute('id')
      for (const el of node.querySelectorAll('[data-testid]')) el.removeAttribute('data-testid')
      for (const el of node.querySelectorAll('[data-selected], [aria-selected]')) {
        el.removeAttribute('data-selected')
        el.removeAttribute('aria-selected')
      }
      // 屏幕位置不变：内容层里的位置跟着补偿过的滚动距离走
      node.style.transform = `translateY(${g.y + scrollDelta}px)`
      // 画在指示器之上、所有行之下：合拢过来的行压在淡出的残影上面
      host?.after(node)
      liveGhosts.current.add(node)
      animateSingleValue(1, 0, {
        ...GHOST_OUT,
        onUpdate: (v) => {
          node.style.opacity = String(v)
        },
        onComplete: () => {
          node.remove()
          liveGhosts.current.delete(node)
        },
      })
    }
  }

  /**
   * 实时变化的位移（FLIP）。先做视口锚定（列表已经滚下去时，变化之前视口顶部的那一行在新列表里的位置差就是要补偿的
   * 滚动距离），再让视口里屏幕位置变了的行从旧位置平移过来。返回本次开始位移的行（key → 起始偏移）与补偿的滚动距离；
   * 减少动效时、`animate` 为 false 时只锚定、不位移
   */
  const playMoves = (
    before: Snapshot,
    animate: boolean,
  ): { started: Map<string | number, number>; delta: number } => {
    const started = new Map<string | number, number>()
    const el = listRef.current?.element()
    if (!el) return { started, delta: 0 }
    const top = el.scrollTop
    let delta = 0
    if (top > 1) {
      for (let i = 0; i < before.rows.length; i++) {
        const start = before.offsets[i] ?? 0
        if (start + rowHeight(before.rows[i]) <= top) continue
        const key = before.rows[i]?.key
        const j = rows.findIndex((r) => r.key === key)
        if (j < 0) continue
        const shift = (offsets[j] ?? start) - start
        if (shift !== 0) {
          el.scrollTop = top + shift
          delta = el.scrollTop - top
        }
        break
      }
    }
    if (reduced || !animate) return { started, delta }
    const old = new Map<string | number, number>()
    before.rows.forEach((r, i) => old.set(r.key, before.offsets[i] ?? 0))
    const running = flips.current
    for (const node of el.querySelectorAll<HTMLElement>('.ui-vlist-row[data-index]')) {
      const index = Number(node.dataset.index)
      const row = rows[index]
      const content = node.firstElementChild
      if (!row || !(content instanceof HTMLElement)) continue
      const from = old.get(row.key)
      if (from === undefined) continue
      const prev = running.get(content)
      prev?.stop()
      // 变化之前画在屏幕上的位置（含进行中的位移）→ 新位置
      const offset = from + delta - (offsets[index] ?? 0) + (prev?.offset ?? 0)
      if (Math.abs(offset) < 0.5) {
        content.style.transform = ''
        running.delete(content)
        continue
      }
      // 跨过其他行的那一行抬起来：不透明底、压在上面（收藏 / 取消收藏 / 重复复制移到顶部）
      const lift = Math.abs(offset) > rowHeight(row)
      if (lift) node.dataset.lift = ''
      const flip: Flip = { offset, stop: () => {} }
      const controls = animateSingleValue(offset, 0, {
        ...MOVE,
        onUpdate: (v) => {
          flip.offset = v
          content.style.transform = `translateY(${v}px)`
        },
        onComplete: () => {
          content.style.transform = ''
          delete node.dataset.lift
          if (running.get(content) === flip) running.delete(content)
        },
      })
      flip.stop = () => {
        controls.stop()
        delete node.dataset.lift
      }
      running.set(content, flip)
      content.style.transform = `translateY(${offset}px)`
      started.set(row.key, offset)
    }
    return { started, delta }
  }

  // ----- 每次提交：位移 / 残影 / 指示器（在「选中项滚进视口」之前执行：判断是否在顶部用的是变化之前的滚动位置）
  const snap = useRef<Snapshot>({
    rows,
    offsets,
    moves,
    reveal,
    index: selectedIndex,
    key: selectedId,
    y: startOf(selectedIndex),
  })
  /** 这次提交开始了实时变化的位移（「选中项滚进视口」需要滚动时据此收尾） */
  const movedNow = useRef(false)
  useLayoutEffect(() => {
    const before = snap.current
    const y = startOf(selectedIndex)
    snap.current = { rows, offsets, moves, reveal, index: selectedIndex, key: selectedId, y }
    const rowsChanged = before.rows !== rows
    const moved = rowsChanged && before.moves !== moves
    movedNow.current = false
    if (!moved) pendingGhosts.current.clear()
    // 重新检索、刷新首页这类不带位移动画的变化同样会让行换位置
    if (rowsChanged && !moved) markShifted(before, 0)
    // 实时变化：先位移与残影，再让指示器从选中行此刻画出来的位置出发、与它同一条曲线
    if (moved) {
      const el = listRef.current?.element()
      // 列表已滚下去、这次还要把选中项滚进视口（收藏 / 取消收藏后跟随条目）：随后的滚动会让位移的起点失效，
      // 只做锚定，不播放位移与残影
      const scrolledReveal = before.reveal !== reveal && !!el && el.scrollTop > 1
      const { started, delta } = playMoves(before, !scrolledReveal)
      markShifted(before, delta)
      if (scrolledReveal) pendingGhosts.current.clear()
      else playGhosts(delta)
      movedNow.current = started.size > 0
      if (selectedIndex < 0) {
        stopIndicator()
        return
      }
      const h = rowHeight(rows[selectedIndex])
      const offset = selectedId === null ? undefined : started.get(selectedId)
      if (offset === undefined) {
        // 选中行没有位移（例如删掉的是别的行）：指示器已经在它下面、或正朝它追（目标没变）就不打断，否则直接到位
        const settled = indicatorY.get() === y && indicatorH.get() === h
        if (!settled && !(indicatorAnim.current.length > 0 && before.y === y)) jumpIndicator(y, h)
        return
      }
      stopIndicator()
      indicatorY.jump(y + offset)
      const a = animateSingleValue(indicatorY, y, {
        ...MOVE,
        onComplete: () => {
          if (indicatorAnim.current[0] === a) indicatorAnim.current = []
        },
      })
      indicatorAnim.current = [a, animateSingleValue(indicatorH, h, MOVE)]
      return
    }
    if (selectedIndex < 0) {
      stopIndicator()
      return
    }
    const h = rowHeight(rows[selectedIndex])
    if (y === indicatorY.get() && h === indicatorH.get() && indicatorAnim.current.length === 0)
      return
    // 只换了数组（服务端确认收藏、缩略图 / 文件存在性的 updated、分页追加），选中项与它的位置都没变：
    // 进行中的位移 / 追随照常走完
    if (rowsChanged && before.key === selectedId && before.y === y) {
      if (indicatorAnim.current.length > 0) return
      jumpIndicator(y, h)
      return
    }
    // 换了列表来源，或按键重复、按页跳转：直接到位
    if (rowsChanged || reduced || store.getState().jump || before.index < 0) {
      jumpIndicator(y, h)
      return
    }
    if (before.index === selectedIndex && indicatorAnim.current.length > 0) return
    // 单步：presets.indicatorSlide（spring.snappy），从当前位置出发（不做位置跳变）；正在朝同一方向移动时沿用当前速度（快速点按时逐帧位移
    // 连续），从静止出发或反向时给一个朝目标的初速度；起点对齐上一帧（重定向）或提前半帧（从静止出发），第一帧就有
    // 位移，重定向时也没有停顿或加倍的一帧
    const inFlight = indicatorAnim.current.length > 0
    const carried = inFlight ? indicatorY.getVelocity() : 0
    stopIndicator()
    const from = indicatorY.get()
    const lead = (y - from) * LEAD_VELOCITY
    // 同向时沿用当前速度，以初速度的约三分之一为下限（快速点按时当前速度更大，逐帧位移连续；慢一些的连按接近静止，
    // 按「起步」处理，第一帧就走出去）
    const velocity =
      inFlight && Math.sign(carried) === Math.sign(lead)
        ? Math.sign(lead) * Math.max(Math.abs(carried), Math.abs(lead) * LEAD_CARRY)
        : lead
    const done = () => {
      if (indicatorAnim.current[0] === a) indicatorAnim.current = []
    }
    const a = animateSingleValue(indicatorY, y, {
      ...presets.indicatorSlide,
      velocity,
      onComplete: done,
    })
    const b = animateSingleValue(indicatorH, h, presets.indicatorSlide)
    const startAt = inFlight ? springLead() : LEAD_S
    a.time = startAt
    b.time = startAt
    indicatorAnim.current = [a, b]
    // 每次提交都检查（rows / 选中项 / moves 的变化都在这里分流）
  })
  useEffect(
    () => () => {
      for (const f of flips.current.values()) f.stop()
      for (const g of liveGhosts.current) g.remove()
      stopIndicator()
    },
    // 只在卸载时执行
    // oxlint-disable-next-line react/exhaustive-deps
    [],
  )

  // ----- 选中项滚进视口（键盘移动、收藏后跟随）。向上移到某组第一条时连同组标题一起露出。
  // 需要滚动时指示器直接到位（滚动本身就是运动；否则内容瞬间滚动一行、指示器还在内容坐标里从上一行追过来，屏幕上的
  // 高亮会先朝按键的反方向跳一下）；这次提交还有实时变化的位移时一并收尾（位移的起点按滚动之前算，已经失效）。
  // 只响应挂载之后的变化：挂载（尤其是在视图切换的暂存层里）时不读布局，否则 scrollTop 会逼出整个视图的同步排版
  const lastIndex = useRef(selectedIndex)
  const revealSeen = useRef(reveal)
  useLayoutEffect(() => {
    if (revealSeen.current === reveal) return
    revealSeen.current = reveal
    const index = indexOfId(rows, store.getState().selectedId)
    const prev = lastIndex.current
    lastIndex.current = index
    if (index < 0) return
    const el = listRef.current?.element()
    if (!el) return
    const before = el.scrollTop
    if (index === firstItemIndex(rows)) {
      if (before > 0) el.scrollTop = 0
    } else {
      const up = index < prev
      const header = rows[index - 1]?.type === 'group'
      listRef.current?.scrollToIndex(up && header ? index - 1 : index, 'auto')
    }
    if (Math.abs(el.scrollTop - before) < 0.5) return
    if (movedNow.current) stopFlips()
    jumpIndicator(startOf(index), rowHeight(rows[index]))
    // 只在 reveal 变化时执行
    // oxlint-disable-next-line react/exhaustive-deps
  }, [reveal])
  useLayoutEffect(() => {
    lastIndex.current = selectedIndex
  }, [selectedIndex])

  // ----- 换了列表来源：滚回顶部（同样不在挂载时执行）
  const resetSeen = useRef(scrollReset)
  useLayoutEffect(() => {
    if (resetSeen.current === scrollReset) return
    resetSeen.current = scrollReset
    const el = listRef.current?.element()
    if (el && el.scrollTop > 0) el.scrollTop = 0
  }, [scrollReset])

  // ----- 打开时的 rowIn stagger：每次会话显示时重放一次（见文件头）。在显示的那次提交里（绘制之前）开始，
  // 第一帧画出来的就是起点（opacity 0、下移 4 px），不会先整行出现再闪一下
  const active = useViewActive()
  const playedFor = useRef<number | null>(null)
  useLayoutEffect(() => {
    if (showKey === null || !active || playedFor.current === showKey) return
    playedFor.current = showKey
    const el = listRef.current?.element()
    if (el && !reduced) playOpenStagger(el, rows, offsets)
    // 只在显示时执行（rows 取那一刻的）
    // oxlint-disable-next-line react/exhaustive-deps
  }, [showKey, active])
  // 位移进行中（新出现的分组标题与新行一起延后淡入）
  const [settledMoves, setSettledMoves] = useState(moves)
  const moving = moves !== settledMoves
  useEffect(() => {
    if (!moving) return
    const id = setTimeout(() => setSettledMoves(moves), ms(duration.base))
    return () => clearTimeout(id)
  }, [moves, moving])

  // ----- ⌘n 角标：按住 ⌘ 才看得见，视图显示后空闲时再补上（不进唤出时的同步渲染与暂存层的首次排版）
  const [badges, setBadges] = useState(false)
  useEffect(() => {
    if (!active || badges) return
    const show = () => startTransition(() => setBadges(true))
    if (typeof window.requestIdleCallback !== 'function') {
      const id = setTimeout(show, 0)
      return () => clearTimeout(id)
    }
    const id = window.requestIdleCallback(show)
    return () => window.cancelIdleCallback(id)
  }, [active, badges])

  // ----- 覆盖式滚动条：不占内容宽度（行与高亮离卡片两边恒为 6 px，三端一致）。滚动时、指针靠近右边缘时、拖动时
  // 出现，停下一会儿后淡出；几何只在显示时计算（不在挂载 / 每次渲染时读布局）
  const total = listContentHeight(rows) + CHIP_STRIP_INSET
  const barRef = useRef<HTMLDivElement>(null)
  const thumbRef = useRef<HTMLDivElement>(null)
  const barTimer = useRef<ReturnType<typeof setTimeout> | null>(null)
  const thumb = useRef({ size: 0, track: 0 })
  const syncBar = useCallback(() => {
    const el = listRef.current?.element()
    const bar = barRef.current
    const knob = thumbRef.current
    if (!el || !bar || !knob) return false
    const max = total - height
    if (max <= 1) {
      bar.dataset.none = ''
      return false
    }
    delete bar.dataset.none
    const track = height - 2 * BAR_INSET
    const size = Math.max(BAR_MIN_THUMB, Math.round((track * height) / total))
    thumb.current = { size, track }
    const y = BAR_INSET + (Math.min(max, Math.max(0, el.scrollTop)) / max) * (track - size)
    knob.style.height = `${size}px`
    knob.style.transform = `translateY(${y.toFixed(1)}px)`
    return true
  }, [total, height])
  const flashBar = useCallback(() => {
    const bar = barRef.current
    if (!bar || !syncBar()) return
    bar.dataset.active = ''
    if (barTimer.current !== null) clearTimeout(barTimer.current)
    // 与轻提示相同的停留（timing.toastHold），之后淡出
    barTimer.current = setTimeout(() => {
      barTimer.current = null
      delete bar.dataset.active
    }, ms(timing.toastHold))
  }, [syncBar])
  useEffect(
    () => () => {
      if (barTimer.current !== null) clearTimeout(barTimer.current)
    },
    [],
  )
  // 内容高度变化时（实时插入、分页、删除）滑块正在显示就同步
  useLayoutEffect(() => {
    const bar = barRef.current
    if (bar && (bar.dataset.active !== undefined || bar.dataset.near !== undefined)) syncBar()
  }, [syncBar])
  const drag = useRef<{ y: number; top: number } | null>(null)
  const onThumbDown = (e: ReactPointerEvent<HTMLDivElement>) => {
    const el = listRef.current?.element()
    if (e.button !== 0 || !el) return
    e.preventDefault()
    e.stopPropagation()
    e.currentTarget.setPointerCapture(e.pointerId)
    drag.current = { y: e.clientY, top: el.scrollTop }
    if (barRef.current) barRef.current.dataset.drag = ''
  }
  const onThumbMove = (e: ReactPointerEvent<HTMLDivElement>) => {
    const d = drag.current
    const el = listRef.current?.element()
    if (!d || !el) return
    e.stopPropagation()
    const { size, track } = thumb.current
    const ratio = (total - height) / Math.max(1, track - size)
    el.scrollTop = d.top + (e.clientY - d.y) * ratio
  }
  const onThumbUp = (e: ReactPointerEvent<HTMLDivElement>) => {
    if (!drag.current) return
    drag.current = null
    if (e.currentTarget.hasPointerCapture(e.pointerId)) {
      e.currentTarget.releasePointerCapture(e.pointerId)
    }
    if (barRef.current) delete barRef.current.dataset.drag
    flashBar()
  }

  // ----- 悬停：选中 + 300 ms 后的预览卡片
  const [hover, setHover] = useState<Hover | null>(null)
  const hoverTimer = useRef<ReturnType<typeof setTimeout> | null>(null)
  /**
   * `armed`：已经见过一次指针位置。面板显示时指针恰好停在列表上，WebKit 会派发一次**合成的** pointermove（坐标与初始值
   * 不同，但指针根本没动）：第一次只记作基线、不选中，坐标真正变化之后才算悬停——否则「⌥V → ↩」粘贴的是指针下的
   * 那一行而不是最新的一条。列表随每次会话重新挂载，基线也随之重置
   */
  const pointer = useRef({ x: -1, y: -1, id: -1, t: 0, speed: 0, armed: false })
  const clearHoverTimer = () => {
    if (hoverTimer.current !== null) clearTimeout(hoverTimer.current)
    hoverTimer.current = null
  }
  const hideHover = useCallback(() => {
    if (hoverTimer.current !== null) clearTimeout(hoverTimer.current)
    hoverTimer.current = null
    pointer.current.id = -1
    setHover((h) => (h ? null : h))
  }, [])
  // 键盘移动选中项、打开预览 / 菜单 / 确认条时收起悬停卡片
  useEffect(() => hideHover(), [reveal, covered, confirm, hideHover])
  useEffect(() => () => clearHoverTimer(), [])

  // ----- 分页：距离末尾不到两屏
  const maybeLoadMore = useCallback(
    (el: HTMLElement) => {
      if (el.scrollTop + height >= total - PREFETCH_SCREENS * height) cmd.loadMore()
    },
    [cmd, height, total],
  )
  useEffect(() => {
    const el = listRef.current?.element()
    if (!el) return
    const onScroll = () => {
      maybeLoadMore(el)
      hideHover()
      flashBar()
    }
    el.addEventListener('scroll', onScroll, { passive: true })
    return () => el.removeEventListener('scroll', onScroll)
  }, [maybeLoadMore, hideHover, flashBar])
  // 选中项（键盘）走到接近末尾时也加载
  useEffect(() => {
    if (selectedIndex >= 0 && selectedIndex >= rows.length - screenRows(height) * PREFETCH_SCREENS)
      cmd.loadMore()
  }, [selectedIndex, rows.length, height, cmd])

  const rowAt = (target: EventTarget | null): HTMLElement | null =>
    target instanceof Element ? target.closest<HTMLElement>('[data-clip-id]') : null
  const inCard = (target: EventTarget | null) =>
    target instanceof Element && target.closest('.clip-hover') !== null
  const idOf = (el: HTMLElement | null) => (el ? Number(el.dataset.clipId) : null)
  /** 卡片下面的那一行（指针穿过卡片往下扫时） */
  const rowUnder = (x: number, y: number): HTMLElement | null => {
    if (typeof document.elementsFromPoint !== 'function') return null
    for (const el of document.elementsFromPoint(x, y)) {
      if (el.closest('.clip-hover')) continue
      const row = rowAt(el)
      if (row) return row
      if (el === wrapRef.current) break
    }
    return null
  }

  /** 指针是否在悬停行与卡片之间的空隙里（卡片在行下方时是行底到卡片顶，在上方时是卡片底到行顶） */
  const inBridge = (e: ReactPointerEvent, h: Hover): boolean => {
    const wrap = wrapRef.current
    const card = wrap?.querySelector<HTMLElement>('.clip-hover:not([data-exiting])')
    if (!wrap || !card) return false
    const w = wrap.getBoundingClientRect()
    const c = card.getBoundingClientRect()
    if (e.clientX < c.left || e.clientX > c.right) return false
    const y = e.clientY
    return (y >= w.top + h.bottom && y <= c.top) || (y <= w.top + h.top && y >= c.bottom)
  }

  const onPointerMove = (e: ReactPointerEvent) => {
    // 按着大条目开始移动：可能要拖出，现在取拖拽数据（见 DRAG_EAGER_CHARS）
    const lazy = lazyDrag.current
    if (lazy !== null && (e.buttons & 1) === 1) {
      lazyDrag.current = null
      cmd.prepareDrag(lazy)
    }
    if (e.pointerType === 'touch') return
    const p = pointer.current
    // 第一次只记基线（见 pointer 的说明）：面板出现时的合成事件不选中指针下的行
    if (!p.armed) {
      // 只记坐标；时间戳留空，第一次真实移动的速度按 0 起算（与之前一样，不拿合成事件的时刻算速度）
      p.armed = true
      p.x = e.clientX
      p.y = e.clientY
      return
    }
    // 只响应真实的指针移动：键盘滚动列表时，指针下经过的行会收到 pointerover 但坐标不变
    if (p.x === e.clientX && p.y === e.clientY) return
    // 指针速度（平滑过的 px / ms）：判断是想进入悬停卡片，还是正在往下扫
    const dt = e.timeStamp - p.t
    const step = Math.hypot(e.clientX - p.x, e.clientY - p.y)
    p.speed = p.t > 0 && dt > 0 ? (p.speed + Math.min(step / dt, 10)) / 2 : 0
    p.t = e.timeStamp
    p.x = e.clientX
    p.y = e.clientY
    // 指针靠近右边缘：显示滚动条
    const wrap = wrapRef.current
    const bar = barRef.current
    if (wrap && bar) {
      const near = wrap.getBoundingClientRect().right - e.clientX <= BAR_NEAR
      if (near && bar.dataset.near === undefined && syncBar()) bar.dataset.near = ''
      else if (!near && bar.dataset.near !== undefined) delete bar.dataset.near
    }
    let el: HTMLElement | null
    if (hover && !inCard(e.target) && p.speed < CARD_INTENT_SPEED && inBridge(e, hover)) {
      // 慢慢从行移向卡片、正经过两者之间的那几 px（属于相邻的行）：仍算在这一行上，卡片不收起
      clearHoverTimer()
      return
    }
    if (inCard(e.target)) {
      // 在卡片上慢下来：想看卡片，保持卡片与当前条目（不选中被盖住的行）
      if (p.speed < CARD_INTENT_SPEED) {
        clearHoverTimer()
        return
      }
      // 正常速度往下扫：卡片让路，选中卡片下面的那一行
      hideHover()
      el = rowUnder(e.clientX, e.clientY)
    } else el = rowAt(e.target)
    // 行内确认进行中：悬停不移动选中项（再按一次 ⌫ 只作用于发起确认的那一条）
    if (store.getState().confirm) return
    const id = idOf(el)
    if (id === null || !el) {
      if (p.id !== -1) hideHover()
      return
    }
    if (store.getState().selectedId !== id) cmd.select(id)
    if (p.id === id) return
    p.id = id
    clearHoverTimer()
    setHover(null)
    hoverTimer.current = setTimeout(() => {
      hoverTimer.current = null
      const area = wrapRef.current
      const row = area?.querySelector<HTMLElement>(`[data-clip-id="${id}"]`)
      if (!area || !row || pointer.current.id !== id) return
      void preloadPreview().catch(() => undefined)
      const w = area.getBoundingClientRect()
      const r = row.getBoundingClientRect()
      setHover({ id, top: r.top - w.top, bottom: r.bottom - w.top })
    }, ms(timing.hoverDelay))
  }

  // ----- 防误点（见文件头）：按下指针时记下按在哪一条上、什么时候
  const pressed = useRef<{ id: number | null; at: number } | null>(null)
  /** 按下了大条目、还没开始移动：移动时再取拖拽数据 */
  const lazyDrag = useRef<number[] | null>(null)
  const onPointerDown = (e: ReactPointerEvent) => {
    if (e.button !== 0) return
    const row = rowAt(e.target)
    pressed.current = {
      id: inCard(e.target) ? (hover?.id ?? null) : idOf(row),
      at: performance.now(),
    }
    // 拖出：拖拽开始的回调里只能同步写数据，按下时先取好（多选时拖的是整个选择）；大条目等开始移动再取
    lazyDrag.current = null
    const id = idOf(row)
    if (id === null || !row?.draggable) return
    const ids = dragIds(id)
    if (heavyDrag(ids)) lazyDrag.current = ids
    else cmd.prepareDrag(ids)
  }
  const onPointerUp = () => {
    lazyDrag.current = null
    restoreFocus()
  }

  // ----- 拖出（HTML5）：文本 / 链接 / 富文本带到别的应用；按在已选中的行上时拖出整个选择（按选择顺序合并）
  const dragIds = (id: number): number[] => {
    const marked = store.getState().marks
    return marked.some((it) => it.id === id) ? marked.map((it) => it.id) : [id]
  }
  /** 拖出的数据可能很大（见 DRAG_EAGER_CHARS） */
  const heavyDrag = (ids: readonly number[]): boolean => {
    const s = store.getState()
    const items = new Map<number, ClipItemMeta>()
    for (const h of visibleHits(s)) items.set(h.item.id, h.item)
    for (const it of s.marks) items.set(it.id, it)
    return ids.some((id) => {
      const it = items.get(id)
      return (
        it !== undefined && (it.kind === 'rich_text' || (it.char_count ?? 0) > DRAG_EAGER_CHARS)
      )
    })
  }
  const onDragStart = (e: ReactDragEvent) => {
    const id = idOf(rowAt(e.target))
    const data = id === null ? null : cmd.dragData(dragIds(id))
    // 数据还没取到（按下后立刻拖、条目已不在）：这次不拖，松开后再试一次即可
    if (!data) {
      e.preventDefault()
      return
    }
    hideHover()
    pressed.current = null
    const dt = e.dataTransfer
    dt.effectAllowed = 'copy'
    dt.setData('text/plain', data.text)
    if (data.uri) dt.setData('text/uri-list', data.uri)
    if (data.html) dt.setData('text/html', data.html)
  }
  /** 松开 / 拖拽结束：可以拖出的行没拦下 mousedown，焦点可能已经离开搜索框（面板里还回去） */
  const restoreFocus = () => {
    if (!refocusSearch) return
    const search = searchRef.current
    if (search && document.activeElement !== search) search.focus({ preventScroll: true })
  }
  /** 这次单击落在的条目刚换过位置（或按下之后才换到指针下面）：只选中、不粘贴 */
  const shiftedUnder = (id: number): boolean => {
    const down = pressed.current
    pressed.current = null
    const moved = shiftedAt.current.get(id)
    if (moved !== undefined && performance.now() - moved < SHIFT_GUARD_MS) return true
    if (!down) return false
    return down.id !== id || (moved !== undefined && moved >= down.at)
  }

  // 点卡片等于点它对应的行（单击粘贴；改为双击时双击粘贴）
  const paste = (id: number) => runner.run('paste', runner.ctx('pointer', id))
  /**
   * 带修饰键的单击是多选：⌘ / Ctrl 切换一条、⇧ 从起点选到这一条；已有选择时单击也是切换（选择状态下误点不会粘贴
   * 一条、丢掉整个选择）。返回是否按多选处理了
   */
  const markClick = (e: ReactMouseEvent, id: number): boolean => {
    const toggle = os === 'macos' ? e.metaKey : e.ctrlKey
    if (e.shiftKey) {
      runner.run('mark_range', runner.ctx('pointer', id), id)
      return true
    }
    if (toggle || store.getState().marks.length > 0) {
      runner.run('mark_toggle', runner.ctx('pointer', id), id)
      return true
    }
    return false
  }
  const onClick = (e: ReactMouseEvent) => {
    if (e.button !== 0) return
    const el = e.target instanceof Element ? e.target : null
    // 收藏分组的标题（折叠 / 展开）与「查看全部 N 个收藏」
    if (el?.closest('[data-group-toggle]')) {
      hideHover()
      runner.run('favorites_toggle', runner.ctx('pointer', null))
      return
    }
    if (el?.closest('[data-show-favorites]')) {
      hideHover()
      runner.run('show_favorites', runner.ctx('pointer', null))
      return
    }
    if (inCard(e.target)) {
      if (hover && !pasteOnDoubleClick) {
        const id = hover.id
        hideHover()
        if (markClick(e, id)) return
        if (shiftedUnder(id)) cmd.select(id)
        else paste(id)
      }
      return
    }
    const id = idOf(rowAt(e.target))
    if (id === null) return
    hideHover()
    if (markClick(e, id)) return
    if (pasteOnDoubleClick) cmd.select(id)
    else if (shiftedUnder(id)) cmd.select(id)
    else paste(id)
  }
  const onDoubleClick = (e: ReactMouseEvent) => {
    if (!pasteOnDoubleClick || e.shiftKey || (os === 'macos' ? e.metaKey : e.ctrlKey)) return
    if (store.getState().marks.length > 0) return
    const id = inCard(e.target) ? (hover?.id ?? null) : idOf(rowAt(e.target))
    if (id === null) return
    hideHover()
    paste(id)
  }

  // ----- 右键菜单 / ⌘K 操作菜单
  const [menu, setMenu] = useState<{
    id: number
    anchor: { left: number; top: number; width: number; height: number }
    side: 'top' | 'bottom'
    align: 'start' | 'end'
    via: 'pointer' | 'keyboard'
    /** 当前显示的子页（「粘贴为…」「提取」）；null 是操作菜单本身 */
    page: ClipActionId | null
    /** 换页的方向（进入子页 1，返回 -1） */
    direction: 1 | -1
  } | null>(null)
  // 第一次打开时才挂载菜单（之后保留，退出动画照常播放）；焦点由组件库的 Menu 在定位完成后移进菜单、关闭后还给搜索框
  const [menuUsed, setMenuUsed] = useState(false)
  if (menu !== null && !menuUsed) setMenuUsed(true)
  const itemsFor = useCallback(
    (id: number, via: 'pointer' | 'keyboard'): MenuEntry[] => {
      const surface = surfaceOf(via)
      const c = runner.ctx(surface, id)
      // 条目已不在列表里（刚被删除 / 同步移走）：没有可做的操作
      return c.target ? runner.menu(surface, c, t) : []
    },
    // rows / paused / marks / inspect：菜单项随列表、暂停状态、多选与检视结果更新（上下文在调用时从 store 读）
    // oxlint-disable-next-line react/exhaustive-deps
    [rows, paused, marks, inspect, t, runner],
  )
  const menuPage = useMemo<MenuPage>(() => {
    const root: MenuPage = {
      key: 'root',
      label: t('clipboard.a11y.actions'),
      items: menu ? itemsFor(menu.id, menu.via) : [],
      'data-testid': 'clip-context-menu',
    }
    if (!menu?.page) return root
    const surface = surfaceOf(menu.via)
    const back = () => setMenu((cur) => (cur ? { ...cur, page: null, direction: -1 } : cur))
    const sub = runner.page(menu.page, runner.ctx(surface, menu.id), t, back)
    // 子页没有项（例如这一条没有可用的变换）：留在操作菜单本身
    return sub && sub.items.length > 0 ? { ...sub, 'data-testid': `clip-menu-${menu.page}` } : root
    // inspect / marks：子页的项来自检视结果与多选
    // oxlint-disable-next-line react/exhaustive-deps
  }, [menu, itemsFor, inspect, marks, runner, t])
  /** 最近一次打开菜单的请求（检视结果到达之前又打开了别的，旧的作废） */
  const menuSeq = useRef(0)
  const openMenu = (
    id: number,
    x: number,
    y: number,
    via: 'pointer' | 'keyboard',
    page: ClipActionId | null = null,
  ) => {
    hideHover()
    cmd.select(id)
    // 「粘贴为」「提取」取决于检视结果：没检视过的文本条目先检视（有缓存时不等），菜单项一次到位、打开后不再增减
    const seq = ++menuSeq.current
    const wait = cmd.inspectNow()
    if (!wait) placeMenu(id, x, y, via, page)
    else
      void wait.then(() => {
        if (seq === menuSeq.current && store.getState().selectedId === id)
          placeMenu(id, x, y, via, page)
      })
  }
  const placeMenu = (
    id: number,
    x: number,
    y: number,
    via: 'pointer' | 'keyboard',
    page: ClipActionId | null,
  ) => {
    // 菜单里的操作先做准备（⌘K 操作菜单的最后一项是「清空历史…」：先读清空会不会同步到其他设备）
    const surface = surfaceOf(via)
    runner.prepare(surface, runner.ctx(surface, id))
    const card = wrapRef.current?.closest('.clip')?.getBoundingClientRect()
    // 直接打开到子页（⌥⇧↩）时按子页估算：返回行 + 分隔线 + 子页的项
    const sub = page ? runner.page(page, runner.ctx(surface, id), t, () => {}) : null
    const need =
      menuHeight(
        sub && sub.items.length > 0 ? [{}, { type: 'separator' }, ...sub.items] : itemsFor(id, via),
      ) + 8
    let side: 'top' | 'bottom' = 'bottom'
    let align: 'start' | 'end' = 'start'
    let left = x
    let top = y
    if (card) {
      const below = card.bottom - y
      const above = y - card.top
      if (need > below) {
        // 放不下时的顺序：翻到上方 → 整份上移到卡片内 → 卡片长高；只有卡片上限都放不下时才在菜单里滚动（组件库菜单的
        // 滚动阴影提示还有内容）
        const shifted = card.bottom - need - 4
        if (need <= above) side = 'top'
        else if (via === 'keyboard' && need <= card.bottom - card.top - HEADER_HEIGHT) {
          // 键盘打开的操作菜单较长：两侧都放不下时从列表区顶部展开、贴卡片右侧（整份菜单可见，不需要在菜单里滚动；
          // 选中行左边的图标与标题露在外面，看得出菜单作用于哪一条）
          top = card.top + HEADER_HEIGHT
          left = card.right
          align = 'end'
        } else if (shifted >= card.top) {
          // 右键菜单两侧都放不下、卡片本身放得下（矮剪贴板面板里右键靠下的行）：整份菜单上移到卡片内、贴卡片下沿
          // （盖住指针附近的几行），不长高、不在菜单里滚动
          top = shifted
        } else ensureHeight(y - card.top + need)
      }
    }
    setMenu({
      id,
      anchor: { left, top, width: 0, height: 0 },
      side,
      align,
      via,
      page,
      direction: 1,
    })
  }
  const onContextMenu = (e: ReactMouseEvent) => {
    e.preventDefault()
    const id = idOf(inCard(e.target) ? rowUnder(e.clientX, e.clientY) : rowAt(e.target))
    if (id === null) return
    openMenu(id, e.clientX, e.clientY, 'pointer')
  }
  // ⌘K / ⇧F10 / ContextMenu / ⌥⇧↩（视图的键位表递增 actionMenu，可带上要直接打开的子页）：锚在选中行的标题起点
  // 下方，与指针右键菜单同一套上下翻转规则；选中行不在渲染范围里时（滚出去了）锚在列表区顶部。菜单已经打开时
  // （菜单里选了「粘贴为…」）只换页
  const actionSeen = useRef(actionMenu)
  useEffect(() => {
    if (actionSeen.current === actionMenu) return
    actionSeen.current = actionMenu
    const page = store.getState().actionPage
    if (menu && page) {
      setMenu({ ...menu, page, direction: 1 })
      return
    }
    const id = store.getState().selectedId
    const wrap = wrapRef.current
    if (id === null || !wrap) return
    const row = wrap.querySelector<HTMLElement>(`[data-clip-id="${id}"]`)
    const r = (row ?? wrap).getBoundingClientRect()
    openMenu(id, r.left + 36, row ? r.bottom - 6 : r.top + 8, 'keyboard', page)
    // 只响应 actionMenu 的变化
    // oxlint-disable-next-line react/exhaustive-deps
  }, [actionMenu])

  const renderItem = useCallback(
    (i: number) => {
      const r = rows[i]
      if (!r) return null
      if (r.type === 'group') {
        return (
          <RowShell appear={moving && !reduced ? 'insert' : null}>
            {r.fold ? (
              <FavoritesHeader count={r.fold.count} collapsed={r.fold.collapsed} />
            ) : (
              <div className="clip-group" data-group={r.group} role="presentation">
                {t(`clipboard.group.${r.group}`)}
              </div>
            )}
          </RowShell>
        )
      }
      if (r.type === 'more') {
        return (
          <RowShell appear={moving && !reduced ? 'insert' : null}>
            <div className="clip-more" role="presentation">
              {/* 点击由列表的事件委托处理（data-show-favorites）；键盘经 Tab 切到「收藏」过滤 */}
              <button
                type="button"
                className="clip-more-button"
                tabIndex={-1}
                data-show-favorites=""
                data-testid="clip-show-favorites"
              >
                {t('clipboard.group.more', { n: r.count })}
              </button>
            </div>
          </RowShell>
        )
      }
      return (
        <RowShell appear={fresh.has(r.key) && !reduced ? 'insert' : null}>
          <ClipRow
            hit={r.hit}
            group={r.group}
            ordinal={r.ordinal}
            badge={badges && r.ordinal <= 9 ? r.ordinal : null}
            selected={r.key === selectedId}
            mark={markOf.get(r.key) ?? null}
            now={now}
            confirming={r.key === confirmingId}
            flat={flat}
            query={r.hit.item.name ? nameQuery : ''}
            os={os}
          />
        </RowShell>
      )
    },
    [
      rows,
      selectedId,
      markOf,
      now,
      fresh,
      confirmingId,
      badges,
      t,
      flat,
      nameQuery,
      os,
      moving,
      reduced,
    ],
  )
  const estimateSize = useCallback((i: number) => rowHeight(rows[i]), [rows])
  const getItemKey = useCallback((i: number) => rows[i]?.key ?? i, [rows])

  const hoverHit =
    hover && !covered ? rows.find((r) => r.type === 'item' && r.key === hover.id) : undefined
  const HoverCard = preview?.HoverCard

  return (
    // 列表的键盘操作由操作注册表派生的键位层提供（↑↓ / ↩ / ⌘P / ⌫ / Space…，见 actions/registry.ts），这里只处理指针：
    // 悬停选中、单击 / 双击粘贴、右键菜单（事件委托，行组件保持纯展示、可 memo）。按下时焦点不离开搜索框由视图的
    // 根节点统一处理（focus.ts 的 keepSearchFocus）
    // oxlint-disable-next-line jsx-a11y/click-events-have-key-events, jsx-a11y/no-static-element-interactions
    <div
      ref={wrapRef}
      className="clip-listwrap"
      data-covered={covered || undefined}
      onPointerMove={onPointerMove}
      onPointerLeave={() => {
        hideHover()
        if (barRef.current) delete barRef.current.dataset.near
      }}
      onWheel={(e) => {
        // 卡片不滚动：在卡片上滚轮时收起卡片，之后的滚轮落到列表上
        if (inCard(e.target)) hideHover()
      }}
      onPointerDown={onPointerDown}
      onPointerUp={onPointerUp}
      onDragStart={onDragStart}
      onDragEnd={restoreFocus}
      onClick={onClick}
      onDoubleClick={onDoubleClick}
      onContextMenu={onContextMenu}
    >
      <VirtualList
        ref={listRef}
        id={listboxId}
        className="clip-list"
        count={rows.length}
        estimateSize={estimateSize}
        getItemKey={getItemKey}
        renderItem={renderItem}
        height={height}
        endInset={CHIP_STRIP_INSET}
        selectedIndex={-1}
        overscan={screenRows(height)}
        aria-label={t('clipboard.a11y.list')}
        data-testid="clip-list"
      />
      <div ref={barRef} className="clip-scrollbar" aria-hidden="true" data-none="">
        <div
          ref={thumbRef}
          className="clip-thumb"
          data-testid="clip-thumb"
          onPointerDown={onThumbDown}
          onPointerMove={onThumbMove}
          onPointerUp={onThumbUp}
          onPointerCancel={onThumbUp}
          onWheel={(e) => {
            // 滑块不在滚动容器里：在它上面的滚轮转给列表
            const el = listRef.current?.element()
            if (el) el.scrollTop += e.deltaMode === 1 ? e.deltaY * ROW_HEIGHT : e.deltaY
          }}
        />
      </div>
      {host && selectedIndex >= 0
        ? createPortal(
            <m.div
              className="ui-vlist-indicator clip-indicator"
              aria-hidden="true"
              data-testid="clip-indicator"
              style={{ y: indicatorY, height: indicatorH }}
            />,
            host,
          )
        : null}
      <AnimatePresence>
        {HoverCard && hoverHit?.type === 'item' && hover ? (
          <HoverCard
            key={hover.id}
            hit={hoverHit.hit}
            anchorTop={hover.top}
            anchorBottom={hover.bottom}
            areaHeight={height}
            now={now}
            cmd={cmd}
            runner={runner}
            store={store}
          />
        ) : null}
      </AnimatePresence>
      {menuUsed ? (
        <PagedMenu
          open={menu !== null}
          anchor={menu?.anchor ?? NO_ANCHOR}
          side={menu?.side ?? 'bottom'}
          align={menu?.align ?? 'start'}
          flip={false}
          page={menuPage}
          direction={menu?.direction ?? 1}
          initialActive={menu?.via === 'keyboard' || menu?.page ? 'first' : 'none'}
          returnFocus={searchRef}
          onClose={() => setMenu(null)}
        />
      ) : null}
    </div>
  )
}

const NO_ANCHOR = { left: 0, top: 0, width: 0, height: 0 } as const

/**
 * 「收藏」分组的标题：折叠按钮（`aria-expanded`），折叠时在标题后面给出条数。点击由列表的事件委托处理
 * （data-group-toggle，执行注册表的 favorites_toggle）；键盘从 ⌘K / ⋯ 菜单里的「折叠收藏」到达，按钮本身不进 Tab 顺序
 * （焦点一直在搜索框里）
 */
function FavoritesHeader({ count, collapsed }: { count: number; collapsed: boolean }) {
  const t = useT()
  return (
    <div className="clip-group" data-group="pinned" role="presentation">
      <button
        type="button"
        className="clip-group-toggle"
        tabIndex={-1}
        aria-expanded={!collapsed}
        aria-label={t('clipboard.a11y.favorites', { count, n: count })}
        data-group-toggle=""
        data-collapsed={collapsed || undefined}
        data-testid="clip-favorites-toggle"
      >
        {t('clipboard.group.pinned')}
        {collapsed ? <span className="clip-group-count">{count}</span> : null}
        <Icon icon={ChevronDown} size={12} className="clip-group-chevron" />
      </button>
    </div>
  )
}

/** rowIn 的起点位移（px，presets.rowIn.initial.y） */
const ROW_IN_SHIFT = Number((presets.rowIn.initial as { y?: number }).y ?? 0)

/**
 * 打开时的 stagger（presets.rowIn + listStagger）：对视口里的行内容逐个播一次「淡入 + 下移 4 px → 0」，第 n 个可选行
 * 延后 n × 12 ms（分组标题与它下面那一条同时，第 8 个之后的与第 8 个一起）。WAAPI、`fill: backwards`：等待期间停在起点，
 * 播完不留样式；作用在行内容（FLIP 位移的那一层的子节点）上，与实时变化的位移互不干扰
 */
function playOpenStagger(
  list: HTMLElement,
  rows: readonly Row[],
  offsets: readonly number[],
): void {
  const t = presets.rowIn.transition as { duration: number; ease: readonly number[] }
  const timing_: KeyframeAnimationOptions = {
    duration: ms(t.duration),
    easing: cssEase(t.ease),
    fill: 'backwards',
  }
  const keyframes: Keyframe[] = [
    { opacity: 0, transform: `translateY(${ROW_IN_SHIFT}px)` },
    { opacity: 1, transform: 'none' },
  ]
  const last = presets.listStagger.maxItems - 1
  const step = ms(presets.listStagger.staggerChildren)
  const bottom = list.scrollTop + list.clientHeight
  for (const node of list.querySelectorAll<HTMLElement>('.ui-vlist-row[data-index]')) {
    const index = Number(node.dataset.index)
    const row = rows[index]
    const target = node.firstElementChild?.firstElementChild
    if (!row || !(target instanceof HTMLElement) || typeof target.animate !== 'function') continue
    // 只播首屏（overscan 的行看不见）；行在视口下方的不动
    if ((offsets[index] ?? 0) >= bottom) continue
    const next = rows[index + 1]
    const order =
      row.type === 'item' ? row.ordinal - 1 : next?.type === 'item' ? next.ordinal - 1 : 0
    target.animate(keyframes, { ...timing_, delay: Math.min(order, last) * step })
  }
}

/**
 * 行外层：实时插入的进入动效在挂载时决定一次（之后不变，元素类型稳定，内容不会因此重新挂载）——新行 / 新出现的分组
 * 标题等已有行的位移走过一半（让出空位）再 rowInsert，任何一帧都不会与正在下移的旧行叠印。打开时的 stagger 不在这里
 * （playOpenStagger，每次会话显示时重放）。外层的普通 div 是 FLIP 位移的对象（行内容层，VirtualList 的定位 transform
 * 不受影响）。
 */
function RowShell({ appear, children }: { appear: 'insert' | null; children: ReactNode }) {
  const [mode] = useState(appear)
  const reduced = useReducedMotionPref()
  if (mode === null) return children
  const p = adaptPreset(presets.rowInsert, reducedPolicy.rowInsert, reduced)
  const enter = p.animate?.transition ?? {}
  return (
    <m.div
      initial={p.initial}
      animate={{ ...p.animate, transition: { ...enter, delay: duration.base / 2 } }}
    >
      {children}
    </m.div>
  )
}
