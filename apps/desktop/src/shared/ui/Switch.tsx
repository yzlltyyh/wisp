/**
 * Switch：role="switch" 的按钮。滑块只做 transform（CSS 过渡，fast + ease.standard），
 * 打开为强调色。带 `label` 时整行可点击（命中区 ≥ 28）。
 */
import type { ReactNode } from 'react'
import { useId } from 'react'
import { cx } from './cx'

export interface SwitchProps {
  checked: boolean
  onChange: (checked: boolean) => void
  /** 可见标签（不传时必须给 aria-label） */
  label?: ReactNode
  description?: ReactNode
  'aria-label'?: string
  disabled?: boolean
  size?: 'sm' | 'md'
  className?: string
  'data-testid'?: string
  'data-force'?: string
}

export function Switch({
  checked,
  onChange,
  label,
  description,
  'aria-label': ariaLabel,
  disabled = false,
  size = 'md',
  className,
  'data-testid': testId = 'switch',
  'data-force': force,
}: SwitchProps) {
  const id = useId()
  const button = (
    <button
      type="button"
      role="switch"
      id={id}
      aria-checked={checked}
      aria-label={label ? undefined : ariaLabel}
      aria-describedby={description ? `${id}-desc` : undefined}
      disabled={disabled}
      className="ui-switch"
      data-size={size}
      data-testid={testId}
      data-force={force}
      onClick={() => onChange(!checked)}
    >
      <span className="ui-switch-knob" aria-hidden="true" />
    </button>
  )
  if (!label) return button
  return (
    <div className={cx('ui-toggle-row', className)} data-disabled={disabled || undefined}>
      <label htmlFor={id} className="ui-toggle-text">
        <span className="ui-toggle-label">{label}</span>
        {description ? (
          <span id={`${id}-desc`} className="ui-toggle-desc">
            {description}
          </span>
        ) : null}
      </label>
      {button}
    </div>
  )
}
