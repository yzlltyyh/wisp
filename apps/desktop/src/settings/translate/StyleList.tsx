/**
 * 风格列表：顺序即面板 chip 顺序与 ⌘1–9；拖动把手排序（指针拖动只改 transform，
 * 松手后 `styles_reorder`，失败时回滚）；拖到滚动区上下边缘 40 px 以内时按距离自动滚动（rAF），落点随之更新；
 * 把手获得焦点时 ↑↓ 移动（读屏播报新位置）。每行显示序号快捷键、名称、「已修改 / 自定义 / 默认」标记、
 * 绑定的模型（失效时提示回退到默认模型）与风格快捷键。
 *
 * 松手时有落位动画（FLIP 的后半段）：被拖的行从指针处滑到目标槽位（--dur-fast），浮起的底色与阴影在同一段时间里淡出
 * （::before 上切 opacity，不动画 box-shadow），结束后才提交新顺序——此时每一行的位移恰好等于它在新顺序里的位置，
 * 换顺序与去掉位移在同一帧完成，没有跳动。减少动效时直接落位。
 * 序号快捷键用组件库 Kbd 的单键帽（与快捷键页、引导一致；Windows 的「Ctrl+1」不折行），第 10 个以后留同宽的空位对齐。
 */
import { ChevronRight, GripVertical, Plus, TriangleAlert } from 'lucide-react'
import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
  type PointerEvent as ReactPointerEvent,
} from 'react'
import { usePlatform } from '../../shared/hooks/usePlatform'
import { useT } from '../../shared/i18n/useT'
import type { Provider, Style } from '../../shared/ipc/types'
import { formatHotkeyRange } from '../../shared/keys/formatHotkey'
import { duration, ms } from '../../shared/motion/tokens'
import { useReducedMotionPref } from '../../shared/motion/useReducedMotionPref'
import { Badge, Button, Icon, Kbd, Section } from '../../shared/ui'
import { modelRefState, styleName } from '../lib/styles'

/** 指针移动超过这么多像素才算开始拖动（之前的点击照常打开编辑器） */
const DRAG_SLOP_PX = 3
/** 拖到滚动区上下边缘这么近时开始自动滚动 */
const EDGE_PX = 40
/** 贴边时每帧最多滚动的像素（越靠近边缘越快） */
const EDGE_MAX_STEP_PX = 14
/** 滚动区顶部被标题条带遮住的高度（贴边判断从它下面算） */
const TOP_INSET_PX = 40
/** 拖动的行最多超出列表首尾这么多（不会被拖到列表外面） */
const OVERSHOOT_PX = 8

interface DragState {
  index: number
  startY: number
  /** 开始拖动时滚动容器的 scrollTop（自动滚动后，位移要加上滚过的距离） */
  startScroll: number
  /** 最近一次的指针位置 */
  clientY: number
  dy: number
  rowHeight: number
  target: number
  pointerId: number
  moved: boolean
  /** 已松手、正在滑向目标槽位（落位动画结束后才提交新顺序） */
  settling?: boolean
}

