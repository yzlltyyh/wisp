/** Badge：小号状态标签（18 px 高、caption 字号），淡底 + 保证对比度的文字色。 */
import type { ReactNode } from 'react'
import { cx } from './cx'

export type Tone = 'neutral' | 'accent' | 'success' | 'warning' | 'danger'

export interface BadgeProps {
  tone?: Tone
  children: ReactNode
  /** 前置圆点（状态指示） */
  dot?: boolean
  className?: string
  'data-testid'?: string
}

export function Badge({ tone = 'neutral', children, dot = false, className, ...rest }: BadgeProps) {
  return (
    <span className={cx('ui-badge', className)} data-tone={tone} data-testid={rest['data-testid']}>
      {dot ? <span className="ui-badge-dot" aria-hidden="true" /> : null}
      {children}
    </span>
  )
}
