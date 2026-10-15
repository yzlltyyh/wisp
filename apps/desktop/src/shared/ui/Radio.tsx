/**
 * Radio / RadioGroup：原生 radio + 自绘圆点；方向键在组内移动由浏览器原生处理。
 */
import { useId, type ReactNode } from 'react'
import { cx } from './cx'

export interface RadioOption<V extends string> {
  value: V
  label: ReactNode
  description?: ReactNode
  disabled?: boolean
}

export interface RadioGroupProps<V extends string> {
  value: V
  options: readonly RadioOption<V>[]
  onChange: (value: V) => void
  label: string
  /** 横排（短选项）或竖排（默认） */
  orientation?: 'vertical' | 'horizontal'
  disabled?: boolean
  className?: string
  'data-testid'?: string
}

export function RadioGroup<V extends string>({
  value,
  options,
  onChange,
  label,
  orientation = 'vertical',
  disabled = false,
  className,
  'data-testid': testId = 'radio-group',
}: RadioGroupProps<V>) {
  const name = useId()
  return (
    <div
      role="radiogroup"
      aria-label={label}
      className={cx('ui-radio-group', className)}
      data-orientation={orientation}
      data-testid={testId}
    >
      {options.map((o) => (
        <label
          key={o.value}
          className="ui-check ui-radio"
          data-disabled={disabled || o.disabled || undefined}
        >
          <input
            type="radio"
            className="ui-check-input"
            name={name}
            value={o.value}
            checked={o.value === value}
            disabled={disabled || o.disabled}
            data-testid={`${testId}-${o.value}`}
            onChange={() => onChange(o.value)}
          />
          <span className="ui-check-box ui-radio-dot" aria-hidden="true" />
          <span className="ui-toggle-text">
            <span className="ui-toggle-label">{o.label}</span>
            {o.description ? <span className="ui-toggle-desc">{o.description}</span> : null}
          </span>
        </label>
      ))}
    </div>
  )
}
