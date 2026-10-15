/**
 * Tooltip：悬停 500 ms 后出现（presets.tooltip，80 ms 淡入）；刚关掉一个 tooltip 后的短时间内
 * 移到相邻按钮会立即出现（与 macOS 一致，扫过工具栏时不用每个都等）。键盘聚焦时同样出现，按下指针、按任意键或失焦即消失。
 * 可带快捷键（`shortcut`，显示为反色键帽）。在面板里挂到卡片内，不会越出卡片。
 */
import { AnimatePresence, m } from 'motion/react'
import { useCallback, useEffect, useId, useRef, useState, type ReactNode } from 'react'
import { adaptPreset, presets } from '../motion/presets'
import { ms, timing } from '../motion/tokens'
import { useReducedMotionPref } from '../motion/useReducedMotionPref'
import { cx } from './cx'
import { FloatingPortal, useFloating, type Side } from './floating'
import { Kbd } from './Kbd'

export interface TooltipProps {
  content: ReactNode
  /** 快捷键组合串（显示在文案右侧） */
  shortcut?: string | null
  side?: Side
  /** 禁用（例如菜单打开时） */
  disabled?: boolean
  children: ReactNode
  className?: string
  /** 测试 / 展示页：强制显示 */
  forceOpen?: boolean
}

function clearTimer(timer: { current: ReturnType<typeof setTimeout> | null }): void {
  if (timer.current !== null) clearTimeout(timer.current)
  timer.current = null
}

/** 刚关闭一个 tooltip 后，多久内悬停另一个立即显示 */
const WARM_MS = ms(timing.hoverDelay)
let lastClosedAt = -Infinity

export function Tooltip({
  content,
  shortcut,
  side = 'top',
  disabled = false,
  children,
  className,
  forceOpen = false,
}: TooltipProps) {
  const anchor = useRef<HTMLSpanElement>(null)
  const [open, setOpen] = useState(false)
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null)
  const id = useId()
  const reduced = useReducedMotionPref()
  const shown = (open || forceOpen) && !disabled
  const {
    ref: floatingRef,
    style: floatingStyle,
    side: floatingSide,
  } = useFloating({ open: shown, anchor, side, align: 'center', offset: 6 })

  const show = useCallback(() => {
    clearTimer(timer)
    if (performance.now() - lastClosedAt < WARM_MS) setOpen(true)
    else timer.current = setTimeout(() => setOpen(true), ms(timing.tooltipDelay))
  }, [])
  const hide = useCallback(() => {
    clearTimer(timer)
    setOpen((was) => {
      if (was) lastClosedAt = performance.now()
      return false
    })
  }, [])
  useEffect(() => () => clearTimer(timer), [])
  useEffect(() => {
    if (!open) return
    const onKey = () => hide()
    window.addEventListener('keydown', onKey, true)
    return () => window.removeEventListener('keydown', onKey, true)
  }, [open, hide])

  const motion = adaptPreset(presets.tooltip, 'keep', reduced)
  return (
    <>
      <span
        ref={anchor}
        className={cx('ui-tooltip-anchor', className)}
        onPointerEnter={(e) => {
          if (e.pointerType === 'mouse') show()
        }}
        onPointerLeave={hide}
        onPointerDown={hide}
        onFocus={(e) => {
          if (e.target instanceof HTMLElement && e.target.matches(':focus-visible')) show()
        }}
        onBlur={hide}
        aria-describedby={shown ? id : undefined}
      >
        {children}
      </span>
      <FloatingPortal>
        <AnimatePresence>
          {shown ? (
            <m.div
              key="tooltip"
              ref={floatingRef}
              id={id}
              role="tooltip"
              className="ui-tooltip"
              data-side={floatingSide}
              style={floatingStyle}
              {...motion}
            >
              <span className="ui-tooltip-text">{content}</span>
              {shortcut ? <Kbd combo={shortcut} tone="inverted" /> : null}
            </m.div>
          ) : null}
        </AnimatePresence>
      </FloatingPortal>
    </>
  )
}