export function StyleList({
  styles,
  providers,
  defaultStyle,
  onOpen,
  onReorder,
  onCreate,
  creating,
}: {
  styles: readonly Style[]
  providers: readonly Provider[]
  defaultStyle: string
  onOpen: (id: string, keyboard: boolean) => void
  onReorder: (ids: string[]) => void
  onCreate: () => void
  creating: boolean
}) {
  const t = useT()
  const os = usePlatform()
  const reduced = useReducedMotionPref()
  const [drag, setDrag] = useState<DragState | null>(null)
  const settleTimer = useRef<ReturnType<typeof setTimeout> | null>(null)
  useEffect(
    () => () => {
      if (settleTimer.current !== null) clearTimeout(settleTimer.current)
    },
    [],
  )
  const [announce, setAnnounce] = useState('')
  const handles = useRef<Array<HTMLButtonElement | null>>([])
  const listRef = useRef<HTMLDivElement>(null)
  const dragRef = useRef<DragState | null>(null)
  useLayoutEffect(() => {
    dragRef.current = drag
  })

  const scroller = () => listRef.current?.closest<HTMLElement>('.st-page-scroll') ?? null

  /** 按指针位置与滚过的距离重算位移与落点 */
  const follow = (d: DragState, clientY: number): DragState => {
    const scrolled = (scroller()?.scrollTop ?? 0) - d.startScroll
    const min = -d.index * d.rowHeight - OVERSHOOT_PX
    const max = (styles.length - 1 - d.index) * d.rowHeight + OVERSHOOT_PX
    const dy = Math.max(min, Math.min(max, clientY - d.startY + scrolled))
    const moved = d.moved || Math.abs(dy) > DRAG_SLOP_PX
    const target = Math.max(0, Math.min(styles.length - 1, d.index + Math.round(dy / d.rowHeight)))
    return { ...d, clientY, dy, target, moved }
  }

  // 拖动中贴边自动滚动：rAF 循环只在拖动期间存在
  const dragging = drag?.moved ?? false
  useEffect(() => {
    if (!dragging) return
    let frame = 0
    const tick = () => {
      frame = requestAnimationFrame(tick)
      const d = dragRef.current
      const el = scroller()
      if (!d || !el) return
      const box = el.getBoundingClientRect()
      const list = listRef.current?.getBoundingClientRect()
      const top = d.clientY - (box.top + TOP_INSET_PX)
      const bottom = box.bottom - d.clientY
      let step = 0
      if (top < EDGE_PX)
        step = -Math.ceil(((EDGE_PX - Math.max(top, 0)) / EDGE_PX) * EDGE_MAX_STEP_PX)
      else if (bottom < EDGE_PX)
        step = Math.ceil(((EDGE_PX - Math.max(bottom, 0)) / EDGE_PX) * EDGE_MAX_STEP_PX)
      // 列表的首 / 尾已经在视野里：不再滚（只在列表范围内滚动）
      if (list && step > 0 && list.bottom <= box.bottom - OVERSHOOT_PX) step = 0
      if (list && step < 0 && list.top >= box.top + TOP_INSET_PX + OVERSHOOT_PX) step = 0
      if (step === 0) return
      const before = el.scrollTop
      el.scrollTop = before + step
      if (el.scrollTop !== before) setDrag(follow(d, d.clientY))
    }
    frame = requestAnimationFrame(tick)
    return () => cancelAnimationFrame(frame)
    // follow / scroller 只读 ref 与当前 styles 长度
    // eslint-disable-next-line react/exhaustive-deps
  }, [dragging])

  const move = (from: number, to: number) => {
    if (from === to || to < 0 || to >= styles.length) return
    const ids = styles.map((s) => s.id)
    const [id] = ids.splice(from, 1)
    if (id === undefined) return
    ids.splice(to, 0, id)
    onReorder(ids)
    const style = styles[from]
    if (style) setAnnounce(t('settings.styles.moved', { name: styleName(style), n: to + 1 }))
  }

  const onPointerDown = (e: ReactPointerEvent<HTMLButtonElement>, index: number) => {
    // 上一次的落位动画还没结束：不开始新的拖动
    if (e.button !== 0 || drag) return
    const row = e.currentTarget.closest('.st-style-row') as HTMLElement | null
    e.currentTarget.setPointerCapture(e.pointerId)
    setDrag({
      index,
      startY: e.clientY,
      startScroll: scroller()?.scrollTop ?? 0,
      clientY: e.clientY,
      dy: 0,
      rowHeight: row?.offsetHeight ?? 1,
      target: index,
      pointerId: e.pointerId,
      moved: false,
    })
  }
  const onPointerMove = (e: ReactPointerEvent<HTMLButtonElement>) => {
    if (!drag || drag.settling || e.pointerId !== drag.pointerId) return
    setDrag(follow(drag, e.clientY))
  }
  const onPointerUp = (e: ReactPointerEvent<HTMLButtonElement>) => {
    if (!drag || drag.settling || e.pointerId !== drag.pointerId) return
    const { index, target, moved, rowHeight } = drag
    if (!moved) {
      setDrag(null)
      return
    }
    if (reduced) {
      setDrag(null)
      move(index, target)
      return
    }
    // 落位：滑到目标槽位（其他行已经让好位），结束后再提交新顺序
    setDrag({ ...drag, dy: (target - index) * rowHeight, settling: true })
    settleTimer.current = setTimeout(() => {
      settleTimer.current = null
      setDrag(null)
      move(index, target)
    }, ms(duration.fast))
  }
  const onPointerCancel = () => {
    if (!drag?.settling) setDrag(null)
  }
  const onHandleKey = (e: ReactKeyboardEvent<HTMLButtonElement>, index: number) => {
    const to = e.key === 'ArrowUp' ? index - 1 : e.key === 'ArrowDown' ? index + 1 : null
    if (to === null) return
    e.preventDefault()
    move(index, to)
    requestAnimationFrame(() => handles.current[to]?.focus())
  }

  /** 拖动中其他行让位：落点与原位置之间的行整体移动一行 */
  const offsetOf = (i: number): number => {
    if (!drag || !drag.moved) return 0
    if (i === drag.index) return drag.dy
    if (drag.target > drag.index && i > drag.index && i <= drag.target) return -drag.rowHeight
    if (drag.target < drag.index && i < drag.index && i >= drag.target) return drag.rowHeight
    return 0
  }

  return (
    <Section
      title={t('settings.styles.title')}
      // 快捷键区间：macOS「⌘1–⌘9」，Windows / Linux「Ctrl+1–9」（formatHotkeyRange，全应用一种写法）
      description={t('settings.styles.description', {
        range: formatHotkeyRange('Mod+Digit1', 'Mod+Digit9', os),
      })}
      action={
        <Button
          size="sm"
          icon={Plus}
          loading={creating}
          onClick={onCreate}
          data-testid="style-create"
        >
          {t('settings.styles.create')}
        </Button>
      }
      data-testid="style-list"
    >
      <div ref={listRef} className="st-style-list" data-dragging={drag?.moved || undefined}>
        {styles.map((s, i) => {
          const ref = s.model_ref ? modelRefState(s.model_ref, providers) : null
          const offset = offsetOf(i)
          const lifted = drag?.moved && drag.index === i
          return (
            <div
              key={s.id}
              className="st-style-row"
              data-dragging={lifted || undefined}
              data-settling={(lifted && drag?.settling) || undefined}
              data-shifting={(drag?.moved && !lifted) || undefined}
              style={offset ? { transform: `translateY(${offset}px)` } : undefined}
              data-testid={`style-row-${s.id}`}
            >
              <button
                ref={(el) => {
                  handles.current[i] = el
                }}
                type="button"
                className="st-grip"
                aria-label={t('settings.styles.reorder', { name: styleName(s) })}
                aria-describedby="style-reorder-hint"
                onPointerDown={(e) => onPointerDown(e, i)}
                onPointerMove={onPointerMove}
                onPointerUp={onPointerUp}
                onPointerCancel={onPointerCancel}
                onKeyDown={(e) => onHandleKey(e, i)}
                data-testid={`style-grip-${s.id}`}
              >
                <Icon icon={GripVertical} size={14} />
              </button>
              <button
                type="button"
                className="st-style-main"
                onClick={(e) => onOpen(s.id, e.detail === 0)}
                data-nav-id={`style:${s.id}`}
                data-testid={`style-open-${s.id}`}
              >
                <span className="st-style-index" aria-hidden="true">
                  {/* 第 10 个以后没有序号快捷键：放一个看不见的同宽键帽，名称仍然对齐 */}
                  <Kbd
                    combo={`Mod+Digit${i < 9 ? i + 1 : 9}`}
                    variant="cap"
                    className={i < 9 ? undefined : 'st-invisible'}
                  />
                </span>
                <span className="st-style-text">
                  <span className="st-style-name">
                    {styleName(s)}
                    {s.id === defaultStyle ? (
                      <Badge tone="accent">{t('settings.styles.default')}</Badge>
                    ) : null}
                    {s.builtin && s.modified ? (
                      <Badge tone="neutral">{t('settings.styles.modified')}</Badge>
                    ) : null}
                    {!s.builtin ? (
                      <Badge tone="neutral">{t('settings.styles.custom')}</Badge>
                    ) : null}
                  </span>
                  <span className="st-style-sub">
                    {ref && ref !== 'ok' ? (
                      <span className="st-warn-inline">
                        <Icon icon={TriangleAlert} size={12} />
                        {t('settings.styles.model_fallback')}
                      </span>
                    ) : s.model_ref ? (
                      t('settings.styles.bound_model', { model: s.model_ref.model_id })
                    ) : s.builtin ? (
                      t(`styles.desc.${s.id}` as 'styles.desc.natural')
                    ) : (
                      t('styles.custom')
                    )}
                  </span>
                </span>
                {s.hotkey ? <Kbd combo={s.hotkey} /> : null}
                <Icon icon={ChevronRight} size={14} className="st-row-chevron" />
              </button>
            </div>
          )
        })}
      </div>
      <p id="style-reorder-hint" className="st-sr-only">
        {t('settings.styles.reorder_hint')}
      </p>
      <p className="st-sr-only" aria-live="polite">
        {announce}
      </p>
    </Section>
  )
}
