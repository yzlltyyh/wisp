/**
 * 图标（Lucide 线性图标，1.5 px 描边）。描边宽度与尺寸无关（absoluteStrokeWidth），
 * 14 / 16 / 20 px 下都是 1.5 px。纯装饰时 aria-hidden；需要被读出时传 `label`。
 */
import type { LucideIcon } from 'lucide-react'
import { cx } from './cx'

export interface IconProps {
  icon: LucideIcon
  /** 默认 16（--icon） */
  size?: number
  className?: string
  /** 可访问名称；不传则对辅助技术隐藏 */
  label?: string
}

/** 图标的描边宽度 */
export const ICON_STROKE = 1.5

export function Icon({ icon: Glyph, size = 16, className, label }: IconProps) {
  return (
    <Glyph
      size={size}
      strokeWidth={ICON_STROKE}
      absoluteStrokeWidth
      className={cx('ui-icon', className)}
      aria-hidden={label ? undefined : true}
      aria-label={label}
      role={label ? 'img' : undefined}
      focusable={false}
    />
  )
}
