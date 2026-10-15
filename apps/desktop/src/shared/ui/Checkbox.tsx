/**
 * Checkbox：原生 checkbox（键盘与无障碍语义完整）+ 自绘方框；支持不确定态。
 */
import { Check, Minus } from 'lucide-react'
import { useEffect, useRef, type ReactNode } from 'react'
import { cx } from './cx'
import { Icon } from './Icon'

export interface CheckboxProps {
  checked: boolean
  onChange: (checked: boolean) => void
  label?: ReactNode
  description?: ReactNode
  indeterminate?: boolean
  disabled?: boolean
  'aria-label'?: string
  className?: string
  'data-testid'?: string
  'data-force'?: string
}

export function Checkbox({
  checked,
  onChange,
  label,
  description,
  indeterminate = false,
  disabled = false,
  'aria-label': ariaLabel,
  className,
  'data-testid': testId = 'checkbox',
  'data-force': force,
}: CheckboxProps) {
  const input = useRef<HTMLInputElement>(null)
  useEffect(() => {
    if (input.current) input.current.indeterminate = indeterminate
  }, [indeterminate])
  const mark = indeterminate ? Minus : checked ? Check : null
  return (
    <label
      className={cx('ui-check', className)}
      data-disabled={disabled || undefined}
      data-force={force}
    >
      <input
        ref={input}
        type="checkbox"
        className="ui-check-input"
        checked={checked}
        disabled={disabled}
        aria-label={label ? undefined : ariaLabel}
        data-testid={testId}
        onChange={(e) => onChange(e.target.checked)}
      />
      <span className="ui-check-box" aria-hidden="true">
        {mark && <Icon icon={mark} size={12} />}
      </span>
      {label ? (
        <span className="ui-toggle-text">
          <span className="ui-toggle-label">{label}</span>
          {description ? <span className="ui-toggle-desc">{description}</span> : null}
        </span>
      ) : null}
    </label>
  )
}
