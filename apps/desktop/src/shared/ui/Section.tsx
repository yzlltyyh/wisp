/**
 * Section / Field：设置页的分组布局，参照系统设置——分组标题在外，内容是一张圆角卡片，
 * 行与行之间 hairline 分隔；Field 左侧标签 + 说明、右侧控件（`stacked` 时上下排列，用于宽控件如录制器、文本框）。
 */
import type { ReactNode } from 'react'
import { useId } from 'react'
import { keepQuoted } from '../i18n/keepQuoted'
import { cx } from './cx'

export interface SectionProps {
  title?: ReactNode
  description?: ReactNode
  /** 卡片右上方的附加操作（例如「添加」） */
  action?: ReactNode
  children: ReactNode
  className?: string
  'data-testid'?: string
}

export function Section({
  title,
  description,
  action,
  children,
  className,
  'data-testid': testId,
}: SectionProps) {
  const id = useId()
  return (
    <section
      className={cx('ui-section', className)}
      aria-labelledby={title ? id : undefined}
      data-testid={testId}
    >
      {title || action ? (
        <header className="ui-section-head">
          <div>
            {title ? (
              <h2 id={id} className="ui-section-title">
                {title}
              </h2>
            ) : null}
            {description ? <p className="ui-section-desc">{description}</p> : null}
          </div>
          {action ? <div className="ui-section-action">{action}</div> : null}
        </header>
      ) : null}
      <div className="ui-section-card">{children}</div>
    </section>
  )
}

export interface FieldProps {
  label: ReactNode
  description?: ReactNode
  /** 控件（右侧，或 stacked 时在下方） */
  children: ReactNode
  stacked?: boolean
  /** 错误 / 警告提示（在说明下方） */
  message?: ReactNode
  messageTone?: 'danger' | 'warning' | 'default'
  /** 给 label 关联的控件 id */
  htmlFor?: string
  className?: string
  'data-testid'?: string
}

export function Field({
  label,
  description,
  children,
  stacked = false,
  message,
  messageTone = 'danger',
  htmlFor,
  className,
  'data-testid': testId,
}: FieldProps) {
  return (
    <div
      className={cx('ui-fieldrow', className)}
      data-stacked={stacked || undefined}
      data-testid={testId}
    >
      <div className="ui-fieldrow-text">
        <label className="ui-fieldrow-label" htmlFor={htmlFor}>
          {label}
        </label>
        {description ? <p className="ui-fieldrow-desc">{keepQuoted(description)}</p> : null}
        {message ? (
          <p
            className="ui-fieldrow-message"
            data-tone={messageTone}
            role={messageTone === 'danger' ? 'alert' : undefined}
          >
            {message}
          </p>
        ) : null}
      </div>
      <div className="ui-fieldrow-control">{children}</div>
    </div>
  )
}
