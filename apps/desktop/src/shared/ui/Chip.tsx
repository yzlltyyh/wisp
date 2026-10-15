/**
 * Chip / StyleChip。高 28（命中区），圆角 --r-chip；
 * `selected` 为强调色淡底；`StyleChip` 是带下拉箭头的风格选择触发器（翻译视图头部，展开 StyleMenu）。
 */
import type { LucideIcon } from 'lucide-react'
import { ChevronDown } from 'lucide-react'
import type { ButtonHTMLAttributes, ReactNode, Ref } from 'react'
import { cx } from './cx'
import { Icon } from './Icon'

export interface ChipProps extends Omit<ButtonHTMLAttributes<HTMLButtonElement>, 'type'> {
  icon?: LucideIcon
  /** 末尾的下拉箭头 */
  dropdown?: boolean
  selected?: boolean
  /** subtle：静止时无底色（头部里的 chip）；filled：有底色 */
  variant?: 'subtle' | 'filled'
  children: ReactNode
  ref?: Ref<HTMLButtonElement>
  'data-testid'?: string
  'data-force'?: string
}

export function Chip({
  icon,
  dropdown = false,
  selected = false,
  variant = 'filled',
  className,
  children,
  ...rest
}: ChipProps) {
  return (
    <button
      {...rest}
      type="button"
      className={cx('ui-chip', className)}
      data-variant={variant}
      data-selected={selected || undefined}
    >
      {icon ? <Icon icon={icon} size={14} /> : null}
      <span className="ui-chip-label">{children}</span>
      {dropdown ? <Icon icon={ChevronDown} size={12} className="ui-chip-caret" /> : null}
    </button>
  )
}

export interface StyleChipProps extends Omit<ChipProps, 'dropdown' | 'children'> {
  /** 当前风格的显示名（styles.natural 等） */
  name: string
}

/** 翻译风格选择器的触发器（配合 DropdownMenu 使用） */
export function StyleChip({ name, ...rest }: StyleChipProps) {
  return (
    <Chip
      {...rest}
      dropdown
      variant={rest.variant ?? 'subtle'}
      data-testid={rest['data-testid'] ?? 'style-chip'}
    >
      {name}
    </Chip>
  )
}
