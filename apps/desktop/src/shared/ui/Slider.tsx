/**
 * Slider：原生 range（键盘、无障碍、触控板滚动都由浏览器提供）+ 自绘轨道与滑块；
 * 已填充部分用 CSS 变量 `--fill` 表示，不做动画。可选在右侧显示格式化后的值。
 */
import { cx } from './cx'

export interface SliderProps {
  value: number
  onChange: (value: number) => void
  min: number
  max: number
  step?: number
  label: string
  /** 右侧显示的值（格式化后的文本）；不传则不显示 */
  valueLabel?: string
  disabled?: boolean
  className?: string
  'data-testid'?: string
  'data-force'?: string
}

export function Slider({
  value,
  onChange,
  min,
  max,
  step = 1,
  label,
  valueLabel,
  disabled = false,
  className,
  'data-testid': testId = 'slider',
  'data-force': force,
}: SliderProps) {
  const fill = max > min ? ((value - min) / (max - min)) * 100 : 0
  return (
    <div className={cx('ui-slider', className)} data-disabled={disabled || undefined}>
      <input
        type="range"
        className="ui-slider-input"
        min={min}
        max={max}
        step={step}
        value={value}
        disabled={disabled}
        aria-label={label}
        aria-valuetext={valueLabel}
        data-testid={testId}
        data-force={force}
        style={{ ['--fill' as string]: `${fill}%` }}
        onChange={(e) => onChange(Number(e.target.value))}
      />
      {valueLabel !== undefined ? <span className="ui-slider-value">{valueLabel}</span> : null}
    </div>
  )
}
