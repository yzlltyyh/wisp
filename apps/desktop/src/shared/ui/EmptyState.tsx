/**
 * EmptyState：空列表 / 无结果 / 未识别到文字。图标在淡底圆形里，标题 + 一句说明 + 可选操作。
 * 出现动效 presets.cardIn。
 */
import type { LucideIcon } from 'lucide-react'
import { Inbox } from 'lucide-react'
import { m } from 'motion/react'
import type { ReactNode } from 'react'
import { keepQuoted } from '../i18n/keepQuoted'
import { usePreset } from '../motion/presets'
import { cx } from './cx'
import { Icon } from './Icon'

export interface EmptyStateProps {
  icon?: LucideIcon
  title: ReactNode
  detail?: ReactNode
  action?: ReactNode
  /** compact：嵌在面板里的紧凑版（较小的上下留白） */
  compact?: boolean
  className?: string
  'data-testid'?: string
}

export function EmptyState({
  icon = Inbox,
  title,
  detail,
  action,
  compact = false,
  className,
  'data-testid': testId = 'empty-state',
}: EmptyStateProps) {
  const motion = usePreset('cardIn')
  return (
    <m.div
      className={cx('ui-empty', className)}
      data-compact={compact || undefined}
      data-testid={testId}
      {...motion}
    >
      <span className="ui-empty-icon" aria-hidden="true">
        <Icon icon={icon} size={20} />
      </span>
      <p className="ui-empty-title">{keepQuoted(title)}</p>
      {detail ? <p className="ui-empty-detail">{keepQuoted(detail)}</p> : null}
      {action ? <div className="ui-empty-action">{action}</div> : null}
    </m.div>
  )
}
