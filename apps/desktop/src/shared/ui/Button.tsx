/**
 * Button / IconButton。
 *
 * - 变体：primary / secondary / ghost / danger；尺寸：sm 28 / md 32（命中区 ≥ 28）；
 * - 状态：悬停（80 ms 背景过渡）、按下（scale 0.98，80 ms）、键盘焦点（2 px 强调色环）、禁用、加载（转圈，宽度不变，CLS = 0）；
 * - IconButton 必须给 `label`（无障碍名称 + tooltip），可带快捷键提示；`active` 表示切换型按钮的按下态（aria-pressed）。
 *
 * 状态样式全部在 components.css（`.ui-btn`），展示页可用 `data-force="hover|active|focus"` 冻结某个状态。
 */
import type { LucideIcon } from 'lucide-react'
import type { ButtonHTMLAttributes, ReactNode, Ref } from 'react'
import { cx } from './cx'
import { Icon } from './Icon'
import { Spinner } from './Spinner'
import { Tooltip } from './Tooltip'

export type ButtonVariant = 'primary' | 'secondary' | 'ghost' | 'danger'
export type ButtonSize = 'sm' | 'md'

export interface ButtonProps extends Omit<ButtonHTMLAttributes<HTMLButtonElement>, 'type'> {
  variant?: ButtonVariant
  size?: ButtonSize
  /** 文字前的图标 */
  icon?: LucideIcon
  /** 文字后的图标（例如下拉箭头） */
  trailingIcon?: LucideIcon
  loading?: boolean
  type?: 'button' | 'submit' | 'reset'
  ref?: Ref<HTMLButtonElement>
  'data-testid'?: string
  'data-force'?: string
}

export function Button({
  variant = 'secondary',
  size = 'sm',
  icon,
  trailingIcon,
  loading = false,
  type = 'button',
  className,
  children,
  disabled,
  onClick,
  ...rest
}: ButtonProps) {
  return (
    <button
      {...rest}
      type={type}
      className={cx('ui-btn', className)}
      data-variant={variant}
      data-size={size}
      data-loading={loading || undefined}
      aria-busy={loading || undefined}
      disabled={disabled}
      aria-disabled={loading || undefined}
      onClick={loading ? undefined : onClick}
    >
      <span className="ui-btn-content">
        {icon ? <Icon icon={icon} size={size === 'md' ? 16 : 14} /> : null}
        {children !== undefined && children !== null ? (
          <span className="ui-btn-label">{children}</span>
        ) : null}
        {trailingIcon ? <Icon icon={trailingIcon} size={14} className="ui-btn-trailing" /> : null}
      </span>
      {loading ? <Spinner size={14} className="ui-btn-spinner" /> : null}
    </button>
  )
}

export interface IconButtonProps extends Omit<
  ButtonHTMLAttributes<HTMLButtonElement>,
  'type' | 'children'
> {
  icon: LucideIcon
  /** 无障碍名称，同时作为 tooltip 文案 */
  label: string
  /** tooltip 里显示的快捷键（组合串） */
  shortcut?: string | null
  /** 切换型按钮的按下态（置顶、固定） */
  active?: boolean
  /** 28（默认）或 24（只用于密集的行内操作，命中区仍由外边距补足到 28） */
  size?: 'md' | 'sm'
  tone?: 'default' | 'danger'
  /** 不显示 tooltip（例如已经有可见文字说明） */
  noTooltip?: boolean
  tooltipSide?: 'top' | 'bottom'
  ref?: Ref<HTMLButtonElement>
  'data-testid'?: string
  'data-force'?: string
  /** 替换默认图标渲染（CopyButton 的图标切换动效） */
  glyph?: ReactNode
}

export function IconButton({
  icon,
  label,
  shortcut,
  active,
  size = 'md',
  tone = 'default',
  noTooltip = false,
  tooltipSide = 'top',
  className,
  glyph,
  ...rest
}: IconButtonProps) {
  const button = (
    <button
      {...rest}
      type="button"
      className={cx('ui-iconbtn', className)}
      data-size={size}
      data-tone={tone}
      aria-label={label}
      aria-pressed={active === undefined ? undefined : active}
    >
      {glyph ?? <Icon icon={icon} size={16} />}
    </button>
  )
  if (noTooltip) return button
  return (
    <Tooltip content={label} shortcut={shortcut} side={tooltipSide} disabled={rest.disabled}>
      {button}
    </Tooltip>
  )
}
