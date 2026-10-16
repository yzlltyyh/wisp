/**
 * InlineConfirm：行内二次确认条（「再按一次 ⌫ 删除置顶项」、清空确认）。
 * 展开动效 presets.confirmIn（高度 spring.gentle + 内容交叉淡入；减少动效时直接出现）；
 * `timeoutMs` 后自动取消（默认 2 s）。确认 / 取消由调用方的按键或按钮触发。
 *
 * 语调：danger（删除 / 清空，红底 + 红色确认）、warning（会丢掉改动但不是删除：琥珀底，确认按钮用中性的
 * secondary，例如「重新识别会覆盖修改」）、default（还原类，强调色淡底 + 主按钮）。
 */
import { TriangleAlert, type LucideIcon } from 'lucide-react'
import { AnimatePresence, m } from 'motion/react'
import { useEffect, useLayoutEffect, useRef, type ReactNode } from 'react'
import { useT } from '../i18n/useT'
import { usePreset } from '../motion/presets'
import { ms, timing } from '../motion/tokens'
import { Button } from './Button'
import { cx } from './cx'
import { Icon } from './Icon'

export interface InlineConfirmProps {
  open: boolean
  message: ReactNode
  onConfirm: () => void
  onCancel: () => void
  confirmLabel?: string
  tone?: 'danger' | 'warning' | 'default'
  /** 自动取消的时长；null 表示不自动取消 */
  timeoutMs?: number | null
  /** 是否显示按钮（只靠按键确认时可以不显示） */
  buttons?: boolean
  /** 前导图标（默认警示三角；「下载并使用」这类不是警告的确认换成对应的图标） */
  icon?: LucideIcon
  className?: string
  'data-testid'?: string
}

export function InlineConfirm({
  open,
  message,
  onConfirm,
  onCancel,
  confirmLabel,
  tone = 'danger',
  timeoutMs = ms(timing.confirmHold),
  buttons = true,
  icon = TriangleAlert,
  className,
  'data-testid': testId = 'inline-confirm',
}: InlineConfirmProps) {
  const t = useT()
  const motion = usePreset('confirmIn')
  // 调用方常传内联箭头函数：计时器只随 open / timeoutMs 重置，回调取最新的（父组件重渲染不会推迟自动取消）
  const cancel = useRef(onCancel)
  useLayoutEffect(() => {
    cancel.current = onCancel
  })
  useEffect(() => {
    if (!open || timeoutMs === null) return
    const timer = setTimeout(() => cancel.current(), timeoutMs)
    return () => clearTimeout(timer)
  }, [open, timeoutMs])
  return (
    <AnimatePresence initial={false}>
      {open ? (
        <m.div
          key="confirm"
          className={cx('ui-confirm-clip', className)}
          data-testid={testId}
          {...motion}
        >
          <div className="ui-confirm" data-tone={tone} role="alert">
            <Icon icon={icon} size={14} className="ui-confirm-icon" />
            <span className="ui-confirm-text">{message}</span>
            {buttons ? (
              <span className="ui-confirm-actions">
                <Button size="sm" variant="ghost" onClick={onCancel}>
                  {t('common.action.cancel')}
                </Button>
                <Button
                  size="sm"
                  variant={
                    tone === 'danger' ? 'danger' : tone === 'warning' ? 'secondary' : 'primary'
                  }
                  onClick={onConfirm}
                >
                  {confirmLabel ?? t('common.action.confirm')}
                </Button>
              </span>
            ) : null}
          </div>
        </m.div>
      ) : null}
    </AnimatePresence>
  )
}
