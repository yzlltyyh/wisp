/**
 * InlineToast：卡片内的轻提示（「已复制」），presets.toast：y 4→0 + 淡入，停留 1.5 s 后淡出。
 * `useInlineToast()` 给出节点与 show / hide：重复触发会重新计时；内容一变（编辑、重新识别）调用方可以立即收起，
 * 不等停留结束——剪贴板里已不是眼前的内容时，「已复制」不该还挂着。通过 aria-live 播报；调用方已经在别处播报了同一件事
 * （例如一句「已切换为…，已复制」）时用 `silent` 只显示、不再播报，读屏不会听到两次。
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
  /** 只显示、不播报（提示从无障碍树里隐去） */
  silent?: boolean
  className?: string
  'data-testid'?: string
}

export function InlineToast({
  show,
  children,
  silent = false,
  className,
  'data-testid': testId = 'inline-toast',
}: InlineToastProps) {
  const motion = usePreset('toast')
  return (
    <span className={cx('ui-toast-slot', className)} aria-live="polite">
      <AnimatePresence>
        {show ? (
          <m.span
            key="toast"
            className="ui-toast"
            aria-hidden={silent || undefined}
            data-testid={testId}
            {...motion}
          >
            <Icon icon={CircleCheck} size={14} className="ui-toast-icon" />
            {children}
          </m.span>
        ) : null}
      </AnimatePresence>
    </span>
  )
}

export interface InlineToastControl {
  /** 放进界面的提示（例如 StatusLine 的 `trailing`） */
  node: ReactNode
  /** 正在显示：显示期间调用方可以把同一位置的其他提示让出来（两者并排时放不下） */
  visible: boolean
  /** 显示一条提示，停留 `timing.toastHold` 后淡出；再次调用重新计时。`silent`：只显示、不播报（见模块注释） */
  show(message: ReactNode, options?: { silent?: boolean }): void
  /** 立即收起（淡出动画照常）；没有显示时什么也不做 */
  hide(): void
}

/** 带自动消失的 InlineToast */
export function useInlineToast(className?: string): InlineToastControl {
  const [message, setMessage] = useState<ReactNode>(null)
  const [visible, setVisible] = useState(false)
  const [silent, setSilent] = useState(false)
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null)
  useEffect(
    () => () => {
      if (timer.current !== null) clearTimeout(timer.current)
    },
    [],
  )
  const show = useCallback((next: ReactNode, options?: { silent?: boolean }) => {
    if (timer.current !== null) clearTimeout(timer.current)
    setMessage(next)
    setSilent(options?.silent ?? false)
    setVisible(true)
    timer.current = setTimeout(() => setVisible(false), ms(timing.toastHold))
  }, [])
  const hide = useCallback(() => {
    if (timer.current !== null) clearTimeout(timer.current)
    timer.current = null
    // 收起后保留文字：淡出动画期间仍显示原来那句
    setVisible(false)
  }, [])
  return {
    node: (
      <InlineToast key="inline-toast" show={visible} silent={silent} className={className}>
        {message}
      </InlineToast>
    ),
    visible,
    show,
    hide,
  }
}
