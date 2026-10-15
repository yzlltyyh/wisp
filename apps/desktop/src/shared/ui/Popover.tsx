/**
 * Popover：锚定在触发器上的任意内容浮层（HoverPreview、详情气泡、选择器等）。
 * 动效 presets.popover（`preview` 为 Space 全文预览：额外 y 4→0，base）；点外面 / Esc 关闭；
 * 在面板里挂到卡片内并预留高度。受控组件：`open` + `onClose`。
 *
 * 焦点（与 Menu 一致）：
 * - `initialFocus`：打开时把焦点移进浮层（定位完成、浮层可见之后——第一次打开的那次提交里浮层还是
 *   `visibility: hidden`，focus() 会静默落空）。`'first'` = 第一个可聚焦元素，也可以给一个元素的 ref；不给时不动焦点
 *   （悬停预览）；
 * - `returnFocus`（默认 true，给了 initialFocus 时才生效）：关闭（Esc、点外面、调用方把 open 置 false：选中一项）之后焦点
 *   还在退场的浮层里、或已经落到 body 时还给触发器——键盘用户不用从头 Tab。
 */
import { AnimatePresence, m } from 'motion/react'
import { useEffect, useLayoutEffect, useRef, type ReactNode, type RefObject } from 'react'
import { useKeyScope } from '../keys/useKeyScope'
import { adaptPreset, presets } from '../motion/presets'
import { useReducedMotionPref } from '../motion/useReducedMotionPref'
import { cx } from './cx'
import { FloatingPortal, useFloating, type Align, type Side } from './floating'

export interface PopoverProps {
  open: boolean
  onClose: () => void
  anchor:
    RefObject<HTMLElement | null> | { left: number; top: number; width: number; height: number }
  children: ReactNode
  side?: Side
  align?: Align
  /** 全文预览的进入动效 */
  preview?: boolean
  /** 是否预留卡片高度（默认 true） */
  reserve?: boolean
  /** 点外面是否关闭（默认 true） */
  dismissOnOutside?: boolean
  label?: string
  /** 打开时把焦点移进浮层：第一个可聚焦元素，或指定元素 */
  initialFocus?: 'first' | RefObject<HTMLElement | null>
  /** 关闭后把焦点还给触发器（默认 true；只在给了 initialFocus 时生效） */
  returnFocus?: boolean
  className?: string
  'data-testid'?: string
}

/** 浮层里第一个可聚焦的元素 */
const FOCUSABLE =
  'input:not([disabled]), textarea:not([disabled]), select:not([disabled]), button:not([disabled]), [href], [tabindex]:not([tabindex="-1"])'

export function Popover({
  open,
  onClose,
  anchor,
  children,
  side = 'bottom',
  align = 'start',
  preview = false,
  reserve = true,
  dismissOnOutside = true,
  label,
  initialFocus,
  returnFocus = true,
  className,
  'data-testid': testId = 'popover',
}: PopoverProps) {
  const reduced = useReducedMotionPref()
  const {
    ref: floatingRef,
    style: floatingStyle,
    side: floatingSide,
    placed,
  } = useFloating({ open, anchor, side, align, offset: 6, reserve })

  // 定位完成（浮层可见）的那次提交里、绘制之前把焦点移进浮层；焦点已经在里面时不动
  useLayoutEffect(() => {
    if (!open || !placed || !initialFocus) return
    const el = floatingRef.current
    if (!el || el.contains(document.activeElement)) return
    const target =
      initialFocus === 'first' ? el.querySelector<HTMLElement>(FOCUSABLE) : initialFocus.current
    ;(target ?? el).focus({ preventScroll: true })
  }, [open, placed, initialFocus, floatingRef])

  // 关闭之后焦点还在退场的浮层里、或已经落到 body：还给触发器
  const wasOpen = useRef(open)
  useLayoutEffect(() => {
    const closing = wasOpen.current && !open
    wasOpen.current = open
    if (!closing || !initialFocus || !returnFocus || !('current' in anchor)) return
    const focused = document.activeElement
    const inside = focused instanceof Node && floatingRef.current?.contains(focused) === true
    if (inside || !focused || focused === document.body)
      anchor.current?.focus({ preventScroll: true })
  }, [open, initialFocus, returnFocus, anchor, floatingRef])
  useKeyScope('popover', [{ combo: 'Escape', allowInInput: true, run: onClose }], {
    layer: 'overlay',
    enabled: open,
  })
  useEffect(() => {
    if (!open || !dismissOnOutside) return
    const onPointer = (e: PointerEvent) => {
      const t = e.target as Node
      if (floatingRef.current?.contains(t)) return
      if ('current' in anchor && anchor.current?.contains(t)) return
      onClose()
    }
    document.addEventListener('pointerdown', onPointer, true)
    return () => document.removeEventListener('pointerdown', onPointer, true)
  }, [open, dismissOnOutside, anchor, onClose, floatingRef])
  const motion = adaptPreset(preview ? presets.previewIn : presets.popover, 'fade', reduced)
  return (
    <FloatingPortal>
      <AnimatePresence>
        {open ? (
          <m.div
            key="popover"
            ref={floatingRef}
            // 非模态浮层：原生 <dialog> 的 top layer 会越出面板卡片，这里用 role 表达语义
            // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role
            role="dialog"
            aria-label={label}
            className={cx('ui-popover', className)}
            data-side={floatingSide}
            data-testid={testId}
            // 移进焦点时没有可聚焦的子元素：浮层自己接住焦点（Esc 照常经键盘路由关闭）
            tabIndex={initialFocus ? -1 : undefined}
            style={floatingStyle}
            {...motion}
          >
            {children}
          </m.div>
        ) : null}
      </AnimatePresence>
    </FloatingPortal>
  )
}
