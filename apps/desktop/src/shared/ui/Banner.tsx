/**
 * Banner：卡片内的整行提示条。info（强调色）/ warning（警告色）/ paused（中性，剪贴板监听已暂停）。
 * 出现动效 presets.rowInsert（减少动效时直接出现）；可带一个操作按钮与关闭按钮。
 */
import type { LucideIcon } from 'lucide-react'
import { Info, PauseCircle, TriangleAlert, X } from 'lucide-react'
import { m } from 'motion/react'
import type { ReactNode } from 'react'
import { useT } from '../i18n/useT'
import { usePreset } from '../motion/presets'
import { cx } from './cx'
import { Icon } from './Icon'

export type BannerTone = 'info' | 'warning' | 'paused'

export interface BannerProps {
  tone?: BannerTone
  icon?: LucideIcon
  children: ReactNode
  /** 右侧操作（通常是 ghost / secondary 小按钮） */
  action?: ReactNode
  onDismiss?: () => void
  className?: string
  'data-testid'?: string
}

const ICONS: Record<BannerTone, LucideIcon> = {
  info: Info,
  warning: TriangleAlert,
  paused: PauseCircle,
}

export function Banner({
  tone = 'info',
  icon,
  children,
  action,
  onDismiss,
  className,
  'data-testid': testId = 'banner',
}: BannerProps) {
  const t = useT()
  const enter = usePreset('rowInsert')
  return (
    <m.div
      role={tone === 'warning' ? 'alert' : 'status'}
      className={cx('ui-banner', className)}
      data-tone={tone}
      data-testid={testId}
      {...enter}
    >
      <Icon icon={icon ?? ICONS[tone]} size={16} className="ui-banner-icon" />
      <div className="ui-banner-text">{children}</div>
      {action ? <div className="ui-banner-action">{action}</div> : null}
      {onDismiss ? (
        <button
          type="button"
          className="ui-banner-close"
          aria-label={t('common.a11y.dismiss')}
          onClick={onDismiss}
        >
          <Icon icon={X} size={14} />
        </button>
      ) : null}
    </m.div>
  )
}
