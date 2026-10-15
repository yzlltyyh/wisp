/**
 * Select：触发器按钮 + 下拉菜单（勾选当前值，打开时高亮当前值）。
 * 键盘：触发器上 ↑↓ / Enter / Space 打开；菜单内同 Menu。
 */
import { ChevronsUpDown } from 'lucide-react'
import { useRef, useState } from 'react'
import { cx } from './cx'
import { Icon } from './Icon'
import { Menu, type MenuEntry } from './Menu'

export interface SelectOption<V extends string> {
  value: V
  label: string
  hint?: string
  disabled?: boolean
}

export interface SelectProps<V extends string> {
  value: V | null
  options: readonly SelectOption<V>[]
  onChange: (value: V) => void
  /** 无障碍名称 */
  label: string
  placeholder?: string
  size?: 'sm' | 'md'
  disabled?: boolean
  invalid?: boolean
  className?: string
  'data-testid'?: string
  'data-force'?: string
}

export function Select<V extends string>({
  value,
  options,
  onChange,
  label,
  placeholder,
  size = 'sm',
  disabled = false,
  invalid = false,
  className,
  'data-testid': testId = 'select',
  'data-force': force,
}: SelectProps<V>) {
  const trigger = useRef<HTMLButtonElement>(null)
  const [open, setOpen] = useState(false)
  const [width, setWidth] = useState<number | undefined>(undefined)
  const current = options.find((o) => o.value === value)
  const items: MenuEntry[] = options.map((o) => ({
    id: o.value,
    label: o.label,
    hint: o.hint,
    disabled: o.disabled,
    checked: o.value === value,
    onSelect: () => onChange(o.value),
  }))
  const openMenu = () => {
    setWidth(trigger.current?.offsetWidth)
    setOpen(true)
  }
  return (
    <>
      <button
        ref={trigger}
        type="button"
        className={cx('ui-select', className)}
        data-size={size}
        data-invalid={invalid || undefined}
        data-open={open || undefined}
        data-placeholder={current ? undefined : true}
        data-testid={testId}
        data-force={force}
        aria-label={label}
        aria-haspopup="menu"
        aria-expanded={open}
        disabled={disabled}
        onClick={() => (open ? setOpen(false) : openMenu())}
        onKeyDown={(e) => {
          if (['ArrowDown', 'ArrowUp', 'Enter', ' '].includes(e.key)) {
            e.preventDefault()
            openMenu()
          }
        }}
      >
        <span className="ui-select-value">{current?.label ?? placeholder ?? ''}</span>
        <Icon icon={ChevronsUpDown} size={14} className="ui-select-caret" />
      </button>
      <Menu
        open={open}
        anchor={trigger}
        items={items}
        label={label}
        minWidth={width}
        initialActive="selected"
        data-testid={`${testId}-menu`}
        onClose={() => setOpen(false)}
      />
    </>
  )
}
