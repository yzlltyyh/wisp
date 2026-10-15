/**
 * SegmentedControl：等宽分段，选中指示器只做 transform（x 平移，spring.snappy），
 * 减少动效时直接跳到目标。键盘：←→ 移动并选择（role="radiogroup"）。
 */
import type { LucideIcon } from 'lucide-react'
import { m } from 'motion/react'
import { useRef, type KeyboardEvent as ReactKeyboardEvent } from 'react'
import { spring } from '../motion/tokens'
import { useReducedMotionPref } from '../motion/useReducedMotionPref'
import { cx } from './cx'
import { Icon } from './Icon'

export interface SegmentOption<V extends string> {
  value: V
  label: string
  icon?: LucideIcon
  disabled?: boolean
}

export interface SegmentedControlProps<V extends string> {
  value: V
  options: readonly SegmentOption<V>[]
  onChange: (value: V) => void
  label: string
  size?: 'sm' | 'md'
  disabled?: boolean
  className?: string
  'data-testid'?: string
}

export function SegmentedControl<V extends string>({
  value,
  options,
  onChange,
  label,
  size = 'sm',
  disabled = false,
  className,
  'data-testid': testId = 'segmented',
}: SegmentedControlProps<V>) {
  const reduced = useReducedMotionPref()
  const refs = useRef<Array<HTMLButtonElement | null>>([])
  const index = Math.max(
    0,
    options.findIndex((o) => o.value === value),
  )
  const move = (from: number, step: 1 | -1) => {
    const n = options.length
    for (let k = 1; k <= n; k++) {
      const i = (from + step * k + n) % n
      const o = options[i]
      if (o && !o.disabled) {
        onChange(o.value)
        refs.current[i]?.focus()
        return
      }
    }
  }
  const onKeyDown = (e: ReactKeyboardEvent) => {
    if (e.key === 'ArrowRight' || e.key === 'ArrowDown') {
      e.preventDefault()
      move(index, 1)
    } else if (e.key === 'ArrowLeft' || e.key === 'ArrowUp') {
      e.preventDefault()
      move(index, -1)
    }
  }
  return (
    <div
      role="radiogroup"
      tabIndex={-1}
      aria-label={label}
      aria-disabled={disabled || undefined}
      className={cx('ui-segmented', className)}
      data-size={size}
      data-testid={testId}
      style={{ ['--seg-count' as string]: options.length }}
      onKeyDown={onKeyDown}
    >
      <m.span
        className="ui-segmented-thumb"
        aria-hidden="true"
        initial={false}
        animate={{ x: `${index * 100}%` }}
        transition={reduced ? { duration: 0 } : spring.snappy}
      />
      {options.map((o, i) => (
        <button
          key={o.value}
          ref={(el) => {
            refs.current[i] = el
          }}
          type="button"
          // 分段按钮是 radio 语义的自绘按钮（原生 radio 无法做滑动指示器），键盘行为按 WAI-ARIA radiogroup 实现
          // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role
          role="radio"
          aria-checked={o.value === value}
          tabIndex={o.value === value ? 0 : -1}
          disabled={disabled || o.disabled}
          className="ui-segmented-item"
          data-testid={`${testId}-${o.value}`}
          onClick={() => onChange(o.value)}
        >
          {o.icon ? <Icon icon={o.icon} size={14} /> : null}
          <span>{o.label}</span>
        </button>
      ))}
    </div>
  )
}
