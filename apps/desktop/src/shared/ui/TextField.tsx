/**
 * TextField / SearchField / NumberField。
 *
 * - 高 28（sm）/ 32（md）；输入框底色 --field-bg + 描边 --field-border，聚焦时强调色焦点环；
 * - `invalid` 为危险色描边并设置 aria-invalid；`leading` / `trailing` 放图标或单位；
 * - SearchField：前置放大镜、有内容时出现清除按钮；`variant="bare"` 是面板头部的大号无边框搜索（剪贴板）；
 * - NumberField：上下键 / 步进按钮调整，失焦时夹到 [min, max]；输入中允许临时非法值。
 * 输入法组合期间（isComposing）`onEnter` 不触发。
 * `autoFocus` 走 `useAutoFocus`：所在面板视图还在切换的暂存层里时，推迟到视图显示时再聚焦（shared/hooks/useViewActive）。
 */
import { ChevronDown, ChevronUp, Search, X } from 'lucide-react'
import {
  useCallback,
  useRef,
  useState,
  type InputHTMLAttributes,
  type KeyboardEvent as ReactKeyboardEvent,
  type ReactNode,
  type Ref,
} from 'react'
import { useAutoFocus } from '../hooks/useViewActive'
import { useT } from '../i18n/useT'
import { cx } from './cx'
import { Icon } from './Icon'

export interface TextFieldProps extends Omit<
  InputHTMLAttributes<HTMLInputElement>,
  'size' | 'onChange' | 'value'
> {
  value: string
  onChange: (value: string) => void
  size?: 'sm' | 'md'
  invalid?: boolean
  leading?: ReactNode
  trailing?: ReactNode
  /** 回车（输入法组合中不触发） */
  onEnter?: (e: ReactKeyboardEvent<HTMLInputElement>) => void
  ref?: Ref<HTMLInputElement>
  /** 外层容器的 class */
  className?: string
  'data-testid'?: string
  'data-force'?: string
  /** 外层容器的变体（SearchField 用） */
  'data-variant'?: string
}

export function TextField({
  value,
  onChange,
  size = 'sm',
  invalid = false,
  leading,
  trailing,
  onEnter,
  onKeyDown,
  className,
  disabled,
  autoFocus,
  ref,
  'data-testid': testId = 'text-field',
  'data-force': force,
  'data-variant': variant,
  ...input
}: TextFieldProps) {
  const inner = useRef<HTMLInputElement | null>(null)
  const setRef = useCallback(
    (el: HTMLInputElement | null) => {
      inner.current = el
      if (typeof ref === 'function') ref(el)
      else if (ref) ref.current = el
    },
    [ref],
  )
  useAutoFocus(inner, autoFocus)
  return (
    <div
      className={cx('ui-field', className)}
      data-variant={variant}
      data-size={size}
      data-invalid={invalid || undefined}
      data-disabled={disabled || undefined}
      data-force={force}
    >
      {leading ? <span className="ui-field-leading">{leading}</span> : null}
      <input
        {...input}
        ref={setRef}
        className="ui-field-input"
        value={value}
        disabled={disabled}
        aria-invalid={invalid || undefined}
        data-testid={testId}
        spellCheck={input.spellCheck ?? false}
        autoComplete={input.autoComplete ?? 'off'}
        onChange={(e) => onChange(e.target.value)}
        onKeyDown={(e) => {
          onKeyDown?.(e)
          if (e.key === 'Enter' && !e.nativeEvent.isComposing && e.keyCode !== 229) onEnter?.(e)
        }}
      />
      {trailing ? <span className="ui-field-trailing">{trailing}</span> : null}
    </div>
  )
}

export interface SearchFieldProps extends Omit<TextFieldProps, 'leading' | 'trailing'> {
  /** field：普通输入框；bare：面板头部的大号无边框搜索 */
  variant?: 'field' | 'bare'
  /** 清除后的回调（默认 onChange('')） */
  onClear?: () => void
  /** 末尾附加内容（例如过滤菜单） */
  accessory?: ReactNode
}

export function SearchField({
  variant = 'field',
  onClear,
  accessory,
  className,
  'data-testid': testId = 'search-field',
  ...field
}: SearchFieldProps) {
  const t = useT()
  const clear = () => (onClear ? onClear() : field.onChange(''))
  return (
    <TextField
      {...field}
      type="search"
      size={variant === 'bare' ? 'md' : field.size}
      className={cx('ui-search', className)}
      data-testid={testId}
      data-variant={variant}
      aria-label={field['aria-label'] ?? field.placeholder}
      leading={<Icon icon={Search} size={variant === 'bare' ? 18 : 14} />}
      trailing={
        <>
          {field.value ? (
            <button
              type="button"
              className="ui-field-clear"
              aria-label={t('common.a11y.clear_search')}
              onClick={clear}
              tabIndex={-1}
            >
              <Icon icon={X} size={12} />
            </button>
          ) : null}
          {accessory}
        </>
      }
    />
  )
}

export interface NumberFieldProps extends Omit<
  TextFieldProps,
  'value' | 'onChange' | 'type' | 'trailing'
> {
  value: number
  onChange: (value: number) => void
  min?: number
  max?: number
  step?: number
  /** 单位（显示在数字后面，例如 px、ms） */
  unit?: string
}

export function NumberField({
  value,
  onChange,
  min = -Infinity,
  max = Infinity,
  step = 1,
  unit,
  'data-testid': testId = 'number-field',
  ...field
}: NumberFieldProps) {
  const t = useT()
  /** 编辑中的文字；null 表示没有在编辑，显示 value */
  const [draft, setDraft] = useState<string | null>(null)
  const clamp = (n: number) => Math.min(max, Math.max(min, n))
  const commit = (raw: string | null) => {
    setDraft(null)
    if (raw === null) return
    const n = Number(raw)
    if (raw.trim() === '' || !Number.isFinite(n)) return
    const next = clamp(n)
    if (next !== value) onChange(next)
  }
  const bump = (dir: 1 | -1) => {
    const next = clamp(Number((value + dir * step).toFixed(6)))
    if (next !== value) onChange(next)
  }
  return (
    <TextField
      {...field}
      value={draft ?? String(value)}
      inputMode="decimal"
      className={cx('ui-number', field.className)}
      data-testid={testId}
      onChange={setDraft}
      onBlur={() => commit(draft)}
      onEnter={() => commit(draft)}
      onKeyDown={(e) => {
        if (e.key === 'ArrowUp' || e.key === 'ArrowDown') {
          e.preventDefault()
          bump(e.key === 'ArrowUp' ? 1 : -1)
        }
      }}
      trailing={
        <>
          {unit ? <span className="ui-field-unit">{unit}</span> : null}
          <span className="ui-stepper">
            <button
              type="button"
              tabIndex={-1}
              aria-label={t('common.a11y.increase')}
              disabled={field.disabled || value >= max}
              onClick={() => bump(1)}
            >
              <Icon icon={ChevronUp} size={10} />
            </button>
            <button
              type="button"
              tabIndex={-1}
              aria-label={t('common.a11y.decrease')}
              disabled={field.disabled || value <= min}
              onClick={() => bump(-1)}
            >
              <Icon icon={ChevronDown} size={10} />
            </button>
          </span>
        </>
      }
    />
  )
}
