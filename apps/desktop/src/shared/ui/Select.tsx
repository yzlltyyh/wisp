/**
 * Select：触发器按钮 + 下拉菜单（勾选当前值，打开时高亮当前值）。
 * 键盘：触发器上 ↑↓ / Enter / Space 打开；菜单内同 Menu。
 * 选项可以分组（`group`：相邻的同组选项上方显示一次分组标题，组与组之间有分隔线），也可以带第二行说明（`description`）。
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
  /** 标签下方的第二行说明（可以折行） */
  description?: string
  disabled?: boolean
  /** 分组标题：与上一个选项不同时插入分隔线与标题 */
  group?: string
}

/** 选项 → 菜单项：分组变化处插入分隔线与分组标题 */
export function selectEntries<V extends string>(
  options: readonly SelectOption<V>[],
  value: V | null,
  onChange: (value: V) => void,
): MenuEntry[] {
  const items: MenuEntry[] = []
  let group: string | undefined
  options.forEach((o, i) => {
    if (o.group !== undefined && o.group !== group) {
      if (i > 0) items.push({ type: 'separator', id: `sep-${i}` })
      items.push({ type: 'label', id: `group-${i}`, label: o.group })
    }
    group = o.group
    items.push({
      id: o.value,
      label: o.label,
      hint: o.hint,
      description: o.description,
      disabled: o.disabled,
      checked: o.value === value,
      onSelect: () => onChange(o.value),
    })
  })
  return items
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
  const items = selectEntries(options, value, onChange)
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
