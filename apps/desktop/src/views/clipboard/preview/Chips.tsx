/**
 * 识别出的片段（芯片）：选中行右侧的芯片小条（RowChips）与全文预览里的芯片组（EntityChips）。识别在 Rust，这里只按
 * 展示注册表（shared/smart/specs.ts）画出来（芯片本身见 shared/smart/EntityChip.tsx）；点芯片 = 粘贴这个片段（⌥ 点击
 * 只复制），都经操作注册表的 paste_fragment。
 *
 * 芯片小条：
 * - 浮在选中行的右侧，与选中指示器同一个位置（列表把它放在滚动内容里、listbox 之外，随内容原生滚动；实时变化的位移里
 *   与选中行同一条曲线）。指针从行上横着移过去就点得到，路上不经过别的行——底部的芯片条要竖着穿过别的行，悬停即选中
 *   会在半路换掉条目，芯片条随之撤掉；
 * - 收起时最多 COLLAPSED 个芯片 + 「+N」（点它打开操作菜单的「提取」子页）；⌘E 进入芯片模式时向左展开、显示全部并带
 *   序号（数字键直接粘贴第 n 个），当前芯片高亮并滚进可见范围，搜索框的 aria-activedescendant 指向它；
 * - 按动作激活，不按时间：小条出现时鼠标如果已经停在它下面，单击仍是「点这一行」（用户瞄准的是原来在那里的行）；指针在
 *   小条上真的移动过（坐标变了）或从外面移进来之后，单击才是点芯片。只挡鼠标：触屏 / 笔的轻点没有悬停，读屏与键盘
 *   激活是没有指针事件的合成点击，它们点到的就是芯片。「真的移动」与文档里上一次指针事件比（不论落在哪里，含菜单），
 *   不与小条出现时记下的位置比：盖在小条上的菜单关掉时，引擎在指针原处补发的移动不算；
 * - 收起时第一个芯片在给后面两样（第二个芯片、「+N」）留出位置之后放不下完整的值（窄面板、悬浮窗最窄时）：第二个芯片
 *   也折进「+N」，不把第一个削短；
 * - 换条目时旧小条原地淡出（不随指示器滑过相邻的行）、立即不接收指针；新小条由列表在指示器到达目标行之后才给出；面板
 *   打开时等首屏的 rowIn 进场结束再淡入；
 * - 片段的值不写日志。
 */
import { AnimatePresence, m, useIsPresent, useMotionValue, type MotionValue } from 'motion/react'
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
  type PointerEvent as ReactPointerEvent,
} from 'react'
import { useStore } from 'zustand'
import type { Entity } from '../../../shared/ipc/types'
import { useT } from '../../../shared/i18n/useT'
import { usePreset } from '../../../shared/motion/presets'
import { EntityChip, useChipTip, type ChipMode } from '../../../shared/smart/EntityChip'
import type { ClipRunner } from '../actions'
import type { ClipStore } from '../store'

/** 芯片小条里芯片的 DOM id（搜索框的 aria-activedescendant 指向有键盘焦点的那一个） */
export function chipDomId(index: number): string {
  return `clip-chip-${index}`
}

/** 芯片小条的 DOM id（芯片模式时搜索框的 aria-controls 带上它：活动后代在 listbox 之外） */
export const ROW_CHIPS_ID = 'clip-rowchips'

/** 收起时最多显示几个芯片（多出的折成「+N」）：小条宽度上限 min(45%, 200px)，两个短值放得下 */
const COLLAPSED = 2
/** 有序号的芯片：数字键 1–9 */
const NUMBERED = 9

export interface RowChipsProps {
  store: ClipStore
  runner: ClipRunner
  /**
   * 小条属于哪一条：由列表判定（选中项有片段、指示器已到达选中行，没有多选 / 确认条 / 命名条 / 全文预览）；null 不显示
   */
  id: number | null
  /** 选中指示器的位置与高度（同一组 MotionValue） */
  y: MotionValue<number>
  h: MotionValue<number>
  /** 列表最近一次见到的指针坐标（还没见过时为 null）：「真的移动」的起始基线（之后由文档级的指针事件接着记） */
  pointer(): { x: number; y: number } | null
  /** 打开动效（rowIn stagger）结束的时刻（performance.now）：在这之前出现的小条等到那时再淡入 */
  holdUntil(): number
}

