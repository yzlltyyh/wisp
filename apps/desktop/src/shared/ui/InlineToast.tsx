/**
 * InlineToast：卡片内的轻提示（「已复制」），presets.toast：y 4→0 + 淡入，停留 1.5 s 后淡出。
 * `useInlineToast()` 返回 [节点, 触发函数]，重复触发会重新计时。通过 aria-live 播报。
 */
import { CircleCheck } from 'lucide-react'
import { AnimatePresence, m } from 'motion/react'
import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react'
import { usePreset } from '../motion/presets'
import { ms, timing } from '../motion/tokens'
import { cx } from './cx'
import { Icon } from './Icon'

export interface InlineToastProps {
  show: boolean
  children: ReactNode
  className?: string
  'data-testid'?: string
}

export function InlineToast({
  show,
  children,
  className,
  'data-testid': testId = 'inline-toast',
}: InlineToastProps) {
  const motion = usePreset('toast')
  return (
    <span className={cx('ui-toast-slot', className)} aria-live="polite">
      <AnimatePresence>
        {show ? (
          <m.span key="toast" className="ui-toast" data-testid={testId} {...motion}>
            <Icon icon={CircleCheck} size={14} className="ui-toast-icon" />
            {children}
          </m.span>
        ) : null}
      </AnimatePresence>
    </span>
  )
}

/** 带自动消失的 InlineToast；返回 [节点, show(message)] */
export function useInlineToast(className?: string): [ReactNode, (message: ReactNode) => void] {
  const [message, setMessage] = useState<ReactNode>(null)
  const [visible, setVisible] = useState(false)
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null)
  useEffect(
    () => () => {
      if (timer.current !== null) clearTimeout(timer.current)
    },
    [],
  )
  const show = useCallback((next: ReactNode) => {
    if (timer.current !== null) clearTimeout(timer.current)
    setMessage(next)
    setVisible(true)
    timer.current = setTimeout(() => setVisible(false), ms(timing.toastHold))
  }, [])
  return [
    <InlineToast key="inline-toast" show={visible} className={className}>
      {message}
    </InlineToast>,
    show,
  ]
}