/** 选中行右侧的芯片小条（见文件头） */
export function RowChips(props: RowChipsProps) {
  const { store, id, pointer } = props
  const entities = useStore(store, (s) =>
    id !== null && s.inspect?.id === id ? s.inspect.result.entities : null,
  )
  // 每换一次条目换一个 key：旧小条原地淡出，新小条重新挂载；A → B → A 也不会把正在淡出的那个拉回来、从 B 滑过去
  const [slot, setSlot] = useState({ id, key: 0 })
  if (slot.id !== id) setSlot({ id, key: slot.key + 1 })

  // 防误点的基线：文档里最近两次指针事件的坐标（任何目标——菜单、头部、菜单开着时被拦下的移动都算）。引擎在布局变化时
  // （小条出现、盖在上面的菜单消失、恢复接收指针）补发的移动重复上一次指针事件的坐标，据此与真实移动区分。只在列表给出
  // 小条期间监听；开始监听时以列表最近见到的坐标为起点
  const track = useRef<{ prev: Point | null; last: Point | null }>({ prev: null, last: null })
  const tracking = id !== null
  useLayoutEffect(() => {
    if (!tracking) return
    track.current = { prev: null, last: pointer() }
    // 文档的捕获阶段：早于 React 挂在根节点上的监听，小条处理这次事件时 prev 已是它之前的那一次
    const note = (e: PointerEvent) => {
      const t = track.current
      t.prev = t.last
      t.last = { x: e.clientX, y: e.clientY }
    }
    const opts = { capture: true, passive: true }
    document.addEventListener('pointermove', note, opts)
    document.addEventListener('pointerdown', note, opts)
    return () => {
      document.removeEventListener('pointermove', note, opts)
      document.removeEventListener('pointerdown', note, opts)
    }
  }, [tracking, pointer])
  const moved = useCallback((e: ReactPointerEvent) => {
    const p = track.current.prev
    return p !== null && (p.x !== e.clientX || p.y !== e.clientY)
  }, [])

  return (
    <AnimatePresence>
      {id !== null && entities && entities.length > 0 ? (
        <Strip key={slot.key} {...props} id={id} entities={entities} moved={moved} />
      ) : null}
    </AnimatePresence>
  )
}

interface Point {
  x: number
  y: number
}

/**
 * 收起时第一个芯片给后面两样（第二个芯片、「+N」）留的位置：读 clipboard.css 的 --chips-rest-both（与 data-rest 的
 * 预留同一个值）
 */
function restBoth(strip: HTMLElement): number {
  return parseFloat(getComputedStyle(strip).getPropertyValue('--chips-rest-both')) || 0
}

function Strip({
  store,
  runner,
  id,
  entities,
  y,
  h,
  holdUntil,
  moved,
}: RowChipsProps & {
  id: number
  entities: readonly Entity[]
  /** 这次指针事件是真实的移动（坐标与文档里上一次指针事件不同） */
  moved(e: ReactPointerEvent): boolean
}) {
  const t = useT()
  const tip = useChipTip(runner.os)
  const chip = useStore(store, (s) => s.chip)
  const present = useIsPresent()
  const motion = usePreset('popover')
  const box = useRef<HTMLDivElement>(null)
  const slotRef = useRef<HTMLDivElement>(null)

  // 位置：在场时跟着选中指示器（实时变化的位移里与选中行同一条曲线）；退场时停在原处淡出，不随指示器滑到新的选中行。
  // 布局阶段解绑：列表在同一次提交的布局阶段才让指示器出发
  const sy = useMotionValue(y.get())
  const sh = useMotionValue(h.get())
  useLayoutEffect(() => {
    if (!present) return
    sy.set(y.get())
    sh.set(h.get())
    const offY = y.on('change', (v) => sy.set(v))
    const offH = h.on('change', (v) => sh.set(v))
    return () => {
      offY()
      offH()
    }
  }, [present, y, h, sy, sh])

  // 防误点（按动作激活）：在小条上收到真实的移动（坐标与文档里上一次指针事件不同）才算真的移到了小条上。引擎在布局
  // 变化、pointer-events 恢复、盖在上面的菜单消失时会在原坐标补发指针事件，坐标与上一次相同，不算
  const armed = useRef(false)
  const onPointerMove = (e: ReactPointerEvent) => {
    if (!armed.current && moved(e)) armed.current = true
  }
  // 触屏 / 笔按下：没有悬停，小条不会「冒到静止的指针下面」，按到哪里就是点哪里
  const onPointerDown = (e: ReactPointerEvent) => {
    if (e.pointerType === 'touch' || e.pointerType === 'pen') armed.current = true
  }
  /**
   * 这次单击交给列表当作「点这一行」：鼠标还没在小条上移动过（退场中的小条什么都不做，也不交给列表）。detail 为 0 的
   * 是读屏 / 键盘激活的合成点击（VoiceOver 的 AXPress、NVDA 的激活、HTMLElement.click()），前面没有指针事件，按点芯片算
   */
  const toRow = (e: ReactMouseEvent) => present && !armed.current && e.detail !== 0
  /** 小条自己处理了这次单击（激活之后点在芯片之间的空隙、退场中）：不冒泡成「点这一行」 */
  const keep = (e: ReactMouseEvent) => {
    if (!toRow(e)) e.stopPropagation()
  }
  const pick = (index: number, mode: ChipMode) => {
    if (present) runner.run('paste_fragment', runner.ctx('pointer', id), { index, mode })
  }

  // 打开动效期间出现的：等首屏的 rowIn 播完再淡入，之前不接收指针（一次性计时器）
  const [wait] = useState(() => Math.max(0, holdUntil() - performance.now()))
  const [live, setLive] = useState(wait === 0)
  useEffect(() => {
    if (live) return
    const timer = setTimeout(() => setLive(true), wait)
    return () => clearTimeout(timer)
  }, [live, wait])
  const enter = motion.animate?.transition
  const animate =
    wait > 0 && motion.animate
      ? { ...motion.animate, transition: { ...enter, delay: wait / 1000 } }
      : motion.animate

  // 选中行的标题按小条的宽度提前省略（行尾让出这么宽，见 clipboard.css 的 --clip-chips-w）：宽度在布局之后、绘制之前
  // 回报，第一帧就是对的
  useLayoutEffect(() => {
    const el = box.current
    const list = el?.closest<HTMLElement>('.ui-vlist')
    if (!el || !list || !present || typeof ResizeObserver === 'undefined') return
    const ro = new ResizeObserver(() => {
      list.style.setProperty('--clip-chips-w', `${Math.ceil(el.offsetWidth)}px`)
    })
    ro.observe(el)
    return () => ro.disconnect()
  }, [present])

  // 芯片模式里键盘移动时把当前芯片横向滚进可见范围（只滚小条自己，不连带滚动列表）
  useLayoutEffect(() => {
    const el = box.current
    const node = chip === null ? null : el?.querySelector<HTMLElement>(`#${chipDomId(chip)}`)
    if (!el || !node) return
    const left = node.offsetLeft
    const right = left + node.offsetWidth
    if (left < el.scrollLeft) el.scrollLeft = left
    else if (right > el.scrollLeft + el.clientWidth) el.scrollLeft = right - el.clientWidth
  }, [chip])

  // 收起时第一个芯片放不下完整的值：第二个芯片也折进「+N」（见文件头）。槽位的宽度就是小条能用的宽度（clipboard.css），
  // 第一个芯片完整显示要多宽 = 现在的宽度 + 被省略掉的部分；两者折不折都量得到，结果不随折叠来回变。布局之后、绘制之前
  // 量（第一帧就是对的），尺寸变了时重新量
  const focus = chip !== null
  const foldable = !focus && entities.length > COLLAPSED
  const [fold, setFold] = useState(false)
  useLayoutEffect(() => {
    const slot = slotRef.current
    const strip = box.current
    if (!foldable || !slot || !strip) {
      setFold(false)
      return
    }
    const measure = () => {
      const first = strip.querySelector<HTMLElement>('.clip-chip')
      const text = first?.querySelector<HTMLElement>('.clip-chip-text')
      // 没有排版（宽度为 0）时不折
      if (!first || !text || slot.clientWidth === 0) return setFold(false)
      const cs = getComputedStyle(strip)
      const room =
        slot.clientWidth - (parseFloat(cs.paddingLeft) || 0) - (parseFloat(cs.paddingRight) || 0)
      const natural = first.offsetWidth + Math.max(0, text.scrollWidth - text.clientWidth)
      // 宽度都是取整过的：差 1 px 以内也折，免得小数部分被省略号吃掉
      setFold(natural + 1 > room - restBoth(strip))
    }
    measure()
    if (typeof ResizeObserver === 'undefined') return
    // 槽位：缩放窗口；小条：芯片的内容或字体变了（折不折的结果只取决于这两者，不会因为折叠本身来回变）
    const ro = new ResizeObserver(measure)
    ro.observe(slot)
    ro.observe(strip)
    return () => ro.disconnect()
    // oxlint-disable-next-line react/exhaustive-effect-dependencies -- 片段变了（第一个芯片换了值）时重新量
  }, [foldable, entities])
  const shown = focus ? entities : entities.slice(0, fold && foldable ? 1 : COLLAPSED)
  const more = entities.length - shown.length
  const numbered = focus && entities.length >= 2
  // 收起时第一个芯片后面排着什么（第二个芯片、「+N」）：第一个芯片据此给它们留位置（clipboard.css）
  let rest: 'chip' | 'more' | 'both' | undefined
  if (!focus && shown.length > 1) rest = more > 0 ? 'both' : 'chip'
  else if (!focus && more > 0) rest = 'more'
  return (
    // 槽位上的指针处理只决定单击的去向（见 toRow，芯片的点击冒泡上来）；键盘经 ⌘E 进入芯片模式，焦点一直在搜索框里
    // oxlint-disable-next-line jsx-a11y/click-events-have-key-events, jsx-a11y/no-static-element-interactions
    <m.div
      ref={slotRef}
      className="clip-rowchips-slot"
      data-focus={focus || undefined}
      data-exiting={!present || undefined}
      data-hold={!live || undefined}
      style={{ y: sy, height: sh, transformOrigin: 'right center' }}
      {...motion}
      animate={animate}
      onPointerMove={onPointerMove}
      onPointerDown={onPointerDown}
      onClick={keep}
      onDoubleClick={keep}
    >
      <div
        ref={box}
        // 退场中的旧小条与新小条会同时在场：只有在场的那个带 id（搜索框的 aria-controls / activedescendant 指向它）
        id={present ? ROW_CHIPS_ID : undefined}
        className="clip-chips clip-rowchips"
        role="toolbar"
        aria-label={t('smart.a11y.chips')}
        data-focus={focus || undefined}
        data-rest={rest}
        data-testid="clip-chips"
      >
        {shown.map((e, index) => (
          <EntityChip
            key={`${e.kind}-${e.start}`}
            entity={e}
            id={present ? chipDomId(index) : undefined}
            active={chip === index}
            tip={tip}
            index={numbered && index < NUMBERED ? index + 1 : undefined}
            guard={toRow}
            onPick={(mode) => pick(index, mode)}
          />
        ))}
        {more > 0 ? (
          <button
            type="button"
            className="clip-chip clip-chip-more"
            tabIndex={-1}
            aria-label={t('smart.a11y.more', { n: more })}
            data-testid="clip-chips-more"
            onClick={(e) => {
              if (toRow(e)) return
              e.stopPropagation()
              // 「提取」：指针来源时在选中行打开操作菜单、直接到这一页（列出全部片段）
              if (present) runner.run('extract', runner.ctx('pointer', id))
            }}
          >
            {t('smart.chip.more', { n: more })}
          </button>
        ) : null}
      </div>
    </m.div>
  )
}

/**
 * 全文预览里的芯片组：作用于预览的那一条（Space 预览选中项，片段来自芯片小条的同一份检视，不另外请求）；粘贴同样经
 * 注册表。全文预览是稳定的浮层，芯片点得到（悬停卡片里不放芯片：卡片压在往下扫的路径上，里面的芯片几乎点不到）
 */
export function EntityChips({
  store,
  runner,
  id,
}: {
  store: ClipStore
  runner: ClipRunner
  /** 预览的条目 */
  id: number
}) {
  const t = useT()
  const tip = useChipTip(runner.os)
  const inspect = useStore(store, (s) => s.inspect)
  const entities = inspect?.id === id ? inspect.result.entities : []
  if (entities.length === 0) return null
  return (
    <div
      className="clip-chips clip-chips-inline"
      role="toolbar"
      aria-label={t('smart.a11y.chips')}
      data-testid="clip-preview-chips"
    >
      {entities.map((e, index) => (
        <EntityChip
          key={`${e.kind}-${e.start}`}
          entity={e}
          tip={tip}
          onPick={(mode) =>
            runner.run('paste_fragment', runner.ctx('pointer', id), { index, mode })
          }
        />
      ))}
    </div>
  )
}
